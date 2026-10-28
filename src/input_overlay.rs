//! 录屏输入显示：低级键鼠钩子只记录事件，录制线程把鼠标指针、点击光圈、
//! 滚轮胶囊和按键键帽直接合成进视频帧，成片无需二次编码。
//!
//! 开启鼠标显示时，钩子线程另用同一套图形驱动一个排除采集、鼠标穿透的分层小窗，
//! 在屏幕上同步显示选区内的点击与滚轮效果，录制者能直接看到成片里会出现什么；
//! 该小窗不进入录像，也不写入其它进程。
use crate::{geometry::ScreenRect, recording_options::KeyPosition, Result};
use std::{
    cell::RefCell,
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
        Graphics::Gdi::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const KEY_ROW_LIFETIME: Duration = Duration::from_millis(2200);
const KEY_MERGE_WINDOW: Duration = Duration::from_millis(1000);
const KEY_FADE_IN: Duration = Duration::from_millis(120);
const KEY_FADE_OUT: Duration = Duration::from_millis(350);
const MAX_KEY_ROWS: usize = 4;
/// 按下光圈从略大收拢到静止半径的时长。
const PRESS_IN: Duration = Duration::from_millis(110);
/// 松开后光圈外扩淡出的时长。
const RIPPLE_LIFETIME: Duration = Duration::from_millis(460);
/// 漏掉松开事件时，按下光圈最多保留这么久。
const HOLD_LIMIT: Duration = Duration::from_secs(10);
/// 最后一格滚动后滚轮胶囊停留多久，再用 WHEEL_FADE 淡出。
const WHEEL_HOLD: Duration = Duration::from_millis(380);
const WHEEL_FADE: Duration = Duration::from_millis(220);
const WHEEL_PULSE: Duration = Duration::from_millis(180);
const MAX_RIPPLES: usize = 12;
/// 按下光圈静止半径（DIP）。
const PRESS_RADIUS: f32 = 12.;
const RIPPLE_GROWTH: f32 = 20.;
/// 屏幕同步小窗的动画节拍（毫秒）。
const MIRROR_TICK_MS: u32 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseKind {
    Left,
    Right,
    Middle,
    XButton,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}
impl MouseKind {
    fn is_wheel(self) -> bool {
        matches!(
            self,
            Self::WheelUp | Self::WheelDown | Self::WheelLeft | Self::WheelRight
        )
    }
    /// 左键蓝、右键琥珀、中键紫，侧键中性灰。
    fn color(self) -> [u8; 3] {
        match self {
            Self::Left => [0x3D, 0x8B, 0xFD],
            Self::Right => [0xF5, 0xA5, 0x24],
            Self::Middle => [0xA6, 0x6C, 0xFF],
            _ => [0x8F, 0xA3, 0xB0],
        }
    }
}

#[derive(Clone, Debug)]
enum RawEvent {
    Key {
        vk: u32,
        down: bool,
        secret: bool,
    },
    /// 按键按下／松开与滚轮格；滚轮只有 released = false。
    Mouse {
        kind: MouseKind,
        point: POINT,
        released: bool,
    },
}

#[derive(Default)]
struct HookShared {
    events: Mutex<Vec<(Instant, RawEvent)>>,
    /// 录制线程正在出帧；倒计时与暂停期间为 false，屏幕同步小窗随之隐藏。
    live: AtomicBool,
}

thread_local! {
    static HOOK_TARGET: RefCell<Option<Arc<HookShared>>> = const { RefCell::new(None) };
    static MIRROR: RefCell<Option<Mirror>> = const { RefCell::new(None) };
}

fn push(event: RawEvent) {
    HOOK_TARGET.with(|target| {
        if let Some(shared) = target.borrow().as_ref() {
            let mut events = shared.events.lock().unwrap();
            // 录制线程停滞时限制积压，避免钩子线程无限增长。
            if events.len() < 512 {
                events.push((Instant::now(), event));
            }
        }
    });
}

/// 焦点在标准密码框时不显示按键内容。
fn focus_is_secret() -> bool {
    unsafe {
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let foreground = GetForegroundWindow();
        let thread = GetWindowThreadProcessId(foreground, None);
        if thread == 0
            || GetGUIThreadInfo(thread, &mut info).is_err()
            || info.hwndFocus.is_invalid()
        {
            return false;
        }
        GetWindowLongW(info.hwndFocus, GWL_STYLE) & 0x20 != 0 // ES_PASSWORD
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let message = wparam.0 as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let up = message == WM_KEYUP || message == WM_SYSKEYUP;
        if down || up {
            push(RawEvent::Key {
                vk: info.vkCode,
                down,
                secret: down && focus_is_secret(),
            });
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let wheel = (info.mouseData >> 16) as u16 as i16;
        let event = match wparam.0 as u32 {
            WM_LBUTTONDOWN => Some((MouseKind::Left, false)),
            WM_LBUTTONUP => Some((MouseKind::Left, true)),
            WM_RBUTTONDOWN => Some((MouseKind::Right, false)),
            WM_RBUTTONUP => Some((MouseKind::Right, true)),
            WM_MBUTTONDOWN => Some((MouseKind::Middle, false)),
            WM_MBUTTONUP => Some((MouseKind::Middle, true)),
            WM_XBUTTONDOWN => Some((MouseKind::XButton, false)),
            WM_XBUTTONUP => Some((MouseKind::XButton, true)),
            WM_MOUSEWHEEL if wheel > 0 => Some((MouseKind::WheelUp, false)),
            WM_MOUSEWHEEL if wheel < 0 => Some((MouseKind::WheelDown, false)),
            WM_MOUSEHWHEEL if wheel < 0 => Some((MouseKind::WheelLeft, false)),
            WM_MOUSEHWHEEL if wheel > 0 => Some((MouseKind::WheelRight, false)),
            _ => None,
        };
        if let Some((kind, released)) = event {
            push(RawEvent::Mouse {
                kind,
                point: info.pt,
                released,
            });
            Mirror::event(kind, released, info.pt);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 钩子线程：仅在开启按键或鼠标显示时创建，录制结束即卸载。
pub(crate) struct InputWatcher {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    shared: Arc<HookShared>,
}
impl InputWatcher {
    /// mouse 开启时同时在钩子线程上建屏幕同步小窗，只显示 region 内的效果。
    pub(crate) fn start(keys: bool, mouse: bool, region: ScreenRect, scale: f32) -> Result<Self> {
        let shared = Arc::new(HookShared::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("rustshot-input-hooks".into())
            .spawn(move || unsafe {
                let mut message = MSG::default();
                // 先建立消息队列，PostThreadMessage 才能可靠送达退出消息。
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                if mouse {
                    // 小窗失败只少了屏幕同步，成片照常合成。
                    let mirror = Mirror::new(thread_shared.clone(), region, scale).ok();
                    MIRROR.with(|m| *m.borrow_mut() = mirror);
                }
                HOOK_TARGET.with(|t| *t.borrow_mut() = Some(thread_shared));
                let instance = GetModuleHandleW(None).ok().map(Into::into);
                let keyboard = keys
                    .then(|| SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), instance, 0))
                    .transpose();
                let mouse = mouse
                    .then(|| SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), instance, 0))
                    .transpose();
                let ok = keyboard.is_ok() && mouse.is_ok();
                let _ = ready_tx.send((GetCurrentThreadId(), ok));
                if ok {
                    while GetMessageW(&mut message, None, 0, 0).as_bool() {
                        if message.message == WM_TIMER && Mirror::owns(message.hwnd) {
                            Mirror::tick();
                            continue;
                        }
                        DispatchMessageW(&message);
                    }
                }
                for hook in [keyboard.ok().flatten(), mouse.ok().flatten()]
                    .into_iter()
                    .flatten()
                {
                    let _ = UnhookWindowsHookEx(hook);
                }
                HOOK_TARGET.with(|t| t.borrow_mut().take());
                MIRROR.with(|m| m.borrow_mut().take());
            })?;
        let (thread_id, ok) = ready_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            tr!(
                "输入显示初始化超时",
                "Input display timed out while starting"
            )
        })?;
        let watcher = Self {
            thread_id,
            thread: Some(thread),
            shared,
        };
        if !ok {
            return Err(tr!(
                "无法安装键鼠钩子，已关闭输入显示",
                "Couldn't install the keyboard/mouse hooks; input display turned off"
            )
            .into());
        }
        Ok(watcher)
    }

    fn drain(&self) -> Vec<(Instant, RawEvent)> {
        std::mem::take(&mut *self.shared.events.lock().unwrap())
    }

    fn set_live(&self, live: bool) {
        self.shared.live.store(live, Ordering::Relaxed);
    }
}
impl Drop for InputWatcher {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const CTRL: u8 = 1;
const SHIFT: u8 = 2;
const ALT: u8 = 4;
const WIN: u8 = 8;

fn modifier_bit(vk: u32) -> Option<u8> {
    Some(match VIRTUAL_KEY(vk as u16) {
        VK_CONTROL | VK_LCONTROL | VK_RCONTROL => CTRL,
        VK_SHIFT | VK_LSHIFT | VK_RSHIFT => SHIFT,
        VK_MENU | VK_LMENU | VK_RMENU => ALT,
        VK_LWIN | VK_RWIN => WIN,
        _ => return None,
    })
}

fn is_typing_key(vk: u32) -> bool {
    matches!(vk, 0x30..=0x39 | 0x41..=0x5A | 0xBA..=0xC0 | 0xDB..=0xDF | 0xE2)
}

fn is_numpad_key(vk: u32) -> bool {
    matches!(vk, 0x60..=0x6F) || vk == u32::from(VK_NUMLOCK.0)
}

pub(crate) fn key_name(vk: u32) -> String {
    let named = match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x13 => "Pause",
        0x14 => "Caps",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "PgUp",
        0x22 => "PgDn",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "←",
        0x26 => "↑",
        0x27 => "→",
        0x28 => "↓",
        0x2C => "PrtSc",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5D => "Menu",
        0x6A => "Num *",
        0x6B => "Num +",
        0x6D => "Num -",
        0x6E => "Num .",
        0x6F => "Num /",
        0x90 => "NumLock",
        0x91 => "ScrollLock",
        0xBA => ";",
        0xBB => "=",
        0xBC => ",",
        0xBD => "-",
        0xBE => ".",
        0xBF => "/",
        0xC0 => "`",
        0xDB => "[",
        0xDC => "\\",
        0xDD => "]",
        0xDE => "'",
        0xAD => tr!("静音", "Mute"),
        0xAE => tr!("音量-", "Vol-"),
        0xAF => tr!("音量+", "Vol+"),
        0xB0 => tr!("下一曲", "Next"),
        0xB1 => tr!("上一曲", "Prev"),
        0xB3 => tr!("播放/暂停", "Play/Pause"),
        _ => "",
    };
    if !named.is_empty() {
        return named.into();
    }
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => format!("Key {vk:02X}"),
    }
}

