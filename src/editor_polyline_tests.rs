//! 未提交折线切换到已提交对象：真实 Down/Move/Up 与增量历史合同。
use super::*;

fn point(x: f32, y: f32) -> Point {
    Point { x, y }
}

fn editor() -> Editor {
    Editor::new(Rect::full(800, 600), Some(Rect::full(800, 600)))
}

fn line() -> Annotation {
    let mut a = Annotation::new(Tool::Polyline, point(80., 80.), Style::default());
    a.points = vec![point(80., 80.), point(200., 80.), point(240., 160.)];
    a
}

fn pending(e: &mut Editor) {
    e.select_tool(Tool::Polyline);
    e.pointer_down((400, 300), 4.);
    e.pointer_up((400, 300));
    e.pointer_down((480, 300), 4.);
    e.pointer_up((480, 300));
    assert!(e.has_polyline());
}

fn fixture() -> (Editor, ObjectId, Annotation) {
    let mut e = editor();
    let a = line();
    let id = e.document.insert(a.clone());
    pending(&mut e);
    (e, id, a)
}

fn near(a: Point, b: Point) {
    assert!(a.distance(b) < 0.001, "世界坐标发生偏移：{a:?} / {b:?}");
}

#[test]
fn switch_click_preserves_crop_mask_objects_style_and_history() {
    let (mut e, id, a) = fixture();
    let crop = Rect {
        x: 40,
        y: 30,
        width: 570,
        height: 390,
    };
    e.crop = Some(crop);
    let mask = crate::crop_mask::Polygon::new(&[
        point(50., 40.),
        point(590., 40.),
        point(590., 400.),
        point(50., 400.),
    ])
    .unwrap();
    e.document.set_crop_mask(Some(mask.clone()));
    let before = e.document.items().to_vec();
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    assert_eq!(e.hover((140, 80), 4.), Hover::Move);
    e.pointer_down((140, 80), 4.);
    assert!(!e.has_polyline());
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.style, a.style);
    assert_eq!(e.tool, Tool::Polyline);
    e.pointer_up((140, 80));
    assert_eq!(e.document.items(), before);
    assert_eq!(e.annotations(), vec![a]);
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert_eq!(e.crop, Some(crop));
    assert_eq!(e.document.crop_mask, Some(mask));
}

#[test]
fn switch_click_retains_preexisting_redo() {
    let mut e = editor();
    let id = e.document.insert(line());
    let mut extra = line();
    extra.translate(300., 0.);
    let redo_id = e.document.insert(extra.clone());
    e.document.undo();
    pending(&mut e);
    let revision = e.document.revision();
    e.pointer_down((140, 80), 4.);
    e.pointer_up((140, 80));
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.document.revision(), revision);
    assert!(e.document.can_redo());
    e.redo();
    assert_eq!(e.document.get(redo_id), Some(&extra));
}

#[test]
fn switch_line_drag_commits_once_and_undo_redo_restore_exact_objects() {
    let (mut e, id, a) = fixture();
    let edits = e.document.edit_revision();
    e.pointer_down((140, 80), 4.);
    e.pointer_move((150, 90));
    e.pointer_move((165, 100));
    assert_eq!(e.document.get(id), Some(&a));
    e.pointer_up((165, 100));
    let mut moved = a.clone();
    moved.translate(25., 20.);
    assert_eq!(e.document.get(id), Some(&moved));
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    e.redo();
    assert_eq!(e.document.get(id), Some(&moved));
    e.undo();
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}

#[test]
fn switch_vertex_drag_uses_same_press_and_keeps_other_nodes() {
    let (mut e, id, a) = fixture();
    let edits = e.document.edit_revision();
    assert_eq!(e.hover_at(a.handles()[1], 4.), Hover::Crosshair);
    e.pointer_down_at(a.handles()[1], 4., false);
    assert_eq!(e.active_node(), Some(1));
    e.pointer_move((215, 90));
    assert_eq!(e.document.get(id), Some(&a));
    e.pointer_up((225, 105));
    let result = e.document.get(id).unwrap();
    assert_eq!(result.points[1], point(225., 105.));
    assert_eq!(result.points[0], a.points[0]);
    assert_eq!(result.points[2], a.points[2]);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
}

