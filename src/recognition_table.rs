//! 保守表格复制：优先完整视觉行，可信大列间隔可拆同一行的既有词组。
//! 仅提供有界纯模型；不执行 OCR、写剪贴板或推测表格软件的类型转换。
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use super::{
    geometry::{Projection, ReadingDirection},
    HorizontalDirection, RecognizedText, TextGeometry,
};
use crate::geometry::Rect;

const MAX_SOURCE_BYTES: usize = 1_048_576;
const MAX_ROWS: usize = 256;
const MAX_COLUMNS: usize = 16;
const MAX_CELLS: usize = MAX_ROWS * MAX_COLUMNS;
const MAX_CELL_GRAPHEMES: usize = 32;
const MAX_WORDS: usize = 32_768;
const MAX_COORDINATE: u32 = 1_000_000;
const AXIS_TOLERANCE: f64 = 0.01;
const LOCAL_TOLERANCE: f64 = 1e-6;

#[derive(Clone, Debug, PartialEq)]
pub struct TableWordSource {
    pub source_range: Range<usize>,
    pub word_index: usize,
    pub rect: Rect,
    pub geometry: TextGeometry,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableCell {
    /// 原识别行编号；同一行拆出的多个格可复用该编号。
    pub source_line: usize,
    /// 原识别正文中单元格的 UTF-8 byte 范围，保留格内及整行两端普通空格。
    pub source_range: Range<usize>,
    /// TSV 编码范围；含可能添加的双引号，不冒充逐字符一一映射。
    pub output_range: Range<usize>,
    pub rect: Rect,
    pub geometry: TextGeometry,
    pub words: Vec<TableWordSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableSeparatorKind {
    Column,
    Row,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSeparator {
    /// 原行间唯一 '\n' 或同一行的完整列间普通空格；没有墨迹几何。
    pub source_range: Range<usize>,
    pub output_range: Range<usize>,
    pub kind: TableSeparatorKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OcrTableCopy {
    pub text: String,
    pub source_range: Range<usize>,
    pub rows: Vec<Vec<TableCell>>,
    pub separators: Vec<TableSeparator>,
    pub horizontal_direction: HorizontalDirection,
}

impl OcrTableCopy {
    /// 只有全部来源组成可信轴向网格且具有完整数字列时才返回 TSV。
    /// 不适用时调用方保留其它复制路线；本函数不改变任何识别数据。
    pub fn new(source: &RecognizedText) -> Option<Self> {
        let rows = atomic_rows(source).or_else(|| inline_rows(source))?;
        let direction = source.lines.first()?.horizontal_direction;
        let mut text = String::with_capacity(source.text.len());
        let mut separators = Vec::new();
        let mut output_rows = Vec::with_capacity(rows.len());
        let mut previous: Option<(usize, usize)> = None;
        for row in rows {
            let mut output_row = Vec::with_capacity(row.len());
            for (column, mut cell) in row.into_iter().enumerate() {
                if let Some((previous_end, previous_line)) = previous {
                    let kind = if column > 0 {
                        TableSeparatorKind::Column
                    } else {
                        TableSeparatorKind::Row
                    };
                    let source_range = previous_end..cell.source_range.start;
                    let separator = source.text.get(source_range.clone())?;
                    if previous_line == cell.source_line {
                        if kind != TableSeparatorKind::Column
                            || separator.is_empty()
                            || !only_spaces(separator)
                        {
                            return None;
                        }
                    } else if previous_line.checked_add(1)? != cell.source_line || separator != "\n"
                    {
                        return None;
                    }
                    let begin = text.len();
                    match kind {
                        TableSeparatorKind::Column => text.push('\t'),
                        TableSeparatorKind::Row => text.push_str("\r\n"),
                    }
                    separators.push(TableSeparator {
                        source_range,
                        output_range: begin..text.len(),
                        kind,
                    });
                } else if cell.source_range.start != 0 {
                    return None;
                }
                let begin = text.len();
                encode_cell(source.text.get(cell.source_range.clone())?, &mut text);
                cell.output_range = begin..text.len();
                previous = Some((cell.source_range.end, cell.source_line));
                output_row.push(cell);
            }
            output_rows.push(output_row);
        }
        if previous?.0 != source.text.len() {
            return None;
        }
        Some(Self {
            text,
            source_range: 0..source.text.len(),
            rows: output_rows,
            separators,
            horizontal_direction: direction,
        })
    }
}

fn atomic_rows(source: &RecognizedText) -> Option<Vec<Vec<TableCell>>> {
    let rows = physical_rows(source_cells(source, SourceMode::Atomic)?)?;
    (credible_grid(&rows) && numeric_column_evidence(source, &rows)).then_some(rows)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceMode {
    Atomic,
    Inline,
}

fn source_cells(source: &RecognizedText, mode: SourceMode) -> Option<Vec<TableCell>> {
    let line_budget = match mode {
        SourceMode::Atomic => 6..=MAX_CELLS,
        SourceMode::Inline => 3..=MAX_ROWS,
    };
    if source.text.is_empty()
        || source.text.len() > MAX_SOURCE_BYTES
        || !line_budget.contains(&source.lines.len())
        || source.words.is_empty()
        || source.words.len() > MAX_WORDS
        || source.words.len() != source.regions.len()
        || source
            .words
            .windows(2)
            .any(|pair| pair[0].line > pair[1].line)
        || source.words.last()?.line >= source.lines.len()
    {
        return None;
    }
    let direction = source.lines.first()?.horizontal_direction;
    let mut cells = Vec::with_capacity(source.lines.len());
    let mut offset = 0usize;
    let mut word_index = 0usize;
    for (line_index, line) in source.lines.iter().enumerate() {
        if line.text.len() > source.text.len()
            || line.horizontal_direction != direction
            || !readable_text(&line.text)
            || (mode == SourceMode::Atomic && !cell_text(&line.text))
        {
            return None;
        }
        let end = offset.checked_add(line.text.len())?;
        if source.text.get(offset..end)? != line.text {
            return None;
        }
        if line_index + 1 < source.lines.len() && source.text.as_bytes().get(end) != Some(&b'\n') {
            return None;
        }
        let rect = valid_rect(line.rect?)?;
        let geometry = line.geometry?;
        let projection = axis_projection(geometry, direction)?;
        let width = (f64::from(geometry.quad[1].x) - f64::from(geometry.quad[0].x)).abs();
        let line_height = (f64::from(geometry.quad[3].y) - f64::from(geometry.quad[0].y)).abs();
        let mut boundaries: Vec<_> = line.text.grapheme_indices(true).map(|(at, _)| at).collect();
        boundaries.push(line.text.len());
        let mut cursor = 0usize;
        let mut bounds = None;
        let mut words = Vec::new();
        let mut previous: Option<(f64, f64, f64)> = None;
        while let Some(word) = source
            .words
            .get(word_index)
            .filter(|word| word.line == line_index)
        {
            let word_rect = valid_rect(word.rect)?;
            if word_rect != source.regions[word_index]
                || word.horizontal_direction != direction
                || word.text.is_empty()
                || word.text.len() > line.text.len()
                || word.text.trim().is_empty()
            {
                return None;
            }
            let begin = cursor.checked_add(line.text.get(cursor..)?.find(&word.text)?)?;
            let word_end = begin.checked_add(word.text.len())?;
            if !only_spaces(line.text.get(cursor..begin)?)
                || boundaries.binary_search(&begin).is_err()
                || boundaries.binary_search(&word_end).is_err()
            {
                return None;
            }
            let word_geometry = word.geometry?;
            word_geometry.validated_in(word_rect)?;
            let word_projection = axis_projection(word_geometry, direction)?;
            // CTC 裁带及对称墨迹收紧均保留行的 v=.5；不同基线不合并为同一 cell。
            for u in [0., 1.] {
                let [_, v] = projection.inverse(word_projection.point(u, 0.5))?;
                if (v - 0.5).abs() * line_height > AXIS_TOLERANCE {
                    return None;
                }
            }
            let (first, last) = word_interval(projection, word_geometry)?;
            if let Some((previous_first, previous_last, previous_height)) = previous {
                let height = previous_height.min(f64::from(word_rect.height));
                let gap = (first - previous_last) * width;
                if first + LOCAL_TOLERANCE < previous_first
                    || last + LOCAL_TOLERANCE < previous_last
                    || gap < -1. - LOCAL_TOLERANCE * width
                    || (mode == SourceMode::Atomic && gap > height)
                {
                    return None;
                }
            }
            previous = Some((first, last, f64::from(word_rect.height)));
            bounds = Some(match bounds {
                Some(bounds) => rect_union(bounds, word_rect)?,
                None => word_rect,
            });
            words.push(TableWordSource {
                source_range: offset + begin..offset + word_end,
                word_index,
                rect: word_rect,
                geometry: word_geometry,
            });
            cursor = word_end;
            word_index += 1;
        }
        if bounds != Some(rect) || !only_spaces(line.text.get(cursor..)?) {
            return None;
        }
        cells.push(TableCell {
            source_line: line_index,
            source_range: offset..end,
            output_range: 0..0,
            rect,
            geometry,
            words,
        });
        offset = if line_index + 1 < source.lines.len() {
            end.checked_add(1)?
        } else {
            end
        };
    }
    (offset == source.text.len() && word_index == source.words.len()).then_some(cells)
}

fn cell_text(text: &str) -> bool {
    readable_text(text)
        && text.graphemes(true).take(MAX_CELL_GRAPHEMES + 1).count() <= MAX_CELL_GRAPHEMES
}

fn readable_text(text: &str) -> bool {
    !text.trim().is_empty()
        && !text.chars().any(|c| {
            c.is_control()
                || matches!(c, '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{200e}' | '\u{200f}'
                    | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

fn word_interval(projection: Projection, geometry: TextGeometry) -> Option<(f64, f64)> {
    let mut first = f64::INFINITY;
    let mut last = f64::NEG_INFINITY;
    for point in geometry.quad {
        let [u, v] = projection.inverse(point)?;
        if !(-LOCAL_TOLERANCE..=1. + LOCAL_TOLERANCE).contains(&u)
            || !(-LOCAL_TOLERANCE..=1. + LOCAL_TOLERANCE).contains(&v)
        {
            return None;
        }
        first = first.min(u);
        last = last.max(u);
    }
    Some((first, last))
}

fn inline_rows(source: &RecognizedText) -> Option<Vec<Vec<TableCell>>> {
    let lines = source_cells(source, SourceMode::Inline)?;
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        let row = inline_row(source, line)?;
        if row
            .iter()
            .enumerate()
            .skip(1)
            .any(|(at, cell)| !same_row(&row[..at], cell))
            || rows
                .first()
                .is_some_and(|first: &Vec<TableCell>| first.len() != row.len())
        {
            return None;
        }
        rows.push(row);
    }
    (credible_grid(&rows) && numeric_column_evidence(source, &rows)).then_some(rows)
}

fn inline_row(source: &RecognizedText, line: TableCell) -> Option<Vec<TableCell>> {
    let direction = source.lines[line.source_line].horizontal_direction;
    let projection = axis_projection(line.geometry, direction)?;
    let width = (f64::from(line.geometry.quad[1].x) - f64::from(line.geometry.quad[0].x)).abs();
    let tolerance = LOCAL_TOLERANCE * width;
    let mut starts = vec![0];
    for (at, pair) in line.words.windows(2).enumerate() {
        let (_, previous_last) = word_interval(projection, pair[0].geometry)?;
        let (next_first, _) = word_interval(projection, pair[1].geometry)?;
        let projected_gap = (next_first - previous_last) * width;
        let rect_gap = match direction {
            HorizontalDirection::LeftToRight => {
                f64::from(pair[1].rect.x)
                    - f64::from(pair[0].rect.x.checked_add(pair[0].rect.width)?)
            }
            HorizontalDirection::RightToLeft => {
                f64::from(pair[0].rect.x)
                    - f64::from(pair[1].rect.x.checked_add(pair[1].rect.width)?)
            }
        };
        let height = f64::from(pair[0].rect.height.min(pair[1].rect.height));
        let projected_column = gap_is_column(projected_gap, height, tolerance)?;
        let rect_column = gap_is_column(rect_gap, height, 0.)?;
        if projected_column != rect_column {
            return None;
        }
        if projected_column {
            let gap = source
                .text
                .get(pair[0].source_range.end..pair[1].source_range.start)?;
            if gap.is_empty() || !only_spaces(gap) || starts.len() == MAX_COLUMNS {
                return None;
            }
            starts.push(at + 1);
        }
    }
    if starts.len() < 2 {
        return None;
    }
    starts.push(line.words.len());
    let mut row = Vec::with_capacity(starts.len() - 1);
    for pair in starts.windows(2) {
        let words = line.words.get(pair[0]..pair[1])?;
        let first_word = words.first()?;
        let last_word = words.last()?;
        let begin = if pair[0] == 0 {
            line.source_range.start
        } else {
            first_word.source_range.start
        };
        let end = if pair[1] == line.words.len() {
            line.source_range.end
        } else {
            last_word.source_range.end
        };
        if !cell_text(source.text.get(begin..end)?) {
            return None;
        }
        let rect = words.iter().try_fold(first_word.rect, |bounds, word| {
            rect_union(bounds, word.rect)
        })?;
        // 来源顺序允许局部浮点微回退；格投影必须覆盖其所有原词四角。
        let (first, last) =
            words
                .iter()
                .try_fold((f64::INFINITY, f64::NEG_INFINITY), |(first, last), word| {
                    let (word_first, word_last) = word_interval(projection, word.geometry)?;
                    Some((first.min(word_first), last.max(word_last)))
                })?;
        let geometry = TextGeometry {
            quad: [
                projection.point(first, 0.),
                projection.point(last, 0.),
                projection.point(last, 1.),
                projection.point(first, 1.),
            ],
        };
        axis_projection(geometry, direction)?;
        row.push(TableCell {
            source_line: line.source_line,
            source_range: begin..end,
            output_range: 0..0,
            rect,
            geometry,
            words: words.to_vec(),
        });
    }
    Some(row)
}

fn gap_is_column(gap: f64, height: f64, tolerance: f64) -> Option<bool> {
    if gap < -1. - tolerance {
        None
    } else if gap <= height + tolerance {
        Some(false)
    } else if gap + tolerance >= 2. * height {
        Some(true)
    } else {
        None
    }
}

fn only_spaces(text: &str) -> bool {
    text.bytes().all(|c| c == b' ')
}

fn valid_rect(rect: Rect) -> Option<Rect> {
    rect.valid(MAX_COORDINATE, MAX_COORDINATE).then_some(rect)
}

fn rect_union(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = a.x.checked_add(a.width)?.max(b.x.checked_add(b.width)?);
    let bottom = a.y.checked_add(a.height)?.max(b.y.checked_add(b.height)?);
    valid_rect(Rect {
        x,
        y,
        width: right.checked_sub(x)?,
        height: bottom.checked_sub(y)?,
    })
}

fn axis_projection(geometry: TextGeometry, direction: HorizontalDirection) -> Option<Projection> {
    let projection = Projection::new(geometry)?;
    let q = geometry.quad.map(|p| [f64::from(p.x), f64::from(p.y)]);
    if q.iter().any(|p| p[0] < 0. || p[1] < 0.)
        || (q[1][1] - q[0][1]).abs() > AXIS_TOLERANCE
        || (q[2][1] - q[3][1]).abs() > AXIS_TOLERANCE
        || (q[3][0] - q[0][0]).abs() > AXIS_TOLERANCE
        || (q[2][0] - q[1][0]).abs() > AXIS_TOLERANCE
    {
        return None;
    }
    let expected = match direction {
        HorizontalDirection::LeftToRight => ReadingDirection::LeftToRight,
        HorizontalDirection::RightToLeft => ReadingDirection::RightToLeft,
    };
    (projection.direction() == expected).then_some(projection)
}

fn center_y(cell: &TableCell) -> f64 {
    f64::from(cell.rect.y) + f64::from(cell.rect.height) / 2.
}

fn physical_rows(cells: Vec<TableCell>) -> Option<Vec<Vec<TableCell>>> {
    let mut rows: Vec<Vec<TableCell>> = Vec::new();
    for cell in cells {
        if rows.last().is_some_and(|row| same_row(row, &cell)) {
            let row = rows.last_mut()?;
            if row.len() == MAX_COLUMNS {
                return None;
            }
            row.push(cell);
        } else {
            if rows.len() == MAX_ROWS {
                return None;
            }
            rows.push(vec![cell]);
        }
    }
    let columns = rows.first()?.len();
    (rows.len() >= 3
        && (2..=MAX_COLUMNS).contains(&columns)
        && rows.iter().all(|row| row.len() == columns))
    .then_some(rows)
}

fn same_row(row: &[TableCell], cell: &TableCell) -> bool {
    let mut min_center = center_y(cell);
    let mut max_center = min_center;
    let mut min_height = f64::from(cell.rect.height);
    let mut top = f64::from(cell.rect.y);
    let mut bottom = top + min_height;
    for other in row {
        min_center = min_center.min(center_y(other));
        max_center = max_center.max(center_y(other));
        min_height = min_height.min(f64::from(other.rect.height));
        top = top.max(f64::from(other.rect.y));
        bottom = bottom.min(f64::from(other.rect.y) + f64::from(other.rect.height));
    }
    max_center - min_center <= 0.2 * min_height && bottom - top >= 0.8 * min_height
}

fn credible_grid(rows: &[Vec<TableCell>]) -> bool {
    let mut min_height = f64::INFINITY;
    let mut max_height = f64::NEG_INFINITY;
    for cell in rows.iter().flatten() {
        min_height = min_height.min(f64::from(cell.rect.height));
        max_height = max_height.max(f64::from(cell.rect.height));
    }
    if max_height > min_height * 1.5 {
        return false;
    }
    let row_bounds: Vec<_> = rows
        .iter()
        .map(|row| {
            let top = row
                .iter()
                .map(|cell| f64::from(cell.rect.y))
                .fold(f64::INFINITY, f64::min);
            let bottom = row
                .iter()
                .map(|cell| f64::from(cell.rect.y) + f64::from(cell.rect.height))
                .fold(f64::NEG_INFINITY, f64::max);
            let center = row.iter().map(center_y).sum::<f64>() / row.len() as f64;
            (top, bottom, center)
        })
        .collect();
    let ascending_y = row_bounds[1].2 > row_bounds[0].2;
    let mut min_step = f64::INFINITY;
    let mut max_step = f64::NEG_INFINITY;
    for pair in row_bounds.windows(2) {
        let step = if ascending_y {
            pair[1].2 - pair[0].2
        } else {
            pair[0].2 - pair[1].2
        };
        let gap = if ascending_y {
            pair[1].0 - pair[0].1
        } else {
            pair[0].0 - pair[1].1
        };
        if gap < 0.25 * min_height || step <= 0. || step > 4. * min_height {
            return false;
        }
        min_step = min_step.min(step);
        max_step = max_step.max(step);
    }
    if max_step > min_step * 1.3 {
        return false;
    }
    let ascending_x = rows[0][1].rect.x > rows[0][0].rect.x;
    let mut previous_bounds: Option<(f64, f64)> = None;
    for column in 0..rows[0].len() {
        let mut left = [f64::INFINITY, f64::NEG_INFINITY];
        let mut right = left;
        let mut center = left;
        for row in rows {
            let cell = &row[column];
            let x = f64::from(cell.rect.x);
            let end = x + f64::from(cell.rect.width);
            for (range, value) in [
                (&mut left, x),
                (&mut right, end),
                (&mut center, (x + end) / 2.),
            ] {
                range[0] = range[0].min(value);
                range[1] = range[1].max(value);
            }
        }
        if [left, right, center]
            .iter()
            .all(|range| range[1] - range[0] > 0.2 * min_height)
        {
            return false;
        }
        if let Some((previous_left, previous_right)) = previous_bounds {
            let gutter = if ascending_x {
                left[0] - previous_right
            } else {
                previous_left - right[1]
            };
            if gutter < 2. * min_height {
                return false;
            }
        }
        previous_bounds = Some((left[0], right[1]));
    }
    true
}

fn numeric_column_evidence(source: &RecognizedText, rows: &[Vec<TableCell>]) -> bool {
    let numeric = |cell: &TableCell| numeric_shape(&source.text[cell.source_range.clone()]);
    (0..rows[0].len()).any(|column| rows.iter().all(|row| numeric(&row[column])))
        || (rows.len() >= 4
            && rows[0].iter().all(|cell| !numeric(cell))
            && (0..rows[0].len()).any(|column| rows[1..].iter().all(|row| numeric(&row[column]))))
}

fn numeric_shape(text: &str) -> bool {
    let bytes = text.trim_matches(' ').as_bytes();
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let begin = at;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    if at == begin {
        return false;
    }
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let begin = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == begin {
            return false;
        }
    }
    if bytes.get(at) == Some(&b'%') {
        at += 1;
    }
    at == bytes.len()
}

fn encode_cell(source: &str, output: &mut String) {
    if source.contains('"') {
        output.push('"');
        for c in source.chars() {
            output.push(c);
            if c == '"' {
                output.push('"');
            }
        }
        output.push('"');
    } else {
        output.push_str(source);
    }
}

#[cfg(test)]
#[path = "recognition_table_tests.rs"]
mod tests;
