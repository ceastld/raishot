//! 原录制 writer 快照的只读反馈；不检查媒体，也不把请求或数值一致当成实际效果。
use super::video_type_feedback;
use crate::{
    recording::encoding_report::{SessionEncodingReport, StepState},
    recording_encoder_caps::{EncoderKind, VideoCodec, VideoInput},
    recording_options::{
        advanced::{OptionalOverride, Override},
        RateControl,
    },
};
use serde_json::Value;

const MAX_CHECKS: usize = 32;
const MAX_ROWS: usize = 24;

pub(super) struct EncodingFeedback {
    pub summary: String,
    pub rows: Vec<String>,
    pub note: String,
}

pub(super) fn from_report(report: Option<&SessionEncodingReport>) -> EncodingFeedback {
    let note = tr!(
        "原录制 writer 快照，收尾状态单列；不代表转 GIF 或重新混音后的文件。观测尺寸、帧率及色彩来自初始化类型，不证明成片类型、实际帧率或色准。缩放行仅描述源/目标请求，不证明实际像素或生效。参数数值匹配不证明 Set 成功、支持或效果；视频负载码率、画质及完整回读未测。",
        "Original recording writer snapshot; finalization is separate. It does not describe a GIF or remixed file. Observed size, rate and color come from initial types, not proof of file types, measured frame rate or color fidelity. The resize row describes source/target requests, not resized pixels or effectiveness. Matching values do not prove Set success, support or effects. Video payload bitrate, quality and full readback were not measured."
    )
    .into();
    let Some(report) = report else {
        return EncodingFeedback {
            summary: tr!("编码信息未知", "Encoding unknown").into(),
            rows: vec![tr!(
                "没有原录制 writer 报告",
                "No original recording writer report"
            )
            .into()],
            note,
        };
    };
    let codec = match report.confirmed_video_codec() {
        Some(VideoCodec::H264) => "H.264",
        Some(VideoCodec::Hevc) => "HEVC",
        Some(VideoCodec::Av1) => "AV1",
        None => tr!("编码未知", "Codec unknown"),
    };
    let kind = match report.video_encoder_kind() {
        EncoderKind::Hardware => tr!("硬件编码", "Hardware encoding"),
        EncoderKind::Software => tr!("软件编码", "Software encoding"),
        EncoderKind::Unknown => tr!("编码器类型未知", "Encoder kind unknown"),
    };
    let input = match report.input() {
        Some(VideoInput::GpuNv12) => tr!("GPU NV12 输入", "GPU NV12 input"),
        Some(VideoInput::CpuRgb32) => tr!("CPU RGB 输入", "CPU RGB input"),
        None => tr!("输入未知", "Input unknown"),
    };
    let mut rows = vec![trf!(
        "请求画质：{}",
        "Requested profile: {}",
        report.requested_profile.label()
    )];
    if let Some(scale) = report.requested_scale {
        rows.push(trf!(
            "缩放请求：源 {}×{}；裁偶 {}×{} → 目标 {}×{}（仅请求，未证明实际像素或生效）",
            "Resize request: source {}×{}; even-cropped {}×{} → target {}×{} (request only; resized pixels and effectiveness unverified)",
            scale.source.source_width,
            scale.source.source_height,
            scale.geometry.source_width,
            scale.geometry.source_height,
            scale.geometry.target_width,
            scale.geometry.target_height
        ));
    }
    rows.extend(video_type_feedback::rows(report));
    let name = report
        .video_encoder_name()
        .map(|name| clip(name, 128))
        .unwrap_or_else(|| unknown().into());
    rows.push(trf!(
        "实际编码器名称：{name}",
        "Observed encoder name: {name}"
    ));
    rows.push(adapter_row(report));
    if let Some(error) = report
        .finalized_writer
        .pointer("/encoding_trial/inspection/video/encoder_adapter_diagnostic")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    {
        let error = clip(error, 256);
        rows.push(trf!(
            "适配器观测说明：{error}",
            "Adapter observation: {error}"
        ));
    }
    rows.push(trf!(
        "原 writer 收尾：{}；重开检查：{}",
        "Original writer Finalize: {}; reopen check: {}",
        step(report.finalize_state()),
        step(report.reopen_state())
    ));
    advanced_rows(report, &mut rows);
    for (key, label, unit) in [
        (
            "mean_bitrate",
            tr!("平均码率属性", "Mean bitrate property"),
            "bit/s",
        ),
        (
            "max_bitrate",
            tr!("峰值码率属性", "Peak bitrate property"),
            "bit/s",
        ),
        (
            "rate_control",
            tr!("控制模式属性", "Rate-control property"),
            "",
        ),
        (
            "gop_frames",
            tr!("GOP 最大图像数属性", "Maximum GOP picture-count property"),
            "",
        ),
        (
            "quality_vs_speed",
            tr!("质量/速度属性", "Quality/speed property"),
            "",
        ),
        ("low_latency", tr!("低延迟属性", "Low-latency property"), ""),
        (
            "b_frames",
            tr!("B 帧数量属性", "B-frame-count property"),
            "",
        ),
    ] {
        rows.push(parameter_row(report, key, label, unit));
    }
    rows.push(buffer_row(report));
    rows.truncate(MAX_ROWS);
    EncodingFeedback {
        summary: format!("{codec} · {kind} · {input}"),
        rows,
        note,
    }
}

