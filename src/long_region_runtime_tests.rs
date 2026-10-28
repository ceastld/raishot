//! 真实 producer 与 runtime 提交门的 ROI 事务；不创建窗口、不读取桌面。
use super::*;
use crate::long_capture_pipeline::CaptureSource;

const LIMIT: Duration = Duration::from_secs(10);
enum Reply {
    Image(Image),
    Error,
}
struct Source {
    replies: Receiver<Reply>,
    reads: Sender<ScreenRect>,
    releases: Arc<AtomicUsize>,
}
impl CaptureSource for Source {
    fn read(&mut self, region: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>> {
        self.reads.send(region)?;
        loop {
            if cancelled() {
                return Err("受控 source 已取消".into());
            }
            match self.replies.recv_timeout(Duration::from_millis(2)) {
                Ok(Reply::Image(image)) => return Ok(Arc::new(image)),
                Ok(Reply::Error) => return Err("受控 ROI 首帧错误".into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("source 通道结束".into()),
            }
        }
    }
    fn release(&mut self) {
        self.releases.fetch_add(1, Ordering::AcqRel);
    }
}
fn old_region() -> ScreenRect {
    ScreenRect {
        left: -48,
        top: 20,
        width: 64,
        height: 128,
    }
}
fn pixels(width: u32, height: u32, row: u32) -> Image {
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in row..row + height {
        let mut h = y.wrapping_mul(2_654_435_761);
        h ^= h >> 16;
        for x in 0..width {
            bgra.extend_from_slice(&[
                h.wrapping_add(x * 17) as u8,
                ((h >> 8).wrapping_add(x * 5)) as u8,
                ((h >> 16).wrapping_add(x * 11)) as u8,
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
    session: LongSession,
    worker: Worker,
    commands: Receiver<Command>,
    replies: Sender<Reply>,
    reads: Receiver<ScreenRect>,
    releases: Arc<AtomicUsize>,
    pipeline: CapturePipeline,
}
impl Rig {
    fn new() -> Self {
        let region = old_region();
        let (replies, receive) = mpsc::channel();
        let (read, reads) = mpsc::channel();
        let releases = Arc::new(AtomicUsize::new(0));
        let source = Source {
            replies: receive,
            reads: read,
            releases: releases.clone(),
        };
        let pipeline =
            CapturePipeline::start_with_source(region, Duration::ZERO, None, Box::new(source))
                .unwrap();
        pipeline.configure(0, ScrollAxis::Vertical, Mode::Paused);
        wait(|| pipeline.probe().producer_quiesced);
        let options = LongOptions {
            stitch: StitchOptions {
                detect_fixed_borders: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: Some(CommittedContext {
                    capture: pipeline.context(),
                    options: options.clone(),
                    target: Err("旧受控目标".into()),
                }),
                pending_region: None,
            }),
            requested_generation: AtomicU64::new(0),
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
        worker.stitch = Some(
            Stitcher::with_options_shared(
                Arc::new(pixels(region.width, region.height, 0)),
                options.stitch,
            )
            .unwrap(),
        );
        worker.initialize = false;
        worker.paused = true;
        worker.meta.phase = LongPhase::Paused;
        worker.meta.busy = false;
        worker.meta.accepted_frames = 1;
        worker.update_output();
        worker.publish(&shared, true);
        let (commands, receiver) = mpsc::sync_channel(8);
        let (cleanup, _) = mpsc::channel();
        let session = LongSession {
            shared,
            commands,
            worker: None,
            cleanup,
            gate_factory: None,
            fixed_region_gate: false,
        };
        Self {
            session,
            worker,
            commands: receiver,
            replies,
            reads,
            releases,
            pipeline,
        }
    }
    fn request(&mut self, region: ScreenRect, kind: RegionChange) {
        self.session
            .change_region_with_target(region, kind, |_| Err("新受控目标".into()))
            .unwrap();
    }
    fn apply(&mut self) {
        let command = self.commands.recv_timeout(LIMIT).unwrap();
        self.worker.apply(command, &self.session.shared);
    }
    fn send(&self, region: ScreenRect, image: Image) {
        assert_eq!(self.reads.recv_timeout(LIMIT).unwrap(), region);
        self.replies.send(Reply::Image(image)).unwrap();
        wait(|| self.pipeline.probe().queue_depth == 1);
    }
    fn collect(&mut self) {
        let frame = self
            .pipeline
            .try_take_for_epoch(self.worker.meta.generation)
            .unwrap();
        self.worker.collect(frame, &self.session.shared);
    }
    fn ack(&mut self) {
        wait(|| {
            let p = self.pipeline.probe();
            p.producer_quiesced && !p.capture_in_flight && p.reserved_raw_bytes == 0
        });
        self.worker.meta.busy = false;
        self.worker.publish(&self.session.shared, true);
        assert!(!self.session.snapshot().busy);
    }
    fn snapshot(&self) -> Arc<StitchSnapshot> {
        self.session
            .shared
            .latest
            .lock()
            .unwrap()
            .snapshot
            .clone()
            .unwrap()
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.pipeline.stop();
        self.pipeline.join();
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + LIMIT;
    while !predicate() {
        assert!(Instant::now() < until, "真实管线条件超时");
        thread::yield_now();
    }
}
fn exported(snapshot: &StitchSnapshot) -> Image {
    crate::long_document::LongDocument::new(snapshot.clone())
        .unwrap()
        .export()
        .unwrap()
}

#[test]
fn long_region_reset_commits_single_pixels_and_context_together() {
    let mut rig = Rig::new();
    let old = rig.snapshot();
    let old_context = rig
        .session
        .shared
        .latest
        .lock()
        .unwrap()
        .context
        .clone()
        .unwrap();
    let region = ScreenRect {
        left: -32,
        top: 40,
        width: 48,
        height: 160,
    };
    let image = pixels(region.width, region.height, 600);
    let expected = image.bgra.clone();
    rig.request(region, RegionChange::Reset);
    assert_eq!(rig.session.snapshot().region, old_region());
    assert_eq!(rig.session.snapshot().pending_region, Some(region));
    rig.send(region, image);
    assert!(
        rig.pipeline.try_take_for_epoch(0).is_none(),
        "新Single必须留队等待apply"
    );
    rig.apply();
    rig.collect();
    rig.ack();
    let meta = rig.session.snapshot();
    assert_eq!(
        (meta.region, meta.pending_region, meta.phase),
        (region, None, LongPhase::Paused)
    );
    assert!(!meta.auto.active);
    let snapshot = rig.session.stitch_snapshot().unwrap();
    assert_eq!(exported(&snapshot).bgra, expected);
    assert!(!Arc::ptr_eq(&old, &rig.snapshot()));
    let p = rig.pipeline.probe();
    assert_eq!(
        p.raw_budget_bytes,
        u64::from(region.width) * u64::from(region.height) * 4 * p.max_raw_slots as u64
    );
    let context = rig
        .session
        .shared
        .latest
        .lock()
        .unwrap()
        .context
        .clone()
        .unwrap();
    assert_eq!(context.capture.region, region);
    assert!(context
        .target
        .as_ref()
        .is_err_and(|reason| reason == "新受控目标"));
    assert!(
        rig.session
            .command(Operation::CancelRegion(old_context))
            .is_err(),
        "已提交后迟到取消不能恢复错图"
    );
    assert_eq!(rig.session.snapshot().region, region);
    assert!(rig.releases.load(Ordering::Acquire) > 0);
}

#[test]
fn long_region_continue_requires_reliable_motion_or_entire_equal_frame() {
    for case in ["advance", "equal", "changed-zero", "unmatched"] {
        let mut rig = Rig::new();
        let old = rig.snapshot();
        let region = ScreenRect {
            top: old_region().top + 12,
            ..old_region()
        };
        let mut image = pixels(
            region.width,
            region.height,
            if case == "advance" {
                12
            } else if case == "unmatched" {
                2000
            } else {
                0
            },
        );
        if case == "changed-zero" {
            image.bgra[0] ^= 16;
            let mut baseline = rig.worker.stitch.as_ref().unwrap().clone();
            let probe_image = Image {
                width: image.width,
                height: image.height,
                bgra: image.bgra.clone(),
                readback_ms: 0.,
            };
            assert_eq!(
                baseline.process_shared(Arc::new(probe_image)).unwrap().kind,
                UpdateKind::Unchanged
            );
        }
        rig.request(region, RegionChange::Continue);
        rig.apply();
        rig.send(region, image);
        rig.collect();
        rig.ack();
        let meta = rig.session.snapshot();
        if case == "advance" || case == "equal" {
            assert_eq!(meta.region, region, "{case}");
            assert_eq!(meta.height, if case == "advance" { 140 } else { 128 });
        } else {
            assert_eq!(meta.region, old_region(), "{case}");
            assert!(meta.requested_generation >= 2 && meta.status.contains("调整选区失败"));
            assert!(
                Arc::ptr_eq(&old, &rig.snapshot()),
                "失败不得换旧snapshot Arc：{case}"
            );
        }
        assert_eq!(meta.pending_region, None);
        assert_eq!(meta.phase, LongPhase::Paused);
    }
}

#[test]
fn long_region_cancel_rejects_copied_candidate_and_actual_commit() {
    let mut rig = Rig::new();
    let old = rig.snapshot();
    let region = ScreenRect {
        left: -40,
        top: 40,
        ..old_region()
    };
    rig.request(region, RegionChange::Reset);
    rig.apply();
    rig.send(region, pixels(region.width, region.height, 800));
    let frame = rig
        .pipeline
        .try_take_for_epoch(rig.worker.meta.generation)
        .unwrap();
    let staged =
        Stitcher::with_options_shared(frame.image.clone(), rig.worker.options.stitch.clone())
            .unwrap();
    let staged_output = Arc::new(staged.snapshot());
    let candidate_epoch = rig.worker.meta.generation;
    rig.session.cancel_region_change().unwrap();
    assert!(!rig
        .worker
        .commit(&rig.session.shared, staged, Some(staged_output), Some(1)));
    assert!(!rig.pipeline.accepts(&frame.stamp));
    drop(frame);
    rig.apply();
    rig.ack();
    assert!(rig.session.snapshot().requested_generation > candidate_epoch);
    assert_eq!(rig.session.snapshot().region, old_region());
    assert!(Arc::ptr_eq(&old, &rig.snapshot()));
    assert_eq!(
        exported(&rig.session.stitch_snapshot().unwrap()).bgra,
        exported(&old).bgra
    );
}

#[test]
fn long_region_source_error_before_apply_is_not_lost_and_restores_old_arc() {
    let mut rig = Rig::new();
    let old = rig.snapshot();
    let region = ScreenRect {
        top: 40,
        ..old_region()
    };
    rig.request(region, RegionChange::Reset);
    assert_eq!(rig.reads.recv_timeout(LIMIT).unwrap(), region);
    rig.replies.send(Reply::Error).unwrap();
    wait(|| rig.pipeline.probe().error.is_some());
    assert!(
        rig.pipeline.take_error_for_epoch(0).is_none(),
        "未apply的新代错误必须留给正确消费者"
    );
    rig.apply();
    let error = rig
        .pipeline
        .take_error_for_epoch(rig.worker.meta.generation)
        .unwrap();
    rig.worker.fail(error, &rig.session.shared);
    rig.ack();
    let meta = rig.session.snapshot();
    assert_eq!(
        (meta.region, meta.pending_region, meta.phase),
        (old_region(), None, LongPhase::Paused)
    );
    assert!(meta.status.contains("受控 ROI 首帧错误"));
    assert!(Arc::ptr_eq(&old, &rig.snapshot()));
}

#[test]
fn long_region_running_fixed_gate_and_cross_axis_requests_do_not_issue_permits() {
    let mut rig = Rig::new();
    let along = ScreenRect {
        top: 40,
        ..old_region()
    };
    rig.session.fixed_region_gate = true;
    assert!(rig
        .session
        .change_region_with_target(along, RegionChange::Reset, |_| Err("不可调用".into()))
        .is_err());
    rig.session.fixed_region_gate = false;
    assert!(rig
        .session
        .change_region_with_target(
            ScreenRect {
                left: -32,
                ..old_region()
            },
            RegionChange::Continue,
            |_| Err("不可调用".into())
        )
        .is_err());
    rig.worker.meta.phase = LongPhase::Running;
    rig.worker.publish(&rig.session.shared, false);
    assert!(rig
        .session
        .change_region_with_target(along, RegionChange::Reset, |_| Err("受控目标".into()))
        .is_err());
    assert_eq!(rig.pipeline.probe().last_issued_sequence, 0);
    assert_eq!(rig.session.snapshot().region, old_region());
}
