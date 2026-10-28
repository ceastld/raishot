//! 截图历史的容量上限、缩略图尺寸与全进程解码预算。
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const MAX_ENTRIES: usize = 50;
pub(crate) const THUMB_WIDTH: u32 = crate::library::THUMB_WIDTH;
pub(crate) const THUMB_HEIGHT: u32 = crate::library::THUMB_HEIGHT;
pub(crate) const DECODER_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PIXELS: u64 = 32_000_000;

/// 全进程共用一份 gate；解码前获得，消费或丢弃结果后释放。
/// 只允许一份逐行 decoder + 一份输出；全图像素最多 128 MB，缩略图最多 614400 bytes。
/// png::Limits 约束元数据/预留预算；PNG 内部行缓冲另由先验边长、RGBA8 和逐行读取约束。
#[derive(Default)]
pub(crate) struct DecodeGate(AtomicBool);
pub(crate) struct DecodePermit<'a>(&'a DecodeGate);
impl DecodeGate {
    pub(crate) fn try_enter(&self) -> Option<DecodePermit<'_>> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| DecodePermit(self))
    }
}
impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        self.0 .0.store(false, Ordering::Release);
    }
}

pub(crate) fn thumbnail_size(width: u32, height: u32) -> Option<(u32, u32)> {
    if width == 0
        || height == 0
        || width > 65535
        || height > 65535
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return None;
    }
    let scale = (f64::from(THUMB_WIDTH) / f64::from(width))
        .min(f64::from(THUMB_HEIGHT) / f64::from(height))
        .min(1.);
    Some((
        (f64::from(width) * scale).round().max(1.) as u32,
        (f64::from(height) * scale).round().max(1.) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_gate_is_shared_exclusive_and_released_on_drop() {
        let gate = DecodeGate::default();
        let permit = gate.try_enter().unwrap();
        assert!(gate.try_enter().is_none());
        drop(permit);
        assert!(gate.try_enter().is_some());
    }
    #[test]
    fn thumbnail_and_full_pixel_bounds_precede_allocation() {
        for (w, h) in [(1, 1), (65535, 1), (1, 65535), (8000, 4000), (1920, 1080)] {
            let (x, y) = thumbnail_size(w, h).unwrap();
            assert!(x <= 480 && y <= 320 && u64::from(x) * u64::from(y) * 4 <= 614_400);
        }
        for (w, h) in [(0, 1), (1, 0), (65536, 1), (8001, 4000)] {
            assert_eq!(thumbnail_size(w, h), None);
        }
    }
}
