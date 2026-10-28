//! 共用的 GDI 离屏位图：32 位自上而下 DIB 位图头，以及分层小窗的一次性文字位图。
//!
//! 文字位图的字体、位图和 DC 都随 Drop 归还；GDI 文字画在不透明底上，`finish` 再统一补圆角、描边和预乘 alpha（GDI 会把文字像素的 alpha 清零）。
use crate::Result;
use windows::{
    core::PCWSTR,
    Win32::{Foundation::*, Graphics::Gdi::*},
};

pub(crate) struct Surface {
    pub(crate) dc: HDC,
    font: HFONT,
    previous_font: HGDIOBJ,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    bits: *mut u8,
    pub(crate) size: (u32, u32),
}
impl Surface {
    /// 建 DC 并选入字体；位图在量好版面后由 `allocate` 建立。
    pub(crate) fn new(font_px: i32, face: PCWSTR) -> Result<Self> {
        let mut surface = Self {
            dc: HDC::default(),
            font: HFONT::default(),
            previous_font: HGDIOBJ::default(),
            bitmap: HBITMAP::default(),
            previous_bitmap: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            size: (0, 0),
        };
        unsafe {
            surface.dc = CreateCompatibleDC(None);
            if surface.dc.is_invalid() {
                return Err("分层浮层 DC 创建失败".into());
            }
            surface.font = CreateFontW(
                -font_px.max(1),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                face,
            );
            if surface.font.is_invalid() {
                return Err("分层浮层字体创建失败".into());
            }
            surface.previous_font = SelectObject(surface.dc, surface.font.into());
        }
        Ok(surface)
    }
    /// 建 32 位自上而下的 DIB 并用不透明底色（0xRRGGBB）铺满。
    pub(crate) fn allocate(&mut self, width: u32, height: u32, background: u32) -> Result<()> {
        unsafe {
            let info = bitmap_info(width, height);
            let mut bits = std::ptr::null_mut();
            self.bitmap =
                CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            self.bits = bits.cast();
            if self.bits.is_null() {
                return Err("分层浮层位图像素不可用".into());
            }
            self.previous_bitmap = SelectObject(self.dc, self.bitmap.into());
            SetBkMode(self.dc, TRANSPARENT);
        }
        self.size = (width, height);
        let fill = [
            (background & 0xff) as u8,
            ((background >> 8) & 0xff) as u8,
            ((background >> 16) & 0xff) as u8,
            255,
        ];
        for pixel in self.pixels().chunks_exact_mut(4) {
            pixel.copy_from_slice(&fill);
        }
        Ok(())
    }
    /// 按当前字体量文字（`limit` 为换行宽度，单行时可为 0）。
    pub(crate) fn measure(&self, text: &str, flags: DRAW_TEXT_FORMAT, limit: i32) -> (i32, i32) {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            right: limit,
            ..Default::default()
        };
        unsafe {
            DrawTextW(
                self.dc,
                &mut wide,
                &mut rect,
                flags | DT_CALCRECT | DT_NOPREFIX,
            )
        };
        (rect.right - rect.left, rect.bottom - rect.top)
    }
    pub(crate) fn draw(&self, text: &str, color: u32, rect: RECT, flags: DRAW_TEXT_FORMAT) {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = rect;
        unsafe {
            SetTextColor(self.dc, colorref(color));
            DrawTextW(self.dc, &mut wide, &mut rect, flags | DT_NOPREFIX);
        }
    }
    pub(crate) fn pixels(&mut self) -> &mut [u8] {
        let bytes = self.size.0 as usize * self.size.1 as usize * 4;
        unsafe { std::slice::from_raw_parts_mut(self.bits, bytes) }
    }
    /// 等 GDI 画完，再补圆角（`radius_dip`）、描边（0xRRGGBB）和预乘 alpha。
    pub(crate) fn finish(&mut self, scale: f32, radius_dip: f64, border: u32) {
        unsafe {
            let _ = GdiFlush();
        }
        let (width, height) = self.size;
        finish_pixels(self.pixels(), width, height, scale, radius_dip, border);
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if !self.previous_bitmap.is_invalid() {
                SelectObject(self.dc, self.previous_bitmap);
            }
            if !self.previous_font.is_invalid() {
                SelectObject(self.dc, self.previous_font);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
            if !self.font.is_invalid() {
                let _ = DeleteObject(self.font.into());
            }
            if !self.dc.is_invalid() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

/// 32 位 BI_RGB、自上而下（负高度）的 DIB 位图头，像素按 BGRA 行优先排列。
pub(crate) fn bitmap_info(width: u32, height: u32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    }
}
pub(crate) fn colorref(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff))
}
fn rounded_distance(x: f64, y: f64, width: f64, height: f64, radius: f64) -> f64 {
    let (half_w, half_h) = (width * 0.5, height * 0.5);
    let px = (x - half_w).abs() - half_w + radius;
    let py = (y - half_h).abs() - half_h + radius;
    px.max(0.).hypot(py.max(0.)) + px.max(py).min(0.) - radius
}
fn finish_pixels(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    scale: f32,
    radius_dip: f64,
    border: u32,
) {
    let radius = (radius_dip * f64::from(scale))
        .min(f64::from(width) * 0.5)
        .min(f64::from(height) * 0.5);
    let stroke = f64::from(scale).round().max(1.);
    let border = [border & 0xff, (border >> 8) & 0xff, (border >> 16) & 0xff].map(|c| c as f64);
    for y in 0..height {
        for x in 0..width {
            let distance = rounded_distance(
                f64::from(x) + 0.5,
                f64::from(y) + 0.5,
                f64::from(width),
                f64::from(height),
                radius,
            );
            let outer = (0.5 - distance).clamp(0., 1.);
            let inner = (0.5 - distance - stroke).clamp(0., 1.);
            let i = ((y * width + x) * 4) as usize;
            for channel in 0..3 {
                let color = f64::from(pixels[i + channel]) * inner + border[channel] * (1. - inner);
                pixels[i + channel] = (color * outer).round() as u8;
            }
            pixels[i + 3] = (255. * outer).round() as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layered_surface_pixels_are_premultiplied_with_transparent_corners() {
        let (width, height) = (40, 20);
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0x40, 0x80, 0xc0, 0]);
        }
        finish_pixels(&mut pixels, width, height, 1., 6., 0x112233);
        let at = |x: u32, y: u32| {
            let i = ((y * width + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        assert_eq!(at(0, 0)[3], 0, "圆角外透明");
        assert_eq!(at(20, 10), [0x40, 0x80, 0xc0, 255], "内部保留文字底色");
        let edge = at(20, 0);
        assert_eq!(edge[3], 255);
        assert_eq!([edge[0], edge[1], edge[2]], [0x33, 0x22, 0x11], "描边色");
        for pixel in pixels.chunks_exact(4) {
            assert!(
                pixel[..3].iter().all(|&c| c <= pixel[3]),
                "预乘颜色不超过 alpha"
            );
        }
    }
}