fn unknown() -> &'static str {
    tr!("未知", "Unknown")
}

fn step(state: StepState) -> &'static str {
    match state {
        StepState::Unknown => unknown(),
        StepState::NotAttempted => tr!("未尝试", "Not attempted"),
        StepState::Succeeded => tr!("成功", "Succeeded"),
        StepState::Failed => tr!("失败", "Failed"),
    }
}

fn clip(text: &str, limit: usize) -> String {
    let mut characters = text.chars();
    let mut result: String = characters
        .by_ref()
        .take(limit)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if characters.next().is_some() && limit != 0 {
        result.pop();
        result.push('…');
    }
    result
}

fn adapter_row(report: &SessionEncodingReport) -> String {
    let adapter = (|| {
        let video = report
            .finalized_writer
            .pointer("/encoding_trial/inspection/video")?;
        let encoder = video.get("encoder")?;
        if !encoder.is_object() || encoder.get("error").is_some_and(|error| !error.is_null()) {
            return None;
        }
        // 只读取该实际 encoder 的独立适配器字段；没有 input_adapter 或目录回填路线。
        let adapter = video.get("encoder_adapter")?;
        let high = i32::try_from(adapter.get("luid_high")?.as_i64()?).ok()?;
        let low = u32::try_from(adapter.get("luid_low")?.as_u64()?).ok()?;
        let name = adapter
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(|name| clip(name, 128))
            .unwrap_or_else(|| unknown().into());
        Some(format!("{name} (LUID high={high}, low={low})"))
    })()
    .unwrap_or_else(|| unknown().into());
    trf!(
        "实际编码适配器：{adapter}",
        "Observed encoder adapter: {adapter}"
    )
}

