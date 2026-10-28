//! 无模型表格候选：持续列留白、行对齐及字素分配；证据不足时返回 None。
#[cfg(test)]
use crate::capture::Image;
use crate::{
    geometry::Rect,
    recognition::{
        geometry::Projection, table::OcrTableCopy, HorizontalDirection, RecognizedLine,
        RecognizedText, RecognizedWord, TextGeometry,
    },
    table_grid::{
        extent_edges, TableCell, TableGrid, TableSource, MAX_CELL_BYTES, MAX_COLUMNS,
        MAX_COORDINATE, MAX_ROWS, MAX_TEXT_BYTES,
    },
    table_lines::{DetectedLines, LineKind, LineSegment},
};
use std::{collections::BTreeMap, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

const MAX_PIECES: usize = 32_768;
const MAX_WORDS: usize = 32_768;
// DB 检测裁行四角会有像素级倾斜，约 6.8° 内仍按截图的水平表格行消费。
const MAX_LINE_SLOPE: f32 = 0.12;

#[derive(Clone)]
struct Piece {
    line: usize,
    range: Range<usize>,
    rect: Rect,
    word: usize,
}

#[derive(Clone)]
struct Block {
    left: u32,
    right: u32,
    top: u32,
    bottom: u32,
}
impl Block {
    fn center_y(&self) -> u32 {
        self.top + (self.bottom - self.top) / 2
    }
    fn height(&self) -> u32 {
        self.bottom - self.top
    }
}

struct Row {
    blocks: Vec<Block>,
    top: u32,
    bottom: u32,
}

#[cfg(test)]
pub fn recognize(source: &RecognizedText, image: Option<&Image>) -> Option<TableGrid> {
    recognize_detected(source, image.and_then(crate::table_lines::detect).as_ref())
}

/// 线检测只依赖像素，可在 OCR 后台线程预先算好；这里只做几何分配。
pub fn recognize_detected(
    source: &RecognizedText,
    detected: Option<&DetectedLines>,
) -> Option<TableGrid> {
    let pieces = source_pieces(source)?;
    if let Some(grid) = detected.and_then(|lines| lines.grid.clone()) {
        if let Some(grid) = assign_text(source, &pieces, collapse_thin(grid, &pieces)) {
            return Some(grid);
        }
    }
    let aligned = alignment(&pieces, detected).and_then(|grid| assign_text(source, &pieces, grid));
    // 旧矩阵逐视觉行分配；可靠键值表的右列续行应先归回原单元格。
    if aligned.as_ref().is_some_and(|grid| {
        key_value_grid(grid) && grid.cells.iter().any(|cell| cell.text.contains('\n'))
    }) {
        return aligned;
    }
    if let Some(grid) =
        OcrTableCopy::new(source).and_then(|table| TableGrid::from_legacy(source, &table))
    {
        return Some(grid);
    }
    aligned
}

/// 独立结构模型给出行列和跨度；用原分辨率字素填字，表外的完整识别行仍交给正文。
pub fn fill_structure(
    source: &RecognizedText,
    mut grid: TableGrid,
    detected: Option<&DetectedLines>,
) -> Option<(TableGrid, Option<Vec<usize>>)> {
    if !grid.validate() {
        return None;
    }
    let pieces = source_pieces_with(source, true)?;
    let mut heights: Vec<_> = pieces.iter().map(|piece| piece.rect.height).collect();
    heights.sort_unstable();
    let height = *heights.get(heights.len() / 2)?;
    // 模型边界有轻微漂移时，只在邻边距离的十分之一内吸附真实线，不改变模型跨度。
    if let Some(lines) = detected {
        let slots = |edges: &[u32]| {
            edges
                .iter()
                .enumerate()
                .map(|(index, edge)| {
                    let before = index.checked_sub(1).map(|i| edge - edges[i]);
                    let after = edges.get(index + 1).map(|next| next - edge);
                    let radius = before.into_iter().chain(after).min().unwrap_or(height) / 10;
                    (
                        edge.saturating_sub(radius.max(2)),
                        edge.saturating_add(radius.max(2)),
                    )
                })
                .collect::<Vec<_>>()
        };
        let x = (*grid.column_edges.first()?, *grid.column_edges.last()?);
        let y = (*grid.row_edges.first()?, *grid.row_edges.last()?);
        let columns = slots(&grid.column_edges);
        let rows = slots(&grid.row_edges);
        snap_edges(&mut grid.column_edges, &columns, &lines.vertical, y);
        snap_edges(&mut grid.row_edges, &rows, &lines.horizontal, x);
        for cell in &mut grid.cells {
            cell.rect = grid_rect(&grid.row_edges, &grid.column_edges, cell)?;
        }
    }
    let x = *grid.column_edges.first()?..=*grid.column_edges.last()?;
    let y = *grid.row_edges.first()?..=*grid.row_edges.last()?;
    let mut by_line: BTreeMap<usize, Vec<&Piece>> = BTreeMap::new();
    for piece in &pieces {
        by_line.entry(piece.line).or_default().push(piece);
    }
    let mut lines = Vec::new();
    for (line, pieces) in by_line {
        let inside = pieces
            .iter()
            .filter(|piece| {
                x.contains(&(piece.rect.x + piece.rect.width / 2))
                    && y.contains(&(piece.rect.y + piece.rect.height / 2))
            })
            .count();
        if inside == pieces.len() {
            lines.push(line);
        } else if inside != 0 {
            // 不返回被模型边界切掉一半文字的表格。
            return None;
        }
    }
    let selected: Vec<_> = pieces
        .into_iter()
        .filter(|piece| lines.binary_search(&piece.line).is_ok())
        .collect();
    if selected.is_empty() {
        return None;
    }
    let grid = assign_text(source, &selected, grid)?;
    let region = (lines.len() < source.lines.len()).then_some(lines);
    Some((grid, region))
}

fn grid_rect(rows: &[u32], columns: &[u32], cell: &TableCell) -> Option<Rect> {
    let x = *columns.get(cell.column)?;
    let y = *rows.get(cell.row)?;
    Some(Rect {
        x,
        y,
        width: columns.get(cell.column + cell.colspan)?.checked_sub(x)?,
        height: rows.get(cell.row + cell.rowspan)?.checked_sub(y)?,
    })
}

/// 整体不成表时，找正文中夹着的表格：有线网格范围内的行，或连续至少三行的多列对齐行段。
/// 规则与自动检测同样保守，另要求单元格文字较短，避免把双栏正文当表格。
/// 返回表格及其来源行号（升序）；表格只覆盖部分行，其余行仍按正文显示。
pub fn recognize_region(
    source: &RecognizedText,
    detected: Option<&DetectedLines>,
) -> Option<(TableGrid, Vec<usize>)> {
    let pieces = source_pieces(source)?;
    let mut by_line: BTreeMap<usize, Vec<&Piece>> = BTreeMap::new();
    for piece in &pieces {
        by_line.entry(piece.line).or_default().push(piece);
    }
    let text_lines = by_line.len();
    let mut candidates: Vec<(Vec<usize>, Option<TableGrid>)> = Vec::new();
    if let Some(grid) = detected.and_then(|lines| lines.grid.as_ref()) {
        let (left, right) = (*grid.column_edges.first()?, *grid.column_edges.last()?);
        let (top, bottom) = (*grid.row_edges.first()?, *grid.row_edges.last()?);
        let mut inside = Vec::new();
        let mut straddles = false;
        for (line, line_pieces) in &by_line {
            let count = line_pieces
                .iter()
                .filter(|piece| {
                    let x = piece.rect.x + piece.rect.width / 2;
                    let y = piece.rect.y + piece.rect.height / 2;
                    (left..=right).contains(&x) && (top..=bottom).contains(&y)
                })
                .count();
            if count == line_pieces.len() {
                inside.push(*line);
            } else if count > 0 {
                straddles = true;
            }
        }
        if !straddles && !inside.is_empty() {
            candidates.push((inside, Some(grid.clone())));
        }
    }
    candidates.extend(
        aligned_runs(&by_line)
            .into_iter()
            .map(|lines| (lines, None)),
    );
    for (mut lines, wired) in candidates {
        // 卡片标题、图下说明等与表格相邻的多段行可能被并进来：首尾稀疏行剔除后重算，最多三轮。
        for _ in 0..3 {
            if lines.len() >= text_lines {
                break;
            }
            let subset: Vec<Piece> = pieces
                .iter()
                .filter(|piece| lines.binary_search(&piece.line).is_ok())
                .cloned()
                .collect();
            let grid = match &wired {
                Some(grid) => collapse_thin(grid.clone(), &subset),
                None => match alignment(&subset, detected) {
                    Some(grid) => grid,
                    None => break,
                },
            };
            let Some(grid) = assign_text(source, &subset, grid) else {
                break;
            };
            let sparse = sparse_edge_lines(&grid, &subset);
            if !sparse.is_empty() && wired.is_none() {
                lines.retain(|line| !sparse.contains(line));
                continue;
            }
            let filled: Vec<usize> = grid
                .cells
                .iter()
                .map(|cell| cell.text.graphemes(true).count())
                .filter(|count| *count > 0)
                .collect();
            // 双栏正文的“单元格”多是整句：平均偏长或多数格超过 12 字时不当表格。
            let long = filled.iter().filter(|count| **count >= 12).count();
            if filled.len() < 4
                || (!key_value_grid(&grid)
                    && (filled.iter().sum::<usize>() > filled.len() * 16
                        || long * 2 >= filled.len()))
            {
                break;
            }
            return Some((grid, lines));
        }
    }
    None
}

/// 短标签配长说明是键值表，右列长短不应触发“双栏正文”过滤。
fn key_value_grid(grid: &TableGrid) -> bool {
    grid.columns() == 2
        && grid.rows() >= 3
        && grid.cells.iter().all(|cell| {
            cell.rowspan == 1
                && cell.colspan == 1
                && (cell.column != 0
                    || (!cell.text.contains('\n')
                        && (1..=12).contains(&cell.text.graphemes(true).count())))
        })
}

/// 首尾连续的稀疏行（非空单元格不到各行中位数的 2/3）所含的识别行；表格本身规整时为空。
/// 只用于正文夹表格：被剔除的行仍按正文显示，不会丢字。
fn sparse_edge_lines(grid: &TableGrid, pieces: &[Piece]) -> Vec<usize> {
    let rows = grid.rows();
    let mut filled = vec![0usize; rows];
    for cell in &grid.cells {
        if !cell.text.trim().is_empty() {
            filled[cell.row] += 1;
        }
    }
    let mut sorted = filled.clone();
    sorted.sort_unstable();
    let median = sorted[rows / 2];
    let sparse = |row: usize| filled[row] * 3 < median * 2;
    let mut drop = vec![false; rows];
    for row in 0..rows {
        if !sparse(row) {
            break;
        }
        drop[row] = true;
    }
    for row in (0..rows).rev() {
        if !sparse(row) {
            break;
        }
        drop[row] = true;
    }
    if drop.iter().filter(|dropped| !**dropped).count() < 3 {
        return Vec::new();
    }
    let mut lines: Vec<usize> = pieces
        .iter()
        .filter(|piece| {
            containing(&grid.row_edges, piece.rect.y + piece.rect.height / 2)
                .is_some_and(|row| drop[row])
        })
        .map(|piece| piece.line)
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// 按行中心把识别行聚成视觉行，返回连续至少三行“有两段以上留白分隔”的行段所含识别行。
fn aligned_runs(by_line: &BTreeMap<usize, Vec<&Piece>>) -> Vec<Vec<usize>> {
    struct VisualRow {
        top: u32,
        bottom: u32,
        lines: Vec<usize>,
        spans: Vec<(u32, u32)>,
    }
    let mut boxes: Vec<(usize, Rect)> = by_line
        .iter()
        .filter_map(|(line, pieces)| {
            let left = pieces.iter().map(|p| p.rect.x).min()?;
            let top = pieces.iter().map(|p| p.rect.y).min()?;
            let right = pieces.iter().map(|p| p.rect.x + p.rect.width).max()?;
            let bottom = pieces.iter().map(|p| p.rect.y + p.rect.height).max()?;
            Some((
                *line,
                Rect {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                },
            ))
        })
        .collect();
    let mut heights: Vec<u32> = boxes.iter().map(|(_, rect)| rect.height).collect();
    heights.sort_unstable();
    let Some(&height) = heights.get(heights.len() / 2) else {
        return Vec::new();
    };
    boxes.sort_by_key(|(line, rect)| (rect.y + rect.height / 2, rect.x, *line));
    let mut rows: Vec<VisualRow> = Vec::new();
    for (line, rect) in boxes {
        let center = rect.y + rect.height / 2;
        if let Some(row) = rows.last_mut().filter(|row| {
            (row.top + (row.bottom - row.top) / 2).abs_diff(center)
                <= height.max(rect.height) * 3 / 5
        }) {
            row.top = row.top.min(rect.y);
            row.bottom = row.bottom.max(rect.y + rect.height);
            row.lines.push(line);
        } else {
            rows.push(VisualRow {
                top: rect.y,
                bottom: rect.y + rect.height,
                lines: vec![line],
                spans: Vec::new(),
            });
        }
    }
    let minimum_gap = (height * 4 / 5).max(8);
    for row in &mut rows {
        let mut pieces: Vec<&Piece> = row
            .lines
            .iter()
            .flat_map(|line| by_line[line].iter().copied())
            .collect();
        pieces.sort_by_key(|piece| piece.rect.x);
        for piece in pieces {
            let (start, end) = (piece.rect.x, piece.rect.x + piece.rect.width);
            match row.spans.last_mut() {
                Some(last) if start <= last.1 + minimum_gap => last.1 = last.1.max(end),
                _ => row.spans.push((start, end)),
            }
        }
    }
    let mut runs = Vec::new();
    let mut current: Vec<&VisualRow> = Vec::new();
    for row in rows.iter().chain(std::iter::once(&VisualRow {
        top: 0,
        bottom: 0,
        lines: Vec::new(),
        spans: Vec::new(),
    })) {
        if row.spans.len() >= 2 {
            current.push(row);
            continue;
        }
        // 右列自动换行仍属于同一行：已有列锚点，且续行贴近前一行、
        // 起点与右列一致。明显段间距、跨列正文或左列缺字都不据此扩张候选。
        let anchors: Vec<_> = current
            .iter()
            .filter(|row| {
                row.spans.len() >= 2 && row.spans[0].0.abs_diff(current[0].spans[0].0) <= height
            })
            .collect();
        if let (Some(&(start, _)), Some(previous), Some(anchor)) =
            (row.spans.first(), current.last(), anchors.last())
        {
            if row.spans.len() == 1
                && start.abs_diff(anchor.spans[1].0) <= height
                && row.top.saturating_sub(previous.bottom) <= height * 3 / 5
            {
                current.push(row);
                continue;
            }
        }
        // 首尾可能混入相邻的标题或说明行：同时给出去掉首行、末行和首末两行的候选。
        let count = current.len();
        for (skip_first, skip_last) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            if count < 3 + skip_first + skip_last {
                continue;
            }
            let mut lines: Vec<usize> = current[skip_first..count - skip_last]
                .iter()
                .flat_map(|row| row.lines.iter().copied())
                .collect();
            lines.sort_unstable();
            runs.push(lines);
        }
        current.clear();
    }
    // 行数多的候选优先；同长时保持原顺序。
    runs.sort_by_key(|lines| std::cmp::Reverse(lines.len()));
    runs
}

/// 用户明确选择“表格识别”时调用：先走同一套可信候选，不足时放宽——
/// 有线网格忽略表外标题等文字，对齐聚类允许两行、正文跨列，最后退化为单列逐行表。
pub fn recognize_forced(
    source: &RecognizedText,
    detected: Option<&DetectedLines>,
) -> Option<TableGrid> {
    if let Some(grid) = recognize_detected(source, detected) {
        return Some(grid);
    }
    let pieces = source_pieces_with(source, true)?;
    if let Some(grid) = detected.and_then(|lines| lines.grid.clone()) {
        let inside = |edges: &[u32], value: u32| {
            edges
                .first()
                .zip(edges.last())
                .is_some_and(|(first, last)| (*first..=*last).contains(&value))
        };
        let pieces: Vec<Piece> = pieces
            .iter()
            .filter(|piece| {
                inside(&grid.column_edges, piece.rect.x + piece.rect.width / 2)
                    && inside(&grid.row_edges, piece.rect.y + piece.rect.height / 2)
            })
            .cloned()
            .collect();
        if !pieces.is_empty() {
            if let Some(grid) = assign_text(source, &pieces, collapse_thin(grid, &pieces)) {
                return Some(grid);
            }
        }
    }
    let grid = alignment_with(&pieces, detected, true)?;
    assign_text(source, &pieces, grid)
}

fn valid_rect(rect: Rect) -> bool {
    rect.width > 0
        && rect.height > 0
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|value| value <= MAX_COORDINATE)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|value| value <= MAX_COORDINATE)
}

