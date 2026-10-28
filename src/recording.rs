//! 录屏会话引擎：DXGI 选区采集 + 指针/输入合成 + Media Foundation 边录边码。
//! GIF 预设先录高清 MP4，停止后转码；暂停不推进时间轴，成片时长等于实际录制时长。
#[path = "recording_audio_control.rs"]
mod audio_control;
#[path = "recording_audio_placement.rs"]
mod audio_placement;
#[path = "recording_audio_source.rs"]
mod audio_source;
#[path = "recording_bitrate_check.rs"]
pub(crate) mod bitrate_check;
#[path = "recording_camera.rs"]
pub(crate) mod camera;
#[path = "recording_camera_entry.rs"]
mod camera_entry;
#[path = "recording_clock.rs"]
mod clock;
#[path = "recording_control_receipt.rs"]
mod control_receipt;
#[path = "recording_control_state.rs"]
mod control_state;
#[cfg(test)]
#[path = "recording_deferred_trial.rs"]
mod deferred_trial;
#[path = "recording_encoding_report.rs"]
pub(crate) mod encoding_report;
#[path = "recording_live_annotation.rs"]
pub(crate) mod live_annotation;
#[path = "recording_output.rs"]
mod output;
#[path = "recording_output_plan.rs"]
pub(crate) mod output_plan;
#[path = "recording_recovery.rs"]
pub(crate) mod recovery;
#[path = "recording_scale_plan.rs"]
pub(crate) mod scale_plan;
#[path = "recording_spool.rs"]
pub(crate) mod spool;
#[path = "recording_stop.rs"]
mod stop;
#[cfg(test)]
#[path = "recording_video_lookbehind_native.rs"]
mod video_lookbehind_native;

use crate::{
    audio_capture::{
        mix_tracks, AudioCapture, AudioSource, CaptureRead, MixLevels, RawAudioTime,
        MAX_TAKE_FRAMES,
    },
    capture::cancellation::CaptureOutcome,
    geometry::ScreenRect,
    input_overlay::{region_scale, FramePainter},
    live_capture::LiveCapture,
    media_foundation::{
        classify_recording_error, recording_failure_after_finalize, EncoderBackendSession,
        MfRuntime, Mp4Writer, PreparedVideoSample, AUDIO_RATE,
    },
    recording_encoder_caps::VideoCodec,
    recording_encoder_policy::{
        codec::{
            CodecAttempt, CodecAttemptFailure, CodecDecision, CodecPlan, CodecRequest,
            CodecSequence,
        },
        EncoderPolicy, EncoderRequirement, FailureStage, RetryContext,
    },
    recording_options::{
        advanced::{self, ResolvedQualityRequest},
        quality_plan, video_bitrate, Preset, QualityProfile, RecordingOptions,
    },
    recording_size_layout::RecordingSizeLayout,
    Result,
};
use audio_source::TimedTrack;
use camera::{
    session::{CameraRecordingRequest, CameraRecordingSession},
    CameraStatus,
};
use clock::{ActiveRange, ClockPoint, RecordingClock};
use control_receipt::{ControlBatch, ControlBegin, ControlKind, ControlQueue};
use encoding_report::SessionEncodingReport;
use output::RecordingFiles;
use output_plan::OutputPlan;
use scale_plan::{ResizeRequest, ScalePlan};
use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use stop::{FrameCommit, RecordingTail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Phase {
    Starting = 0,
    Recording = 1,
    Paused = 2,
    Finishing = 3,
    Converting = 4,
    Done = 5,
    Failed = 6,
}
impl Phase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Phase::Recording,
            2 => Phase::Paused,
            3 => Phase::Finishing,
            4 => Phase::Converting,
            5 => Phase::Done,
            6 => Phase::Failed,
            _ => Phase::Starting,
        }
    }
    pub(crate) fn active(self) -> bool {
        matches!(self, Phase::Starting | Phase::Recording | Phase::Paused)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecordingResult {
    /// 用户可见的成品（MP4 或 GIF）。
    pub path: PathBuf,
    /// 框内预览使用的 MP4；GIF 预设为中间文件，随会话结束删除。
    pub preview: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration: Duration,
    pub preset: Preset,
    pub bytes: u64,
    /// 分轨 WAV（系统声／麦克风），供预览混音与保存时重新混音；GIF 无音轨。
    pub tracks: Vec<(AudioSource, PathBuf)>,
    /// 成片已有的混音；分轨仍保存未施加该增益的 PCM。
    pub initial_mix: MixLevels,
    /// 本次最终 writer 的收尾后快照；不代表成片已重新读取或播放。
    pub encoding: Option<SessionEncodingReport>,
}

/// 当前窗口的内部实验选择，不写入正式录制配置。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EncodingOptions {
    pub policy: EncoderPolicy,
    pub quality: Option<QualityProfile>,
}

#[derive(Clone, Debug)]
struct SessionRequests {
    encoding: EncodingOptions,
    quality_request: Option<ResolvedQualityRequest>,
    codec_request: Option<CodecRequest>,
    codec_plan: Option<CodecPlan>,
    output_plan: OutputPlan,
    camera_request: Option<CameraRecordingRequest>,
    scale_plan: Option<ScalePlan>,
}

#[derive(Default)]
struct EncoderSelection {
    attempt: Option<CodecAttempt>,
    failures: Option<Vec<CodecAttemptFailure>>,
}

fn validate_session_requests(
    region: ScreenRect,
    options: &RecordingOptions,
    output: &Path,
    encoding: EncodingOptions,
    quality_request: Option<ResolvedQualityRequest>,
    codec_request: Option<CodecRequest>,
    experimental: bool,
) -> Result<SessionRequests> {
    validate_session_requests_with_resize(
        region,
        options,
        output,
        encoding,
        quality_request,
        codec_request,
        None,
        experimental,
    )
}

fn validate_session_requests_with_resize(
    region: ScreenRect,
    options: &RecordingOptions,
    output: &Path,
    encoding: EncodingOptions,
    quality_request: Option<ResolvedQualityRequest>,
    codec_request: Option<CodecRequest>,
    resize_request: Option<ResizeRequest>,
    experimental: bool,
) -> Result<SessionRequests> {
    if matches!(
        codec_request,
        Some(CodecRequest::Av1) | Some(CodecRequest::Av1WithH264Fallback)
    ) {
        return Err(tr!(
            "AV1 目前仅用于独立技术试验，暂不支持生产录制",
            "AV1 is currently limited to independent technical trials, not production recording"
        )
        .into());
    }
    let scale_plan = scale_plan::resolve(region, options.preset, resize_request, experimental)?;
    let (quality_width, quality_height) = scale_plan
        .map_or((region.width, region.height), |plan| {
            (plan.geometry.target_width, plan.geometry.target_height)
        });
    let quality_request = quality_request
        .map(|request| {
            if scale_plan.is_some() {
                validate_session_quality_request_at_size(
                    quality_width,
                    quality_height,
                    options,
                    request,
                )
            } else {
                validate_session_quality_request(region, options, request)
            }
        })
        .transpose()?;
    if let Some(request) = quality_request {
        if encoding.quality != Some(request.base_profile) {
            return Err(tr!(
                "画质请求与所选预设不一致",
                "The quality request does not match the selected profile"
            )
            .into());
        }
    }
    if codec_request.is_some_and(|request| request != CodecRequest::H264) && !experimental {
        return Err(tr!(
            "HEVC 录制仍在实验验证，请先启用媒体实验入口",
            "HEVC recording is experimental; enable the media experiment first"
        )
        .into());
    }
    let legacy = codec_request.is_none()
        && encoding.policy == EncoderPolicy::Auto
        && encoding.quality.is_none()
        && quality_request.is_none()
        && scale_plan.is_none();
    let codec_plan = if legacy {
        None
    } else {
        let quality = match quality_request {
            Some(request) => request.plan,
            None => quality_plan(
                quality_width,
                quality_height,
                options.fps.max(1),
                encoding
                    .quality
                    .unwrap_or(QualityProfile::Current(options.preset)),
            )?,
        };
        Some(CodecPlan::new(
            codec_request.unwrap_or_default(),
            encoding.policy,
            quality,
        )?)
    };
    let output_plan = OutputPlan::new(
        options.preset,
        codec_plan
            .as_ref()
            .map_or(&[VideoCodec::H264][..], |plan| plan.codec_order()),
        options.microphone || options.system_audio,
        output,
    )?;
    Ok(SessionRequests {
        encoding,
        quality_request,
        codec_request,
        codec_plan,
        output_plan,
        camera_request: None,
        scale_plan,
    })
}

fn validate_session_quality_request(
    region: ScreenRect,
    options: &RecordingOptions,
    request: ResolvedQualityRequest,
) -> Result<ResolvedQualityRequest> {
    validate_session_quality_request_at_size(region.width, region.height, options, request)
}

fn validate_session_quality_request_at_size(
    width: u32,
    height: u32,
    options: &RecordingOptions,
    request: ResolvedQualityRequest,
) -> Result<ResolvedQualityRequest> {
    if options.preset.is_gif()
        || matches!(request.base_profile, QualityProfile::Current(preset) if preset.is_gif())
    {
        return Err(tr!(
            "高级码率请求仅用于 MP4 录制",
            "Advanced bitrate requests apply to MP4 recording only"
        )
        .into());
    }
    let checked = advanced::resolve(
        width,
        height,
        options.fps.max(1),
        request.base_profile,
        request.overrides,
    )?;
    if checked != request {
        return Err(tr!(
            "画质请求与当前选区或参数不一致，请重新准备",
            "The quality request no longer matches the region or settings; prepare it again"
        )
        .into());
    }
    Ok(checked)
}

#[derive(Debug)]
struct IncompleteRecording {
    error: Box<dyn Error>,
    directory: PathBuf,
}
impl fmt::Display for IncompleteRecording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let directory = self.directory.display();
        write!(
            f,
            "{}{}",
            self.error,
            trf!(
                "；未完成录制暂存目录：{directory}（现有文件保留，未验证可播放）",
                "; Incomplete recording staging folder: {directory} (existing files kept; playability unverified)"
            )
        )
    }
}
impl Error for IncompleteRecording {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error.as_ref())
    }
}

fn preserve_incomplete_recording<T>(
    result: Result<T>,
    discard: bool,
    captured_frames: u64,
    retain: impl FnOnce() -> Option<PathBuf>,
) -> Result<T> {
    match result {
        Err(error) if !discard && captured_frames > 0 => match retain() {
            Some(directory) => Err(Box::new(IncompleteRecording { error, directory })),
            None => Err(error),
        },
        other => other,
    }
}

pub(crate) struct Recording {
    pub annotations: Arc<live_annotation::FrameState>,
    // 仅用于采集取消和即时提示；活动时间只认控制锁中的有序收据。
    stop: AtomicBool,
    pause: AtomicBool,
    controls: Mutex<ControlQueue>,
    /// 开始信号：倒计时期间先初始化设备与编码器，归零时才采集首帧并起算时间轴。
    pub go: AtomicBool,
    /// 取消：终态提交门前请求丢弃，停止后删除输出，不进入预览。
    pub discard: stop::DiscardFlag,
    pub frames: AtomicU64,
    active_ms: AtomicU64,
    progress: AtomicU32,
    phase: AtomicU8,
    warnings: Mutex<Vec<String>>,
    encoding_report: Mutex<Option<SessionEncodingReport>>,
    camera_report: Mutex<Option<CameraStatus>>,
    pub outcome: Mutex<Option<std::result::Result<RecordingResult, String>>>,
}
impl Recording {
    pub(crate) fn start(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
    ) -> Result<Arc<Self>> {
        Self::spawn(region, options, output, true, EncodingOptions::default())
    }

    /// 只做初始化，`go` 置位后才开始录制；`stop` 可在开始前直接取消。
    pub(crate) fn prepare(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
    ) -> Result<Arc<Self>> {
        Self::spawn(region, options, output, false, EncodingOptions::default())
    }

    /// 内部策略入口；默认 start/prepare 的选择与配置保持原行为。
    pub(crate) fn start_with_encoder_policy(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
    ) -> Result<Arc<Self>> {
        Self::start_with_encoding_options(
            region,
            options,
            output,
            EncodingOptions {
                policy,
                quality: None,
            },
        )
    }

    pub(crate) fn prepare_with_encoder_policy(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
    ) -> Result<Arc<Self>> {
        Self::prepare_with_encoding_options(
            region,
            options,
            output,
            EncodingOptions {
                policy,
                quality: None,
            },
        )
    }

    pub(crate) fn start_with_encoding_options(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        encoding: EncodingOptions,
    ) -> Result<Arc<Self>> {
        Self::spawn(region, options, output, true, encoding)
    }

    pub(crate) fn prepare_with_encoding_options(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        encoding: EncodingOptions,
    ) -> Result<Arc<Self>> {
        Self::spawn(region, options, output, false, encoding)
    }

