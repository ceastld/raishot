//! 媒体预览的声音：逐块解码和混音，WASAPI 共享模式输出到默认设备。
//! 播放位置由预览命令驱动（播放／暂停／跳转／倍速），与画面使用同一媒体时间。
use crate::{
    audio_capture::{AudioSource, MixLevels},
    audio_devices::{default_id, resolve_endpoint, EndpointInfo, EndpointKind, EndpointSelection},
    media_foundation::{AudioReader, DecodedAudio, MfRuntime},
    media_playback_sync::WorkerExit,
    preview_audio_clock::ClockPhase,
    preview_audio_device_clock::{ClockProjection, OutputClock, PublishedClock},
    Result,
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::Win32::{
    Media::{
        Audio::*,
        KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
        Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
    },
    System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL},
};

#[path = "audio_resample_quality.rs"]
mod resample_quality;
#[path = "audio_time_stretch.rs"]
mod time_stretch;
pub(crate) use resample_quality::ResampleQuality;
#[path = "preview_audio_processing.rs"]
mod processing;
use processing::ProcessingStream;
pub(crate) use processing::{AudioProcessing, PitchMode};
#[path = "preview_audio_source_resample.rs"]
mod source_resample;
use source_resample::{SourcePoll, SourceResampler};
#[path = "preview_audio_mix.rs"]
mod mix;
#[path = "audio_wav_blocks.rs"]
mod wav_blocks;
pub(crate) use mix::remux_recording_tracks;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioStreamSelection {
    First,
    Stream(u32),
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStreamState {
    selection: AudioStreamSelection,
    request_generation: u64,
    revision: u64,
    switching: bool,
    /// 新 client 没有旧播放位置；None 传输命令不能覆盖尚未确认的切轨目标。
    pending_origin: Option<Duration>,
}
impl FileStreamState {
    fn initial() -> Self {
        Self {
            selection: AudioStreamSelection::First,
            request_generation: 0,
            revision: 0,
            switching: true,
            pending_origin: Some(Duration::ZERO),
        }
    }
}

#[derive(Clone)]
struct State {
    playing: bool,
    position: Option<Duration>,
    rate: f64,
    levels: MixLevels,
    volume: f32,
    muted: bool,
    processing: AudioProcessing,
    /// 输出线程用此本地修订号识别一次原子传输状态变更。
    generation: u64,
    transport_generation: u64,
    published: Option<crate::preview_audio_device_clock::PublishedClock>,
    /// 最近一次真实设备尾部排空回执；控制可以清实时快照，但不抹掉同代诊断。
    drained: Option<PublishedClock>,
    projection: Option<ClockProjection>,
    file: Option<FileStreamState>,
}
impl State {
    fn stream_revision(&self) -> u64 {
        self.file.map_or(0, |file| file.revision)
    }
    fn attempt_key(&self) -> AttemptKey {
        AttemptKey {
            stream_revision: self.stream_revision(),
            generation: self.generation,
        }
    }
    fn retain_pending_origin(&mut self) {
        if let Some(file) = &mut self.file {
            if file.pending_origin.is_some() {
                if let Some(position) = self.position {
                    file.pending_origin = Some(position);
                }
            }
        }
    }
    fn invalidate_clock(&mut self) {
        self.published = None;
        self.projection = None;
    }
    fn reset_origin(&self, settled: Duration) -> Duration {
        self.file
            .and_then(|file| file.pending_origin)
            .or(self.position)
            .unwrap_or(settled)
    }
}
/// 每路最多保留两块，每块最多两秒；限制与影片长度无关。
const MAX_CHUNK_SECONDS: usize = 2;
type AudioInfo = Arc<Mutex<Option<Vec<(u32, usize)>>>>;

pub(crate) struct PreviewAudio {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    worker_exit: WorkerExit,
    error: Arc<Mutex<Option<String>>>,
    info: AudioInfo,
    pub sources: Vec<AudioSource>,
}
impl PreviewAudio {
    /// 没有分轨（GIF、未录声音）时返回 None。
    pub(crate) fn open(
        tracks: &[(AudioSource, PathBuf)],
        levels: MixLevels,
    ) -> Result<Option<Self>> {
        if tracks.is_empty() {
            return Ok(None);
        }
        let sources = tracks.iter().map(|(source, _)| *source).collect();
        let paths = tracks
            .iter()
            .map(|(source, path)| (Some(*source), path.clone()))
            .collect();
        Self::start(paths, sources, levels).map(Some)
    }
    /// 普通文件允许没有音轨；文件打开与解码错误通过 error 异步报告。
    pub(crate) fn open_file(path: PathBuf) -> Result<Self> {
        Self::start(vec![(None, path)], Vec::new(), MixLevels::default())
    }
    fn start(
        paths: Vec<(Option<AudioSource>, PathBuf)>,
        sources: Vec<AudioSource>,
        levels: MixLevels,
    ) -> Result<Self> {
        let file = sources.is_empty().then(FileStreamState::initial);
        let state = Arc::new(Mutex::new(State {
            playing: false,
            position: Some(Duration::ZERO),
            rate: 1.,
            levels,
            volume: 1.,
            muted: false,
            processing: AudioProcessing::default(),
            generation: 0,
            transport_generation: 0,
            published: None,
            drained: None,
            projection: None,
            file,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let info = Arc::new(Mutex::new(None));
        let worker_exit = WorkerExit::pending();
        let thread = {
            let state = state.clone();
            let stop = stop.clone();
            let error = error.clone();
            let info = info.clone();
            let exit = worker_exit.clone();
            std::thread::Builder::new()
                .name("rustshot-preview-audio".into())
                .spawn(move || {
                    let _exit = exit.guard();
                    let failure =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            render(paths, &state, &stop, &info, &error)
                        })) {
                            Ok(Ok(())) => None,
                            Ok(Err(failure)) => Some(failure.to_string()),
                            Err(_) => Some(
                                tr!(
                                    "音频播放线程意外终止",
                                    "Audio playback thread stopped unexpectedly"
                                )
                                .to_string(),
                            ),
                        };
                    if let Some(failure) = failure {
                        if !stop.load(Ordering::Acquire) {
                            let mut shared = lock_state(&state);
                            shared.invalidate_clock();
                            if let Some(file) = &mut shared.file {
                                file.switching = false;
                            }
                            *info.lock().unwrap() = None;
                            *error.lock().unwrap() = Some(failure);
                        }
                    }
                })?
        };
        Ok(Self {
            state,
            stop,
            thread: Some(thread),
            worker_exit,
            error,
            info,
            sources,
        })
    }
    fn wake(&self) {
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }
    fn update_gain(&self, f: impl FnOnce(&mut State)) {
        let mut state = lock_state(&self.state);
        f(&mut state);
        drop(state);
        self.wake();
    }
    fn update_legacy_transport(&self, f: impl FnOnce(&mut State)) {
        let mut state = lock_state(&self.state);
        f(&mut state);
        state.retain_pending_origin();
        state.transport_generation = state.transport_generation.wrapping_add(1);
        state.generation = state.generation.wrapping_add(1);
        state.invalidate_clock();
        drop(state);
        self.wake();
    }
    /// 同次发布目标、播放状态和倍率；设备重置由输出线程执行。
    /// None 保留实际已播位置，Some 用于显式跳转；相同界面代次也可以提交后继状态。
    pub(crate) fn set_transport(
        &self,
        generation: u64,
        position: Option<Duration>,
        playing: bool,
        rate: f32,
    ) {
        if !rate.is_finite() || rate <= 0. {
            return;
        }
        let mut state = lock_state(&self.state);
        if generation != state.transport_generation
            && !newer_generation(generation, state.transport_generation)
        {
            return;
        }
        let rate = f64::from(rate.clamp(0.1, 4.));
        if state.transport_generation == generation
            && state.position == position
            && state.playing == playing
            && state.rate == rate
        {
            return;
        }
        state.transport_generation = generation;
        state.position = position;
        state.retain_pending_origin();
        state.playing = playing;
        state.rate = rate;
        state.generation = state.generation.wrapping_add(1);
        state.invalidate_clock();
        drop(state);
        self.wake();
    }
    pub(crate) fn file_selection(&self) -> Option<AudioStreamSelection> {
        if !self.sources.is_empty() {
            return None;
        }
        lock_state(&self.state).file.map(|file| file.selection)
    }
    pub(crate) fn stream_switching(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && lock_state(&self.state)
                .file
                .is_some_and(|file| file.switching)
    }
    /// 每次新的选轨使用新的界面代次；同代同选择的后继播放只更新传输状态。
    pub(crate) fn select_file_stream(
        &self,
        generation: u64,
        selection: AudioStreamSelection,
        position: Duration,
        playing: bool,
        rate: f32,
    ) {
        if !rate.is_finite() || rate <= 0. || !self.sources.is_empty() {
            return;
        }
        let mut state = lock_state(&self.state);
        let Some(mut file) = state.file else {
            return;
        };
        // 已被更新控制覆盖的选轨后继不能回滚传输；同代不能换成另一个来源。
        if (generation != state.transport_generation
            && !newer_generation(generation, state.transport_generation))
            || (generation == file.request_generation && selection != file.selection)
        {
            return;
        }
        let reopen = generation != file.request_generation;
        let rate = f64::from(rate.clamp(0.1, 4.));
        if !reopen
            && state.transport_generation == generation
            && state.position == Some(position)
            && state.playing == playing
            && state.rate == rate
        {
            return;
        }
        if reopen {
            file.selection = selection;
            file.request_generation = generation;
            file.revision = file.revision.wrapping_add(1);
            file.switching = true;
            file.pending_origin = Some(position);
            *self.info.lock().unwrap() = None;
            *self.error.lock().unwrap() = None;
        }
        state.file = Some(file);
        state.transport_generation = generation;
        state.position = Some(position);
        state.retain_pending_origin();
        state.playing = playing;
        state.rate = rate;
        state.generation = state.generation.wrapping_add(1);
        state.invalidate_clock();
        drop(state);
        self.wake();
    }
    pub(crate) fn play(&self, position: Duration) {
        self.update_legacy_transport(|s| {
            s.position = Some(position);
            s.playing = true;
        });
    }
    pub(crate) fn pause(&self) {
        self.update_legacy_transport(|s| {
            s.position = None;
            s.playing = false;
        });
    }
    pub(crate) fn seek(&self, position: Duration) {
        self.update_legacy_transport(|s| s.position = Some(position));
    }
    pub(crate) fn set_rate(&self, rate: f32) {
        if !rate.is_finite() || rate <= 0. {
            return;
        }
        self.update_legacy_transport(|s| {
            s.position = None;
            s.rate = f64::from(rate.clamp(0.1, 4.));
        });
    }
    pub(crate) fn set_levels(&self, levels: MixLevels) {
        self.update_gain(|s| s.levels = levels);
    }
    pub(crate) fn set_volume(&self, volume: f32) {
        self.update_gain(|s| {
            s.volume = if volume.is_finite() {
                volume.clamp(0., 1.)
            } else {
                0.
            }
        });
    }
    pub(crate) fn set_muted(&self, muted: bool) {
        self.update_gain(|s| s.muted = muted);
    }
    pub(crate) fn processing(&self) -> AudioProcessing {
        lock_state(&self.state).processing
    }
    /// 改变处理方式时保留播放位置和界面传输代次，由输出线程重置全部 DSP 缓冲。
    pub(crate) fn set_processing(&self, settings: AudioProcessing) -> Result<()> {
        let mut state = lock_state(&self.state);
        settings.validate(state.rate)?;
        if state.processing == settings {
            return Ok(());
        }
        state.processing = settings;
        // 已确认的显式 seek 目标只是历史请求；新模式从实际已播位置继续。
        // 没有确认快照时保留尚未完成的 seek/选轨目标，不能用旧设备位置覆盖它。
        if state.published.is_some() {
            state.position = None;
        }
        state.retain_pending_origin();
        state.generation = state.generation.wrapping_add(1);
        state.invalidate_clock();
        drop(state);
        self.wake();
        Ok(())
    }
    pub(crate) fn clock(&self) -> Option<crate::preview_audio_device_clock::PublishedClock> {
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        let state = lock_state(&self.state);
        state
            .published
            .filter(|published| published.transport_generation == state.transport_generation)
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
    /// 只将纯时钟模型对齐到调用者的 QPC；不从界面线程访问 WASAPI。
    pub(crate) fn clock_at(&self, qpc_100ns: u64) -> Option<PublishedClock> {
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        let state = lock_state(&self.state);
        let clock = state.projection.as_ref()?.snapshot_at(qpc_100ns).ok()?;
        (clock.transport_generation == state.transport_generation).then_some(clock)
    }
    /// None 表示仍在探测；空列表表示文件没有音轨。
    pub(crate) fn audio_info(&self) -> Option<Vec<(u32, usize)>> {
        self.info.lock().unwrap().clone()
    }

    /// 只保留退出标记；不会持有设备、PCM 或工作线程。
    pub(crate) fn worker_exit(&self) -> WorkerExit {
        self.worker_exit.clone()
    }

    /// 只返回指定传输代已经实际 Ended 的单条回执，不把取消或暂停当成排空。
    pub(crate) fn drained_clock(&self, transport_generation: u64) -> Option<PublishedClock> {
        lock_state(&self.state)
            .drained
            .filter(|clock| clock.transport_generation == transport_generation)
    }
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let mut state = lock_state(&self.state);
        state.playing = false;
        state.generation = state.generation.wrapping_add(1);
        state.invalidate_clock();
        if let Some(file) = &mut state.file {
            file.switching = false;
        }
        drop(state);
        self.wake();
    }
}
impl Drop for PreviewAudio {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            // MF 同步读取可能仍在系统解码器内；界面不等待它，线程会在返回后自行释放设备。
            thread.thread().unpark();
        }
    }
}