fn full_crop_projection(geometry: TextGeometry) -> Option<Projection> {
    if geometry.quad.iter().any(|point| {
        point.x < 0.
            || point.y < 0.
            || point.x > MAX_COORDINATE as f32
            || point.y > MAX_COORDINATE as f32
    }) {
        return None;
    }
    Projection::new(geometry)
}

#[cfg(test)]
pub(crate) fn debug_pieces(source: &RecognizedText, lenient: bool) -> String {
    let mut failed = Vec::new();
    for (index, line) in source.lines.iter().enumerate() {
        let words: Vec<(usize, &RecognizedWord)> = source
            .words
            .iter()
            .enumerate()
            .filter(|(_, word)| word.line == index)
            .collect();
        let mut budget = 0;
        let mut out = Vec::new();
        if line_pieces(index, line, &words, &mut budget, &mut out).is_none() {
            let projection = line.geometry.and_then(full_crop_projection);
            let slope = projection.map(|p| {
                let (a, b) = (p.point(0., 0.5), p.point(1., 0.5));
                ((b.y - a.y), (b.x - a.x))
            });
            let words_ok: Vec<bool> = words
                .iter()
                .map(|(_, w)| w.geometry.is_some_and(|g| g.validated_in(w.rect).is_some()))
                .collect();
            failed.push(format!("{index}:slope={slope:?} words_ok={words_ok:?}"));
        }
    }
    format!(
        "{} failed={failed:?}",
        source_pieces_with(source, lenient)
            .map_or("None".into(), |pieces| pieces.len().to_string())
    )
}

