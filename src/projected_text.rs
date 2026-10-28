//! 保留原始排版，在目标 DIB 重绘字形；分块只裁切，不重新换行或重置水印相位。
use crate::{
    annotation::{Annotation, Bounds, Point, Tool},
    capture::Image,
    geometry::Rect,
    output_projection::OutputProjection,
    raster::TextSurface,
    text_layout::TextLayout,
    Result,
};
use windows::Win32::{
    Foundation::{RECT, SIZE},
    Graphics::Gdi::*,
};

const CORE: u32 = 512;
/// 横向宽块：长文字每行跨列时要重画，宽块让一行通常只画一遍。
const WIDE_CORE: u32 = 2048;
const MAX_SCRATCH: usize = 64 * 1024 * 1024;
const INF: f32 = 1e20;

/// 元数据与临时字形缓冲预算检查，不创建画布大小的 DIB 或透明图像。
pub(crate) fn validate(a: &Annotation, projection: &OutputProjection) -> Result<()> {
    if !matches!(a.tool, Tool::Text | Tool::Watermark | Tool::Serial) {
        return Err("目标文字重绘仅支持文字、水印和序号".into());
    }
    if !projection.sx.is_finite()
        || !projection.sy.is_finite()
        || projection.sx <= 0.
        || projection.sy <= 0.
        || !projection.origin.x.is_finite()
        || !projection.origin.y.is_finite()
        || !a.rotation.is_finite()
        || !a.style.font_size.is_finite()
        || a.style.font_size <= 0.
        || !a.style.opacity.is_finite()
        || !a.style.width.is_finite()
        || a.style.width < 0.
        || !a.style.text.outline_width.is_finite()
        || a.style.text.outline_width < 0.
        || !a.style.text.background_radius.is_finite()
        || a.style.text.background_radius < 0.
        || !a.style.text.background_opacity.is_finite()
    {
        return Err("目标文字样式或尺寸无效".into());
    }
    let b = a.local_bounds();
    if ![b.x, b.y, b.width, b.height].iter().all(|v| v.is_finite()) {
        return Err("目标文字坐标不是有限数值".into());
    }
    if b.width <= 0. || b.height <= 0. {
        return Ok(());
    }
    let radius = if a.tool != Tool::Serial && a.style.text.outline {
        a.style.text.outline_width
    } else {
        0.
    };
    let pad = (a.style.width.max(radius) + 3.) * projection.sx.max(projection.sy) + 2.;
    let Some(region) = projection.bounds(a.bounds(), pad) else {
        return Ok(());
    };
    let halo = (radius * projection.sx.max(projection.sy) + 3.).ceil();
    if !halo.is_finite() || halo > u32::MAX as f32 {
        return Err("目标文字描边超出临时缓冲预算".into());
    }
    let halo = halo as u32;
    tile_size(region, halo)?;
    Ok(())
}

