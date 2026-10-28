use crate::{
    annotation::{Annotation, Point, Tool},
    editor::Editor,
    geometry::Rect,
};
use std::time::Duration;

fn text(editor: &mut Editor, value: &str, ms: u64) {
    assert!(editor.set_watermark_text_at(
        editor.watermark.binding(),
        value,
        Duration::from_millis(ms)
    ));
}
#[test]
fn editor_watermark_global_layer_keeps_crop_phase_and_one_managed_id() {
    let mut e = Editor::new(
        Rect::full(960, 32000),
        Some(Rect {
            x: 20,
            y: 600,
            width: 200,
            height: 100,
        }),
    );
    e.select_tool(Tool::Watermark);
    text(&mut e, "内部资料", 0);
    let id = e.watermark.target().unwrap();
    let points = e.document.get(id).unwrap().points.clone();
    assert_eq!(
        points,
        vec![Point { x: 0., y: 0. }, Point { x: 960., y: 32000. }]
    );
    e.pointer_down((100, 200), 4.);
    e.pointer_move((300, 400));
    e.pointer_up((300, 400));
    assert!(!e.is_dragging());
    assert!(e.text_draft.is_none());
    e.set_crop(Some(Rect {
        x: 70,
        y: 1600,
        width: 180,
        height: 100,
    }));
    e.select_tool(Tool::Watermark);
    text(&mut e, "内部资料 V2", 100);
    assert_eq!(e.watermark.target(), Some(id));
    assert_eq!(e.document.get(id).unwrap().points, points);
    assert_eq!(e.document.items().len(), 1);
}
#[test]
fn editor_watermark_style_text_clear_undo_redo_preserve_insert_boundary() {
    let mut e = Editor::new(Rect::full(640, 420), Some(Rect::full(640, 420)));
    e.select_tool(Tool::Watermark);
    text(&mut e, "A", 0);
    let id = e.watermark.target().unwrap();
    text(&mut e, "AB", 100);
    let before = e.document.get(id).unwrap().style;
    let mut style = before;
    style.color = 0x123456;
    e.set_watermark_style_at(style, Duration::from_millis(1500));
    text(&mut e, "", 3000); // 距上一修改1.5秒，同一滑动组；对象保留但无绘制。
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.get(id).unwrap().text, "");
    e.undo();
    assert_eq!(e.document.get(id).unwrap().text, "A");
    assert_eq!(e.document.get(id).unwrap().style, before);
    e.redo();
    assert_eq!(e.document.get(id).unwrap().text, "");
    e.undo();
    e.undo();
    assert!(e.document.items().is_empty());
    e.redo();
    assert_eq!(e.watermark.target(), Some(id));
    assert_eq!(e.watermark.committed_text(), "A");
}
#[test]
fn editor_watermark_composition_is_preview_and_late_binding_cannot_edit_new_document() {
    let mut e = Editor::new(Rect::full(640, 420), Some(Rect::full(640, 420)));
    e.select_tool(Tool::Watermark);
    let initial = e.watermark.binding();
    e.preview_watermark_text(initial, "预编辑", true);
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
    assert_eq!(e.annotations()[0].text, "预编辑");
    e.finish_watermark_input();
    assert!(e.annotations().is_empty());
    text(&mut e, "已确认", 0);
    let old = e.watermark.binding();
    e.clear();
    assert!(!e.set_watermark_text_at(old, "迟到事件", Duration::from_millis(100)));
    assert!(e.document.items().is_empty());
    e.undo();
    assert_eq!(e.watermark.committed_text(), "已确认");
    assert_eq!(e.document.items().len(), 1);
}
#[test]
fn editor_watermark_first_spotlight_preview_and_commit_are_below_watermark() {
    let mut e = Editor::new(Rect::full(640, 420), Some(Rect::full(640, 420)));
    e.select_tool(Tool::Watermark);
    text(&mut e, "资料", 0);
    let watermark = e.watermark.target().unwrap();
    e.select_tool(Tool::Spotlight);
    e.pointer_down((100, 100), 4.);
    e.pointer_move((260, 220));
    let preview = e.annotations();
    assert_eq!(preview[0].tool, Tool::Spotlight);
    assert_eq!(preview[1].tool, Tool::Watermark);
    e.pointer_up((260, 220));
    assert_eq!(e.document.items()[0].annotation.tool, Tool::Spotlight);
    assert_eq!(e.document.items()[1].id, watermark);
    e.undo();
    assert_eq!(e.document.items()[0].id, watermark);
    let mut legacy = Annotation::new(Tool::Watermark, Point { x: 15., y: 25. }, e.style);
    legacy.text = "旧独立对象".into();
    let legacy_id = e.document.insert(legacy.clone());
    e.selected = Some(legacy_id);
    e.select_tool(Tool::Watermark);
    text(&mut e, "仅修改旧对象", 100);
    assert_eq!(e.document.get(watermark).unwrap().text, "资料");
    assert_eq!(e.document.items().len(), 2);
    assert_eq!(e.document.get(legacy_id).unwrap().points, legacy.points);
    e.select_tool(Tool::Edit);
    e.selected = Some(watermark);
    let mut style = e.document.get(watermark).unwrap().style;
    style.opacity = 0.4;
    e.set_style(style);
    assert_eq!(e.watermark.target(), Some(watermark));
    assert_eq!(e.document.get(watermark).unwrap().style.opacity, 0.4);
    assert_eq!(e.document.get(legacy_id).unwrap().style, legacy.style);
}
