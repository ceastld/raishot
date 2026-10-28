//! 录屏停止后的框内预览：窗口覆盖原选区，后台线程用系统解码器逐帧解码，
//! 按播放速度呈现；旧帧在新帧完成绘制后才释放，避免 atlas 纹理提前失效。
use crate::{
    audio_capture::{AudioSource, MixLevels},
    geometry::ScreenRect,
    gpui_capture::hwnd,
    media_foundation::{MfRuntime, VideoReader},
    media_playback_sync::{
        qpc_now_100ns, FrameObservation, FrameStage, PlaybackObserver, SyncScenario, WorkerExit,
    },
    pin_model::Placement,
    preview_audio::{AudioStreamSelection, PreviewAudio},
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, App, Bounds, Context, MouseButton, Render,
    RenderImage, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::POINT,
    UI::{
        Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

pub(crate) use crate::preview_audio::{AudioProcessing, PitchMode, ResampleQuality};

pub(crate) const RATES: [f32; 5] = [0.5, 1., 1.25, 1.5, 2.];

enum Command {
    Play(u64),
    Pause(u64),
    Seek(Duration, u64),
    Rate(f32, u64),
    Step(Option<FramePoint>, u64),
    AudioStream(AudioStreamSelection, u64),
}

#[derive(Default)]
struct Status {
    /// 单槽邮箱：消费者移走像素，未消费的旧帧可由新帧直接替换。
    frame: Option<(u64, Duration, Vec<u8>)>,
    position: Duration,
    duration: Duration,
    size: (u32, u32),
    playing: bool,
    error: Option<String>,
    info: Option<crate::media_foundation::VideoInfo>,
    generation: u64,
    pending_seek: Option<Duration>,
    pending_step: bool,
    scenario: SyncScenario,
    frame_generation: u64,
    frame_scenario: SyncScenario,
    frame_history: VecDeque<(u64, FramePoint)>,
    last_taken: Option<FramePoint>,
    last_submitted: Option<(u64, FramePoint)>,
}

/// 关闭时立即发出取消；系统单次读取在后台结束，界面不等待解码线程。
pub(crate) struct Player {
    commands: Option<Sender<Command>>,
    status: Arc<Mutex<Status>>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    worker_exit: WorkerExit,
    pub rate: f32,
    /// 分轨或媒体文件的内嵌声音，由视频循环同步控制。
    audio: Option<Arc<PreviewAudio>>,
    pub audio_error: Option<String>,
    sync: Mutex<PlaybackObserver>,
}

/// 只观察后台退出标记，不保留播放器、媒体或窗口资源。
#[derive(Clone)]
pub(crate) struct PlaybackShutdownProbe {
    video: WorkerExit,
    audio: Option<WorkerExit>,
}
impl PlaybackShutdownProbe {
    pub(crate) fn video_exited(&self) -> bool {
        self.video.is_exited()
    }
    pub(crate) fn audio_exited(&self) -> bool {
        self.audio.as_ref().is_none_or(WorkerExit::is_exited)
    }
    pub(crate) fn finished(&self) -> bool {
        self.video_exited() && self.audio_exited()
    }
}

impl Player {
    pub(crate) fn open(
        path: PathBuf,
        tracks: &[(AudioSource, PathBuf)],
        levels: MixLevels,
    ) -> Self {
        let (audio, audio_error) = match PreviewAudio::open(tracks, levels) {
            Ok(audio) => (audio, None),
            Err(error) => (
                None,
                Some(trf!(
                    "预览声音不可用：{error}",
                    "Preview audio unavailable: {error}"
                )),
            ),
        };
        Self::start(path, audio, audio_error, true)
    }
    pub(crate) fn open_file(path: PathBuf) -> Self {
        Self::open_file_with_audio_settings(path, 1., false)
    }
    pub(crate) fn open_file_with_audio_settings(path: PathBuf, volume: f32, muted: bool) -> Self {
        Self::open_file_with_initial_transport(path, volume, muted, true)
    }
    pub(crate) fn open_file_paused_with_audio_settings(
        path: PathBuf,
        volume: f32,
        muted: bool,
    ) -> Self {
        Self::open_file_with_initial_transport(path, volume, muted, false)
    }
    fn open_file_with_initial_transport(
        path: PathBuf,
        volume: f32,
        muted: bool,
        initial_playing: bool,
    ) -> Self {
        let (audio, error) = match PreviewAudio::open_file(path.clone()) {
            Ok(audio) => {
                audio.set_volume(volume);
                audio.set_muted(muted);
                (Some(audio), None)
            }
            Err(error) => (None, Some(error.to_string())),
        };
        Self::start(path, audio, error, initial_playing)
    }
    fn start(
        path: PathBuf,
        audio: Option<PreviewAudio>,
        audio_error: Option<String>,
        initial_playing: bool,
    ) -> Self {
        let audio = audio.map(Arc::new);
        let worker_audio = audio.clone();
        let (sender, receiver) = channel();
        let status = Arc::new(Mutex::new(Status {
            playing: initial_playing,
            ..Default::default()
        }));
        let shared = status.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_exit = WorkerExit::pending();
        let video_exit = worker_exit.clone();
        let spawned = std::thread::Builder::new()
            .name("rustshot-recording-preview".into())
            .spawn(move || {
                let _exit = video_exit.guard();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    decode_loop(
                        &path,
                        &receiver,
                        &shared,
                        &worker_stop,
                        worker_audio.as_deref(),
                    )
                }));
                // 原生读取失败已在循环内按代次处理；此处只收尾会话关闭或工作者意外失败。
                if let Some(audio) = worker_audio.as_deref() {
                    audio.stop();
                }
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(_) => Some(
                        tr!("视频解码线程意外停止", "Video decoder stopped unexpectedly").into(),
                    ),
                };
                if let Some(error) = error {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if !worker_stop.load(Ordering::Acquire) {
                        s.playing = false;
                        s.pending_seek = None;
                        s.pending_step = false;
                        s.error = Some(error);
                    }
                }
            });
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(error) => {
                worker_exit.mark_exited();
                status.lock().unwrap().error = Some(trf!(
                    "无法启动视频解码：{error}",
                    "Cannot start video decoder: {error}"
                ));
                if let Some(audio) = audio.as_deref() {
                    audio.stop();
                }
                None
            }
        };
        let _ = sender.send(if initial_playing {
            Command::Play(0)
        } else {
            Command::Pause(0)
        });
        Self {
            commands: Some(sender),
            status,
            thread,
            stop,
            worker_exit,
            rate: 1.,
            audio,
            audio_error,
            sync: Mutex::new(PlaybackObserver::default()),
        }
    }
    pub(crate) fn shutdown_probe(&self) -> PlaybackShutdownProbe {
        PlaybackShutdownProbe {
            video: self.worker_exit.clone(),
            audio: self.audio.as_ref().map(|audio| audio.worker_exit()),
        }
    }
    /// 旧结果窗读取字段；通用查看器通过方法随时读取异步错误。
    fn start_audio_with_video(&mut self) {
        self.audio_error = self.audio_error();
    }
    pub(crate) fn audio_sources(&self) -> Vec<AudioSource> {
        self.audio
            .as_ref()
            .map_or_else(Vec::new, |a| a.sources.clone())
    }
    pub(crate) fn set_levels(&self, levels: MixLevels) {
        if let Some(audio) = &self.audio {
            audio.set_levels(levels);
        }
    }
    fn send(&self, command: Command) {
        let mut status = self.status.lock().unwrap();
        self.send_locked(command, &mut status);
    }
    fn send_locked(&self, command: Command, status: &mut Status) {
        status.generation = status.generation.wrapping_add(1);
        let generation = status.generation;
        let command = match command {
            Command::Play(_) => {
                status.scenario = if status.position.is_zero() {
                    SyncScenario::Start
                } else {
                    SyncScenario::Resume
                };
                status.playing = true;
                Command::Play(generation)
            }
            Command::Pause(_) => {
                status.playing = false;
                Command::Pause(generation)
            }
            Command::Rate(rate, _) => {
                status.scenario = SyncScenario::RateChange;
                Command::Rate(rate, generation)
            }
            Command::Seek(target, _) => {
                status.frame = None;
                status.pending_seek = Some(target);
                status.pending_step = false;
                status.scenario = SyncScenario::Seek;
                Command::Seek(target, generation)
            }
            Command::Step(_, _) => {
                status.playing = false;
                status.pending_step = true;
                status.scenario = SyncScenario::Seek;
                Command::Step(
                    status
                        .last_submitted
                        .map(|(_, point)| point)
                        .or(status.last_taken),
                    generation,
                )
            }
            Command::AudioStream(selection, _) => {
                status.scenario = SyncScenario::Seek;
                Command::AudioStream(selection, generation)
            }
        };
        // 控制代次与音频暂停在同一状态锁内发布，旧解码返回后不能再启动旧音。
        if !matches!(command, Command::Play(_)) {
            if let Some(audio) = &self.audio {
                if let Command::AudioStream(selection, _) = command {
                    audio.select_file_stream(
                        generation,
                        selection,
                        status.pending_seek.unwrap_or(status.position),
                        false,
                        self.rate,
                    );
                } else {
                    let (target, playing, rate) = audio_control(&command, status, self.rate);
                    audio.set_transport(generation, target, playing, rate);
                }
            }
        }
        if let Some(sender) = &self.commands {
            let _ = sender.send(command);
        }
    }
    pub(crate) fn toggle(&mut self) {
        let (playing, at_end) = {
            let status = self.status.lock().unwrap();
            (
                status.playing,
                status.duration > Duration::ZERO
                    && status.pending_seek.is_none()
                    && !status.pending_step
                    && status.position + Duration::from_millis(80) >= status.duration,
            )
        };
        if playing {
            self.send(Command::Pause(0));
        } else {
            if at_end {
                self.seek(Duration::ZERO);
            }
            self.send(Command::Play(0));
        }
    }
    /// 幂等设置播放意图，不在片尾隐式重播，供进度拖动的暂停/恢复事务使用。
    pub(crate) fn set_playing(&mut self, playing: bool) {
        if self.playing() != playing {
            self.send(if playing {
                Command::Play(0)
            } else {
                Command::Pause(0)
            });
        }
    }
    pub(crate) fn seek_ratio(&self, ratio: f32) {
        if !ratio.is_finite() {
            return;
        }
        let duration = self.status.lock().unwrap().duration;
        let target = duration.mul_f32(ratio.clamp(0., 1.));
        self.seek(target);
    }
    fn seek(&self, target: Duration) {
        self.send(Command::Seek(target, 0));
    }
    /// 暂停并前进到下一个实际解码帧；尚未完成的连续请求合并为一步。
    pub(crate) fn step_forward(&mut self) {
        self.send(Command::Step(None, 0));
    }
    pub(crate) fn set_rate(&mut self, rate: f32) {
        if !rate.is_finite() {
            return;
        }
        self.rate = rate.clamp(0.1, 4.);
        self.send(Command::Rate(self.rate, 0));
    }
    pub(crate) fn set_volume(&self, value: f32) {
        if let Some(audio) = &self.audio {
            audio.set_volume(value);
        }
    }
    pub(crate) fn set_muted(&self, muted: bool) {
        if let Some(audio) = &self.audio {
            audio.set_muted(muted);
        }
    }
    pub(crate) fn audio_processing(&self) -> AudioProcessing {
        self.audio
            .as_ref()
            .map_or(AudioProcessing::default(), |audio| audio.processing())
    }
    pub(crate) fn set_audio_processing(&self, settings: AudioProcessing) -> Result<()> {
        let Some(audio) = &self.audio else {
            return Err(tr!(
                "此媒体没有可处理的音轨",
                "This media has no audio track to process"
            )
            .into());
        };
        let mut status = self.status.lock().unwrap();
        let recovery = audio_processing_change(&status, audio.error().is_some(), || {
            audio.set_processing(settings)
        })?;
        if let Some(command) = recovery {
            // 错误期间画面已按墙钟继续；新 UI 代先隔离旧时钟的视频提交，再沿用 seek 恢复。
            // 持 Status 锁不使两次音频调用原子化，但旧代结果不能覆盖这次恢复目标。
            self.send_locked(command, &mut status);
        }
        Ok(())
    }
    pub(crate) fn audio_error(&self) -> Option<String> {
        self.audio
            .as_ref()
            .map_or_else(|| self.audio_error.clone(), |audio| audio.error())
    }
    pub(crate) fn video_info(&self) -> Option<crate::media_foundation::VideoInfo> {
        self.status.lock().unwrap().info.clone()
    }
    /// None 表示仍在探测；空列表表示没有音轨。
    pub(crate) fn audio_info(&self) -> Option<Vec<(u32, usize)>> {
        self.audio
            .as_ref()
            .map_or_else(|| Some(Vec::new()), |audio| audio.audio_info())
    }
    pub(crate) fn audio_selection(&self) -> Option<AudioStreamSelection> {
        self.audio.as_ref().and_then(|audio| audio.file_selection())
    }
    pub(crate) fn audio_switching(&self) -> bool {
        self.audio
            .as_ref()
            .is_some_and(|audio| audio.stream_switching())
    }
    /// 真实源流索引；None 禁用文件声音，录屏分轨混音不接受此操作。
    pub(crate) fn select_audio_stream(&self, index: Option<u32>) {
        if self.audio_selection().is_some() {
            self.send(Command::AudioStream(
                index.map_or(AudioStreamSelection::Disabled, AudioStreamSelection::Stream),
                0,
            ));
        }
    }
    /// 恢复文件默认音轨；流枚举未完整成功时仍允许重新尝试，分轨混音不接受。
    pub(crate) fn select_default_audio_stream(&self) {
        if self.audio_selection().is_some() {
            self.send(Command::AudioStream(AudioStreamSelection::First, 0));
        }
    }
    pub(crate) fn playing(&self) -> bool {
        self.status.lock().unwrap().playing
    }
    pub(crate) fn seeking(&self) -> bool {
        let status = self.status.lock().unwrap();
        status.pending_seek.is_some() || status.pending_step
    }
    pub(crate) fn position(&self) -> (Duration, Duration) {
        let status = self.status.lock().unwrap();
        (status.position, status.duration)
    }
    pub(crate) fn size(&self) -> (u32, u32) {
        self.status.lock().unwrap().size
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.status.lock().unwrap().error.clone()
    }
    pub(crate) fn take_frame(&self) -> Option<(u64, Vec<u8>)> {
        self.take_frame_with_time()
            .map(|(serial, _, pixels)| (serial, pixels))
    }
    /// 时间戳与像素来自同一发布帧，与之后推进的播放时钟无关。
    pub(crate) fn take_frame_with_time(&self) -> Option<(u64, Duration, Vec<u8>)> {
        let (frame, generation, scenario, current_transport) = {
            let mut s = self.status.lock().unwrap();
            let frame = s.frame.take()?;
            s.last_taken = Some(
                s.frame_history
                    .iter()
                    .rev()
                    .find(|(serial, _)| *serial == frame.0)
                    .map_or(
                        FramePoint {
                            time: frame.1,
                            repeated: 0,
                        },
                        |(_, point)| *point,
                    ),
            );
            let data = (frame, s.frame_generation, s.frame_scenario, s.generation);
            if s.frame_generation == s.generation {
                s.scenario = SyncScenario::Steady;
            }
            data
        };
        if let Some(qpc_100ns) = qpc_now_100ns() {
            let clock = self
                .audio
                .as_ref()
                .and_then(|audio| audio.clock_at(qpc_100ns));
            self.sync.lock().unwrap().taken(
                frame.0,
                FrameObservation {
                    transport_generation: generation,
                    pts: frame.1,
                    qpc_100ns,
                    stage: FrameStage::Taken,
                    scenario,
                },
                current_transport,
                clock,
            );
        }
        Some(frame)
    }
    /// 场景绘制提交回执；这不是显示器已呈现的证据。
    pub(crate) fn observe_submitted(&self, serial: u64) {
        let qpc = qpc_now_100ns();
        let generation = {
            let mut s = self.status.lock().unwrap();
            if let Some((_, point)) = s
                .frame_history
                .iter()
                .rev()
                .find(|(id, _)| *id == serial)
                .copied()
            {
                if s.last_submitted
                    .is_none_or(|(previous, _)| serial.wrapping_sub(previous) < 1_u64 << 63)
                {
                    s.last_submitted = Some((serial, point));
                }
            }
            s.generation
        };
        let Some(qpc) = qpc else {
            return;
        };
        let clock = self.audio.as_ref().and_then(|audio| audio.clock_at(qpc));
        self.sync
            .lock()
            .unwrap()
            .submitted(serial, qpc, generation, clock);
    }
    /// 已收到场景提交回执的同一帧身份；读取不取走像素，也不补造回执。
    pub(crate) fn submitted_frame_point(&self) -> Option<(u64, Duration)> {
        self.status
            .lock()
            .unwrap()
            .last_submitted
            .map(|(serial, point)| (serial, point.time))
    }

    pub(crate) fn sync_report(&self) -> serde_json::Value {
        self.sync.lock().unwrap().report()
    }
    #[cfg(test)]
    pub(crate) fn test_transport_generation(&self) -> u64 {
        self.status.lock().unwrap().generation
    }
    #[cfg(test)]
    pub(crate) fn audio_clock_at(
        &self,
        qpc_100ns: u64,
    ) -> Option<crate::preview_audio_device_clock::PublishedClock> {
        let status = self.status.lock().unwrap();
        self.audio.as_ref()?.clock_at(qpc_100ns).filter(|clock| {
            !self.stop.load(Ordering::Acquire) && clock.transport_generation == status.generation
        })
    }
}

