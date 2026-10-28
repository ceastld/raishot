//! Windows 输入适配。Hook 只负责同步决策，触发通知交由外部 UI 处理。

use omnitrigger_core::{Activation, Button, MouseButton, TriggerSpec, WheelDirection};

/// 专用于诊断 Hook 链顺序的注入标记；探针只计数，不进入触发器状态机。
pub const PRIORITY_PROBE_TAG: usize = 0x4f4d_5052;

/// 监听线程的观测快照。计数表示收到的回调，不代表系统全部输入。
#[derive(Debug, Clone, Default)]
pub struct ListenerDiagnostics {
    pub keyboard_events: u64,
    pub mouse_events: u64,
    pub external_injected: u64,
    pub self_replayed: u64,
    pub probe_events: u64,
    pub foreground_cancellations: u64,
    pub replay_failures: u64,
    pub held: Vec<Button>,
    pub owned: Vec<Button>,
    /// 距最近键盘回调的毫秒数，None 表示尚未收到。
    pub last_keyboard_ms: Option<u64>,
    /// 距最近鼠标回调的毫秒数，None 表示尚未收到。
    pub last_mouse_ms: Option<u64>,
}

#[derive(Debug)]
pub enum BackendCommand {
    SetBindings(Vec<TriggerSpec>),
    StartRecording,
    CancelRecording,
    SetEnabled(bool),
    GetDiagnostics,
    Shutdown,
}

#[derive(Debug)]
pub enum BackendEvent {
    Ready,
    /// 录制期间当前按住的按键（不含录制前已按住的键），按下集合变化时发送。
    RecordingProgress(Vec<Button>),
    Recorded(TriggerSpec),
    Triggered(u64),
    Error(String),
    RecordingCancelled,
    Diagnostics(ListenerDiagnostics),
}

#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod title_bar;
#[cfg(windows)]
pub use platform::Backend;

/// 显示由本应用持有的隐藏提示窗口，保留输入焦点和原有扩展样式。
#[cfg(windows)]
pub fn show_notification_window(
    hwnd: isize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), String> {
    platform::show_notification_window(hwnd, x, y, width, height)
}

#[cfg(windows)]
pub use title_bar::CaptionButtons;

/// 主窗口内容延伸到标题栏，保留系统绘制的最小化、最大化、关闭按钮。
#[cfg(windows)]
pub fn extend_into_title_bar(hwnd: isize) -> Result<(), String> {
    title_bar::extend_into_title_bar(hwnd)
}

/// 系统标题按钮在客户区右上角占据的物理像素尺寸。
#[cfg(windows)]
pub fn caption_buttons(hwnd: isize) -> Option<CaptionButtons> {
    title_bar::caption_buttons(hwnd)
}

/// 设置标题按钮下方的底色，`rgb` 为 0xRRGGBB；系统不支持时保持默认。
#[cfg(windows)]
pub fn set_caption_color(hwnd: isize, rgb: u32) {
    title_bar::set_caption_color(hwnd, rgb)
}

/// 本地时间的时、分、秒，用于界面记录；不参与触发判定。
#[cfg(windows)]
pub fn local_clock() -> (u16, u16, u16) {
    platform::local_clock()
}

pub fn format_button(button: &Button) -> String {
    match button {
        Button::Mouse(mouse) => match mouse {
            MouseButton::Left => "鼠标左键".into(),
            MouseButton::Right => "鼠标右键".into(),
            MouseButton::Middle => "鼠标中键".into(),
            MouseButton::X1 => "鼠标侧键 1".into(),
            MouseButton::X2 => "鼠标侧键 2".into(),
        },
        Button::Key(key) => format_key(*key),
    }
}

fn format_key(key: u16) -> String {
    match key {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0d => "Enter".into(),
        0x14 => "CapsLock".into(),
        0x1b => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        0x2d => "Insert".into(),
        0x2e => "Delete".into(),
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(key.into()).unwrap().into(),
        0x5b => "左 Win".into(),
        0x5c => "右 Win".into(),
        0x70..=0x87 => format!("F{}", key - 0x6f),
        0xa0 => "左 Shift".into(),
        0xa1 => "右 Shift".into(),
        0xa2 => "左 Ctrl".into(),
        0xa3 => "右 Ctrl".into(),
        0xa4 => "左 Alt".into(),
        0xa5 => "右 Alt".into(),
        _ => {
            #[cfg(windows)]
            if let Some(name) = platform::key_name(key) {
                return name;
            }
            format!("VK {key:02X}")
        }
    }
}

pub fn format_trigger(spec: &TriggerSpec) -> String {
    let mut parts: Vec<String> = spec.held.iter().map(format_button).collect();
    parts.push(match &spec.activation {
        Activation::Press(button) => format_button(button),
        Activation::Wheel(direction) => match direction {
            WheelDirection::Up => "滚轮上滚".into(),
            WheelDirection::Down => "滚轮下滚".into(),
            WheelDirection::Left => "滚轮左滚".into(),
            WheelDirection::Right => "滚轮右滚".into(),
        },
    });
    parts.join(" + ")
}
