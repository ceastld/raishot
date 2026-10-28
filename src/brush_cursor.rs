//! 笔刷尺寸系统光标：用 CreateIconIndirect 生成圆环光标，由 Windows 硬件光标绘制，
//! 不在画布上跟随指针自绘，也不会与系统十字光标重叠或滞后。
use gpui::CursorStyle;
use std::sync::Mutex;
use windows::Win32::{
    Foundation::FALSE,
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO},
};

/// 小于此物理直径时圆环不可辨认，沿用系统十字光标。
const MIN_DIAMETER: f32 = 6.;
/// 超大笔刷改用十字光标，避免生成过大的光标位图。
const MAX_DIAMETER: f32 = 512.;
/// GPUI 帧、未处理消息和其它窗口可长期留存裸句柄，已暴露的句柄只在进程退出时回收。
const CACHE_HANDLES: usize = 64;
/// 32 位色图加 WORD 对齐单色掩码的总预算；不代表 Windows 内部的全部资源占用。
const CACHE_BITMAP_BYTES: u64 = 4 * 1024 * 1024;
const RING: (u8, u8, u8) = (0xff, 0xff, 0xff);
const HALO: (u8, u8, u8) = (0x10, 0x18, 0x20);

// 全进程共用，线程退出也不释放已交给 GPUI 的句柄。静态缓存无 Drop；Windows 在进程退出时回收。
static CURSORS: Mutex<CursorCache> =
    Mutex::new(CursorCache::new(CACHE_HANDLES, CACHE_BITMAP_BYTES));

struct CursorCache {
    entries: Vec<(u32, isize)>,
    bitmap_bytes: u64,
    handle_limit: usize,
    bitmap_limit: u64,
}

impl CursorCache {
    const fn new(handle_limit: usize, bitmap_limit: u64) -> Self {
        Self {
            entries: Vec::new(),
            bitmap_bytes: 0,
            handle_limit,
            bitmap_limit,
        }
    }

    fn cursor(&mut self, diameter: f32, create: impl FnOnce(f32) -> Option<isize>) -> CursorStyle {
        if !diameter.is_finite() || !(MIN_DIAMETER..=MAX_DIAMETER).contains(&diameter) {
            return CursorStyle::Crosshair;
        }
        // 以 1/4 像素量化，滚轮调整宽度时不会为浮点抖动反复建句柄。
        let key = (diameter * 4.).round() as u32;
        if let Some((_, handle)) = self.entries.iter().find(|(k, _)| *k == key) {
            return CursorStyle::Native(*handle);
        }
        let diameter = key as f32 / 4.;
        let bytes = bitmap_bytes(ring_side(diameter));
        if self.entries.len() >= self.handle_limit
            || bytes > self.bitmap_limit.saturating_sub(self.bitmap_bytes)
        {
            // 不创建、更不淘汰已有句柄；也不用邻近尺寸冒充当前笔刷尺寸。
            return CursorStyle::Crosshair;
        }
        let Some(handle) = create(diameter).filter(|handle| *handle != 0) else {
            return CursorStyle::Crosshair;
        };
        self.entries.push((key, handle));
        self.bitmap_bytes += bytes;
        CursorStyle::Native(handle)
    }
}

/// 按物理像素直径返回笔刷光标；无法创建时退回系统十字光标。
pub(crate) fn brush_cursor(diameter: f32) -> CursorStyle {
    CURSORS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .cursor(diameter, create)
}

fn ring_side(diameter: f32) -> u32 {
    ((diameter / 2.).ceil() as u32 + 2) * 2 + 1
}

fn bitmap_bytes(side: u32) -> u64 {
    u64::from(side) * (u64::from(side) * 4 + u64::from(side.div_ceil(16)) * 2)
}

