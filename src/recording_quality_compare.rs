//! 使用同一有界合成素材比较编码候选；实际协商、成片时间戳与质量指标分别记录。
//! 指标仅描述 ASCII 文字与简单几何素材，不代表真实桌面或 HDR 画质。
use super::profile_samples as samples;
use crate::{
    media_foundation::{classify_recording_error, EncoderBackendSession, MfRuntime, VideoReader},
    recording_encoder_policy::{
        AttemptSequence, EncoderPlan, EncoderPolicy, FailureCause, FailureDecision, FailureStage,
        RetryContext,
    },
    recording_options::{quality_plan, Preset, QualityProfile},
    Result,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FRAME_LIMIT: u32 = 90;
const PIXEL_LIMIT: u64 = 1920 * 1080;
const FILE_LIMIT: u64 = 64 * 1024 * 1024;
const REPORT_LIMIT: usize = 4 * 1024 * 1024;
const PTS_TOLERANCE_NS: u128 = 1_000_000;
const CASE_BUDGET: Duration = Duration::from_secs(60);
const RUN_BUDGET: Duration = Duration::from_secs(360);
const BASELINES: [QualityProfile; 2] = [
    QualityProfile::Current(Preset::CompactMp4),
    QualityProfile::Current(Preset::HighQualityMp4),
];
const CASE_COUNT: usize = BASELINES.len() + QualityProfile::EXPERIMENTS.len();

fn output_directory(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let path = parent.join(format!(
            "quality-compare-{}-{stamp}-{index}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(tr!(
        "无法分配画质比较目录",
        "Cannot allocate the quality comparison directory"
    )
    .into())
}

fn interruption(
    stop: &AtomicBool,
    case_start: Instant,
    run_start: Instant,
) -> Option<FailureCause> {
    if stop.load(Ordering::Acquire) {
        Some(FailureCause::Cancelled)
    } else if case_start.elapsed() > CASE_BUDGET || run_start.elapsed() > RUN_BUDGET {
        Some(FailureCause::Backend {
            message: tr!(
                "画质比较超过调用间预算",
                "Quality comparison exceeded the between-call budget"
            )
            .into(),
        })
    } else {
        None
    }
}

fn frame_time(index: u32) -> Duration {
    Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(samples::fps()))
}

fn source_length() -> Result<usize> {
    let pixels = u64::from(samples::width()) * u64::from(samples::height());
    if samples::width() == 0
        || samples::height() == 0
        || !samples::width().is_multiple_of(2)
        || !samples::height().is_multiple_of(2)
        || samples::width() > 1920
        || samples::height() > 1080
        || pixels > PIXEL_LIMIT
        || samples::fps() != 30
        || samples::frame_count() != FRAME_LIMIT
    {
        return Err(tr!(
            "画质合成素材超出固定比较范围",
            "Quality samples exceed the bounded comparison contract"
        )
        .into());
    }
    usize::try_from(pixels * 4).map_err(|error| error.into())
}

fn source_frame(index: u32, expected: usize) -> Result<Vec<u8>> {
    let frame = samples::sample(index)?;
    if frame.len() != expected || frame.chunks_exact(4).any(|pixel| pixel[3] != 255) {
        return Err(tr!(
            "画质合成素材像素布局无效",
            "Invalid quality sample pixel layout"
        )
        .into());
    }
    Ok(frame)
}

