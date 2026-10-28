//! 自有纹理的 GPU NV12 look-behind 回归；不捕获桌面，也不启动播放或声音输出。
//! 预算只在公开原生调用之间检查，不能中断阻塞的 MFT；文件上限也不是磁盘配额。
#![cfg(test)]

use super::stop::{video_span, VideoSpan};
use crate::{
    geometry::{Rect, ScreenRect},
    media_foundation::{
        EncoderBackendSession, MfRuntime, Mp4Writer, PreparedVideoSample, VideoReader,
    },
    recording_color_check::ColorChart,
    recording_encoder_caps::VideoInput,
    recording_encoder_policy::{EncoderPlan, EncoderPolicy},
    recording_options::{quality_plan, Preset, QualityProfile},
    recording_size_layout::RecordingSizeLayout,
    Result,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Graphics::{
        Direct3D11::{
            ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
            D3D11_USAGE_DEFAULT,
        },
        Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
    },
    Media::MediaFoundation::{MFMediaType_Audio, MFVideoFormat_H264},
};

const WIDTH: u32 = 192;
const HEIGHT: u32 = 128;
const SOURCE_WIDTH: u32 = WIDTH + 1;
const SOURCE_HEIGHT: u32 = HEIGHT + 1;
const FPS: u32 = 30;
const BUDGET: Duration = Duration::from_secs(90);
const MAX_VIDEO_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 256 * 1024;
const TIME_TOLERANCE: Duration = Duration::from_millis(1);
const COLOR_TOLERANCE: f64 = 12.;
const ACTIVE_STARTS: [u64; 6] = [0, 333_333, 666_667, 1_000_000, 1_333_333, 1_666_667];
const WALL_STARTS: [u64; 6] = [0, 333_333, 666_667, 6_000_000, 6_333_333, 6_666_667];
const PAIR_STARTS: [u64; 2] = [0, 333_333];
const CHANNEL_ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [1, 2, 0],
    [2, 0, 1],
    [0, 2, 1],
    [2, 1, 0],
    [1, 0, 2],
];

#[derive(Clone, Copy)]
enum Order {
    SubmitThenPrepare,
    TwoPreparedSamples,
}
impl Order {
    fn name(self) -> &'static str {
        match self {
            Self::SubmitThenPrepare => "upload_submit_previous_prepare_next",
            Self::TwoPreparedSamples => "two_frame_prepare_next_before_submit_previous",
        }
    }

    fn starts(self) -> &'static [u64] {
        match self {
            Self::SubmitThenPrepare => &ACTIVE_STARTS,
            Self::TwoPreparedSamples => &PAIR_STARTS,
        }
    }

    fn end(self) -> u64 {
        match self {
            Self::SubmitThenPrepare => 1_750_000,
            Self::TwoPreparedSamples => 416_667,
        }
    }
}

#[test]
#[ignore = "真实 D3D/MF GPU NV12 写入和完整回读；仅显式运行，不启动窗口"]
fn recording_video_lookbehind_native_gpu_samples_and_short_tail() -> Result<()> {
    let directory = unique_directory()?;
    let started = Instant::now();
    let mut cases = Vec::with_capacity(2);
    let setup = (|| -> Result<()> {
        let _runtime = MfRuntime::new()?;
        checkpoint(started)?;
        let device = crate::recording_gpu::create_video_device()?;
        let layout = RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width: SOURCE_WIDTH,
            height: SOURCE_HEIGHT,
        })?;
        let chart = ColorChart::new(layout)?;
        for order in [Order::SubmitThenPrepare, Order::TwoPreparedSamples] {
            cases.push(run_case(&directory, order, &device, &chart, started));
        }
        Ok(())
    })();
    let ok = setup.is_ok()
        && cases.len() == 2
        && cases.iter().all(|case| case["ok"].as_bool() == Some(true));
    let report = json!({
        "ok": ok,
        "setup_error": setup.err().map(|error| error.to_string()),
        "cases": cases,
        "elapsed_ms": started.elapsed().as_millis(),
        "limits": {"between_native_calls_seconds": BUDGET.as_secs(),
            "video_bytes_observed_after_calls": MAX_VIDEO_BYTES,
            "report_bytes": MAX_REPORT_BYTES},
        "scope": [
            "Synthetic video-only H264; only Auto attempt 0 with GPU NV12 is allowed. No CPU input fallback or claim that the selected encoder is hardware.",
            "The primary case uploads new source content, submits the previous held sample, then prepares the next sample. The separate two-frame case checks two prepared samples' content independence only.",
            "The explicit active-time fixture excludes a synthetic 500ms pause; it does not exercise real pause controls, button-press timing, or long-duration synchronization.",
            "Prepared content is checked after source reuse and a final poison upload. Actual MFT deep-retention/backpressure pressure remains unverified by this short case.",
            "Each returned writer has exactly one explicit Finalize. A failed creation's internal teardown is outside that count.",
            "The time budget is cooperative between native calls, not a hard timeout; file size checks are observations, not a hard disk quota.",
            "Compressed sample metadata and full decoded BGRA frames are separate evidence; this is not a performance, perceptual quality, HDR, playback, or real desktop test.",
            "All artifacts are retained in this newly owned directory, including failures."
        ]
    });
    let report = write_report(&directory, report)?;
    assert_eq!(
        report["ok"].as_bool(),
        Some(true),
        "GPU look-behind 回归失败，证据：{}",
        directory.join("report.json").display()
    );
    Ok(())
}

