//! 原生消息到 GPUI 的有界快照；修饰键在收到消息时保存，不改系统键盘状态。
use crate::{
    capture_double_click::ClickSeed,
    geometry::{Rect, ScreenRect},
    startup_preview::PresentedWindowPreview,
};
use gpui::{
    point, px, Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta,
    ScrollWheelEvent, TouchPhase,
};
use std::time::Instant;
use windows::Win32::{
    System::SystemServices::{MK_LBUTTON, MK_RBUTTON},
    UI::{
        Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL, VK_LBUTTON, VK_RBUTTON},
        WindowsAndMessaging::*,
    },
};

pub(crate) type RawInput = (u32, usize, isize);

/// 已在原生消费的切换不再执行；交接后收到的按键只借事件当时快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeMonitorSnap {
    Applied,
    Pending {
        global_point: Option<(i32, i32)>,
        shown_preview: Option<Rect>,
    },
}

/// 与编辑界面一致：Ctrl+A 与 Ctrl+Shift+A 都吸附显示器。
pub(crate) fn monitor_snap_down(raw: RawInput, modifiers: Modifiers) -> bool {
    raw.0 == WM_KEYDOWN
        && raw.1 == 0x41
        && modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextShortcut {
    Region,
    Control,
}

fn text_shortcut(raw: RawInput, modifiers: Modifiers) -> Option<TextShortcut> {
    if raw.0 != WM_KEYDOWN
        || modifiers.control
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
    {
        return None;
    }
    match (raw.1, modifiers.shift) {
        (0x52, false) => Some(TextShortcut::Region),
        (0x43, true) => Some(TextShortcut::Control),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct MouseMotion {
    min: (i32, i32),
    max: (i32, i32),
    editing_button_seen: bool,
}

impl MouseMotion {
    fn from_raw(value: isize, key: usize, bounds: ScreenRect) -> Self {
        let point = (
            (value as u16 as i16 as i32).clamp(0, bounds.width as i32),
            ((value >> 16) as u16 as i16 as i32).clamp(0, bounds.height as i32),
        );
        Self {
            min: point,
            max: point,
            editing_button_seen: key & (MK_LBUTTON.0 | MK_RBUTTON.0) as usize != 0,
        }
    }

    fn inherit(&mut self, previous: Self) {
        self.editing_button_seen |= previous.editing_button_seen;
        self.min = (
            self.min.0.min(previous.min.0),
            self.min.1.min(previous.min.1),
        );
        self.max = (
            self.max.0.max(previous.max.0),
            self.max.1.max(previous.max.1),
        );
    }
}

/// 圆形预览容差的证据只保留真实最远点，矩形包络的角不能充当走过的点。
#[derive(Clone, Copy)]
struct PreviewMotion {
    origin: (i32, i32),
    farthest: (i32, i32),
    gesture_id: u64,
}
#[derive(Default)]
pub(crate) struct PreviewMotionSource {
    last_gesture: u64,
    active: Option<(u64, (i32, i32))>,
}
impl PreviewMotionSource {
    pub(crate) fn reset(&mut self) {
        self.active = None;
    }
}
impl PreviewMotion {
    fn distance_squared(self, point: (i32, i32)) -> i128 {
        let dx = i128::from(point.0) - i128::from(self.origin.0);
        let dy = i128::from(point.1) - i128::from(self.origin.1);
        dx * dx + dy * dy
    }
    fn inherit(&mut self, previous: Self) {
        if self.origin == previous.origin
            && self.gesture_id == previous.gesture_id
            && self.distance_squared(previous.farthest) > self.distance_squared(self.farthest)
        {
            self.farthest = previous.farthest;
        }
    }
    fn crossed(self, start: (i32, i32), scale: f32) -> bool {
        self.origin == start
            && crate::capture_monitor_snap::preview_dragged(start, self.farthest, scale)
    }
}

/// 一次左键手势曾跨过点击容差后，回到起点也不能重新成为点击。
#[derive(Default)]
pub(crate) struct ClickGesture {
    start: Option<(i32, i32)>,
    moved: bool,
}

impl ClickGesture {
    pub(crate) fn start(&mut self, point: (i32, i32)) {
        self.start = Some(point);
        self.moved = false;
    }

    pub(crate) fn observe_move(&mut self, event: &CapturedInput) {
        let Some(start) = self.start else {
            return;
        };
        if let Some(motion) = event.mouse_motion {
            self.moved |=
                moved_beyond_click(start, motion.min) || moved_beyond_click(start, motion.max);
        }
    }

    pub(crate) fn finish(&mut self, point: (i32, i32)) -> bool {
        let clicked = self
            .start
            .is_some_and(|start| !self.moved && !moved_beyond_click(start, point));
        self.clear();
        clicked
    }

    pub(crate) fn clear(&mut self) {
        self.start = None;
        self.moved = false;
    }
}

fn moved_beyond_click(start: (i32, i32), point: (i32, i32)) -> bool {
    (i64::from(point.0) - i64::from(start.0)).abs() >= 3
        || (i64::from(point.1) - i64::from(start.1)).abs() >= 3
}

/// 交接输入携带经过范围；取消边界不能伪装成鼠标移动或按键。
pub(crate) enum ReplayInput {
    Event(ReplayEvent),
    CancelPointerGesture,
    Output(crate::startup_output::StartupOutput),
}

pub(crate) struct ReplayEvent {
    pub(crate) event: PlatformInput,
    mouse_motion: Option<MouseMotion>,
    received_at: Instant,
    physical_point: (i32, i32),
    raw: RawInput,
    native_preview_middle_consumed: bool,
    native_preview_wheel_notches: Option<f32>,
    native_preview_hover: bool,
    native_preview_confirm_consumed: bool,
    native_text_consumed: bool,
    native_text_shortcut_seen: bool,
    native_monitor_snap: Option<NativeMonitorSnap>,
    preview_motion: Option<PreviewMotion>,
}

impl ReplayEvent {
    pub(crate) fn preview_gesture_id(&self) -> Option<u64> {
        self.preview_motion.map(|motion| motion.gesture_id)
    }
    pub(crate) fn crossed_preview_drag(&self, start: (i32, i32), scale: f32) -> bool {
        if let Some(motion) = self.preview_motion {
            return motion.crossed(start, scale);
        }
        matches!(self.raw.0, WM_MOUSEMOVE | WM_LBUTTONUP)
            && crate::capture_monitor_snap::preview_dragged(
                start,
                crate::startup_preview::client_point(self.raw.2),
                scale,
            )
    }
    pub(crate) fn native_monitor_snap(&self) -> Option<NativeMonitorSnap> {
        self.native_monitor_snap
    }
    /// 原生无确认选区时保留的滚轮，只能重放为预览父子导航，不能落入参数调整。
    pub(crate) fn native_preview_wheel_notches(&self) -> Option<f32> {
        self.native_preview_wheel_notches
    }
    pub(crate) fn native_preview_hover(&self) -> bool {
        self.native_preview_hover
    }

    /// 该真实 Middle 已成为 typed 输出；重放只执行点击边界，不再派发任一 Pin 分支。
    pub(crate) fn native_preview_middle_consumed(&self) -> bool {
        self.native_preview_middle_consumed
    }
    pub(crate) fn native_preview_confirm_consumed(&self) -> bool {
        self.native_preview_confirm_consumed
    }
    pub(crate) fn native_text_consumed(&self) -> bool {
        self.native_text_consumed
    }
    pub(crate) fn native_text_shortcut_seen(&self) -> bool {
        self.native_text_shortcut_seen
    }
    /// 与旧 prepare_click 的真实失效消息表一致；LeftUp/hover/ModifiersChanged 不断链。
    pub(crate) fn breaks_click_sequence(&self) -> bool {
        breaks_click_sequence(self.raw)
    }

    /// 保留 prepare_click 消费的当前 LeftDown 证明；其余事件不留下待派发上下文。
    pub(crate) fn prepare_canvas_click(
        &mut self,
        seed: &mut Option<ClickSeed>,
    ) -> Option<crate::capture_double_click::CanvasClickReplay> {
        let context = matches!(
            &self.event,
            PlatformInput::MouseDown(down) if down.button == MouseButton::Left
        )
        .then_some(crate::capture_double_click::CanvasClickReplay {
            seed: *seed,
            physical_point: self.physical_point,
            received_at: self.received_at,
        });
        self.prepare_click(seed);
        context
    }

    /// 使用消息收到时的时间和物理坐标；后台重放耗时不会使真实双击过期。
    pub(crate) fn prepare_click(&mut self, seed: &mut Option<ClickSeed>) {
        if let PlatformInput::MouseDown(down) = &mut self.event {
            let previous = seed.take();
            if down.button == MouseButton::Left {
                if let Some(previous) = previous {
                    down.click_count = previous.click_count(
                        down.click_count,
                        self.physical_point,
                        self.received_at,
                    );
                }
            }
        } else if breaks_click_sequence(self.raw) {
            *seed = None;
        }
    }

    /// 只有 GPUI 接受了左键按下才应用经过范围，避免悬停或被控件消费的 Down 污染手势。
    pub(crate) fn crossed_click_tolerance(&self, start: Option<(i32, i32)>) -> bool {
        match (start, self.mouse_motion) {
            (Some(start), Some(motion)) => {
                moved_beyond_click(start, motion.min) || moved_beyond_click(start, motion.max)
            }
            _ => false,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CapturedInput {
    pub raw: RawInput,
    pub modifiers: Modifiers,
    pub received_at: Instant,
    keystroke: Option<Keystroke>,
    capslock: bool,
    bounds: ScreenRect,
    wheel: (u32, u32),
    mouse_motion: Option<MouseMotion>,
    native_preview: Option<PresentedWindowPreview>,
    native_confirmed_selection: Option<PresentedWindowPreview>,
    native_preview_middle_consumed: bool,
    native_preview_wheel_notches: Option<f32>,
    native_preview_hover: bool,
    native_preview_confirm_consumed: bool,
    preview_confirm_mouse_held: bool,
    native_text_consumed: bool,
    monitor_snap_global_point: Option<(i32, i32)>,
    monitor_snap_shown_preview: Option<Rect>,
    native_monitor_snap: Option<NativeMonitorSnap>,
    preview_motion: Option<PreviewMotion>,
}

impl CapturedInput {
    #[cfg(test)]
    pub(crate) fn for_test(
        raw: RawInput,
        bounds: ScreenRect,
        wheel: (u32, u32),
        modifiers: Modifiers,
    ) -> Self {
        Self {
            raw,
            modifiers,
            received_at: Instant::now(),
            keystroke: None,
            capslock: false,
            bounds,
            wheel,
            mouse_motion: (raw.0 == WM_MOUSEMOVE)
                .then(|| MouseMotion::from_raw(raw.2, raw.1, bounds)),
            native_preview: None,
            native_confirmed_selection: None,
            native_preview_middle_consumed: false,
            native_preview_wheel_notches: None,
            native_preview_hover: false,
            native_preview_confirm_consumed: false,
            preview_confirm_mouse_held: false,
            native_text_consumed: false,
            monitor_snap_global_point: None,
            monitor_snap_shown_preview: None,
            native_monitor_snap: None,
            preview_motion: None,
        }
    }

    pub(crate) fn keystroke(&self) -> Option<&Keystroke> {
        self.keystroke.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn with_test_monitor_snap_point(mut self, point: Option<(i32, i32)>) -> Self {
        self.monitor_snap_global_point = point;
        self
    }
    pub(crate) fn monitor_snap_point(&self) -> Option<(i32, i32)> {
        self.monitor_snap_global_point
    }
    pub(crate) fn monitor_snap_shown_preview(&self) -> Option<Rect> {
        self.monitor_snap_shown_preview
    }
    pub(crate) fn set_presented_monitor_preview(
        &mut self,
        preview: Option<PresentedWindowPreview>,
    ) {
        self.monitor_snap_shown_preview = monitor_snap_down(self.raw, self.modifiers)
            .then(|| {
                preview
                    .filter(|receipt| receipt.serial != 0)
                    .map(|receipt| receipt.rect)
            })
            .flatten();
    }
    pub(crate) fn retain_native_monitor_snap(&mut self, applied: bool) {
        self.native_monitor_snap = Some(if applied {
            NativeMonitorSnap::Applied
        } else {
            NativeMonitorSnap::Pending {
                global_point: self.monitor_snap_global_point,
                shown_preview: self.monitor_snap_shown_preview,
            }
        });
    }
    pub(crate) fn native_monitor_snap(&self) -> Option<NativeMonitorSnap> {
        self.native_monitor_snap
    }
    #[cfg(test)]
    pub(crate) fn with_test_mouse_edit_held(mut self) -> Self {
        self.preview_confirm_mouse_held = true;
        self
    }

    /// 由原生队列在输出输入入队时复制已呈现 receipt，后来的 Paint 不得回填。
    pub(crate) fn set_presented_window_preview(&mut self, preview: Option<PresentedWindowPreview>) {
        self.native_preview = if matches!(self.raw.0, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK)
            || self.preview_confirm_down()
            || self.text_shortcut_down().is_some()
        {
            preview
        } else {
            None
        };
    }
    pub(crate) fn presented_window_preview(&self) -> Option<PresentedWindowPreview> {
        self.native_preview
    }

    /// 确认范围与自动预览独立，旧 confirmed frame 不能被无 crop 预览借用。
    pub(crate) fn set_presented_confirmed_selection(
        &mut self,
        receipt: Option<PresentedWindowPreview>,
    ) {
        self.native_confirmed_selection = if matches!(self.raw.0, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK)
        {
            receipt
        } else {
            None
        };
    }
    pub(crate) fn presented_confirmed_selection(&self) -> Option<PresentedWindowPreview> {
        self.native_confirmed_selection
    }

    /// Runtime 接受 typed 范围或确认原生输入归属后标记；撤销输出不还原原输入。
    pub(crate) fn consume_native_preview_middle(&mut self) {
        if matches!(self.raw.0, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK) {
            self.native_preview_middle_consumed = true;
        }
    }
    pub(crate) fn preview_confirm_down(&self) -> bool {
        self.raw.0 == WM_KEYDOWN
            && self.raw.1 == 0x0d
            && self.raw.2 & (1 << 30) == 0
            && !self.modifiers.modified()
    }
    pub(crate) fn preview_confirm_mouse_held(&self) -> bool {
        self.preview_confirm_mouse_held
    }
    pub(crate) fn consume_native_preview_confirm(&mut self) {
        if self.preview_confirm_down() {
            self.native_preview_confirm_consumed = true;
        }
    }
    pub(crate) fn native_preview_confirm_consumed(&self) -> bool {
        self.native_preview_confirm_consumed
    }

    pub(crate) fn text_shortcut_down(&self) -> Option<TextShortcut> {
        (self.raw.2 & (1 << 30) == 0)
            .then(|| text_shortcut(self.raw, self.modifiers))
            .flatten()
    }
    pub(crate) fn consume_native_text(&mut self) {
        if self.text_shortcut_down().is_some() {
            self.native_text_consumed = true;
        }
    }
    pub(crate) fn native_text_consumed(&self) -> bool {
        self.native_text_consumed
    }

    /// 连续 Move 的旧按键状态不能因最终 hover 替换而丢失。
    pub(crate) fn deferred_edit_button_seen(&self) -> bool {
        self.mouse_motion
            .is_some_and(|motion| motion.editing_button_seen)
    }
    pub(crate) fn crossed_preview_drag(&self, start: (i32, i32), scale: f32) -> bool {
        if let Some(motion) = self.preview_motion {
            return motion.crossed(start, scale);
        }
        matches!(self.raw.0, WM_MOUSEMOVE | WM_LBUTTONUP)
            && crate::capture_monitor_snap::preview_dragged(
                start,
                crate::startup_preview::client_point(self.raw.2),
                scale,
            )
    }
    pub(crate) fn track_preview_motion(&mut self, source: &mut PreviewMotionSource) {
        match self.raw.0 {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                source.active = source.last_gesture.checked_add(1).map(|gesture| {
                    source.last_gesture = gesture;
                    (gesture, crate::startup_preview::client_point(self.raw.2))
                });
                self.preview_motion = source.active.map(|(gesture_id, origin)| PreviewMotion {
                    gesture_id,
                    origin,
                    farthest: origin,
                });
            }
            WM_MOUSEMOVE | WM_LBUTTONUP => {
                self.preview_motion = source.active.map(|(gesture_id, origin)| PreviewMotion {
                    gesture_id,
                    origin,
                    farthest: crate::startup_preview::client_point(self.raw.2),
                });
                if self.raw.0 == WM_LBUTTONUP {
                    source.reset();
                }
            }
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_CAPTURECHANGED | WM_CLOSE => source.reset(),
            _ => {}
        }
        if monitor_snap_down(self.raw, self.modifiers)
            || crate::capture_cursor_nudge::NativeArrowNudge::loses_input(self.raw)
        {
            source.reset();
        }
    }
    #[cfg(test)]
    pub(crate) fn with_test_preview_motion_origin(mut self, start: (i32, i32)) -> Self {
        self.track_preview_motion(&mut PreviewMotionSource {
            last_gesture: 1,
            active: Some((1, start)),
        });
        self
    }

    /// 合并被连续 move 替换的经过范围，仍保留当前 raw 和修饰键快照。
    pub(crate) fn inherit_mouse_motion(&mut self, previous: &Self) {
        if self.raw.0 != WM_MOUSEMOVE
            || previous.raw.0 != WM_MOUSEMOVE
            || self.bounds != previous.bounds
        {
            return;
        }
        if let (Some(current), Some(previous)) = (self.mouse_motion.as_mut(), previous.mouse_motion)
        {
            current.inherit(previous);
        }
        if let (Some(current), Some(previous)) =
            (self.preview_motion.as_mut(), previous.preview_motion)
        {
            current.inherit(previous);
        }
    }

    /// 保留原 Raw/坐标/修饰键，另固定它在 Native 发生时属于无选区预览。
    pub(crate) fn retain_preview_wheel_for_handoff(&mut self) -> bool {
        // GPUI Lines 仅 Shift 使用水平轴；plain 横滚没有父子导航刻度。
        let distance = (self.raw.1 >> 16) as u16 as i16 as f32 / 120.;
        self.native_preview_wheel_notches = match self.raw.0 {
            WM_MOUSEWHEEL if self.modifiers.shift => (self.wheel.1 != 0).then_some(distance),
            WM_MOUSEWHEEL => (self.wheel.0 != 0).then_some(distance),
            WM_MOUSEHWHEEL if self.modifiers.shift => (self.wheel.1 != 0).then_some(-distance),
            _ => None,
        };
        self.native_preview_wheel_notches.is_some()
    }
    pub(crate) fn is_native_preview_wheel(&self) -> bool {
        self.native_preview_wheel_notches.is_some()
    }
    pub(crate) fn retain_preview_hover_for_handoff(&mut self) {
        self.native_preview_hover = self.raw.0 == WM_MOUSEMOVE;
    }
    pub(crate) fn is_native_preview_hover(&self) -> bool {
        self.native_preview_hover
    }

    /// 解码原生选区滚轮，匹配 GPUI 的 Lines 与 notches；系统禁用轴不产生输入。
    pub(crate) fn crop_wheel_notches(&self) -> Option<f32> {
        let distance = (self.raw.1 >> 16) as u16 as i16 as f32 / 120.;
        match self.raw.0 {
            WM_MOUSEWHEEL if self.modifiers.shift => (self.wheel.1 != 0).then_some(distance),
            WM_MOUSEWHEEL => (self.wheel.0 != 0).then_some(distance),
            WM_MOUSEHWHEEL if self.modifiers.shift => (self.wheel.1 != 0).then_some(-distance),
            _ => None,
        }
    }

    /// 只在自有 QA 窗口的显式标记消息上调用；不改键盘状态或用户输入。
    pub(crate) fn control_for_qa(&mut self) {
        self.modifiers.control = true;
        if let Some(key) = self.keystroke.as_mut() {
            key.modifiers.control = true;
        }
    }

    pub(crate) fn snapshot(raw: RawInput, bounds: ScreenRect, wheel: (u32, u32)) -> Self {
        let received_at = Instant::now();
        let key_message = matches!(raw.0, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP);
        let mouse_message = matches!(
            raw.0,
            WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONDBLCLK
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_RBUTTONDBLCLK
                | WM_RBUTTONUP
                | WM_MBUTTONDOWN
                | WM_MBUTTONDBLCLK
                | WM_MBUTTONUP
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        );
        let modifiers = if key_message || mouse_message {
            gpui::snapshot_windows_modifiers()
        } else {
            Modifiers::default()
        };
        let monitor_snap_global_point = if monitor_snap_down(raw, modifiers) {
            let mut cursor = windows::Win32::Foundation::POINT::default();
            unsafe { GetCursorPos(&mut cursor) }
                .ok()
                .map(|_| (cursor.x, cursor.y))
        } else {
            None
        };
        Self {
            raw,
            modifiers,
            received_at,
            keystroke: key_message
                .then(|| gpui::snapshot_windows_keystroke(raw.1 as u16, raw.2))
                .flatten(),
            capslock: key_message && unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0,
            bounds,
            wheel,
            mouse_motion: (raw.0 == WM_MOUSEMOVE)
                .then(|| MouseMotion::from_raw(raw.2, raw.1, bounds)),
            native_preview: None,
            native_confirmed_selection: None,
            native_preview_middle_consumed: false,
            native_preview_wheel_notches: None,
            native_preview_hover: false,
            native_preview_confirm_consumed: false,
            native_text_consumed: false,
            monitor_snap_global_point,
            monitor_snap_shown_preview: None,
            native_monitor_snap: None,
            preview_motion: None,
            preview_confirm_mouse_held: raw.0 == WM_KEYDOWN
                && (raw.1 == 0x0d
                    || text_shortcut(raw, modifiers).is_some()
                    || monitor_snap_down(raw, modifiers))
                && (unsafe { GetKeyState(VK_LBUTTON.0 as i32) } < 0
                    || unsafe { GetKeyState(VK_RBUTTON.0 as i32) } < 0),
        }
    }

    pub(crate) fn into_replay(self, scale: f32) -> Option<ReplayInput> {
        if self.raw.0 == WM_CAPTURECHANGED {
            return Some(ReplayInput::CancelPointerGesture);
        }
        let mouse_motion = self.mouse_motion;
        let received_at = self.received_at;
        let raw = self.raw;
        let native_preview_middle_consumed = self.native_preview_middle_consumed;
        let native_preview_wheel_notches = self.native_preview_wheel_notches;
        let native_preview_hover = self.native_preview_hover;
        let native_preview_confirm_consumed = self.native_preview_confirm_consumed;
        let native_text_consumed = self.native_text_consumed;
        let native_text_shortcut_seen = text_shortcut(raw, self.modifiers).is_some();
        let native_monitor_snap = self.native_monitor_snap;
        let preview_motion = self.preview_motion;
        let physical_point = (
            (raw.2 as u16 as i16 as i32).clamp(0, self.bounds.width as i32),
            ((raw.2 >> 16) as u16 as i16 as i32).clamp(0, self.bounds.height as i32),
        );
        self.platform_input(scale).map(|event| {
            ReplayInput::Event(ReplayEvent {
                event,
                mouse_motion,
                received_at,
                physical_point,
                raw,
                native_preview_middle_consumed,
                native_preview_wheel_notches,
                native_preview_hover,
                native_preview_confirm_consumed,
                native_text_consumed,
                native_text_shortcut_seen,
                native_monitor_snap,
                preview_motion,
            })
        })
    }

    pub(crate) fn platform_input(self, scale: f32) -> Option<PlatformInput> {
        let (message, key, value) = self.raw;
        let x = value as u16 as i16 as i32;
        let y = (value >> 16) as u16 as i16 as i32;
        let wheel_message = matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL);
        let (x, y) = if wheel_message {
            (
                x as i64 - self.bounds.left as i64,
                y as i64 - self.bounds.top as i64,
            )
        } else {
            (x as i64, y as i64)
        };
        let position = point(px(x as f32 / scale), px(y as f32 / scale));
        let modifiers = self.modifiers;
        let button = match message {
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP => MouseButton::Left,
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_RBUTTONUP => MouseButton::Right,
            WM_MBUTTONDOWN | WM_MBUTTONDBLCLK | WM_MBUTTONUP => MouseButton::Middle,
            _ => MouseButton::Left,
        };
        Some(match message {
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                if is_modifier(key as u16) {
                    PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                        modifiers,
                        capslock: Capslock { on: self.capslock },
                        physical_key: gpui::decode_windows_modifier_key_event(message, key, value),
                    })
                } else if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
                    PlatformInput::KeyDown(KeyDownEvent {
                        keystroke: self.keystroke?,
                        is_held: value & (1 << 30) != 0,
                    })
                } else {
                    PlatformInput::KeyUp(KeyUpEvent {
                        keystroke: self.keystroke?,
                    })
                }
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count: if matches!(
                    message,
                    WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK
                ) {
                    2
                } else {
                    1
                },
                first_mouse: false,
            }),
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers,
                click_count: 1,
            }),
            WM_MOUSEMOVE => PlatformInput::MouseMove(MouseMoveEvent {
                position,
                modifiers,
                pressed_button: if key & 1 != 0 {
                    Some(MouseButton::Left)
                } else if key & 2 != 0 {
                    Some(MouseButton::Right)
                } else if key & 0x10 != 0 {
                    Some(MouseButton::Middle)
                } else {
                    None
                },
            }),
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let distance = (key >> 16) as u16 as i16 as f32 / 120.;
                let delta = if message == WM_MOUSEHWHEEL {
                    point(-distance * self.wheel.1 as f32, 0.)
                } else if modifiers.shift {
                    point(distance * self.wheel.1 as f32, 0.)
                } else {
                    point(0., distance * self.wheel.0 as f32)
                };
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    modifiers,
                    delta: ScrollDelta::Lines(delta),
                    touch_phase: TouchPhase::Moved,
                })
            }
            // WM_CHAR 与 Keystroke.key_char 重复；原生阶段没有文本/IME 输入区。
            _ => return None,
        })
    }
}

