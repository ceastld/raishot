//! 独立启动专项：不标注、导出或操作剪贴板；第二帧后等待 DWM，再走原生取消。
use crate::{capture_view::CaptureView, gpui_capture::hwnd, Result};
use gpui::{App, Entity, Window};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        System::Threading::{
            GetCurrentThread, GetCurrentThreadId, GetThreadTimes, OpenMutexW,
            SYNCHRONIZATION_ACCESS_RIGHTS,
        },
        UI::{HiDpi::GetDpiForWindow, Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::*},
    },
};

// 使用已有系统库，避免仅为专项增加 windows crate feature 或运行时依赖。
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
    fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> i32;
    // 公开 WinSDK API 直接返回 DWORD 错误码，0 成功，不读 GetLastError。
    fn QueryThreadProfiling(thread: HANDLE, enabled: *mut u8) -> u32;
    fn EnableThreadProfiling(
        thread: HANDLE,
        flags: u32,
        hardware: u64,
        profile: *mut HANDLE,
    ) -> u32;
    fn ReadThreadProfilingData(
        profile: HANDLE,
        flags: u32,
        data: *mut ThreadPerformanceData,
    ) -> u32;
    fn DisableThreadProfiling(profile: HANDLE) -> u32;
    fn GetStdHandle(kind: u32) -> *mut std::ffi::c_void;
    fn GetConsoleMode(output: *mut std::ffi::c_void, mode: *mut u32) -> i32;
    fn WriteConsoleW(
        output: *mut std::ffi::c_void,
        buffer: *const u16,
        count: u32,
        written: *mut u32,
        reserved: *mut std::ffi::c_void,
    ) -> i32;
    fn WriteFile(
        output: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        count: u32,
        written: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}

// 调用者仅在 Trace=1 时使用；当前线程 pseudo HANDLE 借用、不关闭，也不跨线程采样。
// CPU 累计计数两侧包住真实调用，包络含观测开销；cycles 不能换算为毫秒。
#[derive(Clone, serde::Serialize)]
pub(crate) struct ThreadCpuSample {
    sample_begin_qpc: Option<i64>,
    sample_end_qpc: Option<i64>,
    thread_id: u32,
    creation_file_time: Option<u64>,
    kernel_100ns: Option<u64>,
    user_100ns: Option<u64>,
    cycles: Option<u64>,
    times_error_hresult: Option<i32>,
    cycles_error_win32: Option<u32>,
}
fn trace_qpc() -> Option<i64> {
    let mut value = 0;
    (unsafe { QueryPerformanceCounter(&mut value) } != 0).then_some(value)
}
fn file_time_value(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}
fn sample_thread_cpu() -> ThreadCpuSample {
    let sample_begin_qpc = trace_qpc();
    let thread_id = unsafe { GetCurrentThreadId() };
    let thread = unsafe { GetCurrentThread() };
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let times = unsafe { GetThreadTimes(thread, &mut creation, &mut exit, &mut kernel, &mut user) };
    let times_error_hresult = times.as_ref().err().map(|error| error.code().0);
    let mut cycles = 0;
    let cycles_ok = unsafe { QueryThreadCycleTime(thread, &mut cycles) } != 0;
    let cycles_error_win32 = (!cycles_ok).then(|| unsafe { GetLastError() }.0);
    ThreadCpuSample {
        sample_begin_qpc,
        sample_end_qpc: trace_qpc(),
        thread_id,
        creation_file_time: times.is_ok().then(|| file_time_value(creation)),
        kernel_100ns: times.is_ok().then(|| file_time_value(kernel)),
        user_100ns: times.is_ok().then(|| file_time_value(user)),
        cycles: cycles_ok.then_some(cycles),
        times_error_hresult,
        cycles_error_win32,
    }
}
#[derive(Clone, serde::Serialize)]
pub(crate) struct ThreadCpuEnvelope {
    before: ThreadCpuSample,
    api_begin_qpc: Option<i64>,
    api_end_qpc: Option<i64>,
    after: Option<ThreadCpuSample>,
    profiling: ThreadProfileEnvelope,
}

