//! HEVC 固定短片技术验证；通过测试只表示报告完整，实际可用性取决于报告结果。
//! 此入口只运行固定 codec 路径；会话兼容回退由组合试验另验，HEVC 成品仍不支持重新混音。

use super::{
    output_directory, quality_plan, run_codec_case, EncoderPolicy, MfRuntime, Preset,
    QualityProfile, VideoCodec, FPS, FRAMES, SOURCE_HEIGHT, SOURCE_WIDTH,
};
use crate::Result;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::atomic::AtomicBool,
    time::Instant,
};

fn canonical_payload(
    output: &Path,
    trial: &Value,
    codec: VideoCodec,
    stop: &AtomicBool,
) -> Result<Value> {
    let index = trial["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .and_then(|attempt| attempt["index"].as_u64())
        .filter(|index| *index < 3)
        .ok_or("已收尾短片缺少有效尝试编号")?;
    let path = fs::canonicalize(output.join(format!("Auto-{index}.mp4")))?;
    // 真实 canonical 路径原样进入字节流读取器，不裁掉 Windows 文件命名空间前缀。
    let payload = super::read_payload(&path, codec, stop, Instant::now())?;
    Ok(json!({"path": path, "payload": payload}))
}

fn hevc_outcome(trial: &Value) -> &'static str {
    if trial["encoded_and_reopened"] == true {
        "bounded_roundtrip_verified"
    } else if trial["encoded_and_finalized"] != true {
        "encoding_path_not_verified"
    } else if trial["attempts"].as_array().is_some_and(|attempts| {
        attempts
            .iter()
            .any(|attempt| attempt["video_payload"]["complete"] == true)
    }) {
        "encoded_payload_readable_but_decode_or_color_validation_failed"
    } else {
        "finalized_but_native_payload_reading_failed"
    }
}

fn assert_request_codec(trial: &Value, codec: VideoCodec) {
    assert_eq!(trial["codec"], codec.id());
    let attempts = trial["attempts"].as_array().expect("试编码缺少尝试记录");
    assert!(!attempts.is_empty(), "{trial}");
    for attempt in attempts {
        let diagnostics = attempt
            .get("encoder")
            .unwrap_or(&attempt["creation_error"]["diagnostics"]);
        let actual_request = diagnostics
            .pointer("/encoder_attempt/codec")
            .or_else(|| diagnostics.get("codec"))
            .expect("后端诊断缺少实际接收的 codec");
        assert_eq!(actual_request, codec.id(), "{attempt}");
        if attempt["encoding_finalized"] == true {
            assert_eq!(diagnostics["output_request"]["codec"], codec.id());
            if codec == VideoCodec::Hevc {
                assert_eq!(diagnostics["output_request"]["profile"], "hevc_main_420_8");
                assert_eq!(diagnostics["observed_video_codec_matches_request"], true);
                assert_eq!(
                    diagnostics["encoding_trial"]["inspection"]["video"]["expected_codec"],
                    "hevc"
                );
                let checks = diagnostics["parameter_checks"]
                    .as_array()
                    .expect("已收尾 HEVC 短片缺少参数请求记录");
                for key in [
                    "rate_control",
                    "mean_bitrate",
                    "max_bitrate",
                    "quality",
                    "buffer_bytes",
                ] {
                    let requests: Vec<_> = checks
                        .iter()
                        .filter(|check| check["request"]["key"] == key)
                        .collect();
                    assert_eq!(requests.len(), 1, "{diagnostics}");
                    assert!(requests[0]["request"]["value"].is_null());
                }
                assert!(diagnostics["buffer_request"]["logical_bits"].is_null());
                assert!(diagnostics["buffer_request"]["native_bytes"].is_null());
            }
            for stage in ["first_video_sample", "first_audio_sample", "finalize"] {
                assert_eq!(diagnostics["encoding_trial"][stage]["state"], "succeeded");
            }
        }
    }
}

