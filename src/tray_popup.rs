//! 托盘弹层：托盘再备一个不初始化 GPU 的待命进程，右键请求到达后才启动 GPUI 画弹层。
//! P0 只放固定布局并回报启动各阶段时刻，托盘仍用系统菜单；设计与阶段见
//! references/tray-popup-2026-10-24.md。
use crate::{
    capture_toolbar::Palette,
    pin_model::Placement,
    quick_action::Step,
    toolbar_icons::{self, Icon},
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, rgba, App, Application, Context, Div, FocusHandle, FontWeight,
    KeyDownEvent, Render, SharedString, Stateful, Subscription, Window, WindowBackgroundAppearance,
    WindowBounds, WindowKind, WindowOptions,
};
use std::{
    os::windows::process::CommandExt,
    process::{Child, Command},
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST},
        System::Threading::*,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
};

/// 待命 → 属主：就绪，wParam 为待命消息窗口，lParam 为进程号。
pub(crate) const READY: u32 = WM_APP + 0x70;
/// 属主 → 待命：打开弹层，wParam 打包锚点物理坐标，lParam 为请求 QPC。
const REQUEST: u32 = WM_APP + 0x71;
/// 待命 → 属主：启动阶段回执，wParam 为 [`Stage`]，lParam 为当时 QPC。
pub(crate) const STAGE: u32 = WM_APP + 0x72;

const WIDTH: f32 = 344.;
const HEIGHT: f32 = 646.;
const GAP: f32 = 8.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Received = 1,
    AppReady = 2,
    WindowOpened = 3,
    FirstRender = 4,
    /// 首个成功 Present 之后再 DwmFlush，是首帧可见的上界。
    Presented = 5,
    /// `Application::new()` 返回，平台与渲染设备已建好、尚未进入事件循环。
    AppCreated = 6,
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        attribute: u32,
        value: *const std::ffi::c_void,
        size: u32,
    ) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
}

/// 属主（托盘或测速程序）持有的弹层待命进程。
pub(crate) struct Standby {
    child: Child,
    window: Option<HWND>,
}
impl Standby {
    pub(crate) fn spawn(owner: HWND, extra: &[String]) -> Result<Self> {
        let mut command =
            Command::new(std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE));
        command.args([
            "--tray-popup-standby",
            &(owner.0 as usize).to_string(),
            &std::process::id().to_string(),
        ]);
        command.args(extra);
        Ok(Self {
            child: command.creation_flags(0x08000000).spawn()?,
            window: None,
        })
    }
    /// READY 只认本次启动的子进程。
    pub(crate) fn accept_ready(&mut self, wp: WPARAM, lp: LPARAM) -> bool {
        let accepted = lp.0 as u32 == self.child.id();
        if accepted {
            self.window = Some(HWND(wp.0 as *mut _));
        }
        accepted
    }
    /// 前台授权要在属主仍持有右键带来的前台权利时转交。
    pub(crate) fn request(&mut self, anchor: POINT, request_qpc: i64) -> Result<()> {
        let window = self.window.take().ok_or("弹层待命进程尚未就绪")?;
        unsafe {
            let _ = AllowSetForegroundWindow(self.child.id());
            PostMessageW(
                Some(window),
                REQUEST,
                WPARAM(pack(anchor)),
                LPARAM(request_qpc as isize),
            )?;
        }
        Ok(())
    }
}

fn pack(p: POINT) -> usize {
    (p.x as u32 as usize) | ((p.y as u32 as usize) << 32)
}
fn unpack(value: usize) -> POINT {
    POINT {
        x: value as u32 as i32,
        y: (value >> 32) as u32 as i32,
    }
}

fn report(owner: HWND, stage: Stage) {
    unsafe {
        let _ = PostMessageW(
            Some(owner),
            STAGE,
            WPARAM(stage as usize),
            LPARAM(crate::capture_standby::qpc() as isize),
        );
    }
}

