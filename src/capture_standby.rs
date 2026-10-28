//! 预装载一次性截图进程。待命阶段不初始化 GPUI / 应用 GPU 设备或冻结桌面。
//! 固定 CPU OCR 原生环境可能枚举系统显示适配器，不据此宣称系统没有合成资源。
use crate::Result;
use std::{
    cell::RefCell,
    marker::PhantomData,
    mem::size_of,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::{Child, Command},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Memory::*, Threading::*},
        UI::{
            HiDpi::*,
            Input::Ime::{ImmAssociateContextEx, ImmDisableIME, HIMC},
            WindowsAndMessaging::*,
        },
    },
};

pub(crate) const READY: u32 = WM_APP + 15;
pub(crate) const SURFACE_READY: u32 = WM_APP + 16;
const REQUEST: u32 = WM_APP + 0x62;
const STOP: u32 = WM_APP + 0x63;
const IPC_MAGIC: u32 = 0x5253_4950;
const IPC_VERSION: u32 = 3;
const TRANSPORT_EVENT: usize = 1;
const TRANSPORT_WINDOW: usize = 2;
const PREPARING: u32 = 1;
const IDLE: u32 = 2;
const WRITING: u32 = 3;
const REQUESTED: u32 = 4;
const CONSUMED: u32 = 5;
const CLOSED: u32 = 6;
const FOREGROUND_PENDING: u32 = 0;
const FOREGROUND_ALLOWED: u32 = 1;
const FOREGROUND_DENIED: u32 = 2;

// 固定大小、只传标量的本机协议。映射不包含指针、Rust 集合、图像或 GPUI 状态。
// 仅本项目 Windows x64、普通缓存 PAGE_READWRITE 共享页：使用硬件原子与一致映射的
// 平台约定，并不把 Rust 线程内存模型宣称为可移植的跨进程保证。identity 发布后不再修改。
#[repr(C, align(8))]
struct RequestMetadata {
    magic: AtomicU32,
    version: AtomicU32,
    bytes: AtomicU32,
    parent_pid: AtomicU32,
    child_pid: AtomicU32,
    quick: AtomicU32,
    parent_token: AtomicU64,
    child_token: AtomicU64,
    parent_window: AtomicU64,
    control_window: AtomicU64,
    state: AtomicU32,
    stop: AtomicU32,
    mode: AtomicU32,
    foreground_state: AtomicU32,
    request_qpc: AtomicI64,
}
const _: () = {
    assert!(size_of::<RequestMetadata>() == 80);
    assert!(std::mem::align_of::<RequestMetadata>() >= 8);
    assert!(std::mem::offset_of!(RequestMetadata, parent_token) % 8 == 0);
    assert!(std::mem::offset_of!(RequestMetadata, request_qpc) % 8 == 0);
};

#[derive(Clone, Copy)]
struct Binding {
    parent_pid: u32,
    child_pid: u32,
    parent_token: usize,
    child_token: i64,
    parent_window: HWND,
}
impl Binding {
    fn object_name(self, suffix: &str) -> Vec<u16> {
        format!(
            "Local\\RustShot.Capture.IPC.v{IPC_VERSION}.{}.{}.{:016x}.{:016x}.{suffix}",
            self.parent_pid, self.child_pid, self.parent_token, self.child_token as u64
        )
        .encode_utf16()
        .chain(Some(0))
        .collect()
    }
}
impl RequestMetadata {
    // 新建 paging-file mapping 的零页已经是这些原子字段的合法零表示。
    // 逐字段原子初始化，避免 accept_ready 前的 stop 打开者和整结构非原子写发生竞争。
    fn initialize(&self, binding: Binding, control: HWND) {
        self.magic.store(IPC_MAGIC, Ordering::Relaxed);
        self.version.store(IPC_VERSION, Ordering::Relaxed);
        self.bytes
            .store(size_of::<Self>() as u32, Ordering::Relaxed);
        self.parent_pid.store(binding.parent_pid, Ordering::Relaxed);
        self.child_pid.store(binding.child_pid, Ordering::Relaxed);
        self.parent_token
            .store(binding.parent_token as u64, Ordering::Relaxed);
        self.child_token
            .store(binding.child_token as u64, Ordering::Relaxed);
        self.parent_window
            .store(binding.parent_window.0 as usize as u64, Ordering::Relaxed);
        self.control_window
            .store(control.0 as usize as u64, Ordering::Relaxed);
        self.state.store(PREPARING, Ordering::Release);
    }
    #[cfg(test)]
    fn new(binding: Binding, control: HWND) -> Self {
        Self {
            magic: AtomicU32::new(IPC_MAGIC),
            version: AtomicU32::new(IPC_VERSION),
            bytes: AtomicU32::new(size_of::<Self>() as u32),
            parent_pid: AtomicU32::new(binding.parent_pid),
            child_pid: AtomicU32::new(binding.child_pid),
            quick: AtomicU32::new(0),
            parent_token: AtomicU64::new(binding.parent_token as u64),
            child_token: AtomicU64::new(binding.child_token as u64),
            parent_window: AtomicU64::new(binding.parent_window.0 as usize as u64),
            control_window: AtomicU64::new(control.0 as usize as u64),
            state: AtomicU32::new(0),
            stop: AtomicU32::new(0),
            mode: AtomicU32::new(0),
            foreground_state: AtomicU32::new(FOREGROUND_PENDING),
            request_qpc: AtomicI64::new(0),
        }
    }
    fn valid(&self, binding: Binding, control: HWND) -> bool {
        let state = self.state.load(Ordering::Acquire);
        (PREPARING..=CLOSED).contains(&state)
            && self.magic.load(Ordering::Relaxed) == IPC_MAGIC
            && self.version.load(Ordering::Relaxed) == IPC_VERSION
            && self.bytes.load(Ordering::Relaxed) as usize == size_of::<Self>()
            && self.parent_pid.load(Ordering::Relaxed) == binding.parent_pid
            && self.child_pid.load(Ordering::Relaxed) == binding.child_pid
            && self.parent_token.load(Ordering::Relaxed) == binding.parent_token as u64
            && self.child_token.load(Ordering::Relaxed) == binding.child_token as u64
            && self.parent_window.load(Ordering::Relaxed) == binding.parent_window.0 as usize as u64
            && self.control_window.load(Ordering::Relaxed) == control.0 as usize as u64
            && crate::quick_action::QuickRecipe::unpack(self.quick.load(Ordering::Relaxed)).is_ok()
            && self.foreground_state.load(Ordering::Acquire) <= FOREGROUND_DENIED
    }
    #[cfg(test)]
    fn publish_request(&self, mode: usize, timestamp: i64) -> Result<()> {
        self.publish_request_with_quick(mode, timestamp, 0)
    }
    fn publish_request_with_quick(&self, mode: usize, timestamp: i64, quick: u32) -> Result<()> {
        if mode > 2 || timestamp <= 0 || self.stop.load(Ordering::Acquire) != 0 {
            return Err("待命 IPC 请求参数或停止状态无效".into());
        }
        crate::quick_action::QuickRecipe::unpack(quick)?;
        self.state
            .compare_exchange(IDLE, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "待命 IPC 已消费或正在处理请求")?;
        self.mode.store(mode as u32, Ordering::Relaxed);
        self.quick.store(quick, Ordering::Relaxed);
        self.request_qpc.store(timestamp, Ordering::Relaxed);
        self.foreground_state
            .store(FOREGROUND_PENDING, Ordering::Relaxed);
        self.state.store(REQUESTED, Ordering::Release);
        Ok(())
    }
    #[cfg(test)]
    fn consume_request(&self) -> Result<Option<(usize, i64)>> {
        Ok(self
            .consume_request_with_quick()?
            .map(|(mode, timestamp, _)| (mode, timestamp)))
    }
    fn consume_request_with_quick(&self) -> Result<Option<(usize, i64, u32)>> {
        if self.stop.load(Ordering::Acquire) != 0 {
            return Ok(None);
        }
        self.state
            .compare_exchange(REQUESTED, CONSUMED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "待命 IPC 收到未发布或重复的请求信号")?;
        let mode = self.mode.load(Ordering::Relaxed) as usize;
        let timestamp = self.request_qpc.load(Ordering::Relaxed);
        let quick = self.quick.load(Ordering::Relaxed);
        if mode > 2 || timestamp <= 0 {
            return Err("待命 IPC 已发布的请求元数据无效".into());
        }
        crate::quick_action::QuickRecipe::unpack(quick)?;
        Ok(Some((mode, timestamp, quick)))
    }
}

// 无可继承 HANDLE；每个进程持有各自的 mapping/event/view。该类型不跨线程转移。
struct Channel {
    mapping: HANDLE,
    event: HANDLE,
    foreground: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    _thread_bound: PhantomData<Rc<()>>,
}
impl Channel {
    unsafe fn create(binding: Binding, control: HWND) -> Result<Self> {
        if !cfg!(target_arch = "x86_64") {
            return Err("待命 IPC 共享原子协议仅启用于 Windows x64".into());
        }
        let mapping_name = binding.object_name("metadata");
        SetLastError(ERROR_SUCCESS);
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            size_of::<RequestMetadata>() as u32,
            PCWSTR(mapping_name.as_ptr()),
        )?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mapping);
            return Err("待命 IPC 映射同名对象已存在；拒绝覆盖".into());
        }
        let mut channel = Self {
            mapping,
            event: HANDLE::default(),
            foreground: HANDLE::default(),
            view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
            _thread_bound: PhantomData,
        };
        let event_name = binding.object_name("request");
        SetLastError(ERROR_SUCCESS);
        channel.event = CreateEventW(None, false, false, PCWSTR(event_name.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Err("待命 IPC 事件同名对象已存在；拒绝复用".into());
        }
        let foreground_name = binding.object_name("foreground-complete");
        SetLastError(ERROR_SUCCESS);
        channel.foreground = CreateEventW(None, true, false, PCWSTR(foreground_name.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Err("待命 IPC 前台完成事件同名对象已存在；拒绝复用".into());
        }
        channel.map()?;
        // 三个对象均确定为新建后才初始化；碰撞和打开失败都不会写旧对象。
        channel.metadata().initialize(binding, control);
        Ok(channel)
    }
    unsafe fn open(binding: Binding) -> Result<Self> {
        let mapping_name = binding.object_name("metadata");
        let mapping = OpenFileMappingW(
            (FILE_MAP_READ | FILE_MAP_WRITE).0,
            false,
            PCWSTR(mapping_name.as_ptr()),
        )?;
        let mut channel = Self {
            mapping,
            event: HANDLE::default(),
            foreground: HANDLE::default(),
            view: MEMORY_MAPPED_VIEW_ADDRESS::default(),
            _thread_bound: PhantomData,
        };
        let event_name = binding.object_name("request");
        channel.event = OpenEventW(
            EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(event_name.as_ptr()),
        )?;
        let foreground_name = binding.object_name("foreground-complete");
        channel.foreground = OpenEventW(
            EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(foreground_name.as_ptr()),
        )?;
        channel.map()?;
        Ok(channel)
    }
    unsafe fn map(&mut self) -> Result<()> {
        self.view = MapViewOfFile(
            self.mapping,
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
            size_of::<RequestMetadata>(),
        );
        if self.view.Value.is_null() {
            return Err(windows::core::Error::from_win32().into());
        }
        if !(self.view.Value as usize).is_multiple_of(std::mem::align_of::<RequestMetadata>()) {
            return Err("待命 IPC 映射未满足原子字段对齐".into());
        }
        Ok(())
    }
    fn metadata(&self) -> &RequestMetadata {
        // 唯一入口在成功映射及创建/打开后使用；映射生命周期由 Channel 持有。
        unsafe { &*self.view.Value.cast::<RequestMetadata>() }
    }
    fn stop(&self) -> Result<()> {
        self.metadata().stop.store(1, Ordering::Release);
        // 初次 main wait 用 auto-reset request；两个并行前台门须由 manual-reset
        // foreground 一起唤醒。完成事件不等于授权：醒后必须先查 sticky stop。
        let request = unsafe { SetEvent(self.event) };
        let foreground = unsafe { SetEvent(self.foreground) };
        request?;
        foreground?;
        Ok(())
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        unsafe {
            if !self.view.Value.is_null() {
                let _ = UnmapViewOfFile(self.view);
            }
            if !self.event.is_invalid() {
                let _ = CloseHandle(self.event);
            }
            if !self.foreground.is_invalid() {
                let _ = CloseHandle(self.foreground);
            }
            if !self.mapping.is_invalid() {
                let _ = CloseHandle(self.mapping);
            }
        }
    }
}

