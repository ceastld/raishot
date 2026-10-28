//! 仅供未来显式选择摄像头后的内部端到端调用；不注册原生测试、CLI 或自动设备选择。
//! 不打开预览窗口，不播放声音；产物保留在调用方指定父目录下的新建独占子目录。

use super::{
    frame::composite_camera, CameraCapture, CameraIssue, CameraPhase, CameraRequest, CameraSample,
    CameraStatus,
};
use crate::{
    capture::Image,
    geometry::ScreenRect,
    media_foundation::{MfRuntime, Mp4Writer, VideoReader},
    Result,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::Media::MediaFoundation::{MFMediaType_Audio, MFVideoFormat_H264};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FPS: u32 = 30;
const BITRATE: u32 = 4_000_000;
const FRAME_COUNT: usize = 12;
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 4;
const WORK_BUDGET: Duration = Duration::from_secs(30);
const STOP_WAIT: Duration = Duration::from_secs(1);
const MAX_FRAME_AGE: Duration = Duration::from_millis(500);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const PTS_TOLERANCE: Duration = Duration::from_millis(1);
const OUTPUT_LIMIT: u64 = 32 * 1024 * 1024;
const REPORT_LIMIT: usize = 256 * 1024;
const MAX_ERROR_CHARS: usize = 1024;
const REGION: ScreenRect = ScreenRect {
    left: 0,
    top: 0,
    width: WIDTH,
    height: HEIGHT,
};
const DESTINATION: ScreenRect = ScreenRect {
    left: 384,
    top: 284,
    width: 240,
    height: 180,
};

#[derive(Serialize)]
struct ProbeIssue {
    stage: &'static str,
    message: String,
}

impl ProbeIssue {
    fn new(stage: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            stage,
            message: short(&message.to_string(), MAX_ERROR_CHARS),
        }
    }
}

#[derive(Serialize)]
struct FrameEvidence {
    sequence: u64,
    source_timestamp_100ns: i64,
    received_since_probe_ns: u64,
    age_when_taken_ns: u64,
    source_size: [u32; 2],
    video_pts_ns: u64,
    video_duration_ns: Option<u64>,
    write_accepted: bool,
}

struct PendingFrame {
    sample: CameraSample,
    time: Duration,
    evidence_index: usize,
}

fn short(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn nanoseconds(duration: Duration) -> Result<u64> {
    Ok(u64::try_from(duration.as_nanos())?)
}

fn receipt_timing(
    received: Instant,
    started: Instant,
    now: Instant,
) -> Result<(Duration, Duration)> {
    // Windows 的 checked_duration_since 可把 QPC epsilon 内的逆序投影为零，先按 Instant 排序。
    if received > now {
        return Err("Camera frame receipt time is in the future".into());
    }
    if received < started {
        return Err("Camera receipt time precedes this probe".into());
    }
    let age = now
        .checked_duration_since(received)
        .ok_or("Camera frame receipt time is in the future")?;
    let received_since_probe = received
        .checked_duration_since(started)
        .ok_or("Camera receipt time precedes this probe")?;
    Ok((age, received_since_probe))
}

fn checkpoint(stop: &AtomicBool, started: Instant) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err("Camera probe was cancelled".into());
    }
    if started.elapsed() >= WORK_BUDGET {
        return Err("Camera probe exceeded its cooperative 30-second work budget".into());
    }
    Ok(())
}

