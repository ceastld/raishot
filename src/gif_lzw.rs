//! GIF 索引矩形的有界 LZW 流水线；最多三个工人并行压缩，调用线程按原顺序写入。
use crate::Result;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    io::Write,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

const MAX_IN_FLIGHT: usize = 3;
const QUEUE_CAPACITY: usize = 2;
const WAIT_SLICE: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rectangle {
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
}

struct Request {
    sequence: u64,
    rect: Rectangle,
    indices: Vec<u8>,
    start: Duration,
    end: Duration,
}

struct Encoded {
    sequence: u64,
    frame: gif::Frame<'static>,
    start: Duration,
    end: Duration,
}

type Response = std::result::Result<Encoded, String>;

/// 总计最多三个待压缩、正在压缩或等待写入的矩形，不保留整段视频。
pub(crate) struct LzwPipeline {
    requests: Option<SyncSender<Request>>,
    responses: Option<Receiver<Response>>,
    workers: Vec<JoinHandle<()>>,
    pending: BTreeMap<u64, Encoded>,
    in_flight: usize,
    next_request: u64,
    next_response: u64,
}

impl LzwPipeline {
    pub(crate) fn new() -> Result<Self> {
        let count = std::thread::available_parallelism()
            .map(|count| count.get().min(MAX_IN_FLIGHT))
            .unwrap_or(1);
        Self::start_factory(count, |_| encode_frame)
    }

    #[cfg(test)]
    fn start(
        encode: impl FnMut(Rectangle, Vec<u8>) -> gif::Frame<'static> + Send + 'static,
    ) -> Result<Self> {
        let mut encode = Some(encode);
        Self::start_factory(1, |_| encode.take().unwrap())
    }

