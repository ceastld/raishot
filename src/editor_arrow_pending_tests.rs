//! 箭头等待终点时切换旧对象：原始坐标、分组、实际手势与增量历史。
use super::*;

fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}

fn editor() -> Editor {
    Editor::new(Rect::full(800, 600), Some(Rect::full(800, 600)))
}

fn arrow() -> Annotation {
    let mut a = Annotation::new(
        Tool::Arrow,
        point(80., 80.),
        Style {
            color: 0x2266cc,
            ..Style::default()
        },
    );
    a.update(point(240., 120.));
    a
}

fn pending(e: &mut Editor) {
    e.select_tool(Tool::Arrow);
    e.pointer_down((600, 350), 4.);
    e.pointer_up((600, 350));
    e.pointer_move((660, 390));
    assert!(matches!(e.gesture, Some(Gesture::ArrowPending(_))));
    assert!(!e.is_dragging());
}

fn fixture() -> (Editor, ObjectId, Annotation) {
    let mut e = editor();
    let a = arrow();
    let id = e.document.insert(a.clone());
    pending(&mut e);
    (e, id, a)
}

fn redo(e: &mut Editor) -> (ObjectId, Annotation) {
    let mut a = Annotation::new(Tool::Rectangle, point(680., 40.), Style::default());
    a.update(point(760., 120.));
    let id = e.document.insert(a.clone());
    e.document.undo();
    assert!(e.document.can_redo());
    (id, a)
}

fn redo_fixture() -> (Editor, ObjectId, Annotation) {
    let mut e = editor();
    let a = arrow();
    let id = e.document.insert(a.clone());
    redo(&mut e);
    pending(&mut e);
    (e, id, a)
}

fn near(a: Point, b: Point) {
    assert!(a.distance(b) < 0.002, "世界坐标发生偏移：{a:?} / {b:?}");
}

fn bend(a: &Annotation) -> (f32, f32) {
    let handles = a.handles();
    let (tail, tip, mid) = (handles[0], handles[1], handles[2]);
    let (dx, dy) = (tip.x - tail.x, tip.y - tail.y);
    let (mx, my) = (mid.x - tail.x, mid.y - tail.y);
    let length2 = dx * dx + dy * dy;
    ((mx * dx + my * dy) / length2, (dx * my - dy * mx) / length2)
}

#[test]
fn raw_shift_hit_keeps_crop_mask_style_history_and_redo() {
    let mut e = editor();
    let a = arrow();
    let id = e.document.insert(a.clone());
    e.crop = Some(Rect {
        x: 40,
        y: 30,
        width: 700,
        height: 500,
    });
    let mask = crate::crop_mask::Polygon::new(&[
        point(50., 40.),
        point(730., 40.),
        point(730., 520.),
        point(50., 520.),
    ])
    .unwrap();
    e.document.set_crop_mask(Some(mask.clone()));
    let (redo_id, redo_object) = redo(&mut e);
    pending(&mut e);
    let crop = e.crop;
    let before = e.document.items().to_vec();
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    let hit = point(140., 95.);
    assert!(e.constrained_point(hit, true).distance(hit) > 4.);
    assert_eq!(e.hover_at(hit, 4.), Hover::Move);
    e.pointer_down_at(hit, 4., true);
    assert!(!e.has_arrow_draft());
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.style, a.style);
    assert_eq!(e.tool, Tool::Arrow);
    e.pointer_up_at(e.constrained_point(hit, true));
    assert_eq!(e.document.items(), before);
    assert_eq!(e.annotations(), vec![a]);
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert_eq!(e.document.crop_mask, Some(mask));
    assert_eq!(e.crop, crop);
    assert!(e.document.can_redo());
    e.redo();
    assert_eq!(e.document.get(redo_id), Some(&redo_object));
}

