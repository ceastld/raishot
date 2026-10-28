//! 只读 OCR 正文的字节停点；不修改文字、编辑历史或 IME 状态。
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// 实际 TextLayout 字节命中落在字素内部时，退到该完整扩展字素的起点。
/// 原文不做正规化；越过文末的命中仍夹到文末，CRLF 没有中间停点。
pub(crate) fn floor_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .take_while(|(start, _)| *start <= index)
        .map(|(start, _)| start)
        .last()
        .unwrap_or(0)
}

/// 返回只读选择的 (anchor, caret)，不支持的按键不接管。
/// Home/End 是原文硬换行边界；本函数不声称掌握 TextLayout 的视觉折行。
pub(crate) fn navigate(
    text: &str,
    range: Range<usize>,
    reversed: bool,
    key: &str,
    shift: bool,
    control: bool,
) -> Option<(usize, usize)> {
    if !matches!(key, "left" | "right" | "home" | "end") {
        return None;
    }
    let first = floor_boundary(text, range.start);
    let last = floor_boundary(text, range.end);
    let start = first.min(last);
    let end = first.max(last);
    let (anchor, caret) = if reversed { (end, start) } else { (start, end) };
    let next = match key {
        "left" if !shift && start != end => start,
        "right" if !shift && start != end => end,
        "left" if control => word_destination(text, caret, false),
        "right" if control => word_destination(text, caret, true),
        "left" => text
            .grapheme_indices(true)
            .take_while(|(start, _)| *start < caret)
            .map(|(start, _)| start)
            .last()
            .unwrap_or(0),
        "right" => text
            .grapheme_indices(true)
            .find(|(start, _)| *start > caret)
            .map_or(text.len(), |(start, _)| start),
        "home" if control => 0,
        "end" if control => text.len(),
        "home" => hard_line_bounds(text, caret).start,
        "end" => hard_line_bounds(text, caret).end,
        _ => unreachable!(),
    };
    Some((if shift { anchor } else { next }, next))
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    text.grapheme_indices(true)
        .find(|(start, _)| *start >= index)
        .map_or(text.len(), |(start, _)| start)
}

/// UAX29 分段同时保留词、中文和标点；不能用只保留字母数字的 unicode_words。
/// 当前非空白段内向左到段首、向右到段末；段外跳过空白后走一个非空白段。
/// 最后扩到字素边界，避免 Unicode 词规则与字素规则不重合时拆开字素。
fn word_destination(text: &str, caret: usize, forward: bool) -> usize {
    if forward {
        for (start, segment) in text.split_word_bound_indices() {
            let end = start + segment.len();
            if end <= caret || segment.chars().all(char::is_whitespace) {
                continue;
            }
            let next = ceil_boundary(text, end);
            if next > caret {
                return next;
            }
        }
        text.len()
    } else {
        for (start, segment) in text.split_word_bound_indices().rev() {
            if start >= caret || segment.chars().all(char::is_whitespace) {
                continue;
            }
            let next = floor_boundary(text, start);
            if next < caret {
                return next;
            }
        }
        0
    }
}

fn hard_line_bounds(text: &str, caret: usize) -> Range<usize> {
    let mut line_start = 0;
    for (start, grapheme) in text.grapheme_indices(true) {
        if !matches!(
            grapheme,
            "\r\n" | "\r" | "\n" | "\u{0085}" | "\u{2028}" | "\u{2029}"
        ) {
            continue;
        }
        if caret <= start {
            return line_start..start;
        }
        line_start = start + grapheme.len();
    }
    line_start..text.len()
}