/// 物理按键 → “Ctrl + Shift + S”。过滤打字时隐藏无命令修饰键的字母/数字/标点。
#[derive(Default)]
pub(crate) struct KeyFormatter {
    modifiers: u8,
    held: HashSet<u32>,
    pub filter_typing: bool,
}
impl KeyFormatter {
    pub(crate) fn new(filter_typing: bool) -> Self {
        Self {
            filter_typing,
            ..Self::default()
        }
    }
    pub(crate) fn key_down(&mut self, vk: u32) -> Option<String> {
        if let Some(bit) = modifier_bit(vk) {
            self.modifiers |= bit;
            return None;
        }
        // 按住时的自动重复只显示首次按下。
        if !self.held.insert(vk) {
            return None;
        }
        let command = self.modifiers & (CTRL | ALT | WIN) != 0;
        if self.filter_typing && !command && !is_numpad_key(vk) && is_typing_key(vk) {
            return None;
        }
        let mut parts = Vec::new();
        for (bit, name) in [(CTRL, "Ctrl"), (SHIFT, "Shift"), (ALT, "Alt"), (WIN, "Win")] {
            if self.modifiers & bit != 0 {
                parts.push(name.to_string());
            }
        }
        parts.push(key_name(vk));
        Some(parts.join(" + "))
    }
    pub(crate) fn key_up(&mut self, vk: u32) {
        if let Some(bit) = modifier_bit(vk) {
            self.modifiers &= !bit;
        }
        self.held.remove(&vk);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeyRow {
    pub label: String,
    pub count: u32,
    pub first: Instant,
    pub last: Instant,
}
impl KeyRow {
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        if self.count > 1 {
            format!("{} ×{}", self.label, self.count)
        } else {
            self.label.clone()
        }
    }
    /// 键帽文字：“Ctrl + Num +” → ["Ctrl", "Num +"]。
    fn caps(&self) -> impl Iterator<Item = &str> {
        self.label.split(" + ")
    }
    /// 新行淡入，到期前淡出。
    fn opacity(&self, now: Instant) -> f32 {
        let shown = now.saturating_duration_since(self.first).as_secs_f32();
        let left = KEY_ROW_LIFETIME
            .saturating_sub(now.saturating_duration_since(self.last))
            .as_secs_f32();
        (shown / KEY_FADE_IN.as_secs_f32())
            .min(left / KEY_FADE_OUT.as_secs_f32())
            .clamp(0., 1.)
    }
}

/// 最近按键行：1 秒内相同组合合并为 ×N，2.2 秒后消失，最多 4 行。
#[derive(Default)]
pub(crate) struct KeyRows {
    pub rows: Vec<KeyRow>,
}
impl KeyRows {
    pub(crate) fn push(&mut self, label: String, now: Instant) {
        if let Some(last) = self.rows.last_mut() {
            if last.label == label && now.saturating_duration_since(last.last) <= KEY_MERGE_WINDOW {
                last.count += 1;
                last.last = now;
                return;
            }
        }
        self.rows.push(KeyRow {
            label,
            count: 1,
            first: now,
            last: now,
        });
        if self.rows.len() > MAX_KEY_ROWS {
            self.rows.remove(0);
        }
    }
    pub(crate) fn prune(&mut self, now: Instant) {
        self.rows
            .retain(|row| now.saturating_duration_since(row.last) < KEY_ROW_LIFETIME);
    }
}

#[derive(Clone, Copy)]
struct Press {
    kind: MouseKind,
    at: Instant,
}

#[derive(Clone, Copy)]
struct Ripple {
    kind: MouseKind,
    point: POINT,
    at: Instant,
}

#[derive(Clone, Copy)]
struct Wheel {
    kind: MouseKind,
    first: Instant,
    last: Instant,
}

fn ease_out(t: f32) -> f32 {
    1. - (1. - t.clamp(0., 1.)).powi(3)
}

fn progress(now: Instant, since: Instant, span: Duration) -> f32 {
    (now.saturating_duration_since(since).as_secs_f32() / span.as_secs_f32()).clamp(0., 1.)
}

/// 滚轮胶囊的半宽／半高（物理像素）；水平滚动时横放。
fn wheel_half(kind: MouseKind, s: f32) -> (f32, f32) {
    let (across, along) = (9. * s, 14. * s);
    if matches!(kind, MouseKind::WheelLeft | MouseKind::WheelRight) {
        (along, across)
    } else {
        (across, along)
    }
}

fn wheel_axis(kind: MouseKind) -> (f32, f32) {
    match kind {
        MouseKind::WheelUp => (0., -1.),
        MouseKind::WheelLeft => (-1., 0.),
        MouseKind::WheelRight => (1., 0.),
        _ => (0., 1.),
    }
}

/// 点击与滚轮效果的状态。录制线程与屏幕同步小窗各持一份，按同一事件流演进；
/// 坐标均为屏幕物理像素，按下光圈与滚轮胶囊跟随当前指针，松开光圈留在松开处。
#[derive(Default)]
struct MouseFx {
    held: Vec<Press>,
    ripples: Vec<Ripple>,
    wheel: Option<Wheel>,
}
impl MouseFx {
    fn event(&mut self, kind: MouseKind, released: bool, point: POINT, at: Instant) {
        if kind.is_wheel() {
            // 胶囊仍可见时续滚或换向都不重新淡入。
            let first = self
                .wheel
                .filter(|w| at.saturating_duration_since(w.last) < WHEEL_HOLD + WHEEL_FADE)
                .map_or(at, |w| w.first);
            self.wheel = Some(Wheel {
                kind,
                first,
                last: at,
            });
        } else if released {
            // 只有见过按下的松开才外扩；暂停期间按下、恢复后松开不补效果。
            if let Some(index) = self.held.iter().position(|p| p.kind == kind) {
                self.held.remove(index);
                self.ripples.push(Ripple { kind, point, at });
                if self.ripples.len() > MAX_RIPPLES {
                    self.ripples.remove(0);
                }
            }
        } else {
            self.held.retain(|p| p.kind != kind);
            self.held.push(Press { kind, at });
        }
    }