fn assert_roundtrip(trial: &Value, codec: VideoCodec) {
    assert_eq!(trial["encoded_and_finalized"], true);
    assert_eq!(trial["encoded_and_reopened"], true);
    let last = trial["attempts"]
        .as_array()
        .and_then(|attempts| attempts.last())
        .expect("已往返的短片缺少最终尝试");
    assert_eq!(last["decode_completed"], true);
    assert!(last["reopen_error"].is_null());
    assert!(last["video_payload_error"].is_null());
    let reopened = &last["reopen"];
    assert_eq!(reopened["native_video_codec"], codec.id());
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
    assert!(trial["canonical_payload_error"].is_null());
    let canonical = &trial["canonical_payload"]["payload"];
    assert_eq!(canonical["complete"], true);
    for key in ["samples", "payload_bytes", "observed_span_100ns"] {
        assert_eq!(canonical[key], payload[key], "{canonical}");
    }
}

#[test]
#[ignore = "真实 H.264 基线与 HEVC/AAC 静音短片报告；不打开窗口或播放声音，HEVC 不可用也保留诊断"]
fn recording_hevc_native_roundtrip_or_diagnosed_unavailable() -> Result<()> {
    let directory = output_directory(Path::new("target/recording-hevc-trial"))?;
    let stop = AtomicBool::new(false);
    let generated = (|| -> Result<Value> {
        let _runtime = MfRuntime::new()?;
        let quality = quality_plan(
            SOURCE_WIDTH,
            SOURCE_HEIGHT,
            FPS,
            QualityProfile::Current(Preset::CompactMp4),
        )?;
        let mut cases = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            let output = directory.join(codec.id());
            fs::create_dir(&output)?;
            let mut case = run_codec_case(&output, EncoderPolicy::Auto, quality, codec, &stop)?;
            if case["encoded_and_finalized"] == true {
                let canonical = canonical_payload(&output, &case, codec, &stop);
                case["canonical_payload"] = canonical.as_ref().ok().cloned().unwrap_or(Value::Null);
                case["canonical_payload_error"] =
                    canonical.err().map(|error| error.to_string()).into();
            }
            cases.push(case);
        }
        Ok(json!({"cases": cases, "requested_plan": quality}))
    })();
    let report = json!({
        "output": directory,
        "trial": generated.as_ref().ok(),
        "error": generated.as_ref().err().map(|error| error.to_string()),
        "hevc_outcome": generated.as_ref().ok().map(|trial| hevc_outcome(&trial["cases"][1])),
        "fixture": {"source_width": SOURCE_WIDTH, "source_height": SOURCE_HEIGHT,
            "encoded_width": 384, "encoded_height": 256, "fps": FPS, "frames": FRAMES,
            "audio_is_synthetic_silence": true, "audio_output": false},
        "product_recording_route": "separate experimental session API; not exercised here",
        "h264_compatibility_fallback_tested": false,
        "hevc_recording_remix_supported": false,
        "scope": "one fixed Auto trial per codec; codec stays fixed within each trial; reports actual MFT output, first video/audio samples, Finalize, native compressed payload and complete decode/color validation separately; finalized clips are also scanned through the unchanged canonical file path; a passing diagnostic test does not imply HEVC availability; no real desktop/text/motion, HDR, long recording, playback UI or other device validation",
    });
    let bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > 512 * 1024 {
        return Err("HEVC 试编码报告超过大小上限".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    // 先留证据，再断言；不支持 HEVC 的机器也必须有明确失败阶段，不能静默改成 H.264。
    let trial = generated?;
    let baseline = &trial["cases"][0];
    assert_request_codec(baseline, VideoCodec::H264);
    assert_roundtrip(baseline, VideoCodec::H264);
    let hevc = &trial["cases"][1];
    assert_request_codec(hevc, VideoCodec::Hevc);
    if hevc["encoded_and_reopened"] == true {
        assert_roundtrip(hevc, VideoCodec::Hevc);
    } else {
        let attempts = hevc["attempts"].as_array().expect("已确认存在尝试记录");
        assert!(
            attempts.iter().any(|attempt| {
                attempt.get("creation_error").is_some()
                    || attempt.get("cause").is_some()
                    || attempt["finalize_error"].is_string()
                    || attempt["reopen_error"].is_string()
                    || attempt["video_payload_error"].is_string()
            }),
            "HEVC 未成功但没有失败诊断：{hevc}"
        );
    }
    Ok(())
}
