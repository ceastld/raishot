//! 不读桌面、不创建窗口的真实 Media Foundation 合成素材编解码对比。
//! 吞吐仅表示本机合成素材的处理速度，不代表实际录屏帧率或用户交互验收。
use crate::{
    media_foundation::{MfRuntime, Mp4Writer, VideoReader},
    Result,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FPS: u32 = 30;
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const COLOR_MEAN_LIMIT: f64 = 20.;
const COLOR_MAX_LIMIT: u8 = 90;
const TIME_TOLERANCE: Duration = Duration::from_millis(2);

#[derive(Clone, Copy)]
enum Mode {
    Legacy,
    Auto,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Auto => "auto",
        }
    }
}

#[derive(Clone, Copy)]
struct Case {
    name: &'static str,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    frames: u32,
    bitrate: u32,
}
impl Case {
    fn time(self, index: u32) -> Duration {
        Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(FPS))
    }
    fn frame_duration(self, index: u32) -> Duration {
        self.time(index + 1) - self.time(index)
    }
    fn description(self) -> Value {
        json!({
            "name": self.name,
            "encoded_size": [self.width, self.height],
            "source_size": [self.source_width, self.source_height],
            "fps": FPS,
            "frames": self.frames,
            "bitrate": self.bitrate,
            "expected_duration_ms": milliseconds(self.time(self.frames)),
        })
    }
}

/// 仅保留一份屏幕底图与一份工作帧；按确定性帧编号更新，两种模式素材完全相同。
struct Fixture {
    case: Case,
    base: Vec<u8>,
    frame: Vec<u8>,
}
impl Fixture {
    fn new(case: Case) -> Result<Self> {
        let bytes = case.source_width as usize * case.source_height as usize * 4;
        // 加上 reader 的单帧返回与 writer 的一帧裁剪，仍为严格有界的 CPU 素材预算。
        let cpu_frame_budget = bytes * 4;
        if cpu_frame_budget > MEMORY_LIMIT {
            return Err(format!("合成素材缓冲预算 {cpu_frame_budget} 超过 64 MiB").into());
        }
        let mut fixture = Self {
            case,
            base: vec![0; bytes],
            frame: vec![0; bytes],
        };
        let w = case.source_width;
        let h = case.source_height;
        for y in 0..h {
            for x in 0..w {
                let shade = if x % 96 == 0 || y % 40 == 0 {
                    75
                } else if (x / 96 + y / 40) % 2 == 0 {
                    38
                } else {
                    44
                };
                pixel(&mut fixture.base, w, x, y, [shade, shade, shade, 255]);
            }
        }
        fill_rect(&mut fixture.base, w, 0, 0, w, 100, [28, 28, 28, 255]);
        text(
            &mut fixture.base,
            w,
            96,
            22,
            "RUSTSHOT SCREEN RECORDING",
            3,
            [240, 240, 240, 255],
        );
        text(
            &mut fixture.base,
            w,
            96,
            60,
            "TEXT GRID COLOR AND MOVEMENT",
            2,
            [180, 180, 180, 255],
        );
        for y in (144..h.saturating_sub(112)).step_by(40) {
            text(
                &mut fixture.base,
                w,
                96,
                y,
                "WINDOW 0123456789  RGB  SCREEN TEXT",
                2,
                [220, 220, 220, 255],
            );
        }
        // 四角大色块独立检验上下方向、显示孔径和行跨度，不依赖文字识别。
        for (x, y, color) in [
            (8, 8, [30, 30, 190, 255]),
            (w - 80, 8, [35, 160, 40, 255]),
            (8, h - 80, [185, 45, 30, 255]),
            (w - 80, h - 80, [35, 190, 190, 255]),
        ] {
            fill_rect(&mut fixture.base, w, x, y, 72, 72, color);
        }
        text(
            &mut fixture.base,
            w,
            96,
            h - 48,
            "BOTTOM  TOP DOWN BGRA",
            2,
            [240, 240, 240, 255],
        );
        Ok(fixture)
    }

