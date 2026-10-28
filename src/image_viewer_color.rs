//! 预览合成取色：包括透明棋盘格背景，只报告显示RGB，不推断源alpha。
use crate::{
    annotation::Point, geometry::Rect, image_viewer_rotation::ImageRotation, viewport::Viewport,
};
use gpui::RenderImage;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PreviewColor {
    pub display: (u32, u32),
    pub source: (u32, u32),
    pub rgb: [u8; 3],
}

impl PreviewColor {
    pub(super) fn hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.rgb[0], self.rgb[1], self.rgb[2])
    }
}

/// point和area使用窗口物理像素；先裁可见区域，再经视口落到显示像素格。
/// tiles必须是当前方向的显示图块；这里只借用像素，不打开文件或复制整图。
pub(super) fn sample(
    tiles: &[(Rect, Arc<RenderImage>)],
    viewport: Viewport,
    projection: ImageRotation,
    area: Rect,
    point: Point,
) -> Option<PreviewColor> {
    let right = area.x.checked_add(area.width)?;
    let bottom = area.y.checked_add(area.height)?;
    if area.width == 0
        || area.height == 0
        || !point.x.is_finite()
        || !point.y.is_finite()
        || point.x < area.x as f32
        || point.y < area.y as f32
        || point.x >= right as f32
        || point.y >= bottom as f32
        || !viewport.zoom.is_finite()
        || viewport.zoom <= 0.
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
    {
        return None;
    }
    let pixel = viewport.image_point(point);
    let (width, height) = projection.display_dimensions();
    if !pixel.x.is_finite()
        || !pixel.y.is_finite()
        || pixel.x < 0.
        || pixel.y < 0.
        || pixel.x >= width as f32
        || pixel.y >= height as f32
    {
        return None;
    }
    let display = (pixel.x.floor() as u32, pixel.y.floor() as u32);
    let source = projection.display_pixel_to_source(display.0, display.1)?;
    let (rect, image) = tiles.iter().find(|(rect, _)| {
        rect.valid(width, height)
            && display.0 >= rect.x
            && display.0 < rect.x + rect.width
            && display.1 >= rect.y
            && display.1 < rect.y + rect.height
    })?;
    let bytes = image.as_bytes(0)?;
    let dimensions = image.size(0);
    let expected_bytes = u64::from(rect.width) * u64::from(rect.height) * 4;
    if u32::from(dimensions.width) != rect.width
        || u32::from(dimensions.height) != rect.height
        || bytes.len() as u64 != expected_bytes
    {
        return None;
    }
    let index =
        (u64::from(display.1 - rect.y) * u64::from(rect.width) + u64::from(display.0 - rect.x)) * 4;
    let index = usize::try_from(index).ok()?;
    let pixel = bytes.get(index..index.checked_add(4)?)?;
    Some(PreviewColor {
        display,
        source,
        rgb: [pixel[2], pixel[1], pixel[0]],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rotation;

    const A: [u8; 4] = [10, 20, 30, 255];
    const B: [u8; 4] = [11, 21, 31, 255];
    const C: [u8; 4] = [12, 22, 32, 255];
    const D: [u8; 4] = [13, 23, 33, 255];
    const E: [u8; 4] = [3, 2, 1, 255];
    const F: [u8; 4] = [50, 100, 200, 255];

    fn tile(rect: Rect, pixels: &[[u8; 4]]) -> (Rect, Arc<RenderImage>) {
        (
            rect,
            crate::canvas_source::render_image(
                rect.width,
                rect.height,
                pixels.iter().flatten().copied().collect(),
            ),
        )
    }

    #[test]
    fn image_viewer_color_rotated_display_maps_source_and_bgra_without_copying() {
        // 2×3原图AB/CD/EF，顺时针90度后的显示块为ECA/FDB。
        let full = vec![tile(Rect::full(3, 2), &[E, C, A, F, D, B])];
        let split = vec![
            tile(Rect::full(1, 2), &[E, F]),
            tile(
                Rect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                &[C, A, D, B],
            ),
        ];
        let viewport = Viewport {
            zoom: 2.5,
            offset: Point { x: -1.25, y: -0.75 },
        };
        let projection = ImageRotation::new((2, 3), Rotation::R90).unwrap();
        for tiles in [&full, &split] {
            let first = sample(
                tiles,
                viewport,
                projection,
                Rect::full(10, 10),
                Point { x: 0., y: 0.5 },
            )
            .unwrap();
            assert_eq!(first.display, (0, 0));
            assert_eq!(first.source, (0, 2));
            assert_eq!(first.hex(), "#010203");
            let last = sample(
                tiles,
                viewport,
                projection,
                Rect::full(10, 10),
                Point { x: 5., y: 3. },
            )
            .unwrap();
            assert_eq!(last.display, (2, 1));
            assert_eq!(last.source, (1, 0));
            assert_eq!(last.rgb, [31, 21, 11]);
            let blue = sample(
                tiles,
                viewport,
                projection,
                Rect::full(10, 10),
                Point { x: 0., y: 3. },
            )
            .unwrap();
            assert_eq!(blue.source, (1, 2));
            assert_eq!(blue.hex(), "#C86432");
        }
    }

    #[test]
    fn image_viewer_color_clips_area_and_rejects_invalid_or_missing_pixels() {
        let tiles = vec![tile(Rect::full(3, 2), &[E, C, A, F, D, B])];
        let projection = ImageRotation::new((2, 3), Rotation::R90).unwrap();
        let area = Rect {
            x: 10,
            y: 20,
            width: 3,
            height: 2,
        };
        let viewport = Viewport {
            zoom: 1.,
            offset: Point { x: 10., y: 20. },
        };
        let point = Point { x: 10., y: 20. };
        assert_eq!(
            sample(&tiles, viewport, projection, area, point)
                .unwrap()
                .rgb,
            [1, 2, 3]
        );
        assert_eq!(
            sample(
                &tiles,
                viewport,
                projection,
                area,
                Point {
                    x: 12.999,
                    y: 21.999
                }
            )
            .unwrap()
            .display,
            (2, 1)
        );
        for outside in [
            Point { x: 13., y: 20. },
            Point { x: 10., y: 22. },
            Point { x: 9.999, y: 20. },
            Point { x: 10., y: 19.999 },
            Point {
                x: f32::NAN,
                y: 20.,
            },
        ] {
            assert_eq!(sample(&tiles, viewport, projection, area, outside), None);
        }
        for invalid in [
            Viewport {
                zoom: 0.,
                ..viewport
            },
            Viewport {
                zoom: f32::NAN,
                ..viewport
            },
            Viewport {
                offset: Point {
                    x: f32::INFINITY,
                    y: 20.,
                },
                ..viewport
            },
            Viewport {
                offset: Point { x: 100., y: 20. },
                ..viewport
            },
        ] {
            assert_eq!(sample(&tiles, invalid, projection, area, point), None);
        }
        assert_eq!(sample(&[], viewport, projection, area, point), None);
        assert_eq!(
            sample(&tiles, viewport, projection, Rect::full(0, 2), point),
            None
        );
        assert_eq!(
            sample(
                &tiles,
                viewport,
                projection,
                Rect {
                    x: u32::MAX,
                    y: 0,
                    width: 2,
                    height: 1
                },
                point
            ),
            None
        );
        let wrong_size = vec![(
            Rect::full(3, 2),
            crate::canvas_source::render_image(3, 1, vec![0; 12]),
        )];
        assert_eq!(sample(&wrong_size, viewport, projection, area, point), None);
    }
}
