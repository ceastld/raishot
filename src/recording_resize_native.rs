//! 显式 CPU/GPU 缩放路线的合成短片验收；不捕桌面、不打开窗口或声音设备。
//! 时间/文件预算只在原生调用之间检查，不能中断阻塞调用或构成磁盘硬配额。
#![cfg(test)]

use super::{
    frame_resize::{BgraDownscaler, FrameResizeGeometry},
    EncoderBackendSession, MfRuntime, Mp4Writer, VideoReader,
};
use crate::{
    geometry::{Rect, ScreenRect},
    recording::{bitrate_check, encoding_report::SessionEncodingReport},
    recording_color_check::ColorChart,
    recording_encoder_caps::{EncoderKind, VideoCodec, VideoInput},
    recording_encoder_policy::{EncoderPlan, EncoderPolicy},
    recording_options::{quality_plan, Preset, QualityPlan, QualityProfile},
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
        Direct3D11::{ID3D11Device, ID3D11Texture2D, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT},
        Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
    },
    Media::MediaFoundation::{MFMediaType_Audio, MFVideoFormat_H264},
};

const SOURCE_WIDTH: u32 = 385;
const SOURCE_HEIGHT: u32 = 257;
const CODED_WIDTH: u32 = 384;
const CODED_HEIGHT: u32 = 256;
const WIDTH: u32 = 192;
const HEIGHT: u32 = 128;
const FPS: u32 = 30;
const FRAMES: usize = 12;
const COLOR_TOLERANCE: f64 = 12.;
const TIME_TOLERANCE: Duration = Duration::from_millis(1);
const CASE_BUDGET: Duration = Duration::from_secs(45);
const MAX_VIDEO_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Cpu,
    GpuUpload,
    GpuTexture,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu_rgb32",
            Self::GpuUpload => "gpu_bgra_upload",
            Self::GpuTexture => "gpu_texture_prepared",
        }
    }

    fn index(self) -> usize {
        if self == Self::Cpu {
            1
        } else {
            0
        }
    }

    fn input(self) -> VideoInput {
        if self == Self::Cpu {
            VideoInput::CpuRgb32
        } else {
            VideoInput::GpuNv12
        }
    }
}

#[test]
#[ignore = "三条显式缩放路线的真实 H.264 往返；无回退、不打开窗口或声音设备"]
fn recording_resize_native_explicit_routes() -> Result<()> {
    let directory = unique_directory()?;
    let mut cases = Vec::with_capacity(3);
    let setup = (|| -> Result<()> {
        let source_layout = RecordingSizeLayout::new(ScreenRect {
            left: -100,
            top: -50,
            width: SOURCE_WIDTH,
            height: SOURCE_HEIGHT,
        })?;
        let target_layout = RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width: WIDTH,
            height: HEIGHT,
        })?;
        let source = ColorChart::new(source_layout)?;
        let target = ColorChart::new(target_layout)?;
        let geometry = FrameResizeGeometry::new(CODED_WIDTH, CODED_HEIGHT, WIDTH, HEIGHT)?;
        if source_layout.encoded_width != CODED_WIDTH
            || source_layout.encoded_height != CODED_HEIGHT
            || source.bgra().len() != (SOURCE_WIDTH * SOURCE_HEIGHT * 4) as usize
            || target.bgra().len() != (WIDTH * HEIGHT * 4) as usize
            || source.bgra().len() + target.bgra().len() > 512 * 1024
        {
            return Err("缩放 fixture 的源/目标像素范围或预算不匹配".into());
        }
        // 16块的边界恰好按半缩放对齐。独立目标图必须逐字节等于纯像素结果，
        // 包括最外行列；不能用同一个错误的缩放结果生成自己的预期。
        let mut reference = BgraDownscaler::new(geometry)?;
        if reference.resize(source.bgra(), SOURCE_WIDTH)? != target.bgra() {
            return Err("独立目标 ColorChart 与源裁哨兵后的精确半缩放不一致".into());
        }
        drop(reference);
        let quality = quality_plan(
            WIDTH,
            HEIGHT,
            FPS,
            QualityProfile::Current(Preset::CompactMp4),
        )?;
        let _runtime = MfRuntime::new()?;
        for route in [Route::Cpu, Route::GpuUpload, Route::GpuTexture] {
            cases.push(run_case(&directory, route, quality, &source, &target));
        }
        Ok(())
    })();
    let ok = setup.is_ok() && cases.len() == 3 && cases.iter().all(|case| case["ok"] == true);
    let report = write_report(
        &directory,
        json!({
            "ok":ok,"setup_error":setup.err().map(|error| error.to_string()),"cases":cases,
            "source_size":[SOURCE_WIDTH,SOURCE_HEIGHT],"source_coded_size":[CODED_WIDTH,CODED_HEIGHT],
            "source_physical_origin":[-100,-50],"target_size":[WIDTH,HEIGHT],"fps":FPS,"frames":FRAMES,
            "reference":"independent target ColorChart; exact CPU pre-encode comparison confirms the 4x4 block geometry and cropped odd-edge sentinel reference",
            "color_tolerance":COLOR_TOLERANCE,"time_tolerance_ns":TIME_TOLERANCE.as_nanos(),
            "limits":{"case_seconds":CASE_BUDGET.as_secs(),"video_bytes":MAX_VIDEO_BYTES,
                "report_bytes":MAX_REPORT_BYTES,"fixture_bgra_bytes":512*1024},
            "scope":"three explicit attempts, without route or codec fallback; one static synthetic chart repeated for 12 frames, no desktop/camera/audio/window; each route requires an observed input and MFT identity, H264 output, one explicit Finalize, complete native payload and independent complete display-frame decoding; not GPU/CPU bit identity, physical-linear interpolation, HDR, text quality, performance comparison or production ROI/annotation validation",
            "budget_scope":"cooperative checks before/after native calls and between file writes/reads; native blocking calls cannot be interrupted and a write/Finalize can exceed the observed file cap before returning",
            "corner_scope":"four corner interiors plus separate final-column/final-row strips and nearby interior controls use lossy RGB mean tolerance 12; no exact absence of sub-tolerance leakage or color accuracy is claimed"
        }),
    )?;
    assert_eq!(
        report["ok"],
        true,
        "缩放原生往返未全部通过；证据：{}；{report}",
        directory.display()
    );
    Ok(())
}

