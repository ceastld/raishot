//! 贴图右键的真实 Editor／Document／Scene 收尾；不创建 GPUI 或原生窗口。
use super::{prepare_editor_right_click, RightClickPreparation};
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
};

fn editor() -> Editor {
    let full = Rect::full(320, 240);
    Editor::new(full, Some(full))
}
fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn rectangle(x: f32, y: f32) -> Annotation {
    let mut a = Annotation::new(Tool::Rectangle, point(x, y), Style::default());
    a.update(point(x + 60., y + 50.));
    a
}
fn redo_fixture(editor: &mut Editor) -> (ObjectId, Annotation) {
    let a = rectangle(250., 180.);
    let id = editor.document.insert(a.clone());
    editor.document.undo();
    assert!(editor.document.can_redo());
    (id, a)
}
fn pending_polyline(editor: &mut Editor, second_vertex: bool) {
    editor.select_tool(Tool::Polyline);
    editor.pointer_down((160, 100), 4.);
    editor.pointer_up((160, 100));
    if second_vertex {
        editor.pointer_down((220, 120), 4.);
        editor.pointer_up((220, 120));
    }
    editor.pointer_move((250, 200));
    assert!(editor.has_polyline());
    assert!(!editor.is_dragging());
}
fn export_scene(scene: &Scene, source: &SourcePixels<'_>) -> Vec<u8> {
    let mut image = source.read(source.canvas);
    scene.apply(&mut image, source.canvas);
    image.bgra
}

