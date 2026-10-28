//! 选字键盘操作只读取原文和既有行几何，返回带方向的 byte 端点。
use super::{Point, SelectionAnchor, SelectionText};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

#[path = "pin_text_word_line_navigation.rs"]
mod advanced;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyboardSelection {
    anchor: usize,
    caret: usize,
    preferred: Option<advanced::PreferredColumn>,
}
impl KeyboardSelection {
    pub(crate) fn caret_byte(self) -> usize {
        self.caret
    }

    pub(crate) fn range(self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavigationKey {
    ExtendPrevious,
    ExtendNext,
    Home,
    End,
    ExtendHome,
    ExtendEnd,
    PreviousWord,
    NextWord,
    ExtendPreviousWord,
    ExtendNextWord,
    ExtendPreviousLine,
    ExtendNextLine,
    DocumentHome,
    DocumentEnd,
    ExtendDocumentHome,
    ExtendDocumentEnd,
}

impl SelectionText {
    /// 鼠标的词／行选择以拖动方向的另一端为固定锚点；不从排序后的范围猜方向。
    pub(crate) fn keyboard_selection_from_drag(
        &self,
        anchor: &SelectionAnchor,
        point: Point,
    ) -> Option<KeyboardSelection> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return None;
        }
        self.text.get(anchor.range.clone())?;
        let target = self.extension_target(anchor, point)?;
        let range = anchor.range.start.min(target.start)..anchor.range.end.max(target.end);
        let forward = target.start >= anchor.range.start;
        let cursor = if forward {
            KeyboardSelection {
                preferred: None,
                anchor: range.start,
                caret: range.end,
            }
        } else {
            KeyboardSelection {
                preferred: None,
                anchor: range.end,
                caret: range.start,
            }
        };
        self.valid_keyboard_selection(cursor, &self.grapheme_boundaries())
            .then_some(cursor)
    }

    /// Ctrl+A 等没有方向的合法范围默认从 start 锚定到 end，也允许已有插入停点。
    pub(crate) fn keyboard_selection_from_range(
        &self,
        range: Range<usize>,
    ) -> Option<KeyboardSelection> {
        if range.start > range.end {
            return None;
        }
        let cursor = KeyboardSelection {
            preferred: None,
            anchor: range.start,
            caret: range.end,
        };
        self.valid_keyboard_selection(cursor, &self.grapheme_boundaries())
            .then_some(cursor)
    }

    /// 左右沿原文阅读顺序移动一个完整字素；Home/End 只使用当前已有识别行。
    /// 合法端点已在边界时仍返回 Some，调用方可消费按键而不退回贴图移动。
    pub(crate) fn navigate_keyboard(
        &self,
        cursor: KeyboardSelection,
        key: NavigationKey,
    ) -> Option<KeyboardSelection> {
        let boundaries = self.grapheme_boundaries();
        if !self.valid_keyboard_selection(cursor, &boundaries) {
            return None;
        }
        if advanced::handles(key) {
            return self.navigate_advanced(cursor, key, &boundaries);
        }
        let line = self.keyboard_line(cursor.caret, &boundaries)?;
        let index = boundaries.binary_search(&cursor.caret).ok()?;
        let caret = match key {
            NavigationKey::ExtendPrevious => boundaries[index.saturating_sub(1)],
            NavigationKey::ExtendNext => boundaries[(index + 1).min(boundaries.len() - 1)],
            NavigationKey::Home | NavigationKey::ExtendHome => line.start,
            NavigationKey::End | NavigationKey::ExtendEnd => line.end,
            _ => return None, // 新导航已由独立模块处理。
        };
        let next = KeyboardSelection {
            preferred: None,
            anchor: if matches!(key, NavigationKey::Home | NavigationKey::End) {
                caret
            } else {
                cursor.anchor
            },
            caret,
        };
        self.valid_keyboard_selection(next, &boundaries)
            .then_some(next)
    }

