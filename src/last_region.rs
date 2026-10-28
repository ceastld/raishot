//! 上次成功截图的全局物理区域；只在冻结后读，输出成功后写。
use crate::{
    geometry::{Rect, ScreenRect},
    Result,
};
use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

// 单个 u32 同时锁定格式和版本；固定32字节，不存本地化文本。
const HEADER: u32 = 0x5253_4c01;
const BYTES: usize = 32;

pub(crate) fn load(canvas: ScreenRect) -> Option<Rect> {
    load_at(
        &crate::preferences::data_dir().join("last_region.bin"),
        canvas,
    )
}
pub(crate) fn save(canvas: ScreenRect, local: Rect) -> Result<()> {
    save_at(
        &crate::preferences::data_dir().join("last_region.bin"),
        canvas,
        local,
    )
}
fn canvas_key(canvas: ScreenRect) -> u64 {
    [
        canvas.left as u32,
        canvas.top as u32,
        canvas.width,
        canvas.height,
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}
fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}
fn encode(canvas: ScreenRect, local: Rect) -> Option<[u8; BYTES]> {
    if !local.valid(canvas.width, canvas.height) {
        return None;
    }
    let left = i32::try_from(i64::from(canvas.left) + i64::from(local.x)).ok()?;
    let top = i32::try_from(i64::from(canvas.top) + i64::from(local.y)).ok()?;
    let key = canvas_key(canvas);
    let words = [
        HEADER,
        key as u32,
        (key >> 32) as u32,
        left as u32,
        top as u32,
        local.width,
        local.height,
    ];
    let mut bytes = [0; BYTES];
    for (index, word) in words.into_iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    let check = checksum(&bytes[..28]);
    bytes[28..].copy_from_slice(&check.to_le_bytes());
    Some(bytes)
}
fn decode(bytes: &[u8], canvas: ScreenRect) -> Option<Rect> {
    let bytes: &[u8; BYTES] = bytes.try_into().ok()?;
    let word =
        |index: usize| u32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap());
    if word(0) != HEADER || word(7) != checksum(&bytes[..28]) {
        return None;
    }
    if (u64::from(word(2)) << 32 | u64::from(word(1))) != canvas_key(canvas) {
        return None;
    }
    let rect = Rect {
        x: u32::try_from(i64::from(word(3) as i32) - i64::from(canvas.left)).ok()?,
        y: u32::try_from(i64::from(word(4) as i32) - i64::from(canvas.top)).ok()?,
        width: word(5),
        height: word(6),
    };
    rect.valid(canvas.width, canvas.height).then_some(rect)
}
fn load_at(path: &Path, canvas: ScreenRect) -> Option<Rect> {
    let mut bytes = Vec::with_capacity(BYTES + 1);
    File::open(path)
        .ok()?
        .take(BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    decode(&bytes, canvas)
}
fn save_at(path: &Path, canvas: ScreenRect, local: Rect) -> Result<()> {
    let bytes = encode(canvas, local).ok_or("上次截图区域超出桌面或坐标溢出")?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    static TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let temp = path.with_extension(format!(
        "bin-{}-{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let write = std::fs::write(&temp, bytes).and_then(|_| std::fs::rename(&temp, path));
    if let Err(error) = write {
        let _ = std::fs::remove_file(temp);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn canvas() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: -120,
            width: 3840,
            height: 1200,
        }
    }
    #[test]
    fn last_region_round_trip_negative_desktop_and_fixed_size() {
        let rect = Rect {
            x: 80,
            y: 30,
            width: 300,
            height: 200,
        };
        let bytes = encode(canvas(), rect).unwrap();
        assert_eq!(bytes.len(), 32);
        assert_eq!(decode(&bytes, canvas()), Some(rect));
        assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), -1840);
    }
    #[test]
    fn last_region_rejects_changed_desktop_corruption_and_invalid_bounds() {
        let rect = Rect {
            x: 80,
            y: 30,
            width: 300,
            height: 200,
        };
        let mut bytes = encode(canvas(), rect).unwrap();
        assert_eq!(
            decode(
                &bytes,
                ScreenRect {
                    left: 0,
                    ..canvas()
                }
            ),
            None
        );
        assert_eq!(decode(&bytes[..31], canvas()), None);
        bytes[24] ^= 1;
        assert_eq!(decode(&bytes, canvas()), None);
        assert!(encode(
            canvas(),
            Rect {
                x: u32::MAX,
                ..rect
            }
        )
        .is_none());
        assert!(encode(canvas(), Rect { width: 0, ..rect }).is_none());
        assert!(encode(
            ScreenRect {
                left: i32::MAX,
                ..canvas()
            },
            rect
        )
        .is_none());
    }
}
