//! 基于当前识别行及其局部版面的保守杂项建议；不改写识别正文或几何。
//! 分数和图像像素尚不在输入中，缺少相互印证的证据时始终保留。
use crate::{
    geometry::Rect,
    recognition::{geometry::Projection, HorizontalDirection, RecognizedLine, RecognizedText},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterReason {
    IsolatedSmallGraphic,
    DetachedGraphicColumn,
}

impl FilterReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::IsolatedSmallGraphic => "isolated-small-graphic",
            Self::DetachedGraphicColumn => "detached-graphic-column",
        }
    }
}

// 所有限制均约束当前模型的证据与工作量；不是置信度或正确率阈值。
const MAX_LINES: usize = 2_048;
const MAX_SOURCE_BYTES: usize = 1_048_576;
const MIN_BODY_LINES: usize = 3;

#[derive(Clone, Copy)]
struct BoxEvidence {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl BoxEvidence {
    fn new(rect: Rect) -> Option<Self> {
        let right = rect.x.checked_add(rect.width)?;
        let bottom = rect.y.checked_add(rect.height)?;
        (rect.width > 0 && rect.height > 0).then_some(Self {
            left: f64::from(rect.x),
            top: f64::from(rect.y),
            right: f64::from(right),
            bottom: f64::from(bottom),
        })
    }

    fn width(self) -> f64 {
        self.right - self.left
    }

    fn height(self) -> f64 {
        self.bottom - self.top
    }

    fn gap(self, other: Self) -> f64 {
        let x = (self.left - other.right)
            .max(other.left - self.right)
            .max(0.);
        let y = (self.top - other.bottom)
            .max(other.top - self.bottom)
            .max(0.);
        x.hypot(y)
    }

    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

#[derive(Clone, Copy)]
struct BodyEvidence {
    bounds: BoxEvidence,
    char_width: f64,
}

/// 每项对应原始行号；Some 只是一条可恢复的建议，不删除或重排行。
/// 短词、数字、代码标点和非拉丁文字不会仅因尺寸小或孤立而被过滤。
pub fn classify(result: &RecognizedText) -> Vec<Option<FilterReason>> {
    let mut decisions = classify_baseline(result);
    if !within_budget(result) {
        return decisions;
    }
    let bounds: Vec<_> = result.lines.iter().map(horizontal_bounds).collect();
    let bodies: Vec<_> = result
        .lines
        .iter()
        .zip(&bounds)
        .filter_map(|(line, bounds)| body_evidence(&line.text, (*bounds)?))
        .collect();
    if bodies.len() < MIN_BODY_LINES {
        return decisions;
    }
    let glyphs: Vec<_> = result
        .lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let bounds = bounds[index]?;
            let glyph = single_glyph(&line.text)?;
            (bounds.width() >= bounds.height() * 0.65 && bounds.width() <= bounds.height() * 1.5)
                .then_some(ShortGlyph {
                    index,
                    glyph,
                    bounds,
                })
        })
        .collect();
    // 大型表格或密集符号页没有足够确定的正文／图形分界。
    if glyphs.len() > 128 {
        return decisions;
    }
    for seed in &glyphs {
        let mut column: Vec<_> = glyphs
            .iter()
            .filter(|item| same_graphic_column(seed.bounds, item.bounds))
            .collect();
        column.sort_by(|left, right| left.bounds.top.total_cmp(&right.bounds.top));
        if column.len() > 12 {
            continue;
        }
        let mut first = 0;
        for end in 1..=column.len() {
            if end != column.len()
                && consecutive_graphics(column[end - 1].bounds, column[end].bounds)
            {
                continue;
            }
            let run = &column[first..end];
            if graphic_column_evidence(run, &bodies, &bounds) {
                for glyph in run {
                    decisions[glyph.index] = Some(FilterReason::DetachedGraphicColumn);
                }
            }
            first = end;
        }
    }
    decisions
}

