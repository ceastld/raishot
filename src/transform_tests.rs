//! 旋转、组合、撤销和效果世界坐标的验收。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::Document,
    editor::Editor,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
    selection_transform::{self, Manipulation},
    transform,
};
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn mark(tool: Tool, a: Point, b: Point) -> Annotation {
    let mut m = Annotation::new(tool, a, Style::default());
    m.update(b);
    m
}
fn editor() -> Editor {
    Editor::new(Rect::full(640, 480), Some(Rect::full(640, 480)))
}
fn draw(e: &mut Editor, a: (i32, i32), b: (i32, i32)) {
    e.select_tool(Tool::Rectangle);
    e.pointer_down(a, 6.);
    e.pointer_up(b);
}
#[test]
fn rigid_rotation_preserves_source_points_and_inverse_hit_is_oriented() {
    let mut a = mark(Tool::Rectangle, p(100., 100.), p(220., 140.));
    let raw = a.points.clone();
    let center = transform::center(a.local_bounds());
    a.rotate_about(center, 45.);
    assert_eq!(a.points, raw);
    let q = a.to_world(p(110., 110.));
    assert!(a.hit(q, 0., true));
    assert!(a.to_local(q).distance(p(110., 110.)) < 0.001);
    let b = a.bounds();
    assert!(!a.hit(p(b.x + 2., b.y + 2.), 0., true));
    let mut ellipse = mark(Tool::Ellipse, p(100., 100.), p(220., 140.));
    ellipse.rotate_about(center, 45.);
    assert!(ellipse.hit(center, 0., true));
    assert!(!ellipse.hit(ellipse.to_world(p(103., 103.)), 0., true));
    a.rotate_about(center, -45.);
    assert_eq!(a.rotation, 0.);
    assert_eq!(a.points, raw);
}
#[test]
fn rotated_pencil_retains_exact_protected_corner() {
    let mut a = mark(Tool::Pen, p(100., 100.), p(200., 100.));
    a.update(p(200., 200.));
    let raw = a.points.clone();
    let pivot = transform::center(a.local_bounds());
    a.rotate_about(pivot, 37.);
    let corner = a.to_world(raw[1]);
    assert!(a
        .vector_path()
        .spans
        .iter()
        .any(|s| s.end().distance(corner) < 0.001));
    assert_eq!(a.points, raw);
    assert!(a.hit(corner, 0., false));
}
#[test]
fn group_membership_duplication_delete_and_regroup_undo_preserve_identity() {
    let mut d = Document::default();
    let first = d.insert(mark(Tool::Rectangle, p(50., 50.), p(100., 100.)));
    let second = d.insert(mark(Tool::Ellipse, p(150., 50.), p(200., 100.)));
    assert!(d.set_group(&[first, second], true));
    assert_eq!(d.expand_groups(&[first]), vec![first, second]);
    assert!(!d.set_group(&[first], true));
    let copies = d.duplicate(&d.expand_groups(&[first]), Rect::full(640, 480));
    assert_eq!(d.expand_groups(&[copies[0]]), copies);
    assert_ne!(d.items()[0].group, d.items()[2].group);
    d.undo();
    assert_eq!(d.items().len(), 2);
    d.redo();
    assert_eq!(d.items().len(), 4);
    assert!(d.set_group(&[copies[0]], false));
    assert_eq!(d.expand_groups(&[copies[0]]), vec![copies[0]]);
    d.undo();
    assert_eq!(d.expand_groups(&[copies[0]]), copies);
    d.remove_many(&d.expand_groups(&[first]));
    assert_eq!(d.items().len(), 2);
    d.undo();
    assert_eq!(d.expand_groups(&[first]), vec![first, second]);
}
#[test]
fn selecting_group_member_moves_all_and_scaling_is_one_transaction() {
    let mut e = editor();
    draw(&mut e, (50, 60), (100, 110));
    draw(&mut e, (150, 60), (200, 110));
    e.select_all_objects();
    e.group_selection(true);
    let original = e.document.items().to_vec();
    e.select_tool(Tool::Edit);
    e.pointer_down((70, 80), 6.);
    e.pointer_up((90, 90));
    assert_eq!(e.selected_ids().len(), 2);
    assert_eq!(e.document.items()[0].annotation.bounds().x, 70.);
    assert_eq!(e.document.items()[1].annotation.bounds().x, 170.);
    e.undo();
    assert_eq!(e.document.items(), original);
    e.selected = Some(original[0].id);
    let b = e.selection_bounds().unwrap();
    let handle = b.handles()[7];
    e.pointer_down((handle.x as i32, handle.y as i32), 6.);
    e.pointer_up((350, 160));
    let changed = e.document.items()[0].annotation.bounds();
    assert!((changed.width - 100.).abs() < 0.001);
    e.undo();
    assert_eq!(e.document.items(), original);
}
#[test]
fn rotation_drag_is_one_undo_cancel_keeps_redo_and_shift_snaps() {
    let mut e = editor();
    draw(&mut e, (100, 100), (220, 140));
    let id = e.selected.unwrap();
    let original = e.document.get(id).unwrap().clone();
    let knob = e.rotation_handle(6.).unwrap();
    let pivot = transform::center(e.selection_bounds().unwrap());
    e.pointer_down((knob.x as i32, knob.y as i32), 6.);
    let q = transform::Rotation::new(pivot, 45.).map(knob);
    let q = e.constrained_pointer((q.x.round() as i32, q.y.round() as i32), true);
    e.pointer_move(q);
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.pointer_up(q);
    assert!((e.document.get(id).unwrap().rotation - 45.).abs() < 1.);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    assert!(e.document.can_redo());
    e.selected = Some(id);
    e.pointer_down((knob.x as i32, knob.y as i32), 6.);
    e.pointer_move(q);
    e.cancel_gesture();
    assert_eq!(e.document.get(id).unwrap(), &original);
    assert!(e.document.can_redo());
}
#[test]
fn rotated_vertex_and_shape_resize_keep_other_anchors_fixed() {
    let mut e = editor();
    let mut a = mark(Tool::Polyline, p(100., 100.), p(200., 200.));
    a.update(p(300., 100.));
    a.rotate_about(transform::center(a.local_bounds()), 90.);
    let id = e.document.insert(a.clone());
    e.tool = Tool::Edit;
    e.selected = Some(id);
    let handles = a.handles();
    e.pointer_down(
        (handles[1].x.round() as i32, handles[1].y.round() as i32),
        6.,
    );
    e.pointer_up((handles[1].x as i32 + 30, handles[1].y as i32));
    let changed = e.document.get(id).unwrap();
    assert!(changed.handles()[0].distance(handles[0]) < 0.001);
    assert!(changed.handles()[2].distance(handles[2]) < 0.001);
    e.undo();
    let mut rect = mark(Tool::Rectangle, p(100., 100.), p(200., 150.));
    rect.rotate_about(transform::center(rect.local_bounds()), 30.);
    let id = e.document.insert(rect.clone());
    e.selected = Some(id);
    let h = rect.handles();
    let next = rect.to_world(p(230., 180.));
    e.pointer_down((h[7].x.round() as i32, h[7].y.round() as i32), 6.);
    e.pointer_up((next.x.round() as i32, next.y.round() as i32));
    assert!(e.document.get(id).unwrap().handles()[0].distance(h[0]) < 0.01);
}
#[test]
fn rotated_masks_text_and_region_export_use_the_same_world_geometry() {
    let canvas = Rect::full(320, 300);
    let pixels: Vec<_> = (0..320 * 300)
        .flat_map(|i| [(i % 250) as u8, 180, 70, 255])
        .collect();
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    for tool in [
        Tool::Mosaic,
        Tool::Blur,
        Tool::Spotlight,
        Tool::HighlighterRect,
        Tool::Rectangle,
        Tool::Text,
        Tool::Serial,
        Tool::Watermark,
    ] {
        let mut a = mark(tool, p(70., 70.), p(240., 150.));
        a.style.filled = true;
        a.text = "Rust 中文".into();
        a.rotate_about(transform::center(a.local_bounds()), 37.);
        let scene = Scene::prepare(&[a.clone()], &source);
        assert!(
            scene.errors.is_empty(),
            "{}: {:?}",
            tool.label(),
            scene.errors
        );
        if tool == Tool::Spotlight {
            assert!(matches!(
                scene.layers[0],
                crate::scene::Layer::Spotlight { .. }
            ));
        }
        let mut full = source.read(canvas);
        scene.apply(&mut full, canvas);
        let crop = Rect {
            x: 50,
            y: 50,
            width: 190,
            height: 180,
        };
        let mut part = source.read(crop);
        scene.apply(&mut part, crop);
        for y in 0..crop.height {
            let i = ((y + crop.y) * canvas.width + crop.x) as usize * 4;
            assert_eq!(
                &part.bgra[(y * crop.width) as usize * 4..((y + 1) * crop.width) as usize * 4],
                &full.bgra[i..i + crop.width as usize * 4],
                "{}",
                tool.label()
            );
        }
        assert_ne!(
            full.bgra,
            pixels,
            "{} rotation rendered no content",
            tool.label()
        );
        if matches!(tool, Tool::Mosaic | Tool::Blur) {
            let b = a.bounds();
            let x = (b.x + 1.) as u32;
            let y = (b.y + 1.) as u32;
            let i = ((y * canvas.width + x) * 4) as usize;
            assert_eq!(&full.bgra[i..i + 4], &pixels[i..i + 4]);
        }
    }
}
#[test]
fn edit_rotated_text_preserves_angle_and_top_left_world_anchor() {
    let mut e = editor();
    let mut a = mark(Tool::Text, p(100., 100.), p(300., 150.));
    a.text = "原文字".into();
    a.rotate_about(transform::center(a.local_bounds()), 40.);
    let anchor = a.to_world(a.points[0]);
    let id = e.document.insert(a.clone());
    e.selected = Some(id);
    e.edit_text();
    e.set_text("修改后\n中文🙂".into());
    e.finish_text();
    let b = e.document.get(id).unwrap();
    assert_eq!(b.rotation, 40.);
    assert!(b.to_world(b.points[0]).distance(anchor) < 0.001);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &a);
}
#[test]
fn group_metadata_only_change_reuses_rendered_effect_cache() {
    let canvas = Rect::full(320, 240);
    let pixels = vec![255; 320 * 240 * 4];
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let mut e = Editor::new(canvas, Some(canvas));
    e.document
        .insert(mark(Tool::Mosaic, p(30., 30.), p(80., 80.)));
    e.document
        .insert(mark(Tool::Rectangle, p(100., 30.), p(160., 80.)));
    let before = Scene::prepare(&e.annotations(), &source);
    e.select_all_objects();
    e.group_selection(true);
    let after = before.updated(&e.annotations(), &source);
    let (crate::scene::Layer::Mosaic { blocks: a }, crate::scene::Layer::Mosaic { blocks: b }) =
        (&before.layers[0], &after.layers[0])
    else {
        panic!()
    };
    assert!(std::sync::Arc::ptr_eq(a, b));
}
#[test]
fn group_transform_preview_preserves_relative_centers_and_styles() {
    let before = vec![
        (1, mark(Tool::Rectangle, p(60., 60.), p(120., 90.))),
        (2, mark(Tool::Text, p(180., 80.), p(280., 120.))),
    ];
    let mut current = before.clone();
    let bounds = selection_transform::bounds(&before);
    let pivot = transform::center(bounds);
    let start = p(pivot.x, pivot.y - 60.);
    let end = p(pivot.x + 60., pivot.y);
    selection_transform::preview(
        &before,
        &mut current,
        start,
        end,
        Manipulation::Rotate { pivot, angle: -90. },
    );
    let old = transform::center(before[0].1.local_bounds())
        .distance(transform::center(before[1].1.local_bounds()));
    let next = transform::center(current[0].1.local_bounds())
        .distance(transform::center(current[1].1.local_bounds()));
    assert!((old - next).abs() < 0.001);
    assert_eq!(current[0].1.rotation, 90.);
    assert_eq!(current[1].1.rotation, 90.);
}