#[derive(Clone, Copy)]
struct DeviceFormat {
    rate: u32,
    channels: usize,
    float: bool,
    bits: u16,
}

struct Stream {
    reader: AudioReader,
    source: Option<AudioSource>,
    blocks: VecDeque<DecodedAudio>,
    ended: bool,
    end_time: Duration,
    source_resampler: Option<SourceResampler>,
    source_offset: usize,
}
impl Stream {
    fn set_source_grid(&mut self, high: bool, grid: u32) -> Result<()> {
        if !high || self.reader.rate == grid {
            self.source_resampler = None;
        } else if self
            .source_resampler
            .as_ref()
            .is_none_or(|source| source.output_rate() != grid)
        {
            self.source_resampler = Some(SourceResampler::new(
                self.reader.rate,
                self.reader.channels,
                grid,
            )?);
        }
        Ok(())
    }
    fn seek(&mut self, position: Duration) -> Result<()> {
        self.blocks.clear();
        self.ended = false;
        self.end_time = Duration::ZERO;
        self.source_offset = 0;
        if let Some(source) = &mut self.source_resampler {
            source.reset(position)?;
        }
        self.reader.seek(position)
    }
    fn read(&mut self) -> Result<()> {
        match self.reader.next()? {
            Some(block) => {
                let limit = self.reader.rate as usize * self.reader.channels * MAX_CHUNK_SECONDS;
                if block.samples.len() > limit || block.samples.len() % self.reader.channels != 0 {
                    return Err(tr!(
                        "音频解码块超过缓冲限制",
                        "Decoded audio exceeds the buffer limit"
                    )
                    .into());
                }
                if !block.samples.is_empty() {
                    let frames = (block.samples.len() / self.reader.channels) as u64;
                    let duration = Duration::new(
                        frames / u64::from(self.reader.rate),
                        (frames % u64::from(self.reader.rate) * 1_000_000_000
                            / u64::from(self.reader.rate)) as u32,
                    );
                    let end = block
                        .time
                        .checked_add(duration)
                        .ok_or(tr!("音频媒体时间溢出", "Audio media time overflow"))?;
                    self.end_time = self.end_time.max(end);
                    self.blocks.push_back(block);
                }
            }
            None => self.ended = true,
        }
        Ok(())
    }
    fn exhausted(&self, position: f64) -> bool {
        self.ended && position >= self.end_time.as_secs_f64()
    }
    fn actual_end(&self) -> Option<Duration> {
        (!self.end_time.is_zero() || self.ended).then_some(self.end_time)
    }
    /// None 表示控制代次变化；取消的解码块不会进入输出设备。
    fn sample(
        &mut self,
        position: f64,
        state: &Mutex<State>,
        stop: &AtomicBool,
        generation: u64,
    ) -> Result<Option<[f32; 2]>> {
        if self.source_resampler.is_some() {
            return self.sample_high(position, state, stop, generation);
        }
        for _ in 0..4096 {
            if !current(state, stop, generation) {
                return Ok(None);
            }
            let Some(block) = self.blocks.front() else {
                if self.ended {
                    return Ok(Some([0.; 2]));
                }
                self.read()?;
                if !current(state, stop, generation) {
                    return Ok(None);
                }
                continue;
            };
            let offset = (position - block.time.as_secs_f64()) * f64::from(self.reader.rate);
            if offset < 0. {
                return Ok(Some([0.; 2]));
            }
            let frames = block.samples.len() / self.reader.channels;
            if offset >= frames as f64 {
                self.blocks.pop_front();
                continue;
            }
            let index = offset.floor() as usize;
            if index + 1 >= frames && self.blocks.len() == 1 && !self.ended {
                self.read()?;
                if !current(state, stop, generation) {
                    return Ok(None);
                }
            }
            return Ok(Some(interpolate(
                self.blocks.front().unwrap(),
                self.blocks.get(1),
                self.reader.rate,
                self.reader.channels,
                index,
                (offset - index as f64) as f32,
            )));
        }
        Err(tr!(
            "音频定位未能取得有效样本",
            "Audio seeking produced no usable samples"
        )
        .into())
    }

