//! 内联画布与悬浮工具栏共享按键顺序：文字、临时操作、标注、宿主贴图。
use crate::{
    annotation::Tool,
    capture_view::{Action, CaptureView},
    pin_view::PinAction,
    toolbar_layout::ToolbarPanel,
};
use gpui::{Context, KeyDownEvent, Window};

#[path = "pin_toolbar_shortcuts.rs"]
pub(crate) mod toolbar_shortcuts;

/// 状态翻转只消费首次按下；位移、历史与连续图像调节继续接受系统重复键。
/// 宿主完成文字／IME／参数／画布优先路由后才调用，不能吞掉原生字符输入。
pub(crate) fn allow_held_shortcut(action: PinAction, is_held: bool) -> bool {
    !is_held
        || !matches!(
            action,
            PinAction::Close
                | PinAction::Lock
                | PinAction::Topmost
                | PinAction::ClickThrough
                | PinAction::Thumbnail
                | PinAction::Text
                | PinAction::Chrome
                | PinAction::Shadow
                | PinAction::Annotations
                | PinAction::FlipX
                | PinAction::FlipY
                | PinAction::Gray
                | PinAction::Invert
        )
}

impl CaptureView {
    pub(crate) fn pin_key(
        &mut self,
        e: &KeyDownEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = e.keystroke.key.as_str();
        let m = e.keystroke.modifiers;
        let digit_context = if !self.pin_mode {
            toolbar_shortcuts::DigitContext::Collapsed
        } else if self.watermark_input_focused
            || self.editor.text_draft.is_some()
            || (self.toolbar_panel != ToolbarPanel::None && !self.toolbar_panel.more())
        {
            toolbar_shortcuts::DigitContext::NativeInput
        } else {
            toolbar_shortcuts::DigitContext::Toolbar {
                editable: !self.saving && !self.processing,
            }
        };
        if self.watermark_input_focused {
            return false;
        }
        // 先给输入法让行，再处理重复键；不能借属性页 Esc 抢先结束文字合成。
        if self.editor.text_draft.is_some() && self.input.marked.is_some() {
            return false;
        }
        if crate::text_contract::terminal_held(key, e.is_held) {
            cx.stop_propagation();
            return true;
        }
        if self.saving || self.processing {
            return true;
        }
        if key == "escape" && self.toolbar_panel != ToolbarPanel::None {
            self.toolbar_panel = self.toolbar_panel.parent();
            cx.notify();
            return true;
        }
        if self.toolbar_panel != ToolbarPanel::None && !self.toolbar_panel.more() {
            // 未处理的数字键仍需原生字符转换，不能落到标注快捷键。
            return false;
        }
        if self.editor.text_draft.is_some() {
            if m.control && key == "s" {
                return false;
            }
            // 普通字符必须继续交给 Windows TranslateMessage；是否消费由文字编辑器决定。
            return self.text_key(e, w, cx);
        }
        if key == "escape" && self.editor.cancel_gesture() {
            self.cancel_pointer();
            cx.notify();
            return true;
        }
        if key == "escape" && self.editor.group_scope_depth() > 0 && self.flyout.close() {
            cx.notify();
            return true;
        }
        if key == "escape" && self.exit_group_scope_input(cx) {
            cx.stop_propagation();
            return true;
        }
        if key == "enter" && !m.modified() && self.editor.has_pending_drawing() {
            if !self.finish_content_or_status() {
                cx.notify();
                return true;
            }
            self.cancel_pointer();
            cx.notify();
            return true;
        }
        // 输出与文档历史由宿主路由，保持跨图像处理边界的先后顺序。
        if m.control && matches!(key, "c" | "s" | "z" | "y") {
            return false;
        }
        // 数字与贴图主栏的同一可见列表对应；没有对应按钮也不能漏回宿主处理图片。
        match toolbar_shortcuts::route(
            key,
            m,
            e.is_held,
            digit_context,
            &self.toolbar_config,
            toolbar_shortcuts::current_tool(self.editor.tool, self.pin_editing),
        ) {
            toolbar_shortcuts::DigitRoute::Pass => {}
            toolbar_shortcuts::DigitRoute::Consume => {
                cx.stop_propagation();
                return true;
            }
            toolbar_shortcuts::DigitRoute::Select(tool) => {
                self.pin_editing = true;
                self.perform(Action::Tool(tool), w, cx);
                w.focus(&self.focus);
                return true;
            }
        }
        let tool = crate::host_shortcuts::drawing_tool(key, m, false);
        if let Some(tool) = tool {
            self.pin_editing = true;
            self.perform(Action::Tool(tool), w, cx);
            w.focus(&self.focus);
            return true;
        }
        if !self.pin_editing {
            return false;
        }
        let control = crate::host_shortcuts::control(m);
        let plain = !m.modified();
        let action = match key {
            "delete" if crate::host_shortcuts::shift(m) => Some(Action::Clear),
            "delete" if plain => Some(Action::Delete),
            "d" if control && !m.shift => Some(Action::Duplicate),
            "f2" if plain => Some(Action::EditText),
            "g" if control && m.shift => Some(Action::Ungroup),
            "g" if control => Some(Action::Group),
            "]" if crate::host_shortcuts::alt(m) => Some(Action::Rotate(15.)),
            "[" if crate::host_shortcuts::alt(m) => Some(Action::Rotate(-15.)),
            "]" if control && !m.shift => Some(Action::Front),
            "[" if control && !m.shift => Some(Action::Back),
            _ => None,
        };
        if let Some(a) = action {
            self.perform(a, w, cx);
            return true;
        }
        if key == "a" && control && !m.shift {
            if !self.finish_content_or_status() {
                cx.notify();
                return true;
            }
            self.wheel_input.reset();
            self.editor.select_tool(Tool::Edit);
            self.editor.select_all_objects();
            cx.notify();
            return true;
        }
        if key == "backspace" && plain && self.editor.remove_polyline_point() {
            cx.notify();
            return true;
        }
        if !m.control && !m.alt && !m.platform && !m.function {
            let step = if m.shift { 10. } else { 1. };
            let delta = match key {
                "left" => Some((-step, 0.)),
                "right" => Some((step, 0.)),
                "up" => Some((0., -step)),
                "down" => Some((0., step)),
                _ => None,
            };
            if let Some((dx, dy)) = delta {
                self.editor.nudge(dx, dy);
                cx.notify();
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod held_shortcut_tests {
    use super::*;

    #[test]
    fn state_shortcuts_toggle_once_during_hold_and_rearm_for_next_press() {
        for action in [
            PinAction::Lock,
            PinAction::Topmost,
            PinAction::ClickThrough,
            PinAction::Thumbnail,
            PinAction::Text,
            PinAction::Chrome,
            PinAction::Shadow,
            PinAction::Annotations,
            PinAction::FlipX,
            PinAction::FlipY,
            PinAction::Gray,
            PinAction::Invert,
        ] {
            let (mut enabled, mut toggles) = (false, 0);
            for held in [false, true, true, true] {
                if allow_held_shortcut(action, held) {
                    enabled = !enabled;
                    toggles += 1;
                }
            }
            assert!(enabled, "{action:?}");
            assert_eq!(toggles, 1, "{action:?}");
            // 抬起后的新按下仍能主动切回，后续重复不会再次翻转。
            for held in [false, true, true] {
                if allow_held_shortcut(action, held) {
                    enabled = !enabled;
                    toggles += 1;
                }
            }
            assert!(!enabled, "{action:?}");
            assert_eq!(toggles, 2, "{action:?}");
        }
    }

    #[test]
    fn held_shortcut_guard_keeps_continuous_adjustment_and_history_repeat() {
        for action in [
            PinAction::Rotate(1),
            PinAction::Rotate(-1),
            PinAction::Brightness(10),
            PinAction::Brightness(-10),
            PinAction::Zoom(1.25),
            PinAction::Undo,
            PinAction::Redo,
            PinAction::Fit,
            PinAction::Original,
        ] {
            for held in [false, true, true] {
                assert!(allow_held_shortcut(action, held), "{action:?}");
            }
        }
    }
}
