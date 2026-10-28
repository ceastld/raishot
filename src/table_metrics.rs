//! 有界的表格评测：结构格 F1、含文字 F1、树编辑相似度和字符错误率。
use crate::table_grid::{TableCell, TableGrid, MAX_TEXT_BYTES};
use std::collections::BTreeMap;
use windows::Win32::Globalization::{NormalizationKC, NormalizeString};

const MAX_TREE_NODES: usize = 1_024;
const MAX_EDIT_WORK: usize = 16_000_000;
type CellKey = (usize, usize, usize, usize);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TableMetrics {
    pub precision: f64,
    pub recall: f64,
    pub cell_f1: f64,
    pub cell_text_f1: f64,
    pub teds_lite: f64,
    pub cer: f64,
}

pub fn evaluate(predicted: &TableGrid, truth: &TableGrid) -> Option<TableMetrics> {
    if !predicted.validate() || !truth.validate() {
        return None;
    }
    let predicted_keys = multiset(predicted.cells.iter().map(key));
    let truth_keys = multiset(truth.cells.iter().map(key));
    let matched = matches(&predicted_keys, &truth_keys);
    let predicted_text = predicted
        .cells
        .iter()
        .map(|cell| Some((key(cell), normalized(&cell.text)?)))
        .collect::<Option<Vec<_>>>()?;
    let truth_text = truth
        .cells
        .iter()
        .map(|cell| Some((key(cell), normalized(&cell.text)?)))
        .collect::<Option<Vec<_>>>()?;
    let text_matched = matches(&multiset(predicted_text), &multiset(truth_text));
    let precision = ratio(matched, predicted.cells.len());
    let recall = ratio(matched, truth.cells.len());
    let predicted_tree = Tree::from_grid(predicted)?;
    let truth_tree = Tree::from_grid(truth)?;
    let distance = tree_distance(&predicted_tree, &truth_tree)?;
    let teds_lite =
        1. - distance as f64 / predicted_tree.nodes.len().max(truth_tree.nodes.len()) as f64;
    let predicted_chars = text_characters(predicted)?;
    let truth_chars = text_characters(truth)?;
    let edits = character_distance(&predicted_chars, &truth_chars)?;
    Some(TableMetrics {
        precision,
        recall,
        cell_f1: f1(matched, predicted.cells.len(), truth.cells.len()),
        cell_text_f1: f1(text_matched, predicted.cells.len(), truth.cells.len()),
        teds_lite,
        cer: if truth_chars.is_empty() {
            if predicted_chars.is_empty() {
                0.
            } else {
                1.
            }
        } else {
            edits as f64 / truth_chars.len() as f64
        },
    })
}

fn key(cell: &TableCell) -> CellKey {
    (cell.row, cell.column, cell.rowspan, cell.colspan)
}
fn multiset<K: Ord>(items: impl IntoIterator<Item = K>) -> BTreeMap<K, usize> {
    let mut result = BTreeMap::new();
    for item in items {
        *result.entry(item).or_default() += 1;
    }
    result
}
fn matches<K: Ord>(predicted: &BTreeMap<K, usize>, truth: &BTreeMap<K, usize>) -> usize {
    predicted
        .iter()
        .map(|(key, count)| (*count).min(truth.get(key).copied().unwrap_or(0)))
        .sum()
}
fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.
    } else {
        numerator as f64 / denominator as f64
    }
}
fn f1(matched: usize, predicted: usize, truth: usize) -> f64 {
    if predicted + truth == 0 {
        1.
    } else {
        2. * matched as f64 / (predicted + truth) as f64
    }
}

