//! 合成矢量样图，只展示工具自身的绘制结果。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    export,
    geometry::Rect,
    scene::{PixelPlane, Scene, SourcePixels},
    Result,
};
use std::{fmt::Write, path::Path};
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
pub fn run() -> Result<()> {
    let folder = Path::new("target/qa/vector");
    std::fs::create_dir_all(folder)?;
    let canvas = Rect::full(1000, 540);
    let mut annotations = Vec::new();
    let cases = [
        (
            "曲线箭头：拖中点弯曲",
            Tool::Arrow,
            vec![p(55., 145.), p(425., 145.), p(240., 250.)],
            Style {
                color: 0x079a76,
                width: 5.,
                ..Style::default()
            },
        ),
        (
            "双向箭头：端点跟随切线",
            Tool::Arrow,
            vec![p(570., 215.), p(930., 115.), p(780., 240.)],
            Style {
                color: 0x5796ff,
                width: 5.,
                arrow_start: true,
                ..Style::default()
            },
        ),
        (
            "折线：顶点保留、节点可编辑",
            Tool::Polyline,
            vec![p(55., 365.), p(170., 490.), p(270., 365.), p(425., 470.)],
            Style {
                color: 0xd56a28,
                width: 5.,
                ..Style::default()
            },
        ),
        (
            "平滑曲线：闭合、半透明填充",
            Tool::Polyline,
            vec![
                p(565., 380.),
                p(700., 450.),
                p(770., 350.),
                p(920., 440.),
                p(760., 500.),
            ],
            Style {
                color: 0x8e67d8,
                width: 4.,
                filled: true,
                opacity: 0.55,
                polyline_smooth: true,
                ..Style::default()
            },
        ),
    ];
    let mut svg = String::from(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="540"><rect width="1000" height="540" fill="white"/>"##,
    );
    for (i, (label, tool, points, style)) in cases.iter().enumerate() {
        let a = Annotation {
            tool: *tool,
            points: points.clone(),
            style: *style,
            text: String::new(),
            rotation: 0.,
            fill_geometry: Default::default(),
            effect_regions: None,
            rich: None,
        };
        let d = a.vector_path().svg_commands();
        let fill = if style.filled {
            format!("#{:06x}", style.color)
        } else {
            "none".into()
        };
        let _ = write!(
            svg,
            r##"<path d="{d}" stroke="#{:06x}" stroke-width="{}" fill="{fill}" opacity="{}" stroke-linecap="round" stroke-linejoin="round"/>"##,
            style.color, style.width, style.opacity
        );
        annotations.push(a);
        let (x, y) = if i < 2 {
            (if i == 0 { 25. } else { 525. }, 45.)
        } else {
            (if i == 2 { 25. } else { 525. }, 315.)
        };
        let mut text = Annotation::new(
            Tool::Text,
            p(x, y - 20.),
            Style {
                font_size: 18.,
                color: 0x23364a,
                ..Style::default()
            },
        );
        text.points.push(p(x + 445., y + 12.));
        text.text = label.to_string();
        annotations.push(text);
        let _ = write!(
            svg,
            r##"<text x="{x}" y="{y}" font-family="Microsoft YaHei UI,sans-serif" font-size="18" fill="#23364a">{label}</text>"##
        );
    }
    svg.push_str("</svg>");
    std::fs::write(folder.join("vector-quality.svg"), svg)?;
    let pixels = [255u8, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
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
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    export::save_png(image, &folder.join("vector-export-quality.png"))?;
    println!("{{\"test\":\"shared_arrow_polyline_vector_export\",\"cases\":4,\"ok\":true}}");
    arrow_styles_quality()?;
    Ok(())
}

fn arrow_styles_quality() -> Result<()> {
    use crate::arrow_geometry::ArrowPreset;
    let folder = Path::new("target/qa/arrow");
    std::fs::create_dir_all(folder)?;
    let canvas = Rect::full(1080, 740);
    let mut objects = Vec::new();
    let mut svg = String::from(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="1080" height="740"><rect width="1080" height="740" fill="white"/>"##,
    );
    for (i, preset) in ArrowPreset::ALL.iter().copied().enumerate() {
        let y = 100. + i as f32 * 108.;
        let mut label = Annotation::new(
            Tool::Text,
            p(24., y - 12.),
            Style {
                font_size: 18.,
                color: 0x23364a,
                ..Style::default()
            },
        );
        label.points.push(p(152., y + 20.));
        label.text = preset.label().into();
        objects.push(label);
        let _ = write!(
            svg,
            r##"<text x="24" y="{y}" font-family="Microsoft YaHei UI,sans-serif" font-size="18" fill="#23364a">{}</text>"##,
            preset.label()
        );
        for curved in [false, true] {
            let x = if curved { 650. } else { 180. };
            let mut style = Style {
                color: 0x207cbe,
                width: 4.,
                ..Style::default()
            };
            preset.apply(&mut style);
            let mut arrow = Annotation::new(Tool::Arrow, p(x, y), style);
            arrow.update(p(x + 280., y));
            if curved {
                arrow.move_arrow_handle(2, p(x + 140., y + 36.));
            }
            let path = arrow.vector_path();
            let d = path.svg_commands();
            let _ = write!(
                svg,
                r##"<path d="{d}" stroke="#207cbe" stroke-width="4" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"##
            );
            for contour in path.fills {
                let mut d = String::new();
                for (j, p) in contour.iter().enumerate() {
                    let _ = write!(d, "{} {} {} ", if j == 0 { "M" } else { "L" }, p.x, p.y);
                }
                d.push('Z');
                let _ = write!(
                    svg,
                    r##"<path d="{d}" fill="#207cbe" fill-rule="evenodd"/>"##
                );
            }
            objects.push(arrow);
        }
    }
    svg.push_str("</svg>");
    std::fs::write(folder.join("arrow-styles.svg"), svg)?;
    let pixels = [255u8, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &pixels,
        }],
        overlay: None,
    };
    let scene = Scene::default().updated(&objects, &source);
    if !scene.errors.is_empty() {
        return Err(scene.errors.join(";").into());
    }
    let mut image = source.read(canvas);
    scene.apply(&mut image, canvas);
    export::save_png(image, &folder.join("arrow-styles.png"))?;
    println!(
        "{{\"test\":\"arrow_six_styles_straight_and_curve_export\",\"cases\":12,\"ok\":true}}"
    );
    Ok(())
}
