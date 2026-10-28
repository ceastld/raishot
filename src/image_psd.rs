//! 有界读取PSD RGB8保存合并图；不提供图层或颜色管理能力。
use crate::capture::Image;
use std::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};

/// 读入控制量及上层短提示、固定元数据侧车共用这份预留。
pub(super) const CONTROL_BYTES: u64 = 64 * 1024;
const MAX_RESOURCES_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESOURCES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorStatus {
    Unmanaged,
}

pub(crate) struct Decoded {
    pub image: Image,
    pub color: ColorStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnsupportedFeature {
    Psb,
    Channels,
    Depth,
    ColorMode,
    ColorData,
    Metadata,
    LayerTransparency,
    NoMergedComposite,
    Compression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PsdError {
    Unsupported(UnsupportedFeature),
    Invalid,
    Budget,
    Cancelled,
}

impl fmt::Display for PsdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Unsupported(UnsupportedFeature::Psb) => {
                tr!("暂不读取PSB", "PSB is not supported")
            }
            Self::Unsupported(UnsupportedFeature::Channels) => tr!(
                "只读取PSD三通道合并图",
                "Only three-channel PSD composites are supported"
            ),
            Self::Unsupported(UnsupportedFeature::Depth) => tr!(
                "只读取PSD八位合并图",
                "Only 8-bit PSD composites are supported"
            ),
            Self::Unsupported(UnsupportedFeature::ColorMode) => tr!(
                "只读取PSD RGB合并图",
                "Only RGB PSD composites are supported"
            ),
            Self::Unsupported(UnsupportedFeature::ColorData) => tr!(
                "暂不读取PSD颜色模式数据",
                "PSD color-mode data is not supported"
            ),
            Self::Unsupported(UnsupportedFeature::Metadata) => tr!(
                "暂不读取带颜色配置或方向元数据的PSD",
                "PSD color profiles and orientation metadata are not supported"
            ),
            Self::Unsupported(UnsupportedFeature::LayerTransparency) => tr!(
                "暂不读取PSD合并透明度",
                "PSD composite transparency is not supported"
            ),
            Self::Unsupported(UnsupportedFeature::NoMergedComposite) => tr!(
                "PSD未保存可用的合并图，暂不能查看",
                "PSD has no usable saved composite"
            ),
            Self::Unsupported(UnsupportedFeature::Compression) => tr!(
                "只读取PSD Raw或PackBits压缩",
                "Only Raw and PackBits PSD compression are supported"
            ),
            Self::Invalid => tr!("PSD结构或像素数据无效", "Invalid PSD structure or pixels"),
            Self::Budget => tr!("PSD超过本次内存预算", "PSD exceeds the memory budget"),
            Self::Cancelled => tr!("PSD读取已取消", "PSD reading canceled"),
        };
        write!(formatter, "{message}")
    }
}

impl std::error::Error for PsdError {}

type Result<T> = std::result::Result<T, PsdError>;

/// 借用冻结来源；source_reserved_bytes为其真实容量，external_resident_bytes只含额外驻留量。
/// 只返回未色管的不透明RGB合并图，不保留ICC、EXIF、XMP或图层编辑资格。
pub(crate) fn decode(
    bytes: &[u8],
    source_reserved_bytes: u64,
    external_resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Decoded> {
    let result = decode_inner(
        bytes,
        source_reserved_bytes,
        external_resident_bytes,
        max_output_bytes,
        &|| checkpoint(cancel),
    );
    // 与错误同时到达的取消也优先，不交付已取消的整图或部分像素。
    checkpoint(cancel)?;
    result
}

fn checkpoint(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(PsdError::Cancelled)
    } else {
        Ok(())
    }
}

fn fits(source: u64, external: u64, output_capacity: u64) -> Result<()> {
    if external
        .checked_add(source)
        .and_then(|bytes| bytes.checked_add(output_capacity))
        .and_then(|bytes| bytes.checked_add(CONTROL_BYTES))
        .is_none_or(|bytes| bytes > crate::image_decode::MAX_DECODE_BYTES)
    {
        return Err(PsdError::Budget);
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(count).ok_or(PsdError::Invalid)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(PsdError::Invalid)?;
        self.position = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
    fn length(&mut self) -> Result<usize> {
        usize::try_from(self.u32()?).map_err(|_| PsdError::Invalid)
    }
    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.position..]
    }
}