#[derive(Clone, Debug, Default)]
struct Errors {
    bgr_squared: u64,
    channels: u64,
    luma_squared: f64,
    pixels: u64,
}
impl Errors {
    fn add(&mut self, source: &[u8], decoded: &[u8]) -> Result<()> {
        if source.is_empty()
            || source.len() != decoded.len()
            || source.len() % 4 != 0
            || source.len() as u64 > PIXEL_LIMIT * 4
        {
            return Err(tr!(
                "画质误差输入像素范围无效",
                "Invalid pixel range for quality error measurement"
            )
            .into());
        }
        let mut bgr = 0_u64;
        let mut luma = 0_f64;
        for (source, decoded) in source.chunks_exact(4).zip(decoded.chunks_exact(4)) {
            let mut delta = [0_f64; 3];
            for channel in 0..3 {
                let difference = i32::from(decoded[channel]) - i32::from(source[channel]);
                bgr += (difference * difference) as u64;
                delta[channel] = f64::from(difference);
            }
            let difference = 0.0722 * delta[0] + 0.7152 * delta[1] + 0.2126 * delta[2];
            luma += difference * difference;
        }
        let pixels = u64::try_from(source.len() / 4)?;
        self.merge(&Self {
            bgr_squared: bgr,
            channels: pixels * 3,
            luma_squared: luma,
            pixels,
        })
    }
    fn merge(&mut self, other: &Self) -> Result<()> {
        let next_bgr = self
            .bgr_squared
            .checked_add(other.bgr_squared)
            .ok_or("quality squared error overflow")?;
        let next_pixels = self
            .pixels
            .checked_add(other.pixels)
            .ok_or("quality pixel count overflow")?;
        let next_channels = self
            .channels
            .checked_add(other.channels)
            .ok_or("quality channel count overflow")?;
        let next_luma = self.luma_squared + other.luma_squared;
        if !next_luma.is_finite() {
            return Err(tr!("亮度误差累计溢出", "Luma error accumulation overflowed").into());
        }
        self.bgr_squared = next_bgr;
        self.channels = next_channels;
        self.pixels = next_pixels;
        self.luma_squared = next_luma;
        Ok(())
    }
    fn report(&self) -> Value {
        let bgr_mse = (self.channels != 0).then(|| self.bgr_squared as f64 / self.channels as f64);
        let luma_mse = (self.pixels != 0).then(|| self.luma_squared / self.pixels as f64);
        json!({
            "pixels": self.pixels,
            "bgr": {"squared_error": self.bgr_squared, "channels": self.channels, "mse": bgr_mse,
                "psnr_db": bgr_mse.and_then(psnr), "exact_match": self.channels != 0 && self.bgr_squared == 0},
            "luma": {"squared_error": self.luma_squared, "mse": luma_mse,
                "psnr_db": luma_mse.and_then(psnr), "exact_match": self.pixels != 0 && self.luma_squared == 0.},
        })
    }
}

fn psnr(mse: f64) -> Option<f64> {
    (mse > 0. && mse.is_finite()).then(|| 10. * (255_f64 * 255. / mse).log10())
}

#[derive(Debug)]
struct SegmentErrors {
    name: &'static str,
    source_frames: u32,
    decoded_frames: u32,
    errors: Errors,
}
fn segment_catalog() -> Result<Vec<SegmentErrors>> {
    let mut catalog: Vec<SegmentErrors> = Vec::new();
    for index in 0..samples::frame_count() {
        let segment = samples::segment(index).ok_or("quality source segment missing")?;
        let name = segment.as_str();
        if let Some(existing) = catalog.iter_mut().find(|entry| entry.name == name) {
            existing.source_frames += 1;
        } else {
            if catalog.len() == 3 {
                return Err(tr!(
                    "画质素材片段超过三个",
                    "Quality samples contain more than three segments"
                )
                .into());
            }
            catalog.push(SegmentErrors {
                name,
                source_frames: 1,
                decoded_frames: 0,
                errors: Errors::default(),
            });
        }
    }
    if catalog.len() != 3 {
        return Err(tr!(
            "画质比较需要三个素材片段",
            "Quality comparison requires three sample segments"
        )
        .into());
    }
    Ok(catalog)
}

fn match_frame(time: Duration, seen: &mut [bool; FRAME_LIMIT as usize]) -> Result<u32> {
    let scaled = time
        .as_nanos()
        .checked_mul(u128::from(samples::fps()))
        .ok_or("quality PTS overflow")?;
    let index = scaled
        .checked_add(500_000_000)
        .ok_or("quality PTS overflow")?
        / 1_000_000_000;
    let index = u32::try_from(index)
        .ok()
        .filter(|index| *index < samples::frame_count())
        .ok_or(tr!(
            "画质回读帧时间越界",
            "Decoded quality frame time is out of range"
        ))?;
    if time.as_nanos().abs_diff(frame_time(index).as_nanos()) > PTS_TOLERANCE_NS {
        return Err(tr!(
            "画质回读帧无法匹配源时间戳",
            "Decoded quality frame does not match a source timestamp"
        )
        .into());
    }
    if std::mem::replace(&mut seen[index as usize], true) {
        return Err(tr!(
            "画质回读出现重复帧时间",
            "Quality decoding returned a duplicate frame time"
        )
        .into());
    }
    Ok(index)
}