/// 除下一次左键按下外，编辑输入会终止原生首击；悬停和修饰键不打断。
pub(crate) fn breaks_click_sequence((message, key, _): RawInput) -> bool {
    matches!(
        message,
        WM_RBUTTONDOWN
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONDBLCLK
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
            | WM_CLOSE
    ) || matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
        && !is_modifier(key as u16)
}

pub(crate) fn is_modifier(key: u16) -> bool {
    matches!(key, 0x10..=0x12 | 0x14 | 0x5b | 0x5c | 0xa0..=0xa5)
}

pub(crate) fn wheel_settings() -> (u32, u32) {
    let (mut lines, mut chars) = (3_u32, 3_u32);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&mut lines as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let _ = SystemParametersInfoW(
            SPI_GETWHEELSCROLLCHARS,
            0,
            Some((&mut chars as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    // GPUI 同样忽略 page-scroll sentinel；0 表示系统禁用滚轮，不擅自改成 1。
    if lines == u32::MAX {
        lines = 3;
    }
    if chars == u32::MAX {
        chars = 3;
    }
    (lines, chars)
}

#[cfg(test)]
mod monitor_snap_input_tests {
    use super::*;
    fn bounds() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: -240,
            width: 640,
            height: 480,
        }
    }
    fn replay(event: CapturedInput) -> ReplayEvent {
        let Some(ReplayInput::Event(replay)) = event.into_replay(1.5) else {
            panic!("真实事件应保留快照");
        };
        replay
    }

    #[test]
    fn monitor_snap_pending_preserves_missing_pointer_shown_receipt_and_repeat() {
        for global_point in [None, Some((-1800, -100))] {
            let rect = Rect::full(320, 480);
            let mut event = CapturedInput::for_test(
                (WM_KEYDOWN, 0x41, 1 << 30),
                bounds(),
                (3, 3),
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
            )
            .with_test_monitor_snap_point(global_point);
            event.keystroke = Some(Keystroke::parse("ctrl-shift-a").unwrap());
            event.set_presented_monitor_preview(Some(PresentedWindowPreview { rect, serial: 9 }));
            event.retain_native_monitor_snap(false);
            let saved = event.clone();
            event.monitor_snap_global_point = Some((999, 999));
            event.set_presented_monitor_preview(None);
            let replay = replay(saved);
            assert_eq!(
                replay.native_monitor_snap(),
                Some(NativeMonitorSnap::Pending {
                    global_point,
                    shown_preview: Some(rect)
                })
            );
            let PlatformInput::KeyDown(key) = replay.event else {
                panic!("应保持KeyDown");
            };
            assert!(key.is_held);
            assert!(key.keystroke.modifiers.control && key.keystroke.modifiers.shift);
        }
    }

    #[test]
    fn monitor_snap_applied_marker_survives_replay_and_alt_is_not_a_gesture() {
        let mut event = CapturedInput::for_test(
            (WM_KEYDOWN, 0x41, 0),
            bounds(),
            (3, 3),
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
        event.keystroke = Some(Keystroke::parse("ctrl-a").unwrap());
        event.retain_native_monitor_snap(true);
        assert_eq!(
            replay(event).native_monitor_snap(),
            Some(NativeMonitorSnap::Applied)
        );
        for (raw, modifiers, accepted) in [
            (
                (WM_KEYDOWN, 0x41, 0),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                true,
            ),
            (
                (WM_KEYDOWN, 0x41, 1 << 30),
                Modifiers {
                    control: true,
                    shift: true,
                    ..Default::default()
                },
                true,
            ),
            (
                (WM_KEYDOWN, 0x41, 0),
                Modifiers {
                    control: true,
                    alt: true,
                    ..Default::default()
                },
                false,
            ),
            (
                (WM_SYSKEYDOWN, 0x41, 0),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
            ),
            (
                (WM_KEYUP, 0x41, 0),
                Modifiers {
                    control: true,
                    ..Default::default()
                },
                false,
            ),
            ((WM_KEYDOWN, 0x41, 0), Modifiers::default(), false),
        ] {
            assert_eq!(monitor_snap_down(raw, modifiers), accepted);
        }
    }

    #[test]
    fn preview_motion_merges_real_farthest_points_and_rejects_another_down_origin() {
        let moved = |x: i16, y: i16, origin| {
            CapturedInput::for_test(
                (
                    WM_MOUSEMOVE,
                    1,
                    (u32::from(x as u16) | (u32::from(y as u16) << 16)) as isize,
                ),
                bounds(),
                (3, 3),
                Modifiers::default(),
            )
            .with_test_preview_motion_origin(origin)
        };
        let first = moved(5, 0, (0, 0));
        let mut next = moved(0, 5, (0, 0));
        next.inherit_mouse_motion(&first);
        assert!(!next.crossed_preview_drag((0, 0), 1.));
        let far = moved(6, 0, (0, 0));
        let mut returned = moved(0, 0, (0, 0));
        returned.inherit_mouse_motion(&next);
        assert!(!returned.crossed_preview_drag((0, 0), 1.));
        returned.inherit_mouse_motion(&far);
        assert!(replay(returned).crossed_preview_drag((0, 0), 1.));
        let mut other = moved(100, 100, (100, 100));
        other.inherit_mouse_motion(&far);
        assert!(!other.crossed_preview_drag((100, 100), 1.));
        assert!(!other.crossed_preview_drag((0, 0), 1.));
    }

    #[test]
    fn preview_motion_gesture_ids_reject_same_origin_aba_and_counter_exhaustion() {
        let captured = |message, x: i16| {
            CapturedInput::for_test(
                (message, 1, u32::from(x as u16) as isize),
                bounds(),
                (3, 3),
                Modifiers::default(),
            )
        };
        let mut source = PreviewMotionSource::default();
        let mut down = captured(WM_LBUTTONDOWN, 0);
        down.track_preview_motion(&mut source);
        let first_id = replay(down).preview_gesture_id().unwrap();
        let mut old_far = captured(WM_MOUSEMOVE, 6);
        old_far.track_preview_motion(&mut source);
        let mut up = captured(WM_LBUTTONUP, 0);
        up.track_preview_motion(&mut source);
        let mut down = captured(WM_LBUTTONDOWN, 0);
        down.track_preview_motion(&mut source);
        assert_ne!(replay(down).preview_gesture_id(), Some(first_id));
        let mut new_return = captured(WM_MOUSEMOVE, 0);
        new_return.track_preview_motion(&mut source);
        new_return.inherit_mouse_motion(&old_far);
        assert!(!new_return.crossed_preview_drag((0, 0), 1.));
        source.last_gesture = u64::MAX;
        let mut down = captured(WM_LBUTTONDOWN, 0);
        down.track_preview_motion(&mut source);
        assert_eq!(replay(down).preview_gesture_id(), None);
        assert!(source.active.is_none());
    }
}

#[cfg(test)]
mod early_selection_input_tests {
    use super::*;

    #[test]
    fn alt_modifier_replay_keeps_side_phase_repeat_and_the_last_modifier_snapshot() {
        use gpui::{ModifierKey, ModifierKeyEvent, ModifierKeyPhase};
        for (raw, modifiers, expected) in [
            (
                (WM_SYSKEYDOWN, 0x12, 0),
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                ModifierKeyEvent {
                    key: ModifierKey::LeftAlt,
                    phase: ModifierKeyPhase::Down,
                    is_repeat: false,
                },
            ),
            (
                (WM_SYSKEYDOWN, 0x12, (1 << 24) | (1 << 30)),
                Modifiers {
                    alt: true,
                    control: true,
                    ..Default::default()
                },
                ModifierKeyEvent {
                    key: ModifierKey::RightAlt,
                    phase: ModifierKeyPhase::Down,
                    is_repeat: true,
                },
            ),
            (
                (WM_KEYUP, 0xa5, 1 << 30),
                Modifiers::default(),
                ModifierKeyEvent {
                    key: ModifierKey::RightAlt,
                    phase: ModifierKeyPhase::Up,
                    is_repeat: false,
                },
            ),
            (
                (WM_SYSKEYUP, 0x12, (1 << 24) | (1 << 30) | (1 << 31)),
                Modifiers::default(),
                ModifierKeyEvent {
                    key: ModifierKey::RightAlt,
                    phase: ModifierKeyPhase::Up,
                    is_repeat: false,
                },
            ),
            (
                (WM_KEYDOWN, 0xa4, 0),
                Modifiers {
                    alt: true,
                    ..Default::default()
                },
                ModifierKeyEvent {
                    key: ModifierKey::LeftAlt,
                    phase: ModifierKeyPhase::Down,
                    is_repeat: false,
                },
            ),
            (
                (WM_KEYDOWN, 0x14, 0),
                Modifiers::default(),
                ModifierKeyEvent {
                    key: ModifierKey::Other,
                    phase: ModifierKeyPhase::Down,
                    is_repeat: false,
                },
            ),
            (
                (WM_KEYDOWN, 0x10, 0),
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                ModifierKeyEvent {
                    key: ModifierKey::Other,
                    phase: ModifierKeyPhase::Down,
                    is_repeat: false,
                },
            ),
        ] {
            let captured = CapturedInput::for_test(raw, bounds(), (3, 3), modifiers);
            let replay = replay_event(captured, 1.25);
            let PlatformInput::ModifiersChanged(event) = replay.event else {
                panic!("原始修饰键必须按序进入 ModifiersChanged");
            };
            assert_eq!(event.physical_key, Some(expected));
            assert_eq!(event.modifiers, modifiers);
        }
        assert_eq!(
            gpui::decode_windows_modifier_key_event(WM_KEYDOWN, 0x41, 0),
            None
        );
        assert_eq!(
            gpui::decode_windows_modifier_key_event(WM_MOUSEMOVE, 0x12, 0),
            None
        );
    }

    #[test]
    fn plain_enter_copies_receipt_and_consumed_marker_without_using_key_flags_as_point() {
        let receipt = PresentedWindowPreview {
            rect: crate::geometry::Rect {
                x: 10,
                y: 5,
                width: 40,
                height: 30,
            },
            serial: 5,
        };
        let mut event = input((WM_KEYDOWN, 0x0d, 0x001c0001), bounds(), false, (3, 3));
        event.keystroke = Some(Keystroke::parse("enter").unwrap());
        event.set_presented_window_preview(Some(receipt));
        let frozen = event.clone();
        event.set_presented_window_preview(None);
        assert_eq!(frozen.presented_window_preview(), Some(receipt));
        event.consume_native_preview_confirm();
        let Some(ReplayInput::Event(mut replay)) = event.into_replay(1.25) else {
            panic!("真实 Enter 输入必须保留直到按序消费");
        };
        assert!(replay.native_preview_confirm_consumed());
        assert!(!replay.native_preview_middle_consumed());
        assert!(
            matches!(&replay.event, PlatformInput::KeyDown(key) if key.keystroke.key == "enter" && !key.is_held)
        );
        let mut seed = Some(ClickSeed::new(
            (10, 10),
            Instant::now(),
            std::time::Duration::from_millis(500),
            (4, 4),
        ));
        replay.prepare_click(&mut seed);
        assert!(seed.is_none());
    }

    #[test]
    fn preview_enter_excludes_modified_held_system_and_other_keys() {
        let receipt = PresentedWindowPreview {
            rect: crate::geometry::Rect::full(100, 60),
            serial: 1,
        };
        for (raw, modified) in [
            ((WM_KEYDOWN, 0x0d, 1 << 30), false),
            ((WM_KEYDOWN, 0x0d, 0), true),
            ((WM_SYSKEYDOWN, 0x0d, 0), false),
            ((WM_KEYUP, 0x0d, 0), false),
            ((WM_KEYDOWN, 0x20, 0), false),
        ] {
            let mut event = input(raw, bounds(), modified, (3, 3));
            event.set_presented_window_preview(Some(receipt));
            event.consume_native_preview_confirm();
            assert!(!event.preview_confirm_down());
            assert_eq!(event.presented_window_preview(), None);
            assert!(!event.native_preview_confirm_consumed());
        }
        let mut held_mouse = input((WM_KEYDOWN, 0x0d, 0), bounds(), false, (3, 3));
        held_mouse.preview_confirm_mouse_held = true;
        assert!(held_mouse.preview_confirm_down());
        assert!(held_mouse.preview_confirm_mouse_held());
    }

    fn bounds() -> ScreenRect {
        ScreenRect {
            left: -1000,
            top: -200,
            width: 100,
            height: 60,
        }
    }

    fn input(raw: RawInput, bounds: ScreenRect, shift: bool, wheel: (u32, u32)) -> CapturedInput {
        CapturedInput::for_test(
            raw,
            bounds,
            wheel,
            Modifiers {
                shift,
                ..Default::default()
            },
        )
    }

    fn moved(x: i16, y: i16) -> CapturedInput {
        let position = (x as u16 as isize) | ((y as u16 as isize) << 16);
        input((WM_MOUSEMOVE, 1, position), bounds(), false, (3, 3))
    }

    fn wheel(message: u32, delta: i16, shift: bool, settings: (u32, u32)) -> CapturedInput {
        input(
            (message, usize::from(delta as u16) << 16, 0),
            bounds(),
            shift,
            settings,
        )
    }

    fn replay_event(input: CapturedInput, scale: f32) -> ReplayEvent {
        match input.into_replay(scale).unwrap() {
            ReplayInput::Event(event) => event,
            ReplayInput::CancelPointerGesture => panic!("expected normal replay event"),
            ReplayInput::Output(_) => panic!("expected normal replay event"),
        }
    }

    #[test]
    fn replay_two_layer_coalescing_preserves_excursion_and_latest_snapshot() {
        // WindowProc 队列先合并，再由交接尾部 deferred 队列合并。
        let mut first_batch = moved(10, 40);
        first_batch.inherit_mouse_motion(&moved(10, 10));
        let mut second_batch = moved(10, 10);
        second_batch.inherit_mouse_motion(&moved(40, 10));
        second_batch.modifiers.control = true;
        second_batch.modifiers.shift = true;
        second_batch.inherit_mouse_motion(&first_batch);
        let replay = replay_event(second_batch, 1.25);
        assert!(replay.crossed_click_tolerance(Some((10, 10))));
        let PlatformInput::MouseMove(event) = replay.event else {
            panic!("expected mouse move");
        };
        assert_eq!(event.position, point(px(8.), px(8.)));
        assert!(event.modifiers.control && event.modifiers.shift);
        assert_eq!(event.pressed_button, Some(MouseButton::Left));
    }

    #[test]
    fn replay_tolerance_uses_physical_pixels_at_high_dpi() {
        for (x, y, crossed) in [
            (8, 12, false),
            (12, 8, false),
            (13, 10, true),
            (7, 10, true),
            (10, 13, true),
            (10, 7, true),
        ] {
            let replay = replay_event(moved(x, y), 2.);
            assert_eq!(replay.crossed_click_tolerance(Some((10, 10))), crossed);
        }
    }

    #[test]
    fn deferred_preview_two_layer_hover_coalescing_keeps_unknown_edit_buttons() {
        for editing_button in [MK_LBUTTON.0, MK_RBUTTON.0] {
            let editing = input(
                (WM_MOUSEMOVE, editing_button as usize, 0),
                bounds(),
                false,
                (3, 3),
            );
            let mut first_hover = input((WM_MOUSEMOVE, 0, 0), bounds(), false, (3, 3));
            first_hover.inherit_mouse_motion(&editing);
            let mut final_hover = input((WM_MOUSEMOVE, 0, 0), bounds(), false, (3, 3));
            final_hover.inherit_mouse_motion(&first_hover);
            assert_eq!(final_hover.raw.1, 0);
            assert!(final_hover.deferred_edit_button_seen());
        }
        let middle_held = input((WM_MOUSEMOVE, 0x10, 0), bounds(), false, (3, 3));
        let mut hover = input((WM_MOUSEMOVE, 0, 0), bounds(), false, (3, 3));
        hover.inherit_mouse_motion(&middle_held);
        assert!(!hover.deferred_edit_button_seen());
    }

    #[test]
    fn admitted_preview_middle_keeps_actual_input_and_consumes_click_seed_before_skip() {
        use std::time::Duration;
        let down_at = Instant::now();
        for message in [WM_MBUTTONDOWN, WM_MBUTTONDBLCLK] {
            let mut middle = input((message, 0x10, 0), bounds(), false, (3, 3));
            middle.consume_native_preview_middle();
            let mut replay = replay_event(middle.clone(), 1.25);
            assert!(replay.native_preview_middle_consumed());
            let mut seed = Some(ClickSeed::new(
                (0, 0),
                down_at,
                Duration::from_secs(1),
                (3, 3),
            ));
            replay.prepare_click(&mut seed);
            assert!(seed.is_none());
            let PlatformInput::MouseDown(down) = replay.event else {
                panic!("expected original Middle");
            };
            assert_eq!(down.button, MouseButton::Middle);
            assert_eq!(
                down.click_count,
                if message == WM_MBUTTONDBLCLK { 2 } else { 1 }
            );
            // later 编辑清除 output，也不会还原原始 Middle 的已消费标记。
            assert!(replay_event(middle, 1.25).native_preview_middle_consumed());
        }
    }

    #[test]
    fn consumed_preview_marker_cannot_suppress_other_buttons_or_unaccepted_middle() {
        for message in [WM_LBUTTONDOWN, WM_RBUTTONDOWN, WM_MOUSEMOVE] {
            let mut event = input((message, 0, 0), bounds(), false, (3, 3));
            event.consume_native_preview_middle();
            assert!(!replay_event(event, 1.).native_preview_middle_consumed());
        }
        assert!(!replay_event(
            input((WM_MBUTTONDOWN, 0x10, 0), bounds(), false, (3, 3)),
            1.
        )
        .native_preview_middle_consumed());
    }

    #[test]
    fn replay_without_accepted_down_cannot_mark_a_gesture() {
        let mut returned = moved(10, 10);
        returned.inherit_mouse_motion(&moved(50, 50));
        let replay = replay_event(returned, 1.);
        assert!(!replay.crossed_click_tolerance(None));
        assert!(replay.crossed_click_tolerance(Some((10, 10))));
        // 上次 Up/取消清空后，悬停不会为后续 Down 保留拖动标记。
        assert!(!replay.crossed_click_tolerance(None));
        let fresh = replay_event(moved(12, 10), 1.);
        assert!(!fresh.crossed_click_tolerance(Some((10, 10))));
    }

    #[test]
    fn replay_does_not_merge_motion_across_button_boundaries_or_desktop_bounds() {
        let far = moved(50, 50);
        let mut down = input((WM_LBUTTONDOWN, 1, 0), bounds(), false, (3, 3));
        down.inherit_mouse_motion(&far);
        let down = replay_event(down, 1.);
        assert!(!down.crossed_click_tolerance(Some((10, 10))));
        assert!(matches!(down.event, PlatformInput::MouseDown(_)));

        let mut new_bounds = bounds();
        new_bounds.left = 0;
        let mut changed = input(moved(10, 10).raw, new_bounds, false, (3, 3));
        changed.inherit_mouse_motion(&far);
        assert!(!replay_event(changed, 1.).crossed_click_tolerance(Some((10, 10))));
    }

    #[test]
    fn replay_capture_loss_is_a_typed_cancel_boundary() {
        let cancelled = input((WM_CAPTURECHANGED, 0, 123), bounds(), false, (3, 3));
        assert!(matches!(
            cancelled.into_replay(1.25),
            Some(ReplayInput::CancelPointerGesture)
        ));
        assert!(input((WM_CHAR, 65, 0), bounds(), false, (3, 3))
            .into_replay(1.)
            .is_none());
        let up = replay_event(input((WM_LBUTTONUP, 0, 0), bounds(), false, (3, 3)), 1.);
        assert!(!up.crossed_click_tolerance(Some((10, 10))));
        assert!(matches!(up.event, PlatformInput::MouseUp(_)));
    }

    #[test]
    fn native_preview_receipt_is_copied_with_middle_and_later_paint_cannot_fill_old_event() {
        let receipt = PresentedWindowPreview {
            rect: crate::geometry::Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 80,
            },
            serial: 7,
        };
        let mut middle = input(
            (WM_MBUTTONDOWN, 0x10, 40 | (50 << 16)),
            bounds(),
            false,
            (3, 1),
        );
        let before_paint = middle.clone();
        middle.set_presented_window_preview(Some(receipt));
        let at_enqueue = middle.clone();
        middle.set_presented_window_preview(Some(PresentedWindowPreview {
            serial: 8,
            ..receipt
        }));
        assert_eq!(before_paint.presented_window_preview(), None);
        assert_eq!(at_enqueue.presented_window_preview(), Some(receipt));
        assert_eq!(at_enqueue.raw, (WM_MBUTTONDOWN, 0x10, 40 | (50 << 16)));
        assert_eq!(at_enqueue.received_at, before_paint.received_at);
    }

    #[test]
    fn native_preview_metadata_is_only_for_middle_and_does_not_create_left_replay() {
        let receipt = PresentedWindowPreview {
            rect: crate::geometry::Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 80,
            },
            serial: 7,
        };
        let mut moved = moved(40, 50);
        moved.set_presented_window_preview(Some(receipt));
        assert_eq!(moved.presented_window_preview(), None);
        let mut middle = input(
            (WM_MBUTTONDBLCLK, 0x10, 40 | (50 << 16)),
            bounds(),
            true,
            (3, 1),
        );
        middle.set_presented_window_preview(Some(receipt));
        let replay = replay_event(middle, 2.);
        let PlatformInput::MouseDown(down) = replay.event else {
            panic!("expected middle replay");
        };
        assert_eq!(down.button, MouseButton::Middle);
        assert_eq!(down.click_count, 2);
        assert_eq!(down.position, point(px(20.), px(25.)));
        assert!(down.modifiers.shift);
    }

    #[test]
    fn replay_native_double_click_preserves_button_position_and_modifiers() {
        for (message, button) in [
            (WM_LBUTTONDBLCLK, MouseButton::Left),
            (WM_RBUTTONDBLCLK, MouseButton::Right),
            (WM_MBUTTONDBLCLK, MouseButton::Middle),
        ] {
            let mut captured = input((message, 1, moved(25, 30).raw.2), bounds(), true, (3, 3));
            captured.modifiers.control = true;
            let event = replay_event(captured, 1.25);
            let PlatformInput::MouseDown(event) = event.event else {
                panic!("double click must be a mouse down");
            };
            assert_eq!(event.click_count, 2);
            assert_eq!(event.button, button);
            assert_eq!(event.position, point(px(20.), px(24.)));
            assert!(event.modifiers.shift && event.modifiers.control);
        }
    }

    #[test]
    fn replay_cross_handoff_click_uses_receipt_time_and_physical_dpi_point_once() {
        let now = Instant::now();
        let interval = std::time::Duration::from_millis(500);
        let at = now - std::time::Duration::from_secs(10);
        let mut seed = Some(ClickSeed::new((25, 30), at, interval, (4, 4)));
        let mut captured = input(
            (WM_LBUTTONDOWN, 1, moved(29, 34).raw.2),
            bounds(),
            true,
            (3, 3),
        );
        captured.received_at = at + std::time::Duration::from_millis(499);
        let mut replay = replay_event(captured, 2.);
        replay.prepare_click(&mut seed);
        assert!(seed.is_none());
        let PlatformInput::MouseDown(down) = &replay.event else {
            panic!("expected down")
        };
        assert_eq!(down.click_count, 2);
        assert_eq!(down.position, point(px(14.5), px(17.)));
        assert!(down.modifiers.shift);
        let mut later = replay_event(
            input(
                (WM_LBUTTONDOWN, 1, moved(25, 30).raw.2),
                bounds(),
                false,
                (3, 3),
            ),
            2.,
        );
        later.prepare_click(&mut seed);
        let PlatformInput::MouseDown(down) = later.event else {
            panic!("expected down")
        };
        assert_eq!(down.click_count, 1);
    }

    #[test]
    fn replay_cross_handoff_expired_click_consumes_seed_without_confirmation() {
        let at = Instant::now();
        let interval = std::time::Duration::from_millis(500);
        let mut captured = input(
            (WM_LBUTTONDOWN, 1, moved(25, 30).raw.2),
            bounds(),
            false,
            (3, 3),
        );
        captured.received_at = at + interval;
        let mut seed = Some(ClickSeed::new((25, 30), at, interval, (4, 4)));
        let mut replay = replay_event(captured, 1.25);
        replay.prepare_click(&mut seed);
        assert!(seed.is_none());
        let PlatformInput::MouseDown(down) = replay.event else {
            panic!("expected down")
        };
        assert_eq!(down.click_count, 1);
    }

    #[test]
    fn replay_cross_handoff_hover_and_modifiers_preserve_seed_edit_inputs_clear_it() {
        let at = Instant::now();
        let make_seed = || {
            Some(ClickSeed::new(
                (25, 30),
                at,
                std::time::Duration::from_millis(500),
                (4, 4),
            ))
        };
        for raw in [
            (WM_MOUSEMOVE, 0, moved(25, 30).raw.2),
            (WM_LBUTTONUP, 0, 0),
            (WM_KEYDOWN, 0x10, 0),
            (WM_KEYUP, 0x11, 0),
        ] {
            let mut seed = make_seed();
            replay_event(input(raw, bounds(), false, (3, 3)), 1.).prepare_click(&mut seed);
            assert!(seed.is_some());
        }
        for raw in [
            (WM_RBUTTONDOWN, 0, 0),
            (WM_MBUTTONDBLCLK, 0, 0),
            (WM_MOUSEWHEEL, 120 << 16, 0),
            (WM_MOUSEHWHEEL, 120 << 16, 0),
            (WM_KEYDOWN, 0x41, 0),
            (WM_KEYUP, 0x1b, 0),
        ] {
            let mut seed = make_seed();
            let mut captured = input(raw, bounds(), false, (3, 3));
            captured.keystroke = Some(Keystroke {
                modifiers: Modifiers::default(),
                key: "a".into(),
                key_char: None,
            });
            replay_event(captured, 1.).prepare_click(&mut seed);
            assert!(seed.is_none());
        }
    }

    #[test]
    fn replay_coalescing_keeps_latest_receipt_time() {
        let at = Instant::now();
        let mut previous = moved(50, 50);
        previous.received_at = at;
        let mut latest = moved(10, 10);
        latest.received_at = at + std::time::Duration::from_millis(100);
        latest.inherit_mouse_motion(&previous);
        let replay = replay_event(latest, 1.);
        assert_eq!(
            replay.received_at,
            at + std::time::Duration::from_millis(100)
        );
        assert!(replay.crossed_click_tolerance(Some((10, 10))));
    }

    #[test]
    fn click_gesture_far_move_and_return_stays_dragged() {
        let mut gesture = ClickGesture::default();
        gesture.start((10, 10));
        gesture.observe_move(&moved(40, 10));
        gesture.observe_move(&moved(10, 10));
        assert!(!gesture.finish((10, 10)));
    }

    #[test]
    fn click_gesture_coalesced_return_retains_motion_and_latest_input() {
        let mut far = moved(10, 40);
        far.inherit_mouse_motion(&moved(10, 10));
        let mut returned = moved(10, 10);
        returned.modifiers.shift = true;
        let latest_raw = returned.raw;
        returned.inherit_mouse_motion(&far);
        assert_eq!(returned.raw, latest_raw);
        assert!(returned.modifiers.shift);
        let mut gesture = ClickGesture::default();
        gesture.start((10, 10));
        gesture.observe_move(&returned);
        assert!(!gesture.finish((10, 10)));
    }

    #[test]
    fn click_gesture_two_pixel_jitter_remains_click() {
        let mut gesture = ClickGesture::default();
        gesture.start((10, 10));
        gesture.observe_move(&moved(8, 12));
        gesture.observe_move(&moved(12, 8));
        assert!(gesture.finish((12, 12)));
    }

    #[test]
    fn click_gesture_exact_three_pixels_on_either_axis_is_not_click() {
        for point in [(13, 10), (7, 10), (10, 13), (10, 7)] {
            let mut gesture = ClickGesture::default();
            gesture.start((10, 10));
            gesture.observe_move(&moved(point.0, point.1));
            assert!(!gesture.finish((10, 10)));
        }
        let mut gesture = ClickGesture::default();
        gesture.start((10, 10));
        assert!(!gesture.finish((13, 10)));
    }

    #[test]
    fn click_gesture_finish_and_clear_do_not_leak_to_next_gesture() {
        let mut gesture = ClickGesture::default();
        assert!(!gesture.finish((10, 10)));
        gesture.observe_move(&moved(50, 50));
        gesture.start((10, 10));
        assert!(gesture.finish((10, 10)));
        assert!(!gesture.finish((10, 10)));
        gesture.start((10, 10));
        gesture.observe_move(&moved(40, 40));
        gesture.clear();
        assert!(!gesture.finish((10, 10)));
        gesture.start((10, 10));
        assert!(gesture.finish((10, 10)));
        gesture.start((i32::MIN, 0));
        assert!(!gesture.finish((i32::MAX, 0)));
    }

    #[test]
    fn click_gesture_motion_uses_clamped_client_points_at_negative_origin() {
        let mut gesture = ClickGesture::default();
        gesture.start((0, 0));
        gesture.observe_move(&moved(i16::MIN, -50));
        assert!(gesture.finish((0, 0)));
        gesture.start((100, 60));
        gesture.observe_move(&moved(i16::MAX, i16::MAX));
        assert!(gesture.finish((100, 60)));
        gesture.start((10, 10));
        gesture.observe_move(&moved(10, 10));
        assert!(gesture.finish((10, 10)));

        let far = moved(i16::MAX, i16::MAX);
        let mut returned = moved(-5, -5);
        returned.inherit_mouse_motion(&far);
        gesture.start((0, 0));
        gesture.observe_move(&returned);
        assert!(!gesture.finish((0, 0)));
    }

    #[test]
    fn crop_wheel_notches_preserve_signed_and_partial_deltas() {
        for (delta, expected) in [(120, 1.), (-120, -1.), (60, 0.5), (-60, -0.5)] {
            assert_eq!(
                wheel(WM_MOUSEWHEEL, delta, false, (3, 0)).crop_wheel_notches(),
                Some(expected)
            );
            assert_eq!(
                wheel(WM_MOUSEWHEEL, delta, true, (0, 3)).crop_wheel_notches(),
                Some(expected)
            );
        }
    }

    #[test]
    fn crop_wheel_notches_respect_disabled_axes_and_shift_horizontal() {
        assert_eq!(
            wheel(WM_MOUSEWHEEL, 120, false, (0, 3)).crop_wheel_notches(),
            None
        );
        assert_eq!(
            wheel(WM_MOUSEWHEEL, 120, true, (3, 0)).crop_wheel_notches(),
            None
        );
        assert_eq!(
            wheel(WM_MOUSEHWHEEL, 120, false, (3, 3)).crop_wheel_notches(),
            None
        );
        assert_eq!(
            wheel(WM_MOUSEHWHEEL, 120, true, (3, 0)).crop_wheel_notches(),
            None
        );
        assert_eq!(
            wheel(WM_MOUSEHWHEEL, 60, true, (0, 3)).crop_wheel_notches(),
            Some(-0.5)
        );
        assert_eq!(
            wheel(WM_MOUSEHWHEEL, -120, true, (0, 3)).crop_wheel_notches(),
            Some(1.)
        );
        assert_eq!(moved(10, 10).crop_wheel_notches(), None);
    }
}