fn source_pieces(source: &RecognizedText) -> Option<Vec<Piece>> {
    source_pieces_with(source, false)
}

/// `lenient` 只用于手动表格识别：单行方向、倾斜或词框不合格时不放弃整表，
/// 改按该行的词框联合近似字素；自动检测仍任一行不合格即返回 None。
fn source_pieces_with(source: &RecognizedText, lenient: bool) -> Option<Vec<Piece>> {
    if source.text.is_empty()
        || source.text.len() > MAX_TEXT_BYTES
        || source.text.contains('\0')
        || source.words.len() > MAX_WORDS
        || source.lines.is_empty()
        || source.lines.len() > MAX_PIECES
        || source
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            != source.text
        || source
            .words
            .iter()
            .any(|word| word.line >= source.lines.len())
    {
        return None;
    }
    let mut by_line = vec![Vec::new(); source.lines.len()];
    for (index, word) in source.words.iter().enumerate() {
        by_line[word.line].push((index, word));
    }
    let mut result = Vec::new();
    let mut sampling_budget = 0usize;
    for (line_index, line) in source.lines.iter().enumerate() {
        let mut pieces = Vec::new();
        if line_pieces(
            line_index,
            line,
            &by_line[line_index],
            &mut sampling_budget,
            &mut pieces,
        )
        .is_some()
        {
            result.extend(pieces);
        } else if !lenient {
            return None;
        } else if let Some(rect) = line.rect.filter(|rect| valid_rect(*rect)) {
            approximate(
                &mut result,
                line_index,
                0,
                &line.text,
                rect,
                line_index + MAX_WORDS,
            )?;
        }
        if result.len() > MAX_PIECES {
            return None;
        }
    }
    (!result.is_empty()).then_some(result)
}

