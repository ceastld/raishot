//! 系统 Media Foundation 的 MP4 写入与解码；默认 H.264 视频 + AAC 音频。
//! 实验路径可请求 HEVC，独立试编码可请求 AV1；各阶段分别验证实际可用性。
#[path = "recording_frame_resize.rs"]
pub(crate) mod frame_resize;
#[cfg(test)]
#[path = "recording_resize_native.rs"]
mod resize_native;
#[cfg(test)]
#[path = "recording_storage_fault_native.rs"]
mod storage_fault_native;
#[cfg(test)]
#[path = "recording_async_finalize_native.rs"]
mod async_finalize_native;
use frame_resize::{BgraDownscaler, FrameResizeGeometry};

use crate::recording_encoder_caps::{
    compare_parameter_requests, inspect_sink_writer_for_video_codec, probe_for_video_codec,
    AdapterInfo, EncoderCatalog, EncoderKind, EncodingTrial, ParameterRequest, TrialStep,
    VideoCodec, VideoInput,
};
use crate::recording_encoder_policy::{EncoderPlan, EncoderPolicy, FailureCause, FailureStage};
use crate::recording_gpu::GpuEncoderFrames;
use crate::recording_options::{
    validate_region, GopRequest, QualityPlan, RateControl, FPS_CHOICES,
};
use crate::Result;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use windows::{
    core::{Interface, BSTR, HSTRING, PCWSTR},
    Win32::{
        Graphics::{
            Direct3D11::{ID3D11Device, ID3D11Texture2D},
            Dxgi::{IDXGIAdapter1, IDXGIDevice},
        },
        Media::MediaFoundation::*,
        System::Com::{
            CoInitializeEx, CoUninitialize, StructuredStorage::PROPVARIANT, COINIT_MULTITHREADED,
        },
        System::Variant::{VARIANT, VT_BSTR, VT_EMPTY, VT_LPWSTR, VT_NULL},
    },
};

pub(crate) const AUDIO_RATE: u32 = 48_000;
pub(crate) const AUDIO_CHANNELS: u32 = 2;

#[path = "media_video_orientation.rs"]
mod video_orientation;
use video_orientation::VideoOrientation;
#[path = "media_audio_remux.rs"]
mod audio_remux;
pub(crate) use audio_remux::remux_audio_streaming;
#[path = "media_file_reader.rs"]
mod file_reader;
pub(crate) use file_reader::open_file_source_reader;

fn ticks(duration: Duration) -> i64 {
    (duration.as_nanos() / 100).min(i64::MAX as u128) as i64
}

fn duration_from_ticks(value: u64) -> Duration {
    Duration::new(value / 10_000_000, (value % 10_000_000) as u32 * 100)
}

/// 当前线程的 COM（MTA）与 MF 生命周期；必须在使用它的线程创建并释放。
pub(crate) struct MfRuntime {
    com_owned: bool,
}
impl MfRuntime {
    pub(crate) fn new() -> Result<Self> {
        let com_owned = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            if com_owned {
                unsafe { CoUninitialize() };
            }
            return Err(trf!("系统 Media Foundation 不可用（Windows N 版需安装“媒体功能包”）：{error}", "Windows Media Foundation is unavailable (Windows N editions need the Media Feature Pack): {error}"
            )
            .into());
        }
        Ok(Self { com_owned })
    }
}
impl Drop for MfRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com_owned {
                CoUninitialize();
            }
        }
    }
}

fn video_type(
    subtype: &windows::core::GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType> {
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        media.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps.max(1)) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        Ok(media)
    }
}

fn audio_type(subtype: &windows::core::GUID, bytes_per_second: u32) -> Result<IMFMediaType> {
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
        media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
        media.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bytes_per_second)?;
        Ok(media)
    }
}

fn sample(bytes: &[u8], time: Duration, duration: Duration) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(ticks(time))?;
        sample.SetSampleDuration(ticks(duration).max(1))?;
        Ok(sample)
    }
}

/// 编码器实验参数；默认值保持现有产品行为。
/// 经 SetInputMediaType 的 encoding parameters 在编码器开始前下发，并用 ICodecAPI 回读。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EncoderTuning {
    /// eAVEncCommonRateControlMode：0 CBR、1 PeakConstrainedVBR、2 UnconstrainedVBR、3 Quality。
    pub rate_control: Option<u32>,
    /// Quality 模式的质量 0–100。
    pub quality: Option<u32>,
    /// PeakConstrainedVBR 的峰值码率。
    pub max_bitrate: Option<u32>,
    /// 0 最快、100 最好；覆盖产品默认的 0。
    pub quality_vs_speed: Option<u32>,
    /// 覆盖产品默认的低延迟模式（true）。
    pub low_latency: Option<bool>,
    /// 逻辑缓冲大小（bit）；H.264/HEVC 的原生属性使用 byte，须能精确换算。
    pub buffer_bits: Option<u32>,
    /// 沿用现行 GOP、撤回请求或显式最大 picture 数；不保证精确关键帧周期。
    pub gop_request: GopRequest,
}
impl EncoderTuning {
    fn buffer_bytes(self) -> Result<Option<u32>> {
        self.buffer_bits
            .map(|bits| {
                if bits == 0 || !bits.is_multiple_of(8) {
                    return Err(tr!(
                        "编码缓冲位数必须为正的 8 倍数",
                        "The encoding buffer size in bits must be a positive multiple of 8"
                    )
                    .into());
                }
                Ok(bits / 8)
            })
            .transpose()
    }

    fn permits_codec_request(self, codec: VideoCodec) -> bool {
        // 新 codec 仅允许未显式调参的技术路径，不直接套用 H.264 的模式和缓冲请求。
        codec == VideoCodec::H264 || self == Self::default()
    }

    pub(crate) fn from_quality_plan(plan: QualityPlan) -> Self {
        Self {
            rate_control: match plan.parameters.rate_control {
                RateControl::EncoderDefault => None,
                RateControl::PeakConstrainedVbr => Some(1),
            },
            max_bitrate: plan.parameters.max_bitrate_bps,
            quality_vs_speed: plan.parameters.quality_vs_speed,
            low_latency: plan.parameters.low_latency,
            buffer_bits: plan.parameters.buffer_bits,
            gop_request: plan.parameters.gop_request,
            ..Default::default()
        }
    }

    fn parameters(self, bitrate: u32) -> Result<Option<IMFAttributes>> {
        let buffer_bytes = self.buffer_bytes()?;
        // GOP 仅由开始编码前的 ICodecAPI 下发，撤回请求不能生成空 encoding parameters。
        if (Self {
            gop_request: GopRequest::ProductDefault,
            ..self
        }) == Self::default()
        {
            return Ok(None);
        }
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 5)?;
            let attributes = attributes.ok_or("无法创建编码参数")?;
            if let Some(mode) = self.rate_control {
                attributes.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, mode)?;
                attributes.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bitrate)?;
            }
            if let Some(quality) = self.quality {
                attributes.SetUINT32(&CODECAPI_AVEncCommonQuality, quality)?;
            }
            if let Some(max) = self.max_bitrate {
                attributes.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, max)?;
            }
            if let Some(speed) = self.quality_vs_speed {
                attributes.SetUINT32(&CODECAPI_AVEncCommonQualityVsSpeed, speed)?;
            }
            if let Some(low) = self.low_latency {
                attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, u32::from(low))?;
            }
            if let Some(bytes) = buffer_bytes {
                attributes.SetUINT32(&CODECAPI_AVEncCommonBufferSize, bytes)?;
            }
            Ok(Some(attributes))
        }
    }
}

/// 查询实际接入的编码 MFT；硬件允许开关不能当作硬件生效证明。
fn encoder_diagnostics(
    writer: &IMFSinkWriter,
    stream: u32,
    gop_frames: Option<u32>,
    tune: bool,
    tuning: EncoderTuning,
) -> serde_json::Value {
    let mut info = serde_json::json!({"hardware": null, "tuned": tune || gop_frames.is_some()});
    let Ok(writer) = writer.cast::<IMFSinkWriterEx>() else {
        return info;
    };
    unsafe {
        for index in 0..8 {
            let mut category = windows::core::GUID::zeroed();
            let mut transform = None;
            if writer
                .GetTransformForStream(stream, index, Some(&mut category), &mut transform)
                .is_err()
            {
                break;
            }
            if category != MFT_CATEGORY_VIDEO_ENCODER {
                continue;
            }
            let Some(transform) = transform else { continue };
            if let Ok(attributes) = transform.GetAttributes() {
                if let Ok(clsid) = attributes.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) {
                    info["clsid"] = format!("{clsid:?}").into();
                }
                let mut name = [0_u16; 512];
                if attributes
                    .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, None)
                    .is_ok()
                {
                    info["name"] = String::from_utf16_lossy(
                        &name[..name.iter().position(|c| *c == 0).unwrap_or(name.len())],
                    )
                    .into();
                }
                if attributes
                    .GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute)
                    .is_ok_and(|length| length != 0)
                {
                    info["hardware"] = true.into();
                }
            }
            if tune || gop_frames.is_some() {
                if let Ok(codec) = transform.cast::<ICodecAPI>() {
                    // 实时编码优先吞吐；各可选请求都在 BeginWriting 之前下发。
                    // 驱动可拒绝可选属性，逐项记录真实接收结果，不影响兼容回退。
                    for (name, key, value) in [
                        (
                            "speed",
                            CODECAPI_AVEncCommonQualityVsSpeed,
                            tune.then(|| VARIANT::from(tuning.quality_vs_speed.unwrap_or(0))),
                        ),
                        (
                            "b_frames",
                            CODECAPI_AVEncMPVDefaultBPictureCount,
                            tune.then(|| VARIANT::from(0_u32)),
                        ),
                        (
                            "gop",
                            CODECAPI_AVEncMPVGOPSize,
                            gop_frames.map(VARIANT::from),
                        ),
                        (
                            "low_latency",
                            CODECAPI_AVLowLatencyMode,
                            tune.then(|| VARIANT::from(tuning.low_latency.unwrap_or(true))),
                        ),
                    ] {
                        if let Some(value) = value {
                            info[name] = codec.SetValue(&key, &value).is_ok().into();
                        }
                    }
                }
            }
            if tuning != EncoderTuning::default() {
                if let Ok(codec) = transform.cast::<ICodecAPI>() {
                    // 回读编码器实际采用的值；不支持的键返回错误记为 null。
                    for (name, key) in [
                        ("read_rate_control", CODECAPI_AVEncCommonRateControlMode),
                        ("read_quality", CODECAPI_AVEncCommonQuality),
                        ("read_mean_bitrate", CODECAPI_AVEncCommonMeanBitRate),
                        ("read_quality_vs_speed", CODECAPI_AVEncCommonQualityVsSpeed),
                        ("read_buffer_bytes", CODECAPI_AVEncCommonBufferSize),
                    ] {
                        info[name] = codec
                            .GetValue(&key)
                            .ok()
                            .and_then(|value| u32::try_from(&value).ok())
                            .map_or(serde_json::Value::Null, Into::into);
                    }
                }
            }
            break;
        }
    }
    info
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct EncoderAttemptError {
    pub stage: FailureStage,
    pub cause: FailureCause,
    pub diagnostics: serde_json::Value,
}
impl std::fmt::Display for EncoderAttemptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            FailureCause::Backend { message } | FailureCause::Output { message } => {
                formatter.write_str(message)
            }
            FailureCause::PolicyRejected { rejection } => {
                std::fmt::Display::fmt(rejection, formatter)
            }
            FailureCause::Cancelled => formatter.write_str(tr!("录制已取消", "Recording canceled")),
        }
    }
}
impl std::error::Error for EncoderAttemptError {}

