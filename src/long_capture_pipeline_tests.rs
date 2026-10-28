//! 测试真实 producer 与生产控制门；通道/latch 决定顺序，超时只用于失败退出。
use super::*;
use crate::{
    capture::Image,
    geometry::ScreenRect,
    long_auto::{AutoPolicy, AutoScrollOptions, Decision},
    stitch::{ScrollAxis, StitchOptions, Stitcher, UpdateKind},
};
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

enum ReadCommand {
    Image(Image),
    Error,
    Panic,
}

#[derive(Clone, Debug)]
enum SourceEvent {
    Read {
        number: usize,
        thread: thread::ThreadId,
        region: ScreenRect,
    },
    Returned {
        number: usize,
        pixels: usize,
        image: Weak<Image>,
    },
    Cancelled(usize),
    Released,
    Dropped,
}

#[derive(Default)]
struct Trace {
    events: Mutex<Vec<SourceEvent>>,
    changed: Condvar,
}
impl Trace {
    fn push(&self, event: SourceEvent) {
        self.events.lock().unwrap().push(event);
        self.changed.notify_all();
    }
    fn wait(&self, predicate: impl Fn(&[SourceEvent]) -> bool) -> Vec<SourceEvent> {
        let until = Instant::now() + TEST_TIMEOUT;
        let mut events = self.events.lock().unwrap();
        while !predicate(&events) {
            let remaining = until.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "source latch 未到达：{events:?}");
            let (next, _) = self.changed.wait_timeout(events, remaining).unwrap();
            events = next;
        }
        events.clone()
    }
    fn reads(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, SourceEvent::Read { .. }))
            .count()
    }
    fn read(&self, number: usize) -> Vec<SourceEvent> {
        self.wait(|events| events.iter().any(|event| matches!(event, SourceEvent::Read { number: actual, .. } if *actual == number)))
    }
    fn returned(&self, number: usize) -> Weak<Image> {
        let events = self.wait(|events| events.iter().any(|event| matches!(event, SourceEvent::Returned { number: actual, .. } if *actual == number)));
        events
            .into_iter()
            .find_map(|event| match event {
                SourceEvent::Returned {
                    number: actual,
                    image,
                    ..
                } if actual == number => Some(image),
                _ => None,
            })
            .unwrap()
    }
}

struct ScriptSource {
    commands: mpsc::Receiver<ReadCommand>,
    trace: Arc<Trace>,
    reads: usize,
}
impl CaptureSource for ScriptSource {
    fn read(&mut self, region: ScreenRect, cancelled: &dyn Fn() -> bool) -> Result<Arc<Image>> {
        self.reads += 1;
        self.trace.push(SourceEvent::Read {
            number: self.reads,
            thread: thread::current().id(),
            region,
        });
        loop {
            if cancelled() {
                self.trace.push(SourceEvent::Cancelled(self.reads));
                return Err("受控 source 已取消".into());
            }
            // 此等待仅模拟可取消 read；所有测试均先等待 Read latch 再发送图像。
            match self.commands.recv_timeout(Duration::from_millis(2)) {
                Ok(ReadCommand::Image(image)) => {
                    let image = Arc::new(image);
                    self.trace.push(SourceEvent::Returned {
                        number: self.reads,
                        pixels: image.bgra.as_ptr() as usize,
                        image: Arc::downgrade(&image),
                    });
                    return Ok(image);
                }
                Ok(ReadCommand::Error) => return Err("受控 source 错误".into()),
                Ok(ReadCommand::Panic) => panic!("受控 source panic"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("受控 source 通道关闭".into())
                }
            }
        }
    }
    fn release(&mut self) {
        self.trace.push(SourceEvent::Released);
    }
}
impl Drop for ScriptSource {
    fn drop(&mut self) {
        self.trace.push(SourceEvent::Dropped);
    }
}

