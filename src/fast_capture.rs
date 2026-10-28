//! 静态截图的一次 GDI 冻结；GPUI 预览与原始裁剪共享同一份 CPU BGRA。
//! 原生采集 API 失败或设备 DC 映射不受支持时允许 DXGI 回退；尺寸和内存限制直接返回错误。
//! 此路径不用于录屏；待命仅保留线程 GDI 句柄和清黑的 1px DIB，不保存桌面。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, SourcePixels},
    Result,
};
use gpui::RenderImage;
use std::{
    cell::RefCell,
    error::Error,
    fmt,
    marker::PhantomData,
    mem::{size_of, MaybeUninit},
    rc::Rc,
    sync::Arc,
    time::Instant,
};
use windows::Win32::{
    Foundation::{
        SetLastError, ERROR_COMMITMENT_LIMIT, ERROR_INSUFFICIENT_VIRTUAL_ADDR_RESOURCES,
        ERROR_NONPAGED_SYSTEM_RESOURCES, ERROR_NOT_ENOUGH_MEMORY, ERROR_NOT_ENOUGH_QUOTA,
        ERROR_NOT_ENOUGH_SERVER_MEMORY, ERROR_NO_SYSTEM_RESOURCES, ERROR_OUTOFMEMORY,
        ERROR_PAGED_SYSTEM_RESOURCES, ERROR_PAGEFILE_QUOTA, ERROR_PAGEFILE_QUOTA_EXCEEDED,
        ERROR_WORKING_SET_QUOTA, LPARAM, POINT, RECT, WIN32_ERROR,
    },
    Graphics::Gdi::*,
    UI::{HiDpi::*, WindowsAndMessaging::GetCursorPos},
};

const MAX_FROZEN_BYTES: usize = 256 * 1024 * 1024;
const MAX_CANVAS_SIDE: u32 = 16_384;
// 限制补集分带的工作量；不是按桌面像素分配额外图像。
const MAX_NATIVE_CACHE_REGIONS: usize = 64;
const MAX_NATIVE_CACHE_GAP_RECTS: usize =
    (MAX_NATIVE_CACHE_REGIONS * 2 + 1) * (MAX_NATIVE_CACHE_REGIONS + 1);

#[derive(Debug)]
struct NativeCaptureError {
    operation: &'static str,
    source: windows::core::Error,
}
impl NativeCaptureError {
    fn last(operation: &'static str) -> Self {
        Self {
            operation,
            source: windows::core::Error::from_win32(),
        }
    }
}
impl fmt::Display for NativeCaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.source.code().0 == 0 {
            write!(f, "{} 失败（Win32 未提供错误码）", self.operation)
        } else {
            write!(f, "{}: {}", self.operation, self.source)
        }
    }
}
impl Error for NativeCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}
fn native_call<T>(operation: &'static str, result: windows::core::Result<T>) -> Result<T> {
    result.map_err(|source| NativeCaptureError { operation, source }.into())
}

/// 尚未冻结像素时，设备 DC 的格式或坐标映射不受 GDI 路径支持。
#[derive(Debug)]
struct UnsupportedDeviceDc(&'static str);
impl fmt::Display for UnsupportedDeviceDc {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}
impl Error for UnsupportedDeviceDc {}

/// 回退只解决采集 API 或设备 DC 映射不可用；换后端不能绕过像素上限或重复分配。
pub(crate) fn can_fallback(error: &(dyn Error + 'static)) -> bool {
    if error.is::<UnsupportedDeviceDc>() {
        return true;
    }
    let Some(native) = error.downcast_ref::<NativeCaptureError>() else {
        return false;
    };
    // 清理失败不能把同次会话交给另一后端重新截图；错误码为零也一样。
    if ["DeleteDC", "ReleaseDC", "DeleteObject"]
        .iter()
        .any(|operation| native.operation.starts_with(operation))
        || native.operation == "SelectObject(detach)"
    {
        return false;
    }
    let code = native.source.code();
    ![
        ERROR_NOT_ENOUGH_MEMORY,
        ERROR_OUTOFMEMORY,
        ERROR_INSUFFICIENT_VIRTUAL_ADDR_RESOURCES,
        ERROR_NOT_ENOUGH_SERVER_MEMORY,
        ERROR_PAGEFILE_QUOTA_EXCEEDED,
        ERROR_NO_SYSTEM_RESOURCES,
        ERROR_NONPAGED_SYSTEM_RESOURCES,
        ERROR_PAGED_SYSTEM_RESOURCES,
        ERROR_WORKING_SET_QUOTA,
        ERROR_PAGEFILE_QUOTA,
        ERROR_COMMITMENT_LIMIT,
        ERROR_NOT_ENOUGH_QUOTA,
    ]
    .iter()
    .any(|error| code == windows::core::HRESULT::from_win32(error.0))
}

/// 原图已冻结后，同源读回失败不能再交给另一后端采集新的桌面。
#[derive(Debug)]
struct FrozenReadbackError(Box<dyn Error>);

impl fmt::Display for FrozenReadbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "已冻结原图读回失败，保留原失败：{}", self.0)
    }
}

impl Error for FrozenReadbackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn frozen_readback_error(error: Box<dyn Error>) -> Box<dyn Error> {
    Box::new(FrozenReadbackError(error))
}

thread_local! {
    static PRIMED_GDI: RefCell<Option<PrimedGdi>> = const { RefCell::new(None) };
    static PREWARMED_DEVICE_TASKS: RefCell<Option<PrewarmedDeviceTasks>> = const { RefCell::new(None) };
}

// 字段顺序保证先释放已选入 DC 的 1px DIB，再释放原线程取得的桌面 HDC。
// 不允许跨线程转移；稀疏多屏工作线程仍各自创建/释放 ScreenDc。
struct PrimedGdi {
    black_pixel: DibSurface,
    desktop: ScreenDc,
    _thread_bound: PhantomData<Rc<()>>,
}

pub(crate) fn discard_prewarmed_thread() {
    discard_primed_gdi();
    let tasks = PREWARMED_DEVICE_TASKS.with(|slot| slot.borrow_mut().take());
    // 每个尚未消费的线程收到 Stop；所有原 JoinHandle 都在当前 owner 退出前汇合。
    drop(tasks);
}

fn discard_primed_gdi() {
    let primed = PRIMED_GDI.with(|slot| slot.borrow_mut().take());
    drop(primed);
}

enum DeviceCapture {
    Pixels(FrozenScreen, Option<DeviceTiming>),
    Deferred(DetachedDeviceDdb),
}

impl DeviceCapture {
    fn timing_mut(&mut self) -> Option<&mut DeviceTiming> {
        match self {
            Self::Pixels(_, timing) => timing.as_mut(),
            Self::Deferred(capture) => capture.timing.as_mut(),
        }
    }

    fn record_worker_request(
        &mut self,
        dispatch_qpc: Option<i64>,
        receive_qpc: Option<i64>,
        precreated: bool,
    ) {
        if let Some(request) = self.timing_mut().and_then(|timing| timing.request.as_mut()) {
            request.dispatch_qpc = dispatch_qpc;
            request.worker_receive_qpc = receive_qpc;
            request.precreated_worker = Some(precreated);
        }
    }

    fn record_idle_dpi_restore(&mut self, begin: Option<i64>, end: Option<i64>) {
        if let Some(request) = self.timing_mut().and_then(|timing| timing.request.as_mut()) {
            request.idle_dpi_restore_begin_qpc = begin;
            request.idle_dpi_restore_end_qpc = end;
        }
    }
}

type DeviceTaskResult = std::result::Result<Option<DeviceCapture>, Box<dyn Error + Send>>;

enum DeviceTaskCommand {
    Capture(MonitorDevice, bool, bool, Option<i64>),
    Stop,
}

// 待机只持有一次性线程/DPI/通道/标量；诊断暖身的自有纯黑 DDB/DC 在 ACK 前全部 checked 释放。
struct DeviceTask {
    command: Option<std::sync::mpsc::SyncSender<DeviceTaskCommand>>,
    result: std::sync::mpsc::Receiver<DeviceTaskResult>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl DeviceTask {
    fn precreate(warm_device: Option<MonitorDevice>) -> Result<Self> {
        let (command, receive) = std::sync::mpsc::sync_channel(1);
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("rustshot-device-ready".into())
            .spawn(move || {
                // 在成功确认前完成线程 USER/DPI 初始化；请求仍重新核验 DPI 和新布局。
                // 外层守卫晚于本次捕获的内层守卫析构，最后恢复线程原始 DPI 上下文。
                let idle_dpi = match PhysicalDpi::new() {
                    Ok(dpi) => dpi,
                    Err(error) => {
                        let _ = started.send(Err(sendable_device_error(error)));
                        return;
                    }
                };
                let warm_result = warm_device
                    .as_ref()
                    .map(|device| warm_device_black_ddb(device, "secondary"))
                    .transpose();
                if let Err(error) = warm_result {
                    let _ = started.send(Err(sendable_device_error(error)));
                    return;
                }
                // 只有同线程暖身五个 GDI 句柄已 checked 释放才确认可待命。
                if started.send(Ok(())).is_err() {
                    return;
                }
                let (mut result, trace) = match receive.recv() {
                    Ok(DeviceTaskCommand::Capture(device, trace, deferred, dispatch_qpc)) => {
                        let receive_qpc = trace.then(crate::capture_standby::qpc);
                        let result = capture_device_result(&device, trace, deferred)
                            .map(|mut capture| {
                                capture.record_worker_request(dispatch_qpc, receive_qpc, true);
                                capture
                            })
                            .map(Some)
                            .map_err(sendable_device_error);
                        (result, trace)
                    }
                    Ok(DeviceTaskCommand::Stop) | Err(_) => (Ok(None), false),
                };
                let restore_begin = trace.then(crate::capture_standby::qpc);
                drop(idle_dpi);
                let restore_end = trace.then(crate::capture_standby::qpc);
                if let Ok(Some(capture)) = &mut result {
                    capture.record_idle_dpi_restore(restore_begin, restore_end);
                }
                // 原图已 flush、精确反选、释放捕获 DC，并调用原线程 DPI 恢复。
                // 一次性移动所有权；发布后本线程不再访问位图，不必等 OS 线程退出才显示。
                let _ = completed.send(result);
            })?;
        let mut task = Self {
            command: Some(command),
            result,
            worker: Some(worker),
        };
        // timeout 仅限制 ACK 接收；不能中断 GDI，原 join 的安全等待并非总时限。
        match ready.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(task),
            Ok(Err(error)) => Err(reap_failed_prewarm_tasks(
                std::slice::from_mut(&mut task),
                error,
            )),
            Err(error) => Err(reap_failed_prewarm_tasks(
                std::slice::from_mut(&mut task),
                precreate_ack_receive_error(error),
            )),
        }
    }

    fn start(&mut self, device: MonitorDevice, trace: bool, deferred: bool) -> Result<()> {
        let command = self.command.take().ok_or("显示设备线程任务已被消费")?;
        command
            .send(DeviceTaskCommand::Capture(
                device,
                trace,
                deferred,
                trace.then(crate::capture_standby::qpc),
            ))
            .map_err(|_| "预建显示设备线程已退出，不能派发本次冻结任务")?;
        Ok(())
    }

    fn fresh(device: MonitorDevice, trace: bool, deferred: bool) -> Result<Self> {
        let dispatch_qpc = trace.then(crate::capture_standby::qpc);
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("rustshot-device-gdi".into())
            .spawn(move || {
                // 新建线程没有接收通道；此时间是闭包进入点，区别于预建线程 recv 返回。
                let receive_qpc = trace.then(crate::capture_standby::qpc);
                let result = capture_device_result(&device, trace, deferred)
                    .map(|mut capture| {
                        capture.record_worker_request(dispatch_qpc, receive_qpc, false);
                        capture
                    })
                    .map(Some)
                    .map_err(sendable_device_error);
                let _ = completed.send(result);
            })?;
        Ok(Self {
            command: None,
            result,
            worker: Some(worker),
        })
    }

    fn receive_result(&self) -> Result<DeviceCapture> {
        match self.result.recv() {
            Ok(Ok(Some(capture))) => Ok(capture),
            Ok(Err(error)) => Err(error),
            Ok(Ok(None)) => Err("显示设备线程没有执行本次捕获任务".into()),
            Err(_) => Err("显示设备捕获线程未交付结果或意外终止".into()),
        }
    }

    fn stop_and_join(&mut self) -> Result<()> {
        if let Some(command) = self.command.take() {
            let _ = command.send(DeviceTaskCommand::Stop);
        }
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "显示设备捕获线程意外终止")?;
        }
        Ok(())
    }
}

impl Drop for DeviceTask {
    fn drop(&mut self) {
        // 容量 1 的结果通道在 join 完成前仍有效；Stop 或一次发布均不会等接收者。
        let _ = self.stop_and_join();
    }
}

pub(crate) struct PrewarmedDeviceTasks {
    workers: Vec<DeviceTask>,
    startup_ms: f64,
}

/// 仅移动已预建线程的通道与 JoinHandle；这些线程自己保有并清理 GDI 资源。
/// 原线程的 PRIMED_GDI/HDC 和 Pending 捕获对象都不跨线程移动。
pub(crate) fn take_prewarmed_device_tasks() -> Option<PrewarmedDeviceTasks> {
    PREWARMED_DEVICE_TASKS.with(|slot| slot.borrow_mut().take())
}

pub(crate) fn install_prewarmed_device_tasks(tasks: Option<PrewarmedDeviceTasks>) {
    PREWARMED_DEVICE_TASKS.with(|slot| *slot.borrow_mut() = tasks);
}

fn precreate_ack_receive_error(error: std::sync::mpsc::RecvTimeoutError) -> Box<dyn Error> {
    // 超时后原线程仍可能在清理 GDI；正常 join 不能证明迟到的 ACK 没有 hard 清理错误。
    // 未收到成功/明确失败确认时保守退出本 worker，不能追认为可冷捕获。
    Box::new(DevicePrewarmHardError(format!(
        "预建显示设备线程未确认暖身与清理完成：{error}"
    )))
}

fn prewarm_device_tasks(monitors: &[ScreenRect]) -> Result<()> {
    // 预建数量仅取本次合法布局的 N-1；请求不会信任这些闲置名称或矩形。
    pending_layout(monitors)?;
    let started = Instant::now();
    let count = monitors.len().saturating_sub(1);
    let mut workers = Vec::new();
    workers.try_reserve_exact(count)?;
    let warm = device_black_ddb_warm_enabled_value(
        std::env::var(DEVICE_BLACK_DDB_WARM_ENV).ok().as_deref(),
    );
    let devices = if warm {
        let devices = monitor_devices()?;
        if devices.iter().map(|device| device.rect).collect::<Vec<_>>() != monitors {
            return Err("纯黑 DDB 暖身的枚举顺序/物理布局与原预热布局不符".into());
        }
        // main 是首设备未来的原 capture 线程；不能只暖 N-1 个 secondary。
        warm_device_black_ddb(&devices[0], "main")?;
        Some(devices)
    } else {
        None
    };
    for index in 0..count {
        let warm_device = devices.as_ref().map(|devices| devices[index + 1].clone());
        match DeviceTask::precreate(warm_device) {
            Ok(task) => workers.push(task),
            Err(error) => return Err(reap_failed_prewarm_tasks(&mut workers, error)),
        }
    }
    PREWARMED_DEVICE_TASKS.with(|slot| {
        *slot.borrow_mut() = Some(PrewarmedDeviceTasks {
            workers,
            startup_ms: started.elapsed().as_secs_f64() * 1000.,
        });
    });
    Ok(())
}

fn sendable_device_error(error: Box<dyn Error>) -> Box<dyn Error + Send> {
    match error.downcast::<NativeCaptureError>() {
        Ok(native) => native,
        Err(error) => match error.downcast::<DevicePrewarmHardError>() {
            Ok(hard) => hard,
            Err(error) => match error.downcast::<UnsupportedDeviceDc>() {
                Ok(unsupported) => unsupported,
                Err(error) => Box::new(std::io::Error::other(error.to_string())),
            },
        },
    }
}

// 诊断默认为关闭；只暖同设备的自有 memory DDB，不读写 device desktop。
const DEVICE_BLACK_DDB_WARM_ENV: &str = "RUSTSHOT_QA_DEVICE_BLACK_DDB_WARM";

fn device_black_ddb_warm_enabled_value(value: Option<&str>) -> bool {
    value == Some("1")
}

#[derive(Debug)]
struct DevicePrewarmHardError(String);
impl fmt::Display for DevicePrewarmHardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for DevicePrewarmHardError {}

pub(crate) fn prewarm_requires_worker_exit(error: &(dyn Error + 'static)) -> bool {
    error.is::<DevicePrewarmHardError>()
}

fn device_warm_finish_result(operation: Result<()>, cleanup: Result<()>) -> Result<()> {
    cleanup?;
    operation
}

// 已获取句柄立即登记；slot 无独立 Drop，全部由同线程 aggregate owner 管理。
#[derive(Default)]
struct BlackDdbWarmSlot {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    selection_attempted: bool,
}

#[derive(Default)]
struct BlackDdbWarmOwner {
    device_dc: HDC,
    slots: [BlackDdbWarmSlot; 2],
    _thread_bound: PhantomData<Rc<()>>,
}

impl BlackDdbWarmOwner {
    fn run(&mut self, device: &MonitorDevice) -> Result<()> {
        self.device_dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateDCW(
                windows::core::w!("DISPLAY"),
                windows::core::PCWSTR(device.name.as_ptr()),
                windows::core::PCWSTR::null(),
                None,
            )
        };
        if self.device_dc.is_invalid() {
            return Err(NativeCaptureError::last("CreateDCW(black-ddb-warm)").into());
        }
        for slot in &mut self.slots {
            slot.dc = unsafe {
                SetLastError(WIN32_ERROR(0));
                CreateCompatibleDC(Some(self.device_dc))
            };
            if slot.dc.is_invalid() {
                return Err(NativeCaptureError::last("CreateCompatibleDC(black-ddb-warm)").into());
            }
            // 兼容对象来自 device DC，不能使用 memory DC 默认单色对象。
            slot.bitmap = unsafe {
                SetLastError(WIN32_ERROR(0));
                CreateCompatibleBitmap(self.device_dc, 1, 1)
            };
            if slot.bitmap.is_invalid() {
                return Err(
                    NativeCaptureError::last("CreateCompatibleBitmap(black-ddb-warm)").into(),
                );
            }
            let mut header = BITMAP::default();
            let read = unsafe {
                GetObjectW(
                    slot.bitmap.into(),
                    size_of::<BITMAP>() as i32,
                    Some((&mut header as *mut BITMAP).cast()),
                )
            };
            if read != size_of::<BITMAP>() as i32
                || header.bmWidth != 1
                || header.bmHeight != 1
                || header.bmPlanes != 1
                || header.bmBitsPixel != 32
                || !header.bmBits.is_null()
            {
                return Err("纯黑暖身不是兼容的 1px 32 位非 DIB bitmap".into());
            }
            slot.previous = unsafe { GetCurrentObject(slot.dc, OBJ_BITMAP) };
            if slot.previous.is_invalid() {
                return Err(NativeCaptureError::last(
                    "GetCurrentObject(black-ddb-warm-before-select)",
                )
                .into());
            }
            // 即使 SelectObject 返回错误，也要在清理时查询实际当前对象。
            slot.selection_attempted = true;
            let previous = unsafe {
                SetLastError(WIN32_ERROR(0));
                SelectObject(slot.dc, slot.bitmap.into())
            };
            if previous != slot.previous {
                return Err(NativeCaptureError::last("SelectObject(black-ddb-warm)").into());
            }
            if unsafe { GetCurrentObject(slot.dc, OBJ_BITMAP) } != slot.bitmap.into() {
                return Err("纯黑暖身当前 bitmap 与自有对象不符".into());
            }
            native_call("PatBlt(black-ddb-warm)", unsafe {
                SetLastError(WIN32_ERROR(0));
                PatBlt(slot.dc, 0, 0, 1, 1, BLACKNESS).ok()
            })?;
        }
        // source 和 destination 均为自有 memory DC；device DC 从不作为像素源/目标。
        native_call("BitBlt(black-ddb-memory-to-memory)", unsafe {
            SetLastError(WIN32_ERROR(0));
            BitBlt(
                self.slots[1].dc,
                0,
                0,
                1,
                1,
                Some(self.slots[0].dc),
                0,
                0,
                SRCCOPY,
            )
        })?;
        native_call("GdiFlush(black-ddb-warm)", unsafe {
            SetLastError(WIN32_ERROR(0));
            GdiFlush().ok()
        })?;
        for slot in &self.slots {
            let color = unsafe { GetPixel(slot.dc, 0, 0) }.0;
            // COLORREF 只证明自有 1px RGB 黑；不证明 alpha 或桌面捕获已暖。
            if color == u32::MAX || color & 0x00ff_ffff != 0 {
                return Err("自有纯黑 DDB 的 1px RGB oracle 失败".into());
            }
        }
        Ok(())
    }

    fn release_checked(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        if !self.device_dc.is_invalid() || self.slots.iter().any(|slot| !slot.dc.is_invalid()) {
            let flushed = unsafe {
                SetLastError(WIN32_ERROR(0));
                GdiFlush()
            };
            if !flushed.as_bool() {
                failures
                    .push(NativeCaptureError::last("GdiFlush(black-ddb-warm-cleanup)").to_string());
            }
        }
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if !slot.dc.is_invalid() {
                if slot.selection_attempted {
                    let current = unsafe { GetCurrentObject(slot.dc, OBJ_BITMAP) };
                    let restored = if current == slot.previous && !current.is_invalid() {
                        true
                    } else if current == slot.bitmap.into() && !slot.bitmap.is_invalid() {
                        let displaced = unsafe { SelectObject(slot.dc, slot.previous) };
                        displaced == slot.bitmap.into()
                            && unsafe { GetCurrentObject(slot.dc, OBJ_BITMAP) } == slot.previous
                    } else {
                        false
                    };
                    if !restored {
                        failures.push(format!("slot{index}: bitmap 精确反选未确认"));
                    }
                }
                let deleted = unsafe {
                    SetLastError(WIN32_ERROR(0));
                    DeleteDC(slot.dc)
                };
                if deleted.as_bool() {
                    slot.dc = HDC::default();
                    slot.previous = HGDIOBJ::default();
                    slot.selection_attempted = false;
                } else {
                    failures.push(
                        NativeCaptureError::last("DeleteDC(black-ddb-warm-memory)").to_string(),
                    );
                }
            }
            // DC 未确认销毁时不能尝试删除可能仍被选入的 bitmap。
            if slot.dc.is_invalid() && !slot.bitmap.is_invalid() {
                let deleted = unsafe {
                    SetLastError(WIN32_ERROR(0));
                    DeleteObject(slot.bitmap.into())
                };
                if deleted.as_bool() {
                    slot.bitmap = HBITMAP::default();
                } else {
                    failures
                        .push(NativeCaptureError::last("DeleteObject(black-ddb-warm)").to_string());
                }
            }
        }
        if !self.device_dc.is_invalid() {
            let deleted = unsafe {
                SetLastError(WIN32_ERROR(0));
                DeleteDC(self.device_dc)
            };
            if deleted.as_bool() {
                self.device_dc = HDC::default();
            } else {
                failures
                    .push(NativeCaptureError::last("DeleteDC(black-ddb-warm-device)").to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Box::new(DevicePrewarmHardError(failures.join("; "))))
        }
    }
}

impl Drop for BlackDdbWarmOwner {
    fn drop(&mut self) {
        // 同线程尽力重试；重试成功不能把先前 checked 失败追认为 Ready 合格。
        if let Err(error) = self.release_checked() {
            eprintln!("纯黑 DDB 暖身 Drop 清理仍失败：{error}");
        }
    }
}