/// 首版规则保留为评测基线；与生产分类使用同一份原始识别结果。
pub fn classify_baseline(result: &RecognizedText) -> Vec<Option<FilterReason>> {
    let mut decisions = vec![None; result.lines.len()];
    if !within_budget(result) {
        return decisions;
    }

    let bounds: Vec<_> = result.lines.iter().map(horizontal_bounds).collect();
    let bodies: Vec<_> = result
        .lines
        .iter()
        .zip(&bounds)
        .filter_map(|(line, bounds)| body_evidence(&line.text, (*bounds)?))
        .collect();
    if bodies.len() < MIN_BODY_LINES {
        return decisions;
    }
    let global_height = median(bodies.iter().map(|body| body.bounds.height()));

    for (index, line) in result.lines.iter().enumerate() {
        let Some(graphics) = graphic_count(&line.text) else {
            continue;
        };
        let Some(candidate) = bounds[index] else {
            continue;
        };
        // 在图像局部重新建立尺寸基线，避免大字号标题支配小字号正文。
        let mut local: Vec<_> = bodies
            .iter()
            .filter(|body| candidate.gap(body.bounds) <= global_height * 18.)
            .copied()
            .collect();
        local.sort_by(|left, right| {
            candidate
                .gap(left.bounds)
                .total_cmp(&candidate.gap(right.bounds))
        });
        local.truncate(5);
        if local.len() < MIN_BODY_LINES || !coherent_body(&local) {
            continue;
        }
        let height = median(local.iter().map(|body| body.bounds.height()));
        let char_width = median(local.iter().map(|body| body.char_width));
        if candidate.height() > height * 0.65
            || candidate.width() / graphics as f64 > char_width * 1.6
        {
            continue;
        }
        let body_bounds = local
            .iter()
            .map(|body| body.bounds)
            .reduce(BoxEvidence::union)
            .expect("局部正文至少有三行");
        // 正文区域内部的符号可能属于公式、表格或列表，保留供人工选择。
        if candidate.gap(body_bounds) <= height * 2.5 {
            continue;
        }
        if bounds.iter().enumerate().any(|(other, bounds)| {
            other != index
                && bounds.is_some_and(|bounds| supports_symbol(candidate, bounds, height))
        }) {
            continue;
        }
        decisions[index] = Some(FilterReason::IsolatedSmallGraphic);
    }
    decisions
}

fn within_budget(result: &RecognizedText) -> bool {
    result.lines.len() <= MAX_LINES
        && result.text.len() <= MAX_SOURCE_BYTES
        && source_matches_lines(result)
}

struct ShortGlyph {
    index: usize,
    glyph: char,
    bounds: BoxEvidence,
}

fn single_glyph(text: &str) -> Option<char> {
    let mut characters = text.trim().chars();
    let glyph = characters.next()?;
    if characters.any(|c| c != '\u{fe0f}') {
        return None;
    }
    (glyph.is_ascii_alphanumeric()
        || graphic_count(text).is_some()
        || matches!(
            glyph,
            '√' | '×' | '+' | '-' | '|' | '/' | '\\' | '<' | '>' | '^'
        ))
    .then_some(glyph)
}

fn same_graphic_column(left: BoxEvidence, right: BoxEvidence) -> bool {
    let center_delta = (left.left + left.right - right.left - right.right).abs() * 0.5;
    center_delta <= left.width().max(right.width()) * 0.35
        && (0.6..=1.7).contains(&(left.height() / right.height()))
        && (0.6..=1.7).contains(&(left.width() / right.width()))
}

fn consecutive_graphics(top: BoxEvidence, bottom: BoxEvidence) -> bool {
    let step = (bottom.top + bottom.bottom - top.top - top.bottom) * 0.5;
    let height = (top.height() + bottom.height()) * 0.5;
    step >= height * 0.95 && step <= height * 2.8 && bottom.top > top.bottom
}