pub(crate) fn paint(
    a: &Annotation,
    target: &mut Image,
    projection: &OutputProjection,
) -> Result<()> {
    validate(a, projection)?;
    let len = (target.width as usize)
        .checked_mul(target.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("目标文字图片尺寸溢出")?;
    if (target.width, target.height) != (projection.width, projection.height)
        || target.bgra.len() != len
    {
        return Err("目标文字图片尺寸无效".into());
    }
    let b = a.local_bounds();
    if b.width <= 0. || b.height <= 0. {
        return Ok(());
    }
    let radius = if a.tool != Tool::Serial && a.style.text.outline {
        a.style.text.outline_width
    } else {
        0.
    };
    let pad = (a.style.width.max(radius) + 3.) * projection.sx.max(projection.sy) + 2.;
    let Some(region) = projection.bounds(a.bounds(), pad) else {
        return Ok(());
    };
    let halo = (radius * projection.sx.max(projection.sy) + 3.).ceil() as u32;
    let (core_width, (w, h)) = tile_size(region, halo)?;
    let surface = TextSurface::new(
        w,
        h,
        a.style,
        if a.tool == Tool::Watermark { 300 } else { 0 },
    )?;
    // 必须先在 identity 下测量，再变换同一 HDC；SSA 内保存的设备不能跨线程/设备搬移。
    let layout = if a.tool == Tool::Text {
        Some(TextLayout::from_dc(a, surface.dc)?)
    } else {
        None
    };
    let mut utf: Vec<u16> = a.text.encode_utf16().collect();
    let watermark_extent = if a.tool == Tool::Watermark && !utf.is_empty() {
        let mut size = SIZE::default();
        unsafe {
            GetTextExtentPoint32W(surface.dc, &utf, &mut size).ok()?;
        }
        // 字体自身有 30° 斜排；用整段字形的保守影响范围选格点，不能拿行距代替长字形高度。
        size.cx.unsigned_abs() as f32 + size.cy.unsigned_abs() as f32 + a.style.font_size * 2.
    } else {
        0.
    };
    let watermark_phase = (a.tool == Tool::Watermark).then(|| watermark_region(a));
    let has_background = a.tool == Tool::Text && a.style.text.background;
    let plain = (a.tool != Tool::Serial && !has_background && radius <= 0.)
        .then(|| plain_table(a.style.color, a.style.opacity));
    #[cfg(test)]
    let plain = plain.filter(|_| !tests::PER_PIXEL.with(std::cell::Cell::get));
    for y in (region.y..region.y + region.height).step_by(CORE as usize) {
        for x in (region.x..region.x + region.width).step_by(core_width as usize) {
            let core = Rect {
                x,
                y,
                width: core_width.min(region.x + region.width - x),
                height: CORE.min(region.y + region.height - y),
            };
            // 允许负目标 origin；halo 中的字形可跨输出裁剪边界，但只把 core 写回一次。
            let origin = Point {
                x: x as f32 - halo as f32,
                y: y as f32 - halo as f32,
            };
            let transform = world_transform(a, projection, origin, watermark_phase);
            // 长文字每块只画与本块（含描边外扩）相交的行；无行且无底色时整块没有像素。
            let rows = layout
                .as_ref()
                .map(|_| layout_rows(&transform, (w, h), b.y.round()));
            #[cfg(test)]
            let rows = rows.filter(|_| !tests::DRAW_ALL_ROWS.with(std::cell::Cell::get));
            if let (Some(layout), Some((top, bottom))) = (&layout, rows) {
                if !a.style.text.background && !layout.has_rows_between(top, bottom) {
                    continue;
                }
            }
            surface.clear()?;
            {
                let saved = SavedDc::new(surface.dc)?;
                unsafe {
                    if SetGraphicsMode(surface.dc, GM_ADVANCED) == 0 {
                        return Err("无法设置目标字形绘制模式".into());
                    }
                    SetWorldTransform(surface.dc, &transform).ok()?;
                }
                draw(
                    a,
                    layout.as_ref(),
                    &surface,
                    &mut utf,
                    origin,
                    (w, h),
                    projection,
                    watermark_extent,
                    watermark_phase,
                    rows,
                )?;
                saved.restore()?;
            }
            let mask = surface.coverage()?;
            let outline = if radius > 0. {
                Some(dilate(
                    &mask,
                    w as usize,
                    h as usize,
                    radius,
                    projection.sx,
                    projection.sy,
                )?)
            } else {
                None
            };
            for dy in 0..core.height {
                for dx in 0..core.width {
                    let mx = (dx + halo) as usize;
                    let my = (dy + halo) as usize;
                    let i = my * w as usize + mx;
                    if let Some(table) = &plain {
                        let (pixel, over_transparent) = table[mask[i] as usize];
                        if pixel[3] == 0 {
                            continue;
                        }
                        let dst = ((core.y + dy) as usize * target.width as usize
                            + (core.x + dx) as usize)
                            * 4;
                        let dst = &mut target.bgra[dst..dst + 4];
                        if dst[3] == 0 {
                            dst.copy_from_slice(&over_transparent);
                        } else {
                            crate::raster::over_pixel(dst, &pixel[..3], pixel[3] as f32 / 255.);
                        }
                        continue;
                    }
                    // 没有底色时，字形与描边都为零的像素合成的是 0 透明度，目标保持不变。
                    if a.tool != Tool::Serial
                        && !has_background
                        && mask[i] == 0
                        && outline.as_ref().is_none_or(|outline| outline[i] == 0)
                    {
                        continue;
                    }
                    let mut pixel = [0u8; 4];
                    let local = || a.to_local(projection.source_center(core.x + dx, core.y + dy));
                    if a.tool == Tool::Serial {
                        let local = local();
                        // 序号的实心圆、原线宽外沿与白字在同一对象内合成，只乘一次透明度。
                        let center = crate::transform::center(b);
                        let r = b.width.min(b.height) / 2. + a.style.width / 2.;
                        let distance = local.distance(center) - r;
                        let world = a.to_world(local);
                        let n = Point {
                            x: world.x - center.x,
                            y: world.y - center.y,
                        };
                        let coverage = coverage(distance, n, projection);
                        color_over(&mut pixel, a.style.color, coverage);
                        color_over(&mut pixel, 0xffffff, mask[i] as f32 / 255.);
                    } else {
                        if has_background {
                            color_over(
                                &mut pixel,
                                a.style.text.background_color,
                                background(a, local(), projection)
                                    * a.style.text.background_opacity.clamp(0., 1.),
                            );
                        }
                        if let Some(outline) = &outline {
                            color_over(
                                &mut pixel,
                                a.style.text.outline_color,
                                outline[i] as f32 / 255.,
                            );
                        }
                        color_over(&mut pixel, a.style.color, mask[i] as f32 / 255.);
                    }
                    pixel[3] = (pixel[3] as f32 * a.style.opacity.clamp(0., 1.)).round() as u8;
                    let dst = ((core.y + dy) as usize * target.width as usize
                        + (core.x + dx) as usize)
                        * 4;
                    crate::raster::over_pixel(
                        &mut target.bgra[dst..dst + 4],
                        &pixel[..3],
                        pixel[3] as f32 / 255.,
                    );
                }
            }
        }
    }
    Ok(())
}

/// 无底色、无描边的文字与水印：像素只由字形覆盖率决定。按 256 级覆盖率预先走一遍
/// 逐像素合成，得到（乘对象透明度后的像素，落在全透明目标上的结果），与逐像素路径逐字节相同。
fn plain_table(color: u32, opacity: f32) -> Vec<([u8; 4], [u8; 4])> {
    (0..=255u8)
        .map(|coverage| {
            let mut pixel = [0u8; 4];
            color_over(&mut pixel, color, coverage as f32 / 255.);
            pixel[3] = (pixel[3] as f32 * opacity.clamp(0., 1.)).round() as u8;
            let mut over_transparent = [0u8; 4];
            crate::raster::over_pixel(&mut over_transparent, &pixel[..3], pixel[3] as f32 / 255.);
            (pixel, over_transparent)
        })
        .collect()
}

/// 先试宽块，大描边放不下时退回方块；能否绘制仍只取决于方块是否在预算内。
fn tile_size(region: Rect, halo: u32) -> Result<(u32, (u32, u32))> {
    if let Ok(size) = scratch_size(region, halo, WIDE_CORE) {
        return Ok((WIDE_CORE, size));
    }
    scratch_size(region, halo, CORE).map(|size| (CORE, size))
}

fn scratch_size(region: Rect, halo: u32, core_width: u32) -> Result<(u32, u32)> {
    let margin = halo.checked_mul(2).ok_or("目标文字描边尺寸溢出")?;
    let w = core_width
        .min(region.width)
        .checked_add(margin)
        .ok_or("目标文字工作区尺寸溢出")?;
    let h = CORE
        .min(region.height)
        .checked_add(margin)
        .ok_or("目标文字工作区尺寸溢出")?;
    // DIB 4 + mask 1 + 距离场 4 + outline 1，另留 EDT 行及 sites/boundaries 临时空间。
    let bytes = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(10))
        .and_then(|n| n.checked_add(w.max(h) as usize * 32))
        .ok_or("目标文字工作区尺寸溢出")?;
    if bytes > MAX_SCRATCH || w > i32::MAX as u32 || h > i32::MAX as u32 {
        return Err("目标文字描边超过 64 MiB 临时缓冲预算".into());
    }
    Ok((w, h))
}