fn resources(bytes: &[u8], check: &dyn Fn() -> Result<()>) -> Result<()> {
    let mut cursor = Cursor::new(bytes);
    let mut count = 0usize;
    let mut unsupported_metadata = false;
    while !cursor.remaining().is_empty() {
        check()?;
        count = count.checked_add(1).ok_or(PsdError::Budget)?;
        if count > MAX_RESOURCES {
            return Err(PsdError::Budget);
        }
        if cursor.take(4)? != b"8BIM" {
            return Err(PsdError::Invalid);
        }
        let id = cursor.u16()?;
        let name_length = usize::from(cursor.byte()?);
        cursor.take(name_length)?;
        if name_length % 2 == 0 {
            cursor.take(1)?;
        }
        let length = cursor.length()?;
        let payload = cursor.take(length)?;
        if length % 2 != 0 {
            cursor.take(1)?;
        }
        // XMP也可能声明方向；不猜测或忽略这些受支持范围外的像素解释。
        unsupported_metadata |= matches!(id, 1039 | 1058 | 1059 | 1060);
        if id == 1057 && *payload.get(4).ok_or(PsdError::Invalid)? == 0 {
            // Version Info前四字节为版本，随后标志否定真实合并图；后续占位像素不可用。
            return Err(PsdError::Unsupported(UnsupportedFeature::NoMergedComposite));
        }
    }
    if unsupported_metadata {
        return Err(PsdError::Unsupported(UnsupportedFeature::Metadata));
    }
    Ok(())
}

fn layers(bytes: &[u8], check: &dyn Fn() -> Result<()>) -> Result<()> {
    check()?;
    if bytes.is_empty() {
        return Ok(());
    }
    let mut cursor = Cursor::new(bytes);
    let length = cursor.length()?;
    if length % 2 != 0 {
        return Err(PsdError::Invalid);
    }
    let info = cursor.take(length)?;
    if !info.is_empty() {
        let count = info.get(..2).ok_or(PsdError::Invalid)?;
        if i16::from_be_bytes([count[0], count[1]]) < 0 {
            return Err(PsdError::Unsupported(UnsupportedFeature::LayerTransparency));
        }
    }
    let global_mask_length = cursor.length()?;
    if global_mask_length != 0 && global_mask_length < 13 {
        return Err(PsdError::Invalid);
    }
    cursor.take(global_mask_length)?;
    // 图层记录、通道及后续附加信息只借范围跳过；不声称验证或重建其内部内容。
    Ok(())
}

