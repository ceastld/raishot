use crate::{
    annotation::{Annotation, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
    interaction::Parameter,
    tool_presets::default_style,
};
use std::collections::HashMap;

fn shape(tool: Tool, radius: f32, x: f32) -> Annotation {
    let mut a = Annotation::new(
        tool,
        Point { x, y: 80. },
        Style {
            radius,
            ..default_style(tool)
        },
    );
    a.update(Point {
        x: x + 120.,
        y: 180.,
    });
    a
}
fn seeded() -> Editor {
    let mut e = Editor::new(Rect::full(900, 600), Some(Rect::full(900, 600)));
    for (i, (tool, radius)) in [
        (Tool::Rectangle, 8.),
        (Tool::Spotlight, 13.),
        (Tool::HighlighterRect, 19.),
        (Tool::Arrow, 31.),
    ]
    .into_iter()
    .enumerate()
    {
        e.document
            .insert(shape(tool, radius, 30. + 180. * i as f32));
    }
    e.restore_tool_styles(
        [Tool::Rectangle, Tool::Spotlight, Tool::HighlighterRect].map(|tool| {
            (
                tool,
                Style {
                    radius: 29.,
                    width: 9.,
                    ..default_style(tool)
                },
            )
        }),
    );
    e.select_all_objects();
    e
}
fn presets(e: &Editor) -> HashMap<Tool, Style> {
    e.tool_styles().collect()
}

#[test]
fn radius_bulk_core_absolute_filters_mixed_tools_one_history_and_no_preset_change() {
    let mut e = seeded();
    let before = e.document.items().to_vec();
    let defaults = presets(&e);
    let crop = e.crop;
    let revision = e.document.edit_revision();
    let snapshot = e.radius_selection();
    assert_eq!(
        (
            snapshot.selected_count,
            snapshot.targets.len(),
            snapshot.value,
            snapshot.mixed
        ),
        (4, 3, 8., true)
    );
    assert_eq!(e.apply_radius(&snapshot, 17.), Ok(true));
    assert_eq!(e.document.edit_revision(), revision + 1);
    assert_eq!(e.crop, crop);
    assert_eq!(presets(&e), defaults);
    for (after, old) in e.document.items().iter().zip(&before) {
        let mut expected = old.clone();
        if expected.annotation.tool.supports_radius() {
            expected.annotation.style.radius = 17.;
        }
        assert_eq!(after, &expected);
    }
    let after = e.document.items().to_vec();
    e.undo();
    assert_eq!(e.document.items(), before);
    e.redo();
    assert_eq!(e.document.items(), after);
}

#[test]
fn radius_bulk_core_wheel_preserves_mixed_offsets_and_merges_at_individual_limits() {
    let mut e = seeded();
    let first = e.document.items()[0].id;
    let mut a = e.document.get(first).unwrap().clone();
    a.style.radius = 48.;
    e.document.replace(first, a);
    let before = e.document.items().to_vec();
    let defaults = presets(&e);
    assert_eq!(e.wheel_parameter(true, false), Some(Parameter::Radius));
    assert!(e.adjust_parameter(Parameter::Radius, 5, 912, (0, 0), 4.));
    assert!(e.adjust_parameter(Parameter::Radius, 1, 912, (0, 0), 4.));
    assert_eq!(
        e.document
            .items()
            .iter()
            .map(|o| o.annotation.style.radius)
            .collect::<Vec<_>>(),
        [50., 19., 25., 31.]
    );
    assert_eq!(presets(&e), defaults);
    e.undo();
    assert_eq!(e.document.items(), before);
}

#[test]
fn radius_bulk_core_stale_and_invalid_confirm_do_not_mutate_any_target() {
    let mut e = seeded();
    let snapshot = e.radius_selection();
    let old = e.document.items().to_vec();
    for value in [f32::NAN, f32::INFINITY, -1., 51.] {
        assert!(e.apply_radius(&snapshot, value).is_err());
        assert_eq!(e.document.items(), old);
    }
    let first = old[0].id;
    let mut a = e.document.get(first).unwrap().clone();
    a.rotation = 13.;
    e.document.replace(first, a);
    let changed = e.document.items().to_vec();
    assert!(e.apply_radius(&snapshot, 17.).is_err());
    assert_eq!(e.document.items(), changed);
    let new = e.radius_selection();
    e.selected = Some(first);
    assert!(e.apply_radius(&new, 17.).is_err());
    assert_eq!(e.document.items(), changed);
}

#[test]
fn radius_bulk_core_uniform_noop_preserves_redo_and_defaults() {
    let mut e = seeded();
    assert_eq!(e.apply_radius(&e.radius_selection(), 17.), Ok(true));
    e.document.insert(shape(Tool::Ellipse, 0., 100.));
    e.document.undo();
    let before = e.document.items().to_vec();
    let defaults = presets(&e);
    let revision = e.document.edit_revision();
    assert_eq!(e.apply_radius(&e.radius_selection(), 17.), Ok(false));
    assert_eq!(e.document.items(), before);
    assert_eq!(e.document.edit_revision(), revision);
    assert!(e.document.can_redo());
    assert_eq!(presets(&e), defaults);
}

#[test]
fn radius_bulk_core_single_explicit_commit_remembers_rectangle_spotlight_only() {
    for tool in [Tool::Rectangle, Tool::Spotlight, Tool::HighlighterRect] {
        let mut e = seeded();
        let id = e
            .document
            .items()
            .iter()
            .find(|o| o.annotation.tool == tool)
            .unwrap()
            .id;
        e.selection.clear();
        e.selected = Some(id);
        e.style = e.document.get(id).unwrap().style;
        let before = presets(&e);
        assert_eq!(e.apply_radius(&e.radius_selection(), 17.), Ok(true));
        let mut expected = before;
        if tool != Tool::HighlighterRect {
            expected.get_mut(&tool).unwrap().radius = 17.;
        }
        assert_eq!(presets(&e), expected);
        assert!(e.adjust_parameter(Parameter::Radius, 1, 77, (0, 0), 4.));
        e.select_tool(tool);
        assert_eq!(
            e.style.radius,
            if tool == Tool::HighlighterRect {
                29.
            } else {
                18.
            }
        );
    }
}

#[test]
fn radius_bulk_core_highlight_active_tool_object_edit_keeps_next_stroke_radius() {
    let mut e = Editor::new(Rect::full(900, 600), Some(Rect::full(900, 600)));
    e.select_tool(Tool::HighlighterRect);
    let mut style = e.style;
    style.radius = 9.;
    e.set_style(style);
    e.pointer_down((50, 50), 4.);
    e.pointer_up((200, 150));
    assert_eq!(e.apply_radius(&e.radius_selection(), 17.), Ok(true));
    assert_eq!(e.document.items()[0].annotation.style.radius, 17.);
    let defaults = presets(&e);
    assert_eq!(defaults[&Tool::HighlighterRect].radius, 9.);
    e.pointer_down((400, 250), 4.);
    e.pointer_up((580, 390));
    assert_eq!(e.document.items()[1].annotation.style.radius, 9.);
}

#[test]
fn radius_bulk_core_highlight_draft_wheel_is_published_after_drawing_not_object_edit() {
    let mut e = Editor::new(Rect::full(900, 600), Some(Rect::full(900, 600)));
    e.select_tool(Tool::HighlighterRect);
    e.pointer_down((50, 50), 4.);
    assert!(e.adjust_parameter(Parameter::Radius, 7, 12, (0, 0), 4.));
    assert!(e.document.items().is_empty());
    e.pointer_up((200, 150));
    assert_eq!(e.document.items()[0].annotation.style.radius, 7.);
    let id = e.document.items()[0].id;
    let control = e.radius_controls(6.)[0];
    let before = presets(&e);
    e.pointer_down_at(control.position, 6., false);
    assert!(e.adjust_parameter(Parameter::Radius, 3, 14, (0, 0), 4.));
    e.cancel_gesture();
    assert_eq!(e.document.get(id).unwrap().style.radius, 7.);
    assert_eq!(presets(&e), before);
    e.pointer_down((400, 250), 4.);
    e.pointer_up((580, 390));
    assert_eq!(e.document.items()[1].annotation.style.radius, 7.);
}

#[test]
fn radius_bulk_core_rectangle_spotlight_move_wheel_cancel_keeps_next_stroke() {
    for tool in [Tool::Rectangle, Tool::Spotlight] {
        let mut e = Editor::new(Rect::full(900, 600), Some(Rect::full(900, 600)));
        e.select_tool(tool);
        let mut style = e.style;
        style.radius = 9.;
        e.set_style(style);
        e.pointer_down((50, 50), 4.);
        e.pointer_up((200, 150));
        let id = e.document.items()[0].id;
        let before = presets(&e);
        e.pointer_down((120, 100), 4.);
        assert!(e.is_dragging());
        assert!(e.adjust_parameter(Parameter::Radius, 3, 42, (0, 0), 4.));
        assert_eq!(presets(&e), before, "预览不能写入下一笔或关闭保存");
        assert_eq!(e.document.get(id).unwrap().style.radius, 9.);
        e.cancel_gesture();
        assert_eq!(presets(&e), before);
        assert_eq!(e.style.radius, 9.);
        e.pointer_down((400, 250), 4.);
        e.pointer_up((580, 390));
        assert_eq!(e.document.items()[1].annotation.style.radius, 9.);
    }
}

#[test]
fn effect_radius_bulk_routes_fields_and_keeps_compound_and_defaults_unchanged() {
    let area = Rect::full(1200, 700);
    let mut e = Editor::new(area, Some(area));
    for (index, tool) in [
        Tool::Rectangle,
        Tool::Mosaic,
        Tool::Blur,
        Tool::Mosaic,
        Tool::Arrow,
    ]
    .into_iter()
    .enumerate()
    {
        let mut a = shape(tool, 8. + index as f32, 30. + index as f32 * 180.);
        if matches!(tool, Tool::Mosaic | Tool::Blur) {
            a.style.effect_radius = 17. + index as f32;
        }
        if index == 3 {
            a.set_effect_regions(vec![a.local_bounds()], true).unwrap();
        }
        e.document.insert(a);
    }
    e.select_all_objects();
    assert_eq!(e.selected_annotation().unwrap().tool, Tool::Arrow);
    let before = e.document.items().to_vec();
    let defaults = presets(&e);
    let crop = e.crop;
    let revision = e.document.edit_revision();
    let snapshot = e.radius_selection();
    assert_eq!(
        (
            snapshot.selected_count,
            snapshot.targets.len(),
            snapshot.mixed
        ),
        (5, 3, true)
    );
    assert_eq!(e.apply_radius(&snapshot, 20.), Ok(true));
    assert_eq!(e.document.edit_revision(), revision + 1);
    for (index, (after, old)) in e.document.items().iter().zip(&before).enumerate() {
        let mut expected = old.clone();
        match index {
            0 => expected.annotation.style.radius = 20.,
            1 | 2 => expected.annotation.style.effect_radius = 20.,
            _ => {}
        }
        assert_eq!(after, &expected);
    }
    let after = e.document.items().to_vec();
    assert_eq!(e.crop, crop);
    assert_eq!(presets(&e), defaults);
    e.undo();
    assert_eq!(e.document.items(), before);
    e.redo();
    assert_eq!(e.document.items(), after);
    // Undo/Redo 按已有合同清除选择；再次批量调整要走真实重选入口。
    assert!(e.selected_ids().is_empty());
    e.select_all_objects();
    let selected = e.radius_selection();
    assert_eq!((selected.selected_count, selected.targets.len()), (5, 3));
    assert_eq!(selected.value, 20.);
    assert!(!selected.mixed);
    assert!(e.adjust_parameter(Parameter::Radius, 3, 74, (0, 0), 6.));
    for (index, object) in e.document.items().iter().enumerate() {
        match index {
            0 => assert_eq!(object.annotation.style.radius, 23.),
            1 | 2 => {
                assert_eq!(object.annotation.style.effect_radius, 23.);
                assert_eq!(
                    object.annotation.style.radius,
                    before[index].annotation.style.radius
                );
            }
            _ => assert_eq!(object, &before[index]),
        }
    }
    assert_eq!(e.document.edit_revision(), revision + 2); // 两次内容提交；Undo/Redo不计入编辑版本。
    assert_eq!(presets(&e), defaults);
}