#[derive(Debug)]
struct MfWriteError {
    context: &'static str,
    source: windows::core::Error,
}
impl std::fmt::Display for MfWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}：{}", self.context, self.source)
    }
}
impl std::error::Error for MfWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// 保留原生错误链后区分已知文件系统故障，不能把磁盘/权限错误当编码器不兼容反复重试。
pub(crate) fn classify_recording_error(error: &(dyn std::error::Error + 'static)) -> FailureCause {
    let mut source = Some(error);
    let mut output = false;
    for _ in 0..16 {
        let Some(current) = source else { break };
        let code = if let Some(error) = current.downcast_ref::<windows::core::Error>() {
            // COM 存储流也可报告空间不足；它不属于 HRESULT_FROM_WIN32 facility。
            output |= error.code() == windows::Win32::Foundation::STG_E_MEDIUMFULL;
            let value = error.code().0 as u32;
            (value & 0xffff_0000 == 0x8007_0000).then_some(value & 0xffff)
        } else {
            current
                .downcast_ref::<std::io::Error>()
                .and_then(|error| error.raw_os_error())
                .map(|code| code as u32)
        };
        output |= code
            .is_some_and(|code| matches!(code, 2 | 3 | 5 | 19 | 21 | 32 | 39 | 112 | 123 | 206));
        source = current.source();
    }
    let message = error.to_string();
    if output {
        FailureCause::Output { message }
    } else {
        FailureCause::Backend { message }
    }
}

/// 每个录制请求复用一个会话；非 Auto 只在这里探测一次目录，不在各次尝试重新枚举。
/// 调用线程仍须持有 MfRuntime。只执行单次计划，不自行换文件或回放首帧。
pub(crate) struct EncoderBackendSession {
    plan: EncoderPlan,
    audio: bool,
    codec: VideoCodec,
    catalog: Option<EncoderCatalog>,
    catalog_error: Option<String>,
}
impl EncoderBackendSession {
    pub(crate) fn new(plan: EncoderPlan, audio: bool) -> Self {
        Self::new_for_codec(plan, audio, VideoCodec::H264)
    }

    /// codec 在整轮计划中固定；只供独立试编码，不能把失败样本跨 codec 重放。
    pub(crate) fn new_for_codec(plan: EncoderPlan, audio: bool, codec: VideoCodec) -> Self {
        let (catalog, catalog_error) = if plan.policy() == EncoderPolicy::Auto {
            (None, None)
        } else {
            match probe_for_video_codec(codec) {
                Ok(catalog) => (Some(catalog), None),
                Err(error) => (None, Some(error.to_string())),
            }
        };
        Self {
            plan,
            audio,
            codec,
            catalog,
            catalog_error,
        }
    }

    pub(crate) fn plan(&self) -> &EncoderPlan {
        &self.plan
    }

    /// path 必须是调用方独占的新暂存；拒绝已存在的路径，发布与清理由会话事务负责。
    pub(crate) fn create_attempt(
        &self,
        path: &Path,
        index: usize,
        device: Option<&ID3D11Device>,
    ) -> std::result::Result<Mp4Writer, EncoderAttemptError> {
        self.create_attempt_for_source(path, index, device, None)
    }

    /// 独立缩放技术入口；计划宽高是目标，源范围已裁偶数，像素不预先改变物理坐标。
    /// 只创建指定尝试，不在此重试或改写源帧；旧入口保持同尺寸行为。
    pub(crate) fn create_scaled_attempt(
        &self,
        path: &Path,
        index: usize,
        source_width: u32,
        source_height: u32,
        device: Option<&ID3D11Device>,
    ) -> std::result::Result<Mp4Writer, EncoderAttemptError> {
        self.create_attempt_for_source(path, index, device, Some((source_width, source_height)))
    }

    fn create_attempt_for_source(
        &self,
        path: &Path,
        index: usize,
        device: Option<&ID3D11Device>,
        source_size: Option<(u32, u32)>,
    ) -> std::result::Result<Mp4Writer, EncoderAttemptError> {
        let quality = self.plan.quality();
        let mut trace = serde_json::json!({
            "policy": self.plan.policy(), "quality": quality, "attempt_index": index,
            "codec": self.codec,
            "catalog_error": self.catalog_error,
        });
        let Some(&attempt) = self.plan.attempts().get(index) else {
            return Err(EncoderAttemptError {
                stage: FailureStage::InputInitialization,
                cause: FailureCause::Backend {
                    message: "编码尝试编号不在计划内".into(),
                },
                diagnostics: trace,
            });
        };
        if let Some((width, height)) = source_size {
            trace["source_coded_size"] = serde_json::json!([width, height]);
        }
        let geometry = source_size
            .map(|(width, height)| {
                FrameResizeGeometry::new(width, height, quality.width, quality.height)
            })
            .transpose()
            .map_err(|error| EncoderAttemptError {
                stage: FailureStage::InputInitialization,
                cause: FailureCause::Backend {
                    message: error.to_string(),
                },
                diagnostics: trace.clone(),
            })?;
        if let Some(geometry) = geometry {
            trace["frame_resize"] = serde_json::json!(geometry);
        }
        let valid_size = validate_region(quality.width, quality.height);
        let valid_parameters = quality.width.is_multiple_of(2)
            && quality.height.is_multiple_of(2)
            && FPS_CHOICES.contains(&quality.fps)
            && quality.bitrate_bps != 0
            && quality
                .parameters
                .quality_vs_speed
                .is_none_or(|speed| speed <= 100)
            && quality
                .parameters
                .buffer_bits
                .is_none_or(|bits| bits != 0 && bits.is_multiple_of(8))
            && EncoderTuning::from_quality_plan(quality).permits_codec_request(self.codec)
            && quality
                .parameters
                .gop_request
                .requested_frames(quality.fps, true)
                .is_ok()
            && quality
                .parameters
                .max_bitrate_bps
                .is_none_or(|max| max >= quality.bitrate_bps)
            && (quality.parameters.rate_control != RateControl::PeakConstrainedVbr
                || quality.parameters.max_bitrate_bps.is_some());
        if valid_size.is_err() || !valid_parameters {
            return Err(EncoderAttemptError {
                stage: FailureStage::InputInitialization,
                cause: FailureCause::Backend {
                    message: valid_size.err().map_or_else(
                        || {
                            tr!("录屏编码参数无效", "Invalid recording encoder settings")
                                .to_string()
                        },
                        |error| error.to_string(),
                    ),
                },
                diagnostics: trace,
            });
        }
        let path_error = match std::fs::symlink_metadata(path) {
            Ok(_) => Some(
                tr!(
                    "编码尝试的暂存路径已存在",
                    "The encoder attempt path already exists"
                )
                .to_string(),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(error.to_string()),
        };
        if let Some(message) = path_error {
            return Err(EncoderAttemptError {
                stage: FailureStage::WriterCreation,
                cause: FailureCause::Output { message },
                diagnostics: trace,
            });
        }
        let downscaler = geometry
            .filter(|geometry| !geometry.is_identity() && attempt.input == VideoInput::CpuRgb32)
            .map(BgraDownscaler::new)
            .transpose()
            .map_err(|error| EncoderAttemptError {
                stage: FailureStage::InputInitialization,
                cause: FailureCause::Backend {
                    message: error.to_string(),
                },
                diagnostics: trace.clone(),
            })?;
        let gpu = match attempt.input {
            VideoInput::CpuRgb32 => Ok(None),
            VideoInput::GpuNv12 => match (geometry, device) {
                (Some(geometry), Some(device)) => {
                    GpuEncoderFrames::new_resized_with_device(geometry, quality.fps, device)
                }
                (Some(geometry), None) => GpuEncoderFrames::new_resized(geometry, quality.fps),
                (None, Some(device)) => GpuEncoderFrames::new_with_device(
                    quality.width,
                    quality.height,
                    quality.fps,
                    device,
                ),
                (None, None) => GpuEncoderFrames::new(quality.width, quality.height, quality.fps),
            }
            .map(Some),
        }
        .map_err(|error| EncoderAttemptError {
            stage: FailureStage::InputInitialization,
            cause: FailureCause::Backend {
                message: error.to_string(),
            },
            diagnostics: trace.clone(),
        })?;
        let mut request = WriterRequest {
            hardware_transforms: attempt.allow_hardware_transforms(),
            catalog: self.catalog.as_ref(),
            codec: self.codec,
            ..WriterRequest::automatic()
        };
        let created = Mp4Writer::create_for_request(
            path,
            quality.width,
            quality.height,
            quality.fps,
            quality.bitrate_bps,
            self.audio,
            gpu,
            false,
            EncoderTuning::from_quality_plan(quality),
            &mut request,
        );
        let mut writer = created.map_err(|error| {
            let message = error.to_string();
            let mut diagnostics = request
                .diagnostics
                .take()
                .unwrap_or_else(|| serde_json::json!({}));
            let mut trial = EncodingTrial::default();
            match request.stage {
                FailureStage::MediaTypes => {
                    trial.media_types = TrialStep::Failed {
                        error: message.clone(),
                    }
                }
                FailureStage::BeginWriting => {
                    trial.media_types = TrialStep::Succeeded;
                    trial.begin_writing = TrialStep::Failed {
                        error: message.clone(),
                    };
                }
                _ => {}
            }
            diagnostics["encoding_trial"] =
                serde_json::to_value(trial).unwrap_or(serde_json::Value::Null);
            diagnostics["encoder_attempt"] = trace.clone();
            EncoderAttemptError {
                stage: request.stage,
                cause: if request.output_failure {
                    FailureCause::Output { message }
                } else {
                    classify_recording_error(error.as_ref())
                },
                diagnostics,
            }
        })?;
        writer.downscaler = downscaler;
        writer.diagnostics["encoder_attempt"] = trace;
        writer.diagnostics["encoder_attempt"]["input"] =
            serde_json::to_value(attempt.input).unwrap_or(serde_json::Value::Null);
        if self.codec != VideoCodec::H264
            && writer
                .trial
                .inspection
                .as_ref()
                .and_then(|inspection| inspection.video.codec_matches_expected())
                != Some(true)
        {
            // 新 codec 必须先观测到实际编码输出；请求标签或目录命中均不够。
            return Err(reject_writer_observation(
                writer,
                FailureCause::Backend {
                    message: tr!(
                        "未确认请求编码的实际输出类型",
                        "The requested encoder output type could not be confirmed"
                    )
                    .into(),
                },
            ));
        }
        let actual = writer
            .trial
            .inspection
            .as_ref()
            .and_then(|inspection| inspection.video.encoder.as_ref())
            .map_or(EncoderKind::Unknown, |encoder| encoder.kind);
        if let Err(rejection) = attempt.check(actual) {
            // 未交样本就拒绝；空尝试即使收尾成功也不具有可录制/发布资格。
            return Err(reject_writer_observation(
                writer,
                FailureCause::PolicyRejected { rejection },
            ));
        }
        Ok(writer)
    }
}

/// 收尾发现的输出故障必须阻止重建；其余收尾错误保留调用方的原始失败分类。
pub(crate) fn recording_failure_after_finalize(
    rejection: FailureCause,
    finalized: &Result<()>,
) -> FailureCause {
    if let Err(error) = finalized {
        if let output @ FailureCause::Output { .. } = classify_recording_error(error.as_ref()) {
            return output;
        }
    }
    rejection
}

fn reject_writer_observation(writer: Mp4Writer, rejection: FailureCause) -> EncoderAttemptError {
    let (finished, mut diagnostics) = writer.finish_with_diagnostics();
    diagnostics["encoder_observation_rejection"] =
        serde_json::to_value(&rejection).unwrap_or(serde_json::Value::Null);
    let cause = recording_failure_after_finalize(rejection, &finished);
    diagnostics["rejected_attempt_finalize_error"] =
        finished.err().map(|error| error.to_string()).into();
    EncoderAttemptError {
        stage: FailureStage::EncoderObservation,
        cause,
        diagnostics,
    }
}

#[cfg(test)]
mod encoder_observation_tests {
    use super::*;
    use crate::recording_encoder_policy::{
        AttemptSequence, EncoderRequirement, FailureDecision, ObservationRejection, RetryContext,
        StopReason,
    };

    #[test]
    fn recording_encoder_buffer_bits_convert_exactly_to_native_bytes() {
        assert_eq!(EncoderTuning::default().buffer_bytes().unwrap(), None);
        for (bits, bytes) in [
            (8, 1),
            (16, 2),
            (4_000_000, 500_000),
            (160_000_000, 20_000_000),
        ] {
            let tuning = EncoderTuning {
                buffer_bits: Some(bits),
                ..Default::default()
            };
            assert_eq!(tuning.buffer_bytes().unwrap(), Some(bytes));
        }
        for bits in [0, 1, 7, 9, u32::MAX] {
            assert!(EncoderTuning {
                buffer_bits: Some(bits),
                ..Default::default()
            }
            .buffer_bytes()
            .is_err());
        }
    }

    #[test]
    fn recording_encoder_hevc_rejects_h264_advanced_requests() {
        assert!(EncoderTuning::default().permits_codec_request(VideoCodec::Hevc));
        assert!(EncoderTuning::default().permits_codec_request(VideoCodec::Av1));
        for tuning in [
            EncoderTuning {
                rate_control: Some(1),
                ..Default::default()
            },
            EncoderTuning {
                quality: Some(50),
                ..Default::default()
            },
            EncoderTuning {
                max_bitrate: Some(2_000_000),
                ..Default::default()
            },
            EncoderTuning {
                quality_vs_speed: Some(0),
                ..Default::default()
            },
            EncoderTuning {
                low_latency: Some(false),
                ..Default::default()
            },
            EncoderTuning {
                buffer_bits: Some(8),
                ..Default::default()
            },
            EncoderTuning {
                gop_request: GopRequest::EncoderDefault,
                ..Default::default()
            },
            EncoderTuning {
                gop_request: GopRequest::Frames(6),
                ..Default::default()
            },
        ] {
            assert!(tuning.permits_codec_request(VideoCodec::H264));
            assert!(!tuning.permits_codec_request(VideoCodec::Hevc));
            assert!(!tuning.permits_codec_request(VideoCodec::Av1));
        }
    }

    #[test]
    fn recording_encoder_gop_only_request_does_not_create_empty_encoding_attributes() {
        for gop_request in [GopRequest::EncoderDefault, GopRequest::Frames(6)] {
            let tuning = EncoderTuning {
                gop_request,
                ..Default::default()
            };
            // 该请求只交 ICodecAPI；这里不得进入创建 MF 属性的原生分支。
            assert!(tuning.parameters(1_000_000).unwrap().is_none());
        }
    }

    #[test]
    fn recording_encoder_observation_output_finalize_prevents_retry() {
        let quality = crate::recording_options::quality_plan(
            384,
            256,
            30,
            crate::recording_options::QualityProfile::Current(
                crate::recording_options::Preset::CompactMp4,
            ),
        )
        .unwrap();
        for rejection in [
            FailureCause::Backend {
                message: "unconfirmed codec".into(),
            },
            FailureCause::PolicyRejected {
                rejection: ObservationRejection::Unknown {
                    required: EncoderRequirement::Hardware,
                },
            },
        ] {
            for full in [
                Err(std::io::Error::from_raw_os_error(112).into()),
                Err(MfWriteError {
                    context: "test storage finalization",
                    source: windows::core::Error::from_hresult(
                        windows::Win32::Foundation::STG_E_MEDIUMFULL,
                    ),
                }
                .into()),
            ] {
                let cause = recording_failure_after_finalize(rejection.clone(), &full);
                let mut sequence = AttemptSequence::new(EncoderPlan::new(
                    EncoderPolicy::HardwarePreferred,
                    quality,
                ));
                assert!(matches!(
                    sequence.failed(
                        FailureStage::EncoderObservation,
                        cause,
                        RetryContext::default()
                    ),
                    FailureDecision::Stop {
                        reason: StopReason::OutputFailure,
                        ..
                    }
                ));
            }
            assert_eq!(
                recording_failure_after_finalize(rejection.clone(), &Ok(())),
                rejection
            );
            let driver = Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE).into());
            assert_eq!(
                recording_failure_after_finalize(rejection.clone(), &driver),
                rejection
            );
        }
    }
}

