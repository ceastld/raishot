//! AV1 固定短片的独立技术试验；诊断测试通过不能代替本机 AV1 往返成功证据。
//! 故障注入发生在 AV1 backend/writer 创建前，不是实际不支持证据，也不开放生产入口。

use super::{
    output_directory, quality_plan, run_codec_case, run_request_case, run_request_case_with_hook,
    CodecAttempt, CodecPlan, CodecRequest, EncoderAttemptError, EncoderPolicy, FailureCause,
    FailureStage, MfRuntime, Preset, QualityPlan, QualityProfile, VideoCodec, FPS, FRAMES,
    SOURCE_HEIGHT, SOURCE_WIDTH,
};
use crate::Result;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
    sync::atomic::AtomicBool,
};

const REPORT_LIMIT: usize = 512 * 1024;

#[derive(Clone, Copy, Debug)]
enum Injection {
    None,
    PreCreateBackend,
    PreCreateOutput,
}

fn injected_error(
    attempt: CodecAttempt,
    quality: QualityPlan,
    injection: Injection,
) -> Option<EncoderAttemptError> {
    if attempt.codec != VideoCodec::Av1 {
        return None;
    }
    let cause = match injection {
        Injection::None => return None,
        Injection::PreCreateBackend => FailureCause::Backend {
            message: "test-only injected AV1 media-type failure before backend creation".into(),
        },
        Injection::PreCreateOutput => FailureCause::Output {
            message: "test-only injected output failure before AV1 backend creation".into(),
        },
    };
    Some(EncoderAttemptError {
        stage: FailureStage::MediaTypes,
        cause,
        diagnostics: json!({
            "injected_pre_create_failure": true,
            "injection_point": "before_av1_backend_and_writer_creation",
            "not_evidence_of_actual_av1_unavailability": true,
            "encoder_attempt": {"policy": EncoderPolicy::Auto, "quality": quality,
                "codec": attempt.codec, "slot": attempt.slot,
                "attempt_index": attempt.attempt_index},
        }),
    })
}

fn run_case(
    directory: &Path,
    label: &str,
    quality: QualityPlan,
    request: CodecRequest,
    injection: Injection,
    stop: &AtomicBool,
) -> Value {
    let output = directory.join(label);
    let generated = (|| -> Result<Value> {
        fs::create_dir(&output)?;
        match (request, injection) {
            (CodecRequest::H264, Injection::None) => run_codec_case(
                &output,
                EncoderPolicy::Auto,
                quality,
                VideoCodec::H264,
                stop,
            ),
            (CodecRequest::Av1, Injection::None) => {
                run_codec_case(&output, EncoderPolicy::Auto, quality, VideoCodec::Av1, stop)
            }
            (_, Injection::None) => {
                run_request_case(&output, EncoderPolicy::Auto, quality, request, stop)
            }
            _ => run_request_case_with_hook(
                &output,
                EncoderPolicy::Auto,
                quality,
                request,
                stop,
                |attempt| injected_error(attempt, quality, injection),
            ),
        }
    })();
    let outcome = match &generated {
        Err(_) => "probe_error",
        Ok(trial) if trial["encoded_and_reopened"] == true => "bounded_roundtrip_verified",
        Ok(trial) if trial["encoded_and_finalized"] == true => {
            "finalized_but_payload_or_decode_not_verified"
        }
        Ok(_) => "not_verified_with_stage_diagnostics",
    };
    json!({
        "case": label, "output": output, "requested_codec": request,
        "injection": format!("{injection:?}"), "outcome": outcome,
        "injection_is_not_actual_av1_support_evidence": !matches!(injection, Injection::None),
        "trial": generated.as_ref().ok(),
        "error": generated.as_ref().err().map(|error| error.to_string()),
    })
}