    fn generate(&mut self, index: u32) -> &[u8] {
        self.frame.copy_from_slice(&self.base);
        let w = self.case.source_width;
        let h = self.case.source_height;
        let x = 96 + (index * 13) % (w - 280);
        let y = h * 2 / 3;
        fill_rect(&mut self.frame, w, x, y, 144, 64, [160, 95, 35, 255]);
        text(
            &mut self.frame,
            w,
            x + 8,
            y + 22,
            &format!("FRAME {index:03}"),
            2,
            [245, 245, 245, 255],
        );
        &self.frame
    }
}

fn pixel(bytes: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 4]) {
    let index = ((u64::from(y) * u64::from(width) + u64::from(x)) * 4) as usize;
    if let Some(destination) = bytes.get_mut(index..index + 4) {
        destination.copy_from_slice(&color);
    }
}

fn fill_rect(
    bytes: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    rect_width: u32,
    rect_height: u32,
    color: [u8; 4],
) {
    let height = bytes.len() as u32 / (width * 4);
    for row in y..(y + rect_height).min(height) {
        for column in x..(x + rect_width).min(width) {
            pixel(bytes, width, column, row, color);
        }
    }
}

fn text(bytes: &mut [u8], width: u32, x: u32, y: u32, value: &str, scale: u32, color: [u8; 4]) {
    for (index, character) in value.chars().enumerate() {
        for (row, bits) in glyph(character).iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    fill_rect(
                        bytes,
                        width,
                        x + index as u32 * 6 * scale + column * scale,
                        y + row as u32 * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
    }
}

fn glyph(character: char) -> [u8; 7] {
    match character {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        _ => [0; 7],
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.
}

fn processing_fps(frames: u32, milliseconds: f64) -> Option<f64> {
    (milliseconds > 0.).then(|| f64::from(frames) * 1000. / milliseconds)
}

fn percentile(samples: &[f64], percentile: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) as f64 * percentile).round() as usize;
    Some(sorted[index])
}

fn near(actual: Duration, expected: Duration) -> bool {
    actual.abs_diff(expected) <= TIME_TOLERANCE
}

#[derive(Default)]
struct PixelComparison {
    samples: u64,
    error_sum: u64,
    max_error: u8,
    normal_corner_error: u64,
    flipped_corner_error: u64,
    corner_samples: u64,
    length_ok: bool,
}
impl PixelComparison {
    fn compare(&mut self, decoded: &[u8], expected: &[u8], case: Case) {
        let expected_length = case.width as usize * case.height as usize * 4;
        if decoded.len() != expected_length {
            self.length_ok = false;
            return;
        }
        // 含网格与文字的均匀稀疏采样；不会用宽高错位的 zip 隐藏显示孔径错误。
        for y in (16..case.height).step_by((case.height / 20).max(1) as usize) {
            for x in (16..case.width).step_by((case.width / 24).max(1) as usize) {
                for channel in 0..3 {
                    let actual = decoded[((y * case.width + x) * 4) as usize + channel];
                    let expected = expected[((y * case.source_width + x) * 4) as usize + channel];
                    let error = actual.abs_diff(expected);
                    self.samples += 1;
                    self.error_sum += u64::from(error);
                    self.max_error = self.max_error.max(error);
                }
            }
        }
        for (x, y) in [
            (40, 40),
            (case.width - 40, 40),
            (40, case.height - 40),
            (case.width - 40, case.height - 40),
        ] {
            for channel in 0..3 {
                let actual = decoded[((y * case.width + x) * 4) as usize + channel];
                let normal = expected[((y * case.source_width + x) * 4) as usize + channel];
                let flipped = expected
                    [(((case.height - 1 - y) * case.source_width + x) * 4) as usize + channel];
                self.normal_corner_error += u64::from(actual.abs_diff(normal));
                self.flipped_corner_error += u64::from(actual.abs_diff(flipped));
                self.corner_samples += 1;
            }
        }
    }
    fn mean(&self) -> Option<f64> {
        (self.samples > 0).then(|| self.error_sum as f64 / self.samples as f64)
    }
    fn report(&self) -> Value {
        let divisor = self.corner_samples.max(1) as f64;
        let normal = self.normal_corner_error as f64 / divisor;
        let flipped = self.flipped_corner_error as f64 / divisor;
        json!({
            "sparse_channel_samples": self.samples,
            "sparse_mean_abs_error": self.mean(),
            "sparse_max_abs_error": self.max_error,
            "corner_channel_samples": self.corner_samples,
            "normal_corner_mean_abs_error": normal,
            "vertically_flipped_corner_mean_abs_error": flipped,
            "thresholds": { "mean_abs_error": COLOR_MEAN_LIMIT, "max_abs_error": COLOR_MAX_LIMIT },
            "checks": {
                "buffer_length_exact": self.length_ok,
                "sparse_color": self.mean().is_some_and(|mean| mean <= COLOR_MEAN_LIMIT)
                    && self.max_error <= COLOR_MAX_LIMIT,
                "top_down_orientation": self.corner_samples > 0 && normal < 20. && flipped > normal + 25.,
            },
        })
    }
}

