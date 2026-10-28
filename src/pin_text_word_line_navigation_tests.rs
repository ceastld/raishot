use super::*;
use crate::geometry::Rect;
use crate::recognition::{
    HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
};
use unicode_segmentation::UnicodeSegmentation;

fn fixture(contents: &[&str], axis: [f32; 2]) -> SelectionText {
    let normal = [-axis[1], axis[0]];
    let mut recognized = RecognizedText {
        text: contents.join("\n"),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (line, content) in contents.iter().enumerate() {
        let width = (content.graphemes(true).count() as f32 * 20.).max(20.);
        let origin = Point {
            x: 400. + normal[0] * line as f32 * 60.,
            y: 400. + normal[1] * line as f32 * 60.,
        };
        let p = |u: f32, v: f32| Point {
            x: origin.x + axis[0] * u + normal[0] * v,
            y: origin.y + axis[1] * u + normal[1] * v,
        };
        let quad = [p(0., 0.), p(width, 0.), p(width, 20.), p(0., 20.)];
        let low_x = quad
            .iter()
            .map(|p| p.x)
            .fold(f32::INFINITY, f32::min)
            .floor() as u32;
        let low_y = quad
            .iter()
            .map(|p| p.y)
            .fold(f32::INFINITY, f32::min)
            .floor() as u32;
        let rect = Rect {
            x: low_x,
            y: low_y,
            width: quad
                .iter()
                .map(|p| p.x)
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil() as u32
                - low_x,
            height: quad
                .iter()
                .map(|p| p.y)
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil() as u32
                - low_y,
        };
        let geometry = Some(TextGeometry { quad });
        let horizontal_direction = if axis[0] < 0. {
            HorizontalDirection::RightToLeft
        } else {
            HorizontalDirection::LeftToRight
        };
        recognized.words.push(RecognizedWord {
            text: (*content).into(),
            rect,
            line,
            horizontal_direction,
            geometry,
        });
        recognized.lines.push(RecognizedLine {
            graphemes: None,
            text: (*content).into(),
            rect: Some(rect),
            horizontal_direction,
            geometry,
        });
    }
    SelectionText::from_recognized(&recognized)
}
fn text(raw: &str) -> SelectionText {
    let mut value = fixture(&raw.split('\n').collect::<Vec<_>>(), [1., 0.]);
    value.words = raw
        .unicode_word_indices()
        .map(|(byte, word)| byte..byte + word.len())
        .collect();
    value
}
fn cursor(text: &SelectionText, byte: usize) -> KeyboardSelection {
    text.keyboard_selection_from_range(byte..byte).unwrap()
}
fn step(text: &SelectionText, cursor: KeyboardSelection, key: NavigationKey) -> KeyboardSelection {
    text.navigate_keyboard(cursor, key)
        .expect("来源合法的新导航必须返回实际端点")
}
fn shift_row(text: &mut SelectionText, row: usize, dx: f32, dy: f32) {
    let line = &mut text.lines[row];
    line.bounds.x += dx;
    line.bounds.y += dy;
    for stop in &mut line.stops {
        stop.1 += dx;
    }
    for cell in &mut line.cells {
        cell.bounds.x += dx;
        cell.bounds.y += dy;
        if let Some(projected) = &mut cell.projected {
            for point in &mut projected.source.quad {
                point.x += dx;
                point.y += dy;
            }
        }
    }
}

#[test]
fn word_navigation_reuses_source_words_and_skips_defined_intervals() {
    let value = text("one  two,three");
    assert_eq!(value.words, vec![0..3, 5..8, 9..14]);
    let original = value.text.clone();
    let mut current = cursor(&value, 0);
    for byte in [3, 8, 14, 14] {
        current = step(&value, current, NavigationKey::NextWord);
        assert_eq!(current.range(), byte..byte);
    }
    for byte in [9, 5, 0, 0] {
        current = step(&value, current, NavigationKey::PreviousWord);
        assert_eq!(current.range(), byte..byte);
    }
    assert_eq!(value.text, original);
}

#[test]
fn word_control_collapses_a_selection_before_another_jump() {
    let value = text("one two");
    let selected = value.keyboard_selection_from_range(1..6).unwrap();
    let previous = step(&value, selected, NavigationKey::PreviousWord);
    assert_eq!(previous.range(), 1..1);
    assert_eq!(
        step(&value, previous, NavigationKey::PreviousWord).range(),
        0..0
    );
    let next = step(&value, selected, NavigationKey::NextWord);
    assert_eq!(next.range(), 6..6);
    assert_eq!(step(&value, next, NavigationKey::NextWord).range(), 7..7);
}

#[test]
fn word_extension_keeps_reverse_anchor_when_shrinking_and_crossing() {
    let value = text("one two");
    let mut current = KeyboardSelection {
        anchor: 4,
        caret: 0,
        preferred: None,
    };
    current = step(&value, current, NavigationKey::ExtendNextWord);
    assert_eq!(
        (current.anchor, current.caret, current.range()),
        (4, 3, 3..4)
    );
    current = step(&value, current, NavigationKey::ExtendNextWord);
    assert_eq!(
        (current.anchor, current.caret, current.range()),
        (4, 7, 4..7)
    );
    current = step(&value, current, NavigationKey::ExtendPreviousWord);
    assert_eq!(
        (current.anchor, current.caret, current.range()),
        (4, 4, 4..4)
    );
    current = step(&value, current, NavigationKey::ExtendPreviousWord);
    assert_eq!(
        (current.anchor, current.caret, current.range()),
        (4, 0, 0..4)
    );
}

#[test]
fn word_ranges_exact_duplicates_and_unsorted_input_are_deterministic() {
    let mut value = text("one two");
    value.words = vec![4..7, 0..3, 4..7, 0..3];
    assert_eq!(
        step(&value, cursor(&value, 0), NavigationKey::NextWord).caret_byte(),
        3
    );
    assert_eq!(
        step(&value, cursor(&value, 3), NavigationKey::NextWord).caret_byte(),
        7
    );
    assert_eq!(value.words, vec![4..7, 0..3, 4..7, 0..3]);
}

#[test]
fn word_ranges_overlap_reverse_outside_and_unknown_line_are_rejected() {
    for ranges in [
        vec![0..3, 1..3],
        vec![Range { start: 3, end: 2 }],
        vec![Range { start: 0, end: 99 }],
        vec![Range { start: 3, end: 3 }],
        vec![Range { start: 3, end: 4 }],
        vec![Range { start: 0, end: 5 }],
    ] {
        let mut value = text("one\ntwo");
        value.words = ranges;
        assert!(value
            .navigate_keyboard(cursor(&value, 0), NavigationKey::NextWord)
            .is_none());
    }
    let mut value = text("one\ntwo");
    value.lines.pop();
    assert!(value
        .navigate_keyboard(cursor(&value, 0), NavigationKey::NextWord)
        .is_none());
}

#[test]
fn word_missing_ranges_advance_whole_graphemes_without_inventing_words() {
    let mut value = text("👩‍🔬e\u{301}Z");
    value.words.clear();
    let mut current = cursor(&value, 0);
    for cluster in ["👩‍🔬", "e\u{301}", "Z"] {
        let old = current.caret;
        current = step(&value, current, NavigationKey::ExtendNextWord);
        assert_eq!(value.text.get(old..current.caret), Some(cluster));
    }
    assert_eq!(
        step(&value, current, NavigationKey::ExtendNextWord),
        current
    );
    current = step(&value, current, NavigationKey::ExtendPreviousWord);
    assert_eq!(current.caret, value.text.len() - 1);
}

#[test]
fn word_unknown_body_between_words_is_not_silently_skipped() {
    let mut value = text("one ABC two");
    value.words = vec![0..3, 8..11];
    let next = step(&value, cursor(&value, 3), NavigationKey::NextWord);
    assert_eq!(next.caret, 4);
    let previous = step(&value, cursor(&value, 8), NavigationKey::PreviousWord);
    assert_eq!(previous.caret, 7);
}

#[test]
fn word_crlf_and_unicode_ranges_keep_complete_original_bytes() {
    let value = text("e\u{301}\r\n中国");
    let first_end = "e\u{301}".len();
    let current = step(&value, cursor(&value, 0), NavigationKey::ExtendNextWord);
    assert_eq!(current.caret, first_end);
    let next = step(&value, current, NavigationKey::ExtendNextWord);
    assert_eq!(value.selected(next.range()).unwrap(), "e\u{301}\r\n中");
    assert!(value
        .keyboard_selection_from_range(first_end + 1..first_end + 1)
        .is_none());
    let mut bad = text("e\u{301}X");
    bad.words = vec![Range { start: 0, end: 1 }];
    assert!(bad
        .navigate_keyboard(cursor(&bad, 0), NavigationKey::ExtendNextWord)
        .is_none());
}

#[test]
fn document_boundaries_collapse_or_extend_without_changing_original_text() {
    let value = text("A\r\n👩‍🔬中");
    let selected = value
        .keyboard_selection_from_range(1..value.text.len())
        .unwrap();
    assert_eq!(
        step(&value, selected, NavigationKey::DocumentHome).range(),
        0..0
    );
    assert_eq!(
        step(&value, selected, NavigationKey::DocumentEnd).range(),
        value.text.len()..value.text.len()
    );
    assert_eq!(
        step(&value, selected, NavigationKey::ExtendDocumentHome).range(),
        0..1
    );
    let reverse = KeyboardSelection {
        anchor: value.text.len(),
        caret: 1,
        preferred: None,
    };
    assert_eq!(
        step(&value, reverse, NavigationKey::ExtendDocumentHome).range(),
        0..value.text.len()
    );
    assert_eq!(
        step(&value, reverse, NavigationKey::ExtendDocumentEnd).range(),
        value.text.len()..value.text.len()
    );
    assert_eq!(value.text, "A\r\n👩‍🔬中");
}

#[test]
fn document_boundaries_require_owned_first_and_last_geometry() {
    let mut value = text("first\nlast");
    value.lines.remove(0);
    assert!(value
        .navigate_keyboard(cursor(&value, 6), NavigationKey::DocumentHome)
        .is_none());
    let mut value = text("first\nlast");
    value.lines.pop();
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::DocumentEnd)
        .is_none());
    let value = text("first\n");
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendDocumentEnd)
        .is_none());
}