fn line_pieces(
    line_index: usize,
    line: &RecognizedLine,
    words: &[(usize, &RecognizedWord)],
    sampling_budget: &mut usize,
    result: &mut Vec<Piece>,
) -> Option<()> {
    if line.horizontal_direction != HorizontalDirection::LeftToRight
        || line.text.len() > MAX_CELL_BYTES
    {
        return None;
    }
    // PP-OCR 的 rect 是词框联合，geometry 则保留完整裁行，不能要求二者相互嵌套。
    // 列聚类仍只消费有界有效水平投影；旋转行和缺失投影保留原复制路线。
    if !valid_rect(line.rect?) {
        return None;
    }
    let projection = full_crop_projection(line.geometry?)?;
    let start = projection.point(0., 0.5);
    let end = projection.point(1., 0.5);
    if end.x <= start.x || (end.y - start.y).abs() > (end.x - start.x) * MAX_LINE_SLOPE {
        // 单个数字等很短的单元格（如“8”）裁行不比高宽，方向本来分不清，检测会给出竖直阅读轴；
        // 这类短行按横向词框近似，不因此放弃整张表。宽框给出竖直或倾斜轴仍视为竖排／倾斜。
        let rect = line.rect?;
        if line.text.graphemes(true).count() <= 3 && rect.width <= rect.height {
            return approximate(
                result,
                line_index,
                0,
                &line.text,
                rect,
                line_index + MAX_WORDS,
            );
        }
        return None;
    }
    let refs: Vec<_> = words.iter().map(|(_, word)| *word).collect();
    if let Some(sampling) = &line.graphemes {
        *sampling_budget = sampling_budget.checked_add(sampling.run_count())?;
        if *sampling_budget > MAX_PIECES {
            return None;
        }
        if let Some(bands) = sampling.validated_bands(line_index, line, &refs) {
            let projection = full_crop_projection(sampling.source_geometry())?;
            for band in bands {
                let text = line.text.get(band.source_range.clone())?;
                if text.chars().all(char::is_whitespace) {
                    continue;
                }
                let points = [
                    projection.point(band.start, 0.),
                    projection.point(band.end, 0.),
                    projection.point(band.end, 1.),
                    projection.point(band.start, 1.),
                ];
                let left = points
                    .iter()
                    .map(|point| point.x)
                    .fold(f32::INFINITY, f32::min);
                let right = points
                    .iter()
                    .map(|point| point.x)
                    .fold(f32::NEG_INFINITY, f32::max);
                let top = points
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::INFINITY, f32::min);
                let bottom = points
                    .iter()
                    .map(|point| point.y)
                    .fold(f32::NEG_INFINITY, f32::max);
                if left < 0.
                    || top < 0.
                    || right > MAX_COORDINATE as f32
                    || bottom > MAX_COORDINATE as f32
                {
                    return None;
                }
                let rect = Rect {
                    x: left.floor() as u32,
                    y: top.floor() as u32,
                    width: (right.ceil() as u32).checked_sub(left.floor() as u32)?,
                    height: (bottom.ceil() as u32).checked_sub(top.floor() as u32)?,
                };
                if !valid_rect(rect) {
                    return None;
                }
                result.push(Piece {
                    line: line_index,
                    range: band.source_range,
                    rect,
                    word: line_index + MAX_WORDS,
                });
            }
            return Some(());
        }
    }
    if words.is_empty() {
        let rect = line.rect?;
        approximate(
            result,
            line_index,
            0,
            &line.text,
            rect,
            line_index + MAX_WORDS,
        )?;
    } else {
        let mut offset = 0usize;
        for (index, word) in words {
            if word.horizontal_direction != HorizontalDirection::LeftToRight
                || !valid_rect(word.rect)
                || word.geometry?.validated_in(word.rect).is_none()
            {
                return None;
            }
            let start = offset.checked_add(line.text.get(offset..)?.find(&word.text)?)?;
            if !line
                .text
                .get(offset..start)?
                .chars()
                .all(char::is_whitespace)
            {
                return None;
            }
            approximate(result, line_index, start, &word.text, word.rect, *index)?;
            offset = start.checked_add(word.text.len())?;
        }
        if !line.text.get(offset..)?.chars().all(char::is_whitespace) {
            return None;
        }
    }
    Some(())
}

