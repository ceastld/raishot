//! WASAPI 共享模式采集：系统输出设备环回与麦克风，支持系统默认或固定端点。
//! 每个来源在独立线程转换为 48 kHz 立体声 f32；混音由录屏线程按成片时间轴取数。
use crate::{
    audio_devices::{default_id, resolve_endpoint, EndpointKind, EndpointSelection},
    media_foundation::{AUDIO_CHANNELS, AUDIO_RATE},
    media_playback_sync::WorkerExit,
    Result,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[path = "audio_capture_buffer.rs"]
mod capture_buffer;
use capture_buffer::CaptureBuffer;
pub(crate) use capture_buffer::{CaptureRead, CaptureSegment, RawAudioTime, MAX_TAKE_FRAMES};
#[path = "audio_capture_clock.rs"]
mod capture_clock;
use capture_clock::CaptureClockObserver;
pub(crate) use capture_clock::CaptureClockSnapshot;
#[path = "audio_capture_phase.rs"]
mod capture_phase;
pub(crate) use capture_phase::{OutputPhase, PhaseError};
#[path = "audio_capture_meter.rs"]
mod capture_meter;
use capture_meter::SourceMonitor;
pub(crate) use capture_meter::{CaptureLevel, CaptureSourceSnapshot, CaptureSourceState};
#[path = "audio_input_check.rs"]
mod input_check;
pub(crate) use input_check::AudioInputCheck;
use windows::Win32::{
    Media::{
        Audio::*,
        KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
        Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
    },
    System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    },
};

/// 缓冲超过此长度说明来源时钟领先或混音停滞，丢弃最旧部分以限制延迟。
const MAX_BUFFERED_FRAMES: usize = AUDIO_RATE as usize * 3 / 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioSource {
    System,
    Microphone,
}
impl AudioSource {
    fn label(self) -> &'static str {
        match self {
            AudioSource::System => tr!("系统音频", "System audio"),
            AudioSource::Microphone => tr!("麦克风", "Microphone"),
        }
    }
}

struct Shared {
    buffer: Mutex<CaptureBuffer>,
    clock: Mutex<CaptureClockObserver>,
    monitor: Mutex<SourceMonitor>,
    worker_exit: WorkerExit,
    error: Mutex<Option<String>>,
    warning: Mutex<Option<String>>,
    ready: AtomicBool,
}

