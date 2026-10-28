//! 主选参数同步与低强度效果的模型验收；不创建窗口，不读剪贴板。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    interaction::Parameter,
    tool_presets::default_style,
};
use std::collections::HashMap;

fn object(tool: Tool, x: f32, style: Style) -> Annotation {
    let mut a = Annotation::new(tool, Point { x, y: 100. }, style);
    a.update(Point {
        x: x + 120.,
        y: 200.,
    });
    if tool.text() {
        a.text = "测试".into();
    }
    a
}
fn seeded(tool: Tool, style: Style) -> (Editor, ObjectId, ObjectId) {
    let canvas = Rect::full(800, 600);
    let mut e = Editor::new(canvas, Some(canvas));
    e.restore_tool_styles([
        (
            Tool::Rectangle,
            Style {
                width: 9.,
                radius: 14.,
                ..default_style(Tool::Rectangle)
            },
        ),
        (Tool::Text, default_style(Tool::Text)),
    ]);
    let first = e.document.insert(object(
        Tool::Rectangle,
        100.,
        default_style(Tool::Rectangle),
    ));
    let second = e.document.insert(object(tool, 360., style));
    (e, first, second)
}
fn select_first(e: &mut Editor) {
    e.select_tool(Tool::Edit);
    e.pointer_down((160, 150), 4.);
    e.pointer_up((160, 150));
}
fn presets(e: &Editor) -> HashMap<Tool, Style> {
    e.tool_styles().collect()
}

#[test]
fn ctrl_primary_selection_syncs_width_and_removing_other_selection_stays_correct() {
    let (mut e, first, second) = seeded(
        Tool::Rectangle,
        Style {
            width: 20.,
            ..default_style(Tool::Rectangle)
        },
    );
    select_first(&mut e);
    let before = e.document.items().to_vec();
    let revision = e.document.edit_revision();
    let defaults = presets(&e);
    assert_eq!(e.style.width, 4.);
    e.toggle_selection((420, 150), 4.);
    assert_eq!(e.selected, Some(second));
    assert_eq!(e.style.width, 20.);
    e.toggle_selection((160, 150), 4.);
    assert_eq!(e.selected_ids(), vec![second]);
    assert_eq!(e.style.width, 20.);
    assert_eq!(e.document.items(), before);
    assert_eq!(e.document.edit_revision(), revision);
    assert_eq!(presets(&e), defaults);
    assert!(e.adjust_parameter(Parameter::Width, 1, 10, (0, 0), 4.));
    assert_eq!(e.document.get(first).unwrap().style.width, 4.);
    assert_eq!(e.document.get(second).unwrap().style.width, 21.);
    assert_eq!(e.document.edit_revision(), revision + 1);
    e.undo();
    assert_eq!(e.document.items(), before);
    e.redo();
    assert_eq!(e.document.get(second).unwrap().style.width, 21.);

    select_first(&mut e);
    e.toggle_selection((420, 150), 4.);
    assert_eq!(e.style.width, 21.);
    e.toggle_selection((420, 150), 4.);
    assert_eq!(e.selected_ids(), vec![first]);
    assert_eq!(e.style.width, 4.);
}

#[test]
fn select_all_uses_primary_font_radius_block_and_opacity_without_replacing_drawing_preset() {
    for (tool, parameter) in [
        (Tool::Text, Parameter::Font),
        (Tool::Rectangle, Parameter::Radius),
        (Tool::Mosaic, Parameter::Block),
        (Tool::Spotlight, Parameter::Opacity),
    ] {
        let selected_style = Style {
            font_size: 60.,
            radius: 19.,
            block_size: 32,
            opacity: 0.55,
            ..default_style(tool)
        };
        let (mut e, _, second) = seeded(tool, selected_style);
        e.select_tool(Tool::Arrow);
        e.set_width(7.);
        let defaults = presets(&e);
        let before = e.document.items().to_vec();
        let revision = e.document.edit_revision();
        e.select_all_objects();
        assert_eq!(e.selected, Some(second));
        assert_eq!(e.style, selected_style);
        assert_eq!(e.document.edit_revision(), revision);
        assert_eq!(presets(&e), defaults);
        assert_eq!(
            e.wheel_parameter(parameter == Parameter::Radius, false),
            Some(parameter)
        );
        assert!(e.adjust_parameter(parameter, 1, 11, (0, 0), 4.));
        let style = e.document.get(second).unwrap().style;
        match parameter {
            Parameter::Font => assert_eq!(style.font_size, 61.),
            Parameter::Radius => assert_eq!(style.radius, 20.),
            Parameter::Block => assert_eq!(style.block_size, 33),
            Parameter::Opacity => assert!((style.opacity - 0.56).abs() < 0.00001),
            _ => unreachable!(),
        }
        assert_eq!(e.document.edit_revision(), revision + 1);
        e.undo();
        assert_eq!(e.document.items(), before);
    }
}