    fn grapheme_boundaries(&self) -> Vec<usize> {
        self.text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(self.text.len()))
            .collect()
    }

    fn valid_keyboard_selection(&self, cursor: KeyboardSelection, boundaries: &[usize]) -> bool {
        boundaries.binary_search(&cursor.anchor).is_ok()
            && boundaries.binary_search(&cursor.caret).is_ok()
            && self.keyboard_line(cursor.anchor, boundaries).is_some()
            && self.keyboard_line(cursor.caret, boundaries).is_some()
    }

    fn keyboard_line(&self, caret: usize, boundaries: &[usize]) -> Option<Range<usize>> {
        let mut matches = self.lines.iter().filter_map(|line| {
            if line.cells.is_empty() || self.text.get(line.range.clone()).is_none() {
                return None;
            }
            let mut range = line.range.clone();
            // OCR 按 LF 分行，来源范围可能含 CR；CRLF 全文字素不可在中间停下。
            if self.text.as_bytes().get(range.end) == Some(&b'\n')
                && range.end > range.start
                && self.text.as_bytes().get(range.end - 1) == Some(&b'\r')
            {
                range.end -= 1;
            }
            if boundaries.binary_search(&range.start).is_err()
                || boundaries.binary_search(&range.end).is_err()
                || caret < range.start
                || caret > range.end
            {
                return None;
            }
            Some(range)
        });
        let range = matches.next()?;
        // 原文 byte 停点不能证明属于哪一条重叠／相接行；保留其它键盘路线。
        matches.next().is_none().then_some(range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::Point,
        geometry::Rect,
        recognition::{
            HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
        },
    };

    fn rows(raw: &str) -> SelectionText {
        SelectionText::from_recognized(&RecognizedText {
            text: raw.into(),
            regions: Vec::new(),
            words: Vec::new(),
            lines: raw
                .split('\n')
                .enumerate()
                .map(|(index, content)| RecognizedLine {
                    graphemes: None,
                    text: content.into(),
                    rect: Some(Rect {
                        x: 10,
                        y: 10 + index as u32 * 50,
                        width: (content.graphemes(true).count() as u32 * 20).max(20),
                        height: 20,
                    }),
                    geometry: None,
                    horizontal_direction: HorizontalDirection::LeftToRight,
                })
                .collect(),
        })
    }

    fn navigate(
        text: &SelectionText,
        cursor: KeyboardSelection,
        key: NavigationKey,
    ) -> KeyboardSelection {
        text.navigate_keyboard(cursor, key).unwrap()
    }

    #[test]
    fn keyboard_graphemes_keep_combining_zwj_flags_and_crlf_whole() {
        let clusters = ["e\u{301}", "👨‍👩‍👧", "🇨🇳", "X", "\r\n", "中"];
        let raw = clusters.concat();
        let text = rows(&raw);
        let mut cursor = text.keyboard_selection_from_range(0..0).unwrap();
        let mut end = 0;
        for cluster in clusters {
            end += cluster.len();
            cursor = navigate(&text, cursor, NavigationKey::ExtendNext);
            assert_eq!(
                cursor,
                KeyboardSelection {
                    preferred: None,
                    anchor: 0,
                    caret: end
                }
            );
            assert_eq!(text.selected(cursor.range()).unwrap(), &raw[..end]);
        }
        assert_eq!(navigate(&text, cursor, NavigationKey::ExtendNext), cursor);
        for cluster in clusters.into_iter().rev() {
            end -= cluster.len();
            cursor = navigate(&text, cursor, NavigationKey::ExtendPrevious);
            assert_eq!(
                cursor,
                KeyboardSelection {
                    preferred: None,
                    anchor: 0,
                    caret: end
                }
            );
        }
        assert_eq!(cursor.range(), 0..0);
        assert_eq!(
            navigate(&text, cursor, NavigationKey::ExtendPrevious),
            cursor
        );
        assert_eq!(text.text, raw);
    }

    #[test]
    fn keyboard_anchor_survives_shrink_and_reverse_crossing() {
        let text = rows("abcd");
        let mut cursor = text.keyboard_selection_from_range(1..1).unwrap();
        for (key, caret) in [
            (NavigationKey::ExtendNext, 2),
            (NavigationKey::ExtendNext, 3),
            (NavigationKey::ExtendPrevious, 2),
            (NavigationKey::ExtendPrevious, 1),
            (NavigationKey::ExtendPrevious, 0),
            (NavigationKey::ExtendNext, 1),
            (NavigationKey::ExtendNext, 2),
        ] {
            cursor = navigate(&text, cursor, key);
            assert_eq!(
                cursor,
                KeyboardSelection {
                    preferred: None,
                    anchor: 1,
                    caret
                }
            );
            assert_eq!(cursor.range(), 1.min(caret)..1.max(caret));
        }
    }

    #[test]
    fn keyboard_home_end_use_caret_line_and_shift_keeps_anchor() {
        let text = rows("AB\r\nCDe\u{301}");
        let first = text.keyboard_selection_from_range(0..0).unwrap();
        let first_end = navigate(&text, first, NavigationKey::End);
        assert_eq!(
            first_end,
            KeyboardSelection {
                preferred: None,
                anchor: 2,
                caret: 2
            }
        );
        let second_start = navigate(&text, first_end, NavigationKey::ExtendNext);
        assert_eq!(
            second_start,
            KeyboardSelection {
                preferred: None,
                anchor: 2,
                caret: 4
            }
        );
        assert_eq!(text.selected(second_start.range()).unwrap(), "\r\n");
        let full = text.keyboard_selection_from_range(1..9).unwrap();
        let home = navigate(&text, full, NavigationKey::ExtendHome);
        assert_eq!(
            home,
            KeyboardSelection {
                preferred: None,
                anchor: 1,
                caret: 4
            }
        );
        assert_eq!(navigate(&text, home, NavigationKey::ExtendHome), home);
        assert_eq!(navigate(&text, home, NavigationKey::ExtendEnd), full);
        let collapsed_home = navigate(&text, full, NavigationKey::Home);
        assert_eq!(
            collapsed_home,
            KeyboardSelection {
                preferred: None,
                anchor: 4,
                caret: 4
            }
        );
        let collapsed_end = navigate(&text, collapsed_home, NavigationKey::End);
        assert_eq!(
            collapsed_end,
            KeyboardSelection {
                preferred: None,
                anchor: 9,
                caret: 9
            }
        );
        let reverse = KeyboardSelection {
            preferred: None,
            anchor: 9,
            caret: 4,
        };
        assert_eq!(
            navigate(&text, reverse, NavigationKey::ExtendEnd).range(),
            9..9
        );
        assert_eq!(navigate(&text, reverse, NavigationKey::ExtendHome), reverse);
    }

    #[test]
    fn keyboard_boundary_keys_return_some_without_changing_valid_selection() {
        let text = rows("中");
        let start = text.keyboard_selection_from_range(0..0).unwrap();
        let end = text.keyboard_selection_from_range(3..3).unwrap();
        for (cursor, key) in [
            (start, NavigationKey::ExtendPrevious),
            (start, NavigationKey::Home),
            (start, NavigationKey::ExtendHome),
            (end, NavigationKey::ExtendNext),
            (end, NavigationKey::End),
            (end, NavigationKey::ExtendEnd),
        ] {
            assert_eq!(text.navigate_keyboard(cursor, key), Some(cursor));
        }
    }

    #[test]
    fn keyboard_rejects_invalid_utf8_grapheme_and_crlf_endpoints() {
        let text = rows("e\u{301}👨‍👩‍👧🇨🇳\r\nX");
        for range in [
            1..1,
            1..3,
            4..4,
            10..10,
            29..30,
            0..usize::MAX,
            Range { start: 4, end: 3 },
        ] {
            assert!(text.keyboard_selection_from_range(range).is_none());
        }
        let crlf_start = "e\u{301}👨‍👩‍👧🇨🇳".len();
        assert!(text
            .keyboard_selection_from_range(crlf_start + 1..crlf_start + 1)
            .is_none());
        for cursor in [
            KeyboardSelection {
                preferred: None,
                anchor: 1,
                caret: 3,
            },
            KeyboardSelection {
                preferred: None,
                anchor: 0,
                caret: crlf_start + 1,
            },
            KeyboardSelection {
                preferred: None,
                anchor: 0,
                caret: usize::MAX,
            },
        ] {
            assert!(text
                .navigate_keyboard(cursor, NavigationKey::ExtendNext)
                .is_none());
        }
    }

    #[test]
    fn keyboard_requires_existing_unambiguous_endpoint_geometry() {
        let missing = SelectionText::from_recognized(&RecognizedText {
            text: "raw".into(),
            regions: Vec::new(),
            words: Vec::new(),
            lines: Vec::new(),
        });
        assert!(missing.keyboard_selection_from_range(0..3).is_none());
        let mut partial = rows("AB\nCD");
        partial.lines.pop();
        assert!(partial.keyboard_selection_from_range(0..5).is_none());
        let end = partial.keyboard_selection_from_range(2..2).unwrap();
        assert!(partial
            .navigate_keyboard(end, NavigationKey::ExtendNext)
            .is_none());
        assert_eq!(
            partial.navigate_keyboard(end, NavigationKey::End),
            Some(end)
        );
        let mut ambiguous = rows("AB");
        let line = &ambiguous.lines[0];
        let duplicate = super::super::Line {
            range: line.range.clone(),
            bounds: line.bounds,
            rotation: line.rotation,
            stops: line.stops.clone(),
            projected: line.projected,
            cells: line
                .cells
                .iter()
                .map(|cell| super::super::Cell {
                    range: cell.range.clone(),
                    bounds: cell.bounds,
                    projected: cell.projected,
                    source_id: cell.source_id,
                    reverse_x: cell.reverse_x,
                })
                .collect(),
        };
        ambiguous.lines.push(duplicate);
        assert!(ambiguous.keyboard_selection_from_range(0..0).is_none());
        assert!(rows("").keyboard_selection_from_range(0..0).is_none());
    }

    #[test]
    fn keyboard_drag_cursor_preserves_actual_caret_word_and_line_directions() {
        let text = rows("AB CD\nEF GH");
        for (clicks, down, up, expected_anchor, expected_caret) in [
            (
                1,
                Point { x: 11., y: 15. },
                Point { x: 109., y: 65. },
                0,
                11,
            ),
            (
                1,
                Point { x: 109., y: 65. },
                Point { x: 11., y: 15. },
                11,
                0,
            ),
            (
                2,
                Point { x: 22., y: 15. },
                Point { x: 109., y: 65. },
                0,
                11,
            ),
            (2, Point { x: 98., y: 65. }, Point { x: 11., y: 15. }, 11, 0),
            (
                3,
                Point { x: 22., y: 15. },
                Point { x: 109., y: 65. },
                0,
                11,
            ),
            (3, Point { x: 98., y: 65. }, Point { x: 11., y: 15. }, 11, 0),
        ] {
            let (anchor, _) = text.begin(down, clicks).unwrap();
            let cursor = text.keyboard_selection_from_drag(&anchor, up).unwrap();
            assert_eq!(
                cursor,
                KeyboardSelection {
                    preferred: None,
                    anchor: expected_anchor,
                    caret: expected_caret
                }
            );
            assert_eq!(cursor.range(), text.extend(&anchor, up));
            let mut drag_anchor = Some(anchor.clone());
            let mut selected = None;
            text.drag(&mut drag_anchor, &mut selected, up, true, true);
            assert_eq!(selected, Some(cursor.range()));
            let mut release_anchor = Some(anchor);
            let mut released = None;
            text.finish_drag(&mut release_anchor, &mut released, up, true);
            assert_eq!(released, Some(cursor.range()));
        }
        let (anchor, _) = text.begin(Point { x: 11., y: 15. }, 1).unwrap();
        assert!(text
            .keyboard_selection_from_drag(
                &anchor,
                Point {
                    x: f32::NAN,
                    y: 15.
                }
            )
            .is_none());
    }

    #[test]
    fn keyboard_unknown_range_defaults_forward_and_reverse_drag_shrinks_from_caret() {
        let text = rows("abcd");
        let unknown = text.keyboard_selection_from_range(1..3).unwrap();
        assert_eq!(
            unknown,
            KeyboardSelection {
                preferred: None,
                anchor: 1,
                caret: 3
            }
        );
        assert_eq!(
            navigate(&text, unknown, NavigationKey::ExtendPrevious).range(),
            1..2
        );
        let (anchor, _) = text.begin(Point { x: 89., y: 15. }, 1).unwrap();
        let reverse = text
            .keyboard_selection_from_drag(&anchor, Point { x: 11., y: 15. })
            .unwrap();
        assert_eq!(
            reverse,
            KeyboardSelection {
                preferred: None,
                anchor: 4,
                caret: 0
            }
        );
        let shrunk = navigate(&text, reverse, NavigationKey::ExtendNext);
        assert_eq!(
            shrunk,
            KeyboardSelection {
                preferred: None,
                anchor: 4,
                caret: 1
            }
        );
        assert_eq!(shrunk.range(), 1..4);
    }

    #[test]
    fn keyboard_line_drag_accepts_only_the_final_legal_crlf_endpoints() {
        let text = rows("AB\r\nC");
        let (first, old_first) = text.begin(Point { x: 11., y: 15. }, 3).unwrap();
        assert_eq!(old_first, 0..3);
        assert!(text.keyboard_selection_from_range(old_first).is_none());
        assert!(text
            .keyboard_selection_from_drag(&first, Point { x: 11., y: 15. })
            .is_none());
        let forward = text
            .keyboard_selection_from_drag(&first, Point { x: 29., y: 65. })
            .unwrap();
        assert_eq!(
            forward,
            KeyboardSelection {
                preferred: None,
                anchor: 0,
                caret: 5
            }
        );
        assert_eq!(
            forward.range(),
            text.extend(&first, Point { x: 29., y: 65. })
        );
        let (second, _) = text.begin(Point { x: 29., y: 65. }, 3).unwrap();
        let reverse = text
            .keyboard_selection_from_drag(&second, Point { x: 11., y: 15. })
            .unwrap();
        assert_eq!(
            reverse,
            KeyboardSelection {
                preferred: None,
                anchor: 5,
                caret: 0
            }
        );
        assert_eq!(
            reverse.range(),
            text.extend(&second, Point { x: 11., y: 15. })
        );
        assert_eq!(
            navigate(&text, reverse, NavigationKey::ExtendNext).range(),
            1..5
        );
    }

    #[test]
    fn keyboard_reversed_rectangle_keeps_logical_bytes_and_existing_highlights() {
        let text = SelectionText::from_recognized(&RecognizedText {
            text: "AB".into(),
            regions: Vec::new(),
            lines: Vec::new(),
            words: vec![RecognizedWord {
                text: "AB".into(),
                rect: Rect {
                    x: 40,
                    y: 10,
                    width: 40,
                    height: 20,
                },
                line: 0,
                geometry: None,
                horizontal_direction: HorizontalDirection::RightToLeft,
            }],
        });
        let (forward_anchor, _) = text.begin(Point { x: 79., y: 15. }, 1).unwrap();
        let forward = text
            .keyboard_selection_from_drag(&forward_anchor, Point { x: 79., y: 15. })
            .unwrap();
        let first = navigate(&text, forward, NavigationKey::ExtendNext);
        assert_eq!(first.range(), 0..1);
        assert_eq!(text.selected(first.range()).unwrap(), "A");
        assert_eq!(
            text.highlights(first.range()),
            vec![crate::transform::corners(crate::annotation::Bounds {
                x: 60.,
                y: 10.,
                width: 20.,
                height: 20.,
            })]
        );
        let (reverse_anchor, _) = text.begin(Point { x: 41., y: 15. }, 1).unwrap();
        let reverse = text
            .keyboard_selection_from_drag(&reverse_anchor, Point { x: 79., y: 15. })
            .unwrap();
        assert_eq!(
            reverse,
            KeyboardSelection {
                preferred: None,
                anchor: 2,
                caret: 0
            }
        );
        let last = navigate(&text, reverse, NavigationKey::ExtendNext);
        assert_eq!(last.range(), 1..2);
        assert_eq!(text.selected(last.range()).unwrap(), "B");
        assert_eq!(
            text.highlights(last.range()),
            vec![crate::transform::corners(crate::annotation::Bounds {
                x: 40.,
                y: 10.,
                width: 20.,
                height: 20.,
            })]
        );
    }

    #[test]
    fn keyboard_projected_180_and_slanted_sources_keep_reading_order_and_geometry() {
        for (rect, quad) in [
            (
                Rect {
                    x: 10,
                    y: 10,
                    width: 100,
                    height: 20,
                },
                [(110., 30.), (10., 30.), (10., 10.), (110., 10.)],
            ),
            (
                Rect {
                    x: 0,
                    y: 10,
                    width: 90,
                    height: 60,
                },
                [(10., 10.), (90., 50.), (80., 70.), (0., 30.)],
            ),
        ] {
            let geometry = TextGeometry {
                quad: quad.map(|(x, y)| Point { x, y }),
            };
            let text = SelectionText::from_recognized(&RecognizedText {
                text: "ABCD".into(),
                regions: Vec::new(),
                lines: Vec::new(),
                words: vec![RecognizedWord {
                    text: "ABCD".into(),
                    rect,
                    line: 0,
                    geometry: Some(geometry),
                    horizontal_direction: HorizontalDirection::RightToLeft,
                }],
            });
            assert!(text.lines[0].projected && text.lines[0].stops.is_empty());
            let original_highlights = text.highlights(0..4);
            let original_marks = text.marking_quads(0..4);
            let original_words = text.words.clone();
            let original_cells: Vec<_> = text.lines[0]
                .cells
                .iter()
                .map(super::super::cell_quad)
                .collect();
            let point = |u: f32| Point {
                x: ((quad[0].0 + quad[3].0) * (1. - u) + (quad[1].0 + quad[2].0) * u) * 0.5,
                y: ((quad[0].1 + quad[3].1) * (1. - u) + (quad[1].1 + quad[2].1) * u) * 0.5,
            };
            let (anchor, _) = text.begin(point(0.99), 1).unwrap();
            let reverse = text
                .keyboard_selection_from_drag(&anchor, point(0.01))
                .unwrap();
            assert_eq!(
                reverse,
                KeyboardSelection {
                    preferred: None,
                    anchor: 4,
                    caret: 0
                }
            );
            let next = navigate(&text, reverse, NavigationKey::ExtendNext);
            assert_eq!(next.range(), 1..4);
            assert_eq!(text.selected(next.range()).unwrap(), "BCD");
            assert_eq!(navigate(&text, next, NavigationKey::ExtendHome), reverse);
            assert_eq!(
                navigate(&text, next, NavigationKey::ExtendEnd).range(),
                4..4
            );
            assert_eq!(text.text, "ABCD");
            assert_eq!(text.words, original_words);
            assert_eq!(text.highlights(0..4), original_highlights);
            assert_eq!(text.marking_quads(0..4), original_marks);
            assert_eq!(
                text.lines[0]
                    .cells
                    .iter()
                    .map(super::super::cell_quad)
                    .collect::<Vec<_>>(),
                original_cells
            );
            assert!(text.lines[0].stops.is_empty());
        }
    }
}
