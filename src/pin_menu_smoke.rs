//! 贴图菜单有限原生专项：合成 Pin、定向消息及原 HWND 的焦点/捕获/首帧检查。
//! 不移动系统鼠标、不注入全局按键、不设置 OS 焦点，不访问剪贴板或保存桌面图像。
use crate::{
    capture::Image,
    gpui_capture::hwnd,
    gpui_smoke::{self, SMOKE_MOVE},
    pin_menu::PinMenu,
    pin_model::PinState,
    pin_native as native,
    pin_view::PinView,
    Result,
};
use gpui::{
    div, prelude::*, App, AppContext, AsyncApp, Render, Window, WindowHandle, WindowOptions,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::{Component, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const PROPERTY: windows::core::PCWSTR = w!("RustShot.PinMenuSmoke.Owner");
const COLOR: [u8; 4] = [0x48, 0x80, 0xc0, 255];
const WAIT: Duration = Duration::from_secs(8);
const TICK: Duration = Duration::from_millis(8);

fn ensure(ok: bool, message: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

/// 在任何 Pin 创建/工作区写入前隔离目录；父 runner 可提供一次新的固定 QA 目录。
pub(crate) fn isolate_data_dir() -> Result<PathBuf> {
    let qa = std::env::current_dir()?.join("target/qa");
    std::fs::create_dir_all(&qa)?;
    let qa = qa.canonicalize()?;
    let folder = match std::env::var_os("RUSTSHOT_PIN_MENU_QA_DIR") {
        Some(folder) => PathBuf::from(folder),
        None => qa.join("pin-menu").join(format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        )),
    };
    ensure(
        folder.is_absolute()
            && folder.starts_with(&qa)
            && folder != qa
            && !folder
                .components()
                .any(|p| matches!(p, Component::ParentDir)),
        "贴图菜单专项路径必须是 target/qa 下的绝对独立目录",
    )?;
    std::fs::create_dir_all(&folder)?;
    let folder = folder.canonicalize()?;
    ensure(folder.starts_with(&qa) && folder != qa, "QA 目录解析后越界")?;
    let settings = folder.join("settings");
    std::fs::create_dir_all(&settings)?;
    ensure(
        settings.canonicalize()?.starts_with(&folder),
        "QA 配置目录解析后越界",
    )?;
    std::env::set_var("RUSTSHOT_PIN_MENU_QA_DIR", &folder);
    Ok(settings)
}

fn last_input() -> Result<u32> {
    let mut value = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetLastInputInfo(&mut value).ok()? };
    Ok(value.dwTime)
}

#[derive(Clone)]
struct Owned {
    handle: usize,
    thread: u32,
    token: Rc<()>,
}
impl Owned {
    fn bind(handle: HWND) -> Result<Self> {
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(handle, Some(&mut process)) };
        ensure(
            unsafe { IsWindow(Some(handle)) }.as_bool()
                && process == std::process::id()
                && thread != 0,
            "QA 只能绑定本进程自有 HWND",
        )?;
        let token = Rc::new(());
        unsafe { SetPropW(handle, PROPERTY, Some(HANDLE(Rc::as_ptr(&token) as *mut _)))? };
        gpui_smoke::isolate_input(handle)?;
        let value = Self {
            handle: handle.0 as usize,
            thread,
            token,
        };
        value.alive()?;
        Ok(value)
    }
    fn hwnd(&self) -> HWND {
        HWND(self.handle as *mut _)
    }
    fn alive(&self) -> Result<()> {
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(self.hwnd(), Some(&mut process)) };
        ensure(
            unsafe { IsWindow(Some(self.hwnd())) }.as_bool()
                && process == std::process::id()
                && thread == self.thread
                && unsafe { GetPropW(self.hwnd(), PROPERTY) }.0
                    == Rc::as_ptr(&self.token) as *mut _,
            "原 QA HWND 的进程/线程/nonce 身份改变",
        )
    }
    fn visible(&self) -> Result<bool> {
        self.alive()?;
        Ok(unsafe { IsWindowVisible(self.hwnd()) }.as_bool())
    }
    fn post(&self, message: u32, state: usize, p: (i32, i32)) -> Result<()> {
        self.alive()?;
        ensure(
            (i16::MIN as i32..=i16::MAX as i32).contains(&p.0)
                && (i16::MIN as i32..=i16::MAX as i32).contains(&p.1),
            "QA 定向消息坐标超出有符号 16 位范围",
        )?;
        unsafe {
            PostMessageW(
                Some(self.hwnd()),
                message,
                WPARAM(state),
                LPARAM((((p.1 as u16 as u32) << 16) | p.0 as u16 as u32) as isize),
            )?;
        }
        Ok(())
    }
    fn key(&self, key: usize) -> Result<()> {
        self.alive()?;
        unsafe {
            PostMessageW(Some(self.hwnd()), WM_KEYDOWN, WPARAM(key), LPARAM(1))?;
            PostMessageW(
                Some(self.hwnd()),
                WM_KEYUP,
                WPARAM(key),
                LPARAM((3_u32 << 30) as isize),
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct NativeInput {
    foreground: usize,
    active: usize,
    focus: usize,
    capture: usize,
}
fn native_input(owner: &Owned) -> Result<NativeInput> {
    owner.alive()?;
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(owner.thread, &mut info)? };
    Ok(NativeInput {
        foreground: unsafe { GetForegroundWindow() }.0 as usize,
        active: info.hwndActive.0 as usize,
        focus: info.hwndFocus.0 as usize,
        capture: info.hwndCapture.0 as usize,
    })
}
struct Guard {
    owner: Owned,
    input_tick: u32,
    baseline: NativeInput,
}
impl Guard {
    fn input_unchanged(&self) -> Result<()> {
        self.owner.alive()?;
        ensure(
            last_input()? == self.input_tick,
            "真实鼠标/键盘输入改变，本轮合成菜单专项中止",
        )
    }
    fn focus_unchanged(&self) -> Result<NativeInput> {
        self.input_unchanged()?;
        let value = native_input(&self.owner)?;
        ensure(
            (value.foreground, value.active, value.focus)
                == (
                    self.baseline.foreground,
                    self.baseline.active,
                    self.baseline.focus,
                ),
            format!(
                "菜单改变了 OS foreground/active/focus：{:?} -> {value:?}",
                self.baseline
            ),
        )?;
        Ok(value)
    }
    fn released(&self) -> Result<NativeInput> {
        let value = self.focus_unchanged()?;
        ensure(
            value.capture == 0,
            format!("抬起后 mouse capture 未释放：{value:?}"),
        )?;
        Ok(value)
    }
}

struct Report {
    path: PathBuf,
}
impl Report {
    fn write(&self, phase: &str, detail: Value) -> Result<()> {
        let value = json!({"suite":"pin-menu", "phase":phase, "detail":detail});
        let mut stream = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut stream, &value)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        println!("{value}");
        Ok(())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor().timer(TICK).await;
}
async fn settle(cx: &mut AsyncApp, guard: &Guard) -> Result<()> {
    for _ in 0..5 {
        tick(cx).await;
        guard.focus_unchanged()?;
    }
    Ok(())
}
fn present(handle: HWND) -> usize {
    unsafe { GetPropW(handle, w!("RustShot.GpuiPresentSerial")) }.0 as usize
}

#[derive(Clone, Debug, Serialize)]
struct WindowReady {
    hwnd: usize,
    serial: usize,
    rect: [i64; 4],
    client: [i32; 4],
    viewport_physical: [u32; 2],
    style: u32,
    ex_style: u32,
    cloaked: u32,
}
fn ready<V: 'static + Render>(
    handle: WindowHandle<V>,
    owned: &Owned,
    newer_than: usize,
    cx: &mut AsyncApp,
) -> Result<Option<WindowReady>> {
    owned.alive()?;
    if !owned.visible()? || present(owned.hwnd()) <= newer_than {
        return Ok(None);
    }
    let (same, physical) = handle.update(cx, |_, window, _| {
        (
            hwnd(window) == owned.hwnd(),
            [
                (f32::from(window.viewport_size().width) * window.scale_factor()).round() as u32,
                (f32::from(window.viewport_size().height) * window.scale_factor()).round() as u32,
            ],
        )
    })?;
    ensure(same, "实际 GPUI WindowHandle 不再指向原 HWND")?;
    let mut client = RECT::default();
    unsafe { GetClientRect(owned.hwnd(), &mut client)? };
    let rect = native::rect(owned.hwnd())?;
    let size = unsafe { GetPropW(owned.hwnd(), w!("RustShot.GpuiPresentSize")) }.0 as usize as u32;
    if physical != [rect.width, rect.height]
        || client
            != (RECT {
                left: 0,
                top: 0,
                right: rect.width as i32,
                bottom: rect.height as i32,
            })
        || [size >> 16, size & 0xffff] != physical
    {
        return Ok(None);
    }
    let mut cloaked = u32::MAX;
    unsafe {
        DwmGetWindowAttribute(
            owned.hwnd(),
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )?;
    }
    ensure(cloaked == 0, "QA 窗口仍被 DWM cloak")?;
    ensure(
        !unsafe { IsIconic(owned.hwnd()) }.as_bool(),
        "QA 窗口被最小化",
    )?;
    Ok(Some(WindowReady {
        hwnd: owned.handle,
        serial: present(owned.hwnd()),
        rect: [
            i64::from(rect.x),
            i64::from(rect.y),
            i64::from(rect.width),
            i64::from(rect.height),
        ],
        client: [client.left, client.top, client.right, client.bottom],
        viewport_physical: physical,
        style: unsafe { GetWindowLongPtrW(owned.hwnd(), GWL_STYLE) } as u32,
        ex_style: unsafe { GetWindowLongPtrW(owned.hwnd(), GWL_EXSTYLE) } as u32,
        cloaked,
    }))
}
async fn wait_ready<V: 'static + Render>(
    handle: WindowHandle<V>,
    owned: &Owned,
    newer_than: usize,
    guard: Option<&Guard>,
    cx: &mut AsyncApp,
) -> Result<WindowReady> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(guard) = guard {
            guard.focus_unchanged()?;
        }
        if let Some(value) = ready(handle, owned, newer_than, cx)? {
            unsafe { DwmFlush()? };
            if let Some(guard) = guard {
                guard.focus_unchanged()?;
            }
            if let Some(after) = ready(handle, owned, newer_than, cx)? {
                ensure(after.rect == value.rect, "DWM 完成期间 QA 窗口几何改变")?;
                return Ok(after);
            }
        }
        ensure(
            Instant::now() < deadline,
            "QA 窗口未通过实际 Present/客户区/viewport/DWM 首显门",
        )?;
        tick(cx).await;
    }
}