fn trace_enabled() -> bool {
    std::env::var_os("RUSTSHOT_TRAY_DISPATCH_TRACE").is_some()
        || std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some()
}

#[derive(Default)]
pub(crate) struct SubmissionTrace {
    request_qpc: i64,
    worker_pid: usize,
    window: usize,
    transport: usize,
    validation_begin: i64,
    validation_end: i64,
    foreground_begin: i64,
    foreground_end: i64,
    metadata_begin: i64,
    metadata_end: i64,
    notify_begin: i64,
    notify_end: i64,
    foreground_complete_begin: i64,
    foreground_complete_end: i64,
    foreground_allowed: bool,
}
impl SubmissionTrace {
    fn new(request_qpc: i64, window: HWND, worker_pid: usize) -> Self {
        Self {
            request_qpc,
            worker_pid,
            window: window.0 as usize,
            ..Self::default()
        }
    }
    pub(crate) fn emit_after_ready(self, worker_pid: usize) {
        if self.worker_pid != worker_pid {
            return;
        }
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase":"standby_request_submission", "pid":std::process::id(),
            "worker_pid":worker_pid, "control_window":self.window,
            "transport":transport_name(self.transport), "request_qpc":self.request_qpc,
            "identity_validation_begin_qpc":self.validation_begin,
            "identity_validation_end_qpc":self.validation_end,
            "allow_foreground_begin_qpc":self.foreground_begin,
            "allow_foreground_end_qpc":self.foreground_end,
            "metadata_publish_begin_qpc":self.metadata_begin,
            "metadata_publish_end_qpc":self.metadata_end,
            "notify_begin_qpc":self.notify_begin, "notify_end_qpc":self.notify_end,
            "foreground_complete_begin_qpc":self.foreground_complete_begin,
            "foreground_complete_end_qpc":self.foreground_complete_end,
            "allow_foreground_succeeded":self.foreground_allowed,
            "emitted_after_actual_surface_ready":true
        }));
    }
}
fn mark_submission(
    trace: &mut Option<SubmissionTrace>,
    field: impl FnOnce(&mut SubmissionTrace) -> &mut i64,
) {
    if let Some(trace) = trace.as_mut() {
        *field(trace) = qpc();
    }
}

#[derive(Default)]
struct RequestWaitTrace {
    transport: usize,
    request_qpc: i64,
    request_mode: usize,
    wait_begin: i64,
    wait_end: i64,
    wake_kind: u32,
    wait_wake_kind: u32,
    priority_begin: i64,
    priority_end: i64,
    event_return_qpc: i64,
    peek_begin: i64,
    peek_end: i64,
    dispatch_begin: i64,
    dispatch_end: i64,
    previous_message: u32,
    previous_window: usize,
    previous_message_received: i64,
    previous_parent_poll_begin: i64,
    previous_parent_poll_end: i64,
    decode_begin: i64,
    decode_end: i64,
    parent_wait_begin: i64,
    parent_wait_end: i64,
    parent_window_begin: i64,
    parent_window_end: i64,
    parent_class_begin: i64,
    parent_class_end: i64,
    parent_props_begin: i64,
    parent_props_end: i64,
    environment_begin: i64,
    environment_end: i64,
    retain_begin: i64,
    retain_end: i64,
    remove_ready_begin: i64,
    remove_ready_end: i64,
    timer_begin: i64,
    timer_end: i64,
    return_qpc: i64,
    foreground_gate_begin: i64,
    foreground_gate_end: i64,
    foreground_state: u32,
}
thread_local! {
    static REQUEST_TRACE: RefCell<Option<RequestWaitTrace>> = const { RefCell::new(None) };
}
fn mark_request(
    trace: &mut Option<RequestWaitTrace>,
    field: impl FnOnce(&mut RequestWaitTrace) -> &mut i64,
) {
    if let Some(trace) = trace.as_mut() {
        *field(trace) = qpc();
    }
}
fn transport_name(transport: usize) -> &'static str {
    if transport == TRANSPORT_EVENT {
        "named-event-mapping-v2"
    } else {
        "window-only-fallback-v2"
    }
}
/// 只由 GPUI 原生真实 Ready 后调用；待命等待/首屏之前不输出诊断 IO。
pub(crate) fn emit_request_trace_after_ready() {
    REQUEST_TRACE.with(|slot| {
        if let Some(trace) = slot.borrow_mut().take() {
            let mut event = serde_json::json!({
                "phase":"standby_request_wait", "pid":std::process::id(),
                "transport":transport_name(trace.transport), "request_qpc":trace.request_qpc,
                "protocol_version":IPC_VERSION, "metadata_bytes":size_of::<RequestMetadata>(),
                "request_mode":trace.request_mode, "event_request_consumed":trace.transport == TRANSPORT_EVENT,
                "wait_begin_qpc":trace.wait_begin,"wait_return_qpc":trace.wait_end,
                "wake_kind":trace.wake_kind,
                "wait_wake_kind":trace.wait_wake_kind,
                "priority_recheck_begin_qpc":trace.priority_begin,"priority_recheck_end_qpc":trace.priority_end,
                "event_wait_return_qpc":trace.event_return_qpc,
                "message_peek_begin_qpc":trace.peek_begin,"message_peek_end_qpc":trace.peek_end,
                "message_dispatch_begin_qpc":trace.dispatch_begin,"message_dispatch_end_qpc":trace.dispatch_end,
                "previous_message_id":trace.previous_message,"previous_message_window":trace.previous_window,
                "previous_message_received_qpc":trace.previous_message_received,
                "previous_parent_poll_begin_qpc":trace.previous_parent_poll_begin,
                "previous_parent_poll_end_qpc":trace.previous_parent_poll_end,
                "metadata_decode_begin_qpc":trace.decode_begin,"metadata_decode_end_qpc":trace.decode_end
            });
            let timings = serde_json::json!({
                "parent_handle_wait_begin_qpc":trace.parent_wait_begin,"parent_handle_wait_end_qpc":trace.parent_wait_end,
                "parent_window_identity_begin_qpc":trace.parent_window_begin,"parent_window_identity_end_qpc":trace.parent_window_end,
                "parent_class_begin_qpc":trace.parent_class_begin,"parent_class_end_qpc":trace.parent_class_end,
                "parent_properties_begin_qpc":trace.parent_props_begin,"parent_properties_end_qpc":trace.parent_props_end,
                "request_environment_begin_qpc":trace.environment_begin,"request_environment_end_qpc":trace.environment_end,
                "warm_retain_begin_qpc":trace.retain_begin,"warm_retain_end_qpc":trace.retain_end,
                "remove_ready_begin_qpc":trace.remove_ready_begin,"remove_ready_end_qpc":trace.remove_ready_end,
                "kill_timer_begin_qpc":trace.timer_begin,"kill_timer_end_qpc":trace.timer_end,
                "return_to_mode_qpc":trace.return_qpc,
                "foreground_gate_begin_qpc":trace.foreground_gate_begin,
                "foreground_gate_end_qpc":trace.foreground_gate_end,
                "foreground_state":trace.foreground_state,
                "request_thread_owns_control_window":false,
                "emitted_after_actual_surface_ready":true
            });
            if let serde_json::Value::Object(timings) = timings {
                event.as_object_mut().expect("固定诊断对象").extend(timings);
            }
            crate::startup_probe::emit_trace_value(&event);
        }
    });
}

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
}
pub(crate) fn qpc() -> i64 {
    let mut value = 0;
    unsafe {
        QueryPerformanceCounter(&mut value);
    }
    value
}

fn missing_ready_channel(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| {
            error.code() == windows::core::HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
        })
}

// 只取已发布、尚未消费的本实例控制窗；原 Child HANDLE 和窗口属性由调用者核验。
// PREPARING/零页不是错误，不能据此销毁正在装载的待命进程。
fn poll_ready_control(
    metadata: &RequestMetadata,
    binding: Binding,
    ready_window: impl FnOnce(HWND) -> bool,
) -> Option<HWND> {
    if metadata.state.load(Ordering::Acquire) != IDLE
        || metadata.stop.load(Ordering::Acquire) != 0
        || metadata.foreground_state.load(Ordering::Acquire) != FOREGROUND_PENDING
    {
        return None;
    }
    let control = HWND(metadata.control_window.load(Ordering::Relaxed) as usize as *mut _);
    if control.0.is_null() || !metadata.valid(binding, control) || !ready_window(control) {
        return None;
    }
    Some(control)
}

#[derive(Clone, Copy)]
struct ReadyPublication {
    pid: u32,
    transport: usize,
    version: usize,
    bytes: usize,
    ready: bool,
}
fn explicit_window_fallback(
    expected_pid: u32,
    same_instance: bool,
    published: ReadyPublication,
) -> bool {
    published.pid == expected_pid
        && same_instance
        && published.transport == TRANSPORT_WINDOW
        && published.version == IPC_VERSION as usize
        && published.bytes == size_of::<RequestMetadata>()
        && published.ready
}