struct SourceThread {
    source: AudioSource,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for SourceThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) struct AudioCapture {
    sources: Vec<SourceThread>,
}
impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop_and_wait();
    }
}
impl AudioCapture {
    /// 停止全部来源并等待终态；保留缓冲和错误，便于调用方收集末次诊断。
    pub(crate) fn stop_and_wait(&mut self) {
        // 先通知所有来源，避免等待首个线程时其它来源仍继续采集。
        for source in &self.sources {
            source.stop.store(true, Ordering::Release);
        }
        for source in &mut self.sources {
            if let Some(thread) = source.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// 打开请求的来源；全部失败时返回错误，部分失败时记为警告继续录制。
    pub(crate) fn start(sources: &[AudioSource]) -> Result<(Self, Vec<String>)> {
        Self::start_with_devices(
            sources,
            &EndpointSelection::Default,
            &EndpointSelection::Default,
        )
    }

    /// 固定端点临时不可用时回退默认并报告警告；调用方保存的选择保持原值。
    pub(crate) fn start_with_devices(
        sources: &[AudioSource],
        system_output: &EndpointSelection,
        microphone_input: &EndpointSelection,
    ) -> Result<(Self, Vec<String>)> {
        Self::start_with_devices_inner(sources, system_output, microphone_input, false)
    }

    /// 检测保留初始化失败的来源，以便逐项显示设备与错误；不改变录制的过滤语义。
    fn start_monitoring(
        sources: &[AudioSource],
        system_output: &EndpointSelection,
        microphone_input: &EndpointSelection,
    ) -> Result<(Self, Vec<String>)> {
        Self::start_with_devices_inner(sources, system_output, microphone_input, true)
    }

    fn start_with_devices_inner(
        sources: &[AudioSource],
        system_output: &EndpointSelection,
        microphone_input: &EndpointSelection,
        keep_failed: bool,
    ) -> Result<(Self, Vec<String>)> {
        // 各来源并行初始化，开始录制只等最慢的一个设备，而不是逐个累加。
        let mut pending = Vec::new();
        for &source in sources {
            let selection = match source {
                AudioSource::System => system_output,
                AudioSource::Microphone => microphone_input,
            }
            .clone();
            let stop = Arc::new(AtomicBool::new(false));
            let shared = Arc::new(Shared {
                buffer: Mutex::new(CaptureBuffer::new(MAX_BUFFERED_FRAMES)?),
                clock: Mutex::new(CaptureClockObserver::new()),
                monitor: Mutex::new(SourceMonitor::new(selection.clone())),
                worker_exit: WorkerExit::pending(),
                error: Mutex::new(None),
                warning: Mutex::new(None),
                ready: AtomicBool::new(false),
            });
            let thread = {
                let stop = stop.clone();
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name("rustshot-audio".into())
                    .spawn(move || {
                        let _exited = shared.worker_exit.guard();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_source(source, &selection, &stop, &shared)
                        }));
                        let error = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(error)) => Some(error.to_string()),
                            Err(_) => Some(
                                tr!("音频采集线程意外停止", "Audio capture stopped unexpectedly")
                                    .into(),
                            ),
                        };
                        if error.is_some() {
                            *shared
                                .error
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) = error;
                        }
                        shared.ready.store(true, Ordering::Release);
                    })?
            };
            pending.push(SourceThread {
                source,
                stop,
                shared,
                thread: Some(thread),
            });
        }
        // 等待设备初始化，让开始录制前就能报告缺少设备。
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline
            && !pending
                .iter()
                .all(|entry| entry.shared.ready.load(Ordering::Acquire))
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut running = Vec::new();
        let mut warnings = Vec::new();
        for entry in pending {
            if let Some(error) = entry.shared.error.lock().unwrap().clone() {
                warnings.push(trf!(
                    "{}不可用：{error}",
                    "{} unavailable: {error}",
                    entry.source.label()
                ));
                if !keep_failed {
                    continue;
                }
            }
            if let Some(warning) = entry.shared.warning.lock().unwrap().clone() {
                warnings.push(warning);
            }
            running.push(entry);
        }
        if running.is_empty() && !sources.is_empty() && !keep_failed {
            return Err(warnings.join("；").into());
        }
        Ok((Self { sources: running }, warnings))
    }

    pub(crate) fn source_status(&self) -> Vec<CaptureSourceSnapshot> {
        let now = Instant::now();
        self.sources
            .iter()
            .map(|entry| {
                let error = entry
                    .shared
                    .error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let warning = entry
                    .shared
                    .warning
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                entry
                    .shared
                    .monitor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .snapshot(
                        entry.source,
                        entry.shared.worker_exit.is_exited(),
                        error,
                        warning,
                        now,
                    )
            })
            .collect()
    }

    /// 来源警告与运行中错误（设备拔出等）；录制继续，缺少的音轨补静音。
    pub(crate) fn errors(&self) -> Vec<String> {
        self.sources
            .iter()
            .flat_map(|s| {
                let error = s
                    .shared
                    .error
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|e| trf!("{}中断：{e}", "{} interrupted: {e}", s.source.label()));
                s.shared
                    .warning
                    .lock()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .chain(error)
            })
            .collect()
    }

    /// 暂停期间丢弃已采集的声音，成片不出现暂停时段的音频。
    pub(crate) fn discard(&self) {
        for source in &self.sources {
            source.shared.buffer.lock().unwrap().discard();
        }
    }

    /// 已成功打开的来源；顺序与 `take` 返回的分轨一致。
    pub(crate) fn sources(&self) -> Vec<AudioSource> {
        self.sources.iter().map(|s| s.source).collect()
    }

    /// 每个来源各取 frames 个立体声帧（分轨，不混音）；来源不足时补静音。
    pub(crate) fn take(&self, frames: usize) -> Vec<Vec<f32>> {
        self.sources
            .iter()
            .map(|source| {
                let result = (|| -> Result<Vec<f32>> {
                    let count = frames
                        .checked_mul(AUDIO_CHANNELS as usize)
                        .ok_or("音频请求过大")?;
                    let mut extended = Vec::new();
                    if frames > MAX_TAKE_FRAMES {
                        // 先验证最终分配，失败不能消费已有 PCM 与时间片段。
                        extended.try_reserve_exact(count)?;
                    }
                    // 缓存远小于单次读取上限；更长的旧请求只需在末尾补静音。
                    let mut track = source
                        .shared
                        .buffer
                        .lock()
                        .unwrap()
                        .take(frames.min(MAX_TAKE_FRAMES))?
                        .samples;
                    if frames > MAX_TAKE_FRAMES {
                        extended.extend(track);
                        track = extended;
                    }
                    track.resize(count, 0.);
                    Ok(track)
                })();
                match result {
                    Ok(track) => track,
                    Err(error) => {
                        *source.shared.error.lock().unwrap() = Some(error.to_string());
                        Vec::new()
                    }
                }
            })
            .collect()
    }

    /// 时间片段只描述真实采集帧；不足补零没有设备时间锚。
    /// 请求超过 MAX_TAKE_FRAMES 时，在消费任何来源前返回错误。
    pub(crate) fn take_with_timing(
        &self,
        frames: usize,
    ) -> Result<Vec<(AudioSource, CaptureRead)>> {
        if frames > MAX_TAKE_FRAMES {
            return Err(
                capture_buffer::CaptureBufferError::TakeTooLarge { requested: frames }.into(),
            );
        }
        self.sources
            .iter()
            .map(|source| {
                let read = source.shared.buffer.lock().unwrap().take(frames)?;
                Ok((source.source, read))
            })
            .collect()
    }

    /// 每源只取当前可用的真实帧，不补静音；时间窗口与缺口由消费者决定。
    /// 在消费任何来源前验证上限，单源采集锁内决定真实可用数量。
    pub(crate) fn take_available_with_timing(
        &self,
        max_frames: usize,
    ) -> Result<Vec<(AudioSource, CaptureRead)>> {
        if max_frames > MAX_TAKE_FRAMES {
            return Err(capture_buffer::CaptureBufferError::TakeTooLarge {
                requested: max_frames,
            }
            .into());
        }
        self.sources
            .iter()
            .map(|source| {
                let read = source
                    .shared
                    .buffer
                    .lock()
                    .unwrap()
                    .take_available(max_frames)?;
                Ok((source.source, read))
            })
            .collect()
    }

    /// 每源的原始设备时钟观测，不代表成片音画同步或已经校正 PCM。
    pub(crate) fn clock_diagnostics(&self) -> Vec<(AudioSource, CaptureClockSnapshot)> {
        self.sources
            .iter()
            .map(|source| {
                (
                    source.source,
                    source.shared.clock.lock().unwrap().snapshot(),
                )
            })
            .collect()
    }
}