fn decode_inner(
    bytes: &[u8],
    source: u64,
    external: u64,
    max_output: u64,
    check: &dyn Fn() -> Result<()>,
) -> Result<Decoded> {
    check()?;
    if source < bytes.len() as u64 {
        return Err(PsdError::Invalid);
    }
    fits(source, external, 0)?;
    let mut cursor = Cursor::new(bytes);
    if cursor.take(4)? != b"8BPS" {
        return Err(PsdError::Invalid);
    }
    match cursor.u16()? {
        1 => {}
        2 => return Err(PsdError::Unsupported(UnsupportedFeature::Psb)),
        _ => return Err(PsdError::Invalid),
    }
    if cursor.take(6)? != [0; 6] {
        return Err(PsdError::Invalid);
    }
    match cursor.u16()? {
        3 => {}
        1..=56 => return Err(PsdError::Unsupported(UnsupportedFeature::Channels)),
        _ => return Err(PsdError::Invalid),
    }
    let height = cursor.u32()?;
    let width = cursor.u32()?;
    if !(1..=30_000).contains(&width) || !(1..=30_000).contains(&height) {
        return Err(PsdError::Invalid);
    }
    match cursor.u16()? {
        8 => {}
        1 | 16 | 32 => return Err(PsdError::Unsupported(UnsupportedFeature::Depth)),
        _ => return Err(PsdError::Invalid),
    }
    match cursor.u16()? {
        3 => {}
        0..=2 | 4 | 7..=9 => return Err(PsdError::Unsupported(UnsupportedFeature::ColorMode)),
        _ => return Err(PsdError::Invalid),
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(PsdError::Budget)?;
    let output_bytes = pixels.checked_mul(4).ok_or(PsdError::Budget)?;
    if pixels > crate::image_decode::MAX_DECODE_PIXELS || output_bytes > max_output {
        return Err(PsdError::Budget);
    }
    fits(source, external, output_bytes)?;
    check()?;
    let color_length = cursor.length()?;
    cursor.take(color_length)?;
    if color_length != 0 {
        return Err(PsdError::Unsupported(UnsupportedFeature::ColorData));
    }
    let resource_length = cursor.length()?;
    if resource_length > MAX_RESOURCES_BYTES {
        return Err(PsdError::Budget);
    }
    resources(cursor.take(resource_length)?, check)?;
    let layer_length = cursor.length()?;
    layers(cursor.take(layer_length)?, check)?;
    check()?;
    let compression = cursor.u16()?;
    let (table, payload) = match compression {
        0 => {
            if cursor.remaining().len() as u64 != pixels.checked_mul(3).ok_or(PsdError::Budget)? {
                return Err(PsdError::Invalid);
            }
            (&[][..], cursor.remaining())
        }
        1 => {
            let table_length = u64::from(height)
                .checked_mul(3)
                .and_then(|rows| rows.checked_mul(2))
                .and_then(|bytes| usize::try_from(bytes).ok())
                .ok_or(PsdError::Invalid)?;
            let table = cursor.take(table_length)?;
            let mut compressed = 0usize;
            for count in table.chunks_exact(2) {
                check()?;
                compressed = compressed
                    .checked_add(usize::from(u16::from_be_bytes([count[0], count[1]])))
                    .ok_or(PsdError::Invalid)?;
            }
            if compressed != cursor.remaining().len() {
                return Err(PsdError::Invalid);
            }
            (table, cursor.remaining())
        }
        2 | 3 => return Err(PsdError::Unsupported(UnsupportedFeature::Compression)),
        _ => return Err(PsdError::Invalid),
    };
    check()?;
    let length = usize::try_from(output_bytes).map_err(|_| PsdError::Budget)?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(length)
        .map_err(|_| PsdError::Budget)?;
    fits(source, external, bgra.capacity() as u64)?;
    bgra.resize(length, 255);
    let width_usize = usize::try_from(width).map_err(|_| PsdError::Invalid)?;
    let height_usize = usize::try_from(height).map_err(|_| PsdError::Invalid)?;
    let mut offset = 0usize;
    for channel in 0..3usize {
        for row in 0..height_usize {
            check()?;
            let output_start = row * width_usize * 4;
            if compression == 0 {
                let end = offset.checked_add(width_usize).ok_or(PsdError::Invalid)?;
                let values = payload.get(offset..end).ok_or(PsdError::Invalid)?;
                for (x, value) in values.iter().enumerate() {
                    bgra[output_start + x * 4 + 2 - channel] = *value;
                }
                offset = end;
            } else {
                let index = (channel * height_usize + row) * 2;
                let count = usize::from(u16::from_be_bytes([table[index], table[index + 1]]));
                let end = offset.checked_add(count).ok_or(PsdError::Invalid)?;
                packbits(
                    payload.get(offset..end).ok_or(PsdError::Invalid)?,
                    width_usize,
                    &mut bgra[output_start..output_start + width_usize * 4],
                    2 - channel,
                    check,
                )?;
                offset = end;
            }
        }
    }
    if offset != payload.len() {
        return Err(PsdError::Invalid);
    }
    check()?;
    Ok(Decoded {
        image: Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        },
        color: ColorStatus::Unmanaged,
    })
}

fn packbits(
    bytes: &[u8],
    width: usize,
    output: &mut [u8],
    channel: usize,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    let mut cursor = Cursor::new(bytes);
    let mut x = 0usize;
    while !cursor.remaining().is_empty() {
        check()?;
        let control = cursor.byte()? as i8;
        match control {
            -128 => {}
            0..=127 => {
                let count = control as usize + 1;
                let end = x
                    .checked_add(count)
                    .filter(|end| *end <= width)
                    .ok_or(PsdError::Invalid)?;
                for (index, value) in cursor.take(count)?.iter().enumerate() {
                    output[(x + index) * 4 + channel] = *value;
                }
                x = end;
            }
            -127..=-1 => {
                let count =
                    usize::try_from(1i16 - i16::from(control)).map_err(|_| PsdError::Invalid)?;
                let end = x
                    .checked_add(count)
                    .filter(|end| *end <= width)
                    .ok_or(PsdError::Invalid)?;
                let value = cursor.byte()?;
                for index in x..end {
                    output[index * 4 + channel] = value;
                }
                x = end;
            }
        }
    }
    if x != width {
        return Err(PsdError::Invalid);
    }
    check()
}

#[cfg(test)]
#[path = "image_psd_probe_tests.rs"]
mod tests;