pub(crate) struct Slot {
    pub child: Child,
    window: Option<HWND>,
    token: i64,
    owner: HWND,
    parent_token: usize,
    channel: Option<Channel>,
    request_published: bool,
    submission_trace: Option<SubmissionTrace>,
    trace: bool,
}
impl Slot {
    pub fn spawn(owner: HWND) -> Result<Self> {
        let token = qpc();
        let parent_token = unsafe { GetPropW(owner, w!("RustShot.Resident.Token")) }.0 as usize;
        if token <= 0 || parent_token == 0 {
            return Err("待命进程缺少有效实例标识".into());
        }
        let child = Command::new(std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE))
            .args([
                "--capture-standby",
                &std::process::id().to_string(),
                &(owner.0 as usize).to_string(),
                &token.to_string(),
                &parent_token.to_string(),
            ])
            .creation_flags(0x08000000)
            .spawn()?;
        Ok(Self {
            child,
            window: None,
            token,
            owner,
            parent_token,
            channel: None,
            request_published: false,
            submission_trace: None,
            trace: trace_enabled(),
        })
    }
    fn binding(&self) -> Binding {
        Binding {
            parent_pid: std::process::id(),
            child_pid: self.child.id(),
            parent_token: self.parent_token,
            child_token: self.token,
            parent_window: self.owner,
        }
    }
    /// READY 通知遗漏时补绑同一个原 Child；不改绑其它进程，不创建或唤醒请求。
    pub fn poll_ready(&mut self) -> Result<bool> {
        if let Some(window) = self.window {
            // 已完成完整 READY 绑定，不在每个 Timer 重开同一组 IPC HANDLE。
            return Ok(self.valid(window) && !self.request_published);
        }
        if self.child.try_wait()?.is_some() {
            return Ok(false);
        }
        let binding = self.binding();
        let channel = match unsafe { Channel::open(binding) } {
            Ok(channel) => channel,
            // 未创建对象通常仍在 Preparing；只有明确 WINDOW 的同实例控制窗可补绑。
            // Event 模式缺映射绝不能静默降级为窗口消息。
            Err(error) if missing_ready_channel(error.as_ref()) => {
                return self.poll_window_fallback()
            }
            Err(error) => return Err(error),
        };
        let Some(control) = poll_ready_control(channel.metadata(), binding, |control| {
            self.valid(control)
                && !unsafe { GetPropW(control, w!("RustShot.Standby.Ready")) }.is_invalid()
        }) else {
            return Ok(false);
        };
        // 再使用通知路径的完整核验，包含原 HANDLE、传输版本、Ready 与 IPC 状态。
        // 先前只读探测通过不能替代这次核验；此处不投递 REQUEST。
        self.accept_ready(self.child.id() as usize, control)
    }
    fn poll_window_fallback(&mut self) -> Result<bool> {
        struct OwnedWindows {
            pid: u32,
            windows: Vec<HWND>,
        }
        unsafe extern "system" fn collect(window: HWND, parameter: LPARAM) -> windows::core::BOOL {
            let owned = &mut *(parameter.0 as *mut OwnedWindows);
            let mut pid = 0;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if pid == owned.pid {
                owned.windows.push(window);
            }
            windows::core::BOOL(1)
        }
        let mut owned = OwnedWindows {
            pid: self.child.id(),
            windows: Vec::new(),
        };
        unsafe {
            EnumWindows(Some(collect), LPARAM(&mut owned as *mut _ as isize))?;
        }
        for control in owned.windows {
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(control, Some(&mut pid));
            }
            // 枚举只收原 PID，随后仍使用原 Child HANDLE/class/tokens 完整核验。
            // 控制窗或 PID 复用时不能凭枚举结果接纳。
            let same_instance = self.valid(control);
            if !same_instance {
                continue;
            }
            let published = unsafe {
                ReadyPublication {
                    pid,
                    transport: GetPropW(control, w!("RustShot.Standby.Transport")).0 as usize,
                    version: GetPropW(control, w!("RustShot.Standby.TransportVersion")).0 as usize,
                    bytes: GetPropW(control, w!("RustShot.Standby.MetadataBytes")).0 as usize,
                    ready: !GetPropW(control, w!("RustShot.Standby.Ready")).is_invalid(),
                }
            };
            if explicit_window_fallback(self.child.id(), same_instance, published) {
                // accept_ready 再读取真实发布属性，并且只接纳 worker 明确公布的兼容模式。
                return self.accept_ready(self.child.id() as usize, control);
            }
        }
        Ok(false)
    }
    pub fn accept_ready(&mut self, pid: usize, window: HWND) -> Result<bool> {
        if pid != self.child.id() as usize
            || !matches!(self.child.try_wait(), Ok(None))
            || !self.valid(window)
        {
            return Ok(false);
        }
        unsafe {
            if GetPropW(window, w!("RustShot.Standby.TransportVersion")).0 as usize
                != IPC_VERSION as usize
                || GetPropW(window, w!("RustShot.Standby.MetadataBytes")).0 as usize
                    != size_of::<RequestMetadata>()
                || GetPropW(window, w!("RustShot.Standby.Ready")).is_invalid()
            {
                return Err("待命窗口未发布完整传输版本及 READY".into());
            }
            match GetPropW(window, w!("RustShot.Standby.Transport")).0 as usize {
                TRANSPORT_EVENT => {
                    let channel = Channel::open(self.binding())?;
                    if !channel.metadata().valid(self.binding(), window)
                        || channel.metadata().state.load(Ordering::Acquire) != IDLE
                        || channel.metadata().stop.load(Ordering::Acquire) != 0
                        || channel.metadata().foreground_state.load(Ordering::Acquire)
                            != FOREGROUND_PENDING
                    {
                        return Err("待命 IPC READY 绑定或状态不符；拒绝窗口回退".into());
                    }
                    self.channel = Some(channel);
                }
                TRANSPORT_WINDOW => {
                    // 只有 worker 初始化 IPC 失败并明确公布的兼容模式可以收旧消息。
                    self.channel = None;
                }
                _ => return Err("待命窗口未公布支持的请求传输".into()),
            }
        }
        self.window = Some(window);
        Ok(true)
    }
    fn valid(&self, window: HWND) -> bool {
        unsafe {
            let mut owner = 0;
            GetWindowThreadProcessId(window, Some(&mut owner));
            let mut class = [0u16; 64];
            let length = GetClassNameW(window, &mut class).max(0) as usize;
            owner == self.child.id()
                && WaitForSingleObject(HANDLE(self.child.as_raw_handle()), 0) == WAIT_TIMEOUT
                && String::from_utf16_lossy(&class[..length]) == "RustShot.Capture.Standby"
                && GetPropW(window, w!("RustShot.Standby.Owner")).0 as usize
                    == std::process::id() as usize
                && GetPropW(window, w!("RustShot.Standby.OwnerWindow")).0 == self.owner.0
                && GetPropW(window, w!("RustShot.Standby.Token")).0 as isize == self.token as isize
                && GetPropW(window, w!("RustShot.Standby.ParentToken")).0 as usize
                    == self.parent_token
        }
    }
    /// REQUESTED 已发布后，无论通知是否成功，本次请求都不能重排到新 Slot。
    pub(crate) fn request_published(&self) -> bool {
        self.request_published
    }
    pub fn request_quick(&mut self, mode: usize, timestamp: i64, quick: u32) -> Result<bool> {
        let Some(window) = self.window else {
            return Ok(false);
        };
        if mode > 2 || timestamp <= 0 {
            return Err("待命请求 mode/QPC 无效".into());
        }
        crate::quick_action::QuickRecipe::unpack(quick)?;
        let mut trace = self
            .trace
            .then(|| SubmissionTrace::new(timestamp, window, self.child.id() as usize));
        mark_submission(&mut trace, |trace| &mut trace.validation_begin);
        if !self.valid(window) {
            return Err("待命窗口已失效".into());
        }
        let transport = unsafe { GetPropW(window, w!("RustShot.Standby.Transport")) }.0 as usize;
        if transport == TRANSPORT_EVENT {
            let channel = self.channel.as_ref().ok_or("待命 IPC 通道尚未绑定")?;
            if !channel.metadata().valid(self.binding(), window) {
                return Err("待命 IPC 请求身份变更".into());
            }
        } else if transport != TRANSPORT_WINDOW || self.channel.is_some() {
            return Err("待命请求传输变更；拒绝降级投递".into());
        }
        mark_submission(&mut trace, |trace| &mut trace.validation_end);
        mark_submission(&mut trace, |trace| &mut trace.metadata_begin);
        if let Some(channel) = self.channel.as_ref() {
            channel
                .metadata()
                .publish_request_with_quick(mode, timestamp, quick)?;
            self.request_published = true;
        }
        mark_submission(&mut trace, |trace| &mut trace.metadata_end);
        if let Some(channel) = self.channel.as_ref() {
            mark_submission(&mut trace, |trace| &mut trace.notify_begin);
            unsafe {
                SetEvent(channel.event)?;
            }
            mark_submission(&mut trace, |trace| &mut trace.notify_end);
        }
        // 请求先唤醒，桌面捕获与本次真实点击后的前台授权并行。
        // worker 的原生表面启动仍须通过独立完成门及实际 focus/Ready 核验。
        mark_submission(&mut trace, |trace| &mut trace.foreground_begin);
        let allowed = unsafe { AllowSetForegroundWindow(self.child.id()).is_ok() };
        mark_submission(&mut trace, |trace| &mut trace.foreground_end);
        if let Some(channel) = self.channel.as_ref() {
            mark_submission(&mut trace, |trace| &mut trace.foreground_complete_begin);
            channel.metadata().foreground_state.store(
                if allowed {
                    FOREGROUND_ALLOWED
                } else {
                    FOREGROUND_DENIED
                },
                Ordering::Release,
            );
            unsafe {
                SetEvent(channel.foreground)?;
            }
            mark_submission(&mut trace, |trace| &mut trace.foreground_complete_end);
        } else {
            // 明确窗口兼容模式由 controller 中继；维持授权完成后才发旧 REQUEST 的顺序。
            mark_submission(&mut trace, |trace| &mut trace.notify_begin);
            unsafe {
                PostMessageW(
                    Some(window),
                    REQUEST,
                    WPARAM(mode | ((quick as usize) << 32)),
                    LPARAM(timestamp as isize),
                )?;
            }
            self.request_published = true;
            mark_submission(&mut trace, |trace| &mut trace.notify_end);
        }
        if let Some(trace) = trace.as_mut() {
            trace.transport = transport;
            trace.foreground_allowed = allowed;
        }
        self.submission_trace = trace;
        Ok(true)
    }
    pub(crate) fn take_submission_trace(&mut self) -> Option<SubmissionTrace> {
        self.submission_trace.take()
    }
    /// 仅对本 Slot 原 Child HANDLE 所绑定的 IPC 写入停止；尚未收到 READY 也可尝试打开。
    pub(crate) fn request_stop(&mut self) -> Result<bool> {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return Ok(false);
        }
        if let Some(channel) = self.channel.as_ref() {
            let control =
                HWND(channel.metadata().control_window.load(Ordering::Relaxed) as usize as *mut _);
            if !channel.metadata().valid(self.binding(), control)
                || !self.valid(control)
                || unsafe { GetPropW(control, w!("RustShot.Standby.Transport")) }.0 as usize
                    != TRANSPORT_EVENT
            {
                return Err("待命 IPC 停止身份不符".into());
            }
            channel.stop()?;
            return Ok(true);
        }
        let channel = unsafe { Channel::open(self.binding()) };
        let channel = match channel {
            Ok(channel) => channel,
            Err(_) => return Ok(false), // IPC 尚未创建或明确窗口回退；Closing + WM_CLOSE 仍保留。
        };
        if channel.metadata().state.load(Ordering::Acquire) == 0 {
            return Ok(false); // 新对象还未发布 header；Closing 属性的最终校验仍会结束加载。
        }
        let control =
            HWND(channel.metadata().control_window.load(Ordering::Relaxed) as usize as *mut _);
        if !channel.metadata().valid(self.binding(), control)
            || !self.valid(control)
            || unsafe { GetPropW(control, w!("RustShot.Standby.Transport")) }.0 as usize
                != TRANSPORT_EVENT
        {
            return Err("待命 IPC 提前停止身份不符；未写入对象".into());
        }
        channel.stop()?;
        Ok(true)
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_CLOSE {
        let _ = PostMessageW(Some(window), STOP, WPARAM(0), LPARAM(0));
        LRESULT(0)
    } else {
        DefWindowProcW(window, message, wp, lp)
    }
}
#[derive(Debug)]
struct StandbyCancelled;
impl std::fmt::Display for StandbyCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("待命截图已由本实例停止")
    }
}
impl std::error::Error for StandbyCancelled {}
pub(crate) fn is_cancelled(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<StandbyCancelled>()
}
fn cancelled<T>() -> Result<T> {
    Err(Box::new(StandbyCancelled))
}