#[test]
fn line_navigation_long_short_long_restores_original_physical_column() {
    let value = fixture(&["0123456789", "ab", "ABCDEFGHIJ"], [1., 0.]);
    let mut current = cursor(&value, 7);
    for caret in [13, 21] {
        current = step(&value, current, NavigationKey::ExtendNextLine);
        assert_eq!(current.caret, caret);
        assert_eq!(current.anchor, 7);
    }
    for caret in [13, 7] {
        current = step(&value, current, NavigationKey::ExtendPreviousLine);
        assert_eq!(current.caret, caret);
        assert_eq!(current.anchor, 7);
    }
    assert_eq!(current.range(), 7..7);
    assert_eq!(
        step(&value, current, NavigationKey::ExtendPreviousLine),
        current
    );
}

#[test]
fn line_navigation_180_vertical_both_directions_and_slanted_keep_source_order() {
    for axis in [[-1., 0.], [0., 1.], [0., -1.], [0.8, 0.6]] {
        let value = fixture(&["0123456789", "ab", "ABCDEFGHIJ"], axis);
        let a = step(&value, cursor(&value, 7), NavigationKey::ExtendNextLine);
        let b = step(&value, a, NavigationKey::ExtendNextLine);
        assert_eq!((a.caret, b.caret, b.anchor), (13, 21, 7));
        let c = step(&value, b, NavigationKey::ExtendPreviousLine);
        assert_eq!(step(&value, c, NavigationKey::ExtendPreviousLine).caret, 7);
    }
}

