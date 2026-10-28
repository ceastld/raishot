//! 复杂标注只生成局部 BGRA 位图；预览和导出复用同一份像素。
use crate::{
    annotation::{distance_to_segment, Annotation, Point, Tool},
    capture::Image,
    geometry::Rect,
    scene::{Scene, SourcePixels},
    Result,
};
use windows::Win32::{Foundation::*, Graphics::Gdi::*};

pub fn empty(rect: Rect) -> Image {
    Image {
        width: rect.width,
        height: rect.height,
        bgra: vec![0; rect.width as usize * rect.height as usize * 4],
        readback_ms: 0.,
    }
}
pub fn composite(dst: &mut Image, region: Rect, src: &Image, rect: Rect) {
    let Some(part) = region.intersection(rect) else {
        return;
    };
    for y in part.y..part.y + part.height {
        for x in part.x..part.x + part.width {
            let d = (((y - region.y) * dst.width + x - region.x) as usize) * 4;
            let s = (((y - rect.y) * src.width + x - rect.x) as usize) * 4;
            let alpha = src.bgra[s + 3] as f32 / 255.;
            over_pixel(&mut dst.bgra[d..d + 4], &src.bgra[s..s + 3], alpha);
        }
    }
}
pub fn region(a: &Annotation, canvas: Rect) -> Option<Rect> {
    let b = a.render_bounds();
    let margin = if a.fill_only() {
        2.
    } else if a.tool.freehand() {
        a.style.width / 2. + 2.
    } else {
        a.style.width.max(if a.tool.text() && a.style.text.outline {
            a.style.text.outline_width
        } else {
            0.
        }) + 2.
    };
    Rect::drag(
        ((b.x - margin).floor() as i32, (b.y - margin).floor() as i32),
        (
            (b.x + b.width + margin).ceil() as i32,
            (b.y + b.height + margin).ceil() as i32,
        ),
        canvas.width,
        canvas.height,
    )
    .intersection(canvas)
}
// 先合并单个对象内的覆盖率，再混色。路径交点不会重复叠加透明度。
fn stroke_mask(a: &Annotation, rect: Rect) -> Vec<u8> {
    let mut mask = if a.tool == Tool::Arrow && a.style.dashed {
        let path = a.vector_path();
        let body = crate::vector_path::VectorPath {
            spans: path.spans.iter().take(1).copied().collect(),
            ..Default::default()
        }
        .flattened();
        let heads = crate::vector_path::VectorPath {
            spans: path.spans.iter().skip(1).copied().collect(),
            ..Default::default()
        }
        .flattened();
        let mut mask = stroke_mask_segments(&body, a.style.width, true, rect);
        let head_mask = stroke_mask_segments(&heads, a.style.width, false, rect);
        // 虚线只作用于箭杆，箭头始终完整；覆盖合并后只叠一次透明度。
        for (pixel, head) in mask.iter_mut().zip(head_mask) {
            *pixel = (*pixel).max(head);
        }
        mask
    } else {
        stroke_mask_segments(&a.segments(), a.style.width, a.style.dashed, rect)
    };
    if a.tool == Tool::Arrow {
        merge_fills(&mut mask, rect, &a.vector_path().fills);
    }
    mask
}
fn merge_fills(mask: &mut [u8], rect: Rect, contours: &[Vec<Point>]) {
    for contour in contours {
        let edges = crate::vector_path::contour_edges(contour);
        if edges.is_empty() {
            continue;
        }
        let (left, top, right, bottom) = contour.iter().fold(
            (
                f32::INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::NEG_INFINITY,
            ),
            |(l, t, r, b), p| (l.min(p.x), t.min(p.y), r.max(p.x), b.max(p.y)),
        );
        let left = (left - 1.).floor().max(rect.x as f32) as u32;
        let top = (top - 1.).floor().max(rect.y as f32) as u32;
        let right = (right + 1.)
            .ceil()
            .max(0.)
            .min((rect.x + rect.width) as f32) as u32;
        let bottom = (bottom + 1.)
            .ceil()
            .max(0.)
            .min((rect.y + rect.height) as f32) as u32;
        for y in top..bottom {
            for x in left..right {
                let i = ((y - rect.y) * rect.width + x - rect.x) as usize;
                if mask[i] == 255 {
                    continue;
                }
                let p = Point {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                };
                let distance = edges
                    .iter()
                    .map(|&(a, b)| distance_to_segment(p, a, b))
                    .fold(f32::INFINITY, f32::min);
                let signed = if crate::annotation::contains_path(p, &edges) {
                    distance
                } else {
                    -distance
                };
                let coverage = ((0.5 + signed).clamp(0., 1.) * 255.).round() as u8;
                mask[i] = mask[i].max(coverage);
            }
        }
    }
}
fn stroke_mask_segments(
    segments: &[(Point, Point)],
    width: f32,
    dashed: bool,
    rect: Rect,
) -> Vec<u8> {
    let mut mask = vec![0; rect.width as usize * rect.height as usize];
    let radius = width / 2.;
    let mut distance = 0.;
    for &(start, end) in segments {
        let left = (start.x.min(end.x) - radius - 1.)
            .floor()
            .max(rect.x as f32) as u32;
        let top = (start.y.min(end.y) - radius - 1.)
            .floor()
            .max(rect.y as f32) as u32;
        let right = (start.x.max(end.x) + radius + 1.)
            .ceil()
            .max(0.)
            .min((rect.x + rect.width) as f32) as u32;
        let bottom = (start.y.max(end.y) + radius + 1.)
            .ceil()
            .max(0.)
            .min((rect.y + rect.height) as f32) as u32;
        let len = start.distance(end);
        for y in top..bottom {
            for x in left..right {
                let p = Point {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                };
                if dashed && len > 0. {
                    let along = ((p.x - start.x) * (end.x - start.x)
                        + (p.y - start.y) * (end.y - start.y))
                        / len;
                    if (distance + along).rem_euclid(width * 5. + 4.) > width * 3. + 2. {
                        continue;
                    }
                }
                let coverage = ((radius + 0.5 - distance_to_segment(p, start, end)).clamp(0., 1.)
                    * 255.)
                    .round() as u8;
                let offset = ((y - rect.y) * rect.width + x - rect.x) as usize;
                mask[offset] = mask[offset].max(coverage);
            }
        }
        distance += len;
    }
    mask
}
/// 矢量铅笔的局部覆盖率。交叉及细分结点只混合一次，避免抗锯齿边缘反复加深。
fn vector_mask(
    region: Rect,
    path: &crate::vector_path::VectorPath,
    segments: &[(Point, Point)],
    style: crate::annotation::Style,
    arrow: bool,
) -> Option<(Rect, Vec<u8>)> {
    let bounds = path.bounds()?;
    let margin = style.width / 2. + 1.;
    let (left, top, right, bottom) = (
        bounds.x,
        bounds.y,
        bounds.x + bounds.width,
        bounds.y + bounds.height,
    );
    let x = (left - margin).floor().max(region.x as f32) as u32;
    let y = (top - margin).floor().max(region.y as f32) as u32;
    let right = (right + margin)
        .ceil()
        .min((region.x + region.width) as f32)
        .max(0.) as u32;
    let bottom = (bottom + margin)
        .ceil()
        .min((region.y + region.height) as f32)
        .max(0.) as u32;
    if x >= right || y >= bottom {
        return None;
    }
    let rect = Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    };
    let mut mask = if arrow && style.dashed {
        let body = crate::vector_path::VectorPath {
            spans: path.spans.iter().take(1).copied().collect(),
            ..Default::default()
        }
        .flattened();
        let heads = crate::vector_path::VectorPath {
            spans: path.spans.iter().skip(1).copied().collect(),
            ..Default::default()
        }
        .flattened();
        let mut mask = stroke_mask_segments(&body, style.width, true, rect);
        let solid = stroke_mask_segments(&heads, style.width, false, rect);
        for (pixel, head) in mask.iter_mut().zip(solid) {
            *pixel = (*pixel).max(head);
        }
        mask
    } else {
        stroke_mask_segments(segments, style.width, style.dashed, rect)
    };

    merge_fills(&mut mask, rect, &path.fills);
    Some((rect, mask))
}
pub fn paint_vector(
    image: &mut Image,
    region: Rect,
    path: &crate::vector_path::VectorPath,
    segments: &[(Point, Point)],
    style: crate::annotation::Style,
    arrow: bool,
) {
    let Some((rect, mask)) = vector_mask(region, path, segments, style, arrow) else {
        return;
    };
    for row in 0..rect.height {
        for col in 0..rect.width {
            let coverage = mask[(row * rect.width + col) as usize] as f32 / 255. * style.opacity;
            if coverage == 0. {
                continue;
            }
            let offset =
                (((row + rect.y - region.y) * image.width + col + rect.x - region.x) * 4) as usize;
            let color = [
                style.color as u8,
                (style.color >> 8) as u8,
                (style.color >> 16) as u8,
            ];
            over_pixel(&mut image.bgra[offset..offset + 4], &color, coverage);
        }
    }
}
/// 画刷保留原始底图 alpha；不把矢量轮廓/opacity 烘焙到图片里。
#[allow(clippy::too_many_arguments)]
pub fn paint_vector_image(
    image: &mut Image,
    region: Rect,
    path: &crate::vector_path::VectorPath,
    segments: &[(Point, Point)],
    style: crate::annotation::Style,
    clip_rect: Rect,
    brush_rect: Rect,
    brush: &Image,
) {
    let Some(mask_region) = region.intersection(clip_rect) else {
        return;
    };
    let Some((rect, mask)) = vector_mask(mask_region, path, segments, style, false) else {
        return;
    };
    let Some(part) = rect.intersection(brush_rect) else {
        return;
    };
    for y in part.y..part.y + part.height {
        for x in part.x..part.x + part.width {
            let coverage = mask[((y - rect.y) * rect.width + x - rect.x) as usize] as f32 / 255.;
            let source = (((y - brush_rect.y) * brush.width + x - brush_rect.x) * 4) as usize;
            let alpha =
                coverage * style.opacity.clamp(0., 1.) * brush.bgra[source + 3] as f32 / 255.;
            if alpha == 0. {
                continue;
            }
            let destination = (((y - region.y) * image.width + x - region.x) * 4) as usize;
            over_pixel(
                &mut image.bgra[destination..destination + 4],
                &brush.bgra[source..source + 3],
                alpha,
            );
        }
    }
}
pub fn shape(a: &Annotation, canvas: Rect) -> Option<(Rect, Image)> {
    let rect = region(a, canvas)?;
    let mut image = empty(rect);
    let b = a.local_bounds();
    let inverse = a.rotation_transform().inverse();
    let filled = a.tool == Tool::Serial
        || a.tool == Tool::HighlighterRect
        || a.style.filled && (a.tool.closed_shape() || a.tool == Tool::Polyline);
    let mut mask = if a.fill_only() {
        vec![0; rect.width as usize * rect.height as usize]
    } else {
        stroke_mask(a, rect)
    };
    let contour = if a.tool == Tool::Polyline {
        a.segments()
    } else {
        Vec::new()
    };
    if filled && (matches!(a.tool, Tool::Rectangle | Tool::HighlighterRect) || a.tool.polygon()) {
        // 填充与描边使用同一旋转圆角轮廓，覆盖先合并、透明度只应用一次。
        // 不另用像素中心的二值圆角判断，保持矢量导出边缘的抗锯齿覆盖率。
        merge_fills(&mut mask, rect, &a.vector_path().fills);
    } else if filled {
        for y in 0..rect.height {
            for x in 0..rect.width {
                let world = Point {
                    x: (x + rect.x) as f32 + 0.5,
                    y: (y + rect.y) as f32 + 0.5,
                };
                let p = inverse.map(world);
                let inside = if a.tool == Tool::Polyline {
                    a.points.len() >= 3 && crate::annotation::contains_path(world, &contour)
                } else if matches!(a.tool, Tool::Serial | Tool::Ellipse) {
                    let rx = b.width / 2.;
                    let ry = b.height / 2.;
                    ((p.x - b.x - rx) / rx).powi(2) + ((p.y - b.y - ry) / ry).powi(2) <= 1.
                } else {
                    let radius = a.style.radius.min(b.width / 2.).min(b.height / 2.);
                    let nx = p.x.clamp(b.x + radius, b.x + b.width - radius);
                    let ny = p.y.clamp(b.y + radius, b.y + b.height - radius);
                    b.contains(p, 0.) && (radius == 0. || (p.x - nx).hypot(p.y - ny) <= radius)
                };
                if inside {
                    mask[(y * rect.width + x) as usize] = 255;
                }
            }
        }
    }
    for (coverage, pixel) in mask.iter().zip(image.bgra.chunks_exact_mut(4)) {
        pixel[..3].copy_from_slice(&[
            a.style.color as u8,
            (a.style.color >> 8) as u8,
            (a.style.color >> 16) as u8,
        ]);
        pixel[3] = (*coverage as f32 * a.style.opacity).round() as u8;
    }
    Some((rect, image))
}
// RAII 释放 GDI 对象，且先还原被选入 DC 的旧对象。
pub(crate) struct TextSurface {
    pub(crate) dc: HDC,
    bitmap: HBITMAP,
    font: HFONT,
    old_bitmap: HGDIOBJ,
    old_font: HGDIOBJ,
    bits: *mut u8,
    byte_len: usize,
    _native: crate::text_format::NativeTextLease,
}
impl Drop for TextSurface {
    fn drop(&mut self) {
        unsafe {
            let _ = GdiFlush();
            SelectObject(self.dc, self.old_font);
            SelectObject(self.dc, self.old_bitmap);
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
impl TextSurface {
    pub(crate) fn new(
        width: u32,
        height: u32,
        style: crate::annotation::Style,
        angle: i32,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err("文字绘制尺寸无效".into());
        }
        let byte_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("文字绘制尺寸溢出")?;
        let native = crate::text_format::native_text_lease();
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("无法创建文字绘制设备".into());
            }
            let info = crate::gdi_surface::bitmap_info(width, height);
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(b) => b,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e.into());
                }
            };
            let font = match crate::text_format::create_font(style, angle) {
                Ok(font) => font,
                Err(e) => {
                    let _ = DeleteObject(bitmap.into());
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            if bits.is_null() {
                let _ = DeleteObject(font.into());
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return Err("文字绘制位图没有像素缓冲".into());
            }
            let old_bitmap = SelectObject(dc, bitmap.into());
            let old_font = SelectObject(dc, font.into());
            let _ = SetBkMode(dc, TRANSPARENT);
            let _ = SetTextColor(dc, COLORREF(0xffffff));
            std::ptr::write_bytes(bits, 0, byte_len);
            Ok(Self {
                dc,
                bitmap,
                font,
                old_bitmap,
                old_font,
                bits: bits.cast(),
                byte_len,
                _native: native,
            })
        }
    }
    // 输出分块复用同一 HDC，字形分析与输出始终留在同一线程/设备。
    pub(crate) fn clear(&self) -> Result<()> {
        unsafe {
            GdiFlush().ok()?;
            std::ptr::write_bytes(self.bits, 0, self.byte_len);
        }
        Ok(())
    }
    pub(crate) fn coverage(&self) -> Result<Vec<u8>> {
        unsafe {
            GdiFlush().ok()?;
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.bits, self.byte_len) };
        let mut mask = Vec::new();
        mask.try_reserve_exact(self.byte_len / 4)?;
        mask.extend(bytes.chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2])));
        Ok(mask)
    }
}
const MAX_ROTATED_TEXT_SCRATCH: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq)]
enum RotatedTextPlan {
    Local {
        rect: Rect,
        local_canvas: Rect,
        pad: f32,
    },
    Projected {
        rect: Rect,
    },
}

