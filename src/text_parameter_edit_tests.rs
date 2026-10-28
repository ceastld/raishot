//! 文字数值参数的定向编辑事务：选中对象一次提交，正文草稿保留到显式完成。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::ObjectId,
    editor::Editor,
    geometry::Rect,
    interaction::Parameter,
    tool_presets::{default_style, ToolPresets},
};

fn cases() -> [(Tool, Parameter); 4] {
    [
        (Tool::Text, Parameter::TextOutline),
        (Tool::Text, Parameter::TextBackgroundAlpha),
        (Tool::Text, Parameter::TextBackgroundRadius),
        (Tool::Watermark, Parameter::TextOutline),
    ]
}
fn changed_style(mut style: Style, parameter: Parameter) -> Style {
    match parameter {
        Parameter::TextOutline => style.text.outline_width = 7.5,
        Parameter::TextBackgroundAlpha => style.text.background_opacity = 0.42,
        Parameter::TextBackgroundRadius => style.text.background_radius = 17.25,
        _ => unreachable!(),
    }
    style
}
fn annotation(tool: Tool) -> Annotation {
    let mut style = default_style(tool);
    style.text.outline = true;
    style.text.background = tool == Tool::Text;
    let mut a = Annotation::new(tool, Point { x: 40., y: 40. }, style);
    a.update(Point { x: 340., y: 150. });
    a.text = "原中文🙂\n第二行".into();
    a
}
fn seeded(tool: Tool) -> (Editor, ObjectId, Annotation) {
    let canvas = Rect::full(640, 480);
    let mut editor = Editor::new(canvas, Some(canvas));
    let original = annotation(tool);
    let id = editor.document.insert(original.clone());
    let mut redo = original.clone();
    redo.style.text.outline_color = 0x5796ff;
    editor.document.replace(id, redo);
    editor.document.undo();
    editor.tool = Tool::Edit;
    editor.selected = Some(id);
    editor.style = original.style;
    assert!(editor.document.can_redo());
    (editor, id, original)
}

#[test]
fn finish_text_parameter_selected_single_field_commits_once_and_undo_restores() {
    for (tool, parameter) in cases() {
        let (mut editor, id, original) = seeded(tool);
        let before_edit = editor.document.edit_revision();
        let mut expected = original.clone();
        expected.style = changed_style(editor.style, parameter);
        editor.set_style(expected.style);
        assert_eq!(
            editor.document.get(id),
            Some(&expected),
            "{tool:?}/{parameter:?}"
        );
        assert_eq!(editor.document.edit_revision(), before_edit + 1);
        assert!(!editor.document.can_redo());
        editor.undo();
        assert_eq!(editor.document.get(id), Some(&original));
        assert!(editor.document.can_redo());
        editor.redo();
        assert_eq!(editor.document.get(id), Some(&expected));
        assert_eq!(editor.document.edit_revision(), before_edit + 1);
    }
}

#[test]
fn finish_text_parameter_existing_draft_keeps_body_and_identity_cancel_noop_keep_redo() {
    for (tool, parameter) in cases() {
        let (mut editor, id, original) = seeded(tool);
        let before_revision = editor.document.revision();
        let before_edit = editor.document.edit_revision();
        editor.set_style(editor.style);
        assert_eq!(editor.document.revision(), before_revision);
        assert_eq!(editor.document.edit_revision(), before_edit);
        assert!(editor.document.can_redo());

        editor.begin_text(Some(id), original.points[0]);
        editor.set_text("输入中中文🙂\n末行".into());
        let mut expected = editor.text_draft.as_ref().unwrap().annotation.clone();
        expected.style = changed_style(editor.style, parameter);
        editor.set_style(expected.style);
        let draft = editor
            .text_draft
            .as_ref()
            .expect("样式确认不能结束正文草稿");
        assert_eq!(draft.id, Some(id));
        assert_eq!(draft.annotation, expected, "{tool:?}/{parameter:?}");
        assert_eq!(editor.document.get(id), Some(&original));
        assert_eq!(editor.document.revision(), before_revision);
        assert_eq!(editor.document.edit_revision(), before_edit);
        assert!(editor.document.can_redo());

        editor.set_style(editor.style);
        assert_eq!(editor.text_draft.as_ref().unwrap().annotation, expected);
        assert_eq!(editor.document.edit_revision(), before_edit);
        assert!(editor.cancel_text());
        assert_eq!(editor.document.get(id), Some(&original));
        assert_eq!(editor.document.revision(), before_revision);
        assert_eq!(editor.document.edit_revision(), before_edit);
        assert!(editor.document.can_redo());
    }
}

#[test]
fn finish_text_parameter_new_draft_commits_only_when_text_is_finished() {
    for (tool, parameter) in cases() {
        let canvas = Rect::full(640, 480);
        let mut editor = Editor::new(canvas, Some(canvas));
        editor.select_tool(tool);
        editor.begin_text(None, Point { x: 40., y: 40. });
        editor.set_text("新中文🙂草稿".into());
        let mut expected = editor.text_draft.as_ref().unwrap().annotation.clone();
        let before_edit = editor.document.edit_revision();
        expected.style = changed_style(editor.style, parameter);
        editor.set_style(expected.style);
        let draft = editor.text_draft.as_ref().unwrap();
        assert_eq!(draft.id, None);
        assert_eq!(draft.annotation, expected);
        assert!(editor.document.items().is_empty());
        assert_eq!(editor.document.edit_revision(), before_edit);
        editor.finish_text();
        let id = editor.selected.unwrap();
        assert_eq!(editor.document.get(id), Some(&expected));
        assert_eq!(editor.document.edit_revision(), before_edit + 1);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(editor.document.can_redo());
    }
}

#[test]
fn finish_text_parameter_text_watermark_serial_presets_stay_independent() {
    let canvas = Rect::full(640, 480);
    let mut editor = Editor::new(canvas, Some(canvas));
    editor.select_tool(Tool::Text);
    let mut text = editor.style;
    text.text.outline_width = 7.5;
    text.text.background_opacity = 0.42;
    text.text.background_radius = 17.25;
    editor.set_style(text);
    editor.select_tool(Tool::Watermark);
    assert_eq!(editor.style, default_style(Tool::Watermark));
    let mut watermark = editor.style;
    watermark.text.outline_width = 6.25;
    editor.set_style(watermark);
    editor.select_tool(Tool::Serial);
    assert_eq!(editor.style, default_style(Tool::Serial));
    let mut serial = editor.style;
    serial.text.italic = true;
    editor.set_style(serial);
    let mut presets = ToolPresets::default();
    for (tool, style) in editor.tool_styles() {
        presets.update(tool, style);
    }
    let mut next = Editor::new(canvas, Some(canvas));
    next.restore_tool_styles(presets.styles());
    for (tool, style) in [
        (Tool::Text, text),
        (Tool::Watermark, watermark),
        (Tool::Serial, serial),
    ] {
        next.select_tool(tool);
        assert_eq!(next.style, style, "{tool:?}");
    }
    next.select_tool(Tool::Rectangle);
    assert_eq!(next.style, default_style(Tool::Rectangle));
}
