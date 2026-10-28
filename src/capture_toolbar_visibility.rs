//! 普通截图工具栏会话显隐；范围和标注仍由 Editor 独立管理。
use crate::{editor::Editor, geometry::Rect, toolbar_layout::ToolbarPanel};
use gpui::KeyDownEvent;

pub(super) struct ToolbarVisibility {
    visible: bool,
    awaiting_new_crop: bool,
}

impl Default for ToolbarVisibility {
    fn default() -> Self {
        Self {
            visible: true,
            awaiting_new_crop: true,
        }
    }
}

impl ToolbarVisibility {
    pub(super) fn observe_crop(&mut self, editor: &Editor) {
        if editor.is_changing_crop() {
            return;
        }
        if !valid_crop(editor) {
            self.awaiting_new_crop = true;
        } else if self.awaiting_new_crop {
            self.visible = true;
            self.awaiting_new_crop = false;
        }
    }

    pub(super) fn finished_new_crop(&mut self, editor: &Editor, before: Option<Option<Rect>>) {
        // New 手势才可恢复；Move/Resize 的 Rect 变化不能撤销用户隐藏选择。
        if before.is_some_and(|before| editor.crop != before)
            && valid_crop(editor)
            && !editor.is_changing_crop()
        {
            self.visible = true;
            self.awaiting_new_crop = false;
        }
    }

    pub(super) fn visible(&self) -> bool {
        self.visible
    }

    pub(super) fn space(
        &mut self,
        editor: &Editor,
        event: &KeyDownEvent,
        ordinary: bool,
        busy: bool,
        input_active: bool,
        panel: ToolbarPanel,
    ) -> bool {
        if !ordinary
            || busy
            || input_active
            || editor.text_draft.is_some()
            || (panel != ToolbarPanel::None && !panel.more())
            || editor.is_changing_crop()
            || !valid_crop(editor)
            || event.keystroke.key != "space"
            || event.is_held
            || event.keystroke.modifiers.modified()
        {
            return false;
        }
        self.observe_crop(editor);
        self.visible = !self.visible;
        true
    }
}