fn evaluate(
    path: &Path,
    stop: &AtomicBool,
    case_start: Instant,
    run_start: Instant,
    expected: usize,
) -> Result<Value> {
    let mut reader = VideoReader::open(path)?;
    let mut segments = segment_catalog()?;
    let mut seen = [false; FRAME_LIMIT as usize];
    let mut frames = 0;
    let mut worst_pts_error = 0_u128;
    let mut total = Errors::default();
    loop {
        if let Some(cause) = interruption(stop, case_start, run_start) {
            return Err(format!("{cause:?}").into());
        }
        let Some(frame) = reader.next_cancellable(stop)? else {
            break;
        };
        if frames >= samples::frame_count() {
            return Err(tr!(
                "画质回读帧数超过上限",
                "Quality decoded frame count exceeds the limit"
            )
            .into());
        }
        if reader.width != samples::width()
            || reader.height != samples::height()
            || frame.bgra.len() != expected
        {
            return Err(tr!(
                "画质回读尺寸与源素材不一致",
                "Decoded quality dimensions do not match the source"
            )
            .into());
        }
        let index = match_frame(frame.time, &mut seen)?;
        worst_pts_error =
            worst_pts_error.max(frame.time.as_nanos().abs_diff(frame_time(index).as_nanos()));
        let source = source_frame(index, expected)?;
        let segment = samples::segment(index).ok_or("quality source segment missing")?;
        let bucket = segments
            .iter_mut()
            .find(|entry| entry.name == segment.as_str())
            .ok_or("quality source segment changed")?;
        let mut frame_errors = Errors::default();
        frame_errors.add(&source, &frame.bgra)?;
        bucket.errors.merge(&frame_errors)?;
        bucket.decoded_frames += 1;
        total.merge(&frame_errors)?;
        frames += 1;
    }
    if frames != samples::frame_count()
        || seen.iter().any(|seen| !seen)
        || segments
            .iter()
            .any(|entry| entry.decoded_frames != entry.source_frames)
    {
        return Err(tr!(
            "画质回读缺少源帧或片段",
            "Quality decoding is missing source frames or segments"
        )
        .into());
    }
    Ok(json!({
        "frames": frames, "source_frames": samples::frame_count(),
        "pts_tolerance_ns": PTS_TOLERANCE_NS as u64, "maximum_pts_error_ns": worst_pts_error as u64,
        "aggregate": total.report(),
        "segments": segments.iter().map(|entry| json!({"segment": entry.name,
            "source_frames": entry.source_frames, "decoded_frames": entry.decoded_frames,
            "metrics": entry.errors.report()})).collect::<Vec<_>>(),
    }))
}

fn failed_case(mut report: Value, attempts: Vec<Value>, stage: Value, failure: Value) -> Value {
    report["attempts"] = attempts.into();
    report["failure_stage"] = stage;
    report["failure"] = failure;
    report
}