unsafe fn owned_handle(handle: HANDLE) -> OwnedHandle {
    OwnedHandle::from_raw_handle(handle.0)
}
fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}
fn classify_parent_wait(status: WAIT_EVENT) -> Result<bool> {
    match status {
        WAIT_TIMEOUT => Ok(false),
        WAIT_OBJECT_0 => Ok(true),
        WAIT_FAILED => Err(windows::core::Error::from_win32().into()),
        _ => Err("原托盘进程等待返回未知状态".into()),
    }
}
fn parent_exited(parent: HANDLE) -> Result<bool> {
    // 必须在其它 API/QPC 之前分类，保留 WAIT_FAILED 的原始 GetLastError。
    classify_parent_wait(unsafe { WaitForSingleObject(parent, 0) })
}
unsafe fn duplicate(handle: HANDLE) -> Result<OwnedHandle> {
    let mut copied = HANDLE::default();
    DuplicateHandle(
        GetCurrentProcess(),
        handle,
        GetCurrentProcess(),
        &mut copied,
        0,
        false,
        DUPLICATE_SAME_ACCESS,
    )?;
    Ok(owned_handle(copied))
}
unsafe fn private_event() -> Result<OwnedHandle> {
    Ok(owned_handle(CreateEventW(None, false, false, None)?))
}

enum ControllerCommand {
    WarmReady {
        identity: Option<crate::startup_surface::WarmSurfaceIdentity>,
        gdi_primed: bool,
        prewarm_failed: bool,
    },
    Consumed,
    CaptureStarted,
    Shutdown,
}
struct ControllerInfo {
    control: usize,
    transport: usize,
    request_wake: OwnedHandle,
    legacy: mpsc::Receiver<(usize, i64, u32)>,
}
struct Controller {
    commands: mpsc::Sender<ControllerCommand>,
    wake: OwnedHandle,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<std::result::Result<(), String>>>,
}
impl Controller {
    fn send(&self, command: ControllerCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| "待命控制线程命令通道已关闭")?;
        unsafe {
            SetEvent(raw_handle(&self.wake))?;
        }
        Ok(())
    }
    fn start(
        binding: Binding,
        parent: HANDLE,
        main_thread: u32,
    ) -> Result<(
        Self,
        mpsc::Receiver<std::result::Result<ControllerInfo, String>>,
    )> {
        let wake = unsafe { private_event()? };
        let thread_wake = unsafe { duplicate(raw_handle(&wake))? };
        let thread_parent = unsafe { duplicate(parent)? };
        let (commands, receiver) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        // HWND/原子 view 由专用线程本地创建；只传标量 binding 和独立 OwnedHandle。
        let scalar_binding = (
            binding.parent_pid,
            binding.child_pid,
            binding.parent_token,
            binding.child_token,
            binding.parent_window.0 as usize,
        );
        let worker = thread::Builder::new()
            .name("rustshot-standby-controller".into())
            .spawn(move || {
                let binding = Binding {
                    parent_pid: scalar_binding.0,
                    child_pid: scalar_binding.1,
                    parent_token: scalar_binding.2,
                    child_token: scalar_binding.3,
                    parent_window: HWND(scalar_binding.4 as *mut _),
                };
                controller_thread(
                    binding,
                    thread_parent,
                    thread_wake,
                    main_thread,
                    receiver,
                    ready,
                    thread_stopped,
                )
                .map_err(|error| error.to_string())
            })?;
        Ok((
            Self {
                commands,
                wake,
                stopped,
                worker: Some(worker),
            },
            initialized,
        ))
    }
    fn finish(&mut self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let _ = self.commands.send(ControllerCommand::Shutdown);
        let signaled = unsafe { SetEvent(raw_handle(&self.wake)) };
        let joined: Result<()> = match self.worker.take() {
            Some(worker) => match worker.join() {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.into()),
                Err(_) => Err("待命控制线程发生异常".into()),
            },
            None => Ok(()),
        };
        joined?;
        signaled?;
        Ok(())
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        if self.worker.is_some() {
            if let Err(error) = self.finish() {
                eprintln!("待命控制线程回收失败：{error}");
            }
        }
    }
}