#[test]
fn repeated_group_scale_events_do_not_accumulate_style_and_wheel_stays_atomic() {
    let mut e = editor();
    draw(&mut e, (50, 60), (100, 110));
    draw(&mut e, (150, 60), (200, 110));
    e.select_all_objects();
    e.group_selection(true);
    let before = e.document.items().to_vec();
    e.pointer_down((200, 110), 6.);
    for q in [(275, 135), (350, 160), (350, 160), (350, 160)] {
        e.pointer_move(q);
    }
    assert_eq!(e.selected_annotation().unwrap().style.width, 8.);
    assert_eq!(e.style.width, 8.);
    e.adjust_parameter(crate::interaction::Parameter::Width, 1, 10, (350, 160), 6.);
    e.pointer_move((350, 160));
    e.pointer_up((350, 160));
    assert_eq!(e.document.items()[0].annotation.style.width, 9.);
    assert_eq!(e.document.items()[1].annotation.style.width, 9.);
    assert_eq!(
        e.document.items()[0].annotation.style.font_size,
        before[0].annotation.style.font_size
    );
    assert_eq!(e.document.items()[0].annotation.local_bounds().width, 100.);
    e.undo();
    assert_eq!(e.document.items(), before);
}

#[test]
fn group_scale_can_exceed_the_canvas_and_repeated_text_samples_keep_font_size() {
    let before = vec![
        (1, mark(Tool::Rectangle, p(30., 40.), p(100., 80.))),
        (2, mark(Tool::Text, p(140., 60.), p(300., 110.))),
    ];
    let bounds = selection_transform::bounds(&before);
    let start = bounds.handles()[7];
    let mode = Manipulation::Scale { bounds, handle: 7 };
    let mut current = before.clone();
    for _ in 0..10 {
        selection_transform::preview(&before, &mut current, start, p(99999., 99999.), mode);
    }
    let b = selection_transform::bounds(&current);
    assert!((b.width - 270. * 64.).abs() < 0.001);
    assert!((b.height - 70. * 64.).abs() < 0.001);
    assert!((current[1].1.style.font_size - 28. * 64.).abs() < 0.001);
}