const THREAD_PROFILE_DISPATCH: u32 = 1;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ThreadProfileCounter {
    counter_type: i32,
    reserved: u32,
    value: u64,
}
#[repr(C)]
#[derive(Default)]
struct ThreadPerformanceData {
    size: u16,
    version: u8,
    hardware_counters_count: u8,
    context_switch_count: u32,
    wait_reason_bitmap: u64,
    cycle_time: u64,
    retry_count: u32,
    reserved: u32,
    hardware_counters: [ThreadProfileCounter; 16],
}
// 公开 PERFORMANCE_DATA Version1 / COUNTER_DATA[16]，只使用既有 kernel32。
const _: [(); 16] = [(); std::mem::size_of::<ThreadProfileCounter>()];
const _: [(); 288] = [(); std::mem::size_of::<ThreadPerformanceData>()];
const _: [(); 32] = [(); std::mem::offset_of!(ThreadPerformanceData, hardware_counters)];

#[derive(Clone, serde::Serialize)]
struct ThreadProfileSample {
    sample_begin_qpc: Option<i64>,
    sample_end_qpc: Option<i64>,
    read_error_win32: u32,
    valid: bool,
    size: u16,
    version: u8,
    hardware_counters_count: u8,
    context_switch_count_raw: Option<u32>,
    wait_reason_bitmap_raw: Option<u64>,
    cycle_time_raw: Option<u64>,
    retry_count_raw: Option<u32>,
}

#[derive(Clone, serde::Serialize)]
struct ThreadProfileEnvelope {
    enabling_thread_id: u32,
    performance_data_bytes: u16,
    enable_flags: u32,
    read_flags: u32,
    hardware_counters: u64,
    query_before_begin_qpc: Option<i64>,
    query_before_end_qpc: Option<i64>,
    query_before_error_win32: u32,
    preexisting_enabled_raw: Option<u8>,
    enable_begin_qpc: Option<i64>,
    enable_end_qpc: Option<i64>,
    enable_error_win32: Option<u32>,
    profiling_owned: bool,
    enable_returned_null: bool,
    before: Option<ThreadProfileSample>,
    after: Option<ThreadProfileSample>,
    disable_begin_qpc: Option<i64>,
    disable_end_qpc: Option<i64>,
    disable_error_win32: Option<u32>,
    disabled_on_enabling_thread: bool,
    query_after_begin_qpc: Option<i64>,
    query_after_end_qpc: Option<i64>,
    query_after_error_win32: Option<u32>,
    enabled_after_disable_raw: Option<u8>,
    drop_cleanup_may_retry_after_disable_error: bool,
    valid: bool,
    scope: &'static str,
}

// HANDLE 的原始指针阻止 Send/Sync；不 Clone、不序列化、不放入跨线程 ticket。
// profile HANDLE 只有 DisableThreadProfiling 这一回收入口，不是 CloseHandle。
struct ThreadProfileGuard {
    handle: HANDLE,
    thread_id: u32,
    owned: bool,
}
impl ThreadProfileGuard {
    fn read(&self) -> ThreadProfileSample {
        let mut data = ThreadPerformanceData {
            size: std::mem::size_of::<ThreadPerformanceData>() as u16,
            version: 1,
            ..Default::default()
        };
        let sample_begin_qpc = trace_qpc();
        let error =
            unsafe { ReadThreadProfilingData(self.handle, THREAD_PROFILE_DISPATCH, &mut data) };
        let sample_end_qpc = trace_qpc();
        ThreadProfileSample {
            sample_begin_qpc,
            sample_end_qpc,
            read_error_win32: error,
            valid: error == 0
                && data.size == 288
                && data.version == 1
                && data.hardware_counters_count == 0
                && sample_begin_qpc
                    .zip(sample_end_qpc)
                    .is_some_and(|(begin, end)| begin > 0 && end >= begin),
            size: data.size,
            version: data.version,
            hardware_counters_count: data.hardware_counters_count,
            context_switch_count_raw: (error == 0).then_some(data.context_switch_count),
            wait_reason_bitmap_raw: (error == 0).then_some(data.wait_reason_bitmap),
            cycle_time_raw: (error == 0).then_some(data.cycle_time),
            retry_count_raw: (error == 0).then_some(data.retry_count),
        }
    }
    fn disable(&mut self) -> u32 {
        let error = unsafe { DisableThreadProfiling(self.handle) };
        if error == 0 {
            self.owned = false;
        }
        error
    }
}
impl Drop for ThreadProfileGuard {
    fn drop(&mut self) {
        if self.owned && unsafe { GetCurrentThreadId() } == self.thread_id {
            // 早退/失败只尽力关闭本 guard 成功启用的 profile，不碰 preexisting。
            let _ = self.disable();
        }
    }
}

