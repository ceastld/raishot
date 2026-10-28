//! 圆角拖柄的对象事务回归；只使用纯模型，不创建窗口或 GPU。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    interaction::{Hover, Parameter},
    tool_presets::default_style,
};

fn shape(tool: Tool, radius: f32) -> Annotation {
    let mut a = Annotation::new(
        tool,
        Point { x: 100., y: 120. },
        Style {
            radius,
            ..default_style(tool)
        },
    );
    a.update(Point { x: 400., y: 300. });
    a
}
fn editor(a: Annotation) -> (Editor, ObjectId) {
    let canvas = Rect::full(900, 700);
    let mut e = Editor::new(canvas, Some(canvas));
    let id = e.document.insert(a);
    select(&mut e, id);
    (e, id)
}
fn select(e: &mut Editor, id: ObjectId) {
    e.select_tool(Tool::Edit);
    e.selected = Some(id);
    e.style = e.document.get(id).unwrap().style;
}
fn near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.001, "{actual} != {expected}");
}
// 将世界起点沿它所属角的局部向内对角线移动指定物理距离。
fn inward(a: &Annotation, start: Point, distance: f32) -> Point {
    let local = a.to_local(start);
    let center = crate::transform::center(a.local_bounds());
    let step = distance / std::f32::consts::SQRT_2;
    a.to_world(Point {
        x: local.x + if local.x < center.x { step } else { -step },
        y: local.y + if local.y < center.y { step } else { -step },
    })
}

#[test]
fn radius_bulk_core_highlight_rotated_drag_is_one_transaction_and_preserves_defaults() {
    let mut original = shape(Tool::HighlighterRect, 12.);
    original.rotation = 37.;
    let (mut e, id) = editor(original.clone());
    e.restore_tool_styles([(
        Tool::Rectangle,
        Style {
            radius: 33.,
            ..default_style(Tool::Rectangle)
        },
    )]);
    select(&mut e, id);
    let revision = e.document.edit_revision();
    let start = e.radius_controls(6.)[1].position;
    let end = inward(&original, start, 15.);
    e.pointer_down_at(start, 6., false);
    assert!(e.is_dragging_radius());
    e.pointer_move_at(end);
    near(e.style.radius, 27.);
    assert_eq!(e.document.get(id).unwrap(), &original);
    assert_eq!(e.document.edit_revision(), revision);
    e.pointer_up_at(end);
    let changed = e.document.get(id).unwrap().clone();
    near(changed.style.radius, 27.);
    assert_eq!(changed.points, original.points);
    assert_eq!(changed.rotation, original.rotation);
    assert_eq!(changed.style.opacity, original.style.opacity);
    assert_eq!(e.document.edit_revision(), revision + 1);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.redo();
    assert_eq!(e.document.get(id).unwrap(), &changed);
    e.select_tool(Tool::HighlighterRect);
    // 高亮框的对象圆角不发布为下一笔默认值。
    near(e.style.radius, default_style(Tool::HighlighterRect).radius);
    e.select_tool(Tool::Rectangle);
    near(e.style.radius, 33.);
}

#[test]
fn highlight_radius_cancel_and_unchanged_drag_preserve_redo() {
    for cancel in [false, true] {
        let (mut e, id) = editor(shape(Tool::HighlighterRect, 17.));
        let second = e.document.insert(shape(Tool::Ellipse, 0.));
        e.document.undo();
        select(&mut e, id);
        let original = e.document.get(id).unwrap().clone();
        let revision = e.document.edit_revision();
        let start = e.radius_controls(6.)[3].position;
        e.pointer_down_at(start, 6., false);
        if cancel {
            e.pointer_move_at(inward(&original, start, 13.));
            assert!(e.cancel_gesture());
        } else {
            e.pointer_up_at(start);
        }
        assert_eq!(e.document.get(id).unwrap(), &original);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.can_redo());
        e.redo();
        assert!(e.document.get(second).is_some());
    }
}