/// 会话初始、预览与最终混音的音量（百分比 0–400）；默认系统 100%、麦克风 200%。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct MixLevels {
    pub system: u32,
    pub microphone: u32,
}
impl Default for MixLevels {
    fn default() -> Self {
        Self {
            system: 100,
            microphone: 200,
        }
    }
}
impl MixLevels {
    pub(crate) const MAX: u32 = 400;
    pub(crate) fn normalized(self) -> Self {
        Self {
            system: self.system.min(Self::MAX),
            microphone: self.microphone.min(Self::MAX),
        }
    }
    pub(crate) fn gain(&self, source: AudioSource) -> f32 {
        let percent = match source {
            AudioSource::System => self.system,
            AudioSource::Microphone => self.microphone,
        };
        percent.min(Self::MAX) as f32 / 100.
    }
}

/// 按音量混合等长分轨为 16 位交错 PCM，超出范围削波。
pub(crate) fn mix_tracks(tracks: &[(AudioSource, &[f32])], levels: MixLevels) -> Vec<i16> {
    let count = tracks.iter().map(|(_, t)| t.len()).max().unwrap_or(0);
    let mut mixed = vec![0_f32; count];
    for (source, track) in tracks {
        let gain = levels.gain(*source);
        for (target, value) in mixed.iter_mut().zip(track.iter()) {
            *target += value * gain;
        }
    }
    mixed
        .into_iter()
        .map(|value| (value.clamp(-1., 1.) * 32767.) as i16)
        .collect()
}