struct WriterRequest<'a> {
    hardware_transforms: bool,
    catalog: Option<&'a EncoderCatalog>,
    codec: VideoCodec,
    stage: FailureStage,
    output_failure: bool,
    diagnostics: Option<serde_json::Value>,
}
impl WriterRequest<'_> {
    fn automatic() -> Self {
        Self {
            hardware_transforms: true,
            catalog: None,
            codec: VideoCodec::H264,
            stage: FailureStage::InputInitialization,
            output_failure: false,
            diagnostics: None,
        }
    }
}

/// 已转换但尚未交给 Sink Writer 的视频样本；只在创建它的写入器上消费一次。
/// 持有原 allocator sample，不复制 BGRA，也不改动已经提交的样本时间。
pub(crate) struct PreparedVideoSample {
    sample: IMFSample,
    owner: IMFSinkWriter,
    stream: u32,
}

/// 边录边码的 MP4 写入器。宽高必须为偶数；源帧按 top-down BGRA 行写入。
pub(crate) struct Mp4Writer {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    width: u32,
    height: u32,
    frame: Vec<u8>,
    downscaler: Option<BgraDownscaler>,
    gpu: Option<GpuEncoderFrames>,
    diagnostics: serde_json::Value,
    trial: EncodingTrial,
    finished: bool,
    #[cfg(test)]
    test_finalize_calls: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
    #[cfg(test)]
    test_finalize_callback: Option<IMFSinkWriterCallback>,
}
impl Mp4Writer {
    pub(crate) fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
    ) -> Result<Self> {
        Self::new_for_device(path, width, height, fps, bitrate, audio, None)
    }

    pub(crate) fn new_for_device(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
        device: Option<&ID3D11Device>,
    ) -> Result<Self> {
        let gpu = match device {
            Some(device) => GpuEncoderFrames::new_with_device(width, height, fps, device),
            None => GpuEncoderFrames::new(width, height, fps),
        };
        let tuning = EncoderTuning::default();
        let gpu_error = match gpu.and_then(|gpu| {
            Self::create(
                path,
                width,
                height,
                fps,
                bitrate,
                audio,
                Some(gpu),
                false,
                tuning,
            )
        }) {
            Ok(writer) => return Ok(writer),
            Err(error) => error.to_string(),
        };
        let mut writer = Self::create(
            path, width, height, fps, bitrate, audio, None, false, tuning,
        )?;
        writer.diagnostics["gpu_fallback"] = gpu_error.into();
        Ok(writer)
    }

    /// 仅后台比较：保留本轮修改前的 RGB32、默认限流及编码配置。
    pub(crate) fn new_legacy_for_benchmark(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
    ) -> Result<Self> {
        Self::create(
            path,
            width,
            height,
            fps,
            bitrate,
            audio,
            None,
            true,
            EncoderTuning::default(),
        )
    }

    /// 编解码实验：与产品相同的 GPU NV12 输入路径，只换码率控制参数。
    #[cfg(test)]
    pub(crate) fn new_tuned_for_lab(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        tuning: EncoderTuning,
    ) -> Result<Self> {
        let gpu = GpuEncoderFrames::new(width, height, fps)?;
        Self::create(
            path,
            width,
            height,
            fps,
            bitrate,
            false,
            Some(gpu),
            false,
            tuning,
        )
    }

    /// 自有文件的原生故障检查；仍由真实 RGB32/H.264 Sink Writer 写入。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn new_with_test_byte_stream(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        stream: &IMFByteStream,
        finalize_calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) -> Result<Self> {
        let mut writer = Self::create_for_request_with_stream(
            path,
            width,
            height,
            fps,
            bitrate,
            false,
            None,
            false,
            EncoderTuning::default(),
            &mut WriterRequest::automatic(),
            Some(stream),
            None,
        )?;
        writer.test_finalize_calls = Some(finalize_calls);
        Ok(writer)
    }

    /// 独立异步收尾试验：完成状态由回调报告，不经过同步finish或隐式重试。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn new_with_test_async_byte_stream(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        stream: &IMFByteStream,
        finalize_calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
        callback: &IMFSinkWriterCallback,
    ) -> Result<Self> {
        let mut writer = Self::create_for_request_with_stream(
            path,
            width,
            height,
            fps,
            bitrate,
            false,
            None,
            false,
            EncoderTuning::default(),
            &mut WriterRequest::automatic(),
            Some(stream),
            Some(callback),
        )?;
        writer.test_finalize_calls = Some(finalize_calls);
        Ok(writer)
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
        gpu: Option<GpuEncoderFrames>,
        legacy: bool,
        tuning: EncoderTuning,
    ) -> Result<Self> {
        Self::create_for_request(
            path,
            width,
            height,
            fps,
            bitrate,
            audio,
            gpu,
            legacy,
            tuning,
            &mut WriterRequest::automatic(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_for_request(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
        gpu: Option<GpuEncoderFrames>,
        legacy: bool,
        tuning: EncoderTuning,
        request: &mut WriterRequest<'_>,
    ) -> Result<Self> {
        Self::create_for_request_with_stream(
            path,
            width,
            height,
            fps,
            bitrate,
            audio,
            gpu,
            legacy,
            tuning,
            request,
            None,
            #[cfg(test)]
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_for_request_with_stream(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        audio: bool,
        gpu: Option<GpuEncoderFrames>,
        legacy: bool,
        tuning: EncoderTuning,
        request: &mut WriterRequest<'_>,
        stream: Option<&IMFByteStream>,
        #[cfg(test)] callback: Option<&IMFSinkWriterCallback>,
    ) -> Result<Self> {
        let buffer_bytes = tuning.buffer_bytes()?;
        let codec_api_tuning = !legacy && request.codec != VideoCodec::Av1;
        let gop_frames = tuning.gop_request.requested_frames(fps, codec_api_tuning)?;
        if !tuning.permits_codec_request(request.codec) {
            return Err(tr!(
                "此编码的试验暂不接受自定义编码参数",
                "This codec trial does not accept custom encoder parameters yet"
            )
            .into());
        }
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(tr!(
                "视频编码尺寸必须为不小于 2 的偶数",
                "Video encoding dimensions must be even and at least 2"
            )
            .into());
        }
        unsafe {
            request.stage = FailureStage::WriterCreation;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 4)?;
            let attributes = attributes.ok_or("无法创建编码属性")?;
            attributes.SetUINT32(
                &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                u32::from(request.hardware_transforms),
            )?;
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            #[cfg(test)]
            if let Some(callback) = callback {
                attributes.SetUnknown(&MF_SINK_WRITER_ASYNC_CALLBACK, callback)?;
            }
            if let Some(gpu) = &gpu {
                attributes.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, gpu.manager())?;
                // 实时时钟由录屏调度；allocator最多6帧，仍保留有界背压。
                attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            }
            let url = HSTRING::from(path.as_os_str());
            let writer = MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), stream, &attributes)
                .map_err(|e| {
                    request.output_failure = true;
                    MfWriteError {
                        context: "无法创建 MP4 文件",
                        source: e,
                    }
                })?;
            request.stage = FailureStage::MediaTypes;
            let output = video_type(&request.codec.subtype(), width, height, fps)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            match request.codec {
                VideoCodec::H264 => {
                    output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
                }
                VideoCodec::Hevc => {
                    output
                        .SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
                }
                // 首轮 AV1 协商不显式请求 profile/level；回读类型才是实际格式证据。
                VideoCodec::Av1 => {}
            }
            if gpu.is_some() {
                output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
                output.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
                output.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            }
            let video = writer.AddStream(&output)?;
            let subtype = if gpu.is_some() {
                &MFVideoFormat_NV12
            } else {
                &MFVideoFormat_RGB32
            };
            let input = video_type(subtype, width, height, fps)?;
            // RGB 类型默认 bottom-up；正 stride 声明 top-down，避免逐帧翻转。
            input.SetUINT32(
                &MF_MT_DEFAULT_STRIDE,
                width * if gpu.is_some() { 1 } else { 4 },
            )?;
            input.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            if gpu.is_some() {
                input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
                input.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
                input.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            }
            let parameters = tuning.parameters(bitrate)?;
            writer
                .SetInputMediaType(video, &input, parameters.as_ref())
                .map_err(|source| MfWriteError {
                    context: match request.codec {
                        VideoCodec::H264 => tr!(
                            "系统 H.264 编码器不接受此尺寸或帧率",
                            "The system H.264 encoder rejected these dimensions or frame rate"
                        ),
                        VideoCodec::Hevc => tr!(
                            "系统 HEVC 编码器不接受此尺寸或帧率",
                            "The system HEVC encoder rejected these dimensions or frame rate"
                        ),
                        VideoCodec::Av1 => tr!(
                            "系统 AV1 编码器不接受此尺寸或帧率",
                            "The system AV1 encoder rejected these dimensions or frame rate"
                        ),
                    },
                    source,
                })?;
            let audio = if audio {
                // 微软 AAC 编码器只接受 16 位 PCM；24000 B/s 即 192 kbps。
                let output = audio_type(&MFAudioFormat_AAC, 24_000)?;
                let stream = writer.AddStream(&output)?;
                let input = audio_type(&MFAudioFormat_PCM, AUDIO_RATE * AUDIO_CHANNELS * 2)?;
                input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, AUDIO_CHANNELS * 2)?;
                writer
                    .SetInputMediaType(stream, &input, None)
                    .map_err(|source| MfWriteError {
                        context: "系统 AAC 编码器不可用",
                        source,
                    })?;
                Some(stream)
            } else {
                None
            };
            // AV1 未验证这些 CodecAPI 请求的语义，保留编码器默认值并记录未请求。
            let mut diagnostics =
                encoder_diagnostics(&writer, video, gop_frames, codec_api_tuning, tuning);
            diagnostics["input"] = if gpu.is_some() {
                "gpu_nv12"
            } else {
                "cpu_rgb32"
            }
            .into();
            diagnostics["output_request"] = serde_json::json!({
                "codec": request.codec, "width": width, "height": height, "fps": fps,
                "avg_bitrate_bps": bitrate,
                "profile": match request.codec {
                    VideoCodec::H264 => Some("h264_high"),
                    VideoCodec::Hevc => Some("hevc_main_420_8"),
                    VideoCodec::Av1 => None,
                },
                "level": null,
            });
            diagnostics["buffer_request"] = serde_json::json!({
                "logical_bits": tuning.buffer_bits, "native_bytes": buffer_bytes,
                "native_property_unit": (request.codec != VideoCodec::Av1).then_some("bytes"),
                "conversion": (request.codec != VideoCodec::Av1).then_some("exact_divide_by_8"),
            });
            diagnostics["gop_request"] = serde_json::json!({
                "mode": tuning.gop_request, "requested_frames": gop_frames, "unit": "frames",
                "scope": "requested maximum picture count only; Set success and CodecAPI readback do not prove an exact keyframe or IDR interval",
            });
            let input_adapter = if let Some(gpu) = &gpu {
                let adapter: IDXGIAdapter1 =
                    gpu.device().cast::<IDXGIDevice>()?.GetAdapter()?.cast()?;
                let desc = adapter.GetDesc1()?;
                let end = desc
                    .Description
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(desc.Description.len());
                diagnostics["gpu_adapter"] = serde_json::json!({
                    "name": String::from_utf16_lossy(&desc.Description[..end]),
                    "luid": [desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart as i64],
                });
                Some(AdapterInfo {
                    name: Some(String::from_utf16_lossy(&desc.Description[..end])),
                    luid_high: desc.AdapterLuid.HighPart,
                    luid_low: desc.AdapterLuid.LowPart,
                })
            } else {
                None
            };
            request.stage = FailureStage::BeginWriting;
            request.diagnostics = Some(diagnostics.clone());
            writer.BeginWriting()?;
            request.stage = FailureStage::EncoderObservation;
            // 只读取本次 writer 的实际链，不在录制启动或逐帧路径枚举系统目录。
            let inspection = inspect_sink_writer_for_video_codec(
                &writer,
                video,
                audio,
                if gpu.is_some() {
                    VideoInput::GpuNv12
                } else {
                    VideoInput::CpuRgb32
                },
                input_adapter,
                request.catalog,
                request.codec,
            );
            diagnostics["observed_video_codec_matches_request"] =
                inspection.video.codec_matches_expected().into();
            // 输出媒体类型与 CodecAPI 是两条观测通道；只有同一属性的显式请求才比较。
            diagnostics["legacy_tuning_field_semantics"] = serde_json::json!({
                "tuned": "tuning_requested", "speed": "set_call_succeeded", "b_frames": "set_call_succeeded",
                "gop": "set_call_succeeded", "low_latency": "set_call_succeeded", "phase": "before_begin_writing",
            });
            let mut requests = vec![];
            for (key, value) in [
                ("rate_control", tuning.rate_control),
                ("mean_bitrate", tuning.rate_control.map(|_| bitrate)),
                ("max_bitrate", tuning.max_bitrate),
                ("quality", tuning.quality),
                (
                    if request.codec == VideoCodec::Av1 {
                        "buffer_size_native"
                    } else {
                        "buffer_bytes"
                    },
                    buffer_bytes,
                ),
            ] {
                requests.push(ParameterRequest {
                    key,
                    value,
                    source: "encoding_parameters",
                    set_succeeded: None,
                });
            }
            for (key, legacy_key, value, fallback) in [
                (
                    "quality_vs_speed",
                    "speed",
                    tuning.quality_vs_speed.unwrap_or(0),
                    tuning.quality_vs_speed,
                ),
                (
                    "low_latency",
                    "low_latency",
                    u32::from(tuning.low_latency.unwrap_or(true)),
                    tuning.low_latency.map(u32::from),
                ),
                ("b_frames", "b_frames", 0, None),
            ] {
                requests.push(ParameterRequest {
                    key,
                    value: if codec_api_tuning {
                        Some(value)
                    } else {
                        fallback
                    },
                    source: if !codec_api_tuning {
                        "encoding_parameters"
                    } else {
                        "codec_api_before_begin_writing"
                    },
                    set_succeeded: diagnostics
                        .get(legacy_key)
                        .and_then(serde_json::Value::as_bool),
                });
            }
            requests.push(ParameterRequest {
                key: "gop_frames",
                value: gop_frames,
                source: match tuning.gop_request {
                    GopRequest::EncoderDefault => "explicit_request_withdrawal",
                    GopRequest::ProductDefault if !codec_api_tuning => {
                        "product_default_not_requested"
                    }
                    _ => "codec_api_before_begin_writing",
                },
                set_succeeded: diagnostics.get("gop").and_then(serde_json::Value::as_bool),
            });
            diagnostics["parameter_checks"] = serde_json::to_value(compare_parameter_requests(
                &inspection.video.parameters,
                requests,
            ))
            .unwrap_or(serde_json::Value::Null);
            let encoder = inspection.video.encoder.as_ref();
            diagnostics["hardware"] = match encoder.map(|encoder| encoder.kind) {
                Some(EncoderKind::Hardware) => true.into(),
                Some(EncoderKind::Software) => false.into(),
                _ => serde_json::Value::Null,
            };
            diagnostics["clsid"] = encoder.and_then(|encoder| encoder.clsid.clone()).into();
            diagnostics["name"] = encoder.and_then(|encoder| encoder.name.clone()).into();
            let trial = EncodingTrial {
                inspection: Some(inspection),
                media_types: TrialStep::Succeeded,
                begin_writing: TrialStep::Succeeded,
                ..Default::default()
            };
            let mut result = Self {
                writer,
                video,
                audio,
                width,
                height,
                frame: Vec::new(),
                downscaler: None,
                gpu,
                diagnostics,
                trial,
                finished: {
                    #[cfg(test)]
                    {
                        callback.is_some()
                    }
                    #[cfg(not(test))]
                    {
                        false
                    }
                },
                #[cfg(test)]
                test_finalize_calls: None,
                #[cfg(test)]
                test_finalize_callback: callback.cloned(),
            };
            result.refresh_trial_diagnostics();
            Ok(result)
        }
    }

    /// 源行可含奇数选区的额外右列；显式缩放入口先按源范围采样，旧入口只复制编码范围。
    pub(crate) fn write_video(
        &mut self,
        bgra: &[u8],
        source_width: u32,
        time: Duration,
        duration: Duration,
    ) -> Result<()> {
        let result = self.write_video_sample(bgra, source_width, time, duration);
        self.record_video_result(&result);
        result
    }

    fn write_video_sample(
        &mut self,
        bgra: &[u8],
        source_width: u32,
        time: Duration,
        duration: Duration,
    ) -> Result<()> {
        if let Some(gpu) = &mut self.gpu {
            let sample = gpu.frame_from_bgra(bgra, source_width, time, duration)?;
            unsafe { self.writer.WriteSample(self.video, &sample) }.map_err(|source| {
                MfWriteError {
                    context: "GPU 视频编码失败",
                    source,
                }
            })?;
            return Ok(());
        }
        if let Some(downscaler) = &mut self.downscaler {
            let bytes = downscaler.resize(bgra, source_width)?;
            let sample = sample(bytes, time, duration)?;
            unsafe { self.writer.WriteSample(self.video, &sample) }.map_err(|source| MfWriteError {
                context: tr!("缩放视频编码失败", "Resized video encoding failed"),
                source,
            })?;
            return Ok(());
        }
        let row = self.width as usize * 4;
        let source_row = source_width as usize * 4;
        if source_width < self.width || bgra.len() < source_row * self.height as usize {
            return Err("录屏帧尺寸与编码尺寸不一致".into());
        }
        let bytes = if source_row == row {
            &bgra[..row * self.height as usize]
        } else {
            self.frame.resize(row * self.height as usize, 0);
            for y in 0..self.height as usize {
                self.frame[y * row..(y + 1) * row]
                    .copy_from_slice(&bgra[y * source_row..y * source_row + row]);
            }
            &self.frame
        };
        let sample = sample(bytes, time, duration)?;
        unsafe { self.writer.WriteSample(self.video, &sample) }.map_err(|source| MfWriteError {
            context: "视频编码失败",
            source,
        })?;
        Ok(())
    }

    pub(crate) fn uses_gpu_input(&self) -> bool {
        self.gpu.is_some()
    }

    pub(crate) fn diagnostics(&self) -> &serde_json::Value {
        &self.diagnostics
    }

    pub(crate) fn write_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        crop: crate::geometry::Rect,
        time: Duration,
        duration: Duration,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            let gpu = self.gpu.as_mut().ok_or("当前编码器不支持GPU输入")?;
            let sample = gpu.frame_from_texture(texture, crop, time, duration)?;
            unsafe { self.writer.WriteSample(self.video, &sample) }.map_err(|source| {
                MfWriteError {
                    context: "GPU 视频编码失败",
                    source,
                }
            })?;
            Ok(())
        })();
        self.record_video_result(&result);
        result
    }

    /// 复用原 GPU 裁剪/转换，延后到下一画面或停止边界才确定样本 duration。
    /// 准备不等于接受视频；占用现有固定 allocator 池中的一个样本。
    pub(crate) fn prepare_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        crop: crate::geometry::Rect,
    ) -> Result<PreparedVideoSample> {
        let gpu = self.gpu.as_mut().ok_or("当前编码器不支持GPU输入")?;
        let sample =
            gpu.frame_from_texture(texture, crop, Duration::ZERO, Duration::from_nanos(100))?;
        Ok(PreparedVideoSample {
            sample,
            owner: self.writer.clone(),
            stream: self.video,
        })
    }

    /// 消费尚未提交的样本；最终时间在 WriteSample 前设置，接受后不再修改或重放。
    pub(crate) fn write_prepared_video(
        &mut self,
        prepared: PreparedVideoSample,
        time: Duration,
        duration: Duration,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            if prepared.owner.as_raw() != self.writer.as_raw() || prepared.stream != self.video {
                return Err("延后视频样本不属于当前编码器".into());
            }
            if duration.is_zero() || time.as_nanos() % 100 != 0 || duration.as_nanos() % 100 != 0 {
                return Err("延后视频样本必须使用正的 100ns 对齐跨度".into());
            }
            let time_ticks = i64::try_from(time.as_nanos() / 100)?;
            let duration_ticks = i64::try_from(duration.as_nanos() / 100)?;
            time_ticks
                .checked_add(duration_ticks)
                .ok_or("延后视频样本终点超出媒体时间范围")?;
            unsafe {
                prepared.sample.SetSampleTime(time_ticks)?;
                prepared.sample.SetSampleDuration(duration_ticks)?;
                self.writer.WriteSample(self.video, &prepared.sample)
            }
            .map_err(|source| MfWriteError {
                context: "GPU 视频编码失败",
                source,
            })?;
            Ok(())
        })();
        self.record_video_result(&result);
        result
    }

    /// 交错立体声 16 位 PCM，时间从成片零点起算。
    pub(crate) fn write_audio(&mut self, pcm: &[i16], time: Duration) -> Result<()> {
        let Some(stream) = self.audio else {
            return Ok(());
        };
        if pcm.is_empty() {
            return Ok(());
        }
        let result = (|| -> Result<()> {
            let frames = pcm.len() as u64 / u64::from(AUDIO_CHANNELS);
            let duration = Duration::from_nanos(frames * 1_000_000_000 / u64::from(AUDIO_RATE));
            let bytes =
                unsafe { std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), pcm.len() * 2) };
            let sample = sample(bytes, time, duration)?;
            unsafe { self.writer.WriteSample(stream, &sample) }.map_err(|source| MfWriteError {
                context: "音频编码失败",
                source,
            })?;
            Ok(())
        })();
        if self.trial.first_audio_sample.record_first(&result) {
            self.refresh_trial_diagnostics();
        }
        result
    }

    fn record_video_result(&mut self, result: &Result<()>) {
        if self.trial.first_video_sample.record_first(result) {
            self.refresh_trial_diagnostics();
        }
    }

    fn refresh_trial_diagnostics(&mut self) {
        // 只在初始化、首样本和收尾时序列化，不增加逐帧诊断分配。
        self.diagnostics["encoding_trial"] = serde_json::to_value(&self.trial)
            .unwrap_or_else(|error| serde_json::json!({"error": error.to_string()}));
    }

    pub(crate) fn finish(self) -> Result<()> {
        self.finish_with_diagnostics().0
    }

    /// 返回收尾后的快照，包括 Finalize 失败；普通 diagnostics() 的旧快照不会预测未来结果。
    /// 收尾成功仍不代表已经重新读取/播放验证，reopen 保留 NotAttempted。
    pub(crate) fn finish_with_diagnostics(mut self) -> (Result<()>, serde_json::Value) {
        self.finished = true;
        let result: Result<()> = self.finalize_writer().map_err(|source| {
            MfWriteError {
                context: "无法完成 MP4 文件",
                source,
            }
            .into()
        });
        self.trial.finalize = TrialStep::from_result(&result);
        self.refresh_trial_diagnostics();
        (result, std::mem::take(&mut self.diagnostics))
    }

    fn finalize_writer(&self) -> windows::core::Result<()> {
        #[cfg(test)]
        if let Some(calls) = &self.test_finalize_calls {
            calls.fetch_add(1, Ordering::Relaxed);
            eprintln!("MF storage fault native Finalize: begin");
        }
        let result = unsafe { self.writer.Finalize() };
        #[cfg(test)]
        if self.test_finalize_calls.is_some() {
            eprintln!(
                "MF storage fault native Finalize: returned (ok={})",
                result.is_ok()
            );
        }
        result
    }
}
impl Drop for Mp4Writer {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.test_finalize_callback.is_some() {
            return;
        }
        if !self.finished {
            // 异常中止也尽量写出容器，调用方随后决定删除或保留。
            let _ = self.finalize_writer();
        }
    }
}

