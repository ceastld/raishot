//! 新增关闭恢复的组合模型定义；只走真实 Editor/Document 和 Wire JSON。
//! 不证明 Pin 窗口关闭、磁盘租约、Scene 或实际 GUI 的恢复行为。
use super::*;

const SIZE: (u32, u32) = (640, 480);

fn editor() -> Editor {
    Editor::new(Rect::full(SIZE.0, SIZE.1), Some(Rect::full(SIZE.0, SIZE.1)))
}

fn rectangle(x: f32, color: u32) -> Annotation {
    let mut annotation = Annotation::new(
        Tool::Rectangle,
        Point { x, y: 20. },
        Style {
            color,
            ..Style::default()
        },
    );
    annotation.update(Point { x: x + 30., y: 45. });
    annotation
}

fn json_roundtrip(source: &Editor) -> (WireRasterDocument, Editor) {
    let wire = WireRasterDocument::from_editor(source, SIZE).unwrap();
    let json = serde_json::to_vec(&wire).unwrap();
    let decoded: WireRasterDocument = serde_json::from_slice(&json).unwrap();
    let mut restored = editor();
    decoded.restore_into(&mut restored).unwrap();
    (decoded, restored)
}

#[test]
fn restored_mixed_commands_keep_ids_batch_undo_and_pending_redo() {
    let mut source = editor();
    let batch_ids = source
        .document
        .insert_many(vec![rectangle(10., 0x112233), rectangle(60., 0x445566)]);
    let after_batch = source.document.items().to_vec();
    let single_id = source.document.insert(rectangle(110., 0x778899));
    let after_insert = source.document.items().to_vec();
    let mut changed = source.document.get(batch_ids[0]).unwrap().clone();
    changed.translate(9., 7.);
    changed.style.color = 0xabcdef;
    source.document.replace(batch_ids[0], changed);
    let after_replace = source.document.items().to_vec();
    source.document.remove(batch_ids[1]);
    let after_remove = source.document.items().to_vec();
    source.undo();
    assert_eq!(source.document.items(), after_replace.as_slice());
    assert!(source.document.can_undo());
    assert!(source.document.can_redo());

    let (_, mut restored) = json_roundtrip(&source);
    assert_eq!(restored.crop, source.crop);
    assert_eq!(restored.document.items(), source.document.items());
    assert!(restored.document.can_undo());
    assert!(restored.document.can_redo());
    restored.redo();
    assert_eq!(restored.document.items(), after_remove.as_slice());
    restored.undo();
    assert_eq!(restored.document.items(), after_replace.as_slice());
    restored.undo();
    assert_eq!(restored.document.items(), after_insert.as_slice());
    restored.undo();
    assert_eq!(restored.document.items(), after_batch.as_slice());
    // insert_many 的两个真实对象仍是一条命令，不拆成恢复过程的新插入。
    restored.undo();
    assert!(restored.document.items().is_empty());
    assert!(!restored.document.can_undo());
    for expected in [&after_batch, &after_insert, &after_replace, &after_remove] {
        restored.redo();
        assert_eq!(restored.document.items(), expected.as_slice());
    }
    assert!(!restored.document.can_redo());
    let next = restored.document.insert(rectangle(160., 0x123456));
    assert!(next > single_id);
    assert!(next > batch_ids[0] && next > batch_ids[1]);
}

#[test]
fn legacy_wire_without_history_restores_content_with_empty_navigation() {
    let mut source = editor();
    let removed = source.document.insert(rectangle(10., 0x112233));
    source.document.insert(rectangle(60., 0x445566));
    source.document.remove(removed);
    let expected = source.document.items()[0].annotation.clone();
    let wire = WireRasterDocument::from_editor(&source, SIZE).unwrap();
    let mut json = serde_json::to_value(&wire).unwrap();
    assert!(json.as_object_mut().unwrap().remove("history").is_some());
    let legacy: WireRasterDocument = serde_json::from_value(json).unwrap();
    let mut restored = editor();
    legacy.restore_into(&mut restored).unwrap();
    assert_eq!(restored.document.items().len(), 1);
    assert_eq!(restored.document.items()[0].annotation, expected);
    assert_eq!(restored.document.items()[0].id, 1);
    assert!(!restored.document.can_undo());
    assert!(!restored.document.can_redo());
    let baseline = restored.document.items().to_vec();
    restored.undo();
    restored.redo();
    assert_eq!(restored.document.items(), baseline.as_slice());
}

