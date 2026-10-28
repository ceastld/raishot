//! EXR目录与元数据的借用式前检。此阶段不解压像素，也不构造目录Vec。
//! 压缩工作区尚未证明时明确返回Pending，不能作为完整解码预算许可。
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;

const FIXED_BYTES: u64 = 64 * 1024;
const PREVIEW_STEP: u64 = 4 * 1024 * 1024;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const DECODER_THREAD_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Window {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Blocks {
    ScanLines {
        lines_per_block: u32,
    },
    Tiles {
        width: u32,
        height: u32,
        level_mode: u8,
        round_up: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    pub part_index: usize,
    pub data_window: Window,
    pub display_window: Window,
    pub has_alpha: bool,
    pub native_bytes: u64,
    pub channel_count: u32,
    pub channel_bytes: u64,
    pub compression: u8,
    pub blocks: Blocks,
    pub offset_table_start: usize,
    /// 此part全部分辨率层级的目录项数。
    pub chunk_count: usize,
    /// 实际交付首图的最大分辨率层级目录项数。
    pub selected_chunk_count: usize,
    channels_start: usize,
    channels_end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CompressionWorkspace {
    /// 目录事实已验证；每种压缩器及跨块scratch峰值还未获完整许可。
    Pending,
}

#[derive(Debug)]
pub(super) struct ExrWorkspace<'a> {
    source: &'a [u8],
    pub selection: Selection,
    pub native_bytes: u64,
    /// 原raw属性、typed属性及容器增长同时存在的构造阶段峰值。
    pub constructor_bytes: u64,
    /// 后端完整目录的保守保留量，不含借用source或caller像素。
    pub retained_metadata_bytes: u64,
    /// 在保留metadata与caller像素之外，过滤目录和属性副本的峰值。
    /// adapter的内部浮点图由聚合许可另计，此目录观察不授予完整解码额度。
    pub filter_extra_bytes: u64,
    /// 串行解块的专用线程栈；构造metadata不在该线程，不能提前叠加。
    pub decoder_thread_bytes: u64,
    pub header_count: usize,
    pub total_chunk_count: usize,
    pub compression: CompressionWorkspace,
    multipart: bool,
    metadata_end: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Channel<'a> {
    pub name: &'a [u8],
    /// 0=UINT，1=HALF，2=FLOAT。
    pub sample_type: u8,
    pub sampling_x: u32,
    pub sampling_y: u32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Channels<'a> {
    bytes: &'a [u8],
}

impl<'a> Channels<'a> {
    pub fn iter(self) -> ChannelIter<'a> {
        ChannelIter {
            remaining: self.bytes,
        }
    }
}

#[derive(Clone)]
pub(super) struct ChannelIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for ChannelIter<'a> {
    type Item = Channel<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.first().copied()? == 0 {
            return None;
        }
        let end = self.remaining.iter().position(|byte| *byte == 0)?;
        let name = self.remaining.get(..end)?;
        let fields = self.remaining.get(end + 1..end + 17)?;
        let sample_type = u32::from_le_bytes(fields.get(..4)?.try_into().ok()?) as u8;
        let sampling_x = u32::from_le_bytes(fields.get(8..12)?.try_into().ok()?);
        let sampling_y = u32::from_le_bytes(fields.get(12..16)?.try_into().ok()?);
        self.remaining = self.remaining.get(end + 17..)?;
        Some(Channel {
            name,
            sample_type,
            sampling_x,
            sampling_y,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SelectedChunk<'a> {
    pub table_index: usize,
    pub offset: u64,
    pub compression: u8,
    pub dimensions: (u32, u32),
    pub pixel_origin: (u32, u32),
    pub origin_y: i32,
    pub raw_block_bytes: u64,
    pub payload: &'a [u8],
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn invalid() -> Box<dyn std::error::Error> {
    tr!("EXR目录无效或被截断", "Invalid or truncated EXR directory").into()
}

fn unsupported() -> Box<dyn std::error::Error> {
    tr!(
        "EXR头不受当前后端支持",
        "EXR header is unsupported by the current backend"
    )
    .into()
}

fn budget() -> Box<dyn std::error::Error> {
    tr!(
        "EXR工作区超过共享驻留预算",
        "EXR workspace exceeds the shared resident budget"
    )
    .into()
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or_else(budget)
}
fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).ok_or_else(budget)
}
fn usize_of(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| budget())
}
fn check(resident: u64, extra: u64) -> Result<()> {
    if add(resident, extra)? > MAX_BYTES {
        return Err(budget());
    }
    Ok(())
}