#[test]
fn shift_endpoint_click_and_micro_motion_preserve_redo() {
    for endpoint in [0, 1] {
        let (mut e, id, a) = redo_fixture();
        let hit = a.handles()[endpoint];
        let revision = e.document.revision();
        assert_eq!(e.hover_at(hit, 4.), Hover::Crosshair);
        e.pointer_down_at(hit, 4., true);
        assert!(
            matches!(e.gesture, Some(Gesture::Object { handle: Some(h), .. }) if h == endpoint)
        );
        let up = e.constrained_point(hit, true);
        assert_eq!(up, hit);
        e.pointer_up_at(up);
        assert_eq!(e.document.get(id), Some(&a));
        assert_eq!(e.document.revision(), revision);
        assert!(e.document.can_redo());

        pending(&mut e);
        e.pointer_down_at(hit, 4., true);
        let raw_up = point(hit.x + 3., hit.y);
        let up = e.constrained_point(raw_up, true);
        assert_eq!(up, raw_up);
        e.pointer_up_at(up);
        assert_eq!(e.document.get(id), Some(&a));
        assert_eq!(e.document.revision(), revision);
        assert!(e.document.can_redo());
    }
}

#[test]
fn line_drag_from_switch_commits_once() {
    let (mut e, id, a) = fixture();
    let edits = e.document.edit_revision();
    e.pointer_down((140, 95), 4.);
    e.pointer_move((160, 110));
    e.pointer_move((175, 120));
    assert_eq!(e.document.get(id), Some(&a));
    e.pointer_up((185, 130));
    let mut expected = a.clone();
    expected.translate(45., 35.);
    assert_eq!(e.document.get(id), Some(&expected));
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    e.redo();
    assert_eq!(e.document.get(id), Some(&expected));
    e.undo();
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}

#[test]
fn rotated_curve_endpoint_drag_snaps_and_keeps_bend() {
    let mut e = editor();
    let mut a = arrow();
    a.points.push(point(145., 45.));
    a.rotation = 29.;
    let id = e.document.insert(a.clone());
    pending(&mut e);
    let original = a.handles();
    let original_bend = bend(&a);
    let edits = e.document.edit_revision();
    e.pointer_down_at(original[1], 4., true);
    let raw = point(original[1].x + 38., original[1].y + 65.);
    let snapped = e.constrained_point(raw, true);
    near(snapped, snap_angle(original[0], raw));
    assert!(snapped.distance(raw) > 4.);
    e.pointer_move_at(snapped);
    assert_eq!(e.document.get(id), Some(&a));
    e.pointer_up_at(snapped);
    let result = e.document.get(id).unwrap();
    near(result.handles()[0], original[0]);
    near(result.handles()[1], snapped);
    let result_bend = bend(result);
    assert!((result_bend.0 - original_bend.0).abs() < 0.001);
    assert!((result_bend.1 - original_bend.1).abs() < 0.001);
    assert_eq!(result.rotation, a.rotation);
    assert_eq!(result.points.len(), 3);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
}

#[test]
fn grouped_endpoint_switch_moves_all_members_once() {
    let mut e = editor();
    let a = arrow();
    let arrow_id = e.document.insert(a.clone());
    let mut rectangle = Annotation::new(Tool::Rectangle, point(300., 40.), Style::default());
    rectangle.update(point(400., 120.));
    let rectangle_id = e.document.insert(rectangle);
    assert!(e.document.set_group(&[arrow_id, rectangle_id], true));
    let before = e.document.items().to_vec();
    let edits = e.document.edit_revision();
    pending(&mut e);
    let hit = a.handles()[1];
    assert_eq!(e.hover_at(hit, 4.), Hover::Move);
    e.pointer_down_at(hit, 4., true);
    assert!(matches!(e.gesture, Some(Gesture::Group { .. })));
    let up = e.constrained_point(point(hit.x + 20., hit.y + 15.), true);
    e.pointer_up_at(up);
    for original in &before {
        let mut expected = original.annotation.clone();
        expected.translate(20., 15.);
        assert_eq!(e.document.get(original.id), Some(&expected));
    }
    assert_eq!(e.selected_ids(), vec![arrow_id, rectangle_id]);
    assert!(e.document.grouped(arrow_id) && e.document.grouped(rectangle_id));
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.items(), before);
}