/// `--tray-popup-standby <属主窗口> <属主进程号> [--exit-after-present] [--prewarm x,y]`。
pub(crate) fn run_standby(args: &[String]) -> Result<()> {
    let owner = args
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or("弹层待命缺少属主窗口")?;
    let owner = HWND(owner as *mut _);
    let owner_pid = args
        .get(2)
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or("弹层待命缺少属主进程号")?;
    let exit_after_present = args.iter().any(|arg| arg == "--exit-after-present");
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if let Some(index) = args.iter().position(|arg| arg == "--prewarm") {
        let anchor = args
            .get(index + 1)
            .and_then(|value| value.split_once(','))
            .and_then(|(x, y)| {
                Some(POINT {
                    x: x.parse().ok()?,
                    y: y.parse().ok()?,
                })
            })
            .ok_or("预热缺少锚点")?;
        return show_warm(owner, owner_pid, anchor, exit_after_present);
    }
    let announce = |window: HWND| -> Result<()> {
        unsafe {
            PostMessageW(
                Some(owner),
                READY,
                WPARAM(window.0 as usize),
                LPARAM(GetCurrentProcessId() as isize),
            )?;
        }
        Ok(())
    };
    let Some(anchor) = wait_request(owner_pid, announce)? else {
        return Ok(());
    };
    report(owner, Stage::Received);
    show(owner, anchor, exit_after_present)
}

/// 待命期间只有一个消息窗口，不碰 GPUI 与 D3D；属主退出时随之结束。
/// `announce` 拿到消息窗口后决定何时告诉属主就绪。
fn wait_request(
    owner_pid: u32,
    announce: impl FnOnce(HWND) -> Result<()>,
) -> Result<Option<POINT>> {
    unsafe {
        let parent = OpenProcess(PROCESS_SYNCHRONIZE, false, owner_pid)?;
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        );
        let window = match window {
            Ok(window) => window,
            Err(error) => {
                let _ = CloseHandle(parent);
                return Err(error.into());
            }
        };
        let result = (|| -> Result<Option<POINT>> {
            announce(window)?;
            loop {
                let status = MsgWaitForMultipleObjectsEx(
                    Some(&[parent]),
                    INFINITE,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
                if status == WAIT_OBJECT_0 {
                    return Ok(None);
                }
                if status == WAIT_FAILED {
                    return Err("弹层待命等待失败".into());
                }
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    if message.message == WM_QUIT {
                        return Ok(None);
                    }
                    if message.hwnd == window && message.message == REQUEST {
                        return Ok(Some(unpack(message.wParam.0)));
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        })();
        let _ = DestroyWindow(window);
        let _ = CloseHandle(parent);
        result
    }
}

fn monitor_scale(anchor: POINT) -> f32 {
    let monitor = unsafe { MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST) };
    let (mut x, mut y) = (96, 96);
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_ok() && x > 0 {
        x as f32 / 96.
    } else {
        1.
    }
}

/// 右对齐锚点、优先放在锚点上方；上方放不下（任务栏在顶部等）时放到下方，最后夹进工作区。
pub(crate) fn place(anchor: POINT, work: Placement, scale: f32) -> Placement {
    let width = (WIDTH * scale).round() as u32;
    let height = (HEIGHT * scale).round() as u32;
    let gap = (GAP * scale).round() as i32;
    let right = work.x + work.width as i32;
    let bottom = work.y + work.height as i32;
    let x = (anchor.x - width as i32).clamp(work.x, (right - width as i32).max(work.x));
    let above = anchor.y - gap - height as i32;
    let y = if above >= work.y {
        above
    } else {
        anchor.y + gap
    };
    let y = y.clamp(work.y, (bottom - height as i32).max(work.y));
    Placement {
        x,
        y,
        width,
        height,
    }
}

fn show(owner: HWND, anchor: POINT, exit_after_present: bool) -> Result<()> {
    let placement = place(
        anchor,
        crate::pin_native::work_area((anchor.x, anchor.y)),
        monitor_scale(anchor),
    );
    let failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let reported = failure.clone();
    let application = Application::new();
    report(owner, Stage::AppCreated);
    application
        .with_assets(toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            report(owner, Stage::AppReady);
            crate::theme::install(cx);
            let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
            // 测速时不抢焦点：测速程序本身没有前台，抢也只会失败。
            let activate = !exit_after_present;
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    display_id,
                    kind: WindowKind::PopUp,
                    window_background: WindowBackgroundAppearance::Transparent,
                    focus: activate,
                    ..Default::default()
                },
                move |window, cx| {
                    native_setup(crate::gpui_capture::hwnd(window), placement, activate);
                    watch_present(owner, exit_after_present, 0, window, 600);
                    cx.new(|cx| TrayPopup::new(owner, true, window, cx))
                },
            );
            match opened {
                Ok(_) => {
                    report(owner, Stage::WindowOpened);
                    if activate {
                        cx.activate(true);
                    }
                }
                Err(error) => {
                    *reported.borrow_mut() = Some(error.to_string());
                    cx.quit();
                }
            }
        });
    let failure = failure.borrow_mut().take();
    failure.map_or(Ok(()), |error| Err(error.into()))
}

