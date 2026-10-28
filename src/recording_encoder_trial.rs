//! 策略后端的有界真实试编码，复用产品 writer、色块夹具和系统回读。
//! 每次尝试使用独立自有暂存，不发布成片；严格策略不可用与试编码成功分别报告。
#[cfg(test)]
#[path = "recording_av1_native.rs"]
mod av1_native;
#[cfg(test)]
#[path = "recording_codec_fallback_native.rs"]
mod codec_fallback_native;
#[cfg(test)]
#[path = "recording_hevc_native.rs"]
mod hevc_native;
#[path = "recording_trial_measurements.rs"]
pub(crate) mod measurements;
#[cfg(test)]
#[path = "recording_quality_request_native.rs"]
mod quality_request_native;

use crate::{
    geometry::ScreenRect,
    media_foundation::{
        classify_recording_error, recording_failure_after_finalize, AudioReader,
        EncoderAttemptError, EncoderBackendSession, MfRuntime, VideoReader, AUDIO_CHANNELS,
        AUDIO_RATE,
    },
    recording_color_check::ColorChart,
    recording_encoder_caps::VideoCodec,
    recording_encoder_policy::{
        codec::{CodecAttempt, CodecDecision, CodecPlan, CodecRequest, CodecSequence},
        EncoderPolicy, FailureCause, FailureDecision, FailureStage, RetryContext,
    },
    recording_options::{encoded_size, quality_plan, Preset, QualityPlan, QualityProfile},
    recording_size_layout::RecordingSizeLayout,
    Result,
};
use measurements::TrialMeasurement;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FPS: u32 = 30;
const FRAMES: u32 = 12;
const SOURCE_WIDTH: u32 = 385;
const SOURCE_HEIGHT: u32 = 257;
const BUDGET: Duration = Duration::from_secs(30);

fn output_directory(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let path = parent.join(format!(
            "encoder-trial-{}-{stamp}-{index}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法分配编码试验目录".into())
}

fn check_read_budget(stop: &AtomicBool, started: Instant) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err("编码试验已取消".into());
    }
    if started.elapsed() > BUDGET {
        return Err("编码试验回读超过调用间预算".into());
    }
    Ok(())
}

fn output_file_size(path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(metadata) => json!({"present": true, "is_file": metadata.is_file(),
            "bytes": metadata.is_file().then_some(metadata.len()),
            "scope": "whole container file; includes audio and container overhead"}),
        Err(error) => {
            json!({"present": if error.kind() == ErrorKind::NotFound { Some(false) } else { None },
            "bytes": null, "error": error.to_string()})
        }
    }
}

