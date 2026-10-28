//! 图片翻译核心：OCR 段落 → 翻译块；本机取样底色与文字色；译文按原区域排成带底色的
//! 文字标注。只有块文字会交给翻译服务；取色与排版都在本机完成，不依赖 GPUI。
use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    capture::Image,
    text_format::Alignment,
    Result,
};
use std::ops::Range;

/// 译文底色框比原文行框外扩的距离；文字标注背景另有 4px 内边距。
const COVER_MARGIN: f32 = 3.;
const MIN_FONT: f32 = 9.;
const MAX_FONT: f32 = 160.;
/// 首行上沿相对水平方向的最大斜率；更斜的文字第一版保持原样。
const MAX_SLOPE: f32 = 0.12;

/// 一个待译段落：原文与其在全屏画布坐标中的行框。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SourceBlock {
    pub text: String,
    pub bounds: Bounds,
    /// 各识别行高度的中位数，用于推算起始字号。
    pub line_height: f32,
    pub alignment: Alignment,
    pub rotated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Skip {
    /// 没有字母或文字，如数字、符号、日期。
    NoLetters,
    /// 网址、路径、命令或代码。
    Code,
    /// 原文已是明确指定的目标语言。
    SameLanguage,
    /// 倾斜或竖排文字，第一版保持原样。
    Rotated,
}

impl Skip {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::NoLetters => tr!("数字或符号", "numbers or symbols"),
            Self::Code => tr!("代码或网址", "code or links"),
            Self::SameLanguage => tr!("已是目标语言", "already in target language"),
            Self::Rotated => tr!("倾斜或竖排文字", "rotated or vertical text"),
        }
    }
}

/// 按段落划分行，并合并各行四角得到块的轴对齐范围；空段与缺几何的段落不产生块。
pub(crate) fn blocks_from_paragraphs<S: AsRef<str>>(
    paragraphs: &[Range<usize>],
    paragraph_text: &[S],
    line_quads: &[Vec<[Point; 4]>],
) -> Vec<SourceBlock> {
    paragraphs
        .iter()
        .zip(paragraph_text)
        .filter_map(|(rows, text)| {
            let text = text.as_ref().trim();
            if text.is_empty() {
                return None;
            }
            let rows: Vec<(Bounds, bool)> = line_quads
                .get(rows.clone())?
                .iter()
                .filter(|quads| !quads.is_empty())
                .map(|quads| (quad_bounds(quads), quads.iter().any(|q| tilted(q))))
                .collect();
            let first = rows.first()?.0;
            let (mut left, mut top, mut right, mut bottom) = (
                first.x,
                first.y,
                first.x + first.width,
                first.y + first.height,
            );
            for (b, _) in &rows {
                left = left.min(b.x);
                top = top.min(b.y);
                right = right.max(b.x + b.width);
                bottom = bottom.max(b.y + b.height);
            }
            let mut heights: Vec<f32> = rows.iter().map(|(b, _)| b.height).collect();
            heights.sort_by(f32::total_cmp);
            let bounds = Bounds {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            };
            (bounds.width > 0. && bounds.height > 0.).then(|| SourceBlock {
                text: text.to_owned(),
                bounds,
                line_height: heights[heights.len() / 2],
                alignment: infer_alignment(&rows.iter().map(|(b, _)| *b).collect::<Vec<_>>()),
                rotated: rows.iter().any(|(_, tilted)| *tilted),
            })
        })
        .collect()
}