fn run_case(
    parent: &Path,
    order: Order,
    device: &ID3D11Device,
    chart: &ColorChart,
    started: Instant,
) -> Value {
    let directory = parent.join(order.name());
    let path = directory.join("clip.mp4");
    let mut stage = "create_case_directory";
    let mut writer: Option<Mp4Writer> = None;
    let mut creation_error = Value::Null;
    let mut finalized_writer = Value::Null;
    let mut trace = Vec::with_capacity(order.starts().len() * 3 + 2);
    let mut prepared = 0;
    let mut written = 0;
    let mut uses_gpu_input = false;
    let mut explicit_finalize_calls = 0;
    let mut errors = Vec::new();
    let work = (|| -> Result<()> {
        fs::create_dir(&directory)?;
        checkpoint(started)?;
        stage = "fixture_spans";
        let spans = spans(order)?;
        stage = "create_source_texture";
        let (texture, context) = source_texture(device)?;
        checkpoint(started)?;
        stage = "create_gpu_attempt_0";
        let quality = quality_plan(
            WIDTH,
            HEIGHT,
            FPS,
            QualityProfile::Current(Preset::CompactMp4),
        )?;
        let plan = EncoderPlan::new(EncoderPolicy::Auto, quality);
        if plan.attempts().first().map(|attempt| attempt.input) != Some(VideoInput::GpuNv12) {
            return Err("Auto 第一次尝试不是 GPU NV12，拒绝改变测试路线".into());
        }
        let backend = EncoderBackendSession::new(plan, false);
        match backend.create_attempt(&path, 0, Some(device)) {
            Ok(created) => writer = Some(created),
            Err(error) => {
                creation_error = serde_json::to_value(&error)?;
                return Err(error.into());
            }
        }
        checkpoint(started)?;
        let active_writer = writer.as_mut().ok_or("writer 未创建")?;
        uses_gpu_input = active_writer.uses_gpu_input();
        if !uses_gpu_input {
            return Err("要求 GPU NV12，但后端返回了其它输入路线".into());
        }
        let crop = Rect::full(WIDTH, HEIGHT);
        let mut held: Option<PreparedVideoSample> = None;
        for (index, _) in order.starts().iter().enumerate() {
            stage = "upload_reused_source";
            checkpoint(started)?;
            let pixels = variant(chart.bgra(), index);
            upload(&context, &texture, &pixels)?;
            trace.push(json!({"operation":"upload_source", "frame":index}));
            checkpoint(started)?;
            // 先覆盖源，才能证明旧 Prepared 的 NV12 内容不再依赖可复用 BGRA 源。
            if matches!(order, Order::SubmitThenPrepare) {
                if let Some(previous) = held.take() {
                    stage = "write_previous_before_next_prepare";
                    active_writer.write_prepared_video(
                        previous,
                        spans[index - 1].time,
                        spans[index - 1].duration,
                    )?;
                    written += 1;
                    trace.push(json!({"operation":"write_previous", "frame":index - 1}));
                    observe_file(&path, started)?;
                }
            }
            stage = "prepare_gpu_sample";
            let next = active_writer.prepare_texture(&texture, crop)?;
            prepared += 1;
            trace.push(json!({"operation":"prepare", "frame":index}));
            checkpoint(started)?;
            if matches!(order, Order::TwoPreparedSamples) {
                // 仅两帧的一次内容稳定性检查，不把该顺序当生产背压压力测试。
                if let Some(previous) = held.take() {
                    stage = "write_previous_after_next_prepare";
                    active_writer.write_prepared_video(
                        previous,
                        spans[index - 1].time,
                        spans[index - 1].duration,
                    )?;
                    written += 1;
                    trace.push(json!({"operation":"write_previous", "frame":index - 1}));
                    observe_file(&path, started)?;
                }
            }
            held = Some(next);
        }
        stage = "poison_source_before_short_tail";
        let mut poison = vec![0; (SOURCE_WIDTH * SOURCE_HEIGHT * 4) as usize];
        for pixel in poison.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[244, 16, 212, 255]);
        }
        upload(&context, &texture, &poison)?;
        trace.push(json!({"operation":"poison_source", "encoded":false}));
        checkpoint(started)?;
        stage = "write_short_tail";
        let last = spans.last().ok_or("缺少短尾时间段")?;
        active_writer.write_prepared_video(
            held.take().ok_or("缺少 held 尾帧")?,
            last.time,
            last.duration,
        )?;
        written += 1;
        trace.push(json!({"operation":"write_short_tail", "frame":written - 1}));
        observe_file(&path, started)?;
        Ok(())
    })();
    if let Err(error) = work {
        errors.push(json!({"stage":stage, "error":error.to_string()}));
    }
    if let Some(active_writer) = writer.take() {
        explicit_finalize_calls += 1;
        let (result, diagnostics) = active_writer.finish_with_diagnostics();
        finalized_writer = diagnostics;
        if let Err(error) = result {
            errors.push(json!({"stage":"finalize", "error":error.to_string()}));
        }
        if let Err(error) = observe_file(&path, started) {
            errors.push(json!({"stage":"after_finalize", "error":error.to_string()}));
        }
    }
    let mut payload = Value::Null;
    let mut decode = Value::Null;
    if errors.is_empty() {
        match super::bitrate_check::inspect_cancellable(&path, &|| started.elapsed() >= BUDGET) {
            Ok(value) => {
                payload = value;
                if let Err(error) = validate_payload(&payload, order) {
                    errors.push(json!({"stage":"native_payload", "error":error.to_string()}));
                }
            }
            Err(error) => {
                errors.push(json!({"stage":"native_payload", "error":error.to_string()}));
            }
        }
        // payload 失败仍可保留独立完整解码证据；过预算时 decode 自己拒绝进入原生调用。
        decode = decode_clip(&path, order, chart, started);
        if decode["ok"].as_bool() != Some(true) {
            errors.push(json!({"stage":"full_decode", "error":decode["error"]}));
        }
    }
    json!({
        "case":order.name(), "ok":errors.is_empty(), "errors":errors,
        "output":path.display().to_string(),
        "output_bytes":fs::metadata(&path).ok().map(|metadata| metadata.len()),
        "source_size":[SOURCE_WIDTH,SOURCE_HEIGHT], "encoded_size":[WIDTH,HEIGHT],
        "fps":FPS, "source_right_bottom_sentinel":"cropped, source origin unchanged",
        "expected_active_pts_100ns":order.starts(), "expected_active_end_100ns":order.end(),
        "wall_pts_100ns":match order {Order::SubmitThenPrepare => Some(&WALL_STARTS[..]), _ => None},
        "excluded_pause_100ns":match order {Order::SubmitThenPrepare => Some([1_000_000,6_000_000]), _ => None},
        "wall_stop_100ns":match order {Order::SubmitThenPrepare => Some(6_750_000), _ => None},
        "tail_duration_100ns":order.end() - order.starts()[order.starts().len() - 1],
        "time_tolerance_ns":TIME_TOLERANCE.as_nanos(),
        "channel_orders":&CHANNEL_ORDERS[..order.starts().len()],
        "prepared":prepared, "written":written, "uses_gpu_input":uses_gpu_input,
        "explicit_finalize_calls":explicit_finalize_calls,
        "creation_error":creation_error, "finalized_writer":finalized_writer,
        "operations":trace, "video_payload":payload, "full_decode":decode
    })
}

