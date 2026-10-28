//! 录屏编解码画质实验（默认忽略）：同一段无损“静止 + 滚动”素材分别走产品 GPU 编码路径的
//! 不同码率控制，以及 x264 基线编码；全部用预览同一解码路径（VideoReader）解码，
//! 计算 PSNR、锐度保持率并导出对比裁图。测试台“录屏编解码”页读取结果展示。
//!
//! RUSTSHOT_LAB_SOURCE=shot.png cargo test --lib recording_quality_lab -- --ignored --nocapture
//! 结果写入 target/qa/codec-lab/（report.json、PNG、MP4）；缺 ffmpeg 时跳过 x264 对照。
//! 自包含候选比较用 recording_quality_compare_native_profiles，结果写入
//! target/recording-quality-compare/；它只覆盖合成 ASCII、滚动和简单运动。
#[path = "recording_quality_compare.rs"]
mod profile_compare;
#[path = "recording_quality_samples.rs"]
pub(crate) mod profile_samples;
#[path = "recording_quality_trial.rs"]
mod profile_trial;

use crate::{
    media_foundation::{EncoderTuning, MfRuntime, Mp4Writer, VideoReader},
    recording_encoder_caps::probe_baseline,
    recording_options::{quality_plan, video_bitrate, Preset, QualityPlan, QualityProfile},
    Result,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const WIDTH: u32 = 1012;
const HEIGHT: u32 = 876;
const FPS: u32 = 30;
const STATIC_FRAMES: u32 = 60;
const SCROLL_FRAMES: u32 = 90;
const CROP: (u32, u32) = (480, 300);
/// 展示用的两帧：静止中段与滚动中段。
const SHOTS: [(u32, &str); 2] = [(30, "静止"), (105, "滚动中")];

struct Clip {
    base: image::RgbaImage,
    x0: u32,
    y0: u32,
    speed: u32,
}
impl Clip {
    fn frames(&self) -> u32 {
        STATIC_FRAMES + SCROLL_FRAMES
    }
    fn offset(&self, index: u32) -> u32 {
        self.y0 + index.saturating_sub(STATIC_FRAMES) * self.speed
    }
    /// top-down BGRA，与录屏采集给编码器的格式一致。
    fn bgra(&self, index: u32) -> Vec<u8> {
        let top = self.offset(index);
        let mut out = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let [r, g, b, _] = self.base.get_pixel(self.x0 + x, top + y).0;
                out.extend_from_slice(&[b, g, r, 255]);
            }
        }
        out
    }
}

#[derive(Default)]
struct Stats {
    static_psnr: Vec<f64>,
    scroll_psnr: Vec<f64>,
    static_luma: Vec<f64>,
    scroll_luma: Vec<f64>,
}
impl Stats {
    fn report(&self) -> Value {
        let mean =
            |v: &[f64]| (!v.is_empty()).then(|| round2(v.iter().sum::<f64>() / v.len() as f64));
        let min = |v: &[f64]| v.iter().copied().reduce(f64::min).map(round2);
        json!({
            "psnr": {
                "static": mean(&self.static_psnr),
                "scroll": mean(&self.scroll_psnr),
                "min": min(&[self.static_psnr.as_slice(), self.scroll_psnr.as_slice()].concat()),
            },
            "luma_psnr": {
                "static": mean(&self.static_luma),
                "scroll": mean(&self.scroll_luma),
            },
            "frames": self.static_psnr.len() + self.scroll_psnr.len(),
        })
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.).round() / 100.
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mut sum = 0_u64;
    let mut count = 0_u64;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for c in 0..3 {
            let d = i64::from(pa[c]) - i64::from(pb[c]);
            sum += (d * d) as u64;
        }
        count += 3;
    }
    if sum == 0 {
        return 99.;
    }
    10. * (255_f64.powi(2) / (sum as f64 / count as f64)).log10()
}

