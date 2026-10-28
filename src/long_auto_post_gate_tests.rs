//! 发送前原采集门的精确分支；真实 producer/permit/fence，不建立 HWND 或发送输入。
use super::*;
use crate::long_capture_pipeline::CaptureSource;
use std::sync::Weak;

const EPOCH: u64 = 11;
const DEADLINE: Duration = Duration::from_secs(10);

struct Source {
    entered: Sender<u64>,
    replies: Receiver<Image>,
    returned: Sender<Weak<Image>>,
    sequence: u64,
}
impl CaptureSource for Source {
    fn read(&mut self, _: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>> {
        self.sequence += 1;
        self.entered.send(self.sequence)?;
        loop {
            if cancelled() {
                return Err("受控 source 已取消".into());
            }
            match self.replies.recv_timeout(Duration::from_millis(2)) {
                Ok(image) => {
                    let image = Arc::new(image);
                    self.returned.send(Arc::downgrade(&image))?;
                    return Ok(image);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("source 通道已关闭".into()),
            }
        }
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < until, "受控 pipeline 未达到条件");
        thread::yield_now();
    }
}
fn world(first: u32, height: u32) -> Image {
    let mut bgra = Vec::with_capacity(64 * height as usize * 4);
    for y in first..first + height {
        let mut row = y.wrapping_mul(2_654_435_761);
        row ^= row >> 16;
        for x in 0_u32..64 {
            bgra.extend_from_slice(&[
                row.wrapping_add(x * 17) as u8,
                ((row >> 8).wrapping_add(x * 5)) as u8,
                ((row >> 16).wrapping_add(x * 11)) as u8,
                255,
            ]);
        }
    }
    Image {
        width: 64,
        height,
        bgra,
        readback_ms: 0.,
    }
}
struct Rig {
    worker: Worker,
    shared: Arc<Shared>,
    pipeline: CapturePipeline,
    entered: Receiver<u64>,
    replies: Sender<Image>,
    returned: Receiver<Weak<Image>>,
    pending: Option<u64>,
    weaks: Vec<Weak<Image>>,
}
impl Rig {
    fn new(gate: FrameGate) -> Self {
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
        let (send_entered, entered) = mpsc::channel();
        let (replies, receive) = mpsc::channel();
        let (send_returned, returned) = mpsc::channel();
        let pipeline = CapturePipeline::start_with_source(
            region,
            Duration::ZERO,
            Some(gate),
            Box::new(Source {
                entered: send_entered,
                replies: receive,
                returned: send_returned,
                sequence: 0,
            }),
        )
        .unwrap();
        wait(|| pipeline.probe().producer_quiesced);
        let context = pipeline.context();
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest {
                meta: LongSnapshot::starting(region, &options),
                snapshot: None,
                preview: None,
                context: Some(CommittedContext {
                    capture: context.clone(),
                    options: options.clone(),
                    target: Err("核心使用闭包；没有原生目标".into()),
                }),
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
        worker.frame_gate = context.gate.clone();
        worker.meta.generation = EPOCH;
        worker.meta.phase = LongPhase::Running;
        worker.stitch = Some(Stitcher::with_options(world(0, 128), options.stitch).unwrap());
        worker.initialize = false;
        worker.update_output();
        pipeline.configure(EPOCH, ScrollAxis::Vertical, Mode::Running);
        worker.publish(&shared, true);
        Self {
            worker,
            shared,
            pipeline,
            entered,
            replies,
            returned,
            pending: None,
            weaks: vec![],
        }
    }
    fn entered(&mut self) -> u64 {
        *self
            .pending
            .get_or_insert_with(|| self.entered.recv_timeout(DEADLINE).unwrap())
    }
    fn collect(&mut self, first: u32) -> (u64, Instant) {
        let sequence = self.entered();
        self.pending = None;
        self.replies.send(world(first, 128)).unwrap();
        self.weaks
            .push(self.returned.recv_timeout(DEADLINE).unwrap());
        wait(|| self.pipeline.probe().queue_depth > 0);
        let frame = self.pipeline.try_take_for_epoch(EPOCH).unwrap();
        assert_eq!(frame.stamp.sequence, sequence);
        let started = frame.stamp.read_start;
        self.worker.collect(frame, &self.shared);
        (sequence, started)
    }
    fn ready_policy(&mut self) -> AutoPolicy {
        let start = Instant::now() - Duration::from_millis(100);
        let mut policy = AutoPolicy::new(AutoScrollOptions::default(), EPOCH, start);
        let (first, _) = self.collect(0);
        assert_eq!(
            policy.observe(EPOCH, first, self.worker.meta.last_result, 0, 0, start),
            Decision::Wait
        );
        let (second, _) = self.collect(0);
        assert_eq!(
            policy.observe(
                EPOCH,
                second,
                self.worker.meta.last_result,
                0,
                0,
                start + Duration::from_millis(100)
            ),
            Decision::Post
        );
        assert_eq!(self.entered(), 3);
        policy
    }
    fn assert_pixels(&self, height: u32) {
        let snapshot = self.worker.output.as_ref().unwrap();
        assert_eq!((snapshot.width, snapshot.height), (64, height));
        let actual =
            crate::long_document::render_region(snapshot, Rect::full(64, height), 64, height)
                .unwrap();
        assert_eq!(actual.bgra, world(0, height).bgra);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.pipeline.stop();
        self.pipeline.join();
    }
}

#[test]
fn long_auto_post_gate_revoked_context_refuses_input_and_preserves_committed_pixels() {
    // 同一捕获上下文门失效应 Failed；只有原生目标失效仍保持原有 Paused 合同。
    for capture_failure in [true, false] {
        let live = Arc::new(AtomicBool::new(true));
        let guard_live = live.clone();
        let gate: FrameGate = Arc::new(move || {
            if guard_live.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err("原租约已撤销".into())
            }
        });
        let weaks = {
            let mut rig = Rig::new(gate.clone());
            let mut policy = rig.ready_policy();
            let old = rig.worker.output.as_ref().unwrap().clone();
            let capture = rig.pipeline.context();
            assert!(Arc::ptr_eq(capture.gate.as_ref().unwrap(), &gate));
            let posts = AtomicUsize::new(0);
            let validations = AtomicUsize::new(0);
            if capture_failure {
                live.store(false, Ordering::Release);
            }
            assert!(!rig.worker.post_auto_guarded(
                &rig.shared,
                &mut policy,
                "受控目标",
                || {
                    validations.fetch_add(1, Ordering::AcqRel);
                    if capture_failure {
                        Ok(())
                    } else {
                        Err("原生目标已失效".into())
                    }
                },
                || {
                    posts.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                }
            ));
            assert_eq!(posts.load(Ordering::Acquire), 0);
            assert_eq!(
                validations.load(Ordering::Acquire),
                usize::from(!capture_failure)
            );
            assert_eq!(policy.snapshot(String::new()).steps, 0);
            assert!(rig.worker.auto_post_fence.is_none());
            assert!(rig
                .shared
                .auto_probe
                .lock()
                .unwrap()
                .last_post_fence
                .is_none());
            assert!(!rig.worker.meta.auto.active && rig.worker.paused);
            assert_eq!(
                rig.worker.meta.phase,
                if capture_failure {
                    LongPhase::Failed
                } else {
                    LongPhase::Paused
                }
            );
            assert_eq!(rig.worker.meta.auto.phase, AutoPhase::Blocked);
            assert!(Arc::ptr_eq(rig.worker.output.as_ref().unwrap(), &old));
            assert!(Arc::ptr_eq(
                rig.shared.latest.lock().unwrap().snapshot.as_ref().unwrap(),
                &old
            ));
            assert_eq!(
                rig.shared
                    .auto_probe
                    .lock()
                    .unwrap()
                    .events
                    .iter()
                    .filter(|e| e.event == "capture-gate-rejected")
                    .count(),
                usize::from(capture_failure)
            );
            rig.assert_pixels(128);
            if !capture_failure {
                // 原生目标拒绝只暂停同一代，已 issued 的 read 仍需正常完成。
                // 受控 Source 没有 DXGI 的自然返回，必须显式归还这次在途读回。
                assert_eq!(rig.entered(), 3);
                rig.pending = None;
                rig.replies.send(world(0, 128)).unwrap();
                rig.weaks.push(rig.returned.recv_timeout(DEADLINE).unwrap());
                wait(|| rig.pipeline.probe().queue_depth == 1);
                let late = rig.pipeline.try_take_for_epoch(EPOCH).unwrap();
                assert_eq!(late.stamp.sequence, 3);
                drop(late);
            }
            wait(|| rig.pipeline.probe().producer_quiesced);
            assert_eq!(rig.pipeline.probe().reserved_raw_bytes, 0);
            std::mem::take(&mut rig.weaks)
        };
        assert!(weaks.iter().all(|image| image.upgrade().is_none()));
    }
}

#[test]
fn long_auto_post_gate_blocked_check_cannot_send_or_fail_a_new_control_generation() {
    for control in ["pause", "drain", "context"] {
        let posting_thread = thread::current().id();
        let blocked = Arc::new(AtomicBool::new(false));
        let guard_blocked = blocked.clone();
        let (entered, entered_gate) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let gate: FrameGate = Arc::new(move || {
            if guard_blocked.load(Ordering::Acquire) && thread::current().id() == posting_thread {
                entered.send(())?;
                released.lock().unwrap().recv_timeout(DEADLINE)?;
                return Err("迟到的旧门失效".into());
            }
            Ok(())
        });
        let weaks = {
            let mut rig = Rig::new(gate);
            let mut policy = rig.ready_policy();
            let old = rig.worker.output.as_ref().unwrap().clone();
            let shared = rig.shared.clone();
            let pipeline = rig.pipeline.clone();
            let publisher = thread::spawn(move || {
                entered_gate.recv_timeout(DEADLINE).unwrap();
                // 与真实 UI 同样 latest→pipeline；若发送校验持短锁，此处不能在释放门前完成。
                let mut latest = shared.latest.lock().unwrap();
                if control == "drain" {
                    pipeline.begin_drain(EPOCH + 1);
                    latest.meta.phase = LongPhase::Pausing;
                    latest.meta.busy = true;
                } else if control == "context" {
                    let previous = latest.context.as_ref().unwrap().clone();
                    let region = ScreenRect {
                        left: -96,
                        ..previous.capture.region
                    };
                    let capture =
                        CaptureConfig::new(region, previous.capture.gate.clone()).unwrap();
                    let retired = pipeline
                        .restore_context_if_current(
                            EPOCH,
                            EPOCH + 1,
                            capture.clone(),
                            ScrollAxis::Vertical,
                        )
                        .unwrap();
                    latest.context = Some(CommittedContext {
                        capture,
                        ..previous
                    });
                    latest.meta.region = region;
                    latest.meta.phase = LongPhase::Paused;
                    latest.meta.busy = false;
                    drop(retired);
                } else {
                    pipeline.configure(EPOCH + 1, ScrollAxis::Vertical, Mode::Paused);
                    latest.meta.phase = LongPhase::Paused;
                    latest.meta.busy = false;
                }
                latest.meta.generation = EPOCH + 1;
                latest.meta.requested_generation = EPOCH + 1;
                latest.meta.status = format!("新控制：{control}");
                shared
                    .requested_generation
                    .store(EPOCH + 1, Ordering::Release);
                drop(latest);
                release.send(()).unwrap();
            });
            blocked.store(true, Ordering::Release);
            let posts = AtomicUsize::new(0);
            assert!(!rig.worker.post_auto_guarded(
                &rig.shared,
                &mut policy,
                "受控目标",
                || Ok(()),
                || {
                    posts.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                }
            ));
            publisher.join().unwrap();
            assert_eq!(posts.load(Ordering::Acquire), 0);
            assert_eq!(policy.snapshot(String::new()).steps, 0);
            assert!(rig.worker.auto_post_fence.is_none());
            assert!(rig.shared.auto_probe.lock().unwrap().events.is_empty());
            let latest = rig.shared.latest.lock().unwrap();
            assert_eq!(latest.meta.generation, EPOCH + 1);
            assert_eq!(latest.meta.status, format!("新控制：{control}"));
            assert_eq!(
                latest.meta.phase,
                if control == "drain" {
                    LongPhase::Pausing
                } else {
                    LongPhase::Paused
                }
            );
            assert!(Arc::ptr_eq(latest.snapshot.as_ref().unwrap(), &old));
            drop(latest);
            rig.assert_pixels(128);
            std::mem::take(&mut rig.weaks)
        };
        assert!(weaks.iter().all(|image| image.upgrade().is_none()));
    }
}

#[test]
fn long_auto_post_gate_success_keeps_the_real_permit_fence_and_fresh_position_ack() {
    let checks = Arc::new(AtomicUsize::new(0));
    let guard_checks = checks.clone();
    let gate: FrameGate = Arc::new(move || {
        guard_checks.fetch_add(1, Ordering::AcqRel);
        Ok(())
    });
    let weaks = {
        let mut rig = Rig::new(gate);
        let mut policy = rig.ready_policy();
        let before = checks.load(Ordering::Acquire);
        let posts = AtomicUsize::new(0);
        let mut posted_at = Instant::now();
        assert!(rig.worker.post_auto_guarded(
            &rig.shared,
            &mut policy,
            "受控目标",
            || Ok(()),
            || {
                posted_at = Instant::now();
                posts.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
        ));
        assert!(checks.load(Ordering::Acquire) > before);
        assert_eq!(posts.load(Ordering::Acquire), 1);
        assert_eq!(policy.snapshot(String::new()).steps, 1);
        assert_eq!(rig.worker.auto_post_fence, Some(3));
        let probe = rig.shared.auto_probe.lock().unwrap();
        assert_eq!(probe.last_post_fence, Some(3));
        assert_eq!(probe.events.len(), 1);
        assert_eq!(probe.events[0].event, "post");
        assert_eq!(probe.events[0].fence_sequence, Some(3));
        drop(probe);
        let (old, started) = rig.collect(16);
        assert_eq!(old, 3);
        assert!(started <= posted_at);
        assert_eq!(
            (
                rig.worker.meta.last_result,
                rig.worker.trusted_scroll_position
            ),
            (UpdateKind::Added, 16)
        );
        rig.assert_pixels(144);
        assert_eq!(
            policy.observe(
                EPOCH,
                old,
                UpdateKind::Added,
                16,
                16,
                posted_at + Duration::from_millis(50)
            ),
            Decision::Wait
        );
        let (fresh, started) = rig.collect(16);
        assert_eq!(fresh, 4);
        assert!(started >= posted_at);
        assert_eq!(rig.worker.meta.last_result, UpdateKind::Unchanged);
        assert_eq!(
            policy.observe(
                EPOCH,
                fresh,
                UpdateKind::Unchanged,
                0,
                16,
                posted_at + Duration::from_millis(100)
            ),
            Decision::Wait
        );
        let (settled, _) = rig.collect(16);
        assert_eq!(settled, 5);
        assert_eq!(
            policy.observe(
                EPOCH,
                settled,
                UpdateKind::Unchanged,
                0,
                16,
                posted_at + Duration::from_millis(130)
            ),
            Decision::Post
        );
        // 返回可发送意图还不算实际投递；累计/fence 必须等待下一次真实 guarded post。
        assert_eq!(policy.snapshot(String::new()).steps, 1);
        assert_eq!(rig.worker.auto_post_fence, Some(3));
        rig.assert_pixels(144);
        std::mem::take(&mut rig.weaks)
    };
    assert!(weaks.iter().all(|image| image.upgrade().is_none()));
}