pub(crate) struct DecodedFrame {
    pub time: Duration,
    pub bgra: Vec<u8>,
}

/// 一次解码只保留一个有时间戳的交错 PCM 块，不缓存整条音轨。
pub(crate) struct DecodedAudio {
    pub time: Duration,
    pub samples: Vec<f32>,
}

/// 原生流描述不代表系统已经具备对应解码器；未知主类型按原始 GUID 保留。
#[derive(Clone, Debug)]
pub(crate) struct MediaStreamInfo {
    pub stream_index: u32,
    pub major_type: windows::core::GUID,
    pub codec: Option<windows::core::GUID>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub language: Option<String>,
    pub name: Option<String>,
}

const MAX_MEDIA_STREAMS: u32 = 128;
const MAX_STREAM_PROBE_ERRORS: usize = 8;
const MAX_STREAM_PROBE_ERROR_CHARS: usize = 512;

#[derive(Clone, Debug)]
pub(crate) struct MediaStreamProbe {
    pub streams: Vec<MediaStreamInfo>,
    /// 已成功探测的流仍可使用；有诊断时不能由空列表推断文件没有其它轨道。
    pub error: Option<String>,
}

#[derive(Default)]
struct StreamProbeDiagnostics {
    messages: Vec<String>,
    omitted: usize,
}
impl StreamProbeDiagnostics {
    fn push(&mut self, message: impl AsRef<str>) {
        if self.messages.len() == MAX_STREAM_PROBE_ERRORS {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        let mut chars = message.as_ref().chars().map(|value| {
            if matches!(value, '\r' | '\n') {
                ' '
            } else {
                value
            }
        });
        let mut line: String = chars
            .by_ref()
            .take(MAX_STREAM_PROBE_ERROR_CHARS - 1)
            .collect();
        if chars.next().is_some() {
            line.push('…');
        }
        self.messages.push(line);
    }

    fn finish(mut self) -> Option<String> {
        if self.omitted != 0 {
            let count = self.omitted;
            self.messages.push(trf!(
                "另有 {count} 项媒体流探测诊断未展开",
                "{count} more media stream probe diagnostics were omitted"
            ));
        }
        (!self.messages.is_empty()).then(|| self.messages.join("\n"))
    }
}

fn media_stream_error(index: u32, error: windows::core::Error) -> Box<dyn std::error::Error> {
    let code = format!("0x{:08X}", error.code().0 as u32);
    if error.code() == MF_E_NO_MORE_TYPES {
        trf!(
            "媒体流 {index} 没有原生格式：{error}（{code}）",
            "Media stream {index} has no native format: {error} ({code})"
        )
        .into()
    } else {
        trf!(
            "无法读取媒体流 {index}：{error}（{code}）",
            "Cannot inspect media stream {index}: {error} ({code})"
        )
        .into()
    }
}

fn optional_stream_attribute<T>(
    index: u32,
    result: windows::core::Result<T>,
    diagnostics: &mut StreamProbeDiagnostics,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => None,
        Err(error) => {
            diagnostics.push(media_stream_error(index, error).to_string());
            None
        }
    }
}

