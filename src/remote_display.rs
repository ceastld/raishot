//! 远程画面兼容。
//!
//! 工具窗（长截图面板／边框／预览、录屏工具栏／边框／提示、贴图装饰）原本用
//! `WDA_EXCLUDEFROMCAPTURE` 保证不进入本机采集；但远程桌面（RDP）和依赖桌面采集传输画面的
//! 远控软件同样看不到这些窗口，远端用户会看到工具界面整片消失。
//!
//! 分三级（`Exposure`）：
//! - `Remote`：确认在远程画面中。设置 `remoteDisplay=on`，或 `auto` 下处于远程会话
//!   （`SM_REMOTESESSION`、`SM_REMOTECONTROL`、当前会话不是物理控制台），或本次框选收到足够多
//!   经 `SendInput` 注入的输入（远控软件的通常做法）。工具窗全部可见，边框画在选区外，
//!   压住采集区的预览不显示。
//! - `Possible`：`auto` 下本机正运行远控软件，但没有确认正被控制（驱动级注入测不到）。
//!   不压采集区的面板／工具栏／预览可见——它们本来就不会进成品；压住时仍排除采集。
//! - `Local`：维持排除采集。
//!
//! 结论在进程内首次查询时定下：同一窗口的创建与后续校验看到同一结论。截图／录屏会话进程
//! 在框选结束后才创建工具窗，届时注入输入已被统计。`RUSTSHOT_REMOTE_DISPLAY` 可覆盖设置，供验证。
use crate::geometry::ScreenRect;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    OnceLock,
};
use windows::{
    core::{w, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND},
        System::{
            ProcessStatus::EnumProcesses,
            Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD},
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
        UI::{
            Input::{GetCurrentInputMessageSource, IMO_INJECTED, INPUT_MESSAGE_SOURCE},
            WindowsAndMessaging::{
                GetSystemMetrics, GetWindowDisplayAffinity, SetWindowDisplayAffinity,
                SM_REMOTECONTROL, SM_REMOTESESSION, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
                WINDOW_DISPLAY_AFFINITY,
            },
        },
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemoteDisplayMode {
    #[default]
    Auto,
    On,
    Off,
}
impl RemoteDisplayMode {
    pub const ALL: [Self; 3] = [Self::Auto, Self::On, Self::Off];
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
    /// 未知值回到自动。
    pub fn from_wire(value: &str) -> Self {
        let value = value.trim();
        Self::ALL
            .into_iter()
            .find(|mode| mode.wire_name().eq_ignore_ascii_case(value))
            .unwrap_or_default()
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => tr!("自动", "Auto"),
            Self::On => tr!("始终兼容", "Always"),
            Self::Off => tr!("关闭", "Off"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exposure {
    Local,
    Possible,
    Remote,
}

/// 达到该数量的注入输入才认定为远控，避免偶发的单次模拟按键（按键映射、鼠标防休眠）误判。
const INJECTED_THRESHOLD: u32 = 8;
static INJECTED_INPUT: AtomicU32 = AtomicU32::new(0);
static EXPOSURE: OnceLock<Exposure> = OnceLock::new();

/// 在窗口过程内同步调用：输入来源只在处理该输入消息期间可读。
pub(crate) fn observe_input_source() {
    if INJECTED_INPUT.load(Ordering::Relaxed) >= INJECTED_THRESHOLD {
        return;
    }
    let mut source = INPUT_MESSAGE_SOURCE::default();
    if unsafe { GetCurrentInputMessageSource(&mut source) }.is_ok()
        && source.originId == IMO_INJECTED
    {
        INJECTED_INPUT.fetch_add(1, Ordering::Relaxed);
    }
}

fn exposure() -> Exposure {
    *EXPOSURE.get_or_init(|| {
        let mode = std::env::var("RUSTSHOT_REMOTE_DISPLAY")
            .map(|value| RemoteDisplayMode::from_wire(&value))
            .unwrap_or_else(|_| crate::preferences::Preferences::load().remote_display);
        decide(
            mode,
            || injected_input_seen() || remote_session(),
            remote_control_software_running,
        )
    })
}

/// 专项验证本机排除采集的合同；须在本进程创建任何工具窗之前调用。
pub(crate) fn force_local_for_smoke() {
    let _ = EXPOSURE.set(Exposure::Local);
}

fn decide(
    mode: RemoteDisplayMode,
    confirmed: impl FnOnce() -> bool,
    software: impl FnOnce() -> bool,
) -> Exposure {
    match mode {
        RemoteDisplayMode::On => Exposure::Remote,
        RemoteDisplayMode::Off => Exposure::Local,
        RemoteDisplayMode::Auto if confirmed() => Exposure::Remote,
        RemoteDisplayMode::Auto if software() => Exposure::Possible,
        RemoteDisplayMode::Auto => Exposure::Local,
    }
}

fn injected_input_seen() -> bool {
    INJECTED_INPUT.load(Ordering::Relaxed) >= INJECTED_THRESHOLD
}

/// 本进程是否确认在远程画面中：边框外移、预览避让采集区都看它。
pub(crate) fn compatible() -> bool {
    exposure() == Exposure::Remote
}

/// 始终压在采集区上的工具窗（边框、绘图层、提示、贴图装饰）应设置的显示亲和性；
/// 校验已有窗口时也按它比较。
pub(crate) fn capture_affinity() -> WINDOW_DISPLAY_AFFINITY {
    if compatible() {
        WDA_NONE
    } else {
        WDA_EXCLUDEFROMCAPTURE
    }
}

/// 本机排除采集；远程兼容时保持普通显示，远端画面才能看到该窗口。
pub(crate) fn exclude_from_capture(hwnd: HWND) -> windows::core::Result<()> {
    unsafe { SetWindowDisplayAffinity(hwnd, capture_affinity()) }
}

/// 面板跟随选区或被拖动的位置在轮询间才同步，外扩这一段防止移进采集区的那几帧被采到。
const TOOL_MARGIN: i32 = 96;

/// 会移动的面板／工具栏／预览：不压采集区时在 `Possible` 下也可见。
pub(crate) fn tool_affinity(window: ScreenRect, capture: ScreenRect) -> WINDOW_DISPLAY_AFFINITY {
    tool_affinity_for(exposure(), window, capture)
}

fn tool_affinity_for(
    exposure: Exposure,
    window: ScreenRect,
    capture: ScreenRect,
) -> WINDOW_DISPLAY_AFFINITY {
    match exposure {
        Exposure::Remote => WDA_NONE,
        Exposure::Possible if !near(window, capture, TOOL_MARGIN) => WDA_NONE,
        _ => WDA_EXCLUDEFROMCAPTURE,
    }
}

fn near(a: ScreenRect, b: ScreenRect, margin: i32) -> bool {
    let right = |r: ScreenRect| i64::from(r.left) + i64::from(r.width);
    let bottom = |r: ScreenRect| i64::from(r.top) + i64::from(r.height);
    let margin = i64::from(margin);
    i64::from(a.left) < right(b) + margin
        && right(a) > i64::from(b.left) - margin
        && i64::from(a.top) < bottom(b) + margin
        && bottom(a) > i64::from(b.top) - margin
}

/// 移动前调用：目标位置需要排除时先排除，移动后再由 `sync_tool_window` 按新位置放开。
pub(crate) fn guard_tool_move(hwnd: HWND, target: ScreenRect, capture: ScreenRect) {
    if tool_affinity(target, capture) == WDA_EXCLUDEFROMCAPTURE {
        let _ = unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) };
    }
}

/// 按窗口当前位置同步可移动工具窗的亲和性；与现值相同时不调用系统接口。
pub(crate) fn sync_tool_window(hwnd: HWND, captures: &[ScreenRect]) {
    let Ok(rect) = crate::pin_native::rect(hwnd) else {
        return;
    };
    let window = ScreenRect {
        left: rect.x,
        top: rect.y,
        width: rect.width,
        height: rect.height,
    };
    let want = if captures
        .iter()
        .all(|capture| tool_affinity(window, *capture) == WDA_NONE)
    {
        WDA_NONE
    } else {
        WDA_EXCLUDEFROMCAPTURE
    };
    let mut current = 0;
    unsafe {
        if GetWindowDisplayAffinity(hwnd, &mut current).is_ok() && current == want.0 {
            return;
        }
        let _ = SetWindowDisplayAffinity(hwnd, want);
    }
}

/// 常见远控软件的被控端进程名片段（小写，按文件名匹配）。它们常驻托盘，
/// 只说明“可能被远控”，因此只放开不压采集区的工具窗。
const REMOTE_CONTROL_SOFTWARE: &[&str] = &[
    "todesk",
    "sunlogin",
    "awesun",
    "teamviewer",
    "anydesk",
    "rustdesk",
    "parsecd",
    "remoting_host",
    "raylink",
    "srserver",
    "winvnc",
    "tvnserver",
    "vncserver",
];

fn is_remote_control_software(file_name: &str) -> bool {
    let name = file_name.to_ascii_lowercase();
    REMOTE_CONTROL_SOFTWARE
        .iter()
        .any(|part| name.contains(part))
}

fn remote_control_software_running() -> bool {
    let mut ids = vec![0u32; 4096];
    let mut used = 0u32;
    let listed = unsafe {
        EnumProcesses(
            ids.as_mut_ptr(),
            (ids.len() * std::mem::size_of::<u32>()) as u32,
            &mut used,
        )
    };
    if listed.is_err() {
        return false;
    }
    ids.truncate(used as usize / std::mem::size_of::<u32>());
    ids.into_iter().filter(|id| *id != 0).any(|id| {
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, id) })
        else {
            return false;
        };
        let mut path = [0u16; 512];
        let mut length = path.len() as u32;
        let named = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(path.as_mut_ptr()),
                &mut length,
            )
        };
        unsafe {
            let _ = CloseHandle(process);
        }
        named.is_ok() && {
            let path = String::from_utf16_lossy(&path[..length as usize]);
            is_remote_control_software(path.rsplit('\\').next().unwrap_or_default())
        }
    })
}