struct ControllerWindow {
    window: HWND,
    channel: Option<Channel>,
    fallback_wake: Option<OwnedHandle>,
    stopped: Arc<AtomicBool>,
    parent: OwnedHandle,
}
impl ControllerWindow {
    fn stop_main(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        if let Some(channel) = self.channel.as_ref() {
            channel.stop()?;
        } else if let Some(wake) = self.fallback_wake.as_ref() {
            unsafe {
                SetEvent(raw_handle(wake))?;
            }
        }
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        let stopped = self.stop_main();
        if let Some(channel) = self.channel.as_ref() {
            channel.metadata().state.store(CLOSED, Ordering::Release);
        }
        // WindowProc 无 Rust 借用；必须在创建线程销毁 HWND 后才放 creator view/events。
        unsafe {
            DestroyWindow(self.window)?;
        }
        self.window = HWND::default();
        self.channel = None;
        stopped
    }
    unsafe fn stop_own_windows(&self, main_thread: u32) {
        struct CloseRequest {
            controller: isize,
            has_pins: bool,
        }
        // 此控制线程只属于当前 worker；不枚举/关闭其他进程的窗口。
        unsafe extern "system" fn close_own(hwnd: HWND, control: LPARAM) -> windows::core::BOOL {
            let request = &mut *(control.0 as *mut CloseRequest);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == std::process::id() && hwnd.0 as isize != request.controller {
                if !GetPropW(hwnd, w!("RustShot.Pin.v1")).is_invalid() {
                    request.has_pins = true;
                    crate::pin_workspace_runtime::preserve_on_exit(hwnd);
                }
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            windows::core::BOOL(1)
        }
        let mut request = CloseRequest {
            controller: self.window.0 as isize,
            has_pins: false,
        };
        let _ = EnumWindows(
            Some(close_own),
            LPARAM(&mut request as *mut CloseRequest as isize),
        );
        // 创建 GPUI 队列之前可能失败；stop 原子及后续 gate 仍阻止初始化/呈现。
        // 有贴图时让 WM_CLOSE 完成异步单帧和工作区保存，由最后一个窗口退出。
        if !request.has_pins {
            let _ = PostThreadMessageW(main_thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}
impl Drop for ControllerWindow {
    fn drop(&mut self) {
        let _ = self.stop_main();
        unsafe {
            if let Some(channel) = self.channel.as_ref() {
                channel.metadata().state.store(CLOSED, Ordering::Release);
            }
            if !self.window.is_invalid() {
                let _ = DestroyWindow(self.window);
            }
        }
        // HWND 已在其创建线程销毁；此后才回收 creator view/event/foreground event。
        self.channel = None;
    }
}

unsafe fn publish_warm(
    control: HWND,
    identity: Option<crate::startup_surface::WarmSurfaceIdentity>,
    gdi_primed: bool,
    prewarm_failed: bool,
) -> Result<()> {
    if gdi_primed {
        SetPropW(
            control,
            w!("RustShot.Standby.PrimedSinglePixelAndDiscarded"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )?;
    }
    if prewarm_failed {
        SetPropW(
            control,
            w!("RustShot.Standby.PrewarmFailed"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )?;
    }
    if let Some(identity) = identity {
        for (name, value) in [
            (w!("RustShot.Standby.PrewarmedSurface"), identity.hwnd),
            (w!("RustShot.Standby.PrewarmedToken"), identity.token),
            (w!("RustShot.Standby.PrewarmedReadyQpc"), identity.ready_qpc),
            (
                w!("RustShot.Standby.PrewarmedCloaked"),
                identity.cloaked as usize,
            ),
            (
                w!("RustShot.Standby.PrewarmedWidth"),
                identity.physical_bounds.width as usize,
            ),
            (
                w!("RustShot.Standby.PrewarmedHeight"),
                identity.physical_bounds.height as usize,
            ),
            (
                w!("RustShot.Standby.PrewarmedLeft"),
                identity.physical_bounds.left as isize as usize,
            ),
            (
                w!("RustShot.Standby.PrewarmedTop"),
                identity.physical_bounds.top as isize as usize,
            ),
            (
                w!("RustShot.Standby.PrewarmedWindowVisible"),
                identity.window_visible as usize,
            ),
        ] {
            if value != 0 {
                SetPropW(control, name, Some(HANDLE(value as *mut _)))?;
            }
        }
    }
    Ok(())
}

fn controller_thread(
    binding: Binding,
    parent: OwnedHandle,
    wake: OwnedHandle,
    main_thread: u32,
    commands: mpsc::Receiver<ControllerCommand>,
    initialized: mpsc::SyncSender<std::result::Result<ControllerInfo, String>>,
    stopped: Arc<AtomicBool>,
) -> Result<()> {
    let created = (|| unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let name = w!("RustShot.Capture.Standby");
        let class = WNDCLASSW {
            hInstance: instance,
            lpfnWndProc: Some(window_proc),
            lpszClassName: name,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows::core::Error::from_win32().into());
        }
        let controller_thread = GetCurrentThreadId();
        if controller_thread == main_thread {
            return Err("待命控制窗口不能归截图主线程".into());
        }
        // 只禁专用无文本 controller；GPUI 主线程从未调用 ImmDisableIME。
        let ime_disabled = ImmDisableIME(controller_thread).as_bool();
        let control = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            name,
            name,
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        let mut owner = ControllerWindow {
            window: control,
            channel: None,
            fallback_wake: None,
            stopped: stopped.clone(),
            parent,
        };
        if ImmAssociateContextEx(control, HIMC::default(), 0).as_bool() {
            SetPropW(
                control,
                w!("RustShot.Standby.ImeDetached"),
                Some(HANDLE(std::ptr::dangling_mut())),
            )?;
        }
        for (name, value) in [
            (w!("RustShot.Standby.Owner"), binding.parent_pid as usize),
            (
                w!("RustShot.Standby.OwnerWindow"),
                binding.parent_window.0 as usize,
            ),
            (w!("RustShot.Standby.Token"), binding.child_token as usize),
            (w!("RustShot.Standby.ParentToken"), binding.parent_token),
            (
                w!("RustShot.Standby.TransportVersion"),
                IPC_VERSION as usize,
            ),
            (
                w!("RustShot.Standby.MetadataBytes"),
                size_of::<RequestMetadata>(),
            ),
            (w!("RustShot.Standby.MainThreadId"), main_thread as usize),
            (
                w!("RustShot.Standby.ControllerThreadId"),
                controller_thread as usize,
            ),
            (w!("RustShot.Standby.ControllerImeAttempted"), 1),
            (
                w!("RustShot.Standby.ControllerImeDisabled"),
                ime_disabled as usize,
            ),
        ] {
            if value != 0 {
                SetPropW(control, name, Some(HANDLE(value as *mut _)))?;
            }
        }
        owner.channel = match Channel::create(binding, control) {
            Ok(channel) => Some(channel),
            Err(error) => {
                eprintln!("待命 IPC v2 初始化失败，使用明确 controller 窗口兼容模式：{error}");
                owner.fallback_wake = Some(private_event()?);
                None
            }
        };
        let transport = if owner.channel.is_some() {
            TRANSPORT_EVENT
        } else {
            TRANSPORT_WINDOW
        };
        SetPropW(
            control,
            w!("RustShot.Standby.Transport"),
            Some(HANDLE(transport as *mut _)),
        )?;
        let request_wake = duplicate(owner.channel.as_ref().map_or_else(
            || raw_handle(owner.fallback_wake.as_ref().expect("明确 fallback event")),
            |channel| channel.event,
        ))?;
        Ok::<_, Box<dyn std::error::Error>>((owner, request_wake, transport))
    })();
    let (owner, request_wake, transport) = match created {
        Ok(created) => created,
        Err(error) => {
            stopped.store(true, Ordering::Release);
            let _ = initialized.send(Err(error.to_string()));
            return Err(error);
        }
    };
    let (legacy_send, legacy) = mpsc::channel();
    if initialized
        .send(Ok(ControllerInfo {
            control: owner.window.0 as usize,
            transport,
            request_wake,
            legacy,
        }))
        .is_err()
    {
        return Ok(());
    }
    let mut published = false;
    let mut active = false;
    let mut stopping = false;
    let mut message = MSG::default();
    let result = (|| unsafe {
        loop {
            while let Ok(command) = commands.try_recv() {
                match command {
                    ControllerCommand::Shutdown => return Ok(()),
                    ControllerCommand::Consumed => {
                        active = true;
                        let _ = RemovePropW(owner.window, w!("RustShot.Standby.Ready"));
                    }
                    ControllerCommand::CaptureStarted => {
                        // 只有 main 成功取得 capture mutex 后才入队；这里不搜索其它托盘窗口。
                        let parent_exited = parent_exited(raw_handle(&owner.parent))?;
                        if stopping
                            || stopped.load(Ordering::Acquire)
                            || parent_exited
                            || !valid_parent(
                                binding.parent_window,
                                binding.parent_pid,
                                binding.parent_token,
                            )
                        {
                            owner.stop_main()?;
                            let _ = RemovePropW(owner.window, w!("RustShot.Standby.Ready"));
                            stopping = true;
                            continue;
                        }
                        if !active {
                            return Err("截图启动通知早于唯一请求消费".into());
                        }
                        PostMessageW(
                            Some(binding.parent_window),
                            crate::ui::CAPTURE_STARTED_MESSAGE,
                            WPARAM(std::process::id() as usize),
                            LPARAM(0),
                        )?;
                    }
                    ControllerCommand::WarmReady {
                        identity,
                        gdi_primed,
                        prewarm_failed,
                    } => {
                        let parent_exited = parent_exited(raw_handle(&owner.parent))?;
                        if stopping
                            || stopped.load(Ordering::Acquire)
                            || parent_exited
                            || !valid_parent(
                                binding.parent_window,
                                binding.parent_pid,
                                binding.parent_token,
                            )
                        {
                            owner.stop_main()?;
                            stopping = true;
                            continue;
                        }
                        publish_warm(owner.window, identity, gdi_primed, prewarm_failed)?;
                        if let Some(channel) = owner.channel.as_ref() {
                            if channel.metadata().stop.load(Ordering::Acquire) != 0 {
                                owner.stop_main()?;
                                stopping = true;
                                continue;
                            }
                            channel.metadata().state.store(IDLE, Ordering::Release);
                        }
                        SetPropW(
                            owner.window,
                            w!("RustShot.Standby.Ready"),
                            Some(HANDLE(std::ptr::dangling_mut())),
                        )?;
                        PostMessageW(
                            Some(binding.parent_window),
                            READY,
                            WPARAM(std::process::id() as usize),
                            LPARAM(owner.window.0 as isize),
                        )?;
                        published = true;
                    }
                }
            }
            if !stopping {
                let parent_exited = parent_exited(raw_handle(&owner.parent))?;
                if stopped.load(Ordering::Acquire)
                    || parent_exited
                    || !valid_parent(
                        binding.parent_window,
                        binding.parent_pid,
                        binding.parent_token,
                    )
                {
                    owner.stop_main()?;
                    let _ = RemovePropW(owner.window, w!("RustShot.Standby.Ready"));
                    stopping = true;
                }
            }
            if stopping && active {
                owner.stop_own_windows(main_thread);
            }
            let handles = if stopping {
                vec![raw_handle(&wake)]
            } else {
                vec![raw_handle(&owner.parent), raw_handle(&wake)]
            };
            let status = MsgWaitForMultipleObjectsEx(
                Some(&handles),
                if stopping { 50 } else { 1000 },
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            if status == WAIT_FAILED {
                return Err(windows::core::Error::from_win32().into());
            }
            if status.0 < WAIT_OBJECT_0.0 + handles.len() as u32 || status == WAIT_TIMEOUT {
                continue;
            }
            if status.0 != WAIT_OBJECT_0.0 + handles.len() as u32 {
                return Err("待命 controller 收到未知等待结果".into());
            }
            if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                continue;
            }
            if message.message == WM_QUIT || message.hwnd == owner.window && message.message == STOP
            {
                owner.stop_main()?;
                let _ = RemovePropW(owner.window, w!("RustShot.Standby.Ready"));
                stopping = true;
                continue;
            }
            if message.hwnd == owner.window && message.message == REQUEST {
                // IPC 的旧 REQUEST 始终忽略；只有明确 window fallback 可以中继一次标量。
                if transport == TRANSPORT_WINDOW
                    && published
                    && !active
                    && !stopping
                    && message.wParam.0 & 0xffff_ffff <= 2
                    && crate::quick_action::QuickRecipe::unpack((message.wParam.0 >> 32) as u32)
                        .is_ok()
                    && message.lParam.0 > 0
                    && valid_parent(
                        binding.parent_window,
                        binding.parent_pid,
                        binding.parent_token,
                    )
                {
                    legacy_send
                        .send((
                            message.wParam.0 & 0xffff_ffff,
                            message.lParam.0 as i64,
                            (message.wParam.0 >> 32) as u32,
                        ))
                        .map_err(|_| "待命主线程兼容请求通道已关闭")?;
                    active = true;
                    RemovePropW(owner.window, w!("RustShot.Standby.Ready"))?;
                    SetEvent(raw_handle(
                        owner.fallback_wake.as_ref().expect("fallback wake"),
                    ))?;
                }
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    })();
    if result.is_err() && active {
        // 内核 stop 唤醒准备门；已运行的 GPUI 消息泵也需要自有窗口关闭消息。
        let _ = owner.stop_main();
        unsafe { owner.stop_own_windows(main_thread) };
    }
    // 外层显式释放/返回错误；Drop 仅处理初始化或异常路径的备用回收。
    let cleanup = owner.finish();
    cleanup?;
    result
}

struct WorkerEndpoint {
    binding: Binding,
    control: HWND,
    process: OwnedHandle,
    channel: Option<Channel>,
    request_wake: OwnedHandle,
    legacy: mpsc::Receiver<(usize, i64, u32)>,
    stopped: Arc<AtomicBool>,
    transport: usize,
}
impl WorkerEndpoint {
    fn cancelled(&self) -> Result<bool> {
        let parent_exited = parent_exited(raw_handle(&self.process))?;
        Ok(parent_exited
            || self.stopped.load(Ordering::Acquire)
            || self
                .channel
                .as_ref()
                .is_some_and(|channel| channel.metadata().stop.load(Ordering::Acquire) != 0))
    }
}
struct Owner {
    cursor_task: Option<crate::cursor::StandbyCursorGuard>,
    ocr_warmup: Option<crate::recognition::task::CancelOwner>,
    _endpoint: Rc<WorkerEndpoint>,
    controller: Controller,
}
pub(crate) struct CaptureRequest {
    pub full_screen: bool,
    pub all_screens: bool,
    pub quick: u32,
    _owner: Owner,
}
impl CaptureRequest {
    pub(crate) fn finish(mut self) -> Result<()> {
        self._owner.finish()
    }
}
impl Owner {
    fn finish(&mut self) -> Result<()> {
        self.ocr_warmup.take();
        FOREGROUND_GATE.with(|slot| {
            slot.borrow_mut().take();
        });
        self.cursor_task.take();
        self.controller.finish()
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.ocr_warmup.take();
        FOREGROUND_GATE.with(|slot| {
            slot.borrow_mut().take();
        });
        self.cursor_task.take();
        crate::fast_capture::discard_prewarmed_thread();
        // controller 独立资源直到 Shutdown + join 后才释放；原 parent/channel 仍由 endpoint 持有。
        if self.controller.worker.is_some() {
            if let Err(error) = self.finish() {
                eprintln!("待命 Owner 回收失败：{error}");
            }
        }
    }
}
struct ForegroundGate {
    cursor_task: Option<crate::cursor::CursorTaskTicket>,
    endpoint: Rc<WorkerEndpoint>,
    completed: bool,
    reporter: Option<CaptureStartedReporter>,
}
struct CaptureStartedReporter {
    commands: mpsc::Sender<ControllerCommand>,
    wake: OwnedHandle,
    reported: bool,
}
pub(crate) struct PreparationGate {
    cursor_task: Option<crate::cursor::CursorTaskTicket>,
    binding: (u32, u32, usize, i64, usize),
    control: usize,
    parent: OwnedHandle,
    request: OwnedHandle,
    transport: usize,
    stopped: Arc<AtomicBool>,
    trace: Option<RequestWaitTrace>,
}
pub(crate) fn preparation_gate() -> Result<Option<PreparationGate>> {
    FOREGROUND_GATE.with(|slot| {
        let slot = slot.borrow();
        let Some(gate) = slot.as_ref() else {
            return Ok(None);
        };
        let endpoint = &gate.endpoint;
        Ok(Some(PreparationGate {
            cursor_task: gate.cursor_task.clone(),
            binding: (
                endpoint.binding.parent_pid,
                endpoint.binding.child_pid,
                endpoint.binding.parent_token,
                endpoint.binding.child_token,
                endpoint.binding.parent_window.0 as usize,
            ),
            control: endpoint.control.0 as usize,
            parent: unsafe { duplicate(raw_handle(&endpoint.process))? },
            request: unsafe { duplicate(raw_handle(&endpoint.request_wake))? },
            transport: endpoint.transport,
            stopped: endpoint.stopped.clone(),
            trace: REQUEST_TRACE.with(|trace| trace.borrow_mut().take()),
        }))
    })
}
pub(crate) fn install_preparation_gate(gate: Option<PreparationGate>) -> Result<()> {
    let Some(gate) = gate else {
        return Ok(());
    };
    let binding = Binding {
        parent_pid: gate.binding.0,
        child_pid: gate.binding.1,
        parent_token: gate.binding.2,
        child_token: gate.binding.3,
        parent_window: HWND(gate.binding.4 as *mut _),
    };
    let control = HWND(gate.control as *mut _);
    let channel = if gate.transport == TRANSPORT_EVENT {
        let channel = unsafe { Channel::open(binding)? };
        if !channel.metadata().valid(binding, control) {
            return Err("并行冻结线程前台门绑定失效".into());
        }
        Some(channel)
    } else {
        None
    };
    let (_, legacy) = mpsc::channel();
    REQUEST_TRACE.with(|trace| *trace.borrow_mut() = gate.trace);
    FOREGROUND_GATE.with(|slot| {
        *slot.borrow_mut() = Some(ForegroundGate {
            cursor_task: gate.cursor_task,
            endpoint: Rc::new(WorkerEndpoint {
                binding,
                control,
                process: gate.parent,
                channel,
                request_wake: gate.request,
                legacy,
                stopped: gate.stopped,
                transport: gate.transport,
            }),
            completed: false,
            // 准备线程只等前台门；CaptureStarted 唯一由持有 mutex 的 main 发起。
            reporter: None,
        })
    });
    Ok(())
}
pub(crate) fn preparation_error(cancelled: bool, message: String) -> Box<dyn std::error::Error> {
    if cancelled {
        Box::new(StandbyCancelled)
    } else {
        message.into()
    }
}
thread_local! {
    static FOREGROUND_GATE: RefCell<Option<ForegroundGate>> = const { RefCell::new(None) };
}
/// 仅在捕获 mutex 获取成功后调用；standby main 只操作 Rust 队列和既有内核 event。
/// 返回 false 表示直接 CLI，沿用它原来的托盘搜索/通知路径。
pub(crate) fn report_started_via_controller() -> Result<bool> {
    FOREGROUND_GATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(gate) = slot.as_mut() else {
            return Ok(false);
        };
        if gate.endpoint.cancelled()? {
            return cancelled();
        }
        let reporter = gate
            .reporter
            .as_mut()
            .ok_or("准备线程不能发送 main 截图启动通知")?;
        if reporter.reported {
            return Err("本次截图启动通知已发送".into());
        }
        reporter
            .commands
            .send(ControllerCommand::CaptureStarted)
            .map_err(|_| "待命控制线程启动通知通道已关闭")?;
        // DuplicateHandle 保持同一个 controllerWake 对象，不新建事件或按 PID 重开身份。
        unsafe {
            SetEvent(raw_handle(&reporter.wake))?;
        }
        reporter.reported = true;
        Ok(true)
    })
}
pub(crate) fn take_cursor_task() -> Result<Option<crate::cursor::CursorTaskTicket>> {
    FOREGROUND_GATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(gate) = slot.as_mut() else {
            return Ok(None);
        };
        if gate.endpoint.cancelled()? {
            return cancelled();
        }
        Ok(gate.cursor_task.take())
    })
}
pub(crate) fn ensure_standby_owner_active() -> Result<()> {
    FOREGROUND_GATE.with(|slot| {
        if let Some(gate) = slot.borrow().as_ref() {
            if gate.endpoint.cancelled()? {
                return cancelled();
            }
        }
        Ok(())
    })
}
/// NativeSetup/真实原生表面开始之前调用；direct CLI/自有 fixture 没有 standby gate。
pub(crate) fn wait_foreground_grant_before_native() -> Result<()> {
    FOREGROUND_GATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(gate) = slot.as_mut() else {
            return Ok(());
        };
        if gate.endpoint.cancelled()? {
            return cancelled();
        }
        if gate.completed {
            return Ok(());
        }
        REQUEST_TRACE.with(|trace| {
            mark_request(&mut trace.borrow_mut(), |trace| {
                &mut trace.foreground_gate_begin
            })
        });
        let state = if let Some(channel) = gate.endpoint.channel.as_ref() {
            // 请求 event 已由 main 消费；之后此 event 只作为 sticky stop 的唤醒，不由 controller 等待。
            let handles = [
                raw_handle(&gate.endpoint.process),
                channel.event,
                channel.foreground,
            ];
            let status = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            if status == WAIT_FAILED {
                return Err(windows::core::Error::from_win32().into());
            }
            if gate.endpoint.cancelled()? || status == WAIT_OBJECT_0 {
                return cancelled();
            }
            if status.0 != WAIT_OBJECT_0.0 + 2 {
                return Err("前台完成门收到未发布完成或重复的请求事件".into());
            }
            if !channel
                .metadata()
                .valid(gate.endpoint.binding, gate.endpoint.control)
            {
                return Err("前台完成门 IPC 绑定失效".into());
            }
            let state = channel.metadata().foreground_state.load(Ordering::Acquire);
            if !matches!(state, FOREGROUND_ALLOWED | FOREGROUND_DENIED) {
                return Err("前台完成事件未发布 ASFW 结果".into());
            }
            state
        } else {
            // controller 收到旧窗口请求之前，parent 已完成 ASFW；BOOL 不冒称成功。
            FOREGROUND_PENDING
        };
        gate.completed = true;
        REQUEST_TRACE.with(|trace| {
            let mut trace = trace.borrow_mut();
            if let Some(trace) = trace.as_mut() {
                trace.foreground_state = state;
                trace.foreground_gate_end = qpc();
            }
        });
        // BOOL false 不是假的授权成功；真实 NativeSurface focus/active+DWM 门仍决定 Ready。
        if gate.endpoint.cancelled()? {
            return cancelled();
        }
        Ok(())
    })
}