#[test]
fn cancelled_gesture_and_noop_do_not_add_restored_history_or_consume_redo() {
    let mut source = editor();
    let first = source.document.insert(rectangle(10., 0x112233));
    source.document.insert(rectangle(60., 0x445566));
    source.undo();
    let before = WireRasterDocument::from_editor(&source, SIZE).unwrap();
    assert!(source.document.can_redo());
    source.select_tool(Tool::Rectangle);
    source.pointer_down((120, 140), 1.);
    source.pointer_move((150, 170));
    assert!(source.cancel_gesture());
    source.pointer_down((180, 200), 1.);
    source.pointer_up((180, 200));
    let same = source.document.get(first).unwrap().clone();
    source.document.replace(first, same);
    source.document.remove(u64::MAX);
    assert!(source.document.insert_many(Vec::new()).is_empty());
    let after = WireRasterDocument::from_editor(&source, SIZE).unwrap();
    assert_eq!(after, before);
    let (_, mut restored) = json_roundtrip(&source);
    assert!(restored.document.can_redo());
    source.redo();
    restored.redo();
    assert_eq!(restored.document.items(), source.document.items());
    source.undo();
    restored.undo();
    assert_eq!(restored.document.items(), source.document.items());
}

#[test]
fn bounded_wire_keeps_latest_contiguous_real_history_under_json_budget() {
    let mut source = editor();
    let mut annotation = Annotation::new(
        Tool::Text,
        Point { x: 10., y: 20. },
        Style {
            color: 0,
            ..Style::default()
        },
    );
    annotation.text = "x".repeat(8 * 1024);
    let id = source.document.insert(annotation);
    for color in 1..=140 {
        let mut after = source.document.get(id).unwrap().clone();
        after.style.color = color;
        source.document.replace(id, after);
    }
    assert_eq!(
        source
            .document
            .bounded_history_snapshot()
            .unwrap()
            .undo
            .len(),
        128
    );
    let wire = WireRasterDocument::from_editor(&source, SIZE).unwrap();
    let bytes = serde_json::to_vec(&wire).unwrap();
    assert!(bytes.len() <= 480 * 1024);
    let json = serde_json::to_value(&wire).unwrap();
    let kept = json["history"]["undo"].as_array().unwrap().len();
    assert!(kept > 0 && kept < 128);
    assert!(json["history"]["redo"].as_array().unwrap().is_empty());
    let decoded: WireRasterDocument = serde_json::from_slice(&bytes).unwrap();
    let mut restored = editor();
    decoded.restore_into(&mut restored).unwrap();
    assert_eq!(restored.document.items(), source.document.items());
    // 对照由原 Editor 实际 undo 取得，不重写命令重放或猜被裁剪后的状态。
    for _ in 0..kept {
        source.undo();
        restored.undo();
        assert_eq!(restored.document.items(), source.document.items());
    }
    assert!(!restored.document.can_undo());
    assert!(source.document.can_undo());
    let oldest_kept = restored.document.items().to_vec();
    restored.undo();
    assert_eq!(restored.document.items(), oldest_kept.as_slice());
    for _ in 0..kept {
        source.redo();
        restored.redo();
        assert_eq!(restored.document.items(), source.document.items());
    }
    assert!(!restored.document.can_redo());
    assert_eq!(restored.document.items()[0].id, id);
}

#[test]
fn damaged_history_rejects_before_touching_fresh_restore_target() {
    let mut source = editor();
    let first = source.document.insert(rectangle(10., 0x112233));
    source.document.insert(rectangle(60., 0x445566));
    let mut after = source.document.get(first).unwrap().clone();
    after.style.color = 0xabcdef;
    source.document.replace(first, after);
    let wire = WireRasterDocument::from_editor(&source, SIZE).unwrap();
    let valid = serde_json::to_value(&wire).unwrap();
    assert_eq!(valid["history"]["undo"][2]["kind"], "replace");
    let mut wrong_version = valid.clone();
    wrong_version["history"]["version"] = serde_json::json!(2);
    let mut wrong_index = valid.clone();
    wrong_index["history"]["undo"][2]["index"] = serde_json::json!(999_999);
    let mut duplicate_ids = valid.clone();
    duplicate_ids["history"]["ids"][1] = valid["history"]["ids"][0].clone();
    let mut reused_counter = valid;
    reused_counter["history"]["nextId"] = serde_json::json!(0);
    for (reason, json) in [
        ("history version", wrong_version),
        ("command index", wrong_index),
        ("duplicate IDs", duplicate_ids),
        ("ID counter", reused_counter),
    ] {
        let damaged: WireRasterDocument = serde_json::from_value(json).unwrap();
        assert!(damaged.validate().is_err(), "应拒绝 {reason}");
        let mut target = editor();
        assert!(
            damaged.restore_into(&mut target).is_err(),
            "应拒绝 {reason}"
        );
        assert!(target.document.items().is_empty());
        assert!(target.document.crop_mask.is_none());
        assert!(!target.document.can_undo());
        assert!(!target.document.can_redo());
        assert_eq!(target.crop, Some(Rect::full(SIZE.0, SIZE.1)));
    }
}