/// 当前会话是否经远程桌面显示。
pub(crate) fn remote_session() -> bool {
    let metrics = unsafe {
        GetSystemMetrics(SM_REMOTESESSION) != 0 || GetSystemMetrics(SM_REMOTECONTROL) != 0
    };
    metrics || !console_session()
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(process: u32, session: *mut u32) -> i32;
}

/// 部分远程桌面（如带虚拟 GPU 的会话）不置 `SM_REMOTESESSION`；物理控制台会话号记在
/// `GlassSessionId`，与本进程会话不同即为远程。读不到时按控制台处理。
fn console_session() -> bool {
    let mut glass = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Control\Terminal Server"),
            w!("GlassSessionId"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut glass as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    let mut session = 0u32;
    if read.is_err() || unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return true;
    }
    session == glass
}

impl crate::settings_view::SettingsView {
    pub(crate) fn remote_display_settings(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use crate::settings_widgets as widgets;
        use gpui::prelude::*;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        for (mode, id) in [
            (RemoteDisplayMode::Auto, "settings-remote-display-auto"),
            (RemoteDisplayMode::On, "settings-remote-display-on"),
            (RemoteDisplayMode::Off, "settings-remote-display-off"),
        ] {
            let selected = self.preferences.remote_display == mode;
            choices = choices.child(
                widgets::segment(id, mode.label(), selected, palette, surface)
                    .child(self.settings_probe(id))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        // 只在最新配置上改这一项；下次截图／录屏会话起生效。
                        let previous = crate::preferences::Preferences::load();
                        view.preferences = previous.clone();
                        view.preferences.remote_display = mode;
                        if !view.save() {
                            view.preferences = previous;
                        }
                        cx.notify();
                    })),
            );
        }
        widgets::row(
            tr!("远程画面兼容", "Remote display"),
            Some(tr!("远程桌面或远控软件里看不到长截图、录屏控制条时，改为“始终兼容”。", "If long-capture or recording controls are missing over Remote Desktop or remote-control apps, choose Always.").into()),
            palette,
        )
        .flex_wrap()
        .child(choices)
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_display_mode_wire_round_trip_and_unknown_is_auto() {
        for mode in RemoteDisplayMode::ALL {
            assert_eq!(RemoteDisplayMode::from_wire(mode.wire_name()), mode);
        }
        assert_eq!(RemoteDisplayMode::from_wire(" ON "), RemoteDisplayMode::On);
        assert_eq!(RemoteDisplayMode::from_wire("rdp"), RemoteDisplayMode::Auto);
    }

    #[test]
    fn remote_display_decision_respects_explicit_mode() {
        let yes = || true;
        let no = || false;
        assert_eq!(decide(RemoteDisplayMode::On, no, no), Exposure::Remote);
        assert_eq!(decide(RemoteDisplayMode::Off, yes, yes), Exposure::Local);
        assert_eq!(decide(RemoteDisplayMode::Auto, yes, no), Exposure::Remote);
        assert_eq!(decide(RemoteDisplayMode::Auto, no, yes), Exposure::Possible);
        assert_eq!(decide(RemoteDisplayMode::Auto, no, no), Exposure::Local);
    }

    #[test]
    fn possible_remote_shows_only_tools_clear_of_capture() {
        let capture = ScreenRect {
            left: 0,
            top: 0,
            width: 800,
            height: 600,
        };
        let below = ScreenRect {
            top: 600 + TOOL_MARGIN,
            ..capture
        };
        let touching = ScreenRect {
            top: 610,
            ..capture
        };
        assert_eq!(
            tool_affinity_for(Exposure::Possible, below, capture),
            WDA_NONE
        );
        assert_eq!(
            tool_affinity_for(Exposure::Possible, touching, capture),
            WDA_EXCLUDEFROMCAPTURE
        );
        assert_eq!(
            tool_affinity_for(Exposure::Local, below, capture),
            WDA_EXCLUDEFROMCAPTURE
        );
        assert_eq!(
            tool_affinity_for(Exposure::Remote, capture, capture),
            WDA_NONE
        );
    }

    #[test]
    fn remote_control_software_matches_file_names_case_insensitively() {
        for name in [
            "ToDesk.exe",
            "SunloginClient.exe",
            "AweSun.exe",
            "TeamViewer.exe",
        ] {
            assert!(is_remote_control_software(name), "{name}");
        }
        assert!(!is_remote_control_software("explorer.exe"));
        assert!(!is_remote_control_software("mstsc.exe"));
    }
}