fn stream_text_value(
    index: u32,
    value: PROPVARIANT,
    diagnostics: &mut StreamProbeDiagnostics,
) -> Option<String> {
    if value.vt() == VT_EMPTY || value.vt() == VT_NULL {
        return None;
    }
    // PropVariantToBSTR 会把数值等类型也转成文字，流名称/语言只接受实际字符串属性。
    if value.vt() != VT_BSTR && value.vt() != VT_LPWSTR {
        let kind = value.vt().0;
        diagnostics.push(trf!(
            "媒体流 {index} 的文字属性类型无效（{kind}）",
            "Media stream {index} has an invalid text attribute type ({kind})"
        ));
        return None;
    }
    // PROPVARIANT 与 BSTR 均由绑定释放；返回文字不借用媒体源持有的内存。
    let text = match BSTR::try_from(&value) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(media_stream_error(index, error).to_string());
            return None;
        }
    };
    if text.len() > 4096 {
        diagnostics.push(trf!(
            "媒体流 {index} 的描述文字超过限制",
            "Media stream {index} metadata text exceeds the limit"
        ));
        return None;
    }
    (!text.is_empty()).then(|| text.to_string())
}

fn stream_text(
    reader: &IMFSourceReader,
    index: u32,
    key: &windows::core::GUID,
    diagnostics: &mut StreamProbeDiagnostics,
) -> Option<String> {
    let value = optional_stream_attribute(
        index,
        unsafe { reader.GetPresentationAttribute(index, key) },
        diagnostics,
    )?;
    stream_text_value(index, value, diagnostics)
}

fn bounded_media_streams(
    mut read: impl FnMut(u32, &mut StreamProbeDiagnostics) -> Result<Option<MediaStreamInfo>>,
) -> MediaStreamProbe {
    let mut streams = Vec::new();
    let mut diagnostics = StreamProbeDiagnostics::default();
    // 多看一个索引，恰好 128 路与确实超过上限可以明确区分。
    for index in 0..=MAX_MEDIA_STREAMS {
        let stream = match read(index, &mut diagnostics) {
            Ok(None) => break,
            Ok(stream) => stream,
            Err(error) => {
                diagnostics.push(error.to_string());
                None
            }
        };
        if index == MAX_MEDIA_STREAMS {
            diagnostics.push(trf!(
                "媒体流探测达到 {MAX_MEDIA_STREAMS} 路上限，列表可能不完整",
                "Media stream probe reached its {MAX_MEDIA_STREAMS}-stream limit; the list may be incomplete"
            ));
            break;
        }
        if let Some(stream) = stream {
            streams.push(stream);
        }
    }
    MediaStreamProbe {
        streams,
        error: diagnostics.finish(),
    }
}