/// 只比亮度（BT.709 加权）：与 RGB PSNR 的差距主要来自色度（4:2:0 下采样、色彩转换）。
fn luma_psnr(a: &[u8], b: &[u8]) -> f64 {
    let (la, lb) = (luma(a), luma(b));
    let mse = la
        .iter()
        .zip(&lb)
        .map(|(x, y)| f64::from((x - y) * (x - y)))
        .sum::<f64>()
        / la.len().max(1) as f64;
    if mse < 1e-9 {
        return 99.;
    }
    10. * (255_f64.powi(2) / mse).log10()
}

fn luma(bgra: &[u8]) -> Vec<f32> {
    bgra.chunks_exact(4)
        .map(|p| 0.0722 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.2126 * f32::from(p[2]))
        .collect()
}

/// 亮度梯度能量（|dx|+|dy| 之和），用于比较边缘保留程度。
fn gradient_energy(luma: &[f32], width: u32, area: Option<(u32, u32, u32, u32)>) -> f64 {
    let (x0, y0, w, h) = area.unwrap_or((0, 0, width, luma.len() as u32 / width));
    let mut sum = 0_f64;
    for y in y0..(y0 + h).saturating_sub(1) {
        for x in x0..(x0 + w).saturating_sub(1) {
            let i = (y * width + x) as usize;
            sum += f64::from(
                (luma[i + 1] - luma[i]).abs() + (luma[i + width as usize] - luma[i]).abs(),
            );
        }
    }
    sum
}

/// 在静止帧里找文字最密的裁剪框，展示时最能看出差别。
fn densest_crop(bgra: &[u8]) -> (u32, u32) {
    let l = luma(bgra);
    let mut best = (0, 0, -1_f64);
    for y in (0..=HEIGHT - CROP.1).step_by(20) {
        for x in (0..=WIDTH - CROP.0).step_by(20) {
            let energy = gradient_energy(&l, WIDTH, Some((x, y, CROP.0, CROP.1)));
            if energy > best.2 {
                best = (x, y, energy);
            }
        }
    }
    (best.0, best.1)
}

fn save_png(
    path: &Path,
    bgra: &[u8],
    width: u32,
    area: Option<(u32, u32, u32, u32)>,
) -> Result<()> {
    let (x0, y0, w, h) = area.unwrap_or((0, 0, width, bgra.len() as u32 / 4 / width));
    let mut out = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let i = (((y0 + y) * width + x0 + x) * 4) as usize;
            out.put_pixel(x, y, image::Rgba([bgra[i + 2], bgra[i + 1], bgra[i], 255]));
        }
    }
    out.save(path)?;
    Ok(())
}

/// 误差放大 4 倍的灰度图：越亮误差越大。
fn save_diff(path: &Path, decoded: &[u8], source: &[u8], area: (u32, u32, u32, u32)) -> Result<()> {
    let (x0, y0, w, h) = area;
    let mut out = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let i = (((y0 + y) * WIDTH + x0 + x) * 4) as usize;
            let d = (0..3)
                .map(|c| (i32::from(decoded[i + c]) - i32::from(source[i + c])).unsigned_abs())
                .max()
                .unwrap_or(0);
            let v = (d * 4).min(255) as u8;
            out.put_pixel(x, y, image::Rgba([v, v, v, 255]));
        }
    }
    out.save(path)?;
    Ok(())
}

