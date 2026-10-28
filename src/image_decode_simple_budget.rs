//! QOI、farbfeld与DDS首图的可见工作区；像素仍交给原有统一后端。
//! 这里只验证影响尺寸或分配的头，不另写QOI熵解码器，也不复制压缩来源。
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;

// 三种构造器只保留reader及固定头。解码时QOI最多有256个RGBA索引项，
// farbfeld逐个WORD换端序，DDS逐个4×4块写进调用方目标。
// 此余量覆盖这些小对象、索引、探读及栈暂存；8KiB输入BufReader由宿主另计。
const FIXED_BYTES: u64 = 64 * 1024;

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "图片工作区头无效或被截断",
        "Invalid or truncated image workspace header"
    )
    .into()
}

fn unsupported() -> Box<dyn std::error::Error> {
    tr!(
        "DDS编码不受当前解码器支持",
        "DDS encoding is unsupported by the current decoder"
    )
    .into()
}

fn budget() -> Box<dyn std::error::Error> {
    tr!(
        "图片解码工作区超过共享驻留预算",
        "Image decode workspace exceeds the shared resident budget"
    )
    .into()
}

fn sum(parts: &[u64]) -> Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |total, bytes| total.checked_add(*bytes))
        .ok_or_else(budget)
}

fn word(bytes: &[u8], offset: usize, big_endian: bool) -> Result<u32> {
    let value: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or_else(invalid)?
        .try_into()
        .map_err(|_| invalid())?;
    Ok(if big_endian {
        u32::from_be_bytes(value)
    } else {
        u32::from_le_bytes(value)
    })
}

fn dimensions(bytes: &[u8], offset: usize, big_endian: bool) -> Result<(u32, u32)> {
    Ok((
        word(bytes, offset, big_endian)?,
        word(bytes, offset + 4, big_endian)?,
    ))
}

fn layout(dimensions: (u32, u32), channels: u64, row_bytes: u64) -> Result<super::CodecWorkspace> {
    let native_bytes = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(channels))
        .ok_or_else(budget)?;
    super::check_dimensions(dimensions, native_bytes)?;
    // DDS每次read_scanline新建一个encoded block-row Vec，先try_reserve(row)，
    // 再Take(row).read_to_end；不能把其容量当作恒等于逻辑行长。
    // 留一份旧行和双倍新行的摊销增长峰值，探读与最小容量由固定余量覆盖。
    // 没有第二份解压行或完整私有像素图；分配器自身开销不在此模型内。
    let rows = row_bytes.checked_mul(3).ok_or_else(budget)?;
    Ok(super::CodecWorkspace {
        dimensions,
        native_bytes,
        constructor_bytes: FIXED_BYTES,
        decode_extra_bytes: sum(&[FIXED_BYTES, rows])?,
        // 这三种decoder沿默认set_limits只检查尺寸，不另reserve私有缓冲。
        backend_limit_floor: 0,
    })
}

fn qoi(bytes: &[u8]) -> Result<super::CodecWorkspace> {
    let header = bytes.get(..14).ok_or_else(invalid)?;
    let channels = match header[12] {
        3 => 3,
        4 => 4,
        _ => return Err(invalid()),
    };
    if header[13] > 1 {
        return Err(invalid());
    }
    // 工作集与opcode序列无关。run、index、颜色及结尾padding仍由后端验证。
    layout(dimensions(header, 4, true)?, channels, 0)
}

fn farbfeld(bytes: &[u8]) -> Result<super::CodecWorkspace> {
    let dimensions = dimensions(bytes, 8, true)?;
    let workspace = layout(dimensions, 8, 0)?;
    let end = sum(&[16, workspace.native_bytes])?;
    if end > bytes.len() as u64 {
        return Err(invalid());
    }
    // 允许尾随数据，与原后端仅读取首图的行为一致。
    Ok(workspace)
}