/// 复用 Windows 的 NFKC，避免为评测新增 Unicode 运行依赖。
fn normalized(text: &str) -> Option<String> {
    if text.is_empty() {
        return Some(String::new());
    }
    if text.len() > MAX_TEXT_BYTES {
        return None;
    }
    let source: Vec<_> = text.encode_utf16().collect();
    let length = unsafe { NormalizeString(NormalizationKC, &source, None) }.unsigned_abs() as usize;
    if length == 0 || length > MAX_TEXT_BYTES * 4 {
        return None;
    }
    let mut output = vec![0u16; length];
    let mut written = unsafe { NormalizeString(NormalizationKC, &source, Some(&mut output)) };
    if written < 0 {
        let required = written.unsigned_abs() as usize;
        if required <= output.len() || required > MAX_TEXT_BYTES * 4 {
            return None;
        }
        output.resize(required, 0);
        written = unsafe { NormalizeString(NormalizationKC, &source, Some(&mut output)) };
    }
    if written <= 0 || written as usize > output.len() {
        return None;
    }
    let normalized = String::from_utf16(&output[..written as usize]).ok()?;
    Some(
        normalized
            .chars()
            .flat_map(char::to_lowercase)
            .filter(|character| !character.is_whitespace())
            .collect(),
    )
}

fn text_characters(grid: &TableGrid) -> Option<Vec<char>> {
    let mut cells: Vec<_> = grid.cells.iter().collect();
    cells.sort_by_key(|cell| (cell.row, cell.column));
    let mut text = String::new();
    for cell in cells {
        text.push_str(&normalized(&cell.text)?);
    }
    Some(text.chars().collect())
}

fn character_distance(a: &[char], b: &[char]) -> Option<usize> {
    if a.len().checked_mul(b.len())? > MAX_EDIT_WORK {
        return None;
    }
    let mut previous: Vec<_> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, left) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            current[j + 1] = (current[j] + 1)
                .min(previous[j + 1] + 1)
                .min(previous[j] + usize::from(left != right));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    Some(previous[b.len()])
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    Table,
    Row,
    Cell(usize, usize),
}
struct Tree {
    nodes: Vec<Label>,
    leftmost: Vec<usize>,
    keyroots: Vec<usize>,
}
impl Tree {
    fn from_grid(grid: &TableGrid) -> Option<Self> {
        let mut nodes = Vec::new();
        let mut leftmost = vec![0];
        for row in 0..grid.rows() {
            let mut cells: Vec<_> = grid.cells.iter().filter(|cell| cell.row == row).collect();
            cells.sort_by_key(|cell| cell.column);
            let first = nodes.len() + 1;
            for cell in cells {
                nodes.push(Label::Cell(cell.rowspan, cell.colspan));
                leftmost.push(nodes.len());
            }
            nodes.push(Label::Row);
            leftmost.push(first);
        }
        nodes.push(Label::Table);
        leftmost.push(1);
        if nodes.len() > MAX_TREE_NODES {
            return None;
        }
        let mut roots = BTreeMap::new();
        for (index, leaf) in leftmost.iter().enumerate().skip(1) {
            roots.insert(*leaf, index);
        }
        let mut keyroots: Vec<_> = roots.into_values().collect();
        keyroots.sort_unstable();
        Some(Self {
            nodes,
            leftmost,
            keyroots,
        })
    }
}