fn run_case(
    parent: &Path,
    route: Route,
    quality: QualityPlan,
    source: &ColorChart,
    target: &ColorChart,
) -> Value {
    let started = Instant::now();
    let directory = parent.join(route.name());
    let path = directory.join("clip.mp4");
    let mut stage = "create_case_directory";
    let mut writer: Option<Mp4Writer> = None;
    let mut creation_error = Value::Null;
    let mut finalized_writer = Value::Null;
    let mut written = 0;
    let mut prepared = 0;
    let mut finalize_calls = 0;
    let mut errors = Vec::new();
    let work = (|| -> Result<()> {
        fs::create_dir(&directory)?;
        checkpoint(started)?;
        stage = "create_explicit_scaled_attempt";
        let plan = EncoderPlan::new(EncoderPolicy::Auto, quality);
        if plan
            .attempts()
            .get(route.index())
            .map(|attempt| attempt.input)
            != Some(route.input())
        {
            return Err("缩放 fixture 的固定尝试 index 与指定输入路线不匹配".into());
        }
        let backend = EncoderBackendSession::new(plan, false);
        match backend.create_scaled_attempt(&path, route.index(), CODED_WIDTH, CODED_HEIGHT, None) {
            Ok(created) => writer = Some(created),
            Err(error) => {
                creation_error = serde_json::to_value(&error)?;
                return Err(error.into());
            }
        }
        checkpoint(started)?;
        let active = writer.as_mut().ok_or("缺少缩放 writer")?;
        if active.uses_gpu_input() != (route != Route::Cpu) {
            return Err("缩放尝试未返回指定输入路线，拒绝静默回退".into());
        }
        stage = "create_source_texture";
        let texture = if route == Route::GpuTexture {
            // 本测试是 MF 子模块，只读实际 GPU device；不向产品 API 增加 getter。
            let device = active
                .gpu
                .as_ref()
                .ok_or("纹理路线缺少实际 GPU 输入")?
                .device()
                .clone();
            let texture = source_texture(&device)?;
            unsafe {
                device.GetImmediateContext()?.UpdateSubresource(
                    &texture,
                    0,
                    None,
                    source.bgra().as_ptr().cast(),
                    SOURCE_WIDTH * 4,
                    0,
                );
            }
            checkpoint(started)?;
            Some(texture)
        } else {
            None
        };
        for index in 0..FRAMES {
            stage = "write_scaled_frame";
            checkpoint(started)?;
            let time = frame_time(index);
            let duration = frame_time(index + 1) - time;
            if let Some(texture) = &texture {
                let sample =
                    active.prepare_texture(texture, Rect::full(SOURCE_WIDTH, SOURCE_HEIGHT))?;
                prepared += 1;
                checkpoint(started)?;
                active.write_prepared_video(sample, time, duration)?;
            } else {
                active.write_video(source.bgra(), SOURCE_WIDTH, time, duration)?;
            }
            written += 1;
            observe_file(&path, started)?;
        }
        Ok(())
    })();
    if let Err(error) = work {
        errors.push(json!({"stage":stage,"error":error.to_string()}));
    }
    if let Some(active) = writer.take() {
        finalize_calls += 1;
        let (finished, diagnostics) = active.finish_with_diagnostics();
        finalized_writer = diagnostics;
        if let Err(error) = finished {
            errors.push(json!({"stage":"finalize","error":error.to_string()}));
        }
        if let Err(error) = observe_file(&path, started) {
            errors.push(json!({"stage":"after_finalize","error":error.to_string()}));
        }
    }
    let mut observation = Value::Null;
    if finalize_calls == 1 {
        match validate_writer(&finalized_writer, route, quality) {
            Ok(value) => observation = value,
            Err(error) => {
                errors.push(json!({"stage":"actual_writer_observation","error":error.to_string()}))
            }
        }
    }
    let mut payload = Value::Null;
    let mut decoded = Value::Null;
    if errors.is_empty() && written == FRAMES && finalize_calls == 1 {
        match bitrate_check::inspect_cancellable(&path, &|| started.elapsed() >= CASE_BUDGET) {
            Ok(value) => {
                payload = value;
                if let Err(error) = validate_payload(&payload) {
                    errors.push(json!({"stage":"native_payload","error":error.to_string()}));
                }
            }
            Err(error) => errors.push(json!({"stage":"native_payload","error":error.to_string()})),
        }
        decoded = decode_clip(&path, target, started);
        if decoded["ok"] != true {
            errors.push(json!({"stage":"complete_decode","error":decoded["error"]}));
        }
    }
    let ok = errors.is_empty()
        && written == FRAMES
        && finalize_calls == 1
        && decoded["ok"] == true
        && payload["complete"] == true;
    json!({"route":route.name(),"attempt_index":route.index(),"ok":ok,"errors":errors,
        "output":path.display().to_string(),"output_bytes":fs::metadata(&path).ok().map(|file|file.len()),
        "requested_plan":quality,"written":written,"prepared":prepared,"explicit_finalize_calls":finalize_calls,
        "creation_error":creation_error,"finalized_writer":finalized_writer,"actual_observation":observation,
        "video_payload":payload,"full_decode":decoded,"elapsed_ms":started.elapsed().as_secs_f64()*1000.})
}