#[test]
fn pin_right_click_polyline_commits_fixed_vertices_once_with_shared_scene_and_history() {
    let mut e = editor();
    pending_polyline(&mut e, true);
    let pixels = [255; 4].repeat(320 * 240);
    let full = Rect::full(320, 240);
    let source = SourcePixels {
        fill: None,
        canvas: full,
        planes: vec![PixelPlane {
            rect: full,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let preview = Scene::prepare(&e.annotations(), &source);
    let edits = e.document.edit_revision();
    let stamp = e.scene_stamp();
    assert_eq!(
        prepare_editor_right_click(&mut e, true, false),
        RightClickPreparation::Finished
    );
    assert!(!e.has_pending_drawing());
    assert!(!e.is_dragging());
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.edit_revision(), edits + 1);
    assert_ne!(e.scene_stamp(), stamp);
    let committed = e.document.items()[0].clone();
    assert_eq!(
        committed.annotation.points,
        vec![point(160., 100.), point(220., 120.)]
    );
    assert_eq!(e.selected, Some(committed.id));
    let updated = preview.updated(&e.annotations(), &source);
    assert!(updated.first_error().is_none());
    assert_eq!(
        export_scene(&updated, &source),
        export_scene(&Scene::prepare(&e.annotations(), &source), &source)
    );
    assert_ne!(
        export_scene(&preview, &source),
        export_scene(&updated, &source)
    );
    let revision = e.document.revision();
    assert_eq!(
        prepare_editor_right_click(&mut e, true, false),
        RightClickPreparation::Pass
    );
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
    e.redo();
    assert_eq!(e.document.items(), &[committed]);
}

#[test]
fn pin_right_click_short_polyline_discards_hover_tail_without_consuming_redo() {
    let mut e = editor();
    let mask = crate::crop_mask::Polygon::new(&[
        point(5., 5.),
        point(300., 5.),
        point(300., 230.),
        point(5., 230.),
    ])
    .unwrap();
    e.document.set_crop_mask(Some(mask.clone()));
    let (redo_id, redo_annotation) = redo_fixture(&mut e);
    let crop = e.crop;
    let before = e.document.items().to_vec();
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    let original_mask = e.document.crop_mask.clone();
    pending_polyline(&mut e, false);
    assert_eq!(
        prepare_editor_right_click(&mut e, true, false),
        RightClickPreparation::Finished
    );
    assert!(!e.has_pending_drawing());
    assert_eq!(e.document.items(), before);
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert_eq!(e.document.crop_mask, original_mask);
    assert_eq!(e.crop, crop);
    assert!(e.document.can_redo());
    e.redo();
    assert_eq!(e.document.get(redo_id), Some(&redo_annotation));
    assert_eq!(e.document.crop_mask, Some(mask));
}

#[test]
fn pin_right_click_object_drag_cancels_preview_without_committing_or_losing_redo() {
    let mut e = editor();
    let original = rectangle(20., 20.);
    let id = e.document.insert(original.clone());
    let (redo_id, redo_annotation) = redo_fixture(&mut e);
    e.select_tool(Tool::Edit);
    e.pointer_down((50, 40), 4.);
    e.pointer_move((75, 60));
    assert!(e.is_dragging());
    assert_ne!(e.annotations(), vec![original.clone()]);
    let before = e.document.items().to_vec();
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    let stamp = e.scene_stamp();
    assert_eq!(
        prepare_editor_right_click(&mut e, true, false),
        RightClickPreparation::Cancelled
    );
    assert!(!e.is_dragging());
    assert_eq!(e.annotations(), vec![original.clone()]);
    assert_eq!(e.document.get(id), Some(&original));
    assert_eq!(e.document.items(), before);
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert_ne!(e.scene_stamp(), stamp);
    // 迟到的左键 Up 不会再次提交被取消的预览。
    e.pointer_up((90, 80));
    assert_eq!(e.document.items(), before);
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.document.can_redo());
    e.redo();
    assert_eq!(e.document.get(redo_id), Some(&redo_annotation));
}

#[test]
fn pin_right_click_group_drag_restores_all_members_and_keeps_one_existing_history() {
    let mut e = editor();
    let first = e.document.insert(rectangle(20., 20.));
    let second = e.document.insert(rectangle(120., 20.));
    assert!(e.document.set_group(&[first, second], true));
    let (redo_id, redo_annotation) = redo_fixture(&mut e);
    e.select_tool(Tool::Edit);
    let before = e.document.items().to_vec();
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    e.pointer_down((50, 40), 4.);
    e.pointer_move((75, 60));
    assert!(e.is_dragging());
    assert_eq!(e.selected_ids().len(), 2);
    assert_ne!(
        e.annotations(),
        before
            .iter()
            .map(|o| o.annotation.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        prepare_editor_right_click(&mut e, true, false),
        RightClickPreparation::Cancelled
    );
    assert_eq!(e.document.items(), before);
    assert_eq!(
        e.annotations(),
        before
            .iter()
            .map(|o| o.annotation.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.document.grouped(first) && e.document.grouped(second));
    assert!(e.document.can_redo());
    e.redo();
    assert_eq!(e.document.get(redo_id), Some(&redo_annotation));
}

#[test]
fn pin_right_click_pending_and_dragging_arrows_cancel_without_ghost_or_history_change() {
    for pending in [false, true] {
        let mut e = editor();
        let (redo_id, redo_annotation) = redo_fixture(&mut e);
        e.select_tool(Tool::Arrow);
        e.pointer_down((160, 100), 4.);
        if pending {
            e.pointer_up((160, 100));
        }
        e.pointer_move((220, 120));
        assert!(e.has_arrow_draft());
        assert_eq!(e.has_pending_drawing(), pending);
        assert_eq!(e.is_dragging(), !pending);
        let revision = e.document.revision();
        let edits = e.document.edit_revision();
        let stamp = e.scene_stamp();
        assert_eq!(
            prepare_editor_right_click(&mut e, true, false),
            RightClickPreparation::Cancelled
        );
        assert!(!e.has_arrow_draft());
        assert!(!e.has_pending_drawing());
        assert!(!e.is_dragging());
        assert!(e.annotations().is_empty());
        assert_eq!(e.document.revision(), revision);
        assert_eq!(e.document.edit_revision(), edits);
        assert_ne!(e.scene_stamp(), stamp);
        e.pointer_up((230, 130));
        assert!(e.document.items().is_empty());
        assert!(e.document.can_redo());
        e.redo();
        assert_eq!(e.document.get(redo_id), Some(&redo_annotation));
    }
}

#[test]
fn pin_right_click_ineligible_or_composing_keeps_pending_state_and_text_is_not_submitted() {
    let mut e = editor();
    pending_polyline(&mut e, true);
    let annotations = e.annotations();
    let stamp = e.scene_stamp();
    assert_eq!(
        prepare_editor_right_click(&mut e, false, false),
        RightClickPreparation::Pass
    );
    assert_eq!(e.annotations(), annotations);
    assert_eq!(e.scene_stamp(), stamp);
    assert_eq!(
        prepare_editor_right_click(&mut e, true, true),
        RightClickPreparation::Blocked
    );
    assert_eq!(e.annotations(), annotations);
    assert_eq!(e.scene_stamp(), stamp);
    assert!(!e.document.can_undo());
    assert!(e.has_polyline());
    assert_eq!(
        prepare_editor_right_click(&mut e, true, false),
        RightClickPreparation::Finished
    );
    assert_eq!(e.document.items().len(), 1);

    let mut text = editor();
    text.select_tool(Tool::Text);
    text.begin_text(None, point(20., 20.));
    text.set_text("保留未提交文字".into());
    let annotations = text.annotations();
    let stamp = text.scene_stamp();
    assert_eq!(
        prepare_editor_right_click(&mut text, true, true),
        RightClickPreparation::Pass
    );
    assert!(text.text_draft.is_some());
    assert_eq!(text.annotations(), annotations);
    assert_eq!(text.scene_stamp(), stamp);
    assert!(text.document.items().is_empty());
}