#[test]
fn highlight_radius_transparent_corners_do_not_hit_rotated_filled_shapes() {
    for tool in [Tool::HighlighterRect, Tool::Rectangle, Tool::Spotlight] {
        for angle in [0., 31., 90.] {
            let mut a = shape(tool, 40.);
            a.style.width = 2.;
            a.style.filled = true;
            a.rotation = angle;
            let b = a.local_bounds();
            assert!(!a.hit(
                a.to_world(Point {
                    x: b.x + 1.,
                    y: b.y + 1.
                }),
                0.,
                true
            ));
            assert!(a.hit(
                a.to_world(Point {
                    x: b.x + 40.,
                    y: b.y + 1.
                }),
                0.,
                true
            ));
            assert!(a.hit(a.to_world(crate::transform::center(b)), 0., true));
        }
    }
}

#[test]
fn radius_four_corners_drag_zero_without_jump_and_commit_once() {
    for corner in 0..4 {
        let (mut e, id) = editor(shape(Tool::Rectangle, 0.));
        let original = e.document.get(id).unwrap().clone();
        let control = e.radius_controls(6.)[corner];
        let start = Point {
            x: control.position.x + 1.,
            y: control.position.y - 1.,
        };
        let revision = e.document.edit_revision();
        let diagonal = if corner % 2 == 0 {
            Hover::DiagonalDown
        } else {
            Hover::DiagonalUp
        };
        assert_eq!(e.hover_at(control.position, 6.), diagonal);
        e.pointer_down_at(start, 6., false);
        assert!(e.is_dragging_radius());
        near(e.selected_annotation().unwrap().style.radius, 0.);
        let end = inward(&original, start, 20.);
        e.pointer_move_at(end);
        near(e.selected_annotation().unwrap().style.radius, 20.);
        near(e.style.radius, 20.);
        assert_eq!(e.document.get(id).unwrap(), &original);
        assert_eq!(e.document.edit_revision(), revision);
        e.pointer_up_at(end);
        assert!(!e.is_dragging_radius());
        near(e.document.get(id).unwrap().style.radius, 20.);
        assert_eq!(e.document.edit_revision(), revision + 1);
        e.undo();
        assert_eq!(e.document.get(id).unwrap(), &original);
        e.redo();
        near(e.document.get(id).unwrap().style.radius, 20.);
        e.select_tool(Tool::Rectangle);
        near(e.style.radius, 20.);
    }
}

#[test]
fn radius_rotated_corners_use_local_direction_and_leave_geometry_unchanged() {
    for corner in 0..4 {
        let mut original = shape(Tool::Rectangle, 12.);
        original.rotation = 57.;
        let (mut e, id) = editor(original.clone());
        let start = e.radius_controls(6.)[corner].position;
        let end = inward(&original, start, 15.);
        e.pointer_down_at(start, 6., false);
        assert!(e.is_dragging_radius());
        assert_eq!(e.constrained_point(end, true), end);
        e.pointer_up_at(end);
        let changed = e.document.get(id).unwrap();
        near(changed.style.radius, 27.);
        assert_eq!(changed.points, original.points);
        assert_eq!(changed.rotation, original.rotation);
        assert_eq!(changed.style.width, original.style.width);
        e.undo();
        assert_eq!(e.document.get(id).unwrap(), &original);
    }
}

#[test]
fn radius_click_cancel_and_return_to_origin_preserve_history_and_redo() {
    for finish in ["click", "cancel", "return"] {
        let (mut e, id) = editor(shape(Tool::Rectangle, 8.));
        let second = e.document.insert(shape(Tool::Ellipse, 0.));
        e.document.undo();
        select(&mut e, id);
        let original = e.document.get(id).unwrap().clone();
        let revision = e.document.edit_revision();
        let start = e.radius_controls(6.)[0].position;
        e.pointer_down_at(start, 6., false);
        assert!(e.is_dragging_radius());
        if finish != "click" {
            e.pointer_move_at(inward(&original, start, 19.));
            near(e.style.radius, 27.);
        }
        if finish == "cancel" {
            assert!(e.cancel_gesture());
        } else {
            e.pointer_up_at(start);
        }
        assert_eq!(e.document.get(id).unwrap(), &original);
        near(e.style.radius, 8.);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.can_redo());
        e.redo();
        assert!(e.document.get(second).is_some());
    }
}