fn quad_bounds(quads: &[[Point; 4]]) -> Bounds {
    let mut points = quads.iter().flatten();
    let first = points.next().copied().unwrap_or(Point { x: 0., y: 0. });
    let (mut left, mut top, mut right, mut bottom) = (first.x, first.y, first.x, first.y);
    for p in points {
        left = left.min(p.x);
        top = top.min(p.y);
        right = right.max(p.x);
        bottom = bottom.max(p.y);
    }
    Bounds {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// 四角按左上、右上、右下、左下排列；上沿斜率过大或阅读方向不是从左到右都算倾斜。
fn tilted(quad: &[Point; 4]) -> bool {
    let (dx, dy) = (quad[1].x - quad[0].x, quad[1].y - quad[0].y);
    dx <= 0. || dy.abs() > dx * MAX_SLOPE
}

/// 多行段落左缘对齐为左对齐；左缘参差而中线对齐为居中；单行按左对齐保留原起点。
fn infer_alignment(rows: &[Bounds]) -> Alignment {
    if rows.len() < 2 {
        return Alignment::Left;
    }
    let spread = |values: Vec<f32>| {
        let (min, max) = values
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(*v), b.max(*v)));
        max - min
    };
    let left = spread(rows.iter().map(|r| r.x).collect());
    let center = spread(rows.iter().map(|r| r.x + r.width / 2.).collect());
    let tolerance = rows[0].height.max(1.) * 0.4;
    if left > tolerance && center <= tolerance {
        Alignment::Center
    } else {
        Alignment::Left
    }
}

#[derive(Default)]
struct Scripts {
    han: usize,
    kana: usize,
    hangul: usize,
    latin: usize,
    other: usize,
}

fn scripts(text: &str) -> Scripts {
    let mut s = Scripts::default();
    for c in text.chars() {
        match c as u32 {
            0x3040..=0x30ff | 0x31f0..=0x31ff => s.kana += 1,
            0xac00..=0xd7af | 0x1100..=0x11ff | 0x3130..=0x318f => s.hangul += 1,
            0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x2ffff => s.han += 1,
            _ if c.is_ascii_alphabetic() => s.latin += 1,
            _ if c.is_alphabetic() => s.other += 1,
            _ => {}
        }
    }
    s
}

fn looks_like_code(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("www.") || t.starts_with("$ ") {
        return true;
    }
    let tokens: Vec<&str> = t.split_whitespace().collect();
    // 单个无空格记号：路径、文件名、邮箱、标识符等。
    if tokens.len() == 1 && t.is_ascii() {
        let inner = t.trim_end_matches(['.', ',', ':', ';', '!', '?']);
        if inner.contains(['/', '\\', '@', '_', '=', '<', '>', '{', '}'])
            || inner.split('.').filter(|part| !part.is_empty()).count() > 1
        {
            return true;
        }
    }
    let visible = t.chars().filter(|c| !c.is_whitespace()).count();
    let symbols = t
        .chars()
        .filter(|c| "{}[]()<>;=$|\\/*`_#&~^".contains(*c))
        .count();
    visible >= 6 && symbols * 100 >= visible * 15
}

/// 不需要发送的块及原因；`target` 为设置中的目标语言（`auto` 表示中英互译）。
pub(crate) fn skip_reason(block: &SourceBlock, target: &str) -> Option<Skip> {
    if block.rotated {
        return Some(Skip::Rotated);
    }
    let s = scripts(&block.text);
    let letters = s.han + s.kana + s.hangul + s.latin + s.other;
    if letters == 0 {
        return Some(Skip::NoLetters);
    }
    if looks_like_code(&block.text) {
        return Some(Skip::Code);
    }
    let target = target.trim().to_ascii_lowercase();
    let same = match target.as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-sg" | "zh-tw" | "zh-hk" | "zh-hant" => {
            s.kana == 0 && s.hangul == 0 && s.han * 10 >= letters * 6
        }
        "en" | "en-us" | "en-gb" => s.latin == letters,
        "ja" => s.kana > 0,
        "ko" => s.hangul * 2 >= letters,
        _ => false,
    };
    same.then_some(Skip::SameLanguage)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Colors {
    /// 0xRRGGBB，与 `Style::color` 同格式。
    pub background: u32,
    pub ink: u32,
}

fn pixel(image: &Image, x: i64, y: i64) -> Option<[u8; 3]> {
    if x < 0 || y < 0 || x >= i64::from(image.width) || y >= i64::from(image.height) {
        return None;
    }
    let i = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra.get(i..i + 3).map(|p| [p[2], p[1], p[0]])
}