struct Rig {
    pipeline: CapturePipeline,
    commands: mpsc::Sender<ReadCommand>,
    trace: Arc<Trace>,
}
impl Rig {
    fn new(region: ScreenRect) -> Self {
        Self::with_interval(region, Duration::ZERO)
    }
    fn with_interval(region: ScreenRect, interval: Duration) -> Self {
        Self::with_gate(region, interval, None)
    }
    fn with_gate(region: ScreenRect, interval: Duration, gate: Option<FrameGate>) -> Self {
        let (commands, receive) = mpsc::channel();
        let trace = Arc::new(Trace::default());
        let source = ScriptSource {
            commands: receive,
            trace: trace.clone(),
            reads: 0,
        };
        let pipeline =
            CapturePipeline::start_with_source(region, interval, gate, Box::new(source)).unwrap();
        Self {
            pipeline,
            commands,
            trace,
        }
    }
    fn send(&self, number: usize, image: Image) -> usize {
        self.trace.read(number);
        let pointer = image.bgra.as_ptr() as usize;
        self.commands.send(ReadCommand::Image(image)).unwrap();
        self.trace.returned(number);
        pointer
    }
    fn take(&self) -> CapturedFrame {
        let until = Instant::now() + TEST_TIMEOUT;
        loop {
            if let Some(frame) = self.pipeline.try_take() {
                return frame;
            }
            assert!(Instant::now() < until, "未取得已安排的生产帧");
            thread::yield_now();
        }
    }
    fn wait_state(&self, predicate: impl Fn(&State) -> bool) -> PipelineProbe {
        let until = Instant::now() + TEST_TIMEOUT;
        let mut state = self
            .pipeline
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while !predicate(&state) {
            let remaining = until.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "pipeline latch 未到达：{:?}",
                state.wait_reason
            );
            // 只等待生产控制门的状态条件；小超时用于兼容 wait_reason 本身不发通知。
            let (next, _) = self
                .pipeline
                .shared
                .wake
                .wait_timeout(state, remaining.min(Duration::from_millis(10)))
                .unwrap_or_else(|e| e.into_inner());
            state = next;
        }
        drop(state);
        self.pipeline.probe()
    }
    fn join(&mut self) {
        self.pipeline.stop();
        self.trace.wait(|events| {
            events
                .iter()
                .any(|event| matches!(event, SourceEvent::Dropped))
        });
        let until = Instant::now() + TEST_TIMEOUT;
        while !self.pipeline.try_join() {
            assert!(Instant::now() < until, "producer 未真实退出");
            thread::yield_now();
        }
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.pipeline.stop();
        let until = Instant::now() + TEST_TIMEOUT;
        while !self.pipeline.try_join() && Instant::now() < until {
            thread::yield_now();
        }
    }
}

fn region() -> ScreenRect {
    ScreenRect {
        left: -48,
        top: 20,
        width: 64,
        height: 128,
    }
}

fn pixel(row: u32, x: u32) -> [u8; 4] {
    let mut hash = row.wrapping_mul(2_654_435_761);
    hash ^= hash >> 16;
    [
        (hash.wrapping_add(x * 17)) as u8,
        ((hash >> 8).wrapping_add(x * 5)) as u8,
        ((hash >> 16).wrapping_add(x * 11)) as u8,
        255,
    ]
}

fn frame(start: u32) -> Image {
    let region = region();
    let mut bgra = Vec::with_capacity(region.width as usize * region.height as usize * 4);
    for y in 0..region.height {
        for x in 0..region.width {
            bgra.extend_from_slice(&pixel(start + y, x));
        }
    }
    Image {
        width: region.width,
        height: region.height,
        bgra,
        readback_ms: 0.,
    }
}

fn assert_rows(image: &Image, first: u32, rows: u32) {
    assert_eq!((image.width, image.height), (64, rows));
    for y in 0..rows {
        for x in 0..64 {
            let offset = (y as usize * 64 + x as usize) * 4;
            assert_eq!(
                &image.bgra[offset..offset + 4],
                &pixel(first + y, x),
                "x={x},y={y}"
            );
        }
    }
}