fn cloak(h: HWND, cloaked: bool) {
    let value = i32::from(cloaked);
    // DWMWA_CLOAK：隐身期间窗口照常绘制、Present，只是不合成到屏幕。
    unsafe { DwmSetWindowAttribute(h, 13, (&value as *const i32).cast(), 4) };
}

/// 悬停预热的测速形态：先启动 GPUI 并在隐身窗口里画好一帧，再报就绪；
/// 请求到达后只需移动、显形并重画。
fn show_warm(owner: HWND, owner_pid: u32, guess: POINT, exit_after_present: bool) -> Result<()> {
    // 等待线程只管消息窗口；结果经 oneshot 唤醒 GPUI，不靠定时轮询。
    // 收到 None 表示属主已退出或等待失败。
    let (sender, receiver) = futures::channel::oneshot::channel::<Option<POINT>>();
    let (window_sender, window_receiver) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let result = wait_request(owner_pid, |window| {
            window_sender
                .send(window.0 as usize)
                .map_err(|_| "弹层预热已退出".into())
        });
        if let Err(error) = result.as_ref() {
            eprintln!("托盘弹层预热等待失败：{error}");
        }
        let _ = sender.send(result.ok().flatten());
    });
    let listener = window_receiver
        .recv()
        .map_err(|_| "弹层预热消息窗口创建失败")?;
    let warm_at = place(
        guess,
        crate::pin_native::work_area((guess.x, guess.y)),
        monitor_scale(guess),
    );
    Application::new()
        .with_assets(toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            let (display_id, bounds) = crate::pin_native::initial_pin_bounds(warm_at, cx);
            let opened = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    display_id,
                    kind: WindowKind::PopUp,
                    window_background: WindowBackgroundAppearance::Transparent,
                    focus: false,
                    ..Default::default()
                },
                move |window, cx| {
                    let h = crate::gpui_capture::hwnd(window);
                    cloak(h, true);
                    native_setup(h, warm_at, false);
                    announce_when_presented(owner, listener, window, 600);
                    cx.new(|cx| TrayPopup::new(owner, false, window, cx))
                },
            );
            let Ok(handle) = opened else {
                quit(cx, "预热窗口创建失败");
                return;
            };
            cx.spawn(async move |cx| {
                let Some(anchor) = receiver.await.ok().flatten() else {
                    let _ = cx.update(|cx| quit(cx, "属主退出或等待失败"));
                    return;
                };
                report(owner, Stage::Received);
                let target = place(
                    anchor,
                    crate::pin_native::work_area((anchor.x, anchor.y)),
                    monitor_scale(anchor),
                );
                let _ = handle.update(cx, |view, window, cx| {
                    let h = crate::gpui_capture::hwnd(window);
                    native_setup(h, target, !exit_after_present);
                    cloak(h, false);
                    view.shown = true;
                    if !exit_after_present {
                        window.focus(&view.focus);
                    }
                    let after = present_serial(window);
                    watch_present(owner, exit_after_present, after, window, 600);
                    cx.notify();
                });
            })
            .detach();
        });
    let _ = waiter.join();
    Ok(())
}

/// 预热窗口首个成功 Present 后才告诉属主就绪。
fn announce_when_presented(owner: HWND, listener: usize, window: &mut Window, remaining: u16) {
    window.on_next_frame(move |window, _| {
        if present_serial(window) > 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(owner),
                    READY,
                    WPARAM(listener),
                    LPARAM(GetCurrentProcessId() as isize),
                );
            }
            return;
        }
        if remaining > 0 {
            announce_when_presented(owner, listener, window, remaining - 1);
        }
    });
    window.refresh();
}

fn quit(cx: &mut App, reason: &str) {
    eprintln!("托盘弹层退出：{reason}");
    cx.quit();
}

fn native_setup(h: HWND, p: Placement, activate: bool) {
    unsafe {
        SetWindowLongPtrW(h, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        SetWindowLongPtrW(
            h,
            GWL_EXSTYLE,
            GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_TOOLWINDOW.0 as isize,
        );
        let mut flags = SWP_SHOWWINDOW | SWP_FRAMECHANGED;
        if !activate {
            flags |= SWP_NOACTIVATE;
        }
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            p.x,
            p.y,
            p.width as i32,
            p.height as i32,
            flags,
        );
    }
}

