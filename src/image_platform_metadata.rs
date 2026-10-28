//! HEIF Exif图项的严格方向读取；只确认方向，不授予元数据保留资格。
use super::PlatformError;
use image::metadata::Orientation;
use std::sync::atomic::AtomicBool;

const MAX_EXIF_ITEM_BYTES: usize = 256 * 1024 + 4;

/// HEIF ExifDataBlock的前4字节是大端offset，相对于这4字节之后的负载。
/// 仅按该offset定位TIFF；不搜索其它TIFF头，也不解释XMP方向。
pub(super) fn exif_orientation(
    payload: &[u8],
    cancel: &AtomicBool,
) -> Result<Option<Orientation>, PlatformError> {
    super::cancelled(cancel)?;
    let result = (|| {
        if payload.len() > MAX_EXIF_ITEM_BYTES {
            return Err(PlatformError::Budget(
                tr!(
                    "HEIF EXIF图项超过256KiB加4字节头的上限",
                    "HEIF EXIF item exceeds 256 KiB plus its 4-byte header"
                )
                .into(),
            ));
        }
        let bad_offset = || {
            super::invalid(
                "HEIF EXIF的TIFF偏移无效或被截断",
                "Invalid or truncated HEIF EXIF TIFF offset",
            )
        };
        let offset =
            u32::from_be_bytes(payload.get(..4).ok_or_else(bad_offset)?.try_into().unwrap());
        let start = usize::try_from(offset)
            .ok()
            .and_then(|offset| offset.checked_add(4))
            .ok_or_else(bad_offset)?;
        let tiff = payload.get(start..).ok_or_else(bad_offset)?;
        crate::image_container_metadata::exif_orientation_cancellable(tiff, cancel).map_err(
            |message| PlatformError::Invalid {
                hresult: None,
                message,
            },
        )
    })();
    // 底层读取取消也可能携带String错误；返回前仍优先报告结构化取消。
    super::cancelled(cancel)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn word(value: u16, little: bool) -> [u8; 2] {
        if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        }
    }

    fn long(value: u32, little: bool) -> [u8; 4] {
        if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        }
    }

    // 只构造TIFF目录，不包含或声称有任何编码像素。
    fn tiff(little: bool, entries: &[(u16, u16, u32, u32)]) -> Vec<u8> {
        let mut bytes = if little { b"II" } else { b"MM" }.to_vec();
        bytes.extend_from_slice(&word(42, little));
        bytes.extend_from_slice(&long(8, little));
        bytes.extend_from_slice(&word(entries.len() as u16, little));
        for &(tag, kind, count, value) in entries {
            bytes.extend_from_slice(&word(tag, little));
            bytes.extend_from_slice(&word(kind, little));
            bytes.extend_from_slice(&long(count, little));
            if kind == 3 && count == 1 {
                bytes.extend_from_slice(&word(value as u16, little));
                bytes.extend_from_slice(&[0, 0]);
            } else {
                bytes.extend_from_slice(&long(value, little));
            }
        }
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    fn item(tiff: &[u8], prefix: &[u8]) -> Vec<u8> {
        let mut bytes = (prefix.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(tiff);
        bytes
    }

    fn read(bytes: &[u8]) -> Result<Option<Orientation>, PlatformError> {
        exif_orientation(bytes, &AtomicBool::new(false))
    }

    #[test]
    fn image_platform_metadata_reads_all_eight_orientations_in_both_tiff_byte_orders() {
        for little in [false, true] {
            for value in 1..=8 {
                let tiff = tiff(little, &[(0x112, 3, 1, value)]);
                for prefix in [&[][..], &b"Exif\0\0"[..]] {
                    let found = read(&item(&tiff, prefix)).unwrap().unwrap();
                    assert_eq!(found.to_exif(), value as u8);
                }
            }
        }
    }

    #[test]
    fn image_platform_metadata_offset_is_relative_to_the_bytes_after_its_big_endian_field() {
        let decoy = tiff(true, &[(0x112, 3, 1, 2)]);
        let actual = tiff(false, &[(0x112, 3, 1, 6)]);
        let payload = item(&actual, &decoy);
        assert_eq!(&payload[..4], &26u32.to_be_bytes());
        assert_eq!(read(&payload), Ok(Some(Orientation::Rotate90)));
        // 不能扫描到更早的合法TIFF头或把offset当成相对item开头的位置。
        let mut bad = payload;
        bad[..4].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(read(&bad), Err(PlatformError::Invalid { .. })));
    }

    #[test]
    fn image_platform_metadata_absent_first_ifd_orientation_stays_unknown() {
        for little in [false, true] {
            assert_eq!(read(&item(&tiff(little, &[]), &[])), Ok(None));
            assert_eq!(
                read(&item(&tiff(little, &[(0x100, 3, 1, 90)]), &[])),
                Ok(None)
            );
            let mut bytes = tiff(little, &[]);
            let second = tiff(little, &[(0x112, 3, 1, 8)]);
            let next = bytes.len() as u32;
            bytes[10..14].copy_from_slice(&long(next, little));
            bytes.extend_from_slice(&second[8..]);
            // 不从缩略图或其它IFD寻找主图方向。
            assert_eq!(read(&item(&bytes, &[])), Ok(None));
        }
    }

    #[test]
    fn image_platform_metadata_rejects_bad_offsets_and_truncated_first_directories() {
        let valid = item(&tiff(true, &[(0x112, 3, 1, 1)]), &[]);
        for length in 0..valid.len() {
            assert!(matches!(
                read(&valid[..length]),
                Err(PlatformError::Invalid { .. })
            ));
        }
        for offset in [u32::MAX, valid.len() as u32, valid.len() as u32 - 4] {
            let mut bytes = valid.clone();
            bytes[..4].copy_from_slice(&offset.to_be_bytes());
            assert!(matches!(read(&bytes), Err(PlatformError::Invalid { .. })));
        }
        assert!(matches!(
            read(&tiff(true, &[(0x112, 3, 1, 1)])),
            Err(PlatformError::Invalid { .. })
        ));
        let mut wrong_magic = tiff(false, &[]);
        wrong_magic[2..4].copy_from_slice(&43u16.to_be_bytes());
        assert!(matches!(
            read(&item(&wrong_magic, &[])),
            Err(PlatformError::Invalid { .. })
        ));
    }

    #[test]
    fn image_platform_metadata_rejects_duplicate_invalid_and_later_malformed_ifd_entries() {
        for little in [false, true] {
            for entries in [
                vec![(0x112, 3, 1, 1), (0x112, 3, 1, 1)],
                vec![(0x112, 3, 1, 1), (0x112, 3, 1, 8)],
                vec![(0x112, 4, 1, 6)],
                vec![(0x112, 3, 2, 6)],
                vec![(0x112, 3, 1, 0)],
                vec![(0x112, 3, 1, 9)],
                vec![(0x112, 3, 1, 256)],
                // 方向本身有效，后一个字段的数据范围越界仍必须报错。
                vec![(0x112, 3, 1, 6), (0x10e, 2, 9, u32::MAX)],
                vec![(0x112, 3, 1, 6), (0x10e, 0, 1, 0)],
            ] {
                assert!(matches!(
                    read(&item(&tiff(little, &entries), &[])),
                    Err(PlatformError::Invalid { .. })
                ));
            }
        }
    }

    #[test]
    fn image_platform_metadata_checks_the_complete_item_budget_and_prioritizes_cancellation() {
        let mut bytes = item(&tiff(true, &[]), &[]);
        bytes.resize(MAX_EXIF_ITEM_BYTES, 0);
        assert_eq!(read(&bytes), Ok(None));
        bytes.push(0);
        assert!(matches!(read(&bytes), Err(PlatformError::Budget(_))));
        let cancel = AtomicBool::new(true);
        for payload in [&[][..], bytes.as_slice()] {
            assert_eq!(
                exif_orientation(payload, &cancel),
                Err(PlatformError::Cancelled)
            );
        }
        let valid = item(&tiff(false, &[(0x112, 3, 1, 8)]), &[]);
        assert_eq!(
            exif_orientation(&valid, &cancel),
            Err(PlatformError::Cancelled)
        );
        cancel.store(false, Ordering::Release);
        assert_eq!(
            exif_orientation(&valid, &cancel),
            Ok(Some(Orientation::Rotate270))
        );
    }
}