fn median(histogram: &[u32; 256], count: u32) -> u8 {
    let mut seen = 0;
    for (value, n) in histogram.iter().enumerate() {
        seen += n;
        if seen * 2 >= count {
            return value as u8;
        }
    }
    0
}

fn rgb(c: [u8; 3]) -> u32 {
    (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2])
}

/// `image` 的左上角位于画布 `origin`；底色取行框外 1–3px 环带的逐通道中位数，
/// 文字色取框内与底色差异最大的一批像素均值。
pub(crate) fn sample_colors(image: &Image, origin: (f32, f32), bounds: Bounds) -> Colors {
    let left = (bounds.x - origin.0).floor() as i64;
    let top = (bounds.y - origin.1).floor() as i64;
    let right = (bounds.x + bounds.width - origin.0).ceil() as i64;
    let bottom = (bounds.y + bounds.height - origin.1).ceil() as i64;
    let mut histograms = [[0u32; 256]; 3];
    let mut count = 0;
    for y in top - 3..bottom + 3 {
        for x in left - 3..right + 3 {
            let inside = x >= left - 1 && x < right + 1 && y >= top - 1 && y < bottom + 1;
            if inside {
                continue;
            }
            if let Some(p) = pixel(image, x, y) {
                for c in 0..3 {
                    histograms[c][p[c] as usize] += 1;
                }
                count += 1;
            }
        }
    }
    let mut inner = Vec::new();
    for y in top.max(0)..bottom.min(i64::from(image.height)) {
        for x in left.max(0)..right.min(i64::from(image.width)) {
            if let Some(p) = pixel(image, x, y) {
                inner.push(p);
            }
        }
    }
    if count == 0 {
        // 段落贴着图片边缘：退回框内中位色。
        for p in &inner {
            for c in 0..3 {
                histograms[c][p[c] as usize] += 1;
            }
        }
        count = inner.len() as u32;
    }
    let background = if count == 0 {
        [255, 255, 255]
    } else {
        [0, 1, 2].map(|c| median(&histograms[c], count))
    };
    let distance = |p: &[u8; 3]| -> u32 {
        (0..3)
            .map(|c| (i32::from(p[c]) - i32::from(background[c])).pow(2) as u32)
            .sum()
    };
    let farthest = inner.iter().map(distance).max().unwrap_or(0);
    let ink = if farthest < 40 * 40 {
        contrast(background)
    } else {
        let threshold = farthest * 9 / 25; // 与底色距离不少于最远者的 60%。
        let (mut sum, mut n) = ([0u64; 3], 0u64);
        for p in inner.iter().filter(|p| distance(p) >= threshold) {
            for c in 0..3 {
                sum[c] += u64::from(p[c]);
            }
            n += 1;
        }
        [0, 1, 2].map(|c| (sum[c] / n.max(1)) as u8)
    };
    Colors {
        background: rgb(background),
        ink: rgb(ink),
    }
}

fn contrast(background: [u8; 3]) -> [u8; 3] {
    let luminance = 0.2126 * f32::from(background[0])
        + 0.7152 * f32::from(background[1])
        + 0.0722 * f32::from(background[2]);
    if luminance > 140. {
        [0x1f, 0x23, 0x28]
    } else {
        [0xf2, 0xf4, 0xf6]
    }
}

pub(crate) struct Fitted {
    pub annotation: Annotation,
    /// 最小字号仍放不下，底色框已向下延伸。
    pub overflow: bool,
}

