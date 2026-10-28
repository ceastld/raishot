//! 自有色块的真实 MF 往返检查；Auto 与 CPU 像素输入都不等同于编码器硬软。
//! 不读桌面、不开窗口、不播声音，不修改用户配置；只在调用者指定目录内创建自有样本。
use crate::{
    geometry::ScreenRect,
    media_foundation::{EncoderBackendSession, MfRuntime, Mp4Writer, VideoReader},
    recording_encoder_policy::{EncoderPlan, EncoderPolicy},
    recording_options::{quality_plan, Preset, QualityProfile},
    recording_size_layout::RecordingSizeLayout,
    Result,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RGB: [[u8; 3]; 16] = [
    [0, 0, 0],
    [16, 16, 16],
    [32, 32, 32],
    [64, 64, 64],
    [128, 128, 128],
    [192, 192, 192],
    [235, 235, 235],
    [255, 255, 255],
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [32, 96, 192],
    [192, 96, 32],
];
const FPS: u32 = 30;
const FRAMES: u32 = 15;
const MAX_MEAN_CHANNEL_ERROR: f64 = 12.;
const CASE_BUDGET: Duration = Duration::from_secs(30);
const MANDATORY_BASELINES: [&str; 3] = ["auto-even", "auto-odd", "cpu-odd"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PixelRoute {
    Auto,
    LegacyCpu,
    GpuNv12,
}

pub(crate) struct ColorChart {
    layout: RecordingSizeLayout,
    pixels: Vec<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PatchComparison {
    pub expected_rgb: [u8; 3],
    pub observed_rgb: [f64; 3],
    pub max_mean_channel_error: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ColorComparison {
    pub patches: Vec<PatchComparison>,
    pub gray_order_preserved: bool,
    pub black_max: f64,
    pub white_min: f64,
    pub max_mean_channel_error: f64,
    pub tolerance: f64,
    pub passed: bool,
}

impl ColorChart {
    pub(crate) fn new(layout: RecordingSizeLayout) -> Result<Self> {
        let verified = RecordingSizeLayout::new(ScreenRect {
            left: layout.left,
            top: layout.top,
            width: layout.source_width,
            height: layout.source_height,
        })?;
        if layout != verified {
            return Err("色块样本布局与源区域不一致".into());
        }
        if layout.encoded_width < 128
            || layout.encoded_height < 128
            || u64::from(layout.source_width) * u64::from(layout.source_height) > 4_000_000
        {
            return Err("色块样本尺寸须至少 128×128 且不超过四百万像素".into());
        }
        let mut pixels = vec![0; layout.source_bytes];
        for y in 0..layout.source_height {
            for x in 0..layout.source_width {
                let [r, g, b] = if x >= layout.encoded_width || y >= layout.encoded_height {
                    // 只在应裁掉的右列/底行放高对比哨兵，不移动有效图的原点。
                    [255, 128, 0]
                } else {
                    RGB[((y * 4 / layout.encoded_height) * 4 + x * 4 / layout.encoded_width)
                        as usize]
                };
                let at = y as usize * layout.source_stride + x as usize * 4;
                pixels[at..at + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        Ok(Self { layout, pixels })
    }

    pub(crate) fn bgra(&self) -> &[u8] {
        &self.pixels
    }

    /// 只量色块内部，避开有损编码和 4:2:0 边界混色；不把单通道均值误称为无损画质。
    pub(crate) fn compare(
        &self,
        decoded: &[u8],
        width: u32,
        height: u32,
        tolerance: f64,
    ) -> Result<ColorComparison> {
        if !tolerance.is_finite() || tolerance < 0. {
            return Err("色块误差阈值无效".into());
        }
        if (width, height) != (self.layout.encoded_width, self.layout.encoded_height)
            || decoded.len() != self.layout.encoded_bytes
        {
            return Err("色块回读尺寸或像素长度与编码范围不一致".into());
        }
        let mut patches = Vec::with_capacity(RGB.len());
        for (index, &expected_rgb) in RGB.iter().enumerate() {
            let col = index as u32 % 4;
            let row = index as u32 / 4;
            let (left, right) = (
                col * width / 4 + width / 32,
                (col + 1) * width / 4 - width / 32,
            );
            let (top, bottom) = (
                row * height / 4 + height / 32,
                (row + 1) * height / 4 - height / 32,
            );
            let mut sum = [0_u64; 3];
            for y in top..bottom {
                for x in left..right {
                    let at = y as usize * self.layout.encoded_stride + x as usize * 4;
                    for (channel, offset) in [2, 1, 0].into_iter().enumerate() {
                        sum[channel] += u64::from(decoded[at + offset]);
                    }
                }
            }
            let count = f64::from((right - left) * (bottom - top));
            let observed_rgb = sum.map(|value| value as f64 / count);
            let max_mean_channel_error = observed_rgb
                .iter()
                .zip(expected_rgb)
                .map(|(actual, expected)| (actual - f64::from(expected)).abs())
                .fold(0., f64::max);
            patches.push(PatchComparison {
                expected_rgb,
                observed_rgb,
                max_mean_channel_error,
            });
        }
        let max_mean_channel_error = patches
            .iter()
            .map(|patch| patch.max_mean_channel_error)
            .fold(0., f64::max);
        let gray_order_preserved = patches[..8].windows(2).all(|pair| {
            pair[1].observed_rgb.iter().sum::<f64>() > pair[0].observed_rgb.iter().sum::<f64>()
        });
        Ok(ColorComparison {
            black_max: patches[0].observed_rgb.into_iter().fold(0., f64::max),
            white_min: patches[7].observed_rgb.into_iter().fold(255., f64::min),
            patches,
            gray_order_preserved,
            max_mean_channel_error,
            tolerance,
            passed: gray_order_preserved && max_mean_channel_error <= tolerance,
        })
    }
}

fn create_output(parent: &Path) -> Result<PathBuf> {
    fs::create_dir_all(parent)?;
    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let directory = parent.join(format!("color-check-{}-{time}-{index}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("无法分配自有色块检查目录".into())
}

fn check_budget(started: Instant) -> Result<()> {
    // 检查位于系统调用之间，不能宣称能中断一次卡住的驱动调用。
    if started.elapsed() > CASE_BUDGET {
        Err("色块样本处理超过调用间预算".into())
    } else {
        Ok(())
    }
}

fn check_case(directory: &Path, name: &str, width: u32, height: u32, route: PixelRoute) -> Value {
    let mut report = json!({"name": name, "source_size": [width, height],
        "cpu_input_requested": route == PixelRoute::LegacyCpu, "requested_route": route,
        "mandatory_baseline": route != PixelRoute::GpuNv12});
    let result = (|| -> Result<()> {
        let started = Instant::now();
        let layout = RecordingSizeLayout::new(ScreenRect {
            left: 0,
            top: 0,
            width,
            height,
        })?;
        let chart = ColorChart::new(layout)?;
        layout.validate_frame(width, height, chart.bgra().len())?;
        report["layout"] = serde_json::to_value(layout)?;
        let path = directory.join(format!("{name}.mp4"));
        let mut writer = match route {
            PixelRoute::LegacyCpu => Mp4Writer::new_legacy_for_benchmark(
                &path,
                layout.encoded_width,
                layout.encoded_height,
                FPS,
                4_000_000,
                false,
            )?,
            PixelRoute::Auto => Mp4Writer::new(
                &path,
                layout.encoded_width,
                layout.encoded_height,
                FPS,
                4_000_000,
                false,
            )?,
            PixelRoute::GpuNv12 => {
                let mut quality = quality_plan(
                    width,
                    height,
                    FPS,
                    QualityProfile::Current(Preset::CompactMp4),
                )?;
                // 与三项基线使用相同请求码率，其余 CurrentCompact 参数保持原值。
                quality.bitrate_bps = 4_000_000;
                report["requested_plan"] = serde_json::to_value(quality)?;
                report["attempt_index"] = 0.into();
                let session = EncoderBackendSession::new(
                    EncoderPlan::new(EncoderPolicy::Auto, quality),
                    false,
                );
                // 只调用局部尝试 0（GpuNv12）；不执行包含 CPU 回退的 Auto 构造入口。
                match session.create_attempt(&path, 0, None) {
                    Ok(writer) => writer,
                    Err(error) => {
                        report["creation_error"] = json!(&error);
                        return Err(Box::new(error));
                    }
                }
            }
        };
        let gpu_pixel_input = writer.uses_gpu_input();
        report["gpu_pixel_input"] = gpu_pixel_input.into();
        let writing = (|| -> Result<()> {
            if route == PixelRoute::GpuNv12 && !gpu_pixel_input {
                return Err("显式 NV12 样本未使用 GPU 像素输入，不能作为 GPU 路线验证".into());
            }
            for index in 0..FRAMES {
                check_budget(started)?;
                let time = Duration::from_nanos(u64::from(index) * 1_000_000_000 / u64::from(FPS));
                let end =
                    Duration::from_nanos(u64::from(index + 1) * 1_000_000_000 / u64::from(FPS));
                writer.write_video(chart.bgra(), layout.source_width, time, end - time)?;
            }
            Ok(())
        })();
        let (finished, diagnostics) = writer.finish_with_diagnostics();
        report["encoder"] = diagnostics;
        report["write_error"] = writing.as_ref().err().map(ToString::to_string).into();
        report["finalize_error"] = finished.as_ref().err().map(ToString::to_string).into();
        writing?;
        finished?;
        check_budget(started)?;
        let mut reader = VideoReader::open(&path)?;
        let mut count = 0;
        let mut worst: Option<ColorComparison> = None;
        while let Some(frame) = reader.next()? {
            check_budget(started)?;
            count += 1;
            if count > FRAMES + 8 {
                return Err("色块回读帧数超过上限".into());
            }
            let comparison = chart.compare(
                &frame.bgra,
                reader.width,
                reader.height,
                MAX_MEAN_CHANNEL_ERROR,
            )?;
            let replace = worst.as_ref().is_none_or(|old| {
                (!comparison.passed && old.passed)
                    || (comparison.passed == old.passed
                        && comparison.max_mean_channel_error > old.max_mean_channel_error)
            });
            if replace {
                worst = Some(comparison);
            }
        }
        report["decoded_frames"] = count.into();
        report["decoder"] = reader.diagnostics();
        report["worst_frame"] = serde_json::to_value(&worst)?;
        if count != FRAMES || !worst.is_some_and(|comparison| comparison.passed) {
            return Err("色块回读的帧数、灰阶顺序或通道误差未通过".into());
        }
        Ok(())
    })();
    report["ok"] = result.is_ok().into();
    report["error"] = result.err().map(|error| error.to_string()).into();
    report
}

fn mandatory_baseline_ok(cases: &[Value]) -> bool {
    MANDATORY_BASELINES.iter().all(|name| {
        cases
            .iter()
            .find(|case| case["name"].as_str() == Some(*name))
            .is_some_and(|case| case["ok"] == true)
    })
}

fn route_coverage(cases: &[Value]) -> Value {
    let gpu_route = json!(PixelRoute::GpuNv12);
    let gpu_case = cases
        .iter()
        .find(|case| case["requested_route"] == gpu_route);
    let gpu_verified =
        gpu_case.is_some_and(|case| case["ok"] == true && case["gpu_pixel_input"] == true);
    let gpu_unverified_reason = if gpu_verified {
        None
    } else {
        Some(match gpu_case {
            None => "The explicit GPU NV12 case was not run".to_string(),
            Some(case) if case["ok"] != true => case["error"].as_str().map_or_else(
                || {
                    "The explicit GPU NV12 case did not complete its full color roundtrip"
                        .to_string()
                },
                |error| format!("The explicit GPU NV12 case was not verified: {error}"),
            ),
            Some(_) => "The explicit NV12 case did not confirm actual GPU pixel input".to_string(),
        })
    };
    let cpu_route = json!(PixelRoute::LegacyCpu);
    let cpu_verified = cases.iter().any(|case| {
        case["requested_route"] == cpu_route
            && case["ok"] == true
            && case["gpu_pixel_input"] == false
    });
    json!({"gpu_nv12_verified": gpu_verified, "cpu_rgb32_verified": cpu_verified,
        "gpu_nv12_unverified_reason": gpu_unverified_reason,
        "scope": "explicit-route complete synthetic color roundtrips only; GPU pixel input does not establish a hardware MFT"})
}

/// 只验证这组有界自有样本；不能据此证明实际桌面、HDR、多屏或硬件编码已验收。
pub(crate) fn run(directory: &Path) -> Result<Value> {
    let output = create_output(directory)?;
    let mut report = match MfRuntime::new() {
        Ok(_runtime) => {
            let cases = [
                check_case(&output, "auto-even", 384, 256, PixelRoute::Auto),
                check_case(&output, "auto-odd", 385, 257, PixelRoute::Auto),
                check_case(&output, "cpu-odd", 385, 257, PixelRoute::LegacyCpu),
                check_case(&output, "gpu-nv12-odd", 385, 257, PixelRoute::GpuNv12),
            ];
            json!({"cases": cases})
        }
        Err(error) => json!({"error": error.to_string(), "cases": []}),
    };
    let (baseline_ok, coverage) = {
        let cases = report["cases"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        (mandatory_baseline_ok(cases), route_coverage(cases))
    };
    report["ok"] = baseline_ok.into();
    report["mandatory_baseline_ok"] = baseline_ok.into();
    report["mandatory_baseline_cases"] = json!(MANDATORY_BASELINES);
    report["route_coverage"] = coverage;
    report["scope"] = "ok summarizes only the three mandatory Auto-even/Auto-odd/legacy-CPU baselines; forced GPU NV12 is reported separately and may be unavailable on this machine; this checks synthetic BGRA upload/conversion, not live texture capture, multiple adapters, text, HDR or hardware encoder availability".into();
    report["output"] = output.to_string_lossy().to_string().into();
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

    fn chart() -> ColorChart {
        ColorChart::new(
            RecordingSizeLayout::new(ScreenRect {
                left: -384,
                top: -256,
                width: 385,
                height: 257,
            })
            .unwrap(),
        )
        .unwrap()
    }

    fn cropped(chart: &ColorChart) -> Vec<u8> {
        (0..chart.layout.encoded_height)
            .flat_map(|row| {
                chart.bgra()[chart.layout.source_row(row).unwrap()]
                    .iter()
                    .copied()
            })
            .collect()
    }

    #[test]
    fn recording_color_check_identity_crops_sentinel_without_shifting() {
        let chart = chart();
        let decoded = cropped(&chart);
        let result = chart.compare(&decoded, 384, 256, 0.).unwrap();
        assert!(result.passed && result.gray_order_preserved);
        assert_eq!(
            (
                result.black_max,
                result.white_min,
                result.max_mean_channel_error
            ),
            (0., 255., 0.)
        );
        assert_eq!(&chart.bgra()[384 * 4..385 * 4], &[0, 128, 255, 255]);
        assert!(chart.compare(chart.bgra(), 385, 257, 12.).is_err());
        assert!(chart
            .compare(&decoded[..decoded.len() - 1], 384, 256, 12.)
            .is_err());
    }

    #[test]
    fn recording_color_check_detects_lifted_black_and_swapped_channels() {
        let chart = chart();
        let mut lifted = cropped(&chart);
        for pixel in lifted.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel = channel.saturating_add(20);
            }
        }
        let lifted = chart.compare(&lifted, 384, 256, 12.).unwrap();
        assert!(!lifted.passed && lifted.black_max >= 20.);
        let mut swapped = cropped(&chart);
        for pixel in swapped.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        assert!(!chart.compare(&swapped, 384, 256, 12.).unwrap().passed);
        assert!(chart.compare(&swapped, 384, 256, f64::NAN).is_err());
        let mut invalid = chart.layout;
        invalid.source_bytes = 1;
        assert!(ColorChart::new(invalid).is_err());
    }

    #[test]
    fn recording_color_check_route_coverage_requires_explicit_complete_cases() {
        let mut cases = vec![
            json!({"name": "auto-even", "requested_route": PixelRoute::Auto,
                "ok": true, "gpu_pixel_input": true}),
            json!({"name": "auto-odd", "requested_route": PixelRoute::Auto,
                "ok": true, "gpu_pixel_input": false}),
            json!({"name": "cpu-odd", "requested_route": PixelRoute::LegacyCpu,
                "ok": true, "gpu_pixel_input": false}),
        ];
        assert!(mandatory_baseline_ok(&cases));
        assert!(!mandatory_baseline_ok(&cases[..2]));
        let coverage = route_coverage(&cases);
        assert_eq!(coverage["gpu_nv12_verified"], false);
        assert_eq!(coverage["cpu_rgb32_verified"], true);
        assert!(coverage["gpu_nv12_unverified_reason"].is_string());

        cases.push(
            json!({"name": "gpu-nv12-odd", "requested_route": PixelRoute::GpuNv12,
            "ok": false, "gpu_pixel_input": true, "error": "decode did not complete"}),
        );
        assert!(mandatory_baseline_ok(&cases));
        let coverage = route_coverage(&cases);
        assert_eq!(coverage["gpu_nv12_verified"], false);
        assert!(coverage["gpu_nv12_unverified_reason"]
            .as_str()
            .unwrap()
            .contains("decode did not complete"));

        cases[3]["ok"] = true.into();
        let coverage = route_coverage(&cases);
        assert_eq!(coverage["gpu_nv12_verified"], true);
        assert!(coverage["gpu_nv12_unverified_reason"].is_null());
        cases[3]["gpu_pixel_input"] = false.into();
        assert_eq!(route_coverage(&cases)["gpu_nv12_verified"], false);
        cases[2]["ok"] = false.into();
        assert!(!mandatory_baseline_ok(&cases));
        assert_eq!(route_coverage(&cases)["cpu_rgb32_verified"], false);
    }

    #[test]
    #[ignore = "需要系统 MF 编码器；只生成自有色块 MP4，不打开窗口"]
    fn recording_color_check_native_roundtrip() {
        let report = run(Path::new("target/recording-color-check")).unwrap();
        assert_eq!(
            report["ok"], true,
            "色块回读失败，报告目录：{}",
            report["output"]
        );
    }
}

#[cfg(test)]
mod text_edges {
    use super::*;
    use crate::{
        recording::bitrate_check, recording_encoder_caps::VideoInput,
        recording_options::QualityPlan, recording_quality_lab::profile_samples as samples,
    };
    use std::sync::atomic::AtomicBool;
    use windows::Win32::Media::MediaFoundation::MFVideoFormat_H264;

    const FRAME_COUNT: u32 = 6;
    const MAX_PIXELS: u64 = 4_000_000;
    const MAX_VIDEO_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_REPORT_BYTES: usize = 256 * 1024;
    const RUN_BUDGET: Duration = Duration::from_secs(90);
    const TIME_TOLERANCE: Duration = Duration::from_millis(1);
    // 项目合成黑白样本的验收阈值，不是系统编解码器的能力或感知画质标准。
    // 0/255 输入允许各端均值偏离 64；有向相邻边缘至少保留 96/255 的亮度差。
    const BLACK_MAX: f64 = 64.;
    const WHITE_MIN: f64 = 192.;
    const EDGE_CONTRAST_MIN: f64 = 96.;
    const PIXEL_AGREEMENT_MIN: f64 = 0.98;
    const EDGE_AGREEMENT_MIN: f64 = 0.95;

    fn ticks(index: u32) -> u64 {
        (u64::from(index) * 10_000_000 + u64::from(FPS / 2)) / u64::from(FPS)
    }

    fn time(index: u32) -> Duration {
        Duration::from_nanos(ticks(index) * 100)
    }

    fn short(value: &str) -> String {
        value.chars().take(256).collect()
    }

    fn checkpoint(case_started: Instant, run_started: Instant) -> Result<()> {
        check_budget(case_started)?;
        if run_started.elapsed() > RUN_BUDGET {
            return Err("文字边缘检查超过总调用间预算".into());
        }
        Ok(())
    }

    fn observe_file(path: &Path) -> Result<u64> {
        let bytes = fs::metadata(path)?.len();
        if bytes > MAX_VIDEO_BYTES {
            return Err("文字边缘短片超过文件观察预算".into());
        }
        Ok(bytes)
    }

    fn fixture() -> Result<Vec<u8>> {
        let pixels = u64::from(samples::width()) * u64::from(samples::height());
        if samples::width() < 960 || samples::height() < 540 || pixels > MAX_PIXELS {
            return Err("文字边缘样本尺寸不符合有界 ROI 合同".into());
        }
        let mut frame = samples::sample(0)?;
        if frame.len() != pixels as usize * 4 {
            return Err("文字边缘样本 BGRA 长度不符".into());
        }
        // 复用既有自绘 ASCII/细线几何，阈值化为无色度的 0/255 黑白源。
        for pixel in frame.chunks_exact_mut(4) {
            let level = (u32::from(pixel[0]) + u32::from(pixel[1]) + u32::from(pixel[2])) / 3;
            let gray = if level >= 128 { 255 } else { 0 };
            pixel.copy_from_slice(&[gray, gray, gray, 255]);
        }
        Ok(frame)
    }

    fn compare(source: &[u8], decoded: &[u8]) -> Result<Value> {
        let width = samples::width() as usize;
        let height = samples::height() as usize;
        if source.len() != width * height * 4 || decoded.len() != source.len() {
            return Err("文字边缘回读像素长度不符".into());
        }
        let middle = width / 2;
        let regions = [
            ("ascii_text", 8, 5, middle - 32, 90),
            (
                "horizontal_one_pixel_lines",
                middle + 8,
                104,
                width / 4 - 48,
                56,
            ),
            ("vertical_one_pixel_lines", width * 3 / 4 - 4, 112, 88, 112),
        ];
        let level = |pixels: &[u8], x: usize, y: usize| {
            let at = (y * width + x) * 4;
            (u32::from(pixels[at]) + u32::from(pixels[at + 1]) + u32::from(pixels[at + 2])) as f64
                / 3.
        };
        let mut rows = Vec::with_capacity(regions.len());
        for (name, left, top, w, h) in regions {
            if left + w > width || top + h > height {
                return Err("文字边缘 ROI 超出样本".into());
            }
            let mut counts = [0_u64; 2];
            let mut sum = [0.; 2];
            let mut correct = [0_u64; 2];
            let mut edges = [0_u64; 2];
            let mut matching_edges = [0_u64; 2];
            let mut contrast_sum = [0.; 2];
            for y in top..top + h {
                for x in left..left + w {
                    let expected = level(source, x, y);
                    let observed = level(decoded, x, y);
                    let class = usize::from(expected >= 128.);
                    counts[class] += 1;
                    sum[class] += observed;
                    correct[class] += u64::from((observed >= 128.) == (class == 1));
                    for (axis, nx, ny) in [(0, x + 1, y), (1, x, y + 1)] {
                        if nx >= left + w || ny >= top + h {
                            continue;
                        }
                        let next = level(source, nx, ny);
                        if next == expected {
                            continue;
                        }
                        // 固定在参考边缘的同一对像素，含明暗方向；平移、翻转不重新对齐放行。
                        let signed =
                            (level(decoded, nx, ny) - observed) * (next - expected).signum();
                        edges[axis] += 1;
                        contrast_sum[axis] += signed;
                        matching_edges[axis] += u64::from(signed >= EDGE_CONTRAST_MIN);
                    }
                }
            }
            let means = std::array::from_fn::<_, 2, _>(|i| {
                (counts[i] != 0).then(|| sum[i] / counts[i] as f64)
            });
            let pixel_agreement = std::array::from_fn::<_, 2, _>(|i| {
                (counts[i] != 0).then(|| correct[i] as f64 / counts[i] as f64)
            });
            let edge_agreement = std::array::from_fn::<_, 2, _>(|i| {
                (edges[i] != 0).then(|| matching_edges[i] as f64 / edges[i] as f64)
            });
            let passed = means[0].is_some_and(|v| v <= BLACK_MAX)
                && means[1].is_some_and(|v| v >= WHITE_MIN)
                && pixel_agreement
                    .iter()
                    .all(|v| v.is_some_and(|v| v >= PIXEL_AGREEMENT_MIN))
                && edges.iter().sum::<u64>() > 0
                && edge_agreement
                    .iter()
                    .flatten()
                    .all(|v| *v >= EDGE_AGREEMENT_MIN);
            rows.push(json!({"region":name,"rect":[left,top,w,h],"passed":passed,
                "black_white_pixels":counts,"black_white_mean":means,
                "black_white_classification_agreement":pixel_agreement,
                "x_y_reference_edges":edges,"x_y_position_direction_agreement":edge_agreement,
                "x_y_mean_signed_contrast":std::array::from_fn::<_,2,_>(|i|
                    (edges[i] != 0).then(|| contrast_sum[i] / edges[i] as f64))}));
        }
        Ok(json!({"passed":rows.iter().all(|row| row["passed"] == true),"regions":rows}))
    }

    fn writer_evidence(writer: &Value) -> Value {
        let video = &writer["encoding_trial"]["inspection"]["video"];
        let text = |value: &Value| value.as_str().map(short);
        let types = |value: &Value| {
            value.as_array().map(|types| {
                types.iter().take(16).map(|media| json!({
                "stream_id":media["stream_id"],"major_type":text(&media["major_type"]),
                "subtype":text(&media["subtype"]),"size":media["size"],"fps":media["fps"]
            })).collect::<Vec<_>>()
            })
        };
        json!({"input":text(&writer["input"]),
            "observed_video_codec_matches_request":writer["observed_video_codec_matches_request"],
            "actual_encoder":video["encoder"].as_object().map(|_| json!({
                "clsid":text(&video["encoder"]["clsid"]),"name":text(&video["encoder"]["name"]),
                "kind":text(&video["encoder"]["kind"])})),
            "actual_mft_inputs":types(&video["inputs"]),"actual_mft_outputs":types(&video["outputs"]),
            "inspection_errors":video["errors"].as_array().map(|errors| errors.iter().take(16)
                .map(|error| text(error)).collect::<Vec<_>>()),
            "scope":"selected writer MFT inspection; CPU/GPU pixel input does not establish encoder hardware/software kind"})
    }

    fn decode(path: &Path, source: &[u8], started: Instant, run_started: Instant) -> Value {
        let mut frames = Vec::with_capacity(FRAME_COUNT as usize);
        let mut eof = false;
        let result = (|| -> Result<()> {
            checkpoint(started, run_started)?;
            let mut video = VideoReader::open(path)?;
            checkpoint(started, run_started)?;
            if (video.width, video.height) != (samples::width(), samples::height())
                || video.info.codec != MFVideoFormat_H264
            {
                return Err("文字边缘短片回读尺寸或原生 H264 类型不符".into());
            }
            let stop = AtomicBool::new(false);
            let mut previous = None;
            loop {
                checkpoint(started, run_started)?;
                let frame = video.next_cancellable(&stop)?;
                checkpoint(started, run_started)?;
                let Some(frame) = frame else {
                    eof = true;
                    break;
                };
                if frames.len() >= FRAME_COUNT as usize {
                    return Err("文字边缘回读帧数超过固定上限".into());
                }
                if (video.width, video.height) != (samples::width(), samples::height()) {
                    return Err("文字边缘回读尺寸在中途变化".into());
                }
                let index = frames.len() as u32;
                let delta = frame.time.abs_diff(time(index));
                let increasing = previous.is_none_or(|old| frame.time > old);
                previous = Some(frame.time);
                frames.push(
                    json!({"index":index,"pts_100ns":frame.time.as_nanos() / 100,
                    "expected_pts_100ns":ticks(index),"pts_error_ns":delta.as_nanos(),
                    "time_ok":increasing && delta <= TIME_TOLERANCE,
                    "edges":compare(source,&frame.bgra)?}),
                );
            }
            if frames.len() != FRAME_COUNT as usize
                || !eof
                || frames
                    .iter()
                    .any(|frame| frame["time_ok"] != true || frame["edges"]["passed"] != true)
            {
                return Err("文字边缘完整回读的帧数、PTS、亮度或定位/方向检查未通过".into());
            }
            Ok(())
        })();
        json!({"ok":result.is_ok(),"error":result.err().map(|error| short(&error.to_string())),
            "complete_video_eof":eof,"frames":frames})
    }

    fn payload(path: &Path, started: Instant, run_started: Instant) -> Result<Value> {
        checkpoint(started, run_started)?;
        let raw = bitrate_check::inspect_cancellable(path, &|| {
            checkpoint(started, run_started).is_err()
        })?;
        checkpoint(started, run_started)?;
        // 这里只保留跨度证据，不重复序列化 SPS/逐压缩样本属性等其它实验的正文。
        let mut report = json!({"error":raw["error"].as_str().map(short)});
        for key in [
            "complete",
            "samples",
            "payload_bytes",
            "minimum_pts_100ns",
            "maximum_pts_100ns",
            "maximum_end_100ns",
            "observed_span_100ns",
            "missing_timestamps",
            "unknown_durations",
            "invalid_durations",
            "native_type_changes",
            "current_type_changes",
        ] {
            report[key] = raw[key].clone();
        }
        let mut valid = raw["complete"] == true
            && raw["error"].is_null()
            && raw["samples"].as_u64().is_some_and(|count| count > 0)
            && raw["payload_bytes"].as_u64().is_some_and(|bytes| bytes > 0);
        for key in [
            "missing_timestamps",
            "unknown_durations",
            "invalid_durations",
            "native_type_changes",
            "current_type_changes",
        ] {
            valid &= raw[key].as_u64() == Some(0);
        }
        for (key, expected) in [
            ("minimum_pts_100ns", 0),
            ("maximum_pts_100ns", ticks(FRAME_COUNT - 1)),
            ("maximum_end_100ns", ticks(FRAME_COUNT)),
            ("observed_span_100ns", ticks(FRAME_COUNT)),
        ] {
            valid &= raw[key]
                .as_i64()
                .is_some_and(|value| value >= 0 && value.abs_diff(expected as i64) <= 10_000);
        }
        report["span_ok"] = valid.into();
        Ok(report)
    }

    fn route(
        directory: &Path,
        source: &[u8],
        quality: QualityPlan,
        gpu: bool,
        run_started: Instant,
    ) -> Value {
        let started = Instant::now();
        let path = directory.join(if gpu { "gpu-nv12.mp4" } else { "cpu-rgb32.mp4" });
        let mut report = json!({"requested_input":if gpu {"gpu_nv12"} else {"cpu_rgb32"},
            "requested_plan":quality,
            "attempt_index":if gpu {0} else {1},"ok":false,"errors":[],
            "explicit_finalize_calls":0,"written_frames":0,"actual":null,"payload":null,"decode":null});
        let mut writer = None;
        let mut errors = Vec::with_capacity(4);
        let mut written = 0;
        let writing = (|| -> Result<()> {
            checkpoint(started, run_started)?;
            let plan = EncoderPlan::new(EncoderPolicy::Auto, quality);
            let index = if gpu { 0 } else { 1 };
            let expected = if gpu {
                VideoInput::GpuNv12
            } else {
                VideoInput::CpuRgb32
            };
            if plan.attempts().get(index).map(|attempt| attempt.input) != Some(expected) {
                return Err("文字边缘指定 attempt 的像素输入合同改变".into());
            }
            let session = EncoderBackendSession::new(plan, false);
            checkpoint(started, run_started)?;
            let created = match session.create_attempt(&path, index, None) {
                Ok(created) => created,
                Err(error) => {
                    report["creation_error"] =
                        json!({"stage":error.stage,"message":short(&error.to_string())});
                    return Err(Box::new(error));
                }
            };
            writer = Some(created);
            let active = writer.as_mut().ok_or("文字边缘 writer 缺失")?;
            report["uses_gpu_input"] = active.uses_gpu_input().into();
            report["actual"] = writer_evidence(active.diagnostics());
            if active.uses_gpu_input() != gpu
                || report["actual"]["observed_video_codec_matches_request"] != true
                || !report["actual"]["actual_encoder"].is_object()
            {
                return Err("指定像素输入或唯一实际 H264 MFT 未得到观测，不能标已验证".into());
            }
            for index in 0..FRAME_COUNT {
                checkpoint(started, run_started)?;
                active.write_video(
                    source,
                    samples::width(),
                    time(index),
                    time(index + 1) - time(index),
                )?;
                written += 1;
                checkpoint(started, run_started)?;
                observe_file(&path)?;
            }
            Ok(())
        })();
        if let Err(error) = writing {
            errors.push(json!({"stage":"create_or_write","error":short(&error.to_string())}));
        }
        if let Some(writer) = writer {
            // 即使写入/预算检查失败，也只显式 Finalize 一次并同时保留主、副错误。
            report["explicit_finalize_calls"] = 1.into();
            let (finished, diagnostics) = writer.finish_with_diagnostics();
            report["actual"] = writer_evidence(&diagnostics);
            if let Err(error) = finished {
                errors.push(json!({"stage":"finalize","error":short(&error.to_string())}));
            }
        }
        report["written_frames"] = written.into();
        match observe_file(&path) {
            Ok(bytes) => report["bytes"] = bytes.into(),
            Err(error) => {
                errors.push(json!({"stage":"file_budget","error":short(&error.to_string())}))
            }
        }
        if errors.is_empty() {
            match payload(&path, started, run_started) {
                Ok(value) => {
                    if value["span_ok"] != true {
                        errors.push(
                            json!({"stage":"payload_span","error":"压缩负载 EOF/PTS/末帧跨度不符"}),
                        );
                    }
                    report["payload"] = value;
                }
                Err(error) => {
                    errors.push(json!({"stage":"payload","error":short(&error.to_string())}))
                }
            }
            let decoded = decode(&path, source, started, run_started);
            if decoded["ok"] != true {
                errors.push(json!({"stage":"decode","error":decoded["error"]}));
            }
            report["decode"] = decoded;
        }
        report["ok"] = (errors.is_empty() && written == FRAME_COUNT).into();
        report["errors"] = errors.into();
        report["elapsed_ms"] = json!(started.elapsed().as_millis());
        report
    }

    #[test]
    fn recording_color_text_edges_rejects_shift_flip_and_collapsed_contrast() {
        let source = fixture().unwrap();
        assert_eq!(compare(&source, &source).unwrap()["passed"], true);
        let row = samples::width() as usize * 4;
        let mut shifted = source.clone();
        for (dst, src) in shifted.chunks_exact_mut(row).zip(source.chunks_exact(row)) {
            dst[4..].copy_from_slice(&src[..row - 4]);
            dst[..4].copy_from_slice(&[0, 0, 0, 255]);
        }
        assert_eq!(compare(&source, &shifted).unwrap()["passed"], false);
        let flipped = source
            .chunks_exact(row)
            .rev()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(compare(&source, &flipped).unwrap()["passed"], false);
        let mut washed = source.clone();
        for pixel in washed.chunks_exact_mut(4) {
            let gray = if pixel[0] == 0 { 96 } else { 160 };
            pixel[..3].fill(gray);
        }
        assert_eq!(compare(&source, &washed).unwrap()["passed"], false);
    }

    #[test]
    #[ignore = "真实 CPU RGB32/GPU NV12 黑白文字细线 H264 往返；不读桌面、不开摄像头、不播音、不开窗口"]
    fn recording_color_native_text_edges() -> Result<()> {
        let directory = create_output(Path::new("target/recording-color-text-edges"))?;
        let started = Instant::now();
        let mut cases = Vec::with_capacity(2);
        let setup = (|| -> Result<()> {
            let source = fixture()?;
            let mut quality = quality_plan(
                samples::width(),
                samples::height(),
                FPS,
                QualityProfile::Current(Preset::CompactMp4),
            )?;
            quality.bitrate_bps = 4_000_000;
            let _runtime = MfRuntime::new()?;
            for gpu in [false, true] {
                cases.push(route(&directory, &source, quality, gpu, started));
            }
            Ok(())
        })();
        let report = json!({"ok":setup.is_ok() && cases.len() == 2 && cases.iter().all(|case| case["ok"] == true),
            "setup_error":setup.err().map(|error| short(&error.to_string())),"cases":cases,
            "fixture":{"width":samples::width(),"height":samples::height(),"fps":FPS,"frames":FRAME_COUNT,
                "maximum_end_100ns":ticks(FRAME_COUNT),"source":"existing synthetic ASCII/fine lines, thresholded to black/white","audio":false},
            "thresholds":{"black_mean_max":BLACK_MAX,"white_mean_min":WHITE_MIN,
                "pixel_classification_min":PIXEL_AGREEMENT_MIN,"signed_adjacent_contrast_min":EDGE_CONTRAST_MIN,
                "edge_position_direction_agreement_min":EDGE_AGREEMENT_MIN,"pts_tolerance_ms":TIME_TOLERANCE.as_millis()},
            "limits":{"pixels":MAX_PIXELS,"frames_per_route":FRAME_COUNT,"routes":2,
                "observed_file_bytes_per_route":MAX_VIDEO_BYTES,"report_bytes":MAX_REPORT_BYTES,
                "case_seconds_between_calls":CASE_BUDGET.as_secs(),"run_seconds_between_calls":RUN_BUDGET.as_secs()},
            "scope":["Only the specified Auto-plan attempt is created: CPU RGB32 index 1 or GPU NV12 index 0, with the same QualityPlan and no input fallback. Input route does not imply hardware/software encoding.",
                "Every decoded frame is checked to EOF at fixed reference coordinates. Directed adjacent grayscale differences test edge position and polarity; thresholds are project tolerances for this 8-bit binary synthetic fixture, not a perceptual-quality or codec standard.",
                "Native compressed-sample metadata separately checks first/last PTS and maximum end/span within 1 ms; compressed sample count is not displayed frame count.",
                "Budgets are cooperative around encoding/decoding and before each compressed-sample read. Blocking native calls cannot be interrupted; a native write or Finalize can exceed the observed file budget before returning.",
                "Each returned writer is explicitly finalized once, including failed writes. All owned artifacts are retained. No desktop, external font, capture device, window, sound, HDR, scaling or multiple-adapter claim."]});
        let bytes = serde_json::to_vec_pretty(&report)?;
        if bytes.len() > MAX_REPORT_BYTES {
            // 固定投影仍意外超限时先留下明确失败回执；不写超限正文或删除唯一媒体。
            let receipt = json!({"ok":false,"error":"文字边缘报告超过预算；媒体保留，完整正文未写入",
                "original_report_bytes":bytes.len(),"report_limit_bytes":MAX_REPORT_BYTES});
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(directory.join("report.json"))?;
            file.write_all(&serde_json::to_vec_pretty(&receipt)?)?;
            file.flush()?;
            return Err("文字边缘报告超过预算".into());
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join("report.json"))?;
        file.write_all(&bytes)?;
        file.flush()?;
        assert_eq!(
            report["ok"],
            true,
            "文字边缘回读失败，报告：{}",
            directory.join("report.json").display()
        );
        Ok(())
    }
}
