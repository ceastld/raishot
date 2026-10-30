//! 拖动排序用的张开手与握拳光标。Windows 没有系统抓手光标，GPUI 的 OpenHand／ClosedHand
//! 在 Windows 上退化为箭头；这里用 resvg 绘制白底黑线的手形，CreateIconIndirect 生成硬件光标。
use gpui::CursorStyle;
use std::sync::OnceLock;
use windows::Win32::{
    Foundation::FALSE,
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{CreateIconIndirect, GetSystemMetrics, ICONINFO, SM_CXCURSOR},
};

/// 24 单位画布，手指与手掌用实心圆角形状拼成：先画加粗的黑色外轮廓，再盖白色实心，
/// 最后画指缝短线；单层细描边在深浅背景上都清楚。
const OPEN: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" stroke-linecap="round" stroke-linejoin="round">
<g id="body">
<rect x="5.4" y="5.2" width="3.8" height="10" rx="1.9"/>
<rect x="9.2" y="3" width="3.8" height="10" rx="1.9"/>
<rect x="13" y="3.8" width="3.8" height="10" rx="1.9"/>
<rect x="16.8" y="6.4" width="3.6" height="9" rx="1.8"/>
<path d="M5.4 11H20.4V15.6C20.4 19 17.8 21.6 14.4 21.6H11.4C9.4 21.6 8.1 20.9 6.9 19.7L5.4 18.2Z"/>
</g>
<path id="thumb" d="M7.4 17.2L3.8 13.4" fill="none"/>
<use href="#body" fill="black" stroke="black" stroke-width="2.4"/>
<use href="#thumb" stroke="black" stroke-width="6"/>
<use href="#thumb" stroke="white" stroke-width="3.6"/>
<use href="#body" fill="white"/>
<path d="M9.2 5.6V11.4M13 4.6V11.4M16.8 6.6V11.4" stroke="black" stroke-width="1.1" fill="none"/>
</svg>"##;

const CLOSED: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" stroke-linecap="round" stroke-linejoin="round">
<g id="body">
<rect x="5.4" y="8.6" width="3.8" height="7" rx="1.9"/>
<rect x="9.2" y="7.6" width="3.8" height="7" rx="1.9"/>
<rect x="13" y="7.8" width="3.8" height="7" rx="1.9"/>
<rect x="16.8" y="9" width="3.6" height="6.4" rx="1.8"/>
<path d="M5.4 12H20.4V15.8C20.4 19.1 17.8 21.4 14.4 21.4H11.4C9.4 21.4 8.1 20.8 6.9 19.6L5.4 18.1Z"/>
</g>
<path id="thumb" d="M7.6 16.6L4.6 15" fill="none"/>
<use href="#body" fill="black" stroke="black" stroke-width="2.4"/>
<use href="#thumb" stroke="black" stroke-width="6"/>
<use href="#thumb" stroke="white" stroke-width="3.6"/>
<use href="#body" fill="white"/>
<path d="M9.2 9V12.6M13 8.2V12.6M16.8 9.4V12.6" stroke="black" stroke-width="1.1" fill="none"/>
</svg>"##;

/// 热点取掌心（24 单位坐标）。
const HOTSPOT: (f32, f32) = (12.6, 14.);

static OPEN_CURSOR: OnceLock<CursorStyle> = OnceLock::new();
static CLOSED_CURSOR: OnceLock<CursorStyle> = OnceLock::new();

/// 可拖动处的悬停光标；生成失败时用手形。句柄全进程共用，进程退出时由系统回收。
pub(crate) fn open() -> CursorStyle {
    *OPEN_CURSOR.get_or_init(|| create(OPEN).unwrap_or(CursorStyle::PointingHand))
}

/// 拖动中的握拳光标；生成失败时用移动光标。
pub(crate) fn closed() -> CursorStyle {
    *CLOSED_CURSOR.get_or_init(|| create(CLOSED).unwrap_or(CursorStyle::ResizeAll))
}

/// 按系统光标尺寸取边长；手形约占四分之三，与系统箭头的视觉大小相近。
fn side() -> u32 {
    let system = unsafe { GetSystemMetrics(SM_CXCURSOR) };
    u32::try_from(system).unwrap_or(32).clamp(32, 128)
}

/// 直通 alpha 的 BGRA 像素（自上而下）与热点。
fn pixels(svg: &str, side: u32) -> Option<(Vec<u8>, u32, u32)> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(side, side)?;
    let scale = side as f32 * 0.8 / 24.;
    let offset = side as f32 * 0.1;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_row(scale, 0., 0., scale, offset, offset),
        &mut pixmap.as_mut(),
    );
    let bgra = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.blue(), color.green(), color.red(), color.alpha()]
        })
        .collect();
    let hotspot = |unit: f32| (offset + unit * scale).round() as u32;
    Some((bgra, hotspot(HOTSPOT.0), hotspot(HOTSPOT.1)))
}

fn create(svg: &str) -> Option<CursorStyle> {
    let side = side();
    let (bgra, x, y) = pixels(svg, side)?;
    unsafe {
        let info = crate::gdi_surface::bitmap_info(side, side);
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bgra.len());
        // 32 位 alpha 位图决定透明度；单色掩码仍须提供，按 WORD 对齐清零。
        let mask_bits = vec![0u8; side.div_ceil(16) as usize * 2 * side as usize];
        let mask = CreateBitmap(
            side as i32,
            side as i32,
            1,
            1,
            Some(mask_bits.as_ptr().cast()),
        );
        let icon = ICONINFO {
            fIcon: FALSE,
            xHotspot: x,
            yHotspot: y,
            hbmMask: mask,
            hbmColor: color,
        };
        let cursor = CreateIconIndirect(&icon);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        cursor
            .ok()
            .map(|cursor| cursor.0 as isize)
            .filter(|handle| *handle != 0)
            .map(CursorStyle::Native)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_cursors_render_opaque_outline_and_transparent_corners() {
        for svg in [OPEN, CLOSED] {
            let (bgra, x, y) = pixels(svg, 32).expect("手形光标 SVG 可解析");
            let alpha = |px: u32, py: u32| bgra[((py * 32 + px) * 4 + 3) as usize];
            assert_eq!(alpha(0, 0), 0);
            assert!(alpha(x, y) > 200, "热点应落在不透明的掌心");
            let opaque = bgra.chunks_exact(4).filter(|p| p[3] > 200).count();
            assert!(opaque > 150, "手形应有足够的实心像素：{opaque}");
        }
    }
}
