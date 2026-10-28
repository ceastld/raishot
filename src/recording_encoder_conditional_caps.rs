//! HEVC/AV1 的注册候选目录；不是可录制、容器兼容或实时性能的证明。
//! 在工作线程同步调用；不激活 MFT、不创建 GPU、不写媒体，COM 对象在本线程释放。

use super::{identity, ActivationArray, EncoderIdentity, EncoderKind};
use crate::{media_foundation::MfRuntime, Result};
use serde::Serialize;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    core::GUID,
    Win32::Media::MediaFoundation::{
        MFMediaType_Video, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_HEVC,
        MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_REGISTER_TYPE_INFO,
    },
};

const MAX_CANDIDATES_PER_GROUP: usize = 32;
const MAX_TEXT_CHARS: usize = 512;
const MAX_ERROR_CHARS: usize = 1024;
const PROBE_BUDGET: Duration = Duration::from_secs(5);
const SCOPE: &str = "registered HEVC/AV1 encoder candidates only; kind identifies the hardware/software enumeration query, not an encoder observed recording; no activation, GPU initialization, media negotiation, sample write, container output or playback was attempted; native enumeration, attribute reads and COM cleanup cannot be forcibly interrupted; cooperative checks occur around groups and between candidate-identity reads, one identity read may contain several native attribute calls; limits bound retained candidates and report text, not the native enumeration allocation or blocking-call duration; an empty successful group only means not found by this query, and an error or partial group is not proof of permanent lack of support";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConditionalCodec {
    Hevc,
    Av1,
}

impl ConditionalCodec {
    pub(crate) const ALL: [Self; 2] = [Self::Hevc, Self::Av1];