fn decode_clip(
    path: &Path,
    chart: &ColorChart,
    codec: VideoCodec,
    stop: &AtomicBool,
    started: Instant,
) -> Result<Value> {
    check_read_budget(stop, started)?;
    let mut video = VideoReader::open(path)?;
    if video.info.codec != codec.subtype() {
        return Err("编码试验成片的实际视频编码与请求不符".into());
    }
    use windows::Win32::Media::MediaFoundation::{MFAudioFormat_AAC, MFMediaType_Audio};
    if video
        .info
        .streams
        .iter()
        .find(|stream| stream.major_type == MFMediaType_Audio)
        .and_then(|stream| stream.codec)
        != Some(MFAudioFormat_AAC)
    {
        return Err("编码试验未确认实际 AAC 音轨".into());
    }
    let mut frames = 0;
    let mut max_error = 0_f64;
    let mut max_frame_time_error_ns = 0_u64;
    loop {
        check_read_budget(stop, started)?;
        let Some(frame) = video.next_cancellable(stop)? else {
            break;
        };
        let expected_time =
            Duration::from_nanos(u64::from(frames) * 1_000_000_000 / u64::from(FPS));
        let time_error = frame.time.abs_diff(expected_time);
        if time_error > Duration::from_millis(1) {
            return Err("编码试验解码帧的展示时间与素材不符".into());
        }
        max_frame_time_error_ns = max_frame_time_error_ns.max(time_error.as_nanos() as u64);
        frames += 1;
        if frames > FRAMES + 8 {
            return Err("编码试验回读帧数超过上限".into());
        }
        let comparison = chart.compare(&frame.bgra, video.width, video.height, 12.)?;
        max_error = max_error.max(comparison.max_mean_channel_error);
        if !comparison.passed {
            return Err("编码试验色块回读未通过".into());
        }
    }
    if frames != FRAMES {
        return Err("编码试验未回读到全部视频帧".into());
    }
    check_read_budget(stop, started)?;
    let mut audio = AudioReader::open(path)?.ok_or("编码试验未产生 AAC 音轨")?;
    if audio.rate != AUDIO_RATE || audio.channels != AUDIO_CHANNELS as usize {
        return Err("编码试验的解码音轨格式与素材不符".into());
    }
    let mut blocks = 0;
    let mut samples = 0;
    loop {
        check_read_budget(stop, started)?;
        let Some(block) = audio.next()? else { break };
        blocks += 1;
        if blocks > 256 || block.samples.iter().any(|sample| !sample.is_finite()) {
            return Err("编码试验音轨无效或超过块数上限".into());
        }
        samples += block.samples.len();
    }
    if samples == 0 {
        return Err("编码试验音轨未返回样本".into());
    }
    check_read_budget(stop, started)?;
    Ok(
        json!({"video_frames": frames, "max_mean_channel_error": max_error,
        "max_frame_time_error_ns": max_frame_time_error_ns, "frame_time_tolerance_ns": 1_000_000,
        "native_video_codec": codec, "native_audio_codec": "aac",
        "audio_blocks": blocks, "audio_samples": samples, "audio_is_synthetic_silence": true}),
    )
}

fn read_payload(
    path: &Path,
    codec: VideoCodec,
    stop: &AtomicBool,
    started: Instant,
) -> Result<Value> {
    check_read_budget(stop, started)?;
    let payload =
        crate::recording::bitrate_check::inspect_for_codec_cancellable(path, codec, &|| {
            stop.load(Ordering::Acquire) || started.elapsed() > BUDGET
        });
    check_read_budget(stop, started)?;
    let payload = payload?;
    if !payload_read_complete(&payload, codec) {
        return Err(trf!(
            "编码试验负载未完整回读：{payload}",
            "Encoder trial payload reading is incomplete: {payload}"
        )
        .into());
    }
    Ok(payload)
}