fn validate_payload(payload: &Value) -> Result<()> {
    if payload["complete"] != true
        || payload["samples"].as_u64() != Some(u64::from(samples::frame_count()))
        || payload["payload_bytes"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || payload["actual_payload_bitrate_bps"]
            .as_f64()
            .is_none_or(|bitrate| !bitrate.is_finite() || bitrate <= 0.)
    {
        return Err("compressed payload statistics are incomplete".into());
    }
    const MISMATCH: &str = "compressed payload time span does not match the source";
    let signed_ticks = |key: &str| -> Result<i128> {
        payload[key]
            .as_i64()
            .map(i128::from)
            .ok_or_else(|| format!("{MISMATCH}: missing or invalid {key}").into())
    };
    let first = signed_ticks("minimum_pts_100ns")?;
    let last = signed_ticks("maximum_pts_100ns")?;
    let end = signed_ticks("maximum_end_100ns")?;
    let span = payload["observed_span_100ns"]
        .as_u64()
        .ok_or_else(|| format!("{MISMATCH}: missing or invalid observed_span_100ns"))?;
    // Scanner 的 span 是同一组原生 tick 的精确差值；不能靠容差接受自相矛盾的字段。
    if first > last || last >= end || i128::from(span) != end - first {
        return Err(
            format!("{MISMATCH}: reversed endpoints or contradictory observed span").into(),
        );
    }
    for (actual, expected) in [
        (first, frame_time(0)),
        (last, frame_time(samples::frame_count() - 1)),
        (end, frame_time(samples::frame_count())),
    ] {
        // i64 原生 tick × 100 和 u64 span × 100 均能无损放入这些更宽的整数。
        let expected = i128::try_from(expected.as_nanos())?;
        if (actual * 100).abs_diff(expected) > PTS_TOLERANCE_NS {
            return Err(format!("{MISMATCH}: a native timestamp exceeds the 1ms tolerance").into());
        }
    }
    let source_span = (frame_time(samples::frame_count()) - frame_time(0)).as_nanos();
    if (u128::from(span) * 100).abs_diff(source_span) > PTS_TOLERANCE_NS {
        return Err(format!("{MISMATCH}: native duration exceeds the 1ms tolerance").into());
    }
    Ok(())
}

fn run_case(
    directory: &Path,
    case_index: usize,
    profile: QualityProfile,
    stop: &AtomicBool,
    run_start: Instant,
    expected: usize,
) -> Result<Value> {
    let quality = quality_plan(samples::width(), samples::height(), samples::fps(), profile)?;
    let plan = EncoderPlan::new(EncoderPolicy::Auto, quality);
    let attempt_limit = plan.attempts().len();
    let session = EncoderBackendSession::new(plan.clone(), false);
    let mut sequence = AttemptSequence::new(plan);
    let mut report = json!({"profile": profile, "policy": EncoderPolicy::Auto,
        "requested_plan": quality, "encoded": false, "compared": false, "actual": null,
        "encode_ms": null, "decode_eval_ms": null, "payload_scan_ms": null, "bytes": null,
        "failure_stage": null, "failure": null, "attempts": []});
    let case_start = Instant::now();
    if let Some(cause) = interruption(stop, case_start, run_start) {
        return Ok(failed_case(
            report,
            Vec::new(),
            json!(FailureStage::InputInitialization),
            json!(cause),
        ));
    }
    let mut first_frame = Some(source_frame(0, expected)?);
    let mut attempts = Vec::with_capacity(attempt_limit);
    let mut index = 0;
    loop {
        if let Some(cause) = interruption(stop, case_start, run_start) {
            return Ok(failed_case(
                report,
                attempts,
                json!(FailureStage::InputInitialization),
                json!(cause),
            ));
        }
        if attempts.len() >= attempt_limit {
            return Ok(failed_case(
                report,
                attempts,
                json!("attempt_limit"),
                json!("encoder attempt limit reached"),
            ));
        }
        let path = directory.join(format!("case-{case_index}-attempt-{index}.mp4"));
        let mut writer = match session.create_attempt(&path, index, None) {
            Ok(writer) => writer,
            Err(error) => {
                let stage = error.stage;
                let cause = error.cause.clone();
                let decision = sequence.failed(stage, cause.clone(), RetryContext::default());
                attempts.push(json!({"index": index, "path": path, "creation_error": error, "decision": decision}));
                match decision {
                    FailureDecision::Retry { attempt_index, .. } => {
                        index = attempt_index;
                        continue;
                    }
                    FailureDecision::Stop { .. } => {
                        return Ok(failed_case(report, attempts, json!(stage), json!(cause)))
                    }
                }
            }
        };
        let mut context = RetryContext::default();
        let mut failure = None;
        for frame in 0..samples::frame_count() {
            if let Some(cause) = interruption(stop, case_start, run_start) {
                failure = Some((FailureStage::Recording, cause));
                break;
            }
            let generated = if frame == 0 {
                None
            } else {
                match source_frame(frame, expected) {
                    Ok(frame) => Some(frame),
                    Err(error) => {
                        failure = Some((
                            FailureStage::Recording,
                            FailureCause::Backend {
                                message: error.to_string(),
                            },
                        ));
                        break;
                    }
                }
            };
            let pixels = if frame == 0 {
                first_frame.as_deref()
            } else {
                generated.as_deref()
            };
            let Some(pixels) = pixels else {
                failure = Some((
                    FailureStage::Recording,
                    FailureCause::Backend {
                        message: "source frame was not retained".into(),
                    },
                ));
                break;
            };
            let time = frame_time(frame);
            let duration = frame_time(frame + 1) - time;
            if let Err(error) = writer.write_video(pixels, samples::width(), time, duration) {
                let stage = if context.accepted_video_samples == 0 {
                    FailureStage::FirstVideoSample
                } else {
                    FailureStage::Recording
                };
                failure = Some((stage, classify_recording_error(error.as_ref())));
                break;
            }
            context.accepted_video_samples += 1;
            if frame == 0 {
                // 只有真实 WriteSample 接受以后才释放原首帧，重试不得重新生成它冒充保留。
                first_frame.take();
            }
            match fs::metadata(&path) {
                Ok(metadata) if metadata.len() <= FILE_LIMIT => {}
                Ok(_) => {
                    failure = Some((
                        FailureStage::Recording,
                        FailureCause::Output {
                            message: tr!(
                                "画质比较输出超过文件大小上限",
                                "Quality comparison output exceeds the file size limit"
                            )
                            .into(),
                        },
                    ));
                    break;
                }
                Err(error) => {
                    failure = Some((
                        FailureStage::Recording,
                        FailureCause::Output {
                            message: error.to_string(),
                        },
                    ));
                    break;
                }
            }
        }
        context.first_video_sample_retained = first_frame.is_some();
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        let finalize_error = finished.as_ref().err().map(ToString::to_string);
        let encode_ms = case_start.elapsed().as_secs_f64() * 1000.;
        report["encode_ms"] = json!(encode_ms);
        report["actual"] = json!({"attempt_index": index, "writer": diagnostics});
        if let Some((stage, cause)) = failure {
            let mut retry_cause = cause.clone();
            if !matches!(cause, FailureCause::Cancelled) {
                if let Err(error) = &finished {
                    let secondary = classify_recording_error(error.as_ref());
                    if matches!(secondary, FailureCause::Output { .. }) {
                        retry_cause = secondary;
                    }
                }
            }
            let decision = sequence.failed(stage, retry_cause, context);
            attempts.push(
                json!({"index": index, "path": path, "stage": stage, "cause": cause,
                "accepted_video_samples": context.accepted_video_samples,
                "first_video_sample_retained": context.first_video_sample_retained,
                "encode_ms": encode_ms, "encoder": diagnostics,
                "finalize_error": finalize_error, "decision": decision}),
            );
            match decision {
                FailureDecision::Retry { attempt_index, .. } => {
                    index = attempt_index;
                    continue;
                }
                FailureDecision::Stop { .. } => {
                    return Ok(failed_case(report, attempts, json!(stage), json!(cause)))
                }
            }
        }
        if let Err(error) = finished {
            let cause = classify_recording_error(error.as_ref());
            let decision = sequence.failed(FailureStage::Finalize, cause.clone(), context);
            attempts.push(json!({"index": index, "path": path, "encoder": diagnostics,
                "accepted_video_samples": context.accepted_video_samples,
                "encode_ms": encode_ms, "finalize_error": error.to_string(), "decision": decision}));
            return Ok(failed_case(
                report,
                attempts,
                json!(FailureStage::Finalize),
                json!(cause),
            ));
        }
        attempts.push(json!({"index": index, "path": path, "encoder": diagnostics,
            "accepted_video_samples": context.accepted_video_samples,
            "encode_ms": encode_ms, "finalize_error": null}));
        report["encoded"] = true.into();
        let metadata = match fs::metadata(&path) {
            Ok(metadata)
                if metadata.is_file() && metadata.len() != 0 && metadata.len() <= FILE_LIMIT =>
            {
                metadata
            }
            Ok(_) => {
                return Ok(failed_case(
                    report,
                    attempts,
                    json!("output_validation"),
                    json!("encoded output is empty or exceeds the byte limit"),
                ))
            }
            Err(error) => {
                return Ok(failed_case(
                    report,
                    attempts,
                    json!("output_validation"),
                    json!(error.to_string()),
                ))
            }
        };
        report["bytes"] = json!(metadata.len());
        let evaluation_start = Instant::now();
        match evaluate(&path, stop, case_start, run_start, expected) {
            Ok(metrics) => report["metrics"] = metrics,
            Err(error) => {
                report["decode_eval_ms"] = json!(evaluation_start.elapsed().as_secs_f64() * 1000.);
                return Ok(failed_case(
                    report,
                    attempts,
                    json!(FailureStage::Reopen),
                    json!(error.to_string()),
                ));
            }
        }
        report["decode_eval_ms"] = json!(evaluation_start.elapsed().as_secs_f64() * 1000.);
        if let Some(cause) = interruption(stop, case_start, run_start) {
            return Ok(failed_case(
                report,
                attempts,
                json!("payload_scan"),
                json!(cause),
            ));
        }
        let payload_start = Instant::now();
        let payload = crate::recording::bitrate_check::inspect_cancellable(&path, &|| {
            interruption(stop, case_start, run_start).is_some()
        })
        .unwrap_or_else(|error| json!({"complete": false, "error": error.to_string()}));
        report["payload_scan_ms"] = json!(payload_start.elapsed().as_secs_f64() * 1000.);
        report["video_payload"] = payload;
        if let Some(cause) = interruption(stop, case_start, run_start) {
            return Ok(failed_case(
                report,
                attempts,
                json!("comparison_budget"),
                json!(cause),
            ));
        }
        if let Err(error) = validate_payload(&report["video_payload"]) {
            return Ok(failed_case(
                report,
                attempts,
                json!("payload_scan"),
                json!(error.to_string()),
            ));
        }
        report["attempts"] = attempts.into();
        report["compared"] = true.into();
        return Ok(report);
    }
}

/// 只创建本次独占子目录；输出是内部证据，不发布成片或修改默认预设。
pub(crate) fn run(directory: &Path, stop: &AtomicBool) -> Result<Value> {
    let expected = source_length()?;
    let _ = segment_catalog()?;
    let output = output_directory(directory)?;
    let _runtime = MfRuntime::new()?;
    let started = Instant::now();
    let mut cases = Vec::with_capacity(CASE_COUNT);
    for (index, profile) in BASELINES
        .into_iter()
        .chain(QualityProfile::EXPERIMENTS)
        .enumerate()
    {
        cases.push(run_case(&output, index, profile, stop, started, expected)
            .unwrap_or_else(|error| json!({"profile": profile,
                "requested_plan": quality_plan(samples::width(), samples::height(), samples::fps(), profile).ok(),
                "compared": false, "failure_stage": "case_setup", "failure": error.to_string(), "attempts": []})));
    }
    let baseline_ok = cases.first().is_some_and(|case| case["compared"] == true);
    let all_baselines_compared = cases
        .get(..BASELINES.len())
        .is_some_and(|baselines| baselines.iter().all(|case| case["compared"] == true));
    let all_profiles_compared =
        cases.len() == CASE_COUNT && cases.iter().all(|case| case["compared"] == true);
    let report = json!({
        "baseline_ok": baseline_ok, "all_profiles_compared": all_profiles_compared,
        "baseline_profiles": BASELINES.map(QualityProfile::id),
        "all_baselines_compared": all_baselines_compared,
        "cancelled": stop.load(Ordering::Acquire), "output": output,
        "source": {"width": samples::width(), "height": samples::height(), "fps": samples::fps(),
            "frames": samples::frame_count(), "description": "synthetic ASCII text and simple geometry",
            "real_desktop": false, "hdr": false, "alpha": 255},
        "audio": false, "audio_output": false, "default_changed": false, "recommendation": null,
        "psnr_method": "sum squared error and channel/pixel counts over each entire segment; alpha excluded; zero MSE uses exact_match and null PSNR",
        "metric_scope": "source BGRA versus Media Foundation decoded BGRA; includes color conversion and chroma subsampling",
        "encode_ms_includes_setup_and_source_generation": true,
        "timing_comparison_scope": "one run per profile in fixed order; warm-up and cache effects are not isolated",
        "timing_budget_scope": "cooperative checks around encoder/decoder work and before each compressed-sample read; blocking native calls cannot be interrupted",
        "file_limit_scope": "checked between writes and before scanning; a native write or Finalize may exceed the limit before returning",
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.,
        "limits": {"profiles": CASE_COUNT, "source_frames": FRAME_LIMIT, "pixels_per_frame": PIXEL_LIMIT,
            "bytes_per_file": FILE_LIMIT, "report_bytes": REPORT_LIMIT,
            "case_budget_seconds": CASE_BUDGET.as_secs(), "run_budget_seconds": RUN_BUDGET.as_secs()},
        "cases": cases,
    });
    let bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > REPORT_LIMIT {
        return Err(tr!(
            "画质比较报告超过大小上限",
            "Quality comparison report exceeds its size limit"
        )
        .into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_fixture() -> Value {
        let first = (frame_time(0).as_nanos() / 100) as i64;
        let last = (frame_time(samples::frame_count() - 1).as_nanos() / 100) as i64;
        let end = (frame_time(samples::frame_count()).as_nanos() / 100) as i64;
        json!({"complete":true,"samples":samples::frame_count(),"payload_bytes":375000,
            "actual_payload_bitrate_bps":1000000.,"minimum_pts_100ns":first,
            "maximum_pts_100ns":last,"maximum_end_100ns":end,
            "observed_span_100ns":end - first})
    }

    #[test]
    fn recording_quality_compare_payload_accepts_quantized_tail_and_tolerance_boundary() {
        let payload = payload_fixture();
        assert!(validate_payload(&payload).is_ok());
        let end = payload["maximum_end_100ns"].as_i64().unwrap();
        let tolerance_ticks = i64::try_from(PTS_TOLERANCE_NS / 100).unwrap();
        for delta in [-1, -tolerance_ticks, tolerance_ticks] {
            let mut adjusted = payload.clone();
            adjusted["maximum_end_100ns"] = json!(end + delta);
            adjusted["observed_span_100ns"] = json!(end + delta);
            assert!(validate_payload(&adjusted).is_ok(), "tail delta={delta}");
        }
        // 容差同时约束总跨度；首端略偏后但末端不变仍可在 1ms 边界内。
        let mut shifted_start = payload;
        shifted_start["minimum_pts_100ns"] = json!(tolerance_ticks);
        shifted_start["observed_span_100ns"] = json!(end - tolerance_ticks);
        assert!(validate_payload(&shifted_start).is_ok());
    }

    #[test]
    fn recording_quality_compare_payload_rejects_wrong_tail_pts_and_span() {
        let payload = payload_fixture();
        let end = payload["maximum_end_100ns"].as_i64().unwrap();
        for delta in [-10_001, 10_001, -10_000_000, 10_000_000] {
            let mut wrong_tail = payload.clone();
            wrong_tail["maximum_end_100ns"] = json!(end + delta);
            wrong_tail["observed_span_100ns"] = json!(end + delta);
            let error = validate_payload(&wrong_tail).unwrap_err().to_string();
            assert!(error.contains("time span does not match"), "{error}");
        }
        let last = payload["maximum_pts_100ns"].as_i64().unwrap();
        let mut offset = payload.clone();
        offset["minimum_pts_100ns"] = json!(20_000);
        offset["maximum_pts_100ns"] = json!(last + 20_000);
        offset["maximum_end_100ns"] = json!(end + 20_000);
        assert!(validate_payload(&offset).is_err(), "整段偏移2ms不能通过");
        let mut reversed = payload.clone();
        reversed["minimum_pts_100ns"] = json!(last);
        reversed["maximum_pts_100ns"] = json!(0);
        reversed["observed_span_100ns"] = json!(end - last);
        assert!(validate_payload(&reversed).is_err());
        let mut contradiction = payload;
        contradiction["observed_span_100ns"] = json!(end + 1);
        assert!(
            validate_payload(&contradiction).is_err(),
            "内部差值矛盾不使用1ms容差放行"
        );
    }

    #[test]
    fn recording_quality_compare_payload_requires_typed_complete_native_statistics() {
        let payload = payload_fixture();
        for key in [
            "minimum_pts_100ns",
            "maximum_pts_100ns",
            "maximum_end_100ns",
            "observed_span_100ns",
        ] {
            let mut missing = payload.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(validate_payload(&missing).is_err(), "missing {key}");
            for invalid in [
                Value::Null,
                json!("0"),
                json!(true),
                json!(0.0),
                json!(u64::MAX),
            ] {
                let mut wrong_type = payload.clone();
                wrong_type[key] = invalid;
                assert!(validate_payload(&wrong_type).is_err(), "invalid {key}");
            }
        }
        for (key, value) in [
            ("complete", json!(false)),
            ("samples", json!(89)),
            ("payload_bytes", json!(0)),
            ("actual_payload_bitrate_bps", json!(0)),
            ("actual_payload_bitrate_bps", Value::Null),
        ] {
            let mut incomplete = payload.clone();
            incomplete[key] = value;
            assert!(validate_payload(&incomplete).is_err(), "incomplete {key}");
        }
    }

    #[test]
    fn recording_quality_compare_matches_real_pts_with_tolerance_and_rejects_duplicates() {
        let mut seen = [false; FRAME_LIMIT as usize];
        let expected = frame_time(37);
        assert_eq!(
            match_frame(expected + Duration::from_millis(1), &mut seen).unwrap(),
            37
        );
        assert!(match_frame(expected, &mut seen).is_err());
        assert!(match_frame(
            frame_time(38) + Duration::from_nanos(PTS_TOLERANCE_NS as u64 + 1),
            &mut seen
        )
        .is_err());
        assert!(!seen[38]);
        assert!(match_frame(frame_time(FRAME_LIMIT), &mut seen).is_err());
        assert!(match_frame(Duration::MAX, &mut seen).is_err());
    }

    #[test]
    fn recording_quality_compare_accumulates_squared_error_before_psnr_and_ignores_alpha() {
        let mut errors = Errors::default();
        errors.add(&[0, 0, 0, 255], &[1, 0, 0, 0]).unwrap();
        errors.add(&[0, 0, 0, 255], &[10, 0, 0, 0]).unwrap();
        assert_eq!(errors.bgr_squared, 101);
        assert_eq!(errors.channels, 6);
        assert_eq!(errors.pixels, 2);
        let report = errors.report();
        assert!((report["bgr"]["mse"].as_f64().unwrap() - 101. / 6.).abs() < 1e-12);
        assert!(
            (report["bgr"]["psnr_db"].as_f64().unwrap() - psnr(101. / 6.).unwrap()).abs() < 1e-12
        );
        assert!((errors.luma_squared - 101. * 0.0722_f64.powi(2)).abs() < 1e-12);
        let mut exact = Errors::default();
        exact.add(&[0, 0, 0, 255], &[0, 0, 0, 0]).unwrap();
        assert_eq!(exact.report()["bgr"]["exact_match"], true);
        assert!(exact.report()["bgr"]["psnr_db"].is_null());
        assert!(exact.add(&[], &[]).is_err());
    }

    #[test]
    #[ignore = "原生合成文字、滚动与运动候选比较；不打开窗口或播放声音"]
    fn recording_quality_compare_native_profiles() {
        let stop = AtomicBool::new(false);
        let report = run(Path::new("target/recording-quality-compare"), &stop).unwrap();
        assert_eq!(
            report["baseline_ok"], true,
            "baseline failed; output: {}",
            report["output"]
        );
        let cases = report["cases"].as_array().unwrap();
        assert_eq!(cases.len(), CASE_COUNT);
        assert_eq!(
            report["baseline_profiles"],
            json!(BASELINES.map(QualityProfile::id))
        );
        for (index, profile) in BASELINES.into_iter().enumerate() {
            assert_eq!(cases[index]["profile"], json!(profile));
        }
        assert_eq!(
            report["all_baselines_compared"], true,
            "one of the current presets failed; output: {}",
            report["output"]
        );
        for case in cases {
            assert!(!case["requested_plan"].is_null());
            if case["compared"] == true {
                assert!(!case["actual"].is_null());
                assert_eq!(case["metrics"]["frames"], FRAME_LIMIT);
                let segments = case["metrics"]["segments"].as_array().unwrap();
                assert_eq!(segments.len(), 3);
                for segment in segments {
                    assert_eq!(segment["source_frames"], segment["decoded_frames"]);
                    assert!(segment["decoded_frames"]
                        .as_u64()
                        .is_some_and(|frames| frames > 0));
                    assert!(segment["metrics"]["bgr"]["mse"]
                        .as_f64()
                        .is_some_and(|mse| mse.is_finite()));
                    assert!(segment["metrics"]["luma"]["mse"]
                        .as_f64()
                        .is_some_and(|mse| mse.is_finite()));
                }
                assert_eq!(case["video_payload"]["complete"], true);
            } else {
                assert!(!case["failure_stage"].is_null());
                assert!(!case["failure"].is_null());
            }
        }
        assert_eq!(
            report["all_profiles_compared"],
            cases.iter().all(|case| case["compared"] == true)
        );
    }
}