    /// 内部会话请求入口；不写录制配置，启动前按真实选区重新校验整份请求。
    pub(crate) fn start_with_quality_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
        request: ResolvedQualityRequest,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_quality_request(
            region,
            options,
            output,
            true,
            EncodingOptions {
                policy,
                quality: Some(request.base_profile),
            },
            Some(request),
        )
    }

    pub(crate) fn prepare_with_quality_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
        request: ResolvedQualityRequest,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_quality_request(
            region,
            options,
            output,
            false,
            EncodingOptions {
                policy,
                quality: Some(request.base_profile),
            },
            Some(request),
        )
    }

    /// 显式编码格式实验；不写配置，HEVC 只允许在媒体实验入口启用后请求。
    pub(crate) fn start_with_codec_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
        codec: CodecRequest,
        quality_request: Option<ResolvedQualityRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_codec_request(
            region,
            options,
            output,
            true,
            EncodingOptions {
                policy,
                quality: quality_request.map(|request| request.base_profile),
            },
            quality_request,
            Some(codec),
        )
    }

    pub(crate) fn prepare_with_codec_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
        codec: CodecRequest,
        quality_request: Option<ResolvedQualityRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_codec_request(
            region,
            options,
            output,
            false,
            EncodingOptions {
                policy,
                quality: quality_request.map(|request| request.base_profile),
            },
            quality_request,
            Some(codec),
        )
    }

    /// 仅内部实验请求；输出尺寸不改变采集区域、标注或持久设置。
    pub(crate) fn start_with_resize_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
        resize: ResizeRequest,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_capture_and_resize_requests(
            region,
            options,
            output,
            true,
            EncodingOptions {
                policy,
                quality: quality_request.map(|request| request.base_profile),
            },
            quality_request,
            codec_request,
            None,
            Some(resize),
        )
    }

    pub(crate) fn prepare_with_resize_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        policy: EncoderPolicy,
        resize: ResizeRequest,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_capture_and_resize_requests(
            region,
            options,
            output,
            false,
            EncodingOptions {
                policy,
                quality: quality_request.map(|request| request.base_profile),
            },
            quality_request,
            codec_request,
            None,
            Some(resize),
        )
    }

    fn spawn(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        go: bool,
        encoding: EncodingOptions,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_quality_request(region, options, output, go, encoding, None)
    }

    fn spawn_with_quality_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        go: bool,
        encoding: EncodingOptions,
        quality_request: Option<ResolvedQualityRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_codec_request(region, options, output, go, encoding, quality_request, None)
    }

    fn spawn_with_codec_request(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        go: bool,
        encoding: EncodingOptions,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_capture_requests(
            region,
            options,
            output,
            go,
            encoding,
            quality_request,
            codec_request,
            None,
        )
    }

    fn spawn_with_capture_requests(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        go: bool,
        encoding: EncodingOptions,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
        camera_request: Option<CameraRecordingRequest>,
    ) -> Result<Arc<Self>> {
        Self::spawn_with_capture_and_resize_requests(
            region,
            options,
            output,
            go,
            encoding,
            quality_request,
            codec_request,
            camera_request,
            None,
        )
    }

    fn spawn_with_capture_and_resize_requests(
        region: ScreenRect,
        options: RecordingOptions,
        output: PathBuf,
        go: bool,
        encoding: EncodingOptions,
        quality_request: Option<ResolvedQualityRequest>,
        codec_request: Option<CodecRequest>,
        camera_request: Option<CameraRecordingRequest>,
        resize_request: Option<ResizeRequest>,
    ) -> Result<Arc<Self>> {
        let experimental = crate::file_launch_command::experimental_media_enabled();
        let mut requests = if resize_request.is_some() {
            validate_session_requests_with_resize(
                region,
                &options,
                &output,
                encoding,
                quality_request,
                codec_request,
                resize_request,
                experimental,
            )?
        } else {
            validate_session_requests(
                region,
                &options,
                &output,
                encoding,
                quality_request,
                codec_request,
                experimental,
            )?
        };
        if let Some(request) = &camera_request {
            request.validate(region, experimental)?;
        }
        requests.camera_request = camera_request;
        let state = Arc::new(Self {
            annotations: Arc::new(live_annotation::FrameState::default()),
            stop: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            controls: Mutex::new(ControlQueue::new()),
            go: AtomicBool::new(go),
            discard: stop::DiscardFlag::new(false),
            frames: AtomicU64::new(0),
            active_ms: AtomicU64::new(0),
            progress: AtomicU32::new(0),
            phase: AtomicU8::new(Phase::Starting as u8),
            warnings: Mutex::new(Vec::new()),
            encoding_report: Mutex::new(None),
            camera_report: Mutex::new(None),
            outcome: Mutex::new(None),
        });
        let shared = state.clone();
        std::thread::Builder::new()
            .name("rustshot-recording".into())
            .spawn(move || {
                let result = (|| -> Result<RecordingResult> {
                    let mut files = match RecordingFiles::new(&output) {
                        Ok(files) => files,
                        Err(error) => {
                            shared.discard.seal();
                            return Err(error);
                        }
                    };
                    let result = run(region, &options, &output, &shared, &mut files, requests);
                    // 请求和门共享同一原子状态；门后已完成的会话不再接受丢弃。
                    let discard = shared.discard.seal();
                    let result = match result {
                        Ok(_) if discard => Err(tr!("录制已取消", "Recording canceled").into()),
                        other => other,
                    };
                    let result = preserve_incomplete_recording(
                        result,
                        discard,
                        shared.frames.load(Ordering::Relaxed),
                        || files.retain_incomplete(),
                    );
                    let cleanup = files.cleanup(result.is_err());
                    // run 成功表示全部发布已完成，副清理错误不能再撤销唯一成片。
                    stop::after_publication_cleanup(result, cleanup, |warning| shared.warn(warning))
                })()
                .map_err(|error| error.to_string());
                shared.set_phase(if result.is_ok() {
                    Phase::Done
                } else {
                    Phase::Failed
                });
                *shared.outcome.lock().unwrap() = Some(result);
            })?;
        Ok(state)
    }
    /// 同一锁内取请求时刻并发布；队列错误锁存，由录制线程按真实失败收尾。
    fn request_control(&self, kind: impl FnOnce(bool) -> ControlKind) {
        let mut controls = self.controls.lock().unwrap();
        let kind = kind(controls.paused());
        let _ = controls.request(kind, ClockPoint::now());
        self.sync_control_hints(&controls);
    }
    fn sync_control_hints(&self, controls: &ControlQueue) {
        self.pause.store(controls.paused(), Ordering::Release);
        self.stop.store(controls.stopped(), Ordering::Release);
    }
    pub(crate) fn request_stop(&self) {
        self.request_control(|_| ControlKind::Stop);
    }
    pub(crate) fn set_paused(&self, paused: bool) {
        self.request_control(|_| ControlKind::Pause(paused));
    }
    pub(crate) fn toggle_pause(&self) {
        self.request_control(|paused| ControlKind::Pause(!paused));
    }
    pub(crate) fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    fn control_begin(&self) -> ControlBegin {
        let mut controls = self.controls.lock().unwrap();
        let begin = controls.begin(ClockPoint::now());
        self.sync_control_hints(&controls);
        begin
    }
    fn control_take(&self) -> ControlBatch {
        let mut controls = self.controls.lock().unwrap();
        let batch = controls.take(ClockPoint::now());
        self.sync_control_hints(&controls);
        batch
    }
    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
    }
    pub(crate) fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }
    pub(crate) fn elapsed(&self) -> Duration {
        Duration::from_millis(self.active_ms.load(Ordering::Relaxed))
    }
    /// GIF 转码进度 0..=1。
    pub(crate) fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }
    pub(crate) fn warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap().clone()
    }
    /// 失败会话也可读取已显式收尾的 writer；未创建或未完成收尾时为 None。
    pub(crate) fn encoding_report(&self) -> Option<SessionEncodingReport> {
        self.encoding_report.lock().unwrap().clone()
    }
    /// 仅为已请求摄像头会话的停机回执；不证明成片含摄像头画面或物理设备已释放。
    pub(crate) fn camera_report(&self) -> Option<CameraStatus> {
        self.camera_report.lock().unwrap().clone()
    }
    fn warn(&self, message: String) {
        let mut warnings = self.warnings.lock().unwrap();
        if !warnings.contains(&message) {
            warnings.push(message);
        }
    }
}