struct SavedDc {
    dc: HDC,
    saved: i32,
}
impl SavedDc {
    fn new(dc: HDC) -> Result<Self> {
        let saved = unsafe { SaveDC(dc) };
        if saved == 0 {
            Err("无法保存文字设备状态".into())
        } else {
            Ok(Self { dc, saved })
        }
    }
    fn restore(mut self) -> Result<()> {
        unsafe {
            RestoreDC(self.dc, self.saved).ok()?;
        }
        self.saved = 0;
        Ok(())
    }
}
impl Drop for SavedDc {
    fn drop(&mut self) {
        if self.saved != 0 {
            unsafe {
                let _ = RestoreDC(self.dc, self.saved);
            }
        }
    }
}

fn world_transform(
    a: &Annotation,
    p: &OutputProjection,
    origin: Point,
    watermark_phase: Option<Bounds>,
) -> XFORM {
    let (sin, cos) = a.rotation.to_radians().sin_cos();
    let c = crate::transform::center(a.local_bounds());
    let mut transform = XFORM {
        eM11: p.sx * cos,
        eM12: p.sy * sin,
        eM21: -p.sx * sin,
        eM22: p.sy * cos,
        eDx: p.sx * (c.x - cos * c.x + sin * c.y - p.source.x as f32) - p.origin.x - origin.x,
        eDy: p.sy * (c.y - sin * c.x - cos * c.y - p.source.y as f32) - p.origin.y - origin.y,
    };
    if let Some(phase) = watermark_phase {
        // 水印沿局部 DIB 的整数网格输出，把含负数/小数的源原点放进变换，不能转成 i32。
        transform.eDx += transform.eM11 * phase.x + transform.eM21 * phase.y;
        transform.eDy += transform.eM12 * phase.x + transform.eM22 * phase.y;
    }
    transform
}

