//! 有界的合成素材先存后码实验；不接正式录屏，不采集桌面或设备。
//! 比较包含原始中间文件的实际落盘成本；取消和失败均保留独占目录中的产物。
use super::spool::{SpoolReader, SpoolSpec, SpoolWriter};
use crate::{
    media_foundation::{EncoderTuning, MfRuntime, Mp4Writer, VideoReader},
    recording_encoder_trial::measurements::TrialMeasurement,
    recording_options::{quality_plan, Preset, QualityProfile},
    recording_quality_lab::profile_samples as samples,
    Result,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::HSTRING,
    Win32::{
        Media::MediaFoundation::{MFMediaType_Audio, MFVideoFormat_H264},
        Storage::FileSystem::GetDiskFreeSpaceExW,
    },
};

const SPOOL_LIMIT: u64 = 256 * 1024 * 1024;
const VIDEO_LIMIT: u64 = 32 * 1024 * 1024;
const REPORT_LIMIT: usize = 256 * 1024;
const DISK_HEADROOM: u64 = 16 * 1024 * 1024;
const WORK_BUDGET: Duration = Duration::from_secs(180);
const PTS_TOLERANCE: Duration = Duration::from_millis(1);

fn checkpoint(stop: &AtomicBool, started: Instant) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err("Deferred-encoding trial was cancelled".into());
    }
    if started.elapsed() >= WORK_BUDGET {
        return Err("Deferred-encoding trial exceeded its cooperative work budget".into());
    }
    Ok(())
}