#[test]
fn line_navigation_uses_physical_stop_column_instead_of_character_count() {
    let mut value = fixture(&["0123456789", "ab", "ABCDEFGHIJ"], [1., 0.]);
    shift_row(&mut value, 1, 100., 0.);
    let a = step(&value, cursor(&value, 6), NavigationKey::ExtendNextLine);
    assert_eq!(a.caret, 12); // 物理列 x=520 对应缩进行的第一个字符后，而非行尾。
    assert_eq!(step(&value, a, NavigationKey::ExtendNextLine).caret, 20);
}

#[test]
fn line_navigation_non_vertical_keys_and_new_mouse_cursor_reset_preferred_column() {
    let value = fixture(&["0123456789", "ab", "ABCDEFGHIJ"], [1., 0.]);
    let a = step(&value, cursor(&value, 7), NavigationKey::ExtendNextLine);
    assert!(a.preferred.is_some());
    for key in [
        NavigationKey::ExtendPrevious,
        NavigationKey::ExtendNext,
        NavigationKey::Home,
        NavigationKey::End,
        NavigationKey::ExtendHome,
        NavigationKey::ExtendEnd,
        NavigationKey::PreviousWord,
        NavigationKey::NextWord,
        NavigationKey::ExtendPreviousWord,
        NavigationKey::ExtendNextWord,
        NavigationKey::DocumentHome,
        NavigationKey::DocumentEnd,
        NavigationKey::ExtendDocumentHome,
        NavigationKey::ExtendDocumentEnd,
    ] {
        let reset = step(&value, a, key);
        assert!(reset.preferred.is_none());
    }
    let reset = step(&value, a, NavigationKey::Home);
    assert_eq!(step(&value, reset, NavigationKey::ExtendNextLine).caret, 14);
    let reset = value.keyboard_selection_from_range(a.range()).unwrap();
    assert!(reset.preferred.is_none());
    let (anchor, _) = value.begin(Point { x: 401., y: 465. }, 1).unwrap();
    let mouse = value
        .keyboard_selection_from_drag(&anchor, Point { x: 439., y: 465. })
        .unwrap();
    assert!(mouse.preferred.is_none());
    assert_eq!(step(&value, mouse, NavigationKey::ExtendNextLine).caret, 16);
}