/// 调用者只在 Trace==1 创建；本线程局部 guard，finish 后只留下可跨线程标量。
pub(crate) struct ThreadCpuProbe {
    envelope: ThreadCpuEnvelope,
    profile: Option<ThreadProfileGuard>,
}
impl ThreadCpuProbe {
    pub(crate) fn begin() -> Self {
        let thread_id = unsafe { GetCurrentThreadId() };
        let mut enabled = 0_u8;
        let query_before_begin_qpc = trace_qpc();
        let query_error = unsafe { QueryThreadProfiling(GetCurrentThread(), &mut enabled) };
        let query_before_end_qpc = trace_qpc();
        let mut profiling = ThreadProfileEnvelope {
            enabling_thread_id: thread_id,
            performance_data_bytes: 288,
            enable_flags: THREAD_PROFILE_DISPATCH,
            read_flags: THREAD_PROFILE_DISPATCH,
            hardware_counters: 0,
            query_before_begin_qpc,
            query_before_end_qpc,
            query_before_error_win32: query_error,
            preexisting_enabled_raw: (query_error == 0).then_some(enabled),
            enable_begin_qpc: None,
            enable_end_qpc: None,
            enable_error_win32: None,
            profiling_owned: false,
            enable_returned_null: false,
            before: None,
            after: None,
            disable_begin_qpc: None,
            disable_end_qpc: None,
            disable_error_win32: None,
            disabled_on_enabling_thread: false,
            query_after_begin_qpc: None,
            query_after_end_qpc: None,
            query_after_error_win32: None,
            enabled_after_disable_raw: None,
            drop_cleanup_may_retry_after_disable_error: false,
            valid: false,
            scope: "Trace-only own thread dispatch raw samples; CPU span includes before-profile read; profile span includes after-CPU read. Bitmap is not subtracted; cycles are not ms; no precise wait duration or causal claim. Query-after describes explicit disable before optional failure Drop retry.",
        };
        let mut profile = None;
        // 查询失败或已启用都不接管，不关闭未知所有者的 profiling。
        if query_error == 0 && enabled == 0 {
            let mut handle = HANDLE::default();
            profiling.enable_begin_qpc = trace_qpc();
            let error = unsafe {
                EnableThreadProfiling(GetCurrentThread(), THREAD_PROFILE_DISPATCH, 0, &mut handle)
            };
            profiling.enable_end_qpc = trace_qpc();
            profiling.enable_error_win32 = Some(error);
            profiling.enable_returned_null = error == 0 && handle.0.is_null();
            if error == 0 && !handle.0.is_null() {
                profiling.profiling_owned = true;
                profile = Some(ThreadProfileGuard {
                    handle,
                    thread_id,
                    owned: true,
                });
            }
        }
        let before = sample_thread_cpu();
        profiling.before = profile.as_ref().map(ThreadProfileGuard::read);
        Self {
            envelope: ThreadCpuEnvelope {
                before,
                api_begin_qpc: None,
                api_end_qpc: None,
                after: None,
                profiling,
            },
            profile,
        }
    }
    pub(crate) fn mark_begin(&mut self, qpc: Option<i64>) {
        self.envelope.api_begin_qpc = qpc;
    }
    pub(crate) fn finish(mut self, qpc: Option<i64>) -> ThreadCpuEnvelope {
        self.envelope.api_end_qpc = qpc;
        self.envelope.after = Some(sample_thread_cpu());
        if let Some(mut profile) = self.profile.take() {
            let profiling = &mut self.envelope.profiling;
            profiling.after = Some(profile.read());
            // Probe/guard 非 Send；仍现场记录同线程门，拒把别线程查询冒充本次。
            profiling.disabled_on_enabling_thread =
                unsafe { GetCurrentThreadId() } == profile.thread_id;
            if profiling.disabled_on_enabling_thread {
                profiling.disable_begin_qpc = trace_qpc();
                let error = profile.disable();
                profiling.disable_end_qpc = trace_qpc();
                profiling.disable_error_win32 = Some(error);
                profiling.drop_cleanup_may_retry_after_disable_error = error != 0;
                let mut enabled = 0_u8;
                profiling.query_after_begin_qpc = trace_qpc();
                let query_error = unsafe { QueryThreadProfiling(GetCurrentThread(), &mut enabled) };
                profiling.query_after_end_qpc = trace_qpc();
                profiling.query_after_error_win32 = Some(query_error);
                profiling.enabled_after_disable_raw = (query_error == 0).then_some(enabled);
            }
            // 全阶段必须有同一 QPC 时钟且依次推进；不把缺失/跨阶段倒序标为有效。
            let clocks_valid = [
                profiling.query_before_begin_qpc,
                profiling.query_before_end_qpc,
                profiling.enable_begin_qpc,
                profiling.enable_end_qpc,
                self.envelope.before.sample_begin_qpc,
                self.envelope.before.sample_end_qpc,
                profiling
                    .before
                    .as_ref()
                    .and_then(|sample| sample.sample_begin_qpc),
                profiling
                    .before
                    .as_ref()
                    .and_then(|sample| sample.sample_end_qpc),
                self.envelope.api_begin_qpc,
                self.envelope.api_end_qpc,
                self.envelope
                    .after
                    .as_ref()
                    .and_then(|sample| sample.sample_begin_qpc),
                self.envelope
                    .after
                    .as_ref()
                    .and_then(|sample| sample.sample_end_qpc),
                profiling
                    .after
                    .as_ref()
                    .and_then(|sample| sample.sample_begin_qpc),
                profiling
                    .after
                    .as_ref()
                    .and_then(|sample| sample.sample_end_qpc),
                profiling.disable_begin_qpc,
                profiling.disable_end_qpc,
                profiling.query_after_begin_qpc,
                profiling.query_after_end_qpc,
            ]
            .into_iter()
            .try_fold(0_i64, |previous, value| {
                value.filter(|qpc| *qpc > 0 && *qpc >= previous)
            })
            .is_some();
            profiling.valid = clocks_valid
                && profiling.before.as_ref().is_some_and(|sample| sample.valid)
                && profiling.after.as_ref().is_some_and(|sample| sample.valid)
                && profiling.disabled_on_enabling_thread
                && profiling.disable_error_win32 == Some(0)
                && profiling.query_after_error_win32 == Some(0)
                && profiling.enabled_after_disable_raw == Some(0)
                && self.envelope.before.thread_id == profile.thread_id
                && self.envelope.after.as_ref().is_some_and(|sample| {
                    sample.thread_id == profile.thread_id
                        && sample.creation_file_time.is_some()
                        && sample.creation_file_time == self.envelope.before.creation_file_time
                });
            // 失败仍保留原 DWORD，guard Drop 可再尽力一次；有效性不会因此改成成功。
        }
        self.envelope
    }
}

