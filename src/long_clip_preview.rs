//! 可见剪辑片段独立取原图小块；隐藏的大间隔不参与显示纹理的像素预算。
use crate::{
    capture::Image, capture_view::CaptureView, geometry::Rect,
    long_clip_projection::LongClipProjection, scene::SourceIdentity, viewport::Viewport, Result,
};
use gpui::{RenderImage, Window};
use std::{collections::BTreeSet, sync::Arc};

const TILE_SIDE: u32 = 1024;
const MAX_PREVIEW_PIXELS: u64 = 8_000_000;

struct PreviewTile {
    /// 纹理完整采样格的原图范围，边界格可略超原图；绘制仍由片段与原图 mask 裁剪。
    bounds: Rect,
    source: Rect,
    image: Arc<RenderImage>,
}

#[derive(Default)]
pub(crate) struct LongClipPreviews {
    identity: Option<SourceIdentity>,
    step: u32,
    dpi: u32,
    tiles: Vec<PreviewTile>,
}

struct PreviewPlan {
    source: Vec<Rect>,
    step: u32,
}

fn preview_plan(
    projection: &LongClipProjection,
    viewport: Viewport,
    host: Rect,
) -> Result<PreviewPlan> {
    if !viewport.zoom.is_finite() || viewport.zoom <= 0. {
        return Err("剪辑预览缩放无效".into());
    }
    if host.width == 0 || host.height == 0 {
        return Ok(PreviewPlan {
            source: Vec::new(),
            step: 1,
        });
    }
    let (width, height) = projection.source_size();
    let mut tiles = BTreeSet::new();
    for piece in projection.pieces() {
        let Some(visible) =
            crate::image_preview::visible_image_region(piece.display, viewport, host)?
        else {
            continue;
        };
        let source_x = i64::from(visible.x) - piece.translation.0;
        let source_y = i64::from(visible.y) - piece.translation.1;
        if source_x < 0 || source_y < 0 {
            return Err("剪辑可见片段超出原图".into());
        }
        let source = Rect {
            x: source_x as u32,
            y: source_y as u32,
            width: visible.width,
            height: visible.height,
        };
        if !source.valid(width, height) {
            return Err("剪辑预览区域超出原图".into());
        }
        for y in source.y / TILE_SIDE..=(source.y + source.height - 1) / TILE_SIDE {
            for x in source.x / TILE_SIDE..=(source.x + source.width - 1) / TILE_SIDE {
                tiles.insert((y, x));
            }
        }
    }
    let source: Vec<_> = tiles
        .into_iter()
        .map(|(y, x)| Rect {
            x: x * TILE_SIDE,
            y: y * TILE_SIDE,
            width: TILE_SIDE.min(width - x * TILE_SIDE),
            height: TILE_SIDE.min(height - y * TILE_SIDE),
        })
        .collect();
    // 2 次幂网格始终锚定原图 0；Rect 可精确表示纹理边界，平移不会改变采样相位。
    let mut step = 1;
    while step < TILE_SIDE && f64::from(step * 2) * f64::from(viewport.zoom) <= 1. {
        step *= 2;
    }
    while target_pixels(&source, step) > MAX_PREVIEW_PIXELS {
        if step >= TILE_SIDE {
            return Err("剪辑可见预览超过纹理预算".into());
        }
        step *= 2;
    }
    Ok(PreviewPlan { source, step })
}

fn target_pixels(source: &[Rect], step: u32) -> u64 {
    source
        .iter()
        .map(|rect| u64::from(rect.width.div_ceil(step)) * u64::from(rect.height.div_ceil(step)))
        .sum()
}

