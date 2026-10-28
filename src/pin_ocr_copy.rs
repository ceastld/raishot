//! 贴图段落复制仅消费同一 OCR 结果的原文字素范围，不改动原图选字模型。
use crate::{
    recognition::layout::{CopyLayoutMode, OcrCopyLayout},
    Result,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// 紧凑复制排版加来源资格；不保留识别 DTO、词框或图片副本。
pub(super) struct ParagraphCopy<S: Copy + PartialEq> {
    layout: OcrCopyLayout,
    source: S,
    generation: u64,
}

impl<S: Copy + PartialEq> ParagraphCopy<S> {
    pub(super) fn new(layout: OcrCopyLayout, source: S, generation: u64) -> Self {
        Self {
            layout,
            source,
            generation,
        }
    }

    /// 菜单资格检查不生成复制正文，也不为字素端点分配临时表。
    pub(super) fn eligible(
        &self,
        selection_source: Option<S>,
        current_source: S,
        generation: u64,
        selection_text: &str,
        range: Option<&Range<usize>>,
    ) -> bool {
        selection_source == Some(self.source)
            && current_source == self.source
            && generation == self.generation
            && selection_text.as_bytes() == self.layout.text().as_bytes()
            && range.is_some_and(|range| legal_range(self.layout.text(), range))
    }

    pub(super) fn copy(
        &self,
        selection_source: Option<S>,
        current_source: S,
        generation: u64,
        selection_text: &str,
        range: Option<&Range<usize>>,
    ) -> Result<String> {
        if !self.eligible(
            selection_source,
            current_source,
            generation,
            selection_text,
            range,
        ) {
            return Err("段落复制须选择当前 OCR 结果的原文字素范围".into());
        }
        // eligible 已排除缺失与空范围；仍保持错误返回而不依赖 unwrap。
        let range = range.ok_or("段落复制缺少原文选区")?;
        self.layout.copy(range.clone(), CopyLayoutMode::Auto)
    }

    /// 仅由宿主已确认合法的标记／历史事务调用；新识别和正文编辑不能借此换源。
    pub(super) fn rebase(
        &mut self,
        before: Option<S>,
        after: S,
        generation: u64,
        selection_text: &str,
    ) -> bool {
        if before != Some(self.source)
            || generation != self.generation
            || selection_text.as_bytes() != self.layout.text().as_bytes()
        {
            return false;
        }
        self.source = after;
        true
    }
}

fn legal_range(text: &str, range: &Range<usize>) -> bool {
    if range.start >= range.end || range.end > text.len() {
        return false;
    }
    let mut start = range.start == 0;
    let mut end = range.end == text.len();
    if start && end {
        return true;
    }
    for (boundary, _) in text.grapheme_indices(true) {
        start |= boundary == range.start;
        end |= boundary == range.end;
        if start && end {
            return true;
        }
        if boundary > range.end {
            break;
        }
    }
    false
}

/// GPUI 字母键使用小写；宿主在命中后继续执行既有长按与输入优先门。
pub(super) fn shortcut(key: &str, modifiers: gpui::Modifiers) -> bool {
    key == "c"
        && modifiers.control
        && modifiers.alt
        && !modifiers.shift
        && !modifiers.platform
        && !modifiers.function
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        geometry::Rect,
        recognition::{HorizontalDirection, RecognizedLine, RecognizedText},
    };

    fn fixture(rows: &[&str], separator: &str) -> RecognizedText {
        RecognizedText {
            text: rows.join(separator),
            regions: Vec::new(),
            words: Vec::new(),
            lines: rows
                .iter()
                .enumerate()
                .map(|(index, text)| RecognizedLine {
                    graphemes: None,
                    text: (*text).into(),
                    rect: Some(Rect {
                        x: 20,
                        y: 20 + index as u32 * 26,
                        width: 200,
                        height: 20,
                    }),
                    horizontal_direction: HorizontalDirection::LeftToRight,
                    geometry: None,
                })
                .collect(),
        }
    }

    fn cache(result: &RecognizedText) -> ParagraphCopy<u64> {
        ParagraphCopy::new(OcrCopyLayout::new(result), 41, 7)
    }

    #[test]
    fn pin_paragraph_copy_auto_keeps_original_selection_body_and_range() {
        let result = fixture(&["English first line", "continues here"], "\n");
        let body = result.text.clone();
        let range = 0..body.len();
        let cache = cache(&result);
        drop(result);
        assert!(cache.eligible(Some(41), 41, 7, &body, Some(&range)));
        assert_eq!(
            cache.copy(Some(41), 41, 7, &body, Some(&range)).unwrap(),
            "English first line continues here"
        );
        assert_eq!(body, "English first line\ncontinues here");
        assert_eq!(range, 0..body.len());
        assert_eq!(
            cache.layout.copy(range, CopyLayoutMode::Original).unwrap(),
            body
        );
    }

    #[test]
    fn pin_paragraph_copy_partial_range_joins_only_with_both_sides_selected() {
        let result = fixture(&["English first line", "continues here"], "\n");
        let cache = cache(&result);
        let body = &result.text;
        let start = body.find("line").unwrap();
        let separator = body.find('\n').unwrap();
        let end = body.find("continues").unwrap() + "continues".len();
        for (range, expected) in [
            (start..end, "line continues"),
            (start..separator + 1, "line\n"),
            (separator..end, "\ncontinues"),
            (separator..separator + 1, "\n"),
        ] {
            assert_eq!(
                cache.copy(Some(41), 41, 7, body, Some(&range)).unwrap(),
                expected,
                "选区不能借未选中的相邻字素改写换行"
            );
        }
    }

    #[test]
    fn pin_paragraph_copy_cjk_joins_keep_original_utf8_bytes() {
        let result = fixture(&["这是正文第一行", "以及正文第二行"], "\r\n");
        let cache = cache(&result);
        let range = 0..result.text.len();
        assert_eq!(
            cache
                .copy(Some(41), 41, 7, &result.text, Some(&range))
                .unwrap(),
            "这是正文第一行以及正文第二行"
        );
        assert_eq!(cache.layout.text().as_bytes(), result.text.as_bytes());
        assert_eq!(result.text, "这是正文第一行\r\n以及正文第二行");
    }

    #[test]
    fn pin_paragraph_copy_rejects_utf8_combining_zwj_and_crlf_interiors() {
        let result = fixture(&["中e\u{301}👩‍💻", "tail"], "\r\n");
        let cache = cache(&result);
        let body = &result.text;
        let combining = body.find('e').unwrap() + 1;
        let zwj = body.find('👩').unwrap() + '👩'.len_utf8();
        let crlf = body.find("\r\n").unwrap() + 1;
        for range in [
            1..body.len(),
            0..2,
            combining..body.len(),
            0..combining,
            zwj..body.len(),
            0..zwj,
            crlf..body.len(),
            0..crlf,
            0..usize::MAX,
            Range { start: 4, end: 3 },
        ] {
            assert!(!cache.eligible(Some(41), 41, 7, body, Some(&range)));
            assert!(cache.copy(Some(41), 41, 7, body, Some(&range)).is_err());
        }
        let valid = 0.."中e\u{301}👩‍💻".len();
        assert!(cache.eligible(Some(41), 41, 7, body, Some(&valid)));
    }

    #[test]
    fn pin_paragraph_copy_empty_or_missing_selection_is_ineligible() {
        let result = fixture(&["正文"], "\n");
        let cache = cache(&result);
        assert!(!cache.eligible(Some(41), 41, 7, &result.text, None));
        assert!(cache.copy(Some(41), 41, 7, &result.text, None).is_err());
        for range in [0..0, result.text.len()..result.text.len()] {
            assert!(!cache.eligible(Some(41), 41, 7, &result.text, Some(&range)));
            assert!(cache
                .copy(Some(41), 41, 7, &result.text, Some(&range))
                .is_err());
        }
        let empty = fixture(&[], "\n");
        assert!(!self::cache(&empty).eligible(Some(41), 41, 7, "", Some(&(0..0))));
    }

    #[test]
    fn pin_paragraph_copy_requires_same_source_generation_and_body_bytes() {
        let result = fixture(&["first line", "second line"], "\n");
        let cache = cache(&result);
        let range = 0..result.text.len();
        for (selection_source, current_source, generation, body) in [
            (None, 41, 7, result.text.as_str()),
            (Some(42), 41, 7, result.text.as_str()),
            (Some(41), 42, 7, result.text.as_str()),
            (Some(41), 41, 8, result.text.as_str()),
            (Some(41), 41, 7, "First line\nsecond line"),
        ] {
            assert!(!cache.eligible(
                selection_source,
                current_source,
                generation,
                body,
                Some(&range),
            ));
            assert!(cache
                .copy(
                    selection_source,
                    current_source,
                    generation,
                    body,
                    Some(&range),
                )
                .is_err());
        }
    }

    #[test]
    fn pin_paragraph_copy_rebase_requires_prior_source_and_same_result() {
        let result = fixture(&["first line", "second line"], "\n");
        let mut cache = cache(&result);
        let range = 0..result.text.len();
        assert!(cache.rebase(Some(41), 42, 7, &result.text));
        assert!(!cache.eligible(Some(41), 41, 7, &result.text, Some(&range)));
        assert_eq!(
            cache
                .copy(Some(42), 42, 7, &result.text, Some(&range))
                .unwrap(),
            "first line second line"
        );
        for (before, generation, body) in [
            (None, 7, result.text.as_str()),
            (Some(41), 7, result.text.as_str()),
            (Some(42), 8, result.text.as_str()),
            (Some(42), 7, "First line\nsecond line"),
        ] {
            assert!(!cache.rebase(before, 43, generation, body));
            assert!(cache.eligible(Some(42), 42, 7, &result.text, Some(&range)));
            assert!(!cache.eligible(Some(43), 43, 7, &result.text, Some(&range)));
        }
    }

    #[test]
    fn pin_paragraph_copy_shortcut_requires_exact_control_alt_c() {
        let exact = gpui::Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert!(shortcut("c", exact));
        for key in ["C", "v", "", "copy"] {
            assert!(!shortcut(key, exact));
        }
        for modifiers in [
            gpui::Modifiers {
                control: false,
                ..exact
            },
            gpui::Modifiers {
                alt: false,
                ..exact
            },
            gpui::Modifiers {
                shift: true,
                ..exact
            },
            gpui::Modifiers {
                platform: true,
                ..exact
            },
            gpui::Modifiers {
                function: true,
                ..exact
            },
        ] {
            assert!(!shortcut("c", modifiers));
        }
    }
}