fn dds(bytes: &[u8]) -> Result<super::CodecWorkspace> {
    bytes.get(..128).ok_or_else(invalid)?;
    const REQUIRED_FLAGS: u32 = 0x1007;
    const VALID_FLAGS: u32 = 0x008a_100f;
    let flags = word(bytes, 8, false)?;
    if word(bytes, 4, false)? != 124
        || flags & (REQUIRED_FLAGS | !VALID_FLAGS) != REQUIRED_FLAGS
        || word(bytes, 76, false)? != 32
    {
        return Err(invalid());
    }
    if word(bytes, 80, false)? & 4 == 0 {
        return Err(unsupported());
    }
    let (kind, payload_start) = match bytes.get(84..88).ok_or_else(invalid)? {
        b"DXT1" => (1, 128),
        b"DXT3" => (3, 128),
        b"DXT5" => (5, 128),
        b"DX10" => {
            bytes.get(..148).ok_or_else(invalid)?;
            let format = word(bytes, 128, false)?;
            let dimension = word(bytes, 132, false)?;
            let misc = word(bytes, 136, false)?;
            let array_size = word(bytes, 140, false)?;
            let alpha_mode = word(bytes, 144, false)?;
            if format > 132
                || !(2..=4).contains(&dimension)
                || !matches!(misc, 0 | 4)
                || (dimension == 4 && array_size != 1)
                || alpha_mode > 4
            {
                return Err(invalid());
            }
            let kind = match format {
                70..=72 => 1,
                73..=75 => 3,
                76..=78 => 5,
                _ => return Err(unsupported()),
            };
            (kind, 148)
        }
        _ => return Err(unsupported()),
    };
    let dimensions = (word(bytes, 16, false)?, word(bytes, 12, false)?);
    // 后端实际要求倍数4，并不要求二次幂；不把文档里的较窄描述套到这里。
    if dimensions.0 % 4 != 0 || dimensions.1 % 4 != 0 {
        return Err(invalid());
    }
    let block_bytes = if kind == 1 { 8 } else { 16 };
    let row_bytes = u64::from(dimensions.0 / 4) * block_bytes;
    let workspace = layout(dimensions, if kind == 1 { 3 } else { 4 }, row_bytes)?;
    let encoded_bytes = row_bytes
        .checked_mul(u64::from(dimensions.1 / 4))
        .ok_or_else(budget)?;
    if sum(&[payload_start, encoded_bytes])? > bytes.len() as u64 {
        return Err(invalid());
    }
    // pitch、mips、数组、cubemap等字段不触发本后端额外分配；仍只解第一平面。
    // 不按它们扩大工作区，也不禁止后端原本忽略的合法尾随平面。
    Ok(workspace)
}