struct ComGuard(bool);
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct MixFormatGuard(*mut WAVEFORMATEX);
impl Drop for MixFormatGuard {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleFormat {
    Float32,
    Int16,
    Int24,
    Int32,
}

#[derive(Clone, Copy, Debug)]
struct Format {
    rate: u32,
    channels: usize,
    sample: SampleFormat,
    block: usize,
}

unsafe fn parse_format(format: *const WAVEFORMATEX) -> Result<Format> {
    let base = unsafe { std::ptr::read_unaligned(format) };
    let tag = u32::from(base.wFormatTag);
    let bits = base.wBitsPerSample;
    let float = if tag == WAVE_FORMAT_EXTENSIBLE && base.cbSize >= 22 {
        let extensible = unsafe { std::ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>()) };
        let sub = extensible.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
            false
        } else {
            return Err(tr!("不支持的音频子格式", "Unsupported audio subformat").into());
        }
    } else if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_PCM {
        false
    } else {
        return Err(trf!("不支持的音频格式 {tag}", "Unsupported audio format {tag}").into());
    };
    let sample = match (float, bits) {
        (true, 32) => SampleFormat::Float32,
        (false, 16) => SampleFormat::Int16,
        (false, 24) => SampleFormat::Int24,
        (false, 32) => SampleFormat::Int32,
        _ => {
            return Err(trf!(
                "不支持的音频位深 {bits}",
                "Unsupported audio bit depth {bits}"
            )
            .into())
        }
    };
    if base.nChannels == 0
        || base.nChannels > 32
        || base.nSamplesPerSec == 0
        || base.nSamplesPerSec > 384_000
    {
        return Err(tr!(
            "音频声道数或采样率无效或超过支持范围",
            "Audio channel count or sample rate is invalid or exceeds the supported range"
        )
        .into());
    }
    let channels = usize::from(base.nChannels);
    let block = usize::from(base.nBlockAlign);
    if block < channels * usize::from(bits / 8) {
        return Err(tr!(
            "音频帧步长小于各声道采样所需长度",
            "Audio block alignment is smaller than the channel samples"
        )
        .into());
    }
    Ok(Format {
        rate: base.nSamplesPerSec,
        channels,
        sample,
        block,
    })
}

fn read_sample(bytes: &[u8], format: SampleFormat) -> f32 {
    match format {
        SampleFormat::Float32 => {
            let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if value.is_finite() {
                value
            } else {
                0.
            }
        }
        SampleFormat::Int16 => f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.,
        SampleFormat::Int24 => {
            (i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8) as f32 / 8_388_608.
        }
        SampleFormat::Int32 => {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.
        }
    }
}

/// 设备帧转换为立体声：单声道复制，多声道取前左右并混入中置。
fn to_stereo(data: &[u8], frames: usize, format: &Format) -> Vec<[f32; 2]> {
    let width = match format.sample {
        SampleFormat::Int16 => 2,
        SampleFormat::Int24 => 3,
        SampleFormat::Float32 | SampleFormat::Int32 => 4,
    };
    let mut out = Vec::with_capacity(frames);
    for frame in 0..frames {
        let base = frame * format.block;
        let channel = |index: usize| {
            let start = base + index * width;
            data.get(start..start + width)
                .map_or(0., |bytes| read_sample(bytes, format.sample))
        };
        out.push(match format.channels {
            1 => {
                let v = channel(0);
                [v, v]
            }
            2 => [channel(0), channel(1)],
            _ => {
                let center = channel(2) * 0.707;
                [channel(0) + center, channel(1) + center]
            }
        });
    }
    out
}