#[test]
fn marquee_group_expansion_ungroup_and_duplicate_keep_the_primary_style() {
    let selected_style = Style {
        width: 2.,
        opacity: 0.5,
        radius: 23.,
        ..default_style(Tool::Spotlight)
    };
    let (mut e, first, second) = seeded(Tool::Spotlight, selected_style);
    e.select_tool(Tool::Edit);
    let defaults = presets(&e);
    let revision = e.document.edit_revision();
    e.pointer_down((40, 40), 4.);
    e.pointer_up((560, 260));
    assert_eq!(e.selected, Some(second));
    assert_eq!(e.selected_ids(), vec![first, second]);
    assert_eq!(e.style, selected_style);
    assert_eq!(e.document.edit_revision(), revision);
    assert_eq!(presets(&e), defaults);
    assert!(e.group_selection(true));
    assert_eq!(e.style, selected_style);
    e.select_tool(Tool::Edit);
    let grouped_revision = e.document.edit_revision();
    e.toggle_selection((160, 150), 4.);
    assert_eq!(e.selected_ids(), vec![first, second]);
    assert_eq!(e.selected, Some(second));
    assert_eq!(e.style, selected_style);
    assert_eq!(e.document.edit_revision(), grouped_revision);
    assert!(e.group_selection(false));
    assert_eq!(e.style, selected_style);
    let before_copy = e.document.edit_revision();
    e.duplicate_selected();
    assert_eq!(e.selected_ids().len(), 2);
    assert_eq!(e.style, selected_style);
    assert_eq!(e.document.edit_revision(), before_copy + 1);
    assert_eq!(presets(&e), defaults);
}

#[test]
fn selection_does_not_replace_a_text_draft_or_an_empty_drawing_default() {
    let (mut e, _, _) = seeded(Tool::Rectangle, default_style(Tool::Rectangle));
    e.select_tool(Tool::Text);
    e.pointer_down((200, 300), 4.);
    e.set_text("草稿".into());
    let draft = e.text_draft.as_ref().unwrap().annotation.clone();
    let style = e.style;
    let defaults = presets(&e);
    let revision = e.document.edit_revision();
    e.select_all_objects();
    assert_eq!(e.text_draft.as_ref().unwrap().annotation, draft);
    assert_eq!(e.style, style);
    assert_eq!(presets(&e), defaults);
    assert_eq!(e.document.edit_revision(), revision);

    let canvas = Rect::full(800, 600);
    let mut e = Editor::new(canvas, Some(canvas));
    e.select_tool(Tool::Pen);
    e.set_width(9.);
    let style = e.style;
    e.select_all_objects();
    assert!(e.selected_ids().is_empty());
    assert_eq!(e.style, style);
    e.select_tool(Tool::Pen);
    assert_eq!(e.style.width, 9.);
}

#[test]
fn mosaic_minimum_two_survives_selection_setter_wheel_and_tool_preset_validation() {
    let (mut e, _, second) = seeded(
        Tool::Mosaic,
        Style {
            block_size: 3,
            ..default_style(Tool::Mosaic)
        },
    );
    e.select_tool(Tool::Edit);
    e.toggle_selection((420, 150), 4.);
    assert_eq!(e.style.block_size, 3);
    let original = e.document.get(second).unwrap().clone();
    let revision = e.document.edit_revision();
    assert!(e.adjust_parameter(Parameter::Block, -1, 12, (0, 0), 4.));
    assert_eq!(e.style.block_size, 2);
    assert_eq!(e.document.get(second).unwrap().style.block_size, 2);
    assert_eq!(e.document.edit_revision(), revision + 1);
    assert!(!e.adjust_parameter(Parameter::Block, -1, 12, (0, 0), 4.));
    e.undo();
    assert_eq!(e.document.get(second).unwrap(), &original);
    e.select_tool(Tool::Edit);
    e.toggle_selection((420, 150), 4.);
    e.set_block_size(2);
    assert_eq!(e.document.get(second).unwrap().style.block_size, 2);
    let mut store = crate::tool_presets::ToolPresets::default();
    assert!(store.update(Tool::Mosaic, e.style));
    assert_eq!(store.style(Tool::Mosaic).block_size, 2);
}
