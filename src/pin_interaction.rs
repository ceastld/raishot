//! 贴图手势只保存屏幕物理坐标；GPUI Windows 的 start_window_move 尚未实现。
use crate::{gpui_capture::hwnd, pin_model::Placement, pin_view::PinView};
use gpui::{Context, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Window};
use windows::Win32::{
    Foundation::POINT,
    Graphics::Gdi::ClientToScreen,
    UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
};
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Move,
    Resize { left: bool, top: bool },
    Export,
}
#[derive(Clone, Copy)]
pub(crate) struct Gesture {
    start: (i32, i32),
    bounds: Placement,
    kind: Kind,
    threshold: i32,
    moved: bool,
}
/// 仅紧接真实移动的一次 Down 可抑制双击；其它输入消费标记而不改其原路线。
#[derive(Default)]
pub(crate) struct PostDragClick {
    pending: bool,
    suppress_current_double: bool,
}
impl PostDragClick {
    fn finish(&mut self, gesture: Option<Gesture>) {
        self.pending = gesture.is_some_and(|g| matches!(g.kind, Kind::Move) && g.moved);
        self.suppress_current_double = false;
    }
    fn cancel(&mut self) {
        self.pending = false;
        self.suppress_current_double = false;
    }
    pub(crate) fn mouse_down(&mut self, event: &MouseDownEvent) {
        self.suppress_current_double = std::mem::take(&mut self.pending)
            && event.button == MouseButton::Left
            && event.click_count == 2;
    }
    pub(crate) fn suppress_double_click(&self) -> bool {
        self.suppress_current_double
    }
}
pub(crate) fn ocr_alt_move_requested(event: &MouseDownEvent) -> bool {
    event.button == MouseButton::Left
        && event.click_count == 1
        && event.modifiers.alt
        && !event.modifiers.control
}
pub(crate) fn ocr_cursor(
    gesture: Option<&Gesture>,
    modifiers: gpui::Modifiers,
    move_allowed: bool,
) -> gpui::CursorStyle {
    if gesture.is_some_and(|gesture| matches!(gesture.kind, Kind::Move))
        || (move_allowed && modifiers.alt && !modifiers.control)
    {
        // 普通贴图的 OpenHand / ClosedHand 在 Windows 也映射到共享 IDC_ARROW。
        gpui::CursorStyle::Arrow
    } else {
        gpui::CursorStyle::IBeam
    }
}
/// 取色的键盘入口只允许移动态；不能借 Color 动作提交隐藏的文字/绘图草稿。
pub(crate) fn color_pick_alt_idle(
    editor: Option<&crate::editor::Editor>,
    editing: bool,
    interaction_busy: bool,
    input_active: bool,
) -> bool {
    !editing
        && !interaction_busy
        && !input_active
        && editor.is_none_or(|editor| {
            !editor.is_dragging()
                && !editor.has_pending_drawing()
                && editor.text_draft.is_none()
                && editor.lasso_draft.is_none()
                && !editor.watermark.has_preview()
        })
}

#[cfg(test)]
mod color_pick_alt_tests {
    use super::*;
    use crate::{capture_view::GuideToggleGesture, editor::Editor, geometry::Rect};
    use gpui::{ModifierKey, Modifiers, ModifiersChangedEvent};

    fn alt(side: ModifierKey, up: bool, repeat: bool) -> ModifiersChangedEvent {
        let extended = if side == ModifierKey::RightAlt {
            1 << 24
        } else {
            0
        };
        let message = if up { 0x0105 } else { 0x0104 };
        let value = extended
            | if up {
                (1_isize << 30) | (1_isize << 31)
            } else if repeat {
                1 << 30
            } else {
                0
            };
        ModifiersChangedEvent {
            modifiers: Modifiers {
                alt: !up,
                ..Default::default()
            },
            physical_key: gpui::decode_windows_modifier_key_event(message, 0x12, value),
            ..Default::default()
        }
    }

    fn idle(editor: &Editor) -> bool {
        color_pick_alt_idle(Some(editor), false, false, false)
    }