/// 边长为奇数的直通 alpha BGRA 像素（自上而下），热点位于中心像素。
fn ring_pixels(diameter: f32) -> (u32, Vec<[u8; 4]>) {
    let radius = diameter / 2.;
    let side = ring_side(diameter);
    let half = side / 2;
    let center = half as f32 + 0.5;
    let mut pixels = vec![[0u8; 4]; (side * side) as usize];
    for y in 0..side {
        for x in 0..side {
            let distance = (x as f32 + 0.5 - center).hypot(y as f32 + 0.5 - center);
            let offset = (distance - radius).abs();
            let ring = (1.25 - offset).clamp(0., 1.);
            let halo = (2.25 - offset).clamp(0., 1.) * 0.55;
            pixels[(y * side + x) as usize] = blend(ring, halo);
        }
    }
    if diameter >= 10. {
        // 中心点：白点加深色四邻，大笔刷时仍能精确对位。
        let at = |x: u32, y: u32| (y * side + x) as usize;
        for (x, y) in [
            (half - 1, half),
            (half + 1, half),
            (half, half - 1),
            (half, half + 1),
        ] {
            pixels[at(x, y)] = blend(0., 0.8);
        }
        pixels[at(half, half)] = blend(1., 0.);
    }
    (side, pixels)
}

fn blend(ring: f32, halo: f32) -> [u8; 4] {
    let alpha = ring + halo * (1. - ring);
    if alpha <= 0. {
        return [0; 4];
    }
    let mix =
        |a: u8, b: u8| ((a as f32 * ring + b as f32 * halo * (1. - ring)) / alpha).round() as u8;
    [
        mix(RING.2, HALO.2),
        mix(RING.1, HALO.1),
        mix(RING.0, HALO.0),
        (alpha * 255.).round() as u8,
    ]
}

