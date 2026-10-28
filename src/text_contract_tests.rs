//! 文字终端决策与真实文档事务；不把模型 marked 检查冒充原生 IME 验证。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
    text_contract::{decide, terminal_held, TextKeyAction},
};
use gpui::Modifiers;

fn editor() -> Editor {
    let canvas = Rect::full(640, 480);
    let mut editor = Editor::new(canvas, Some(canvas));
    editor.select_tool(Tool::Text);
    editor
}
fn text_annotation(text: &str) -> Annotation {
    let mut annotation = Annotation::new(Tool::Text, Point { x: 80., y: 80. }, Style::default());
    annotation.update(Point { x: 380., y: 170. });
    annotation.text = text.into();
    annotation.rotation = 30.;
    annotation
}
fn finish_by_key(editor: &mut Editor, key: &str) {
    let empty = editor
        .text_draft
        .as_ref()
        .unwrap()
        .annotation
        .text
        .is_empty();
    assert!(matches!(
        decide(key, Modifiers::default(), false, false, empty),
        TextKeyAction::Finish | TextKeyAction::DeleteEmpty
    ));
    editor.finish_text();
}

#[test]
fn text_contract_enter_shift_control_and_escape_finish_matrix() {
    for control in [false, true] {
        for shift in [false, true] {
            let modifiers = Modifiers {
                control,
                shift,
                ..Default::default()
            };
            assert_eq!(
                decide("enter", modifiers, false, false, false),
                if shift {
                    TextKeyAction::LineBreak
                } else {
                    TextKeyAction::Finish
                }
            );
            assert_eq!(
                decide("escape", modifiers, false, false, false),
                TextKeyAction::Finish
            );
        }
    }
}

#[test]
fn text_contract_composition_precedes_terminal_repeat_and_empty_delete() {
    for key in ["enter", "escape", "backspace", "delete", "a"] {
        for held in [false, true] {
            for shift in [false, true] {
                let modifiers = Modifiers {
                    control: true,
                    shift,
                    ..Default::default()
                };
                assert_eq!(decide(key, modifiers, held, true, true), TextKeyAction::Ime);
            }
        }
    }
}

#[test]
fn text_contract_repeat_never_cascades_terminal_but_nonempty_delete_can_repeat() {
    for key in ["enter", "escape"] {
        assert!(terminal_held(key, true));
        assert!(!terminal_held(key, false));
        for shift in [false, true] {
            assert_eq!(
                decide(
                    key,
                    Modifiers {
                        shift,
                        ..Default::default()
                    },
                    true,
                    false,
                    false
                ),
                TextKeyAction::Consume
            );
        }
    }
    for key in ["backspace", "delete"] {
        assert!(!terminal_held(key, true));
        assert_eq!(
            decide(key, Modifiers::default(), false, false, true),
            TextKeyAction::DeleteEmpty
        );
        assert_eq!(
            decide(key, Modifiers::default(), true, false, true),
            TextKeyAction::Consume
        );
        assert_eq!(
            decide(key, Modifiers::default(), true, false, false),
            TextKeyAction::Pass
        );
    }
}

#[test]
fn text_contract_system_modifiers_pass_without_consuming_application_shortcuts() {
    for (alt, platform) in [(true, false), (false, true), (true, true)] {
        let modifiers = Modifiers {
            alt,
            platform,
            control: true,
            shift: true,
            ..Default::default()
        };
        for key in ["enter", "escape", "backspace", "delete", "f4"] {
            for marked in [false, true] {
                assert_eq!(
                    decide(key, modifiers, true, marked, true),
                    TextKeyAction::Pass
                );
            }
        }
    }
    assert!(!terminal_held("f4", true));
}

#[test]
fn text_contract_new_enter_or_escape_commits_once_and_undo_redo_restores_text() {
    for key in ["enter", "escape"] {
        let mut editor = editor();
        editor.pointer_down((80, 80), 2.);
        editor.set_text("中文🙂\n第二行".into());
        assert!(editor.document.items().is_empty());
        finish_by_key(&mut editor, key);
        let object = editor.document.items()[0].clone();
        assert!(editor.text_draft.is_none());
        assert_eq!(object.annotation.text, "中文🙂\n第二行");
        assert_eq!(editor.document.edit_revision(), 1);
        editor.finish_text();
        assert_eq!(editor.document.edit_revision(), 1);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        editor.redo();
        assert_eq!(editor.document.items(), [object]);
    }
}