fn validate_writer(diagnostics: &Value, route: Route, quality: QualityPlan) -> Result<Value> {
    let report = SessionEncodingReport::from_finalized_writer(
        EncoderPolicy::Auto,
        quality.profile,
        Some(route.index()),
        diagnostics.clone(),
    );
    let kind = report.video_encoder_kind();
    if report.input() != Some(route.input())
        || report.confirmed_video_codec() != Some(VideoCodec::H264)
        || !matches!(kind, EncoderKind::Hardware | EncoderKind::Software)
        || report.video_encoder_name().is_none()
        || diagnostics
            .pointer("/encoding_trial/finalize/state")
            .and_then(Value::as_str)
            != Some("succeeded")
        || diagnostics.pointer("/encoder_attempt/quality") != Some(&serde_json::to_value(quality)?)
        || diagnostics.pointer("/encoder_attempt/source_coded_size")
            != Some(&json!([CODED_WIDTH, CODED_HEIGHT]))
    {
        return Err("实际输入/MFT/H264输出/缩放源范围/参数或Finalize证据不完整".into());
    }
    let observed = report.initial_video_output_observation();
    if observed.size != Some((WIDTH, HEIGHT))
        || observed
            .fps
            .is_none_or(|(n, d)| u64::from(n) != u64::from(FPS) * u64::from(d))
    {
        return Err("实际初始化输出类型不是192×128、30fps".into());
    }
    Ok(
        json!({"input":report.input(),"codec":report.confirmed_video_codec(),
        "encoder_kind":kind,"encoder_name":report.video_encoder_name(),
        "initial_output_size":observed.size,"initial_output_fps":observed.fps}),
    )
}