fn warm_device_black_ddb(device: &MonitorDevice, role: &'static str) -> Result<()> {
    let started = Instant::now();
    let mut owner = BlackDdbWarmOwner::default();
    let operation = owner.run(device);
    let cleanup = owner.release_checked();
    device_warm_finish_result(operation, cleanup)?;
    let name_end = device
        .name
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(device.name.len());
    let name = String::from_utf16_lossy(&device.name[..name_end]);
    // 只有 checked 释放五个 GDI 句柄后才输出；JSON 序列化负责设备名转义。
    println!(
        "{}",
        serde_json::json!({
            "phase": "device_black_ddb_warm", "status": "completed",
            "pid": std::process::id(),
            "tid": unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
            "role": role, "device_name": name,
            "rect": [device.rect.left, device.rect.top, device.rect.width, device.rect.height],
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.,
            "all_resources_released": true, "released_scope": "warm-two-memory-dc-two-ddb-one-device-dc",
            "oracle": "own-1px-rgb-black",
            "desktop_source_used": false, "desktop_capture_warm_proven": false
        })
    );
    Ok(())
}

fn reap_failed_prewarm_tasks(tasks: &mut [DeviceTask], original: Box<dyn Error>) -> Box<dyn Error> {
    let mut failures = Vec::new();
    for task in tasks {
        if let Err(error) = task.stop_and_join() {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        original
    } else {
        Box::new(DevicePrewarmHardError(format!(
            "预建失败 {original}；原 worker 回收失败：{}",
            failures.join("; ")
        )))
    }
}

struct PrimedCaptureCleanup;
impl Drop for PrimedCaptureCleanup {
    fn drop(&mut self) {
        discard_prewarmed_thread();
    }
}

fn capture_desktop_dc() -> Result<ScreenDc> {
    let primed = PRIMED_GDI.with(|slot| slot.borrow_mut().take());
    match primed {
        Some(PrimedGdi {
            black_pixel,
            desktop,
            _thread_bound,
        }) => {
            drop(black_pixel);
            Ok(desktop)
        }
        None => ScreenDc::new(),
    }
}

/// 同一待命主线程实际读回 1 个桌面像素，立即覆盖为黑色并提交。
/// 单屏和显式诊断保留原线程 HDC/黑色 1px DIB；默认多屏在待机阶段释放。
/// 没有屏幕内容、GPUI 或显式 GPU 设备，失败与其它提前退出均由同线程清理。
pub(crate) fn prewarm_thread() -> Result<()> {
    discard_prewarmed_thread();
    let _dpi = PhysicalDpi::new()?;
    let desktop_dc = ScreenDc::new()?;
    let surface = DibSurface::new(desktop_dc.0, 1, 1)?;
    let monitors = monitor_rects()?;
    let monitor = monitors.first().ok_or("GDI 预热没有活动屏幕")?;
    native_call("BitBlt(single-pixel-prime)", unsafe {
        SetLastError(WIN32_ERROR(0));
        BitBlt(
            surface.dc,
            0,
            0,
            1,
            1,
            Some(desktop_dc.0),
            monitor.left,
            monitor.top,
            SRCCOPY,
        )
    })?;
    native_call("GdiFlush(single-pixel-prime)", unsafe {
        SetLastError(WIN32_ERROR(0));
        GdiFlush().ok()
    })?;
    native_call("PatBlt(discard-primed-pixel)", unsafe {
        SetLastError(WIN32_ERROR(0));
        PatBlt(surface.dc, 0, 0, 1, 1, BLACKNESS).ok()
    })?;
    native_call("GdiFlush(discard-primed-pixel)", unsafe {
        SetLastError(WIN32_ERROR(0));
        GdiFlush().ok()
    })?;
    // 清除全部 BGRA 字节（包括未由 BLACKNESS 保证的 alpha），不日志或持久化曾读取的颜色。
    unsafe { std::ptr::write_bytes(surface.bits, 0, 4) };
    let automatic = !matches!(
        std::env::var("RUSTSHOT_GDI_CAPTURE_STRATEGY").as_deref(),
        Ok("compact" | "parallel")
    );
    if automatic && monitors.len() > 1 {
        // 默认多屏使用各设备自己的 DC，提前释放无用的全桌面预热对象，避免请求时同步清理。
        // 若待机后布局变成单屏，捕获仍可在原线程重新取得 DC；请求清理保持幂等。
        drop(surface);
        drop(desktop_dc);
        prewarm_device_tasks(&monitors)?;
        return Ok(());
    }
    if automatic
        && device_black_ddb_warm_enabled_value(
            std::env::var(DEVICE_BLACK_DDB_WARM_ENV).ok().as_deref(),
        )
    {
        // 默认单屏同样暖 main；原唯一桌面像素的覆盖/清零/保留逻辑不变。
        prewarm_device_tasks(&monitors)?;
    }
    PRIMED_GDI.with(|slot| {
        *slot.borrow_mut() = Some(PrimedGdi {
            black_pixel: surface,
            desktop: desktop_dc,
            _thread_bound: PhantomData,
        });
    });
    Ok(())
}

/// 请求内互不嵌套的 wall 区间；不含资格检查、区间间控制开销，也不当作纯 GPU 执行耗时。
/// PatBlt/BitBlt 可延迟提交，其完成等待计入对应 flush；只在读回后的原事件中输出。
#[derive(Default)]
struct PendingCaptureTimings {
    desktop_dc_ms: f64,
    original_create_ms: f64,
    original_clear_ms: f64,
    desktop_bitblt_ms: f64,
    fixture_upload_ms: f64,
    source_flush_ms: f64,
    native_cache_create_ms: f64,
    native_cache_gap_plan_ms: f64,
    native_cache_gap_clear: Option<NativeCacheGapClear>,
    native_cache_copy_ms: f64,
    native_cache_flush_ms: f64,
    native_cache_detach_validate_ms: f64,
    original_detach_ms: f64,
    desktop_dc_release_ms: f64,
}

#[derive(Clone, Copy)]
struct NativeCacheGapClear {
    rectangles: usize,
    pixels: usize,
}

/// 同次真实冻结的原捕获 DDB 与独立原生缓存；CPU 读回延后到原生 Ready 之后。
/// 这里只转移反选后的 HBITMAP；没有捕获 HDC 跨线程，两个线程不共享选图对象或应用互斥锁。
/// 驱动仍可能内部争用；是否缩短真实 Ready 必须由实际采样确认。
pub(crate) struct PendingStartupCapture {
    original: Option<CapturedStartupDdb>,
    devices: Option<PendingDeviceCapture>,
    native: Arc<crate::startup_bitmap::StartupBitmap>,
    bounds: ScreenRect,
    monitors: Vec<ScreenRect>,
    local_rects: Vec<Rect>,
    cursor: POINT,
    current: usize,
    capture_ms: f64,
    capture_timings: PendingCaptureTimings,
    fixture: bool,
    requested_strategy: &'static str,
    _thread_bound: PhantomData<Rc<()>>,
}

impl PendingStartupCapture {
    #[allow(clippy::too_many_arguments)]
    fn from_device_ddbs(
        mut originals: Vec<DetachedDeviceDdb>,
        bounds: ScreenRect,
        monitors: Vec<ScreenRect>,
        locals: Vec<Rect>,
        cursor: POINT,
        capture_id: i64,
        started: Instant,
        trace: bool,
        fixture: bool,
        requested_strategy: &'static str,
        mut workers: Option<DeviceWorkerTiming>,
        force_cpu_fixture: bool,
    ) -> Result<Self> {
        let result = (|| -> Result<Self> {
            if originals
                .iter()
                .map(|original| original.rect)
                .collect::<Vec<_>>()
                != monitors
            {
                return Err("设备 DDB 原图顺序与冻结物理矩形不符".into());
            }
            let mut timings = PendingCaptureTimings::default();
            let native_attempt = if force_cpu_fixture {
                // 仅自有 fixture 可强制覆盖跨设备失败后的同源 CPU 回退。
                Ok(None)
            } else {
                match compose_device_ddbs(&originals, bounds, &locals, &mut timings) {
                    Ok(native) => Ok(Some(native)),
                    Err(error) if device_cache_can_fallback(error.as_ref()) => {
                        if trace {
                            crate::startup_probe::emit_trace_value(&serde_json::json!({
                                "phase":"startup_device_ddb_cache_fallback","pid":std::process::id(),
                                "capture_id":capture_id.to_string(),"error":error.to_string(),
                                "before_native_surface":true,"recapture_desktop":false,
                                "fallback":"read-back-same-device-ddbs-to-native-dib"
                            }));
                        }
                        Ok(None)
                    }
                    Err(error) => Err(error),
                }
            }?;
            let (native, early_readback) = match native_attempt {
                Some(native) => (native, None),
                None => {
                    let readback = readback_device_ddbs(&mut originals, cursor, 0.)
                        .map_err(frozen_readback_error)?;
                    let image = readback
                        .frozen
                        .read_region(Rect::full(bounds.width, bounds.height))?;
                    let native = Arc::new(crate::startup_bitmap::StartupBitmap::from_pixels(
                        bounds,
                        locals.clone(),
                        &image.bgra,
                    )?);
                    (native, Some(readback))
                }
            };
            if originals
                .iter()
                .any(|original| original.bitmap_id == native.object_id())
            {
                return Err("设备 DDB 原源与显示缓存不能相同".into());
            }
            for original in &mut originals {
                original.capture_id = capture_id;
                original.native_id = native.object_id();
            }
            Ok(Self {
                original: None,
                devices: Some(PendingDeviceCapture {
                    originals,
                    early_readback,
                    capture_id,
                    trace,
                    workers: workers.take(),
                }),
                native,
                bounds,
                current: pending_current(&monitors, cursor),
                monitors,
                local_rects: locals,
                cursor,
                capture_ms: started.elapsed().as_secs_f64() * 1000.,
                capture_timings: timings,
                fixture,
                requested_strategy,
                _thread_bound: PhantomData,
            })
        })();
        match result {
            Ok(pending) => Ok(pending),
            Err(error) => {
                if let Some(workers) = &mut workers {
                    // 缓存构建错误向上回退前也检查原线程的发布后 panic。
                    workers.join_tasks_prioritizing(trace, Some(error))?;
                    Err("设备 DDB 构建错误未保留".into())
                } else {
                    Err(error)
                }
            }
        }
    }

    /// 超出紧凑缓存预算和显式 parallel 保留既有路径；拒绝前不消费 TLS。
    pub(crate) fn capture() -> Result<Option<Self>> {
        let requested = match std::env::var("RUSTSHOT_GDI_CAPTURE_STRATEGY").as_deref() {
            Ok("parallel") => return Ok(None),
            Ok("compact") => "compact",
            _ => "auto",
        };
        let eligibility = (|| -> Result<_> {
            let dpi = PhysicalDpi::new()?;
            let monitors = monitor_rects()?;
            let (bounds, total, locals) = pending_layout(&monitors)?;
            if compact_desktop_bytes(bounds, total).is_none() {
                return Ok(None);
            }
            let mut cursor = POINT::default();
            native_call("GetCursorPos(deferred)", unsafe {
                GetCursorPos(&mut cursor)
            })?;
            Ok(Some((dpi, monitors, bounds, locals, cursor)))
        })();
        let Some((_dpi, monitors, bounds, locals, cursor)) = (match eligibility {
            Ok(eligible) => eligible,
            Err(error) => {
                discard_prewarmed_thread();
                return Err(error);
            }
        }) else {
            return Ok(None);
        };
        let _primed_cleanup = PrimedCaptureCleanup;
        let started = Instant::now();
        let capture_id = crate::capture_standby::qpc();
        if capture_id <= 0 {
            return Err("DDB 捕获时钟无效".into());
        }
        if requested == "auto" && monitors.len() > 1 {
            let devices = monitor_devices()?;
            if devices.iter().map(|device| device.rect).collect::<Vec<_>>() != monitors {
                return Err("显示设备名称与物理屏幕枚举次序不符".into());
            }
            discard_primed_gdi();
            let trace = std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
            let (captures, workers) = capture_device_tasks(&devices, trace, true)?;
            let originals = captures
                .into_iter()
                .map(|capture| match capture {
                    DeviceCapture::Deferred(original) => Ok(original),
                    DeviceCapture::Pixels(..) => Err("延后读回任务返回了 CPU 像素".into()),
                })
                .collect::<Result<Vec<_>>>()?;
            return Self::from_device_ddbs(
                originals,
                bounds,
                monitors,
                locals,
                cursor,
                capture_id,
                started,
                trace,
                false,
                requested,
                Some(workers),
                false,
            )
            .map(Some);
        }
        let mut capture_timings = PendingCaptureTimings::default();
        let stage_started = Instant::now();
        let desktop = capture_desktop_dc()?;
        capture_timings.desktop_dc_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        let source = DdbCaptureSurface::new(desktop.0, bounds)?;
        capture_timings.original_create_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        // 自有 bitmap 的空隙先置黑；只有这一处 BitBlt 的源是桌面。
        let stage_started = Instant::now();
        native_call("PatBlt(deferred-gap-black)", unsafe {
            SetLastError(WIN32_ERROR(0));
            PatBlt(
                source.dc,
                0,
                0,
                bounds.width as i32,
                bounds.height as i32,
                BLACKNESS,
            )
            .ok()
        })?;
        capture_timings.original_clear_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        native_call("BitBlt(deferred-desktop)", unsafe {
            SetLastError(WIN32_ERROR(0));
            BitBlt(
                source.dc,
                0,
                0,
                bounds.width as i32,
                bounds.height as i32,
                Some(desktop.0),
                bounds.left,
                bounds.top,
                SRCCOPY,
            )
        })?;
        capture_timings.desktop_bitblt_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        native_call("GdiFlush(deferred-desktop)", unsafe { GdiFlush().ok() })?;
        capture_timings.source_flush_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let (original, native) = detach_startup_pair(
            source,
            desktop,
            bounds,
            &locals,
            capture_id,
            &mut capture_timings,
        )?;
        let current = pending_current(&monitors, cursor);
        Ok(Some(Self {
            original: Some(original),
            devices: None,
            native,
            bounds,
            monitors,
            local_rects: locals,
            cursor,
            current,
            capture_ms: started.elapsed().as_secs_f64() * 1000.,
            capture_timings,
            fixture: false,
            requested_strategy: requested,
            _thread_bound: PhantomData,
        }))
    }

    /// 自有 fixture 走相同的 DDB 克隆/反选/读回路径；不从任何桌面 HDC 复制像素。
    pub(crate) fn from_fixture(frozen: FrozenDesktop) -> Result<Self> {
        let _primed_cleanup = PrimedCaptureCleanup;
        let _dpi = PhysicalDpi::new()?;
        let started = Instant::now();
        let monitors: Vec<_> = frozen.screens.iter().map(|screen| screen.rect).collect();
        let (bounds, total, locals) = pending_layout(&monitors)?;
        if bounds != frozen.bounds
            || frozen.current >= monitors.len()
            || compact_desktop_bytes(bounds, total).is_none()
        {
            return Err("DDB fixture 的元数据或紧凑预算不符".into());
        }
        for screen in &frozen.screens {
            if screen.preview.as_bytes(0).is_none_or(|pixels| {
                pixels.len() != image_bytes(screen.rect.width, screen.rect.height).unwrap_or(0)
            }) {
                return Err("DDB fixture 原始 BGRA 不可用或长度不符".into());
            }
        }
        let capture_id = crate::capture_standby::qpc();
        if capture_id <= 0 {
            return Err("DDB fixture 时钟无效".into());
        }
        let mut capture_timings = PendingCaptureTimings::default();
        let stage_started = Instant::now();
        let desktop = capture_desktop_dc()?;
        capture_timings.desktop_dc_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        let source = DdbCaptureSurface::new(desktop.0, bounds)?;
        capture_timings.original_create_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        native_call("PatBlt(deferred-fixture-gap)", unsafe {
            SetLastError(WIN32_ERROR(0));
            PatBlt(
                source.dc,
                0,
                0,
                bounds.width as i32,
                bounds.height as i32,
                BLACKNESS,
            )
            .ok()
        })?;
        capture_timings.original_clear_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        for (screen, local) in frozen.screens.iter().zip(&locals) {
            let pixels = screen.preview.as_bytes(0).ok_or("DDB fixture 原图已失效")?;
            let info = top_down_info(screen.rect.width, screen.rect.height);
            let rows = unsafe {
                SetLastError(WIN32_ERROR(0));
                StretchDIBits(
                    source.dc,
                    local.x as i32,
                    local.y as i32,
                    local.width as i32,
                    local.height as i32,
                    0,
                    0,
                    screen.rect.width as i32,
                    screen.rect.height as i32,
                    Some(pixels.as_ptr().cast()),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                )
            };
            if rows != screen.rect.height as i32 {
                return Err(NativeCaptureError::last("StretchDIBits(deferred-fixture)").into());
            }
        }
        capture_timings.fixture_upload_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let stage_started = Instant::now();
        native_call("GdiFlush(deferred-fixture)", unsafe { GdiFlush().ok() })?;
        capture_timings.source_flush_ms = stage_started.elapsed().as_secs_f64() * 1000.;
        let (original, native) = detach_startup_pair(
            source,
            desktop,
            bounds,
            &locals,
            capture_id,
            &mut capture_timings,
        )?;
        let chosen = monitors[frozen.current];
        let cursor = POINT {
            x: chosen
                .left
                .checked_add_unsigned(chosen.width / 2)
                .ok_or("fixture 横坐标溢出")?,
            y: chosen
                .top
                .checked_add_unsigned(chosen.height / 2)
                .ok_or("fixture 纵坐标溢出")?,
        };
        let current = pending_current(&monitors, cursor);
        if current != frozen.current {
            return Err("DDB fixture 当前屏幕不唯一或顺序不符".into());
        }
        drop(frozen);
        Ok(Self {
            original: Some(original),
            devices: None,
            native,
            bounds,
            monitors,
            local_rects: locals,
            cursor,
            current,
            capture_ms: started.elapsed().as_secs_f64() * 1000.,
            capture_timings,
            fixture: true,
            requested_strategy: "fixture",
            _thread_bound: PhantomData,
        })
    }

    pub(crate) fn bounds(&self) -> ScreenRect {
        self.bounds
    }
    pub(crate) fn current(&self) -> usize {
        self.current
    }
    pub(crate) fn monitor_rects(&self) -> &[ScreenRect] {
        &self.monitors
    }
    pub(crate) fn local_rects(&self) -> &[Rect] {
        &self.local_rects
    }
    pub(crate) fn native_bitmap(&self) -> Arc<crate::startup_bitmap::StartupBitmap> {
        Arc::clone(&self.native)
    }

    /// 只从本请求原 DDB 读回；首屏调用方在真实原生 Ready 后调用，不重截桌面。
    /// 原生启动失败的 CPU 回退和无窗口 fixture 也可消费同源；本方法不自行声称 Ready。
    /// scratch 和各屏 BGRA 各自 <=256MiB；DDB 字节值仅为 BGRA 等价，不是 GPU 分配查询。
    pub(crate) fn finish(mut self) -> Result<FrozenDesktop> {
        if let Some(devices) = self.devices.take() {
            return devices.finish(&self);
        }
        let original = self.original.as_mut().ok_or("缺少本次原始 DDB")?;
        let readback_started = Instant::now();
        let readback_start_qpc = crate::capture_standby::qpc();
        let mut scratch = Vec::new();
        scratch.try_reserve_exact(original.source_bytes)?;
        scratch.resize(original.source_bytes, 0);
        let dc = ReadbackDc::new()?;
        let mut info = top_down_info(self.bounds.width, self.bounds.height);
        let rows = unsafe {
            SetLastError(WIN32_ERROR(0));
            GetDIBits(
                dc.0,
                original.bitmap,
                0,
                self.bounds.height,
                Some(scratch.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if rows != self.bounds.height as i32 {
            return Err(NativeCaptureError::last("GetDIBits(deferred-original)").into());
        }
        validate_readback_header(self.bounds, &info.bmiHeader, rows)?;
        native_call("GdiFlush(deferred-readback)", unsafe { GdiFlush().ok() })?;
        dc.finish()?;
        let readback_finish_qpc = crate::capture_standby::qpc();
        let readback_ms = readback_started.elapsed().as_secs_f64() * 1000.;
        let copy_started = Instant::now();
        let screens = split_pending_bgra(self.bounds, &self.monitors, &scratch)?;
        drop(scratch);
        let frozen = FrozenDesktop::from_screens(screens, self.cursor, self.capture_ms)?;
        if frozen.bounds != self.bounds || frozen.current != self.current {
            return Err("DDB 读回后屏幕边界/顺序不符".into());
        }
        original.readback_completed = true;
        let cpu_finish_qpc = crate::capture_standby::qpc();
        if original.trace {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_ddb_readback", "pid":std::process::id(),
                "capture_id":original.capture_id.to_string(),
                "original_capture_bitmap_id":original.object_id().to_string(),
                "native_cache_bitmap_id":self.native.object_id().to_string(),
                "source_bitmap_id":original.object_id().to_string(),
                "bounds":[self.bounds.left,self.bounds.top,self.bounds.width as i32,self.bounds.height as i32],
                "source_bytes":original.source_bytes,"planes_count":self.monitors.len(),
                "fixture":self.fixture,"screen_capture_calls":if self.fixture {0} else {1},
                "native_cache_independent":original.object_id()!=self.native.object_id(),
                "capture_dc_deleted":true,"screen_dc_released":true,"original_deselected":true,
                "readback_start_qpc":readback_start_qpc,"readback_finish_qpc":readback_finish_qpc,
                "cpu_finish_qpc":cpu_finish_qpc,"capture_ms":self.capture_ms,
                "capture_timings_ms":{
                    "desktop_dc_ms":self.capture_timings.desktop_dc_ms,
                    "original_create_ms":self.capture_timings.original_create_ms,
                    "original_clear_ms":self.capture_timings.original_clear_ms,
                    "desktop_bitblt_ms":self.capture_timings.desktop_bitblt_ms,
                    "fixture_upload_ms":self.capture_timings.fixture_upload_ms,
                    "source_flush_ms":self.capture_timings.source_flush_ms,
                    "native_cache_create_ms":self.capture_timings.native_cache_create_ms,
                    "native_cache_copy_ms":self.capture_timings.native_cache_copy_ms,
                    "native_cache_flush_ms":self.capture_timings.native_cache_flush_ms,
                    "native_cache_detach_validate_ms":self.capture_timings.native_cache_detach_validate_ms,
                    "original_detach_ms":self.capture_timings.original_detach_ms,
                    "desktop_dc_release_ms":self.capture_timings.desktop_dc_release_ms
                },
                "capture_timing_scope":"disjoint wall intervals; excludes eligibility and unmeasured control overhead; not GPU execution times or a replacement capture total",
                "capture_timing_desktop_bitblt_executed":!self.fixture,
                "capture_timing_fixture_upload_executed":self.fixture,
                "readback_ms":readback_ms,"copy_ms":copy_started.elapsed().as_secs_f64()*1000.,
                "rows_read":rows,"top_down":true,"alpha_normalized":true,
                "requested_capture_strategy":self.requested_strategy,
                "capture_strategy":"one-virtual-ddb-deferred-readback",
                "bitmap_bytes_are_bgra_equivalent_not_gpu_allocation":true
            }));
        }
        Ok(frozen)
    }
}

fn pending_layout(monitors: &[ScreenRect]) -> Result<(ScreenRect, usize, Vec<Rect>)> {
    let bounds = ScreenRect::union(monitors).ok_or("DDB 冻结没有活动屏幕")?;
    // 这里只验证原逐屏预算；稀疏虚拟桌面超过 scratch 预算应返回旧路径，而非拒绝原合法逐屏采集。
    if bounds.width > MAX_CANVAS_SIDE
        || bounds.height > MAX_CANVAS_SIDE
        || bounds.left.checked_add_unsigned(bounds.width).is_none()
        || bounds.top.checked_add_unsigned(bounds.height).is_none()
    {
        return Err("DDB 全局边界溢出".into());
    }
    let mut total = 0usize;
    let mut locals = Vec::with_capacity(monitors.len());
    for &monitor in monitors {
        let local = monitor.local(bounds);
        if !local.valid(bounds.width, bounds.height) {
            return Err("DDB 显示器局部边界不符".into());
        }
        total = total
            .checked_add(image_bytes(monitor.width, monitor.height)?)
            .ok_or("DDB 屏幕容量溢出")?;
        if total > MAX_FROZEN_BYTES {
            return Err("DDB 各屏 BGRA 超过 256 MiB".into());
        }
        locals.push(local);
    }
    Ok((bounds, total, locals))
}

fn pending_current(monitors: &[ScreenRect], cursor: POINT) -> usize {
    monitors
        .iter()
        .position(|rect| {
            i64::from(cursor.x) >= i64::from(rect.left)
                && i64::from(cursor.y) >= i64::from(rect.top)
                && i64::from(cursor.x) < i64::from(rect.left) + i64::from(rect.width)
                && i64::from(cursor.y) < i64::from(rect.top) + i64::from(rect.height)
        })
        .unwrap_or(0)
}

fn top_down_info(width: u32, height: u32) -> BITMAPINFO {
    crate::gdi_surface::bitmap_info(width, height)
}

fn validate_readback_header(
    bounds: ScreenRect,
    header: &BITMAPINFOHEADER,
    rows: i32,
) -> Result<()> {
    image_bytes(bounds.width, bounds.height)?;
    if rows != bounds.height as i32
        || header.biSize != size_of::<BITMAPINFOHEADER>() as u32
        || header.biWidth != bounds.width as i32
        || header.biHeight != -(bounds.height as i32)
        || header.biPlanes != 1
        || header.biBitCount != 32
        || header.biCompression != BI_RGB.0
    {
        return Err("DDB 读回行数或 top-down BGRA 头不符".into());
    }
    Ok(())
}

fn split_pending_bgra(
    bounds: ScreenRect,
    monitors: &[ScreenRect],
    source: &[u8],
) -> Result<Vec<FrozenScreen>> {
    if source.len() != image_bytes(bounds.width, bounds.height)? {
        return Err("DDB 读回源长度不符".into());
    }
    let plans: Vec<_> = monitors
        .iter()
        .map(|&rect| PlaneCopyPlan::new(bounds, rect))
        .collect::<Result<_>>()?;
    let total = plans.iter().try_fold(0usize, |sum, plan| {
        sum.checked_add(plan.bytes).ok_or("DDB 输出容量溢出")
    })?;
    if total > MAX_FROZEN_BYTES {
        return Err("DDB 输出超过 256 MiB".into());
    }
    let mut screens = Vec::with_capacity(plans.len());
    for plan in plans {
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(plan.bytes)?;
        unsafe {
            plan.initialize(
                source.as_ptr(),
                source.len(),
                &mut pixels.spare_capacity_mut()[..plan.bytes],
            )?;
            pixels.set_len(plan.bytes);
        }
        screens.push(FrozenScreen::from_pixels(plan.rect, pixels)?);
    }
    Ok(screens)
}

struct DdbCaptureSurface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    _thread_bound: PhantomData<Rc<()>>,
}
impl DdbCaptureSurface {
    fn new(screen: HDC, bounds: ScreenRect) -> Result<Self> {
        image_bytes(bounds.width, bounds.height)?;
        let dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateCompatibleDC(Some(screen))
        };
        if dc.is_invalid() {
            return Err(NativeCaptureError::last("CreateCompatibleDC(DDB)").into());
        }
        let mut owner = Self {
            dc,
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            _thread_bound: PhantomData,
        };
        // 使用原屏幕 DC 创建，不能使用新 memory DC 的默认单色 bitmap，也不能选着 DIB 创建。
        owner.bitmap = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateCompatibleBitmap(screen, bounds.width as i32, bounds.height as i32)
        };
        if owner.bitmap.is_invalid() {
            return Err(NativeCaptureError::last("CreateCompatibleBitmap(DDB)").into());
        }
        let mut header = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                owner.bitmap.into(),
                size_of::<BITMAP>() as i32,
                Some((&mut header as *mut BITMAP).cast()),
            )
        };
        if read != size_of::<BITMAP>() as i32
            || header.bmWidth != bounds.width as i32
            || header.bmHeight != bounds.height as i32
            || header.bmPlanes != 1
            || header.bmBitsPixel != 32
            || !header.bmBits.is_null()
        {
            return Err("捕获 DDB 原生尺寸或 32 位非 DIB 格式不符".into());
        }
        owner.previous = unsafe { SelectObject(dc, owner.bitmap.into()) };
        if owner.previous.is_invalid()
            || unsafe { GetCurrentObject(dc, OBJ_BITMAP) } != owner.bitmap.into()
        {
            return Err(NativeCaptureError::last("SelectObject(DDB)").into());
        }
        Ok(owner)
    }
    fn detach(mut self) -> Result<HBITMAP> {
        let displaced = unsafe { SelectObject(self.dc, self.previous) };
        if displaced != self.bitmap.into()
            || unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } != self.previous
        {
            return Err("捕获 DDB 没有精确反选".into());
        }
        self.previous = HGDIOBJ::default();
        if !unsafe { DeleteDC(self.dc) }.as_bool() {
            return Err(NativeCaptureError::last("DeleteDC(DDB-detach)").into());
        }
        self.dc = HDC::default();
        Ok(std::mem::take(&mut self.bitmap))
    }
    fn finish_checked(mut self) -> Result<()> {
        let flushed = unsafe { GdiFlush() }.as_bool();
        let displaced = unsafe { SelectObject(self.dc, self.previous) };
        let restored = displaced == self.bitmap.into()
            && unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } == self.previous;
        if restored {
            self.previous = HGDIOBJ::default();
        }
        let dc_deleted = unsafe { DeleteDC(self.dc) }.as_bool();
        if dc_deleted {
            self.dc = HDC::default();
            self.previous = HGDIOBJ::default();
        }
        let bitmap_deleted = unsafe { DeleteObject(self.bitmap.into()) }.as_bool();
        if bitmap_deleted {
            self.bitmap = HBITMAP::default();
        }
        if !flushed || !restored || !dc_deleted || !bitmap_deleted {
            return Err(format!("DDB 缓存清理失败：flush={flushed}, restored={restored}, dc={dc_deleted}, bitmap={bitmap_deleted}").into());
        }
        Ok(())
    }
}
impl Drop for DdbCaptureSurface {
    fn drop(&mut self) {
        let restored = if self.previous.is_invalid() || self.dc.is_invalid() {
            true
        } else {
            let displaced = unsafe { SelectObject(self.dc, self.previous) };
            displaced == self.bitmap.into()
                && unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } == self.previous
        };
        let dc_deleted = self.dc.is_invalid() || unsafe { DeleteDC(self.dc) }.as_bool();
        let bitmap_deleted =
            self.bitmap.is_invalid() || unsafe { DeleteObject(self.bitmap.into()) }.as_bool();
        if !restored || !dc_deleted || !bitmap_deleted {
            eprintln!("{{\"phase\":\"startup_ddb_capture_cleanup_error\",\"pid\":{},\"restored\":{restored},\"dc_deleted\":{dc_deleted},\"bitmap_deleted\":{bitmap_deleted}}}",std::process::id());
        }
    }
}