fn graphic_column_evidence(
    column: &[&ShortGlyph],
    bodies: &[BodyEvidence],
    all_bounds: &[Option<BoxEvidence>],
) -> bool {
    if column.len() < MIN_BODY_LINES {
        return false;
    }
    let mut distinct: Vec<_> = column.iter().map(|item| item.glyph).collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < 3 || !column.iter().any(|item| item.glyph.is_ascii_alphabetic()) {
        return false;
    }
    let only_letters = column.iter().all(|item| item.glyph.is_ascii_alphabetic());
    let has_symbol = column
        .iter()
        .any(|item| !item.glyph.is_ascii_alphanumeric());
    // 纯数字列、混合字母编号以及顺序字母列表都可能是合法标签。
    if (!only_letters && !has_symbol)
        || (only_letters
            && (column.iter().any(|item| !item.glyph.is_ascii_uppercase())
                || sequential_letters(column)))
    {
        return false;
    }
    let column_bounds = column
        .iter()
        .map(|item| item.bounds)
        .reduce(BoxEvidence::union)
        .expect("图形列至少有三项");
    let global_height = median(bodies.iter().map(|body| body.bounds.height()));
    let mut local: Vec<_> = bodies
        .iter()
        .filter(|body| column_bounds.gap(body.bounds) <= global_height * 28.)
        .copied()
        .collect();
    local.sort_by(|left, right| {
        column_bounds
            .gap(left.bounds)
            .total_cmp(&column_bounds.gap(right.bounds))
    });
    local.truncate(5);
    if local.len() < MIN_BODY_LINES || !coherent_body(&local) {
        return false;
    }
    let body_height = median(local.iter().map(|body| body.bounds.height()));
    let body_char_width = median(local.iter().map(|body| body.char_width));
    let glyph_height = median(column.iter().map(|item| item.bounds.height()));
    let glyph_width = median(column.iter().map(|item| item.bounds.width()));
    if glyph_height < body_height * 1.2 || glyph_width < body_char_width * 1.8 {
        return false;
    }
    let body_bounds = local
        .iter()
        .map(|body| body.bounds)
        .reduce(BoxEvidence::union)
        .expect("局部正文至少有三行");
    if column_bounds.gap(body_bounds) <= body_height * 6. {
        return false;
    }
    !column.iter().any(|glyph| {
        all_bounds.iter().enumerate().any(|(index, bounds)| {
            !column.iter().any(|member| member.index == index)
                && bounds.is_some_and(|bounds| supports_symbol(glyph.bounds, bounds, body_height))
        })
    })
}

fn sequential_letters(column: &[&ShortGlyph]) -> bool {
    column
        .windows(2)
        .all(|pair| (pair[0].glyph as u32).abs_diff(pair[1].glyph as u32) == 1)
}

fn source_matches_lines(result: &RecognizedText) -> bool {
    let mut remaining = result.text.as_str();
    for (index, line) in result.lines.iter().enumerate() {
        if line.text.contains(['\r', '\n']) {
            return false;
        }
        let Some(rest) = remaining.strip_prefix(&line.text) else {
            return false;
        };
        remaining = rest;
        if index + 1 != result.lines.len() {
            let Some(rest) = remaining.strip_prefix('\n') else {
                return false;
            };
            remaining = rest;
        }
    }
    remaining.is_empty()
}

fn horizontal_bounds(line: &RecognizedLine) -> Option<BoxEvidence> {
    if line.horizontal_direction != HorizontalDirection::LeftToRight {
        return None;
    }
    let bounds = BoxEvidence::new(line.rect?)?;
    if bounds.height() > bounds.width() * 1.4 {
        return None;
    }
    if let Some(geometry) = line.geometry {
        let projection = Projection::new(geometry)?;
        let start = projection.point(0., 0.5);
        let end = projection.point(1., 0.5);
        let dx = f64::from(end.x) - f64::from(start.x);
        let dy = f64::from(end.y) - f64::from(start.y);
        // 旋转、竖排、反向阅读与明显斜行缺少对应的统计模型，明确保留。
        if dx <= 0.
            || dy.abs() > dx * 0.08
            || !axis_intersects_bounds([f64::from(start.x), f64::from(start.y)], [dx, dy], bounds)
        {
            return None;
        }
    }
    Some(bounds)
}