#[test]
fn font_wheel_during_large_group_scale_keeps_value_and_one_undo() {
    let mut e = editor();
    e.document
        .insert(mark(Tool::Rectangle, p(30., 60.), p(80., 100.)));
    let mut text = mark(Tool::Text, p(130., 60.), p(200., 100.));
    text.text = "字号".into();
    e.document.insert(text);
    e.select_tool(Tool::Edit);
    e.select_all_objects();
    let before = e.document.items().to_vec();
    e.pointer_down((200, 100), 6.);
    e.pointer_move((540, 180));
    assert_eq!(e.selected_annotation().unwrap().style.font_size, 84.);
    assert!(!e.adjust_parameter(crate::interaction::Parameter::Font, 1, 1, (540, 180), 6.));
    assert_eq!(e.selected_annotation().unwrap().style.font_size, 84.);
    e.adjust_parameter(crate::interaction::Parameter::Font, -1, 1, (540, 180), 6.);
    e.pointer_move((540, 180));
    e.pointer_up((540, 180));
    assert!((e.document.items()[1].annotation.style.font_size - 83.).abs() < 0.001);
    e.undo();
    assert_eq!(e.document.items(), before);
}

#[test]
fn rotation_handle_tracks_oriented_frame_and_zoom_for_shapes_and_paths() {
    for tool in [
        Tool::Rectangle,
        Tool::Ellipse,
        Tool::Pen,
        Tool::Arrow,
        Tool::Polyline,
        Tool::Text,
    ] {
        let mut e = editor();
        let mut a = mark(tool, p(200., 180.), p(350., 280.));
        if tool == Tool::Pen || tool == Tool::Polyline {
            a.update(p(240., 310.));
        }
        if tool == Tool::Text {
            a.text = "旋转".into();
        }
        let id = e.document.insert(a);
        e.selected = Some(id);
        for angle in [0., 30., 90., 135., -180., -90.] {
            for tolerance in [1.5, 6., 9.] {
                let mut a = e.document.get(id).unwrap().clone();
                a.rotation = angle;
                e.document.replace(id, a.clone());
                let b = a.local_bounds();
                let control = e.rotation_control(tolerance).unwrap();
                let anchor = p(b.x + b.width / 2., b.y);
                assert!(control.anchor.distance(a.to_world(anchor)) < 0.001);
                assert!(
                    control
                        .position
                        .distance(a.to_world(p(anchor.x, anchor.y - 4. * tolerance)))
                        < 0.001
                );
                assert!(control.pivot.distance(transform::center(b)) < 0.001);
                assert!((control.anchor.distance(control.position) - 4. * tolerance).abs() < 0.001);
            }
        }
    }
}
#[test]
fn rotation_handle_follows_live_preview_and_can_drag_again_after_commit() {
    let mut e = editor();
    draw(&mut e, (180, 180), (340, 260));
    let id = e.selected.unwrap();
    let original = e.document.get(id).unwrap().clone();
    for (delta, expected) in [(45., 45.), (45., 90.), (90., -180.)] {
        let c = e.rotation_control(6.).unwrap();
        let next = transform::Rotation::new(c.pivot, delta).map(c.position);
        e.pointer_down_at(c.position, 6., false);
        e.pointer_move_at(next);
        let live = e.rotation_control(6.).unwrap();
        assert!(live.position.distance(next) < 0.001);
        assert!((e.selected_annotation().unwrap().rotation - expected).abs() < 0.001);
        e.pointer_up_at(next);
        assert!(e.rotation_handle(6.).unwrap().distance(next) < 0.001);
    }
    e.undo();
    e.undo();
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.selected = Some(id);
    let c = e.rotation_control(6.).unwrap();
    e.pointer_down_at(c.position, 6., false);
    e.pointer_move_at(transform::Rotation::new(c.pivot, 30.).map(c.position));
    assert!(e.cancel_gesture());
    assert!(e.document.can_redo());
    assert!(e.rotation_handle(6.).unwrap().distance(c.position) < 0.001);
}
#[test]
fn rotation_handle_stays_clickable_near_each_canvas_edge() {
    for (a, b, angle) in [
        (p(200., 2.), p(320., 82.), 0.),
        (p(538., 160.), p(638., 260.), 90.),
        (p(200., 398.), p(320., 478.), -180.),
        (p(2., 160.), p(102., 260.), -90.),
    ] {
        let mut e = editor();
        let mut a = mark(Tool::Rectangle, a, b);
        a.rotation = angle;
        let id = e.document.insert(a.clone());
        e.selected = Some(id);
        e.tool = Tool::Edit;
        let c = e.rotation_control(6.).unwrap();
        assert!(
            c.position.x >= 6.
                && c.position.x <= 634.
                && c.position.y >= 6.
                && c.position.y <= 474.
        );
        assert!(a.handles().iter().any(|p| p.distance(c.anchor) < 0.001));
        e.pointer_down_at(c.position, 6., false);
        e.pointer_up_at(transform::Rotation::new(c.pivot, 15.).map(c.position));
        assert!((e.document.get(id).unwrap().rotation - (angle + 15.)).abs() < 0.001);
    }
}
