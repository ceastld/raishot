//! 识别结果输出键的纯模型定义；不触碰系统剪贴板或实际窗口。
use super::*;

fn ready_gate() -> OutputGate {
    OutputGate {
        owns_result: true,
        editing: false,
        input_ready: true,
        frame_current: true,
        active: true,
        focused: true,
    }
}

fn modifiers(mask: u8) -> gpui::Modifiers {
    gpui::Modifiers {
        control: mask & 1 != 0,
        shift: mask & 2 != 0,
        alt: mask & 4 != 0,
        platform: mask & 8 != 0,
        function: mask & 16 != 0,
    }
}

#[test]
fn bare_enter_and_control_c_share_utf8_selection_or_full_payload() {
    let text = "甲🙂\n乙";
    let mut selection = Selection::default();
    // GPUI 字节位置落在字符内部时，实际 Selection 向前收敛至 UTF-8 边界。
    selection.begin(text, 6);
    selection.extend(text, 1);
    selection.end_drag();
    assert_eq!(selection.range(), 0..3);
    for (key, mask) in [("enter", 0), ("c", 1)] {
        assert_eq!(
            output_key(key, modifiers(mask), false, ready_gate()),
            OutputKey::CopySelection
        );
        assert_eq!(
            copy_payload(text, selection.range(), CopyScope::Selection),
            Some(selection.selected_or_full(text))
        );
    }
    selection.begin(text, 3);
    selection.extend(text, 7);
    selection.end_drag();
    assert_eq!(
        copy_payload(text, selection.range(), CopyScope::Selection),
        Some("🙂")
    );
    selection.reset();
    assert_eq!(
        copy_payload(text, selection.range(), CopyScope::Selection),
        Some(text)
    );
}

#[test]
fn output_keys_match_all_five_modifiers_exactly() {
    for mask in 0..32 {
        let mods = modifiers(mask);
        assert_eq!(
            output_key("enter", mods, false, ready_gate()),
            if mask == 0 {
                OutputKey::CopySelection
            } else {
                OutputKey::Consume
            },
            "Enter 修饰键组合 {mask}"
        );
        for (key, action) in [("c", OutputKey::CopySelection), ("a", OutputKey::SelectAll)] {
            assert_eq!(
                output_key(key, mods, false, ready_gate()),
                if mask == 1 { action } else { OutputKey::Pass },
                "{key} 修饰键组合 {mask}"
            );
        }
    }
}