#[test]
fn radius_spotlight_drag_changes_only_one_hole_and_keeps_other_tool_preset() {
    let (mut e, id) = editor(shape(Tool::Spotlight, 8.));
    let mut other = shape(Tool::Spotlight, 24.);
    other.translate(350., 230.);
    let other_id = e.document.insert(other.clone());
    e.restore_tool_styles([(
        Tool::Rectangle,
        Style {
            radius: 33.,
            ..default_style(Tool::Rectangle)
        },
    )]);
    select(&mut e, id);
    let original = e.document.get(id).unwrap().clone();
    let start = e.radius_controls(6.)[2].position;
    let end = inward(&original, start, 14.);
    e.pointer_down_at(start, 6., false);
    e.pointer_move_at(end);
    assert_eq!(e.document.get(id).unwrap(), &original);
    assert_eq!(e.annotations()[1], other);
    e.pointer_up_at(end);
    let changed = e.document.get(id).unwrap();
    near(changed.style.radius, 22.);
    assert_eq!(changed.style.color, original.style.color);
    assert_eq!(changed.style.width, original.style.width);
    assert_eq!(changed.style.opacity, original.style.opacity);
    assert_eq!(changed.points, original.points);
    assert_eq!(e.document.get(other_id).unwrap(), &other);
    e.select_tool(Tool::Spotlight);
    near(e.style.radius, 22.);
    e.select_tool(Tool::Rectangle);
    near(e.style.radius, 33.);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    assert_eq!(e.document.get(other_id).unwrap(), &other);
    e.redo();
    near(e.document.get(id).unwrap().style.radius, 22.);
}

#[test]
fn radius_small_shape_resize_and_multiselection_keep_existing_priority() {
    let mut small = shape(Tool::Rectangle, 0.);
    small.points[1] = Point { x: 108., y: 128. };
    let (mut e, id) = editor(small.clone());
    assert!(e.radius_controls(6.).is_empty());
    let corner = small.handles()[7];
    e.pointer_down_at(corner, 6., false);
    assert!(!e.is_dragging_radius());
    e.pointer_up_at(Point { x: 140., y: 160. });
    assert!(e.document.get(id).unwrap().local_bounds().width > 8.);
    near(e.document.get(id).unwrap().style.radius, 0.);

    let (mut e, id) = editor(shape(Tool::Rectangle, 8.));
    let other = e.document.insert(shape(Tool::Spotlight, 20.));
    e.selection = vec![id, other];
    e.selected = Some(other);
    assert!(e.radius_controls(6.).is_empty());
    e.group_selection(true);
    e.selection.clear();
    e.selected = Some(id);
    assert!(e.radius_controls(6.).is_empty());
}

#[test]
fn radius_midgesture_wheel_keeps_baseline_and_commit_is_one_transaction() {
    let (mut e, id) = editor(shape(Tool::Spotlight, 8.));
    let original = e.document.get(id).unwrap().clone();
    let start = e.radius_controls(6.)[0].position;
    let end = inward(&original, start, 12.);
    let revision = e.document.edit_revision();
    e.pointer_down_at(start, 6., false);
    e.pointer_move_at(end);
    near(e.style.radius, 20.);
    let mut width_only = e.style;
    width_only.width = 3.;
    e.set_style(width_only);
    e.pointer_move_at(end);
    near(e.style.radius, 20.);
    near(e.selected_annotation().unwrap().style.width, 3.);
    assert!(e.adjust_parameter(Parameter::Radius, 3, 99, (0, 0), 6.));
    near(e.selected_annotation().unwrap().style.radius, 23.);
    e.pointer_move_at(end);
    near(e.selected_annotation().unwrap().style.radius, 23.);
    assert_eq!(e.document.edit_revision(), revision);
    e.pointer_up_at(end);
    assert_eq!(e.document.edit_revision(), revision + 1);
    near(e.document.get(id).unwrap().style.radius, 23.);
    e.undo();
    assert_eq!(e.document.get(id).unwrap(), &original);
    e.redo();
    near(e.document.get(id).unwrap().style.radius, 23.);
}