fn audio_processing_change(
    status: &Status,
    audio_failed: bool,
    apply: impl FnOnce() -> Result<()>,
) -> Result<Option<Command>> {
    let recover = audio_failed && status.error.is_none();
    if recover && status.pending_step {
        return Err(tr!(
            "逐帧定位进行中，请稍后重试",
            "Frame seeking is in progress; try again shortly"
        )
        .into());
    }
    // 参数错误不改变传输；无音频错误或视频已失败时只保存设置。
    apply()?;
    Ok(recover.then(|| Command::Seek(status.pending_seek.unwrap_or(status.position), 0)))
}

fn audio_control(command: &Command, status: &Status, rate: f32) -> (Option<Duration>, bool, f32) {
    match command {
        Command::Seek(target, _) => (Some(*target), false, rate),
        Command::Rate(value, _) => (
            status.pending_seek,
            status.playing && status.pending_seek.is_none() && !status.pending_step,
            *value,
        ),
        _ => (status.pending_seek, false, rate),
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(audio) = &self.audio {
            audio.stop();
        }
        self.commands.take();
        self.thread.take();
    }
}

/// 等到下一帧的媒体时刻；无待呈现帧时立即读下一帧，命令通道可随时唤醒。
fn playback_wait(
    playing: bool,
    pending_time: Option<Duration>,
    media_now: Duration,
    rate: f32,
) -> Duration {
    const IDLE: Duration = Duration::from_millis(200);
    if !playing {
        return IDLE;
    }
    pending_time.map_or(Duration::ZERO, |time| {
        time.saturating_sub(media_now)
            .div_f64(f64::from(rate))
            .min(IDLE)
    })
}