fn rotated_text_plan(a: &Annotation, canvas: Rect) -> Result<Option<RotatedTextPlan>> {
    let b = a.local_bounds();
    let pad = a.style.width.max(if a.style.text.outline {
        a.style.text.outline_width
    } else {
        0.
    }) + 3.;
    if ![b.x, b.y, b.width, b.height, pad, a.rotation]
        .iter()
        .all(|value| value.is_finite())
        || pad < 0.
    {
        return Err("旋转文字局部绘制尺寸无效".into());
    }
    // 全外对象连局部 DIB 都不生成；标注几何和文档仍原样保留。
    let Some(rect) = region(a, canvas) else {
        return Ok(None);
    };
    let width = (b.width + pad * 2.).ceil().max(1.);
    let height = (b.height + pad * 2.).ceil().max(1.);
    // DIB 4 + 输出 4 + mask 1 + EDT 4 + outline 1；另留行变换临时空间。
    let scratch = (width as u64)
        .checked_mul(height as u64)
        .and_then(|pixels| pixels.checked_mul(14))
        .and_then(|bytes| {
            (width.max(height) as u64)
                .checked_mul(32)?
                .checked_add(bytes)
        });
    if matches!(a.tool, Tool::Text | Tool::Serial)
        && (width > i32::MAX as f32
            || height > i32::MAX as f32
            || scratch.is_none_or(|bytes| bytes > MAX_ROTATED_TEXT_SCRATCH))
    {
        let projection = rotated_text_projection(rect);
        crate::projected_text::validate(a, &projection)?;
        return Ok(Some(RotatedTextPlan::Projected { rect }));
    }
    Ok(Some(RotatedTextPlan::Local {
        rect,
        local_canvas: Rect::full(width as u32, height as u32),
        pad,
    }))
}

