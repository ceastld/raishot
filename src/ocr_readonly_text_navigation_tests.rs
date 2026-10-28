//! 调用真实生产只读方法；预期停点来自固定原文，不借导航 helper 计算 oracle。
use super::{copy_payload, CopyScope, ResultEditor, Selection};
use crate::text_input::TextInputState;
use std::sync::Arc;

const CLUSTERS: &str = "甲e\u{301}👨‍👩‍👧‍👦🇨🇳👍🏽\r\n乙";
const STOPS: [usize; 8] = [0, 3, 6, 31, 39, 47, 49, 52];

struct ContentStamp {
    bytes: Vec<u8>,
    allocation: usize,
    identity: Arc<()>,
    revision: u64,
    marked: Option<std::ops::Range<usize>>,
    undo: Vec<(String, TextInputState)>,
    redo: Vec<(String, TextInputState)>,
    composition: Option<(String, TextInputState)>,
}

impl ContentStamp {
    fn of(editor: &ResultEditor) -> Self {
        Self {
            bytes: editor.text.as_bytes().to_vec(),
            allocation: editor.text.as_ptr() as usize,
            identity: Arc::clone(&editor.body_identity),
            revision: editor.revision,
            marked: editor.input.marked.clone(),
            undo: editor
                .undo
                .iter()
                .map(|s| (s.text.clone(), s.input.clone()))
                .collect(),
            redo: editor
                .redo
                .iter()
                .map(|s| (s.text.clone(), s.input.clone()))
                .collect(),
            composition: editor
                .composition_before
                .as_ref()
                .map(|s| (s.text.clone(), s.input.clone())),
        }
    }

    fn assert_kept(&self, editor: &ResultEditor) {
        assert_eq!(editor.text.as_bytes(), self.bytes);
        assert_eq!(editor.text.as_ptr() as usize, self.allocation);
        assert!(Arc::ptr_eq(&self.identity, &editor.body_identity));
        assert_eq!(editor.revision, self.revision);
        assert_eq!(editor.input.marked, self.marked);
        assert_eq!(
            editor
                .undo
                .iter()
                .map(|s| (s.text.clone(), s.input.clone()))
                .collect::<Vec<_>>(),
            self.undo
        );
        assert_eq!(
            editor
                .redo
                .iter()
                .map(|s| (s.text.clone(), s.input.clone()))
                .collect::<Vec<_>>(),
            self.redo
        );
        assert_eq!(
            editor
                .composition_before
                .as_ref()
                .map(|s| (s.text.clone(), s.input.clone())),
            self.composition
        );
    }
}

fn point(editor: &mut ResultEditor, index: usize) {
    assert!(editor.readonly_begin_drag(index));
    assert!(editor.end_drag());
}