unsafe fn valid_parent(window: HWND, pid: u32, token: usize) -> bool {
    valid_parent_traced(window, pid, token, &mut None)
}
unsafe fn valid_parent_traced(
    window: HWND,
    pid: u32,
    token: usize,
    trace: &mut Option<RequestWaitTrace>,
) -> bool {
    mark_request(trace, |trace| &mut trace.parent_window_begin);
    let mut owner = 0;
    GetWindowThreadProcessId(window, Some(&mut owner));
    mark_request(trace, |trace| &mut trace.parent_window_end);
    mark_request(trace, |trace| &mut trace.parent_class_begin);
    let mut class = [0u16; 64];
    let length = GetClassNameW(window, &mut class).max(0) as usize;
    let expected_class = String::from_utf16_lossy(&class[..length]) == "RustShot.MVP.Window";
    mark_request(trace, |trace| &mut trace.parent_class_end);
    mark_request(trace, |trace| &mut trace.parent_props_begin);
    let current_token = GetPropW(window, w!("RustShot.Resident.Token")).0 as usize;
    let closing = !GetPropW(window, w!("RustShot.Resident.Closing")).is_invalid();
    mark_request(trace, |trace| &mut trace.parent_props_end);
    token != 0 && owner == pid && expected_class && current_token == token && !closing
}
/// main 待命阶段不创建 HWND，也不取消息；所有 control HWND 状态在专用线程外层拥有。
pub(crate) fn wait(args: &[String]) -> Result<Option<CaptureRequest>> {
    let pid = args.get(1).ok_or("缺少托盘进程")?.parse::<u32>()?;
    let parent_window = HWND(args.get(2).ok_or("缺少托盘窗口")?.parse::<usize>()? as *mut _);
    let token = args.get(3).ok_or("缺少待命标识")?.parse::<i64>()?;
    let parent_token = args.get(4).ok_or("缺少托盘实例标识")?.parse::<usize>()?;
    if token <= 0 || parent_token == 0 {
        return Err("待命参数实例标识无效".into());
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if !unsafe { valid_parent(parent_window, pid, parent_token) } {
        return Ok(None);
    }
    let process = unsafe { owned_handle(OpenProcess(PROCESS_SYNCHRONIZE, false, pid)?) };
    if parent_exited(raw_handle(&process))?
        || !unsafe { valid_parent(parent_window, pid, parent_token) }
    {
        return Ok(None);
    }
    let binding = Binding {
        parent_pid: pid,
        child_pid: std::process::id(),
        parent_token,
        child_token: token,
        parent_window,
    };
    let main_thread = unsafe { GetCurrentThreadId() };
    let (controller, initialized) = Controller::start(binding, raw_handle(&process), main_thread)?;
    // 预热仍在原 main TLS，不能移到 controller 后遗失 thread-bound GDI/黑 surface。
    let gdi_primed = match crate::fast_capture::prewarm_thread() {
        Ok(()) => true,
        Err(error) if crate::fast_capture::prewarm_requires_worker_exit(error.as_ref()) => {
            // 暖身清理未知或原预建线程 panic：本自有 worker 自然错误退出，不确认 WarmReady。
            // 原 controller RAII 仍正常 stop/join；不能把此类 hard 失败改为冷捕获。
            return Err(error);
        }
        Err(error) => {
            eprintln!("待命 GDI 预热失败，截图时重新初始化：{error}");
            false
        }
    };
    // 局部 RAII 先覆盖 Owner 建立前所有 ?/return；WarmReady 之前已确认 USER/DPI ready。
    // 一次性线程只暂时复制并销毁预定义 stock 图标，不采样当前指针/桌面/DC；载入/复制失败保留冷路径。
    let cursor_task = match crate::cursor::precreate_standby() {
        Ok(task) => Some(task),
        Err(error) if crate::cursor::stock_copy_cleanup_failed(error.as_ref()) => {
            // 副本释放尚未确认：不能继续驻留并宣称待机没有该 stock 图标像素。
            // 局部 controller 的 RAII 正常 stop/join；返回错误让本自有 worker 自然失败退出。
            return Err(error);
        }
        Err(error) => {
            eprintln!("待命光标线程预建失败，截图时重新初始化：{error}");
            None
        }
    };
    let mut warm = match crate::startup_surface::prewarm_standby() {
        Ok(warm) => Some(warm),
        Err(error) => {
            eprintln!("待命首屏预热失败，保留冷启动路径：{error}");
            None
        }
    };
    let info = initialized
        .recv()
        .map_err(|_| "待命控制线程未发布初始化结果")?
        .map_err(|error| format!("待命控制线程初始化失败：{error}"))?;
    let control = HWND(info.control as *mut _);
    let channel = if info.transport == TRANSPORT_EVENT {
        let channel = unsafe { Channel::open(binding)? };
        if !channel.metadata().valid(binding, control) {
            return Err("待命主线程打开 IPC 后绑定失效".into());
        }
        Some(channel)
    } else if info.transport == TRANSPORT_WINDOW {
        None
    } else {
        return Err("待命 controller 公布未知传输".into());
    };
    let endpoint = Rc::new(WorkerEndpoint {
        binding,
        control,
        process,
        channel,
        request_wake: info.request_wake,
        legacy: info.legacy,
        stopped: controller.stopped.clone(),
        transport: info.transport,
    });
    let mut owner = Owner {
        cursor_task,
        ocr_warmup: None,
        _endpoint: endpoint.clone(),
        controller,
    };
    owner.controller.send(ControllerCommand::WarmReady {
        identity: warm.as_ref().and_then(|warm| warm.identity),
        gdi_primed,
        prewarm_failed: warm.is_none(),
    })?;
    // 固定 CPU 缓存的后台准备不进入 WarmReady 或截图请求的等待链。
    // Owner 随唯一 CaptureRequest 保留；退出时合作取消，不 join 原生初始化。
    let preferences = crate::preferences::Preferences::load();
    if preferences.ocr_warmup != crate::ppocr::WarmupMode::Off {
        owner.ocr_warmup = crate::ppocr::start_cached_profile_warmup(
            preferences.ocr_profile,
            std::time::Duration::ZERO,
        );
    }
    let mut trace = trace_enabled().then(RequestWaitTrace::default);
    if let Some(trace) = trace.as_mut() {
        trace.transport = endpoint.transport;
    }
    if endpoint.cancelled()? {
        return no_capture(owner);
    }
    let handles = [
        raw_handle(&endpoint.process),
        raw_handle(&endpoint.request_wake),
    ];
    mark_request(&mut trace, |trace| &mut trace.wait_begin);
    let status = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
    let wait_error = (status == WAIT_FAILED).then(windows::core::Error::from_win32);
    mark_request(&mut trace, |trace| &mut trace.wait_end);
    if let Some(trace) = trace.as_mut() {
        trace.wake_kind = status.0;
        trace.wait_wake_kind = status.0;
        trace.event_return_qpc = trace.wait_end;
    }
    if let Some(error) = wait_error {
        return Err(error.into());
    }
    if endpoint.cancelled()? || status == WAIT_OBJECT_0 {
        return no_capture(owner);
    }
    if status.0 != WAIT_OBJECT_0.0 + 1 {
        return Err("待命 main 收到未知内核等待结果".into());
    }
    mark_request(&mut trace, |trace| &mut trace.decode_begin);
    let (mode, timestamp, quick) = if let Some(channel) = endpoint.channel.as_ref() {
        if !channel.metadata().valid(binding, control) {
            return Err("待命 IPC 消费时绑定失效".into());
        }
        match channel.metadata().consume_request_with_quick()? {
            Some(request) => request,
            None => return no_capture(owner),
        }
    } else {
        endpoint
            .legacy
            .try_recv()
            .map_err(|_| "兼容请求 wake 没有唯一完整标量")?
    };
    mark_request(&mut trace, |trace| &mut trace.decode_end);
    if mode > 2 || timestamp <= 0 {
        return Err("待命消费的 mode/QPC 无效".into());
    }
    crate::quick_action::QuickRecipe::unpack(quick)?;
    if endpoint.cancelled()? {
        return no_capture(owner);
    }
    owner.controller.send(ControllerCommand::Consumed)?;
    if let Some(trace) = trace.as_mut() {
        trace.request_qpc = timestamp;
        trace.request_mode = mode;
    }
    mark_request(&mut trace, |trace| &mut trace.environment_begin);
    std::env::set_var("RUSTSHOT_STARTUP_REQUEST_QPC", timestamp.to_string());
    mark_request(&mut trace, |trace| &mut trace.environment_end);
    mark_request(&mut trace, |trace| &mut trace.retain_begin);
    if let Some(warm) = warm.as_mut() {
        warm.retain_for_capture();
    }
    mark_request(&mut trace, |trace| &mut trace.retain_end);
    let reporter = CaptureStartedReporter {
        commands: owner.controller.commands.clone(),
        wake: unsafe { duplicate(raw_handle(&owner.controller.wake))? },
        reported: false,
    };
    FOREGROUND_GATE.with(|slot| {
        *slot.borrow_mut() = Some(ForegroundGate {
            cursor_task: owner
                .cursor_task
                .as_ref()
                .map(crate::cursor::StandbyCursorGuard::ticket),
            endpoint,
            completed: false,
            reporter: Some(reporter),
        })
    });
    mark_request(&mut trace, |trace| &mut trace.return_qpc);
    REQUEST_TRACE.with(|slot| *slot.borrow_mut() = trace);
    Ok(Some(CaptureRequest {
        full_screen: mode == 1,
        all_screens: mode == 2,
        quick,
        _owner: owner,
    }))
}
fn no_capture(mut owner: Owner) -> Result<Option<CaptureRequest>> {
    owner.finish()?;
    Ok(None)
}

#[cfg(test)]
mod ready_poll_tests {
    use super::*;

    fn binding() -> Binding {
        Binding {
            parent_pid: 101,
            child_pid: 202,
            parent_token: 303,
            child_token: 404,
            parent_window: HWND(505_usize as *mut _),
        }
    }

    #[test]
    fn quick_action_recipe_uses_reserved_slot_without_changing_metadata_size() {
        let metadata = RequestMetadata::new(binding(), HWND(606_usize as *mut _));
        metadata.state.store(IDLE, Ordering::Release);
        let quick = crate::quick_action::builtin_action("copy-pin")
            .unwrap()
            .recipe
            .pack()
            .unwrap();
        metadata.publish_request_with_quick(0, 901, quick).unwrap();
        assert_eq!(size_of::<RequestMetadata>(), 80);
        assert_eq!(
            metadata.consume_request_with_quick().unwrap(),
            Some((0, 901, quick))
        );
        assert!(metadata.consume_request_with_quick().is_err());
    }

    #[test]
    fn malformed_quick_action_does_not_publish_and_consumer_rechecks_payload() {
        let metadata = RequestMetadata::new(binding(), HWND(606_usize as *mut _));
        metadata.state.store(IDLE, Ordering::Release);
        assert!(metadata
            .publish_request_with_quick(0, 901, 0xa500_0090)
            .is_err());
        assert_eq!(metadata.state.load(Ordering::Acquire), IDLE);
        metadata.publish_request(0, 901).unwrap();
        metadata.quick.store(0xa500_0090, Ordering::Relaxed);
        assert!(metadata.consume_request_with_quick().is_err());
    }

    #[test]
    fn absent_channel_and_unpublished_or_preparing_metadata_wait_without_window_probe() {
        let missing = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
            ERROR_FILE_NOT_FOUND.0,
        ));
        assert!(missing_ready_channel(&missing));
        let denied = windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
            ERROR_ACCESS_DENIED.0,
        ));
        assert!(!missing_ready_channel(&denied));
        let metadata = RequestMetadata::new(binding(), HWND(606_usize as *mut _));
        for state in [0, PREPARING] {
            metadata.state.store(state, Ordering::Release);
            assert!(poll_ready_control(&metadata, binding(), |_| {
                panic!("未发布的控制窗不能查询或销毁");
            })
            .is_none());
            assert_eq!(metadata.state.load(Ordering::Acquire), state);
            assert_eq!(metadata.stop.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn ready_zero_then_lost_notification_recovers_same_control_and_original_request_once() {
        let control = HWND(606_usize as *mut _);
        let metadata = RequestMetadata::new(binding(), control);
        metadata.state.store(IDLE, Ordering::Release);
        assert!(poll_ready_control(&metadata, binding(), |_| false).is_none());
        assert_eq!(metadata.state.load(Ordering::Acquire), IDLE);
        assert_eq!(metadata.request_qpc.load(Ordering::Acquire), 0);
        let recovered = poll_ready_control(&metadata, binding(), |window| window == control);
        assert_eq!(recovered, Some(control));
        // 绑定本身不发布截图；原派发随后才写入同一 mode/QPC。
        assert_eq!(metadata.state.load(Ordering::Acquire), IDLE);
        let original_qpc = (1_i64 << 50) + 917;
        metadata.publish_request(2, original_qpc).unwrap();
        assert_eq!(metadata.consume_request().unwrap(), Some((2, original_qpc)));
        assert!(metadata.consume_request().is_err());
        assert!(poll_ready_control(&metadata, binding(), |_| {
            panic!("已消费的工作进程不能再次当作待命绑定");
        })
        .is_none());
    }

    #[test]
    fn old_binding_or_protocol_never_reaches_native_window_probe() {
        let original = binding();
        let control = HWND(606_usize as *mut _);
        let metadata = RequestMetadata::new(original, control);
        metadata.state.store(IDLE, Ordering::Release);
        for other in [
            Binding {
                child_pid: 203,
                ..original
            },
            Binding {
                parent_pid: 102,
                ..original
            },
            Binding {
                child_token: 405,
                ..original
            },
            Binding {
                parent_token: 304,
                ..original
            },
            Binding {
                parent_window: HWND(506_usize as *mut _),
                ..original
            },
        ] {
            assert!(poll_ready_control(&metadata, other, |_| {
                panic!("旧身份不得查询其它 HWND");
            })
            .is_none());
        }
        metadata.version.store(IPC_VERSION + 1, Ordering::Relaxed);
        assert!(poll_ready_control(&metadata, original, |_| panic!("协议不符")).is_none());
        metadata.version.store(IPC_VERSION, Ordering::Relaxed);
        // 元数据身份通过仍须实际窗口身份通过，句柄复用不能被模型默认放行。
        assert!(poll_ready_control(&metadata, original, |_| false).is_none());
        metadata.control_window.store(0, Ordering::Relaxed);
        assert!(poll_ready_control(&metadata, original, |_| panic!("零 HWND")).is_none());
    }

    #[test]
    fn stopped_non_idle_or_foreground_completed_slots_are_never_rearmed() {
        let metadata = RequestMetadata::new(binding(), HWND(606_usize as *mut _));
        for state in [PREPARING, WRITING, REQUESTED, CONSUMED, CLOSED] {
            metadata.state.store(state, Ordering::Release);
            assert!(poll_ready_control(&metadata, binding(), |_| panic!("非闲置状态")).is_none());
            assert_eq!(metadata.state.load(Ordering::Acquire), state);
        }
        metadata.state.store(IDLE, Ordering::Release);
        metadata.stop.store(1, Ordering::Release);
        assert!(poll_ready_control(&metadata, binding(), |_| panic!("停止状态")).is_none());
        assert_eq!(metadata.stop.load(Ordering::Acquire), 1);
        metadata.stop.store(0, Ordering::Release);
        for foreground in [FOREGROUND_ALLOWED, FOREGROUND_DENIED] {
            metadata
                .foreground_state
                .store(foreground, Ordering::Release);
            assert!(poll_ready_control(&metadata, binding(), |_| panic!("已完成前台门")).is_none());
            assert_eq!(
                metadata.foreground_state.load(Ordering::Acquire),
                foreground
            );
        }
    }

    #[test]
    fn missing_mapping_only_accepts_explicit_window_transport_of_same_ready_instance() {
        let ready = ReadyPublication {
            pid: 202,
            transport: TRANSPORT_WINDOW,
            version: IPC_VERSION as usize,
            bytes: size_of::<RequestMetadata>(),
            ready: true,
        };
        assert!(explicit_window_fallback(202, true, ready));
        // same_instance 来自原 HANDLE/class/parent HWND/tokens，旧 token 必须失败。
        assert!(!explicit_window_fallback(202, false, ready));
        for published in [
            ReadyPublication { pid: 203, ..ready },
            ReadyPublication {
                transport: TRANSPORT_EVENT,
                ..ready
            },
            ReadyPublication {
                transport: 0,
                ..ready
            },
            ReadyPublication {
                version: ready.version + 1,
                ..ready
            },
            ReadyPublication {
                bytes: ready.bytes + 8,
                ..ready
            },
            ReadyPublication {
                ready: false,
                ..ready
            },
        ] {
            assert!(!explicit_window_fallback(202, true, published));
        }
    }
}

#[cfg(test)]
mod ipc_metadata_tests {
    use super::*;

    fn binding() -> Binding {
        Binding {
            parent_pid: 101,
            child_pid: 202,
            parent_token: 303,
            child_token: 404,
            parent_window: HWND(505_usize as *mut _),
        }
    }

    #[test]
    fn metadata_requires_published_exact_binding_and_version() {
        let binding = binding();
        let control = HWND(606_usize as *mut _);
        let metadata = RequestMetadata::new(binding, control);
        assert!(!metadata.valid(binding, control));
        metadata.state.store(PREPARING, Ordering::Release);
        assert!(metadata.valid(binding, control));
        for other in [
            Binding {
                child_pid: 203,
                ..binding
            },
            Binding {
                parent_pid: 102,
                ..binding
            },
            Binding {
                child_token: 405,
                ..binding
            },
            Binding {
                parent_token: 304,
                ..binding
            },
            Binding {
                parent_window: HWND(506_usize as *mut _),
                ..binding
            },
        ] {
            assert!(!metadata.valid(other, control));
        }
        assert!(!metadata.valid(binding, HWND(607_usize as *mut _)));
        metadata.version.store(IPC_VERSION + 1, Ordering::Relaxed);
        assert!(!metadata.valid(binding, control));
        metadata.version.store(IPC_VERSION, Ordering::Relaxed);
        metadata
            .foreground_state
            .store(FOREGROUND_DENIED + 1, Ordering::Relaxed);
        assert!(!metadata.valid(binding, control));
        metadata
            .foreground_state
            .store(FOREGROUND_PENDING, Ordering::Relaxed);
        metadata
            .bytes
            .store(size_of::<RequestMetadata>() as u32 + 8, Ordering::Relaxed);
        assert!(!metadata.valid(binding, control));
    }

    #[test]
    fn one_shot_metadata_keeps_each_mode_and_original_qpc() {
        for mode in 0..=2 {
            let metadata = RequestMetadata::new(binding(), HWND(606_usize as *mut _));
            metadata.state.store(IDLE, Ordering::Release);
            assert!(metadata.publish_request(3, 900).is_err());
            assert!(metadata.publish_request(mode, 0).is_err());
            let timestamp = (1_i64 << 50) + mode as i64;
            metadata.publish_request(mode, timestamp).unwrap();
            assert!(metadata.publish_request(mode, timestamp + 1).is_err());
            assert_eq!(metadata.consume_request().unwrap(), Some((mode, timestamp)));
            assert!(metadata.consume_request().is_err());
        }
    }

    #[test]
    fn sticky_stop_wins_before_ready_and_after_publication() {
        assert!(!classify_parent_wait(WAIT_TIMEOUT).unwrap());
        assert!(classify_parent_wait(WAIT_OBJECT_0).unwrap());
        unsafe {
            SetLastError(ERROR_INVALID_HANDLE);
        }
        let error = classify_parent_wait(WAIT_FAILED).unwrap_err();
        assert!(!is_cancelled(error.as_ref()));
        assert!(error.downcast_ref::<windows::core::Error>().is_some());
        assert!(classify_parent_wait(WAIT_EVENT(WAIT_OBJECT_0.0 + 7)).is_err());
        for publish in [false, true] {
            let metadata = RequestMetadata::new(binding(), HWND(606_usize as *mut _));
            metadata
                .state
                .store(if publish { IDLE } else { PREPARING }, Ordering::Release);
            if publish {
                metadata.publish_request(0, 900).unwrap();
            }
            metadata.stop.store(1, Ordering::Release);
            assert_eq!(metadata.consume_request().unwrap(), None);
            assert!(metadata.publish_request(0, 901).is_err());
            assert_eq!(metadata.stop.load(Ordering::Acquire), 1);
        }
    }

    #[test]
    fn owned_channel_two_views_roundtrip_collision_and_stop() {
        // 自有 Windows 内核对象的同进程两 view 集成测试，不是跨进程、窗口或桌面测试。
        let token = qpc();
        assert!(token > 0);
        let binding = Binding {
            parent_pid: std::process::id(),
            child_pid: std::process::id(),
            parent_token: token as usize,
            child_token: token.checked_add(1).unwrap(),
            parent_window: HWND(11_usize as *mut _),
        };
        let control = HWND(22_usize as *mut _);
        unsafe {
            let creator = Channel::create(binding, control).unwrap();
            assert!(creator.metadata().valid(binding, control));
            creator.metadata().state.store(IDLE, Ordering::Release);
            let opened = Channel::open(binding).unwrap();
            assert_ne!(creator.view.Value, opened.view.Value);
            assert!(opened.metadata().valid(binding, control));
            assert!(!opened.metadata().valid(
                Binding {
                    child_token: binding.child_token + 1,
                    ..binding
                },
                control
            ));
            let snapshot = || {
                let metadata = opened.metadata();
                [
                    metadata.magic.load(Ordering::Relaxed) as u64,
                    metadata.version.load(Ordering::Relaxed) as u64,
                    metadata.bytes.load(Ordering::Relaxed) as u64,
                    metadata.parent_pid.load(Ordering::Relaxed) as u64,
                    metadata.child_pid.load(Ordering::Relaxed) as u64,
                    metadata.quick.load(Ordering::Relaxed) as u64,
                    metadata.parent_token.load(Ordering::Relaxed),
                    metadata.child_token.load(Ordering::Relaxed),
                    metadata.parent_window.load(Ordering::Relaxed),
                    metadata.control_window.load(Ordering::Relaxed),
                    metadata.state.load(Ordering::Acquire) as u64,
                    metadata.stop.load(Ordering::Acquire) as u64,
                    metadata.mode.load(Ordering::Relaxed) as u64,
                    metadata.foreground_state.load(Ordering::Acquire) as u64,
                    metadata.request_qpc.load(Ordering::Relaxed) as u64,
                ]
            };
            let before_collision = snapshot();
            assert!(Channel::create(binding, control).is_err());
            assert_eq!(snapshot(), before_collision);

            let timestamp = token.checked_add(2).unwrap();
            assert_eq!(WaitForSingleObject(creator.foreground, 0), WAIT_TIMEOUT);
            opened.metadata().publish_request(2, timestamp).unwrap();
            SetEvent(opened.event).unwrap();
            assert_eq!(WaitForSingleObject(creator.event, 1000), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(creator.event, 0), WAIT_TIMEOUT);
            assert_eq!(
                creator.metadata().consume_request().unwrap(),
                Some((2, timestamp))
            );
            assert!(opened.metadata().publish_request(1, timestamp + 1).is_err());
            assert!(creator.metadata().consume_request().is_err());
            opened
                .metadata()
                .foreground_state
                .store(FOREGROUND_ALLOWED, Ordering::Release);
            SetEvent(opened.foreground).unwrap();
            assert_eq!(WaitForSingleObject(creator.foreground, 0), WAIT_OBJECT_0);
            assert_eq!(WaitForSingleObject(creator.foreground, 0), WAIT_OBJECT_0);
            assert_eq!(
                creator.metadata().foreground_state.load(Ordering::Acquire),
                FOREGROUND_ALLOWED
            );
            // 重新清掉测试的完成信号，让两个 gate 实际验证同一次 stop 的唤醒。
            ResetEvent(opened.foreground).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let waiters = (0..2)
                .map(|_| {
                    let request = duplicate(creator.event).unwrap();
                    let foreground = duplicate(creator.foreground).unwrap();
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        WaitForMultipleObjects(
                            &[raw_handle(&request), raw_handle(&foreground)],
                            false,
                            1000,
                        )
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            opened.stop().unwrap();
            for waiter in waiters {
                let status = waiter.join().unwrap();
                assert!(status == WAIT_OBJECT_0 || status.0 == WAIT_OBJECT_0.0 + 1);
            }
            assert_eq!(WaitForSingleObject(creator.foreground, 0), WAIT_OBJECT_0);
            assert_eq!(creator.metadata().consume_request().unwrap(), None);
            assert_eq!(opened.metadata().stop.load(Ordering::Acquire), 1);
        }
    }
}