#[test]
fn reverse_arrow_hit_ignores_later_other_tools() {
    let mut e = editor();
    let bottom = e.document.insert(arrow());
    let mut top = arrow();
    top.style.color = 0x44bb66;
    let top_id = e.document.insert(top.clone());
    let mut rectangle = Annotation::new(
        Tool::Rectangle,
        point(60., 60.),
        Style {
            filled: true,
            ..Style::default()
        },
    );
    rectangle.update(point(270., 160.));
    let rectangle_id = e.document.insert(rectangle.clone());
    pending(&mut e);
    let revision = e.document.revision();
    e.pointer_down((140, 95), 4.);
    e.pointer_up((140, 95));
    assert_eq!(e.selected, Some(top_id));
    assert_eq!(e.style, top.style);
    assert_eq!(e.document.get(rectangle_id), Some(&rectangle));
    assert_eq!(e.document.items()[0].id, bottom);
    assert_eq!(e.document.items()[1].id, top_id);
    assert_eq!(e.document.revision(), revision);
}

#[test]
fn missed_shift_hit_resumes_from_fixed_tail_and_commits_once() {
    let (mut e, id, a) = fixture();
    let edits = e.document.edit_revision();
    let tail = point(600., 350.);
    let raw_down = point(720., 410.);
    let first_tip = snap_angle(tail, raw_down);
    assert_eq!(e.hover_at(raw_down, 4.), Hover::Crosshair);
    e.pointer_down_at(raw_down, 4., true);
    let Some(Gesture::Draw(draft)) = &e.gesture else {
        panic!("未命中旧箭头必须恢复本次终点拖动");
    };
    assert_eq!(draft.points[0], tail);
    near(draft.points[1], first_tip);
    assert_eq!(e.document.edit_revision(), edits);
    let raw_up = point(760., 420.);
    let tip = e.constrained_point(raw_up, true);
    near(tip, snap_angle(tail, raw_up));
    e.pointer_up_at(tip);
    assert!(!e.has_arrow_draft());
    assert_eq!(e.document.items().len(), 2);
    assert_eq!(e.document.get(id), Some(&a));
    let result = &e.document.items()[1].annotation;
    assert_eq!(result.points[0], tail);
    near(result.points[1], tip);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.items().len(), 1);
}

#[test]
fn switched_drag_cancel_ignores_late_up_and_keeps_redo() {
    let (mut e, id, a) = redo_fixture();
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    e.pointer_down((140, 95), 4.);
    e.pointer_move((175, 125));
    assert_ne!(e.annotations(), vec![a.clone()]);
    assert_eq!(e.document.get(id), Some(&a));
    assert!(e.cancel_gesture());
    e.pointer_up((190, 135));
    assert_eq!(e.annotations(), vec![a.clone()]);
    assert_eq!(e.document.get(id), Some(&a));
    assert!(!e.has_arrow_draft());
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.document.can_redo());
}

#[test]
fn grouped_switch_cancel_keeps_members_and_redo_after_late_up() {
    let mut e = editor();
    let a = arrow();
    let id = e.document.insert(a.clone());
    let mut companion = arrow();
    companion.translate(300., 0.);
    let companion_id = e.document.insert(companion);
    assert!(e.document.set_group(&[id, companion_id], true));
    redo(&mut e);
    pending(&mut e);
    let before = e.document.items().to_vec();
    let revision = e.document.revision();
    e.pointer_down_at(a.handles()[1], 4., false);
    assert!(matches!(e.gesture, Some(Gesture::Group { .. })));
    e.pointer_move((270, 145));
    assert!(e.cancel_gesture());
    e.pointer_up((290, 165));
    assert_eq!(e.document.items(), before);
    assert_eq!(e.document.revision(), revision);
    assert!(e.document.can_redo());
    assert!(e.document.grouped(id) && e.document.grouped(companion_id));
}