#[test]
fn radius_midgesture_wheel_at_clamps_reanchors_without_rebound_or_early_preset() {
    let cases = [
        // 向内拖后轮调到零；同点不能回弹到旧鼠标增量。
        (8., 12., -100, 0., 3., 3.),
        // 向外触底后反向轮调；同点不能再次消耗已被限幅的鼠标位移。
        (8., -12., 10, 10., 3., 13.),
        // 已缩放的大半径向外不增加，轮调后仍从当前合法大值向内回调。
        (80., 12., -1, 79., -3., 76.),
        // 首段微动不改圆角，但轮调后的重锚必须使用真实指针投影。
        (8., 2., 1, 9., 3., 12.),
    ];
    for (corner, (initial, mouse, wheel, expected, next_mouse, next_expected)) in
        cases.into_iter().enumerate()
    {
        let (mut e, id) = editor(shape(Tool::Spotlight, initial));
        let original = e.document.get(id).unwrap().clone();
        let revision = e.document.edit_revision();
        let presets = e.tool_styles().collect::<std::collections::HashMap<_, _>>();
        let start = e.radius_controls(6.)[corner].position;
        let point = inward(&original, start, mouse);
        e.pointer_down_at(start, 6., false);
        e.pointer_move_at(point);
        assert!(e.adjust_parameter(Parameter::Radius, wheel, 100, (0, 0), 6.));
        near(e.style.radius, expected);
        for _ in 0..2 {
            e.pointer_move_at(point);
            near(e.style.radius, expected);
            near(e.selected_annotation().unwrap().style.radius, expected);
        }
        e.pointer_move_at(inward(&original, start, mouse + next_mouse));
        near(e.style.radius, next_expected);
        assert_eq!(e.document.get(id).unwrap(), &original);
        assert_eq!(e.document.edit_revision(), revision);
        assert_eq!(
            e.tool_styles().collect::<std::collections::HashMap<_, _>>(),
            presets
        );
        assert!(e.cancel_gesture());
        near(e.style.radius, initial);
        assert_eq!(e.document.get(id).unwrap(), &original);
        assert_eq!(e.document.edit_revision(), revision);
        assert_eq!(
            e.tool_styles().collect::<std::collections::HashMap<_, _>>(),
            presets
        );
    }
}

#[test]
fn radius_scaled_values_move_inward_without_jump_and_drawing_stays_a_draft() {
    let (mut e, id) = editor(shape(Tool::Rectangle, 80.));
    let original = e.document.get(id).unwrap().clone();
    let start = e.radius_controls(6.)[0].position;
    e.pointer_down_at(start, 6., false);
    e.pointer_move_at(inward(&original, start, 12.));
    near(e.style.radius, 80.);
    e.pointer_move_at(inward(&original, start, -10.));
    near(e.style.radius, 70.);
    e.cancel_gesture();
    near(e.style.radius, 80.);
    assert_eq!(e.document.get(id).unwrap(), &original);

    let canvas = Rect::full(900, 700);
    let mut e = Editor::new(canvas, Some(canvas));
    e.select_tool(Tool::Spotlight);
    e.pointer_down((120, 120), 6.);
    e.pointer_move((360, 300));
    assert!(e.radius_controls(6.).is_empty());
    assert!(e.adjust_parameter(Parameter::Radius, 17, 10, (0, 0), 6.));
    assert!(e.document.items().is_empty());
    assert!(!e.document.can_undo());
    e.pointer_up((360, 300));
    near(e.document.items()[0].annotation.style.radius, 17.);
    e.undo();
    assert!(e.document.items().is_empty());
}