fn approximate(
    result: &mut Vec<Piece>,
    line: usize,
    offset: usize,
    text: &str,
    rect: Rect,
    word: usize,
) -> Option<()> {
    if !valid_rect(rect) {
        return None;
    }
    let count = text.graphemes(true).count();
    if count == 0 || count > MAX_PIECES {
        return None;
    }
    for (index, (start, grapheme)) in text.grapheme_indices(true).enumerate() {
        if grapheme.chars().all(char::is_whitespace) {
            continue;
        }
        let x = rect.x + (u64::from(rect.width) * index as u64 / count as u64) as u32;
        let right = rect.x + (u64::from(rect.width) * (index + 1) as u64 / count as u64) as u32;
        let width = right.saturating_sub(x).max(1).min(rect.x + rect.width - x);
        result.push(Piece {
            line,
            range: offset + start..offset + start + grapheme.len(),
            rect: Rect {
                x,
                y: rect.y,
                width,
                height: rect.height,
            },
            word,
        });
    }
    Some(())
}

fn blocks(pieces: &[Piece]) -> Vec<Block> {
    let mut by_line: BTreeMap<usize, Vec<&Piece>> = BTreeMap::new();
    for piece in pieces {
        by_line.entry(piece.line).or_default().push(piece);
    }
    let mut blocks = Vec::new();
    for mut pieces in by_line.into_values() {
        pieces.sort_by_key(|piece| piece.rect.x);
        let mut current: Option<Block> = None;
        for piece in pieces {
            let rect = piece.rect;
            if let Some(block) = current.as_mut().filter(|block| {
                rect.x <= block.right + (block.height().min(rect.height) / 2).max(3)
            }) {
                block.right = block.right.max(rect.x + rect.width);
                block.top = block.top.min(rect.y);
                block.bottom = block.bottom.max(rect.y + rect.height);
            } else {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
                current = Some(Block {
                    left: rect.x,
                    right: rect.x + rect.width,
                    top: rect.y,
                    bottom: rect.y + rect.height,
                });
            }
        }
        if let Some(block) = current {
            blocks.push(block);
        }
    }
    blocks.sort_by_key(|block| (block.center_y(), block.left));
    blocks
}

fn alignment(pieces: &[Piece], detected: Option<&DetectedLines>) -> Option<TableGrid> {
    alignment_with(pieces, detected, false)
}