/// input_reserved_bytes包含仍驻留的来源实际capacity和宿主R，不重复加bytes.len()。
/// 五字段与其它codec模型相同：构造与native+decode_extra分相位核算，extra不含
/// 来源或调用方native目标。magic不属于这三种时返回None，不改变其它格式范围。
pub(super) fn inspect(
    bytes: &[u8],
    input_reserved_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Option<super::CodecWorkspace>> {
    let result = (|| {
        cancelled(cancel)?;
        let parser = if bytes.starts_with(b"qoif") {
            qoi
        } else if bytes.starts_with(b"farbfeld") {
            farbfeld
        } else if bytes.starts_with(b"DDS ") {
            dds
        } else {
            return Ok(None);
        };
        if input_reserved_bytes < bytes.len() as u64 {
            return Err(budget());
        }
        let workspace = parser(bytes)?;
        super::check_shared_budget(&[input_reserved_bytes, workspace.constructor_bytes])?;
        super::check_shared_budget(&[
            input_reserved_bytes,
            workspace.native_bytes,
            workspace.decode_extra_bytes,
        ])?;
        Ok(Some(workspace))
    })();
    cancelled(cancel)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageDecoder;
    use std::io::{Cursor, Read};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    fn qoi_rgba(channels: u8) -> Vec<u8> {
        let mut bytes = b"qoif".to_vec();
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&[channels, 0]);
        if channels == 3 {
            bytes.extend_from_slice(&[254, 17, 33, 71, 254, 220, 5, 13]);
        } else {
            bytes.extend_from_slice(&[255, 17, 33, 71, 128, 255, 220, 5, 13, 0]);
        }
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
        bytes
    }

    fn farbfeld_rgba() -> Vec<u8> {
        let mut bytes = b"farbfeld".to_vec();
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        for value in [17u16, 33, 71, 128, 220, 5, 13, 0] {
            bytes.extend_from_slice(&(value * 257).to_be_bytes());
        }
        bytes
    }

    fn dds_blocks(kind: u32, dimensions: (u32, u32), dx10: bool) -> Vec<u8> {
        let mut bytes = b"DDS ".to_vec();
        for value in [124, 0x1007, dimensions.1, dimensions.0, 0, 0, 0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&[0; 44]);
        bytes.extend_from_slice(&32u32.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(if dx10 {
            b"DX10"
        } else {
            match kind {
                1 => b"DXT1",
                3 => b"DXT3",
                _ => b"DXT5",
            }
        });
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(&0x1000u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        assert_eq!(bytes.len(), 128);
        if dx10 {
            for value in [
                match kind {
                    1 => 72u32,
                    3 => 75,
                    _ => 78,
                },
                3,
                0,
                1,
                0,
            ] {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        for _ in 0..(dimensions.0 / 4) * (dimensions.1 / 4) {
            if kind == 3 {
                bytes.extend_from_slice(&[0x88; 8]);
            } else if kind == 5 {
                bytes.extend_from_slice(&[128, 0, 0, 0, 0, 0, 0, 0]);
            }
            bytes.extend_from_slice(&0xf800u16.to_le_bytes());
            bytes.extend_from_slice(&0x001fu16.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn image_decode_simple_workspace_real_pixels_match_ordinary_backend() {
        let cancel = AtomicBool::new(false);
        let cases = [
            (
                qoi_rgba(3),
                (2, 1),
                6,
                vec![71, 33, 17, 255, 13, 5, 220, 255],
            ),
            (qoi_rgba(4), (2, 1), 8, vec![71, 33, 17, 128, 13, 5, 220, 0]),
            (
                farbfeld_rgba(),
                (2, 1),
                16,
                vec![71, 33, 17, 128, 13, 5, 220, 0],
            ),
            (
                dds_blocks(1, (4, 4), false),
                (4, 4),
                48,
                [0, 0, 255, 255].repeat(16),
            ),
            (
                dds_blocks(3, (4, 4), false),
                (4, 4),
                64,
                [0, 0, 255, 136].repeat(16),
            ),
            (
                dds_blocks(5, (4, 4), true),
                (4, 4),
                64,
                [0, 0, 255, 128].repeat(16),
            ),
        ];
        for (bytes, dimensions, native_bytes, pixels) in cases {
            let workspace = inspect(&bytes, bytes.capacity() as u64, &cancel)
                .unwrap()
                .unwrap();
            assert_eq!(workspace.dimensions, dimensions);
            assert_eq!(workspace.native_bytes, native_bytes);
            assert_eq!(workspace.backend_limit_floor, 0);
            let decoder = image::ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()
                .unwrap()
                .into_decoder()
                .unwrap();
            assert_eq!(decoder.dimensions(), dimensions);
            assert_eq!(decoder.total_bytes(), native_bytes);
            let ordinary = super::super::decode_bytes(&bytes).unwrap();
            assert_eq!(ordinary.image.bgra, pixels);
            let foreground = super::super::decode_bytes_inner_with_budget(
                &bytes,
                bytes.capacity() as u64,
                super::super::MAX_DECODE_BYTES,
                &cancel,
                super::super::DecodeContent::File,
                super::super::DecodeBudget::Foreground(32 * 1024 * 1024),
            )
            .unwrap();
            assert_eq!(foreground.image.bgra, ordinary.image.bgra);
            assert_eq!(foreground.metadata, ordinary.metadata);
        }
    }

    #[test]
    fn image_decode_simple_workspace_phase_and_source_capacity_boundaries() {
        let cancel = AtomicBool::new(false);
        for bytes in [qoi_rgba(4), farbfeld_rgba(), dds_blocks(3, (12, 8), true)] {
            let workspace = inspect(&bytes, bytes.len() as u64, &cancel)
                .unwrap()
                .unwrap();
            let peak = workspace
                .constructor_bytes
                .max(workspace.native_bytes + workspace.decode_extra_bytes);
            let exact = super::super::MAX_DECODE_BYTES - peak;
            assert!(inspect(&bytes, exact, &cancel).is_ok());
            assert!(inspect(&bytes, exact + 1, &cancel).is_err());
            assert!(inspect(&bytes, bytes.len() as u64 - 1, &cancel).is_err());
            assert!(inspect(&bytes, u64::MAX, &cancel).is_err());
        }
        assert!(inspect(b"unrelated format", 0, &cancel).unwrap().is_none());
    }

    struct ObservedRead<'a> {
        bytes: Cursor<&'a [u8]>,
        largest: Arc<AtomicUsize>,
    }

    impl Read for ObservedRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.largest.fetch_max(buf.len(), Ordering::Relaxed);
            self.bytes.read(buf)
        }
    }

    #[test]
    fn image_decode_simple_workspace_dds_block_rows_dx10_and_ignored_padding() {
        let cancel = AtomicBool::new(false);
        for kind in [1, 3, 5] {
            for dx10 in [false, true] {
                let mut bytes = dds_blocks(kind, (12, 8), dx10);
                // 这些值不是decoder使用的row pitch；不能据它们分配大行或拒绝首图。
                bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
                bytes[28..32].copy_from_slice(&99u32.to_le_bytes());
                bytes.extend_from_slice(&[7; 64]);
                let workspace = inspect(&bytes, bytes.capacity() as u64, &cancel)
                    .unwrap()
                    .unwrap();
                let row = if kind == 1 { 24 } else { 48 };
                assert_eq!(workspace.decode_extra_bytes, FIXED_BYTES + 3 * row);
                let largest = Arc::new(AtomicUsize::new(0));
                let decoder = image::codecs::dds::DdsDecoder::new(ObservedRead {
                    bytes: Cursor::new(&bytes),
                    largest: largest.clone(),
                })
                .unwrap();
                largest.store(0, Ordering::Relaxed);
                let mut pixels = vec![0; workspace.native_bytes as usize];
                decoder.read_image(&mut pixels).unwrap();
                assert!(largest.load(Ordering::Relaxed) <= 2 * (row as usize).max(32));
                let alpha = match kind {
                    1 => 255,
                    3 => 136,
                    _ => 128,
                };
                let ordinary = super::super::decode_bytes(&bytes).unwrap();
                assert_eq!(ordinary.image.bgra, [0, 0, 255, alpha].repeat(96));
            }
        }
    }

    #[test]
    fn image_decode_simple_workspace_bad_headers_dimensions_and_truncated_rows() {
        let cancel = AtomicBool::new(false);
        for bytes in [b"qoif".to_vec(), b"farbfeld".to_vec(), b"DDS ".to_vec()] {
            assert!(inspect(&bytes, bytes.len() as u64, &cancel).is_err());
        }
        let valid = qoi_rgba(4);
        for (offset, value) in [(12, 2), (13, 2)] {
            let mut bytes = valid.clone();
            bytes[offset] = value;
            assert!(inspect(&bytes, bytes.len() as u64, &cancel).is_err());
        }
        for dimensions in [(0u32, 1u32), (65536, 1), (8000, 4001)] {
            let mut bytes = valid.clone();
            bytes[4..8].copy_from_slice(&dimensions.0.to_be_bytes());
            bytes[8..12].copy_from_slice(&dimensions.1.to_be_bytes());
            assert!(inspect(&bytes, bytes.len() as u64, &cancel).is_err());
        }
        for offset in [4, 8, 76, 80, 84, 16] {
            let mut bytes = dds_blocks(1, (4, 4), false);
            bytes[offset..offset + 4].copy_from_slice(&3u32.to_le_bytes());
            assert!(inspect(&bytes, bytes.len() as u64, &cancel).is_err());
        }
        for (offset, value) in [(128, 69u32), (132, 1), (136, 8), (144, 5)] {
            let mut bytes = dds_blocks(5, (4, 4), true);
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(inspect(&bytes, bytes.len() as u64, &cancel).is_err());
        }
        for mut bytes in [farbfeld_rgba(), dds_blocks(5, (4, 8), true)] {
            bytes.pop();
            assert!(inspect(&bytes, bytes.len() as u64, &cancel).is_err());
        }
    }

    #[test]
    fn image_decode_simple_workspace_cancel_precedes_bad_header_and_budget() {
        let cancel = AtomicBool::new(true);
        for bytes in [
            b"unrelated".as_slice(),
            b"qoif".as_slice(),
            b"DDS ".as_slice(),
        ] {
            let error = inspect(bytes, u64::MAX, &cancel).err().unwrap();
            assert_eq!(
                error.to_string(),
                tr!("图片读取已取消", "Image reading canceled")
            );
        }
    }
}