struct CapturedStartupDdb {
    bitmap: HBITMAP,
    capture_id: i64,
    native_id: usize,
    source_bytes: usize,
    readback_completed: bool,
    trace: bool,
    _thread_bound: PhantomData<Rc<()>>,
}
impl CapturedStartupDdb {
    fn object_id(&self) -> usize {
        self.bitmap.0 as usize
    }
}
impl Drop for CapturedStartupDdb {
    fn drop(&mut self) {
        let deleted = unsafe { DeleteObject(self.bitmap.into()) }.as_bool();
        let flushed = unsafe { GdiFlush() }.as_bool();
        if self.trace || !deleted || !flushed {
            if self.trace {
                crate::startup_probe::emit_trace_value(
                    &serde_json::json!({"phase":"startup_ddb_drop","pid":std::process::id(),
                    "capture_id":self.capture_id.to_string(),"bitmap_id":self.object_id().to_string(),
                    "native_cache_bitmap_id":self.native_id.to_string(),"source_bytes":self.source_bytes,
                    "readback_completed":self.readback_completed,"bitmap_not_selected_in_owned_dc":true,
                    "delete_ok":deleted,"flush_ok":flushed}),
                );
            } else {
                println!(
                    "{}",
                    serde_json::json!({"phase":"startup_ddb_drop","pid":std::process::id(),
                    "capture_id":self.capture_id.to_string(),"bitmap_id":self.object_id().to_string(),
                    "native_cache_bitmap_id":self.native_id.to_string(),"source_bytes":self.source_bytes,
                    "readback_completed":self.readback_completed,"bitmap_not_selected_in_owned_dc":true,
                    "delete_ok":deleted,"flush_ok":flushed})
                );
            };
        }
    }
}

fn detach_startup_pair(
    source: DdbCaptureSurface,
    desktop: ScreenDc,
    bounds: ScreenRect,
    locals: &[Rect],
    capture_id: i64,
    timings: &mut PendingCaptureTimings,
) -> Result<(
    CapturedStartupDdb,
    Arc<crate::startup_bitmap::StartupBitmap>,
)> {
    let source_bytes = image_bytes(bounds.width, bounds.height)?;
    let stage_started = Instant::now();
    let cache = DdbCaptureSurface::new(desktop.0, bounds)?;
    timings.native_cache_create_ms = stage_started.elapsed().as_secs_f64() * 1000.;
    let stage_started = Instant::now();
    native_call("BitBlt(independent-native-DDB)", unsafe {
        SetLastError(WIN32_ERROR(0));
        BitBlt(
            cache.dc,
            0,
            0,
            bounds.width as i32,
            bounds.height as i32,
            Some(source.dc),
            0,
            0,
            SRCCOPY,
        )
    })?;
    timings.native_cache_copy_ms = stage_started.elapsed().as_secs_f64() * 1000.;
    let stage_started = Instant::now();
    native_call("GdiFlush(independent-native-DDB)", unsafe {
        GdiFlush().ok()
    })?;
    timings.native_cache_flush_ms = stage_started.elapsed().as_secs_f64() * 1000.;
    let stage_started = Instant::now();
    let cache_bitmap = cache.detach()?;
    let native = Arc::new(unsafe {
        crate::startup_bitmap::StartupBitmap::from_deselected_ddb(
            cache_bitmap,
            bounds,
            locals.to_vec(),
        )?
    });
    timings.native_cache_detach_validate_ms = stage_started.elapsed().as_secs_f64() * 1000.;
    let stage_started = Instant::now();
    let original_bitmap = source.detach()?;
    timings.original_detach_ms = stage_started.elapsed().as_secs_f64() * 1000.;
    let original = CapturedStartupDdb {
        bitmap: original_bitmap,
        capture_id,
        native_id: native.object_id(),
        source_bytes,
        readback_completed: false,
        trace: std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1"),
        _thread_bound: PhantomData,
    };
    // 未把 capture HDC 或 DC 给原生线程；GetDIBits 的原 bitmap 也已从全部自有 DC 解除。
    if original.object_id() == native.object_id() {
        return Err("DDB 原源与显示缓存不能相同".into());
    }
    let stage_started = Instant::now();
    desktop.release_checked()?;
    timings.desktop_dc_release_ms = stage_started.elapsed().as_secs_f64() * 1000.;
    // 明确只由原 capture owner 回收；没有原生缓存的强引用存入读回 owner。
    Ok((original, native))
}

/// 只能由 GdiFlush + 精确反选 + 删除捕获 DC 之后构造。
/// 这里的 usize 是唯一拥有的、不在任何 DC 上的 HBITMAP；不转移 HDC/像素地址。
/// 工作线程交给请求线程后，后者独占临时选择、GetDIBits 和最终 DeleteObject。
struct DetachedDeviceDdb {
    bitmap_id: usize,
    rect: ScreenRect,
    capture_id: i64,
    native_id: usize,
    readback_completed: bool,
    timing: Option<DeviceTiming>,
}
impl DetachedDeviceDdb {
    fn bitmap(&self) -> HBITMAP {
        HBITMAP(self.bitmap_id as *mut _)
    }
}
impl Drop for DetachedDeviceDdb {
    fn drop(&mut self) {
        let deleted = unsafe { DeleteObject(self.bitmap().into()) }.as_bool();
        let flushed = unsafe { GdiFlush() }.as_bool();
        if self.timing.is_some() || !deleted || !flushed {
            if self.timing.is_some() {
                crate::startup_probe::emit_trace_value(&serde_json::json!({
                    "phase":"startup_device_ddb_drop","pid":std::process::id(),
                    "capture_id":self.capture_id.to_string(),"bitmap_id":self.bitmap_id.to_string(),
                    "native_cache_bitmap_id":self.native_id.to_string(),
                    "readback_completed":self.readback_completed,
                    "bitmap_not_selected_in_owned_dc":true,"delete_ok":deleted,"flush_ok":flushed
                }));
            } else {
                println!(
                    "{}",
                    serde_json::json!({
                        "phase":"startup_device_ddb_drop","pid":std::process::id(),
                        "capture_id":self.capture_id.to_string(),"bitmap_id":self.bitmap_id.to_string(),
                        "native_cache_bitmap_id":self.native_id.to_string(),
                        "readback_completed":self.readback_completed,
                        "bitmap_not_selected_in_owned_dc":true,"delete_ok":deleted,"flush_ok":flushed
                    })
                );
            };
        }
    }
}

struct DeviceReadback {
    frozen: FrozenDesktop,
    start_qpc: i64,
    finish_qpc: i64,
    wall_ms: f64,
}

struct PendingDeviceCapture {
    originals: Vec<DetachedDeviceDdb>,
    early_readback: Option<DeviceReadback>,
    capture_id: i64,
    trace: bool,
    workers: Option<DeviceWorkerTiming>,
}
impl PendingDeviceCapture {
    fn finish(mut self, pending: &PendingStartupCapture) -> Result<FrozenDesktop> {
        // 调用方已完成原生真实 Ready（或首屏失败）；错误与取消同样保留原句柄并回收。
        // 不把线程退出、TLS 清理当作结果可用门，但读回前仍检查原线程没有恐慌。
        if let Some(workers) = &mut self.workers {
            workers.join_tasks(self.trace)?;
        }
        let early = self.early_readback.is_some();
        let mut readback = match self.early_readback.take() {
            Some(readback) => readback,
            None => readback_device_ddbs(&mut self.originals, pending.cursor, pending.capture_ms)?,
        };
        readback.frozen.capture_ms = pending.capture_ms;
        if readback.frozen.bounds != pending.bounds || readback.frozen.current != pending.current {
            return Err("设备 DDB 读回后屏幕边界/顺序不符".into());
        }
        if self.trace {
            if let Some(workers) = &self.workers {
                workers.print();
            }
            for (index, original) in self.originals.iter().enumerate() {
                if let Some(timing) = &original.timing {
                    timing.print_deferred(index);
                }
            }
            let ids: Vec<_> = self
                .originals
                .iter()
                .map(|original| original.bitmap_id.to_string())
                .collect();
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_ddb_readback","pid":std::process::id(),
                "capture_id":self.capture_id.to_string(),
                "original_capture_bitmap_id":ids.first(),"original_capture_bitmap_ids":ids,
                "native_cache_bitmap_id":pending.native.object_id().to_string(),
                "bounds":[pending.bounds.left,pending.bounds.top,pending.bounds.width,pending.bounds.height],
                "source_bytes":self.originals.iter().map(|original| image_bytes(original.rect.width,original.rect.height).unwrap_or(0)).sum::<usize>(),
                "planes_count":self.originals.len(),"fixture":pending.fixture,
                "screen_capture_calls":if pending.fixture {0} else {self.originals.len()},
                "native_cache_independent":true,"capture_dc_deleted":true,
                "screen_dc_released":true,"original_deselected":true,
                "readback_start_qpc":readback.start_qpc,"readback_finish_qpc":readback.finish_qpc,
                "cpu_finish_qpc":readback.finish_qpc,"readback_ms":readback.wall_ms,
                "readback_before_native_surface":early,"capture_ms":pending.capture_ms,
                "native_cache_copy_ms":pending.capture_timings.native_cache_copy_ms,
                "native_cache_clear_ms":pending.capture_timings.original_clear_ms,
                "native_cache_gap_plan_ms":pending.capture_timings.native_cache_gap_plan_ms,
                "native_cache_clear_scope":pending.capture_timings.native_cache_gap_clear.map(|_| "monitor-union-gaps-only"),
                "native_cache_clear_rectangles":pending.capture_timings.native_cache_gap_clear.map(|clear| clear.rectangles),
                "native_cache_clear_pixels":pending.capture_timings.native_cache_gap_clear.map(|clear| clear.pixels),
                "native_cache_clear_screen_pixels":pending.capture_timings.native_cache_gap_clear.map(|_| 0),
                "top_down":true,"alpha_normalized":true,"desktop_recaptured":false,
                "requested_capture_strategy":pending.requested_strategy,
                "capture_strategy":if early {"device-ddb-same-source-cpu-fallback"} else {"device-ddb-deferred-readback"},
                "bitmap_bytes_are_bgra_equivalent_not_gpu_allocation":true,
                "timing_scope":"request wall intervals; per-device captures overlap; readback includes BGRA allocation, GetDIBits, flush, alpha normalization and CPU image construction"
            }));
        }
        Ok(readback.frozen)
    }
}

fn readback_device_ddbs(
    originals: &mut [DetachedDeviceDdb],
    cursor: POINT,
    capture_ms: f64,
) -> Result<DeviceReadback> {
    let started = Instant::now();
    let start_qpc = crate::capture_standby::qpc();
    let mut screens = Vec::new();
    screens.try_reserve_exact(originals.len())?;
    for original in originals {
        let bytes = image_bytes(original.rect.width, original.rect.height)?;
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(bytes)?;
        bgra.resize(bytes, 0);
        let dc = ReadbackDc::new()?;
        let mut info = top_down_info(original.rect.width, original.rect.height);
        let rows = unsafe {
            SetLastError(WIN32_ERROR(0));
            GetDIBits(
                dc.0,
                original.bitmap(),
                0,
                original.rect.height,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if rows != original.rect.height as i32 {
            return Err(NativeCaptureError::last("GetDIBits(deferred-device-original)").into());
        }
        validate_readback_header(original.rect, &info.bmiHeader, rows)?;
        native_call("GdiFlush(deferred-device-readback)", unsafe {
            GdiFlush().ok()
        })?;
        dc.finish()?;
        // 请求 top-down 32bpp BI_RGB 保证紧凑输出；DDB 自身的 driver stride 不外露。
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        screens.push(FrozenScreen::from_pixels(original.rect, bgra)?);
        original.readback_completed = true;
    }
    Ok(DeviceReadback {
        frozen: FrozenDesktop::from_screens(screens, cursor, capture_ms)?,
        start_qpc,
        finish_qpc: crate::capture_standby::qpc(),
        wall_ms: started.elapsed().as_secs_f64() * 1000.,
    })
}

fn device_cache_can_fallback(error: &(dyn Error + 'static)) -> bool {
    can_fallback(error)
        && error
            .downcast_ref::<NativeCaptureError>()
            .is_some_and(|native| {
                matches!(
                    native.operation,
                    "SelectObject(device-cache-source)" | "BitBlt(device-native-cache)"
                )
            })
}

struct NativeCacheGapPlan {
    gaps: Vec<Rect>,
    pixels: usize,
}

/// 按所有矩形的 Y 边界分带，每带合并 X 联合区域，只输出其补集。
/// 区域允许重叠和贴边；每个返回像素只属于一个 gap，且不属于任何屏幕。
fn native_cache_gap_plan(bounds: ScreenRect, regions: &[Rect]) -> Result<NativeCacheGapPlan> {
    let canvas_pixels = image_bytes(bounds.width, bounds.height)? / 4;
    if bounds.left.checked_add_unsigned(bounds.width).is_none()
        || bounds.top.checked_add_unsigned(bounds.height).is_none()
        || regions.is_empty()
        || regions.len() > MAX_NATIVE_CACHE_REGIONS
        || regions
            .iter()
            .any(|rect| !rect.valid(bounds.width, bounds.height))
    {
        return Err("DDB 缓存空隙的物理边界、区域或几何预算无效".into());
    }
    let edge_budget = regions
        .len()
        .checked_mul(2)
        .and_then(|count| count.checked_add(2))
        .ok_or("DDB 缓存空隙边界数量溢出")?;
    let gap_budget = edge_budget
        .checked_sub(1)
        .and_then(|bands| {
            regions
                .len()
                .checked_add(1)
                .and_then(|intervals| bands.checked_mul(intervals))
        })
        .filter(|&count| count <= MAX_NATIVE_CACHE_GAP_RECTS)
        .ok_or("DDB 缓存空隙矩形数量超过预算")?;
    let mut edges = Vec::new();
    edges.try_reserve_exact(edge_budget)?;
    edges.extend([0, bounds.height]);
    for rect in regions {
        // valid 已检查相加不溢出且落在画布内。
        edges.extend([rect.y, rect.y + rect.height]);
    }
    edges.sort_unstable();
    edges.dedup();
    let mut intervals = Vec::new();
    intervals.try_reserve_exact(regions.len())?;
    let mut gaps = Vec::new();
    gaps.try_reserve_exact(gap_budget)?;
    let mut pixels = 0usize;
    let mut append_gap = |rect: Rect| -> Result<()> {
        let total = (rect.width as usize)
            .checked_mul(rect.height as usize)
            .and_then(|area| pixels.checked_add(area))
            .ok_or("DDB 缓存空隙像素数量溢出")?;
        if !rect.valid(bounds.width, bounds.height)
            || gaps.len() >= gap_budget
            || total > canvas_pixels
        {
            return Err("DDB 缓存空隙矩形越界或超过预算".into());
        }
        gaps.push(rect);
        pixels = total;
        Ok(())
    };
    for band in edges.windows(2) {
        let top = band[0];
        let bottom = band[1];
        intervals.clear();
        for rect in regions {
            if rect.y <= top && rect.y + rect.height >= bottom {
                intervals.push((rect.x, rect.x + rect.width));
            }
        }
        intervals.sort_unstable();
        let mut covered_right = 0;
        for &(left, right) in &intervals {
            if left > covered_right {
                append_gap(Rect {
                    x: covered_right,
                    y: top,
                    width: left - covered_right,
                    height: bottom - top,
                })?;
            }
            covered_right = covered_right.max(right);
        }
        if covered_right < bounds.width {
            append_gap(Rect {
                x: covered_right,
                y: top,
                width: bounds.width - covered_right,
                height: bottom - top,
            })?;
        }
    }
    Ok(NativeCacheGapPlan { gaps, pixels })
}

/// 每个原图仅在本线程临时选入 DC；跨设备不兼容只能在首屏前回退同源 CPU。
fn compose_device_ddbs(
    originals: &[DetachedDeviceDdb],
    bounds: ScreenRect,
    locals: &[Rect],
    timings: &mut PendingCaptureTimings,
) -> Result<Arc<crate::startup_bitmap::StartupBitmap>> {
    let started = Instant::now();
    let gaps = native_cache_gap_plan(bounds, locals)?;
    if originals.len() != locals.len() {
        return Err("DDB 缓存原图与物理区域数量不符".into());
    }
    for (original, local) in originals.iter().zip(locals) {
        let x = u32::try_from(i64::from(original.rect.left) - i64::from(bounds.left))
            .map_err(|_| "DDB 缓存原图横坐标越界")?;
        let y = u32::try_from(i64::from(original.rect.top) - i64::from(bounds.top))
            .map_err(|_| "DDB 缓存原图纵坐标越界")?;
        if *local
            != (Rect {
                x,
                y,
                width: original.rect.width,
                height: original.rect.height,
            })
        {
            return Err("DDB 缓存原图位置或尺寸与物理区域不符".into());
        }
    }
    timings.native_cache_gap_plan_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = Instant::now();
    let desktop = ScreenDc::new()?;
    timings.desktop_dc_ms = started.elapsed().as_secs_f64() * 1000.;
    let started = Instant::now();
    let cache = DdbCaptureSurface::new(desktop.0, bounds)?;
    timings.native_cache_create_ms = started.elapsed().as_secs_f64() * 1000.;
    let composited = (|| -> Result<()> {
        let started = Instant::now();
        for gap in &gaps.gaps {
            native_call("PatBlt(device-native-cache-gap-black)", unsafe {
                SetLastError(WIN32_ERROR(0));
                PatBlt(
                    cache.dc,
                    gap.x as i32,
                    gap.y as i32,
                    gap.width as i32,
                    gap.height as i32,
                    BLACKNESS,
                )
                .ok()
            })?;
        }
        timings.original_clear_ms = started.elapsed().as_secs_f64() * 1000.;
        timings.native_cache_gap_clear = Some(NativeCacheGapClear {
            rectangles: gaps.gaps.len(),
            pixels: gaps.pixels,
        });
        let started = Instant::now();
        for (original, local) in originals.iter().zip(locals) {
            let selected = SelectedDeviceDdb::new(desktop.0, original.bitmap())?;
            let copied = native_call("BitBlt(device-native-cache)", unsafe {
                SetLastError(WIN32_ERROR(0));
                BitBlt(
                    cache.dc,
                    local.x as i32,
                    local.y as i32,
                    local.width as i32,
                    local.height as i32,
                    Some(selected.dc),
                    0,
                    0,
                    SRCCOPY,
                )
            });
            // 清理失败优先返回且不可回退；只有明确反选/删 DC 后才能 CPU 读回。
            selected.finish()?;
            copied?;
        }
        timings.native_cache_copy_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        native_call("GdiFlush(device-native-cache)", unsafe { GdiFlush().ok() })?;
        timings.native_cache_flush_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok(())
    })();
    if let Err(error) = composited {
        let cache_released = cache.finish_checked();
        let desktop_released = desktop.release_checked();
        cache_released?;
        desktop_released?;
        return Err(error);
    }
    let started = Instant::now();
    let bitmap = cache.detach()?;
    let native = Arc::new(unsafe {
        // 只清黑 regions 联合区域之外的空隙；所有屏内像素已由同次原图 SRCCOPY 覆盖。
        // 两部分完成 flush 后整个缓存均已初始化，空隙不能含未定义像素。
        crate::startup_bitmap::StartupBitmap::from_deselected_ddb_with_black_gaps(
            bitmap,
            bounds,
            locals.to_vec(),
        )?
    });
    timings.native_cache_detach_validate_ms = started.elapsed().as_secs_f64() * 1000.;
    desktop.release_checked()?;
    Ok(native)
}

struct SelectedDeviceDdb {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    _thread_bound: PhantomData<Rc<()>>,
}
impl SelectedDeviceDdb {
    fn new(compatible: HDC, bitmap: HBITMAP) -> Result<Self> {
        let dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateCompatibleDC(Some(compatible))
        };
        if dc.is_invalid() {
            return Err(NativeCaptureError::last("CreateCompatibleDC(device-cache-source)").into());
        }
        let mut owner = Self {
            dc,
            bitmap,
            previous: HGDIOBJ::default(),
            _thread_bound: PhantomData,
        };
        owner.previous = unsafe {
            SetLastError(WIN32_ERROR(0));
            SelectObject(dc, bitmap.into())
        };
        if owner.previous.is_invalid()
            || unsafe { GetCurrentObject(dc, OBJ_BITMAP) } != bitmap.into()
        {
            let error = NativeCaptureError::last("SelectObject(device-cache-source)");
            owner.finish()?;
            return Err(error.into());
        }
        Ok(owner)
    }
    fn finish(mut self) -> Result<()> {
        if !unsafe { GdiFlush() }.as_bool() {
            return Err("设备 DDB 合成 flush 失败".into());
        }
        if self.previous.is_invalid() {
            if unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } == self.bitmap.into() {
                return Err("设备 DDB 合成源 DC 保留了无法反选的原图".into());
            }
        } else {
            let displaced = unsafe { SelectObject(self.dc, self.previous) };
            if displaced != self.bitmap.into()
                || unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } != self.previous
            {
                return Err("设备 DDB 合成没有精确反选".into());
            }
            self.previous = HGDIOBJ::default();
        }
        if !unsafe { DeleteDC(self.dc) }.as_bool() {
            return Err("设备 DDB 合成源 DC 释放失败".into());
        }
        self.dc = HDC::default();
        Ok(())
    }
}
impl Drop for SelectedDeviceDdb {
    fn drop(&mut self) {
        if !self.dc.is_invalid() {
            unsafe {
                let _ = GdiFlush();
            }
            if !self.previous.is_invalid() {
                unsafe {
                    let _ = SelectObject(self.dc, self.previous);
                }
            }
            if !unsafe { DeleteDC(self.dc) }.as_bool() {
                eprintln!(
                    "{{\"phase\":\"startup_device_ddb_select_cleanup_error\",\"pid\":{}}}",
                    std::process::id()
                );
            }
        }
    }
}

