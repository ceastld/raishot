//! 编码格式请求及有界回退的纯模型；不创建 MF、GPU、文件或录制会话。
//! 每个 codec 内复用现有编码策略，跨 codec 只在尚未接纳样本的安全阶段发生。

use crate::{
    recording_encoder_caps::VideoCodec,
    recording_encoder_policy::{
        AttemptFailure, AttemptSequence, EncoderAttempt, EncoderPlan, EncoderPolicy, FailureCause,
        FailureDecision, FailureStage, RetryContext, StopReason,
    },
    recording_options::{
        advanced::validate_resolved, Preset, QualityParameters, QualityPlan, QualityProfile,
    },
    Result,
};
use serde::Serialize;

const MAX_ATTEMPTS: usize = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodecRequest {
    #[default]
    H264,
    Hevc,
    HevcWithH264Fallback,
    /// 仅供独立技术试验，生产会话预检始终拒绝。
    Av1,
    Av1WithH264Fallback,
}

impl CodecRequest {
    fn codec_order(self) -> &'static [VideoCodec] {
        match self {
            Self::H264 => &[VideoCodec::H264],
            Self::Hevc => &[VideoCodec::Hevc],
            Self::HevcWithH264Fallback => &[VideoCodec::Hevc, VideoCodec::H264],
            Self::Av1 => &[VideoCodec::Av1],
            Self::Av1WithH264Fallback => &[VideoCodec::Av1, VideoCodec::H264],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CodecAttempt {
    /// 整轮计划内唯一的暂存身份；换 codec 不复用局部 attempt_index 对应的槽位。
    pub slot: usize,
    pub codec: VideoCodec,
    pub attempt_index: usize,
    pub attempt: EncoderAttempt,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CodecPlan {
    request: CodecRequest,
    encoder_plan: EncoderPlan,
    attempts: Vec<CodecAttempt>,
}

impl CodecPlan {
    pub(crate) fn new(
        request: CodecRequest,
        policy: EncoderPolicy,
        quality: QualityPlan,
    ) -> Result<Self> {
        validate_resolved(&quality)?;
        if request != CodecRequest::H264 {
            let codec = if matches!(
                request,
                CodecRequest::Av1 | CodecRequest::Av1WithH264Fallback
            ) {
                "AV1"
            } else {
                "HEVC"
            };
            if matches!(quality.profile, QualityProfile::Current(Preset::Gif)) {
                return Err(trf!(
                    "{codec} 编码请求不支持 GIF 中间片预设",
                    "{codec} requests do not support the GIF intermediate preset"
                )
                .into());
            }
            if quality.parameters != QualityParameters::default() {
                return Err(trf!(
                    "{codec} 编码请求暂不支持自定义编码参数，未自动丢弃请求",
                    "{codec} requests do not yet support custom encoder parameters; the request was not discarded"
                )
                .into());
            }
        }
        let encoder_plan = EncoderPlan::new(policy, quality);
        let codecs = request.codec_order();
        let count = encoder_plan.attempts().len() * codecs.len();
        if count == 0 || count > MAX_ATTEMPTS {
            return Err(tr!(
                "编码格式计划超过有界尝试数量",
                "The codec plan exceeds its bounded attempt count"
            )
            .into());
        }
        let mut attempts = Vec::with_capacity(count);
        for &codec in codecs {
            for (attempt_index, &attempt) in encoder_plan.attempts().iter().enumerate() {
                attempts.push(CodecAttempt {
                    slot: attempts.len(),
                    codec,
                    attempt_index,
                    attempt,
                });
            }
        }
        Ok(Self {
            request,
            encoder_plan,
            attempts,
        })
    }

    pub(crate) fn encoder_plan(&self) -> &EncoderPlan {
        &self.encoder_plan
    }

    pub(crate) fn request(&self) -> CodecRequest {
        self.request
    }

    pub(crate) fn codec_order(&self) -> &[VideoCodec] {
        self.request.codec_order()
    }

    pub(crate) fn attempts(&self) -> &[CodecAttempt] {
        &self.attempts
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CodecAttemptFailure {
    pub attempt: CodecAttempt,
    pub failure: AttemptFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum CodecDecision {
    /// 先释放旧 writer，再使用新 slot 创建自有暂存；不授权删除或覆盖裸路径。
    Retry {
        attempt: CodecAttempt,
        replay_first_video: bool,
        codec_changed: bool,
    },
    Stop {
        reason: StopReason,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CodecSequence {
    plan: CodecPlan,
    sequence: AttemptSequence,
    phase_offset: usize,
    slot: usize,
    failures: Vec<CodecAttemptFailure>,
    stopped: Option<StopReason>,
}

impl CodecSequence {
    pub(crate) fn new(plan: CodecPlan) -> Self {
        let sequence = AttemptSequence::new(plan.encoder_plan.clone());
        Self {
            plan,
            sequence,
            phase_offset: 0,
            slot: 0,
            failures: Vec::new(),
            stopped: None,
        }
    }

    pub(crate) fn current(&self) -> Option<CodecAttempt> {
        self.stopped
            .is_none()
            .then(|| self.plan.attempts[self.slot])
    }

    pub(crate) fn plan(&self) -> &CodecPlan {
        &self.plan
    }

    pub(crate) fn failures(&self) -> &[CodecAttemptFailure] {
        &self.failures
    }

    pub(crate) fn failed(
        &mut self,
        stage: FailureStage,
        cause: FailureCause,
        context: RetryContext,
    ) -> CodecDecision {
        if let Some(reason) = &self.stopped {
            return CodecDecision::Stop {
                reason: reason.clone(),
            };
        }
        let current = self.plan.attempts[self.slot];
        let decision = self.sequence.failed(stage, cause, context);
        let failure = self
            .sequence
            .failures()
            .last()
            .expect("活动父序列必须记录本次失败")
            .clone();
        let change_codec = match &decision {
            FailureDecision::Stop { reason } => self.can_change_codec(current, reason, &failure),
            FailureDecision::Retry { .. } => false,
        };
        self.failures.push(CodecAttemptFailure {
            attempt: current,
            failure,
        });
        match decision {
            FailureDecision::Retry {
                attempt_index,
                attempt,
                replay_first_video,
            } => {
                self.slot = self.phase_offset + attempt_index;
                let next = self.plan.attempts[self.slot];
                debug_assert_eq!(next.attempt, attempt);
                CodecDecision::Retry {
                    attempt: next,
                    replay_first_video,
                    codec_changed: false,
                }
            }
            FailureDecision::Stop { .. } if change_codec => {
                // 每个 codec 独立消费同一份编码计划，H.264 从自己的局部尝试 0 开始。
                self.phase_offset = self.plan.encoder_plan.attempts().len();
                self.slot = self.phase_offset;
                self.sequence = AttemptSequence::new(self.plan.encoder_plan.clone());
                CodecDecision::Retry {
                    attempt: self.plan.attempts[self.slot],
                    replay_first_video: stage == FailureStage::FirstVideoSample,
                    codec_changed: true,
                }
            }
            FailureDecision::Stop { reason } => {
                self.stopped = Some(reason.clone());
                CodecDecision::Stop { reason }
            }
        }
    }

    fn can_change_codec(
        &self,
        current: CodecAttempt,
        reason: &StopReason,
        failure: &AttemptFailure,
    ) -> bool {
        if !matches!(
            (self.plan.request, current.codec),
            (CodecRequest::HevcWithH264Fallback, VideoCodec::Hevc)
                | (CodecRequest::Av1WithH264Fallback, VideoCodec::Av1)
        ) || failure.context.accepted_video_samples != 0
            || failure.context.accepted_audio_samples != 0
            || matches!(
                &failure.cause,
                FailureCause::Output { .. } | FailureCause::Cancelled
            )
        {
            return false;
        }
        let stage_allowed = match failure.stage {
            FailureStage::InputInitialization
            | FailureStage::WriterCreation
            | FailureStage::MediaTypes
            | FailureStage::BeginWriting
            | FailureStage::EncoderObservation => true,
            FailureStage::FirstVideoSample => failure.context.first_video_sample_retained,
            _ => false,
        };
        if !stage_allowed {
            return false;
        }
        match reason {
            StopReason::Exhausted => true,
            // Auto 的同 codec 首帧停止保持原样；只有显式 codec 回退可以跨到 H.264。
            StopReason::StageCannotRetry {
                stage: FailureStage::FirstVideoSample,
            } => {
                self.plan.encoder_plan.policy() == EncoderPolicy::Auto
                    && failure.stage == FailureStage::FirstVideoSample
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_options::quality_plan;

    fn quality() -> QualityPlan {
        let mut quality =
            quality_plan(1920, 1080, 30, QualityProfile::Current(Preset::CompactMp4)).unwrap();
        quality.bitrate_bps = 2_345_678;
        quality
    }

    fn backend() -> FailureCause {
        FailureCause::Backend {
            message: "encoder rejected the request".into(),
        }
    }

    fn fallback(request: CodecRequest, policy: EncoderPolicy) -> CodecSequence {
        CodecSequence::new(CodecPlan::new(request, policy, quality()).unwrap())
    }

    fn at_last_initial_codec_attempt(
        request: CodecRequest,
        policy: EncoderPolicy,
    ) -> CodecSequence {
        let mut sequence = fallback(request, policy);
        let last = sequence.plan().encoder_plan().attempts().len() - 1;
        while sequence.current().unwrap().attempt_index < last {
            assert!(matches!(
                sequence.failed(
                    FailureStage::WriterCreation,
                    backend(),
                    RetryContext::default()
                ),
                CodecDecision::Retry {
                    codec_changed: false,
                    ..
                }
            ));
        }
        sequence
    }

    #[test]
    fn recording_codec_policy_flattens_all_policies_and_preserves_one_quality_plan() {
        assert_eq!(CodecRequest::default(), CodecRequest::H264);
        for policy in EncoderPolicy::ALL {
            for (request, codecs) in [
                (CodecRequest::H264, &[VideoCodec::H264][..]),
                (CodecRequest::Hevc, &[VideoCodec::Hevc][..]),
                (
                    CodecRequest::HevcWithH264Fallback,
                    &[VideoCodec::Hevc, VideoCodec::H264][..],
                ),
                (CodecRequest::Av1, &[VideoCodec::Av1][..]),
                (
                    CodecRequest::Av1WithH264Fallback,
                    &[VideoCodec::Av1, VideoCodec::H264][..],
                ),
            ] {
                let plan = CodecPlan::new(request, policy, quality()).unwrap();
                assert_eq!(plan.request(), request);
                assert_eq!(plan.codec_order(), codecs);
                assert_eq!(plan.encoder_plan().policy(), policy);
                assert_eq!(plan.encoder_plan().quality(), quality());
                let local = plan.encoder_plan().attempts();
                assert_eq!(plan.attempts().len(), local.len() * codecs.len());
                assert!(plan.attempts().len() <= MAX_ATTEMPTS);
                for (slot, attempt) in plan.attempts().iter().enumerate() {
                    assert_eq!(attempt.slot, slot);
                    assert_eq!(attempt.codec, codecs[slot / local.len()]);
                    assert_eq!(attempt.attempt_index, slot % local.len());
                    assert_eq!(attempt.attempt, local[attempt.attempt_index]);
                }
            }
        }
    }

    #[test]
    fn recording_codec_policy_rejects_non_h264_gif_and_tuning_without_discarding_requests() {
        let gif = quality_plan(1920, 1080, 30, QualityProfile::Current(Preset::Gif)).unwrap();
        let tuned = quality_plan(1920, 1080, 30, QualityProfile::TextDetail).unwrap();
        for request in [
            CodecRequest::Hevc,
            CodecRequest::HevcWithH264Fallback,
            CodecRequest::Av1,
            CodecRequest::Av1WithH264Fallback,
        ] {
            assert!(CodecPlan::new(request, EncoderPolicy::Auto, gif).is_err());
            assert!(CodecPlan::new(request, EncoderPolicy::Auto, tuned).is_err());
        }
        for quality in [gif, tuned] {
            let plan = CodecPlan::new(CodecRequest::H264, EncoderPolicy::Auto, quality).unwrap();
            assert_eq!(plan.encoder_plan().quality(), quality);
        }
        let mut invalid = quality();
        invalid.width += 1;
        for request in [
            CodecRequest::H264,
            CodecRequest::Hevc,
            CodecRequest::HevcWithH264Fallback,
            CodecRequest::Av1,
            CodecRequest::Av1WithH264Fallback,
        ] {
            assert!(CodecPlan::new(request, EncoderPolicy::Auto, invalid).is_err());
        }
    }

    #[test]
    fn recording_codec_policy_uses_parent_phase_decisions_before_default_context_codec_fallback() {
        for request in [
            CodecRequest::HevcWithH264Fallback,
            CodecRequest::Av1WithH264Fallback,
        ] {
            for policy in EncoderPolicy::ALL {
                for stage in [
                    FailureStage::InputInitialization,
                    FailureStage::WriterCreation,
                    FailureStage::MediaTypes,
                    FailureStage::BeginWriting,
                    FailureStage::EncoderObservation,
                ] {
                    let mut sequence = fallback(request, policy);
                    let initial_codec = sequence.current().unwrap().codec;
                    let mut parent = AttemptSequence::new(sequence.plan().encoder_plan().clone());
                    let local_count = parent.plan().attempts().len();
                    for index in 0..local_count {
                        let expected = parent.failed(stage, backend(), RetryContext::default());
                        let actual = sequence.failed(stage, backend(), RetryContext::default());
                        match expected {
                            FailureDecision::Retry {
                                attempt_index,
                                attempt,
                                replay_first_video,
                            } => {
                                assert_eq!(
                                    actual,
                                    CodecDecision::Retry {
                                        attempt: CodecAttempt {
                                            slot: attempt_index,
                                            codec: initial_codec,
                                            attempt_index,
                                            attempt
                                        },
                                        replay_first_video,
                                        codec_changed: false,
                                    }
                                );
                            }
                            FailureDecision::Stop { reason } => {
                                assert_eq!(reason, StopReason::Exhausted);
                                assert_eq!(index + 1, local_count);
                                assert_eq!(
                                    actual,
                                    CodecDecision::Retry {
                                        attempt: sequence.plan().attempts()[local_count],
                                        replay_first_video: false,
                                        codec_changed: true,
                                    }
                                );
                            }
                        }
                        let logged = sequence.failures().last().unwrap();
                        assert_eq!(logged.attempt.slot, index);
                        assert_eq!(logged.attempt.codec, initial_codec);
                        assert_eq!(&logged.failure, &parent.failures()[index]);
                    }
                    assert_eq!(sequence.current().unwrap().codec, VideoCodec::H264);
                    assert_eq!(sequence.plan().encoder_plan().quality(), quality());
                }
            }
        }
    }

    #[test]
    fn recording_codec_policy_first_frame_replay_changes_codec_once_without_expanding_auto() {
        for request in [
            CodecRequest::HevcWithH264Fallback,
            CodecRequest::Av1WithH264Fallback,
        ] {
            let retained = RetryContext {
                first_video_sample_retained: true,
                ..Default::default()
            };
            let mut sequence = fallback(request, EncoderPolicy::Auto);
            let h264_slot = sequence.plan().encoder_plan().attempts().len();
            assert_eq!(
                sequence.failed(FailureStage::FirstVideoSample, backend(), retained),
                CodecDecision::Retry {
                    attempt: sequence.plan().attempts()[h264_slot],
                    replay_first_video: true,
                    codec_changed: true
                }
            );
            assert_eq!(sequence.current().unwrap().attempt_index, 0);
            assert_eq!(
                sequence.failed(FailureStage::FirstVideoSample, backend(), retained),
                CodecDecision::Stop {
                    reason: StopReason::StageCannotRetry {
                        stage: FailureStage::FirstVideoSample
                    }
                }
            );
            assert_eq!(sequence.failures().len(), 2);

            for policy in EncoderPolicy::ALL {
                let mut missing = fallback(request, policy);
                assert!(matches!(
                    missing.failed(
                        FailureStage::FirstVideoSample,
                        backend(),
                        RetryContext::default()
                    ),
                    CodecDecision::Stop { .. }
                ));
                assert_eq!(missing.failures().len(), 1);
                assert!(missing.current().is_none());
            }
            let mut preferred = fallback(request, EncoderPolicy::HardwarePreferred);
            let count = preferred.plan().encoder_plan().attempts().len();
            for index in 0..count {
                assert!(
                    matches!(preferred.failed(FailureStage::FirstVideoSample, backend(), retained),
                CodecDecision::Retry { replay_first_video: true, codec_changed, .. }
                    if codec_changed == (index + 1 == count))
                );
            }
        }
    }

    #[test]
    fn recording_codec_policy_output_cancel_samples_and_tail_failures_never_change_codec() {
        let retained = RetryContext {
            first_video_sample_retained: true,
            ..Default::default()
        };
        let mut cases = vec![
            (
                FailureStage::WriterCreation,
                FailureCause::Output {
                    message: "disk full".into(),
                },
                RetryContext::default(),
                StopReason::OutputFailure,
            ),
            (
                FailureStage::WriterCreation,
                FailureCause::Cancelled,
                RetryContext::default(),
                StopReason::Cancelled,
            ),
        ];
        for (video, audio) in [(1, 0), (0, 1)] {
            cases.push((
                FailureStage::FirstVideoSample,
                backend(),
                RetryContext {
                    accepted_video_samples: video,
                    accepted_audio_samples: audio,
                    ..retained
                },
                StopReason::SamplesAlreadyAccepted { video, audio },
            ));
        }
        for stage in [
            FailureStage::FirstAudioSample,
            FailureStage::Recording,
            FailureStage::Finalize,
            FailureStage::Reopen,
        ] {
            cases.push((
                stage,
                backend(),
                retained,
                StopReason::StageCannotRetry { stage },
            ));
        }
        for request in [
            CodecRequest::HevcWithH264Fallback,
            CodecRequest::Av1WithH264Fallback,
        ] {
            for (stage, cause, context, reason) in cases.clone() {
                let mut sequence =
                    at_last_initial_codec_attempt(request, EncoderPolicy::HardwarePreferred);
                let initial_codec = sequence.current().unwrap().codec;
                let before = sequence.failures().len();
                assert_eq!(
                    sequence.failed(stage, cause, context),
                    CodecDecision::Stop { reason }
                );
                assert_eq!(sequence.failures().len(), before + 1);
                assert_eq!(
                    sequence.failures().last().unwrap().attempt.codec,
                    initial_codec
                );
                assert!(sequence.current().is_none());
            }
        }
    }

    #[test]
    fn recording_codec_policy_exhaustion_and_terminal_reports_keep_a_bounded_real_history() {
        for request in [
            CodecRequest::H264,
            CodecRequest::Hevc,
            CodecRequest::HevcWithH264Fallback,
            CodecRequest::Av1,
            CodecRequest::Av1WithH264Fallback,
        ] {
            let mut sequence = CodecSequence::new(
                CodecPlan::new(request, EncoderPolicy::HardwarePreferred, quality()).unwrap(),
            );
            let count = sequence.plan().attempts().len();
            for slot in 0..count {
                assert_eq!(sequence.current().unwrap().slot, slot);
                let decision = sequence.failed(
                    FailureStage::InputInitialization,
                    backend(),
                    RetryContext::default(),
                );
                if slot + 1 == count {
                    assert_eq!(
                        decision,
                        CodecDecision::Stop {
                            reason: StopReason::Exhausted
                        }
                    );
                } else {
                    assert!(matches!(decision, CodecDecision::Retry { .. }));
                }
            }
            let history = sequence.failures().to_vec();
            assert_eq!(history.len(), count);
            assert!(history.len() <= MAX_ATTEMPTS);
            for _ in 0..8 {
                assert_eq!(
                    sequence.failed(
                        FailureStage::Recording,
                        FailureCause::Cancelled,
                        RetryContext::default()
                    ),
                    CodecDecision::Stop {
                        reason: StopReason::Exhausted
                    }
                );
                assert_eq!(sequence.failures(), history.as_slice());
                assert!(sequence.current().is_none());
            }
        }
    }
}
