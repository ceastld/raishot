//! 同一短片驱动的 HEVC→H.264 组合试验；故障注入只用于显式运行的测试。
//! 注入发生在 HEVC writer 创建前，不能据此认定本机 HEVC 后端不受支持。

use super::{
    output_directory, quality_plan, run_request_case, run_request_case_with_hook, CodecAttempt,
    CodecRequest, EncoderAttemptError, EncoderPolicy, FailureCause, FailureStage, MfRuntime,
    Preset, QualityPlan, QualityProfile, VideoCodec, FPS, FRAMES, SOURCE_HEIGHT, SOURCE_WIDTH,
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
    PreSampleBackend,
    PreSampleOutput,
}

fn injected_error(
    attempt: CodecAttempt,
    quality: QualityPlan,
    injection: Injection,
) -> Option<EncoderAttemptError> {
    if attempt.codec != VideoCodec::Hevc || matches!(injection, Injection::None) {
        return None;
    }
    let cause = match injection {
        Injection::PreSampleBackend => FailureCause::Backend {
            message: "test-only injected HEVC media-type incompatibility before writer creation"
                .into(),
        },
        Injection::PreSampleOutput => FailureCause::Output {
            message: "test-only injected output failure before HEVC writer creation".into(),
        },
        Injection::None => return None,
    };
    Some(EncoderAttemptError {
        stage: FailureStage::MediaTypes,
        cause,
        diagnostics: json!({
            "injected_pre_sample_failure": true,
            "injection_point": "before_hevc_writer_and_backend_creation",
            "not_evidence_of_actual_hevc_unavailability": true,
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
    injection: Injection,
    stop: &AtomicBool,
) -> Value {
    let output = directory.join(label);
    let generated = (|| -> Result<Value> {
        fs::create_dir(&output)?;
        if matches!(injection, Injection::None) {
            return run_request_case(
                &output,
                EncoderPolicy::Auto,
                quality,
                CodecRequest::HevcWithH264Fallback,
                stop,
            );
        }
        run_request_case_with_hook(
            &output,
            EncoderPolicy::Auto,
            quality,
            CodecRequest::HevcWithH264Fallback,
            stop,
            |attempt| injected_error(attempt, quality, injection),
        )
    })();
    let outcome = match &generated {
        Err(_) => "probe_error",
        Ok(trial) if trial["encoded_and_reopened"] == true => "bounded_roundtrip_verified",
        Ok(trial) if trial["encoded_and_finalized"] == true => {
            "finalized_but_payload_or_decode_not_verified"
        }
        Ok(_) => "encoding_path_not_verified_with_diagnostics",
    };
    json!({
        "case": label, "output": output, "injection": format!("{injection:?}"),
        "outcome": outcome,
        "injected_pre_sample_failure": !matches!(injection, Injection::None),
        "injection_is_not_actual_hevc_support_evidence": !matches!(injection, Injection::None),
        "trial": generated.as_ref().ok(),
        "error": generated.as_ref().err().map(|error| error.to_string()),
    })
}

fn write_report(directory: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    if bytes.len() > REPORT_LIMIT {
        return Err("编码格式回退试验报告超过大小上限".into());
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

fn assert_history(report: &Value, quality: &Value) {
    assert_eq!(report["codec_request"], "hevc_with_h264_fallback");
    assert_eq!(report["requested_codec_order"], json!(["hevc", "h264"]));
    assert_eq!(&report["requested_plan"], quality);
    let attempts = report["attempts"].as_array().expect("缺少尝试历史");
    assert!(!attempts.is_empty() && attempts.len() <= 4, "{report}");
    let mut previous_slot = None;
    for attempt in attempts {
        let slot = attempt["slot"].as_u64().expect("缺少全局槽位");
        assert!(slot < 4, "{attempt}");
        assert!(previous_slot.is_none_or(|previous| slot > previous));
        previous_slot = Some(slot);
        assert_eq!(attempt["index"].as_u64(), Some(slot));
        assert_eq!(attempt["attempt_index"].as_u64(), Some(slot % 2));
        let expected_codec = if slot < 2 { "hevc" } else { "h264" };
        assert_eq!(attempt["codec"], expected_codec);
        let path = Path::new(attempt["path"].as_str().expect("缺少尝试路径"));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(format!("Auto-{slot}.mp4").as_str())
        );
        let diagnostics = attempt
            .get("encoder")
            .unwrap_or(&attempt["creation_error"]["diagnostics"]);
        let request = diagnostics.get("encoder_attempt").unwrap_or(diagnostics);
        assert_eq!(request["codec"], expected_codec, "{attempt}");
        assert_eq!(&request["quality"], quality, "{attempt}");
        assert_eq!(request["attempt_index"].as_u64(), Some(slot % 2));
        if diagnostics["injected_pre_sample_failure"] == true {
            assert_eq!(expected_codec, "hevc");
            assert_eq!(attempt["creation_error"]["stage"], "media_types");
            assert_eq!(
                diagnostics["not_evidence_of_actual_hevc_unavailability"],
                true
            );
            assert_eq!(
                fs::symlink_metadata(path).unwrap_err().kind(),
                ErrorKind::NotFound
            );
        }
    }
}

fn assert_roundtrip(report: &Value, expected_codec: &str) {
    assert_eq!(report["selected_codec"], expected_codec, "{report}");
    assert_eq!(report["encoded_and_finalized"], true);
    assert_eq!(report["encoded_and_reopened"], true);
    let last = report["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .expect("成功短片缺少最终尝试");
    assert_eq!(last["codec"], expected_codec);
    assert_eq!(last["encoding_finalized"], true);
    assert_eq!(last["decode_completed"], true);
    assert_eq!(
        last["retry_context"]["accepted_video_samples"].as_u64(),
        Some(u64::from(FRAMES))
    );
    assert_eq!(
        last["retry_context"]["accepted_audio_samples"].as_u64(),
        Some(u64::from(FRAMES))
    );
    assert!(last["finalize_error"].is_null());
    assert!(last["video_payload_error"].is_null());
    assert!(last["reopen_error"].is_null());
    let reopened = &last["reopen"];
    assert_eq!(reopened["native_video_codec"], expected_codec);
    assert_eq!(reopened["native_audio_codec"], "aac");
    assert_eq!(reopened["video_frames"].as_u64(), Some(u64::from(FRAMES)));
    assert!(reopened["audio_samples"]
        .as_u64()
        .is_some_and(|samples| samples > 0));
    let payload = &last["video_payload"];
    assert_eq!(payload["complete"], true);
    assert_eq!(payload["samples"].as_u64(), Some(u64::from(FRAMES)));
    assert!(payload["payload_bytes"]
        .as_u64()
        .is_some_and(|bytes| bytes > 0));
    assert!(payload["actual_payload_bitrate_bps"]
        .as_f64()
        .is_some_and(|bitrate| bitrate.is_finite() && bitrate > 0.));
    assert_eq!(&reopened["video_payload"], payload);
}

#[test]
#[ignore = "真实 HEVC/H.264/AAC 组合试编码与显式样本前故障注入；不打开窗口或播放声音"]
fn recording_codec_fallback_native_roundtrip_and_pre_sample_gates() -> Result<()> {
    let directory = output_directory(Path::new("target/recording-codec-fallback"))?;
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
            ("normal", Injection::None),
            ("injected-backend", Injection::PreSampleBackend),
            ("injected-output", Injection::PreSampleOutput),
        ]
        .into_iter()
        .map(|(label, injection)| run_case(&directory, label, quality, injection, &stop))
        .collect::<Vec<_>>();
        Ok(json!({"cases": cases, "requested_plan": quality}))
    })();
    let report = json!({
        "output": directory,
        "trial": generated.as_ref().ok(),
        "error": generated.as_ref().err().map(|error| error.to_string()),
        "fixture": {"source_width": SOURCE_WIDTH, "source_height": SOURCE_HEIGHT,
            "encoded_width": 384, "encoded_height": 256, "fps": FPS, "frames": FRAMES,
            "audio_is_synthetic_silence": true, "audio_output": false},
        "injected_pre_sample_failure_is_hevc_unavailability_evidence": false,
        "scope": "one unified CodecSequence and write loop for normal Auto HEVC-with-H264-fallback and two test-only pre-writer injected failures; each Auto request has at most four unique attempt slots; backend injection skips HEVC creation and must complete an actual 12-frame H264/AAC encode, Finalize, native payload EOF and full decode; output injection must stop without H264 creation; actual HEVC support is described only by the uninjected case; 30-second checks occur between native calls per encoding attempt and combined payload/decode pass, with no hard wall-clock bound on a native call; no desktop capture, playback UI, sound output, HDR, long recording or remix validation",
    });
    // 报告先落盘；随后所有可用性及注入门断言失败都能回看证据。
    write_report(&directory, &report)?;
    let generated = generated?;
    let quality = &generated["requested_plan"];
    let normal = trial(&generated["cases"][0]);
    assert_history(normal, quality);
    if normal["encoded_and_reopened"] == true {
        let codec = normal["selected_codec"].as_str().expect("缺少实际 codec");
        assert!(matches!(codec, "hevc" | "h264"));
        assert_roundtrip(normal, codec);
    } else {
        assert!(
            normal["attempts"]
                .as_array()
                .expect("已检查尝试历史")
                .iter()
                .any(|attempt| attempt["creation_error"].is_object()
                    || attempt["cause"].is_object()
                    || attempt["finalize_error"].is_string()
                    || attempt["video_payload_error"].is_string()
                    || attempt["reopen_error"].is_string()),
            "没有明确失败诊断：{normal}"
        );
    }
    let backend = trial(&generated["cases"][1]);
    assert_history(backend, quality);
    assert_roundtrip(backend, "h264");
    let attempts = backend["attempts"].as_array().expect("已检查尝试历史");
    assert!(attempts.len() >= 3);
    for attempt in &attempts[..2] {
        assert_eq!(attempt["codec"], "hevc");
        assert_eq!(attempt["creation_error"]["cause"]["kind"], "backend");
        assert_eq!(
            attempt["creation_error"]["diagnostics"]["injected_pre_sample_failure"],
            true
        );
    }
    assert_eq!(attempts[1]["codec_decision"]["codec_changed"], true);
    assert_eq!(attempts[2]["codec"], "h264");
    assert_eq!(attempts[2]["attempt_index"], 0);

    let output_case = &generated["cases"][2];
    let output = trial(output_case);
    assert_history(output, quality);
    let attempts = output["attempts"].as_array().expect("已检查尝试历史");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["codec"], "hevc");
    assert_eq!(attempts[0]["creation_error"]["cause"]["kind"], "output");
    assert_eq!(attempts[0]["codec_decision"]["action"], "stop");
    assert_eq!(
        attempts[0]["codec_decision"]["reason"]["reason"],
        "output_failure"
    );
    assert_eq!(output["encoded_and_finalized"], false);
    assert_eq!(output["encoded_and_reopened"], false);
    assert!(output["selected_codec"].is_null());
    let output_directory = Path::new(output_case["output"].as_str().expect("缺少输出目录"));
    assert!(fs::read_dir(output_directory)?
        .next()
        .transpose()?
        .is_none());
    Ok(())
}