fn present_serial(window: &Window) -> usize {
    unsafe {
        GetPropW(
            crate::gpui_capture::hwnd(window),
            w!("RustShot.GpuiPresentSerial"),
        )
    }
    .0 as usize
}

/// on_next_frame 先于每帧绘制；看到串号超过 `after` 的成功 Present 后 DwmFlush，
/// 再回报呈现上界。
fn watch_present(
    owner: HWND,
    exit_after_present: bool,
    after: usize,
    window: &mut Window,
    remaining: u16,
) {
    window.on_next_frame(move |window, cx| {
        if present_serial(window) > after {
            unsafe { DwmFlush() };
            report(owner, Stage::Presented);
            if exit_after_present {
                cx.quit();
            }
            return;
        }
        if remaining > 0 {
            watch_present(owner, exit_after_present, after, window, remaining - 1);
        }
    });
    window.refresh();
}

struct TrayPopup {
    owner: HWND,
    focus: FocusHandle,
    rendered: bool,
    /// 预热窗口隐身期间不算显示，激活变化不触发关闭。
    shown: bool,
    was_active: bool,
    hotkey: String,
    pin_hotkey: String,
    quick: Vec<(Icon, String, String)>,
    _activation: Subscription,
}

impl TrayPopup {
    fn new(owner: HWND, shown: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        if shown {
            window.focus(&focus);
        }
        // 显示后激活过、再失去激活即关闭，与系统菜单一样点外面就收起。
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if !view.shown {
                return;
            }
            if window.is_window_active() {
                view.was_active = true;
            } else if view.was_active {
                quit(cx, "失去激活");
            }
        });
        let preferences = crate::preferences::Preferences::load();
        let quick = crate::quick_action_store::QuickActionStore::load()
            .actions
            .into_iter()
            .filter(|action| action.enabled && action.in_tray)
            .map(|action| {
                let icon = match action.recipe.steps.iter().flatten().last() {
                    Some(Step::OcrCopy) => Icon::Ocr,
                    Some(Step::TableCopy) => Icon::Table,
                    Some(Step::QrCopy | Step::QrOpen) => Icon::Qr,
                    Some(Step::TranslatePin) => Icon::Translate,
                    Some(Step::QuickSave) => Icon::Save,
                    Some(Step::Pin) => Icon::Pin,
                    Some(Step::CopyImage) => Icon::Copy,
                    None => Icon::RegionRect,
                };
                let hotkey = action.hotkey.map_or_else(
                    || crate::tr!("无快捷键", "No hotkey").to_string(),
                    |hotkey| hotkey.label(),
                );
                (icon, action.label().to_string(), hotkey)
            })
            .collect();
        Self {
            owner,
            focus,
            rendered: false,
            shown,
            was_active: false,
            hotkey: preferences.hotkey_label(),
            pin_hotkey: preferences.hotkey_label_for(crate::hotkey::HotkeyTarget::ClipboardPin),
            quick,
            _activation: activation,
        }
    }
}

fn glyph(icon: Icon, size: f32, color: u32) -> gpui::Svg {
    toolbar_icons::action(icon, color).size(px(size))
}

/// P0 的每一项点击都只关闭弹层；命令回传在 P1 接入。
fn item(id: impl Into<SharedString>, p: &Palette) -> Stateful<Div> {
    let hover = p.hover;
    div()
        .id(id.into())
        .rounded(px(6.))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(hover)))
        .on_click(|_, _, cx| cx.quit())
}

fn section(p: &Palette, title: Option<&str>, meta: Option<String>) -> Div {
    div()
        .flex()
        .flex_col()
        .px(px(8.))
        .py(px(6.))
        .border_t_1()
        .border_color(rgb(p.border))
        .when_some(title, |section, title| {
            section.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(22.))
                    .px(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(title.to_string())
                    .child(div().flex_1())
                    .when_some(meta, |row, meta| row.child(meta)),
            )
        })
}

fn key_chip(text: &str, p: &Palette) -> Div {
    div()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(p.border))
        .bg(rgb(p.track))
        .text_size(px(11.))
        .text_color(rgb(p.muted))
        .child(text.to_string())
}

fn two_lines(title: String, detail: String, p: &Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .child(div().truncate().child(title))
        .child(
            div()
                .truncate()
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(detail),
        )
}

