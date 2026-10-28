#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    pub fn drag(a: (i32, i32), b: (i32, i32), w: u32, h: u32) -> Self {
        let x1 = a.0.min(b.0).clamp(0, w as i32) as u32;
        let y1 = a.1.min(b.1).clamp(0, h as i32) as u32;
        let x2 = a.0.max(b.0).clamp(0, w as i32) as u32;
        let y2 = a.1.max(b.1).clamp(0, h as i32) as u32;
        Self {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        }
    }

    pub fn valid(self, w: u32, h: u32) -> bool {
        self.width > 0
            && self.height > 0
            && self.x.checked_add(self.width).is_some_and(|v| v <= w)
            && self.y.checked_add(self.height).is_some_and(|v| v <= h)
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > x && bottom > y).then(|| Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }

    pub fn contains(self, p: (i32, i32)) -> bool {
        p.0 >= self.x as i32
            && p.1 >= self.y as i32
            && p.0 < (self.x + self.width) as i32
            && p.1 < (self.y + self.height) as i32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

impl ScreenRect {
    pub fn union(screens: &[Self]) -> Option<Self> {
        let left = screens.iter().map(|r| r.left).min()?;
        let top = screens.iter().map(|r| r.top).min()?;
        let right = screens
            .iter()
            .map(|r| r.left as i64 + r.width as i64)
            .max()?;
        let bottom = screens
            .iter()
            .map(|r| r.top as i64 + r.height as i64)
            .max()?;
        let width = u32::try_from(right - left as i64).ok()?;
        let height = u32::try_from(bottom - top as i64).ok()?;
        (width > 0 && height > 0 && width <= i32::MAX as u32 && height <= i32::MAX as u32)
            .then_some(Self {
                left,
                top,
                width,
                height,
            })
    }

    pub fn local(self, desktop: Self) -> Rect {
        Rect {
            x: (self.left as i64 - desktop.left as i64) as u32,
            y: (self.top as i64 - desktop.top as i64) as u32,
            width: self.width,
            height: self.height,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Rotation {
    Identity,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn size(self, w: u32, h: u32) -> (u32, u32) {
        match self {
            Self::R90 | Self::R270 => (h, w),
            _ => (w, h),
        }
    }

    pub fn raw_pixel(self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32) {
        match self {
            Self::Identity => (x, y),
            Self::R90 => (y, h - 1 - x),
            Self::R180 => (w - 1 - x, h - 1 - y),
            Self::R270 => (w - 1 - y, x),
        }
    }

    pub fn raw_rect(self, r: Rect, w: u32, h: u32) -> Rect {
        let (a, b) = self.raw_pixel(r.x, r.y, w, h);
        let (c, d) = self.raw_pixel(r.x + r.width - 1, r.y + r.height - 1, w, h);
        Rect {
            x: a.min(c),
            y: b.min(d),
            width: a.abs_diff(c) + 1,
            height: b.abs_diff(d) + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_clamps_negative_and_reverse_coordinates() {
        assert_eq!(
            Rect::drag((110, 70), (-8, -9), 100, 60),
            Rect::full(100, 60)
        );
        assert_eq!(
            Rect::drag((40, 50), (10, 20), 100, 60),
            Rect {
                x: 10,
                y: 20,
                width: 30,
                height: 30
            }
        );
        assert!(!Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1
        }
        .valid(100, 60));
    }

    #[test]
    fn all_rotations_crop_only_selected_pixels() {
        for rotation in [
            Rotation::Identity,
            Rotation::R90,
            Rotation::R180,
            Rotation::R270,
        ] {
            let (w, h) = rotation.size(7, 5);
            for y in 0..h {
                for x in 0..w {
                    let r = Rect {
                        x,
                        y,
                        width: w - x,
                        height: h - y,
                    };
                    let raw = rotation.raw_rect(r, 7, 5);
                    assert!(raw.valid(7, 5));
                    assert_eq!(raw.width * raw.height, r.width * r.height);
                    for sy in y..h {
                        for sx in x..w {
                            let (rx, ry) = rotation.raw_pixel(sx, sy, 7, 5);
                            assert!((raw.x..raw.x + raw.width).contains(&rx));
                            assert!((raw.y..raw.y + raw.height).contains(&ry));
                        }
                    }
                }
            }
        }
        assert_eq!(Rotation::R90.raw_pixel(0, 0, 7, 5), (0, 4));
        assert_eq!(Rotation::R270.raw_pixel(0, 0, 7, 5), (6, 0));
    }
}
