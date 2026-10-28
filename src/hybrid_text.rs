//! 有界的 UIA / 冻结 OCR 文本合并；本模块只处理文本及物理几何，不调用系统 API。
//!
//! OCR 的行及词框是选区局部坐标，UIA TextBlock 是全画布物理坐标。
//! 不从多行控件的大矩形猜测逐行边界，不以相交等同于完整覆盖。
//! 两字段 TextBlock 无法区分完整 TextRange 与 whole Value 控件，因此本版只用
//! normalized 文本相同且紧贴整行的 UIA 块代表该 OCR 行。不同文本保留 OCR。
//! 新增的无交叠单行 UIA 内容是控件当前值，不证明与冻结截图同时刻一致。
//! UIA 资格、密码子树、窗口身份、provider 超时和可见性由调用者检查。
use crate::{control_selection::TextBlock, geometry::Rect, recognition::RecognizedText};

const MAX_NATIVE_BLOCKS: usize = 1024;
const MAX_OCR_LINES: usize = 4096;

pub struct MergedText {
    pub text: String,
    pub native_lines: usize,
    pub ocr_lines: usize,
}

struct Item {
    text: String,
    rect: Rect,
    ordinal: usize,
    native: bool,
}

/// 保留所有 OCR 行；几何缺失/不一致时整体保留原 OCR 文本，不盲目追加 UIA。
/// 仅在有独立 UIA 行补充时按行带从上到下、从左到右排序；不推断多栏段落布局。
pub fn merge(selection: Rect, native: &[TextBlock], ocr: &RecognizedText) -> MergedText {
    if extent(selection).is_none()
        || native.len() > MAX_NATIVE_BLOCKS
        || ocr.lines.len() > MAX_OCR_LINES
        || ocr
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            != ocr.text
        || ocr.words.iter().any(|word| word.line >= ocr.lines.len())
    {
        return ocr_only(ocr);
    }
    let mut items = Vec::with_capacity(ocr.lines.len());
    for (index, line) in ocr.lines.iter().enumerate() {
        if single_line_key(&line.text).is_none() {
            return ocr_only(ocr);
        }
        let Some(local) = line.rect else {
            return ocr_only(ocr);
        };
        let Some(rect) = offset_inside(local, selection) else {
            return ocr_only(ocr);
        };
        let words: Vec<_> = ocr.words.iter().filter(|word| word.line == index).collect();
        if words.is_empty()
            || words
                .iter()
                .any(|word| offset_inside(word.rect, selection).is_none())
            || union_rects(words.iter().map(|word| word.rect)) != Some(local)
            || compact(
                &words
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<String>(),
            ) != compact(&line.text)
        {
            return ocr_only(ocr);
        }
        items.push(Item {
            text: line.text.clone(),
            rect,
            ordinal: index,
            native: false,
        });
    }

    let valid: Vec<_> = native
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let key = single_line_key(&block.text)?;
            contains_rect(selection, block.rect).then_some((index, block, key))
        })
        .collect();
    let mut added = false;
    for (position, (index, block, key)) in valid.iter().enumerate() {
        // 同位置相同内容的重复 provider 块只消费一次。重叠但不等价的 native
        // 几何/内容是歧义，不由遍历顺序任意挑一个。
        if valid
            .iter()
            .enumerate()
            .any(|(other_position, (_, other, other_key))| {
                other_position != position
                    && intersects(block.rect, other.rect)
                    && (*key != *other_key || !same_site(block.rect, other.rect))
            })
            || valid[..position].iter().any(|(_, other, other_key)| {
                *key == *other_key && same_site(block.rect, other.rect)
            })
        {
            continue;
        }
        // 仅与原 OCR 行比对，不能用先加入的 UIA 块冒充 OCR 的覆盖证据。
        let overlaps: Vec<_> = items[..ocr.lines.len()]
            .iter()
            .enumerate()
            .filter_map(|(line, item)| intersects(block.rect, item.rect).then_some(line))
            .collect();
        if let [line] = overlaps.as_slice() {
            let item = &mut items[*line];
            if !item.native
                && single_line_key(&item.text).as_ref() == Some(key)
                && tightly_covers(block.rect, item.rect)
            {
                item.text = block.text.trim().to_string();
                item.native = true;
            }
        } else if overlaps.is_empty() {
            items.push(Item {
                text: block.text.trim().to_string(),
                rect: block.rect,
                ordinal: ocr.lines.len() + index,
                native: true,
            });
            added = true;
        }
    }
    let native_lines = items.iter().filter(|item| item.native).count();
    let ocr_lines = items.len() - native_lines;
    if native_lines == 0 {
        return ocr_only(ocr);
    }
    if added {
        sort_reading_order(&mut items);
    }
    MergedText {
        text: items
            .into_iter()
            .map(|item| item.text)
            .collect::<Vec<_>>()
            .join("\n"),
        native_lines,
        ocr_lines,
    }
}

