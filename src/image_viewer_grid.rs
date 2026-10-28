//! 图片查看覆盖层；网格始终按整图像素边界对齐，不读取或修改源像素。
use crate::{geometry::Rect, viewport::Viewport};
use gpui::{fill, point, px, rgba, size, Bounds, Window};

const MIN_ZOOM: f32 = 8.;
const MAX_LINES: usize = 2048;

#[derive(Clone, Copy, Debug)]
struct Segment {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

/// 返回完整图原点上的边界索引；包含左侧有一部分线宽落入裁剪区的边界。
fn visible_indices(
    origin: f64,
    zoom: f64,
    pixels: u32,
    clip_start: f64,
    clip_end: f64,
) -> Option<(u64, u64)> {
    let first = (((clip_start - 1. - origin) / zoom).floor() + 1.).max(0.);
    let last = (((clip_end - origin) / zoom).ceil() - 1.).min(f64::from(pixels));
    (first <= last && first <= f64::from(pixels) && last >= 0.)
        .then_some((first as u64, last as u64))
}

fn segments(viewport: Viewport, dimensions: (u32, u32), area: Rect) -> Vec<Segment> {
    if !viewport.zoom.is_finite()
        || viewport.zoom < MIN_ZOOM
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
        || dimensions.0 == 0
        || dimensions.1 == 0
        || area.width == 0
        || area.height == 0
    {
        return Vec::new();
    }
    let (Some(area_right), Some(area_bottom)) = (
        area.x.checked_add(area.width),
        area.y.checked_add(area.height),
    ) else {
        return Vec::new();
    };
    let zoom = f64::from(viewport.zoom);
    let origin = (f64::from(viewport.offset.x), f64::from(viewport.offset.y));
    let left = origin.0.max(f64::from(area.x));
    let top = origin.1.max(f64::from(area.y));
    let right = (origin.0 + f64::from(dimensions.0) * zoom).min(f64::from(area_right));
    let bottom = (origin.1 + f64::from(dimensions.1) * zoom).min(f64::from(area_bottom));
    if left >= right || top >= bottom {
        return Vec::new();
    }
    let vertical = visible_indices(origin.0, zoom, dimensions.0, left, right);
    let horizontal = visible_indices(origin.1, zoom, dimensions.1, top, bottom);
    let count = |range: Option<(u64, u64)>| range.map_or(0, |(first, last)| last - first + 1);
    let count = count(vertical) + count(horizontal);
    if count > MAX_LINES as u64 {
        // 超额整体隐藏，避免把抽稀网格误作真实的逐像素边界。
        return Vec::new();
    }
    let mut result = Vec::with_capacity(count as usize);
    if let Some((first, last)) = vertical {
        for index in first..=last {
            let boundary = origin.0 + index as f64 * zoom;
            let line_left = boundary.max(left);
            let line_right = (boundary + 1.).min(right);
            if line_right > line_left {
                result.push(Segment {
                    left: line_left,
                    top,
                    width: line_right - line_left,
                    height: bottom - top,
                });
            }
        }
    }
    if let Some((first, last)) = horizontal {
        for index in first..=last {
            let boundary = origin.1 + index as f64 * zoom;
            let line_top = boundary.max(top);
            let line_bottom = (boundary + 1.).min(bottom);
            if line_bottom > line_top {
                result.push(Segment {
                    left,
                    top: line_top,
                    width: right - left,
                    height: line_bottom - line_top,
                });
            }
        }
    }
    result
}

pub(super) fn paint(
    viewport: Viewport,
    dimensions: (u32, u32),
    area: Rect,
    dpi: f32,
    window: &mut Window,
) {
    if !dpi.is_finite() || dpi <= 0. {
        return;
    }
    let dpi = f64::from(dpi);
    // 极小的无效DPI不能让有限物理坐标在绘制API处溢出。
    if [
        (f64::from(area.x) + f64::from(area.width)) / dpi,
        (f64::from(area.y) + f64::from(area.height)) / dpi,
        1. / dpi,
    ]
    .into_iter()
    .any(|value| !(value as f32).is_finite())
    {
        return;
    }
    for segment in segments(viewport, dimensions, area) {
        window.paint_quad(fill(
            Bounds::new(
                point(
                    px((segment.left / dpi) as f32),
                    px((segment.top / dpi) as f32),
                ),
                size(
                    px((segment.width / dpi) as f32),
                    px((segment.height / dpi) as f32),
                ),
            ),
            rgba(0x80808099),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Point;

    #[test]
    fn image_viewer_grid_keeps_image_origin_and_clips_fractional_lines() {
        let viewport = Viewport {
            zoom: 8.,
            offset: Point { x: -13.25, y: -5.5 },
        };
        let area = Rect {
            x: 3,
            y: 2,
            width: 31,
            height: 24,
        };
        let lines = segments(viewport, (8, 6), area);
        assert_eq!(lines.len(), 7);
        assert_eq!((lines[0].left, lines[0].width), (3., 0.75));
        assert_eq!((lines[1].left, lines[1].width), (10.75, 1.));
        assert_eq!((lines[4].top, lines[4].height), (2.5, 1.));
        for line in &lines {
            assert!(line.left >= 3. && line.top >= 2.);
            assert!(line.left + line.width <= 34. && line.top + line.height <= 26.);
            assert!(line.width > 0. && line.height > 0.);
            assert!(line.width <= 1. || line.height <= 1.);
        }
        let shifted_clip = segments(viewport, (8, 6), Rect { x: 4, ..area });
        assert_eq!(shifted_clip[0].left, 10.75);
        let image_clip = segments(
            Viewport {
                zoom: 8.,
                offset: Point { x: 10., y: 20. },
            },
            (2, 2),
            Rect::full(100, 100),
        );
        assert_eq!(image_clip.len(), 4);
        for line in image_clip {
            assert!(line.left >= 10. && line.top >= 20.);
            assert!(line.left + line.width <= 26. && line.top + line.height <= 36.);
        }
    }

    #[test]
    fn image_viewer_grid_rejects_invalid_inputs_and_hides_over_budget_grids() {
        let viewport = Viewport {
            zoom: 8.,
            ..Viewport::default()
        };
        assert_eq!(
            segments(viewport, (1024, 1024), Rect::full(8192, 8192)).len(),
            MAX_LINES
        );
        assert!(segments(viewport, (1025, 1024), Rect::full(8200, 8192)).is_empty());
        for zoom in [7.99, 0., -8., f32::NAN, f32::INFINITY] {
            assert!(segments(Viewport { zoom, ..viewport }, (8, 8), Rect::full(80, 80)).is_empty());
        }
        for offset in [
            Point { x: f32::NAN, y: 0. },
            Point {
                x: 0.,
                y: f32::INFINITY,
            },
            Point { x: 500., y: 500. },
        ] {
            assert!(
                segments(Viewport { offset, ..viewport }, (8, 8), Rect::full(80, 80)).is_empty()
            );
        }
        assert!(segments(viewport, (0, 8), Rect::full(80, 80)).is_empty());
        assert!(segments(viewport, (8, 8), Rect::full(0, 80)).is_empty());
        assert!(segments(
            viewport,
            (8, 8),
            Rect {
                x: u32::MAX,
                y: 0,
                width: 1,
                height: 80
            }
        )
        .is_empty());
    }
}