fn advanced_rows(report: &SessionEncodingReport, rows: &mut Vec<String>) {
    let Some(request) = report.requested_quality else {
        rows.push(tr!("高级请求 DTO：未记录", "Advanced request DTO: not recorded").into());
        return;
    };
    let overrides = request.overrides;
    let mean = match overrides.avg_bitrate_bps {
        Override::Inherit => tr!("继承", "Inherit").into(),
        Override::Set(value) => trf!("显式设置 {value} bit/s", "Explicitly set {value} bit/s"),
    };
    let speed = match overrides.quality_vs_speed {
        Override::Inherit => tr!("继承", "Inherit").into(),
        Override::Set(value) => trf!("显式设置 {value}", "Explicitly set {value}"),
    };
    let mode = match overrides.rate_control {
        Override::Inherit => tr!("继承", "Inherit"),
        Override::Set(RateControl::EncoderDefault) => tr!(
            "撤回控制模式请求（编码器默认）",
            "Withdraw rate-control request (encoder default)"
        ),
        Override::Set(RateControl::PeakConstrainedVbr) => {
            tr!("显式峰值受限 VBR", "Explicit peak-constrained VBR")
        }
    };
    let peak = optional(overrides.max_bitrate_bps, "bit/s");
    let buffer = optional(overrides.buffer_bits, "bit");
    let gop = optional(overrides.gop_frames, tr!("个最大图像", "maximum pictures"));
    rows.extend([
        trf!(
            "高级请求 · 平均码率：{mean}",
            "Advanced request · mean bitrate: {mean}"
        ),
        trf!(
            "高级请求 · 控制模式：{mode}",
            "Advanced request · rate control: {mode}"
        ),
        trf!(
            "高级请求 · 峰值码率：{peak}",
            "Advanced request · peak bitrate: {peak}"
        ),
        trf!(
            "高级请求 · 缓冲：{buffer}",
            "Advanced request · buffer: {buffer}"
        ),
        trf!("高级请求 · GOP：{gop}", "Advanced request · GOP: {gop}"),
        trf!(
            "高级请求 · 质量/速度：{speed}",
            "Advanced request · quality/speed: {speed}"
        ),
    ]);
}

fn optional(value: OptionalOverride<u32>, unit: &str) -> String {
    match value {
        OptionalOverride::Inherit => tr!("继承", "Inherit").into(),
        OptionalOverride::Clear => tr!("明确清除请求", "Explicitly clear request").into(),
        OptionalOverride::Set(value) => {
            trf!("显式设置 {value} {unit}", "Explicitly set {value} {unit}")
        }
    }
}

enum Check<'a> {
    Missing,
    Ambiguous,
    Unique(&'a Value),
}

fn unique_check<'a>(report: &'a SessionEncodingReport, keys: &[&str]) -> Check<'a> {
    let Some(checks) = report.parameter_checks() else {
        return Check::Missing;
    };
    if checks.len() > MAX_CHECKS {
        return Check::Ambiguous;
    }
    let mut matches = checks.iter().filter(|check| {
        check
            .pointer("/request/key")
            .and_then(Value::as_str)
            .is_some_and(|key| keys.contains(&key))
    });
    match (matches.next(), matches.next()) {
        (None, _) => Check::Missing,
        (Some(check), None) => Check::Unique(check),
        _ => Check::Ambiguous,
    }
}

fn buffer_row(report: &SessionEncodingReport) -> String {
    match unique_check(report, &["buffer_bytes", "buffer_size_native"]) {
        Check::Unique(check) => match check.pointer("/request/key").and_then(Value::as_str) {
            Some("buffer_bytes") => parameter_row(
                report,
                "buffer_bytes",
                tr!("缓冲属性", "Buffer property"),
                "byte",
            ),
            Some("buffer_size_native") => parameter_row(
                report,
                "buffer_size_native",
                tr!(
                    "缓冲属性（原生单位未知）",
                    "Buffer property (native unit unknown)"
                ),
                "",
            ),
            _ => unreachable!(),
        },
        Check::Missing => tr!(
            "缓冲属性：未知（未记录）",
            "Buffer property: unknown (not recorded)"
        )
        .into(),
        Check::Ambiguous => tr!(
            "缓冲属性：未知（记录重复、单位冲突或数量超限）",
            "Buffer property: unknown (duplicate records, conflicting units or record limit)"
        )
        .into(),
    }
}

fn unsigned(value: &Value) -> Option<u32> {
    u32::try_from(value.as_u64()?).ok()
}

fn value_text(value: Option<u32>, unit: &str) -> String {
    value.map_or_else(
        || unknown().into(),
        |value| {
            if unit.is_empty() {
                value.to_string()
            } else {
                format!("{value} {unit}")
            }
        },
    )
}