#[test]
fn cancel_switched_vertex_edit_preserves_document_and_redo() {
    let mut e = editor();
    let a = line();
    let id = e.document.insert(a.clone());
    let mut extra = a.clone();
    extra.translate(300., 0.);
    e.document.insert(extra);
    e.document.undo();
    pending(&mut e);
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    e.pointer_down((200, 80), 4.);
    e.pointer_move((230, 105));
    assert_ne!(e.annotations(), vec![a.clone()]);
    assert!(e.cancel_gesture());
    assert_eq!(e.annotations(), vec![a.clone()]);
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.document.can_redo());
    assert!(!e.has_polyline());
}

#[test]
fn switched_click_or_subthreshold_motion_does_not_add_history() {
    let (mut e, id, a) = fixture();
    let revision = e.document.revision();
    e.pointer_down((140, 80), 4.);
    e.pointer_up((142, 81));
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.revision(), revision);
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}

#[test]
fn overlapping_polylines_choose_last_committed_object() {
    let mut e = editor();
    let bottom = e.document.insert(line());
    let mut top = line();
    top.style.color = 0x00aaff;
    let top_id = e.document.insert(top.clone());
    pending(&mut e);
    e.pointer_down((140, 80), 4.);
    e.pointer_up((140, 80));
    assert_eq!(e.selected, Some(top_id));
    assert_eq!(e.style, top.style);
    assert_eq!(e.document.items().len(), 2);
    assert_eq!(e.document.items()[0].id, bottom);
    assert_eq!(e.document.items()[1].id, top_id);
}

#[test]
fn later_other_tool_does_not_occlude_committed_polyline_hit() {
    let mut e = editor();
    let id = e.document.insert(line());
    let mut rectangle = Annotation::new(
        Tool::Rectangle,
        point(60., 60.),
        Style {
            filled: true,
            ..Style::default()
        },
    );
    rectangle.update(point(260., 180.));
    let rectangle_id = e.document.insert(rectangle.clone());
    pending(&mut e);
    let revision = e.document.revision();
    e.pointer_down((140, 80), 4.);
    e.pointer_up((140, 80));
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.document.get(rectangle_id), Some(&rectangle));
    assert_eq!(e.document.revision(), revision);
}

#[test]
fn rotated_polyline_vertex_switch_and_drag_preserve_world_frame() {
    let mut e = editor();
    let mut a = line();
    a.rotation = 31.;
    let id = e.document.insert(a.clone());
    pending(&mut e);
    let original = a.handles();
    let target = point(original[1].x + 21.25, original[1].y - 17.5);
    e.pointer_down_at(original[1], 4., false);
    e.pointer_up_at(target);
    let result = e.document.get(id).unwrap();
    near(result.handles()[1], target);
    near(result.handles()[0], original[0]);
    near(result.handles()[2], original[2]);
    assert_eq!(result.rotation, a.rotation);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
}

#[test]
fn smooth_polyline_switch_hits_rendered_curve_away_from_straight_chord() {
    let mut e = editor();
    let mut a = line();
    a.points = vec![point(80., 160.), point(200., 80.), point(320., 160.)];
    a.style.polyline_smooth = true;
    let mut straight = a.clone();
    straight.style.polyline_smooth = false;
    let hit = a
        .segments()
        .iter()
        .map(|(start, end)| point((start.x + end.x) / 2., (start.y + end.y) / 2.))
        .find(|p| !straight.hit(*p, 1., false) && nearest_handle(&a, *p, 4.).is_none())
        .expect("平滑夹具必须包含远离原折线段的实际曲线点");
    assert!(a.hit(hit, 1., false));
    let id = e.document.insert(a.clone());
    pending(&mut e);
    e.pointer_down_at(hit, 1., false);
    e.pointer_up_at(hit);
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.document.get(id), Some(&a));
    assert!(!e.has_polyline());
}

