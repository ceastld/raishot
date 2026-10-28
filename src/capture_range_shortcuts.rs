//! 普通截图的显式范围键；预览微调、文字和其它宿主保留各自的键所有权。
use super::{hwnd, CaptureView, ToolbarPanel};
use crate::{annotation::Tool, editor::Editor, geometry::Rect};
use gpui::{Context, KeyDownEvent, Modifiers, Window};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeKey {
    Current,
    All,
    Consume,
}

fn first_press(key: RangeKey, held: bool) -> bool {
    key != RangeKey::Consume && !held
}

fn shortcut(key: &str, m: Modifiers, ordinary: bool, has_crop: bool) -> Option<RangeKey> {
    if key != "a" || !ordinary || m.platform || m.function {
        return None;
    }
    match (m.control, m.alt, m.shift) {
        (false, true, false) => Some(RangeKey::Current),
        (true, false, true) => Some(RangeKey::All),
        (false, false, _) if has_crop => Some(RangeKey::Consume),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Availability {
    source_current: bool,
    active: bool,
    focused: bool,
    busy: bool,
    input_active: bool,
    menu_open: bool,
    replayed: bool,
    capture_idle: bool,
}

fn eligible(editor: &Editor, access: Availability) -> bool {
    access.source_current
        && access.active
        && access.focused
        && !access.busy
        && !access.input_active
        && !access.menu_open
        && !access.replayed
        && access.capture_idle
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
        && editor.local_fill_candidate().is_none()
        && editor
            .crop
            .is_none_or(|crop| crop.valid(editor.canvas.width, editor.canvas.height))
}

/// 全屏画布内的物理屏幕矩形；全桌面沿用已固定锚点，屏间空隙不猜一个屏。
fn target(
    key: RangeKey,
    canvas: Rect,
    screens: &[Rect],
    point: Option<(i32, i32)>,
    anchor: usize,
) -> Option<(Rect, usize)> {
    if canvas.x != 0
        || canvas.y != 0
        || !canvas.valid(canvas.width, canvas.height)
        || canvas.width > i32::MAX as u32
        || canvas.height > i32::MAX as u32
    {
        return None;
    }
    match key {
        RangeKey::Current => {
            let point = point?;
            screens
                .iter()
                .copied()
                .enumerate()
                .find_map(|(index, rect)| {
                    (rect.valid(canvas.width, canvas.height) && rect.contains(point))
                        .then_some((rect, index))
                })
        }
        RangeKey::All => screens
            .get(anchor)
            .filter(|screen| screen.valid(canvas.width, canvas.height))
            .map(|_| (canvas, anchor)),
        RangeKey::Consume => None,
    }
}

/// 沿既有范围更新合同；不平移标注，不插入虚构的矩形选区 Undo。
fn apply(editor: &mut Editor, rect: Rect) -> bool {
    if !rect.valid(editor.canvas.width, editor.canvas.height) || editor.crop == Some(rect) {
        return false;
    }
    if editor.tool == Tool::SmartMosaic {
        editor.set_smart_crop(Some(rect));
    } else {
        editor.set_crop(Some(rect));
    }
    true
}

impl CaptureView {
    /// 临时菜单的旧未知键路线会先关闭再穿透，范围键必须在菜单仍可见时拒绝。
    pub(super) fn range_shortcut_blocked_by_menu(&self, event: &KeyDownEvent) -> bool {
        shortcut(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers,
            !self.pin_mode && self.image_canvas.is_none(),
            self.editor.crop.is_some(),
        )
        .is_some()
            && self.editor.text_draft.is_none()
            && self.result_editor.is_none()
            && self.recognition_result.is_none()
            && self.ocr_overlay.is_none()
            && !self.watermark_input_focused
            && self.input.marked.is_none()
            && (self.toolbar_panel != ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some()
                || self.flyout.open().is_some())
    }

    fn range_shortcut_source_current(&self, window: &Window) -> bool {
        let full = self.source.full();
        let own = hwnd(window);
        let (serial, size) = unsafe {
            use windows::{core::w, Win32::UI::WindowsAndMessaging::GetPropW};
            (
                GetPropW(own, w!("RustShot.GpuiPresentSerial")).0 as usize,
                GetPropW(own, w!("RustShot.GpuiPresentSize")).0 as usize as u32,
            )
        };
        self.control_selection_ready
            && self
                .capture_lock
                .as_ref()
                .is_some_and(|owner| !owner.is_cancelled())
            && self.editor.canvas == full
            && serial != 0
            && full.width <= u16::MAX as u32
            && full.height <= u16::MAX as u32
            && size == ((full.width << 16) | full.height)
            && crate::preview_middle_pin::physical_size_matches(
                full,
                (
                    f32::from(window.viewport_size().width),
                    f32::from(window.viewport_size().height),
                ),
                window.scale_factor(),
            )
            && self.painted_selection.get().is_some()
            && self
                .editor
                .crop
                .is_none_or(|crop| self.presented_selection(window) == Some(crop))
    }

    pub(super) fn range_shortcut_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(key) = shortcut(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers,
            !self.pin_mode && self.image_canvas.is_none(),
            self.editor.crop.is_some(),
        ) else {
            return false;
        };
        // 先确定所有权：已选区的裸 A、长按和拒绝的组合键均不落到旧 Current。
        if !first_press(key, event.is_held) {
            return true;
        }
        use windows::Win32::{
            System::Threading::GetCurrentThreadId,
            UI::{
                Input::KeyboardAndMouse::{
                    GetAsyncKeyState, GetCapture, GetFocus, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
                    VK_XBUTTON1, VK_XBUTTON2,
                },
                WindowsAndMessaging::{
                    GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, IsWindow,
                },
            },
        };
        let own = hwnd(window);
        let capture_idle = unsafe {
            IsWindow(Some(own)).as_bool()
                && GetForegroundWindow() == own
                && GetFocus() == own
                && GetWindowThreadProcessId(own, None) == GetCurrentThreadId()
                && GetCapture().is_invalid()
                && [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
                    .into_iter()
                    .all(|key| GetAsyncKeyState(key.0 as i32) >= 0)
        };
        if !eligible(
            &self.editor,
            Availability {
                source_current: self.range_shortcut_source_current(window),
                active: window.is_window_active(),
                focused: self.focus.is_focused(window),
                busy: self.saving
                    || self.processing
                    || self.pin_handoff_pending
                    || self.color_picker_pending
                    || self.pointer_down_start.is_some()
                    || self.toolbar_drag.is_some(),
                input_active: self.watermark_input_focused
                    || self.input.marked.is_some()
                    || self.input.drag_anchor.is_some()
                    || self.result_editor.is_some()
                    || self.recognition_result.is_some()
                    || self.ocr_overlay.is_some(),
                menu_open: self.toolbar_panel != ToolbarPanel::None
                    || self.screenshot_toolbar_menu.presentation().is_some()
                    || self.flyout.open().is_some(),
                // Native 延迟的 A 不借新的鼠标、crop 或来源重新解释。
                replayed: self.preview_cursor_replay
                    || self.preview_text_replay
                    || self.capture_color_replay,
                capture_idle,
            },
        ) {
            return true;
        }
        let point = if key == RangeKey::Current {
            let mut global = windows::Win32::Foundation::POINT::default();
            if unsafe { GetCursorPos(&mut global) }.is_err() {
                return true;
            }
            global
                .x
                .checked_sub(self.source.bounds.left)
                .zip(global.y.checked_sub(self.source.bounds.top))
        } else {
            None
        };
        let screens: Vec<_> = (0..self.source.screen_count())
            .map(|index| self.source.screen_rect(index))
            .collect();
        let Some((rect, anchor)) = target(
            key,
            self.source.full(),
            &screens,
            point,
            self.toolbar_monitor,
        ) else {
            return true;
        };
        let before = self.editor.crop;
        if apply(&mut self.editor, rect) {
            self.set_manual_preview(None);
            self.clear_pointer_tracking();
            self.wheel_input.reset();
            self.preview_cursor_nudge.reset();
            self.clear_idle_preview_cursor();
            self.manual_preview_cursor_keys.cancel_gesture();
            self.manual_preview_cursor_claimed = false;
            self.new_crop_cursor_origin = None;
            self.crop_adjust_cursor_origin = None;
            self.crop_adjust_cursor_owner = None;
            self.toolbar_monitor = anchor;
            self.toolbar_visibility
                .finished_new_crop(&self.editor, Some(before));
            self.status.clear();
            cx.notify();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Annotation, Point, Style};

    fn ready() -> Availability {
        Availability {
            source_current: true,
            active: true,
            focused: true,
            busy: false,
            input_active: false,
            menu_open: false,
            replayed: false,
            capture_idle: true,
        }
    }

    #[test]
    fn explicit_range_keys_are_exact_and_bare_a_keeps_preview_owner() {
        let plain = Modifiers::default();
        assert_eq!(shortcut("a", plain, true, false), None);
        assert_eq!(shortcut("a", plain, true, true), Some(RangeKey::Consume));
        assert!(crate::capture_cursor_nudge::claims_preview_wasd(
            &Editor::new(Rect::full(400, 300), None),
            true,
            &KeyDownEvent {
                keystroke: gpui::Keystroke {
                    key: "a".into(),
                    key_char: None,
                    modifiers: plain,
                },
                is_held: false,
            }
        ));
        let current = Modifiers { alt: true, ..plain };
        let all = Modifiers {
            control: true,
            shift: true,
            ..plain
        };
        assert_eq!(shortcut("a", current, true, true), Some(RangeKey::Current));
        assert_eq!(shortcut("a", all, true, true), Some(RangeKey::All));
        for key in [RangeKey::Current, RangeKey::All] {
            assert!(first_press(key, false));
            assert!(!first_press(key, true));
        }
        assert!(!first_press(RangeKey::Consume, false));
        for (m, ordinary) in [
            (
                Modifiers {
                    shift: true,
                    ..current
                },
                true,
            ),
            (Modifiers { alt: true, ..all }, true),
            (
                Modifiers {
                    platform: true,
                    ..all
                },
                true,
            ),
            (
                Modifiers {
                    function: true,
                    ..all
                },
                true,
            ),
            (
                Modifiers {
                    control: true,
                    ..plain
                },
                true,
            ),
            (current, false),
            (all, false),
        ] {
            assert_eq!(shortcut("a", m, ordinary, true), None);
        }
    }

    #[test]
    fn range_gate_rejects_foreign_source_busy_input_menu_drag_and_replay() {
        let mut editor = Editor::new(Rect::full(400, 300), Some(Rect::full(120, 90)));
        assert!(eligible(&editor, ready()));
        for access in [
            Availability {
                source_current: false,
                ..ready()
            },
            Availability {
                active: false,
                ..ready()
            },
            Availability {
                focused: false,
                ..ready()
            },
            Availability {
                busy: true,
                ..ready()
            },
            Availability {
                input_active: true,
                ..ready()
            },
            Availability {
                menu_open: true,
                ..ready()
            },
            Availability {
                replayed: true,
                ..ready()
            },
            Availability {
                capture_idle: false,
                ..ready()
            },
        ] {
            let stamp = editor.scene_stamp();
            assert!(!eligible(&editor, access));
            assert_eq!(editor.crop, Some(Rect::full(120, 90)));
            assert_eq!(editor.scene_stamp(), stamp);
        }
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((30, 30), 3.);
        assert!(!eligible(&editor, ready()));
        editor.cancel_gesture();
        assert!(eligible(&editor, ready()));
        editor.crop = Some(Rect {
            x: 390,
            y: 0,
            width: 40,
            height: 10,
        });
        assert!(!eligible(&editor, ready()));
    }

    #[test]
    fn range_geometry_keeps_physical_pixels_gap_failure_and_full_screen_anchor() {
        let canvas = Rect::full(4480, 1680);
        // 虚拟桌面全局原点为 (-1920, -240)，宿主只减一次原点；第二屏为竖屏。
        let screens = [
            Rect::full(1920, 1080),
            Rect {
                x: 1920,
                y: 240,
                width: 1440,
                height: 1440,
            },
        ];
        let point = (
            (-1700_i32).checked_sub(-1920).unwrap(),
            (-100_i32).checked_sub(-240).unwrap(),
        );
        assert_eq!(
            target(RangeKey::Current, canvas, &screens, Some(point), 1),
            Some((screens[0], 0))
        );
        assert_eq!(
            target(RangeKey::Current, canvas, &screens, Some((1920, 240)), 0),
            Some((screens[1], 1))
        );
        assert_eq!(
            target(RangeKey::Current, canvas, &screens, Some((2200, 20)), 0),
            None
        );
        assert_eq!(target(RangeKey::Current, canvas, &screens, None, 0), None);
        assert_eq!(
            target(RangeKey::All, canvas, &screens, Some((2200, 300)), 0),
            Some((canvas, 0))
        );
        assert_eq!(target(RangeKey::All, canvas, &screens, None, 2), None);
        assert_eq!(
            target(RangeKey::All, Rect::full(u32::MAX, 1), &screens, None, 0),
            None
        );
    }

    #[test]
    fn range_apply_preserves_world_annotations_and_true_undo_redo_without_range_commands() {
        let mut editor = Editor::new(Rect::full(400, 300), Some(Rect::full(120, 90)));
        let first = editor.document.insert(Annotation::new(
            Tool::Rectangle,
            Point { x: 250., y: 200. },
            Style::default(),
        ));
        let second = editor.document.insert(Annotation::new(
            Tool::Ellipse,
            Point { x: 330., y: 240. },
            Style::default(),
        ));
        editor.document.undo();
        let objects = editor.document.items().to_vec();
        assert!(editor.document.can_undo() && editor.document.can_redo());
        let stamp = editor.scene_stamp();
        assert!(!apply(
            &mut editor,
            Rect {
                x: 390,
                y: 0,
                width: 40,
                height: 10
            }
        ));
        assert!(!apply(&mut editor, Rect::full(120, 90)));
        assert_eq!(editor.scene_stamp(), stamp);
        assert_eq!(editor.document.items(), objects);
        assert!(apply(&mut editor, Rect::full(400, 300)));
        assert_eq!(editor.document.items(), objects);
        assert!(editor.document.can_undo() && editor.document.can_redo());
        editor.document.redo();
        assert_eq!(editor.document.items()[1].id, second);
        assert_eq!(editor.document.items()[0].id, first);
        editor.document.undo();
        editor.document.undo();
        assert!(editor.document.items().is_empty());
        assert_eq!(editor.crop, Some(Rect::full(400, 300)));
        assert!(!editor.document.can_undo());
    }
}
