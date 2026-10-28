//! 显式运行的高级请求短片往返；只使用自有合成色块和 AAC 静音，不打开窗口或播放声音。
//! 单个固定请求的成功不代表全部参数范围受支持，也不评价真实文字、滚动、桌面或 HDR 画质。

use crate::{
    media_foundation::MfRuntime,
    recording_encoder_policy::EncoderPolicy,
    recording_encoder_trial::run_quality_case,
    recording_options::{
        advanced::{resolve, OptionalOverride, Override, QualityOverrides},
        GopRequest, Preset, QualityProfile, RateControl,
    },
    Result,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{SystemTime, UNIX_EPOCH},
};

const SOURCE_WIDTH: u32 = 385;
const SOURCE_HEIGHT: u32 = 257;
const FPS: u32 = 30;
const FRAMES: u64 = 12;
const REPORT_LIMIT: usize = 256 * 1024;

fn unique_directory() -> Result<PathBuf> {
    let parent = Path::new("target/recording-quality-request");
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let directory = parent.join(format!("native-{}-{stamp}-{index}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法分配高级画质请求往返的独占目录".into())
}

fn write_report(directory: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    if bytes.len() > REPORT_LIMIT {
        return Err("高级画质请求往返报告超过大小上限".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(())
}

fn assert_backend_request(diagnostics: &Value, expected_plan: &Value) {
    // 早期创建失败直接携带 trace；已创建 writer 和媒体类型失败把它放在 encoder_attempt。
    let backend_plan = diagnostics
        .pointer("/encoder_attempt/quality")
        .or_else(|| diagnostics.get("quality"))
        .expect("后端尝试诊断缺少实际接收的 QualityPlan");
    assert_eq!(backend_plan, expected_plan, "{diagnostics}");
}

fn assert_gop_request(diagnostics: &Value, expected: GopRequest) {
    let (frames, source) = match expected {
        GopRequest::Frames(frames) => (Some(frames), "codec_api_before_begin_writing"),
        GopRequest::EncoderDefault => (None, "explicit_request_withdrawal"),
        GopRequest::ProductDefault => panic!("本用例只核对显式 GOP 请求或撤回"),
    };
    assert_eq!(diagnostics["gop_request"]["mode"], json!(expected));
    assert_eq!(
        diagnostics["gop_request"]["requested_frames"],
        json!(frames)
    );
    assert_eq!(diagnostics["gop_request"]["unit"], "frames");
    let checks = diagnostics["parameter_checks"]
        .as_array()
        .expect("已创建 writer 缺少逐项参数诊断");
    let matching: Vec<_> = checks
        .iter()
        .filter(|check| check["request"]["key"] == "gop_frames")
        .collect();
    assert_eq!(matching.len(), 1, "GOP 诊断缺失或重复：{diagnostics}");
    let check = matching[0];
    assert_eq!(check["request"]["value"], json!(frames));
    assert_eq!(check["request"]["source"], source);
    for field in ["observed", "read_error", "matches_request"] {
        assert!(check.get(field).is_some(), "GOP 缺少 {field}：{check}");
    }
    let set = check["request"]
        .get("set_succeeded")
        .expect("GOP 缺少 Set 反馈字段");
    assert!(set.is_null() || set.as_bool().is_some(), "{check}");
    if expected == GopRequest::EncoderDefault {
        assert!(set.is_null(), "撤回 GOP 请求不得产生 Set 反馈：{check}");
        assert!(diagnostics.get("gop").is_none_or(Value::is_null));
        assert!(check["matches_request"].is_null());
    }
    // Set 返回值及读回可能未知或不匹配；这里不把任何一种当作精确 IDR 周期证明。
}

fn assert_parameter_requests(diagnostics: &Value) {
    let checks = diagnostics["parameter_checks"]
        .as_array()
        .expect("已创建 writer 缺少逐项参数诊断");
    for (key, expected) in [
        ("rate_control", 1_u64),
        ("mean_bitrate", 2_000_000),
        ("max_bitrate", 4_000_000),
        // 逻辑计划以 bit 表示；原生 CommonBufferSize 请求及回读以 byte 表示。
        ("buffer_bytes", 500_000),
    ] {
        let matching: Vec<_> = checks
            .iter()
            .filter(|check| check["request"]["key"].as_str() == Some(key))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "参数 {key} 诊断缺失或重复：{diagnostics}"
        );
        let check = matching[0];
        assert_eq!(check["request"]["value"].as_u64(), Some(expected));
        assert_eq!(check["request"]["source"], "encoding_parameters");
        // 整体媒体类型接受不等于逐项 Set 成功；未知和不匹配都是必须保留的真实结果。
        assert!(check["request"]["set_succeeded"].is_null());
        for field in ["observed", "read_error", "matches_request"] {
            assert!(
                check.get(field).is_some(),
                "参数 {key} 缺少 {field}：{check}"
            );
        }
    }
    let speed: Vec<_> = checks
        .iter()
        .filter(|check| check["request"]["key"] == "quality_vs_speed")
        .collect();
    assert_eq!(speed.len(), 1, "质量/速度诊断缺失或重复：{diagnostics}");
    assert_eq!(speed[0]["request"]["value"], 100);
    assert_eq!(
        speed[0]["request"]["source"],
        "codec_api_before_begin_writing"
    );
    for field in ["observed", "read_error", "matches_request"] {
        assert!(speed[0].get(field).is_some(), "质量/速度缺少 {field}");
    }
    assert!(
        speed[0]["request"]["set_succeeded"].is_null()
            || speed[0]["request"]["set_succeeded"].as_bool().is_some()
    );
    assert_eq!(diagnostics["output_request"]["width"].as_u64(), Some(384));
    assert_eq!(diagnostics["output_request"]["height"].as_u64(), Some(256));
    assert_eq!(
        diagnostics["output_request"]["fps"].as_u64(),
        Some(u64::from(FPS))
    );
    assert_eq!(
        diagnostics["output_request"]["avg_bitrate_bps"].as_u64(),
        Some(2_000_000)
    );
    assert_gop_request(diagnostics, GopRequest::Frames(6));
}

fn assert_roundtrip(trial: &Value, directory: &Path) {
    assert_eq!(
        trial["encoded_and_reopened"],
        true,
        "该高级请求未通过真实试编码与完整回读，证据目录：{}；{trial}",
        directory.display()
    );
    let attempts = trial["attempts"].as_array().expect("缺少尝试报告");
    let attempt = attempts.last().expect("缺少成功尝试报告");
    let encoder = &attempt["encoder"];
    for stage in ["first_video_sample", "first_audio_sample", "finalize"] {
        assert_eq!(encoder["encoding_trial"][stage]["state"], "succeeded");
    }
    let reopened = &attempt["reopen"];
    assert_eq!(reopened["video_frames"].as_u64(), Some(FRAMES));
    assert!(reopened["audio_blocks"]
        .as_u64()
        .is_some_and(|blocks| blocks > 0));
    // audio_samples 是解码器返回的交错样本值数，不冒充音频帧数或持续同步证据。
    assert!(reopened["audio_samples"]
        .as_u64()
        .is_some_and(|samples| samples > 0));
    let payload = &reopened["video_payload"];
    assert_eq!(payload["complete"], true, "{payload}");
    assert_eq!(payload["samples"].as_u64(), Some(FRAMES));
    assert!(payload["payload_bytes"]
        .as_u64()
        .is_some_and(|bytes| bytes > 0));
    assert!(payload["actual_payload_bitrate_bps"]
        .as_f64()
        .is_some_and(|bitrate| bitrate.is_finite() && bitrate > 0.));
}

#[test]
#[ignore = "固定高级请求的原生 H.264/AAC 静音短片往返；不打开窗口或播放声音"]
fn recording_quality_request_native_roundtrip() -> Result<()> {
    let directory = unique_directory()?;
    let request = resolve(
        SOURCE_WIDTH,
        SOURCE_HEIGHT,
        FPS,
        QualityProfile::Current(Preset::CompactMp4),
        QualityOverrides {
            avg_bitrate_bps: Override::Set(2_000_000),
            rate_control: Override::Set(RateControl::PeakConstrainedVbr),
            quality_vs_speed: Override::Set(100),
            max_bitrate_bps: OptionalOverride::Set(4_000_000),
            buffer_bits: OptionalOverride::Set(4_000_000),
            gop_frames: OptionalOverride::Set(6),
        },
    )?;
    let stop = AtomicBool::new(false);
    let generated = (|| -> Result<Value> {
        let trial_directory = directory.join("trial");
        fs::create_dir(&trial_directory)?;
        let _runtime = MfRuntime::new()?;
        run_quality_case(&trial_directory, EncoderPolicy::Auto, request.plan, &stop)
    })();
    // 在所有断言前保留请求、各次真实诊断及失败原因；后端拒绝不能被当成跳过或成功。
    write_report(
        &directory,
        &json!({
            "requested_quality": request,
            "fixture": {"source_width": SOURCE_WIDTH, "source_height": SOURCE_HEIGHT,
                "encoded_width": 384, "encoded_height": 256, "fps": FPS, "frames": FRAMES,
                "audio_is_synthetic_silence": true, "audio_output": false},
            "trial": generated.as_ref().ok(),
            "error": generated.as_ref().err().map(|error| error.to_string()),
            "scope": "one fixed advanced request through the existing bounded Auto trial; success requires actual write, Finalize, complete video/audio decode and native compressed video payload reading; not proof that every CodecAPI request matched or every parameter range is supported; no real text, scrolling, motion, desktop, HDR or device color validation",
            "parameter_evidence_scope": "each attempt retains its backend QualityPlan and available encoder diagnostics; QualityPlan.buffer_bits retains the logical 4000000-bit request, while buffer_bytes uses native bytes for request/readback, with a 500000-byte request; GOP requests a maximum picture count of 6, and its Set feedback/readback may be unknown or mismatched; effective GOP and bitstream keyframe/IDR period remain unverified; clean_points are Reader metadata only, not bitstream parsing; actual encoder identity comes from encoding_trial.inspection.video.encoder and may remain unknown",
        }),
    )?;
    let trial = generated?;
    assert_eq!(request.plan.parameters.buffer_bits, Some(4_000_000));
    assert_eq!(request.plan.parameters.quality_vs_speed, Some(100));
    assert_eq!(request.plan.parameters.gop_request, GopRequest::Frames(6));
    let expected_plan = serde_json::to_value(request.plan)?;
    assert_eq!(trial["requested_plan"], expected_plan);
    let attempts = trial["attempts"]
        .as_array()
        .expect("原生高级请求没有尝试报告");
    assert!(!attempts.is_empty(), "{trial}");
    for attempt in attempts {
        if let Some(encoder) = attempt.get("encoder") {
            assert_backend_request(encoder, &expected_plan);
            assert_parameter_requests(encoder);
        } else {
            let creation = &attempt["creation_error"];
            assert_backend_request(&creation["diagnostics"], &expected_plan);
            assert!(creation.get("stage").is_some(), "{creation}");
            assert!(creation.get("cause").is_some(), "{creation}");
        }
    }
    assert_roundtrip(&trial, &directory);
    Ok(())
}

#[test]
#[ignore = "显式 GOP 请求及撤回的 H.264/AAC 静音往返；不验证精确 IDR 周期，不打开窗口"]
fn recording_gop_request_native_explicit_and_encoder_default() -> Result<()> {
    let directory = unique_directory()?;
    let stop = AtomicBool::new(false);
    let mut cases = Vec::with_capacity(2);
    for (name, gop_frames) in [
        ("frames_6", OptionalOverride::Set(6)),
        ("encoder_default", OptionalOverride::Clear),
    ] {
        let request = resolve(
            SOURCE_WIDTH,
            SOURCE_HEIGHT,
            FPS,
            QualityProfile::Current(Preset::CompactMp4),
            QualityOverrides {
                gop_frames,
                ..Default::default()
            },
        )?;
        let generated = (|| -> Result<Value> {
            let trial_directory = directory.join(name);
            fs::create_dir(&trial_directory)?;
            let _runtime = MfRuntime::new()?;
            run_quality_case(&trial_directory, EncoderPolicy::Auto, request.plan, &stop)
        })();
        cases.push((name, request, generated));
    }
    // 两个独占 case 均完成后先写全部真实结果，再作任何成功断言。
    write_report(
        &directory,
        &json!({
            "fixture": {"source_width": SOURCE_WIDTH, "source_height": SOURCE_HEIGHT,
                "encoded_width": 384, "encoded_height": 256, "fps": FPS, "frames": FRAMES,
                "audio_is_synthetic_silence": true, "audio_output": false},
            "cases": cases.iter().map(|(name, request, generated)| json!({
                "name": name, "requested_quality": request,
                "trial": generated.as_ref().ok(),
                "error": generated.as_ref().err().map(|error| error.to_string()),
            })).collect::<Vec<_>>(),
            "scope": "two explicit H.264 requests through the existing bounded Auto trial, using only the fixed 12-frame synthetic chart and AAC silence; each case must actually write, Finalize, fully decode video/audio and completely read native compressed video payload; unavailable or rejected encoding fails the test after preserving the report; no camera, desktop, HDR or long-session synchronization validation",
            "gop_evidence_scope": "Frames(6) requests a maximum picture count of 6; Clear withdraws the request, sends no GOP Set and must retain null Set feedback; CodecAPI observed/matches_request may be null or mismatched; effective GOP and exact bitstream keyframe/IDR period remain unverified; MFSampleExtension_CleanPoint is Source Reader metadata only and does not prove parsed IDR or display frame indexes",
        }),
    )?;
    for (name, request, generated) in cases {
        let trial = generated?;
        let expected = if name == "frames_6" {
            GopRequest::Frames(6)
        } else {
            GopRequest::EncoderDefault
        };
        assert_eq!(request.plan.parameters.gop_request, expected);
        let expected_plan = serde_json::to_value(request.plan)?;
        assert_eq!(trial["requested_plan"], expected_plan);
        let attempts = trial["attempts"].as_array().expect("缺少 GOP 尝试报告");
        assert!(!attempts.is_empty(), "{trial}");
        for attempt in attempts {
            if let Some(encoder) = attempt.get("encoder") {
                assert_backend_request(encoder, &expected_plan);
                assert_gop_request(encoder, expected);
            } else {
                let creation = &attempt["creation_error"];
                assert_backend_request(&creation["diagnostics"], &expected_plan);
                assert!(creation.get("stage").is_some(), "{creation}");
                assert!(creation.get("cause").is_some(), "{creation}");
            }
        }
        assert_roundtrip(&trial, &directory.join(name));
    }
    Ok(())
}
