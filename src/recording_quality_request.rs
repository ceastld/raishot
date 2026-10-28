//! 会话内的画质请求覆盖；只解析请求，不探测驱动、不写配置，也不改变基线预设。
//! 继承、移除可选请求和显式设置分别保留，不能用最终的 None 反推用户意图。

use super::{quality_plan, GopRequest, QualityPlan, QualityProfile, RateControl};
use crate::Result;
use serde::Serialize;

pub(crate) const MIN_AVG_BITRATE_BPS: u32 = 1_000_000;
pub(crate) const MAX_AVG_BITRATE_BPS: u32 = 80_000_000;
pub(crate) const MAX_PEAK_BITRATE_BPS: u32 = 160_000_000;
pub(crate) const MAX_BUFFER_BITS: u32 = 160_000_000;
const SCOPE: &str = "session request model only; the base quality plan still owns size/fps validation and even-dimension cropping; inherited and explicit requests remain distinguishable, including same-value sets and clears of absent fields; clear removes an optional request and encoder_default withdraws an explicit rate-control request, neither proves a driver feature is disabled; quality versus speed supports inheritance or an explicit 0..100 request, not constant-quality encoding or withdrawal, and does not establish driver support or effective quality; GOP inheritance preserves the product default, Clear withdraws the request without restoring the product default, and positive maximum picture counts are bounded by a project experiment budget of 60 seconds at the requested fps, not a driver capability or an exact keyframe interval; no driver acceptance, effective bitrate, quality improvement, persistence, writer or UI integration is claimed; base profile, experimental marker and low-latency requests are retained";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum Override<T> {
    #[default]
    Inherit,
    Set(T),
}

impl<T: Copy> Override<T> {
    fn apply(self, inherited: T) -> T {
        match self {
            Self::Inherit => inherited,
            Self::Set(value) => value,
        }
    }