#[test]
fn closed_filled_polyline_switch_hits_interior_without_appending_draft() {
    let mut e = editor();
    let mut a = line();
    a.points = vec![point(80., 60.), point(240., 60.), point(160., 220.)];
    a.style.filled = true;
    let id = e.document.insert(a.clone());
    pending(&mut e);
    assert_eq!(e.hover((160, 120), 4.), Hover::Move);
    e.pointer_down((160, 120), 4.);
    e.pointer_up((160, 120));
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.items().len(), 1);
    assert!(!e.has_polyline());
}

#[test]
fn grouped_polyline_vertex_switch_moves_entire_group_with_one_undo() {
    let mut e = editor();
    let a = line();
    let line_id = e.document.insert(a.clone());
    let mut rectangle = Annotation::new(Tool::Rectangle, point(280., 40.), Style::default());
    rectangle.update(point(360., 120.));
    let rectangle_id = e.document.insert(rectangle);
    assert!(e.document.set_group(&[line_id, rectangle_id], true));
    let before = e.document.items().to_vec();
    pending(&mut e);
    let edits = e.document.edit_revision();
    assert_eq!(e.hover_at(a.handles()[0], 4.), Hover::Move);
    e.pointer_down_at(a.handles()[0], 4., false);
    assert!(matches!(e.gesture, Some(Gesture::Group { .. })));
    assert_eq!(e.active_node(), None);
    e.pointer_up((100, 95));
    assert_eq!(e.document.edit_revision(), edits + 1);
    for original in &before {
        let mut expected = original.annotation.clone();
        expected.translate(20., 15.);
        assert_eq!(e.document.get(original.id), Some(&expected));
    }
    assert_eq!(e.selected_ids(), vec![line_id, rectangle_id]);
    e.undo();
    assert_eq!(e.document.items(), before);
}

#[test]
fn missed_switch_continues_draft_and_commits_only_fixed_nodes() {
    let (mut e, id, a) = fixture();
    let edits = e.document.edit_revision();
    assert_eq!(e.hover((550, 340), 4.), Hover::Crosshair);
    e.pointer_down((550, 340), 4.);
    e.pointer_up((550, 340));
    e.pointer_move((590, 420));
    assert!(e.has_polyline());
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.finish_gesture());
    assert_eq!(e.document.items().len(), 2);
    assert_eq!(
        e.document.items()[1].annotation.points,
        vec![point(400., 300.), point(480., 300.), point(550., 340.)]
    );
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.items().len(), 1);
}

#[test]
fn shift_uses_raw_hit_before_draft_angle_constraint_and_still_snaps_misses() {
    let (mut e, id, a) = fixture();
    let hit = point(140., 80.);
    assert!(e.constrained_point(hit, true).distance(hit) > 4.);
    e.pointer_down_at(hit, 4., true);
    e.pointer_up_at(hit);
    assert_eq!(e.selected, Some(id));
    assert_eq!(e.document.get(id), Some(&a));
    assert!(!e.has_polyline());

    pending(&mut e);
    let raw = point(530., 330.);
    let snapped = e.constrained_point(raw, true);
    assert!(snapped.distance(raw) > 4.);
    e.pointer_down_at(raw, 4., true);
    e.pointer_up_at(raw);
    e.finish_gesture();
    let result = &e.document.items()[1].annotation;
    near(result.points[2], snapped);
    assert_eq!(result.points.len(), 3);
}

#[test]
fn second_double_click_press_inserts_into_selected_polyline_only_once() {
    let (mut e, id, a) = fixture();
    let edits = e.document.edit_revision();
    e.pointer_down((140, 80), 4.);
    e.pointer_up((140, 80));
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.insert_polyline_node((140, 80), 4.));
    let inserted = e.document.get(id).unwrap();
    assert_eq!(inserted.points.len(), a.points.len() + 1);
    assert_eq!(inserted.points[1], point(140., 80.));
    assert_eq!(e.active_node(), Some(1));
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.edit_revision(), edits + 1);
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
}