/// 只查询原生类型和流描述属性，不协商 PCM、不读取 sample，也不创建其它轨道的解码器。
/// 单次系统查询无法中断；本函数以索引数和诊断长度限制总探测范围。
pub(crate) fn enumerate_media_streams(reader: &IMFSourceReader) -> MediaStreamProbe {
    bounded_media_streams(|index, diagnostics| unsafe {
        let native = match reader.GetNativeMediaType(index, 0) {
            Ok(native) => native,
            Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => return Ok(None),
            Err(error) => return Err(media_stream_error(index, error)),
        };
        let major_type = native
            .GetGUID(&MF_MT_MAJOR_TYPE)
            .map_err(|error| media_stream_error(index, error))?;
        let audio = major_type == MFMediaType_Audio;
        Ok(Some(MediaStreamInfo {
            stream_index: index,
            major_type,
            codec: optional_stream_attribute(index, native.GetGUID(&MF_MT_SUBTYPE), diagnostics),
            sample_rate: if audio {
                optional_stream_attribute(
                    index,
                    native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND),
                    diagnostics,
                )
            } else {
                None
            },
            channels: if audio {
                optional_stream_attribute(
                    index,
                    native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS),
                    diagnostics,
                )
            } else {
                None
            },
            language: stream_text(reader, index, &MF_SD_LANGUAGE, diagnostics),
            name: stream_text(reader, index, &MF_SD_STREAM_NAME, diagnostics),
        }))
    })
}

pub(crate) struct AudioReader {
    reader: IMFSourceReader,
    stream_index: u32,
    pub rate: u32,
    pub channels: usize,
    bits: u32,
    float: bool,
}

impl AudioReader {
    /// 无音轨与无法解码分别返回 None 和错误；调用线程持有 MfRuntime。
    pub(crate) fn open(path: &Path) -> Result<Option<Self>> {
        Self::open_first(path, false)
    }

    /// 预览只消费单声道或立体声；多声道由系统按源声道布局混合，避免丢失中置对白。
    pub(crate) fn open_for_preview(path: &Path) -> Result<Option<Self>> {
        Self::open_first(path, true)
    }

    fn open_first(path: &Path, preview: bool) -> Result<Option<Self>> {
        unsafe {
            let reader = Self::source(path)?;
            let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
            let native = match reader.GetNativeMediaType(stream, 0) {
                Ok(native) => native,
                Err(error) if error.code() == MF_E_INVALIDSTREAMNUMBER => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            Self::from_reader(reader, stream, native, preview).map(Some)
        }
    }

    /// 只接受真实的零起始流索引；显式选中的无效流或非音频流不会伪装成“没有声音”。
    pub(crate) fn open_stream(path: &Path, stream_index: u32) -> Result<Self> {
        Self::open_selected(path, stream_index, false)
    }

    pub(crate) fn open_stream_for_preview(path: &Path, stream_index: u32) -> Result<Self> {
        Self::open_selected(path, stream_index, true)
    }

    fn open_selected(path: &Path, stream_index: u32, preview: bool) -> Result<Self> {
        if stream_index >= MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32 {
            return Err(tr!("音轨流索引无效", "Invalid audio stream index").into());
        }
        let reader = Self::source(path)?;
        let native = unsafe { reader.GetNativeMediaType(stream_index, 0) }
            .map_err(|error| media_stream_error(stream_index, error))?;
        Self::from_reader(reader, stream_index, native, preview)
    }

    fn source(path: &Path) -> Result<IMFSourceReader> {
        open_file_source_reader(path, None).map_err(|error| {
            trf!(
                "无法打开媒体声音：{error}",
                "Cannot open media audio: {error}"
            )
            .into()
        })
    }

    fn from_reader(
        reader: IMFSourceReader,
        stream: u32,
        native: IMFMediaType,
        preview: bool,
    ) -> Result<Self> {
        unsafe {
            if native.GetGUID(&MF_MT_MAJOR_TYPE)? != MFMediaType_Audio {
                return Err(trf!(
                    "所选媒体流 {stream} 不是音频",
                    "Selected media stream {stream} is not audio"
                )
                .into());
            }
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            // 先取实际解码格式；部分类型由 Source Reader 补全源采样率和声道数。
            let wanted = MFCreateMediaType()?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)?;
            if reader.SetCurrentMediaType(stream, None, &wanted).is_err() {
                wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                wanted.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                reader
                    .SetCurrentMediaType(stream, None, &wanted)
                    .map_err(|error| {
                        trf!(
                            "系统无法解码此音轨：{error}",
                            "Cannot decode this audio track: {error}"
                        )
                    })?;
            }
            let mut media = reader.GetCurrentMediaType(stream)?;
            let source_rate = media.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
            let source_channels = media.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)?;
            if source_rate == 0 || source_rate > 384_000 || !(1..=32).contains(&source_channels) {
                return Err(tr!("音轨解码格式不受支持", "Unsupported decoded audio format").into());
            }
            if preview && source_channels > 2 {
                // 只下混多声道；单声道仍由预览复制到左右，避免增声道时右声道补零。
                let wanted = Self::stereo_type(source_rate, true)?;
                if reader.SetCurrentMediaType(stream, None, &wanted).is_err() {
                    let wanted = Self::stereo_type(source_rate, false)?;
                    reader
                        .SetCurrentMediaType(stream, None, &wanted)
                        .map_err(|error| {
                            trf!(
                                "系统无法将此音轨混合为立体声：{error}",
                                "Cannot mix this audio track to stereo: {error}"
                            )
                        })?;
                }
                media = reader.GetCurrentMediaType(stream)?;
                if media.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? != 2
                    || media.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)? != source_rate
                {
                    return Err(tr!(
                        "系统未返回所需的立体声格式",
                        "The system did not return the requested stereo format"
                    )
                    .into());
                }
            }
            let rate = media.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)?;
            let channels = media.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? as usize;
            let bits = media.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE)?;
            let subtype = media.GetGUID(&MF_MT_SUBTYPE)?;
            let float = subtype == MFAudioFormat_Float;
            if rate == 0
                || rate > 384_000
                || channels == 0
                || channels > 32
                || !((float && bits == 32) || (subtype == MFAudioFormat_PCM && bits == 16))
            {
                return Err(tr!("音轨解码格式不受支持", "Unsupported decoded audio format").into());
            }
            Ok(Self {
                reader,
                stream_index: stream,
                rate,
                channels,
                bits,
                float,
            })
        }
    }

    fn stereo_type(rate: u32, float: bool) -> windows::core::Result<IMFMediaType> {
        unsafe {
            let media = MFCreateMediaType()?;
            let bits = if float { 32 } else { 16 };
            let alignment = 2 * (bits / 8);
            media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            media.SetGUID(
                &MF_MT_SUBTYPE,
                if float {
                    &MFAudioFormat_Float
                } else {
                    &MFAudioFormat_PCM
                },
            )?;
            media.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            media.SetUINT32(&MF_MT_AUDIO_CHANNEL_MASK, 3)?;
            media.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            media.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits)?;
            media.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, alignment)?;
            media.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * alignment)?;
            media.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            Ok(media)
        }
    }

    pub(crate) fn seek(&mut self, time: Duration) -> Result<()> {
        let position = PROPVARIANT::from(ticks(time));
        unsafe {
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &position)?;
        }
        Ok(())
    }

    pub(crate) fn next(&mut self) -> Result<Option<DecodedAudio>> {
        let stream = self.stream_index;
        // 损坏文件不能靠无限个空 sample 占住输出线程。
        for _ in 0..64 {
            let mut flags = 0;
            let mut timestamp = 0_i64;
            let mut sample = None;
            unsafe {
                self.reader.ReadSample(
                    stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )?;
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(tr!("音轨解码失败", "Audio decoding failed").into());
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                let media = unsafe { self.reader.GetCurrentMediaType(stream)? };
                if unsafe { media.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)? } != self.rate
                    || unsafe { media.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)? } as usize
                        != self.channels
                    || unsafe { media.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE)? } != self.bits
                    || unsafe { media.GetGUID(&MF_MT_SUBTYPE)? }
                        != if self.float {
                            MFAudioFormat_Float
                        } else {
                            MFAudioFormat_PCM
                        }
                {
                    return Err(tr!(
                        "音轨中途改变了解码格式",
                        "Audio format changed during playback"
                    )
                    .into());
                }
            }
            if let Some(sample) = sample {
                unsafe {
                    if sample.GetTotalLength()? > 4 * 1024 * 1024 {
                        return Err(
                            tr!("音轨数据块过大", "Audio block exceeds the buffer limit").into(),
                        );
                    }
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let length = buffer.GetCurrentLength()? as usize;
                    // 拒绝异常的单块分配；普通 AAC 块远小于此上限。
                    if length > 4 * 1024 * 1024
                        || length % (self.channels * (self.bits as usize / 8)) != 0
                    {
                        return Err(tr!(
                            "音轨数据块无效或过大",
                            "Invalid or oversized audio block"
                        )
                        .into());
                    }
                    if length == 0 {
                        continue;
                    }
                    let mut data = std::ptr::null_mut();
                    buffer.Lock(&mut data, None, None)?;
                    let bytes = std::slice::from_raw_parts(data, length);
                    let samples = if self.float {
                        bytes
                            .chunks_exact(4)
                            .map(|v| {
                                let sample = f32::from_le_bytes([v[0], v[1], v[2], v[3]]);
                                if sample.is_finite() {
                                    sample
                                } else {
                                    0.
                                }
                            })
                            .collect()
                    } else {
                        bytes
                            .chunks_exact(2)
                            .map(|v| f32::from(i16::from_le_bytes([v[0], v[1]])) / 32768.)
                            .collect()
                    };
                    buffer.Unlock()?;
                    return Ok(Some(DecodedAudio {
                        time: duration_from_ticks(timestamp.max(0) as u64),
                        samples,
                    }));
                }
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
        }
        Err(tr!("音轨连续返回空数据", "Audio decoder returned no samples").into())
    }
}

/// 用新的混音替换 MP4 音轨：视频压缩数据原样拷贝（不重新编码），音频按 PCM 重新编码 AAC。
/// 输出写到 target；调用方负责原子替换原文件。
pub(crate) fn remux_audio(source: &Path, target: &Path, pcm: &[i16]) -> Result<()> {
    let mut blocks = pcm.chunks((AUDIO_RATE / 2 * AUDIO_CHANNELS) as usize);
    remux_audio_streaming(
        source,
        target,
        || Ok(blocks.next().map(<[i16]>::to_vec)),
        || true,
    )
}