#[test]
fn line_navigation_reverse_anchor_crosses_back_without_reanchoring() {
    let value = fixture(&["ABCD", "EFGH", "IJKL"], [1., 0.]);
    let mut current = KeyboardSelection {
        anchor: 7,
        caret: 12,
        preferred: None,
    };
    current = step(&value, current, NavigationKey::ExtendPreviousLine);
    assert_eq!(
        (current.anchor, current.caret, current.range()),
        (7, 7, 7..7)
    );
    current = step(&value, current, NavigationKey::ExtendPreviousLine);
    assert_eq!(
        (current.anchor, current.caret, current.range()),
        (7, 2, 2..7)
    );
    assert_eq!(value.selected(current.range()).unwrap(), "CD\nEF");
}

#[test]
fn line_navigation_crlf_and_complex_graphemes_never_split_source_clusters() {
    let value = fixture(&["A👩‍🔬e\u{301}\r", "B🇨🇳中"], [1., 0.]);
    let before_crlf = "A👩‍🔬e\u{301}".len();
    let start = cursor(&value, "A👩‍🔬".len());
    let next = step(&value, start, NavigationKey::ExtendNextLine);
    assert_eq!(next.caret, before_crlf + 2 + "B🇨🇳".len());
    assert_eq!(value.selected(next.range()).unwrap(), "e\u{301}\r\nB🇨🇳");
    assert_eq!(
        step(&value, next, NavigationKey::ExtendPreviousLine).caret,
        start.caret
    );
}

#[test]
fn line_navigation_missing_blank_and_whitespace_rows_are_not_skipped() {
    let mut missing = fixture(&["abcd", "xx", "abcd"], [1., 0.]);
    missing.lines.remove(1);
    assert!(missing
        .navigate_keyboard(cursor(&missing, 2), NavigationKey::ExtendNextLine)
        .is_none());
    for middle in ["", "  ", "\r"] {
        let value = fixture(&["abcd", middle, "abcd"], [1., 0.]);
        assert!(value
            .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
            .is_none());
    }
}

#[test]
fn line_navigation_overlapping_rows_different_columns_and_opposed_axes_refuse() {
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    shift_row(&mut value, 1, 0., -60.);
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    shift_row(&mut value, 1, 1000., 0.);
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    let mut reverse = fixture(&["abcd", "efgh"], [-1., 0.]);
    std::mem::swap(&mut value.lines[1], &mut reverse.lines[1]);
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
}

#[test]
fn line_navigation_does_not_sort_rows_by_desktop_coordinates() {
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    shift_row(&mut value, 0, 0., 120.);
    let next = step(&value, cursor(&value, 2), NavigationKey::ExtendNextLine);
    assert_eq!(next.caret, 7);
    assert_eq!(value.selected(next.range()).unwrap(), "cd\nef");
}

#[test]
fn line_navigation_bad_projection_missing_character_and_non_monotone_stops_refuse() {
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    value.lines[1].cells[0]
        .projected
        .as_mut()
        .unwrap()
        .source
        .quad[0]
        .x = f32::NAN;
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    value.lines[1].cells.remove(1);
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    value.lines[1].cells.swap(0, 1);
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
}

