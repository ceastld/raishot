//! 录屏结束策略：停止只结束采集，真实失败始终阻止成片发布。
use crate::{capture::cancellation::CaptureOutcome, Result};
use std::{
    error::Error,
    fmt,
    sync::atomic::{AtomicU64, AtomicU8, Ordering},
    time::Duration,
};

type Failure = Box<dyn Error>;
pub(super) type SecondaryErrors = Vec<(&'static str, Failure)>;

/// discard 请求与成功提交共享一个原子终态门，迟到请求不能重开已结束会话。
/// 门在 worker 清理和发布回执前关闭，不持锁执行文件系统或驱动调用。
pub(crate) struct DiscardFlag {
    state: AtomicU8,
}
impl DiscardFlag {
    const REQUESTED: u8 = 1;
    const SEALED: u8 = 2;

    pub(super) fn new(requested: bool) -> Self {
        let flag = Self {
            state: AtomicU8::new(0),
        };
        if requested {
            flag.store(true, Ordering::Relaxed);
        }
        flag
    }
    pub(crate) fn load(&self, ordering: Ordering) -> bool {
        self.state.load(ordering) & Self::REQUESTED != 0
    }
    /// 与原 bool 调用形状兼容；丢弃不可撤销，false 不复活会话。
    pub(crate) fn store(&self, requested: bool, ordering: Ordering) {
        if requested {
            let _ = self
                .state
                .fetch_update(ordering, Ordering::Relaxed, |state| {
                    (state & Self::SEALED == 0).then_some(state | Self::REQUESTED)
                });
        }
    }
    pub(super) fn seal(&self) -> bool {
        self.state.fetch_or(Self::SEALED, Ordering::AcqRel) & Self::REQUESTED != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FrameCommit {
    Written,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VideoSpan {
    pub time: Duration,
    pub duration: Duration,
}

/// 所有视频边界采用同一向下量化规则，且必须能表示为 MF 的有符号 100ns tick。
pub(super) fn video_time(time: Duration) -> Result<Duration> {
    let ticks = time.as_nanos() / 100;
    if ticks > i64::MAX as u128 {
        return Err(tr!(
            "录屏视频时间超出媒体时钟范围",
            "Recording video time exceeds the media clock range"
        )
        .into());
    }
    Ok(Duration::new(
        (ticks / 10_000_000) as u64,
        ((ticks % 10_000_000) * 100) as u32,
    ))
}

/// 只有正的已闭合跨度才能提交；不把零长尾部扩成一帧或一个 tick。
pub(super) fn video_span(start: Duration, end: Duration) -> Result<Option<VideoSpan>> {
    if end < start {
        return Err(tr!(
            "录屏视频边界发生倒序",
            "Recording video boundaries are out of order"
        )
        .into());
    }
    let time = video_time(start)?;
    let end = video_time(end)?;
    Ok((end > time).then(|| VideoSpan {
        time,
        duration: end - time,
    }))
}

pub(super) fn initial_capture<T>(capture: Result<CaptureOutcome<T>>) -> Result<T> {
    match capture? {
        CaptureOutcome::Ready(value) => Ok(value),
        CaptureOutcome::Cancelled => Err(no_frames()),
    }
}

/// 只有采集器明确返回 Cancelled 才跳过提交；提交器没有用户取消结果。
pub(super) fn after_capture<T>(
    capture: Result<CaptureOutcome<T>>,
    submit: impl FnOnce(T) -> Result<FrameCommit>,
) -> Result<FrameCommit> {
    match capture? {
        CaptureOutcome::Ready(value) => submit(value),
        CaptureOutcome::Cancelled => Ok(FrameCommit::Cancelled),
    }
}

/// 画面成功交给会话后才计 UI 数；成片零帧门另计已闭合并写入或安全排队的跨度。
/// 没有读取 stop 的二次错误解释。
pub(super) fn commit_frame(result: Result<FrameCommit>, frames: &AtomicU64) -> Result<bool> {
    match result? {
        FrameCommit::Written => {
            frames.fetch_add(1, Ordering::Relaxed);
            Ok(true)
        }
        FrameCommit::Cancelled => Ok(false),
    }
}

pub(super) fn no_frames() -> Failure {
    tr!("没有采集到画面", "No frames were captured").into()
}

#[derive(Debug)]
struct RecordingFailure {
    primary: Failure,
    secondary: SecondaryErrors,
}
impl fmt::Display for RecordingFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.primary.as_ref(), f)?;
        for (stage, error) in &self.secondary {
            write!(
                f,
                "{}",
                trf!(
                    "；{stage}另有错误：{error}",
                    "; also failed during {stage}: {error}"
                )
            )?;
        }
        Ok(())
    }
}
impl Error for RecordingFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary.as_ref())
    }
}

