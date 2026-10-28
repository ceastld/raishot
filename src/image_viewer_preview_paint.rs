//! 总览与局部细节共用一个棋盘底；透明细节不会混入旧总览像素。
use super::preview_region::Surface;
use crate::{
    geometry::Rect,
    image_preview::{
        paint::{overview_regions, CheckerGrid},
        viewport::{PhysicalRect, PreviewViewport},
        PreviewGeometry,
    },
};
use gpui::{fill, point, px, rgb, rgba, size, Bounds, ContentMask, RenderImage, Window};
use std::sync::Arc;

const MAX_FALLBACK_CELLS: u64 = 4096;
const MAX_CANDIDATES: u64 = 64 * 1024;
const MAX_TEXTURE_ERROR: f64 = 0.125;

pub(super) fn paint(
    overview: &Arc<RenderImage>,
    geometry: PreviewGeometry,
    viewport: PreviewViewport,
    detail: Option<&Surface>,
    area: Rect,
    dpi: f32,
    checker_colors: (u32, u32),
    window: &mut Window,
) -> Result<(), String> {
    if !dpi.is_finite() || dpi <= 0. {
        return Err(tr!("预览显示密度无效", "Invalid preview display density").into());
    }
    let source_dimensions = geometry.normalized_source_dimensions();
    if source_dimensions != viewport.source_dimensions() {
        return Err(tr!(
            "预览视口来源不匹配",
            "Preview viewport source does not match"
        )
        .into());
    }
    let domain = Rect::full(source_dimensions.0, source_dimensions.1);
    let full = project(domain, viewport)?;
    let grid = CheckerGrid::new(full, area, f64::from(dpi)).map_err(|error| error.to_string())?;
    let detail_region = detail.map(|surface| surface.geometry.normalized_source_region());
    let masks = overview_regions(source_dimensions, detail_region).map_err(|e| e.to_string())?;
    let overview_plan = LayerPlan::new(
        domain,
        geometry.raster_dimensions(),
        viewport,
        masks,
        area,
        dpi,
        false,
    )?;
    check_image(overview, geometry.raster_dimensions())?;
    let detail_plan = detail
        .map(|surface| {
            if surface.geometry.normalized_source_dimensions() != source_dimensions {
                return Err(invalid());
            }
            let region = surface.geometry.normalized_source_region();
            let raster = surface.geometry.raster_dimensions();
            check_image(&surface.image, raster)?;
            LayerPlan::new(
                region,
                raster,
                viewport,
                [Some(region), None, None, None],
                area,
                dpi,
                raster == (region.width, region.height) && viewport.zoom() >= 1.,
            )
        })
        .transpose()?;
    // 所有回退格数与坐标先检查，再发出绘制命令；失败不会只留下棋盘。
    let mut fallback_cells = 0;
    overview_plan.preflight(dpi, &mut fallback_cells)?;
    if let Some(plan) = &detail_plan {
        plan.preflight(dpi, &mut fallback_cells)?;
    }
    for cell in grid.clone() {
        bounds(cell.rect, dpi)?;
    }
    // 格子迭代器有固定上限，不创建一份临时格子Vec。
    for cell in grid {
        window.paint_quad(fill(
            bounds(cell.rect, dpi)?,
            rgb(if cell.alternate {
                checker_colors.1
            } else {
                checker_colors.0
            }),
        ));
    }
    overview_plan.paint(overview, dpi, window)?;
    if let (Some(detail), Some(plan)) = (detail, detail_plan) {
        plan.paint(&detail.image, dpi, window)?;
    }
    Ok(())
}

