//! 冻结鼠标指针；在底图片段上用 GDI 合成，兼容彩色、透明和单色 XOR 指针。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, SourcePixels},
    Result,
};
use std::{
    mem::size_of,
    sync::{mpsc, Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};
use windows::Win32::{
    Foundation::POINT,
    Graphics::Gdi::*,
    System::Threading::GetCurrentThreadId,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

// 与 startup_probe 相同的 kernel32 标量计时 ABI；不新增 windows feature/Cargo 依赖。
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}

/// 只冻结本次鼠标位置和图标句柄；像素合成留到首屏可操作之后。
pub(crate) struct CursorSnapshot {
    icon: CopiedIcon,
    position: POINT,
}
// CopyIcon 返回本进程独占的 USER 图标对象，可跨线程使用/销毁；不携带 DC 或窗口。
// 用不透明句柄值传递其所有权，所有原生访问仍在本模块内。
struct CopiedIcon(usize);
impl CopiedIcon {
    fn raw(&self) -> HICON {
        HICON(self.0 as *mut _)
    }
}
impl Drop for CopiedIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.raw());
        }
    }
}

// Owner 持有清理责任；同一票据显式随 ForegroundGate/PreparationGate 移交。
// 没有全局任务槽；锁只移动自己的任务，从不在锁内 join 或调用 USER。
#[derive(Clone)]
pub(crate) struct CursorTaskTicket(Arc<Mutex<Option<IdleCursorTask>>>);
pub(crate) struct StandbyCursorGuard(CursorTaskTicket);
impl StandbyCursorGuard {
    pub(crate) fn ticket(&self) -> CursorTaskTicket {
        self.0.clone()
    }
}
enum CursorCommand {
    Capture(Option<CursorTraceTicket>),
    Stop,
}
struct IdleCursorTask {
    commands: mpsc::Sender<CursorCommand>,
    worker: Option<JoinHandle<Option<CursorSnapshot>>>,
}
impl Drop for IdleCursorTask {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.commands.send(CursorCommand::Stop);
            drop(worker.join());
        }
    }
}
impl Drop for StandbyCursorGuard {
    fn drop(&mut self) {
        let task = {
            self.0
                 .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        };
        drop(task); // 未消费：Stop 后 join；已消费：Pending 保有原 worker。
    }
}
struct CursorDpi(Option<DPI_AWARENESS_CONTEXT>);
impl CursorDpi {
    fn new() -> Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0.is_null() {
            return Err("光标待命线程设置物理 DPI 失败".into());
        }
        Ok(Self(Some(previous)))
    }
    fn restore(&mut self) -> bool {
        let Some(previous) = self.0 else {
            return true;
        };
        if unsafe { SetThreadDpiAwarenessContext(previous) }
            .0
            .is_null()
        {
            return false;
        }
        self.0 = None;
        true
    }
}
impl Drop for CursorDpi {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
// 仅用预定义资源暖一次 USER 图标复制路径；不采样或设置当前系统指针。
// LoadCursor 返回的 stock 原件不归本进程；仅 CopyIcon 副本由此 RAII 管理。
#[derive(Debug)]
enum CursorInitializationError {
    Native(String),
    StockCopyCleanup(String),
}
impl std::fmt::Display for CursorInitializationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native(message) => write!(formatter, "光标待命线程初始化失败：{message}"),
            Self::StockCopyCleanup(message) => {
                write!(
                    formatter,
                    "标准光标复制预热清理未确认，禁止发布待命 Ready：{message}"
                )
            }
        }
    }
}
impl std::error::Error for CursorInitializationError {}
pub(crate) fn stock_copy_cleanup_failed(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<CursorInitializationError>()
        .is_some_and(|error| matches!(error, CursorInitializationError::StockCopyCleanup(_)))
}
struct DiscardStockIcon(Option<HICON>);
impl DiscardStockIcon {
    fn destroy(&mut self) -> windows::core::Result<()> {
        if let Some(icon) = self.0 {
            unsafe { DestroyIcon(icon) }?;
            self.0 = None;
        }
        Ok(())
    }
}
impl Drop for DiscardStockIcon {
    fn drop(&mut self) {
        // 显式销毁失败时再尽力一次；即使重试成功，原失败仍禁止此 worker 待命。
        let _ = self.destroy();
    }
}
fn prewarm_stock_icon_copy() -> std::result::Result<(), CursorInitializationError> {
    let stock = unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(|error| {
        CursorInitializationError::Native(format!("载入预定义箭头失败：{error}"))
    })?;
    let icon = unsafe { CopyIcon(HICON(stock.0)) }.map_err(|error| {
        CursorInitializationError::Native(format!("复制预定义箭头失败：{error}"))
    })?;
    let mut icon = DiscardStockIcon(Some(icon));
    icon.destroy()
        .map_err(|error| CursorInitializationError::StockCopyCleanup(error.to_string()))?;
    Ok(())
}
pub(crate) fn precreate_standby() -> Result<StandbyCursorGuard> {
    precreate_with(|trace, dpi| requested_snapshot(trace, "precreated", Some(dpi)))
}
fn precreate_with(
    copy: impl FnOnce(Option<CursorTraceTicket>, CursorDpi) -> Option<CursorSnapshot> + Send + 'static,
) -> Result<StandbyCursorGuard> {
    let (commands, receive) = mpsc::channel();
    let (ready, initialized) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("rustshot-cursor-copy".into())
        .spawn(move || {
            let dpi = match CursorDpi::new() {
                Ok(dpi) => dpi,
                Err(error) => {
                    let _ = ready.send(Err(CursorInitializationError::Native(error.to_string())));
                    return None;
                }
            };
            // 只创建本线程 USER 队列。false 表示没有匹配消息，不是初始化失败。
            // 不创建 HWND、不采样系统指针、不取得 DC/桌面 bitmap/应用 GPU；不保留图标副本。
            {
                let mut message = MSG::default();
                let _ = unsafe { PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE) };
            }
            if let Err(error) = prewarm_stock_icon_copy() {
                let _ = ready.send(Err(error));
                return None;
            }
            if ready.send(Ok(())).is_err() {
                return None;
            }
            match receive.recv() {
                Ok(CursorCommand::Capture(trace)) => copy(trace, dpi),
                Ok(CursorCommand::Stop) | Err(_) => None,
            }
        })?;
    let task = IdleCursorTask {
        commands,
        worker: Some(worker),
    };
    initialized
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| format!("光标待命线程 ready 失败：{error}"))?
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)?;
    Ok(StandbyCursorGuard(CursorTaskTicket(Arc::new(Mutex::new(
        Some(task),
    )))))
}

