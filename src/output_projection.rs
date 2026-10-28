//! 用户输出的尺寸预算和坐标投影；不修改文档、视口或原始像素。
use crate::{
    annotation::{Bounds, Point},
    capture::Image,
    geometry::Rect,
    scene::SourcePixels,
    Result,
};

pub(crate) const MAX_OUTPUT_PIXELS: u64 = 32_000_000;
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputProjection {
    pub source: Rect,
    pub width: u32,
    pub height: u32,
    pub sx: f32,
    pub sy: f32,
    /// 当前目标块在完整输出栅格中的原点；裁块不能重置抗锯齿与文字相位。
    pub origin: Point,
}
impl OutputProjection {
    pub fn new(source: Rect, zoom: f32) -> Result<Self> {
        if source.width == 0 || source.height == 0 || !zoom.is_finite() || zoom <= 0. {
            return Err("导出范围或缩放比例无效".into());
        }
        let scale = f64::from(zoom.max(1.));
        let width = (f64::from(source.width) * scale).round_ties_even().max(1.);
        let height = (f64::from(source.height) * scale).round_ties_even().max(1.);
        if width > 65_535. || height > 65_535. || width * height > MAX_OUTPUT_PIXELS as f64 {
            return Err("放大导出超过 3200 万像素 / 65535 单边限制，请降低贴图缩放比例".into());
        }
        let (width, height) = (width as u32, height as u32);
        Ok(Self {
            source,
            width,
            height,
            sx: width as f32 / source.width as f32,
            sy: height as f32 / source.height as f32,
            origin: Point { x: 0., y: 0. },
        })
    }
    pub fn map(&self, p: Point) -> Point {
        Point {
            x: (p.x - self.source.x as f32) * self.sx - self.origin.x,
            y: (p.y - self.source.y as f32) * self.sy - self.origin.y,
        }
    }
    pub fn unmap(&self, p: Point) -> Point {
        Point {
            x: (p.x + self.origin.x) / self.sx + self.source.x as f32,
            y: (p.y + self.origin.y) / self.sy + self.source.y as f32,
        }
    }
    pub fn source_center(&self, x: u32, y: u32) -> Point {
        self.unmap(Point {
            x: x as f32 + 0.5,
            y: y as f32 + 0.5,
        })
    }
    pub fn bounds(&self, b: Bounds, padding: f32) -> Option<Rect> {
        let p = self.map(Point { x: b.x, y: b.y });
        let left = (p.x - padding).floor().clamp(0., self.width as f32) as u32;
        let top = (p.y - padding).floor().clamp(0., self.height as f32) as u32;
        let right = (p.x + b.width * self.sx + padding)
            .ceil()
            .clamp(0., self.width as f32) as u32;
        let bottom = (p.y + b.height * self.sy + padding)
            .ceil()
            .clamp(0., self.height as f32) as u32;
        (right > left && bottom > top).then(|| Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
    pub fn identity(&self) -> bool {
        self.width == self.source.width
            && self.height == self.source.height
            && self.sx == 1.
            && self.sy == 1.
            && self.origin == (Point { x: 0., y: 0. })
    }
    pub fn background(&self, source: &SourcePixels<'_>) -> Result<Image> {
        let len = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("导出像素大小溢出")?;
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(len)?;
        bgra.resize(len, 0);
        for y in 0..self.height {
            for x in 0..self.width {
                let p = source.sample(self.source_center(x, y));
                let i = (y as usize * self.width as usize + x as usize) * 4;
                bgra[i..i + 4].copy_from_slice(&p);
            }
        }
        Ok(Image {
            width: self.width,
            height: self.height,
            bgra,
            readback_ms: 0.,
        })
    }
}
impl SourcePixels<'_> {
    pub(crate) fn sample(&self, p: Point) -> [u8; 4] {
        let mut pixel = self.fill.unwrap_or([0, 0, 0, 255]);
        if !p.x.is_finite() || !p.y.is_finite() || p.x < 0. || p.y < 0. {
            return pixel;
        }
        let (x, y) = (p.x.floor() as u32, p.y.floor() as u32);
        for plane in self.planes.iter().chain(self.overlay.iter()) {
            if x >= plane.rect.x
                && y >= plane.rect.y
                && x - plane.rect.x < plane.rect.width
                && y - plane.rect.y < plane.rect.height
            {
                let i = ((y - plane.rect.y) as usize * plane.rect.width as usize
                    + (x - plane.rect.x) as usize)
                    * 4;
                if let Some(bytes) = plane.bgra.get(i..i + 4) {
                    pixel.copy_from_slice(bytes);
                }
            }
        }
        pixel
    }
}
pub(crate) fn window_opacity(image: &mut Image, opacity: u8) {
    if opacity == 255 {
        return;
    }
    for p in image.bgra.chunks_exact_mut(4) {
        p[3] = ((u32::from(p[3]) * u32::from(opacity) + 127) / 255) as u8;
        if p[3] == 0 {
            p.fill(0);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enlarged_export_plan_rounding_budget_and_minimum_source_size() {
        for z in [0.1, 0.5, 1.] {
            let p = OutputProjection::new(Rect::full(13, 7), z).unwrap();
            assert!(p.identity());
        }
        let p = OutputProjection::new(Rect::full(1, 3), 1.5).unwrap();
        assert_eq!((p.width, p.height), (2, 4));
        assert_eq!(p.map(Point { x: 1., y: 3. }), Point { x: 2., y: 4. });
        assert!(OutputProjection::new(Rect::full(1000, 32_000), 1.).is_ok());
        for (r, z) in [
            (Rect::full(1000, 32_000), 1.01),
            (Rect::full(1, 65_535), 2.),
            (Rect::full(8, 8), f32::MAX),
            (Rect::full(8, 8), f32::NAN),
            (Rect::full(8, 8), 0.),
        ] {
            assert!(OutputProjection::new(r, z).is_err());
        }
    }
    #[test]
    fn enlarged_export_nearest_crop_planes_and_alpha_once() {
        let bytes = [
            10, 20, 30, 128, 40, 50, 60, 255, 70, 80, 90, 0, 100, 110, 120, 255,
        ];
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(4, 1),
            planes: vec![crate::scene::PixelPlane {
                rect: Rect::full(4, 1),
                bgra: &bytes,
            }],
            overlay: None,
        };
        let p = OutputProjection::new(
            Rect {
                x: 1,
                y: 0,
                width: 2,
                height: 1,
            },
            2.,
        )
        .unwrap();
        let mut i = p.background(&source).unwrap();
        assert_eq!(&i.bgra[..8], &[40, 50, 60, 255, 40, 50, 60, 255]);
        window_opacity(&mut i, 128);
        assert_eq!(&i.bgra[..4], &[40, 50, 60, 128]);
        assert_eq!(&i.bgra[8..12], &[0, 0, 0, 0]);
        assert_eq!(source.sample(Point { x: 0., y: 0. }), [10, 20, 30, 128]);
        let clipped = p
            .bounds(
                Bounds {
                    x: -20.,
                    y: -10.,
                    width: 23.,
                    height: 11.,
                },
                1.,
            )
            .unwrap();
        assert_eq!(clipped, Rect::full(4, 2));
    }
    #[test]
    fn enlarged_export_lasso_target_edges_and_crop_phase() {
        let mask = crate::crop_mask::Polygon::new(&[
            Point { x: 2.3, y: 1.2 },
            Point { x: 22.6, y: 5.4 },
            Point { x: 5.7, y: 20.8 },
        ])
        .unwrap();
        let make = |w, h| Image {
            width: w,
            height: h,
            bgra: [50, 80, 100, 255].repeat((w * h) as usize),
            readback_ms: 0.,
        };
        let mut full = make(96, 96);
        let p = OutputProjection::new(Rect::full(24, 24), 4.).unwrap();
        mask.apply_projected(&mut full, &p);
        let crop = Rect {
            x: 4,
            y: 3,
            width: 12,
            height: 11,
        };
        let mut clipped = make(48, 44);
        let q = OutputProjection::new(crop, 4.).unwrap();
        mask.apply_projected(&mut clipped, &q);
        for y in 0..44 {
            let begin = ((y + 12) * 96 + 16) as usize * 4;
            assert_eq!(
                &clipped.bgra[y as usize * 48 * 4..(y as usize + 1) * 48 * 4],
                &full.bgra[begin..begin + 48 * 4]
            );
        }
        let mut one = make(24, 24);
        mask.apply(&mut one, Rect::full(24, 24));
        let source = SourcePixels {
            fill: None,
            canvas: Rect::full(24, 24),
            planes: vec![crate::scene::PixelPlane {
                rect: Rect::full(24, 24),
                bgra: &one.bgra,
            }],
            overlay: None,
        };
        let nearest = p.background(&source).unwrap();
        assert_ne!(full.bgra, nearest.bgra, "套索目标边缘不能最近邻放大旧mask");
        assert!(
            full.bgra
                .chunks_exact(4)
                .filter(|p| p[3] > 0 && p[3] < 255)
                .count()
                > 30
        );
    }
}