/// 仅合并有稳定短左标签的两列表：右列独占、贴近上一视觉行的续行归入同格。
/// 真实横线是逻辑行边界，即使下一行左格为空也不能跨线合并。
fn merge_key_value_rows(
    rows: &mut Vec<Row>,
    pieces: &[Piece],
    height: u32,
    detected: Option<&DetectedLines>,
) {
    let minimum_gap = (height * 4 / 5).max(8);
    let mut anchors = Vec::new();
    for row in rows.iter() {
        let mut spans: Vec<(u32, u32)> = Vec::new();
        for block in &row.blocks {
            match spans.last_mut() {
                Some(last) if block.left <= last.1 + minimum_gap => {
                    last.1 = last.1.max(block.right)
                }
                _ => spans.push((block.left, block.right)),
            }
        }
        let Some((index, _)) = spans
            .windows(2)
            .enumerate()
            .max_by_key(|(_, pair)| pair[1].0 - pair[0].1)
        else {
            continue;
        };
        let (left, end, right) = (spans[0].0, spans[index].1, spans[index + 1].0);
        if end - left <= height * 12 && right - end >= height * 2 {
            anchors.push((left, end, right));
        }
    }
    let Some(&(left, _, right)) = anchors.iter().max_by_key(|(left, _, right)| {
        anchors
            .iter()
            .filter(|(x, _, r)| x.abs_diff(*left) <= height && r.abs_diff(*right) <= height)
            .count()
    }) else {
        return;
    };
    let matching: Vec<_> = anchors
        .iter()
        .filter(|(x, _, r)| x.abs_diff(left) <= height && r.abs_diff(right) <= height)
        .collect();
    if matching.len() < 3 {
        return;
    }
    let left_end = matching.iter().map(|(_, end, _)| *end).max().unwrap();
    let right_start = matching.iter().map(|(_, _, start)| *start).min().unwrap();
    if right_start <= left_end + minimum_gap {
        return;
    }
    let cut = left_end + (right_start - left_end) / 2;
    // 所有左格都必须是短标签；跨列标题/正文交给局部候选或原有规则处理。
    for row in rows.iter() {
        if row
            .blocks
            .iter()
            .any(|block| block.left < cut && block.right > cut)
        {
            return;
        }
        let keys: Vec<_> = row.blocks.iter().filter(|block| block.left < cut).collect();
        if !keys.is_empty() {
            let count = pieces
                .iter()
                .filter(|piece| {
                    let y = piece.rect.y + piece.rect.height / 2;
                    piece.rect.x + piece.rect.width / 2 < cut && (row.top..row.bottom).contains(&y)
                })
                .count();
            if keys[0].left.abs_diff(left) > height
                || keys.last().unwrap().right > left_end + height
                || count > 12
            {
                return;
            }
        } else if row
            .blocks
            .first()
            .is_none_or(|block| block.left.abs_diff(right_start) > height)
        {
            return;
        }
    }
    let mut merged: Vec<Row> = Vec::new();
    for row in std::mem::take(rows) {
        let right_only = row.blocks.iter().all(|block| block.left >= cut);
        if let Some(previous) = merged.last_mut().filter(|previous| {
            right_only
                && previous.blocks.iter().any(|block| block.left < cut)
                && row.top.saturating_sub(previous.bottom) <= height * 3 / 5
                && !detected.is_some_and(|lines| {
                    lines.horizontal.iter().any(|line| {
                        (previous.bottom..=row.top).contains(&line.coordinate)
                            && line.start <= left_end
                            && line.end >= right_start
                    })
                })
        }) {
            previous.bottom = previous.bottom.max(row.bottom);
            previous.blocks.extend(row.blocks);
        } else {
            merged.push(row);
        }
    }
    *rows = merged;
}

/// `relaxed` 只用于手动表格识别：放宽行数、列留白支持行数与正文跨列，没有列留白时为单列。
fn alignment_with(
    pieces: &[Piece],
    detected: Option<&DetectedLines>,
    relaxed: bool,
) -> Option<TableGrid> {
    let blocks = blocks(pieces);
    let mut heights: Vec<_> = blocks.iter().map(Block::height).collect();
    heights.sort_unstable();
    let height = *heights.get(heights.len() / 2)?;
    let mut rows: Vec<Row> = Vec::new();
    for block in blocks {
        if let Some(row) = rows.last_mut().filter(|row| {
            let center = row.top + (row.bottom - row.top) / 2;
            center.abs_diff(block.center_y()) <= height.max(block.height()) * 3 / 5
        }) {
            row.top = row.top.min(block.top);
            row.bottom = row.bottom.max(block.bottom);
            row.blocks.push(block);
        } else {
            rows.push(Row {
                top: block.top,
                bottom: block.bottom,
                blocks: vec![block],
            });
        }
    }
    if rows.len() < if relaxed { 1 } else { 3 } || rows.len() > MAX_ROWS {
        return None;
    }
    for row in &mut rows {
        row.blocks.sort_by_key(|block| block.left);
    }
    merge_key_value_rows(&mut rows, pieces, height, detected);
    for row in &mut rows {
        row.blocks.sort_by_key(|block| block.left);
    }
    let left = rows
        .iter()
        .flat_map(|row| &row.blocks)
        .map(|block| block.left)
        .min()?;
    let right = rows
        .iter()
        .flat_map(|row| &row.blocks)
        .map(|block| block.right)
        .max()?;
    let mut events: BTreeMap<u32, i32> = BTreeMap::new();
    let minimum_gap = (height * 4 / 5).max(8);
    for row in &rows {
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for block in &row.blocks {
            if let Some(previous) = merged
                .last_mut()
                .filter(|previous| block.left <= previous.1 + minimum_gap)
            {
                previous.1 = previous.1.max(block.right);
            } else {
                merged.push((block.left, block.right));
            }
        }
        for pair in merged.windows(2) {
            *events.entry(pair[0].1).or_default() += 1;
            *events.entry(pair[1].0).or_default() -= 1;
        }
    }
    let needed = if relaxed {
        (rows.len() * 2).div_ceil(5).max(2).min(rows.len())
    } else {
        (rows.len() * 3).div_ceil(5).max(2)
    } as i32;
    let mut count = 0i32;
    let mut start = None;
    let mut cuts = Vec::new();
    let mut gaps = Vec::new();
    for (x, change) in events {
        count += change;
        if count >= needed {
            start.get_or_insert(x);
        } else if let Some(begin) = start.take() {
            if x - begin >= minimum_gap {
                cuts.push(begin + (x - begin) / 2);
                gaps.push((begin, x));
            }
        }
    }
    if (cuts.is_empty() && !relaxed) || cuts.len() >= MAX_COLUMNS {
        return None;
    }
    let mut column_edges = vec![left];
    column_edges.extend(cuts);
    column_edges.push(right);
    let mut row_edges = extent_edges(rows.len(), |row| Some((rows[row].top, rows[row].bottom)))?;
    let top = row_edges[0];
    let bottom = *row_edges.last()?;
    if let Some(lines) = detected {
        // 留白中点只是估计；留白里有真实竖线、文字上下有真实横线时吸附到线上，
        // 图上描边和复制的结构都与原表一致。
        let mut slots = vec![(left.saturating_sub(height * 3), left)];
        slots.extend(gaps.iter().copied());
        slots.push((right, right.saturating_add(height * 3)));
        snap_edges(&mut column_edges, &slots, &lines.vertical, (top, bottom));
        let mut slots = vec![(top.saturating_sub(height * 2), top)];
        slots.extend(rows.windows(2).map(|pair| (pair[0].bottom, pair[1].top)));
        slots.push((bottom, bottom.saturating_add(height * 2)));
        snap_edges(&mut row_edges, &slots, &lines.horizontal, (left, right));
    }
    // 只有横线时使用真实行边界，支持同一单元格内的多行文字。
    if let Some(lines) = detected.filter(|lines| lines.kind == LineKind::Ruled) {
        let mut y: Vec<_> = lines
            .horizontal
            .iter()
            .filter(|line| line.start <= left + height && line.end + height >= right)
            .map(|line| line.coordinate)
            .collect();
        y.sort_unstable();
        y.dedup();
        if y.len() >= 3
            && y.len() <= MAX_ROWS + 1
            && y[0] <= row_edges[0]
            && *y.last()? >= *row_edges.last()?
        {
            row_edges = y;
        }
    }
    let mut grid = TableGrid {
        row_edges,
        column_edges,
        cells: Vec::new(),
        confidence: 0.75,
        source: TableSource::Alignment,
    };
    // 只允许最上方表头跨列；正文切穿列分隔时淘汰这个候选。
    let mut spans: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for row in &rows {
        for block in &row.blocks {
            let row_index = containing(&grid.row_edges, block.center_y())?;
            let first = containing(&grid.column_edges, block.left)?;
            let last = containing(&grid.column_edges, block.right.saturating_sub(1))?;
            if last > first {
                if row_index != 0 && !relaxed {
                    return None;
                }
                let key = (row_index, first);
                let span = last - first + 1;
                if spans.iter().any(|((r, c), n)| {
                    *r == row_index && first < *c + *n && *c < first + span && *c != first
                }) {
                    return None;
                }
                spans.insert(key, span);
            }
        }
    }
    for row in 0..grid.rows() {
        let mut column = 0;
        while column < grid.columns() {
            let colspan = spans.get(&(row, column)).copied().unwrap_or(1);
            grid.cells.push(TableCell {
                row,
                column,
                rowspan: 1,
                colspan,
                text: String::new(),
                rect: grid.cell_rect(row, column, 1, colspan)?,
                confidence: grid.confidence,
            });
            column += colspan;
        }
    }
    let populated = pieces
        .iter()
        .filter_map(|piece| {
            Some((
                containing(&grid.row_edges, piece.rect.y + piece.rect.height / 2)?,
                containing(&grid.column_edges, piece.rect.x + piece.rect.width / 2)?,
            ))
        })
        .collect::<std::collections::BTreeSet<_>>();
    if !relaxed && populated.len() * 5 < grid.rows() * grid.columns() * 3 {
        return None;
    }
    grid.validate().then_some(grid)
}