    fn output(self) -> MFT_REGISTER_TYPE_INFO {
        MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: self.subtype(),
        }
    }

    fn subtype(self) -> GUID {
        // 使用项目锁定的 windows 0.61.3 绑定常量，不自造 GUID 或将 HEVC_ES 混为同一查询。
        match self {
            Self::Hevc => MFVideoFormat_HEVC,
            Self::Av1 => MFVideoFormat_AV1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EnumerationState {
    NotAttempted,
    Completed,
    Failed,
    Cancelled,
    Limited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EnumerationLimit {
    CandidateCount,
    ElapsedTime,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConditionalEnumeration {
    pub codec: ConditionalCodec,
    pub kind: EncoderKind,
    pub output_subtype: String,
    pub state: EnumerationState,
    /// 系统报告的数组长度；None 表示尚未取得成功枚举结果。
    pub native_count: Option<u32>,
    pub visited_slots: u32,
    pub candidates: Vec<EncoderIdentity>,
    /// 已知原生数组超出保留上限；取消/时间限制还可能使更小数组未读完。
    pub truncated: bool,
    pub metadata_text_truncated: bool,
    pub limit: Option<EnumerationLimit>,
    pub error_hresult: Option<i32>,
    pub error: Option<String>,
}

impl ConditionalEnumeration {
    fn new(codec: ConditionalCodec, kind: EncoderKind) -> Self {
        Self {
            codec,
            kind,
            output_subtype: format!("{:?}", codec.subtype()),
            state: EnumerationState::NotAttempted,
            native_count: None,
            visited_slots: 0,
            candidates: Vec::new(),
            truncated: false,
            metadata_text_truncated: false,
            limit: None,
            error_hresult: None,
            error: None,
        }
    }

    fn interrupt(&mut self, interruption: Interruption) {
        match interruption {
            Interruption::Cancelled => self.state = EnumerationState::Cancelled,
            Interruption::Elapsed => {
                self.state = EnumerationState::Limited;
                self.limit = Some(EnumerationLimit::ElapsedTime);
            }
        }
    }

    fn begin_array(&mut self, count: u32) {
        self.native_count = Some(count);
        self.truncated = u64::from(count) > MAX_CANDIDATES_PER_GROUP as u64;
        self.state = if self.truncated {
            self.limit = Some(EnumerationLimit::CandidateCount);
            EnumerationState::Limited
        } else {
            EnumerationState::Completed
        };
    }

    fn fail(&mut self, hresult: Option<i32>, error: impl std::fmt::Display) {
        self.state = EnumerationState::Failed;
        self.error_hresult = hresult;
        self.error = Some(error.to_string().chars().take(MAX_ERROR_CHARS).collect());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Recordability {
    NotTested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordingRoute {
    NotImplemented,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConditionState {
    NotVerified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordingCondition {
    SystemComponentsAndDriverRuntime,
    ActualEncoderAndAdapter,
    MediaTypesDimensionsAndFrameRate,
    InputPixelFormat,
    ProfileLevelBitDepthAndChroma,
    ContainerAndAudioCombination,
    FirstVideoAndAudioSamples,
    FinalizeAndContainerIntegrity,
    CompleteReopenDecode,
    EncodingSpeedAndResources,
    PlayerCompatibility,
    FallbackToBaseline,
}

const CONDITIONS: [RecordingCondition; 12] = [
    RecordingCondition::SystemComponentsAndDriverRuntime,
    RecordingCondition::ActualEncoderAndAdapter,
    RecordingCondition::MediaTypesDimensionsAndFrameRate,
    RecordingCondition::InputPixelFormat,
    RecordingCondition::ProfileLevelBitDepthAndChroma,
    RecordingCondition::ContainerAndAudioCombination,
    RecordingCondition::FirstVideoAndAudioSamples,
    RecordingCondition::FinalizeAndContainerIntegrity,
    RecordingCondition::CompleteReopenDecode,
    RecordingCondition::EncodingSpeedAndResources,
    RecordingCondition::PlayerCompatibility,
    RecordingCondition::FallbackToBaseline,
];

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct ConditionObservation {
    pub condition: RecordingCondition,
    pub state: ConditionState,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConditionalRecordingStatus {
    pub codec: ConditionalCodec,
    pub recordability: Recordability,
    pub route: RecordingRoute,
    pub conditions: [ConditionObservation; CONDITIONS.len()],
}

impl ConditionalRecordingStatus {
    fn untested(codec: ConditionalCodec) -> Self {
        Self {
            codec,
            recordability: Recordability::NotTested,
            route: RecordingRoute::NotImplemented,
            conditions: CONDITIONS.map(|condition| ConditionObservation {
                condition,
                state: ConditionState::NotVerified,
            }),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConditionalCatalog {
    pub enumerations: Vec<ConditionalEnumeration>,
    pub recording: [ConditionalRecordingStatus; ConditionalCodec::ALL.len()],
    pub enumerations_complete: bool,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    pub budget_ms: u64,
    pub max_candidates_per_group: usize,
    pub max_text_chars: usize,
    pub scope: &'static str,
}

impl ConditionalCatalog {
    fn empty() -> Self {
        Self {
            enumerations: ConditionalCodec::ALL
                .into_iter()
                .flat_map(|codec| {
                    [EncoderKind::Hardware, EncoderKind::Software]
                        .into_iter()
                        .map(move |kind| ConditionalEnumeration::new(codec, kind))
                })
                .collect(),
            recording: ConditionalCodec::ALL.map(ConditionalRecordingStatus::untested),
            enumerations_complete: false,
            cancelled: false,
            elapsed_ms: 0,
            budget_ms: PROBE_BUDGET.as_millis() as u64,
            max_candidates_per_group: MAX_CANDIDATES_PER_GROUP,
            max_text_chars: MAX_TEXT_CHARS,
            scope: SCOPE,
        }
    }

    fn finish(&mut self, elapsed: Duration) {
        self.enumerations_complete = self
            .enumerations
            .iter()
            .all(|group| group.state == EnumerationState::Completed);
        self.cancelled = self
            .enumerations
            .iter()
            .any(|group| group.state == EnumerationState::Cancelled);
        self.elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interruption {
    Cancelled,
    Elapsed,
}

fn interruption(stop: &AtomicBool, elapsed: Duration) -> Option<Interruption> {
    if stop.load(Ordering::Acquire) {
        Some(Interruption::Cancelled)
    } else if elapsed >= PROBE_BUDGET {
        Some(Interruption::Elapsed)
    } else {
        None
    }
}

/// 同步、只枚举。限额检查在调用间进行，不能强制中止原生枚举、属性读取或 COM 清理。
/// 初始取消不初始化 COM/MF；初始化错误返回 Err，各组枚举错误保留在报告内。
pub(crate) fn probe(stop: &AtomicBool) -> Result<ConditionalCatalog> {
    let started = Instant::now();
    let mut report = ConditionalCatalog::empty();
    if let Some(interrupted) = interruption(stop, started.elapsed()) {
        for group in &mut report.enumerations {
            group.interrupt(interrupted);
        }
        report.finish(started.elapsed());
        return Ok(report);
    }
    // 报告只返回字符串和数值；不把 activation 或其它 COM 对象传给宿主线程。
    let runtime = MfRuntime::new()?;
    for group in &mut report.enumerations {
        if let Some(interrupted) = interruption(stop, started.elapsed()) {
            group.interrupt(interrupted);
            continue;
        }
        enumerate(group, stop, started);
        // enumerate 返回时数组和 COM 引用已经在当前线程释放；收尾耗时也计入预算。
        if let Some(interrupted) = interruption(stop, started.elapsed()) {
            group.interrupt(interrupted);
        }
    }
    drop(runtime);
    // MFShutdown 自身同样不可强停；超时/取消时不把整轮目录称为完整结果。
    if let Some(interrupted) = interruption(stop, started.elapsed()) {
        if let Some(group) = report.enumerations.last_mut() {
            group.interrupt(interrupted);
        }
    }
    report.finish(started.elapsed());
    Ok(report)
}

fn enumeration_flags(kind: EncoderKind) -> Option<MFT_ENUM_FLAG> {
    match kind {
        EncoderKind::Hardware => Some(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER),
        EncoderKind::Software => {
            Some(MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER)
        }
        EncoderKind::Unknown => None,
    }
}

fn enumerate(group: &mut ConditionalEnumeration, stop: &AtomicBool, started: Instant) {
    let Some(flags) = enumeration_flags(group.kind) else {
        group.fail(None, "Unknown is not an enumeration query route");
        return;
    };
    let output = group.codec.output();
    let mut array = ActivationArray {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    let enumerated = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output),
            &mut array.ptr,
            &mut array.len,
        )
    };
    if let Err(error) = enumerated {
        group.fail(Some(error.code().0), error);
        return;
    }
    group.begin_array(array.len);
    if array.len != 0 && array.ptr.is_null() {
        group.fail(
            None,
            "MFTEnumEx returned a null activation array with a nonzero count",
        );
        return;
    }
    if let Some(interrupted) = interruption(stop, started.elapsed()) {
        group.interrupt(interrupted);
        return;
    }
    let retained_slots = (u64::from(array.len).min(MAX_CANDIDATES_PER_GROUP as u64)) as usize;
    for index in 0..retained_slots {
        if let Some(interrupted) = interruption(stop, started.elapsed()) {
            group.interrupt(interrupted);
            return;
        }
        group.visited_slots += 1;
        let Some(activation) = (unsafe { &*array.ptr.add(index) }) else {
            group.fail(None, "MFTEnumEx returned an empty activation slot");
            return;
        };
        let (candidate, truncated) = bounded_identity(identity(activation, group.kind));
        group.metadata_text_truncated |= truncated;
        group.candidates.push(candidate);
        if let Some(interrupted) = interruption(stop, started.elapsed()) {
            group.interrupt(interrupted);
            return;
        }
    }
}

fn bounded_identity(mut candidate: EncoderIdentity) -> (EncoderIdentity, bool) {
    let mut truncated = false;
    for value in [
        &mut candidate.clsid,
        &mut candidate.name,
        &mut candidate.hardware_url,
    ] {
        if let Some(text) = value {
            if text.chars().count() > MAX_TEXT_CHARS {
                *text = text.chars().take(MAX_TEXT_CHARS).collect();
                truncated = true;
            }
        }
    }
    (candidate, truncated)
}

// DTO 可跨线程；没有把 MFT activation 或 MfRuntime 放入结果。
fn _catalog_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<ConditionalCatalog>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_encoder_conditional_caps_guid_and_conditions_do_not_claim_recordability() {
        assert_eq!(
            ConditionalCodec::Hevc.output().guidSubtype,
            MFVideoFormat_HEVC
        );
        assert_eq!(
            ConditionalCodec::Av1.output().guidSubtype,
            MFVideoFormat_AV1
        );
        assert_eq!(
            ConditionalCodec::Av1.output().guidMajorType,
            MFMediaType_Video
        );
        assert!(enumeration_flags(EncoderKind::Unknown).is_none());
        let report = ConditionalCatalog::empty();
        assert_eq!(report.enumerations.len(), 4);
        for item in &report.recording {
            assert_eq!(item.recordability, Recordability::NotTested);
            assert_eq!(item.route, RecordingRoute::NotImplemented);
            assert!(item
                .conditions
                .iter()
                .all(|condition| condition.state == ConditionState::NotVerified));
        }
        let encoded = serde_json::to_value(report).unwrap();
        assert!(encoded["recording"]
            .as_array()
            .unwrap()
            .iter()
            .all(
                |item| item["recordability"] == "not_tested" && item["route"] == "not_implemented"
            ));
        assert!(!encoded.to_string().contains("\"supported\""));
    }

    #[test]
    fn recording_encoder_conditional_caps_pre_cancel_skips_runtime_and_enumeration() {
        let report = probe(&AtomicBool::new(true)).unwrap();
        assert!(report.cancelled);
        assert!(!report.enumerations_complete);
        assert!(report
            .enumerations
            .iter()
            .all(|group| group.state == EnumerationState::Cancelled
                && group.native_count.is_none()
                && group.candidates.is_empty()));
        let stop = AtomicBool::new(false);
        assert_eq!(
            interruption(&stop, PROBE_BUDGET),
            Some(Interruption::Elapsed)
        );
        stop.store(true, Ordering::Release);
        assert_eq!(
            interruption(&stop, PROBE_BUDGET),
            Some(Interruption::Cancelled)
        );
    }

    #[test]
    fn recording_encoder_conditional_caps_empty_error_and_truncation_remain_distinct() {
        let mut group = ConditionalEnumeration::new(ConditionalCodec::Hevc, EncoderKind::Hardware);
        group.begin_array(0);
        assert_eq!(group.state, EnumerationState::Completed);
        assert_eq!(group.native_count, Some(0));
        assert!(group.candidates.is_empty());
        assert!(group.error.is_none());
        group.begin_array(MAX_CANDIDATES_PER_GROUP as u32 + 1);
        assert_eq!(group.state, EnumerationState::Limited);
        assert!(group.truncated);
        assert_eq!(group.limit, Some(EnumerationLimit::CandidateCount));
        group.fail(Some(-1), "enumeration failed");
        assert_eq!(group.state, EnumerationState::Failed);
        assert_eq!(group.error_hresult, Some(-1));
        group.interrupt(Interruption::Cancelled);
        assert_eq!(group.state, EnumerationState::Cancelled);
        assert_eq!(group.error.as_deref(), Some("enumeration failed"));
        assert!(group.truncated);
    }

    #[test]
    fn recording_encoder_conditional_caps_text_limits_keep_utf8_and_unknown_attributes() {
        let candidate = EncoderIdentity {
            clsid: None,
            name: Some("编".repeat(MAX_TEXT_CHARS + 1)),
            hardware_url: Some("h".repeat(MAX_TEXT_CHARS)),
            kind: EncoderKind::Hardware,
        };
        let (candidate, truncated) = bounded_identity(candidate);
        assert!(truncated);
        assert_eq!(
            candidate.name.as_ref().unwrap().chars().count(),
            MAX_TEXT_CHARS
        );
        assert_eq!(
            candidate.hardware_url.as_ref().unwrap().len(),
            MAX_TEXT_CHARS
        );
        assert!(candidate.clsid.is_none());
        let (_, truncated_again) = bounded_identity(candidate);
        assert!(!truncated_again);
    }

    #[test]
    #[ignore = "native enumeration only; run explicitly on the target Windows machine"]
    fn recording_encoder_conditional_caps_native_catalog_report() -> Result<()> {
        use std::{
            fs,
            io::Write,
            time::{SystemTime, UNIX_EPOCH},
        };
        let report = probe(&AtomicBool::new(false))?;
        assert_eq!(report.enumerations.len(), 4);
        assert!(report
            .enumerations
            .iter()
            .all(|group| group.candidates.len() <= MAX_CANDIDATES_PER_GROUP));
        assert!(report
            .recording
            .iter()
            .all(|item| item.recordability == Recordability::NotTested
                && item.route == RecordingRoute::NotImplemented));
        let bytes = serde_json::to_vec_pretty(&report)?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err("Conditional encoder report exceeds its bound".into());
        }
        let root = std::path::absolute("target/recording-conditional-caps")?;
        fs::create_dir_all(&root)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = root.join(format!("{}-{stamp}", std::process::id()));
        fs::create_dir(&directory)?;
        let path = directory.join("catalog.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        eprintln!("Conditional encoder enumeration report: {}", path.display());
        Ok(())
    }
}