fn spans(order: Order) -> Result<Vec<VideoSpan>> {
    let mut spans = Vec::with_capacity(order.starts().len());
    for (index, &start) in order.starts().iter().enumerate() {
        let end = order
            .starts()
            .get(index + 1)
            .copied()
            .unwrap_or(order.end());
        let span = video_span(ticks(start), ticks(end))?.ok_or("夹具包含零长时间段")?;
        if span.time + span.duration != ticks(end) {
            return Err("100ns 时间段未闭合".into());
        }
        spans.push(span);
    }
    if spans.last().ok_or("缺少尾帧")?.duration >= Duration::from_secs_f64(1. / FPS as f64) {
        return Err("夹具尾帧未短于名义帧间隔".into());
    }
    Ok(spans)
}

fn ticks(value: u64) -> Duration {
    Duration::from_nanos(value * 100)
}

fn source_texture(device: &ID3D11Device) -> Result<(ID3D11Texture2D, ID3D11DeviceContext)> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: SOURCE_WIDTH,
        Height: SOURCE_HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        ..Default::default()
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(&description, None, Some(&mut texture))?;
        Ok((
            texture.ok_or("未创建自有 BGRA 纹理")?,
            device.GetImmediateContext()?,
        ))
    }
}

fn upload(context: &ID3D11DeviceContext, texture: &ID3D11Texture2D, pixels: &[u8]) -> Result<()> {
    if pixels.len() != (SOURCE_WIDTH * SOURCE_HEIGHT * 4) as usize {
        return Err("自有纹理上传长度不一致".into());
    }
    unsafe {
        context.UpdateSubresource(
            texture,
            0,
            None,
            pixels.as_ptr().cast(),
            SOURCE_WIDTH * 4,
            0,
        );
    }
    Ok(())
}