struct ReadbackDc(HDC);
impl ReadbackDc {
    fn new() -> Result<Self> {
        let dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateCompatibleDC(None)
        };
        if dc.is_invalid() {
            return Err(NativeCaptureError::last("CreateCompatibleDC(readback)").into());
        }
        Ok(Self(dc))
    }
    fn finish(mut self) -> Result<()> {
        if !unsafe { DeleteDC(self.0) }.as_bool() {
            return Err(NativeCaptureError::last("DeleteDC(readback)").into());
        }
        self.0 = HDC::default();
        Ok(())
    }
}
impl Drop for ReadbackDc {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !unsafe { DeleteDC(self.0) }.as_bool() {
            eprintln!("{{\"phase\":\"startup_ddb_readback_cleanup_error\",\"pid\":{},\"dc_deleted\":false}}",std::process::id());
        }
    }
}

/// 自有 480x320 图案的同源 DDB 专项，不创建窗口/GPUI、不读取桌面、不派发输入。
/// 原生缓存的保留字节 alpha 不作格式保证；独立核全部 BGR，CPU 输出仍严格 A255。
pub(crate) fn verify_pending_ddb_fixture() -> Result<()> {
    let _dpi = PhysicalDpi::new()?;
    let bounds = ScreenRect {
        left: -37,
        top: 19,
        width: 480,
        height: 320,
    };
    let expected = |x: u32, y: u32| -> [u8; 4] {
        [
            (30 + (x * 7 + y * 3) % 171) as u8,
            (40 + (x * 3 + y * 11) % 151) as u8,
            (50 + (x * 13 + y * 5) % 131) as u8,
            255,
        ]
    };
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(image_bytes(bounds.width, bounds.height)?)?;
    for y in 0..bounds.height {
        for x in 0..bounds.width {
            pixels.extend_from_slice(&expected(x, y));
        }
    }
    let fixture = FrozenDesktop::from_screens(
        vec![FrozenScreen::from_pixels(bounds, pixels)?],
        POINT {
            x: bounds.left + 5,
            y: bounds.top + 5,
        },
        0.,
    )?;
    let pending = PendingStartupCapture::from_fixture(fixture)?;
    let original = pending.original.as_ref().ok_or("fixture 原 DDB 缺失")?;
    let original_id = original.object_id();
    let capture_id = original.capture_id;
    let native = pending.native_bitmap();
    let native_id = native.object_id();
    if original_id == native_id
        || native.kind() != "ddb"
        || native.regions() != [Rect::full(480, 320)]
    {
        return Err("自有 DDB 源、独立缓存或区域身份不符".into());
    }
    let desktop = ScreenDc::new()?;
    let destination = DibSurface::new(desktop.0, bounds.width, bounds.height)?;
    native.blit_to(
        destination.dc,
        &[(Rect::full(480, 320), Rect::full(480, 320))],
    )?;
    let raw = unsafe {
        std::slice::from_raw_parts(destination.bits, image_bytes(bounds.width, bounds.height)?)
    };
    for y in 0..bounds.height {
        for x in 0..bounds.width {
            let start = (y as usize * bounds.width as usize + x as usize) * 4;
            let actual = [raw[start], raw[start + 1], raw[start + 2], 255];
            if actual != expected(x, y) {
                return Err(format!("自有 DDB 原生缓存像素不符 ({x},{y})").into());
            }
        }
    }
    destination.finish_checked()?;
    desktop.release_checked()?;
    let cpu = pending.finish()?;
    if cpu.bounds != bounds || cpu.current != 0 || cpu.screens.len() != 1 {
        return Err("自有 DDB CPU 元数据不符".into());
    }
    let bytes = cpu.screens[0]
        .preview
        .as_bytes(0)
        .ok_or("自有 DDB CPU 图像不存在")?;
    if bytes.len() != image_bytes(bounds.width, bounds.height)? {
        return Err("自有 DDB CPU 长度不符".into());
    }
    for y in 0..bounds.height {
        for x in 0..bounds.width {
            let start = (y as usize * bounds.width as usize + x as usize) * 4;
            if bytes[start..start + 4] != expected(x, y) {
                return Err(format!("自有 DDB CPU/BGRA/方向不符 ({x},{y})").into());
            }
        }
    }
    let actual_blits = native.blit_count();
    if actual_blits != 1 {
        return Err("自有 DDB 实际原生缓存绘制次数不符".into());
    }
    drop(cpu);
    drop(native);
    println!(
        "{}",
        serde_json::json!({"phase":"startup_ddb_fixture_complete","pid":std::process::id(),
        "width":480,"height":320,"source_bytes":614400,"pixels":153600,
        "capture_id":capture_id.to_string(),"original_capture_bitmap_id":original_id.to_string(),
        "native_cache_bitmap_id":native_id.to_string(),"cpu_all_pixels_verified":true,
        "native_cache_all_pixels_verified":true,"cpu_alpha_opaque":true,"top_down_verified":true,
        "native_cache_independent":true,"actual_dib_destination_released":true,"screen_dc_released":true,
        "source_same_capture":true,"native_cache_actual_blits":actual_blits,
        "native_reserved_alpha_ignored":true,"native_bgr_only_verified":true,
        "native_cache_bitmap_kind":"ddb","screen_capture_calls":0,
        "window_created":false,"gpui_created":false,"gpui_application_created":false,
        "cpu_render_image_created":true,"global_input":false,"fixture_only":true,
        "actual_ready_verified":false})
    );
    verify_device_ddb_fixture(false)?;
    verify_device_ddb_fixture(true)?;
    Ok(())
}

fn device_fixture_pixel(x: i32, y: i32) -> [u8; 4] {
    [
        (30 + (x + 97).rem_euclid(171)) as u8,
        (40 + (x * 3 + y * 11 + 503).rem_euclid(151)) as u8,
        (50 + (x * 13 + y * 5 + 701).rem_euclid(131)) as u8,
        255,
    ]
}

/// 自有图案上传；不 BitBlt 桌面，HDC 全程留在构造线程，只转移反选后的 DDB。
fn device_fixture_ddb(rect: ScreenRect) -> Result<DetachedDeviceDdb> {
    let _dpi = PhysicalDpi::new()?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(image_bytes(rect.width, rect.height)?)?;
    for y in 0..rect.height {
        for x in 0..rect.width {
            let mut pixel = device_fixture_pixel(rect.left + x as i32, rect.top + y as i32);
            pixel[3] = (x + y * 13) as u8;
            pixels.extend_from_slice(&pixel);
        }
    }
    let desktop = ScreenDc::new()?;
    let surface = DdbCaptureSurface::new(desktop.0, rect)?;
    let info = top_down_info(rect.width, rect.height);
    let rows = unsafe {
        SetLastError(WIN32_ERROR(0));
        StretchDIBits(
            surface.dc,
            0,
            0,
            rect.width as i32,
            rect.height as i32,
            0,
            0,
            rect.width as i32,
            rect.height as i32,
            Some(pixels.as_ptr().cast()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
    if rows != rect.height as i32 {
        return Err(NativeCaptureError::last("StretchDIBits(device-fixture)").into());
    }
    native_call("GdiFlush(device-fixture)", unsafe { GdiFlush().ok() })?;
    let bitmap = surface.detach()?;
    let original = DetachedDeviceDdb {
        bitmap_id: bitmap.0 as usize,
        rect,
        capture_id: 0,
        native_id: 0,
        readback_completed: false,
        timing: None,
    };
    desktop.release_checked()?;
    Ok(original)
}

fn verify_device_ddb_fixture(force_cpu: bool) -> Result<()> {
    let started = Instant::now();
    let monitors = vec![
        ScreenRect {
            left: -37,
            top: 19,
            width: 120,
            height: 320,
        },
        ScreenRect {
            left: 83,
            top: 79,
            width: 360,
            height: 260,
        },
    ];
    let (bounds, total, locals) = pending_layout(&monitors)?;
    if compact_desktop_bytes(bounds, total).is_none() {
        return Err("多屏 fixture 不符合真实紧凑预算".into());
    }
    let worker_rect = monitors[1];
    let worker = std::thread::Builder::new()
        .name("rustshot-device-ddb-fixture".into())
        .spawn(move || device_fixture_ddb(worker_rect).map_err(sendable_device_error))?;
    let first = device_fixture_ddb(monitors[0]);
    // 即使主线程构造失败也必须汇合另一个线程，避免遗失其独立 GDI 资源。
    let second = worker.join().map_err(|_| "多屏 DDB fixture 线程意外终止")?;
    let originals = vec![first?, second.map_err(|error| -> Box<dyn Error> { error })?];
    let ids: Vec<_> = originals
        .iter()
        .map(|original| original.bitmap_id)
        .collect();
    let pending = PendingStartupCapture::from_device_ddbs(
        originals,
        bounds,
        monitors.clone(),
        locals.clone(),
        POINT { x: 100, y: 100 },
        crate::capture_standby::qpc(),
        started,
        false,
        true,
        "device-fixture",
        None,
        force_cpu,
    )?;
    let native = pending.native_bitmap();
    if ids.contains(&native.object_id())
        || native.regions() != locals
        || native.kind() != if force_cpu { "dib" } else { "ddb" }
        || native.whole_canvas_black_gaps_valid() == force_cpu
    {
        return Err("多屏 DDB fixture 独立缓存身份/区域或回退种类不符".into());
    }
    let desktop = ScreenDc::new()?;
    let destination = DibSurface::new(desktop.0, bounds.width, bounds.height)?;
    native_call("PatBlt(device-fixture-rejection-sentinel)", unsafe {
        PatBlt(
            destination.dc,
            0,
            0,
            bounds.width as i32,
            bounds.height as i32,
            WHITENESS,
        )
        .ok()
    })?;
    // 任意 from_pixels 的屏间字节不能冒称黑色；拒绝发生在 DC 选择和复制之前。
    let unknown_gaps = crate::startup_bitmap::StartupBitmap::from_pixels(
        bounds,
        locals.clone(),
        &vec![0x7c; image_bytes(bounds.width, bounds.height)?],
    )?;
    let full = Rect::full(bounds.width, bounds.height);
    if unknown_gaps.whole_canvas_black_gaps_valid()
        || unknown_gaps
            .blit_full_canvas_to(destination.dc, bounds)
            .is_ok()
        || [
            ScreenRect {
                left: bounds.left + 1,
                ..bounds
            },
            ScreenRect {
                width: bounds.width + 1,
                ..bounds
            },
            ScreenRect {
                height: bounds.height + 1,
                ..bounds
            },
        ]
        .iter()
        .any(|viewport| {
            native
                .blit_full_canvas_to(destination.dc, *viewport)
                .is_ok()
        })
        || native.blit_to(destination.dc, &[(full, full)]).is_ok()
        || native.blit_count() != 0
        || unknown_gaps.blit_count() != 0
    {
        return Err("整画布能力/错误视口拒绝或原区域校验不符".into());
    }
    native_call("GdiFlush(device-fixture-rejection-sentinel)", unsafe {
        GdiFlush().ok()
    })?;
    let rejected_pixels = unsafe {
        std::slice::from_raw_parts(destination.bits, image_bytes(bounds.width, bounds.height)?)
    };
    if rejected_pixels
        .chunks_exact(4)
        .any(|pixel| pixel[..3] != [255, 255, 255])
    {
        return Err("拒绝整画布复制时改动了目标哨兵".into());
    }
    drop(unknown_gaps);
    native_call("PatBlt(device-fixture-destination-sentinel)", unsafe {
        PatBlt(
            destination.dc,
            0,
            0,
            bounds.width as i32,
            bounds.height as i32,
            if force_cpu { BLACKNESS } else { WHITENESS },
        )
        .ok()
    })?;
    if force_cpu {
        if native.blit_full_canvas_to(destination.dc, bounds).is_ok() {
            return Err("没有来源能力的 CPU 缓存错误允许整画布复制".into());
        }
        native.blit_to(
            destination.dc,
            &locals.iter().map(|rect| (*rect, *rect)).collect::<Vec<_>>(),
        )?;
    } else {
        // 不先清黑目标；随后逐像素检查必须证明空隙黑色来自该冻结缓存。
        native.blit_full_canvas_to(destination.dc, bounds)?;
    }
    let expected_blits = if force_cpu { locals.len() } else { 1 };
    if native.blit_count() != expected_blits
        || native.last_batch_planes() != expected_blits
        || native.batch_count() != 1
    {
        return Err("多屏缓存实际 BitBlt 数或 batch 回执不符".into());
    }
    let raw = unsafe {
        std::slice::from_raw_parts(destination.bits, image_bytes(bounds.width, bounds.height)?)
    };
    for y in 0..bounds.height {
        for x in 0..bounds.width {
            let gx = bounds.left + x as i32;
            let gy = bounds.top + y as i32;
            let expected = if monitors.iter().any(|rect| {
                gx >= rect.left
                    && gy >= rect.top
                    && gx < rect.left + rect.width as i32
                    && gy < rect.top + rect.height as i32
            }) {
                device_fixture_pixel(gx, gy)
            } else {
                [0, 0, 0, 255]
            };
            let offset = (y as usize * bounds.width as usize + x as usize) * 4;
            if raw[offset..offset + 3] != expected[..3] {
                return Err(format!("多屏缓存/空隙 BGR 不符 ({gx},{gy})").into());
            }
        }
    }
    if !force_cpu {
        // 上面的逐像素 oracle 已核定这份纯自有原图与空隙；生产 draw 改写同一独占目标。
        let canonical_bgra = raw.to_vec();
        unsafe {
            crate::startup_surface::verify_whole_canvas_paint_fixture(
                destination.dc,
                destination.bitmap,
                destination.bits,
                &native,
                bounds,
                &locals,
                &canonical_bgra,
            )?;
        }
    }
    destination.finish_checked()?;
    desktop.release_checked()?;
    let cpu = pending.finish()?;
    if cpu.current != 1 || cpu.bounds != bounds || cpu.screens.len() != monitors.len() {
        return Err("多屏 DDB fixture CPU 元数据不符".into());
    }
    let image = cpu.read_region(Rect::full(bounds.width, bounds.height))?;
    for y in 0..bounds.height {
        for x in 0..bounds.width {
            let gx = bounds.left + x as i32;
            let gy = bounds.top + y as i32;
            let expected = if monitors.iter().any(|rect| {
                gx >= rect.left
                    && gy >= rect.top
                    && gx < rect.left + rect.width as i32
                    && gy < rect.top + rect.height as i32
            }) {
                device_fixture_pixel(gx, gy)
            } else {
                [0, 0, 0, 255]
            };
            let offset = (y as usize * bounds.width as usize + x as usize) * 4;
            if image.bgra[offset..offset + 4] != expected {
                return Err(format!("多屏 CPU 读回/方向/alpha/空隙不符 ({gx},{gy})").into());
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "phase":"startup_device_ddb_fixture_complete","pid":std::process::id(),
            "force_same_source_cpu_fallback":force_cpu,"native_bitmap_kind":native.kind(),
            "whole_canvas_black_gaps_valid":native.whole_canvas_black_gaps_valid(),
            "native_bitmap_blits":native.blit_count(),"native_bitmap_batches":native.batch_count(),
            "native_bitmap_last_batch_planes":native.last_batch_planes(),"logical_monitor_planes":locals.len(),
            "source_same_capture":true,"cpu_all_pixels_verified":true,"native_all_bgr_and_gaps_verified":true,
            "negative_origin":true,"portrait_and_landscape_shapes":true,"cpu_alpha_opaque":true,
            "detached_bitmap_transferred_from_worker":true,"all_workers_joined":true,
            "screen_capture_calls":0,"window_created":false,"gpui_created":false,"global_input":false,
            "actual_ready_verified":false
        })
    );
    Ok(())
}

pub struct FrozenScreen {
    /// 显示器的全局物理坐标；屏幕旋转已由 GDI 合成，不再次旋转像素。
    pub rect: ScreenRect,
    pub preview: Arc<RenderImage>,
}
impl FrozenScreen {
    fn from_pixels(rect: ScreenRect, bgra: Vec<u8>) -> Result<Self> {
        if bgra.len() != image_bytes(rect.width, rect.height)? {
            return Err("冻结屏幕像素长度与尺寸不符".into());
        }
        let buffer =
            image::RgbaImage::from_raw(rect.width, rect.height, bgra).ok_or("冻结屏幕尺寸无效")?;
        Ok(Self {
            rect,
            preview: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        })
    }
}

pub struct FrozenDesktop {
    pub screens: Vec<FrozenScreen>,
    pub bounds: ScreenRect,
    pub current: usize,
    pub capture_ms: f64,
}
impl FrozenDesktop {
    pub fn capture() -> Result<Self> {
        Self::capture_with_bitmap(false).map(|(desktop, _)| desktop)
    }
    pub(crate) fn capture_for_startup(
    ) -> Result<(Self, Option<Arc<crate::startup_bitmap::StartupBitmap>>)> {
        Self::capture_with_bitmap(true)
    }
    fn capture_with_bitmap(
        retain_startup_bitmap: bool,
    ) -> Result<(Self, Option<Arc<crate::startup_bitmap::StartupBitmap>>)> {
        let _primed_cleanup = PrimedCaptureCleanup;
        let started = Instant::now();
        // 此方法也在准备线程使用，显式保证枚举和 BitBlt 都使用物理坐标。
        let _dpi = PhysicalDpi::new()?;
        let monitors = monitor_rects()?;
        let bounds = ScreenRect::union(&monitors).ok_or("虚拟桌面尺寸无效")?;
        if bounds.width > MAX_CANVAS_SIDE || bounds.height > MAX_CANVAS_SIDE {
            return Err("虚拟桌面超过 GPUI 16384 像素单边限制".into());
        }
        let mut total = 0_usize;
        for rect in &monitors {
            total = total
                .checked_add(image_bytes(rect.width, rect.height)?)
                .ok_or("冻结桌面像素长度溢出")?;
            if total > MAX_FROZEN_BYTES {
                return Err("冻结桌面超过 256 MiB 像素上限".into());
            }
        }
        let mut cursor = POINT::default();
        native_call("GetCursorPos", unsafe { GetCursorPos(&mut cursor) })?;
        let trace = std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
        let freeze_begin_qpc = trace.then(crate::capture_standby::qpc);
        // 普通多屏按原布局和像素预算自动选择；显式诊断可强制 parallel。
        // 默认多屏使用真正的显示设备 DC；诊断仍可指定旧 compact/parallel。
        let requested_capture_strategy =
            match std::env::var("RUSTSHOT_GDI_CAPTURE_STRATEGY").as_deref() {
                Ok("compact") => "compact",
                Ok("parallel") => "parallel",
                _ => "auto",
            };
        let compact_requested = requested_capture_strategy == "compact";
        let compact_bytes = if requested_capture_strategy != "parallel" {
            compact_desktop_bytes(bounds, total)
        } else {
            None
        };
        let mut device_timings = None;
        let mut device_dispatch_timings = None;
        let mut device_worker_timings = None;
        let (captures, desktop_timing, startup_bitmap, capture_strategy) = if monitors.len() == 1 {
            // 单屏不增加线程启动成本；多屏不把这个线程的 HDC 传给工作线程。
            let desktop_dc = capture_desktop_dc()?;
            (
                vec![capture_screen_timed(desktop_dc.0, monitors[0], trace)?],
                None,
                None,
                "single-monitor-bitblt",
            )
        } else if requested_capture_strategy == "auto" {
            let stage_started = trace.then(Instant::now);
            let devices = monitor_devices()?;
            let monitor_devices_ms =
                stage_started.map(|started| started.elapsed().as_secs_f64() * 1000.);
            if devices.iter().map(|device| device.rect).collect::<Vec<_>>() != monitors {
                return Err("显示设备名称与物理屏幕枚举次序不符".into());
            }
            // 全桌面 DC 不借给设备线程；在当前创建线程释放已清黑的预热资源。
            let stage_started = trace.then(Instant::now);
            discard_primed_gdi();
            let primed_discard_ms =
                stage_started.map(|started| started.elapsed().as_secs_f64() * 1000.);
            let stage_started = trace.then(Instant::now);
            let (captures, timings, worker_timing) = capture_devices_parallel(&devices, trace)?;
            let capture_devices_parallel_ms =
                stage_started.map(|started| started.elapsed().as_secs_f64() * 1000.);
            if let (Some(enumeration), Some(discard), Some(capture)) = (
                monitor_devices_ms,
                primed_discard_ms,
                capture_devices_parallel_ms,
            ) {
                device_dispatch_timings = Some([enumeration, discard, capture]);
            }
            device_timings = Some(timings);
            device_worker_timings = Some(worker_timing);
            (captures, None, None, "device-per-monitor-bitblt")
        } else if let Some(bytes) = compact_bytes {
            // 显式 compact 仍受原紧凑布局和像素预算约束。
            // 普通采集在 Vec 初始化后释放 DIB；首屏会话可继续只读持有原位图。
            // 屏间空隙不进入任何持久像素 plane，也不能从原位图作为 monitor 区域读取。
            let (captures, timing, bitmap) =
                capture_screens_compact(&monitors, bounds, bytes, trace, retain_startup_bitmap)?;
            (captures, timing, bitmap, "one-virtual-bitblt")
        } else {
            (
                capture_screens_parallel(&monitors, trace)?,
                None,
                None,
                "parallel-per-monitor-bitblt",
            )
        };
        let mut screens = Vec::with_capacity(monitors.len());
        let mut timings = trace.then(|| Vec::with_capacity(monitors.len()));
        for (screen, timing) in captures {
            screens.push(screen);
            if let (Some(timings), Some(timing)) = (timings.as_mut(), timing) {
                timings.push(timing);
            }
        }
        let frozen = Self::from_screens(screens, cursor, started.elapsed().as_secs_f64() * 1000.)?;
        let freeze_end_qpc = trace.then(crate::capture_standby::qpc);
        // 所有屏幕像素均已冻结再输出；stdout 不进入逐屏抓取/复制计量。
        if trace {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_gdi_freeze_interval", "pid":std::process::id(),
                "capture_strategy":capture_strategy,
                "begin_qpc":freeze_begin_qpc.map(|qpc|qpc.to_string()),
                "end_qpc":freeze_end_qpc.map(|qpc|qpc.to_string()),
                "all_screen_pixels_frozen_before_interval_end":true,
                "timing_scope":"capture and CPU-plane preparation, including device enumeration and final device revalidation; excludes initial monitor_rects and trace I/O"
            }));
            crate::startup_probe::emit_trace_args(format_args!(
                "{{\"phase\":\"startup_gdi_strategy\",\"pid\":{},\"capture_strategy\":\"{capture_strategy}\",\"requested_capture_strategy\":\"{requested_capture_strategy}\",\"screens\":{},\"compact_diagnostic_requested\":{compact_requested}}}",
                std::process::id(), monitors.len(),
            ));
        }
        if let Some([monitor_devices_ms, primed_discard_ms, capture_devices_parallel_ms]) =
            device_dispatch_timings
        {
            crate::startup_probe::emit_trace_value(&serde_json::json!({
                "phase":"startup_gdi_device_dispatch", "pid":std::process::id(),
                "monitor_devices_ms":monitor_devices_ms,
                "primed_discard_ms":primed_discard_ms,
                "capture_devices_parallel_ms":capture_devices_parallel_ms,
                "timing_scope":"disjoint wall stages; parallel capture includes device stages, thread launch/join and final device revalidation; do not sum with per-screen timings"
            }));
        }
        if let Some(timings) = device_timings {
            for (index, timing) in timings.into_iter().enumerate() {
                if let Some(timing) = timing {
                    timing.print(index);
                }
            }
        }
        if trace {
            if let Some(timing) = device_worker_timings {
                timing.print();
            }
        }
        if let Some(timing) = desktop_timing {
            timing.print();
        }
        if let Some(timings) = timings {
            for (index, timing) in timings.into_iter().enumerate() {
                timing.print(index);
            }
        }
        Ok((frozen, startup_bitmap))
    }
    fn from_screens(screens: Vec<FrozenScreen>, cursor: POINT, capture_ms: f64) -> Result<Self> {
        let areas: Vec<_> = screens.iter().map(|s| s.rect).collect();
        let bounds = ScreenRect::union(&areas).ok_or("未找到活动显示器")?;
        let current = areas
            .iter()
            .position(|r| {
                let x = cursor.x as i64;
                let y = cursor.y as i64;
                x >= r.left as i64
                    && y >= r.top as i64
                    && x < r.left as i64 + r.width as i64
                    && y < r.top as i64 + r.height as i64
            })
            .unwrap_or(0);
        Ok(Self {
            screens,
            bounds,
            current,
            capture_ms,
        })
    }
    pub fn full(&self) -> Rect {
        Rect::full(self.bounds.width, self.bounds.height)
    }
    pub fn screen_rect(&self, index: usize) -> Rect {
        self.screens[index].rect.local(self.bounds)
    }
    pub fn monitor_at(&self, point: (i32, i32)) -> usize {
        (0..self.screens.len())
            .find(|&i| self.screen_rect(i).contains(point))
            .unwrap_or(self.current)
    }
    pub fn read_region(&self, rect: Rect) -> Result<Image> {
        if !rect.valid(self.bounds.width, self.bounds.height) {
            return Err("裁剪范围超出冻结桌面".into());
        }
        image_bytes(rect.width, rect.height)?;
        let started = Instant::now();
        // 仅分配请求的选区；屏间空隙遵循既有黑色不透明桌面语义。
        let mut image = SourcePixels {
            fill: None,
            canvas: self.full(),
            overlay: None,
            planes: self
                .screens
                .iter()
                .enumerate()
                .map(|(i, screen)| PixelPlane {
                    rect: self.screen_rect(i),
                    bgra: screen.preview.as_bytes(0).expect("frozen screen pixels"),
                })
                .collect(),
        }
        .read(rect);
        image.readback_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok(image)
    }
}