fn bytes_at(bytes: &[u8], start: usize, length: usize) -> Result<&[u8]> {
    bytes
        .get(start..start.checked_add(length).ok_or_else(invalid)?)
        .ok_or_else(invalid)
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes_at(bytes, offset, 4)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}
fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
    Ok(u32_at(bytes, offset)? as i32)
}
fn u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes_at(bytes, offset, 8)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}

fn text_end(bytes: &[u8], start: usize, max: usize) -> Result<usize> {
    let remaining = bytes.get(start..).ok_or_else(invalid)?;
    remaining
        .iter()
        .take(max + 1)
        .position(|byte| *byte == 0)
        .map(|length| start + length)
        .ok_or_else(invalid)
}

// Vec/SmallVec的push或resize保留双倍逻辑量，增长时再覆盖一份旧块。
// 最小槽与固定对象另计；不把分配器自身的管理开销当作像素容量。
fn vector_retained(logical: u64, minimum: u64) -> Result<u64> {
    add(mul(logical, 2)?, minimum)
}
fn vector_peak(logical: u64, minimum: u64) -> Result<u64> {
    add(mul(logical, 3)?, minimum)
}

fn text_storage(length: usize) -> Result<u64> {
    if length <= 24 {
        Ok(0)
    } else {
        vector_retained(length as u64, 0)
    }
}

fn map_storage(entries: u64, growth: bool) -> Result<u64> {
    if entries == 0 {
        return Ok(0);
    }
    // 两个attribute map各按全部标签数计，包含7/8装载率、control bytes和增长旧表。
    let needed = add(mul(entries, 8)?, 6)? / 7;
    let buckets = needed
        .max(4)
        .checked_next_power_of_two()
        .ok_or_else(budget)?;
    let slot = add(
        size_of::<(
            exr::meta::attribute::Text,
            exr::meta::attribute::AttributeValue,
        )>() as u64,
        1,
    )?;
    mul(add(mul(buckets, slot)?, 32)?, if growth { 6 } else { 2 })
}

#[derive(Clone, Copy, Default)]
struct Dynamic {
    retained: u64,
    peak: u64,
}