fn write_report(directory: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    if bytes.len() > REPORT_LIMIT {
        return Err("AV1 技术试验报告超过大小上限".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}

fn trial(case: &Value) -> &Value {
    assert!(case["error"].is_null(), "{case}");
    assert!(case["trial"].is_object(), "{case}");
    &case["trial"]
}

fn assert_av1_diagnostics(encoder: &Value) {
    assert_eq!(encoder["output_request"]["codec"], "av1");
    for key in ["profile", "level"] {
        assert!(
            encoder["output_request"]
                .get(key)
                .is_some_and(Value::is_null),
            "{encoder}"
        );
    }
    assert_eq!(encoder["observed_video_codec_matches_request"], true);
    assert_eq!(
        encoder["encoding_trial"]["inspection"]["video"]["expected_codec"],
        "av1"
    );
    for key in [
        "logical_bits",
        "native_bytes",
        "native_property_unit",
        "conversion",
    ] {
        assert!(
            encoder["buffer_request"]
                .get(key)
                .is_some_and(Value::is_null),
            "{encoder}"
        );
    }
    let checks = encoder["parameter_checks"]
        .as_array()
        .expect("AV1 缺少真实参数诊断");
    for check in checks {
        let request = &check["request"];
        assert!(request.get("value").is_some_and(Value::is_null), "{check}");
        assert!(
            request.get("set_succeeded").is_some_and(Value::is_null),
            "{check}"
        );
        assert_eq!(
            request["source"],
            if request["key"] == "gop_frames" {
                "product_default_not_requested"
            } else {
                "encoding_parameters"
            }
        );
    }
    for key in [
        "rate_control",
        "mean_bitrate",
        "max_bitrate",
        "quality",
        "buffer_size_native",
        "quality_vs_speed",
        "low_latency",
        "b_frames",
        "gop_frames",
    ] {
        assert_eq!(
            checks
                .iter()
                .filter(|check| check["request"]["key"] == key)
                .count(),
            1,
            "AV1 缺少或重复参数 {key}：{encoder}"
        );
    }
}

fn assert_history(case: &Value, request: CodecRequest, quality: QualityPlan) {
    let report = trial(case);
    let plan = CodecPlan::new(request, EncoderPolicy::Auto, quality).unwrap();
    assert_eq!(
        report["codec_request"],
        serde_json::to_value(request).unwrap()
    );
    assert_eq!(
        report["requested_codec_order"],
        serde_json::to_value(plan.codec_order()).unwrap()
    );
    if let Some(selected) = report["selected_codec"].as_str() {
        assert!(
            plan.codec_order()
                .iter()
                .any(|codec| codec.id() == selected),
            "{report}"
        );
    } else {
        assert!(report["selected_codec"].is_null(), "{report}");
    }
    let quality = serde_json::to_value(quality).unwrap();
    assert_eq!(report["requested_plan"], quality);
    let attempts = report["attempts"].as_array().expect("缺少真实尝试历史");
    assert!(
        !attempts.is_empty() && attempts.len() <= plan.attempts().len(),
        "{report}"
    );
    let output = Path::new(case["output"].as_str().expect("缺少独占目录"));
    let mut previous_slot = None;
    for attempt in attempts {
        let slot = usize::try_from(attempt["slot"].as_u64().expect("缺少全局槽位")).unwrap();
        assert!(previous_slot.is_none_or(|previous| slot > previous));
        previous_slot = Some(slot);
        let expected = plan.attempts().get(slot).expect("槽位不属于本次请求");
        assert_eq!(attempt["index"].as_u64(), Some(slot as u64));
        assert_eq!(attempt["codec"], expected.codec.id());
        assert_eq!(
            attempt["attempt_index"].as_u64(),
            Some(expected.attempt_index as u64)
        );
        let path = Path::new(attempt["path"].as_str().expect("缺少尝试路径"));
        let expected_path = output.join(format!("Auto-{slot}.mp4"));
        assert_eq!(path, expected_path.as_path());
        let diagnostics = attempt
            .get("encoder")
            .unwrap_or(&attempt["creation_error"]["diagnostics"]);
        let backend_request = diagnostics.get("encoder_attempt").unwrap_or(diagnostics);
        assert_eq!(backend_request["codec"], expected.codec.id());
        assert_eq!(&backend_request["quality"], &quality);
        assert_eq!(
            backend_request["attempt_index"].as_u64(),
            Some(expected.attempt_index as u64)
        );
        if expected.codec == VideoCodec::Av1 && attempt.get("encoder").is_some() {
            assert_av1_diagnostics(diagnostics);
        }
        if diagnostics["injected_pre_create_failure"] == true {
            assert_eq!(expected.codec, VideoCodec::Av1);
            assert_eq!(
                diagnostics["not_evidence_of_actual_av1_unavailability"],
                true
            );
            assert_eq!(attempt["creation_error"]["stage"], "media_types");
            assert_eq!(
                fs::symlink_metadata(path).unwrap_err().kind(),
                ErrorKind::NotFound
            );
        }
    }
}

fn assert_roundtrip(report: &Value, codec: VideoCodec) {
    assert_eq!(report["encoded_and_finalized"], true, "{report}");
    assert_eq!(report["encoded_and_reopened"], true, "{report}");
    assert_eq!(report["selected_codec"], codec.id());
    let attempt = report["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .unwrap();
    assert_eq!(attempt["codec"], codec.id());
    assert_eq!(attempt["encoding_finalized"], true);
    assert_eq!(attempt["decode_completed"], true);
    for key in ["finalize_error", "video_payload_error", "reopen_error"] {
        assert!(attempt[key].is_null(), "{attempt}");
    }
    for key in ["accepted_video_samples", "accepted_audio_samples"] {
        assert_eq!(
            attempt["retry_context"][key].as_u64(),
            Some(u64::from(FRAMES))
        );
    }
    for stage in ["first_video_sample", "first_audio_sample", "finalize"] {
        assert_eq!(
            attempt["encoder"]["encoding_trial"][stage]["state"],
            "succeeded"
        );
    }
    let reopened = &attempt["reopen"];
    assert_eq!(reopened["native_video_codec"], codec.id());
    assert_eq!(reopened["native_audio_codec"], "aac");
    assert_eq!(reopened["video_frames"].as_u64(), Some(u64::from(FRAMES)));
    assert!(reopened["max_frame_time_error_ns"]
        .as_u64()
        .is_some_and(|error| error <= 1_000_000));
    assert_eq!(
        reopened["frame_time_tolerance_ns"].as_u64(),
        Some(1_000_000)
    );
    assert!(reopened["max_mean_channel_error"]
        .as_f64()
        .is_some_and(|error| error.is_finite() && error <= 12.));
    assert!(reopened["audio_samples"]
        .as_u64()
        .is_some_and(|samples| samples > 0));
    assert!(reopened["audio_blocks"]
        .as_u64()
        .is_some_and(|blocks| blocks > 0));
    let payload = &attempt["video_payload"];
    assert_eq!(payload["complete"], true);
    assert!(payload["samples"]
        .as_u64()
        .is_some_and(|samples| samples > 0));
    assert!(payload["payload_bytes"]
        .as_u64()
        .is_some_and(|bytes| bytes > 0));
    assert_eq!(payload["invalid_durations"].as_u64(), Some(0));
    if codec != VideoCodec::Av1 {
        assert_eq!(payload["samples"].as_u64(), Some(u64::from(FRAMES)));
        assert!(payload["actual_payload_bitrate_bps"]
            .as_f64()
            .is_some_and(|rate| rate.is_finite() && rate > 0.));
    }
    assert_eq!(&reopened["video_payload"], payload);
}

fn assert_success_or_diagnosed(report: &Value) {
    if report["encoded_and_reopened"] == true {
        let codec = match report["selected_codec"].as_str() {
            Some("av1") => VideoCodec::Av1,
            Some("h264") => VideoCodec::H264,
            _ => panic!("成功报告未确认本次允许的 codec：{report}"),
        };
        assert_roundtrip(report, codec);
    } else {
        assert!(
            report["attempts"]
                .as_array()
                .expect("缺少失败历史")
                .iter()
                .any(|attempt| {
                    (attempt["creation_error"]["stage"].is_string()
                        && attempt["creation_error"]["cause"]["kind"].is_string())
                        || (attempt["stage"].is_string() && attempt["cause"]["kind"].is_string())
                        || ["finalize_error", "video_payload_error", "reopen_error"]
                            .iter()
                            .any(|key| {
                                attempt[*key]
                                    .as_str()
                                    .is_some_and(|error| !error.is_empty())
                            })
                }),
            "未通过往返却缺少明确阶段诊断：{report}"
        );
    }
}

#[test]
#[ignore = "真实 H.264/AV1/AAC 技术短片与创建前故障注入；不打开窗口或播放声音"]
fn recording_av1_native_roundtrip_or_diagnosed_unavailable() -> Result<()> {
    let directory = output_directory(Path::new("target/recording-av1-trial"))?;
    let stop = AtomicBool::new(false);
    let generated = (|| -> Result<Value> {
        let _runtime = MfRuntime::new()?;
        let quality = quality_plan(
            SOURCE_WIDTH,
            SOURCE_HEIGHT,
            FPS,
            QualityProfile::Current(Preset::CompactMp4),
        )?;
        let cases = [
            ("h264_baseline", CodecRequest::H264, Injection::None),
            ("av1_only", CodecRequest::Av1, Injection::None),
            (
                "av1_with_h264_fallback",
                CodecRequest::Av1WithH264Fallback,
                Injection::None,
            ),
            (
                "injected_av1_backend",
                CodecRequest::Av1WithH264Fallback,
                Injection::PreCreateBackend,
            ),
            (
                "injected_av1_output",
                CodecRequest::Av1WithH264Fallback,
                Injection::PreCreateOutput,
            ),
        ]
        .map(|(label, request, injection)| {
            run_case(&directory, label, quality, request, injection, &stop)
        });
        Ok(json!({"cases": cases, "requested_plan": quality}))
    })();
    write_report(
        &directory,
        &json!({
            "output": directory, "trial": generated.as_ref().ok(),
            "error": generated.as_ref().err().map(|error| error.to_string()),
            "fixture": {"source_width": SOURCE_WIDTH, "source_height": SOURCE_HEIGHT,
                "encoded_width": 384, "encoded_height": 256, "fps": FPS, "frames": FRAMES,
                "audio_is_synthetic_silence": true, "audio_output": false},
            "production_av1_entry": "remains rejected by separate session preflight; not exercised by this native test",
            "passing_test_proves_av1_availability": false,
            "injected_failure_is_actual_av1_unavailability_evidence": false,
        "scope": "five sequential independent Auto cases using the same bounded 12-frame fixture and existing trial writer loop; baseline and injected-backend H264 paths must complete real encoding, Finalize, native compressed payload EOF and full video/AAC decode; uninjected AV1 availability is only established by its own successful roundtrip; AV1 compressed sample count is not a displayed frame count and payload bitrate may remain unknown; complete decode separately checks 12 displayed frames within 1 ms of expected times and synthetic color blocks; only allowed failures before sample acceptance can change codec; Finalize, payload or decode failures after acceptance retain diagnostics without replaying the clip; pre-create injections test explicit fallback and output-stop gates without creating an AV1 backend or writer; no canonical duplicate scan; existing cancellation and budget guards operate between native calls and cannot interrupt one native call; single-clip costs are not a performance comparison or validation of desktop/text/motion, HDR, long recording, sound output, playback UI or production AV1 support",
        }),
    )?;
    // 所有 case 的原始请求、诊断和顶层错误先保留，之后任何断言失败都不会丢失报告。
    let generated = generated?;
    let quality = quality_plan(
        SOURCE_WIDTH,
        SOURCE_HEIGHT,
        FPS,
        QualityProfile::Current(Preset::CompactMp4),
    )?;
    let cases = generated["cases"].as_array().expect("缺少五组技术试验");
    assert_eq!(cases.len(), 5);
    assert_history(&cases[0], CodecRequest::H264, quality);
    assert_roundtrip(trial(&cases[0]), VideoCodec::H264);
    assert_history(&cases[1], CodecRequest::Av1, quality);
    assert_success_or_diagnosed(trial(&cases[1]));
    assert_history(&cases[2], CodecRequest::Av1WithH264Fallback, quality);
    assert_success_or_diagnosed(trial(&cases[2]));
    assert_history(&cases[3], CodecRequest::Av1WithH264Fallback, quality);
    let injected_backend = trial(&cases[3]);
    assert_roundtrip(injected_backend, VideoCodec::H264);
    let attempts = injected_backend["attempts"].as_array().unwrap();
    assert!(attempts.len() >= 3);
    for attempt in &attempts[..2] {
        assert_eq!(attempt["codec"], "av1");
        assert_eq!(attempt["creation_error"]["cause"]["kind"], "backend");
        assert_eq!(
            attempt["creation_error"]["diagnostics"]["injected_pre_create_failure"],
            true
        );
    }
    assert_eq!(attempts[1]["codec_decision"]["codec_changed"], true);
    assert_eq!(attempts[2]["codec"], "h264");
    assert_eq!(attempts[2]["attempt_index"], 0);
    assert_history(&cases[4], CodecRequest::Av1WithH264Fallback, quality);
    let injected_output = trial(&cases[4]);
    let attempts = injected_output["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["creation_error"]["cause"]["kind"], "output");
    assert_eq!(attempts[0]["codec_decision"]["action"], "stop");
    assert_eq!(
        attempts[0]["codec_decision"]["reason"]["reason"],
        "output_failure"
    );
    assert_eq!(injected_output["encoded_and_finalized"], false);
    assert_eq!(injected_output["encoded_and_reopened"], false);
    assert!(injected_output["selected_codec"].is_null());
    let output = Path::new(cases[4]["output"].as_str().unwrap());
    assert!(fs::read_dir(output)?.next().transpose()?.is_none());
    Ok(())
}