/// 空隙最多占显示器总面积的四分之一；稀疏布局保留原逐屏路径。
/// 临时虚拟桌面 DIB 和最终各屏像素各自都遵守原 256 MiB 上限。
fn compact_desktop_bytes(bounds: ScreenRect, screen_bytes: usize) -> Option<usize> {
    let bytes = image_bytes(bounds.width, bounds.height).ok()?;
    let compact_limit = screen_bytes.checked_add(screen_bytes / 4)?;
    (screen_bytes > 0 && screen_bytes <= MAX_FROZEN_BYTES && bytes <= compact_limit)
        .then_some(bytes)
}

struct PlaneCopyPlan {
    rect: ScreenRect,
    source_bytes: usize,
    source_stride: usize,
    source_start: usize,
    row_bytes: usize,
    bytes: usize,
}
impl PlaneCopyPlan {
    fn new(bounds: ScreenRect, rect: ScreenRect) -> Result<Self> {
        let source_bytes = image_bytes(bounds.width, bounds.height)?;
        let bytes = image_bytes(rect.width, rect.height)?;
        let x = u32::try_from(rect.left as i64 - bounds.left as i64)
            .map_err(|_| "冻结屏幕横坐标超出虚拟桌面")?;
        let y = u32::try_from(rect.top as i64 - bounds.top as i64)
            .map_err(|_| "冻结屏幕纵坐标超出虚拟桌面")?;
        if !(Rect {
            x,
            y,
            width: rect.width,
            height: rect.height,
        })
        .valid(bounds.width, bounds.height)
        {
            return Err("冻结屏幕超出虚拟桌面".into());
        }
        let source_stride = bounds.width as usize * 4;
        let row_bytes = rect.width as usize * 4;
        let source_start = y as usize * source_stride + x as usize * 4;
        // image_bytes 已检查总长度；局部 Rect 完整包含，末行也必须完整可读。
        let end = source_start
            .checked_add((rect.height as usize - 1) * source_stride)
            .and_then(|start| start.checked_add(row_bytes))
            .ok_or("冻结屏幕拆分长度溢出")?;
        if end > source_bytes {
            return Err("冻结屏幕拆分超出源缓冲区".into());
        }
        Ok(Self {
            rect,
            source_bytes,
            source_stride,
            source_start,
            row_bytes,
            bytes,
        })
    }

    /// 所有错误门先于首写；每行保留 BGR 并规范 alpha，不复制屏间空隙。
    /// source 指向 source_bytes 长的原生分配，与 target 不重叠；
    /// 本屏幕各行已在 GdiFlush 后可读，屏间空隙不要求作为像素初始化。
    unsafe fn initialize(
        &self,
        source: *const u8,
        source_bytes: usize,
        target: &mut [MaybeUninit<u8>],
    ) -> Result<()> {
        if source.is_null() || source_bytes != self.source_bytes || target.len() != self.bytes {
            return Err("冻结屏幕拆分的源或目标长度无效".into());
        }
        for (row, target_row) in target.chunks_exact_mut(self.row_bytes).enumerate() {
            let start = self.source_start + row * self.source_stride;
            // 不为屏间未绘制的空隙构造 &[u8]；只读取已捕获的显示器行。
            let source_row = std::slice::from_raw_parts(source.add(start), self.row_bytes);
            initialize_opaque_bgra(source_row, target_row);
        }
        Ok(())
    }
}

struct DesktopTiming {
    timing: ScreenTiming,
    screen_count: usize,
    surface_bytes: usize,
    screen_bytes: usize,
}
impl DesktopTiming {
    fn print(&self) {
        crate::startup_probe::emit_trace_args(format_args!(
            "{{\"phase\":\"startup_gdi_desktop\",\"pid\":{},\"capture_strategy\":\"one-virtual-bitblt\",\"screens\":{},\"width\":{},\"height\":{},\"surface_bytes\":{},\"screen_bytes\":{},\"pixel_allocate_ms\":{:.3},\"dib_allocate_ms\":{:.3},\"bitblt_ms\":{:.3},\"gdi_flush_ms\":{:.3},\"pixel_copy_alpha_ms\":{:.3},\"resource_drop_ms\":{:.3}}}",
            std::process::id(), self.screen_count, self.timing.width, self.timing.height,
            self.surface_bytes, self.screen_bytes, self.timing.durations[0],
            self.timing.durations[1], self.timing.durations[2], self.timing.durations[3],
            self.timing.durations[4], self.timing.durations[5],
        ));
    }
}

type TimedScreens = Vec<(FrozenScreen, Option<ScreenTiming>)>;
type CompactCapture = (
    TimedScreens,
    Option<DesktopTiming>,
    Option<Arc<crate::startup_bitmap::StartupBitmap>>,
);

fn capture_screens_compact(
    monitors: &[ScreenRect],
    bounds: ScreenRect,
    bytes: usize,
    trace: bool,
    retain_startup_bitmap: bool,
) -> Result<CompactCapture> {
    if bytes != image_bytes(bounds.width, bounds.height)? {
        return Err("虚拟桌面 DIB 长度不符".into());
    }
    let plans = monitors
        .iter()
        .map(|&rect| PlaneCopyPlan::new(bounds, rect))
        .collect::<Result<Vec<_>>>()?;
    let screen_bytes = plans.iter().try_fold(0_usize, |total, plan| {
        total.checked_add(plan.bytes).ok_or("冻结桌面像素长度溢出")
    })?;
    if compact_desktop_bytes(bounds, screen_bytes) != Some(bytes) {
        return Err("虚拟桌面不满足紧凑布局或像素上限".into());
    }
    let mut timing = trace.then_some(ScreenTiming {
        width: bounds.width,
        height: bounds.height,
        durations: [0.; 6],
    });
    let mut timing_ref = timing.as_mut();
    let started = screen_stage_start(&timing_ref);
    let mut buffers = Vec::with_capacity(plans.len());
    for plan in &plans {
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(plan.bytes)?;
        buffers.push(bgra);
    }
    screen_stage_end(&mut timing_ref, 0, started);
    let started = screen_stage_start(&timing_ref);
    let desktop_dc = capture_desktop_dc()?;
    let mut surface = DibSurface::new(desktop_dc.0, bounds.width, bounds.height)?;
    screen_stage_end(&mut timing_ref, 1, started);
    let started = screen_stage_start(&timing_ref);
    native_call("BitBlt", unsafe {
        SetLastError(WIN32_ERROR(0));
        BitBlt(
            surface.dc,
            0,
            0,
            bounds.width as i32,
            bounds.height as i32,
            Some(desktop_dc.0),
            bounds.left,
            bounds.top,
            // CAPTUREBLT 在部分多屏环境会触发 GreMovePointer，临时移走硬件光标。
            // 冻结桌面使用 SRCCOPY；光标由既有快照在 Ready 后独立合成。
            SRCCOPY,
        )
    })?;
    screen_stage_end(&mut timing_ref, 2, started);
    let started = screen_stage_start(&timing_ref);
    native_call("GdiFlush", unsafe {
        SetLastError(WIN32_ERROR(0));
        GdiFlush().ok()
    })?;
    screen_stage_end(&mut timing_ref, 3, started);
    let started = screen_stage_start(&timing_ref);
    let mut captures = Vec::with_capacity(plans.len());
    for (plan, mut bgra) in plans.into_iter().zip(buffers) {
        unsafe {
            plan.initialize(
                surface.bits,
                bytes,
                &mut bgra.spare_capacity_mut()[..plan.bytes],
            )?
        };
        // initialize 成功后全部目标字节都已初始化，DIB 并非最终像素所有者。
        unsafe { bgra.set_len(plan.bytes) };
        captures.push((FrozenScreen::from_pixels(plan.rect, bgra)?, None));
    }
    screen_stage_end(&mut timing_ref, 4, started);
    let started = screen_stage_start(&timing_ref);
    // 保留的只是已脱离捕获 DC 的不可变 bitmap；CPU Vec/alpha 流程完全不变。
    // 此请求内原 DIB 与最终 Vec 同时存活，各自受原 256 MiB 预算约束。
    let startup_bitmap = if retain_startup_bitmap {
        let bitmap = surface.detach_bitmap()?;
        let regions = monitors.iter().map(|rect| rect.local(bounds)).collect();
        Some(Arc::new(unsafe {
            crate::startup_bitmap::StartupBitmap::from_deselected(bitmap, bounds, regions)?
        }))
    } else {
        None
    };
    drop(surface);
    drop(desktop_dc);
    screen_stage_end(&mut timing_ref, 5, started);
    Ok((
        captures,
        timing.map(|timing| DesktopTiming {
            timing,
            screen_count: monitors.len(),
            surface_bytes: bytes,
            screen_bytes,
        }),
        startup_bitmap,
    ))
}

fn capture_screen_timed(
    dc: HDC,
    rect: ScreenRect,
    trace: bool,
) -> Result<(FrozenScreen, Option<ScreenTiming>)> {
    let mut timing = trace.then_some(ScreenTiming {
        width: rect.width,
        height: rect.height,
        durations: [0.; 6],
    });
    let screen = capture_screen(dc, rect, timing.as_mut())?;
    Ok((screen, timing))
}

fn capture_screens_parallel(
    monitors: &[ScreenRect],
    trace: bool,
) -> Result<Vec<(FrozenScreen, Option<ScreenTiming>)>> {
    let (&first_rect, _) = monitors.split_first().ok_or("没有可冻结的屏幕")?;
    std::thread::scope(|scope| {
        let mut tasks = Vec::with_capacity(monitors.len() - 1);
        let mut first_error: Option<Box<dyn Error>> = None;
        for (index, &rect) in monitors.iter().enumerate().skip(1) {
            let task = std::thread::Builder::new()
                .name(format!("rustshot-gdi-{index}"))
                .spawn_scoped(scope, move || {
                    let capture = || -> Result<_> {
                        // DPI 上下文和全部 GDI 句柄均在同一工作线程创建、使用、销毁。
                        let _dpi = PhysicalDpi::new()?;
                        let desktop_dc = ScreenDc::new()?;
                        capture_screen_timed(desktop_dc.0, rect, trace)
                    };
                    capture().map_err(sendable_device_error)
                });
            match task {
                Ok(task) => tasks.push(task),
                Err(error) => {
                    first_error = Some(error.into());
                    break;
                }
            }
        }
        let mut captures = Vec::with_capacity(monitors.len());
        // 其它屏先启动；第一屏在当前主线程消费预热 HDC，绝不跨线程转移句柄。
        let first_capture = (|| -> Result<_> {
            let desktop_dc = capture_desktop_dc()?;
            capture_screen_timed(desktop_dc.0, first_rect, trace)
        })();
        match first_capture {
            Ok(capture) => captures.push(capture),
            Err(error) => {
                if first_error.as_ref().is_none_or(|first| {
                    can_fallback(first.as_ref()) && !can_fallback(error.as_ref())
                }) {
                    first_error = Some(error);
                }
            }
        }
        // 即使某屏失败也必须等待其余屏结束；原枚举次序不受完成先后影响。
        for task in tasks {
            let error: Option<Box<dyn Error>> = match task.join() {
                Ok(Ok(capture)) => {
                    captures.push(capture);
                    None
                }
                Ok(Err(error)) => Some(error),
                Err(_) => Some("冻结屏幕工作线程意外终止".into()),
            };
            if let Some(error) = error {
                // 并发时若另一屏发生内存/线程错误，不允许较早的 API 错误触发回退。
                if first_error.as_ref().is_none_or(|first| {
                    can_fallback(first.as_ref()) && !can_fallback(error.as_ref())
                }) {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            // 已完成的屏幕缓冲也在这里丢弃，DXGI 回退之前不留下 GDI 线程或像素。
            Err(error)
        } else {
            Ok(captures)
        }
    })
}

// 每个设备名直接来自本次 MONITORINFOEX；名称和矩形序列都在冻结后再次核对。
#[derive(Clone, PartialEq, Eq)]
struct MonitorDevice {
    rect: ScreenRect,
    name: [u16; 32],
}
fn monitor_devices() -> Result<Vec<MonitorDevice>> {
    #[derive(Default)]
    struct State {
        devices: Vec<MonitorDevice>,
        error: Option<NativeCaptureError>,
        invalid: bool,
    }
    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let state = &mut *(data.0 as *mut State);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        SetLastError(WIN32_ERROR(0));
        if !GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            state.error = Some(NativeCaptureError::last("GetMonitorInfoW(device)"));
            return windows::core::BOOL(0);
        }
        let r = info.monitorInfo.rcMonitor;
        let width = r.right as i64 - r.left as i64;
        let height = r.bottom as i64 - r.top as i64;
        if width <= 0
            || height <= 0
            || width > MAX_CANVAS_SIDE as i64
            || height > MAX_CANVAS_SIDE as i64
            || info.szDevice[0] == 0
            || !info.szDevice.contains(&0)
        {
            state.invalid = true;
            return windows::core::BOOL(0);
        }
        let device = MonitorDevice {
            rect: ScreenRect {
                left: r.left,
                top: r.top,
                width: width as u32,
                height: height as u32,
            },
            name: info.szDevice,
        };
        if !state
            .devices
            .iter()
            .any(|previous| previous.rect == device.rect)
        {
            state.devices.push(device);
        }
        windows::core::BOOL(1)
    }
    let mut state = State::default();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM((&mut state as *mut State) as isize),
        )
    };
    if let Some(error) = state.error {
        return Err(error.into());
    }
    if state.invalid {
        return Err("显示设备名称或物理范围无效".into());
    }
    if !ok.as_bool() {
        return Err(NativeCaptureError::last("EnumDisplayMonitors(device)").into());
    }
    if state.devices.is_empty() {
        return Err("没有活动显示设备".into());
    }
    Ok(state.devices)
}

// CreateDCW 和其 DeleteDC 始终在同一线程；不使用或转移待命的桌面 HDC。
struct DeviceDc {
    dc: HDC,
    _thread_bound: PhantomData<Rc<()>>,
}
impl DeviceDc {
    fn new(device: &MonitorDevice) -> Result<Self> {
        let dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateDCW(
                windows::core::w!("DISPLAY"),
                windows::core::PCWSTR(device.name.as_ptr()),
                windows::core::PCWSTR::null(),
                None,
            )
        };
        if dc.is_invalid() {
            return Err(NativeCaptureError::last("CreateDCW(monitor)").into());
        }
        Ok(Self {
            dc,
            _thread_bound: PhantomData,
        })
    }
    fn finish(mut self) -> Result<()> {
        if !unsafe { DeleteDC(self.dc) }.as_bool() {
            // 清理失败必须终止本次请求；不能冒充后端不可用而重新截图。
            let error = NativeCaptureError::last("DeleteDC(monitor)");
            return Err(format!("显示设备上下文释放失败：{error}").into());
        }
        self.dc = HDC::default();
        Ok(())
    }
}
impl Drop for DeviceDc {
    fn drop(&mut self) {
        if !self.dc.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

// 成功与错误路径都精确反选、先删 DC 再删 bitmap；失败仍保留句柄给 Drop 重试。
struct DeviceDib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
}
impl DeviceDib {
    fn new(device: HDC, width: u32, height: u32) -> Result<Self> {
        let mut surface = DibSurface::new(device, width, height)?;
        let owner = Self {
            dc: std::mem::take(&mut surface.dc),
            bitmap: std::mem::take(&mut surface.bitmap),
            previous: std::mem::take(&mut surface.old),
            bits: surface.bits,
        };
        std::mem::forget(surface);
        Ok(owner)
    }
    fn release(&mut self) -> bool {
        let restored = if self.previous.is_invalid() || self.dc.is_invalid() {
            true
        } else {
            let displaced = unsafe { SelectObject(self.dc, self.previous) };
            let restored = displaced == self.bitmap.into()
                && unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } == self.previous;
            if restored {
                self.previous = HGDIOBJ::default();
            }
            restored
        };
        let dc_deleted = self.dc.is_invalid() || unsafe { DeleteDC(self.dc) }.as_bool();
        if dc_deleted {
            self.dc = HDC::default();
            self.previous = HGDIOBJ::default();
        }
        let bitmap_deleted = self.bitmap.is_invalid()
            || (dc_deleted && unsafe { DeleteObject(self.bitmap.into()) }.as_bool());
        if bitmap_deleted {
            self.bitmap = HBITMAP::default();
            self.bits = std::ptr::null_mut();
        }
        let flushed = unsafe { GdiFlush() }.as_bool();
        restored && dc_deleted && bitmap_deleted && flushed
    }
    fn finish(mut self) -> Result<()> {
        if !self.release() {
            return Err("设备截图 DIB 反选或释放失败".into());
        }
        Ok(())
    }
}
impl Drop for DeviceDib {
    fn drop(&mut self) {
        if !self.dc.is_invalid() || !self.bitmap.is_invalid() {
            self.release();
        }
    }
}

// 仅 trace 请求记录标量；输出由现有 deferred readback 日志统一延后，不在捕获线程打印。
#[derive(serde::Serialize)]
struct DeviceRequestTiming {
    thread_id: u32,
    precreated_worker: Option<bool>,
    dispatch_qpc: Option<i64>,
    worker_receive_qpc: Option<i64>,
    capture_enter_qpc: i64,
    dpi_begin_qpc: i64,
    dpi_end_qpc: Option<i64>,
    capture_return_qpc: Option<i64>,
    idle_dpi_restore_begin_qpc: Option<i64>,
    idle_dpi_restore_end_qpc: Option<i64>,
    bitblt_cpu_envelope: Option<crate::startup_probe::ThreadCpuEnvelope>,
    flush_cpu_envelope: Option<crate::startup_probe::ThreadCpuEnvelope>,
}

impl DeviceRequestTiming {
    fn begin() -> Self {
        let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        let begin = crate::capture_standby::qpc();
        Self {
            thread_id,
            precreated_worker: None,
            dispatch_qpc: None,
            worker_receive_qpc: None,
            // 捕获入口紧接着调用 PhysicalDpi::new，使用同一 QPC 边界。
            capture_enter_qpc: begin,
            dpi_begin_qpc: begin,
            dpi_end_qpc: None,
            capture_return_qpc: None,
            idle_dpi_restore_begin_qpc: None,
            idle_dpi_restore_end_qpc: None,
            bitblt_cpu_envelope: None,
            flush_cpu_envelope: None,
        }
    }
}

