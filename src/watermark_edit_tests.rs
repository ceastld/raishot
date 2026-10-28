use crate::watermark_edit::{
    choose_target, normalize_single_line, EditError, EditKind, PreparedEdit, WatermarkEdit,
};
use std::time::Duration;

fn at(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn text(edit: &WatermarkEdit, value: &str, ms: u64) -> PreparedEdit {
    edit.prepare_text(edit.binding(), value, at(ms))
        .unwrap()
        .unwrap()
}

#[test]
fn watermark_edit_creation_and_clear_have_distinct_history_boundaries() {
    let mut edit = WatermarkEdit::default();
    let unbound = edit.bind(None, "");
    assert!(edit
        .prepare_text(unbound, " \r\n ", at(0))
        .unwrap()
        .is_none());
    let insert = text(&edit, "私有图片", 10);
    assert_eq!(insert.kind, EditKind::Create);
    assert!(insert.history.is_none());
    let bound = edit.acknowledge(insert, Some(17)).unwrap();
    assert_ne!(unbound, bound);

    let first = text(&edit, "私有图片·甲", 20);
    assert_eq!(first.kind, EditKind::Replace);
    let replacement_group = first.history.unwrap();
    edit.acknowledge(first, None).unwrap();
    let clear = text(&edit, "", 30);
    assert_eq!(clear.kind, EditKind::Replace);
    assert_eq!(clear.history, Some(replacement_group));
    edit.acknowledge(clear, None).unwrap();
    assert_eq!(edit.target(), Some(17));
    assert_eq!(edit.committed_text(), "");

    edit.bind(Some(17), "私有图片·甲"); // 文档 Undo 后的静默同步。
    let restored = text(&edit, "再次输入", 40);
    assert_eq!(restored.kind, EditKind::Replace);
    assert_ne!(restored.history, Some(replacement_group));
}

#[test]
fn watermark_edit_sliding_two_second_group_and_noop_does_not_extend() {
    let mut edit = WatermarkEdit::default();
    edit.bind(Some(3), "原文");
    let first = text(&edit, "一", 100);
    let group = first.history;
    edit.acknowledge(first, None).unwrap();
    let second = text(&edit, "二", 2100);
    assert_eq!(second.history, group); // 恰好两秒仍合并。
    edit.acknowledge(second, None).unwrap();
    let third = text(&edit, "三", 4100);
    assert_eq!(third.history, group); // 滑动窗口，不是从首次修改计时。
    edit.acknowledge(third, None).unwrap();
    assert!(edit
        .prepare_text(edit.binding(), "三", at(6000))
        .unwrap()
        .is_none());
    let expired = text(&edit, "四", 6101);
    assert_ne!(expired.history, group);
    edit.acknowledge(expired, None).unwrap();
    let backwards = text(&edit, "五", 6100);
    assert_ne!(
        backwards.history,
        edit.prepare_style(edit.binding(), true, at(6101))
            .unwrap()
            .unwrap()
            .history
    );
}

#[test]
fn watermark_edit_ime_preview_cancel_and_confirmation_are_transactional() {
    let mut edit = WatermarkEdit::default();
    let binding = edit.bind(Some(6), "图片说明");
    let prepared = text(&edit, "图片说明甲", 0);
    edit.set_preview(binding, "图片说ming").unwrap();
    assert_eq!(edit.display_text(), "图片说ming");
    assert_eq!(edit.committed_text(), "图片说明");
    assert!(edit.accepts(&prepared));
    edit.cancel_preview(binding).unwrap();
    assert_eq!(edit.display_text(), "图片说明");
    assert!(!edit.has_preview());
    edit.set_preview(binding, "图片说明甲").unwrap();
    edit.acknowledge(prepared, None).unwrap();
    assert_eq!(edit.committed_text(), "图片说明甲");
    assert!(!edit.has_preview());
    edit.set_preview(binding, "图片说明甲").unwrap();
    assert!(edit
        .prepare_text(binding, "图片说明甲", at(1))
        .unwrap()
        .is_none());
    edit.cancel_preview(binding).unwrap();
    assert_eq!(edit.display_text(), "图片说明甲");
}

#[test]
fn watermark_edit_binding_and_command_boundary_reject_late_updates() {
    let mut edit = WatermarkEdit::default();
    let first_binding = edit.bind(Some(7), "甲");
    let prepared = text(&edit, "乙", 0);
    edit.break_history(); // 其它命令与工具切换的历史边界。
    assert!(!edit.accepts(&prepared));
    assert_eq!(
        edit.acknowledge(prepared, None),
        Err(EditError::StalePrepared)
    );
    assert_eq!(edit.committed_text(), "甲");
    let replacement = text(&edit, "乙", 1);
    edit.acknowledge(replacement, None).unwrap();
    let old = text(&edit, "丙", 2);
    let rebound = edit.bind(Some(8), "另一层");
    assert_ne!(first_binding, rebound);
    assert_eq!(
        edit.set_preview(first_binding, "迟到组合"),
        Err(EditError::StaleBinding)
    );
    assert_eq!(edit.acknowledge(old, None), Err(EditError::StaleBinding));
    assert_eq!(edit.committed_text(), "另一层");
    assert_eq!(edit.target(), Some(8));
    let mut other_canvas = WatermarkEdit::default();
    other_canvas.bind(Some(7), "甲");
    assert_eq!(
        other_canvas.set_preview(first_binding, "另一画布的迟到输入"),
        Err(EditError::StaleBinding)
    );
    assert_eq!(other_canvas.committed_text(), "甲");
}

#[test]
fn watermark_edit_utf8_single_line_paste_preserves_chinese_and_whitespace() {
    assert_eq!(
        normalize_single_line("  中文📷\r\n多行\n末行\t甲\u{2028}乙\u{2029}丙\0  "),
        "  中文📷 多行 末行 甲 乙 丙  "
    );
    let mut edit = WatermarkEdit::default();
    edit.bind(Some(2), "旧");
    let prepared = text(&edit, "中文\r\n粘贴", 0);
    assert_eq!(prepared.text.as_deref(), Some("中文 粘贴"));
    edit.acknowledge(prepared, None).unwrap();
    assert_eq!(edit.committed_text(), "中文 粘贴");
    assert!(edit
        .prepare_text(edit.binding(), "中文 粘贴", at(1))
        .unwrap()
        .is_none());
    edit.bind(Some(2), "旧\n多行");
    assert_eq!(edit.committed_text(), "旧\n多行");
    let user_confirmation = text(&edit, "旧\n多行", 2);
    assert_eq!(user_confirmation.text.as_deref(), Some("旧 多行"));
}

#[test]
fn watermark_edit_selects_one_legacy_layer_without_removing_others() {
    let legacy = [4, 9, 13];
    assert_eq!(choose_target(Some(9), Some(4), &legacy), Some(9));
    assert_eq!(choose_target(Some(99), Some(4), &legacy), Some(4));
    assert_eq!(choose_target(None, Some(99), &legacy), Some(13));
    assert_eq!(choose_target(None, None, &[]), None);
    assert_eq!(legacy, [4, 9, 13]);
    let mut edit = WatermarkEdit::default();
    edit.bind(choose_target(Some(9), None, &legacy), "中间旧层");
    let replacement = text(&edit, "覆盖这一层", 0);
    assert_eq!(replacement.kind, EditKind::Replace);
    edit.acknowledge(replacement, None).unwrap();
    assert_eq!(edit.target(), Some(9));
}

#[test]
fn watermark_edit_failed_ack_leaves_text_preview_and_group_unchanged() {
    let mut edit = WatermarkEdit::default();
    edit.bind(None, "");
    let binding = edit.binding();
    edit.set_preview(binding, "预编辑").unwrap();
    let insert = text(&edit, "确认", 0);
    assert_eq!(
        edit.acknowledge(insert.clone(), None),
        Err(EditError::MissingCreatedTarget)
    );
    assert!(edit.accepts(&insert));
    assert_eq!(edit.target(), None);
    assert_eq!(edit.committed_text(), "");
    assert_eq!(edit.display_text(), "预编辑");
    edit.acknowledge(insert, Some(20)).unwrap();
    let first = text(&edit, "一", 1);
    let group = first.history;
    edit.acknowledge(first, None).unwrap();
    let pending = text(&edit, "二", 1000);
    assert_eq!(
        edit.acknowledge(pending.clone(), Some(21)),
        Err(EditError::UnexpectedCreatedTarget)
    );
    assert_eq!(edit.committed_text(), "一");
    assert!(edit.accepts(&pending));
    assert_eq!(text(&edit, "二", 1000).history, group);
    edit.acknowledge(pending, None).unwrap();
}

#[test]
fn watermark_edit_text_and_style_share_group_without_creation_from_style() {
    let mut edit = WatermarkEdit::default();
    edit.bind(None, "");
    assert!(edit
        .prepare_style(edit.binding(), true, at(0))
        .unwrap()
        .is_none());
    edit.bind(Some(12), "版权");
    let first = text(&edit, "版权甲", 0);
    let group = first.history;
    edit.acknowledge(first, None).unwrap();
    let style = edit
        .prepare_style(edit.binding(), true, at(1999))
        .unwrap()
        .unwrap();
    assert_eq!(style.history, group);
    assert_eq!(style.text, None);
    edit.acknowledge(style, None).unwrap();
    assert_eq!(edit.committed_text(), "版权甲");
    let following = text(&edit, "版权乙", 3999);
    assert_eq!(following.history, group);
    edit.acknowledge(following, None).unwrap();
    let latest = text(&edit, "版权丙", 4000);
    assert!(edit
        .prepare_style(edit.binding(), false, at(5000))
        .unwrap()
        .is_none());
    assert!(edit.accepts(&latest));
    edit.break_history(); // Undo/Redo/其它命令后不能继续合并旧替换。
    assert_ne!(text(&edit, "版权丙", 4001).history, group);
}