/// 播放落后时只呈现本批最后一个到期帧；未来帧留给下一次呈现。
fn take_due_frame(pending: &mut Option<QueuedFrame>, media_now: Duration) -> Option<QueuedFrame> {
    if pending
        .as_ref()
        .is_some_and(|frame| frame.point.time <= media_now)
    {
        pending.take()
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FramePoint {
    time: Duration,
    repeated: u64,
}

#[derive(Default)]
struct FrameOrder(Option<FramePoint>);
impl FrameOrder {
    fn track(&mut self, time: Duration) -> FramePoint {
        let repeated = self
            .0
            .filter(|previous| previous.time == time)
            .map_or(0, |previous| previous.repeated.saturating_add(1));
        let point = FramePoint { time, repeated };
        self.0 = Some(point);
        point
    }
}

struct QueuedFrame {
    decoded: crate::media_foundation::DecodedFrame,
    point: FramePoint,
}

fn next_frame(
    reader: &mut VideoReader,
    stop: &AtomicBool,
    order: &mut FrameOrder,
) -> Result<Option<QueuedFrame>> {
    Ok(reader.next_cancellable(stop)?.map(|decoded| QueuedFrame {
        point: order.track(decoded.time),
        decoded,
    }))
}

#[derive(Clone, Copy)]
enum SeekAim {
    At(Duration),
    After(FramePoint),
}
impl SeekAim {
    fn target(self) -> Duration {
        match self {
            Self::At(time) => time,
            Self::After(point) => point.time,
        }
    }
}

/// 最多保留目标前的一帧；遇到首个未来帧后将它交还顺序播放。
struct SeekFrames {
    aim: SeekAim,
    before: Option<QueuedFrame>,
}
struct SeekResult {
    frame: Option<QueuedFrame>,
    pending: Option<QueuedFrame>,
    position: Duration,
    ended: bool,
}

enum StepStart {
    After(FramePoint),
    First(QueuedFrame),
    Empty,
}

fn step_start(
    shown: Option<FramePoint>,
    requested: Option<FramePoint>,
    pending: &mut Option<QueuedFrame>,
) -> StepStart {
    if let Some(shown) = shown {
        StepStart::After(requested.unwrap_or(shown))
    } else {
        pending.take().map_or(StepStart::Empty, StepStart::First)
    }
}

impl SeekFrames {
    fn new(aim: SeekAim) -> Self {
        Self { aim, before: None }
    }
    fn accept(&mut self, frame: Option<QueuedFrame>) -> Option<SeekResult> {
        let target = self.aim.target();
        let Some(frame) = frame else {
            return Some(SeekResult {
                frame: self.before.take(),
                pending: None,
                position: target,
                ended: true,
            });
        };
        match self.aim {
            SeekAim::At(_) if frame.point.time <= target => {
                self.before = Some(frame);
                None
            }
            SeekAim::At(_) => Some(SeekResult {
                frame: self.before.take(),
                pending: Some(frame),
                position: target,
                ended: false,
            }),
            SeekAim::After(point)
                if frame.point.time < point.time
                    || frame.point.time == point.time && frame.point.repeated <= point.repeated =>
            {
                None
            }
            SeekAim::After(_) => Some(SeekResult {
                position: frame.point.time,
                frame: Some(frame),
                pending: None,
                ended: false,
            }),
        }
    }
}

enum NativeResult<T> {
    Ready(T),
    Cancelled,
    Superseded,
    Failed,
}

/// 成功结果先留在工作线程本地；失败只能在所属代次内改变公开状态。
fn resolve_native_result<T>(
    result: Result<T>,
    status: &mut Status,
    stop: &AtomicBool,
    generation: u64,
) -> NativeResult<T> {
    if stop.load(Ordering::Acquire) {
        return NativeResult::Cancelled;
    }
    match result {
        Ok(value) => NativeResult::Ready(value),
        Err(_) if status.generation != generation => NativeResult::Superseded,
        Err(error) => {
            status.playing = false;
            status.pending_seek = None;
            status.pending_step = false;
            status.error = Some(error.to_string());
            NativeResult::Failed
        }
    }
}

#[cfg(test)]
fn publish_frame(
    status: &Mutex<Status>,
    stop: &AtomicBool,
    generation: u64,
    serial: u64,
    frame: crate::media_foundation::DecodedFrame,
) -> bool {
    let mut s = status.lock().unwrap();
    store_frame(&mut s, stop, generation, serial, frame, None)
}

fn store_frame(
    s: &mut Status,
    stop: &AtomicBool,
    generation: u64,
    serial: u64,
    frame: crate::media_foundation::DecodedFrame,
    point: Option<FramePoint>,
) -> bool {
    if s.generation == generation && !stop.load(Ordering::Acquire) {
        let point = point.unwrap_or(FramePoint {
            time: frame.time,
            repeated: 0,
        });
        if s.frame_history.len() == 16 {
            s.frame_history.pop_front();
        }
        s.frame_history.push_back((serial, point));
        s.frame = Some((serial, frame.time, frame.bgra));
        s.frame_generation = generation;
        s.frame_scenario = s.scenario;
        return true;
    }
    false
}

fn publish_queued(
    status: &Mutex<Status>,
    stop: &AtomicBool,
    generation: u64,
    serial: &mut u64,
    shown: &mut Option<FramePoint>,
    frame: QueuedFrame,
) {
    *serial = serial.wrapping_add(1);
    let mut s = status.lock().unwrap();
    if store_frame(
        &mut s,
        stop,
        generation,
        *serial,
        frame.decoded,
        Some(frame.point),
    ) {
        *shown = Some(frame.point);
    }
}

#[derive(Clone, Copy)]
enum AudioTime {
    Hold,
    Position(Duration),
    Fallback(Option<Duration>),
}

fn audio_time(audio: Option<&PreviewAudio>, generation: u64) -> AudioTime {
    let Some(audio) = audio else {
        return AudioTime::Fallback(None);
    };
    if audio.file_selection() == Some(AudioStreamSelection::Disabled)
        || audio.error().is_some()
        || audio.audio_info().is_some_and(|tracks| tracks.is_empty())
    {
        return AudioTime::Fallback(None);
    }
    let Some(published) = audio
        .clock()
        .filter(|clock| clock.transport_generation == generation)
    else {
        return AudioTime::Hold;
    };
    if published.clock.phase == crate::preview_audio_clock::ClockPhase::Ended {
        AudioTime::Fallback(Some(published.clock.media_position))
    } else {
        AudioTime::Position(published.clock.media_position)
    }
}

struct PlaybackClock {
    origin: Duration,
    anchor: Instant,
    position: Duration,
    using_audio: bool,
    audio_ended: bool,
}
impl PlaybackClock {
    fn new(position: Duration) -> Self {
        Self {
            origin: position,
            anchor: Instant::now(),
            position,
            using_audio: false,
            audio_ended: false,
        }
    }
    fn reset(&mut self, position: Duration) {
        self.origin = position;
        self.position = position;
        self.anchor = Instant::now();
    }
    fn seek(&mut self, position: Duration) {
        self.audio_ended = false;
        self.reset(position);
    }
    fn now(&mut self, source: AudioTime, playing: bool, rate: f32) -> Duration {
        let source = if self.audio_ended {
            AudioTime::Fallback(None)
        } else {
            source
        };
        self.position = match source {
            AudioTime::Position(position) => {
                self.using_audio = true;
                position
            }
            AudioTime::Hold => {
                self.using_audio = true;
                self.position
            }
            AudioTime::Fallback(position) => {
                if let Some(position) = position {
                    self.audio_ended = true;
                    self.origin = position;
                    self.anchor = Instant::now();
                    self.using_audio = false;
                } else if self.using_audio {
                    self.origin = position.unwrap_or(self.position);
                    self.anchor = Instant::now();
                    self.using_audio = false;
                }
                if playing {
                    self.origin + self.anchor.elapsed().mul_f32(rate)
                } else {
                    self.origin
                }
            }
        };
        self.position
    }
}

fn decode_loop(
    path: &std::path::Path,
    commands: &Receiver<Command>,
    status: &Mutex<Status>,
    stop: &AtomicBool,
    audio: Option<&PreviewAudio>,
) -> Result<()> {
    let mut runtime: Option<MfRuntime> = None;
    let mut reader: Option<VideoReader> = None;
    let mut opened_once = false;
    let mut failed_generation = None;
    let mut serial = 0_u64;
    let mut shown = None;
    let mut order = FrameOrder::default();
    let mut clock = PlaybackClock::new(Duration::ZERO);
    let mut opening_frame: Option<QueuedFrame> = None;
    let mut pending: Option<QueuedFrame> = None;
    let mut playing = false;
    let mut rate = 1_f32;
    let mut generation = 0;
    let mut seeking: Option<SeekFrames> = None;
    let mut seek_ready: Option<SeekResult> = None;
    let mut step_requested = false;
    let mut step_base = None;
    let mut video_ended = false;
    'decode: loop {
        // 原生错误在循环内同代收口；失败代等待新命令，新代用新 reader 明确重试一次。
        macro_rules! native {
            ($operation:expr) => {{
                let result = $operation;
                let outcome = {
                    let mut s = status.lock().unwrap();
                    let outcome = resolve_native_result(result, &mut s, stop, generation);
                    if matches!(&outcome, NativeResult::Failed) {
                        clock.reset(s.position);
                        if let Some(audio) = audio {
                            audio.set_transport(generation, Some(s.position), false, rate);
                        }
                    }
                    outcome
                };
                match outcome {
                    NativeResult::Ready(value) => value,
                    NativeResult::Cancelled => return Ok(()),
                    NativeResult::Superseded => {
                        reader = None;
                        continue 'decode;
                    }
                    NativeResult::Failed => {
                        reader = None;
                        playing = false;
                        failed_generation = Some(generation);
                        continue 'decode;
                    }
                }
            }};
        }
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let media_now = clock.now(audio_time(audio, generation), playing, rate);
        {
            let mut s = status.lock().unwrap();
            if s.generation == generation && seeking.is_none() {
                s.position = media_now;
            }
        }
        let timeout = if failed_generation == Some(generation) {
            playback_wait(false, None, media_now, rate)
        } else if reader.is_none() || seeking.is_some() || step_requested {
            Duration::ZERO
        } else {
            playback_wait(
                playing,
                pending
                    .as_ref()
                    .map(|frame| frame.point.time)
                    .or_else(|| video_ended.then(|| reader.as_ref().unwrap().duration)),
                media_now,
                rate,
            )
        }
        .max(if video_ended && clock.using_audio {
            Duration::from_millis(10)
        } else {
            Duration::ZERO
        });
        let mut next_command = match commands.recv_timeout(timeout) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let mut changed = false;
        let mut requested_seek = None;
        let mut changed_audio_stream = false;
        // 连续拖动只为本批最后一个目标执行系统 seek，不堆积关键帧追赶。
        while let Some(command) = next_command {
            clock.now(audio_time(audio, generation), playing, rate);
            clock.reset(clock.position);
            match command {
                Command::Play(next_generation) => {
                    playing = true;
                    generation = next_generation;
                }
                Command::Pause(next_generation) => {
                    playing = false;
                    generation = next_generation;
                }
                Command::Rate(value, next_generation) => {
                    rate = value;
                    generation = next_generation;
                }
                Command::Seek(target, next_generation) => {
                    generation = next_generation;
                    clock.seek(target);
                    requested_seek = Some(target);
                    step_requested = false;
                    step_base = None;
                }
                Command::Step(base, next_generation) => {
                    playing = false;
                    generation = next_generation;
                    step_requested = requested_seek.is_some()
                        || !matches!(
                            seeking.as_ref().map(|seek| seek.aim),
                            Some(SeekAim::After(_))
                        );
                    if step_requested {
                        step_base = if requested_seek.is_some() || seeking.is_some() {
                            None
                        } else {
                            base
                        };
                    }
                }
                Command::AudioStream(_, next_generation) => {
                    generation = next_generation;
                    changed_audio_stream = true;
                }
            }
            changed = true;
            next_command = commands.try_recv().ok();
        }
        if failed_generation == Some(generation) {
            continue;
        }
        let mut recovery_aim = None;
        if reader.is_none() {
            {
                let mut s = status.lock().unwrap();
                if s.generation != generation || stop.load(Ordering::Acquire) {
                    continue;
                }
                if opened_once {
                    let aim = requested_seek
                        .map(SeekAim::At)
                        .or_else(|| s.pending_seek.map(SeekAim::At))
                        .or_else(|| seeking.as_ref().map(|seek| seek.aim))
                        .unwrap_or(SeekAim::At(clock.position));
                    match aim {
                        SeekAim::At(target) => s.pending_seek = Some(target),
                        SeekAim::After(_) => s.pending_step = true,
                    }
                    recovery_aim = Some(aim);
                }
                if let Some(audio) = audio {
                    audio.set_transport(
                        generation,
                        Some(s.pending_seek.unwrap_or(clock.position)),
                        false,
                        rate,
                    );
                }
            }
            if runtime.is_none() {
                runtime = Some(native!(MfRuntime::new()));
            }
            let opened = native!(VideoReader::open(path));
            {
                let mut s = status.lock().unwrap();
                if s.generation != generation || stop.load(Ordering::Acquire) {
                    continue;
                }
                s.duration = opened.duration;
                s.size = (opened.width, opened.height);
                s.info = Some(opened.info.clone());
                s.error = None;
            }
            reader = Some(opened);
            opened_once = true;
            failed_generation = None;
            order = FrameOrder::default();
            pending = None;
            opening_frame = None;
            seek_ready = None;
            video_ended = false;
            if requested_seek.is_none() && recovery_aim.is_none() {
                let result =
                    next_frame(reader.as_mut().unwrap(), stop, &mut order).and_then(|frame| {
                        frame.ok_or_else(|| {
                            tr!(
                                "文件中没有可播放的视频帧",
                                "No playable video frames in this file"
                            )
                            .into()
                        })
                    });
                let first = native!(result);
                clock.reset(first.point.time);
                opening_frame = Some(first);
            }
            changed = true;
        }
        if let Some(aim) = requested_seek.map(SeekAim::At).or(recovery_aim) {
            let target = aim.target();
            if let Some(audio) = audio {
                audio.set_transport(generation, Some(target), false, rate);
            }
            seeking = Some(SeekFrames::new(aim));
            seek_ready = None;
            native!(reader.as_mut().unwrap().seek(target));
            order = FrameOrder::default();
            pending = None;
            opening_frame = None;
            video_ended = false;
            changed = true;
        }
        if changed {
            let mut s = status.lock().unwrap();
            if s.generation != generation {
                continue;
            }
            if changed_audio_stream {
                // 新轨可能长于已结束的旧轨；重新允许设备成为主时钟，位置保持当前目标。
                clock.seek(s.pending_seek.unwrap_or(clock.position));
            }
            if let Some(audio) = audio {
                audio.set_transport(
                    generation,
                    requested_seek
                        .or(s.pending_seek)
                        .or_else(|| changed_audio_stream.then_some(clock.position)),
                    playing && seeking.is_none() && !step_requested,
                    rate,
                );
            }
            s.playing = playing;
            s.position = s.pending_seek.unwrap_or(clock.position);
        }
        // 初次解码期间的暂停/变速/逐帧命令也应有首帧；显式 seek 才替换它。
        if opening_frame.is_some() {
            let mut s = status.lock().unwrap();
            if s.generation == generation && !stop.load(Ordering::Acquire) {
                let first = opening_frame.take().unwrap();
                serial = serial.wrapping_add(1);
                if store_frame(
                    &mut s,
                    stop,
                    generation,
                    serial,
                    first.decoded,
                    Some(first.point),
                ) {
                    shown = Some(first.point);
                }
            } else {
                continue;
            }
        }
        // 解码结果先保存在本地；暂停/变速换代只禁止旧发布，不丢掉已读回的目标帧。
        if let Some(seek) = seeking.as_mut() {
            if seek_ready.is_none() {
                let frame = native!(next_frame(reader.as_mut().unwrap(), stop, &mut order));
                seek_ready = seek.accept(frame);
            }
            let mut s = status.lock().unwrap();
            if s.generation != generation || stop.load(Ordering::Acquire) {
                continue;
            }
            let Some(mut result) = seek_ready.take() else {
                continue;
            };
            let is_step = matches!(seek.aim, SeekAim::After(_));
            if !is_step && result.frame.is_none() {
                // 可保留界面旧图，但它不再是新目标的逐帧基点。
                shown = None;
                step_base = None;
            }
            if !is_step && step_requested && result.frame.is_none() {
                if let Some(next) = result.pending.take() {
                    // 目标前没有画面时，显式逐帧应落到首个实际帧，不能沿用旧画面的步进基点。
                    result.position = next.point.time;
                    result.frame = Some(next);
                    step_requested = false;
                }
            }
            let position = if is_step && result.frame.is_none() {
                clock.position
            } else {
                result.position
            };
            clock.seek(position);
            pending = result.pending;
            video_ended = result.ended;
            seeking = None;
            if !s.duration.is_zero() && position >= s.duration {
                playing = false;
            }
            s.position = position;
            s.playing = playing;
            s.pending_seek = None;
            s.pending_step = step_requested;
            if let Some(audio) = audio {
                audio.set_transport(generation, Some(position), playing && !step_requested, rate);
            }
            if let Some(frame) = result.frame {
                serial = serial.wrapping_add(1);
                if store_frame(
                    &mut s,
                    stop,
                    generation,
                    serial,
                    frame.decoded,
                    Some(frame.point),
                ) {
                    shown = Some(frame.point);
                }
            }
            continue;
        }
        if step_requested {
            step_requested = false;
            match step_start(shown, step_base.take(), &mut pending) {
                StepStart::After(point) => {
                    // 从当前画面的真实时间戳和重复序号定位，不以标称帧率估算下一帧。
                    seeking = Some(SeekFrames::new(SeekAim::After(point)));
                    seek_ready = None;
                    native!(reader.as_mut().unwrap().seek(point.time));
                    order = FrameOrder::default();
                    pending = None;
                    video_ended = false;
                }
                StepStart::First(frame) => {
                    let mut s = status.lock().unwrap();
                    if s.generation != generation || stop.load(Ordering::Acquire) {
                        pending = Some(frame);
                        step_requested = true;
                        continue;
                    }
                    let point = frame.point;
                    clock.seek(point.time);
                    s.position = point.time;
                    s.playing = playing;
                    s.pending_step = false;
                    if let Some(audio) = audio {
                        audio.set_transport(generation, Some(point.time), playing, rate);
                    }
                    serial = serial.wrapping_add(1);
                    if store_frame(&mut s, stop, generation, serial, frame.decoded, Some(point)) {
                        shown = Some(point);
                    }
                }
                StepStart::Empty => {
                    let mut s = status.lock().unwrap();
                    if s.generation == generation {
                        s.pending_step = false;
                    }
                }
            }
            continue;
        }
        if !playing {
            continue;
        }
        let catch_up = Instant::now();
        let mut newest_due = None;
        let mut ended = video_ended;
        // 有限追赶保持命令响应；解码本身仍严格顺序，丢弃的只是过时显示帧。
        for _ in 0..16 {
            if video_ended {
                break;
            }
            if pending.is_none() {
                pending = native!(next_frame(reader.as_mut().unwrap(), stop, &mut order));
                if pending.is_none() {
                    ended = true;
                    video_ended = true;
                    break;
                }
            }
            let media_now = clock.now(audio_time(audio, generation), playing, rate);
            let Some(frame) = take_due_frame(&mut pending, media_now) else {
                break;
            };
            newest_due = Some(frame);
            if catch_up.elapsed() >= Duration::from_millis(8) {
                break;
            }
        }
        if let Some(frame) = newest_due {
            publish_queued(status, stop, generation, &mut serial, &mut shown, frame);
        }
        // 保留最后一帧直到容器时长，避免把视频末帧之后的音频尾段截掉。
        let media_now = clock.now(audio_time(audio, generation), playing, rate);
        if ended && !clock.using_audio && media_now >= reader.as_ref().unwrap().duration {
            let mut s = status.lock().unwrap();
            if s.generation != generation {
                continue;
            }
            playing = false;
            s.playing = false;
            s.position = s.duration.max(s.position);
            clock.reset(s.position);
        } else {
            let mut s = status.lock().unwrap();
            if s.generation == generation {
                let position = media_now;
                s.position = if s.duration.is_zero() {
                    position
                } else {
                    position.min(s.duration)
                };
            }
        }
    }
}

/// 预览窗口移动后的新左上角（物理像素），由录屏宿主同步边框与工具栏。
pub(crate) type MoveHandler = Rc<dyn Fn((i32, i32), &mut App)>;

/// 按下时的光标与窗口原点；超过阈值才算拖动，否则松开时切换播放。
#[derive(Clone, Copy)]
struct Drag {
    cursor: (i32, i32),
    origin: (i32, i32),
    moved: bool,
}

/// 拖动阈值（物理像素）内的移动仍算点击。
fn drag_target(drag: Drag, cursor: (i32, i32), threshold: i32) -> Option<(i32, i32)> {
    let (dx, dy) = (cursor.0 - drag.cursor.0, cursor.1 - drag.cursor.1);
    (drag.moved || dx.abs() > threshold || dy.abs() > threshold)
        .then_some((drag.origin.0 + dx, drag.origin.1 + dy))
}

fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }
        .ok()
        .map(|()| (point.x, point.y))
}