#[derive(Clone, Default, serde::Serialize)]
struct CursorRequestTiming {
    request_qpc: Option<i64>,
    dispatch_qpc: Option<i64>,
    receive_qpc: Option<i64>,
    query_begin_qpc: Option<i64>,
    query_end_qpc: Option<i64>,
    copy_begin_qpc: Option<i64>,
    copy_done_qpc: Option<i64>,
    copy_cpu_envelope: Option<crate::startup_probe::ThreadCpuEnvelope>,
    dpi_restore_begin_qpc: Option<i64>,
    dpi_restore_end_qpc: Option<i64>,
    worker_before_return_qpc: Option<i64>,
    join_complete_qpc: Option<i64>,
    worker_thread_id: u32,
    path: &'static str,
    query_succeeded: bool,
    cursor_showing: bool,
    copy_succeeded: bool,
    dpi_restore_first_attempt_succeeded: Option<bool>,
    join_succeeded: Option<bool>,
}
#[derive(Clone)]
pub(crate) struct CursorTraceTicket(Arc<Mutex<CursorRequestTiming>>);
fn cursor_qpc() -> Option<i64> {
    let mut value = 0;
    (unsafe { QueryPerformanceCounter(&mut value) } != 0).then_some(value)
}
impl CursorTraceTicket {
    fn begin() -> Option<Self> {
        std::env::var_os("RUSTSHOT_STARTUP_TRACE")
            .is_some()
            .then(|| {
                Self(Arc::new(Mutex::new(CursorRequestTiming {
                    request_qpc: cursor_qpc(),
                    ..Default::default()
                })))
            })
    }
    fn update(&self, update: impl FnOnce(&mut CursorRequestTiming)) {
        update(&mut self.0.lock().unwrap_or_else(|error| error.into_inner()));
    }
    pub(crate) fn emit_after_ready(&self) {
        let timing = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut frequency = 0;
        let frequency =
            (unsafe { QueryPerformanceFrequency(&mut frequency) } != 0).then_some(frequency);
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "phase": "startup_cursor_request_timing", "pid": std::process::id(),
            "qpc_frequency": frequency, "timing": timing,
            "emitted_after_native_ready": true,
            "thread_return_is_pre_return_marker": true,
            "join_confirms_natural_thread_exit": timing.join_succeeded == Some(true),
            "cpu_observation_scope": "Trace=1 only; before/after samples surround original Copy QPC interval; cumulative thread CPU in 100ns and raw cycles; wider envelope includes observation overhead; failures are null with error codes, not zero",
        }));
    }
}
fn requested_snapshot(
    trace: Option<CursorTraceTicket>,
    path: &'static str,
    mut dpi: Option<CursorDpi>,
) -> Option<CursorSnapshot> {
    let receive_qpc = trace.as_ref().and_then(|_| cursor_qpc());
    let mut timing = trace.as_ref().map(|ticket| {
        let mut timing = ticket
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        timing.receive_qpc = receive_qpc;
        timing.worker_thread_id = unsafe { GetCurrentThreadId() };
        timing.path = path;
        timing
    });
    let cpu_trace =
        timing.is_some() && std::env::var("RUSTSHOT_STARTUP_TRACE").as_deref() == Ok("1");
    // Trace 仅记录标量；查询与复制之间不锁 Mutex、不格式化/输出日志。
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if let Some(timing) = timing.as_mut() {
        timing.query_begin_qpc = cursor_qpc();
    }
    let query_succeeded = unsafe { GetCursorInfo(&mut cursor) }.is_ok();
    if let Some(timing) = timing.as_mut() {
        timing.query_end_qpc = cursor_qpc();
        timing.query_succeeded = query_succeeded;
        timing.cursor_showing = query_succeeded && cursor.flags == CURSOR_SHOWING;
    }
    let snapshot = if query_succeeded && cursor.flags == CURSOR_SHOWING {
        // Normal 不查询 CPU；前后采样在原 Copy QPC 区间外，不覆盖调用错误。
        let mut cpu = cpu_trace.then(crate::startup_probe::ThreadCpuProbe::begin);
        if let Some(timing) = timing.as_mut() {
            timing.copy_begin_qpc = cursor_qpc();
            if let Some(cpu) = cpu.as_mut() {
                cpu.mark_begin(timing.copy_begin_qpc);
            }
        }
        let snapshot = copy_icon(cursor.hCursor.0 as usize, cursor.ptScreenPos);
        if let Some(timing) = timing.as_mut() {
            timing.copy_done_qpc = cursor_qpc();
            timing.copy_succeeded = snapshot.is_some();
            timing.copy_cpu_envelope = cpu.map(|cpu| cpu.finish(timing.copy_done_qpc));
        }
        snapshot
    } else {
        None
    };
    if let Some(dpi) = dpi.as_mut() {
        if let Some(timing) = timing.as_mut() {
            timing.dpi_restore_begin_qpc = cursor_qpc();
        }
        let restored = dpi.restore();
        if let Some(timing) = timing.as_mut() {
            timing.dpi_restore_first_attempt_succeeded = Some(restored);
        }
    }
    drop(dpi); // restore 首次失败时 Drop 再试；end 包含整个外层 RAII Drop。
    if let Some(mut timing) = timing {
        if timing.dpi_restore_begin_qpc.is_some() {
            timing.dpi_restore_end_qpc = cursor_qpc();
        }
        timing.worker_before_return_qpc = cursor_qpc();
        trace
            .expect("timing only exists with trace")
            .update(|target| *target = timing);
    }
    snapshot
}