    fn prune(&mut self, now: Instant) {
        self.held
            .retain(|p| now.saturating_duration_since(p.at) < HOLD_LIMIT);
        self.ripples
            .retain(|r| now.saturating_duration_since(r.at) < RIPPLE_LIFETIME);
        if self
            .wheel
            .is_some_and(|w| now.saturating_duration_since(w.last) >= WHEEL_HOLD + WHEEL_FADE)
        {
            self.wheel = None;
        }
    }

    fn is_idle(&self) -> bool {
        self.held.is_empty() && self.ripples.is_empty() && self.wheel.is_none()
    }

    fn wheel_center(cursor: POINT, s: f32) -> (f32, f32) {
        (cursor.x as f32 + 26. * s, cursor.y as f32 + 24. * s)
    }

    /// 当前效果的外接矩形（左、上、右、下，右下不含），已裁到 clip 内。
    fn bounds(&self, cursor: POINT, s: f32, clip: ScreenRect) -> Option<(i32, i32, i32, i32)> {
        let mut out: Option<(f32, f32, f32, f32)> = None;
        let mut add = |cx: f32, cy: f32, hx: f32, hy: f32| {
            let r = (cx - hx, cy - hy, cx + hx, cy + hy);
            out = Some(out.map_or(r, |o| {
                (o.0.min(r.0), o.1.min(r.1), o.2.max(r.2), o.3.max(r.3))
            }));
        };
        let pad = 2. * s + 2.;
        let tip = (cursor.x as f32 + 0.5, cursor.y as f32 + 0.5);
        if !self.held.is_empty() {
            let r = (PRESS_RADIUS + 5.) * s + pad;
            add(tip.0, tip.1, r, r);
        }
        for ripple in &self.ripples {
            let r = (PRESS_RADIUS + RIPPLE_GROWTH) * s + pad;
            add(
                ripple.point.x as f32 + 0.5,
                ripple.point.y as f32 + 0.5,
                r,
                r,
            );
        }
        if let Some(wheel) = self.wheel {
            let (cx, cy) = Self::wheel_center(cursor, s);
            let (hx, hy) = wheel_half(wheel.kind, s);
            add(cx, cy, hx + pad, hy + pad);
        }
        let (l, t, r, b) = out?;
        let l = (l.floor() as i32).max(clip.left);
        let t = (t.floor() as i32).max(clip.top);
        let r = (r.ceil() as i32).min(clip.left + clip.width as i32);
        let b = (b.ceil() as i32).min(clip.top + clip.height as i32);
        (r > l && b > t).then_some((l, t, r, b))
    }

    fn draw(&self, layer: &mut Layer, now: Instant, cursor: POINT, s: f32) {
        for ripple in &self.ripples {
            let t = progress(now, ripple.at, RIPPLE_LIFETIME);
            let e = ease_out(t);
            let c = (ripple.point.x as f32 + 0.5, ripple.point.y as f32 + 0.5);
            let color = ripple.kind.color();
            // 实心核先散开，外圈随后变细淡出。
            let core = 1. - (t / 0.45).min(1.);
            layer.disc(c, PRESS_RADIUS * s * (1. + 0.25 * e), color, 0.26 * core);
            let radius = (PRESS_RADIUS + RIPPLE_GROWTH * e) * s;
            let width = ((2. - 1.2 * e) * s).max(1.);
            let fade = (1. - t).powf(1.6);
            layer.ring(c, radius + width, s.max(1.), [255, 255, 255], 0.3 * fade);
            layer.ring(c, radius, width, color, 0.85 * fade);
        }
        let c = (cursor.x as f32 + 0.5, cursor.y as f32 + 0.5);
        for press in &self.held {
            let e = ease_out(progress(now, press.at, PRESS_IN));
            let color = press.kind.color();
            let radius = (PRESS_RADIUS + 5. * (1. - e)) * s;
            layer.disc(c, radius, color, 0.12 + 0.14 * e);
            // 外侧细白边让光圈在同色背景上也分得清。
            layer.ring(c, radius + 2. * s, s.max(1.), [255, 255, 255], 0.25);
            layer.ring(c, radius, 2. * s, color, 0.92);
        }
        if let Some(wheel) = self.wheel {
            let idle = now.saturating_duration_since(wheel.last);
            let fade = 1. - progress(now, wheel.last + WHEEL_HOLD, WHEEL_FADE);
            let shown = progress(now, wheel.first, KEY_FADE_IN);
            let alpha = if idle < WHEEL_HOLD {
                shown
            } else {
                shown * fade
            };
            let (cx, cy) = Self::wheel_center(cursor, s);
            let (hx, hy) = wheel_half(wheel.kind, s);
            let rect = (cx - hx, cy - hy, cx + hx, cy + hy);
            layer.round_rect(rect, hx.min(hy), [0x1C, 0x1C, 0x20], 0.8 * alpha);
            layer.round_rect_stroke(rect, hx.min(hy), s.max(1.), [255, 255, 255], 0.18 * alpha);
            // 两道箭头指向滚动方向，每一格从后方滑入一次。
            let (fx, fy) = wheel_axis(wheel.kind);
            let (px, py) = (-fy, fx);
            let slide = (1. - ease_out(progress(now, wheel.last, WHEEL_PULSE))) * 3. * s;
            for (step, weight) in [(3.5, 1.), (-3.5, 0.5)] {
                let off = step * s - slide;
                let (ox, oy) = (cx + fx * off, cy + fy * off);
                let tip = (ox + fx * 2. * s, oy + fy * 2. * s);
                let back = (ox - fx * 2. * s, oy - fy * 2. * s);
                let arms = [
                    (back.0 + px * 4. * s, back.1 + py * 4. * s),
                    (back.0 - px * 4. * s, back.1 - py * 4. * s),
                ];
                layer.chevron(tip, arms, 1.8 * s, [255, 255, 255], weight * alpha);
            }
        }
    }
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len > 0. {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len).clamp(0., 1.)
    } else {
        0.
    };
    distance(p, (a.0 + dx * t, a.1 + dy * t))
}

/// 圆角矩形的有向距离：内部为负。rect 为（左、上、右、下）。
fn round_rect_distance(p: (f32, f32), rect: (f32, f32, f32, f32), radius: f32) -> f32 {
    let (l, t, r, b) = rect;
    let radius = radius.min((r - l) / 2.).min((b - t) / 2.);
    let qx = (p.0 - (l + r) / 2.).abs() - ((r - l) / 2. - radius);
    let qy = (p.1 - (t + b) / 2.).abs() - ((b - t) / 2. - radius);
    qx.max(0.).hypot(qy.max(0.)) + qx.max(qy).min(0.) - radius
}