#[allow(clippy::too_many_arguments)]
fn draw(
    a: &Annotation,
    layout: Option<&TextLayout>,
    surface: &TextSurface,
    utf: &mut [u16],
    origin: Point,
    size: (u32, u32),
    p: &OutputProjection,
    watermark_extent: f32,
    watermark_phase: Option<Bounds>,
    rows: Option<(f32, f32)>,
) -> Result<()> {
    let b = a.local_bounds();
    match a.tool {
        Tool::Text => {
            let (top, bottom) = rows.unwrap_or((f32::NEG_INFINITY, f32::INFINITY));
            layout.ok_or("缺少原始文字排版")?.draw_between(
                b.x.round() as i32,
                b.y.round() as i32,
                top,
                bottom,
            )?
        }
        Tool::Serial => {
            let mut rect = RECT {
                left: b.x as i32,
                top: b.y as i32,
                right: (b.x + b.width) as i32,
                bottom: (b.y + b.height) as i32,
            };
            let result = unsafe {
                DrawTextW(
                    surface.dc,
                    utf,
                    &mut rect,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                )
            };
            if result == 0 && !utf.is_empty() {
                return Err("目标序号字形绘制失败".into());
            }
        }
        Tool::Watermark => {
            // 用整个对象的源坐标网格，不从 tile/crop 的左上角重新平铺。
            let phase = watermark_phase.ok_or("缺少源水印平铺原点")?;
            let step_x =
                (a.style.font_size * (a.text.chars().count() as f32 + 4.)).max(160.) as i32;
            let step_y = (a.style.font_size * 5.).max(120.) as i32;
            let local = crate::transform::bounds(
                [
                    Point {
                        x: origin.x,
                        y: origin.y,
                    },
                    Point {
                        x: origin.x + size.0 as f32,
                        y: origin.y,
                    },
                    Point {
                        x: origin.x,
                        y: origin.y + size.1 as f32,
                    },
                    Point {
                        x: origin.x + size.0 as f32,
                        y: origin.y + size.1 as f32,
                    },
                ]
                .map(|q| a.to_local(p.unmap(q))),
            );
            let start_x =
                (((local.x - watermark_extent - phase.x) / step_x as f32).floor() as i32).max(0);
            let end_x = (((local.x + local.width + watermark_extent - phase.x) / step_x as f32)
                .ceil() as i32)
                .min((phase.width / step_x as f32).ceil() as i32);
            let base_y = phase.y + (step_y / 2) as f32;
            let start_y =
                (((local.y - watermark_extent - base_y) / step_y as f32).floor() as i32).max(0);
            let end_y = (((local.y + local.height + watermark_extent - base_y) / step_y as f32)
                .ceil() as i32)
                .min(
                    ((phase.height + step_y as f32 - (step_y / 2) as f32) / step_y as f32).ceil()
                        as i32,
                );
            for y in start_y..end_y {
                for x in start_x..end_x {
                    unsafe {
                        TextOutW(surface.dc, x * step_x, step_y / 2 + y * step_y, utf).ok()?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// 临时面 `(0,0)-(w,h)` 经世界变换逆映射回文字局部坐标后的纵向范围，减去排版原点 `layout_y`。
/// 变换退化或数值无效时返回全范围，按原样画全部行。
fn layout_rows(transform: &XFORM, (w, h): (u32, u32), layout_y: f32) -> (f32, f32) {
    let det = transform.eM11 * transform.eM22 - transform.eM12 * transform.eM21;
    if !det.is_finite() || det.abs() < 1e-12 {
        return (f32::NEG_INFINITY, f32::INFINITY);
    }
    let (mut top, mut bottom) = (f32::INFINITY, f32::NEG_INFINITY);
    for (x, y) in [
        (0., 0.),
        (w as f32, 0.),
        (0., h as f32),
        (w as f32, h as f32),
    ] {
        let ly =
            (transform.eM11 * (y - transform.eDy) - transform.eM12 * (x - transform.eDx)) / det;
        top = top.min(ly);
        bottom = bottom.max(ly);
    }
    if !top.is_finite() || !bottom.is_finite() {
        return (f32::NEG_INFINITY, f32::INFINITY);
    }
    (top - layout_y, bottom - layout_y)
}

fn watermark_region(a: &Annotation) -> Bounds {
    let b = a.local_bounds();
    let margin = a.style.width.max(if a.style.text.outline {
        a.style.text.outline_width
    } else {
        0.
    }) + 2.;
    if a.rotation != 0. {
        // 1× 旋转文字先移到有 padding 的局部面，平铺后再映回源坐标。
        // 复用同一 region 规则，只复制几何，不创建 1× DIB 或复制水印字符串。
        let pad = margin + 1.;
        let (dx, dy) = (pad - b.x, pad - b.y);
        let mut local = Annotation {
            tool: a.tool,
            points: a.points.clone(),
            style: a.style,
            text: String::new(),
            rotation: 0.,
            fill_geometry: Default::default(),
            effect_regions: None,
            rich: None,
        };
        local.translate(dx, dy);
        let canvas = Rect::full(
            (b.width + pad * 2.).ceil().max(1.) as u32,
            (b.height + pad * 2.).ceil().max(1.) as u32,
        );
        if let Some(rect) = crate::raster::region(&local, canvas) {
            return Bounds {
                x: rect.x as f32 - dx,
                y: rect.y as f32 - dy,
                width: rect.width as f32,
                height: rect.height as f32,
            };
        }
    }
    let x = (b.x - margin).floor().max(0.);
    let y = (b.y - margin).floor().max(0.);
    Bounds {
        x,
        y,
        width: (b.x + b.width + margin).ceil() - x,
        height: (b.y + b.height + margin).ceil() - y,
    }
}

fn coverage(distance: f32, normal: Point, p: &OutputProjection) -> f32 {
    let len = normal.x.hypot(normal.y);
    let scale = if len > 0.0001 {
        len / (normal.x / p.sx).hypot(normal.y / p.sy)
    } else {
        p.sx.min(p.sy)
    };
    (0.5 - distance * scale).clamp(0., 1.)
}
fn background(a: &Annotation, q: Point, p: &OutputProjection) -> f32 {
    let b = a.local_bounds();
    let r = a
        .style
        .text
        .background_radius
        .clamp(0., b.width.min(b.height) / 2.);
    let dx = q.x - b.x - b.width / 2.;
    let dy = q.y - b.y - b.height / 2.;
    let x = dx.abs() - b.width / 2. + r;
    let y = dy.abs() - b.height / 2. + r;
    let d = x.max(0.).hypot(y.max(0.)) + x.max(y).min(0.) - r;
    let normal = if x > 0. && y > 0. {
        Point {
            x: dx.signum() * x,
            y: dy.signum() * y,
        }
    } else if x > y {
        Point {
            x: dx.signum(),
            y: 0.,
        }
    } else {
        Point {
            x: 0.,
            y: dy.signum(),
        }
    };
    let (sin, cos) = a.rotation.to_radians().sin_cos();
    coverage(
        d,
        Point {
            x: cos * normal.x - sin * normal.y,
            y: sin * normal.x + cos * normal.y,
        },
        p,
    )
}
fn color_over(pixel: &mut [u8; 4], color: u32, coverage: f32) {
    crate::raster::over_pixel(
        pixel,
        &[color as u8, (color >> 8) as u8, (color >> 16) as u8],
        coverage.clamp(0., 1.),
    );
}

// 在源长度度量下做加权 EDT，sx != sy 时描边随整个对象变换，不能只取最大倍率。
fn dilate(mask: &[u8], w: usize, h: usize, radius: f32, sx: f32, sy: f32) -> Result<Vec<u8>> {
    let mut distance = vec_checked(mask.len(), INF)?;
    let mut input = vec_checked(w.max(h), INF)?;
    let mut output = vec_checked(w.max(h), INF)?;
    let edge = 1. / sx.min(sy);
    for x in 0..w {
        for y in 0..h {
            let m = mask[y * w + x];
            input[y] = if m == 0 {
                INF
            } else {
                ((1. - m as f32 / 255.) * edge).powi(2)
            };
        }
        transform(&input[..h], &mut output[..h], 1. / sy.powi(2));
        for y in 0..h {
            distance[y * w + x] = output[y];
        }
    }
    for y in 0..h {
        input[..w].copy_from_slice(&distance[y * w..(y + 1) * w]);
        transform(&input[..w], &mut output[..w], 1. / sx.powi(2));
        distance[y * w..(y + 1) * w].copy_from_slice(&output[..w]);
    }
    let mut result = Vec::new();
    result.try_reserve_exact(mask.len())?;
    result.extend(
        distance
            .into_iter()
            .map(|d| (((radius + edge - d.sqrt()) / edge).clamp(0., 1.) * 255.).round() as u8),
    );
    Ok(result)
}
fn vec_checked(n: usize, value: f32) -> Result<Vec<f32>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)?;
    v.resize(n, value);
    Ok(v)
}
fn transform(input: &[f32], out: &mut [f32], weight: f32) {
    let mut sites = Vec::<usize>::new();
    let mut edges = Vec::<f32>::new();
    for (q, &f) in input.iter().enumerate().filter(|(_, f)| **f < INF) {
        let mut edge = f32::NEG_INFINITY;
        while let Some(&i) = sites.last() {
            edge = (f - input[i] + weight * ((q - i) * (q + i)) as f32)
                / (2. * weight * (q - i) as f32);
            if sites.len() == 1 || edge > edges[sites.len() - 1] {
                break;
            }
            sites.pop();
            edges.pop();
        }
        if sites.is_empty() {
            edge = f32::NEG_INFINITY;
        }
        sites.push(q);
        edges.push(edge);
    }
    if sites.is_empty() {
        out.fill(INF);
        return;
    }
    let mut k = 0;
    for (q, d) in out.iter_mut().enumerate() {
        while k + 1 < sites.len() && edges[k + 1] < q as f32 {
            k += 1;
        }
        *d = weight * (q as f32 - sites[k] as f32).powi(2) + input[sites[k]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Style;

    thread_local! {
        /// 恢复“每块画全部行”的旧路径，作为按行裁剪的逐字节对照。
        pub(super) static DRAW_ALL_ROWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// 关闭覆盖率查表，回到逐像素合成。
        pub(super) static PER_PIXEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[test]
    fn plain_text_coverage_table_matches_per_pixel_compositing_byte_for_byte() {
        for tool in [Tool::Text, Tool::Watermark] {
            let mut a = annotation(tool);
            for (color, opacity) in [(0x20252b, 1.), (0x315bb7, 0.6), (0xd9e2e9, 0.37)] {
                a.style.color = color;
                a.style.opacity = opacity;
                for under in [None, Some([10, 200, 30, 128]), Some([255, 255, 255, 255])] {
                    let p = OutputProjection::new(Rect::full(700, 400), 1.25).unwrap();
                    let fill = |image: &mut Image| {
                        if let Some(under) = under {
                            for (i, px) in image.bgra.chunks_exact_mut(4).enumerate() {
                                // 一半透明、一半有底，覆盖查表的两个分支。
                                if i % 3 != 0 {
                                    px.copy_from_slice(&under);
                                }
                            }
                        }
                    };
                    let mut fast = crate::raster::empty(Rect::full(p.width, p.height));
                    fill(&mut fast);
                    paint(&a, &mut fast, &p).unwrap();
                    PER_PIXEL.with(|slow| slow.set(true));
                    let mut slow = crate::raster::empty(Rect::full(p.width, p.height));
                    fill(&mut slow);
                    paint(&a, &mut slow, &p).unwrap();
                    PER_PIXEL.with(|slow| slow.set(false));
                    assert!(
                        fast.bgra == slow.bgra,
                        "查表合成改变了像素：{tool:?} color={color:06x} opacity={opacity} under={under:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn long_text_row_culling_matches_drawing_every_row_byte_for_byte() {
        let mut a = annotation(Tool::Text);
        a.points = vec![Point { x: 18.25, y: 17.5 }, Point { x: 640., y: 1400. }];
        a.text = (0..40)
            .map(|i| format!("{i:02} 中文与 Latin 混排 e\u{301} 换行测试，第 {i} 行 gjpqy"))
            .collect::<Vec<_>>()
            .join("\n");
        for (outline, background, rotation, zoom) in [
            (false, false, 0., 1.25),
            (true, false, 0., 1.5),
            (false, true, 0., 2.),
            (true, true, 17., 1.25),
            (false, false, -23., 0.6),
        ] {
            a.style.text.outline = outline;
            a.style.text.outline_width = 3.5;
            a.style.text.background = background;
            a.rotation = rotation;
            let source = Rect::full(700, 1440);
            let culled = render(&a, source, zoom);
            DRAW_ALL_ROWS.with(|all| all.set(true));
            let full = render(&a, source, zoom);
            DRAW_ALL_ROWS.with(|all| all.set(false));
            assert!(culled.bgra.chunks_exact(4).any(|p| p[3] > 0));
            assert!(
                culled.bgra == full.bgra,
                "按行裁剪改变了像素：outline={outline} background={background} rotation={rotation} zoom={zoom}"
            );
        }
    }

    #[test]
    fn layout_rows_inverts_world_transform_to_local_vertical_range() {
        let mut a = annotation(Tool::Text);
        a.points = vec![Point { x: 10., y: 100. }, Point { x: 310., y: 900. }];
        let p = OutputProjection::new(Rect::full(400, 1000), 2.).unwrap();
        // 目标块原点 (0, 600) 对应源 y=300；512 高的块覆盖源 300..556，减去排版原点 100。
        let t = world_transform(&a, &p, Point { x: 0., y: 600. }, None);
        let (top, bottom) = layout_rows(&t, (512, 512), 100.);
        assert!((top - 200.).abs() < 0.01 && (bottom - 456.).abs() < 0.01);
        a.rotation = 90.;
        let t = world_transform(&a, &p, Point { x: 0., y: 0. }, None);
        let (top, bottom) = layout_rows(&t, (512, 512), 100.);
        assert!(top.is_finite() && bottom > top);
    }

    fn annotation(tool: Tool) -> Annotation {
        let mut a = Annotation::new(
            tool,
            Point { x: 35., y: 40. },
            Style {
                font_size: 22.,
                color: 0x315bb7,
                opacity: 0.6,
                ..Default::default()
            },
        );
        if tool != Tool::Serial {
            a.update(Point { x: 655., y: 360. });
        }
        a.text = "中文🙂 Latin e\u{301} العربية\n保留原图中的换行".into();
        a
    }
    fn render(a: &Annotation, source: Rect, zoom: f32) -> Image {
        let p = OutputProjection::new(source, zoom).unwrap();
        let mut image = crate::raster::empty(Rect::full(p.width, p.height));
        paint(a, &mut image, &p).unwrap();
        image
    }
    fn assert_crop(full: &Image, crop: &Image, x: u32, y: u32) {
        for row in 0..crop.height {
            let from = ((row + y) as usize * full.width as usize + x as usize) * 4;
            let to = row as usize * crop.width as usize * 4;
            assert_eq!(
                &full.bgra[from..from + crop.width as usize * 4],
                &crop.bgra[to..to + crop.width as usize * 4],
                "目标裁剪相位在行 {row} 改变"
            );
        }
    }
    #[test]
    fn watermark_edge_phase_matches_legacy_local_surface_without_integer_truncation() {
        for (x, y, outline, phase_x, phase_y) in [
            (2., 3., false, -4., -3.),
            (2.25, 3.75, false, -3.75, -2.25),
            (-2.25, -3.75, false, -8.25, -9.75),
            (35.25, 40.5, false, 29.25, 34.5),
            (2.25, 3.75, true, -6.5, -5.),
        ] {
            let mut a = annotation(Tool::Watermark);
            a.points = vec![
                Point { x, y },
                Point {
                    x: x + 620.,
                    y: y + 320.,
                },
            ];
            a.style.width = 4.;
            a.style.text.outline = outline;
            a.style.text.outline_width = 6.75;
            for rotation in [-31., 13.] {
                a.rotation = rotation;
                let phase = watermark_region(&a);
                assert_eq!((phase.x, phase.y), (phase_x, phase_y));
                assert_eq!(phase.width, if outline { 638. } else { 632. });
                assert_eq!(phase.height, if outline { 338. } else { 332. });
                let p = OutputProjection::new(Rect::full(700, 400), 1.5).unwrap();
                let transform = world_transform(&a, &p, Point { x: 0., y: 0. }, Some(phase));
                // 字体原点在局部整数格点 (0, 60)，映到源图时必须保留 phase 的小数部分。
                let expected = p.map(a.to_world(Point {
                    x: phase.x,
                    y: phase.y + 60.,
                }));
                assert!((transform.eM21 * 60. + transform.eDx - expected.x).abs() < 0.0002);
                assert!((transform.eM22 * 60. + transform.eDy - expected.y).abs() < 0.0002);
            }
            a.rotation = 0.;
            let phase = watermark_region(&a);
            let margin = if outline { 8.75 } else { 6. };
            assert_eq!(phase.x, (x - margin).floor().max(0.));
            assert_eq!(phase.y, (y - margin).floor().max(0.));
        }
    }

    // 独立目标 DIB 直接画旧局部面的全部格点，作为相位 oracle；不调用新网格/变换帮助函数。
    fn legacy_watermark_target(a: &Annotation, source: Rect, zoom: f32) -> Image {
        let p = OutputProjection::new(source, zoom).unwrap();
        let b = a.local_bounds();
        let pad = a.style.width + 3.;
        let (dx, dy) = (pad - b.x, pad - b.y);
        let mut local = a.clone();
        local.rotation = 0.;
        local.translate(dx, dy);
        let local_canvas = Rect::full(
            (b.width + pad * 2.).ceil() as u32,
            (b.height + pad * 2.).ceil() as u32,
        );
        let rect = crate::raster::region(&local, local_canvas).unwrap();
        let (sin, cos) = a.rotation.to_radians().sin_cos();
        let center = crate::transform::center(b);
        let origin = Point {
            x: rect.x as f32 - dx,
            y: rect.y as f32 - dy,
        };
        let transform = XFORM {
            eM11: p.sx * cos,
            eM12: p.sy * sin,
            eM21: -p.sx * sin,
            eM22: p.sy * cos,
            eDx: p.sx
                * (center.x + cos * (origin.x - center.x)
                    - sin * (origin.y - center.y)
                    - source.x as f32),
            eDy: p.sy
                * (center.y + sin * (origin.x - center.x) + cos * (origin.y - center.y)
                    - source.y as f32),
        };
        let surface = TextSurface::new(p.width, p.height, a.style, 300).unwrap();
        let utf = a.text.encode_utf16().collect::<Vec<_>>();
        let step_x = (a.style.font_size * (a.text.chars().count() as f32 + 4.)).max(160.) as i32;
        let step_y = (a.style.font_size * 5.).max(120.) as i32;
        unsafe {
            assert_ne!(SetGraphicsMode(surface.dc, GM_ADVANCED), 0);
            SetWorldTransform(surface.dc, &transform).unwrap();
            for y in (step_y / 2..rect.height as i32 + step_y).step_by(step_y as usize) {
                for x in (0..rect.width as i32).step_by(step_x as usize) {
                    TextOutW(surface.dc, x, y, &utf).unwrap();
                }
            }
        }
        let mask = surface.coverage().unwrap();
        let mut image = crate::raster::empty(Rect::full(p.width, p.height));
        let region = p
            .bounds(a.bounds(), (a.style.width + 3.) * p.sx.max(p.sy) + 2.)
            .unwrap();
        let color = [
            a.style.color as u8,
            (a.style.color >> 8) as u8,
            (a.style.color >> 16) as u8,
        ];
        for y in region.y..region.y + region.height {
            for x in region.x..region.x + region.width {
                let i = (y as usize * p.width as usize + x as usize) * 4;
                let alpha = (mask[i / 4] as f32 * a.style.opacity).round() as u8;
                crate::raster::over_pixel(&mut image.bgra[i..i + 4], &color, alpha as f32 / 255.);
            }
        }
        image
    }

    #[test]
    fn watermark_edge_phase_target_glyphs_match_legacy_grid_and_nonzero_crop() {
        let mut a = annotation(Tool::Watermark);
        a.points = vec![
            Point { x: 2.25, y: 3.75 },
            Point {
                x: 622.25,
                y: 363.75,
            },
        ];
        a.text = "Phase QA".into();
        a.style.width = 4.;
        a.style.text.outline = false;
        for rotation in [13., -17.] {
            a.rotation = rotation;
            let full = render(&a, Rect::full(700, 400), 1.5);
            let expected = legacy_watermark_target(&a, Rect::full(700, 400), 1.5);
            let first_difference = full
                .bgra
                .iter()
                .zip(&expected.bgra)
                .position(|(a, b)| a != b);
            assert_eq!(
                first_difference, None,
                "旋转 {rotation}° 的目标字形格点发生偏移"
            );
            let crop = render(
                &a,
                Rect {
                    x: 100,
                    y: 40,
                    width: 350,
                    height: 260,
                },
                1.5,
            );
            assert_crop(&full, &crop, 150, 60);
            assert!(crop.bgra.chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert!(full.bgra.chunks_exact(4).all(|pixel| pixel[3] <= 153));
        }
    }

    #[test]
    fn watermark_edge_phase_outline_negative_origin_crop_stays_stable() {
        let mut a = annotation(Tool::Watermark);
        a.points = vec![
            Point { x: -2.25, y: -3.75 },
            Point {
                x: 617.75,
                y: 356.25,
            },
        ];
        a.text = "边缘 QA".into();
        a.style.text.outline = true;
        a.style.text.outline_width = 6.75;
        for rotation in [19., -23.] {
            a.rotation = rotation;
            let full = render(&a, Rect::full(700, 400), 1.5);
            let crop = render(
                &a,
                Rect {
                    x: 100,
                    y: 40,
                    width: 350,
                    height: 260,
                },
                1.5,
            );
            assert_crop(&full, &crop, 150, 60);
            assert!(crop.bgra.chunks_exact(4).any(|pixel| pixel[3] > 0));
            assert!(full.bgra.chunks_exact(4).all(|pixel| pixel[3] <= 153));
        }
    }
    #[test]
    fn enlarged_export_text_redraw_preserves_layout_and_is_not_nearest_bitmap() {
        let mut a = annotation(Tool::Text);
        a.style.opacity = 1.;
        // 源宽 100 px：第一段必须自动换行，不能只靠文本中的显式换行验证排版。
        a.update(Point { x: 135., y: 320. });
        let before = TextLayout::new(&a)
            .unwrap()
            .rows
            .iter()
            .map(|r| (r.range.clone(), r.x, r.y, r.width, r.height))
            .collect::<Vec<_>>();
        let (rect, one) = crate::raster::text(&a, Rect::full(700, 400))
            .unwrap()
            .unwrap();
        let enlarged = render(&a, Rect::full(700, 400), 2.);
        let after = TextLayout::new(&a)
            .unwrap()
            .rows
            .iter()
            .map(|r| (r.range.clone(), r.x, r.y, r.width, r.height))
            .collect::<Vec<_>>();
        assert_eq!(before, after);
        assert!(before.len() > 2);
        assert!(before[0].0.end < a.text.find('\n').unwrap());
        let mut changed = 0;
        let mut drawn = 0;
        for y in 0..enlarged.height {
            for x in 0..enlarged.width {
                let alpha =
                    enlarged.bgra[(y as usize * enlarged.width as usize + x as usize) * 4 + 3];
                drawn += usize::from(alpha > 0);
                let (sx, sy) = (x / 2, y / 2);
                let nearest = if sx >= rect.x
                    && sy >= rect.y
                    && sx < rect.x + rect.width
                    && sy < rect.y + rect.height
                {
                    one.bgra[((sy - rect.y) as usize * one.width as usize + (sx - rect.x) as usize)
                        * 4
                        + 3]
                } else {
                    0
                };
                changed += usize::from(alpha != nearest);
            }
        }
        assert!(drawn > 500);
        assert!(changed > 100, "没有按目标像素重绘字体");
    }
    #[test]
    fn enlarged_export_text_rotated_outline_background_crop_and_tile_phase_match() {
        let mut a = annotation(Tool::Text);
        a.rotation = 23.;
        a.style.text.outline = true;
        a.style.text.outline_width = 2.3;
        a.style.text.background = true;
        a.style.text.background_opacity = 0.45;
        a.style.text.background_radius = 17.;
        let full = render(&a, Rect::full(700, 400), 1.5);
        let crop = render(
            &a,
            Rect {
                x: 250,
                y: 60,
                width: 280,
                height: 200,
            },
            1.5,
        );
        assert_crop(&full, &crop, 375, 90);
        assert!(crop.bgra.chunks_exact(4).any(|p| p[3] > 0));
        assert!(full.bgra.chunks_exact(4).all(|p| p[3] <= 153));
    }
    #[test]
    fn enlarged_export_watermark_grid_and_serial_alpha_are_projected_once() {
        let mut a = annotation(Tool::Watermark);
        a.text = "水印 QA".into();
        a.rotation = 13.;
        a.style.text.outline = true;
        a.style.text.outline_width = 1.5;
        let full = render(&a, Rect::full(700, 400), 1.5);
        let crop = render(
            &a,
            Rect {
                x: 100,
                y: 120,
                width: 350,
                height: 200,
            },
            1.5,
        );
        assert_crop(&full, &crop, 150, 180);
        assert!(crop.bgra.chunks_exact(4).any(|p| p[3] > 0));
        a.text = "ABCDEFGHIJKLMNOPQRSTUVWXYZ 中文水印".repeat(3);
        let full = render(&a, Rect::full(700, 400), 1.5);
        let crop = render(
            &a,
            Rect {
                x: 100,
                y: 40,
                width: 280,
                height: 170,
            },
            1.5,
        );
        assert_crop(&full, &crop, 150, 60);
        a = annotation(Tool::Serial);
        a.points[0] = Point { x: 85., y: 95. };
        a.text = "12".into();
        a.rotation = 31.;
        let serial = render(&a, Rect::full(200, 200), 2.);
        assert!(serial.bgra.chunks_exact(4).any(|p| p[3] > 0));
        assert!(serial.bgra.chunks_exact(4).all(|p| p[3] <= 153));
        assert!(serial
            .bgra
            .chunks_exact(4)
            .any(|p| p[3] == 153 && p[..3] == [255, 255, 255]));
    }
    #[test]
    fn enlarged_export_text_scratch_and_anisotropic_outline_are_bounded() {
        assert_eq!(
            scratch_size(Rect::full(32_000, 1000), 3, CORE).unwrap(),
            (518, 518)
        );
        assert_eq!(
            tile_size(Rect::full(32_000, 1000), 3).unwrap(),
            (WIDE_CORE, (2054, 518))
        );
        // 宽块超预算时退回方块，可绘制范围不因宽块收窄。
        assert_eq!(
            tile_size(Rect::full(32_000, 4000), 1000).unwrap(),
            (CORE, (2512, 2512))
        );
        assert!(tile_size(Rect::full(32_000, 1000), 10_000).is_err());
        let mut mask = vec![0; 21 * 21];
        mask[10 * 21 + 10] = 255;
        let out = dilate(&mask, 21, 21, 2., 2., 1.).unwrap();
        assert_eq!(out[10 * 21 + 14], 255);
        assert_eq!(out[12 * 21 + 10], 255);
        assert_eq!(out[14 * 21 + 10], 0);
        assert_eq!(out[10 * 21 + 16], 0);
        let mut a = annotation(Tool::Text);
        a.style.text.outline_width = f32::NAN;
        let p = OutputProjection::new(Rect::full(32, 32), 2.).unwrap();
        let mut target = crate::raster::empty(Rect::full(64, 64));
        assert!(paint(&a, &mut target, &p).is_err());
    }
}