/// 解码输出的几何：编码面（含宏块对齐填充）与实际显示区域。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DecodedLayout {
    pub coded: (u32, u32),
    pub origin: (u32, u32),
    pub width: u32,
    pub height: u32,
}
impl DecodedLayout {
    /// H.264 解码面按 16 对齐，显示区域取最小显示孔径；缺失时视为整面。
    unsafe fn from_type(media: &IMFMediaType) -> Result<Self> {
        let size = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE)? };
        let coded = ((size >> 32) as u32, size as u32);
        if coded.0 == 0 || coded.1 == 0 || u64::from(coded.0) * u64::from(coded.1) > 64_000_000 {
            return Err("录屏文件尺寸无效".into());
        }
        let mut area = MFVideoArea::default();
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                (&mut area as *mut MFVideoArea).cast::<u8>(),
                size_of::<MFVideoArea>(),
            )
        };
        let aperture =
            unsafe { media.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None) }.is_ok();
        Ok(Self::new(coded, aperture.then_some(area)))
    }

    pub(crate) fn new(coded: (u32, u32), aperture: Option<MFVideoArea>) -> Self {
        let full = Self {
            coded,
            origin: (0, 0),
            width: coded.0,
            height: coded.1,
        };
        let Some(area) = aperture else {
            return full;
        };
        let x = area.OffsetX.value.max(0) as u32;
        let y = area.OffsetY.value.max(0) as u32;
        let w = area.Area.cx.max(0) as u32;
        let h = area.Area.cy.max(0) as u32;
        if w == 0 || h == 0 || x + w > coded.0 || y + h > coded.1 {
            return full;
        }
        Self {
            coded,
            origin: (x, y),
            width: w,
            height: h,
        }
    }

    /// 按真实行跨度复制显示区域；pitch 为负表示 bottom-up，越界行保持黑色。
    pub(crate) fn copy(&self, source: &[u8], first_row: usize, pitch: isize) -> Vec<u8> {
        let row = self.width as usize * 4;
        let mut bgra = vec![0_u8; row * self.height as usize];
        for y in 0..self.height as usize {
            let line = (self.origin.1 as usize + y) as isize;
            let start = first_row as isize + line * pitch + self.origin.0 as isize * 4;
            if start < 0 {
                continue;
            }
            let start = start as usize;
            if let Some(src) = source.get(start..start + row) {
                bgra[y * row..(y + 1) * row].copy_from_slice(src);
            }
        }
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        bgra
    }
}

/// MP4 解码为 top-down BGRA，供框内预览和 GIF 转换使用。
#[derive(Clone, Debug)]
pub(crate) struct VideoInfo {
    /// 原始压缩视频格式；能力查询不能把像素输出格式当成视频编码。
    pub codec: windows::core::GUID,
    pub frame_rate: Option<(u32, u32)>,
    /// 容器提供的原始旋转，未提供时为零；VideoReader 输出像素已纠正，消费者不再旋转。
    pub rotation_degrees: u32,
    /// 仅表示媒体源报告的流；列表完整性由 stream_probe_error 单独表示。
    pub streams: Vec<MediaStreamInfo>,
    pub stream_probe_error: Option<String>,
}

pub(crate) struct VideoReader {
    reader: IMFSourceReader,
    layout: DecodedLayout,
    orientation: VideoOrientation,
    pub width: u32,
    pub height: u32,
    pub duration: Duration,
    pub info: VideoInfo,
    _device: Option<ID3D11Device>,
    _manager: Option<IMFDXGIDeviceManager>,
    gpu_frames: u64,
    processing: &'static str,
}
impl VideoReader {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        // 旧VIDEO_PROCESSING仅软件转换，官方明确未针对实时播放优化。
        // 预览/GIF当前消费CPU BGRA。三路同素材实测XVP无需应用D3D设备最快，
        // 避免GPU解码后再同步读回及重复初始化；真正纹理呈现接通后再接DXVA分支。
        Self::open_impl(path, false, false).or_else(|_| Self::open_impl(path, true, false))
    }

    pub(crate) fn open_legacy_for_benchmark(path: &Path) -> Result<Self> {
        Self::open_impl(path, true, false)
    }

    /// 编解码实验：D3D11 硬件解码 + 显卡视频处理器转 RGB，对比预览默认的软件 XVP。
    #[cfg(test)]
    pub(crate) fn open_gpu_for_lab(path: &Path) -> Result<Self> {
        Self::open_impl(path, false, true)
    }

    fn open_impl(path: &Path, legacy: bool, gpu: bool) -> Result<Self> {
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 3)?;
            let attributes = attributes.ok_or("无法创建解码属性")?;
            let (device, manager) = if legacy {
                attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
                (None, None)
            } else if gpu {
                let device = crate::recording_gpu::create_video_device()?;
                let mut token = 0;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.ok_or("无法创建解码设备管理器")?;
                manager.ResetDevice(&device, token)?;
                attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
                attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
                attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
                (Some(device), Some(manager))
            } else {
                // XVP保留BT.709/601与range语义，不用旧的简化软件转换器。
                attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
                (None, None)
            };
            let reader = open_file_source_reader(path, Some(&attributes))
                .map_err(|e| format!("无法打开录屏文件：{e}"))?;
            // 坏轨和坏属性保留诊断，已经探测到的其它流仍可供界面选择。
            let stream_probe = enumerate_media_streams(&reader);
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            let native = reader.GetNativeMediaType(stream, 0)?;
            let frame_rate = native.GetUINT64(&MF_MT_FRAME_RATE).ok().and_then(|value| {
                let (numerator, denominator) = ((value >> 32) as u32, value as u32);
                (numerator > 0 && denominator > 0).then_some((numerator, denominator))
            });
            let info = VideoInfo {
                codec: native.GetGUID(&MF_MT_SUBTYPE)?,
                frame_rate,
                rotation_degrees: native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0),
                streams: stream_probe.streams,
                stream_probe_error: stream_probe.error,
            };
            let wanted = MFCreateMediaType()?;
            wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            if info.rotation_degrees != 0 {
                // 输出保持原始几何，旋转在明确关闭处理器纠正后只做一次。
                wanted.SetUINT64(&MF_MT_FRAME_SIZE, native.GetUINT64(&MF_MT_FRAME_SIZE)?)?;
            }
            reader
                .SetCurrentMediaType(stream, None, &wanted)
                .map_err(|e| format!("系统无法解码此视频：{e}"))?;
            let orientation = VideoOrientation::prepare(&reader, info.rotation_degrees, !legacy)?;
            let layout = DecodedLayout::from_type(&reader.GetCurrentMediaType(stream)?)?;
            let (width, height) = orientation.size(layout.width, layout.height)?;
            let duration = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .and_then(|value| u64::try_from(&value).ok())
                .map_or(Duration::ZERO, duration_from_ticks);
            Ok(Self {
                reader,
                width,
                height,
                layout,
                orientation,
                duration,
                info,
                _device: device,
                _manager: manager,
                gpu_frames: 0,
                processing: if legacy {
                    "legacy_software_rgb"
                } else if gpu {
                    "dxva_xvp"
                } else {
                    "xvp_no_device"
                },
            })
        }
    }

    pub(crate) fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({
            "processing":self.processing, "gpu_output_frames":self.gpu_frames,
            "source_rotation_degrees":self.orientation.raw_degrees(),
            "orientation": "decoder_bgra_normalized", "display_size": [self.width, self.height],
            "frame_rate_conversion_disabled": self.orientation.frame_rate_conversion_disabled(),
        })
    }

    pub(crate) fn seek(&mut self, time: Duration) -> Result<()> {
        let position = PROPVARIANT::from(ticks(time));
        unsafe {
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &position)?;
        }
        Ok(())
    }

    /// 返回下一帧；None 表示结束。
    pub(crate) fn next(&mut self) -> Result<Option<DecodedFrame>> {
        self.next_at(Duration::ZERO)
    }

    /// 顺序解码到输出时刻；被降帧率丢弃的帧不再复制完整 BGRA 到 CPU。
    /// 保留 H.264 的参考帧解码顺序，不在帧间反复 seek。
    pub(crate) fn next_at(&mut self, minimum: Duration) -> Result<Option<DecodedFrame>> {
        self.next_impl(minimum, None)
    }

    /// 播放会话关闭后，不继续追赶关键帧或空 sample；不能中断系统内部的单次调用。
    pub(crate) fn next_cancellable(&mut self, stop: &AtomicBool) -> Result<Option<DecodedFrame>> {
        self.next_impl(Duration::ZERO, Some(stop))
    }

    fn next_impl(
        &mut self,
        minimum: Duration,
        stop: Option<&AtomicBool>,
    ) -> Result<Option<DecodedFrame>> {
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut empty_samples = 0_u32;
        loop {
            if stop.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Ok(None);
            }
            let mut flags = 0_u32;
            let mut timestamp = 0_i64;
            let mut sample = None;
            unsafe {
                self.reader.ReadSample(
                    stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )?;
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(tr!("视频解码失败", "Video decoding failed").into());
            }
            if flags & MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32 != 0 {
                return Err(trf!(
                    "暂不支持此视频中途改变源格式（flags={flags:#x}）",
                    "Changes to the source video format are not supported (flags={flags:#x})"
                )
                .into());
            }
            if flags & MF_SOURCE_READERF_ALLEFFECTSREMOVED.0 as u32 != 0 {
                // 已返出的 sample 不受后续配置追溯影响；新链须重开并在流送前配置。
                return Err(trf!(
                    "视频处理链已移除，无法保持方向和时序（flags={flags:#x}）",
                    "The video processing chain was removed; orientation and timing cannot be preserved (flags={flags:#x})"
                )
                .into());
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                let media = unsafe { self.reader.GetCurrentMediaType(stream)? };
                self.orientation
                    .validate_current_type(&self.reader, &media)
                    .map_err(|error| {
                        trf!(
                            "视频输出协商无效（flags={flags:#x}）：{error}",
                            "Invalid video output negotiation (flags={flags:#x}): {error}"
                        )
                    })?;
                let layout = unsafe { DecodedLayout::from_type(&media)? };
                let display_size = self.orientation.size(layout.width, layout.height)?;
                if display_size != (self.width, self.height) {
                    return Err(trf!(
                        "视频输出尺寸变化（flags={flags:#x}）：expected={}x{}, actual={}x{}",
                        "Video output size changed (flags={flags:#x}): expected={}x{}, actual={}x{}",
                        self.width, self.height, display_size.0, display_size.1
                    ).into());
                }
                self.layout = layout;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(None);
            }
            let Some(sample) = sample else {
                empty_samples += 1;
                if empty_samples >= 64 {
                    return Err(
                        tr!("视频连续返回空数据", "Video decoder returned no samples").into(),
                    );
                }
                continue;
            };
            empty_samples = 0;
            let time = duration_from_ticks(timestamp.max(0) as u64);
            if time + Duration::from_millis(1) < minimum {
                continue;
            }
            if unsafe { sample.GetBufferByIndex(0) }
                .is_ok_and(|buffer| buffer.cast::<IMFDXGIBuffer>().is_ok())
            {
                self.gpu_frames += 1;
            }
            let bgra = unsafe { self.read_sample(&sample)? };
            let bgra = self
                .orientation
                .apply(bgra, self.layout.width, self.layout.height)?;
            return Ok(Some(DecodedFrame { time, bgra }));
        }
    }

    /// 解码面按宏块对齐，行跨度大于显示宽度；优先用 IMF2DBuffer 的实际 pitch，
    /// 不支持时按缓冲长度 / 解码面行数推算。
    unsafe fn read_sample(&self, sample: &IMFSample) -> Result<Vec<u8>> {
        let buffer = if unsafe { sample.GetBufferCount()? } == 1 {
            unsafe { sample.GetBufferByIndex(0)? }
        } else {
            unsafe { sample.ConvertToContiguousBuffer()? }
        };
        let rows = self.layout.coded.1 as usize;
        if let Ok(surface) = buffer.cast::<IMF2DBuffer2>() {
            let mut scan0 = std::ptr::null_mut();
            let mut base = std::ptr::null_mut();
            let mut pitch = 0_i32;
            let mut length = 0_u32;
            if unsafe {
                surface.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scan0,
                    &mut pitch,
                    &mut base,
                    &mut length,
                )
            }
            .is_ok()
            {
                // 只读锁避免将未修改的CPU内容写回GPU；使用系统返回的实际边界。
                let first_row = (scan0 as usize).checked_sub(base as usize);
                let result = if pitch != 0
                    && !base.is_null()
                    && first_row.is_some_and(|offset| offset < length as usize)
                {
                    let source = unsafe { std::slice::from_raw_parts(base, length as usize) };
                    Ok(self.layout.copy(source, first_row.unwrap(), pitch as isize))
                } else {
                    Err("解码帧的二维缓冲无效".into())
                };
                unsafe { surface.Unlock2D()? };
                return result;
            }
        }
        if let Ok(surface) = buffer.cast::<IMF2DBuffer>() {
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0_i32;
            if unsafe { surface.Lock2D(&mut scan0, &mut pitch) }.is_ok() {
                if pitch == 0 || scan0.is_null() {
                    unsafe { surface.Unlock2D()? };
                    return Err("解码帧的二维缓冲无效".into());
                }
                let span = pitch.unsigned_abs() as usize * rows;
                // 负 pitch 时 scan0 指向最后一个内存行（显示第一行）。
                let (base, first_row) = if pitch < 0 {
                    let base = unsafe { scan0.sub(pitch.unsigned_abs() as usize * (rows - 1)) };
                    (base, pitch.unsigned_abs() as usize * (rows - 1))
                } else {
                    (scan0, 0)
                };
                let source = unsafe { std::slice::from_raw_parts(base, span) };
                let bgra = self.layout.copy(source, first_row, pitch as isize);
                unsafe { surface.Unlock2D()? };
                return Ok(bgra);
            }
        }
        let mut data = std::ptr::null_mut();
        let mut length = 0_u32;
        unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
        let length = length as usize;
        let minimum = self.layout.coded.0 as usize * 4;
        let pitch = if rows > 0 && length.is_multiple_of(rows) && length / rows >= minimum {
            length / rows
        } else {
            minimum
        };
        let source = unsafe { std::slice::from_raw_parts(data, length) };
        let bgra = self.layout.copy(source, 0, pitch as isize);
        unsafe { buffer.Unlock()? };
        Ok(bgra)
    }
}

