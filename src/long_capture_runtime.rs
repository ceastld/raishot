//! 长截图会话：独立抓屏生产者、有界 FIFO 与匹配消费者，界面只读 Arc 条带。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    long_auto::{
        AutoPhase, AutoPolicy, AutoScrollOptions, AutoSnapshot, Decision, AUTO_WHEEL_DELTA,
    },
    long_auto_native::{ScrollTargetProbe, VerifiedScrollTarget},
    long_capture_pipeline::{
        monitor_reach, AutoEventProbe, CaptureConfig, CapturePipeline, CapturedFrame, DrainFence,
        FrameStamp, Mode, PipelineProbe,
    },
    stitch::{ScrollAxis, StitchMatchQuality, StitchOptions, StitchSnapshot, Stitcher, UpdateKind},
    Result,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_millis(50);
// 预览按缩略线增量渲染，单次只处理新增内容；按显示帧率发布，积压时略放宽。
const PREVIEW_INTERVAL: Duration = Duration::from_millis(15);
const BUSY_PREVIEW_INTERVAL: Duration = Duration::from_millis(40);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
static PANICKED_WORKERS: AtomicUsize = AtomicUsize::new(0);
pub(crate) type FrameGate = Arc<dyn Fn() -> Result<()> + Send + Sync>;
pub(crate) type MatchHook = Arc<dyn Fn(&FrameStamp) -> Result<()> + Send + Sync>;
pub(crate) type FrameGateFactory = Arc<dyn Fn(ScreenRect) -> Result<FrameGate> + Send + Sync>;
pub(crate) type RegionGuardFactory = FrameGateFactory;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionChange {
    Reset,
    Continue,
}
pub(crate) fn active_workers() -> usize {
    ACTIVE_WORKERS.load(Ordering::Acquire)
}
pub(crate) fn panicked_workers() -> usize {
    PANICKED_WORKERS.load(Ordering::Acquire)
        + crate::long_capture_pipeline::panicked_capture_threads()
}

/// 仅 GPUI 短生命周期工作进程使用。start 预先建立无 GPU 的收尾线程；
/// Drop 不临时 spawn、不等待 DXGI，也不依赖界面继续泵消息。
fn cleanup_sender() -> Result<Sender<JoinHandle<()>>> {
    static REAPER: OnceLock<Mutex<Option<Sender<JoinHandle<()>>>>> = OnceLock::new();
    let mut sender = REAPER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(sender) = sender.as_ref() {
        return Ok(sender.clone());
    }
    let (next, receiver) = mpsc::channel::<JoinHandle<()>>();
    thread::Builder::new()
        .name("rustshot-long-cleanup".into())
        .spawn(move || {
            while let Ok(worker) = receiver.recv() {
                let _ = worker.join();
            }
        })?;
    *sender = Some(next.clone());
    Ok(next)
}