/// 预乘 BGRA 小图层。坐标系由调用方决定（屏幕或帧），left/top 为图层左上角在其中的位置；
/// 同一份图层既能叠进视频帧，也能直接交给分层窗口。
#[derive(Default)]
struct Layer {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    px: Vec<u8>,
}
impl Layer {
    fn reset(&mut self, (left, top, right, bottom): (i32, i32, i32, i32)) {
        self.left = left;
        self.top = top;
        self.width = (right - left).max(0) as u32;
        self.height = (bottom - top).max(0) as u32;
        self.px.clear();
        self.px.resize((self.width * self.height * 4) as usize, 0);
    }

    /// 在 bounds 内逐像素取覆盖率（像素中心坐标），按 alpha 叠上纯色。
    fn shade(
        &mut self,
        bounds: (f32, f32, f32, f32),
        color: [u8; 3],
        alpha: f32,
        coverage: impl Fn((f32, f32)) -> f32,
    ) {
        if alpha <= 0. {
            return;
        }
        let x0 = (bounds.0.floor() as i32).max(self.left);
        let y0 = (bounds.1.floor() as i32).max(self.top);
        let x1 = (bounds.2.ceil() as i32).min(self.left + self.width as i32);
        let y1 = (bounds.3.ceil() as i32).min(self.top + self.height as i32);
        for y in y0..y1 {
            for x in x0..x1 {
                let a = coverage((x as f32 + 0.5, y as f32 + 0.5)).clamp(0., 1.) * alpha;
                if a > 0. {
                    self.over(x, y, color, a.min(1.));
                }
            }
        }
    }

    fn over(&mut self, x: i32, y: i32, color: [u8; 3], a: f32) {
        let i = (((y - self.top) as u32 * self.width + (x - self.left) as u32) * 4) as usize;
        let keep = 1. - a;
        let px = &mut self.px[i..i + 4];
        for (channel, value) in [(2, color[0]), (1, color[1]), (0, color[2])] {
            px[channel] = (f32::from(value) * a + f32::from(px[channel]) * keep).round() as u8;
        }
        px[3] = (255. * a + f32::from(px[3]) * keep).round() as u8;
    }

    fn disc(&mut self, c: (f32, f32), radius: f32, color: [u8; 3], alpha: f32) {
        let e = radius + 1.;
        self.shade((c.0 - e, c.1 - e, c.0 + e, c.1 + e), color, alpha, |p| {
            radius + 0.5 - distance(p, c)
        });
    }

    fn ring(&mut self, c: (f32, f32), radius: f32, width: f32, color: [u8; 3], alpha: f32) {
        let e = radius + width + 1.;
        self.shade((c.0 - e, c.1 - e, c.0 + e, c.1 + e), color, alpha, |p| {
            width / 2. + 0.5 - (distance(p, c) - radius).abs()
        });
    }

    fn round_rect(&mut self, rect: (f32, f32, f32, f32), radius: f32, color: [u8; 3], alpha: f32) {
        let bounds = (rect.0 - 1., rect.1 - 1., rect.2 + 1., rect.3 + 1.);
        self.shade(bounds, color, alpha, |p| {
            0.5 - round_rect_distance(p, rect, radius)
        });
    }

    /// 贴着内缘的描边。
    fn round_rect_stroke(
        &mut self,
        rect: (f32, f32, f32, f32),
        radius: f32,
        width: f32,
        color: [u8; 3],
        alpha: f32,
    ) {
        let bounds = (rect.0 - 1., rect.1 - 1., rect.2 + 1., rect.3 + 1.);
        self.shade(bounds, color, alpha, |p| {
            width / 2. + 0.5 - (round_rect_distance(p, rect, radius) + width / 2.).abs()
        });
    }

    /// 两段折线共用一次覆盖率，尖端不会叠成亮点。
    fn chevron(
        &mut self,
        tip: (f32, f32),
        arms: [(f32, f32); 2],
        width: f32,
        color: [u8; 3],
        alpha: f32,
    ) {
        let e = width + 1.;
        let xs = [tip.0, arms[0].0, arms[1].0];
        let ys = [tip.1, arms[0].1, arms[1].1];
        let bounds = (
            xs.iter().copied().fold(f32::MAX, f32::min) - e,
            ys.iter().copied().fold(f32::MAX, f32::min) - e,
            xs.iter().copied().fold(f32::MIN, f32::max) + e,
            ys.iter().copied().fold(f32::MIN, f32::max) + e,
        );
        self.shade(bounds, color, alpha, |p| {
            let d = segment_distance(p, arms[0], tip).min(segment_distance(p, tip, arms[1]));
            width / 2. + 0.5 - d
        });
    }

    /// 叠到不透明 BGRA 帧上；origin 为帧左上角在图层坐标系中的位置。
    fn composite(&self, frame: &mut [u8], frame_width: u32, frame_height: u32, origin: (i32, i32)) {
        let x0 = (self.left - origin.0).max(0);
        let y0 = (self.top - origin.1).max(0);
        let x1 = (self.left + self.width as i32 - origin.0).min(frame_width as i32);
        let y1 = (self.top + self.height as i32 - origin.1).min(frame_height as i32);
        for y in y0..y1 {
            let src_row = ((y + origin.1 - self.top) as u32 * self.width) as usize;
            let dst_row = (y as u32 * frame_width) as usize;
            for x in x0..x1 {
                let src = (src_row + (x + origin.0 - self.left) as usize) * 4;
                let a = u32::from(self.px[src + 3]);
                if a == 0 {
                    continue;
                }
                let dst = (dst_row + x as usize) * 4;
                for c in 0..3 {
                    let under = (u32::from(frame[dst + c]) * (255 - a) + 127) / 255;
                    frame[dst + c] = (u32::from(self.px[src + c]) + under).min(255) as u8;
                }
            }
        }
    }
}

const MIRROR_CLASS: PCWSTR = w!("RustShot.Recording.InputMirror.v1");
const MIRROR_TIMER: usize = 1;

unsafe extern "system" fn mirror_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(h, message, wp, lp) }
}