fn owner_ready(
    pin: WindowHandle<PinView>,
    owner: &Owned,
    cx: &mut AsyncApp,
) -> Result<Option<(WindowReady, NativeInput, native::FirstShowProbe)>> {
    owner.alive()?;
    let first_show = native::first_show_probe(owner.hwnd());
    if first_show.starting
        || !first_show.requested_visible
        || !first_show.first_show_ready
        || first_show.aborted
        || first_show.first_show_serial == 0
        || first_show.first_show_rect[2..] != [640, 400]
        || present(owner.hwnd()) < first_show.first_show_serial
    {
        return Ok(None);
    }
    let Some(value) = ready(pin, owner, 0, cx)? else {
        return Ok(None);
    };
    if value.rect != first_show.first_show_rect.map(i64::from)
        || value.viewport_physical != [640, 400]
        || value.serial < first_show.first_show_serial
    {
        return Ok(None);
    }
    let input = native_input(owner)?;
    if input
        != (NativeInput {
            foreground: owner.handle,
            active: owner.handle,
            focus: owner.handle,
            capture: 0,
        })
    {
        return Ok(None);
    }
    Ok(Some((value, input, first_show)))
}

/// 菜单的焦点基线必须在原 Pin 完成真实首显之后建立，不能接收初始可见帧。
async fn wait_owner_ready(
    pin: WindowHandle<PinView>,
    owner: &Owned,
    input_tick: u32,
    cx: &mut AsyncApp,
) -> Result<(WindowReady, NativeInput, native::FirstShowProbe)> {
    let deadline = Instant::now() + WAIT;
    loop {
        owner.alive()?;
        ensure(
            last_input()? == input_tick,
            "菜单专项等待 owner 首显期间真实输入已改变，未重设父进程基线",
        )?;
        if let Some((before, _, _)) = owner_ready(pin, owner, cx)? {
            unsafe { DwmFlush()? };
            ensure(
                last_input()? == input_tick,
                "菜单专项 owner DWM 首显检查期间真实输入已改变，未重设父进程基线",
            )?;
            if let Some((after, input, first_show)) = owner_ready(pin, owner, cx)? {
                ensure(
                    after.rect == before.rect,
                    "owner DWM 首显完成期间原 Pin 几何改变",
                )?;
                return Ok((after, input, first_show));
            }
        }
        ensure(
            Instant::now() < deadline,
            "owner 原 Pin 首显/焦点未就绪，尚未进入菜单焦点验证",
        )?;
        tick(cx).await;
    }
}