/// 有状态线性重采样；跨包保留上一帧，避免包边界咔哒声。
pub(crate) struct Resampler {
    input_rate: u32,
    output_rate: u32,
    phase_num: i64,
    previous: [f32; 2],
}
impl Resampler {
    pub(crate) fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate,
            output_rate,
            phase_num: 0,
            previous: [0.; 2],
        }
    }
    fn reset(&mut self) {
        self.phase_num = 0;
        self.previous = [0.; 2];
    }

    /// 相位以输出采样率为分母，避免长时间累计浮点误差；空输入不改变历史。
    pub(crate) fn process(
        &mut self,
        input: &[[f32; 2]],
        output: &mut Vec<f32>,
    ) -> Option<OutputPhase> {
        let n = input.len();
        if n == 0
            || !(1..=384_000).contains(&self.input_rate)
            || !(1..=384_000).contains(&self.output_rate)
        {
            return None;
        }
        let before = output.len();
        let first_input_offset_num = self.phase_num;
        let denominator = i128::from(self.output_rate);
        let mut phase = i128::from(self.phase_num);
        let limit = (n as i128 - 1) * denominator;
        let previous = self.previous;
        let at = |index: i128| -> [f32; 2] {
            if index < 0 {
                previous
            } else {
                input[index as usize]
            }
        };
        while phase <= limit {
            let index = phase.div_euclid(denominator);
            let fraction = (phase.rem_euclid(denominator) as f64 / denominator as f64) as f32;
            let a = at(index);
            let b = if index + 1 < n as i128 {
                input[(index + 1) as usize]
            } else {
                a
            };
            output.push(a[0] + (b[0] - a[0]) * fraction);
            output.push(a[1] + (b[1] - a[1]) * fraction);
            phase += i128::from(self.input_rate);
        }
        // 循环停止后相位位于 [-output_rate, input_rate)，与包长度无关。
        self.phase_num = (phase - n as i128 * denominator) as i64;
        self.previous = input[n - 1];
        let output_frames = (output.len() - before) / 2;
        (output_frames != 0).then_some(OutputPhase {
            first_input_offset_num,
            input_rate: self.input_rate,
            output_rate: self.output_rate,
            output_frames,
        })
    }
}

fn capture_packets_contiguous(previous: Option<RawAudioTime>, current: RawAudioTime) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    !current.discontinuity
        && previous.endpoint_epoch == current.endpoint_epoch
        && previous.device_rate == current.device_rate
        && previous
            .device_position
            .checked_add(u64::from(previous.input_frames))
            == Some(current.device_position)
        && previous
            .qpc_100ns
            .zip(current.qpc_100ns)
            .is_some_and(|(before, now)| now > before)
}