fn valid_crop(editor: &Editor) -> bool {
    editor
        .crop
        .is_some_and(|rect| rect.valid(editor.canvas.width, editor.canvas.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Tool;
    use gpui::{Keystroke, Modifiers};

    fn editor() -> Editor {
        Editor::new(
            Rect::full(240, 160),
            Some(Rect {
                x: 20,
                y: 20,
                width: 100,
                height: 80,
            }),
        )
    }
    fn space(modifiers: Modifiers, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: "space".into(),
                key_char: Some(" ".into()),
                modifiers,
            },
            is_held: held,
        }
    }
    fn toggle(state: &mut ToolbarVisibility, editor: &Editor) -> bool {
        state.space(
            editor,
            &space(Modifiers::default(), false),
            true,
            false,
            false,
            ToolbarPanel::None,
        )
    }

    #[test]
    fn confirmed_plain_space_toggles_group_once_and_preserves_editor_content_and_history() {
        let mut editor = editor();
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((30, 30), 3.);
        editor.pointer_up((65, 55));
        let crop = editor.crop;
        let items = editor.document.items().to_vec();
        let revision = (editor.document.revision(), editor.document.edit_revision());
        let history = (editor.document.can_undo(), editor.document.can_redo());
        let tool = editor.tool;
        let mut state = ToolbarVisibility::default();
        assert!(toggle(&mut state, &editor));
        assert!(!state.visible());
        assert!(!state.space(
            &editor,
            &space(Modifiers::default(), true),
            true,
            false,
            false,
            ToolbarPanel::None
        ));
        assert!(!state.visible());
        assert!(toggle(&mut state, &editor));
        assert!(state.visible());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.items(), items);
        assert_eq!(
            (editor.document.revision(), editor.document.edit_revision()),
            revision
        );
        assert_eq!(
            (editor.document.can_undo(), editor.document.can_redo()),
            history
        );
        assert_eq!(editor.tool, tool);
    }

    #[test]
    fn preview_modified_other_hosts_and_input_occupancy_keep_space_unhandled() {
        let mut editor = editor();
        let mut state = ToolbarVisibility::default();
        for modifiers in [
            Modifiers {
                shift: true,
                ..Default::default()
            },
            Modifiers {
                control: true,
                ..Default::default()
            },
            Modifiers {
                alt: true,
                ..Default::default()
            },
            Modifiers {
                platform: true,
                ..Default::default()
            },
            Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            assert!(!state.space(
                &editor,
                &space(modifiers, false),
                true,
                false,
                false,
                ToolbarPanel::None
            ));
        }
        for (ordinary, busy, input, panel) in [
            (false, false, false, ToolbarPanel::None),
            (true, true, false, ToolbarPanel::None),
            (true, false, true, ToolbarPanel::None),
            (true, false, false, ToolbarPanel::Font),
            (true, false, false, ToolbarPanel::Finish),
        ] {
            assert!(!state.space(
                &editor,
                &space(Modifiers::default(), false),
                ordinary,
                busy,
                input,
                panel
            ));
        }
        editor.select_tool(Tool::Text);
        editor.pointer_down((40, 40), 3.);
        assert!(editor.text_draft.is_some());
        assert!(!toggle(&mut state, &editor));
        editor.finish_text();
        editor.set_crop(None);
        assert!(!toggle(&mut state, &editor));
        assert!(state.visible());
        editor.set_crop(Some(Rect::full(240, 160)));
        assert!(state.space(
            &editor,
            &space(Modifiers::default(), false),
            true,
            false,
            false,
            ToolbarPanel::Tools
        ));
        assert!(!state.visible());
    }

    #[test]
    fn existing_crop_move_resize_and_center_wheel_keep_hidden_but_reselect_restores() {
        let mut editor = editor();
        let mut state = ToolbarVisibility::default();
        assert!(toggle(&mut state, &editor));
        for (start, end) in [((50, 50), (65, 60)), ((135, 110), (150, 120))] {
            editor.pointer_down(start, 3.);
            assert!(editor.is_changing_crop());
            assert_eq!(editor.crop_creation_before(), None);
            editor.pointer_move(end);
            state.observe_crop(&editor);
            assert!(!state.visible());
            editor.pointer_up(end);
            state.observe_crop(&editor);
            assert!(!state.visible());
        }
        assert!(editor.adjust_crop_from_center(1));
        state.observe_crop(&editor);
        assert!(!state.visible());
        editor.set_crop(None);
        state.observe_crop(&editor);
        editor.pointer_down((30, 25), 3.);
        assert!(editor.crop_creation_before().is_some());
        editor.pointer_move((100, 75));
        state.observe_crop(&editor);
        assert!(!state.visible());
        editor.pointer_up((100, 75));
        state.observe_crop(&editor);
        assert!(state.visible());
    }

    #[test]
    fn lasso_outside_creates_a_new_crop_and_cancelled_creation_does_not_restore_chrome() {
        let mut editor = editor();
        editor.select_tool(Tool::Lasso);
        let mut state = ToolbarVisibility::default();
        assert!(toggle(&mut state, &editor));
        let before = editor.crop;
        editor.pointer_down((160, 115), 3.);
        assert!(editor.crop_creation_before().is_some());
        editor.pointer_move((215, 145));
        state.observe_crop(&editor);
        editor.cancel_gesture();
        state.observe_crop(&editor);
        assert_eq!(editor.crop, before);
        assert!(!state.visible());
        editor.pointer_down((160, 115), 3.);
        let creating = editor.crop_creation_before();
        assert_eq!(creating, Some(before));
        editor.pointer_up((215, 145));
        state.finished_new_crop(&editor, creating);
        assert!(state.visible());
    }
}