/// 覆盖选区的预览窗口；点击切换播放/暂停，拖动移动整个结果窗口。
pub(crate) struct RecordingPreview {
    pub player: Player,
    image: Option<(u64, Arc<RenderImage>)>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    placement: Placement,
    drag: Option<Drag>,
    on_move: MoveHandler,
}
impl RecordingPreview {
    pub(crate) fn open(
        region: ScreenRect,
        path: PathBuf,
        tracks: Vec<(AudioSource, PathBuf)>,
        levels: MixLevels,
        on_move: MoveHandler,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let placement = Placement {
            x: region.left,
            y: region.top,
            width: region.width,
            height: region.height,
        };
        let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                focus: false,
                // 首帧解码前保持透明，露出原选区，不先闪一段黑屏。
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(brand_title!("录屏预览", "Recording preview"));
                let h = hwnd(window);
                unsafe {
                    SetWindowLongPtrW(h, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                    SetWindowLongPtrW(
                        h,
                        GWL_EXSTYLE,
                        GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_TOOLWINDOW.0 as isize,
                    );
                    let _ = SetWindowPos(
                        h,
                        Some(HWND_TOPMOST),
                        placement.x,
                        placement.y,
                        placement.width as i32,
                        placement.height as i32,
                        SWP_SHOWWINDOW | SWP_FRAMECHANGED | SWP_NOACTIVATE,
                    );
                }
                let view = cx.new(|_| Self {
                    player: Player::open(path, &tracks, levels),
                    image: None,
                    retiring: Vec::new(),
                    retired: Vec::new(),
                    placement,
                    drag: None,
                    on_move,
                });
                let weak = view.downgrade();
                cx.spawn(async move |cx| loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(15))
                        .await;
                    let Ok(changed) = weak.update(cx, |v, _| v.pull()) else {
                        break;
                    };
                    if changed && weak.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                })
                .detach();
                view
            },
        )?)
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.image.is_some()
    }

    fn begin_drag(&mut self, window: &Window) {
        let Some(cursor) = cursor_position() else {
            return;
        };
        let h = hwnd(window);
        unsafe {
            SetCapture(h);
            if GetCapture() != h {
                return;
            }
        }
        self.drag = Some(Drag {
            cursor,
            origin: (self.placement.x, self.placement.y),
            moved: false,
        });
    }

    fn drag_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut drag) = self.drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left)
            || unsafe { GetCapture() } != hwnd(window)
        {
            self.end_drag(window);
            return true;
        }
        let threshold = (3. * window.scale_factor()).ceil() as i32;
        let Some(origin) = cursor_position().and_then(|c| drag_target(drag, c, threshold)) else {
            return true;
        };
        drag.moved = true;
        self.drag = Some(drag);
        if origin != (self.placement.x, self.placement.y) {
            self.placement.x = origin.0;
            self.placement.y = origin.1;
            crate::pin_native::place_later(hwnd(window), self.placement, None, false, cx);
            // 宿主窗口在本窗口借用结束后再同步，避免跨窗口重入。
            let on_move = self.on_move.clone();
            cx.defer(move |cx| on_move(origin, cx));
        }
        true
    }

    /// 返回本次按下是否只是点击（未拖动）。
    fn end_drag(&mut self, window: &Window) -> bool {
        let clicked = self.drag.take().is_some_and(|drag| !drag.moved);
        unsafe {
            if GetCapture() == hwnd(window) {
                let _ = ReleaseCapture();
            }
        }
        clicked
    }

    /// 取最新解码帧；返回是否需要重绘。
    fn pull(&mut self) -> bool {
        if self.image.is_some() {
            self.player.start_audio_with_video();
        }
        let Some((serial, bgra)) = self.player.take_frame() else {
            return false;
        };
        if self.image.as_ref().is_some_and(|(s, _)| *s == serial) {
            return false;
        }
        let (width, height) = self.player.size();
        let Some(pixels) = image::RgbaImage::from_raw(width, height, bgra) else {
            return false;
        };
        let next = Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]));
        if let Some((_, previous)) = self.image.replace((serial, next)) {
            self.retiring.push(previous);
        }
        true
    }
}
impl Render for RecordingPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 上一帧场景引用的纹理在本次绘制后才失效：两段式退休。
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        let scale = window.scale_factor();
        let view = cx.entity().downgrade();
        let mut root = div()
            .id("recording-preview")
            .size_full()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(|v, _, w, _| v.begin_drag(w)))
            // 捕获期间光标可能短暂离开窗口，移动和松开都在窗口级接收。
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        let weak = view.clone();
                        window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture {
                                let _ = weak.update(cx, |v, cx| {
                                    if v.drag_move(e, w, cx) {
                                        cx.stop_propagation();
                                    }
                                });
                            }
                        });
                        let weak = view.clone();
                        window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, w, cx| {
                            if phase == gpui::DispatchPhase::Capture
                                && e.button == MouseButton::Left
                            {
                                let _ = weak.update(cx, |v, cx| {
                                    if v.drag.is_some() && v.end_drag(w) {
                                        v.player.toggle();
                                        cx.notify();
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        if let Some((serial, image)) = &self.image {
            // 按解码帧物理尺寸 1:1 绘制：布局取整会把画面拉伸 1px，整帧双线性重采样后文字发虚。
            let serial = *serial;
            let image = image.clone();
            let weak = cx.entity().downgrade();
            let frame = image.size(0);
            let bounds = Bounds::new(
                point(px(0.), px(0.)),
                size(
                    px(frame.width.0 as f32 / scale),
                    px(frame.height.0 as f32 / scale),
                ),
            );
            root = root.bg(rgb(0x000000)).child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        if window
                            .paint_image_nearest_exact(
                                bounds,
                                px(0.).into(),
                                image.clone(),
                                0,
                                false,
                            )
                            .is_ok()
                        {
                            let _ = weak.update(cx, |view, _| {
                                if view.image.as_ref().is_some_and(|(current, pixels)| {
                                    *current == serial && Arc::ptr_eq(pixels, &image)
                                }) {
                                    view.player.observe_submitted(serial);
                                }
                            });
                        }
                    },
                )
                .size_full(),
            );
        } else if let Some(error) = self.player.error() {
            root = root.bg(rgb(0x000000)).child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0xffffff))
                    .text_size(px(13.))
                    .child(trf!(
                        "当前系统无法预览此视频：{error}",
                        "This system can't preview the video: {error}"
                    )),
            );
        }
        root
    }
}
impl Drop for RecordingPreview {
    fn drop(&mut self) {
        self.image.take();
    }
}