struct Variant {
    id: &'static str,
    label: &'static str,
    group: &'static str,
    note: &'static str,
    source: Encode,
    decode: Decode,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decode {
    /// 预览当前路径：Media Foundation 软件 XVP 转 RGB32。
    Mf,
    /// D3D11 硬件解码 + 显卡视频处理器转 RGB32。
    MfGpu,
    /// ffmpeg（精确色度插值），区分解码端损失。
    Ffmpeg,
}
impl Decode {
    fn name(self) -> &'static str {
        match self {
            Self::Mf => "media_foundation",
            Self::MfGpu => "media_foundation_d3d11",
            Self::Ffmpeg => "ffmpeg",
        }
    }
}
enum Encode {
    Planned(QualityPlan),
    Mf {
        preset: Preset,
        tuning: EncoderTuning,
    },
    /// 指定每像素位数（码率曲线）。
    MfBpp {
        bpp: f64,
        tuning: EncoderTuning,
    },
    X264 {
        crf: u32,
    },
    /// qp 0：只剩 4:2:0 色度下采样与 RGB↔YUV 转换的损失，是 H.264 4:2:0 的上限。
    X264Lossless,
    /// 复用已编码文件，只换解码路径。
    Reuse(&'static str),
}

fn variants() -> Vec<Variant> {
    let vbr = |preset| EncoderTuning {
        rate_control: Some(1),
        max_bitrate: Some(video_bitrate(WIDTH, HEIGHT, FPS, preset) * 2),
        ..Default::default()
    };
    let mut variants = vec![
        Variant {
            id: "mf_compact_cbr",
            label: "压缩 MP4（当前）",
            group: "RustShot 当前",
            note: "0.07 bit/像素，速度优先；编码器与控制模式以实际回读为准",
            source: Encode::Mf {
                preset: Preset::CompactMp4,
                tuning: EncoderTuning::default(),
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_hq_cbr",
            label: "高清 MP4（当前）",
            group: "RustShot 当前",
            note: "0.16 bit/像素；编码器与控制模式以实际回读为准",
            source: Encode::Mf {
                preset: Preset::HighQualityMp4,
                tuning: EncoderTuning::default(),
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_compact_vbr",
            label: "压缩 + 峰值 VBR",
            group: "RustShot 候选",
            note: "平均码率同压缩档，峰值 2 倍",
            source: Encode::Mf {
                preset: Preset::CompactMp4,
                tuning: vbr(Preset::CompactMp4),
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_hq_vbr",
            label: "高清 + 峰值 VBR",
            group: "RustShot 候选",
            note: "平均码率同高清档，峰值 2 倍",
            source: Encode::Mf {
                preset: Preset::HighQualityMp4,
                tuning: vbr(Preset::HighQualityMp4),
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_hq_vbr_slow",
            label: "高清 + VBR + 质量优先",
            group: "诊断",
            note: "同上，QualityVsSpeed=100",
            source: Encode::Mf {
                preset: Preset::HighQualityMp4,
                tuning: EncoderTuning {
                    quality_vs_speed: Some(100),
                    ..vbr(Preset::HighQualityMp4)
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_compact_cbr_nolow",
            label: "压缩 + 关低延迟",
            group: "诊断",
            note: "仍是 CBR 与压缩档码率，只关 AVLowLatencyMode",
            source: Encode::Mf {
                preset: Preset::CompactMp4,
                tuning: EncoderTuning {
                    low_latency: Some(false),
                    ..Default::default()
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_compact_vbr_nolow",
            label: "压缩 + VBR + 关低延迟",
            group: "诊断",
            note: "平均码率同压缩档，峰值 2 倍，关低延迟",
            source: Encode::Mf {
                preset: Preset::CompactMp4,
                tuning: EncoderTuning {
                    low_latency: Some(false),
                    ..vbr(Preset::CompactMp4)
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_compact_vbr_buf",
            label: "压缩 + VBR + 关低延迟 + 2 秒缓冲",
            group: "诊断",
            note: "同上，VBV 放大到 2 秒平均码率，让首帧／场景切换拿到更多比特",
            source: Encode::Mf {
                preset: Preset::CompactMp4,
                tuning: EncoderTuning {
                    low_latency: Some(false),
                    buffer_bits: Some(
                        (video_bitrate(WIDTH, HEIGHT, FPS, Preset::CompactMp4) * 2).div_ceil(8) * 8,
                    ),
                    ..vbr(Preset::CompactMp4)
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_hq_vbr_nolow",
            label: "高清 + VBR + 关低延迟",
            group: "诊断",
            note: "平均码率同高清档，峰值 2 倍，关低延迟",
            source: Encode::Mf {
                preset: Preset::HighQualityMp4,
                tuning: EncoderTuning {
                    low_latency: Some(false),
                    ..vbr(Preset::HighQualityMp4)
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_vbr_bpp10",
            label: "VBR · 0.10 bit/像素",
            group: "码率曲线",
            note: "峰值 VBR，平均码率按 0.1 bit/像素（压缩 0.07、高清 0.16）",
            source: Encode::MfBpp {
                bpp: 0.1,
                tuning: EncoderTuning {
                    rate_control: Some(1),
                    max_bitrate: Some(bpp_bitrate(0.1) * 2),
                    ..Default::default()
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_vbr_bpp12",
            label: "VBR · 0.12 bit/像素",
            group: "码率曲线",
            note: "峰值 VBR，平均码率按 0.12 bit/像素（压缩 0.07、高清 0.16）",
            source: Encode::MfBpp {
                bpp: 0.12,
                tuning: EncoderTuning {
                    rate_control: Some(1),
                    max_bitrate: Some(bpp_bitrate(0.12) * 2),
                    ..Default::default()
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_quality_75",
            label: "质量模式 75",
            group: "RustShot 候选",
            note: "Quality 模式，AVEncCommonQuality=75，码率随内容",
            source: Encode::Mf {
                preset: Preset::HighQualityMp4,
                tuning: EncoderTuning {
                    rate_control: Some(3),
                    quality: Some(75),
                    ..Default::default()
                },
            },
            decode: Decode::Mf,
        },
        Variant {
            id: "x264_crf23",
            label: "x264 CRF 23",
            group: "x264 基线",
            note: "标准档：libx264 fast，关 AQ／mb-tree",
            source: Encode::X264 { crf: 23 },
            decode: Decode::Mf,
        },
        Variant {
            id: "x264_crf18",
            label: "x264 CRF 18",
            group: "x264 基线",
            note: "高清档",
            source: Encode::X264 { crf: 18 },
            decode: Decode::Mf,
        },
        Variant {
            id: "x264_lossless420",
            label: "x264 无损 4:2:0（上限）",
            group: "诊断",
            note: "qp 0：码率不限，只剩色度下采样与颜色转换损失",
            source: Encode::X264Lossless,
            decode: Decode::Mf,
        },
        Variant {
            id: "mf_compact_cbr_ffdec",
            label: "压缩 MP4 · ffmpeg 解码",
            group: "诊断",
            note: "同一文件，换精确色度插值解码，对比预览解码器",
            source: Encode::Reuse("mf_compact_cbr"),
            decode: Decode::Ffmpeg,
        },
        Variant {
            id: "mf_hq_cbr_ffdec",
            label: "高清 MP4 · ffmpeg 解码",
            group: "诊断",
            note: "同一文件，换精确色度插值解码",
            source: Encode::Reuse("mf_hq_cbr"),
            decode: Decode::Ffmpeg,
        },
        Variant {
            id: "mf_compact_cbr_gpudec",
            label: "压缩 MP4 · 显卡解码",
            group: "诊断",
            note: "同一文件，D3D11 硬解 + 显卡视频处理器转 RGB",
            source: Encode::Reuse("mf_compact_cbr"),
            decode: Decode::MfGpu,
        },
        Variant {
            id: "mf_hq_cbr_gpudec",
            label: "高清 MP4 · 显卡解码",
            group: "诊断",
            note: "同一文件，D3D11 硬解 + 显卡视频处理器转 RGB",
            source: Encode::Reuse("mf_hq_cbr"),
            decode: Decode::MfGpu,
        },
        Variant {
            id: "x264_lossless420_ffdec",
            label: "无损 4:2:0 · ffmpeg 解码",
            group: "诊断",
            note: "4:2:0 上限，换精确色度插值解码",
            source: Encode::Reuse("x264_lossless420"),
            decode: Decode::Ffmpeg,
        },
    ];
    variants.extend(
        QualityProfile::EXPERIMENTS
            .into_iter()
            .map(|profile| Variant {
                id: profile.id(),
                label: profile.label(),
                group: "预设候选",
                note: profile.explanation(),
                source: Encode::Planned(
                    quality_plan(WIDTH, HEIGHT, FPS, profile).expect("实验固定尺寸和帧率有效"),
                ),
                decode: Decode::Mf,
            }),
    );
    variants
}

fn plan_tuning(plan: QualityPlan) -> EncoderTuning {
    EncoderTuning::from_quality_plan(plan)
}

fn bpp_bitrate(bpp: f64) -> u32 {
    (f64::from(WIDTH) * f64::from(HEIGHT) * f64::from(FPS) * bpp) as u32
}

fn encode_mf(clip: &Clip, path: &Path, bitrate: u32, tuning: EncoderTuning) -> Result<Value> {
    let mut writer = Mp4Writer::new_tuned_for_lab(path, WIDTH, HEIGHT, FPS, bitrate, tuning)?;
    let interval = Duration::from_nanos(1_000_000_000 / u64::from(FPS));
    for index in 0..clip.frames() {
        writer.write_video(&clip.bgra(index), WIDTH, interval * index, interval)?;
    }
    let gpu_input = writer.uses_gpu_input();
    let (finished, mut diagnostics) = writer.finish_with_diagnostics();
    diagnostics["target_bitrate"] = bitrate.into();
    diagnostics["gpu_input"] = gpu_input.into();
    finished?;
    Ok(diagnostics)
}

fn encode_x264(clip: &Clip, path: &Path, crf: Option<u32>) -> Result<Value> {
    let rate = match crf {
        Some(crf) => vec!["-crf".to_string(), crf.to_string()],
        None => vec!["-qp".to_string(), "0".to_string()],
    };
    use std::io::Write;
    // 与 MF 路径完全相同的 BGRA 帧经管道送入，避免两边素材不一致。
    let mut child = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "bgra"])
        .args([
            "-s",
            &format!("{WIDTH}x{HEIGHT}"),
            "-r",
            &FPS.to_string(),
            "-i",
            "-",
        ])
        .args([
            "-vf",
            "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p",
        ])
        .args(["-c:v", "libx264", "-preset", "fast"])
        .args(&rate)
        .args(["-aq-mode", "0", "-x264-params", "mbtree=0"])
        .args([
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
        ])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().ok_or("ffmpeg 无输入管道")?;
        for index in 0..clip.frames() {
            stdin.write_all(&clip.bgra(index))?;
        }
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("ffmpeg 退出码 {status}").into());
    }
    Ok(
        json!({"name": "libx264", "crf": crf, "qp": crf.is_none().then_some(0), "preset": "fast", "aq": 0, "mbtree": 0}),
    )
}

/// ffmpeg 解码为 top-down BGRA：显式按 BT.709 limited 解释，色度用精确插值。
struct FfmpegFrames {
    child: std::process::Child,
}
impl FfmpegFrames {
    fn open(path: &Path) -> Result<Self> {
        let child = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args([
                "-vf",
                "scale=in_color_matrix=bt709:in_range=tv:flags=bicubic+accurate_rnd+full_chroma_int,format=bgra",
                "-f",
                "rawvideo",
                "-",
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        Ok(Self { child })
    }
    fn next(&mut self) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        let stdout = self.child.stdout.as_mut().ok_or("ffmpeg 无输出管道")?;
        let mut frame = vec![0_u8; (WIDTH * HEIGHT * 4) as usize];
        match stdout.read_exact(&mut frame) {
            Ok(()) => Ok(Some(frame)),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}
impl Drop for FfmpegFrames {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn evaluate(
    clip: &Clip,
    path: &Path,
    decode: Decode,
    out: &Path,
    id: &str,
    crop: (u32, u32, u32, u32),
) -> Result<Value> {
    let (mut next, decoder): (Box<dyn FnMut() -> Result<Option<Vec<u8>>>>, Value) = if decode
        == Decode::Ffmpeg
    {
        let mut frames = FfmpegFrames::open(path)?;
        (
            Box::new(move || frames.next()),
            json!({"processing": "ffmpeg bicubic+accurate_rnd+full_chroma_int"}),
        )
    } else {
        let mut reader = if decode == Decode::MfGpu {
            VideoReader::open_gpu_for_lab(path)?
        } else {
            VideoReader::open(path)?
        };
        if (reader.width, reader.height) != (WIDTH, HEIGHT) {
            return Err(format!("解码尺寸 {}×{} 与源不一致", reader.width, reader.height).into());
        }
        let diagnostics = reader.diagnostics();
        (
            Box::new(move || Ok(reader.next()?.map(|frame| frame.bgra))),
            diagnostics,
        )
    };
    let mut stats = Stats::default();
    let mut index = 0;
    while let Some(bgra) = next()? {
        if index >= clip.frames() {
            break;
        }
        let frame = crate::media_foundation::DecodedFrame {
            time: Duration::ZERO,
            bgra,
        };
        let source = clip.bgra(index);
        let value = psnr(&frame.bgra, &source);
        let luma_value = luma_psnr(&frame.bgra, &source);
        if index < STATIC_FRAMES {
            stats.static_psnr.push(value);
            stats.static_luma.push(luma_value);
        } else {
            stats.scroll_psnr.push(value);
            stats.scroll_luma.push(luma_value);
        }
        if let Some((shot, _)) = SHOTS.iter().find(|(shot, _)| *shot == index) {
            save_png(
                &out.join(format!("{id}-f{shot}-full.png")),
                &frame.bgra,
                WIDTH,
                None,
            )?;
            save_png(
                &out.join(format!("{id}-f{shot}-crop.png")),
                &frame.bgra,
                WIDTH,
                Some(crop),
            )?;
            save_diff(
                &out.join(format!("{id}-f{shot}-diff.png")),
                &frame.bgra,
                &source,
                crop,
            )?;
        }
        index += 1;
    }
    if index == 0 {
        return Err(if decode == Decode::Ffmpeg {
            "ffmpeg 未解出任何帧".into()
        } else {
            "预览解码器（Media Foundation）不支持该文件（x264 无损为 High 4:4:4 Predictive）".into()
        });
    }
    let mut report = stats.report();
    report["decoded_frames"] = index.into();
    report["decoder"] = decoder;
    Ok(report)
}

fn run(source_path: &Path) -> Result<()> {
    let base = image::open(source_path)?.to_rgba8();
    if base.width() < WIDTH || base.height() < HEIGHT + 60 {
        return Err("素材至少需要 1012×936".into());
    }
    let x0 = ((base.width() - WIDTH) / 2).min(300);
    let y0 = 0;
    let speed = ((base.height() - HEIGHT - y0) / SCROLL_FRAMES).clamp(1, 5);
    let clip = Clip {
        base,
        x0,
        y0,
        speed,
    };
    let out = PathBuf::from("target/qa/codec-lab");
    std::fs::create_dir_all(&out)?;
    let _runtime = MfRuntime::new()?;
    let catalog = match probe_baseline() {
        Ok(catalog) => serde_json::to_value(catalog)?,
        Err(error) => json!({"error": error.to_string()}),
    };

    let first = clip.bgra(SHOTS[0].0);
    let (cx, cy) = densest_crop(&first);
    let crop = (cx, cy, CROP.0, CROP.1);
    for (shot, _) in SHOTS {
        let frame = clip.bgra(shot);
        save_png(
            &out.join(format!("source-f{shot}-full.png")),
            &frame,
            WIDTH,
            None,
        )?;
        save_png(
            &out.join(format!("source-f{shot}-crop.png")),
            &frame,
            WIDTH,
            Some(crop),
        )?;
    }

    let ffmpeg = Command::new("ffmpeg").arg("-version").output().is_ok();
    let mut results = Vec::new();
    for variant in variants() {
        let file = match variant.source {
            Encode::Reuse(id) => id,
            _ => variant.id,
        };
        let video = out.join(format!("{file}.mp4"));
        let started = Instant::now();
        let needs_ffmpeg = variant.decode == Decode::Ffmpeg
            || matches!(variant.source, Encode::X264 { .. } | Encode::X264Lossless);
        let encoded = match &variant.source {
            _ if needs_ffmpeg && !ffmpeg => Err("未找到 ffmpeg".into()),
            Encode::Planned(plan) => encode_mf(&clip, &video, plan.bitrate_bps, plan_tuning(*plan)),
            Encode::Mf { preset, tuning } => encode_mf(
                &clip,
                &video,
                video_bitrate(WIDTH, HEIGHT, FPS, *preset),
                *tuning,
            ),
            Encode::MfBpp { bpp, tuning } => encode_mf(&clip, &video, bpp_bitrate(*bpp), *tuning),
            Encode::X264 { crf } => encode_x264(&clip, &video, Some(*crf)),
            Encode::X264Lossless => encode_x264(&clip, &video, None),
            Encode::Reuse(id) => {
                if video.exists() {
                    Ok(json!({"reused": id}))
                } else {
                    Err(format!("缺少 {id} 的编码文件").into())
                }
            }
        };
        let encode_ms = started.elapsed().as_secs_f64() * 1000.;
        let mut entry = json!({
            "id": variant.id,
            "label": variant.label,
            "group": variant.group,
            "note": variant.note,
            "video": format!("{file}.mp4"),
            "decode": variant.decode.name(),
        });
        if let Encode::Planned(plan) = &variant.source {
            entry["requested_plan"] = serde_json::to_value(plan)?;
        }
        match encoded.and_then(|encoder| {
            let bytes = std::fs::metadata(&video)?.len();
            let seconds = f64::from(clip.frames()) / f64::from(FPS);
            let quality = evaluate(&clip, &video, variant.decode, &out, variant.id, crop)?;
            Ok((encoder, bytes, seconds, quality))
        }) {
            Ok((encoder, bytes, seconds, quality)) => {
                entry["encoder"] = encoder;
                entry["bytes"] = bytes.into();
                entry["kbps"] = round2(bytes as f64 * 8. / seconds / 1000.).into();
                entry["container_bitrate_bps"] = (bytes as f64 * 8. / seconds).into();
                entry["kbps_scope"] = "container bytes divided by source clip duration".into();
                entry["video_payload"] = crate::recording::bitrate_check::inspect(&video)
                    .unwrap_or_else(|error| json!({"complete": false, "error": error.to_string()}));
                entry["encode_ms"] = round2(encode_ms).into();
                entry["quality"] = quality;
            }
            Err(error) => entry["error"] = error.to_string().into(),
        }
        println!("{}", serde_json::to_string(&entry)?);
        results.push(entry);
    }

    let report = json!({
        "generated": chrono_like_now(),
        "encoder_catalog": catalog,
        "source": {
            "file": source_path.file_name().map(|n| n.to_string_lossy().to_string()),
            "size": [WIDTH, HEIGHT],
            "fps": FPS,
            "static_frames": STATIC_FRAMES,
            "scroll_frames": SCROLL_FRAMES,
            "scroll_px_per_frame": clip.speed,
        },
        "crop": [crop.0, crop.1, crop.2, crop.3],
        "shots": SHOTS.iter().map(|(frame, label)| json!({"frame": frame, "label": label})).collect::<Vec<_>>(),
        "variants": results,
    });
    std::fs::write(
        out.join("report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    Ok(())
}

fn chrono_like_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[test]
#[ignore = "需要真实 GPU 编码器与素材：RUSTSHOT_LAB_SOURCE=png"]
fn recording_quality_lab() {
    let source = std::env::var_os("RUSTSHOT_LAB_SOURCE").expect("设置 RUSTSHOT_LAB_SOURCE");
    run(Path::new(&source)).unwrap();
}

#[test]
fn lab_metrics_identity_and_crop_bounds() {
    let frame: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|i| {
            let v = ((i % WIDTH) * 7 % 255) as u8;
            [v, v, v, 255]
        })
        .collect();
    assert_eq!(psnr(&frame, &frame), 99.);
    let (x, y) = densest_crop(&frame);
    assert!(x + CROP.0 <= WIDTH && y + CROP.1 <= HEIGHT);
    let blurred: Vec<u8> = frame
        .chunks_exact(4)
        .map(|p| [p[0] / 2 + 64, p[1] / 2 + 64, p[2] / 2 + 64, 255])
        .flatten()
        .collect();
    let l = luma(&frame);
    assert!(gradient_energy(&luma(&blurred), WIDTH, None) < gradient_energy(&l, WIDTH, None));
}
