//! 浮层 Pause 被 Auto(None) 覆盖时的实际生产者/消费者合同；不创建原生窗口。
use super::*;
use crate::long_capture_pipeline::CaptureSource;
use std::sync::{Condvar, Weak};

const DEADLINE: Duration = Duration::from_secs(10);
const INITIAL_EPOCH: u64 = 7;

fn wait(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !ready() {
        assert!(Instant::now() < deadline, "浮层运行时受控条件没有到达");
        thread::yield_now();
    }
}
fn world(width: u32, height: u32, first_row: u32) -> Image {
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in first_row..first_row + height {
        let mut row = y.wrapping_mul(2_654_435_761);
        row ^= row >> 16;
        for x in 0..width {
            bgra.extend_from_slice(&[
                row.wrapping_add(x * 17) as u8,
                ((row >> 8).wrapping_add(x * 5)) as u8,
                ((row >> 16).wrapping_add(x * 11)) as u8,
                255,
            ]);
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}

struct Source {
    reads: Sender<(u64, ScreenRect)>,
    replies: Receiver<Image>,
    returned: Sender<Weak<Image>>,
    count: Arc<AtomicU64>,
    releases: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
}
impl CaptureSource for Source {
    fn read(&mut self, region: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>> {
        let sequence = self.count.fetch_add(1, Ordering::AcqRel) + 1;
        self.reads.send((sequence, region))?;
        loop {
            if cancelled() {
                return Err("受控浮层 source 已取消".into());
            }
            match self.replies.recv_timeout(Duration::from_millis(2)) {
                Ok(image) => {
                    let image = Arc::new(image);
                    self.returned.send(Arc::downgrade(&image))?;
                    return Ok(image);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("source 回复通道已关闭".into())
                }
            }
        }
    }
    fn release(&mut self) {
        self.releases.fetch_add(1, Ordering::AcqRel);
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}
#[derive(Default)]
struct MatchLatch {
    released: Mutex<bool>,
    wake: Condvar,
}
impl MatchLatch {
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
}
struct Rig {
    session: LongSession,
    region: ScreenRect,
    reads: Receiver<(u64, ScreenRect)>,
    replies: Sender<Image>,
    returned: Receiver<Weak<Image>>,
    count: Arc<AtomicU64>,
    releases: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
    entered: Receiver<FrameStamp>,
    latch: Arc<MatchLatch>,
    worker_joined: Arc<AtomicBool>,
    _cleanup_receiver: Receiver<JoinHandle<()>>,
}
impl Rig {
    fn new() -> Self {
        let region = ScreenRect {
            left: -64,
            top: 20,
            width: 64,
            height: 128,
        };
        let options = LongOptions {
            stitch: StitchOptions {
                detect_fixed_borders: false,
                fixed_start: Some(0),
                fixed_end: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        let (read, reads) = mpsc::channel();
        let (replies, receive) = mpsc::channel();
        let (returned, receive_returned) = mpsc::channel();
        let count = Arc::new(AtomicU64::new(0));
        let releases = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let pipeline = CapturePipeline::start_with_source(
            region,
            Duration::ZERO,
            None,
            Box::new(Source {
                reads: read,
                replies: receive,
                returned,
                count: count.clone(),
                releases: releases.clone(),
                dropped: dropped.clone(),
            }),
        )
        .unwrap();
        wait(|| pipeline.probe().producer_quiesced);
        let latch = Arc::new(MatchLatch::default());
        let (enter, entered) = mpsc::channel();
        let hook_latch = latch.clone();
        let hook: MatchHook = Arc::new(move |stamp| {
            if stamp.sequence == 1 {
                enter.send(*stamp)?;
                let mut released = hook_latch
                    .released
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                while !*released {
                    released = hook_latch
                        .wake
                        .wait(released)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
            Ok(())
        });
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: Some(CommittedContext {
                    capture: pipeline.context(),
                    options: options.clone(),
                    target: Err("核心受控源没有原生滚动目标".into()),
                }),
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(INITIAL_EPOCH),
            capture_in_flight: AtomicBool::new(false),
            preview_density: AtomicU32::new(100),
            stop: AtomicBool::new(false),
            exited: Arc::new(AtomicBool::new(false)),
            pipeline: Some(pipeline.clone()),
            match_hook: Some(hook),
            consumer_in_flight: AtomicBool::new(false),
            last_frame_sequence: AtomicU64::new(0),
            allow_coalescing: AtomicBool::new(false),
            auto_probe: Mutex::new(AutoProbe::default()),
            relocation: Mutex::new(None),
        });
        let mut worker = Worker::new(region, options.clone());
        worker.pipeline = Some(pipeline.clone());
        worker.meta.generation = INITIAL_EPOCH;
        worker.meta.phase = LongPhase::Running;
        worker.meta.accepted_frames = 1;
        worker.stitch = Some(Stitcher::with_options(world(64, 128, 0), options.stitch).unwrap());
        worker.initialize = false;
        worker.update_output();
        pipeline.configure(INITIAL_EPOCH, ScrollAxis::Vertical, Mode::Running);
        worker.publish(&shared, true);
        let (commands, receiver) = mpsc::sync_channel(8);
        let lease = WorkerLease::new(shared.clone());
        let thread_shared = shared.clone();
        let handle = thread::Builder::new()
            .name("long-auto-popup-test-consumer".into())
            .spawn(move || {
                let _lease = lease;
                worker.run(receiver, &thread_shared);
            })
            .unwrap();
        let (cleanup, cleanup_receiver) = mpsc::channel();
        Self {
            session: LongSession {
                shared,
                commands,
                worker: Some(handle),
                cleanup,
                gate_factory: None,
                fixed_region_gate: false,
            },
            region,
            reads,
            replies,
            returned: receive_returned,
            count,
            releases,
            dropped,
            entered,
            latch,
            worker_joined: Arc::new(AtomicBool::new(false)),
            _cleanup_receiver: cleanup_receiver,
        }
    }
    fn quiet(&self, generation: u64) -> bool {
        let meta = self.session.snapshot();
        let p = self.session.pipeline_probe();
        meta.generation == generation
            && meta.requested_generation == generation
            && meta.phase == LongPhase::Paused
            && !meta.busy
            && !meta.auto.active
            && p.epoch == generation
            && p.producer_quiesced
            && !p.capture_in_flight
            && !p.consumer_in_flight
            && p.queue_depth == 0
            && p.reserved_raw_bytes == 0
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.latch.release();
        self.session.stop();
        if let Some(worker) = self.session.worker.take() {
            self.worker_joined
                .store(worker.join().is_ok(), Ordering::Release);
        }
    }
}

#[test]
fn long_auto_popup_runtime_pause_superseded_by_auto_stop_keeps_real_capture_paused_then_resumes() {
    let final_weak;
    let source_dropped;
    let worker_joined;
    {
        let mut rig = Rig::new();
        source_dropped = rig.dropped.clone();
        worker_joined = rig.worker_joined.clone();
        let (sequence, region) = rig.reads.recv_timeout(DEADLINE).unwrap();
        assert_eq!((sequence, region), (1, rig.region));
        let baseline = rig
            .session
            .shared
            .latest
            .lock()
            .unwrap()
            .snapshot
            .clone()
            .unwrap();
        rig.replies.send(world(64, 128, 16)).unwrap();
        let old_weak = rig.returned.recv_timeout(DEADLINE).unwrap();
        let old_stamp = rig.entered.recv_timeout(DEADLINE).unwrap();
        assert_eq!((old_stamp.epoch, old_stamp.sequence), (INITIAL_EPOCH, 1));
        wait(|| rig.count.load(Ordering::Acquire) >= 2);

        // consumer 被真实旧帧的 MatchHook 持住：旧 Pause 尚未 apply 就被 Auto(None) 取代。
        rig.session.set_paused(true).unwrap();
        assert_eq!(
            rig.session.snapshot().requested_generation,
            INITIAL_EPOCH + 1
        );
        assert_eq!(rig.session.snapshot().phase, LongPhase::Pausing);
        rig.session.set_auto_scroll(None).unwrap();
        let paused_generation = INITIAL_EPOCH + 2;
        assert_eq!(
            rig.session.snapshot().requested_generation,
            paused_generation
        );
        rig.latch.release();
        wait(|| rig.quiet(paused_generation));
        assert!(
            rig.releases.load(Ordering::Acquire) > 0,
            "没有实际 source.release ACK"
        );
        assert!(old_weak.upgrade().is_none(), "取消的旧帧像素仍被保留");
        let paused = rig.session.stitch_snapshot().unwrap();
        assert_eq!((paused.width, paused.height), (64, 128));
        assert!(
            Arc::ptr_eq(
                rig.session
                    .shared
                    .latest
                    .lock()
                    .unwrap()
                    .snapshot
                    .as_ref()
                    .unwrap(),
                &baseline
            ),
            "旧在途帧越过 Pause 代数栅栏提交"
        );
        let paused_reads = rig.count.load(Ordering::Acquire);
        let paused_issued = rig.session.pipeline_probe().last_issued_sequence;
        for (old_sequence, old_region) in rig.reads.try_iter() {
            assert!(old_sequence <= paused_reads);
            assert_eq!(old_region, rig.region);
        }
        let mut posts = 0;
        let fence = rig
            .session
            .shared
            .pipeline
            .as_ref()
            .unwrap()
            .with_post_fence(paused_generation, || {
                posts += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(fence, None, "真正 Paused 时仍允许发输入");
        assert_eq!(posts, 0, "Paused 的输入闭包被执行");
        assert_eq!(
            rig.session.pipeline_probe().last_issued_sequence,
            paused_issued
        );
        assert_eq!(rig.count.load(Ordering::Acquire), paused_reads);
        assert!(rig.session.pipeline_probe().auto_events.is_empty());

        // 真实 public manual Resume 发布新的 Running permit，不能只有 meta flag 变化。
        rig.session.set_paused(false).unwrap();
        let resumed_generation = paused_generation + 1;
        let (new_sequence, new_region) = rig.reads.recv_timeout(DEADLINE).unwrap();
        assert_eq!(new_sequence, paused_reads + 1);
        assert_eq!(new_region, rig.region);
        rig.replies.send(world(64, 128, 16)).unwrap();
        final_weak = rig.returned.recv_timeout(DEADLINE).unwrap();
        wait(|| {
            let meta = rig.session.snapshot();
            meta.generation == resumed_generation
                && meta.requested_generation == resumed_generation
                && meta.phase == LongPhase::Running
                && !meta.auto.active
                && meta.height == 144
                && meta.accepted_frames == 2
        });
        let result = rig
            .session
            .shared
            .latest
            .lock()
            .unwrap()
            .snapshot
            .clone()
            .unwrap();
        let actual =
            crate::long_document::render_region(&result, Rect::full(64, 144), 64, 144).unwrap();
        assert_eq!(
            actual.bgra,
            world(64, 144, 0).bgra,
            "Resume 的实际采集/匹配结果与独立世界 oracle 不同"
        );
        wait(|| rig.count.load(Ordering::Acquire) > new_sequence);
        assert!(rig.session.pipeline_probe().capture_in_flight);
        assert!(!rig.session.snapshot().auto.active);
    }
    assert!(
        source_dropped.load(Ordering::Acquire),
        "真实 producer source 未析构"
    );
    assert!(
        worker_joined.load(Ordering::Acquire),
        "真实 consumer 未正常 join"
    );
    assert!(
        final_weak.upgrade().is_none(),
        "会话关闭后最后 read 帧仍被持有"
    );
}
