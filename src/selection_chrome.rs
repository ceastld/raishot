//! 选区辅助 UI 的物理像素几何；外框在选区外，圆点居中叠加，不改变数据范围。
use crate::geometry::Rect;

pub(crate) const COLOR: u32 = 0x1498ff;
pub(crate) const FRAME_WIDTH: i64 = 3;
pub(crate) const HANDLE_SIZE: i64 = 12;
pub(crate) const HANDLE_BORDER: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelBox {
    pub left: i64,
    pub top: i64,
    pub right: i64,
    pub bottom: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HandleCircle {
    pub center: (f64, f64),
}

impl HandleCircle {
    pub fn bounds(self) -> PixelBox {
        let radius = HANDLE_SIZE as f64 / 2.;
        PixelBox {
            left: (self.center.0 - radius).floor() as i64,
            top: (self.center.1 - radius).floor() as i64,
            right: (self.center.0 + radius).ceil() as i64,
            bottom: (self.center.1 + radius).ceil() as i64,
        }
    }

    /// GDI 按像素中心采样完整圆，再裁剪每行；屏边不会把半个圆挪回选区。
    pub fn spans(self, inset: i64) -> impl Iterator<Item = PixelBox> {
        let radius = (HANDLE_SIZE as f64 / 2. - inset as f64).max(0.);
        let top = (self.center.1 - radius).floor() as i64;
        let bottom = (self.center.1 + radius).ceil() as i64;
        (top..bottom).filter_map(move |y| {
            let dy = y as f64 + 0.5 - self.center.1;
            if radius <= 0. || dy.abs() > radius {
                return None;
            }
            let dx = (radius * radius - dy * dy).max(0.).sqrt();
            let left = (self.center.0 - dx - 0.5).ceil() as i64;
            let right = (self.center.0 + dx - 0.5).floor() as i64 + 1;
            (left < right).then_some(PixelBox {
                left,
                top: y,
                right,
                bottom: y + 1,
            })
        })
    }
}

pub(crate) struct SelectionChrome(pub PixelBox, [HandleCircle; 8]);

impl SelectionChrome {
    pub fn projected(rect: Rect, zoom: f64, offset: (f64, f64)) -> Self {
        let left = offset.0 + f64::from(rect.x) * zoom;
        let top = offset.1 + f64::from(rect.y) * zoom;
        let right = offset.0 + (f64::from(rect.x) + f64::from(rect.width)) * zoom;
        let bottom = offset.1 + (f64::from(rect.y) + f64::from(rect.height)) * zoom;
        let mid_x = (left + right) / 2.;
        let mid_y = (top + bottom) / 2.;
        Self(
            PixelBox {
                left: left.floor() as i64,
                top: top.floor() as i64,
                right: right.ceil() as i64,
                bottom: bottom.ceil() as i64,
            },
            [
                (left, top),
                (mid_x, top),
                (right, top),
                (left, mid_y),
                (right, mid_y),
                (left, bottom),
                (mid_x, bottom),
                (right, bottom),
            ]
            .map(|center| HandleCircle { center }),
        )
    }

    pub fn frame(&self) -> [PixelBox; 4] {
        let PixelBox {
            left,
            top,
            right,
            bottom,
        } = self.0;
        [
            PixelBox {
                left: left - FRAME_WIDTH,
                top: top - FRAME_WIDTH,
                right: right + FRAME_WIDTH,
                bottom: top,
            },
            PixelBox {
                left: left - FRAME_WIDTH,
                top,
                right: left,
                bottom,
            },
            PixelBox {
                left: right,
                top,
                right: right + FRAME_WIDTH,
                bottom,
            },
            PixelBox {
                left: left - FRAME_WIDTH,
                top: bottom,
                right: right + FRAME_WIDTH,
                bottom: bottom + FRAME_WIDTH,
            },
        ]
    }

    pub fn handles(&self) -> [HandleCircle; 8] {
        self.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_stays_outside_crop_and_round_handles_center_on_real_corners_and_midpoints() {
        for rect in [
            Rect::full(1, 1),
            Rect::full(257, 193),
            Rect {
                x: 39,
                y: 47,
                width: 127,
                height: 91,
            },
        ] {
            let chrome = SelectionChrome::projected(rect, 1., (0., 0.));
            let crop = chrome.0;
            for painted in chrome.frame() {
                assert!(
                    painted.right <= crop.left
                        || painted.left >= crop.right
                        || painted.bottom <= crop.top
                        || painted.top >= crop.bottom
                );
            }
            let [top, left, right, bottom] = chrome.frame();
            assert_eq!(top.bottom, crop.top);
            assert_eq!(left.right, crop.left);
            assert_eq!(right.left, crop.right);
            assert_eq!(bottom.top, crop.bottom);
            assert_eq!(top.bottom - top.top, FRAME_WIDTH);
            assert_eq!(left.right - left.left, FRAME_WIDTH);
            let handles = chrome.handles();
            assert_eq!(handles[0].center, (f64::from(rect.x), f64::from(rect.y)));
            assert_eq!(
                handles[1].center,
                (
                    f64::from(rect.x) + f64::from(rect.width) / 2.,
                    f64::from(rect.y)
                )
            );
            assert_eq!(
                handles[7].center,
                (
                    f64::from(rect.x) + f64::from(rect.width),
                    f64::from(rect.y) + f64::from(rect.height)
                )
            );
            let corner = handles[0].bounds();
            assert!(corner.left < crop.left && corner.right > crop.left);
            assert!(corner.top < crop.top && corner.bottom > crop.top);
            assert_eq!(
                chrome.0,
                PixelBox {
                    left: i64::from(rect.x),
                    top: i64::from(rect.y),
                    right: i64::from(rect.x) + i64::from(rect.width),
                    bottom: i64::from(rect.y) + i64::from(rect.height),
                }
            );
        }
    }

    #[test]
    fn zoomed_and_negative_projection_encloses_fractional_edges() {
        let rect = Rect {
            x: 17,
            y: 19,
            width: 31,
            height: 27,
        };
        for zoom in [0.7, 1., 1.25, 1.5, 2.75] {
            let offset = (-25.3, -11.7);
            let chrome = SelectionChrome::projected(rect, zoom, offset);
            assert!(chrome.0.left as f64 <= offset.0 + 17. * zoom);
            assert!(chrome.0.top as f64 <= offset.1 + 19. * zoom);
            assert!(chrome.0.right as f64 >= offset.0 + 48. * zoom);
            assert!(chrome.0.bottom as f64 >= offset.1 + 46. * zoom);
            assert_eq!(
                chrome.handles()[0].center,
                (offset.0 + 17. * zoom, offset.1 + 19. * zoom)
            );
            assert!((chrome.handles()[1].center.0 - (offset.0 + 32.5 * zoom)).abs() < 1e-9);
            assert!((chrome.handles()[1].center.1 - (offset.1 + 19. * zoom)).abs() < 1e-9);
            for handle in chrome.handles() {
                let bounds = handle.bounds();
                assert!(bounds.right - bounds.left >= HANDLE_SIZE);
                assert!(bounds.right - bounds.left <= HANDLE_SIZE + 1);
                assert!(bounds.bottom - bounds.top >= HANDLE_SIZE);
                assert!(bounds.bottom - bounds.top <= HANDLE_SIZE + 1);
            }
        }
    }

    #[test]
    fn native_round_spans_have_white_center_blue_ring_and_unpainted_square_corners() {
        let circle = HandleCircle { center: (30., 40.) };
        let contains = |inset, point: (i64, i64)| {
            circle.spans(inset).any(|row| {
                point.0 >= row.left
                    && point.0 < row.right
                    && point.1 >= row.top
                    && point.1 < row.bottom
            })
        };
        assert!(contains(0, (30, 40)) && contains(HANDLE_BORDER, (30, 40)));
        assert!(contains(0, (24, 40)) && !contains(HANDLE_BORDER, (24, 40)));
        assert!(!contains(0, (24, 34)));
        assert_eq!(
            circle.spans(0).next().unwrap(),
            PixelBox {
                left: 28,
                top: 34,
                right: 32,
                bottom: 35
            }
        );
        assert_eq!(circle.spans(HANDLE_BORDER).count(), 8);
        assert_eq!(circle.spans(0).count(), 12);
        assert!(circle.spans(HANDLE_SIZE).next().is_none());
    }

    #[test]
    fn odd_size_midpoints_and_large_native_coordinates_keep_circle_centers() {
        let chrome = SelectionChrome::projected(
            Rect {
                x: 39,
                y: 47,
                width: 127,
                height: 91,
            },
            1.,
            (0., 0.),
        );
        assert_eq!(chrome.handles()[1].center, (102.5, 47.));
        assert_eq!(chrome.handles()[3].center, (39., 92.5));
        let limit = i32::MAX as u32;
        let chrome = SelectionChrome::projected(
            Rect {
                x: limit - 1,
                y: limit - 1,
                width: 1,
                height: 1,
            },
            1.,
            (0., 0.),
        );
        assert_eq!(
            chrome.handles()[7].center,
            (f64::from(limit), f64::from(limit))
        );
        assert_eq!(
            chrome.handles()[7].bounds().right,
            i64::from(limit) + HANDLE_SIZE / 2
        );
        assert!(chrome.handles()[7]
            .spans(0)
            .all(|span| span.left > 0 && span.right > span.left && span.bottom == span.top + 1));
    }
}