#[test]
fn long_pipeline_fifo_moves_pixels_and_producer_reads_while_consumer_holds_frame() {
    let mut rig = Rig::new(region());
    rig.pipeline
        .configure(1, ScrollAxis::Vertical, Mode::Running);
    let mut pointers = vec![rig.send(1, frame(0))];
    let first = rig.take();
    assert_eq!(first.stamp.sequence, 1);
    let consumer_thread = thread::current().id();
    // 消费者仍持有第一帧，没有处理/释放；producer 独立读满后续两帧和在途尾帧。
    pointers.push(rig.send(2, frame(12)));
    pointers.push(rig.send(3, frame(24)));
    pointers.push(rig.send(4, frame(36)));
    let full = rig.wait_state(|state| state.wait_reason == Some("queue-full"));
    assert_eq!(
        (full.queue_depth, full.peak_queue_depth, full.max_raw_slots),
        (2, 2, 4)
    );
    let frame_bytes = 64 * 128 * 4;
    assert_eq!(full.reserved_raw_bytes, 4 * frame_bytes);
    assert_eq!(full.raw_bytes, 4 * frame_bytes);
    assert_eq!(rig.trace.reads(), 4);
    let events = rig.trace.events.lock().unwrap().clone();
    let read_threads: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SourceEvent::Read {
                thread,
                region: actual,
                ..
            } => {
                assert_eq!(*actual, region());
                Some(*thread)
            }
            _ => None,
        })
        .collect();
    assert_eq!(read_threads.len(), 4);
    assert!(read_threads
        .iter()
        .all(|id| *id != consumer_thread && *id == read_threads[0]));
    for event in &events {
        if let SourceEvent::Returned { number, pixels, .. } = event {
            assert_eq!(*pixels, pointers[*number - 1]);
        }
    }
    let fence = rig.pipeline.begin_drain(2);
    assert_eq!(fence.tail_sequence, 4);
    assert_eq!(first.image.bgra.as_ptr() as usize, pointers[0]);
    assert!(rig.pipeline.accepts(&first.stamp));
    rig.pipeline.mark_processed(first.stamp.sequence);
    drop(first);
    for number in 2..=4 {
        let captured = rig.take();
        assert_eq!(captured.stamp.sequence, number);
        assert_eq!(
            captured.image.bgra.as_ptr() as usize,
            pointers[number as usize - 1]
        );
        assert_eq!(captured.image.bgra, frame((number as u32 - 1) * 12).bgra);
        assert!(rig.pipeline.accepts(&captured.stamp));
        assert!(captured.stamp.read_end >= captured.stamp.read_start);
        rig.pipeline.mark_processed(captured.stamp.sequence);
        drop(captured);
    }
    let paused = rig.wait_state(|state| state.quiesced && state.queue.is_empty());
    assert!(rig.pipeline.drain_ready(&fence));
    assert!(rig.pipeline.finish_drain(&fence));
    assert_eq!(
        (
            paused.reserved_raw_bytes,
            paused.raw_bytes,
            paused.coalesced_count
        ),
        (0, 0, 0)
    );
    assert_eq!(rig.trace.reads(), 4, "排空后不能再发第五个 read permit");
    rig.join();
}

#[test]
fn long_pipeline_large_roi_budget_counts_consumer_and_stop_wakes_budget_wait() {
    let large = ScreenRect {
        left: 0,
        top: 0,
        width: 4096,
        height: 2049,
    };
    let bytes = u64::from(large.width) * u64::from(large.height) * 4;
    let mut rig = Rig::new(large);
    rig.pipeline
        .configure(5, ScrollAxis::Vertical, Mode::Running);
    let pointer = rig.send(
        1,
        Image {
            width: large.width,
            height: large.height,
            bgra: vec![9; bytes as usize],
            readback_ms: 0.,
        },
    );
    let held = rig.take();
    let blocked = rig.wait_state(|state| state.wait_reason == Some("raw-budget"));
    assert_eq!((blocked.queue_depth, blocked.max_raw_slots), (0, 1));
    assert_eq!(
        (
            blocked.reserved_raw_bytes,
            blocked.raw_budget_bytes,
            blocked.raw_bytes
        ),
        (bytes, bytes, bytes)
    );
    assert_eq!(held.image.bgra.as_ptr() as usize, pointer);
    assert_eq!(
        rig.trace.reads(),
        1,
        "消费中的原帧仍占许可，不能先分配第二帧"
    );
    rig.join();
    assert_eq!(rig.trace.reads(), 1, "停止预算等待不应获得新的 read permit");
    assert_eq!(held.image.bgra[0], 9, "停止后消费者持有的帧仍然有效");
    drop(held);
    let closed = rig.pipeline.probe();
    assert_eq!(
        (
            closed.reserved_raw_bytes,
            closed.raw_bytes,
            closed.queue_depth
        ),
        (0, 0, 0)
    );
}