/// 完整格式化后只作一次原生输出；调用点自行保留原有 Trace 条件和发生时序。
/// 同步、共享同一 file object 的继承磁盘 HANDLE 才是本项目诊断验证的连续记录路径。
/// 不承诺管道/控制台跨进程原子性或磁盘事务/持久化；失败及短写不重试残片。
pub(crate) fn emit_trace_value(value: &serde_json::Value) {
    let mut record = serde_json::to_vec(value).expect("启动 Trace JSON 序列化失败");
    record.push(b'\n');
    write_trace_record(&record).expect("启动 Trace 完整记录输出失败");
}

pub(crate) fn emit_trace_args(args: std::fmt::Arguments<'_>) {
    let mut record = std::fmt::format(args);
    record.push('\n');
    write_trace_record(record.as_bytes()).expect("启动 Trace 完整记录输出失败");
}

fn write_trace_record(record: &[u8]) -> std::io::Result<()> {
    // GetStdHandle 返回借用的本进程标准 HANDLE；本函数不关闭、替换或 flush 它。
    let output = unsafe { GetStdHandle((-11_i32) as u32) };
    if output as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if output.is_null() {
        // Windows detached GUI 的 stdio 原语义是无接收端时成功但不输出。
        // NULL 不代表记录已写入；真实 FILE 诊断仍必须验证日志存在及完整记录。
        return Ok(());
    }
    let mut mode = 0_u32;
    if unsafe { GetConsoleMode(output, &mut mode) } != 0 {
        // Console 继续使用 Unicode；不把 UTF-8 字节按当前控制台代码页解释。
        let text = std::str::from_utf8(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let count = u32::try_from(wide.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "启动 Trace 记录过长")
        })?;
        let mut written = 0_u32;
        if unsafe {
            WriteConsoleW(
                output,
                wide.as_ptr(),
                count,
                &mut written,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if written != count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("启动 Trace 控制台短写：{written}/{count} UTF-16 单元"),
            ));
        }
    } else {
        let count = u32::try_from(record.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "启动 Trace 记录过长")
        })?;
        let mut written = 0_u32;
        // 与既有 stdout 一样要求同步 HANDLE；NULL OVERLAPPED 下只调用一次 WriteFile。
        if unsafe {
            WriteFile(
                output,
                record.as_ptr().cast(),
                count,
                &mut written,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if written != count {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("启动 Trace 文件/管道短写：{written}/{count} 字节"),
            ));
        }
    }
    Ok(())
}