#[test]
fn pending_double_click_finish_preserves_only_clicked_nodes_and_one_command() {
    let mut e = editor();
    pending(&mut e);
    e.pointer_move((550, 370));
    assert!(e.insert_polyline_node((480, 300), 4.));
    assert!(!e.has_polyline());
    assert_eq!(e.tool, Tool::Polyline);
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(
        e.document.items()[0].annotation.points,
        vec![point(400., 300.), point(480., 300.)]
    );
    assert_eq!(e.document.edit_revision(), 1);
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}

#[test]
fn pending_shift_double_click_hits_old_object_before_snapping_new_draft_node() {
    let (mut e, id, a) = fixture();
    let revision = e.document.revision();
    let hit = point(140., 80.);
    assert!(e.constrained_point(hit, true).distance(hit) > 4.);
    assert!(e.insert_polyline_node_at_with_shift(hit, 4., true));
    assert_eq!(e.selected, Some(id));
    assert!(!e.has_polyline());
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.items().len(), 1);

    pending(&mut e);
    let raw = point(530.25, 330.5);
    let snapped = e.constrained_point(raw, true);
    assert!(snapped.distance(raw) > 4.);
    assert!(e.insert_polyline_node_at_with_shift(raw, 4., true));
    assert!(!e.has_polyline());
    let nodes = &e.document.items()[1].annotation.points;
    assert_eq!(nodes.len(), 3);
    near(nodes[2], snapped);
    assert_eq!(e.document.revision(), revision + 1);
    e.undo();
    assert_eq!(e.document.items().len(), 1);
    assert_eq!(e.document.get(id), Some(&a));
}

#[test]
fn shift_vertex_click_and_subthreshold_up_keep_redo_before_actual_snapped_drag() {
    let mut e = editor();
    let mut a = line();
    a.points[1] = point(200., 110.);
    let id = e.document.insert(a.clone());
    let mut extra = a.clone();
    extra.translate(300., 0.);
    e.document.insert(extra);
    e.document.undo();
    pending(&mut e);
    let revision = e.document.revision();
    let edits = e.document.edit_revision();
    let vertex = a.handles()[1];
    // 与宿主一致：Down 用真实点命中，Up 重新调用活动对象的 Shift 约束。
    e.pointer_down_at(vertex, 4., true);
    assert_eq!(e.active_node(), Some(1));
    let up = e.constrained_point(vertex, true);
    assert_eq!(up, vertex);
    e.pointer_up_at(up);
    assert_eq!(e.document.get(id), Some(&a));
    assert_eq!(e.document.revision(), revision);
    assert_eq!(e.document.edit_revision(), edits);
    assert!(e.document.can_redo());

    pending(&mut e);
    e.pointer_down_at(vertex, 4., true);
    let near_vertex = point(vertex.x + 3., vertex.y);
    let near_sample = e.constrained_point(near_vertex, true);
    assert_eq!(near_sample, near_vertex);
    e.pointer_move_at(near_sample);
    assert_eq!(e.annotations(), vec![a.clone()]);
    assert_eq!(e.document.get(id), Some(&a));
    assert!(e.document.can_redo());

    let raw_drag = point(vertex.x + 38., vertex.y + 65.);
    let snapped = e.constrained_point(raw_drag, true);
    near(snapped, snap_angle(a.handles()[0], raw_drag));
    assert!(snapped.distance(vertex) >= 4.);
    assert!(snapped.distance(raw_drag) > 4.);
    e.pointer_move_at(snapped);
    assert_eq!(e.document.get(id), Some(&a));
    e.pointer_up_at(snapped);
    let result = e.document.get(id).unwrap();
    near(result.handles()[1], snapped);
    near(result.handles()[0], a.handles()[0]);
    near(result.handles()[2], a.handles()[2]);
    assert_eq!(e.document.edit_revision(), edits + 1);
    assert!(!e.document.can_redo());
    e.undo();
    assert_eq!(e.document.get(id), Some(&a));
    e.undo();
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
}