fn decode(path: &Path, mode: Mode, fixture: &mut Fixture) -> Value {
    let case = fixture.case;
    let start = Instant::now();
    let opened = match mode {
        Mode::Legacy => VideoReader::open_legacy_for_benchmark(path),
        Mode::Auto => VideoReader::open(path),
    };
    let open_ms = milliseconds(start.elapsed());
    let mut reader = match opened {
        Ok(reader) => reader,
        Err(error) => {
            return json!({"mode": mode.name(), "open_ms": open_ms, "error": error.to_string()});
        }
    };
    let mut frame_times = Vec::new();
    let mut read_ms = Vec::new();
    let mut generation_ms = 0.;
    let mut comparison = PixelComparison {
        length_ok: true,
        ..PixelComparison::default()
    };
    let mut error = None;
    let mut eof = false;
    let mut frames = 0_u32;
    // 超出预计帧数后仍允许少量错误帧写入报告，但不会在坏文件上无限循环。
    while frames <= case.frames + 8 {
        let start = Instant::now();
        let result = reader.next();
        read_ms.push(milliseconds(start.elapsed()));
        let decoded = match result {
            Ok(Some(decoded)) => decoded,
            Ok(None) => {
                eof = true;
                break;
            }
            Err(failure) => {
                error = Some(failure.to_string());
                break;
            }
        };
        frame_times.push(decoded.time);
        let start = Instant::now();
        let expected = fixture.generate(frames);
        generation_ms += milliseconds(start.elapsed());
        comparison.compare(&decoded.bgra, expected, case);
        frames += 1;
    }
    let total_ms: f64 = read_ms.iter().sum();
    let pixels = comparison.report();
    let checks = json!({
        "dimensions_exact": ([reader.width, reader.height] == [case.width, case.height]),
        "frame_count_exact": frames == case.frames,
        "end_of_stream": eof,
        "first_timestamp": frame_times.first().is_some_and(|time| near(*time, Duration::ZERO)),
        "last_timestamp": frame_times.last().is_some_and(|time| near(*time, case.time(case.frames - 1))),
        "all_timestamps": frame_times.iter().enumerate().all(|(index, time)| near(*time, case.time(index as u32))),
        "strictly_increasing_timestamps": frame_times.windows(2).all(|pair| pair[1] > pair[0]),
        "container_duration": near(reader.duration, case.time(case.frames)),
    });
    json!({
        "mode": mode.name(),
        "open_ms": open_ms,
        "next_call_ms": read_ms,
        "next_total_ms": total_ms,
        "next_p50_ms": percentile(&read_ms, 0.5),
        "next_p95_ms": percentile(&read_ms, 0.95),
        "continuous_decode_processing_fps": processing_fps(frames, total_ms),
        "generation_excluded_ms": generation_ms,
        "dimensions": [reader.width, reader.height],
        "frames": frames,
        "container_duration_ms": milliseconds(reader.duration),
        "timestamps_100ns": frame_times.iter().map(|time| time.as_nanos() / 100).collect::<Vec<_>>(),
        "checks": checks,
        "pixels": pixels,
        "diagnostics": reader.diagnostics(),
        "error": error,
    })
}

