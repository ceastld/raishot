//! 独占 DXGI 的抓屏生产者；最多两帧 FIFO，控制命令与排空屏障可唤醒所有等待。
use crate::{
    capture::Image, geometry::ScreenRect, live_capture::LiveCapture,
    long_capture_runtime::FrameGate, stitch::ScrollAxis, Result,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const RAW_BUDGET: u64 = 64 * 1024 * 1024;
const TRACE_LIMIT: usize = 64;
static CAPTURE_THREADS: AtomicUsize = AtomicUsize::new(0);
static PANICKED_THREADS: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn active_capture_threads() -> usize {
    CAPTURE_THREADS.load(Ordering::Acquire)
}
pub(crate) fn panicked_capture_threads() -> usize {
    PANICKED_THREADS.load(Ordering::Acquire)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Running,
    Paused,
    Single,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct FrameStamp {
    pub epoch: u64,
    pub sequence: u64,
    pub region: ScreenRect,
    pub axis: ScrollAxis,
    pub read_start: Instant,
    pub read_end: Instant,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DrainFence {
    pub from_epoch: u64,
    pub desired_epoch: u64,
    pub tail_sequence: u64,
}
pub(crate) struct CapturedFrame {
    pub image: Arc<Image>,
    pub stamp: FrameStamp,
    _permit: RawPermit,
}
struct RawPermit {
    shared: Weak<Shared>,
    bytes: u64,
}
impl Drop for RawPermit {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.reserved.fetch_sub(self.bytes, Ordering::AcqRel);
            shared.wake.notify_all();
        }
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ReadProbe {
    pub epoch: u64,
    pub sequence: u64,
    pub read_start_ms: f64,
    pub read_end_ms: f64,
    pub duration_ms: f64,
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct AutoEventProbe {
    #[serde(rename = "sequence")]
    pub sequence: u64,
    pub event: &'static str,
    pub epoch: u64,
    pub frame_sequence: u64,
    pub fence_sequence: Option<u64>,
}
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct PipelineProbe {
    pub epoch: u64,
    pub last_issued_sequence: u64,
    pub processed_sequence: u64,
    pub queue_depth: usize,
    pub peak_queue_depth: usize,
    pub capture_in_flight: bool,
    pub consumer_in_flight: bool,
    pub consumer_alive: bool,
    pub last_frame_sequence: u64,
    pub producer_alive: bool,
    pub producer_quiesced: bool,
    /// 仅 producer/queue/consumer 的瞬态许可；匹配基线持有的 Arc 另记 raw_bytes。
    pub reserved_raw_bytes: u64,
    pub peak_reserved_raw_bytes: u64,
    pub raw_bytes: u64,
    pub raw_budget_bytes: u64,
    pub max_raw_slots: usize,
    pub capture_storage_pixels: u64,
    pub capture_output_pixels: u64,
    pub capture_screen_count: usize,
    pub drain_fence: Option<DrainFence>,
    pub wait_reason: Option<&'static str>,
    pub error: Option<String>,
    pub coalesced_count: u64,
    pub allow_coalescing: bool,
    pub reads: Vec<ReadProbe>,
    pub last_post_fence: Option<u64>,
    pub auto_ignored_pre_post_frames: u64,
    pub auto_events: Vec<AutoEventProbe>,
}
pub(crate) trait CaptureSource: Send {
    fn read(&mut self, region: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>>;
    fn storage_probe(&self) -> (u64, u64, usize) {
        (0, 0, 0)
    }
    fn release(&mut self) {}
    /// 重建采集器前告知可读范围；选区在该范围内实时移动时不必重建。
    fn prepare(&mut self, _reach: ScreenRect) {}
}
pub(crate) struct CaptureConfig {
    pub region: ScreenRect,
    /// 采集器初始化覆盖的范围；同尺寸选区在其中移动可沿用同一采集器继续拼接。
    pub reach: ScreenRect,
    pub frame_bytes: u64,
    pub budget: u64,
    pub slots: usize,
    pub gate: Option<FrameGate>,
}
pub(crate) struct RetiredCapture {
    _context: Arc<CaptureConfig>,
    _frames: VecDeque<CapturedFrame>,
}
impl CaptureConfig {
    #[cfg(test)]
    pub(crate) fn new(region: ScreenRect, gate: Option<FrameGate>) -> Result<Arc<Self>> {
        Self::new_reaching(region, region, gate)
    }
    pub(crate) fn new_reaching(
        region: ScreenRect,
        reach: ScreenRect,
        gate: Option<FrameGate>,
    ) -> Result<Arc<Self>> {
        if !rect_within(region, reach) {
            return Err("采集选区超出采集器范围".into());
        }
        let frame_bytes = u64::from(region.width)
            .checked_mul(u64::from(region.height))
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n > 0)
            .ok_or("采集尺寸无效")?;
        let slots = (RAW_BUDGET / frame_bytes).clamp(1, 4) as usize;
        Ok(Arc::new(Self {
            region,
            reach,
            frame_bytes,
            budget: frame_bytes * slots as u64,
            slots,
            gate,
        }))
    }
    /// 同一采集器内的同尺寸新位置：沿用读回预算，只换读取的物理矩形。
    pub(crate) fn relocated(
        &self,
        region: ScreenRect,
        gate: Option<FrameGate>,
    ) -> Result<Arc<Self>> {
        if region.width != self.region.width || region.height != self.region.height {
            return Err("实时移动选区必须保持尺寸".into());
        }
        Self::new_reaching(region, self.reach, gate)
    }
    /// 两份配置可共用同一采集器与在途帧：只是选区在同一范围内移动。
    fn shares_source(&self, other: &Self) -> bool {
        self.reach == other.reach
            && self.frame_bytes == other.frame_bytes
            && self.region.width == other.region.width
    }
}
fn rect_within(inner: ScreenRect, outer: ScreenRect) -> bool {
    let edges = |r: ScreenRect| {
        (
            i64::from(r.left),
            i64::from(r.top),
            i64::from(r.left) + i64::from(r.width),
            i64::from(r.top) + i64::from(r.height),
        )
    };
    let (a, b) = (edges(inner), edges(outer));
    a.0 >= b.0 && a.1 >= b.1 && a.2 <= b.2 && a.3 <= b.3
}
/// 选区所在显示器与选区的并集：拖动选区沿滚动方向移动时不出这块范围。
pub(crate) fn monitor_reach(region: ScreenRect) -> ScreenRect {
    use windows::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    };
    let rect = RECT {
        left: region.left,
        top: region.top,
        right: region
            .left
            .saturating_add(region.width.min(i32::MAX as u32) as i32),
        bottom: region
            .top
            .saturating_add(region.height.min(i32::MAX as u32) as i32),
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return region;
    }
    let m = info.rcMonitor;
    let (Ok(width), Ok(height)) = (
        u32::try_from(i64::from(m.right) - i64::from(m.left)),
        u32::try_from(i64::from(m.bottom) - i64::from(m.top)),
    ) else {
        return region;
    };
    let screen = ScreenRect {
        left: m.left,
        top: m.top,
        width,
        height,
    };
    ScreenRect::union(&[screen, region]).unwrap_or(region)
}
struct DxgiSource {
    live: Option<LiveCapture>,
    reach: Option<ScreenRect>,
}
impl CaptureSource for DxgiSource {
    fn read(&mut self, region: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>> {
        if self.live.is_none() {
            let reach = self.reach.unwrap_or(region);
            self.live = Some(LiveCapture::new_for_region_cancellable(reach, cancelled)?);
        }
        self.live
            .as_ref()
            .ok_or("采集器尚未初始化")?
            .read_cancellable(region, cancelled)
            .map(Arc::new)
    }
    fn storage_probe(&self) -> (u64, u64, usize) {
        self.live
            .as_ref()
            .map_or((0, 0, 0), LiveCapture::storage_probe)
    }
    fn release(&mut self) {
        self.live.take();
    }
    fn prepare(&mut self, reach: ScreenRect) {
        self.reach = Some(reach);
    }
}
struct State {
    context: Arc<CaptureConfig>,
    epoch: u64,
    axis: ScrollAxis,
    running: bool,
    single: bool,
    stop: bool,
    issued: u64,
    processed: u64,
    in_flight: bool,
    alive: bool,
    quiesced: bool,
    queue: VecDeque<CapturedFrame>,
    peak_queue: usize,
    peak_reserved: u64,
    raw: Vec<Weak<Image>>,
    storage: (u64, u64, usize),
    drain: Option<DrainFence>,
    error: Option<String>,
    wait_reason: Option<&'static str>,
    reads: VecDeque<ReadProbe>,
    coalesced: u64,
    allow_coalescing: bool,
    revision: u64,
    interval: Duration,
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    reserved: AtomicU64,
    started: Instant,
}
#[derive(Clone)]
pub(crate) struct CapturePipeline {
    shared: Arc<Shared>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}
impl CapturePipeline {
    pub(crate) fn start(
        region: ScreenRect,
        interval: Duration,
        gate: Option<FrameGate>,
    ) -> Result<Self> {
        let context = CaptureConfig::new_reaching(region, monitor_reach(region), gate)?;
        let source = DxgiSource {
            live: None,
            reach: None,
        };
        Self::start_with_context(context, interval, Box::new(source))
    }
    #[cfg(test)]
    pub(crate) fn start_with_source(
        region: ScreenRect,
        interval: Duration,
        gate: Option<FrameGate>,
        source: Box<dyn CaptureSource>,
    ) -> Result<Self> {
        Self::start_with_context(CaptureConfig::new(region, gate)?, interval, source)
    }
    fn start_with_context(
        context: Arc<CaptureConfig>,
        interval: Duration,
        source: Box<dyn CaptureSource>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                context,
                epoch: 0,
                axis: ScrollAxis::Vertical,
                running: false,
                single: false,
                stop: false,
                issued: 0,
                processed: 0,
                in_flight: false,
                alive: true,
                quiesced: false,
                queue: VecDeque::with_capacity(2),
                peak_queue: 0,
                peak_reserved: 0,
                raw: Vec::new(),
                storage: (0, 0, 0),
                drain: None,
                error: None,
                wait_reason: None,
                reads: VecDeque::with_capacity(TRACE_LIMIT),
                coalesced: 0,
                allow_coalescing: false,
                revision: 0,
                interval,
            }),
            wake: Condvar::new(),
            reserved: AtomicU64::new(0),
            started: Instant::now(),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("rustshot-long-producer".into())
            .spawn(move || {
                CAPTURE_THREADS.fetch_add(1, Ordering::AcqRel);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    producer(source, &worker_shared)
                }));
                let mut state = worker_shared
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if result.is_err() {
                    PANICKED_THREADS.fetch_add(1, Ordering::AcqRel);
                    state.error = Some("抓屏线程意外退出；最后成功结果已保留".into());
                }
                state.alive = false;
                state.in_flight = false;
                state.quiesced = true;
                state.storage = (0, 0, 0);
                worker_shared.wake.notify_all();
                CAPTURE_THREADS.fetch_sub(1, Ordering::AcqRel);
            })?;
        Ok(Self {
            shared,
            worker: Arc::new(Mutex::new(Some(worker))),
        })
    }
    pub(crate) fn configure(&self, epoch: u64, axis: ScrollAxis, mode: Mode) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = epoch;
        state.axis = axis;
        state.running = mode == Mode::Running;
        state.single = mode == Mode::Single;
        state.drain = None;
        let retired = std::mem::take(&mut state.queue);
        state.processed = state.issued;
        state.revision = state.revision.wrapping_add(1);
        state.error = None;
        state.quiesced = !state.alive;
        self.shared.wake.notify_all();
        drop(state);
        drop(retired);
    }
    pub(crate) fn context(&self) -> Arc<CaptureConfig> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .context
            .clone()
    }
    /// 运行中把同尺寸选区换到采集范围内的新位置：不换代、不清队列、不重建采集器。
    /// 选区移动对拼接而言等同滚动，已排队和在途的帧照常进入拼接。
    pub(crate) fn relocate(&self, context: Arc<CaptureConfig>) -> Result<()> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop || !state.context.shares_source(&context) {
            return Err("采集器已变化，不能实时移动选区".into());
        }
        let retired = std::mem::replace(&mut state.context, context);
        let region = state.context.region;
        for frame in &mut state.queue {
            frame.stamp.region = region;
        }
        state.revision = state.revision.wrapping_add(1);
        self.shared.wake.notify_all();
        drop(state);
        drop(retired);
        Ok(())
    }
    pub(crate) fn retarget_single(
        &self,
        expected_epoch: u64,
        epoch: u64,
        context: Arc<CaptureConfig>,
        axis: ScrollAxis,
    ) -> Result<()> {
        if epoch <= expected_epoch {
            return Err("选区代次必须递增".into());
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop
            || !state.alive
            || state.epoch != expected_epoch
            || state.running
            || state.single
            || !state.quiesced
            || state.in_flight
            || !state.queue.is_empty()
            || state.drain.is_some()
            || self.shared.reserved.load(Ordering::Acquire) != 0
        {
            return Err("请先等待采集完全暂停再调整选区".into());
        }
        let retired = std::mem::replace(&mut state.context, context);
        state.epoch = epoch;
        state.axis = axis;
        state.single = true;
        state.processed = state.issued;
        state.peak_reserved = 0;
        state.error = None;
        state.quiesced = false;
        state.revision = state.revision.wrapping_add(1);
        self.shared.wake.notify_all();
        drop(state);
        drop(retired);
        Ok(())
    }
    pub(crate) fn restore_context_if_current(
        &self,
        expected_epoch: u64,
        epoch: u64,
        context: Arc<CaptureConfig>,
        axis: ScrollAxis,
    ) -> Option<RetiredCapture> {
        if epoch <= expected_epoch {
            return None;
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop || state.epoch != expected_epoch {
            return None;
        }
        let retired_context = std::mem::replace(&mut state.context, context);
        let retired_frames = std::mem::take(&mut state.queue);
        state.epoch = epoch;
        state.axis = axis;
        state.running = false;
        state.single = false;
        state.drain = None;
        state.processed = state.issued;
        state.error = None;
        state.quiesced = !state.alive;
        // 峰值只归属当前配置；尚未退出的旧许可仍在 reserved_raw_bytes 如实显示。
        state.peak_reserved = 0;
        state.revision = state.revision.wrapping_add(1);
        self.shared.wake.notify_all();
        drop(state);
        Some(RetiredCapture {
            _context: retired_context,
            _frames: retired_frames,
        })
    }
    /// 消费者的停止决定不能覆盖 UI 已发布的新代；排空失败只能收束自己的目标代。
    pub(crate) fn pause_if_current(&self, epoch: u64, axis: ScrollAxis) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop
            || !(state.epoch == epoch && state.drain.is_none()
                || state
                    .drain
                    .is_some_and(|fence| fence.desired_epoch == epoch))
        {
            return false;
        }
        state.epoch = epoch;
        state.axis = axis;
        state.running = false;
        state.single = false;
        state.drain = None;
        let retired = std::mem::take(&mut state.queue);
        state.processed = state.issued;
        state.revision = state.revision.wrapping_add(1);
        state.error = None;
        state.quiesced = !state.alive;
        self.shared.wake.notify_all();
        drop(state);
        drop(retired);
        true
    }
    pub(crate) fn begin_drain(&self, desired_epoch: u64) -> DrainFence {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let fence = DrainFence {
            from_epoch: state.epoch,
            desired_epoch,
            tail_sequence: state.issued,
        };
        state.running = false;
        state.single = false;
        state.drain = Some(fence);
        state.revision = state.revision.wrapping_add(1);
        self.shared.wake.notify_all();
        fence
    }
    #[cfg(test)]
    pub(crate) fn try_take(&self) -> Option<CapturedFrame> {
        self.take(None)
    }
    /// 新 Single 帧可以先于 consumer apply 完成；必须留队，不能按旧消费者代次丢弃。
    pub(crate) fn try_take_for_epoch(&self, epoch: u64) -> Option<CapturedFrame> {
        self.take(Some(epoch))
    }
    fn take(&self, epoch: Option<u64>) -> Option<CapturedFrame> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if epoch.is_some_and(|epoch| {
            state
                .queue
                .front()
                .is_some_and(|frame| frame.stamp.epoch != epoch)
        }) {
            return None;
        }
        let frame = state.queue.pop_front();
        if frame.is_some() {
            state.revision = state.revision.wrapping_add(1);
        }
        self.shared.wake.notify_all();
        frame
    }
    pub(crate) fn mark_processed(&self, sequence: u64) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.processed = state.processed.max(sequence);
        self.shared.wake.notify_all();
    }
    pub(crate) fn accepts(&self, stamp: &FrameStamp) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.stop
            && state.epoch == stamp.epoch
            && stamp.region == state.context.region
            && stamp.axis == state.axis
            && state
                .drain
                .is_none_or(|d| stamp.sequence <= d.tail_sequence)
    }
    pub(crate) fn accepts_epoch(&self, epoch: u64, requested: u64) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        !state.stop
            && (epoch == requested && epoch == state.epoch
                || state
                    .drain
                    .is_some_and(|d| d.from_epoch == epoch && d.desired_epoch == requested))
    }
    pub(crate) fn drain_ready(&self, fence: &DrainFence) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.drain.as_ref() == Some(fence)
            && state.quiesced
            && !state.in_flight
            && state.queue.is_empty()
            && state.processed >= fence.tail_sequence
    }
    pub(crate) fn finish_drain(&self, fence: &DrainFence) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.drain.as_ref() != Some(fence)
            || !state.quiesced
            || state.in_flight
            || !state.queue.is_empty()
            || state.processed < fence.tail_sequence
        {
            return false;
        }
        state.epoch = fence.desired_epoch;
        state.drain = None;
        self.shared.wake.notify_all();
        true
    }
    #[cfg(test)]
    pub(crate) fn take_error(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .take()
    }
    pub(crate) fn take_error_for_epoch(&self, epoch: u64) -> Option<String> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.epoch != epoch {
            return None;
        }
        state.error.take()
    }
    /// 与 read permit 使用同一短门；成功 post 以前已经 issued 的帧不能充当响应。
    pub(crate) fn with_post_fence(
        &self,
        epoch: u64,
        post: impl FnOnce() -> Result<()>,
    ) -> Result<Option<u64>> {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop || !state.running || state.epoch != epoch || state.drain.is_some() {
            return Ok(None);
        }
        post()?;
        Ok(Some(state.issued))
    }
    pub(crate) fn set_coalescing(&self, enabled: bool) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allow_coalescing = enabled;
        self.shared.wake.notify_all();
    }
    pub(crate) fn set_interval(&self, interval: Duration) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interval = interval;
        self.shared.wake.notify_all();
    }
    pub(crate) fn axis(&self) -> ScrollAxis {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .axis
    }
    pub(crate) fn probe(&self) -> PipelineProbe {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.raw.retain(|raw| raw.strong_count() != 0);
        let live_raw: Vec<_> = state.raw.iter().filter_map(Weak::upgrade).collect();
        let raw_bytes = live_raw.iter().map(|raw| raw.bgra.len() as u64).sum();
        let probe = PipelineProbe {
            epoch: state.epoch,
            last_issued_sequence: state.issued,
            processed_sequence: state.processed,
            queue_depth: state.queue.len(),
            peak_queue_depth: state.peak_queue,
            capture_in_flight: state.in_flight,
            producer_alive: state.alive,
            producer_quiesced: state.quiesced,
            consumer_in_flight: false,
            consumer_alive: false,
            last_frame_sequence: 0,
            reserved_raw_bytes: self.shared.reserved.load(Ordering::Acquire),
            peak_reserved_raw_bytes: state.peak_reserved,
            raw_bytes,
            raw_budget_bytes: state.context.budget,
            max_raw_slots: state.context.slots,
            capture_storage_pixels: state.storage.0,
            capture_output_pixels: state.storage.1,
            capture_screen_count: state.storage.2,
            drain_fence: state.drain,
            wait_reason: state.wait_reason,
            error: state.error.clone(),
            coalesced_count: state.coalesced,
            allow_coalescing: state.allow_coalescing,
            reads: state.reads.iter().cloned().collect(),
            last_post_fence: None,
            auto_ignored_pre_post_frames: 0,
            auto_events: Vec::new(),
        };
        drop(state);
        drop(live_raw);
        probe
    }
    pub(crate) fn stop(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stop = true;
        state.running = false;
        state.single = false;
        state.drain = None;
        let retired = std::mem::take(&mut state.queue);
        state.revision = state.revision.wrapping_add(1);
        self.shared.wake.notify_all();
        drop(state);
        drop(retired);
    }
    pub(crate) fn try_join(&self) -> bool {
        let mut slot = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|w| !w.is_finished()) {
            return false;
        }
        if let Some(worker) = slot.take() {
            let _ = worker.join();
        }
        true
    }
    pub(crate) fn join(&self) {
        if self.try_join() {
            return;
        }
        let worker = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }
}
impl Drop for CapturePipeline {
    fn drop(&mut self) {
        // 最后一个控制 handle 消失也会唤醒生产者；UI 不在 Drop 中等待 DXGI。
        if Arc::strong_count(&self.worker) == 1 {
            self.stop();
        }
    }
}

