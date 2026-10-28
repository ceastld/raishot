use crate::{
    capture::{Image, Snapshot},
    geometry::{Rect, ScreenRect},
    Result,
};
use std::time::Instant;
use windows::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

pub struct DesktopCapture {
    pub screens: Vec<Snapshot>,
    pub bounds: ScreenRect,
    pub current: usize,
    pub capture_ms: f64,
}

impl DesktopCapture {
    pub fn capture() -> Result<Self> {
        Self::capture_with_initial_recovery(None)
    }

    pub(crate) fn capture_initial(cancelled: &dyn Fn() -> bool) -> Result<Self> {
        Self::capture_with_initial_recovery(Some(cancelled))
    }

    fn capture_with_initial_recovery(cancelled: Option<&dyn Fn() -> bool>) -> Result<Self> {
        let started = Instant::now();
        let screens = match cancelled {
            Some(cancelled) => Snapshot::capture_all_initial(cancelled)?,
            None => Snapshot::capture_all()?,
        };
        let areas: Vec<_> = screens
            .iter()
            .map(|screen| ScreenRect {
                left: screen.desktop.left,
                top: screen.desktop.top,
                width: screen.width,
                height: screen.height,
            })
            .collect();
        let bounds = ScreenRect::union(&areas).ok_or("虚拟桌面尺寸无效")?;
        let mut cursor = POINT::default();
        unsafe {
            GetCursorPos(&mut cursor)?;
        }
        let position = (cursor.x - bounds.left, cursor.y - bounds.top);
        let current = areas
            .iter()
            .position(|r| r.local(bounds).contains(position))
            .unwrap_or(0);
        Ok(Self {
            screens,
            bounds,
            current,
            capture_ms: started.elapsed().as_secs_f64() * 1000.,
        })
    }

    pub fn screen_rect(&self, index: usize) -> Rect {
        let screen = &self.screens[index];
        ScreenRect {
            left: screen.desktop.left,
            top: screen.desktop.top,
            width: screen.width,
            height: screen.height,
        }
        .local(self.bounds)
    }

    pub fn monitor_at(&self, point: (i32, i32)) -> usize {
        (0..self.screens.len())
            .find(|&i| self.screen_rect(i).contains(point))
            .unwrap_or(self.current)
    }

    pub fn full(&self) -> Rect {
        Rect::full(self.bounds.width, self.bounds.height)
    }

    pub fn read_region(&self, selected: Rect) -> Result<Image> {
        if !selected.valid(self.bounds.width, self.bounds.height) {
            return Err("选区超出虚拟桌面".into());
        }
        let started = Instant::now();
        let mut result = black_image(selected.width, selected.height)?;
        for (index, screen) in self.screens.iter().enumerate() {
            let area = self.screen_rect(index);
            if let Some(part) = area.intersection(selected) {
                let source = screen.read_region(Rect {
                    x: part.x - area.x,
                    y: part.y - area.y,
                    width: part.width,
                    height: part.height,
                })?;
                blit(
                    &mut result,
                    &source,
                    part.x - selected.x,
                    part.y - selected.y,
                );
            }
        }
        result.readback_ms = started.elapsed().as_secs_f64() * 1000.;
        Ok(result)
    }
}

fn black_image(width: u32, height: u32) -> Result<Image> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("图像过大")?;
    let mut bgra = vec![0; bytes];
    for pixel in bgra.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

fn blit(target: &mut Image, source: &Image, x: u32, y: u32) {
    for row in 0..source.height {
        let dst = (((y + row) as usize * target.width as usize) + x as usize) * 4;
        let src = row as usize * source.width as usize * 4;
        let count = source.width as usize * 4;
        target.bgra[dst..dst + count].copy_from_slice(&source.bgra[src..src + count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_monitor_origin_cross_screen_crop_and_gaps() {
        let monitors = [
            ScreenRect {
                left: -4,
                top: -2,
                width: 4,
                height: 4,
            },
            ScreenRect {
                left: 0,
                top: 0,
                width: 5,
                height: 4,
            },
        ];
        let desktop = ScreenRect::union(&monitors).unwrap();
        assert_eq!(
            desktop,
            ScreenRect {
                left: -4,
                top: -2,
                width: 9,
                height: 6
            }
        );
        let selected = Rect {
            x: 2,
            y: 1,
            width: 5,
            height: 4,
        };
        let mut output = black_image(selected.width, selected.height).unwrap();
        for (i, monitor) in monitors.iter().enumerate() {
            let area = monitor.local(desktop);
            let part = area.intersection(selected).unwrap();
            let mut source = black_image(part.width, part.height).unwrap();
            for p in source.bgra.chunks_exact_mut(4) {
                p[..3].copy_from_slice(if i == 0 { &[0, 0, 255] } else { &[255, 0, 0] });
            }
            blit(
                &mut output,
                &source,
                part.x - selected.x,
                part.y - selected.y,
            );
        }
        for y in 0..4 {
            for x in 0..5 {
                let expected = if x < 2 && y < 3 {
                    [0, 0, 255, 255]
                } else if x >= 2 && y >= 1 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 0, 255]
                };
                let start = (y * 5 + x) * 4;
                assert_eq!(&output.bgra[start..start + 4], &expected);
            }
        }
    }
}