pub(crate) struct PendingCursorSnapshot {
    worker: Option<JoinHandle<Option<CursorSnapshot>>>,
    immediate: Option<CursorSnapshot>,
    trace: Option<CursorTraceTicket>,
}
impl PendingCursorSnapshot {
    pub(crate) fn begin() -> Self {
        Self::launch_requested(CursorTraceTicket::begin())
    }
    pub(crate) fn begin_precreated(ticket: CursorTaskTicket) -> Self {
        let trace = CursorTraceTicket::begin();
        let task = ticket
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut task) = task {
            if let Some(trace) = trace.as_ref() {
                trace.update(|timing| timing.dispatch_qpc = cursor_qpc());
            }
            if task
                .commands
                .send(CursorCommand::Capture(trace.clone()))
                .is_ok()
            {
                return Self {
                    worker: task.worker.take(),
                    immediate: None,
                    trace,
                };
            }
            drop(task); // ready 后异常退出：先回收原线程，再保留冷路径。
            return Self::launch_requested(trace);
        }
        // 显式票据已被消费/Owner 已 Stop，不能再启动第二次新采样。
        Self::empty()
    }
    pub(crate) fn empty() -> Self {
        Self {
            worker: None,
            immediate: None,
            trace: None,
        }
    }
    fn launch_requested(trace: Option<CursorTraceTicket>) -> Self {
        let Some(trace) = trace else {
            return Self::launch(snapshot);
        };
        trace.update(|timing| timing.dispatch_qpc = cursor_qpc());
        let thread_trace = trace.clone();
        match std::thread::Builder::new()
            .name("rustshot-cursor-copy".into())
            .spawn(move || requested_snapshot(Some(thread_trace), "cold-thread", None))
        {
            Ok(worker) => Self {
                worker: Some(worker),
                immediate: None,
                trace: Some(trace),
            },
            Err(_) => {
                let immediate = requested_snapshot(Some(trace.clone()), "cold-synchronous", None);
                Self {
                    worker: None,
                    immediate,
                    trace: Some(trace),
                }
            }
        }
    }
    // 保留旧冷路径和自有 CopyIcon ownership 测试入口。
    fn launch(copy: impl Fn() -> Option<CursorSnapshot> + Send + Copy + 'static) -> Self {
        match std::thread::Builder::new()
            .name("rustshot-cursor-copy".into())
            .spawn(copy)
        {
            Ok(worker) => Self {
                worker: Some(worker),
                immediate: None,
                trace: None,
            },
            Err(_) => Self {
                worker: None,
                immediate: copy(),
                trace: None,
            },
        }
    }
    pub(crate) fn trace_ticket(&self) -> Option<CursorTraceTicket> {
        self.trace.clone()
    }
    pub(crate) fn finish(mut self) -> Option<CursorSnapshot> {
        match self.worker.take() {
            Some(worker) => {
                let result = worker.join();
                if let Some(trace) = self.trace.as_ref() {
                    trace.update(|timing| {
                        timing.join_complete_qpc = cursor_qpc();
                        timing.join_succeeded = Some(result.is_ok());
                    });
                }
                result.ok().flatten()
            }
            None => self.immediate.take(),
        }
    }
}
impl Drop for PendingCursorSnapshot {
    fn drop(&mut self) {
        // 已消费请求的复制线程会自然退出；失败时返回的独占图标也随 join 结果释放。
        if let Some(worker) = self.worker.take() {
            drop(worker.join());
        }
    }
}
fn snapshot() -> Option<CursorSnapshot> {
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut cursor) }.ok()?;
    if cursor.flags != CURSOR_SHOWING {
        return None;
    }
    // GetCursorInfo / CopyIcon 是系统的 best-effort 相邻采样，不能原子冻结外部进程。
    copy_icon(cursor.hCursor.0 as usize, cursor.ptScreenPos)
}
fn copy_icon(icon: usize, position: POINT) -> Option<CursorSnapshot> {
    unsafe { CopyIcon(HICON(icon as *mut _)) }
        .ok()
        .map(|icon| CursorSnapshot {
            icon: CopiedIcon(icon.0 as usize),
            position,
        })
}