fn run_source(
    source: AudioSource,
    selection: &EndpointSelection,
    stop: &AtomicBool,
    shared: &Shared,
) -> Result<()> {
    let _com = ComGuard(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok());
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let kind = match source {
            AudioSource::System => EndpointKind::Render,
            AudioSource::Microphone => EndpointKind::Capture,
        };
        let resolved = resolve_endpoint(&enumerator, kind, selection)?;
        let follows_default =
            matches!(selection, EndpointSelection::Default) || resolved.fallback.is_some();
        let endpoint_id = resolved.info.id.clone();
        shared
            .monitor
            .lock()
            .unwrap()
            .resolved(resolved.info.clone(), resolved.fallback);
        if let Some(reason) = resolved.fallback {
            let name = &resolved.info.name;
            *shared.warning.lock().unwrap() = Some(trf!(
                "{}指定设备不可用（{reason}），本次使用默认设备 {name}",
                "{} selected endpoint unavailable ({reason}); using default endpoint {name}",
                source.label()
            ));
        }
        let device = resolved.device;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix_format = MixFormatGuard(client.GetMixFormat()?);
        let mix = mix_format.0;
        let format = parse_format(mix);
        let flags = if source == AudioSource::System {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, mix, None);
        // 环回在无声时不产出数据包；同设备播放静音流保持时间轴连续。
        let keepalive = if source == AudioSource::System && init.is_ok() {
            let render: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            render
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix, None)
                .ok()
                .and_then(|()| {
                    let writer: IAudioRenderClient = render.GetService().ok()?;
                    let size = render.GetBufferSize().ok()?;
                    writer.GetBuffer(size).ok()?;
                    writer
                        .ReleaseBuffer(size, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                        .ok()?;
                    render.Start().ok()?;
                    Some((render, writer, size))
                })
        } else {
            None
        };
        drop(mix_format);
        let format = format?;
        init.map_err(|e| format!("无法初始化设备：{e}"))?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        shared.monitor.lock().unwrap().started(Instant::now());
        shared.ready.store(true, Ordering::Release);
        let mut resampler = Resampler::new(format.rate, AUDIO_RATE);
        let mut previous_packet = None;
        let mut converted = Vec::new();
        let mut check_default_at = std::time::Instant::now() + Duration::from_secs(1);
        let result = (|| -> Result<()> {
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if follows_default && std::time::Instant::now() >= check_default_at {
                    check_default_at = std::time::Instant::now() + Duration::from_secs(1);
                    let current_default = default_id(&enumerator, kind).map_err(|reason| {
                        trf!(
                            "无法检查默认音频设备：{reason}",
                            "Cannot check the default audio endpoint: {reason}"
                        )
                    })?;
                    if current_default.as_deref() != Some(endpoint_id.as_str()) {
                        return Err(tr!(
                            "默认设备已变化，此音频来源已停止；请重新开始录制以使用新设备",
                            "Default endpoint changed. This audio source stopped; restart recording to use the new endpoint."
                        ).into());
                    }
                }
                if let Some((render, writer, size)) = &keepalive {
                    let padding = render.GetCurrentPadding().unwrap_or(*size);
                    let free = size.saturating_sub(padding);
                    if free > 0 && writer.GetBuffer(free).is_ok() {
                        let _ = writer.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
                    }
                }
                while !stop.load(Ordering::Acquire) && capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0_u32;
                    let mut flags = 0_u32;
                    let mut device_position = 0;
                    let mut qpc_100ns = 0;
                    capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        Some(&mut device_position),
                        Some(&mut qpc_100ns),
                    )?;
                    let frames_usize = frames as usize;
                    let stereo = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0
                        || data.is_null()
                    {
                        vec![[0_f32; 2]; frames_usize]
                    } else {
                        let bytes = std::slice::from_raw_parts(data, frames_usize * format.block);
                        to_stereo(bytes, frames_usize, &format)
                    };
                    capture.ReleaseBuffer(frames)?;
                    shared.monitor.lock().unwrap().observe(
                        &stereo,
                        flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
                        Instant::now(),
                    );
                    converted.clear();
                    let raw_time = RawAudioTime {
                        endpoint_epoch: 0,
                        device_position,
                        qpc_100ns: (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0
                            && qpc_100ns != 0)
                            .then_some(qpc_100ns),
                        device_rate: format.rate,
                        input_frames: frames,
                        discontinuity: flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
                    };
                    shared.clock.lock().unwrap().observe(raw_time);
                    if !capture_packets_contiguous(previous_packet, raw_time) {
                        resampler.reset();
                    }
                    let phase = resampler.process(&stereo, &mut converted);
                    previous_packet = Some(raw_time);
                    shared
                        .buffer
                        .lock()
                        .unwrap()
                        .push_with_phase(&converted, raw_time, phase)?;
                }
            }
            Ok(())
        })();
        let mut errors = Vec::new();
        if let Err(error) = result {
            errors.push(trf!("采集中断：{error}", "Audio capture interrupted: {error}"));
        }
        if let Err(error) = client.Stop() {
            errors.push(trf!(
                "停止音频采集失败：{error}",
                "Failed to stop audio capture: {error}"
            ));
        }
        if let Some((render, _, _)) = &keepalive {
            if let Err(error) = render.Stop() {
                errors.push(trf!(
                    "停止音频保活流失败：{error}",
                    "Failed to stop audio keepalive: {error}"
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join(tr!("；", "; ")).into())
        }
    }
}