#[test]
fn long_pipeline_pause_rejects_queued_matching_and_late_read_without_poisoning_resume() {
    let mut rig = Rig::new(region());
    rig.pipeline
        .configure(7, ScrollAxis::Vertical, Mode::Running);
    rig.send(1, frame(0));
    let matching = rig.take();
    let original = matching.image.bgra.clone();
    rig.send(2, frame(12));
    let queued = rig.trace.returned(2);
    rig.trace.read(3);
    rig.pipeline
        .configure(8, ScrollAxis::Vertical, Mode::Paused);
    assert!(
        !rig.pipeline
            .pause_if_current(matching.stamp.epoch, matching.stamp.axis),
        "旧消费者不能把已请求的新一代改回旧代暂停"
    );
    rig.trace.wait(|events| {
        events
            .iter()
            .any(|event| matches!(event, SourceEvent::Cancelled(3)))
    });
    let paused = rig.wait_state(|state| state.quiesced && !state.in_flight);
    assert!(
        !rig.pipeline.accepts(&matching.stamp),
        "已进入匹配的旧帧不能提交到新一代"
    );
    assert!(!rig.pipeline.accepts_epoch(7, 8));
    assert!(rig.pipeline.try_take().is_none(), "旧排队帧必须释放");
    assert!(queued.upgrade().is_none());
    assert_eq!(matching.image.bgra, original);
    assert_eq!(paused.reserved_raw_bytes, 64 * 128 * 4);
    assert!(paused.error.is_none(), "预期取消不能污染新一代错误槽");
    rig.pipeline
        .configure(9, ScrollAxis::Horizontal, Mode::Single);
    let fresh_pointer = rig.send(4, frame(100));
    let queued_single = rig.wait_state(|state| state.quiesced && state.queue.len() == 1);
    assert_eq!(queued_single.epoch, 9);
    // 旧代真实消费帧尚未释放；新 Single 已返回但消费者尚未 apply 新命令。
    assert!(rig
        .pipeline
        .try_take_for_epoch(matching.stamp.epoch)
        .is_none());
    assert!(rig.pipeline.try_take_for_epoch(8).is_none());
    assert_eq!(
        rig.pipeline.probe().queue_depth,
        1,
        "拒绝旧代 pop 必须保留新 Single 的唯一帧"
    );
    assert!(!rig.pipeline.pause_if_current(7, ScrollAxis::Vertical));
    assert!(!rig.pipeline.pause_if_current(8, ScrollAxis::Vertical));
    assert_eq!(
        (rig.pipeline.probe().epoch, rig.pipeline.probe().queue_depth),
        (9, 1)
    );
    let fresh = rig
        .pipeline
        .try_take_for_epoch(9)
        .expect("新消费者必须取得此前保留的唯一 Single 帧");
    assert_eq!(
        (fresh.stamp.epoch, fresh.stamp.sequence, fresh.stamp.axis),
        (9, 4, ScrollAxis::Horizontal)
    );
    assert_eq!(fresh.image.bgra.as_ptr() as usize, fresh_pointer);
    assert_eq!(fresh.image.bgra, frame(100).bgra);
    assert_eq!(
        matching.image.bgra, original,
        "切代和拒 pop 不能破坏旧消费者手中像素"
    );
    assert!(rig.pipeline.accepts(&fresh.stamp));
    let mut forged = fresh.stamp;
    forged.region.left += 1;
    assert!(!rig.pipeline.accepts(&forged));
    forged = fresh.stamp;
    forged.axis = ScrollAxis::Vertical;
    assert!(!rig.pipeline.accepts(&forged));
    rig.pipeline.mark_processed(fresh.stamp.sequence);
    drop(fresh);
    drop(matching);
    assert!(rig.pipeline.pause_if_current(9, ScrollAxis::Horizontal));
    let single = rig.wait_state(|state| state.quiesced && !state.in_flight);
    assert_eq!((single.epoch, single.reserved_raw_bytes), (9, 0));
    assert_eq!(rig.trace.reads(), 4, "Single 只允许一个新 read");
    rig.join();
}