#[test]
fn text_contract_existing_escape_commits_once_noop_and_explicit_cancel_stays_rollback() {
    let mut editor = editor();
    let original = text_annotation("原文🙂");
    let id = editor.document.insert(original.clone());
    editor.begin_text(Some(id), original.points[0]);
    editor.set_text("显式取消".into());
    assert!(editor.cancel_text());
    assert_eq!(editor.document.get(id), Some(&original));
    let before = editor.document.edit_revision();
    editor.begin_text(Some(id), original.points[0]);
    editor.set_text("修改中文🙂\n第二行".into());
    let expected = editor.text_draft.as_ref().unwrap().annotation.clone();
    finish_by_key(&mut editor, "escape");
    assert_eq!(editor.document.get(id), Some(&expected));
    assert_eq!(editor.document.edit_revision(), before + 1);
    assert_eq!(editor.selected, Some(id));
    assert_eq!(expected.rotation, original.rotation);
    editor.begin_text(Some(id), expected.points[0]);
    finish_by_key(&mut editor, "enter");
    assert_eq!(
        editor.document.edit_revision(),
        before + 1,
        "无变化完成不增加历史"
    );
    editor.undo();
    assert_eq!(editor.document.get(id), Some(&original));
    editor.redo();
    assert_eq!(editor.document.get(id), Some(&expected));
}

#[test]
fn text_contract_blank_existing_finish_removes_once_clears_selection_and_is_reversible() {
    for key in ["enter", "escape", "backspace", "delete"] {
        let mut editor = editor();
        let original = text_annotation("待删除中文🙂");
        let id = editor.document.insert(original.clone());
        editor.selected = Some(id);
        editor.selection = vec![id];
        let before = editor.document.edit_revision();
        editor.begin_text(Some(id), original.points[0]);
        editor.set_text(
            if matches!(key, "backspace" | "delete") {
                ""
            } else {
                " \n\t"
            }
            .into(),
        );
        finish_by_key(&mut editor, key);
        assert!(editor.text_draft.is_none());
        assert!(editor.document.get(id).is_none());
        assert!(editor.selected_ids().is_empty());
        assert_eq!(editor.selected, None);
        assert_eq!(editor.document.edit_revision(), before + 1);
        editor.undo();
        assert_eq!(editor.document.get(id), Some(&original));
        editor.redo();
        assert!(editor.document.get(id).is_none());
    }
}

#[test]
fn text_contract_new_blank_finish_does_not_create_history_or_consume_existing_redo() {
    for key in ["enter", "escape", "backspace", "delete"] {
        let mut editor = editor();
        let original = text_annotation("应保留的 redo");
        let id = editor.document.insert(original.clone());
        editor.undo();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        editor.pointer_down((80, 80), 2.);
        editor.set_text(
            if matches!(key, "backspace" | "delete") {
                ""
            } else {
                " \n\t"
            }
            .into(),
        );
        finish_by_key(&mut editor, key);
        assert!(editor.text_draft.is_none());
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        assert!(editor.document.can_redo());
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.redo();
        assert_eq!(editor.document.get(id), Some(&original));
    }
}

#[test]
fn text_contract_watermark_blank_finish_preserves_existing_object_history_and_redo() {
    for blank in ["", " \n\t"] {
        let mut editor = editor();
        editor.select_tool(Tool::Watermark);
        let mut original = text_annotation("水印原文🙂");
        original.tool = Tool::Watermark;
        let id = editor.document.insert(original.clone());
        let pending_redo = text_annotation("保留的下一项");
        let redo_id = editor.document.insert(pending_redo.clone());
        editor.undo();
        editor.selected = Some(id);
        editor.selection = vec![id];
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();

        editor.begin_text(Some(id), original.points[0]);
        editor.set_text(blank.into());
        editor.finish_text();

        assert!(editor.text_draft.is_none());
        assert_eq!(editor.document.get(id), Some(&original));
        assert_eq!(editor.selected, Some(id));
        assert_eq!(editor.selected_ids(), vec![id]);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        assert!(editor.document.can_redo());
        editor.redo();
        assert_eq!(editor.document.get(id), Some(&original));
        assert_eq!(editor.document.get(redo_id), Some(&pending_redo));
    }
}