#[cfg(test)]
#[path = "audio_capture_shutdown_tests.rs"]
mod shutdown_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_capture_legacy_allocation_failure_does_not_consume_real_pcm() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        buffer
            .push(
                &[0.25, -0.25],
                RawAudioTime {
                    endpoint_epoch: 0,
                    device_position: 0,
                    qpc_100ns: Some(10),
                    device_rate: AUDIO_RATE,
                    input_frames: 1,
                    discontinuity: false,
                },
            )
            .unwrap();
        let capture = AudioCapture {
            sources: vec![SourceThread {
                source: AudioSource::System,
                stop: Arc::new(AtomicBool::new(false)),
                shared: Arc::new(Shared {
                    buffer: Mutex::new(buffer),
                    clock: Mutex::new(CaptureClockObserver::new()),
                    monitor: Mutex::new(SourceMonitor::new(EndpointSelection::Default)),
                    worker_exit: WorkerExit::completed(),
                    error: Mutex::new(None),
                    warning: Mutex::new(None),
                    ready: AtomicBool::new(true),
                }),
                thread: None,
            }],
        };
        assert!(capture.take(usize::MAX / 2)[0].is_empty());
        assert_eq!(
            capture.sources[0]
                .shared
                .buffer
                .lock()
                .unwrap()
                .len_frames(),
            1
        );
        let mut reads = capture.take_with_timing(1).unwrap();
        let read = reads.pop().unwrap().1;
        assert_eq!(read.samples, [0.25, -0.25]);
        assert_eq!(read.segments[0].raw_time.qpc_100ns, Some(10));
    }

    #[test]
    fn audio_resampler_preserves_rate_and_continuity() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut output = Vec::new();
        let input: Vec<[f32; 2]> = (0..44_100).map(|i| [i as f32, -(i as f32)]).collect();
        for chunk in input.chunks(441) {
            resampler.process(chunk, &mut output).unwrap();
        }
        let frames = output.len() / 2;
        assert!((47_990..=48_000).contains(&frames), "{frames}");
        for pair in output.chunks_exact(4) {
            assert!(pair[2] >= pair[0], "单调输入在包边界不能回跳");
        }
        let mut same = Resampler::new(48_000, 48_000);
        let mut copy = Vec::new();
        same.process(&[[0.5, 0.25], [1., 0.]], &mut copy).unwrap();
        assert_eq!(copy, vec![0.5, 0.25, 1., 0.]);
    }

    #[test]
    fn audio_resampler_integer_phase_is_independent_of_packet_boundaries() {
        let input: Vec<_> = (0..1003)
            .map(|i| [(i % 29) as f32 / 30., -(i % 19) as f32 / 20.])
            .collect();
        for (input_rate, output_rate) in [
            (44_100, 48_000),
            (48_000, 48_000),
            (192_000, 48_000),
            (48_000, 44_100),
        ] {
            let mut whole = Resampler::new(input_rate, output_rate);
            let mut expected = Vec::new();
            whole.process(&input, &mut expected).unwrap();
            let mut split = Resampler::new(input_rate, output_rate);
            let mut actual = Vec::new();
            let mut offset = 0;
            for count in [1, 17, 3, 400, 2, 59].into_iter().cycle() {
                let end = (offset + count).min(input.len());
                let before = actual.len();
                if let Some(phase) = split.process(&input[offset..end], &mut actual) {
                    assert!(phase.is_valid());
                    assert_eq!(phase.output_frames * 2, actual.len() - before);
                } else {
                    assert_eq!(actual.len(), before);
                }
                offset = end;
                if offset == input.len() {
                    break;
                }
            }
            assert_eq!(actual, expected, "{input_rate}→{output_rate}");
        }
    }

    #[test]
    fn audio_resampler_exposes_negative_phase_and_resets_cross_gap_history() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut output = Vec::new();
        let first = resampler.process(&vec![[1.; 2]; 441], &mut output).unwrap();
        assert_eq!(
            (first.first_input_offset_num, first.output_frames),
            (0, 479)
        );
        output.clear();
        let next = resampler.process(&vec![[0.; 2]; 441], &mut output).unwrap();
        assert_eq!(
            (next.first_input_offset_num, next.output_frames),
            (-44_100, 480)
        );
        assert!(output[0] > 0.);
        resampler.reset();
        output.clear();
        let after_gap = resampler.process(&[[0.; 2]], &mut output).unwrap();
        assert_eq!(after_gap.first_input_offset_num, 0);
        assert_eq!(output, [0., 0.]);
    }

    #[test]
    fn audio_capture_only_interpolates_across_contiguous_valid_packet_anchors() {
        let before = RawAudioTime {
            endpoint_epoch: 2,
            device_position: 10,
            qpc_100ns: Some(10_000),
            device_rate: 44_100,
            input_frames: 50,
            discontinuity: false,
        };
        let current = RawAudioTime {
            device_position: 60,
            qpc_100ns: Some(21_338),
            ..before
        };
        assert!(capture_packets_contiguous(Some(before), current));
        assert!(!capture_packets_contiguous(None, current));
        for invalid in [
            RawAudioTime {
                discontinuity: true,
                ..current
            },
            RawAudioTime {
                device_position: 61,
                ..current
            },
            RawAudioTime {
                endpoint_epoch: 3,
                ..current
            },
            RawAudioTime {
                device_rate: 48_000,
                ..current
            },
            RawAudioTime {
                qpc_100ns: None,
                ..current
            },
            RawAudioTime {
                qpc_100ns: Some(10_000),
                ..current
            },
        ] {
            assert!(!capture_packets_contiguous(Some(before), invalid));
        }
        assert!(!capture_packets_contiguous(
            Some(RawAudioTime {
                qpc_100ns: None,
                ..before
            }),
            current
        ));
    }

    #[test]
    fn audio_device_frames_convert_to_stereo() {
        let format = Format {
            rate: 48_000,
            channels: 1,
            sample: SampleFormat::Int16,
            block: 2,
        };
        let data = [0x00, 0x40];
        assert_eq!(to_stereo(&data, 1, &format), vec![[0.5, 0.5]]);
        let format = Format {
            rate: 48_000,
            channels: 2,
            sample: SampleFormat::Int24,
            block: 6,
        };
        let data = [0, 0, 0x40, 0, 0, 0xC0];
        assert_eq!(to_stereo(&data, 1, &format), vec![[0.5, -0.5]]);
    }

    fn capture_pcm_format(channels: u16, rate: u32, block: u16) -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: 16,
            cbSize: 0,
        }
    }

    #[test]
    fn audio_capture_format_rejects_blocks_shorter_than_channel_samples() {
        for (tag, bits, bytes) in [
            (WAVE_FORMAT_PCM, 16_u16, 2_u16),
            (WAVE_FORMAT_PCM, 24, 3),
            (WAVE_FORMAT_PCM, 32, 4),
            (WAVE_FORMAT_IEEE_FLOAT, 32, 4),
        ] {
            for block in [0, bytes * 2 - 1] {
                let mut format = capture_pcm_format(2, 48_000, block);
                format.wFormatTag = tag as u16;
                format.wBitsPerSample = bits;
                assert!(unsafe { parse_format(&format) }.is_err());
            }
            let mut format = capture_pcm_format(2, 48_000, bytes * 2);
            format.wFormatTag = tag as u16;
            format.wBitsPerSample = bits;
            assert!(unsafe { parse_format(&format) }.is_ok());
        }
    }

    #[test]
    fn audio_capture_format_bounds_channels_and_sample_rate() {
        for (channels, rate) in [(0, 48_000), (33, 48_000), (2, 0), (2, 384_001)] {
            let format = capture_pcm_format(channels, rate, channels * 2);
            assert!(unsafe { parse_format(&format) }.is_err());
        }
        let format = capture_pcm_format(32, 384_000, 64);
        let parsed = unsafe { parse_format(&format) }.unwrap();
        assert_eq!(
            (parsed.channels, parsed.rate, parsed.block),
            (32, 384_000, 64)
        );
    }

    #[test]
    fn audio_capture_format_accepts_frame_padding_without_mixing_it_into_samples() {
        let format = capture_pcm_format(2, 48_000, 6);
        let parsed = unsafe { parse_format(&format) }.unwrap();
        let data = [0, 0x40, 0, 0xC0, 0xFF, 0x7F, 0, 0xC0, 0, 0x40, 0, 0x80];
        assert_eq!(to_stereo(&data, 2, &parsed), vec![[0.5, -0.5], [-0.5, 0.5]]);
    }

    #[test]
    fn audio_capture_nonfinite_float_samples_become_silence() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(read_sample(&value.to_le_bytes(), SampleFormat::Float32), 0.);
        }
        for value in [-0.5_f32, 0., 1.25] {
            assert_eq!(
                read_sample(&value.to_le_bytes(), SampleFormat::Float32),
                value
            );
        }
    }
}
