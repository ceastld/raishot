//! 铅笔质量与耗时验收；只生成合成图，不读取桌面、剪贴板或上传内容。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    export,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
    Result,
};
use std::{fmt::Write, path::Path, time::Instant};
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
pub fn run() -> Result<()> {
    let folder = Path::new("target/qa/pencil");
    std::fs::create_dir_all(folder)?;
    let canvas = Rect::full(1000, 560);
    let cases = [
        (
            "直角：保护原始折点",
            p(35., 95.),
            (0..=160)
                .map(|x| p(x as f32, 0.))
                .chain((1..=130).map(|y| p(160., y as f32)))
                .collect::<Vec<_>>(),
        ),
        (
            "轻微抖动：有限降噪",
            p(540., 150.),
            (0..400)
                .map(|i| p(i as f32 * 0.8, if i % 2 == 0 { 0.8 } else { -0.8 }))
                .collect(),
        ),
        (
            "尖角：保持准确顶点",
            p(40., 350.),
            vec![p(0., 0.), p(160., 130.), p(320., 0.)],
        ),
        (
            "圆弧：受限贝塞尔拟合",
            p(720., 400.),
            (0..240)
                .map(|i| {
                    let a = i as f32 / 239. * std::f32::consts::PI;
                    p(145. * a.cos(), 90. * a.sin())
                })
                .collect(),
        ),
    ];
    let mut annotations = vec![];
    let mut svg=String::from("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1000\" height=\"560\" viewBox=\"0 0 1000 560\"><rect width=\"1000\" height=\"560\" fill=\"#ffffff\"/><g font-family=\"Microsoft YaHei UI, sans-serif\" fill=\"#23364a\"><text x=\"24\" y=\"32\" font-size=\"20\">RustShot 保角矢量铅笔</text><text x=\"24\" y=\"56\" font-size=\"14\">灰色：原始样本　绿色：新路径　首尾及明显转角保持原位</text></g>");
    let mut counts = vec![];
    for (index, (label, origin, raw)) in cases.iter().enumerate() {
        let points: Vec<_> = raw
            .iter()
            .map(|point| p(point.x + origin.x, point.y + origin.y))
            .collect();
        let path = crate::pencil::fit(&points);
        let y = if index < 2 { 80. } else { 325. };
        let x = if index % 2 == 0 { 24. } else { 525. };
        let _=write!(svg,"<text x=\"{x}\" y=\"{y}\" font-family=\"Microsoft YaHei UI, sans-serif\" font-size=\"16\" fill=\"#23364a\">{label}</text>");
        let mut d = String::new();
        for (i, point) in points.iter().enumerate() {
            let _ = write!(
                d,
                "{} {:.3} {:.3} ",
                if i == 0 { "M" } else { "L" },
                point.x,
                point.y
            );
        }
        let _=write!(svg,"<path d=\"{d}\" fill=\"none\" stroke=\"#aeb6c0\" stroke-width=\"1\"/><path d=\"{}\" fill=\"none\" stroke=\"#079a76\" stroke-width=\"3\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>",path.svg_commands());
        annotations.push(Annotation {
            tool: Tool::Polyline,
            points: points.clone(),
            style: Style {
                color: 0xaeb6c0,
                width: 1.,
                ..Default::default()
            },
            text: String::new(),
            rotation: 0.,
            fill_geometry: Default::default(),
            effect_regions: None,
            rich: None,
        });
        annotations.push(Annotation {
            tool: Tool::Pen,
            points,
            style: Style {
                color: 0x079a76,
                width: 3.,
                ..Default::default()
            },
            text: String::new(),
            rotation: 0.,
            fill_geometry: Default::default(),
            effect_regions: None,
            rich: None,
        });
        let mut text = Annotation::new(
            Tool::Text,
            p(x, y - 21.),
            Style {
                color: 0x23364a,
                font_size: 17.,
                ..Default::default()
            },
        );
        text.points.push(p(x + 420., y + 8.));
        text.text = label.to_string();
        annotations.push(text);
        counts.push((raw.len(), path.spans.len()));
    }
    svg.push_str("</svg>");
    std::fs::write(folder.join("pencil-quality.svg"), svg)?;
    let pixels = [255_u8, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let scene = Scene::default().updated(&annotations, &source);
    if !scene.errors.is_empty() {
        return Err(scene.errors.join(";").into());
    }
    let mut image = Image {
        width: canvas.width,
        height: canvas.height,
        bgra: pixels.clone(),
        readback_ms: 0.,
    };
    scene.apply(&mut image, canvas);
    export::save_png(image, &folder.join("pencil-export-quality.png"))?;
    let long: Vec<_> = (0..6000)
        .map(|i| {
            p(
                i as f32 * 0.5,
                70. * (i as f32 * 0.03).sin() + if i % 2 == 0 { 0.4 } else { -0.4 },
            )
        })
        .collect();
    let mut elapsed = Vec::with_capacity(25);
    let mut spans = 0;
    for _ in 0..25 {
        let start = Instant::now();
        let path = crate::pencil::fit(&long);
        let _ = std::hint::black_box(path.flattened());
        spans = path.spans.len();
        elapsed.push(start.elapsed().as_secs_f64() * 1000.);
    }
    elapsed.sort_by(f64::total_cmp);
    let report=format!("{{\"test\":\"corner_preserving_vector_pencil\",\"cases_raw_and_spans\":{counts:?},\"long_samples\":6000,\"long_spans\":{spans},\"median_ms\":{:.3},\"p95_ms\":{:.3},\"ok\":true}}",elapsed[12],elapsed[23]).replace('(',"[").replace(')',"]");
    println!("{report}");
    std::fs::write(folder.join("pencil-quality.json"), &report)?;
    detail_quality(folder)?;
    quantization_quality(folder)?;
    Ok(())
}

// 相同原始样本分别走原始与平滑模式；细线样图只含合成内容。
fn detail_quality(folder: &Path) -> Result<()> {
    let canvas = Rect::full(960, 820);
    let pixels = [255_u8, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let traces = [
        (
            "1 px 斜线 · 整数像素采样",
            (0..320)
                .map(|i| p(i as f32, (i as f32 * 0.27).round()))
                .collect::<Vec<_>>(),
        ),
        (
            "1 px 圆弧 · 整数像素采样",
            (0..240)
                .map(|i| {
                    let t = i as f32 / 239. * std::f32::consts::PI;
                    p((155. + 145. * t.cos()).round(), (75. * t.sin()).round())
                })
                .collect(),
        ),
        (
            "轻微抖动 · 连续矢量路径",
            (0..400)
                .map(|i| p(i as f32 * 0.8, if i % 2 == 0 { 0.8 } else { -0.8 }))
                .collect(),
        ),
        (
            "直角与回折 · 保留真实顶点",
            vec![
                p(0., 0.),
                p(130., 0.),
                p(130., 72.),
                p(220., 0.),
                p(310., 72.),
            ],
        ),
    ];
    let mut annotations = Vec::new();
    let label = |content: &str, x: f32, y: f32, size: f32| {
        let mut a = Annotation::new(
            Tool::Text,
            p(x, y),
            Style {
                color: 0x25374b,
                font_size: size,
                ..Default::default()
            },
        );
        a.update(p(x + 420., y + 40.));
        a.text = content.into();
        a
    };
    annotations.push(label("原始采样", 24., 12., 23.));
    annotations.push(label("优化后的矢量铅笔", 504., 12., 23.));
    for (i, (name, raw)) in traces.iter().enumerate() {
        let y = 72. + i as f32 * 180.;
        for (x, smooth) in [(24., false), (504., true)] {
            annotations.push(label(name, x, y, 17.));
            let origin = p(x + 40., y + 45.);
            annotations.push(Annotation {
                tool: Tool::Pen,
                points: raw
                    .iter()
                    .map(|v| {
                        p(
                            v.x + origin.x,
                            v.y + origin.y + if i == 2 { 35. } else { 0. },
                        )
                    })
                    .collect(),
                style: Style {
                    color: 0x25374b,
                    width: if i == 3 { 3. } else { 1. },
                    pencil_smooth: smooth,
                    ..Default::default()
                },
                rotation: 0.,
                fill_geometry: Default::default(),
                effect_regions: None,
                rich: None,
                text: String::new(),
            });
        }
    }
    let scene = Scene::default().updated(&annotations, &source);
    if !scene.errors.is_empty() {
        return Err(scene.errors.join(";").into());
    }
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    export::save_png(image, &folder.join("pencil-detail-quality.png"))
}

// 多斜率、多像素相位合成轨迹，避免只针对某一条斜线调参。
fn quantization_quality(folder: &Path) -> Result<()> {
    let mut raw_error = 0.;
    let mut fitted_error = 0.;
    let mut worst_rms = 0_f32;
    let mut cases = 0;
    for slope in [0.08_f32, 0.18, 0.27, 0.37, 0.68, 0.95] {
        for sign in [-1., 1.] {
            for phase in [0., 0.3, 0.7] {
                let slope = slope * sign;
                let raw: Vec<_> = (0..320)
                    .map(|i| p(i as f32, (i as f32 * slope + phase).round()))
                    .collect();
                let path = crate::pencil::fit(&raw);
                let lines = path.flattened();
                let mut error = 0.;
                for (i, sample) in raw.iter().enumerate().take(312).skip(8) {
                    let x = i as f32;
                    let (a, b) = lines
                        .iter()
                        .find(|(a, b)| a.x <= x && b.x > x)
                        .ok_or("铅笔路径出现缺口")?;
                    let y = a.y + (b.y - a.y) * (x - a.x) / (b.x - a.x);
                    let ideal = x * slope + phase;
                    let denominator = 1. + slope * slope;
                    error += (y - ideal).powi(2) / denominator;
                    raw_error += (sample.y - ideal).powi(2) / denominator;
                }
                error /= 304.;
                fitted_error += error;
                worst_rms = worst_rms.max(error.sqrt());
                cases += 1;
            }
        }
    }
    let rms = (fitted_error / cases as f32).sqrt();
    if rms >= 0.105 || worst_rms >= 0.18 {
        return Err(format!("量化降噪回归：rms={rms}, worst={worst_rms}").into());
    }
    let raw_rms = (raw_error / (cases * 304) as f32).sqrt();
    let report = format!("{{\"test\":\"pencil_quantization_spectrum\",\"cases\":{cases},\"raw_rms_px\":{raw_rms:.5},\"fitted_rms_px\":{rms:.5},\"worst_rms_px\":{worst_rms:.5},\"ok\":true}}");
    println!("{report}");
    std::fs::write(folder.join("pencil-quantization.json"), report)?;
    Ok(())
}
