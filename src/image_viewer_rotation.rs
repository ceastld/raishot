//! EXIF归一后的图片显示投影；四分之一圈旋转不改变原图、编辑坐标或输出。
use crate::{
    annotation::Point,
    geometry::{Rect, Rotation},
    image_decode::{MAX_DECODE_BYTES, MAX_DECODE_PIXELS},
    viewport::Viewport,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ImageRotation {
    source: (u32, u32),
    rotation: Rotation,
}

pub(crate) struct RotatedTile {
    /// 旋转后整图坐标中的位置，同时给出本块的宽高。
    pub rect: Rect,
    pub bgra: Vec<u8>,
}

impl ImageRotation {
    /// 尺寸属于已应用EXIF方向的像素；本层不再解释源文件方向。
    pub(crate) fn new(source: (u32, u32), rotation: Rotation) -> Result<Self, String> {
        let (width, height) = source;
        let pixels = u64::from(width) * u64::from(height);
        if width == 0
            || height == 0
            || width > crate::export::MAX_DECODE_SIDE
            || height > crate::export::MAX_DECODE_SIDE
            || pixels > MAX_DECODE_PIXELS
            || pixels * 4 > MAX_DECODE_BYTES
        {
            return Err(tr!(
                "图片旋转尺寸超过查看预算",
                "Image rotation exceeds the viewing budget"
            )
            .into());
        }
        Ok(Self { source, rotation })
    }

    pub(crate) fn source_dimensions(self) -> (u32, u32) {
        self.source
    }

    pub(crate) fn display_dimensions(self) -> (u32, u32) {
        self.rotation.size(self.source.0, self.source.1)
    }

    pub(crate) fn rotation(self) -> Rotation {
        self.rotation
    }

    pub(crate) fn turned(self, clockwise: bool) -> Self {
        let rotation = match (self.rotation, clockwise) {
            (Rotation::Identity, true) | (Rotation::R180, false) => Rotation::R90,
            (Rotation::R90, true) | (Rotation::R270, false) => Rotation::R180,
            (Rotation::R180, true) | (Rotation::Identity, false) => Rotation::R270,
            (Rotation::R270, true) | (Rotation::R90, false) => Rotation::Identity,
        };
        Self { rotation, ..self }
    }

    /// 连续边界坐标：图片范围为[0,w]×[0,h]，像素中心为(x+0.5,y+0.5)。
    /// 图外坐标继续外推，使已平移到图外的窗口锚点不会因旋转跳动。
    pub(crate) fn source_to_display(self, point: Point) -> Point {
        let (width, height) = (self.source.0 as f32, self.source.1 as f32);
        let (x, y) = match self.rotation {
            Rotation::Identity => (point.x, point.y),
            Rotation::R90 => (height - point.y, point.x),
            Rotation::R180 => (width - point.x, height - point.y),
            Rotation::R270 => (point.y, width - point.x),
        };
        Point { x, y }
    }

    pub(crate) fn display_to_source(self, point: Point) -> Point {
        let (width, height) = (self.source.0 as f32, self.source.1 as f32);
        let (x, y) = match self.rotation {
            Rotation::Identity => (point.x, point.y),
            Rotation::R90 => (point.y, height - point.x),
            Rotation::R180 => (width - point.x, height - point.y),
            Rotation::R270 => (width - point.y, point.x),
        };
        Point { x, y }
    }

    /// 整数像素索引不包含右、下边界，与连续边界变换分开。
    pub(crate) fn source_pixel_to_display(self, x: u32, y: u32) -> Option<(u32, u32)> {
        if x >= self.source.0 || y >= self.source.1 {
            return None;
        }
        let (width, height) = self.display_dimensions();
        Some(inverse(self.rotation).raw_pixel(x, y, width, height))
    }

    pub(crate) fn display_pixel_to_source(self, x: u32, y: u32) -> Option<(u32, u32)> {
        let (width, height) = self.display_dimensions();
        if x >= width || y >= height {
            return None;
        }
        Some(self.rotation.raw_pixel(x, y, self.source.0, self.source.1))
    }

    /// 矩形是正尺寸的半开像素区域；先验边界，避免raw_rect的减一溢出。
    pub(crate) fn source_rect_to_display(self, rect: Rect) -> Option<Rect> {
        if !valid_rect(rect, self.source) {
            return None;
        }
        let (width, height) = self.display_dimensions();
        Some(inverse(self.rotation).raw_rect(rect, width, height))
    }

    pub(crate) fn display_rect_to_source(self, rect: Rect) -> Option<Rect> {
        if !valid_rect(rect, self.display_dimensions()) {
            return None;
        }
        Some(self.rotation.raw_rect(rect, self.source.0, self.source.1))
    }

    /// area和Viewport均使用窗口物理像素，调用方只做一次DPI转换。
    /// 手动视口保留区域中心对应的同一原图点；自动适应重新调用已有fit。
    /// 无效状态返回错误，不改传入视口。
    pub(crate) fn reorient_viewport(
        self,
        rotation: Rotation,
        viewport: &mut Viewport,
        area: Rect,
        auto_fit: bool,
    ) -> Result<Self, String> {
        if area.width == 0 || area.height == 0 {
            return Err(tr!("图片显示区域为空", "Image viewing area is empty").into());
        }
        let next = Self { rotation, ..self };
        let mut updated = *viewport;
        if auto_fit {
            let (width, height) = next.display_dimensions();
            updated.fit(Rect::full(width, height), area);
        } else {
            if !valid_viewport(updated) {
                return Err(tr!("图片视口无效", "Invalid image viewport").into());
            }
            let center = Point {
                x: area.x as f32 + area.width as f32 / 2.,
                y: area.y as f32 + area.height as f32 / 2.,
            };
            let source = self.display_to_source(updated.image_point(center));
            let display = next.source_to_display(source);
            updated.offset = Point {
                x: center.x - display.x * updated.zoom,
                y: center.y - display.y * updated.zoom,
            };
        }
        if !valid_viewport(updated) {
            return Err(tr!("图片旋转视口无效", "Invalid rotated viewport").into());
        }
        *viewport = updated;
        Ok(next)
    }

    /// 单块无损整数排列，按行取消；只创建当前方向的一份输出，不保存方向缓存。
    /// 调用方逐块替换显示，并沿用原图/预取各自的输出预算；编辑仍持有未旋转源。
    pub(crate) fn rotate_tile(
        self,
        source_rect: Rect,
        bgra: &[u8],
        max_output_bytes: u64,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<RotatedTile, String> {
        let rect = self.source_rect_to_display(source_rect).ok_or_else(|| {
            tr!("图片旋转块超出原图", "Rotated tile lies outside the image").to_string()
        })?;
        let bytes = u64::from(source_rect.width) * u64::from(source_rect.height) * 4;
        if bytes > max_output_bytes.min(MAX_DECODE_BYTES)
            || bytes * 2 > MAX_DECODE_BYTES
            || usize::try_from(bytes).ok() != Some(bgra.len())
        {
            return Err(tr!(
                "图片旋转块超过预算或像素不完整",
                "Rotated tile exceeds its budget or has incomplete pixels"
            )
            .into());
        }
        check_cancel(&mut cancelled)?;
        let mut output = Vec::new();
        output.try_reserve_exact(bgra.len()).map_err(|_| {
            tr!("图片旋转缓冲分配失败", "Rotated tile allocation failed").to_string()
        })?;
        output.resize(bgra.len(), 0);
        for y in 0..rect.height {
            check_cancel(&mut cancelled)?;
            for x in 0..rect.width {
                let (source_x, source_y) =
                    self.rotation
                        .raw_pixel(x, y, source_rect.width, source_rect.height);
                let source =
                    (source_y as usize * source_rect.width as usize + source_x as usize) * 4;
                let target = (y as usize * rect.width as usize + x as usize) * 4;
                output[target..target + 4].copy_from_slice(&bgra[source..source + 4]);
            }
        }
        check_cancel(&mut cancelled)?;
        Ok(RotatedTile { rect, bgra: output })
    }
}

fn inverse(rotation: Rotation) -> Rotation {
    match rotation {
        Rotation::Identity => Rotation::Identity,
        Rotation::R90 => Rotation::R270,
        Rotation::R180 => Rotation::R180,
        Rotation::R270 => Rotation::R90,
    }
}

fn valid_rect(rect: Rect, dimensions: (u32, u32)) -> bool {
    rect.width > 0 && rect.height > 0 && rect.valid(dimensions.0, dimensions.1)
}

fn valid_viewport(viewport: Viewport) -> bool {
    viewport.zoom.is_finite()
        && viewport.zoom > 0.
        && viewport.offset.x.is_finite()
        && viewport.offset.y.is_finite()
}

fn check_cancel(cancelled: &mut impl FnMut() -> bool) -> Result<(), String> {
    if cancelled() {
        Err(tr!("图片旋转已取消", "Image rotation canceled").into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROTATIONS: [Rotation; 4] = [
        Rotation::Identity,
        Rotation::R90,
        Rotation::R180,
        Rotation::R270,
    ];

    fn pixels(labels: &[u8]) -> Vec<u8> {
        labels
            .iter()
            .flat_map(|label| [*label, 71, 129, 193])
            .collect()
    }

    fn close(actual: Point, expected: Point) {
        assert!(
            actual.distance(expected) < 0.003,
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn image_viewer_rotation_boundaries_pixel_centers_and_outside_points_roundtrip() {
        for rotation in ROTATIONS {
            let projection = ImageRotation::new((2, 3), rotation).unwrap();
            for point in [
                Point { x: 0., y: 0. },
                Point { x: 2., y: 3. },
                Point { x: -4., y: 8. },
                Point { x: 0.5, y: 2.5 },
            ] {
                close(
                    projection.display_to_source(projection.source_to_display(point)),
                    point,
                );
            }
            for y in 0..3 {
                for x in 0..2 {
                    let (display_x, display_y) = projection.source_pixel_to_display(x, y).unwrap();
                    assert_eq!(
                        projection.display_pixel_to_source(display_x, display_y),
                        Some((x, y))
                    );
                    close(
                        projection.source_to_display(Point {
                            x: x as f32 + 0.5,
                            y: y as f32 + 0.5,
                        }),
                        Point {
                            x: display_x as f32 + 0.5,
                            y: display_y as f32 + 0.5,
                        },
                    );
                }
            }
            assert_eq!(projection.source_pixel_to_display(2, 0), None);
            let (width, height) = projection.display_dimensions();
            assert_eq!(projection.display_pixel_to_source(width, 0), None);
            assert_eq!(projection.display_pixel_to_source(0, height), None);
        }
        let projection = ImageRotation::new((2, 3), Rotation::R90).unwrap();
        close(
            projection.source_to_display(Point { x: 0., y: 0. }),
            Point { x: 3., y: 0. },
        );
        assert_eq!(projection.source_pixel_to_display(0, 0), Some((2, 0)));
    }

    #[test]
    fn image_viewer_rotation_integer_bgra_preserves_every_channel() {
        let source = pixels(b"ABCDEF");
        for (rotation, expected) in [
            (Rotation::Identity, b"ABCDEF"),
            (Rotation::R90, b"ECAFDB"),
            (Rotation::R180, b"FEDCBA"),
            (Rotation::R270, b"BDFACE"),
        ] {
            let projection = ImageRotation::new((2, 3), rotation).unwrap();
            let tile = projection
                .rotate_tile(Rect::full(2, 3), &source, MAX_DECODE_BYTES, || false)
                .unwrap();
            assert_eq!(
                (tile.rect.width, tile.rect.height),
                projection.display_dimensions()
            );
            assert_eq!(tile.bgra, pixels(expected));
        }
        assert_eq!(source, pixels(b"ABCDEF"));
    }

    #[test]
    fn image_viewer_rotation_separate_tiles_reassemble_in_global_display_coordinates() {
        for rotation in ROTATIONS {
            let projection = ImageRotation::new((3, 2), rotation).unwrap();
            let mut output = vec![0; 24];
            let (display_width, _) = projection.display_dimensions();
            for (rect, bytes) in [
                (
                    Rect {
                        x: 0,
                        y: 0,
                        width: 2,
                        height: 2,
                    },
                    pixels(b"ABDE"),
                ),
                (
                    Rect {
                        x: 2,
                        y: 0,
                        width: 1,
                        height: 2,
                    },
                    pixels(b"CF"),
                ),
            ] {
                let tile = projection
                    .rotate_tile(rect, &bytes, MAX_DECODE_BYTES, || false)
                    .unwrap();
                assert_eq!(projection.display_rect_to_source(tile.rect), Some(rect));
                for y in 0..tile.rect.height as usize {
                    let source = y * tile.rect.width as usize * 4;
                    let target = ((tile.rect.y as usize + y) * display_width as usize
                        + tile.rect.x as usize)
                        * 4;
                    let length = tile.rect.width as usize * 4;
                    output[target..target + length]
                        .copy_from_slice(&tile.bgra[source..source + length]);
                }
            }
            let full = projection
                .rotate_tile(
                    Rect::full(3, 2),
                    &pixels(b"ABCDEF"),
                    MAX_DECODE_BYTES,
                    || false,
                )
                .unwrap();
            assert_eq!(output, full.bgra);
        }
    }

    #[test]
    fn image_viewer_rotation_manual_turns_keep_same_source_point_at_physical_center() {
        let area = Rect {
            x: 24,
            y: 120,
            width: 1600,
            height: 900,
        };
        let center = Point { x: 824., y: 570. };
        let mut projection = ImageRotation::new((640, 480), Rotation::Identity).unwrap();
        let mut viewport = Viewport {
            zoom: 0.37,
            offset: Point { x: 120., y: -42. },
        };
        let original = viewport;
        let source = projection.display_to_source(viewport.image_point(center));
        assert!(source.x > 640. && source.y > 480.);
        for _ in 0..4 {
            projection = projection
                .reorient_viewport(
                    projection.turned(true).rotation(),
                    &mut viewport,
                    area,
                    false,
                )
                .unwrap();
            assert_eq!(viewport.zoom, original.zoom);
            close(
                viewport.window_point(projection.source_to_display(source)),
                center,
            );
        }
        close(viewport.offset, original.offset);
    }

    #[test]
    fn image_viewer_rotation_auto_fit_uses_rotated_dimensions_and_existing_viewport() {
        let projection = ImageRotation::new((2400, 1200), Rotation::Identity).unwrap();
        let area = Rect {
            x: 20,
            y: 60,
            width: 900,
            height: 500,
        };
        let mut viewport = Viewport::default();
        let next = projection
            .reorient_viewport(Rotation::R90, &mut viewport, area, true)
            .unwrap();
        assert_eq!(next.source_dimensions(), (2400, 1200));
        assert_eq!(next.display_dimensions(), (1200, 2400));
        assert!((viewport.zoom - 500. / 2400.).abs() < f32::EPSILON);
        close(
            viewport.window_point(Point { x: 600., y: 1200. }),
            Point { x: 470., y: 310. },
        );
        let back = next.turned(false);
        assert_eq!(back.display_dimensions(), projection.display_dimensions());
        close(
            back.source_to_display(Point { x: 14., y: 27. }),
            Point { x: 14., y: 27. },
        );
    }

    #[test]
    fn image_viewer_rotation_rejects_invalid_geometry_budgets_and_cancelled_rows() {
        for dimensions in [(0, 1), (1, 0), (65_536, 1), (8000, 4001)] {
            assert!(ImageRotation::new(dimensions, Rotation::R90).is_err());
        }
        assert!(ImageRotation::new((8000, 4000), Rotation::R90).is_ok());
        let projection = ImageRotation::new((2, 3), Rotation::Identity).unwrap();
        for rect in [
            Rect::full(0, 1),
            Rect {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 1,
            },
            Rect::full(3, 3),
        ] {
            assert_eq!(projection.source_rect_to_display(rect), None);
        }
        let bytes = pixels(b"ABCDEF");
        assert!(projection
            .rotate_tile(Rect::full(2, 3), &bytes, 23, || false)
            .is_err());
        assert!(projection
            .rotate_tile(Rect::full(2, 3), &bytes[..20], MAX_DECODE_BYTES, || false)
            .is_err());
        let mut calls = 0;
        assert!(projection
            .rotate_tile(Rect::full(2, 3), &bytes, MAX_DECODE_BYTES, || {
                calls += 1;
                calls == 3
            })
            .is_err());
        assert_eq!(calls, 3);
        assert_eq!(bytes, pixels(b"ABCDEF"));
        let area = Rect::full(100, 100);
        for mut viewport in [
            Viewport {
                zoom: 0.,
                ..Default::default()
            },
            Viewport {
                zoom: f32::INFINITY,
                ..Default::default()
            },
            Viewport {
                offset: Point { x: f32::NAN, y: 0. },
                ..Default::default()
            },
        ] {
            let before = (
                viewport.zoom.to_bits(),
                viewport.offset.x.to_bits(),
                viewport.offset.y.to_bits(),
            );
            assert!(projection
                .reorient_viewport(Rotation::R90, &mut viewport, area, false)
                .is_err());
            assert_eq!(
                (
                    viewport.zoom.to_bits(),
                    viewport.offset.x.to_bits(),
                    viewport.offset.y.to_bits()
                ),
                before
            );
        }
    }
}
