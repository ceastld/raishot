//! GPU 初始化期间的冻结首屏。窗口、目标 DC 和手势由同一个原生线程独占。
//! 反选后的不可变源位图可跨线程持有；绘制仅串行借用本地源 DC。
use crate::{
    capture_double_click::ClickSeed,
    editor::Editor,
    geometry::{Rect, ScreenRect},
    interaction::WheelAccumulator,
    selection_assist::WindowTarget,
    selection_chrome::{PixelBox, SelectionChrome},
    startup_bitmap::StartupBitmap,
    startup_output::{StartupOutput, StartupOutputKind, StartupOutputQueue},
    startup_preview::{self, PreviewPresentation},
    startup_surface_input::{self, CapturedInput, ClickGesture, RawInput},
    Result,
};
use gpui::RenderImage;
use std::{
    collections::VecDeque,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            SystemServices::{MK_LBUTTON, MK_RBUTTON},
            Threading::{
                AttachThreadInput, CreateEventW, GetCurrentProcess, GetCurrentThreadId,
                GetGuiResources, SetEvent, GR_GDIOBJECTS,
            },
        },
        UI::{
            HiDpi::{
                GetDpiForWindow, SetThreadDpiAwarenessContext,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
            Input::{
                Ime::ImmDisableIME,
                KeyboardAndMouse::{
                    EnableWindow, GetAsyncKeyState, GetCapture, GetDoubleClickTime, GetFocus,
                    IsWindowEnabled, ReleaseCapture, SetActiveWindow, SetCapture, SetFocus,
                    VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
                },
            },
            WindowsAndMessaging::*,
        },
    },
};

const WAKE: u32 = WM_APP + 0x4b;
const EVENT_LIMIT: usize = 128;
const KEY_LIMIT: usize = 32;
const QA_MOVE: usize = 1 << 24;
const QA_CONTROL: usize = 1 << 25;
const WAIT: Duration = Duration::from_secs(3);
/// 与 GPUI 截图暗层 `rgba(0x00000077)` 相同的原生遮罩不透明度。
const MASK_ALPHA: u8 = 0x77;

/// 原生遮罩下单个颜色通道的期望值（黑色以 `MASK_ALPHA` 覆盖），供 QA 允许 ±1 舍入。
pub(crate) fn masked_channel(value: u8) -> u8 {
    ((value as u32 * (255 - MASK_ALPHA as u32) + 127) / 255) as u8
}
type Input = RawInput;

/// 仅在内存中标记当前阶段，超时诊断不读取截图或窗口内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum NativeStartupStage {
    Queued,
    Creating,
    Preparing,
    Painting,
    Enabling,
    Uncloaking,
    Foreground,
    DwmReady,
    Ready,
}
impl NativeStartupStage {
    fn load(value: &AtomicU32) -> Self {
        match value.load(Ordering::Acquire) {
            1 => Self::Creating,
            2 => Self::Preparing,
            3 => Self::Painting,
            4 => Self::Enabling,
            5 => Self::Uncloaking,
            6 => Self::Foreground,
            7 => Self::DwmReady,
            8 => Self::Ready,
            _ => Self::Queued,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeSurfaceFailureKind {
    ActivationFailed,
    ReadyTimeout,
    Other,
}

/// 跨原生 ready 通道保留分类，不能先转 String 后让调用方猜测错误文本。
#[derive(Clone, Debug)]
pub(crate) struct NativeSurfaceFailure {
    pub(crate) kind: NativeSurfaceFailureKind,
    pub(crate) stage: NativeStartupStage,
    message: String,
}
impl NativeSurfaceFailure {
    pub(crate) fn activation(stage: NativeStartupStage, message: impl Into<String>) -> Self {
        Self {
            kind: NativeSurfaceFailureKind::ActivationFailed,
            stage,
            message: message.into(),
        }
    }
    fn from_error(error: &(dyn std::error::Error + 'static), stage: NativeStartupStage) -> Self {
        error
            .downcast_ref::<Self>()
            .cloned()
            .unwrap_or_else(|| Self {
                kind: NativeSurfaceFailureKind::Other,
                stage,
                message: error.to_string(),
            })
    }
    fn timeout(stage: NativeStartupStage) -> Self {
        Self {
            kind: NativeSurfaceFailureKind::ReadyTimeout,
            stage,
            message: "快速首屏未能及时创建；仍按创建线程规则回收，回收耗时不受 Ready 等待限制"
                .into(),
        }
    }
    fn disconnected(stage: NativeStartupStage, published: Option<Self>) -> Self {
        published.unwrap_or_else(|| Self {
            kind: NativeSurfaceFailureKind::Other,
            stage,
            message: "快速首屏 ready 通道已关闭".into(),
        })
    }
}
impl std::fmt::Display for NativeSurfaceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} [native={:?}, stage={:?}]",
            self.message, self.kind, self.stage
        )
    }
}
impl std::error::Error for NativeSurfaceFailure {}

pub(crate) fn is_activation_failure(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<NativeSurfaceFailure>()
        .is_some_and(|failure| failure.kind == NativeSurfaceFailureKind::ActivationFailed)
}

fn report_activation_failure(failure: NativeSurfaceFailure) -> Box<dyn std::error::Error> {
    // 在错误导致原生 RAII 回收之前落失败记录；ready 接收方不重复写。
    crate::capture_failure::record("native_surface", &failure.to_string());
    failure.into()
}

/// 前台窗口、本线程活动窗口与焦点都已是 `hwnd`。
unsafe fn owns_foreground(hwnd: HWND) -> bool {
    let mut gui = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    GetForegroundWindow() == hwnd
        && GetGUIThreadInfo(GetCurrentThreadId(), &mut gui).is_ok()
        && gui.hwndActive == hwnd
        && gui.hwndFocus == hwnd
}

/// 前台锁拒绝或焦点被瞬时抢走时的有界补救：短暂附着当前前台线程的输入队列后重试，
/// 随即解除附着。成功与否仍由调用方的真实焦点门判定，这里不伪造 Ready。
unsafe fn reacquire_foreground(hwnd: HWND) -> bool {
    let current = GetCurrentThreadId();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(4));
        }
        let foreground = GetForegroundWindow();
        let other = if foreground.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let attached =
            other != 0 && other != current && AttachThreadInput(current, other, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetActiveWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        if attached {
            let _ = AttachThreadInput(current, other, false);
        }
        if owns_foreground(hwnd) {
            return true;
        }
    }
    false
}

fn check_activation(
    stage: NativeStartupStage,
    expected: usize,
    foreground: usize,
    active: usize,
    focus: usize,
) -> std::result::Result<(), NativeSurfaceFailure> {
    if expected != 0 && foreground == expected && active == expected && focus == expected {
        return Ok(());
    }
    let message = if stage == NativeStartupStage::DwmReady {
        "快速首屏合成后输入焦点已失效"
    } else if expected == 0 || foreground != expected {
        "快速首屏未能获得键盘焦点"
    } else {
        "快速首屏尚未成为活动输入窗口"
    };
    Err(NativeSurfaceFailure::activation(stage, message))
}

#[cfg(test)]
mod native_startup_failure_tests {
    use super::*;

    #[test]
    fn activation_failure_keeps_type_through_ready_channel_and_boxing() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let original = NativeSurfaceFailure::activation(
            NativeStartupStage::Foreground,
            "前台/活动窗口/焦点未通过",
        );
        let boxed: Box<dyn std::error::Error> = original.into();
        let transported =
            NativeSurfaceFailure::from_error(boxed.as_ref(), NativeStartupStage::Ready);
        sender.send(Err::<(), _>(transported)).unwrap();
        let error: Box<dyn std::error::Error> = receiver.recv().unwrap().unwrap_err().into();
        assert!(is_activation_failure(error.as_ref()));
        assert_eq!(
            error.downcast_ref::<NativeSurfaceFailure>().unwrap().stage,
            NativeStartupStage::Foreground
        );
    }

    #[test]
    fn real_activation_requires_all_three_owned_handles_before_and_after_dwm() {
        for stage in [NativeStartupStage::Foreground, NativeStartupStage::DwmReady] {
            assert!(check_activation(stage, 7, 7, 7, 7).is_ok());
            for observed in [(8, 7, 7), (7, 8, 7), (7, 7, 8), (0, 0, 0)] {
                let failure =
                    check_activation(stage, 7, observed.0, observed.1, observed.2).unwrap_err();
                assert_eq!(failure.kind, NativeSurfaceFailureKind::ActivationFailed);
                assert_eq!(failure.stage, stage);
            }
            assert!(check_activation(stage, 0, 0, 0, 0).is_err());
        }
    }

    #[test]
    fn render_or_setup_error_does_not_become_activation_failure_by_its_text() {
        let error = std::io::Error::other("SetForegroundWindow 前的绘制资源创建失败");
        let failure = NativeSurfaceFailure::from_error(&error, NativeStartupStage::Painting);
        assert_eq!(failure.kind, NativeSurfaceFailureKind::Other);
        let boxed: Box<dyn std::error::Error> = failure.into();
        assert!(!is_activation_failure(boxed.as_ref()));
    }

    #[test]
    fn timeout_keeps_the_stage_observed_before_cleanup_or_late_ready() {
        let stage = AtomicU32::new(NativeStartupStage::Painting as u32);
        stage.store(NativeStartupStage::Foreground as u32, Ordering::Release);
        let failure = NativeSurfaceFailure::timeout(NativeStartupStage::load(&stage));
        stage.store(NativeStartupStage::Ready as u32, Ordering::Release);
        assert_eq!(failure.kind, NativeSurfaceFailureKind::ReadyTimeout);
        assert_eq!(failure.stage, NativeStartupStage::Foreground);
        assert!(failure.to_string().contains("stage=Foreground"));
        assert!(!is_activation_failure(&failure));
    }

    #[test]
    fn disconnected_preserves_published_failure_and_is_never_a_timeout() {
        let (sender, receiver) = mpsc::sync_channel::<()>(1);
        drop(sender);
        assert_eq!(
            receiver.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        let closed = NativeSurfaceFailure::disconnected(NativeStartupStage::Creating, None);
        assert_eq!(closed.kind, NativeSurfaceFailureKind::Other);
        assert!(closed.to_string().contains("通道已关闭"));
        let known =
            NativeSurfaceFailure::activation(NativeStartupStage::DwmReady, "合成后焦点已失效");
        let closed = NativeSurfaceFailure::disconnected(NativeStartupStage::Ready, Some(known));
        assert_eq!(closed.kind, NativeSurfaceFailureKind::ActivationFailed);
        assert_eq!(closed.stage, NativeStartupStage::DwmReady);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SurfaceState {
    pub anchor: Option<(i32, i32)>,
    pub pointer: (i32, i32),
    pub crop: Option<Rect>,
    /// 未确认选区时实际绘制的顶层窗口预览；不成为 Editor crop。
    pub window_preview: Option<Rect>,
    /// Ctrl+A 固定的屏幕预览；点击确认前保持独立于 Editor crop。
    pub manual_preview: Option<Rect>,
    pub dragging: bool,
    pub cancelled: bool,
    pub toolbar_monitor: usize,
    /// 仅观察；成功路径 finish_handoff 返回带原始修饰键的 typed 输入。
    pub pending_keys: Vec<Input>,
    pub pending_keystrokes: Vec<gpui::Keystroke>,
    /// 保留高精度滚轮的半刻度，交接后继续使用同一累加状态。
    pub wheel_input: WheelAccumulator,
    pub click_seed: Option<ClickSeed>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfaceMetrics {
    /// 从调用方提供的 worker 起始时刻算起，包含冻结像素准备。
    pub ready_ms: f64,
    pub request_to_ready_ms: Option<f64>,
    pub start_surface_to_ready_ms: f64,
}

struct Shared {
    state: Mutex<SurfaceState>,
    metrics: Mutex<Option<SurfaceMetrics>>,
    failure: Mutex<Option<NativeSurfaceFailure>>,
    startup_stage: AtomicU32,
    hwnd: AtomicUsize,
    thread_id: AtomicU32,
    stop: AtomicBool,
    finish: AtomicBool,
    handed_off: AtomicBool,
    completed: Mutex<Vec<CapturedInput>>,
    completed_output: Mutex<Option<StartupOutput>>,
    exited: AtomicBool,
    dispatch: DispatchTrace,
    cancel_token: Option<crate::capture_lock::NativeCaptureCancelToken>,
}

/// 只记小型时间戳；首屏真正就绪前不做日志 I/O。
#[derive(Default)]
struct DispatchTrace {
    enabled: bool,
    before_surface_start_ms: f64,
    start_qpc: usize,
    planes_validated_qpc: usize,
    slot_begin_qpc: AtomicUsize,
    slot_taken_qpc: AtomicUsize,
    warm_validated_qpc: AtomicUsize,
    setup_send_begin_qpc: AtomicUsize,
    setup_sent_qpc: AtomicUsize,
    wake_begin_qpc: AtomicUsize,
    wake_checked_qpc: AtomicUsize,
    wake_signal_begin_qpc: AtomicUsize,
    wake_signal_end_qpc: AtomicUsize,
    wake_identity_valid: AtomicBool,
    wake_signalled: AtomicBool,
    wake_error: AtomicU32,
    setup_received_qpc: AtomicUsize,
    native_enter_qpc: AtomicUsize,
    warm_wait_begin_qpc: AtomicUsize,
    warm_wait_end_qpc: AtomicUsize,
    warm_kernel_event_received_qpc: AtomicUsize,
    warm_drain_end_qpc: AtomicUsize,
    warm_messages: AtomicU32,
    warm_paints_validated: AtomicU32,
}
impl DispatchTrace {
    fn stamp(&self, target: &AtomicUsize) {
        if self.enabled {
            target.store(crate::capture_standby::qpc() as usize, Ordering::Release);
        }
    }

    fn emit_after_ready(&self, hwnd: HWND, prewarmed: bool, ready_qpc: usize) {
        if !self.enabled {
            return;
        }
        let load = |value: &AtomicUsize| value.load(Ordering::Acquire);
        let slot_begin = load(&self.slot_begin_qpc);
        let slot_taken = load(&self.slot_taken_qpc);
        let sent = load(&self.setup_sent_qpc);
        let received = load(&self.setup_received_qpc);
        let entered = load(&self.native_enter_qpc);
        let post_begin = load(&self.wake_signal_begin_qpc);
        let post_end = load(&self.wake_signal_end_qpc);
        let mut frequency = 0_i64;
        let frequency_ok =
            unsafe { QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0;
        let elapsed = |begin: usize, end: usize| {
            (frequency_ok && begin != 0 && end >= begin)
                .then(|| (end - begin) as f64 * 1000. / frequency as f64)
        };
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase": "startup_surface_dispatch", "pid": std::process::id(),
            "wake_mechanism": "kernel-auto-reset-event",
            "hwnd": hwnd.0 as usize, "prewarmed": prewarmed,
            "qpc_frequency": if frequency_ok { Some(frequency) } else { None },
            "before_surface_start_ms": self.before_surface_start_ms,
            "start_enter_qpc": self.start_qpc,
            "planes_validated_qpc": self.planes_validated_qpc,
            "slot_begin_qpc": slot_begin, "slot_taken_qpc": slot_taken,
            "warm_validated_qpc": load(&self.warm_validated_qpc),
            "setup_send_begin_qpc": load(&self.setup_send_begin_qpc),
            "setup_sent_qpc": sent, "wake_begin_qpc": load(&self.wake_begin_qpc),
            "wake_identity_checked_qpc": load(&self.wake_checked_qpc),
            "wake_identity_valid": self.wake_identity_valid.load(Ordering::Acquire),
            "wake_signal_begin_qpc": post_begin, "wake_signal_end_qpc": post_end,
            "wake_signalled": self.wake_signalled.load(Ordering::Acquire),
            "wake_error_hresult": self.wake_error.load(Ordering::Acquire) as i32,
            "warm_wait_begin_qpc": load(&self.warm_wait_begin_qpc),
            "warm_wait_end_qpc": load(&self.warm_wait_end_qpc),
            "warm_kernel_event_received_qpc": load(&self.warm_kernel_event_received_qpc),
            "warm_drain_end_qpc": load(&self.warm_drain_end_qpc),
            "warm_messages_drained": self.warm_messages.load(Ordering::Acquire),
            "warm_paints_validated": self.warm_paints_validated.load(Ordering::Acquire),
            "warm_drain_batch_limit": 256,
            "setup_received_qpc": received, "run_native_enter_qpc": entered,
            "ready_qpc": ready_qpc,
            "start_to_slot_ms": elapsed(self.start_qpc, slot_begin),
            "slot_lock_ms": elapsed(slot_begin, slot_taken),
            "setup_send_to_receive_ms": elapsed(sent, received),
            "wake_signal_ms": elapsed(post_begin, post_end),
            "receive_to_run_native_ms": elapsed(received, entered),
            "surface_start_to_run_native_ms": elapsed(self.start_qpc, entered),
            "run_native_to_ready_ms": elapsed(entered, ready_qpc),
            // 收取可以先于发送方记录完成；保留真实缺失/先后次序，不阻挡首屏。
            "sender_trace_complete": !prewarmed || post_end != 0
        }));
    }
}

enum Command {
    Handoff {
        target: usize,
        reply: SyncSender<std::result::Result<Option<SurfaceState>, String>>,
    },
}

struct Controller {
    shared: Arc<Shared>,
    commands: SyncSender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct NativePlane {
    rect: Rect,
    image: Option<Arc<RenderImage>>,
}

enum PlaneInput {
    Cpu(Vec<(Rect, Arc<RenderImage>)>),
    Native(Vec<Rect>),
}

struct NativeSetup {
    bounds: ScreenRect,
    planes: Vec<NativePlane>,
    startup_bitmap: Option<Arc<StartupBitmap>>,
    wheel_settings: FreshWheelSettings,
    current: usize,
    targets: Vec<WindowTarget>,
    started: Instant,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    ready: SyncSender<std::result::Result<(), NativeSurfaceFailure>>,
}

#[derive(Clone, Copy)]
pub(crate) struct WarmSurfaceIdentity {
    pub hwnd: usize,
    pub token: usize,
    pub ready_qpc: usize,
    pub physical_bounds: ScreenRect,
    pub cloaked: bool,
    pub window_visible: bool,
}

struct WarmSignals {
    wake_event: OwnedHandle,
    hwnd: AtomicUsize,
    thread_id: AtomicU32,
    token: AtomicUsize,
    stop: AtomicBool,
    exited: AtomicBool,
}
impl WarmSignals {
    fn new() -> Result<Self> {
        let event = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self {
            wake_event: unsafe { OwnedHandle::from_raw_handle(event.0) },
            hwnd: AtomicUsize::new(0),
            thread_id: AtomicU32::new(0),
            token: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            exited: AtomicBool::new(false),
        })
    }
    fn event_handle(&self) -> HANDLE {
        HANDLE(self.wake_event.as_raw_handle())
    }
    fn original_window(&self) -> bool {
        let hwnd = HWND(self.hwnd.load(Ordering::Acquire) as *mut _);
        let token = self.token.load(Ordering::Acquire);
        let mut pid = 0;
        !self.exited.load(Ordering::Acquire)
            && token != 0
            && unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) }
                == self.thread_id.load(Ordering::Acquire)
            && pid == std::process::id()
            && unsafe { GetPropW(hwnd, w!("RustShot.StartupSurface.Token")) }.0 as usize == token
    }
    fn valid(&self) -> bool {
        !self.stop.load(Ordering::Acquire) && self.original_window()
    }
    fn wake(&self) {
        self.wake_recorded(None);
    }
    fn wake_recorded(&self, trace: Option<&DispatchTrace>) {
        if let Some(trace) = trace {
            trace.stamp(&trace.wake_begin_qpc);
        }
        let valid = self.original_window();
        if let Some(trace) = trace {
            trace.wake_identity_valid.store(valid, Ordering::Release);
            trace.stamp(&trace.wake_checked_qpc);
        }
        if valid {
            if let Some(trace) = trace {
                trace.stamp(&trace.wake_signal_begin_qpc);
            }
            unsafe {
                // 原 HWND/TID/token 仍决定归属；唤醒不经过 USER 消息队列。
                let result = SetEvent(self.event_handle());
                if let Some(trace) = trace {
                    trace
                        .wake_signalled
                        .store(result.is_ok(), Ordering::Release);
                    if let Err(error) = result {
                        trace
                            .wake_error
                            .store(error.code().0 as u32, Ordering::Release);
                    }
                    trace.stamp(&trace.wake_signal_end_qpc);
                }
            }
        }
    }
}

struct WarmNative {
    setup: SyncSender<NativeSetup>,
    signals: Arc<WarmSignals>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for WarmNative {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            // 停止位先于唤醒；原 HWND/TID/token 校验不会因停止而跳过投递。
            self.signals.stop.store(true, Ordering::Release);
            self.signals.wake();
            let _ = worker.join();
        }
    }
}

fn warm_slot() -> &'static Mutex<Option<WarmNative>> {
    static SLOT: OnceLock<Mutex<Option<WarmNative>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(crate) fn discard_standby_warm() {
    // join 不持全局锁；不会挡住原生 WindowProc 或把句柄从别的线程销毁。
    let warm = warm_slot().lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(warm);
}

pub(crate) struct StandbyWarmGuard {
    pub identity: Option<WarmSurfaceIdentity>,
    retain_for_capture: bool,
}
impl StandbyWarmGuard {
    pub fn retain_for_capture(&mut self) {
        self.retain_for_capture = true;
    }
}
impl Drop for StandbyWarmGuard {
    fn drop(&mut self) {
        if !self.retain_for_capture {
            discard_standby_warm();
        }
    }
}

pub(crate) fn prewarm_standby() -> Result<StandbyWarmGuard> {
    discard_standby_warm();
    let mut guard = StandbyWarmGuard {
        identity: None,
        retain_for_capture: false,
    };
    if !enabled() {
        return Ok(guard);
    }
    let (setup, setup_receiver) = mpsc::sync_channel(1);
    let (ready, ready_receiver) = mpsc::sync_channel(1);
    let signals = Arc::new(WarmSignals::new()?);
    let native_signals = signals.clone();
    let worker = thread::Builder::new()
        .name("rustshot-first-surface".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_warm_native(&native_signals, setup_receiver, &ready)
            }));
            if !matches!(result, Ok(Ok(()))) {
                let error = match result {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "快速首屏预热线程异常".to_owned(),
                    Ok(Ok(())) => unreachable!(),
                };
                let _ = ready.try_send(Err(error));
            }
            native_signals.hwnd.store(0, Ordering::Release);
            native_signals.exited.store(true, Ordering::Release);
        })?;
    let warm = WarmNative {
        setup,
        signals,
        worker: Some(worker),
    };
    let identity = ready_receiver
        .recv_timeout(WAIT)
        .map_err(|_| "快速首屏预热未能及时完成")?
        .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
    if !warm.signals.valid() {
        return Err("快速首屏预热原实例已失效".into());
    }
    guard.identity = Some(identity);
    *warm_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(warm);
    Ok(guard)
}

struct WheelQueryTimings {
    begin_qpc: usize,
    end_qpc: usize,
    thread_id: u32,
    duration_ms: f64,
}

struct WheelQuery {
    wheel: (u32, u32),
    timings: Option<WheelQueryTimings>,
}

impl WheelQuery {
    fn read(tracing: bool) -> Self {
        let thread_id = tracing.then(|| unsafe { GetCurrentThreadId() });
        let begin_qpc = tracing.then(|| crate::capture_standby::qpc() as usize);
        let started = tracing.then(Instant::now);
        let wheel = startup_surface_input::wheel_settings();
        let duration_ms = started.map(|start| start.elapsed().as_secs_f64() * 1000.);
        let end_qpc = tracing.then(|| crate::capture_standby::qpc() as usize);
        let timings = duration_ms.map(|duration_ms| WheelQueryTimings {
            begin_qpc: begin_qpc.expect("wheel query begin"),
            end_qpc: end_qpc.expect("wheel query end"),
            thread_id: thread_id.expect("wheel query thread"),
            duration_ms,
        });
        Self { wheel, timings }
    }
}

/// 每个请求独占一次最新滚轮查询；不持有窗口、DC 或输入状态。
/// 任意提前退出都自然 join，不能把查询线程留到下一次请求。
pub(crate) struct PendingWheelSettings {
    worker: Option<JoinHandle<WheelQuery>>,
    completed: Option<WheelQuery>,
    tracing: bool,
    mode: &'static str,
    spawn_error: Option<String>,
    prepare_ms: Option<f64>,
}

pub(crate) struct FreshWheelSettings {
    query: WheelQuery,
    mode: &'static str,
    consumer_wait_ms: Option<f64>,
    spawn_error: Option<String>,
    prepare_ms: Option<f64>,
}

impl PendingWheelSettings {
    pub(crate) fn begin() -> Self {
        let tracing = std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
        let started = tracing.then(Instant::now);
        let mut request = match thread::Builder::new()
            .name("rustshot-wheel-settings".to_owned())
            .spawn(move || WheelQuery::read(tracing))
        {
            Ok(worker) => Self {
                worker: Some(worker),
                completed: None,
                tracing,
                mode: "parallel-request-worker",
                spawn_error: None,
                prepare_ms: None,
            },
            Err(error) => Self {
                worker: None,
                // spawn 失败没有运行 worker；这里只同步查询一次。
                completed: Some(WheelQuery::read(tracing)),
                tracing,
                mode: "synchronous-spawn-fallback",
                spawn_error: Some(error.to_string()),
                prepare_ms: None,
            },
        };
        request.prepare_ms = started.map(|start| start.elapsed().as_secs_f64() * 1000.);
        request
    }

    pub(crate) fn resolve(mut self) -> Result<FreshWheelSettings> {
        let started = self.tracing.then(Instant::now);
        let query = if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "滚轮参数读取线程异常退出")?
        } else {
            self.completed.take().ok_or("滚轮参数请求已被消费")?
        };
        let consumer_wait_ms = started.map(|start| start.elapsed().as_secs_f64() * 1000.);
        Ok(FreshWheelSettings {
            query,
            mode: self.mode,
            consumer_wait_ms,
            spawn_error: self.spawn_error.take(),
            prepare_ms: self.prepare_ms,
        })
    }
}

impl Drop for PendingWheelSettings {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl FreshWheelSettings {
    fn synchronous() -> Self {
        let tracing = std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
        Self {
            query: WheelQuery::read(tracing),
            mode: "synchronous-caller",
            consumer_wait_ms: tracing.then_some(0.),
            spawn_error: None,
            prepare_ms: None,
        }
    }
}

/// 只跨线程传数值 HWND token；没有把 Win32 线程关联对象标为 Send。
#[derive(Clone)]
pub(crate) struct StartupSurface {
    controller: Arc<Controller>,
}

impl StartupSurface {
    pub(crate) fn start(
        bounds: ScreenRect,
        planes: Vec<(Rect, Arc<RenderImage>)>,
        current: usize,
        crop: Option<Rect>,
        window_targets: Vec<WindowTarget>,
        started: Instant,
    ) -> Result<Self> {
        Self::start_with_bitmap(bounds, planes, current, crop, window_targets, started, None)
    }

    pub(crate) fn start_with_bitmap(
        bounds: ScreenRect,
        planes: Vec<(Rect, Arc<RenderImage>)>,
        current: usize,
        crop: Option<Rect>,
        window_targets: Vec<WindowTarget>,
        started: Instant,
        startup_bitmap: Option<Arc<StartupBitmap>>,
    ) -> Result<Self> {
        Self::start_with_bitmap_and_wheel(
            bounds,
            planes,
            current,
            crop,
            window_targets,
            started,
            startup_bitmap,
            None,
            None,
        )
    }

    // 保持旧入口参数顺序，只追加同请求的查询票据。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_with_bitmap_and_wheel(
        bounds: ScreenRect,
        planes: Vec<(Rect, Arc<RenderImage>)>,
        current: usize,
        crop: Option<Rect>,
        window_targets: Vec<WindowTarget>,
        started: Instant,
        startup_bitmap: Option<Arc<StartupBitmap>>,
        wheel: Option<PendingWheelSettings>,
        cancel_token: Option<crate::capture_lock::NativeCaptureCancelToken>,
    ) -> Result<Self> {
        Self::start_planes(
            bounds,
            PlaneInput::Cpu(planes),
            current,
            crop,
            window_targets,
            started,
            startup_bitmap,
            wheel,
            cancel_token,
        )
    }

    // 保持旧原生入口参数顺序，只追加同请求的查询票据。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_native_with_wheel(
        bounds: ScreenRect,
        rects: Vec<Rect>,
        current: usize,
        crop: Option<Rect>,
        window_targets: Vec<WindowTarget>,
        started: Instant,
        bitmap: Arc<StartupBitmap>,
        wheel: Option<PendingWheelSettings>,
        cancel_token: Option<crate::capture_lock::NativeCaptureCancelToken>,
    ) -> Result<Self> {
        Self::start_planes(
            bounds,
            PlaneInput::Native(rects),
            current,
            crop,
            window_targets,
            started,
            Some(bitmap),
            wheel,
            cancel_token,
        )
    }

    // 两个兼容入口都在此验证元数据并消费票据，不复制启动逻辑。
    #[allow(clippy::too_many_arguments)]
    fn start_planes(
        bounds: ScreenRect,
        input: PlaneInput,
        current: usize,
        crop: Option<Rect>,
        window_targets: Vec<WindowTarget>,
        started: Instant,
        startup_bitmap: Option<Arc<StartupBitmap>>,
        wheel: Option<PendingWheelSettings>,
        cancel_token: Option<crate::capture_lock::NativeCaptureCancelToken>,
    ) -> Result<Self> {
        let start_enter = Instant::now();
        let tracing = std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some();
        let start_qpc = if tracing {
            crate::capture_standby::qpc() as usize
        } else {
            0
        };
        if !enabled() {
            return Err("快速首屏已禁用".into());
        }
        let planes = match input {
            PlaneInput::Cpu(planes) => {
                // 原 CPU 路径完整验证真实帧，不用空图替代尚未读回的像素。
                validate_planes(bounds, &planes)?;
                planes
                    .into_iter()
                    .map(|(rect, image)| NativePlane {
                        rect,
                        image: Some(image),
                    })
                    .collect::<Vec<_>>()
            }
            PlaneInput::Native(rects) => {
                let bitmap = startup_bitmap
                    .as_deref()
                    .ok_or("快速首屏原生元数据缺少冻结位图")?;
                validate_native_planes(bounds, &rects, current, bitmap)?;
                rects
                    .into_iter()
                    .map(|rect| NativePlane { rect, image: None })
                    .collect::<Vec<_>>()
            }
        };
        if let Some(bitmap) = startup_bitmap.as_ref() {
            let expected_bytes = (bounds.width as usize)
                .checked_mul(bounds.height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or("快速首屏原生位图长度溢出")?;
            if bitmap.bounds() != bounds || bitmap.source_bytes() != expected_bytes {
                return Err("快速首屏原生位图与本次冻结桌面不符".into());
            }
        }
        if crop.is_some_and(|r| !r.valid(bounds.width, bounds.height)) {
            return Err("快速首屏选区无效".into());
        }
        let planes_validated_qpc = if tracing {
            crate::capture_standby::qpc() as usize
        } else {
            0
        };
        let current = current.min(planes.len() - 1);
        let r = planes[current].rect;
        let initial = SurfaceState {
            anchor: None,
            pointer: (
                r.x as i32 + r.width as i32 / 2,
                r.y as i32 + r.height as i32 / 2,
            ),
            crop,
            window_preview: None,
            manual_preview: None,
            dragging: false,
            cancelled: false,
            toolbar_monitor: current,
            pending_keys: Vec::new(),
            pending_keystrokes: Vec::new(),
            wheel_input: WheelAccumulator::default(),
            click_seed: None,
        };
        let shared = Arc::new(Shared {
            cancel_token,
            state: Mutex::new(initial),
            metrics: Mutex::new(None),
            failure: Mutex::new(None),
            startup_stage: AtomicU32::new(NativeStartupStage::Queued as u32),
            hwnd: AtomicUsize::new(0),
            thread_id: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            finish: AtomicBool::new(false),
            handed_off: AtomicBool::new(false),
            completed: Mutex::new(Vec::new()),
            completed_output: Mutex::new(None),
            exited: AtomicBool::new(false),
            dispatch: DispatchTrace {
                enabled: tracing,
                before_surface_start_ms: start_enter.duration_since(started).as_secs_f64() * 1000.,
                start_qpc,
                planes_validated_qpc,
                ..Default::default()
            },
        });
        let (commands, receiver) = mpsc::sync_channel(8);
        let (ready, ready_receiver) = mpsc::sync_channel(1);
        // 同请求 ticket 已在捕获期间查询；旧调用入口仍在这里同步读取最新设置。
        let wheel_settings = match wheel {
            Some(pending) => pending.resolve()?,
            None => FreshWheelSettings::synchronous(),
        };
        // 待命请求可先捕获；原生呈现前必须等同请求的前台授权调用完成。
        crate::capture_standby::wait_foreground_grant_before_native()?;
        let mut setup = Some(NativeSetup {
            bounds,
            planes,
            startup_bitmap,
            wheel_settings,
            current,
            targets: window_targets,
            started,
            shared: shared.clone(),
            commands: receiver,
            ready,
        });
        shared.dispatch.stamp(&shared.dispatch.slot_begin_qpc);
        let warm = warm_slot().lock().unwrap_or_else(|e| e.into_inner()).take();
        shared.dispatch.stamp(&shared.dispatch.slot_taken_qpc);
        let mut prewarmed_worker = None;
        if let Some(mut warm) = warm {
            let valid = warm.signals.valid();
            shared.dispatch.stamp(&shared.dispatch.warm_validated_qpc);
            if valid {
                shared.dispatch.stamp(&shared.dispatch.setup_send_begin_qpc);
                match warm.setup.try_send(setup.take().expect("native setup")) {
                    Ok(()) => {
                        shared.dispatch.stamp(&shared.dispatch.setup_sent_qpc);
                        warm.signals.wake_recorded(Some(&shared.dispatch));
                        prewarmed_worker = Some(warm.worker.take().expect("warm native worker"));
                    }
                    Err(
                        mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value),
                    ) => {
                        setup = Some(value);
                    }
                }
            }
        }
        let worker = if let Some(worker) = prewarmed_worker {
            worker
        } else {
            thread::Builder::new()
                .name("rustshot-first-surface".to_owned())
                .spawn(move || run_capture_native(setup.expect("cold native setup"), None))?
        };
        let surface = Self {
            controller: Arc::new(Controller {
                shared,
                commands,
                worker: Mutex::new(Some(worker)),
            }),
        };
        match ready_receiver.recv_timeout(WAIT) {
            Ok(Ok(())) => Ok(surface),
            Ok(Err(error)) => Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let failure = NativeSurfaceFailure::timeout(NativeStartupStage::load(
                    &surface.controller.shared.startup_stage,
                ));
                // 必须在 surface Drop 的 stop/join 之前记录。3 秒仅限 ready 等待，
                // 不把仍可能阻塞的原生回收声称为已结束或分离线程。
                crate::capture_failure::record("native_surface_timeout", &failure.to_string());
                Err(failure.into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let published = surface
                    .controller
                    .shared
                    .failure
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let no_published_failure = published.is_none();
                let failure = NativeSurfaceFailure::disconnected(
                    NativeStartupStage::load(&surface.controller.shared.startup_stage),
                    published,
                );
                if no_published_failure {
                    crate::capture_failure::record(
                        "native_surface_channel_closed",
                        &failure.to_string(),
                    );
                }
                Err(failure.into())
            }
        }
    }

    pub(crate) fn handle(&self) -> HWND {
        HWND(self.controller.shared.hwnd.load(Ordering::Acquire) as *mut _)
    }

    pub(crate) fn snapshot(&self) -> SurfaceState {
        self.controller
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn metrics(&self) -> Option<SurfaceMetrics> {
        *self
            .controller
            .shared
            .metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 尚在按下/拖动时不交接，避免将一个手势拆到两个 HWND。
    pub(crate) fn handoff(&self, target: HWND) -> Result<Option<SurfaceState>> {
        if let Some(error) = self
            .controller
            .shared
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(error.into());
        }
        if self.snapshot().cancelled {
            return Ok(Some(self.snapshot()));
        }
        if self.controller.shared.exited.load(Ordering::Acquire) {
            return Err("快速首屏已关闭".into());
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.controller
            .commands
            .try_send(Command::Handoff {
                target: target.0 as usize,
                reply,
            })
            .map_err(|_| "快速首屏交接队列不可用")?;
        self.controller.wake();
        receiver
            .recv_timeout(WAIT)
            .map_err(|_| "快速首屏交接超时")?
            .map_err(Into::into)
    }

    /// 失败、取消和外层退出只回收，不执行排队的复制/保存操作。
    pub(crate) fn dismiss(&self) -> Result<()> {
        self.controller
            .shared
            .finish
            .store(false, Ordering::Release);
        self.controller.stop();
        self.controller.join()?;
        self.controller
            .shared
            .completed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.controller
            .shared
            .completed_output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        Ok(())
    }

    /// 唯一成功入口。先销毁原生窗口、回收线程，再由调用方在 GPUI UI 线程重放。
    pub(crate) fn finish_handoff(
        &self,
        scale: f32,
    ) -> Result<Vec<startup_surface_input::ReplayInput>> {
        if !scale.is_finite() || scale <= 0. {
            return Err("快速首屏交接缩放无效".into());
        }
        if !self.controller.shared.handed_off.load(Ordering::Acquire) {
            return Err("快速首屏尚未完成稳定交接".into());
        }
        self.controller.shared.finish.store(true, Ordering::Release);
        self.controller.stop();
        self.controller.join()?;
        if self.snapshot().cancelled {
            return Ok(Vec::new());
        }
        if let Some(error) = self
            .controller
            .shared
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(error.into());
        }
        let inputs = std::mem::take(
            &mut *self
                .controller
                .shared
                .completed
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let mut replay: Vec<_> = inputs
            .into_iter()
            .filter_map(|event| event.into_replay(scale))
            .collect();
        if let Some(output) = self
            .controller
            .shared
            .completed_output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            replay.push(startup_surface_input::ReplayInput::Output(output));
        }
        Ok(replay)
    }
}

pub(crate) fn enabled() -> bool {
    std::env::var("RUSTSHOT_STARTUP_SURFACE").as_deref() != Ok("0")
}

impl Controller {
    fn wake(&self) {
        let hwnd = HWND(self.shared.hwnd.load(Ordering::Acquire) as *mut _);
        if !hwnd.is_invalid()
            && unsafe { GetWindowThreadProcessId(hwnd, None) }
                == self.shared.thread_id.load(Ordering::Acquire)
        {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }
    fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.wake();
    }
    fn join(&self) -> Result<()> {
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            worker.join().map_err(|_| "快速首屏线程回收失败")?;
        }
        Ok(())
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
        let _ = self.join();
    }
}

fn validate_planes(bounds: ScreenRect, planes: &[(Rect, Arc<RenderImage>)]) -> Result<()> {
    if bounds.width == 0
        || bounds.height == 0
        || bounds.width > 16384
        || bounds.height > 16384
        || bounds.left.checked_add(bounds.width as i32).is_none()
        || bounds.top.checked_add(bounds.height as i32).is_none()
        || planes.is_empty()
    {
        return Err("快速首屏桌面范围无效".into());
    }
    for (rect, image) in planes {
        if !rect.valid(bounds.width, bounds.height) || image.frame_count() == 0 {
            return Err("快速首屏图像范围无效".into());
        }
        let size = image.size(0);
        if size.width.0 != rect.width as i32
            || size.height.0 != rect.height as i32
            || image.as_bytes(0).map(|b| b.len())
                != Some(rect.width as usize * rect.height as usize * 4)
        {
            return Err("快速首屏像素与物理尺寸不匹配".into());
        }
    }
    Ok(())
}

fn validate_native_planes(
    bounds: ScreenRect,
    rects: &[Rect],
    current: usize,
    bitmap: &StartupBitmap,
) -> Result<()> {
    if bounds.width == 0
        || bounds.height == 0
        || bounds.width > 16384
        || bounds.height > 16384
        || bounds.left.checked_add(bounds.width as i32).is_none()
        || bounds.top.checked_add(bounds.height as i32).is_none()
        || rects.is_empty()
        || current >= rects.len()
    {
        return Err("快速首屏原生桌面范围或当前屏幕无效".into());
    }
    let expected_bytes = (bounds.width as usize)
        .checked_mul(bounds.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("快速首屏原生 BGRA 等价长度溢出")?;
    if rects
        .iter()
        .any(|rect| !rect.valid(bounds.width, bounds.height))
        || bitmap.bounds() != bounds
        || bitmap.source_bytes() != expected_bytes
        || bitmap.regions() != rects
        || !matches!(bitmap.kind(), "dib" | "ddb")
    {
        return Err("快速首屏原生位图、捕获屏幕区域或 BGRA 等价长度不符".into());
    }
    Ok(())
}

struct EventQueue {
    messages: Mutex<VecDeque<CapturedInput>>,
    presented_preview: Mutex<PreviewPresentation>,
    /// 已确认选区和预览的可见范围，供 Ctrl+A 比较及早期确认范围 Middle 的独立回执。
    presented_selection: Mutex<PreviewPresentation>,
    preview_drag_origin: Mutex<startup_surface_input::PreviewMotionSource>,
    overflow: AtomicBool,
    dormant: AtomicBool,
    cursors: [usize; 6],
    current_cursor: AtomicUsize,
    isolate_moves: bool,
    qa_modifiers: bool,
    bounds: ScreenRect,
    wheel: (u32, u32),
}

impl EventQueue {
    fn push(&self, mut event: Input) {
        // 禁用窗口仍能收到发送的消息；待命期间已派发的输入不转存为截图手势。
        // WindowProc 只转发到队列，框选状态仍由外层消息循环独占。
        if self.dormant.load(Ordering::Acquire) && is_input(event.0) {
            return;
        }
        if event.0 == WM_MOUSEMOVE && self.isolate_moves {
            if event.1 & QA_MOVE == 0 {
                return;
            }
            event.1 &= !QA_MOVE;
        }
        let force_control = self.qa_modifiers
            && matches!(event.0, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
            && event.1 & QA_CONTROL != 0;
        if force_control {
            event.1 &= !QA_CONTROL;
        }
        let mut captured = CapturedInput::snapshot(event, self.bounds, self.wheel);
        if force_control {
            captured.control_for_qa();
        }
        self.push_captured(captured);
    }

    fn push_captured(&self, mut captured: CapturedInput) {
        let message = captured.raw.0;
        captured.track_preview_motion(
            &mut self
                .preview_drag_origin
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        if startup_surface_input::monitor_snap_down(captured.raw, captured.modifiers) {
            captured.set_presented_monitor_preview(
                self.presented_selection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .snapshot(),
            );
        }
        if matches!(message, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK)
            || captured.preview_confirm_down()
            || captured.text_shortcut_down().is_some()
        {
            // 这里只复制外层真实绘制后发布的小型回执，不触碰 Editor 或重算目标。
            captured.set_presented_window_preview(
                self.presented_preview
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .snapshot(),
            );
        }
        if matches!(message, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK) {
            captured.set_presented_confirmed_selection(
                self.presented_selection
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .snapshot(),
            );
        }
        let mut messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        if message == WM_MOUSEMOVE
            && messages
                .back()
                .is_some_and(|last| last.raw.0 == WM_MOUSEMOVE)
        {
            captured.inherit_mouse_motion(messages.back().unwrap());
            *messages.back_mut().unwrap() = captured;
            return;
        }
        if message == WM_PAINT && messages.iter().any(|e| e.raw.0 == WM_PAINT) {
            return;
        }
        if messages.len() == EVENT_LIMIT {
            // move 的包络可能是本手势唯一的远移证据；只能丢弃可重建的重绘。
            if let Some(index) = messages.iter().position(|e| e.raw.0 == WM_PAINT) {
                messages.remove(index);
            } else {
                self.overflow.store(true, Ordering::Release);
                return;
            }
        }
        messages.push_back(captured);
    }
    fn pop(&self) -> Option<CapturedInput> {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    fn begin_preview_paint(&self) {
        self.presented_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_paint();
        self.presented_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_paint();
    }

    fn reset_preview(&self) {
        *self
            .presented_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = PreviewPresentation::default();
        *self
            .presented_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = PreviewPresentation::default();
    }

    #[cfg(test)]
    fn publish_completed_preview(&self, rect: Option<Rect>) -> Result<()> {
        self.publish_completed_selection(rect, rect)
    }
    fn publish_completed_selection(
        &self,
        selection: Option<Rect>,
        preview: Option<Rect>,
    ) -> Result<()> {
        self.presented_selection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_completed(Rect::full(self.bounds.width, self.bounds.height), selection)?;
        self.presented_preview
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_completed(Rect::full(self.bounds.width, self.bounds.height), preview)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const EventQueue;
        if !data.is_null() {
            match message {
                WM_NCHITTEST if (*data).dormant.load(Ordering::Acquire) => {
                    return LRESULT(HTTRANSPARENT as isize)
                }
                WM_MOUSEACTIVATE if (*data).dormant.load(Ordering::Acquire) => {
                    return LRESULT(MA_NOACTIVATE as isize)
                }
                WM_SETCURSOR
                    if !(*data).dormant.load(Ordering::Acquire)
                        && lparam.0 as u16 as u32 == HTCLIENT
                        && GetForegroundWindow() == hwnd
                        && set_surface_cursor(hwnd) =>
                {
                    return LRESULT(1);
                }
                WM_PAINT | WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP | WM_MOUSEMOVE
                | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_RBUTTONUP | WM_MBUTTONDOWN
                | WM_MBUTTONDBLCLK | WM_MBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_KEYDOWN
                | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR
                | WM_CAPTURECHANGED | WM_CLOSE => {
                    if matches!(message, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_KEYDOWN) {
                        // 输入来源只在本消息处理期间可读；远控注入决定后续工具窗是否兼容远程画面。
                        crate::remote_display::observe_input_source();
                    }
                    (*data).push((message, wparam.0, lparam.0));
                    return LRESULT(0);
                }
                WM_ERASEBKGND | WAKE => return LRESULT(1),
                WM_ACTIVATE | WM_ACTIVATEAPP | WM_KILLFOCUS | WM_CANCELMODE => {
                    // 只转发边界；焦点/取消模式仍交给系统默认处理，不在回调借用 Runtime。
                    (*data).push((message, wparam.0, lparam.0));
                }
                WM_NCDESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum SurfaceCursorKind {
    Cross,
    Horizontal,
    Vertical,
    DiagonalDown,
    DiagonalUp,
    Move,
}
fn surface_cursor_kind(hover: crate::interaction::Hover) -> SurfaceCursorKind {
    use crate::interaction::Hover;
    match hover {
        Hover::Horizontal => SurfaceCursorKind::Horizontal,
        Hover::Vertical => SurfaceCursorKind::Vertical,
        Hover::DiagonalDown => SurfaceCursorKind::DiagonalDown,
        Hover::DiagonalUp => SurfaceCursorKind::DiagonalUp,
        Hover::Move => SurfaceCursorKind::Move,
        _ => SurfaceCursorKind::Cross,
    }
}
fn load_surface_cursors() -> Result<[usize; 6]> {
    // LoadCursor 的系统共享句柄不由本实例销毁；单个 resize 资源不可用时沿用十字兜底。
    let cross = unsafe { LoadCursorW(None, IDC_CROSS)? };
    let mut cursors = [cross.0 as usize; 6];
    for (index, name) in [
        IDC_SIZEWE,
        IDC_SIZENS,
        IDC_SIZENWSE,
        IDC_SIZENESW,
        IDC_SIZEALL,
    ]
    .into_iter()
    .enumerate()
    {
        if let Ok(cursor) = unsafe { LoadCursorW(None, name) } {
            if !cursor.is_invalid() {
                cursors[index + 1] = cursor.0 as usize;
            }
        }
    }
    Ok(cursors)
}
fn surface_window_owned(hwnd: HWND) -> bool {
    let mut pid = 0;
    unsafe {
        IsWindow(Some(hwnd)).as_bool()
            && GetWindowThreadProcessId(hwnd, Some(&mut pid)) == GetCurrentThreadId()
            && pid == std::process::id()
    }
}
fn owns_surface_pointer(hwnd: HWND) -> bool {
    // 不因伪造的 WM_SETCURSOR 或已失去输入所有权而修改其它窗口的当前光标。
    unsafe {
        let mut pid = 0;
        if !IsWindow(Some(hwnd)).as_bool()
            || !IsWindowEnabled(hwnd).as_bool()
            || GetWindowThreadProcessId(hwnd, Some(&mut pid)) != GetCurrentThreadId()
            || pid != std::process::id()
            || GetForegroundWindow() != hwnd
        {
            return false;
        }
        let captured = GetCapture();
        if captured == hwnd {
            return true;
        }
        if !captured.is_invalid() {
            return false;
        }
        // GetCapture 仅覆盖本线程；前台其它线程的 capture 也必须排除。
        let mut foreground = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(0, &mut foreground).is_err()
            || (!foreground.hwndCapture.is_invalid() && foreground.hwndCapture != hwnd)
        {
            return false;
        }
        let mut point = POINT::default();
        GetCursorPos(&mut point).is_ok()
            && WindowFromPoint(point) == hwnd
            && WindowFromPhysicalPoint(point) == hwnd
    }
}
fn set_surface_cursor(hwnd: HWND) -> bool {
    if !surface_window_owned(hwnd) {
        return false;
    }
    // WindowProc 仅读队列的原子句柄镜像，永不访问 Editor 或 &mut Runtime。
    let data = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const EventQueue;
    let mirrored = if data.is_null() {
        0
    } else {
        let queue = unsafe { &*data };
        if queue.dormant.load(Ordering::Acquire) {
            return true;
        }
        queue.current_cursor.load(Ordering::Acquire)
    };
    let cursor = HCURSOR(if mirrored == 0 {
        (unsafe { GetClassLongPtrW(hwnd, GCLP_HCURSOR) }) as *mut _
    } else {
        mirrored as *mut _
    });
    if cursor.is_invalid() {
        return false;
    }
    if owns_surface_pointer(hwnd) {
        unsafe {
            SetCursor(Some(cursor));
        }
    }
    // 保留“句柄可用”语义；未拥有实际指针时不 SetCursor，也不能让默认类光标抢占它。
    true
}
fn update_surface_cursor(hwnd: HWND, hover: crate::interaction::Hover) {
    if !surface_window_owned(hwnd) {
        return;
    }
    let data = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const EventQueue;
    if data.is_null() {
        return;
    }
    let queue = unsafe { &*data };
    if queue.dormant.load(Ordering::Acquire) {
        return;
    }
    let cursor = queue.cursors[surface_cursor_kind(hover) as usize];
    queue.current_cursor.store(cursor, Ordering::Release);
    // 鼠标捕获期间未必产生 WM_SETCURSOR；外层事件消费完成后也按自有指针门即时更新。
    let _ = set_surface_cursor(hwnd);
}

struct NativeWindow {
    hwnd: HWND,
    thread_id: u32,
    token: usize,
    // 保持 CREATESTRUCT 的 queue 指针有效直到 DestroyWindow 返回。
    _queue: Box<EventQueue>,
}
impl NativeWindow {
    fn close(&mut self) -> Result<()> {
        if !self.hwnd.is_invalid() {
            unsafe {
                if GetCurrentThreadId() != self.thread_id {
                    return Err("快速首屏只能由创建线程销毁".into());
                }
                if !IsWindow(Some(self.hwnd)).as_bool() {
                    self.hwnd = HWND::default();
                    return Ok(());
                }
                let mut pid = 0;
                if IsWindow(Some(self.hwnd)).as_bool() {
                    let original_queue = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA)
                        == (&*self._queue as *const EventQueue) as isize;
                    let token = GetPropW(self.hwnd, w!("RustShot.StartupSurface.Token")).0 as usize;
                    if GetWindowThreadProcessId(self.hwnd, Some(&mut pid)) != self.thread_id
                        || pid != std::process::id()
                        || (token != self.token && !(token == 0 && original_queue))
                    {
                        return Err("快速首屏销毁时原窗口身份变化".into());
                    }
                }
                // 先断开 userdata；即使 DestroyWindow 失败，Box 回收后也不会被回调引用。
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                if GetCapture() == self.hwnd {
                    let _ = ReleaseCapture();
                }
                if IsWindow(Some(self.hwnd)).as_bool() {
                    DestroyWindow(self.hwnd)?;
                    // DestroyWindow 成功仍核对原 binding 已退休，不能拿 hwnd=0 代替销毁。
                    if IsWindow(Some(self.hwnd)).as_bool()
                        && GetWindowThreadProcessId(self.hwnd, Some(&mut pid)) == self.thread_id
                        && pid == std::process::id()
                        && GetPropW(self.hwnd, w!("RustShot.StartupSurface.Token")).0 as usize
                            == self.token
                    {
                        return Err("快速首屏销毁后原窗口仍存在".into());
                    }
                }
            }
            self.hwnd = HWND::default();
        }
        Ok(())
    }
}
impl Drop for NativeWindow {
    fn drop(&mut self) {
        let _ = self.close();
        // 异常身份/销毁失败也不能留下指向即将释放 Box 的回调指针。
        // 该 Box 尚存活，地址未归还；仅对原 PID/TID 上仍引用它的 userdata 断开。
        let mut pid = 0;
        if !self.hwnd.is_invalid()
            && unsafe { GetCurrentThreadId() } == self.thread_id
            && unsafe { GetWindowThreadProcessId(self.hwnd, Some(&mut pid)) } == self.thread_id
            && pid == std::process::id()
            && unsafe { GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) }
                == (&*self._queue as *const EventQueue) as isize
        {
            unsafe {
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            }
        }
    }
}

fn uses_whole_canvas_bitmap(
    bitmap: &StartupBitmap,
    bounds: ScreenRect,
    planes: &[NativePlane],
) -> bool {
    bitmap.whole_canvas_black_gaps_valid()
        && bitmap.bounds() == bounds
        && bitmap.regions().len() == planes.len()
        && bitmap
            .regions()
            .iter()
            .zip(planes)
            .all(|(region, plane)| *region == plane.rect)
}

#[derive(Default, serde::Serialize)]
struct NativePaintTrace {
    pid: u32,
    thread_id: u32,
    hwnd: usize,
    paint_enter_qpc: i64,
    paint_end_qpc: i64,
    begin_paint_begin_qpc: i64,
    begin_paint_end_qpc: i64,
    background_begin_qpc: i64,
    background_end_qpc: i64,
    bitmap_draw_cleanup_begin_qpc: i64,
    bitmap_draw_cleanup_end_qpc: i64,
    overlay_begin_qpc: i64,
    overlay_end_qpc: i64,
    draw_flush_begin_qpc: i64,
    draw_flush_end_qpc: i64,
    end_paint_begin_qpc: i64,
    end_paint_end_qpc: i64,
    paint_mode: &'static str,
    window_dc_attempted: bool,
    window_dc_qualified: bool,
    window_dc_owner_thread: Option<u32>,
    window_dc_fallback_release_succeeded: bool,
    window_dc_fallback_geometry_verified: bool,
    window_dc_release_succeeded: bool,
    window_dc_validate_succeeded: bool,
    window_dc_fallback_reason: Option<&'static str>,
    window_dc_coverage: Option<FirstWindowDcCoverage>,
    window_dc_get_begin_qpc: i64,
    window_dc_get_end_qpc: i64,
    window_dc_check_begin_qpc: i64,
    window_dc_check_end_qpc: i64,
    window_dc_fallback_release_begin_qpc: i64,
    window_dc_fallback_release_end_qpc: i64,
    window_dc_release_begin_qpc: i64,
    window_dc_release_end_qpc: i64,
    window_dc_validate_begin_qpc: i64,
    window_dc_validate_end_qpc: i64,
    native_bitmap: bool,
    whole_canvas: bool,
    crop_overlay: bool,
    window_preview_overlay: bool,
    window_preview_rect: Option<[u32; 4]>,
    full_canvas_required: bool,
    paint_rect: [i32; 4],
}
impl NativePaintTrace {
    fn new(hwnd: HWND) -> Self {
        Self {
            pid: std::process::id(),
            thread_id: unsafe { GetCurrentThreadId() },
            hwnd: hwnd.0 as usize,
            ..Default::default()
        }
    }
    #[inline]
    fn stamp<const TRACING: bool>(
        trace: &mut Option<&mut Self>,
        field: impl FnOnce(&mut Self) -> &mut i64,
    ) {
        if !TRACING {
            return;
        }
        if let Some(trace) = trace.as_deref_mut() {
            *field(trace) = crate::capture_standby::qpc();
        }
    }
    fn emit_after_ready(&self, ready_qpc: usize) {
        let mut frequency = 0_i64;
        let frequency_ok =
            unsafe { QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0;
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase":"startup_native_paint_timing",
            "pid":self.pid,"thread_id":self.thread_id,"hwnd":self.hwnd,
            "ready_qpc":ready_qpc,
            "qpc_frequency":if frequency_ok { Some(frequency) } else { None },
            "timing":self,
            "emitted_after_actual_surface_ready":true,
            "timing_scope":"paint/draw/successful-release/validate QPC belong to native_paint; window-dc get/check/fallback-release QPC belong to preceding native_show_window_dc_prepare; API wall boundaries, not GPU execution times; do not sum subsets with their containing stages",
            "bitmap_draw_cleanup_scope":"one existing StartupBitmap::blit_* call including temporary source DC create/select, BitBlt, GdiFlush and deselect/delete; CPU fallback contains original StretchDIBits loop; source DC cleanup is not separately timed",
            "overlay_scope":"crop/window-preview outside dimming and frame; handles only for confirmed crop; excludes final draw GdiFlush",
            "window_dc_scope":"first frozen paint only; get/check/fallback-release precede native_paint and belong to native_show_window_dc_prepare; successful release/validate belong to native_paint; BeginPaint/EndPaint fields remain zero in window-dc mode",
            "normal_qpc_calls_added":false,"normal_trace_vector_allocation_added":false
        }));
    }
}

/// GDI 使用客户区物理像素；全局负原点不参与裁剪。先生成外框/圆点行，再裁剪，
/// 不能把越界控制点挪回选区，也不能在裁剪后的边界重新生成描边。
fn selection_chrome_rect(rect: PixelBox, bounds: ScreenRect) -> Result<Option<RECT>> {
    let left = rect.left.max(0);
    let top = rect.top.max(0);
    let right = rect.right.min(i64::from(bounds.width));
    let bottom = rect.bottom.min(i64::from(bounds.height));
    if left >= right || top >= bottom {
        return Ok(None);
    }
    Ok(Some(RECT {
        left: i32::try_from(left)?,
        top: i32::try_from(top)?,
        right: i32::try_from(right)?,
        bottom: i32::try_from(bottom)?,
    }))
}

struct PaintResources {
    dim_dc: HDC,
    dim_bitmap: HBITMAP,
    previous: HGDIOBJ,
    border: HBRUSH,
    trace: bool,
    /// 交互重绘的离屏缓冲：底图、遮罩与外框先在此合成，再一次 BitBlt 到窗口，
    /// 避免 DWM 合成到“已贴亮底图、尚未压暗”的中间态而使选区外侧闪烁。首绘仍直画。
    back: std::cell::RefCell<Option<BackBuffer>>,
}

struct BackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: u32,
    height: u32,
}

impl BackBuffer {
    unsafe fn new(target: HDC, width: u32, height: u32) -> Option<Self> {
        let dc = CreateCompatibleDC(Some(target));
        if dc.is_invalid() {
            return None;
        }
        let bitmap = CreateCompatibleBitmap(target, width as i32, height as i32);
        if bitmap.is_invalid() {
            let _ = DeleteDC(dc);
            return None;
        }
        let previous = SelectObject(dc, bitmap.into());
        if previous.is_invalid() {
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            return None;
        }
        Some(Self {
            dc,
            bitmap,
            previous,
            width,
            height,
        })
    }
}

impl Drop for BackBuffer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
impl PaintResources {
    fn new(_hwnd: HWND) -> Result<Self> {
        let mut result = Self {
            dim_dc: HDC::default(),
            dim_bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            border: HBRUSH::default(),
            trace: std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some()
                || std::env::var_os("RUSTSHOT_SURFACE_QA_CASE").is_some(),
            back: std::cell::RefCell::new(None),
        };
        unsafe {
            result.dim_dc = CreateCompatibleDC(None);
            if result.dim_dc.is_invalid() {
                return Err("快速首屏 GDI DC 创建失败".into());
            }
            let info = crate::gdi_surface::bitmap_info(1, 1);
            let mut bits = std::ptr::null_mut();
            result.dim_bitmap = CreateDIBSection(
                Some(result.dim_dc),
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                None,
                0,
            )?;
            if bits.is_null() {
                return Err("快速首屏遮罩创建失败".into());
            }
            std::ptr::write(bits.cast::<u32>(), 0xff000000);
            result.previous = SelectObject(result.dim_dc, result.dim_bitmap.into());
            if result.previous.is_invalid() {
                return Err("快速首屏遮罩选择失败".into());
            }
            let color = crate::selection_chrome::COLOR;
            let bgr = ((color & 0xff) << 16) | (color & 0xff00) | (color >> 16);
            result.border = CreateSolidBrush(COLORREF(bgr));
            if result.border.is_invalid() {
                return Err("快速首屏边框创建失败".into());
            }
        }
        Ok(result)
    }
    fn draw(
        &self,
        dc: HDC,
        bounds: ScreenRect,
        planes: &[NativePlane],
        startup_bitmap: Option<&StartupBitmap>,
        state: &SurfaceState,
    ) -> Result<()> {
        self.draw_with_trace::<false>(dc, bounds, planes, startup_bitmap, state, None)
    }
    fn draw_with_trace<const TRACING: bool>(
        &self,
        dc: HDC,
        bounds: ScreenRect,
        planes: &[NativePlane],
        startup_bitmap: Option<&StartupBitmap>,
        state: &SurfaceState,
        mut trace: Option<&mut NativePaintTrace>,
    ) -> Result<()> {
        unsafe {
            let full = RECT {
                left: 0,
                top: 0,
                right: bounds.width as i32,
                bottom: bounds.height as i32,
            };
            let whole_canvas = startup_bitmap
                .is_some_and(|bitmap| uses_whole_canvas_bitmap(bitmap, bounds, planes));
            if let Some(trace) = trace.as_deref_mut() {
                trace.native_bitmap = startup_bitmap.is_some();
                trace.whole_canvas = whole_canvas;
            }
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.background_begin_qpc);
            // run_native 已核验客户区与本次物理范围；完整缓存同时恢复底图与黑色空隙。
            // 预热纯黑窗及没有该来源保证的 DIB/DDB 仍走原填黑路径。
            if !whole_canvas && FillRect(dc, &full, HBRUSH(GetStockObject(BLACK_BRUSH).0)) == 0 {
                return Err("快速首屏屏间空隙填充失败".into());
            }
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.background_end_qpc);
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| {
                &mut trace.bitmap_draw_cleanup_begin_qpc
            });
            if let Some(bitmap) = startup_bitmap {
                if whole_canvas {
                    bitmap.blit_full_canvas_to(dc, bounds)?;
                } else {
                    let regions = planes
                        .iter()
                        .map(|plane| (plane.rect, plane.rect))
                        .collect::<Vec<_>>();
                    // 同次冻结的各屏共享一次源 DC 和提交，空隙保持上面的黑色。
                    bitmap.blit_to(dc, &regions)?;
                }
            } else {
                for plane in planes {
                    let rect = plane.rect;
                    let image = plane
                        .image
                        .as_ref()
                        .ok_or("快速首屏 CPU 绘制缺少已验证的真实图像")?;
                    let pixels = image.as_bytes(0).ok_or("快速首屏冻结图像已失效")?;
                    let result = StretchDIBits(
                        dc,
                        rect.x as i32,
                        rect.y as i32,
                        rect.width as i32,
                        rect.height as i32,
                        0,
                        0,
                        rect.width as i32,
                        rect.height as i32,
                        Some(pixels.as_ptr().cast()),
                        &crate::gdi_surface::bitmap_info(rect.width, rect.height),
                        DIB_RGB_COLORS,
                        SRCCOPY,
                    );
                    if result == 0 || result == GDI_ERROR {
                        return Err("快速首屏像素绘制失败".into());
                    }
                }
            }
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| {
                &mut trace.bitmap_draw_cleanup_end_qpc
            });
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.overlay_begin_qpc);
            if let Some(r) = state
                .crop
                .or(state.window_preview)
                .filter(|r| r.valid(bounds.width, bounds.height))
            {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.crop_overlay = state.crop.is_some();
                    trace.window_preview_overlay = state.crop.is_none();
                    trace.window_preview_rect = state
                        .crop
                        .is_none()
                        .then_some([r.x, r.y, r.width, r.height]);
                }
                let (x, y, right, bottom) = (
                    r.x as i32,
                    r.y as i32,
                    (r.x + r.width) as i32,
                    (r.y + r.height) as i32,
                );
                for (left, top, width, height) in [
                    (0, 0, full.right, y),
                    (0, y, x, bottom - y),
                    (right, y, full.right - right, bottom - y),
                    (0, bottom, full.right, full.bottom - bottom),
                ] {
                    if width > 0
                        && height > 0
                        && !AlphaBlend(
                            dc,
                            left,
                            top,
                            width,
                            height,
                            self.dim_dc,
                            0,
                            0,
                            1,
                            1,
                            BLENDFUNCTION {
                                BlendOp: AC_SRC_OVER as u8,
                                BlendFlags: 0,
                                SourceConstantAlpha: MASK_ALPHA,
                                AlphaFormat: 0,
                            },
                        )
                        .as_bool()
                    {
                        return Err("快速首屏遮罩绘制失败".into());
                    }
                }
                let chrome = SelectionChrome::projected(r, 1., (0., 0.));
                let paint = |rect: PixelBox, brush: HBRUSH| -> Result<()> {
                    if let Some(rect) = selection_chrome_rect(rect, bounds)? {
                        if FillRect(dc, &rect, brush) == 0 {
                            return Err("快速首屏选区外框/控制点绘制失败".into());
                        }
                    }
                    Ok(())
                };
                for rect in chrome.frame() {
                    paint(rect, self.border)?;
                }
                if state.crop.is_some() {
                    let white = HBRUSH(GetStockObject(WHITE_BRUSH).0);
                    for handle in chrome.handles() {
                        if selection_chrome_rect(handle.bounds(), bounds)?.is_none() {
                            continue;
                        }
                        for row in handle.spans(0) {
                            paint(row, self.border)?;
                        }
                        for row in handle.spans(crate::selection_chrome::HANDLE_BORDER) {
                            paint(row, white)?;
                        }
                    }
                }
            } else if !AlphaBlend(
                dc,
                0,
                0,
                full.right,
                full.bottom,
                self.dim_dc,
                0,
                0,
                1,
                1,
                BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: MASK_ALPHA,
                    AlphaFormat: 0,
                },
            )
            .as_bool()
            {
                // 与 GPUI 无选区/无预览时的整屏暗层一致，交接时画面不再由亮变暗。
                return Err("快速首屏整屏遮罩绘制失败".into());
            }
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.overlay_end_qpc);
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.draw_flush_begin_qpc);
            if !GdiFlush().as_bool() {
                return Err("快速首屏 GDI 提交失败".into());
            }
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.draw_flush_end_qpc);
        }
        Ok(())
    }
}
impl PaintResources {
    /// 在离屏缓冲合成整幅画面后按更新区一次拷到窗口；缓冲不可用时退回直画。
    #[allow(clippy::too_many_arguments)]
    fn draw_buffered_with_trace<const TRACING: bool>(
        &self,
        dc: HDC,
        update: RECT,
        bounds: ScreenRect,
        planes: &[NativePlane],
        startup_bitmap: Option<&StartupBitmap>,
        state: &SurfaceState,
        trace: Option<&mut NativePaintTrace>,
    ) -> Result<()> {
        let mut back = self.back.borrow_mut();
        if back
            .as_ref()
            .is_none_or(|b| b.width != bounds.width || b.height != bounds.height)
        {
            *back = None;
            *back = unsafe { BackBuffer::new(dc, bounds.width, bounds.height) };
        }
        let Some(buffer) = back.as_ref() else {
            drop(back);
            return self.draw_with_trace::<TRACING>(
                dc,
                bounds,
                planes,
                startup_bitmap,
                state,
                trace,
            );
        };
        self.draw_with_trace::<TRACING>(buffer.dc, bounds, planes, startup_bitmap, state, trace)?;
        let width = update.right - update.left;
        let height = update.bottom - update.top;
        if width > 0 && height > 0 {
            unsafe {
                BitBlt(
                    dc,
                    update.left,
                    update.top,
                    width,
                    height,
                    Some(buffer.dc),
                    update.left,
                    update.top,
                    SRCCOPY,
                )
                .map_err(|e| format!("快速首屏离屏画面提交失败：{e}"))?;
                if !GdiFlush().as_bool() {
                    return Err("快速首屏离屏画面 GDI 提交失败".into());
                }
            }
        }
        Ok(())
    }
}
impl Drop for PaintResources {
    fn drop(&mut self) {
        // 先释放离屏缓冲，使下方 GDI 计数与提交覆盖它。
        self.back.get_mut().take();
        unsafe {
            let before = self
                .trace
                .then(|| GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS));
            let restore_ok = self.previous.is_invalid()
                || SelectObject(self.dim_dc, self.previous) == HGDIOBJ(self.dim_bitmap.0);
            let bitmap_deleted =
                self.dim_bitmap.is_invalid() || DeleteObject(self.dim_bitmap.into()).as_bool();
            let brush_deleted =
                self.border.is_invalid() || DeleteObject(self.border.into()).as_bool();
            let dc_deleted = self.dim_dc.is_invalid() || DeleteDC(self.dim_dc).as_bool();
            // 资源销毁也可能排入本线程 GDI batch；在线程返回前提交，不能靠另一个线程 flush。
            let flush_ok = GdiFlush().as_bool();
            if let Some(before) = before {
                let after = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
                crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_surface_resources_drop\",\"pid\":{},\"restore_ok\":{restore_ok},\"bitmap_deleted\":{bitmap_deleted},\"brush_deleted\":{brush_deleted},\"dc_deleted\":{dc_deleted},\"flush_ok\":{flush_ok},\"gdi_before\":{before},\"gdi_after\":{after},\"ok\":{}}}",
                    std::process::id(), restore_ok && bitmap_deleted && brush_deleted && dc_deleted && flush_ok));
            }
        }
    }
}

/// 自有设备 fixture 的真实绘制专项；不创建窗口、GPUI 或读取桌面。
/// 调用者须独占当前线程的 top-down 32bpp 自有 DIB/DC，保证 bits 指向其完整像素；
/// canonical_bgra 必须已逐像素核验为本次自有原图及黑色空隙，不能来自未知桌面内容。
pub(crate) unsafe fn verify_whole_canvas_paint_fixture(
    destination: HDC,
    destination_bitmap: HBITMAP,
    bits: *mut u8,
    bitmap: &StartupBitmap,
    bounds: ScreenRect,
    locals: &[Rect],
    canonical_bgra: &[u8],
) -> Result<()> {
    let bytes = (bounds.width as usize)
        .checked_mul(bounds.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("整画布重绘 fixture 像素长度溢出")?;
    if bounds.width != 480
        || bounds.height != 320
        || canonical_bgra.len() != bytes
        || destination.is_invalid()
        || destination_bitmap.is_invalid()
        || bits.is_null()
        || destination_bitmap.0 as usize == bitmap.object_id()
    {
        return Err("整画布重绘 fixture 自有目标或尺寸不符".into());
    }
    validate_native_planes(bounds, locals, 0, bitmap)?;
    let planes = locals
        .iter()
        .map(|&rect| NativePlane { rect, image: None })
        .collect::<Vec<_>>();
    if !uses_whole_canvas_bitmap(bitmap, bounds, &planes) {
        return Err("整画布重绘 fixture 没有生产整画布来源能力".into());
    }
    let mut header = DIBSECTION::default();
    let read = unsafe {
        GetObjectW(
            destination_bitmap.into(),
            std::mem::size_of::<DIBSECTION>() as i32,
            Some((&mut header as *mut DIBSECTION).cast()),
        )
    };
    if read != std::mem::size_of::<DIBSECTION>() as i32
        || unsafe { GetCurrentObject(destination, OBJ_BITMAP) } != destination_bitmap.into()
        || header.dsBm.bmWidth != bounds.width as i32
        || header.dsBm.bmHeight != bounds.height as i32
        || header.dsBm.bmWidthBytes != (bounds.width * 4) as i32
        || header.dsBm.bmPlanes != 1
        || header.dsBm.bmBitsPixel != 32
        || header.dsBm.bmBits != bits.cast()
        || header.dsBmih.biCompression != BI_RGB.0
    {
        return Err("整画布重绘 fixture 目标 DIB 选择、像素地址或跨度不符".into());
    }
    let gdi_before_prime = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    if gdi_before_prime == 0 {
        return Err("整画布重绘 fixture 无法取得自有 GDI 资源计数".into());
    }
    let blits_before_prime = bitmap.blit_count();
    let batches_before_prime = bitmap.batch_count();
    // 仅 QA 在基线之前覆盖同一 GDI API 的一次性初始化；独立资源正常 Drop。
    // 只改自有目标 (0,0) 一像素，不借用冻结 bitmap；A 的整画布复制会恢复它。
    let warm_resources = PaintResources::new(HWND::default())?;
    unsafe {
        if !AlphaBlend(
            destination,
            0,
            0,
            1,
            1,
            warm_resources.dim_dc,
            0,
            0,
            1,
            1,
            BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: MASK_ALPHA,
                AlphaFormat: 0,
            },
        )
        .as_bool()
        {
            return Err("整画布重绘 fixture 自有一像素 AlphaBlend 预备失败".into());
        }
        let one = RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        };
        if FrameRect(destination, &one, warm_resources.border) == 0
            || FillRect(destination, &one, HBRUSH(GetStockObject(WHITE_BRUSH).0)) == 0
            || !GdiFlush().as_bool()
        {
            return Err("整画布重绘 fixture 自有一像素 FrameRect/白填充/flush 预备失败".into());
        }
    }
    drop(warm_resources);
    let gdi_after_prime = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    if gdi_after_prime == 0
        || bitmap.blit_count() != blits_before_prime
        || bitmap.batch_count() != batches_before_prime
    {
        return Err("整画布重绘 fixture 预备改变了冻结复制计数或 GDI 计数失效".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "phase":"startup_whole_canvas_paint_fixture_prime","pid":std::process::id(),
            "native_cache_bitmap_id":bitmap.object_id().to_string(),"destination_bitmap_id":(destination_bitmap.0 as usize).to_string(),
            "gdi_before_prime":gdi_before_prime,"gdi_after_prime":gdi_after_prime,
            "source_blits_before_prime":blits_before_prime,"source_blits_after_prime":bitmap.blit_count(),
            "source_batches_before_prime":batches_before_prime,"source_batches_after_prime":bitmap.batch_count(),
            "source_copy_counts_unchanged":true,"own_destination_pixel":[0,0,1,1],
            "alpha_blend_completed":true,"frame_rect_completed":true,"white_fill_completed":true,"flush_completed":true,
            "warm_resources_dropped":true,"screen_capture_calls":0,"window_created":false,
            "gpui_created":false,"gpui_application_created":false,"global_input":false,"actual_ready_verified":false
        })
    );
    let gdi_before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    if gdi_before == 0 {
        return Err("整画布重绘 fixture 无法取得预备后的稳定 GDI 基线".into());
    }
    // 仍由生产构造器创建同一遮罩/蓝色描边；白填充使用系统 stock brush，无新增所有权。
    let resources = PaintResources::new(HWND::default())?;
    let mut editor = Editor::new(Rect::full(bounds.width, bounds.height), None);
    let mut state = SurfaceState {
        anchor: None,
        pointer: (150, 140),
        crop: None,
        window_preview: None,
        manual_preview: None,
        dragging: false,
        cancelled: false,
        toolbar_monitor: 0,
        pending_keys: Vec::new(),
        pending_keystrokes: Vec::new(),
        wheel_input: WheelAccumulator::default(),
        click_seed: None,
    };
    let crop_a = Rect::drag((40, 90), (220, 220), bounds.width, bounds.height);
    let crop_b = Rect::drag((20, 75), (300, 195), bounds.width, bounds.height);
    let before_blits = bitmap.blit_count();
    let before_batches = bitmap.batch_count();
    let bgr_at = |pixels: &[u8], x: usize, y: usize| -> [u8; 3] {
        let offset = (y * bounds.width as usize + x) * 4;
        [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
    };
    let chrome_color = crate::selection_chrome::COLOR;
    let chrome_bgr = [
        chrome_color as u8,
        (chrome_color >> 8) as u8,
        (chrome_color >> 16) as u8,
    ];
    let mut outside_once = None;
    for (step, (name, crop)) in [("A", Some(crop_a)), ("B", Some(crop_b)), ("None", None)]
        .into_iter()
        .enumerate()
    {
        editor.set_crop(crop);
        state.crop = editor.crop;
        resources.draw(destination, bounds, &planes, Some(bitmap), &state)?;
        if bitmap.blit_count() != before_blits + step + 1
            || bitmap.batch_count() != before_batches + step + 1
            || bitmap.last_batch_planes() != 1
        {
            return Err(format!("整画布重绘 fixture {name} 未实际提交单次 BitBlt/batch").into());
        }
        // draw 已在同线程完成 GdiFlush；这里只读取调用者独占的自有 DIB。
        let actual = unsafe { std::slice::from_raw_parts(bits, bytes) };
        if name == "None" {
            for (index, (pixel, original)) in actual
                .chunks_exact(4)
                .zip(canonical_bgra.chunks_exact(4))
                .enumerate()
            {
                // 无选区时与 GPUI 一致整屏暗化：每个像素恰为原图一次遮罩，旧框/圆点
                // 与选区内原图均被同一暗层覆盖，黑色空隙保持黑色，不累计加深。
                if pixel[..3]
                    .iter()
                    .zip(&original[..3])
                    .any(|(&value, &original)| value.abs_diff(masked_channel(original)) > 1)
                {
                    return Err(format!(
                        "清除选区后整画布未恰为一次整屏遮罩 ({},{})",
                        index % bounds.width as usize,
                        index / bounds.width as usize
                    )
                    .into());
                }
            }
        } else {
            let inside = bgr_at(actual, 150, 140);
            let outside = bgr_at(actual, 340, 290);
            let original_outside = bgr_at(canonical_bgra, 340, 290);
            if inside != bgr_at(canonical_bgra, 150, 140)
                || !outside
                    .iter()
                    .zip(original_outside)
                    .all(|(&value, original)| value < original)
                || bgr_at(actual, 200, 20) != [0, 0, 0]
            {
                return Err(format!("整画布重绘 fixture {name} 原图/遮罩/黑色空隙不符").into());
            }
            if name == "A" {
                if bgr_at(actual, 39, 140) != chrome_bgr
                    || bgr_at(actual, 40, 140) != bgr_at(canonical_bgra, 40, 140)
                    || bgr_at(actual, 34, 89) != chrome_bgr
                    || bgr_at(actual, 34, 84) == chrome_bgr
                    || bgr_at(actual, 34, 84) == [255, 255, 255]
                    || bgr_at(actual, 37, 87) != [255, 255, 255]
                    || bgr_at(actual, 40, 90) != [255, 255, 255]
                    || !bgr_at(actual, 260, 140)
                        .iter()
                        .zip(bgr_at(canonical_bgra, 260, 140))
                        .all(|(&value, original)| value < original)
                {
                    return Err(
                        "整画布重绘 fixture A 外侧蓝框/居中白底圆点/选区原图或将来 B 内的遮罩不符"
                            .into(),
                    );
                }
                outside_once = Some(outside);
            } else if bgr_at(actual, 40, 140) != bgr_at(canonical_bgra, 40, 140)
                || bgr_at(actual, 39, 140) != bgr_at(canonical_bgra, 39, 140)
                || bgr_at(actual, 37, 87) != bgr_at(canonical_bgra, 37, 87)
                || bgr_at(actual, 260, 140) != bgr_at(canonical_bgra, 260, 140)
                || bgr_at(actual, 19, 150) != chrome_bgr
                || bgr_at(actual, 20, 150) != bgr_at(canonical_bgra, 20, 150)
                || bgr_at(actual, 14, 74) != chrome_bgr
                || bgr_at(actual, 14, 69) == chrome_bgr
                || bgr_at(actual, 14, 69) == [255, 255, 255]
                || bgr_at(actual, 17, 72) != [255, 255, 255]
                || bgr_at(actual, 20, 75) != [255, 255, 255]
                || Some(outside) != outside_once
                || !bgr_at(actual, 150, 210)
                    .iter()
                    .zip(bgr_at(canonical_bgra, 150, 210))
                    .all(|(&value, original)| value < original)
            {
                return Err(
                    "整画布重绘 fixture B 旧边框/圆点/遮罩未恢复、新圆点偏离选区角或发生累计加深"
                        .into(),
                );
            }
        }
    }
    drop(resources);
    let gdi_after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    if gdi_after != gdi_before {
        return Err(format!("整画布重绘 fixture 稳定 GDI 计数不相等：预备前={gdi_before_prime}, 预备后={gdi_after_prime}, 被测前={gdi_before}, 被测后={gdi_after}").into());
    }
    println!(
        "{}",
        serde_json::json!({
            "phase":"startup_whole_canvas_paint_fixture_complete","pid":std::process::id(),
            "native_cache_bitmap_id":bitmap.object_id().to_string(),"destination_bitmap_id":(destination_bitmap.0 as usize).to_string(),
            "bounds":[bounds.left,bounds.top,bounds.width,bounds.height],"pixels":bounds.width as usize*bounds.height as usize,
            "crop_sequence":["A","B","None"],"production_paint_resources_draw":true,"whole_canvas_black_gaps_valid":true,
            "overlapping_crop_old_border_restored":true,"previously_masked_new_crop_pixels_restored":true,
            "selection_frame_outside_crop":true,"selection_handle_overlay_only":true,"selection_chrome_rgb":"#1498FF","selection_chrome_frame_physical_pixels":crate::selection_chrome::FRAME_WIDTH,
            "selection_handle_physical_pixels":crate::selection_chrome::HANDLE_SIZE,"selection_handle_circle_verified":true,"selection_handle_white_fill_verified":true,"previous_selection_handle_restored":true,
            "mask_did_not_accumulate":true,"cleared_selection_uniform_mask_and_gaps":true,
            "bitblts_delta":bitmap.blit_count()-before_blits,"batches_delta":bitmap.batch_count()-before_batches,
            "single_bitblt_and_batch_each_step":true,"native_reserved_alpha_ignored":true,
            "gdi_before_prime":gdi_before_prime,"gdi_after_prime":gdi_after_prime,
            "gdi_before_stable":gdi_before,"gdi_after_stable":gdi_after,"gdi_stable_equal":true,
            "paint_gdi_resources_restored":true,"screen_capture_calls":0,"window_created":false,
            "gpui_created":false,"gpui_application_created":false,"global_input":false,"actual_ready_verified":false
        })
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeIdleCursorSource {
    hwnd: usize,
    thread: u32,
    token: usize,
    bounds: ScreenRect,
}
struct NativeIdleCursorCandidate {
    input: crate::capture_cursor_nudge::IdlePreviewCursorSnapshot,
    source: NativeIdleCursorSource,
}
struct NativeCursorReadback {
    before: (i32, i32),
    target: (i32, i32),
    actual: (i32, i32),
}

struct Runtime {
    editor: Editor,
    state: SurfaceState,
    bounds: ScreenRect,
    planes: Vec<NativePlane>,
    startup_bitmap: Option<Arc<StartupBitmap>>,
    targets: Vec<WindowTarget>,
    /// 最后收到的真实客户区点；不以屏幕中心或边缘 clamp 点制造预览。
    preview_pointer: Option<(i32, i32)>,
    tolerance: f32,
    button_down: bool,
    click_gesture: ClickGesture,
    click_started: Option<((i32, i32), Instant)>,
    double_click_interval: Duration,
    double_click_tolerance: (i32, i32),
    target: Option<(HWND, u32)>,
    pending: Vec<CapturedInput>,
    deferred: VecDeque<CapturedInput>,
    early_output: StartupOutputQueue,
    ime_disabled: bool,
    native_arrow: crate::capture_cursor_nudge::NativeArrowNudge,
    native_idle_source: Option<NativeIdleCursorSource>,
    manual_click_preview: Option<(Rect, (i32, i32), bool)>,
    /// 本次手动预览接受的原始 Down；转入 CropGesture 后仍认领至 Up/取消。
    manual_cursor_origin: Option<(i32, i32)>,
    /// 成功移鼠的实际读回或真实路径曾越过容差，返回起点也不重获 click 资格。
    manual_cursor_dragged: bool,
    /// 普通无选区 New 接受的原 Down 和原 Native 来源，不借用 fixed Preview 的矩形标记。
    new_cursor_origin: Option<(i32, i32)>,
    new_cursor_owner: Option<NativeIdleCursorSource>,
    /// 只由成功 Set/Get 的实际非零读回置位，阻止旧自动目标/点击 seed 复活。
    new_cursor_moved: bool,
    /// 既有范围 Resize/Inside/Move 接受的原 Down 与同源身份，不借用 manual/New。
    adjust_cursor_origin: Option<(i32, i32)>,
    adjust_cursor_owner: Option<NativeIdleCursorSource>,
    /// 成功 Set/Get 的合法非零实际运动保留至 Up；失焦只能撤未来移动来源。
    adjust_cursor_moved: bool,
    monitor_snap_cycle: bool,
    preview_scale: f32,
}

impl Runtime {
    fn native_cursor_source(&self, hwnd: HWND, native: &NativeWindow) -> NativeIdleCursorSource {
        NativeIdleCursorSource {
            hwnd: hwnd.0 as usize,
            thread: native.thread_id,
            token: native.token,
            bounds: self.bounds,
        }
    }
    fn clear_native_idle(&mut self) {
        self.native_arrow.clear_idle();
        self.native_idle_source = None;
    }
    fn record_native_idle_commit(
        &mut self,
        event: &CapturedInput,
        source: NativeIdleCursorSource,
        commit: crate::capture_cursor_nudge::IdlePreviewCursorCommit,
    ) {
        if self.manual_cursor_state().claimed
            || self.new_cursor_claimed()
            || self.adjust_cursor_claimed()
        {
            return;
        }
        self.native_arrow
            .record_idle_commit(event.raw, event.modifiers, commit);
        if commit != crate::capture_cursor_nudge::IdlePreviewCursorCommit::Rejected
            && crate::capture_cursor_nudge::NativeArrowNudge::movement_down(
                event.raw,
                event.modifiers,
            )
            && event.raw.2 & (1 << 30) == 0
        {
            self.native_idle_source = Some(source);
        }
    }
    fn take_native_idle_candidate(
        &mut self,
        event: &CapturedInput,
        source: Option<NativeIdleCursorSource>,
    ) -> Option<NativeIdleCursorCandidate> {
        let source = source?;
        let eligible = event.raw.0 == WM_LBUTTONDOWN
            && self.target.is_none()
            && !self.state.cancelled
            && !self.button_down
            && self.native_idle_source == Some(source)
            && source.bounds == self.bounds
            && crate::capture_cursor_nudge::preview_movement_modifiers(event.modifiers)
            && crate::capture_cursor_nudge::preview_ready(
                &self.editor,
                true,
                true,
                self.has_pending_action_keys(),
                false,
            )
            && self.state.manual_preview.is_some_and(|rect| {
                crate::capture_monitor_snap::hit_preview(
                    rect,
                    startup_preview::client_point(event.raw.2),
                    self.preview_scale,
                )
            });
        if !eligible {
            return None;
        }
        self.native_idle_source = None;
        let input = self.native_arrow.idle_snapshot();
        if input.is_empty() {
            return None;
        }
        Some(NativeIdleCursorCandidate { input, source })
    }
    fn apply_native_idle_candidate(
        &mut self,
        candidate: Option<NativeIdleCursorCandidate>,
        source: Option<NativeIdleCursorSource>,
        physical_held: [bool; 8],
        owner_ready: bool,
    ) -> bool {
        self.clear_native_idle();
        let Some(candidate) = candidate else {
            return false;
        };
        if !owner_ready
            || source != Some(candidate.source)
            || candidate.source.bounds != self.bounds
            || self.target.is_some()
            || !self.manual_cursor_ready()
        {
            return false;
        }
        self.native_arrow.adopt_idle(candidate.input, physical_held)
    }
    fn adopt_native_idle_after_capture(
        &mut self,
        candidate: Option<NativeIdleCursorCandidate>,
        hwnd: HWND,
        live: Option<(&Shared, &NativeWindow)>,
    ) {
        if candidate.is_none() {
            return;
        }
        let Some((shared, native)) = live else {
            self.clear_native_idle();
            return;
        };
        let source = self.native_cursor_source(hwnd, native);
        let owner_ready = self
            .native_cursor_point(hwnd, shared, native)
            .ok()
            .flatten()
            .is_some();
        let held = if owner_ready {
            [0x25, 0x26, 0x27, 0x28, 0x41, 0x57, 0x44, 0x53]
                .map(|key| unsafe { GetAsyncKeyState(key) < 0 })
        } else {
            [false; 8]
        };
        // 捕获/焦点/交换逻辑按钮及会话在读物理 held 后再核；接纳本身不移鼠。
        let owner_ready = owner_ready
            && self
                .native_cursor_point(hwnd, shared, native)
                .ok()
                .flatten()
                .is_some();
        self.apply_native_idle_candidate(candidate, Some(source), held, owner_ready);
    }
    fn manual_cursor_state(&self) -> crate::capture_cursor_nudge::ManualPreviewCursorState {
        crate::capture_cursor_nudge::ManualPreviewCursorState {
            manual: self.state.manual_preview,
            click: self.manual_click_preview.map(|candidate| candidate.0),
            down: self.manual_cursor_origin,
            claimed: self.button_down && self.manual_cursor_origin.is_some(),
        }
    }
    fn manual_cursor_ready(&self) -> bool {
        crate::capture_cursor_nudge::manual_preview_cursor_ready(
            &self.editor,
            self.manual_cursor_state(),
            true,
            !self.state.cancelled,
            self.has_pending_action_keys(),
            false,
        )
    }
    fn clear_new_cursor(&mut self) {
        self.new_cursor_origin = None;
        self.new_cursor_owner = None;
        self.new_cursor_moved = false;
    }
    fn accept_new_cursor_down(
        &mut self,
        source: Option<NativeIdleCursorSource>,
        admitted_before_down: bool,
    ) {
        self.clear_new_cursor();
        if !admitted_before_down
            || self.manual_cursor_origin.is_some()
            || !crate::capture_cursor_nudge::new_crop_cursor_source(
                &self.editor,
                true,
                self.button_down,
            )
        {
            return;
        }
        let Some(source) = source else {
            return;
        };
        self.new_cursor_origin = Some(self.state.pointer);
        self.new_cursor_owner = Some(source);
    }
    fn new_cursor_claimed(&self) -> bool {
        self.button_down && self.new_cursor_origin.is_some()
    }
    fn new_cursor_ready(&self) -> bool {
        crate::capture_cursor_nudge::new_crop_cursor_ready(
            &self.editor,
            true,
            self.new_cursor_claimed() && self.manual_cursor_origin.is_none(),
            !self.state.cancelled && self.new_cursor_owner.is_some(),
            self.has_pending_action_keys(),
            false,
        )
    }
    fn clear_adjust_cursor(&mut self) {
        self.adjust_cursor_origin = None;
        self.adjust_cursor_owner = None;
        self.adjust_cursor_moved = false;
    }
    fn accept_adjust_cursor_down(
        &mut self,
        source: Option<NativeIdleCursorSource>,
        admitted_before_down: bool,
    ) {
        self.clear_adjust_cursor();
        if !admitted_before_down
            || self.manual_cursor_origin.is_some()
            || !crate::capture_cursor_nudge::crop_adjust_cursor_source(
                &self.editor,
                true,
                self.button_down,
            )
        {
            return;
        }
        let Some(source) = source else {
            return;
        };
        self.adjust_cursor_origin = Some(self.state.pointer);
        self.adjust_cursor_owner = Some(source);
    }
    fn adjust_cursor_claimed(&self) -> bool {
        self.button_down && self.adjust_cursor_origin.is_some()
    }
    fn adjust_cursor_ready(&self) -> bool {
        crate::capture_cursor_nudge::crop_adjust_cursor_ready(
            &self.editor,
            true,
            self.adjust_cursor_claimed() && self.manual_cursor_origin.is_none(),
            !self.state.cancelled && self.adjust_cursor_owner.is_some(),
            self.has_pending_action_keys(),
            false,
        )
    }
    fn observe_manual_cursor_readback(
        &mut self,
        before_global: (i32, i32),
        actual_global: (i32, i32),
    ) {
        if !self.button_down
            || actual_global == before_global
            || crate::capture_cursor_nudge::target_in_client(self.bounds, actual_global, (0, 0))
                .is_none()
        {
            return;
        }
        let Some(start) = self.manual_cursor_origin else {
            return;
        };
        if let Some(actual_local) = startup_preview::local_point(self.bounds, actual_global) {
            self.manual_cursor_dragged |= crate::capture_monitor_snap::preview_dragged(
                start,
                actual_local,
                self.preview_scale,
            );
        }
    }
    /// 只由实际 Set 成功且 Get 成功的路线调用；后续 owner 失效不撤销已发生的轨迹。
    fn record_native_cursor_readback(
        &mut self,
        event: &CapturedInput,
        source: NativeIdleCursorSource,
        readback: NativeCursorReadback,
        owner_ready: bool,
    ) {
        let manual_claimed = self.manual_cursor_state().claimed;
        let new_claimed = self.new_cursor_claimed();
        let adjust_claimed = self.adjust_cursor_claimed();
        let claimed = manual_claimed || new_claimed || adjust_claimed;
        if manual_claimed {
            self.observe_manual_cursor_readback(readback.before, readback.actual);
        }
        if new_claimed {
            self.new_cursor_moved |= crate::capture_cursor_nudge::new_crop_cursor_readback_moved(
                self.bounds,
                readback.before,
                readback.actual,
            );
        }
        if adjust_claimed {
            self.adjust_cursor_moved |=
                crate::capture_cursor_nudge::crop_adjust_cursor_readback_moved(
                    self.bounds,
                    readback.before,
                    readback.actual,
                );
        }
        let commit = crate::capture_cursor_nudge::idle_preview_cursor_commit(
            self.bounds,
            readback.before,
            readback.target,
            Some(readback.actual),
            true,
            owner_ready,
        );
        self.record_native_idle_commit(event, source, commit);
        if !owner_ready {
            self.clear_native_idle();
            if adjust_claimed {
                self.adjust_cursor_owner = None;
            }
            if claimed {
                self.native_arrow
                    .claim_manual_down(event.raw, event.modifiers);
            }
        }
    }
    fn native_cursor_point(
        &self,
        hwnd: HWND,
        shared: &Shared,
        native: &NativeWindow,
    ) -> Result<Option<(i32, i32)>> {
        // 阶段门先于 Editor/Win32 查询；targetSome 的 Editor 已经不再是当前状态。
        if self.target.is_some()
            || self.state.cancelled
            || !self.ime_disabled
            || shared.stop.load(Ordering::Acquire)
            || shared.finish.load(Ordering::Acquire)
            || shared.handed_off.load(Ordering::Acquire)
            || native._queue.dormant.load(Ordering::Acquire)
            || hwnd != native.hwnd
            || !shared
                .cancel_token
                .as_ref()
                .is_some_and(|token| token.session_is_live())
        {
            return Ok(None);
        }
        let manual_cursor_ready = self.manual_cursor_ready();
        let new_cursor_ready = self.new_cursor_ready();
        if new_cursor_ready
            && self.new_cursor_owner != Some(self.native_cursor_source(hwnd, native))
        {
            return Ok(None);
        }
        let adjust_cursor_ready = self.adjust_cursor_ready();
        if adjust_cursor_ready
            && self.adjust_cursor_owner != Some(self.native_cursor_source(hwnd, native))
        {
            return Ok(None);
        }
        if !manual_cursor_ready
            && !new_cursor_ready
            && !adjust_cursor_ready
            && !crate::capture_cursor_nudge::preview_ready(
                &self.editor,
                true,
                true,
                self.button_down || self.has_pending_action_keys(),
                false,
            )
        {
            return Ok(None);
        }
        unsafe {
            if native.token == 0
                || shared.hwnd.load(Ordering::Acquire) != hwnd.0 as usize
                || shared.thread_id.load(Ordering::Acquire) != native.thread_id
                || native.thread_id != GetCurrentThreadId()
                || !surface_window_owned(hwnd)
                || GetPropW(hwnd, w!("RustShot.StartupSurface.Token")).0 as usize != native.token
                || !owns_surface_pointer(hwnd)
                || GetFocus() != hwnd
            {
                return Ok(None);
            }
            if manual_cursor_ready || new_cursor_ready || adjust_cursor_ready {
                let (left, right) = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                    (VK_RBUTTON, VK_LBUTTON)
                } else {
                    (VK_LBUTTON, VK_RBUTTON)
                };
                if GetCapture() != hwnd
                    || !crate::capture_cursor_nudge::manual_preview_cursor_buttons_ready(
                        GetAsyncKeyState(left.0 as i32) < 0,
                        [right, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
                            .map(|key| GetAsyncKeyState(key.0 as i32) < 0),
                    )
                {
                    return Ok(None);
                }
            } else if [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
                .iter()
                .any(|key| GetAsyncKeyState(key.0 as i32) < 0)
            {
                return Ok(None);
            }
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client)?;
            let mut origin = POINT {
                x: client.left,
                y: client.top,
            };
            if !ClientToScreen(hwnd, &mut origin).as_bool() {
                return Err(windows::core::Error::from_win32().into());
            }
            if origin.x != self.bounds.left
                || origin.y != self.bounds.top
                || client
                    .right
                    .checked_sub(client.left)
                    .and_then(|v| u32::try_from(v).ok())
                    != Some(self.bounds.width)
                || client
                    .bottom
                    .checked_sub(client.top)
                    .and_then(|v| u32::try_from(v).ok())
                    != Some(self.bounds.height)
            {
                return Ok(None);
            }
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor)?;
            let point = (cursor.x, cursor.y);
            Ok(
                crate::capture_cursor_nudge::target_in_client(self.bounds, point, (0, 0))
                    .map(|_| point),
            )
        }
    }
    fn native_arrow_input(
        &mut self,
        hwnd: HWND,
        event: &CapturedInput,
        live: Option<(&Shared, &NativeWindow)>,
    ) -> bool {
        use crate::capture_cursor_nudge::{NativeArrowNudge, NativeArrowRoute};
        let source = live.map(|(_, native)| self.native_cursor_source(hwnd, native));
        if self.target.is_some() || live.is_none() || self.state.cancelled {
            self.clear_native_idle();
        } else if self.native_idle_source.is_some() && self.native_idle_source != source {
            // 不同原生窗口/token/冻结客户区不能恢复先前周期，旧 KeyUp 仍需消费。
            self.native_arrow.input_lost();
            self.clear_native_idle();
        }
        // 物理新周期先在真实状态过期，不能被后续资格拒绝/暂定 intent 丢弃而回退。
        self.native_arrow.begin_physical_down(event.raw);
        let claimed = self.manual_cursor_state().claimed
            || self.new_cursor_claimed()
            || self.adjust_cursor_claimed()
            || self
                .native_arrow
                .owns_claimed_down(event.raw, event.modifiers);
        // Up 收尾无需 OS/旧 Editor 资格；目标已交接或 drain 时仍清掉已领取的对应 Up。
        let point = if NativeArrowNudge::movement_down(event.raw, event.modifiers) {
            live.and_then(
                |(shared, native)| match self.native_cursor_point(hwnd, shared, native) {
                    Ok(point) => point,
                    Err(error) => {
                        eprintln!("原生鼠标微调资格失败，本次不移动：{error}");
                        None
                    }
                },
            )
        } else {
            None
        };
        let mut next = self.native_arrow.clone();
        let route = next.route(event.raw, event.modifiers, point.is_some());
        match route {
            NativeArrowRoute::Pass => {
                self.native_arrow = next;
                if NativeArrowNudge::movement_down(event.raw, event.modifiers) {
                    if point.is_none() {
                        self.clear_native_idle();
                    } else {
                        self.native_arrow.record_idle_commit(
                            event.raw,
                            event.modifiers,
                            crate::capture_cursor_nudge::IdlePreviewCursorCommit::Rejected,
                        );
                    }
                }
                claimed
                    && self
                        .native_arrow
                        .claim_manual_down(event.raw, event.modifiers)
            }
            NativeArrowRoute::ConsumeUp => {
                self.native_arrow = next;
                true
            }
            NativeArrowRoute::Move(delta) => {
                let Some((shared, native)) = live else {
                    self.native_arrow.reset_repeat();
                    self.clear_native_idle();
                    return claimed
                        && self
                            .native_arrow
                            .claim_manual_down(event.raw, event.modifiers);
                };
                // 同一条实时 FIFO 消息，最后复核窗口/会话/焦点；失败没有领取、仍走原队列。
                let current = match self.native_cursor_point(hwnd, shared, native) {
                    Ok(Some(current)) => current,
                    Ok(None) => {
                        self.native_arrow.reset_repeat();
                        self.clear_native_idle();
                        return claimed
                            && self
                                .native_arrow
                                .claim_manual_down(event.raw, event.modifiers);
                    }
                    Err(error) => {
                        self.native_arrow.reset_repeat();
                        self.clear_native_idle();
                        eprintln!("原生鼠标微调最终资格失败，本次不移动：{error}");
                        return claimed
                            && self
                                .native_arrow
                                .claim_manual_down(event.raw, event.modifiers);
                    }
                };
                let Some(target) =
                    crate::capture_cursor_nudge::target_in_client(self.bounds, current, delta)
                else {
                    self.native_arrow.reset_repeat();
                    self.clear_native_idle();
                    return claimed
                        && self
                            .native_arrow
                            .claim_manual_down(event.raw, event.modifiers);
                };
                self.native_arrow = next;
                if target != current {
                    if let Err(error) = unsafe { SetCursorPos(target.0, target.1) } {
                        self.native_arrow.reset_repeat();
                        self.clear_native_idle();
                        if claimed {
                            self.native_arrow
                                .claim_manual_down(event.raw, event.modifiers);
                        }
                        eprintln!("原生鼠标微调失败，本次已尝试且不重放：{error}");
                        return true;
                    }
                    let mut actual = POINT::default();
                    match unsafe { GetCursorPos(&mut actual) } {
                        Ok(()) => {
                            let owner_ready = self
                                .native_cursor_point(hwnd, shared, native)
                                .ok()
                                .flatten()
                                .is_some();
                            let source = self.native_cursor_source(hwnd, native);
                            self.record_native_cursor_readback(
                                event,
                                source,
                                NativeCursorReadback {
                                    before: current,
                                    target,
                                    actual: (actual.x, actual.y),
                                },
                                owner_ready,
                            );
                        }
                        Err(error) => {
                            self.clear_native_idle();
                            if claimed {
                                self.native_arrow
                                    .claim_manual_down(event.raw, event.modifiers);
                            } else {
                                self.native_arrow.reset_repeat();
                            }
                            eprintln!("原生预览微调读回失败，不补位移也不重放：{error}");
                        }
                    }
                } else {
                    // 最后一次实时 GetCursorPos 和 owner 门已通过，明确边界夹紧 no-op。
                    let source = self.native_cursor_source(hwnd, native);
                    let commit = crate::capture_cursor_nudge::idle_preview_cursor_commit(
                        self.bounds,
                        current,
                        target,
                        Some(current),
                        false,
                        true,
                    );
                    self.record_native_idle_commit(event, source, commit);
                }
                // 不修改 pointer/hover/preview receipt，等待实际 MouseMove 和原 paint 路线。
                true
            }
        }
    }
    fn refresh_window_preview(&mut self) {
        let eligible = !self.state.cancelled
            && self.target.is_none()
            && self.editor.crop.is_none()
            && crate::image_editor::screenshot_middle_pin_idle(
                &self.editor,
                self.button_down || self.has_pending_action_keys(),
                false,
            );
        let old = self.state.window_preview;
        self.state.window_preview = self.state.manual_preview.or_else(|| {
            // 原生首屏与 GPUI 共用冻结窗口候选，不等待最小 UIA 元素。
            let point = self.preview_pointer?;
            startup_preview::window_preview(
                Rect::full(self.bounds.width, self.bounds.height),
                Some(point),
                self.targets.iter().map(|target| target.rect),
                eligible,
            )
            .map(|rect| self.auto_target_rect(rect, point))
        });
        if old != self.state.window_preview {
            crate::uia_log::uia_log!(
                "原生首屏预览 {:?} pointer={:?} cancelled={} handoff={} crop={} button={} keys={:?} dragging={}",
                self.state.window_preview,
                self.preview_pointer,
                self.state.cancelled,
                self.target.is_some(),
                self.editor.crop.is_some(),
                self.button_down,
                self.pending
                    .iter()
                    .map(|event| (event.raw.0, event.raw.1))
                    .collect::<Vec<_>>(),
                self.editor.is_dragging()
            );
        }
    }

    fn auto_target_rect(&self, rect: Rect, point: (i32, i32)) -> Rect {
        startup_preview::auto_target_rect(rect, self.planes.iter().map(|plane| plane.rect), point)
    }

    fn publish(&mut self, shared: &Shared) {
        self.state.crop = self.editor.crop;
        self.state.dragging = self.button_down || self.editor.is_dragging();
        *shared.state.lock().unwrap_or_else(|e| e.into_inner()) = self.state.clone();
        // 外部验收读取实际编辑器状态；不以投递输入成功冒充框选已经生效。
        let window = HWND(shared.hwnd.load(Ordering::Acquire) as *mut _);
        unsafe {
            let crop = self.state.crop;
            let lt = crop.map_or(0, |r| (((r.x as usize) << 16) | r.y as usize) + 1);
            let rb = crop.map_or(0, |r| {
                ((((r.x + r.width) as usize) << 16) | (r.y + r.height) as usize) + 1
            });
            let _ = SetPropW(
                window,
                w!("RustShot.StartupSurface.CropLT"),
                Some(HANDLE(lt as *mut _)),
            );
            let _ = SetPropW(
                window,
                w!("RustShot.StartupSurface.CropRB"),
                Some(HANDLE(rb as *mut _)),
            );
            let _ = SetPropW(
                window,
                w!("RustShot.StartupSurface.Dragging"),
                Some(HANDLE(self.state.dragging as usize as *mut _)),
            );
        }
    }
    fn point(&self, value: isize) -> (i32, i32) {
        (
            (value as u16 as i16 as i32).clamp(0, self.bounds.width as i32),
            ((value >> 16) as u16 as i16 as i32).clamp(0, self.bounds.height as i32),
        )
    }
    fn monitor(&self, point: (i32, i32)) -> usize {
        self.planes
            .iter()
            .position(|plane| plane.rect.contains(point))
            .unwrap_or(self.state.toolbar_monitor)
    }
    fn queue_key(&mut self, event: CapturedInput) -> Result<()> {
        if matches!(event.raw.0, WM_KEYDOWN | WM_SYSKEYDOWN)
            && event.raw.2 & (1 << 30) != 0
            && event.native_monitor_snap().is_none()
        {
            return Ok(());
        }
        if self.state.pending_keys.len() >= KEY_LIMIT || self.pending.len() >= EVENT_LIMIT {
            return Err("快速首屏按键队列已满，需要切换到完整界面".into());
        }
        self.state.pending_keys.push(event.raw);
        if let Some(keystroke) = event.keystroke() {
            self.state.pending_keystrokes.push(keystroke.clone());
        }
        self.pending.push(event);
        Ok(())
    }
    fn has_pending_action_keys(&self) -> bool {
        self.pending.iter().enumerate().any(|(index, event)| {
            let key = event.raw.1;
            let pressed_here = || {
                self.pending[..index].iter().any(|earlier| {
                    matches!(earlier.raw.0, WM_KEYDOWN | WM_SYSKEYDOWN) && earlier.raw.1 == key
                })
            };
            // 触发截图的快捷键在首屏出现前已按下，这里只收到它松开的 KeyUp；
            // 松开触发键不是新操作，不能让首屏窗口预览先清空、交接后再出现。
            (matches!(event.raw.0, WM_KEYDOWN | WM_SYSKEYDOWN)
                || matches!(event.raw.0, WM_KEYUP | WM_SYSKEYUP) && pressed_here())
                && !startup_surface_input::is_modifier(key as u16)
                && !event.native_preview_confirm_consumed()
                && !event.native_text_consumed()
                && event.native_monitor_snap()
                    != Some(startup_surface_input::NativeMonitorSnap::Applied)
        })
    }
    fn queue_preview_confirm(&mut self, mut event: CapturedInput) -> Result<()> {
        let busy = self.button_down
            || self.has_pending_action_keys()
            || event.preview_confirm_mouse_held();
        let receipt = self.current_manual_receipt(&event);
        let accepted = self.early_output.offer_preview_confirm(
            &self.editor,
            Rect::full(self.bounds.width, self.bounds.height),
            receipt,
            busy,
        );
        if accepted || self.state.manual_preview.is_some() {
            self.early_output.consume_preview_confirm_cycle();
            event.consume_native_preview_confirm();
        }
        self.queue_key(event)
    }
    fn current_manual_receipt(
        &self,
        event: &CapturedInput,
    ) -> Option<startup_preview::PresentedWindowPreview> {
        event.presented_window_preview().filter(|receipt| {
            self.state
                .manual_preview
                .is_none_or(|manual| manual == receipt.rect)
        })
    }
    fn deferred_manual_preview(&self) -> bool {
        self.state.manual_preview.is_some()
            || self.deferred.iter().any(|event| {
                matches!(
                    event.native_monitor_snap(),
                    Some(startup_surface_input::NativeMonitorSnap::Pending { .. })
                )
            })
    }
    fn deferred_manual_receipt(
        &self,
        event: &CapturedInput,
    ) -> Option<startup_preview::PresentedWindowPreview> {
        if self.deferred.iter().any(|event| {
            matches!(
                event.native_monitor_snap(),
                Some(startup_surface_input::NativeMonitorSnap::Pending { .. })
            )
        }) {
            return None;
        }
        self.state.manual_preview.and_then(|manual| {
            event
                .presented_window_preview()
                .filter(|receipt| receipt.rect == manual)
        })
    }
    fn queue_monitor_snap(&mut self, hwnd: HWND, mut event: CapturedInput) -> Result<()> {
        event.retain_native_monitor_snap(false);
        if self.target.is_some() {
            return self.defer(event);
        }
        let eligible = self.editor.tool == crate::annotation::Tool::Crop
            && !self.state.cancelled
            && self.editor.lasso_draft.is_none()
            && !self.editor.watermark.has_preview()
            && self.editor.text_draft.is_none()
            && !self.editor.has_pending_drawing()
            && !self.has_pending_action_keys();
        let screens = self
            .planes
            .iter()
            .filter_map(|plane| {
                let left =
                    i32::try_from(i64::from(self.bounds.left) + i64::from(plane.rect.x)).ok()?;
                let top =
                    i32::try_from(i64::from(self.bounds.top) + i64::from(plane.rect.y)).ok()?;
                Some(ScreenRect {
                    left,
                    top,
                    width: plane.rect.width,
                    height: plane.rect.height,
                })
            })
            .collect::<Vec<_>>();
        let target = eligible
            .then(|| event.monitor_snap_point())
            .flatten()
            .and_then(|point| {
                crate::capture_monitor_snap::snap_target(
                    self.bounds,
                    &screens,
                    point,
                    self.state
                        .manual_preview
                        .or(event.monitor_snap_shown_preview()),
                )
            });
        if let Some(rect) = target {
            self.discard_preview_wheels();
            self.early_output.clear();
            self.clear_native_idle();
            if self.manual_cursor_origin.is_some()
                || self.new_cursor_claimed()
                || self.adjust_cursor_claimed()
            {
                self.native_arrow.input_lost();
            }
            self.editor.cancel_gesture();
            self.editor.set_crop(None);
            self.state.crop = None;
            self.state.manual_preview = Some(rect);
            self.state.window_preview = Some(rect);
            self.state.click_seed = None;
            self.click_started = None;
            self.manual_click_preview = None;
            self.manual_cursor_origin = None;
            self.manual_cursor_dragged = false;
            self.clear_new_cursor();
            self.clear_adjust_cursor();
            self.button_down = false;
            self.click_gesture.clear();
            self.state.anchor = None;
            self.state.dragging = false;
            self.state.wheel_input.reset();
            self.monitor_snap_cycle = true;
            event.retain_native_monitor_snap(true);
            unsafe {
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        self.queue_key(event)
    }
    fn queue_text(&mut self, mut event: CapturedInput) -> Result<()> {
        if let Some(shortcut) = event.text_shortcut_down() {
            let busy = self.button_down
                || self.has_pending_action_keys()
                || event.preview_confirm_mouse_held();
            let receipt = self.current_manual_receipt(&event);
            let accepted = self.early_output.offer_text(
                &self.editor,
                Rect::full(self.bounds.width, self.bounds.height),
                shortcut,
                receipt,
                busy,
            );
            if accepted || self.state.manual_preview.is_some() {
                self.early_output.consume_text_cycle(shortcut);
                event.consume_native_text();
            }
        }
        self.queue_key(event)
    }
    /// 原生滚轮只走一条路线：已完成本地中心调整，或按收到顺序交给 GPUI。
    fn discard_preview_wheels(&mut self) {
        // 原生选区变化不会在 GPUI 重放一遍，必须让之前的预览滚轮永久失效。
        // 工具键之后的未标记参数滚轮仍保留它们的原 FIFO。
        self.pending
            .retain(|event| !event.is_native_preview_wheel() && !event.is_native_preview_hover());
    }
    fn retain_preview_hover(&mut self, mut event: CapturedInput) -> Result<()> {
        // 只补足已保留滚轮之后收到的真实 hover，不把普通 Native Move 全量重放。
        if event.raw.0 != WM_MOUSEMOVE {
            return Ok(());
        }
        if self.button_down
            || self.editor.crop.is_some()
            || event.raw.1 & 0x73 != 0
            || event.deferred_edit_button_seen()
        {
            self.discard_preview_wheels();
            return Ok(());
        }
        if !self
            .pending
            .iter()
            .any(CapturedInput::is_native_preview_wheel)
        {
            return Ok(());
        }
        event.retain_preview_hover_for_handoff();
        if self
            .pending
            .last()
            .is_some_and(CapturedInput::is_native_preview_hover)
        {
            event.inherit_mouse_motion(self.pending.last().unwrap());
            *self.pending.last_mut().unwrap() = event;
        } else if self.pending.len() < EVENT_LIMIT {
            self.pending.push(event);
        } else {
            return Err("快速首屏待重放输入队列已满".into());
        }
        Ok(())
    }
    fn mouse_move(&mut self, event: CapturedInput) -> Result<()> {
        self.state.pointer = self.point(event.raw.2);
        if self.button_down {
            self.click_gesture.observe_move(&event);
            if let Some((_, start, shift)) = self.manual_click_preview {
                if !self.manual_cursor_dragged
                    && !event.crossed_preview_drag(start, self.preview_scale)
                {
                    return Ok(());
                }
                self.manual_cursor_dragged = true;
                self.manual_click_preview = None;
                self.state.manual_preview = None;
                self.editor
                    .pointer_down_with_shift(start, self.tolerance, shift);
            }
            self.editor
                .pointer_move(crate::capture_cursor_nudge::preview_crop_pointer(
                    &self.editor,
                    self.state.pointer,
                    event.modifiers.shift,
                    true,
                    self.manual_cursor_origin.is_some(),
                ));
        } else {
            self.retain_preview_hover(event)?;
        }
        Ok(())
    }
    fn wheel(&mut self, mut event: CapturedInput) -> Result<()> {
        let (_, key, value) = event.raw;
        self.state.pointer = (
            (value as u16 as i16 as i64 - self.bounds.left as i64)
                .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            ((value >> 16) as u16 as i16 as i64 - self.bounds.top as i64)
                .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        );
        if self.state.manual_preview.is_some() && !self.has_pending_action_keys() {
            // 固定 Preview 不再导航 UIA，也不会在交接后把刻度落入参数调整。
            return Ok(());
        }
        if self.has_pending_action_keys() {
            if self.pending.len() >= EVENT_LIMIT {
                return Err("快速首屏待重放输入队列已满".into());
            }
            // 必须同 pending 按键穿插入队；放 deferred 尾部会把后来的工具键移到滚轮之前。
            self.pending.push(event);
            return Ok(());
        }
        let busy = self.button_down || key & (MK_LBUTTON.0 | MK_RBUTTON.0) as usize != 0;
        if crate::image_editor::screenshot_preview_pin_idle(&self.editor, busy, false)
            && self.editor.canvas.contains(self.state.pointer)
            && event.retain_preview_wheel_for_handoff()
        {
            if self.pending.len() >= EVENT_LIMIT {
                return Err("快速首屏待重放输入队列已满".into());
            }
            // 与后续工具键共用原 pending 前缀；不放到 targetSome 的 deferred 尾部。
            self.pending.push(event);
            return Ok(());
        }
        if let Some(notches) = event.crop_wheel_notches() {
            let busy = self.button_down || key & (MK_LBUTTON.0 | MK_RBUTTON.0) as usize != 0;
            adjust_startup_crop_wheel(
                &mut self.editor,
                &mut self.state.wheel_input,
                notches,
                self.state.pointer,
                self.tolerance,
                busy,
                event.received_at,
            );
        }
        Ok(())
    }
    fn defer(&mut self, mut event: CapturedInput) -> Result<()> {
        if event.raw.0 == WM_MOUSEMOVE
            && self
                .deferred
                .back()
                .is_some_and(|last| last.raw.0 == WM_MOUSEMOVE)
        {
            event.inherit_mouse_motion(self.deferred.back().unwrap());
            *self.deferred.back_mut().unwrap() = event;
        } else if self.deferred.len() < EVENT_LIMIT {
            self.deferred.push_back(event);
        } else {
            return Err("快速首屏交接输入队列已满".into());
        }
        Ok(())
    }
    fn cancel(&mut self, hwnd: HWND, reselect: bool) -> bool {
        self.discard_preview_wheels();
        self.native_arrow.input_lost();
        self.clear_native_idle();
        self.early_output.clear();
        self.state.window_preview = None;
        self.state.click_seed = None;
        self.click_started = None;
        self.manual_click_preview = None;
        self.manual_cursor_origin = None;
        self.manual_cursor_dragged = false;
        self.clear_new_cursor();
        self.clear_adjust_cursor();
        let had_manual = self.state.manual_preview.take().is_some();
        let closed = if reselect && had_manual {
            self.editor.cancel_gesture();
            self.editor.set_crop(None);
            false
        } else if self.editor.cancel_gesture() {
            false
        } else if reselect && self.editor.crop.is_some() {
            self.editor.set_crop(None);
            false
        } else {
            self.state.cancelled = true;
            true
        };
        self.button_down = false;
        self.click_gesture.clear();
        self.state.wheel_input.reset();
        self.state.anchor = None;
        unsafe {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
        }
        closed
    }
    fn input(&mut self, hwnd: HWND, event: CapturedInput) -> Result<bool> {
        // 关闭尾部 drain 和现有 CPU 路由调用没有实时副作用资格。
        self.input_with_arrow(hwnd, event, None)
    }
    fn input_with_arrow(
        &mut self,
        hwnd: HWND,
        mut event: CapturedInput,
        live: Option<(&Shared, &NativeWindow)>,
    ) -> Result<bool> {
        let (message, key, value) = event.raw;
        // 两层非键路线会先清加速；只有此真实单次 LeftDown 的同源 proof 可暂存。
        let source = live.map(|(_, native)| self.native_cursor_source(hwnd, native));
        let idle_candidate = self.take_native_idle_candidate(&event, source);
        let movement_cycle = matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP)
            && matches!(key, 0x25 | 0x26 | 0x27 | 0x28 | 0x41 | 0x57 | 0x44 | 0x53);
        let preserves_idle =
            crate::capture_cursor_nudge::preview_movement_modifiers(event.modifiers)
                && (movement_cycle
                    || crate::capture_cursor_nudge::NativeArrowNudge::shift_transition(
                        event.raw,
                        event.modifiers,
                    )
                    || matches!(message, WM_MOUSEMOVE | WM_PAINT));
        if !preserves_idle || live.is_none() || self.target.is_some() || self.state.cancelled {
            self.clear_native_idle();
        }
        if !crate::capture_cursor_nudge::NativeArrowNudge::movement_down(event.raw, event.modifiers)
            && !crate::capture_cursor_nudge::NativeArrowNudge::shift_transition(
                event.raw,
                event.modifiers,
            )
            && !matches!(message, WM_MOUSEMOVE | WM_PAINT)
        {
            self.native_arrow.reset_repeat();
        }
        if crate::capture_cursor_nudge::NativeArrowNudge::loses_input(event.raw) {
            self.native_arrow.input_lost();
            self.clear_native_idle();
            // 失焦不可重用旧范围手势的移动来源；原 Down/实际轨迹保留给 Up 和旧键归属。
            self.new_cursor_owner = None;
            self.adjust_cursor_owner = None;
            if !text_loss_is_own_handoff(event.raw, self.target) {
                self.monitor_snap_cycle = false;
                self.early_output.invalidate_text();
            }
            return Ok(false);
        }
        if self.monitor_snap_cycle && matches!(message, WM_KEYUP | WM_SYSKEYUP) && key == 0x41 {
            self.monitor_snap_cycle = false;
            event.retain_native_monitor_snap(true);
            if self.target.is_some() {
                self.defer(event)?;
            } else {
                self.queue_key(event)?;
            }
            return Ok(false);
        }
        // 真实 held Enter 不能经 GPUI dispatch_keystroke 重建成首次按下。
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) && key == 0x0d && value & (1 << 30) != 0 {
            return Ok(false);
        }
        if self.early_output.take_text_cycle(message, key, value) {
            return Ok(false);
        }
        if self.early_output.take_preview_confirm_release(message, key) {
            return Ok(false);
        }
        let preview_confirm = event.preview_confirm_down();
        let text_shortcut = event.text_shortcut_down();
        if event.deferred_edit_button_seen() {
            self.early_output.invalidate_text();
        }
        // targetSome 的旧 Editor 已不是最新状态；必须在 defer 前撤销旧范围请求。
        if text_shortcut.is_some() {
            self.early_output.observe_text_down();
        } else if preview_confirm {
            self.early_output.observe_preview_confirm_down();
        } else {
            self.early_output
                .observe_input(message, key, self.target.is_some());
        }
        if self.target.is_none() && startup_surface_input::breaks_click_sequence(event.raw) {
            self.state.click_seed = None;
            self.click_started = None;
        }
        // 这里只处理关闭语义，不能让原生吞掉系统 Alt+F4。
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) && key == 0x73 && event.modifiers.alt {
            self.state.cancelled = true;
            return Ok(true);
        }
        if self.native_arrow_input(hwnd, &event, live) {
            return Ok(false);
        }
        if startup_surface_input::monitor_snap_down(event.raw, event.modifiers) {
            self.queue_monitor_snap(hwnd, event)?;
            return Ok(false);
        }
        if self.target.is_some() {
            // 已合并的未知拖动仍属于编辑边界；不能让后面的纯 hover 重开预览资格。
            if event.deferred_edit_button_seen() {
                self.early_output.invalidate_deferred_preview();
            }
            if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) && key == 0x1b
                || message == WM_RBUTTONDOWN
                    && self.state.crop.is_none()
                    && !self.deferred_manual_preview()
                || message == WM_CLOSE
            {
                self.state.cancelled = true;
                return Ok(true);
            }
            if is_handoff_input(message, value, hwnd) {
                if let Some(shortcut) = text_shortcut {
                    let manual = self.deferred_manual_preview();
                    let receipt = if manual {
                        self.deferred_manual_receipt(&event)
                    } else {
                        event.presented_window_preview()
                    };
                    if self.early_output.offer_deferred_text(
                        Rect::full(self.bounds.width, self.bounds.height),
                        shortcut,
                        receipt,
                        event.preview_confirm_mouse_held(),
                    ) {
                        event.consume_native_text();
                    } else {
                        if manual {
                            self.early_output.consume_text_cycle(shortcut);
                            event.consume_native_text();
                        }
                        self.early_output.invalidate_deferred_preview();
                    }
                }
                if preview_confirm {
                    let manual = self.deferred_manual_preview();
                    let receipt = if manual {
                        self.deferred_manual_receipt(&event)
                    } else {
                        event.presented_window_preview()
                    };
                    if self.early_output.offer_deferred_preview_confirm(
                        Rect::full(self.bounds.width, self.bounds.height),
                        receipt,
                        event.preview_confirm_mouse_held(),
                    ) {
                        event.consume_native_preview_confirm();
                    } else {
                        // 拒绝 Enter 撤销 fence；先清资格再登记周期，反序会清掉 Enter Up 标记。
                        self.early_output.invalidate_deferred_preview();
                        if manual {
                            self.early_output.consume_preview_confirm_cycle();
                            event.consume_native_preview_confirm();
                        }
                    }
                }
                if matches!(message, WM_MBUTTONDOWN | WM_MBUTTONDBLCLK) {
                    // 此处旧 Editor 已停用。只读取首次 handoff 建立的 fence 和入队快照。
                    let busy = key & (MK_LBUTTON.0 | MK_RBUTTON.0) as usize != 0;
                    let manual = self.deferred_manual_preview();
                    let manual_receipt = self.deferred_manual_receipt(&event);
                    let confirmed_owned = self.early_output.owns_deferred_confirmed_middle(
                        Rect::full(self.bounds.width, self.bounds.height),
                        event.presented_confirmed_selection(),
                    );
                    let confirmed = self.early_output.offer_deferred_confirmed(
                        Rect::full(self.bounds.width, self.bounds.height),
                        event.presented_confirmed_selection(),
                        startup_preview::client_point(value),
                        busy,
                    );
                    let admitted = if confirmed {
                        true
                    } else if manual {
                        self.early_output.offer_deferred_manual_preview(
                            Rect::full(self.bounds.width, self.bounds.height),
                            manual_receipt,
                            startup_preview::client_point(value),
                            busy,
                        )
                    } else {
                        self.early_output.offer_deferred_preview(
                            Rect::full(self.bounds.width, self.bounds.height),
                            event.presented_window_preview(),
                            startup_preview::client_point(value),
                            busy,
                        )
                    };
                    if confirmed_owned || admitted || manual {
                        // 原确认范围即使输出已撤销仍拥有该输入，不能借晚到 Chrome/新 crop 再解释。
                        event.consume_native_preview_middle();
                    }
                }
                self.defer(event)?;
            }
            return Ok(false);
        }
        let old_crop = self.editor.crop;
        let old_preview = self.state.window_preview;
        if matches!(
            message,
            WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONDBLCLK
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_RBUTTONDBLCLK
                | WM_MBUTTONDOWN
                | WM_MBUTTONDBLCLK
        ) {
            self.preview_pointer = Some(startup_preview::client_point(value));
        } else if matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
            self.preview_pointer =
                startup_preview::local_point(self.bounds, startup_preview::client_point(value));
        }
        match message {
            WM_MOUSEMOVE => self.mouse_move(event)?,
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                // 新鼠标手势不承接先前闲置/其它手势的持键位移；对应旧KeyUp仍归本端。
                self.native_arrow.input_lost();
                self.clear_new_cursor();
                self.clear_adjust_cursor();
                self.discard_preview_wheels();
                self.state.pointer = self.point(value);
                self.state.click_seed = None;
                let busy = self.button_down || self.has_pending_action_keys();
                // 保存原 Down 前资格；pointer_down 的 finish/清选择不能给草稿/忙碌补来源。
                let new_cursor_admitted = crate::capture_cursor_nudge::preview_ready(
                    &self.editor,
                    true,
                    !self.state.cancelled,
                    busy,
                    false,
                );
                let adjust_cursor_admitted =
                    crate::capture_cursor_nudge::crop_adjust_cursor_admission(
                        &self.editor,
                        true,
                        !self.state.cancelled,
                        busy,
                        false,
                    );
                if message == WM_LBUTTONDBLCLK
                    && crate::capture_double_click::ordinary_double_click(
                        &self.editor,
                        Rect::full(self.bounds.width, self.bounds.height),
                        2,
                        true,
                        event.modifiers.modified(),
                        busy,
                        false,
                    ) == crate::capture_double_click::DoubleClickAction::Confirm
                {
                    self.click_started = None;
                    self.early_output.offer(
                        StartupOutputKind::Confirm,
                        &self.editor,
                        Rect::full(self.bounds.width, self.bounds.height),
                        busy,
                    );
                    return Ok(false);
                }
                self.state.anchor = Some(self.state.pointer);
                self.button_down = true;
                self.click_gesture.start(self.state.pointer);
                self.click_started = Some((self.state.pointer, event.received_at));
                self.state.wheel_input.reset();
                self.manual_click_preview = self
                    .state
                    .manual_preview
                    .filter(|rect| {
                        crate::capture_monitor_snap::hit_preview(
                            *rect,
                            startup_preview::client_point(value),
                            self.preview_scale,
                        )
                    })
                    .map(|rect| {
                        (
                            rect,
                            startup_preview::client_point(value),
                            event.modifiers.shift,
                        )
                    });
                self.manual_cursor_origin = self.manual_click_preview.map(|candidate| candidate.1);
                self.manual_cursor_dragged = false;
                if self.manual_click_preview.is_none() {
                    self.state.manual_preview = None;
                    self.editor.pointer_down_with_shift(
                        self.state.pointer,
                        self.tolerance,
                        event.modifiers.shift,
                    );
                }
                unsafe {
                    SetCapture(hwnd);
                }
                self.accept_new_cursor_down(source, new_cursor_admitted);
                self.accept_adjust_cursor_down(source, adjust_cursor_admitted);
                self.adopt_native_idle_after_capture(idle_candidate, hwnd, live);
            }
            WM_LBUTTONUP => {
                self.state.pointer = self.point(value);
                if self.button_down {
                    if self.manual_cursor_origin.is_some()
                        || self.new_cursor_claimed()
                        || self.adjust_cursor_claimed()
                    {
                        // 鼠标手势结束停止持键位移，对应方向 KeyUp 仍属于原周期。
                        self.native_arrow.input_lost();
                        self.clear_native_idle();
                    }
                    let clicked = self.click_gesture.finish(self.state.pointer)
                        && !self.manual_cursor_dragged
                        && !self.new_cursor_moved
                        && !self.adjust_cursor_moved;
                    let manual_confirmed =
                        if let Some((rect, start, shift)) = self.manual_click_preview.take() {
                            self.state.manual_preview = None;
                            if !self.manual_cursor_dragged
                                && !crate::capture_monitor_snap::preview_dragged(
                                    start,
                                    startup_preview::client_point(value),
                                    self.preview_scale,
                                )
                            {
                                self.editor.set_crop(Some(rect));
                                true
                            } else {
                                self.editor
                                    .pointer_down_with_shift(start, self.tolerance, shift);
                                false
                            }
                        } else {
                            false
                        };
                    if !manual_confirmed {
                        self.editor
                            .pointer_up(crate::capture_cursor_nudge::preview_crop_pointer(
                                &self.editor,
                                self.state.pointer,
                                event.modifiers.shift,
                                true,
                                self.manual_cursor_origin.is_some(),
                            ));
                    }
                    if self.editor.crop.is_none() && clicked && !manual_confirmed {
                        if let Some(target) = self
                            .targets
                            .iter()
                            .find(|t| t.rect.contains(self.state.pointer))
                        {
                            let rect = self.auto_target_rect(target.rect, self.state.pointer);
                            self.editor.set_crop(Some(rect));
                        }
                    }
                    let end = self.monitor(self.state.pointer);
                    self.state.toolbar_monitor = self.editor.crop.map_or(end, |crop| {
                        let screens: Vec<Rect> =
                            self.planes.iter().map(|plane| plane.rect).collect();
                        crate::toolbar_layout::anchor_monitor(crop, end, &screens)
                    });
                    self.button_down = false;
                    self.manual_cursor_origin = None;
                    self.manual_cursor_dragged = false;
                    self.clear_new_cursor();
                    self.clear_adjust_cursor();
                    self.state.anchor = None;
                    self.state.click_seed = self.click_started.take().and_then(|(point, at)| {
                        (clicked && !self.has_pending_action_keys()).then(|| {
                            ClickSeed::new(
                                point,
                                at,
                                self.double_click_interval,
                                self.double_click_tolerance,
                            )
                        })
                    });
                    unsafe {
                        if GetCapture() == hwnd {
                            let _ = ReleaseCapture();
                        }
                    }
                }
            }
            WM_RBUTTONDOWN | WM_RBUTTONDBLCLK if self.cancel(hwnd, true) => {
                return Ok(true);
            }
            WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => {
                let busy = self.button_down
                    || self.has_pending_action_keys()
                    || key & MK_LBUTTON.0 as usize != 0;
                let canvas = Rect::full(self.bounds.width, self.bounds.height);
                if self.editor.crop.is_some() {
                    self.early_output
                        .offer(StartupOutputKind::Pin, &self.editor, canvas, busy);
                } else {
                    let receipt = self.current_manual_receipt(&event);
                    if self.state.manual_preview.is_some() {
                        self.early_output.offer_manual_preview(
                            &self.editor,
                            canvas,
                            receipt,
                            startup_preview::client_point(value),
                            busy,
                        );
                    } else {
                        self.early_output.offer_preview(
                            &self.editor,
                            canvas,
                            receipt,
                            startup_preview::client_point(value),
                            busy,
                        );
                    }
                }
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                self.wheel(event)?;
            }
            WM_KEYDOWN | WM_SYSKEYDOWN
                if key == 0x1b && value & (1 << 30) == 0 && self.cancel(hwnd, false) =>
            {
                return Ok(true);
            }
            // Esc 的 held／未退出事件仍归本分支，不落通用 queue_key。
            WM_KEYDOWN | WM_SYSKEYDOWN if key == 0x1b => {}
            WM_KEYUP | WM_SYSKEYUP if key == 0x1b => {}
            WM_CHAR | WM_SYSCHAR if key == 0x1b => {}
            WM_KEYDOWN if text_shortcut.is_some() => self.queue_text(event)?,
            WM_KEYDOWN if preview_confirm => self.queue_preview_confirm(event)?,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => self.queue_key(event)?,
            WM_CHAR | WM_SYSCHAR => {}
            WM_CAPTURECHANGED if self.button_down && value as usize != hwnd.0 as usize => {
                if self.manual_cursor_origin.is_some()
                    || self.new_cursor_claimed()
                    || self.adjust_cursor_claimed()
                {
                    self.native_arrow.input_lost();
                    self.clear_native_idle();
                }
                self.editor.cancel_gesture();
                self.button_down = false;
                self.click_gesture.clear();
                self.click_started = None;
                self.state.click_seed = None;
                self.state.wheel_input.reset();
                self.state.anchor = None;
                self.manual_click_preview = None;
                self.manual_cursor_origin = None;
                self.manual_cursor_dragged = false;
                self.clear_new_cursor();
                self.clear_adjust_cursor();
            }
            WM_CLOSE => {
                self.state.cancelled = true;
                return Ok(true);
            }
            _ => {}
        }
        self.refresh_window_preview();
        if old_crop != self.editor.crop {
            self.early_output.observe_crop(old_crop, self.editor.crop);
        }
        if old_crop != self.editor.crop || old_preview != self.state.window_preview {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        update_surface_cursor(hwnd, self.editor.hover(self.state.pointer, self.tolerance));
        Ok(false)
    }
    fn handoff(&mut self, hwnd: HWND, target: HWND) -> Result<Option<SurfaceState>> {
        if self.target.is_some() {
            return Ok(Some(self.state.clone()));
        }
        if self.button_down || self.editor.is_dragging() {
            return Ok(None);
        }
        // 排队但尚未消费的 mouse-down 不能被一个 command 抢先截断。
        let mut pending = MSG::default();
        if unsafe { PeekMessageW(&mut pending, None, WM_MOUSEFIRST, WM_MOUSELAST, PM_NOREMOVE) }
            .as_bool()
        {
            return Ok(None);
        }
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(target, Some(&mut pid)) };
        let mut rect = RECT::default();
        let mut client = RECT::default();
        unsafe {
            GetWindowRect(target, &mut rect)?;
            GetClientRect(target, &mut client)?;
        }
        if thread == 0
            || pid != std::process::id()
            || rect.left != self.bounds.left
            || rect.top != self.bounds.top
            || rect.right - rect.left != self.bounds.width as i32
            || rect.bottom - rect.top != self.bounds.height as i32
            || client.right != self.bounds.width as i32
            || client.bottom != self.bounds.height as i32
        {
            return Err("完整截图窗口与快速首屏的物理坐标不一致".into());
        }
        let preview_busy = self.button_down || self.has_pending_action_keys();
        // 所有 HWND/PID/TID/物理范围门已通过；重复 handoff 在入口提前返回，不重开 fence。
        self.early_output.begin_handoff_preview(
            &self.editor,
            Rect::full(self.bounds.width, self.bounds.height),
            preview_busy,
        );
        self.target = Some((target, thread));
        crate::uia_log::uia_log!(
            "原生首屏交接 预览={:?} crop={:?}",
            self.state.window_preview,
            self.editor.crop
        );
        self.native_arrow.input_lost();
        self.clear_native_idle();
        self.clear_new_cursor();
        self.clear_adjust_cursor();
        self.state.dragging = false;
        self.state.anchor = None;
        self.click_gesture.clear();
        self.click_started = None;
        unsafe {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
        }
        Ok(Some(self.state.clone()))
    }
}

fn adjust_startup_crop_wheel(
    editor: &mut Editor,
    wheel: &mut WheelAccumulator,
    notches: f32,
    pointer: (i32, i32),
    _tolerance: f32,
    button_down: bool,
    now: Instant,
) -> bool {
    if !crate::selection_wheel::center_ready(editor, button_down, false)
        || editor.crop.is_none()
        || !editor.canvas.contains(pointer)
    {
        wheel.reset();
        return false;
    }
    let (steps, _) = wheel.push_selection_center(notches, now);
    editor.adjust_crop_from_center(steps)
}

/// 首次 handoff 已核目标 HWND/PID/TID/物理范围；只保留这份目标的精确焦点转移。
/// 无目标、外部/未知失焦、CancelMode 与 ActivateApp(false) 都不能借交接豁免。
fn text_loss_is_own_handoff(raw: RawInput, target: Option<(HWND, u32)>) -> bool {
    let Some((target, _)) = target.filter(|(target, thread)| !target.is_invalid() && *thread != 0)
    else {
        return false;
    };
    match raw {
        (WM_KILLFOCUS, next, _) => next == target.0 as usize,
        (WM_ACTIVATE, state, next) if state & 0xffff == WA_INACTIVE as usize => {
            next as usize == target.0 as usize
        }
        _ => false,
    }
}

fn is_handoff_input(message: u32, value: isize, source: HWND) -> bool {
    is_input(message) || message == WM_CAPTURECHANGED && value as usize != source.0 as usize
}

fn is_input(message: u32) -> bool {
    matches!(
        message,
        WM_LBUTTONDOWN
            | WM_LBUTTONDBLCLK
            | WM_LBUTTONUP
            | WM_MOUSEMOVE
            | WM_RBUTTONDOWN
            | WM_RBUTTONDBLCLK
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONDBLCLK
            | WM_MBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
            | WM_KEYDOWN
            | WM_KEYUP
            | WM_SYSKEYDOWN
            | WM_SYSKEYUP
            | WM_CHAR
            | WM_SYSCHAR
    )
}

struct NativePrepared {
    native: NativeWindow,
    resources: PaintResources,
    ime_disabled: bool,
    physical_bounds: ScreenRect,
    cloaked: bool,
    layered_configured: bool,
    compositor_error: Option<String>,
    synthetic_readback_prewarmed: bool,
}

fn prepare_native(
    bounds: ScreenRect,
    stages: &mut Vec<(&'static str, f64)>,
) -> Result<NativePrepared> {
    let trace_stages = std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
    if unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .0
        .is_null()
    {
        return Err("快速首屏不能使用物理像素 DPI 坐标".into());
    }
    let stage_started = trace_stages.then(Instant::now);
    // 此线程只有框选，不接收文字；避免建立无用的 IME 上下文，不影响 GPUI 线程。
    let ime_disabled = unsafe { ImmDisableIME(GetCurrentThreadId()) }.as_bool();
    record_native_stage(stages, "native_ime_disable", stage_started);
    let stage_started = trace_stages.then(Instant::now);
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    let name = w!("RustShot.StartupSurface");
    let cursors = load_surface_cursors()?;
    let mut class = WNDCLASSW::default();
    if unsafe { GetClassInfoW(Some(instance), name, &mut class) }.is_err() {
        let class = WNDCLASSW {
            style: CS_DBLCLKS,
            hInstance: instance,
            lpfnWndProc: Some(window_proc),
            lpszClassName: name,
            hCursor: HCURSOR(cursors[SurfaceCursorKind::Cross as usize] as *mut _),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
    }
    record_native_stage(stages, "native_class", stage_started);
    let stage_started = trace_stages.then(Instant::now);
    let mut queue = Box::new(EventQueue {
        messages: Mutex::new(VecDeque::new()),
        presented_preview: Mutex::new(PreviewPresentation::default()),
        presented_selection: Mutex::new(PreviewPresentation::default()),
        preview_drag_origin: Mutex::new(Default::default()),
        overflow: AtomicBool::new(false),
        dormant: AtomicBool::new(true),
        cursors,
        current_cursor: AtomicUsize::new(cursors[SurfaceCursorKind::Cross as usize]),
        isolate_moves: std::env::var_os("RUSTSHOT_SURFACE_QA_CASE").is_some(),
        qa_modifiers: std::env::var("RUSTSHOT_SURFACE_QA_CASE").as_deref() == Ok("modifiers"),
        bounds,
        wheel: startup_surface_input::wheel_settings(),
    });
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            name,
            &windows::core::HSTRING::from(concat!(crate::brand_name!(), " · 快速框选")),
            WS_POPUP | WS_DISABLED,
            bounds.left,
            bounds.top,
            bounds.width as i32,
            bounds.height as i32,
            None,
            None,
            Some(instance),
            Some((&mut *queue as *mut EventQueue).cast()),
        )?
    };
    record_native_stage(stages, "native_create", stage_started);
    let native = NativeWindow {
        hwnd,
        thread_id: unsafe { GetCurrentThreadId() },
        token: crate::capture_standby::qpc() as usize,
        _queue: queue,
    };
    unsafe {
        SetPropW(
            hwnd,
            w!("RustShot.StartupSurface.Token"),
            Some(HANDLE(native.token as *mut _)),
        )?;
    }
    let stage_started = trace_stages.then(Instant::now);
    let resources = PaintResources::new(hwnd)?;
    record_native_stage(stages, "native_resources_create", stage_started);
    Ok(NativePrepared {
        native,
        resources,
        ime_disabled,
        physical_bounds: bounds,
        cloaked: false,
        layered_configured: false,
        compositor_error: None,
        synthetic_readback_prewarmed: false,
    })
}

fn validate_layered_opaque(hwnd: HWND) -> Result<()> {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool()
            || GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId()
            || GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_LAYERED.0 as isize == 0
        {
            return Err("快速首屏 layered 原窗口/线程/样式不符".into());
        }
        let mut color_key = COLORREF(0);
        let mut alpha = 0;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS::default();
        GetLayeredWindowAttributes(
            hwnd,
            Some(&mut color_key),
            Some(&mut alpha),
            Some(&mut flags),
        )?;
        if color_key.0 != 0 || alpha != 255 || flags != LWA_ALPHA {
            return Err("快速首屏 layered 不透明状态不符".into());
        }
    }
    Ok(())
}

fn validate_surface_input_state(native: &NativeWindow, enabled: bool) -> Result<isize> {
    let hwnd = native.hwnd;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool()
            || GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId()
        {
            return Err("快速首屏输入状态原窗口/线程不符".into());
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let required = (WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED).0 as isize;
        if ex_style & required != required
            || ex_style & (WS_EX_NOACTIVATE | WS_EX_TRANSPARENT).0 as isize != 0
            || IsWindowEnabled(hwnd).as_bool() != enabled
            || (GetWindowLongPtrW(hwnd, GWL_STYLE) & WS_DISABLED.0 as isize == 0) != enabled
        {
            return Err("快速首屏最终扩展样式或禁用输入状态不符".into());
        }
        Ok(ex_style)
    }
}

fn configure_layered_opaque(native: &NativeWindow) -> Result<()> {
    let hwnd = native.hwnd;
    unsafe {
        // 调用方始终持有原 HWND 的 RAII 所有者；不配置其他线程的窗口。
        if !IsWindow(Some(hwnd)).as_bool()
            || GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId()
        {
            return Err("快速首屏 layered 配置原窗口已失效".into());
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let layered_style = ex_style | WS_EX_LAYERED.0 as isize;
        if ex_style != layered_style {
            SetLastError(WIN32_ERROR(0));
            let previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, layered_style);
            if previous == 0 && GetLastError() != WIN32_ERROR(0) {
                return Err(windows::core::Error::from_win32().into());
            }
        }
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
        validate_layered_opaque(hwnd)?;
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) != layered_style {
            return Err("快速首屏 layered 配置改变了其他窗口样式".into());
        }
        // 截图首屏直接呈现，关闭窗口过渡；仅验证设置成功，不伪造属性读回。
        let disable_transitions = 1_i32;
        let changed =
            DwmSetWindowAttribute(hwnd, 3, (&disable_transitions as *const i32).cast(), 4);
        if changed < 0 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(changed)).into());
        }
    }
    Ok(())
}

fn cloak_window(hwnd: HWND, cloak: bool) -> Result<()> {
    request_cloak(hwnd, cloak)?;
    let actual = window_cloak(hwnd)?;
    if (cloak && actual & 1 == 0) || (!cloak && actual != 0) {
        return Err("快速首屏 DWM cloak 实际状态不符".into());
    }
    Ok(())
}

fn request_cloak(hwnd: HWND, cloak: bool) -> Result<()> {
    let value = i32::from(cloak);
    let changed = unsafe { DwmSetWindowAttribute(hwnd, 13, (&value as *const i32).cast(), 4) };
    if changed < 0 {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(changed)).into());
    }
    Ok(())
}

fn window_cloak(hwnd: HWND) -> Result<u32> {
    let mut actual = 0_u32;
    let queried = unsafe { DwmGetWindowAttribute(hwnd, 14, (&mut actual as *mut u32).cast(), 4) };
    if queried < 0 {
        return Err(windows::core::Error::from_hresult(windows::core::HRESULT(queried)).into());
    }
    Ok(actual)
}

struct WindowDc {
    hwnd: HWND,
    dc: HDC,
    owner_thread: u32,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl WindowDc {
    fn new(hwnd: HWND) -> Result<Self> {
        let dc = unsafe { GetDC(Some(hwnd)) };
        if dc.is_invalid() {
            return Err("快速首屏预热客户区 DC 创建失败".into());
        }
        Ok(Self {
            hwnd,
            dc,
            owner_thread: unsafe { GetCurrentThreadId() },
            _not_send: std::marker::PhantomData,
        })
    }
    fn release_checked(&mut self) -> Result<()> {
        if self.dc.is_invalid() || self.owner_thread != unsafe { GetCurrentThreadId() } {
            return Err("快速首屏客户区 DC 回收线程或所有权不符".into());
        }
        if unsafe { ReleaseDC(Some(self.hwnd), self.dc) } != 1 {
            // 留给 Drop 尝试兜底；失败不能成为首绘或 fallback 成功回执。
            return Err("快速首屏客户区 DC 回收失败".into());
        }
        self.dc = HDC::default();
        Ok(())
    }
}
impl Drop for WindowDc {
    fn drop(&mut self) {
        if !self.dc.is_invalid() && self.owner_thread == unsafe { GetCurrentThreadId() } {
            unsafe {
                ReleaseDC(Some(self.hwnd), self.dc);
            }
        }
    }
}

/// 资格快照只描述当前 DC；完整 bounding box 不能代替 SIMPLEREGION。
#[derive(Clone, Copy, Debug, serde::Serialize)]
struct FirstWindowDcCoverage {
    outer: [i32; 4],
    client: [i32; 4],
    clip: [i32; 4],
    clip_complexity: i32,
    map_mode: i32,
    graphics_mode: i32,
    layout: u32,
    window_origin: [i32; 2],
    viewport_origin: [i32; 2],
    window_extent: [i32; 2],
    viewport_extent: [i32; 2],
    world_transform: [f32; 6],
    queries_succeeded: bool,
}

fn first_window_dc_rejection(
    bounds: ScreenRect,
    coverage: &FirstWindowDcCoverage,
) -> Option<&'static str> {
    if !coverage.queries_succeeded {
        return Some("coverage-query-failed");
    }
    if !window_rects_cover_physical_bounds(bounds, coverage.outer, coverage.client) {
        return Some("physical-window-bounds-mismatch");
    }
    if coverage.map_mode != MM_TEXT.0
        || coverage.graphics_mode != GM_COMPATIBLE.0
        || coverage.layout != 0
        || coverage.window_origin != [0, 0]
        || coverage.viewport_origin != [0, 0]
        || coverage.window_extent[0] <= 0
        || coverage.window_extent[1] <= 0
        || coverage.viewport_extent != coverage.window_extent
        || coverage.world_transform != [1., 0., 0., 1., 0., 0.]
    {
        return Some("dc-mapping-not-physical-identity");
    }
    if coverage.clip_complexity != SIMPLEREGION.0 {
        return Some("dc-clip-not-simple-rectangle");
    }
    if coverage.clip != coverage.client {
        return Some("dc-clip-not-whole-client");
    }
    None
}

fn window_rects_cover_physical_bounds(
    bounds: ScreenRect,
    outer: [i32; 4],
    client: [i32; 4],
) -> bool {
    bounds.width > 0
        && bounds.height > 0
        && bounds.width <= 16384
        && bounds.height <= 16384
        && outer[0] == bounds.left
        && outer[1] == bounds.top
        && outer[2] as i64 - outer[0] as i64 == bounds.width as i64
        && outer[3] as i64 - outer[1] as i64 == bounds.height as i64
        && client == [0, 0, bounds.width as i32, bounds.height as i32]
}

fn query_first_window_dc_coverage(hwnd: HWND, dc: HDC) -> FirstWindowDcCoverage {
    let mut outer = RECT::default();
    let mut client = RECT::default();
    let mut clip = RECT::default();
    let mut window_origin = POINT::default();
    let mut viewport_origin = POINT::default();
    let mut window_extent = SIZE::default();
    let mut viewport_extent = SIZE::default();
    let mut world_transform = XFORM::default();
    unsafe {
        let outer_ok = GetWindowRect(hwnd, &mut outer).is_ok();
        let client_ok = GetClientRect(hwnd, &mut client).is_ok();
        let clip_complexity = GetClipBox(dc, &mut clip).0;
        let map_mode = GetMapMode(dc).0;
        let graphics_mode = GetGraphicsMode(dc);
        let layout = GetLayout(dc);
        let window_origin_ok = GetWindowOrgEx(dc, &mut window_origin).as_bool();
        let viewport_origin_ok = GetViewportOrgEx(dc, &mut viewport_origin).as_bool();
        let window_extent_ok = GetWindowExtEx(dc, &mut window_extent).as_bool();
        let viewport_extent_ok = GetViewportExtEx(dc, &mut viewport_extent).as_bool();
        let world_transform_ok = GetWorldTransform(dc, &mut world_transform).as_bool();
        FirstWindowDcCoverage {
            outer: [outer.left, outer.top, outer.right, outer.bottom],
            client: [client.left, client.top, client.right, client.bottom],
            clip: [clip.left, clip.top, clip.right, clip.bottom],
            clip_complexity,
            map_mode,
            graphics_mode,
            layout,
            window_origin: [window_origin.x, window_origin.y],
            viewport_origin: [viewport_origin.x, viewport_origin.y],
            window_extent: [window_extent.cx, window_extent.cy],
            viewport_extent: [viewport_extent.cx, viewport_extent.cy],
            world_transform: [
                world_transform.eM11,
                world_transform.eM12,
                world_transform.eM21,
                world_transform.eM22,
                world_transform.eDx,
                world_transform.eDy,
            ],
            queries_succeeded: outer_ok
                && client_ok
                && window_origin_ok
                && viewport_origin_ok
                && window_extent_ok
                && viewport_extent_ok
                && world_transform_ok,
        }
    }
}

fn prepare_first_window_dc<const TRACING: bool>(
    native: &NativeWindow,
    bounds: ScreenRect,
    eligible: bool,
    expected_ex_style: isize,
    mut trace: Option<&mut NativePaintTrace>,
) -> Result<Option<WindowDc>> {
    if !eligible {
        if let Some(trace) = trace.as_deref_mut() {
            trace.window_dc_fallback_reason = Some("not-prewarmed-reused-placement");
        }
        return Ok(None);
    }
    if let Some(trace) = trace.as_deref_mut() {
        trace.window_dc_attempted = true;
    }
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.window_dc_get_begin_qpc);
    let acquired = WindowDc::new(native.hwnd);
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.window_dc_get_end_qpc);
    let mut window_dc = match acquired {
        Ok(window_dc) => window_dc,
        Err(_) => {
            if let Some(trace) = trace.as_deref_mut() {
                trace.window_dc_fallback_reason = Some("get-dc-unavailable");
            }
            return Ok(None);
        }
    };
    if let Some(trace) = trace.as_deref_mut() {
        trace.window_dc_owner_thread = Some(window_dc.owner_thread);
    }
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.window_dc_check_begin_qpc);
    // GetDC 之后再读当前物理范围、映射和保护状态；资格不使用待命时的旧快照。
    let coverage = query_first_window_dc_coverage(native.hwnd, window_dc.dc);
    let rejection = first_window_dc_rejection(bounds, &coverage);
    if !native._queue.dormant.load(Ordering::Acquire)
        || validate_surface_input_state(native, false)? != expected_ex_style
        || window_cloak(native.hwnd)? & 1 == 0
    {
        return Err("快速首屏客户区 DC 资格期间原输入保护或 cloak 失效".into());
    }
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.window_dc_check_end_qpc);
    if let Some(trace) = trace.as_deref_mut() {
        trace.window_dc_coverage = Some(coverage);
        trace.window_dc_fallback_reason = rejection;
        trace.window_dc_qualified = rejection.is_none();
    }
    if rejection.is_some() {
        // 只有尚未绘制的资格失败可回退；不验证/清空旧更新区，沿用原首绘。
        NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| {
            &mut trace.window_dc_fallback_release_begin_qpc
        });
        let released = window_dc.release_checked();
        NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| {
            &mut trace.window_dc_fallback_release_end_qpc
        });
        if let Some(trace) = trace.as_deref_mut() {
            trace.window_dc_fallback_release_succeeded = released.is_ok();
        }
        released?;
        // 若资格期间观察到客户区/全局位置改变，原 BeginPaint 只读 rcPaint
        // 不足以重新证明物理原点。回退前再核验当前窗口，不能忽略已发现的变化。
        let mut outer = RECT::default();
        let mut client = RECT::default();
        unsafe {
            GetWindowRect(native.hwnd, &mut outer)?;
            GetClientRect(native.hwnd, &mut client)?;
        }
        if !window_rects_cover_physical_bounds(
            bounds,
            [outer.left, outer.top, outer.right, outer.bottom],
            [client.left, client.top, client.right, client.bottom],
        ) {
            return Err("快速首屏客户区 DC 回退窗口与物理桌面不一致".into());
        }
        if let Some(trace) = trace {
            trace.window_dc_fallback_geometry_verified = true;
        }
        return Ok(None);
    }
    Ok(Some(window_dc))
}

#[derive(Debug, serde::Serialize)]
struct SyntheticReadbackCleanup {
    flush_ok: bool,
    pixels_zeroed: bool,
    restored: Option<bool>,
    dc_deleted: Option<bool>,
    bitmap_deleted: Option<bool>,
    window_dc_released: Option<bool>,
    remaining_handles: bool,
}

impl SyntheticReadbackCleanup {
    fn succeeded(&self) -> bool {
        self.flush_ok
            && self.pixels_zeroed
            && self.restored != Some(false)
            && self.dc_deleted != Some(false)
            && self.bitmap_deleted != Some(false)
            && self.window_dc_released != Some(false)
            && !self.remaining_handles
    }
}

// 所有句柄只在创建线程使用/回收；raw bits 也阻止该对象被 Send 到其他线程。
struct SyntheticReadback {
    hwnd: HWND,
    window_dc: HDC,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    initialized_bytes: usize,
}

impl SyntheticReadback {
    fn new(hwnd: HWND, bounds: ScreenRect, bytes: usize) -> Result<Self> {
        let mut owner = Self {
            hwnd,
            window_dc: HDC::default(),
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            initialized_bytes: 0,
        };
        let prepared = (|| -> Result<()> {
            unsafe {
                // 原客户区 DC，绝不 GetDC(None) 或 GetWindowDC(None)。
                owner.window_dc = GetDC(Some(hwnd));
                if owner.window_dc.is_invalid() {
                    return Err("合成读回原窗口 DC 创建失败".into());
                }
                owner.dc = CreateCompatibleDC(Some(owner.window_dc));
                if owner.dc.is_invalid() {
                    return Err("合成读回内存 DC 创建失败".into());
                }
                let info = crate::gdi_surface::bitmap_info(bounds.width, bounds.height);
                let mut bits = std::ptr::null_mut();
                owner.bitmap = CreateDIBSection(
                    Some(owner.window_dc),
                    &info,
                    DIB_RGB_COLORS,
                    &mut bits,
                    None,
                    0,
                )?;
                // 先取得原 bitmap 所有权，随后任何头校验失败仍会回收。
                let mut header = DIBSECTION::default();
                let read = GetObjectW(
                    owner.bitmap.into(),
                    std::mem::size_of::<DIBSECTION>() as i32,
                    Some((&mut header as *mut DIBSECTION).cast()),
                );
                // top-down 来自本函数传入的负 biHeight；GetObject 规范化为正数。
                if read != std::mem::size_of::<DIBSECTION>() as i32
                    || bits.is_null()
                    || !(bits as usize).is_multiple_of(std::mem::align_of::<u32>())
                    || header.dsBm.bmBits != bits
                    || header.dsBm.bmWidth != bounds.width as i32
                    || header.dsBm.bmHeight != bounds.height as i32
                    || header.dsBm.bmWidthBytes != (bounds.width * 4) as i32
                    || header.dsBm.bmPlanes != 1
                    || header.dsBm.bmBitsPixel != 32
                    || header.dsBmih.biSize != std::mem::size_of::<BITMAPINFOHEADER>() as u32
                    || header.dsBmih.biWidth != bounds.width as i32
                    || header.dsBmih.biHeight != bounds.height as i32
                    || header.dsBmih.biPlanes != 1
                    || header.dsBmih.biBitCount != 32
                    || header.dsBmih.biCompression != BI_RGB.0
                {
                    return Err("合成读回自有 DIB 原生头/地址/跨度不符".into());
                }
                // 只有验证地址和实际长度后，才允许初始化及最终清零。
                owner.bits = bits.cast();
                GdiFlush().ok()?;
                owner.initialized_bytes = bytes;
                std::ptr::write_bytes(owner.bits, 0x5a, bytes);
                owner.previous = SelectObject(owner.dc, owner.bitmap.into());
                if owner.previous.is_invalid()
                    || GetCurrentObject(owner.dc, OBJ_BITMAP) != HGDIOBJ(owner.bitmap.0)
                {
                    return Err("合成读回 DIB 选择失败".into());
                }
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            let cleanup = owner.cleanup();
            return Err(format!("{error}; 合成读回清理={cleanup:?}").into());
        }
        Ok(owner)
    }

    fn cleanup(&mut self) -> SyntheticReadbackCleanup {
        let mut receipt = SyntheticReadbackCleanup {
            flush_ok: true,
            pixels_zeroed: self.initialized_bytes == 0,
            restored: None,
            dc_deleted: None,
            bitmap_deleted: None,
            window_dc_released: None,
            remaining_handles: false,
        };
        unsafe {
            if self.initialized_bytes != 0 {
                // Flush 提交当前线程 GDI 批次；即使批次回执失败，也不能保留缓冲内容。
                receipt.flush_ok = GdiFlush().as_bool();
                std::ptr::write_bytes(self.bits, 0, self.initialized_bytes);
                self.initialized_bytes = 0;
                receipt.pixels_zeroed = true;
            }
            if !self.dc.is_invalid() {
                if !self.previous.is_invalid() {
                    let displaced = SelectObject(self.dc, self.previous);
                    receipt.restored = Some(
                        displaced == HGDIOBJ(self.bitmap.0)
                            && GetCurrentObject(self.dc, OBJ_BITMAP) == self.previous,
                    );
                }
                // 即使恢复失败，也先删除原内存 DC，解除选择后才可删除 bitmap。
                let deleted = DeleteDC(self.dc).as_bool();
                receipt.dc_deleted = Some(deleted);
                if deleted {
                    self.dc = HDC::default();
                    self.previous = HGDIOBJ::default();
                }
            }
            if self.dc.is_invalid() && !self.bitmap.is_invalid() {
                let deleted = DeleteObject(self.bitmap.into()).as_bool();
                receipt.bitmap_deleted = Some(deleted);
                if deleted {
                    self.bitmap = HBITMAP::default();
                    self.bits = std::ptr::null_mut();
                }
            }
            if !self.window_dc.is_invalid() {
                let released = ReleaseDC(Some(self.hwnd), self.window_dc) == 1;
                receipt.window_dc_released = Some(released);
                if released {
                    self.window_dc = HDC::default();
                }
            }
        }
        // 提交删除批次后再返回，不能把尚未提交的临时资源带到 WarmReady。
        receipt.flush_ok &= unsafe { GdiFlush() }.as_bool();
        receipt.remaining_handles =
            !self.dc.is_invalid() || !self.bitmap.is_invalid() || !self.window_dc.is_invalid();
        receipt
    }
}

impl Drop for SyntheticReadback {
    fn drop(&mut self) {
        // 正常路径先显式 cleanup，所有已成功释放的句柄置空，不会重复删除。
        if !self.dc.is_invalid() || !self.bitmap.is_invalid() || !self.window_dc.is_invalid() {
            let receipt = self.cleanup();
            if !receipt.succeeded() {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "phase": "startup_synthetic_readback_cleanup_failed",
                        "pid": std::process::id(), "hwnd": self.hwnd.0 as usize,
                        "cleanup": receipt,
                    })
                );
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SyntheticWindowState {
    token: usize,
    dpi: u32,
    ex_style: isize,
}

fn synthetic_window_state(
    native: &NativeWindow,
    bounds: ScreenRect,
) -> Result<SyntheticWindowState> {
    let hwnd = native.hwnd;
    let mut pid = 0;
    let mut outer = RECT::default();
    let mut client = RECT::default();
    let mut client_origin = POINT::default();
    let mut gui = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool()
            || GetWindowThreadProcessId(hwnd, Some(&mut pid)) != GetCurrentThreadId()
            || pid != std::process::id()
            || !native._queue.dormant.load(Ordering::Acquire)
            || !IsWindowVisible(hwnd).as_bool()
            || window_cloak(hwnd)? != 1
        {
            return Err("合成读回原窗口/线程/待命/cloak 身份不符".into());
        }
        validate_layered_opaque(hwnd)?;
        // WS_DISABLED 从创建起禁止用户键鼠和激活；不是用 cloak 或
        // 同线程 HTTRANSPARENT 冒充系统输入保护。扩展样式保持截图最终值。
        let ex_style = validate_surface_input_state(native, false)?;
        GetWindowRect(hwnd, &mut outer)?;
        GetClientRect(hwnd, &mut client)?;
        ClientToScreen(hwnd, &mut client_origin).ok()?;
        if outer.left != bounds.left
            || outer.top != bounds.top
            || outer.right as i64 - outer.left as i64 != bounds.width as i64
            || outer.bottom as i64 - outer.top as i64 != bounds.height as i64
            || client.left != 0
            || client.top != 0
            || client.right != bounds.width as i32
            || client.bottom != bounds.height as i32
            || client_origin.x != bounds.left
            || client_origin.y != bounds.top
        {
            return Err("合成读回物理窗口/完整客户区不符".into());
        }
        let token = GetPropW(hwnd, w!("RustShot.StartupSurface.Token")).0 as usize;
        let dpi = GetDpiForWindow(hwnd);
        GetGUIThreadInfo(GetCurrentThreadId(), &mut gui)?;
        let foreground = GetForegroundWindow();
        if token == 0
            || dpi == 0
            || foreground == hwnd
            || gui.hwndActive == hwnd
            || gui.hwndFocus == hwnd
            || gui.hwndCapture == hwnd
        {
            return Err("合成读回原令牌/DPI/非前台焦点门失败".into());
        }
        Ok(SyntheticWindowState {
            token,
            dpi,
            ex_style,
        })
    }
}

// Ok(true) 只表示自有黑色窗口全量读回和清理实际成功，不是冻结桌面 Ready。
// Ok(false) 仅表示大于临时 DIB 的 256MiB 预算，正常请求描画逻辑不变。
fn prewarm_synthetic_readback(native: &NativeWindow, bounds: ScreenRect) -> Result<bool> {
    const LIMIT: usize = 256 * 1024 * 1024;
    if bounds.width == 0
        || bounds.height == 0
        || bounds.width > 16_384
        || bounds.height > 16_384
        || bounds.left.checked_add(bounds.width as i32).is_none()
        || bounds.top.checked_add(bounds.height as i32).is_none()
    {
        return Err("合成读回物理范围非法".into());
    }
    let before = synthetic_window_state(native, bounds)?;
    let bytes = (bounds.width as usize)
        .checked_mul(bounds.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("合成读回字节数溢出")?;
    if bytes > LIMIT {
        if synthetic_window_state(native, bounds)? != before {
            return Err("合成读回预算跳过期间原窗口/DPI变化".into());
        }
        return Ok(false);
    }
    let mut readback = SyntheticReadback::new(native.hwnd, bounds, bytes)?;
    let measured = (|| -> Result<()> {
        let client = RECT {
            left: 0,
            top: 0,
            right: bounds.width as i32,
            bottom: bounds.height as i32,
        };
        unsafe {
            if FillRect(
                readback.window_dc,
                &client,
                HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ) == 0
            {
                return Err("合成读回 layered 纯黑绘制失败".into());
            }
            GdiFlush().ok()?;
            // 哨兵已写满实际 DIB；空 clip 或仅部分复制不能通过逐像素黑色门。
            let copied = BitBlt(
                readback.dc,
                0,
                0,
                bounds.width as i32,
                bounds.height as i32,
                Some(readback.window_dc),
                0,
                0,
                SRCCOPY,
            );
            let flushed = GdiFlush().ok();
            copied?;
            flushed?;
            let pixels = std::slice::from_raw_parts(readback.bits.cast::<u32>(), bytes / 4);
            if pixels.iter().any(|pixel| pixel & 0x00ff_ffff != 0) {
                return Err("合成读回全部 BGR 并非原窗口纯黑内容".into());
            }
        }
        if synthetic_window_state(native, bounds)? != before {
            return Err("合成读回期间原 HWND/令牌/物理坐标/DPI发生变化".into());
        }
        Ok(())
    })();
    // 不论测量成败，先清零再回收，之后才允许返回；清理失败不能报告 true。
    let cleanup = readback.cleanup();
    let after = synthetic_window_state(native, bounds);
    if !cleanup.succeeded()
        || cleanup.restored != Some(true)
        || cleanup.dc_deleted != Some(true)
        || cleanup.bitmap_deleted != Some(true)
        || cleanup.window_dc_released != Some(true)
    {
        return Err(format!(
            "合成读回清理失败：{cleanup:?}; 测量={:?}",
            measured.as_ref().err().map(|error| error.to_string())
        )
        .into());
    }
    measured?;
    if after? != before {
        return Err("合成读回清理期间原窗口或DPI变化".into());
    }
    Ok(true)
}

fn virtual_surface_bounds() -> Result<ScreenRect> {
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0
        || height <= 0
        || width > 16384
        || height > 16384
        || left.checked_add(width).is_none()
        || top.checked_add(height).is_none()
    {
        return Err("快速首屏待命桌面范围无效".into());
    }
    Ok(ScreenRect {
        left,
        top,
        width: width as u32,
        height: height as u32,
    })
}

fn prewarm_compositor(prepared: &mut NativePrepared, signals: &WarmSignals) -> Result<()> {
    let bounds = virtual_surface_bounds()?;
    let hwnd = prepared.native.hwnd;
    // 先确认 app cloak，再显示自有黑色窗口；不能在桌面暴露预热内容或抢焦点。
    cloak_window(hwnd, true)?;
    if signals.stop.load(Ordering::Acquire) {
        return Err("快速首屏预热已停止".into());
    }
    unsafe {
        // 自创建以来保持 WS_DISABLED，预热不再往返更改扩展样式。
        // SWP_NOACTIVATE 仍防止此次显示/定位激活窗口。
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            bounds.left,
            bounds.top,
            bounds.width as i32,
            bounds.height as i32,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        )?;
    }
    cloak_window(hwnd, true)?;
    let mut client = RECT::default();
    let mut outer = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut client)?;
        GetWindowRect(hwnd, &mut outer)?;
    }
    if outer.left != bounds.left
        || outer.top != bounds.top
        || outer.right as i64 - outer.left as i64 != bounds.width as i64
        || outer.bottom as i64 - outer.top as i64 != bounds.height as i64
        || client.left != 0
        || client.top != 0
        || client.right != bounds.width as i32
        || client.bottom != bounds.height as i32
        || !unsafe { IsWindowVisible(hwnd) }.as_bool()
    {
        return Err("快速首屏预热客户区与物理桌面不符".into());
    }
    unsafe {
        if !InvalidateRect(Some(hwnd), None, false).as_bool() || !UpdateWindow(hwnd).as_bool() {
            return Err("快速首屏预热更新失败".into());
        }
    }
    // GetDC 的绘制范围不依赖 update region，避免空 BeginPaint clip 造成假成功。
    let dc = WindowDc::new(hwnd)?;
    let painted = unsafe { FillRect(dc.dc, &client, HBRUSH(GetStockObject(BLACK_BRUSH).0)) } != 0
        && unsafe { GdiFlush() }.as_bool();
    drop(dc);
    unsafe {
        let _ = ValidateRect(Some(hwnd), None);
    }
    if !painted || unsafe { DwmFlush() } < 0 {
        return Err("快速首屏预热纯色绘制或合成提交失败".into());
    }
    if signals.stop.load(Ordering::Acquire) {
        return Err("快速首屏预热已停止".into());
    }
    // 先提交 opaque 纯黑合成资源，再切换 layered，复用已核验的实验时序。
    // 全程保持 app cloak，不暴露黑色预热画面。
    if window_cloak(hwnd)? & 1 == 0 {
        return Err("快速首屏预热 layered 配置前 cloak 已失效".into());
    }
    configure_layered_opaque(&prepared.native)?;
    prepared.layered_configured = true;
    if window_cloak(hwnd)? & 1 == 0 {
        return Err("快速首屏预热 layered 配置后 cloak 已失效".into());
    }
    // 在最终 layered 配置上提交自有纯黑合成资源；实际截图的最终 DwmFlush 仍保留。
    // 两侧复验同一个待命窗口的归属、物理范围、cloak 和无活动输入状态。
    let layered_state = synthetic_window_state(&prepared.native, bounds)?;
    if unsafe { DwmFlush() } < 0 {
        return Err("快速首屏预热 layered 合成提交失败".into());
    }
    if signals.stop.load(Ordering::Acquire) {
        return Err("快速首屏预热已停止".into());
    }
    if synthetic_window_state(&prepared.native, bounds)? != layered_state {
        return Err("快速首屏预热 layered 合成后原窗口状态变化".into());
    }
    prepared.synthetic_readback_prewarmed = prewarm_synthetic_readback(&prepared.native, bounds)?;
    if signals.stop.load(Ordering::Acquire) {
        return Err("快速首屏预热已停止".into());
    }
    prepared.physical_bounds = bounds;
    prepared.cloaked = true;
    Ok(())
}

fn run_capture_native(setup: NativeSetup, prepared: Option<NativePrepared>) {
    let shared = setup.shared.clone();
    let ready = setup.ready.clone();
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_native(setup, prepared)));
    let error = match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(NativeSurfaceFailure::from_error(
            error.as_ref(),
            NativeStartupStage::load(&shared.startup_stage),
        )),
        Err(_) => Some(NativeSurfaceFailure {
            kind: NativeSurfaceFailureKind::Other,
            stage: NativeStartupStage::load(&shared.startup_stage),
            message: "快速首屏原生线程异常".into(),
        }),
    };
    if let Some(error) = error {
        *shared.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
        let _ = ready.try_send(Err(error));
    }
    shared.hwnd.store(0, Ordering::Release);
    shared.exited.store(true, Ordering::Release);
}

fn run_warm_native(
    signals: &WarmSignals,
    setup: Receiver<NativeSetup>,
    ready: &SyncSender<std::result::Result<WarmSurfaceIdentity, String>>,
) -> Result<()> {
    let tracing = std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some();
    let clock = || {
        if tracing {
            crate::capture_standby::qpc() as usize
        } else {
            0
        }
    };
    let small = ScreenRect {
        left: 0,
        top: 0,
        width: 1,
        height: 1,
    };
    let mut prepared = prepare_native(small, &mut Vec::new())?;
    if let Err(error) = prewarm_compositor(&mut prepared, signals) {
        // 先隐藏，再销毁失败窗口。新建未碰 cloak 的 1x1 资源路径，避免错误
        // 清除 cloak 时把全屏黑色暴露出来，也不把未知状态冒称可用的热表面。
        unsafe {
            let _ = ShowWindow(prepared.native.hwnd, SW_HIDE);
        }
        drop(prepared);
        if signals.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        prepared = prepare_native(small, &mut Vec::new())?;
        prepared.compositor_error = Some(error.to_string());
    }
    let hwnd = prepared.native.hwnd;
    // 只绘制自有的一个黑色像素到遮罩 DIB；不读屏幕，也不创建 RenderImage/驱动设备。
    let pixel = [0_u8, 0, 0, 255];
    let info = crate::gdi_surface::bitmap_info(1, 1);
    unsafe {
        let copied = StretchDIBits(
            prepared.resources.dim_dc,
            0,
            0,
            1,
            1,
            0,
            0,
            1,
            1,
            Some(pixel.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        if copied == 0 || copied == GDI_ERROR || !GdiFlush().as_bool() {
            return Err("快速首屏小型 GDI 预热失败".into());
        }
        let token = GetPropW(hwnd, w!("RustShot.StartupSurface.Token")).0 as usize;
        let ready_qpc = crate::capture_standby::qpc() as usize;
        SetPropW(
            hwnd,
            w!("RustShot.StartupSurface.Prewarmed"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )?;
        SetPropW(
            hwnd,
            w!("RustShot.StartupSurface.WarmReadyQpc"),
            Some(HANDLE(ready_qpc as *mut _)),
        )?;
        signals
            .thread_id
            .store(GetCurrentThreadId(), Ordering::Release);
        signals.token.store(token, Ordering::Release);
        signals.hwnd.store(hwnd.0 as usize, Ordering::Release);
        ready
            .send(Ok(WarmSurfaceIdentity {
                hwnd: hwnd.0 as usize,
                token,
                ready_qpc,
                physical_bounds: prepared.physical_bounds,
                cloaked: prepared.cloaked,
                window_visible: IsWindowVisible(hwnd).as_bool(),
            }))
            .map_err(|_| "快速首屏预热调用方已退出")?;
    }
    let (mut wait_begin, mut wait_end, mut wake_dequeued, mut drain_end) = (0, 0, 0, 0);
    let (mut messages_drained, mut paints_validated) = (0_u32, 0_u32);
    let mut message_budget = 0_usize;
    loop {
        if signals.stop.load(Ordering::Acquire) {
            break;
        }
        // 每条消息之前先消费 setup，不能让 USER drain 批次挡住 kernel wake。
        match setup.try_recv() {
            Ok(setup) => {
                let dispatch = &setup.shared.dispatch;
                dispatch.stamp(&dispatch.setup_received_qpc);
                dispatch
                    .warm_wait_begin_qpc
                    .store(wait_begin, Ordering::Release);
                dispatch
                    .warm_wait_end_qpc
                    .store(wait_end, Ordering::Release);
                dispatch
                    .warm_kernel_event_received_qpc
                    .store(wake_dequeued, Ordering::Release);
                dispatch
                    .warm_drain_end_qpc
                    .store(drain_end, Ordering::Release);
                dispatch
                    .warm_messages
                    .store(messages_drained, Ordering::Release);
                dispatch
                    .warm_paints_validated
                    .store(paints_validated, Ordering::Release);
                run_capture_native(setup, Some(prepared));
                return Ok(());
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if message_budget == 0 {
            // Event 不依赖 USER 投递；一秒上限仍覆盖停止时 HWND 标识失效。
            wait_begin = clock();
            let status = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[signals.event_handle()]),
                    1000,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
            wait_end = clock();
            if status == WAIT_FAILED {
                return Err(windows::core::Error::from_win32().into());
            }
            if status == WAIT_OBJECT_0 {
                wake_dequeued = clock();
            }
            message_budget = 256;
            // 唤醒后先回到 try_recv/stop，再处理第一条窗口消息。
            continue;
        }
        let mut message = MSG::default();
        if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            message_budget = 0;
            drain_end = clock();
            continue;
        }
        message_budget -= 1;
        messages_drained = messages_drained.saturating_add(1);
        if message.message == WM_QUIT {
            return Ok(());
        }
        unsafe {
            DispatchMessageW(&message);
        }
        while let Some(event) = prepared.native._queue.pop() {
            if event.raw.0 == WM_CLOSE {
                return Ok(());
            }
            if event.raw.0 == WM_PAINT {
                // 待命内容只有已经绘制的自有黑色；只清除更新区，不读屏幕。
                if unsafe { ValidateRect(Some(hwnd), None) }.as_bool() {
                    paints_validated = paints_validated.saturating_add(1);
                }
            }
        }
        drain_end = clock();
    }
    Ok(())
}

fn run_native(setup: NativeSetup, prepared: Option<NativePrepared>) -> Result<()> {
    let native_started = Instant::now();
    setup
        .shared
        .startup_stage
        .store(NativeStartupStage::Creating as u32, Ordering::Release);
    setup
        .shared
        .dispatch
        .stamp(&setup.shared.dispatch.native_enter_qpc);
    if setup.shared.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let NativeSetup {
        bounds,
        planes,
        startup_bitmap,
        wheel_settings,
        current,
        targets,
        started,
        shared,
        commands,
        ready,
    } = setup;
    let shared = shared.as_ref();
    let trace = std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some();
    let trace_stages = std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
    let mut stages = Vec::with_capacity(if trace_stages { 16 } else { 0 });
    if let Some(timings) = wheel_settings.query.timings.as_ref() {
        // 查询可能与捕获重叠；原生实际 Ready 后才输出，不能把其时长重复加总。
        stages.push(("native_wheel_settings", timings.duration_ms));
    }
    let prewarmed = prepared.is_some();
    let NativePrepared {
        mut native,
        resources,
        ime_disabled,
        physical_bounds: warm_bounds,
        cloaked: compositor_prewarmed,
        layered_configured,
        compositor_error,
        synthetic_readback_prewarmed,
    } = match prepared {
        Some(prepared) => prepared,
        None => prepare_native(bounds, &mut stages)?,
    };
    shared
        .startup_stage
        .store(NativeStartupStage::Preparing as u32, Ordering::Release);
    // 真实桌面布局只在请求后采集；待命时的 1x1/DPI 不用于冻结或输入换算。
    if unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        .0
        .is_null()
    {
        return Err("快速首屏不能恢复物理像素 DPI 坐标".into());
    }
    let hwnd = native.hwnd;
    let stage_started = trace_stages.then(Instant::now);
    let queue_started = trace_stages.then(Instant::now);
    native._queue.bounds = bounds;
    native._queue.wheel = wheel_settings.query.wheel;
    native._queue.reset_preview();
    native
        ._queue
        .preview_drag_origin
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reset();
    native
        ._queue
        .messages
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    native._queue.overflow.store(false, Ordering::Release);
    // 队列准备属于下方请求准备总包络的子段；两者不能相加。
    record_native_stage(&mut stages, "native_queue_prepare", queue_started);
    shared
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    shared.hwnd.store(hwnd.0 as usize, Ordering::Release);
    record_native_stage(&mut stages, "native_request_setup", stage_started);
    let stage_started = trace_stages.then(Instant::now);
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut cursor = POINT::default();
    let mut preview_pointer = None;
    if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
        preview_pointer = startup_preview::local_point(bounds, (cursor.x, cursor.y));
        state.pointer = (
            (cursor.x as i64 - bounds.left as i64).clamp(0, bounds.width as i64) as i32,
            (cursor.y as i64 - bounds.top as i64).clamp(0, bounds.height as i64) as i32,
        );
    }
    state.toolbar_monitor = current;
    let mut runtime = Runtime {
        editor: Editor::new(Rect::full(bounds.width, bounds.height), state.crop),
        state,
        bounds,
        planes,
        startup_bitmap,
        targets,
        preview_pointer,
        // 首绘前完成本次物理定位后统一取得 DPI；此时尚未消费输入。
        tolerance: 6.,
        button_down: false,
        click_gesture: ClickGesture::default(),
        click_started: None,
        double_click_interval: Duration::from_millis(unsafe { GetDoubleClickTime() } as u64),
        double_click_tolerance: unsafe {
            (
                GetSystemMetrics(SM_CXDOUBLECLK),
                GetSystemMetrics(SM_CYDOUBLECLK),
            )
        },
        target: None,
        pending: Vec::new(),
        deferred: VecDeque::new(),
        early_output: StartupOutputQueue::default(),
        ime_disabled,
        native_arrow: Default::default(),
        native_idle_source: None,
        manual_click_preview: None,
        manual_cursor_origin: None,
        manual_cursor_dragged: false,
        new_cursor_origin: None,
        new_cursor_owner: None,
        new_cursor_moved: false,
        adjust_cursor_origin: None,
        adjust_cursor_owner: None,
        adjust_cursor_moved: false,
        monitor_snap_cycle: false,
        preview_scale: 1.,
    };
    runtime.refresh_window_preview();
    record_native_stage(&mut stages, "native_resources", stage_started);
    if shared.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let ready_qpc;
    let foreground_set_foreground_ok;
    let foreground_pre_focus_query_ok;
    let foreground_pre_focus_active;
    let foreground_pre_focus_focus;
    let foreground_set_focus_called;
    let native_show_reused_placement;
    let native_show_final_ex_style;
    let cloaked;
    let mut native_paint_trace = trace_stages.then(|| NativePaintTrace::new(hwnd));
    shared
        .startup_stage
        .store(NativeStartupStage::Painting as u32, Ordering::Release);
    unsafe {
        let stage_started = trace_stages.then(Instant::now);
        let part_started = trace_stages.then(Instant::now);
        if layered_configured {
            // 成功预热已配置；只读核验，不重复提交 layered/过渡属性。
            validate_layered_opaque(hwnd)?;
        } else {
            // 冷启动及预热失败重建的 1x1 窗口均在首次显示前配置。
            configure_layered_opaque(&native)?;
        }
        record_native_stage(&mut stages, "native_show_layered", part_started);
        let part_started = trace_stages.then(Instant::now);
        let mut placed = RECT::default();
        let mut placed_client = RECT::default();
        GetWindowRect(hwnd, &mut placed)?;
        GetClientRect(hwnd, &mut placed_client)?;
        let reuse_placement = compositor_prewarmed
            && placed.left == bounds.left
            && placed.top == bounds.top
            && placed.right as i64 - placed.left as i64 == bounds.width as i64
            && placed.bottom as i64 - placed.top as i64 == bounds.height as i64
            && placed_client.left == 0
            && placed_client.top == 0
            && placed_client.right == bounds.width as i32
            && placed_client.bottom == bounds.height as i32;
        native_show_reused_placement = reuse_placement;
        record_native_stage(&mut stages, "native_show_initial_geometry", part_started);
        let part_started = trace_stages.then(Instant::now);
        if compositor_prewarmed && window_cloak(hwnd)? & 1 == 0 {
            return Err("快速首屏待命 cloak 在本次绘制前失效".into());
        }
        // 冷启动及预热失败重建的窗口尚未显示：同样先 cloak 再显示，首绘合成后才撤，
        // 避免 DWM 先呈现未初始化的重定向表面。系统拒绝 cloak 时沿用原直接显示。
        cloaked = compositor_prewarmed || cloak_window(hwnd, true).is_ok();
        record_native_stage(&mut stages, "native_show_initial_cloak", part_started);
        let part_started = trace_stages.then(Instant::now);
        if !reuse_placement {
            // 布局变化仍按本次新冻结范围定位，保持 cloak 直到真实内容成功画完。
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                bounds.left,
                bounds.top,
                bounds.width as i32,
                bounds.height as i32,
                SWP_SHOWWINDOW | SWP_NOACTIVATE,
            )?;
        }
        record_native_stage(&mut stages, "native_show_placement", part_started);
        let part_started = trace_stages.then(Instant::now);
        // 1x1 待命 HWND 可能在另一屏；扩展到本次真实物理范围后才使用窗口 DPI。
        let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.;
        runtime.tolerance = 6. * scale;
        runtime.preview_scale = scale;
        record_native_stage(&mut stages, "native_show_dpi", part_started);
        let part_started = trace_stages.then(Instant::now);
        let mut actual = RECT::default();
        let mut client = RECT::default();
        GetWindowRect(hwnd, &mut actual)?;
        GetClientRect(hwnd, &mut client)?;
        if actual.left != bounds.left
            || actual.top != bounds.top
            || actual.right as i64 - actual.left as i64 != bounds.width as i64
            || actual.bottom as i64 - actual.top as i64 != bounds.height as i64
            || client.left != 0
            || client.top != 0
            || client.right != bounds.width as i32
            || client.bottom != bounds.height as i32
        {
            return Err("快速首屏窗口与本次物理桌面不一致".into());
        }
        record_native_stage(&mut stages, "native_show_final_geometry", part_started);
        {
            // 待命到实际首绘始终保持 disabled/dormant；不在本次请求中切换
            // 扩展样式。冷启动也遵守首绘后才启用输入的顺序。
            let part_started = trace_stages.then(Instant::now);
            if !native._queue.dormant.load(Ordering::Acquire)
                || GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId()
                || (cloaked && window_cloak(hwnd)? & 1 == 0)
            {
                return Err("快速首屏绘制前原待命窗口状态失效".into());
            }
            record_native_stage(&mut stages, "native_show_style_guard", part_started);
            let part_started = trace_stages.then(Instant::now);
            native_show_final_ex_style = validate_surface_input_state(&native, false)?;
            record_native_stage(&mut stages, "native_show_style_read", part_started);
            let part_started = trace_stages.then(Instant::now);
            if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) != native_show_final_ex_style
                || (cloaked && window_cloak(hwnd)? & 1 == 0)
            {
                return Err("快速首屏绘制前最终样式或 cloak 失效".into());
            }
            record_native_stage(&mut stages, "native_show_style_verify", part_started);
        }
        let part_started = trace_stages.then(Instant::now);
        let direct_dc_eligible = compositor_prewarmed && reuse_placement && layered_configured;
        let window_dc = match native_paint_trace.as_mut() {
            Some(trace) => prepare_first_window_dc::<true>(
                &native,
                bounds,
                direct_dc_eligible,
                native_show_final_ex_style,
                Some(trace),
            ),
            None => prepare_first_window_dc::<false>(
                &native,
                bounds,
                direct_dc_eligible,
                native_show_final_ex_style,
                None,
            ),
        }?;
        record_native_stage(&mut stages, "native_show_window_dc_prepare", part_started);
        if window_dc.is_none() {
            let part_started = trace_stages.then(Instant::now);
            if !InvalidateRect(Some(hwnd), None, false).as_bool() {
                return Err("快速首屏首次更新区创建失败".into());
            }
            record_native_stage(&mut stages, "native_show_invalidate", part_started);
        }
        record_native_stage(&mut stages, "native_show", stage_started);
        let stage_started = trace_stages.then(Instant::now);
        native._queue.begin_preview_paint();
        let painted = match window_dc {
            Some(window_dc) => match native_paint_trace.as_mut() {
                Some(trace) => paint_first_window_dc_with_trace::<true>(
                    window_dc,
                    &resources,
                    &runtime,
                    Some(trace),
                ),
                None => {
                    paint_first_window_dc_with_trace::<false>(window_dc, &resources, &runtime, None)
                }
            },
            None => match native_paint_trace.as_mut() {
                Some(trace) => paint_window_with_trace::<true>(
                    hwnd,
                    &resources,
                    &runtime,
                    true,
                    None,
                    Some(trace),
                ),
                None => {
                    paint_window_with_trace::<false>(hwnd, &resources, &runtime, true, None, None)
                }
            },
        }?;
        if !painted {
            return Err("快速首屏没有执行真实的首次绘制".into());
        }
        record_native_stage(&mut stages, "native_paint", stage_started);
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let stage_started = trace_stages.then(Instant::now);
        // EnableWindow 的返回值是旧禁用状态，不是成功标志；实际读回两种
        // 输入状态并核验扩展样式未变。同步 WM_ENABLE 不借用 Runtime。
        shared
            .startup_stage
            .store(NativeStartupStage::Enabling as u32, Ordering::Release);
        let _ = EnableWindow(hwnd, true);
        if validate_surface_input_state(&native, true)? != native_show_final_ex_style
            || !native._queue.dormant.load(Ordering::Acquire)
            || (cloaked && window_cloak(hwnd)? & 1 == 0)
        {
            return Err("快速首屏首绘后启用输入或原 cloak/样式失效".into());
        }
        record_native_stage(&mut stages, "native_enable_input", stage_started);
        // EnableWindow 可能同步派发消息；若此时收到停止请求，保持 dormant/cloak
        // 并按原生命周期回收，不能继续显示已取消的会话。
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if cloaked {
            // 首绘虽已 GdiFlush，重定向表面的更新仍由 DWM 异步取用；立即撤 cloak 时
            // 实测各屏会先合成一帧待命预热的纯黑（冷启动为未初始化）内容。
            // 等本次更新进入合成后再显示，代价至多一个合成周期。
            let stage_started = trace_stages.then(Instant::now);
            if DwmFlush() < 0 {
                return Err("快速首屏首绘合成提交失败".into());
            }
            record_native_stage(&mut stages, "native_paint_dwm_flush", stage_started);
            if shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
        }
        native._queue.dormant.store(false, Ordering::Release);
        if cloaked {
            shared
                .startup_stage
                .store(NativeStartupStage::Uncloaking as u32, Ordering::Release);
            let stage_started = trace_stages.then(Instant::now);
            // 合成完成后统一读回实际 cloak，避免重复往返阻塞框选线程。
            request_cloak(hwnd, false)?;
            record_native_stage(&mut stages, "native_uncloak", stage_started);
        }
        let foreground_started = trace_stages.then(Instant::now);
        shared
            .startup_stage
            .store(NativeStartupStage::Foreground as u32, Ordering::Release);
        let stage_started = trace_stages.then(Instant::now);
        foreground_set_foreground_ok = SetForegroundWindow(hwnd).as_bool();
        record_native_stage(
            &mut stages,
            "native_foreground_set_foreground",
            stage_started,
        );
        let stage_started = trace_stages.then(Instant::now);
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        foreground_pre_focus_query_ok = GetGUIThreadInfo(GetCurrentThreadId(), &mut gui).is_ok();
        foreground_pre_focus_active = gui.hwndActive.0 as usize;
        foreground_pre_focus_focus = gui.hwndFocus.0 as usize;
        // 只有本线程活动窗口和焦点都已是原目标时，才省略重复的 SetFocus。
        // 查询失败仍沿用原调用；下方两个真实焦点门继续重新读取，不使用此快照作 Ready。
        foreground_set_focus_called =
            !foreground_pre_focus_query_ok || gui.hwndFocus != hwnd || gui.hwndActive != hwnd;
        record_native_stage(
            &mut stages,
            "native_foreground_pre_focus_query",
            stage_started,
        );
        let stage_started = trace_stages.then(Instant::now);
        if foreground_set_focus_called {
            let _ = SetFocus(Some(hwnd));
        }
        record_native_stage(&mut stages, "native_foreground_set_focus", stage_started);
        let stage_started = trace_stages.then(Instant::now);
        if !owns_foreground(hwnd) {
            reacquire_foreground(hwnd);
        }
        if GetForegroundWindow() != hwnd {
            return Err(report_activation_failure(NativeSurfaceFailure::activation(
                NativeStartupStage::Foreground,
                "快速首屏未能获得键盘焦点",
            )));
        }
        GetGUIThreadInfo(GetCurrentThreadId(), &mut gui).map_err(|error| {
            report_activation_failure(NativeSurfaceFailure::activation(
                NativeStartupStage::Foreground,
                format!("快速首屏活动窗口/焦点读回失败：{error}"),
            ))
        })?;
        check_activation(
            NativeStartupStage::Foreground,
            hwnd.0 as usize,
            GetForegroundWindow().0 as usize,
            gui.hwndActive.0 as usize,
            gui.hwndFocus.0 as usize,
        )
        .map_err(report_activation_failure)?;
        record_native_stage(&mut stages, "native_foreground_focus_check", stage_started);
        if !set_surface_cursor(hwnd) {
            return Err("快速首屏十字光标不可用".into());
        }
        record_native_stage(&mut stages, "native_foreground", foreground_started);
        shared
            .startup_stage
            .store(NativeStartupStage::DwmReady as u32, Ordering::Release);
        let stage_started = trace_stages.then(Instant::now);
        if DwmFlush() < 0 {
            return Err("快速首屏合成提交失败".into());
        }
        record_native_stage(&mut stages, "native_dwm_flush", stage_started);
        let stage_started = trace_stages.then(Instant::now);
        if window_cloak(hwnd)? != 0 {
            return Err("快速首屏仍被 DWM cloak，不得报告 Ready".into());
        }
        record_native_stage(&mut stages, "native_final_cloak", stage_started);
        let stage_started = trace_stages.then(Instant::now);
        if !owns_foreground(hwnd) {
            // 显示到合成之间焦点可能被通知或其它窗口瞬时抢走；补救一次再判失败。
            reacquire_foreground(hwnd);
        }
        GetGUIThreadInfo(GetCurrentThreadId(), &mut gui).map_err(|error| {
            report_activation_failure(NativeSurfaceFailure::activation(
                NativeStartupStage::DwmReady,
                format!("快速首屏合成后活动窗口/焦点读回失败：{error}"),
            ))
        })?;
        check_activation(
            NativeStartupStage::DwmReady,
            hwnd.0 as usize,
            GetForegroundWindow().0 as usize,
            gui.hwndActive.0 as usize,
            gui.hwndFocus.0 as usize,
        )
        .map_err(report_activation_failure)?;
        record_native_stage(&mut stages, "native_final_focus", stage_started);
        // 首次 draw/DC 回收、DwmFlush、cloak 和焦点门都真实成功，才允许输入快照读此范围。
        native._queue.publish_completed_selection(
            runtime.state.crop.or(runtime.state.window_preview),
            runtime
                .state
                .window_preview
                .filter(|_| runtime.state.crop.is_none()),
        )?;
        let stage_started = trace_stages.then(Instant::now);
        SetPropW(
            hwnd,
            w!("RustShot.StartupSurface.Ready"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )?;
        if let Ok(request) = std::env::var("RUSTSHOT_STARTUP_REQUEST_QPC")
            .unwrap_or_default()
            .parse::<usize>()
        {
            SetPropW(
                hwnd,
                w!("RustShot.StartupSurface.RequestQpc"),
                Some(HANDLE(request as *mut _)),
            )?;
        }
        record_native_stage(&mut stages, "native_ready_properties", stage_started);
        ready_qpc = crate::capture_standby::qpc() as usize;
        SetPropW(
            hwnd,
            w!("RustShot.StartupSurface.ReadyQpc"),
            Some(HANDLE(ready_qpc as *mut _)),
        )?;
        if let Ok(resident) = FindWindowW(w!("RustShot.MVP.Window"), None) {
            let _ = PostMessageW(
                Some(resident),
                crate::capture_standby::SURFACE_READY,
                WPARAM(std::process::id() as usize),
                LPARAM(0),
            );
        }
    }
    runtime.publish(shared);
    let metrics = SurfaceMetrics {
        ready_ms: started.elapsed().as_secs_f64() * 1000.,
        request_to_ready_ms: crate::startup_probe::request_elapsed_ms(),
        start_surface_to_ready_ms: native_started.elapsed().as_secs_f64() * 1000.,
    };
    *shared.metrics.lock().unwrap_or_else(|e| e.into_inner()) = Some(metrics);
    // 准备完毕后再输出，避免测量中的 stdout I/O 混入首屏各阶段。
    shared.dispatch.emit_after_ready(hwnd, prewarmed, ready_qpc);
    if let Some(trace) = native_paint_trace.as_ref() {
        trace.emit_after_ready(ready_qpc);
    }
    if trace {
        crate::startup_probe::emit_trace_value(
            &serde_json::json!({"phase":"startup_compositor_prewarm",
            "pid":std::process::id(),"hwnd":hwnd.0 as usize,
            "compositor_prewarmed":compositor_prewarmed,"fallback_error":compositor_error,
            "synthetic_readback_prewarmed":synthetic_readback_prewarmed,
            "synthetic_readback_bounds":if synthetic_readback_prewarmed {
                Some([warm_bounds.left as i64,warm_bounds.top as i64,warm_bounds.width as i64,warm_bounds.height as i64])
            } else {None},
            "synthetic_readback_bytes":if synthetic_readback_prewarmed {
                Some(warm_bounds.width as usize*warm_bounds.height as usize*4)
            } else {None},
            "synthetic_readback_source":if synthetic_readback_prewarmed {Some("own-black-window-dc")} else {None},
            // 成功值来自预热函数全量 BGR 及真实回收回执；失败路径不发布成功元数据。
            "synthetic_readback_all_bgr_black":synthetic_readback_prewarmed,
            "synthetic_readback_restored":synthetic_readback_prewarmed,
            "synthetic_readback_bitmap_deleted":synthetic_readback_prewarmed,
            "synthetic_readback_dc_deleted":synthetic_readback_prewarmed,
            "synthetic_readback_flush_ok":synthetic_readback_prewarmed,
            "synthetic_readback_retained_cpu_bytes":0,
            "native_frozen_bitmap":runtime.startup_bitmap.is_some(),
            "native_frozen_bitmap_kind":runtime.startup_bitmap.as_ref().map(|bitmap|bitmap.kind()),
            "native_metadata_only":runtime.planes.iter().all(|plane|plane.image.is_none()),
            "native_frozen_bitmap_id":runtime.startup_bitmap.as_ref().map(|bitmap|bitmap.object_id().to_string()),
            "native_frozen_bitmap_source_bytes":runtime.startup_bitmap.as_ref().map(|bitmap|bitmap.source_bytes()),
            "native_frozen_bitmap_planes":runtime.planes.len(),
            "native_frozen_bitmap_whole_canvas_black_gaps_valid":runtime.startup_bitmap.as_ref().map(|bitmap|bitmap.whole_canvas_black_gaps_valid()),
            "native_frozen_bitmap_draw_method":runtime.startup_bitmap.as_ref().map(|bitmap| if uses_whole_canvas_bitmap(bitmap, runtime.bounds, &runtime.planes) { "whole-canvas-bitblt" } else { "monitor-planes-bitblt" }),
            "native_frozen_bitmap_batches":runtime.startup_bitmap.as_ref().map(|bitmap|bitmap.batch_count()),
            "native_frozen_bitmap_last_batch_planes":runtime.startup_bitmap.as_ref().map(|bitmap|bitmap.last_batch_planes()),
            "transitions_disabled_requested":true,
            "paint_method":if runtime.startup_bitmap.is_some() { "bitblt" } else { "stretch-dibits" },
            "window_has_original_frozen_content_before_uncloak":cloaked,
            "own_gpu_device_created":false,"system_compositor_allocations_queried":false}),
        );
    }
    for (stage, duration_ms) in stages {
        if stage == "native_ime_disable" {
            crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_stage\",\"stage\":\"{stage}\",\"duration_ms\":{duration_ms:.3},\"ok\":{ime_disabled}}}"));
        } else if stage == "native_foreground" || stage.starts_with("native_foreground_") {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_stage", "stage":stage, "pid":std::process::id(),
                "hwnd":hwnd.0 as usize, "duration_ms":duration_ms,
                "timing_scope":if stage == "native_foreground" {
                    "aggregate-includes-native_foreground_*;do-not-sum"
                } else {
                    "subset-of-native_foreground;do-not-sum"
                },
                "set_foreground_returned":foreground_set_foreground_ok,
                "pre_focus_query_ok":foreground_pre_focus_query_ok,
                "pre_focus_active":if foreground_pre_focus_query_ok {Some(foreground_pre_focus_active)} else {None},
                "pre_focus_focus":if foreground_pre_focus_query_ok {Some(foreground_pre_focus_focus)} else {None},
                "set_focus_called":foreground_set_focus_called,
                "before_dwm_focus_verified":true,
                "after_dwm_focus_verified":true,
            }));
        } else if stage.starts_with("native_show_") {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_stage", "stage":stage, "pid":std::process::id(),
                "hwnd":hwnd.0 as usize, "duration_ms":duration_ms,
                "timing_scope":"subset-of-native_show;do-not-sum",
                "compositor_prewarmed":compositor_prewarmed,
                "layered_configuration_reused":layered_configured,
                "placement_reused":native_show_reused_placement,
                "placement_called":!native_show_reused_placement,
                "request_extended_style_set_called":false,
                "final_extended_style":native_show_final_ex_style,
                "disabled_until_first_frozen_paint":true,
                "emitted_after_actual_surface_ready":true,
                "normal_qpc_calls_added":false,
            }));
        } else if stage == "native_queue_prepare" {
            crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_stage\",\"stage\":\"{stage}\",\"duration_ms\":{duration_ms:.3},\"timing_scope\":\"subset-of-native_request_setup;do-not-sum\"}}"));
        } else if stage == "native_wheel_settings" {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_stage", "stage":stage, "pid":std::process::id(),
                "duration_ms":duration_ms,
                "timing_scope":"request-wheel-query;outside-native_request_setup;may-overlap-capture",
                "query_mode":wheel_settings.mode,
                "query_begin_qpc":wheel_settings.query.timings.as_ref().map(|timings|timings.begin_qpc),
                "query_end_qpc":wheel_settings.query.timings.as_ref().map(|timings|timings.end_qpc),
                "query_thread_id":wheel_settings.query.timings.as_ref().map(|timings|timings.thread_id),
                "consumer_wait_ms":wheel_settings.consumer_wait_ms,
                "request_prepare_ms":wheel_settings.prepare_ms,
                "spawn_fallback_error":wheel_settings.spawn_error,
                "fresh_request_only":true, "spi_queries_per_request":2,
                "helper_worker_used":wheel_settings.mode=="parallel-request-worker",
                "helper_worker_joined":wheel_settings.mode=="parallel-request-worker"
            }));
        } else {
            crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_stage\",\"stage\":\"{stage}\",\"duration_ms\":{duration_ms:.3}}}"));
        }
    }
    if trace {
        let requested = metrics
            .request_to_ready_ms
            .map_or_else(|| "null".to_owned(), |v| format!("{v:.3}"));
        let token = unsafe { GetPropW(hwnd, w!("RustShot.StartupSurface.Token")) }.0 as usize;
        crate::startup_probe::emit_trace_args(format_args!("{{\"phase\":\"startup_surface_ready\",\"pid\":{},\"hwnd\":{},\"token\":{token},\"width\":{},\"height\":{},\"ready_ms\":{:.3},\"start_surface_to_ready_ms\":{:.3},\"request_to_ready_ms\":{},\"request_to_surface_ready_ms\":{},\"prewarmed\":{prewarmed},\"foreground\":true,\"ok\":true}}",
            std::process::id(), hwnd.0 as usize, bounds.width, bounds.height, metrics.ready_ms, metrics.start_surface_to_ready_ms, requested, requested));
    }
    shared
        .startup_stage
        .store(NativeStartupStage::Ready as u32, Ordering::Release);
    let _ = ready.send(Ok(()));
    let mut should_close = false;
    while !should_close && !shared.stop.load(Ordering::Acquire) {
        let mut message = MSG::default();
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if status == -1 {
            return Err(windows::core::Error::from_win32().into());
        }
        if status == 0 {
            break;
        }
        for index in 0..256 {
            unsafe {
                // Keystroke 快照已经解码字符；首屏没有 IME 输入区，不再生成 WM_CHAR。
                DispatchMessageW(&message);
            }
            while let Some(event) = native._queue.pop() {
                if event.raw.0 == WM_PAINT {
                    paint_window(hwnd, &resources, &runtime, &native._queue)?;
                } else {
                    should_close |=
                        runtime.input_with_arrow(hwnd, event, Some((shared, &native)))?;
                    runtime.publish(shared);
                }
                if should_close {
                    break;
                }
            }
            if native._queue.overflow.load(Ordering::Acquire) {
                return Err("快速首屏原生输入队列已满".into());
            }
            if should_close
                || shared.stop.load(Ordering::Acquire)
                || index == 255
                || !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool()
            {
                break;
            }
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Handoff { target, reply } => {
                    let result = runtime
                        .handoff(hwnd, HWND(target as *mut _))
                        .map_err(|e| e.to_string());
                    if matches!(&result, Ok(Some(state)) if !state.cancelled) {
                        shared.handed_off.store(true, Ordering::Release);
                    }
                    runtime.publish(shared);
                    let _ = reply.send(result);
                }
            }
        }
    }
    if shared.finish.load(Ordering::Acquire) && !runtime.state.cancelled {
        // close command 与 native 输入不同队列。销毁前消费所有已排队输入，避免丢掉尾部按键/抬起。
        for _ in 0..256 {
            let mut pending = MSG::default();
            if !unsafe { PeekMessageW(&mut pending, Some(hwnd), 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            unsafe {
                DispatchMessageW(&pending);
            }
            while let Some(event) = native._queue.pop() {
                if event.raw.0 != WM_PAINT && runtime.input(hwnd, event)? {
                    break;
                }
            }
            if runtime.state.cancelled {
                break;
            }
        }
        // SendMessage 在 PeekMessage 调用期间也可能仅向 EventQueue 入队。
        while let Some(event) = native._queue.pop() {
            if event.raw.0 != WM_PAINT && runtime.input(hwnd, event)? {
                break;
            }
        }
        if native._queue.overflow.load(Ordering::Acquire) {
            return Err("快速首屏交接尾部输入超限".into());
        }
        let mut remaining = MSG::default();
        if !runtime.state.cancelled
            && (unsafe {
                PeekMessageW(
                    &mut remaining,
                    Some(hwnd),
                    WM_KEYFIRST,
                    WM_KEYLAST,
                    PM_NOREMOVE,
                )
            }
            .as_bool()
                || unsafe {
                    PeekMessageW(
                        &mut remaining,
                        Some(hwnd),
                        WM_MOUSEFIRST,
                        WM_MOUSELAST,
                        PM_NOREMOVE,
                    )
                }
                .as_bool())
        {
            return Err("快速首屏交接时仍有过量排队输入".into());
        }
    }
    runtime.publish(shared);
    native._queue.begin_preview_paint();
    native.close()?;
    shared.hwnd.store(0, Ordering::Release);
    if runtime.state.cancelled {
        if let Some(cancel_token) = shared.cancel_token.as_ref() {
            // 输入已停且原 HWND 已销毁；不等待 GDI/GPUI 初始化或线程析构才让下一轮进入。
            cancel_token.native_cancelled_after_close();
        }
    }
    if shared.finish.load(Ordering::Acquire) && !runtime.state.cancelled && runtime.target.is_some()
    {
        runtime.pending.extend(runtime.deferred.drain(..));
        *shared.completed.lock().unwrap_or_else(|e| e.into_inner()) = runtime.pending;
        *shared
            .completed_output
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = runtime.early_output.finish(true);
    }
    Ok(())
}

fn record_native_stage(
    stages: &mut Vec<(&'static str, f64)>,
    stage: &'static str,
    started: Option<Instant>,
) {
    if let Some(started) = started {
        stages.push((stage, started.elapsed().as_secs_f64() * 1000.));
    }
}

fn paint_window(
    hwnd: HWND,
    resources: &PaintResources,
    runtime: &Runtime,
    queue: &EventQueue,
) -> Result<bool> {
    let preview = runtime
        .state
        .window_preview
        .filter(|_| runtime.state.crop.is_none());
    let selection = runtime.state.crop.or(preview);
    if selection.is_some() {
        // 空更新区不清旧回执、不制造新绘制。更新原生预览必须画完整客户区，
        // 不能用一次局部 rcPaint 的成功把新遮罩/外框冒作整幅完成。
        if !unsafe { GetUpdateRect(hwnd, None, false) }.as_bool() {
            return Ok(false);
        }
        if !unsafe { InvalidateRect(Some(hwnd), None, false) }.as_bool() {
            return Err("快速首屏窗口预览完整更新区创建失败".into());
        }
    }
    let painted = paint_window_with_trace::<false>(
        hwnd,
        resources,
        runtime,
        selection.is_some(),
        Some(queue),
        None,
    )?;
    if painted {
        if selection.is_some() && unsafe { DwmFlush() } < 0 {
            return Err("快速首屏窗口预览合成提交失败".into());
        }
        // 这里只有真实 BeginPaint/draw/GdiFlush/EndPaint 都完成才推进回执。
        // 无 preview 的画面清空旧范围，不把它误标为可见窗口。
        queue.publish_completed_selection(selection, preview)?;
    }
    Ok(painted)
}

fn paint_first_window_dc_with_trace<const TRACING: bool>(
    mut window_dc: WindowDc,
    resources: &PaintResources,
    runtime: &Runtime,
    mut trace: Option<&mut NativePaintTrace>,
) -> Result<bool> {
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.paint_enter_qpc);
    if let Some(trace) = trace.as_deref_mut() {
        trace.paint_mode = "window-dc";
        trace.full_canvas_required = true;
        // 直接 DC 的覆盖证明在 window_dc_coverage；没有 PAINTSTRUCT，不能伪造 rcPaint。
    }
    let drawn = resources.draw_with_trace::<TRACING>(
        window_dc.dc,
        runtime.bounds,
        &runtime.planes,
        runtime.startup_bitmap.as_deref(),
        &runtime.state,
        trace.as_deref_mut(),
    );
    // 绘制失败仍在同一线程检查释放；既不继续显示，也不升级为第二次绘制回退。
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.window_dc_release_begin_qpc);
    let released = window_dc.release_checked();
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.window_dc_release_end_qpc);
    if let Some(trace) = trace.as_deref_mut() {
        trace.window_dc_release_succeeded = released.is_ok();
    }
    let result = match (drawn, released) {
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => {
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| {
                &mut trace.window_dc_validate_begin_qpc
            });
            // 已真实绘制并完成同线程 flush/release，才清空迟到 WM_PAINT 的更新区。
            let validated = unsafe { ValidateRect(Some(window_dc.hwnd), None) }.as_bool();
            NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| {
                &mut trace.window_dc_validate_end_qpc
            });
            if let Some(trace) = trace.as_deref_mut() {
                trace.window_dc_validate_succeeded = validated;
            }
            if validated {
                Ok(true)
            } else {
                Err("快速首屏实际客户区首绘更新区清理失败".into())
            }
        }
    };
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.paint_end_qpc);
    result
}

fn paint_window_with_trace<const TRACING: bool>(
    hwnd: HWND,
    resources: &PaintResources,
    runtime: &Runtime,
    full_canvas_required: bool,
    preview_queue: Option<&EventQueue>,
    mut trace: Option<&mut NativePaintTrace>,
) -> Result<bool> {
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.paint_enter_qpc);
    if let Some(trace) = trace.as_deref_mut() {
        trace.paint_mode = "begin-paint";
    }
    // 首绘刚完成整客户区 Invalidate，直接核验 BeginPaint 的物理更新区，
    // 省去一次重复 USER 查询；之后的迟到 WM_PAINT 仍先拒绝空更新区。
    if !full_canvas_required && !unsafe { GetUpdateRect(hwnd, None, false) }.as_bool() {
        return Ok(false);
    }
    if let Some(queue) = preview_queue {
        // BeginPaint 可同步派发消息；在任何绘制/擦背景之前撤销回执，并立即放锁。
        queue.begin_preview_paint();
    }
    let mut paint = PAINTSTRUCT::default();
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.begin_paint_begin_qpc);
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.begin_paint_end_qpc);
    if let Some(trace) = trace.as_deref_mut() {
        trace.full_canvas_required = full_canvas_required;
        trace.paint_rect = [
            paint.rcPaint.left,
            paint.rcPaint.top,
            paint.rcPaint.right,
            paint.rcPaint.bottom,
        ];
    }
    let nonempty =
        paint.rcPaint.right > paint.rcPaint.left && paint.rcPaint.bottom > paint.rcPaint.top;
    let result = if dc.is_invalid() {
        Err("快速首屏重绘 DC 创建失败".into())
    } else if full_canvas_required
        && (paint.rcPaint.left != 0
            || paint.rcPaint.top != 0
            || paint.rcPaint.right != runtime.bounds.width as i32
            || paint.rcPaint.bottom != runtime.bounds.height as i32)
    {
        Err("快速首屏首次绘制更新区没有覆盖完整物理客户区".into())
    } else if !nonempty {
        Ok(())
    } else if preview_queue.is_none() {
        // 首绘（尚未有输入回执队列）仍直画，保持首屏时序。注意 full_canvas_required
        // 在交互重绘中等于“存在选区”，不能用它区分首绘，否则拖拽时恰好走直画路径。
        resources.draw_with_trace::<TRACING>(
            dc,
            runtime.bounds,
            &runtime.planes,
            runtime.startup_bitmap.as_deref(),
            &runtime.state,
            trace.as_deref_mut(),
        )
    } else {
        // 交互重绘（拖拽框选、窗口预览切换）整幅离屏合成，避免选区外侧亮暗闪烁。
        resources.draw_buffered_with_trace::<TRACING>(
            dc,
            paint.rcPaint,
            runtime.bounds,
            &runtime.planes,
            runtime.startup_bitmap.as_deref(),
            &runtime.state,
            trace.as_deref_mut(),
        )
    };
    // 绘制失败也执行 EndPaint；只有绘制与回收都成功才返回真实绘制回执。
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.end_paint_begin_qpc);
    let ended = unsafe { EndPaint(hwnd, &paint) }.as_bool();
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.end_paint_end_qpc);
    NativePaintTrace::stamp::<TRACING>(&mut trace, |trace| &mut trace.paint_end_qpc);
    result?;
    if !ended {
        return Err("快速首屏重绘 DC 回收失败".into());
    }
    Ok(nonempty)
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmFlush() -> i32;
    fn DwmSetWindowAttribute(
        hwnd: HWND,
        attribute: u32,
        value: *const std::ffi::c_void,
        size: u32,
    ) -> i32;
    fn DwmGetWindowAttribute(
        hwnd: HWND,
        attribute: u32,
        value: *mut std::ffi::c_void,
        size: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}

#[cfg(test)]
mod native_crop_adjust_cursor_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        capture_cursor_nudge::NativeArrowRoute,
    };

    fn own() -> HWND {
        HWND(std::ptr::dangling_mut())
    }
    fn packed(point: (i16, i16)) -> isize {
        (u32::from(point.0 as u16) | (u32::from(point.1 as u16) << 16)) as isize
    }
    fn crop() -> Rect {
        Rect {
            x: 100,
            y: 80,
            width: 120,
            height: 100,
        }
    }
    fn owner(runtime: &Runtime) -> NativeIdleCursorSource {
        NativeIdleCursorSource {
            hwnd: 1,
            thread: 1,
            token: 17,
            bounds: runtime.bounds,
        }
    }
    fn event(runtime: &Runtime, raw: RawInput, shift: bool) -> CapturedInput {
        CapturedInput::for_test(
            raw,
            runtime.bounds,
            (3, 3),
            gpui::Modifiers {
                shift,
                ..Default::default()
            },
        )
    }
    fn idle() -> Runtime {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        runtime.editor.set_crop(Some(crop()));
        runtime
    }
    fn adjust(start: (i32, i32), inside: bool) -> Runtime {
        let mut runtime = idle();
        if inside {
            runtime.editor.select_tool(Tool::Edit);
        }
        let admitted = crate::capture_cursor_nudge::crop_adjust_cursor_admission(
            &runtime.editor,
            true,
            true,
            false,
            false,
        );
        runtime.state.pointer = start;
        runtime.state.anchor = Some(start);
        runtime.button_down = true;
        runtime.click_gesture.start(start);
        runtime.click_started = Some((start, Instant::now()));
        if inside {
            assert!(runtime.editor.begin_screenshot_empty_crop_inside(start, 6.));
        } else {
            runtime.editor.pointer_down(start, 6.);
        }
        let source = owner(&runtime);
        // 只注入原 Down 接受结果；不调用真实窗口输入、捕获或 Set/GetCursorPos。
        runtime.accept_adjust_cursor_down(Some(source), admitted);
        runtime
    }
    fn prime(runtime: &mut Runtime, key: usize, shift: bool) -> CapturedInput {
        let down = event(runtime, (WM_KEYDOWN, key, 0), shift);
        assert!(matches!(
            runtime
                .native_arrow
                .route(down.raw, down.modifiers, runtime.adjust_cursor_ready()),
            NativeArrowRoute::Move(_)
        ));
        down
    }
    fn readback(runtime: &mut Runtime, down: &CapturedInput, delta: (i32, i32), ready: bool) {
        let start = runtime.adjust_cursor_origin.unwrap();
        let before = (runtime.bounds.left + start.0, runtime.bounds.top + start.1);
        let source = owner(runtime);
        runtime.record_native_cursor_readback(
            down,
            source,
            NativeCursorReadback {
                before,
                target: (before.0 + 1, before.1),
                actual: (before.0 + delta.0, before.1 + delta.1),
            },
            ready,
        );
    }

    #[test]
    fn native_adjust_source_keeps_only_accepted_original_resize_inside_and_move() {
        for (start, inside) in [((60, 130), false), ((101, 130), true), ((160, 130), false)] {
            let runtime = adjust(start, inside);
            assert!(runtime.adjust_cursor_ready());
            assert_eq!(runtime.adjust_cursor_origin, Some(start));
            assert_eq!(runtime.adjust_cursor_owner, Some(owner(&runtime)));
            assert_eq!(runtime.editor.crop_creation_before(), None);
            assert!(runtime.editor.is_changing_crop());
            assert_eq!(runtime.manual_cursor_origin, None);
            assert_eq!(runtime.new_cursor_origin, None);
        }
        for rejection in 0..6 {
            let mut runtime = adjust((160, 130), false);
            if rejection == 2 {
                runtime.manual_cursor_origin = Some((160, 130));
            }
            if rejection == 3 {
                runtime.button_down = false;
            }
            if rejection == 4 {
                runtime.editor.set_crop(None);
                runtime.editor.pointer_down((160, 130), 6.);
                assert_eq!(runtime.editor.crop_creation_before(), Some(None));
            }
            if rejection == 5 {
                runtime.editor.select_tool(Tool::Lasso);
                runtime.editor.pointer_down((60, 130), 6.);
                assert_eq!(runtime.editor.crop_creation_before(), Some(Some(crop())));
            }
            let source = owner(&runtime);
            runtime.accept_adjust_cursor_down((rejection != 1).then_some(source), rejection != 0);
            assert_eq!(runtime.adjust_cursor_origin, None);
            assert!(!runtime.adjust_cursor_ready());
        }
        // Tool 标签不缩窄已接受 Crop 来源，也不能把真实标注 Down 当范围调整。
        let mut drawing = idle();
        drawing.editor.select_tool(Tool::Rectangle);
        let admitted = crate::capture_cursor_nudge::crop_adjust_cursor_admission(
            &drawing.editor,
            true,
            true,
            false,
            false,
        );
        assert!(admitted);
        drawing.state.pointer = (160, 130);
        drawing.button_down = true;
        drawing.editor.pointer_down((160, 130), 6.);
        assert!(drawing.editor.is_dragging());
        assert!(!drawing.editor.is_changing_crop());
        let source = owner(&drawing);
        drawing.accept_adjust_cursor_down(Some(source), admitted);
        assert_eq!(drawing.adjust_cursor_origin, None);
        assert!(!drawing.adjust_cursor_ready());

        let mut runtime = adjust((160, 130), false);
        let tool = event(&runtime, (WM_KEYDOWN, 0x54, 0), false);
        runtime.pending.push(tool);
        assert!(runtime.adjust_cursor_claimed());
        assert!(!runtime.adjust_cursor_ready());
        runtime.editor.cancel_gesture();
        assert!(!runtime.adjust_cursor_ready());
    }

    #[test]
    fn native_adjust_eight_keys_accelerate_without_changing_original_mode_or_crop() {
        for (start, inside) in [((60, 130), false), ((101, 130), true), ((160, 130), false)] {
            for shift in [false, true] {
                for (key, axis) in [
                    (0x25, (-1, 0)),
                    (0x26, (0, -1)),
                    (0x27, (1, 0)),
                    (0x28, (0, 1)),
                    (0x41, (-1, 0)),
                    (0x57, (0, -1)),
                    (0x44, (1, 0)),
                    (0x53, (0, 1)),
                ] {
                    let mut runtime = adjust(start, inside);
                    let revision = (
                        runtime.editor.document.revision(),
                        runtime.editor.document.edit_revision(),
                    );
                    let down = event(&runtime, (WM_KEYDOWN, key, 0), shift);
                    assert_eq!(
                        runtime.native_arrow.route(
                            down.raw,
                            down.modifiers,
                            runtime.adjust_cursor_ready()
                        ),
                        NativeArrowRoute::Move(axis)
                    );
                    let held = event(&runtime, (WM_KEYDOWN, key, 1 << 30), shift);
                    for repeat in 1..=42 {
                        let step = (1 + repeat / 2).min(20);
                        assert_eq!(
                            runtime.native_arrow.route(
                                held.raw,
                                held.modifiers,
                                runtime.adjust_cursor_ready()
                            ),
                            NativeArrowRoute::Move((axis.0 * step, axis.1 * step))
                        );
                    }
                    assert_eq!(runtime.state.pointer, start);
                    assert_eq!(runtime.adjust_cursor_origin, Some(start));
                    assert_eq!(runtime.editor.crop, Some(crop()));
                    assert_eq!(runtime.editor.crop_creation_before(), None);
                    assert!(runtime.editor.is_changing_crop());
                    assert_eq!(
                        (
                            runtime.editor.document.revision(),
                            runtime.editor.document.edit_revision()
                        ),
                        revision
                    );
                    assert!(runtime.native_idle_source.is_none());
                    // drain无live只认领旧周期，不把A/S/Shift交给旧动作或setter。
                    assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                    let up = event(&runtime, (WM_KEYUP, key, 1 << 31), shift);
                    assert!(runtime.native_arrow_input(HWND::default(), &up, None));
                    assert!(!runtime.native_arrow_input(HWND::default(), &up, None));
                    assert!(runtime.pending.is_empty());
                }
            }
        }
        let mut runtime = adjust((160, 130), false);
        let unknown = event(&runtime, (WM_KEYDOWN, 0x41, 1 << 30), false);
        assert!(runtime.native_arrow_input(HWND::default(), &unknown, None));
        assert_eq!(
            runtime.native_arrow.route(
                unknown.raw,
                unknown.modifiers,
                runtime.adjust_cursor_ready()
            ),
            NativeArrowRoute::Pass
        );
    }

    #[test]
    fn native_adjust_outside_zones_inside_offset_and_move_keep_shift_geometry_through_up() {
        for (start, expected, corner) in [
            (
                (60, 40),
                Rect {
                    x: 62,
                    y: 42,
                    width: 158,
                    height: 138,
                },
                true,
            ),
            (
                (160, 40),
                Rect {
                    x: 100,
                    y: 42,
                    width: 120,
                    height: 138,
                },
                false,
            ),
            (
                (260, 40),
                Rect {
                    x: 100,
                    y: 42,
                    width: 162,
                    height: 138,
                },
                true,
            ),
            (
                (60, 130),
                Rect {
                    x: 62,
                    y: 80,
                    width: 158,
                    height: 100,
                },
                false,
            ),
            (
                (260, 130),
                Rect {
                    x: 100,
                    y: 80,
                    width: 162,
                    height: 100,
                },
                false,
            ),
            (
                (60, 220),
                Rect {
                    x: 62,
                    y: 80,
                    width: 158,
                    height: 142,
                },
                true,
            ),
            (
                (160, 220),
                Rect {
                    x: 100,
                    y: 80,
                    width: 120,
                    height: 142,
                },
                false,
            ),
            (
                (260, 220),
                Rect {
                    x: 100,
                    y: 80,
                    width: 162,
                    height: 142,
                },
                true,
            ),
        ] {
            for shift in [false, true] {
                let mut runtime = adjust(start, false);
                let down = prime(&mut runtime, 0x44, shift);
                readback(&mut runtime, &down, (1, 0), true);
                assert_eq!(runtime.editor.crop, Some(crop()));
                let next = ((start.0 + 2) as i16, (start.1 + 2) as i16);
                let movement = event(&runtime, (WM_MOUSEMOVE, 1, packed(next)), shift);
                runtime.mouse_move(movement).unwrap();
                let move_crop = runtime.editor.crop;
                let up = event(&runtime, (WM_LBUTTONUP, 0, packed(next)), shift);
                runtime.input(own(), up).unwrap();
                assert_eq!(runtime.editor.crop, move_crop);
                let final_crop = runtime.editor.crop.unwrap();
                if shift && corner {
                    assert!(
                        (i64::from(final_crop.width) * 100 - i64::from(final_crop.height) * 120)
                            .abs()
                            <= 120
                    );
                } else {
                    assert_eq!(final_crop, expected);
                }
                assert_eq!(runtime.adjust_cursor_origin, None);
                assert_eq!(runtime.manual_cursor_origin, None);
                assert_eq!(runtime.new_cursor_origin, None);
                assert!(!runtime.editor.is_dragging());
                assert!(runtime.state.click_seed.is_none());
            }
        }
        for (start, inside, expected) in [
            (
                (101, 130),
                true,
                Rect {
                    x: 102,
                    y: 80,
                    width: 118,
                    height: 100,
                },
            ),
            (
                (160, 130),
                false,
                Rect {
                    x: 102,
                    y: 82,
                    width: 120,
                    height: 100,
                },
            ),
        ] {
            for shift in [false, true] {
                let mut runtime = adjust(start, inside);
                let down = prime(&mut runtime, 0x53, shift);
                readback(&mut runtime, &down, (1, 1), true);
                let up = event(
                    &runtime,
                    (
                        WM_LBUTTONUP,
                        0,
                        packed(((start.0 + 2) as i16, (start.1 + 2) as i16)),
                    ),
                    shift,
                );
                runtime.input(own(), up).unwrap();
                assert_eq!(runtime.editor.crop, Some(expected));
                assert!(!runtime.editor.is_dragging());
            }
        }
    }

    #[test]
    fn native_adjust_cancel_return_and_changed_up_preserve_mask_and_object_history() {
        for finish in 0..3 {
            let mut runtime = adjust((160, 130), false);
            let mask = crate::crop_mask::Polygon::new(&[
                Point { x: 100., y: 80. },
                Point { x: 220., y: 80. },
                Point { x: 160., y: 180. },
            ])
            .unwrap();
            runtime.editor.document.set_crop_mask(Some(mask.clone()));
            let mut object = Annotation::new(
                Tool::Rectangle,
                Point { x: 120., y: 100. },
                Style::default(),
            );
            object.update(Point { x: 150., y: 130. });
            let id = runtime.editor.document.insert(object.clone());
            let extra = runtime.editor.document.insert(object.clone());
            runtime.editor.document.undo();
            assert!(runtime.editor.document.can_redo());
            let revision = runtime.editor.document.revision();
            let edit_revision = runtime.editor.document.edit_revision();
            let down = prime(&mut runtime, 0x41, false);
            readback(&mut runtime, &down, (1, 0), true);
            let movement = event(&runtime, (WM_MOUSEMOVE, 1, packed((162, 132))), false);
            runtime.mouse_move(movement).unwrap();
            assert_eq!(runtime.editor.document.revision(), revision);
            if finish == 0 {
                assert!(!runtime.cancel(own(), true));
            } else {
                let point = if finish == 1 { (160, 130) } else { (162, 132) };
                let up = event(&runtime, (WM_LBUTTONUP, 0, packed(point)), false);
                runtime.input(own(), up).unwrap();
                assert!(runtime.state.click_seed.is_none());
                let duplicate = event(&runtime, (WM_LBUTTONUP, 0, packed(point)), false);
                runtime.input(own(), duplicate).unwrap();
            }
            assert_eq!(runtime.editor.document.edit_revision(), edit_revision);
            assert_eq!(runtime.editor.document.get(id), Some(&object));
            assert_eq!(
                runtime.editor.document.revision(),
                revision + u64::from(finish == 2)
            );
            assert_eq!(
                runtime.editor.document.crop_mask,
                (finish != 2).then_some(mask)
            );
            assert_eq!(
                runtime.editor.crop,
                Some(if finish == 2 {
                    Rect {
                        x: 102,
                        y: 82,
                        width: 120,
                        height: 100,
                    }
                } else {
                    crop()
                })
            );
            runtime.editor.document.redo();
            assert_eq!(runtime.editor.document.get(extra), Some(&object));
            runtime.editor.document.undo();
            assert_eq!(runtime.editor.document.get(id), Some(&object));
            assert!(runtime.editor.document.get(extra).is_none());
        }
    }

    #[test]
    fn native_adjust_actual_motion_and_focus_loss_keep_sticky_without_fresh_owner_repair() {
        for raw in [
            (WM_KILLFOCUS, 0, 0),
            (WM_CANCELMODE, 0, 0),
            (WM_ACTIVATE, WA_INACTIVE as usize, 0),
            (WM_ACTIVATEAPP, 0, 0),
        ] {
            for motion in [0, 1, 8] {
                for ready_after_commit in [false, true] {
                    let mut runtime = adjust((160, 130), false);
                    let down = prime(&mut runtime, 0x41, false);
                    readback(&mut runtime, &down, (motion, 0), ready_after_commit);
                    assert_eq!(runtime.adjust_cursor_moved, motion != 0);
                    assert_eq!(
                        runtime.adjust_cursor_owner,
                        ready_after_commit.then(|| owner(&runtime))
                    );
                    assert_eq!(runtime.adjust_cursor_ready(), ready_after_commit);
                    // true 分支必须由实际失焦撤有效来源，不能先用 postcommit=false 清掉。
                    let loss = event(&runtime, raw, false);
                    runtime.input(own(), loss).unwrap();
                    assert_eq!(runtime.adjust_cursor_owner, None);
                    assert_eq!(runtime.adjust_cursor_origin, Some((160, 130)));
                    assert_eq!(runtime.adjust_cursor_moved, motion != 0);
                    assert!(runtime.adjust_cursor_claimed());
                    assert!(!runtime.adjust_cursor_ready());
                    let held = event(&runtime, (WM_KEYDOWN, 0x41, 1 << 30), false);
                    assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                    let fresh = event(&runtime, (WM_KEYDOWN, 0x44, 0), false);
                    assert!(runtime.native_arrow_input(HWND::default(), &fresh, None));
                    let held = event(&runtime, (WM_KEYDOWN, 0x44, 1 << 30), false);
                    assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                    assert_eq!(runtime.adjust_cursor_owner, None);
                    assert!(!runtime.adjust_cursor_ready());
                    assert_eq!(runtime.state.pointer, (160, 130));
                    assert_eq!(runtime.editor.crop, Some(crop()));
                    assert!(runtime.pending.is_empty());
                    assert!(runtime.deferred.is_empty());
                    let returned = event(&runtime, (WM_MOUSEMOVE, 1, packed((160, 130))), false);
                    runtime.mouse_move(returned).unwrap();
                    let up = event(&runtime, (WM_LBUTTONUP, 0, packed((160, 130))), false);
                    runtime.input(own(), up).unwrap();
                    assert_eq!(runtime.editor.crop, Some(crop()));
                    if motion != 0 {
                        assert!(runtime.state.click_seed.is_none());
                    }
                    assert_eq!(runtime.adjust_cursor_origin, None);
                    for key in [0x41, 0x44] {
                        let up = event(&runtime, (WM_KEYUP, key, 1 << 31), false);
                        assert!(runtime.native_arrow_input(HWND::default(), &up, None));
                        assert!(!runtime.native_arrow_input(HWND::default(), &up, None));
                    }
                }
            }
        }
    }

    #[test]
    fn native_adjust_cancel_capture_loss_target_and_drain_consume_without_late_motion() {
        for boundary in 0..4 {
            let mut runtime = adjust((160, 130), false);
            prime(&mut runtime, 0x53, true);
            match boundary {
                0 => {
                    assert!(!runtime.cancel(own(), true));
                }
                1 => {
                    let loss = event(&runtime, (WM_CAPTURECHANGED, 0, 2), false);
                    runtime.input(own(), loss).unwrap();
                    assert_eq!(runtime.adjust_cursor_origin, None);
                }
                2 => {
                    runtime.target = Some((own(), 1));
                    runtime.native_arrow.input_lost();
                }
                _ => {} // 关闭drain没有live，绝不进入Win32移鼠资格。
            }
            let before = runtime.editor.crop;
            let held = event(&runtime, (WM_KEYDOWN, 0x53, 1 << 30), true);
            assert!(runtime.native_arrow_input(HWND::default(), &held, None));
            assert_eq!(runtime.state.pointer, (160, 130));
            assert_eq!(runtime.editor.crop, before);
            assert!(runtime.pending.is_empty());
            assert!(runtime.deferred.is_empty());
            let up = event(&runtime, (WM_KEYUP, 0x53, 1 << 31), true);
            assert!(runtime.native_arrow_input(HWND::default(), &up, None));
            assert!(!runtime.native_arrow_input(HWND::default(), &up, None));
            runtime.clear_adjust_cursor();
            runtime.button_down = false;
            let fresh = event(&runtime, (WM_KEYDOWN, 0x53, 0), true);
            assert!(!runtime.native_arrow_input(HWND::default(), &fresh, None));
        }
    }
}
#[cfg(test)]
mod native_new_crop_cursor_tests {
    use super::*;
    use crate::capture_cursor_nudge::NativeArrowRoute;

    fn own() -> HWND {
        HWND(std::ptr::dangling_mut())
    }
    fn packed(point: (i16, i16)) -> isize {
        (u32::from(point.0 as u16) | (u32::from(point.1 as u16) << 16)) as isize
    }
    fn source(runtime: &Runtime) -> NativeIdleCursorSource {
        NativeIdleCursorSource {
            hwnd: 1,
            thread: 1,
            token: 17,
            bounds: runtime.bounds,
        }
    }
    fn event(runtime: &Runtime, raw: RawInput, shift: bool) -> CapturedInput {
        CapturedInput::for_test(
            raw,
            runtime.bounds,
            (3, 3),
            gpui::Modifiers {
                shift,
                ..Default::default()
            },
        )
    }
    fn new_runtime(shift: bool) -> Runtime {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        let admitted =
            crate::capture_cursor_nudge::preview_ready(&runtime.editor, true, true, false, false);
        runtime.state.anchor = Some((40, 50));
        runtime.button_down = true;
        runtime.click_gesture.start((40, 50));
        runtime.click_started = Some(((40, 50), Instant::now()));
        runtime.editor.pointer_down_with_shift((40, 50), 6., shift);
        // 注入已接受 Down 的来源；不调用实际 SetCapture 或窗口输入 API。
        let owner = source(&runtime);
        runtime.accept_new_cursor_down(Some(owner), admitted);
        runtime
    }
    fn target(runtime: &mut Runtime) {
        runtime.targets.push(WindowTarget {
            rect: Rect::full(320, 480),
            global_bounds: RECT::default(),
            title: String::new(),
            hwnd: 0,
            process_id: 0,
            thread_id: 0,
        });
    }
    fn key_cycle(runtime: &mut Runtime, key: usize, shift: bool) -> CapturedInput {
        let down = event(runtime, (WM_KEYDOWN, key, 0), shift);
        assert!(matches!(
            runtime
                .native_arrow
                .route(down.raw, down.modifiers, runtime.new_cursor_ready()),
            NativeArrowRoute::Move(_)
        ));
        down
    }

    #[test]
    fn native_new_source_requires_original_down_admission_and_never_becomes_manual_or_resize() {
        let mut runtime = new_runtime(false);
        assert!(runtime.new_cursor_ready());
        assert_eq!(runtime.new_cursor_origin, Some((40, 50)));
        assert_eq!(runtime.new_cursor_owner, Some(source(&runtime)));
        assert_eq!(runtime.manual_cursor_origin, None);
        assert_eq!(runtime.editor.crop_creation_before(), Some(None));
        runtime.editor.pointer_move((75, 68));
        assert!(runtime.editor.crop.is_some());
        assert!(runtime.new_cursor_ready());

        for rejection in 0..5 {
            let mut runtime = new_runtime(false);
            if rejection == 2 {
                runtime.manual_cursor_origin = Some((40, 50));
            }
            if rejection == 3 {
                runtime.editor.set_crop(Some(Rect::full(100, 100)));
                runtime.editor.pointer_down((40, 50), 6.);
                assert_eq!(runtime.editor.crop_creation_before(), None);
            }
            if rejection == 4 {
                runtime.button_down = false;
            }
            let owner = source(&runtime);
            runtime.accept_new_cursor_down((rejection != 1).then_some(owner), rejection != 0);
            assert_eq!(runtime.new_cursor_origin, None);
            assert!(!runtime.new_cursor_ready());
        }
        let mut runtime = new_runtime(false);
        let tool = event(&runtime, (WM_KEYDOWN, 0x54, 0), false);
        runtime.pending.push(tool);
        assert!(runtime.new_cursor_claimed());
        assert!(!runtime.new_cursor_ready());
        let owner = runtime.new_cursor_owner;
        runtime.editor.cancel_gesture();
        assert!(runtime.new_cursor_claimed());
        assert!(!runtime.new_cursor_ready());
        assert_eq!(runtime.new_cursor_owner, owner);
    }

    #[test]
    fn native_new_eight_keys_accelerate_without_editing_original_new_or_adopting_idle_keys() {
        for shift in [false, true] {
            for (key, axis) in [
                (0x25, (-1, 0)),
                (0x26, (0, -1)),
                (0x27, (1, 0)),
                (0x28, (0, 1)),
                (0x41, (-1, 0)),
                (0x57, (0, -1)),
                (0x44, (1, 0)),
                (0x53, (0, 1)),
            ] {
                let mut runtime = new_runtime(shift);
                let revision = runtime.editor.document.revision();
                let down = event(&runtime, (WM_KEYDOWN, key, 0), shift);
                assert_eq!(
                    runtime.native_arrow.route(
                        down.raw,
                        down.modifiers,
                        runtime.new_cursor_ready()
                    ),
                    NativeArrowRoute::Move(axis)
                );
                let held = event(&runtime, (WM_KEYDOWN, key, 1 << 30), shift);
                for repeat in 1..=42 {
                    let step = (1 + repeat / 2).min(20);
                    assert_eq!(
                        runtime.native_arrow.route(
                            held.raw,
                            held.modifiers,
                            runtime.new_cursor_ready()
                        ),
                        NativeArrowRoute::Move((axis.0 * step, axis.1 * step))
                    );
                }
                assert_eq!(runtime.new_cursor_origin, Some((40, 50)));
                assert_eq!(runtime.manual_cursor_origin, None);
                assert_eq!(runtime.editor.crop_creation_before(), Some(None));
                assert_eq!(runtime.editor.crop, None);
                assert_eq!(runtime.editor.document.revision(), revision);
                // 无 live 的旧队列只能认领，不把 A/S 交给工具或将 Shift Arrow 变成 set_crop。
                assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                assert!(runtime.pending.is_empty());
                assert!(runtime.editor.is_changing_crop());
                let up = event(&runtime, (WM_KEYUP, key, 1 << 31), shift);
                assert!(runtime.native_arrow_input(HWND::default(), &up, None));
                assert!(!runtime.native_arrow_input(HWND::default(), &up, None));
            }
        }
        let mut runtime = new_runtime(false);
        let unknown = event(&runtime, (WM_KEYDOWN, 0x41, 1 << 30), false);
        assert!(runtime.native_arrow_input(HWND::default(), &unknown, None));
        assert_eq!(
            runtime
                .native_arrow
                .route(unknown.raw, unknown.modifiers, runtime.new_cursor_ready()),
            NativeArrowRoute::Pass
        );
        assert!(runtime.native_idle_source.is_none());
    }

    #[test]
    fn native_new_actual_motion_survives_owner_loss_and_return_up_without_old_target_or_seed() {
        for actual_motion in [0, 1, 8] {
            for returned_mouse_move in [false, true] {
                let mut runtime = new_runtime(false);
                target(&mut runtime);
                let down = key_cycle(&mut runtime, 0x27, false);
                let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
                let actual = (before.0 + actual_motion, before.1);
                let owner = source(&runtime);
                runtime.record_native_cursor_readback(
                    &down,
                    owner,
                    NativeCursorReadback {
                        before,
                        target: (before.0 + 1, before.1),
                        actual,
                    },
                    false,
                );
                assert_eq!(runtime.new_cursor_moved, actual_motion != 0);
                assert_eq!(runtime.state.pointer, (40, 50));
                assert_eq!(runtime.new_cursor_origin, Some((40, 50)));
                assert_eq!(runtime.manual_cursor_origin, None);
                assert_eq!(runtime.editor.crop, None);
                assert_eq!(runtime.native_idle_source, None);
                if returned_mouse_move {
                    let returned = event(&runtime, (WM_MOUSEMOVE, 1, packed((40, 50))), false);
                    runtime.mouse_move(returned).unwrap();
                }
                let up = event(&runtime, (WM_LBUTTONUP, 0, packed((40, 50))), false);
                runtime.input(own(), up).unwrap();
                assert_eq!(
                    runtime.editor.crop,
                    (actual_motion == 0).then_some(Rect::full(320, 480))
                );
                assert_eq!(runtime.state.click_seed.is_some(), actual_motion == 0);
                assert_eq!(runtime.new_cursor_origin, None);
                assert_eq!(runtime.new_cursor_owner, None);
                let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30), false);
                assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                let up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31), false);
                assert!(runtime.native_arrow_input(HWND::default(), &up, None));
            }
        }
    }

    #[test]
    fn native_new_shift_move_final_up_keeps_square_and_one_crop_finish_revision() {
        for shift in [false, true] {
            let mut runtime = new_runtime(shift);
            let revision = runtime.editor.document.revision();
            let edit_revision = runtime.editor.document.edit_revision();
            let down = key_cycle(&mut runtime, 0x53, shift);
            let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
            let owner = source(&runtime);
            runtime.record_native_cursor_readback(
                &down,
                owner,
                NativeCursorReadback {
                    before,
                    target: (before.0, before.1 + 1),
                    actual: (before.0, before.1 + 1),
                },
                true,
            );
            assert_eq!(runtime.editor.crop, None);
            assert_eq!(runtime.editor.document.revision(), revision);
            let movement = event(&runtime, (WM_MOUSEMOVE, 1, packed((62, 55))), shift);
            runtime.mouse_move(movement).unwrap();
            assert_eq!(
                runtime.editor.crop,
                Some(Rect {
                    x: 40,
                    y: 50,
                    width: 22,
                    height: if shift { 22 } else { 5 }
                })
            );
            assert!(runtime.new_cursor_ready());
            assert_eq!(runtime.manual_cursor_origin, None);
            assert_eq!(runtime.new_cursor_origin, Some((40, 50)));
            let up = event(&runtime, (WM_LBUTTONUP, 0, packed((63, 57))), shift);
            runtime.input(own(), up).unwrap();
            assert_eq!(
                runtime.editor.crop,
                Some(Rect {
                    x: 40,
                    y: 50,
                    width: 23,
                    height: if shift { 23 } else { 7 }
                })
            );
            assert_eq!(runtime.editor.document.revision(), revision + 1);
            assert_eq!(runtime.editor.document.edit_revision(), edit_revision);
            assert!(runtime.editor.document.items().is_empty());
            assert_eq!(runtime.new_cursor_origin, None);
            assert!(runtime.state.click_seed.is_none());
            let duplicate = event(&runtime, (WM_LBUTTONUP, 0, packed((63, 57))), shift);
            runtime.input(own(), duplicate).unwrap();
            assert_eq!(runtime.editor.document.revision(), revision + 1);
        }
    }

    #[test]
    fn native_new_focus_loss_revokes_owner_without_erasing_original_down_or_actual_motion() {
        for raw in [
            (WM_KILLFOCUS, 0, 0),
            (WM_CANCELMODE, 0, 0),
            (WM_ACTIVATE, WA_INACTIVE as usize, 0),
            (WM_ACTIVATEAPP, 0, 0),
        ] {
            let mut runtime = new_runtime(false);
            target(&mut runtime);
            let down = key_cycle(&mut runtime, 0x41, false);
            let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
            let owner = source(&runtime);
            runtime.record_native_cursor_readback(
                &down,
                owner,
                NativeCursorReadback {
                    before,
                    target: (before.0 - 1, before.1),
                    actual: (before.0 - 1, before.1),
                },
                true,
            );
            assert!(runtime.new_cursor_moved);
            let loss = event(&runtime, raw, false);
            runtime.input(own(), loss).unwrap();
            assert_eq!(runtime.new_cursor_owner, None);
            assert_eq!(runtime.new_cursor_origin, Some((40, 50)));
            assert!(runtime.new_cursor_moved);
            assert!(runtime.new_cursor_claimed());
            assert!(!runtime.new_cursor_ready());
            // 恢复焦点也不能由 fresh 键补旧来源；claim 只消费，不授予 Move。
            let fresh = event(&runtime, (WM_KEYDOWN, 0x44, 0), false);
            assert!(runtime.native_arrow_input(HWND::default(), &fresh, None));
            let held = event(&runtime, (WM_KEYDOWN, 0x41, 1 << 30), false);
            assert!(runtime.native_arrow_input(HWND::default(), &held, None));
            assert_eq!(runtime.state.pointer, (40, 50));
            assert_eq!(runtime.editor.crop, None);
            assert!(!runtime.new_cursor_ready());
            assert!(runtime.pending.is_empty());
            assert!(runtime.deferred.is_empty());
            let returned = event(&runtime, (WM_MOUSEMOVE, 1, packed((40, 50))), false);
            runtime.mouse_move(returned).unwrap();
            let pointer_up = event(&runtime, (WM_LBUTTONUP, 0, packed((40, 50))), false);
            runtime.input(own(), pointer_up).unwrap();
            assert_eq!(runtime.editor.crop, None); // 不复活旧自动目标。
            assert!(runtime.state.click_seed.is_none());
            assert_eq!(runtime.new_cursor_origin, None);
            for key in [0x41, 0x44] {
                let key_up = event(&runtime, (WM_KEYUP, key, 1 << 31), false);
                assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
                assert!(!runtime.native_arrow_input(HWND::default(), &key_up, None));
            }
        }
    }
    #[test]
    fn native_new_cancel_capture_loss_target_and_drain_consume_old_cycles_without_late_motion() {
        for boundary in 0..4 {
            let mut runtime = new_runtime(false);
            key_cycle(&mut runtime, 0x41, false);
            match boundary {
                0 => {
                    assert!(!runtime.cancel(own(), true));
                }
                1 => {
                    let loss = event(&runtime, (WM_CAPTURECHANGED, 0, 2), false);
                    runtime.input(own(), loss).unwrap();
                    assert_eq!(runtime.new_cursor_origin, None);
                }
                2 => {
                    runtime.target = Some((own(), 1));
                    runtime.native_arrow.input_lost();
                }
                _ => {
                    let loss = event(&runtime, (WM_KILLFOCUS, 0, 0), false);
                    runtime.input(own(), loss).unwrap();
                }
            }
            let original = runtime.state.pointer;
            let crop = runtime.editor.crop;
            let held = event(&runtime, (WM_KEYDOWN, 0x41, 1 << 30), false);
            assert!(runtime.native_arrow_input(HWND::default(), &held, None));
            assert_eq!(runtime.state.pointer, original);
            assert_eq!(runtime.editor.crop, crop);
            assert!(runtime.pending.is_empty());
            assert!(runtime.deferred.is_empty());
            let up = event(&runtime, (WM_KEYUP, 0x41, 1 << 31), false);
            assert!(runtime.native_arrow_input(HWND::default(), &up, None));
            assert!(!runtime.native_arrow_input(HWND::default(), &up, None));
            // fresh 新周期不会由 accepted Up 位自动继承移动许可。
            runtime.clear_new_cursor();
            runtime.button_down = false;
            let fresh = event(&runtime, (WM_KEYDOWN, 0x41, 0), false);
            assert!(!runtime.native_arrow_input(HWND::default(), &fresh, None));
        }
    }
}

#[cfg(test)]
mod native_idle_preview_transfer_tests {
    use super::*;
    use crate::capture_cursor_nudge::{
        idle_preview_cursor_commit, IdlePreviewCursorCommit, NativeArrowRoute,
    };

    fn own() -> HWND {
        HWND(std::ptr::dangling_mut())
    }
    fn packed(point: (i16, i16)) -> isize {
        (u32::from(point.0 as u16) | (u32::from(point.1 as u16) << 16)) as isize
    }
    fn fixed() -> Runtime {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        runtime.state.manual_preview = Some(Rect::full(320, 480));
        runtime.state.window_preview = runtime.state.manual_preview;
        runtime
    }
    fn source(runtime: &Runtime) -> NativeIdleCursorSource {
        NativeIdleCursorSource {
            hwnd: 1,
            thread: 1,
            token: 17,
            bounds: runtime.bounds,
        }
    }
    fn event(runtime: &Runtime, raw: RawInput) -> CapturedInput {
        CapturedInput::for_test(raw, runtime.bounds, (3, 3), gpui::Modifiers::default())
    }
    fn confirmed_idle(runtime: &mut Runtime) -> NativeIdleCursorSource {
        let source = source(runtime);
        let down = event(runtime, (WM_KEYDOWN, 0x27, 0));
        assert_eq!(
            runtime.native_arrow.route(down.raw, down.modifiers, true),
            NativeArrowRoute::Move((1, 0))
        );
        let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
        let target = (before.0 + 1, before.1);
        // 注入生产提交分类器的成功回执，不调用 Set/Get 或窗口输入 API。
        let receipt =
            idle_preview_cursor_commit(runtime.bounds, before, target, Some(target), true, true);
        runtime.record_native_idle_commit(&down, source, receipt);
        source
    }
    fn accepted_manual(runtime: &mut Runtime) {
        runtime.native_arrow.input_lost();
        runtime.button_down = true;
        runtime.state.anchor = Some((40, 50));
        runtime.manual_cursor_origin = Some((40, 50));
        runtime.manual_click_preview = Some((Rect::full(320, 480), (40, 50), false));
        runtime.click_gesture.start((40, 50));
    }
    fn held_right() -> [bool; 8] {
        [false, false, true, false, false, false, false, false]
    }
    fn candidate(runtime: &mut Runtime) -> (NativeIdleCursorSource, NativeIdleCursorCandidate) {
        let source = confirmed_idle(runtime);
        let down = event(runtime, (WM_LBUTTONDOWN, 1, packed((40, 50))));
        let candidate = runtime
            .take_native_idle_candidate(&down, Some(source))
            .unwrap();
        // 生产前置及 route 两层非键 reset，局部快照必须在它们之前取得。
        runtime.native_arrow.reset_repeat();
        assert!(!runtime.native_arrow_input(HWND::default(), &down, None));
        accepted_manual(runtime);
        (source, candidate)
    }

    #[test]
    fn native_idle_receipts_require_fresh_submission_or_qualified_edge_noop_and_never_held_backfill(
    ) {
        for qualified in [false, true] {
            let mut runtime = fixed();
            let source = source(&runtime);
            let down = event(&runtime, (WM_KEYDOWN, 0x25, 0));
            assert_eq!(
                runtime.native_arrow.route(down.raw, down.modifiers, true),
                NativeArrowRoute::Move((-1, 0))
            );
            let before = (runtime.bounds.left, runtime.bounds.top + 50);
            let receipt = idle_preview_cursor_commit(
                runtime.bounds,
                before,
                before,
                Some(before),
                false,
                qualified,
            );
            assert_eq!(
                receipt,
                if qualified {
                    IdlePreviewCursorCommit::EdgeNoop
                } else {
                    IdlePreviewCursorCommit::Rejected
                }
            );
            runtime.record_native_idle_commit(&down, source, receipt);
            let click = event(&runtime, (WM_LBUTTONDOWN, 1, packed((40, 50))));
            assert_eq!(
                runtime
                    .take_native_idle_candidate(&click, Some(source))
                    .is_some(),
                qualified
            );
        }
        for actual_readback in [None, Some((-1921, -190))] {
            let mut runtime = fixed();
            let source = source(&runtime);
            let down = event(&runtime, (WM_KEYDOWN, 0x27, 0));
            assert_eq!(
                runtime.native_arrow.route(down.raw, down.modifiers, true),
                NativeArrowRoute::Move((1, 0))
            );
            let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
            let target = (before.0 + 1, before.1);
            let failed = idle_preview_cursor_commit(
                runtime.bounds,
                before,
                target,
                actual_readback,
                true,
                true,
            );
            assert_eq!(failed, IdlePreviewCursorCommit::Rejected);
            runtime.record_native_idle_commit(&down, source, failed);
            let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
            runtime.native_arrow.route(held.raw, held.modifiers, true);
            runtime.record_native_idle_commit(&held, source, IdlePreviewCursorCommit::Requested);
            let click = event(&runtime, (WM_LBUTTONDOWN, 1, packed((40, 50))));
            assert!(runtime
                .take_native_idle_candidate(&click, Some(source))
                .is_none());
            assert_eq!(runtime.native_idle_source, None);
        }
        let mut runtime = fixed();
        let source = source(&runtime);
        let unknown = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        assert_eq!(
            runtime
                .native_arrow
                .route(unknown.raw, unknown.modifiers, true),
            NativeArrowRoute::Pass
        );
        runtime.record_native_idle_commit(&unknown, source, IdlePreviewCursorCommit::Requested);
        assert_eq!(runtime.native_idle_source, None);
    }

    #[test]
    fn native_idle_left_down_keeps_verified_acceleration_across_nonkey_resets_and_small_click_up() {
        let mut runtime = fixed();
        let source = confirmed_idle(&mut runtime);
        for expected in [1, 2, 2] {
            assert_eq!(
                runtime.native_arrow.route(
                    (WM_KEYDOWN, 0x27, 1 << 30),
                    gpui::Modifiers::default(),
                    true
                ),
                NativeArrowRoute::Move((expected, 0))
            );
        }
        let down = event(&runtime, (WM_LBUTTONDOWN, 1, packed((40, 50))));
        let snapshot = runtime
            .take_native_idle_candidate(&down, Some(source))
            .unwrap();
        runtime.native_arrow.reset_repeat();
        assert!(!runtime.native_arrow_input(HWND::default(), &down, None));
        accepted_manual(&mut runtime);
        let revision = runtime.editor.document.revision();
        assert!(runtime.apply_native_idle_candidate(
            Some(snapshot),
            Some(source),
            held_right(),
            true
        ));
        assert_eq!(runtime.state.pointer, (40, 50));
        assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.editor.document.revision(), revision);
        assert_eq!(
            runtime.native_arrow.route(
                (WM_KEYDOWN, 0x27, 1 << 30),
                gpui::Modifiers::default(),
                true
            ),
            NativeArrowRoute::Move((3, 0))
        );
        let up = event(&runtime, (WM_LBUTTONUP, 0, packed((43, 52))));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.editor.crop, Some(Rect::full(320, 480)));
        let crop = runtime.editor.crop;
        let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        assert!(runtime.native_arrow_input(HWND::default(), &held, None));
        assert_eq!(runtime.editor.crop, crop);
        let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
        assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
        assert!(!runtime.native_arrow_input(HWND::default(), &key_up, None));
    }

    #[test]
    fn native_idle_adoption_rejects_changed_source_released_key_busy_and_late_routes_but_owns_up() {
        for rejection in 0..8 {
            let mut runtime = fixed();
            let (source, snapshot) = candidate(&mut runtime);
            let actual_source = match rejection {
                0 => NativeIdleCursorSource { hwnd: 2, ..source },
                1 => NativeIdleCursorSource {
                    token: 18,
                    ..source
                },
                2 => NativeIdleCursorSource {
                    thread: 2,
                    ..source
                },
                3 => NativeIdleCursorSource {
                    bounds: ScreenRect {
                        width: 641,
                        ..source.bounds
                    },
                    ..source
                },
                _ => source,
            };
            if rejection == 6 {
                runtime.target = Some((own(), 1));
            }
            if rejection == 7 {
                let busy = event(&runtime, (WM_KEYDOWN, 0x54, 0));
                runtime.pending.push(busy);
            }
            assert!(!runtime.apply_native_idle_candidate(
                Some(snapshot),
                Some(actual_source),
                if rejection == 5 {
                    [false; 8]
                } else {
                    held_right()
                },
                rejection != 4
            ));
            assert_eq!(runtime.editor.crop, None);
            assert_eq!(runtime.state.pointer, (40, 50));
            assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
            let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
            assert!(runtime.native_arrow_input(HWND::default(), &held, None));
            let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
            assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
        }
        for raw in [
            (WM_LBUTTONDBLCLK, 1, packed((40, 50))),
            (WM_LBUTTONDOWN, 1, packed((500, 50))),
        ] {
            let mut runtime = fixed();
            let source = confirmed_idle(&mut runtime);
            let rejected = event(&runtime, raw);
            assert!(runtime
                .take_native_idle_candidate(&rejected, Some(source))
                .is_none());
            assert!(!runtime.native_arrow_input(HWND::default(), &rejected, None));
            let click = event(&runtime, (WM_LBUTTONDOWN, 1, packed((40, 50))));
            assert!(runtime
                .take_native_idle_candidate(&click, Some(source))
                .is_none());
        }
    }

    #[test]
    fn native_idle_adopted_far_return_and_cancel_keep_original_down_and_matching_key_up() {
        for cancel in [false, true] {
            let mut runtime = fixed();
            let (source, snapshot) = candidate(&mut runtime);
            assert!(runtime.apply_native_idle_candidate(
                Some(snapshot),
                Some(source),
                held_right(),
                true
            ));
            let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
            let far = (before.0 + 8, before.1 + 1);
            runtime.observe_manual_cursor_readback(before, far);
            runtime.observe_manual_cursor_readback(far, before);
            let returned = event(&runtime, (WM_MOUSEMOVE, 1, packed((40, 50))));
            runtime.mouse_move(returned).unwrap();
            assert!(runtime.manual_cursor_dragged);
            assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
            assert!(runtime.editor.is_changing_crop());
            if cancel {
                assert!(!runtime.cancel(own(), true));
            } else {
                let up = event(&runtime, (WM_LBUTTONUP, 0, packed((40, 50))));
                runtime.input(own(), up).unwrap();
            }
            assert_eq!(runtime.editor.crop, None);
            assert_eq!(runtime.manual_cursor_origin, None);
            assert_eq!(runtime.native_idle_source, None);
            let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
            assert!(runtime.native_arrow_input(HWND::default(), &held, None));
            let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
            assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
        }
    }

    #[test]
    fn native_manual_actual_far_readback_survives_postcommit_owner_loss_and_return_up() {
        for returned_mouse_move in [false, true] {
            let mut runtime = fixed();
            let (source, snapshot) = candidate(&mut runtime);
            assert!(runtime.apply_native_idle_candidate(
                Some(snapshot),
                Some(source),
                held_right(),
                true
            ));
            let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
            let actual = (before.0 + 8, before.1 + 1);
            let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
            // 真实成功 Set/Get 后 Left/焦点门撤失：历史有效位移仍需阻止原范围确认。
            runtime.record_native_cursor_readback(
                &held,
                source,
                NativeCursorReadback {
                    before,
                    target: actual,
                    actual,
                },
                false,
            );
            assert!(runtime.manual_cursor_dragged);
            assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
            assert_eq!(runtime.state.pointer, (40, 50));
            assert_eq!(runtime.editor.crop, None);
            assert_eq!(runtime.native_idle_source, None);
            assert!(runtime.native_arrow_input(HWND::default(), &held, None));
            if returned_mouse_move {
                let returned = event(&runtime, (WM_MOUSEMOVE, 1, packed((40, 50))));
                runtime.mouse_move(returned).unwrap();
                assert!(runtime.editor.is_changing_crop());
            }
            let up = event(&runtime, (WM_LBUTTONUP, 0, packed((40, 50))));
            runtime.input(own(), up).unwrap();
            assert_eq!(runtime.editor.crop, None);
            assert_eq!(runtime.manual_cursor_origin, None);
            assert!(!runtime.button_down);
            let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
            assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
            assert!(!runtime.native_arrow_input(HWND::default(), &key_up, None));
        }
        let mut runtime = fixed();
        let source = confirmed_idle(&mut runtime);
        let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        let before = (runtime.bounds.left + 40, runtime.bounds.top + 50);
        let actual = (before.0 + 8, before.1 + 1);
        runtime.record_native_cursor_readback(
            &held,
            source,
            NativeCursorReadback {
                before,
                target: actual,
                actual,
            },
            false,
        );
        assert_eq!(runtime.native_idle_source, None);
        assert!(!runtime.manual_cursor_dragged);
        let down = event(&runtime, (WM_LBUTTONDOWN, 1, packed((40, 50))));
        assert!(runtime
            .take_native_idle_candidate(&down, Some(source))
            .is_none());
    }
}

#[cfg(test)]
mod native_manual_preview_cursor_tests {
    use super::*;

    fn own() -> HWND {
        HWND(std::ptr::dangling_mut())
    }
    fn packed(point: (i16, i16)) -> isize {
        (u32::from(point.0 as u16) | (u32::from(point.1 as u16) << 16)) as isize
    }
    fn pending_runtime() -> Runtime {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        let rect = Rect::full(320, 480);
        let start = (40, 50);
        runtime.state.manual_preview = Some(rect);
        runtime.state.window_preview = Some(rect);
        runtime.state.anchor = Some(start);
        runtime.button_down = true;
        runtime.manual_click_preview = Some((rect, start, false));
        runtime.manual_cursor_origin = Some(start);
        runtime.click_gesture.start(start);
        runtime
    }
    fn event(runtime: &Runtime, raw: RawInput) -> CapturedInput {
        CapturedInput::for_test(raw, runtime.bounds, (3, 3), gpui::Modifiers::default())
    }
    fn shift_event(runtime: &Runtime, raw: RawInput) -> CapturedInput {
        CapturedInput::for_test(
            raw,
            runtime.bounds,
            (3, 3),
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
        )
    }
    fn actual(runtime: &Runtime, point: (i32, i32)) -> (i32, i32) {
        (runtime.bounds.left + point.0, runtime.bounds.top + point.1)
    }
    fn own_right_cycle(runtime: &mut Runtime) {
        // 只建立纯意图；模型不执行 SetCursorPos 或任何实时 Win32 资格查询。
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x27, 0), gpui::Modifiers::default(), true),
            crate::capture_cursor_nudge::NativeArrowRoute::Move((1, 0))
        );
    }

    #[test]
    fn native_pending_manual_cursor_requires_matching_range_original_down_and_no_pending_tool() {
        let mut runtime = pending_runtime();
        assert!(runtime.manual_cursor_ready());
        runtime.manual_click_preview = Some((Rect::full(640, 480), (40, 50), false));
        assert!(!runtime.manual_cursor_ready());
        runtime.manual_click_preview = Some((Rect::full(320, 480), (40, 50), false));
        runtime.manual_cursor_origin = None;
        assert!(!runtime.manual_cursor_ready());
        runtime.manual_cursor_origin = Some((40, 50));
        let tool = event(&runtime, (WM_KEYDOWN, 0x54, 0));
        runtime.pending.push(tool);
        assert!(!runtime.manual_cursor_ready());
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.manual_preview, Some(Rect::full(320, 480)));
    }

    #[test]
    fn native_manual_readback_uses_physical_dpi_and_only_records_monotonic_proof() {
        let mut runtime = pending_runtime();
        runtime.preview_scale = 2.;
        let revision = runtime.editor.document.revision();
        let before_pointer = runtime.state.pointer;
        let unchanged = actual(&runtime, (40, 50));
        runtime.observe_manual_cursor_readback(unchanged, unchanged);
        let clipped_far = actual(&runtime, (51, 50));
        runtime.observe_manual_cursor_readback(clipped_far, clipped_far);
        assert!(!runtime.manual_cursor_dragged);
        let outside = (runtime.bounds.left - 1, unchanged.1);
        runtime.observe_manual_cursor_readback(unchanged, outside);
        assert!(!runtime.manual_cursor_dragged);
        let boundary = actual(&runtime, (50, 50));
        runtime.observe_manual_cursor_readback(unchanged, boundary);
        assert!(!runtime.manual_cursor_dragged);
        let beyond = actual(&runtime, (51, 50));
        runtime.observe_manual_cursor_readback(boundary, beyond);
        runtime.observe_manual_cursor_readback(beyond, unchanged);
        assert!(runtime.manual_cursor_dragged);
        assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
        assert_eq!(runtime.state.pointer, before_pointer);
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.manual_preview, Some(Rect::full(320, 480)));
        assert_eq!(runtime.editor.document.revision(), revision);
    }

    #[test]
    fn native_manual_readback_far_return_without_move_never_restores_auto_on_up() {
        let mut runtime = pending_runtime();
        let auto = Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        };
        runtime.targets.push(WindowTarget {
            rect: auto,
            global_bounds: RECT {
                left: -1900,
                top: -210,
                right: -1800,
                bottom: -130,
            },
            title: String::new(),
            hwnd: 1,
            process_id: 1,
            thread_id: 1,
        });
        let far = actual(&runtime, (48, 50));
        let returned = actual(&runtime, (40, 50));
        runtime.observe_manual_cursor_readback(returned, far);
        runtime.observe_manual_cursor_readback(far, returned);
        assert_eq!(runtime.editor.crop, None);
        let up = event(&runtime, (WM_LBUTTONUP, 0, packed((40, 50))));
        assert!(!runtime.input(own(), up).unwrap());
        assert_ne!(runtime.editor.crop, Some(Rect::full(320, 480)));
        assert_ne!(runtime.editor.crop, Some(auto));
        assert!(runtime.state.click_seed.is_none());
        assert_eq!(runtime.manual_cursor_origin, None);
        assert!(!runtime.manual_cursor_dragged);
    }

    #[test]
    fn native_manual_drag_keeps_original_down_and_wasd_claim_after_crop_start() {
        let mut runtime = pending_runtime();
        let far = actual(&runtime, (48, 50));
        let before = actual(&runtime, (40, 50));
        runtime.observe_manual_cursor_readback(before, far);
        let movement = event(&runtime, (WM_MOUSEMOVE, 1, packed((40, 50))));
        runtime.mouse_move(movement).unwrap();
        assert_eq!(runtime.state.manual_preview, None);
        assert_eq!(runtime.manual_click_preview, None);
        assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
        assert!(runtime.manual_cursor_dragged);
        assert!(runtime.editor.is_changing_crop());
        assert!(runtime.manual_cursor_ready());
        let before = runtime.editor.crop;
        let revision = runtime.editor.document.revision();
        for key in [0x25, 0x26, 0x27, 0x28, 0x41, 0x57, 0x44, 0x53] {
            let down = event(&runtime, (WM_KEYDOWN, key, 0));
            assert!(runtime.native_arrow_input(HWND::default(), &down, None));
            let up = event(&runtime, (WM_KEYUP, key, 1 << 31));
            assert!(runtime.native_arrow_input(HWND::default(), &up, None));
        }
        assert_eq!(runtime.editor.crop, before);
        assert_eq!(runtime.editor.document.revision(), revision);
        assert!(runtime.pending.is_empty());
    }

    #[test]
    fn native_manual_no_live_target_and_drain_consume_keys_without_late_motion() {
        for target in [None, Some((own(), 1))] {
            let mut runtime = pending_runtime();
            runtime.target = target;
            let before = runtime.state.pointer;
            for key in [0x25, 0x26, 0x27, 0x28, 0x41, 0x57, 0x44, 0x53] {
                let down = event(&runtime, (WM_KEYDOWN, key, 0));
                assert!(runtime.native_arrow_input(HWND::default(), &down, None));
                let held = event(&runtime, (WM_KEYDOWN, key, 1 << 30));
                assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                let up = event(&runtime, (WM_KEYUP, key, 1 << 31));
                assert!(runtime.native_arrow_input(HWND::default(), &up, None));
            }
            assert_eq!(runtime.state.pointer, before);
            assert_eq!(runtime.editor.crop, None);
            assert!(!runtime.manual_cursor_dragged);
            assert!(runtime.pending.is_empty());
            assert!(runtime.deferred.is_empty());
        }
    }

    #[test]
    fn native_manual_mouse_up_keeps_key_up_owned_and_fresh_down_expires_old_cycle() {
        let mut runtime = pending_runtime();
        own_right_cycle(&mut runtime);
        let up = event(&runtime, (WM_LBUTTONUP, 0, packed((40, 50))));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.editor.crop, Some(Rect::full(320, 480)));
        let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        assert!(runtime.native_arrow_input(HWND::default(), &held, None));
        let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
        assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
        assert!(!runtime.native_arrow_input(HWND::default(), &key_up, None));
        own_right_cycle(&mut runtime);
        let fresh = event(&runtime, (WM_KEYDOWN, 0x27, 0));
        assert!(!runtime.native_arrow_input(HWND::default(), &fresh, None));
        assert!(!runtime.native_arrow_input(HWND::default(), &key_up, None));
    }

    #[test]
    fn native_manual_cancel_and_capture_loss_clear_origin_and_keep_old_up_owned() {
        let mut runtime = pending_runtime();
        own_right_cycle(&mut runtime);
        assert!(!runtime.cancel(own(), true));
        assert_eq!(runtime.manual_cursor_origin, None);
        assert!(!runtime.manual_cursor_dragged);
        let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        assert!(runtime.native_arrow_input(HWND::default(), &held, None));
        let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
        assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
        let mut runtime = pending_runtime();
        own_right_cycle(&mut runtime);
        let far = actual(&runtime, (48, 50));
        let before = actual(&runtime, (40, 50));
        runtime.observe_manual_cursor_readback(before, far);
        let loss = event(&runtime, (WM_CAPTURECHANGED, 0, 2));
        runtime.input(own(), loss).unwrap();
        assert_eq!(runtime.manual_cursor_origin, None);
        assert!(!runtime.manual_cursor_dragged);
        assert!(!runtime.button_down);
        let held = event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        assert!(runtime.native_arrow_input(HWND::default(), &held, None));
        let key_up = event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
        assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
    }

    #[test]
    fn native_manual_shift_crop_move_and_final_up_keep_physical_pointer_and_ordinary_square() {
        for manual_source in [true, false] {
            let mut runtime = if manual_source {
                let mut runtime = pending_runtime();
                runtime.manual_click_preview = Some((Rect::full(320, 480), (40, 50), true));
                runtime
            } else {
                let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
                runtime.button_down = true;
                runtime.state.anchor = Some((40, 50));
                runtime.editor.pointer_down_with_shift((40, 50), 6., true);
                runtime
            };
            let revision = runtime.editor.document.revision();
            let edit_revision = runtime.editor.document.edit_revision();
            let movement = shift_event(&runtime, (WM_MOUSEMOVE, 5, packed((62, 55))));
            runtime.mouse_move(movement).unwrap();
            assert_eq!(runtime.editor.document.revision(), revision);
            assert_eq!(runtime.state.pointer, (62, 55));
            assert_eq!(
                runtime.editor.crop,
                Some(Rect {
                    x: 40,
                    y: 50,
                    width: 22,
                    height: if manual_source { 5 } else { 22 },
                })
            );
            assert_eq!(
                runtime.manual_cursor_origin,
                manual_source.then_some((40, 50))
            );
            let up = shift_event(&runtime, (WM_LBUTTONUP, 4, packed((63, 57))));
            runtime.input(own(), up).unwrap();
            assert_eq!(
                runtime.editor.crop,
                Some(Rect {
                    x: 40,
                    y: 50,
                    width: 23,
                    height: if manual_source { 7 } else { 23 },
                })
            );
            assert_eq!(runtime.state.pointer, (63, 57));
            assert_eq!(runtime.manual_cursor_origin, None);
            assert!(!runtime.button_down);
            // 裁剪完成只使旧mask/场景版本失效，不向对象撤销栈添加矩形命令。
            assert_eq!(runtime.editor.document.revision(), revision + 1);
            assert_eq!(runtime.editor.document.edit_revision(), edit_revision);
            assert!(!runtime.editor.is_changing_crop());
            assert!(runtime.editor.document.items().is_empty());
            assert!(!runtime.editor.document.can_undo());
            assert!(!runtime.editor.document.can_redo());
            let duplicate_up = shift_event(&runtime, (WM_LBUTTONUP, 4, packed((63, 57))));
            runtime.input(own(), duplicate_up).unwrap();
            assert_eq!(runtime.editor.document.revision(), revision + 1);
            assert_eq!(runtime.editor.document.edit_revision(), edit_revision);
        }
        let mut runtime = pending_runtime();
        let up = shift_event(&runtime, (WM_LBUTTONUP, 4, packed((44, 52))));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.editor.crop, Some(Rect::full(320, 480)));
    }

    #[test]
    fn native_manual_shift_far_return_and_cancel_keep_original_claim_until_matching_key_up() {
        let mut runtime = pending_runtime();
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x27, 0), shift, true),
            crate::capture_cursor_nudge::NativeArrowRoute::Move((1, 0))
        );
        let before = actual(&runtime, (40, 50));
        let far = actual(&runtime, (48, 51));
        runtime.observe_manual_cursor_readback(before, far);
        runtime.observe_manual_cursor_readback(far, before);
        let returned = shift_event(&runtime, (WM_MOUSEMOVE, 5, packed((40, 50))));
        runtime.mouse_move(returned).unwrap();
        assert!(runtime.manual_cursor_dragged);
        assert!(runtime.editor.is_changing_crop());
        assert_eq!(runtime.manual_cursor_origin, Some((40, 50)));
        let up = shift_event(&runtime, (WM_LBUTTONUP, 4, packed((40, 50))));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.manual_preview, None);
        assert!(runtime.state.click_seed.is_none());
        let held = shift_event(&runtime, (WM_KEYDOWN, 0x27, 1 << 30));
        assert!(runtime.native_arrow_input(HWND::default(), &held, None));
        let key_up = shift_event(&runtime, (WM_KEYUP, 0x27, 1 << 31));
        assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));

        let mut runtime = pending_runtime();
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x41, 0), shift, true),
            crate::capture_cursor_nudge::NativeArrowRoute::Move((-1, 0))
        );
        let movement = shift_event(&runtime, (WM_MOUSEMOVE, 5, packed((62, 55))));
        runtime.mouse_move(movement).unwrap();
        assert!(!runtime.cancel(own(), true));
        assert_eq!(runtime.manual_cursor_origin, None);
        assert!(!runtime.manual_cursor_dragged);
        assert!(!runtime.button_down);
        assert_eq!(runtime.editor.crop, None);
        let held = shift_event(&runtime, (WM_KEYDOWN, 0x41, 1 << 30));
        assert!(runtime.native_arrow_input(HWND::default(), &held, None));
        let key_up = shift_event(&runtime, (WM_KEYUP, 0x41, 1 << 31));
        assert!(runtime.native_arrow_input(HWND::default(), &key_up, None));
        assert!(!runtime.native_arrow_input(HWND::default(), &key_up, None));
    }

    #[test]
    fn native_shift_edges_preserve_acceleration_fifo_and_drain_never_moves_or_accepts_other_chords()
    {
        use crate::capture_cursor_nudge::NativeArrowRoute;
        let mut runtime = pending_runtime();
        own_right_cycle(&mut runtime);
        let bare = gpui::Modifiers::default();
        let shift = gpui::Modifiers {
            shift: true,
            ..bare
        };
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x27, 1 << 30), bare, true),
            NativeArrowRoute::Move((1, 0))
        );
        let shift_down = shift_event(&runtime, (WM_KEYDOWN, 0x10, 0));
        assert!(!runtime.input(own(), shift_down).unwrap());
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x27, 1 << 30), shift, true),
            NativeArrowRoute::Move((2, 0))
        );
        let shift_up = event(&runtime, (WM_KEYUP, 0x10, 1 << 31));
        assert!(!runtime.input(own(), shift_up).unwrap());
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x27, 1 << 30), bare, true),
            NativeArrowRoute::Move((2, 0))
        );
        assert_eq!(runtime.pending.len(), 2);
        assert_eq!(runtime.pending[0].raw, (WM_KEYDOWN, 0x10, 0));
        assert_eq!(runtime.pending[1].raw, (WM_KEYUP, 0x10, 1 << 31));
        assert!(runtime.pending[0].modifiers.shift);
        assert!(!runtime.pending[1].modifiers.shift);
        for target in [None, Some((own(), 1))] {
            runtime.target = target;
            for key in [0x25, 0x26, 0x27, 0x28, 0x41, 0x57, 0x44, 0x53] {
                let down = shift_event(&runtime, (WM_KEYDOWN, key, 0));
                assert!(runtime.native_arrow_input(HWND::default(), &down, None));
                let held = shift_event(&runtime, (WM_KEYDOWN, key, 1 << 30));
                assert!(runtime.native_arrow_input(HWND::default(), &held, None));
                let up = shift_event(&runtime, (WM_KEYUP, key, 1 << 31));
                assert!(runtime.native_arrow_input(HWND::default(), &up, None));
            }
        }
        for chord in [
            gpui::Modifiers {
                control: true,
                ..shift
            },
            gpui::Modifiers { alt: true, ..shift },
            gpui::Modifiers {
                platform: true,
                ..shift
            },
            gpui::Modifiers {
                function: true,
                ..shift
            },
        ] {
            let down =
                CapturedInput::for_test((WM_KEYDOWN, 0x41, 0), runtime.bounds, (3, 3), chord);
            assert!(!runtime.native_arrow_input(HWND::default(), &down, None));
        }
        assert_eq!(runtime.state.pointer, (40, 50));
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.manual_preview, Some(Rect::full(320, 480)));
        assert!(!runtime.manual_cursor_dragged);
        assert_eq!(runtime.pending.len(), 2);
        assert!(runtime.deferred.is_empty());
    }
}

#[cfg(test)]
mod startup_surface_monitor_snap_tests {
    use super::*;
    use crate::{
        startup_preview::PresentedWindowPreview, startup_surface_input::NativeMonitorSnap,
    };

    fn own() -> HWND {
        HWND(std::ptr::dangling_mut())
    }
    fn packed(x: i16, y: i16) -> isize {
        (u32::from(x as u16) | (u32::from(y as u16) << 16)) as isize
    }
    fn monitor_runtime() -> Runtime {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        runtime.planes = vec![
            NativePlane {
                rect: Rect::full(320, 480),
                image: None,
            },
            NativePlane {
                rect: Rect {
                    x: 320,
                    y: 0,
                    width: 320,
                    height: 480,
                },
                image: None,
            },
        ];
        runtime
    }
    fn event(runtime: &Runtime, raw: RawInput) -> CapturedInput {
        CapturedInput::for_test(raw, runtime.bounds, (3, 3), gpui::Modifiers::default())
    }
    fn snap(
        runtime: &Runtime,
        point: Option<(i32, i32)>,
        shown: Option<Rect>,
        shift: bool,
        repeat: bool,
    ) -> CapturedInput {
        let mut event = CapturedInput::for_test(
            (WM_KEYDOWN, 0x41, if repeat { 1 << 30 } else { 0 }),
            runtime.bounds,
            (3, 3),
            gpui::Modifiers {
                control: true,
                shift,
                ..Default::default()
            },
        )
        .with_test_monitor_snap_point(point);
        event.set_presented_monitor_preview(
            shown.map(|rect| PresentedWindowPreview { rect, serial: 7 }),
        );
        event
    }
    fn fixed(runtime: &mut Runtime, rect: Rect) {
        runtime.state.manual_preview = Some(rect);
        runtime.state.window_preview = Some(rect);
    }
    fn handoff(runtime: &mut Runtime) {
        runtime
            .early_output
            .begin_handoff_preview(&runtime.editor, Rect::full(640, 480), false);
        runtime.target = Some((own(), 1));
    }

    #[test]
    fn native_snap_uses_event_point_and_actual_shown_receipt_for_shift_and_repeat() {
        let mut runtime = monitor_runtime();
        let monitor = runtime.planes[1].rect;
        let first = snap(&runtime, Some((-1450, -100)), None, false, false);
        assert!(!runtime.input(own(), first).unwrap());
        assert_eq!(runtime.state.manual_preview, Some(monitor));
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(
            runtime.pending.last().unwrap().native_monitor_snap(),
            Some(NativeMonitorSnap::Applied)
        );
        assert!(!runtime.has_pending_action_keys());
        // 模型中的 serial 只是输入回执 fixture，未调用实际 paint/DWM。
        let repeated = snap(&runtime, Some((-1450, -100)), Some(monitor), true, true);
        assert!(!runtime.input(own(), repeated).unwrap());
        assert_eq!(runtime.state.manual_preview, Some(Rect::full(640, 480)));
        assert_eq!(runtime.pending.len(), 2);
        let before_paint = snap(&runtime, Some((-1450, -100)), Some(monitor), false, false);
        runtime.input(own(), before_paint).unwrap();
        assert_eq!(runtime.state.manual_preview, Some(monitor));
        let release = event(&runtime, (WM_KEYUP, 0x41, 1 << 31));
        assert!(!runtime.input(own(), release).unwrap());
        assert!(!runtime.has_pending_action_keys());
    }

    #[test]
    fn native_snap_cancels_crop_drag_and_pending_manual_click_without_committing_them() {
        let mut runtime = monitor_runtime();
        let before = runtime.editor.document.revision();
        let down = event(&runtime, (WM_LBUTTONDOWN, 1, packed(20, 30)));
        runtime.input(own(), down).unwrap();
        assert!(runtime.button_down);
        let first_snap = snap(&runtime, Some((-1800, -100)), None, false, false);
        runtime.input(own(), first_snap).unwrap();
        assert!(!runtime.button_down);
        assert!(!runtime.editor.is_dragging());
        assert_eq!(runtime.state.anchor, None);
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.manual_preview, Some(runtime.planes[0].rect));
        assert_eq!(runtime.editor.document.revision(), before);
        let down = event(&runtime, (WM_LBUTTONDOWN, 1, packed(40, 50)));
        runtime.input(own(), down).unwrap();
        assert!(runtime.manual_click_preview.is_some());
        let snap = snap(&runtime, Some((-1450, -100)), None, false, false);
        runtime.input(own(), snap).unwrap();
        assert_eq!(runtime.manual_click_preview, None);
        assert!(!runtime.button_down);
        let up = event(&runtime, (WM_LBUTTONUP, 0, packed(40, 50)));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.manual_preview, Some(runtime.planes[1].rect));
    }

    #[test]
    fn native_confirmed_crop_snap_uses_visible_receipt_instead_of_unpainted_crop() {
        for (shown, expected_full) in [(None, false), (Some(Rect::full(320, 480)), true)] {
            let mut runtime = monitor_runtime();
            runtime.editor.set_crop(Some(Rect::full(320, 480)));
            runtime.state.crop = runtime.editor.crop;
            let snap = snap(&runtime, Some((-1800, -100)), shown, false, false);
            runtime.input(own(), snap).unwrap();
            assert_eq!(runtime.editor.crop, None);
            assert_eq!(
                runtime.state.manual_preview,
                Some(if expected_full {
                    Rect::full(640, 480)
                } else {
                    Rect::full(320, 480)
                })
            );
        }
    }

    #[test]
    fn native_preview_down_and_small_motion_keep_range_until_click_up_confirms() {
        let mut runtime = monitor_runtime();
        let rect = runtime.planes[0].rect;
        fixed(&mut runtime, rect);
        let down = event(&runtime, (WM_LBUTTONDOWN, 1, packed(-5, 50)));
        runtime.input(own(), down).unwrap();
        assert!(runtime.button_down);
        assert_eq!(runtime.state.manual_preview, Some(rect));
        assert_eq!(runtime.editor.crop, None);
        let movement = event(&runtime, (WM_MOUSEMOVE, 1, packed(-2, 52)));
        runtime.input(own(), movement).unwrap();
        assert_eq!(runtime.editor.crop, None);
        assert_eq!(runtime.state.window_preview, Some(rect));
        let up = event(&runtime, (WM_LBUTTONUP, 0, packed(-2, 52)));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.editor.crop, Some(rect));
        assert_eq!(runtime.state.manual_preview, None);
        assert!(!runtime.button_down);
    }

    #[test]
    fn native_coalesced_preview_drag_cannot_restore_click_when_pointer_returns() {
        let mut runtime = monitor_runtime();
        fixed(&mut runtime, Rect::full(320, 480));
        let down = event(&runtime, (WM_LBUTTONDOWN, 1, packed(40, 50)));
        runtime.input(own(), down).unwrap();
        let far = event(&runtime, (WM_MOUSEMOVE, 1, packed(80, 50)))
            .with_test_preview_motion_origin((40, 50));
        let mut returned = event(&runtime, (WM_MOUSEMOVE, 1, packed(40, 50)))
            .with_test_preview_motion_origin((40, 50));
        returned.inherit_mouse_motion(&far);
        runtime.input(own(), returned).unwrap();
        assert_eq!(runtime.state.manual_preview, None);
        assert_eq!(runtime.manual_click_preview, None);
        let up = event(&runtime, (WM_LBUTTONUP, 0, packed(40, 50)));
        runtime.input(own(), up).unwrap();
        assert_ne!(runtime.editor.crop, Some(Rect::full(320, 480)));
    }

    #[test]
    fn native_right_leaves_fixed_preview_restores_auto_and_discards_uia_wheels() {
        let mut runtime = monitor_runtime();
        let auto = Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        };
        runtime.targets.push(WindowTarget {
            rect: auto,
            global_bounds: RECT {
                left: -1900,
                top: -210,
                right: -1800,
                bottom: -130,
            },
            title: String::new(),
            hwnd: 1,
            process_id: 1,
            thread_id: 1,
        });
        let wheel = event(
            &runtime,
            (WM_MOUSEWHEEL, (120_u16 as usize) << 16, packed(-1880, -190)),
        );
        runtime.input(own(), wheel).unwrap();
        assert!(runtime
            .pending
            .iter()
            .any(CapturedInput::is_native_preview_wheel));
        let snap = snap(&runtime, Some((-1800, -100)), None, false, false);
        runtime.input(own(), snap).unwrap();
        assert!(!runtime
            .pending
            .iter()
            .any(CapturedInput::is_native_preview_wheel));
        let wheel = event(
            &runtime,
            (WM_MOUSEWHEEL, (120_u16 as usize) << 16, packed(-1880, -190)),
        );
        runtime.input(own(), wheel).unwrap();
        assert!(!runtime
            .pending
            .iter()
            .any(CapturedInput::is_native_preview_wheel));
        let right = event(&runtime, (WM_RBUTTONDOWN, 2, packed(40, 50)));
        assert!(!runtime.input(own(), right).unwrap());
        assert_eq!(runtime.state.manual_preview, None);
        assert_eq!(runtime.state.window_preview, Some(auto));
        assert!(!runtime.state.cancelled);
        let right = event(&runtime, (WM_RBUTTONDOWN, 2, packed(40, 50)));
        assert!(runtime.input(own(), right).unwrap());
    }

    #[test]
    fn native_manual_middle_preserves_outside_point_but_rejects_old_auto_receipt() {
        let mut runtime = monitor_runtime();
        let manual = runtime.planes[0].rect;
        fixed(&mut runtime, manual);
        let mut middle = event(&runtime, (WM_MBUTTONDOWN, 0x10, packed(500, 250)));
        middle.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: runtime.planes[1].rect,
            serial: 7,
        }));
        runtime.input(own(), middle).unwrap();
        assert!(runtime.early_output.finish(true).is_none());
        let mut middle = event(&runtime, (WM_MBUTTONDOWN, 0x10, packed(500, 250)));
        middle.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: manual,
            serial: 8,
        }));
        runtime.input(own(), middle).unwrap();
        assert_eq!(
            runtime.early_output.finish(true).unwrap().manual_preview(),
            Some((manual, (500, 250)))
        );
    }

    #[test]
    fn native_unpainted_manual_enter_is_consumed_without_late_output_or_busy_release() {
        let mut runtime = monitor_runtime();
        fixed(&mut runtime, Rect::full(320, 480));
        let enter = event(&runtime, (WM_KEYDOWN, 0x0d, 0));
        runtime.input(own(), enter).unwrap();
        assert!(runtime
            .pending
            .last()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(runtime.early_output.finish(true).is_none());
        // finish 完结会撤周期；生产未finish时应由真实Up清理。
        let mut runtime = monitor_runtime();
        fixed(&mut runtime, Rect::full(320, 480));
        let enter = event(&runtime, (WM_KEYDOWN, 0x0d, 0));
        runtime.input(own(), enter).unwrap();
        let up = event(&runtime, (WM_KEYUP, 0x0d, 1 << 31));
        runtime.input(own(), up).unwrap();
        assert!(!runtime.has_pending_action_keys());
        assert_eq!(runtime.pending.len(), 1);
    }

    #[test]
    fn deferred_snap_right_and_output_keep_fifo_and_never_backfill_new_manual_receipt() {
        let mut runtime = monitor_runtime();
        handoff(&mut runtime);
        let snap = snap(
            &runtime,
            Some((-1450, -100)),
            Some(runtime.planes[0].rect),
            true,
            true,
        );
        runtime.input(own(), snap).unwrap();
        assert_eq!(
            runtime.deferred[0].native_monitor_snap(),
            Some(NativeMonitorSnap::Pending {
                global_point: Some((-1450, -100)),
                shown_preview: Some(runtime.planes[0].rect),
            })
        );
        let mut middle = event(&runtime, (WM_MBUTTONDOWN, 0x10, packed(500, 250)));
        middle.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: runtime.planes[1].rect,
            serial: 8,
        }));
        runtime.input(own(), middle).unwrap();
        assert!(runtime.deferred[1].clone().into_replay(1.).is_some());
        assert!(runtime.early_output.finish(true).is_none());
        let right = event(&runtime, (WM_RBUTTONDOWN, 2, packed(40, 50)));
        assert!(!runtime.input(own(), right).unwrap());
        assert!(!runtime.state.cancelled);
        assert_eq!(
            runtime
                .deferred
                .iter()
                .map(|event| event.raw.0)
                .collect::<Vec<_>>(),
            vec![WM_KEYDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN]
        );

        // 原固定预览尚未显示其范围时，拒绝 Enter 的 Down 与真实 Up 必须一起消费。
        let mut runtime = monitor_runtime();
        let manual = runtime.planes[0].rect;
        fixed(&mut runtime, manual);
        handoff(&mut runtime);
        let mut enter = event(&runtime, (WM_KEYDOWN, 0x0d, 0));
        enter.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: runtime.planes[1].rect,
            serial: 9,
        }));
        runtime.input(own(), enter).unwrap();
        assert_eq!(runtime.deferred.len(), 1);
        let up = event(&runtime, (WM_KEYUP, 0x0d, 1 << 31));
        runtime.input(own(), up).unwrap();
        assert_eq!(runtime.deferred.len(), 1);
        assert!(runtime.early_output.finish(true).is_none());
    }

    #[test]
    fn deferred_original_manual_middle_uses_receipt_instead_of_stale_editor_or_pointer() {
        let mut runtime = monitor_runtime();
        let manual = runtime.planes[0].rect;
        fixed(&mut runtime, manual);
        handoff(&mut runtime);
        runtime.editor.crop = Some(Rect::full(640, 480));
        runtime.state.pointer = (10, 10);
        let mut middle = event(&runtime, (WM_MBUTTONDOWN, 0x10, packed(500, 250)));
        middle.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: manual,
            serial: 7,
        }));
        runtime.input(own(), middle).unwrap();
        assert_eq!(
            runtime.early_output.finish(true).unwrap().manual_preview(),
            Some((manual, (500, 250)))
        );
    }

    #[test]
    fn native_missing_event_pointer_cannot_snap_using_current_runtime_pointer() {
        let mut runtime = monitor_runtime();
        let snap = snap(&runtime, None, Some(runtime.planes[0].rect), false, false);
        runtime.input(own(), snap).unwrap();
        assert_eq!(runtime.state.manual_preview, None);
        assert_eq!(
            runtime.pending.last().unwrap().native_monitor_snap(),
            Some(NativeMonitorSnap::Pending {
                global_point: None,
                shown_preview: Some(runtime.planes[0].rect)
            })
        );
    }
}

#[cfg(test)]
mod startup_surface_deferred_preview_tests {
    use super::*;
    use crate::startup_preview::PresentedWindowPreview;

    fn own() -> HWND {
        HWND(std::ptr::dangling_mut())
    }

    fn preview() -> Rect {
        Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        }
    }

    fn point(x: i16, y: i16) -> isize {
        (u32::from(x as u16) | (u32::from(y as u16) << 16)) as isize
    }

    pub(super) fn before_handoff() -> Runtime {
        let bounds = ScreenRect {
            left: -1920,
            top: -240,
            width: 640,
            height: 480,
        };
        Runtime {
            editor: Editor::new(Rect::full(bounds.width, bounds.height), None),
            state: SurfaceState {
                anchor: None,
                pointer: (40, 50),
                crop: None,
                window_preview: Some(preview()),
                manual_preview: None,
                dragging: false,
                cancelled: false,
                toolbar_monitor: 0,
                pending_keys: Vec::new(),
                pending_keystrokes: Vec::new(),
                wheel_input: WheelAccumulator::default(),
                click_seed: None,
            },
            bounds,
            planes: Vec::new(),
            startup_bitmap: None,
            targets: Vec::new(),
            preview_pointer: Some((40, 50)),
            tolerance: 6.,
            button_down: false,
            click_gesture: ClickGesture::default(),
            click_started: None,
            double_click_interval: Duration::from_millis(500),
            double_click_tolerance: (4, 4),
            target: None,
            pending: Vec::new(),
            deferred: VecDeque::new(),
            early_output: StartupOutputQueue::default(),
            ime_disabled: false,
            native_arrow: Default::default(),
            native_idle_source: None,
            manual_click_preview: None,
            manual_cursor_origin: None,
            manual_cursor_dragged: false,
            new_cursor_origin: None,
            new_cursor_owner: None,
            new_cursor_moved: false,
            adjust_cursor_origin: None,
            adjust_cursor_owner: None,
            adjust_cursor_moved: false,
            monitor_snap_cycle: false,
            preview_scale: 1.,
        }
    }

    fn input(runtime: &Runtime, raw: RawInput) -> CapturedInput {
        CapturedInput::for_test(raw, runtime.bounds, (3, 3), gpui::Modifiers::default())
    }

    #[test]
    fn released_trigger_key_keeps_native_window_preview() {
        let mut runtime = before_handoff();
        let rect = Rect::full(640, 480);
        runtime.targets.push(crate::selection_assist::WindowTarget {
            rect,
            global_bounds: RECT {
                left: -1920,
                top: -240,
                right: -1280,
                bottom: 240,
            },
            title: String::new(),
            hwnd: 11,
            process_id: 22,
            thread_id: 33,
        });
        runtime.refresh_window_preview();
        assert_eq!(runtime.state.window_preview, Some(rect));
        // F1 在首屏出现前按下：只收到松开，窗口预览保持且仍交给 GPUI 重放。
        runtime
            .input(own(), input(&runtime, (WM_KEYUP, 0x70, 0xc03b_0001)))
            .unwrap();
        assert!(!runtime.has_pending_action_keys());
        assert_eq!(runtime.state.window_preview, Some(rect));
        assert_eq!(runtime.pending.len(), 1);

        // 首屏内真正按下的键仍然暂停预览，直到交接处理完。
        runtime
            .input(own(), input(&runtime, (WM_KEYDOWN, 0x41, 0x001e_0001)))
            .unwrap();
        assert!(runtime.has_pending_action_keys());
        assert_eq!(runtime.state.window_preview, None);
        runtime
            .input(own(), input(&runtime, (WM_KEYUP, 0x41, 0xc01e_0001)))
            .unwrap();
        assert!(runtime.has_pending_action_keys());
        assert_eq!(runtime.state.window_preview, None);
    }

    #[test]
    fn automatic_uia_native_surface_shows_cached_window_and_drag_still_works() {
        for has_window in [false, true] {
            let mut runtime = before_handoff();
            let rect = Rect::full(640, 480);
            if has_window {
                runtime.targets.push(crate::selection_assist::WindowTarget {
                    rect,
                    global_bounds: RECT {
                        left: -1920,
                        top: -240,
                        right: -1280,
                        bottom: 240,
                    },
                    title: String::new(),
                    hwnd: 11,
                    process_id: 22,
                    thread_id: 33,
                });
            }
            runtime.refresh_window_preview();
            assert_eq!(runtime.state.window_preview, has_window.then_some(rect));
            runtime
                .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
                .unwrap();
            runtime
                .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(40, 50))))
                .unwrap();
            assert_eq!(runtime.editor.crop, has_window.then_some(rect));

            runtime.editor.set_crop(None);
            runtime
                .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
                .unwrap();
            runtime
                .input(own(), input(&runtime, (WM_MOUSEMOVE, 1, point(100, 120))))
                .unwrap();
            runtime
                .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(100, 120))))
                .unwrap();
            assert_eq!(
                runtime.editor.crop,
                Some(Rect {
                    x: 40,
                    y: 50,
                    width: 60,
                    height: 70
                })
            );
        }
        let mut runtime = before_handoff();
        runtime.state.manual_preview = Some(preview());
        runtime.refresh_window_preview();
        assert_eq!(runtime.state.window_preview, Some(preview()));
        runtime
            .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
            .unwrap();
        runtime
            .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(40, 50))))
            .unwrap();
        assert_eq!(runtime.editor.crop, Some(preview()));
    }

    fn middle(runtime: &Runtime, raw_flags: usize, at: (i16, i16)) -> CapturedInput {
        let mut event = input(runtime, (WM_MBUTTONDOWN, raw_flags, point(at.0, at.1)));
        event.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: preview(),
            serial: 7,
        }));
        event
    }

    #[test]
    fn native_without_window_has_no_invisible_output_and_keeps_explicit_wheel() {
        for message in [WM_KEYDOWN, WM_MBUTTONDOWN] {
            let mut runtime = before_handoff();
            runtime.refresh_window_preview();
            let event = input(
                &runtime,
                (
                    message,
                    if message == WM_KEYDOWN { 0x0d } else { 0 },
                    point(40, 50),
                ),
            );
            if message == WM_KEYDOWN {
                runtime.queue_preview_confirm(event).unwrap();
            } else {
                runtime.input(own(), event).unwrap();
            }
            assert!(runtime.early_output.finish(true).is_none());
            assert!(runtime.editor.crop.is_none());
        }
        let mut runtime = before_handoff();
        runtime.refresh_window_preview();
        let event = input(&runtime, (WM_MOUSEWHEEL, 120 << 16, point(-1880, -190)));
        runtime.wheel(event).unwrap();
        assert!(runtime.pending.last().unwrap().is_native_preview_wheel());
        assert_eq!(runtime.state.window_preview, None);
    }

    fn begin_model_handoff(runtime: &mut Runtime) {
        // 只模拟所有 Win32 身份/范围门已经成功后的纯状态边界，不调用这些 API。
        let busy = runtime.button_down || runtime.has_pending_action_keys();
        runtime.early_output.begin_handoff_preview(
            &runtime.editor,
            Rect::full(runtime.bounds.width, runtime.bounds.height),
            busy,
        );
        runtime.target = Some((own(), 1));
    }

    fn consumed(event: &CapturedInput) -> bool {
        let Some(startup_surface_input::ReplayInput::Event(replay)) = event.clone().into_replay(1.)
        else {
            panic!("Middle 必须保留其真实 typed 输入");
        };
        replay.native_preview_middle_consumed()
    }

    fn enter(runtime: &Runtime) -> CapturedInput {
        let mut event = input(runtime, (WM_KEYDOWN, 0x0d, 0x001c0001));
        event.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: preview(),
            serial: 7,
        }));
        event
    }

    #[test]
    fn native_enter_freezes_displayed_receipt_and_preserves_handoff_without_repeat_output() {
        let mut runtime = before_handoff();
        let event = enter(&runtime);
        runtime.state.window_preview = Some(Rect::full(640, 480));
        runtime.queue_preview_confirm(event).unwrap();
        assert!(runtime
            .pending
            .last()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(!runtime.has_pending_action_keys());
        assert_eq!(runtime.editor.crop, None);
        assert!(!runtime.editor.document.can_undo());
        begin_model_handoff(&mut runtime);
        let held = input(&runtime, (WM_KEYDOWN, 0x0d, 1 << 30));
        assert!(!runtime.input(own(), held).unwrap());
        assert!(runtime.deferred.is_empty());
        let up = input(&runtime, (WM_KEYUP, 0x0d, 0));
        assert!(!runtime.input(own(), up).unwrap());
        assert!(runtime.deferred.is_empty());
        assert_eq!(
            runtime.early_output.finish(true).unwrap().preview_confirm(),
            Some(preview())
        );
        assert!(runtime.early_output.finish(true).is_none());
    }

    #[test]
    fn deferred_enter_stays_consumed_after_edit_and_pending_actions_cannot_create_preview_output() {
        let mut runtime = before_handoff();
        runtime
            .queue_key(input(&runtime, (WM_KEYDOWN, 0x31, 0)))
            .unwrap();
        runtime.queue_preview_confirm(enter(&runtime)).unwrap();
        assert!(!runtime
            .pending
            .last()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(runtime.early_output.finish(true).is_none());

        let mut rejected = before_handoff();
        begin_model_handoff(&mut rejected);
        let missing = input(&rejected, (WM_KEYDOWN, 0x0d, 0x001c0001));
        assert!(!rejected.input(own(), missing).unwrap());
        assert!(!rejected
            .deferred
            .back()
            .unwrap()
            .native_preview_confirm_consumed());
        // 失败的非修饰动作先重放；任何后来出现的真实 receipt 都不能重开该 fence。
        assert!(!rejected
            .input(own(), middle(&rejected, 0x10, (40, 50)))
            .unwrap());
        assert!(!consumed(rejected.deferred.back().unwrap()));
        assert!(!rejected.input(own(), enter(&rejected)).unwrap());
        assert!(!rejected
            .deferred
            .back()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(rejected.early_output.finish(true).is_none());

        let mut held_mouse = before_handoff();
        begin_model_handoff(&mut held_mouse);
        let held = enter(&held_mouse).with_test_mouse_edit_held();
        assert!(!held_mouse.input(own(), held).unwrap());
        assert!(!held_mouse
            .deferred
            .back()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(!held_mouse
            .input(own(), middle(&held_mouse, 0x10, (40, 50)))
            .unwrap());
        assert!(!consumed(held_mouse.deferred.back().unwrap()));
        assert!(held_mouse.early_output.finish(true).is_none());

        let mut runtime = before_handoff();
        begin_model_handoff(&mut runtime);
        runtime.editor.crop = Some(Rect::full(640, 480)); // 已停用的副本不决定 saved preview。
        assert!(!runtime.input(own(), enter(&runtime)).unwrap());
        assert!(runtime
            .deferred
            .back()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(!runtime
            .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
            .unwrap());
        assert!(!runtime
            .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(40, 50))))
            .unwrap());
        assert!(runtime
            .deferred
            .front()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(!runtime.input(own(), enter(&runtime)).unwrap());
        assert!(!runtime
            .deferred
            .back()
            .unwrap()
            .native_preview_confirm_consumed());
        assert!(runtime.early_output.finish(true).is_none());
    }

    #[test]
    fn deferred_middle_uses_the_enqueue_receipt_without_reading_the_stale_editor() {
        let mut runtime = before_handoff();
        begin_model_handoff(&mut runtime);
        let event = middle(&runtime, 0x10, (40, 50));
        // 接收端另有 GPUI Editor；旧副本和晚到 hover 都不能决定保存的范围。
        runtime.editor.crop = Some(Rect::full(640, 480));
        runtime.state.window_preview = Some(Rect {
            x: 200,
            y: 100,
            width: 120,
            height: 90,
        });
        assert!(!runtime.input(own(), event).unwrap());
        assert!(consumed(runtime.deferred.back().unwrap()));
        assert_eq!(
            runtime.early_output.finish(true).unwrap().preview(),
            Some((preview(), (40, 50)))
        );
        assert!(runtime.early_output.finish(true).is_none());
    }

    #[test]
    fn editing_before_middle_stays_invalid_across_an_unchanged_old_crop_and_repeated_handoff() {
        let mut runtime = before_handoff();
        begin_model_handoff(&mut runtime);
        let saved_middle = middle(&runtime, 0x10, (40, 50));
        assert!(!runtime
            .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
            .unwrap());
        assert!(!runtime
            .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(40, 50))))
            .unwrap());
        assert_eq!(runtime.editor.crop, None);
        // targetSome 的真实早返回不执行 PeekMessage/GetWindowRect，也不重开资格。
        assert!(runtime.handoff(own(), own()).unwrap().is_some());
        assert!(!runtime.input(own(), saved_middle).unwrap());
        assert!(!consumed(runtime.deferred.back().unwrap()));
        assert!(runtime.early_output.finish(true).is_none());
    }

    #[test]
    fn deferred_confirmed_middle_owner_consumes_old_frame_after_edit_without_late_output() {
        let original = preview();
        let receipt = PresentedWindowPreview {
            rect: original,
            serial: 31,
        };
        // 调用真实 targetSome input 接线；不手工设置 consume 位，也不调用 Win32/GPUI。
        for (frame, flags, expected_owned) in [
            (Some(receipt), 0x10, true),
            (None, 0x10, false),
            (
                Some(PresentedWindowPreview {
                    serial: 0,
                    ..receipt
                }),
                0x10,
                false,
            ),
            (
                Some(PresentedWindowPreview {
                    rect: Rect::full(640, 480),
                    ..receipt
                }),
                0x10,
                false,
            ),
            (
                Some(PresentedWindowPreview {
                    rect: Rect {
                        x: 639,
                        y: 0,
                        width: 2,
                        height: 1,
                    },
                    ..receipt
                }),
                0x10,
                false,
            ),
            (Some(receipt), 0x11, true),
        ] {
            for message in [WM_MBUTTONDOWN, WM_MBUTTONDBLCLK] {
                let mut runtime = before_handoff();
                runtime.editor.set_crop(Some(original));
                runtime.state.crop = Some(original);
                runtime.state.window_preview = None;
                begin_model_handoff(&mut runtime);
                assert!(!runtime
                    .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
                    .unwrap());
                assert!(!runtime
                    .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(40, 50))))
                    .unwrap());
                assert_eq!(runtime.editor.crop, Some(original));
                assert_eq!(runtime.state.crop, Some(original));
                assert!(runtime.handoff(own(), own()).unwrap().is_some());
                let mut event = input(&runtime, (message, flags, point(480, 250)));
                event.set_presented_confirmed_selection(frame);
                let raw = event.raw;
                assert!(!runtime.input(own(), event).unwrap());
                let saved = runtime.deferred.back().unwrap();
                assert_eq!(saved.raw, raw);
                assert_eq!(saved.presented_confirmed_selection(), frame);
                assert_eq!(consumed(saved), expected_owned);
                // 晚 host 有 B 或恢复 A 都不得取得输出；finish 只交付 typed 请求。
                assert!(runtime.early_output.finish(true).is_none());
                assert!(runtime.early_output.finish(true).is_none());
            }
        }
        for initial in 0..3 {
            let mut runtime = before_handoff();
            runtime.editor.set_crop(Some(original));
            runtime.state.crop = Some(original);
            runtime.state.window_preview = None;
            match initial {
                0 => runtime.pending.push(input(&runtime, (WM_KEYDOWN, 0x41, 0))),
                1 => runtime.editor.pointer_down((40, 50), 6.),
                2 => runtime.editor.set_crop(None),
                _ => unreachable!(),
            }
            begin_model_handoff(&mut runtime);
            runtime.pending.clear();
            runtime.editor.cancel_gesture();
            runtime.editor.set_crop(Some(original));
            // 初次不合格不能在后来变空闲时补建 owner。
            begin_model_handoff(&mut runtime);
            let mut event = input(&runtime, (WM_MBUTTONDOWN, 0x10, point(480, 250)));
            event.set_presented_confirmed_selection(Some(receipt));
            assert!(!runtime.input(own(), event).unwrap());
            assert!(!consumed(runtime.deferred.back().unwrap()));
            assert!(runtime.early_output.finish(true).is_none());
        }
    }

    #[test]
    fn editing_after_an_admitted_middle_clears_output_but_preserves_consumed_replay() {
        let mut runtime = before_handoff();
        begin_model_handoff(&mut runtime);
        assert!(!runtime
            .input(own(), middle(&runtime, 0x10, (40, 50)))
            .unwrap());
        assert!(consumed(runtime.deferred.front().unwrap()));
        assert!(!runtime
            .input(own(), input(&runtime, (WM_LBUTTONDOWN, 1, point(40, 50))))
            .unwrap());
        assert!(!runtime
            .input(own(), input(&runtime, (WM_LBUTTONUP, 0, point(40, 50))))
            .unwrap());
        assert!(consumed(runtime.deferred.front().unwrap()));
        assert!(runtime.early_output.finish(true).is_none());
    }

    #[test]
    fn absent_receipt_and_other_held_buttons_are_rejected_but_outside_point_is_preserved() {
        for (at, flags, has_receipt, accepted) in [
            ((40, 50), 0x10, false, false),
            ((-1, 50), 0x10, true, true),
            ((120, 50), 0x10, true, true),
            ((40, 50), 0x11, true, false),
            ((40, 50), 0x12, true, false),
        ] {
            let mut runtime = before_handoff();
            begin_model_handoff(&mut runtime);
            let mut event = middle(&runtime, flags, at);
            if !has_receipt {
                event.set_presented_window_preview(None);
            } else if at.0 < 0 {
                // 原范围允许框外 Middle；LPARAM 的真实 -1 仍不能 clamp 或重采。
                event.set_presented_window_preview(Some(PresentedWindowPreview {
                    rect: Rect {
                        x: 0,
                        width: 120,
                        ..preview()
                    },
                    serial: 7,
                }));
            }
            let receipt = event.presented_window_preview();
            assert!(!runtime.input(own(), event).unwrap());
            assert_eq!(consumed(runtime.deferred.back().unwrap()), accepted);
            let output = runtime.early_output.finish(true);
            if accepted {
                assert_eq!(
                    output.unwrap().preview(),
                    Some((receipt.unwrap().rect, (at.0 as i32, at.1 as i32)))
                );
            } else {
                assert!(output.is_none());
            }
        }
    }

    #[test]
    fn automatic_middle_outside_point_keeps_original_presented_receipt_through_handoff() {
        let receipt = PresentedWindowPreview {
            rect: preview(),
            serial: 31,
        };
        let b = Rect {
            x: 400,
            y: 200,
            width: 120,
            height: 90,
        };
        let original_point = (480, 250);
        assert!(!receipt.rect.contains(original_point));
        assert!(b.contains(original_point));
        for before_target in [true, false] {
            let mut runtime = before_handoff();
            let mut event = input(&runtime, (WM_MBUTTONDOWN, 0x10, point(480, 250)));
            event.set_presented_window_preview(Some(receipt));
            // Native 后来显示 B；原 event 已复制 A，不能用新 hover 替换回执。
            runtime.state.window_preview = Some(b);
            if before_target {
                // None 的真实 input 尾部调用系统 Cursor/Invalidate；本模型只调用该分支同一生产 Queue API。
                assert!(runtime.early_output.offer_preview(
                    &runtime.editor,
                    runtime.editor.canvas,
                    event.presented_window_preview(),
                    crate::startup_preview::client_point(event.raw.2),
                    false,
                ));
                begin_model_handoff(&mut runtime);
            } else {
                begin_model_handoff(&mut runtime);
                let raw = event.raw;
                let received_at = event.received_at;
                assert!(!runtime.input(own(), event).unwrap());
                let saved = runtime.deferred.back().unwrap();
                assert!(consumed(saved));
                assert_eq!(saved.raw, raw);
                assert_eq!(saved.received_at, received_at);
                assert_eq!(saved.presented_window_preview(), Some(receipt));
            }
            let output = runtime.early_output.finish(true).unwrap();
            assert_eq!(output.preview(), Some((receipt.rect, original_point)));
            assert_eq!(output.manual_preview(), None);
            assert_eq!(output.native_confirmed(), None);
            // 与 GPUI 接收端共用同一资格模型；不读取晚到的 hover 或当前 pointer。
            assert_eq!(
                crate::preview_middle_pin::candidate(
                    &runtime.editor,
                    runtime.editor.canvas,
                    original_point,
                    Some(receipt.rect),
                    false,
                    false,
                ),
                Some(receipt.rect)
            );
            assert!(runtime.early_output.finish(true).is_none());
            assert_eq!(runtime.editor.crop, None);
            assert!(!runtime.editor.document.can_undo());
        }
        for invalid in [
            None,
            Some(PresentedWindowPreview {
                serial: 0,
                ..receipt
            }),
            Some(PresentedWindowPreview {
                rect: Rect {
                    x: u32::MAX,
                    ..receipt.rect
                },
                ..receipt
            }),
        ] {
            for before_target in [true, false] {
                let mut runtime = before_handoff();
                let mut event = input(&runtime, (WM_MBUTTONDOWN, 0x10, point(480, 250)));
                event.set_presented_window_preview(invalid);
                runtime.state.window_preview = Some(b);
                if before_target {
                    assert!(!runtime.early_output.offer_preview(
                        &runtime.editor,
                        runtime.editor.canvas,
                        event.presented_window_preview(),
                        original_point,
                        false,
                    ));
                    begin_model_handoff(&mut runtime);
                } else {
                    begin_model_handoff(&mut runtime);
                    assert!(!runtime.input(own(), event).unwrap());
                    assert!(!consumed(runtime.deferred.back().unwrap()));
                }
                assert!(runtime.early_output.finish(true).is_none());
            }
        }
    }

    #[test]
    fn pending_action_keys_disable_the_initial_fence_while_only_modifiers_keep_it_idle() {
        for (key, expected) in [(0x41, false), (0x11, true), (0x10, true)] {
            let mut runtime = before_handoff();
            runtime.pending.push(input(&runtime, (WM_KEYDOWN, key, 0)));
            begin_model_handoff(&mut runtime);
            assert!(!runtime
                .input(own(), middle(&runtime, 0x10, (40, 50)))
                .unwrap());
            assert_eq!(consumed(runtime.deferred.back().unwrap()), expected);
            assert_eq!(runtime.early_output.finish(true).is_some(), expected);
        }
    }

    #[test]
    fn coalesced_unknown_left_or_right_drag_is_observed_before_deferred_offer() {
        for held in [MK_LBUTTON.0 as usize, MK_RBUTTON.0 as usize] {
            let mut runtime = before_handoff();
            begin_model_handoff(&mut runtime);
            let held_move = input(&runtime, (WM_MOUSEMOVE, held, point(60, 50)));
            let mut returned = input(&runtime, (WM_MOUSEMOVE, 0, point(40, 50)));
            returned.inherit_mouse_motion(&held_move);
            assert_eq!(returned.raw.1, 0);
            assert!(returned.deferred_edit_button_seen());
            assert!(!runtime.input(own(), returned).unwrap());
            assert!(!runtime
                .input(own(), middle(&runtime, 0x10, (40, 50)))
                .unwrap());
            assert!(!consumed(runtime.deferred.back().unwrap()));
            assert!(runtime.early_output.finish(true).is_none());
        }
    }

    fn wheel_event(runtime: &Runtime, delta: i16) -> CapturedInput {
        let global = (runtime.bounds.left + 40, runtime.bounds.top + 50);
        input(
            runtime,
            (
                WM_MOUSEWHEEL,
                usize::from(delta as u16) << 16,
                point(global.0 as i16, global.1 as i16),
            ),
        )
    }

    #[test]
    fn native_preview_wheel_keeps_event_point_modifiers_and_half_notches() {
        let mut runtime = before_handoff();
        let raw = (
            WM_MOUSEWHEEL,
            60_usize << 16,
            point(
                (runtime.bounds.left + 40) as i16,
                (runtime.bounds.top + 50) as i16,
            ),
        );
        let modifiers = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        for _ in 0..2 {
            runtime
                .wheel(CapturedInput::for_test(
                    raw,
                    runtime.bounds,
                    (3, 3),
                    modifiers,
                ))
                .unwrap();
        }
        assert!(runtime.editor.crop.is_none());
        assert_eq!(runtime.pending.len(), 2);
        assert!(runtime.deferred.is_empty());
        for captured in runtime.pending {
            assert_eq!(captured.raw, raw);
            let Some(startup_surface_input::ReplayInput::Event(replay)) =
                captured.into_replay(1.25)
            else {
                panic!("expected retained wheel");
            };
            assert_eq!(replay.native_preview_wheel_notches(), Some(0.5));
            let gpui::PlatformInput::ScrollWheel(event) = replay.event else {
                panic!("expected original wheel event");
            };
            assert_eq!(
                (f32::from(event.position.x), f32::from(event.position.y)),
                (32., 40.)
            );
            assert_eq!(event.modifiers, modifiers);
            assert_eq!(crate::capture_interaction::notches(&event, (3., 3.)), 0.5);
        }
        for (message, shift, settings, expected) in [
            (WM_MOUSEHWHEEL, false, (3, 3), None),
            (WM_MOUSEHWHEEL, true, (3, 3), Some(-0.5)),
            (WM_MOUSEHWHEEL, true, (3, 0), None),
            (WM_MOUSEWHEEL, false, (0, 3), None),
            (WM_MOUSEWHEEL, true, (3, 0), None),
        ] {
            let mut runtime = before_handoff();
            let event = CapturedInput::for_test(
                (message, raw.1, raw.2),
                runtime.bounds,
                settings,
                gpui::Modifiers {
                    shift,
                    ..Default::default()
                },
            );
            let gpui::PlatformInput::ScrollWheel(decoded) =
                event.clone().platform_input(1.).unwrap()
            else {
                panic!("expected wheel axis");
            };
            assert_eq!(
                crate::capture_interaction::notches(&decoded, (3., 3.)),
                expected.unwrap_or(0.)
            );
            runtime.wheel(event).unwrap();
            assert_eq!(runtime.pending.len(), usize::from(expected.is_some()));
            if let Some(captured) = runtime.pending.pop() {
                let Some(startup_surface_input::ReplayInput::Event(replay)) =
                    captured.into_replay(1.)
                else {
                    panic!("expected retained Shift horizontal wheel");
                };
                assert_eq!(replay.native_preview_wheel_notches(), expected);
            }
        }
    }

    #[test]
    fn native_preview_wheel_replays_later_hover_and_discards_it_with_new_selection() {
        let mut runtime = before_handoff();
        runtime
            .mouse_move(input(&runtime, (WM_MOUSEMOVE, 0, point(60, 70))))
            .unwrap();
        assert!(runtime.pending.is_empty());
        runtime.wheel(wheel_event(&runtime, 60)).unwrap();
        runtime
            .mouse_move(input(&runtime, (WM_MOUSEMOVE, 0, point(80, 90))))
            .unwrap();
        runtime
            .mouse_move(input(&runtime, (WM_MOUSEMOVE, 0, point(100, 110))))
            .unwrap();
        assert_eq!(runtime.state.pointer, (100, 110));
        assert_eq!(runtime.pending.len(), 2);
        let events: Vec<_> = runtime
            .pending
            .iter()
            .cloned()
            .map(|event| {
                let Some(startup_surface_input::ReplayInput::Event(replay)) =
                    event.into_replay(1.25)
                else {
                    panic!("expected actual wheel/hover replay");
                };
                replay
            })
            .collect();
        assert_eq!(events[0].native_preview_wheel_notches(), Some(0.5));
        assert!(!events[0].native_preview_hover());
        let gpui::PlatformInput::ScrollWheel(wheel) = &events[0].event else {
            panic!("expected original wheel first");
        };
        assert_eq!(
            (f32::from(wheel.position.x), f32::from(wheel.position.y)),
            (32., 40.)
        );
        assert!(events[1].native_preview_hover());
        let gpui::PlatformInput::MouseMove(hover) = &events[1].event else {
            panic!("expected last real hover after wheel");
        };
        assert_eq!(
            (f32::from(hover.position.x), f32::from(hover.position.y)),
            (80., 88.)
        );
        assert_eq!(hover.pressed_button, None);
        // Production LeftDown/cancel discard both members before Editor's Native crop mutation.
        runtime.discard_preview_wheels();
        runtime.editor.pointer_down((100, 110), 6.);
        runtime.editor.pointer_move((200, 180));
        runtime.editor.pointer_up((200, 180));
        assert!(runtime.editor.crop.is_some());
        runtime.editor.set_crop(None);
        assert!(runtime.pending.is_empty());
        runtime
            .mouse_move(input(&runtime, (WM_MOUSEMOVE, 0, point(120, 130))))
            .unwrap();
        assert!(runtime.pending.is_empty());
        assert!(runtime.editor.document.items().is_empty());
    }

    #[test]
    fn native_preview_wheel_is_invalidated_before_new_native_selection() {
        let mut runtime = before_handoff();
        runtime.wheel(wheel_event(&runtime, 60)).unwrap();
        assert_eq!(runtime.pending.len(), 1);
        // Production LeftDown calls this before Editor starts its original New gesture.
        runtime.discard_preview_wheels();
        runtime.editor.pointer_down((100, 100), 6.);
        runtime.editor.pointer_move((200, 180));
        runtime.editor.pointer_up((200, 180));
        assert!(runtime.editor.crop.is_some());
        runtime.editor.set_crop(None);
        assert!(runtime.pending.is_empty());
        runtime.wheel(wheel_event(&runtime, -60)).unwrap();
        assert_eq!(runtime.pending.len(), 1);
        let Some(startup_surface_input::ReplayInput::Event(replay)) =
            runtime.pending.pop().unwrap().into_replay(1.)
        else {
            panic!("expected new preview wheel");
        };
        assert_eq!(replay.native_preview_wheel_notches(), Some(-0.5));
        assert!(runtime.editor.document.items().is_empty());
    }

    #[test]
    fn native_preview_wheel_and_pending_tool_wheel_keep_original_order_and_limit() {
        let mut runtime = before_handoff();
        runtime.wheel(wheel_event(&runtime, 60)).unwrap();
        runtime
            .queue_key(input(&runtime, (WM_KEYDOWN, 0x31, 0)))
            .unwrap();
        runtime.wheel(wheel_event(&runtime, -120)).unwrap();
        assert_eq!(
            runtime.pending.iter().map(|e| e.raw.0).collect::<Vec<_>>(),
            vec![WM_MOUSEWHEEL, WM_KEYDOWN, WM_MOUSEWHEEL]
        );
        assert!(runtime.pending[0].is_native_preview_wheel());
        assert!(!runtime.pending[2].is_native_preview_wheel());
        runtime.discard_preview_wheels();
        assert_eq!(
            runtime.pending.iter().map(|e| e.raw.0).collect::<Vec<_>>(),
            vec![WM_KEYDOWN, WM_MOUSEWHEEL]
        );
        let padding = wheel_event(&runtime, 60);
        runtime.pending.resize(EVENT_LIMIT, padding);
        assert!(runtime.wheel(wheel_event(&runtime, 120)).is_err());
        assert_eq!(runtime.pending.len(), EVENT_LIMIT);
        assert!(runtime.editor.crop.is_none());
    }

    #[test]
    fn native_center_half_then_target_some_replay_consumes_same_wheel_once() {
        let mut runtime = before_handoff();
        runtime.editor.crop = Some(preview());
        runtime.state.crop = Some(preview());
        let first = wheel_event(&runtime, 60);
        let received_at = first.received_at;
        runtime.wheel(first).unwrap();
        assert_eq!(runtime.editor.crop, Some(preview()));
        assert!(runtime.pending.is_empty());
        let mut transferred = runtime.state.wheel_input.clone();
        begin_model_handoff(&mut runtime);
        let mut second = wheel_event(&runtime, 60);
        second.received_at = received_at + Duration::from_millis(20);
        assert!(!runtime.input(own(), second).unwrap());
        assert_eq!(runtime.editor.crop, Some(preview()));
        assert_eq!(runtime.deferred.len(), 1);
        let raw = runtime.deferred.pop_front().unwrap();
        let Some(startup_surface_input::ReplayInput::Event(replay)) = raw.into_replay(1.25) else {
            panic!("expected raw wheel");
        };
        let gpui::PlatformInput::ScrollWheel(event) = replay.event else {
            panic!("expected wheel event");
        };
        let distance = crate::capture_interaction::notches(&event, (3., 3.));
        assert_eq!(distance, 0.5);
        let (steps, _) =
            transferred.push_selection_center(distance, received_at + Duration::from_millis(20));
        assert_eq!(steps, 1);
        assert!(runtime.editor.adjust_crop_from_center(steps));
        assert_eq!(
            runtime.editor.crop,
            Some(Rect {
                x: 19,
                y: 29,
                width: 102,
                height: 82
            })
        );
        assert!(runtime.pending.is_empty());
        assert!(runtime.deferred.is_empty());
    }

    #[test]
    fn native_wheel_preserves_signed_outside_points_and_rejects_canvas_end() {
        for local_x in [-1, 640] {
            let mut runtime = before_handoff();
            runtime.editor.crop = Some(preview());
            let event = input(
                &runtime,
                (
                    WM_MOUSEWHEEL,
                    120_usize << 16,
                    point(
                        (runtime.bounds.left + local_x) as i16,
                        (runtime.bounds.top + 50) as i16,
                    ),
                ),
            );
            runtime.wheel(event).unwrap();
            assert_eq!(runtime.state.pointer, (local_x, 50));
            assert_eq!(runtime.editor.crop, Some(preview()));
            assert!(runtime.pending.is_empty());
            assert!(runtime.deferred.is_empty());
        }
    }

    #[test]
    fn pending_tool_keys_keep_native_wheel_in_order_once_and_remain_bounded() {
        let mut runtime = before_handoff();
        runtime.editor.crop = Some(preview());
        runtime
            .queue_key(input(&runtime, (WM_KEYDOWN, 0x31, 0)))
            .unwrap();
        runtime.wheel(wheel_event(&runtime, 120)).unwrap();
        runtime
            .queue_key(input(&runtime, (WM_KEYDOWN, 0x35, 0)))
            .unwrap();
        assert_eq!(
            runtime
                .pending
                .iter()
                .map(|event| (event.raw.0, event.raw.1))
                .collect::<Vec<_>>(),
            vec![
                (WM_KEYDOWN, 0x31),
                (WM_MOUSEWHEEL, 120_usize << 16),
                (WM_KEYDOWN, 0x35)
            ]
        );
        assert!(runtime.deferred.is_empty());
        assert_eq!(runtime.editor.crop, Some(preview()));
        let padding = wheel_event(&runtime, 60);
        runtime.pending.resize(EVENT_LIMIT, padding);
        assert!(runtime.wheel(wheel_event(&runtime, 120)).is_err());
        assert_eq!(runtime.pending.len(), EVENT_LIMIT);
        assert_eq!(runtime.editor.crop, Some(preview()));
    }
}

#[cfg(test)]
mod native_arrow_queue_tests {
    use super::*;
    use crate::capture_cursor_nudge::NativeArrowRoute;

    #[test]
    fn rejected_down_and_accepted_up_keep_original_pending_order_and_held_filter() {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        let raw = (WM_KEYDOWN, 0x27, 0);
        let event =
            CapturedInput::for_test(raw, runtime.bounds, (3, 3), gpui::Modifiers::default());
        // live=None 不调用任何 OS 资格/鼠标 API；未接受的 Down 仍进入真实旧 queue_key。
        assert!(!runtime.native_arrow_input(HWND::default(), &event, None));
        runtime.queue_key(event).unwrap();
        let held = (WM_KEYDOWN, 0x27, 1 << 30);
        let event =
            CapturedInput::for_test(held, runtime.bounds, (3, 3), gpui::Modifiers::default());
        assert!(!runtime.native_arrow_input(HWND::default(), &event, None));
        runtime.queue_key(event).unwrap();
        assert_eq!(
            runtime
                .pending
                .iter()
                .map(|event| event.raw)
                .collect::<Vec<_>>(),
            vec![raw]
        );
        let up = (WM_KEYUP, 0x27, 0);
        let event = CapturedInput::for_test(up, runtime.bounds, (3, 3), gpui::Modifiers::default());
        assert!(!runtime.native_arrow_input(HWND::default(), &event, None));
        runtime.queue_key(event).unwrap();
        assert_eq!(runtime.state.pending_keys, vec![raw, up]);
        assert_eq!(
            runtime
                .pending
                .iter()
                .map(|event| event.raw)
                .collect::<Vec<_>>(),
            vec![raw, up]
        );

        // 领取只在纯状态建立 intent，模型不执行移动；实际 input 的尾部 Up 消费路线仍核验。
        assert_eq!(
            runtime
                .native_arrow
                .route((WM_KEYDOWN, 0x26, 0), gpui::Modifiers::default(), true),
            NativeArrowRoute::Move((0, -1))
        );
        let accepted_up = CapturedInput::for_test(
            (WM_KEYUP, 0x26, 0),
            runtime.bounds,
            (3, 3),
            gpui::Modifiers::default(),
        );
        assert!(!runtime.input(HWND::default(), accepted_up).unwrap());
        assert_eq!(runtime.state.pending_keys, vec![raw, up]);

        // targetSome 不读取旧 Editor 或移动；已领取 Up 收尾，新 held 仍保留原 deferred 合同。
        runtime
            .native_arrow
            .route((WM_KEYDOWN, 0x25, 0), gpui::Modifiers::default(), true);
        runtime.target = Some((HWND::default(), 1));
        let event = CapturedInput::for_test(
            (WM_KEYUP, 0x25, 0),
            runtime.bounds,
            (3, 3),
            gpui::Modifiers::default(),
        );
        assert!(!runtime.input(HWND::default(), event).unwrap());
        assert!(runtime.deferred.is_empty());
        let event =
            CapturedInput::for_test(held, runtime.bounds, (3, 3), gpui::Modifiers::default());
        assert!(!runtime.input(HWND::default(), event).unwrap());
        assert_eq!(runtime.deferred.len(), 1);
        assert_eq!(runtime.deferred[0].raw, held);
    }
}

#[cfg(test)]
mod surface_cursor_tests {
    use super::{surface_cursor_kind, SurfaceCursorKind};
    use crate::interaction::Hover;

    #[test]
    fn eight_crop_edges_keep_windows_resize_orientation_move_uses_sizeall_and_others_cross() {
        for (edges, expected) in [
            (1, SurfaceCursorKind::Horizontal),
            (2, SurfaceCursorKind::Horizontal),
            (4, SurfaceCursorKind::Vertical),
            (8, SurfaceCursorKind::Vertical),
            (5, SurfaceCursorKind::DiagonalDown),
            (10, SurfaceCursorKind::DiagonalDown),
            (6, SurfaceCursorKind::DiagonalUp),
            (9, SurfaceCursorKind::DiagonalUp),
        ] {
            assert_eq!(surface_cursor_kind(Hover::edges(edges)), expected);
        }
        assert_eq!(surface_cursor_kind(Hover::Move), SurfaceCursorKind::Move);
        for hover in [Hover::Crosshair, Hover::Arrow, Hover::Text] {
            assert_eq!(surface_cursor_kind(hover), SurfaceCursorKind::Cross);
        }
    }
}

#[cfg(test)]
mod first_window_dc_tests {
    use super::*;

    fn eligible_fixture() -> (ScreenRect, FirstWindowDcCoverage) {
        let bounds = ScreenRect {
            left: -1920,
            top: -240,
            width: 4480,
            height: 1680,
        };
        let client = [0, 0, 4480, 1680];
        let coverage = FirstWindowDcCoverage {
            outer: [-1920, -240, 2560, 1440],
            client,
            clip: client,
            clip_complexity: SIMPLEREGION.0,
            map_mode: MM_TEXT.0,
            graphics_mode: GM_COMPATIBLE.0,
            layout: 0,
            window_origin: [0, 0],
            viewport_origin: [0, 0],
            window_extent: [1, 1],
            viewport_extent: [1, 1],
            world_transform: [1., 0., 0., 1., 0., 0.],
            queries_succeeded: true,
        };
        (bounds, coverage)
    }

    #[test]
    fn physical_full_client_with_negative_global_origin_is_eligible() {
        let (bounds, coverage) = eligible_fixture();
        assert_eq!(first_window_dc_rejection(bounds, &coverage), None);
    }

    #[test]
    fn full_bounding_box_does_not_accept_complex_or_empty_clip() {
        let (bounds, mut coverage) = eligible_fixture();
        for complexity in [COMPLEXREGION.0, NULLREGION.0, 0] {
            coverage.clip_complexity = complexity;
            assert_eq!(
                first_window_dc_rejection(bounds, &coverage),
                Some("dc-clip-not-simple-rectangle")
            );
        }
    }

    #[test]
    fn partial_client_clip_and_changed_window_geometry_are_rejected() {
        let (bounds, coverage) = eligible_fixture();
        let mut partial = coverage;
        partial.clip[2] -= 1;
        assert_eq!(
            first_window_dc_rejection(bounds, &partial),
            Some("dc-clip-not-whole-client")
        );
        partial = coverage;
        partial.clip[0] = -1;
        assert_eq!(
            first_window_dc_rejection(bounds, &partial),
            Some("dc-clip-not-whole-client")
        );
        for index in 0..4 {
            let mut changed_outer = coverage;
            changed_outer.outer[index] += 1;
            assert_eq!(
                first_window_dc_rejection(bounds, &changed_outer),
                Some("physical-window-bounds-mismatch")
            );
            let mut changed_client = coverage;
            changed_client.client[index] += 1;
            assert_eq!(
                first_window_dc_rejection(bounds, &changed_client),
                Some("physical-window-bounds-mismatch")
            );
        }
    }

    #[test]
    fn mapping_origins_extents_layout_and_world_transform_must_be_identity() {
        let (bounds, coverage) = eligible_fixture();
        let mut changed = [coverage; 12];
        changed[0].map_mode = MM_ANISOTROPIC.0;
        changed[1].graphics_mode = GM_ADVANCED.0;
        changed[2].layout = LAYOUT_RTL.0;
        changed[3].window_origin[0] = 1;
        changed[4].viewport_origin[1] = -1;
        changed[5].window_extent[0] = 2;
        changed[6].viewport_extent[1] = 2;
        changed[7].window_extent = [0, 0];
        changed[8].window_extent = [-1, -1];
        changed[8].viewport_extent = [-1, -1];
        changed[9].world_transform[0] = 1.25;
        changed[10].world_transform[4] = 1.;
        changed[11].world_transform[0] = f32::NAN;
        for mapping in changed {
            assert_eq!(
                first_window_dc_rejection(bounds, &mapping),
                Some("dc-mapping-not-physical-identity")
            );
        }
    }

    #[test]
    fn failed_coverage_queries_never_enable_direct_paint() {
        let (bounds, mut coverage) = eligible_fixture();
        coverage.queries_succeeded = false;
        assert_eq!(
            first_window_dc_rejection(bounds, &coverage),
            Some("coverage-query-failed")
        );
    }
}

#[cfg(test)]
mod early_selection_wheel_tests {
    use super::*;
    use crate::annotation::{Annotation, Point, Style, Tool};

    #[test]
    fn half_notch_survives_handoff_and_crop_wheel_keeps_annotations() {
        let crop = Rect {
            x: 100,
            y: 80,
            width: 200,
            height: 120,
        };
        let mut editor = Editor::new(Rect::full(400, 300), Some(crop));
        let mut annotation = Annotation::new(
            Tool::Rectangle,
            Point { x: 120., y: 100. },
            Style::default(),
        );
        annotation.update(Point { x: 160., y: 130. });
        editor.document.insert(annotation);
        let objects = editor.document.items().to_vec();
        let mut wheel = WheelAccumulator::default();
        let now = Instant::now();
        assert!(!adjust_startup_crop_wheel(
            &mut editor,
            &mut wheel,
            0.5,
            (200, 140),
            6.,
            false,
            now,
        ));
        assert_eq!(editor.crop, Some(crop));
        // 交接复制的是实际累加状态，不能丢弃第一半刻度。
        let mut transferred = wheel.clone();
        assert!(adjust_startup_crop_wheel(
            &mut editor,
            &mut transferred,
            0.5,
            (200, 140),
            6.,
            false,
            now + Duration::from_millis(50),
        ));
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 99,
                y: 79,
                width: 202,
                height: 122,
            })
        );
        assert!(adjust_startup_crop_wheel(
            &mut editor,
            &mut transferred,
            -1.,
            (99, 140),
            6.,
            false,
            now + Duration::from_millis(100),
        ));
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 100,
                y: 80,
                width: 200,
                height: 120,
            })
        );
        assert_eq!(editor.document.items(), objects);
    }

    #[test]
    fn wheel_during_drag_or_without_crop_does_not_leak_into_next_selection() {
        let crop = Rect {
            x: 40,
            y: 40,
            width: 100,
            height: 80,
        };
        let mut editor = Editor::new(Rect::full(300, 200), None);
        let mut wheel = WheelAccumulator::default();
        let now = Instant::now();
        assert!(!adjust_startup_crop_wheel(
            &mut editor,
            &mut wheel,
            0.5,
            (80, 80),
            6.,
            false,
            now,
        ));
        editor.set_crop(Some(crop));
        editor.pointer_down((80, 80), 6.);
        assert!(editor.is_dragging());
        assert!(!adjust_startup_crop_wheel(
            &mut editor,
            &mut wheel,
            1.,
            (80, 80),
            6.,
            false,
            now + Duration::from_millis(20),
        ));
        assert_eq!(editor.crop, Some(crop));
        assert!(editor.cancel_gesture());
        assert!(!adjust_startup_crop_wheel(
            &mut editor,
            &mut wheel,
            1.,
            (80, 80),
            6.,
            true,
            now + Duration::from_millis(30),
        ));
        assert!(!adjust_startup_crop_wheel(
            &mut editor,
            &mut wheel,
            0.5,
            (80, 80),
            6.,
            false,
            now + Duration::from_millis(40),
        ));
        assert_eq!(editor.crop, Some(crop));
        assert!(adjust_startup_crop_wheel(
            &mut editor,
            &mut wheel,
            0.5,
            (80, 80),
            6.,
            false,
            now + Duration::from_millis(50),
        ));
        assert_eq!(editor.crop.unwrap().width, 102);
    }
}

#[cfg(test)]
mod startup_surface_queue_tests {
    use super::*;

    #[test]
    fn selection_receipt_is_only_copied_for_snap_and_keeps_preview_output_semantics() {
        let queue = queue();
        let crop = Rect::full(320, 480);
        queue.publish_completed_selection(Some(crop), None).unwrap();
        let snap = CapturedInput::for_test(
            (WM_KEYDOWN, 0x41, 0),
            queue.bounds,
            (3, 3),
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        )
        .with_test_monitor_snap_point(Some((-1700, -100)));
        queue.push_captured(snap);
        queue.begin_preview_paint();
        let saved = queue.pop().unwrap();
        assert_eq!(saved.monitor_snap_shown_preview(), Some(crop));
        assert_eq!(saved.presented_window_preview(), None);
        assert_eq!(queue.presented_selection.lock().unwrap().snapshot(), None);
        queue.publish_completed_selection(Some(crop), None).unwrap();
        let middle = CapturedInput::for_test(
            (WM_MBUTTONDOWN, 0x10, 0),
            queue.bounds,
            (3, 3),
            gpui::Modifiers::default(),
        );
        queue.push_captured(middle);
        let middle = queue.pop().unwrap();
        assert_eq!(middle.presented_window_preview(), None);
        assert_eq!(middle.monitor_snap_shown_preview(), None);
    }

    #[test]
    fn queue_circle_motion_keeps_real_points_through_both_coalescing_layers() {
        let queue = queue();
        let captured = |message, x: i16, y: i16| {
            CapturedInput::for_test(
                (
                    message,
                    1,
                    (u32::from(x as u16) | (u32::from(y as u16) << 16)) as isize,
                ),
                queue.bounds,
                (3, 3),
                gpui::Modifiers::default(),
            )
        };
        queue.push_captured(captured(WM_LBUTTONDOWN, 0, 0));
        queue.pop().unwrap();
        queue.push_captured(captured(WM_MOUSEMOVE, 5, 0));
        queue.push_captured(captured(WM_MOUSEMOVE, 0, 5));
        let boundary = queue.pop().unwrap();
        assert!(!boundary.crossed_preview_drag((0, 0), 1.));
        queue.push_captured(captured(WM_MOUSEMOVE, 6, 0));
        let far = queue.pop().unwrap();
        queue.push_captured(captured(WM_MOUSEMOVE, 0, 0));
        let mut returned = queue.pop().unwrap();
        returned.inherit_mouse_motion(&boundary);
        assert!(!returned.crossed_preview_drag((0, 0), 1.));
        returned.inherit_mouse_motion(&far);
        let Some(startup_surface_input::ReplayInput::Event(replay)) = returned.into_replay(1.)
        else {
            panic!("Move应保留路径证明");
        };
        assert!(replay.crossed_preview_drag((0, 0), 1.));
    }

    fn queue() -> EventQueue {
        EventQueue {
            messages: Mutex::new(VecDeque::new()),
            presented_preview: Mutex::new(PreviewPresentation::default()),
            presented_selection: Mutex::new(PreviewPresentation::default()),
            preview_drag_origin: Mutex::new(Default::default()),
            overflow: AtomicBool::new(false),
            dormant: AtomicBool::new(false),
            cursors: [0; 6],
            current_cursor: AtomicUsize::new(0),
            isolate_moves: false,
            qa_modifiers: false,
            bounds: ScreenRect {
                left: -1920,
                top: -240,
                width: 640,
                height: 480,
            },
            wheel: (3, 3),
        }
    }

    fn input(queue: &EventQueue, raw: RawInput) -> CapturedInput {
        CapturedInput::for_test(raw, queue.bounds, queue.wheel, gpui::Modifiers::default())
    }

    fn point(x: i16, y: i16) -> isize {
        (u32::from(x as u16) | (u32::from(y as u16) << 16)) as isize
    }

    #[test]
    fn preview_enter_queue_snapshot_is_not_backfilled_or_changed_by_later_paint() {
        let queue = queue();
        let a = Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        };
        let b = Rect {
            x: 200,
            y: 100,
            width: 120,
            height: 90,
        };
        queue.publish_completed_preview(Some(a)).unwrap();
        queue.push_captured(input(&queue, (WM_KEYDOWN, 0x0d, 0x001c0001)));
        queue.begin_preview_paint();
        queue.push_captured(input(&queue, (WM_KEYDOWN, 0x0d, 0x001c0001)));
        queue.publish_completed_preview(Some(b)).unwrap();
        assert_eq!(
            queue
                .pop()
                .unwrap()
                .presented_window_preview()
                .unwrap()
                .rect,
            a
        );
        assert_eq!(queue.pop().unwrap().presented_window_preview(), None);
    }

    #[test]
    fn preview_middle_keeps_the_receipt_copied_at_enqueue_before_a_later_paint() {
        let queue = queue();
        let a = Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        };
        let b = Rect {
            x: 200,
            y: 100,
            width: 120,
            height: 90,
        };
        // 纯队列模型，publish_completed_preview 不代表执行过 GDI/DWM。
        queue.publish_completed_preview(Some(a)).unwrap();
        queue.push_captured(input(&queue, (WM_MBUTTONDOWN, 0x10, point(40, 50))));
        queue.begin_preview_paint();
        queue.publish_completed_preview(Some(b)).unwrap();
        let event = queue.pop().unwrap();
        let snapshot = event.presented_window_preview().unwrap();
        assert_eq!(snapshot.rect, a);
        assert_eq!(snapshot.serial, 1);
        assert_eq!(startup_preview::client_point(event.raw.2), (40, 50));
    }

    #[test]
    fn preview_middle_without_receipt_cannot_be_backfilled_by_a_later_paint() {
        let queue = queue();
        let preview = Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        };
        queue.publish_completed_preview(Some(preview)).unwrap();
        queue.begin_preview_paint();
        queue.push_captured(input(&queue, (WM_MBUTTONDOWN, 0x10, point(40, 50))));
        queue.publish_completed_preview(Some(preview)).unwrap();
        assert!(queue.pop().unwrap().presented_window_preview().is_none());
        queue.reset_preview();
        queue.push_captured(input(&queue, (WM_MBUTTONDOWN, 0x10, point(40, 50))));
        assert!(queue.pop().unwrap().presented_window_preview().is_none());
    }

    #[test]
    fn full_handoff_queue_preserves_motion_and_sets_overflow_without_paint() {
        let queue = queue();
        queue.push_captured(input(&queue, (WM_LBUTTONDOWN, 1, point(20, 20))));
        queue.push_captured(input(&queue, (WM_MOUSEMOVE, 1, point(80, 80))));
        for _ in 2..EVENT_LIMIT {
            queue.push_captured(input(&queue, (WM_KEYUP, 0, 0)));
        }
        let before: Vec<_> = queue
            .messages
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.raw)
            .collect();
        queue.push_captured(input(&queue, (WM_LBUTTONUP, 0, point(20, 20))));
        assert!(queue.overflow.load(Ordering::Acquire));
        let after: Vec<_> = queue
            .messages
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.raw)
            .collect();
        assert_eq!(after, before);
        assert_eq!(after.len(), EVENT_LIMIT);
    }

    #[test]
    fn full_handoff_queue_evicts_only_paint_and_keeps_gesture_order() {
        let queue = queue();
        queue.push_captured(input(&queue, (WM_LBUTTONDOWN, 1, point(20, 20))));
        queue.push_captured(input(&queue, (WM_MOUSEMOVE, 1, point(80, 80))));
        queue.push_captured(input(&queue, (WM_PAINT, 0, 0)));
        for _ in 3..EVENT_LIMIT {
            queue.push_captured(input(&queue, (WM_KEYUP, 0, 0)));
        }
        queue.push_captured(input(&queue, (WM_LBUTTONUP, 0, point(20, 20))));
        assert!(!queue.overflow.load(Ordering::Acquire));
        let events: Vec<_> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(events.len(), EVENT_LIMIT);
        assert_eq!(events[0].raw, (WM_LBUTTONDOWN, 1, point(20, 20)));
        assert_eq!(events[1].raw, (WM_MOUSEMOVE, 1, point(80, 80)));
        assert!(events[2..EVENT_LIMIT - 1]
            .iter()
            .all(|event| event.raw.0 == WM_KEYUP));
        assert_eq!(
            events[EVENT_LIMIT - 1].raw,
            (WM_LBUTTONUP, 0, point(20, 20))
        );
        let mut gesture = ClickGesture::default();
        gesture.start((20, 20));
        gesture.observe_move(&events[1]);
        assert!(!gesture.finish((20, 20)));
    }

    #[test]
    fn full_handoff_queue_coalesces_return_without_losing_far_motion_or_last_modifiers() {
        let queue = queue();
        for _ in 0..EVENT_LIMIT - 2 {
            queue.push_captured(input(&queue, (WM_KEYUP, 0, 0)));
        }
        queue.push_captured(input(&queue, (WM_LBUTTONDOWN, 1, point(10, 10))));
        queue.push_captured(input(&queue, (WM_MOUSEMOVE, 1, point(30, 30))));
        let mut returned = input(&queue, (WM_MOUSEMOVE, 1, point(10, 10)));
        returned.modifiers.shift = true;
        queue.push_captured(returned);
        assert!(!queue.overflow.load(Ordering::Acquire));
        let events: Vec<_> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(events.len(), EVENT_LIMIT);
        let last = events.last().unwrap();
        assert_eq!(last.raw, (WM_MOUSEMOVE, 1, point(10, 10)));
        assert!(last.modifiers.shift);
        let mut gesture = ClickGesture::default();
        gesture.start((10, 10));
        gesture.observe_move(last);
        assert!(!gesture.finish((10, 10)));
        let Some(startup_surface_input::ReplayInput::Event(replay)) =
            last.clone().into_replay(1.25)
        else {
            panic!("合并 move 必须保留为 typed replay");
        };
        assert!(replay.crossed_click_tolerance(Some((10, 10))));
        assert!(!replay.crossed_click_tolerance(None));
    }

    #[test]
    fn handoff_queue_down_and_up_keep_each_gestures_motion_separate() {
        let queue = queue();
        let raw = [
            (WM_MOUSEMOVE, 0, point(30, 30)),
            (WM_LBUTTONDOWN, 1, point(10, 10)),
            (WM_MOUSEMOVE, 1, point(10, 10)),
            (WM_LBUTTONUP, 0, point(10, 10)),
            (WM_MOUSEMOVE, 0, point(30, 30)),
            (WM_LBUTTONDOWN, 1, point(15, 15)),
            (WM_MOUSEMOVE, 1, point(15, 15)),
            (WM_LBUTTONUP, 0, point(15, 15)),
        ];
        for event in raw {
            queue.push_captured(input(&queue, event));
        }
        let events: Vec<_> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(
            events.iter().map(|event| event.raw).collect::<Vec<_>>(),
            raw
        );
        for (index, start) in [
            (0, None),
            (2, Some((10, 10))),
            (4, None),
            (6, Some((15, 15))),
        ] {
            let Some(startup_surface_input::ReplayInput::Event(replay)) =
                events[index].clone().into_replay(1.25)
            else {
                panic!("手势内与悬停的 move 都必须保留");
            };
            assert!(!replay.crossed_click_tolerance(start));
        }
    }

    #[test]
    fn handoff_capture_loss_is_deferred_for_foreign_capture_and_keeps_move_barriers() {
        let source = HWND(0x1234_usize as *mut _);
        assert!(!is_handoff_input(WM_CAPTURECHANGED, 0x1234, source));
        assert!(is_handoff_input(WM_CAPTURECHANGED, 0, source));
        assert!(is_handoff_input(WM_CAPTURECHANGED, 0x5678, source));
        assert!(is_handoff_input(WM_LBUTTONUP, 0, source));
        assert!(!is_handoff_input(WM_PAINT, 0, source));
        let queue = queue();
        queue.push_captured(input(&queue, (WM_MOUSEMOVE, 1, point(30, 30))));
        queue.push_captured(input(&queue, (WM_CAPTURECHANGED, 0, 0x5678)));
        queue.push_captured(input(&queue, (WM_MOUSEMOVE, 1, point(10, 10))));
        let events: Vec<_> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].raw, (WM_CAPTURECHANGED, 0, 0x5678));
        assert!(matches!(
            events[1].clone().into_replay(1.25),
            Some(startup_surface_input::ReplayInput::CancelPointerGesture)
        ));
        let mut gesture = ClickGesture::default();
        gesture.start((10, 10));
        gesture.observe_move(&events[2]);
        assert!(gesture.finish((10, 10)));
    }
}

#[cfg(test)]
mod selection_chrome_clip_tests {
    use super::*;

    fn fields(rect: RECT) -> [i32; 4] {
        [rect.left, rect.top, rect.right, rect.bottom]
    }

    #[test]
    fn desktop_edge_frame_stays_outside_and_circle_is_clipped_without_repositioning() {
        let bounds = ScreenRect {
            left: -1920,
            top: -240,
            width: 257,
            height: 193,
        };
        let full = SelectionChrome::projected(Rect::full(257, 193), 1., (0., 0.));
        for rect in full.frame() {
            assert!(selection_chrome_rect(rect, bounds).unwrap().is_none());
        }
        let corner = full.handles()[0];
        assert_eq!(corner.center, (0., 0.));
        let first_visible = |inset| {
            corner
                .spans(inset)
                .find_map(|row| selection_chrome_rect(row, bounds).unwrap())
        };
        assert_eq!(fields(first_visible(0).unwrap()), [0, 0, 6, 1]);
        assert_eq!(
            fields(first_visible(crate::selection_chrome::HANDLE_BORDER).unwrap()),
            [0, 0, 4, 1]
        );
        assert_eq!(corner.center, (0., 0.));
        let crop = Rect {
            x: 0,
            y: 19,
            width: 31,
            height: 27,
        };
        let chrome = SelectionChrome::projected(crop, 1., (0., 0.));
        let top = selection_chrome_rect(chrome.frame()[0], bounds)
            .unwrap()
            .unwrap();
        assert_eq!(fields(top), [0, 16, 34, 19]);
        assert_eq!(
            fields(
                selection_chrome_rect(chrome.handles()[0].bounds(), bounds)
                    .unwrap()
                    .unwrap()
            ),
            [0, 13, 6, 25]
        );
        for rect in chrome.frame() {
            if let Some(rect) = selection_chrome_rect(rect, bounds).unwrap() {
                assert!(
                    rect.right <= crop.x as i32
                        || rect.left >= (crop.x + crop.width) as i32
                        || rect.bottom <= crop.y as i32
                        || rect.top >= (crop.y + crop.height) as i32
                );
            }
        }
        for circle in chrome.handles() {
            for row in circle
                .spans(0)
                .chain(circle.spans(crate::selection_chrome::HANDLE_BORDER))
            {
                if let Some(rect) = selection_chrome_rect(row, bounds).unwrap() {
                    assert!(rect.left >= 0 && rect.top >= 0);
                    assert!(
                        rect.right <= bounds.width as i32 && rect.bottom <= bounds.height as i32
                    );
                    assert_eq!(rect.bottom - rect.top, 1);
                }
            }
        }
    }

    #[test]
    fn outer_chrome_at_signed_client_limit_never_wraps_or_repositions() {
        let limit = i32::MAX as u32;
        let bounds = ScreenRect {
            left: i32::MIN,
            top: -1,
            width: limit,
            height: limit,
        };
        let chrome = SelectionChrome::projected(
            Rect {
                x: limit - 1,
                y: limit - 1,
                width: 1,
                height: 1,
            },
            1.,
            (0., 0.),
        );
        let frame = chrome.frame();
        assert_eq!(
            frame[2].right,
            i64::from(limit) + crate::selection_chrome::FRAME_WIDTH
        );
        assert!(selection_chrome_rect(frame[2], bounds).unwrap().is_none());
        assert!(selection_chrome_rect(frame[3], bounds).unwrap().is_none());
        assert_eq!(
            fields(selection_chrome_rect(frame[0], bounds).unwrap().unwrap()),
            [i32::MAX - 4, i32::MAX - 4, i32::MAX, i32::MAX - 1]
        );
        let circle = chrome.handles()[7];
        assert_eq!(circle.bounds().right, i64::from(limit) + 6);
        assert_eq!(
            fields(
                selection_chrome_rect(circle.bounds(), bounds)
                    .unwrap()
                    .unwrap()
            ),
            [i32::MAX - 6, i32::MAX - 6, i32::MAX, i32::MAX]
        );
        let rows: Vec<_> = circle
            .spans(0)
            .filter_map(|row| selection_chrome_rect(row, bounds).unwrap())
            .collect();
        assert_eq!(rows.len(), 6);
        assert_eq!(
            fields(rows[0]),
            [i32::MAX - 2, i32::MAX - 6, i32::MAX, i32::MAX - 5]
        );
        assert_eq!(circle.center, (f64::from(limit), f64::from(limit)));
    }
}

#[cfg(test)]
mod startup_text_roi_runtime_tests {
    use super::*;
    use crate::startup_preview::PresentedWindowPreview;

    fn r(runtime: &Runtime) -> CapturedInput {
        let mut event = CapturedInput::for_test(
            (WM_KEYDOWN, 0x52, 0),
            runtime.bounds,
            (3, 3),
            Default::default(),
        );
        event.set_presented_window_preview(Some(PresentedWindowPreview {
            rect: Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 80,
            },
            serial: 7,
        }));
        event
    }

    #[test]
    fn pending_tool_prefix_and_after_handoff_edit_keep_fifo_without_late_text_output() {
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        // 先前尚未消费的 Text 工具键不能用 Native 旧 Editor 解释后来的 R。
        runtime
            .queue_key(CapturedInput::for_test(
                (WM_KEYDOWN, 0x58, 0),
                runtime.bounds,
                (3, 3),
                Default::default(),
            ))
            .unwrap();
        runtime.queue_text(r(&runtime)).unwrap();
        assert_eq!(
            runtime
                .pending
                .iter()
                .map(|event| event.raw.1)
                .collect::<Vec<_>>(),
            vec![0x58, 0x52]
        );
        assert!(!runtime.pending[1].native_text_consumed());
        assert!(runtime.early_output.finish(true).is_none());
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        runtime.queue_text(r(&runtime)).unwrap();
        assert!(runtime.pending[0].native_text_consumed());
        assert!(!runtime.has_pending_action_keys());
        runtime
            .early_output
            .begin_handoff_preview(&runtime.editor, runtime.editor.canvas, false);
        let own = HWND(std::ptr::dangling_mut());
        runtime.target = Some((own, 1));
        // targetSome 的真实纯输入分支只defer，没有真实 Win32 调用。
        runtime
            .input(
                own,
                CapturedInput::for_test(
                    (WM_LBUTTONDOWN, 1, 0),
                    runtime.bounds,
                    (3, 3),
                    Default::default(),
                ),
            )
            .unwrap();
        assert!(runtime.early_output.finish(true).is_none());
        assert!(runtime.pending[0].native_text_consumed());
        let before = runtime.deferred.len();
        runtime
            .input(
                own,
                CapturedInput::for_test(
                    (WM_KEYDOWN, 0x52, 1 << 30),
                    runtime.bounds,
                    (3, 3),
                    Default::default(),
                ),
            )
            .unwrap();
        runtime
            .input(
                own,
                CapturedInput::for_test(
                    (WM_SYSKEYUP, 0x52, 0),
                    runtime.bounds,
                    (3, 3),
                    Default::default(),
                ),
            )
            .unwrap();
        assert_eq!(runtime.deferred.len(), before);

        let destination = HWND(0x2468usize as *mut _);
        let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
        runtime.queue_text(r(&runtime)).unwrap();
        runtime
            .early_output
            .begin_handoff_preview(&runtime.editor, runtime.editor.canvas, false);
        runtime.target = Some((destination, 1));
        let loss = |raw, bounds| CapturedInput::for_test(raw, bounds, (3, 3), Default::default());
        // GPUI show/SetForeground 在 finish_handoff 前发生；尾部 drain 也走本纯分支。
        runtime
            .input(
                own,
                loss(
                    (WM_ACTIVATE, WA_INACTIVE as usize, destination.0 as isize),
                    runtime.bounds,
                ),
            )
            .unwrap();
        runtime
            .input(
                own,
                loss((WM_KILLFOCUS, destination.0 as usize, 0), runtime.bounds),
            )
            .unwrap();
        let output = runtime.early_output.finish(true).unwrap();
        assert_eq!(
            output.text().unwrap().1,
            Rect {
                x: 20,
                y: 30,
                width: 100,
                height: 80
            }
        );
        assert!(runtime.pending[0].native_text_consumed());

        for raw in [
            (WM_KILLFOCUS, 0, 0),
            (WM_KILLFOCUS, 0x5678, 0),
            (WM_ACTIVATE, WA_INACTIVE as usize, 0),
            (WM_ACTIVATE, WA_INACTIVE as usize, 0x5678),
            (
                WM_CANCELMODE,
                destination.0 as usize,
                destination.0 as isize,
            ),
            (WM_ACTIVATEAPP, 0, 1),
        ] {
            let mut runtime = super::startup_surface_deferred_preview_tests::before_handoff();
            runtime.queue_text(r(&runtime)).unwrap();
            runtime.early_output.begin_handoff_preview(
                &runtime.editor,
                runtime.editor.canvas,
                false,
            );
            runtime.target = Some((destination, 1));
            runtime.input(own, loss(raw, runtime.bounds)).unwrap();
            // 外部失活后再收到 own focus pair，不能恢复旧请求/fence。
            runtime
                .input(
                    own,
                    loss((WM_KILLFOCUS, destination.0 as usize, 0), runtime.bounds),
                )
                .unwrap();
            assert!(runtime.early_output.finish(true).is_none());
            assert!(runtime.pending[0].native_text_consumed());
            assert!(runtime.early_output.take_text_cycle(WM_SYSKEYUP, 0x52, 0));
        }
        assert!(!text_loss_is_own_handoff(
            (WM_KILLFOCUS, destination.0 as usize, 0),
            None
        ));
        assert!(!text_loss_is_own_handoff(
            (WM_KILLFOCUS, destination.0 as usize, 0),
            Some((destination, 0))
        ));
    }
}