pub(super) fn with_secondary(primary: Failure, secondary: SecondaryErrors) -> Failure {
    if secondary.is_empty() {
        primary
    } else {
        Box::new(RecordingFailure { primary, secondary })
    }
}

pub(super) fn after_cleanup<T>(result: Result<T>, mut cleanup: SecondaryErrors) -> Result<T> {
    match result {
        Err(error) => Err(with_secondary(error, cleanup)),
        Ok(value) if cleanup.is_empty() => Ok(value),
        Ok(_) => {
            let (_, primary) = cleanup.remove(0);
            Err(with_secondary(primary, cleanup))
        }
    }
}

/// 仅供所有发布已完成的成功回执使用；副清理错误告警，原失败仍保留主错及 source。
pub(super) fn after_publication_cleanup<T>(
    result: Result<T>,
    cleanup: SecondaryErrors,
    mut warn: impl FnMut(String),
) -> Result<T> {
    match result {
        Err(error) => Err(with_secondary(error, cleanup)),
        Ok(value) => {
            for (stage, error) in cleanup {
                warn(trf!(
                    "录制输出已提交；{stage}未完成：{error}",
                    "Recording output was committed; {stage} did not complete: {error}"
                ));
            }
            Ok(value)
        }
    }
}

/// 顺序收集失败，保留首个原始对象；后续失败仅附加诊断。
#[derive(Default)]
pub(super) struct Failures {
    primary: Option<Failure>,
    secondary: SecondaryErrors,
}
impl Failures {
    pub(super) fn record<T>(&mut self, stage: &'static str, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                if self.primary.is_none() {
                    self.primary = Some(error);
                } else {
                    self.secondary.push((stage, error));
                }
                None
            }
        }
    }
    fn failed(&self) -> bool {
        self.primary.is_some()
    }
    pub(super) fn result<T>(self, value: T) -> Result<T> {
        match self.primary {
            Some(error) => Err(with_secondary(error, self.secondary)),
            None => Ok(value),
        }
    }
}

/// 实际录屏和纯模型共用收尾顺序。消费 video 防止失败后再次 Finalize。
pub(super) trait RecordingTail {
    type Tracks: Default;
    fn flush_audio(&mut self) -> Result<()>;
    fn finish_tracks(&mut self) -> Result<Self::Tracks>;
    fn finish_video(self) -> Result<()>;
}

pub(super) fn finalize_recording<T: RecordingTail>(
    body: Result<()>,
    frames: u64,
    mut tail: T,
) -> Result<T::Tracks> {
    let mut failures = Failures::default();
    failures.record(tr!("录制", "recording"), body);
    if !failures.failed() && frames == 0 {
        failures.record::<()>(tr!("录制", "recording"), Err(no_frames()));
    }
    // 原始失败或零帧之后不再扩写音频，但所有已有出口仍各收尾一次。
    if !failures.failed() {
        failures.record(tr!("尾部音频", "tail audio"), tail.flush_audio());
    }
    let tracks = failures
        .record(
            tr!("音轨收尾", "audio track finalization"),
            tail.finish_tracks(),
        )
        .unwrap_or_default();
    failures.record(tr!("MP4 收尾", "MP4 finalization"), tail.finish_video());
    failures.result(tracks)
}