fn directory(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let path = parent.join(format!(
            "deferred-trial-{}-{stamp}-{index}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("Cannot allocate an exclusive deferred-encoding trial directory".into())
}

fn short(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(2048).collect()
}

fn timing(index: u32) -> Result<(Duration, Duration)> {
    let fps = u64::from(samples::fps());
    if fps == 0 || index >= samples::frame_count() {
        return Err("Invalid deferred-encoding sample index or FPS".into());
    }
    let ticks = u64::from(index) * 10_000_000 / fps;
    let next = (u64::from(index) + 1) * 10_000_000 / fps;
    Ok((
        Duration::from_nanos(ticks * 100),
        Duration::from_nanos((next - ticks) * 100),
    ))
}

fn update_digest(
    hash: &mut Sha256,
    index: u32,
    pixels: &[u8],
    time: Duration,
    duration: Duration,
) -> Result<()> {
    hash.update(index.to_le_bytes());
    hash.update(u64::try_from(time.as_nanos())?.to_le_bytes());
    hash.update(u64::try_from(duration.as_nanos())?.to_le_bytes());
    hash.update(u64::try_from(pixels.len())?.to_le_bytes());
    hash.update(pixels);
    Ok(())
}

fn digest_text(hash: Sha256) -> String {
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn disk_plan(path: &Path, spec: SpoolSpec) -> Result<Value> {
    let required = spec
        .expected_max_bytes
        .checked_add(VIDEO_LIMIT * 2)
        .and_then(|bytes| bytes.checked_add(REPORT_LIMIT as u64))
        .and_then(|bytes| bytes.checked_add(DISK_HEADROOM))
        .ok_or("Deferred-encoding disk budget overflowed")?;
    let mut available = 0_u64;
    let native_path = HSTRING::from(path.as_os_str());
    unsafe { GetDiskFreeSpaceExW(&native_path, Some(&mut available), None, None) }?;
    let raw_per_second = u64::from(spec.width)
        .checked_mul(u64::from(spec.height))
        .and_then(|bytes| bytes.checked_mul(4))
        .and_then(|bytes| bytes.checked_mul(u64::from(spec.fps)))
        .ok_or("Deferred-encoding raw data rate overflowed")?;
    Ok(
        json!({"required_available_bytes": required, "available_to_caller_bytes": available,
        "enough_at_preflight": available >= required, "spool_max_bytes": spec.expected_max_bytes,
        "two_mp4_limits_bytes": VIDEO_LIMIT * 2, "report_limit_bytes": REPORT_LIMIT,
        "headroom_bytes": DISK_HEADROOM, "raw_payload_bytes_per_second": raw_per_second,
        "raw_payload_bytes_per_minute": raw_per_second.checked_mul(60),
        "raw_1920x1080_30fps_bytes_per_minute": 1920_u64 * 1080 * 4 * 30 * 60,
        "scope": "quota-aware caller-available bytes sampled once on the output volume; this is not a reservation, and later writes can still fail; projections exclude record headers, filesystem allocation and audio"}),
    )
}

fn file_bytes(path: &Path, limit: u64) -> Result<u64> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("Deferred-encoding artifact exceeds its regular-file size limit".into());
    }
    Ok(metadata.len())
}

fn writer_snapshot(value: &Value) -> Value {
    json!({"input": value.get("input"),
        "actual_encoder": value.pointer("/encoding_trial/inspection/video/encoder"),
        "actual_encoder_adapter": value.pointer("/encoding_trial/inspection/video/encoder_adapter"),
        "observed_video_codec_matches_request": value.get("observed_video_codec_matches_request"),
        "encoding_trial": value.get("encoding_trial"),
        "scope": "selected finalized-writer fields; actual encoder identity comes only from the unique-transform inspection, while input is the pixel route; unknown identity remains unknown"})
}

fn encode(
    path: &Path,
    stop: &AtomicBool,
    started: Instant,
    mut source: impl FnMut(u32) -> Result<(Vec<u8>, Duration, Duration)>,
) -> Value {
    let measurement = TrialMeasurement::start();
    let mut stage = "writer_creation";
    let mut writer = None;
    let mut frames = 0_u32;
    let mut hash = Sha256::new();
    let mut writer_create_ms = None;
    let mut source_read_and_hash_ms = 0.;
    let mut native_write_ms = 0.;
    let work: Result<()> = (|| {
        checkpoint(stop, started)?;
        let plan = quality_plan(
            samples::width(),
            samples::height(),
            samples::fps(),
            QualityProfile::Current(Preset::CompactMp4),
        )?;
        // 两条路线都显式请求相同 GPU NV12 输入；不可用就报告失败，不静默换路线。
        let created = Instant::now();
        let created_writer = Mp4Writer::new_tuned_for_lab(
            path,
            samples::width(),
            samples::height(),
            samples::fps(),
            plan.bitrate_bps,
            EncoderTuning::from_quality_plan(plan),
        );
        writer_create_ms = Some(created.elapsed().as_secs_f64() * 1000.);
        writer = Some(created_writer?);
        let writer = writer
            .as_mut()
            .ok_or("Deferred-encoding writer is missing")?;
        for index in 0..samples::frame_count() {
            checkpoint(stop, started)?;
            stage = "source_read";
            let read_started = Instant::now();
            let (pixels, time, duration) = source(index)?;
            let expected = u64::from(samples::width()) * u64::from(samples::height()) * 4;
            if u64::try_from(pixels.len())? != expected || timing(index)? != (time, duration) {
                return Err(
                    "Deferred-encoding source geometry or timestamps differ from the baseline"
                        .into(),
                );
            }
            update_digest(&mut hash, index, &pixels, time, duration)?;
            source_read_and_hash_ms += read_started.elapsed().as_secs_f64() * 1000.;
            stage = "write_video";
            let write_started = Instant::now();
            let submitted = writer.write_video(&pixels, samples::width(), time, duration);
            native_write_ms += write_started.elapsed().as_secs_f64() * 1000.;
            submitted?;
            frames += 1;
            checkpoint(stop, started)?;
            file_bytes(path, VIDEO_LIMIT)?;
        }
        Ok(())
    })();
    let primary = work
        .err()
        .map(|error| json!({"stage": stage, "message": short(error)}));
    let mut secondary = Vec::new();
    let mut finalized = None;
    let mut observation = None;
    let mut finalize_calls = 0;
    let mut finalize_ms = None;
    if let Some(writer) = writer.take() {
        finalize_calls = 1;
        let finalize_started = Instant::now();
        let (result, diagnostics) = writer.finish_with_diagnostics();
        finalize_ms = Some(finalize_started.elapsed().as_secs_f64() * 1000.);
        finalized = Some(result.is_ok());
        observation = Some(writer_snapshot(&diagnostics));
        if let Err(error) = result {
            secondary.push(json!({"stage": "finalize", "message": short(error)}));
        }
    }
    let bytes = match file_bytes(path, VIDEO_LIMIT) {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            secondary.push(json!({"stage": "output_size", "message": short(error)}));
            None
        }
    };
    if let Err(error) = checkpoint(stop, started) {
        secondary.push(json!({"stage": "after_finalize", "message": short(error)}));
    }
    json!({"ok": primary.is_none() && secondary.is_empty() && finalized == Some(true)
            && frames == samples::frame_count(),
        "frames_written": frames, "source_sha256": digest_text(hash), "bytes": bytes,
        "primary_failure": primary, "secondary_failures": secondary,
        "explicit_finalize_calls": finalize_calls, "finalize_succeeded": finalized,
        "writer": observation, "measurement": measurement.finish(),
        "wall_parts_ms": {"writer_creation": writer_create_ms,
            "completed_source_reads_and_hashes": source_read_and_hash_ms,
            "native_video_submissions": native_write_ms, "finalize": finalize_ms},
        "wall_parts_scope": "sequential wall brackets, including failed native create/write/Finalize calls; an incomplete source-read/hash call is not included in its completed-call sum; housekeeping and endpoint sampling remain in the total measurement"})
}