fn ocr_only(ocr: &RecognizedText) -> MergedText {
    MergedText {
        text: ocr.text.clone(),
        native_lines: 0,
        ocr_lines: ocr
            .text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    }
}

fn single_line_key(text: &str) -> Option<String> {
    if text.chars().any(|character| {
        matches!(
            character,
            '\r' | '\n' | '\0' | '\u{0085}' | '\u{2028}' | '\u{2029}' | '\u{000b}' | '\u{000c}'
        )
    }) {
        return None;
    }
    let key = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!key.is_empty()).then_some(key)
}

fn compact(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn extent(rect: Rect) -> Option<(u32, u32)> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    Some((
        rect.x.checked_add(rect.width)?,
        rect.y.checked_add(rect.height)?,
    ))
}

fn offset_inside(local: Rect, selection: Rect) -> Option<Rect> {
    let (right, bottom) = extent(local)?;
    if right > selection.width || bottom > selection.height {
        return None;
    }
    let rect = Rect {
        x: selection.x.checked_add(local.x)?,
        y: selection.y.checked_add(local.y)?,
        width: local.width,
        height: local.height,
    };
    contains_rect(selection, rect).then_some(rect)
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    match (extent(outer), extent(inner)) {
        (Some((right, bottom)), Some((inner_right, inner_bottom))) => {
            inner.x >= outer.x
                && inner.y >= outer.y
                && inner_right <= right
                && inner_bottom <= bottom
        }
        _ => false,
    }
}

fn intersects(left: Rect, right: Rect) -> bool {
    match (extent(left), extent(right)) {
        (Some((left_right, left_bottom)), Some((right_right, right_bottom))) => {
            left.x < right_right
                && right.x < left_right
                && left.y < right_bottom
                && right.y < left_bottom
        }
        _ => false,
    }
}

fn union_rects(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    let mut answer: Option<Rect> = None;
    for rect in rects {
        let (right, bottom) = extent(rect)?;
        answer = Some(if let Some(old) = answer {
            let (old_right, old_bottom) = extent(old)?;
            let x = old.x.min(rect.x);
            let y = old.y.min(rect.y);
            Rect {
                x,
                y,
                width: old_right.max(right).checked_sub(x)?,
                height: old_bottom.max(bottom).checked_sub(y)?,
            }
        } else {
            rect
        });
    }
    answer
}

fn tightly_covers(native: Rect, ocr: Rect) -> bool {
    let tolerance = (ocr.height / 4).clamp(1, 3);
    contains_rect(native, ocr)
        && native.width as u64 <= ocr.width as u64 + 2 * tolerance as u64
        && native.height as u64 <= ocr.height as u64 + 2 * tolerance as u64
}

/// 近乎相同且彼此包含的矩形才作为同一位置，不能按文本相同消去别处的重复词。
fn same_site(left: Rect, right: Rect) -> bool {
    tightly_covers(left, right) || tightly_covers(right, left)
}

fn same_row(left: Rect, right: Rect) -> bool {
    let (Some((_, left_bottom)), Some((_, right_bottom))) = (extent(left), extent(right)) else {
        return false;
    };
    let overlap = left_bottom
        .min(right_bottom)
        .saturating_sub(left.y.max(right.y));
    let smaller = left.height.min(right.height) as u64;
    let left_center = 2 * left.y as u64 + left.height as u64;
    let right_center = 2 * right.y as u64 + right.height as u64;
    overlap as u64 * 2 >= smaller && left_center.abs_diff(right_center) <= smaller
}