#[cfg(test)]
mod ordinary_canvas_sequence_tests {
    use super::*;
    use crate::{
        annotation::Tool,
        capture_double_click::*,
        capture_interaction::{screenshot_empty_crop_inside_target, ScreenshotToolbarMenu},
        editor::Editor,
        geometry::Rect,
        toolbar_layout::PanelRect,
    };
    use std::time::Duration;

    const CANVAS: Rect = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
    };
    const CROP: Rect = Rect {
        x: 80,
        y: 60,
        width: 200,
        height: 160,
    };
    const INTERVAL: Duration = Duration::from_millis(500);
    const TOLERANCE: (i32, i32) = (4, 6);

    fn editor() -> Editor {
        let mut editor = Editor::new(CANVAS, Some(CROP));
        editor.select_tool(Tool::Edit);
        editor
    }

    fn ready(editor: &Editor) -> bool {
        ordinary_canvas_click_ready(editor, true, false, false, false)
    }

    fn decision(editor: &Editor, count: usize) -> DoubleClickAction {
        ordinary_double_click(editor, CANVAS, count, true, false, false, false)
    }

    fn replay(raw: RawInput, at: Instant, scale: f32) -> ReplayEvent {
        let mut captured = CapturedInput::for_test(
            raw,
            ScreenRect {
                left: -400,
                top: 100,
                width: 400,
                height: 300,
            },
            (3, 3),
            gpui::Modifiers::default(),
        );
        captured.received_at = at;
        if matches!(raw.0, WM_KEYUP | WM_SYSKEYUP) {
            captured.keystroke = Some(gpui::Keystroke {
                key: "a".into(),
                key_char: None,
                modifiers: gpui::Modifiers::default(),
            });
        }
        match captured.into_replay(scale).unwrap() {
            ReplayInput::Event(event) => event,
            _ => panic!("expected ordinary replay event"),
        }
    }

    fn packed(point: (i32, i32)) -> isize {
        (point.0 as u16 as isize) | ((point.1 as u16 as isize) << 16)
    }

    #[test]
    fn consumed_menu_and_parameter_down_restart_real_center_and_inner_resize_without_confirming() {
        let at = Instant::now();
        // 菜单 Right→Left 关闭通常为 raw1；参数/控件关闭自身也可能已是 raw2/3。
        for consumed_count in [1, 2, 3] {
            for (point, inside) in [((250, 210), false), ((81, 100), true)] {
                let mut editor = editor();
                if consumed_count == 1 {
                    let mut menu = ScreenshotToolbarMenu::default();
                    assert!(menu.open(
                        &editor,
                        (100., 100.),
                        PanelRect {
                            x: 0.,
                            y: 0.,
                            width: 400.,
                            height: 300.,
                        }
                    ));
                    let (rect, _) = menu.presentation().unwrap();
                    assert!((point.0 as f32) < rect.x || (point.1 as f32) >= rect.bottom());
                    assert!(menu.dismiss());
                } // raw2/3 对应参数/子控件关闭；不假设 Right→Left 可以直接到2。
                let mut sequence = CanvasClickSequence::default();
                let mut seed = None; // 实际 consumed Down 的统一屏障。
                sequence.consumed();
                assert!(sequence.take_pending().is_none()); // 对应关闭 Down 的 Up 无画布首击。
                let first_at = at + Duration::from_millis(100);
                let first = sequence.count(seed.take(), consumed_count + 1, point, first_at);
                assert_eq!(first, 1);
                assert_eq!(decision(&editor, first), DoubleClickAction::None);
                sequence.begin(first, ready(&editor), point, first_at, INTERVAL, TOLERANCE);
                if inside {
                    assert!(screenshot_empty_crop_inside_target(
                        &editor,
                        point,
                        6.,
                        gpui::Modifiers::default(),
                        true,
                    )
                    .is_some());
                    assert!(editor.begin_screenshot_empty_crop_inside(point, 6.));
                } else {
                    assert!(editor.screenshot_empty_crop_center_ready(point, 6.));
                    assert!(editor.begin_screenshot_empty_crop_center(point, 6.));
                }
                // 真实 Crop gesture，不能误落回 Edit Marquee。
                assert!(editor.is_changing_crop());
                editor.pointer_up(point);
                let pending = sequence.take_pending().unwrap();
                seed = sequence.finish(pending, point, ready(&editor));
                assert!(seed.is_some());
                assert_eq!(editor.crop, Some(CROP));
                assert_eq!(editor.tool, Tool::Edit);
                assert!(!editor.document.can_undo());
                let second = sequence.count(
                    seed.take(),
                    consumed_count + 2,
                    point,
                    at + Duration::from_millis(200),
                );
                assert_eq!(second, 2);
                assert_eq!(decision(&editor, second), DoubleClickAction::Confirm);
                sequence.consumed(); // perform(Copy) 的输入清除。
                sequence.after_double(); // 必须在 perform 后完成。
                assert!(sequence.take_pending().is_none());
            }
        }
        for valid_preview in [true, false] {
            let point = (120, 100);
            let mut editor = Editor::new(CANVAS, None);
            let mut sequence = CanvasClickSequence::default();
            assert!(ready(&editor)); // Down 时无 crop 仍可作为候选首击。
            sequence.begin(1, ready(&editor), point, at, INTERVAL, TOLERANCE);
            editor.pointer_down(point, 6.);
            editor.pointer_up(point);
            assert!(editor.crop.is_none());
            if valid_preview {
                editor.set_crop(Some(CROP)); // View Up 使用 Down 时 selection_click_target。
            }
            let pending = sequence.take_pending().unwrap();
            let valid = editor
                .crop
                .is_some_and(|crop| crop.valid(CANVAS.width, CANVAS.height));
            let seed = sequence.finish(pending, point, ready(&editor) && valid);
            assert_eq!(seed.is_some(), valid_preview);
            let count = sequence.count(seed, 2, point, at + Duration::from_millis(100));
            assert_eq!(
                decision(&editor, count),
                if valid_preview {
                    DoubleClickAction::Confirm
                } else {
                    DoubleClickAction::None
                }
            );
            assert!(!editor.document.can_undo());
            assert!(editor.document.items().is_empty());
        }
    }

    #[test]
    fn completed_native_handoff_residual_up_and_physical_time_bounds_keep_legitimate_double_click()
    {
        let editor = editor();
        let point = (120, 100);
        let at = Instant::now();
        for native_count in [1, 2] {
            let mut sequence = CanvasClickSequence::default();
            let mut seed = Some(ClickSeed::new(point, at, INTERVAL, TOLERANCE));
            // GPUI 的 residual Up 没有自身 pending；View 的 if-let 不覆盖 Native 首击。
            assert!(sequence.take_pending().is_none());
            assert!(seed.is_some_and(ClickSeed::native));
            let second = sequence.count(
                seed.take(),
                native_count,
                point,
                at + Duration::from_millis(100),
            );
            assert_eq!(second, 2);
            assert_eq!(decision(&editor, second), DoubleClickAction::Confirm);
        }
        for (point, time) in [
            ((125, 100), at),
            ((120, 107), at),
            ((120, 100), at + INTERVAL),
        ] {
            let mut sequence = CanvasClickSequence::default();
            let count = sequence.count(
                Some(ClickSeed::new((120, 100), at, INTERVAL, TOLERANCE)),
                2,
                point,
                time,
            );
            assert_eq!(count, 1);
            assert_eq!(decision(&editor, count), DoubleClickAction::None);
        }
        let before = at;
        let future = at + Duration::from_millis(1);
        let mut sequence = CanvasClickSequence::default();
        assert_eq!(
            sequence.count(
                Some(ClickSeed::new(point, future, INTERVAL, TOLERANCE)),
                2,
                point,
                before
            ),
            1
        );
        for count in [0, 3, 4] {
            let mut sequence = CanvasClickSequence::default();
            let seed = ClickSeed::new(point, at, INTERVAL, TOLERANCE);
            assert_eq!(seed.click_count(count, point, at), count);
            assert_eq!(sequence.count(Some(seed), count, point, at), count);
        }
        // 原生重放 prepare_click 先 take 首击；typed context 必须把时空与资格带到 View。
        let old_at = at - Duration::from_secs(10);
        let mut seed = Some(ClickSeed::new(point, old_at, INTERVAL, TOLERANCE));
        let mut sequence = CanvasClickSequence::default();
        let mut residual_up = replay((WM_LBUTTONUP, 0, packed(point)), old_at, 2.);
        assert!(!residual_up.breaks_click_sequence());
        assert!(residual_up.prepare_canvas_click(&mut seed).is_none());
        assert!(sequence.take_pending().is_none());
        assert!(seed.is_some_and(ClickSeed::native));
        let mut replay_down = replay(
            (WM_LBUTTONDOWN, 1, packed(point)),
            old_at + Duration::from_millis(499),
            2.,
        );
        let context = replay_down.prepare_canvas_click(&mut seed).unwrap();
        assert!(seed.is_none());
        let PlatformInput::MouseDown(down) = replay_down.event else {
            panic!("expected Down");
        };
        assert_eq!(down.click_count, 2);
        assert_eq!(context.physical_point, point);
        assert_eq!(down.position, gpui::point(gpui::px(60.), gpui::px(50.)));
        let count = sequence.count(
            context.seed,
            down.click_count,
            context.physical_point,
            context.received_at,
        );
        assert_eq!(decision(&editor, count), DoubleClickAction::Confirm);
        let pending = CanvasClickDown::new((i32::MIN, i32::MAX), at, INTERVAL, TOLERANCE);
        assert!(pending.finish((i32::MAX, i32::MIN), true).is_none());
    }

    #[test]
    fn drag_return_cancel_child_consumption_modifiers_and_busy_cannot_republish_a_first_click() {
        let at = Instant::now();
        let point = (120, 100);
        let mut editor = editor();
        let mut sequence = CanvasClickSequence::default();
        sequence.begin(1, ready(&editor), point, at, INTERVAL, TOLERANCE);
        assert!(editor.begin_screenshot_empty_crop_center(point, 6.));
        sequence.moved((128, 108), true);
        editor.pointer_move((128, 108));
        sequence.moved(point, true);
        editor.pointer_move(point);
        editor.pointer_up(point);
        let pending = sequence.take_pending().unwrap();
        assert!(sequence.finish(pending, point, ready(&editor)).is_none());
        assert_eq!(editor.crop, Some(CROP));
        assert_eq!(
            decision(&editor, sequence.count(None, 2, point, at)),
            DoubleClickAction::None
        );

        sequence.begin(1, ready(&editor), point, at, INTERVAL, TOLERANCE);
        assert!(editor.begin_screenshot_empty_crop_center(point, 6.));
        editor.pointer_move((135, 115));
        assert!(editor.cancel_gesture());
        sequence.consumed(); // Cancel/clear_pointer_tracking 与菜单、toolbar child 同一屏障。
        assert!(sequence.take_pending().is_none());
        assert_eq!(editor.crop, Some(CROP));
        assert!(!editor.document.can_undo());
        for consume in [true, false] {
            sequence.begin(1, ready(&editor), point, at, INTERVAL, TOLERANCE);
            if consume {
                sequence.consumed();
                assert!(sequence.take_pending().is_none());
            } else {
                sequence.moved(point, false); // 非裸 Left / 修饰键变化使当前 Down 失效。
                let pending = sequence.take_pending().unwrap();
                assert!(sequence.finish(pending, point, true).is_none());
            }
            let count = sequence.count(None, 3, point, at);
            assert_eq!(count, 1);
            assert_eq!(decision(&editor, count), DoubleClickAction::None);
        }
        // 对应 View lost-left 早退：先完成已收到的 Pen 笔画，再消费普通点击链。
        let mut pen = Editor::new(CANVAS, Some(CROP));
        pen.select_tool(Tool::Pen);
        sequence.begin(1, ready(&pen), point, at, INTERVAL, TOLERANCE);
        pen.pointer_down(point, 6.);
        assert!(pen.drawing_freehand_pen());
        sequence.moved((128, 108), true);
        pen.pointer_move((128, 108));
        assert!(pen.finish_gesture()); // finish_content_or_status 的真实 Editor 收笔路线。
        assert!(!pen.drawing_freehand_pen());
        assert_eq!(pen.document.items().len(), 1);
        assert!(pen.document.can_undo());
        let stroke = pen.document.items()[0].clone();
        sequence.consumed(); // 对应 finish 之后的 invalidate_click_sequence。
        assert!(sequence.take_pending().is_none());
        let next = sequence.count(None, 3, point, at);
        assert_eq!(next, 1);
        assert_eq!(decision(&pen, next), DoubleClickAction::None);
        assert_eq!(pen.document.items()[0], stroke);
        pen.undo();
        assert!(pen.document.items().is_empty());
        assert!(!pen.document.can_undo());
        assert!(pen.document.can_redo());
        pen.redo();
        assert_eq!(pen.document.items(), &[stroke]);
        assert!(pen.document.can_undo());
        assert!(!pen.document.can_redo());

        for (screenshot, modified, busy, input) in [
            (false, false, false, false),
            (true, true, false, false),
            (true, false, true, false),
            (true, false, false, true),
        ] {
            let ready = ordinary_canvas_click_ready(&editor, screenshot, modified, busy, input);
            assert!(!ready);
            sequence.begin(1, ready, point, at, INTERVAL, TOLERANCE);
            assert!(sequence.take_pending().is_none());
            let next = sequence.count(None, 3, point, at);
            assert_eq!(next, 1);
            assert_eq!(decision(&editor, next), DoubleClickAction::None);
            assert!(editor.begin_screenshot_empty_crop_center(point, 6.));
            assert!(editor.is_changing_crop());
            editor.pointer_up(point);
            assert_eq!(editor.crop, Some(CROP));
            assert_eq!(
                ordinary_double_click(&editor, CANVAS, 2, screenshot, modified, busy, input),
                DoubleClickAction::None
            );
        }
        sequence.begin(1, true, point, at, INTERVAL, TOLERANCE);
        let pending = sequence.take_pending().unwrap();
        assert!(sequence.finish(pending, point, false).is_none()); // 未拥有实际画布 Up。
                                                                   // 没有鼠标 Move 的 Shift Down/Up 仍由真实 ModifiersChanged 入口锁存。
        sequence.begin(1, true, point, at, INTERVAL, TOLERANCE);
        for (message, shifted) in [(WM_KEYDOWN, true), (WM_KEYUP, false)] {
            let captured = CapturedInput::for_test(
                (message, 0x10, 0),
                ScreenRect {
                    left: -400,
                    top: 100,
                    width: 400,
                    height: 300,
                },
                (3, 3),
                gpui::Modifiers {
                    shift: shifted,
                    ..Default::default()
                },
            );
            let ReplayInput::Event(event) = captured.into_replay(1.).unwrap() else {
                panic!("expected modifiers event");
            };
            assert!(!event.breaks_click_sequence());
            let PlatformInput::ModifiersChanged(changed) = event.event else {
                panic!("expected modifiers changed");
            };
            sequence.modifiers_changed(changed.modifiers.modified());
        }
        let pending = sequence.take_pending().unwrap();
        assert!(sequence.finish(pending, point, true).is_none());
        assert_eq!(
            decision(&editor, sequence.count(None, 2, point, at)),
            DoubleClickAction::None
        );
        // 没有 pending 时不清完成的 Native 首击，也不改其 raw/native 升级。
        let native = Some(ClickSeed::new(point, at, INTERVAL, TOLERANCE));
        sequence.modifiers_changed(true);
        sequence.modifiers_changed(false);
        assert_eq!(sequence.count(native, 1, point, at), 2);

        for message in [WM_KEYUP, WM_SYSKEYUP] {
            sequence.begin(1, true, point, at, INTERVAL, TOLERANCE);
            let mut key_up = replay((message, 0x41, 0), at, 1.);
            assert!(key_up.breaks_click_sequence());
            sequence.consumed(); // handoff 对真实边界先同步 pending，不依赖 View.key。
            let mut seed = None;
            assert!(key_up.prepare_canvas_click(&mut seed).is_none());
            assert!(sequence.take_pending().is_none());
            let next = sequence.count(seed, 2, point, at);
            assert_eq!(next, 1);
            assert_eq!(decision(&editor, next), DoubleClickAction::None);
        }
    }

    #[test]
    fn normal_confirm_and_drawing_exit_do_not_reseed_triple_fourth_or_change_special_text_polyline_counts(
    ) {
        let at = Instant::now();
        let point = (120, 100);
        for drawing in [false, true] {
            let mut editor = editor();
            if drawing {
                editor.select_tool(Tool::Rectangle);
                editor.pointer_down((100, 90), 4.);
                editor.pointer_up((150, 130));
                editor.undo();
                assert!(editor.document.can_redo());
                editor.select_tool(Tool::Arrow);
            }
            let mut sequence = CanvasClickSequence::default();
            let first = sequence.count(None, 1, point, at);
            sequence.begin(first, ready(&editor), point, at, INTERVAL, TOLERANCE);
            if drawing {
                editor.pointer_down(point, 6.);
            } else {
                assert!(editor.begin_screenshot_empty_crop_center(point, 6.));
            }
            editor.pointer_up(point);
            if drawing {
                assert!(editor.has_arrow_draft());
            }
            let pending = sequence.take_pending().unwrap();
            let mut seed = sequence.finish(pending, point, ready(&editor));
            let canvas_seed = seed.unwrap();
            // 专用分支不受 Canvas 来源的应用级重计数影响。
            for raw in [1, 2, 3, 4] {
                assert_eq!(canvas_seed.click_count(raw, point, at), raw);
            }
            let second = sequence.count(seed.take(), 2, point, at + Duration::from_millis(100));
            assert_eq!(
                decision(&editor, second),
                if drawing {
                    DoubleClickAction::Edit
                } else {
                    DoubleClickAction::Confirm
                }
            );
            sequence.consumed(); // perform 的统一失效在先。
            if drawing {
                editor.select_tool(Tool::Edit);
            }
            sequence.after_double(); // 最后的普通 double 完成在后。
            assert!(sequence.take_pending().is_none()); // 第二击 Up 不重新生成首击。
            for raw in [3, 4] {
                let count =
                    sequence.count(seed.take(), raw, point, at + Duration::from_millis(200));
                assert_eq!(count, raw);
                assert_eq!(decision(&editor, count), DoubleClickAction::None);
                sequence.begin(count, ready(&editor), point, at, INTERVAL, TOLERANCE);
                assert!(sequence.take_pending().is_none());
            }
            assert_eq!(editor.crop, Some(CROP));
            assert_eq!(editor.tool, Tool::Edit);
            if drawing {
                assert!(!editor.has_arrow_draft());
                assert!(editor.document.items().is_empty());
                assert!(editor.document.can_redo());
            }
        }
        let mut polyline = editor();
        polyline.select_tool(Tool::Polyline);
        polyline.pointer_down((100, 90), 4.);
        polyline.pointer_up((100, 90));
        polyline.pointer_down((160, 120), 4.);
        polyline.pointer_up((160, 120));
        assert!(polyline.has_polyline());
        assert_eq!(decision(&polyline, 2), DoubleClickAction::None);
        assert!(polyline.insert_polyline_node((160, 120), 4.));
        assert_eq!(polyline.document.items().len(), 1);
    }
}