fn rotated_text_projection(rect: Rect) -> crate::output_projection::OutputProjection {
    crate::output_projection::OutputProjection {
        source: rect,
        width: rect.width,
        height: rect.height,
        sx: 1.,
        sy: 1.,
        origin: Point { x: 0., y: 0. },
    }
}

pub fn text(a: &Annotation, canvas: Rect) -> Result<Option<(Rect, Image)>> {
    if a.rotation != 0. {
        let Some(plan) = rotated_text_plan(a, canvas)? else {
            return Ok(None);
        };
        let (rect, local_canvas, pad) = match plan {
            RotatedTextPlan::Projected { rect } => {
                // 大对象只在可见原图 ROI 上分块绘制，不分配整块局部文字面。
                // 这一支使用 GDI 世界变换；普通尺寸继续保留旧双线性采样像素。
                let projection = rotated_text_projection(rect);
                let mut image = empty(rect);
                crate::projected_text::paint(a, &mut image, &projection)?;
                return Ok(Some((rect, image)));
            }
            RotatedTextPlan::Local {
                rect,
                local_canvas,
                pad,
            } => (rect, local_canvas, pad),
        };
        // 原始文字在局部面绘制一次，再按同一逆变换采样；边缘先算 alpha，避免黑边。
        let b = a.local_bounds();
        let (dx, dy) = (pad - b.x, pad - b.y);
        let mut local = a.clone();
        local.rotation = 0.;
        local.translate(dx, dy);
        let Some((source_rect, source)) = text(&local, local_canvas)? else {
            return Ok(None);
        };
        let mut image = empty(rect);
        let inverse = a.rotation_transform().inverse();
        for y in 0..rect.height {
            for x in 0..rect.width {
                let q = inverse.map(Point {
                    x: (x + rect.x) as f32 + 0.5,
                    y: (y + rect.y) as f32 + 0.5,
                });
                let (u, v) = (
                    q.x + dx - source_rect.x as f32 - 0.5,
                    q.y + dy - source_rect.y as f32 - 0.5,
                );
                let (ix, iy) = (u.floor() as i32, v.floor() as i32);
                let (fx, fy) = (u - u.floor(), v - v.floor());
                let mut alpha = 0.;
                let mut colors = [0.; 3];
                for (sx, sy, w) in [
                    (ix, iy, (1. - fx) * (1. - fy)),
                    (ix + 1, iy, fx * (1. - fy)),
                    (ix, iy + 1, (1. - fx) * fy),
                    (ix + 1, iy + 1, fx * fy),
                ] {
                    if sx < 0 || sy < 0 || sx >= source.width as i32 || sy >= source.height as i32 {
                        continue;
                    }
                    let i = (sy as usize * source.width as usize + sx as usize) * 4;
                    let coverage = source.bgra[i + 3] as f32 * w;
                    alpha += coverage;
                    for (c, color) in colors.iter_mut().enumerate() {
                        *color += source.bgra[i + c] as f32 * coverage;
                    }
                }
                if alpha > 0. {
                    let i = ((y * rect.width + x) * 4) as usize;
                    for (c, color) in colors.iter().enumerate() {
                        image.bgra[i + c] = (*color / alpha).round() as u8;
                    }
                    image.bgra[i + 3] = alpha.round() as u8;
                }
            }
        }
        return Ok(Some((rect, image)));
    }
    let Some(rect) = region(a, canvas) else {
        return Ok(None);
    };
    let surface = TextSurface::new(
        rect.width,
        rect.height,
        a.style,
        if a.tool == Tool::Watermark { 300 } else { 0 },
    )?;
    let mut utf: Vec<u16> = a.text.encode_utf16().collect();
    unsafe {
        if a.tool == Tool::Watermark {
            let step_x =
                (a.style.font_size * (a.text.chars().count() as f32 + 4.)).max(160.) as i32;
            let step_y = (a.style.font_size * 5.).max(120.) as i32;
            for y in (step_y / 2..rect.height as i32 + step_y).step_by(step_y as usize) {
                for x in (0..rect.width as i32).step_by(step_x as usize) {
                    let _ = TextOutW(surface.dc, x, y, &utf);
                }
            }
        } else if a.tool == Tool::Serial {
            let b = a.bounds();
            let mut r = RECT {
                left: (b.x - rect.x as f32) as i32,
                top: (b.y - rect.y as f32) as i32,
                right: (b.x + b.width - rect.x as f32) as i32,
                bottom: (b.y + b.height - rect.y as f32) as i32,
            };
            DrawTextW(
                surface.dc,
                &mut utf,
                &mut r,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        } else {
            let layout = crate::text_layout::TextLayout::from_dc(a, surface.dc)?;
            let b = a.local_bounds();
            layout.draw(
                (b.x - rect.x as f32).round() as i32,
                (b.y - rect.y as f32).round() as i32,
            )?;
        }
        GdiFlush().ok()?;
        let mask = std::slice::from_raw_parts(
            surface.bits,
            rect.width as usize * rect.height as usize * 4,
        );
        if a.tool != Tool::Serial {
            let mask: Vec<u8> = mask
                .chunks_exact(4)
                .map(|p| p[0].max(p[1]).max(p[2]))
                .collect();
            return Ok(Some((rect, crate::text_paint::paint(a, rect, &mask))));
        }
        let mut image = if a.tool == Tool::Serial {
            shape(a, canvas).unwrap().1
        } else {
            empty(rect)
        };
        for (i, p) in mask.chunks_exact(4).enumerate() {
            let alpha = p[0].max(p[1]).max(p[2]);
            if alpha == 0 {
                continue;
            }
            let color = if a.tool == Tool::Serial {
                0xffffff
            } else {
                a.style.color
            };
            let offset = i * 4;
            if a.tool == Tool::Serial {
                let coverage = alpha as f32 / 255.;
                for c in 0..3 {
                    image.bgra[offset + c] = (255. * coverage
                        + image.bgra[offset + c] as f32 * (1. - coverage))
                        .round() as u8;
                }
            } else {
                image.bgra[offset..offset + 3].copy_from_slice(&[
                    color as u8,
                    (color >> 8) as u8,
                    (color >> 16) as u8,
                ]);
                image.bgra[offset + 3] = (alpha as f32 * a.style.opacity).round() as u8;
            }
        }
        Ok(Some((rect, image)))
    }
}
pub fn try_effect(
    a: &Annotation,
    source: &SourcePixels<'_>,
    below: &Scene,
) -> Result<Option<(Rect, Image)>> {
    let Some(rect) = region(a, source.canvas) else {
        return Ok(None);
    };
    let radius = a.style.block_size.clamp(2, 64);
    let expanded = Rect::drag(
        (
            rect.x as i32 - radius as i32 * 2,
            rect.y as i32 - radius as i32 * 2,
        ),
        (
            (rect.x + rect.width + radius * 2) as i32,
            (rect.y + rect.height + radius * 2) as i32,
        ),
        source.canvas.width,
        source.canvas.height,
    );
    let mut pixels = below.read_composited(source, expanded)?;
    if matches!(a.tool, Tool::Blur | Tool::BlurBrush) {
        for _ in 0..3 {
            box_blur(&mut pixels, (radius / 3).max(1));
        }
    }
    let mut image = empty(rect);
    let mask = if a.tool.freehand() {
        Some(stroke_mask(a, rect))
    } else {
        None
    };
    let effect_geometry = if mask.is_none() {
        a.effect_geometry()
    } else {
        None
    };
    let bs = radius;
    let mut cells = std::collections::HashMap::<(u32, u32), [u8; 3]>::new();
    for y in 0..rect.height {
        for x in 0..rect.width {
            let gx = x + rect.x;
            let gy = y + rect.y;
            let p = Point {
                x: gx as f32 + 0.5,
                y: gy as f32 + 0.5,
            };
            let covered = if let Some(mask) = &mask {
                mask[(y * rect.width + x) as usize] > 0
            } else {
                effect_geometry.as_ref().is_some_and(|g| g.contains(p, 0.))
            };
            if !covered {
                continue;
            }
            let dst = ((y * rect.width + x) as usize) * 4;
            if matches!(a.tool, Tool::MosaicBrush | Tool::Mosaic) {
                let cx = gx / bs * bs;
                let cy = gy / bs * bs;
                let cell = Rect {
                    x: cx,
                    y: cy,
                    width: bs,
                    height: bs,
                }
                .intersection(expanded)
                .ok_or("马赛克采样范围没有交集")?;
                let color = cells.entry((cx, cy)).or_insert_with(|| {
                    let mut sum = [0_u64; 3];
                    for sy in cell.y..cell.y + cell.height {
                        for sx in cell.x..cell.x + cell.width {
                            let offset =
                                (((sy - expanded.y) * pixels.width + sx - expanded.x) as usize) * 4;
                            for (c, total) in sum.iter_mut().enumerate() {
                                *total += pixels.bgra[offset + c] as u64;
                            }
                        }
                    }
                    let n = cell.width as u64 * cell.height as u64;
                    [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
                });
                image.bgra[dst..dst + 3].copy_from_slice(color);
            } else {
                let src = (((gy - expanded.y) * pixels.width + gx - expanded.x) as usize) * 4;
                image.bgra[dst..dst + 3].copy_from_slice(&pixels.bgra[src..src + 3]);
            }
            image.bgra[dst + 3] = 255;
        }
    }
    Ok(Some((rect, image)))
}
pub(crate) fn box_blur(image: &mut Image, radius: u32) {
    let w = image.width as usize;
    let h = image.height as usize;
    let r = radius as usize;
    let mut temp = vec![0; image.bgra.len()];
    for y in 0..h {
        let mut sum = [0_u32; 3];
        for x in 0..=r.min(w - 1) {
            for (c, total) in sum.iter_mut().enumerate() {
                *total += image.bgra[(y * w + x) * 4 + c] as u32;
            }
        }
        for x in 0..w {
            let n = (x + r + 1).min(w) - x.saturating_sub(r);
            for (c, total) in sum.iter().enumerate() {
                temp[(y * w + x) * 4 + c] = (*total / n as u32) as u8;
            }
            if x >= r {
                for (c, total) in sum.iter_mut().enumerate() {
                    *total -= image.bgra[(y * w + x - r) * 4 + c] as u32;
                }
            }
            if x + r + 1 < w {
                for (c, total) in sum.iter_mut().enumerate() {
                    *total += image.bgra[(y * w + x + r + 1) * 4 + c] as u32;
                }
            }
        }
    }
    for x in 0..w {
        let mut sum = [0_u32; 3];
        for y in 0..=r.min(h - 1) {
            for (c, total) in sum.iter_mut().enumerate() {
                *total += temp[(y * w + x) * 4 + c] as u32;
            }
        }
        for y in 0..h {
            let n = (y + r + 1).min(h) - y.saturating_sub(r);
            for (c, total) in sum.iter().enumerate() {
                image.bgra[(y * w + x) * 4 + c] = (*total / n as u32) as u8;
            }
            if y >= r {
                for (c, total) in sum.iter_mut().enumerate() {
                    *total -= temp[((y - r) * w + x) * 4 + c] as u32;
                }
            }
            if y + r + 1 < h {
                for (c, total) in sum.iter_mut().enumerate() {
                    *total += temp[((y + r + 1) * w + x) * 4 + c] as u32;
                }
            }
        }
    }
}

/// 恢复笔迹局部合成：覆盖率合并一次，原图和所有对象均保持不变。
pub fn try_restore(
    a: &Annotation,
    source: &SourcePixels<'_>,
    below: &Scene,
) -> Result<Option<(Rect, Image)>> {
    let Some(rect) = region(a, source.canvas) else {
        return Ok(None);
    };
    let original = source.read(rect);
    let mut image = source.read(rect);
    below.try_apply(&mut image, rect)?;
    for ((dst, src), coverage) in image
        .bgra
        .chunks_exact_mut(4)
        .zip(original.bgra.chunks_exact(4))
        .zip(stroke_mask(a, rect))
    {
        let t = coverage as f32 / 255. * a.style.opacity.clamp(0., 1.);
        if t == 0. {
            continue;
        }
        let old_a = dst[3] as f32 / 255.;
        let src_a = src[3] as f32 / 255.;
        let alpha = old_a * (1. - t) + src_a * t;
        for c in 0..3 {
            dst[c] = if alpha > 0. {
                ((dst[c] as f32 * old_a * (1. - t) + src[c] as f32 * src_a * t) / alpha).round()
                    as u8
            } else {
                0
            };
        }
        dst[3] = (alpha * 255.).round() as u8;
    }
    Ok(Some((rect, image)))
}
pub fn replace(dst: &mut Image, region: Rect, src: &Image, rect: Rect) {
    if let Some(part) = region.intersection(rect) {
        for y in part.y..part.y + part.height {
            let d = (((y - region.y) * dst.width + part.x - region.x) * 4) as usize;
            let s = (((y - rect.y) * src.width + part.x - rect.x) * 4) as usize;
            let n = part.width as usize * 4;
            dst.bgra[d..d + n].copy_from_slice(&src.bgra[s..s + n]);
        }
    }
}

pub fn over_pixel(dst: &mut [u8], color: &[u8], alpha: f32) {
    if alpha <= 0. {
        return;
    }
    let old = dst[3] as f32 / 255.;
    let out = alpha + old * (1. - alpha);
    for c in 0..3 {
        dst[c] =
            ((color[c] as f32 * alpha + dst[c] as f32 * old * (1. - alpha)) / out).round() as u8;
    }
    dst[3] = (out * 255.).round() as u8;
}

#[cfg(test)]
mod rotated_text_plan_tests {
    use super::*;
    use crate::{annotation::Style, scene::PixelPlane, text_format::Alignment};

    fn annotation(tool: Tool, large: bool) -> Annotation {
        let mut style = Style {
            width: 2.,
            font_size: if tool == Tool::Serial && large {
                2500.
            } else {
                24.
            },
            ..Style::default()
        };
        style.text.alignment = Alignment::Center;
        let (start, end) = if tool == Tool::Serial && large {
            (Point { x: 1024., y: 1024. }, Point { x: 1024., y: 1024. })
        } else if large {
            (
                Point {
                    x: -9900.,
                    y: -100.,
                },
                Point { x: 10100., y: 300. },
            )
        } else {
            (Point { x: -20., y: 20. }, Point { x: 140., y: 100. })
        };
        let mut a = Annotation::new(tool, start, style);
        a.points.push(end);
        a.rotation = if large { 90. } else { 37. };
        a.text = if tool == Tool::Serial {
            "12"
        } else {
            "中文 Rust"
        }
        .into();
        a
    }

    #[test]
    fn normal_rotated_text_retains_local_sampling_and_visible_canvas_roi() {
        let canvas = Rect::full(320, 240);
        let a = annotation(Tool::Text, false);
        let Some(RotatedTextPlan::Local {
            rect,
            local_canvas,
            pad,
        }) = rotated_text_plan(&a, canvas).unwrap()
        else {
            panic!("normal text must retain its local sampler")
        };
        assert_eq!(local_canvas, Rect::full(170, 90));
        assert_eq!(pad, 5.);
        assert_eq!(rect.x, 0);
        assert!(rect.intersection(canvas) == Some(rect));
        assert_eq!(rect, region(&a, canvas).unwrap());
        assert_eq!(a.points[0], Point { x: -20., y: 20. });
    }

    #[test]
    fn outside_rotated_text_returns_empty_without_requesting_surface() {
        let canvas = Rect::full(320, 240);
        for tool in [Tool::Text, Tool::Serial] {
            let mut a = annotation(tool, true);
            a.translate(-1_000_000., -1_000_000.);
            assert_eq!(rotated_text_plan(&a, canvas).unwrap(), None);
        }
    }

    #[test]
    fn oversized_rotated_text_and_serial_plan_clipped_identity_projection() {
        let canvas = Rect::full(320, 240);
        for tool in [Tool::Text, Tool::Serial] {
            let a = annotation(tool, true);
            let Some(RotatedTextPlan::Projected { rect }) = rotated_text_plan(&a, canvas).unwrap()
            else {
                panic!("large local surfaces must use bounded target tiles")
            };
            assert_eq!(rect, region(&a, canvas).unwrap());
            assert!(rect.intersection(canvas) == Some(rect));
            let projection = rotated_text_projection(rect);
            assert!(projection.identity());
            assert_eq!(projection.source, rect);
            assert_eq!(
                projection.source_center(0, 0),
                Point {
                    x: rect.x as f32 + 0.5,
                    y: rect.y as f32 + 0.5,
                }
            );
            assert_eq!(
                a.local_bounds().width,
                if tool == Tool::Serial { 4000. } else { 20_000. }
            );
        }
        let mut invalid = annotation(Tool::Text, true);
        invalid.style.text.outline = true;
        invalid.style.text.outline_width = 100_000.;
        assert!(rotated_text_plan(&invalid, canvas).is_err());
    }

    /// 后台 GDI 像素检查：无窗口/桌面输入，不能归入纯算术模型的通过范围。
    #[test]
    fn oversized_rotated_text_and_serial_render_visible_glyphs_with_bounded_roi() {
        let canvas = Rect::full(2048, 2048);
        let bytes = [40_u8, 80, 120, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
        };
        for tool in [Tool::Text, Tool::Serial] {
            let a = annotation(tool, true);
            assert!(matches!(
                rotated_text_plan(&a, canvas).unwrap(),
                Some(RotatedTextPlan::Projected { .. })
            ));
            let (rect, raster) = text(&a, canvas).unwrap().unwrap();
            assert!(rect.intersection(canvas) == Some(rect));
            assert_eq!((raster.width, raster.height), (rect.width, rect.height));
            assert_eq!(
                raster.bgra.len(),
                rect.width as usize * rect.height as usize * 4
            );
            assert!(
                raster
                    .bgra
                    .chunks_exact(4)
                    .filter(|pixel| pixel[3] > 0)
                    .count()
                    > 10
            );
            if tool == Tool::Serial {
                assert!(raster.bgra.chunks_exact(4).any(|pixel| pixel[0] > 220
                    && pixel[1] > 220
                    && pixel[2] > 220
                    && pixel[3] > 0));
            }
            let scene = Scene::prepare(&[a], &source);
            assert!(scene.errors.is_empty());
            let mut actual = source.read(canvas);
            scene.try_apply(&mut actual, canvas).unwrap();
            let mut expected = source.read(canvas);
            composite(&mut expected, canvas, &raster, rect);
            assert_eq!(actual.bgra, expected.bgra);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::Style,
        scene::{PixelPlane, SourcePixels},
    };
    fn annotation(tool: Tool) -> Annotation {
        let mut a = Annotation::new(tool, Point { x: 10., y: 10. }, Style::default());
        a.update(Point { x: 70., y: 60. });
        a
    }
    #[test]
    fn self_intersecting_highlighter_has_uniform_alpha() {
        let mut a = annotation(Tool::Highlighter);
        a.style.opacity = 0.35;
        a.style.width = 12.;
        a.points = vec![
            Point { x: 20., y: 20. },
            Point { x: 60., y: 60. },
            Point { x: 20., y: 60. },
            Point { x: 60., y: 20. },
        ];
        let (rect, image) = shape(&a, Rect::full(100, 100)).unwrap();
        let alpha = |x: u32, y: u32| {
            image.bgra[(((y - rect.y) * rect.width + x - rect.x) as usize) * 4 + 3]
        };
        assert_eq!(alpha(40, 40), 89);
        assert_eq!(alpha(25, 25), 89);
    }
    #[test]
    fn brush_mosaic_respects_mask_and_crop_and_ignores_fill_style() {
        let full = Rect::full(100, 100);
        let bytes: Vec<_> = (0..10000)
            .flat_map(|i| [(i % 100 * 2) as u8, (i / 100 * 2) as u8, 50, 255])
            .collect();
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
        };
        let mut a = annotation(Tool::MosaicBrush);
        a.style.width = 12.;
        a.style.filled = true;
        let scene = Scene::prepare(&[a], &source);
        let mut image = source.read(full);
        scene.apply(&mut image, full);
        assert_ne!(image.bgra, bytes);
        assert_eq!(
            &image.bgra[(10 * 100 + 60) * 4..(10 * 100 + 60) * 4 + 4],
            &bytes[(10 * 100 + 60) * 4..(10 * 100 + 60) * 4 + 4]
        );
        let mut a = annotation(Tool::Mosaic);
        a.style.filled = true;
        a.style.opacity = 0.35;
        let scene = Scene::prepare(&[a], &source);
        assert!(matches!(
            scene.layers[0],
            crate::scene::Layer::Mosaic { .. }
        ));
    }
    #[test]
    fn blur_preserves_constant_colors_and_softens_edges() {
        let rect = Rect::full(80, 60);
        let mut image = empty(rect);
        for p in image.bgra.chunks_exact_mut(4) {
            p.copy_from_slice(&[40, 80, 120, 255]);
        }
        box_blur(&mut image, 8);
        assert!(image.bgra.chunks_exact(4).all(|p| p == [40, 80, 120, 255]));
        for y in 0..60 {
            for x in 40..80 {
                image.bgra[((y * 80 + x) as usize) * 4] = 240;
            }
        }
        box_blur(&mut image, 8);
        let v = image.bgra[(30 * 80 + 39) * 4];
        assert!(v > 40 && v < 240);
    }
    #[test]
    fn chinese_text_and_serial_have_visible_glyphs_and_release_gdi_handles() {
        let full = Rect::full(300, 120);
        let mut a = annotation(Tool::Text);
        a.points[1] = Point { x: 290., y: 110. };
        a.text = "中文 Rust 2026".into();
        let (_, image) = text(&a, full).unwrap().unwrap();
        assert!(image.bgra.chunks_exact(4).filter(|p| p[3] > 0).count() > 100);
        let mut a = annotation(Tool::Serial);
        a.text = "12".into();
        let (_, image) = text(&a, full).unwrap().unwrap();
        assert!(image
            .bgra
            .chunks_exact(4)
            .any(|p| p[0] > 220 && p[1] > 220 && p[2] > 220 && p[3] > 0));
    }
}