fn bounds(rect: PhysicalRect, dpi: f32) -> Result<Bounds<gpui::Pixels>, String> {
    // 差值与裁边已在f64完成，最后一步才转GPUI的窗口局部坐标。
    let values =
        [rect.left, rect.top, rect.width, rect.height].map(|value| (value / f64::from(dpi)) as f32);
    let right = values[0] + values[2];
    let bottom = values[1] + values[3];
    let physical = values.map(|value| value * dpi);
    let physical_right = physical[0] + physical[2];
    let physical_bottom = physical[1] + physical[3];
    if values.iter().any(|value| !value.is_finite())
        || !right.is_finite()
        || !bottom.is_finite()
        || values[2] <= 0.
        || values[3] <= 0.
        || right <= values[0]
        || bottom <= values[1]
        || physical.iter().any(|value| !value.is_finite())
        || !physical_right.is_finite()
        || !physical_bottom.is_finite()
        || physical_right <= physical[0]
        || physical_bottom <= physical[1]
    {
        return Err(tr!(
            "预览显示边界无法表示",
            "Preview display bounds are not representable"
        )
        .into());
    }
    Ok(Bounds::new(
        point(px(values[0]), px(values[1])),
        size(px(values[2]), px(values[3])),
    ))
}

enum LayerPlan {
    Texture {
        projected: PhysicalRect,
        clips: [Option<PhysicalRect>; 4],
        nearest: bool,
    },
    Texels(TexelPlan),
}

impl LayerPlan {
    fn new(
        domain: Rect,
        raster: (u32, u32),
        viewport: PreviewViewport,
        masks: [Option<Rect>; 4],
        area: Rect,
        dpi: f32,
        nearest: bool,
    ) -> Result<Self, String> {
        let projected = project(domain, viewport)?;
        if texture_is_precise(projected, dpi) {
            let mut clips = [None; 4];
            for (index, mask) in masks.into_iter().enumerate() {
                if let Some(mask) = mask {
                    clips[index] = clip_to_area(project(mask, viewport)?, area);
                }
            }
            Ok(Self::Texture {
                projected,
                clips,
                nearest,
            })
        } else {
            Ok(Self::Texels(TexelPlan::new(
                domain, raster, viewport, masks, area,
            )?))
        }
    }

    fn preflight(&self, dpi: f32, count: &mut u64) -> Result<(), String> {
        match self {
            Self::Texture {
                projected, clips, ..
            } => {
                bounds(*projected, dpi)?;
                for clip in clips.iter().flatten() {
                    bounds(*clip, dpi)?;
                }
            }
            Self::Texels(plan) => {
                for cell in plan.cells() {
                    *count = count.checked_add(1).ok_or_else(fallback_limit)?;
                    if *count > MAX_FALLBACK_CELLS {
                        return Err(fallback_limit());
                    }
                    bounds(cell.rect, dpi)?;
                }
            }
        }
        Ok(())
    }