fn render_tile(image: Image, source: Rect, step: u32) -> Result<(Rect, Image)> {
    if step == 0
        || !step.is_power_of_two()
        || step > TILE_SIDE
        || source.x % step != 0
        || source.y % step != 0
        || image.width != source.width
        || image.height != source.height
        || source.width == 0
        || source.height == 0
        || source.width > TILE_SIDE
        || source.height > TILE_SIDE
        || image.bgra.len() != image.width as usize * image.height as usize * 4
    {
        return Err("剪辑预览小块尺寸或采样格无效".into());
    }
    let width = source.width.div_ceil(step);
    let height = source.height.div_ceil(step);
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(width as usize * height as usize * 4)?;
    for y in 0..height {
        let from_y = y * step;
        let to_y = (from_y + step).min(source.height);
        for x in 0..width {
            let from_x = x * step;
            let to_x = (from_x + step).min(source.width);
            let mut sums = [0_u64; 3];
            for sy in from_y..to_y {
                for sx in from_x..to_x {
                    let at = (sy as usize * image.width as usize + sx as usize) * 4;
                    let pixel = &image.bgra[at..at + 4];
                    let alpha = u64::from(pixel[3]);
                    let background =
                        if ((source.x + sx) / 16 + (source.y + sy) / 16).is_multiple_of(2) {
                            224
                        } else {
                            192
                        };
                    for channel in 0..3 {
                        // 棋盘先按原图像素混合，再做面积平均；透明颜色不会污染显示。
                        sums[channel] +=
                            (u64::from(pixel[channel]) * alpha + background * (255 - alpha) + 127)
                                / 255;
                    }
                }
            }
            let count = u64::from(to_x - from_x) * u64::from(to_y - from_y);
            for sum in sums {
                bgra.push(((sum + count / 2) / count) as u8);
            }
            bgra.push(255);
        }
    }
    Ok((
        Rect {
            x: source.x,
            y: source.y,
            width: width * step,
            height: height * step,
        },
        Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        },
    ))
}

impl CaptureView {
    /// 只准备可见片段；每次最多读取 1024 × 1024 原图 ROI，原始 previews/SourcePixels 不变。
    pub(crate) fn prepare_long_clip_previews(&mut self, window: &Window) -> Result<()> {
        let Some(projection) = self.long_clip_projection() else {
            return Ok(());
        };
        let host = window.viewport_size();
        let dpi = window.scale_factor();
        if !dpi.is_finite() || dpi <= 0. {
            return Err("剪辑预览 DPI 无效".into());
        }
        let host = Rect::full(
            (f32::from(host.width) * dpi).round() as u32,
            (f32::from(host.height) * dpi).round() as u32,
        );
        let plan = preview_plan(&projection, self.canvas_viewport(window), host)?;
        let identity = SourceIdentity::of(&self.source_pixels());
        let Some(ui) = self.long_clip_ui.as_mut() else {
            return Ok(());
        };
        let mut previous = std::mem::take(&mut ui.piece_previews);
        let reuse = previous.identity.as_ref() == Some(&identity)
            && previous.step == plan.step
            && previous.dpi == dpi.to_bits();
        let mut next = Vec::with_capacity(plan.source.len());
        for source in plan.source {
            if reuse {
                if let Some(index) = previous.tiles.iter().position(|tile| tile.source == source) {
                    next.push(previous.tiles.remove(index));
                    continue;
                }
            }
            let rendered = self
                .source
                .read_region(source)
                .and_then(|image| render_tile(image, source, plan.step));
            match rendered {
                Ok((bounds, image)) => next.push(PreviewTile {
                    bounds,
                    source,
                    image: crate::canvas_source::render_image(
                        image.width,
                        image.height,
                        image.bgra,
                    ),
                }),
                Err(error) => {
                    // 失效的新投影不能沿用旧帧，也不能遗留未被当前帧引用的纹理。
                    self.retired_raster_previews.extend(
                        previous
                            .tiles
                            .into_iter()
                            .chain(next)
                            .map(|tile| tile.image),
                    );
                    return Err(error);
                }
            }
        }
        self.retired_raster_previews
            .extend(previous.tiles.into_iter().map(|tile| tile.image));
        if let Some(ui) = self.long_clip_ui.as_mut() {
            ui.piece_previews = LongClipPreviews {
                identity: Some(identity),
                step: plan.step,
                dpi: dpi.to_bits(),
                tiles: next,
            };
        }
        Ok(())
    }

