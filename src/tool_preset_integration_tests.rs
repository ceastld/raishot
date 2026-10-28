use crate::{annotation::Tool, editor::Editor, geometry::Rect, tool_presets::ToolPresets};

#[test]
fn tool_presets_restore_active_and_switched_styles_without_cross_tool_leaks() {
    let full = Rect::full(800, 600);
    let mut editor = Editor::new(full, Some(full));
    let mut presets = ToolPresets::default();
    editor.restore_tool_styles(presets.styles());
    editor.select_tool(Tool::Pen);
    editor.set_color(0x5796ff);
    editor.set_width(7.);
    editor.select_tool(Tool::Rectangle);
    editor.set_color(0x39c996);
    editor.set_width(3.);
    for (tool, style) in editor.tool_styles() {
        presets.update(tool, style);
    }
    let mut next = Editor::new(full, Some(full));
    next.restore_tool_styles(presets.styles());
    next.select_tool(Tool::Pen);
    assert_eq!((next.style.color, next.style.width), (0x5796ff, 7.));
    next.select_tool(Tool::Rectangle);
    assert_eq!((next.style.color, next.style.width), (0x39c996, 3.));
    next.select_tool(Tool::Highlighter);
    assert_eq!(
        next.style,
        crate::tool_presets::default_style(Tool::Highlighter)
    );
}