fn output_directory(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let path = parent.join(format!(
            "camera-probe-{}-{stamp}-{index}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("Could not allocate an exclusive camera probe directory".into())
}

fn output_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > OUTPUT_LIMIT {
        return Err("Camera probe output is not a regular file within its 32-MiB limit".into());
    }
    Ok(metadata.len())
}

fn issue_value(issue: &CameraIssue) -> Value {
    json!({"stage": issue.stage, "hresult": issue.hresult,
        "message": short(&issue.message, MAX_ERROR_CHARS)})
}

fn status_value(status: &CameraStatus) -> Value {
    let negotiated = status.negotiated.as_ref().map(|format| {
        json!({
            "stream": format.stream, "width": format.width, "height": format.height,
            "fps_numerator": format.fps_numerator, "fps_denominator": format.fps_denominator,
            "requested_native_subtype": short(&format.requested_native_subtype, 128),
            "converted_subtype": format.converted_subtype,
        })
    });
    json!({"phase": status.phase, "negotiated": negotiated,
        "frames_received": status.frames_received, "frames_replaced": status.frames_replaced,
        "format_notifications": status.format_notifications,
        "layout_reconfigurations": status.layout_reconfigurations,
        "source_timestamp_100ns": status.source_timestamp_100ns,
        "failure": status.failure.as_ref().map(issue_value),
        "cleanup_issues": status.cleanup_issues.iter().take(8).map(issue_value).collect::<Vec<_>>(),
        "cleanup_issue_count": status.cleanup_issues.len(),
        "cleanup_issues_truncated": status.cleanup_issues.len() > 8,
        "delivery_closed": status.delivery_closed, "worker_finished": status.worker_finished,
        "scope": short(status.scope, 2048)})
}

fn step_value(value: Option<&Value>) -> Value {
    json!({"state": value.and_then(|item| item.get("state")).and_then(Value::as_str),
        "error": value.and_then(|item| item.get("error")).and_then(Value::as_str)
            .map(|message| short(message, MAX_ERROR_CHARS))})
}

fn writer_value(value: &Value) -> Value {
    json!({"input": value.get("input").and_then(Value::as_str),
        "observed_video_codec_matches_request": value.get("observed_video_codec_matches_request").and_then(Value::as_bool),
        "encoder_kind": value.pointer("/encoding_trial/inspection/video/encoder/kind").and_then(Value::as_str),
        "clsid": value.pointer("/encoding_trial/inspection/video/encoder/clsid").and_then(Value::as_str).map(|text| short(text, 128)),
        "name": value.pointer("/encoding_trial/inspection/video/encoder/name").and_then(Value::as_str).map(|text| short(text, 512)),
        "first_video_sample": step_value(value.pointer("/encoding_trial/first_video_sample")),
        "first_audio_sample": step_value(value.pointer("/encoding_trial/first_audio_sample")),
        "finalize": step_value(value.pointer("/encoding_trial/finalize")),
        "scope": "bounded fields from the finalized writer snapshot; encoder identity comes only from the unique-transform inspection, while input is the pixel route; constructor may choose GPU or CPU input"})
}

fn write_frame(
    writer: &mut Mp4Writer,
    image: &mut Image,
    pending: &PendingFrame,
    duration: Duration,
    evidence: &mut [FrameEvidence],
) -> Result<()> {
    if duration.as_nanos() < 100 {
        return Err("Camera receipt interval is shorter than one 100-ns media tick".into());
    }
    for pixel in image.bgra.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[25, 22, 19, 255]);
    }
    if composite_camera(image, REGION, &pending.sample.frame, DESTINATION)?.is_none() {
        return Err("Camera composition produced no visible picture-in-picture pixels".into());
    }
    evidence[pending.evidence_index].video_duration_ns = Some(nanoseconds(duration)?);
    writer.write_video(&image.bgra, image.width, pending.time, duration)?;
    evidence[pending.evidence_index].write_accepted = true;
    Ok(())
}