impl Render for TrayPopup {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.rendered {
            self.rendered = true;
            report(self.owner, Stage::FirstRender);
        }
        let p = crate::theme::palette(cx);
        let accent_soft = (p.accent << 8) | 0x24;
        let header = div()
            .flex()
            .items_center()
            .gap(px(9.))
            .h(px(44.))
            .px(px(14.))
            .child(
                div()
                    .size(px(22.))
                    .rounded(px(6.))
                    .bg(rgb(p.accent))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(glyph(Icon::RegionRect, 15., p.on_accent)),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(crate::brand::NAME),
            )
            .child(div().flex_1())
            .child(div().size(px(6.)).rounded_full().bg(rgb(p.accent)))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(crate::tr!(
                        "快捷键正常 · 待命就绪",
                        "Hotkeys ready · standby warm"
                    )),
            );
        let hero = item("region", &p)
            .flex()
            .items_center()
            .gap(px(11.))
            .h(px(40.))
            .px(px(10.))
            .rounded(px(8.))
            .bg(rgba(accent_soft))
            .child(glyph(Icon::RegionRect, 22., p.accent))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(crate::tr!("区域截图", "Capture region")),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(crate::tr!("或单击托盘图标", "or click the tray icon")),
            )
            .child(div().flex_1())
            .child(key_chip(&self.hotkey, &p));
        let tiles = [
            (
                Icon::Monitor,
                crate::tr!("当前屏幕", "This screen"),
                "2560 × 1440".to_string(),
            ),
            (
                Icon::Monitors,
                crate::tr!("所有屏幕", "All screens"),
                crate::tr!("2 台显示器", "2 displays").to_string(),
            ),
            (
                Icon::LongCapture,
                crate::tr!("长截图", "Long"),
                crate::tr!("滚动拼接", "Scroll").to_string(),
            ),
            (
                Icon::Record,
                crate::tr!("录屏", "Record"),
                "MP4".to_string(),
            ),
        ];
        let tiles =
            div()
                .flex()
                .gap(px(4.))
                .mt(px(4.))
                .children(
                    tiles
                        .into_iter()
                        .enumerate()
                        .map(|(index, (icon, name, info))| {
                            item(SharedString::from(format!("tile-{index}")), &p)
                                .flex_1()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(2.))
                                .py(px(6.))
                                .child(glyph(icon, 20., p.foreground))
                                .child(div().text_size(px(12.)).child(name))
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(p.muted))
                                        .child(info),
                                )
                        }),
                );
        let capture = div()
            .flex()
            .flex_col()
            .px(px(8.))
            .pb(px(8.))
            .child(hero)
            .child(tiles);
        let quick = (!self.quick.is_empty()).then(|| {
            let more =
                (self.quick.len() > 4).then(|| crate::trf!("全部 {}", "All {}", self.quick.len()));
            section(&p, Some(crate::tr!("快捷动作", "Quick actions")), more).child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(self.quick.iter().take(4).enumerate().map(
                        |(index, (icon, name, hotkey))| {
                            item(SharedString::from(format!("quick-{index}")), &p)
                                .w(px(161.))
                                .h(px(38.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(glyph(*icon, 16., p.muted))
                                .child(
                                    two_lines(name.clone(), hotkey.clone(), &p).text_size(px(12.)),
                                )
                        },
                    )),
            )
        });
        let shades = [0x2f3b47, 0x3c4a3f, 0x473a3a, 0x2f3b47, 0x3c4a3f];
        let recent = section(
            &p,
            Some(crate::tr!("最近", "Recent")),
            Some(crate::tr!("今天 12 张 · 图库", "12 today · Library").to_string()),
        )
        .child(div().flex().gap(px(6.)).px(px(6.)).children(
            shades.into_iter().enumerate().map(|(index, shade)| {
                item(SharedString::from(format!("recent-{index}")), &p)
                    .flex_1()
                    .h(px(44.))
                    .rounded(px(5.))
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(shade))
            }),
        ));
        let row = |id: &'static str, icon: Icon, title: String, detail: Option<String>| {
            item(id, &p)
                .flex()
                .items_center()
                .gap(px(10.))
                .min_h(px(34.))
                .px(px(8.))
                .py(px(4.))
                .child(glyph(icon, 18., p.muted))
                .child(match detail {
                    Some(detail) => two_lines(title, detail, &p),
                    None => div().flex_1().child(title),
                })
        };
        let controls = [
            (Icon::EyeOff, crate::tr!("隐藏本组", "Hide group")),
            (Icon::ClickThrough, crate::tr!("取消穿透", "Interact")),
            (Icon::Pin, crate::tr!("管理", "Manage")),
            (Icon::Close, crate::tr!("全部关闭", "Close all")),
        ];
        let pins = section(
            &p,
            Some(crate::tr!("贴图 · 默认", "Pins · Default")),
            Some(crate::tr!("4 显示 · 2 隐藏", "4 shown · 2 hidden").to_string()),
        )
        .child(
            row(
                "clipboard",
                Icon::File,
                crate::tr!("剪贴板贴图", "Pin clipboard").into(),
                Some(crate::tr!("剪贴板：图片 1280 × 720", "Clipboard: image 1280 × 720").into()),
            )
            .child(key_chip(&self.pin_hotkey, &p)),
        )
        .child(row(
            "pin-file",
            Icon::Image,
            crate::tr!("打开图片贴图…", "Pin image file…").into(),
            None,
        ))
        .child(row(
            "restore",
            Icon::Undo,
            crate::tr!("恢复最近关闭", "Restore last closed").into(),
            Some(
                crate::tr!(
                    "3 分钟前 · 820 × 460 · 含标注",
                    "3 min ago · 820 × 460 · with markup"
                )
                .into(),
            ),
        ))
        .child(
            div()
                .flex()
                .gap(px(3.))
                .mt(px(4.))
                .p(px(3.))
                .rounded(px(8.))
                .bg(rgb(p.track))
                .children(
                    controls
                        .into_iter()
                        .enumerate()
                        .map(|(index, (icon, label))| {
                            item(SharedString::from(format!("control-{index}")), &p)
                                .flex_1()
                                .h(px(46.))
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap(px(2.))
                                .text_size(px(11.))
                                .text_color(rgb(p.muted))
                                .child(glyph(icon, 17., p.foreground))
                                .child(label)
                        }),
                ),
        );
        let footer_button = |id: &'static str, icon: Icon, label: &'static str| {
            item(id, &p)
                .flex()
                .items_center()
                .gap(px(7.))
                .h(px(30.))
                .px(px(9.))
                .text_size(px(12.))
                .child(glyph(icon, 16., p.muted))
                .child(label)
        };
        let footer = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .px(px(8.))
            .py(px(6.))
            .border_t_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.track))
            .child(footer_button(
                "library",
                Icon::Images,
                crate::tr!("图库", "Library"),
            ))
            .child(footer_button(
                "settings",
                Icon::Settings,
                crate::tr!("设置", "Settings"),
            ))
            .child(footer_button(
                "help",
                Icon::Keyboard,
                crate::tr!("帮助", "Help"),
            ))
            .child(div().flex_1())
            .child(footer_button(
                "exit",
                Icon::Close,
                crate::tr!("退出", "Exit"),
            ));
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    quit(cx, "Esc");
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.background))
            .text_size(px(13.))
            .text_color(rgb(p.foreground))
            .child(header)
            .child(capture)
            .children(quick)
            .child(recent)
            .child(pins)
            .child(div().flex_1())
            .child(footer)
    }
}

