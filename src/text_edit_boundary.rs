//! 键盘与鼠标的完整扩展字素停点；原生 UTF-16 输入范围不经过这里。
use std::ops::Range;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

fn char_floor(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(crate) fn floor(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let index = char_floor(text, index);
    let mut cursor = GraphemeCursor::new(index, text.len(), true);
    if cursor.is_boundary(text, 0) == Ok(true) {
        index
    } else {
        cursor.prev_boundary(text, 0).ok().flatten().unwrap_or(0)
    }
}

pub(crate) fn ceil(text: &str, index: usize) -> usize {
    let byte = char_floor(text, index);
    let mut cursor = GraphemeCursor::new(byte, text.len(), true);
    if byte == index && cursor.is_boundary(text, 0) == Ok(true) {
        byte
    } else {
        cursor
            .next_boundary(text, 0)
            .ok()
            .flatten()
            .unwrap_or(text.len())
    }
}

fn previous(text: &str, index: usize) -> usize {
    let byte = floor(text, index);
    if byte < index {
        byte
    } else {
        GraphemeCursor::new(byte, text.len(), true)
            .prev_boundary(text, 0)
            .ok()
            .flatten()
            .unwrap_or(0)
    }
}

fn next(text: &str, index: usize) -> usize {
    GraphemeCursor::new(char_floor(text, index), text.len(), true)
        .next_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(text.len())
}

// 与普通文字的 LF 硬换行合同一致；CRLF 作为一个字素，不生成中间停点。
fn line_bounds(text: &str, index: usize) -> Range<usize> {
    let index = floor(text, index);
    let start = text[..index].rfind('\n').map_or(0, |position| position + 1);
    let mut end = text[index..]
        .find('\n')
        .map_or(text.len(), |position| index + position);
    if end > start && text.as_bytes().get(end - 1) == Some(&b'\r') && end < text.len() {
        end -= 1;
    }
    start..end
}

/// 只用于键盘／鼠标的目标停点；原生 selected／marked 不调用此函数。
pub(crate) fn selection_to(
    text: &str,
    selection: Range<usize>,
    reversed: bool,
    target: usize,
    shift: bool,
) -> (usize, usize) {
    let target = floor(text, target);
    let anchor = if !shift {
        target
    } else if selection.is_empty() {
        let raw = selection.start.min(text.len());
        if target < raw {
            ceil(text, raw)
        } else {
            floor(text, raw)
        }
    } else if reversed {
        ceil(text, selection.end)
    } else {
        floor(text, selection.start)
    };
    (anchor, target)
}

/// 返回键盘动作的 (anchor, caret)，保留选择折叠、Shift 反向及短行夹止合同。
/// control 只改变 Home/End，普通输入原有 Ctrl+Left/Right 不新增词导航。
pub(crate) fn navigate(
    text: &str,
    selection: Range<usize>,
    reversed: bool,
    key: &str,
    shift: bool,
    control: bool,
) -> (usize, usize) {
    let raw_caret = if reversed {
        selection.start
    } else {
        selection.end
    }
    .min(text.len());
    let caret = floor(text, raw_caret);
    let target = match key {
        "left" if !shift && !selection.is_empty() => floor(text, selection.start),
        "right" if !shift && !selection.is_empty() => ceil(text, selection.end),
        "left" => previous(text, raw_caret),
        "right" => next(text, raw_caret),
        "home" if control => 0,
        "end" if control => text.len(),
        "home" => line_bounds(text, caret).start,
        "end" => line_bounds(text, caret).end,
        "up" | "down" => {
            let line = line_bounds(text, caret);
            let column = text[line.start..caret.min(line.end)]
                .graphemes(true)
                .count();
            let other = if key == "up" && line.start > 0 {
                Some(line_bounds(text, line.start - 1))
            } else if key == "down" && line.end < text.len() {
                Some(line_bounds(text, next(text, line.end)))
            } else {
                None
            };
            other.map_or(caret, |range| {
                text[range.clone()]
                    .grapheme_indices(true)
                    .nth(column)
                    .map_or(range.end, |(position, _)| range.start + position)
            })
        }
        _ => caret,
    };
    selection_to(text, selection, reversed, target, shift)
}

/// 用户删除扩展到完整字素；原生 IME 的 replacement/marked 范围保持码点精度。
pub(crate) fn deletion_range(text: &str, selection: Range<usize>, backward: bool) -> Range<usize> {
    let start = selection.start.min(selection.end).min(text.len());
    let end = selection.start.max(selection.end).min(text.len());
    let first = floor(text, start);
    let last = ceil(text, end);
    if start != end || first != last {
        return first..last;
    }
    if backward {
        previous(text, first)..first
    } else {
        first..next(text, first)
    }
}

/// 普通标注和单行字段只复制非空选择；无选择时不调用实际剪贴板写入器。
pub(crate) fn publish_selected(
    text: &str,
    selection: Range<usize>,
    write: impl FnOnce(&str),
) -> bool {
    let Some(selected) = text.get(selection).filter(|selected| !selected.is_empty()) else {
        return false;
    };
    write(selected);
    true
}

/// 普通插入／粘贴超过剩余容量时按完整字素截断，而不是整段丢弃；容量为码点数。
pub(crate) fn fit_insert(text: &str, capacity: usize) -> &str {
    if text.chars().count() <= capacity {
        return text;
    }
    let mut used = 0;
    let mut end = 0;
    for (position, grapheme) in text.grapheme_indices(true) {
        used += grapheme.chars().count();
        if used > capacity {
            break;
        }
        end = position + grapheme.len();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_insert_truncates_on_grapheme_boundary() {
        assert_eq!(fit_insert("abc", 5), "abc");
        assert_eq!(fit_insert("abc", 2), "ab");
        assert_eq!(fit_insert("abc", 0), "");
        // 家庭 emoji 为 7 个码点；容量不足时不拆半。
        assert_eq!(fit_insert("甲👨‍👩‍👧‍👦乙", 5), "甲");
        assert_eq!(fit_insert("甲👨‍👩‍👧‍👦乙", 8), "甲👨‍👩‍👧‍👦");
    }

    #[test]
    fn selected_copy_does_not_publish_empty_or_invalid_selection() {
        let mut clipboard = "保留旧剪贴板".to_owned();
        let text = "甲👨‍👩‍👧‍👦e\u{301}乙";
        for range in [0..0, 3..3, text.len()..text.len(), 4..5, 0..usize::MAX] {
            assert!(!publish_selected(text, range, |selected| clipboard = selected.to_owned()));
            assert_eq!(clipboard, "保留旧剪贴板");
        }
        assert!(publish_selected(text, 3..28, |selected| clipboard = selected.to_owned()));
        assert_eq!(clipboard, "👨‍👩‍👧‍👦");
    }
}