fn axis_intersects_bounds(start: [f64; 2], delta: [f64; 2], bounds: BoxEvidence) -> bool {
    let mut from = 0_f64;
    let mut to = 1_f64;
    for (axis, lower, upper) in [
        (0, bounds.left, bounds.right),
        (1, bounds.top, bounds.bottom),
    ] {
        if delta[axis].abs() < 1e-8 {
            if start[axis] < lower || start[axis] > upper {
                return false;
            }
            continue;
        }
        let first = (lower - start[axis]) / delta[axis];
        let last = (upper - start[axis]) / delta[axis];
        from = from.max(first.min(last));
        to = to.min(first.max(last));
        if from > to {
            return false;
        }
    }
    true
}

fn body_evidence(text: &str, bounds: BoxEvidence) -> Option<BodyEvidence> {
    let visible = text.chars().filter(|c| !c.is_whitespace()).count();
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    if letters < 4 || visible < 4 || bounds.width() < bounds.height() * 2.5 {
        return None;
    }
    let char_width = bounds.width() / visible as f64;
    (char_width >= bounds.height() * 0.15 && char_width <= bounds.height() * 1.5)
        .then_some(BodyEvidence { bounds, char_width })
}

fn coherent_body(bodies: &[BodyEvidence]) -> bool {
    let height = median(bodies.iter().map(|body| body.bounds.height()));
    let consistent = bodies
        .iter()
        .filter(|body| (height * 0.75..=height * 1.35).contains(&body.bounds.height()))
        .collect::<Vec<_>>();
    // 至少三行形成同侧对齐且相邻的正文，单个标题或散落文字不构成证据。
    consistent
        .iter()
        .filter(|body| {
            consistent.iter().any(|other| {
                !std::ptr::eq(**body, *other)
                    && (body.bounds.left - other.bounds.left).abs() <= height * 1.5
                    && body.bounds.gap(other.bounds) <= height * 4.
            })
        })
        .count()
        >= MIN_BODY_LINES
}

fn supports_symbol(candidate: BoxEvidence, other: BoxEvidence, height: f64) -> bool {
    if candidate.gap(other) <= height * 2.5 {
        return true;
    }
    let center_y = (candidate.top + candidate.bottom - other.top - other.bottom).abs() * 0.5;
    let center_x = (candidate.left + candidate.right - other.left - other.right).abs() * 0.5;
    // 同行列表符号、邻列单元格和同列短项都可能是有效内容。
    (center_y <= other.height().max(candidate.height()) * 0.75
        && candidate.gap(other) <= height * 6.)
        || (center_x <= other.width().max(candidate.width()) * 0.75
            && candidate.gap(other) <= height * 4.)
}

fn graphic_count(text: &str) -> Option<usize> {
    let mut count = 0;
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if c == '\u{fe0f}' && count != 0 {
            continue;
        }
        // 有意不用通用 punctuation/symbol 分类：数学、代码和文字标点需保留。
        if !matches!(
            c,
            '■' | '□'
                | '▢'
                | '▪'
                | '▫'
                | '●'
                | '○'
                | '◯'
                | '◉'
                | '◎'
                | '◆'
                | '◇'
                | '◈'
                | '▲'
                | '△'
                | '▼'
                | '▽'
                | '◀'
                | '▶'
                | '◁'
                | '▷'
                | '☰'
                | '☷'
                | '⚙'
                | '⌂'
                | '⌕'
                | '✕'
                | '✖'
                | '✚'
                | '✓'
                | '✔'
                | '✗'
                | '✘'
                | '\u{2500}'..='\u{259f}'
        ) {
            return None;
        }
        count += 1;
        if count > 3 {
            return None;
        }
    }
    (count != 0).then_some(count)
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<_> = values.collect();
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) * 0.5
    } else {
        values[middle]
    }
}

#[cfg(test)]
#[path = "ocr_result_filter_tests.rs"]
mod tests;