/// 没有可收尾的视频出口时，仍显式关闭已有音轨；此路径始终失败且不扩写尾音。
pub(super) fn finalize_without_video<T>(
    body: Result<()>,
    finish_tracks: impl FnOnce() -> Result<T>,
) -> Failure {
    let mut failures = Failures::default();
    failures.record(tr!("录制", "recording"), body);
    if !failures.failed() {
        failures.record::<()>(
            tr!("编码器", "encoder"),
            Err(tr!("视频编码器不可用", "Video encoder unavailable").into()),
        );
    }
    failures.record(tr!("音轨收尾", "audio track finalization"), finish_tracks());
    match failures.result(()) {
        Err(error) => error,
        Ok(()) => unreachable!("缺少视频编码器不能发布成片"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc, sync::atomic::AtomicBool};

    #[test]
    fn recording_video_spans_quantize_shared_boundaries_without_overlap() {
        let start = Duration::from_nanos(127);
        let middle = Duration::from_nanos(33_333_399);
        let end = Duration::from_nanos(100_000_098);
        let first = video_span(start, middle).unwrap().unwrap();
        let second = video_span(middle, end).unwrap().unwrap();
        assert_eq!(first.time, Duration::from_nanos(100));
        assert_eq!(first.time + first.duration, second.time);
        assert_eq!(second.time + second.duration, video_time(end).unwrap());
        assert_eq!(first.duration.as_nanos() % 100, 0);
        assert_eq!(second.duration.as_nanos() % 100, 0);
    }

    #[test]
    fn recording_video_span_preserves_short_tail_and_skips_zero_ticks() {
        let start = Duration::from_nanos(1_000_000_037);
        let end = start + Duration::from_nanos(1_234);
        let span = video_span(start, end).unwrap().unwrap();
        assert_eq!(span.time, Duration::from_secs(1));
        assert_eq!(span.duration, Duration::from_nanos(1_200));
        assert_eq!(span.time + span.duration, video_time(end).unwrap());
        assert!(span.duration < Duration::from_secs_f64(1. / 30.));
        assert_eq!(video_span(end, end).unwrap(), None);
        assert_eq!(
            video_span(Duration::from_nanos(123), Duration::from_nanos(199)).unwrap(),
            None
        );
    }

    #[test]
    fn recording_video_span_rejects_reversal_and_signed_tick_overflow() {
        // 同一量化 tick 内发生倒序，也必须保留原始边界的契约错误。
        assert!(video_span(Duration::from_nanos(201), Duration::from_nanos(200)).is_err());
        let ticks = i64::MAX as u64;
        let maximum = Duration::new(ticks / 10_000_000, ((ticks % 10_000_000) * 100) as u32);
        assert_eq!(video_time(maximum).unwrap(), maximum);
        assert_eq!(
            video_time(maximum + Duration::from_nanos(99)).unwrap(),
            maximum
        );
        let overflow = maximum + Duration::from_nanos(100);
        assert!(video_time(overflow).is_err());
        assert!(video_span(Duration::ZERO, overflow).is_err());
    }

    #[derive(Debug)]
    struct TestFailure(&'static str);
    impl fmt::Display for TestFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl Error for TestFailure {}
    fn fail(message: &'static str) -> Failure {
        Box::new(TestFailure(message))
    }
    fn original<'a>(error: &'a (dyn Error + 'static)) -> &'a TestFailure {
        if let Some(original) = error.downcast_ref::<TestFailure>() {
            original
        } else {
            original(error.source().expect("应保留原始错误"))
        }
    }

    struct Tail {
        calls: Rc<RefCell<Vec<&'static str>>>,
        audio: Result<()>,
        tracks: Result<Vec<u8>>,
        video: Result<()>,
    }
    impl RecordingTail for Tail {
        type Tracks = Vec<u8>;
        fn flush_audio(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("audio");
            std::mem::replace(&mut self.audio, Ok(()))
        }
        fn finish_tracks(&mut self) -> Result<Vec<u8>> {
            self.calls.borrow_mut().push("tracks");
            std::mem::replace(&mut self.tracks, Ok(Vec::new()))
        }
        fn finish_video(self) -> Result<()> {
            self.calls.borrow_mut().push("video");
            self.video
        }
    }
    fn tail() -> Tail {
        Tail {
            calls: Rc::new(RefCell::new(Vec::new())),
            audio: Ok(()),
            tracks: Ok(vec![1, 2]),
            video: Ok(()),
        }
    }

    #[test]
    fn initial_capture_cancel_is_zero_frames_but_initial_failure_keeps_its_identity() {
        assert_eq!(initial_capture(Ok(CaptureOutcome::Ready(9))).unwrap(), 9);
        let error = initial_capture::<()>(Ok(CaptureOutcome::Cancelled)).unwrap_err();
        assert_eq!(error.to_string(), "没有采集到画面");
        let failure = fail("初始采集失败");
        let identity = failure.as_ref() as *const dyn Error as *const ();
        let stop = AtomicBool::new(true);
        let error = initial_capture::<()>(Err(failure)).unwrap_err();
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(error.as_ref() as *const dyn Error as *const (), identity);
    }

    #[test]
    fn discard_before_commit_is_accepted_and_late_discard_cannot_reopen_success() {
        let discard = DiscardFlag::new(false);
        discard.store(true, Ordering::Release);
        assert!(discard.seal());
        discard.store(false, Ordering::Release);
        assert!(discard.load(Ordering::Acquire));

        let discard = DiscardFlag::new(false);
        assert!(!discard.seal());
        discard.store(true, Ordering::Release);
        assert!(!discard.load(Ordering::Acquire));
        assert!(!discard.seal());
        assert!(DiscardFlag::new(true).seal());
    }

    #[test]
    fn discard_racing_terminal_commit_has_one_consistent_decision() {
        use std::sync::{Arc, Barrier};
        // 不模拟时间延迟：两端使用同一生产 CAS/终态门，允许任一端先成功。
        for _ in 0..32 {
            let discard = Arc::new(DiscardFlag::new(false));
            let barrier = Arc::new(Barrier::new(2));
            let request = discard.clone();
            let ready = barrier.clone();
            let worker = std::thread::spawn(move || {
                ready.wait();
                request.store(true, Ordering::Release);
            });
            barrier.wait();
            let accepted = discard.seal();
            worker.join().unwrap();
            assert_eq!(accepted, discard.load(Ordering::Acquire));
            assert_eq!(discard.seal(), accepted);
        }
    }

    #[test]
    fn capture_cancel_skips_submission_and_does_not_count_a_frame() {
        let frames = AtomicU64::new(3);
        let result = after_capture::<()>(Ok(CaptureOutcome::Cancelled), |_| {
            panic!("取消帧不能进入编码器")
        });
        assert!(!commit_frame(result, &frames).unwrap());
        assert_eq!(frames.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn first_and_last_capture_failure_remain_failures_when_stop_arrives() {
        for count in [0, 8] {
            let frames = AtomicU64::new(count);
            let stop = AtomicBool::new(false);
            let failure = fail("采集设备丢失");
            let identity = failure.as_ref() as *const dyn Error as *const ();
            stop.store(true, Ordering::Release);
            let result = after_capture::<()>(Err(failure), |_| panic!("失败不能提交"));
            let error = commit_frame(result, &frames).unwrap_err();
            assert!(stop.load(Ordering::Acquire));
            assert_eq!(error.as_ref() as *const dyn Error as *const (), identity);
            assert_eq!(frames.load(Ordering::Relaxed), count);
        }
    }

    #[test]
    fn gpu_and_cpu_write_failures_are_not_reclassified_by_stop() {
        for path in ["GPU 转换失败", "GPU 编码失败", "CPU 编码失败", "磁盘已满"] {
            for count in [0, 9] {
                let frames = AtomicU64::new(count);
                let stop = AtomicBool::new(false);
                let result = after_capture(Ok(CaptureOutcome::Ready(())), |_| {
                    stop.store(true, Ordering::Release);
                    Err(fail(path))
                });
                let error = commit_frame(result, &frames).unwrap_err();
                assert_eq!(original(error.as_ref()).0, path);
                assert_eq!(frames.load(Ordering::Relaxed), count);
                assert!(stop.load(Ordering::Acquire));
            }
        }
    }

    #[test]
    fn a_successful_last_write_is_counted_even_if_stop_arrives_during_write() {
        let frames = AtomicU64::new(4);
        let stop = AtomicBool::new(false);
        let result = after_capture(Ok(CaptureOutcome::Ready(())), |_| {
            stop.store(true, Ordering::Release);
            Ok(FrameCommit::Written)
        });
        assert!(commit_frame(result, &frames).unwrap());
        assert_eq!(frames.load(Ordering::Relaxed), 5);
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn explicit_last_frame_cancel_finalizes_previously_submitted_frames() {
        let tail = tail();
        let calls = tail.calls.clone();
        assert_eq!(finalize_recording(Ok(()), 3, tail).unwrap(), vec![1, 2]);
        assert_eq!(&*calls.borrow(), &["audio", "tracks", "video"]);
    }

    #[test]
    fn zero_frames_is_primary_even_when_finalize_also_fails() {
        let mut tail = tail();
        let calls = tail.calls.clone();
        tail.video = Err(fail("Finalize 失败"));
        let error = finalize_recording(Ok(()), 0, tail).unwrap_err();
        assert!(error.to_string().starts_with("没有采集到画面"));
        assert!(error.to_string().contains("Finalize 失败"));
        assert_eq!(&*calls.borrow(), &["tracks", "video"]);
    }

    #[test]
    fn body_failure_remains_primary_and_all_exits_finish_once() {
        for frames in [0, 7] {
            let mut tail = tail();
            let calls = tail.calls.clone();
            tail.tracks = Err(fail("WAV 收尾失败"));
            tail.video = Err(fail("Finalize 失败"));
            let error = finalize_recording(Err(fail("原始 GPU 失败")), frames, tail).unwrap_err();
            assert_eq!(original(error.as_ref()).0, "原始 GPU 失败");
            assert!(error.to_string().contains("WAV 收尾失败"));
            assert!(error.to_string().contains("Finalize 失败"));
            assert_eq!(&*calls.borrow(), &["tracks", "video"]);
        }
    }

    #[test]
    fn recording_unavailable_video_finishes_tracks_and_keeps_all_prior_failures() {
        let primary = fail("原始采集失败");
        let identity = primary.as_ref() as *const dyn Error as *const ();
        let mut failures = Failures::default();
        failures.record::<()>("录制", Err(primary));
        failures.record::<()>("编码器交接", Err(fail("初始化失败")));
        let mut finishes = 0;
        let error = finalize_without_video(failures.result(()), || -> Result<()> {
            finishes += 1;
            Err(fail("WAV 收尾失败"))
        });
        assert_eq!(finishes, 1);
        assert_eq!(
            original(error.as_ref()) as *const TestFailure as *const (),
            identity
        );
        assert!(error.to_string().starts_with("原始采集失败"));
        assert!(error.to_string().contains("初始化失败"));
        assert!(error.to_string().contains("WAV 收尾失败"));
    }

    #[test]
    fn recording_unavailable_video_cannot_succeed_after_successful_audio_finalization() {
        let mut finishes = 0;
        let error = finalize_without_video(Ok(()), || {
            finishes += 1;
            Ok(vec![1, 2])
        });
        assert_eq!(finishes, 1);
        assert!(error
            .to_string()
            .contains(tr!("视频编码器不可用", "Video encoder unavailable")));
    }

    #[test]
    fn stopping_does_not_hide_audio_track_or_finalize_failures() {
        for stage in ["audio", "tracks", "video"] {
            let mut tail = tail();
            let calls = tail.calls.clone();
            match stage {
                "audio" => tail.audio = Err(fail("尾音写入失败")),
                "tracks" => tail.tracks = Err(fail("WAV 失败")),
                _ => tail.video = Err(fail("Finalize 失败")),
            }
            assert!(finalize_recording(Ok(()), 4, tail).is_err());
            assert_eq!(&*calls.borrow(), &["audio", "tracks", "video"]);
        }
    }

    #[test]
    fn cleanup_failures_preserve_the_original_error_and_its_identity() {
        let error = fail("原始写盘失败");
        let identity = error.as_ref() as *const dyn Error as *const ();
        let error = with_secondary(error, vec![("删除暂存", fail("文件被占用"))]);
        assert_eq!(
            error.source().unwrap() as *const dyn Error as *const (),
            identity
        );
        assert!(error.to_string().starts_with("原始写盘失败"));
        assert!(error.to_string().contains("文件被占用"));
    }

    #[test]
    fn cleanup_failure_prevents_success_without_hiding_an_existing_failure() {
        assert_eq!(after_cleanup(Ok(7), Vec::new()).unwrap(), 7);
        let error = after_cleanup(Ok(7), vec![("清理", fail("清理失败"))]).unwrap_err();
        assert_eq!(original(error.as_ref()).0, "清理失败");
        let error = after_cleanup::<u8>(Err(fail("采集失败")), vec![("清理", fail("清理失败"))])
            .unwrap_err();
        assert_eq!(original(error.as_ref()).0, "采集失败");
        assert!(error.to_string().contains("清理失败"));
    }

    #[test]
    fn publication_cleanup_keeps_committed_value_and_warns_without_hiding_failure() {
        let value = Box::new(7);
        let identity = value.as_ref() as *const i32;
        let mut warnings = Vec::new();
        let committed = after_publication_cleanup(
            Ok(value),
            vec![
                ("删除暂存", fail("文件被占用")),
                ("删除目录", fail("目录非空")),
            ],
            |warning| warnings.push(warning),
        )
        .unwrap();
        assert_eq!(committed.as_ref() as *const i32, identity);
        assert_eq!(*committed, 7);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("删除暂存") && warnings[0].contains("文件被占用"));
        assert!(warnings[1].contains("删除目录") && warnings[1].contains("目录非空"));
        assert_eq!(
            after_publication_cleanup(Ok(9), Vec::new(), |_| panic!("无清理错误不能告警")).unwrap(),
            9
        );

        let primary = fail("原始写盘失败");
        let identity = primary.as_ref() as *const dyn Error as *const ();
        let error = after_publication_cleanup::<u8>(
            Err(primary),
            vec![
                ("删除暂存", fail("文件被占用")),
                ("删除目录", fail("目录非空")),
            ],
            |_| panic!("失败不能发出已提交警告"),
        )
        .unwrap_err();
        assert_eq!(
            error.source().unwrap() as *const dyn Error as *const (),
            identity
        );
        assert!(error.to_string().starts_with("原始写盘失败"));
        for detail in ["删除暂存", "文件被占用", "删除目录", "目录非空"] {
            assert!(error.to_string().contains(detail));
        }
    }
}