    fn sample_high(
        &mut self,
        position: f64,
        state: &Mutex<State>,
        stop: &AtomicBool,
        generation: u64,
    ) -> Result<Option<[f32; 2]>> {
        let time = Duration::try_from_secs_f64(position)
            .map_err(|_| tr!("音频媒体时间无效", "Invalid audio media time"))?;
        for _ in 0..4096 {
            if !current(state, stop, generation) {
                return Ok(None);
            }
            match self.source_resampler.as_mut().unwrap().sample_at(time)? {
                SourcePoll::Frame(frame) => return Ok(Some(frame)),
                SourcePoll::Ended => return Ok(Some([0.; 2])),
                SourcePoll::Pending => continue,
                SourcePoll::NeedInput => {
                    if let Some(block) = self.blocks.front() {
                        let progress = self.source_resampler.as_mut().unwrap().push_block(
                            block.time,
                            &block.samples,
                            self.source_offset,
                        )?;
                        self.source_offset += progress.consumed_frames;
                        if self.source_offset == block.samples.len() / self.reader.channels {
                            self.blocks.pop_front();
                            self.source_offset = 0;
                        }
                    } else if self.ended {
                        let source = self.source_resampler.as_mut().unwrap();
                        source.finish()?;
                        self.end_time = source.end_time().unwrap();
                    } else {
                        // 解码器调用前后都验代；未消费的块始终保存在本 Stream。
                        if !current(state, stop, generation) {
                            return Ok(None);
                        }
                        self.read()?;
                        if !current(state, stop, generation) {
                            return Ok(None);
                        }
                    }
                }
            }
        }
        Err(tr!(
            "原生音频定位超出单次预算",
            "Source audio seeking exceeded its work budget"
        )
        .into())
    }
}

fn interpolate(
    block: &DecodedAudio,
    next: Option<&DecodedAudio>,
    rate: u32,
    channels: usize,
    index: usize,
    fraction: f32,
) -> [f32; 2] {
    let frames = block.samples.len() / channels;
    let contiguous = next.filter(|next| {
        let end = block.time.as_secs_f64() + frames as f64 / f64::from(rate);
        (next.time.as_secs_f64() - end).abs() < 1.5 / f64::from(rate)
    });
    std::array::from_fn(|channel| {
        let channel = channel.min(channels - 1);
        let a = block.samples[index * channels + channel];
        let b = if index + 1 < frames {
            block.samples[(index + 1) * channels + channel]
        } else {
            contiguous.map_or(a, |next| next.samples[channel])
        };
        let value = a + (b - a) * fraction;
        if value.is_finite() {
            value
        } else {
            0.
        }
    })
}

fn current(state: &Mutex<State>, stop: &AtomicBool, generation: u64) -> bool {
    !stop.load(Ordering::Acquire) && lock_state(state).generation == generation
}

fn newer_generation(candidate: u64, previous: u64) -> bool {
    let distance = candidate.wrapping_sub(previous);
    distance != 0 && distance < 1_u64 << 63
}

fn current_stream(state: &Mutex<State>, stop: &AtomicBool, revision: u64) -> bool {
    !stop.load(Ordering::Acquire) && lock_state(state).stream_revision() == revision
}

fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn device_sample(value: [f32; 2], channels: usize, channel: usize) -> f32 {
    if channels == 1 {
        ((f64::from(value[0]) + f64::from(value[1])) * 0.5) as f32
    } else {
        value.get(channel).copied().unwrap_or(0.)
    }
}

/// 最终持状态锁提交时读取全局音量；MF/DSP 前瞻不能烘焙过期的静音状态。
fn device_output_sample(value: [f32; 2], channels: usize, channel: usize, state: &State) -> f32 {
    if state.muted || !state.volume.is_finite() {
        return 0.;
    }
    let sample = device_sample(value, channels, channel);
    if !sample.is_finite() {
        return 0.;
    }
    (f64::from(sample) * f64::from(state.volume.clamp(0., 1.))).clamp(-1., 1.) as f32
}

struct Output {
    enumerator: IMMDeviceEnumerator,
    endpoint: EndpointInfo,
    check_default_at: Instant,
    client: IAudioClient,
    writer: IAudioRenderClient,
    format: DeviceFormat,
    size: u32,
    started: bool,
    clock: OutputClock,
    last_played: Duration,
}
impl Output {
    #[cfg(test)]
    fn open() -> Result<Self> {
        Self::open_for_attempt(0, Duration::ZERO, 1., 0)
    }
    fn open_for_attempt(
        transport_generation: u64,
        media_origin: Duration,
        rate: f64,
        clock_epoch: u64,
    ) -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let resolved = resolve_endpoint(
                &enumerator,
                EndpointKind::Render,
                &EndpointSelection::Default,
            )?;
            let device = resolved.device;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client.GetMixFormat()?;
            let format = device_format(mix);
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            let format = format?;
            init?;
            let writer = client.GetService()?;
            let size = client.GetBufferSize()?;
            if size == 0 || size > format.rate * 2 {
                return Err(tr!(
                    "音频输出缓冲超过限制",
                    "Audio output buffer exceeds the limit"
                )
                .into());
            }
            let clock = OutputClock::from_client_with_epoch(
                &client,
                transport_generation,
                media_origin,
                rate,
                clock_epoch,
            )?;
            Ok(Self {
                enumerator,
                endpoint: resolved.info,
                check_default_at: Instant::now() + Duration::from_secs(1),
                client,
                writer,
                format,
                size,
                started: false,
                clock,
                last_played: media_origin,
            })
        }
    }
    fn observe(&mut self, queued_until: Duration, phase: ClockPhase) -> Result<PublishedClock> {
        let observed = self.clock.observe(queued_until, phase)?;
        self.last_played = observed.clock.media_position;
        Ok(observed)
    }
    fn check_default_endpoint(&mut self) -> Result<()> {
        let now = Instant::now();
        if now < self.check_default_at {
            return Ok(());
        }
        self.check_default_at = now + Duration::from_secs(1);
        let name = &self.endpoint.name;
        let current = default_id(&self.enumerator, EndpointKind::Render).map_err(|reason| {
            trf!(
                "无法检查默认播放设备 {name}：{reason}",
                "Cannot check the default playback endpoint {name}: {reason}"
            )
        })?;
        if current.as_deref() != Some(self.endpoint.id.as_str()) {
            return Err(trf!(
                "默认播放设备已变化（原设备 {name}），声音已停止；请重新打开视频以使用新设备",
                "Default playback endpoint changed (previously {name}). Audio stopped; reopen the video to use the new endpoint."
            ).into());
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        unsafe {
            self.client.Stop()?;
            self.started = false;
            self.client.Reset()?;
        }
        Ok(())
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
    }
}