/// 双线边框（网页表格的单元格间距、双线外框）会被线检测拆成很窄的空行／空列。
/// 没有文字的窄行／窄列并入相邻边界，外侧的直接去掉；真实合并格保持原网格。
fn collapse_thin(grid: TableGrid, pieces: &[Piece]) -> TableGrid {
    // 窄条不到正常行列的 1/4 且不超过文字高度：双线间距只有几像素，真实空行空列不受影响。
    let mut heights: Vec<u32> = pieces.iter().map(|piece| piece.rect.height).collect();
    heights.sort_unstable();
    let Some(&text_height) = heights.get(heights.len() / 2) else {
        return grid;
    };
    // 每条轴：哪些行（列）是没有文字的窄条。
    let thin = |edges: &[u32], centers: &[u32]| -> Option<Vec<bool>> {
        let count = edges.len().checked_sub(1)?;
        let occupied: Vec<bool> = (0..count)
            .map(|index| {
                centers
                    .iter()
                    .any(|center| (edges[index]..edges[index + 1]).contains(center))
            })
            .collect();
        let mut sizes: Vec<u32> = (0..count)
            .filter(|index| occupied[*index])
            .map(|index| edges[index + 1] - edges[index])
            .collect();
        sizes.sort_unstable();
        let normal = *sizes.get(sizes.len() / 2)?;
        Some(
            (0..count)
                .map(|index| {
                    let size = edges[index + 1] - edges[index];
                    !occupied[index] && size * 4 < normal && size <= text_height
                })
                .collect(),
        )
    };
    let collapse = |edges: &[u32], thin: &[bool]| -> Option<Vec<u32>> {
        let count = thin.len();
        let mut out = vec![edges[0]];
        for index in 0..count {
            let end = edges[index + 1];
            if !thin[index] {
                out.push(end);
            } else if index == 0 {
                out[0] = end;
            } else if index + 1 < count {
                let last = out.last_mut()?;
                *last = *last + (end - *last) / 2;
            }
        }
        (out.len() >= 2 && out.windows(2).all(|pair| pair[0] < pair[1])).then_some(out)
    };
    let x: Vec<u32> = pieces
        .iter()
        .map(|piece| piece.rect.x + piece.rect.width / 2)
        .collect();
    let y: Vec<u32> = pieces
        .iter()
        .map(|piece| piece.rect.y + piece.rect.height / 2)
        .collect();
    let (Some(thin_columns), Some(thin_rows)) =
        (thin(&grid.column_edges, &x), thin(&grid.row_edges, &y))
    else {
        return grid;
    };
    if !thin_columns.contains(&true) && !thin_rows.contains(&true) {
        return grid;
    }
    // 只有完全落在窄条里的合并格（双线之间没有隔线）可以随窄条去掉；真实合并格保持原网格。
    if grid.cells.iter().any(|cell| {
        (cell.rowspan != 1 || cell.colspan != 1)
            && !(cell.rowspan == 1 && thin_rows[cell.row])
            && !(cell.colspan == 1 && thin_columns[cell.column])
    }) {
        return grid;
    }
    let (Some(column_edges), Some(row_edges)) = (
        collapse(&grid.column_edges, &thin_columns),
        collapse(&grid.row_edges, &thin_rows),
    ) else {
        return grid;
    };
    let mut result = TableGrid {
        row_edges,
        column_edges,
        cells: Vec::new(),
        ..grid.clone()
    };
    for row in 0..result.rows() {
        for column in 0..result.columns() {
            let Some(rect) = result.cell_rect(row, column, 1, 1) else {
                return grid;
            };
            result.cells.push(TableCell {
                row,
                column,
                rowspan: 1,
                colspan: 1,
                text: String::new(),
                rect,
                confidence: result.confidence,
            });
        }
    }
    if result.validate() {
        result
    } else {
        grid
    }
}

