//! 视频编码策略的纯计划与回退守卫，不创建 writer、GPU 或文件。
//! 像素输入路线与视频编码器硬软分开；音频编码器不受视频严格策略约束。
#[path = "recording_codec_policy.rs"]
pub(crate) mod codec;

use crate::{
    recording_encoder_caps::{EncoderKind, VideoInput},
    recording_options::QualityPlan,
};
use serde::Serialize;
use std::fmt;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EncoderPolicy {
    #[default]
    Auto,
    HardwarePreferred,
    HardwareOnly,
    SoftwareOnly,
}
impl EncoderPolicy {
    pub(crate) const ALL: [Self; 4] = [
        Self::Auto,
        Self::HardwarePreferred,
        Self::HardwareOnly,
        Self::SoftwareOnly,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => tr!("自动", "Auto"),
            Self::HardwarePreferred => tr!("硬件优先", "Hardware first"),
            Self::HardwareOnly => tr!("仅硬件", "Hardware only"),
            Self::SoftwareOnly => tr!("仅软件", "Software only"),
        }
    }

    pub(crate) fn explanation(self) -> &'static str {
        match self {
            Self::Auto => tr!(
                "沿用自动输入与兼容回退。",
                "Use automatic input and compatibility fallback."
            ),
            Self::HardwarePreferred => tr!(
                "先尝试硬件，再回退到已确认的软件编码器。",
                "Try hardware, then verified software."
            ),
            Self::HardwareOnly => tr!(
                "无法验证硬件编码器时停止录制。",
                "Stop if hardware encoding cannot be verified."
            ),
            Self::SoftwareOnly => tr!(
                "仅使用已确认的软件编码器。",
                "Require a verified software encoder."
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EncoderRequirement {
    Any,
    Hardware,
    Software,
}

impl EncoderRequirement {
    /// 只能传入实际视频编码器的观测；目录候选、GPU 输入或允许硬件开关不能代替。
    pub(crate) fn check(self, actual: EncoderKind) -> Result<(), ObservationRejection> {
        match (self, actual) {
            (Self::Any, _)
            | (Self::Hardware, EncoderKind::Hardware)
            | (Self::Software, EncoderKind::Software) => Ok(()),
            (_, EncoderKind::Unknown) => Err(ObservationRejection::Unknown { required: self }),
            _ => Err(ObservationRejection::Mismatch {
                required: self,
                actual,
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub(crate) enum ObservationRejection {
    Unknown {
        required: EncoderRequirement,
    },
    Mismatch {
        required: EncoderRequirement,
        actual: EncoderKind,
    },
}

impl fmt::Display for ObservationRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unknown { .. } => tr!(
                "无法验证实际视频编码器，当前严格编码策略不能继续",
                "Cannot verify the video encoder required by this policy"
            ),
            Self::Mismatch {
                required: EncoderRequirement::Hardware,
                ..
            } => tr!(
                "实际视频编码器不是硬件编码器",
                "The video encoder is not a hardware encoder"
            ),
            Self::Mismatch { .. } => tr!(
                "实际视频编码器不是软件编码器",
                "The video encoder is not a software encoder"
            ),
        };
        formatter.write_str(message)
    }
}
impl std::error::Error for ObservationRejection {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct EncoderAttempt {
    pub input: VideoInput,
    pub requirement: EncoderRequirement,
}
impl EncoderAttempt {
    /// 这是创建 writer 时的请求，仍须 check 实际视频编码器。
    pub(crate) fn allow_hardware_transforms(self) -> bool {
        self.requirement != EncoderRequirement::Software
    }

    pub(crate) fn check(self, actual: EncoderKind) -> Result<(), ObservationRejection> {
        self.requirement.check(actual)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EncoderPlan {
    policy: EncoderPolicy,
    /// 所有尝试共享一份设置；回退不能悄悄降帧率、尺寸、码率或改变调参。
    quality: QualityPlan,
    attempts: Vec<EncoderAttempt>,
}
impl EncoderPlan {
    /// quality 应来自 recording_options::quality_plan 的已校验结果。
    pub(crate) fn new(policy: EncoderPolicy, quality: QualityPlan) -> Self {
        use EncoderRequirement::{Any, Hardware, Software};
        use VideoInput::{CpuRgb32, GpuNv12};
        let attempts = match policy {
            // 保持现有行为：GPU 输入失败才尝试 CPU，允许实际编码器自行选择。
            EncoderPolicy::Auto => vec![
                EncoderAttempt {
                    input: GpuNv12,
                    requirement: Any,
                },
                EncoderAttempt {
                    input: CpuRgb32,
                    requirement: Any,
                },
            ],
            EncoderPolicy::HardwarePreferred => vec![
                EncoderAttempt {
                    input: GpuNv12,
                    requirement: Hardware,
                },
                EncoderAttempt {
                    input: CpuRgb32,
                    requirement: Hardware,
                },
                EncoderAttempt {
                    input: CpuRgb32,
                    requirement: Software,
                },
            ],
            EncoderPolicy::HardwareOnly => vec![
                EncoderAttempt {
                    input: GpuNv12,
                    requirement: Hardware,
                },
                EncoderAttempt {
                    input: CpuRgb32,
                    requirement: Hardware,
                },
            ],
            EncoderPolicy::SoftwareOnly => vec![EncoderAttempt {
                input: CpuRgb32,
                requirement: Software,
            }],
        };
        Self {
            policy,
            quality,
            attempts,
        }
    }

    pub(crate) fn policy(&self) -> EncoderPolicy {
        self.policy
    }

    pub(crate) fn quality(&self) -> QualityPlan {
        self.quality
    }

    pub(crate) fn attempts(&self) -> &[EncoderAttempt] {
        &self.attempts
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureStage {
    InputInitialization,
    WriterCreation,
    MediaTypes,
    BeginWriting,
    EncoderObservation,
    FirstVideoSample,
    FirstAudioSample,
    Recording,
    Finalize,
    Reopen,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FailureCause {
    Backend {
        message: String,
    },
    PolicyRejected {
        rejection: ObservationRejection,
    },
    /// 文件权限、磁盘满等输出错误不通过换编码器重试。
    Output {
        message: String,
    },
    Cancelled,
}

/// 只计 WriteSample 成功接受的样本；采集或排队帧不能算已提交。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct RetryContext {
    pub accepted_video_samples: u64,
    pub accepted_audio_samples: u64,
    /// 第一帧写失败后，调用方仍拥有可原样重放的帧与原始时间戳。
    pub first_video_sample_retained: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AttemptFailure {
    pub attempt_index: usize,
    pub attempt: EncoderAttempt,
    pub stage: FailureStage,
    pub cause: FailureCause,
    pub context: RetryContext,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub(crate) enum StopReason {
    Exhausted,
    SamplesAlreadyAccepted { video: u64, audio: u64 },
    FirstVideoSampleUnavailable,
    StageCannotRetry { stage: FailureStage },
    OutputFailure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum FailureDecision {
    /// 调用方须先释放旧 writer，并用会话自有的新暂存创建下一步；不授权删除裸路径。
    /// replay_first_video 为 true 时原帧与时间戳必须重放，不能跳过失败帧继续采集。
    Retry {
        attempt_index: usize,
        attempt: EncoderAttempt,
        replay_first_video: bool,
    },
    /// 已提交样本后的失败交现有收尾流程，不覆盖重录已有内容。
    Stop { reason: StopReason },
}

/// 有界尝试序列。只计算下一步，不执行重建或清理；终止后的重复报告不追加失败。
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AttemptSequence {
    plan: EncoderPlan,
    index: usize,
    failures: Vec<AttemptFailure>,
    stopped: Option<StopReason>,
}
impl AttemptSequence {
    pub(crate) fn new(plan: EncoderPlan) -> Self {
        Self {
            plan,
            index: 0,
            failures: Vec::new(),
            stopped: None,
        }
    }

    pub(crate) fn plan(&self) -> &EncoderPlan {
        &self.plan
    }

    pub(crate) fn current(&self) -> Option<EncoderAttempt> {
        self.stopped
            .is_none()
            .then(|| self.plan.attempts[self.index])
    }

    pub(crate) fn failures(&self) -> &[AttemptFailure] {
        &self.failures
    }

    pub(crate) fn failed(
        &mut self,
        stage: FailureStage,
        cause: FailureCause,
        context: RetryContext,
    ) -> FailureDecision {
        if let Some(reason) = &self.stopped {
            return FailureDecision::Stop {
                reason: reason.clone(),
            };
        }
        let attempt = self.plan.attempts[self.index];
        let stop = stop_reason(self.plan.policy, stage, &cause, context);
        self.failures.push(AttemptFailure {
            attempt_index: self.index,
            attempt,
            stage,
            cause,
            context,
        });
        let stop = stop.or_else(|| {
            (self.index + 1 >= self.plan.attempts.len()).then_some(StopReason::Exhausted)
        });
        if let Some(reason) = stop {
            self.stopped = Some(reason.clone());
            return FailureDecision::Stop { reason };
        }
        self.index += 1;
        FailureDecision::Retry {
            attempt_index: self.index,
            attempt: self.plan.attempts[self.index],
            replay_first_video: stage == FailureStage::FirstVideoSample,
        }
    }
}

fn stop_reason(
    policy: EncoderPolicy,
    stage: FailureStage,
    cause: &FailureCause,
    context: RetryContext,
) -> Option<StopReason> {
    match cause {
        FailureCause::Cancelled => return Some(StopReason::Cancelled),
        FailureCause::Output { .. } => return Some(StopReason::OutputFailure),
        _ => {}
    }
    if context.accepted_video_samples != 0 || context.accepted_audio_samples != 0 {
        return Some(StopReason::SamplesAlreadyAccepted {
            video: context.accepted_video_samples,
            audio: context.accepted_audio_samples,
        });
    }
    // Auto 保持现有创建期间的 GPU→CPU 回退，不扩大成首帧失败后自动重建。
    if policy == EncoderPolicy::Auto && stage == FailureStage::FirstVideoSample {
        return Some(StopReason::StageCannotRetry { stage });
    }
    match stage {
        FailureStage::InputInitialization
        | FailureStage::WriterCreation
        | FailureStage::MediaTypes
        | FailureStage::BeginWriting
        | FailureStage::EncoderObservation => None,
        FailureStage::FirstVideoSample if context.first_video_sample_retained => None,
        FailureStage::FirstVideoSample => Some(StopReason::FirstVideoSampleUnavailable),
        _ => Some(StopReason::StageCannotRetry { stage }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_options::{quality_plan, QualityProfile};

    fn plan(policy: EncoderPolicy) -> EncoderPlan {
        EncoderPlan::new(
            policy,
            quality_plan(1920, 1080, 30, QualityProfile::TextDetail).unwrap(),
        )
    }

    fn backend() -> FailureCause {
        FailureCause::Backend {
            message: "encoder rejected the request".into(),
        }
    }

    #[test]
    fn recording_encoder_policy_backend_rejects_invalid_request_before_file_or_gpu() {
        use crate::media_foundation::EncoderBackendSession;
        let backend = EncoderBackendSession::new(plan(EncoderPolicy::Auto), false);
        let invalid_index = backend.create_attempt(std::path::Path::new(""), usize::MAX, None);
        assert!(
            matches!(invalid_index, Err(error) if error.stage == FailureStage::InputInitialization)
        );
        let directory = crate::config_json::TestDir::new("encoder-existing");
        let protected = directory.join("protected.mp4");
        std::fs::write(&protected, b"keep original").unwrap();
        assert!(
            matches!(backend.create_attempt(&protected, 0, None), Err(error) if matches!(error.cause, FailureCause::Output { .. }))
        );
        assert_eq!(std::fs::read(&protected).unwrap(), b"keep original");
        let mut invalid_quality = backend.plan().quality();
        invalid_quality.width = 3;
        let backend = EncoderBackendSession::new(
            EncoderPlan::new(EncoderPolicy::Auto, invalid_quality),
            false,
        );
        assert!(
            matches!(backend.create_attempt(std::path::Path::new(""), 0, None), Err(error) if error.stage == FailureStage::InputInitialization)
        );
        invalid_quality.width = 1920;
        invalid_quality.parameters.max_bitrate_bps = Some(1);
        let backend = EncoderBackendSession::new(
            EncoderPlan::new(EncoderPolicy::Auto, invalid_quality),
            false,
        );
        assert!(
            matches!(backend.create_attempt(std::path::Path::new(""), 0, None), Err(error) if error.stage == FailureStage::InputInitialization)
        );
    }

    #[test]
    fn recording_encoder_policy_known_output_errors_do_not_become_backend_fallback() {
        use crate::media_foundation::classify_recording_error;
        for code in [
            windows::core::HRESULT(0x8007_0070_u32 as i32),
            windows::Win32::Foundation::STG_E_MEDIUMFULL,
        ] {
            let disk_full = windows::core::Error::from_hresult(code);
            let cause = classify_recording_error(&disk_full);
            assert!(matches!(cause, FailureCause::Output { .. }));
            let mut sequence = AttemptSequence::new(plan(EncoderPolicy::HardwarePreferred));
            assert!(matches!(
                sequence.failed(
                    FailureStage::FirstVideoSample,
                    cause,
                    RetryContext {
                        first_video_sample_retained: true,
                        ..Default::default()
                    }
                ),
                FailureDecision::Stop {
                    reason: StopReason::OutputFailure,
                    ..
                }
            ));
        }
        let driver =
            windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005_u32 as i32));
        assert!(matches!(
            classify_recording_error(&driver),
            FailureCause::Backend { .. }
        ));
    }

    #[test]
    fn recording_encoder_policy_auto_keeps_input_fallback() {
        let plan = plan(EncoderPolicy::Auto);
        assert_eq!(plan.policy(), EncoderPolicy::Auto);
        assert_eq!(
            plan.attempts(),
            &[
                EncoderAttempt {
                    input: VideoInput::GpuNv12,
                    requirement: EncoderRequirement::Any
                },
                EncoderAttempt {
                    input: VideoInput::CpuRgb32,
                    requirement: EncoderRequirement::Any
                },
            ]
        );
        assert!(plan
            .attempts()
            .iter()
            .all(|attempt| attempt.allow_hardware_transforms()));
        assert!(plan.attempts()[0].check(EncoderKind::Unknown).is_ok());
        let mut sequence = AttemptSequence::new(plan);
        assert_eq!(
            sequence.failed(
                FailureStage::FirstVideoSample,
                backend(),
                RetryContext {
                    first_video_sample_retained: true,
                    ..Default::default()
                }
            ),
            FailureDecision::Stop {
                reason: StopReason::StageCannotRetry {
                    stage: FailureStage::FirstVideoSample
                }
            }
        );
    }

    #[test]
    fn recording_encoder_policy_strict_observation_rejects_unknown_and_wrong_kind() {
        for (requirement, correct, wrong) in [
            (
                EncoderRequirement::Hardware,
                EncoderKind::Hardware,
                EncoderKind::Software,
            ),
            (
                EncoderRequirement::Software,
                EncoderKind::Software,
                EncoderKind::Hardware,
            ),
        ] {
            assert!(requirement.check(correct).is_ok());
            assert_eq!(
                requirement.check(EncoderKind::Unknown),
                Err(ObservationRejection::Unknown {
                    required: requirement
                })
            );
            assert_eq!(
                requirement.check(wrong),
                Err(ObservationRejection::Mismatch {
                    required: requirement,
                    actual: wrong
                })
            );
        }
        let hardware = plan(EncoderPolicy::HardwareOnly);
        assert!(hardware
            .attempts()
            .iter()
            .all(|attempt| attempt.requirement == EncoderRequirement::Hardware));
        let software = plan(EncoderPolicy::SoftwareOnly);
        assert_eq!(software.attempts().len(), 1);
        assert_eq!(software.attempts()[0].input, VideoInput::CpuRgb32);
        assert!(!software.attempts()[0].allow_hardware_transforms());
    }

    #[test]
    fn recording_encoder_policy_preferred_orders_hardware_before_software() {
        let plan = plan(EncoderPolicy::HardwarePreferred);
        assert_eq!(
            plan.attempts(),
            &[
                EncoderAttempt {
                    input: VideoInput::GpuNv12,
                    requirement: EncoderRequirement::Hardware
                },
                EncoderAttempt {
                    input: VideoInput::CpuRgb32,
                    requirement: EncoderRequirement::Hardware
                },
                EncoderAttempt {
                    input: VideoInput::CpuRgb32,
                    requirement: EncoderRequirement::Software
                },
            ]
        );
    }

    #[test]
    fn recording_encoder_policy_no_retry_after_any_sample_was_accepted() {
        for context in [
            RetryContext {
                accepted_video_samples: 1,
                ..Default::default()
            },
            RetryContext {
                accepted_audio_samples: 1,
                ..Default::default()
            },
        ] {
            let mut sequence = AttemptSequence::new(plan(EncoderPolicy::HardwarePreferred));
            assert_eq!(
                sequence.failed(FailureStage::FirstVideoSample, backend(), context),
                FailureDecision::Stop {
                    reason: StopReason::SamplesAlreadyAccepted {
                        video: context.accepted_video_samples,
                        audio: context.accepted_audio_samples
                    },
                }
            );
            assert!(sequence.current().is_none());
            assert_eq!(sequence.failures().len(), 1);
        }
    }

    #[test]
    fn recording_encoder_policy_first_frame_retry_requires_replayable_source() {
        let mut missing = AttemptSequence::new(plan(EncoderPolicy::HardwarePreferred));
        assert_eq!(
            missing.failed(
                FailureStage::FirstVideoSample,
                backend(),
                RetryContext::default()
            ),
            FailureDecision::Stop {
                reason: StopReason::FirstVideoSampleUnavailable
            }
        );
        let mut retained = AttemptSequence::new(plan(EncoderPolicy::HardwarePreferred));
        assert!(matches!(
            retained.failed(
                FailureStage::FirstVideoSample,
                backend(),
                RetryContext {
                    first_video_sample_retained: true,
                    ..Default::default()
                }
            ),
            FailureDecision::Retry {
                attempt_index: 1,
                replay_first_video: true,
                ..
            }
        ));
    }

    #[test]
    fn recording_encoder_policy_output_cancel_and_tail_failures_do_not_retry() {
        for (stage, cause, reason) in [
            (
                FailureStage::WriterCreation,
                FailureCause::Output {
                    message: "disk full".into(),
                },
                StopReason::OutputFailure,
            ),
            (
                FailureStage::MediaTypes,
                FailureCause::Cancelled,
                StopReason::Cancelled,
            ),
            (
                FailureStage::Recording,
                backend(),
                StopReason::StageCannotRetry {
                    stage: FailureStage::Recording,
                },
            ),
            (
                FailureStage::FirstAudioSample,
                backend(),
                StopReason::StageCannotRetry {
                    stage: FailureStage::FirstAudioSample,
                },
            ),
            (
                FailureStage::Finalize,
                backend(),
                StopReason::StageCannotRetry {
                    stage: FailureStage::Finalize,
                },
            ),
            (
                FailureStage::Reopen,
                backend(),
                StopReason::StageCannotRetry {
                    stage: FailureStage::Reopen,
                },
            ),
        ] {
            let mut sequence = AttemptSequence::new(plan(EncoderPolicy::Auto));
            assert_eq!(
                sequence.failed(stage, cause, RetryContext::default()),
                FailureDecision::Stop { reason }
            );
        }
    }

    #[test]
    fn recording_encoder_policy_fallback_is_bounded_and_keeps_parameters_and_failures() {
        for policy in [
            EncoderPolicy::Auto,
            EncoderPolicy::HardwarePreferred,
            EncoderPolicy::HardwareOnly,
            EncoderPolicy::SoftwareOnly,
        ] {
            let plan = plan(policy);
            let quality = plan.quality();
            let count = plan.attempts().len();
            let mut sequence = AttemptSequence::new(plan);
            for index in 0..count {
                assert!(sequence.current().is_some());
                assert_eq!(sequence.plan().quality(), quality);
                let cause = FailureCause::Backend {
                    message: format!("attempt {index} failed"),
                };
                let result = sequence.failed(
                    FailureStage::MediaTypes,
                    cause.clone(),
                    RetryContext::default(),
                );
                assert_eq!(sequence.failures()[index].cause, cause);
                assert_eq!(sequence.failures()[index].attempt_index, index);
                if index + 1 == count {
                    assert_eq!(
                        result,
                        FailureDecision::Stop {
                            reason: StopReason::Exhausted
                        }
                    );
                } else {
                    assert!(
                        matches!(result, FailureDecision::Retry { attempt_index, replay_first_video: false, .. } if attempt_index == index + 1)
                    );
                }
            }
            assert!(sequence.current().is_none());
            assert_eq!(
                sequence.failed(
                    FailureStage::InputInitialization,
                    backend(),
                    RetryContext::default()
                ),
                FailureDecision::Stop {
                    reason: StopReason::Exhausted
                }
            );
            assert_eq!(sequence.failures().len(), count);
        }
    }
}