#[test]
fn held_output_keys_are_consumed_without_publication_or_selection_change() {
    let text = "重复键不能复制🙂";
    let mut selection = Selection::default();
    selection.begin(text, 0);
    selection.extend(text, 3);
    selection.end_drag();
    let before = Selection {
        range: selection.range(),
        drag_anchor: selection.drag_anchor,
    };
    let mut writes = 0;
    for (key, mask) in [("enter", 0), ("c", 1), ("a", 1)] {
        let action = output_key(key, modifiers(mask), true, ready_gate());
        assert_eq!(action, OutputKey::Consume);
        if action == OutputKey::CopySelection {
            publish_text(text, selection.range(), CopyScope::Selection, |_| {
                writes += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        if action == OutputKey::SelectAll {
            selection.select_all(text);
        }
    }
    assert_eq!(writes, 0);
    assert_eq!(selection, before);
}

#[test]
fn busy_unready_unpainted_or_unfocused_result_consumes_output_keys() {
    let ready = ready_gate();
    for gate in [
        // Host 将 processing、输入所有权和 IME 门禁折叠到 input_ready。
        OutputGate {
            input_ready: false,
            ..ready
        },
        OutputGate {
            frame_current: false,
            ..ready
        },
        OutputGate {
            active: false,
            ..ready
        },
        OutputGate {
            focused: false,
            ..ready
        },
    ] {
        assert!(!gate.ready());
        for (key, mask) in [("enter", 0), ("c", 1), ("a", 1)] {
            assert_eq!(
                output_key(key, modifiers(mask), false, gate),
                OutputKey::Consume
            );
        }
        assert_eq!(output_key("x", modifiers(0), false, gate), OutputKey::Pass);
    }
}

#[test]
fn missing_result_owner_passes_keys_without_authorizing_output() {
    let gate = OutputGate {
        owns_result: false,
        ..ready_gate()
    };
    assert!(!gate.ready());
    for (key, mask) in [("enter", 0), ("enter", 2), ("c", 1), ("a", 1)] {
        assert_eq!(
            output_key(key, modifiers(mask), false, gate),
            OutputKey::Pass
        );
    }
}

#[test]
fn editor_output_fallback_preserves_enter_and_shift_enter_draft() {
    // 全文输入器已先处理自己的 Enter；此模型只核其后的结果输出 fallback。
    let mut editor = ResultEditor::new("编辑草稿\n第二行🙂".into(), 0..0);
    editor.move_to(editor.text.len(), false);
    let before = format!("{editor:?}");
    let gate = OutputGate {
        editing: true,
        ..ready_gate()
    };
    for (key, mask) in [("enter", 0), ("enter", 2), ("enter", 1), ("c", 1), ("a", 1)] {
        assert_eq!(
            output_key(key, modifiers(mask), false, gate),
            OutputKey::Consume
        );
    }
    assert_eq!(format!("{editor:?}"), before);
}

#[test]
fn result_frame_requires_paint_request_and_current_generation_revision() {
    let current = ResultFrame {
        generation: 7,
        revision: Some(11),
    };
    assert!(result_frame_current(Some(current), current, current));
    assert!(!result_frame_current(None, current, current));
    for stale in [
        ResultFrame {
            generation: 6,
            ..current
        },
        ResultFrame {
            revision: Some(10),
            ..current
        },
        ResultFrame {
            revision: None,
            ..current
        },
    ] {
        assert!(!result_frame_current(Some(stale), current, current));
        assert!(!result_frame_current(Some(stale), stale, current));
        assert!(!result_frame_current(Some(current), stale, current));
    }
    let readonly = ResultFrame {
        revision: None,
        ..current
    };
    assert!(result_frame_current(Some(readonly), readonly, readonly));
    assert!(!result_frame_current(Some(readonly), readonly, current));
}

#[test]
fn actual_recognized_placeholder_phrases_remain_copyable_content() {
    for text in ["未识别到文字", "No text recognized"] {
        assert_eq!(
            route(Ok(text.into()), false, true),
            Completion::ShowResult(text.into())
        );
        let mut published = None;
        assert_eq!(
            publish_text(text, 0..0, CopyScope::Selection, |value| {
                published = Some(value.to_owned());
                Ok::<_, ()>(())
            }),
            Ok(true)
        );
        assert_eq!(published.as_deref(), Some(text));
    }
}

#[test]
fn empty_or_unicode_whitespace_never_publishes_status_placeholder() {
    let mut writes = 0;
    for text in ["", " \t\r\n", "\u{a0}\u{2003}\u{3000}"] {
        for copy_only in [false, true] {
            assert_eq!(
                route(Ok(text.into()), copy_only, true),
                Completion::ShowResult(text.into())
            );
        }
        for scope in [CopyScope::Selection, CopyScope::Full] {
            assert_eq!(
                publish_text(text, 0..text.len(), scope, |_| {
                    writes += 1;
                    Ok::<_, ()>(())
                }),
                Ok(false)
            );
        }
    }
    assert_eq!(writes, 0);
}

#[test]
fn empty_invalid_or_non_boundary_selection_falls_back_to_full_text() {
    let text = "甲🙂 乙";
    for range in [
        0..0,
        1..3,
        3..4,
        std::ops::Range { start: 7, end: 3 },
        0..usize::MAX,
    ] {
        assert_eq!(copy_payload(text, range, CopyScope::Selection), Some(text));
    }
    assert_eq!(copy_payload(text, 3..7, CopyScope::Selection), Some("🙂"));
}

#[test]
fn selected_whitespace_is_preserved_when_full_text_has_content() {
    let text = "甲 \u{3000}🙂";
    let mut selection = Selection::default();
    selection.begin(text, 7);
    selection.extend(text, 3);
    selection.end_drag();
    assert_eq!(selection.range(), 3..7);
    assert_eq!(
        copy_payload(text, selection.range(), CopyScope::Selection),
        Some(" \u{3000}")
    );
}

#[test]
fn full_copy_scope_uses_actual_editor_draft_despite_partial_selection() {
    let mut editor = ResultEditor::new("OCR 原文🙂".into(), 0..0);
    editor.select_all();
    editor.replace(None, "用户未收起的草稿\n第二行🙂", false, None);
    editor.begin_drag(0);
    editor.extend_drag(3);
    editor.end_drag();
    assert_eq!(editor.selected_or_full(), "用");
    let before = format!("{editor:?}");
    let mut published = String::new();
    assert_eq!(
        publish_text(
            &editor.text,
            editor.input.selection.clone(),
            CopyScope::Full,
            |value| {
                published = value.to_owned();
                Ok::<_, ()>(())
            }
        ),
        Ok(true)
    );
    assert_eq!(published, editor.text);
    assert_eq!(format!("{editor:?}"), before);
}

#[test]
fn failed_selection_publication_keeps_body_drag_and_selection_for_retry() {
    let text = String::from("甲🙂\n乙");
    let mut selection = Selection::default();
    selection.begin(&text, 7);
    selection.extend(&text, 3);
    let before = Selection {
        range: selection.range(),
        drag_anchor: selection.drag_anchor,
    };
    let mut attempts = Vec::new();
    assert_eq!(
        publish_text(&text, selection.range(), CopyScope::Selection, |value| {
            attempts.push(value.to_owned());
            Err("剪贴板繁忙")
        }),
        Err("剪贴板繁忙")
    );
    assert_eq!(text, "甲🙂\n乙");
    assert_eq!(selection, before);
    assert_eq!(
        publish_text(&text, selection.range(), CopyScope::Selection, |value| {
            attempts.push(value.to_owned());
            Ok::<_, &str>(())
        }),
        Ok(true)
    );
    assert_eq!(attempts, ["🙂", "🙂"]);
    assert_eq!(selection, before);
}

#[test]
fn failed_editor_publication_keeps_draft_input_revision_and_undo_redo_for_retry() {
    let mut editor = ResultEditor::new("原文🙂".into(), 0..0);
    editor.select_all();
    editor.replace(None, "草稿甲🙂", false, None);
    editor.select_all();
    editor.replace(None, "草稿乙🙂", false, None);
    editor.undo();
    assert!(!editor.undo.is_empty());
    assert!(!editor.redo.is_empty());
    editor.begin_drag(editor.text.len());
    editor.extend_drag(0);
    let before = format!("{editor:?}");
    let text = editor.text.clone();
    let input = editor.input.clone();
    let revision = editor.revision;
    let mut attempts = Vec::new();
    assert_eq!(
        publish_text(
            &editor.text,
            editor.input.selection.clone(),
            CopyScope::Full,
            |value| {
                attempts.push(value.to_owned());
                Err("剪贴板繁忙")
            }
        ),
        Err("剪贴板繁忙")
    );
    assert_eq!(editor.text, text);
    assert_eq!(editor.input, input);
    assert_eq!(editor.revision, revision);
    assert_eq!(format!("{editor:?}"), before);
    assert_eq!(
        publish_text(
            &editor.text,
            editor.input.selection.clone(),
            CopyScope::Full,
            |value| {
                attempts.push(value.to_owned());
                Ok::<_, &str>(())
            }
        ),
        Ok(true)
    );
    assert_eq!(attempts, [text.clone(), text]);
    assert_eq!(format!("{editor:?}"), before);

    // 实际 IME 草稿仍由 host 门禁拥有输入；输出 fallback 不提交或取消组合。
    editor.end_drag();
    editor.move_to(editor.text.len(), false);
    editor.replace(None, "预编辑", true, Some(0..1));
    assert!(editor.composing());
    assert!(editor.composition_before.is_some());
    let composing = format!("{editor:?}");
    assert_eq!(
        output_key(
            "enter",
            modifiers(0),
            false,
            OutputGate {
                editing: true,
                input_ready: false,
                ..ready_gate()
            }
        ),
        OutputKey::Consume
    );
    assert_eq!(format!("{editor:?}"), composing);
}
