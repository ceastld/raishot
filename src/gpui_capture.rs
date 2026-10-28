//! 平台入口：冻结多屏图像、建立全屏窗口、管理短生命周期截图会话。
use crate::{
    capture_view::CaptureView,
    desktop::DesktopCapture,
    geometry::{Rect, ScreenRect},
    gpui_smoke,
    quick_action::{QuickRecipe, Source},
    Result,
};
use gpui::{
    point, px, size, App, AppContext, Application, Bounds, RenderImage, TitlebarOptions, Window,
    WindowBounds, WindowKind, WindowOptions,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
        System::Threading::CreateMutexW,
        UI::{
            HiDpi::*,
            Input::KeyboardAndMouse::{GetCapture, ReleaseCapture},
            WindowsAndMessaging::*,
        },
    },
};

#[path = "capture_preparation.rs"]
mod preparation;

pub(crate) use crate::capture_lock::CaptureLock;
pub(crate) fn hwnd(window: &Window) -> HWND {
    match HasWindowHandle::window_handle(window)
        .expect("GPUI Windows handle")
        .as_raw()
    {
        RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
        _ => unreachable!("Windows only"),
    }
}
pub(crate) fn foreground_auxiliary(window: &Window) {
    unsafe {
        let native = hwnd(window);
        let _ = SetWindowPos(
            native,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(native);
    }
}

/// 图库工具窗：打开时提到最前并取得焦点，但不常驻置顶，切到别的窗口后按普通层级让位。
pub(crate) fn foreground_tool(window: &Window) {
    unsafe {
        let native = hwnd(window);
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
        // 先置顶再取消：后台进程拿不到前台权时也能把新窗口排到普通窗口最上面。
        let _ = SetWindowPos(native, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        let _ = SetWindowPos(native, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        let _ = SetForegroundWindow(native);
    }
}

fn owns_capture_activation(native: HWND, foreground: HWND, active: HWND, focus: HWND) -> bool {
    !native.is_invalid() && foreground == native && active == native && focus == native
}

fn recover_native_surface(
    result: Result<crate::startup_surface::StartupSurface>,
    fixture: bool,
) -> Result<Option<crate::startup_surface::StartupSurface>> {
    match result {
        Ok(surface) => Ok(Some(surface)),
        Err(error) if fixture || crate::capture_standby::is_cancelled(error.as_ref()) => Err(error),
        Err(error) => {
            // 首屏已经按创建线程规则回收。焦点竞争也可沿用同次冻结图，
            // 由 GPUI 显示路径重新取得并核验实际焦点，不能伪造原生 Ready。
            if crate::startup_surface::is_activation_failure(error.as_ref())
                || std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some()
            {
                eprintln!("快速框选界面不可用，从同次冻结图继续打开 GPUI：{error}");
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod capture_activation_tests {
    use super::*;

    #[test]
    fn quick_window_crop_clips_negative_coordinates_without_moving_document_origin() {
        let canvas = ScreenRect {
            left: -1920,
            top: -120,
            width: 3840,
            height: 1200,
        };
        let window = ScreenRect {
            left: -2000,
            top: -200,
            width: 800,
            height: 600,
        };
        assert_eq!(
            window_crop(window, canvas),
            Some(Rect {
                x: 0,
                y: 0,
                width: 720,
                height: 520
            })
        );
        assert_eq!(
            window_crop(
                ScreenRect {
                    left: 5000,
                    ..window
                },
                canvas
            ),
            None
        );
    }

    #[test]
    fn foreground_without_its_own_active_window_and_focus_is_not_ready() {
        let own = HWND(7_usize as *mut _);
        let other = HWND(8_usize as *mut _);
        assert!(owns_capture_activation(own, own, own, own));
        assert!(!owns_capture_activation(own, other, own, own));
        assert!(!owns_capture_activation(own, own, other, own));
        assert!(!owns_capture_activation(own, own, own, other));
        assert!(!owns_capture_activation(
            HWND::default(),
            HWND::default(),
            HWND::default(),
            HWND::default()
        ));
    }

    #[test]
    fn native_focus_failure_can_continue_without_a_native_surface() {
        let error = crate::startup_surface::NativeSurfaceFailure::activation(
            crate::startup_surface::NativeStartupStage::Foreground,
            "快速首屏未能获得键盘焦点",
        );
        assert!(recover_native_surface(Err(error.into()), false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn native_fallback_preserves_cancellation_and_explicit_surface_validation() {
        let cancelled = crate::capture_standby::preparation_error(true, "已取消".into());
        let Err(error) = recover_native_surface(Err(cancelled), false) else {
            panic!("取消请求不得继续打开 GPUI");
        };
        assert!(crate::capture_standby::is_cancelled(error.as_ref()));
        let failure = crate::startup_surface::NativeSurfaceFailure::activation(
            crate::startup_surface::NativeStartupStage::DwmReady,
            "合成后焦点失效",
        );
        let Err(error) = recover_native_surface(Err(failure.into()), true) else {
            panic!("首屏专项不得借回退掩盖失败");
        };
        assert!(crate::startup_surface::is_activation_failure(
            error.as_ref()
        ));
    }

    #[test]
    fn initial_dxgi_cancellation_stays_typed_across_existing_preparation_transport() {
        let cancelled = initial_dxgi_error(crate::capture::cancellation::CaptureCancelled.into());
        assert!(crate::capture_standby::is_cancelled(cancelled.as_ref()));
        let transported = crate::capture_standby::preparation_error(
            crate::capture_standby::is_cancelled(cancelled.as_ref()),
            cancelled.to_string(),
        );
        assert!(crate::capture_standby::is_cancelled(transported.as_ref()));
        let native = initial_dxgi_error(
            windows::core::Error::from_hresult(
                windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED,
            )
            .into(),
        );
        assert!(!crate::capture_standby::is_cancelled(native.as_ref()));
        assert_eq!(
            native
                .downcast_ref::<windows::core::Error>()
                .unwrap()
                .code(),
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED
        );
        let matching_text = initial_dxgi_error("采集取消".into());
        assert!(!crate::capture_standby::is_cancelled(
            matching_text.as_ref()
        ));
    }
}

#[derive(Clone, Copy)]
enum Smoke {
    None,
    Full,
    VectorAa,
    Rotation,
    Startup,
    StartupSurface,
    SelectionFinish,
    Customization,
    Spotlight,
    RadiusHandle,
    Parameter,
}
fn uses_startup_surface(smoke: Smoke) -> bool {
    matches!(smoke, Smoke::None | Smoke::Startup | Smoke::StartupSurface)
        && std::env::var("RUSTSHOT_STARTUP_SURFACE").as_deref() != Ok("0")
}
pub fn run(full_screen: bool, all_screens: bool, smoke: bool) -> Result<()> {
    run_mode(
        full_screen,
        all_screens,
        if smoke { Smoke::Full } else { Smoke::None },
    )
}
pub fn run_quick(full_screen: bool, all_screens: bool, smoke: bool, quick: u32) -> Result<()> {
    run_mode_quick(
        full_screen,
        all_screens,
        if smoke { Smoke::Full } else { Smoke::None },
        quick,
    )
}
pub fn run_aa_smoke() -> Result<()> {
    run_mode(false, false, Smoke::VectorAa)
}
pub fn run_spotlight_smoke() -> Result<()> {
    std::env::set_var(
        "RUSTSHOT_DATA_DIR",
        format!("target/qa/spotlight/settings-{}", std::process::id()),
    );
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    run_mode(false, false, Smoke::Spotlight)
}
pub fn run_rotation_smoke() -> Result<()> {
    run_mode(false, false, Smoke::Rotation)
}
pub fn run_radius_handle_smoke() -> Result<()> {
    std::env::set_var(
        "RUSTSHOT_DATA_DIR",
        format!("target/qa/radius-handle/settings-{}", std::process::id()),
    );
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    run_mode(false, false, Smoke::RadiusHandle)
}
pub fn run_parameter_smoke() -> Result<()> {
    std::env::set_var(
        "RUSTSHOT_DATA_DIR",
        format!("target/qa/parameter/settings-{}", std::process::id()),
    );
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    run_mode(false, false, Smoke::Parameter)
}
pub fn run_selection_finish_smoke() -> Result<()> {
    std::env::set_var(
        "RUSTSHOT_DATA_DIR",
        format!("target/qa/selection-finish/settings-{}", std::process::id()),
    );
    run_mode(false, false, Smoke::SelectionFinish)
}
pub(crate) fn run_startup_probe() -> Result<()> {
    run_mode(false, false, Smoke::Startup)
}
pub(crate) fn run_startup_surface_smoke() -> Result<()> {
    if std::env::var("RUSTSHOT_SURFACE_QA_PREWARM").as_deref() == Ok("1") {
        let mut warm = crate::startup_surface::prewarm_standby()?;
        let identity = warm.identity.ok_or("原生首屏预热未完成")?;
        println!(
            "{}",
            serde_json::json!({
                "phase":"surface_qa_prewarmed", "hwnd":identity.hwnd,
                "token":identity.token, "ready_qpc":identity.ready_qpc,
                "physical_bounds":[identity.physical_bounds.left,identity.physical_bounds.top,
                    identity.physical_bounds.width as i64,identity.physical_bounds.height as i64],
                "cloaked":identity.cloaked,"window_visible":identity.window_visible
            })
        );
        warm.retain_for_capture();
        let result = run_startup_surface_smoke_cases();
        crate::startup_surface::discard_standby_warm();
        result
    } else {
        run_startup_surface_smoke_cases()
    }
}
fn run_startup_surface_smoke_cases() -> Result<()> {
    if std::env::var("RUSTSHOT_SURFACE_QA_CASE").as_deref() == Ok("ddb-readback") {
        crate::fast_capture::verify_pending_ddb_fixture()
    } else if std::env::var("RUSTSHOT_SURFACE_QA_CASE").as_deref() == Ok("native") {
        crate::startup_surface_smoke::native_run()
    } else {
        run_mode(false, false, Smoke::StartupSurface)?;
        crate::startup_surface_smoke::recycled()
    }
}
pub fn run_customization_smoke() -> Result<()> {
    std::env::set_var(
        "RUSTSHOT_DATA_DIR",
        format!("target/qa/customization/settings-{}", std::process::id()),
    );
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    run_mode(false, false, Smoke::Customization)
}
struct PreparedCapture {
    source: crate::canvas_source::CanvasSource,
    previews: Vec<Arc<RenderImage>>,
    cursor: Option<crate::cursor::CapturedCursor>,
    window_targets: Vec<crate::selection_assist::WindowTarget>,
    preview_ms: f64,
    surface: Option<crate::startup_surface::StartupSurface>,
    crop: Option<Rect>,
}

#[derive(Clone, Copy)]
struct QuickCapture {
    recipe: Option<QuickRecipe>,
    window: Option<ScreenRect>,
}
impl QuickCapture {
    fn new(code: u32) -> Result<Self> {
        let recipe = QuickRecipe::unpack(code)?;
        let window = if recipe.is_some_and(|recipe| recipe.source == Source::ActiveWindow) {
            active_window_bounds()
        } else {
            None
        };
        Ok(Self { recipe, window })
    }
    fn crop(
        self,
        bounds: ScreenRect,
        current: Rect,
        full_screen: bool,
        all_screens: bool,
    ) -> Option<Rect> {
        match self.recipe.map(|recipe| recipe.source) {
            Some(Source::Region) => None,
            Some(Source::LastRegion) => crate::last_region::load(bounds),
            Some(Source::CurrentScreen) => Some(current),
            Some(Source::AllScreens) => Some(Rect::full(bounds.width, bounds.height)),
            Some(Source::ActiveWindow) => {
                self.window.and_then(|window| window_crop(window, bounds))
            }
            None if all_screens => Some(Rect::full(bounds.width, bounds.height)),
            None if full_screen => Some(current),
            None => None,
        }
    }
}
// 在冻结和任何原生首屏激活之前采样目标，避免把截图遮罩误当作当前窗口。
fn active_window_bounds() -> Option<ScreenRect> {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_invalid() || IsIconic(window).as_bool() || !IsWindowVisible(window).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(window, &mut rect).is_err()
        {
            return None;
        }
        let width = u32::try_from(i64::from(rect.right) - i64::from(rect.left)).ok()?;
        let height = u32::try_from(i64::from(rect.bottom) - i64::from(rect.top)).ok()?;
        (width > 0 && height > 0).then_some(ScreenRect {
            left: rect.left,
            top: rect.top,
            width,
            height,
        })
    }
}
fn window_crop(window: ScreenRect, canvas: ScreenRect) -> Option<Rect> {
    let left = i64::from(window.left).max(i64::from(canvas.left));
    let top = i64::from(window.top).max(i64::from(canvas.top));
    let right = (i64::from(window.left) + i64::from(window.width))
        .min(i64::from(canvas.left) + i64::from(canvas.width));
    let bottom = (i64::from(window.top) + i64::from(window.height))
        .min(i64::from(canvas.top) + i64::from(canvas.height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect {
        x: u32::try_from(left - i64::from(canvas.left)).ok()?,
        y: u32::try_from(top - i64::from(canvas.top)).ok()?,
        width: u32::try_from(right - left).ok()?,
        height: u32::try_from(bottom - top).ok()?,
    })
}

fn join_preparation(
    task: preparation::PreparationTask<std::result::Result<PreparedCapture, (bool, String)>>,
) -> Result<PreparedCapture> {
    task.join()
        .map_err(|_| "屏幕冻结任务异常退出")?
        .map_err(|(cancelled, message)| {
            crate::capture_standby::preparation_error(cancelled, message)
        })
}

#[derive(Clone, Copy)]
struct ModePreludeTrace {
    enabled: bool,
    begin_qpc: i64,
    dpi_done_qpc: i64,
    mutex_done_qpc: i64,
    notify_done_qpc: i64,
    notify_via_controller: bool,
    prepare_enter_qpc: i64,
}
impl ModePreludeTrace {
    fn clock(&self) -> i64 {
        if self.enabled {
            crate::capture_standby::qpc()
        } else {
            0
        }
    }

    fn emit_after_ready(self) {
        if self.enabled {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase": "startup_mode_prelude", "pid": std::process::id(),
                "begin_qpc": self.begin_qpc, "dpi_done_qpc": self.dpi_done_qpc,
                "mutex_done_qpc": self.mutex_done_qpc,
                "notify_done_qpc": self.notify_done_qpc,
                "notify_via_controller": self.notify_via_controller,
                "notify_done_scope": if self.notify_via_controller {
                    "controller-command-enqueued-and-existing-kernel-wake-signalled"
                } else { "direct-cli-resident-search-and-post-attempt-completed" },
                "prepare_enter_qpc": self.prepare_enter_qpc,
                "emitted_after_actual_surface_ready": true
            }));
            crate::capture_standby::emit_request_trace_after_ready();
        }
    }
}
fn trace_stage(stage: &str, started: Instant) {
    if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
        crate::startup_probe::emit_trace_args(format_args!(
            "{{\"phase\":\"startup_stage\",\"stage\":\"{stage}\",\"duration_ms\":{:.3}}}",
            started.elapsed().as_secs_f64() * 1000.
        ));
    }
}
/// 后台冻结与主线程 GPUI 初始化重叠；准备完毕前不创建截图覆盖窗。
/// immediate context 只由本任务使用，join 后才将冻结纹理交给 UI，避免跨线程并发。
/// 准备任务的跨线程取消传输只识别既有 StandbyCancelled，必须在字符串化前桥接。
/// 只转换明确 typed 取消；同遇设备/租约失败不靠文本匹配消音。
fn initial_dxgi_error(error: Box<dyn std::error::Error>) -> Box<dyn std::error::Error> {
    if error.is::<crate::capture::cancellation::CaptureCancelled>() {
        crate::capture_standby::preparation_error(true, error.to_string())
    } else {
        error
    }
}

fn prepare_dxgi(
    cancel_token: &crate::capture_lock::NativeCaptureCancelToken,
) -> Result<(
    crate::canvas_source::CanvasSource,
    Vec<Arc<RenderImage>>,
    f64,
)> {
    prepare_dxgi_cancelled(&|| !cancel_token.session_is_live()).map_err(initial_dxgi_error)
}

/// 初始化与显式刷新共用同次冻结纹理；取消观察覆盖每块屏幕及 CPU 读回。
pub(crate) fn prepare_dxgi_cancelled(
    cancelled: &dyn Fn() -> bool,
) -> Result<(
    crate::canvas_source::CanvasSource,
    Vec<Arc<RenderImage>>,
    f64,
)> {
    let capture_started = Instant::now();
    let desktop = DesktopCapture::capture_initial(cancelled)?;
    trace_stage("desktop_freeze", capture_started);
    let bounds = desktop.bounds;
    if bounds.width > 16384 || bounds.height > 16384 {
        return Err(
            tr!("虚拟桌面单边超过 16384 像素；当前 GPUI 单窗口版本无法覆盖，请调整屏幕布局", "The virtual desktop exceeds 16384 pixels on one side, which a single GPUI window can't cover; adjust the display layout").into(),
        );
    }
    let mut previews = Vec::new();
    let readback_started = Instant::now();
    for screen in &desktop.screens {
        let image =
            screen.read_region_cancellable(Rect::full(screen.width, screen.height), cancelled)?;
        // GPUI RenderImage 使用 BGRA。冻结像素也供马赛克采样，不另外复制整屏 CPU 图像。
        let buffer = image::RgbaImage::from_raw(image.width, image.height, image.bgra)
            .ok_or("预览尺寸无效")?;
        previews.push(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])));
    }
    let preview_ms = readback_started.elapsed().as_secs_f64() * 1000.;
    trace_stage("preview_readback", readback_started);
    if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
        crate::startup_probe::emit_trace_args(format_args!(
            "{{\"phase\":\"startup_backend\",\"backend\":\"dxgi\"}}"
        ));
    }
    Ok((
        crate::canvas_source::CanvasSource::desktop(desktop),
        previews,
        preview_ms,
    ))
}
fn prepare_capture(
    full_screen: bool,
    all_screens: bool,
    smoke: Smoke,
    quick: QuickCapture,
    started: Instant,
    mut prelude: ModePreludeTrace,
    cancel_token: crate::capture_lock::NativeCaptureCancelToken,
    mut ready_gate: Option<&mut preparation::ReadyGate<crate::startup_surface::StartupSurface>>,
) -> Result<PreparedCapture> {
    prelude.prepare_enter_qpc = prelude.clock();
    // 首屏先消费不可变原生冻结图，CPU 预览在原生 Ready 后从同次冻结源读回。
    // 不创建另一套应用 D3D 设备，也不从已带选区的窗口或桌面重新采集。
    let fixture = matches!(smoke, Smoke::StartupSurface);
    let early = uses_startup_surface(smoke);
    // 本次的系统滚轮设置与屏幕冻结并行；ticket 在失败/取消时也会等待查询退出。
    let mut pending_wheel =
        (early && !fixture).then(crate::startup_surface::PendingWheelSettings::begin);
    // 指针查询与独占图标复制一起并行采样；在首屏改变光标之前回收复制线程。
    let cursor_started = Instant::now();
    let pending_cursor = if fixture {
        crate::cursor::PendingCursorSnapshot::empty()
    } else if let Some(ticket) = crate::capture_standby::take_cursor_task()? {
        crate::cursor::PendingCursorSnapshot::begin_precreated(ticket)
    } else {
        crate::cursor::PendingCursorSnapshot::begin()
    };
    trace_stage("cursor_snapshot_begin", cursor_started);
    // 仅全链 Trace 的真实首屏诊断可把本次复制 join 提前；常规路径保持并行。
    let cursor_serial = early
        && !fixture
        && std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1")
        && std::env::var("RUSTSHOT_STARTUP_CURSOR_SERIAL").as_deref() == Ok("1");
    let (pending_cursor, serial_cursor_result) = if cursor_serial {
        let cursor_join_started = Instant::now();
        let cursor_trace = pending_cursor.trace_ticket();
        let cursor_snapshot = pending_cursor.finish();
        trace_stage("cursor_snapshot_join_before_capture", cursor_join_started);
        (None, Some((cursor_trace, cursor_snapshot)))
    } else {
        (Some(pending_cursor), None)
    };
    let mut startup_bitmap = None;
    let mut deferred_surface = None;
    let mut deferred_targets = None;
    let mut deferred_attempted = false;
    let mut deferred_dxgi_fallback = false;
    let pending = if early && fixture {
        Some(crate::fast_capture::PendingStartupCapture::from_fixture(
            crate::startup_surface_smoke::fixture_desktop()?,
        )?)
    } else if early && std::env::var("RUSTSHOT_CAPTURE_BACKEND").as_deref() != Ok("dxgi") {
        match crate::fast_capture::PendingStartupCapture::capture() {
            Ok(pending) => pending,
            Err(error) if crate::fast_capture::can_fallback(error.as_ref()) => {
                // 只有尚未显示首屏的原生采集 API 失败可回退；后续读回失败不能重截。
                deferred_dxgi_fallback = true;
                if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
                    eprintln!("原生首屏冻结失败，回退 DXGI：{error}");
                }
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let (cursor_trace, cursor_snapshot) = if let Some(result) = serial_cursor_result {
        result
    } else {
        let pending_cursor = pending_cursor.ok_or("并行光标任务缺失")?;
        let cursor_join_started = Instant::now();
        let cursor_trace = pending_cursor.trace_ticket();
        let cursor_snapshot = pending_cursor.finish();
        trace_stage("cursor_snapshot_join_before_native", cursor_join_started);
        (cursor_trace, cursor_snapshot)
    };
    let mut deferred_crop = None;
    let (source, previews, preview_ms) = if let Some(pending) = pending {
        deferred_attempted = true;
        let (frozen, surface, targets, crop) = prepare_pending_native(
            pending,
            full_screen,
            all_screens,
            quick,
            fixture,
            started,
            pending_wheel.take(),
            cancel_token.clone(),
            ready_gate.as_deref_mut(),
        )?;
        deferred_surface = surface;
        deferred_targets = Some(targets);
        deferred_crop = Some(crop);
        let (source, previews) = crate::canvas_source::CanvasSource::frozen_desktop(frozen);
        if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
            crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_backend\",\"backend\":\"gdi\",\"native_freeze\":\"ddb-deferred-readback\"}}"));
        }
        (source, previews, 0.)
    } else if fixture {
        let frozen = crate::startup_surface_smoke::fixture_desktop()?;
        let pixels = frozen.screens[0]
            .preview
            .as_bytes(0)
            .ok_or("自有冻结首屏像素已失效")?;
        startup_bitmap = Some(Arc::new(crate::startup_bitmap::StartupBitmap::from_pixels(
            frozen.bounds,
            vec![Rect::full(frozen.bounds.width, frozen.bounds.height)],
            pixels,
        )?));
        let (source, previews) = crate::canvas_source::CanvasSource::frozen_desktop(frozen);
        (source, previews, 0.)
    } else if deferred_dxgi_fallback
        || std::env::var("RUSTSHOT_CAPTURE_BACKEND").as_deref() == Ok("dxgi")
    {
        prepare_dxgi(&cancel_token)?
    } else {
        let capture_started = Instant::now();
        let frozen = if early {
            crate::fast_capture::FrozenDesktop::capture_for_startup()
        } else {
            crate::fast_capture::FrozenDesktop::capture().map(|desktop| (desktop, None))
        };
        match frozen {
            Ok((frozen, bitmap)) => {
                startup_bitmap = bitmap;
                trace_stage("gdi_freeze", capture_started);
                let (source, previews) = crate::canvas_source::CanvasSource::frozen_desktop(frozen);
                if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
                    crate::startup_probe::emit_trace_args(format_args!(
                        "{{\"phase\":\"startup_backend\",\"backend\":\"gdi\"}}"
                    ));
                }
                (source, previews, 0.)
            }
            Err(error) if crate::fast_capture::can_fallback(error.as_ref()) => {
                if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
                    eprintln!("静态采集失败，回退 DXGI：{error}");
                }
                prepare_dxgi(&cancel_token)?
            }
            Err(error) => return Err(error),
        }
    };
    let bounds = source.bounds;
    let targets_started = Instant::now();
    let window_targets = if let Some(targets) = deferred_targets {
        targets
    } else if fixture {
        Vec::new()
    } else {
        crate::selection_assist::windows(bounds)
    };
    trace_stage("window_targets", targets_started);
    let crop = deferred_crop.unwrap_or_else(|| {
        quick.crop(
            bounds,
            source.screen_rect(source.current),
            full_screen,
            all_screens,
        )
    });
    let surface = if deferred_attempted {
        deferred_surface
    } else if early {
        let planes = previews
            .iter()
            .enumerate()
            .map(|(i, image)| (source.screen_rect(i), image.clone()))
            .collect();
        let surface = recover_native_surface(
            crate::startup_surface::StartupSurface::start_with_bitmap_and_wheel(
                bounds,
                planes,
                source.current,
                crop,
                window_targets.clone(),
                started,
                startup_bitmap,
                pending_wheel.take(),
                Some(cancel_token),
            ),
            fixture,
        )?;
        if let Some(surface) = surface.as_ref() {
            if fixture {
                crate::startup_surface_smoke::prepare(surface)?;
            }
            if let Some(gate) = ready_gate.as_mut() {
                gate.release(surface.clone());
            }
        }
        surface
    } else {
        None
    };
    if surface.is_some() {
        // 前置 API 的真实时间戳只在首屏成功冻结、绘制并取得输入焦点后输出。
        prelude.emit_after_ready();
        if let Some(trace) = cursor_trace.as_ref() {
            trace.emit_after_ready();
        }
    }
    let cursor_started = Instant::now();
    let cursor = cursor_snapshot.as_ref().and_then(|snapshot| {
        crate::cursor::compose(
            snapshot,
            &crate::scene::SourcePixels {
                fill: None,
                canvas: source.full(),
                planes: previews
                    .iter()
                    .enumerate()
                    .map(|(i, image)| crate::scene::PixelPlane {
                        rect: source.screen_rect(i),
                        bgra: image.as_bytes(0).expect("frozen pixels"),
                    })
                    .collect(),
                overlay: None,
            },
            source.bounds,
        )
        .unwrap_or(None)
    });
    trace_stage("cursor_compose_after_ready", cursor_started);
    Ok(PreparedCapture {
        source,
        previews,
        cursor,
        window_targets,
        preview_ms,
        surface,
        crop,
    })
}