/// 钩子线程上的屏幕同步小窗：与成片同款的点击与滚轮效果，只覆盖效果所在的小块，
/// 排除采集、鼠标穿透、不激活；空闲时隐藏并停表。
struct Mirror {
    hwnd: HWND,
    shared: Arc<HookShared>,
    region: ScreenRect,
    scale: f32,
    fx: MouseFx,
    layer: Layer,
    surface: Option<Surface>,
    ticking: bool,
    shown: bool,
}
impl Mirror {
    fn new(shared: Arc<HookShared>, region: ScreenRect, scale: f32) -> Result<Self> {
        // 远程兼容时工具窗不排除采集，小窗会被录进去，与成片里的效果叠成两份。
        if crate::remote_display::compatible() {
            return Err("远程兼容模式不显示屏幕同步".into());
        }
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let class = WNDCLASSW {
                hInstance: instance,
                lpszClassName: MIRROR_CLASS,
                lpfnWndProc: Some(mirror_proc),
                ..Default::default()
            };
            // 第二次录制时类已存在，注册失败无妨，建窗失败才算失败。
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOPMOST,
                MIRROR_CLASS,
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )?;
            // 排除采集失败就不显示，免得同一效果在成片里出现两次。
            if let Err(error) = crate::remote_display::exclude_from_capture(hwnd) {
                let _ = DestroyWindow(hwnd);
                return Err(error.into());
            }
            Ok(Self {
                hwnd,
                shared,
                region,
                scale,
                fx: MouseFx::default(),
                layer: Layer::default(),
                surface: None,
                ticking: false,
                shown: false,
            })
        }
    }

    fn owns(hwnd: HWND) -> bool {
        MIRROR.with(|m| {
            m.try_borrow()
                .is_ok_and(|m| m.as_ref().is_some_and(|m| m.hwnd == hwnd))
        })
    }

    /// 钩子回调里只记账和起表，绘制留给计时器，不拖慢系统输入。
    fn event(kind: MouseKind, released: bool, point: POINT) {
        MIRROR.with(|m| {
            let Ok(mut m) = m.try_borrow_mut() else {
                return;
            };
            let Some(m) = m.as_mut() else {
                return;
            };
            if !m.shared.live.load(Ordering::Relaxed) {
                return;
            }
            m.fx.event(kind, released, point, Instant::now());
            if !m.ticking {
                unsafe { SetTimer(Some(m.hwnd), MIRROR_TIMER, MIRROR_TICK_MS, None) };
                m.ticking = true;
            }
        });
    }

    fn tick() {
        MIRROR.with(|m| {
            if let Ok(mut m) = m.try_borrow_mut() {
                if let Some(m) = m.as_mut() {
                    m.render();
                }
            }
        });
    }

    fn render(&mut self) {
        let now = Instant::now();
        if !self.shared.live.load(Ordering::Relaxed) {
            self.fx = MouseFx::default();
        }
        self.fx.prune(now);
        if self.fx.is_idle() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), MIRROR_TIMER);
            }
            self.ticking = false;
            self.hide();
            return;
        }
        let mut cursor = POINT::default();
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let Some(bounds) = self.fx.bounds(cursor, self.scale, self.region) else {
            self.hide();
            return;
        };
        self.layer.reset(bounds);
        self.fx.draw(&mut self.layer, now, cursor, self.scale);
        let (w, h) = (self.layer.width, self.layer.height);
        let Ok(surface) = Surface::ensure(&mut self.surface, w, h) else {
            return;
        };
        unsafe {
            let _ = GdiFlush();
            let dib = std::slice::from_raw_parts_mut(
                surface.bits,
                (surface.width * surface.height * 4) as usize,
            );
            for row in 0..h as usize {
                let src = row * w as usize * 4;
                let dst = row * surface.width as usize * 4;
                dib[dst..dst + w as usize * 4]
                    .copy_from_slice(&self.layer.px[src..src + w as usize * 4]);
            }
            let updated = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&POINT {
                    x: bounds.0,
                    y: bounds.1,
                }),
                Some(&SIZE {
                    cx: w as i32,
                    cy: h as i32,
                }),
                Some(surface.dc),
                Some(&POINT { x: 0, y: 0 }),
                COLORREF(0),
                Some(&BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            );
            if updated.is_ok() && !self.shown {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
                self.shown = true;
            }
        }
    }

    fn hide(&mut self) {
        if self.shown {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.shown = false;
        }
    }
}
impl Drop for Mirror {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), MIRROR_TIMER);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// 可复用的 32 位 top-down DIB，承载小块帧像素做 GDI 绘制。
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}
impl Surface {
    fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err(tr!("无法创建绘制表面", "Couldn't create the drawing surface").into());
            }
            let info = crate::gdi_surface::bitmap_info(width, height);
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(bitmap) => bitmap,
                Err(error) => {
                    let _ = DeleteDC(dc);
                    return Err(error.into());
                }
            };
            let old = SelectObject(dc, bitmap.into());
            Ok(Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                width,
                height,
            })
        }
    }
    fn ensure(slot: &mut Option<Surface>, width: u32, height: u32) -> Result<&mut Surface> {
        if slot
            .as_ref()
            .is_none_or(|s| s.width < width || s.height < height)
        {
            let w = width.max(slot.as_ref().map_or(0, |s| s.width));
            let h = height.max(slot.as_ref().map_or(0, |s| s.height));
            *slot = Some(Surface::new(w, h)?);
        }
        Ok(slot.as_mut().expect("刚创建表面"))
    }
    /// 帧 → DIB 左上角（rect 为帧内坐标）。
    fn load(&mut self, frame: &[u8], frame_width: u32, rect: (u32, u32, u32, u32)) {
        let (x, y, w, h) = rect;
        unsafe {
            let _ = GdiFlush();
            let dib =
                std::slice::from_raw_parts_mut(self.bits, (self.width * self.height * 4) as usize);
            for row in 0..h {
                let src = (((y + row) * frame_width + x) * 4) as usize;
                let dst = (row * self.width * 4) as usize;
                dib[dst..dst + (w * 4) as usize]
                    .copy_from_slice(&frame[src..src + (w * 4) as usize]);
            }
        }
    }
    fn store(&self, frame: &mut [u8], frame_width: u32, rect: (u32, u32, u32, u32)) {
        let (x, y, w, h) = rect;
        unsafe {
            let _ = GdiFlush();
            let dib =
                std::slice::from_raw_parts(self.bits, (self.width * self.height * 4) as usize);
            for row in 0..h {
                let dst = (((y + row) * frame_width + x) * 4) as usize;
                let src = (row * self.width * 4) as usize;
                frame[dst..dst + (w * 4) as usize]
                    .copy_from_slice(&dib[src..src + (w * 4) as usize]);
                for pixel in frame[dst..dst + (w * 4) as usize].chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
            }
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct CursorShape {
    handle: usize,
    hotspot: (i32, i32),
    size: (i32, i32),
}

fn cursor_shape(handle: HCURSOR) -> Option<CursorShape> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(HICON(handle.0), &mut info).ok()?;
        let color = !info.hbmColor.is_invalid();
        let mut bitmap = BITMAP::default();
        let read = GetObjectW(
            if color { info.hbmColor } else { info.hbmMask }.into(),
            size_of::<BITMAP>() as i32,
            Some((&mut bitmap as *mut BITMAP).cast()),
        );
        for owned in [info.hbmColor, info.hbmMask] {
            if !owned.is_invalid() {
                let _ = DeleteObject(owned.into());
            }
        }
        if read == 0 {
            return None;
        }
        let height = bitmap.bmHeight / if color { 1 } else { 2 };
        (bitmap.bmWidth > 0 && height > 0 && bitmap.bmWidth <= 256 && height <= 256).then_some(
            CursorShape {
                handle: handle.0 as usize,
                hotspot: (info.xHotspot as i32, info.yHotspot as i32),
                size: (bitmap.bmWidth, height),
            },
        )
    }
}