unsafe fn device_format(format: *const WAVEFORMATEX) -> Result<DeviceFormat> {
    let base = unsafe { std::ptr::read_unaligned(format) };
    let tag = u32::from(base.wFormatTag);
    let float = if tag == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= 22 {
        let ext = unsafe { std::ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>()) };
        let sub = ext.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            false
        } else {
            return Err("不支持的输出设备格式".into());
        }
    } else if tag == 1 || tag == WAVE_FORMAT_IEEE_FLOAT {
        tag == WAVE_FORMAT_IEEE_FLOAT
    } else {
        return Err(tr!("不支持的输出设备格式", "Unsupported audio output format").into());
    };
    let bits = base.wBitsPerSample;
    let supported_sample = bits == 32 || !float && bits == 16;
    if !supported_sample
        || base.nChannels == 0
        || base.nChannels > 32
        || base.nSamplesPerSec == 0
        || base.nSamplesPerSec > 384_000
    {
        return Err("不支持的输出设备格式".into());
    }
    Ok(DeviceFormat {
        rate: base.nSamplesPerSec,
        channels: usize::from(base.nChannels),
        float,
        bits,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AttemptKey {
    stream_revision: u64,
    generation: u64,
}

#[derive(Default)]
struct RenderAttempts {
    failed_key: Option<AttemptKey>,
    completed_revision: Option<u64>,
}
impl RenderAttempts {
    fn should_attempt(&self, key: AttemptKey) -> bool {
        self.failed_key != Some(key) && self.completed_revision != Some(key.stream_revision)
    }
}

#[derive(Default)]
struct OutputContinuation {
    media_origin: Duration,
    clock_epoch: Option<u64>,
}
impl OutputContinuation {
    fn next_epoch(&self) -> u64 {
        self.clock_epoch.map_or(0, |epoch| epoch.wrapping_add(1))
    }
}

enum RenderOutcome {
    Interrupted,
    NoAudio,
}

struct RenderFailure {
    key: AttemptKey,
    error: Box<dyn std::error::Error>,
}

fn render(
    paths: Vec<(Option<AudioSource>, PathBuf)>,
    state: &Mutex<State>,
    stop: &AtomicBool,
    info: &Mutex<Option<Vec<(u32, usize)>>>,
    error: &Mutex<Option<String>>,
) -> Result<()> {
    let mut attempts = RenderAttempts::default();
    let mut continuation = OutputContinuation::default();
    while !stop.load(Ordering::Acquire) {
        let request = lock_state(state).clone();
        let key = request.attempt_key();
        if !attempts.should_attempt(key) {
            std::thread::park_timeout(Duration::from_millis(200));
            continue;
        }
        if request
            .file
            .is_some_and(|file| file.selection == AudioStreamSelection::Disabled)
        {
            if finish_file_attempt(state, stop, info, error, key, Some(Vec::new()), None) {
                attempts.completed_revision = Some(key.stream_revision);
            }
            continue;
        }
        match render_streams(
            &paths,
            state,
            stop,
            info,
            error,
            &request,
            &mut continuation,
        ) {
            Ok(RenderOutcome::Interrupted) => {}
            Ok(RenderOutcome::NoAudio) => {
                if finish_file_attempt(state, stop, info, error, key, Some(Vec::new()), None) {
                    attempts.completed_revision = Some(key.stream_revision);
                }
            }
            Err(failure) => {
                // 当前失败只发布一次；旧失败不锁住已更新的传输或处理配置。
                attempts.failed_key = Some(failure.key);
                finish_file_attempt(
                    state,
                    stop,
                    info,
                    error,
                    failure.key,
                    None,
                    Some(failure.error.to_string()),
                );
            }
        }
    }
    Ok(())
}

fn render_streams(
    paths: &[(Option<AudioSource>, PathBuf)],
    state: &Mutex<State>,
    stop: &AtomicBool,
    info: &Mutex<Option<Vec<(u32, usize)>>>,
    error: &Mutex<Option<String>>,
    request: &State,
    continuation: &mut OutputContinuation,
) -> std::result::Result<RenderOutcome, RenderFailure> {
    let stream_revision = request.stream_revision();
    let mut failure_key = request.attempt_key();
    // runtime 比 Output 活得更久；失败后先回收已播位置/epoch，再释放设备及 COM。
    let mut runtime = None;
    let mut output = None;
    let result = (|| -> Result<RenderOutcome> {
        if !current_stream(state, stop, stream_revision) {
            return Ok(RenderOutcome::Interrupted);
        }
        runtime = Some(MfRuntime::new()?);
        let mut streams = Vec::new();
        for (source, path) in paths {
            if !current_stream(state, stop, stream_revision) {
                return Ok(RenderOutcome::Interrupted);
            }
            let reader = match request.file.map(|file| file.selection) {
                Some(AudioStreamSelection::Stream(index)) => {
                    Some(AudioReader::open_stream_for_preview(path, index)?)
                }
                Some(AudioStreamSelection::Disabled) => return Ok(RenderOutcome::NoAudio),
                Some(AudioStreamSelection::First) | None => AudioReader::open_for_preview(path)?,
            };
            match reader {
                Some(reader) => {
                    if reader.rate == 0
                        || reader.rate > 384_000
                        || reader.channels == 0
                        || reader.channels > 2
                    {
                        return Err(
                            tr!("不支持的音轨格式", "Unsupported audio track format").into()
                        );
                    }
                    streams.push(Stream {
                        reader,
                        source: *source,
                        blocks: VecDeque::new(),
                        ended: false,
                        end_time: Duration::ZERO,
                        source_resampler: None,
                        source_offset: 0,
                    });
                }
                None if source.is_some() => {
                    return Err(tr!("录屏分轨没有音频", "Recording track contains no audio").into())
                }
                None => {}
            }
        }
        let formats: Vec<_> = streams
            .iter()
            .map(|stream| (stream.reader.rate, stream.reader.channels))
            .collect();
        if !current_stream(state, stop, stream_revision) {
            return Ok(RenderOutcome::Interrupted);
        }
        if streams.is_empty() {
            return Ok(RenderOutcome::NoAudio);
        }
        output = Some(Output::open_for_attempt(
            request.transport_generation,
            continuation.media_origin,
            request.rate,
            continuation.next_epoch(),
        )?);
        let output = output.as_mut().unwrap();
        let format = output.format;
        let mut generation = None;
        let mut cursor = continuation.media_origin.as_secs_f64();
        let mut queued_until = continuation.media_origin;
        let mut phase = ClockPhase::Priming;
        let mut draining = false;
        let mut processor: Option<ProcessingStream> = None;
        while !stop.load(Ordering::Acquire) {
            let snapshot = lock_state(state).clone();
            failure_key = snapshot.attempt_key();
            if snapshot.stream_revision() != stream_revision {
                // Output 在返回时先 Stop/Reset 和释放，下一次尝试才打开新 Reader。
                return Ok(RenderOutcome::Interrupted);
            }
            output.check_default_endpoint()?;
            if generation != Some(snapshot.generation) {
                // Stop 冻结设备位置，Reset 前结算真正已播出的媒体，保留尚未听到的内容。
                unsafe {
                    output.client.Stop()?;
                }
                output.started = false;
                let settle_phase = match phase {
                    ClockPhase::Paused | ClockPhase::Underrun | ClockPhase::Ended => phase,
                    _ => ClockPhase::Paused,
                };
                let settled = output.observe(queued_until, settle_phase)?;
                let origin = snapshot.reset_origin(settled.clock.media_position);
                let grid = reset_timeline(output, &mut streams, state, stop, &snapshot, origin)?;
                processor = processing_stream(&snapshot, format.rate, grid, origin)?;
                // 即使控制已经更新，driver 也已重锚；本地队列边界必须先与它保持一致。
                cursor = origin.as_secs_f64();
                queued_until = origin;
                phase = if snapshot.playing {
                    ClockPhase::Priming
                } else {
                    ClockPhase::Paused
                };
                draining = false;
                generation = Some(snapshot.generation);
                if !confirm_timeline(state, stop, info, error, &snapshot, &formats) {
                    continue;
                }
                // Seek 与处理器均已成功且仍属此代，才把新 origin 作为恢复锚。
                output.last_played = origin;
                let clock = output.observe(queued_until, phase)?;
                phase = clock.clock.phase;
                publish_clock(
                    state,
                    stop,
                    snapshot.generation,
                    clock,
                    Some(output.clock.projection()),
                );
            }
            if !snapshot.playing || !current(state, stop, snapshot.generation) {
                std::thread::park_timeout(Duration::from_millis(200));
                continue;
            }
            if phase == ClockPhase::Ended {
                // 已结束的输出只等控制唤醒，不反复访问设备或调用 Stop。
                if output.started {
                    unsafe {
                        output.client.Stop()?;
                    }
                    output.started = false;
                }
                std::thread::park_timeout(Duration::from_millis(200));
                continue;
            }
            let intent = if output.started && !draining {
                ClockPhase::Playing
            } else {
                phase
            };
            let observed = output.observe(queued_until, intent)?;
            phase = observed.clock.phase;
            publish_clock(
                state,
                stop,
                snapshot.generation,
                observed,
                Some(output.clock.projection()),
            );
            if phase == ClockPhase::Ended {
                unsafe {
                    output.client.Stop()?;
                }
                output.started = false;
                std::thread::park_timeout(Duration::from_millis(200));
                continue;
            }
            if draining {
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            }
            let padding = unsafe { output.client.GetCurrentPadding()? };
            if output.started && padding == 0 && phase == ClockPhase::Playing {
                let held = output.observe(queued_until, ClockPhase::Underrun)?;
                publish_clock(
                    state,
                    stop,
                    snapshot.generation,
                    held,
                    Some(output.clock.projection()),
                );
                let origin = held.clock.media_position;
                let grid = reset_timeline(output, &mut streams, state, stop, &snapshot, origin)?;
                processor = processing_stream(&snapshot, format.rate, grid, origin)?;
                cursor = origin.as_secs_f64();
                queued_until = origin;
                phase = ClockPhase::Priming;
                continue;
            }
            // 只排队约 50 ms；音量更新沿用当前队列，不触发定位与设备重置。
            let target = (format.rate / 20).max(1).min(output.size);
            if padding >= target {
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            }
            let frames = target - padding;
            let mut samples = Vec::with_capacity(frames as usize);
            let mut interrupted = false;
            let source_ended;
            if let Some(processor) = &mut processor {
                let Some(processed) =
                    processor.render(&mut streams, state, stop, &snapshot, frames as usize)?
                else {
                    continue;
                };
                samples = processed.samples;
                cursor = processed.media_end.as_secs_f64();
                source_ended = processed.finished;
            } else {
                let step = snapshot.rate / f64::from(format.rate);
                for _ in 0..frames {
                    if streams.iter().all(|stream| stream.exhausted(cursor)) {
                        break;
                    }
                    let mut mixed = [0.; 2];
                    for stream in &mut streams {
                        let Some(sample) =
                            stream.sample(cursor, state, stop, snapshot.generation)?
                        else {
                            interrupted = true;
                            break;
                        };
                        let gain = stream
                            .source
                            .map_or(1., |source| snapshot.levels.gain(source));
                        for channel in 0..2 {
                            mixed[channel] += sample[channel] * gain;
                        }
                    }
                    if interrupted {
                        break;
                    }
                    if streams.iter().all(|stream| stream.exhausted(cursor)) {
                        break;
                    }
                    samples.push(mixed);
                    cursor += step;
                }
                source_ended = streams.iter().all(|stream| stream.exhausted(cursor));
            }
            if interrupted {
                continue;
            }
            if samples.is_empty() {
                if !source_ended {
                    std::thread::park_timeout(Duration::from_millis(1));
                    continue;
                }
                draining = true;
                phase = ClockPhase::Draining;
                let clock = output.observe(queued_until, phase)?;
                phase = clock.clock.phase;
                publish_clock(
                    state,
                    stop,
                    snapshot.generation,
                    clock,
                    Some(output.clock.projection()),
                );
                continue;
            }
            // MF 解码期间也可能耗尽队列，不能把设备空跑的时间算成已播出的新媒体。
            let padding_now = unsafe { output.client.GetCurrentPadding()? };
            if output.started {
                let observed = output.observe(queued_until, ClockPhase::Playing)?;
                phase = observed.clock.phase;
                publish_clock(
                    state,
                    stop,
                    snapshot.generation,
                    observed,
                    Some(output.clock.projection()),
                );
            }
            if output.started && padding_now == 0 && phase == ClockPhase::Playing {
                let held = output.observe(queued_until, ClockPhase::Underrun)?;
                publish_clock(
                    state,
                    stop,
                    snapshot.generation,
                    held,
                    Some(output.clock.projection()),
                );
                let origin = held.clock.media_position;
                let grid = reset_timeline(output, &mut streams, state, stop, &snapshot, origin)?;
                processor = processing_stream(&snapshot, format.rate, grid, origin)?;
                cursor = origin.as_secs_f64();
                queued_until = origin;
                phase = ClockPhase::Priming;
                continue;
            }
            // MF 读取发生在锁外；只在最终短暂提交期间排斥控制更新，防止旧代缓冲重新入队。
            let shared = lock_state(state);
            if stop.load(Ordering::Acquire) || shared.generation != snapshot.generation {
                continue;
            }
            unsafe {
                let frames = samples.len() as u32;
                let bytes_per_sample = usize::from(format.bits / 8);
                let data = output.writer.GetBuffer(frames)?;
                let out = std::slice::from_raw_parts_mut(
                    data,
                    frames as usize * format.channels * bytes_per_sample,
                );
                for (frame, value) in samples.iter().enumerate() {
                    for channel in 0..format.channels {
                        let sample =
                            device_output_sample(*value, format.channels, channel, &shared);
                        let at = (frame * format.channels + channel) * bytes_per_sample;
                        let bytes = &mut out[at..at + bytes_per_sample];
                        if format.float {
                            bytes.copy_from_slice(&sample.to_le_bytes());
                        } else if format.bits == 16 {
                            bytes.copy_from_slice(&((sample * 32767.) as i16).to_le_bytes());
                        } else {
                            bytes
                                .copy_from_slice(&((sample * 2_147_483_647.) as i32).to_le_bytes());
                        }
                    }
                }
                output.writer.ReleaseBuffer(frames, 0)?;
                queued_until = Duration::try_from_secs_f64(cursor)
                    .map_err(|_| tr!("音频媒体时间无效", "Invalid audio media time"))?;
                if !output.started {
                    output.client.Start()?;
                    output.started = true;
                }
            }
            drop(shared);
            phase = if source_ended {
                draining = true;
                ClockPhase::Draining
            } else {
                ClockPhase::Playing
            };
            let clock = output.observe(queued_until, phase)?;
            phase = clock.clock.phase;
            publish_clock(
                state,
                stop,
                snapshot.generation,
                clock,
                Some(output.clock.projection()),
            );
        }
        Ok(RenderOutcome::Interrupted)
    })();
    if let Some(output) = &output {
        continuation.media_origin = output.last_played;
        continuation.clock_epoch = Some(output.clock.epoch());
    }
    drop(output);
    drop(runtime);
    result.map_err(|error| RenderFailure {
        key: failure_key,
        error,
    })
}

fn processing_stream(
    snapshot: &State,
    output_rate: u32,
    source_grid_rate: u32,
    origin: Duration,
) -> Result<Option<ProcessingStream>> {
    snapshot
        .processing
        .active()
        .then(|| {
            ProcessingStream::new_with_source_rate(
                snapshot.processing,
                snapshot.rate,
                source_grid_rate,
                output_rate,
                origin,
            )
        })
        .transpose()
}

/// High 先保留变速后仍可进入设备通带的源频率；保调算法保持 48 kHz 网格。
fn source_grid_rate(
    settings: AudioProcessing,
    rate: f64,
    device_rate: u32,
    max_native_rate: u32,
) -> Result<u32> {
    if !settings.active() {
        return Ok(time_stretch::SAMPLE_RATE);
    }
    settings.validate(rate)?;
    let grid = if settings.quality == ResampleQuality::High && settings.pitch == PitchMode::Resample
    {
        if !(8_000..=384_000).contains(&device_rate)
            || !(8_000..=384_000).contains(&max_native_rate)
        {
            return Err(tr!(
                "音频网格采样率超出范围",
                "Audio grid sample rate is out of range"
            )
            .into());
        }
        let desired = (f64::from(device_rate) / rate)
            .ceil()
            .min(f64::from(max_native_rate))
            .max(f64::from(time_stretch::SAMPLE_RATE)) as u32;
        // UI 的 quarter 倍率都能精确映射；任意实验倍率仍沿用旧的整数率舍入。
        desired.div_ceil(4) * 4
    } else {
        time_stretch::SAMPLE_RATE
    };
    processing::effective_input_rate(settings, rate, grid)?;
    Ok(grid)
}

/// 无音轨结果属于整个选轨修订；失败还须匹配实际操作的本地代次。
fn finish_file_attempt(
    state: &Mutex<State>,
    stop: &AtomicBool,
    info: &Mutex<Option<Vec<(u32, usize)>>>,
    error: &Mutex<Option<String>>,
    key: AttemptKey,
    formats: Option<Vec<(u32, usize)>>,
    failure: Option<String>,
) -> bool {
    let mut shared = lock_state(state);
    if stop.load(Ordering::Acquire)
        || shared.stream_revision() != key.stream_revision
        || (failure.is_some() && shared.generation != key.generation)
    {
        return false;
    }
    // 本代已发布快照证明显式目标已完成；失败后不能拿历史 seek 覆盖已播恢复锚。
    if failure.is_some()
        && shared
            .published
            .is_some_and(|clock| clock.transport_generation == shared.transport_generation)
    {
        shared.position = None;
    }
    shared.invalidate_clock();
    if let Some(file) = &mut shared.file {
        file.switching = false;
    }
    *info.lock().unwrap() = formats;
    *error.lock().unwrap() = failure;
    true
}

/// 调用方已成功重锚并定位 Reader；传输在此期间变化就保留 pending_origin 再结算。
fn confirm_timeline(
    state: &Mutex<State>,
    stop: &AtomicBool,
    info: &Mutex<Option<Vec<(u32, usize)>>>,
    error: &Mutex<Option<String>>,
    snapshot: &State,
    formats: &[(u32, usize)],
) -> bool {
    let mut shared = lock_state(state);
    if stop.load(Ordering::Acquire)
        || shared.generation != snapshot.generation
        || shared.stream_revision() != snapshot.stream_revision()
    {
        return false;
    }
    if let Some(file) = &mut shared.file {
        file.pending_origin = None;
        file.switching = false;
    }
    *info.lock().unwrap() = Some(formats.to_vec());
    *error.lock().unwrap() = None;
    true
}

fn reset_timeline(
    output: &mut Output,
    streams: &mut [Stream],
    state: &Mutex<State>,
    stop: &AtomicBool,
    snapshot: &State,
    origin: Duration,
) -> Result<u32> {
    let grid = source_grid_rate(
        snapshot.processing,
        snapshot.rate,
        output.format.rate,
        streams
            .iter()
            .map(|stream| stream.reader.rate)
            .max()
            .unwrap_or(time_stretch::SAMPLE_RATE),
    )?;
    output.reset()?;
    output
        .clock
        .reanchor(snapshot.transport_generation, origin, snapshot.rate, origin)?;
    for stream in streams {
        if !current(state, stop, snapshot.generation) {
            break;
        }
        stream.set_source_grid(snapshot.processing.quality == ResampleQuality::High, grid)?;
        stream.seek(origin)?;
    }
    Ok(grid)
}

fn publish_clock(
    state: &Mutex<State>,
    stop: &AtomicBool,
    revision: u64,
    clock: PublishedClock,
    projection: Option<ClockProjection>,
) {
    let mut shared = lock_state(state);
    if !stop.load(Ordering::Acquire)
        && shared.generation == revision
        && shared.transport_generation == clock.transport_generation
    {
        if clock.clock.phase == ClockPhase::Ended
            && !clock.clock.advancing
            && clock.clock.queued_media.is_zero()
        {
            shared.drained = Some(clock);
        }
        shared.published = Some(clock);
        shared.projection = projection;
    }
}

#[cfg(test)]
#[path = "preview_audio_device_probe.rs"]
mod device_probe;

#[cfg(test)]
#[path = "preview_audio_file_probe.rs"]
mod file_probe;

#[cfg(test)]
#[path = "preview_audio_mix_probe.rs"]
mod mix_probe;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_audio_processing_grid_keeps_default_rates_and_ui_extremes_valid() {
        for rate in [0.1, 4.] {
            assert_eq!(
                source_grid_rate(AudioProcessing::default(), rate, 48_000, 96_000).unwrap(),
                48_000
            );
        }
        let high = AudioProcessing {
            pitch: PitchMode::Resample,
            quality: ResampleQuality::High,
        };
        assert!(source_grid_rate(high, 0.1, 48_000, 96_000).is_err());
        assert_eq!(source_grid_rate(high, 0.5, 48_000, 96_000).unwrap(), 96_000);
        assert_eq!(source_grid_rate(high, 1., 48_000, 96_000).unwrap(), 48_000);
        assert_eq!(source_grid_rate(high, 2., 48_000, 96_000).unwrap(), 48_000);
        for rate in [0.5, 0.75, 1., 1.25, 1.5, 2.] {
            for device in [8_000, 44_100, 48_000, 96_000, 192_000, 384_000] {
                for native in [8_000, 44_100, 48_000, 96_000, 384_000] {
                    let grid = source_grid_rate(high, rate, device, native).unwrap();
                    assert!((48_000..=384_000).contains(&grid));
                    assert_eq!(grid % 4, 0);
                    let effective = processing::effective_input_rate(high, rate, grid).unwrap();
                    assert!((8_000..=384_000).contains(&effective));
                    assert_eq!(f64::from(grid) * rate, f64::from(effective));
                    let preserve = AudioProcessing {
                        pitch: PitchMode::PitchPreserving,
                        ..high
                    };
                    assert_eq!(
                        source_grid_rate(preserve, rate, device, native).unwrap(),
                        48_000
                    );
                }
            }
        }
        // 任意实验倍率仍 round，但对齐后的有效率越界须在构造前显式失败。
        assert!(source_grid_rate(high, 1.1, 384_000, 384_000).is_err());
        assert!(source_grid_rate(high, f64::NAN, 48_000, 96_000).is_err());
    }

    #[test]
    fn preview_audio_interpolates_across_contiguous_chunks() {
        let block = DecodedAudio {
            time: Duration::ZERO,
            samples: vec![0.2, -0.2],
        };
        let next = DecodedAudio {
            time: Duration::from_millis(1),
            samples: vec![0.6, -0.6],
        };
        let mixed = interpolate(&block, Some(&next), 1000, 2, 0, 0.5);
        assert!((mixed[0] - 0.4).abs() < 1e-5);
        assert!((mixed[1] + 0.4).abs() < 1e-5);
        let gap = DecodedAudio {
            time: Duration::from_secs(1),
            samples: vec![1., 1.],
        };
        assert_eq!(
            interpolate(&block, Some(&gap), 1000, 2, 0, 0.5),
            [0.2, -0.2]
        );
    }

    #[test]
    fn preview_audio_duplicates_mono_and_sanitizes_samples() {
        let mono = DecodedAudio {
            time: Duration::ZERO,
            samples: vec![0.25, 0.75],
        };
        assert_eq!(interpolate(&mono, None, 1000, 1, 0, 0.5), [0.5, 0.5]);
        let invalid = DecodedAudio {
            time: Duration::ZERO,
            samples: vec![f32::NAN, f32::INFINITY],
        };
        assert_eq!(interpolate(&invalid, None, 1000, 2, 0, 0.5), [0., 0.]);
        assert_eq!(device_sample([0.25, 0.75], 1, 0), 0.5);
        assert_eq!(device_sample([0.25, 0.75], 2, 1), 0.75);
        assert_eq!(device_sample([0.25, 0.75], 6, 2), 0.);
    }

    #[test]
    fn preview_audio_rejects_stale_generation_and_stop() {
        let state = Mutex::new(State {
            playing: false,
            position: Some(Duration::ZERO),
            rate: 1.,
            levels: MixLevels::default(),
            volume: 1.,
            muted: false,
            processing: AudioProcessing::default(),
            generation: 3,
            transport_generation: 7,
            published: None,
            drained: None,
            projection: None,
            file: None,
        });
        let stop = AtomicBool::new(false);
        assert!(current(&state, &stop, 3));
        assert!(!current(&state, &stop, 2));
        stop.store(true, Ordering::Release);
        assert!(!current(&state, &stop, 3));
    }

    fn control_clock(
        transport_generation: u64,
        clock_epoch: u64,
    ) -> crate::preview_audio_device_clock::PublishedClock {
        crate::preview_audio_device_clock::PublishedClock {
            transport_generation,
            clock: crate::preview_audio_clock::ClockSnapshot {
                generation: clock_epoch,
                media_position: Duration::from_secs(3),
                qpc_100ns: 1_000_000,
                phase: crate::preview_audio_clock::ClockPhase::Playing,
                advancing: true,
                queued_media: Duration::from_millis(50),
            },
        }
    }

    /// 无线程、无 client，只验证真实控制方法对共享状态的修改。
    pub(super) fn control_audio() -> PreviewAudio {
        PreviewAudio {
            state: Arc::new(Mutex::new(State {
                playing: true,
                position: Some(Duration::from_secs(2)),
                rate: 1.,
                levels: MixLevels::default(),
                volume: 1.,
                muted: false,
                processing: AudioProcessing::default(),
                generation: 17,
                transport_generation: 41,
                published: Some(control_clock(41, 8)),
                drained: None,
                projection: None,
                file: Some(FileStreamState {
                    selection: AudioStreamSelection::First,
                    request_generation: 41,
                    revision: 2,
                    switching: false,
                    pending_origin: None,
                }),
            })),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            worker_exit: WorkerExit::completed(),
            error: Arc::new(Mutex::new(None)),
            info: Arc::new(Mutex::new(Some(vec![(48_000, 2)]))),
            sources: Vec::new(),
        }
    }

    #[test]
    fn preview_audio_failed_attempt_waits_but_new_processing_retries_file_and_wav() {
        for wav in [false, true] {
            let audio = control_audio();
            if wav {
                lock_state(&audio.state).file = None;
            }
            audio
                .set_processing(AudioProcessing {
                    pitch: PitchMode::Resample,
                    quality: ResampleQuality::High,
                })
                .unwrap();
            let failed = lock_state(&audio.state).attempt_key();
            let mut attempts = RenderAttempts::default();
            assert!(attempts.should_attempt(failed));
            attempts.failed_key = Some(failed);
            assert!(finish_file_attempt(
                &audio.state,
                &audio.stop,
                &audio.info,
                &audio.error,
                failed,
                None,
                Some("source conversion failed".into()),
            ));
            assert!(!attempts.should_attempt(failed));
            audio.set_volume(0.4);
            assert!(!attempts.should_attempt(lock_state(&audio.state).attempt_key()));
            audio.set_processing(AudioProcessing::default()).unwrap();
            let retry = lock_state(&audio.state).clone();
            assert_eq!(retry.stream_revision(), failed.stream_revision);
            assert_ne!(retry.generation, failed.generation);
            assert!(attempts.should_attempt(retry.attempt_key()));
            assert!(confirm_timeline(
                &audio.state,
                &audio.stop,
                &audio.info,
                &audio.error,
                &retry,
                &[(48_000, 2)],
            ));
            assert!(audio.error().is_none());
            assert!(retry.playing);
        }
    }

    #[test]
    fn preview_audio_late_failure_cannot_clear_same_stream_new_generation() {
        let audio = control_audio();
        let failed = lock_state(&audio.state).attempt_key();
        audio
            .set_processing(AudioProcessing {
                pitch: PitchMode::PitchPreserving,
                quality: ResampleQuality::Standard,
            })
            .unwrap();
        let active = lock_state(&audio.state).clone();
        let clock = control_clock(active.transport_generation, 12);
        publish_clock(&audio.state, &audio.stop, active.generation, clock, None);
        *audio.info.lock().unwrap() = Some(vec![(96_000, 2)]);
        *audio.error.lock().unwrap() = Some("current diagnostic".into());
        assert!(!finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            failed,
            None,
            Some("late old failure".into()),
        ));
        let after = lock_state(&audio.state).clone();
        assert_eq!(after.attempt_key(), active.attempt_key());
        assert_eq!(after.position, active.position);
        assert_eq!(after.published, Some(clock));
        assert_eq!(audio.audio_info(), Some(vec![(96_000, 2)]));
        assert_eq!(audio.error().as_deref(), Some("current diagnostic"));
    }

    #[test]
    fn preview_audio_no_audio_completion_survives_transport_changes_until_new_selection() {
        let audio = control_audio();
        let completed = lock_state(&audio.state).attempt_key();
        let attempts = RenderAttempts {
            completed_revision: Some(completed.stream_revision),
            ..RenderAttempts::default()
        };
        audio.set_transport(42, None, false, 1.5);
        let paused = lock_state(&audio.state).attempt_key();
        assert!(!attempts.should_attempt(paused));
        assert!(finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            completed,
            Some(Vec::new()),
            None,
        ));
        assert_eq!(audio.audio_info(), Some(Vec::new()));
        audio.select_file_stream(
            43,
            AudioStreamSelection::Stream(1),
            Duration::from_secs(7),
            true,
            1.,
        );
        assert!(attempts.should_attempt(lock_state(&audio.state).attempt_key()));
    }

    #[test]
    fn preview_audio_recovery_uses_played_origin_without_losing_pending_seek() {
        let audio = control_audio();
        let key = lock_state(&audio.state).attempt_key();
        assert!(finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            key,
            None,
            Some("output failed".into()),
        ));
        let continuation = OutputContinuation {
            media_origin: Duration::from_secs(3),
            clock_epoch: Some(8),
        };
        let failed = lock_state(&audio.state).clone();
        assert!(failed.position.is_none());
        assert_eq!(
            failed.reset_origin(continuation.media_origin),
            Duration::from_secs(3)
        );
        // 已有 client 用新的 Stop 后读数；actor 的重开 fallback 不参与此结算。
        assert_eq!(
            failed.reset_origin(Duration::from_secs(4)),
            Duration::from_secs(4)
        );
        assert_eq!(continuation.next_epoch(), 9);
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(2),
            Duration::from_secs(9),
            true,
            1.,
        );
        audio.set_transport(43, None, true, 1.5);
        let pending = lock_state(&audio.state).clone();
        assert!(finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            pending.attempt_key(),
            None,
            Some("seek not confirmed".into()),
        ));
        assert_eq!(
            lock_state(&audio.state).reset_origin(continuation.media_origin),
            Duration::from_secs(9)
        );
        audio.set_transport(44, Some(Duration::from_secs(11)), true, 1.);
        let unconfirmed = lock_state(&audio.state).clone();
        assert!(finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            unconfirmed.attempt_key(),
            None,
            Some("seek failed".into()),
        ));
        let after = lock_state(&audio.state).clone();
        assert_eq!(after.position, Some(Duration::from_secs(11)));
        assert_eq!(
            after.reset_origin(continuation.media_origin),
            Duration::from_secs(11)
        );
    }

    #[test]
    fn preview_audio_processing_change_retains_transport_and_pending_seek() {
        let audio = control_audio();
        let settings = AudioProcessing {
            pitch: PitchMode::PitchPreserving,
            quality: ResampleQuality::High,
        };
        audio.set_processing(settings).unwrap();
        let after = lock_state(&audio.state).clone();
        assert_eq!(after.processing, settings);
        assert_eq!((after.generation, after.transport_generation), (18, 41));
        assert!(after.playing && after.position.is_none() && after.published.is_none());
        assert_eq!(
            (after.volume, after.muted, after.levels),
            (1., false, MixLevels::default())
        );
        audio.set_processing(settings).unwrap();
        assert_eq!(lock_state(&audio.state).generation, 18);

        audio.set_transport(42, Some(Duration::from_secs(7)), false, 1.25);
        audio.set_processing(AudioProcessing::default()).unwrap();
        let pending = lock_state(&audio.state).clone();
        assert_eq!(pending.transport_generation, 42);
        assert_eq!(pending.position, Some(Duration::from_secs(7)));
        assert!(!pending.playing);
        audio.set_transport(43, None, false, 0.1);
        let generation = lock_state(&audio.state).generation;
        assert!(audio.set_processing(settings).is_err());
        let invalid = lock_state(&audio.state).clone();
        assert_eq!(invalid.generation, generation);
        assert_eq!(invalid.processing, AudioProcessing::default());
    }

    #[test]
    fn preview_audio_gain_changes_preserve_transport_target_and_clock() {
        let audio = control_audio();
        let before = lock_state(&audio.state).clone();
        audio.set_volume(0.25);
        audio.set_muted(true);
        audio.set_levels(MixLevels {
            system: 25,
            microphone: 50,
        });
        let after = lock_state(&audio.state).clone();
        assert_eq!(
            (
                after.generation,
                after.transport_generation,
                after.position,
                after.published
            ),
            (
                before.generation,
                before.transport_generation,
                before.position,
                before.published
            ),
        );
        assert_eq!((after.playing, after.rate), (before.playing, before.rate));
        assert_eq!((after.volume, after.muted), (0.25, true));
        assert_eq!((after.levels.system, after.levels.microphone), (25, 50));
        assert_eq!(audio.clock(), before.published);
    }

    #[test]
    fn preview_audio_publication_cannot_overwrite_a_new_control_or_closed_session() {
        let audio = control_audio();
        let expected = audio.clock();
        publish_clock(&audio.state, &audio.stop, 16, control_clock(41, 9), None);
        assert_eq!(audio.clock(), expected);
        publish_clock(&audio.state, &audio.stop, 17, control_clock(40, 9), None);
        assert_eq!(audio.clock(), expected);
        publish_clock(&audio.state, &audio.stop, 17, control_clock(41, 9), None);
        assert_eq!(audio.clock(), Some(control_clock(41, 9)));
        audio.stop();
        publish_clock(&audio.state, &audio.stop, 18, control_clock(41, 10), None);
        assert!(audio.clock().is_none());
    }

    #[test]
    fn preview_audio_keeps_real_drain_receipt_after_pause_without_reusing_it_for_new_generation() {
        let audio = control_audio();
        let mut ended = control_clock(41, 9);
        ended.clock.phase = ClockPhase::Ended;
        ended.clock.advancing = false;
        ended.clock.queued_media = Duration::ZERO;
        publish_clock(&audio.state, &audio.stop, 16, ended, None);
        assert!(audio.drained_clock(41).is_none());
        publish_clock(&audio.state, &audio.stop, 17, ended, None);
        assert_eq!(audio.drained_clock(41), Some(ended));
        audio.set_transport(42, None, false, 1.);
        assert!(audio.clock().is_none());
        assert!(audio.drained_clock(42).is_none());
        assert_eq!(audio.drained_clock(41), Some(ended));
        let mut canceled = ended;
        canceled.transport_generation = 42;
        canceled.clock.phase = ClockPhase::Paused;
        let revision = lock_state(&audio.state).generation;
        publish_clock(&audio.state, &audio.stop, revision, canceled, None);
        assert!(audio.drained_clock(42).is_none());
    }

    #[test]
    fn preview_audio_same_transport_generation_commits_one_revision_per_update() {
        let audio = control_audio();
        audio.set_transport(41, Some(Duration::from_secs(7)), true, 2.);
        let first = lock_state(&audio.state).clone();
        assert_eq!((first.generation, first.transport_generation), (18, 41));
        assert_eq!(
            (first.position, first.playing, first.rate),
            (Some(Duration::from_secs(7)), true, 2.)
        );
        assert_eq!(first.published, None);
        lock_state(&audio.state).published = Some(control_clock(41, 9));
        audio.set_transport(41, None, false, 0.5);
        let second = lock_state(&audio.state).clone();
        assert_eq!((second.generation, second.transport_generation), (19, 41));
        assert_eq!(
            (second.position, second.playing, second.rate),
            (None, false, 0.5)
        );
        assert_eq!(second.published, None);
    }

    #[test]
    fn preview_audio_legacy_pause_rate_and_seek_keep_target_contract() {
        let audio = control_audio();
        audio.pause();
        let paused = lock_state(&audio.state).clone();
        assert_eq!((paused.generation, paused.transport_generation), (18, 42));
        assert_eq!((paused.position, paused.playing), (None, false));
        audio.set_rate(2.);
        let rate = lock_state(&audio.state).clone();
        assert_eq!((rate.generation, rate.transport_generation), (19, 43));
        assert_eq!((rate.position, rate.rate), (None, 2.));
        audio.seek(Duration::from_secs(9));
        let sought = lock_state(&audio.state).clone();
        assert_eq!((sought.generation, sought.transport_generation), (20, 44));
        assert_eq!(
            (sought.position, sought.playing),
            (Some(Duration::from_secs(9)), false)
        );
        assert_eq!(sought.published, None);
    }

    #[test]
    fn preview_audio_clock_filters_transport_tag_and_late_publication_after_stop() {
        let audio = control_audio();
        lock_state(&audio.state).published = Some(control_clock(40, 8));
        assert_eq!(audio.clock(), None);
        let valid = control_clock(41, u64::MAX);
        lock_state(&audio.state).published = Some(valid);
        assert_eq!(audio.clock(), Some(valid));
        audio.stop();
        let stopped = lock_state(&audio.state).clone();
        assert!(!stopped.playing);
        assert_eq!(stopped.generation, 18);
        assert_eq!(stopped.published, None);
        lock_state(&audio.state).published = Some(valid);
        assert_eq!(audio.clock(), None);
    }

    #[test]
    fn preview_audio_file_switch_is_atomic_and_same_generation_only_updates_transport() {
        let audio = control_audio();
        *audio.error.lock().unwrap() = Some("old track failed".into());
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(7),
            Duration::from_secs(9),
            false,
            2.,
        );
        let first = lock_state(&audio.state).clone();
        let file = first.file.unwrap();
        assert_eq!((first.generation, first.transport_generation), (18, 42));
        assert_eq!(
            (file.revision, file.pending_origin),
            (3, Some(Duration::from_secs(9)))
        );
        assert_eq!(
            audio.file_selection(),
            Some(AudioStreamSelection::Stream(7))
        );
        assert!(audio.stream_switching());
        assert!(audio.clock().is_none());
        assert!(audio.audio_info().is_none());
        assert!(audio.error().is_none());
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(7),
            Duration::from_secs(10),
            true,
            2.,
        );
        let resumed = lock_state(&audio.state).clone();
        assert_eq!(resumed.generation, 19);
        assert_eq!(resumed.file.unwrap().revision, file.revision);
        assert_eq!(
            resumed.file.unwrap().pending_origin,
            Some(Duration::from_secs(10))
        );
        assert!(resumed.playing);
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(7),
            Duration::from_secs(10),
            true,
            2.,
        );
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(8),
            Duration::from_secs(10),
            true,
            2.,
        );
        assert_eq!(lock_state(&audio.state).generation, resumed.generation);
        assert_eq!(
            audio.file_selection(),
            Some(AudioStreamSelection::Stream(7))
        );
    }

    #[test]
    fn preview_audio_file_origin_survives_rate_none_until_current_reanchor_is_confirmed() {
        let audio = control_audio();
        let origin = Duration::from_secs(9);
        let sought = Duration::from_secs(12);
        audio.select_file_stream(42, AudioStreamSelection::Stream(7), origin, false, 1.);
        let opening = lock_state(&audio.state).clone();
        audio.set_transport(43, None, true, 0.5);
        assert_eq!(
            lock_state(&audio.state).reset_origin(Duration::ZERO),
            origin
        );
        audio.set_transport(44, Some(sought), false, 1.);
        audio.set_transport(45, None, true, 0.5);
        let changed = lock_state(&audio.state).clone();
        assert_eq!(changed.position, None);
        assert_eq!(changed.reset_origin(Duration::ZERO), sought);
        assert_eq!(
            changed.file.unwrap().revision,
            opening.file.unwrap().revision
        );
        assert!(!confirm_timeline(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            &opening,
            &[(48_000, 2)]
        ));
        assert_eq!(
            lock_state(&audio.state).file.unwrap().pending_origin,
            Some(sought)
        );
        // 旧选轨命令的晚到播放后继也不能覆盖较新的倍率命令。
        audio.select_file_stream(42, AudioStreamSelection::Stream(7), origin, true, 1.);
        assert_eq!(lock_state(&audio.state).generation, changed.generation);
        assert!(confirm_timeline(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            &changed,
            &[(44_100, 1)]
        ));
        assert_eq!(lock_state(&audio.state).file.unwrap().pending_origin, None);
        assert!(!audio.stream_switching());
        assert_eq!(audio.audio_info(), Some(vec![(44_100, 1)]));
        audio.set_transport(46, None, true, 2.);
        let confirmed = lock_state(&audio.state).clone();
        assert_eq!(confirmed.file.unwrap().pending_origin, None);
        assert_eq!(
            confirmed.reset_origin(Duration::from_secs(17)),
            Duration::from_secs(17)
        );
    }

    #[test]
    fn preview_audio_late_transport_cannot_overwrite_a_new_file_selection() {
        let audio = control_audio();
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(7),
            Duration::from_secs(9),
            true,
            2.,
        );
        lock_state(&audio.state).published = Some(control_clock(42, 1));
        let before = lock_state(&audio.state).clone();
        audio.set_transport(41, Some(Duration::from_secs(2)), false, 0.5);
        let after = lock_state(&audio.state).clone();
        assert_eq!(
            (
                after.generation,
                after.transport_generation,
                after.position,
                after.playing,
                after.rate,
                after.file,
                after.published
            ),
            (
                before.generation,
                before.transport_generation,
                before.position,
                before.playing,
                before.rate,
                before.file,
                before.published
            ),
        );
        assert_eq!(audio.clock(), before.published);
        assert!(audio.stream_switching());
    }

    #[test]
    fn preview_audio_failed_or_disabled_track_can_be_replaced_without_accepting_stale_results() {
        let audio = control_audio();
        audio.select_file_stream(
            42,
            AudioStreamSelection::Stream(99),
            Duration::from_secs(5),
            false,
            1.,
        );
        let failed = lock_state(&audio.state).clone();
        assert!(finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            failed.attempt_key(),
            None,
            Some("bad track".into())
        ));
        assert!(!audio.stream_switching());
        assert_eq!(
            audio.file_selection(),
            Some(AudioStreamSelection::Stream(99))
        );
        assert_eq!(audio.error().as_deref(), Some("bad track"));
        audio.set_transport(43, None, true, 2.);
        assert_eq!(
            lock_state(&audio.state).stream_revision(),
            failed.stream_revision()
        );
        assert_eq!(audio.error().as_deref(), Some("bad track"));
        audio.select_file_stream(
            44,
            AudioStreamSelection::Stream(1),
            Duration::from_secs(6),
            false,
            2.,
        );
        let good = lock_state(&audio.state).clone();
        assert!(audio.error().is_none());
        assert!(!finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            failed.attempt_key(),
            None,
            Some("late failure".into())
        ));
        assert!(audio.stream_switching());
        assert!(confirm_timeline(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            &good,
            &[(48_000, 2)]
        ));
        audio.select_file_stream(
            45,
            AudioStreamSelection::Disabled,
            Duration::from_secs(7),
            false,
            2.,
        );
        let disabled = lock_state(&audio.state).attempt_key();
        assert!(finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            disabled,
            Some(Vec::new()),
            None
        ));
        assert_eq!(audio.file_selection(), Some(AudioStreamSelection::Disabled));
        assert_eq!(audio.audio_info(), Some(Vec::new()));
        assert!(!audio.stream_switching());
        audio.select_file_stream(
            46,
            AudioStreamSelection::First,
            Duration::from_secs(8),
            true,
            1.,
        );
        assert_eq!(audio.file_selection(), Some(AudioStreamSelection::First));
        assert!(audio.stream_switching());
        assert_eq!(
            lock_state(&audio.state).reset_origin(Duration::ZERO),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn preview_audio_wav_roles_ignore_file_stream_controls_and_stop_rejects_completion() {
        let mut audio = control_audio();
        audio.sources.push(AudioSource::System);
        lock_state(&audio.state).file = None;
        let before = lock_state(&audio.state).clone();
        audio.select_file_stream(
            42,
            AudioStreamSelection::Disabled,
            Duration::ZERO,
            false,
            1.,
        );
        assert_eq!(audio.file_selection(), None);
        assert_eq!(lock_state(&audio.state).generation, before.generation);
        assert_eq!(audio.clock(), before.published);
        audio.stop();
        assert!(!confirm_timeline(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            &before,
            &[(48_000, 2)]
        ));
        assert!(!finish_file_attempt(
            &audio.state,
            &audio.stop,
            &audio.info,
            &audio.error,
            before.attempt_key(),
            Some(Vec::new()),
            None
        ));
    }
}
