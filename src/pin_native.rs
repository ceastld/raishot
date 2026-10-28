//! 贴图 HWND 操作；托盘只对自己持有的工作进程和显式标记的贴图发命令。
use crate::{pin_model::Placement, Result};
use windows::{
    core::w,
    Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*},
};
pub fn rect(h: HWND) -> Result<Placement> {
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(h, &mut r)?;
    }
    Ok(Placement {
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(1) as u32,
        height: (r.bottom - r.top).max(1) as u32,
    })
}
pub fn work_area(origin: (i32, i32)) -> Placement {
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: origin.0,
                y: origin.1,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = info.rcWork;
            Placement {
                x: r.left,
                y: r.top,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
            }
        } else {
            Placement {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            }
        }
    }
}
/// 初建和 GPUI 隐藏窗口缓存的放置都使用目标显示器 DIP，最终 HWND 仍用物理坐标。
pub(crate) fn initial_pin_bounds(
    p: Placement,
    cx: &gpui::App,
) -> (Option<gpui::DisplayId>, gpui::Bounds<gpui::Pixels>) {
    let center = POINT {
        x: p.x.saturating_add((p.width / 2) as i32),
        y: p.y.saturating_add((p.height / 2) as i32),
    };
    let monitor = unsafe { MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    let scale = if unsafe {
        windows::Win32::UI::HiDpi::GetDpiForMonitor(
            monitor,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        )
    }
    .is_ok()
        && dpi_x > 0
    {
        dpi_x as f32 / 96.
    } else {
        1.
    };
    let display_id = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let r = info.rcMonitor;
        cx.displays()
            .iter()
            .find(|display| {
                let b = display.bounds();
                (f32::from(b.origin.x) * scale - r.left as f32).abs() < 2.
                    && (f32::from(b.origin.y) * scale - r.top as f32).abs() < 2.
                    && (f32::from(b.size.width) * scale - (r.right - r.left) as f32).abs() < 2.
                    && (f32::from(b.size.height) * scale - (r.bottom - r.top) as f32).abs() < 2.
            })
            .map(|display| display.id())
    } else {
        None
    };
    let bounds = gpui::Bounds::new(
        gpui::point(gpui::px(p.x as f32 / scale), gpui::px(p.y as f32 / scale)),
        gpui::size(
            gpui::px(p.width as f32 / scale),
            gpui::px(p.height as f32 / scale),
        ),
    );
    (display_id, bounds)
}
pub fn place(h: HWND, p: Placement) -> Result<()> {
    place_window(h, p)?;
    crate::pin_shadow::sync_native(h);
    Ok(())
}
/// 只改主窗口，不同步阴影；调用方成功后须自行 `pin_shadow::sync_native`。
/// 改尺寸时先画新内容再重绘阴影，让阴影与内容尽量落在同一合成帧。
pub(crate) fn place_window(h: HWND, p: Placement) -> Result<()> {
    unsafe {
        SetWindowPos(
            h,
            None,
            p.x,
            p.y,
            p.width as i32,
            p.height as i32,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    Ok(())
}
/// 改尺寸时能否先按目标尺寸画好再移动：客户区须与窗口矩形等大（无边框），
/// 目标矩形所在显示器的 DPI 须与当前窗口相同，否则移动会触发 DPI 变化，只能先移动再绘制。
/// 返回目标客户区物理矩形。
pub(crate) fn present_first_bounds(
    h: HWND,
    p: Placement,
) -> Option<gpui::Bounds<gpui::DevicePixels>> {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
    // 仅 QA 对照：恢复“先移动再绘制”。
    if std::env::var_os("RUSTSHOT_PIN_ZOOM_PLACE_FIRST").is_some() {
        return None;
    }
    let current = rect(h).ok()?;
    let mut client = RECT::default();
    unsafe { GetClientRect(h, &mut client) }.ok()?;
    if (client.right - client.left, client.bottom - client.top)
        != (current.width as i32, current.height as i32)
    {
        return None;
    }
    let target = RECT {
        left: p.x,
        top: p.y,
        right: p.x.checked_add(i32::try_from(p.width).ok()?)?,
        bottom: p.y.checked_add(i32::try_from(p.height).ok()?)?,
    };
    let (mut dpi_x, mut dpi_y) = (0, 0);
    unsafe {
        let monitor = MonitorFromRect(&target, MONITOR_DEFAULTTONEAREST);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).ok()?;
        if dpi_x == 0 || dpi_x != GetDpiForWindow(h) {
            return None;
        }
    }
    Some(gpui::Bounds::new(
        gpui::point(gpui::DevicePixels(p.x), gpui::DevicePixels(p.y)),
        gpui::size(
            gpui::DevicePixels(target.right - target.left),
            gpui::DevicePixels(target.bottom - target.top),
        ),
    ))
}
/// 等 DWM 完成下一次合成；失败（如合成暂停）立即返回。
pub(crate) fn wait_for_composition() {
    unsafe {
        let _ = DwmFlush();
    }
}
/// GPUI 的尺寸回调会同步更新 Window；必须在当前 App/Window 借用结束后移动浮窗。
/// 前台任务直接调用 Win32，不在 App::update 内执行，避免旧 viewport 和回调重入。
pub(crate) fn place_later(
    h: HWND,
    p: Placement,
    topmost: Option<bool>,
    frame_changed: bool,
    cx: &gpui::App,
) {
    let handle = h.0 as usize;
    cx.spawn(async move |_| {
        let h = HWND(handle as *mut _);
        if !unsafe { IsWindow(Some(h)) }.as_bool() {
            return;
        }
        // 多个观察者可能在同一帧安排相同定位；任务执行时再检查，避免重复原生重排。
        if !frame_changed
            && rect(h).is_ok_and(|current| current == p)
            && topmost.is_none_or(|on| {
                (unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize != 0) == on
            })
        {
            crate::pin_shadow::sync_native(h);
            return;
        }
        let mut flags = SWP_NOACTIVATE;
        if frame_changed {
            flags |= SWP_SHOWWINDOW | SWP_FRAMECHANGED;
        }
        let after = topmost.map(|on| if on { HWND_TOPMOST } else { HWND_NOTOPMOST });
        if after.is_none() {
            flags |= SWP_NOZORDER;
        }
        unsafe {
            let _ = SetWindowPos(h, after, p.x, p.y, p.width as i32, p.height as i32, flags);
        }
        crate::pin_shadow::sync_native(h);
    })
    .detach();
}
pub fn topmost(h: HWND, on: bool) -> Result<()> {
    unsafe {
        SetWindowPos(
            h,
            Some(if on { HWND_TOPMOST } else { HWND_NOTOPMOST }),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
    }
    crate::pin_shadow::sync_native(h);
    Ok(())
}
/// 工作区首建也经过同一显隐门，不能在首帧上传之前显示窗口。
pub(crate) fn begin_initial_pin(h: HWND, show: bool) -> Result<()> {
    unsafe {
        let mut owner = 0;
        GetWindowThreadProcessId(h, Some(&mut owner));
        if owner != std::process::id() || GetPropW(h, w!("RustShot.Pin.v1")).is_invalid() {
            return Err("贴图首显 HWND 尚未登记到当前进程".into());
        }
        SetPropW(
            h,
            w!("RustShot.Pin.Starting"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )?;
        defer_initial_visibility(h, show)?;
    }
    Ok(())
}
pub(crate) fn defer_initial_visibility(h: HWND, show: bool) -> Result<bool> {
    unsafe {
        if GetPropW(h, w!("RustShot.Pin.Starting")).is_invalid() {
            return Ok(false);
        }
        SetPropW(
            h,
            w!("RustShot.Pin.InitialVisibility"),
            Some(HANDLE((if show { 2usize } else { 1usize }) as *mut _)),
        )?;
    }
    Ok(true)
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct FirstShowProbe {
    pub starting: bool,
    pub requested_visible: bool,
    pub first_show_ready: bool,
    pub prepared_serial: usize,
    pub prepared_unix_ms: u64,
    pub first_show_serial: usize,
    pub presented_size: [u32; 2],
    pub first_show_style: u32,
    pub first_show_rect: [i32; 4],
    pub first_show_unix_ms: u64,
    pub visible: bool,
    pub aborted: bool,
}
/// 仅由原 WindowHandle 的实际首显门产出；不以 on_next_frame 代替成功 Present。
pub(crate) struct PinVisibleReady {
    pub window: gpui::AnyWindowHandle,
    pub hwnd: usize,
    pub process_id: u32,
    pub thread_id: u32,
    pub presented_serial: usize,
    pub placement: Placement,
    pub dwm_flushed: bool,
    pub timing: Option<PinHandoffTiming>,
}
impl PinVisibleReady {
    /// 原截图已取消时，回收本次尚未交接的贴图；不按 PID 查找或关闭其它窗口。
    pub(crate) fn discard(self, cx: &mut gpui::AsyncApp) -> Result<bool> {
        use gpui::AppContext;
        let h = HWND(self.hwnd as *mut _);
        let same = cx
            .update_window(self.window, |_, w, _| crate::gpui_capture::hwnd(w) == h)
            .unwrap_or(false);
        let mut owner = 0;
        let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
        if !same
            || owner != self.process_id
            || owner != std::process::id()
            || thread != self.thread_id
            || unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 as usize != self.hwnd
        {
            return Ok(false);
        }
        // 原生隐藏可能同步回调，先结束身份查询的 App 借用。
        crate::pin_shadow::hide_native(h);
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
        Ok(cx.update_window(self.window, |view, w, cx| {
            if crate::gpui_capture::hwnd(w) != h {
                return false;
            }
            let Ok(pin) = view.downcast::<crate::pin_view::PinView>() else {
                return false;
            };
            pin.update(cx, |view, cx| {
                view.workspace = None;
                view.close_toolbar(cx);
                w.remove_window();
            });
            true
        })?)
    }
}
pub(crate) type InitialPinCompletion =
    Box<dyn FnOnce(std::result::Result<PinVisibleReady, String>, &mut gpui::AsyncApp)>;
pub(crate) struct CaptureHandoffOwner {
    window: gpui::AnyWindowHandle,
    handle: usize,
    process_id: u32,
    thread_id: u32,
}
impl CaptureHandoffOwner {
    pub(crate) fn new(window: gpui::AnyWindowHandle, h: HWND) -> Result<Self> {
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(h, Some(&mut process_id)) };
        if process_id != std::process::id()
            || thread_id == 0
            || !unsafe { IsWindow(Some(h)) }.as_bool()
        {
            return Err("贴图交接原截图窗口不是当前进程有效窗口".into());
        }
        Ok(Self {
            window,
            handle: h.0 as usize,
            process_id,
            thread_id,
        })
    }
    fn alive(&self, cx: &mut gpui::AsyncApp) -> bool {
        use gpui::AppContext;
        let h = HWND(self.handle as *mut _);
        let same = cx
            .update_window(self.window, |_, w, _| crate::gpui_capture::hwnd(w) == h)
            .unwrap_or(false);
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(h, Some(&mut process_id)) };
        same && process_id == self.process_id
            && thread_id == self.thread_id
            && unsafe { IsWindow(Some(h)) }.as_bool()
    }
}
/// 仅专项 QA 启用；普通贴图不读性能时钟、不分配阶段集合。
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PinHandoffTiming {
    pub frequency: i64,
    pub placement_started_qpc: i64,
    pub hidden_placement_completed_qpc: Option<i64>,
    pub successful_present_observed_qpc: Option<i64>,
    pub show_started_qpc: Option<i64>,
    pub show_completed_qpc: Option<i64>,
    pub dwm_flush_completed_qpc: Option<i64>,
    pub visible_ready_qpc: Option<i64>,
}
impl PinHandoffTiming {
    fn begin() -> Option<Self> {
        if std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() != Ok("1") {
            return None;
        }
        let mut frequency = 0;
        if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
            return None;
        }
        Some(Self {
            frequency,
            placement_started_qpc: handoff_qpc()?,
            hidden_placement_completed_qpc: None,
            successful_present_observed_qpc: None,
            show_started_qpc: None,
            show_completed_qpc: None,
            dwm_flush_completed_qpc: None,
            visible_ready_qpc: None,
        })
    }
}
fn handoff_qpc() -> Option<i64> {
    let mut value = 0;
    (unsafe { QueryPerformanceCounter(&mut value) } != 0).then_some(value)
}
fn complete_initial_pin(
    completion: &mut Option<InitialPinCompletion>,
    result: std::result::Result<PinVisibleReady, String>,
    cx: &mut gpui::AsyncApp,
) {
    if let Some(completion) = completion.take() {
        completion(result, cx);
    }
}
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmFlush() -> i32;
    fn DwmGetWindowAttribute(
        hwnd: HWND,
        attribute: u32,
        value: *mut std::ffi::c_void,
        bytes: u32,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}
/// 只读原生小元数据，不延长任何源图或 GPU 资源的生命期。
pub(crate) fn first_show_probe(h: HWND) -> FirstShowProbe {
    unsafe {
        let value = |key| GetPropW(h, key).0 as usize;
        let size = value(w!("RustShot.Pin.PreparedSize")) as u32;
        FirstShowProbe {
            starting: value(w!("RustShot.Pin.Starting")) != 0,
            requested_visible: value(w!("RustShot.Pin.InitialVisibility")) == 2,
            first_show_ready: value(w!("RustShot.Pin.PreparedSerial")) != 0
                && value(w!("RustShot.Pin.StartupAborted")) == 0,
            prepared_serial: value(w!("RustShot.Pin.PreparedSerial")),
            prepared_unix_ms: value(w!("RustShot.Pin.PreparedUnixMs")) as u64,
            first_show_serial: value(w!("RustShot.Pin.FirstShowSerial")),
            presented_size: [size >> 16, size & 0xffff],
            first_show_style: value(w!("RustShot.Pin.FirstShowStyle")) as u32,
            first_show_rect: [
                value(w!("RustShot.Pin.FirstShowX")) as i32,
                value(w!("RustShot.Pin.FirstShowY")) as i32,
                value(w!("RustShot.Pin.FirstShowWidth")) as i32,
                value(w!("RustShot.Pin.FirstShowHeight")) as i32,
            ],
            first_show_unix_ms: value(w!("RustShot.Pin.FirstShowUnixMs")) as u64,
            visible: IsWindowVisible(h).as_bool(),
            aborted: value(w!("RustShot.Pin.StartupAborted")) != 0,
        }
    }
}
/// 首显失败不是用户关闭：还没有可操作的编辑，不能因保存失败永久留下隐藏宿主。
/// 只清理原 WindowHandle 的未完成初始化，释放租约；已有 canonical 恢复图片不改写。
fn abort_initial_pin(
    window: gpui::AnyWindowHandle,
    h: HWND,
    original_thread: u32,
    cx: &mut gpui::AsyncApp,
) {
    use gpui::AppContext;
    let identity = cx
        .update_window(window, |_, w, _| {
            crate::gpui_capture::hwnd(w) == h
                && unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 == h.0
                && !unsafe { GetPropW(h, w!("RustShot.Pin.Starting")) }.is_invalid()
        })
        .unwrap_or(false);
    let mut owner = 0;
    let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
    if !identity
        || owner != std::process::id()
        || thread == 0
        || thread != original_thread
        || !unsafe { IsWindow(Some(h)) }.as_bool()
    {
        return;
    }
    let marked = unsafe {
        SetPropW(
            h,
            w!("RustShot.Pin.StartupAborted"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )
    };
    if let Err(error) = marked {
        eprintln!("贴图初始化取消标记失败：{error}");
        return;
    }
    // 原生显隐可能同步回调 GPUI；必须先离开任何 App / Window 借用。
    crate::pin_shadow::hide_native(h);
    unsafe {
        let _ = ShowWindow(h, SW_HIDE);
    }
    let removed = cx.update_window(window, |view, w, cx| {
        if crate::gpui_capture::hwnd(w) != h
            || unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 != h.0
            || unsafe { GetPropW(h, w!("RustShot.Pin.Starting")) }.is_invalid()
            || unsafe { GetPropW(h, w!("RustShot.Pin.StartupAborted")) }.is_invalid()
        {
            return false;
        }
        let Ok(pin) = view.downcast::<crate::pin_view::PinView>() else {
            return false;
        };
        pin.update(cx, |v, cx| {
            v.workspace = None;
            v.close_toolbar(cx);
            w.remove_window();
        });
        true
    });
    match removed {
        Ok(true) => {}
        Ok(false) => eprintln!("贴图初始化取消时原窗口身份已变更，未操作其它窗口"),
        Err(error) => eprintln!("贴图初始化取消回收失败：{error}"),
    }
}
fn pin_viewport_matches(logical: [f32; 2], dpi: f32, expected: (u32, u32)) -> bool {
    dpi.is_finite()
        && dpi > 0.
        && logical.iter().all(|value| value.is_finite() && *value > 0.)
        && (logical[0] * dpi).round() == expected.0 as f32
        && (logical[1] * dpi).round() == expected.1 as f32
}

/// 首建保持隐藏，等尺寸正确的 GPUI 成功 Present 后再显示；不重放初始缓存尺寸。
/// 所有原生显示/焦点操作均在 App/Window 借用结束后，且绑定原 WindowHandle。
pub(crate) fn place_initial_pin(
    window: gpui::AnyWindowHandle,
    h: HWND,
    p: Placement,
    activate: bool,
    cx: &gpui::App,
) {
    place_initial_pin_with_completion(window, h, p, activate, None, None, cx);
}
pub(crate) fn place_initial_pin_with_completion(
    window: gpui::AnyWindowHandle,
    h: HWND,
    p: Placement,
    activate: bool,
    capture_owner: Option<CaptureHandoffOwner>,
    mut completion: Option<InitialPinCompletion>,
    cx: &gpui::App,
) {
    let handle = h.0 as usize;
    let initial_foreground = unsafe { GetForegroundWindow() };
    let mut original_owner = 0;
    let original_thread = unsafe { GetWindowThreadProcessId(h, Some(&mut original_owner)) };
    cx.spawn(async move |cx| {
        use gpui::AppContext;
        let mut timing = PinHandoffTiming::begin();
        let since = std::time::Instant::now();
        let mut placed = false;
        let mut initial_serial = 0;
        let mut frame_hint = None;
        loop {
            let h = HWND(handle as *mut _);
            if capture_owner.as_ref().is_some_and(|owner| !owner.alive(cx)) {
                abort_initial_pin(window, h, original_thread, cx);
                complete_initial_pin(
                    &mut completion,
                    Err("原截图已取消，未显示本次贴图".into()),
                    cx,
                );
                return;
            }
            let (alive, viewport_matches, viewport_logical, scale, source_ready) = cx
                .update_window(window, |view, w, cx| {
                    let alive = crate::gpui_capture::hwnd(w).0 as usize == handle;
                    if alive {
                        w.refresh();
                    }
                    let size = w.viewport_size();
                    let scale = w.scale_factor();
                    let source_ready =
                        view.downcast::<crate::pin_view::PinView>()
                            .is_ok_and(|pin| {
                                pin.read(cx).initial_capture_content_ready(
                                    (p.width, p.height),
                                    w,
                                    cx,
                                )
                            });
                    (
                        alive,
                        pin_viewport_matches(
                            [f32::from(size.width), f32::from(size.height)],
                            scale,
                            (p.width, p.height),
                        ),
                        [f32::from(size.width), f32::from(size.height)],
                        scale,
                        source_ready,
                    )
                })
                .unwrap_or((false, false, [0., 0.], 0., false));
            let mut owner = 0;
            let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
            if !alive
                || owner != std::process::id()
                || owner != original_owner
                || thread == 0
                || thread != original_thread
                || !unsafe { IsWindow(Some(h)) }.as_bool()
                || unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 as usize != handle
                || unsafe { GetPropW(h, w!("RustShot.Pin.Starting")) }.is_invalid()
            {
                complete_initial_pin(
                    &mut completion,
                    Err("贴图首显前原窗口已关闭或身份变更".into()),
                    cx,
                );
                return;
            }
            if !placed {
                // 工厂闭包时 Window 尚未登记，原生尺寸回调无法回填它的 viewport。
                // 在 App 借用外补一次隐藏定位；完成真正的布局后才允许 Present/首显。
                let positioned = unsafe {
                    SetWindowPos(
                        h,
                        None,
                        p.x,
                        p.y,
                        p.width as i32,
                        p.height as i32,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_HIDEWINDOW,
                    )
                };
                if let Err(error) = positioned {
                    eprintln!("贴图隐藏定位失败：{error}");
                    abort_initial_pin(window, h, original_thread, cx);
                    complete_initial_pin(
                        &mut completion,
                        Err(format!("贴图隐藏定位失败：{error}")),
                        cx,
                    );
                    return;
                }
                crate::pin_shadow::sync_native(h);
                initial_serial =
                    unsafe { GetPropW(h, w!("RustShot.GpuiPresentSerial")) }.0 as usize;
                // 工厂已放在最终尺寸，同矩形 SetWindowPos 可能不再产生 WM_SIZE。
                // 重发真实客户区的尺寸通知，让已登记的 GPUI Window 同步 viewport。
                // 不改变几何、不伪造大小，仍须随后成功 Present 和全部实际尺寸校验。
                let synchronized = (|| -> Result<()> {
                    let mut client = RECT::default();
                    unsafe {
                        GetClientRect(h, &mut client)?;
                    }
                    let width = u16::try_from(client.right - client.left)?;
                    let height = u16::try_from(client.bottom - client.top)?;
                    if width == 0 || height == 0 {
                        return Err("贴图隐藏客户区为空".into());
                    }
                    let packed = u32::from(width) | (u32::from(height) << 16);
                    unsafe {
                        PostMessageW(
                            Some(h),
                            WM_SIZE,
                            WPARAM(SIZE_RESTORED as usize),
                            LPARAM(packed as isize),
                        )?;
                    }
                    Ok(())
                })();
                if let Err(error) = synchronized {
                    eprintln!("贴图隐藏视口同步失败：{error}");
                    abort_initial_pin(window, h, original_thread, cx);
                    complete_initial_pin(
                        &mut completion,
                        Err(format!("贴图隐藏视口同步失败：{error}")),
                        cx,
                    );
                    return;
                }
                if let Some(timing) = &mut timing {
                    timing.hidden_placement_completed_qpc = handoff_qpc();
                }
                placed = true;
                continue;
            }
            let serial = unsafe { GetPropW(h, w!("RustShot.GpuiPresentSerial")) }.0 as usize;
            let size = unsafe { GetPropW(h, w!("RustShot.GpuiPresentSize")) }.0 as usize as u32;
            let mut client = RECT::default();
            let mut client_origin = POINT::default();
            let client_matches = unsafe { GetClientRect(h, &mut client) }.is_ok()
                && client.right - client.left == p.width as i32
                && client.bottom - client.top == p.height as i32
                && unsafe { ClientToScreen(h, &mut client_origin) }.as_bool()
                && client_origin.x == p.x
                && client_origin.y == p.y;
            let prepared = serial != 0
                && serial != initial_serial
                && size == ((p.width << 16) | p.height)
                && viewport_matches
                && client_matches
                && source_ready
                && rect(h).is_ok_and(|actual| actual == p);
            if prepared {
                if let Some(timing) = &mut timing {
                    timing.successful_present_observed_qpc = handoff_qpc();
                }
                let show =
                    unsafe { GetPropW(h, w!("RustShot.Pin.InitialVisibility")) }.0 as usize == 2;
                let result = (|| -> Result<()> {
                    if capture_owner.as_ref().is_some_and(|owner| !owner.alive(cx)) {
                        return Err("贴图显示前原截图已取消".into());
                    }
                    unsafe {
                        SetPropW(
                            h,
                            w!("RustShot.Pin.PreparedSerial"),
                            Some(HANDLE(serial as *mut _)),
                        )?;
                        SetPropW(
                            h,
                            w!("RustShot.Pin.PreparedSize"),
                            Some(HANDLE(size as usize as *mut _)),
                        )?;
                        let prepared_at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_millis() as usize;
                        SetPropW(
                            h,
                            w!("RustShot.Pin.PreparedUnixMs"),
                            Some(HANDLE(prepared_at as *mut _)),
                        )?;
                        // 样式和客户区已经正确；首显不再 resize / FRAMECHANGED 清空 swapchain。
                        if let Some(timing) = &mut timing {
                            timing.show_started_qpc = handoff_qpc();
                        }
                        SetWindowPos(
                            h,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOZORDER
                                | SWP_NOACTIVATE
                                | SWP_NOMOVE
                                | SWP_NOSIZE
                                | if show { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW },
                        )?;
                        if let Some(timing) = &mut timing {
                            timing.show_completed_qpc = handoff_qpc();
                        }
                        if show {
                            if !IsWindowVisible(h).as_bool() {
                                return Err("贴图首帧成功后窗口仍未显示".into());
                            }
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)?
                                .as_millis() as usize;
                            for (key, value) in [
                                (w!("RustShot.Pin.FirstShowSerial"), serial),
                                (
                                    w!("RustShot.Pin.FirstShowStyle"),
                                    GetWindowLongPtrW(h, GWL_STYLE) as usize,
                                ),
                                (w!("RustShot.Pin.FirstShowX"), p.x as usize),
                                (w!("RustShot.Pin.FirstShowY"), p.y as usize),
                                (w!("RustShot.Pin.FirstShowWidth"), p.width as usize),
                                (w!("RustShot.Pin.FirstShowHeight"), p.height as usize),
                                (w!("RustShot.Pin.FirstShowUnixMs"), timestamp),
                            ] {
                                SetPropW(h, key, Some(HANDLE(value as *mut _)))?;
                            }
                        }
                        let foreground = GetForegroundWindow();
                        let mut foreground_owner = 0;
                        GetWindowThreadProcessId(foreground, Some(&mut foreground_owner));
                        if show
                            && activate
                            && (foreground == initial_foreground
                                || foreground_owner == std::process::id())
                        {
                            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow(h);
                            let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(h));
                        }
                    }
                    crate::pin_shadow::sync_native(h);
                    if completion.is_some() {
                        if !show {
                            return Err("截图交接的贴图首显被改为隐藏".into());
                        }
                        let flushed = unsafe { DwmFlush() };
                        if flushed < 0 {
                            return Err(windows::core::Error::from_hresult(
                                windows::core::HRESULT(flushed),
                            )
                            .into());
                        }
                        if let Some(timing) = &mut timing {
                            timing.dwm_flush_completed_qpc = handoff_qpc();
                        }
                        // DWM 同步之后再查原窗口；此门不等于其它窗口无遮挡的屏幕像素证明。
                        let same_window = cx
                            .update_window(window, |_, w, _| crate::gpui_capture::hwnd(w) == h)
                            .unwrap_or(false);
                        let mut final_owner = 0;
                        let final_thread =
                            unsafe { GetWindowThreadProcessId(h, Some(&mut final_owner)) };
                        let mut final_client = RECT::default();
                        let mut final_client_origin = POINT::default();
                        let mut cloaked = u32::MAX;
                        let cloak_result = unsafe {
                            DwmGetWindowAttribute(h, 14, (&mut cloaked as *mut u32).cast(), 4)
                        };
                        let live_size = unsafe { GetPropW(h, w!("RustShot.GpuiPresentSize")) }.0
                            as usize as u32;
                        let live_serial =
                            unsafe { GetPropW(h, w!("RustShot.GpuiPresentSerial")) }.0 as usize;
                        let style = unsafe { GetWindowLongPtrW(h, GWL_STYLE) } as u32;
                        let content_point = POINT {
                            x: p.x.saturating_add((p.width / 2) as i32),
                            y: p.y.saturating_add((p.height / 2) as i32),
                        };
                        let content_window = unsafe { WindowFromPoint(content_point) };
                        let content_on_top =
                            content_window == h || unsafe { IsChild(h, content_window) }.as_bool();
                        if !same_window
                            || final_owner != original_owner
                            || final_thread != original_thread
                            || unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 as usize != handle
                            || !unsafe { IsWindowVisible(h) }.as_bool()
                            || cloak_result < 0
                            || cloaked != 0
                            || unsafe { IsIconic(h) }.as_bool()
                            || !content_on_top
                            || rect(h)? != p
                            || unsafe { GetClientRect(h, &mut final_client) }.is_err()
                            || final_client.right - final_client.left != p.width as i32
                            || final_client.bottom - final_client.top != p.height as i32
                            || !unsafe { ClientToScreen(h, &mut final_client_origin) }.as_bool()
                            || final_client_origin.x != p.x
                            || final_client_origin.y != p.y
                            || live_serial < serial
                            || live_size != size
                            || style & WS_POPUP.0 == 0
                            || style & WS_CAPTION.0 != 0
                        {
                            return Err("贴图 DWM 首显后身份、尺寸、样式或可见状态校验失败".into());
                        }
                        if let Some(timing) = &mut timing {
                            timing.visible_ready_qpc = handoff_qpc();
                        }
                    }
                    if unsafe { RemovePropW(h, w!("RustShot.Pin.Starting")) }?.is_invalid() {
                        return Err("贴图首显完成标记移除失败".into());
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    eprintln!("贴图首显失败：{error}");
                    abort_initial_pin(window, h, original_thread, cx);
                    complete_initial_pin(
                        &mut completion,
                        Err(format!("贴图首显失败：{error}")),
                        cx,
                    );
                } else {
                    complete_initial_pin(
                        &mut completion,
                        Ok(PinVisibleReady {
                            window,
                            hwnd: handle,
                            process_id: original_owner,
                            thread_id: original_thread,
                            presented_serial: serial,
                            placement: p,
                            dwm_flushed: true,
                            timing,
                        }),
                        cx,
                    );
                }
                return;
            }
            if since.elapsed() >= std::time::Duration::from_secs(5) {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "phase":"pin_startup_not_ready",
                        "hwnd":handle,"serial":serial,"initial_serial":initial_serial,
                        "presented_size":[size >> 16,size & 0xffff],
                        "expected_rect":[p.x,p.y,p.width as i32,p.height as i32],
                        "actual_rect":rect(h).ok().map(|r|[r.x,r.y,r.width as i32,r.height as i32]),
                        "client_rect":[client.left,client.top,client.right,client.bottom],
                        "client_origin":[client_origin.x,client_origin.y],
                        "viewport_logical":viewport_logical,"scale_factor":scale,
                        "viewport_physical":[viewport_logical[0]*scale,viewport_logical[1]*scale],
                        "viewport_matches":viewport_matches,"client_matches":client_matches,
                        "hidden":!unsafe {IsWindowVisible(h)}.as_bool(),
                    })
                );
                eprintln!("贴图首帧未成功呈现，已取消显示");
                abort_initial_pin(window, h, original_thread, cx);
                complete_initial_pin(
                    &mut completion,
                    Err("贴图首帧未成功呈现，已取消显示".into()),
                    cx,
                );
                return;
            }
            // 回调在 Present 前执行，只作为异步唤醒提示。醒来仍须重新核验真实
            // serial / 尺寸 / 原窗口身份等全部门，不能在回调中报告首显成功。
            // 定时器先到时保留同一个 receiver，避免未绘制时反复积累帧回调。
            if frame_hint.is_none() {
                let (sender, receiver) = futures::channel::oneshot::channel();
                if cx
                    .update_window(window, |_, w, _| {
                        w.on_next_frame(move |_, _| {
                            let _ = sender.send(());
                        });
                    })
                    .is_ok()
                {
                    frame_hint = Some(receiver);
                }
            }
            // 隐藏 HWND 没有可见的 invalid region；明确请求自有窗口呈现准备好的场景。
            // PostMessage 发生在 App 借用外，不显示窗口，也不模拟用户输入。
            unsafe {
                let _ = PostMessageW(Some(h), WM_PAINT, WPARAM(0), LPARAM(0));
            }
            let timer = cx
                .background_executor()
                .timer(std::time::Duration::from_millis(16));
            if let Some(receiver) = frame_hint.as_mut() {
                futures::pin_mut!(timer);
                if matches!(
                    futures::future::select(receiver, timer).await,
                    futures::future::Either::Left(_)
                ) {
                    frame_hint = None;
                }
            } else {
                timer.await;
            }
        }
    })
    .detach();
}
pub fn opacity(h: HWND, alpha: u8) -> Result<()> {
    unsafe {
        SetPropW(
            h,
            w!("RustShot.Pin.Opacity"),
            Some(HANDLE(alpha as usize as *mut _)),
        )?;
        let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
        SetLayeredWindowAttributes(
            h,
            COLORREF(0),
            if transparent(h) {
                alpha.min(153)
            } else {
                alpha
            },
            LWA_ALPHA,
        )?;
    }
    crate::pin_shadow::sync_native(h);
    Ok(())
}
pub fn transparent(h: HWND) -> bool {
    unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_TRANSPARENT.0 as isize != 0 }
}
pub fn click_through(h: HWND, on: bool) -> Result<()> {
    unsafe {
        let old = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let flags = (WS_EX_TRANSPARENT | WS_EX_NOACTIVATE).0 as isize;
        SetWindowLongPtrW(
            h,
            GWL_EXSTYLE,
            if on {
                old | flags | WS_EX_LAYERED.0 as isize
            } else {
                old & !flags
            },
        );
        let stored = GetPropW(h, w!("RustShot.Pin.Opacity")).0 as usize;
        let alpha = if stored == 0 {
            255
        } else {
            stored.min(255) as u8
        };
        SetLayeredWindowAttributes(
            h,
            COLORREF(0),
            if on { alpha.min(153) } else { alpha },
            LWA_ALPHA,
        )?;
        SetWindowPos(
            h,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;
    }
    crate::pin_shadow::sync_native(h);
    Ok(())
}
pub fn mark(h: HWND) -> Result<()> {
    unsafe {
        SetPropW(h, w!("RustShot.Pin.v1"), Some(HANDLE(h.0)))?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
pub enum Manage {
    Hide,
    Show,
    Interact,
    Close,
}
pub fn manage(owners: &[u32], action: Manage) -> usize {
    struct Request<'a> {
        owners: &'a [u32],
        action: Manage,
        count: usize,
    }
    unsafe extern "system" fn visit(h: HWND, p: LPARAM) -> windows::core::BOOL {
        let request = &mut *(p.0 as *mut Request<'_>);
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if request.owners.contains(&pid)
            && !GetPropW(h, w!("RustShot.Pin.v1")).is_invalid()
            && GetPropW(h, w!("RustShot.Pin.WorkspaceScope")).is_invalid()
        {
            request.count += 1;
            match request.action {
                Manage::Hide => {
                    crate::pin_shadow::hide_native(h);
                    let _ = ShowWindowAsync(h, SW_HIDE);
                }
                Manage::Show => {
                    let _ = ShowWindowAsync(h, SW_SHOWNOACTIVATE);
                }
                Manage::Interact => {
                    let _ = click_through(h, false);
                    let _ = ShowWindowAsync(h, SW_SHOWNOACTIVATE);
                }
                Manage::Close => {
                    let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        windows::core::BOOL(1)
    }
    let mut request = Request {
        owners,
        action,
        count: 0,
    };
    unsafe {
        let _ = EnumWindows(
            Some(visit),
            LPARAM((&mut request as *mut Request<'_>) as isize),
        );
    }
    request.count
}

#[cfg(test)]
mod viewport_tests {
    use super::pin_viewport_matches;

    #[test]
    fn fractional_dpi_roundoff_is_accepted_but_a_stale_physical_pixel_is_not() {
        let expected = (27, 25);
        for dpi in [1., 1.25, 1.5, 1.75, 2.] {
            let logical = [expected.0 as f32 / dpi, expected.1 as f32 / dpi];
            assert!(pin_viewport_matches(logical, dpi, expected));
            // 初始 GPUI 缓存可能仍是移动到另一显示器之前的物理尺寸。
            // 只能容忍浮点误差，不能把整整一个物理像素也当成误差放过。
            for axis in 0..2 {
                for delta in [-1., 1.] {
                    let mut stale = logical;
                    stale[axis] += delta / dpi;
                    assert!(!pin_viewport_matches(stale, dpi, expected));
                }
            }
        }
        for dpi in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(!pin_viewport_matches([27., 25.], dpi, expected));
        }
        assert!(!pin_viewport_matches([f32::NAN, 25.], 1., expected));
        assert!(!pin_viewport_matches([27., 0.], 1., expected));
    }
}