struct DeviceTiming {
    device: String,
    rect: ScreenRect,
    geometry: DeviceDcGeometry,
    source: POINT,
    durations: [f64; 6],
    request: Option<DeviceRequestTiming>,
}
impl DeviceTiming {
    fn print_deferred(&self, index: usize) {
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase":"startup_gdi_device_ddb_screen","pid":std::process::id(),"screen":index,
            "capture_strategy":"device-ddb-deferred-readback","device_name":self.device,
            "physical_rect":[self.rect.left,self.rect.top,self.rect.width,self.rect.height],
            "dc_origin":[self.geometry.origin.x,self.geometry.origin.y],
            "source_origin":[self.source.x,self.source.y],
            "device_dc_and_mapping_ms":self.durations[0],"ddb_allocate_ms":self.durations[1],
            "bitblt_ms":self.durations[2],"gdi_flush_ms":self.durations[3],
            "detach_and_device_dc_release_ms":self.durations[5],
            "cpu_pixels_before_cache":false,"capture_dc_deleted":true,"original_deselected":true,
            "request_thread_timing":self.request,
            "cpu_observation_scope":"Trace=1 default DDB only; before/after surround API QPC interval and stored elapsed; cumulative CPU in 100ns, raw cycles; observation overhead included; invalid samples are not zero; BitBlt is still followed by required GdiFlush",
            "request_thread_timing_scope":"QPC; fresh worker receive means closure entry; capture return includes request DPI restore; idle DPI restore has a separate call interval, not a success assertion; emitted during deferred readback",
            "timing_scope":"wall intervals after request; parallel workers overlap; not GPU execution times"
        }));
    }

    fn print(&self, index: usize) {
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase":"startup_gdi_device_screen", "pid":std::process::id(), "screen":index,
            "capture_strategy":"device-per-monitor-bitblt", "device_name":self.device,
            "physical_rect":[self.rect.left,self.rect.top,self.rect.width,self.rect.height],
            "dc_origin":[self.geometry.origin.x,self.geometry.origin.y], "source_origin":[self.source.x,self.source.y],
            "window_origin":[self.geometry.window.x,self.geometry.window.y],
            "viewport_origin":[self.geometry.viewport.x,self.geometry.viewport.y],
            "dc_clip":[self.geometry.clip.left,self.geometry.clip.top,self.geometry.clip.right,self.geometry.clip.bottom],
            "device_caps":[self.geometry.width,self.geometry.height,self.geometry.planes,self.geometry.bits],
            "mapping":"MM_TEXT; logical=physical-DCorigin-viewport+window",
            "device_dc_and_mapping_ms":self.durations[0], "pixel_and_dib_allocate_ms":self.durations[1],
            "bitblt_ms":self.durations[2], "gdi_flush_ms":self.durations[3],
            "pixel_copy_alpha_ms":self.durations[4], "resource_drop_ms":self.durations[5],
            "deselected_and_all_device_resources_deleted":true,
            "timing_scope":"wall intervals after all device freezes; not GPU execution times"
        }));
    }
}

#[derive(Clone, Copy)]
struct DeviceDcGeometry {
    origin: POINT,
    window: POINT,
    viewport: POINT,
    clip: RECT,
    width: i32,
    height: i32,
    planes: i32,
    bits: i32,
    text_mapping: bool,
    compatible: bool,
    layout: u32,
}
fn checked_device_source_origin(rect: ScreenRect, geometry: DeviceDcGeometry) -> Result<POINT> {
    image_bytes(rect.width, rect.height)?;
    if geometry.width != rect.width as i32 || geometry.height != rect.height as i32 {
        return Err("显示设备 DC 物理尺寸与本次屏幕范围不符".into());
    }
    if !geometry.text_mapping
        || !geometry.compatible
        || geometry.layout != 0
        || geometry.planes != 1
        || geometry.bits != 32
    {
        return Err(UnsupportedDeviceDc("显示设备 DC 的格式或坐标映射不受支持").into());
    }
    // MM_TEXT 下 logical→physical = logical-window+viewport+原生 DC origin。
    // 各项先用 i64 计算；不能把 secondary 的原点假设成全局零或 local 零。
    let x = rect.left as i64 - geometry.origin.x as i64 - geometry.viewport.x as i64
        + geometry.window.x as i64;
    let y = rect.top as i64 - geometry.origin.y as i64 - geometry.viewport.y as i64
        + geometry.window.y as i64;
    let source = POINT {
        x: i32::try_from(x).map_err(|_| UnsupportedDeviceDc("显示设备横坐标映射溢出"))?,
        y: i32::try_from(y).map_err(|_| UnsupportedDeviceDc("显示设备纵坐标映射溢出"))?,
    };
    let right = i32::try_from(x + rect.width as i64)
        .map_err(|_| UnsupportedDeviceDc("显示设备右边界映射溢出"))?;
    let bottom = i32::try_from(y + rect.height as i64)
        .map_err(|_| UnsupportedDeviceDc("显示设备下边界映射溢出"))?;
    if geometry.clip.left != source.x
        || geometry.clip.top != source.y
        || geometry.clip.right != right
        || geometry.clip.bottom != bottom
    {
        return Err(UnsupportedDeviceDc("显示设备 DC 精确裁剪与物理屏幕映射不对应").into());
    }
    Ok(source)
}
fn device_source_origin(dc: HDC, device: &MonitorDevice) -> Result<(DeviceDcGeometry, POINT)> {
    let mut geometry = DeviceDcGeometry {
        origin: POINT::default(),
        window: POINT::default(),
        viewport: POINT::default(),
        clip: RECT::default(),
        width: 0,
        height: 0,
        planes: 0,
        bits: 0,
        text_mapping: false,
        compatible: false,
        layout: 0,
    };
    unsafe {
        native_call(
            "GetDCOrgEx(device)",
            GetDCOrgEx(dc, &mut geometry.origin).ok(),
        )?;
        native_call(
            "GetWindowOrgEx(device)",
            GetWindowOrgEx(dc, &mut geometry.window).ok(),
        )?;
        native_call(
            "GetViewportOrgEx(device)",
            GetViewportOrgEx(dc, &mut geometry.viewport).ok(),
        )?;
        geometry.text_mapping = GetMapMode(dc) == MM_TEXT;
        geometry.compatible = GetGraphicsMode(dc) == GM_COMPATIBLE.0;
        geometry.layout = GetLayout(dc);
        geometry.width = GetDeviceCaps(Some(dc), HORZRES);
        geometry.height = GetDeviceCaps(Some(dc), VERTRES);
        geometry.planes = GetDeviceCaps(Some(dc), PLANES);
        geometry.bits = GetDeviceCaps(Some(dc), BITSPIXEL);
        if GetClipBox(dc, &mut geometry.clip).0 <= NULLREGION.0 {
            return Err("显示设备 DC 可见区域无效".into());
        }
    }
    let source = checked_device_source_origin(device.rect, geometry)?;
    Ok((geometry, source))
}

/// 两种设备捕获共用的开头：物理 DPI 守卫（须活过 DC）、设备 DC 与映射几何、首段计时。
fn open_device(
    device: &MonitorDevice,
    trace: bool,
) -> Result<(PhysicalDpi, DeviceDc, DeviceTiming)> {
    let mut request = trace.then(DeviceRequestTiming::begin);
    let dpi = PhysicalDpi::new()?;
    if let Some(request) = &mut request {
        request.dpi_end_qpc = Some(crate::capture_standby::qpc());
    }
    let started = Instant::now();
    let dc = DeviceDc::new(device)?;
    let (geometry, source) = match device_source_origin(dc.dc, device) {
        Ok(geometry) => geometry,
        Err(error) => {
            // 映射不兼容可以回退，但释放失败必须优先终止本次请求。
            dc.finish()?;
            return Err(error);
        }
    };
    let mut timing = DeviceTiming {
        device: String::from_utf16_lossy(
            &device.name[..device.name.iter().position(|&v| v == 0).unwrap_or(32)],
        ),
        rect: device.rect,
        geometry,
        source,
        durations: [0.; 6],
        request,
    };
    timing.durations[0] = started.elapsed().as_secs_f64() * 1000.;
    Ok((dpi, dc, timing))
}

fn capture_device(
    device: &MonitorDevice,
    trace: bool,
) -> Result<(FrozenScreen, Option<DeviceTiming>)> {
    let (_dpi, dc, mut timing) = open_device(device, trace)?;
    let source = timing.source;
    let started = Instant::now();
    let bytes = image_bytes(device.rect.width, device.rect.height)?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(bytes)?;
    let surface = DeviceDib::new(dc.dc, device.rect.width, device.rect.height)?;
    timing.durations[1] = started.elapsed().as_secs_f64() * 1000.;
    let captured = (|| -> Result<_> {
        let started = Instant::now();
        native_call("BitBlt(device)", unsafe {
            SetLastError(WIN32_ERROR(0));
            BitBlt(
                surface.dc,
                0,
                0,
                device.rect.width as i32,
                device.rect.height as i32,
                Some(dc.dc),
                source.x,
                source.y,
                SRCCOPY,
            )
        })?;
        timing.durations[2] = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        native_call("GdiFlush(device)", unsafe { GdiFlush().ok() })?;
        timing.durations[3] = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        unsafe {
            initialize_opaque_bgra(
                std::slice::from_raw_parts(surface.bits, bytes),
                &mut bgra.spare_capacity_mut()[..bytes],
            );
            bgra.set_len(bytes);
        }
        timing.durations[4] = started.elapsed().as_secs_f64() * 1000.;
        FrozenScreen::from_pixels(device.rect, bgra)
    })();
    let started = Instant::now();
    // 两个清理即使前者失败仍全部执行，失败不得宣称已得到可发表的截图。
    let released = surface.finish();
    let dc_released = dc.finish();
    timing.durations[5] = started.elapsed().as_secs_f64() * 1000.;
    let mut first_error = None;
    let frozen = match captured {
        Ok(frozen) => Some(frozen),
        Err(error) => {
            choose_device_error(&mut first_error, error);
            None
        }
    };
    if let Err(error) = released {
        choose_device_error(&mut first_error, error);
    }
    if let Err(error) = dc_released {
        choose_device_error(&mut first_error, error);
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok((
        frozen.ok_or("显示设备捕获未产生像素")?,
        trace.then_some(timing),
    ))
}

fn capture_device_result(
    device: &MonitorDevice,
    trace: bool,
    deferred: bool,
) -> Result<DeviceCapture> {
    let mut captured = if deferred {
        capture_device_ddb(device, trace).map(DeviceCapture::Deferred)
    } else {
        capture_device(device, trace).map(|(screen, timing)| DeviceCapture::Pixels(screen, timing))
    }?;
    if let Some(request) = captured
        .timing_mut()
        .and_then(|timing| timing.request.as_mut())
    {
        // 被调用捕获函数的请求 DPI 守卫已恢复；预建线程外层守卫仍有效。
        request.capture_return_qpc = Some(crate::capture_standby::qpc());
    }
    Ok(captured)
}

fn capture_device_ddb(device: &MonitorDevice, trace: bool) -> Result<DetachedDeviceDdb> {
    let (_dpi, dc, mut timing) = open_device(device, trace)?;
    let source = timing.source;
    let started = Instant::now();
    let surface = DdbCaptureSurface::new(dc.dc, device.rect)?;
    timing.durations[1] = started.elapsed().as_secs_f64() * 1000.;
    let mut cpu = trace.then(crate::startup_probe::ThreadCpuProbe::begin);
    let started = Instant::now();
    if let Some(cpu) = cpu.as_mut() {
        cpu.mark_begin(Some(crate::capture_standby::qpc()));
    }
    let copied = native_call("BitBlt(device-original-DDB)", unsafe {
        SetLastError(WIN32_ERROR(0));
        BitBlt(
            surface.dc,
            0,
            0,
            device.rect.width as i32,
            device.rect.height as i32,
            Some(dc.dc),
            source.x,
            source.y,
            SRCCOPY,
        )
    }); // 必须先持有真实 API 结果，再调用 CPU 计数 API，保留原错误。
    let api_end = trace.then(crate::capture_standby::qpc);
    timing.durations[2] = started.elapsed().as_secs_f64() * 1000.;
    if let Some(request) = timing.request.as_mut() {
        request.bitblt_cpu_envelope = cpu.map(|cpu| cpu.finish(api_end));
    }
    copied?;
    let mut cpu = trace.then(crate::startup_probe::ThreadCpuProbe::begin);
    let started = Instant::now();
    if let Some(cpu) = cpu.as_mut() {
        cpu.mark_begin(Some(crate::capture_standby::qpc()));
    }
    let flushed = native_call("GdiFlush(device-original-DDB)", unsafe { GdiFlush().ok() });
    let api_end = trace.then(crate::capture_standby::qpc);
    timing.durations[3] = started.elapsed().as_secs_f64() * 1000.;
    if let Some(request) = timing.request.as_mut() {
        request.flush_cpu_envelope = cpu.map(|cpu| cpu.finish(api_end));
    }
    flushed?;
    let started = Instant::now();
    // 唯一 HBITMAP 立即交给 RAII；DeleteDC(device) 失败也不泄漏已反选原图。
    let bitmap = surface.detach()?;
    let mut detached = DetachedDeviceDdb {
        bitmap_id: bitmap.0 as usize,
        rect: device.rect,
        capture_id: 0,
        native_id: 0,
        readback_completed: false,
        timing: None,
    };
    dc.finish()?;
    timing.durations[5] = started.elapsed().as_secs_f64() * 1000.;
    detached.timing = trace.then_some(timing);
    Ok(detached)
}

fn capture_devices_parallel(devices: &[MonitorDevice], trace: bool) -> Result<DeviceCaptures> {
    let (captures, mut worker_timing) = capture_device_tasks(devices, trace, false)?;
    // 无原生首屏的 CPU 路径仍在返回前汇合全部原线程。
    worker_timing.join_tasks(trace)?;
    let captures = captures
        .into_iter()
        .map(|capture| match capture {
            DeviceCapture::Pixels(screen, timing) => Ok((screen, timing)),
            DeviceCapture::Deferred(_) => Err("CPU 任务返回了延后读回 DDB".into()),
        })
        .collect::<Result<Vec<_>>>()?;
    let (screens, timings) = captures
        .into_iter()
        .map(|(screen, timing)| ((screen, None), timing))
        .unzip();
    Ok((screens, timings, worker_timing))
}

fn capture_device_tasks(
    devices: &[MonitorDevice],
    trace: bool,
    deferred: bool,
) -> Result<(Vec<DeviceCapture>, DeviceWorkerTiming)> {
    let first = devices.first().ok_or("没有可冻结的显示设备")?;
    let precreated = PREWARMED_DEVICE_TASKS.with(|slot| slot.borrow_mut().take());
    let mut worker_timing = DeviceWorkerTiming {
        joins_deferred: deferred,
        ..Default::default()
    };
    let mut available = match precreated {
        Some(precreated) => {
            worker_timing.available_precreated_count = precreated.workers.len();
            worker_timing.prewarm_startup_ms = precreated.startup_ms;
            precreated.workers.into_iter()
        }
        None => Vec::new().into_iter(),
    };
    let mut tasks = Vec::new();
    tasks.try_reserve_exact(devices.len().saturating_sub(1))?;
    let mut captures = Vec::new();
    captures.try_reserve_exact(devices.len())?;
    let mut first_error: Option<Box<dyn Error>> = None;
    let started = trace.then(Instant::now);
    for device in devices.iter().skip(1) {
        if let Some(mut task) = available.next() {
            // 所有名称和范围来自本次枚举；闲置线程没有捕获过旧布局。
            let dispatched = task.start(device.clone(), trace, deferred);
            tasks.push(task);
            if let Err(error) = dispatched {
                choose_device_error(&mut first_error, error);
                break;
            }
            worker_timing.precreated_worker_count += 1;
        } else {
            match DeviceTask::fresh(device.clone(), trace, deferred) {
                Ok(task) => {
                    tasks.push(task);
                    worker_timing.fresh_worker_count += 1;
                }
                Err(error) => {
                    choose_device_error(&mut first_error, error);
                    break;
                }
            }
        }
    }
    worker_timing.unused_precreated_count = available.len();
    // 布局减少或派发失败时，未消费的原线程也在本次返回前 Stop + join。
    drop(available);
    if let Some(started) = started {
        worker_timing.dispatch_wall_ms = started.elapsed().as_secs_f64() * 1000.;
    }
    let started = trace.then(Instant::now);
    match capture_device_result(first, trace, deferred) {
        Ok(capture) => captures.push(capture),
        Err(error) => choose_device_error(&mut first_error, error),
    }
    if let Some(started) = started {
        worker_timing.main_capture_wall_ms = started.elapsed().as_secs_f64() * 1000.;
    }
    let started = trace.then(Instant::now);
    for task in &tasks {
        match task.receive_result() {
            Ok(capture) => captures.push(capture),
            Err(error) => choose_device_error(&mut first_error, error),
        }
    }
    if let Some(started) = started {
        worker_timing.result_wait_wall_ms = started.elapsed().as_secs_f64() * 1000.;
    }
    // 成功交付后句柄归同次 pending 所有；任何提前返回则 Drop 在回退/退出前汇合。
    worker_timing.tasks = tasks;
    if first_error.is_some() {
        // 回退之前明确观察所有线程的最终结果；发布后 panic 不能被 Drop 吞掉，
        // 否则可回退的采集 API 错误会掩盖硬失败并导致重新截取另一时刻的桌面。
        worker_timing.join_tasks_prioritizing(trace, first_error)?;
    }
    let started = trace.then(Instant::now);
    let validated = monitor_devices().and_then(|current| {
        if current == devices {
            Ok(())
        } else {
            Err("冻结期间原显示设备名称或物理范围改变".into())
        }
    });
    if let Err(error) = validated {
        worker_timing.join_tasks_prioritizing(trace, Some(error))?;
    }
    if let Some(started) = started {
        worker_timing.post_validate_wall_ms = started.elapsed().as_secs_f64() * 1000.;
    }
    Ok((captures, worker_timing))
}
type DeviceCaptures = (
    Vec<(FrozenScreen, Option<ScreenTiming>)>,
    Vec<Option<DeviceTiming>>,
    DeviceWorkerTiming,
);

#[derive(Default)]
struct DeviceWorkerTiming {
    tasks: Vec<DeviceTask>,
    joins_deferred: bool,
    available_precreated_count: usize,
    precreated_worker_count: usize,
    fresh_worker_count: usize,
    unused_precreated_count: usize,
    prewarm_startup_ms: f64,
    dispatch_wall_ms: f64,
    main_capture_wall_ms: f64,
    result_wait_wall_ms: f64,
    join_wall_ms: f64,
    join_begin_qpc: Option<i64>,
    join_end_qpc: Option<i64>,
    post_validate_wall_ms: f64,
}
impl DeviceWorkerTiming {
    fn join_tasks(&mut self, trace: bool) -> Result<()> {
        self.join_tasks_prioritizing(trace, None)
    }

    fn join_tasks_prioritizing(
        &mut self,
        trace: bool,
        mut first_error: Option<Box<dyn Error>>,
    ) -> Result<()> {
        self.join_begin_qpc = trace.then(crate::capture_standby::qpc);
        let started = trace.then(Instant::now);
        for task in &mut self.tasks {
            if let Err(error) = task.join() {
                choose_device_error(&mut first_error, error);
            }
        }
        self.join_end_qpc = trace.then(crate::capture_standby::qpc);
        if let Some(started) = started {
            self.join_wall_ms = started.elapsed().as_secs_f64() * 1000.;
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn print(&self) {
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase":"startup_gdi_device_workers", "pid":std::process::id(),
            "available_precreated_count":self.available_precreated_count,
            "precreated_worker_count":self.precreated_worker_count,
            "fresh_worker_count":self.fresh_worker_count,
            "unused_precreated_count":self.unused_precreated_count,
            "precreated_worker_used":self.precreated_worker_count > 0,
            "prewarm_startup_ms":self.prewarm_startup_ms,
            "dispatch_wall_ms":self.dispatch_wall_ms,
            "main_capture_wall_ms":self.main_capture_wall_ms,
            "result_wait_wall_ms":self.result_wait_wall_ms,
            "join_wall_ms":self.join_wall_ms,
            "join_deferred_until_pending_finish":self.joins_deferred,
            "join_begin_qpc":self.join_begin_qpc,
            "join_end_qpc":self.join_end_qpc,
            "post_validate_wall_ms":self.post_validate_wall_ms,
            "all_device_threads_joined":self.tasks.iter().all(|task|task.worker.is_none()),
            "request_device_names_and_rects_revalidated":true,
            "idle_device_dc_created":false,"idle_device_pixels_retained":false,
            "timing_scope":"request dispatch/main-capture/result-wait/post-validation; actual thread join is a separate later stage for deferred startup, before CPU readback; worker captures overlap dispatch and main capture; prewarm_startup_ms is idle-only; do not sum with per-screen timings"
        }));
    }
}
fn choose_device_error(current: &mut Option<Box<dyn Error>>, error: Box<dyn Error>) {
    if current
        .as_ref()
        .is_none_or(|first| can_fallback(first.as_ref()) && !can_fallback(error.as_ref()))
    {
        *current = Some(error);
    }
}

fn image_bytes(width: u32, height: u32) -> Result<usize> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|count| count.checked_mul(4))
        .ok_or("冻结屏幕像素长度溢出")?;
    if width == 0
        || height == 0
        || width > MAX_CANVAS_SIDE
        || height > MAX_CANVAS_SIDE
        || bytes > MAX_FROZEN_BYTES
    {
        return Err("冻结屏幕尺寸无效或超过像素上限".into());
    }
    Ok(bytes)
}

struct PhysicalDpi(DPI_AWARENESS_CONTEXT);
impl PhysicalDpi {
    fn new() -> Result<Self> {
        let previous = unsafe {
            SetLastError(WIN32_ERROR(0));
            SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        };
        if previous.0.is_null() {
            return Err(NativeCaptureError::last("SetThreadDpiAwarenessContext").into());
        }
        Ok(Self(previous))
    }
}
impl Drop for PhysicalDpi {
    fn drop(&mut self) {
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

fn monitor_rects() -> Result<Vec<ScreenRect>> {
    #[derive(Default)]
    struct Monitors {
        screens: Vec<ScreenRect>,
        error: Option<NativeCaptureError>,
    }
    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let state = &mut *(data.0 as *mut Monitors);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        SetLastError(WIN32_ERROR(0));
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            state.error = Some(NativeCaptureError::last("GetMonitorInfoW"));
            return windows::core::BOOL(0);
        }
        let r = info.rcMonitor;
        let width = r.right as i64 - r.left as i64;
        let height = r.bottom as i64 - r.top as i64;
        if width > 0 && height > 0 && width <= u32::MAX as i64 && height <= u32::MAX as i64 {
            let rect = ScreenRect {
                left: r.left,
                top: r.top,
                width: width as u32,
                height: height as u32,
            };
            if !state.screens.contains(&rect) {
                state.screens.push(rect);
            }
        }
        windows::core::BOOL(1)
    }
    let mut monitors = Monitors::default();
    let ok = unsafe {
        SetLastError(WIN32_ERROR(0));
        EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM((&mut monitors as *mut Monitors) as isize),
        )
    };
    if let Some(error) = monitors.error {
        return Err(error.into());
    }
    if !ok.as_bool() {
        return Err(NativeCaptureError::last("EnumDisplayMonitors").into());
    }
    if monitors.screens.is_empty() {
        return Err("无法枚举全部活动显示器".into());
    }
    Ok(monitors.screens)
}

struct ScreenDc(HDC);
impl ScreenDc {
    fn release_checked(self) -> Result<()> {
        if unsafe { ReleaseDC(None, self.0) } != 1 {
            return Err(NativeCaptureError::last("ReleaseDC(deferred-desktop)").into());
        }
        // 已在取得该 HDC 的原线程释放；不再让 Drop 重复 ReleaseDC。
        std::mem::forget(self);
        Ok(())
    }
    fn new() -> Result<Self> {
        let dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            GetDC(None)
        };
        if dc.is_invalid() {
            return Err(NativeCaptureError::last("GetDC").into());
        }
        Ok(Self(dc))
    }
}
impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(None, self.0) };
    }
}