/// Zhang–Shasha 的有序树编辑距离；插入、删除和跨度替换的代价均为 1。
fn tree_distance(a: &Tree, b: &Tree) -> Option<usize> {
    let width = b.nodes.len() + 1;
    if a.nodes.len().checked_mul(b.nodes.len())? > MAX_EDIT_WORK {
        return None;
    }
    let mut distances = vec![0usize; (a.nodes.len() + 1) * width];
    let mut work = 0usize;
    for &i in &a.keyroots {
        for &j in &b.keyroots {
            let ai = a.leftmost[i];
            let bj = b.leftmost[j];
            let rows = i - ai + 2;
            let columns = j - bj + 2;
            work = work.checked_add(rows.checked_mul(columns)?)?;
            if work > MAX_EDIT_WORK {
                return None;
            }
            let mut forest = vec![0usize; rows * columns];
            for x in 0..rows {
                forest[x * columns] = x;
            }
            for y in 0..columns {
                forest[y] = y;
            }
            for x in 1..rows {
                for y in 1..columns {
                    let node_a = ai + x - 1;
                    let node_b = bj + y - 1;
                    let deletion = forest[(x - 1) * columns + y] + 1;
                    let insertion = forest[x * columns + y - 1] + 1;
                    if a.leftmost[node_a] == ai && b.leftmost[node_b] == bj {
                        let replacement = forest[(x - 1) * columns + y - 1]
                            + usize::from(a.nodes[node_a - 1] != b.nodes[node_b - 1]);
                        forest[x * columns + y] = deletion.min(insertion).min(replacement);
                        distances[node_a * width + node_b] = forest[x * columns + y];
                    } else {
                        let prefix_a = a.leftmost[node_a] - ai;
                        let prefix_b = b.leftmost[node_b] - bj;
                        let replacement = forest[prefix_a * columns + prefix_b]
                            + distances[node_a * width + node_b];
                        forest[x * columns + y] = deletion.min(insertion).min(replacement);
                    }
                }
            }
        }
    }
    Some(distances[a.nodes.len() * width + b.nodes.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table_grid::TableSource;
    fn grid(merged: bool) -> TableGrid {
        let mut grid = TableGrid {
            row_edges: vec![0, 10, 20],
            column_edges: vec![0, 10, 20],
            cells: Vec::new(),
            confidence: 1.,
            source: TableSource::Lines,
        };
        for (row, column, colspan, text) in if merged {
            vec![(0, 0, 2, "AB"), (1, 0, 1, "C"), (1, 1, 1, "D")]
        } else {
            vec![
                (0, 0, 1, "A"),
                (0, 1, 1, "B"),
                (1, 0, 1, "C"),
                (1, 1, 1, "D"),
            ]
        } {
            grid.cells.push(TableCell {
                row,
                column,
                rowspan: 1,
                colspan,
                text: text.to_owned(),
                rect: grid.cell_rect(row, column, 1, colspan).unwrap(),
                confidence: 1.,
            });
        }
        grid
    }
    #[test]
    fn table_metrics_identical_structure_and_text_are_perfect() {
        let metrics = evaluate(&grid(false), &grid(false)).unwrap();
        assert_eq!(
            (
                metrics.cell_f1,
                metrics.cell_text_f1,
                metrics.teds_lite,
                metrics.cer
            ),
            (1., 1., 1., 0.)
        );
    }
    #[test]
    fn table_metrics_merged_cell_has_hand_calculated_scores() {
        let metrics = evaluate(&grid(true), &grid(false)).unwrap();
        assert!((metrics.precision - 2. / 3.).abs() < 1e-9);
        assert_eq!(metrics.recall, 0.5);
        assert!((metrics.cell_f1 - 4. / 7.).abs() < 1e-9);
        assert!((metrics.teds_lite - 5. / 7.).abs() < 1e-9);
        assert_eq!(metrics.cer, 0.);
    }
    #[test]
    fn table_metrics_nfkc_case_and_whitespace_do_not_change_text_match() {
        let mut predicted = grid(false);
        predicted.cells[0].text = " Ａ\t".to_owned();
        assert_eq!(evaluate(&predicted, &grid(false)).unwrap().cell_text_f1, 1.);
    }
    #[test]
    fn table_metrics_character_error_rate_and_tree_deletion_are_bounded() {
        assert_eq!(
            character_distance(&['a', 'b', 'c'], &['a', 'x', 'c']),
            Some(1)
        );
        assert!(character_distance(&vec!['a'; 5000], &vec!['b'; 5000]).is_none());
        let a = Tree {
            nodes: vec![Label::Cell(1, 1), Label::Row, Label::Table],
            leftmost: vec![0, 1, 1, 1],
            keyroots: vec![3],
        };
        let b = Tree {
            nodes: vec![Label::Row, Label::Table],
            leftmost: vec![0, 1, 1],
            keyroots: vec![2],
        };
        assert_eq!(tree_distance(&a, &b), Some(1));
    }
}
