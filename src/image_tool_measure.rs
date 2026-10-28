//! 输入图物理像素的几何与取样；不依赖窗口缩放、主题或输出状态。
use crate::capture::Image;

pub(super) use crate::image_tool_view::geometry::{ContentRect, PixelPoint};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RulerTick {
    pub pixel: u32,
    pub offset: f64,
}

/// 刻度落在源像素中心；规则刻度至少间隔 80 逻辑像素，始终保留两端。
/// 小于 80 逻辑像素的整条标尺仍显示两端，单像素图仅显示一个中心刻度。
pub(super) fn ruler_ticks(length: u32, extent: f64) -> Vec<RulerTick> {
    const MAX_TICKS: usize = 32;
    const SPACING: f64 = 80.;
    if length == 0 || !extent.is_finite() || extent <= 0. {
        return Vec::new();
    }
    let tick = |pixel| RulerTick {
        pixel,
        offset: (f64::from(pixel) + 0.5) / f64::from(length) * extent,
    };
    if length == 1 {
        return vec![tick(0)];
    }
    let last = u64::from(length - 1);
    let mut step = 1_u64;
    let mut decade = 1_u64;
    while step < u64::from(length) {
        let count = 2 + (last - 1) / step;
        let spacing = step as f64 / f64::from(length) * extent;
        if count <= MAX_TICKS as u64 && spacing >= SPACING {
            break;
        }
        // 输入最多 u32::MAX，序列最迟在 5_000_000_000 停下。
        step = match step / decade {
            1 => decade * 2,
            2 => decade * 5,
            _ => {
                decade *= 10;
                decade
            }
        };
    }
    let end = tick(length - 1);
    let mut ticks = Vec::with_capacity(MAX_TICKS);
    ticks.push(tick(0));
    let mut pixel = step;
    while pixel < last && ticks.len() < MAX_TICKS - 1 {
        let next = tick(pixel as u32);
        // 尾端不一定是步距的整数倍；省略靠得过近的规则刻度，为末像素留标签位置。
        if end.offset - next.offset < SPACING {
            break;
        }
        ticks.push(next);
        pixel += step;
    }
    ticks.push(end);
    ticks
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PixelSample {
    pub point: PixelPoint,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub alpha: u8,
}

impl PixelSample {
    pub(super) fn hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

/// 直读源 BGRA 的通道及 alpha；不合成底色、不反预乘，也不取缩略图的像素。
pub(super) fn sample(image: &Image, point: PixelPoint) -> Option<PixelSample> {
    if point.x >= image.width || point.y >= image.height {
        return None;
    }
    let expected = u64::from(image.width)
        .checked_mul(u64::from(image.height))?
        .checked_mul(4)?;
    if usize::try_from(expected).ok()? != image.bgra.len() {
        return None;
    }
    let offset = u64::from(point.y)
        .checked_mul(u64::from(image.width))?
        .checked_add(u64::from(point.x))?
        .checked_mul(4)?;
    let offset = usize::try_from(offset).ok()?;
    let pixel = image.bgra.get(offset..offset.checked_add(4)?)?;
    Some(PixelSample {
        point,
        r: pixel[2],
        g: pixel[1],
        b: pixel[0],
        alpha: pixel[3],
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Measurement {
    pub dx: f64,
    pub dy: f64,
    pub distance: f64,
    /// 向右0°，向下90°；重合点没有方向。
    pub angle_degrees: Option<f64>,
}

pub(super) fn measure(a: PixelPoint, b: PixelPoint) -> Measurement {
    let dx = f64::from(b.x) - f64::from(a.x);
    let dy = f64::from(b.y) - f64::from(a.y);
    let angle_degrees = if dx == 0. && dy == 0. {
        None
    } else {
        let angle = dy.atan2(dx).to_degrees().rem_euclid(360.);
        Some(if angle == 0. { 0. } else { angle })
    };
    Measurement {
        dx,
        dy,
        distance: dx.hypot(dy),
        angle_degrees,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tool_measure_ruler_ticks_bound_extremes_and_keep_physical_endpoints() {
        assert!(ruler_ticks(0, 960.).is_empty());
        for extent in [0., -1., f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(ruler_ticks(20, extent).is_empty());
        }
        for (length, extent) in [
            (1, 960.),
            (2, 1.),
            (1002, 1002.),
            (50_000_000, 960.),
            (u32::MAX, f64::MAX),
            (u32::MAX, f64::MIN_POSITIVE),
            (u32::MAX, f64::from_bits(1)),
        ] {
            let ticks = ruler_ticks(length, extent);
            assert!(!ticks.is_empty() && ticks.len() <= 32);
            assert_eq!(ticks.first().unwrap().pixel, 0);
            assert_eq!(ticks.last().unwrap().pixel, length - 1);
            if length == 1 {
                assert_eq!(ticks.len(), 1);
            }
            assert!(ticks.iter().all(|tick| {
                tick.offset.is_finite() && tick.offset >= 0. && tick.offset <= extent
            }));
            assert!(ticks
                .windows(2)
                .all(|pair| { pair[0].pixel < pair[1].pixel && pair[0].offset <= pair[1].offset }));
            if ticks.len() > 2 {
                let mut step = ticks[1].pixel;
                while step % 10 == 0 {
                    step /= 10;
                }
                assert!(matches!(step, 1 | 2 | 5));
                assert!(ticks
                    .windows(2)
                    .all(|pair| pair[1].offset - pair[0].offset >= 80. - 1e-9));
            }
        }
    }

    #[test]
    fn image_tool_measure_ruler_offsets_roundtrip_through_content_pixel_mapping() {
        for (width, height, viewport_width, viewport_height) in [
            (4000, 3000, 960., 600.),
            (4, 2, 960., 600.),
            (65_535, 1, 960., 600.),
            (1, 65_535, 960., 600.),
            (u32::MAX, u32::MAX, 960., 600.),
            (1, 1, 10., 10.),
        ] {
            let content = ContentRect {
                x: 7.25,
                y: 12.5,
                width: viewport_width,
                height: viewport_height,
            }
            .contain(width, height)
            .unwrap();
            for tick in ruler_ticks(width, content.width) {
                let point = PixelPoint {
                    x: tick.pixel,
                    y: height / 2,
                };
                let center = content.center(width, height, point).unwrap();
                assert_eq!(content.x + tick.offset, center.0);
                assert_eq!(
                    content.pixel(width, height, content.x + tick.offset, center.1),
                    Some(point)
                );
            }
            for tick in ruler_ticks(height, content.height) {
                let point = PixelPoint {
                    x: width / 2,
                    y: tick.pixel,
                };
                let center = content.center(width, height, point).unwrap();
                assert_eq!(content.y + tick.offset, center.1);
                assert_eq!(
                    content.pixel(width, height, center.0, content.y + tick.offset),
                    Some(point)
                );
            }
        }
    }

    #[test]
    fn image_tool_measure_mapping_rejects_letterbox_and_maps_last_pixel() {
        let viewport = ContentRect {
            x: 10.,
            y: 20.,
            width: 300.,
            height: 300.,
        };
        let content = viewport.contain(4, 2).unwrap();
        assert_eq!(
            content,
            ContentRect {
                x: 10.,
                y: 95.,
                width: 300.,
                height: 150.
            }
        );
        assert_eq!(
            content.pixel(4, 2, 10., 95.),
            Some(PixelPoint { x: 0, y: 0 })
        );
        assert_eq!(
            content.pixel(4, 2, 309.9999, 244.9999),
            Some(PixelPoint { x: 3, y: 1 })
        );
        assert_eq!(content.pixel(4, 2, 10., 94.9999), None);
        assert_eq!(content.pixel(4, 2, 310., 200.), None);
        assert_eq!(content.pixel(4, 2, 20., 245.), None);
        assert_eq!(content.pixel(4, 2, f64::NAN, 100.), None);
        for point in [PixelPoint { x: 0, y: 0 }, PixelPoint { x: 3, y: 1 }] {
            let (x, y) = content.center(4, 2, point).unwrap();
            assert_eq!(content.pixel(4, 2, x, y), Some(point));
        }
        assert!(viewport.contain(0, 1).is_none());
        assert!(ContentRect {
            width: f64::INFINITY,
            ..viewport
        }
        .contain(1, 1)
        .is_none());
        assert!(ContentRect {
            x: 1e100,
            width: 1.,
            ..viewport
        }
        .contain(1, 1)
        .is_none());
    }

    #[test]
    fn image_tool_measure_samples_original_hidden_rgb_and_rejects_bad_storage() {
        let mut image = Image {
            width: 2,
            height: 1,
            bgra: vec![1, 2, 3, 255, 9, 8, 7, 0],
            readback_ms: 0.,
        };
        let last = sample(&image, PixelPoint { x: 1, y: 0 }).unwrap();
        assert_eq!((last.r, last.g, last.b, last.alpha), (7, 8, 9, 0));
        assert_eq!(last.hex(), "#070809");
        assert!(sample(&image, PixelPoint { x: 2, y: 0 }).is_none());
        image.bgra.pop();
        assert!(sample(&image, PixelPoint { x: 0, y: 0 }).is_none());
    }

    #[test]
    fn image_tool_measure_axes_distance_and_coincident_direction_are_explicit() {
        let a = PixelPoint { x: 10, y: 20 };
        let diagonal = measure(a, PixelPoint { x: 13, y: 24 });
        assert_eq!((diagonal.dx, diagonal.dy, diagonal.distance), (3., 4., 5.));
        assert!((diagonal.angle_degrees.unwrap() - 53.13010235415598).abs() < 1e-10);
        for (b, angle) in [
            (PixelPoint { x: 11, y: 20 }, 0.),
            (PixelPoint { x: 10, y: 21 }, 90.),
            (PixelPoint { x: 9, y: 20 }, 180.),
            (PixelPoint { x: 10, y: 19 }, 270.),
        ] {
            assert_eq!(measure(a, b).angle_degrees, Some(angle));
        }
        assert_eq!(measure(a, a).angle_degrees, None);
        assert_eq!(
            measure(
                PixelPoint { x: u32::MAX, y: 0 },
                PixelPoint { x: 0, y: u32::MAX }
            )
            .dx,
            -f64::from(u32::MAX)
        );
    }
}