/// 生成覆盖原文的文字标注：底色不透明、无圆角，字号从原行高开始逐级缩小直到放下。
pub(crate) fn fitted_text(block: &SourceBlock, text: &str, colors: Colors) -> Result<Fitted> {
    let text = text.trim().replace("\r\n", "\n").replace('\r', "\n");
    let mut style = Style {
        color: colors.ink,
        opacity: 1.,
        font_size: (block.line_height * 0.8).round().clamp(MIN_FONT, MAX_FONT),
        ..Style::default()
    };
    style.text.alignment = block.alignment;
    style.text.background = true;
    style.text.background_color = colors.background;
    style.text.background_opacity = 1.;
    style.text.background_radius = 0.;
    let start = Point {
        x: block.bounds.x - COVER_MARGIN,
        y: block.bounds.y - COVER_MARGIN,
    };
    let end = Point {
        x: block.bounds.x + block.bounds.width + COVER_MARGIN,
        y: block.bounds.y + block.bounds.height + COVER_MARGIN,
    };
    let mut annotation = Annotation::new(Tool::Text, start, style);
    annotation.update(end);
    annotation.text = text;
    let box_height = end.y - start.y;
    let available = end.x - start.x - annotation.style.text.padding() * 2.;
    loop {
        let layout = crate::text_layout::TextLayout::new(&annotation)?;
        let fits = layout.height <= box_height + 0.5
            && layout.rows.iter().all(|row| row.width <= available + 0.5);
        if fits {
            return Ok(Fitted {
                annotation,
                overflow: false,
            });
        }
        if annotation.style.font_size <= MIN_FONT {
            let height = layout.height.max(box_height);
            if let Some(last) = annotation.points.last_mut() {
                last.y = start.y + height;
            }
            return Ok(Fitted {
                annotation,
                overflow: true,
            });
        }
        annotation.style.font_size = (annotation.style.font_size - 1.).max(MIN_FONT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(x: f32, y: f32, w: f32, h: f32) -> [Point; 4] {
        [
            Point { x, y },
            Point { x: x + w, y },
            Point { x: x + w, y: y + h },
            Point { x, y: y + h },
        ]
    }

    fn block(text: &str) -> SourceBlock {
        SourceBlock {
            text: text.into(),
            bounds: Bounds {
                x: 10.,
                y: 10.,
                width: 200.,
                height: 20.,
            },
            line_height: 20.,
            alignment: Alignment::Left,
            rotated: false,
        }
    }

    #[test]
    fn paragraphs_merge_row_quads_and_skip_empty_or_missing_geometry() {
        let quads = vec![
            vec![quad(100., 50., 300., 20.)],
            vec![quad(100., 74., 280., 22.)],
            vec![],
            vec![quad(-40., 120., 60., 18.)],
        ];
        let blocks = blocks_from_paragraphs(
            &[0..2, 2..3, 3..4],
            &["Two lines", "missing", "Neg"],
            &quads,
        );
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0].bounds,
            Bounds {
                x: 100.,
                y: 50.,
                width: 300.,
                height: 46.
            }
        );
        assert_eq!(blocks[0].line_height, 22.);
        assert_eq!(blocks[0].alignment, Alignment::Left);
        assert_eq!(blocks[1].bounds.x, -40.);
        assert!(!blocks[1].rotated);
    }

    #[test]
    fn centered_rows_and_tilted_quads_are_detected() {
        let centered = blocks_from_paragraphs(
            &[0..2],
            &["a b"],
            &[
                vec![quad(100., 0., 200., 20.)],
                vec![quad(150., 24., 100., 20.)],
            ],
        );
        assert_eq!(centered[0].alignment, Alignment::Center);
        let tilted = [
            Point { x: 0., y: 0. },
            Point { x: 100., y: 30. },
            Point { x: 95., y: 50. },
            Point { x: -5., y: 20. },
        ];
        let rotated = blocks_from_paragraphs(&[0..1], &["tilt"], &[vec![tilted]]);
        assert_eq!(skip_reason(&rotated[0], "auto"), Some(Skip::Rotated));
    }

    #[test]
    fn skip_rules_keep_prose_and_drop_code_numbers_and_same_language() {
        for prose in [
            "Export presets let you save the size.",
            "Save preset",
            "Settings",
            "e.g. Web 2x",
        ] {
            assert_eq!(skip_reason(&block(prose), "zh-CN"), None, "{prose}");
        }
        for code in [
            "https://example.com/docs",
            "./shots/*.png",
            "$ lumen export --preset \"Web 2x\"",
            "user@example.com",
            "config.toml",
            "fn main() { let x = a[0]; }",
        ] {
            assert_eq!(
                skip_reason(&block(code), "auto"),
                Some(Skip::Code),
                "{code}"
            );
        }
        assert_eq!(
            skip_reason(&block("2026-10-12 12:30"), "auto"),
            Some(Skip::NoLetters)
        );
        assert_eq!(
            skip_reason(&block("使用导出预设"), "zh-CN"),
            Some(Skip::SameLanguage)
        );
        assert_eq!(skip_reason(&block("使用导出预设"), "auto"), None);
        assert_eq!(
            skip_reason(&block("Save preset"), "en"),
            Some(Skip::SameLanguage)
        );
        assert_eq!(skip_reason(&block("プリセットを保存"), "zh-CN"), None);
        assert_eq!(
            skip_reason(&block("プリセットを保存"), "ja"),
            Some(Skip::SameLanguage)
        );
    }

    fn image(width: u32, height: u32, background: [u8; 3]) -> Image {
        let mut bgra = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            bgra.extend_from_slice(&[background[2], background[1], background[0], 255]);
        }
        Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        }
    }

    fn paint(image: &mut Image, x: u32, y: u32, w: u32, h: u32, color: [u8; 3]) {
        for yy in y..y + h {
            for xx in x..x + w {
                let i = ((yy * image.width + xx) * 4) as usize;
                image.bgra[i..i + 3].copy_from_slice(&[color[2], color[1], color[0]]);
            }
        }
    }

    #[test]
    fn colors_sample_ring_background_and_strongest_ink_with_canvas_origin() {
        // 画布原点 (-100, 50)：块框位于图内 (20,20) 起。
        let mut img = image(120, 80, [0x2a, 0x24, 0x15]);
        paint(&mut img, 24, 24, 30, 8, [0xe9, 0xd8, 0xa6]);
        paint(&mut img, 60, 26, 2, 2, [0x80, 0x70, 0x50]); // 抗锯齿边缘
        let bounds = Bounds {
            x: -80.,
            y: 70.,
            width: 50.,
            height: 16.,
        };
        let colors = sample_colors(&img, (-100., 50.), bounds);
        assert_eq!(colors.background, 0x2a2415);
        assert_eq!(colors.ink, 0xe9d8a6);
    }

    #[test]
    fn uniform_box_uses_contrasting_ink_and_edge_box_falls_back_inside() {
        let img = image(40, 20, [250, 250, 250]);
        let colors = sample_colors(
            &img,
            (0., 0.),
            Bounds {
                x: -10.,
                y: -10.,
                width: 60.,
                height: 40.,
            },
        );
        assert_eq!(colors.background, 0xfafafa);
        assert_eq!(colors.ink, 0x1f2328);
    }

    #[test]
    fn fitted_text_covers_source_and_shrinks_or_extends() {
        let colors = Colors {
            background: 0x161a1f,
            ink: 0xbec7d0,
        };
        let short = block("Save");
        let fitted = fitted_text(&short, "保存", colors).unwrap();
        let a = &fitted.annotation;
        assert!(!fitted.overflow);
        assert_eq!(a.tool, Tool::Text);
        assert!(a.style.text.background);
        assert_eq!(a.style.text.background_color, 0x161a1f);
        assert_eq!(a.style.text.background_opacity, 1.);
        assert_eq!(a.style.color, 0xbec7d0);
        let b = a.local_bounds();
        assert!(b.x <= short.bounds.x && b.y <= short.bounds.y);
        assert!(b.x + b.width >= short.bounds.x + short.bounds.width);
        assert!(b.y + b.height >= short.bounds.y + short.bounds.height);

        let long = "很长的译文".repeat(12);
        let shrunk = fitted_text(&short, &long, colors).unwrap();
        assert!(shrunk.annotation.style.font_size < fitted.annotation.style.font_size);
        assert!(shrunk.overflow);
        assert!(shrunk.annotation.local_bounds().height > short.bounds.height + 6.);
    }
}