/// `--tray-popup-bench [次数]`：本进程扮演托盘，逐次起弹层待命、等就绪后静置，再发请求，
/// 打印请求到各阶段的毫秒数。弹层呈现后立即退出，不抢焦点。
pub(crate) fn run_bench(runs: usize, settle: Duration, prewarm: bool) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut frequency = 0_i64;
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
        return Err("无法读取 QPC 频率".into());
    }
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )?
    };
    // 锚点取主屏工作区右下角，与通知区图标的位置相近。
    let work = crate::pin_native::work_area((0, 0));
    let anchor = POINT {
        x: work.x + work.width as i32 - 120,
        y: work.y + work.height as i32 + 20,
    };
    let millis = |from: i64, to: i64| (to - from) as f64 * 1000. / frequency as f64;
    let mut extra = vec!["--exit-after-present".to_string()];
    if prewarm {
        extra.extend([
            "--prewarm".to_string(),
            format!("{},{}", anchor.x, anchor.y),
        ]);
    }
    let mut presented = Vec::new();
    for run in 1..=runs {
        let mut standby = Standby::spawn(owner, &extra)?;
        let spawned = Instant::now();
        pump(owner, Duration::from_secs(10), |message| {
            message.message == READY && standby.accept_ready(message.wParam, message.lParam)
        })
        .then_some(())
        .ok_or("弹层待命 10 秒内未就绪")?;
        let ready_ms = spawned.elapsed().as_secs_f64() * 1000.;
        pump(owner, settle, |_| false);
        let (working_set, private) = process_memory(&standby.child);
        let start = crate::capture_standby::qpc();
        standby.request(anchor, start)?;
        let mut stages = [None::<f64>; 7];
        let done = pump(owner, Duration::from_secs(10), |message| {
            if message.message == STAGE && message.wParam.0 < stages.len() {
                stages[message.wParam.0] = Some(millis(start, message.lParam.0 as i64));
            }
            stages[Stage::Presented as usize].is_some()
        });
        let exited = wait_exit(&mut standby.child, Duration::from_secs(5));
        let field = |stage: Stage| {
            stages[stage as usize].map_or("null".to_string(), |ms| format!("{ms:.1}"))
        };
        println!(
            "{{\"run\":{run},\"prewarm\":{prewarm},\"standby_ready_ms\":{ready_ms:.1},\"standby_working_set_mib\":{working_set:.1},\"standby_private_mib\":{private:.1},\"received_ms\":{},\"app_created_ms\":{},\"app_ready_ms\":{},\"window_opened_ms\":{},\"first_render_ms\":{},\"presented_ms\":{},\"exited\":{exited}}}",
            field(Stage::Received),
            field(Stage::AppCreated),
            field(Stage::AppReady),
            field(Stage::WindowOpened),
            field(Stage::FirstRender),
            field(Stage::Presented),
        );
        if !done {
            unsafe {
                let _ = DestroyWindow(owner);
            }
            return Err(format!("第 {run} 次 10 秒内未呈现首帧").into());
        }
        presented.extend(stages[Stage::Presented as usize]);
    }
    unsafe {
        let _ = DestroyWindow(owner);
    }
    presented.sort_by(f64::total_cmp);
    if let (Some(first), Some(last)) = (presented.first(), presented.last()) {
        let middle = presented.len() / 2;
        let median = if presented.len() % 2 == 0 {
            (presented[middle - 1] + presented[middle]) / 2.
        } else {
            presented[middle]
        };
        println!(
            "{{\"summary\":\"presented_ms\",\"runs\":{},\"min\":{first:.1},\"median\":{median:.1},\"max\":{last:.1}}}",
            presented.len()
        );
    }
    Ok(())
}