fn ordinary_effect(tool: Tool, radius: f32) -> Annotation {
    let mut a = shape(tool, 8.);
    a.style.effect_radius = radius;
    a
}

#[test]
fn effect_radius_rotated_drag_and_midgesture_wheel_commit_one_command() {
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut original = ordinary_effect(tool, 12.);
        original.rotation = 37.;
        let (mut e, id) = editor(original.clone());
        let defaults: std::collections::HashMap<_, _> = e.tool_styles().collect();
        let revision = e.document.edit_revision();
        let start = e.radius_controls(6.)[1].position;
        let end = inward(&original, start, 15.);
        e.pointer_down_at(start, 6., false);
        assert!(e.is_dragging_radius());
        e.pointer_move_at(end);
        near(e.style.effect_radius, 27.);
        assert_eq!(e.style.radius, 8.);
        assert_eq!(e.document.get(id).unwrap(), &original);
        assert!(e.adjust_parameter(Parameter::Radius, 2, 71, (0, 0), 6.));
        near(e.style.effect_radius, 29.);
        // 数值变化以当前指针重锚，同一点不再叠加原拖动增量。
        e.pointer_move_at(end);
        near(e.style.effect_radius, 29.);
        assert_eq!(e.document.edit_revision(), revision);
        e.pointer_up_at(end);
        let changed = e.document.get(id).unwrap().clone();
        near(changed.style.effect_radius, 29.);
        assert_eq!(changed.style.radius, original.style.radius);
        assert_eq!(changed.points, original.points);
        assert_eq!(changed.rotation, original.rotation);
        assert_eq!(changed.style.block_size, original.style.block_size);
        assert_eq!(e.document.edit_revision(), revision + 1);
        assert_eq!(
            e.tool_styles().collect::<std::collections::HashMap<_, _>>(),
            defaults
        );
        e.undo();
        assert_eq!(e.document.get(id).unwrap(), &original);
        e.redo();
        assert_eq!(e.document.get(id).unwrap(), &changed);
    }
}

#[test]
fn effect_radius_cancel_and_noop_preserve_redo_and_legacy_radius() {
    for cancel in [false, true] {
        let (mut e, id) = editor(ordinary_effect(Tool::Blur, 17.));
        let extra = e.document.insert(shape(Tool::Ellipse, 0.));
        e.document.undo();
        select(&mut e, id);
        let original = e.document.get(id).unwrap().clone();
        let revision = e.document.edit_revision();
        let start = e.radius_controls(6.)[3].position;
        e.pointer_down_at(start, 6., false);
        if cancel {
            e.pointer_move_at(inward(&original, start, 13.));
            assert!(e.cancel_gesture());
        } else {
            e.pointer_up_at(start);
        }
        assert_eq!(e.document.get(id).unwrap(), &original);
        assert_eq!(e.document.edit_revision(), revision);
        assert_eq!(e.style.effect_radius, 17.);
        assert_eq!(e.style.radius, 8.);
        assert!(e.document.can_redo());
        e.redo();
        assert!(e.document.get(extra).is_some());
    }
}

#[test]
fn effect_radius_compound_object_has_no_controls_or_parameter_target() {
    for tool in [Tool::Mosaic, Tool::Blur] {
        let mut a = ordinary_effect(tool, 17.);
        a.set_effect_regions(
            vec![crate::annotation::Bounds {
                x: 100.,
                y: 120.,
                width: 300.,
                height: 180.,
            }],
            true,
        )
        .unwrap();
        let (mut e, id) = editor(a.clone());
        let revision = e.document.edit_revision();
        assert!(!a.supports_radius());
        assert!(e.radius_controls(6.).is_empty());
        assert_eq!(e.wheel_parameter(true, false), Some(Parameter::Block));
        let snapshot = e.radius_selection();
        assert_eq!(snapshot.selected_count, 1);
        assert!(snapshot.targets.is_empty());
        assert!(e.apply_radius(&snapshot, 20.).is_err());
        assert!(!e.adjust_parameter(Parameter::Radius, 3, 72, (0, 0), 6.));
        assert_eq!(e.document.get(id).unwrap(), &a);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.get(id).unwrap().effect_regions_managed());
    }
}

