//! MF 编码器目录与实际 writer 观测。枚举只证明候选已注册，不证明能录制。
//! 在工作线程调用探测；所有 COM 对象在本线程释放，报告只包含可跨线程的数据。
#[path = "recording_encoder_conditional_caps.rs"]
pub(crate) mod conditional;

use crate::{media_foundation::MfRuntime, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use windows::{
    core::{Interface, GUID, HRESULT},
    Win32::{
        Foundation::{E_NOTIMPL, LUID},
        Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4},
        Media::MediaFoundation::*,
        System::Com::{CoTaskMemFree, IPersist},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Codec {
    H264,
    Hevc,
    Av1,
    Aac,
}
impl Codec {
    fn buffer_size_key(self) -> &'static str {
        match self {
            Self::H264 | Self::Hevc => "buffer_bytes",
            Self::Av1 | Self::Aac => "buffer_size_native",
        }
    }

    fn category(self) -> GUID {
        match self {
            Self::H264 | Self::Hevc | Self::Av1 => MFT_CATEGORY_VIDEO_ENCODER,
            Self::Aac => MFT_CATEGORY_AUDIO_ENCODER,
        }
    }
    fn output(self) -> MFT_REGISTER_TYPE_INFO {
        match self {
            Self::H264 => MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            },
            Self::Hevc => MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_HEVC,
            },
            Self::Av1 => MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_AV1,
            },
            Self::Aac => MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Audio,
                guidSubtype: MFAudioFormat_AAC,
            },
        }
    }
}

/// 实际视频请求与观测使用的类型；AAC 不能传入视频入口，AV1 供独立技术试编码使用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VideoCodec {
    #[default]
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    pub(crate) fn subtype(self) -> GUID {
        self.codec().output().guidSubtype
    }

    fn codec(self) -> Codec {
        match self {
            Self::H264 => Codec::H264,
            Self::Hevc => Codec::Hevc,
            Self::Av1 => Codec::Av1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EncoderKind {
    Hardware,
    Software,
    Unknown,
}

/// 送入 writer 的像素与内存路线，与编码器硬软无关。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VideoInput {
    CpuRgb32,
    GpuNv12,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AdapterInfo {
    pub name: Option<String>,
    pub luid_high: i32,
    pub luid_low: u32,
}

const ENCODER_OBSERVATION_SCOPE: &str = "Same writer stream, at most 32 transforms; only one observed encoder is accepted. The first GetTransformForStream failure ends the scan without classifying its HRESULT. Only an exact 8-byte MFT_ENUM_ADAPTER_LUID blob from that encoder instance is recognized; other representations remain unknown. DXGI naming is optional and no catalog or pixel-input adapter supplies missing evidence.";

fn parse_encoder_adapter_blob(
    kind: MF_ATTRIBUTE_TYPE,
    declared_size: u32,
    bytes: &[u8],
    returned_size: u32,
) -> std::result::Result<AdapterInfo, &'static str> {
    if kind != MF_ATTRIBUTE_BLOB {
        return Err("MFT_ENUM_ADAPTER_LUID is not a blob");
    }
    if declared_size != 8 || bytes.len() != 8 || returned_size != 8 {
        return Err("MFT_ENUM_ADAPTER_LUID must contain exactly 8 bytes");
    }
    Ok(AdapterInfo {
        name: None,
        luid_low: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        luid_high: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    })
}

fn read_encoder_adapter(attributes: &IMFAttributes) -> std::result::Result<AdapterInfo, String> {
    let kind = unsafe { attributes.GetItemType(&MFT_ENUM_ADAPTER_LUID) }
        .map_err(|error| format!("MFT_ENUM_ADAPTER_LUID GetItemType: {error}"))?;
    if kind != MF_ATTRIBUTE_BLOB {
        return Err(format!(
            "MFT_ENUM_ADAPTER_LUID type={} is not blob type={}; representation is unknown",
            kind.0, MF_ATTRIBUTE_BLOB.0
        ));
    }
    let declared_size = unsafe { attributes.GetBlobSize(&MFT_ENUM_ADAPTER_LUID) }
        .map_err(|error| format!("MFT_ENUM_ADAPTER_LUID GetBlobSize: {error}"))?;
    if declared_size != 8 {
        return Err(format!(
            "MFT_ENUM_ADAPTER_LUID declared {declared_size} bytes, expected 8"
        ));
    }
    let mut bytes = [0_u8; 8];
    let mut returned_size = 0;
    unsafe { attributes.GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut bytes, Some(&mut returned_size)) }
        .map_err(|error| format!("MFT_ENUM_ADAPTER_LUID GetBlob: {error}"))?;
    if returned_size != 8 {
        return Err(format!(
            "MFT_ENUM_ADAPTER_LUID GetBlob returned {returned_size} bytes after declaring {declared_size}, expected 8"
        ));
    }
    parse_encoder_adapter_blob(kind, declared_size, &bytes, returned_size).map_err(str::to_owned)
}

fn encoder_adapter_name(adapter: &AdapterInfo) -> std::result::Result<String, String> {
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| format!("Encoder adapter name CreateDXGIFactory1: {error}"))?;
    let selected: IDXGIAdapter1 = unsafe {
        factory.EnumAdapterByLuid(LUID {
            LowPart: adapter.luid_low,
            HighPart: adapter.luid_high,
        })
    }
    .map_err(|error| format!("Encoder adapter name EnumAdapterByLuid: {error}"))?;
    let description = unsafe { selected.GetDesc1() }
        .map_err(|error| format!("Encoder adapter name GetDesc1: {error}"))?;
    if description.AdapterLuid.LowPart != adapter.luid_low
        || description.AdapterLuid.HighPart != adapter.luid_high
    {
        return Err("Encoder adapter name lookup returned a different LUID".into());
    }
    let length = description
        .Description
        .iter()
        .position(|&value| value == 0)
        .unwrap_or(description.Description.len());
    if length == 0 {
        return Err("Encoder adapter description is empty".into());
    }
    Ok(String::from_utf16_lossy(&description.Description[..length]))
}