#[cfg(test)]
mod startup_text_roi_input_tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn text_replay_preserves_snapshot_and_marks_only_accepted_shortcut_consumed() {
        let bounds = ScreenRect {
            left: -1920,
            top: -240,
            width: 640,
            height: 480,
        };
        let a = PresentedWindowPreview {
            rect: Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 80,
            },
            serial: 7,
        };
        let mut down =
            CapturedInput::for_test((WM_KEYDOWN, 0x52, 0), bounds, (3, 3), Modifiers::default());
        down.keystroke = Some(Keystroke {
            modifiers: Modifiers::default(),
            key: "r".into(),
            key_char: Some("r".into()),
        });
        down.set_presented_window_preview(Some(a));
        let rejected = down.clone();
        down.set_presented_window_preview(None);
        assert_eq!(rejected.presented_window_preview(), Some(a));
        let Some(ReplayInput::Event(replay)) = rejected.into_replay(1.5) else {
            panic!("真实输入必须保留")
        };
        assert!(replay.native_text_shortcut_seen());
        assert!(!replay.native_text_consumed());
        assert!(
            matches!(&replay.event, PlatformInput::KeyDown(event) if event.keystroke.key_char.as_deref() == Some("r"))
        );
        down.consume_native_text();
        let Some(ReplayInput::Event(replay)) = down.into_replay(1.5) else {
            panic!("已消费输入仍保留边界")
        };
        assert!(replay.native_text_consumed());
        for (raw, modifiers, expected) in [
            (
                (WM_KEYDOWN, 0x43, 0),
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                Some(TextShortcut::Control),
            ),
            ((WM_KEYDOWN, 0x43, 0), Modifiers::default(), None),
            ((WM_KEYDOWN, 0x52, 1 << 30), Modifiers::default(), None),
            ((WM_SYSKEYDOWN, 0x52, 0), Modifiers::default(), None),
            (
                (WM_KEYDOWN, 0x52, 0),
                Modifiers {
                    function: true,
                    ..Default::default()
                },
                None,
            ),
            (
                (WM_KEYDOWN, 0x43, 0),
                Modifiers {
                    shift: true,
                    control: true,
                    ..Default::default()
                },
                None,
            ),
        ] {
            let mut event = CapturedInput::for_test(raw, bounds, (3, 3), modifiers);
            assert_eq!(event.text_shortcut_down(), expected);
            event.set_presented_window_preview(Some(a));
            assert_eq!(event.presented_window_preview(), expected.map(|_| a));
            event.consume_native_text();
            assert_eq!(event.native_text_consumed(), expected.is_some());
        }
    }
}