/// `slots[i]` 是 `edges[i]` 可移动的范围（文字之间的留白）；只采用与表格跨度重叠过半的线段，
/// 吸附后仍须严格递增，否则保留原边界。
fn snap_edges(edges: &mut [u32], slots: &[(u32, u32)], lines: &[LineSegment], span: (u32, u32)) {
    if edges.len() != slots.len() || span.1 <= span.0 {
        return;
    }
    let need = (span.1 - span.0) / 2;
    let mut snapped = edges.to_vec();
    for (edge, (low, high)) in snapped.iter_mut().zip(slots) {
        let target = *edge;
        if let Some(coordinate) = lines
            .iter()
            .filter(|line| {
                (*low..=*high).contains(&line.coordinate)
                    && line.end.min(span.1).saturating_sub(line.start.max(span.0)) >= need
            })
            .map(|line| line.coordinate)
            .min_by_key(|coordinate| coordinate.abs_diff(target))
        {
            *edge = coordinate;
        }
    }
    if snapped.windows(2).all(|pair| pair[0] < pair[1]) {
        edges.copy_from_slice(&snapped);
    }
}

fn containing(edges: &[u32], position: u32) -> Option<usize> {
    if position < *edges.first()? || position > *edges.last()? {
        return None;
    }
    Some(
        edges
            .partition_point(|edge| *edge <= position)
            .saturating_sub(1)
            .min(edges.len() - 2),
    )
}

fn assign_text(
    source: &RecognizedText,
    pieces: &[Piece],
    mut grid: TableGrid,
) -> Option<TableGrid> {
    if !grid.validate() {
        return None;
    }
    let mut assigned: Vec<Vec<&Piece>> = vec![Vec::new(); grid.cells.len()];
    let mut row_cells = vec![Vec::new(); grid.rows()];
    for (index, cell) in grid.cells.iter().enumerate() {
        for row in cell.row..cell.row + cell.rowspan {
            row_cells[row].push(index);
        }
    }
    let mut buddies: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for piece in pieces {
        let center_x = piece.rect.x + piece.rect.width / 2;
        let center_y = piece.rect.y + piece.rect.height / 2;
        let row = containing(&grid.row_edges, center_y)?;
        let column = containing(&grid.column_edges, center_x)?;
        let mut candidates: Vec<_> = row_cells[row]
            .iter()
            .filter_map(|&index| {
                let cell = &grid.cells[index];
                let overlap = cell.rect.intersection(piece.rect)?;
                let area = u64::from(overlap.width) * u64::from(overlap.height);
                Some((
                    index,
                    area,
                    column >= cell.column && column < cell.column + cell.colspan,
                ))
            })
            .collect();
        candidates.sort_by_key(|(_, area, contains)| {
            (std::cmp::Reverse(*area), std::cmp::Reverse(*contains))
        });
        let &(mut chosen, best_area, _) = candidates.first()?;
        // 边界附近的短碎片：只有重叠接近时，采用同检测行/同词已经匹配的伙伴。
        if let Some(buddy) = buddies.get(&(piece.line, piece.word)) {
            if candidates
                .iter()
                .any(|(index, area, _)| index == buddy && *area * 10 >= best_area * 9)
            {
                chosen = *buddy;
            }
        }
        assigned[chosen].push(piece);
        buddies.insert((piece.line, piece.word), chosen);
    }
    for (cell, mut pieces) in grid.cells.iter_mut().zip(assigned) {
        pieces.sort_by_key(|piece| {
            (
                piece.rect.y + piece.rect.height / 2,
                piece.line,
                piece.range.start,
            )
        });
        // OCR 同一行的字素高度可有轻微差异；同一来源行按字节顺序复原。
        let mut lines: BTreeMap<(u32, usize), Vec<&Piece>> = BTreeMap::new();
        for piece in pieces {
            let y = source.lines[piece.line]
                .rect
                .map_or(piece.rect.y, |rect| rect.y);
            lines.entry((y, piece.line)).or_default().push(piece);
        }
        for ((_, line_index), mut pieces) in lines {
            if !cell.text.is_empty() {
                cell.text.push('\n');
            }
            pieces.sort_by_key(|piece| piece.range.start);
            let line = &source.lines[line_index].text;
            let mut previous_end = None;
            for piece in pieces {
                if let Some(end) = previous_end {
                    let gap = line.get(end..piece.range.start)?;
                    if gap.chars().all(char::is_whitespace) {
                        cell.text.push_str(gap);
                    }
                }
                cell.text.push_str(line.get(piece.range.clone())?);
                previous_end = Some(piece.range.end);
            }
        }
        if cell.text.len() > MAX_CELL_BYTES {
            return None;
        }
    }
    grid.validate().then_some(grid)
}

#[cfg(test)]
#[path = "table_layout_tests.rs"]
mod tests;