/// `--tray-popup-d3d-probe [--debug-flag] [--preload a.dll,b.dll]`：在全新进程里单独测
/// DXGI 工厂与 D3D11 设备创建，列出设备创建新加载的模块，用于拆分 GPUI 冷启动。
pub(crate) fn run_d3d_probe(args: &[String]) -> Result<()> {
    use windows::{
        core::{HSTRING, PCWSTR},
        Win32::{
            Foundation::HMODULE,
            Graphics::{
                Direct3D::{
                    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_11_1,
                },
                Direct3D11::{
                    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    D3D11_CREATE_DEVICE_DEBUG, D3D11_SDK_VERSION,
                },
                Dxgi::{
                    CreateDXGIFactory2, DXGIGetDebugInterface1, IDXGIFactory6, IDXGIInfoQueue,
                    DXGI_CREATE_FACTORY_FLAGS,
                },
            },
            System::LibraryLoader::LoadLibraryW,
        },
    };
    let started = Instant::now();
    let since = |at: Instant| at.elapsed().as_secs_f64() * 1000.;
    let before = loaded_modules();
    let mut preload_ms = 0.;
    if let Some(list) = args
        .iter()
        .position(|arg| arg == "--preload")
        .and_then(|index| args.get(index + 1))
    {
        let at = Instant::now();
        for path in list.split(',').filter(|path| !path.is_empty()) {
            let path = HSTRING::from(path);
            if let Err(error) = unsafe { LoadLibraryW(PCWSTR(path.as_ptr())) } {
                eprintln!("预载失败 {path}：{error}");
            }
        }
        preload_ms = since(at);
    }
    let at = Instant::now();
    let debug_interface = unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }.is_ok();
    let debug_check_ms = since(at);
    let at = Instant::now();
    let factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))? };
    let factory_ms = since(at);
    let describe = |adapter: &windows::Win32::Graphics::Dxgi::IDXGIAdapter| {
        unsafe { adapter.GetDesc() }
            .map(|desc| {
                String::from_utf16_lossy(&desc.Description)
                    .trim_matches(char::from(0))
                    .to_string()
            })
            .unwrap_or_default()
    };
    let adapters: Vec<String> = (0..)
        .map_while(|index| unsafe { factory.EnumAdapters(index) }.ok())
        .map(|adapter| describe(&adapter))
        .collect();
    let index = args
        .iter()
        .position(|arg| arg == "--adapter")
        .and_then(|position| args.get(position + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let adapter = unsafe { factory.EnumAdapters(index)? };
    let name = describe(&adapter);
    let flags = if args.iter().any(|arg| arg == "--debug-flag") {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    // 设备要活到列模块之后：释放最后一个设备时驱动模块会被卸载。
    let create = || -> Result<(f64, ID3D11Device)> {
        let at = Instant::now();
        let mut device: Option<ID3D11Device> = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&[
                    D3D_FEATURE_LEVEL_11_1,
                    D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_10_1,
                ]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
        }
        Ok((since(at), device.ok_or("D3D11 未返回设备")?))
    };
    let (first_device_ms, _first) = create()?;
    let (second_device_ms, _second) = create()?;
    let after = loaded_modules();
    let added: Vec<String> = after
        .iter()
        .filter(|path| !before.contains(path))
        .map(|path| path.replace('\\', "/"))
        .collect();
    let (working_set, private) = crate::capture::memory_mib();
    println!(
        "{{\"adapters\":{:?},\"adapter\":{:?},\"debug_interface\":{debug_interface},\"debug_flag\":{},\"preload_ms\":{preload_ms:.1},\"debug_check_ms\":{debug_check_ms:.1},\"factory_ms\":{factory_ms:.1},\"first_device_ms\":{first_device_ms:.1},\"second_device_ms\":{second_device_ms:.1},\"total_ms\":{:.1},\"working_set_mib\":{working_set:.1},\"private_mib\":{private:.1},\"modules_added\":{:?}}}",
        adapters,
        name,
        flags.contains(D3D11_CREATE_DEVICE_DEBUG),
        since(started),
        added,
    );
    Ok(())
}

fn loaded_modules() -> Vec<String> {
    use windows::Win32::{
        Foundation::HMODULE,
        System::ProcessStatus::{K32EnumProcessModules, K32GetModuleFileNameExW},
    };
    let process = unsafe { GetCurrentProcess() };
    let mut modules = vec![HMODULE::default(); 1024];
    let mut needed = 0_u32;
    let ok = unsafe {
        K32EnumProcessModules(
            process,
            modules.as_mut_ptr(),
            (modules.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
        )
    };
    if !ok.as_bool() {
        return Vec::new();
    }
    modules.truncate(needed as usize / std::mem::size_of::<HMODULE>());
    modules
        .into_iter()
        .filter_map(|module| {
            let mut name = [0_u16; 520];
            let length = unsafe { K32GetModuleFileNameExW(Some(process), Some(module), &mut name) };
            (length > 0).then(|| String::from_utf16_lossy(&name[..length as usize]))
        })
        .collect()
}

/// 泵消息直到 `accept` 认可某条发给 `owner` 的消息或超时；返回是否被认可。
fn pump(owner: HWND, timeout: Duration, mut accept: impl FnMut(&MSG) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.hwnd == owner && accept(&message) {
                return true;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                left.as_millis().min(u32::MAX as u128) as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
        }
    }
}

fn process_memory(child: &Child) -> (f64, f64) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            HANDLE(child.as_raw_handle()),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        )
    };
    if !ok.as_bool() {
        return (f64::NAN, f64::NAN);
    }
    (
        counters.WorkingSetSize as f64 / 1048576.,
        counters.PrivateUsage as f64 / 1048576.,
    )
}

fn wait_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: Placement = Placement {
        x: 0,
        y: 0,
        width: 2560,
        height: 1392,
    };

    #[test]
    fn tray_popup_sits_above_bottom_taskbar_right_aligned() {
        let p = place(POINT { x: 2440, y: 1412 }, WORK, 1.);
        assert_eq!((p.width, p.height), (344, 646));
        assert_eq!(p.x, 2440 - 344);
        assert_eq!(p.y + p.height as i32, 1392);
    }

    #[test]
    fn tray_popup_drops_below_top_taskbar_and_clamps_to_left_edge() {
        let work = Placement {
            x: 0,
            y: 48,
            width: 1920,
            height: 1032,
        };
        let p = place(POINT { x: 100, y: 20 }, work, 1.5);
        assert_eq!((p.width, p.height), (516, 969));
        assert_eq!((p.x, p.y), (0, 48));
    }

    #[test]
    fn tray_popup_anchor_round_trips_negative_coordinates() {
        let anchor = POINT { x: -1800, y: -40 };
        let back = unpack(pack(anchor));
        assert_eq!((back.x, back.y), (anchor.x, anchor.y));
    }
}