fn parameter_row(report: &SessionEncodingReport, key: &str, label: &str, unit: &str) -> String {
    let check = match unique_check(report, &[key]) {
        Check::Unique(check) => check,
        Check::Missing => {
            return trf!("{label}：未知（未记录）", "{label}: unknown (not recorded)")
        }
        Check::Ambiguous => {
            return trf!(
                "{label}：未知（记录重复或数量超限）",
                "{label}: unknown (duplicate records or record limit)"
            )
        }
    };
    let request = &check["request"];
    let requested = request.get("value").and_then(unsigned);
    let source = request.get("source").and_then(Value::as_str);
    let withdrawn = source == Some("explicit_request_withdrawal");
    let inactive = source == Some("product_default_not_requested");
    let explicitly_absent = request.get("value").is_some_and(Value::is_null);
    let read_ok = check.get("read_error").is_some_and(Value::is_null);
    let observed = read_ok
        .then(|| check.get("observed").and_then(unsigned))
        .flatten();
    let requested_text = if explicitly_absent {
        tr!("无数值请求", "No numeric request").into()
    } else {
        value_text(requested, unit)
    };
    let observed_text = value_text(observed, unit);
    let state = if requested.is_some() && (withdrawn || inactive) {
        tr!(
            "未知（请求来源矛盾）",
            "Unknown (conflicting request source)"
        )
    } else if explicitly_absent && check.get("matches_request").is_some_and(Value::is_null) {
        if withdrawn {
            tr!(
                "未请求（已明确撤回）",
                "Not requested (explicitly withdrawn)"
            )
        } else if inactive {
            tr!(
                "未请求（产品默认未启用）",
                "Not requested (product default inactive)"
            )
        } else if matches!(
            source,
            Some("encoding_parameters" | "codec_api_before_begin_writing")
        ) {
            tr!("未请求此属性数值", "No request for this property value")
        } else {
            unknown()
        }
    } else {
        match report.parameter_value_matches(key) {
            Some(true) => tr!("请求/回读数值匹配", "Requested/readback values match"),
            Some(false) => tr!("请求/回读数值不匹配", "Requested/readback values differ"),
            None => tr!("数值匹配未知", "Value match unknown"),
        }
    };
    let set = match request.get("set_succeeded").and_then(Value::as_bool) {
        Some(true) => tr!("Set 调用成功", "Set call succeeded"),
        Some(false) => tr!("Set 调用失败", "Set call failed"),
        None => tr!("Set 结果未知", "Set result unknown"),
    };
    let mut row = trf!(
        "{label}：请求 {requested_text}；回读 {observed_text}；{state}；{set}",
        "{label}: requested {requested_text}; readback {observed_text}; {state}; {set}"
    );
    if let Some(error) = check.get("read_error").filter(|error| !error.is_null()) {
        let error = error
            .as_str()
            .map(|error| clip(error, 256))
            .unwrap_or_else(|| tr!("错误字段格式未知", "Unknown error-field format").into());
        row.push_str(&trf!("；读取错误：{error}", "; read error: {error}"));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recording_encoder_policy::{codec::CodecRequest, EncoderPolicy},
        recording_options::{
            advanced::{resolve, QualityOverrides},
            Preset, QualityProfile,
        },
    };
    use serde_json::json;

    fn report(snapshot: Value) -> SessionEncodingReport {
        SessionEncodingReport::from_finalized_writer(
            EncoderPolicy::Auto,
            QualityProfile::Current(Preset::CompactMp4),
            None,
            snapshot,
        )
    }

    #[test]
    fn recording_encoding_feedback_unknown_does_not_borrow_requests_or_gpu_adapter() {
        let report = report(json!({
            "output_request": {"codec": "hevc"}, "hardware": true,
            "input": "gpu_nv12", "name": "TOP_LEVEL_ONLY",
            "encoding_trial": {"inspection": {
                "input": "gpu_nv12",
                "input_adapter": {"name": "INPUT_ONLY", "luid_high": 1, "luid_low": 2},
                "video": {"encoder": null, "encoder_adapter": null}
            }}
        }))
        .with_codec_evidence(CodecRequest::Hevc, None, vec![]);
        let feedback = from_report(Some(&report));
        assert!(feedback.summary.contains(tr!("编码未知", "Codec unknown")));
        assert!(feedback
            .summary
            .contains(tr!("编码器类型未知", "Encoder kind unknown")));
        assert!(feedback.summary.contains("GPU NV12"));
        assert!(!feedback.summary.contains("HEVC"));
        assert!(feedback
            .rows
            .iter()
            .all(|row| !row.contains("INPUT_ONLY") && !row.contains("TOP_LEVEL_ONLY")));
        assert!(feedback.rows.len() <= MAX_ROWS);
        assert_eq!(
            from_report(None).summary,
            tr!("编码信息未知", "Encoding unknown")
        );
    }

    #[test]
    fn recording_encoding_feedback_duplicate_and_failed_read_remain_unknown_and_bounded() {
        let report = report(json!({"parameter_checks": [
            {"request": {"key": "mean_bitrate", "value": 100, "source": "encoding_parameters"},
                "observed": 100, "read_error": null, "matches_request": true},
            {"request": {"key": "mean_bitrate", "value": 200, "source": "encoding_parameters"},
                "observed": 200, "read_error": null, "matches_request": true},
            {"request": {"key": "gop_frames", "value": 120, "source": "codec_api_before_begin_writing"},
                "observed": 120, "read_error": "x".repeat(1024), "matches_request": true},
            {"request": {"key": "buffer_bytes", "value": 8},
                "observed": 8, "read_error": null, "matches_request": true},
            {"request": {"key": "buffer_size_native", "value": 8},
                "observed": 8, "read_error": null, "matches_request": true}
        ]}));
        let duplicate = parameter_row(&report, "mean_bitrate", "MEAN", "bit/s");
        assert!(duplicate.contains(tr!("未知", "unknown")));
        assert!(!duplicate.contains(tr!("数值匹配", "values match")));
        let failed = parameter_row(&report, "gop_frames", "GOP", "");
        assert!(failed.contains("120"));
        assert!(failed.contains(tr!("数值匹配未知", "Value match unknown")));
        assert!(!failed.contains(&"x".repeat(257)));
        assert!(buffer_row(&report).contains(tr!("未知", "unknown")));
        assert!(from_report(Some(&report)).rows.len() <= MAX_ROWS);
    }

    #[test]
    fn recording_encoding_feedback_value_match_is_not_set_success_or_a_missing_zero() {
        let request = resolve(
            960,
            540,
            30,
            QualityProfile::Current(Preset::CompactMp4),
            QualityOverrides {
                avg_bitrate_bps: Override::Set(1_000_000),
                quality_vs_speed: Override::Set(0),
                buffer_bits: OptionalOverride::Clear,
                gop_frames: OptionalOverride::Clear,
                ..QualityOverrides::default()
            },
        )
        .unwrap();
        let mut report = report(json!({
            "encoding_trial": {"inspection": {"video": {
                "encoder": {"kind": "unknown", "name": null, "error": null},
                "encoder_adapter": {"name": null, "luid_high": -2, "luid_low": 7}
            }}},
            "parameter_checks": [
                {"request": {"key": "mean_bitrate", "value": 1000000, "source": "encoding_parameters", "set_succeeded": false},
                    "observed": 1000000, "read_error": null, "matches_request": true},
                {"request": {"key": "max_bitrate", "value": null, "source": "encoding_parameters", "set_succeeded": null},
                    "observed": 0, "read_error": null, "matches_request": null},
                {"request": {"key": "gop_frames", "value": null, "source": "explicit_request_withdrawal", "set_succeeded": null},
                    "observed": 60, "read_error": null, "matches_request": null}
            ]
        })).with_requested_quality(Some(request));
        let matched = parameter_row(&report, "mean_bitrate", "MEAN", "bit/s");
        assert!(matched.contains(tr!("请求/回读数值匹配", "Requested/readback values match")));
        assert!(matched.contains(tr!("Set 调用失败", "Set call failed")));
        let absent = parameter_row(&report, "max_bitrate", "PEAK", "bit/s");
        assert!(absent.contains(tr!("无数值请求", "No numeric request")));
        assert!(absent.contains("0 bit/s"));
        assert!(!absent.contains(tr!("请求/回读数值匹配", "Requested/readback values match")));
        let withdrawn = parameter_row(&report, "gop_frames", "GOP", "");
        assert!(withdrawn.contains(tr!("已明确撤回", "explicitly withdrawn")));
        let feedback = from_report(Some(&report));
        assert!(feedback
            .rows
            .iter()
            .any(|row| row.contains("LUID high=-2, low=7")));
        assert!(feedback
            .rows
            .iter()
            .any(|row| row.contains(tr!("明确清除请求", "Explicitly clear request"))));
        assert!(feedback.rows.iter().any(|row| {
            row.contains(tr!(
                "高级请求 · 质量/速度",
                "Advanced request · quality/speed"
            )) && row.contains(tr!("显式设置 0", "Explicitly set 0"))
        }));
        assert!(feedback.rows.len() <= MAX_ROWS);
        report.finalized_writer["parameter_checks"][2]["request"]["source"] =
            "product_default_not_requested".into();
        let inactive = parameter_row(&report, "gop_frames", "GOP", "");
        assert!(inactive.contains(tr!("产品默认未启用", "product default inactive")));
        assert_ne!(inactive, withdrawn);
    }

    #[test]
    fn recording_encoding_feedback_scale_request_keeps_unknown_and_last_buffer_at_row_limit() {
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
        let quality = resolve(
            192,
            128,
            30,
            QualityProfile::Current(Preset::CompactMp4),
            QualityOverrides {
                avg_bitrate_bps: Override::Set(2_000_000),
                rate_control: Override::Set(RateControl::PeakConstrainedVbr),
                quality_vs_speed: Override::Set(100),
                max_bitrate_bps: OptionalOverride::Set(4_000_000),
                buffer_bits: OptionalOverride::Set(4_000_000),
                gop_frames: OptionalOverride::Set(6),
            },
        )
        .unwrap();
        let report = report(json!({
            "encoding_trial": {"inspection": {"video": {
                "encoder": null,
                "encoder_adapter_diagnostic": "adapter observation unavailable",
                "inputs": [], "outputs": []
            }}},
            "parameter_checks": [{
                "request": {"key": "buffer_bytes", "value": 500000,
                    "source": "codec_api_before_begin_writing", "set_succeeded": true},
                "observed": 500000, "read_error": null, "matches_request": true
            }]
        }))
        .with_requested_quality(Some(quality))
        .with_requested_scale(Some(scale));
        let feedback = from_report(Some(&report));
        assert_eq!(feedback.rows.len(), MAX_ROWS);
        assert_eq!(feedback.rows.last(), Some(&buffer_row(&report)));
        assert!(feedback.rows.last().unwrap().contains("500000 byte"));
        let scale_row = feedback
            .rows
            .iter()
            .find(|row| row.starts_with(tr!("缩放请求：", "Resize request:")))
            .unwrap();
        for dimensions in ["385×257", "384×256", "192×128"] {
            assert!(scale_row.contains(dimensions));
        }
        assert!(scale_row.contains(tr!("仅请求", "request only")));
        let actual_rows = video_type_feedback::rows(&report);
        assert!(actual_rows[0].contains(unknown()));
        assert!(actual_rows.iter().all(|row| !row.contains("192×128")));
        assert!(feedback.rows.contains(&actual_rows[0]));
        assert!(feedback.summary.contains(tr!("编码未知", "Codec unknown")));

        let without_scale = report.with_requested_scale(None);
        let feedback = from_report(Some(&without_scale));
        assert_eq!(feedback.rows.len(), MAX_ROWS - 1);
        assert!(feedback
            .rows
            .iter()
            .all(|row| !row.starts_with(tr!("缩放请求：", "Resize request:"))));
        assert_eq!(feedback.rows.last(), Some(&buffer_row(&without_scale)));
    }
}