fn sort_reading_order(items: &mut Vec<Item>) {
    items.sort_by_key(|item| (item.rect.y, item.rect.x, item.ordinal));
    // 每个行带锚定首次真实 bbox；不累计扩张，避免 A接近B、B接近C 将三行串成一行。
    let mut rows: Vec<Vec<Item>> = Vec::new();
    for item in std::mem::take(items) {
        if let Some(row) = rows
            .last_mut()
            .filter(|row| same_row(row[0].rect, item.rect))
        {
            row.push(item);
        } else {
            rows.push(vec![item]);
        }
    }
    for mut row in rows {
        row.sort_by_key(|item| (item.rect.x, item.rect.y, item.ordinal));
        items.extend(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognition::{RecognizedLine, RecognizedWord};

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }
    fn block(text: &str, rect: Rect) -> TextBlock {
        TextBlock {
            text: text.into(),
            rect,
        }
    }
    fn recognized(lines: &[(&str, Option<Rect>)]) -> RecognizedText {
        RecognizedText {
            text: lines
                .iter()
                .map(|(text, _)| *text)
                .collect::<Vec<_>>()
                .join("\n"),
            regions: lines.iter().filter_map(|(_, rect)| *rect).collect(),
            words: lines
                .iter()
                .enumerate()
                .filter_map(|(line, (text, rect))| {
                    Some(RecognizedWord {
                        horizontal_direction: Default::default(),
                        geometry: None,
                        text: (*text).into(),
                        rect: (*rect)?,
                        line,
                    })
                })
                .collect(),
            lines: lines
                .iter()
                .map(|(text, rect)| RecognizedLine {
                    graphemes: None,
                    horizontal_direction: Default::default(),
                    geometry: None,
                    text: (*text).into(),
                    rect: *rect,
                })
                .collect(),
        }
    }
    fn selection() -> Rect {
        rect(100, 200, 300, 200)
    }
    #[test]
    fn no_native_preserves_ocr_exactly() {
        let ocr = recognized(&[
            ("left  spaces", Some(rect(10, 10, 80, 15))),
            ("uncovered suffix 100元", Some(rect(10, 40, 150, 15))),
        ]);
        let merged = merge(selection(), &[], &ocr);
        assert_eq!(merged.text, ocr.text);
        assert_eq!((merged.native_lines, merged.ocr_lines), (0, 2));
    }
    #[test]
    fn identical_complete_line_can_use_native_without_losing_uncovered_ocr() {
        let ocr = recognized(&[
            ("Hello world", Some(rect(10, 10, 100, 20))),
            ("OCR only", Some(rect(10, 40, 80, 20))),
        ]);
        let merged = merge(
            selection(),
            &[block("Hello world", rect(110, 210, 100, 20))],
            &ocr,
        );
        assert_eq!(merged.text, "Hello world\nOCR only");
        assert_eq!((merged.native_lines, merged.ocr_lines), (1, 1));
    }
    #[test]
    fn prefix_cannot_replace_complete_ocr_line_even_with_full_control_box() {
        let ocr = recognized(&[("Total 100元", Some(rect(10, 10, 100, 20)))]);
        let merged = merge(
            selection(),
            &[block("Total", rect(110, 210, 100, 20))],
            &ocr,
        );
        assert_eq!(merged.text, ocr.text);
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn differing_same_geometry_value_keeps_frozen_ocr() {
        let ocr = recognized(&[("old value", Some(rect(10, 10, 100, 20)))]);
        let merged = merge(
            selection(),
            &[block("new value", rect(110, 210, 100, 20))],
            &ocr,
        );
        assert_eq!(merged.text, "old value");
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn multiline_value_does_not_forge_per_line_boxes_or_append_duplicates() {
        let ocr = recognized(&[
            ("A", Some(rect(10, 10, 20, 15))),
            ("B", Some(rect(10, 40, 20, 15))),
        ]);
        let merged = merge(selection(), &[block("A\nB", rect(110, 210, 100, 70))], &ocr);
        assert_eq!(merged.text, "A\nB");
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn single_line_container_intersecting_two_rows_cannot_replace_either() {
        let ocr = recognized(&[
            ("A", Some(rect(10, 10, 20, 15))),
            ("B", Some(rect(10, 40, 20, 15))),
        ]);
        let merged = merge(selection(), &[block("A", rect(110, 210, 100, 70))], &ocr);
        assert_eq!(merged.text, "A\nB");
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn unicode_multiline_values_cannot_be_added_as_one_guessed_row() {
        let ocr = recognized(&[]);
        for separator in ['\u{0085}', '\u{2028}', '\u{2029}', '\u{000b}', '\u{000c}'] {
            let value = format!("A{separator}B");
            let merged = merge(selection(), &[block(&value, rect(110, 210, 100, 70))], &ocr);
            assert_eq!(merged.text, "");
            assert_eq!(merged.native_lines, 0);
        }
    }
    #[test]
    fn missing_line_geometry_keeps_complete_text_and_blocks_blind_supplement() {
        let ocr = recognized(&[
            ("has geometry", Some(rect(10, 10, 100, 20))),
            ("no geometry still retained", None),
        ]);
        let merged = merge(
            selection(),
            &[block(
                "could duplicate unknown row",
                rect(110, 270, 120, 20),
            )],
            &ocr,
        );
        assert_eq!(merged.text, ocr.text);
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn crop_offset_and_mixed_lines_use_full_canvas_coordinates() {
        let ocr = recognized(&[
            ("top", Some(rect(10, 10, 40, 20))),
            ("bottom", Some(rect(10, 80, 70, 20))),
        ]);
        let merged = merge(
            selection(),
            &[block("middle", rect(110, 250, 80, 20))],
            &ocr,
        );
        assert_eq!(merged.text, "top\nmiddle\nbottom");
        assert_eq!((merged.native_lines, merged.ocr_lines), (1, 2));
    }
    #[test]
    fn same_text_at_distinct_sites_is_preserved_and_exact_native_duplicates_removed() {
        let ocr = recognized(&[]);
        let merged = merge(
            selection(),
            &[
                block("OK", rect(110, 210, 20, 15)),
                block("OK", rect(110, 210, 20, 15)),
                block("OK", rect(110, 250, 20, 15)),
            ],
            &ocr,
        );
        assert_eq!(merged.text, "OK\nOK");
        assert_eq!((merged.native_lines, merged.ocr_lines), (2, 0));
    }
    #[test]
    fn conflicting_overlapping_native_blocks_are_not_arbitrarily_selected() {
        let ocr = recognized(&[("frozen", Some(rect(10, 10, 60, 20)))]);
        let merged = merge(
            selection(),
            &[
                block("first", rect(110, 250, 70, 20)),
                block("second", rect(110, 250, 70, 20)),
            ],
            &ocr,
        );
        assert_eq!(merged.text, "frozen");
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn clipped_and_overflowing_native_geometry_never_enters_output() {
        let ocr = recognized(&[]);
        let merged = merge(
            selection(),
            &[
                block("outside", rect(90, 210, 30, 20)),
                block("overflow", rect(u32::MAX - 1, 210, 10, 20)),
            ],
            &ocr,
        );
        assert_eq!(merged.text, "");
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn incoherent_ocr_line_text_and_real_words_keeps_ocr_without_guessing() {
        let mut ocr = recognized(&[("full text suffix", Some(rect(10, 10, 100, 20)))]);
        ocr.words[0].text = "full text".into();
        let merged = merge(
            selection(),
            &[block("full text suffix", rect(110, 210, 100, 20))],
            &ocr,
        );
        assert_eq!(merged.text, ocr.text);
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn orphan_ocr_words_and_legacy_without_line_metadata_keep_original_text() {
        let mut ocr = recognized(&[("legacy", Some(rect(10, 10, 70, 20)))]);
        ocr.words[0].line = 7;
        assert_eq!(merge(selection(), &[], &ocr).text, "legacy");
        ocr.words.clear();
        ocr.lines.clear();
        assert_eq!(
            merge(selection(), &[block("extra", rect(110, 250, 50, 20))], &ocr).text,
            "legacy"
        );
    }
    #[test]
    fn oversized_single_control_box_does_not_claim_tight_line_coverage() {
        let ocr = recognized(&[("same text", Some(rect(10, 10, 90, 20)))]);
        let merged = merge(
            selection(),
            &[block("same text", rect(105, 205, 130, 60))],
            &ocr,
        );
        assert_eq!(merged.text, "same text");
        assert_eq!(merged.native_lines, 0);
    }
    #[test]
    fn existing_ocr_order_is_retained_without_independent_native_additions() {
        let ocr = recognized(&[
            ("left lower", Some(rect(10, 80, 80, 20))),
            ("right upper", Some(rect(180, 10, 80, 20))),
        ]);
        let merged = merge(
            selection(),
            &[block("right upper", rect(280, 210, 80, 20))],
            &ocr,
        );
        assert_eq!(merged.text, "left lower\nright upper");
    }
    #[test]
    fn independent_same_row_values_are_ordered_left_to_right() {
        let ocr = recognized(&[("right", Some(rect(170, 11, 50, 20)))]);
        let merged = merge(selection(), &[block("left", rect(110, 210, 50, 20))], &ocr);
        assert_eq!(merged.text, "left\nright");
    }
}