#[cfg(test)]
#[path = "recording_preview_audio_probe.rs"]
mod audio_probe;
#[cfg(test)]
#[path = "recording_preview_window_smoke.rs"]
mod window_smoke;

#[cfg(test)]
#[test]
#[ignore = "显式打开真实录屏结果窗，以自造静音双WAV验证传输和关闭释放"]
fn native_recording_preview_window_silent_wav_transport_and_close_smoke() -> Result<()> {
    window_smoke::run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_foundation::DecodedFrame;

    #[test]
    fn preview_shutdown_probe_waits_for_both_workers_and_allows_no_audio() {
        let video = WorkerExit::pending();
        let audio = WorkerExit::pending();
        let probe = PlaybackShutdownProbe {
            video: video.clone(),
            audio: Some(audio.clone()),
        };
        let copy = probe.clone();
        let silent = PlaybackShutdownProbe {
            video: video.clone(),
            audio: None,
        };
        assert!(silent.audio_exited());
        assert!(!silent.finished());
        {
            let _audio_exit = audio.guard();
            {
                let _video_exit = video.guard();
                assert!(!probe.video_exited());
                assert!(!probe.audio_exited());
            }
            assert!(probe.video_exited());
            assert!(silent.finished());
            assert!(!copy.finished());
        }
        assert!(probe.finished());
        assert!(copy.finished());
    }
    #[test]
    fn preview_drag_starts_after_threshold_then_follows_cursor() {
        let drag = Drag {
            cursor: (500, 300),
            origin: (100, 80),
            moved: false,
        };
        assert_eq!(drag_target(drag, (503, 297), 4), None);
        assert_eq!(drag_target(drag, (505, 300), 4), Some((105, 80)));
        assert_eq!(drag_target(drag, (480, 340), 4), Some((80, 120)));
        let moved = Drag {
            moved: true,
            ..drag
        };
        // 已开始拖动后回到阈值内也继续跟随，不再当作点击。
        assert_eq!(drag_target(moved, (501, 300), 4), Some((101, 80)));
    }

    #[test]
    fn preview_wait_follows_frame_deadline_and_playback_rate() {
        let now = Duration::from_millis(100);
        let next = Some(Duration::from_millis(150));
        assert_eq!(
            playback_wait(true, next, now, 1.),
            Duration::from_millis(50)
        );
        assert_eq!(
            playback_wait(true, next, now, 2.),
            Duration::from_millis(25)
        );
        assert_eq!(
            playback_wait(true, next, now, 0.5),
            Duration::from_millis(100)
        );
        assert_eq!(playback_wait(true, None, now, 1.), Duration::ZERO);
        assert_eq!(
            playback_wait(true, Some(Duration::from_millis(80)), now, 1.),
            Duration::ZERO
        );
        assert_eq!(
            playback_wait(false, next, now, 1.),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn preview_master_clock_holds_during_audio_priming_and_underrun() {
        let mut clock = PlaybackClock::new(Duration::from_secs(1));
        clock.now(AudioTime::Position(Duration::from_secs(5)), true, 2.);
        assert_eq!(clock.now(AudioTime::Hold, true, 2.), Duration::from_secs(5));
        assert!(clock.using_audio);
        assert_eq!(
            clock.now(AudioTime::Position(Duration::from_millis(5050)), false, 1.),
            Duration::from_millis(5050)
        );
    }

    #[test]
    fn preview_seek_followed_by_rate_or_pause_keeps_audio_target_held() {
        let target = Duration::from_secs(12);
        let status = Status {
            playing: true,
            pending_seek: Some(target),
            ..Default::default()
        };
        assert_eq!(
            audio_control(&Command::Rate(2., 2), &status, 1.),
            (Some(target), false, 2.)
        );
        assert_eq!(
            audio_control(&Command::Pause(3), &status, 2.),
            (Some(target), false, 2.)
        );
        let ready = Status {
            playing: true,
            ..Default::default()
        };
        assert_eq!(
            audio_control(&Command::Rate(2., 4), &ready, 1.),
            (None, true, 2.)
        );
        let stepping = Status {
            playing: true,
            pending_step: true,
            ..Default::default()
        };
        assert_eq!(
            audio_control(&Command::Rate(2., 5), &stepping, 1.),
            (None, false, 2.)
        );
    }

    #[test]
    fn preview_audio_processing_recovery_seeks_current_target_without_changing_play_intent() {
        for (playing, pending_seek) in [
            (true, None),
            (false, None),
            (true, Some(Duration::from_secs(12))),
        ] {
            let (sender, receiver) = channel();
            let player = Player {
                commands: Some(sender),
                status: Arc::new(Mutex::new(Status {
                    generation: 7,
                    position: Duration::from_secs(8),
                    playing,
                    pending_seek,
                    ..Default::default()
                })),
                thread: None,
                stop: Arc::new(AtomicBool::new(false)),
                worker_exit: WorkerExit::completed(),
                rate: 1.,
                audio: None,
                audio_error: None,
                sync: Mutex::new(PlaybackObserver::default()),
            };
            let target = pending_seek.unwrap_or(Duration::from_secs(8));
            let mut status = player.status.lock().unwrap();
            let command = audio_processing_change(&status, true, || Ok(()))
                .unwrap()
                .unwrap();
            // 即使同一处理设置的 apply 是无变化成功，Seek 新代也会触发 actor 显式重试。
            player.send_locked(command, &mut status);
            assert!(matches!(
                receiver.try_recv().unwrap(),
                Command::Seek(at, 8) if at == target
            ));
            assert_eq!(status.generation, 8);
            assert_eq!(status.playing, playing);
            assert_eq!(status.pending_seek, Some(target));
            assert!(!status.pending_step);
            drop(status);
        }
    }

    #[test]
    fn preview_audio_processing_change_preserves_normal_transport_and_failed_video() {
        for (audio_failed, video_error, pending_step) in [
            (false, None, false),
            (false, None, true),
            (true, Some("video failed".to_string()), false),
        ] {
            let status = Status {
                generation: 7,
                position: Duration::from_secs(8),
                playing: video_error.is_none(),
                error: video_error.clone(),
                pending_step,
                ..Default::default()
            };
            let mut applied = false;
            assert!(audio_processing_change(&status, audio_failed, || {
                applied = true;
                Ok(())
            })
            .unwrap()
            .is_none());
            assert!(applied);
            assert_eq!(status.generation, 7);
            assert_eq!(status.position, Duration::from_secs(8));
            assert_eq!(status.playing, video_error.is_none());
            assert_eq!(status.error, video_error);
            assert_eq!(status.pending_step, pending_step);
        }
    }

    #[test]
    fn preview_audio_processing_invalid_or_stepping_recovery_has_no_transport_effect() {
        let mut status = Status {
            generation: 7,
            position: Duration::from_secs(8),
            playing: false,
            pending_seek: Some(Duration::from_secs(12)),
            ..Default::default()
        };
        assert!(audio_processing_change(&status, true, || Err("invalid settings".into())).is_err());
        status.pending_step = true;
        assert!(audio_processing_change(&status, true, || {
            panic!("逐帧恢复被拒绝时不能更改音频设置")
        })
        .is_err());
        assert_eq!(status.generation, 7);
        assert_eq!(status.position, Duration::from_secs(8));
        assert_eq!(status.pending_seek, Some(Duration::from_secs(12)));
        assert!(status.pending_step);
        assert!(!status.playing);
    }

    #[test]
    fn preview_toggle_during_seek_at_eof_keeps_the_requested_target() {
        let (sender, receiver) = channel();
        let mut player = Player {
            commands: Some(sender),
            status: Arc::new(Mutex::new(Status {
                position: Duration::from_secs(10),
                duration: Duration::from_secs(10),
                pending_seek: Some(Duration::from_secs(5)),
                ..Default::default()
            })),
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            worker_exit: WorkerExit::completed(),
            rate: 1.,
            audio: None,
            audio_error: None,
            sync: Mutex::new(PlaybackObserver::default()),
        };
        player.toggle();
        assert!(matches!(receiver.try_recv().unwrap(), Command::Play(1)));
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            player.status.lock().unwrap().pending_seek,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn preview_step_uses_submitted_frame_even_when_a_newer_frame_was_taken() {
        let (sender, receiver) = channel();
        let mut player = Player {
            commands: Some(sender),
            status: Arc::new(Mutex::new(Status::default())),
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            worker_exit: WorkerExit::completed(),
            rate: 1.,
            audio: None,
            audio_error: None,
            sync: Mutex::new(PlaybackObserver::default()),
        };
        let mut order = FrameOrder::default();
        let mut serial = 0;
        let mut shown = None;
        for millis in [0, 40, 40, 120] {
            let frame = seek_frame(&mut order, millis, 1);
            publish_queued(
                &player.status,
                &player.stop,
                0,
                &mut serial,
                &mut shown,
                frame,
            );
            player.take_frame_with_time();
        }
        player.observe_submitted(3);
        player.step_forward();
        let expected = FramePoint {
            time: Duration::from_millis(40),
            repeated: 1,
        };
        assert!(
            matches!(receiver.try_recv().unwrap(), Command::Step(Some(point), 1) if point == expected)
        );
        player.observe_submitted(4);
        player.observe_submitted(3);
        player.step_forward();
        assert!(
            matches!(receiver.try_recv().unwrap(), Command::Step(Some(point), 2) if point.time == Duration::from_millis(120))
        );
    }

    #[test]
    fn preview_master_clock_falls_back_from_last_actual_audio_position() {
        let mut clock = PlaybackClock::new(Duration::ZERO);
        clock.now(AudioTime::Position(Duration::from_secs(5)), true, 1.);
        assert_eq!(
            clock.now(AudioTime::Fallback(None), false, 1.),
            Duration::from_secs(5)
        );
        assert!(!clock.using_audio);
        assert_eq!(
            clock.now(AudioTime::Fallback(None), false, 2.),
            Duration::from_secs(5)
        );
        clock.reset(Duration::from_secs(2));
        assert_eq!(
            clock.now(AudioTime::Fallback(None), false, 1.),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn preview_master_clock_finishes_audio_before_continuing_video_tail() {
        let mut clock = PlaybackClock::new(Duration::ZERO);
        clock.now(AudioTime::Position(Duration::from_millis(2990)), true, 1.);
        assert_eq!(
            clock.now(AudioTime::Fallback(Some(Duration::from_secs(3))), false, 1.),
            Duration::from_secs(3)
        );
        assert!(!clock.using_audio);
    }

    #[test]
    fn preview_master_clock_does_not_reenter_short_audio_on_tail_controls() {
        let mut clock = PlaybackClock::new(Duration::ZERO);
        clock.now(AudioTime::Position(Duration::from_secs(2)), true, 1.);
        clock.now(AudioTime::Fallback(Some(Duration::from_secs(2))), false, 1.);
        clock.reset(Duration::from_millis(2500));
        for source in [
            AudioTime::Hold,
            AudioTime::Position(Duration::from_secs(2)),
            AudioTime::Fallback(Some(Duration::from_secs(2))),
        ] {
            assert_eq!(clock.now(source, false, 2.), Duration::from_millis(2500));
            assert!(!clock.using_audio);
        }
        clock.seek(Duration::from_secs(1));
        assert_eq!(
            clock.now(AudioTime::Position(Duration::from_secs(1)), false, 1.),
            Duration::from_secs(1)
        );
        assert!(clock.using_audio);
    }

    #[test]
    fn preview_catch_up_keeps_latest_due_frame_and_preserves_future_frame() {
        let now = Duration::from_millis(95);
        let mut latest = None;
        let mut order = FrameOrder::default();
        for time in [0, 33, 66, 99] {
            let time = Duration::from_millis(time);
            let mut pending = Some(QueuedFrame {
                decoded: DecodedFrame {
                    time,
                    bgra: vec![1; 4],
                },
                point: order.track(time),
            });
            if let Some(frame) = take_due_frame(&mut pending, now) {
                latest = Some(frame);
                assert!(pending.is_none());
            } else {
                assert_eq!(
                    pending.as_ref().unwrap().point.time,
                    Duration::from_millis(99)
                );
            }
        }
        assert_eq!(latest.unwrap().point.time, Duration::from_millis(66));
    }

    fn seek_frame(order: &mut FrameOrder, millis: u64, pixel: u8) -> QueuedFrame {
        let time = Duration::from_millis(millis);
        QueuedFrame {
            decoded: DecodedFrame {
                time,
                bgra: vec![pixel; 4],
            },
            point: order.track(time),
        }
    }

    #[test]
    fn preview_precise_seek_uses_vfr_floor_and_preserves_future_frame() {
        let mut seek = SeekFrames::new(SeekAim::At(Duration::from_millis(95)));
        let mut order = FrameOrder::default();
        assert!(seek.accept(Some(seek_frame(&mut order, 0, 1))).is_none());
        assert!(seek.accept(Some(seek_frame(&mut order, 40, 2))).is_none());
        let ready = seek.accept(Some(seek_frame(&mut order, 120, 3))).unwrap();
        assert_eq!(ready.position, Duration::from_millis(95));
        assert_eq!(ready.frame.unwrap().decoded.bgra, [2; 4]);
        assert_eq!(
            ready.pending.unwrap().point.time,
            Duration::from_millis(120)
        );
        assert!(!ready.ended);
    }

    #[test]
    fn preview_obsolete_native_error_preserves_new_seek_and_current_failure_is_reported() {
        let target = Duration::from_secs(12);
        let mut status = Status {
            generation: 8,
            playing: true,
            pending_seek: Some(target),
            pending_step: true,
            position: target,
            ..Default::default()
        };
        let stop = AtomicBool::new(false);
        assert!(matches!(
            resolve_native_result::<()>(Err("obsolete read failed".into()), &mut status, &stop, 7),
            NativeResult::Superseded
        ));
        assert_eq!(status.generation, 8);
        assert!(status.playing);
        assert_eq!(status.pending_seek, Some(target));
        assert!(status.pending_step);
        assert_eq!(status.position, target);
        assert!(status.error.is_none());

        assert!(matches!(
            resolve_native_result::<()>(Err("new reader failed".into()), &mut status, &stop, 8),
            NativeResult::Failed
        ));
        assert!(!status.playing);
        assert!(status.pending_seek.is_none());
        assert!(!status.pending_step);
        assert_eq!(status.error.as_deref(), Some("new reader failed"));

        stop.store(true, Ordering::Release);
        assert!(matches!(
            resolve_native_result::<()>(Err("closed read failed".into()), &mut status, &stop, 8),
            NativeResult::Cancelled
        ));
        assert_eq!(status.error.as_deref(), Some("new reader failed"));
    }

    #[test]
    fn preview_precise_seek_handles_leading_gap_and_eof() {
        let mut order = FrameOrder::default();
        let mut early = SeekFrames::new(SeekAim::At(Duration::ZERO));
        let ready = early.accept(Some(seek_frame(&mut order, 80, 1))).unwrap();
        assert_eq!(ready.position, Duration::ZERO);
        assert!(ready.frame.is_none());
        assert!(!ready.ended);
        let mut pending = ready.pending;
        let status = Mutex::new(Status::default());
        let stop = AtomicBool::new(false);
        assert!(take_due_frame(&mut pending, Duration::from_millis(79)).is_none());
        assert!(status.lock().unwrap().frame.is_none());
        let due = take_due_frame(&mut pending, Duration::from_millis(80)).unwrap();
        assert!(publish_frame(&status, &stop, 0, 1, due.decoded));
        assert_eq!(
            status.lock().unwrap().frame.as_ref().map(|frame| frame.1),
            Some(Duration::from_millis(80))
        );
        assert!(pending.is_none());
        let mut late = SeekFrames::new(SeekAim::At(Duration::from_millis(500)));
        late.accept(Some(seek_frame(&mut order, 120, 2)));
        late.accept(Some(seek_frame(&mut order, 300, 3)));
        let ready = late.accept(None).unwrap();
        assert_eq!(ready.position, Duration::from_millis(500));
        assert_eq!(ready.frame.unwrap().point.time, Duration::from_millis(300));
        assert!(ready.ended);
    }

    #[test]
    fn preview_step_after_a_completed_leading_gap_ignores_the_old_visible_frame() {
        let mut order = FrameOrder::default();
        let old = seek_frame(&mut order, 5000, 1).point;
        let mut seek = SeekFrames::new(SeekAim::At(Duration::ZERO));
        let ready = seek.accept(Some(seek_frame(&mut order, 80, 2))).unwrap();
        assert!(ready.frame.is_none());
        let mut pending = ready.pending;
        let StepStart::First(next) = step_start(None, Some(old), &mut pending) else {
            panic!("无 floor 的逐帧必须消费首个未来帧");
        };
        assert_eq!(next.point.time, Duration::from_millis(80));
        assert_eq!(next.decoded.bgra, [2; 4]);
        assert!(pending.is_none());
        assert!(matches!(
            step_start(None, Some(old), &mut pending),
            StepStart::Empty
        ));
    }

    #[test]
    fn preview_step_uses_actual_frame_order_including_duplicate_pts() {
        let mut order = FrameOrder::default();
        let first = seek_frame(&mut order, 40, 1).point;
        let mut step = SeekFrames::new(SeekAim::After(first));
        order = FrameOrder::default();
        assert!(step.accept(Some(seek_frame(&mut order, 0, 0))).is_none());
        assert!(step.accept(Some(seek_frame(&mut order, 40, 1))).is_none());
        let ready = step.accept(Some(seek_frame(&mut order, 40, 2))).unwrap();
        let point = ready.frame.unwrap().point;
        assert_eq!(point.repeated, 1);
        let mut next = SeekFrames::new(SeekAim::After(point));
        order = FrameOrder::default();
        assert!(next.accept(Some(seek_frame(&mut order, 40, 1))).is_none());
        assert!(next.accept(Some(seek_frame(&mut order, 40, 2))).is_none());
        let ready = next.accept(Some(seek_frame(&mut order, 120, 3))).unwrap();
        assert_eq!(ready.position, Duration::from_millis(120));
        let mut end = SeekFrames::new(SeekAim::After(ready.frame.unwrap().point));
        assert!(end.accept(None).unwrap().frame.is_none());
    }

    #[test]
    fn preview_frame_mailbox_moves_pixels_without_copying() {
        let mut pixels = vec![1, 2, 3, 255];
        pixels.reserve(32);
        let pointer = pixels.as_ptr();
        let capacity = pixels.capacity();
        let status = Mutex::new(Status {
            frame: Some((1, Duration::from_millis(120), pixels)),
            ..Default::default()
        });
        let (_, time, pixels) = status.lock().unwrap().frame.take().unwrap();
        assert_eq!(time, Duration::from_millis(120));
        assert!(status.lock().unwrap().frame.is_none());
        let image = image::RgbaImage::from_raw(1, 1, pixels).unwrap();
        assert_eq!(image.as_raw().as_ptr(), pointer);
        assert_eq!(image.as_raw().capacity(), capacity);
    }

    #[test]
    fn preview_frame_mailbox_rejects_previous_seek_and_closed_session() {
        let status = Mutex::new(Status {
            generation: 2,
            ..Default::default()
        });
        let stop = AtomicBool::new(false);
        let frame = || DecodedFrame {
            time: Duration::from_secs(5),
            bgra: vec![1, 2, 3, 255],
        };
        publish_frame(&status, &stop, 1, 10, frame());
        assert!(status.lock().unwrap().frame.is_none());
        assert_eq!(status.lock().unwrap().position, Duration::ZERO);
        publish_frame(&status, &stop, 2, 11, frame());
        assert_eq!(status.lock().unwrap().frame.take().unwrap().0, 11);
        stop.store(true, Ordering::Release);
        publish_frame(&status, &stop, 2, 12, frame());
        assert!(status.lock().unwrap().frame.is_none());
    }

    #[test]
    fn preview_frame_timestamp_stays_bound_to_pixels_after_clock_advances() {
        let player = Player {
            commands: None,
            status: Arc::new(Mutex::new(Status::default())),
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
            worker_exit: WorkerExit::completed(),
            rate: 1.,
            audio: None,
            audio_error: None,
            sync: Mutex::new(PlaybackObserver::default()),
        };
        let time = Duration::from_millis(1234);
        publish_frame(
            &player.status,
            &player.stop,
            0,
            7,
            DecodedFrame {
                time,
                bgra: vec![1, 2, 3, 255],
            },
        );
        player.status.lock().unwrap().position = Duration::from_secs(9);
        let (serial, frame_time, pixels) = player.take_frame_with_time().unwrap();
        assert_eq!((serial, frame_time), (7, time));
        assert_eq!(pixels, [1, 2, 3, 255]);
        assert!(player.take_frame().is_none());
        publish_frame(
            &player.status,
            &player.stop,
            0,
            8,
            DecodedFrame { time, bgra: pixels },
        );
        assert_eq!(player.take_frame().unwrap(), (8, vec![1, 2, 3, 255]));
    }
}