#[allow(clippy::too_many_arguments)]
fn prepare_pending_native(
    pending: crate::fast_capture::PendingStartupCapture,
    full_screen: bool,
    all_screens: bool,
    quick: QuickCapture,
    fixture: bool,
    started: Instant,
    pending_wheel: Option<crate::startup_surface::PendingWheelSettings>,
    cancel_token: crate::capture_lock::NativeCaptureCancelToken,
    ready_gate: Option<&mut preparation::ReadyGate<crate::startup_surface::StartupSurface>>,
) -> Result<(
    crate::fast_capture::FrozenDesktop,
    Option<crate::startup_surface::StartupSurface>,
    Vec<crate::selection_assist::WindowTarget>,
    Option<Rect>,
)> {
    let bounds = pending.bounds();
    let current = pending.current();
    let rects = pending.local_rects().to_vec();
    let monitors = pending.monitor_rects().to_vec();
    let targets_started = Instant::now();
    let targets = if fixture {
        Vec::new()
    } else {
        crate::selection_assist::windows(bounds)
    };
    trace_stage("window_targets_before_native", targets_started);
    let crop = quick.crop(
        bounds,
        *rects.get(current).ok_or("原生冻结当前屏幕无效")?,
        full_screen,
        all_screens,
    );
    let native_start = crate::startup_surface::StartupSurface::start_native_with_wheel(
        bounds,
        rects,
        current,
        crop,
        targets.clone(),
        started,
        pending.native_bitmap(),
        pending_wheel,
        Some(cancel_token),
    );
    let surface = recover_native_surface(native_start, fixture)?;
    if let Some(surface) = surface.as_ref() {
        if fixture {
            crate::startup_surface_smoke::prepare(surface)?;
        }
        if let Some(gate) = ready_gate {
            gate.release(surface.clone());
        }
    }
    // 原生拥有独立缓存位图和消息线程；两线程不共享可选入的 GDI 对象。
    // 失败自动回收首屏和原图，不重新截图，避免编辑/导出换成另一时刻的桌面。
    let readback_started = Instant::now();
    let frozen = pending.finish()?;
    trace_stage("gdi_cpu_prepare_after_native", readback_started);
    if frozen.bounds != bounds
        || frozen.current != current
        || frozen.screens.len() != monitors.len()
        || frozen
            .screens
            .iter()
            .zip(monitors)
            .any(|(screen, rect)| screen.rect != rect)
    {
        return Err("原生首屏与同源 CPU 冻结图的物理布局不符".into());
    }
    Ok((frozen, surface, targets, crop))
}
fn run_mode(full_screen: bool, all_screens: bool, smoke: Smoke) -> Result<()> {
    run_mode_quick(full_screen, all_screens, smoke, 0)
}
fn run_mode_quick(full_screen: bool, all_screens: bool, smoke: Smoke, quick: u32) -> Result<()> {
    let started = Instant::now();
    let begin_qpc = crate::capture_standby::qpc();
    let mut prelude = ModePreludeTrace {
        enabled: std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some(),
        begin_qpc,
        dpi_done_qpc: 0,
        mutex_done_qpc: 0,
        notify_done_qpc: 0,
        notify_via_controller: false,
        prepare_enter_qpc: 0,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    prelude.dpi_done_qpc = prelude.clock();
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\RustShot.MVP.Capture"))? };
    // 查询计时器可能改变 LastError，先保存 mutex 的原始结果。
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    prelude.mutex_done_qpc = prelude.clock();
    let _lock = CaptureLock::new(mutex);
    if already_exists {
        return if !matches!(smoke, Smoke::None) {
            Err("已有截图会话，无法运行界面测试".into())
        } else {
            Ok(())
        };
    }
    prelude.notify_via_controller = crate::capture_standby::report_started_via_controller()?;
    if !prelude.notify_via_controller {
        crate::ui::report_capture_started();
    }
    prelude.notify_done_qpc = prelude.clock();
    let quick = QuickCapture::new(quick)?;
    let cancel_token = _lock.native_cancel_token();
    // 先让框选可用，再将同源 CPU 读回与主 STA 的驱动/字体初始化重叠。
    // 原生线程继续收输入，完整冻结结果回收后才开 GPUI 窗；显式值保留旧专项对照。
    let startup_serial = std::env::var("RUSTSHOT_STARTUP_SERIAL");
    let ready_gated =
        uses_startup_surface(smoke) && !matches!(startup_serial.as_deref(), Ok("0" | "1"));
    let serial = match startup_serial.as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => uses_startup_surface(smoke),
    };
    let mut native_preparing = None;
    let (ready, mut preparation) = if ready_gated {
        let foreground_gate = crate::capture_standby::preparation_gate()?;
        let device_tasks = crate::fast_capture::take_prewarmed_device_tasks();
        let (surface, task) = preparation::spawn(move |mut ready_gate| {
            crate::fast_capture::install_prewarmed_device_tasks(device_tasks);
            let result = (|| {
                crate::capture_standby::install_preparation_gate(foreground_gate)?;
                prepare_capture(
                    full_screen,
                    all_screens,
                    smoke,
                    quick,
                    started,
                    prelude,
                    cancel_token,
                    Some(&mut ready_gate),
                )
            })();
            result.map_err(|error| {
                (
                    crate::capture_standby::is_cancelled(error.as_ref()),
                    error.to_string(),
                )
            })
        })?;
        let wait_started = Instant::now();
        let surface = surface.recv().ok();
        trace_stage("gpui_initialization_gate_wait", wait_started);
        if let Some(surface) = surface {
            native_preparing = Some(surface);
            (None, Some(task))
        } else {
            // 原生失败/回退时只接受完整准备结果；真实失败不创建 GPUI。
            (Some(join_preparation(task)?), None)
        }
    } else if serial {
        (
            Some(prepare_capture(
                full_screen,
                all_screens,
                smoke,
                quick,
                started,
                prelude,
                cancel_token,
                None,
            )?),
            None,
        )
    } else {
        let foreground_gate = crate::capture_standby::preparation_gate()?;
        (
            None,
            Some(
                preparation::spawn(move |_: preparation::ReadyGate<()>| {
                    let result = (|| {
                        crate::capture_standby::install_preparation_gate(foreground_gate)?;
                        prepare_capture(
                            full_screen,
                            all_screens,
                            smoke,
                            quick,
                            started,
                            prelude,
                            cancel_token,
                            None,
                        )
                    })();
                    result.map_err(|error| {
                        (
                            crate::capture_standby::is_cancelled(error.as_ref()),
                            error.to_string(),
                        )
                    })
                })?
                .1,
            ),
        )
    };
    if let Some(surface) = native_preparing.as_ref().or_else(|| {
        ready
            .as_ref()
            .and_then(|prepared| prepared.surface.as_ref())
    }) {
        if surface.snapshot().cancelled {
            if let Some(task) = preparation.take() {
                match join_preparation(task) {
                    Ok(_) => {}
                    Err(error) if crate::capture_standby::is_cancelled(error.as_ref()) => {}
                    Err(error) => return Err(error),
                }
            }
            surface.dismiss()?;
            return Ok(());
        }
    }
    // 未启用原生首屏和显式并行准备也必须完成同次前台授权，并在 App 创建前查 stop。
    // 失败时仍 join 已开始的冻结任务，不能留下持有同源像素/原生窗口的分离线程。
    if let Err(error) = crate::capture_standby::wait_foreground_grant_before_native()
        .and_then(|_| crate::capture_standby::ensure_standby_owner_active())
    {
        if let Some(preparation) = preparation.take() {
            let prepared = preparation.join().map_err(|_| "屏幕冻结任务异常退出")?;
            match prepared {
                Err((false, message)) => return Err(message.into()),
                Ok(prepared) => {
                    if let Some(surface) = prepared.surface {
                        surface.dismiss()?;
                    }
                }
                Err((true, _)) => {}
            }
        }
        return Err(error);
    }
    let application_started = Instant::now();
    let application = Application::new().with_assets(crate::toolbar_icons::IconAssets);
    trace_stage("gpui_initialize", application_started);
    let prepared = if let Some(ready) = ready {
        ready
    } else {
        join_preparation(preparation.take().expect("capture preparation task"))?
    };
    drop(native_preparing);
    let PreparedCapture {
        source,
        previews,
        cursor,
        window_targets,
        preview_ms,
        surface,
        crop,
    } = prepared;
    if surface
        .as_ref()
        .is_some_and(|surface| surface.snapshot().cancelled)
    {
        if let Some(surface) = surface {
            surface.dismiss()?;
        }
        return Ok(());
    }
    // 任何取消、窗口创建失败或绘制异常都由这个外层观察者回收临时原生首屏。
    let surface_owner = surface.clone();
    let bounds = source.bounds;
    // 普通入口冻结后再读取完成动作，原始采集范围和先编辑语义保持一致。
    let session_recipe = quick.recipe.or_else(|| {
        matches!(smoke, Smoke::None).then(|| {
            let mut recipe = crate::quick_action_store::QuickActionStore::load()
                .resolve("capture")
                .unwrap_or_default();
            recipe.edit = true;
            recipe.source = if all_screens {
                Source::AllScreens
            } else if full_screen {
                Source::CurrentScreen
            } else {
                Source::Region
            };
            recipe
        })
    });
    trace_stage("prepared_total", started);
    let errors = Rc::new(RefCell::new(None::<String>));
    let final_errors = errors.clone();
    crate::capture_standby::ensure_standby_owner_active()?;
    application.run(move |cx: &mut App| {
        // 与原生终态取消线性化；旧一轮即使仍在 App 初始化也不能迟到开窗。
        // GPUI 先赢则锁一直保留到旧 GPUI 窗口取消，避免两个截图窗口并存。
        if !_lock.claim_gpui_open() {
            cx.quit();
            return;
        }
        crate::theme::install(cx);
        let activate_immediately = surface.is_none();
        let errors_in_view = errors.clone();
        let window_started = Instant::now();
        // GPUI 接受 DIP；避免先把整个物理桌面当作 DIP 分配，再改回物理全屏。
        // 显式选显示器内的小初建窗口，最终范围仍由下方原生定位统一校正。
        let legacy_bounds = std::env::var("RUSTSHOT_GPUI_LEGACY_BOUNDS").as_deref() == Ok("1");
        let display = cx.primary_display();
        let initial_bounds = if legacy_bounds {
            Some(Bounds::new(
                point(px(bounds.left as f32), px(bounds.top as f32)),
                size(px(bounds.width as f32), px(bounds.height as f32)),
            ))
        } else {
            display.as_ref().and_then(|display| crate::startup_bounds::initial_bounds(display.bounds()))
        };
        let result = cx.open_window(
            WindowOptions {
                window_bounds: initial_bounds.map(WindowBounds::Windowed),
                display_id: if legacy_bounds { None } else { display.as_ref().map(|display| display.id()) },
                // 标准标题栏配合 WS_POPUP，避免 GPUI 自定义客户区内边距影响物理坐标。
                titlebar: Some(TitlebarOptions {
                    appears_transparent: false,
                    ..Default::default()
                }),
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                focus: true,
                show: surface.is_none(),
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(concat!(crate::brand_name!(), " · GPUI"));
                let native = hwnd(window);
                let framebuffer_qa = matches!(smoke, Smoke::Full)
                    && std::env::var("RUSTSHOT_SMOKE_CASE").as_deref() == Ok("highlighter")
                    && std::env::var("RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER").as_deref() == Ok("1");
                unsafe {
                    if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
                        let mut initial = RECT::default();
                        if GetWindowRect(native, &mut initial).is_ok() {
                            crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_window_geometry\",\"legacy\":{legacy_bounds},\"initial_width\":{},\"initial_height\":{},\"final_width\":{},\"final_height\":{}}}", initial.right-initial.left, initial.bottom-initial.top, bounds.width, bounds.height));
                        }
                    }
                    let native_started = Instant::now();
                    SetWindowLongPtrW(native, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                    trace_stage("capture_native_style", native_started);
                    let placement_started = Instant::now();
                    let placed = SetWindowPos(
                        native,
                        Some(surface.as_ref().map_or(HWND_TOPMOST, |surface| surface.handle())),
                        bounds.left,
                        bounds.top,
                        bounds.width as i32,
                        bounds.height as i32,
                        SWP_SHOWWINDOW | SWP_FRAMECHANGED | if surface.is_some() || framebuffer_qa { SWP_NOACTIVATE } else { SET_WINDOW_POS_FLAGS(0) },
                    );
                    trace_stage("capture_native_placement", placement_started);
                    let foreground_started = Instant::now();
                    if surface.is_none() && !framebuffer_qa { let _ = SetForegroundWindow(native); }
                    trace_stage("capture_native_foreground", foreground_started);
                    // 工厂仍持有 App 借用，同步 WM_SIZE 的 GPUI 回调此时无法更新 viewport。
                    // 完成原生定位后排队重放最终尺寸，在外层消息循环里同步缓存与实际客户区。
                    let queued = PostMessageW(
                        Some(native), WM_SIZE, WPARAM(SIZE_RESTORED as usize),
                        LPARAM(((bounds.height << 16) | bounds.width) as isize),
                    );
                    if let Err(error) = placed.and(queued) {
                        *errors_in_view.borrow_mut() = Some(format!("截图窗口定位失败：{error}"));
                        cx.quit();
                    }
                }
                let focus_started = Instant::now();
                let focus = cx.focus_handle();
                window.focus(&focus);
                trace_stage("capture_focus", focus_started);
                let view_started = Instant::now();
                let view = cx.new(|_| {
                    let mut view = CaptureView::new(
                        source,
                        previews,
                        focus,
                        crop,
                        Some(_lock),
                        cursor,
                        window_targets,
                    );
                    if let Some(recipe) = session_recipe {
                        view.install_quick_action(recipe);
                        if crop.is_none() {
                            match recipe.source {
                                Source::LastRegion => view.status = tr!("上次区域不可用，请重新框选", "Last region unavailable; select a new region").into(),
                                Source::ActiveWindow => view.status = tr!("当前窗口不可用，请重新框选", "Window unavailable; select a region").into(),
                                _ => {}
                            }
                        }
                    }
                    view
                });
                let weak = view.downgrade();
                view.update(cx, |_, cx| {
                    cx.observe_window_activation(window, |view, window, cx| {
                        let native = hwnd(window);
                        if !view.cleanup_inactive_screenshot(
                            !window.is_window_active(),
                            unsafe { GetForegroundWindow() } == native,
                        ) {
                            return;
                        }
                        cx.notify();
                        let weak = cx.weak_entity();
                        // ReleaseCapture 可以同步回调；必须退出 Entity 借用后再执行。
                        window.defer(cx, move |window, cx| {
                            let release_ready = weak
                                .update(cx, |view, _| view.inactive_screenshot_release_ready())
                                .unwrap_or(false);
                            let native = hwnd(window);
                            if release_ready
                                && !window.is_window_active()
                                && unsafe { GetForegroundWindow() } != native
                                && unsafe { GetCapture() } == native
                            {
                                unsafe {
                                    let _ = ReleaseCapture();
                                }
                            }
                        });
                    })
                    .detach();
                });
                if surface.is_none() {
                    if matches!(smoke, Smoke::None) {
                        let activation_view = view.downgrade();
                        let activation_errors = errors_in_view.clone();
                        let original_native = native.0 as usize;
                        let mut original_pid = 0;
                        let original_tid = unsafe {
                            GetWindowThreadProcessId(native, Some(&mut original_pid))
                        };
                        // 仅实际用户截图无原生首屏的显示路径。等工厂/App 借用结束后
                        // 只读实际输入归属，不再次激活或注入消息，不把 GPUI focus 缓存当成功。
                        window.on_next_frame(move |window, cx| {
                            let target = hwnd(window);
                            let mut pid = 0;
                            let tid = unsafe { GetWindowThreadProcessId(target, Some(&mut pid)) };
                            if target.0 as usize != original_native
                                || pid != original_pid
                                || pid != std::process::id()
                                || tid != original_tid
                                || !unsafe { IsWindow(Some(target)) }.as_bool()
                            {
                                let message = "GPUI 截图显示时原窗口身份已失效".to_owned();
                                crate::capture_failure::record("gpui_activation", &message);
                                *activation_errors.borrow_mut() = Some(message.clone());
                                let _ = activation_view.update(cx, |view, cx| {
                                    // 仅撤本视图的 Rust 状态和 mutex；不调用会无条件
                                    // ReleaseCapture 的 cancel_pointer/perform，也不退出其它贴图。
                                    view.capture_lock.take();
                                    view.suspend_control_tracking();
                                    view.cleanup_inactive_screenshot(true, false);
                                    view.clear_pointer_tracking();
                                    view.control_selection_ready = false;
                                    view.input.marked = None;
                                    view.status = message;
                                    cx.notify();
                                });
                                // 不对身份已变的 HWND 执行原生焦点或关闭操作。
                                return;
                            }
                            let mut gui = GUITHREADINFO {
                                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                                ..Default::default()
                            };
                            let result = unsafe { GetGUIThreadInfo(tid, &mut gui) };
                            let failure = match result {
                                Err(error) => Some(format!("GPUI 截图活动窗口/焦点读回失败：{error}")),
                                Ok(()) if !owns_capture_activation(
                                    target,
                                    unsafe { GetForegroundWindow() },
                                    gui.hwndActive,
                                    gui.hwndFocus,
                                ) => Some("GPUI 截图窗口未取得实际前台/活动窗口/键盘焦点".into()),
                                Ok(()) => None,
                            };
                            if let Some(message) = failure {
                                crate::capture_failure::record("gpui_activation", &message);
                                *activation_errors.borrow_mut() = Some(message);
                                let _ = activation_view.update(cx, |view, cx| {
                                    view.capture_lock.take();
                                    view.suspend_control_tracking();
                                    view.cleanup_inactive_screenshot(true, false);
                                    view.clear_pointer_tracking();
                                    view.control_selection_ready = false;
                                    view.input.marked = None;
                                    cx.notify();
                                });
                                // 原 HWND/PID/TID 已核验。直接移除本 GPUI 窗口，
                                // 不受 saving/草稿/QA 操作守卫影响，不调用无条件 ReleaseCapture。
                                window.remove_window();
                                return;
                            }
                            let _ = activation_view.update(cx, |view, cx| {
                                view.control_selection_ready = true;
                                view.request_control_probe(cx);
                                cx.notify();
                            });
                        });
                    } else {
                        view.update(cx, |view, _| view.control_selection_ready = true);
                    }
                }
                trace_stage("view_initialize", view_started);
                window.on_window_should_close(cx, move |_, cx| {
                    weak.update(cx, |v, cx| {
                        if v.saving { return false; }
                        if !v.finish_content_or_status() { cx.notify(); return false; }
                        v.flush_tool_presets();
                        true
                    }).unwrap_or(true)
                });
                if let Some(surface) = surface {
                    if matches!(smoke, Smoke::StartupSurface) {
                        crate::startup_surface_smoke::schedule(&view, window, cx, surface.clone(), errors_in_view.clone());
                    }
                    crate::startup_handoff::schedule(&view, window, cx, surface, started, matches!(smoke, Smoke::Startup), errors_in_view);
                } else if matches!(smoke, Smoke::Startup) {
                    crate::startup_probe::schedule(&view, window, cx, started, errors_in_view);
                } else if matches!(smoke, Smoke::Customization) {
                    crate::customization_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::SelectionFinish) {
                    crate::selection_finish_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::Rotation) {
                    crate::rotation_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::Spotlight) {
                    crate::spotlight_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::RadiusHandle) {
                    crate::radius_handle_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::Parameter) {
                    crate::parameter_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::VectorAa) {
                    crate::vector_aa_smoke::schedule(&view, window, cx, errors_in_view);
                } else if matches!(smoke, Smoke::Full) {
                    gpui_smoke::schedule(&view, window, cx, started, preview_ms, errors_in_view);
                }
                view
            },
        );
        trace_stage("window_create", window_started);
        if let Err(e) = result {
            *errors.borrow_mut() = Some(e.to_string());
            cx.quit();
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() && !crate::pin_workspace_runtime::is_restoring(cx) && !crate::pin_live_view::has_open_windows() {
                cx.quit();
            }
        })
        .detach();
        if activate_immediately { cx.activate(true); }
    });
    if let Some(surface) = surface_owner {
        surface.dismiss()?;
    }
    let error = final_errors.borrow_mut().take();
    if let Some(e) = error {
        Err(e.into())
    } else {
        Ok(())
    }
}