fn complete_adapter_observation(
    observed: std::result::Result<AdapterInfo, String>,
    name_lookup: impl FnOnce(&AdapterInfo) -> std::result::Result<String, String>,
) -> (Option<AdapterInfo>, Option<String>) {
    let mut adapter = match observed {
        Ok(adapter) => adapter,
        Err(error) => return (None, Some(error)),
    };
    match name_lookup(&adapter) {
        Ok(name) => {
            adapter.name = Some(name);
            (Some(adapter), None)
        }
        Err(error) => (Some(adapter), Some(error)),
    }
}

fn only_observed_encoder<T>(
    mut encoders: Vec<Option<T>>,
    chain_ended: bool,
) -> std::result::Result<T, &'static str> {
    if !chain_ended {
        return Err("Writer transform scan reached its 32-transform limit; encoder is unknown");
    }
    match encoders.len() {
        0 => Err("Encoder transform was not exposed by the writer"),
        1 => encoders
            .pop()
            .flatten()
            .ok_or("Writer encoder transform pointer was absent"),
        _ => Err("Multiple encoder transforms were exposed by the writer; encoder is unknown"),
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EncoderIdentity {
    pub clsid: Option<String>,
    pub name: Option<String>,
    pub hardware_url: Option<String>,
    pub kind: EncoderKind,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CodecEnumeration {
    pub codec: Codec,
    pub kind: EncoderKind,
    pub candidates: Vec<EncoderIdentity>,
    /// None 且候选为空表示这次枚举未发现；错误时不能解释为“不支持”。
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EncoderCatalog {
    pub enumerations: Vec<CodecEnumeration>,
}

/// 初始化失败直接返回错误；每组枚举失败单独保留，避免掩盖其它可用候选。
/// 不激活编码器、不初始化 GPU、不写临时文件；尺寸/帧率/首帧能力需实际试编码。
pub(crate) fn probe_baseline() -> Result<EncoderCatalog> {
    probe_for_video_codec(VideoCodec::H264)
}

/// 只枚举指定视频编码及 AAC；实际视频能力仍须协商、写样本、Finalize 和回读证明。
/// 不从 conditional 目录转换部分结果，避免把取消/截断的一组当成完整分类依据。
pub(crate) fn probe_for_video_codec(video_codec: VideoCodec) -> Result<EncoderCatalog> {
    let _runtime = MfRuntime::new()?;
    Ok(EncoderCatalog {
        enumerations: catalog_codecs(video_codec)
            .into_iter()
            .flat_map(|codec| {
                [EncoderKind::Hardware, EncoderKind::Software]
                    .into_iter()
                    .map(move |kind| enumerate(codec, kind))
            })
            .collect(),
    })
}

fn catalog_codecs(video_codec: VideoCodec) -> [Codec; 2] {
    [video_codec.codec(), Codec::Aac]
}

/// MFTEnumEx 分配的数组既拥有各 COM 引用，也拥有 CoTaskMem 数组内存。
struct ActivationArray {
    ptr: *mut Option<IMFActivate>,
    len: u32,
}
impl Drop for ActivationArray {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                for index in 0..self.len as usize {
                    std::ptr::drop_in_place(self.ptr.add(index));
                }
                CoTaskMemFree(Some(self.ptr.cast()));
            }
        }
    }
}

fn enumerate(codec: Codec, kind: EncoderKind) -> CodecEnumeration {
    let flags = match kind {
        EncoderKind::Hardware => MFT_ENUM_FLAG_HARDWARE,
        EncoderKind::Software => MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT,
        EncoderKind::Unknown => unreachable!(),
    };
    let output = codec.output();
    let mut array = ActivationArray {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    let mut report = CodecEnumeration {
        codec,
        kind,
        candidates: Vec::new(),
        error: None,
    };
    let result = unsafe {
        MFTEnumEx(
            codec.category(),
            flags | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output),
            &mut array.ptr,
            &mut array.len,
        )
    };
    if let Err(error) = result {
        report.error = Some(error.to_string());
    } else if array.len != 0 && array.ptr.is_null() {
        report.error = Some("MFTEnumEx returned a null activation array".into());
    } else {
        for index in 0..array.len as usize {
            if let Some(activation) = unsafe { &*array.ptr.add(index) } {
                report.candidates.push(identity(activation, kind));
            }
        }
    }
    report
}

