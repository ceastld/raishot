//! 工具预览共用的源物理像素坐标；绘制、命中与标记使用同一内容矩形。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelPoint {
    pub x: u32,
    pub y: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ContentRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ContentRect {
    fn valid(self) -> bool {
        [
            self.x,
            self.y,
            self.width,
            self.height,
            self.x + self.width,
            self.y + self.height,
        ]
        .iter()
        .all(|value| value.is_finite())
            && self.width > 0.
            && self.height > 0.
            && self.x + self.width > self.x
            && self.y + self.height > self.y
    }

    pub(crate) fn contain(self, width: u32, height: u32) -> Option<Self> {
        if !self.valid() || width == 0 || height == 0 {
            return None;
        }
        let scale = (self.width / f64::from(width)).min(self.height / f64::from(height));
        let drawn_width = f64::from(width) * scale;
        let drawn_height = f64::from(height) * scale;
        let drawn = Self {
            x: self.x + (self.width - drawn_width) / 2.,
            y: self.y + (self.height - drawn_height) / 2.,
            width: drawn_width,
            height: drawn_height,
        };
        drawn.valid().then_some(drawn)
    }

    /// 半开边界；留白及图像边界外不钳制为源像素。
    pub(crate) fn pixel(self, width: u32, height: u32, x: f64, y: f64) -> Option<PixelPoint> {
        if !self.valid()
            || width == 0
            || height == 0
            || !x.is_finite()
            || !y.is_finite()
            || x < self.x
            || y < self.y
            || x >= self.x + self.width
            || y >= self.y + self.height
        {
            return None;
        }
        Some(PixelPoint {
            x: (((x - self.x) / self.width * f64::from(width)).floor() as u32).min(width - 1),
            y: (((y - self.y) / self.height * f64::from(height)).floor() as u32).min(height - 1),
        })
    }

    pub(crate) fn center(self, width: u32, height: u32, point: PixelPoint) -> Option<(f64, f64)> {
        self.image_point(width, height, f64::from(point.x), f64::from(point.y))
    }

    /// 小数坐标仍以像素中心为基准；无效检测端点直接拒绝，不捏造边缘位置。
    pub(crate) fn image_point(self, width: u32, height: u32, x: f64, y: f64) -> Option<(f64, f64)> {
        if !self.valid() || width == 0 || height == 0 || !x.is_finite() || !y.is_finite() {
            return None;
        }
        if x < 0. || y < 0. || x > f64::from(width - 1) || y > f64::from(height - 1) {
            return None;
        }
        Some((
            self.x + (x + 0.5) / f64::from(width) * self.width,
            self.y + (y + 0.5) / f64::from(height) * self.height,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tool_geometry_fractional_endpoints_stay_in_source_and_preserve_letterbox() {
        let content = ContentRect {
            x: 10.,
            y: 20.,
            width: 300.,
            height: 300.,
        }
        .contain(4, 2)
        .unwrap();
        assert_eq!(content.image_point(4, 2, 1.5, 0.5), Some((160., 170.)));
        assert_eq!(
            content.image_point(4, 2, 3., 1.),
            content.center(4, 2, PixelPoint { x: 3, y: 1 })
        );
        for (x, y) in [
            (-0.01, 0.),
            (3.01, 0.),
            (0., 1.01),
            (f64::NAN, 0.),
            (0., f64::INFINITY),
        ] {
            assert!(content.image_point(4, 2, x, y).is_none());
        }
        assert!(content.pixel(4, 2, 160., 94.99).is_none());
        assert!(content.image_point(0, 2, 0., 0.).is_none());
    }
}