#[cfg(test)]
#[path = "preview_audio_channel_probe.rs"]
mod channel_probe;

#[cfg(test)]
#[path = "media_video_orientation_probe.rs"]
mod orientation_probe;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_timestamp_conversion_preserves_full_native_tick_range() {
        assert_eq!(duration_from_ticks(0), Duration::ZERO);
        assert_eq!(duration_from_ticks(10_000_001), Duration::new(1, 100));
        assert_eq!(ticks(duration_from_ticks(i64::MAX as u64)), i64::MAX);
        let maximum = duration_from_ticks(u64::MAX);
        assert_eq!(maximum.as_secs(), u64::MAX / 10_000_000);
        assert_eq!(maximum.subsec_nanos(), (u64::MAX % 10_000_000) as u32 * 100);
    }

    fn described_stream(index: u32) -> MediaStreamInfo {
        MediaStreamInfo {
            stream_index: index,
            major_type: MFMediaType_Audio,
            codec: Some(MFAudioFormat_AAC),
            sample_rate: Some(48_000),
            channels: Some(2),
            language: None,
            name: None,
        }
    }

    #[test]
    fn media_stream_probe_retains_real_indices_and_unknown_types() {
        let unknown = windows::core::GUID::from_u128(0x5a33cb8e_6b68_496f_9a71_b1885db4d827);
        let mut visited = Vec::new();
        let probe = bounded_media_streams(|index, _| {
            visited.push(index);
            Ok(match index {
                0 => Some(described_stream(index)),
                1 => Some(MediaStreamInfo {
                    major_type: unknown,
                    codec: None,
                    sample_rate: None,
                    channels: None,
                    ..described_stream(index)
                }),
                _ => None,
            })
        });
        assert_eq!(visited, [0, 1, 2]);
        assert!(probe.error.is_none());
        assert_eq!(probe.streams[0].stream_index, 0);
        assert_eq!(probe.streams[1].stream_index, 1);
        assert_eq!(probe.streams[1].major_type, unknown);
        assert!(probe.streams[1].codec.is_none());
    }

    #[test]
    fn media_stream_probe_distinguishes_exact_limit_and_incomplete_results() {
        let complete = bounded_media_streams(|index, _| {
            Ok((index < MAX_MEDIA_STREAMS).then(|| described_stream(index)))
        });
        assert_eq!(complete.streams.len(), MAX_MEDIA_STREAMS as usize);
        assert!(complete.error.is_none());
        let mut visited = 0;
        let incomplete = bounded_media_streams(|index, _| {
            visited += 1;
            Ok(Some(described_stream(index)))
        });
        assert_eq!(incomplete.streams.len(), MAX_MEDIA_STREAMS as usize);
        assert!(incomplete.error.is_some());
        assert_eq!(visited, MAX_MEDIA_STREAMS + 1);
    }

    #[test]
    fn media_stream_probe_keeps_good_tracks_after_bad_native_or_major_type() {
        let mut visited = Vec::new();
        let probe = bounded_media_streams(|index, _| {
            visited.push(index);
            match index {
                0 | 2 | 4 => Ok(Some(described_stream(index))),
                1 => Err(media_stream_error(
                    index,
                    windows::core::Error::from_hresult(MF_E_NO_MORE_TYPES),
                )),
                3 => Err(media_stream_error(
                    index,
                    windows::core::Error::from_hresult(MF_E_ATTRIBUTENOTFOUND),
                )),
                _ => Ok(None),
            }
        });
        assert_eq!(visited, [0, 1, 2, 3, 4, 5]);
        assert_eq!(
            probe
                .streams
                .iter()
                .map(|stream| stream.stream_index)
                .collect::<Vec<_>>(),
            [0, 2, 4]
        );
        let error = probe.error.unwrap();
        for code in [MF_E_NO_MORE_TYPES, MF_E_ATTRIBUTENOTFOUND] {
            assert!(error.contains(&format!("0x{:08X}", code.0 as u32)));
        }
    }

    #[test]
    fn media_stream_probe_bad_optional_properties_do_not_remove_the_track() {
        let probe = bounded_media_streams(|index, diagnostics| {
            Ok(match index {
                0 => Some(MediaStreamInfo {
                    codec: optional_stream_attribute(
                        index,
                        Err(windows::core::Error::from_hresult(MF_E_INVALIDMEDIATYPE)),
                        diagnostics,
                    ),
                    sample_rate: optional_stream_attribute(
                        index,
                        Err(windows::core::Error::from_hresult(MF_E_ATTRIBUTENOTFOUND)),
                        diagnostics,
                    ),
                    language: stream_text_value(index, PROPVARIANT::from("zh-CN"), diagnostics),
                    name: stream_text_value(index, PROPVARIANT::from(17_i64), diagnostics),
                    ..described_stream(index)
                }),
                1 => Some(described_stream(index)),
                _ => None,
            })
        });
        assert_eq!(probe.streams.len(), 2);
        assert_eq!(probe.streams[0].language.as_deref(), Some("zh-CN"));
        assert!(probe.streams[0].codec.is_none());
        assert!(probe.streams[0].sample_rate.is_none());
        assert!(probe.streams[0].name.is_none());
        assert_eq!(probe.streams[1].stream_index, 1);
        assert_eq!(probe.error.unwrap().lines().count(), 2);
    }

    #[test]
    fn media_stream_probe_diagnostics_bound_unicode_text_and_error_count() {
        assert!(StreamProbeDiagnostics::default().finish().is_none());
        let mut diagnostics = StreamProbeDiagnostics::default();
        let long = "坏属性\n".repeat(MAX_STREAM_PROBE_ERROR_CHARS);
        for _ in 0..MAX_STREAM_PROBE_ERRORS + 3 {
            diagnostics.push(&long);
        }
        assert_eq!(diagnostics.messages.len(), MAX_STREAM_PROBE_ERRORS);
        assert_eq!(diagnostics.omitted, 3);
        let message = diagnostics.finish().unwrap();
        let lines: Vec<_> = message.lines().collect();
        assert_eq!(lines.len(), MAX_STREAM_PROBE_ERRORS + 1);
        for line in &lines[..MAX_STREAM_PROBE_ERRORS] {
            assert!(line.chars().count() <= MAX_STREAM_PROBE_ERROR_CHARS);
            assert!(line.ends_with('…'));
        }
        assert!(lines.last().unwrap().contains('3'));
    }

    #[test]
    fn media_stream_text_is_owned_after_property_release() {
        let text = {
            let value = PROPVARIANT::from("中文音轨 / English");
            BSTR::try_from(&value).unwrap().to_string()
        };
        assert_eq!(text, "中文音轨 / English");
    }

    #[test]
    fn media_stream_empty_optional_text_is_missing_without_a_probe_error() {
        let mut diagnostics = StreamProbeDiagnostics::default();
        assert!(stream_text_value(0, PROPVARIANT::default(), &mut diagnostics).is_none());
        assert!(stream_text_value(0, PROPVARIANT::from(""), &mut diagnostics).is_none());
        assert!(diagnostics.finish().is_none());
    }

    #[test]
    fn media_audio_pcm_reader_uses_timestamps_and_seeks() {
        let _runtime = MfRuntime::new().unwrap();
        let path =
            std::env::temp_dir().join(format!("rustshot-audio-reader-{}.wav", std::process::id()));
        struct RemoveFile(std::path::PathBuf);
        impl Drop for RemoveFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = RemoveFile(path.clone());
        let mut writer = crate::wav_track::WavWriter::create(&path).unwrap();
        let samples = vec![0.25; AUDIO_RATE as usize * AUDIO_CHANNELS as usize * 4];
        writer.write(&samples).unwrap();
        writer.finish().unwrap();
        let mut reader = AudioReader::open(&path).unwrap().unwrap();
        assert_eq!(
            (reader.rate, reader.channels),
            (AUDIO_RATE, AUDIO_CHANNELS as usize)
        );
        let first = reader.next().unwrap().unwrap();
        assert_eq!(first.time, Duration::ZERO);
        assert!(!first.samples.is_empty() && first.samples.len() < samples.len());
        assert!((first.samples[0] - 0.25).abs() < 0.001);
        let target = Duration::from_secs(2);
        reader.seek(target).unwrap();
        let after_seek = reader.next().unwrap().unwrap();
        assert!(after_seek.time >= target.saturating_sub(Duration::from_millis(1)));
        assert!((after_seek.samples[0] - 0.25).abs() < 0.001);
    }

    #[test]
    fn decoded_layout_uses_aperture_and_padded_pitch() {
        let area = MFVideoArea {
            OffsetX: MFOffset { fract: 0, value: 0 },
            OffsetY: MFOffset { fract: 0, value: 0 },
            Area: windows::Win32::Foundation::SIZE { cx: 3, cy: 2 },
        };
        let layout = DecodedLayout::new((4, 3), Some(area));
        assert_eq!((layout.width, layout.height), (3, 2));
        // 解码面 4 像素宽，行跨度 20 字节（含 4 字节填充）。
        let pitch = 20;
        let mut source = vec![0_u8; pitch * 3];
        for y in 0..3 {
            for x in 0..4 {
                source[y * pitch + x * 4] = (y * 10 + x) as u8;
            }
        }
        let out = layout.copy(&source, 0, pitch as isize);
        let firsts: Vec<u8> = out.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(firsts, vec![0, 1, 2, 10, 11, 12]);
        // bottom-up：首个显示行在缓冲末尾。
        let flipped = layout.copy(&source, pitch * 2, -(pitch as isize));
        let firsts: Vec<u8> = flipped.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(firsts, vec![20, 21, 22, 10, 11, 12]);
        assert_eq!(DecodedLayout::new((4, 3), None).width, 4);
    }
}
