//! 词／跨行导航只消费来源已有的范围与停点，未知几何安全拒绝。
use super::super::{cell_quad, Line, Projection};
use super::{KeyboardSelection, NavigationKey, Point, SelectionText};
use std::ops::Range;

const MAX_TEXT_BYTES: usize = 1_048_576;
const MAX_WORDS: usize = 32_768;
const MAX_LINES: usize = 4_096;
const MAX_LINE_CELLS: usize = 2_048;
const PARALLEL_COSINE: f64 = 0.996_194_698; // 阅读轴差异不超过 5°。

/// 固定大小的阅读轴与物理列；浮点位型用于完整保留且不改变选择的 Eq 合同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PreferredColumn {
    axis: [u64; 2],
    coordinate: u64,
}
impl PreferredColumn {
    fn new(axis: [f64; 2], point: Point) -> Self {
        Self {
            axis: axis.map(f64::to_bits),
            coordinate: dot(point, axis).to_bits(),
        }
    }
    fn values(self) -> Option<([f64; 2], f64)> {
        let axis = self.axis.map(f64::from_bits);
        let coordinate = f64::from_bits(self.coordinate);
        (axis.iter().all(|v| v.is_finite())
            && coordinate.is_finite()
            && coordinate.abs() <= 1_500_000.
            && (axis[0].hypot(axis[1]) - 1.).abs() <= 1e-6)
            .then_some((axis, coordinate))
    }
}

pub(super) fn handles(key: NavigationKey) -> bool {
    !matches!(
        key,
        NavigationKey::ExtendPrevious
            | NavigationKey::ExtendNext
            | NavigationKey::Home
            | NavigationKey::End
            | NavigationKey::ExtendHome
            | NavigationKey::ExtendEnd
    )
}

impl SelectionText {
    pub(super) fn navigate_advanced(
        &self,
        cursor: KeyboardSelection,
        key: NavigationKey,
        boundaries: &[usize],
    ) -> Option<KeyboardSelection> {
        if self.text.len() > MAX_TEXT_BYTES || self.lines.len() > MAX_LINES {
            return None;
        }
        if matches!(
            key,
            NavigationKey::ExtendPreviousLine | NavigationKey::ExtendNextLine
        ) {
            return self.navigate_reading_line(cursor, key, boundaries);
        }
        let extend = matches!(
            key,
            NavigationKey::ExtendPreviousWord
                | NavigationKey::ExtendNextWord
                | NavigationKey::ExtendDocumentHome
                | NavigationKey::ExtendDocumentEnd
        );
        let caret = match key {
            NavigationKey::PreviousWord | NavigationKey::NextWord if !cursor.range().is_empty() => {
                if key == NavigationKey::PreviousWord {
                    cursor.range().start
                } else {
                    cursor.range().end
                }
            }
            NavigationKey::PreviousWord | NavigationKey::ExtendPreviousWord => {
                self.word_destination(cursor.caret, false, boundaries)?
            }
            NavigationKey::NextWord | NavigationKey::ExtendNextWord => {
                self.word_destination(cursor.caret, true, boundaries)?
            }
            NavigationKey::DocumentHome | NavigationKey::ExtendDocumentHome => 0,
            NavigationKey::DocumentEnd | NavigationKey::ExtendDocumentEnd => self.text.len(),
            _ => return None,
        };
        let next = KeyboardSelection {
            anchor: if extend { cursor.anchor } else { caret },
            caret,
            preferred: None,
        };
        self.valid_keyboard_selection(next, boundaries)
            .then_some(next)
    }