#[test]
fn readonly_arrows_step_fixed_complete_graphemes_both_directions() {
    let mut editor = ResultEditor::new(CLUSTERS.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    assert_eq!(CLUSTERS.len(), 52);
    for stop in STOPS.into_iter().skip(1) {
        assert!(editor.readonly_navigate("right", false, false));
        assert_eq!(editor.caret(), stop);
        assert_eq!(editor.input.selection, stop..stop);
        assert!(!editor.input.reversed);
        assert!(!editor.dragging());
        stamp.assert_kept(&editor);
    }
    assert!(editor.readonly_navigate("right", false, false));
    assert_eq!(editor.caret(), 52);
    for stop in STOPS.into_iter().rev().skip(1) {
        assert!(editor.readonly_navigate("left", false, false));
        assert_eq!(editor.caret(), stop);
        assert_eq!(editor.input.selection, stop..stop);
        stamp.assert_kept(&editor);
    }
    assert!(editor.readonly_navigate("left", false, false));
    assert_eq!(editor.caret(), 0);
}

#[test]
fn readonly_shift_selects_exact_cluster_bytes_and_copy_range() {
    let mut editor = ResultEditor::new(CLUSTERS.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for (start, end, cluster) in [
        (0, 3, "甲"),
        (3, 6, "e\u{301}"),
        (6, 31, "👨‍👩‍👧‍👦"),
        (31, 39, "🇨🇳"),
        (39, 47, "👍🏽"),
        (47, 49, "\r\n"),
        (49, 52, "乙"),
    ] {
        point(&mut editor, start);
        assert!(editor.readonly_navigate("right", true, false));
        assert_eq!(editor.input.selection, start..end);
        assert_eq!(editor.caret(), end);
        assert!(!editor.input.reversed);
        assert_eq!(
            editor.text.get(editor.input.selection.clone()),
            Some(cluster)
        );
        assert_eq!(
            copy_payload(
                &editor.text,
                editor.input.selection.clone(),
                CopyScope::Selection
            ),
            Some(cluster)
        );
        assert_eq!(
            copy_payload(
                &editor.text,
                editor.input.selection.clone(),
                CopyScope::Full
            ),
            Some(CLUSTERS)
        );
        point(&mut editor, end);
        assert!(editor.readonly_navigate("left", true, false));
        assert_eq!(editor.input.selection, start..end);
        assert_eq!(editor.caret(), start);
        assert!(editor.input.reversed);
        stamp.assert_kept(&editor);
    }
}

#[test]
fn readonly_editor_layout_byte_hits_floor_clusters_and_keep_reverse_drag() {
    let mut editor = ResultEditor::new(CLUSTERS.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for (hit, floor) in [
        (1, 0),
        (2, 0),
        (4, 3),
        (5, 3),
        (10, 6),
        (13, 6),
        (17, 6),
        (20, 6),
        (24, 6),
        (27, 6),
        (35, 31),
        (43, 39),
        (48, 47),
        (52, 52),
        (usize::MAX, 52),
    ] {
        assert!(editor.readonly_begin_drag(hit));
        assert_eq!(editor.caret(), floor);
        assert_eq!(editor.input.selection, floor..floor);
        assert_eq!(editor.input.drag_anchor, Some(floor));
        editor.end_drag();
        point(&mut editor, 52);
        assert!(editor.readonly_begin_drag(52));
        assert!(editor.readonly_extend_drag(hit));
        assert_eq!(editor.input.selection, floor..52);
        assert_eq!(editor.caret(), floor);
        assert_eq!(editor.input.reversed, floor < 52);
        assert_eq!(
            editor.text.get(editor.input.selection.clone()),
            Some(&CLUSTERS[floor..])
        );
        editor.end_drag();
        let previous = editor.input.clone();
        assert!(!editor.readonly_extend_drag(0));
        assert_eq!(editor.input, previous);
        stamp.assert_kept(&editor);
    }
}

#[test]
fn ordinary_readonly_result_selection_uses_same_layout_grapheme_boundaries() {
    let mut selection = Selection::default();
    for (hit, floor) in [
        (4, 3),
        (13, 6),
        (35, 31),
        (43, 39),
        (48, 47),
        (usize::MAX, 52),
    ] {
        selection.begin(CLUSTERS, 52);
        selection.extend(CLUSTERS, hit);
        assert_eq!(selection.range(), floor..52);
        assert_eq!(CLUSTERS.get(selection.range()), Some(&CLUSTERS[floor..]));
        selection.end_drag();
        selection.extend(CLUSTERS, 0);
        assert_eq!(selection.range(), floor..52);
        selection.begin(CLUSTERS, hit);
        selection.extend(CLUSTERS, 52);
        assert_eq!(selection.range(), floor..52);
    }
    selection.begin(CLUSTERS, 4);
    selection.extend(CLUSTERS, 5);
    assert_eq!(selection.range(), 3..3);
    assert_eq!(
        copy_payload(CLUSTERS, selection.range(), CopyScope::Selection),
        Some(CLUSTERS)
    );
    selection.select_all(CLUSTERS);
    assert_eq!(selection.range(), 0..52);
    assert_eq!(CLUSTERS.as_bytes(), "甲e\u{301}👨‍👩‍👧‍👦🇨🇳👍🏽\r\n乙".as_bytes());
}

#[test]
fn readonly_reverse_shift_keeps_anchor_through_collapse_and_direction_change() {
    let mut editor = ResultEditor::new(CLUSTERS.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    editor.readonly_begin_drag(31);
    editor.readonly_extend_drag(6);
    editor.end_drag();
    assert_eq!(editor.input.selection, 6..31);
    assert!(editor.input.reversed);
    assert!(editor.readonly_navigate("right", true, false));
    assert_eq!(editor.input.selection, 31..31);
    assert!(!editor.input.reversed);
    assert!(editor.readonly_navigate("right", true, false));
    assert_eq!(editor.input.selection, 31..39);
    assert!(!editor.input.reversed);
    assert!(editor.readonly_navigate("left", true, false));
    assert_eq!(editor.input.selection, 31..31);
    assert!(editor.readonly_navigate("left", true, false));
    assert_eq!(editor.input.selection, 6..31);
    assert!(editor.input.reversed);
    stamp.assert_kept(&editor);
}

#[test]
fn readonly_unshifted_arrows_with_or_without_control_only_collapse_selection() {
    let mut editor = ResultEditor::new(CLUSTERS.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for reverse in [false, true] {
        for control in [false, true] {
            for (key, expected) in [("left", 6), ("right", 39)] {
                editor.readonly_begin_drag(if reverse { 39 } else { 6 });
                editor.readonly_extend_drag(if reverse { 6 } else { 39 });
                editor.end_drag();
                assert_eq!(editor.input.reversed, reverse);
                assert!(editor.readonly_navigate(key, false, control));
                assert_eq!(editor.input.selection, expected..expected);
                assert_eq!(editor.caret(), expected);
                assert!(!editor.input.reversed);
                stamp.assert_kept(&editor);
            }
        }
    }
}

#[test]
fn readonly_control_words_keep_apostrophes_numbers_and_punctuation_stops() {
    let text = "can't 32.3, ok.";
    let mut editor = ResultEditor::new(text.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for stop in [5, 10, 11, 14, 15] {
        assert!(editor.readonly_navigate("right", false, true));
        assert_eq!(editor.caret(), stop);
    }
    for stop in [14, 12, 10, 6, 0] {
        assert!(editor.readonly_navigate("left", false, true));
        assert_eq!(editor.caret(), stop);
    }
    point(&mut editor, 2);
    editor.readonly_navigate("right", true, true);
    assert_eq!(editor.input.selection, 2..5);
    assert_eq!(editor.selected_or_full(), "n't");
    editor.readonly_navigate("left", true, true);
    assert_eq!(editor.input.selection, 0..2);
    assert!(editor.input.reversed);
    assert_eq!(editor.selected_or_full(), "ca");
    stamp.assert_kept(&editor);
}

#[test]
fn readonly_control_mixed_chinese_symbols_and_emoji_keeps_whole_source_segments() {
    let text = "中英文，word!? e\u{301}\r\n尾";
    let mut editor = ResultEditor::new(text.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for stop in [3, 6, 9, 12, 16, 17, 18, 22, 27] {
        editor.readonly_navigate("right", true, true);
        assert_eq!(editor.input.selection, 0..stop);
        assert_eq!(editor.caret(), stop);
        assert_eq!(
            editor.text.get(editor.input.selection.clone()),
            Some(&text[..stop])
        );
    }
    point(&mut editor, text.len());
    for stop in [24, 19, 17, 16, 12, 9, 6, 3, 0] {
        editor.readonly_navigate("left", true, true);
        assert_eq!(editor.input.selection, stop..27);
        assert_eq!(editor.caret(), stop);
        assert!(editor.input.reversed);
    }
    stamp.assert_kept(&editor);
    let mut emoji = ResultEditor::new(CLUSTERS.into(), 0..0);
    let stamp = ContentStamp::of(&emoji);
    for stop in [3, 6, 31, 39, 47, 52] {
        emoji.readonly_navigate("right", false, true);
        assert_eq!(emoji.caret(), stop);
    }
    for stop in [49, 39, 31, 6, 3, 0] {
        emoji.readonly_navigate("left", false, true);
        assert_eq!(emoji.caret(), stop);
    }
    stamp.assert_kept(&emoji);
}

#[test]
fn readonly_home_end_are_hard_line_and_control_document_boundaries() {
    let text = "甲e\u{301}\r\n\r\n乙\r尾\n";
    let mut editor = ResultEditor::new(text.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for (hit, home, end) in [
        (0, 0, 6),
        (3, 0, 6),
        (6, 0, 6),
        (7, 0, 6),
        (8, 8, 8),
        (9, 8, 8),
        (10, 10, 13),
        (13, 10, 13),
        (14, 14, 17),
        (17, 14, 17),
        (18, 18, 18),
    ] {
        point(&mut editor, hit);
        editor.readonly_navigate("home", false, false);
        assert_eq!(editor.caret(), home);
        point(&mut editor, hit);
        editor.readonly_navigate("end", false, false);
        assert_eq!(editor.caret(), end);
    }
    point(&mut editor, 10);
    editor.readonly_navigate("home", true, true);
    assert_eq!(editor.input.selection, 0..10);
    assert!(editor.input.reversed);
    editor.readonly_navigate("end", true, true);
    assert_eq!(editor.input.selection, 10..18);
    assert!(!editor.input.reversed);
    stamp.assert_kept(&editor);
}

#[test]
fn readonly_unknown_keys_empty_text_and_unicode_hard_separators_are_explicit() {
    let mut editor = ResultEditor::new("a\u{0085}b\u{2028}c\u{2029}d\re\n".into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    for (hit, home, end) in [
        (0, 0, 1),
        (3, 3, 4),
        (7, 7, 8),
        (11, 11, 12),
        (13, 13, 14),
        (15, 15, 15),
    ] {
        point(&mut editor, hit);
        editor.readonly_navigate("home", false, false);
        assert_eq!(editor.caret(), home);
        point(&mut editor, hit);
        editor.readonly_navigate("end", false, false);
        assert_eq!(editor.caret(), end);
    }
    editor.readonly_begin_drag(3);
    let input = editor.input.clone();
    for key in ["up", "down", "pageup", "pagedown", "tab", "delete", "x"] {
        assert!(!editor.readonly_navigate(key, true, true));
        assert_eq!(editor.input, input);
    }
    stamp.assert_kept(&editor);
    let mut empty = ResultEditor::new(String::new(), 0..0);
    for key in ["left", "right", "home", "end"] {
        for control in [false, true] {
            assert!(empty.readonly_navigate(key, true, control));
            assert_eq!(empty.input.selection, 0..0);
            assert_eq!(empty.caret(), 0);
        }
    }
    assert!(empty.readonly_begin_drag(usize::MAX));
    assert!(empty.readonly_extend_drag(usize::MAX));
    assert_eq!(empty.input.selection, 0..0);
}

#[test]
fn readonly_selection_preserves_populated_undo_redo_and_exact_body_identity() {
    let mut editor = ResultEditor::new("before".into(), 0..0);
    editor.select_all();
    editor.replace(None, CLUSTERS, false, None);
    editor.select_all();
    editor.replace(None, "temporary", false, None);
    editor.undo();
    assert_eq!(editor.text, CLUSTERS);
    assert_eq!((editor.undo.len(), editor.redo.len()), (1, 1));
    let stamp = ContentStamp::of(&editor);
    editor.readonly_begin_drag(43);
    editor.readonly_extend_drag(4);
    editor.end_drag();
    assert_eq!(editor.input.selection, 3..39);
    assert_eq!(editor.selected_or_full(), "e\u{301}👨‍👩‍👧‍👦🇨🇳");
    for (key, shift, control) in [
        ("left", true, true),
        ("end", true, true),
        ("home", false, false),
    ] {
        editor.readonly_navigate(key, shift, control);
        stamp.assert_kept(&editor);
    }
    editor.undo();
    assert_eq!(editor.text, "before");
    editor.redo();
    assert_eq!(editor.text, CLUSTERS);
    editor.redo();
    assert_eq!(editor.text, "temporary");
}

#[test]
fn readonly_methods_refuse_active_composition_without_cancelling_or_committing() {
    let mut editor = ResultEditor::new("甲old\r\n乙".into(), 3..6);
    editor.replace(None, "e\u{301}", true, Some(1..1));
    assert!(editor.composing());
    let stamp = ContentStamp::of(&editor);
    let input = editor.input.clone();
    for key in ["left", "right", "home", "end"] {
        assert!(!editor.readonly_navigate(key, true, true));
        assert_eq!(editor.input, input);
    }
    assert!(!editor.readonly_begin_drag(0));
    assert!(!editor.readonly_extend_drag(usize::MAX));
    assert_eq!(editor.input, input);
    stamp.assert_kept(&editor);
    assert!(editor.cancel_composition());
    assert_eq!(editor.text, "甲old\r\n乙");
    assert_eq!(editor.input.selection, 3..6);
}

#[test]
fn editable_navigation_keeps_graphemes_and_native_replacement_keeps_utf16_contract() {
    let mut editor = ResultEditor::new("e\u{301}!".into(), 0..0);
    editor.navigate("right", false, false);
    assert_eq!(editor.caret(), 3);
    editor.navigate("left", false, false);
    assert_eq!(editor.caret(), 0);
    editor.move_to(2, false);
    assert_eq!(editor.caret(), 0);
    // 原生 UTF-16 替换范围仍可独立替换组合音标。
    editor.replace(Some(1..2), "X", false, None);
    assert_eq!(editor.text, "eX!");
    editor.undo();
    assert_eq!(editor.text, "e\u{301}!");
    editor.move_to(0, false);
    editor.delete(false);
    assert_eq!(editor.text, "!");
    editor.undo();
    assert_eq!(editor.text, "e\u{301}!");
}