fn encode(directory: &Path, mode: Mode, trial: usize, fixture: &mut Fixture) -> Value {
    let case = fixture.case;
    let path = directory.join(format!("{}-{trial}-{}.mp4", case.name, mode.name()));
    let start = Instant::now();
    let created = match mode {
        Mode::Legacy => Mp4Writer::new_legacy_for_benchmark(
            &path,
            case.width,
            case.height,
            FPS,
            case.bitrate,
            false,
        ),
        Mode::Auto => Mp4Writer::new(&path, case.width, case.height, FPS, case.bitrate, false),
    };
    let init_ms = milliseconds(start.elapsed());
    let mut writer = match created {
        Ok(writer) => writer,
        Err(error) => {
            return json!({
                "mode": mode.name(), "trial": trial, "file": path,
                "init_ms": init_ms, "error": error.to_string(), "decoder_results": [],
            });
        }
    };
    let mut write_ms = Vec::new();
    let mut generation_ms = 0.;
    let mut error = None;
    let mut submitted_frames = 0_u32;
    for index in 0..case.frames {
        let start = Instant::now();
        let frame = fixture.generate(index);
        generation_ms += milliseconds(start.elapsed());
        let start = Instant::now();
        let result = writer.write_video(
            frame,
            case.source_width,
            case.time(index),
            case.frame_duration(index),
        );
        write_ms.push(milliseconds(start.elapsed()));
        if let Err(failure) = result {
            error = Some(failure.to_string());
            break;
        }
        submitted_frames += 1;
    }
    let diagnostics = writer.diagnostics().clone();
    let start = Instant::now();
    let finished = writer.finish();
    let finish_ms = milliseconds(start.elapsed());
    let finish_error = finished.err().map(|failure| failure.to_string());
    let write_total_ms: f64 = write_ms.iter().sum();
    let mut decoder_results = Vec::new();
    if error.is_none() && finish_error.is_none() {
        // AB / BA 解码顺序与编码顺序交替，保留每一份成品的原始结果。
        let modes = if trial.is_multiple_of(2) {
            [Mode::Legacy, Mode::Auto]
        } else {
            [Mode::Auto, Mode::Legacy]
        };
        for decoder in modes {
            decoder_results.push(decode(&path, decoder, fixture));
        }
    }
    json!({
        "mode": mode.name(),
        "trial": trial,
        "file": path,
        "file_bytes": std::fs::metadata(&path).ok().map(|metadata| metadata.len()),
        "init_ms": init_ms,
        "generation_excluded_ms": generation_ms,
        "write_call_ms": write_ms,
        "submitted_frames": submitted_frames,
        "write_total_ms": write_total_ms,
        "write_p50_ms": percentile(&write_ms, 0.5),
        "write_p95_ms": percentile(&write_ms, 0.95),
        "finish_ms": finish_ms,
        "write_and_finalize_processing_fps": processing_fps(submitted_frames, write_total_ms + finish_ms),
        "init_write_and_finalize_processing_fps": processing_fps(submitted_frames, init_ms + write_total_ms + finish_ms),
        "diagnostics": diagnostics,
        "error": error,
        "finish_error": finish_error,
        "decoder_results": decoder_results,
    })
}

