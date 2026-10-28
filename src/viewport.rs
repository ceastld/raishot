//! 原图像素与窗口物理像素之间的视口；缩放/平移不改文档、选区或历史。
use crate::{annotation::Point, geometry::Rect};
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub zoom: f32,
    pub offset: Point,
}
impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: 1.,
            offset: Point { x: 0., y: 0. },
        }
    }
}
impl Viewport {
    pub fn image_point(self, window: Point) -> Point {
        Point {
            x: (window.x - self.offset.x) / self.zoom,
            y: (window.y - self.offset.y) / self.zoom,
        }
    }
    pub fn window_point(self, image: Point) -> Point {
        Point {
            x: self.offset.x + image.x * self.zoom,
            y: self.offset.y + image.y * self.zoom,
        }
    }
    pub fn set_zoom(&mut self, zoom: f32, anchor: Point) {
        if !zoom.is_finite() || zoom <= 0. {
            return;
        }
        let point = self.image_point(anchor);
        self.zoom = zoom.clamp(0.0001, 8.);
        self.offset = Point {
            x: anchor.x - point.x * self.zoom,
            y: anchor.y - point.y * self.zoom,
        };
    }
    pub fn fit(&mut self, image: Rect, area: Rect) {
        self.zoom = (area.width as f32 / image.width.max(1) as f32)
            .min(area.height as f32 / image.height.max(1) as f32)
            .clamp(0.0001, 1.);
        self.offset = Point {
            x: area.x as f32 + (area.width as f32 - image.width as f32 * self.zoom) / 2.
                - image.x as f32 * self.zoom,
            y: area.y as f32 + (area.height as f32 - image.height as f32 * self.zoom) / 2.
                - image.y as f32 * self.zoom,
        };
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zoom_keeps_original_pixel_under_pointer_and_one_to_one_roundtrips() {
        let mut v = Viewport {
            zoom: 0.375,
            offset: Point { x: 53.5, y: 77.25 },
        };
        let anchor = Point {
            x: 363.25,
            y: 242.75,
        };
        let original = v.image_point(anchor);
        for zoom in [0.5, 1., 2., 8., 0.01, 0.375] {
            v.set_zoom(zoom, anchor);
            assert!(v.image_point(anchor).distance(original) < 0.003);
            assert!(v.window_point(original).distance(anchor) < 0.003);
        }
        let before = v;
        v.set_zoom(f32::NAN, anchor);
        assert_eq!(v, before);
    }
    #[test]
    fn fit_large_transparent_image_preserves_native_size_and_centers_in_available_area() {
        let image = Rect::full(4000, 2000);
        let area = Rect {
            x: 30,
            y: 60,
            width: 1200,
            height: 600,
        };
        let mut v = Viewport::default();
        v.fit(image, area);
        assert_eq!(v.zoom, 0.3);
        assert_eq!(v.offset, Point { x: 30., y: 60. });
        // 缩放逆变换允许单精度舍入误差，误差必须远小于一个原图像素。
        assert!(
            v.image_point(Point { x: 1230., y: 660. })
                .distance(Point { x: 4000., y: 2000. })
                < 0.001
        );
        v.fit(Rect::full(100, 100), area);
        assert_eq!(v.zoom, 1.);
    }

    #[test]
    fn fits_extreme_long_image_below_one_percent_without_losing_anchor() {
        let mut v = Viewport::default();
        let area = Rect {
            x: 20,
            y: 80,
            width: 800,
            height: 400,
        };
        v.fit(Rect::full(64, 65535), area);
        assert!(v.zoom < 0.01 && v.zoom * 65535. <= 400.01);
        let anchor = Point { x: 420., y: 280. };
        let image = v.image_point(anchor);
        v.set_zoom(v.zoom * 1.2, anchor);
        assert!(v.image_point(anchor).distance(image) < 0.01);
    }
}
