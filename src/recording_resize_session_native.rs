//! 自绘输入的生产缩放策略交接；不创建桌面采集、输入覆盖层或窗口。
#![cfg(test)]

use super::*;
use crate::{
    annotation::Point,
    capture::Image,
    media_foundation::VideoReader,
    recording_encoder_caps::{EncoderKind, VideoInput},
};
use serde_json::{json, Value};
use std::{
    cell::Cell,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::windows::fs::MetadataExt,
    panic::{catch_unwind, AssertUnwindSafe},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::Win32::Media::MediaFoundation::{MFMediaType_Audio, MFVideoFormat_H264};

const SOURCE_WIDTH: u32 = 385;
const SOURCE_HEIGHT: u32 = 257;
const WIDTH: u32 = 192;
const HEIGHT: u32 = 128;
const FPS: u32 = 30;
const FRAMES: usize = 12;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 256 * 1024;
const BUDGET: Duration = Duration::from_secs(45);
const TIME_TOLERANCE: Duration = Duration::from_millis(1);
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

fn require(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

struct Fixture {
    directory: PathBuf,
    previous: [Option<OsString>; 2],
}

impl Fixture {
    fn new() -> Result<Self> {
        let target = std::path::absolute("target")?;
        safe_directory(&target)?;
        let parent = target.join("recording-resize-session");
        match fs::create_dir(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        safe_directory(&parent)?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        for index in 0..16 {
            let directory = parent.join(format!("native-{}-{stamp}-{index}", std::process::id()));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let data = directory.join("data");
                    fs::create_dir(&data)?;
                    let previous = [
                        std::env::var_os("RUSTSHOT_DATA_DIR"),
                        std::env::var_os("RUSTSHOT_EXPERIMENTAL_MEDIA"),
                    ];
                    std::env::set_var("RUSTSHOT_DATA_DIR", data);
                    std::env::set_var("RUSTSHOT_EXPERIMENTAL_MEDIA", "1");
                    return Ok(Self {
                        directory,
                        previous,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("缩放策略专项独占目录冲突过多".into())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (name, previous) in ["RUSTSHOT_DATA_DIR", "RUSTSHOT_EXPERIMENTAL_MEDIA"]
            .into_iter()
            .zip(&self.previous)
        {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn safe_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    require(
        metadata.is_dir() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
        "专项目录不是普通本地目录",
    )
}

fn checkpoint(started: Instant) -> Result<()> {
    require(started.elapsed() <= BUDGET, "缩放策略专项超过调用间预算")
}

fn frame_time(index: usize) -> Duration {
    Duration::from_nanos((index as u64 * 1_000_000_000 / u64::from(FPS)) / 100 * 100)
}

fn region() -> ScreenRect {
    ScreenRect {
        left: -120,
        top: -60,
        width: SOURCE_WIDTH,
        height: SOURCE_HEIGHT,
    }
}

fn state() -> Arc<Recording> {
    Arc::new(Recording {
        annotations: Arc::new(live_annotation::FrameState::default()),
        stop: AtomicBool::new(false),
        pause: AtomicBool::new(false),
        controls: Mutex::new(ControlQueue::new()),
        go: AtomicBool::new(true),
        discard: stop::DiscardFlag::new(false),
        frames: AtomicU64::new(0),
        active_ms: AtomicU64::new(0),
        progress: AtomicU32::new(0),
        phase: AtomicU8::new(Phase::Starting as u8),
        warnings: Mutex::new(Vec::new()),
        encoding_report: Mutex::new(None),
        camera_report: Mutex::new(None),
        outcome: Mutex::new(None),
    })
}

fn annotation(state: &Recording) -> Result<Arc<live_annotation::FrameScene>> {
    let region = region();
    let geometry = live_annotation::Geometry::new(
        ScreenRect {
            left: -200,
            top: -100,
            width: 640,
            height: 480,
        },
        region,
    )?;
    let mut editor = live_annotation::LiveEditor::new(geometry, state.annotations.clone());
    editor.set_drawing(true);
    editor.select(live_annotation::LiveTool::Rectangle);
    require(
        editor.apply_style(Some(0xffffff), Some(12.), Some(1.)),
        "标注样式未被真实编辑器接受",
    )?;
    let global = |x, y| Point {
        x: region.left as f32 + x,
        y: region.top as f32 + y,
    };
    require(editor.down(global(48., 40.), 2., false), "标注手势未开始")?;
    require(editor.moved(global(208., 168.)), "标注手势未移动")?;
    require(editor.up(global(208., 168.)), "标注手势未提交")?;
    editor
        .snapshot()
        .ok_or_else(|| "真实标注没有发布 FrameScene".into())
}

fn source_frame(index: usize, scene: &live_annotation::FrameScene) -> Result<Image> {
    let mut image = Image {
        width: SOURCE_WIDTH,
        height: SOURCE_HEIGHT,
        bgra: vec![0; (SOURCE_WIDTH * SOURCE_HEIGHT * 4) as usize],
        readback_ms: 0.,
    };
    for y in 0..SOURCE_HEIGHT {
        for x in 0..SOURCE_WIDTH {
            let offset = ((y * SOURCE_WIDTH + x) * 4) as usize;
            let gray = if (288..352).contains(&x) && (184..232).contains(&y) {
                64 + index as u8 * 12
            } else {
                24
            };
            let pixel = if x == 384 || y == 256 {
                [255, 0, 255, 255]
            } else {
                [gray, gray, gray, 255]
            };
            image.bgra[offset..offset + 4].copy_from_slice(&pixel);
        }
    }
    scene.composite(&mut image, region())?;
    RecordingSizeLayout::new(region())?.validate_frame(
        image.width,
        image.height,
        image.bgra.len(),
    )?;
    // 独立的几何参考，不把同一次合成结果缩放后拿来与自身比较。
    require(
        mean_luma(&image.bgra, SOURCE_WIDTH, SOURCE_HEIGHT, [64, 39, 128, 2])? >= 230.,
        "源标注顶部没有成为白色边",
    )?;
    Ok(image)
}

fn mean_luma(pixels: &[u8], width: u32, height: u32, area: [u32; 4]) -> Result<f64> {
    let [x, y, w, h] = area;
    require(
        pixels.len() == (u64::from(width) * u64::from(height) * 4) as usize
            && w > 0
            && h > 0
            && x.checked_add(w).is_some_and(|end| end <= width)
            && y.checked_add(h).is_some_and(|end| end <= height),
        "专项亮度采样范围无效",
    )?;
    let mut sum = 0_u64;
    for row in y..y + h {
        for column in x..x + w {
            let offset = ((row * width + column) * 4) as usize;
            sum += u64::from(pixels[offset])
                + u64::from(pixels[offset + 1])
                + u64::from(pixels[offset + 2]);
        }
    }
    Ok(sum as f64 / (f64::from(w) * f64::from(h) * 3.))
}

fn pixel_checks(pixels: &[u8], index: usize) -> Result<Value> {
    // 源矩形 (48,40)..(208,168) 的二分之一位置；均避开抗锯齿外缘。
    let mut edges = Vec::with_capacity(4);
    for (name, area) in [
        ("top", [32, 19, 64, 2]),
        ("left", [23, 28, 2, 48]),
        ("right", [103, 28, 2, 48]),
        ("bottom", [32, 83, 64, 2]),
    ] {
        let mean = mean_luma(pixels, WIDTH, HEIGHT, area)?;
        edges.push(json!({"edge":name,"target_area":area,"mean":mean,"white":mean>=200.}));
    }
    let mut dark = Vec::with_capacity(4);
    for area in [
        [8, 8, 6, 6],
        [164, 8, 8, 6],
        [52, 42, 24, 20],
        [178, 117, 8, 6],
    ] {
        let mean = mean_luma(pixels, WIDTH, HEIGHT, area)?;
        dark.push(json!({"target_area":area,"mean":mean,"dark":mean<=55.}));
    }
    let tag = mean_luma(pixels, WIDTH, HEIGHT, [148, 96, 24, 16])?;
    let expected_tag = f64::from(64 + index as u8 * 12);
    let passed = edges.iter().all(|v| v["white"] == true)
        && dark.iter().all(|v| v["dark"] == true)
        && (tag - expected_tag).abs() <= 5.;
    Ok(json!({"ok":passed,"white_edges":edges,"dark_anchors":dark,
        "frame_tag":{"mean":tag,"expected_gray":expected_tag,"tolerance":5},
        "reference":"analytic half-size positions, white >=200 / dark <=55; asymmetric rectangle and frame-index gray patch; not a rescaled copy of the source raster"}))
}

// 只观察生产 RecordingTail 的显式入口次数；不声称这是 MF 底层调用钩子。
struct ObservedTail<'a> {
    inner: SessionTail,
    calls: &'a Cell<u32>,
}
impl RecordingTail for ObservedTail<'_> {
    type Tracks = Vec<(AudioSource, PathBuf)>;
    fn flush_audio(&mut self) -> Result<()> {
        self.inner.flush_audio()
    }
    fn finish_tracks(&mut self) -> Result<Self::Tracks> {
        self.inner.finish_tracks()
    }
    fn finish_video(self) -> Result<()> {
        self.calls.set(self.calls.get().saturating_add(1));
        self.inner.finish_video()
    }
}

fn decode(path: &Path, started: Instant) -> Value {
    let mut frames = Vec::with_capacity(FRAMES);
    let mut eof = false;
    let mut reader_released = false;
    let mut source = Value::Null;
    let mut diagnostics = Value::Null;
    let result = (|| -> Result<()> {
        checkpoint(started)?;
        let mut reader = VideoReader::open(path)?;
        source = json!({"width":reader.width,"height":reader.height,
            "codec":format!("{:?}",reader.info.codec),"duration_ns":reader.duration.as_nanos()});
        let work = (|| -> Result<()> {
            require(
                (reader.width, reader.height) == (WIDTH, HEIGHT)
                    && reader.info.codec == MFVideoFormat_H264
                    && reader.info.stream_probe_error.is_none()
                    && !reader
                        .info
                        .streams
                        .iter()
                        .any(|stream| stream.major_type == MFMediaType_Audio)
                    && reader.duration.abs_diff(frame_time(FRAMES)) <= TIME_TOLERANCE,
                "回读尺寸/H264/无音轨/400ms声明时长不匹配",
            )?;
            let stop = AtomicBool::new(false);
            let mut previous = None;
            loop {
                checkpoint(started)?;
                let next = reader.next_cancellable(&stop)?;
                checkpoint(started)?;
                let Some(frame) = next else {
                    eof = true;
                    break;
                };
                let index = frames.len();
                require(index < FRAMES, "解码超过12帧上限")?;
                let time_ok = frame.time.abs_diff(frame_time(index)) <= TIME_TOLERANCE
                    && previous.is_none_or(|previous| frame.time > previous);
                previous = Some(frame.time);
                let pixels = pixel_checks(&frame.bgra, index)?;
                frames.push(json!({"index":index,"pts_ns":frame.time.as_nanos(),
                    "expected_pts_ns":frame_time(index).as_nanos(),"time_ok":time_ok,"pixels":pixels}));
            }
            require(
                eof && frames.len() == FRAMES
                    && frames
                        .iter()
                        .all(|frame| frame["time_ok"] == true && frame["pixels"]["ok"] == true),
                "完整EOF/12帧/PTS/缩放标注位置或逐帧灰度标记失败",
            )
        })();
        diagnostics = reader.diagnostics();
        drop(reader);
        reader_released = true;
        work
    })();
    json!({"ok":result.is_ok(),"error":result.err().map(|e|bounded(e.to_string())),
        "complete_video_eof":eof,"frames":frames,"source":source,
        "reader_diagnostics":diagnostics,"reader_owner_release_returned":reader_released})
}

fn payload_gate(value: &Value) -> Result<()> {
    require(
        value["complete"] == true
            && value["error"].is_null()
            && value["samples"].as_u64() == Some(FRAMES as u64)
            && value["payload_bytes"].as_u64().is_some_and(|v| v > 0)
            && value["native_subtype"].as_str() == Some(format!("{MFVideoFormat_H264:?}").as_str())
            && value["native_subtype"] == value["current_subtype"],
        "原生H264样本统计未完整通过",
    )?;
    for key in [
        "missing_timestamps",
        "unknown_durations",
        "invalid_durations",
        "native_type_changes",
        "current_type_changes",
    ] {
        require(
            value[key].as_u64() == Some(0),
            "压缩样本时间或类型观测不完整",
        )?;
    }
    for (key, time) in [
        ("minimum_pts_100ns", Duration::ZERO),
        ("maximum_pts_100ns", frame_time(FRAMES - 1)),
        ("maximum_end_100ns", frame_time(FRAMES)),
        ("observed_span_100ns", frame_time(FRAMES)),
    ] {
        let ticks = value[key].as_i64().ok_or("压缩样本缺时间范围")?;
        require(
            ticks >= 0 && i128::from(ticks).abs_diff((time.as_nanos() / 100) as i128) <= 10_000,
            "压缩样本PTS或400ms尾端不匹配",
        )?;
    }
    Ok(())
}

fn bounded(value: String) -> String {
    value.chars().take(1024).collect()
}

fn exercise(fixture: &Fixture, report: &mut Value) -> Result<()> {
    let started = Instant::now();
    report["stage"] = "production_request_validation".into();
    let options = RecordingOptions {
        fps: FPS,
        preset: Preset::CompactMp4,
        show_cursor: false,
        show_keys: false,
        show_mouse: false,
        microphone: false,
        system_audio: false,
        ..Default::default()
    };
    let encoding = EncodingOptions {
        policy: EncoderPolicy::SoftwareOnly,
        quality: Some(QualityProfile::Current(options.preset)),
    };
    let output = fixture.directory.join("requested-output.mp4");
    let requests = validate_session_requests_with_resize(
        region(),
        &options,
        &output,
        encoding,
        None,
        Some(CodecRequest::H264),
        Some(ResizeRequest {
            target_width: WIDTH,
            target_height: HEIGHT,
        }),
        crate::file_launch_command::experimental_media_enabled(),
    )?;
    let plan = requests
        .codec_plan
        .clone()
        .ok_or("生产预检没有生成显式codec计划")?;
    let scale = requests.scale_plan.ok_or("生产预检未保留缩放计划")?;
    require(
        plan.attempts().len() == 1
            && plan.encoder_plan().policy() == EncoderPolicy::SoftwareOnly
            && (
                scale.geometry.source_width,
                scale.geometry.source_height,
                scale.geometry.target_width,
                scale.geometry.target_height,
            ) == (384, 256, WIDTH, HEIGHT),
        "生产计划不是单次SoftwareOnly/384×256到192×128",
    )?;
    let source_region = region();
    report["request"] = json!({"region":{"left":source_region.left,"top":source_region.top,
        "width":source_region.width,"height":source_region.height},"scale":scale,"encoding_policy":encoding.policy,
        "quality":plan.encoder_plan().quality(),"codec_request":requests.codec_request,"output_plan":requests.output_plan});
    let runtime = MfRuntime::new()?;
    let device = crate::recording_gpu::create_video_device()?;
    let state = state();
    let scene = annotation(&state)?;
    let mut files = RecordingFiles::new(&output)?;
    files.mark_recording()?;
    let paths = files.encoder_attempt_paths(plan.attempts().len())?;
    let path = paths.first().ok_or("生产文件资格没有给出尝试槽")?.clone();
    report["artifact"] = path.display().to_string().into();
    checkpoint(started)?;
    report["stage"] = "start_production_policy_encoder".into();
    let mut encoder = start_policy_encoder(plan, Some(scale), device, paths, false, &state)?;
    let mut selected = EncoderSelection::default();
    let mut held = None;
    let mut spans = 0;
    let mut original_frames = 0;
    let body = (|| -> Result<()> {
        require(
            matches!(&encoder,Encoder::PolicyPending{thread:Some(_),frames,bytes} if frames.is_empty() && *bytes==0),
            "真实启动helper没有返回原始PolicyPending",
        )?;
        report["stage"] = "queue_source_frames_before_resolve".into();
        for index in 0..FRAMES {
            checkpoint(started)?;
            let image = source_frame(index, &scene)?;
            close_video_frame(&mut encoder, &mut held, frame_time(index), &mut spans)?;
            held = Some(HeldVideoFrame {
                pixels: VideoPixels::Cpu(image),
                time: frame_time(index),
            });
            original_frames += 1;
        }
        close_video_frame(&mut encoder, &mut held, frame_time(FRAMES), &mut spans)?;
        let expected_bytes = FRAMES * (SOURCE_WIDTH * SOURCE_HEIGHT * 4) as usize;
        require(
            matches!(&encoder,Encoder::PolicyPending{thread:Some(_),frames,bytes}
            if frames.len()==FRAMES && *bytes==expected_bytes
                && frames.iter().enumerate().all(|(i,(image,time,duration))|
                    image.width==SOURCE_WIDTH && image.height==SOURCE_HEIGHT
                    && *time==frame_time(i) && *duration==frame_time(i+1)-frame_time(i))),
            "生产缓存没有完整保留12个原stride源帧与原时间戳",
        )?;
        report["pending_queue"] = json!({"frames":FRAMES,"bytes":expected_bytes,
            "source_stride_pixels":SOURCE_WIDTH,"original_source_frames":original_frames,
            "closed_spans":spans,"held_empty":held.is_none()});
        Ok(())
    })();
    report["queue_error"] = body.as_ref().err().map(|e| bounded(e.to_string())).into();
    drop(held.take());
    report["stage"] = "production_resolve_join_and_replay".into();
    // 即使准备源帧失败，也实际 join 初始化线程，并显式收尾已返回的 writer。
    let resolved = encoder.resolve(None, &mut files, &mut selected, &state);
    let joined = matches!(
        &encoder,
        Encoder::Ready(_) | Encoder::PolicyPending { thread: None, .. }
    );
    report["startup_join_returned"] = joined.into();
    report["resolve_error"] = resolved
        .as_ref()
        .err()
        .map(|e| bounded(e.to_string()))
        .into();
    let calls = Cell::new(0);
    let finished = match encoder {
        Encoder::Ready(writer) => {
            report["stage"] = "production_session_tail".into();
            let result = stop::finalize_recording(
                body.and(resolved),
                spans,
                ObservedTail {
                    calls: &calls,
                    inner: SessionTail {
                        writer,
                        audio: None,
                        duration: frame_time(FRAMES),
                        state: state.clone(),
                        encoding,
                        requested_profile: encoding
                            .quality
                            .unwrap_or(QualityProfile::Current(options.preset)),
                        quality_request: requests.quality_request,
                        scale_plan: requests.scale_plan,
                        codec_request: requests.codec_request,
                        selection: selected,
                        output_plan: requests.output_plan,
                        writer_audio_requested: false,
                    },
                },
            );
            report["writer_owner_consumed_by_production_tail"] = true.into();
            result
        }
        _ => Err("策略交接没有返回可显式收尾的writer".into()),
    };
    report["explicit_finish_video_entries"] = calls.get().into();
    report["finalization_error"] = finished
        .as_ref()
        .err()
        .map(|e| bounded(e.to_string()))
        .into();
    report["stage"] = "inspect_production_session_report".into();
    let actual = state.encoding_report();
    report["session_report"] = serde_json::to_value(&actual)?;
    finished?;
    checkpoint(started)?;
    require(
        joined && calls.get() == 1 && spans == FRAMES as u64 && original_frames == FRAMES,
        "初始化join、显式收尾入口或真实源帧计数不完整",
    )?;
    let actual = actual.ok_or("生产SessionTail没有生成会话报告")?;
    let observed = actual.initial_video_output_observation();
    let expected_quality = serde_json::to_value(quality_plan(
        WIDTH,
        HEIGHT,
        FPS,
        QualityProfile::Current(options.preset),
    )?)?;
    require(
        actual.policy == EncoderPolicy::SoftwareOnly
            && actual.requested_scale == Some(scale)
            && actual.requested_codec == Some(CodecRequest::H264)
            && actual.selected_attempt == Some(0)
            && actual.codec_failures.as_ref().is_some_and(Vec::is_empty)
            && actual.selected_codec_attempt.is_some_and(|a| {
                a.slot == 0 && a.attempt_index == 0 && a.codec == VideoCodec::H264
            })
            && actual.writer_audio_requested == Some(false)
            && actual.input() == Some(VideoInput::CpuRgb32)
            && actual.video_encoder_kind() == EncoderKind::Software
            && actual.video_encoder_name().is_some()
            && actual.confirmed_video_codec() == Some(VideoCodec::H264)
            && actual.finalize_state() == encoding_report::StepState::Succeeded
            && observed.size == Some((WIDTH, HEIGHT))
            && observed
                .fps
                .is_some_and(|(n, d)| d > 0 && u64::from(n) == u64::from(FPS) * u64::from(d))
            && actual
                .finalized_writer
                .pointer("/encoder_attempt/source_coded_size")
                == Some(&json!([384, 256]))
            && actual.finalized_writer.pointer("/encoder_attempt/quality")
                == Some(&expected_quality),
        "实际CPU/软件MFT/精确输出类型/缩放源范围/参数/Finalize证据不完整",
    )?;
    report["actual_writer_gate"] = json!({"ok":true,"input":actual.input(),
        "encoder_kind":actual.video_encoder_kind(),"encoder_name":actual.video_encoder_name(),
        "codec":actual.confirmed_video_codec(),"initial_output_size":observed.size,
        "initial_output_fps":observed.fps,"finalize":actual.finalize_state()});
    let metadata = fs::symlink_metadata(&path)?;
    require(
        metadata.is_file()
            && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0
            && metadata.len() > 0
            && metadata.len() <= MAX_FILE_BYTES,
        "产物为空、属性不安全或超过4MiB",
    )?;
    report["file_bytes"] = metadata.len().into();
    report["stage"] = "compressed_samples".into();
    let payload = bitrate_check::inspect_cancellable(&path, &|| started.elapsed() > BUDGET)?;
    let payload_result = payload_gate(&payload);
    report["payload"] = payload;
    payload_result?;
    checkpoint(started)?;
    report["stage"] = "complete_decode".into();
    let decoded = decode(&path, started);
    let decode_ok = decoded["ok"] == true && decoded["reader_owner_release_returned"] == true;
    report["decoded"] = decoded;
    require(decode_ok, "完整回读或reader所有者释放未通过")?;
    checkpoint(started)?;
    drop(scene);
    drop(state);
    drop(files); // 不调用 cleanup / publish；成功和失败的自有实验产物均保留。
    drop(runtime);
    report["owners_released"] = true.into();
    checkpoint(started)?;
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    report["stage"] = "complete".into();
    Ok(())
}

#[test]
#[ignore = "独立exact缩放策略交接；自绘无采屏、摄像头、音频设备或窗口"]
fn recording_resize_session_native_policy_handoff() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut report = json!({"ok":false,"owners_released":false,"stage":"created",
        "scope":[
            "One dedicated exact process; fresh data/experiment environment precedes Options/MF initialization. No LiveCapture, Recording::start/run, FramePainter, camera, audio capture/output, GUI, clipboard or user media.",
            "Production resize preflight/start_policy_encoder/PolicyPending source queue/close_video_frame/Encoder::resolve/SessionTail/finalize_recording, with analytic synthetic annotation positions and frame-index gray checks. D3D video device is encoder setup only; this case requires actual SoftwareOnly CPU RGB32, not GPU texture input.",
            "The explicit count observes RecordingTail::finish_video entries, including failed entries; it is not a native Finalize call hook. Actual Finalize success is separately required from the original writer report.",
            "Does not exercise the live worker scheduler, actual capture/annotation input, RecordingResult construction, UI dispatch, retry injection or publication. Initial media-type observations are distinct from requested scale; complete decoded pixels are checked separately.",
            "45s cooperative checks between operations and compressed-sample reads cannot interrupt native calls/join/I/O. 4MiB file check and fixed input/output/report bounds are not a hard disk/process memory quota. C1 must provide an external process timeout. MF internal worker shutdown is not independently instrumented."]});
    let result = catch_unwind(AssertUnwindSafe(|| exercise(&fixture, &mut report)));
    let error = match result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(bounded(e.to_string())),
        Err(_) => Some("缩放策略专项捕获panic；资源完成未知".to_owned()),
    };
    let mut ok = error.is_none() && report["owners_released"] == true;
    report["error"] = error.into();
    report["ok"] = ok.into();
    let mut bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > MAX_REPORT_BYTES {
        ok = false;
        bytes = serde_json::to_vec_pretty(
            &json!({"ok":false,"error":"专项报告超过256KiB，未认定通过",
            "original_report_bytes":bytes.len(),"stage":report["stage"],"owners_released":report["owners_released"]}),
        )?;
    }
    let path = fixture.directory.join("report.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "缩放策略交接报告：{}",
        path.display()
    );
    assert!(
        ok && bytes.len() <= MAX_REPORT_BYTES && report["ok"] == true,
        "缩放策略交接失败；报告：{}",
        path.display()
    );
    Ok(())
}
