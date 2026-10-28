//! 会话最终持有的 writer 的编码证据；只解释已有快照，不创建媒体或重新扫描成片。
//! 输入路线与实际编码器硬软分开，设置调用成功也不等于参数已采用。
#[path = "recording_video_observation.rs"]
pub(crate) mod video_observation;
use crate::{
    recording::output_plan::OutputPlan,
    recording_encoder_caps::{EncoderKind, VideoCodec, VideoInput},
    recording_encoder_policy::{
        codec::{CodecAttempt, CodecAttemptFailure, CodecRequest},
        EncoderPolicy,
    },
    recording_options::QualityProfile,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use windows::Win32::Media::MediaFoundation::MFMediaType_Video;

const SCOPE: &str = "full writer snapshot only for the final writer held by the recording session; excludes full snapshots of earlier failed writers but may retain bounded classified codec-attempt failures; requested codec, selected codec attempt, output plan and writer audio request describe requests, not actual MFT output or finished tracks; requested scale preserves source/target geometry intent and does not establish resized pixels, observed output dimensions or final file dimensions; confirmed video codec requires complete matching output inspection and does not verify samples, Finalize, playback, actual device/color fidelity, or video payload bitrate";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PayloadStatus {
    NotMeasured,
}

/// Unknown 表示阶段缺失、格式无效或内部矛盾，不能由其它阶段的成功补足。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepState {
    Unknown,
    NotAttempted,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionEncodingReport {
    pub policy: EncoderPolicy,
    pub requested_profile: QualityProfile,
    /// 只有显式高级请求携带解析结果；旧预设和候选入口保留 None。
    pub requested_quality: Option<crate::recording_options::advanced::ResolvedQualityRequest>,
    /// 显式源/目标几何请求，不代替实际输出尺寸或缩放后像素验证。
    pub requested_scale: Option<crate::recording::scale_plan::ScalePlan>,
    pub selected_attempt: Option<usize>,
    /// None 表示旧证据未知；不能从最终媒体类型反推原始请求。
    pub requested_codec: Option<CodecRequest>,
    /// 选用的请求路线、全局 slot 和局部 index，不是实际 MFT 编码格式。
    pub selected_codec_attempt: Option<CodecAttempt>,
    /// None 为未知，Some([]) 才表示本轮已知没有分类失败；不保存失败 writer 全快照。
    pub codec_failures: Option<Vec<CodecAttemptFailure>>,
    pub output_plan: Option<OutputPlan>,
    /// 设备启动后本次 writer 是否请求 AAC 流，不证明首样本或成片存在音轨。
    pub writer_audio_requested: Option<bool>,
    /// 原样保留 finish_with_diagnostics 返回的收尾后快照，包括未知值和错误。
    pub finalized_writer: Value,
    pub payload: PayloadStatus,
    pub scope: &'static str,
}

impl SessionEncodingReport {
    /// 调用方须传入 finish_with_diagnostics 的快照；这里不从调用顺序预测收尾成功。
    pub(crate) fn from_finalized_writer(
        policy: EncoderPolicy,
        requested_profile: QualityProfile,
        selected_attempt: Option<usize>,
        finalized_writer: Value,
    ) -> Self {
        Self {
            policy,
            requested_profile,
            requested_quality: None,
            requested_scale: None,
            selected_attempt,
            requested_codec: None,
            selected_codec_attempt: None,
            codec_failures: None,
            output_plan: None,
            writer_audio_requested: None,
            finalized_writer,
            payload: PayloadStatus::NotMeasured,
            scope: SCOPE,
        }
    }

    pub(crate) fn with_requested_quality(
        mut self,
        request: Option<crate::recording_options::advanced::ResolvedQualityRequest>,
    ) -> Self {
        self.requested_quality = request;
        self
    }

    pub(crate) fn with_requested_scale(
        mut self,
        request: Option<crate::recording::scale_plan::ScalePlan>,
    ) -> Self {
        self.requested_scale = request;
        self
    }

    pub(crate) fn with_codec_evidence(
        mut self,
        requested: CodecRequest,
        selected: Option<CodecAttempt>,
        failures: Vec<CodecAttemptFailure>,
    ) -> Self {
        self.requested_codec = Some(requested);
        self.selected_codec_attempt = selected;
        self.codec_failures = Some(failures);
        self
    }

    pub(crate) fn with_output_plan(mut self, plan: Option<OutputPlan>) -> Self {
        self.output_plan = plan;
        self
    }

    pub(crate) fn with_writer_audio_requested(mut self, requested: bool) -> Self {
        self.writer_audio_requested = Some(requested);
        self
    }

    /// 只确认最终 writer 的完整输出类型观测；请求、选用路线、名称和输入像素均不补证。
    /// complete 匹配标记与实际所有输出端口交叉核对，缺失或自相矛盾时保留未知。
    pub(crate) fn confirmed_video_codec(&self) -> Option<VideoCodec> {
        if !self
            .finalized_writer
            .get("observed_video_codec_matches_request")?
            .as_bool()?
        {
            return None;
        }
        let video = self
            .finalized_writer
            .pointer("/encoding_trial/inspection/video")?;
        let expected = match video.get("expected_codec")?.as_str()? {
            "h264" => VideoCodec::H264,
            "hevc" => VideoCodec::Hevc,
            "av1" => VideoCodec::Av1,
            _ => return None,
        };
        let outputs = video.get("outputs")?.as_array()?;
        if outputs.is_empty() || outputs.len() > 16 {
            return None;
        }
        // 与 caps::MediaTypeInfo 一样使用正式 GUID 的 Debug 字符串，不接受友好名称。
        let video_major = format!("{:?}", MFMediaType_Video);
        let subtype = format!("{:?}", expected.subtype());
        let mut streams = BTreeSet::new();
        for output in outputs {
            let stream = u32::try_from(output.get("stream_id")?.as_u64()?).ok()?;
            if !streams.insert(stream)
                || output.get("major_type")?.as_str()? != video_major.as_str()
                || output.get("subtype")?.as_str()? != subtype.as_str()
            {
                return None;
            }
        }
        Some(expected)
    }

    /// 初始化后同一 writer 的实际 MFT 输出类型；不重查文件或补齐缺失属性。
    pub(crate) fn initial_video_output_observation(
        &self,
    ) -> video_observation::VideoOutputObservation {
        if self.confirmed_video_codec().is_none() {
            return video_observation::VideoOutputObservation::default();
        }
        video_observation::from_outputs(
            self.finalized_writer
                .pointer("/encoding_trial/inspection/video/outputs"),
        )
    }

    /// 只读取实际 inspection 的输入路线，不借用顶层 legacy input 或硬件开关。
    pub(crate) fn input(&self) -> Option<VideoInput> {
        match self
            .finalized_writer
            .pointer("/encoding_trial/inspection/input")?
            .as_str()?
        {
            "cpu_rgb32" => Some(VideoInput::CpuRgb32),
            "gpu_nv12" => Some(VideoInput::GpuNv12),
            _ => None,
        }
    }

    fn video_encoder(&self) -> Option<&Value> {
        let encoder = self
            .finalized_writer
            .pointer("/encoding_trial/inspection/video/encoder")?;
        if !encoder.is_object() || encoder.get("error").is_some_and(|value| !value.is_null()) {
            return None;
        }
        Some(encoder)
    }

    pub(crate) fn video_encoder_kind(&self) -> EncoderKind {
        let Some(encoder) = self.video_encoder() else {
            return EncoderKind::Unknown;
        };
        match encoder.get("kind").and_then(Value::as_str) {
            Some("hardware") => EncoderKind::Hardware,
            // 硬件 URL 与 software 分类冲突时保留未知，不重新猜测分类。
            Some("software") if encoder.get("hardware_url").is_none_or(Value::is_null) => {
                EncoderKind::Software
            }
            _ => EncoderKind::Unknown,
        }
    }

    /// 名称仍可在 kind 未知时展示；它本身不证明硬件或软件身份。
    pub(crate) fn video_encoder_name(&self) -> Option<&str> {
        self.video_encoder()?
            .get("name")?
            .as_str()
            .filter(|name| !name.trim().is_empty())
    }

    pub(crate) fn finalize_state(&self) -> StepState {
        self.stage_state("finalize")
    }

    pub(crate) fn reopen_state(&self) -> StepState {
        self.stage_state("reopen")
    }

    fn stage_state(&self, stage: &str) -> StepState {
        let Some(step) = self
            .finalized_writer
            .get("encoding_trial")
            .and_then(|trial| trial.get(stage))
        else {
            return StepState::Unknown;
        };
        let error = step.get("error");
        match step.get("state").and_then(Value::as_str) {
            Some("not_attempted") if error.is_none_or(Value::is_null) => StepState::NotAttempted,
            Some("succeeded") if error.is_none_or(Value::is_null) => StepState::Succeeded,
            Some("failed") if error.is_some_and(Value::is_string) => StepState::Failed,
            _ => StepState::Unknown,
        }
    }

    /// 返回原始逐项证据；Set 结果、读取错误与未知值均不做成功归并。
    pub(crate) fn parameter_checks(&self) -> Option<&[Value]> {
        self.finalized_writer
            .get("parameter_checks")?
            .as_array()
            .map(Vec::as_slice)
    }

    /// 仅判断同一属性的请求值与回读值是否一致，不表示设置成功或驱动支持该参数。
    /// 重复属性、读取失败、缺字段以及记录与数值矛盾都保留未知。
    pub(crate) fn parameter_value_matches(&self, key: &str) -> Option<bool> {
        let mut matches = self.parameter_checks()?.iter().filter(|check| {
            check
                .get("request")
                .and_then(|request| request.get("key"))
                .and_then(Value::as_str)
                == Some(key)
        });
        let check = matches.next()?;
        if matches.next().is_some() || !check.get("read_error")?.is_null() {
            return None;
        }
        let requested = check.get("request")?.get("value")?.as_u64()?;
        let observed = check.get("observed")?.as_u64()?;
        let requested = u32::try_from(requested).ok()?;
        let observed = u32::try_from(observed).ok()?;
        let recorded = check.get("matches_request")?.as_bool()?;
        let equal = requested == observed;
        (recorded == equal).then_some(equal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recording::output_plan::PlannedEmbeddedAudio,
        recording_encoder_policy::{
            AttemptFailure, EncoderAttempt, EncoderRequirement, FailureCause, FailureStage,
            RetryContext,
        },
        recording_options::Preset,
    };
    use serde_json::json;
    use std::path::Path;
    use windows::Win32::Media::MediaFoundation::{
        MFMediaType_Audio, MFVideoFormat_AV1, MFVideoFormat_H264, MFVideoFormat_HEVC,
    };

    fn report(snapshot: Value) -> SessionEncodingReport {
        SessionEncodingReport::from_finalized_writer(
            EncoderPolicy::Auto,
            QualityProfile::Current(Preset::CompactMp4),
            Some(1),
            snapshot,
        )
    }

    fn codec_snapshot(codec: VideoCodec) -> Value {
        let subtype = match codec {
            VideoCodec::H264 => MFVideoFormat_H264,
            VideoCodec::Hevc => MFVideoFormat_HEVC,
            VideoCodec::Av1 => MFVideoFormat_AV1,
        };
        json!({
            "observed_video_codec_matches_request": true,
            "encoding_trial": {"inspection": {"video": {
                "expected_codec": codec.id(),
                "outputs": [
                    {"stream_id": 0, "major_type": format!("{:?}", MFMediaType_Video),
                        "subtype": format!("{:?}", subtype)},
                    {"stream_id": 7, "major_type": format!("{:?}", MFMediaType_Video),
                        "subtype": format!("{:?}", subtype)}
                ]
            }}}
        })
    }

    #[test]
    fn recording_encoding_report_initial_output_requires_complete_actual_codec_evidence() {
        let mut snapshot = codec_snapshot(VideoCodec::H264);
        for output in snapshot["encoding_trial"]["inspection"]["video"]["outputs"]
            .as_array_mut()
            .unwrap()
        {
            output["size"] = json!([384, 256]);
            output["fps"] = json!([30000, 1001]);
            output["colorimetry"] = json!({"nominal_range": 2, "read_errors": {}});
        }
        let mut report = report(snapshot);
        use crate::{
            geometry::ScreenRect,
            recording::scale_plan::{self, ResizeRequest},
        };
        let scale = scale_plan::resolve(
            ScreenRect {
                left: -100,
                top: -50,
                width: 385,
                height: 257,
            },
            Preset::CompactMp4,
            Some(ResizeRequest {
                target_width: 192,
                target_height: 128,
            }),
            true,
        )
        .unwrap()
        .unwrap();
        report = report.with_requested_scale(Some(scale));
        assert_eq!(report.requested_scale, Some(scale));
        assert_eq!(
            serde_json::to_value(&report).unwrap()["requested_scale"],
            serde_json::to_value(scale).unwrap()
        );
        let observed = report.initial_video_output_observation();
        // 请求目标是192×128，也不能把真实初始化观测的384×256改写成目标值。
        assert_eq!(observed.size, Some((384, 256)));
        assert_eq!(observed.fps, Some((30000, 1001)));
        assert_eq!(observed.colorimetry.nominal_range, Some(2));

        report.finalized_writer["observed_video_codec_matches_request"] = false.into();
        assert_eq!(
            report.initial_video_output_observation(),
            video_observation::VideoOutputObservation::default()
        );
        report.finalized_writer["observed_video_codec_matches_request"] = true.into();
        report.finalized_writer["encoding_trial"]["inspection"]["video"]["outputs"][1]["subtype"] =
            format!("{:?}", MFVideoFormat_HEVC).into();
        assert_eq!(
            report.initial_video_output_observation(),
            video_observation::VideoOutputObservation::default()
        );
        // 请求里仍有已知尺寸，也不能补充未知或不完整的实际链。
        report.finalized_writer["output_request"] = json!({"width": 384, "height": 256});
        assert!(report.initial_video_output_observation().size.is_none());
        let cleared = report.clone().with_requested_scale(None);
        assert_eq!(cleared.requested_scale, None);
        assert_eq!(report.requested_scale, Some(scale));
        assert_eq!(
            cleared.initial_video_output_observation(),
            report.initial_video_output_observation()
        );
    }

    #[test]
    fn recording_encoding_report_preserves_codec_output_and_writer_audio_requests_as_requests() {
        let selected = CodecAttempt {
            slot: 2,
            codec: VideoCodec::H264,
            attempt_index: 0,
            attempt: EncoderAttempt {
                input: VideoInput::GpuNv12,
                requirement: EncoderRequirement::Any,
            },
        };
        let failed_attempt = CodecAttempt {
            slot: 0,
            codec: VideoCodec::Hevc,
            ..selected
        };
        let failures = vec![CodecAttemptFailure {
            attempt: failed_attempt,
            failure: AttemptFailure {
                attempt_index: failed_attempt.attempt_index,
                attempt: failed_attempt.attempt,
                stage: FailureStage::WriterCreation,
                cause: FailureCause::Backend {
                    message: "encoder unavailable".into(),
                },
                context: RetryContext::default(),
            },
        }];
        let plan = OutputPlan::new(
            Preset::CompactMp4,
            &[VideoCodec::Hevc, VideoCodec::H264],
            true,
            Path::new("recording.mp4"),
        )
        .unwrap();
        let snapshot = json!({
            "output_request": {"codec": "hevc"},
            "hardware": true, "input": "gpu_nv12", "name": "HEVC encoder"
        });
        let original = report(snapshot.clone());
        let explicit = original
            .clone()
            .with_codec_evidence(
                CodecRequest::HevcWithH264Fallback,
                Some(selected),
                failures.clone(),
            )
            .with_output_plan(Some(plan.clone()))
            .with_writer_audio_requested(false);
        assert_eq!(
            explicit.requested_codec,
            Some(CodecRequest::HevcWithH264Fallback)
        );
        assert_eq!(explicit.selected_codec_attempt, Some(selected));
        assert_eq!(explicit.codec_failures, Some(failures.clone()));
        assert_eq!(explicit.output_plan, Some(plan.clone()));
        assert_eq!(explicit.writer_audio_requested, Some(false));
        assert!(plan.capture_audio_requested);
        assert_eq!(plan.planned_embedded_audio, PlannedEmbeddedAudio::Aac);
        assert_eq!(explicit.confirmed_video_codec(), None);
        assert_eq!(explicit.finalized_writer, snapshot);
        assert_eq!(original.requested_codec, None);
        assert_eq!(original.codec_failures, None);
        assert_eq!(original.output_plan, None);
        assert_eq!(original.writer_audio_requested, None);
        let serialized = serde_json::to_value(&explicit).unwrap();
        assert_eq!(serialized["requested_codec"], "hevc_with_h264_fallback");
        assert_eq!(
            serialized["selected_codec_attempt"],
            serde_json::to_value(selected).unwrap()
        );
        assert_eq!(
            serialized["codec_failures"],
            serde_json::to_value(&failures).unwrap()
        );
        assert_eq!(
            serialized["output_plan"],
            serde_json::to_value(&plan).unwrap()
        );
        assert_eq!(serialized["writer_audio_requested"], false);
        assert_eq!(explicit.clone().with_output_plan(None).output_plan, None);

        let observed = report(codec_snapshot(VideoCodec::H264)).with_codec_evidence(
            CodecRequest::HevcWithH264Fallback,
            Some(selected),
            failures,
        );
        assert_eq!(observed.confirmed_video_codec(), Some(VideoCodec::H264));
        let no_failures = report(Value::Null).with_codec_evidence(CodecRequest::H264, None, vec![]);
        assert_eq!(no_failures.codec_failures, Some(vec![]));
        assert_eq!(no_failures.confirmed_video_codec(), None);
        assert_eq!(
            report(Value::Null)
                .with_writer_audio_requested(true)
                .writer_audio_requested,
            Some(true)
        );
    }

    #[test]
    fn recording_encoding_report_confirmed_codec_requires_complete_unique_matching_outputs() {
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            assert_eq!(
                report(codec_snapshot(codec)).confirmed_video_codec(),
                Some(codec)
            );
        }
        let base = codec_snapshot(VideoCodec::H264);
        for (pointer, replacement) in [
            ("/observed_video_codec_matches_request", json!(false)),
            ("/observed_video_codec_matches_request", Value::Null),
            ("/observed_video_codec_matches_request", json!("true")),
            (
                "/encoding_trial/inspection/video/expected_codec",
                Value::Null,
            ),
            (
                "/encoding_trial/inspection/video/expected_codec",
                json!("unknown"),
            ),
            (
                "/encoding_trial/inspection/video/expected_codec",
                json!("aac"),
            ),
            (
                "/encoding_trial/inspection/video/expected_codec",
                json!("hevc"),
            ),
            (
                "/encoding_trial/inspection/video/expected_codec",
                json!("av1"),
            ),
            ("/encoding_trial/inspection/video/outputs", json!([])),
            ("/encoding_trial/inspection/video/outputs", Value::Null),
            (
                "/encoding_trial/inspection/video/outputs/0/major_type",
                Value::Null,
            ),
            (
                "/encoding_trial/inspection/video/outputs/1/major_type",
                json!(format!("{:?}", MFMediaType_Audio)),
            ),
            (
                "/encoding_trial/inspection/video/outputs/0/subtype",
                Value::Null,
            ),
            (
                "/encoding_trial/inspection/video/outputs/0/subtype",
                json!("H264"),
            ),
            (
                "/encoding_trial/inspection/video/outputs/1/subtype",
                json!(format!("{:?}", MFVideoFormat_HEVC)),
            ),
            (
                "/encoding_trial/inspection/video/outputs/1/subtype",
                json!("unknown"),
            ),
            (
                "/encoding_trial/inspection/video/outputs/1/stream_id",
                json!(0),
            ),
            (
                "/encoding_trial/inspection/video/outputs/0/stream_id",
                json!(-1),
            ),
            (
                "/encoding_trial/inspection/video/outputs/0/stream_id",
                json!(u64::from(u32::MAX) + 1),
            ),
            (
                "/encoding_trial/inspection/video/outputs/0/stream_id",
                Value::Null,
            ),
        ] {
            let mut invalid = base.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert_eq!(report(invalid).confirmed_video_codec(), None, "{pointer}");
        }
        let mut missing_major = base.clone();
        missing_major
            .pointer_mut("/encoding_trial/inspection/video/outputs/0")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("major_type");
        assert_eq!(report(missing_major).confirmed_video_codec(), None);
        let mut missing_complete_observation = base;
        missing_complete_observation
            .as_object_mut()
            .unwrap()
            .remove("observed_video_codec_matches_request");
        assert_eq!(
            report(missing_complete_observation).confirmed_video_codec(),
            None
        );
        let mut mixed_av1 = codec_snapshot(VideoCodec::Av1);
        mixed_av1["encoding_trial"]["inspection"]["video"]["outputs"][1]["subtype"] =
            json!(format!("{:?}", MFVideoFormat_H264));
        assert_eq!(report(mixed_av1).confirmed_video_codec(), None);
    }

    #[test]
    fn recording_encoding_report_actual_identity_does_not_come_from_legacy_flags_or_names() {
        let actual = report(json!({
            "hardware": true, "input": "gpu_nv12", "name": "legacy hardware name",
            "encoding_trial": {"inspection": {"input": "cpu_rgb32", "video": {
                "encoder": {"kind": "unknown", "name": "reported name"}
            }}}
        }));
        assert_eq!(actual.input(), Some(VideoInput::CpuRgb32));
        assert_eq!(actual.video_encoder_kind(), EncoderKind::Unknown);
        assert_eq!(actual.video_encoder_name(), Some("reported name"));
        let missing = report(json!({"hardware": true, "input": "gpu_nv12", "name": "legacy"}));
        assert_eq!(missing.input(), None);
        assert_eq!(missing.video_encoder_kind(), EncoderKind::Unknown);
        assert_eq!(missing.video_encoder_name(), None);
        let observed = report(json!({"hardware": false, "name": "legacy software name",
            "encoding_trial": {"inspection": {"input": "gpu_nv12", "video": {
                "encoder": {"kind": "hardware", "name": "actual name", "hardware_url": null}
            }}}
        }));
        assert_eq!(observed.input(), Some(VideoInput::GpuNv12));
        assert_eq!(observed.video_encoder_kind(), EncoderKind::Hardware);
        assert_eq!(observed.video_encoder_name(), Some("actual name"));
        let software = report(json!({"encoding_trial": {"inspection": {"video": {
            "encoder": {"kind": "software", "hardware_url": null}
        }}}}));
        assert_eq!(software.video_encoder_kind(), EncoderKind::Software);
        let conflicting = report(json!({"encoding_trial": {"inspection": {"video": {
            "encoder": {"kind": "software", "hardware_url": "device"}
        }}}}));
        assert_eq!(conflicting.video_encoder_kind(), EncoderKind::Unknown);
        let failed_read = report(json!({"encoding_trial": {"inspection": {"video": {
            "encoder": {"kind": "hardware", "name": "name", "error": "read failed"}
        }}}}));
        assert_eq!(failed_read.video_encoder_kind(), EncoderKind::Unknown);
        assert_eq!(failed_read.video_encoder_name(), None);
    }

    #[test]
    fn recording_encoding_report_preserves_snapshot_and_never_claims_measured_payload() {
        let snapshot = json!({"bytes": 1234, "output_request": {"avg_bitrate_bps": 1_000_000},
            "actual_payload_bitrate_bps": 42, "encoding_trial": {"finalize": {"state": "succeeded"}}});
        let actual = report(snapshot.clone());
        assert_eq!(actual.finalized_writer, snapshot);
        assert_eq!(actual.requested_quality, None);
        assert_eq!(actual.requested_scale, None);
        assert_eq!(actual.requested_codec, None);
        assert_eq!(actual.selected_codec_attempt, None);
        assert_eq!(actual.codec_failures, None);
        assert_eq!(actual.output_plan, None);
        assert_eq!(actual.writer_audio_requested, None);
        assert_eq!(actual.clone().selected_attempt, Some(1));
        assert_eq!(actual.payload, PayloadStatus::NotMeasured);
        let serialized = serde_json::to_value(&actual).unwrap();
        assert_eq!(serialized["payload"], json!({"state": "not_measured"}));
        assert_eq!(serialized["finalized_writer"], snapshot);
        assert!(serialized["requested_quality"].is_null());
        for field in [
            "requested_scale",
            "requested_codec",
            "selected_codec_attempt",
            "codec_failures",
            "output_plan",
            "writer_audio_requested",
        ] {
            assert!(serialized[field].is_null(), "{field}");
        }
        assert!(serialized["payload"]
            .get("actual_payload_bitrate_bps")
            .is_none());
        assert_eq!(actual.finalize_state(), StepState::Succeeded);
        assert_eq!(actual.reopen_state(), StepState::Unknown);

        use crate::recording_options::advanced::{resolve, Override, QualityOverrides};
        let request = resolve(
            960,
            540,
            30,
            actual.requested_profile,
            QualityOverrides {
                avg_bitrate_bps: Override::Set(2_000_000),
                ..QualityOverrides::default()
            },
        )
        .unwrap();
        let explicit = actual.clone().with_requested_quality(Some(request));
        assert_eq!(explicit.requested_quality, Some(request));
        assert_eq!(actual.requested_quality, None);
        assert_eq!(explicit.finalized_writer, snapshot);
        assert_eq!(
            serde_json::to_value(&explicit).unwrap()["requested_quality"],
            serde_json::to_value(request).unwrap()
        );
    }

    #[test]
    fn recording_encoding_report_stages_do_not_predict_success_from_other_steps() {
        let missing = report(json!({"finalize_ok": true, "encoding_trial": {
            "first_video_sample": {"state": "succeeded"}
        }}));
        assert_eq!(missing.finalize_state(), StepState::Unknown);
        for (step, expected) in [
            (json!({"state": "not_attempted"}), StepState::NotAttempted),
            (
                json!({"state": "failed", "error": "disk full"}),
                StepState::Failed,
            ),
            (json!({"state": "failed"}), StepState::Unknown),
            (
                json!({"state": "succeeded", "error": "contradiction"}),
                StepState::Unknown,
            ),
            (json!({"state": true}), StepState::Unknown),
        ] {
            assert_eq!(
                report(json!({"encoding_trial": {"finalize": step}})).finalize_state(),
                expected
            );
        }
        let finished = report(json!({"encoding_trial": {
            "finalize": {"state": "succeeded"}, "reopen": {"state": "not_attempted"}
        }}));
        assert_eq!(finished.finalize_state(), StepState::Succeeded);
        assert_eq!(finished.reopen_state(), StepState::NotAttempted);
    }

    #[test]
    fn recording_encoding_report_parameter_matching_keeps_failed_reads_and_conflicts_unknown() {
        let actual = report(json!({"parameter_checks": [
            {"request": {"key": "mean_bitrate", "value": 100, "set_succeeded": true},
                "observed": 200, "read_error": null, "matches_request": false},
            {"request": {"key": "gop_frames", "value": 120},
                "observed": 60, "read_error": null, "matches_request": true},
            {"request": {"key": "b_frames", "value": 0},
                "observed": 0, "read_error": "not implemented", "matches_request": true},
            {"request": {"key": "unchanged_value", "value": 0, "set_succeeded": false},
                "observed": 0, "read_error": null, "matches_request": true},
            {"request": {"key": "duplicate", "value": 1},
                "observed": 1, "read_error": null, "matches_request": true},
            {"request": {"key": "duplicate", "value": 1},
                "observed": 1, "read_error": null, "matches_request": true}
        ]}));
        assert_eq!(actual.parameter_value_matches("mean_bitrate"), Some(false));
        assert_eq!(actual.parameter_value_matches("gop_frames"), None);
        assert_eq!(actual.parameter_value_matches("b_frames"), None);
        assert_eq!(actual.parameter_value_matches("duplicate"), None);
        assert_eq!(actual.parameter_value_matches("missing"), None);
        // 值相同不证明 Set 成功；原始失败证据仍完整保留供调用方展示。
        assert_eq!(
            actual.parameter_value_matches("unchanged_value"),
            Some(true)
        );
        assert_eq!(
            actual.parameter_checks().unwrap()[3]["request"]["set_succeeded"],
            false
        );
    }
}