/// 录制线程独占的帧合成器；GDI 资源在创建线程释放。
pub(crate) struct FramePainter {
    region: ScreenRect,
    scale: f32,
    cursor: bool,
    watcher: Option<InputWatcher>,
    formatter: KeyFormatter,
    keys: KeyRows,
    key_position: KeyPosition,
    fx: MouseFx,
    layer: Layer,
    surface: Option<Surface>,
    font: HFONT,
    shape: Option<CursorShape>,
}
impl FramePainter {
    pub(crate) fn new(
        region: ScreenRect,
        scale: f32,
        cursor: bool,
        keys: bool,
        mouse: bool,
        filter_typing: bool,
        key_position: KeyPosition,
    ) -> (Self, Option<String>) {
        let (watcher, warning) = if keys || mouse {
            match InputWatcher::start(keys, mouse, region, scale) {
                Ok(watcher) => (Some(watcher), None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let font = unsafe {
            CreateFontW(
                -((14. * scale).round() as i32),
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                ANTIALIASED_QUALITY,
                0,
                w!("Microsoft YaHei UI"),
            )
        };
        (
            Self {
                region,
                scale,
                cursor,
                watcher,
                formatter: KeyFormatter::new(filter_typing),
                keys: KeyRows::default(),
                key_position,
                fx: MouseFx::default(),
                layer: Layer::default(),
                surface: None,
                font,
                shape: None,
            },
            warning,
        )
    }

    pub(crate) fn needs_painting(&self) -> bool {
        self.cursor || self.watcher.is_some()
    }

    /// 暂停时继续消费事件，但不累积到成片中。
    pub(crate) fn discard_events(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.set_live(false);
            for (_, event) in watcher.drain() {
                if let RawEvent::Key {
                    vk, down: false, ..
                } = event
                {
                    self.formatter.key_up(vk);
                }
            }
        }
        self.keys.rows.clear();
        self.fx = MouseFx::default();
    }

    fn collect(&mut self, now: Instant) {
        if let Some(watcher) = &self.watcher {
            for (at, event) in watcher.drain() {
                match event {
                    RawEvent::Key { vk, down, secret } => {
                        if down {
                            if let Some(label) = self.formatter.key_down(vk) {
                                if !secret {
                                    self.keys.push(label, at);
                                }
                            }
                        } else {
                            self.formatter.key_up(vk);
                        }
                    }
                    RawEvent::Mouse {
                        kind,
                        point,
                        released,
                    } => self.fx.event(kind, released, point, at),
                }
            }
        }
        self.keys.prune(now);
        self.fx.prune(now);
    }

    pub(crate) fn paint(&mut self, frame: &mut [u8], width: u32, height: u32) {
        let now = Instant::now();
        if let Some(watcher) = &self.watcher {
            watcher.set_live(true);
        }
        self.collect(now);
        // 光圈在指针下面，指针仍清晰可见；按键提示压在最上层。
        if !self.fx.is_idle() {
            let mut cursor = POINT::default();
            let _ = unsafe { GetCursorPos(&mut cursor) };
            if let Some(bounds) = self.fx.bounds(cursor, self.scale, self.region) {
                self.layer.reset(bounds);
                self.fx.draw(&mut self.layer, now, cursor, self.scale);
                self.layer
                    .composite(frame, width, height, (self.region.left, self.region.top));
            }
        }
        if self.cursor {
            self.paint_cursor(frame, width, height);
        }
        if !self.keys.rows.is_empty() {
            let _ = self.paint_keys(frame, width, height, now);
        }
    }

    fn paint_cursor(&mut self, frame: &mut [u8], width: u32, height: u32) {
        unsafe {
            let mut info = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if GetCursorInfo(&mut info).is_err() || info.flags != CURSOR_SHOWING {
                return;
            }
            if self
                .shape
                .as_ref()
                .is_none_or(|s| s.handle != info.hCursor.0 as usize)
            {
                self.shape = cursor_shape(info.hCursor);
            }
            let Some(shape) = self.shape.as_ref() else {
                return;
            };
            let left = info.ptScreenPos.x - shape.hotspot.0 - self.region.left;
            let top = info.ptScreenPos.y - shape.hotspot.1 - self.region.top;
            let x0 = left.max(0);
            let y0 = top.max(0);
            let x1 = (left + shape.size.0).min(width as i32);
            let y1 = (top + shape.size.1).min(height as i32);
            if x1 <= x0 || y1 <= y0 {
                return;
            }
            let rect = (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
            let size = shape.size;
            let Ok(surface) = Surface::ensure(&mut self.surface, rect.2, rect.3) else {
                return;
            };
            surface.load(frame, width, rect);
            if DrawIconEx(
                surface.dc,
                left - x0,
                top - y0,
                HICON(info.hCursor.0),
                size.0,
                size.1,
                0,
                None,
                DI_NORMAL,
            )
            .is_ok()
            {
                surface.store(frame, width, rect);
            }
        }
    }

    /// 按键提示：按设置贴在四角或底部居中的深色胶囊，组合键逐个画成键帽，“×N”用次要色跟在后面。
    /// 最新一行贴边，旧行向内堆叠并减淡；各行按所在侧对齐，整行淡入淡出。
    fn paint_keys(
        &mut self,
        frame: &mut [u8],
        width: u32,
        height: u32,
        now: Instant,
    ) -> Result<()> {
        struct Piece {
            text: Vec<u16>,
            x: i32,
            y: i32,
            size: SIZE,
            cap: bool,
        }
        let s = self.scale;
        let px = |dip: f32| (dip * s).round() as i32;
        let (pill_pad, cap_pad_x, cap_pad_y, cap_gap, row_gap) =
            (px(5.), px(8.), px(3.), px(4.), px(6.));
        let surface = Surface::ensure(&mut self.surface, 1, 1)?;
        let dc = surface.dc;
        let measure = |text: &[u16]| unsafe {
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(dc, text, &mut size);
            size
        };
        let old = unsafe { SelectObject(dc, self.font.into()) };
        let mut rows = Vec::with_capacity(self.keys.rows.len());
        for row in &self.keys.rows {
            let mut pieces: Vec<Piece> = row
                .caps()
                .map(|cap| {
                    let text: Vec<u16> = cap.encode_utf16().collect();
                    let size = measure(&text);
                    Piece {
                        text,
                        x: 0,
                        y: 0,
                        size,
                        cap: true,
                    }
                })
                .collect();
            if row.count > 1 {
                let text: Vec<u16> = format!("×{}", row.count).encode_utf16().collect();
                let size = measure(&text);
                pieces.push(Piece {
                    text,
                    x: 0,
                    y: 0,
                    size,
                    cap: false,
                });
            }
            rows.push((row.opacity(now), pieces));
        }
        unsafe { SelectObject(dc, old) };
        let text_h = rows
            .iter()
            .flat_map(|(_, pieces)| pieces.iter().map(|p| p.size.cy))
            .max()
            .unwrap_or(0);
        let cap_h = text_h + cap_pad_y * 2;
        let pill_h = cap_h + pill_pad * 2;
        // 从最新一行起贴边排布，旧行向画面内侧堆叠；记下每行胶囊矩形（左、上、右、下）。
        let position = self.key_position;
        let mut pills = Vec::with_capacity(rows.len());
        let margin = px(22.);
        let mut edge = if position.top() {
            margin
        } else {
            height as i32 - margin
        };
        for (_, pieces) in rows.iter_mut().rev() {
            let mut x = pill_pad;
            for piece in pieces.iter_mut() {
                let (lead, trail) = if piece.cap {
                    (cap_pad_x, cap_pad_x)
                } else {
                    (px(2.), px(2.))
                };
                piece.x = x + lead;
                x = piece.x + piece.size.cx + trail + cap_gap;
            }
            let pill_w = x - cap_gap + pill_pad;
            let left = margin
                + ((width as i32 - margin * 2 - pill_w) as f32 * position.align()).round() as i32;
            let top = if position.top() { edge } else { edge - pill_h };
            for piece in pieces.iter_mut() {
                piece.x += left;
                piece.y = top + pill_pad + cap_pad_y + (text_h - piece.size.cy) / 2;
            }
            pills.push((left, top, left + pill_w, top + pill_h));
            edge = if position.top() {
                top + pill_h + row_gap
            } else {
                top - row_gap
            };
        }
        pills.reverse();
        let block = pills
            .iter()
            .fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |b, p| {
                (b.0.min(p.0), b.1.min(p.1), b.2.max(p.2), b.3.max(p.3))
            });
        if block.2 <= block.0 || block.3 <= block.1 {
            return Ok(());
        }
        self.layer.reset(block);
        // 文字先以白字黑底画成灰度遮罩，再按行透明度着色，淡入淡出时文字随底板一起变化。
        let (bw, bh) = ((block.2 - block.0) as u32, (block.3 - block.1) as u32);
        let surface = Surface::ensure(&mut self.surface, bw, bh)?;
        unsafe {
            let _ = GdiFlush();
            std::slice::from_raw_parts_mut(
                surface.bits,
                (surface.width * surface.height * 4) as usize,
            )
            .fill(0);
            let old = SelectObject(surface.dc, self.font.into());
            SetBkMode(surface.dc, TRANSPARENT);
            SetTextColor(surface.dc, COLORREF(0x00FFFFFF));
            for (_, pieces) in &rows {
                for piece in pieces {
                    let _ = TextOutW(
                        surface.dc,
                        piece.x - block.0,
                        piece.y - block.1,
                        &piece.text,
                    );
                }
            }
            SelectObject(surface.dc, old);
            let _ = GdiFlush();
        }
        let mask = unsafe {
            std::slice::from_raw_parts(surface.bits, (surface.width * surface.height * 4) as usize)
        };
        let stride = surface.width as usize;
        let newest = rows.len() - 1;
        let radius = 9. * s;
        let cap_radius = 5. * s;
        for (index, ((opacity, pieces), pill)) in rows.iter().zip(&pills).enumerate() {
            let alpha = opacity * if index == newest { 1. } else { 0.72 };
            if alpha <= 0. {
                continue;
            }
            let rect = (pill.0 as f32, pill.1 as f32, pill.2 as f32, pill.3 as f32);
            self.layer
                .round_rect(rect, radius, [0x18, 0x18, 0x1B], 0.84 * alpha);
            self.layer
                .round_rect_stroke(rect, radius, s.max(1.), [255, 255, 255], 0.1 * alpha);
            for piece in pieces {
                if piece.cap {
                    let cap_top = (pill.1 + pill_pad) as f32;
                    let cap = (
                        (piece.x - cap_pad_x) as f32,
                        cap_top,
                        (piece.x + piece.size.cx + cap_pad_x) as f32,
                        cap_top + cap_h as f32,
                    );
                    self.layer
                        .round_rect(cap, cap_radius, [255, 255, 255], 0.13 * alpha);
                }
                let color = if piece.cap {
                    [0xF4, 0xF4, 0xF5]
                } else {
                    [0xA1, 0xA1, 0xAA]
                };
                for y in piece.y.max(block.1)..(piece.y + piece.size.cy).min(block.3) {
                    for x in piece.x.max(block.0)..(piece.x + piece.size.cx).min(block.2) {
                        let i = ((y - block.1) as usize * stride + (x - block.0) as usize) * 4;
                        let m = mask[i].max(mask[i + 1]).max(mask[i + 2]);
                        if m > 0 {
                            self.layer.over(x, y, color, f32::from(m) / 255. * alpha);
                        }
                    }
                }
            }
        }
        self.layer.composite(frame, width, height, (0, 0));
        Ok(())
    }
}
impl Drop for FramePainter {
    fn drop(&mut self) {
        self.surface.take();
        if !self.font.is_invalid() {
            unsafe {
                let _ = DeleteObject(self.font.into());
            }
        }
    }
}

/// 区域所在显示器的 DPI 缩放；叠加元素按此放大。
pub(crate) fn region_scale(region: ScreenRect) -> f32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: region.left + region.width as i32 / 2,
                y: region.top + region.height as i32 / 2,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let (mut x, mut y) = (96, 96);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() && x > 0 {
            x as f32 / 96.
        } else {
            1.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_key_formatter_filters_typing_and_formats_combos() {
        let mut keys = KeyFormatter::new(true);
        assert_eq!(keys.key_down(0x41), None, "裸字母属于打字");
        keys.key_up(0x41);
        assert_eq!(keys.key_down(0x0D).as_deref(), Some("Enter"));
        keys.key_up(0x0D);
        keys.key_down(u32::from(VK_LCONTROL.0));
        keys.key_down(u32::from(VK_LSHIFT.0));
        assert_eq!(keys.key_down(0x53).as_deref(), Some("Ctrl + Shift + S"));
        assert_eq!(keys.key_down(0x53), None, "按住自动重复只显示一次");
        keys.key_up(0x53);
        keys.key_up(u32::from(VK_LCONTROL.0));
        assert_eq!(keys.key_down(0x31), None, "裸 Shift + 数字仍是打字");
        keys.key_up(0x31);
        keys.key_up(u32::from(VK_LSHIFT.0));
        assert_eq!(keys.key_down(0x61).as_deref(), Some("Num 1"));
        let mut all = KeyFormatter::new(false);
        assert_eq!(all.key_down(0x41).as_deref(), Some("A"));
        assert_eq!(key_name(0x7B), "F12");
        assert_eq!(key_name(0x25), "←");
    }

    #[test]
    fn recording_key_rows_merge_repeat_and_expire() {
        let base = Instant::now();
        let mut rows = KeyRows::default();
        rows.push("Ctrl + C".into(), base);
        rows.push("Ctrl + C".into(), base + Duration::from_millis(500));
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0].text(), "Ctrl + C ×2");
        rows.push("Ctrl + C".into(), base + Duration::from_millis(1700));
        assert_eq!(rows.rows.len(), 2);
        for i in 0..5 {
            rows.push(format!("F{i}"), base + Duration::from_millis(1800));
        }
        assert_eq!(rows.rows.len(), MAX_KEY_ROWS);
        rows.prune(base + Duration::from_millis(1800) + KEY_ROW_LIFETIME);
        assert!(rows.rows.is_empty());
    }