struct WorkerLease(Arc<Shared>);
impl WorkerLease {
    fn new(shared: Arc<Shared>) -> Self {
        ACTIVE_WORKERS.fetch_add(1, Ordering::AcqRel);
        Self(shared)
    }
}
impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.0.capture_in_flight.store(false, Ordering::Release);
        {
            let mut latest = self.0.latest.lock().unwrap_or_else(|e| e.into_inner());
            latest.meta.busy = false;
            latest.meta.worker_alive = false;
            latest.meta.worker_exited = true;
            latest.meta.capture_storage_pixels = 0;
            latest.meta.capture_output_pixels = 0;
            latest.meta.capture_screen_count = 0;
        }
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
        // Worker::run 内的采集器/拼接器已析构，不能把请求取消当作退出。
        self.0.exited.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LongOptions {
    pub stitch: StitchOptions,
    pub poll_interval: Duration,
}
impl Default for LongOptions {
    fn default() -> Self {
        Self {
            stitch: StitchOptions {
                detect_fixed_borders: true,
                ..StitchOptions::default()
            },
            // 约 60 Hz 捕获；快速滚动时保留更多有重叠的相邻帧。
            poll_interval: Duration::from_millis(16),
        }
    }
}
impl LongOptions {
    pub(crate) fn validate(&self, region: ScreenRect) -> Result<()> {
        let (cross, along) = match self.stitch.axis {
            ScrollAxis::Vertical => (region.width, region.height),
            ScrollAxis::Horizontal => (region.height, region.width),
        };
        if cross < 32 || along < 64 || region.width as u64 * region.height as u64 > 32_000_000 {
            return Err(
                tr!("长截图内容轴至少 64 像素、另一边至少 32 像素，且不超过 3200 万像素", "A long capture needs at least 64 pixels along the content axis and 32 on the other side, up to 32 megapixels").into(),
            );
        }
        if region.width > 16_384 || region.height > 16_384 {
            return Err(tr!("采集选区每边不能超过 16384 像素；拼接结果可超过此范围", "Each side of the capture area must be at most 16384 pixels; the stitched result can be larger").into());
        }
        if !(Duration::from_millis(16)..=Duration::from_millis(500)).contains(&self.poll_interval) {
            return Err(tr!(
                "采集间隔须为 16–500 毫秒",
                "The capture interval must be 16–500 ms"
            )
            .into());
        }
        let fixed =
            self.stitch.fixed_start.unwrap_or(0) as u64 + self.stitch.fixed_end.unwrap_or(0) as u64;
        if fixed + 64 > along as u64 {
            return Err(tr!(
                "固定边带之后至少要保留 64 像素可滚动内容",
                "At least 64 pixels of scrollable content must remain after the fixed bands"
            )
            .into());
        }
        if region.left as i64 + region.width as i64 > i32::MAX as i64
            || region.top as i64 + region.height as i64 > i32::MAX as i64
        {
            return Err("长截图物理范围溢出".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongPhase {
    Starting,
    Running,
    Pausing,
    Paused,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct LongSnapshot {
    pub phase: LongPhase,
    pub busy: bool,
    pub capture_in_flight: bool,
    pub poll_count: u64,
    pub accepted_frames: u64,
    pub discarded_frames: u64,
    pub last_result: UpdateKind,
    pub last_delta: i32,
    pub last_quality: Option<StitchMatchQuality>,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub requested_generation: u64,
    pub worker_alive: bool,
    pub worker_exited: bool,
    pub capture_storage_pixels: u64,
    pub capture_output_pixels: u64,
    pub capture_screen_count: usize,
    pub axis: ScrollAxis,
    pub region: ScreenRect,
    pub pending_region: Option<ScreenRect>,
    pub viewport: Rect,
    pub status: String,
    pub preview_revision: u64,
    pub can_undo: bool,
    pub result_available: bool,
    pub auto: AutoSnapshot,
}
impl LongSnapshot {
    pub(crate) fn starting(region: ScreenRect, options: &LongOptions) -> Self {
        Self {
            phase: LongPhase::Starting,
            busy: true,
            capture_in_flight: false,
            poll_count: 0,
            accepted_frames: 0,
            discarded_frames: 0,
            last_result: UpdateKind::Unchanged,
            last_delta: 0,
            last_quality: None,
            width: 0,
            height: 0,
            generation: 0,
            requested_generation: 0,
            worker_alive: true,
            worker_exited: false,
            capture_storage_pixels: 0,
            capture_output_pixels: 0,
            capture_screen_count: 0,
            axis: options.stitch.axis,
            region,
            pending_region: None,
            viewport: Rect::full(0, 0),
            status: tr!("正在准备采集…", "Preparing to capture…").into(),
            preview_revision: 0,
            can_undo: false,
            result_available: false,
            auto: AutoSnapshot::default(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct LongPreview {
    pub image: Arc<Image>,
    pub revision: u64,
    pub generation: u64,
    /// 对应这张预览的来源尺寸/视口，不拿旧预览映射新长图的尺寸。
    pub width: u32,
    pub height: u32,
    pub viewport: Rect,
    pub axis: ScrollAxis,
    /// 预览对应长图中的这个窗口；width/height 仍是完整结果尺寸。
    pub source_rect: Rect,
}
const PREVIEW_WIDTH: u32 = 200;
const PREVIEW_HEIGHT: u32 = 700;
fn preview_density(scale: f32) -> u32 {
    if !scale.is_finite() {
        return 100;
    }
    (scale.clamp(1., 2.5) * 100.).round() as u32
}
fn preview_limits(axis: ScrollAxis, density: u32) -> (u32, u32) {
    let width = PREVIEW_WIDTH * density / 100;
    let height = PREVIEW_HEIGHT * density / 100;
    match axis {
        ScrollAxis::Vertical => (width, height),
        ScrollAxis::Horizontal => (height, width),
    }
}

fn preview_source(snapshot: &StitchSnapshot) -> Rect {
    let full = Rect::full(snapshot.width, snapshot.height);
    match snapshot.axis {
        ScrollAxis::Vertical => {
            let visible = (snapshot.width as u64 * PREVIEW_HEIGHT as u64 / PREVIEW_WIDTH as u64)
                .max(snapshot.viewport.height as u64)
                .min(snapshot.height as u64) as u32;
            Rect {
                y: snapshot
                    .viewport
                    .y
                    .saturating_sub((visible - snapshot.viewport.height) / 2)
                    .min(snapshot.height - visible),
                height: visible,
                ..full
            }
        }
        ScrollAxis::Horizontal => {
            let visible = (snapshot.height as u64 * PREVIEW_HEIGHT as u64 / PREVIEW_WIDTH as u64)
                .max(snapshot.viewport.width as u64)
                .min(snapshot.width as u64) as u32;
            Rect {
                x: snapshot
                    .viewport
                    .x
                    .saturating_sub((visible - snapshot.viewport.width) / 2)
                    .min(snapshot.width - visible),
                width: visible,
                ..full
            }
        }
    }
}
/// 采集中的实时预览：与 `render_preview` 同一投影窗口与缩放上限，但按固定比例网格增量渲染。
fn render_preview_incremental(
    strip: &mut crate::long_preview_strip::PreviewStrip,
    snapshot: &StitchSnapshot,
    density: u32,
) -> Result<(Image, Rect)> {
    let source = preview_source(snapshot);
    let (max_cross, max_long) = (
        PREVIEW_WIDTH * density / 100,
        PREVIEW_HEIGHT * density / 100,
    );
    let (cross, viewport_long) = match snapshot.axis {
        ScrollAxis::Vertical => (snapshot.width, snapshot.viewport.height),
        ScrollAxis::Horizontal => (snapshot.height, snapshot.viewport.width),
    };
    // 与 preview_source 的窗口上限一致，使缩放比例在本代内不随长图增长而变化。
    let cap = (cross as u64 * PREVIEW_HEIGHT as u64 / PREVIEW_WIDTH as u64)
        .max(viewport_long as u64)
        .min(u32::MAX as u64) as u32;
    strip.render(snapshot, max_cross, max_long, cap, source)
}
fn render_preview(snapshot: &StitchSnapshot, density: u32) -> Result<(Image, Rect)> {
    let source = preview_source(snapshot);
    let (max_width, max_height) = preview_limits(snapshot.axis, density);
    let scale = (max_width as f64 / source.width.max(1) as f64)
        .min(max_height as f64 / source.height.max(1) as f64)
        .min(1.);
    let width = (source.width as f64 * scale).round().max(1.) as u32;
    let height = (source.height as f64 * scale).round().max(1.) as u32;
    Ok((
        crate::long_document::render_preview_region(snapshot, source, width, height)?,
        source,
    ))
}
struct Latest {
    meta: LongSnapshot,
    snapshot: Option<Arc<StitchSnapshot>>,
    preview: Option<LongPreview>,
    context: Option<CommittedContext>,
    pending_region: Option<RegionRequest>,
}
#[derive(Clone)]
struct CommittedContext {
    capture: Arc<CaptureConfig>,
    options: LongOptions,
    target: std::result::Result<VerifiedScrollTarget, String>,
}
/// 暂停浮层只持有原采集上下文的身份，不保留 GPU 资源，也不按窗口标题重绑。
#[derive(Clone)]
pub(crate) struct AutoResumeTarget(Arc<CaptureConfig>);
impl PartialEq for AutoResumeTarget {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
#[derive(Clone)]
struct RegionRequest {
    expected_epoch: u64,
    kind: RegionChange,
    capture: Arc<CaptureConfig>,
    target: std::result::Result<VerifiedScrollTarget, String>,
    previous: CommittedContext,
}
struct Shared {
    latest: Mutex<Latest>,
    requested_generation: AtomicU64,
    capture_in_flight: AtomicBool,
    /// 只保留最新 DPI 需求，不进入采集命令队列，也不改变暂停屏障代次。
    preview_density: AtomicU32,
    stop: AtomicBool,
    exited: Arc<AtomicBool>,
    pipeline: Option<CapturePipeline>,
    match_hook: Option<MatchHook>,
    consumer_in_flight: AtomicBool,
    last_frame_sequence: AtomicU64,
    allow_coalescing: AtomicBool,
    auto_probe: Mutex<AutoProbe>,
    /// 拖动选区时的最新目标位置；消费者在帧间原子切换，旧请求被新请求覆盖。
    relocation: Mutex<Option<Arc<CaptureConfig>>>,
}
#[derive(Default)]
struct AutoProbe {
    next_sequence: u64,
    last_post_fence: Option<u64>,
    ignored_pre_post_frames: u64,
    events: VecDeque<AutoEventProbe>,
}
impl AutoProbe {
    fn record(
        &mut self,
        event: &'static str,
        epoch: u64,
        frame_sequence: u64,
        fence_sequence: Option<u64>,
    ) {
        // 序号耗尽时停止记录，不能回绕成重复事件或影响自动滚动。
        let Some(sequence) = self.next_sequence.checked_add(1) else {
            return;
        };
        self.next_sequence = sequence;
        if self.events.len() == 64 {
            self.events.pop_front();
        }
        self.events.push_back(AutoEventProbe {
            sequence,
            event,
            epoch,
            frame_sequence,
            fence_sequence,
        });
    }
}
enum Operation {
    Pause(bool),
    Capture,
    Restart(LongOptions),
    Undo,
    Auto(Option<(VerifiedScrollTarget, AutoScrollOptions)>),
    Drain,
    Region(RegionRequest),
    CancelRegion(CommittedContext),
}
struct Command {
    generation: u64,
    operation: Operation,
    mode: Mode,
}

pub(crate) struct LongSession {
    shared: Arc<Shared>,
    commands: SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
    cleanup: Sender<JoinHandle<()>>,
    gate_factory: Option<RegionGuardFactory>,
    fixed_region_gate: bool,
}
impl LongSession {
    pub(crate) fn start(region: ScreenRect, options: LongOptions) -> Result<Self> {
        Self::start_inner(region, options, None, None, None)
    }
    /// 自有窗口专项在每个可取消采集检查点重新确认内容归属，不走桌面冻结入口。
    pub(crate) fn start_guarded(
        region: ScreenRect,
        options: LongOptions,
        gate: FrameGate,
    ) -> Result<Self> {
        gate()?;
        Self::start_inner(region, options, Some(gate), None, None)
    }
    pub(crate) fn start_guarded_with_match_hook(
        region: ScreenRect,
        options: LongOptions,
        gate: FrameGate,
        hook: Option<MatchHook>,
    ) -> Result<Self> {
        gate()?;
        Self::start_inner(region, options, Some(gate), hook, None)
    }
    pub(crate) fn start_region_guarded(
        region: ScreenRect,
        options: LongOptions,
        factory: RegionGuardFactory,
        hook: Option<MatchHook>,
    ) -> Result<Self> {
        let gate = factory(region)?;
        gate()?;
        Self::start_inner(region, options, Some(gate), hook, Some(factory))
    }
    fn start_inner(
        region: ScreenRect,
        options: LongOptions,
        gate: Option<FrameGate>,
        match_hook: Option<MatchHook>,
        gate_factory: Option<RegionGuardFactory>,
    ) -> Result<Self> {
        options.validate(region)?;
        // 面板/冻结截图退场后绑定一次原始内容窗口；首帧前完成，不随以后焦点重选。
        let target = VerifiedScrollTarget::discover(region).map_err(|error| error.to_string());
        let cleanup = cleanup_sender()?;
        let pipeline = CapturePipeline::start(region, options.poll_interval, gate.clone())?;
        pipeline.configure(0, options.stitch.axis, Mode::Running);
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: Some(CommittedContext {
                    capture: pipeline.context(),
                    options: options.clone(),
                    target: target.clone(),
                }),
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(0),
            capture_in_flight: AtomicBool::new(false),
            preview_density: AtomicU32::new(100),
            stop: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
            pipeline: Some(pipeline),
            match_hook,
            consumer_in_flight: AtomicBool::new(false),
            last_frame_sequence: AtomicU64::new(0),
            allow_coalescing: AtomicBool::new(true),
            auto_probe: Mutex::new(AutoProbe::default()),
            relocation: Mutex::new(None),
        });
        let (commands, receiver) = mpsc::sync_channel(8);
        let lease = WorkerLease::new(shared.clone());
        let thread_shared = shared.clone();
        let target_label = target
            .as_ref()
            .map(|target| target.label().to_string())
            .unwrap_or_default();
        let fixed_region_gate = gate_factory.is_none() && gate.is_some();
        let worker = thread::Builder::new()
            .name("rustshot-long-consumer".into())
            .spawn(move || {
                let _lease = lease;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut worker = Worker::new(region, options);
                    worker.frame_gate = gate;
                    worker.pipeline = thread_shared.pipeline.clone();
                    worker.meta.auto.target = target_label;
                    worker.run(receiver, &thread_shared);
                }));
                if let Some(pipeline) = &thread_shared.pipeline {
                    pipeline.stop();
                    pipeline.join();
                }
                let mut latest = thread_shared
                    .latest
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if result.is_err() {
                    PANICKED_WORKERS.fetch_add(1, Ordering::AcqRel);
                    latest.meta.phase = LongPhase::Failed;
                    latest.meta.status = tr!(
                        "采集线程意外退出；最后成功结果已保留",
                        "The capture thread exited unexpectedly; the last good result is kept"
                    )
                    .into();
                } else {
                    latest.meta.phase = LongPhase::Stopped;
                    latest.meta.status = tr!(
                        "已停止采集；最后成功结果仍可编辑或保存",
                        "Capturing stopped; the last good result can still be edited or saved"
                    )
                    .into();
                }
                latest.meta.busy = false;
                latest.meta.auto.active = false;
            })?;
        Ok(Self {
            shared,
            commands,
            worker: Some(worker),
            cleanup,
            gate_factory,
            fixed_region_gate,
        })
    }

    pub(crate) fn snapshot(&self) -> LongSnapshot {
        let mut result = self
            .shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .clone();
        result.requested_generation = self.shared.requested_generation.load(Ordering::Acquire);
        result.capture_in_flight = self.shared.capture_in_flight.load(Ordering::Acquire);
        result.worker_exited = self.shared.exited.load(Ordering::Acquire);
        if let Some(pipeline) = &self.shared.pipeline {
            let p = pipeline.probe();
            result.capture_in_flight = p.capture_in_flight;
            result.capture_storage_pixels = p.capture_storage_pixels;
            result.capture_output_pixels = p.capture_output_pixels;
            result.capture_screen_count = p.capture_screen_count;
            result.busy |= p.drain_fence.is_some()
                || self.shared.consumer_in_flight.load(Ordering::Acquire)
                || matches!(result.phase, LongPhase::Paused | LongPhase::Pausing)
                    && !p.producer_quiesced;
        }
        result.busy |= result.generation != result.requested_generation || result.capture_in_flight;
        result
    }
    pub(crate) fn pipeline_probe(&self) -> PipelineProbe {
        let mut probe = self
            .shared
            .pipeline
            .as_ref()
            .expect("生产会话抓屏器")
            .probe();
        probe.consumer_in_flight = self.shared.consumer_in_flight.load(Ordering::Acquire);
        probe.consumer_alive = !self.shared.exited.load(Ordering::Acquire);
        probe.last_frame_sequence = self.shared.last_frame_sequence.load(Ordering::Acquire);
        let auto = self
            .shared
            .auto_probe
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        probe.last_post_fence = auto.last_post_fence;
        probe.auto_ignored_pre_post_frames = auto.ignored_pre_post_frames;
        probe.auto_events = auto.events.iter().cloned().collect();
        probe
    }
    pub(crate) fn set_smoke_coalescing(&self, enabled: bool) {
        self.shared
            .allow_coalescing
            .store(enabled, Ordering::Release);
        if let Some(p) = &self.shared.pipeline {
            p.set_coalescing(enabled && !self.snapshot().auto.active);
        }
    }
    /// 必须取得后台暂停 ACK；UI 本地 paused 布尔值不能代替排空屏障。
    pub(crate) fn stitch_snapshot(&self) -> Option<StitchSnapshot> {
        if let Some(pipeline) = &self.shared.pipeline {
            let p = pipeline.probe();
            if p.capture_in_flight
                || !p.producer_quiesced
                || p.drain_fence.is_some()
                || self.shared.consumer_in_flight.load(Ordering::Acquire)
            {
                return None;
            }
        }
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if latest.meta.busy
            || self.shared.capture_in_flight.load(Ordering::Acquire)
            || latest.meta.generation != self.shared.requested_generation.load(Ordering::Acquire)
            || !matches!(
                latest.meta.phase,
                LongPhase::Paused | LongPhase::Failed | LongPhase::Stopped
            )
        {
            return None;
        }
        latest
            .snapshot
            .as_ref()
            .map(|snapshot| (**snapshot).clone())
    }
    /// 自有原生 QA 读取当前代已提交的不可变条带；用户导出仍必须走暂停/排空屏障。
    pub(crate) fn smoke_committed_snapshot(&self) -> Option<StitchSnapshot> {
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if latest.meta.generation != self.shared.requested_generation.load(Ordering::Acquire) {
            return None;
        }
        latest
            .snapshot
            .as_ref()
            .map(|snapshot| (**snapshot).clone())
    }
    pub(crate) fn preview_frame(&self) -> Option<LongPreview> {
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.shared.requested_generation.load(Ordering::Acquire);
        if latest.meta.generation != generation {
            return None;
        }
        latest
            .preview
            .as_ref()
            .filter(|preview| preview.generation == generation)
            .cloned()
    }
    pub(crate) fn set_preview_scale(&self, scale: f32) {
        let density = preview_density(scale);
        if self.shared.preview_density.load(Ordering::Relaxed) != density {
            self.shared
                .preview_density
                .store(density, Ordering::Release);
        }
    }
    pub(crate) fn worker_probe(&self) -> Arc<AtomicBool> {
        self.shared.exited.clone()
    }

    fn command(&mut self, operation: Operation) -> Result<()> {
        // publish/提交暂存拼接器使用同一短锁，投递及 requested 更新不可被拆开观察。
        let mut latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        let mut retired_capture = None;
        if self.shared.stop.load(Ordering::Acquire) || self.shared.exited.load(Ordering::Acquire) {
            return Err(tr!("采集线程已经停止", "The capture thread has stopped").into());
        }
        if latest.pending_region.is_some() && !matches!(&operation, Operation::CancelRegion(_)) {
            return Err(tr!(
                "正在确认新选区，请先取消或等待完成",
                "Confirming the new area; cancel or wait for it to finish"
            )
            .into());
        }
        let region_request = if let Operation::Region(request) = &operation {
            Some(request.clone())
        } else {
            None
        };
        let restore = if let Operation::CancelRegion(context) = &operation {
            Some(context.clone())
        } else {
            None
        };
        if let Some(context) = &restore {
            if latest
                .pending_region
                .as_ref()
                .is_none_or(|request| !Arc::ptr_eq(&request.previous.capture, &context.capture))
            {
                return Err("新选区已确认或取消，本次取消请求已失效".into());
            }
        }
        if let Some(request) = &region_request {
            if latest.meta.generation != request.expected_epoch
                || latest.meta.generation
                    != self.shared.requested_generation.load(Ordering::Acquire)
                || latest.meta.phase != LongPhase::Paused
                || latest.meta.busy
                || self.shared.consumer_in_flight.load(Ordering::Acquire)
            {
                return Err(tr!(
                    "采集状态已变化，请先等待完全暂停",
                    "The capture state changed; wait until it's fully paused"
                )
                .into());
            }
            let p = self
                .shared
                .pipeline
                .as_ref()
                .ok_or("采集器尚未初始化")?
                .probe();
            if !p.producer_quiesced
                || p.capture_in_flight
                || p.queue_depth != 0
                || p.reserved_raw_bytes != 0
                || p.drain_fence.is_some()
            {
                return Err(tr!(
                    "请先等待采集完全暂停再调整选区",
                    "Wait until capturing is fully paused before adjusting the area"
                )
                .into());
            }
        }
        let generation = self
            .shared
            .requested_generation
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or("采集版本溢出")?;
        let phase = match &operation {
            Operation::Pause(true) | Operation::Undo | Operation::Drain => Some(LongPhase::Pausing),
            Operation::Restart(_) | Operation::Region(_) => Some(LongPhase::Starting),
            Operation::CancelRegion(_) => Some(LongPhase::Pausing),
            _ => None,
        };
        let mode = match &operation {
            Operation::Pause(true) | Operation::Undo => Mode::Paused,
            Operation::Capture | Operation::Region(_) => Mode::Single,
            Operation::Pause(false) | Operation::Restart(_) | Operation::Auto(Some(_)) => {
                Mode::Running
            }
            Operation::Auto(None) => {
                if matches!(latest.meta.phase, LongPhase::Running | LongPhase::Starting) {
                    Mode::Running
                } else {
                    Mode::Paused
                }
            }
            Operation::Drain | Operation::CancelRegion(_) => Mode::Paused,
        };
        // 失败重启会保留旧输出轴，但下一次重试仍使用已请求的采集轴。
        let axis = match &operation {
            Operation::Restart(o) => o.stitch.axis,
            Operation::Region(r) => r.previous.options.stitch.axis,
            Operation::CancelRegion(c) => c.options.stitch.axis,
            _ => self
                .shared
                .pipeline
                .as_ref()
                .map_or(latest.meta.axis, CapturePipeline::axis),
        };
        let is_drain = matches!(&operation, Operation::Drain);
        if is_drain
            && latest.meta.generation != self.shared.requested_generation.load(Ordering::Acquire)
        {
            return Err(tr!(
                "上一个控制操作尚未完成，请稍候再编辑或输出",
                "The previous action hasn't finished; wait before editing or exporting"
            )
            .into());
        }
        if is_drain
            && self
                .shared
                .pipeline
                .as_ref()
                .is_some_and(|p| p.probe().drain_fence.is_some())
        {
            return Err(tr!(
                "正在等待已采集帧，请稍候",
                "Waiting for captured frames, please wait"
            )
            .into());
        }
        let entering_auto = matches!(&operation, Operation::Auto(Some(_)));
        self.commands
            .try_send(Command {
                generation,
                operation,
                mode,
            })
            .map_err(|error| {
                match error {
                    TrySendError::Full(_) => {
                        tr!("操作正在排队，请稍候", "The action is queued, please wait")
                    }
                    TrySendError::Disconnected(_) => {
                        tr!("采集线程已经退出", "The capture thread has exited")
                    }
                }
                .to_string()
            })?;
        if let Some(pipeline) = &self.shared.pipeline {
            if entering_auto {
                pipeline.set_coalescing(false);
            }
            if let Some(request) = &region_request {
                pipeline.retarget_single(
                    request.expected_epoch,
                    generation,
                    request.capture.clone(),
                    axis,
                )?;
            } else if let Some(context) = &restore {
                retired_capture = pipeline.restore_context_if_current(
                    self.shared.requested_generation.load(Ordering::Acquire),
                    generation,
                    context.capture.clone(),
                    axis,
                );
                if retired_capture.is_none() {
                    return Err("选区请求已失效".into());
                }
            } else if is_drain {
                pipeline.begin_drain(generation);
            } else {
                pipeline.configure(generation, axis, mode);
            }
        }
        self.shared
            .requested_generation
            .store(generation, Ordering::Release);
        latest.meta.requested_generation = generation;
        latest.meta.busy = true;
        if let Some(request) = region_request {
            latest.meta.pending_region = Some(request.capture.region);
            latest.pending_region = Some(request);
        }
        if let Some(context) = restore {
            latest.pending_region = None;
            latest.meta.pending_region = None;
            latest.context = Some(context);
        }
        // UI 控制与原生 post 使用同一 gate；已请求暂停/重启不能继续发送下一笔。
        latest.meta.auto.active = false;
        if let Some(phase) = phase {
            latest.meta.phase = phase;
        }
        drop(latest);
        drop(retired_capture);
        Ok(())
    }
    pub(crate) fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.command(Operation::Pause(paused))
    }
    pub(crate) fn capture_once(&mut self) -> Result<()> {
        self.command(Operation::Capture)
    }
    pub(crate) fn change_region(&mut self, region: ScreenRect, kind: RegionChange) -> Result<()> {
        self.change_region_with_target(region, kind, VerifiedScrollTarget::discover)
    }
    fn change_region_with_target(
        &mut self,
        region: ScreenRect,
        kind: RegionChange,
        discover: impl FnOnce(ScreenRect) -> Result<VerifiedScrollTarget>,
    ) -> Result<()> {
        if self.fixed_region_gate {
            return Err("固定归属门的采集会话不能调整选区".into());
        }
        let (previous, expected_epoch) = {
            let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            if latest.meta.phase != LongPhase::Paused
                || latest.meta.busy
                || !latest.meta.result_available
                || latest.pending_region.is_some()
                || latest.meta.generation
                    != self.shared.requested_generation.load(Ordering::Acquire)
                || self.shared.consumer_in_flight.load(Ordering::Acquire)
            {
                return Err(tr!(
                    "请先成功采集并等待完全暂停再调整选区",
                    "Capture successfully and wait until fully paused before adjusting the area"
                )
                .into());
            }
            let p = self
                .shared
                .pipeline
                .as_ref()
                .ok_or("采集器尚未初始化")?
                .probe();
            if !p.producer_quiesced
                || p.capture_in_flight
                || p.reserved_raw_bytes != 0
                || p.queue_depth != 0
                || p.drain_fence.is_some()
            {
                return Err(tr!(
                    "请先等待采集完全暂停再调整选区",
                    "Wait until capturing is fully paused before adjusting the area"
                )
                .into());
            }
            (
                latest.context.clone().ok_or("采集上下文尚未确认")?,
                latest.meta.generation,
            )
        };
        previous.options.validate(region)?;
        if kind == RegionChange::Continue {
            let old = previous.capture.region;
            if region.width != old.width
                || region.height != old.height
                || match previous.options.stitch.axis {
                    ScrollAxis::Vertical => region.left != old.left,
                    ScrollAxis::Horizontal => region.top != old.top,
                }
            {
                return Err(tr!("继续长图只支持同尺寸沿滚动轴移动", "Continuing a long image only supports moving the same-size area along the scroll axis").into());
            }
        }
        if previous.capture.region == region {
            return Ok(());
        }
        let gate = self
            .gate_factory
            .as_ref()
            .map(|factory| factory(region))
            .transpose()?;
        if let Some(gate) = &gate {
            gate()?;
        }
        let capture = CaptureConfig::new_reaching(region, monitor_reach(region), gate)?;
        let target = discover(region).map_err(|error| error.to_string());
        self.command(Operation::Region(RegionRequest {
            expected_epoch,
            kind,
            capture,
            target,
            previous,
        }))
    }
    /// 采集中可拖动选区的范围；只有同尺寸、沿滚动轴、在此范围内的移动可实时生效。
    pub(crate) fn live_move_reach(&self) -> Option<ScreenRect> {
        if self.fixed_region_gate {
            return None;
        }
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        latest.context.as_ref().map(|context| context.capture.reach)
    }
    /// 采集不停，把选区移到新位置；选区移动对拼接等同滚动，长图与预览照常增长。
    pub(crate) fn move_region_live(&mut self, region: ScreenRect) -> Result<()> {
        if self.fixed_region_gate {
            return Err("固定归属门的采集会话不能调整选区".into());
        }
        let capture = {
            let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            if latest.meta.phase != LongPhase::Running || latest.pending_region.is_some() {
                return Err(tr!(
                    "采集未在进行，不能实时移动选区",
                    "Capturing isn't running; can't move the area live"
                )
                .into());
            }
            let context = latest.context.as_ref().ok_or("采集上下文尚未确认")?;
            let old = context.capture.region;
            if match context.options.stitch.axis {
                ScrollAxis::Vertical => region.left != old.left,
                ScrollAxis::Horizontal => region.top != old.top,
            } {
                return Err(tr!(
                    "采集中只能沿滚动方向移动选区",
                    "While capturing, the area only moves along the scroll direction"
                )
                .into());
            }
            context.capture.clone()
        };
        let gate = match &self.gate_factory {
            Some(factory) => Some(factory(region)?),
            None => capture.gate.clone(),
        };
        let relocated = capture.relocated(region, gate)?;
        *self
            .shared
            .relocation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(relocated);
        Ok(())
    }
    pub(crate) fn cancel_region_change(&mut self) -> Result<()> {
        let previous = self
            .shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_region
            .as_ref()
            .map(|request| request.previous.clone())
            .ok_or("没有待确认的新选区")?;
        self.command(Operation::CancelRegion(previous))
    }
    pub(crate) fn drain_for_output(&mut self) -> Result<()> {
        self.command(Operation::Drain)
    }
    pub(crate) fn restart(&mut self, options: LongOptions) -> Result<()> {
        options.validate(self.snapshot().region)?;
        self.command(Operation::Restart(options))
    }
    pub(crate) fn undo(&mut self) -> Result<()> {
        self.command(Operation::Undo)
    }
    pub(crate) fn set_auto_scroll(&mut self, options: Option<AutoScrollOptions>) -> Result<()> {
        let binding = match options {
            Some(options) => {
                let meta = self.snapshot();
                if meta.generation != meta.requested_generation
                    || !meta.result_available
                    || !matches!(meta.phase, LongPhase::Running | LongPhase::Paused)
                {
                    return Err(tr!("请先等待当前采集完成，或重新采集成功后开启自动滚动", "Wait for the current capture to finish, or capture again successfully before starting auto-scroll").into());
                }
                let target = self
                    .shared
                    .latest
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .context
                    .as_ref()
                    .ok_or("采集上下文尚未确认")?
                    .target
                    .clone()?;
                target.validate(meta.region, meta.axis)?;
                Some((target, options))
            }
            None => None,
        };
        self.command(Operation::Auto(binding))
    }
    /// 只读已提交上下文的原目标；不发现、重绑或校验当前 Win32 状态。
    pub(crate) fn auto_target_probe(&self) -> Result<ScrollTargetProbe> {
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        let context = latest.context.as_ref().ok_or("采集上下文尚未确认")?;
        let target = context.target.as_ref().map_err(Clone::clone)?;
        Ok(target.probe())
    }
    pub(crate) fn auto_resume_context(
        &self,
    ) -> Option<crate::long_auto_resume::ResumeContext<AutoResumeTarget>> {
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        let context = latest.context.as_ref()?;
        Some(crate::long_auto_resume::ResumeContext {
            region: latest.meta.region,
            axis: latest.meta.axis,
            generation: self.shared.requested_generation.load(Ordering::Acquire),
            target: AutoResumeTarget(context.capture.clone()),
        })
    }
    pub(crate) fn validate_auto_resume_target(&self, expected: &AutoResumeTarget) -> Result<()> {
        self.auto_resume_target(expected).map(|_| ())
    }
    pub(crate) fn auto_resume_target(
        &self,
        expected: &AutoResumeTarget,
    ) -> Result<VerifiedScrollTarget> {
        let latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        let context = latest.context.as_ref().ok_or("采集上下文尚未确认")?;
        if !Arc::ptr_eq(&context.capture, &expected.0) {
            return Err(tr!(
                "选区已变更，自动滚动恢复已取消",
                "The area changed; auto-scroll resume was canceled"
            )
            .into());
        }
        let target = context.target.as_ref().map_err(Clone::clone)?;
        target.validate(latest.meta.region, latest.meta.axis)?;
        Ok(target.clone())
    }
    pub(crate) fn stop(&self) {
        let mut latest = self.shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if self.shared.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(pipeline) = &self.shared.pipeline {
            pipeline.stop();
        }
        latest.pending_region = None;
        latest.meta.pending_region = None;
        // 停止是冻结最后成功结果的新版本；未执行的重启/撤销不能冒充成功。
        let generation = self
            .shared
            .requested_generation
            .load(Ordering::Acquire)
            .saturating_add(1);
        self.shared
            .requested_generation
            .store(generation, Ordering::Release);
        latest.meta.generation = generation;
        latest.meta.requested_generation = generation;
        latest.meta.auto.active = false;
        latest.meta.auto.phase = AutoPhase::Off;
        if let Some(snapshot) = latest.snapshot.clone() {
            latest.meta.width = snapshot.width;
            latest.meta.height = snapshot.height;
            latest.meta.axis = snapshot.axis;
            latest.meta.viewport = snapshot.viewport;
        }
        // 最后一次已提交像素可能尚未来得及生成预览；旧预览不伪装成停止版本。
        if !self.shared.exited.load(Ordering::Acquire) {
            latest.meta.phase = LongPhase::Stopping;
            latest.meta.busy = true;
            latest.meta.status = tr!(
                "正在停止采集并释放资源…",
                "Stopping capture and releasing resources…"
            )
            .into();
        }
    }
    pub(crate) fn try_join_stopped(&mut self) -> bool {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return false;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        true
    }
}
impl Drop for LongSession {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            // 无界控制通道只放线程句柄；send 不等待 DXGI，接收端只 join、不会 panic。
            if let Err(error) = self.cleanup.send(worker) {
                drop(error.0);
            }
        }
    }
}

struct ConsumerGuard<'a>(&'a Shared);
impl<'a> ConsumerGuard<'a> {
    fn new(shared: &'a Shared) -> Self {
        shared.consumer_in_flight.store(true, Ordering::Release);
        Self(shared)
    }
}
impl Drop for ConsumerGuard<'_> {
    fn drop(&mut self) {
        self.0.consumer_in_flight.store(false, Ordering::Release);
    }
}
struct FrameCompletion {
    pipeline: CapturePipeline,
    sequence: u64,
}
impl Drop for FrameCompletion {
    fn drop(&mut self) {
        self.pipeline.mark_processed(self.sequence);
    }
}