fn validate_payload(payload: &Value) -> Result<()> {
    if payload["complete"] != true
        || !payload["error"].is_null()
        || payload["samples"].as_u64() != Some(FRAMES as u64)
        || payload["payload_bytes"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || payload["actual_payload_bitrate_bps"]
            .as_f64()
            .is_none_or(|rate| !rate.is_finite() || rate <= 0.)
        || payload["native_subtype"].as_str() != Some(format!("{MFVideoFormat_H264:?}").as_str())
        || payload["current_subtype"] != payload["native_subtype"]
    {
        return Err("缩放原生H264负载未完整EOF、样本/负载为空或实际subtype不匹配".into());
    }
    for field in [
        "missing_timestamps",
        "unknown_durations",
        "invalid_durations",
    ] {
        if payload[field].as_u64() != Some(0) {
            return Err(format!("原生负载 {field} 非零或未知").into());
        }
    }
    for (field, expected) in [
        ("minimum_pts_100ns", 0),
        ("maximum_pts_100ns", frame_ticks(FRAMES - 1)),
        ("maximum_end_100ns", frame_ticks(FRAMES)),
        ("observed_span_100ns", frame_ticks(FRAMES)),
    ] {
        let actual = payload[field].as_i64().ok_or("原生负载缺少有效时间范围")?;
        if actual < 0 || i128::from(actual).abs_diff(i128::from(expected)) > 10_000 {
            return Err(format!("原生负载 {field} 时间范围不匹配").into());
        }
    }
    let first = payload["minimum_pts_100ns"].as_i64().ok_or("缺首PTS")?;
    let end = payload["maximum_end_100ns"].as_i64().ok_or("缺末尾时间")?;
    let span = payload["observed_span_100ns"]
        .as_i64()
        .ok_or("缺时间跨度")?;
    if i128::from(end) - i128::from(first) != i128::from(span) || span <= 0 {
        return Err("原生负载时间跨度内部矛盾".into());
    }
    Ok(())
}

fn decode_clip(path: &Path, target: &ColorChart, started: Instant) -> Value {
    let mut frames = Vec::with_capacity(FRAMES);
    let mut eof = false;
    let mut source = Value::Null;
    let mut reader_diagnostics = Value::Null;
    let work = (|| -> Result<()> {
        checkpoint(started)?;
        let mut reader = VideoReader::open(path)?;
        checkpoint(started)?;
        source = json!({"size":[reader.width,reader.height],"codec":format!("{:?}",reader.info.codec),
            "frame_rate":reader.info.frame_rate,"stream_probe_error":reader.info.stream_probe_error});
        let decoded = (|| -> Result<()> {
            if (reader.width, reader.height) != (WIDTH, HEIGHT)
                || reader.info.codec != MFVideoFormat_H264
                || reader.info.stream_probe_error.is_some()
                || reader
                    .info
                    .streams
                    .iter()
                    .any(|stream| stream.major_type == MFMediaType_Audio)
            {
                return Err("回读目标尺寸/H264/视频独占流与fixture不一致".into());
            }
            let stop = AtomicBool::new(false);
            let mut previous = None;
            let mut all_ok = true;
            loop {
                checkpoint(started)?;
                let frame = reader.next_cancellable(&stop)?;
                checkpoint(started)?;
                let Some(frame) = frame else {
                    eof = true;
                    break;
                };
                let index = frames.len();
                if index >= FRAMES {
                    return Err("解码帧数超过固定12帧上限".into());
                }
                let error = frame.time.abs_diff(frame_time(index));
                let monotonic = previous.is_none_or(|time| frame.time > time);
                previous = Some(frame.time);
                let color = target.compare(&frame.bgra, WIDTH, HEIGHT, COLOR_TOLERANCE)?;
                let corners = corner_checks(&frame.bgra, target.bgra())?;
                all_ok &=
                    monotonic && error <= TIME_TOLERANCE && color.passed && corners["ok"] == true;
                frames.push(json!({"index":index,"pts_ns":frame.time.as_nanos(),
                    "expected_pts_100ns":frame_ticks(index),"time_error_ns":error.as_nanos(),
                    "strictly_increasing":monotonic,"color":color,"corners_and_edges":corners}));
            }
            if frames.len() != FRAMES || !eof || !all_ok {
                return Err("完整解码EOF/12帧/逐帧PTS/16色块或角落边缘哨兵检查未通过".into());
            }
            Ok(())
        })();
        reader_diagnostics = reader.diagnostics();
        decoded
    })();
    json!({"ok":work.is_ok(),"error":work.err().map(|error|error.to_string()),
        "complete_video_eof":eof,"frames":frames,"source":source,"reader_diagnostics":reader_diagnostics})
}

fn corner_checks(decoded: &[u8], reference: &[u8]) -> Result<Value> {
    let mut records = Vec::with_capacity(8);
    let mut means = Vec::with_capacity(8);
    let mut ok = true;
    for (label, x, y, width, height) in [
        ("top_left", 8, 8, 6, 6),
        ("top_right", WIDTH - 14, 8, 6, 6),
        ("bottom_left", 8, HEIGHT - 14, 6, 6),
        ("bottom_right", WIDTH - 14, HEIGHT - 14, 6, 6),
        ("right_outer", WIDTH - 1, 8, 1, 16),
        ("right_inner", WIDTH - 4, 8, 1, 16),
        ("bottom_outer", 8, HEIGHT - 1, 16, 1),
        ("bottom_inner", 8, HEIGHT - 4, 16, 1),
    ] {
        let actual = mean_rgb(decoded, x, y, width, height)?;
        let expected = mean_rgb(reference, x, y, width, height)?;
        let error = channel_error(actual, expected);
        ok &= error <= COLOR_TOLERANCE;
        means.push(actual);
        records.push(
            json!({"roi":label,"rect":[x,y,width,height],"expected_rgb":expected,
            "observed_rgb":actual,"maximum_mean_channel_error":error}),
        );
    }
    let right_difference = channel_error(means[4], means[5]);
    let bottom_difference = channel_error(means[6], means[7]);
    ok &= right_difference <= COLOR_TOLERANCE && bottom_difference <= COLOR_TOLERANCE;
    Ok(json!({"ok":ok,"tolerance":COLOR_TOLERANCE,"rois":records,
        "right_outer_inner_difference":right_difference,"bottom_outer_inner_difference":bottom_difference}))
}

fn mean_rgb(pixels: &[u8], x: u32, y: u32, width: u32, height: u32) -> Result<[f64; 3]> {
    if pixels.len() != (WIDTH * HEIGHT * 4) as usize
        || width == 0
        || height == 0
        || x.checked_add(width).is_none_or(|right| right > WIDTH)
        || y.checked_add(height).is_none_or(|bottom| bottom > HEIGHT)
    {
        return Err("角落ROI或BGRA范围无效".into());
    }
    let mut sums = [0_u64; 3];
    for row in y..y + height {
        for column in x..x + width {
            let at = ((row * WIDTH + column) * 4) as usize;
            for (channel, offset) in [2, 1, 0].into_iter().enumerate() {
                sums[channel] += u64::from(pixels[at + offset]);
            }
        }
    }
    Ok(sums.map(|sum| sum as f64 / f64::from(width * height)))
}

fn channel_error(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| (left - right).abs())
        .fold(0., f64::max)
}