    #[test]
    fn native_alt_release_toggles_move_mode_color_once_and_rejected_up_cannot_rearm() {
        let editor = Editor::new(Rect::full(300, 200), None);
        for side in [ModifierKey::LeftAlt, ModifierKey::RightAlt] {
            let mut gesture = GuideToggleGesture::default();
            let mut color_pick = false;
            for expected in [true, false] {
                assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
                assert!(!gesture.observe(&alt(side, false, true), idle(&editor)));
                if gesture.observe(&alt(side, true, false), idle(&editor)) {
                    color_pick = !color_pick;
                }
                assert_eq!(color_pick, expected);
                assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
            }
            assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
            assert!(!gesture.observe(
                &alt(side, true, false),
                color_pick_alt_idle(Some(&editor), false, true, false)
            ));
            assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
        }
    }

    #[test]
    fn ocr_alt_move_mouse_use_chords_and_focus_loss_do_not_toggle_color() {
        let editor = Editor::new(Rect::full(300, 200), None);
        let mut gesture = GuideToggleGesture::default();
        let side = ModifierKey::LeftAlt;
        assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
        let event = MouseDownEvent {
            button: MouseButton::Left,
            position: gpui::point(gpui::px(10.), gpui::px(20.)),
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            click_count: 1,
            first_mouse: false,
        };
        assert!(ocr_alt_move_requested(&event));
        // Same pending reset used by Pin's Capture Down/Move/Up/Wheel handlers.
        gesture.reset();
        let mut movement = Gesture {
            start: (-40, 20),
            bounds: Placement {
                x: -40,
                y: 20,
                width: 100,
                height: 80,
            },
            kind: Kind::Move,
            threshold: 3,
            moved: false,
        };
        assert_eq!(movement.placement((-30, 30)).unwrap().x, -30);
        assert!(!gesture.observe(&alt(side, false, true), idle(&editor)));
        assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
        assert!(editor.document.items().is_empty());

        assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
        let mut other = ModifiersChangedEvent {
            modifiers: Modifiers {
                alt: true,
                shift: true,
                ..Default::default()
            },
            physical_key: gpui::decode_windows_modifier_key_event(0x0100, 0x10, 0),
            ..Default::default()
        };
        assert!(!gesture.observe(&other, idle(&editor)));
        other.modifiers.shift = false;
        other.physical_key = gpui::decode_windows_modifier_key_event(0x0101, 0x10, 1_isize << 31);
        assert!(!gesture.observe(&other, idle(&editor)));
        assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
        assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
        assert!(!gesture.observe(&alt(ModifierKey::RightAlt, false, false), idle(&editor)));
        // Left Up leaves Right Alt held; Right Up finally clears the aggregate.
        let mut left_up = alt(side, true, false);
        left_up.modifiers.alt = true;
        assert!(!gesture.observe(&left_up, idle(&editor)));
        assert!(!gesture.observe(&alt(ModifierKey::RightAlt, true, false), idle(&editor)));
        gesture.reset_tracking();
        assert!(!gesture.observe(&alt(side, false, true), idle(&editor)));
        assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
        gesture.reset_tracking();
        assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
        assert!(gesture.observe(&alt(side, true, false), idle(&editor)));
        let snapshot = ModifiersChangedEvent {
            modifiers: Modifiers {
                alt: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!gesture.observe(&snapshot, idle(&editor)));
        assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
    }

    #[test]
    fn real_editor_text_drawing_and_lasso_refuse_toggle_without_committing_history() {
        let mut editor = Editor::new(Rect::full(300, 200), Some(Rect::full(300, 200)));
        editor.select_tool(crate::annotation::Tool::Rectangle);
        editor.pointer_down((20, 20), 4.);
        editor.pointer_move((80, 70));
        editor.pointer_up((80, 70));
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let crop = editor.crop;
        let mut gesture = GuideToggleGesture::default();
        let side = ModifierKey::LeftAlt;
        assert!(!gesture.observe(&alt(side, false, false), idle(&editor)));
        editor.select_tool(crate::annotation::Tool::Text);
        editor.pointer_down((120, 100), 4.);
        editor.set_text("未提交中文".into());
        assert!(editor.text_draft.is_some());
        assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
        assert!(editor.text_draft.is_some());
        assert_eq!(editor.document.items(), items);
        editor.cancel_text();
        assert!(!gesture.observe(&alt(side, true, false), idle(&editor)));
        editor.select_tool(crate::annotation::Tool::Polyline);
        editor.pointer_down((120, 100), 4.);
        editor.pointer_up((120, 100));
        assert!(editor.has_pending_drawing());
        assert!(!idle(&editor));
        editor.cancel_gesture();
        editor.select_tool(crate::annotation::Tool::Lasso);
        editor.pointer_down((120, 100), 4.);
        for point in [(200, 100), (200, 150), (120, 150)] {
            editor.pointer_move(point);
        }
        editor.pointer_up((120, 100));
        assert!(editor.lasso_draft.is_some());
        assert!(!idle(&editor));
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.crop, crop);
        editor.cancel_gesture();
        assert!(idle(&editor));
        assert!(!color_pick_alt_idle(Some(&editor), true, false, false));
        assert!(!color_pick_alt_idle(Some(&editor), false, false, true));
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items(), items);
    }
}
/// 仅宿主移动态微调：保留裸键/Shift，其他修饰键不得误改贴图位置。
pub(crate) fn keyboard_move_delta(
    key: &str,
    modifiers: gpui::Modifiers,
    locked: bool,
) -> Option<(i32, i32)> {
    if locked || modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
        return None;
    }
    let step = if modifiers.shift { 10 } else { 1 };
    match key {
        "left" => Some((-step, 0)),
        "right" => Some((step, 0)),
        "up" => Some((0, -step)),
        "down" => Some((0, step)),
        _ => None,
    }
}
fn screen(p: gpui::Point<gpui::Pixels>, w: &Window) -> (i32, i32) {
    let mut p = POINT {
        x: (f32::from(p.x) * w.scale_factor()).round() as i32,
        y: (f32::from(p.y) * w.scale_factor()).round() as i32,
    };
    unsafe {
        let _ = ClientToScreen(hwnd(w), &mut p);
    }
    (p.x, p.y)
}
impl Gesture {
    /// 正常放开采样最终物理坐标；捕获已转移仅取消，拖出文件不能在 Up 后才开始。
    fn release_placement(
        &mut self,
        point: (i32, i32),
        accepted_release: bool,
    ) -> Option<Placement> {
        if !accepted_release || matches!(self.kind, Kind::Export) {
            return None;
        }
        self.placement(point)
    }