pub struct CapturedCursor {
    pub rect: Rect,
    pub image: Image,
}
impl CapturedCursor {
    pub fn apply(&self, image: &mut Image, region: Rect) {
        if let Some(part) = self.rect.intersection(region) {
            for y in part.y..part.y + part.height {
                let src =
                    (((y - self.rect.y) * self.rect.width + part.x - self.rect.x) as usize) * 4;
                let dst = (((y - region.y) * region.width + part.x - region.x) as usize) * 4;
                let n = part.width as usize * 4;
                image.bgra[dst..dst + n].copy_from_slice(&self.image.bgra[src..src + n]);
            }
        }
    }
    pub fn plane(&self) -> PixelPlane<'_> {
        PixelPlane {
            rect: self.rect,
            bgra: &self.image.bgra,
        }
    }
}
struct IconBits(ICONINFO);
impl Drop for IconBits {
    fn drop(&mut self) {
        unsafe {
            if !self.0.hbmColor.is_invalid() {
                let _ = DeleteObject(self.0.hbmColor.into());
            }
            if !self.0.hbmMask.is_invalid() {
                let _ = DeleteObject(self.0.hbmMask.into());
            }
        }
    }
}
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
impl Surface {
    fn new(image: &Image) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("无法创建指针绘制设备".into());
            }
            let info = crate::gdi_surface::bitmap_info(image.width, image.height);
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e.into());
                }
            };
            let old = SelectObject(dc, bitmap.into());
            std::ptr::copy_nonoverlapping(image.bgra.as_ptr(), bits.cast(), image.bgra.len());
            Ok(Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
            })
        }
    }
}
pub(crate) fn compose(
    snapshot: &CursorSnapshot,
    source: &SourcePixels<'_>,
    global: ScreenRect,
) -> Result<Option<CapturedCursor>> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(snapshot.icon.raw(), &mut info)?;
        let info = IconBits(info);
        let mut bitmap = BITMAP::default();
        let color = !info.0.hbmColor.is_invalid();
        if GetObjectW(
            if color {
                info.0.hbmColor
            } else {
                info.0.hbmMask
            }
            .into(),
            size_of::<BITMAP>() as i32,
            Some((&mut bitmap as *mut BITMAP).cast()),
        ) == 0
        {
            return Err("无法读取鼠标指针尺寸".into());
        }
        let width = bitmap.bmWidth;
        let height = bitmap.bmHeight / if color { 1 } else { 2 };
        if width <= 0 || height <= 0 || width > 1024 || height > 1024 {
            return Err("鼠标指针尺寸不支持".into());
        }
        let left = snapshot.position.x - info.0.xHotspot as i32 - global.left;
        let top = snapshot.position.y - info.0.yHotspot as i32 - global.top;
        let x = left.max(0) as u32;
        let y = top.max(0) as u32;
        let right = (left + width).min(global.width as i32);
        let bottom = (top + height).min(global.height as i32);
        if right <= x as i32 || bottom <= y as i32 {
            return Ok(None);
        }
        let rect = Rect {
            x,
            y,
            width: right as u32 - x,
            height: bottom as u32 - y,
        };
        let mut image = source.read(rect);
        let surface = Surface::new(&image)?;
        DrawIconEx(
            surface.dc,
            left - x as i32,
            top - y as i32,
            snapshot.icon.raw(),
            width,
            height,
            0,
            None,
            DI_NORMAL,
        )?;
        GdiFlush().ok()?;
        let bytes = image.bgra.len();
        image
            .bgra
            .copy_from_slice(std::slice::from_raw_parts(surface.bits, bytes));
        for pixel in image.bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Ok(Some(CapturedCursor { rect, image }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_cursor_survives_original_destroy_and_preserves_xor_pixels() {
        struct FixtureCursor(Option<HCURSOR>);
        impl Drop for FixtureCursor {
            fn drop(&mut self) {
                if let Some(cursor) = self.0.take() {
                    unsafe {
                        let _ = DestroyCursor(cursor);
                    }
                }
            }
        }
        let and_mask = [0xff_u8; 128];
        let mut xor_mask = [0_u8; 128];
        xor_mask[0] = 0x80;
        let instance =
            unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap() };
        let mut original = FixtureCursor(Some(unsafe {
            CreateCursor(
                Some(instance.into()),
                0,
                0,
                32,
                32,
                and_mask.as_ptr().cast(),
                xor_mask.as_ptr().cast(),
            )
            .unwrap()
        }));
        let original_handle = original.0.unwrap().0 as usize;
        let pending = PendingCursorSnapshot::launch(move || {
            copy_icon(original_handle, POINT { x: -36, y: 20 })
        });
        assert!(pending.worker.is_some(), "本测试必须走后台复制线程");
        let snapshot = pending.finish().expect("独立图标复制失败");
        unsafe {
            DestroyCursor(original.0.take().unwrap()).unwrap();
        }
        let background = [10, 20, 30, 255].repeat(48 * 48);
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(48, 48),
            planes: vec![PixelPlane {
                rect: Rect::full(48, 48),
                bgra: &background,
            }],
            overlay: None,
        };
        let captured = compose(
            &snapshot,
            &source,
            ScreenRect {
                left: -37,
                top: 19,
                width: 48,
                height: 48,
            },
        )
        .unwrap()
        .expect("指针应落在冻结画布内");
        assert_eq!(
            captured.rect,
            Rect {
                x: 1,
                y: 1,
                width: 32,
                height: 32
            }
        );
        for (index, pixel) in captured.image.bgra.chunks_exact(4).enumerate() {
            assert_eq!(
                pixel,
                if index == 0 {
                    &[245, 235, 225, 255]
                } else {
                    &[10, 20, 30, 255]
                }
            );
        }
        let mut exported = source.read(source.canvas);
        captured.apply(&mut exported, source.canvas);
        let preview_source = SourcePixels {
            fill: None,
            canvas: source.canvas,
            planes: source.planes,
            overlay: Some(captured.plane()),
        };
        assert_eq!(preview_source.read(source.canvas).bgra, exported.bgra);
    }
    #[test]
    fn cursor_composition_and_effect_sampling_use_same_clipped_physical_pixels() {
        let cursor = CapturedCursor {
            rect: Rect {
                x: 9,
                y: 8,
                width: 4,
                height: 4,
            },
            image: Image {
                width: 4,
                height: 4,
                bgra: [15, 25, 35, 255].repeat(16),
                readback_ms: 0.,
            },
        };
        let region = Rect {
            x: 10,
            y: 9,
            width: 6,
            height: 4,
        };
        let mut image = Image {
            width: 6,
            height: 4,
            bgra: [0, 0, 0, 255].repeat(24),
            readback_ms: 0.,
        };
        cursor.apply(&mut image, region);
        let background = [0, 0, 0, 255].repeat(20 * 20);
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(20, 20),
            planes: vec![PixelPlane {
                rect: Rect::full(20, 20),
                bgra: &background,
            }],
            overlay: Some(cursor.plane()),
        };
        assert_eq!(source.read(region).bgra, image.bgra);
        assert_eq!(&image.bgra[0..4], &[15, 25, 35, 255]);
        assert_eq!(&image.bgra[3 * 4..4 * 4], &[0, 0, 0, 255]);
    }
}

