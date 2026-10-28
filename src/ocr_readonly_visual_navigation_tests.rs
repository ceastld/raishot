//! 注入可信视觉停点，直接调用生产移动/cache/setter；不冒称真实字体或 GPUI 验证。
use super::readonly_visual_navigation::{
    move_vertical, VerticalIntent, VerticalState, VisualCache, VisualLayout, VisualLine, VisualStop,
};
use super::ResultEditor;
use crate::text_input::TextInputState;
use std::sync::Arc;

struct ContentStamp {
    bytes: Vec<u8>,
    allocation: usize,
    identity: Arc<()>,
    revision: u64,
    marked: Option<std::ops::Range<usize>>,
    history: [Vec<(String, TextInputState)>; 2],
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
            history: [&editor.undo, &editor.redo].map(|history| {
                history
                    .iter()
                    .map(|s| (s.text.clone(), s.input.clone()))
                    .collect()
            }),
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
            [&editor.undo, &editor.redo].map(|history| {
                history
                    .iter()
                    .map(|s| (s.text.clone(), s.input.clone()))
                    .collect::<Vec<_>>()
            }),
            self.history
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

fn layout(text: &str, rows: &[&[(usize, f32)]]) -> VisualLayout {
    VisualLayout::new(
        text,
        rows.iter()
            .map(|row| VisualLine {
                stops: row
                    .iter()
                    .map(|(byte, x)| VisualStop { byte: *byte, x: *x })
                    .collect(),
            })
            .collect(),
    )
    .expect("可信原文视觉行停点")
}

fn step(
    editor: &mut ResultEditor,
    rows: &VisualLayout,
    state: &mut VerticalState,
    down: bool,
    shift: bool,
) {
    let (anchor, caret) = move_vertical(
        &editor.text,
        Some(rows),
        true,
        VerticalIntent {
            selection: editor.input.selection.clone(),
            reversed: editor.input.reversed,
            down,
            shift,
        },
        state,
    )
    .expect("当前可信视觉布局可导航");
    assert!(editor.readonly_set_selection(anchor, caret));
}

const COLUMNS: &str = "abcd\ne\nwxyz";
fn columns() -> VisualLayout {
    layout(
        COLUMNS,
        &[
            &[(0, 0.), (1, 10.), (2, 20.), (3, 30.), (4, 40.)],
            &[(5, 0.), (6, 10.)],
            &[(7, 0.), (8, 10.), (9, 20.), (10, 30.), (11, 40.)],
        ],
    )
}

#[test]
fn visual_soft_wrap_chinese_english_uses_rendered_rows_without_hard_newline() {
    let text = "中文abcd尾";
    assert!(!text.contains('\n'));
    let rows = layout(
        text,
        &[
            &[(0, 0.), (3, 12.), (6, 24.), (7, 32.)],
            &[(7, 0.), (8, 8.), (9, 16.), (10, 24.), (13, 36.)],
        ],
    );
    let mut editor = ResultEditor::new(text.into(), 3..3);
    let stamp = ContentStamp::of(&editor);
    let mut state = VerticalState::default();
    step(&mut editor, &rows, &mut state, true, false);
    assert_eq!(editor.input.selection, 8..8); // x=12 平局，较小 byte。
    step(&mut editor, &rows, &mut state, false, false);
    assert_eq!(editor.input.selection, 3..3);
    stamp.assert_kept(&editor);
}

#[test]
fn visual_preferred_x_survives_short_rows_and_equal_repaint_geometry() {
    let mut editor = ResultEditor::new(COLUMNS.into(), 3..3);
    let stamp = ContentStamp::of(&editor);
    let mut state = VerticalState::default();
    step(&mut editor, &columns(), &mut state, true, false);
    assert_eq!(editor.caret(), 6);
    step(&mut editor, &columns(), &mut state, true, false);
    assert_eq!(editor.caret(), 10); // 首选 x=30 没有被短行的 x=10 覆盖。
    step(&mut editor, &columns(), &mut state, false, false);
    assert_eq!(editor.caret(), 6);
    step(&mut editor, &columns(), &mut state, false, false);
    assert_eq!(editor.caret(), 3);
    stamp.assert_kept(&editor);
}

#[test]
fn visual_soft_shared_byte_keeps_destination_row_affinity() {
    let text = "abcdef";
    let rows = layout(
        text,
        &[
            &[(0, 0.), (1, 10.), (2, 20.), (3, 30.)],
            &[(3, 0.), (4, 10.), (5, 20.)],
            &[(5, 0.), (6, 10.)],
        ],
    );
    let mut editor = ResultEditor::new(text.into(), 2..2);
    let mut state = VerticalState::default();
    for expected in [5, 6] {
        step(&mut editor, &rows, &mut state, true, false);
        assert_eq!(editor.caret(), expected);
    }
    for expected in [5, 2] {
        step(&mut editor, &rows, &mut state, false, false);
        assert_eq!(editor.caret(), expected);
    }
    state.reset();
    editor.readonly_set_selection(3, 3);
    step(&mut editor, &rows, &mut state, false, false);
    assert_eq!(editor.caret(), 0); // 新共享 byte 从后行首开始，确定且无循环。
}

#[test]
fn visual_reverse_shift_crosses_anchor_without_splitting_clusters_or_crlf() {
    let text = "甲e\u{301}👩‍💻🇨🇳👍🏽\r\n乙";
    assert_eq!(text.len(), 38);
    let rows = layout(
        text,
        &[
            &[(0, 0.), (3, 10.), (6, 20.)],
            &[(6, 0.), (17, 20.), (25, 40.), (33, 60.)],
            &[(35, 0.), (38, 10.)],
        ],
    );
    let mut editor = ResultEditor::new(text.into(), 0..0);
    editor.readonly_set_selection(33, 6);
    let stamp = ContentStamp::of(&editor);
    let mut state = VerticalState::default();
    step(&mut editor, &rows, &mut state, false, true);
    assert_eq!(editor.input.selection, 0..33);
    assert!(editor.input.reversed);
    step(&mut editor, &rows, &mut state, true, true);
    assert_eq!(editor.input.selection, 6..33);
    assert_eq!(
        editor.text.get(editor.input.selection.clone()),
        Some("👩‍💻🇨🇳👍🏽")
    );
    step(&mut editor, &rows, &mut state, true, true);
    assert_eq!(editor.input.selection, 33..35);
    assert!(!editor.input.reversed);
    assert_eq!(
        editor.text.get(editor.input.selection.clone()),
        Some("\r\n")
    );
    stamp.assert_kept(&editor);
}

#[test]
fn visual_empty_rows_trailing_newline_and_top_bottom_are_stable() {
    let text = "a\n\nb\n";
    let rows = layout(
        text,
        &[
            &[(0, 0.), (1, 10.)],
            &[(2, 0.)],
            &[(3, 0.), (4, 10.)],
            &[(5, 0.)],
        ],
    );
    let mut editor = ResultEditor::new(text.into(), 1..1);
    let mut state = VerticalState::default();
    for expected in [2, 4, 5, 5] {
        step(&mut editor, &rows, &mut state, true, false);
        assert_eq!(editor.caret(), expected);
    }
    for expected in [4, 2, 1, 1] {
        step(&mut editor, &rows, &mut state, false, false);
        assert_eq!(editor.caret(), expected);
    }
    let empty = layout("", &[&[(0, 0.)]]);
    let mut editor = ResultEditor::new(String::new(), 0..0);
    for (down, shift) in [(true, false), (false, true), (true, true), (false, false)] {
        step(&mut editor, &empty, &mut state, down, shift);
        assert_eq!(editor.input.selection, 0..0);
    }
}

#[test]
fn visual_no_shift_moves_original_caret_then_collapses_reverse_selection() {
    let mut editor = ResultEditor::new(COLUMNS.into(), 0..0);
    editor.readonly_set_selection(10, 3);
    let mut state = VerticalState::default();
    step(&mut editor, &columns(), &mut state, true, false);
    assert_eq!(editor.input.selection, 6..6);
    assert!(!editor.input.reversed);
    // Shift 的 anchor 即使经过折叠、方向变化仍使用真实 TextInputState。
    step(&mut editor, &columns(), &mut state, true, true);
    assert_eq!(editor.input.selection, 6..10);
    step(&mut editor, &columns(), &mut state, false, true);
    assert_eq!(editor.input.selection, 6..6);
    step(&mut editor, &columns(), &mut state, false, true);
    assert_eq!(editor.input.selection, 3..6);
    assert!(editor.input.reversed);
}

#[test]
fn visual_reflow_and_normal_horizontal_reset_choose_current_x() {
    let mut editor = ResultEditor::new(COLUMNS.into(), 3..3);
    let mut state = VerticalState::default();
    step(&mut editor, &columns(), &mut state, true, false);
    let reflow = layout(
        COLUMNS,
        &[
            &[(0, 0.), (1, 10.), (2, 20.), (3, 30.), (4, 40.)],
            &[(5, 0.), (6, 15.)],
            &[(7, 0.), (8, 10.), (9, 20.), (10, 30.), (11, 40.)],
        ],
    );
    step(&mut editor, &reflow, &mut state, true, false);
    assert_eq!(editor.caret(), 8); // 新布局从当前 x=15 取最近停点。
    state.reset();
    editor.readonly_begin_drag(3);
    editor.end_drag();
    step(&mut editor, &columns(), &mut state, true, false);
    state.reset(); // 宿主正常 mouse/左右/Ctrl词/Home/End/Ctrl+A 的生产 reset。
    editor.readonly_navigate("left", false, false);
    assert_eq!(editor.caret(), 5);
    step(&mut editor, &columns(), &mut state, true, false);
    assert_eq!(editor.caret(), 7);
}

#[test]
fn visual_missing_stale_or_foreign_text_layout_keeps_editor_input_and_content() {
    let editor = ResultEditor::new(COLUMNS.into(), 3..3);
    let input = editor.input.clone();
    let stamp = ContentStamp::of(&editor);
    let mut state = VerticalState::default();
    let rows = columns();
    assert!(move_vertical(
        &editor.text,
        None,
        true,
        VerticalIntent {
            selection: input.selection.clone(),
            reversed: false,
            down: true,
            shift: true,
        },
        &mut state
    )
    .is_none());
    assert!(move_vertical(
        &editor.text,
        Some(&rows),
        false,
        VerticalIntent {
            selection: input.selection.clone(),
            reversed: false,
            down: true,
            shift: true,
        },
        &mut state
    )
    .is_none());
    let foreign = layout(
        "WXYZ\nE\nABCD",
        &[
            &[(0, 0.), (1, 10.), (2, 20.), (3, 30.), (4, 40.)],
            &[(5, 0.), (6, 10.)],
            &[(7, 0.), (8, 10.), (9, 20.), (10, 30.), (11, 40.)],
        ],
    );
    assert!(move_vertical(
        &editor.text,
        Some(&foreign),
        true,
        VerticalIntent {
            selection: input.selection.clone(),
            reversed: false,
            down: true,
            shift: true,
        },
        &mut state
    )
    .is_none());
    assert_eq!(editor.input, input);
    stamp.assert_kept(&editor);
}

#[test]
fn visual_cache_old_render_cannot_publish_over_new_frame_or_revive_invalidated_frame() {
    let mut cache = VisualCache::default();
    let identity = Arc::new(());
    let viewport = [360., 120.];
    let first = cache.begin_render();
    assert!(cache.publish(&first, &identity, 0, viewport, 1., columns()));
    assert!(cache.current(&identity, 0, viewport, 1.).is_some());
    let second = cache.begin_render();
    assert!(!cache.epoch_is_current(&first));
    assert!(cache.current(&identity, 0, viewport, 1.).is_none());
    assert!(!cache.publish(&first, &identity, 0, viewport, 1., columns()));
    assert!(cache.publish(&second, &identity, 0, viewport, 1., columns()));
    assert!(!cache.publish(&first, &identity, 9, [1., 1.], 2., columns()));
    assert!(cache.current(&identity, 0, viewport, 1.).is_some());
    cache.invalidate();
    assert!(!cache.epoch_is_current(&second));
    assert!(!cache.publish(&second, &identity, 0, viewport, 1., columns()));
    assert!(cache.current(&identity, 0, viewport, 1.).is_none());
}

#[test]
fn visual_cache_same_bytes_new_identity_revision_resize_and_scale_are_stale() {
    let mut cache = VisualCache::default();
    let identity = Arc::new(());
    let other_identity = Arc::new(());
    let viewport = [360., 120.];
    let epoch = cache.begin_render();
    assert!(cache.publish(&epoch, &identity, 7, viewport, 1.25, columns()));
    assert!(cache.current(&other_identity, 7, viewport, 1.25).is_none());
    assert!(cache.current(&identity, 8, viewport, 1.25).is_none());
    assert!(cache.current(&identity, 7, [359., 120.], 1.25).is_none());
    assert!(cache.current(&identity, 7, viewport, 1.).is_none());
    assert!(cache.current(&identity, 7, viewport, 1.25).is_some());
    assert!(!cache.publish(&epoch, &identity, 7, [f32::NAN, 120.], 1.25, columns()));
    assert!(cache.current(&identity, 7, viewport, 1.25).is_none());
}

#[test]
fn visual_painted_frame_without_usable_rows_keeps_mouse_receipt_and_consumes_vertical_only() {
    let mut cache = VisualCache::default();
    let mut editor = ResultEditor::new(COLUMNS.into(), 3..3);
    let identity = Arc::clone(&editor.body_identity);
    let viewport = [360., 120.];
    let first = cache.begin_render();
    assert!(!cache.current_frame(&identity, 0, viewport, 1.));
    assert!(cache.publish_without_rows(&first, &identity, 0, viewport, 1.));
    assert!(cache.epoch_is_current(&first));
    assert!(cache.current_frame(&identity, 0, viewport, 1.));
    assert!(cache.current(&identity, 0, viewport, 1.).is_none());
    let input = editor.input.clone();
    assert!(move_vertical(
        &editor.text,
        cache.current(&identity, 0, viewport, 1.),
        true,
        VerticalIntent {
            selection: input.selection.clone(),
            reversed: false,
            down: true,
            shift: true,
        },
        &mut VerticalState::default()
    )
    .is_none());
    assert_eq!(editor.input, input);
    // 真实paint已证API准备，mouse仍走既有字节命中floor路径，不依赖视觉rows。
    assert!(editor.readonly_begin_drag(5));
    assert!(editor.end_drag());
    assert_eq!(editor.caret(), 5);
    let second = cache.begin_render();
    assert!(!cache.current_frame(&identity, 0, viewport, 1.));
    assert!(!cache.publish_without_rows(&first, &identity, 0, viewport, 1.));
    assert!(cache.publish(&second, &identity, 0, viewport, 1., columns()));
    assert!(!cache.publish_without_rows(&first, &identity, 0, viewport, 1.));
    assert!(cache.current(&identity, 0, viewport, 1.).is_some());
}

#[test]
fn visual_layout_rejects_incomplete_clusters_forged_empty_rows_and_nonfinite_positions() {
    for rows in [
        vec![],
        vec![VisualLine { stops: vec![] }],
        vec![VisualLine {
            stops: vec![
                VisualStop { byte: 0, x: 0. },
                VisualStop { byte: 1, x: 10. },
                VisualStop { byte: 3, x: 20. },
            ],
        }],
        vec![VisualLine {
            stops: vec![
                VisualStop {
                    byte: 0,
                    x: f32::NAN,
                },
                VisualStop { byte: 3, x: 20. },
            ],
        }],
        vec![VisualLine {
            stops: vec![VisualStop { byte: 0, x: 0. }],
        }],
    ] {
        assert!(VisualLayout::new("e\u{301}", rows).is_none());
    }
    assert!(VisualLayout::new(
        "abc",
        vec![
            VisualLine {
                stops: vec![
                    VisualStop { byte: 0, x: 0. },
                    VisualStop { byte: 1, x: 10. }
                ]
            },
            VisualLine {
                stops: vec![VisualStop { byte: 3, x: 0. }]
            },
        ]
    )
    .is_none()); // 不允许把普通正文缺口伪造成空白视觉行。
    let bidi = layout("ab", &[&[(0, 20.), (1, 10.), (2, 0.)]]);
    let mut editor = ResultEditor::new("ab".into(), 1..1);
    step(
        &mut editor,
        &bidi,
        &mut VerticalState::default(),
        true,
        false,
    );
    assert_eq!(editor.caret(), 1);
}

#[test]
fn visual_setter_and_moves_keep_populated_undo_redo_original_bytes_and_body_identity() {
    let mut editor = ResultEditor::new("before".into(), 0..0);
    editor.select_all();
    assert!(editor.replace(None, COLUMNS, false, None));
    editor.select_all();
    assert!(editor.replace(None, "temporary", false, None));
    editor.undo();
    assert!(!editor.undo.is_empty() && !editor.redo.is_empty());
    let stamp = ContentStamp::of(&editor);
    editor.readonly_set_selection(3, 3);
    let mut state = VerticalState::default();
    step(&mut editor, &columns(), &mut state, true, true);
    step(&mut editor, &columns(), &mut state, true, true);
    assert_eq!(
        editor.text.get(editor.input.selection.clone()),
        Some("d\ne\nwxy")
    );
    stamp.assert_kept(&editor);
    editor.undo();
    assert_eq!(editor.text, "before");
    editor.redo();
    assert_eq!(editor.text, COLUMNS);
    editor.redo();
    assert_eq!(editor.text, "temporary");
}

#[test]
fn visual_explicit_setter_clamps_full_clusters_and_refuses_ime_without_history_mutation() {
    let text = "甲e\u{301}👩‍💻🇨🇳👍🏽\r\n乙";
    let mut editor = ResultEditor::new(text.into(), 0..0);
    let stamp = ContentStamp::of(&editor);
    assert!(editor.readonly_set_selection(34, 4));
    assert_eq!(editor.input.selection, 3..33);
    assert!(editor.input.reversed);
    assert_eq!(
        editor.text.get(editor.input.selection.clone()),
        Some("e\u{301}👩‍💻🇨🇳👍🏽")
    );
    assert!(editor.readonly_set_selection(usize::MAX, usize::MAX));
    assert_eq!(editor.caret(), 38);
    stamp.assert_kept(&editor);
    editor.readonly_set_selection(3, 6);
    assert!(editor.replace(None, "预", true, Some(0..1)));
    let input = editor.input.clone();
    let composed = ContentStamp::of(&editor);
    assert!(!editor.readonly_set_selection(0, usize::MAX));
    assert_eq!(editor.input, input);
    composed.assert_kept(&editor);
    assert!(editor.cancel_composition());
    assert_eq!(editor.text.as_bytes(), text.as_bytes());
}