#[test]
fn line_navigation_negative_origin_keeps_original_geometry_and_mark_positions() {
    let mut value = fixture(&["abcd", "efgh"], [0.8, 0.6]);
    value.translate(-900., -800.);
    let original_text = value.text.clone();
    let words = value.words.clone();
    let quads = value.marking_quads(0..value.text.len());
    let first = value.caret_segment(2).unwrap();
    assert!(first[0].x < 0. && first[0].y < 0.);
    let next = step(&value, cursor(&value, 2), NavigationKey::ExtendNextLine);
    assert_eq!(next.caret, 7);
    assert_eq!(value.text, original_text);
    assert_eq!(value.words, words);
    assert_eq!(value.marking_quads(0..value.text.len()), quads);
    assert_eq!(value.caret_segment(2).unwrap(), first);
}

#[test]
fn navigation_state_and_work_are_bounded_without_mutating_existing_ranges() {
    assert!(std::mem::size_of::<KeyboardSelection>() <= 48);
    let mut value = text("one");
    value.words = vec![0..3; MAX_WORDS + 1];
    assert!(value
        .navigate_keyboard(cursor(&value, 0), NavigationKey::NextWord)
        .is_none());
    let mut value = fixture(&["abcd", "efgh"], [1., 0.]);
    let extra = value.lines[1].cells[0].range.clone();
    value.lines[1].range = extra; // 同行范围不能声称覆盖未归属正文。
    assert!(value
        .navigate_keyboard(cursor(&value, 2), NavigationKey::ExtendNextLine)
        .is_none());
}

#[test]
fn word_navigation_projected_180_vertical_and_tilted_do_not_reverse_bytes() {
    for axis in [[1., 0.], [-1., 0.], [0., 1.], [0., -1.], [0.8, 0.6]] {
        let mut value = fixture(&["ONE TWO", "LAST"], axis);
        value.words = vec![0..3, 4..7, 8..12];
        let marks = value.marking_quads(0..12);
        let mut current = cursor(&value, 0);
        for end in [3, 7, 12] {
            current = step(&value, current, NavigationKey::ExtendNextWord);
            assert_eq!((current.anchor, current.caret), (0, end));
        }
        assert_eq!(value.selected(current.range()).unwrap(), "ONE TWO\nLAST");
        assert_eq!(
            step(&value, current, NavigationKey::ExtendPreviousWord).caret,
            8
        );
        assert_eq!(value.marking_quads(0..12), marks);
    }
}

#[test]
fn line_navigation_equal_distance_tie_uses_earlier_logical_stop_and_keeps_goal() {
    let mut value = fixture(&["ABCD", "EF", "GHIJ"], [1., 0.]);
    shift_row(&mut value, 1, 10., 0.);
    let next = step(&value, cursor(&value, 1), NavigationKey::ExtendNextLine);
    assert_eq!(next.caret, 5);
    assert_eq!(step(&value, next, NavigationKey::ExtendNextLine).caret, 9);
}

#[test]
fn line_navigation_duplicate_byte_rows_and_invalid_preferred_state_refuse() {
    let mut value = fixture(&["ABCD", "EFGH"], [1., 0.]);
    let original = cursor(&value, 2);
    value.lines[1].range = value.lines[0].range.clone();
    assert!(value
        .navigate_keyboard(original, NavigationKey::ExtendNextLine)
        .is_none());
    let value = fixture(&["ABCD", "EFGH"], [1., 0.]);
    let mut original = cursor(&value, 2);
    original.preferred = Some(PreferredColumn {
        axis: [f64::NAN.to_bits(), 0_f64.to_bits()],
        coordinate: 20_f64.to_bits(),
    });
    assert!(value
        .navigate_keyboard(original, NavigationKey::ExtendNextLine)
        .is_none());
}

#[test]
fn line_navigation_legacy_rectangle_stops_also_restore_a_short_row_column() {
    let contents = ["0123456789", "ab", "ABCDEFGHIJ"];
    let words = contents
        .iter()
        .enumerate()
        .map(|(line, content)| RecognizedWord {
            text: (*content).into(),
            rect: Rect {
                x: 400,
                y: 400 + line as u32 * 60,
                width: content.len() as u32 * 20,
                height: 20,
            },
            line,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry: None,
        })
        .collect();
    let value = SelectionText::from_recognized(&RecognizedText {
        text: contents.join("\n"),
        regions: Vec::new(),
        words,
        lines: Vec::new(),
    });
    assert!(!value.lines[0].projected && !value.lines[0].stops.is_empty());
    let next = step(&value, cursor(&value, 7), NavigationKey::ExtendNextLine);
    assert_eq!(next.caret, 13);
    assert_eq!(step(&value, next, NavigationKey::ExtendNextLine).caret, 21);
}