fn string_attribute(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let length = attributes.GetStringLength(key).ok()?;
        // 元数据不允许引起无界分配；过长或读失败时保留未知。
        if length == 0 || length > 16_384 {
            return None;
        }
        let mut buffer = vec![0; length as usize + 1];
        attributes.GetString(key, &mut buffer, None).ok()?;
        Some(String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

fn identity(attributes: &IMFAttributes, kind: EncoderKind) -> EncoderIdentity {
    EncoderIdentity {
        clsid: unsafe { attributes.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }
            .ok()
            .map(|value| format!("{value:?}")),
        name: string_attribute(attributes, &MFT_FRIENDLY_NAME_Attribute),
        hardware_url: string_attribute(attributes, &MFT_ENUM_HARDWARE_URL_Attribute),
        kind,
    }
}

fn actual_kind(
    encoder: &EncoderIdentity,
    codec: Codec,
    catalog: Option<&EncoderCatalog>,
) -> EncoderKind {
    if encoder.hardware_url.is_some() {
        return EncoderKind::Hardware;
    }
    let (Some(clsid), Some(catalog)) = (&encoder.clsid, catalog) else {
        return EncoderKind::Unknown;
    };
    let mut hardware = false;
    let mut software = false;
    let mut hardware_complete = false;
    let mut software_complete = false;
    for group in &catalog.enumerations {
        if group.codec != codec || group.error.is_some() {
            continue;
        }
        hardware_complete |= group.kind == EncoderKind::Hardware;
        software_complete |= group.kind == EncoderKind::Software;
        if group
            .candidates
            .iter()
            .any(|item| item.clsid.as_ref() == Some(clsid))
        {
            hardware |= group.kind == EncoderKind::Hardware;
            software |= group.kind == EncoderKind::Software;
        }
    }
    // 缺失或失败的一组可能隐藏同 CLSID 的另一种注册，不能凭不完整目录排除。
    if !hardware_complete || !software_complete {
        return EncoderKind::Unknown;
    }
    match (hardware, software) {
        (true, false) => EncoderKind::Hardware,
        (false, true) => EncoderKind::Software,
        _ => EncoderKind::Unknown,
    }
}

/// 实际视频媒体类型的原始属性；缺失或未知值不推断默认色彩或码流效果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct MediaColorimetry {
    pub nominal_range: Option<u32>,
    pub transfer_matrix: Option<u32>,
    pub transfer_function: Option<u32>,
    pub primaries: Option<u32>,
    pub chroma_siting: Option<u32>,
    pub read_errors: BTreeMap<&'static str, String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct MediaTypeInfo {
    pub stream_id: u32,
    pub major_type: Option<String>,
    pub subtype: Option<String>,
    pub size: Option<(u32, u32)>,
    /// 保留有理数；分母为零或属性缺失时为未知。
    pub fps: Option<(u32, u32)>,
    /// 仅实际 major type 为 Video 时读取，不代表已核验压缩码流色彩。
    pub colorimetry: Option<MediaColorimetry>,
    pub bitrate_bps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u32>,
    pub profile: Option<u32>,
    pub level: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ParameterReadback {
    pub key: &'static str,
    /// 原生属性值：H264/HEVC 的 buffer_bytes 为字节；AV1/AAC 的 buffer_size_native 单位未知。
    /// 不在回读层换算成逻辑 bit，也不把 AV1/AAC 原始值作为逻辑缓冲请求的匹配证据。
    pub value: Option<u32>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ParameterRequest {
    pub key: &'static str,
    /// 须与同 key 的回读使用同一单位；buffer_bytes 不接收逻辑 buffer_bits 数值。
    pub value: Option<u32>,
    pub source: &'static str,
    /// 仅表示逐项 CodecAPI SetValue 的返回值；整体媒体类型接受不能冒充逐项成功。
    pub set_succeeded: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ParameterComparison {
    pub request: ParameterRequest,
    pub observed: Option<u32>,
    pub read_error: Option<String>,
    /// 没有同一属性的请求值或回读失败时为未知，不把它们算一致或不一致。
    pub matches_request: Option<bool>,
}

pub(crate) fn compare_parameter_requests(
    observed: &[ParameterReadback],
    requests: Vec<ParameterRequest>,
) -> Vec<ParameterComparison> {
    requests
        .into_iter()
        .map(|request| {
            let read = observed.iter().find(|item| item.key == request.key);
            let value = read
                .filter(|item| item.error.is_none())
                .and_then(|item| item.value);
            ParameterComparison {
                matches_request: request
                    .value
                    .zip(value)
                    .map(|(requested, actual)| requested == actual),
                request,
                observed: value,
                read_error: read.and_then(|item| item.error.clone()),
            }
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StreamInspection {
    pub expected_codec: Codec,
    pub encoder: Option<EncoderIdentity>,
    /// 没有编码器设备证据时为未知；不能复制像素输入设备冒充编码器适配器。
    pub encoder_adapter: Option<AdapterInfo>,
    /// 属性缺失/损坏和可选 DXGI 查名失败独立记录，不改变编码器硬软分类。
    pub encoder_adapter_diagnostic: Option<String>,
    pub encoder_observation_scope: &'static str,
    /// 编码 MFT 端口上的真实类型；可能不同于 writer 的原始 RGB 输入。
    pub inputs: Vec<MediaTypeInfo>,
    pub outputs: Vec<MediaTypeInfo>,
    pub parameters: Vec<ParameterReadback>,
    pub errors: Vec<String>,
    // 未读出的端口不会进入 outputs，不能仅凭已收集类型都匹配就把部分观测称为完整。
    #[serde(skip_serializing)]
    output_types_complete: bool,
}

impl StreamInspection {
    fn empty(expected_codec: Codec) -> Self {
        Self {
            expected_codec,
            encoder: None,
            encoder_adapter: None,
            encoder_adapter_diagnostic: None,
            encoder_observation_scope: ENCODER_OBSERVATION_SCOPE,
            inputs: Vec::new(),
            outputs: Vec::new(),
            parameters: Vec::new(),
            errors: Vec::new(),
            output_types_complete: false,
        }
    }

    /// expected_codec 是请求标签；只有实际编码 MFT 输出类型才能证明 codec 匹配。
    /// major/subtype 已知不匹配返回 false，端口/类型缺失保留未知；不证明首帧或成片成功。
    pub(crate) fn codec_matches_expected(&self) -> Option<bool> {
        if self.outputs.is_empty() {
            return None;
        }
        let expected = self.expected_codec.output();
        let expected_major = format!("{:?}", expected.guidMajorType);
        let expected_subtype = format!("{:?}", expected.guidSubtype);
        let mut missing = false;
        for output in &self.outputs {
            match output.major_type.as_ref() {
                Some(major) if major != &expected_major => return Some(false),
                Some(_) => {}
                None => missing = true,
            }
            match output.subtype.as_ref() {
                Some(subtype) if subtype != &expected_subtype => return Some(false),
                Some(_) => {}
                None => missing = true,
            }
        }
        (!missing && self.output_types_complete).then_some(true)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SinkWriterInspection {
    pub input: VideoInput,
    /// 这是像素输入设备，不能当作实际编码器使用同一适配器的证明。
    pub input_adapter: Option<AdapterInfo>,
    pub video: StreamInspection,
    pub audio: Option<StreamInspection>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum TrialStep {
    #[default]
    NotAttempted,
    Succeeded,
    Failed {
        error: String,
    },
}
impl TrialStep {
    pub(crate) fn from_result<E: std::fmt::Display>(result: &std::result::Result<(), E>) -> Self {
        match result {
            Ok(()) => Self::Succeeded,
            Err(error) => Self::Failed {
                error: error.to_string(),
            },
        }
    }

    /// 首样本只记录第一次实际尝试，后续成功或失败均不能改写第一步证据。
    /// 返回是否改变，调用方可据此避免每帧重新序列化诊断。
    pub(crate) fn record_first<E: std::fmt::Display>(
        &mut self,
        result: &std::result::Result<(), E>,
    ) -> bool {
        if !matches!(self, Self::NotAttempted) {
            return false;
        }
        *self = Self::from_result(result);
        true
    }
}

/// 试编码各阶段独立记录；目录命中和媒体类型协商都不能替代样本/封装/重新播放。
/// 未请求音频时 first_audio_sample 保留 NotAttempted。
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct EncodingTrial {
    pub inspection: Option<SinkWriterInspection>,
    pub media_types: TrialStep,
    pub begin_writing: TrialStep,
    /// 成功仅表示 WriteSample 接受该样本，仍需封装和重新读取验证产物。
    pub first_video_sample: TrialStep,
    pub first_audio_sample: TrialStep,
    pub finalize: TrialStep,
    pub reopen: TrialStep,
}

/// 在 SetInputMediaType/BeginWriting 后从 writer 所属线程观测，不修改编码属性。
/// 此报告不证明首帧、Finalize 或播放成功，调用方应另记各阶段成功/失败。
pub(crate) fn inspect_sink_writer(
    writer: &IMFSinkWriter,
    video_stream: u32,
    audio_stream: Option<u32>,
    input: VideoInput,
    input_adapter: Option<AdapterInfo>,
    catalog: Option<&EncoderCatalog>,
) -> SinkWriterInspection {
    inspect_sink_writer_for_video_codec(
        writer,
        video_stream,
        audio_stream,
        input,
        input_adapter,
        catalog,
        VideoCodec::H264,
    )
}

/// 保留旧 H.264 入口；新入口的视频参数不能传入 AAC，音轨仍固定按 AAC 观测。
pub(crate) fn inspect_sink_writer_for_video_codec(
    writer: &IMFSinkWriter,
    video_stream: u32,
    audio_stream: Option<u32>,
    input: VideoInput,
    input_adapter: Option<AdapterInfo>,
    catalog: Option<&EncoderCatalog>,
    video_codec: VideoCodec,
) -> SinkWriterInspection {
    SinkWriterInspection {
        input,
        input_adapter,
        video: inspect_stream(writer, video_stream, video_codec.codec(), catalog),
        audio: audio_stream.map(|stream| inspect_stream(writer, stream, Codec::Aac, catalog)),
    }
}

fn inspect_stream(
    writer: &IMFSinkWriter,
    stream: u32,
    codec: Codec,
    catalog: Option<&EncoderCatalog>,
) -> StreamInspection {
    let mut report = StreamInspection::empty(codec);
    let result = (|| -> windows::core::Result<()> {
        let writer = writer.cast::<IMFSinkWriterEx>()?;
        // 同一条 writer 链完整有界扫描后才取唯一编码器，不任取多个实例之一。
        let mut encoders = Vec::new();
        let mut chain_ended = false;
        for index in 0..32 {
            let mut category = GUID::zeroed();
            let mut transform = None;
            if unsafe {
                writer.GetTransformForStream(stream, index, Some(&mut category), &mut transform)
            }
            .is_err()
            {
                // API 文档未单列正常链尾 HRESULT；沿用首个失败结束链的观测约定。
                chain_ended = true;
                break;
            }
            if category == codec.category() {
                encoders.push(transform);
            }
        }
        let transform = match only_observed_encoder(encoders, chain_ended) {
            Ok(transform) => transform,
            Err(reason) => {
                report.encoder_adapter_diagnostic = Some(reason.into());
                report.errors.push(reason.into());
                return Ok(());
            }
        };
        let mut encoder = EncoderIdentity {
            clsid: None,
            name: None,
            hardware_url: None,
            kind: EncoderKind::Unknown,
        };
        match unsafe { transform.GetAttributes() } {
            Ok(attributes) => {
                encoder = identity(&attributes, EncoderKind::Unknown);
                (report.encoder_adapter, report.encoder_adapter_diagnostic) =
                    complete_adapter_observation(
                        read_encoder_adapter(&attributes),
                        encoder_adapter_name,
                    );
            }
            Err(error) => {
                report.encoder_adapter_diagnostic = Some(format!("GetAttributes: {error}"));
                report.errors.push(format!("GetAttributes: {error}"));
            }
        }
        if encoder.clsid.is_none() {
            let class_id = transform
                .cast::<IPersist>()
                .and_then(|persist| unsafe { persist.GetClassID() });
            match class_id {
                Ok(clsid) => encoder.clsid = Some(format!("{clsid:?}")),
                Err(error) => report.errors.push(format!("IPersist::GetClassID: {error}")),
            }
        }
        encoder.kind = actual_kind(&encoder, codec, catalog);
        report.encoder = Some(encoder);
        inspect_types(&transform, &mut report);
        match transform.cast::<ICodecAPI>() {
            Ok(api) => report.parameters = read_parameters(&api, codec),
            Err(error) => report.errors.push(format!("ICodecAPI: {error}")),
        }
        Ok(())
    })();
    if let Err(error) = result {
        report
            .errors
            .push(format!("GetTransformForStream: {error}"));
    }
    report
}

fn inspect_types(transform: &IMFTransform, report: &mut StreamInspection) {
    let result = (|| -> windows::core::Result<()> {
        let (mut inputs, mut outputs) = (0, 0);
        unsafe { transform.GetStreamCount(&mut inputs, &mut outputs)? };
        if inputs > 16 || outputs > 16 {
            report
                .errors
                .push("Encoder reported too many media streams".into());
            return Ok(());
        }
        let mut input_ids: Vec<u32> = (0..inputs).collect();
        let mut output_ids: Vec<u32> = (0..outputs).collect();
        if let Err(error) = unsafe { transform.GetStreamIDs(&mut input_ids, &mut output_ids) } {
            if error.code() != E_NOTIMPL {
                return Err(error);
            }
            // 不实现 GetStreamIDs 的 MFT 使用从零递增的端口 ID。
            input_ids = (0..inputs).collect();
            output_ids = (0..outputs).collect();
        }
        for (ids, is_input) in [(&input_ids, true), (&output_ids, false)] {
            for &id in ids {
                let media = unsafe {
                    if is_input {
                        transform.GetInputCurrentType(id)
                    } else {
                        transform.GetOutputCurrentType(id)
                    }
                };
                match media {
                    Ok(media) => {
                        let info = media_type_info(&media, id);
                        if is_input {
                            report.inputs.push(info)
                        } else {
                            report.outputs.push(info)
                        }
                    }
                    Err(error) => report
                        .errors
                        .push(format!("CurrentType(input={is_input}, id={id}): {error}")),
                }
            }
        }
        report.output_types_complete = outputs > 0 && report.outputs.len() == outputs as usize;
        Ok(())
    })();
    if let Err(error) = result {
        report
            .errors
            .push(format!("GetStreamIDs/GetStreamCount: {error}"));
    }
}

fn media_type_info(media: &IMFMediaType, stream_id: u32) -> MediaTypeInfo {
    let major_type = unsafe { media.GetGUID(&MF_MT_MAJOR_TYPE) }.ok();
    let colorimetry = observe_video_colorimetry(major_type, |key| {
        unsafe { media.GetUINT32(key) }.map_err(|error| (error.code(), error.to_string()))
    });
    let pair = |key| {
        unsafe { media.GetUINT64(key) }
            .ok()
            .map(|value| ((value >> 32) as u32, value as u32))
    };
    unsafe {
        MediaTypeInfo {
            stream_id,
            major_type: major_type.map(|value| format!("{value:?}")),
            subtype: media
                .GetGUID(&MF_MT_SUBTYPE)
                .ok()
                .map(|value| format!("{value:?}")),
            size: pair(&MF_MT_FRAME_SIZE).filter(|&(w, h)| w != 0 && h != 0),
            fps: pair(&MF_MT_FRAME_RATE).filter(|&(n, d)| n != 0 && d != 0),
            colorimetry,
            bitrate_bps: media.GetUINT32(&MF_MT_AVG_BITRATE).ok().or_else(|| {
                media
                    .GetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND)
                    .ok()
                    .and_then(|value| value.checked_mul(8))
            }),
            sample_rate_hz: media.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok(),
            channels: media.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok(),
            profile: media.GetUINT32(&MF_MT_MPEG2_PROFILE).ok(),
            level: media.GetUINT32(&MF_MT_MPEG2_LEVEL).ok(),
        }
    }
}

fn observe_video_colorimetry(
    major_type: Option<GUID>,
    mut read: impl FnMut(&GUID) -> std::result::Result<u32, (HRESULT, String)>,
) -> Option<MediaColorimetry> {
    if major_type != Some(MFMediaType_Video) {
        return None;
    }
    let mut read_errors = BTreeMap::new();
    let [nominal_range, transfer_matrix, transfer_function, primaries, chroma_siting] = [
        ("nominal_range", MF_MT_VIDEO_NOMINAL_RANGE),
        ("transfer_matrix", MF_MT_YUV_MATRIX),
        ("transfer_function", MF_MT_TRANSFER_FUNCTION),
        ("primaries", MF_MT_VIDEO_PRIMARIES),
        ("chroma_siting", MF_MT_VIDEO_CHROMA_SITING),
    ]
    .map(|(key, guid)| match read(&guid) {
        Ok(value) => Some(value),
        Err((code, _)) if code == MF_E_ATTRIBUTENOTFOUND => None,
        Err((_, message)) => {
            read_errors.insert(key, message.chars().take(256).collect());
            None
        }
    });
    Some(MediaColorimetry {
        nominal_range,
        transfer_matrix,
        transfer_function,
        primaries,
        chroma_siting,
        read_errors,
    })
}

fn parameter_value(
    key: &str,
    value: &windows::Win32::System::Variant::VARIANT,
) -> windows::core::Result<u32> {
    if key == "low_latency" {
        // 编码器通常返回 VT_BOOL，也兼容实现返回非零 UINT32 的情况。
        bool::try_from(value)
            .map(u32::from)
            .or_else(|_| u32::try_from(value).map(|value| u32::from(value != 0)))
    } else {
        u32::try_from(value)
    }
}

fn read_parameters(api: &ICodecAPI, codec: Codec) -> Vec<ParameterReadback> {
    [
        ("rate_control", CODECAPI_AVEncCommonRateControlMode),
        ("mean_bitrate", CODECAPI_AVEncCommonMeanBitRate),
        ("max_bitrate", CODECAPI_AVEncCommonMaxBitRate),
        ("quality", CODECAPI_AVEncCommonQuality),
        ("quality_vs_speed", CODECAPI_AVEncCommonQualityVsSpeed),
        (codec.buffer_size_key(), CODECAPI_AVEncCommonBufferSize),
        ("gop_frames", CODECAPI_AVEncMPVGOPSize),
        ("b_frames", CODECAPI_AVEncMPVDefaultBPictureCount),
        ("low_latency", CODECAPI_AVLowLatencyMode),
    ]
    .into_iter()
    .map(|(key, guid)| {
        let result = unsafe { api.GetValue(&guid) }.and_then(|value| parameter_value(key, &value));
        match result {
            Ok(value) => ParameterReadback {
                key,
                value: Some(value),
                error: None,
            },
            Err(error) => ParameterReadback {
                key,
                value: None,
                error: Some(error.to_string()),
            },
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_encoder_caps_colorimetry_requires_actual_video_major_type() {
        for major_type in [None, Some(MFMediaType_Audio)] {
            assert_eq!(
                observe_video_colorimetry(major_type, |_| {
                    panic!("A missing or non-video major type must not read color attributes")
                }),
                None
            );
        }
    }

    #[test]
    fn recording_encoder_caps_colorimetry_preserves_raw_values_and_bounded_errors() {
        let mut calls = Vec::new();
        let observed = observe_video_colorimetry(Some(MFMediaType_Video), |guid| {
            calls.push(*guid);
            match calls.len() {
                1 => Ok(0),
                2 => Ok(u32::MAX),
                3 => Err((MF_E_ATTRIBUTENOTFOUND, "attribute absent".into())),
                4 => Err((E_NOTIMPL, "读".repeat(300))),
                5 => Ok(17),
                _ => panic!("Color observation must use exactly five fixed reads"),
            }
        })
        .unwrap();
        assert_eq!(
            calls,
            [
                MF_MT_VIDEO_NOMINAL_RANGE,
                MF_MT_YUV_MATRIX,
                MF_MT_TRANSFER_FUNCTION,
                MF_MT_VIDEO_PRIMARIES,
                MF_MT_VIDEO_CHROMA_SITING,
            ]
        );
        assert_eq!(observed.nominal_range, Some(0));
        assert_eq!(observed.transfer_matrix, Some(u32::MAX));
        assert_eq!(observed.transfer_function, None);
        assert_eq!(observed.primaries, None);
        assert_eq!(observed.chroma_siting, Some(17));
        assert_eq!(observed.read_errors.len(), 1);
        assert_eq!(observed.read_errors["primaries"].chars().count(), 256);
        assert_eq!(
            serde_json::to_value(observed).unwrap(),
            serde_json::json!({
                "nominal_range": 0,
                "transfer_matrix": u32::MAX,
                "transfer_function": null,
                "primaries": null,
                "chroma_siting": 17,
                "read_errors": { "primaries": "读".repeat(256) },
            })
        );
    }

    #[test]
    fn recording_encoder_caps_colorimetry_missing_attributes_stay_unknown() {
        let mut reads = 0;
        let observed = observe_video_colorimetry(Some(MFMediaType_Video), |_| {
            reads += 1;
            Err((MF_E_ATTRIBUTENOTFOUND, "attribute absent".into()))
        })
        .unwrap();
        assert_eq!(reads, 5);
        assert_eq!(
            serde_json::to_value(observed).unwrap(),
            serde_json::json!({
                "nominal_range": null,
                "transfer_matrix": null,
                "transfer_function": null,
                "primaries": null,
                "chroma_siting": null,
                "read_errors": {},
            })
        );
    }

    fn encoder(kind: EncoderKind) -> EncoderIdentity {
        EncoderIdentity {
            clsid: Some("encoder".into()),
            name: None,
            hardware_url: None,
            kind,
        }
    }

    #[test]
    fn recording_encoder_caps_adapter_blob_requires_exact_type_and_lengths() {
        let bytes = [0x78, 0x56, 0x34, 0x12, 0xff, 0xff, 0xff, 0xff];
        let adapter = parse_encoder_adapter_blob(MF_ATTRIBUTE_BLOB, 8, &bytes, 8).unwrap();
        assert_eq!(adapter.luid_low, 0x1234_5678);
        assert_eq!(adapter.luid_high, -1);
        assert_eq!(adapter.name, None);
        assert!(parse_encoder_adapter_blob(MF_ATTRIBUTE_UINT64, 8, &bytes, 8).is_err());
        for size in [0, 7, 9, u32::MAX] {
            assert!(parse_encoder_adapter_blob(MF_ATTRIBUTE_BLOB, size, &bytes, 8).is_err());
            assert!(parse_encoder_adapter_blob(MF_ATTRIBUTE_BLOB, 8, &bytes, size).is_err());
        }
        assert!(parse_encoder_adapter_blob(MF_ATTRIBUTE_BLOB, 8, &bytes[..7], 8).is_err());
        assert!(parse_encoder_adapter_blob(MF_ATTRIBUTE_BLOB, 8, &[0; 9], 8).is_err());
    }

    #[test]
    fn recording_encoder_caps_actual_encoder_requires_unique_ended_chain() {
        assert_eq!(only_observed_encoder(vec![Some(7)], true), Ok(7));
        assert!(only_observed_encoder::<u8>(vec![], true).is_err());
        assert!(only_observed_encoder::<u8>(vec![None], true).is_err());
        assert!(only_observed_encoder(vec![Some(7), Some(7)], true).is_err());
        assert!(only_observed_encoder(vec![Some(7)], false).is_err());
        assert!(only_observed_encoder(vec![Some(7); 32], false).is_err());
    }

    #[test]
    fn recording_encoder_caps_adapter_name_failure_keeps_luid_without_input_fallback() {
        let known = AdapterInfo {
            name: None,
            luid_low: 17,
            luid_high: -2,
        };
        let (observed, diagnostic) =
            complete_adapter_observation(Ok(known.clone()), |_| Err("name unavailable".into()));
        assert_eq!(observed, Some(known.clone()));
        assert_eq!(diagnostic.as_deref(), Some("name unavailable"));

        let mut video = StreamInspection::empty(Codec::H264);
        video.encoder = Some(encoder(EncoderKind::Hardware));
        (video.encoder_adapter, video.encoder_adapter_diagnostic) =
            complete_adapter_observation(Err("attribute unavailable".into()), |_| {
                panic!("An absent encoder LUID cannot be looked up or replaced")
            });
        let inspection = SinkWriterInspection {
            input: VideoInput::GpuNv12,
            input_adapter: Some(known),
            video,
            audio: None,
        };
        assert!(inspection.input_adapter.is_some());
        assert_eq!(inspection.video.encoder_adapter, None);
        assert_eq!(
            inspection.video.encoder_adapter_diagnostic.as_deref(),
            Some("attribute unavailable")
        );
        assert_eq!(
            inspection.video.encoder.unwrap().kind,
            EncoderKind::Hardware
        );
    }

    #[test]
    fn recording_encoder_caps_video_codec_keeps_h264_baseline_and_aac_out_of_video_requests() {
        assert_eq!(VideoCodec::default(), VideoCodec::H264);
        assert_eq!(VideoCodec::H264.id(), "h264");
        assert_eq!(VideoCodec::Hevc.id(), "hevc");
        assert_eq!(VideoCodec::Av1.id(), "av1");
        assert_eq!(VideoCodec::H264.subtype(), MFVideoFormat_H264);
        assert_eq!(VideoCodec::Hevc.subtype(), MFVideoFormat_HEVC);
        assert_eq!(VideoCodec::Av1.subtype(), MFVideoFormat_AV1);
        assert_eq!(catalog_codecs(VideoCodec::H264), [Codec::H264, Codec::Aac]);
        assert_eq!(catalog_codecs(VideoCodec::Hevc), [Codec::Hevc, Codec::Aac]);
        assert_eq!(catalog_codecs(VideoCodec::Av1), [Codec::Av1, Codec::Aac]);
        assert_eq!(Codec::Hevc.category(), MFT_CATEGORY_VIDEO_ENCODER);
        assert_eq!(Codec::Av1.category(), MFT_CATEGORY_VIDEO_ENCODER);
        assert_eq!(Codec::Aac.category(), MFT_CATEGORY_AUDIO_ENCODER);
        assert_eq!(serde_json::to_value(Codec::H264).unwrap(), "h264");
        assert_eq!(serde_json::to_value(Codec::Aac).unwrap(), "aac");
        assert_eq!(serde_json::to_value(Codec::Hevc).unwrap(), "hevc");
        assert_eq!(serde_json::to_value(Codec::Av1).unwrap(), "av1");
        assert_eq!(serde_json::to_value(VideoCodec::Av1).unwrap(), "av1");
    }

    #[test]
    fn recording_encoder_caps_hevc_and_av1_classification_requires_its_own_complete_catalog() {
        let item = encoder(EncoderKind::Unknown);
        for selected_codec in [Codec::Hevc, Codec::Av1] {
            let mut catalog = EncoderCatalog {
                enumerations: vec![],
            };
            for codec in [Codec::H264, Codec::Aac] {
                for kind in [EncoderKind::Hardware, EncoderKind::Software] {
                    catalog.enumerations.push(CodecEnumeration {
                        codec,
                        kind,
                        candidates: if kind == EncoderKind::Software {
                            vec![encoder(kind)]
                        } else {
                            vec![]
                        },
                        error: None,
                    });
                }
            }
            assert_eq!(
                actual_kind(&item, selected_codec, Some(&catalog)),
                EncoderKind::Unknown
            );
            catalog.enumerations.push(CodecEnumeration {
                codec: selected_codec,
                kind: EncoderKind::Software,
                candidates: vec![encoder(EncoderKind::Software)],
                error: None,
            });
            assert_eq!(
                actual_kind(&item, selected_codec, Some(&catalog)),
                EncoderKind::Unknown
            );
            catalog.enumerations.push(CodecEnumeration {
                codec: selected_codec,
                kind: EncoderKind::Hardware,
                candidates: vec![],
                error: None,
            });
            assert_eq!(
                actual_kind(&item, selected_codec, Some(&catalog)),
                EncoderKind::Software
            );
            catalog.enumerations.last_mut().unwrap().error =
                Some("hardware enumeration failed".into());
            assert_eq!(
                actual_kind(&item, selected_codec, Some(&catalog)),
                EncoderKind::Unknown
            );
        }
    }

    #[test]
    fn recording_encoder_caps_expected_codec_is_checked_against_observed_output_types() {
        let output = |major: Option<GUID>, subtype: Option<GUID>| MediaTypeInfo {
            stream_id: 0,
            major_type: major.map(|value| format!("{value:?}")),
            subtype: subtype.map(|value| format!("{value:?}")),
            size: None,
            fps: None,
            colorimetry: None,
            bitrate_bps: None,
            sample_rate_hz: None,
            channels: None,
            profile: None,
            level: None,
        };
        let mut inspection = StreamInspection::empty(Codec::Hevc);
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection
            .outputs
            .push(output(Some(MFMediaType_Video), Some(MFVideoFormat_HEVC)));
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection.output_types_complete = true;
        assert_eq!(inspection.codec_matches_expected(), Some(true));
        inspection.outputs = vec![output(None, Some(MFVideoFormat_HEVC))];
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection.outputs = vec![output(Some(MFMediaType_Audio), Some(MFVideoFormat_HEVC))];
        assert_eq!(inspection.codec_matches_expected(), Some(false));
        inspection.outputs = vec![output(Some(MFMediaType_Video), Some(MFVideoFormat_HEVC))];
        inspection
            .outputs
            .push(output(Some(MFMediaType_Video), None));
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection
            .outputs
            .push(output(Some(MFMediaType_Video), Some(MFVideoFormat_H264)));
        assert_eq!(inspection.codec_matches_expected(), Some(false));
        inspection.outputs = vec![output(Some(MFMediaType_Audio), Some(MFAudioFormat_AAC))];
        assert_eq!(inspection.codec_matches_expected(), Some(false));
        inspection.expected_codec = Codec::Aac;
        assert_eq!(inspection.codec_matches_expected(), Some(true));
        inspection.outputs = vec![output(None, Some(MFAudioFormat_AAC))];
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection.outputs = vec![output(Some(MFMediaType_Video), Some(MFAudioFormat_AAC))];
        assert_eq!(inspection.codec_matches_expected(), Some(false));
        inspection.expected_codec = Codec::Av1;
        inspection.outputs = vec![output(Some(MFMediaType_Video), Some(MFVideoFormat_AV1))];
        assert_eq!(inspection.codec_matches_expected(), Some(true));
        inspection.output_types_complete = false;
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection.output_types_complete = true;
        inspection.outputs = vec![output(None, Some(MFVideoFormat_AV1))];
        assert_eq!(inspection.codec_matches_expected(), None);
        inspection.outputs = vec![output(Some(MFMediaType_Video), Some(MFVideoFormat_H264))];
        assert_eq!(inspection.codec_matches_expected(), Some(false));
        inspection.outputs = vec![output(Some(MFMediaType_Audio), Some(MFVideoFormat_AV1))];
        assert_eq!(inspection.codec_matches_expected(), Some(false));
    }

    #[test]
    fn recording_encoder_caps_unknown_is_not_software() {
        let item = encoder(EncoderKind::Unknown);
        assert_eq!(actual_kind(&item, Codec::H264, None), EncoderKind::Unknown);
        let mut catalog = EncoderCatalog {
            enumerations: vec![
                CodecEnumeration {
                    codec: Codec::H264,
                    kind: EncoderKind::Software,
                    candidates: vec![encoder(EncoderKind::Software)],
                    error: None,
                },
                CodecEnumeration {
                    codec: Codec::H264,
                    kind: EncoderKind::Hardware,
                    candidates: vec![],
                    error: None,
                },
            ],
        };
        assert_eq!(
            actual_kind(&item, Codec::H264, Some(&catalog)),
            EncoderKind::Software
        );
        assert_eq!(
            actual_kind(&item, Codec::Aac, Some(&catalog)),
            EncoderKind::Unknown
        );
        catalog.enumerations[1].error = Some("enumeration failed".into());
        assert_eq!(
            actual_kind(&item, Codec::H264, Some(&catalog)),
            EncoderKind::Unknown
        );
    }

    #[test]
    fn recording_encoder_caps_conflicting_registration_stays_unknown() {
        let mut catalog = EncoderCatalog {
            enumerations: vec![],
        };
        for kind in [EncoderKind::Hardware, EncoderKind::Software] {
            catalog.enumerations.push(CodecEnumeration {
                codec: Codec::H264,
                kind,
                candidates: vec![encoder(kind)],
                error: None,
            });
        }
        let mut item = encoder(EncoderKind::Unknown);
        assert_eq!(
            actual_kind(&item, Codec::H264, Some(&catalog)),
            EncoderKind::Unknown
        );
        item.hardware_url = Some("hardware-device".into());
        assert_eq!(
            actual_kind(&item, Codec::H264, Some(&catalog)),
            EncoderKind::Hardware
        );
    }

    #[test]
    fn recording_encoder_caps_trial_does_not_imply_sample_success() {
        let trial = EncodingTrial {
            media_types: TrialStep::Succeeded,
            begin_writing: TrialStep::Succeeded,
            first_video_sample: TrialStep::Failed {
                error: "device lost".into(),
            },
            ..Default::default()
        };
        let value = serde_json::to_value(trial).unwrap();
        assert_eq!(value["first_video_sample"]["state"], "failed");
        assert_eq!(value["finalize"]["state"], "not_attempted");
        assert_eq!(value["reopen"]["state"], "not_attempted");
    }

    #[test]
    fn recording_encoder_caps_first_step_keeps_first_actual_result() {
        let mut failed = TrialStep::NotAttempted;
        assert!(failed.record_first(&Err("device lost")));
        assert!(!failed.record_first(&Ok::<(), &str>(())));
        assert_eq!(
            failed,
            TrialStep::Failed {
                error: "device lost".into()
            }
        );
        let mut succeeded = TrialStep::NotAttempted;
        assert!(succeeded.record_first(&Ok::<(), &str>(())));
        assert!(!succeeded.record_first(&Err("late failure")));
        assert_eq!(succeeded, TrialStep::Succeeded);
    }

    #[test]
    fn recording_encoder_caps_buffer_readback_keeps_bytes_and_rejects_legacy_bit_key() {
        use windows::Win32::System::Variant::VARIANT;
        assert_eq!(Codec::H264.buffer_size_key(), "buffer_bytes");
        assert_eq!(Codec::Hevc.buffer_size_key(), "buffer_bytes");
        assert_eq!(Codec::Av1.buffer_size_key(), "buffer_size_native");
        assert_eq!(Codec::Aac.buffer_size_key(), "buffer_size_native");
        let bytes = 125_000_u32;
        assert_eq!(
            parameter_value("buffer_bytes", &VARIANT::from(bytes)).unwrap(),
            bytes
        );
        let request = || ParameterRequest {
            key: "buffer_bytes",
            value: Some(bytes),
            source: "encoding_parameters",
            set_succeeded: None,
        };
        let observed = [ParameterReadback {
            key: "buffer_bytes",
            value: Some(bytes),
            error: None,
        }];
        let compared = compare_parameter_requests(&observed, vec![request()]);
        assert_eq!(compared[0].observed, Some(bytes));
        assert_eq!(compared[0].matches_request, Some(true));
        // 旧 key 即使碰巧有相同数值，也不授予本轮字节请求的回读证据。
        let legacy = [ParameterReadback {
            key: "buffer_bits",
            value: Some(bytes),
            error: None,
        }];
        let compared = compare_parameter_requests(&legacy, vec![request()]);
        assert_eq!(compared[0].observed, None);
        assert_eq!(compared[0].matches_request, None);
        for codec in [Codec::Aac, Codec::Av1] {
            let native = [ParameterReadback {
                key: codec.buffer_size_key(),
                value: Some(bytes),
                error: None,
            }];
            assert_eq!(
                parameter_value(codec.buffer_size_key(), &VARIANT::from(bytes)).unwrap(),
                bytes
            );
            let compared = compare_parameter_requests(&native, vec![request()]);
            assert_eq!(compared[0].observed, None);
            assert_eq!(compared[0].matches_request, None);
        }
    }

    #[test]
    fn recording_encoder_caps_parameter_requests_preserve_unknown_and_mismatch() {
        use windows::Win32::System::Variant::VARIANT;
        assert_eq!(
            parameter_value("low_latency", &VARIANT::from(true)).unwrap(),
            1
        );
        assert_eq!(
            parameter_value("low_latency", &VARIANT::from(false)).unwrap(),
            0
        );
        assert_eq!(
            parameter_value("mean_bitrate", &VARIANT::from(192_000_u32)).unwrap(),
            192_000
        );
        let observed = vec![
            ParameterReadback {
                key: "low_latency",
                value: Some(0),
                error: None,
            },
            ParameterReadback {
                key: "mean_bitrate",
                value: Some(192_000),
                error: None,
            },
            ParameterReadback {
                key: "b_frames",
                value: None,
                error: Some("unsupported".into()),
            },
        ];
        let requests = vec![
            ParameterRequest {
                key: "low_latency",
                value: Some(1),
                source: "codec_api_before_begin_writing",
                set_succeeded: Some(true),
            },
            ParameterRequest {
                key: "mean_bitrate",
                value: None,
                source: "encoding_parameters",
                set_succeeded: None,
            },
            ParameterRequest {
                key: "b_frames",
                value: Some(0),
                source: "codec_api_before_begin_writing",
                set_succeeded: Some(true),
            },
        ];
        let compared = compare_parameter_requests(&observed, requests);
        assert_eq!(compared[0].matches_request, Some(false));
        assert_eq!(compared[1].matches_request, None);
        assert_eq!(compared[2].matches_request, None);
        assert_eq!(compared[2].read_error.as_deref(), Some("unsupported"));
    }
}