/// 会话结束后不再需要分轨；成片 MP4 已包含最终混音。
pub(crate) fn remove_tracks(result: &RecordingResult) {
    for (_, path) in &result.tracks {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn track_path(output: &Path, source: AudioSource) -> PathBuf {
    let suffix = match source {
        AudioSource::System => "system.wav",
        AudioSource::Microphone => "mic.wav",
    };
    PathBuf::from(format!("{}.{suffix}", output.display()))
}

/// 录制线程的音频出口：分轨写 WAV，同时按默认音量混入 MP4。
struct AudioSink {
    capture: AudioCapture,
    sources: Vec<AudioSource>,
    tracks: Vec<crate::wav_track::WavWriter>,
    paths: Vec<PathBuf>,
    written: u64,
    /// 编码器尚在后台创建时的混音，按原时间戳暂存，就绪后先于新数据写入。
    pending: Vec<(Vec<i16>, Duration)>,
    pending_samples: usize,
    timing: Vec<AudioTimingSummary>,
    clock: Option<RecordingClock>,
    control_fence: Option<AudioCommitFence>,
    control_tracks: Vec<audio_control::ControlTrack>,
    control_generations: Vec<Option<u64>>,
    mix: MixLevels,
    /// 设备时间戳放置尚在实验验证；默认录制继续使用原 FIFO。
    timed: Option<Vec<TimedTrack>>,
}

const MAX_PENDING_AUDIO_BYTES: usize = 8 * 1024 * 1024;
const AUDIO_PLACEMENT_DELAY: Duration = Duration::from_millis(150);
const AUDIO_TAIL_WAIT: Duration = Duration::from_millis(200);
const CONTROL_AUDIO_READ_FRAMES: usize = audio_control::MAX_CONTROL_FRAMES;

#[derive(Clone, Copy)]
struct AudioCommitFence {
    point: ClockPoint,
    active: Duration,
}

#[derive(Debug, Default)]
struct AudioTimingSummary {
    first_raw: Option<RawAudioTime>,
    last_raw: Option<RawAudioTime>,
    last_packet: Option<(u64, u64)>,
    reported_discontinuities: u64,
    missing_qpc_packets: u64,
    real_frames: u64,
    padded_frames: u64,
    mapped_packet_anchors: u64,
    unmapped_timed_anchors: u64,
    first_active: Option<ActiveRange>,
    last_active: Option<ActiveRange>,
}
impl AudioTimingSummary {
    fn observe(&mut self, read: &CaptureRead, requested: usize, clock: Option<&RecordingClock>) {
        self.real_frames = self.real_frames.saturating_add(read.real_frames as u64);
        self.padded_frames = self
            .padded_frames
            .saturating_add(requested.saturating_sub(read.real_frames) as u64);
        for segment in &read.segments {
            if self.observe_packet(segment.raw_time, segment.packet_output_start) {
                if let Some(qpc) = segment.raw_time.qpc_100ns {
                    if let Some(active) = clock.and_then(|clock| clock.map(qpc)) {
                        self.mapped_packet_anchors = self.mapped_packet_anchors.saturating_add(1);
                        self.first_active.get_or_insert(active);
                        self.last_active = Some(active);
                    } else {
                        self.unmapped_timed_anchors = self.unmapped_timed_anchors.saturating_add(1);
                    }
                }
            }
        }
    }
    fn observe_packet(&mut self, raw: RawAudioTime, output_start: u64) -> bool {
        let key = (raw.endpoint_epoch, output_start);
        if self.last_packet == Some(key) {
            return false;
        }
        self.first_raw.get_or_insert(raw);
        self.last_raw = Some(raw);
        self.last_packet = Some(key);
        self.reported_discontinuities = self
            .reported_discontinuities
            .saturating_add(u64::from(raw.discontinuity));
        self.missing_qpc_packets = self
            .missing_qpc_packets
            .saturating_add(u64::from(raw.qpc_100ns.is_none()));
        true
    }
}

fn audio_target_frames(time: Duration) -> u64 {
    (time.as_nanos() * u128::from(AUDIO_RATE) / 1_000_000_000).min(u128::from(u64::MAX)) as u64
}

fn audio_sample_time(frames: u64) -> Duration {
    let rate = u64::from(AUDIO_RATE);
    Duration::from_secs(frames / rate) + Duration::from_nanos(frames % rate * 1_000_000_000 / rate)
}
fn initial_mix_for_session(options: &RecordingOptions, experimental: bool) -> MixLevels {
    if experimental {
        options.initial_mix.normalized()
    } else {
        MixLevels::default()
    }
}
impl AudioSink {
    fn new(
        mut capture: AudioCapture,
        files: &mut RecordingFiles,
        mix: MixLevels,
        state: &Recording,
    ) -> Result<Self> {
        let sources = capture.sources();
        let paths: Vec<PathBuf> = sources
            .iter()
            .map(|source| {
                files
                    .track_path(*source == AudioSource::Microphone)
                    .to_path_buf()
            })
            .collect();
        let tracks = (|| -> Result<Vec<crate::wav_track::WavWriter>> {
            let tracks = paths
                .iter()
                .map(|p| crate::wav_track::WavWriter::create(p))
                .collect::<Result<Vec<_>>>()?;
            for source in &sources {
                files.register_track(*source == AudioSource::Microphone)?;
            }
            Ok(tracks)
        })();
        let tracks = match tracks {
            Ok(tracks) => tracks,
            Err(error) => {
                capture.stop_and_wait();
                for warning in capture.errors() {
                    state.warn(warning);
                }
                return Err(error);
            }
        };
        let timing = sources
            .iter()
            .map(|_| AudioTimingSummary::default())
            .collect();
        let timed = crate::file_launch_command::experimental_media_enabled()
            .then(|| sources.iter().map(|_| TimedTrack::new()).collect());
        let control_tracks = sources
            .iter()
            .map(|_| audio_control::ControlTrack::new())
            .collect();
        let control_generations = sources.iter().map(|_| None).collect();
        Ok(Self {
            capture,
            sources,
            tracks,
            paths,
            written: 0,
            pending: Vec::new(),
            pending_samples: 0,
            timing,
            clock: None,
            control_fence: None,
            control_tracks,
            control_generations,
            mix,
            timed,
        })
    }
    /// 控制时钟和提交上界来自同一份收据快照，不能用后续观察时刻扩展。
    fn set_control_fence(
        &mut self,
        clock: &RecordingClock,
        point: ClockPoint,
        active: Duration,
    ) -> Result<()> {
        if clock.active_at(point.instant) != Some(active) {
            return Err(tr!(
                "录屏音频边界与活动时钟不一致",
                "Recording audio boundary differs from the active clock"
            )
            .into());
        }
        if self.control_fence.is_some_and(|previous| {
            point.instant < previous.point.instant || active < previous.active
        }) || self.written > audio_target_frames(active)
        {
            return Err(tr!(
                "录屏音频提交边界早于已写前缀",
                "Recording audio boundary precedes the committed prefix"
            )
            .into());
        }
        self.clock = Some(clock.clone());
        self.control_fence = Some(AudioCommitFence { point, active });
        Ok(())
    }
    fn commit_target(&self, until: Duration) -> Result<u64> {
        let fence = self.control_fence.ok_or(tr!(
            "录屏音频缺少控制边界",
            "Recording audio has no control boundary"
        ))?;
        let target = audio_target_frames(until);
        if until > fence.active || self.written > target {
            return Err(tr!(
                "录屏音频超出控制边界或试图回退已写前缀",
                "Recording audio exceeds its control boundary or committed prefix"
            )
            .into());
        }
        Ok(target)
    }
    /// 先按请求边界筛PCM，再按各活动区段的FIFO或既有实验放置输出。
    fn write(&mut self, mut writer: Option<&mut Mp4Writer>, until: Duration) -> Result<()> {
        let target = self.commit_target(until)?;
        self.drain_control()?;
        if self.timed.is_some() {
            return self.write_timed(writer, until.saturating_sub(AUDIO_PLACEMENT_DELAY));
        }
        let spans = self
            .clock
            .as_ref()
            .ok_or("Missing recording audio clock")?
            .active_spans();
        while self.written < target {
            let frames = (target - self.written).min(CONTROL_AUDIO_READ_FRAMES as u64) as usize;
            let mut rendered = Vec::with_capacity(self.control_tracks.len());
            for (index, track) in self.control_tracks.iter_mut().enumerate() {
                let read = track.render_fifo(&spans, self.written, frames)?;
                self.timing[index].padded_frames = self.timing[index]
                    .padded_frames
                    .saturating_add(frames.saturating_sub(read.real_frames) as u64);
                self.tracks[index].write(&read.samples)?;
                rendered.push(read.samples);
            }
            let labelled: Vec<_> = self
                .sources
                .iter()
                .zip(&rendered)
                .map(|(source, samples)| (*source, samples.as_slice()))
                .collect();
            let pcm = mix_tracks(&labelled, self.mix);
            let time = audio_sample_time(self.written);
            self.written += frames as u64;
            self.submit_mix(writer.as_deref_mut(), pcm, time)?;
        }
        Ok(())
    }
    /// fence之后的原始PCM留在有界队列，下次消费收据后重新分派。
    fn drain_control(&mut self) -> Result<()> {
        let fence = self
            .control_fence
            .ok_or("Missing recording audio control boundary")?;
        let clock = self.clock.as_ref().ok_or("Missing recording audio clock")?;
        let reads = self
            .capture
            .take_available_with_timing(CONTROL_AUDIO_READ_FRAMES)?;
        let clocks = self.capture.clock_diagnostics();
        for (index, source) in self.sources.iter().enumerate() {
            let snapshot = clocks
                .iter()
                .find(|(expected, _)| expected == source)
                .map(|(_, snapshot)| snapshot)
                .ok_or(tr!(
                    "音频来源缺少采集时钟",
                    "Audio source has no capture clock"
                ))?;
            if self.control_generations[index]
                .is_some_and(|previous| previous != snapshot.generation)
            {
                self.control_tracks[index].clear();
            }
            self.control_generations[index] = Some(snapshot.generation);
            if let Some(timed) = &mut self.timed {
                timed[index].sync_clock_generation(snapshot.generation);
            }
        }
        for (source, read) in reads {
            let index = self
                .sources
                .iter()
                .position(|expected| *expected == source)
                .ok_or(tr!(
                    "音频来源与录制分轨不一致",
                    "Audio source does not match a recording track"
                ))?;
            let snapshot = clocks
                .iter()
                .find(|(expected, _)| *expected == source)
                .map(|(_, snapshot)| snapshot)
                .ok_or(tr!(
                    "音频来源缺少采集时钟",
                    "Audio source has no capture clock"
                ))?;
            self.control_tracks[index].queue(read)?;
            let selected =
                self.control_tracks[index].select(clock, fence.point, fence.active, |raw| {
                    snapshot.accepts_raw_timestamp(raw)
                })?;
            for selected in selected {
                let read = &selected.read;
                self.timing[index].observe(read, read.real_frames, Some(clock));
                if let Some(timed) = &mut self.timed {
                    timed[index].ingest(
                        read,
                        fence.active,
                        self.written,
                        |raw| snapshot.accepts_raw_timestamp(raw),
                        |earliest, latest| clock.map_window(earliest, latest),
                    )?;
                } else {
                    self.control_tracks[index].push_ready(selected)?;
                }
            }
        }
        Ok(())
    }
    fn write_timed(&mut self, mut writer: Option<&mut Mp4Writer>, until: Duration) -> Result<()> {
        // 常规150ms等待在暂停/短脉冲后可能落在已提交前缀内，此时只需等待。
        let fence = self
            .control_fence
            .ok_or("Missing recording audio control boundary")?;
        self.commit_target(fence.active)?;
        if until > fence.active {
            return Err("Timed audio exceeds the recording control boundary".into());
        }
        let target = audio_target_frames(until);
        while self.written < target {
            let frames = (target - self.written).min(CONTROL_AUDIO_READ_FRAMES as u64) as usize;
            let timed = self.timed.as_mut().ok_or(tr!(
                "未启用音频时间戳放置",
                "Timestamped audio placement is disabled"
            ))?;
            let mut rendered = Vec::with_capacity(timed.len());
            for (index, track) in timed.iter_mut().enumerate() {
                let samples = track.render(self.written, frames)?;
                self.tracks[index].write(&samples)?;
                rendered.push(samples);
            }
            let labelled: Vec<_> = self
                .sources
                .iter()
                .zip(&rendered)
                .map(|(source, samples)| (*source, samples.as_slice()))
                .collect();
            let pcm = mix_tracks(&labelled, self.mix);
            let time = audio_sample_time(self.written);
            self.written += frames as u64;
            self.submit_mix(writer.as_deref_mut(), pcm, time)?;
        }
        Ok(())
    }
    /// 暂停和停止仅共用一个尾包等待预算；到期后仍缺失的部分由放置模型补静音。
    fn write_tail(&mut self, writer: Option<&mut Mp4Writer>, until: Duration) -> Result<()> {
        let target = self.commit_target(until)?;
        if self.timed.is_none() {
            return self.write(writer, until);
        }
        if self.written >= target {
            return Ok(());
        }
        let deadline = Instant::now() + AUDIO_TAIL_WAIT;
        loop {
            self.drain_control()?;
            let complete = self
                .timed
                .as_ref()
                .is_some_and(|tracks| tracks.iter().all(|track| track.has_end_anchor(target - 1)));
            let now = Instant::now();
            if complete || now >= deadline {
                break;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(5)));
        }
        self.write_timed(writer, until)
    }
    fn report_warnings(&self, state: &Recording) {
        for error in self.capture.errors() {
            state.warn(error);
        }
        for (source, track) in self.sources.iter().zip(&self.control_tracks) {
            if track.summary().rejected_frames >= u64::from(AUDIO_RATE) / 4 {
                state.warn(match source {
                    AudioSource::System => tr!(
                        "部分系统音频无法按暂停／停止边界定位，缺失区间补静音",
                        "Some system audio could not be placed at pause/stop boundaries; gaps use silence"
                    ),
                    AudioSource::Microphone => tr!(
                        "部分麦克风音频无法按暂停／停止边界定位，缺失区间补静音",
                        "Some microphone audio could not be placed at pause/stop boundaries; gaps use silence"
                    ),
                }.into());
            }
        }
        if let Some(tracks) = &self.timed {
            for (source, track) in self.sources.iter().zip(tracks) {
                let input = track.summary().input;
                let rejected = input
                    .missing_qpc
                    .saturating_add(input.missing_phase)
                    .saturating_add(input.untrusted_raw)
                    .saturating_add(input.phase_errors)
                    .saturating_add(input.invalid_window)
                    .saturating_add(input.future)
                    .saturating_add(input.model_rejected);
                if rejected >= u64::from(AUDIO_RATE) / 4 {
                    state.warn(
                        match source {
                            AudioSource::System => tr!(
                                "部分系统音频时间戳不可用，缺失区间补静音",
                                "Some system audio timestamps are unavailable; gaps use silence"
                            ),
                            AudioSource::Microphone => tr!(
                                "部分麦克风时间戳不可用，缺失区间补静音",
                                "Some microphone timestamps are unavailable; gaps use silence"
                            ),
                        }
                        .into(),
                    );
                }
            }
        }
    }
    fn submit_mix(
        &mut self,
        writer: Option<&mut Mp4Writer>,
        pcm: Vec<i16>,
        time: Duration,
    ) -> Result<()> {
        if let Some(writer) = writer {
            self.flush(writer)?;
            return writer.write_audio(&pcm, time);
        }
        if self
            .pending_samples
            .saturating_add(pcm.len())
            .saturating_mul(std::mem::size_of::<i16>())
            > MAX_PENDING_AUDIO_BYTES
        {
            return Err(tr!(
                "编码器准备过慢，待写音频达到缓存上限",
                "Encoder initialization is too slow; pending audio reached its limit"
            )
            .into());
        }
        self.pending_samples += pcm.len();
        self.pending.push((pcm, time));
        Ok(())
    }
    fn pending_full(&self) -> bool {
        self.pending_samples
            .saturating_mul(std::mem::size_of::<i16>())
            >= MAX_PENDING_AUDIO_BYTES - MAX_TAKE_FRAMES * 2 * std::mem::size_of::<i16>()
    }
    fn flush(&mut self, writer: &mut Mp4Writer) -> Result<()> {
        if !self.pending.is_empty() {
            self.commit_target(
                self.control_fence
                    .ok_or("Missing audio control boundary")?
                    .active,
            )?;
        }
        self.pending_samples = 0;
        for (pcm, time) in std::mem::take(&mut self.pending) {
            writer.write_audio(&pcm, time)?;
        }
        Ok(())
    }
    fn finish(mut self, state: &Recording) -> Result<Vec<(AudioSource, PathBuf)>> {
        // 尾音已由 write_tail 排出；停止后再读一次，保留线程收尾产生的警告。
        self.capture.stop_and_wait();
        self.report_warnings(state);
        if std::env::var_os("RUSTSHOT_TRACE").is_some() {
            for (source, diagnostic) in self.capture.clock_diagnostics() {
                eprintln!("REC AUDIO DEVICE CLOCK {source:?} {diagnostic:?}");
            }
            if let Some(clock) = &self.clock {
                eprintln!("REC AUDIO CLOCK {:?}", clock.summary());
            }
            for (source, control) in self.sources.iter().zip(&self.control_tracks) {
                eprintln!("REC AUDIO CONTROL {source:?} {:?}", control.summary());
            }
            for (source, timing) in self.sources.iter().zip(&self.timing) {
                // 包锚只有活动区段范围；它不是重采样帧的精确位置或漂移修正。
                eprintln!("REC AUDIO TIMING {source:?} {timing:?}");
            }
            if let Some(tracks) = &self.timed {
                for (source, track) in self.sources.iter().zip(tracks) {
                    eprintln!("REC AUDIO PLACEMENT {source:?} {:?}", track.summary());
                }
            }
        }
        let mut failures = stop::Failures::default();
        for wav in self.tracks {
            failures.record("音轨收尾", wav.finish());
        }
        failures.result(self.sources.into_iter().zip(self.paths).collect())
    }
}

struct SessionTail {
    writer: Mp4Writer,
    audio: Option<AudioSink>,
    duration: Duration,
    state: Arc<Recording>,
    encoding: EncodingOptions,
    requested_profile: QualityProfile,
    quality_request: Option<ResolvedQualityRequest>,
    scale_plan: Option<ScalePlan>,
    codec_request: Option<CodecRequest>,
    selection: EncoderSelection,
    output_plan: OutputPlan,
    writer_audio_requested: bool,
}
impl RecordingTail for SessionTail {
    type Tracks = Vec<(AudioSource, PathBuf)>;
    fn flush_audio(&mut self) -> Result<()> {
        if let Some(audio) = &mut self.audio {
            audio.write_tail(Some(&mut self.writer), self.duration)?;
            audio.report_warnings(&self.state);
        }
        Ok(())
    }
    fn finish_tracks(&mut self) -> Result<Self::Tracks> {
        self.audio
            .take()
            .map(|audio| audio.finish(&self.state))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    fn finish_video(self) -> Result<()> {
        let (finished, diagnostics) = self.writer.finish_with_diagnostics();
        let mut report = SessionEncodingReport::from_finalized_writer(
            self.encoding.policy,
            self.requested_profile,
            self.selection.attempt.map(|attempt| attempt.slot),
            diagnostics,
        )
        .with_requested_quality(self.quality_request)
        .with_requested_scale(self.scale_plan)
        .with_output_plan(Some(self.output_plan))
        .with_writer_audio_requested(self.writer_audio_requested);
        if let Some(failures) = self.selection.failures {
            report = report.with_codec_evidence(
                self.codec_request.unwrap_or_default(),
                self.selection.attempt,
                failures,
            );
        }
        *self.state.encoding_report.lock().unwrap() = Some(report);
        finished
    }
}

/// GIF 预设的中间 MP4 与成品同目录，名称固定可推导，便于异常清理。
pub(crate) fn intermediate_path(output: &Path) -> PathBuf {
    output.with_extension("gif.mp4")
}

/// 录屏历史目录；成品先落在这里，保存时另存到用户选择的位置。
pub(crate) fn library_dir() -> PathBuf {
    crate::preferences::data_dir().join("recordings")
}

pub(crate) fn new_output_path(preset: Preset) -> Result<PathBuf> {
    new_output_path_in(&library_dir(), preset)
}

/// 真实分配链共用入口；相对数据目录也只返回绝对物理文件路径。
pub(crate) fn new_output_path_in(directory: &Path, preset: Preset) -> Result<PathBuf> {
    let directory = std::path::absolute(directory)?;
    std::fs::create_dir_all(&directory)?;
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let stem = trf!(
        "录屏_{:04}{:02}{:02}_{:02}{:02}{:02}",
        "Recording_{:04}{:02}{:02}_{:02}{:02}{:02}",
        t.wYear,
        t.wMonth,
        t.wDay,
        t.wHour,
        t.wMinute,
        t.wSecond
    );
    let extension = if preset.is_gif() { "gif" } else { "mp4" };
    for index in 0..100 {
        let name = if index == 0 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem}_{index}.{extension}")
        };
        let path = directory.join(name);
        if !path.exists() && !intermediate_path(&path).exists() {
            return Ok(path);
        }
    }
    Err(tr!(
        "录屏文件名冲突过多",
        "Too many recording file name conflicts"
    )
    .into())
}