#[test]
fn switched_curve_midpoint_keeps_right_click_straighten_contract() {
    let mut e = editor();
    let mut a = arrow();
    a.points.push(point(145., 45.));
    a.rotation = 23.;
    let id = e.document.insert(a.clone());
    pending(&mut e);
    let original = a.handles();
    let edits = e.document.edit_revision();
    let mid = original[2];
    assert_eq!(e.hover_at(mid, 4.), Hover::Crosshair);
    e.pointer_down_at(mid, 4., false);
    assert!(matches!(
        e.gesture,
        Some(Gesture::Object {
            handle: Some(2),
            ..
        })
    ));
    e.pointer_up_at(mid);
    assert_eq!(e.document.get(id), Some(&a));
    assert!(!e.straighten_arrow_point(point(mid.x + 40., mid.y + 40.), 4.));
    assert!(e.straighten_arrow_point(mid, 4.));
    let result = e.document.get(id).unwrap();
    assert_eq!(result.points.len(), 2);
    near(result.handles()[0], original[0]);
    near(result.handles()[1], original[1]);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
}

#[test]
fn pending_double_click_exits_without_committing_or_losing_redo() {
    let (mut e, id, a) = redo_fixture();
    let revision = e.document.revision();
    let crop = e.crop;
    assert_eq!(
        crate::capture_double_click::ordinary_double_click(
            &e, e.canvas, 2, true, false, false, false,
        ),
        crate::capture_double_click::DoubleClickAction::Edit
    );
    e.select_tool(Tool::Edit);
    e.pointer_up((700, 450));
    assert!(!e.has_pending_drawing());
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.crop, crop);
    assert!(e.document.can_redo());
}

#[test]
fn shallow_midpoint_click_keeps_curve_and_redo() {
    for delta in [0., 3.] {
        let mut e = editor();
        let mut a = arrow();
        a.points.push(point(160., 105.));
        let id = e.document.insert(a.clone());
        let (redo_id, redo_object) = redo(&mut e);
        pending(&mut e);
        let revision = e.document.revision();
        let edits = e.document.edit_revision();
        let mid = a.handles()[2];
        e.pointer_down_at(mid, 4., false);
        assert!(matches!(
            e.gesture,
            Some(Gesture::Object {
                handle: Some(2),
                ..
            })
        ));
        let up = point(mid.x + delta, mid.y);
        e.pointer_move_at(up);
        e.pointer_up_at(up);
        assert_eq!(e.document.get(id), Some(&a));
        assert_eq!(e.annotations(), vec![a]);
        assert_eq!(e.document.revision(), revision);
        assert_eq!(e.document.edit_revision(), edits);
        assert!(e.document.can_redo());
        e.redo();
        assert_eq!(e.document.get(redo_id), Some(&redo_object));
    }
}

#[test]
fn actual_midpoint_drag_to_chord_straightens_once() {
    let mut e = editor();
    let mut a = arrow();
    a.points.push(point(160., 105.));
    let id = e.document.insert(a.clone());
    redo(&mut e);
    pending(&mut e);
    let edits = e.document.edit_revision();
    let mid = a.handles()[2];
    let center = point(160., 100.);
    assert!(mid.distance(center) >= 4. && mid.distance(center) <= 6.);
    e.pointer_down_at(mid, 4., false);
    e.pointer_move_at(center);
    assert_eq!(e.document.get(id), Some(&a));
    e.pointer_up_at(center);
    let straightened = e.document.get(id).unwrap().clone();
    assert_eq!(straightened.points.len(), 2);
    assert_eq!(straightened.points[0], a.points[0]);
    assert_eq!(straightened.points[1], a.points[1]);
    assert_eq!(e.document.edit_revision(), edits + 1);
    assert!(!e.document.can_redo());
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    e.redo();
    assert_eq!(e.document.get(id), Some(&straightened));
    e.undo();
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}