struct DibSurface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
}
impl DibSurface {
    fn finish_checked(mut self) -> Result<()> {
        // 本专项不用旧 Drop 的 bitmap-before-DC 失败清理顺序。
        // 即使反选失败，也先删除 DC 解除选择，再删除 bitmap；未成功的原句柄留给 RAII 重试。
        struct Cleanup {
            dc: HDC,
            bitmap: HBITMAP,
            previous: HGDIOBJ,
            bitmap_id: usize,
        }
        impl Cleanup {
            fn release(&mut self) -> (bool, bool, bool, bool) {
                let restored = if self.dc.is_invalid() || self.previous.is_invalid() {
                    true
                } else {
                    let displaced = unsafe { SelectObject(self.dc, self.previous) };
                    let exact = displaced == self.bitmap.into()
                        && unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } == self.previous;
                    if exact {
                        self.previous = HGDIOBJ::default();
                    }
                    exact
                };
                let dc_deleted = self.dc.is_invalid() || unsafe { DeleteDC(self.dc) }.as_bool();
                if dc_deleted {
                    self.dc = HDC::default();
                    self.previous = HGDIOBJ::default();
                }
                let bitmap_deleted = self.bitmap.is_invalid()
                    || unsafe { DeleteObject(self.bitmap.into()) }.as_bool();
                if bitmap_deleted {
                    self.bitmap = HBITMAP::default();
                }
                let flushed = unsafe { GdiFlush() }.as_bool();
                (restored, dc_deleted, bitmap_deleted, flushed)
            }
            fn record(&self, receipt: (bool, bool, bool, bool), retry: bool) {
                eprintln!(
                    "{}",
                    serde_json::json!({"phase":"startup_ddb_fixture_destination_cleanup",
                    "pid":std::process::id(),"bitmap_id":self.bitmap_id.to_string(),"retry":retry,
                    "restored":receipt.0,"dc_deleted":receipt.1,"bitmap_deleted":receipt.2,"flush_ok":receipt.3})
                );
            }
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let receipt = self.release();
                self.record(receipt, true);
            }
        }
        let mut cleanup = Cleanup {
            bitmap_id: self.bitmap.0 as usize,
            dc: std::mem::take(&mut self.dc),
            bitmap: std::mem::take(&mut self.bitmap),
            previous: std::mem::take(&mut self.old),
        };
        self.bits = std::ptr::null_mut();
        // 原 surface 已没有任何句柄；转由局部 guard 统一成功/失败清理。
        std::mem::forget(self);
        let receipt = cleanup.release();
        if receipt != (true, true, true, true) {
            cleanup.record(receipt, false);
            return Err("headless DIB 反选/删除 DC/删除 bitmap/flush 未全部成功".into());
        }
        // 成功全部回收后不再对空句柄重复操作；失败则 Drop 用原未释放句柄再试并记录真实回执。
        std::mem::forget(cleanup);
        Ok(())
    }
    fn detach_bitmap(&mut self) -> Result<HBITMAP> {
        if self.bitmap.is_invalid() || self.old.is_invalid() {
            return Err("冻结 DIB 尚未有效选入捕获 DC".into());
        }
        let displaced = unsafe { SelectObject(self.dc, self.old) };
        if displaced.is_invalid() {
            return Err(NativeCaptureError::last("SelectObject(detach)").into());
        }
        if displaced != self.bitmap.into()
            || unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } != self.old
        {
            return Err("冻结 DIB 未从原捕获 DC 精确脱离".into());
        }
        self.old = HGDIOBJ::default();
        self.bits = std::ptr::null_mut();
        Ok(std::mem::take(&mut self.bitmap))
    }

    fn new(screen: HDC, width: u32, height: u32) -> Result<Self> {
        let dc = unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateCompatibleDC(Some(screen))
        };
        if dc.is_invalid() {
            return Err(NativeCaptureError::last("CreateCompatibleDC").into());
        }
        let mut surface = Self {
            dc,
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
        };
        let info = crate::gdi_surface::bitmap_info(width, height);
        let mut bits = std::ptr::null_mut();
        surface.bitmap = native_call("CreateDIBSection", unsafe {
            SetLastError(WIN32_ERROR(0));
            CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        })?;
        if bits.is_null() {
            return Err("静态截图未返回像素缓冲区".into());
        }
        surface.old = unsafe {
            SetLastError(WIN32_ERROR(0));
            SelectObject(surface.dc, surface.bitmap.into())
        };
        if surface.old.is_invalid() {
            return Err(NativeCaptureError::last("SelectObject").into());
        }
        surface.bits = bits.cast();
        Ok(surface)
    }
}
impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            if !self.old.is_invalid() {
                SelectObject(self.dc, self.old);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}

struct ScreenTiming {
    width: u32,
    height: u32,
    durations: [f64; 6],
}
impl ScreenTiming {
    fn print(&self, index: usize) {
        crate::startup_probe::emit_trace_args(format_args!(
            "{{\"phase\":\"startup_gdi_screen\",\"pid\":{},\"capture_strategy\":\"per-monitor-bitblt\",\"screen\":{index},\"width\":{},\"height\":{},\"pixel_allocate_ms\":{:.3},\"dib_allocate_ms\":{:.3},\"bitblt_ms\":{:.3},\"gdi_flush_ms\":{:.3},\"pixel_copy_alpha_ms\":{:.3},\"pixel_copy_ms\":null,\"pixel_alpha_ms\":null,\"resource_drop_ms\":{:.3}}}",
            std::process::id(), self.width, self.height,
            self.durations[0], self.durations[1], self.durations[2],
            self.durations[3], self.durations[4], self.durations[5],
        ));
    }
}
fn screen_stage_start(timing: &Option<&mut ScreenTiming>) -> Option<Instant> {
    timing.as_ref().map(|_| Instant::now())
}
fn screen_stage_end(
    timing: &mut Option<&mut ScreenTiming>,
    stage: usize,
    started: Option<Instant>,
) {
    if let (Some(timing), Some(started)) = (timing.as_deref_mut(), started) {
        timing.durations[stage] = started.elapsed().as_secs_f64() * 1000.;
    }
}

/// 将每个 BGRA 像素一次写入独立目标缓冲，只将 alpha 规范为 255。
/// 返回时全部目标字节已初始化；源可以不对齐，且不会被修改。
fn initialize_opaque_bgra(source: &[u8], target: &mut [MaybeUninit<u8>]) {
    assert_eq!(source.len(), target.len());
    assert!(source.len().is_multiple_of(4));
    #[cfg(target_arch = "x86_64")]
    let offset = unsafe {
        // SSE2 是 x86_64 基线；loadu/storeu 不要求 DIB 或 Vec 地址按 16 字节对齐。
        use std::arch::x86_64::{_mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_storeu_si128};
        let alpha = _mm_set1_epi32(0xff00_0000_u32 as i32);
        let mut offset = 0;
        while source.len() - offset >= 16 {
            let pixels = _mm_loadu_si128(source.as_ptr().add(offset).cast());
            _mm_storeu_si128(
                target.as_mut_ptr().add(offset).cast(),
                _mm_or_si128(pixels, alpha),
            );
            offset += 16;
        }
        offset
    };
    #[cfg(not(target_arch = "x86_64"))]
    let offset = 0;
    // SIMD 最后一组之外的 0–3 像素，以及其他平台，均逐像素完整初始化。
    for (source, target) in source[offset..]
        .chunks_exact(4)
        .zip(target[offset..].chunks_exact_mut(4))
    {
        target[0].write(source[0]);
        target[1].write(source[1]);
        target[2].write(source[2]);
        target[3].write(255);
    }
}
fn capture_screen(
    screen: HDC,
    rect: ScreenRect,
    mut timing: Option<&mut ScreenTiming>,
) -> Result<FrozenScreen> {
    let bytes = image_bytes(rect.width, rect.height)?;
    let started = screen_stage_start(&timing);
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(bytes)?;
    screen_stage_end(&mut timing, 0, started);
    let started = screen_stage_start(&timing);
    let surface = DibSurface::new(screen, rect.width, rect.height)?;
    screen_stage_end(&mut timing, 1, started);
    unsafe {
        let started = screen_stage_start(&timing);
        SetLastError(WIN32_ERROR(0));
        native_call(
            "BitBlt",
            BitBlt(
                surface.dc,
                0,
                0,
                rect.width as i32,
                rect.height as i32,
                Some(screen),
                rect.left,
                rect.top,
                // CAPTUREBLT 在部分多屏环境会触发 GreMovePointer，临时移走硬件光标。
                // 冻结桌面使用 SRCCOPY；光标由既有快照在 Ready 后独立合成。
                SRCCOPY,
            ),
        )?;
        screen_stage_end(&mut timing, 2, started);
        // DIB bits 只有在提交 GDI 批次后才可以直接访问；不能靠 BitBlt 返回值代替同步。
        let started = screen_stage_start(&timing);
        SetLastError(WIN32_ERROR(0));
        native_call("GdiFlush", GdiFlush().ok())?;
        screen_stage_end(&mut timing, 3, started);
        let started = screen_stage_start(&timing);
        initialize_opaque_bgra(
            std::slice::from_raw_parts(surface.bits, bytes),
            &mut bgra.spare_capacity_mut()[..bytes],
        );
        // reserve 已成功，且 helper 在正常返回前初始化了全部 bytes。
        // DIB 仅在此借用；Vec 独立持有复制结果，随后才公开其长度。
        bgra.set_len(bytes);
        screen_stage_end(&mut timing, 4, started);
    }
    let frozen = FrozenScreen::from_pixels(rect, bgra)?;
    let started = screen_stage_start(&timing);
    drop(surface);
    screen_stage_end(&mut timing, 5, started);
    Ok(frozen)
}