    /// 返回原图坐标的纹理边界；调用方沿用该片段的 translation 和内容 mask。
    pub(crate) fn long_clip_piece_previews(
        &self,
        source_rect: Rect,
    ) -> Vec<(Rect, Arc<RenderImage>)> {
        self.long_clip_ui.as_ref().map_or_else(Vec::new, |ui| {
            ui.piece_previews
                .tiles
                .iter()
                .filter(|tile| tile.source.intersection(source_rect).is_some())
                .map(|tile| (tile.bounds, Arc::clone(&tile.image)))
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{annotation::Point, long_clip::LongClip, stitch::ScrollAxis};

    #[test]
    fn huge_deleted_gap_does_not_consume_preview_pixels() {
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let (width, height) = match axis {
                ScrollAxis::Vertical => (800, 65_000),
                ScrollAxis::Horizontal => (65_000, 800),
            };
            let mut clip = LongClip::new(width, height, axis).unwrap();
            clip.delete(1_000..64_000).unwrap();
            let host = match axis {
                ScrollAxis::Vertical => Rect::full(1_000, 1_200),
                ScrollAxis::Horizontal => Rect::full(1_200, 1_000),
            };
            let plan = preview_plan(
                &LongClipProjection::new(&clip, &[]),
                Viewport::default(),
                host,
            )
            .unwrap();
            assert_eq!(plan.step, 1);
            assert!(target_pixels(&plan.source, plan.step) < 3_000_000);
            assert!(plan.source.iter().all(|r| match axis {
                ScrollAxis::Vertical => r.y < 2_048 || r.y >= 63_488,
                ScrollAxis::Horizontal => r.x < 2_048 || r.x >= 63_488,
            }));
        }
    }

    #[test]
    fn visible_texture_budget_reduces_density_without_changing_grid_phase() {
        let clip = LongClip::new(4_000, 8_000, ScrollAxis::Vertical).unwrap();
        let plan = preview_plan(
            &LongClipProjection::new(&clip, &[]),
            Viewport::default(),
            Rect::full(4_000, 8_000),
        )
        .unwrap();
        assert_eq!(plan.step, 2);
        assert_eq!(target_pixels(&plan.source, plan.step), 8_000_000);
        assert!(plan
            .source
            .iter()
            .all(|r| r.x % plan.step == 0 && r.y % plan.step == 0));
    }

    #[test]
    fn offscreen_pieces_create_no_textures() {
        let clip = LongClip::new(100, 2_000, ScrollAxis::Vertical).unwrap();
        let viewport = Viewport {
            zoom: 1.,
            offset: Point { x: 200., y: 0. },
        };
        assert!(preview_plan(
            &LongClipProjection::new(&clip, &[]),
            viewport,
            Rect::full(100, 100)
        )
        .unwrap()
        .source
        .is_empty());
    }

    #[test]
    fn fractional_pan_reuses_the_same_source_grid_and_partial_edge_cell() {
        let clip = LongClip::new(1_501, 5_001, ScrollAxis::Vertical).unwrap();
        let projection = LongClipProjection::new(&clip, &[]);
        let a = Viewport {
            zoom: 0.37,
            offset: Point {
                x: 20.25,
                y: -110.5,
            },
        };
        let b = Viewport {
            zoom: 0.37,
            offset: Point {
                x: 20.75,
                y: -110.25,
            },
        };
        let a = preview_plan(&projection, a, Rect::full(600, 300)).unwrap();
        let b = preview_plan(&projection, b, Rect::full(600, 300)).unwrap();
        assert_eq!(a.step, 2);
        assert_eq!(a.source, b.source);
        let source = Rect {
            x: 1_024,
            y: 0,
            width: 3,
            height: 1,
        };
        let image = Image {
            width: 3,
            height: 1,
            bgra: vec![20, 20, 20, 255, 40, 40, 40, 255, 80, 80, 80, 255],
            readback_ms: 0.,
        };
        let (bounds, image) = render_tile(image, source, 2).unwrap();
        assert_eq!(
            bounds,
            Rect {
                x: 1_024,
                y: 0,
                width: 4,
                height: 2
            }
        );
        assert_eq!(image.bgra, [30, 30, 30, 255, 80, 80, 80, 255]);
    }

    #[test]
    fn tile_area_average_and_checker_use_original_coordinates() {
        let image = Image {
            width: 2,
            height: 2,
            bgra: vec![
                10, 20, 30, 255, 30, 40, 50, 255, 50, 60, 70, 255, 70, 80, 90, 255,
            ],
            readback_ms: 0.,
        };
        let (bounds, output) = render_tile(
            image,
            Rect {
                x: 16,
                y: 0,
                width: 2,
                height: 2,
            },
            2,
        )
        .unwrap();
        assert_eq!(
            bounds,
            Rect {
                x: 16,
                y: 0,
                width: 2,
                height: 2
            }
        );
        assert_eq!(output.bgra, [40, 50, 60, 255]);
        let transparent = Image {
            width: 2,
            height: 1,
            bgra: vec![255, 0, 0, 0, 0, 255, 0, 0],
            readback_ms: 0.,
        };
        let (_, output) = render_tile(
            transparent,
            Rect {
                x: 15,
                y: 0,
                width: 2,
                height: 1,
            },
            1,
        )
        .unwrap();
        assert_eq!(output.bgra, [224, 224, 224, 255, 192, 192, 192, 255]);
    }
}