/// MF 对象均为自由线程（MTA），采集设备已开多线程保护；仅用于把后台创建的写入器交回录制线程。
struct SendWriter(Mp4Writer);
unsafe impl Send for SendWriter {}

struct PolicyStartup {
    backend: EncoderBackendSession,
    sequence: CodecSequence,
    active_codec: VideoCodec,
    has_audio: bool,
    scale_plan: Option<ScalePlan>,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    paths: Vec<PathBuf>,
    index: usize,
    writer: Option<SendWriter>,
    created: Vec<(usize, crate::file_snapshot::FileSnapshot)>,
    messages: Vec<String>,
    error: Option<String>,
}
impl PolicyStartup {
    fn remember_created(&mut self, required: bool) -> Result<()> {
        match crate::file_snapshot::inspect(&self.paths[self.index])? {
            Some(snapshot) => self.created.push((self.index, snapshot)),
            None if required => return Err("编码器未创建可核验的自有暂存".into()),
            None => {}
        }
        Ok(())
    }

    fn register_created(&mut self, files: &mut RecordingFiles) -> Result<()> {
        let mut errors = Vec::new();
        for (index, snapshot) in std::mem::take(&mut self.created) {
            if let Err(error) = files.register_encoder_attempt(index, snapshot) {
                errors.push((tr!("核验编码暂存", "encoder staging verification"), error));
            }
        }
        if errors.is_empty() {
            return Ok(());
        }
        let primary = if self.messages.is_empty() {
            Err(errors.remove(0).1)
        } else {
            Err(self.messages.join("；").into())
        };
        stop::after_cleanup(primary, errors)
    }