fn payload_read_complete(payload: &Value, codec: VideoCodec) -> bool {
    if payload["complete"] != true
        || !payload["samples"].as_u64().is_some_and(|count| count > 0)
        || !payload["payload_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
        || payload["invalid_durations"].as_u64() != Some(0)
    {
        return false;
    }
    // AV1 压缩样本边界不作为展示帧数证据；缺少有效 duration 时不推算码率。
    codec == VideoCodec::Av1
        || (payload["samples"].as_u64() == Some(u64::from(FRAMES))
            && payload["actual_payload_bitrate_bps"]
                .as_f64()
                .is_some_and(|bitrate| bitrate.is_finite() && bitrate > 0.))
}

/// 统一后端的短片试验；每个调用方须提供独占目录，避免候选间覆盖。
/// 调用线程须在整次试验期间持有 MfRuntime。
pub(crate) fn run_profile_case(
    directory: &Path,
    policy: EncoderPolicy,
    profile: QualityProfile,
    stop: &AtomicBool,
) -> Result<Value> {
    let quality = quality_plan(SOURCE_WIDTH, SOURCE_HEIGHT, FPS, profile)?;
    run_quality_case(directory, policy, quality, stop)
}

/// 使用已解析请求，但不改变固定短片素材、尝试策略或回读门。
/// 调用方须持有 MfRuntime，并提供独占目录；计划冲突在创建后端前拒绝。
pub(crate) fn run_quality_case(
    directory: &Path,
    policy: EncoderPolicy,
    quality: QualityPlan,
    stop: &AtomicBool,
) -> Result<Value> {
    run_codec_case(directory, policy, quality, VideoCodec::H264, stop)
}

/// 独立 codec 技术验证；录制会话与默认输出仍通过旧 H.264 入口。
/// 单 codec 签名和旧字段保持兼容，不据目录或协商成功开放产品功能。
pub(crate) fn run_codec_case(
    directory: &Path,
    policy: EncoderPolicy,
    quality: QualityPlan,
    codec: VideoCodec,
    stop: &AtomicBool,
) -> Result<Value> {
    let request = match codec {
        VideoCodec::H264 => CodecRequest::H264,
        VideoCodec::Hevc => CodecRequest::Hevc,
        VideoCodec::Av1 => CodecRequest::Av1,
    };
    let mut report = run_request_case(directory, policy, quality, request, stop)?;
    report["codec"] = serde_json::to_value(codec)?;
    Ok(report)
}

/// 显式 codec 请求的统一短片驱动；各 codec 复用同一份画质和局部策略。
/// 只在纯模型允许的未接纳样本阶段换 codec，不发布、删除或覆盖任何尝试。
pub(crate) fn run_request_case(
    directory: &Path,
    policy: EncoderPolicy,
    quality: QualityPlan,
    request: CodecRequest,
    stop: &AtomicBool,
) -> Result<Value> {
    run_request_case_with_hook(directory, policy, quality, request, stop, |_| None)
}

// 注入 hook 只在内部使用；所有公开入口始终传入无故障 hook，测试也复用同一写帧循环。
fn run_request_case_with_hook(
    directory: &Path,
    policy: EncoderPolicy,
    quality: QualityPlan,
    request: CodecRequest,
    stop: &AtomicBool,
    before_create: impl FnMut(CodecAttempt) -> Option<EncoderAttemptError>,
) -> Result<Value> {
    let plan = CodecPlan::new(request, policy, quality)?;
    let expected = encoded_size(SOURCE_WIDTH, SOURCE_HEIGHT);
    if (quality.width, quality.height) != expected || quality.fps != FPS {
        return Err(tr!(
            "编码试验的计划必须匹配固定 384×256、30 fps 素材",
            "The encoder trial plan must match its fixed 384×256, 30 fps samples"
        )
        .into());
    }
    let codec_order = serde_json::to_value(plan.codec_order())?;
    let mut report = run_policy(directory, plan, stop, before_create)?;
    report["codec_request"] = serde_json::to_value(request)?;
    report["requested_codec_order"] = codec_order;
    report["encoded_and_finalized"] = report["attempts"]
        .as_array()
        .is_some_and(|attempts| {
            attempts
                .iter()
                .any(|attempt| attempt["encoding_finalized"] == true)
        })
        .into();
    report["quality_profile"] = serde_json::to_value(quality.profile)?;
    report["requested_plan"] = serde_json::to_value(quality)?;
    Ok(report)
}

fn run_policy(
    directory: &Path,
    plan: CodecPlan,
    stop: &AtomicBool,
    mut before_create: impl FnMut(CodecAttempt) -> Option<EncoderAttemptError>,
) -> Result<Value> {
    let layout = RecordingSizeLayout::new(ScreenRect {
        left: -384,
        top: -256,
        width: SOURCE_WIDTH,
        height: SOURCE_HEIGHT,
    })?;
    let encoder_plan = plan.encoder_plan().clone();
    let policy = encoder_plan.policy();
    let mut sequence = CodecSequence::new(plan);
    let mut backend = None;
    let chart = ColorChart::new(layout)?;
    let silence = vec![0_i16; (AUDIO_RATE / FPS * AUDIO_CHANNELS) as usize];
    let mut attempts = Vec::new();
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(
                json!({"policy": policy, "selected_codec": null, "encoded_and_reopened": false,
                    "cancelled": true, "attempts": attempts}),
            );
        }
        let attempt = sequence.current().ok_or("编码试验序列已终止")?;
        // 单 codec 的 slot 与旧 index 相同；换 codec 后不会复用先前失败的文件名。
        let path = directory.join(format!("{policy:?}-{}.mp4", attempt.slot));
        let mut trace = json!({"index": attempt.slot, "slot": attempt.slot,
            "codec": attempt.codec, "attempt_index": attempt.attempt_index, "path": path,
            "retry_context": RetryContext::default()});
        let creation_measurement = TrialMeasurement::start();
        let created = match before_create(attempt) {
            Some(error) => Err(error),
            None => {
                let (codec, session) = backend.get_or_insert_with(|| {
                    (
                        attempt.codec,
                        EncoderBackendSession::new_for_codec(
                            encoder_plan.clone(),
                            true,
                            attempt.codec,
                        ),
                    )
                });
                if *codec != attempt.codec {
                    *codec = attempt.codec;
                    *session = EncoderBackendSession::new_for_codec(
                        encoder_plan.clone(),
                        true,
                        attempt.codec,
                    );
                }
                session.create_attempt(&path, attempt.attempt_index, None)
            }
        };
        trace["creation_measurement"] = creation_measurement.finish();
        let mut writer = match created {
            Ok(writer) => writer,
            Err(error) => {
                let decision =
                    sequence.failed(error.stage, error.cause.clone(), RetryContext::default());
                trace["creation_error"] = json!(error);
                trace["output_file"] = output_file_size(&path);
                trace["decision"] = json!(legacy_decision(&decision));
                trace["codec_decision"] = json!(decision);
                attempts.push(trace);
                match decision {
                    CodecDecision::Retry { .. } => continue,
                    CodecDecision::Stop { .. } => {
                        return Ok(json!({"policy": policy, "selected_codec": null,
                                "encoded_and_reopened": false, "attempts": attempts}))
                    }
                }
            }
        };
        let encoding_measurement = TrialMeasurement::start();
        let started = Instant::now();
        let mut context = RetryContext {
            first_video_sample_retained: true,
            ..Default::default()
        };
        let mut failure = None;
        for frame in 0..FRAMES {
            if stop.load(Ordering::Acquire) {
                failure = Some((FailureStage::Recording, FailureCause::Cancelled));
                break;
            }
            if started.elapsed() > BUDGET {
                failure = Some((
                    FailureStage::Recording,
                    FailureCause::Backend {
                        message: "编码试验超过调用间预算".into(),
                    },
                ));
                break;
            }
            let time = Duration::from_nanos(u64::from(frame) * 1_000_000_000 / u64::from(FPS));
            let end = Duration::from_nanos(u64::from(frame + 1) * 1_000_000_000 / u64::from(FPS));
            if let Err(error) =
                writer.write_video(chart.bgra(), layout.source_width, time, end - time)
            {
                let stage = if context.accepted_video_samples == 0 {
                    FailureStage::FirstVideoSample
                } else {
                    FailureStage::Recording
                };
                failure = Some((stage, classify_recording_error(error.as_ref())));
                break;
            }
            context.accepted_video_samples += 1;
            if let Err(error) = writer.write_audio(&silence, time) {
                let stage = if context.accepted_audio_samples == 0 {
                    FailureStage::FirstAudioSample
                } else {
                    FailureStage::Recording
                };
                failure = Some((stage, classify_recording_error(error.as_ref())));
                break;
            }
            context.accepted_audio_samples += 1;
        }
        trace["retry_context"] = json!(context);
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        trace["write_and_finalize_measurement"] = encoding_measurement.finish();
        trace["output_file"] = output_file_size(&path);
        let finalize_error = finished.as_ref().err().map(ToString::to_string);
        if let Some((stage, cause)) = failure {
            let cause = recording_failure_after_finalize(cause, &finished);
            let decision = sequence.failed(stage, cause.clone(), context);
            trace["stage"] = json!(stage);
            trace["cause"] = json!(cause);
            trace["decision"] = json!(legacy_decision(&decision));
            trace["codec_decision"] = json!(decision);
            trace["encoder"] = diagnostics;
            trace["finalize_error"] = json!(finalize_error);
            attempts.push(trace);
            match decision {
                CodecDecision::Retry { .. } => continue,
                CodecDecision::Stop { .. } => {
                    return Ok(json!({"policy": policy, "selected_codec": null,
                            "encoded_and_reopened": false, "attempts": attempts}))
                }
            }
        }
        if let Err(error) = finished {
            let decision = sequence.failed(
                FailureStage::Finalize,
                classify_recording_error(error.as_ref()),
                context,
            );
            trace["encoder"] = diagnostics;
            trace["finalize_error"] = json!(error.to_string());
            trace["decision"] = json!(legacy_decision(&decision));
            trace["codec_decision"] = json!(decision);
            attempts.push(trace);
            return Ok(json!({"policy": policy, "selected_codec": null,
                    "encoded_and_reopened": false, "attempts": attempts}));
        }
        let read_started = Instant::now();
        // 先读取原生负载，解码器缺失时仍保留已封装压缩数据的独立证据。
        let payload_measurement = TrialMeasurement::start();
        let payload = read_payload(&path, attempt.codec, stop, read_started);
        trace["payload_measurement"] = payload_measurement.finish();
        let decode_measurement = TrialMeasurement::start();
        let mut decoded = decode_clip(&path, &chart, attempt.codec, stop, read_started);
        trace["decode_measurement"] = decode_measurement.finish();
        let verified = decoded.is_ok() && payload.is_ok();
        // 只有原生压缩读取或完整解码确认证据才填实际选中格式；请求标签不能替代观测。
        let selected_codec = (payload.is_ok() || decoded.is_ok()).then_some(attempt.codec);
        if let Ok(reopened) = &mut decoded {
            reopened["video_payload"] = payload.as_ref().ok().cloned().unwrap_or(Value::Null);
        }
        trace["encoder"] = diagnostics;
        trace["encoding_finalized"] = true.into();
        trace["video_payload"] = json!(payload.as_ref().ok());
        trace["video_payload_error"] = json!(payload.err().map(|error| error.to_string()));
        trace["reopen"] = json!(decoded.as_ref().ok());
        trace["decode_completed"] = decoded.is_ok().into();
        trace["reopen_error"] = json!(decoded.err().map(|error| error.to_string()));
        attempts.push(trace);
        return Ok(json!({"policy": policy, "selected_codec": selected_codec,
                "encoded_and_reopened": verified, "attempts": attempts}));
    }
}