fn hit(owned: &Owned, local: (i32, i32)) -> Result<POINT> {
    owned.alive()?;
    let mut client = RECT::default();
    unsafe { GetClientRect(owned.hwnd(), &mut client)? };
    ensure(
        local.0 > 0 && local.1 > 0 && local.0 < client.right && local.1 < client.bottom,
        "QA 采样/点击点不在原自有客户区",
    )?;
    let mut global = POINT {
        x: local.0,
        y: local.1,
    };
    unsafe { ClientToScreen(owned.hwnd(), &mut global).ok()? };
    let top = unsafe { WindowFromPoint(global) };
    ensure(
        top == owned.hwnd() || unsafe { IsChild(owned.hwnd(), top) }.as_bool(),
        "原自有窗口点击/合成像素点被遮挡",
    )?;
    Ok(global)
}
fn owned_screen_pixel(owned: &Owned, local: (i32, i32)) -> Result<[u8; 3]> {
    let rect = native::rect(owned.hwnd())?;
    let global = hit(owned, local)?;
    let dc = unsafe { GetDC(None) };
    ensure(!dc.is_invalid(), "QA 自有合成内容点 DC 失败")?;
    let color = unsafe { GetPixel(dc, global.x, global.y) };
    let released = unsafe { ReleaseDC(None, dc) };
    ensure(
        color.0 != u32::MAX && released == 1,
        "QA 自有内容点 GetPixel/ReleaseDC 失败",
    )?;
    owned.alive()?;
    ensure(
        native::rect(owned.hwnd())? == rect,
        "自有内容点读取时窗口移动",
    )?;
    hit(owned, local)?;
    Ok([(color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8])
}
async fn menu_pixel(
    menu: WindowHandle<PinMenu>,
    owned: &Owned,
    guard: &Guard,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (local, expected) = menu.update(cx, |view, window, cx| -> Result<_> {
        let point = view
            .positions
            .borrow()
            .get("pin-menu-background")
            .copied()
            .ok_or("菜单背景探针尚未实际布局")?;
        let background = crate::theme::palette(cx).background;
        Ok((
            (
                (point.0 * window.scale_factor()).round() as i32,
                (point.1 * window.scale_factor()).round() as i32,
            ),
            [
                background as u8,
                (background >> 8) as u8,
                (background >> 16) as u8,
            ],
        ))
    })??;
    let screen = owned_screen_pixel(owned, local)?;
    ensure(
        screen == expected,
        format!("菜单真实自有背景未呈现：{screen:?}"),
    )?;
    menu.update(cx, |_, window, _| {
        window.request_frame_capture([local.0 as u32, local.1 as u32, 1, 1])
    })??;
    let deadline = Instant::now() + WAIT;
    let bytes = loop {
        guard.released()?;
        owned.alive()?;
        if let Some(result) = menu.update(cx, |_, window, _| window.take_frame_capture())? {
            break result?;
        }
        ensure(Instant::now() < deadline, "菜单自有 GPUI 帧缓冲样本超时")?;
        tick(cx).await;
    };
    ensure(
        bytes.len() == 4 && bytes[..3] == expected && bytes[3] == 255,
        "菜单 GPU 帧缓冲背景样本错误",
    )?;
    Ok(
        json!({"local_physical":[local.0,local.1],"expected_theme_bgr":expected,"screen_bgr":screen,"framebuffer_bgra":bytes,
        "screen_sample_scope":"one pre/post owned HWND hit-tested synthetic menu point",
        "full_menu_pixels_verified":false}),
    )
}

fn menu_of(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Option<WindowHandle<PinMenu>>> {
    Ok(pin.update(cx, |view, _, _| view.context_menu)?)
}
async fn hidden(menu: &Owned, guard: &Guard, cx: &mut AsyncApp) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        guard.focus_unchanged()?;
        if !menu.visible()? {
            return Ok(());
        }
        ensure(Instant::now() < deadline, "菜单未实际隐藏")?;
        tick(cx).await;
    }
}
struct MenuSession {
    handle: WindowHandle<PinMenu>,
    owned: Owned,
}
async fn open_menu(
    pin: WindowHandle<PinView>,
    guard: &Guard,
    previous: Option<&MenuSession>,
    label: &str,
    report: &Report,
    cx: &mut AsyncApp,
) -> Result<MenuSession> {
    guard.released()?;
    if let Some(previous) = previous {
        ensure(!previous.owned.visible()?, "再次右键前旧菜单仍可见")?;
    }
    guard.owner.post(WM_RBUTTONDOWN, 2, (120, 100))?;
    settle(cx, guard).await?;
    if let Some(menu) = menu_of(pin, cx)? {
        let h = menu.update(cx, |_, window, _| hwnd(window))?;
        ensure(
            !unsafe { IsWindowVisible(h) }.as_bool(),
            "RightDown 就显示了菜单，抬起语义未修复",
        )?;
    }
    let right_down = guard.focus_unchanged()?;
    ensure(
        right_down.capture == guard.owner.handle,
        "RightDown 未保持原 Pin capture，释放到其他 HWND 时可能丢失 RightUp",
    )?;
    let start_serial = previous.map_or(0, |menu| present(menu.owned.hwnd()));
    let start = Instant::now();
    guard.owner.post(WM_RBUTTONUP, 0, (120, 100))?;
    let deadline = Instant::now() + WAIT;
    let mut first_present_ms = None;
    let (session, visible_ms) = loop {
        guard.focus_unchanged()?;
        if let Some(menu) = menu_of(pin, cx)? {
            let h = menu.update(cx, |_, window, _| hwnd(window))?;
            if first_present_ms.is_none() && present(h) > start_serial {
                first_present_ms = Some(start.elapsed().as_secs_f64() * 1000.);
            }
            if unsafe { IsWindowVisible(h) }.as_bool() {
                let visible_ms = start.elapsed().as_secs_f64() * 1000.;
                let owned = if let Some(previous) = previous {
                    ensure(
                        previous.handle == menu && previous.owned.hwnd() == h,
                        "重开菜单重新创建了 GPUI/HWND",
                    )?;
                    previous.owned.clone()
                } else {
                    Owned::bind(h)?
                };
                ensure(
                    ready(menu, &owned, start_serial, cx)?.is_some(),
                    "菜单首次被观察为可见时尚无新成功 Present/完整客户区，可能闪现旧帧",
                )?;
                break (
                    MenuSession {
                        handle: menu,
                        owned,
                    },
                    visible_ms,
                );
            }
        }
        ensure(Instant::now() < deadline, "RightUp 后菜单未实际显示")?;
        tick(cx).await;
    };
    let actual = wait_ready(
        session.handle,
        &session.owned,
        start_serial,
        Some(guard),
        cx,
    )
    .await?;
    let ready_ms = start.elapsed().as_secs_f64() * 1000.;
    let state = guard.released()?;
    ensure(
        actual.ex_style & WS_EX_NOACTIVATE.0 != 0,
        "菜单缺少 WS_EX_NOACTIVATE",
    )?;
    ensure(
        actual.style & WS_POPUP.0 != 0 && actual.style & WS_CAPTION.0 == 0,
        "菜单原生 Popup 样式错误",
    )?;
    ensure(
        unsafe { GetWindowLongPtrW(session.owned.hwnd(), GWLP_HWNDPARENT) } as usize
            == guard.owner.handle,
        "菜单的原生 owner 不是本次 Pin",
    )?;
    let activation = unsafe {
        SendMessageW(
            session.owned.hwnd(),
            WM_MOUSEACTIVATE,
            Some(WPARAM(guard.owner.handle)),
            Some(LPARAM(((WM_LBUTTONDOWN << 16) | HTCLIENT) as isize)),
        )
    }
    .0;
    ensure(
        activation == MA_NOACTIVATE as isize,
        format!("菜单 WM_MOUSEACTIVATE 没有拒绝激活：{activation}"),
    )?;
    guard.released()?;
    let sample = menu_pixel(session.handle, &session.owned, guard, cx).await?;
    report.write(label, json!({"ok":true,"right_down_menu_hidden":true,"right_down_input":right_down,
        "right_up_input":state,"native":actual,"same_hwnd_reused":previous.is_some(),
        "first_present_observed_ms":first_present_ms,"visible_observed_ms":visible_ms,
        "ready_after_dwm_observed_ms":ready_ms,
        "timing_scope":"pre-PostMessage RightUp to native polling observation; includes dispatch/wake scheduling",
        "poll_interval_ms":TICK.as_millis(),"exact_present_timestamp_available":false,
        "mouseactivate_result":activation,"pixels":sample}))?;
    Ok(session)
}
async fn menu_click(
    session: &MenuSession,
    id: &str,
    guard: &Guard,
    cx: &mut AsyncApp,
) -> Result<()> {
    guard.released()?;
    session.owned.alive()?;
    let physical = session.handle.update(cx, |view, window, _| -> Result<_> {
        let p = view
            .positions
            .borrow()
            .get(id)
            .copied()
            .ok_or_else(|| format!("菜单按钮未实际布局：{id}"))?;
        Ok((
            (p.0 * window.scale_factor()).round() as i32,
            (p.1 * window.scale_factor()).round() as i32,
        ))
    })??;
    hit(&session.owned, physical)?;
    session.owned.post(WM_LBUTTONDOWN, 1, physical)?;
    settle(cx, guard).await?;
    let pressed = guard.focus_unchanged()?;
    ensure(
        pressed.capture == session.owned.handle,
        "菜单按压没有使用自有 mouse capture",
    )?;
    session.owned.post(WM_LBUTTONUP, 0, physical)?;
    settle(cx, guard).await?;
    guard.released()?;
    Ok(())
}
async fn display_tab(session: &MenuSession, guard: &Guard, cx: &mut AsyncApp) -> Result<()> {
    let serial = present(session.owned.hwnd());
    menu_click(session, "tab-2", guard, cx).await?;
    let deadline = Instant::now() + WAIT;
    loop {
        guard.released()?;
        let displayed = session.handle.update(cx, |view, _, _| {
            view.tab == 2 && view.positions.borrow().contains_key("item-1")
        })?;
        if displayed && present(session.owned.hwnd()) > serial {
            break;
        }
        ensure(Instant::now() < deadline, "菜单显示页签没有通过新帧呈现")?;
        tick(cx).await;
    }
    wait_ready(session.handle, &session.owned, serial, Some(guard), cx).await?;
    Ok(())
}
async fn vertical_hover_tabs(
    session: &MenuSession,
    guard: &Guard,
    report: &Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut fixed_x = None;
    for tab in [2_u8, 3, 1, 2] {
        guard.released()?;
        let point = session.handle.update(cx, |view, window, _| -> Result<_> {
            let point = view
                .positions
                .borrow()
                .get(&format!("tab-{tab}"))
                .copied()
                .ok_or_else(|| format!("菜单悬停页签未实际布局：{tab}"))?;
            Ok((
                (point.0 * window.scale_factor()).round() as i32,
                (point.1 * window.scale_factor()).round() as i32,
            ))
        })??;
        let x = *fixed_x.get_or_insert(point.0);
        let physical = (x, point.1);
        hit(&session.owned, physical)?;
        let serial = present(session.owned.hwnd());
        let started = Instant::now();
        session.owned.post(WM_MOUSEMOVE, 0, physical)?;
        let mut state_observed_ms = None;
        let new_present_observed_ms = loop {
            guard.released()?;
            let displayed = session.handle.update(cx, |view, _, _| {
                view.tab == tab && view.positions.borrow().contains_key("item-1")
            })?;
            if displayed {
                let observed_ms = started.elapsed().as_secs_f64() * 1000.;
                state_observed_ms.get_or_insert(observed_ms);
                if present(session.owned.hwnd()) > serial {
                    break observed_ms;
                }
            }
            ensure(
                started.elapsed() < WAIT,
                format!("竖直悬停页签 {tab} 未通过状态/新 Present 门"),
            )?;
            tick(cx).await;
        };
        let actual = wait_ready(session.handle, &session.owned, serial, Some(guard), cx).await?;
        report.write(
            "vertical-hover-native-move",
            json!({"ok":new_present_observed_ms < 150.,"actual_tab":tab,
                "local_physical":[physical.0,physical.1],"same_x_vertical_path":true,
                "state_observed_ms":state_observed_ms,"new_present_observed_ms":new_present_observed_ms,
                "ready_after_dwm_observed_ms":started.elapsed().as_secs_f64()*1000.,
                "present_serial_before":serial,"native":actual,"input":guard.released()?,
                "timing_scope":"pre-PostMessage MouseMove to native polling observation; includes dispatch/wake scheduling",
                "poll_interval_ms":TICK.as_millis(),"exact_present_timestamp_available":false}),
        )?;
        ensure(
            new_present_observed_ms < 150.,
            format!(
                "竖直悬停页签 {tab} 新帧耗时 {new_present_observed_ms:.1} ms，未达到 150 ms 回归门"
            ),
        )?;
    }
    Ok(())
}
async fn owner_delivered_menu_wheel(
    pin: WindowHandle<PinView>,
    session: &MenuSession,
    guard: &Guard,
    report: &Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    display_tab(session, guard, cx).await?;
    let original_input = guard.released()?;
    ensure(
        original_input.focus == guard.owner.handle,
        "菜单滚轮专项需要原 owner 实际 OS 焦点，QA 不制造焦点",
    )?;
    let before_rect = native::rect(guard.owner.hwnd())?;
    let before = pin.update(cx, |view, window, cx| -> Result<_> {
        let canvas = view
            .canvas
            .as_ref()
            .ok_or("菜单滚轮专项没有内联画布")?
            .read(cx);
        ensure(
            canvas.pin_editing && canvas.focus.is_focused(window),
            "菜单滚轮专项要求 pin_editing 且 Canvas 已聚焦",
        )?;
        Ok((view.state.zoom, canvas.editor.tool, canvas.editor.style))
    })??;
    let (offset_before, max_y, local, item_y_before) =
        session.handle.update(cx, |view, window, _| -> Result<_> {
            let offset = view.scroll.offset();
            let point = view
                .positions
                .borrow()
                .get("item-0")
                .copied()
                .ok_or("菜单滚轮前首项没有实际布局位置")?;
            Ok((
                [f32::from(offset.x), f32::from(offset.y)],
                f32::from(view.scroll.max_offset().height),
                (
                    (point.0 * window.scale_factor()).round() as i32,
                    (point.1 * window.scale_factor()).round() as i32,
                ),
                point.1,
            ))
        })??;
    ensure(offset_before[1] == 0., "菜单显示子菜单滚动未复位")?;
    let global = hit(&session.owned, local)?;
    let mut owner_point = global;
    unsafe { ScreenToClient(guard.owner.hwnd(), &mut owner_point).ok()? };
    let owner_logical = pin.update(cx, |_, window, _| {
        [
            owner_point.x as f32 / window.scale_factor(),
            owner_point.y as f32 / window.scale_factor(),
        ]
    })?;
    let serial = present(session.owned.hwnd());
    // WM_MOUSEWHEEL 的 lParam 必须是屏幕物理坐标；GPUI 在 owner 队列转为逻辑坐标。
    // 消息目标固定为已有焦点的 owner，模拟关闭“悬停非活动窗口滚动”的派发路径。
    guard.owner.post(
        WM_MOUSEWHEEL,
        (-120_i16 as u16 as usize) << 16,
        (global.x, global.y),
    )?;
    let deadline = Instant::now() + WAIT;
    let offset_after = loop {
        guard.released()?;
        ensure(session.owned.visible()?, "转发滚轮时菜单被关闭")?;
        ensure(
            native::rect(guard.owner.hwnd())? == before_rect,
            "菜单滚轮误移动/缩放 owner 物理窗口",
        )?;
        let current = pin.update(cx, |view, window, cx| -> Result<_> {
            let canvas = view
                .canvas
                .as_ref()
                .ok_or("菜单滚轮导致内联画布丢失")?
                .read(cx);
            ensure(
                canvas.pin_editing && canvas.focus.is_focused(window),
                "菜单滚轮结束了画布编辑或移走内部焦点",
            )?;
            Ok((view.state.zoom, canvas.editor.tool, canvas.editor.style))
        })??;
        ensure(
            current == before,
            "菜单滚轮误改变 owner 缩放/当前画笔工具或完整 style 参数",
        )?;
        let offset = session.handle.update(cx, |view, _, _| {
            let offset = view.scroll.offset();
            [f32::from(offset.x), f32::from(offset.y)]
        })?;
        let item_y = session.handle.update(cx, |view, _, _| {
            view.positions.borrow().get("item-0").map(|point| point.1)
        })?;
        let presented = present(session.owned.hwnd()) > serial;
        if max_y == 0. {
            ensure(
                offset == offset_before && item_y == Some(item_y_before),
                "完整显示的子菜单在滚轮后改变了滚动或首项位置",
            )?;
            if presented {
                break offset;
            }
        } else if offset[1] < offset_before[1]
            && presented
            && item_y.is_some_and(|value| value < item_y_before)
        {
            break offset;
        }
        ensure(
            Instant::now() < deadline,
            "发给 owner 的菜单内滚轮未完成对应滚动状态与新 Present",
        )?;
        tick(cx).await;
    };
    let displayed = wait_ready(session.handle, &session.owned, serial, Some(guard), cx).await?;
    display_tab(session, guard, cx).await?;
    ensure(
        session
            .handle
            .update(cx, |view, _, _| f32::from(view.scroll.offset().y))?
            == 0.,
        "原生点页签未复位菜单滚动",
    )?;
    report.write("owner-delivered-menu-wheel-isolated", json!({"ok":true,"message_target_hwnd":guard.owner.handle,
        "menu_hwnd":session.owned.handle,"menu_local_physical":[local.0,local.1],"lparam_screen_physical":[global.x,global.y],
        "owner_event_logical":owner_logical,"wheel_delta":-120,"menu_scroll_before":offset_before,"menu_scroll_after":offset_after,
        "menu_scroll_range":max_y,"full_submenu_visible":max_y == 0.,
        "present_serial_before":serial,"present_serial_after":displayed.serial,"menu_tab_reset_by_native_click":true,
        "owner_bounds_unchanged":[before_rect.x,before_rect.y,before_rect.width,before_rect.height],"owner_zoom_unchanged":before.0,
        "canvas_pin_editing_preserved":true,"canvas_tool_and_full_style_preserved":true,"style_before":format!("{:?}",before.2),
        "input":guard.released()?,"system_inactive_window_scroll_preference_modified":false}))?;
    Ok(())
}
fn drag_point(owner: &Owned, menu: &Owned) -> Result<(i32, i32)> {
    menu.alive()?;
    for x in [80, 320, 560] {
        for y in [80, 200, 320] {
            if hit(owner, (x, y)).is_ok() {
                return Ok((x, y));
            }
        }
    }
    Err("菜单打开时没有可命中的自有 Pin 拖动区域".into())
}

struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}
async fn verify(
    pin: WindowHandle<PinView>,
    owner: Owned,
    input_tick: u32,
    report: &Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (owner_ready, baseline, first_show) = wait_owner_ready(pin, &owner, input_tick, cx).await?;
    ensure(
        last_input()? == input_tick,
        "菜单专项启动期间真实输入已改变，未重设父进程基线",
    )?;
    let guard = Guard {
        owner,
        input_tick,
        baseline,
    };
    ensure(
        owned_screen_pixel(&guard.owner, (40, 40))? == COLOR[..3],
        "合成 Pin 背景未实际呈现",
    )?;
    report.write("owner-ready", json!({"ok":true,"native":owner_ready,"first_show":first_show,"input":baseline,"original_parent_input_tick":input_tick,
        "parent_prelaunch_baseline_reinitialized":false,"synthetic_image_size":[640,400]}))?;

    let first = open_menu(pin, &guard, None, "first-right-up", report, cx).await?;
    display_tab(&first, &guard, cx).await?;
    report.write(
        "display-tab-native-click",
        json!({"ok":true,"input":guard.released()?,"actual_tab":2}),
    )?;
    vertical_hover_tabs(&first, &guard, report, cx).await?;
    menu_click(&first, "item-1", &guard, cx).await?;
    hidden(&first.owned, &guard, cx).await?;
    ensure(
        pin.update(cx, |view, _, _| view.state.locked)?,
        "菜单锁定项目没有作用于本次 owner",
    )?;
    report.write(
        "lock-item-native-click",
        json!({"ok":true,"owner_locked":true,"menu_native_hidden":true,"input":guard.released()?}),
    )?;

    let second = open_menu(pin, &guard, Some(&first), "reuse-right-up", report, cx).await?;
    display_tab(&second, &guard, cx).await?;
    menu_click(&second, "item-1", &guard, cx).await?;
    hidden(&second.owned, &guard, cx).await?;
    ensure(
        !pin.update(cx, |view, _, _| view.state.locked)?,
        "复用菜单未解锁原 owner",
    )?;
    report.write("unlock-reused-item", json!({"ok":true,"owner_locked":false,"same_menu_hwnd":second.owned.handle,"input":guard.released()?}))?;

    let drag_menu = open_menu(
        pin,
        &guard,
        Some(&second),
        "drag-dismiss-right-up",
        report,
        cx,
    )
    .await?;
    let before = native::rect(guard.owner.hwnd())?;
    let p = drag_point(&guard.owner, &drag_menu.owned)?;
    guard.owner.post(WM_LBUTTONDOWN, 1, p)?;
    settle(cx, &guard).await?;
    hidden(&drag_menu.owned, &guard, cx).await?;
    ensure(
        guard.focus_unchanged()?.capture == guard.owner.handle,
        "关闭菜单的同次左键没有开始 Pin capture",
    )?;
    guard
        .owner
        .post(WM_MOUSEMOVE, SMOKE_MOVE | 1, (p.0 + 25, p.1 + 15))?;
    let deadline = Instant::now() + WAIT;
    let moved = loop {
        guard.focus_unchanged()?;
        let moved = native::rect(guard.owner.hwnd())?;
        if moved.x == before.x + 25 && moved.y == before.y + 15 {
            break moved;
        }
        ensure(
            Instant::now() < deadline,
            format!("同次拖拽未实际移动 Pin：{before:?} -> {moved:?}"),
        )?;
        tick(cx).await;
    };
    guard.owner.post(WM_LBUTTONUP, 0, (p.0 + 25, p.1 + 15))?;
    settle(cx, &guard).await?;
    ensure(
        moved.width == before.width && moved.height == before.height,
        "关闭菜单同次拖动误改 Pin 尺寸",
    )?;
    report.write("same-click-dismiss-and-drag", json!({"ok":true,"before":[before.x,before.y,before.width,before.height],
        "after":[moved.x,moved.y,moved.width,moved.height],"physical_delta":[25,15],"input":guard.released()?}))?;

    guard.owner.key(VK_SPACE.0 as usize)?;
    let deadline = Instant::now() + WAIT;
    let toolbar = loop {
        guard.released()?;
        if let Some(toolbar) = pin.update(cx, |view, _, _| view.toolbar)? {
            break toolbar;
        }
        ensure(Instant::now() < deadline, "空格未创建工具栏")?;
        tick(cx).await;
    };
    let bar = Owned::bind(toolbar.update(cx, |_, window, _| hwnd(window))?)?;
    wait_ready(toolbar, &bar, 0, Some(&guard), cx).await?;
    ensure(
        pin.update(cx, |view, window, cx| {
            view.chrome
                && view
                    .canvas
                    .as_ref()
                    .is_some_and(|canvas| canvas.read(cx).focus.is_focused(window))
        })?,
        "内联画布 Escape 专项未建立真实 Canvas FocusHandle 焦点",
    )?;
    let canvas_menu = open_menu(
        pin,
        &guard,
        Some(&drag_menu),
        "canvas-escape-right-up",
        report,
        cx,
    )
    .await?;
    owner_delivered_menu_wheel(pin, &canvas_menu, &guard, report, cx).await?;
    guard.owner.key(VK_ESCAPE.0 as usize)?;
    hidden(&canvas_menu.owned, &guard, cx).await?;
    ensure(
        pin.update(cx, |view, window, cx| {
            view.chrome
                && view.toolbar == Some(toolbar)
                && view
                    .canvas
                    .as_ref()
                    .is_some_and(|canvas| canvas.read(cx).focus.is_focused(window))
        })?,
        "Canvas 聚焦时 Escape 未优先仅关闭菜单，或错误收栏/移走画布焦点",
    )?;
    report.write("canvas-focused-escape-menu-only", json!({"ok":true,"canvas_gpui_focus_preserved":true,
        "chrome_and_original_toolbar_preserved":true,"menu_native_hidden":true,"input":guard.released()?}))?;
    let toolbar_menu = open_menu(
        pin,
        &guard,
        Some(&canvas_menu),
        "toolbar-dismiss-right-up",
        report,
        cx,
    )
    .await?;
    let (id, p) = toolbar.update(cx, |view, window, _| -> Result<_> {
        for id in ["move", "undo", "redo"] {
            if let Some(p) = view.positions.borrow().get(id).copied() {
                let physical = (
                    (p.0 * window.scale_factor()).round() as i32,
                    (p.1 * window.scale_factor()).round() as i32,
                );
                if hit(&bar, physical).is_ok() {
                    return Ok((id, physical));
                }
            }
        }
        Err("没有可命中的自有工具栏安全按钮".into())
    })??;
    bar.post(WM_LBUTTONDOWN, 1, p)?;
    settle(cx, &guard).await?;
    bar.post(WM_LBUTTONUP, 0, p)?;
    settle(cx, &guard).await?;
    hidden(&toolbar_menu.owned, &guard, cx).await?;
    report.write(
        "toolbar-click-dismiss",
        json!({"ok":true,"toolbar_hwnd":bar.handle,"button":id,
        "menu_native_hidden":true,"input":guard.released()?}),
    )?;
    guard.owner.key(VK_SPACE.0 as usize)?;
    let deadline = Instant::now() + WAIT;
    loop {
        guard.released()?;
        let closed = pin.update(cx, |view, _, _| !view.chrome && view.toolbar.is_none())?;
        if closed && !unsafe { IsWindow(Some(bar.hwnd())) }.as_bool() {
            break;
        }
        ensure(Instant::now() < deadline, "空格收栏后原工具栏 HWND 仍残留")?;
        tick(cx).await;
    }

    let escape_menu = open_menu(
        pin,
        &guard,
        Some(&toolbar_menu),
        "escape-right-up",
        report,
        cx,
    )
    .await?;
    // 最后的原 owner 关闭后仍需核验隐藏菜单同步销毁，保留一个隐藏的本进程 GPUI keeper。
    let keeper = cx.update(|cx| {
        cx.open_window(
            WindowOptions {
                show: false,
                focus: false,
                ..Default::default()
            },
            |_, cx| cx.new(|_| KeepAlive),
        )
    })??;
    let keeper_owned = Owned::bind(keeper.update(cx, |_, window, _| hwnd(window))?)?;
    guard.released()?;
    guard.owner.key(VK_ESCAPE.0 as usize)?;
    hidden(&escape_menu.owned, &guard, cx).await?;
    guard.owner.alive()?;
    ensure(
        guard.owner.visible()?,
        "第一次 Escape 关闭了 Pin，而不是仅关闭菜单",
    )?;
    report.write(
        "escape-menu-only",
        json!({"ok":true,"owner_hwnd":guard.owner.handle,
        "owner_native_alive_and_visible":true,"menu_native_hidden":true,"input":guard.released()?}),
    )?;
    guard.owner.key(VK_ESCAPE.0 as usize)?;
    let deadline = Instant::now() + WAIT;
    loop {
        ensure(last_input()? == input_tick, "最终关闭期间真实输入改变")?;
        let owner_alive = unsafe { IsWindow(Some(guard.owner.hwnd())) }.as_bool();
        let menu_alive = unsafe { IsWindow(Some(escape_menu.owned.hwnd())) }.as_bool();
        if !owner_alive && !menu_alive {
            break;
        }
        ensure(
            Instant::now() < deadline,
            "第二次 Escape 后原 Pin 或复用菜单 HWND 仍存在",
        )?;
        tick(cx).await;
    }
    ensure(
        cx.update(|cx| pin.read(cx).is_err() && escape_menu.handle.read(cx).is_err())?,
        "原 GPUI owner/menu 仍可读取",
    )?;
    let final_input = native_input(&keeper_owned)?;
    ensure(
        final_input.capture == 0,
        "原 Pin/菜单销毁后 mouse capture 仍残留",
    )?;
    report.write(
        "escape-owner-and-menu-destroyed",
        json!({"ok":true,"original_owner_hwnd":guard.owner.handle,
        "original_menu_hwnd":escape_menu.owned.handle,"original_toolbar_hwnd":bar.handle,
        "owner_menu_toolbar_native_destroyed":true,"owner_menu_gpui_removed":true,"final_input":final_input,
        "final_foreground_scope":"owner close may naturally change focus; only capture cleanup required after close"}),
    )?;
    keeper.update(cx, |_, window, _| window.remove_window())?;
    Ok(())
}

pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    let initial = (|| -> Result<_> {
        let folder = PathBuf::from(
            std::env::var_os("RUSTSHOT_PIN_MENU_QA_DIR").ok_or("缺少独立菜单 QA 目录")?,
        )
        .canonicalize()?;
        let settings = crate::preferences::data_dir().canonicalize()?;
        ensure(
            settings == folder.join("settings"),
            "菜单专项配置未隔离到本次目录",
        )?;
        let input_tick: u32 = std::env::var("RUSTSHOT_PIN_MENU_QA_INPUT_TICK")?.parse()?;
        ensure(
            last_input()? == input_tick,
            "父进程预启动 input tick 已改变；未创建合成 Pin",
        )?;
        for key in [
            VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN, VK_LBUTTON, VK_RBUTTON, VK_MBUTTON,
        ] {
            ensure(
                unsafe { GetAsyncKeyState(key.0 as i32) } >= 0,
                "专项开始时有按键/鼠标键按住",
            )?;
        }
        let preferences = crate::preferences::Preferences {
            history: false,
            pin_restore: false,
            quick_dir: folder.join("exports"),
            ..Default::default()
        };
        preferences.save()?;
        let report = Report {
            path: folder.join("native.jsonl"),
        };
        report.write("start", json!({"pid":std::process::id(),"settings":settings,
            "parent_prelaunch_input_tick":input_tick,"global_input_sent":false,"global_focus_set_by_qa":false,
            "clipboard_accessed":false,"png_written":false,"real_desktop_content_saved":false,
            "limitations":["定向 Win32 消息，不模拟系统硬件命中/全局输入；WM_MOUSEACTIVATE 单独检查返回值",
                "当前工作区单个 640×400 合成 Pin；跨屏混合 DPI/外部应用点击失活另行验证",
                "呈现时间为 native 轮询观测上界，首次与复用各一次，不作为性能分位数或 P0 100ms 验收",
                "菜单只测一个 GPU 背景像素及一个归属验证的自有屏幕点，未证明全部菜单文字/图像"]}))?;
        let work = native::work_area((100, 100));
        let pin = PinView::open_state(
            Image {
                width: 640,
                height: 400,
                bgra: COLOR.repeat(640 * 400),
                readback_ms: 0.,
            },
            (work.x + 100, work.y + 100),
            PinState::default(),
            cx,
        )?;
        let owner = Owned::bind(pin.update(cx, |_, window, _| hwnd(window))?)?;
        Ok((pin, owner, input_tick, report))
    })();
    let (pin, owner, input_tick, report) = match initial {
        Ok(value) => value,
        Err(error) => {
            *errors.borrow_mut() = Some(format!("贴图菜单专项初始化：{error}"));
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let result = verify(pin, owner, input_tick, &report, cx).await;
        match result {
            Ok(()) => {
                if let Err(error) = report.write("complete", json!({"ok":true,"original_input_tick_unchanged":true,
                    "global_input_sent":false,"global_focus_set_by_qa":false,"clipboard_accessed":false,"png_written":false})) {
                    *errors.borrow_mut() = Some(format!("贴图菜单专项记录：{error}"));
                } else {
                    *errors.borrow_mut() = None;
                }
            }
            Err(error) => {
                let text = format!("贴图菜单专项：{error}");
                let _ = report.write("failed", json!({"ok":false,"error":text,"old_failures_reclassified":false}));
                *errors.borrow_mut() = Some(text);
            }
        }
        let _ = cx.update(|cx| cx.quit());
    }).detach();
}