fn unique_directory() -> Result<PathBuf> {
    let root = PathBuf::from("target/qa");
    std::fs::create_dir_all(&root)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for suffix in 0..100 {
        let path = root.join(format!(
            "recording-codec-benchmark-{timestamp}-{}-{suffix}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法创建唯一编解码测量目录".into())
}

/// 只使用合成画面。失败原样保留在 report.json，完整测量后由每项实际检查决定返回值。
pub fn run() -> Result<()> {
    let directory = unique_directory()?;
    let cases = [
        Case {
            name: "1080p",
            width: 1920,
            height: 1080,
            source_width: 1920,
            source_height: 1080,
            frames: 90,
            bitrate: 12_000_000,
        },
        Case {
            name: "odd-source-non16",
            width: 796,
            height: 530,
            source_width: 797,
            source_height: 531,
            frames: 15,
            bitrate: 8_000_000,
        },
    ];
    let mut report = json!({
        "test": "recording_codec_synthetic_media_foundation",
        "scope": "本机合成素材实际 MP4 编码、Finalize 与连续解码；不等于真实录屏 FPS，不含桌面采集、GUI、音频或交互验收",
        "desktop_read": false,
        "window_created": false,
        "global_input_injected": false,
        "cpu_synthetic_frame_budget_bytes": 1920 * 1080 * 4 * 4,
        "cpu_synthetic_frame_budget_limit_bytes": MEMORY_LIMIT,
        "retained_source_frames": 2,
        "note": "底图预生成，帧编号更新完全确定；不保存全部90帧；生成和像素验证均排除处理计时；codec/driver内部内存不在素材预算内",
        "encoder_order": ["legacy", "auto", "auto", "legacy"],
        "cases": [],
    });
    let report_path = directory.join("report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    let runtime = MfRuntime::new();
    if let Err(error) = &runtime {
        report["runtime_error"] = json!(error.to_string());
        std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
        return Err(format!(
            "MF 初始化失败，原始报告：{}：{error}",
            report_path.display()
        )
        .into());
    }
    let _runtime = runtime?;
    let mut output = Vec::new();
    for case in cases {
        let mut fixture = Fixture::new(case)?;
        let mut trials = Vec::new();
        for (trial, mode) in [Mode::Legacy, Mode::Auto, Mode::Auto, Mode::Legacy]
            .into_iter()
            .enumerate()
        {
            trials.push(encode(&directory, mode, trial, &mut fixture));
            report["cases"] = json!(&output);
            report["in_progress"] = json!({"case": case.description(), "trials": &trials});
            std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
        }
        output.push(json!({"case": case.description(), "trials": trials}));
    }
    report["cases"] = json!(output);
    report.as_object_mut().unwrap().remove("in_progress");
    let failures = validation_failures(&report);
    report["validation_failures"] = json!(&failures);
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        json!({"test": "recording_codec_benchmark_report", "report": report_path, "validation_failures": failures})
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "合成编解码有 {} 项失败，原始报告：{}",
            failures.len(),
            report_path.display()
        )
        .into())
    }
}

fn validation_failures(report: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for case in report["cases"].as_array().into_iter().flatten() {
        for trial in case["trials"].as_array().into_iter().flatten() {
            let label = format!(
                "{} / {} / {}",
                case["case"]["name"].as_str().unwrap_or("?"),
                trial["mode"].as_str().unwrap_or("?"),
                trial["trial"]
            );
            for key in ["error", "finish_error"] {
                if let Some(error) = trial[key].as_str() {
                    failures.push(format!("{label} / {key}: {error}"));
                }
            }
            if trial["decoder_results"]
                .as_array()
                .is_none_or(|decoders| decoders.len() != 2)
            {
                failures.push(format!("{label} / 未完成两种解码路径"));
            }
            for decoded in trial["decoder_results"].as_array().into_iter().flatten() {
                let label = format!(
                    "{label} / decode {}",
                    decoded["mode"].as_str().unwrap_or("?")
                );
                if let Some(error) = decoded["error"].as_str() {
                    failures.push(format!("{label}: {error}"));
                }
                for checks in [&decoded["checks"], &decoded["pixels"]["checks"]] {
                    if let Some(checks) = checks.as_object() {
                        for (name, value) in checks {
                            if value != true {
                                failures.push(format!("{label} / {name}"));
                            }
                        }
                    } else {
                        failures.push(format!("{label} / 检查结果缺失"));
                    }
                }
            }
        }
    }
    failures
}