    fn requested(self) -> bool {
        matches!(self, Self::Set(_))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum OptionalOverride<T> {
    #[default]
    Inherit,
    Clear,
    Set(T),
}

impl<T: Copy> OptionalOverride<T> {
    fn apply(self, inherited: Option<T>) -> Option<T> {
        match self {
            Self::Inherit => inherited,
            Self::Clear => None,
            Self::Set(value) => Some(value),
        }
    }

    fn requested(self) -> bool {
        !matches!(self, Self::Inherit)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct QualityOverrides {
    pub avg_bitrate_bps: Override<u32>,
    /// Set(EncoderDefault) 撤回模式请求，不保证编码器停止自己的码率控制。
    pub rate_control: Override<RateControl>,
    /// 质量/速度权衡：Inherit 保留原 Option，Set 请求 0..100；不提供撤回语义。
    pub quality_vs_speed: Override<u32>,
    pub max_bitrate_bps: OptionalOverride<u32>,
    /// 逻辑单位保留 bit；显式请求必须可精确转换为整 byte，不自动取整。
    pub buffer_bits: OptionalOverride<u32>,
    /// Inherit 保留产品默认，Clear 撤回 GOP 请求，Set 为最大 picture 数量。
    pub gop_frames: OptionalOverride<u32>,
}

impl QualityOverrides {
    pub(crate) fn custom_requested(self) -> bool {
        self.avg_bitrate_bps.requested()
            || self.rate_control.requested()
            || self.quality_vs_speed.requested()
            || self.max_bitrate_bps.requested()
            || self.buffer_bits.requested()
            || self.gop_frames.requested()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResolvedQualityRequest {
    pub base_profile: QualityProfile,
    pub overrides: QualityOverrides,
    pub plan: QualityPlan,
    pub custom_requested: bool,
    pub scope: &'static str,
}

/// 使用基线的尺寸/fps 校验与裁偶数；覆盖值冲突或超预算就拒绝，不钳制或补值。
pub(crate) fn resolve(
    width: u32,
    height: u32,
    fps: u32,
    base: QualityProfile,
    overrides: QualityOverrides,
) -> Result<ResolvedQualityRequest> {
    let mut plan = quality_plan(width, height, fps, base)?;
    plan.bitrate_bps = overrides.avg_bitrate_bps.apply(plan.bitrate_bps);
    plan.parameters.rate_control = overrides.rate_control.apply(plan.parameters.rate_control);
    plan.parameters.quality_vs_speed = match overrides.quality_vs_speed {
        Override::Inherit => plan.parameters.quality_vs_speed,
        Override::Set(value) => Some(value),
    };
    plan.parameters.max_bitrate_bps = overrides
        .max_bitrate_bps
        .apply(plan.parameters.max_bitrate_bps);
    plan.parameters.buffer_bits = overrides.buffer_bits.apply(plan.parameters.buffer_bits);
    plan.parameters.gop_request = match overrides.gop_frames {
        OptionalOverride::Inherit => plan.parameters.gop_request,
        OptionalOverride::Clear => GopRequest::EncoderDefault,
        OptionalOverride::Set(frames) => GopRequest::Frames(frames),
    };
    validate_resolved(&plan)?;
    Ok(ResolvedQualityRequest {
        base_profile: base,
        overrides,
        plan,
        custom_requested: overrides.custom_requested(),
        scope: SCOPE,
    })
}

/// 接线前可独立检查最终计划；不修正传入值，也不将请求有效性当成驱动支持。
pub(crate) fn validate_resolved(plan: &QualityPlan) -> Result<()> {
    // 复用原校验。已有最终计划若为奇数边则直接拒绝，不能二次裁剪后掩盖调用错误。
    let checked_geometry = quality_plan(plan.width, plan.height, plan.fps, plan.profile)?;
    if checked_geometry.width != plan.width || checked_geometry.height != plan.height {
        return Err(tr!(
            "最终画质计划的宽高必须为已校验的偶数，未自动裁剪",
            "The resolved quality plan must have validated even dimensions; it was not cropped"
        )
        .into());
    }
    if !(MIN_AVG_BITRATE_BPS..=MAX_AVG_BITRATE_BPS).contains(&plan.bitrate_bps) {
        return Err(tr!(
            "平均码率请求须在 1–80 Mbps 内，未自动调整",
            "The average bitrate request must be within 1–80 Mbps; it was not adjusted"
        )
        .into());
    }
    if let Some(maximum) = plan.parameters.max_bitrate_bps {
        if maximum < plan.bitrate_bps || maximum > MAX_PEAK_BITRATE_BPS {
            return Err(tr!(
                "峰值码率须不低于平均码率且不超过 160 Mbps，未自动调整",
                "Peak bitrate must be at least the average and at most 160 Mbps; it was not adjusted"
            ).into());
        }
    }
    if plan.parameters.rate_control == RateControl::PeakConstrainedVbr
        && plan.parameters.max_bitrate_bps.is_none()
    {
        return Err(tr!(
            "峰值受限 VBR 需要显式或继承的峰值码率；清除峰值时请同时改变控制模式",
            "Peak-constrained VBR requires an explicit or inherited peak bitrate; change the mode when clearing the peak"
        ).into());
    }
    if let Some(bits) = plan.parameters.buffer_bits {
        if !(8..=MAX_BUFFER_BITS).contains(&bits) || !bits.is_multiple_of(8) {
            return Err(tr!(
                "缓冲请求须在 8–160000000 bit 内且为 8 的倍数；移除请求请使用清除，未自动调整",
                "The buffer request must be within 8–160000000 bits and a multiple of 8; use Clear to remove it; it was not adjusted"
            ).into());
        }
    }
    if plan
        .parameters
        .quality_vs_speed
        .is_some_and(|speed| speed > 100)
    {
        return Err(tr!(
            "质量与速度权衡请求须在 0–100 内，未自动调整",
            "The quality versus speed request must be within 0–100; it was not adjusted"
        )
        .into());
    }
    // 不在解析 DTO 时启用产品默认；显式 Frames 仍独立验证正值和预算。
    plan.parameters
        .gop_request
        .requested_frames(plan.fps, false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Preset;
    use super::*;

    fn current() -> QualityProfile {
        QualityProfile::Current(Preset::CompactMp4)
    }

    #[test]
    fn recording_quality_request_inheritance_keeps_the_entire_base_plan() {
        for base in Preset::ALL
            .into_iter()
            .map(QualityProfile::Current)
            .chain(QualityProfile::EXPERIMENTS)
        {
            let resolved = resolve(1921, 1081, 30, base, QualityOverrides::default()).unwrap();
            let original = quality_plan(1921, 1081, 30, base).unwrap();
            assert_eq!(resolved.plan, original);
            assert_eq!((resolved.plan.width, resolved.plan.height), (1920, 1080));
            assert_eq!(resolved.base_profile, base);
            assert!(!resolved.custom_requested);
            assert_eq!(resolved.overrides, QualityOverrides::default());
            for value in [0, 100] {
                let overrides = QualityOverrides {
                    quality_vs_speed: Override::Set(value),
                    ..Default::default()
                };
                let explicit = resolve(1921, 1081, 30, base, overrides).unwrap();
                let mut expected = original;
                expected.parameters.quality_vs_speed = Some(value);
                // 除此请求外保留低延迟、码率、GOP、尺寸与实验标记。
                assert_eq!(explicit.plan, expected);
                assert_eq!(explicit.overrides, overrides);
                assert!(explicit.custom_requested);
                if original.parameters.quality_vs_speed == Some(value) {
                    assert_eq!(explicit.plan, resolved.plan);
                    assert_ne!(explicit.custom_requested, resolved.custom_requested);
                }
                let encoded = serde_json::to_value(explicit).unwrap();
                assert_eq!(encoded["overrides"]["quality_vs_speed"]["kind"], "set");
                assert_eq!(encoded["overrides"]["quality_vs_speed"]["value"], value);
            }
        }
    }

    #[test]
    fn recording_quality_request_clear_set_and_inherit_keep_distinct_intent() {
        let base = quality_plan(1920, 1080, 30, current()).unwrap();
        let inherit = resolve(1920, 1080, 30, current(), QualityOverrides::default()).unwrap();
        let clear = resolve(
            1920,
            1080,
            30,
            current(),
            QualityOverrides {
                buffer_bits: OptionalOverride::Clear,
                ..Default::default()
            },
        )
        .unwrap();
        let same_value = resolve(
            1920,
            1080,
            30,
            current(),
            QualityOverrides {
                avg_bitrate_bps: Override::Set(base.bitrate_bps),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(clear.plan, inherit.plan);
        assert_eq!(same_value.plan, inherit.plan);
        assert!(clear.custom_requested && same_value.custom_requested);
        assert!(!inherit.custom_requested);
        let encoded = serde_json::to_value(clear).unwrap();
        assert_eq!(encoded["overrides"]["buffer_bits"]["kind"], "clear");
        assert_eq!(encoded["overrides"]["max_bitrate_bps"]["kind"], "inherit");
        let set = resolve(
            1920,
            1080,
            30,
            current(),
            QualityOverrides {
                buffer_bits: OptionalOverride::Set(8),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(set.plan.parameters.buffer_bits, Some(8));
        let reset = resolve(
            1920,
            1080,
            30,
            QualityProfile::TextDetail,
            QualityOverrides {
                rate_control: Override::Set(RateControl::EncoderDefault),
                max_bitrate_bps: OptionalOverride::Clear,
                buffer_bits: OptionalOverride::Clear,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            reset.plan.parameters.rate_control,
            RateControl::EncoderDefault
        );
        assert_eq!(reset.plan.parameters.max_bitrate_bps, None);
        assert_eq!(reset.plan.parameters.buffer_bits, None);
    }

    #[test]
    fn recording_quality_request_conflicts_do_not_invent_or_adjust_other_fields() {
        assert!(resolve(
            1920,
            1080,
            30,
            QualityProfile::TextDetail,
            QualityOverrides {
                max_bitrate_bps: OptionalOverride::Clear,
                ..Default::default()
            }
        )
        .is_err());
        assert!(resolve(
            1920,
            1080,
            30,
            current(),
            QualityOverrides {
                rate_control: Override::Set(RateControl::PeakConstrainedVbr),
                ..Default::default()
            }
        )
        .is_err());
        let base = quality_plan(960, 540, 30, QualityProfile::SmallFile).unwrap();
        assert!(resolve(
            960,
            540,
            30,
            QualityProfile::SmallFile,
            QualityOverrides {
                avg_bitrate_bps: Override::Set(base.parameters.max_bitrate_bps.unwrap() + 1),
                ..Default::default()
            }
        )
        .is_err());
        let request = QualityOverrides {
            avg_bitrate_bps: Override::Set(10_000_000),
            rate_control: Override::Set(RateControl::PeakConstrainedVbr),
            quality_vs_speed: Override::Set(73),
            max_bitrate_bps: OptionalOverride::Set(20_000_000),
            buffer_bits: OptionalOverride::Set(30_000_000),
            gop_frames: OptionalOverride::Inherit,
        };
        let original = request;
        let original_base = quality_plan(1920, 1080, 30, QualityProfile::TextDetail).unwrap();
        let resolved = resolve(1920, 1080, 30, QualityProfile::TextDetail, request).unwrap();
        assert_eq!(request, original);
        assert_eq!(
            quality_plan(1920, 1080, 30, QualityProfile::TextDetail).unwrap(),
            original_base
        );
        assert_eq!(resolved.plan.bitrate_bps, 10_000_000);
        assert_eq!(resolved.plan.parameters.max_bitrate_bps, Some(20_000_000));
        assert_eq!(resolved.plan.parameters.buffer_bits, Some(30_000_000));
        assert_eq!(resolved.plan.parameters.quality_vs_speed, Some(73));
        assert_eq!(
            resolved.plan.parameters.low_latency,
            original_base.parameters.low_latency
        );
        assert_eq!(resolved.plan.experimental, original_base.experimental);
    }

    #[test]
    fn recording_quality_request_budgets_reject_out_of_range_without_clamping() {
        for value in [101, u32::MAX] {
            let overrides = QualityOverrides {
                quality_vs_speed: Override::Set(value),
                ..Default::default()
            };
            let original = overrides;
            assert!(resolve(960, 540, 30, current(), overrides).is_err());
            assert_eq!(overrides, original);
        }
        for value in [
            0,
            MIN_AVG_BITRATE_BPS - 1,
            MAX_AVG_BITRATE_BPS + 1,
            u32::MAX,
        ] {
            assert!(resolve(
                960,
                540,
                30,
                current(),
                QualityOverrides {
                    avg_bitrate_bps: Override::Set(value),
                    ..Default::default()
                }
            )
            .is_err());
        }
        for value in [MIN_AVG_BITRATE_BPS, MAX_AVG_BITRATE_BPS] {
            assert_eq!(
                resolve(
                    960,
                    540,
                    30,
                    current(),
                    QualityOverrides {
                        avg_bitrate_bps: Override::Set(value),
                        ..Default::default()
                    }
                )
                .unwrap()
                .plan
                .bitrate_bps,
                value
            );
        }
        for value in [
            0,
            1,
            7,
            9,
            MAX_BUFFER_BITS + 1,
            MAX_BUFFER_BITS + 8,
            u32::MAX,
        ] {
            assert!(resolve(
                960,
                540,
                30,
                current(),
                QualityOverrides {
                    buffer_bits: OptionalOverride::Set(value),
                    ..Default::default()
                }
            )
            .is_err());
            let mut invalid = quality_plan(960, 540, 30, current()).unwrap();
            invalid.parameters.buffer_bits = Some(value);
            let original = invalid;
            assert!(validate_resolved(&invalid).is_err());
            assert_eq!(invalid, original);
        }
        for value in [8, 16, MAX_BUFFER_BITS] {
            let resolved = resolve(
                960,
                540,
                30,
                current(),
                QualityOverrides {
                    buffer_bits: OptionalOverride::Set(value),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(resolved.plan.parameters.buffer_bits, Some(value));
            assert!(validate_resolved(&resolved.plan).is_ok());
        }
        for value in [
            0,
            MIN_AVG_BITRATE_BPS - 1,
            MAX_PEAK_BITRATE_BPS + 1,
            u32::MAX,
        ] {
            assert!(resolve(
                2,
                2,
                5,
                current(),
                QualityOverrides {
                    max_bitrate_bps: OptionalOverride::Set(value),
                    ..Default::default()
                }
            )
            .is_err());
        }
        let boundary = resolve(
            2,
            2,
            5,
            current(),
            QualityOverrides {
                avg_bitrate_bps: Override::Set(MAX_AVG_BITRATE_BPS),
                rate_control: Override::Set(RateControl::PeakConstrainedVbr),
                quality_vs_speed: Override::Inherit,
                max_bitrate_bps: OptionalOverride::Set(MAX_PEAK_BITRATE_BPS),
                buffer_bits: OptionalOverride::Set(MAX_BUFFER_BITS),
                gop_frames: OptionalOverride::Inherit,
            },
        )
        .unwrap();
        assert_eq!(
            boundary.plan.parameters.max_bitrate_bps,
            Some(MAX_PEAK_BITRATE_BPS)
        );
        assert_eq!(boundary.plan.parameters.buffer_bits, Some(MAX_BUFFER_BITS));
    }

    #[test]
    fn recording_quality_request_final_validation_rejects_bad_geometry_and_leaves_input_unchanged()
    {
        for (width, height, fps) in [
            (0, 540, 30),
            (960, 1, 30),
            (960, 540, 0),
            (960, 540, 29),
            (u32::MAX, 540, 30),
        ] {
            assert!(resolve(width, height, fps, current(), QualityOverrides::default()).is_err());
        }
        let valid = quality_plan(960, 540, 30, current()).unwrap();
        for bad in [
            QualityPlan {
                width: 961,
                ..valid
            },
            QualityPlan {
                height: 541,
                ..valid
            },
            QualityPlan { width: 0, ..valid },
            QualityPlan { fps: 0, ..valid },
            QualityPlan { fps: 29, ..valid },
            QualityPlan {
                bitrate_bps: 0,
                ..valid
            },
        ] {
            let original = bad;
            assert!(validate_resolved(&bad).is_err());
            assert_eq!(bad, original);
        }
        assert!(validate_resolved(&valid).is_ok());
        for value in [101, u32::MAX] {
            let mut invalid_speed = valid;
            invalid_speed.parameters.quality_vs_speed = Some(value);
            let original = invalid_speed;
            assert!(validate_resolved(&invalid_speed).is_err());
            assert_eq!(invalid_speed, original);
        }
    }

    #[test]
    fn recording_quality_request_gop_inherit_clear_and_same_effective_frames_keep_intent() {
        for base in [
            current(),
            QualityProfile::TextDetail,
            QualityProfile::Motion,
        ] {
            let original = quality_plan(960, 540, 30, base).unwrap();
            let inherited = resolve(960, 540, 30, base, QualityOverrides::default()).unwrap();
            assert_eq!(
                inherited.plan.parameters.gop_request,
                GopRequest::ProductDefault
            );
            assert_eq!(inherited.plan, original);
            for (overrides, expected) in [
                (OptionalOverride::Clear, GopRequest::EncoderDefault),
                (OptionalOverride::Set(120), GopRequest::Frames(120)),
            ] {
                let request = QualityOverrides {
                    gop_frames: overrides,
                    ..Default::default()
                };
                let resolved = resolve(960, 540, 30, base, request).unwrap();
                assert!(resolved.custom_requested);
                assert_eq!(resolved.overrides.gop_frames, overrides);
                let mut expected_plan = original;
                expected_plan.parameters.gop_request = expected;
                assert_eq!(resolved.plan, expected_plan);
                assert_eq!(quality_plan(960, 540, 30, base).unwrap(), original);
                if overrides == OptionalOverride::Clear {
                    assert_eq!(
                        resolved
                            .plan
                            .parameters
                            .gop_request
                            .requested_frames(30, true)
                            .unwrap(),
                        None
                    );
                    let encoded = serde_json::to_value(resolved).unwrap();
                    assert_eq!(encoded["overrides"]["gop_frames"]["kind"], "clear");
                    assert_eq!(
                        encoded["plan"]["parameters"]["gop_request"]["kind"],
                        "encoder_default"
                    );
                } else {
                    assert_ne!(resolved.plan, inherited.plan);
                    assert_eq!(
                        resolved
                            .plan
                            .parameters
                            .gop_request
                            .requested_frames(30, true)
                            .unwrap(),
                        Some(120)
                    );
                }
            }
        }
    }

    #[test]
    fn recording_quality_request_gop_budget_rejects_without_clamping_or_mutating() {
        for frames in [0, 1801, u32::MAX] {
            let overrides = QualityOverrides {
                gop_frames: OptionalOverride::Set(frames),
                ..Default::default()
            };
            let original = overrides;
            assert!(resolve(960, 540, 30, current(), overrides).is_err());
            assert_eq!(overrides, original);
            let mut plan = quality_plan(960, 540, 30, current()).unwrap();
            plan.parameters.gop_request = GopRequest::Frames(frames);
            let original = plan;
            assert!(validate_resolved(&plan).is_err());
            assert_eq!(plan, original);
        }
        for frames in [1, 1800] {
            let resolved = resolve(
                960,
                540,
                30,
                current(),
                QualityOverrides {
                    gop_frames: OptionalOverride::Set(frames),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                resolved.plan.parameters.gop_request,
                GopRequest::Frames(frames)
            );
            assert!(validate_resolved(&resolved.plan).is_ok());
        }
    }
}