fn variant(base: &[u8], index: usize) -> Vec<u8> {
    let order = CHANNEL_ORDERS[index];
    let mut pixels = base.to_vec();
    for pixel in pixels.chunks_exact_mut(4) {
        let original = [pixel[0], pixel[1], pixel[2]];
        for channel in 0..3 {
            pixel[channel] = original[order[channel]];
        }
    }
    pixels
}

fn validate_payload(payload: &Value, order: Order) -> Result<()> {
    if payload["complete"].as_bool() != Some(true)
        || !payload["error"].is_null()
        || payload["samples"].as_u64().is_none_or(|count| count == 0)
        || payload["payload_bytes"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
    {
        return Err("原生 H264 压缩负载未完整到 EOF 或为空".into());
    }
    for key in [
        "missing_timestamps",
        "unknown_durations",
        "invalid_durations",
    ] {
        if payload[key].as_u64() != Some(0) {
            return Err(format!("原生压缩负载 {key} 非零或未知").into());
        }
    }
    for (key, expected) in [
        ("minimum_pts_100ns", 0),
        (
            "maximum_pts_100ns",
            *order.starts().last().ok_or("缺少 PTS")?,
        ),
        ("maximum_end_100ns", order.end()),
    ] {
        let actual = payload[key].as_i64().ok_or("压缩负载缺少时间范围")?;
        if actual < 0 || actual.abs_diff(expected as i64) > 10_000 {
            return Err(format!("压缩负载 {key}={actual} 与预期 {expected} 相差超过 1ms").into());
        }
    }
    Ok(())
}

fn decode_clip(path: &Path, order: Order, chart: &ColorChart, started: Instant) -> Value {
    let mut frames = Vec::with_capacity(order.starts().len());
    let mut eof = false;
    let mut source = Value::Null;
    let mut reader_diagnostics = Value::Null;
    let result = (|| -> Result<()> {
        checkpoint(started)?;
        let mut video = VideoReader::open(path)?;
        checkpoint(started)?;
        source = json!({"size":[video.width,video.height],
            "codec":format!("{:?}",video.info.codec),
            "frame_rate":video.info.frame_rate,
            "duration_ns":video.duration.as_nanos(),
            "stream_probe_error":video.info.stream_probe_error,
            "audio_present":video.info.streams.iter().any(|stream| stream.major_type == MFMediaType_Audio)});
        let outcome = (|| -> Result<()> {
            if (video.width, video.height) != (WIDTH, HEIGHT)
                || video.info.codec != MFVideoFormat_H264
                || video.info.stream_probe_error.is_some()
                || video
                    .info
                    .streams
                    .iter()
                    .any(|stream| stream.major_type == MFMediaType_Audio)
            {
                return Err("回读尺寸/原生 H264 类型/流探测与 video-only 夹具不符".into());
            }
            let stop = AtomicBool::new(false);
            let mut all_colors = true;
            let mut all_times = true;
            let mut previous_time = None;
            loop {
                checkpoint(started)?;
                let frame = video.next_cancellable(&stop)?;
                // 超预算发生在 ReadSample 内时，不能把返回的 None 冒称完整 EOF。
                checkpoint(started)?;
                let Some(mut frame) = frame else {
                    eof = true;
                    break;
                };
                let index = frames.len();
                let expected = order
                    .starts()
                    .get(index)
                    .copied()
                    .ok_or("完整解码帧数超出夹具上限")?;
                let error = frame.time.abs_diff(ticks(expected));
                let monotonic = previous_time.is_none_or(|previous| frame.time > previous);
                previous_time = Some(frame.time);
                all_times &= error <= TIME_TOLERANCE && monotonic;
                let channel_order = CHANNEL_ORDERS[index];
                for pixel in frame.bgra.chunks_exact_mut(4) {
                    let original = [pixel[0], pixel[1], pixel[2]];
                    for channel in 0..3 {
                        pixel[channel_order[channel]] = original[channel];
                    }
                }
                let comparison = chart.compare(&frame.bgra, WIDTH, HEIGHT, COLOR_TOLERANCE)?;
                all_colors &= comparison.passed;
                frames.push(json!({"index":index,"pts_ns":frame.time.as_nanos(),
                    "expected_pts_100ns":expected,"time_error_ns":error.as_nanos(),
                    "strictly_increasing":monotonic,"color":comparison}));
            }
            if frames.len() != order.starts().len() || !eof || !all_colors || !all_times {
                return Err("完整解码的 EOF/帧数/逐帧时间或色块稳定性未通过".into());
            }
            Ok(())
        })();
        reader_diagnostics = video.diagnostics();
        outcome
    })();
    json!({"ok":result.is_ok(),"error":result.err().map(|error| error.to_string()),
        "complete_video_eof":eof,"frames":frames,"source":source,
        "reader_diagnostics":reader_diagnostics,
        "scope":"Every expected decoded display frame is checked, independent of compressed sample count; channel permutation is undone before the shared interior-patch comparison."})
}

fn checkpoint(started: Instant) -> Result<()> {
    if started.elapsed() >= BUDGET {
        return Err("GPU look-behind 原生回归已达协作式时间预算".into());
    }
    Ok(())
}

fn observe_file(path: &Path, started: Instant) -> Result<()> {
    checkpoint(started)?;
    if fs::metadata(path)?.len() > MAX_VIDEO_BYTES {
        return Err("GPU look-behind 自有 MP4 超出观测字节预算".into());
    }
    Ok(())
}

fn unique_directory() -> Result<PathBuf> {
    let parent = Path::new("target").join("recording-video-lookbehind");
    fs::create_dir_all(&parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for serial in 0..16 {
        let directory = parent.join(format!("native-{}-{stamp}-{serial}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法创建独占 GPU look-behind 回归目录".into())
}

fn write_report(directory: &Path, mut report: Value) -> Result<Value> {
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > MAX_REPORT_BYTES {
        report = json!({"ok":false,"error":"报告超出预算，原报告未写入",
            "original_report_bytes":bytes.len(),"report_limit_bytes":MAX_REPORT_BYTES,
            "setup_error":report["setup_error"],
            "case_failures":report["cases"].as_array().map(|cases| cases.iter()
                .map(|case| json!({"case":case["case"],"ok":case["ok"],"errors":case["errors"]}))
                .collect::<Vec<_>>())});
        bytes = serde_json::to_vec_pretty(&report)?;
        if bytes.len() > MAX_REPORT_BYTES {
            report = json!({"ok":false,"error":"报告与失败摘要均超出预算；自有媒体保留",
                "report_limit_bytes":MAX_REPORT_BYTES});
            bytes = serde_json::to_vec_pretty(&report)?;
        }
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(report)
}

mod control_native {
    use super::*;
    use crate::recording::{
        bitrate_check,
        clock::ClockPoint,
        close_video_frame,
        control_receipt::{ControlKind, ControlQueue},
        control_state::ControlState,
        stop::{self, RecordingTail},
        Encoder, HeldVideoFrame, VideoPixels,
    };

    const STARTS: [u64; 2] = [0, 200_000];

    #[derive(Clone, Copy)]
    enum Scenario {
        FastPulses,
        StopWhilePaused,
    }

    impl Scenario {
        fn name(self) -> &'static str {
            match self {
                Self::FastPulses => "fast_pulses_first_stop",
                Self::StopWhilePaused => "stop_while_paused",
            }
        }

        fn expected_end(self) -> u64 {
            match self {
                Self::FastPulses => 650_000,
                Self::StopWhilePaused => 300_000,
            }
        }

        fn stop_ms(self) -> u64 {
            match self {
                Self::FastPulses => 85,
                Self::StopWhilePaused => 90,
            }
        }

        fn take_ms(self) -> u64 {
            match self {
                Self::FastPulses => 200,
                Self::StopWhilePaused => 300,
            }
        }

        fn pauses(self) -> &'static [(u64, bool)] {
            match self {
                Self::FastPulses => &[(30, true), (40, false), (50, true), (60, false)],
                Self::StopWhilePaused => &[(30, true)],
            }
        }
    }

    fn point(base: Instant, millis: u64) -> ClockPoint {
        ClockPoint {
            instant: base + Duration::from_millis(millis),
            qpc: None,
        }
    }

    #[derive(Default)]
    struct TailReceipt {
        finalize_calls: u32,
        finalize_error: Option<String>,
        finalized_writer: Value,
    }

    struct VideoOnlyTail<'a> {
        writer: Mp4Writer,
        receipt: &'a mut TailReceipt,
    }

    impl RecordingTail for VideoOnlyTail<'_> {
        type Tracks = ();

        fn flush_audio(&mut self) -> Result<()> {
            // 本夹具没有 AudioSink、WAV 或 AAC 流，不模拟任何音频提交。
            Ok(())
        }

        fn finish_tracks(&mut self) -> Result<()> {
            Ok(())
        }

        fn finish_video(self) -> Result<()> {
            self.receipt.finalize_calls += 1;
            let (result, diagnostics) = self.writer.finish_with_diagnostics();
            self.receipt.finalize_error = result.as_ref().err().map(ToString::to_string);
            self.receipt.finalized_writer = diagnostics;
            result
        }
    }

    fn validate_control_payload(payload: &Value, end: u64) -> Result<()> {
        if payload["complete"] != true
            || !payload["error"].is_null()
            || payload["samples"].as_u64().is_none_or(|count| count == 0)
            || payload["payload_bytes"]
                .as_u64()
                .is_none_or(|bytes| bytes == 0)
        {
            return Err("控制短片的原生 H264 负载未完整到 EOF 或为空".into());
        }
        for key in [
            "missing_timestamps",
            "unknown_durations",
            "invalid_durations",
            "native_type_changes",
            "current_type_changes",
        ] {
            if payload[key].as_u64() != Some(0) {
                return Err(format!("控制短片压缩负载 {key} 非零或未知").into());
            }
        }
        for (key, expected) in [
            ("minimum_pts_100ns", STARTS[0]),
            ("maximum_pts_100ns", STARTS[1]),
            ("maximum_end_100ns", end),
            ("observed_span_100ns", end),
        ] {
            let actual = payload[key]
                .as_i64()
                .ok_or("控制短片缺少原生负载时间范围")?;
            if actual < 0 || actual.abs_diff(expected as i64) > 10_000 {
                return Err(format!("控制短片 {key}={actual} 超过收据边界的 1ms 容差").into());
            }
        }
        Ok(())
    }

    fn decode_control_clip(path: &Path, chart: &ColorChart, started: Instant) -> Value {
        let mut frames = Vec::with_capacity(STARTS.len());
        let mut eof = false;
        let mut source = Value::Null;
        let mut reader_diagnostics = Value::Null;
        let result = (|| -> Result<()> {
            checkpoint(started)?;
            let mut reader = VideoReader::open(path)?;
            checkpoint(started)?;
            source = json!({"size":[reader.width,reader.height],
                "codec":format!("{:?}",reader.info.codec),"duration_ns":reader.duration.as_nanos()});
            let work = (|| -> Result<()> {
                if (reader.width, reader.height) != (WIDTH, HEIGHT)
                    || reader.info.codec != MFVideoFormat_H264
                    || reader.info.stream_probe_error.is_some()
                    || reader
                        .info
                        .streams
                        .iter()
                        .any(|stream| stream.major_type == MFMediaType_Audio)
                {
                    return Err("控制短片回读的尺寸/H264/无音轨合同不符".into());
                }
                let stop = AtomicBool::new(false);
                let mut previous = None;
                loop {
                    checkpoint(started)?;
                    let frame = reader.next_cancellable(&stop)?;
                    checkpoint(started)?;
                    let Some(mut frame) = frame else {
                        eof = true;
                        break;
                    };
                    let index = frames.len();
                    let expected = STARTS
                        .get(index)
                        .copied()
                        .ok_or("控制短片解码超过两帧上限")?;
                    if (reader.width, reader.height) != (WIDTH, HEIGHT) {
                        return Err("控制短片解码尺寸中途变化".into());
                    }
                    let error = frame.time.abs_diff(ticks(expected));
                    let increasing = previous.is_none_or(|old| frame.time > old);
                    previous = Some(frame.time);
                    let order = CHANNEL_ORDERS[index];
                    for pixel in frame.bgra.chunks_exact_mut(4) {
                        let original = [pixel[0], pixel[1], pixel[2]];
                        for channel in 0..3 {
                            pixel[order[channel]] = original[channel];
                        }
                    }
                    let color = chart.compare(&frame.bgra, WIDTH, HEIGHT, COLOR_TOLERANCE)?;
                    frames.push(json!({"index":index,"pts_ns":frame.time.as_nanos(),
                        "expected_pts_100ns":expected,"time_error_ns":error.as_nanos(),
                        "time_ok":increasing && error <= TIME_TOLERANCE,"color":color}));
                }
                if !eof
                    || frames.len() != STARTS.len()
                    || frames
                        .iter()
                        .any(|frame| frame["time_ok"] != true || frame["color"]["passed"] != true)
                {
                    return Err("控制短片未通过完整 EOF/两帧/PTS/颜色检查".into());
                }
                Ok(())
            })();
            reader_diagnostics = reader.diagnostics();
            work
        })();
        json!({"ok":result.is_ok(),"error":result.err().map(|error| error.to_string()),
            "complete_video_eof":eof,"frames":frames,"source":source,
            "reader_diagnostics":reader_diagnostics})
    }

    fn run_control_case(
        parent: &Path,
        scenario: Scenario,
        device: &ID3D11Device,
        chart: &ColorChart,
        started: Instant,
    ) -> Value {
        let directory = parent.join(scenario.name());
        let path = directory.join("clip.mp4");
        let mut stage = "case_directory";
        let mut encoder = None;
        let mut held = None;
        let mut spans = 0;
        let mut prepared = 0;
        let mut uses_gpu_input = false;
        let mut creation_error = Value::Null;
        let mut controls_report = Value::Null;
        let mut errors = Vec::new();
        let body = (|| -> Result<()> {
            fs::create_dir(&directory)?;
            checkpoint(started)?;
            let base = Instant::now();
            let mut queue = ControlQueue::new();
            let mut controls = ControlState::new(queue.begin(point(base, 0)));
            controls.check_begin()?;
            let (texture, context) = source_texture(device)?;
            checkpoint(started)?;
            let quality = quality_plan(
                WIDTH,
                HEIGHT,
                FPS,
                QualityProfile::Current(Preset::CompactMp4),
            )?;
            let plan = EncoderPlan::new(EncoderPolicy::Auto, quality);
            if plan.attempts().first().map(|attempt| attempt.input) != Some(VideoInput::GpuNv12) {
                return Err("控制短片的指定 attempt 0 不再是 GPU NV12".into());
            }
            let backend = EncoderBackendSession::new(plan, false);
            stage = "create_gpu_attempt_0";
            let writer = match backend.create_attempt(&path, 0, Some(device)) {
                Ok(writer) => writer,
                Err(error) => {
                    creation_error = serde_json::to_value(&error)?;
                    return Err(error.into());
                }
            };
            encoder = Some(Encoder::Ready(writer));
            let active = encoder.as_mut().ok_or("控制短片编码器缺失")?;
            uses_gpu_input = active
                .writer()
                .ok_or("控制短片 writer 缺失")?
                .uses_gpu_input();
            if !uses_gpu_input {
                return Err("控制短片要求 GPU NV12，拒绝输入回退".into());
            }
            let crop = Rect::full(WIDTH, HEIGHT);
            for index in 0..STARTS.len() {
                stage = "close_previous_then_prepare";
                checkpoint(started)?;
                let fence = if index == 0 {
                    controls.latest()
                } else {
                    controls.apply(queue.take(point(base, 20)), |_, _| {})?
                };
                if fence.stopped || fence.active != ticks(STARTS[index]) {
                    return Err("控制短片采帧 fence 与预期 0/20ms 不符".into());
                }
                upload(&context, &texture, &variant(chart.bgra(), index))?;
                close_video_frame(active, &mut held, fence.active, &mut spans)?;
                let sample = active
                    .writer()
                    .ok_or("控制短片 writer 缺失")?
                    .prepare_texture(&texture, crop)?;
                prepared += 1;
                held = Some(HeldVideoFrame {
                    pixels: VideoPixels::Prepared(sample),
                    time: fence.active,
                });
                observe_file(&path, started)?;
            }
            stage = "apply_delayed_control_batch";
            for &(millis, paused) in scenario.pauses() {
                queue.request(ControlKind::Pause(paused), point(base, millis))?;
            }
            queue.request(ControlKind::Stop, point(base, scenario.stop_ms()))?;
            let first_stop = queue.stop_receipt().ok_or("控制短片缺少首 Stop")?;
            queue.request(ControlKind::Stop, point(base, scenario.stop_ms() + 20))?;
            queue.request(
                ControlKind::Pause(false),
                point(base, scenario.stop_ms() + 30),
            )?;
            let mut callbacks = Vec::with_capacity(4);
            let fence = controls.apply(
                queue.take(point(base, scenario.take_ms())),
                |paused, point| {
                    callbacks.push((
                        point.instant.duration_since(base).as_millis() as u64,
                        paused,
                    ));
                },
            )?;
            queue.request(ControlKind::Stop, point(base, scenario.take_ms() + 10))?;
            let late =
                controls.apply(queue.take(point(base, scenario.take_ms() + 200)), |_, _| {})?;
            let finished = controls.finish()?;
            let repeated_finish = controls.finish()?;
            let valid = fence.stopped
                && fence.point == first_stop.point
                && fence.point == point(base, scenario.stop_ms())
                && fence.active == ticks(scenario.expected_end())
                && callbacks.as_slice() == scenario.pauses()
                && queue.stop_receipt() == Some(first_stop)
                && late == fence
                && finished == fence
                && repeated_finish == fence
                && controls.clock().active_at(point(base, 1000).instant) == Some(fence.active);
            controls_report = json!({"ok":valid,"synthetic_stop_wall_ms":scenario.stop_ms(),
                "synthetic_take_wall_ms":scenario.take_ms(),"late_take_wall_ms":scenario.take_ms()+200,
                "first_stop_seq":first_stop.seq,"first_stop_wall_ms":first_stop.point.instant.duration_since(base).as_millis(),
                "accepted_tail_100ns":fence.active.as_nanos()/100,"expected_tail_100ns":scenario.expected_end(),
                "pause_callbacks":callbacks,"repeated_stop_preserved":queue.stop_receipt()==Some(first_stop),
                "late_take_preserved":late==fence,"finish_idempotent":finished==fence && repeated_finish==fence});
            if !valid {
                return Err("延迟 take、快速暂停或首 Stop/finish 边界不符".into());
            }
            stage = "close_tail_at_accepted_control_fence";
            checkpoint(started)?;
            close_video_frame(active, &mut held, finished.active, &mut spans)?;
            if spans != STARTS.len() as u64 || held.is_some() {
                return Err("控制短片生产封帧计数不等于两帧".into());
            }
            observe_file(&path, started)?;
            Ok(())
        })();
        if let Err(error) = &body {
            errors.push(json!({"stage":stage,"error":error.to_string()}));
        }
        drop(held.take());
        let mut tail = TailReceipt::default();
        if let Some(Encoder::Ready(writer)) = encoder.take() {
            let finished = stop::finalize_recording(
                body,
                spans,
                VideoOnlyTail {
                    writer,
                    receipt: &mut tail,
                },
            );
            if let Err(error) = finished {
                errors.push(json!({"stage":"production_tail","error":error.to_string()}));
            }
            if let Err(error) = observe_file(&path, started) {
                errors.push(json!({"stage":"after_finalize","error":error.to_string()}));
            }
        }
        let mut payload = Value::Null;
        let mut decoded = Value::Null;
        if errors.is_empty() {
            let inspected =
                bitrate_check::inspect_cancellable(&path, &|| checkpoint(started).is_err());
            match inspected {
                Ok(value) => {
                    let checked = checkpoint(started)
                        .and_then(|()| validate_control_payload(&value, scenario.expected_end()));
                    if let Err(error) = checked {
                        errors.push(json!({"stage":"payload","error":error.to_string()}));
                    }
                    payload = value;
                }
                Err(error) => errors.push(json!({"stage":"payload","error":error.to_string()})),
            }
            decoded = decode_control_clip(&path, chart, started);
            if decoded["ok"] != true {
                errors.push(json!({"stage":"decode","error":decoded["error"]}));
            }
        }
        json!({"case":scenario.name(),"ok":errors.is_empty() && tail.finalize_calls==1 && spans==2,
            "errors":errors,"creation_error":creation_error,"controls":controls_report,
            "attempt_index":0,"uses_gpu_input":uses_gpu_input,
            "prepared":prepared,"closed_video_spans":spans,"explicit_finalize_calls":tail.finalize_calls,
            "finalize_error":tail.finalize_error,"finalized_writer":tail.finalized_writer,
            "video_payload":payload,"decode":decoded,
            "bytes":fs::metadata(&path).ok().map(|metadata| metadata.len())})
    }

    #[test]
    #[ignore = "合成有序控制收据与生产 held 封帧的真实 GPU/MF 短片；不采桌面、不播音、不开窗口"]
    fn recording_video_control_native_receipts_bound_media_tail() -> Result<()> {
        let directory = unique_directory()?;
        let started = Instant::now();
        let mut cases = Vec::with_capacity(2);
        let setup = (|| -> Result<()> {
            let _runtime = MfRuntime::new()?;
            checkpoint(started)?;
            let device = crate::recording_gpu::create_video_device()?;
            let chart = ColorChart::new(RecordingSizeLayout::new(ScreenRect {
                left: 0,
                top: 0,
                width: SOURCE_WIDTH,
                height: SOURCE_HEIGHT,
            })?)?;
            for scenario in [Scenario::FastPulses, Scenario::StopWhilePaused] {
                cases.push(run_control_case(
                    &directory, scenario, &device, &chart, started,
                ));
            }
            Ok(())
        })();
        let report = json!({"test":"recording_video_control_native_receipts_bound_media_tail",
            "ok":setup.is_ok() && cases.len()==2 && cases.iter().all(|case| case["ok"]==true),
            "setup_error":setup.err().map(|error| error.to_string()),"cases":cases,
            "source_size":[SOURCE_WIDTH,SOURCE_HEIGHT],"coded_size":[WIDTH,HEIGHT],
            "display_frames_per_case":2,"expected_pts_100ns":STARTS,"color_tolerance":COLOR_TOLERANCE,
            "time_tolerance_ns":TIME_TOLERANCE.as_nanos(),
            "limits":{"between_native_calls_seconds":BUDGET.as_secs(),"observed_video_bytes":MAX_VIDEO_BYTES,
                "report_bytes":MAX_REPORT_BYTES},
            "scope":["Synthetic ClockPoint inputs drive the production ControlQueue and ControlState directly; no sleep simulates blocking, and QPC/audio packet mapping is not exercised.",
                "Real GPU Prepared samples are held in production Encoder::Ready/HeldVideoFrame and submitted only by production close_video_frame; the accepted first-Stop active fence closes the second frame.",
                "A video-only RecordingTail adapter calls production finalize_recording and exactly one explicit real writer Finalize even after body errors; no AudioSink, AAC, WAV or WASAPI is instantiated.",
                "Native compressed-payload EOF and full two-frame decode separately check PTS, colors and the media end within 1 ms of the synthetic accepted boundary; compressed sample count is not a displayed-frame count.",
                "Only GPU attempt 0 is created; unavailable or failed cases remain failures. Pixel input does not prove hardware encoding. All artifacts, including failures, are retained.",
                "This does not verify Recording::request_control/control_begin/control_take Mutex races, the real worker/UI, actual MF blocking, desktop/camera capture, sound output, deep-retention pressure, HDR or long recording. Budgets are cooperative; native calls cannot be interrupted and file caps are observations."]});
        let report = write_report(&directory, report)?;
        assert_eq!(
            report["ok"],
            true,
            "控制收据真实媒体边界检查失败，报告：{}",
            directory.join("report.json").display()
        );
        Ok(())
    }
}
