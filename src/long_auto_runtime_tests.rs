//! Auto 反馈停驱动后仍能手动拼接；实际 producer/stamp/post fence，无原生窗口或输入。
use super::*;
use crate::long_capture_pipeline::CaptureSource;
use std::sync::Weak;

const DEADLINE: Duration = Duration::from_secs(10);
const EPOCH: u64 = 7;

struct Source {
    reads: Sender<(u64, ScreenRect)>,
    replies: Receiver<Image>,
    returned: Sender<Weak<Image>>,
    read_count: Arc<AtomicU64>,
}
impl CaptureSource for Source {
    fn read(&mut self, region: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>> {
        let sequence = self.read_count.fetch_add(1, Ordering::AcqRel) + 1;
        self.reads.send((sequence, region))?;
        loop {
            if cancelled() {
                return Err("受控 Auto source 已取消".into());
            }
            match self.replies.recv_timeout(Duration::from_millis(2)) {
                Ok(image) => {
                    let image = Arc::new(image);
                    self.returned.send(Arc::downgrade(&image))?;
                    return Ok(image);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("source 通道关闭".into()),
            }
        }
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + DEADLINE;
    while !predicate() {
        assert!(Instant::now() < until, "受控真实管线条件未到达");
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
struct Rig {
    shared: Arc<Shared>,
    worker: Worker,
    pipeline: CapturePipeline,
    reads: Receiver<(u64, ScreenRect)>,
    replies: Sender<Image>,
    returned: Receiver<Weak<Image>>,
    read_count: Arc<AtomicU64>,
    pending_read: Option<u64>,
    weaks: Vec<Weak<Image>>,
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
        let read_count = Arc::new(AtomicU64::new(0));
        let pipeline = CapturePipeline::start_with_source(
            region,
            Duration::ZERO,
            None,
            Box::new(Source {
                reads: read,
                replies: receive,
                returned,
                read_count: read_count.clone(),
            }),
        )
        .unwrap();
        wait(|| pipeline.probe().producer_quiesced);
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: None,
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(EPOCH),
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
        });
        let mut worker = Worker::new(region, options.clone());
        worker.pipeline = Some(pipeline.clone());
        worker.meta.generation = EPOCH;
        worker.meta.phase = LongPhase::Running;
        worker.meta.accepted_frames = 1;
        worker.stitch = Some(
            Stitcher::with_options(world(region.width, region.height, 0), options.stitch).unwrap(),
        );
        worker.initialize = false;
        worker.update_output();
        pipeline.configure(EPOCH, ScrollAxis::Vertical, Mode::Running);
        worker.publish(&shared, true);
        Self {
            shared,
            worker,
            pipeline,
            reads,
            replies,
            returned: receive_returned,
            read_count,
            pending_read: None,
            weaks: vec![],
        }
    }
    fn entered(&mut self) -> u64 {
        if let Some(sequence) = self.pending_read {
            return sequence;
        }
        let (sequence, region) = self
            .reads
            .recv_timeout(DEADLINE)
            .expect("真实 producer 未进入下一次 read");
        assert_eq!(region, self.worker.meta.region);
        self.pending_read = Some(sequence);
        sequence
    }
    fn collect(&mut self, first_row: u32) -> (u64, Instant) {
        let sequence = self.entered();
        self.pending_read = None;
        let region = self.worker.meta.region;
        self.replies
            .send(world(region.width, region.height, first_row))
            .unwrap();
        self.weaks
            .push(self.returned.recv_timeout(DEADLINE).unwrap());
        wait(|| self.pipeline.probe().queue_depth > 0);
        let frame = self
            .pipeline
            .try_take_for_epoch(EPOCH)
            .expect("已返回的当前代帧丢失");
        assert_eq!(frame.stamp.sequence, sequence);
        let started = frame.stamp.read_start;
        self.worker.collect(frame, &self.shared);
        (sequence, started)
    }
    fn verify_pixels(&self, height: u32) {
        let snapshot = self.worker.output.as_ref().unwrap();
        assert_eq!((snapshot.width, snapshot.height), (64, height));
        let actual =
            crate::long_document::render_region(snapshot, Rect::full(64, height), 64, height)
                .unwrap();
        assert_eq!(actual.bgra, world(64, height, 0).bgra);
    }
    fn assert_running(&self) {
        assert!(!self.worker.paused && !self.worker.meta.auto.active);
        assert_eq!(self.worker.meta.phase, LongPhase::Running);
        assert!(self.pipeline.accepts_epoch(EPOCH, EPOCH));
        assert!(self.pipeline.probe().capture_in_flight);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.pipeline.stop();
        self.pipeline.join();
    }
}

#[test]
fn long_auto_runtime_feedback_stop_keeps_actual_manual_producer_and_pixels() {
    for (phase, coalescing) in [
        (AutoPhase::NoMotion, false),
        (AutoPhase::NoMotion, true),
        (AutoPhase::Unstable, false),
        (AutoPhase::Unstable, true),
        (AutoPhase::Blocked, false),
        (AutoPhase::Blocked, true),
    ] {
        let weaks = {
            let mut rig = Rig::new();
            assert_eq!(rig.entered(), 1);
            let old = rig.worker.output.as_ref().unwrap().clone();
            rig.worker.meta.auto.active = true;
            rig.worker.meta.auto.steps = 4;
            rig.worker.auto_post_fence = Some(9);
            rig.shared
                .allow_coalescing
                .store(coalescing, Ordering::Release);
            rig.pipeline.set_coalescing(false);
            // 真实 read 已发起但尚未返回：反馈停 Auto 不能撤销这个手动采集 permit。
            rig.worker
                .stop_auto_feedback(phase, "受控反馈停止", &rig.shared);
            rig.assert_running();
            assert_eq!(rig.pipeline.probe().allow_coalescing, coalescing);
            assert_eq!(rig.worker.meta.auto.phase, phase);
            assert_eq!(rig.worker.meta.auto.steps, 4);
            assert!(rig.worker.auto_post_fence.is_none());
            assert!(Arc::ptr_eq(rig.worker.output.as_ref().unwrap(), &old));
            rig.worker.publish(&rig.shared, true);
            assert!(Arc::ptr_eq(
                rig.shared.latest.lock().unwrap().snapshot.as_ref().unwrap(),
                &old
            ));
            rig.collect(16);
            assert_eq!(rig.worker.meta.last_result, UpdateKind::Added);
            assert_eq!(rig.worker.meta.last_delta, 16);
            rig.verify_pixels(144);
            assert_eq!(rig.entered(), 2);
            rig.assert_running();
            std::mem::take(&mut rig.weaks)
        };
        assert!(
            weaks.iter().all(|image| image.upgrade().is_none()),
            "退出后受控源仍被持有"
        );
    }
}

#[test]
fn long_auto_runtime_idle_and_real_post_fences_do_not_blindly_issue_or_ack_old_reads() {
    let weaks = {
        let mut rig = Rig::new();
        let start = Instant::now();
        let mut policy = AutoPolicy::new(AutoScrollOptions::default(), EPOCH, start);
        let (first, _) = rig.collect(0);
        assert_eq!(
            policy.observe(
                EPOCH,
                first,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start
            ),
            Decision::Wait
        );
        let (second, _) = rig.collect(0);
        assert_eq!(
            policy.observe(
                EPOCH,
                second,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(100)
            ),
            Decision::Post
        );
        assert_eq!(rig.entered(), 3);
        let posts = AtomicUsize::new(0);
        let mut first_post = start;
        let fence = rig
            .pipeline
            .with_post_fence(EPOCH, || {
                first_post = Instant::now();
                posts.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(fence, 3);
        policy.sent(fence, start + Duration::from_millis(100));
        // 这个 read 在真实 post 前已发起。即使其图像可信并实际增加16px，也不能确认本次半格响应。
        let (old, old_started) = rig.collect(16);
        assert!(old_started <= first_post && old <= fence);
        assert_eq!(rig.worker.meta.last_result, UpdateKind::Added);
        assert_eq!(rig.worker.meta.last_delta, 16);
        assert_eq!(rig.worker.trusted_scroll_position, 16);
        rig.verify_pixels(144);
        assert_eq!(
            policy.observe(
                EPOCH,
                old,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(150)
            ),
            Decision::Wait
        );
        let (fresh, fresh_started) = rig.collect(16);
        assert!(fresh > fence && fresh_started >= first_post);
        assert_eq!(rig.worker.meta.last_result, UpdateKind::Unchanged);
        // 首个新permit确认同一可信位置，重新等待30ms静置；不能因新帧delta0漏掉旧read发现的位置。
        assert_eq!(
            policy.observe(
                EPOCH,
                fresh,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(200)
            ),
            Decision::Wait
        );
        assert_eq!(rig.entered(), 5);
        let (settling, _) = rig.collect(16);
        assert_eq!(
            policy.observe(
                EPOCH,
                settling,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(229)
            ),
            Decision::Wait
        );
        let (settled, _) = rig.collect(16);
        assert_eq!(
            policy.observe(
                EPOCH,
                settled,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(230)
            ),
            Decision::Post
        );
        assert_eq!(rig.entered(), 7);
        let fence = rig
            .pipeline
            .with_post_fence(EPOCH, || {
                posts.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(fence, 7);
        policy.sent(fence, start + Duration::from_millis(230));
        let (old, _) = rig.collect(16);
        assert_eq!(
            policy.observe(
                EPOCH,
                old,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(280)
            ),
            Decision::Wait
        );
        let (fresh, _) = rig.collect(16);
        assert_eq!(
            policy.observe(
                EPOCH,
                fresh,
                rig.worker.meta.last_result,
                rig.worker.meta.last_delta,
                rig.worker.trusted_scroll_position,
                start + Duration::from_millis(330)
            ),
            Decision::Post
        );
        assert_eq!(rig.entered(), 9);
        let fence = rig
            .pipeline
            .with_post_fence(EPOCH, || {
                posts.fetch_add(1, Ordering::AcqRel);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(fence, 9);
        policy.sent(fence, start + Duration::from_millis(330));
        let before = rig.pipeline.probe();
        assert_eq!(
            policy.idle(EPOCH, start + Duration::from_millis(379)),
            Decision::Wait
        );
        assert_eq!(
            policy.idle(EPOCH, start + Duration::from_millis(2829)),
            Decision::Wait
        );
        let decision = policy.idle(EPOCH, start + Duration::from_millis(2830));
        let Decision::Stop {
            phase: AutoPhase::NoMotion,
            reason,
        } = decision
        else {
            panic!("真正两半格无位移应收束：{decision:?}");
        };
        let after = rig.pipeline.probe();
        assert_eq!(posts.load(Ordering::Acquire), 3);
        assert_eq!(policy.snapshot(String::new()).steps, 3);
        assert_eq!(rig.read_count.load(Ordering::Acquire), 9);
        assert_eq!(after.last_issued_sequence, before.last_issued_sequence);
        assert_eq!(after.reserved_raw_bytes, before.reserved_raw_bytes);
        assert!(after.capture_in_flight && after.queue_depth == 0);
        rig.worker.meta.auto = policy.snapshot("受控非原生post".into());
        rig.worker.auto_post_fence = Some(fence);
        rig.worker
            .stop_auto_feedback(AutoPhase::NoMotion, reason, &rig.shared);
        rig.assert_running();
        rig.collect(32);
        rig.verify_pixels(160);
        assert_eq!(rig.entered(), 10);
        rig.assert_running();
        std::mem::take(&mut rig.weaks)
    };
    assert!(weaks.iter().all(|image| image.upgrade().is_none()));
}