    #[test]
    fn recording_overlay_layer_clips_and_composites() {
        let (w, h) = (40_u32, 30_u32);
        let mut frame = vec![0_u8; (w * h * 4) as usize];
        let mut layer = Layer::default();
        // 图层越出帧的左上和右下，绘制与合成都只落在交集内。
        layer.reset((-10, -10, 50, 40));
        layer.ring((2., 2.), 10., 2., [255, 0, 0], 1.);
        layer.round_rect((-5., 20., 55., 40.), 6., [0, 0, 255], 0.5);
        layer.chevron((38., 26.), [(34., 30.), (42., 30.)], 2., [0, 255, 0], 1.);
        layer.composite(&mut frame, w, h, (0, 0));
        assert!(
            frame.chunks_exact(4).any(|p| p[2] > 200),
            "红色光圈写入 R 通道"
        );
        assert!(frame.chunks_exact(4).any(|p| p[0] > 100), "底板写入 B 通道");
        assert!(frame.chunks_exact(4).any(|p| p[1] > 200), "箭头写入 G 通道");
        assert_eq!(frame[3], 0, "合成不改动帧的 alpha");
    }

    #[test]
    fn recording_mouse_fx_press_release_and_wheel() {
        let base = Instant::now();
        let point = POINT { x: 100, y: 100 };
        let clip = ScreenRect {
            left: 0,
            top: 0,
            width: 400,
            height: 300,
        };
        let mut fx = MouseFx::default();
        fx.event(MouseKind::Left, true, point, base);
        assert!(fx.is_idle(), "没见过按下的松开不出效果");
        fx.event(MouseKind::Left, false, point, base);
        assert_eq!(fx.held.len(), 1);
        fx.event(
            MouseKind::Left,
            true,
            point,
            base + Duration::from_millis(80),
        );
        assert!(fx.held.is_empty());
        assert_eq!(fx.ripples.len(), 1);
        let bounds = fx.bounds(point, 1., clip).unwrap();
        assert!(bounds.0 < 100 && bounds.2 > 100 && bounds.0 >= 0);
        fx.prune(base + Duration::from_millis(80) + RIPPLE_LIFETIME);
        assert!(fx.is_idle(), "外扩结束即清除");

        fx.event(MouseKind::WheelDown, false, point, base);
        let first = fx.wheel.unwrap().first;
        fx.event(
            MouseKind::WheelUp,
            false,
            point,
            base + Duration::from_millis(200),
        );
        assert_eq!(fx.wheel.unwrap().first, first, "可见时换向不重新淡入");
        fx.prune(base + Duration::from_millis(200) + WHEEL_HOLD + WHEEL_FADE);
        assert!(fx.wheel.is_none());

        fx.event(MouseKind::Right, false, point, base);
        fx.prune(base + HOLD_LIMIT);
        assert!(fx.held.is_empty(), "漏掉松开时按下光圈有上限");
        let far = POINT { x: -500, y: -500 };
        fx.event(MouseKind::Left, false, far, base);
        assert_eq!(fx.bounds(far, 1., clip), None, "选区外的效果不绘制");
    }