#[cfg(test)]
mod precreated_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn idle_ticket_stop_and_one_shot_copy_preserve_ownership() {
        let stopped_calls = Arc::new(AtomicUsize::new(0));
        let count = stopped_calls.clone();
        let stopped = precreate_with(move |_, _dpi| {
            count.fetch_add(1, Ordering::SeqCst);
            None
        })
        .expect("自有 USER/DPI 线程应完成 ready");
        assert_eq!(stopped_calls.load(Ordering::SeqCst), 0);
        drop(stopped); // 真正 Stop + 原 JoinHandle join；不运行注入采样。
        assert_eq!(stopped_calls.load(Ordering::SeqCst), 0);

        struct Original(Option<HCURSOR>);
        impl Drop for Original {
            fn drop(&mut self) {
                if let Some(cursor) = self.0.take() {
                    unsafe {
                        let _ = DestroyCursor(cursor);
                    }
                }
            }
        }
        let and_mask = [0xff_u8; 128];
        let mut xor_mask = [0_u8; 128];
        xor_mask[0] = 0x80;
        let instance =
            unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap() };
        let mut original = Original(Some(unsafe {
            CreateCursor(
                Some(instance.into()),
                0,
                0,
                32,
                32,
                and_mask.as_ptr().cast(),
                xor_mask.as_ptr().cast(),
            )
            .unwrap()
        }));
        let handle = original.0.unwrap().0 as usize;
        let copied_calls = Arc::new(AtomicUsize::new(0));
        let count = copied_calls.clone();
        let guard = precreate_with(move |_, _dpi| {
            count.fetch_add(1, Ordering::SeqCst);
            copy_icon(handle, POINT { x: -36, y: 20 })
        })
        .expect("预建线程应完成 ready");
        assert_eq!(copied_calls.load(Ordering::SeqCst), 0);
        let ticket = guard.ticket();
        let pending = PendingCursorSnapshot::begin_precreated(ticket.clone());
        assert!(pending.worker.is_some(), "必须接管原预建线程");
        let snapshot = pending.finish().expect("自有图标应复制成功");
        assert_eq!(copied_calls.load(Ordering::SeqCst), 1);
        assert!(PendingCursorSnapshot::begin_precreated(ticket)
            .finish()
            .is_none());
        drop(guard);
        assert_eq!(copied_calls.load(Ordering::SeqCst), 1);
        unsafe {
            DestroyCursor(original.0.take().unwrap()).unwrap();
        }

        // 原 XOR 测试仍完整保留；此处只核实新任务路径返回的独占副本可以合成。
        let background = [10, 20, 30, 255].repeat(48 * 48);
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(48, 48),
            planes: vec![PixelPlane {
                rect: Rect::full(48, 48),
                bgra: &background,
            }],
            overlay: None,
        };
        let captured = compose(
            &snapshot,
            &source,
            ScreenRect {
                left: -37,
                top: 19,
                width: 48,
                height: 48,
            },
        )
        .unwrap()
        .expect("销毁原图标后副本仍可合成");
        assert_eq!(&captured.image.bgra[..4], &[245, 235, 225, 255]);
    }
}