fn decode_case(path: &Path, stop: &AtomicBool, started: Instant) -> Value {
    let measurement = TrialMeasurement::start();
    match decode(path, stop, started) {
        Ok(result) => json!({"ok": true, "result": result, "measurement": measurement.finish()}),
        Err(error) => {
            json!({"ok": false, "failure": short(error), "measurement": measurement.finish()})
        }
    }
}

fn decode(path: &Path, stop: &AtomicBool, started: Instant) -> Result<Value> {
    checkpoint(stop, started)?;
    file_bytes(path, VIDEO_LIMIT)?;
    let mut reader = VideoReader::open(path)?;
    if reader.info.codec != MFVideoFormat_H264
        || reader.info.stream_probe_error.is_some()
        || reader
            .info
            .streams
            .iter()
            .any(|stream| stream.major_type == MFMediaType_Audio)
    {
        return Err("Deferred-encoding output codec or no-audio contract is unconfirmed".into());
    }
    let mut seen = vec![false; usize::try_from(samples::frame_count())?];
    let mut count = 0_u32;
    let mut squared = 0_u64;
    let mut components = 0_u64;
    let mut worst_pts = Duration::ZERO;
    loop {
        checkpoint(stop, started)?;
        let frame = reader.next_cancellable(stop)?;
        checkpoint(stop, started)?;
        let Some(frame) = frame else {
            break;
        };
        if count >= samples::frame_count()
            || reader.width != samples::width()
            || reader.height != samples::height()
        {
            return Err("Deferred-encoding decoded frame count or geometry is invalid".into());
        }
        let (index, difference) = (0..samples::frame_count())
            .map(|index| timing(index).map(|(time, _)| (index, frame.time.abs_diff(time))))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .min_by_key(|(_, difference)| *difference)
            .ok_or("Deferred-encoding source timestamps are missing")?;
        if difference > PTS_TOLERANCE || seen[index as usize] {
            return Err("Deferred-encoding decoded a duplicate or unmatched PTS".into());
        }
        let source = samples::sample(index)?;
        if source.len() != frame.bgra.len() {
            return Err("Deferred-encoding decoded pixel length differs from the source".into());
        }
        for (a, b) in source.chunks_exact(4).zip(frame.bgra.chunks_exact(4)) {
            for component in 0..3 {
                let difference = i32::from(a[component]) - i32::from(b[component]);
                squared = squared
                    .checked_add((difference * difference) as u64)
                    .ok_or("Pixel error sum overflowed")?;
                components = components
                    .checked_add(1)
                    .ok_or("Pixel component count overflowed")?;
            }
        }
        seen[index as usize] = true;
        worst_pts = worst_pts.max(difference);
        count += 1;
    }
    if count != samples::frame_count() || seen.iter().any(|seen| !seen) || components == 0 {
        return Err("Deferred-encoding EOF is missing source frames or pixels".into());
    }
    let mse = squared as f64 / components as f64;
    Ok(
        json!({"complete_eof": true, "frames": count, "codec": "h264", "audio_stream_absent": true,
        "bgr_mse": mse, "bgr_psnr_db": (mse > 0.).then(|| 10. * (255. * 255. / mse).log10()),
        "lossless_bgr": mse == 0., "maximum_pts_error_ns": u64::try_from(worst_pts.as_nanos())?,
        "scope": "synthetic ASCII/scroll/motion BGR error over all decoded frames; alpha excluded; no playback, perceptual-quality, HDR or audio-sync proof"}),
    )
}