struct Worker {
    /// 只累计当前代已提交的可靠匹配位移，旧 read 可拼接；Auto 仍需新 read 确认位置。
    trusted_scroll_position: i64,
    frame_gate: Option<FrameGate>,
    options: LongOptions,
    meta: LongSnapshot,
    pipeline: Option<CapturePipeline>,
    drain: Option<DrainFence>,
    stitch: Option<Stitcher>,
    output: Option<Arc<StitchSnapshot>>,
    preview: Option<LongPreview>,
    preview_dirty: bool,
    last_preview: Option<Instant>,
    preview_density: u32,
    /// 固定比例缩略线缓存；新拼接内容只渲染新增线，使预览可按显示帧率逐行更新。
    preview_strip: crate::long_preview_strip::PreviewStrip,
    paused: bool,
    initialize: bool,
    capture_once: bool,
    automatic: Option<(VerifiedScrollTarget, AutoPolicy)>,
    auto_post_fence: Option<u64>,
    region_request: Option<RegionRequest>,
}
impl Worker {
    fn new(region: ScreenRect, options: LongOptions) -> Self {
        let meta = LongSnapshot::starting(region, &options);
        Self {
            trusted_scroll_position: 0,
            frame_gate: None,
            options,
            meta,
            pipeline: None,
            drain: None,
            stitch: None,
            output: None,
            preview: None,
            preview_dirty: false,
            last_preview: None,
            preview_density: 100,
            preview_strip: Default::default(),
            paused: false,
            initialize: true,
            capture_once: false,
            automatic: None,
            auto_post_fence: None,
            region_request: None,
        }
    }
    fn current(&self, shared: &Shared) -> bool {
        !shared.stop.load(Ordering::Acquire)
            && self.pipeline.as_ref().map_or_else(
                || self.meta.generation == shared.requested_generation.load(Ordering::Acquire),
                |p| {
                    p.accepts_epoch(
                        self.meta.generation,
                        shared.requested_generation.load(Ordering::Acquire),
                    )
                },
            )
    }
    fn publish(&mut self, shared: &Shared, force_preview: bool) {
        let storage = self.pipeline.as_ref().map_or((0, 0, 0), |p| {
            let p = p.probe();
            (
                p.capture_storage_pixels,
                p.capture_output_pixels,
                p.capture_screen_count,
            )
        });
        self.meta.capture_storage_pixels = storage.0;
        self.meta.capture_output_pixels = storage.1;
        self.meta.capture_screen_count = storage.2;
        {
            let _gate = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            if !self.current(shared) {
                return;
            }
        }
        let density = shared.preview_density.load(Ordering::Acquire);
        let density_changed = self.preview_density != density;
        if density_changed {
            self.preview_density = density;
            self.preview_dirty |= self.output.is_some();
        }
        if self.preview_dirty
            && (force_preview
                || density_changed
                || self.last_preview.is_none_or(|last| {
                    last.elapsed()
                        >= if self
                            .pipeline
                            .as_ref()
                            .is_some_and(|p| p.probe().queue_depth > 0)
                        {
                            BUSY_PREVIEW_INTERVAL
                        } else {
                            PREVIEW_INTERVAL
                        }
                }))
        {
            if let Some(snapshot) = self.output.as_ref() {
                match render_preview_incremental(
                    &mut self.preview_strip,
                    snapshot,
                    self.preview_density,
                ) {
                    Ok((image, source_rect)) => {
                        self.meta.preview_revision = self.meta.preview_revision.saturating_add(1);
                        self.preview = Some(LongPreview {
                            image: Arc::new(image),
                            revision: self.meta.preview_revision,
                            generation: self.meta.generation,
                            width: snapshot.width,
                            height: snapshot.height,
                            viewport: snapshot.viewport,
                            axis: snapshot.axis,
                            source_rect,
                        });
                    }
                    Err(error) => {
                        self.meta.status = trf!("预览生成失败，结果仍可编辑或保存：{error}", "Preview generation failed; the result can still be edited or saved: {error}");
                    }
                }
            }
            self.last_preview = Some(Instant::now());
            self.preview_dirty = false;
        }
        let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if !self.current(shared) {
            return;
        }
        self.meta.requested_generation = shared.requested_generation.load(Ordering::Acquire);
        self.meta.busy |= self.drain.is_some();
        if let Some(p) = &self.pipeline {
            let p = p.probe();
            shared
                .capture_in_flight
                .store(p.capture_in_flight, Ordering::Release);
            self.meta.busy |= self.paused && !p.producer_quiesced;
        }
        self.meta.result_available = self.output.is_some();
        if !self.initialize {
            if let Some(preview) = self.preview.as_mut() {
                preview.generation = self.meta.generation;
            }
        }
        latest.meta = self.meta.clone();
        latest.snapshot = self.output.clone();
        latest.preview = self.preview.clone();
    }
    #[cfg(test)]
    fn update_output(&mut self) {
        if let Some(stitch) = self.stitch.as_ref() {
            let snapshot = Arc::new(stitch.snapshot());
            self.meta.width = snapshot.width;
            self.meta.height = snapshot.height;
            self.meta.viewport = snapshot.viewport;
            self.meta.can_undo = stitch.can_undo();
            self.output = Some(snapshot);
            self.preview_dirty = true;
        }
    }
    fn apply(&mut self, command: Command, shared: &Shared) {
        // UI 先投递再发布 requested；借用同门，避免消费者把尚未完成发布的命令丢弃。
        let accepted = {
            let _gate = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            command.generation == shared.requested_generation.load(Ordering::Acquire)
        };
        if !accepted {
            return;
        }
        self.automatic.take();
        self.auto_post_fence = None;
        self.meta.auto.active = false;
        self.meta.auto.phase = AutoPhase::Off;
        self.meta.auto.status.clear();
        if !matches!(&command.operation, Operation::Region(_)) {
            self.region_request = None;
        }
        if matches!(&command.operation, Operation::Drain) {
            self.drain = self.pipeline.as_ref().and_then(|p| p.probe().drain_fence);
            if self.drain.is_none() {
                // 在途尾帧可能在 apply 前已报错并收束目标代，不能重新等待一个不存在的 fence。
                self.meta.generation = command.generation;
                self.meta.busy = false;
                self.paused = true;
                if self.meta.phase != LongPhase::Failed {
                    self.meta.phase = LongPhase::Paused;
                    self.meta.status = tr!(
                        "采集已暂停；最后成功结果仍可编辑或保存",
                        "Capturing paused; the last good result can still be edited or saved"
                    )
                    .into();
                }
                self.publish(shared, true);
                return;
            }
            self.capture_once = false;
            self.meta.phase = LongPhase::Pausing;
            self.meta.busy = true;
            self.meta.status = tr!(
                "正在处理已采集的尾帧…",
                "Processing the last captured frames…"
            )
            .into();
            self.publish(shared, false);
            return;
        }
        self.drain = None;
        self.meta.generation = command.generation;
        match command.operation {
            Operation::Pause(paused) => {
                self.paused = paused;
                self.capture_once = false;
                if self.stitch.is_none() && !paused {
                    self.initialize = true;
                }
                self.meta.phase = if paused {
                    LongPhase::Paused
                } else {
                    LongPhase::Running
                };
                self.meta.status = if paused {
                    tr!(
                        "已暂停，可以采集单帧、撤销或编辑",
                        "Paused; you can capture a frame, undo or edit"
                    )
                } else {
                    tr!(
                        "请在目标页面手动滚动；正在自动采集",
                        "Scroll the target page manually; capturing automatically"
                    )
                }
                .into();
            }
            Operation::Capture => {
                self.paused = true;
                self.capture_once = true;
                self.meta.phase = LongPhase::Paused;
            }
            Operation::Restart(options) => {
                if let Some(p) = &self.pipeline {
                    p.set_interval(options.poll_interval);
                }
                self.options = options;
                self.meta.axis = self.options.stitch.axis;
                self.meta.phase = LongPhase::Starting;
                self.meta.status = tr!(
                    "正在重新采集；成功后替换原结果…",
                    "Capturing again; the result is replaced on success…"
                )
                .into();
                self.initialize = true;
                self.paused = false;
                self.capture_once = false;
            }
            Operation::Undo => {
                self.paused = true;
                self.capture_once = false;
                self.meta.phase = LongPhase::Paused;
                let mut staged = (!self.initialize).then(|| self.stitch.clone()).flatten();
                if staged.as_mut().is_some_and(Stitcher::undo) {
                    let stitch = staged.expect("撤销成功的拼接器");
                    let output = Arc::new(stitch.snapshot());
                    let accepted = ((self.meta.width, self.meta.height)
                        != (output.width, output.height))
                        .then(|| self.meta.accepted_frames.saturating_sub(1));
                    // 与匹配帧相同的代次提交门，取消的旧撤销不得污染下一次重启的基线。
                    if !self.commit(shared, stitch, Some(output), accepted) {
                        return;
                    }
                    self.meta.last_result = UpdateKind::ViewportOnly;
                    self.meta.last_delta = 0;
                    self.meta.last_quality = None;
                    self.meta.status = tr!(
                        "已撤销上一帧，采集已暂停",
                        "Last frame undone; capturing paused"
                    )
                    .into();
                } else {
                    self.meta.status = tr!(
                        "没有可撤销的采集帧，采集已暂停",
                        "No frame to undo; capturing paused"
                    )
                    .into();
                }
            }
            Operation::Auto(binding) => {
                if let Some((target, options)) = binding {
                    self.paused = false;
                    self.capture_once = false;
                    self.meta.phase = LongPhase::Running;
                    let policy = AutoPolicy::new(options, command.generation, Instant::now());
                    self.meta.auto = policy.snapshot(target.label().to_string());
                    self.meta.status = self.meta.auto.status.clone();
                    self.automatic = Some((target, policy));
                } else {
                    // 较早 Pause 可能被本命令代取代，不能继承尚未应用的旧 worker 状态。
                    self.paused = command.mode == Mode::Paused;
                    self.capture_once = false;
                    self.meta.phase = if self.paused {
                        LongPhase::Paused
                    } else {
                        LongPhase::Running
                    };
                    self.meta.status = if self.paused {
                        tr!(
                            "已停止自动滚动，采集保持暂停",
                            "Auto-scroll stopped; capturing stays paused"
                        )
                    } else {
                        tr!(
                            "已停止自动滚动；仍可手动滚动采集",
                            "Auto-scroll stopped; you can still scroll manually to capture"
                        )
                    }
                    .into();
                }
            }
            Operation::Region(request) => {
                self.paused = true;
                self.capture_once = true;
                self.meta.pending_region = Some(request.capture.region);
                self.meta.phase = LongPhase::Starting;
                self.meta.status = tr!(
                    "正在确认新选区，原长图已保留…",
                    "Confirming the new area; the original long image is kept…"
                )
                .into();
                self.region_request = Some(request);
            }
            Operation::CancelRegion(context) => {
                self.options = context.options;
                self.frame_gate = context.capture.gate.clone();
                self.meta.region = context.capture.region;
                self.meta.pending_region = None;
                self.paused = true;
                self.capture_once = false;
                self.initialize = self.stitch.is_none();
                self.meta.phase = LongPhase::Paused;
                self.meta.status = tr!(
                    "已取消新选区，原长图和选区已保留",
                    "New area canceled; the original long image and area are kept"
                )
                .into();
            }
            Operation::Drain => unreachable!("排空请求已处理"),
        }
        if let Some(p) = &self.pipeline {
            p.set_coalescing(
                self.automatic.is_none() && shared.allow_coalescing.load(Ordering::Acquire),
            );
        }
        self.meta.busy = (self.initialize && !self.paused) || self.capture_once;
        self.publish(shared, self.paused);
    }
    fn fail(&mut self, error: String, shared: &Shared) {
        if self.region_request.is_some() {
            self.rollback_region(error, shared);
            return;
        }
        if !self.current(shared) {
            return;
        }
        let requested = shared.requested_generation.load(Ordering::Acquire);
        if let Some(fence) = self
            .pipeline
            .as_ref()
            .and_then(|p| p.probe().drain_fence)
            .filter(|fence| {
                fence.from_epoch == self.meta.generation && fence.desired_epoch == requested
            })
        {
            self.meta.generation = fence.desired_epoch;
        }
        self.drain = None;
        self.pause_pipeline();
        self.automatic.take();
        self.meta.auto.active = false;
        self.meta.auto.phase = AutoPhase::Blocked;
        self.meta.auto.status = error.clone();
        self.paused = true;
        // 新配置尚无成功初帧时必须保持待初始化状态；不能继续喂给旧轴/旧固定栏基线。
        self.capture_once = false;
        self.meta.phase = LongPhase::Failed;
        self.meta.busy = false;
        self.meta.status = trf!(
            "采集失败：{error}；已有结果已保留",
            "Capture failed: {error}; the existing result is kept"
        );
        if let Some(output) = self.output.as_ref() {
            self.meta.axis = output.axis;
        }
        self.publish(shared, true);
    }
    fn rollback_region(&mut self, error: String, shared: &Shared) {
        let Some(request) = self.region_request.clone() else {
            return;
        };
        let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if shared.stop.load(Ordering::Acquire)
            || shared.requested_generation.load(Ordering::Acquire) != self.meta.generation
            || latest.pending_region.is_none()
        {
            return;
        }
        let Some(generation) = self.meta.generation.checked_add(1) else {
            return;
        };
        let Some(retired_capture) = self
            .pipeline
            .as_ref()
            .expect("生产采集器")
            .restore_context_if_current(
                self.meta.generation,
                generation,
                request.previous.capture.clone(),
                request.previous.options.stitch.axis,
            )
        else {
            return;
        };
        shared
            .requested_generation
            .store(generation, Ordering::Release);
        self.meta.generation = generation;
        self.meta.requested_generation = generation;
        self.options = request.previous.options.clone();
        self.frame_gate = request.previous.capture.gate.clone();
        self.meta.region = request.previous.capture.region;
        self.meta.pending_region = None;
        self.meta.phase = LongPhase::Paused;
        self.meta.busy = false;
        self.meta.status = trf!(
            "调整选区失败：{error}；原长图和选区已保留",
            "Adjusting the area failed: {error}; the original long image and area are kept"
        );
        self.paused = true;
        self.capture_once = false;
        self.initialize = self.stitch.is_none();
        self.region_request = None;
        latest.pending_region = None;
        latest.context = Some(request.previous);
        drop(latest);
        drop(retired_capture);
        self.publish(shared, true);
    }
    fn pause_pipeline(&self) {
        if let Some(p) = &self.pipeline {
            p.pause_if_current(self.meta.generation, self.options.stitch.axis);
        }
    }
    /// 只在计算结束后的极短锁中安装 Arc/元数据，不让 UI 等待匹配或预览计算。
    fn commit(
        &mut self,
        shared: &Shared,
        stitch: Stitcher,
        output: Option<Arc<StitchSnapshot>>,
        accepted_frames: Option<u64>,
    ) -> bool {
        // 匹配可能耗时，归属门不在 mailbox 锁内调用；失效不得安装迟到帧。
        if !self.current(shared) {
            return false;
        }
        let frame_gate = self.region_request.as_ref().map_or_else(
            || self.frame_gate.clone(),
            |request| request.capture.gate.clone(),
        );
        if let Some(error) = frame_gate
            .as_ref()
            .and_then(|gate| gate().err().map(|error| error.to_string()))
        {
            self.fail(error, shared);
            return false;
        }
        let old_stitch;
        let old_output;
        let old_latest_snapshot;
        {
            let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            if !self.current(shared) {
                return false;
            }
            old_stitch = self.stitch.replace(stitch);
            if let Some(request) = self.region_request.take() {
                self.options = request.previous.options.clone();
                self.frame_gate = request.capture.gate.clone();
                self.meta.region = request.capture.region;
                self.meta.pending_region = None;
                self.meta.auto.target = request
                    .target
                    .as_ref()
                    .map(|target| target.label().to_string())
                    .unwrap_or_default();
                latest.context = Some(CommittedContext {
                    capture: request.capture,
                    options: self.options.clone(),
                    target: request.target,
                });
                latest.pending_region = None;
            } else if let Some(context) = latest.context.as_mut() {
                context.options = self.options.clone();
            }
            if let Some(output) = output {
                self.meta.width = output.width;
                self.meta.height = output.height;
                self.meta.viewport = output.viewport;
                self.meta.axis = output.axis;
                old_output = self.output.replace(output);
                self.preview_dirty = true;
            } else {
                old_output = None;
            }
            self.meta.can_undo = self.stitch.as_ref().is_some_and(Stitcher::can_undo);
            if let Some(accepted_frames) = accepted_frames {
                self.meta.accepted_frames = accepted_frames;
            }
            // 同一提交事务保留成功结果，不等昂贵的预览结束才更新最后成功像素。
            old_latest_snapshot = std::mem::replace(&mut latest.snapshot, self.output.clone());
            latest.meta.region = self.meta.region;
            latest.meta.pending_region = self.meta.pending_region;
            latest.meta.width = self.meta.width;
            latest.meta.height = self.meta.height;
            latest.meta.viewport = self.meta.viewport;
            latest.meta.axis = self.meta.axis;
            latest.meta.can_undo = self.meta.can_undo;
            latest.meta.accepted_frames = self.meta.accepted_frames;
            latest.meta.result_available = self.output.is_some();
        }
        drop(old_stitch);
        drop(old_output);
        drop(old_latest_snapshot);
        true
    }
    fn verify_auto_target(&mut self, shared: &Shared) -> bool {
        let error = self.automatic.as_ref().and_then(|(target, _)| {
            target
                .validate(self.meta.region, self.options.stitch.axis)
                .err()
        });
        if let Some(error) = error {
            self.automatic.take();
            self.meta.auto.active = false;
            self.meta.auto.phase = AutoPhase::Blocked;
            self.meta.auto.status = error.to_string();
            self.meta.status = trf!(
                "{}；已有结果已保留",
                "{}; the existing result is kept",
                self.meta.auto.status
            );
            self.paused = true;
            self.pause_pipeline();
            self.meta.busy = false;
            self.meta.phase = LongPhase::Paused;
            self.publish(shared, true);
            return false;
        }
        true
    }
    fn collect(&mut self, frame: CapturedFrame, shared: &Shared) {
        let _flight = ConsumerGuard::new(shared);
        let pipeline = self.pipeline.as_ref().expect("生产抓屏器").clone();
        let _completion = FrameCompletion {
            pipeline: pipeline.clone(),
            sequence: frame.stamp.sequence,
        };
        shared
            .last_frame_sequence
            .store(frame.stamp.sequence, Ordering::Release);
        if !pipeline.accepts(&frame.stamp) || !self.current(shared) {
            return;
        }
        self.meta.poll_count = frame.stamp.sequence;
        if let Some(hook) = &shared.match_hook {
            if let Err(error) = hook(&frame.stamp) {
                self.fail(error.to_string(), shared);
                return;
            }
        }
        if !pipeline.accepts(&frame.stamp) || !self.current(shared) {
            return;
        }
        if self.region_request.is_some() {
            self.collect_region(frame.image.clone(), shared);
            return;
        }
        if !self.verify_auto_target(shared) {
            return;
        }
        self.meta.busy = true;
        self.publish(shared, false);
        let first = self.initialize || self.stitch.is_none();
        let image = frame.image.clone();
        if !self.current(shared) {
            // 取消发生在读回末尾时，直接释放本帧，避免继续做灰度/匹配后才拒绝提交。
            self.meta.discarded_frames = self.meta.discarded_frames.saturating_add(1);
            return;
        }
        // 捕获期间窗口可以被用户移动或关闭；错误目标帧不能先拼入结果再停止驱动。
        if !self.verify_auto_target(shared) {
            return;
        }
        if first {
            let stitch = match Stitcher::with_options_shared(image, self.options.stitch.clone()) {
                Ok(stitch) => stitch,
                Err(error) => {
                    self.fail(error.to_string(), shared);
                    return;
                }
            };
            let output = Arc::new(stitch.snapshot());
            if !self.commit(shared, stitch, Some(output), Some(1)) {
                self.meta.discarded_frames = self.meta.discarded_frames.saturating_add(1);
                return;
            }
            self.initialize = false;
            self.meta.last_result = UpdateKind::Added;
            self.meta.last_delta = 0;
            self.meta.last_quality = None;
            self.meta.status = tr!(
                "请在目标页面手动滚动；正在自动采集",
                "Scroll the target page manually; capturing automatically"
            )
            .into();
        } else {
            // Clone 仅复制 Arc/metadata，匹配或用户新请求都不会污染已提交基线。
            let mut staged = self.stitch.as_ref().expect("first 已检查拼接器").clone();
            let update = match staged.process_shared(image) {
                Ok(update) => update,
                Err(error) => {
                    self.fail(error.to_string(), shared);
                    return;
                }
            };
            let changed = matches!(update.kind, UpdateKind::Added | UpdateKind::ViewportOnly);
            let output = changed.then(|| Arc::new(staged.snapshot()));
            let accepted_frames = (update.kind == UpdateKind::Added)
                .then(|| self.meta.accepted_frames.saturating_add(1));
            if !self.commit(shared, staged, output, accepted_frames) {
                self.meta.discarded_frames = self.meta.discarded_frames.saturating_add(1);
                return;
            }
            self.meta.last_result = update.kind;
            self.meta.last_delta = update.delta;
            self.meta.last_quality = update.quality;
            if matches!(update.kind, UpdateKind::Added | UpdateKind::ViewportOnly) {
                self.trusted_scroll_position = self
                    .trusted_scroll_position
                    .saturating_add(i64::from(update.delta));
            }
            self.meta.viewport = update.viewport;
            self.meta.status = match update.kind {
                UpdateKind::Added => {
                    trf!(
                        "已增加 {} 像素；继续手动滚动",
                        "Added {} pixels; keep scrolling manually",
                        update.added
                    )
                }
                UpdateKind::ViewportOnly => tr!(
                    "已回到采集范围内，未重复拼接",
                    "Back within the captured range; nothing stitched twice"
                )
                .into(),
                UpdateKind::Unchanged => {
                    tr!("等待页面滚动…", "Waiting for the page to scroll…").into()
                }
                UpdateKind::Rejected => update.reason.unwrap_or_else(|| {
                    tr!(
                        "未找到可靠重叠，请放慢滚动",
                        "No reliable overlap found; scroll more slowly"
                    )
                    .into()
                }),
                UpdateKind::Limit => {
                    self.paused = true;
                    if self.drain.is_none() {
                        self.pause_pipeline();
                    }
                    update.reason.unwrap_or_else(|| {
                        tr!(
                            "已达到长截图上限，请保存或编辑",
                            "The long capture limit was reached; save or edit it"
                        )
                        .into()
                    })
                }
            };
        }
        self.capture_once = false;
        self.meta.busy = false;
        self.meta.phase = if self.drain.is_some() {
            LongPhase::Pausing
        } else if self.paused {
            LongPhase::Paused
        } else {
            LongPhase::Running
        };
        if first && self.paused {
            self.meta.status = tr!(
                "已采集初始视口；采集已暂停",
                "Initial view captured; capturing paused"
            )
            .into();
        }
        if self.current(shared) && self.drain.is_none() {
            self.drive_auto(shared);
        }
        self.publish(shared, self.paused || first);
    }
    /// 采集器、已提交上下文与本线程选区同门切换，自动滚动和输出始终看到同一位置。
    fn apply_relocation(&mut self, shared: &Shared) {
        let Some(capture) = shared
            .relocation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        let Some(pipeline) = self.pipeline.clone() else {
            return;
        };
        if self.paused || self.drain.is_some() || self.region_request.is_some() {
            return;
        }
        let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if !self.current(shared) || latest.pending_region.is_some() {
            return;
        }
        let Some(context) = latest.context.as_mut() else {
            return;
        };
        if pipeline.relocate(capture.clone()).is_err() {
            return;
        }
        context.capture = capture.clone();
        self.frame_gate = capture.gate.clone();
        self.meta.region = capture.region;
        latest.meta.region = capture.region;
    }
    fn collect_region(&mut self, image: Arc<Image>, shared: &Shared) {
        let request = self.region_request.as_ref().expect("候选选区").clone();
        self.meta.busy = true;
        let prepared = match request.kind {
            RegionChange::Reset => {
                Stitcher::with_options_shared(image, request.previous.options.stitch.clone())
                    .map(|stitch| (stitch, UpdateKind::Added, 0, 1))
            }
            RegionChange::Continue => (|| {
                let old = self.stitch.as_ref().ok_or(tr!(
                    "还没有拼接成功的内容，暂不能移动选区",
                    "Nothing has been stitched yet; the area can't move"
                ))?;
                let identical = old.matches_last_frame(&image);
                let mut staged = old.clone();
                let update = staged.process_shared(image)?;
                let trusted = matches!(update.kind, UpdateKind::Added | UpdateKind::ViewportOnly)
                    || (update.kind == UpdateKind::Unchanged && identical);
                if !trusted {
                    return Err(update
                        .reason
                        .unwrap_or_else(|| {
                            tr!("新选区未可靠匹配原长图，请减小移动距离或重新采集", "The new area doesn't reliably match the long image; move less or capture again").into()
                        })
                        .into());
                }
                let accepted = self
                    .meta
                    .accepted_frames
                    .saturating_add(u64::from(update.kind == UpdateKind::Added));
                Ok((staged, update.kind, update.delta, accepted))
            })(),
        };
        let (stitch, kind, delta, accepted) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail(error.to_string(), shared);
                return;
            }
        };
        let output = Arc::new(stitch.snapshot());
        if !self.commit(shared, stitch, Some(output), Some(accepted)) {
            return;
        }
        self.meta.last_result = kind;
        self.meta.last_delta = delta;
        self.meta.last_quality = None;
        self.initialize = false;
        self.capture_once = false;
        self.paused = true;
        self.meta.phase = LongPhase::Paused;
        self.meta.busy = false;
        self.meta.status = tr!(
            "新选区已确认，采集暂停；可继续滚动或输出",
            "New area confirmed and capturing paused; keep scrolling or export"
        )
        .into();
        self.publish(shared, true);
    }
    fn drive_auto(&mut self, shared: &Shared) {
        let Some((target, mut policy)) = self.automatic.take() else {
            return;
        };
        // 排队或读回中的旧 permit 可以用于拼接，但不能推进本次滚轮响应策略。
        if self
            .auto_post_fence
            .is_some_and(|fence| self.meta.poll_count <= fence)
        {
            let mut probe = shared.auto_probe.lock().unwrap_or_else(|e| e.into_inner());
            probe.ignored_pre_post_frames = probe.ignored_pre_post_frames.saturating_add(1);
            probe.record(
                "ignore-pre-post",
                self.meta.generation,
                self.meta.poll_count,
                self.auto_post_fence,
            );
            drop(probe);
            self.automatic = Some((target, policy));
            return;
        }
        shared
            .auto_probe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(
                "observe",
                self.meta.generation,
                self.meta.poll_count,
                self.auto_post_fence,
            );
        let now = Instant::now();
        let mut decision = policy.observe_match(
            self.meta.generation,
            self.meta.poll_count,
            self.meta.last_result,
            self.meta.last_delta,
            self.trusted_scroll_position,
            self.meta.last_quality,
            now,
        );
        // 只在新采集帧之后决策；原生消息投递与 UI 请求版本更新原子排序。
        if decision == Decision::Post {
            let region = self.meta.region;
            let axis = self.options.stitch.axis;
            let forward = policy.options.direction.forward();
            if !self.post_auto_guarded(
                shared,
                &mut policy,
                target.label(),
                || target.validate(region, axis),
                || target.post_step_validated(axis, forward, AUTO_WHEEL_DELTA),
            ) {
                return;
            }
            decision = Decision::Wait;
        }
        self.meta.auto = policy.snapshot(target.label().to_string());
        match decision {
            Decision::Stop { phase, reason } => {
                self.stop_auto_feedback(phase, reason, shared);
            }
            Decision::Wait => {
                self.meta.status = self.meta.auto.status.clone();
                self.automatic = Some((target, policy));
            }
            Decision::Post => unreachable!("Post 已在 gate 内处理"),
        }
    }
    /// 与采集复用已提交的 CaptureConfig 和原租约；绝不为发送重新创建 gate。
    /// Win32/租约校验不持短锁，成功后再与 UI 控制发布、read permit 原子排序。
    fn post_auto_guarded(
        &mut self,
        shared: &Shared,
        policy: &mut AutoPolicy,
        target_name: &str,
        validate_target: impl FnOnce() -> Result<()>,
        post: impl FnOnce() -> Result<()>,
    ) -> bool {
        let capture = {
            let latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            // drain 允许旧帧继续提交，但旧代不能再发布自动输入。
            if !self.current(shared)
                || shared.requested_generation.load(Ordering::Acquire) != self.meta.generation
            {
                return false;
            }
            let Some(context) = latest.context.as_ref() else {
                drop(latest);
                self.fail("自动滚动的采集上下文尚未确认".into(), shared);
                return false;
            };
            context.capture.clone()
        };
        let capture_validation = capture.gate.as_ref().map_or(Ok(()), |gate| gate());
        let native_validation = capture_validation.is_ok().then(validate_target);
        let latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        if !self.current(shared)
            || shared.requested_generation.load(Ordering::Acquire) != self.meta.generation
            || latest.context.as_ref().is_none_or(|context| {
                !Arc::ptr_eq(&context.capture, &capture)
                    || context.capture.region != self.meta.region
                    || context.options.stitch.axis != self.options.stitch.axis
            })
            || self
                .pipeline
                .as_ref()
                .is_none_or(|pipeline| !Arc::ptr_eq(&pipeline.context(), &capture))
        {
            return false;
        }
        if let Err(error) = capture_validation {
            // fail 会自行取得 mailbox；不能在这里持 latest 锁重入。
            drop(latest);
            shared
                .auto_probe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(
                    "capture-gate-rejected",
                    self.meta.generation,
                    self.meta.poll_count,
                    self.auto_post_fence,
                );
            self.meta.auto = policy.snapshot(target_name.to_string());
            self.fail(error.to_string(), shared);
            return false;
        }
        let posted = native_validation
            .expect("capture gate 已成功")
            .and_then(|()| {
                self.pipeline
                    .as_ref()
                    .expect("生产抓屏器")
                    .with_post_fence(self.meta.generation, post)
            });
        match posted {
            Ok(Some(fence)) => {
                policy.sent(fence, Instant::now());
                self.auto_post_fence = Some(fence);
                let mut probe = shared.auto_probe.lock().unwrap_or_else(|e| e.into_inner());
                probe.last_post_fence = Some(fence);
                probe.record(
                    "post",
                    self.meta.generation,
                    self.meta.poll_count,
                    Some(fence),
                );
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.meta.auto = policy.snapshot(target_name.to_string());
                self.meta.auto.active = false;
                self.meta.auto.phase = AutoPhase::Blocked;
                self.meta.auto.status = error.to_string();
                self.meta.status = trf!(
                    "{}；已有结果已保留",
                    "{}; the existing result is kept",
                    self.meta.auto.status
                );
                self.paused = true;
                self.pause_pipeline();
                self.meta.phase = LongPhase::Paused;
                false
            }
        }
    }
    fn stop_auto_feedback(&mut self, phase: AutoPhase, reason: &str, shared: &Shared) {
        self.meta.auto.active = false;
        self.meta.auto.phase = phase;
        self.meta.auto.status = reason.into();
        self.meta.status = reason.into();
        self.automatic = None;
        self.auto_post_fence = None;
        if let Some(pipeline) = &self.pipeline {
            pipeline.set_coalescing(shared.allow_coalescing.load(Ordering::Acquire));
        }
        // 匹配反馈只停止自动输入，仍允许手动回滚恢复。
        // 原生目标校验失败在独立分支暂停，抓屏归属门失效走 Failed。
    }
    fn expire_auto_without_frame(&mut self, shared: &Shared) {
        if !self.current(shared) {
            return;
        }
        let Some((target, policy)) = self.automatic.take() else {
            return;
        };
        let decision = policy.idle(self.meta.generation, Instant::now());
        self.meta.auto = policy.snapshot(target.label().to_string());
        match decision {
            Decision::Stop { phase, reason } => self.stop_auto_feedback(phase, reason, shared),
            Decision::Wait => self.automatic = Some((target, policy)),
            Decision::Post => unreachable!("无新帧的超时检查不能发送滚轮"),
        }
    }
    fn run(mut self, receiver: Receiver<Command>, shared: &Shared) {
        if let Some(p) = &self.pipeline {
            p.set_coalescing(shared.allow_coalescing.load(Ordering::Acquire));
        }
        self.publish(shared, false);
        while !shared.stop.load(Ordering::Acquire) {
            while let Ok(command) = receiver.try_recv() {
                self.apply(command, shared);
            }
            if shared.stop.load(Ordering::Acquire) {
                break;
            }
            self.apply_relocation(shared);
            if let Some(pipeline) = self.pipeline.clone() {
                if let Some(error) = pipeline.take_error_for_epoch(self.meta.generation) {
                    self.fail(error, shared);
                }
                if let Some(frame) = pipeline.try_take_for_epoch(self.meta.generation) {
                    self.collect(frame, shared);
                    continue;
                }
                if let Some(fence) = self.drain {
                    if pipeline.drain_ready(&fence)
                        && shared.requested_generation.load(Ordering::Acquire)
                            == fence.desired_epoch
                    {
                        let _gate = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
                        if shared.requested_generation.load(Ordering::Acquire)
                            == fence.desired_epoch
                            && pipeline.finish_drain(&fence)
                        {
                            self.meta.generation = fence.desired_epoch;
                            self.drain = None;
                            self.paused = true;
                            self.capture_once = false;
                            self.meta.phase = LongPhase::Paused;
                            self.meta.busy = false;
                            if self.meta.last_result != UpdateKind::Limit {
                                self.meta.status = tr!(
                                    "尾帧已处理，采集已暂停",
                                    "Last frames processed; capturing paused"
                                )
                                .into();
                            }
                        }
                    }
                }
            }
            if self.drain.is_none() && !self.capture_once && self.paused {
                self.meta.busy = false;
            }
            self.expire_auto_without_frame(shared);
            self.publish(shared, self.paused);
            let wait = if self.paused || self.meta.phase == LongPhase::Failed {
                WAIT
            } else {
                Duration::from_millis(4)
            };
            match receiver.recv_timeout(wait) {
                Ok(command) => self.apply(command, shared),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if let Some(pipeline) = &self.pipeline {
            pipeline.stop();
            pipeline.join();
        }
        self.stitch.take();
        self.automatic.take();
        if shared.stop.load(Ordering::Acquire) {
            // 停止版本在 GPU 资源释放后生成有界预览，不持有界面锁计算或等 UI 回调。
            let frozen = {
                let latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
                (
                    latest.snapshot.clone(),
                    latest.meta.generation,
                    latest.meta.preview_revision,
                )
            };
            if let (Some(snapshot), generation, revision) = frozen {
                let density = shared.preview_density.load(Ordering::Acquire);
                if let Ok((image, source_rect)) = render_preview(&snapshot, density) {
                    let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
                    if latest.meta.generation == generation {
                        let revision = revision.saturating_add(1);
                        latest.meta.preview_revision = revision;
                        latest.preview = Some(LongPreview {
                            image: Arc::new(image),
                            revision,
                            generation,
                            width: snapshot.width,
                            height: snapshot.height,
                            viewport: snapshot.viewport,
                            axis: snapshot.axis,
                            source_rect,
                        });
                    }
                }
            }
        }
        // output/preview 的像素只被最后成功 mailbox 保留；不清空可导出结果。
    }
}

#[cfg(test)]
#[path = "long_region_runtime_tests.rs"]
mod region_tests;

#[cfg(test)]
#[path = "long_auto_runtime_tests.rs"]
mod auto_runtime_tests;

#[cfg(test)]
#[path = "long_auto_post_gate_tests.rs"]
mod auto_post_gate_tests;

#[cfg(test)]
#[path = "long_auto_popup_runtime_tests.rs"]
mod auto_popup_runtime_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_pipeline_failed_tail_before_drain_apply_preserves_terminal_phase() {
        struct PausedSource {
            releasing: Sender<()>,
            allow_release: Receiver<()>,
        }
        impl crate::long_capture_pipeline::CaptureSource for PausedSource {
            fn read(&mut self, _: ScreenRect, _: &dyn Fn() -> bool) -> Result<Arc<Image>> {
                Err("本控制事务夹具不发 read permit".into())
            }
            fn release(&mut self) {
                let _ = self.releasing.send(());
                // 真实 producer 已进入 release，但尚未返回，不能提前断言 quiesced。
                let _ = self.allow_release.recv_timeout(Duration::from_secs(10));
            }
        }
        let region = ScreenRect {
            left: 0,
            top: 0,
            width: 64,
            height: 128,
        };
        let options = LongOptions::default();
        let (releasing, release_started) = mpsc::channel();
        let (allow_release, release_allowed) = mpsc::channel();
        let pipeline = CapturePipeline::start_with_source(
            region,
            Duration::ZERO,
            None,
            Box::new(PausedSource {
                releasing,
                allow_release: release_allowed,
            }),
        )
        .unwrap();
        // 只验证失败与尚未 apply 的真实控制 fence 排序，不声称这里采集过任何窗口。
        pipeline.configure(0, ScrollAxis::Vertical, Mode::Paused);
        pipeline.begin_drain(1);
        let shared = Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: None,
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(1),
            capture_in_flight: AtomicBool::new(false),
            preview_density: AtomicU32::new(100),
            stop: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
            pipeline: Some(pipeline.clone()),
            match_hook: None,
            consumer_in_flight: AtomicBool::new(false),
            last_frame_sequence: AtomicU64::new(0),
            allow_coalescing: AtomicBool::new(false),
            auto_probe: Mutex::new(AutoProbe::default()),
            relocation: Mutex::new(None),
        };
        let mut worker = Worker::new(region, options);
        worker.pipeline = Some(pipeline.clone());
        worker.fail("受控尾帧失败".into(), &shared);
        assert_eq!(
            (worker.meta.generation, worker.meta.phase),
            (1, LongPhase::Failed)
        );
        assert!(worker.meta.status.contains("受控尾帧失败"));
        assert!(pipeline.probe().drain_fence.is_none());
        release_started
            .recv_timeout(Duration::from_secs(10))
            .expect("生产者尚未进入 release");
        assert!(!pipeline.probe().producer_quiesced);
        allow_release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !pipeline.probe().producer_quiesced {
            assert!(
                Instant::now() < until,
                "真实 release 返回后未收到 quiesced ACK"
            );
            thread::yield_now();
        }
        worker.apply(
            Command {
                generation: 1,
                operation: Operation::Drain,
                mode: Mode::Paused,
            },
            &shared,
        );
        assert_eq!(
            (worker.meta.generation, worker.meta.phase),
            (1, LongPhase::Failed)
        );
        assert!(!worker.meta.busy && worker.meta.status.contains("受控尾帧失败"));
        pipeline.stop();
        pipeline.join();
    }
    #[test]
    fn long_preview_quality_density_caps_memory_and_preserves_both_axes() {
        for (scale, limits) in [
            (1., (200, 700)),
            (1.25, (250, 875)),
            (2., (400, 1400)),
            (2.5, (500, 1750)),
            (4., (500, 1750)),
        ] {
            let density = preview_density(scale);
            assert_eq!(preview_limits(ScrollAxis::Vertical, density), limits);
            assert_eq!(
                preview_limits(ScrollAxis::Horizontal, density),
                (limits.1, limits.0)
            );
            assert!(limits.0 as u64 * limits.1 as u64 <= 875_000);
        }
        assert_eq!(preview_density(f32::NAN), 100);
        assert_eq!(preview_density(-1.), 100);
    }
    #[test]
    fn long_preview_quality_paused_density_request_preserves_capture_barrier_and_strips() {
        let region = ScreenRect {
            left: 0,
            top: 0,
            width: 480,
            height: 360,
        };
        let options = LongOptions::default();
        let shared = Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: None,
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(0),
            capture_in_flight: AtomicBool::new(false),
            preview_density: AtomicU32::new(100),
            stop: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
            pipeline: None,
            match_hook: None,
            consumer_in_flight: AtomicBool::new(false),
            last_frame_sequence: AtomicU64::new(0),
            allow_coalescing: AtomicBool::new(false),
            auto_probe: Mutex::new(AutoProbe::default()),
            relocation: Mutex::new(None),
        };
        let mut worker = Worker::new(region, options.clone());
        worker.stitch = Some(
            Stitcher::with_options(
                Image {
                    width: region.width,
                    height: region.height,
                    bgra: vec![255; region.width as usize * region.height as usize * 4],
                    readback_ms: 0.,
                },
                options.stitch,
            )
            .unwrap(),
        );
        worker.initialize = false;
        worker.paused = true;
        worker.meta.phase = LongPhase::Paused;
        worker.update_output();
        worker.publish(&shared, true);
        let before = shared.latest.lock().unwrap().snapshot.clone().unwrap();
        let old_revision = shared
            .latest
            .lock()
            .unwrap()
            .preview
            .as_ref()
            .unwrap()
            .revision;
        shared.preview_density.store(125, Ordering::Release);
        worker.publish(&shared, false);
        let latest = shared.latest.lock().unwrap();
        let preview = latest.preview.as_ref().unwrap();
        assert_eq!((preview.image.width, preview.image.height), (250, 188));
        assert!(preview.revision > old_revision);
        assert_eq!(preview.generation, 0);
        assert_eq!(shared.requested_generation.load(Ordering::Acquire), 0);
        assert!(worker.paused);
        assert!(Arc::ptr_eq(latest.snapshot.as_ref().unwrap(), &before));
        let unchanged = preview.revision;
        drop(latest);
        worker.publish(&shared, true);
        assert_eq!(
            shared
                .latest
                .lock()
                .unwrap()
                .preview
                .as_ref()
                .unwrap()
                .revision,
            unchanged
        );
    }

    #[test]
    fn long_stitch_runtime_default_uses_auto_without_changing_model_default() {
        assert!(LongOptions::default().stitch.detect_fixed_borders);
        assert!(!StitchOptions::default().detect_fixed_borders);
    }

    #[test]
    fn long_stitch_preview_window_tracks_vertical_and_horizontal_viewports() {
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let (width, height, viewport) = match axis {
                ScrollAxis::Vertical => (
                    800,
                    20_000,
                    Rect {
                        x: 0,
                        y: 17_000,
                        width: 800,
                        height: 600,
                    },
                ),
                ScrollAxis::Horizontal => (
                    20_000,
                    800,
                    Rect {
                        x: 17_000,
                        y: 0,
                        width: 600,
                        height: 800,
                    },
                ),
            };
            let mut snapshot = StitchSnapshot {
                width,
                height,
                axis,
                viewport,
                segments: Vec::new(),
            };
            let source = preview_source(&snapshot);
            assert!(source.valid(width, height));
            assert_eq!(source.intersection(viewport), Some(viewport));
            match axis {
                ScrollAxis::Vertical => {
                    assert_eq!((source.width, source.height), (800, 2800));
                    assert_eq!(preview_limits(axis, 100), (200, 700));
                    snapshot.viewport.y = height - 600;
                    assert_eq!(preview_source(&snapshot).y + 2800, height);
                }
                ScrollAxis::Horizontal => {
                    assert_eq!((source.width, source.height), (2800, 800));
                    assert_eq!(preview_limits(axis, 100), (700, 200));
                    snapshot.viewport.x = width - 600;
                    assert_eq!(preview_source(&snapshot).x + 2800, width);
                }
            }
        }
    }

    #[test]
    fn long_auto_control_barrier_clears_visible_driver_before_worker_ack() {
        let region = ScreenRect {
            left: -128,
            top: 32,
            width: 128,
            height: 96,
        };
        let options = LongOptions::default();
        let mut meta = LongSnapshot::starting(region, &options);
        meta.phase = LongPhase::Running;
        meta.busy = false;
        meta.auto.active = true;
        meta.auto.phase = AutoPhase::AwaitingFrame;
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest {
                meta,
                snapshot: None,
                preview: None,
                context: None,
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(0),
            capture_in_flight: AtomicBool::new(false),
            preview_density: AtomicU32::new(100),
            stop: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
            pipeline: None,
            match_hook: None,
            consumer_in_flight: AtomicBool::new(false),
            last_frame_sequence: AtomicU64::new(0),
            allow_coalescing: AtomicBool::new(false),
            auto_probe: Mutex::new(AutoProbe::default()),
            relocation: Mutex::new(None),
        });
        let (commands, receiver) = mpsc::sync_channel(8);
        let (cleanup, _cleanup_receiver) = mpsc::channel();
        let mut session = LongSession {
            shared: shared.clone(),
            commands,
            worker: None,
            cleanup,
            gate_factory: None,
            fixed_region_gate: false,
        };
        let mut worker = Worker::new(region, options);
        session.set_paused(true).unwrap();
        assert!(!worker.current(&shared));
        assert!(!session.snapshot().auto.active);
        assert_eq!(session.snapshot().phase, LongPhase::Pausing);
        assert!(session.snapshot().busy);
        worker.apply(receiver.recv().unwrap(), &shared);
        assert!(worker.current(&shared));
        assert_eq!(session.snapshot().phase, LongPhase::Paused);
        assert!(!session.snapshot().auto.active);
        session.set_paused(false).unwrap();
        worker.apply(receiver.recv().unwrap(), &shared);
        assert_eq!(session.snapshot().phase, LongPhase::Running);
        assert!(!session.snapshot().auto.active);
        session.set_auto_scroll(None).unwrap();
        worker.apply(receiver.recv().unwrap(), &shared);
        assert!(!session.snapshot().auto.active);
        session.stop();
        assert!(!worker.current(&shared));
        assert_eq!(session.snapshot().auto.phase, AutoPhase::Off);
    }

    #[test]
    fn failed_restart_keeps_previous_result_and_retries_new_configuration() {
        let region = ScreenRect {
            left: -128,
            top: 32,
            width: 128,
            height: 96,
        };
        let options = LongOptions::default();
        let shared = Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: None,
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(0),
            capture_in_flight: AtomicBool::new(false),
            preview_density: AtomicU32::new(100),
            stop: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
            pipeline: None,
            match_hook: None,
            consumer_in_flight: AtomicBool::new(false),
            last_frame_sequence: AtomicU64::new(0),
            allow_coalescing: AtomicBool::new(false),
            auto_probe: Mutex::new(AutoProbe::default()),
            relocation: Mutex::new(None),
        };
        let mut worker = Worker::new(region, options.clone());
        worker.stitch = Some(
            Stitcher::with_options(
                Image {
                    width: 128,
                    height: 96,
                    bgra: vec![255; 128 * 96 * 4],
                    readback_ms: 0.,
                },
                options.stitch,
            )
            .unwrap(),
        );
        worker.initialize = false;
        worker.update_output();
        worker.publish(&shared, true);
        let old = shared.latest.lock().unwrap().snapshot.clone().unwrap();

        let mut changed = LongOptions::default();
        changed.stitch.axis = ScrollAxis::Horizontal;
        changed.stitch.fixed_start = Some(8);
        changed.stitch.fixed_end = Some(4);
        shared.requested_generation.store(1, Ordering::Release);
        worker.apply(
            Command {
                generation: 1,
                operation: Operation::Restart(changed),
                mode: Mode::Running,
            },
            &shared,
        );
        worker.fail("合成初始化错误".into(), &shared);
        assert!(worker.initialize);
        assert!(worker.paused);
        assert_eq!(worker.meta.phase, LongPhase::Failed);
        assert_eq!(worker.options.stitch.axis, ScrollAxis::Horizontal);
        assert_eq!(
            worker.stitch.as_ref().unwrap().snapshot().axis,
            ScrollAxis::Vertical
        );
        let latest = shared.latest.lock().unwrap();
        assert!(!latest.meta.busy);
        assert!(Arc::ptr_eq(latest.snapshot.as_ref().unwrap(), &old));
        assert_eq!(latest.meta.axis, ScrollAxis::Vertical);
        drop(latest);

        shared.requested_generation.store(2, Ordering::Release);
        worker.apply(
            Command {
                generation: 2,
                operation: Operation::Pause(false),
                mode: Mode::Running,
            },
            &shared,
        );
        assert!(worker.initialize && !worker.paused && worker.meta.busy);
        shared.requested_generation.store(3, Ordering::Release);
        worker.apply(
            Command {
                generation: 3,
                operation: Operation::Capture,
                mode: Mode::Single,
            },
            &shared,
        );
        assert!(worker.initialize && worker.capture_once && worker.paused && worker.meta.busy);
        assert_eq!(worker.options.stitch.fixed_start, Some(8));
        assert_eq!(worker.options.stitch.fixed_end, Some(4));
    }
}