    fn open_current(&mut self, cancelled: &dyn Fn() -> bool) {
        loop {
            if cancelled() {
                self.error = Some(tr!("录制已取消", "Recording canceled").into());
                return;
            }
            let Some(attempt) = self.sequence.current() else {
                self.error =
                    Some(tr!("编码尝试计划已结束", "The encoder attempt plan has ended").into());
                return;
            };
            self.index = attempt.slot;
            if self.active_codec != attempt.codec {
                self.backend = EncoderBackendSession::new_for_codec(
                    self.sequence.plan().encoder_plan().clone(),
                    self.has_audio,
                    attempt.codec,
                );
                self.active_codec = attempt.codec;
            }
            let Some(path) = self.paths.get(attempt.slot) else {
                self.error = Some(
                    tr!(
                        "编码暂存槽位不在计划内",
                        "The encoder staging slot is outside the plan"
                    )
                    .into(),
                );
                return;
            };
            let created = match self.scale_plan {
                Some(plan) => self.backend.create_scaled_attempt(
                    path,
                    attempt.attempt_index,
                    plan.geometry.source_width,
                    plan.geometry.source_height,
                    Some(&self.device),
                ),
                None => {
                    self.backend
                        .create_attempt(path, attempt.attempt_index, Some(&self.device))
                }
            };
            match created {
                Ok(writer) => {
                    // 身份核验失败也保留已创建的出口，交会话显式 Finalize。
                    self.writer = Some(SendWriter(writer));
                    if let Err(error) = self.remember_created(true) {
                        self.messages.push(error.to_string());
                        self.error = Some(self.messages.join("；"));
                    }
                    return;
                }
                Err(error) => {
                    self.messages.push(error.to_string());
                    if matches!(
                        error.stage,
                        FailureStage::MediaTypes
                            | FailureStage::BeginWriting
                            | FailureStage::EncoderObservation
                    ) {
                        if let Err(error) = self.remember_created(false) {
                            self.messages.push(error.to_string());
                            self.error = Some(self.messages.join("；"));
                            return;
                        }
                    }
                    match self
                        .sequence
                        .failed(error.stage, error.cause, RetryContext::default())
                    {
                        CodecDecision::Retry { attempt, .. } => self.index = attempt.slot,
                        CodecDecision::Stop { .. } => {
                            self.error = Some(self.messages.join("；"));
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// 等待编码器期间暂存的 CPU 帧上限；超出后阻塞等待编码器，避免大选区占用过多内存。
const PENDING_FRAME_BYTES: usize = 256 << 20;
// 极小选区还受数量限制，避免元数据随编码器准备时间增长。
const MAX_PENDING_FRAMES: usize = 240;

enum VideoPixels {
    Cpu(crate::capture::Image),
    Prepared(PreparedVideoSample),
}

/// 只保留一个未提交画面；暂停不换画面，恢复后的下一帧或正常停止确定其终点。
struct HeldVideoFrame {
    pixels: VideoPixels,
    time: Duration,
}
impl HeldVideoFrame {
    fn pending_bytes(&self) -> usize {
        match &self.pixels {
            VideoPixels::Cpu(image) => image.bgra.len(),
            // 只在 Ready 使用，属于原 GPU 固定样本池，不加入 CPU 初始化缓存。
            VideoPixels::Prepared(_) => 0,
        }
    }
}

/// 硬件编码器创建约需数百毫秒；期间先按时间轴采集并暂存，就绪后按原时间戳补写。
enum Encoder {
    Pending {
        thread: Option<std::thread::JoinHandle<std::result::Result<SendWriter, String>>>,
        frames: Vec<(crate::capture::Image, Duration, Duration)>,
        bytes: usize,
    },
    PolicyPending {
        thread: Option<std::thread::JoinHandle<std::result::Result<PolicyStartup, String>>>,
        frames: Vec<(crate::capture::Image, Duration, Duration)>,
        bytes: usize,
    },
    Ready(Mp4Writer),
}

fn start_policy_encoder(
    plan: CodecPlan,
    scale_plan: Option<ScalePlan>,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    paths: Vec<PathBuf>,
    has_audio: bool,
    state: &Arc<Recording>,
) -> Result<Encoder> {
    let shared = state.clone();
    let thread = std::thread::Builder::new()
        .name("rustshot-policy-encoder-start".into())
        .spawn(move || {
            let _runtime = MfRuntime::new().map_err(|error| error.to_string())?;
            let active_codec = plan.attempts()[0].codec;
            let mut startup = PolicyStartup {
                backend: EncoderBackendSession::new_for_codec(
                    plan.encoder_plan().clone(),
                    has_audio,
                    active_codec,
                ),
                sequence: CodecSequence::new(plan),
                active_codec,
                has_audio,
                scale_plan,
                device,
                paths,
                index: 0,
                writer: None,
                created: Vec::new(),
                messages: Vec::new(),
                error: None,
            };
            startup.open_current(&|| {
                shared.discard.load(Ordering::Acquire)
                    || (shared.stop.load(Ordering::Acquire)
                        && !shared.go.load(Ordering::Acquire))
            });
            Ok(startup)
        })?;
    Ok(Encoder::PolicyPending {
        thread: Some(thread),
        frames: Vec::new(),
        bytes: 0,
    })
}

impl Encoder {
    fn fail_startup(&mut self, startup: &mut PolicyStartup, error: Box<dyn Error>) -> Result<()> {
        // 保留已有 writer 只为收尾，不授予未知文件身份或晋升资格。
        if let Some(SendWriter(writer)) = startup.writer.take() {
            *self = Encoder::Ready(writer);
        }
        Err(error)
    }

    fn writer(&mut self) -> Option<&mut Mp4Writer> {
        match self {
            Encoder::Ready(writer) => Some(writer),
            Encoder::Pending { .. } | Encoder::PolicyPending { .. } => None,
        }
    }
    fn due(&self) -> bool {
        match self {
            Encoder::Ready(_) => false,
            Encoder::Pending {
                thread,
                frames,
                bytes,
            } => {
                thread.as_ref().is_none_or(|t| t.is_finished())
                    || *bytes >= PENDING_FRAME_BYTES
                    || frames.len() >= MAX_PENDING_FRAMES
            }
            Encoder::PolicyPending {
                thread,
                frames,
                bytes,
            } => {
                thread.as_ref().is_none_or(|t| t.is_finished())
                    || *bytes >= PENDING_FRAME_BYTES
                    || frames.len() >= MAX_PENDING_FRAMES
            }
        }
    }
    fn has_pending_frames(&self) -> bool {
        match self {
            Encoder::Pending { frames, .. } | Encoder::PolicyPending { frames, .. } => {
                !frames.is_empty()
            }
            Encoder::Ready(_) => false,
        }
    }
    fn needs_pending_capacity(&self, next_bytes: usize, held: Option<&HeldVideoFrame>) -> bool {
        match self {
            Encoder::Pending { frames, bytes, .. }
            | Encoder::PolicyPending { frames, bytes, .. } => {
                let held_count = usize::from(held.is_some());
                let held_bytes = held.map_or(0, HeldVideoFrame::pending_bytes);
                frames
                    .len()
                    .checked_add(held_count)
                    .and_then(|count| count.checked_add(1))
                    .is_none_or(|count| count > MAX_PENDING_FRAMES)
                    || bytes
                        .checked_add(held_bytes)
                        .and_then(|total| total.checked_add(next_bytes))
                        .is_none_or(|total| total > PENDING_FRAME_BYTES)
            }
            Encoder::Ready(_) => false,
        }
    }
    /// 等待编码器并按序写入暂存的画面与声音；创建失败时保持 Pending 并返回错误。
    fn resolve(
        &mut self,
        audio: Option<&mut AudioSink>,
        files: &mut RecordingFiles,
        selected: &mut EncoderSelection,
        state: &Recording,
    ) -> Result<()> {
        if let Encoder::Pending { thread, frames, .. } = self {
            let created = match thread.take() {
                Some(thread) => thread
                    .join()
                    .unwrap_or_else(|_| Err("编码器初始化线程异常退出".into())),
                None => Err("视频编码器不可用".into()),
            };
            let SendWriter(writer) = created?;
            let frames = std::mem::take(frames);
            *self = Encoder::Ready(writer);
            files.register_video()?;
            let Encoder::Ready(writer) = self else {
                unreachable!()
            };
            for (image, time, duration) in frames {
                writer.write_video(&image.bgra, image.width, time, duration)?;
            }
        }
        if let Encoder::PolicyPending { thread, frames, .. } = self {
            let mut startup = thread
                .take()
                .ok_or("策略编码器初始化已结束")?
                .join()
                .unwrap_or_else(|_| Err("策略编码器初始化线程异常退出".into()))?;
            selected.failures = Some(startup.sequence.failures().to_vec());
            if let Err(error) = startup.register_created(files) {
                return self.fail_startup(&mut startup, error);
            }
            if let Some(error) = startup.error.take() {
                return self.fail_startup(&mut startup, error.into());
            }
            let cancelled = || {
                state.discard.load(Ordering::Acquire)
                    || (state.stop.load(Ordering::Acquire) && !state.go.load(Ordering::Acquire))
            };
            loop {
                if cancelled() {
                    return self.fail_startup(
                        &mut startup,
                        tr!("录制已取消", "Recording canceled").into(),
                    );
                }
                let chosen = startup.sequence.current().ok_or("策略编码器已无可用尝试")?;
                let SendWriter(mut writer) =
                    startup.writer.take().ok_or("策略编码器未返回写入器")?;
                let first_submitted = !frames.is_empty();
                let first = match frames.first() {
                    Some((image, time, duration)) => {
                        writer.write_video(&image.bgra, image.width, *time, *duration)
                    }
                    None => Ok(()),
                };
                if let Err(error) = first {
                    let cause = classify_recording_error(error.as_ref());
                    startup.messages.push(error.to_string());
                    let (finished, _) = writer.finish_with_diagnostics();
                    let cause = recording_failure_after_finalize(cause, &finished);
                    if let Err(error) = finished {
                        startup.messages.push(error.to_string());
                    }
                    if let Err(cleanup) = files.discard_encoder_attempt(startup.index) {
                        return stop::after_cleanup(
                            Err(startup.messages.join("；").into()),
                            vec![(tr!("清理编码尝试", "encoder attempt cleanup"), cleanup)],
                        );
                    }
                    let decision = startup.sequence.failed(
                        FailureStage::FirstVideoSample,
                        cause,
                        RetryContext {
                            first_video_sample_retained: first_submitted,
                            ..Default::default()
                        },
                    );
                    selected.failures = Some(startup.sequence.failures().to_vec());
                    match decision {
                        CodecDecision::Retry { attempt, .. } => {
                            startup.index = attempt.slot;
                            startup.open_current(&cancelled);
                            selected.failures = Some(startup.sequence.failures().to_vec());
                            if let Err(error) = startup.register_created(files) {
                                return self.fail_startup(&mut startup, error);
                            }
                            if let Some(error) = startup.error.take() {
                                return self.fail_startup(&mut startup, error.into());
                            }
                            continue;
                        }
                        CodecDecision::Stop { .. } => {
                            return Err(startup.messages.join("；").into())
                        }
                    }
                }
                if startup.sequence.plan().encoder_plan().policy()
                    == EncoderPolicy::HardwarePreferred
                    && chosen.attempt.requirement == EncoderRequirement::Software
                {
                    state.warn(
                        tr!(
                            "硬件编码不可用，本次使用软件编码",
                            "Hardware encoding is unavailable; using software for this recording"
                        )
                        .into(),
                    );
                }
                if startup.sequence.plan().request() == CodecRequest::HevcWithH264Fallback
                    && chosen.codec == VideoCodec::H264
                {
                    state.warn(
                        tr!(
                            "无法使用 HEVC，本次使用 H.264 编码",
                            "HEVC is unavailable; using H.264 for this recording"
                        )
                        .into(),
                    );
                }
                selected.attempt = Some(chosen);
                // 有缓存时首样本已实际接受；此后进入 Ready，任何失败都沿收尾而不重建。
                let buffered = std::mem::take(frames);
                *self = Encoder::Ready(writer);
                let Encoder::Ready(writer) = self else {
                    unreachable!()
                };
                for (image, time, duration) in
                    buffered.into_iter().skip(usize::from(first_submitted))
                {
                    writer.write_video(&image.bgra, image.width, time, duration)?;
                }
                break;
            }
        }
        if let (Encoder::Ready(writer), Some(audio)) = (self, audio) {
            audio.flush(writer)?;
        }
        Ok(())
    }
}

/// 只有跨度已闭合且已写入或安全入队后才计数；原 CPU 图像仍供首次提交失败重试。
fn close_video_frame(
    encoder: &mut Encoder,
    held: &mut Option<HeldVideoFrame>,
    until: Duration,
    spans: &mut u64,
) -> Result<()> {
    let Some(frame) = held.take() else {
        return Ok(());
    };
    let Some(span) = stop::video_span(frame.time, until)? else {
        return Ok(());
    };
    let next_spans = spans.checked_add(1).ok_or("录制视频跨度计数溢出")?;
    match (encoder, frame.pixels) {
        (Encoder::Ready(writer), VideoPixels::Cpu(image)) => {
            writer.write_video(&image.bgra, image.width, span.time, span.duration)?;
        }
        (Encoder::Ready(writer), VideoPixels::Prepared(sample)) => {
            writer.write_prepared_video(sample, span.time, span.duration)?;
        }
        (
            Encoder::Pending { frames, bytes, .. } | Encoder::PolicyPending { frames, bytes, .. },
            VideoPixels::Cpu(image),
        ) => {
            let next_bytes = bytes
                .checked_add(image.bgra.len())
                .ok_or("待编码画面字节计数溢出")?;
            if frames.len() >= MAX_PENDING_FRAMES || next_bytes > PENDING_FRAME_BYTES {
                return Err("待编码画面达到缓存上限".into());
            }
            frames.push((image, span.time, span.duration));
            *bytes = next_bytes;
        }
        (_, VideoPixels::Prepared(_)) => {
            return Err("延后 GPU 样本不能跨编码器尝试使用".into());
        }
    }
    *spans = next_spans;
    Ok(())
}

fn capture_cpu_frame(
    live: &LiveCapture,
    painter: &mut FramePainter,
    region: ScreenRect,
    annotations: Option<&live_annotation::FrameScene>,
    camera: Option<&mut CameraRecordingSession>,
    state: &Recording,
    cancelled: &dyn Fn() -> bool,
) -> Result<CaptureOutcome<VideoPixels>> {
    match live.read_recording(region, cancelled)? {
        CaptureOutcome::Ready(mut image) => {
            RecordingSizeLayout::new(region)?.validate_frame(
                image.width,
                image.height,
                image.bgra.len(),
            )?;
            if let Some(camera) = camera {
                if let Some(warning) = camera.composite(&mut image, region, Instant::now()) {
                    state.warn(warning);
                }
            }
            if let Some(scene) = annotations {
                scene.composite(&mut image, region)?;
            }
            if painter.needs_painting() {
                painter.paint(&mut image.bgra, image.width, image.height);
            }
            Ok(CaptureOutcome::Ready(VideoPixels::Cpu(image)))
        }
        CaptureOutcome::Cancelled => Ok(CaptureOutcome::Cancelled),
    }
}

fn record_frame(
    live: &LiveCapture,
    encoder: &mut Encoder,
    painter: &mut FramePainter,
    region: ScreenRect,
    annotations: Option<&live_annotation::FrameScene>,
    camera: Option<&mut CameraRecordingSession>,
    state: &Recording,
    time: Duration,
    held: &mut Option<HeldVideoFrame>,
    spans: &mut u64,
    cancelled: &dyn Fn() -> bool,
    discarded: &dyn Fn() -> bool,
) -> Result<FrameCommit> {
    let time = stop::video_time(time)?;
    let gpu_input = camera.is_none()
        && encoder.writer().is_some_and(|writer| {
            writer.uses_gpu_input() && !painter.needs_painting() && annotations.is_none()
        });
    if gpu_input {
        match live.recording_texture(region, cancelled)? {
            CaptureOutcome::Ready(Some((texture, crop))) => {
                if discarded() {
                    return Ok(FrameCommit::Cancelled);
                }
                // 先交出旧 held 推进 Sink，再占新的池槽；否则旧样本可能堵住最后一个槽。
                // 新画面已在自有冻结纹理中，提交旧样本不会归还或改写这个输入。
                close_video_frame(encoder, held, time, spans)?;
                let writer = encoder.writer().ok_or("GPU 录屏编码器状态已变化")?;
                let sample = writer.prepare_texture(texture, crop)?;
                if discarded() {
                    return Ok(FrameCommit::Cancelled);
                }
                *held = Some(HeldVideoFrame {
                    pixels: VideoPixels::Prepared(sample),
                    time,
                });
                return Ok(FrameCommit::Written);
            }
            CaptureOutcome::Ready(None) => {}
            CaptureOutcome::Cancelled => return Ok(FrameCommit::Cancelled),
        }
    }
    // 初始化/首样本回退仍保留原 CPU 帧；跨屏、旋转及合成继续沿原路径。
    let captured = capture_cpu_frame(live, painter, region, annotations, camera, state, cancelled)?;
    stop::after_capture(Ok(captured), |pixels| {
        if discarded() {
            return Ok(FrameCommit::Cancelled);
        }
        close_video_frame(encoder, held, time, spans)?;
        *held = Some(HeldVideoFrame { pixels, time });
        Ok(FrameCommit::Written)
    })
}

fn run(
    region: ScreenRect,
    options: &RecordingOptions,
    output: &Path,
    state: &Arc<Recording>,
    files: &mut RecordingFiles,
    requests: SessionRequests,
) -> Result<RecordingResult> {
    let SessionRequests {
        encoding,
        quality_request,
        codec_request,
        codec_plan,
        output_plan,
        camera_request,
        scale_plan,
    } = requests;
    let layout = RecordingSizeLayout::new(region)?;
    let initial_mix = initial_mix_for_session(
        options,
        crate::file_launch_command::experimental_media_enabled(),
    );
    let t0 = Instant::now();
    let trace = |label: &str| {
        if std::env::var_os("RUSTSHOT_TRACE").is_some() {
            eprintln!("REC {label} {:.1}ms", t0.elapsed().as_secs_f64() * 1000.);
        }
    };
    // 音频设备初始化与桌面复制、编码器创建互不依赖，放到并行线程缩短开始等待。
    let audio_start = options.audio_active().then(|| {
        let device_selection =
            crate::file_launch_command::experimental_media_enabled().then(|| {
                (
                    options.system_output.clone(),
                    options.microphone_input.clone(),
                )
            });
        let mut sources = Vec::new();
        if options.system_audio {
            sources.push(AudioSource::System);
        }
        if options.microphone {
            sources.push(AudioSource::Microphone);
        }
        std::thread::Builder::new()
            .name("rustshot-audio-start".into())
            .spawn(move || {
                match device_selection {
                    Some((system_output, microphone_input)) => AudioCapture::start_with_devices(
                        &sources,
                        &system_output,
                        &microphone_input,
                    ),
                    None => AudioCapture::start(&sources),
                }
                .map_err(|error| error.to_string())
            })
    });
    let _runtime = MfRuntime::new()?;
    trace("mf");
    let cancelled = || state.stop.load(Ordering::Acquire);
    let live = stop::initial_capture(LiveCapture::new_for_recording(region, &cancelled))?;
    trace("live");
    let (width, height) = scale_plan
        .map_or((layout.encoded_width, layout.encoded_height), |plan| {
            (plan.geometry.target_width, plan.geometry.target_height)
        });
    let fps = options.fps.max(1);
    let mp4 = files.video_path().to_path_buf();
    let mut audio = if let Some(audio_start) = audio_start {
        let started = audio_start
            .map_err(|error| error.to_string())
            .and_then(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|_| Err("音频初始化线程异常退出".into()))
            });
        match started {
            Ok((capture, warnings)) => {
                for warning in warnings {
                    state.warn(warning);
                }
                Some(AudioSink::new(capture, files, initial_mix, state)?)
            }
            Err(error) => {
                state.warn(trf!(
                    "{error}，本次录制无声音",
                    "{error}; this recording has no sound"
                ));
                None
            }
        }
    } else {
        None
    };
    trace("audio");
    // 开启保护后才让编码器线程与采集并发使用同一设备；失败时编码器自身会回退 CPU 输入。
    let _ = crate::recording_gpu::protect_device(live.recording_device());
    let encoder_start = (|| -> Result<Encoder> {
        Ok(if codec_plan.is_none() {
            let device = live.recording_device().clone();
            let path = mp4.clone();
            let bitrate = video_bitrate(width, height, fps, options.preset);
            let has_audio = audio.is_some();
            let thread = std::thread::Builder::new()
                .name("rustshot-encoder-start".into())
                .spawn(move || {
                    let _runtime = MfRuntime::new().map_err(|e| e.to_string())?;
                    Mp4Writer::new_for_device(
                        &path,
                        width,
                        height,
                        fps,
                        bitrate,
                        has_audio,
                        Some(&device),
                    )
                    .map(SendWriter)
                    .map_err(|e| e.to_string())
                })?;
            Encoder::Pending {
                thread: Some(thread),
                frames: Vec::new(),
                bytes: 0,
            }
        } else {
            let plan = codec_plan.ok_or("缺少预检后的编码计划")?;
            let paths = files.encoder_attempt_paths(plan.attempts().len())?;
            start_policy_encoder(
                plan,
                scale_plan,
                live.recording_device().clone(),
                paths,
                audio.is_some(),
                state,
            )?
        })
    })();
    let mut encoder = match encoder_start {
        Ok(encoder) => encoder,
        Err(error) => {
            return Err(stop::finalize_without_video(Err(error), || {
                audio.take().map(|audio| audio.finish(state)).transpose()
            }));
        }
    };
    let mut selected_attempt = EncoderSelection::default();
    let (mut painter, warning) = FramePainter::new(
        region,
        region_scale(region),
        options.show_cursor,
        options.show_keys,
        options.show_mouse,
        options.filter_typing,
        options.key_position,
    );
    if let Some(warning) = warning {
        state.warn(warning);
    }
    trace("painter");
    let interval = Duration::from_secs_f64(1. / f64::from(fps));
    let mut next_frame = Duration::ZERO;
    let mut held_video = None;
    // UI frames 仍计成功采集；发布零帧门只认已闭合且成功写入或安全排队的跨度。
    let mut video_spans = 0_u64;
    while !state.go.load(Ordering::Acquire) && !state.stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(2));
    }
    // 清单落盘先于活动时钟，避免磁盘延迟变成首帧之前的一段录制时间。
    let recording_phase = if state.stop.load(Ordering::Acquire) {
        Ok(())
    } else {
        files.mark_recording()
    };
    // 显式设备请求只在 go 后启动；首帧到来前和画面失效后都继续录屏，不等待摄像头。
    let mut camera = if recording_phase.is_ok() && !state.stop.load(Ordering::Acquire) {
        camera_request.and_then(|request| {
            match CameraRecordingSession::start(
                request,
                region,
                crate::file_launch_command::experimental_media_enabled(),
            ) {
                Ok(camera) => Some(camera),
                Err(error) => {
                    state.warn(trf!(
                        "摄像头未能开始：{error}；本次屏幕录制继续，摄像头画面不显示",
                        "Camera could not start: {error}; screen recording continues without the camera image"
                    ));
                    None
                }
            }
        })
    } else {
        None
    };
    // 设备初始化期间缓冲的声音早于首帧画面，丢弃后音画从同一时刻开始。
    if let Some(audio) = &audio {
        audio.capture.discard();
    }
    painter.discard_events();
    let mut controls = control_state::ControlState::new(state.control_begin());
    if let Some(camera) = &mut camera {
        camera.set_paused(controls.is_paused(), controls.latest().point.instant);
    }
    trace("timeline start");
    let sync_controls = |controls: &mut control_state::ControlState,
                         camera: &mut Option<CameraRecordingSession>,
                         painter: &mut FramePainter,
                         audio: Option<&mut AudioSink>|
     -> Result<control_state::ControlFence> {
        let fence = controls.apply(state.control_take(), |paused, point| {
            // 每条暂停／恢复都清理摄像头缓存，不能把同一阻塞期间的短脉冲合并。
            if let Some(camera) = camera.as_mut() {
                camera.set_paused(paused, point.instant);
            }
            painter.discard_events();
        })?;
        if let Some(audio) = audio {
            audio.set_control_fence(controls.clock(), fence.point, fence.active)?;
        }
        state.active_ms.store(
            fence.active.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        if !fence.stopped {
            state.set_phase(if controls.is_paused() {
                Phase::Paused
            } else {
                Phase::Recording
            });
        }
        Ok(fence)
    };
    // 主体失败被保留到显式收尾，不能让 writer 的析构吞掉 Finalize 错误。
    let body = (|| -> Result<()> {
        recording_phase?;
        controls.check_begin()?;
        let initial = controls.latest();
        if let Some(audio) = &mut audio {
            audio.set_control_fence(controls.clock(), initial.point, initial.active)?;
        }
        // 起钟前的 Stop 只取消准备，不能拿早于起钟的收据计算活动时长。
        if initial.stopped {
            return Ok(());
        }
        loop {
            let fence = sync_controls(&mut controls, &mut camera, &mut painter, audio.as_mut())?;
            if fence.stopped {
                break;
            }
            if encoder.has_pending_frames()
                && (encoder.due() || audio.as_ref().is_some_and(AudioSink::pending_full))
            {
                encoder.resolve(audio.as_mut(), files, &mut selected_attempt, state)?;
                trace("encoder ready (control fence)");
                continue;
            }
            if controls.is_paused() {
                if let Some(audio) = audio.as_mut() {
                    // 边界不增长；仍可接收迟到的暂停前声音，不丢弃恢复后的原始包。
                    audio.write(encoder.writer(), fence.active)?;
                    audio.report_warnings(state);
                }
                painter.discard_events();
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
            // 先计入 held 与下一张瞬时分配，再决定交接；不在超限后才阻塞。
            if encoder.needs_pending_capacity(layout.source_bytes, held_video.as_ref()) {
                if !encoder.has_pending_frames() {
                    return Err("录制画面与尾帧超过初始化缓存上限".into());
                }
                encoder.resolve(audio.as_mut(), files, &mut selected_attempt, state)?;
                trace("encoder ready (pending frame limit)");
                // 交接可能阻塞；回到循环头重新消费请求后才能提交新媒体。
                continue;
            }
            let time = fence.active;
            if time + Duration::from_millis(1) < next_frame {
                if let Some(audio) = audio.as_mut() {
                    audio.write(encoder.writer(), time)?;
                    audio.report_warnings(state);
                }
                std::thread::sleep((next_frame - time).min(Duration::from_millis(20)));
                continue;
            }
            let annotations = state.annotations.snapshot();
            let frame = record_frame(
                &live,
                &mut encoder,
                &mut painter,
                region,
                annotations.as_deref(),
                camera.as_mut(),
                state,
                time,
                &mut held_video,
                &mut video_spans,
                &cancelled,
                &|| state.discard.load(Ordering::Acquire),
            );
            if !stop::commit_frame(frame, &state.frames)? {
                break;
            }
            // 帧时间取自采集前的同锁 fence；原生提交期间发生的请求在下一次写音前消费。
            next_frame = (next_frame + interval).max(time + interval);
            let fence = sync_controls(&mut controls, &mut camera, &mut painter, audio.as_mut())?;
            if fence.stopped {
                break;
            }
            // 只有 held 时还没有可重试的闭合首样本，不能先 Ready 并接受音频。
            if encoder.has_pending_frames()
                && (encoder.due() || audio.as_ref().is_some_and(AudioSink::pending_full))
            {
                encoder.resolve(audio.as_mut(), files, &mut selected_attempt, state)?;
                trace("encoder ready");
                continue;
            }
            if let Some(audio) = audio.as_mut() {
                audio.write(encoder.writer(), fence.active)?;
                audio.report_warnings(state);
            }
        }
        Ok(())
    })();
    let mut boundary_failures = stop::Failures::default();
    let body_ok = body.is_ok();
    boundary_failures.record(tr!("录制", "recording"), body);
    if body_ok && !controls.latest().stopped {
        // 采集取消也要读取同锁边界；失败时只保留最后认可的 fence，不补一段等待时间。
        boundary_failures.record(
            tr!("停止边界", "stop boundary"),
            sync_controls(&mut controls, &mut camera, &mut painter, audio.as_mut()),
        );
    }
    boundary_failures.record(tr!("时钟封口", "clock closure"), controls.finish());
    let end = controls.latest();
    let duration = end.active;
    if let Some(audio) = &mut audio {
        boundary_failures.record(
            tr!("音频边界", "audio boundary"),
            audio.set_control_fence(controls.clock(), end.point, duration),
        );
    }
    state.active_ms.store(
        duration.as_millis().min(u128::from(u64::MAX)) as u64,
        Ordering::Relaxed,
    );
    let body = boundary_failures.result(());
    if let Some(audio) = &audio {
        audio.report_warnings(state);
    }
    state.set_phase(Phase::Finishing);
    // 采集队列只保留300ms；先保存已封口的尾音，再等待摄像头或提交末帧。
    let body = if body.is_ok()
        && (video_spans != 0 || held_video.is_some())
        && !state.discard.load(Ordering::Acquire)
    {
        if let Some(audio) = audio.as_mut() {
            let tail = audio.write_tail(encoder.writer(), duration);
            audio.report_warnings(state);
            body.and(tail)
        } else {
            body
        }
    } else {
        body
    };
    // 活动时间已封口；有限等待和停机诊断不延长尾帧或音轨时间轴。
    if let Some(camera) = camera.take() {
        let (status, warning) = camera.finish();
        *state.camera_report.lock().unwrap() = Some(status);
        if let Some(warning) = warning {
            state.warn(warning);
        }
    }
    drop(painter);
    let body = match body {
        Err(error) => {
            drop(held_video.take());
            Err(error)
        }
        Ok(()) if state.discard.load(Ordering::Acquire) => {
            drop(held_video.take());
            Err(tr!("录制已取消", "Recording canceled").into())
        }
        Ok(()) => close_video_frame(&mut encoder, &mut held_video, duration, &mut video_spans)
            .and_then(|()| {
                if video_spans == 0 {
                    Err(stop::no_frames())
                } else {
                    Ok(())
                }
            }),
    };
    // 很短的录制可能在编码器就绪前就停止：先等它并补写暂存内容再收尾。
    let resolved = encoder.resolve(
        if body.is_ok() { audio.as_mut() } else { None },
        files,
        &mut selected_attempt,
        state,
    );
    let mut failures = stop::Failures::default();
    failures.record(tr!("录制", "recording"), body);
    failures.record(tr!("编码器交接", "encoder handoff"), resolved);
    let body = failures.result(());
    let Encoder::Ready(writer) = encoder else {
        return Err(stop::finalize_without_video(body, || {
            audio.take().map(|audio| audio.finish(state)).transpose()
        }));
    };
    let selected_slot = selected_attempt.attempt.map(|attempt| attempt.slot);
    let writer_audio_requested = audio.is_some();
    let tracks = stop::finalize_recording(
        body,
        video_spans,
        SessionTail {
            writer,
            audio,
            duration,
            state: state.clone(),
            encoding,
            requested_profile: encoding
                .quality
                .unwrap_or(QualityProfile::Current(options.preset)),
            quality_request,
            scale_plan,
            codec_request,
            selection: selected_attempt,
            output_plan,
            writer_audio_requested,
        },
    )?;
    if state.discard.load(Ordering::Acquire) {
        return Err(tr!("录制已取消", "Recording canceled").into());
    }
    if let Some(index) = selected_slot {
        files.promote_encoder_attempt(index)?;
    }
    if options.preset.is_gif() {
        state.set_phase(Phase::Converting);
        crate::gif_export::mp4_to_gif(&mp4, files.gif_path(), fps, |progress| {
            state.progress.store(progress.to_bits(), Ordering::Relaxed);
            !state.discard.load(Ordering::Acquire)
        })?;
        files.register_gif()?;
    }
    let source = if options.preset.is_gif() {
        files.gif_path().to_path_buf()
    } else {
        mp4.clone()
    };
    let bytes = output::validate_file(&source)?;
    // 每个文件都先验证，最后才发布成片；失败由 worker 撤回已发布的自有文件。
    output::validate_file(&mp4)?;
    for (_, track) in &tracks {
        output::validate_file(track)?;
    }
    if state.discard.load(Ordering::Acquire) {
        return Err(tr!("录制已取消", "Recording canceled").into());
    }
    let mut published_tracks = Vec::with_capacity(tracks.len());
    for (audio_source, source) in tracks {
        let destination = track_path(output, audio_source);
        files.publish_file(&source, &destination)?;
        published_tracks.push((audio_source, destination));
    }
    let preview = if options.preset.is_gif() {
        let preview = intermediate_path(output);
        files.publish_file(&mp4, &preview)?;
        preview
    } else {
        output.to_path_buf()
    };
    files.publish_file(&source, output)?;
    Ok(RecordingResult {
        path: output.to_path_buf(),
        preview,
        width,
        height,
        duration,
        preset: options.preset,
        bytes,
        tracks: published_tracks,
        initial_mix,
        encoding: state.encoding_report(),
    })
}

/// 后台自测：录制约 1.5 秒（含暂停），校验 MP4 可解码、尺寸与时长；再验证 GIF 转码。
pub fn self_test() -> Result<()> {
    let directory = PathBuf::from("target/qa");
    std::fs::create_dir_all(&directory)?;
    let roundtrip = pixel_roundtrip(&directory)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let capture = LiveCapture::new()?;
    let b = capture.bounds;
    drop(capture);
    let region = ScreenRect {
        left: b.left + 40,
        top: b.top + 40,
        width: 321,
        height: 181,
    };
    let mut report = Vec::new();
    for preset in [Preset::CompactMp4, Preset::Gif] {
        let extension = if preset.is_gif() { "gif" } else { "mp4" };
        let path = directory.join(format!("v05-recording-{timestamp}.{extension}"));
        let options = RecordingOptions {
            preset,
            fps: 30,
            show_cursor: true,
            system_audio: !preset.is_gif(),
            ..RecordingOptions::default()
        };
        let recording = Recording::start(region, options, path.clone())?;
        let start = Instant::now();
        while recording.frames.load(Ordering::Relaxed) < 20
            && start.elapsed() < Duration::from_secs(15)
        {
            if recording.outcome.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        recording.set_paused(true);
        std::thread::sleep(Duration::from_millis(150));
        let before = recording.frames.load(Ordering::Relaxed);
        let elapsed_before = recording.elapsed();
        std::thread::sleep(Duration::from_millis(400));
        if recording.frames.load(Ordering::Relaxed) != before {
            return Err("暂停后仍采集帧".into());
        }
        if recording.elapsed() != elapsed_before {
            return Err("暂停期间计时仍在推进".into());
        }
        recording.set_paused(false);
        std::thread::sleep(Duration::from_millis(500));
        recording.request_stop();
        let start = Instant::now();
        while recording.outcome.lock().unwrap().is_none()
            && start.elapsed() < Duration::from_secs(60)
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let result = recording
            .outcome
            .lock()
            .unwrap()
            .take()
            .ok_or("录屏结束超时")?
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        let frames = recording.frames.load(Ordering::Relaxed);
        if frames < 20 || result.bytes < 1000 {
            return Err("录屏输出无效".into());
        }
        let _runtime = MfRuntime::new()?;
        let mut reader = crate::media_foundation::VideoReader::open(&result.preview)?;
        if (reader.width, reader.height) != (320, 180) {
            return Err(format!("录屏尺寸错误：{}×{}", reader.width, reader.height).into());
        }
        let mut decoded = 0;
        while reader.next()?.is_some() {
            decoded += 1;
        }
        // 暂停的 400ms 不计入成片；允许编码器首尾帧误差。
        let total = result.duration.as_secs_f64();
        if decoded < 10 || reader.duration.as_secs_f64() > total + 0.25 {
            return Err(format!(
                "成片时长异常：解码 {decoded} 帧，文件 {:.2}s，录制 {total:.2}s",
                reader.duration.as_secs_f64()
            )
            .into());
        }
        drop(reader);
        // MP4 轮录了系统声：分轨应与成片等长，并能按新音量重新混音（视频直通）。
        let mut audio = String::from("null");
        if !result.tracks.is_empty() {
            let mut tracks = Vec::new();
            for (source, track) in &result.tracks {
                tracks.push((*source, crate::wav_track::read(track)?));
            }
            let track_seconds = tracks[0].1.len() as f64 / 2. / f64::from(AUDIO_RATE);
            if (track_seconds - total).abs() > 0.2 {
                return Err(
                    format!("分轨时长 {track_seconds:.2}s 与成片 {total:.2}s 不一致").into(),
                );
            }
            let labelled: Vec<(AudioSource, &[f32])> =
                tracks.iter().map(|(s, t)| (*s, t.as_slice())).collect();
            let pcm = mix_tracks(
                &labelled,
                MixLevels {
                    system: 50,
                    microphone: 200,
                },
            );
            let mixed = result.path.with_extension("mix.mp4");
            crate::media_foundation::remux_audio(&result.path, &mixed, &pcm)?;
            let mut reader = crate::media_foundation::VideoReader::open(&mixed)?;
            let mut remuxed = 0;
            while reader.next()?.is_some() {
                remuxed += 1;
            }
            drop(reader);
            if remuxed != decoded {
                return Err(format!("重新混音后视频帧 {remuxed}，原 {decoded}").into());
            }
            audio = format!(
                "{{\"tracks\":{},\"trackSeconds\":{track_seconds:.2},\"remuxFrames\":{remuxed},\"remux\":\"{}\"}}",
                result.tracks.len(),
                mixed.display().to_string().replace('\\', "/")
            );
            remove_tracks(&result);
        } else if preset == Preset::CompactMp4 {
            audio = format!("\"无分轨：{}\"", recording.warnings().join("；"));
        }
        report.push(format!(
            "{{\"preset\":\"{}\",\"frames\":{frames},\"decoded\":{decoded},\"seconds\":{total:.2},\"bytes\":{},\"audio\":{audio},\"file\":\"{}\"}}",
            preset.label(),
            result.bytes,
            result.path.display().to_string().replace('\\', "/")
        ));
        if result.preview != result.path {
            let _ = std::fs::remove_file(&result.preview);
        }
    }
    println!(
        "{{\"test\":\"mf_recording_pause_finalize_gif\",\"roundtrip\":{roundtrip},\"results\":[{}],\"ok\":true}}",
        report.join(",")
    );
    Ok(())
}

/// 非 16 倍数尺寸的合成画面经 MP4 编码再解码，逐像素比对；
/// 解码面对齐填充若按显示宽度读取，竖条纹会斜向错位，平均误差会远超阈值。
fn pixel_roundtrip(directory: &Path) -> Result<String> {
    let _runtime = MfRuntime::new()?;
    let (width, height) = (796_u32, 530_u32);
    let mut frame = vec![0_u8; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let stripe = if (x / 8) % 2 == 0 { 230 } else { 30 };
            frame[i] = stripe;
            frame[i + 1] = (y * 255 / height) as u8;
            frame[i + 2] = (x * 255 / width) as u8;
            frame[i + 3] = 255;
        }
    }
    let path = directory.join("v05-recording-roundtrip-796x530.mp4");
    let _ = std::fs::remove_file(&path);
    let mut writer = Mp4Writer::new(&path, width, height, 30, 20_000_000, false)?;
    let interval = Duration::from_millis(33);
    for index in 0..15_u32 {
        writer.write_video(&frame, width, interval * index, interval)?;
    }
    writer.finish()?;
    let mut reader = crate::media_foundation::VideoReader::open(&path)?;
    if (reader.width, reader.height) != (width, height) {
        return Err(format!(
            "往返解码尺寸 {}×{}，期望 {width}×{height}",
            reader.width, reader.height
        )
        .into());
    }
    let mut worst = 0_f64;
    let mut frames = 0;
    while let Some(decoded) = reader.next()? {
        frames += 1;
        let total: u64 = decoded
            .bgra
            .iter()
            .zip(&frame)
            .enumerate()
            .filter(|(i, _)| i % 4 != 3)
            .map(|(_, (a, b))| u64::from(a.abs_diff(*b)))
            .sum();
        let mean = total as f64 / f64::from(width * height * 3);
        worst = worst.max(mean);
    }
    drop(reader);
    if frames < 10 || worst > 6. {
        return Err(format!("编码往返像素误差过大：{frames} 帧，最大平均误差 {worst:.2}").into());
    }
    let _ = std::fs::remove_file(&path);
    Ok(format!(
        "{{\"size\":\"{width}x{height}\",\"frames\":{frames},\"worstMeanAbsDiff\":{worst:.2}}}"
    ))
}

#[cfg(test)]
mod audio_timing_tests {
    use super::*;

    fn codec_test_region() -> ScreenRect {
        ScreenRect {
            left: -384,
            top: -256,
            width: 385,
            height: 257,
        }
    }

    #[test]
    fn recording_resize_request_preserves_legacy_none_and_rejects_closed_gate() {
        let region = codec_test_region();
        let options = RecordingOptions::default();
        let original = validate_session_requests(
            region,
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            false,
        )
        .unwrap();
        assert!(original.scale_plan.is_none() && original.codec_plan.is_none());
        let unchanged = validate_session_requests_with_resize(
            region,
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            None,
            false,
        )
        .unwrap();
        assert!(unchanged.scale_plan.is_none() && unchanged.codec_plan.is_none());
        assert!(validate_session_requests_with_resize(
            region,
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            Some(ResizeRequest {
                target_width: 192,
                target_height: 128
            }),
            false,
        )
        .is_err());
    }

    #[test]
    fn recording_resize_request_uses_target_quality_and_preserves_source_layout() {
        let region = ScreenRect {
            width: 1921,
            height: 1081,
            ..codec_test_region()
        };
        let options = RecordingOptions::default();
        let target = ResizeRequest {
            target_width: 960,
            target_height: 540,
        };
        let checked = validate_session_requests_with_resize(
            region,
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            Some(target),
            true,
        )
        .unwrap();
        let scale = checked.scale_plan.unwrap();
        assert_eq!(scale.source, RecordingSizeLayout::new(region).unwrap());
        assert_eq!(
            (scale.source.left, scale.source.top),
            (region.left, region.top)
        );
        assert_eq!(
            (scale.geometry.source_width, scale.geometry.source_height),
            (1920, 1080)
        );
        assert_eq!(
            (scale.geometry.target_width, scale.geometry.target_height),
            (960, 540)
        );
        let expected = quality_plan(
            960,
            540,
            options.fps.max(1),
            QualityProfile::Current(options.preset),
        )
        .unwrap();
        let source = quality_plan(
            region.width,
            region.height,
            options.fps.max(1),
            QualityProfile::Current(options.preset),
        )
        .unwrap();
        assert_ne!(expected.bitrate_bps, source.bitrate_bps);
        assert_eq!(
            checked.codec_plan.unwrap().encoder_plan().quality(),
            expected
        );
        // 显式同尺寸也进入有请求证据的策略路线，不能退回无请求的旧入口。
        let identity = validate_session_requests_with_resize(
            region,
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            Some(ResizeRequest {
                target_width: 1920,
                target_height: 1080,
            }),
            true,
        )
        .unwrap();
        assert!(identity.scale_plan.unwrap().geometry.is_identity());
        assert!(identity.codec_plan.is_some());
    }

    #[test]
    fn recording_resize_request_validates_advanced_receipt_against_target() {
        let region = codec_test_region();
        let options = RecordingOptions::default();
        let resize = ResizeRequest {
            target_width: 192,
            target_height: 128,
        };
        let profile = QualityProfile::Current(Preset::CompactMp4);
        let encoding = EncodingOptions {
            policy: EncoderPolicy::Auto,
            quality: Some(profile),
        };
        let make = |width, height| {
            advanced::resolve(
                width,
                height,
                options.fps.max(1),
                profile,
                advanced::QualityOverrides::default(),
            )
            .unwrap()
        };
        let original = make(region.width, region.height);
        assert!(validate_session_requests_with_resize(
            region,
            &options,
            Path::new("recording.mp4"),
            encoding,
            Some(original),
            None,
            Some(resize),
            true,
        )
        .is_err());
        let target = make(resize.target_width, resize.target_height);
        let checked = validate_session_requests_with_resize(
            region,
            &options,
            Path::new("recording.mp4"),
            encoding,
            Some(target),
            Some(CodecRequest::HevcWithH264Fallback),
            Some(resize),
            true,
        )
        .unwrap();
        assert_eq!(checked.quality_request, Some(target));
        assert_eq!(
            checked.codec_plan.unwrap().encoder_plan().quality(),
            target.plan
        );
        assert_eq!(
            checked.scale_plan.unwrap().source,
            RecordingSizeLayout::new(region).unwrap()
        );
        assert_eq!(original, make(region.width, region.height));
    }

    #[test]
    fn recording_codec_request_session_preserves_default_and_preflights_output() {
        let options = RecordingOptions::default();
        let requests = validate_session_requests(
            codec_test_region(),
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            false,
        )
        .unwrap();
        assert!(requests.codec_plan.is_none());
        assert!(requests.codec_request.is_none());
        assert!(requests.camera_request.is_none());
        assert_eq!(requests.encoding.policy, EncoderPolicy::Auto);
        assert!(requests.encoding.quality.is_none());
        assert_eq!(requests.output_plan.codec_order, [VideoCodec::H264]);
        assert!(validate_session_requests(
            codec_test_region(),
            &options,
            Path::new("recording.webm"),
            EncodingOptions::default(),
            None,
            None,
            false,
        )
        .is_err());
        let gif = RecordingOptions {
            preset: Preset::Gif,
            system_audio: true,
            ..options
        };
        let requests = validate_session_requests(
            codec_test_region(),
            &gif,
            Path::new("recording.gif"),
            EncodingOptions::default(),
            None,
            None,
            false,
        )
        .unwrap();
        assert!(requests.codec_plan.is_none());
        assert!(requests.camera_request.is_none());
        assert_eq!(
            requests.output_plan.final_format,
            output_plan::FinalFormat::Gif
        );
        assert_eq!(
            requests.output_plan.intermediate_format,
            Some(output_plan::FinalFormat::Mp4)
        );
        assert!(requests.output_plan.capture_audio_requested);
        assert!(requests.output_plan.gif_audio_suppressed);
        assert_eq!(
            requests.output_plan.planned_embedded_audio,
            output_plan::PlannedEmbeddedAudio::None
        );
        assert!(validate_session_requests(
            codec_test_region(),
            &gif,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            None,
            None,
            false,
        )
        .is_err());
    }

    #[test]
    fn recording_codec_request_session_gates_hevc_and_keeps_full_policy() {
        let options = RecordingOptions::default();
        let encoding = EncodingOptions {
            policy: EncoderPolicy::HardwarePreferred,
            quality: None,
        };
        assert!(validate_session_requests(
            codec_test_region(),
            &options,
            Path::new("recording.mp4"),
            encoding,
            None,
            Some(CodecRequest::HevcWithH264Fallback),
            false,
        )
        .is_err());
        let requests = validate_session_requests(
            codec_test_region(),
            &options,
            Path::new("recording.mp4"),
            encoding,
            None,
            Some(CodecRequest::HevcWithH264Fallback),
            true,
        )
        .unwrap();
        let plan = requests.codec_plan.as_ref().unwrap();
        assert_eq!(plan.attempts().len(), 6);
        assert_eq!(plan.codec_order(), [VideoCodec::Hevc, VideoCodec::H264]);
        assert_eq!(
            plan.encoder_plan().policy(),
            EncoderPolicy::HardwarePreferred
        );
        let quality = plan.encoder_plan().quality();
        assert_eq!((quality.width, quality.height), (384, 256));
        assert_eq!(quality.fps, options.fps);
        assert_eq!(quality.parameters, Default::default());
        assert_eq!(requests.output_plan.codec_order, plan.codec_order());
        let h264 = validate_session_requests(
            codec_test_region(),
            &options,
            Path::new("recording.mp4"),
            encoding,
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(h264.codec_plan.unwrap().attempts().len(), 3);
    }

    #[test]
    fn recording_codec_request_session_rejects_av1_even_with_experiments_enabled() {
        let options = RecordingOptions::default();
        for codec in [CodecRequest::Av1, CodecRequest::Av1WithH264Fallback] {
            for experimental in [false, true] {
                for policy in EncoderPolicy::ALL {
                    let error = validate_session_requests(
                        codec_test_region(),
                        &options,
                        Path::new("recording.mp4"),
                        EncodingOptions {
                            policy,
                            ..EncodingOptions::default()
                        },
                        None,
                        Some(codec),
                        experimental,
                    )
                    .unwrap_err();
                    assert!(error.to_string().contains("AV1"));
                }
            }
        }
    }

    #[test]
    fn recording_codec_request_session_rejects_gif_and_hevc_tuning_without_discarding_requests() {
        let options = RecordingOptions::default();
        let gif = RecordingOptions {
            preset: Preset::Gif,
            ..options.clone()
        };
        assert!(validate_session_requests(
            codec_test_region(),
            &gif,
            Path::new("recording.gif"),
            EncodingOptions::default(),
            None,
            Some(CodecRequest::HevcWithH264Fallback),
            true,
        )
        .is_err());
        let region = codec_test_region();
        let request = advanced::resolve(
            region.width,
            region.height,
            options.fps,
            QualityProfile::TextDetail,
            advanced::QualityOverrides::default(),
        )
        .unwrap();
        let encoding = EncodingOptions {
            policy: EncoderPolicy::Auto,
            quality: Some(request.base_profile),
        };
        assert!(validate_session_requests(
            region,
            &options,
            Path::new("recording.mp4"),
            encoding,
            Some(request),
            Some(CodecRequest::Hevc),
            true,
        )
        .is_err());
        let h264 = validate_session_requests(
            region,
            &options,
            Path::new("recording.mp4"),
            encoding,
            Some(request),
            Some(CodecRequest::H264),
            false,
        )
        .unwrap();
        assert_eq!(h264.quality_request, Some(request));
        assert_eq!(
            h264.codec_plan.unwrap().encoder_plan().quality(),
            request.plan
        );
        assert!(validate_session_requests(
            region,
            &options,
            Path::new("recording.mp4"),
            EncodingOptions::default(),
            Some(request),
            Some(CodecRequest::H264),
            false,
        )
        .is_err());
        let mean_only = advanced::resolve(
            region.width,
            region.height,
            options.fps,
            QualityProfile::Current(options.preset),
            advanced::QualityOverrides {
                avg_bitrate_bps: advanced::Override::Set(2_000_000),
                ..Default::default()
            },
        )
        .unwrap();
        let encoding = EncodingOptions {
            quality: Some(mean_only.base_profile),
            ..EncodingOptions::default()
        };
        let hevc = validate_session_requests(
            region,
            &options,
            Path::new("recording.mp4"),
            encoding,
            Some(mean_only),
            Some(CodecRequest::HevcWithH264Fallback),
            true,
        )
        .unwrap();
        assert_eq!(hevc.quality_request, Some(mean_only));
        assert_eq!(
            hevc.codec_plan
                .unwrap()
                .encoder_plan()
                .quality()
                .bitrate_bps,
            2_000_000
        );
    }

    #[test]
    fn recording_quality_request_session_rejects_stale_or_modified_receipts() {
        let region = ScreenRect {
            left: -1920,
            top: -100,
            width: 961,
            height: 541,
        };
        let options = RecordingOptions::default();
        let request = advanced::resolve(
            region.width,
            region.height,
            options.fps,
            QualityProfile::Current(Preset::CompactMp4),
            advanced::QualityOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            validate_session_quality_request(region, &options, request).unwrap(),
            request
        );
        assert_eq!((request.plan.width, request.plan.height), (960, 540));
        let mut changed_region = region;
        changed_region.width += 2;
        assert!(validate_session_quality_request(changed_region, &options, request).is_err());
        let mut changed = request;
        changed.plan.bitrate_bps += 1;
        assert!(validate_session_quality_request(region, &options, changed).is_err());
        let mut changed = request;
        changed.custom_requested = !request.custom_requested;
        assert!(validate_session_quality_request(region, &options, changed).is_err());
        let mut changed = request;
        changed.scope = "modified request metadata";
        assert!(validate_session_quality_request(region, &options, changed).is_err());
        let mut changed_options = options.clone();
        changed_options.fps = 60;
        assert!(validate_session_quality_request(region, &changed_options, request).is_err());
    }

    #[test]
    fn recording_quality_request_session_rejects_gif_output_and_gif_base() {
        let region = ScreenRect {
            left: 0,
            top: 0,
            width: 960,
            height: 540,
        };
        let mut options = RecordingOptions::default();
        let request = advanced::resolve(
            region.width,
            region.height,
            options.fps,
            QualityProfile::Current(Preset::CompactMp4),
            advanced::QualityOverrides::default(),
        )
        .unwrap();
        options.preset = Preset::Gif;
        assert!(validate_session_quality_request(region, &options, request).is_err());
        options.preset = Preset::CompactMp4;
        let gif_base = advanced::resolve(
            region.width,
            region.height,
            options.fps,
            QualityProfile::Current(Preset::Gif),
            advanced::QualityOverrides::default(),
        )
        .unwrap();
        assert!(validate_session_quality_request(region, &options, gif_base).is_err());
    }

    #[test]
    fn recording_incomplete_data_notice_preserves_original_error_source() {
        let mut calls = 0;
        let error = preserve_incomplete_recording::<()>(
            Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "audio write failed").into()),
            false,
            1,
            || {
                calls += 1;
                Some(PathBuf::from("incomplete-session"))
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::WriteZero
        );
        let message = error.to_string();
        assert!(message.contains("audio write failed"));
        assert!(message.contains("incomplete-session"));
    }

    #[test]
    fn recording_incomplete_retention_skips_success_cancel_and_zero_frames() {
        for (failed, discard, frames) in [(false, false, 1), (true, true, 1), (true, false, 0)] {
            let result = if failed {
                Err("recording failed".into())
            } else {
                Ok(())
            };
            let result = preserve_incomplete_recording(result, discard, frames, || {
                panic!("ineligible recording retained files")
            });
            assert_eq!(result.is_err(), failed);
        }
    }

    #[test]
    fn recording_pending_tiny_regions_are_bounded_by_frame_count() {
        for strict in [false, true] {
            let frames = (0..MAX_PENDING_FRAMES)
                .map(|_| {
                    (
                        crate::capture::Image {
                            width: 2,
                            height: 2,
                            bgra: vec![0; 16],
                            readback_ms: 0.0,
                        },
                        Duration::ZERO,
                        Duration::from_millis(33),
                    )
                })
                .collect();
            let bytes = MAX_PENDING_FRAMES * 16;
            let mut encoder = if strict {
                Encoder::PolicyPending {
                    thread: None,
                    frames,
                    bytes,
                }
            } else {
                Encoder::Pending {
                    thread: None,
                    frames,
                    bytes,
                }
            };
            assert!(bytes < PENDING_FRAME_BYTES);
            assert!(encoder.needs_pending_capacity(16, None));
            match &mut encoder {
                Encoder::Pending { frames, bytes, .. }
                | Encoder::PolicyPending { frames, bytes, .. } => {
                    frames.pop();
                    *bytes -= 16;
                }
                Encoder::Ready(_) => unreachable!(),
            }
            assert!(!encoder.needs_pending_capacity(16, None));
            let held = HeldVideoFrame {
                pixels: VideoPixels::Cpu(crate::capture::Image {
                    width: 2,
                    height: 2,
                    bgra: vec![1; 16],
                    readback_ms: 0.0,
                }),
                time: Duration::ZERO,
            };
            // 239 个闭合帧 + held + 下一张瞬时分配已经超出 240 个输入画面。
            assert!(encoder.needs_pending_capacity(16, Some(&held)));
        }
    }

    #[test]
    fn recording_video_lookbehind_queues_original_cpu_pixels_with_short_tail() {
        let mut encoder = Encoder::Pending {
            thread: None,
            frames: Vec::new(),
            bytes: 0,
        };
        let frame = |time| {
            Some(HeldVideoFrame {
                pixels: VideoPixels::Cpu(crate::capture::Image {
                    width: 2,
                    height: 2,
                    bgra: (0..16).collect(),
                    readback_ms: 0.0,
                }),
                time: stop::video_time(Duration::from_nanos(time)).unwrap(),
            })
        };
        let mut spans = 0;
        let mut held = frame(1_234);
        close_video_frame(
            &mut encoder,
            &mut held,
            Duration::from_nanos(9_999),
            &mut spans,
        )
        .unwrap();
        assert!(held.is_none());
        let mut held = frame(9_999);
        close_video_frame(
            &mut encoder,
            &mut held,
            Duration::from_nanos(20_049),
            &mut spans,
        )
        .unwrap();
        // 同 tick 尾部不伪造一个完整 interval，也不增加安全排队的跨度数。
        let mut held = frame(20_049);
        close_video_frame(
            &mut encoder,
            &mut held,
            Duration::from_nanos(20_099),
            &mut spans,
        )
        .unwrap();
        assert_eq!(spans, 2);
        let Encoder::Pending { frames, bytes, .. } = encoder else {
            unreachable!()
        };
        assert_eq!(bytes, 32);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0.bgra, (0..16).collect::<Vec<u8>>());
        assert_eq!(frames[0].1 + frames[0].2, frames[1].1);
        assert_eq!(frames[1].1 + frames[1].2, Duration::from_nanos(20_000));
        assert!(frames
            .iter()
            .all(|(_, _, duration)| *duration < Duration::from_millis(1)));
    }

    #[test]
    fn recording_audio_time_conversion_handles_long_sessions_without_nanosecond_overflow() {
        let seconds = 120 * 3600;
        let frames = seconds * u64::from(AUDIO_RATE);
        assert_eq!(audio_sample_time(frames), Duration::from_secs(seconds));
        assert_eq!(audio_target_frames(Duration::from_secs(seconds)), frames);
        assert_eq!(audio_target_frames(Duration::from_nanos(20_833)), 0);
        assert_eq!(audio_target_frames(Duration::from_nanos(20_834)), 1);
    }

    #[test]
    fn recording_audio_initial_mix_requires_experiment_and_clamps_the_session_copy() {
        let options = RecordingOptions {
            initial_mix: MixLevels {
                system: 0,
                microphone: 450,
            },
            ..Default::default()
        };
        assert_eq!(
            initial_mix_for_session(&options, false),
            MixLevels::default()
        );
        assert_eq!(
            initial_mix_for_session(&options, true),
            MixLevels {
                system: 0,
                microphone: 400,
            }
        );
        assert_eq!(options.initial_mix.microphone, 450);
    }

    #[test]
    fn recording_audio_clock_placement_rejects_paused_tail_and_retired_span() {
        use crate::audio_capture::{CaptureSegment, OutputPhase};

        let base = Instant::now();
        let point = |millis, qpc| ClockPoint {
            instant: base + Duration::from_millis(millis),
            qpc: Some(clock::QpcWindow {
                earliest: qpc,
                latest: qpc,
            }),
        };
        let read = |qpc, ordinal, value| CaptureRead {
            samples: vec![value; 96 * 2],
            real_frames: 96,
            segments: vec![CaptureSegment {
                raw_time: RawAudioTime {
                    endpoint_epoch: 1,
                    device_position: ordinal,
                    qpc_100ns: Some(qpc),
                    device_rate: AUDIO_RATE,
                    input_frames: 96,
                    discontinuity: false,
                },
                output_phase: Some(OutputPhase {
                    first_input_offset_num: 0,
                    input_rate: AUDIO_RATE,
                    output_rate: AUDIO_RATE,
                    output_frames: 96,
                }),
                packet_output_start: ordinal,
                packet_output_frames: 96,
                output_start: ordinal,
                frames: 96,
                sample_offset_frames: 0,
            }],
        };
        let mut clock = RecordingClock::start(point(0, 10_000_000), Duration::ZERO);
        clock.pause(point(20, 10_200_000));
        let mut track = TimedTrack::new();
        track
            .ingest(
                &read(10_190_000, 0, 1.),
                Duration::from_millis(20),
                0,
                |_| true,
                |lower, upper| clock.map_placement_window(lower, upper),
            )
            .unwrap();
        assert_eq!(track.summary().input.outside_window, 48);
        let before_pause = track.render(0, 960).unwrap();
        assert!(before_pause[..912 * 2].iter().all(|sample| *sample == 0.));
        assert!((before_pause[936 * 2] - 1.).abs() < 1e-6);

        track.reset();
        clock.resume(point(1_000, 20_000_000), Duration::from_millis(20));
        for (qpc, ordinal, value) in [(10_190_000, 96, 9.), (20_010_000, 192, 2.)] {
            track
                .ingest(
                    &read(qpc, ordinal, value),
                    Duration::from_millis(24),
                    960,
                    |_| true,
                    |lower, upper| clock.map_placement_window(lower, upper),
                )
                .unwrap();
        }
        let resumed = track.render(960, 192).unwrap();
        assert!(resumed[..48 * 2].iter().all(|sample| *sample == 0.));
        assert!((resumed[72 * 2] - 2.).abs() < 1e-6);
        assert!(resumed.iter().all(|sample| *sample <= 2.));
        assert_eq!(track.summary().input.outside_window, 48 + 96);
        assert_eq!(track.summary().placement.committed_until_frame, Some(1_152));
    }

    #[test]
    fn recording_audio_timing_does_not_count_packet_fragments_or_padding_as_new_anchors() {
        let raw = RawAudioTime {
            endpoint_epoch: 0,
            device_position: 40,
            qpc_100ns: Some(100),
            device_rate: 44_100,
            input_frames: 2,
            discontinuity: true,
        };
        let mut summary = AudioTimingSummary::default();
        summary.observe_packet(raw, 0);
        summary.observe_packet(raw, 0);
        assert_eq!(summary.reported_discontinuities, 1);
        summary.observe_packet(
            RawAudioTime {
                qpc_100ns: None,
                discontinuity: false,
                ..raw
            },
            2,
        );
        let read = CaptureRead {
            samples: vec![0.; 4],
            real_frames: 1,
            segments: vec![],
        };
        summary.observe(&read, 2, None);
        assert_eq!(summary.missing_qpc_packets, 1);
        assert_eq!((summary.real_frames, summary.padded_frames), (1, 1));
        assert_eq!(summary.first_raw, Some(raw));
        assert_eq!(summary.last_raw.unwrap().qpc_100ns, None);
    }
}