#[cfg(test)]
mod device_dc_tests {
    use super::*;
    fn geometry(origin: POINT, window: POINT, viewport: POINT, clip: RECT) -> DeviceDcGeometry {
        DeviceDcGeometry {
            origin,
            window,
            viewport,
            clip,
            width: 1920,
            height: 1080,
            planes: 1,
            bits: 32,
            text_mapping: true,
            compatible: true,
            layout: 0,
        }
    }
    fn rect() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: -200,
            width: 1920,
            height: 1080,
        }
    }
    #[test]
    fn device_dc_mapping_preserves_negative_global_and_nonzero_translation() {
        let native = geometry(
            POINT { x: -1920, y: -200 },
            POINT { x: 7, y: -11 },
            POINT { x: 3, y: 5 },
            RECT {
                left: 4,
                top: -16,
                right: 1924,
                bottom: 1064,
            },
        );
        let source = checked_device_source_origin(rect(), native).unwrap();
        assert_eq!((source.x, source.y), (4, -16));
        for (x, y) in [(0, 0), (1919, 1079), (27, 53)] {
            assert_eq!(
                source.x + x - native.window.x + native.viewport.x + native.origin.x,
                rect().left + x
            );
            assert_eq!(
                source.y + y - native.window.y + native.viewport.y + native.origin.y,
                rect().top + y
            );
        }
        let global = geometry(
            POINT { x: 0, y: 0 },
            POINT { x: 0, y: 0 },
            POINT { x: 0, y: 0 },
            RECT {
                left: -1920,
                top: -200,
                right: 0,
                bottom: 880,
            },
        );
        let source = checked_device_source_origin(rect(), global).unwrap();
        assert_eq!((source.x, source.y), (-1920, -200));
    }
    #[test]
    fn device_dc_mapping_rejects_clip_caps_and_transform_changes() {
        let valid = geometry(
            POINT { x: -1920, y: -200 },
            POINT { x: 0, y: 0 },
            POINT { x: 0, y: 0 },
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
        );
        assert!(checked_device_source_origin(rect(), valid).is_ok());
        let check = |changed, fallback| {
            let error = checked_device_source_origin(rect(), changed).unwrap_err();
            assert_eq!(can_fallback(error.as_ref()), fallback, "{error}");
        };
        let mut changed = valid;
        changed.clip.right -= 1;
        check(changed, true);
        let mut changed = valid;
        changed.width -= 1;
        check(changed, false);
        let mut changed = valid;
        changed.height -= 1;
        check(changed, false);
        let mut changed = valid;
        changed.bits = 24;
        check(changed, true);
        let mut changed = valid;
        changed.planes = 2;
        check(changed, true);
        let mut changed = valid;
        changed.layout = 1;
        check(changed, true);
        let mut changed = valid;
        changed.text_mapping = false;
        check(changed, true);
        let mut changed = valid;
        changed.compatible = false;
        check(changed, true);
        let mut changed = valid;
        changed.origin.x += 1;
        check(changed, true);
    }
    #[test]
    fn device_dc_mapping_rejects_checked_coordinate_overflow_and_budget() {
        let valid = geometry(
            POINT { x: -1920, y: -200 },
            POINT { x: 0, y: 0 },
            POINT { x: 0, y: 0 },
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
        );
        let mut changed = valid;
        changed.origin.x = i32::MIN;
        changed.window.x = i32::MAX;
        let error = checked_device_source_origin(rect(), changed).unwrap_err();
        assert!(can_fallback(error.as_ref()));
        let mut outside = rect();
        outside.left = i32::MAX;
        let mut changed = valid;
        changed.origin.x = 0;
        let error = checked_device_source_origin(outside, changed).unwrap_err();
        assert!(can_fallback(error.as_ref()));
        let mut zero = rect();
        zero.width = 0;
        let error = checked_device_source_origin(zero, valid).unwrap_err();
        assert!(!can_fallback(error.as_ref()));
        let oversized = ScreenRect {
            left: 0,
            top: 0,
            width: 16384,
            height: 16384,
        };
        let mut changed = valid;
        changed.width = 16384;
        changed.height = 16384;
        let error = checked_device_source_origin(oversized, changed).unwrap_err();
        assert!(!can_fallback(error.as_ref()));
    }

    #[test]
    fn device_dc_compatibility_survives_worker_transport_without_hiding_hard_errors() {
        let unsupported =
            || -> Box<dyn Error> { Box::new(UnsupportedDeviceDc("设备 DC 格式不受支持")) };
        let sent = sendable_device_error(unsupported());
        let received = std::thread::spawn(move || sent).join().unwrap();
        assert!(received.is::<UnsupportedDeviceDc>());
        assert!(can_fallback(received.as_ref()));

        let hard_errors: Vec<Box<dyn Error>> = vec![
            "冻结期间原显示设备名称或物理范围改变".into(),
            NativeCaptureError {
                operation: "CreateDIBSection",
                source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
                    ERROR_NOT_ENOUGH_MEMORY.0,
                )),
            }
            .into(),
            NativeCaptureError {
                operation: "DeleteDC(monitor)",
                source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(87)),
            }
            .into(),
            frozen_readback_error(unsupported()),
            Box::new(DevicePrewarmHardError("设备预热清理失败".into())),
        ];
        for hard in hard_errors {
            let message = hard.to_string();
            let transported: Box<dyn Error> = sendable_device_error(hard);
            assert!(!can_fallback(transported.as_ref()), "{message}");
            let mut selected = Some(unsupported());
            choose_device_error(&mut selected, transported);
            choose_device_error(&mut selected, unsupported());
            let selected = selected.unwrap();
            assert_eq!(selected.to_string(), message);
            assert!(!can_fallback(selected.as_ref()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_device_original_readback_failure_never_allows_desktop_recapture() {
        for operation in [
            "GetDIBits(deferred-device-original)",
            "GdiFlush(deferred-device-readback)",
        ] {
            let native = NativeCaptureError {
                operation,
                source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(87)),
            };
            assert!(can_fallback(&native)); // 冻结阶段之前，这个原生码允许后备。
            let failed = frozen_readback_error(Box::new(native));
            assert!(!can_fallback(failed.as_ref()));
            assert!(!device_cache_can_fallback(failed.as_ref()));
            let cause = failed
                .source()
                .unwrap()
                .downcast_ref::<NativeCaptureError>()
                .unwrap();
            assert_eq!(cause.operation, operation);
            assert_eq!(cause.source.code(), windows::core::HRESULT::from_win32(87));
        }
    }
    use crate::canvas_source::CanvasSource;

    #[test]
    fn device_black_ddb_warm_requires_exact_opt_in() {
        assert!(device_black_ddb_warm_enabled_value(Some("1")));
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some(" 1"),
            Some("1 "),
        ] {
            assert!(!device_black_ddb_warm_enabled_value(value));
        }
    }

    #[test]
    fn device_black_ddb_cleanup_error_overrides_success_and_capture_error() {
        for operation in [Ok(()), Err("controlled operation failure".into())] {
            let error = device_warm_finish_result(
                operation,
                Err(Box::new(DevicePrewarmHardError(
                    "controlled cleanup failure".into(),
                ))),
            )
            .unwrap_err();
            assert!(prewarm_requires_worker_exit(error.as_ref()));
            assert!(!can_fallback(error.as_ref()));
            assert_eq!(error.to_string(), "controlled cleanup failure");
        }
    }

    #[test]
    fn device_black_ddb_cleaned_native_error_keeps_original_fallback_class() {
        let native: Box<dyn Error> = Box::new(NativeCaptureError {
            operation: "BitBlt(controlled black memory DDB)",
            source: windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80004005u32 as i32,
            )),
        });
        let error = device_warm_finish_result(Err(native), Ok(())).unwrap_err();
        assert!(!prewarm_requires_worker_exit(error.as_ref()));
        assert!(can_fallback(error.as_ref()));
        assert!(error.is::<NativeCaptureError>());
    }

    #[test]
    fn device_black_ddb_hard_error_survives_worker_publication() {
        let error = sendable_device_error(Box::new(DevicePrewarmHardError(
            "checked cleanup unknown".into(),
        )));
        assert!(prewarm_requires_worker_exit(error.as_ref()));
        assert!(!can_fallback(error.as_ref()));

        // ACK 超时发生于清理之前；原线程正常退出但迟到 ACK 明确为 hard。
        let (command, receive) = std::sync::mpsc::sync_channel(1);
        let (started, ready) =
            std::sync::mpsc::sync_channel::<std::result::Result<(), Box<dyn Error + Send>>>(1);
        let (_completed, result) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            assert!(matches!(receive.recv(), Ok(DeviceTaskCommand::Stop)));
            started
                .send(Err(sendable_device_error(Box::new(
                    DevicePrewarmHardError("late checked cleanup failure".into()),
                ))))
                .unwrap();
        });
        let mut task = DeviceTask {
            command: Some(command),
            result,
            worker: Some(worker),
        };
        let timeout = ready.recv_timeout(std::time::Duration::ZERO).unwrap_err();
        assert_eq!(timeout, std::sync::mpsc::RecvTimeoutError::Timeout);
        let error = reap_failed_prewarm_tasks(
            std::slice::from_mut(&mut task),
            precreate_ack_receive_error(timeout),
        );
        assert!(task.worker.is_none() && task.command.is_none());
        let late = ready.try_recv().unwrap().unwrap_err();
        assert!(prewarm_requires_worker_exit(late.as_ref()));
        assert!(prewarm_requires_worker_exit(error.as_ref()));
        assert!(!can_fallback(error.as_ref()));
        let disconnected =
            precreate_ack_receive_error(std::sync::mpsc::RecvTimeoutError::Disconnected);
        assert!(prewarm_requires_worker_exit(disconnected.as_ref()));
    }

    fn idle_model_task(panic_after_stop: bool) -> (DeviceTask, Arc<std::sync::atomic::AtomicBool>) {
        let (command, receive) = std::sync::mpsc::sync_channel(1);
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = std::thread::spawn(move || {
            assert!(matches!(
                receive.recv_timeout(std::time::Duration::from_secs(5)),
                Ok(DeviceTaskCommand::Stop)
            ));
            worker_stopped.store(true, std::sync::atomic::Ordering::Release);
            if panic_after_stop {
                panic!("controlled prewarm worker panic");
            }
            assert!(completed.send(Ok(None)).is_ok());
        });
        (
            DeviceTask {
                command: Some(command),
                result,
                worker: Some(worker),
            },
            stopped,
        )
    }

    #[test]
    fn device_black_ddb_start_failure_stops_and_reaps_all_original_siblings() {
        let (first, first_stopped) = idle_model_task(false);
        let (second, second_stopped) = idle_model_task(false);
        let mut tasks = [first, second];
        let error = reap_failed_prewarm_tasks(&mut tasks, "controlled startup failure".into());
        assert_eq!(error.to_string(), "controlled startup failure");
        assert!(!prewarm_requires_worker_exit(error.as_ref()));
        assert!(tasks
            .iter()
            .all(|task| task.worker.is_none() && task.command.is_none()));
        assert!(first_stopped.load(std::sync::atomic::Ordering::Acquire));
        assert!(second_stopped.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn device_black_ddb_start_join_panic_is_hard_and_still_reaps_siblings() {
        let (first, first_stopped) = idle_model_task(true);
        let (second, second_stopped) = idle_model_task(false);
        let mut tasks = [first, second];
        let error = reap_failed_prewarm_tasks(&mut tasks, "controlled startup timeout".into());
        assert!(prewarm_requires_worker_exit(error.as_ref()));
        assert!(!can_fallback(error.as_ref()));
        assert!(tasks
            .iter()
            .all(|task| task.worker.is_none() && task.command.is_none()));
        assert!(first_stopped.load(std::sync::atomic::Ordering::Acquire));
        assert!(second_stopped.load(std::sync::atomic::Ordering::Acquire));
    }

    fn held_device_task(
        captured: DeviceTaskResult,
    ) -> (
        DeviceTask,
        std::sync::mpsc::SyncSender<()>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let (release, held) = std::sync::mpsc::sync_channel(1);
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_exited = Arc::clone(&exited);
        let worker = std::thread::spawn(move || {
            assert!(completed.send(captured).is_ok());
            held.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            worker_exited.store(true, std::sync::atomic::Ordering::Release);
        });
        (
            DeviceTask {
                command: None,
                result,
                worker: Some(worker),
            },
            release,
            exited,
        )
    }

    fn one_pixel_device_result() -> DeviceTaskResult {
        Ok(Some(DeviceCapture::Pixels(
            FrozenScreen::from_pixels(
                ScreenRect {
                    left: -7,
                    top: 13,
                    width: 1,
                    height: 1,
                },
                vec![31, 67, 103, 255],
            )
            .unwrap(),
            None,
        )))
    }

    #[test]
    fn device_task_result_is_available_before_original_thread_exit() {
        let (mut task, release, exited) = held_device_task(one_pixel_device_result());
        let DeviceCapture::Pixels(screen, _) = task.receive_result().unwrap() else {
            panic!("expected pixels");
        };
        assert_eq!(screen.rect.left, -7);
        assert_eq!(screen.preview.as_bytes(0).unwrap(), &[31, 67, 103, 255]);
        assert!(!task.worker.as_ref().unwrap().is_finished());
        assert!(!exited.load(std::sync::atomic::Ordering::Acquire));
        release.send(()).unwrap();
        task.join().unwrap();
        assert!(task.worker.is_none());
        assert!(exited.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn device_task_drop_waits_for_original_thread_after_result() {
        let (task, release, exited) = held_device_task(one_pixel_device_result());
        drop(task.receive_result().unwrap());
        let (entered, dropping) = std::sync::mpsc::sync_channel(1);
        let (returned, done) = std::sync::mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            entered.send(()).unwrap();
            drop(task);
            returned.send(()).unwrap();
        });
        dropping
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            done.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(!exited.load(std::sync::atomic::Ordering::Acquire));
        release.send(()).unwrap();
        done.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        dropper.join().unwrap();
        assert!(exited.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn device_task_capture_error_is_rejected_and_worker_is_reaped() {
        let expected = "controlled capture error";
        let (mut task, release, exited) =
            held_device_task(Err(Box::new(std::io::Error::other(expected))));
        let error = match task.receive_result() {
            Err(error) => error,
            Ok(_) => panic!("failed capture must not publish an image"),
        };
        assert_eq!(error.to_string(), expected);
        assert!(!can_fallback(error.as_ref()));
        release.send(()).unwrap();
        task.join().unwrap();
        assert!(task.worker.is_none());
        assert!(exited.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn device_task_panic_before_publication_is_rejected_and_reaped() {
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let _completed = completed;
            panic!("controlled device worker panic");
        });
        let mut task = DeviceTask {
            command: None,
            result,
            worker: Some(worker),
        };
        assert!(task.receive_result().is_err());
        assert!(task.join().is_err());
        assert!(task.worker.is_none());
    }

    #[test]
    fn device_task_panic_after_publication_is_rejected_at_join() {
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let captured = one_pixel_device_result();
        let worker = std::thread::spawn(move || {
            assert!(completed.send(captured).is_ok());
            panic!("controlled post-publication panic");
        });
        let mut task = DeviceTask {
            command: None,
            result,
            worker: Some(worker),
        };
        assert!(task.receive_result().is_ok());
        assert!(task.join().is_err());
        assert!(task.worker.is_none());
    }

    #[test]
    fn device_workers_join_panic_overrides_fallback_error() {
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let captured = one_pixel_device_result();
        let worker = std::thread::spawn(move || {
            assert!(completed.send(captured).is_ok());
            panic!("controlled sibling panic");
        });
        let task = DeviceTask {
            command: None,
            result,
            worker: Some(worker),
        };
        assert!(task.receive_result().is_ok());
        let (sibling, release, exited) = held_device_task(one_pixel_device_result());
        assert!(sibling.receive_result().is_ok());
        release.send(()).unwrap();
        let mut workers = DeviceWorkerTiming {
            tasks: vec![task, sibling],
            ..Default::default()
        };
        let capture_error: Box<dyn Error> = Box::new(NativeCaptureError {
            operation: "BitBlt(controlled)",
            source: windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80004005u32 as i32,
            )),
        });
        assert!(can_fallback(capture_error.as_ref()));
        let error = workers
            .join_tasks_prioritizing(false, Some(capture_error))
            .unwrap_err();
        assert!(!can_fallback(error.as_ref()));
        assert!(workers.tasks.iter().all(|task| task.worker.is_none()));
        assert!(exited.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn device_task_unused_command_stops_and_joins_original_worker() {
        let (command, receive) = std::sync::mpsc::sync_channel(1);
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = std::thread::spawn(move || {
            assert!(matches!(
                receive.recv_timeout(std::time::Duration::from_secs(5)),
                Ok(DeviceTaskCommand::Stop)
            ));
            assert!(completed.send(Ok(None)).is_ok());
            worker_stopped.store(true, std::sync::atomic::Ordering::Release);
        });
        drop(DeviceTask {
            command: Some(command),
            result,
            worker: Some(worker),
        });
        assert!(stopped.load(std::sync::atomic::Ordering::Acquire));
    }

    fn assert_native_cache_gap_pixels(bounds: ScreenRect, regions: &[Rect]) {
        let plan = native_cache_gap_plan(bounds, regions).unwrap();
        assert!(plan
            .gaps
            .iter()
            .all(|gap| gap.valid(bounds.width, bounds.height)));
        let mut outside_pixels = 0usize;
        for y in 0..bounds.height {
            for x in 0..bounds.width {
                // 独立逐像素 membership oracle，不复用分带、排序或区间合并。
                let covered = regions.iter().any(|rect| {
                    x >= rect.x
                        && y >= rect.y
                        && u64::from(x) < u64::from(rect.x) + u64::from(rect.width)
                        && u64::from(y) < u64::from(rect.y) + u64::from(rect.height)
                });
                let gap_memberships = plan
                    .gaps
                    .iter()
                    .filter(|gap| {
                        x >= gap.x
                            && y >= gap.y
                            && u64::from(x) < u64::from(gap.x) + u64::from(gap.width)
                            && u64::from(y) < u64::from(gap.y) + u64::from(gap.height)
                    })
                    .count();
                assert_eq!(gap_memberships, usize::from(!covered), "pixel ({x},{y})");
                outside_pixels += usize::from(!covered);
            }
        }
        assert_eq!(plan.pixels, outside_pixels);
    }

    #[test]
    fn native_cache_gaps_match_pixel_union_for_every_small_rectangle_pair() {
        let bounds = ScreenRect {
            left: -7,
            top: -9,
            width: 4,
            height: 3,
        };
        let mut regions = Vec::new();
        for y in 0..bounds.height {
            for x in 0..bounds.width {
                for bottom in y + 1..=bounds.height {
                    for right in x + 1..=bounds.width {
                        regions.push(Rect {
                            x,
                            y,
                            width: right - x,
                            height: bottom - y,
                        });
                    }
                }
            }
        }
        for &first in &regions {
            for &second in &regions {
                assert_native_cache_gap_pixels(bounds, &[first, second]);
            }
        }
        let full =
            native_cache_gap_plan(bounds, &[Rect::full(bounds.width, bounds.height)]).unwrap();
        assert!(full.gaps.is_empty());
        assert_eq!(full.pixels, 0);
    }

    #[test]
    fn native_cache_gaps_cover_multiple_holes_and_negative_portrait_layout() {
        let bounds = ScreenRect {
            left: -27,
            top: -13,
            width: 19,
            height: 17,
        };
        let regions = [
            Rect {
                x: 0,
                y: 0,
                width: 3,
                height: 17,
            },
            Rect {
                x: 16,
                y: 0,
                width: 3,
                height: 17,
            },
            Rect {
                x: 3,
                y: 0,
                width: 13,
                height: 2,
            },
            Rect {
                x: 3,
                y: 7,
                width: 13,
                height: 2,
            },
            Rect {
                x: 3,
                y: 15,
                width: 13,
                height: 2,
            },
            Rect {
                x: 8,
                y: 0,
                width: 2,
                height: 17,
            },
            Rect {
                x: 7,
                y: 6,
                width: 4,
                height: 4,
            },
            Rect {
                x: 8,
                y: 7,
                width: 1,
                height: 2,
            },
        ];
        let monitors: Vec<_> = regions
            .iter()
            .map(|rect| ScreenRect {
                left: bounds.left + rect.x as i32,
                top: bounds.top + rect.y as i32,
                width: rect.width,
                height: rect.height,
            })
            .collect();
        let (actual_bounds, _, locals) = pending_layout(&monitors).unwrap();
        assert_eq!(actual_bounds, bounds);
        assert_eq!(locals, regions);
        assert_native_cache_gap_pixels(actual_bounds, &locals);
        let plan = native_cache_gap_plan(actual_bounds, &locals).unwrap();
        for y in 0..bounds.height {
            for x in 0..bounds.width {
                let gx = i64::from(bounds.left) + i64::from(x);
                let gy = i64::from(bounds.top) + i64::from(y);
                let on_monitor = monitors.iter().any(|monitor| {
                    gx >= i64::from(monitor.left)
                        && gy >= i64::from(monitor.top)
                        && gx < i64::from(monitor.left) + i64::from(monitor.width)
                        && gy < i64::from(monitor.top) + i64::from(monitor.height)
                });
                assert_eq!(
                    plan.gaps
                        .iter()
                        .any(|gap| gap.contains((x as i32, y as i32))),
                    !on_monitor
                );
            }
        }
    }

    #[test]
    fn native_cache_gap_plan_rejects_invalid_geometry_and_bounds_work() {
        let bounds = ScreenRect {
            left: -3,
            top: -5,
            width: 4,
            height: 3,
        };
        let valid = Rect::full(bounds.width, bounds.height);
        assert!(native_cache_gap_plan(bounds, &[]).is_err());
        for region in [
            Rect { width: 0, ..valid },
            Rect { height: 0, ..valid },
            Rect { x: 1, ..valid },
            Rect { y: 1, ..valid },
            Rect {
                x: u32::MAX,
                width: 2,
                ..valid
            },
            Rect {
                y: u32::MAX,
                height: 2,
                ..valid
            },
        ] {
            assert!(native_cache_gap_plan(bounds, &[region]).is_err());
        }
        for invalid in [
            ScreenRect { width: 0, ..bounds },
            ScreenRect {
                width: MAX_CANVAS_SIDE + 1,
                ..bounds
            },
            ScreenRect {
                width: MAX_CANVAS_SIDE,
                height: MAX_CANVAS_SIDE,
                ..bounds
            },
            ScreenRect {
                left: i32::MAX,
                ..bounds
            },
            ScreenRect {
                top: i32::MAX,
                ..bounds
            },
        ] {
            assert!(native_cache_gap_plan(invalid, &[valid]).is_err());
        }
        let repeated = vec![valid; MAX_NATIVE_CACHE_REGIONS];
        let plan = native_cache_gap_plan(bounds, &repeated).unwrap();
        assert!(plan.gaps.is_empty());
        assert_eq!(plan.pixels, 0);
        assert!(native_cache_gap_plan(bounds, &vec![valid; MAX_NATIVE_CACHE_REGIONS + 1]).is_err());
    }

    #[test]
    fn capture_backend_fallback_rejects_native_cleanup_failures() {
        let error = |operation, code| NativeCaptureError {
            operation,
            source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)),
        };
        for operation in [
            "DeleteDC(DDB-detach)",
            "DeleteDC(readback)",
            "DeleteDC(monitor)",
            "ReleaseDC(deferred-desktop)",
            "DeleteObject(captured-bitmap)",
            "SelectObject(detach)",
        ] {
            for code in [0, 87, windows::Win32::Foundation::ERROR_NOT_SUPPORTED.0] {
                assert!(
                    !can_fallback(&error(operation, code)),
                    "{operation} 清理失败不得重截"
                );
                assert!(!device_cache_can_fallback(&error(operation, code)));
            }
        }
        for operation in [
            "BitBlt",
            "BitBlt(device-original-DDB)",
            "CreateDCW(monitor)",
        ] {
            assert!(can_fallback(&error(
                operation,
                windows::Win32::Foundation::ERROR_NOT_SUPPORTED.0
            )));
        }
    }

    #[test]
    fn device_native_cache_fallback_only_accepts_copy_or_selection_api_failures() {
        let error = |operation, code| NativeCaptureError {
            operation,
            source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)),
        };
        assert!(device_cache_can_fallback(&error(
            "BitBlt(device-native-cache)",
            87
        )));
        assert!(device_cache_can_fallback(&error(
            "SelectObject(device-cache-source)",
            87
        )));
        for operation in [
            "CreateCompatibleDC(device-cache-source)",
            "GdiFlush(device-native-cache)",
            "DeleteDC(DDB-detach)",
            "BitBlt(device-original-DDB)",
        ] {
            assert!(!device_cache_can_fallback(&error(operation, 87)));
        }
        assert!(!device_cache_can_fallback(&error(
            "BitBlt(device-native-cache)",
            ERROR_OUTOFMEMORY.0
        )));
        assert!(!device_cache_can_fallback(&std::io::Error::other(
            "DDB 缓存清理失败"
        )));
    }

    #[test]
    fn compact_gdi_split_keeps_negative_origin_rows_gaps_and_opaque_alpha() {
        let bounds = ScreenRect {
            left: -4,
            top: -3,
            width: 7,
            height: 7,
        };
        let rects = [
            ScreenRect {
                left: -4,
                top: -3,
                width: 2,
                height: 6,
            },
            ScreenRect {
                left: -1,
                top: 2,
                width: 4,
                height: 2,
            },
        ];
        // 独立的全局坐标像素，不均色、非255源alpha；两个屏幕各有行padding。
        let expected = |x: i32, y: i32| {
            [
                (x + 17 + y * 3) as u8,
                (x * x + y + 41) as u8,
                (y * y + x * 7 + 83) as u8,
                255,
            ]
        };
        let mut source = Vec::new();
        for y in 0..bounds.height {
            for x in 0..bounds.width {
                let mut pixel = expected(bounds.left + x as i32, bounds.top + y as i32);
                pixel[3] = ((x + y * 7) * 13) as u8;
                source.extend_from_slice(&pixel);
            }
        }
        let unchanged_source = source.clone();
        let mut screens = Vec::new();
        for rect in rects {
            let plan = PlaneCopyPlan::new(bounds, rect).unwrap();
            let mut storage = vec![MaybeUninit::new(0xa5_u8); plan.bytes + 9];
            unsafe {
                plan.initialize(
                    source.as_ptr(),
                    source.len(),
                    &mut storage[3..3 + plan.bytes],
                )
                .unwrap();
            }
            let actual: Vec<_> = storage
                .into_iter()
                .map(|byte| unsafe { byte.assume_init() })
                .collect();
            assert!(actual[..3].iter().all(|&byte| byte == 0xa5));
            assert!(actual[3 + plan.bytes..].iter().all(|&byte| byte == 0xa5));
            for y in 0..rect.height {
                for x in 0..rect.width {
                    let offset = 3 + (y as usize * rect.width as usize + x as usize) * 4;
                    assert_eq!(
                        actual[offset..offset + 4],
                        expected(rect.left + x as i32, rect.top + y as i32)
                    );
                }
            }
            screens
                .push(FrozenScreen::from_pixels(rect, actual[3..3 + plan.bytes].to_vec()).unwrap());
        }
        assert_eq!(source, unchanged_source);
        let desktop = FrozenDesktop::from_screens(screens, POINT { x: 0, y: 3 }, 0.).unwrap();
        assert_eq!(desktop.bounds, bounds);
        assert_eq!(desktop.current, 1);
        let crop = desktop
            .read_region(Rect::full(bounds.width, bounds.height))
            .unwrap();
        for y in 0..bounds.height {
            for x in 0..bounds.width {
                let gx = bounds.left + x as i32;
                let gy = bounds.top + y as i32;
                let on_screen = rects.iter().any(|rect| {
                    gx >= rect.left
                        && gy >= rect.top
                        && gx < rect.left + rect.width as i32
                        && gy < rect.top + rect.height as i32
                });
                let offset = (y as usize * bounds.width as usize + x as usize) * 4;
                assert_eq!(
                    crop.bgra[offset..offset + 4],
                    if on_screen {
                        expected(gx, gy)
                    } else {
                        [0, 0, 0, 255]
                    }
                );
            }
        }
    }

    #[test]
    fn compact_gdi_split_rejects_bad_headers_before_write_and_bounds_the_surface() {
        let bounds = ScreenRect {
            left: -7,
            top: -9,
            width: 5,
            height: 4,
        };
        let rect = ScreenRect {
            left: -6,
            top: -8,
            width: 3,
            height: 2,
        };
        let plan = PlaneCopyPlan::new(bounds, rect).unwrap();
        let source = [0x42_u8; 80];
        for (pointer, source_bytes, target_bytes) in [
            (source.as_ptr(), 79, plan.bytes),
            (source.as_ptr(), 81, plan.bytes),
            (source.as_ptr(), 80, plan.bytes - 1),
            (source.as_ptr(), 80, plan.bytes + 1),
            (std::ptr::null(), 80, plan.bytes),
        ] {
            let mut target = vec![MaybeUninit::new(0xa5_u8); target_bytes];
            assert!(unsafe { plan.initialize(pointer, source_bytes, &mut target) }.is_err());
            assert!(target
                .into_iter()
                .all(|byte| unsafe { byte.assume_init() } == 0xa5));
        }
        for invalid in [
            ScreenRect { left: -8, ..rect },
            ScreenRect { top: -10, ..rect },
            ScreenRect { left: -3, ..rect },
            ScreenRect { top: -6, ..rect },
            ScreenRect { width: 0, ..rect },
            ScreenRect {
                width: u32::MAX,
                ..rect
            },
            ScreenRect {
                left: i32::MAX,
                ..rect
            },
        ] {
            assert!(PlaneCopyPlan::new(bounds, invalid).is_err());
        }
        assert_eq!(compact_desktop_bytes(bounds, 80), Some(80));
        assert_eq!(compact_desktop_bytes(bounds, 64), Some(80));
        assert_eq!(compact_desktop_bytes(bounds, 63), None);
        assert_eq!(compact_desktop_bytes(bounds, 0), None);
        assert_eq!(compact_desktop_bytes(bounds, MAX_FROZEN_BYTES + 1), None);
        let sparse = ScreenRect {
            width: 16_384,
            height: 16_384,
            ..bounds
        };
        assert_eq!(compact_desktop_bytes(sparse, MAX_FROZEN_BYTES), None);
        assert!(PlaneCopyPlan::new(sparse, rect).is_err());
    }

    #[test]
    fn compact_gdi_split_moves_preallocated_planes_into_shared_preview_and_original() {
        let bounds = ScreenRect {
            left: -30,
            top: 8,
            width: 9,
            height: 3,
        };
        let rects = [
            ScreenRect { width: 4, ..bounds },
            ScreenRect {
                left: -26,
                width: 5,
                ..bounds
            },
        ];
        let mut source = Vec::new();
        for y in 0..3_u8 {
            for x in 0..9_u8 {
                source.extend_from_slice(&[x * 11, y * 29, x * 7 + y, x + y]);
            }
        }
        assert_eq!(
            compact_desktop_bytes(bounds, source.len()),
            Some(source.len())
        );
        let mut screens = Vec::new();
        let mut pointers = Vec::new();
        for rect in rects {
            let plan = PlaneCopyPlan::new(bounds, rect).unwrap();
            let mut bgra = Vec::new();
            bgra.try_reserve_exact(plan.bytes).unwrap();
            let pointer = bgra.as_ptr();
            let capacity = bgra.capacity();
            unsafe {
                plan.initialize(
                    source.as_ptr(),
                    source.len(),
                    &mut bgra.spare_capacity_mut()[..plan.bytes],
                )
                .unwrap();
                bgra.set_len(plan.bytes);
            }
            assert_eq!(bgra.as_ptr(), pointer);
            assert_eq!(bgra.capacity(), capacity);
            assert_eq!(bgra.len(), plan.bytes);
            for y in 0..rect.height as usize {
                for x in 0..rect.width as usize {
                    let global_x = (rect.left - bounds.left) as usize + x;
                    let offset = (y * rect.width as usize + x) * 4;
                    assert_eq!(
                        bgra[offset..offset + 4],
                        [
                            global_x as u8 * 11,
                            y as u8 * 29,
                            global_x as u8 * 7 + y as u8,
                            255
                        ]
                    );
                }
            }
            let screen = FrozenScreen::from_pixels(rect, bgra).unwrap();
            assert_eq!(screen.preview.as_bytes(0).unwrap().as_ptr(), pointer);
            pointers.push(pointer);
            screens.push(screen);
        }
        let desktop = FrozenDesktop::from_screens(screens, POINT { x: -29, y: 9 }, 0.).unwrap();
        let (source, previews) = CanvasSource::frozen_desktop(desktop);
        assert_eq!(previews.len(), rects.len());
        for (i, preview) in previews.iter().enumerate() {
            assert_eq!(preview.as_bytes(0).unwrap().as_ptr(), pointers[i]);
            assert!(std::ptr::eq(source.original(preview), preview.as_ref()));
            assert_eq!(source.screen_rect(i), rects[i].local(bounds));
        }
    }

    #[test]
    fn opaque_bgra_initialization_handles_unaligned_simd_tails_without_touching_guards() {
        for pixels in [0, 1, 2, 3, 4, 5, 17] {
            let bytes = pixels * 4;
            let mut source = vec![0x5a_u8; bytes + 32];
            let start = if (source.as_ptr() as usize + 1).is_multiple_of(16) {
                2
            } else {
                1
            };
            assert!(!(source[start..].as_ptr() as usize).is_multiple_of(16));
            let mut random = 0x1357_2468_u32;
            for byte in &mut source[start..start + bytes] {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = (random >> 24) as u8;
            }
            let before = source.clone();
            let mut target = vec![MaybeUninit::new(0xa5_u8); bytes + 32];
            let dest = if (target.as_ptr() as usize + 1).is_multiple_of(16) {
                2
            } else {
                1
            };
            assert!(!(target[dest..].as_ptr() as usize).is_multiple_of(16));
            initialize_opaque_bgra(
                &source[start..start + bytes],
                &mut target[dest..dest + bytes],
            );
            // 目标 guard 原先已初始化；helper 初始化了其余被测试字节。
            let actual: Vec<_> = target
                .into_iter()
                .map(|byte| unsafe { byte.assume_init() })
                .collect();
            assert!(actual[..dest].iter().all(|&byte| byte == 0xa5));
            assert!(actual[dest + bytes..].iter().all(|&byte| byte == 0xa5));
            for index in 0..bytes {
                let expected = if index % 4 == 3 {
                    255
                } else {
                    source[start + index]
                };
                assert_eq!(
                    actual[dest + index],
                    expected,
                    "{pixels} 像素第 {index} 字节"
                );
            }
            assert_eq!(source, before, "源像素与前后边界不可被修改");
        }
    }

    #[test]
    fn frozen_desktop_negative_origins_crop_across_black_gaps_losslessly() {
        let red = [3, 7, 211, 255];
        let green = [11, 199, 29, 255];
        let desktop = FrozenDesktop::from_screens(
            vec![
                FrozenScreen::from_pixels(
                    ScreenRect {
                        left: -4,
                        top: -2,
                        width: 2,
                        height: 4,
                    },
                    red.repeat(8),
                )
                .unwrap(),
                FrozenScreen::from_pixels(
                    ScreenRect {
                        left: 0,
                        top: 0,
                        width: 3,
                        height: 2,
                    },
                    green.repeat(6),
                )
                .unwrap(),
            ],
            POINT { x: 1, y: 1 },
            4.5,
        )
        .unwrap();
        assert_eq!((desktop.bounds.left, desktop.bounds.top), (-4, -2));
        assert_eq!((desktop.bounds.width, desktop.bounds.height), (7, 4));
        assert_eq!(desktop.current, 1);
        let (source, previews) = CanvasSource::frozen_desktop(desktop);
        assert!(!source.is_image());
        assert_eq!(source.screen_count(), 2);
        assert_eq!(source.monitor_at((0, 0)), 0);
        assert_eq!(source.monitor_at((5, 3)), 1);
        assert_eq!(source.capture_ms, 4.5);
        assert_eq!(previews.len(), 2);
        let cropped = source
            .read_region(Rect {
                x: 1,
                y: 1,
                width: 5,
                height: 3,
            })
            .unwrap();
        for y in 0..3 {
            for x in 0..5 {
                let expected = if x == 0 {
                    red
                } else if x >= 3 && y >= 1 {
                    green
                } else {
                    [0, 0, 0, 255]
                };
                let offset = (y * 5 + x) * 4;
                assert_eq!(&cropped.bgra[offset..offset + 4], &expected);
            }
        }
    }

    #[test]
    fn frozen_desktop_preview_and_original_share_the_captured_allocation() {
        let pixels = [17, 31, 229, 255].repeat(12);
        let captured = pixels.as_ptr();
        let screen = FrozenScreen::from_pixels(
            ScreenRect {
                left: -30,
                top: 10,
                width: 4,
                height: 3,
            },
            pixels,
        )
        .unwrap();
        assert_eq!(screen.preview.as_bytes(0).unwrap().as_ptr(), captured);
        let retained = screen.preview.clone();
        let desktop =
            FrozenDesktop::from_screens(vec![screen], POINT { x: -28, y: 11 }, 0.).unwrap();
        let (source, previews) = CanvasSource::frozen_desktop(desktop);
        assert!(Arc::ptr_eq(&retained, &previews[0]));
        assert_eq!(
            source.original(&previews[0]).as_bytes(0).unwrap().as_ptr(),
            captured
        );
        assert_eq!(previews[0].as_bytes(0).unwrap().as_ptr(), captured);
        assert!(source.read_region(Rect::full(5, 3)).is_err());

        let unavailable = NativeCaptureError {
            operation: "BitBlt",
            source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
                windows::Win32::Foundation::ERROR_NOT_SUPPORTED.0,
            )),
        };
        assert!(can_fallback(&unavailable));
        for code in [
            ERROR_NOT_ENOUGH_MEMORY,
            ERROR_OUTOFMEMORY,
            ERROR_INSUFFICIENT_VIRTUAL_ADDR_RESOURCES,
            ERROR_NOT_ENOUGH_SERVER_MEMORY,
            ERROR_PAGEFILE_QUOTA_EXCEEDED,
            ERROR_NO_SYSTEM_RESOURCES,
            ERROR_NONPAGED_SYSTEM_RESOURCES,
            ERROR_PAGED_SYSTEM_RESOURCES,
            ERROR_WORKING_SET_QUOTA,
            ERROR_PAGEFILE_QUOTA,
            ERROR_COMMITMENT_LIMIT,
            ERROR_NOT_ENOUGH_QUOTA,
        ] {
            let memory = NativeCaptureError {
                operation: "CreateDIBSection",
                source: windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(
                    code.0,
                )),
            };
            assert!(!can_fallback(&memory), "内存错误 {:?} 不得回退", code);
        }
        for (width, height) in [(16_385, 1), (16_384, 16_384), (0, 1)] {
            let limit = image_bytes(width, height).unwrap_err();
            assert!(!can_fallback(limit.as_ref()));
        }
        let allocation = Vec::<u8>::new().try_reserve_exact(usize::MAX).unwrap_err();
        assert!(!can_fallback(&allocation));
        assert!(!can_fallback(&unavailable.source));
    }
}
