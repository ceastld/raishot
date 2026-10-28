//! 公式的进程内数学排版与有界预览；不启动外部程序，不读写截图文档。
use crate::capture::Image;
use ratex_layout::{FallbackGlyphMetrics, LayoutOptions};
use ratex_types::{Color, DisplayItem};
use std::{
    collections::{BTreeSet, HashMap},
    io::Cursor,
    sync::Arc,
};

const MAX_SIDE: u32 = 8192;
const MAX_PIXELS: u64 = 12_000_000;
const MAX_ITEMS: usize = 40_000;
const PADDING: f32 = 12.0;

/// 排版已从 Markdown 分离的单个数学段，返回供 GPUI 显示的 BGRA。
/// 深浅主题只用于预览；复制图片时调用方固定传 false。
pub(crate) fn render(latex: &str, dark: bool, font_size: f32) -> Result<Image, String> {
    let latex = crate::formula::normalize_latex(latex)?;
    if !font_size.is_finite() || !(8.0..=128.0).contains(&font_size) {
        return Err(tr!("公式字号无效", "Invalid formula font size").into());
    }
    let nodes = ratex_parser::parse(&latex).map_err(|_| {
        tr!(
            "当前公式语法无法排版，请编辑LaTeX后重试",
            "This formula syntax cannot be rendered; edit the LaTeX and retry"
        )
        .to_owned()
    })?;
    let chinese: BTreeSet<char> = latex
        .chars()
        .filter(|ch| crate::formula::is_cjk(*ch) && !ch.is_whitespace())
        .collect();
    let metrics = chinese_metrics(&chinese)?;
    let color = if dark {
        Color::rgb(0.92, 0.93, 0.95)
    } else {
        Color::BLACK
    };
    let options = LayoutOptions {
        color,
        ..Default::default()
    };
    let tree = ratex_layout::layout_with_fallback_metrics(&nodes, &options, Arc::new(metrics));
    ratex_layout::validate_layout(&tree).map_err(|_| too_large())?;
    checked_dimensions(tree.width, tree.height + tree.depth, font_size)?;
    let display = ratex_layout::to_display_list(&tree);
    if display.items.is_empty() {
        return Err(tr!(
            "公式没有可见内容，请校对LaTeX",
            "The formula has no visible content; review the LaTeX"
        )
        .into());
    }
    if display.items.len() > MAX_ITEMS {
        return Err(too_large());
    }
    checked_dimensions(display.width, display.height + display.depth, font_size)?;
    // 控制序列也可能生成中文码位，不能只检查输入字符串里的字面字符。
    let drawn_chinese: BTreeSet<char> = display
        .items
        .iter()
        .filter_map(|item| {
            if let DisplayItem::GlyphPath { char_code, .. } = item {
                char::from_u32(*char_code)
                    .filter(|ch| crate::formula::is_cjk(*ch) && !ch.is_whitespace())
            } else {
                None
            }
        })
        .collect();
    let _ = chinese_metrics(&drawn_chinese)?;
    let png = ratex_render::render_to_png(
        &display,
        &ratex_render::RenderOptions {
            font_size,
            padding: PADDING,
            background_color: if dark {
                Color::rgb(0.10, 0.11, 0.13)
            } else {
                Color::WHITE
            },
            device_pixel_ratio: 1.0,
            ..Default::default()
        },
    )
    .map_err(|_| {
        tr!(
            "公式绘制失败，已保留LaTeX供编辑",
            "Formula rendering failed; the LaTeX is preserved for editing"
        )
        .to_owned()
    })?;
    decode_preview(&png)
}

fn too_large() -> String {
    tr!(
        "公式排版尺寸或复杂度过大，请分段或缩小字号",
        "The formula layout is too large or complex; split it or reduce the font size"
    )
    .into()
}

fn checked_dimensions(width_em: f64, height_em: f64, size: f32) -> Result<(u32, u32), String> {
    let width = (width_em * f64::from(size) + 2.0 * f64::from(PADDING)).ceil();
    let height = (height_em * f64::from(size) + 2.0 * f64::from(PADDING)).ceil();
    if !width_em.is_finite()
        || !height_em.is_finite()
        || width_em < 0.0
        || height_em < 0.0
        || !width.is_finite()
        || !height.is_finite()
        || width < 1.0
        || height < 1.0
        || width > f64::from(MAX_SIDE)
        || height > f64::from(MAX_SIDE)
        || width * height > MAX_PIXELS as f64
    {
        return Err(too_large());
    }
    Ok((width as u32, height as u32))
}

fn chinese_metrics(chars: &BTreeSet<char>) -> Result<HashMap<char, FallbackGlyphMetrics>, String> {
    let mut metrics = HashMap::new();
    if chars.is_empty() {
        return Ok(metrics);
    }
    let primary = ratex_unicode_font::unicode_font_data_ref().and_then(|data| {
        ttf_parser::Face::parse(
            data.as_slice(),
            ratex_unicode_font::unicode_font_face_index().unwrap_or(0),
        )
        .ok()
    });
    let fallback = ratex_unicode_font::fallback_font_data_ref().and_then(|data| {
        ttf_parser::Face::parse(
            data.as_slice(),
            ratex_unicode_font::fallback_font_face_index().unwrap_or(0),
        )
        .ok()
    });
    for &ch in chars {
        let measured = [primary.as_ref(), fallback.as_ref()]
            .into_iter()
            .flatten()
            .find_map(|face| {
                let glyph = face.glyph_index(ch)?;
                if glyph.0 == 0 {
                    return None;
                }
                let bounds = face.glyph_bounding_box(glyph)?;
                if bounds.x_max <= bounds.x_min || bounds.y_max <= bounds.y_min {
                    return None;
                }
                let units = f64::from(face.units_per_em());
                if units <= 0.0 {
                    return None;
                }
                Some(FallbackGlyphMetrics {
                    width: f64::from(face.glyph_hor_advance(glyph)?) / units,
                    height: f64::from(bounds.y_max).max(0.0) / units,
                    depth: (-f64::from(bounds.y_min)).max(0.0) / units,
                })
            });
        let Some(measured) = measured else {
            return Err(tr!("当前系统字体无法可靠显示公式中的中文，已保留LaTeX供编辑", "The system fonts cannot reliably display the formula's Chinese text; the LaTeX is preserved for editing").into());
        };
        metrics.insert(ch, measured);
    }
    Ok(metrics)
}

fn decode_preview(bytes: &[u8]) -> Result<Image, String> {
    let invalid = || tr!("公式预览图片无效", "Invalid formula preview image").to_owned();
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| invalid())?;
    let size = reader.output_buffer_size().ok_or_else(invalid)?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
        || size as u64 > MAX_PIXELS * 4
    {
        return Err(too_large());
    }
    let mut decoded = vec![0; size];
    let frame = reader.next_frame(&mut decoded).map_err(|_| invalid())?;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        _ => return Err(invalid()),
    };
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in decoded[..frame.buffer_size()].chunks_exact(channels) {
        bgra.extend_from_slice(&[
            pixel[2],
            pixel[1],
            pixel[0],
            if channels == 4 { pixel[3] } else { 255 },
        ]);
    }
    if bgra.len() != width as usize * height as usize * 4 {
        return Err(invalid());
    }
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.0,
    })
}

#[cfg(test)]
#[path = "formula_layout_tests.rs"]
mod tests;