    fn paint(&self, image: &Arc<RenderImage>, dpi: f32, window: &mut Window) -> Result<(), String> {
        match self {
            Self::Texture {
                projected,
                clips,
                nearest,
            } => {
                let image_bounds = bounds(*projected, dpi)?;
                for clip in clips.iter().flatten() {
                    let mask_bounds = bounds(*clip, dpi)?;
                    window
                        .with_content_mask(
                            Some(ContentMask {
                                bounds: mask_bounds,
                            }),
                            |window| {
                                if *nearest {
                                    window.paint_image_nearest_exact(
                                        image_bounds.clone(),
                                        px(0.).into(),
                                        image.clone(),
                                        0,
                                        false,
                                    )
                                } else {
                                    window.paint_image_exact(
                                        image_bounds.clone(),
                                        px(0.).into(),
                                        image.clone(),
                                        0,
                                        false,
                                    )
                                }
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
            }
            Self::Texels(plan) => {
                let bytes = image.as_bytes(0).ok_or_else(invalid)?;
                // 极端平移时直接画现有采样格，采用最近格颜色；不创建新图片或整图副本。
                for cell in plan.cells() {
                    let index =
                        (u64::from(cell.y) * u64::from(plan.raster.0) + u64::from(cell.x)) * 4;
                    let index = usize::try_from(index).map_err(|_| invalid())?;
                    let pixel = bytes.get(index..index + 4).ok_or_else(invalid)?;
                    window.paint_quad(fill(bounds(cell.rect, dpi)?, rgba(rgba_word(pixel))));
                }
            }
        }
        Ok(())
    }
}

/// 同时核窗口逻辑边与逐项f32缩放后的物理边，避免缩放再次舍入后消抵。
fn texture_is_precise(rect: PhysicalRect, dpi: f32) -> bool {
    if !dpi.is_finite() || dpi <= 0. {
        return false;
    }
    let scale = f64::from(dpi);
    let [left, top, width, height] =
        [rect.left, rect.top, rect.width, rect.height].map(|value| (value / scale) as f32);
    let logical_edges =
        [left, top, left + width, top + height].map(|value| f64::from(value) * scale);
    let [left, top, width, height] = [left, top, width, height].map(|value| value * dpi);
    let physical_edges = [left, top, left + width, top + height].map(f64::from);
    let expected = [
        rect.left,
        rect.top,
        rect.left + rect.width,
        rect.top + rect.height,
    ];
    [logical_edges, physical_edges].into_iter().all(|edges| {
        edges.into_iter().zip(expected).all(|(actual, expected)| {
            actual.is_finite()
                && expected.is_finite()
                && (actual - expected).abs() <= MAX_TEXTURE_ERROR
        })
    })
}

#[derive(Clone, Copy)]
struct CellRange {
    first_x: u32,
    end_x: u32,
    first_y: u32,
    end_y: u32,
    clip: PhysicalRect,
}

#[derive(Clone, Copy)]
struct TexelPlan {
    domain: Rect,
    raster: (u32, u32),
    viewport: PreviewViewport,
    ranges: [Option<CellRange>; 4],
}

impl TexelPlan {
    fn new(
        domain: Rect,
        raster: (u32, u32),
        viewport: PreviewViewport,
        masks: [Option<Rect>; 4],
        area: Rect,
    ) -> Result<Self, String> {
        project(domain, viewport)?;
        if raster.0 == 0 || raster.1 == 0 || area.width == 0 || area.height == 0 {
            return Err(invalid());
        }
        area.x.checked_add(area.width).ok_or_else(invalid)?;
        area.y.checked_add(area.height).ok_or_else(invalid)?;
        let mut ranges = [None; 4];
        let mut candidates = 0u64;
        for (index, mask) in masks.into_iter().enumerate() {
            let Some(mask) = mask else { continue };
            let Some(clip) = clip_to_area(project(mask, viewport)?, area) else {
                continue;
            };
            let first = viewport
                .source_point((clip.left, clip.top))
                .ok_or_else(invalid)?;
            let last = viewport
                .source_point((clip.left + clip.width, clip.top + clip.height))
                .ok_or_else(invalid)?;
            let (first_x, end_x) = index_range(first.0, last.0, domain.x, domain.width, raster.0);
            let (first_y, end_y) = index_range(first.1, last.1, domain.y, domain.height, raster.1);
            candidates = candidates
                .checked_add(u64::from(end_x - first_x) * u64::from(end_y - first_y))
                .ok_or_else(fallback_limit)?;
            if candidates > MAX_CANDIDATES {
                return Err(fallback_limit());
            }
            ranges[index] = Some(CellRange {
                first_x,
                end_x,
                first_y,
                end_y,
                clip,
            });
        }
        Ok(Self {
            domain,
            raster,
            viewport,
            ranges,
        })
    }

    fn cells(self) -> TexelCells {
        TexelCells {
            plan: self,
            range: 0,
            cursor: None,
        }
    }
}

/// 邻接一格只处理逆变换的最后位误差；候选访问与实际发出格数分别有固定上限。
fn index_range(first: f64, last: f64, origin: u32, length: u32, samples: u32) -> (u32, u32) {
    let index = |value: f64| {
        ((value - f64::from(origin)) / f64::from(length) * f64::from(samples))
            .clamp(0., f64::from(samples))
    };
    let start = (index(first).floor() as u32).saturating_sub(1);
    let end = (index(last).ceil() as u32).saturating_add(1).min(samples);
    (start.min(end), end)
}

#[derive(Clone, Copy, Debug)]
struct TexelCell {
    x: u32,
    y: u32,
    rect: PhysicalRect,
}

struct TexelCells {
    plan: TexelPlan,
    range: usize,
    cursor: Option<(u32, u32)>,
}

impl Iterator for TexelCells {
    type Item = TexelCell;

    fn next(&mut self) -> Option<Self::Item> {
        while self.range < self.plan.ranges.len() {
            let Some(range) = self.plan.ranges[self.range] else {
                self.range += 1;
                continue;
            };
            let (x, y) = self.cursor.unwrap_or((range.first_x, range.first_y));
            if x >= range.end_x || y >= range.end_y {
                self.range += 1;
                self.cursor = None;
                continue;
            }
            self.cursor = Some(if x + 1 < range.end_x {
                (x + 1, y)
            } else {
                (range.first_x, y + 1)
            });
            let source = |x: u32, y: u32| {
                (
                    f64::from(self.plan.domain.x)
                        + f64::from(self.plan.domain.width) * f64::from(x)
                            / f64::from(self.plan.raster.0),
                    f64::from(self.plan.domain.y)
                        + f64::from(self.plan.domain.height) * f64::from(y)
                            / f64::from(self.plan.raster.1),
                )
            };
            let first = self.plan.viewport.window_point(source(x, y))?;
            let last = self.plan.viewport.window_point(source(x + 1, y + 1))?;
            let rect = PhysicalRect {
                left: first.0,
                top: first.1,
                width: last.0 - first.0,
                height: last.1 - first.1,
            };
            if let Some(rect) = intersection(rect, range.clip) {
                return Some(TexelCell { x, y, rect });
            }
        }
        None
    }
}

fn project(rect: Rect, viewport: PreviewViewport) -> Result<PhysicalRect, String> {
    viewport
        .projected_bounds(rect)
        .map_err(|error| error.to_string())?;
    let right = rect.x.checked_add(rect.width).ok_or_else(invalid)?;
    let bottom = rect.y.checked_add(rect.height).ok_or_else(invalid)?;
    let first = viewport
        .window_point((f64::from(rect.x), f64::from(rect.y)))
        .ok_or_else(invalid)?;
    let last = viewport
        .window_point((f64::from(right), f64::from(bottom)))
        .ok_or_else(invalid)?;
    if last.0 <= first.0 || last.1 <= first.1 {
        return Err(invalid());
    }
    Ok(PhysicalRect {
        left: first.0,
        top: first.1,
        width: last.0 - first.0,
        height: last.1 - first.1,
    })
}

fn check_image(image: &RenderImage, raster: (u32, u32)) -> Result<(), String> {
    let size = image.size(0);
    let bytes = u64::from(raster.0)
        .checked_mul(u64::from(raster.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(invalid)?;
    if raster.0 == 0
        || raster.1 == 0
        || (u32::from(size.width), u32::from(size.height)) != raster
        || image
            .as_bytes(0)
            .is_none_or(|data| data.len() as u64 != bytes)
    {
        return Err(invalid());
    }
    Ok(())
}

fn rgba_word(bgra: &[u8]) -> u32 {
    u32::from(bgra[2]) << 24
        | u32::from(bgra[1]) << 16
        | u32::from(bgra[0]) << 8
        | u32::from(bgra[3])
}

fn intersection(rect: PhysicalRect, clip: PhysicalRect) -> Option<PhysicalRect> {
    let left = rect.left.max(clip.left);
    let top = rect.top.max(clip.top);
    let right = (rect.left + rect.width).min(clip.left + clip.width);
    let bottom = (rect.top + rect.height).min(clip.top + clip.height);
    (right > left && bottom > top).then_some(PhysicalRect {
        left,
        top,
        width: right - left,
        height: bottom - top,
    })
}

fn invalid() -> String {
    tr!(
        "预览绘制来源或坐标无效",
        "Invalid preview paint source or coordinates"
    )
    .into()
}

fn fallback_limit() -> String {
    tr!(
        "此预览位置需要过多采样格，请调整缩放或位置",
        "Too many preview cells; adjust zoom or position"
    )
    .into()
}

#[cfg(test)]
#[path = "image_viewer_preview_paint_tests.rs"]
mod tests;

fn clip_to_area(rect: PhysicalRect, area: Rect) -> Option<PhysicalRect> {
    let left = rect.left.max(f64::from(area.x));
    let top = rect.top.max(f64::from(area.y));
    let right = (rect.left + rect.width).min(f64::from(area.x) + f64::from(area.width));
    let bottom = (rect.top + rect.height).min(f64::from(area.y) + f64::from(area.height));
    (right > left && bottom > top).then_some(PhysicalRect {
        left,
        top,
        width: right - left,
        height: bottom - top,
    })
}