#[test]
fn long_pipeline_drain_contains_issued_tail_pixels_and_closes_future_permits() {
    let mut rig = Rig::new(region());
    rig.pipeline
        .configure(10, ScrollAxis::Vertical, Mode::Running);
    rig.send(1, frame(0));
    rig.send(2, frame(12));
    rig.trace.read(3); // 两帧 FIFO 已满；第三帧已获许可，但 read 尚未返回。
    let fence = rig.pipeline.begin_drain(11);
    assert_eq!(
        (fence.from_epoch, fence.desired_epoch, fence.tail_sequence),
        (10, 11, 3)
    );
    assert!(rig.pipeline.accepts_epoch(10, 11));
    assert!(!rig.pipeline.drain_ready(&fence));
    let initial = rig.take();
    assert_eq!(initial.stamp.sequence, 1);
    let mut stitch = Stitcher::with_options_shared(
        initial.image.clone(),
        StitchOptions {
            detect_fixed_borders: false,
            ..Default::default()
        },
    )
    .unwrap();
    rig.pipeline.mark_processed(initial.stamp.sequence);
    drop(initial);
    let second = rig.take();
    assert_eq!(second.stamp.sequence, 2);
    let update = stitch.process_shared(second.image.clone()).unwrap();
    assert_eq!(
        (update.kind, update.delta, update.added),
        (UpdateKind::Added, 12, 12)
    );
    rig.pipeline.mark_processed(second.stamp.sequence);
    drop(second);
    assert!(
        !rig.pipeline.drain_ready(&fence),
        "只有排队内容处理完成不代表尾帧完成"
    );
    assert!(!rig.pipeline.finish_drain(&fence));
    rig.send(3, frame(24));
    let tail = rig.take();
    assert_eq!((tail.stamp.epoch, tail.stamp.sequence), (10, 3));
    assert!(rig.pipeline.accepts(&tail.stamp));
    let update = stitch.process_shared(tail.image.clone()).unwrap();
    assert_eq!(
        (update.kind, update.delta, update.added),
        (UpdateKind::Added, 12, 12)
    );
    assert!(
        !rig.pipeline.drain_ready(&fence),
        "匹配后的尾帧还没提交，不能 ACK"
    );
    rig.pipeline.mark_processed(tail.stamp.sequence);
    drop(tail);
    rig.wait_state(|state| state.quiesced && state.queue.is_empty());
    assert!(rig.pipeline.drain_ready(&fence));
    assert!(rig.pipeline.finish_drain(&fence));
    assert_rows(&stitch.image(), 0, 152);
    assert_eq!(
        rig.trace.reads(),
        3,
        "drain 截取 issued=3 后不得新发第四个许可"
    );
    assert_eq!(rig.pipeline.probe().epoch, 11);
    drop(stitch);
    rig.join();
    assert_eq!(
        (
            rig.pipeline.probe().reserved_raw_bytes,
            rig.pipeline.probe().raw_bytes
        ),
        (0, 0)
    );
    for number in 1..=3 {
        assert!(rig.trace.returned(number).upgrade().is_none());
    }
}

#[test]
fn long_pipeline_new_epoch_or_stop_cancels_old_drain_and_late_capture() {
    for close in [false, true] {
        let mut rig = Rig::new(region());
        rig.pipeline
            .configure(20, ScrollAxis::Vertical, Mode::Running);
        rig.send(1, frame(0));
        rig.trace.read(2);
        let fence = rig.pipeline.begin_drain(21);
        let first = rig.take();
        rig.pipeline.mark_processed(first.stamp.sequence);
        drop(first);
        if close {
            rig.pipeline.stop();
        } else {
            rig.pipeline
                .configure(22, ScrollAxis::Horizontal, Mode::Paused);
        }
        rig.trace.wait(|events| {
            events
                .iter()
                .any(|event| matches!(event, SourceEvent::Cancelled(2)))
        });
        assert!(!rig.pipeline.accepts_epoch(20, 21));
        assert!(!rig.pipeline.drain_ready(&fence));
        assert!(
            !rig.pipeline.finish_drain(&fence),
            "旧 completion 不能打开陈旧编辑/导出快照"
        );
        rig.join();
        assert!(rig.pipeline.try_take().is_none());
        assert_eq!(
            (
                rig.pipeline.probe().reserved_raw_bytes,
                rig.pipeline.probe().raw_bytes
            ),
            (0, 0)
        );
    }
}