fn source_texture(device: &ID3D11Device) -> Result<ID3D11Texture2D> {
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
    }
    texture.ok_or_else(|| "未创建独占源BGRA纹理".into())
}

fn frame_ticks(index: usize) -> u64 {
    index as u64 * 10_000_000 / u64::from(FPS)
}
fn frame_time(index: usize) -> Duration {
    Duration::from_nanos(frame_ticks(index) * 100)
}
fn checkpoint(started: Instant) -> Result<()> {
    if started.elapsed() >= CASE_BUDGET {
        return Err("缩放case已达协作式调用间预算".into());
    }
    Ok(())
}
fn observe_file(path: &Path, started: Instant) -> Result<()> {
    checkpoint(started)?;
    if fs::metadata(path)?.len() > MAX_VIDEO_BYTES {
        return Err("缩放自有MP4超过观测字节预算".into());
    }
    Ok(())
}
fn unique_directory() -> Result<PathBuf> {
    let parent = Path::new("target/recording-resize");
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for serial in 0..16 {
        let directory = parent.join(format!("native-{}-{stamp}-{serial}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法创建独占缩放验收目录".into())
}
fn write_report(directory: &Path, mut report: Value) -> Result<Value> {
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > MAX_REPORT_BYTES {
        report = json!({"ok":false,"error":"缩放报告超出预算，媒体保留",
            "original_report_bytes":bytes.len(),"setup_error":report["setup_error"],
            "case_failures":report["cases"].as_array().map(|cases|cases.iter()
                .map(|case|json!({"route":case["route"],"ok":case["ok"],"errors":case["errors"]})).collect::<Vec<_>>())});
        bytes = serde_json::to_vec_pretty(&report)?;
        if bytes.len() > MAX_REPORT_BYTES {
            report = json!({"ok":false,"error":"缩放报告及失败摘要均超出预算，媒体保留"});
            bytes = serde_json::to_vec_pretty(&report)?;
        }
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(report)
}
