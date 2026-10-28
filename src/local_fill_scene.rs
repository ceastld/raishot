//! 局部填充只采下层ROI。离散区域共用基线，输出分块避免长图透明union位图。
use crate::{
    annotation::{distance_to_segment, Annotation, Bounds, LocalFillGeometry, Point, Tool},
    capture::Image,
    geometry::Rect,
    local_fill::{erase_bgra, FillOptions, FillPath, FillStats, MAX_FILL_PIXELS},
    scene::{Scene, SourcePixels},
    Result,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct FillPatch {
    pub rect: Rect,
    pub image: Arc<Image>,
}
pub(crate) struct PreparedFill {
    pub patches: Arc<[FillPatch]>,
    pub stats: FillStats,
}

fn zeros(bytes: usize) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(bytes)
        .map_err(|_| "局部填充内存不足，草稿已保留")?;
    buffer.resize(bytes, 0);
    Ok(buffer)
}
fn area(rect: Rect) -> Result<usize> {
    let count = u64::from(rect.width) * u64::from(rect.height);
    if rect.width > 65535 || rect.height > 65535 || count > MAX_FILL_PIXELS {
        return Err("局部填充超过3200万像素/65535单边预算，草稿已保留".into());
    }
    Ok(count as usize)
}
fn clipped(bounds: Bounds, padding: f32, canvas: Rect) -> Option<Rect> {
    let x = (bounds.x - padding).floor().max(canvas.x as f32);
    let y = (bounds.y - padding).floor().max(canvas.y as f32);
    let right = (bounds.x + bounds.width + padding)
        .ceil()
        .min((canvas.x + canvas.width) as f32);
    let bottom = (bounds.y + bounds.height + padding)
        .ceil()
        .min((canvas.y + canvas.height) as f32);
    (right > x && bottom > y).then_some(Rect {
        x: x as u32,
        y: y as u32,
        width: (right - x) as u32,
        height: (bottom - y) as u32,
    })
}
fn read(source: &SourcePixels<'_>, region: Rect) -> Result<Image> {
    let mut image = Image {
        width: region.width,
        height: region.height,
        bgra: zeros(area(region)? * 4)?,
        readback_ms: 0.,
    };
    let fill = source.fill.unwrap_or([0, 0, 0, 255]);
    for p in image.bgra.chunks_exact_mut(4) {
        p.copy_from_slice(&fill);
    }
    for plane in source.planes.iter().chain(source.overlay.iter()) {
        let Some(part) = plane.rect.intersection(region) else {
            continue;
        };
        let bytes = (plane.rect.width as usize)
            .checked_mul(plane.rect.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("填充底图大小溢出")?;
        if plane.bgra.len() < bytes {
            return Err("填充底图像素不完整".into());
        }
        for y in part.y..part.y + part.height {
            let src = ((y - plane.rect.y) as usize * plane.rect.width as usize
                + (part.x - plane.rect.x) as usize)
                * 4;
            let dst = ((y - region.y) as usize * region.width as usize
                + (part.x - region.x) as usize)
                * 4;
            let count = part.width as usize * 4;
            image.bgra[dst..dst + count].copy_from_slice(&plane.bgra[src..src + count]);
        }
    }
    Ok(image)
}
fn stroke_mask(a: &Annotation, rect: Rect) -> Result<Vec<u8>> {
    let mut mask = zeros(area(rect)?)?;
    let radius = a.style.width / 2.;
    let segments = if a.points.len() == 1 {
        vec![(a.points[0], a.points[0])]
    } else {
        a.vector_path().flattened()
    };
    for (start, end) in segments {
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
        for y in top..bottom {
            for x in left..right {
                let coverage = ((radius + 0.5
                    - distance_to_segment(
                        Point {
                            x: x as f32 + 0.5,
                            y: y as f32 + 0.5,
                        },
                        start,
                        end,
                    ))
                .clamp(0., 1.)
                    * 255.)
                    .round() as u8;
                let i = ((y - rect.y) * rect.width + x - rect.x) as usize;
                mask[i] = mask[i].max(coverage);
            }
        }
    }
    Ok(mask)
}
fn rect_mask(a: &Annotation, bounds: Bounds, rect: Rect) -> Result<Vec<u8>> {
    let mut mask = zeros(area(rect)?)?;
    let inverse = a.rotation_transform().inverse();
    for y in 0..rect.height {
        for x in 0..rect.width {
            let p = inverse.map(Point {
                x: (rect.x + x) as f32 + 0.5,
                y: (rect.y + y) as f32 + 0.5,
            });
            if p.x >= bounds.x
                && p.y >= bounds.y
                && p.x < bounds.x + bounds.width
                && p.y < bounds.y + bounds.height
            {
                mask[(y * rect.width + x) as usize] = 255;
            }
        }
    }
    Ok(mask)
}
fn patches(
    image: &Image,
    rect: Rect,
    mask: &[u8],
    output: &mut Vec<FillPatch>,
    total: &mut u64,
) -> Result<()> {
    const SIDE: u32 = 2048;
    for y in (0..rect.height).step_by(SIDE as usize) {
        for x in (0..rect.width).step_by(SIDE as usize) {
            let width = SIDE.min(rect.width - x);
            let height = SIDE.min(rect.height - y);
            let marked = (y..y + height).any(|row| {
                let i = (row * rect.width + x) as usize;
                mask[i..i + width as usize].iter().any(|&m| m > 8)
            });
            if !marked {
                continue;
            }
            *total = total
                .checked_add(u64::from(width) * u64::from(height))
                .ok_or("局部填充结果大小溢出")?;
            if *total > MAX_FILL_PIXELS {
                return Err("批量填充结果超过3200万像素预算，未提交".into());
            }
            let mut bgra = zeros(width as usize * height as usize * 4)?;
            for row in 0..height {
                for col in 0..width {
                    let src = ((y + row) * rect.width + x + col) as usize;
                    if mask[src] > 8 {
                        let dst = (row * width + col) as usize * 4;
                        bgra[dst..dst + 4].copy_from_slice(&image.bgra[src * 4..src * 4 + 4]);
                    }
                }
            }
            output.try_reserve(1).map_err(|_| "无法保存填充结果分块")?;
            output.push(FillPatch {
                rect: Rect {
                    x: rect.x + x,
                    y: rect.y + y,
                    width,
                    height,
                },
                image: Arc::new(Image {
                    width,
                    height,
                    bgra,
                    readback_ms: 0.,
                }),
            });
        }
    }
    Ok(())
}
pub(crate) fn prepare(
    a: &Annotation,
    source: &SourcePixels<'_>,
    below: &Scene,
) -> Result<Option<PreparedFill>> {
    if a.tool != Tool::LocalFill || !a.valid() || !a.rotation.is_finite() {
        return Err("局部填充意图无效".into());
    }
    if !below.errors.is_empty() {
        return Err(below.errors.join(";").into());
    }
    let regions: &[Bounds] = match &a.fill_geometry {
        LocalFillGeometry::Stroke => &[],
        LocalFillGeometry::Regions(regions) => regions,
    };
    let mut output = Vec::new();
    let mut pixels = 0;
    let mut stats = FillStats {
        path: FillPath::Empty,
        requested: 0,
        filled: 0,
    };
    for index in 0..regions.len().max(1) {
        let region = regions.get(index).copied();
        let bounds = if let Some(region) = region {
            crate::transform::bounds(crate::transform::corners(region).map(|p| a.to_world(p)))
        } else {
            a.vector_path().bounds().unwrap_or_else(|| a.bounds())
        };
        let padding = if region.is_some() {
            2.
        } else {
            a.style.width / 2. + 3.
        };
        let Some(rect) = clipped(bounds, padding, source.canvas) else {
            continue;
        };
        area(rect)?;
        let mask = if let Some(region) = region {
            rect_mask(a, region, rect)?
        } else {
            stroke_mask(a, rect)?
        };
        let mut image = read(source, rect)?;
        below.try_apply(&mut image, rect)?;
        let result = erase_bgra(
            &mut image.bgra,
            rect.width,
            rect.height,
            rect.width as usize * 4,
            &mask,
            rect.width as usize,
            FillOptions::default(),
        )?;
        if result.requested != result.filled {
            return Err("周围没有足够的背景颜色，填充未提交；可以取消或调整范围后重试".into());
        }
        if result.requested == 0 {
            continue;
        }
        stats.path = result.path;
        stats.requested = stats
            .requested
            .checked_add(result.requested)
            .ok_or("填充孔计数溢出")?;
        stats.filled = stats
            .filled
            .checked_add(result.filled)
            .ok_or("填充像素计数溢出")?;
        patches(&image, rect, &mask, &mut output, &mut pixels)?;
    }
    if output.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PreparedFill {
            patches: output.into(),
            stats,
        }))
    }
}