fn decode(
    path: &Path,
    evidence: &[FrameEvidence],
    stop: &AtomicBool,
    started: Instant,
) -> Result<Value> {
    checkpoint(stop, started)?;
    if evidence.len() != FRAME_COUNT || evidence.iter().any(|item| !item.write_accepted) {
        return Err("Camera probe has incomplete encoded-frame evidence".into());
    }
    output_bytes(path)?;
    let mut video = VideoReader::open(path)?;
    checkpoint(stop, started)?;
    if (video.width, video.height) != (WIDTH, HEIGHT) || video.info.codec != MFVideoFormat_H264 {
        return Err("Camera probe output is not the requested 640x480 H.264 video".into());
    }
    if video.info.stream_probe_error.is_some()
        || video
            .info
            .streams
            .iter()
            .any(|stream| stream.major_type == MFMediaType_Audio)
    {
        return Err("Camera probe could not confirm that its output has no audio stream".into());
    }
    let mut matched = [false; FRAME_COUNT];
    let mut decoded_times = Vec::with_capacity(FRAME_COUNT);
    let mut max_pts_error = Duration::ZERO;
    loop {
        checkpoint(stop, started)?;
        let next = video.next_cancellable(stop)?;
        // next_cancellable 也可因取消返回 None；必须先检查取消，再把 None 解释成 EOF。
        checkpoint(stop, started)?;
        let Some(frame) = next else {
            break;
        };
        if decoded_times.len() >= FRAME_COUNT {
            return Err("Camera probe decoded more than 12 video frames".into());
        }
        if (video.width, video.height) != (WIDTH, HEIGHT) || frame.bgra.len() != FRAME_BYTES {
            return Err("Camera probe decoded an invalid frame size or BGRA layout".into());
        }
        let nearest = evidence
            .iter()
            .enumerate()
            .map(|(index, item)| {
                (
                    index,
                    frame.time.abs_diff(Duration::from_nanos(item.video_pts_ns)),
                )
            })
            .min_by_key(|(_, distance)| *distance)
            .ok_or("Camera probe has no encoded PTS evidence")?;
        if nearest.1 > PTS_TOLERANCE || matched[nearest.0] {
            return Err("Camera probe decoded a duplicate or unmatched video timestamp".into());
        }
        matched[nearest.0] = true;
        max_pts_error = max_pts_error.max(nearest.1);
        decoded_times.push(nanoseconds(frame.time)?);
    }
    if decoded_times.len() != FRAME_COUNT || !matched.into_iter().all(|value| value) {
        return Err("Camera probe reached EOF without all 12 distinct encoded frames".into());
    }
    Ok(
        json!({"complete_eof": true, "frames": decoded_times.len(), "size": [WIDTH, HEIGHT],
        "codec": format!("{:?}", video.info.codec), "audio_stream_absent": true,
        "video_pts_ns": decoded_times, "max_pts_error_ns": nanoseconds(max_pts_error)?,
        "scope": "complete current-backend video decode and timestamp/size checks; no playback or perceptual-quality assertion"}),
    )
}

