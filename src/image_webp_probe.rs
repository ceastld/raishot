//! 只识别WebP头部的动画声明；后续块顺序、动画帧和像素由来源校验。
use std::{
    io::{self, Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

fn invalid(zh: &'static str, en: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, crate::i18n::tr(zh, en))
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::other(tr!(
            "WebP格式探测已取消",
            "WebP format detection canceled"
        )))
    } else {
        Ok(())
    }
}

fn read_checked(
    reader: &mut impl Read,
    mut output: &mut [u8],
    cancel: &AtomicBool,
) -> io::Result<()> {
    while !output.is_empty() {
        check_cancel(cancel)?;
        let result = reader.read(output);
        check_cancel(cancel)?;
        match result {
            Ok(0) => {
                return Err(invalid("WebP文件头被截断", "Truncated WebP header"));
            }
            Ok(count) => output = &mut output[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(invalid("WebP文件头被截断", "Truncated WebP header"));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

/// 从来源起点最多读30字节；不读取后续ICC/ANIM/ANMF，也不接受路径或打开文件。
pub(super) fn detect_reader(
    reader: &mut (impl Read + Seek),
    length: u64,
    cancel: &AtomicBool,
) -> io::Result<bool> {
    check_cancel(cancel)?;
    if length < 12 {
        return Ok(false);
    }
    let seek = reader.seek(SeekFrom::Start(0));
    check_cancel(cancel)?;
    seek?;
    let mut riff = [0; 12];
    read_checked(reader, &mut riff, cancel)?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WEBP" {
        return Ok(false);
    }
    let declared = u64::from(le32(&riff[4..]));
    if declared + 8 != length || length & 1 != 0 || length > u64::from(u32::MAX) - 1 {
        return Err(invalid(
            "WebP RIFF长度与来源不符或超限",
            "WebP RIFF length does not match the source or exceeds limits",
        ));
    }
    if length < 20 {
        return Err(invalid(
            "WebP首块头被截断",
            "Truncated first WebP chunk header",
        ));
    }
    let mut chunk = [0; 8];
    read_checked(reader, &mut chunk, cancel)?;
    let size = u64::from(le32(&chunk[4..]));
    if 20 + size + (size & 1) > length {
        return Err(invalid(
            "WebP首块超出来源范围",
            "First WebP chunk exceeds the source range",
        ));
    }
    match &chunk[..4] {
        b"VP8 " | b"VP8L" => return Ok(false),
        b"VP8X" => {}
        _ => {
            return Err(invalid("WebP首块类型无效", "Invalid first WebP chunk type"));
        }
    }
    if size != 10 {
        return Err(invalid(
            "WebP VP8X长度必须为10字节",
            "WebP VP8X must contain 10 bytes",
        ));
    }
    let mut extended = [0; 10];
    read_checked(reader, &mut extended, cancel)?;
    // VP8X只允许ICC、alpha、EXIF、XMP和动画位，其余位与保留字节均须为零。
    if extended[0] & !0x3e != 0 || extended[1..4] != [0, 0, 0] {
        return Err(invalid(
            "WebP VP8X保留位或保留字节无效",
            "Invalid reserved WebP VP8X bits or bytes",
        ));
    }
    let width = u64::from(u24(&extended[4..7])) + 1;
    let height = u64::from(u24(&extended[7..10])) + 1;
    if width * height > u64::from(u32::MAX) {
        return Err(invalid(
            "WebP VP8X画布面积超出格式范围",
            "WebP VP8X canvas area exceeds format limits",
        ));
    }
    check_cancel(cancel)?;
    Ok(extended[0] & 0x02 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn riff(chunk: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(4 + chunk.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(chunk);
        bytes
    }

    fn extended(flags: u8, width: u32, height: u32, tail: &[u8]) -> Vec<u8> {
        let mut chunk = b"VP8X\x0a\0\0\0".to_vec();
        chunk.extend_from_slice(&[flags, 0, 0, 0]);
        chunk.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        chunk.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        chunk.extend_from_slice(tail);
        riff(&chunk)
    }

    fn detect(bytes: Vec<u8>) -> io::Result<bool> {
        let length = bytes.len() as u64;
        detect_reader(&mut Cursor::new(bytes), length, &AtomicBool::new(false))
    }

    #[test]
    fn image_webp_probe_only_declares_animation_and_never_scans_following_icc() {
        struct HeadOnly {
            bytes: Cursor<Vec<u8>>,
            read: usize,
        }
        impl Read for HeadOnly {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                assert!(self.read + output.len() <= 30);
                let count = self.bytes.read(output)?;
                self.read += count;
                Ok(count)
            }
        }
        impl Seek for HeadOnly {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                assert!(matches!(position, SeekFrom::Start(0)));
                self.bytes.seek(position)
            }
        }
        let mut tail = b"ICCP\x00\x20\0\0".to_vec();
        tail.extend_from_slice(&vec![0; 8192]);
        let bytes = extended(0x22, 3, 2, &tail);
        let length = bytes.len() as u64;
        let mut reader = HeadOnly {
            bytes: Cursor::new(bytes),
            read: 0,
        };
        assert!(detect_reader(&mut reader, length, &AtomicBool::new(false)).unwrap());
        assert_eq!(reader.read, 30);
        assert!(!detect(extended(0x3c, 3, 2, &[])).unwrap());
        assert!(!detect(riff(b"VP8 \0\0\0\0")).unwrap());
        assert!(!detect(riff(b"VP8L\0\0\0\0")).unwrap());
        assert!(!detect(b"not a WebP image".to_vec()).unwrap());
        assert!(!detect(b"RIFF\0\0\0\0WAVE".to_vec()).unwrap());
        assert!(!detect(b"RIFF".to_vec()).unwrap());
        // 格式边界由24位minus-one字段表达，不套用像素来源的较小解码预算。
        assert!(detect(extended(2, 1 << 24, 1, &[])).unwrap());
    }

    #[test]
    fn image_webp_probe_rejects_confirmed_webp_header_damage() {
        let valid = extended(2, 3, 2, &[]);
        let mut wrong_riff = valid.clone();
        wrong_riff[4] ^= 2;
        let mut wrong_chunk = valid.clone();
        wrong_chunk[16..20].copy_from_slice(&8u32.to_le_bytes());
        let mut bad_bit = valid.clone();
        bad_bit[20] |= 0x80;
        let mut bad_reserved = valid.clone();
        bad_reserved[22] = 1;
        let mut truncated = valid[..24].to_vec();
        truncated[4..8].copy_from_slice(&16u32.to_le_bytes());
        for bytes in [
            wrong_riff,
            wrong_chunk,
            bad_bit,
            bad_reserved,
            truncated,
            riff(b"VP8X\0\0"),
            extended(2, 1 << 24, 256, &[]),
        ] {
            assert_eq!(
                detect(bytes).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn image_webp_probe_cancel_wins_after_reads_and_seeks() {
        struct CancelIo<'a> {
            bytes: Cursor<Vec<u8>>,
            cancel: &'a AtomicBool,
            on_seek: bool,
        }
        impl Read for CancelIo<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(output)?;
                self.cancel.store(true, Ordering::Release);
                if count != 0 {
                    Err(io::ErrorKind::Interrupted.into())
                } else {
                    Ok(0)
                }
            }
        }
        impl Seek for CancelIo<'_> {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                let result = self.bytes.seek(position)?;
                if self.on_seek {
                    self.cancel.store(true, Ordering::Release);
                }
                Ok(result)
            }
        }
        let bytes = extended(2, 3, 2, &[]);
        for on_seek in [false, true] {
            let cancel = AtomicBool::new(false);
            let mut reader = CancelIo {
                bytes: Cursor::new(bytes.clone()),
                cancel: &cancel,
                on_seek,
            };
            let error = detect_reader(&mut reader, bytes.len() as u64, &cancel).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other);
        }
        assert_eq!(
            detect_reader(&mut Cursor::new(Vec::new()), 0, &AtomicBool::new(true))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
    }
}