    fn word_destination(&self, caret: usize, forward: bool, boundaries: &[usize]) -> Option<usize> {
        if self.words.len() > MAX_WORDS {
            return None;
        }
        let mut words = self.words.clone();
        words.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
        words.dedup();
        let mut rows: Vec<_> = self
            .lines
            .iter()
            .filter(|line| !line.cells.is_empty())
            .filter_map(|line| self.reading_row_range(line, boundaries))
            .collect();
        rows.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));
        if rows.windows(2).any(|pair| pair[1].start <= pair[0].end) {
            return None;
        }
        let mut row_index = 0;
        let mut previous_end = 0;
        for word in &words {
            let body = self.text.get(word.clone())?;
            if word.start >= word.end
                || word.start < previous_end
                || body.chars().all(char::is_whitespace)
                || body.chars().any(|ch| matches!(ch, '\r' | '\n'))
                || boundaries.binary_search(&word.start).is_err()
                || boundaries.binary_search(&word.end).is_err()
            {
                return None;
            }
            while rows.get(row_index).is_some_and(|row| row.end < word.start) {
                row_index += 1;
            }
            let row = rows.get(row_index)?;
            if word.start < row.start || word.end > row.end {
                return None;
            }
            previous_end = word.end;
        }
        let candidate = if forward {
            words.iter().find(|word| caret < word.end)
        } else {
            words.iter().rev().find(|word| word.start < caret)
        };
        if let Some(word) = candidate {
            let gap = if forward && caret < word.start {
                self.text.get(caret..word.start)?
            } else if !forward && word.end < caret {
                self.text.get(word.end..caret)?
            } else {
                ""
            };
            // 没有词范围的未知正文不能被跨过；空白／ASCII 标点明确按间隔处理。
            if gap
                .chars()
                .all(|ch| ch.is_whitespace() || ch.is_ascii_punctuation())
            {
                return Some(if forward { word.end } else { word.start });
            }
        }
        // 缺词／末端未覆盖正文按一个原文字素推进，不猜新的词边界。
        let index = boundaries.binary_search(&caret).ok()?;
        Some(if forward {
            boundaries[(index + 1).min(boundaries.len() - 1)]
        } else {
            boundaries[index.saturating_sub(1)]
        })
    }

    fn navigate_reading_line(
        &self,
        cursor: KeyboardSelection,
        key: NavigationKey,
        boundaries: &[usize],
    ) -> Option<KeyboardSelection> {
        let current = self.reading_row_index(cursor.caret, boundaries)?;
        let next = if key == NavigationKey::ExtendPreviousLine {
            match current.checked_sub(1) {
                Some(index) => index,
                None => return Some(cursor),
            }
        } else {
            if current + 1 == self.lines.len() {
                return Some(cursor);
            }
            current + 1
        };
        let a = self.reading_row_range(&self.lines[current], boundaries)?;
        let b = self.reading_row_range(&self.lines[next], boundaries)?;
        let (earlier, later) = if current < next { (&a, &b) } else { (&b, &a) };
        if earlier.end > later.start
            || !matches!(self.text.get(earlier.end..later.start)?, "\n" | "\r\n")
        {
            return None; // 不跨未归属正文／空行，亦不按物理坐标重排行。
        }
        let from = self.reading_frame(current, boundaries)?;
        let to = self.reading_frame(next, boundaries)?;
        let point = from.stops.iter().find(|stop| stop.0 == cursor.caret)?.1;
        let preferred = cursor
            .preferred
            .unwrap_or_else(|| PreferredColumn::new(from.axis, point));
        let (axis, coordinate) = preferred.values()?;
        if alignment(axis, from.axis) < PARALLEL_COSINE
            || alignment(axis, to.axis) < PARALLEL_COSINE
        {
            return None;
        }
        let normal = [-axis[1], axis[0]];
        let (a_normal, b_normal) = (from.interval(normal)?, to.interval(normal)?);
        let (a_along, b_along) = (from.interval(axis)?, to.interval(axis)?);
        if !(a_normal.1 + 0.01 < b_normal.0 || b_normal.1 + 0.01 < a_normal.0)
            || a_along.0.max(b_along.0) + 0.01 >= a_along.1.min(b_along.1)
        {
            return None;
        } // 重叠行、不同栏和不相容阅读方向不猜测。
        let caret = to
            .stops
            .iter()
            .min_by(|a, b| {
                (dot(a.1, axis) - coordinate)
                    .abs()
                    .total_cmp(&(dot(b.1, axis) - coordinate).abs())
                    .then_with(|| a.0.cmp(&b.0))
            })?
            .0;
        let next = KeyboardSelection {
            anchor: cursor.anchor,
            caret,
            preferred: Some(preferred),
        };
        self.valid_keyboard_selection(next, boundaries)
            .then_some(next)
    }

    fn reading_row_range(&self, line: &Line, boundaries: &[usize]) -> Option<Range<usize>> {
        let mut range = line.range.clone();
        self.text.get(range.clone())?;
        if self.text.as_bytes().get(range.end) == Some(&b'\n')
            && range.end > range.start
            && self.text.as_bytes()[range.end - 1] == b'\r'
        {
            range.end -= 1;
        }
        (range.start < range.end
            && boundaries.binary_search(&range.start).is_ok()
            && boundaries.binary_search(&range.end).is_ok()
            && !self
                .text
                .get(range.clone())?
                .chars()
                .all(char::is_whitespace))
        .then_some(range)
    }

    fn reading_row_index(&self, byte: usize, boundaries: &[usize]) -> Option<usize> {
        let row = self.keyboard_line(byte, boundaries)?;
        let mut matches = self.lines.iter().enumerate().filter_map(|(index, line)| {
            (self.reading_row_range(line, boundaries).as_ref() == Some(&row)).then_some(index)
        });
        let index = matches.next()?;
        matches.next().is_none().then_some(index)
    }

    fn reading_frame(&self, index: usize, boundaries: &[usize]) -> Option<ReadingFrame> {
        let line = self.lines.get(index)?;
        let range = self.reading_row_range(line, boundaries)?;
        if line.cells.is_empty() || line.cells.len() > MAX_LINE_CELLS {
            return None;
        }
        let mut endpoints = Vec::with_capacity(line.cells.len() * 2);
        let mut points = Vec::with_capacity(line.cells.len() * 4);
        let mut previous_end = range.start;
        for cell in &line.cells {
            if cell.range.start >= range.end {
                // 仅完整 CRLF 的 CR 尾格可被排除；缩短的行范围不能隐藏其它正文。
                if cell.range.start == range.end
                    && cell.range.end == line.range.end
                    && self.text.get(cell.range.clone()) == Some("\r")
                    && self.text.as_bytes().get(line.range.end) == Some(&b'\n')
                {
                    continue;
                }
                return None;
            }
            if cell.range.start < previous_end
                || cell.range.start < range.start
                || cell.range.end > range.end
                || cell.range.start >= cell.range.end
                || boundaries.binary_search(&cell.range.start).is_err()
                || boundaries.binary_search(&cell.range.end).is_err()
                || !self
                    .text
                    .get(previous_end..cell.range.start)?
                    .chars()
                    .all(char::is_whitespace)
            {
                return None;
            }
            if let Some(projected) = cell.projected {
                Projection::new(projected.source)?;
                if !projected.start.is_finite()
                    || !projected.end.is_finite()
                    || !(0.0..=1.0).contains(&projected.start)
                    || !(0.0..=1.0).contains(&projected.end)
                    || projected.start >= projected.end
                {
                    return None;
                }
            } else if ![
                cell.bounds.x,
                cell.bounds.y,
                cell.bounds.width,
                cell.bounds.height,
            ]
            .iter()
            .all(|v| v.is_finite())
                || cell.bounds.width <= 0.
                || cell.bounds.height <= 0.
            {
                return None;
            }
            let quad = cell_quad(cell).map(|point| line.rotation.map(point));
            if !quad.iter().all(|point| {
                point.x.is_finite()
                    && point.y.is_finite()
                    && point.x.abs() <= 1_000_000.
                    && point.y.abs() <= 1_000_000.
            }) {
                return None;
            }
            points.extend(quad);
            endpoints.extend([cell.range.start, cell.range.end]);
            previous_end = cell.range.end;
        }
        if !self
            .text
            .get(previous_end..range.end)?
            .chars()
            .all(char::is_whitespace)
        {
            return None;
        }
        endpoints.sort_unstable();
        endpoints.dedup();
        let stops: Option<Vec<_>> = endpoints
            .into_iter()
            .map(|byte| {
                let segment = self.caret_segment(byte)?;
                Some((
                    byte,
                    Point {
                        x: (segment[0].x + segment[1].x) * 0.5,
                        y: (segment[0].y + segment[1].y) * 0.5,
                    },
                ))
            })
            .collect();
        let stops = stops?;
        let (first, last) = (stops.first()?.1, stops.last()?.1);
        let delta = [
            f64::from(last.x) - f64::from(first.x),
            f64::from(last.y) - f64::from(first.y),
        ];
        let length = delta[0].hypot(delta[1]);
        if !(0.0001..=65_535.).contains(&length) {
            return None;
        }
        let axis = [delta[0] / length, delta[1] / length];
        if stops
            .windows(2)
            .any(|pair| dot(pair[1].1, axis) - dot(pair[0].1, axis) <= 0.0001)
        {
            return None;
        }
        Some(ReadingFrame {
            axis,
            stops,
            points,
        })
    }
}

struct ReadingFrame {
    axis: [f64; 2],
    stops: Vec<(usize, Point)>,
    points: Vec<Point>,
}
impl ReadingFrame {
    fn interval(&self, axis: [f64; 2]) -> Option<(f64, f64)> {
        let mut values = self.points.iter().map(|point| dot(*point, axis));
        let first = values.next()?;
        Some(values.fold((first, first), |(low, high), value| {
            (low.min(value), high.max(value))
        }))
    }
}
fn dot(point: Point, axis: [f64; 2]) -> f64 {
    f64::from(point.x) * axis[0] + f64::from(point.y) * axis[1]
}
fn alignment(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

#[cfg(test)]
#[path = "pin_text_word_line_navigation_tests.rs"]
mod tests;
