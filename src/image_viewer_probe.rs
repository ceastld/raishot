//! 有界格式探测只读文件头；像素、CRC及完整动画结构由随后打开的解码器校验。
#[path = "image_webp_probe.rs"]
mod webp;
use super::animation::Kind;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const PNG_SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";
const MAX_SCAN_SPAN: u64 = 8 * 1024 * 1024;
const MAX_CHUNKS: usize = 4096;

pub(super) fn detect(path: &Path, cancel: &AtomicBool) -> io::Result<Option<Kind>> {
    check_cancel(cancel)?;
    crate::image_collection::ensure_local_file(path).map_err(io::Error::other)?;
    check_cancel(cancel)?;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    detect_reader(&mut file, length, cancel)
}

fn invalid(zh: &'static str, en: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, crate::i18n::tr(zh, en))
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        // read_exact会重试Interrupted；取消必须使用不可重试的错误。
        Err(io::Error::other(tr!(
            "图片格式探测已取消",
            "Image format detection canceled"
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
        match reader.read(output) {
            Ok(0) => return Err(invalid("图片格式头被截断", "Truncated image format header")),
            Ok(count) => output = &mut output[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(invalid("图片格式头被截断", "Truncated image format header"));
            }
            Err(error) => return Err(error),
        }
    }
    check_cancel(cancel)
}

fn seek_checked(reader: &mut impl Seek, position: u64, cancel: &AtomicBool) -> io::Result<()> {
    check_cancel(cancel)?;
    reader.seek(SeekFrom::Start(position))?;
    check_cancel(cancel)
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn valid_ihdr(data: &[u8; 13]) -> bool {
    let width = be32(data);
    let height = be32(&data[4..]);
    width != 0
        && height != 0
        && width <= 0x7fff_ffff
        && height <= 0x7fff_ffff
        && matches!(
            (data[9], data[8]),
            (0, 1 | 2 | 4 | 8 | 16) | (2, 8 | 16) | (3, 1 | 2 | 4 | 8) | (4 | 6, 8 | 16)
        )
        && data[10] == 0
        && data[11] == 0
        && data[12] <= 1
}

fn detect_reader(
    reader: &mut (impl Read + Seek),
    length: u64,
    cancel: &AtomicBool,
) -> io::Result<Option<Kind>> {
    seek_checked(reader, 0, cancel)?;
    let mut signature = [0; 8];
    let prefix_len = length.min(signature.len() as u64) as usize;
    read_checked(reader, &mut signature[..prefix_len], cancel)?;
    if prefix_len >= 6 && (signature.starts_with(b"GIF87a") || signature.starts_with(b"GIF89a")) {
        return Ok(Some(Kind::Gif));
    }
    if prefix_len >= 4 && signature.starts_with(b"RIFF") {
        return webp::detect_reader(reader, length, cancel)
            .map(|animated| animated.then_some(Kind::Webp));
    }
    if prefix_len == 0 || signature[..prefix_len] != PNG_SIGNATURE[..prefix_len] {
        return Ok(None);
    }
    if prefix_len < PNG_SIGNATURE.len() {
        return Err(invalid("PNG文件头被截断", "Truncated PNG signature"));
    }

    let mut offset = PNG_SIGNATURE.len() as u64;
    for index in 0..MAX_CHUNKS {
        let header_end = offset
            .checked_add(8)
            .filter(|end| *end <= length)
            .ok_or_else(|| invalid("PNG块头被截断", "Truncated PNG chunk header"))?;
        if header_end > MAX_SCAN_SPAN {
            return Ok(None);
        }
        let mut header = [0; 8];
        read_checked(reader, &mut header, cancel)?;
        let size = u64::from(be32(&header));
        let kind: [u8; 4] = header[4..].try_into().unwrap();
        let end = header_end
            .checked_add(size)
            .and_then(|end| end.checked_add(4))
            .filter(|end| *end <= length)
            .ok_or_else(|| invalid("PNG块超出来源范围", "PNG chunk exceeds the source range"))?;
        if size > 0x7fff_ffff
            || !kind.iter().all(u8::is_ascii_alphabetic)
            || !kind[2].is_ascii_uppercase()
        {
            return Err(invalid("PNG块头无效", "Invalid PNG chunk header"));
        }
        if end > MAX_SCAN_SPAN {
            return Ok(None);
        }
        if index == 0 {
            if kind != *b"IHDR" || size != 13 {
                return Err(invalid("PNG缺少首个IHDR", "PNG is missing its first IHDR"));
            }
            let mut data = [0; 13];
            read_checked(reader, &mut data, cancel)?;
            if !valid_ihdr(&data) {
                return Err(invalid("PNG的IHDR无效", "Invalid PNG IHDR"));
            }
        } else {
            match &kind {
                b"IHDR" => return Err(invalid("PNG包含重复IHDR", "Duplicate PNG IHDR")),
                b"acTL" => {
                    if size != 8 {
                        return Err(invalid(
                            "APNG动画声明长度无效",
                            "Invalid APNG control length",
                        ));
                    }
                    let mut data = [0; 8];
                    read_checked(reader, &mut data, cancel)?;
                    let frames = be32(&data);
                    if frames == 0 || frames > 0x7fff_ffff || be32(&data[4..]) > 0x7fff_ffff {
                        return Err(invalid(
                            "APNG动画声明无效",
                            "Invalid APNG animation control",
                        ));
                    }
                    return Ok(Some(Kind::Apng));
                }
                b"IDAT" => return Ok(None),
                b"IEND" => {
                    return Err(invalid("PNG缺少图像数据", "PNG image data is missing"));
                }
                _ => {}
            }
        }
        seek_checked(reader, end, cancel)?;
        offset = end;
    }
    // 不让大量零长度辅助块占住工作线程；统一静态解码器仍可尝试读取。
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn png(animated: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            if animated {
                encoder.set_animated(1, 1).unwrap();
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[12, 34, 56, 255]).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn probe(bytes: Vec<u8>) -> io::Result<Option<Kind>> {
        let length = bytes.len() as u64;
        detect_reader(&mut Cursor::new(bytes), length, &token())
    }

    #[test]
    fn image_viewer_probe_uses_signatures_instead_of_filename_extensions() {
        for bytes in [b"GIF87a".to_vec(), b"GIF89a trailing data".to_vec()] {
            assert!(matches!(probe(bytes).unwrap(), Some(Kind::Gif)));
        }
        // Reader探测不接收名称；同一文件头被命名为任意扩展名时结果相同。
        assert!(matches!(probe(png(true)).unwrap(), Some(Kind::Apng)));
        assert!(probe(png(false)).unwrap().is_none());
        assert!(probe(b"GIF90a".to_vec()).unwrap().is_none());
    }

    #[test]
    fn image_viewer_probe_routes_animated_webp_through_the_bounded_header_probe() {
        let mut bytes = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
        bytes.extend_from_slice(&[2, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        // 只作格式分派；ANIM、帧、像素及预算交给来源检查。
        assert!(matches!(probe(bytes.clone()).unwrap(), Some(Kind::Webp)));
        bytes[20] = 0;
        assert!(probe(bytes.clone()).unwrap().is_none());
        bytes[20] = 0x80;
        assert_eq!(probe(bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut simple = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut simple)
            .encode(&[23, 61, 107, 80], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(probe(simple).unwrap().is_none());
    }

    #[test]
    fn image_viewer_probe_rejects_truncated_or_invalid_png_headers() {
        for bytes in [PNG_SIGNATURE[..5].to_vec(), PNG_SIGNATURE.to_vec()] {
            assert_eq!(probe(bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        let mut bytes = png(true);
        bytes[33..37].copy_from_slice(&7u32.to_be_bytes());
        assert_eq!(probe(bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);

        let mut bytes = png(false);
        bytes[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(probe(bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn image_viewer_probe_span_budget_does_not_seek_through_large_chunks() {
        let mut bytes = png(false);
        bytes.truncate(33);
        bytes.extend_from_slice(&(MAX_SCAN_SPAN as u32).to_be_bytes());
        bytes.extend_from_slice(b"ruSt");
        let mut reader = Cursor::new(bytes);
        // 声明的固定来源范围足够，但无需分配或读取大块体即可停止探测。
        assert!(detect_reader(&mut reader, MAX_SCAN_SPAN + 45, &token())
            .unwrap()
            .is_none());
        assert_eq!(reader.position(), 41);
    }

    #[test]
    fn image_viewer_probe_chunk_count_budget_is_bounded() {
        let mut bytes = png(false);
        bytes.truncate(33);
        for _ in 0..MAX_CHUNKS {
            bytes.extend_from_slice(&[0; 4]);
            bytes.extend_from_slice(b"ruSt");
            bytes.extend_from_slice(&[0; 4]);
        }
        let length = bytes.len() as u64;
        let mut reader = Cursor::new(bytes);
        assert!(detect_reader(&mut reader, length, &token())
            .unwrap()
            .is_none());
        assert_eq!(reader.position(), 33 + (MAX_CHUNKS as u64 - 1) * 12);
    }

    #[test]
    fn image_viewer_probe_cancellation_is_not_retryable_interrupted() {
        let cancel = AtomicBool::new(true);
        let mut reader = Cursor::new(png(true));
        let length = reader.get_ref().len() as u64;
        assert_eq!(
            detect_reader(&mut reader, length, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
        assert_eq!(reader.position(), 0);
    }
}