/// 仅显式设备请求可调用；运行失败写入 report.json 后以 ok=false 返回。
/// Err 只表示无法准备输出目录或写出有界报告；不会删除源或任何已生成的试验产物。
pub(crate) fn run(request: CameraRequest, parent: &Path, stop: &AtomicBool) -> Result<Value> {
    let started = Instant::now();
    let output = output_directory(parent)?;
    let media_path = output.join("camera-pip.mp4");
    let requested = json!({"device_id_utf16": request.device.as_units(),
        "max_width": request.max_width, "max_height": request.max_height, "max_fps": request.max_fps,
        "first_frame_budget_ms": u64::try_from(request.first_frame_budget.as_millis()).ok(),
        "run_budget_ms": u64::try_from(request.run_budget.as_millis()).ok(),
        "budget_display_scope": "null means the invalid request duration exceeds the report integer range"});
    // runtime 先声明、后释放；writer 和 decoder 的全部 COM 均在当前线程内完成。
    let mut runtime = None;
    let mut writer = None;
    let mut capture = None;
    let mut evidence = Vec::with_capacity(FRAME_COUNT);
    let mut stage = "preflight";
    let mut primary = None;
    let mut secondary = Vec::new();
    let work: Result<()> = (|| {
        checkpoint(stop, started)?;
        // 本文件拟注册为 camera 的 child；复用同一请求校验，避免先创建原生 writer 再拒请求。
        request.validate()?;
        stage = "mf_initialize";
        runtime = Some(MfRuntime::new()?);
        checkpoint(stop, started)?;
        stage = "writer_create";
        writer = Some(Mp4Writer::new(
            &media_path,
            WIDTH,
            HEIGHT,
            FPS,
            BITRATE,
            false,
        )?);
        checkpoint(stop, started)?;
        stage = "camera_start";
        capture = Some(CameraCapture::start(request)?);
        let camera = capture.as_ref().ok_or("Camera capture was not created")?;
        let writer = writer
            .as_mut()
            .ok_or("Camera probe writer was not created")?;
        let mut image = Image {
            width: WIDTH,
            height: HEIGHT,
            bgra: vec![0; FRAME_BYTES],
            readback_ms: 0.0,
        };
        let mut anchor = None;
        let mut last_sequence = None;
        let mut last_received = None;
        let mut pending: Option<PendingFrame> = None;
        while evidence.len() < FRAME_COUNT {
            stage = "camera_frames";
            checkpoint(stop, started)?;
            let status = camera.status();
            if let Some(issue) = &status.failure {
                return Err(format!("Camera capture failed: {issue}").into());
            }
            if status.delivery_closed || status.worker_finished {
                return Err(
                    "Camera delivery closed before 12 distinct frames were received".into(),
                );
            }
            let Some(sample) = camera.take_latest() else {
                std::thread::sleep(
                    POLL_INTERVAL.min(WORK_BUDGET.saturating_sub(started.elapsed())),
                );
                continue;
            };
            let now = Instant::now();
            let (age, received_since_probe) = receipt_timing(sample.received, started, now)?;
            if age > MAX_FRAME_AGE {
                return Err("Camera latest frame was older than 500 ms when taken".into());
            }
            if sample.sequence == 0
                || last_sequence.is_some_and(|previous| sample.sequence <= previous)
            {
                return Err("Camera probe received a duplicate or reversed frame sequence".into());
            }
            if last_received.is_some_and(|previous| sample.received <= previous) {
                return Err("Camera probe received a non-increasing frame receipt time".into());
            }
            let origin = *anchor.get_or_insert(sample.received);
            let time = sample
                .received
                .checked_duration_since(origin)
                .ok_or("Camera receipt time precedes its origin")?;
            let index = evidence.len();
            evidence.push(FrameEvidence {
                sequence: sample.sequence,
                source_timestamp_100ns: sample.source_timestamp_100ns,
                received_since_probe_ns: nanoseconds(received_since_probe)?,
                age_when_taken_ns: nanoseconds(age)?,
                source_size: [sample.frame.width, sample.frame.height],
                video_pts_ns: nanoseconds(time)?,
                video_duration_ns: None,
                write_accepted: false,
            });
            if let Some(previous) = pending.take() {
                stage = "write_video";
                checkpoint(stop, started)?;
                let duration = time
                    .checked_sub(previous.time)
                    .ok_or("Camera video timestamp reversed")?;
                write_frame(writer, &mut image, &previous, duration, &mut evidence)?;
                checkpoint(stop, started)?;
                output_bytes(&media_path)?;
            }
            last_sequence = Some(sample.sequence);
            last_received = Some(sample.received);
            pending = Some(PendingFrame {
                sample,
                time,
                evidence_index: index,
            });
        }
        stage = "write_last_video";
        checkpoint(stop, started)?;
        let pending = pending.ok_or("Camera probe has no final frame")?;
        write_frame(
            writer,
            &mut image,
            &pending,
            Duration::from_nanos(1_000_000_000 / u64::from(FPS)),
            &mut evidence,
        )?;
        checkpoint(stop, started)?;
        output_bytes(&media_path)?;
        Ok(())
    })();
    let work_completed = work.is_ok();
    if let Err(error) = work {
        primary = Some(ProbeIssue::new(stage, error));
    }

    // 取消/预算耗尽也必须尝试清理；这些收尾调用不能因工作预算而直接丢弃 writer。
    let camera_status = capture.take().map(|camera| camera.stop_and_wait(STOP_WAIT));
    let camera_cleanup_complete = camera_status.as_ref().is_some_and(|status| {
        status.worker_finished && status.delivery_closed && status.cleanup_issues.is_empty()
    });
    let camera_clean = camera_status.as_ref().is_some_and(|status| {
        status.worker_finished
            && status.delivery_closed
            && status.phase == CameraPhase::Closed
            && status.failure.is_none()
            && status.cleanup_issues.is_empty()
    });
    if camera_status.is_some() && !camera_cleanup_complete {
        secondary.push(ProbeIssue::new(
            "camera_cleanup",
            "Camera worker did not confirm a clean completed shutdown; inspect camera_status",
        ));
    }
    if work_completed {
        if let Some(status) = &camera_status {
            if let Some(issue) = &status.failure {
                secondary.push(ProbeIssue::new("camera_terminal_failure", issue));
            } else if camera_cleanup_complete && status.phase != CameraPhase::Closed {
                secondary.push(ProbeIssue::new(
                    "camera_terminal_state",
                    "Camera worker did not report Closed",
                ));
            }
        }
    }
    let mut finalize_calls = 0_u32;
    let mut finalize_succeeded = None;
    let mut writer_report = None;
    if let Some(writer) = writer.take() {
        finalize_calls = 1;
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        finalize_succeeded = Some(finished.is_ok());
        writer_report = Some(writer_value(&diagnostics));
        if let Err(error) = finished {
            secondary.push(ProbeIssue::new("finalize", error));
        }
    }
    if let Err(error) = checkpoint(stop, started) {
        secondary.push(ProbeIssue::new("after_cleanup", error));
    }
    let mut decoded = None;
    if work_completed && finalize_succeeded == Some(true) && checkpoint(stop, started).is_ok() {
        match decode(&media_path, &evidence, stop, started) {
            Ok(value) => decoded = Some(value),
            Err(error) => secondary.push(ProbeIssue::new("decode_to_eof", error)),
        }
    }
    if let Err(error) = checkpoint(stop, started) {
        if secondary.iter().all(|issue| issue.stage != "after_cleanup") {
            secondary.push(ProbeIssue::new("after_decode", error));
        }
    }
    let container = match fs::metadata(&media_path) {
        Ok(metadata) => {
            json!({"present": true, "is_file": metadata.is_file(), "bytes": metadata.len(),
            "within_limit": metadata.is_file() && metadata.len() <= OUTPUT_LIMIT,
            "scope": "whole MP4 container size; not actual compressed video payload bitrate"})
        }
        Err(error) => {
            json!({"present": if error.kind() == ErrorKind::NotFound { Some(false) } else { None },
            "error": short(&error.to_string(), MAX_ERROR_CHARS)})
        }
    };
    let written = evidence.iter().filter(|item| item.write_accepted).count();
    if let Err(error) = checkpoint(stop, started) {
        if secondary
            .iter()
            .all(|issue| issue.stage != "after_cleanup" && issue.stage != "after_decode")
        {
            secondary.push(ProbeIssue::new("before_report", error));
        }
    }
    let ok = work_completed
        && primary.is_none()
        && secondary.is_empty()
        && camera_clean
        && finalize_succeeded == Some(true)
        && written == FRAME_COUNT
        && decoded.is_some()
        && container.get("within_limit").and_then(Value::as_bool) == Some(true);
    let mut report = json!({"schema_version": 1, "probe": "explicit_camera_pip_h264_roundtrip", "ok": ok,
        "directory_display": short(&output.display().to_string(), 2048), "media_file": "camera-pip.mp4", "report_file": "report.json",
        "path_display_scope": "lossy display only; file I/O retains the original Path",
        "request": requested, "primary_failure": primary, "secondary_failures": secondary,
        "camera_status": camera_status.as_ref().map(status_value), "camera_cleanup_complete": camera_cleanup_complete,
        "frames_taken": evidence.len(), "frames_written": written, "samples": evidence,
        "output_request": {"codec": "h264", "container": "mp4", "size": [WIDTH, HEIGHT], "nominal_fps": FPS,
            "mean_bitrate_requested_bps": BITRATE, "audio_requested": false, "frames": FRAME_COUNT,
            "pip_destination": [DESTINATION.left, DESTINATION.top, DESTINATION.width, DESTINATION.height]},
        "writer": {"explicit_finalize_calls": finalize_calls, "finalize_succeeded": finalize_succeeded, "observation": writer_report},
        "decode": decoded, "container": container, "elapsed_ms_before_report_write": started.elapsed().as_secs_f64() * 1000.0,
        "limits": {"work_budget_seconds": WORK_BUDGET.as_secs(), "camera_stop_wait_ms": STOP_WAIT.as_millis(),
            "frames": FRAME_COUNT, "frame_max_age_when_taken_ms": MAX_FRAME_AGE.as_millis(), "output_bytes": OUTPUT_LIMIT,
            "report_bytes": REPORT_LIMIT, "max_decoded_frames_before_rejection": FRAME_COUNT + 1},
        "timing_scope": "source PTS remains signed device evidence; receipt Instants are relative to this probe and do not establish camera/desktop/audio synchronization; output PTS is elapsed receipt time from the first taken frame, quantized by MF to 100 ns; each nonfinal duration uses the next taken receipt, and only the final duration uses the requested 30-fps period",
        "budget_scope": "cooperative checks around public encoder/decoder calls and bounded polling; blocking native calls and internal negotiation cannot be interrupted; mandatory camera stop and one explicit writer Finalize are attempted even after cancellation or work-budget expiry, and report persistence follows cleanup",
        "scope": "12 distinct actual camera callback sequences from one latest-frame mailbox, composited on a synthetic dark background; no desktop capture, audio, playback, perceptual-quality, text-quality or long-recording claim; no device enumeration or default-first-device route"});
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > REPORT_LIMIT {
        report = json!({"schema_version": 1, "probe": "explicit_camera_pip_h264_roundtrip", "ok": false,
            "primary_failure": {"stage": "report_limit", "message": "Detailed report exceeded 256 KiB and was omitted"},
            "original_primary_failure": report.get("primary_failure"),
            "original_secondary_failures": report.get("secondary_failures"),
            "frames_written": written, "explicit_finalize_calls": finalize_calls,
            "scope": "bounded failure receipt; the owned camera-pip.mp4 artifact is retained; no success assertion"});
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_camera_probe_receipt_timing_enforces_nanosecond_and_millisecond_boundaries() {
        let started = Instant::now() + Duration::from_secs(1);
        let now = started + Duration::from_millis(10);
        assert_eq!(
            receipt_timing(started, started, now).unwrap(),
            (Duration::from_millis(10), Duration::ZERO)
        );
        assert_eq!(
            receipt_timing(now, started, now).unwrap(),
            (Duration::ZERO, Duration::from_millis(10))
        );
        for offset in [Duration::from_nanos(1), Duration::from_millis(1)] {
            let before_start = started - offset;
            let after_now = now + offset;
            assert!(before_start < started && after_now > now);
            assert_eq!(
                receipt_timing(before_start, started, now)
                    .unwrap_err()
                    .to_string(),
                "Camera receipt time precedes this probe"
            );
            assert_eq!(
                receipt_timing(after_now, started, now)
                    .unwrap_err()
                    .to_string(),
                "Camera frame receipt time is in the future"
            );
            assert!(receipt_timing(started + offset, started, now).is_ok());
            assert!(receipt_timing(now - offset, started, now).is_ok());
        }
    }
}