/// 与启动脚本使用同一系统计时器，把创建进程与 DLL 加载也计入首帧耗时。
pub(crate) fn request_elapsed_ms() -> Option<f64> {
    let request = std::env::var("RUSTSHOT_STARTUP_REQUEST_QPC")
        .ok()?
        .parse::<i64>()
        .ok()?;
    let (mut now, mut frequency) = (0_i64, 0_i64);
    if unsafe { QueryPerformanceCounter(&mut now) } == 0
        || unsafe { QueryPerformanceFrequency(&mut frequency) } == 0
        || frequency <= 0
        || now < request
    {
        return None;
    }
    Some((now - request) as f64 * 1000. / frequency as f64)
}

/// 只在显式专项中记录；把进入 main 前的进程/DLL 加载与 UI 初始化分开。
pub(crate) fn trace_worker_entry() {
    if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
        let elapsed =
            request_elapsed_ms().map_or_else(|| "null".to_owned(), |ms| format!("{ms:.3}"));
        crate::startup_probe::emit_trace_args(format_args!(
            "{{\"phase\":\"startup_entry\",\"pid\":{},\"request_to_entry_ms\":{elapsed}}}",
            std::process::id()
        ));
    }
}
static READY: AtomicBool = AtomicBool::new(false);
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

fn close_on_error(native: HWND, error: impl ToString, errors: &RefCell<Option<String>>) {
    *errors.borrow_mut() = Some(error.to_string());
    unsafe {
        let _ = PostMessageW(Some(native), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    started: Instant,
    errors: Rc<RefCell<Option<String>>>,
) {
    READY.store(false, Ordering::Relaxed);
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    let weak = view.downgrade();
    window.on_next_frame(move |window, _| {
        let first_callback_ms = started.elapsed().as_secs_f64() * 1000.;
        let native = hwnd(window);
        if let Err(error) = crate::gpui_smoke::isolate_input(native) {
            close_on_error(native, error, &errors);
            return;
        }
        // 第一次回调可能在 GPUI draw/present 前执行；明确等待下一次绘制机会。
        window.on_next_frame(move |window, cx| {
            let before_flush_ms = started.elapsed().as_secs_f64() * 1000.;
            let native = hwnd(window);
            let result: Result<()> = (|| {
                let status = unsafe { DwmFlush() };
                if status < 0 {
                    return Err(format!("DwmFlush 失败：0x{:08x}", status as u32).into());
                }
                let ready_ms = started.elapsed().as_secs_f64() * 1000.;
                let request_ms = request_elapsed_ms().map_or_else(|| "null".to_owned(), |ms| format!("{ms:.3}"));
                let (screens, bounds, capture_ms) = weak
                    .update(cx, |v, _| -> Result<_> {
                        if v.source.is_image()
                            || v.previews.len() != v.source.screen_count()
                            || v.editor.crop.is_some()
                            || !v.editor.document.items().is_empty()
                            || v.editor.is_dragging()
                        {
                            return Err("启动专项未处于干净的桌面框选会话".into());
                        }
                        Ok((
                            v.source.screen_count(),
                            v.source.bounds,
                            v.source.capture_ms,
                        ))
                    })
                    .map_err(|error| error.to_string())??;
                let (width, height) = (bounds.width, bounds.height);
                let scale = window.scale_factor();
                let viewport = window.viewport_size();
                let native_scale = unsafe { GetDpiForWindow(native) } as f32 / 96.;
                if (f32::from(viewport.width) * scale - width as f32).abs() > 1.
                    || (f32::from(viewport.height) * scale - height as f32).abs() > 1.
                    || (scale - native_scale).abs() > 0.001 {
                    return Err("首帧 GPUI 视口/DPI 缓存与原生全屏尺寸不一致".into());
                }
                let (mut outer, mut client) = (RECT::default(), RECT::default());
                unsafe {
                    GetWindowRect(native, &mut outer)?;
                    GetClientRect(native, &mut client)?;
                }
                if outer.left != bounds.left || outer.top != bounds.top
                    || outer.right - outer.left != width as i32
                    || outer.bottom - outer.top != height as i32
                    || client.right - client.left != width as i32
                    || client.bottom - client.top != height as i32 {
                    return Err("首帧窗口未精确覆盖虚拟桌面物理坐标".into());
                }
                let mut owner = 0;
                unsafe { GetWindowThreadProcessId(native, Some(&mut owner)) };
                if owner != std::process::id() {
                    return Err("启动专项窗口不属于当前工作进程".into());
                }
                if unsafe { GetForegroundWindow() } != native || unsafe { GetFocus() } != native {
                    return Err("截图已呈现，但尚未获得原生前台/键盘焦点".into());
                }
                let (working_set_mib, private_mib) = crate::capture::memory_mib();
                println!(
                    "{{\"phase\":\"startup_ready\",\"pid\":{owner},\"screens\":{screens},\"width\":{width},\"height\":{height},\"scale\":{:.3},\"capture_ms\":{capture_ms:.3},\"first_callback_ms\":{first_callback_ms:.3},\"before_dwm_flush_ms\":{before_flush_ms:.3},\"ready_ms\":{ready_ms:.3},\"request_to_ready_ms\":{request_ms},\"dwm_flush_ms\":{:.3},\"working_set_mib\":{working_set_mib:.2},\"private_mib\":{private_mib:.2},\"present_method\":\"second_frame_dwm_flush\",\"ok\":true}}",
                    window.scale_factor(), ready_ms - before_flush_ms
                );
                READY.store(true, Ordering::Relaxed);
                // 原生按键经过 GPUI 焦点/快捷键路径，ESC 在未绘制会话中正常取消。
                unsafe {
                    PostMessageW(Some(native), WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
                    PostMessageW(Some(native), WM_KEYUP, WPARAM(0x1b), LPARAM(0))?;
                }
                CANCEL_REQUESTED.store(true, Ordering::Relaxed);
                println!(
                    "{{\"phase\":\"startup_input_cancel\",\"method\":\"owned_escape\",\"requested_ms\":{:.3},\"ok\":true}}",
                    started.elapsed().as_secs_f64() * 1000.
                );
                Ok(())
            })();
            if let Err(error) = result {
                close_on_error(native, error, &errors);
                return;
            }
            let own_window = window.window_handle();
            let native_value = native.0 as usize;
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let _ = cx.update(|cx| {
                    if cx.windows().contains(&own_window) {
                        close_on_error(
                            HWND(native_value as *mut _),
                            "启动专项 ESC 未正常取消，自有窗口改用 WM_CLOSE 回收",
                            &errors,
                        );
                    }
                });
            }).detach();
        });
        window.refresh();
    });
    window.refresh();
}

/// 返回入口之后再检查捕获互斥体已经释放，不把“发出取消”当作回收成功。
pub(crate) fn recycled(started: Instant, success: bool) -> Result<()> {
    let mutex = unsafe {
        OpenMutexW(
            SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
            false,
            w!("Local\\RustShot.MVP.Capture"),
        )
    };
    let released = mutex.is_err();
    if let Ok(mutex) = mutex {
        unsafe {
            let _ = CloseHandle(mutex);
        }
    }
    let ready = READY.load(Ordering::Relaxed);
    let cancelled = CANCEL_REQUESTED.load(Ordering::Relaxed);
    println!(
        "{{\"phase\":\"startup_recycle\",\"elapsed_ms\":{:.3},\"ready_verified\":{ready},\"input_cancel_requested\":{cancelled},\"capture_mutex_released\":{released},\"ok\":{}}}",
        started.elapsed().as_secs_f64() * 1000., success && released && ready && cancelled
    );
    if !released {
        return Err("启动专项退出后仍持有捕获互斥体".into());
    }
    if success && (!ready || !cancelled) {
        return Err("启动专项尚未完成首帧/取消验收即退出".into());
    }
    Ok(())
}