fn producer(mut source: Box<dyn CaptureSource>, shared: &Arc<Shared>) {
    let mut due = Instant::now();
    let mut source_context: Option<Arc<CaptureConfig>> = None;
    loop {
        let (stamp, context) = loop {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.stop {
                return;
            }
            if !state.running && !state.single {
                if !state.quiesced {
                    drop(state);
                    source.release();
                    source_context = None;
                    state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.storage = (0, 0, 0);
                    if !state.running && !state.single {
                        state.quiesced = true;
                    }
                    shared.wake.notify_all();
                }
                state.wait_reason = Some("paused");
                shared.wake.notify_all();
                drop(
                    shared
                        .wake
                        .wait_timeout(state, Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner()),
                );
                due = Instant::now();
                continue;
            }
            let reserved = shared.reserved.load(Ordering::Acquire);
            if reserved + state.context.frame_bytes > state.context.budget {
                state.wait_reason = Some("raw-budget");
                shared.wake.notify_all();
                drop(
                    shared
                        .wake
                        .wait_timeout(state, Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner()),
                );
                continue;
            }
            if !state.single && Instant::now() < due {
                state.wait_reason = Some("cadence");
                shared.wake.notify_all();
                let wait = due
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50));
                drop(
                    shared
                        .wake
                        .wait_timeout(state, wait)
                        .unwrap_or_else(|e| e.into_inner()),
                );
                continue;
            }
            let Some(sequence) = state.issued.checked_add(1) else {
                state.error = Some("采集帧序号溢出".into());
                state.running = false;
                state.single = false;
                continue;
            };
            state.issued = sequence;
            state.single = false;
            state.in_flight = true;
            state.quiesced = false;
            state.wait_reason = None;
            let reserved = shared
                .reserved
                .fetch_add(state.context.frame_bytes, Ordering::AcqRel)
                + state.context.frame_bytes;
            state.peak_reserved = state.peak_reserved.max(reserved);
            let now = Instant::now();
            break (
                FrameStamp {
                    epoch: state.epoch,
                    sequence,
                    region: state.context.region,
                    axis: state.axis,
                    read_start: now,
                    read_end: now,
                },
                state.context.clone(),
            );
        };
        let permit = RawPermit {
            shared: Arc::downgrade(shared),
            bytes: context.frame_bytes,
        };
        let gate_error = Mutex::new(None);
        let cancelled = || {
            let stale = {
                let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                state.stop || state.epoch != stamp.epoch || !state.context.shares_source(&context)
            };
            if stale {
                return true;
            }
            if let Some(gate) = &context.gate {
                if let Err(error) = gate() {
                    *gate_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
                    return true;
                }
            }
            false
        };
        if source_context
            .as_ref()
            .is_none_or(|old| !old.shares_source(&context))
        {
            source.release();
            source.prepare(context.reach);
        }
        source_context = Some(context.clone());
        let result = if cancelled() {
            Err("采集已取消".into())
        } else {
            source.read(context.region, &cancelled)
        };
        let stamp = FrameStamp {
            read_end: Instant::now(),
            ..stamp
        };
        let storage = source.storage_probe();
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        due = stamp.read_start + state.interval;
        state.storage = storage;
        if state.stop || state.epoch != stamp.epoch || !state.context.shares_source(&context) {
            state.in_flight = false;
            drop(state);
            drop(permit);
            continue;
        }
        // 读回期间选区实时移动过：这帧仍是连续画面，归入当前选区参与拼接。
        let stamp = FrameStamp {
            region: state.context.region,
            ..stamp
        };
        let image = match result {
            Ok(image) => image,
            Err(error) => {
                state.error = Some(
                    gate_error
                        .into_inner()
                        .unwrap_or_else(|e| e.into_inner())
                        .unwrap_or_else(|| error.to_string()),
                );
                state.running = false;
                state.single = false;
                state.in_flight = false;
                drop(state);
                drop(permit);
                shared.wake.notify_all();
                continue;
            }
        };
        if image.width != context.region.width
            || image.height != context.region.height
            || image.bgra.len() as u64 != context.frame_bytes
        {
            state.error = Some("采集帧尺寸与选区不一致".into());
            state.running = false;
            state.single = false;
            state.in_flight = false;
            drop(state);
            drop(permit);
            continue;
        }
        state.raw.retain(|raw| raw.strong_count() != 0);
        if !state
            .raw
            .iter()
            .any(|raw| raw.as_ptr() == Arc::as_ptr(&image))
        {
            state.raw.push(Arc::downgrade(&image));
        }
        if state.reads.len() == TRACE_LIMIT {
            state.reads.pop_front();
        }
        state.reads.push_back(ReadProbe {
            epoch: stamp.epoch,
            sequence: stamp.sequence,
            read_start_ms: stamp
                .read_start
                .duration_since(shared.started)
                .as_secs_f64()
                * 1000.,
            read_end_ms: stamp.read_end.duration_since(shared.started).as_secs_f64() * 1000.,
            duration_ms: stamp
                .read_end
                .duration_since(stamp.read_start)
                .as_secs_f64()
                * 1000.,
        });
        let mut pending = Some(CapturedFrame {
            image,
            stamp,
            _permit: permit,
        });
        let mut retired = None;
        while state.queue.len() == 2 && !state.stop && state.epoch == stamp.epoch {
            if state.allow_coalescing
                && state.drain.is_none()
                && stamp.axis == ScrollAxis::Vertical
                && state.queue.iter().all(|f| {
                    f.stamp.epoch == stamp.epoch
                        && f.stamp.axis == stamp.axis
                        && f.stamp.region == stamp.region
                })
            {
                let first = state.queue[0].image.clone();
                let middle = state.queue[1].image.clone();
                let revision = state.revision;
                drop(state);
                let safe = crate::long_frame_coalesce::exact_coalesce(
                    &first,
                    &middle,
                    &pending.as_ref().expect("待移交帧").image,
                );
                // 证明只保留布尔结果；可能是最后一个像素所有者的 Arc 在短门外释放。
                drop(first);
                drop(middle);
                state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if safe
                    && !state.stop
                    && state.epoch == stamp.epoch
                    && state.revision == revision
                    && state.allow_coalescing
                    && state.drain.is_none()
                    && state.queue.len() == 2
                {
                    retired = state.queue.pop_back();
                    state.queue.push_back(pending.take().expect("待移交帧"));
                    state.coalesced = state.coalesced.saturating_add(1);
                    state.revision = state.revision.wrapping_add(1);
                    break;
                }
            }
            if state.queue.len() != 2 || state.stop || state.epoch != stamp.epoch {
                break;
            }
            state.wait_reason = Some("queue-full");
            shared.wake.notify_all();
            state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.in_flight = false;
        if !state.stop && state.epoch == stamp.epoch {
            if let Some(frame) = pending.take() {
                state.queue.push_back(frame);
                state.revision = state.revision.wrapping_add(1);
            }
            state.peak_queue = state.peak_queue.max(state.queue.len());
        }
        shared.wake.notify_all();
        drop(state);
        drop(retired);
        drop(pending);
    }
}

#[cfg(test)]
#[path = "long_capture_pipeline_tests.rs"]
mod tests;