    fn placement(&mut self, point: (i32, i32)) -> Option<Placement> {
        let (dx, dy) = (point.0 - self.start.0, point.1 - self.start.1);
        self.moved |= dx.abs().max(dy.abs()) >= self.threshold;
        if !self.moved {
            return None;
        }
        let mut p = self.bounds;
        match self.kind {
            Kind::Move => {
                p.x += dx;
                p.y += dy;
            }
            Kind::Resize { left, top } => {
                let rx = (p.width as f32 + if left { -dx } else { dx } as f32) / p.width as f32;
                let ry = (p.height as f32 + if top { -dy } else { dy } as f32) / p.height as f32;
                let ratio = if (rx - 1.).abs() > (ry - 1.).abs() {
                    rx
                } else {
                    ry
                };
                let maximum =
                    crate::pin_view::MAX_PIN_WINDOW_SIDE as f32 / p.width.max(p.height) as f32;
                let minimum = (24. / p.width.min(p.height) as f32).min(maximum);
                let ratio = ratio.clamp(minimum, maximum);
                p.width = (p.width as f32 * ratio).round() as u32;
                p.height = (p.height as f32 * ratio).round() as u32;
                if left {
                    p.x += self.bounds.width as i32 - p.width as i32;
                }
                if top {
                    p.y += self.bounds.height as i32 - p.height as i32;
                }
            }
            Kind::Export => {}
        }
        Some(p)
    }
}
impl PinView {
    pub(crate) fn begin_drag(&mut self, e: &MouseDownEvent, w: &mut Window, kind: Kind) -> bool {
        if self.saving || self.processing {
            return false;
        }
        if self.state.locked && !matches!(kind, Kind::Export) {
            return false;
        }
        if let Ok(bounds) = self.placement(w) {
            let gesture = Gesture {
                start: screen(e.position, w),
                bounds,
                kind,
                threshold: (3. * w.scale_factor()).ceil() as i32,
                moved: false,
            };
            unsafe {
                SetCapture(hwnd(w));
                if GetCapture() != hwnd(w) {
                    return false;
                }
            }
            self.gesture = Some(gesture);
            return true;
        }
        false
    }
    pub(crate) fn cancel_drag(&mut self, w: &Window) {
        self.gesture = None;
        self.post_drag_click.cancel();
        unsafe {
            if GetCapture() == hwnd(w) {
                let _ = ReleaseCapture();
            }
        }
    }
    pub(crate) fn finish_drag(
        &mut self,
        e: &MouseUpEvent,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut completed) = self.gesture else {
            return false;
        };
        // GPUI Windows 在派发 Up 前释放正常捕获；其它 HWND 接管不能提交旧终点。
        let owner = unsafe { GetCapture() };
        let accepted_release = owner.0.is_null() || owner == hwnd(w);
        let placement = completed.release_placement(screen(e.position, w), accepted_release);
        self.cancel_drag(w);
        self.post_drag_click
            .finish(accepted_release.then_some(completed));
        if let Some(placement) = placement {
            self.apply_drag_placement(completed, placement, w, cx);
        }
        accepted_release
    }
    fn apply_drag_placement(
        &mut self,
        gesture: Gesture,
        placement: Placement,
        w: &Window,
        cx: &mut Context<Self>,
    ) {
        let resize = matches!(gesture.kind, Kind::Resize { .. }).then(|| {
            (
                self.window_sizing.after_user_resize(
                    (gesture.bounds.width, gesture.bounds.height),
                    (placement.width, placement.height),
                ),
                crate::pin_zoom::projection(
                    self.window_image_size(),
                    (placement.width, placement.height),
                )
                .zoom,
            )
        });
        let validation = if let Some((sizing, zoom)) = resize {
            self.validate_presentation_geometry(placement, zoom, sizing)
        } else {
            self.validate_presentation_placement(placement)
        };
        if let Err(error) = validation {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        if let Some((sizing, zoom)) = resize {
            self.window_sizing = sizing;
            self.state.thumbnail = false;
            self.state.zoom = zoom;
        }
        self.request_placement(placement, w, cx);
        self.sync_toolbar(w, cx);
        cx.notify();
    }
    pub(crate) fn drag_move(
        &mut self,
        e: &MouseMoveEvent,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut g) = self.gesture else {
            return false;
        };
        if e.pressed_button != Some(MouseButton::Left) || unsafe { GetCapture() != hwnd(w) } {
            self.cancel_drag(w);
            cx.notify();
            return true;
        }
        let placement = g.placement(screen(e.position, w));
        self.gesture = Some(g);
        let Some(p) = placement else {
            return true;
        };
        if matches!(g.kind, Kind::Export) {
            // 输出请求保留捕获，后台准备期间仍能收到窗口外的松开事件。
            self.gesture = None;
            self.drag_out(w, cx);
        } else {
            self.apply_drag_placement(g, p, w, cx);
        }
        true
    }
    pub(crate) fn corner(&self, p: gpui::Point<gpui::Pixels>, w: &Window) -> Option<Kind> {
        if self.chrome {
            return None;
        }
        let size = w.viewport_size();
        let (x, y) = (f32::from(p.x), f32::from(p.y));
        let (right, bottom) = (f32::from(size.width) - x, f32::from(size.height) - y);
        ((x < 9. || right < 9.) && (y < 9. || bottom < 9.)).then_some(Kind::Resize {
            left: x < right,
            top: y < bottom,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_move_delta_preserves_bare_shift_and_rejects_other_modifiers_or_lock() {
        for bits in 0_u8..32 {
            let modifiers = gpui::Modifiers {
                control: bits & 1 != 0,
                alt: bits & 2 != 0,
                shift: bits & 4 != 0,
                platform: bits & 8 != 0,
                function: bits & 16 != 0,
            };
            for locked in [false, true] {
                for (key, unit) in [
                    ("left", (-1, 0)),
                    ("right", (1, 0)),
                    ("up", (0, -1)),
                    ("down", (0, 1)),
                ] {
                    let expected = if !locked && matches!(bits, 0 | 4) {
                        let step = if bits == 4 { 10 } else { 1 };
                        Some((unit.0 * step, unit.1 * step))
                    } else {
                        None
                    };
                    assert_eq!(
                        keyboard_move_delta(key, modifiers, locked),
                        expected,
                        "{key}, modifiers={bits:05b}, locked={locked}"
                    );
                }
                for key in ["c", "h", "escape", ""] {
                    assert_eq!(keyboard_move_delta(key, modifiers, locked), None);
                }
            }
        }
    }

    #[test]
    fn exact_capture_sub24_resize_is_bounded_and_retains_opposite_corner() {
        let limit = crate::pin_view::MAX_PIN_WINDOW_SIDE;
        for dimensions in [(1, limit), (limit, 1), (22, 100), (100, 23), (1, 1)] {
            let bounds = Placement {
                x: -1921,
                y: -701,
                width: dimensions.0,
                height: dimensions.1,
            };
            for delta in [(-100_000, -100_000), (100_000, 100_000)] {
                for (left, top) in [(false, false), (true, true), (true, false), (false, true)] {
                    let mut gesture = Gesture {
                        start: (0, 0),
                        bounds,
                        kind: Kind::Resize { left, top },
                        threshold: 3,
                        moved: false,
                    };
                    assert_eq!(gesture.placement((1, 1)), None);
                    let resized = gesture.placement(delta).unwrap();
                    assert!(resized.width >= 1 && resized.width <= limit);
                    assert!(resized.height >= 1 && resized.height <= limit);
                    assert_eq!(
                        if left {
                            resized.x + resized.width as i32
                        } else {
                            resized.x
                        },
                        if left {
                            bounds.x + bounds.width as i32
                        } else {
                            bounds.x
                        }
                    );
                    assert_eq!(
                        if top {
                            resized.y + resized.height as i32
                        } else {
                            resized.y
                        },
                        if top {
                            bounds.y + bounds.height as i32
                        } else {
                            bounds.y
                        }
                    );
                    if dimensions == (1, limit) || dimensions == (limit, 1) {
                        assert_eq!((resized.width, resized.height), dimensions);
                        assert_eq!(
                            crate::pin_model::PinWindowSizing::ExactCapture
                                .after_user_resize(dimensions, (resized.width, resized.height)),
                            crate::pin_model::PinWindowSizing::ExactCapture
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn pin_drag_negative_screen_and_resize_keep_opposite_corner() {
        let p = Placement {
            x: -900,
            y: 40,
            width: 600,
            height: 300,
        };
        let mut g = Gesture {
            start: (-800, 100),
            bounds: p,
            kind: Kind::Move,
            threshold: 3,
            moved: false,
        };
        assert_eq!(g.placement((-799, 101)), None);
        assert_eq!(
            g.placement((-1000, 130)).unwrap(),
            Placement {
                x: -1100,
                y: 70,
                ..p
            }
        );
        let r = Gesture {
            kind: Kind::Resize {
                left: true,
                top: true,
            },
            ..g
        }
        .placement((-900, 50))
        .unwrap();
        assert_eq!((r.width, r.height), (700, 350));
        assert_eq!((r.x + r.width as i32, r.y + r.height as i32), (-300, 340));
    }

    #[test]
    fn pin_drag_release_keeps_final_pixel_and_return_to_start() {
        let bounds = Placement {
            x: -1921,
            y: -701,
            width: 601,
            height: 301,
        };
        let mut gesture = Gesture {
            start: (-1800, -600),
            bounds,
            kind: Kind::Move,
            threshold: 4,
            moved: false,
        };
        assert_eq!(gesture.placement((-1796, -596)).unwrap().x, -1917);
        assert_eq!(
            gesture.release_placement((-1795, -595), true),
            Some(Placement {
                x: -1916,
                y: -696,
                ..bounds
            })
        );
        // 最后一次 Move 之后直接在起点松开，也应恢复精确原点且保留真拖动资格。
        assert_eq!(gesture.release_placement(gesture.start, true), Some(bounds));
        assert!(gesture.moved);
    }

    #[test]
    fn pin_drag_release_without_move_uses_threshold_and_physical_endpoint() {
        for threshold in [3, 4, 5, 6, 9] {
            let mut gesture = Gesture {
                start: (-1919, -700),
                bounds: Placement {
                    x: -1921,
                    y: -701,
                    width: 22,
                    height: 100,
                },
                kind: Kind::Move,
                threshold,
                moved: false,
            };
            assert_eq!(
                gesture.release_placement((-1919 + threshold - 1, -700), true),
                None
            );
            assert!(!gesture.moved);
            let placement = gesture
                .release_placement((-1919 + threshold, -701), true)
                .unwrap();
            assert_eq!((placement.x, placement.y), (-1921 + threshold, -702));
            assert_eq!((placement.width, placement.height), (22, 100));
        }
    }

    #[test]
    fn pin_resize_release_keeps_final_size_and_all_opposite_corners() {
        for (left, top) in [(false, false), (true, true), (true, false), (false, true)] {
            let bounds = Placement {
                x: -900,
                y: -500,
                width: 600,
                height: 300,
            };
            let mut gesture = Gesture {
                start: (-800, -400),
                bounds,
                kind: Kind::Resize { left, top },
                threshold: 4,
                moved: false,
            };
            let direction = if left { -1 } else { 1 };
            assert_eq!(
                gesture
                    .placement((-800 + direction * 100, -400))
                    .unwrap()
                    .width,
                700
            );
            let placement = gesture
                .release_placement((-800 + direction * 102, -400), true)
                .unwrap();
            assert_eq!((placement.width, placement.height), (702, 351));
            assert_eq!(
                placement.x + if left { placement.width as i32 } else { 0 },
                bounds.x + if left { bounds.width as i32 } else { 0 }
            );
            assert_eq!(
                placement.y + if top { placement.height as i32 } else { 0 },
                bounds.y + if top { bounds.height as i32 } else { 0 }
            );
        }
    }

    #[test]
    fn pin_drag_release_rejects_transferred_capture_and_late_export() {
        for kind in [
            Kind::Move,
            Kind::Resize {
                left: true,
                top: true,
            },
            Kind::Export,
        ] {
            let mut gesture = Gesture {
                start: (-1800, -140),
                bounds: Placement {
                    x: -1900,
                    y: -240,
                    width: 640,
                    height: 360,
                },
                kind,
                threshold: 4,
                moved: false,
            };
            assert_eq!(gesture.release_placement((-1700, -40), false), None);
            assert!(!gesture.moved);
            if matches!(kind, Kind::Export) {
                assert_eq!(gesture.release_placement((-1700, -40), true), None);
                assert!(!gesture.moved);
            }
        }
    }

    #[test]
    fn resize_stops_before_native_swapchain_side_limit() {
        let mut gesture = Gesture {
            start: (0, 0),
            bounds: Placement {
                x: -1920,
                y: 0,
                width: 1280,
                height: 800,
            },
            kind: Kind::Resize {
                left: false,
                top: false,
            },
            threshold: 3,
            moved: false,
        };
        let resized = gesture.placement((100_000, 100_000)).unwrap();
        assert!(resized.width <= crate::pin_view::MAX_PIN_WINDOW_SIDE);
        assert!(resized.height <= crate::pin_view::MAX_PIN_WINDOW_SIDE);
        assert_eq!((resized.x, resized.y), (-1920, 0));
    }
}

#[cfg(test)]
mod post_drag_click_tests {
    use super::*;

    fn gesture(kind: Kind) -> Gesture {
        Gesture {
            start: (-1800, -140),
            bounds: Placement {
                x: -1900,
                y: -240,
                width: 640,
                height: 360,
            },
            kind,
            threshold: 4,
            moved: false,
        }
    }

    fn down(button: MouseButton, click_count: usize) -> MouseDownEvent {
        MouseDownEvent {
            button,
            click_count,
            ..Default::default()
        }
    }

    #[test]
    fn moved_pin_return_to_start_suppresses_only_immediate_double() {
        let mut g = gesture(Kind::Move);
        assert_eq!(g.placement((-1797, -141)), None);
        assert_eq!(g.placement((-1796, -140)).unwrap().x, -1896);
        // 越过阈值后返回起点仍是真拖动，位置亦能恢复原点。
        assert_eq!(g.placement(g.start), Some(g.bounds));
        let mut click = PostDragClick::default();
        click.finish(Some(g));
        click.mouse_down(&down(MouseButton::Left, 2));
        assert!(click.suppress_double_click());
        click.mouse_down(&down(MouseButton::Left, 2));
        assert!(!click.suppress_double_click());
    }

    #[test]
    fn no_move_cancel_resize_and_export_do_not_suppress_double() {
        let mut click = PostDragClick::default();
        let mut stationary = gesture(Kind::Move);
        assert_eq!(stationary.placement((-1799, -139)), None);
        click.finish(Some(stationary));
        click.mouse_down(&down(MouseButton::Left, 2));
        assert!(!click.suppress_double_click());
        for kind in [
            Kind::Resize {
                left: true,
                top: false,
            },
            Kind::Export,
        ] {
            let mut g = gesture(kind);
            assert!(g.placement((-1780, -120)).is_some());
            click.finish(Some(g));
            click.mouse_down(&down(MouseButton::Left, 2));
            assert!(!click.suppress_double_click());
        }
        let mut moved = gesture(Kind::Move);
        assert!(moved.placement((-1780, -120)).is_some());
        click.finish(Some(moved));
        click.cancel();
        click.mouse_down(&down(MouseButton::Left, 2));
        assert!(!click.suppress_double_click());
    }

    #[test]
    fn next_down_consumes_guard_before_other_routes_or_new_double() {
        let mut moved = gesture(Kind::Move);
        assert!(moved.placement((-1810, -150)).is_some());
        let mut click = PostDragClick::default();
        for event in [
            down(MouseButton::Right, 1),
            down(MouseButton::Middle, 1),
            down(MouseButton::Left, 1),
            MouseDownEvent {
                modifiers: gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
                ..down(MouseButton::Left, 2)
            },
            MouseDownEvent {
                modifiers: gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
                ..down(MouseButton::Left, 1)
            },
        ] {
            click.finish(Some(moved));
            click.mouse_down(&event);
            // Ctrl/文字/取色仍先走宿主既有分支；无论首 Down 用不用标记，下一次不遗留。
            click.mouse_down(&down(MouseButton::Left, 2));
            assert!(!click.suppress_double_click());
        }
        click.finish(Some(moved));
        click.mouse_down(&down(MouseButton::Left, 1));
        assert!(!click.suppress_double_click());
        click.mouse_down(&down(MouseButton::Left, 2));
        assert!(!click.suppress_double_click());
    }
}

#[cfg(test)]
mod ocr_alt_move_tests {
    use super::*;

    #[test]
    fn ocr_alt_move_route_preserves_control_double_click_and_other_buttons() {
        let event = MouseDownEvent {
            button: MouseButton::Left,
            click_count: 1,
            modifiers: gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(ocr_alt_move_requested(&event));
        assert!(ocr_alt_move_requested(&MouseDownEvent {
            modifiers: gpui::Modifiers {
                alt: true,
                shift: true,
                ..Default::default()
            },
            ..event.clone()
        }));
        for modifiers in [
            gpui::Modifiers::default(),
            gpui::Modifiers {
                alt: true,
                control: true,
                ..Default::default()
            },
        ] {
            assert!(!ocr_alt_move_requested(&MouseDownEvent {
                modifiers,
                ..event.clone()
            }));
        }
        assert!(!ocr_alt_move_requested(&MouseDownEvent {
            click_count: 2,
            ..event.clone()
        }));
        for button in [MouseButton::Right, MouseButton::Middle] {
            assert!(!ocr_alt_move_requested(&MouseDownEvent {
                button,
                ..event.clone()
            }));
        }
    }

    #[test]
    fn ocr_move_cursor_uses_system_arrow_and_returns_to_text_after_end() {
        let gesture = Gesture {
            start: (-1800, -140),
            bounds: Placement {
                x: -1900,
                y: -240,
                width: 640,
                height: 360,
            },
            kind: Kind::Move,
            threshold: 3,
            moved: false,
        };
        let alt = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(ocr_cursor(None, alt, true), gpui::CursorStyle::Arrow);
        assert_eq!(ocr_cursor(None, alt, false), gpui::CursorStyle::IBeam);
        assert_eq!(
            ocr_cursor(
                None,
                gpui::Modifiers {
                    control: true,
                    ..alt
                },
                true
            ),
            gpui::CursorStyle::IBeam
        );
        assert_eq!(
            ocr_cursor(Some(&gesture), gpui::Modifiers::default(), true),
            gpui::CursorStyle::Arrow
        );
        assert_eq!(
            ocr_cursor(None, gpui::Modifiers::default(), true),
            gpui::CursorStyle::IBeam
        );
        let export = Gesture {
            kind: Kind::Export,
            ..gesture
        };
        assert_eq!(
            ocr_cursor(
                Some(&export),
                gpui::Modifiers {
                    control: true,
                    ..alt
                },
                true
            ),
            gpui::CursorStyle::IBeam
        );
    }
}