/// 内部测试构建入口；失败也写出报告并保留中间源，不注册正式录屏选项。
pub(crate) fn run(parent: &Path, stop: &AtomicBool) -> Result<Value> {
    let started = Instant::now();
    let output = directory(parent)?;
    let source_path = output.join("source.bgra-spool");
    let direct_path = output.join("direct.mp4");
    let deferred_path = output.join("deferred.mp4");
    let spec = SpoolSpec::new(
        samples::width(),
        samples::height(),
        samples::fps(),
        samples::frame_count(),
        SPOOL_LIMIT,
    )?;
    let mut stage = "disk_preflight";
    let mut budget = None;
    let mut spool = None;
    let mut direct = None;
    let mut deferred = None;
    let mut direct_decode = None;
    let mut deferred_decode = None;
    let mut validated_source = None;
    let mut generation_digest = None;
    let mut source_measurement = None;
    let mut source_generation_and_append_ms = None;
    let mut source_seal_ms = None;
    let mut runtime = None;
    let work: Result<()> = (|| {
        checkpoint(stop, started)?;
        let plan = disk_plan(&output, spec)?;
        let enough = plan["enough_at_preflight"] == true;
        budget = Some(plan);
        if !enough {
            return Err(
                "Insufficient caller-available space for the bounded deferred-encoding trial"
                    .into(),
            );
        }
        stage = "create_spool";
        let measurement = TrialMeasurement::start();
        let mut writer = SpoolWriter::create(&source_path, spec)?;
        let mut hash = Sha256::new();
        let generation_started = Instant::now();
        stage = "write_spool";
        let capture: Result<()> = (|| {
            for index in 0..samples::frame_count() {
                checkpoint(stop, started)?;
                let pixels = samples::sample(index)?;
                let (time, duration) = timing(index)?;
                writer.push(&pixels, time, duration)?;
                update_digest(&mut hash, index, &pixels, time, duration)?;
            }
            Ok(())
        })();
        source_generation_and_append_ms = Some(generation_started.elapsed().as_secs_f64() * 1000.);
        if let Err(error) = capture {
            // 不伪造 footer 或删残件；显式释放文件后保留已成功写出的完整记录。
            drop(writer);
            source_measurement = Some(measurement.finish());
            return Err(error);
        }
        stage = "seal_spool";
        let seal_started = Instant::now();
        let sealed = writer.seal();
        source_seal_ms = Some(seal_started.elapsed().as_secs_f64() * 1000.);
        source_measurement = Some(measurement.finish());
        spool = Some(serde_json::to_value(sealed?)?);
        generation_digest = Some(digest_text(hash));
        checkpoint(stop, started)?;
        stage = "mf_initialize";
        runtime = Some(MfRuntime::new()?);
        stage = "direct_encode";
        direct = Some(encode(&direct_path, stop, started, |index| {
            let (time, duration) = timing(index)?;
            Ok((samples::sample(index)?, time, duration))
        }));
        checkpoint(stop, started)?;
        stage = "deferred_encode";
        let mut reader = SpoolReader::open(&source_path, SPOOL_LIMIT)?;
        deferred = Some(encode(&deferred_path, stop, started, |index| {
            let frame = reader
                .next()?
                .ok_or("Sealed spool ended before the expected frame")?;
            if frame.index != index {
                return Err("Spool frame index differs from the baseline".into());
            }
            Ok((frame.pixels, frame.pts, frame.duration))
        }));
        if deferred.as_ref().is_some_and(|case| case["ok"] == true) {
            stage = "validate_spool_eof";
            let end = reader.next();
            validated_source = Some(serde_json::to_value(reader.summary())?);
            if end?.is_some() {
                return Err("Sealed spool has unexpected additional frames".into());
            }
        }
        validated_source = Some(serde_json::to_value(reader.summary())?);
        drop(reader);
        checkpoint(stop, started)?;
        if direct.as_ref().is_some_and(|case| case["ok"] == true) {
            stage = "direct_decode";
            direct_decode = Some(decode_case(&direct_path, stop, started));
        }
        checkpoint(stop, started)?;
        if deferred.as_ref().is_some_and(|case| case["ok"] == true) {
            stage = "deferred_decode";
            deferred_decode = Some(decode_case(&deferred_path, stop, started));
        }
        checkpoint(stop, started)?;
        Ok(())
    })();
    let failure = work
        .err()
        .map(|error| json!({"stage": stage, "message": short(error)}));
    let digest_matches = generation_digest.as_ref().is_some_and(|digest| {
        direct
            .as_ref()
            .and_then(|case| case["source_sha256"].as_str())
            == Some(digest.as_str())
            && deferred
                .as_ref()
                .and_then(|case| case["source_sha256"].as_str())
                == Some(digest.as_str())
    });
    // 完成决策与报告共用这一停止快照；晚到的停止请求不再翻转已决定的结果。
    let cancelled = stop.load(Ordering::Acquire);
    let complete = failure.is_none()
        && !cancelled
        && digest_matches
        && validated_source
            .as_ref()
            .is_some_and(|value| value["sealed_complete"] == true)
        && direct_decode
            .as_ref()
            .is_some_and(|value| value["ok"] == true)
        && deferred_decode
            .as_ref()
            .is_some_and(|value| value["ok"] == true);
    let mut report = json!({"schema_version": 1, "probe": "bounded_bgra_deferred_encoding", "ok": complete,
        "output": output, "failure": failure, "cancelled": cancelled,
        "cancellation_scope": "stop snapshot at the final completion decision; later stop requests do not change that decision",
        "budget": budget, "spec": spec, "source_write": spool, "source_read": validated_source,
        "source_sha256": generation_digest, "all_source_digests_match": digest_matches,
        "source_write_measurement": source_measurement,
        "source_wall_parts_ms": {"generation_and_append": source_generation_and_append_ms,
            "seal_footer_flush_sync": source_seal_ms},
        "direct_encode": direct, "deferred_encode": deferred,
        "direct_decode": direct_decode, "deferred_decode": deferred_decode,
        "elapsed_ms": started.elapsed().as_secs_f64() * 1000.,
        "timing_scope": "sequential synthetic trials: source-write includes generation, hashing, raw disk writes and explicit flush/sync; direct-encode includes regeneration, hashing, writer creation/write/Finalize; deferred-encode includes spool reads, SHA verification, hashing and writer creation/write/Finalize; decode and report costs excluded from those encode stages; disk cache, shared process and driver variability remain",
        "retention_scope": "all owned source, MP4 and report artifacts retained after success, cancellation and failure; an incomplete spool may expose a SHA-verified prefix but is not sealed or automatically encoded/repaired; no user files are removed",
        "limit_scope": "spool allocation is bounded by its validated specification; MP4 size limits are observed between public native calls and after Finalize, not filesystem quotas, so a single blocking native call may exceed them; available disk bytes are sampled, not reserved",
        "scope": "90-frame synthetic ASCII/scroll/motion BGRA experiment, same requested GPU NV12 H.264 parameters; raw intermediate format is internal, uncompressed and video-only; no live capture, audio, production route, disk reservation or measured benefit assertion"});
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > REPORT_LIMIT {
        report = json!({"schema_version": 1, "probe": "bounded_bgra_deferred_encoding", "ok": false,
            "failure": {"stage": "report_limit", "message": "Detailed report exceeded 256 KiB"},
            "original_failure": report.get("failure"), "scope": "owned artifacts retained; no success assertion"});
        bytes = serde_json::to_vec_pretty(&report)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(runtime);
    Ok(report)
}

#[test]
fn recording_deferred_timestamps_are_contiguous_100ns_and_total_three_seconds() {
    let mut end = Duration::ZERO;
    for index in 0..samples::frame_count() {
        let (time, duration) = timing(index).unwrap();
        assert_eq!(time, end);
        assert_eq!(time.as_nanos() % 100, 0);
        assert_eq!(duration.as_nanos() % 100, 0);
        assert!(!duration.is_zero());
        end = time + duration;
    }
    assert_eq!(end, Duration::from_secs(3));
    assert!(timing(samples::frame_count()).is_err());
}

#[test]
#[ignore = "writes a bounded raw spool and uses native H.264; run only in the coordinated Dev validation slot"]
fn recording_deferred_native_same_source_budgeted_roundtrip() {
    let report = run(
        Path::new("target/recording-deferred-trial"),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report["ok"], true, "{report:#}");
}
