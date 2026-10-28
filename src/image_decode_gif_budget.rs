//! GIF首图只观察目录与工作集；像素、透明及交错语义仍交给同一解码器。
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;

const FIXED_BYTES: u64 = 1024 * 1024;
const DEFAULT_METADATA_LIMIT: u64 = 50_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GifWorkspace {
    pub dimensions: (u32, u32),
    pub native_bytes: u64,
    pub constructor_bytes: u64,
    pub decode_extra_bytes: u64,
    /// 后端在调用方目标之外还会reserve的局部RGBA块；不是第二份额外驻留。
    pub backend_limit_floor: u64,
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "GIF首图结构无效或被截断",
        "Invalid or truncated GIF first-image structure"
    )
    .into()
}

fn budget() -> Box<dyn std::error::Error> {
    tr!(
        "GIF首图超过共享驻留预算",
        "GIF first image exceeds the shared resident budget"
    )
    .into()
}

fn sum(parts: &[u64]) -> Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |total, part| total.checked_add(*part))
        .ok_or_else(budget)
}

fn within_budget(external: u64, workspace: u64) -> Result<()> {
    if sum(&[external, workspace])? > super::MAX_DECODE_BYTES {
        return Err(budget());
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
    cancel: &'a AtomicBool,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        cancelled(self.cancel)?;
        let end = self.position.checked_add(length).ok_or_else(invalid)?;
        let bytes = self.bytes.get(self.position..end).ok_or_else(invalid)?;
        self.position = end;
        cancelled(self.cancel)?;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
}

#[derive(Clone, Copy, Default)]
struct Retained {
    length: u64,
    capacity: u64,
}

impl Retained {
    fn append(&mut self, bytes: u64, other_capacity: u64) -> Result<u64> {
        let length = sum(&[self.length, bytes])?;
        // 这里观察首帧前应用扩展的Vec增长，不把已经替换的同类扩展永久累计。
        // 一次追加最多255字节（XMP另带一个长度字节）；为增长后的容量留双倍，
        // 并在构造峰值中同时保留旧容量。小Vec的最低容量也单独覆盖。
        let capacity = self
            .capacity
            .max(length.checked_mul(2).ok_or_else(budget)?.max(8));
        if length > DEFAULT_METADATA_LIMIT {
            return Err(budget());
        }
        let peak = sum(&[other_capacity, self.capacity, capacity])?;
        self.length = length;
        self.capacity = capacity;
        Ok(peak)
    }
}

#[derive(Default)]
struct Applications {
    icc: Retained,
    xmp: Retained,
    peak: u64,
}

impl Applications {
    fn retained(&self) -> Result<u64> {
        sum(&[self.icc.capacity, self.xmp.capacity])
    }

    fn observe(&mut self, reader: &mut Cursor<'_>, external: u64) -> Result<()> {
        let length = usize::from(reader.byte()?);
        if length == 0 {
            return Ok(());
        }
        let identifier = reader.take(length)?;
        let kind = match identifier {
            b"ICCRGBG1012" => {
                self.icc = Retained::default();
                1
            }
            b"XMP DataXMP" => {
                self.xmp = Retained::default();
                2
            }
            _ => 0,
        };
        loop {
            let length = usize::from(reader.byte()?);
            if length == 0 {
                break;
            }
            reader.take(length)?;
            let peak = match kind {
                1 => self.icc.append(length as u64, self.xmp.capacity)?,
                // XMP保留每个子块的长度前缀；即使结尾ramp随后截短，容量仍然驻留。
                2 => self.xmp.append(length as u64 + 1, self.icc.capacity)?,
                _ => self.retained()?,
            };
            self.peak = self.peak.max(peak);
            within_budget(external, sum(&[FIXED_BYTES, self.peak])?)?;
        }
        Ok(())
    }
}

fn table_bytes(flags: u8) -> usize {
    if flags & 0x80 == 0 {
        0
    } else {
        3 * (1usize << ((flags & 7) + 1))
    }
}

fn little(bytes: &[u8]) -> u32 {
    u32::from(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// external_source_bytes包含仍驻留的压缩来源实际容量和宿主R，不重复复制来源。
/// 仅扫描首图前缀；不验证LZW熵编码，也不改变首帧的GCE或裁剪语义。
pub(super) fn inspect(
    bytes: &[u8],
    external_source_bytes: u64,
    cancel: &AtomicBool,
) -> Result<GifWorkspace> {
    cancelled(cancel)?;
    let result = inspect_inner(bytes, external_source_bytes, cancel);
    cancelled(cancel)?;
    result
}

fn inspect_inner(bytes: &[u8], external: u64, cancel: &AtomicBool) -> Result<GifWorkspace> {
    if external < bytes.len() as u64 || bytes.len() as u64 > super::MAX_DECODE_BYTES {
        return Err(budget());
    }
    // 观察器只借用切片；任何后端构造之前先为固定对象、palette及LZW暂存留量。
    within_budget(external, FIXED_BYTES)?;
    let mut reader = Cursor {
        bytes,
        position: 0,
        cancel,
    };
    if !matches!(reader.take(6)?, b"GIF87a" | b"GIF89a") {
        return Err(invalid());
    }
    let header = reader.take(7)?;
    let dimensions = (little(&header[..2]), little(&header[2..4]));
    let pixels = u64::from(dimensions.0) * u64::from(dimensions.1);
    if dimensions.0 == 0 || dimensions.1 == 0 || pixels > super::MAX_DECODE_PIXELS {
        return Err(budget());
    }
    let native_bytes = pixels.checked_mul(4).ok_or_else(budget)?;
    let global_palette = table_bytes(header[4]);
    reader.take(global_palette)?;
    let mut applications = Applications::default();
    loop {
        match reader.byte()? {
            0x21 => {
                let extension = reader.byte()?;
                if extension == 0xff {
                    applications.observe(&mut reader, external)?;
                } else {
                    // 注释、Plain Text和GCE均保留给像素解码器；不修改透明索引。
                    if !matches!(extension, 0xfe | 0x01 | 0xf9) {
                        return Err(invalid());
                    }
                    loop {
                        let length = usize::from(reader.byte()?);
                        if length == 0 {
                            break;
                        }
                        reader.take(length)?;
                    }
                }
            }
            0x2c => {
                let frame = reader.take(9)?;
                let (left, top) = (little(&frame[..2]), little(&frame[2..4]));
                let (width, height) = (little(&frame[4..6]), little(&frame[6..8]));
                if width == 0 || height == 0 {
                    return Err(invalid());
                }
                let local_palette = table_bytes(frame[8]);
                reader.take(local_palette)?;
                if global_palette == 0 && local_palette == 0 {
                    return Err(invalid());
                }
                // 保留后端接受的1..=11最小码宽；真正的码流有效性仍由后端判断。
                if !matches!(reader.byte()?, 1..=11) {
                    return Err(invalid());
                }
                let frame_pixels = u64::from(width) * u64::from(height);
                let direct = left == 0
                    && width == dimensions.0
                    && u64::from(top) + u64::from(height) <= u64::from(dimensions.1);
                let local_rgba = if direct {
                    0
                } else {
                    frame_pixels.checked_mul(4).ok_or_else(budget)?
                };
                // RGBA转换先解索引。交错图逐行处理，否则暂存可能是一整幅局部索引图。
                let indices = if frame[8] & 0x40 == 0 {
                    frame_pixels
                } else {
                    u64::from(width)
                };
                let constructor_bytes = sum(&[FIXED_BYTES, applications.peak])?;
                let decode_extra_bytes =
                    sum(&[FIXED_BYTES, applications.retained()?, indices, local_rgba])?;
                within_budget(external, constructor_bytes)?;
                within_budget(external, sum(&[native_bytes, decode_extra_bytes])?)?;
                return Ok(GifWorkspace {
                    dimensions,
                    native_bytes,
                    constructor_bytes,
                    decode_extra_bytes,
                    backend_limit_floor: local_rgba,
                });
            }
            _ => return Err(invalid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageDecoder;

    fn fixture(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder =
                gif::Encoder::new(&mut bytes, width, height, &[17, 43, 89, 211, 71, 5]).unwrap();
            encoder
                .write_frame(&gif::Frame {
                    width,
                    height,
                    transparent: Some(1),
                    buffer: std::borrow::Cow::Owned(
                        (0..usize::from(width) * usize::from(height))
                            .map(|index| (index % 2) as u8)
                            .collect(),
                    ),
                    ..Default::default()
                })
                .unwrap();
        }
        bytes
    }

    fn prefix(bytes: &[u8]) -> usize {
        13 + table_bytes(bytes[10])
    }

    fn application(identifier: &[u8; 11], data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x21, 0xff, 11];
        bytes.extend_from_slice(identifier);
        for chunk in data.chunks(255) {
            bytes.push(chunk.len() as u8);
            bytes.extend_from_slice(chunk);
        }
        bytes.push(0);
        bytes
    }

    fn add_prefix(bytes: &[u8], extensions: &[u8]) -> Vec<u8> {
        let position = prefix(bytes);
        let mut result = bytes[..position].to_vec();
        result.extend_from_slice(extensions);
        result.extend_from_slice(&bytes[position..]);
        result
    }

    fn workspace(bytes: &[u8]) -> GifWorkspace {
        inspect(bytes, bytes.len() as u64, &AtomicBool::new(false)).unwrap()
    }

    fn decode(bytes: &[u8]) -> Vec<u8> {
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let mut pixels = vec![0; decoder.total_bytes() as usize];
        decoder.read_image(&mut pixels).unwrap();
        pixels
    }

    #[test]
    fn image_decode_gif_budget_keeps_extensions_and_first_frame_alpha() {
        let source = fixture(2, 2);
        let mut extensions = vec![0x21, 0xfe, 3, b'a', b'b', b'c', 0];
        extensions.extend(application(b"ICCRGBG1012", &[11; 260]));
        extensions.extend(application(b"XMP DataXMP", &[31; 260]));
        let bytes = add_prefix(&source, &extensions);
        let observed = workspace(&bytes);
        assert_eq!(observed.dimensions, (2, 2));
        assert_eq!(observed.native_bytes, 16);
        assert_eq!(observed.backend_limit_floor, 0);
        assert!(observed.constructor_bytes > FIXED_BYTES);
        assert_eq!(decode(&bytes), decode(&source));
        let pixels = decode(&bytes);
        assert_eq!(&pixels[..4], &[17, 43, 89, 255]);
        assert_eq!(pixels[7], 0);
    }

    #[test]
    fn image_decode_gif_budget_replaced_applications_keep_peak_not_old_storage() {
        let source = fixture(1, 1);
        let mut final_extensions = application(b"ICCRGBG1012", &[1; 3]);
        final_extensions.extend(application(b"XMP DataXMP", &[2; 5]));
        let final_only = workspace(&add_prefix(&source, &final_extensions));
        let mut replaced = application(b"ICCRGBG1012", &[3; 4096]);
        replaced.extend(final_extensions);
        let replaced = workspace(&add_prefix(&source, &replaced));
        assert!(replaced.constructor_bytes > final_only.constructor_bytes);
        assert_eq!(replaced.decode_extra_bytes, final_only.decode_extra_bytes);

        let icc = workspace(&add_prefix(
            &source,
            &application(b"ICCRGBG1012", &[4; 260]),
        ));
        let xmp = workspace(&add_prefix(
            &source,
            &application(b"XMP DataXMP", &[4; 260]),
        ));
        assert!(xmp.decode_extra_bytes > icc.decode_extra_bytes);
    }

    #[test]
    fn image_decode_gif_budget_includes_outside_canvas_first_rectangle() {
        let source = fixture(3, 2);
        let full = workspace(&source);
        let mut small_canvas = source;
        small_canvas[6..8].copy_from_slice(&1u16.to_le_bytes());
        small_canvas[8..10].copy_from_slice(&1u16.to_le_bytes());
        let clipped = workspace(&small_canvas);
        assert_eq!(clipped.dimensions, (1, 1));
        assert_eq!(clipped.native_bytes, 4);
        assert_eq!(clipped.backend_limit_floor, 24);
        assert_eq!(clipped.decode_extra_bytes, full.decode_extra_bytes + 24);
        assert_eq!(decode(&small_canvas), [17, 43, 89, 255]);
    }

    #[test]
    fn image_decode_gif_budget_keeps_local_palette_and_interlaced_row_storage() {
        let source = fixture(3, 2);
        let start = prefix(&source);
        let image = start
            + source[start..]
                .iter()
                .position(|byte| *byte == 0x2c)
                .unwrap();
        let mut local = source[..13].to_vec();
        local[10] &= !0x80;
        local.extend_from_slice(&source[start..image + 10]);
        let flags = local.len() - 1;
        local[flags] = 0x80 | (source[10] & 7);
        local.extend_from_slice(&source[13..start]);
        local.extend_from_slice(&source[image + 10..]);
        assert_eq!(decode(&local), decode(&source));
        let plain = workspace(&local);
        local[flags] |= 0x40;
        let interlaced = workspace(&local);
        // 两行的交错访问仍是第0/1行；像素相同，索引暂存从整帧6B变为一行3B。
        assert_eq!(decode(&local), decode(&source));
        assert_eq!(plain.decode_extra_bytes, interlaced.decode_extra_bytes + 3);
        assert_eq!(interlaced.backend_limit_floor, 0);

        let table_start = flags + 1;
        assert!(inspect(
            &local[..table_start + 1],
            (table_start + 1) as u64,
            &AtomicBool::new(false)
        )
        .is_err());
    }

    #[test]
    fn image_decode_gif_budget_checks_combined_capacity_and_cancel_before_parse() {
        let bytes = fixture(2, 2);
        let observed = workspace(&bytes);
        let peak = observed
            .constructor_bytes
            .max(observed.native_bytes + observed.decode_extra_bytes);
        let remaining = super::super::MAX_DECODE_BYTES - peak;
        let cancel = AtomicBool::new(false);
        assert!(inspect(&bytes, remaining, &cancel).is_ok());
        assert!(inspect(&bytes, remaining + 1, &cancel).is_err());
        assert!(inspect(&bytes, bytes.len() as u64 - 1, &cancel).is_err());
        assert!(inspect(&bytes, u64::MAX, &cancel).is_err());
        let error = inspect(&[], 0, &AtomicBool::new(true)).unwrap_err();
        assert!(error.to_string().contains('消') || error.to_string().contains("canceled"));
    }

    #[test]
    fn image_decode_gif_budget_checks_subblock_palette_and_first_image_ranges() {
        let source = fixture(2, 2);
        for end in [0, 5, 12, prefix(&source) - 1] {
            assert!(inspect(&source[..end], end as u64, &AtomicBool::new(false)).is_err());
        }
        for extensions in [
            vec![0x21, 0xff, 11, b'I'],
            vec![0x21, 0xfe, 4, 1, 2],
            vec![0x21, 0x7f, 0],
        ] {
            let mut truncated = source[..prefix(&source)].to_vec();
            truncated.extend(extensions);
            assert!(inspect(&truncated, truncated.len() as u64, &AtomicBool::new(false)).is_err());
        }
        let mut no_frame = source[..prefix(&source)].to_vec();
        no_frame.push(0x3b);
        assert!(inspect(&no_frame, no_frame.len() as u64, &AtomicBool::new(false)).is_err());
    }
}