fn dynamic(kind: &[u8], payload: &[u8], cancel: &AtomicBool) -> Result<Dynamic> {
    cancelled(cancel)?;
    let n = payload.len() as u64;
    let logical = match kind {
        b"string" => {
            if n > 24 {
                n
            } else {
                0
            }
        }
        b"stringvector" => {
            // 空字符串仍占4B长度；每项Text结构必须单计，不能只算字符bytes。
            add(
                n,
                mul(
                    add(n / 4, 1)?,
                    size_of::<exr::meta::attribute::Text>() as u64,
                )?,
            )?
        }
        b"chlist" => add(
            n,
            mul(
                add(n / 18, 1)?,
                size_of::<exr::meta::attribute::ChannelDescription>() as u64,
            )?,
        )?,
        b"preview" if payload.len() >= 8 => {
            let size = u64::from(u32_at(payload, 0)?)
                .checked_mul(u64::from(u32_at(payload, 4)?))
                .and_then(|pixels| pixels.checked_mul(4));
            // 两次checked_mul溢出时后端在任何Preview Vec前返回typed错误，
            // 宽容读取会忽略该字段；raw属性已单计。其它失败仍先resize一个读取步长。
            size.unwrap_or(0)
                .min(add(n.saturating_sub(8), PREVIEW_STEP)?)
        }
        b"box2i" | b"box2f" | b"int" | b"float" | b"double" | b"rational" | b"timecode"
        | b"v2i" | b"v2f" | b"v3i" | b"v3f" | b"chromaticities" | b"compression" | b"envmap"
        | b"keycode" | b"lineOrder" | b"m33f" | b"m44f" | b"tiledesc" => 0,
        // Custom及bytes还复制内容；bytes的type hint也由同一payload约束。
        _ => add(n, 1024)?,
    };
    Ok(Dynamic {
        retained: vector_retained(logical, 0)?,
        peak: vector_peak(logical, 4096)?,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct ChannelFacts {
    count: u32,
    bytes_per_pixel: u64,
    rgb: u8,
    alpha: bool,
}

fn channel_facts(payload: &[u8], cancel: &AtomicBool) -> Result<ChannelFacts> {
    let mut cursor = 0;
    let mut facts = ChannelFacts::default();
    let mut previous: Option<&[u8]> = None;
    loop {
        cancelled(cancel)?;
        if *payload.get(cursor).ok_or_else(invalid)? == 0 {
            break;
        }
        let end = text_end(payload, cursor, 256)?;
        let name = bytes_at(payload, cursor, end - cursor)?;
        if name.is_empty() || previous.is_some_and(|value| value > name) {
            return Err(invalid());
        }
        let fields = bytes_at(payload, end + 1, 16)?;
        let sample = u32_at(fields, 0)?;
        let sample_bytes = match sample {
            0 | 2 => 4,
            1 => 2,
            _ => return Err(invalid()),
        };
        if fields[4] > 1 {
            return Err(invalid());
        }
        // 锁定后端对任何channel subsampling均返回不支持，不扩大范围。
        if i32_at(fields, 8)? != 1 || i32_at(fields, 12)? != 1 {
            return Err(unsupported());
        }
        facts.count = facts.count.checked_add(1).ok_or_else(budget)?;
        facts.bytes_per_pixel = add(facts.bytes_per_pixel, sample_bytes)?;
        facts.rgb |= match name {
            b"R" => 1,
            b"G" => 2,
            b"B" => 4,
            _ => 0,
        };
        facts.alpha |= name == b"A";
        previous = Some(name);
        cursor = end + 17;
    }
    if facts.count == 0 {
        return Err(invalid());
    }
    Ok(facts)
}

fn window(payload: &[u8]) -> Result<Window> {
    let a = (i32_at(payload, 0)?, i32_at(payload, 4)?);
    let b = (i32_at(payload, 8)?, i32_at(payload, 12)?);
    // 后端将相反端点规范化，不把此合法读取行为缩成严格min<=max子集。
    let (x, y) = (a.0.min(b.0), a.1.min(b.1));
    let (right, bottom) = (i64::from(a.0.max(b.0)) + 1, i64::from(a.1.max(b.1)) + 1);
    let limit = i64::from(i32::MAX / 2);
    if i64::from(x) <= -limit || i64::from(y) <= -limit || right >= limit || bottom >= limit {
        return Err(invalid());
    }
    Ok(Window {
        x,
        y,
        width: u32::try_from(right - i64::from(x)).map_err(|_| invalid())?,
        height: u32::try_from(bottom - i64::from(y)).map_err(|_| invalid())?,
    })
}

fn scan_lines(compression: u8) -> Result<u32> {
    match compression {
        0..=2 => Ok(1),
        3 | 5 => Ok(16),
        4 | 6..=8 | 11 => Ok(32),
        9 | 10 => Ok(256),
        _ => Err(unsupported()),
    }
}

fn tile_description(payload: &[u8]) -> Result<Blocks> {
    let width = u32_at(payload, 0)?;
    let height = u32_at(payload, 4)?;
    let mode = *payload.get(8).ok_or_else(invalid)?;
    let limit = (i32::MAX / 2) as u32;
    if width == 0
        || height == 0
        || width >= limit
        || height >= limit
        || mode & 15 > 2
        || mode >> 4 > 1
    {
        return Err(invalid());
    }
    Ok(Blocks::Tiles {
        width,
        height,
        level_mode: mode & 15,
        round_up: mode >> 4 == 1,
    })
}

fn levels(size: u32, round_up: bool) -> u32 {
    let floor = 31 - size.leading_zeros();
    floor + 1 + u32::from(round_up && !size.is_power_of_two())
}
fn level_size(size: u32, level: u32, round_up: bool) -> u32 {
    let divisor = 1u64 << level;
    let value = (u64::from(size) + if round_up { divisor - 1 } else { 0 }) / divisor;
    value.max(1) as u32
}
fn block_count(size: u32, block: u32) -> u64 {
    (u64::from(size) + u64::from(block) - 1) / u64::from(block)
}

fn counts(data: Window, blocks: Blocks, cancel: &AtomicBool) -> Result<(usize, usize)> {
    match blocks {
        Blocks::ScanLines { lines_per_block } => {
            let count = usize_of(block_count(data.height, lines_per_block))?;
            Ok((count, count))
        }
        Blocks::Tiles {
            width,
            height,
            level_mode,
            round_up,
        } => {
            let largest = mul(
                block_count(data.width, width),
                block_count(data.height, height),
            )?;
            let (nx, ny) = match level_mode {
                0 => (1, 1),
                1 => (levels(data.width.max(data.height), round_up), 1),
                2 => (levels(data.width, round_up), levels(data.height, round_up)),
                _ => return Err(invalid()),
            };
            let mut total = 0;
            for y in 0..ny {
                for x in 0..nx {
                    cancelled(cancel)?;
                    let ly = if level_mode == 1 { x } else { y };
                    total = add(
                        total,
                        mul(
                            block_count(level_size(data.width, x, round_up), width),
                            block_count(level_size(data.height, ly, round_up), height),
                        )?,
                    )?;
                }
            }
            Ok((usize_of(total)?, usize_of(largest)?))
        }
    }
}

#[derive(Clone, Copy)]
struct Header {
    end: usize,
    data: Window,
    display: Window,
    channels: ChannelFacts,
    channels_start: usize,
    channels_end: usize,
    compression: u8,
    blocks: Blocks,
    deep: bool,
    chunks: usize,
    selected_chunks: usize,
    retained: u64,
    constructor_peak: u64,
}

fn parse_header(
    bytes: &[u8],
    start: usize,
    flags: u32,
    resident: u64,
    cancel: &AtomicBool,
) -> Result<Header> {
    let mut cursor = start;
    let max_name = if flags & 0x400 != 0 { 256 } else { 32 };
    let mut data = None;
    let mut display = None;
    let mut channels = None;
    let mut channels_range = (0, 0);
    let mut compression = None;
    let mut tiles = None;
    let mut block_type: Option<u8> = None;
    let mut retained = 0;
    let mut transient = 0;
    let mut entries = 0;
    loop {
        cancelled(cancel)?;
        if *bytes.get(cursor).ok_or_else(invalid)? == 0 {
            cursor += 1;
            break;
        }
        let name_end = text_end(bytes, cursor, max_name)?;
        let name = bytes_at(bytes, cursor, name_end - cursor)?;
        let kind_start = name_end + 1;
        let kind_end = text_end(bytes, kind_start, max_name)?;
        let kind = bytes_at(bytes, kind_start, kind_end - kind_start)?;
        let length = i32_at(bytes, kind_end + 1)?;
        if length < 0 {
            return Err(invalid());
        }
        let payload_start = kind_end.checked_add(5).ok_or_else(invalid)?;
        let payload = bytes_at(bytes, payload_start, length as usize)?;
        cursor = payload_start
            .checked_add(length as usize)
            .ok_or_else(invalid)?;
        let allocation = dynamic(kind, payload, cancel)?;
        let text_bytes = add(text_storage(name.len())?, text_storage(kind.len())?)?;
        retained = add(retained, add(text_bytes, allocation.retained)?)?;
        let raw_peak = vector_peak(payload.len() as u64, 128)?;
        transient = transient.max(add(raw_peak, allocation.peak)?);
        entries += 1;
        // 原raw与typed、旧map与新map全部在调用后端前证明可容纳。
        check(
            resident,
            add(add(retained, transient)?, map_storage(entries, true)?)?,
        )?;
        match (name, kind) {
            (b"dataWindow", b"box2i") => {
                if let Ok(value) = window(payload) {
                    data = Some(value);
                }
            }
            (b"displayWindow", b"box2i") => {
                if let Ok(value) = window(payload) {
                    display = Some(value);
                }
            }
            (b"channels", b"chlist") => {
                // malformed typed属性与后端一样可被后续有效属性替代。
                if let Ok(value) = channel_facts(payload, cancel) {
                    channels = Some(value);
                    channels_range = (payload_start, cursor);
                }
                cancelled(cancel)?;
            }
            (b"compression", b"compression") => {
                if let Some(value) = payload.first().copied().filter(|value| *value <= 11) {
                    compression = Some(value);
                }
            }
            (b"tiles", b"tiledesc") => {
                if let Ok(value) = tile_description(payload) {
                    tiles = Some(value);
                }
            }
            (b"type", b"string") => {
                block_type = Some(match payload {
                    b"scanlineimage" => 0,
                    b"tiledimage" => 1,
                    b"deepscanline" => 2,
                    b"deeptile" => 3,
                    _ => return Err(unsupported()),
                })
            }
            _ => {}
        }
    }
    let data = data.ok_or_else(invalid)?;
    let display = display.ok_or_else(invalid)?;
    let channels = channels.ok_or_else(invalid)?;
    let compression = compression.ok_or_else(invalid)?;
    let tiled = matches!(block_type, Some(1 | 3)) || (block_type.is_none() && flags & 0x200 != 0);
    let blocks = if tiled {
        tiles.ok_or_else(invalid)?
    } else {
        Blocks::ScanLines {
            lines_per_block: scan_lines(compression)?,
        }
    };
    let (chunks, selected_chunks) = counts(data, blocks, cancel)?;
    let retained = add(retained, map_storage(entries, false)?)?;
    let constructor_peak = add(add(retained, transient)?, map_storage(entries, true)?)?;
    Ok(Header {
        end: cursor,
        data,
        display,
        channels,
        channels_start: channels_range.0,
        channels_end: channels_range.1,
        compression,
        blocks,
        deep: matches!(block_type, Some(2 | 3)),
        chunks,
        selected_chunks,
        retained,
        constructor_peak,
    })
}

/// resident_bytes已经包含冻结输入的实际保留量；这里不再按文件长度重计来源。
pub(super) fn inspect<'a>(
    bytes: &'a [u8],
    resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ExrWorkspace<'a>> {
    cancelled(cancel)?;
    let result = inspect_inner(bytes, resident_bytes, cancel);
    cancelled(cancel)?;
    result
}

fn inspect_inner<'a>(
    bytes: &'a [u8],
    resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<ExrWorkspace<'a>> {
    check(resident_bytes, FIXED_BYTES)?;
    if bytes.len() as u64 > MAX_BYTES || resident_bytes < bytes.len() as u64 {
        return Err(budget());
    }
    if bytes_at(bytes, 0, 4)? != [0x76, 0x2f, 0x31, 0x01] {
        return Err(invalid());
    }
    let flags = u32_at(bytes, 4)?;
    // 与锁定后端的version/feature接受范围一致；不因tile/multipart/deep存在而直接拒绝。
    if flags & 15 != 2 || flags >> 13 != 0 {
        return Err(unsupported());
    }
    let multipart = flags & 0x1000 != 0;
    if flags & 0x200 != 0 && (multipart || flags & 0x800 != 0) {
        return Err(invalid());
    }
    let mut cursor = 8;
    let mut header_count = 0usize;
    let mut total_chunks = 0usize;
    let mut preceding_chunks = 0usize;
    let mut selected = None;
    let mut metadata = 0;
    let mut constructor_peak = FIXED_BYTES;
    let mut max_tiles = 0;
    loop {
        cancelled(cancel)?;
        if multipart && bytes.get(cursor).copied() == Some(0) {
            cursor += 1;
            break;
        }
        let header = parse_header(bytes, cursor, flags, add(resident_bytes, metadata)?, cancel)?;
        constructor_peak = constructor_peak.max(add(metadata, header.constructor_peak)?);
        metadata = add(metadata, header.retained)?;
        if selected.is_none() && !header.deep && header.channels.rgb == 7 {
            let native_bytes = mul(
                mul(
                    u64::from(header.display.width),
                    u64::from(header.display.height),
                )?,
                if header.channels.alpha { 16 } else { 12 },
            )?;
            super::check_dimensions((header.display.width, header.display.height), native_bytes)?;
            selected = Some((header, header_count, preceding_chunks, native_bytes));
        }
        header_count = header_count.checked_add(1).ok_or_else(budget)?;
        total_chunks = total_chunks.checked_add(header.chunks).ok_or_else(budget)?;
        preceding_chunks = total_chunks;
        max_tiles = max_tiles.max(header.chunks);
        cursor = header.end;
        let headers_peak = vector_peak(
            mul(
                header_count as u64,
                size_of::<exr::meta::header::Header>() as u64,
            )?,
            size_of::<exr::meta::header::Header>() as u64,
        )?;
        check(
            resident_bytes,
            add(add(constructor_peak, headers_peak)?, FIXED_BYTES)?,
        )?;
        if !multipart {
            break;
        }
    }
    let (header, part_index, previous_chunks, native_bytes) = selected.ok_or_else(unsupported)?;
    let table_bytes = mul(total_chunks as u64, 8)?;
    let tables_end = cursor
        .checked_add(usize_of(table_bytes)?)
        .ok_or_else(invalid)?;
    bytes_at(bytes, cursor, usize_of(table_bytes)?)?;
    let offset_table_start = cursor
        .checked_add(usize_of(mul(previous_chunks as u64, 8)?)?)
        .ok_or_else(invalid)?;
    let header_retained = vector_retained(
        mul(
            header_count as u64,
            size_of::<exr::meta::header::Header>() as u64,
        )?,
        size_of::<exr::meta::header::Header>() as u64,
    )?;
    let retained_metadata_bytes = add(metadata, header_retained)?;
    let headers_peak = vector_peak(
        mul(
            header_count as u64,
            size_of::<exr::meta::header::Header>() as u64,
        )?,
        size_of::<exr::meta::header::Header>() as u64,
    )?;
    constructor_peak = add(add(constructor_peak, headers_peak)?, FIXED_BYTES)?;
    // 全offset表、filtered offsets和一个header全部level TileIndices同时存在。
    let tables = add(
        vector_peak(table_bytes, 0)?,
        vector_peak(
            mul(header_count as u64, size_of::<Vec<u64>>() as u64)?,
            4 * size_of::<Vec<u64>>() as u64,
        )?,
    )?;
    let filtered = vector_peak(
        mul(header.selected_chunks as u64, 8)?,
        mul((header_count.saturating_mul(32).min(4096)) as u64, 8)?,
    )?;
    let tile_indices = vector_peak(
        mul(max_tiles as u64, size_of::<exr::meta::TileIndices>() as u64)?,
        4 * size_of::<exr::meta::TileIndices>() as u64,
    )?;
    // all_attributes克隆首header的shared及所选header的own；按完整metadata两份保守封住。
    let filter_extra_bytes = add(
        add(add(tables, filtered)?, tile_indices)?,
        add(mul(retained_metadata_bytes, 2)?, FIXED_BYTES)?,
    )?;
    check(resident_bytes, constructor_peak)?;
    check(
        resident_bytes,
        add(
            add(
                add(retained_metadata_bytes, native_bytes)?,
                filter_extra_bytes,
            )?,
            DECODER_THREAD_BYTES,
        )?,
    )?;
    // 后端只消费选中part最大level的offset。其余表值不影响分配，
    // 保持原读取范围：证明全表bytes/容量，但不强验未使用offset。
    cancelled(cancel)?;
    let workspace = ExrWorkspace {
        source: bytes,
        selection: Selection {
            part_index,
            data_window: header.data,
            display_window: header.display,
            has_alpha: header.channels.alpha,
            native_bytes,
            channel_count: header.channels.count,
            channel_bytes: header.channels.bytes_per_pixel,
            compression: header.compression,
            blocks: header.blocks,
            offset_table_start,
            chunk_count: header.chunks,
            selected_chunk_count: header.selected_chunks,
            channels_start: header.channels_start,
            channels_end: header.channels_end,
        },
        native_bytes,
        constructor_bytes: constructor_peak,
        retained_metadata_bytes,
        filter_extra_bytes,
        header_count,
        total_chunk_count: total_chunks,
        compression: CompressionWorkspace::Pending,
        decoder_thread_bytes: DECODER_THREAD_BYTES,
        multipart,
        metadata_end: tables_end,
    };
    workspace.for_each_selected_chunk(cancel, |_| Ok(()))?;
    cancelled(cancel)?;
    Ok(workspace)
}

impl<'a> ExrWorkspace<'a> {
    pub fn channels(&self) -> Channels<'a> {
        Channels {
            bytes: &self.source[self.selection.channels_start..self.selection.channels_end],
        }
    }

    /// 仅借用实际选中块；按目录逻辑序观察，不去重重复offset，不分配或解压。
    pub fn for_each_selected_chunk(
        &self,
        cancel: &AtomicBool,
        mut visit: impl FnMut(SelectedChunk<'a>) -> Result<()>,
    ) -> Result<()> {
        let selected = self.selection;
        for table_index in 0..selected.selected_chunk_count {
            cancelled(cancel)?;
            let offset = u64_at(self.source, selected.offset_table_start + table_index * 8)?;
            let mut cursor = usize_of(offset)?;
            if cursor < self.metadata_end {
                return Err(invalid());
            }
            if self.multipart {
                if i32_at(self.source, cursor)?
                    != i32::try_from(selected.part_index).map_err(|_| invalid())?
                {
                    return Err(invalid());
                }
                cursor = cursor.checked_add(4).ok_or_else(invalid)?;
            }
            let (x, y, width, height) = match selected.blocks {
                Blocks::ScanLines { lines_per_block } => {
                    // 宽容后端将真实leader与dataWindow之差做带符号除法，
                    // 并不要求leader恰好对齐或等于table index的预期位置。
                    let relative = i32_at(self.source, cursor)?
                        .checked_sub(selected.data_window.y)
                        .ok_or_else(invalid)?;
                    let index = relative
                        .checked_div(lines_per_block as i32)
                        .ok_or_else(invalid)?;
                    if index < 0 {
                        return Err(invalid());
                    }
                    let y = u32::try_from(mul(index as u64, u64::from(lines_per_block))?)
                        .map_err(|_| invalid())?;
                    if y >= selected.data_window.height {
                        return Err(invalid());
                    }
                    cursor = cursor.checked_add(4).ok_or_else(invalid)?;
                    (
                        0,
                        y,
                        selected.data_window.width,
                        lines_per_block.min(selected.data_window.height - y),
                    )
                }
                Blocks::Tiles { width, height, .. } => {
                    let tx = i32_at(self.source, cursor)?;
                    let ty = i32_at(self.source, cursor + 4)?;
                    if tx < 0
                        || ty < 0
                        || i32_at(self.source, cursor + 8)? != 0
                        || i32_at(self.source, cursor + 12)? != 0
                    {
                        return Err(invalid());
                    }
                    cursor = cursor.checked_add(16).ok_or_else(invalid)?;
                    let x =
                        u32::try_from(mul(tx as u64, u64::from(width))?).map_err(|_| invalid())?;
                    let y =
                        u32::try_from(mul(ty as u64, u64::from(height))?).map_err(|_| invalid())?;
                    if x >= selected.data_window.width || y >= selected.data_window.height {
                        return Err(invalid());
                    }
                    (
                        x,
                        y,
                        width.min(selected.data_window.width - x),
                        height.min(selected.data_window.height - y),
                    )
                }
            };
            let length = i32_at(self.source, cursor)?;
            if length < 0 {
                return Err(invalid());
            }
            cursor = cursor.checked_add(4).ok_or_else(invalid)?;
            let payload = bytes_at(self.source, cursor, length as usize)?;
            let raw_block_bytes = mul(
                mul(u64::from(width), u64::from(height))?,
                selected.channel_bytes,
            )?;
            // 锁定后端读取compressed Vec的hard max使用完整tile/scanline块，不是边缘裁切块。
            let max_pixels = match selected.blocks {
                Blocks::ScanLines { lines_per_block } => mul(
                    u64::from(selected.data_window.width),
                    u64::from(lines_per_block),
                )?,
                Blocks::Tiles { width, height, .. } => mul(u64::from(width), u64::from(height))?,
            };
            if length as u64 > mul(max_pixels, selected.channel_bytes)? {
                return Err(invalid());
            }
            let origin_y = i32::try_from(i64::from(selected.data_window.y) + i64::from(y))
                .map_err(|_| invalid())?;
            let result = visit(SelectedChunk {
                table_index,
                offset,
                compression: selected.compression,
                dimensions: (width, height),
                pixel_origin: (x, y),
                origin_y,
                raw_block_bytes,
                payload,
            });
            cancelled(cancel)?;
            result?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "image_decode_exr_budget_tests.rs"]
mod tests;