#[test]
fn effect_radius_drawing_default_is_published_without_object_edit_leaking() {
    let area = Rect::full(900, 700);
    let mut e = Editor::new(area, Some(area));
    e.select_tool(Tool::Mosaic);
    e.style.radius = 8.;
    let before = e.radius_selection();
    assert_eq!(e.apply_radius(&before, 17.), Ok(true));
    e.pointer_down_at(Point { x: 100., y: 120. }, 6., false);
    e.pointer_move_at(Point { x: 400., y: 300. });
    assert!(e.adjust_parameter(Parameter::Radius, 3, 73, (0, 0), 6.));
    assert!(e.document.items().is_empty());
    e.pointer_up_at(Point { x: 400., y: 300. });
    let id = e.selected.unwrap();
    assert_eq!(e.document.get(id).unwrap().style.effect_radius, 20.);
    assert_eq!(e.document.get(id).unwrap().style.radius, 8.);
    let snapshot = e.radius_selection();
    assert_eq!(e.apply_radius(&snapshot, 35.), Ok(true));
    assert_eq!(e.document.get(id).unwrap().style.effect_radius, 35.);
    e.select_tool(Tool::Mosaic);
    assert_eq!(e.style.effect_radius, 20.);
    assert_eq!(e.style.radius, 8.);
}

#[test]
fn effect_radius_group_scale_parameter_reanchors_both_fields_without_accumulation() {
    let area = Rect::full(900, 700);
    let mut e = Editor::new(area, Some(area));
    let mosaic = ordinary_effect(Tool::Mosaic, 12.);
    let mut rectangle = shape(Tool::Rectangle, 17.);
    rectangle.style.effect_radius = 4.; // 合法但不参与矩形圆角的独立保存值。
    e.document.insert(mosaic);
    e.document.insert(rectangle);
    e.select_all_objects();
    let original = e.document.items().to_vec();
    let revision = e.document.edit_revision();
    let bounds = e.selection_bounds().unwrap();
    let anchor = bounds.handles()[0];
    let start = bounds.handles()[7];
    let end = Point {
        x: anchor.x + 2. * (start.x - anchor.x),
        y: anchor.y + 2. * (start.y - anchor.y),
    };
    e.pointer_down_at(start, 6., false);
    e.pointer_move_at(end);
    let scaled = e.selected_objects();
    near(scaled[0].1.style.radius, 16.);
    near(scaled[0].1.style.effect_radius, 24.);
    near(scaled[1].1.style.radius, 34.);
    near(scaled[1].1.style.effect_radius, 8.);
    assert!(e.adjust_parameter(Parameter::Radius, 2, 75, (0, 0), 6.));
    let changed = e.selected_objects();
    near(changed[0].1.style.radius, 16.);
    near(changed[0].1.style.effect_radius, 26.);
    near(changed[1].1.style.radius, 36.);
    near(changed[1].1.style.effect_radius, 8.);
    assert_eq!(e.document.items(), original);
    assert_eq!(e.document.edit_revision(), revision);
    // 预览再次从手势基线推导，同点更新不得重复放大任何保存字段。
    e.pointer_move_at(end);
    assert_eq!(e.selected_objects(), changed);
    e.pointer_move_at(end);
    assert_eq!(e.selected_objects(), changed);
    e.pointer_up_at(end);
    assert_eq!(e.document.edit_revision(), revision + 1);
    assert_eq!(e.document.items()[0].annotation, changed[0].1);
    assert_eq!(e.document.items()[1].annotation, changed[1].1);
    let after = e.document.items().to_vec();
    e.undo();
    assert_eq!(e.document.items(), original);
    e.redo();
    assert_eq!(e.document.items(), after);
}