fn legacy_decision(decision: &CodecDecision) -> FailureDecision {
    match decision {
        CodecDecision::Retry {
            attempt,
            replay_first_video,
            ..
        } => FailureDecision::Retry {
            attempt_index: attempt.attempt_index,
            attempt: attempt.attempt,
            replay_first_video: *replay_first_video,
        },
        CodecDecision::Stop { reason } => FailureDecision::Stop {
            reason: reason.clone(),
        },
    }
}

/// 首样本、Finalize、重新读取分别记录；严格模式失败不会转成“可录”。
/// baseline_ok 仅要求 Auto 真正往返，严格策略是否可用看各自 encoded_and_reopened。
pub(crate) fn run(directory: &Path, stop: &AtomicBool) -> Result<Value> {
    let output = output_directory(directory)?;
    let _runtime = MfRuntime::new()?;
    let mut cases = Vec::new();
    for policy in [
        EncoderPolicy::Auto,
        EncoderPolicy::HardwarePreferred,
        EncoderPolicy::HardwareOnly,
        EncoderPolicy::SoftwareOnly,
    ] {
        cases.push(run_profile_case(
            &output,
            policy,
            QualityProfile::Current(Preset::CompactMp4),
            stop,
        )?);
        if stop.load(Ordering::Acquire) {
            break;
        }
    }
    let report = json!({
        "baseline_ok": cases.first().is_some_and(|case| case["encoded_and_reopened"] == true),
        "audio_output": false, "synthetic_only": true, "output": output, "cases": cases,
    });
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("report.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_encoder_trial_av1_payload_samples_do_not_prove_display_frames() {
        let mut payload = json!({"complete": true, "samples": 3, "payload_bytes": 1024,
            "invalid_durations": 0, "unknown_durations": 3, "missing_timestamps": 0,
            "actual_payload_bitrate_bps": null});
        assert!(payload_read_complete(&payload, VideoCodec::Av1));
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            assert!(!payload_read_complete(&payload, codec));
        }
        for (key, value) in [
            ("complete", json!(false)),
            ("samples", json!(0)),
            ("payload_bytes", json!(0)),
            ("invalid_durations", json!(1)),
        ] {
            let previous = payload[key].clone();
            payload[key] = value;
            assert!(
                !payload_read_complete(&payload, VideoCodec::Av1),
                "{payload}"
            );
            payload[key] = previous;
        }
        payload["samples"] = json!(FRAMES);
        payload["actual_payload_bitrate_bps"] = json!(20_480.);
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            assert!(payload_read_complete(&payload, codec));
        }
    }

    #[test]
    fn recording_encoder_trial_single_codec_decisions_keep_legacy_json() {
        let quality = quality_plan(
            SOURCE_WIDTH,
            SOURCE_HEIGHT,
            FPS,
            QualityProfile::Current(Preset::CompactMp4),
        )
        .unwrap();
        for policy in EncoderPolicy::ALL {
            let plan = CodecPlan::new(CodecRequest::H264, policy, quality).unwrap();
            let count = plan.attempts().len();
            let mut legacy =
                crate::recording_encoder_policy::AttemptSequence::new(plan.encoder_plan().clone());
            let mut sequence = CodecSequence::new(plan);
            for index in 0..count {
                let current = sequence.current().unwrap();
                assert_eq!(current.slot, index);
                assert_eq!(current.attempt_index, index);
                let cause = FailureCause::Backend {
                    message: "single-codec compatibility model".into(),
                };
                let expected = legacy.failed(
                    FailureStage::MediaTypes,
                    cause.clone(),
                    RetryContext::default(),
                );
                let actual =
                    sequence.failed(FailureStage::MediaTypes, cause, RetryContext::default());
                assert_eq!(json!(legacy_decision(&actual)), json!(expected));
            }
            assert!(sequence.current().is_none());
        }
    }

    #[test]
    #[ignore = "真实策略 H.264/AAC 试编码与回读；不打开窗口或播放声音"]
    fn recording_encoder_trial_native_policies() {
        let stop = AtomicBool::new(false);
        let report = run(Path::new("target/recording-encoder-trial"), &stop).unwrap();
        assert_eq!(
            report["baseline_ok"], true,
            "Auto 基线未通过，报告目录：{}",
            report["output"]
        );
        let attempt = report["cases"][0]["attempts"]
            .as_array()
            .and_then(|attempts| attempts.last())
            .unwrap();
        let payload = &attempt["reopen"]["video_payload"];
        assert_eq!(payload["complete"], true, "{payload}");
        assert_eq!(payload["samples"].as_u64(), Some(u64::from(FRAMES)));
        assert!(payload["payload_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0));
        assert!(payload["actual_payload_bitrate_bps"]
            .as_f64()
            .is_some_and(|bitrate| bitrate > 0.));
    }
}