#[test]
fn long_pipeline_stop_releases_paused_read_full_queue_cadence_and_pending_drain() {
    for wait in ["paused", "read", "queue-full", "cadence", "drain"] {
        let mut rig = Rig::with_interval(
            region(),
            if wait == "cadence" {
                Duration::from_secs(60)
            } else {
                Duration::ZERO
            },
        );
        let mut fence = None;
        match wait {
            "paused" => {
                rig.wait_state(|state| state.wait_reason == Some("paused"));
            }
            "read" => {
                rig.pipeline
                    .configure(30, ScrollAxis::Vertical, Mode::Running);
                rig.trace.read(1);
            }
            "queue-full" => {
                rig.pipeline
                    .configure(30, ScrollAxis::Vertical, Mode::Running);
                for number in 1..=3 {
                    rig.send(number, frame((number as u32 - 1) * 12));
                }
                rig.wait_state(|state| state.wait_reason == Some("queue-full"));
            }
            "cadence" => {
                rig.pipeline
                    .configure(30, ScrollAxis::Vertical, Mode::Running);
                rig.send(1, frame(0));
                let captured = rig.take();
                rig.pipeline.mark_processed(captured.stamp.sequence);
                drop(captured);
                rig.wait_state(|state| state.wait_reason == Some("cadence"));
            }
            "drain" => {
                rig.pipeline
                    .configure(30, ScrollAxis::Vertical, Mode::Running);
                rig.trace.read(1);
                fence = Some(rig.pipeline.begin_drain(31));
            }
            _ => unreachable!(),
        }
        rig.join();
        let closed = rig.pipeline.probe();
        assert!(
            !closed.producer_alive && closed.producer_quiesced && !closed.capture_in_flight,
            "{wait}"
        );
        assert_eq!(
            (
                closed.queue_depth,
                closed.reserved_raw_bytes,
                closed.raw_bytes
            ),
            (0, 0, 0),
            "{wait}"
        );
        assert_eq!(
            (
                closed.capture_storage_pixels,
                closed.capture_output_pixels,
                closed.capture_screen_count
            ),
            (0, 0, 0)
        );
        if let Some(fence) = fence {
            assert!(!rig.pipeline.finish_drain(&fence));
        }
        let events = rig.trace.events.lock().unwrap().clone();
        for event in events {
            if let SourceEvent::Returned { image, .. } = event {
                assert!(image.upgrade().is_none(), "{wait}");
            }
        }
    }
}

#[test]
fn long_pipeline_source_error_and_panic_release_producer_and_preserve_trusted_pixels() {
    for panic in [false, true] {
        let mut rig = Rig::new(region());
        rig.pipeline
            .configure(40, ScrollAxis::Vertical, Mode::Running);
        rig.send(1, frame(0));
        let accepted = rig.take();
        let trusted =
            Stitcher::with_options_shared(accepted.image.clone(), StitchOptions::default())
                .unwrap();
        rig.pipeline.mark_processed(accepted.stamp.sequence);
        drop(accepted);
        rig.trace.read(2);
        rig.commands
            .send(if panic {
                ReadCommand::Panic
            } else {
                ReadCommand::Error
            })
            .unwrap();
        let failure = rig.wait_state(|state| state.error.is_some() && !state.in_flight);
        assert!(failure.error.as_ref().unwrap().contains(if panic {
            "意外退出"
        } else {
            "受控 source 错误"
        }));
        if !panic {
            rig.wait_state(|state| state.quiesced);
        }
        assert!(rig.pipeline.take_error().is_some());
        assert!(rig.pipeline.take_error().is_none(), "故障槽有界且只取一次");
        if panic {
            assert!(!failure.producer_alive);
            assert!(rig.pipeline.pause_if_current(40, ScrollAxis::Vertical));
            let exited = rig.pipeline.probe();
            assert!(
                !exited.producer_alive && exited.producer_quiesced,
                "已经真实退出的 producer 不能被迟到暂停标为等待一个不存在的 release ACK"
            );
        }
        rig.join();
        assert_rows(&trusted.image(), 0, 128);
        assert_eq!(rig.pipeline.probe().reserved_raw_bytes, 0);
        let original = rig.trace.returned(1);
        assert!(
            original.upgrade().is_some(),
            "producer 退出不能破坏最后可信结果"
        );
        drop(trusted);
        assert!(original.upgrade().is_none());
        assert_eq!(rig.pipeline.probe().raw_bytes, 0);
    }
}