    fn start_factory<Encode>(
        worker_count: usize,
        mut factory: impl FnMut(usize) -> Encode,
    ) -> Result<Self>
    where
        Encode: FnMut(Rectangle, Vec<u8>) -> gif::Frame<'static> + Send + 'static,
    {
        if worker_count == 0 || worker_count > MAX_IN_FLIGHT {
            return Err("GIF LZW 工人数无效".into());
        }
        let (requests, receiver) = sync_channel::<Request>(QUEUE_CAPACITY);
        let (sender, responses) = sync_channel::<Response>(QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut pipeline = Self {
            requests: Some(requests),
            responses: Some(responses),
            workers: Vec::with_capacity(worker_count),
            pending: BTreeMap::new(),
            in_flight: 0,
            next_request: 0,
            next_response: 0,
        };
        for worker_index in 0..worker_count {
            let receiver = receiver.clone();
            let sender = sender.clone();
            let mut encode = factory(worker_index);
            let worker = std::thread::Builder::new()
                .name(format!("rustshot-gif-lzw-{worker_index}"))
                .spawn(move || loop {
                    // recv 只负责取走任务；必须先释放锁，再执行压缩与发送回复。
                    let request = match receiver.lock() {
                        Ok(receiver) => receiver.recv(),
                        Err(_) => break,
                    };
                    let Ok(request) = request else { break };
                    // unwind 构建下向调用方报告 panic；abort 构建沿用进程终止语义。
                    let outcome =
                        catch_unwind(AssertUnwindSafe(|| encode(request.rect, request.indices)))
                            .map(|frame| Encoded {
                                sequence: request.sequence,
                                frame,
                                start: request.start,
                                end: request.end,
                            })
                            .map_err(|_| "GIF LZW 工作线程编码时发生 panic".to_string());
                    let failed = outcome.is_err();
                    if sender.send(outcome).is_err() || failed {
                        break;
                    }
                });
            match worker {
                Ok(worker) => pipeline.workers.push(worker),
                Err(error) => {
                    // 部分工人已启动时也必须关闭通道并 join，不能分离出遗留线程。
                    let _ = pipeline.shutdown();
                    return Err(format!("无法启动 GIF LZW 工作线程：{error}").into());
                }
            }
        }
        Ok(pipeline)
    }

    /// 满三个任务时先写出最早一个；proceed 只检查取消/更新原进度，不产生新进度。
    pub(crate) fn submit<W: Write>(
        &mut self,
        encoder: &mut gif::Encoder<W>,
        rect: Rectangle,
        indices: Vec<u8>,
        start: Duration,
        end: Duration,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<()> {
        if rect.width == 0
            || rect.height == 0
            || indices.len() != usize::from(rect.width) * usize::from(rect.height)
        {
            return Err("GIF LZW 索引矩形无效".into());
        }
        if !proceed() {
            return Err("已取消 GIF 转换".into());
        }
        if self.in_flight >= MAX_IN_FLIGHT {
            self.drain_one(encoder, proceed)?;
        }
        let mut request = Request {
            sequence: self.next_request,
            rect,
            indices,
            start,
            end,
        };
        loop {
            let Some(sender) = &self.requests else {
                return Err("GIF LZW 工作线程已关闭".into());
            };
            match sender.try_send(request) {
                Ok(()) => {
                    self.next_request += 1;
                    self.in_flight += 1;
                    return Ok(());
                }
                Err(TrySendError::Full(returned)) => {
                    request = returned;
                    // 先消费回复，避免等待请求队列时工作线程又堵在回复队列。
                    self.drain_one(encoder, proceed)?;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err("GIF LZW 工作线程提前退出".into());
                }
            }
        }
    }

    pub(crate) fn finish<W: Write>(
        &mut self,
        encoder: &mut gif::Encoder<W>,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<()> {
        while self.in_flight != 0 {
            self.drain_one(encoder, proceed)?;
        }
        self.shutdown()
    }

    fn drain_one<W: Write>(
        &mut self,
        encoder: &mut gif::Encoder<W>,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<()> {
        if self.in_flight == 0 {
            return Err("GIF LZW 队列状态无效".into());
        }
        let mut encoded = loop {
            if !proceed() {
                return Err("已取消 GIF 转换".into());
            }
            if let Some(encoded) = self.pending.remove(&self.next_response) {
                break encoded;
            }
            let Some(receiver) = &self.responses else {
                return Err("GIF LZW 回复通道已关闭".into());
            };
            match receiver.recv_timeout(WAIT_SLICE) {
                Ok(response) => {
                    let encoded =
                        response.map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
                    if encoded.sequence < self.next_response
                        || encoded.sequence >= self.next_request
                    {
                        return Err("GIF LZW 回复次序无效".into());
                    }
                    if encoded.sequence == self.next_response {
                        break encoded;
                    }
                    let sequence = encoded.sequence;
                    if self.pending.insert(sequence, encoded).is_some()
                        || self.pending.len() >= MAX_IN_FLIGHT
                    {
                        return Err("GIF LZW 乱序回复状态无效".into());
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("GIF LZW 工作线程提前退出".into());
                }
            }
        };
        self.in_flight -= 1;
        self.next_response += 1;
        let mut remaining = delay_centis(encoded.start, encoded.end);
        while remaining != 0 {
            let delay = next_delay_chunk(remaining);
            encoded.frame.delay = delay;
            encoder.write_lzw_pre_encoded_frame(&encoded.frame)?;
            remaining -= u64::from(delay);
            if !proceed() {
                return Err("已取消 GIF 转换".into());
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        // 两端必须先关闭：只关闭请求再 join，会让满回复队列的 worker 永久堵在 send。
        self.requests.take();
        self.responses.take();
        self.pending.clear();
        self.in_flight = 0;
        let mut panicked = false;
        for worker in self.workers.drain(..) {
            panicked |= worker.join().is_err();
        }
        if panicked {
            return Err("GIF LZW 工作线程发生 panic".into());
        }
        Ok(())
    }
}

fn encode_frame(rect: Rectangle, indices: Vec<u8>) -> gif::Frame<'static> {
    let mut frame = gif::Frame {
        width: rect.width,
        height: rect.height,
        left: rect.left,
        top: rect.top,
        dispose: gif::DisposalMethod::Keep,
        buffer: Cow::Owned(indices),
        ..gif::Frame::default()
    };
    frame.make_lzw_pre_encoded();
    frame
}

impl Drop for LzwPipeline {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub(crate) fn delay_centis(start: Duration, end: Duration) -> u64 {
    let rounded = |time: Duration| {
        ((time.as_nanos() + 5_000_000) / 10_000_000).min(u128::from(u64::MAX)) as u64
    };
    rounded(end).saturating_sub(rounded(start)).max(2)
}

pub(crate) fn next_delay_chunk(remaining: u64) -> u16 {
    if remaining == u64::from(u16::MAX) + 1 {
        u16::MAX - 1
    } else {
        remaining.min(u64::from(u16::MAX)) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[derive(Clone)]
    struct Input {
        rect: Rectangle,
        indices: Vec<u8>,
        start: Duration,
        end: Duration,
    }

    fn palette() -> Vec<u8> {
        (0..=255_u8).flat_map(|value| [value; 3]).collect()
    }

    fn input(index: usize, start: Duration, end: Duration) -> Input {
        Input {
            rect: Rectangle {
                left: 0,
                top: 0,
                width: 4,
                height: 3,
            },
            indices: (0..12).map(|pixel| (index * 13 + pixel) as u8).collect(),
            start,
            end,
        }
    }

    // 同步基线独立走 Encoder::write_frame，与预压缩 API 的产物逐字节比较。
    fn synchronous(inputs: &[Input]) -> Vec<u8> {
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        for input in inputs {
            let mut frame = gif::Frame {
                width: input.rect.width,
                height: input.rect.height,
                left: input.rect.left,
                top: input.rect.top,
                dispose: gif::DisposalMethod::Keep,
                buffer: Cow::Borrowed(&input.indices),
                ..gif::Frame::default()
            };
            let mut remaining = delay_centis(input.start, input.end);
            while remaining != 0 {
                let delay = next_delay_chunk(remaining);
                frame.delay = delay;
                encoder.write_frame(&frame).unwrap();
                remaining -= u64::from(delay);
            }
        }
        encoder.into_inner().unwrap()
    }

    fn pipelined(inputs: Vec<Input>) -> Vec<u8> {
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        let mut pipeline = LzwPipeline::new().unwrap();
        for input in inputs {
            pipeline
                .submit(
                    &mut encoder,
                    input.rect,
                    input.indices,
                    input.start,
                    input.end,
                    &mut || true,
                )
                .unwrap();
            assert!(pipeline.in_flight <= MAX_IN_FLIGHT);
        }
        pipeline.finish(&mut encoder, &mut || true).unwrap();
        assert!(pipeline.workers.is_empty());
        encoder.into_inner().unwrap()
    }

    #[test]
    fn gif_lzw_pipeline_matches_sync_bytes_and_frame_order() {
        let mut inputs = (0..12)
            .map(|index| {
                input(
                    index,
                    Duration::from_millis(index as u64 * 70),
                    Duration::from_millis((index + 1) as u64 * 70),
                )
            })
            .collect::<Vec<_>>();
        inputs[7].rect = Rectangle {
            left: 1,
            top: 1,
            width: 2,
            height: 2,
        };
        inputs[7].indices.truncate(4);
        let baseline = synchronous(&inputs);
        let actual = pipelined(inputs.clone());
        assert_eq!(actual, baseline);
        let mut decoder = gif::DecodeOptions::new();
        decoder.set_color_output(gif::ColorOutput::Indexed);
        let mut reader = decoder.read_info(Cursor::new(&actual)).unwrap();
        for input in inputs {
            let frame = reader.read_next_frame().unwrap().unwrap();
            assert_eq!(frame.buffer.as_ref(), input.indices);
            assert_eq!(frame.left, input.rect.left);
            assert_eq!(frame.top, input.rect.top);
            assert_eq!(frame.dispose, gif::DisposalMethod::Keep);
            assert!(frame.palette.is_none());
        }
        assert!(reader.read_next_frame().unwrap().is_none());
    }

    #[test]
    fn gif_lzw_pipeline_preserves_absolute_15fps_timeline() {
        let time = |index: u64| Duration::from_nanos(index * 1_000_000_000 / 15);
        let inputs = (0..150)
            .map(|index| input(index, time(index as u64), time(index as u64 + 1)))
            .collect::<Vec<_>>();
        let actual = pipelined(inputs.clone());
        assert_eq!(actual, synchronous(&inputs));
        let mut reader = gif::DecodeOptions::new()
            .read_info(Cursor::new(actual))
            .unwrap();
        let mut delay = 0_u64;
        let mut frames = 0;
        while let Some(frame) = reader.read_next_frame().unwrap() {
            delay += u64::from(frame.delay);
            frames += 1;
        }
        assert_eq!((frames, delay), (150, 1000));
    }

    #[test]
    fn gif_lzw_long_delay_encodes_pixels_once_and_splits_65536cs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut pipeline = LzwPipeline::start(move |rect, indices| {
            count.fetch_add(1, Ordering::Relaxed);
            let mut frame = gif::Frame {
                width: rect.width,
                height: rect.height,
                left: rect.left,
                top: rect.top,
                dispose: gif::DisposalMethod::Keep,
                buffer: Cow::Owned(indices),
                ..gif::Frame::default()
            };
            frame.make_lzw_pre_encoded();
            frame
        })
        .unwrap();
        let input = input(0, Duration::ZERO, Duration::from_millis(655_360));
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        pipeline
            .submit(
                &mut encoder,
                input.rect,
                input.indices.clone(),
                input.start,
                input.end,
                &mut || true,
            )
            .unwrap();
        pipeline.finish(&mut encoder, &mut || true).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let bytes = encoder.into_inner().unwrap();
        assert_eq!(bytes, synchronous(&[input]));
        let mut reader = gif::DecodeOptions::new()
            .read_info(Cursor::new(bytes))
            .unwrap();
        let mut delays = Vec::new();
        while let Some(frame) = reader.read_next_frame().unwrap() {
            delays.push(frame.delay);
        }
        assert_eq!(delays, [65_534, 2]);
    }

    #[test]
    fn gif_lzw_pipeline_handles_one_frame_and_empty_worker() {
        let input = input(0, Duration::ZERO, Duration::from_millis(67));
        assert_eq!(pipelined(vec![input.clone()]), synchronous(&[input]));
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        let mut pipeline = LzwPipeline::new().unwrap();
        pipeline.finish(&mut encoder, &mut || true).unwrap();
        assert!(pipeline.workers.is_empty());
        assert!(pipeline.requests.is_none());
        assert!(pipeline.responses.is_none());
    }

    #[test]
    fn gif_lzw_cancel_and_drop_close_both_bounded_channels() {
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        let mut pipeline = LzwPipeline::new().unwrap();
        for index in 0..MAX_IN_FLIGHT {
            let input = input(index, Duration::ZERO, Duration::from_millis(20));
            pipeline
                .submit(
                    &mut encoder,
                    input.rect,
                    input.indices,
                    input.start,
                    input.end,
                    &mut || true,
                )
                .unwrap();
        }
        assert_eq!(
            pipeline
                .finish(&mut encoder, &mut || false)
                .unwrap_err()
                .to_string(),
            "已取消 GIF 转换"
        );
        // 满回复队列或 worker 尚在计算都必须可关闭；shutdown 正是 Drop 的路径。
        pipeline.shutdown().unwrap();
        assert!(pipeline.workers.is_empty());
        assert!(pipeline.requests.is_none());
        assert!(pipeline.responses.is_none());
    }

    #[test]
    fn gif_lzw_drop_releases_a_worker_at_the_full_response_queue() {
        let (finished, completion) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let (started, start) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel();
            let (third, ready) = std::sync::mpsc::channel();
            let mut count = 0;
            let mut pipeline = LzwPipeline::start(move |rect, indices| {
                count += 1;
                if count == 1 {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                }
                let mut frame = gif::Frame {
                    width: rect.width,
                    height: rect.height,
                    left: rect.left,
                    top: rect.top,
                    dispose: gif::DisposalMethod::Keep,
                    buffer: Cow::Owned(indices),
                    ..gif::Frame::default()
                };
                frame.make_lzw_pre_encoded();
                if count == 3 {
                    // 前两份回复已填满容量2的通道；第三份随后进入 send。
                    third.send(()).unwrap();
                }
                frame
            })
            .unwrap();
            let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
            for index in 0..MAX_IN_FLIGHT {
                let input = input(index, Duration::ZERO, Duration::from_millis(20));
                pipeline
                    .submit(
                        &mut encoder,
                        input.rect,
                        input.indices,
                        input.start,
                        input.end,
                        &mut || true,
                    )
                    .unwrap();
                if index == 0 {
                    start.recv().unwrap();
                }
            }
            assert_eq!(pipeline.in_flight, MAX_IN_FLIGHT);
            release.send(()).unwrap();
            ready.recv().unwrap();
            drop(pipeline);
            finished.send(()).unwrap();
        });
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("满回复队列的 GIF LZW worker 必须随 Drop 退出");
        owner.join().unwrap();
    }

    #[test]
    fn gif_lzw_worker_panic_is_an_error_and_worker_is_joined() {
        let mut pipeline = LzwPipeline::start(|_, _| panic!("injected LZW panic")).unwrap();
        let input = input(0, Duration::ZERO, Duration::from_millis(20));
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        pipeline
            .submit(
                &mut encoder,
                input.rect,
                input.indices,
                input.start,
                input.end,
                &mut || true,
            )
            .unwrap();
        assert!(pipeline
            .finish(&mut encoder, &mut || true)
            .unwrap_err()
            .to_string()
            .contains("panic"));
        pipeline.shutdown().unwrap();
        assert!(pipeline.workers.is_empty());
    }

    #[test]
    fn gif_lzw_pool_out_of_order_completion_publishes_original_bytes() {
        let (release, released) = std::sync::mpsc::channel();
        let released = Arc::new(Mutex::new(released));
        let (finished, completion) = std::sync::mpsc::channel();
        let (started, starts) = std::sync::mpsc::channel();
        let mut pipeline = LzwPipeline::start_factory(MAX_IN_FLIGHT, |_| {
            let released = released.clone();
            let finished = finished.clone();
            let started = started.clone();
            move |rect, indices| {
                let index = usize::from(indices[0]) / 13;
                started.send(index).unwrap();
                let frame = encode_frame(rect, indices);
                if index == 0 {
                    released.lock().unwrap().recv().unwrap();
                }
                finished.send(index).unwrap();
                frame
            }
        })
        .unwrap();
        let inputs = (0..MAX_IN_FLIGHT)
            .map(|index| {
                input(
                    index,
                    Duration::from_millis(index as u64 * 70),
                    Duration::from_millis((index + 1) as u64 * 70),
                )
            })
            .collect::<Vec<_>>();
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        for input in &inputs {
            pipeline
                .submit(
                    &mut encoder,
                    input.rect,
                    input.indices.clone(),
                    input.start,
                    input.end,
                    &mut || true,
                )
                .unwrap();
            // 每项已被工人取走才提交下一项，避免注入的首帧gate堵住满请求队列。
            if let Err(error) = starts.recv_timeout(Duration::from_secs(5)) {
                let _ = release.send(());
                panic!("工人未开始接收乱序模型任务：{error}");
            }
        }
        assert_eq!(pipeline.in_flight, MAX_IN_FLIGHT);
        let mut completed = Vec::new();
        for _ in 0..2 {
            match completion.recv_timeout(Duration::from_secs(5)) {
                Ok(index) => completed.push(index),
                Err(error) => {
                    let _ = release.send(());
                    panic!("工人未在第一帧等待时并行完成其它帧：{error}");
                }
            }
        }
        assert!(!completed.contains(&0));
        let mut checkpoints = 0;
        let mut released_first = false;
        pipeline
            .finish(&mut encoder, &mut || {
                checkpoints += 1;
                if checkpoints >= 3 && !released_first {
                    release.send(()).unwrap();
                    released_first = true;
                }
                true
            })
            .unwrap();
        completed.push(completion.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(completed[2], 0);
        assert!(pipeline.workers.is_empty());
        assert!(pipeline.pending.is_empty());
        assert_eq!(encoder.into_inner().unwrap(), synchronous(&inputs));
    }

    #[test]
    fn gif_lzw_pool_bounds_all_active_and_pending_frames_to_three() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (started, starts) = std::sync::mpsc::channel();
        let mut pipeline = LzwPipeline::start_factory(MAX_IN_FLIGHT, |_| {
            let gate = gate.clone();
            let active = active.clone();
            let peak = peak.clone();
            let started = started.clone();
            move |rect, indices| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                started.send(()).unwrap();
                let (lock, wake) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
                drop(open);
                let frame = encode_frame(rect, indices);
                active.fetch_sub(1, Ordering::SeqCst);
                frame
            }
        })
        .unwrap();
        let mut encoder = gif::Encoder::new(Vec::new(), 4, 3, &palette()).unwrap();
        for index in 0..MAX_IN_FLIGHT {
            let input = input(index, Duration::ZERO, Duration::from_millis(20));
            pipeline
                .submit(
                    &mut encoder,
                    input.rect,
                    input.indices,
                    input.start,
                    input.end,
                    &mut || true,
                )
                .unwrap();
            // 注入的工人会等gate；逐项确认取走请求后再提交，避免构造自身死锁。
            if let Err(error) = starts.recv_timeout(Duration::from_secs(5)) {
                let (lock, wake) = &*gate;
                *lock.lock().unwrap() = true;
                wake.notify_all();
                panic!("三个工人必须能在 recv 锁外同时处理任务：{error}");
            }
        }
        assert_eq!(pipeline.in_flight, MAX_IN_FLIGHT);
        assert_eq!(active.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        let fourth = input(MAX_IN_FLIGHT, Duration::ZERO, Duration::from_millis(20));
        let mut released = false;
        pipeline
            .submit(
                &mut encoder,
                fourth.rect,
                fourth.indices,
                fourth.start,
                fourth.end,
                &mut || {
                    if !released {
                        assert_eq!(active.load(Ordering::SeqCst), MAX_IN_FLIGHT);
                        let (lock, wake) = &*gate;
                        *lock.lock().unwrap() = true;
                        wake.notify_all();
                        released = true;
                    }
                    true
                },
            )
            .unwrap();
        assert!(released);
        assert_eq!(pipeline.in_flight, MAX_IN_FLIGHT);
        assert!(pipeline.pending.len() < MAX_IN_FLIGHT);
        pipeline.finish(&mut encoder, &mut || true).unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(pipeline.pending.is_empty());
    }

    #[test]
    fn gif_lzw_pool_cancel_and_writer_error_drop_join_every_worker() {
        struct WorkerLife(Arc<AtomicUsize>);
        impl Drop for WorkerLife {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct FailureWriter(Arc<std::sync::atomic::AtomicBool>);
        impl Write for FailureWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.load(Ordering::SeqCst) {
                    Err(std::io::Error::other("injected GIF write error"))
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for writer_error in [false, true] {
            let stopped = Arc::new(AtomicUsize::new(0));
            let mut pipeline = LzwPipeline::start_factory(MAX_IN_FLIGHT, |_| {
                let life = WorkerLife(stopped.clone());
                move |rect, indices| {
                    let _ = &life;
                    encode_frame(rect, indices)
                }
            })
            .unwrap();
            let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut encoder =
                gif::Encoder::new(FailureWriter(fail.clone()), 4, 3, &palette()).unwrap();
            for index in 0..MAX_IN_FLIGHT {
                let input = input(index, Duration::ZERO, Duration::from_millis(20));
                pipeline
                    .submit(
                        &mut encoder,
                        input.rect,
                        input.indices,
                        input.start,
                        input.end,
                        &mut || true,
                    )
                    .unwrap();
            }
            fail.store(writer_error, Ordering::SeqCst);
            let error = pipeline
                .finish(&mut encoder, &mut || writer_error)
                .unwrap_err()
                .to_string();
            if writer_error {
                assert!(error.contains("injected GIF write error"));
            } else {
                assert_eq!(error, "已取消 GIF 转换");
            }
            drop(pipeline);
            assert_eq!(stopped.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        }
    }
}