fn create(diameter: f32) -> Option<isize> {
    let (side, pixels) = ring_pixels(diameter);
    unsafe {
        let info = crate::gdi_surface::bitmap_info(side, side);
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(
            pixels.as_ptr().cast::<u8>(),
            bits.cast::<u8>(),
            pixels.len() * 4,
        );
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
            xHotspot: side / 2,
            yHotspot: side / 2,
            hbmMask: mask,
            hbmColor: color,
        };
        let cursor = CreateIconIndirect(&icon);
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        cursor.ok().map(|cursor| cursor.0 as isize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_centered_on_odd_hotspot_with_transparent_corners_and_center_dot() {
        let (side, pixels) = ring_pixels(20.);
        assert_eq!(side % 2, 1);
        let half = side / 2;
        let at = |x: u32, y: u32| pixels[(y * side + x) as usize];
        assert_eq!(at(0, 0)[3], 0);
        assert_eq!(at(half, half), [0xff, 0xff, 0xff, 0xff]);
        // 半径 10 处为不透明白环，向外过渡到深色描边。
        assert_eq!(at(half + 10, half), [0xff, 0xff, 0xff, 0xff]);
        let outer = at(half + 12, half);
        assert!(outer[3] > 0 && outer[0] < 0x80);
    }

    #[test]
    fn tiny_huge_and_invalid_diameters_keep_system_crosshair() {
        let mut cache = CursorCache::new(CACHE_HANDLES, CACHE_BITMAP_BYTES);
        for diameter in [0., 5.9, 513., f32::NAN, f32::INFINITY] {
            assert_eq!(
                cache.cursor(diameter, |_| panic!("无效尺寸不能创建原生句柄")),
                CursorStyle::Crosshair
            );
        }
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bitmap_bytes, 0);
    }

    #[test]
    fn same_quantized_diameter_reuses_native_handle() {
        let mut cache = CursorCache::new(CACHE_HANDLES, CACHE_BITMAP_BYTES);
        let a = cache.cursor(24., |_| Some(17));
        let bytes = cache.bitmap_bytes;
        let b = cache.cursor(24.05, |_| panic!("同量化尺寸必须复用已有句柄"));
        assert_eq!(a, CursorStyle::Native(17));
        assert_eq!(a, b);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bitmap_bytes, bytes);
    }

    #[test]
    fn inactive_window_handle_survives_other_window_cache_pressure() {
        let mut cache = CursorCache::new(CACHE_HANDLES, CACHE_BITMAP_BYTES);
        let window_a = cache.cursor(6., |_| Some(1));
        let mut creations = 1;
        let mut window_b_frames = Vec::new();
        for index in 0..CACHE_HANDLES + 8 {
            window_b_frames.push(cache.cursor(7. + index as f32, |_| {
                creations += 1;
                Some(creations as isize)
            }));
        }
        assert_eq!(creations, CACHE_HANDLES);
        assert_eq!(cache.entries.len(), CACHE_HANDLES);
        assert!(window_b_frames[..CACHE_HANDLES - 1]
            .iter()
            .all(|style| matches!(style, CursorStyle::Native(_))));
        assert!(window_b_frames[CACHE_HANDLES - 1..]
            .iter()
            .all(|style| *style == CursorStyle::Crosshair));
        assert_eq!(
            cache.cursor(6., |_| panic!("旧窗口留存的句柄不能销毁或重建")),
            window_a
        );
        for (index, style) in window_b_frames[..CACHE_HANDLES - 1].iter().enumerate() {
            assert_eq!(
                cache.cursor(7. + index as f32, |_| panic!(
                    "已暴露的其它窗口句柄必须存活"
                )),
                *style
            );
        }
    }

    #[test]
    fn bitmap_budget_exhaustion_returns_crosshair_without_creating_or_evicting() {
        let bytes = bitmap_bytes(ring_side(MAX_DIAMETER));
        let mut cache = CursorCache::new(CACHE_HANDLES, bytes * 2);
        let a = cache.cursor(512., |_| Some(11));
        let b = cache.cursor(511.75, |_| Some(12));
        assert_eq!(cache.bitmap_bytes, bytes * 2);
        assert_eq!(
            cache.cursor(511.5, |_| panic!("超出位图预算时不能创建句柄或临时位图")),
            CursorStyle::Crosshair
        );
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.bitmap_bytes, bytes * 2);
        assert_eq!(cache.cursor(512., |_| None), a);
        assert_eq!(cache.cursor(511.75, |_| None), b);
    }

    #[test]
    fn failed_and_null_creation_leave_budget_available() {
        let bytes = bitmap_bytes(ring_side(24.));
        let mut cache = CursorCache::new(1, bytes);
        for failed in [None, Some(0)] {
            assert_eq!(cache.cursor(24., |_| failed), CursorStyle::Crosshair);
            assert!(cache.entries.is_empty());
            assert_eq!(cache.bitmap_bytes, 0);
        }
        assert_eq!(cache.cursor(24., |_| Some(31)), CursorStyle::Native(31));
        assert_eq!(cache.bitmap_bytes, bytes);
        assert_eq!(
            cache.cursor(25., |_| panic!("句柄预算已满时不能继续增配")),
            CursorStyle::Crosshair
        );
        assert_eq!(cache.cursor(24., |_| None), CursorStyle::Native(31));
    }

    #[test]
    fn shared_cache_keeps_handles_after_requesting_thread_exits() {
        use std::sync::Arc;

        let cache = Arc::new(Mutex::new(CursorCache::new(2, CACHE_BITMAP_BYTES)));
        let from_thread = |diameter, handle| {
            let cache = cache.clone();
            std::thread::spawn(move || cache.lock().unwrap().cursor(diameter, |_| Some(handle)))
                .join()
                .unwrap()
        };
        let a = from_thread(20., 41);
        let b = from_thread(21., 42);
        assert_eq!(from_thread(22., 43), CursorStyle::Crosshair);
        assert_eq!(from_thread(20., 44), a);
        assert_eq!(from_thread(21., 45), b);
        assert_eq!(cache.lock().unwrap().entries.len(), 2);
    }
}