    #[test]
    fn recording_key_position_places_pills() {
        let (w, h) = (400_u32, 300_u32);
        let region = ScreenRect {
            left: 0,
            top: 0,
            width: w,
            height: h,
        };
        let (mut painter, _) =
            FramePainter::new(region, 1., false, false, false, true, KeyPosition::default());
        let now = Instant::now();
        painter.keys.push("Ctrl + C".into(), now);
        painter.keys.push("Shift + Tab".into(), now);
        let painted = |painter: &mut FramePainter| {
            let mut frame = vec![0xFF_u8; (w * h * 4) as usize];
            painter
                .paint_keys(&mut frame, w, h, now + Duration::from_millis(500))
                .unwrap();
            let mut b = (u32::MAX, u32::MAX, 0, 0);
            for (i, px) in frame.chunks_exact(4).enumerate() {
                if px[0] != 0xFF {
                    let (x, y) = (i as u32 % w, i as u32 / w);
                    b = (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y));
                }
            }
            b
        };
        // 边距 22；抗锯齿边缘允许差 1 像素。
        let near = |a: u32, b: u32| a.abs_diff(b) <= 1;
        for position in KeyPosition::ALL {
            painter.key_position = position;
            let (l, t, r, b) = painted(&mut painter);
            assert!(l < r && t < b, "{position:?} 应有绘制");
            let ok = match position {
                KeyPosition::TopLeft => near(l, 22) && near(t, 22) && r < w / 2,
                KeyPosition::TopRight => near(r, w - 23) && near(t, 22) && l > w / 2,
                KeyPosition::BottomLeft => near(l, 22) && near(b, h - 23),
                KeyPosition::BottomRight => near(r, w - 23) && near(b, h - 23),
                KeyPosition::BottomCenter => near(b, h - 23) && (l + r).abs_diff(w) <= 2,
            };
            assert!(ok, "{position:?}: {l},{t}..{r},{b}");
        }
    }

    /// 调样式用的静帧对照：浅色、深色两半背景上排出各阶段效果与按键提示。
    /// `RUSTSHOT_OVERLAY_PREVIEW=<png> cargo test --lib recording_overlay_preview -- --ignored`
    #[test]
    #[ignore]
    fn recording_overlay_preview() {
        let path = std::env::var("RUSTSHOT_OVERLAY_PREVIEW").expect("输出路径");
        let s = 1.5;
        let (w, h) = (1200_u32, 640_u32);
        let mut frame = vec![0_u8; (w * h * 4) as usize];
        for (i, px) in frame.chunks_exact_mut(4).enumerate() {
            let light = (i as u32 % w) < w / 2;
            px.copy_from_slice(if light {
                &[0xF6, 0xF4, 0xF3, 255]
            } else {
                &[0x37, 0x29, 0x1F, 255]
            });
        }
        let region = ScreenRect {
            left: 0,
            top: 0,
            width: w,
            height: h,
        };
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let mut layer = Layer::default();
        // 每半边：按下中、按下稳定、松开 15%／40%／70%、右键按下、滚轮下／右。
        for half in 0..2 {
            let x0 = half * w as i32 / 2 + 60;
            let shots: [(i32, i32, &dyn Fn(&mut MouseFx) -> Instant); 8] = [
                (0, 80, &|fx| {
                    fx.event(MouseKind::Left, false, POINT::default(), at(0));
                    at(40)
                }),
                (110, 80, &|fx| {
                    fx.event(MouseKind::Left, false, POINT::default(), at(0));
                    at(400)
                }),
                (220, 80, &|fx| {
                    fx.event(MouseKind::Left, false, POINT::default(), at(0));
                    fx.event(MouseKind::Left, true, POINT { x: 0, y: 0 }, at(10));
                    at(10 + 70)
                }),
                (330, 80, &|fx| {
                    fx.event(MouseKind::Left, false, POINT::default(), at(0));
                    fx.event(MouseKind::Left, true, POINT { x: 0, y: 0 }, at(10));
                    at(10 + 185)
                }),
                (440, 80, &|fx| {
                    fx.event(MouseKind::Left, false, POINT::default(), at(0));
                    fx.event(MouseKind::Left, true, POINT { x: 0, y: 0 }, at(10));
                    at(10 + 320)
                }),
                (0, 220, &|fx| {
                    fx.event(MouseKind::Right, false, POINT::default(), at(0));
                    at(400)
                }),
                (110, 220, &|fx| {
                    fx.event(MouseKind::WheelDown, false, POINT::default(), at(0));
                    at(200)
                }),
                (240, 220, &|fx| {
                    fx.event(MouseKind::WheelRight, false, POINT::default(), at(0));
                    at(60)
                }),
            ];
            for (dx, y, setup) in shots {
                let mut fx = MouseFx::default();
                let now = setup(&mut fx);
                // 下移避开顶部的按键提示。
                let cursor = POINT {
                    x: x0 + dx,
                    y: y + 150,
                };
                for r in &mut fx.ripples {
                    r.point = cursor;
                }
                let bounds = fx.bounds(cursor, s, region).unwrap();
                layer.reset(bounds);
                fx.draw(&mut layer, now, cursor, s);
                layer.composite(&mut frame, w, h, (0, 0));
            }
        }
        let (mut painter, _) = FramePainter::new(
            region,
            s,
            false,
            false,
            false,
            true,
            KeyPosition::default(),
        );
        painter.keys.push("Ctrl + C".into(), at(0));
        painter.keys.push("Ctrl + C".into(), at(300));
        painter.keys.push("Ctrl + Shift + S".into(), at(900));
        for position in KeyPosition::ALL {
            painter.key_position = position;
            painter.paint_keys(&mut frame, w, h, at(1100)).unwrap();
        }
        crate::export::save_png_pixels(w, h, &frame, std::path::Path::new(&path)).unwrap();
    }

    /// 屏幕同步小窗：在主屏左上角跑一次左键点击，确认显示、排除采集、空闲后隐藏停表。
    /// 小窗鼠标穿透且只闪约半秒。`cargo test --lib recording_input_mirror_smoke -- --ignored`
    #[test]
    #[ignore]
    fn recording_input_mirror_smoke() {
        let shared = Arc::new(HookShared::default());
        shared.live.store(true, Ordering::Relaxed);
        let region = ScreenRect {
            left: 0,
            top: 0,
            width: 200,
            height: 200,
        };
        let mirror = Mirror::new(shared.clone(), region, 1.).expect("建小窗");
        let hwnd = mirror.hwnd;
        MIRROR.with(|m| *m.borrow_mut() = Some(mirror));
        let mut affinity = 0;
        unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity).unwrap() };
        assert_eq!(affinity, WDA_EXCLUDEFROMCAPTURE.0, "小窗必须排除采集");
        let point = POINT { x: 60, y: 60 };
        Mirror::event(MouseKind::Left, false, point);
        Mirror::event(MouseKind::Left, true, point);
        let start = Instant::now();
        let mut seen = None;
        let mut message = MSG::default();
        while start.elapsed() < Duration::from_millis(900) {
            unsafe {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_TIMER && Mirror::owns(message.hwnd) {
                        Mirror::tick();
                    } else {
                        DispatchMessageW(&message);
                    }
                }
                if seen.is_none() && IsWindowVisible(hwnd).as_bool() {
                    let mut rect = windows::Win32::Foundation::RECT::default();
                    GetWindowRect(hwnd, &mut rect).unwrap();
                    seen = Some(rect);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let rect = seen.expect("点击后小窗应显示");
        assert!(rect.left <= 60 && rect.right > 60 && rect.top <= 60 && rect.bottom > 60);
        assert!(rect.left >= 0 && rect.top >= 0, "裁在选区内");
        assert!(
            !unsafe { IsWindowVisible(hwnd) }.as_bool(),
            "效果结束后隐藏"
        );
        MIRROR.with(|m| assert!(!m.borrow().as_ref().unwrap().ticking, "空闲后停表"));
        shared.live.store(false, Ordering::Relaxed);
        Mirror::event(MouseKind::Left, false, point);
        MIRROR.with(|m| assert!(m.borrow().as_ref().unwrap().fx.is_idle(), "暂停时不记效果"));
        MIRROR.with(|m| m.borrow_mut().take());
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool(), "释放即销毁窗口");
    }
}
