//! 本窗口的高级画质草稿；原文本与请求模式分开，解析失败不改草稿。
use crate::{
    geometry::ScreenRect,
    recording::scale_plan::{self, ResizeRequest},
    recording_options::{
        advanced::{self, OptionalOverride, Override, QualityOverrides, ResolvedQualityRequest},
        QualityProfile, RateControl, RecordingOptions,
    },
    Result,
};

const MAX_NUMBER_TEXT_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum NumberMode {
    #[default]
    Inherit,
    Set,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OptionalMode {
    #[default]
    Inherit,
    Clear,
    Set,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NumberDraft {
    pub mode: NumberMode,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResizeDraft {
    pub mode: NumberMode,
    pub width_text: String,
    pub height_text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OptionalNumberDraft {
    pub mode: OptionalMode,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct QualityDraft {
    pub enabled: bool,
    pub resize: ResizeDraft,
    pub mean_mbps: NumberDraft,
    pub quality_vs_speed: NumberDraft,
    pub peak_mbps: OptionalNumberDraft,
    /// 十进制 kbit，最多三位小数；不按 KiB 或 byte 解释。
    pub buffer_kbits: OptionalNumberDraft,
    /// 最大 picture 数量的请求，不承诺精确关键帧间隔。
    pub gop_frames: OptionalNumberDraft,
    pub rate_control: Override<RateControl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedDraftRequest {
    pub quality: ResolvedQualityRequest,
    pub resize: Option<ResizeRequest>,
}

impl QualityDraft {
    /// 保留旧接口；组合请求中的显式尺寸由使用 resolve_request 的调用方接线。
    pub(crate) fn resolve(
        &self,
        region: ScreenRect,
        options: &RecordingOptions,
        profile: Option<QualityProfile>,
        experimental: bool,
    ) -> Result<Option<ResolvedQualityRequest>> {
        self.resolve_request(region, options, profile, experimental)
            .map(|request| request.map(|request| request.quality))
    }

    /// 关闭、非实验或 GIF 时保留草稿但不解析；先校验缩放，再按目标解析质量。
    pub(crate) fn resolve_request(
        &self,
        region: ScreenRect,
        options: &RecordingOptions,
        profile: Option<QualityProfile>,
        experimental: bool,
    ) -> Result<Option<ResolvedDraftRequest>> {
        if !self.enabled || !experimental || options.preset.is_gif() {
            return Ok(None);
        }
        let resize = match self.resize.mode {
            NumberMode::Inherit => None,
            NumberMode::Set => Some(ResizeRequest {
                target_width: parse_integer(
                    &self.resize.width_text,
                    tr!("目标宽度", "Target width"),
                )?,
                target_height: parse_integer(
                    &self.resize.height_text,
                    tr!("目标高度", "Target height"),
                )?,
            }),
        };
        let plan = scale_plan::resolve(region, options.preset, resize, experimental)?;
        let (width, height) = plan.map_or((region.width, region.height), |plan| {
            (plan.geometry.target_width, plan.geometry.target_height)
        });
        self.resolve_at_size(width, height, options, profile, experimental)
            .map(|quality| quality.map(|quality| ResolvedDraftRequest { quality, resize }))
    }

    /// 只按已选输出宽高解析质量；不重读尺寸草稿，也不构造或移动物理区域。
    pub(crate) fn resolve_at_size(
        &self,
        width: u32,
        height: u32,
        options: &RecordingOptions,
        profile: Option<QualityProfile>,
        experimental: bool,
    ) -> Result<Option<ResolvedQualityRequest>> {
        if !self.enabled || !experimental || options.preset.is_gif() {
            return Ok(None);
        }
        let overrides = QualityOverrides {
            avg_bitrate_bps: match self.mean_mbps.mode {
                NumberMode::Inherit => Override::Inherit,
                NumberMode::Set => Override::Set(parse_fixed(
                    &self.mean_mbps.text,
                    6,
                    tr!("平均码率", "Mean bitrate"),
                )?),
            },
            rate_control: self.rate_control,
            quality_vs_speed: match self.quality_vs_speed.mode {
                NumberMode::Inherit => Override::Inherit,
                NumberMode::Set => Override::Set(parse_integer(
                    &self.quality_vs_speed.text,
                    tr!("质量/速度权衡", "Quality/speed tradeoff"),
                )?),
            },
            max_bitrate_bps: self
                .peak_mbps
                .resolve_fixed(6, tr!("峰值码率", "Peak bitrate"))?,
            buffer_bits: self
                .buffer_kbits
                .resolve_fixed(3, tr!("缓冲大小", "Buffer size"))?,
            gop_frames: match self.gop_frames.mode {
                OptionalMode::Inherit => OptionalOverride::Inherit,
                OptionalMode::Clear => OptionalOverride::Clear,
                OptionalMode::Set => OptionalOverride::Set(parse_integer(
                    &self.gop_frames.text,
                    tr!("GOP 最大帧数", "Maximum GOP frames"),
                )?),
            },
        };
        advanced::resolve(
            width,
            height,
            options.fps,
            profile.unwrap_or(QualityProfile::Current(options.preset)),
            overrides,
        )
        .map(Some)
    }
}

impl OptionalNumberDraft {
    fn resolve_fixed(&self, places: u32, field: &'static str) -> Result<OptionalOverride<u32>> {
        match self.mode {
            OptionalMode::Inherit => Ok(OptionalOverride::Inherit),
            OptionalMode::Clear => Ok(OptionalOverride::Clear),
            OptionalMode::Set => parse_fixed(&self.text, places, field).map(OptionalOverride::Set),
        }
    }
}

fn number_error(field: &str, reason: &str) -> Box<dyn std::error::Error> {
    trf!("{field}：{reason}", "{field}: {reason}").into()
}

fn check_text(text: &str, field: &str) -> Result<()> {
    if text.is_empty() || text.len() > MAX_NUMBER_TEXT_BYTES {
        return Err(number_error(
            field,
            tr!(
                "显式值须为 1–64 个 ASCII 字符，未自动调整",
                "An explicit value requires 1–64 ASCII characters; it was not adjusted"
            ),
        ));
    }
    Ok(())
}

fn parse_digits(text: &str, field: &str) -> Result<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(number_error(
            field,
            tr!(
                "只接受 ASCII 十进制数字，不接受符号、空白、指数或逗号",
                "Use ASCII decimal digits without signs, whitespace, exponents or commas"
            ),
        ));
    }
    text.bytes().try_fold(0u32, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or_else(|| overflow_error(field))
    })
}

fn overflow_error(field: &str) -> Box<dyn std::error::Error> {
    number_error(
        field,
        tr!(
            "数值超出请求的整数范围，未自动调整",
            "The value exceeds the request integer range; it was not adjusted"
        ),
    )
}

fn parse_integer(text: &str, field: &str) -> Result<u32> {
    check_text(text, field)?;
    parse_digits(text, field)
}

/// 只做精确十进制定点单位换算；请求预算、缓冲对齐和字段间冲突由 advanced::resolve 检查。
fn parse_fixed(text: &str, places: u32, field: &str) -> Result<u32> {
    check_text(text, field)?;
    let (whole, fractional) = match text.split_once('.') {
        Some((whole, fractional)) => (whole, Some(fractional)),
        None => (text, None),
    };
    let whole = parse_digits(whole, field)?;
    let scale = 10u32
        .checked_pow(places)
        .ok_or_else(|| overflow_error(field))?;
    let fraction = match fractional {
        Some(text) => {
            if text.len() > places as usize {
                return Err(number_error(
                    field,
                    &trf!(
                        "最多允许 {places} 位小数，未自动取整",
                        "At most {places} decimal places are allowed; it was not rounded"
                    ),
                ));
            }
            let value = parse_digits(text, field)?;
            let padding = places - text.len() as u32;
            value
                .checked_mul(
                    10u32
                        .checked_pow(padding)
                        .ok_or_else(|| overflow_error(field))?,
                )
                .ok_or_else(|| overflow_error(field))?
        }
        None => 0,
    };
    whole
        .checked_mul(scale)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(|| overflow_error(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_options::{quality_plan, GopRequest, Preset};

    fn region() -> ScreenRect {
        ScreenRect {
            left: -10,
            top: -20,
            width: 961,
            height: 541,
        }
    }

    fn options() -> RecordingOptions {
        RecordingOptions {
            fps: 30,
            ..RecordingOptions::default()
        }
    }

    fn enabled() -> QualityDraft {
        QualityDraft {
            enabled: true,
            ..QualityDraft::default()
        }
    }

    #[test]
    fn recording_quality_resize_draft_inherit_and_inactive_modes_preserve_bad_text() {
        let mut draft = enabled();
        draft.resize.width_text = "invalid width".into();
        draft.resize.height_text = "invalid height".repeat(8);
        let original = draft.clone();
        let request = draft
            .resolve_request(region(), &options(), None, true)
            .unwrap()
            .unwrap();
        assert!(request.resize.is_none());
        assert_eq!(
            (request.quality.plan.width, request.quality.plan.height),
            (960, 540)
        );
        assert_eq!(draft, original);

        for (enabled, experimental, preset) in [
            (false, true, Preset::CompactMp4),
            (true, false, Preset::CompactMp4),
            (true, true, Preset::Gif),
        ] {
            let mut inactive = draft.clone();
            inactive.enabled = enabled;
            inactive.resize.mode = NumberMode::Set;
            let original = inactive.clone();
            let options = RecordingOptions {
                fps: 0,
                preset,
                ..options()
            };
            let invalid_region = ScreenRect {
                width: 0,
                ..region()
            };
            assert!(inactive
                .resolve_request(invalid_region, &options, None, experimental)
                .unwrap()
                .is_none());
            assert!(inactive
                .resolve_at_size(0, 0, &options, None, experimental)
                .unwrap()
                .is_none());
            assert_eq!(inactive, original);
        }
    }

    #[test]
    fn recording_quality_resize_draft_requires_exact_paired_integer_targets() {
        let region = ScreenRect {
            width: 385,
            height: 257,
            ..region()
        };
        for (width, height) in [
            ("", "128"),
            ("192", ""),
            (" 192", "128"),
            ("192 ", "128"),
            ("+192", "128"),
            ("192.0", "128"),
            ("1e2", "128"),
            ("１９２", "128"),
            ("192", "1,28"),
            ("4294967296", "128"),
            ("0", "128"),
            ("1", "128"),
            ("193", "128"),
            ("192", "129"),
            ("386", "128"),
            ("192", "258"),
            ("4294967295", "128"),
        ] {
            let mut draft = enabled();
            draft.resize = ResizeDraft {
                mode: NumberMode::Set,
                width_text: width.into(),
                height_text: height.into(),
            };
            let original = draft.clone();
            assert!(
                draft
                    .resolve_request(region, &options(), None, true)
                    .is_err(),
                "target={width}x{height}"
            );
            assert_eq!(draft, original);
        }
        let mut draft = enabled();
        draft.resize = ResizeDraft {
            mode: NumberMode::Set,
            width_text: "0".repeat(65),
            height_text: "128".into(),
        };
        let original = draft.clone();
        assert!(draft
            .resolve_request(region, &options(), None, true)
            .is_err());
        assert_eq!(draft, original);

        draft.resize.width_text = "0192".into();
        draft.resize.height_text = "00128".into();
        let original = draft.clone();
        assert_eq!(
            draft
                .resolve_request(region, &options(), None, true)
                .unwrap()
                .unwrap()
                .resize,
            Some(ResizeRequest {
                target_width: 192,
                target_height: 128
            })
        );
        assert_eq!(draft, original);
        let invalid_source = ScreenRect {
            left: i32::MAX,
            ..region
        };
        assert!(draft
            .resolve_request(invalid_source, &options(), None, true)
            .is_err());
        assert_eq!(draft, original);
    }

    #[test]
    fn recording_quality_resize_draft_recomputes_quality_for_the_exact_target() {
        let region = ScreenRect {
            left: -1921,
            top: -1081,
            width: 1921,
            height: 1081,
        };
        let mut draft = enabled();
        draft.resize = ResizeDraft {
            mode: NumberMode::Set,
            width_text: "0960".into(),
            height_text: "0540".into(),
        };
        draft.quality_vs_speed = NumberDraft {
            mode: NumberMode::Set,
            text: "073".into(),
        };
        let original = draft.clone();
        let options = options();
        let request = draft
            .resolve_request(region, &options, Some(QualityProfile::Scrolling), true)
            .unwrap()
            .unwrap();
        let resize = request.resize.unwrap();
        let plan = scale_plan::resolve(region, options.preset, Some(resize), true)
            .unwrap()
            .unwrap();
        assert_eq!((plan.source.left, plan.source.top), (-1921, -1081));
        assert_eq!(
            (plan.geometry.source_width, plan.geometry.source_height),
            (1920, 1080)
        );
        assert_eq!(
            (request.quality.plan.width, request.quality.plan.height),
            (960, 540)
        );
        assert_eq!(request.quality.plan.parameters.quality_vs_speed, Some(73));
        assert_ne!(
            request.quality.plan.bitrate_bps,
            quality_plan(
                region.width,
                region.height,
                options.fps,
                QualityProfile::Scrolling
            )
            .unwrap()
            .bitrate_bps
        );
        // 与启动前基于已验证 target 的重新 resolve 相同，不能拿源尺寸计划冒充。
        assert_eq!(
            request.quality,
            advanced::resolve(
                resize.target_width,
                resize.target_height,
                options.fps,
                request.quality.base_profile,
                request.quality.overrides,
            )
            .unwrap()
        );
        assert_eq!(
            draft
                .resolve_at_size(960, 540, &options, Some(QualityProfile::Scrolling), true)
                .unwrap(),
            Some(request.quality)
        );
        assert_eq!(
            draft
                .resolve(region, &options, Some(QualityProfile::Scrolling), true)
                .unwrap(),
            Some(request.quality)
        );
        assert_eq!(draft, original);

        draft.resize.width_text = "1920".into();
        draft.resize.height_text = "1080".into();
        let identity = draft
            .resolve_request(region, &options, None, true)
            .unwrap()
            .unwrap();
        assert_eq!(
            identity.resize,
            Some(ResizeRequest {
                target_width: 1920,
                target_height: 1080
            })
        );
    }

    #[test]
    fn recording_quality_resize_draft_recalculates_fps_and_profile_without_rewriting_text() {
        let mut draft = enabled();
        draft.resize = ResizeDraft {
            mode: NumberMode::Set,
            width_text: "0960".into(),
            height_text: "0540".into(),
        };
        draft.quality_vs_speed.text = "unused speed".into();
        let original = draft.clone();
        let mut bitrates = Vec::new();
        for (fps, profile) in [
            (30, QualityProfile::Current(Preset::CompactMp4)),
            (60, QualityProfile::Current(Preset::CompactMp4)),
            (60, QualityProfile::TextDetail),
        ] {
            let options = RecordingOptions { fps, ..options() };
            let request = draft
                .resolve_request(region(), &options, Some(profile), true)
                .unwrap()
                .unwrap();
            assert_eq!(
                request.resize,
                Some(ResizeRequest {
                    target_width: 960,
                    target_height: 540
                })
            );
            assert_eq!(request.quality.base_profile, profile);
            assert_eq!(request.quality.plan.fps, fps);
            assert_eq!(
                request.quality.plan,
                quality_plan(960, 540, fps, profile).unwrap()
            );
            bitrates.push(request.quality.plan.bitrate_bps);
            assert_eq!(draft, original);
        }
        assert!(bitrates.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn recording_quality_draft_converts_exact_decimal_units_and_preserves_text() {
        let draft = QualityDraft {
            enabled: true,
            resize: ResizeDraft::default(),
            mean_mbps: NumberDraft {
                mode: NumberMode::Set,
                text: "01.000001".into(),
            },
            quality_vs_speed: NumberDraft {
                mode: NumberMode::Set,
                text: "100".into(),
            },
            peak_mbps: OptionalNumberDraft {
                mode: OptionalMode::Set,
                text: "2.000002".into(),
            },
            buffer_kbits: OptionalNumberDraft {
                mode: OptionalMode::Set,
                text: "0.008".into(),
            },
            gop_frames: OptionalNumberDraft {
                mode: OptionalMode::Set,
                text: "6".into(),
            },
            rate_control: Override::Set(RateControl::PeakConstrainedVbr),
        };
        let original = draft.clone();
        let request = draft
            .resolve(region(), &options(), None, true)
            .unwrap()
            .unwrap();
        assert_eq!((request.plan.width, request.plan.height), (960, 540));
        assert_eq!(request.plan.bitrate_bps, 1_000_001);
        assert_eq!(request.plan.parameters.max_bitrate_bps, Some(2_000_002));
        assert_eq!(request.plan.parameters.buffer_bits, Some(8));
        assert_eq!(request.plan.parameters.gop_request, GopRequest::Frames(6));
        assert_eq!(request.plan.parameters.quality_vs_speed, Some(100));
        assert_eq!(request.overrides.quality_vs_speed, Override::Set(100));
        assert!(request.custom_requested);
        assert_eq!(draft, original);
    }

    #[test]
    fn recording_quality_draft_inherit_and_clear_ignore_unselected_text() {
        let mut draft = enabled();
        draft.mean_mbps.text = "x".repeat(65);
        draft.quality_vs_speed.text = "invalid speed".into();
        draft.peak_mbps.text = "invalid peak".into();
        draft.buffer_kbits.text = "invalid buffer".into();
        draft.gop_frames.text = "invalid GOP".into();
        let inherited = draft
            .resolve(region(), &options(), Some(QualityProfile::TextDetail), true)
            .unwrap()
            .unwrap();
        assert_eq!(
            inherited.plan,
            quality_plan(961, 541, 30, QualityProfile::TextDetail).unwrap()
        );
        assert!(!inherited.custom_requested);
        draft.rate_control = Override::Set(RateControl::EncoderDefault);
        draft.peak_mbps.mode = OptionalMode::Clear;
        draft.buffer_kbits.mode = OptionalMode::Clear;
        draft.gop_frames.mode = OptionalMode::Clear;
        let original = draft.clone();
        let cleared = draft
            .resolve(region(), &options(), Some(QualityProfile::TextDetail), true)
            .unwrap()
            .unwrap();
        assert_eq!(cleared.overrides.max_bitrate_bps, OptionalOverride::Clear);
        assert_eq!(cleared.overrides.buffer_bits, OptionalOverride::Clear);
        assert_eq!(cleared.overrides.gop_frames, OptionalOverride::Clear);
        assert_eq!(cleared.plan.parameters.max_bitrate_bps, None);
        assert_eq!(cleared.plan.parameters.buffer_bits, None);
        assert_eq!(
            cleared.plan.parameters.gop_request,
            GopRequest::EncoderDefault
        );
        assert!(cleared.custom_requested);
        assert_eq!(draft, original);
        draft.quality_vs_speed.mode = NumberMode::Set;
        draft.quality_vs_speed.text = "0".into();
        let zero = draft
            .resolve(region(), &options(), Some(QualityProfile::TextDetail), true)
            .unwrap()
            .unwrap();
        assert_eq!(zero.plan.parameters.quality_vs_speed, Some(0));
        assert!(zero.custom_requested);
    }

    #[test]
    fn recording_quality_draft_rejects_invalid_text_and_conflicts_without_mutation() {
        for text in [
            "",
            "-1",
            "+1",
            "1.0",
            " 1",
            "1 ",
            "1e1",
            "１",
            "101",
            "4294967296",
        ] {
            let mut draft = enabled();
            draft.quality_vs_speed = NumberDraft {
                mode: NumberMode::Set,
                text: text.into(),
            };
            let original = draft.clone();
            assert!(draft.resolve(region(), &options(), None, true).is_err());
            assert_eq!(draft, original);
        }
        for text in [
            "",
            "-1",
            "+1",
            ".5",
            "1.",
            " 1",
            "1 ",
            "1e1",
            "1,5",
            "１",
            "1.0000001",
            "4294.967296",
        ] {
            let mut draft = enabled();
            draft.mean_mbps = NumberDraft {
                mode: NumberMode::Set,
                text: text.into(),
            };
            let original = draft.clone();
            let error = draft.resolve(region(), &options(), None, true).unwrap_err();
            assert!(error.to_string().contains(tr!("平均码率", "Mean bitrate")));
            assert_eq!(draft, original);
        }
        let mut draft = enabled();
        draft.mean_mbps = NumberDraft {
            mode: NumberMode::Set,
            text: "0".repeat(65),
        };
        assert!(draft.resolve(region(), &options(), None, true).is_err());
        draft.mean_mbps.mode = NumberMode::Inherit;
        draft.buffer_kbits = OptionalNumberDraft {
            mode: OptionalMode::Set,
            text: "0.009".into(),
        };
        let original = draft.clone();
        assert!(draft.resolve(region(), &options(), None, true).is_err());
        assert_eq!(draft, original);
        draft.buffer_kbits.mode = OptionalMode::Inherit;
        draft.peak_mbps.mode = OptionalMode::Clear;
        draft.rate_control = Override::Set(RateControl::PeakConstrainedVbr);
        let original = draft.clone();
        assert!(draft.resolve(region(), &options(), None, true).is_err());
        assert_eq!(draft, original);
    }

    #[test]
    fn recording_quality_draft_request_budgets_reject_instead_of_clamping() {
        let mut draft = QualityDraft {
            enabled: true,
            mean_mbps: NumberDraft {
                mode: NumberMode::Set,
                text: "80.000000".into(),
            },
            peak_mbps: OptionalNumberDraft {
                mode: OptionalMode::Set,
                text: "160.000000".into(),
            },
            buffer_kbits: OptionalNumberDraft {
                mode: OptionalMode::Set,
                text: "160000.000".into(),
            },
            gop_frames: OptionalNumberDraft {
                mode: OptionalMode::Set,
                text: "1800".into(),
            },
            quality_vs_speed: NumberDraft {
                mode: NumberMode::Set,
                text: "100".into(),
            },
            ..QualityDraft::default()
        };
        let request = draft
            .resolve(region(), &options(), None, true)
            .unwrap()
            .unwrap();
        assert_eq!(request.plan.bitrate_bps, 80_000_000);
        assert_eq!(request.plan.parameters.buffer_bits, Some(160_000_000));
        for field in 0..5 {
            let mut invalid = draft.clone();
            match field {
                0 => invalid.mean_mbps.text = "80.000001".into(),
                1 => invalid.peak_mbps.text = "160.000001".into(),
                2 => invalid.buffer_kbits.text = "160000.008".into(),
                3 => invalid.gop_frames.text = "1801".into(),
                _ => invalid.quality_vs_speed.text = "101".into(),
            }
            let original = invalid.clone();
            assert!(invalid.resolve(region(), &options(), None, true).is_err());
            assert_eq!(invalid, original);
        }
        draft.gop_frames.text = "1.0".into();
        assert!(draft.resolve(region(), &options(), None, true).is_err());
    }

    #[test]
    fn recording_quality_draft_disabled_experiment_off_and_gif_do_not_apply_or_parse() {
        let mut draft = enabled();
        draft.mean_mbps = NumberDraft {
            mode: NumberMode::Set,
            text: "invalid".repeat(10),
        };
        draft.quality_vs_speed = NumberDraft {
            mode: NumberMode::Set,
            text: "invalid speed".into(),
        };
        let mut invalid_options = options();
        invalid_options.fps = 0;
        let invalid_region = ScreenRect {
            width: 0,
            ..region()
        };
        let original = draft.clone();
        assert!(draft
            .resolve(invalid_region, &invalid_options, None, false)
            .unwrap()
            .is_none());
        assert_eq!(draft, original);
        invalid_options.preset = Preset::Gif;
        assert!(draft
            .resolve(
                invalid_region,
                &invalid_options,
                Some(QualityProfile::TextDetail),
                true
            )
            .unwrap()
            .is_none());
        assert_eq!(draft, original);
        draft.enabled = false;
        let original = draft.clone();
        assert!(draft
            .resolve(invalid_region, &options(), None, true)
            .unwrap()
            .is_none());
        assert_eq!(draft, original);
        assert!(!QualityDraft::default().enabled);
    }
}