#[test]
fn long_pipeline_post_fence_rejects_queued_and_inflight_old_response_evidence() {
    let mut rig = Rig::new(region());
    rig.pipeline
        .configure(50, ScrollAxis::Vertical, Mode::Running);
    rig.send(1, frame(0));
    rig.send(2, frame(12));
    rig.trace.read(3);
    let start = Instant::now();
    let mut policy = AutoPolicy::new(AutoScrollOptions::default(), 50, start);
    let posts = Cell::new(0);
    let denied = rig
        .pipeline
        .with_post_fence(50, || Err("受控目标拒绝投递".into()));
    assert!(denied.is_err());
    assert_eq!(
        policy.snapshot("own".into()).steps,
        0,
        "失败投递不能推进 AutoPolicy"
    );
    let fence = rig
        .pipeline
        .with_post_fence(50, || {
            posts.set(posts.get() + 1);
            Ok(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(fence, 3, "post 应把已 issued 的在途帧也排除");
    policy.sent(fence, start);
    assert_eq!(policy.snapshot("own".into()).steps, 1);
    for (number, milliseconds) in [(1, 1000), (2, 2000)] {
        let old = rig.take();
        assert_eq!(old.stamp.sequence, number);
        assert_eq!(
            policy.observe(
                old.stamp.epoch,
                old.stamp.sequence,
                UpdateKind::Unchanged,
                0,
                0,
                start + Duration::from_millis(milliseconds)
            ),
            Decision::Wait
        );
        rig.pipeline.mark_processed(old.stamp.sequence);
        drop(old);
    }
    rig.send(3, frame(24));
    let old_inflight = rig.take();
    assert_eq!(old_inflight.stamp.sequence, 3);
    assert_eq!(
        policy.observe(
            old_inflight.stamp.epoch,
            old_inflight.stamp.sequence,
            UpdateKind::Unchanged,
            0,
            0,
            start + Duration::from_millis(2200)
        ),
        Decision::Wait,
        "晚返回的旧 read 不能被计为 no-motion/稳定响应"
    );
    rig.pipeline.mark_processed(old_inflight.stamp.sequence);
    drop(old_inflight);
    for (number, kind, delta, milliseconds, expected) in [
        (4, UpdateKind::Added, 12, 2300, Decision::Wait),
        (5, UpdateKind::Unchanged, 0, 2320, Decision::Wait),
        (6, UpdateKind::Unchanged, 0, 2600, Decision::Post),
    ] {
        rig.send(number, frame(36));
        let current = rig.take();
        assert_eq!(current.stamp.sequence, number as u64);
        assert!(current.stamp.sequence > fence);
        assert_eq!(
            policy.observe(
                current.stamp.epoch,
                current.stamp.sequence,
                kind,
                delta,
                12,
                start + Duration::from_millis(milliseconds)
            ),
            expected
        );
        rig.pipeline.mark_processed(current.stamp.sequence);
        drop(current);
    }
    assert_eq!(
        posts.get(),
        1,
        "仅新证据产生第二次 Post 决策，本测试尚未发送第二步"
    );
    let post = || {
        posts.set(posts.get() + 1);
        Ok(())
    };
    assert_eq!(rig.pipeline.with_post_fence(49, post).unwrap(), None);
    rig.pipeline.begin_drain(51);
    assert_eq!(rig.pipeline.with_post_fence(50, post).unwrap(), None);
    rig.pipeline
        .configure(52, ScrollAxis::Vertical, Mode::Paused);
    assert_eq!(rig.pipeline.with_post_fence(52, post).unwrap(), None);
    rig.join();
    assert_eq!(rig.pipeline.with_post_fence(52, post).unwrap(), None);
    assert_eq!(posts.get(), 1, "旧代/排空/暂停/停止均不调用原生投递闭包");
}

#[test]
fn long_pipeline_ownership_gate_failure_cancels_read_and_fails_pending_tail() {
    let valid = Arc::new(AtomicBool::new(true));
    let checks = Arc::new(AtomicUsize::new(0));
    let gate_valid = valid.clone();
    let gate_checks = checks.clone();
    let gate: FrameGate = Arc::new(move || {
        gate_checks.fetch_add(1, Ordering::AcqRel);
        if gate_valid.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err("自有 source gate 已失效".into())
        }
    });
    let mut rig = Rig::with_gate(region(), Duration::ZERO, Some(gate));
    rig.pipeline
        .configure(60, ScrollAxis::Vertical, Mode::Running);
    rig.send(1, frame(0));
    let accepted = rig.take();
    let trusted = accepted.image.clone();
    rig.pipeline.mark_processed(accepted.stamp.sequence);
    drop(accepted);
    rig.trace.read(2);
    let fence = rig.pipeline.begin_drain(61);
    valid.store(false, Ordering::Release);
    rig.trace.wait(|events| {
        events
            .iter()
            .any(|event| matches!(event, SourceEvent::Cancelled(2)))
    });
    let failed =
        rig.wait_state(|state| state.error.is_some() && state.quiesced && !state.in_flight);
    assert!(checks.load(Ordering::Acquire) > 0);
    assert!(failed
        .error
        .as_ref()
        .unwrap()
        .contains("自有 source gate 已失效"));
    assert!(!rig.pipeline.drain_ready(&fence));
    assert!(
        !rig.pipeline.finish_drain(&fence),
        "失效尾帧不得冒充成功排空"
    );
    assert!(rig.pipeline.try_take().is_none());
    assert_rows(&trusted, 0, 128);
    assert!(rig
        .pipeline
        .take_error()
        .unwrap()
        .contains("自有 source gate 已失效"));
    rig.join();
    drop(trusted);
    assert_eq!(
        (
            rig.pipeline.probe().reserved_raw_bytes,
            rig.pipeline.probe().raw_bytes
        ),
        (0, 0)
    );
}

#[test]
fn long_pipeline_live_relocate_keeps_epoch_source_and_inflight_frame() {
    let start = region();
    let reach = ScreenRect {
        height: start.height + 200,
        ..start
    };
    let (commands, receive) = mpsc::channel();
    let trace = Arc::new(Trace::default());
    let source = ScriptSource {
        commands: receive,
        trace: trace.clone(),
        reads: 0,
    };
    let pipeline = CapturePipeline::start_with_context(
        CaptureConfig::new_reaching(start, reach, None).unwrap(),
        Duration::ZERO,
        Box::new(source),
    )
    .unwrap();
    let rig = Rig {
        pipeline,
        commands,
        trace,
    };
    rig.pipeline
        .configure(1, ScrollAxis::Vertical, Mode::Running);
    rig.send(1, frame(0));
    assert_eq!(rig.take().stamp.region, start);
    // 第二帧读回途中选区下移：这帧不作废，归入新选区；之后从新位置读。
    rig.trace.read(2);
    let moved = ScreenRect {
        top: start.top + 40,
        ..start
    };
    let context = rig.pipeline.context();
    rig.pipeline
        .relocate(context.relocated(moved, None).unwrap())
        .unwrap();
    rig.send(2, frame(12));
    let second = rig.take();
    assert_eq!((second.stamp.epoch, second.stamp.region), (1, moved));
    assert!(rig.pipeline.accepts(&second.stamp));
    let events = rig.trace.read(3);
    assert!(events.iter().any(
        |event| matches!(event, SourceEvent::Read { number: 3, region, .. } if *region == moved)
    ));
    let released = events
        .iter()
        .filter(|event| matches!(event, SourceEvent::Released))
        .count();
    assert_eq!(released, 1, "实时移动不应重建采集器：{events:?}");
    assert!(!events
        .iter()
        .any(|event| matches!(event, SourceEvent::Cancelled(_))));
    // 越出采集范围或改变尺寸都不能走实时移动。
    let context = rig.pipeline.context();
    assert!(context
        .relocated(
            ScreenRect {
                top: start.top + 201,
                ..start
            },
            None
        )
        .is_err());
    assert!(context
        .relocated(
            ScreenRect {
                height: start.height - 1,
                ..moved
            },
            None
        )
        .is_err());
    rig.pipeline.stop();
    rig.pipeline.join();
}
