//! 受控TIFF分页来源：只读取经典主IFD链的无压缩、chunky、无符号8位RGB/直alpha RGBA。
//! 元数据先冻结为有界描述；选页只按已验证范围读取像素，不缓存整文件或其它页像素。
//! 默认静态入口另用preflight_metadata预检目录，仍由原后端处理压缩和BigTIFF像素。
use crate::{capture::Image, image_decode::DecodedImage, image_metadata::ImageMetadata};
use image::{metadata::Orientation, ColorType, ImageFormat};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_PAGES: usize = 128;
const MAX_TAGS: usize = 256;
const MAX_EXTERNAL_METADATA: u64 = 1024 * 1024;

#[path = "image_decode_tiff_budget.rs"]
mod workspace;
pub(crate) use workspace::{preflight_workspace, TiffWorkspace};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TiffPage {
    pub index: usize,
    pub encoded_dimensions: (u32, u32),
    pub display_dimensions: (u32, u32),
    pub orientation: Orientation,
    pub color: Option<ColorType>,
    pub supported: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}
impl Endian {
    fn u16(self, bytes: &[u8]) -> u16 {
        let bytes = bytes[..2].try_into().unwrap();
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }
    fn u32(self, bytes: &[u8]) -> u32 {
        let bytes = bytes[..4].try_into().unwrap();
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }
    fn u64(self, bytes: &[u8]) -> u64 {
        let bytes = bytes[..8].try_into().unwrap();
        match self {
            Self::Little => u64::from_le_bytes(bytes),
            Self::Big => u64::from_be_bytes(bytes),
        }
    }
}

/// 目录结构已经检查，不表示ICC已提取、颜色已转换或压缩像素已解码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TiffPreflight {
    pub big_tiff: bool,
    pub ifd_count: usize,
    pub external_metadata_bytes: u64,
    pub embedded_jpeg_metadata_bytes: u64,
}

#[derive(Clone, Copy)]
struct CheckedTag<'a> {
    tag: u16,
    kind: u16,
    count: u64,
    data: &'a [u8],
}

impl CheckedTag<'_> {
    fn unsigned(&self, endian: Endian, index: u64) -> Result<u64, String> {
        let width = match self.kind {
            1 => 1,
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => {
                return Err(message(
                    "TIFF指针或数组类型无效",
                    "Invalid TIFF pointer or array type",
                ))
            }
        };
        if index >= self.count {
            return Err(message(
                "TIFF数组索引越界",
                "TIFF array index is out of range",
            ));
        }
        let start = usize::try_from(index.checked_mul(width).ok_or_else(preflight_bad)?)
            .map_err(|_| preflight_bad())?;
        Ok(match width {
            1 => u64::from(self.data[start]),
            2 => u64::from(endian.u16(&self.data[start..])),
            4 => u64::from(endian.u32(&self.data[start..])),
            _ => endian.u64(&self.data[start..]),
        })
    }
}

fn preflight_bad() -> String {
    message(
        "TIFF元数据目录或范围无效",
        "Invalid TIFF metadata directory or range",
    )
}

fn metadata_slice(bytes: &[u8], offset: u64, count: u64) -> Result<&[u8], String> {
    let end = offset
        .checked_add(count)
        .filter(|end| *end <= bytes.len() as u64)
        .ok_or_else(preflight_bad)?;
    Ok(&bytes[offset as usize..end as usize])
}

struct Preflight<'a> {
    bytes: &'a [u8],
    endian: Endian,
    big: bool,
    // true为正在遍历，false为已完成；共享目录可复用，只有回到当前路径才算环。
    seen: Vec<(u64, bool)>,
    external_bytes: u64,
    jpeg_metadata_bytes: u64,
    jpeg_coded_bytes: u64,
    output_budget: u64,
    workspace_requested: bool,
    workspace: Option<TiffWorkspace>,
    external_source_bytes: u64,
    cancel: Option<&'a AtomicBool>,
}

fn preflight_checkpoint(cancel: Option<&AtomicBool>) -> Result<(), String> {
    if let Some(cancel) = cancel {
        cancelled(cancel).map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl Preflight<'_> {
    fn scalar(&self, entries: &[CheckedTag<'_>], tag: u16, default: u64) -> Result<u64, String> {
        match entries.iter().find(|entry| entry.tag == tag) {
            None => Ok(default),
            Some(entry) if entry.count == 1 => entry.unsigned(self.endian, 0),
            _ => Err(preflight_bad()),
        }
    }

    fn visit(&mut self, offset: u64) -> Result<(), String> {
        preflight_checkpoint(self.cancel)?;
        if offset == 0 {
            return Ok(());
        }
        if let Some((_, active)) = self.seen.iter().find(|(seen, _)| *seen == offset) {
            return if *active {
                Err(message("TIFF目录指针有环", "Cyclic TIFF directory pointer"))
            } else {
                Ok(())
            };
        }
        if self.seen.len() >= MAX_PAGES {
            return Err(message(
                "TIFF目录超过128项上限",
                "TIFF directory count exceeds 128",
            ));
        }
        let (header, count_bytes, row_bytes, pointer_bytes) = if self.big {
            (16, 8, 20, 8)
        } else {
            (8, 2, 12, 4)
        };
        if offset < header {
            return Err(preflight_bad());
        }
        let raw_count = metadata_slice(self.bytes, offset, count_bytes)?;
        let count = if self.big {
            self.endian.u64(raw_count)
        } else {
            u64::from(self.endian.u16(raw_count))
        };
        if count > MAX_TAGS as u64 {
            return Err(message(
                "TIFF每个IFD最多256个标签",
                "TIFF IFD exceeds the 256-tag limit",
            ));
        }
        let table_start = offset.checked_add(count_bytes).ok_or_else(preflight_bad)?;
        let table_length = count.checked_mul(row_bytes).ok_or_else(preflight_bad)?;
        let table = metadata_slice(
            self.bytes,
            table_start,
            table_length
                .checked_add(pointer_bytes)
                .ok_or_else(preflight_bad)?,
        )?;
        let node = self.seen.len();
        self.seen.push((offset, true));
        let mut entries = allocate(count as usize)?;
        for row in table[..table_length as usize].chunks_exact(row_bytes as usize) {
            preflight_checkpoint(self.cancel)?;
            let tag = self.endian.u16(row);
            if entries
                .iter()
                .any(|entry: &CheckedTag<'_>| entry.tag == tag)
            {
                return Err(message(
                    "TIFF目录含重复标签",
                    "Duplicate TIFF directory tag",
                ));
            }
            let kind = self.endian.u16(&row[2..]);
            let count = if self.big {
                self.endian.u64(&row[4..])
            } else {
                u64::from(self.endian.u32(&row[4..]))
            };
            let length = type_size(kind)
                .and_then(|size| count.checked_mul(size))
                .ok_or_else(|| {
                    message(
                        "TIFF标签类型或元素数无效",
                        "Invalid TIFF tag type or element count",
                    )
                })?;
            let value = if self.big { &row[12..20] } else { &row[8..12] };
            let data = if length <= pointer_bytes {
                &value[..length as usize]
            } else {
                self.external_bytes = self
                    .external_bytes
                    .checked_add(length)
                    .filter(|bytes| *bytes <= MAX_EXTERNAL_METADATA)
                    .ok_or_else(|| {
                        message(
                            "TIFF外置元数据超过1MiB上限",
                            "TIFF external metadata exceeds the 1 MiB limit",
                        )
                    })?;
                let position = if self.big {
                    self.endian.u64(value)
                } else {
                    u64::from(self.endian.u32(value))
                };
                if position < header {
                    return Err(preflight_bad());
                }
                metadata_slice(self.bytes, position, length)?
            };
            entries.push(CheckedTag {
                tag,
                kind,
                count,
                data,
            });
        }
        self.chunk_ranges(&entries)?;
        // 只检查压缩JPEG的自身头预算，不替换或重新编码任何压缩像素。
        let measure = node == 0 && self.workspace_requested;
        let jpeg_bytes = if self.scalar(&entries, 259, 1)? == 7 {
            self.jpeg_chunks(&entries, measure)?
        } else {
            0
        };
        if measure {
            self.workspace = Some(workspace::measure(
                &entries,
                self.endian,
                jpeg_bytes,
                self.external_source_bytes,
                self.cancel,
            )?);
        }
        let next = &table[table_length as usize..];
        let next = if self.big {
            self.endian.u64(next)
        } else {
            u64::from(self.endian.u32(next))
        };
        self.visit(next)?;
        for entry in &entries {
            if matches!(entry.tag, 330 | 34665 | 34853 | 40965) || matches!(entry.kind, 13 | 18) {
                if !matches!(entry.kind, 4 | 13 | 16 | 18)
                    || (matches!(entry.tag, 34665 | 34853 | 40965) && entry.count != 1)
                {
                    return Err(preflight_bad());
                }
                if entry.count > MAX_PAGES as u64 {
                    return Err(message(
                        "TIFF子目录指针超过128项",
                        "TIFF subdirectory pointers exceed 128",
                    ));
                }
                for index in 0..entry.count {
                    preflight_checkpoint(self.cancel)?;
                    self.visit(entry.unsigned(self.endian, index)?)?;
                }
            }
        }
        self.seen[node].1 = false;
        Ok(())
    }

    fn chunk_ranges(&self, entries: &[CheckedTag<'_>]) -> Result<(), String> {
        for (offset_tag, size_tag) in [(273, 279), (324, 325)] {
            let offsets = entries.iter().find(|entry| entry.tag == offset_tag);
            let sizes = entries.iter().find(|entry| entry.tag == size_tag);
            match (offsets, sizes) {
                (None, None) => {}
                (Some(offsets), Some(sizes)) if offsets.count == sizes.count => {
                    for index in 0..offsets.count {
                        preflight_checkpoint(self.cancel)?;
                        let offset = offsets.unsigned(self.endian, index)?;
                        let size = sizes.unsigned(self.endian, index)?;
                        checked_range(offset, size, self.bytes.len() as u64)?;
                    }
                }
                _ => return Err(preflight_bad()),
            }
        }
        Ok(())
    }

    fn jpeg_chunks(&mut self, entries: &[CheckedTag<'_>], measure: bool) -> Result<u64, String> {
        let offsets = entries
            .iter()
            .find(|entry| matches!(entry.tag, 273 | 324))
            .ok_or_else(preflight_bad)?;
        let sizes_tag = if offsets.tag == 273 { 279 } else { 325 };
        let sizes = entries
            .iter()
            .find(|entry| entry.tag == sizes_tag)
            .ok_or_else(preflight_bad)?;
        if offsets.count != sizes.count {
            return Err(preflight_bad());
        }
        let tables = entries.iter().find(|entry| entry.tag == 347);
        let tables = match tables {
            Some(entry)
                if matches!(entry.kind, 1 | 7)
                    && entry.data.len() >= 2
                    && entry.data.ends_with(&[0xff, 0xd9]) =>
            {
                &entry.data[..entry.data.len() - 2]
            }
            Some(_) => return Err(preflight_bad()),
            None => &[],
        };
        let mut peak = 0;
        for index in 0..offsets.count {
            preflight_checkpoint(self.cancel)?;
            let offset = offsets.unsigned(self.endian, index)?;
            let length = sizes.unsigned(self.endian, index)?;
            let mut data = metadata_slice(self.bytes, offset, length)?;
            if !tables.is_empty() {
                if !data.starts_with(&[0xff, 0xd8]) {
                    return Err(preflight_bad());
                }
                data = &data[2..];
            }
            self.jpeg_coded_bytes = self
                .jpeg_coded_bytes
                .checked_add(tables.len() as u64)
                .and_then(|total| total.checked_add(data.len() as u64))
                .filter(|total| *total <= crate::image_decode::MAX_DECODE_BYTES)
                .ok_or_else(|| {
                    message(
                        "TIFF的JPEG预检数据超过256MiB",
                        "TIFF JPEG inspection data exceeds 256 MiB",
                    )
                })?;
            peak = peak.max(preflight_jpeg_details(
                tables,
                data,
                &mut self.jpeg_metadata_bytes,
                self.output_budget,
                self.cancel,
                measure,
            )?);
        }
        Ok(peak)
    }
}

/// 保留后端的压缩、颜色、位深和BigTIFF能力；仅提前约束它会读到的目录与嵌入JPEG头。
/// 外置typed数据累计限1MiB，也限制按元素构造的中间值数量；不声称等于后端真实峰值。
pub(crate) fn preflight_metadata(bytes: &[u8]) -> Result<TiffPreflight, String> {
    preflight_metadata_with_budget(bytes, crate::image_decode::MAX_DECODE_BYTES)
}

/// 嵌入JPEG也受本次输出预算约束，防止小TIFF画布藏入另一幅超预算JPEG。
pub(crate) fn preflight_metadata_with_budget(
    bytes: &[u8],
    max_output_bytes: u64,
) -> Result<TiffPreflight, String> {
    let (endian, big, first) = preflight_header(bytes)?;
    let mut check = Preflight {
        bytes,
        endian,
        big,
        seen: allocate(MAX_PAGES)?,
        external_bytes: 0,
        jpeg_metadata_bytes: 0,
        jpeg_coded_bytes: 0,
        output_budget: max_output_bytes.min(crate::image_decode::MAX_DECODE_BYTES),
        workspace_requested: false,
        workspace: None,
        external_source_bytes: 0,
        cancel: None,
    };
    check.visit(first)?;
    Ok(TiffPreflight {
        big_tiff: big,
        ifd_count: check.seen.len(),
        external_metadata_bytes: check.external_bytes,
        embedded_jpeg_metadata_bytes: check.jpeg_metadata_bytes,
    })
}

fn preflight_header(bytes: &[u8]) -> Result<(Endian, bool, u64), String> {
    if bytes.len() as u64 > crate::image_decode::MAX_DECODE_BYTES || bytes.len() < 8 {
        return Err(preflight_bad());
    }
    let endian = match &bytes[..2] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return Err(preflight_bad()),
    };
    let (big, first) = match endian.u16(&bytes[2..]) {
        42 => (false, u64::from(endian.u32(&bytes[4..]))),
        43 => {
            if bytes.len() < 16 || endian.u16(&bytes[4..]) != 8 || endian.u16(&bytes[6..]) != 0 {
                return Err(preflight_bad());
            }
            (true, endian.u64(&bytes[8..]))
        }
        _ => return Err(preflight_bad()),
    };
    if first == 0 {
        return Err(preflight_bad());
    }
    Ok((endian, big, first))
}

struct JpegView<'a> {
    prefix: &'a [u8],
    data: &'a [u8],
}
impl JpegView<'_> {
    fn len(&self) -> usize {
        self.prefix.len() + self.data.len()
    }
    fn byte(&self, index: usize) -> Result<u8, String> {
        if index < self.prefix.len() {
            Ok(self.prefix[index])
        } else {
            self.data
                .get(index - self.prefix.len())
                .copied()
                .ok_or_else(preflight_bad)
        }
    }
    fn u16(&self, index: usize) -> Result<u16, String> {
        Ok(u16::from_be_bytes([
            self.byte(index)?,
            self.byte(index + 1)?,
        ]))
    }
    fn next_ff(&self, from: usize, cancel: Option<&AtomicBool>) -> Result<usize, String> {
        let mut base = 0;
        for segment in [self.prefix, self.data] {
            let offset = from.saturating_sub(base);
            if offset < segment.len() {
                for (index, chunk) in segment[offset..].chunks(64 * 1024).enumerate() {
                    preflight_checkpoint(cancel)?;
                    if let Some(found) = chunk.iter().position(|byte| *byte == 0xff) {
                        return Ok(base + offset + index * 64 * 1024 + found);
                    }
                }
            }
            base += segment.len();
        }
        Err(preflight_bad())
    }
}

fn preflight_jpeg(
    prefix: &[u8],
    data: &[u8],
    metadata_bytes: &mut u64,
    output_budget: u64,
) -> Result<(), String> {
    preflight_jpeg_details(prefix, data, metadata_bytes, output_budget, None, false).map(|_| ())
}

fn preflight_jpeg_details(
    prefix: &[u8],
    data: &[u8],
    metadata_bytes: &mut u64,
    output_budget: u64,
    cancel: Option<&AtomicBool>,
    measure: bool,
) -> Result<u64, String> {
    let view = JpegView { prefix, data };
    if view.u16(0)? != 0xffd8 {
        return Err(preflight_bad());
    }
    let mut position = 2;
    let mut entropy = false;
    let mut count = 0;
    let mut sof = false;
    let mut measured = 0;
    while position < view.len() {
        preflight_checkpoint(cancel)?;
        if entropy {
            position = view.next_ff(position, cancel)?;
        }
        if view.byte(position)? != 0xff {
            return Err(preflight_bad());
        }
        while view.byte(position)? == 0xff {
            position += 1;
            if position & 0xffff == 0 {
                preflight_checkpoint(cancel)?;
            }
        }
        let marker = view.byte(position)?;
        position += 1;
        if (entropy && marker == 0) || marker == 1 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        count += 1;
        if count > 4096 {
            return Err(message(
                "TIFF的JPEG头段超过4096项",
                "TIFF JPEG header segments exceed 4096",
            ));
        }
        if marker == 0xd9 {
            return if sof {
                Ok(measured)
            } else {
                Err(preflight_bad())
            };
        }
        if matches!(marker, 0 | 0xd8) {
            return Err(preflight_bad());
        }
        let length = usize::from(view.u16(position)?);
        let end = position
            .checked_add(length)
            .filter(|end| *end <= view.len())
            .ok_or_else(preflight_bad)?;
        if length < 2 {
            return Err(preflight_bad());
        }
        *metadata_bytes = metadata_bytes
            .checked_add(length as u64)
            .filter(|total| *total <= MAX_EXTERNAL_METADATA)
            .ok_or_else(|| {
                message(
                    "TIFF内嵌JPEG头超过1MiB上限",
                    "TIFF embedded JPEG headers exceed the 1 MiB limit",
                )
            })?;
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if sof || length < 8 {
                return Err(preflight_bad());
            }
            let height = u64::from(view.u16(position + 3)?);
            let width = u64::from(view.u16(position + 5)?);
            let channels = view.byte(position + 7)?;
            if width == 0
                || height == 0
                || width * height > crate::image_decode::MAX_DECODE_PIXELS
                || width * height * 4 > output_budget
                || !matches!(channels, 1 | 3 | 4)
                || width * height * u64::from(channels) > crate::image_decode::MAX_DECODE_BYTES
            {
                return Err(message(
                    "TIFF内嵌JPEG尺寸超过解码预算",
                    "TIFF embedded JPEG dimensions exceed the decode budget",
                ));
            }
            sof = true;
            if measure {
                measured = workspace::jpeg_frame(&view, position, length, marker)?;
            }
        }
        entropy = marker == 0xda || (entropy && marker == 0xdc);
        position = end;
    }
    Err(preflight_bad())
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: [u8; 4],
    bytes: Option<u64>,
}

#[derive(Clone, Copy)]
struct Strip {
    offset: u64,
    first_row: u32,
    rows: u32,
}

struct PageLayout {
    samples: usize,
    strips: Vec<Strip>,
}

pub(crate) struct TiffSource<R: Read + Seek> {
    reader: R,
    start: u64,
    length: u64,
    pages: Vec<TiffPage>,
    layouts: Vec<Option<PageLayout>>,
}

fn message(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        // read_exact会重试Interrupted；取消使用不可重试的错误。
        Err(io::Error::other(tr!(
            "TIFF读取已取消",
            "TIFF reading canceled"
        )))
    } else {
        Ok(())
    }
}

fn seek_checked(
    reader: &mut impl Seek,
    position: SeekFrom,
    cancel: &AtomicBool,
) -> Result<u64, String> {
    cancelled(cancel).map_err(|e| e.to_string())?;
    let position = reader.seek(position).map_err(|e| e.to_string())?;
    cancelled(cancel).map_err(|e| e.to_string())?;
    Ok(position)
}

fn read_checked(reader: &mut impl Read, output: &mut [u8], cancel: &AtomicBool) -> io::Result<()> {
    for mut remaining in output.chunks_mut(64 * 1024) {
        while !remaining.is_empty() {
            cancelled(cancel)?;
            match reader.read(remaining) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        tr!("TIFF来源被截断", "Truncated TIFF source"),
                    ))
                }
                Ok(count) => remaining = &mut remaining[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
    cancelled(cancel)
}

fn checked_range(offset: u64, bytes: u64, length: u64) -> Result<(), String> {
    if offset.checked_add(bytes).is_none_or(|end| end > length) {
        Err(message(
            "TIFF数据超出固定来源范围",
            "TIFF data exceeds the fixed source range",
        ))
    } else {
        Ok(())
    }
}

fn read_at<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    length: u64,
    offset: u64,
    output: &mut [u8],
    cancel: &AtomicBool,
) -> Result<(), String> {
    checked_range(offset, output.len() as u64, length)?;
    let position = start
        .checked_add(offset)
        .ok_or_else(|| message("TIFF位置溢出", "TIFF position overflow"))?;
    seek_checked(reader, SeekFrom::Start(position), cancel)?;
    read_checked(reader, output, cancel).map_err(|error| error.to_string())
}

fn allocate<T>(count: usize) -> Result<Vec<T>, String> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| message("TIFF缓冲分配失败", "TIFF buffer allocation failed"))?;
    if result.capacity() > count {
        return Err(resident_error());
    }
    Ok(result)
}

fn zeroes(count: usize) -> Result<Vec<u8>, String> {
    let mut bytes = allocate(count)?;
    bytes.resize(count, 0);
    Ok(bytes)
}

fn resident_error() -> String {
    message(
        "TIFF页驻留量超过256MiB预算",
        "TIFF page storage exceeds the 256 MiB resident budget",
    )
}

fn resident_budget(parts: &[u64]) -> Result<u64, String> {
    parts
        .iter()
        .try_fold(0u64, |total, bytes| total.checked_add(*bytes))
        .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
        .ok_or_else(resident_error)
}

fn capacity_bytes<T>(values: &Vec<T>) -> Result<u64, String> {
    u64::try_from(values.capacity())
        .ok()
        .and_then(|count| count.checked_mul(std::mem::size_of::<T>() as u64))
        .ok_or_else(resident_error)
}

fn zeroes_with_resident(
    count: usize,
    resident: u64,
    pending: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    cancelled(cancel).map_err(|error| error.to_string())?;
    resident_budget(&[resident, count as u64, pending])?;
    let allocated = allocate(count);
    cancelled(cancel).map_err(|error| error.to_string())?;
    let mut bytes = allocated?;
    resident_budget(&[resident, capacity_bytes(&bytes)?, pending])?;
    bytes.resize(count, 0);
    cancelled(cancel).map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn type_size(kind: u16) -> Option<u64> {
    match kind {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

fn entry<'a>(entries: &'a [Entry], tag: u16) -> Result<&'a Entry, String> {
    entries
        .iter()
        .find(|entry| entry.tag == tag)
        .ok_or_else(|| message("TIFF页缺少必要标签", "TIFF page is missing a required tag"))
}

struct MetadataReader<'a, R> {
    reader: &'a mut R,
    start: u64,
    length: u64,
    endian: Endian,
    cancel: &'a AtomicBool,
}

impl<R: Read + Seek> MetadataReader<'_, R> {
    fn unsigned(&mut self, entry: &Entry, count: usize) -> Result<Vec<u32>, String> {
        if entry.count as u64 != count as u64 || !matches!(entry.kind, 3 | 4) {
            return Err(message(
                "TIFF标签类型或元素数不受支持",
                "Unsupported TIFF tag type or element count",
            ));
        }
        let bytes = entry
            .bytes
            .ok_or_else(|| message("TIFF标签类型不受支持", "Unsupported TIFF tag type"))?;
        if bytes > MAX_EXTERNAL_METADATA {
            return Err(message(
                "TIFF标签超过元数据预算",
                "TIFF tag exceeds the metadata budget",
            ));
        }
        let width = if entry.kind == 3 { 2 } else { 4 };
        let mut values = allocate(count)?;
        if bytes <= 4 {
            for bytes in entry.value[..bytes as usize].chunks_exact(width) {
                values.push(if width == 2 {
                    u32::from(self.endian.u16(bytes))
                } else {
                    self.endian.u32(bytes)
                });
            }
        } else {
            let mut data = zeroes(bytes as usize)?;
            read_at(
                self.reader,
                self.start,
                self.length,
                u64::from(self.endian.u32(&entry.value)),
                &mut data,
                self.cancel,
            )?;
            for bytes in data.chunks_exact(width) {
                values.push(if width == 2 {
                    u32::from(self.endian.u16(bytes))
                } else {
                    self.endian.u32(bytes)
                });
            }
        }
        Ok(values)
    }

    fn scalar(&mut self, entries: &[Entry], tag: u16, default: Option<u32>) -> Result<u32, String> {
        match entries.iter().find(|entry| entry.tag == tag) {
            Some(value) => Ok(self.unsigned(value, 1)?[0]),
            None => default.ok_or_else(|| {
                message("TIFF页缺少必要标签", "TIFF page is missing a required tag")
            }),
        }
    }
}

fn dimensions_bytes(width: u32, height: u32) -> Result<usize, String> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > crate::export::MAX_DECODE_SIDE
        || height > crate::export::MAX_DECODE_SIDE
        || pixels > crate::image_decode::MAX_DECODE_PIXELS
    {
        return Err(message(
            "TIFF页超过65535单边或3200万像素上限",
            "TIFF page exceeds the 65535-side or 32-megapixel limit",
        ));
    }
    pixels
        .checked_mul(4)
        .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| {
            message(
                "TIFF页超过解码字节预算",
                "TIFF page exceeds the decoded-byte budget",
            )
        })
}

fn parse_page<R: Read + Seek>(
    metadata: &mut MetadataReader<'_, R>,
    entries: &[Entry],
    index: usize,
    entry_error: Option<String>,
) -> (TiffPage, Option<PageLayout>) {
    let mut info = TiffPage {
        index,
        encoded_dimensions: (0, 0),
        display_dimensions: (0, 0),
        orientation: Orientation::NoTransforms,
        color: None,
        supported: false,
        error: None,
    };
    let parsed = (|| -> Result<PageLayout, String> {
        let width = metadata.scalar(entries, 256, None)?;
        let height = metadata.scalar(entries, 257, None)?;
        info.encoded_dimensions = (width, height);
        info.display_dimensions = (width, height);
        dimensions_bytes(width, height)?;
        let orientation = metadata.scalar(entries, 274, Some(1))?;
        info.orientation = u8::try_from(orientation)
            .ok()
            .and_then(Orientation::from_exif)
            .ok_or_else(|| message("TIFF方向标签无效", "Invalid TIFF orientation tag"))?;
        if matches!(orientation, 5..=8) {
            info.display_dimensions = (height, width);
        }
        if let Some(error) = entry_error {
            return Err(error);
        }
        if entries.iter().any(|entry| entry.tag == 330) {
            return Err(message(
                "TIFF分页暂不读取子IFD",
                "TIFF pages do not support SubIFDs yet",
            ));
        }
        if metadata.scalar(entries, 259, Some(1))? != 1 {
            return Err(message(
                "TIFF分页仅支持无压缩strips",
                "TIFF pages support uncompressed strips only",
            ));
        }
        if metadata.scalar(entries, 266, Some(1))? != 1 {
            return Err(message(
                "TIFF分页仅支持正常位序FillOrder=1",
                "TIFF pages support FillOrder=1 only",
            ));
        }
        if metadata.scalar(entries, 262, None)? != 2 || metadata.scalar(entries, 284, Some(1))? != 1
        {
            return Err(message(
                "TIFF分页仅支持chunky RGB/RGBA",
                "TIFF pages support chunky RGB/RGBA only",
            ));
        }
        let samples = metadata.scalar(entries, 277, Some(1))? as usize;
        if !matches!(samples, 3 | 4) {
            return Err(message(
                "TIFF通道数不受支持",
                "Unsupported TIFF channel count",
            ));
        }
        if metadata
            .unsigned(entry(entries, 258)?, samples)?
            .iter()
            .any(|bits| *bits != 8)
        {
            return Err(message(
                "TIFF分页仅支持8位样本",
                "TIFF pages support 8-bit samples only",
            ));
        }
        if let Some(sample_format) = entries.iter().find(|entry| entry.tag == 339) {
            if metadata
                .unsigned(sample_format, samples)?
                .iter()
                .any(|format| *format != 1)
            {
                return Err(message(
                    "TIFF分页仅支持无符号整数样本",
                    "TIFF pages support unsigned integer samples only",
                ));
            }
        }
        let extra = entries.iter().find(|entry| entry.tag == 338);
        if samples == 4 {
            if metadata.unsigned(
                extra.ok_or_else(|| {
                    message(
                        "TIFF RGBA须明确声明直alpha",
                        "TIFF RGBA must explicitly declare unassociated alpha",
                    )
                })?,
                1,
            )? != [2]
            {
                return Err(message(
                    "TIFF RGBA仅支持直alpha",
                    "TIFF RGBA supports unassociated alpha only",
                ));
            }
        } else if extra.is_some() {
            return Err(message(
                "TIFF RGB不支持额外样本",
                "TIFF RGB does not support extra samples",
            ));
        }
        info.color = Some(if samples == 3 {
            ColorType::Rgb8
        } else {
            ColorType::Rgba8
        });
        if entries
            .iter()
            .any(|entry| matches!(entry.tag, 322 | 323 | 324 | 325))
        {
            return Err(message(
                "TIFF分页暂不读取tiles",
                "TIFF pages do not support tiles yet",
            ));
        }
        if metadata.scalar(entries, 317, Some(1))? != 1 {
            return Err(message(
                "TIFF分页不支持预测器",
                "TIFF pages do not support predictors",
            ));
        }
        let rows_per_strip = metadata.scalar(entries, 278, Some(u32::MAX))?;
        if rows_per_strip == 0 {
            return Err(message(
                "TIFF RowsPerStrip必须大于0",
                "TIFF RowsPerStrip must be positive",
            ));
        }
        let count = u64::from(height).div_ceil(u64::from(rows_per_strip)) as usize;
        let offsets = metadata.unsigned(entry(entries, 273)?, count)?;
        let byte_counts = metadata.unsigned(entry(entries, 279)?, count)?;
        let row_bytes = u64::from(width) * samples as u64;
        let mut strips = allocate(count)?;
        let mut row = 0u32;
        for (offset, bytes) in offsets.into_iter().zip(byte_counts) {
            cancelled(metadata.cancel).map_err(|error| error.to_string())?;
            let rows = rows_per_strip.min(height - row);
            if u64::from(bytes) != row_bytes * u64::from(rows) || offset < 8 {
                return Err(message(
                    "TIFF strip字节数与行布局不一致",
                    "TIFF strip byte count does not match its rows",
                ));
            }
            checked_range(u64::from(offset), u64::from(bytes), metadata.length)?;
            strips.push(Strip {
                offset: u64::from(offset),
                first_row: row,
                rows,
            });
            row = row
                .checked_add(rows)
                .ok_or_else(|| message("TIFF strip行数溢出", "TIFF strip row count overflow"))?;
        }
        if row != height {
            return Err(message(
                "TIFF strips没有覆盖完整页",
                "TIFF strips do not cover the page",
            ));
        }
        Ok(PageLayout { samples, strips })
    })();
    match parsed {
        Ok(layout) => {
            info.supported = true;
            (info, Some(layout))
        }
        Err(error) => {
            info.error = Some(error);
            (info, None)
        }
    }
}

impl TiffSource<File> {
    pub(crate) fn open(path: &Path, cancel: &AtomicBool) -> Result<Self, String> {
        cancelled(cancel).map_err(|error| error.to_string())?;
        crate::image_collection::ensure_local_file(path)?;
        Self::from_reader(File::open(path).map_err(|error| error.to_string())?, cancel)
    }
}

impl<R: Read + Seek> TiffSource<R> {
    pub(crate) fn from_reader(mut reader: R, cancel: &AtomicBool) -> Result<Self, String> {
        let start = seek_checked(&mut reader, SeekFrom::Current(0), cancel)?;
        let end = seek_checked(&mut reader, SeekFrom::End(0), cancel)?;
        let length = end
            .checked_sub(start)
            .ok_or_else(|| message("TIFF来源范围无效", "Invalid TIFF source range"))?;
        if length > crate::image_decode::MAX_DECODE_BYTES {
            return Err(message(
                "TIFF来源超过256MiB上限",
                "TIFF source exceeds the 256 MiB input limit",
            ));
        }
        let mut header = [0; 8];
        read_at(&mut reader, start, length, 0, &mut header, cancel)?;
        let endian = match &header[..2] {
            b"II" => Endian::Little,
            b"MM" => Endian::Big,
            _ => {
                return Err(message(
                    "TIFF字节序文件头无效",
                    "Invalid TIFF byte-order header",
                ))
            }
        };
        match endian.u16(&header[2..]) {
            42 => (),
            43 => {
                return Err(message(
                    "TIFF分页暂不支持BigTIFF",
                    "TIFF pages do not support BigTIFF yet",
                ))
            }
            _ => return Err(message("TIFF文件头无效", "Invalid TIFF header")),
        }
        let mut next = u64::from(endian.u32(&header[4..]));
        let mut seen = allocate(MAX_PAGES)?;
        let mut pages = allocate(MAX_PAGES)?;
        let mut layouts = allocate(MAX_PAGES)?;
        let mut external_bytes = 0u64;
        while next != 0 {
            cancelled(cancel).map_err(|error| error.to_string())?;
            if pages.len() >= MAX_PAGES || seen.contains(&next) {
                return Err(message(
                    "TIFF主IFD链有环或超过128页",
                    "TIFF main IFD chain cycles or exceeds 128 pages",
                ));
            }
            if next < 8 {
                return Err(message("TIFF的IFD位置无效", "Invalid TIFF IFD offset"));
            }
            seen.push(next);
            let mut count = [0; 2];
            read_at(&mut reader, start, length, next, &mut count, cancel)?;
            let count = usize::from(endian.u16(&count));
            if count > MAX_TAGS {
                return Err(message(
                    "TIFF每个IFD最多256个标签",
                    "TIFF IFD exceeds the 256-tag limit",
                ));
            }
            let bytes = count
                .checked_mul(12)
                .and_then(|bytes| bytes.checked_add(4))
                .ok_or_else(|| message("TIFF目录长度溢出", "TIFF directory length overflow"))?;
            checked_range(next, 2 + bytes as u64, length)?;
            let mut table = zeroes(bytes)?;
            read_at(&mut reader, start, length, next + 2, &mut table, cancel)?;
            let mut entries = allocate(count)?;
            let mut entry_error = None;
            for raw in table[..count * 12].chunks_exact(12) {
                cancelled(cancel).map_err(|error| error.to_string())?;
                let tag = endian.u16(raw);
                let kind = endian.u16(&raw[2..]);
                let count = endian.u32(&raw[4..]);
                let value: [u8; 4] = raw[8..12].try_into().unwrap();
                let bytes = type_size(kind).and_then(|size| size.checked_mul(u64::from(count)));
                if entries.iter().any(|entry: &Entry| entry.tag == tag) {
                    entry_error = Some(message(
                        "TIFF页含重复标签",
                        "TIFF page contains duplicate tags",
                    ));
                }
                if let Some(bytes) = bytes {
                    if bytes > 4 {
                        external_bytes = external_bytes
                            .checked_add(bytes)
                            .filter(|bytes| *bytes <= MAX_EXTERNAL_METADATA)
                            .ok_or_else(|| {
                                message(
                                    "TIFF外置元数据超过1MiB上限",
                                    "TIFF external metadata exceeds the 1 MiB limit",
                                )
                            })?;
                        if checked_range(u64::from(endian.u32(&value)), bytes, length).is_err() {
                            entry_error = Some(message(
                                "TIFF页的外置标签超出来源范围",
                                "TIFF page has an out-of-range external tag",
                            ));
                        }
                    }
                } else {
                    entry_error = Some(message(
                        "TIFF页含不支持的标签类型",
                        "TIFF page contains an unsupported tag type",
                    ));
                }
                entries.push(Entry {
                    tag,
                    kind,
                    count,
                    value,
                    bytes,
                });
            }
            next = u64::from(endian.u32(&table[count * 12..]));
            let mut metadata = MetadataReader {
                reader: &mut reader,
                start,
                length,
                endian,
                cancel,
            };
            let (page, layout) = parse_page(&mut metadata, &entries, pages.len(), entry_error);
            cancelled(cancel).map_err(|error| error.to_string())?;
            pages.push(page);
            layouts.push(layout);
        }
        if pages.is_empty() {
            return Err(message("TIFF主IFD链为空", "TIFF main IFD chain is empty"));
        }
        Ok(Self {
            reader,
            start,
            length,
            pages,
            layouts,
        })
    }

    pub(crate) fn pages(&self) -> &[TiffPage] {
        &self.pages
    }

    /// 来源自身拥有的目录；借用reader背后的压缩数据由调用方驻留量计入。
    pub(crate) fn retained_bytes(&self) -> Result<u64, String> {
        let mut bytes =
            resident_budget(&[capacity_bytes(&self.pages)?, capacity_bytes(&self.layouts)?])?;
        for page in &self.pages {
            bytes = resident_budget(&[
                bytes,
                page.error
                    .as_ref()
                    .map_or(0, |error| error.capacity() as u64),
            ])?;
        }
        for layout in self.layouts.iter().flatten() {
            bytes = resident_budget(&[bytes, capacity_bytes(&layout.strips)?])?;
        }
        Ok(bytes)
    }

    /// 只读取已冻结的strip描述；同长元数据改变不会扩大分配或切换到未经检查的布局。
    pub(crate) fn decode(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<DecodedImage, String> {
        self.decode_with_resident(index, max_output_bytes, 0, cancel)
    }

    /// 只分配一幅归一BGRA和一行原编码样本；不把文件长度当成内存驻留。
    /// 内存reader的输入拥有者及调用方目录应包含在external_resident_bytes中。
    pub(crate) fn decode_with_resident(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        external_resident_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<DecodedImage, String> {
        let decoded = self.decode_with_resident_inner(
            index,
            max_output_bytes,
            external_resident_bytes,
            cancel,
        );
        cancelled(cancel).map_err(|error| error.to_string())?;
        decoded
    }

    fn decode_with_resident_inner(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        external_resident_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<DecodedImage, String> {
        cancelled(cancel).map_err(|error| error.to_string())?;
        let resident = resident_budget(&[external_resident_bytes, self.retained_bytes()?])?;
        cancelled(cancel).map_err(|error| error.to_string())?;
        let page = self
            .pages
            .get(index)
            .ok_or_else(|| message("TIFF页索引越界", "TIFF page index is out of range"))?;
        let layout = self.layouts[index].as_ref().ok_or_else(|| {
            page.error
                .clone()
                .unwrap_or_else(|| message("TIFF页不受支持", "Unsupported TIFF page"))
        })?;
        let (width, height) = page.encoded_dimensions;
        let bytes = dimensions_bytes(width, height)?;
        if bytes as u64 > max_output_bytes {
            return Err(message(
                "TIFF页超过当前读取预算",
                "TIFF page exceeds the current output budget",
            ));
        }
        let row_bytes = (width as usize)
            .checked_mul(layout.samples)
            .ok_or_else(resident_error)?;
        let mut bgra = zeroes_with_resident(bytes, resident, row_bytes as u64, cancel)?;
        let row_resident = resident_budget(&[resident, capacity_bytes(&bgra)?])?;
        let mut row = zeroes_with_resident(row_bytes, row_resident, 0, cancel)?;
        let output_width = page.display_dimensions.0 as usize;
        for strip in &layout.strips {
            for local_y in 0..strip.rows {
                cancelled(cancel).map_err(|error| error.to_string())?;
                let offset = strip
                    .offset
                    .checked_add(u64::from(local_y) * row_bytes as u64)
                    .ok_or_else(|| message("TIFF像素位置溢出", "TIFF pixel position overflow"))?;
                read_at(
                    &mut self.reader,
                    self.start,
                    self.length,
                    offset,
                    &mut row,
                    cancel,
                )?;
                let y = strip.first_row + local_y;
                for (x, pixel) in row.chunks_exact(layout.samples).enumerate() {
                    // 直接写入归一方向坐标，只应用一次方向且不复制第二幅整图。
                    let (dx, dy) = display_position(x as u32, y, width, height, page.orientation);
                    let offset = (dy as usize * output_width + dx as usize) * 4;
                    bgra[offset..offset + 4].copy_from_slice(&[
                        pixel[2],
                        pixel[1],
                        pixel[0],
                        if layout.samples == 4 { pixel[3] } else { 255 },
                    ]);
                }
            }
        }
        cancelled(cancel).map_err(|error| error.to_string())?;
        Ok(DecodedImage {
            metadata_source: None,
            warning: None,
            image: Image {
                width: page.display_dimensions.0,
                height: page.display_dimensions.1,
                bgra,
                readback_ms: 0.,
            },
            metadata: ImageMetadata {
                format: ImageFormat::Tiff.into(),
                encoded_dimensions: Some(page.encoded_dimensions),
                display_dimensions: page.display_dimensions,
                orientation: Some(page.orientation),
                encoded_bytes: Some(self.length),
                decoded_color: page.color,
                details: None,
            },
        })
    }
}

fn display_position(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    orientation: Orientation,
) -> (u32, u32) {
    match orientation {
        Orientation::NoTransforms => (x, y),
        Orientation::FlipHorizontal => (width - 1 - x, y),
        Orientation::Rotate180 => (width - 1 - x, height - 1 - y),
        Orientation::FlipVertical => (x, height - 1 - y),
        Orientation::Rotate90FlipH => (y, x),
        Orientation::Rotate90 => (height - 1 - y, x),
        Orientation::Rotate270FlipH => (height - 1 - y, width - 1 - x),
        Orientation::Rotate270 => (y, width - 1 - x),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage, RgbaImage};
    use std::{
        io::Cursor,
        sync::{atomic::AtomicUsize, Arc, Mutex},
    };

    mod resident {
        include!("image_tiff_resident_tests.rs");
    }

    impl Endian {
        fn put16(self, value: u16) -> [u8; 2] {
            match self {
                Self::Little => value.to_le_bytes(),
                Self::Big => value.to_be_bytes(),
            }
        }
        fn put32(self, value: u32) -> [u8; 4] {
            match self {
                Self::Little => value.to_le_bytes(),
                Self::Big => value.to_be_bytes(),
            }
        }
        fn put64(self, value: u64) -> [u8; 8] {
            match self {
                Self::Little => value.to_le_bytes(),
                Self::Big => value.to_be_bytes(),
            }
        }
    }

    #[derive(Clone)]
    struct PageSpec {
        width: u32,
        height: u32,
        samples: u16,
        orientation: u16,
        compression: u16,
        fill_order: u16,
        planar: u16,
        bits: u16,
        extra: Option<u16>,
        rows_per_strip: u32,
        sub_ifd: bool,
        pixels: Vec<u8>,
    }
    fn rgb() -> PageSpec {
        PageSpec {
            width: 3,
            height: 2,
            samples: 3,
            orientation: 1,
            compression: 1,
            fill_order: 1,
            planar: 1,
            bits: 8,
            extra: None,
            rows_per_strip: 1,
            sub_ifd: false,
            pixels: vec![
                255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255,
            ],
        }
    }
    fn rgba() -> PageSpec {
        PageSpec {
            width: 2,
            height: 1,
            samples: 4,
            orientation: 8,
            compression: 1,
            fill_order: 1,
            planar: 1,
            bits: 8,
            extra: Some(2),
            rows_per_strip: 1,
            sub_ifd: false,
            pixels: vec![10, 20, 30, 0, 200, 100, 50, 128],
        }
    }
    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn deflated(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn big_compressed_fixture(
        endian: Endian,
        compression: u16,
        pixels: &[u8],
    ) -> (Vec<u8>, Vec<(u16, usize)>) {
        let mut tags = vec![
            (256u16, 4u16, endian.put32(3).to_vec()),
            (257, 4, endian.put32(2).to_vec()),
            (
                258,
                3,
                [endian.put16(8), endian.put16(8), endian.put16(8)].concat(),
            ),
            (259, 3, endian.put16(compression).to_vec()),
            (262, 3, endian.put16(2).to_vec()),
            (273, 16, vec![0; 8]),
            (277, 3, endian.put16(3).to_vec()),
            (278, 4, endian.put32(2).to_vec()),
            (279, 16, endian.put64(pixels.len() as u64).to_vec()),
            (284, 3, endian.put16(1).to_vec()),
        ];
        tags.sort_by_key(|(tag, _, _)| *tag);
        let payload = 16 + 8 + tags.len() * 20 + 8;
        let mut output = vec![0; payload];
        output[..2].copy_from_slice(match endian {
            Endian::Little => b"II",
            Endian::Big => b"MM",
        });
        output[2..4].copy_from_slice(&endian.put16(43));
        output[4..6].copy_from_slice(&endian.put16(8));
        output[8..16].copy_from_slice(&endian.put64(16));
        output[16..24].copy_from_slice(&endian.put64(tags.len() as u64));
        let mut entries = Vec::new();
        for (index, (tag, kind, data)) in tags.iter().enumerate() {
            let position = 24 + index * 20;
            output[position..position + 2].copy_from_slice(&endian.put16(*tag));
            output[position + 2..position + 4].copy_from_slice(&endian.put16(*kind));
            output[position + 4..position + 12]
                .copy_from_slice(&endian.put64(data.len() as u64 / type_size(*kind).unwrap()));
            output[position + 12..position + 12 + data.len()].copy_from_slice(data);
            if *tag == 273 {
                output[position + 12..position + 20].copy_from_slice(&endian.put64(payload as u64));
            }
            entries.push((*tag, position));
        }
        output.extend_from_slice(pixels);
        (output, entries)
    }

    #[test]
    fn image_tiff_preflight_keeps_real_deflate_packbits_and_big_tiff_backend_pixels() {
        let original = rgb();
        let mut packed = vec![original.pixels.len() as u8 - 1];
        packed.extend_from_slice(&original.pixels);
        for (compression, encoded) in [(8, deflated(&original.pixels)), (32773, packed)] {
            for endian in [Endian::Little, Endian::Big] {
                let mut spec = original.clone();
                spec.rows_per_strip = 2;
                spec.compression = compression;
                let mut classic = fixture(endian, &[spec]);
                let offset = classic.bytes.len() as u32;
                let (_, offsets) = classic.tag(0, 273);
                let (_, counts) = classic.tag(0, 279);
                classic.set32(offsets, offset);
                classic.set32(counts, encoded.len() as u32);
                classic.bytes.extend_from_slice(&encoded);
                let (big, _) = big_compressed_fixture(endian, compression, &encoded);
                for (bytes, expected_big) in [(&classic.bytes, false), (&big, true)] {
                    let result = preflight_metadata(bytes).unwrap();
                    assert_eq!(result.big_tiff, expected_big);
                    assert_eq!(result.ifd_count, 1);
                    let workspace =
                        preflight_workspace(bytes, bytes.capacity() as u64, &token()).unwrap();
                    assert_eq!(workspace.dimensions, (3, 2));
                    assert_eq!(workspace.native_bytes, 18);
                    assert!(workspace.decode_extra_bytes >= 18);
                    let peak = workspace
                        .constructor_bytes
                        .max(workspace.native_bytes + workspace.decode_extra_bytes);
                    let external = crate::image_decode::MAX_DECODE_BYTES - peak;
                    assert!(preflight_workspace(bytes, external, &token()).is_ok());
                    assert!(preflight_workspace(bytes, external + 1, &token()).is_err());
                    let decoded = image::ImageReader::with_format(
                        Cursor::new(bytes),
                        image::ImageFormat::Tiff,
                    )
                    .decode()
                    .unwrap()
                    .into_rgb8();
                    assert_eq!(decoded.dimensions(), (3, 2));
                    assert_eq!(decoded.into_raw(), original.pixels);
                    let unified = crate::image_decode::decode_bytes(bytes).unwrap();
                    assert_eq!((unified.image.width, unified.image.height), (3, 2));
                    let expected: Vec<_> = original
                        .pixels
                        .chunks_exact(3)
                        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
                        .collect();
                    assert_eq!(unified.image.bgra, expected);
                }
            }
        }
    }

    #[test]
    fn image_tiff_workspace_accepts_real_rgb16_rgba16_and_float_encoder_output() {
        use image::{ExtendedColorType, ImageEncoder};
        for (color, data, native) in [
            (ExtendedColorType::Rgb16, vec![0u8; 12], 12),
            (ExtendedColorType::Rgba16, vec![0u8; 16], 16),
            (ExtendedColorType::Rgb32F, vec![0u8; 24], 24),
            (ExtendedColorType::Rgba32F, vec![0u8; 32], 32),
        ] {
            let mut encoded = Vec::new();
            image::codecs::tiff::TiffEncoder::new(Cursor::new(&mut encoded))
                .write_image(&data, 2, 1, color)
                .unwrap();
            let plan = preflight_workspace(&encoded, encoded.capacity() as u64, &token()).unwrap();
            assert_eq!(plan.dimensions, (2, 1));
            assert_eq!(plan.native_bytes, native);
            assert!(plan.decode_extra_bytes >= native);
            // 同一真实编码字节由原统一适配器回读，不把合成声明当像素成功证明。
            let decoded = image::ImageReader::with_format(Cursor::new(&encoded), ImageFormat::Tiff)
                .decode()
                .unwrap();
            assert_eq!(decoded.color(), ColorType::try_from(color).unwrap());
            assert_eq!(decoded.as_bytes(), data);
        }
    }

    #[test]
    fn image_tiff_preflight_shared_nested_directories_are_valid_but_active_cycles_fail() {
        for endian in [Endian::Little, Endian::Big] {
            let mut page = rgb();
            page.sub_ifd = true;
            let mut source = fixture(endian, &[page.clone(), page.clone(), page]);
            let (_, first_pointer) = source.tag(0, 330);
            let (second_entry, second_pointer) = source.tag(1, 330);
            let shared = source.ifds[2] as u32;
            source.set32(first_pointer, shared);
            source.set32(second_pointer, shared);
            source.set16(second_entry + 2, 13);
            assert_eq!(preflight_metadata(&source.bytes).unwrap().ifd_count, 3);
            source.set32(second_pointer, source.ifds[0] as u32);
            assert!(preflight_metadata(&source.bytes).is_err());
            source.set32(second_pointer, u32::MAX);
            assert!(preflight_metadata(&source.bytes).is_err());
        }
    }

    #[test]
    fn image_tiff_preflight_checks_big_headers_types_counts_and_metadata_before_backend_new() {
        for endian in [Endian::Little, Endian::Big] {
            let (valid, entries) = big_compressed_fixture(endian, 1, &rgb().pixels);
            assert!(preflight_metadata(&valid).is_ok());
            let mut cases = vec![valid[..15].to_vec()];
            let mut bytes = valid.clone();
            bytes[4..6].copy_from_slice(&endian.put16(4));
            cases.push(bytes);
            let mut bytes = valid.clone();
            bytes[6..8].copy_from_slice(&endian.put16(1));
            cases.push(bytes);
            let mut bytes = valid.clone();
            bytes[8..16].copy_from_slice(&endian.put64(u64::MAX));
            cases.push(bytes);
            let mut bytes = valid.clone();
            bytes[16..24].copy_from_slice(&endian.put64(257));
            cases.push(bytes);
            let width = entries.iter().find(|(tag, _)| *tag == 256).unwrap().1;
            let mut bytes = valid.clone();
            bytes[width + 4..width + 12].copy_from_slice(&endian.put64(u64::MAX));
            cases.push(bytes);
            let mut bytes = valid.clone();
            bytes[width + 2..width + 4].copy_from_slice(&endian.put16(99));
            cases.push(bytes);
            let mut bytes = valid.clone();
            bytes[width..width + 2].copy_from_slice(&endian.put16(34675));
            bytes[width + 2..width + 4].copy_from_slice(&endian.put16(7));
            bytes[width + 4..width + 12].copy_from_slice(&endian.put64(MAX_EXTERNAL_METADATA + 1));
            cases.push(bytes);
            let mut bytes = valid;
            bytes[width + 4..width + 12].copy_from_slice(&endian.put64(3));
            bytes[width + 12..width + 20].copy_from_slice(&endian.put64(u64::MAX));
            cases.push(bytes);
            for bytes in cases {
                assert!(preflight_metadata(&bytes).is_err());
            }
        }
        let too_many = fixture(Endian::Little, &vec![rgb(); MAX_PAGES + 1]);
        assert!(preflight_metadata(&too_many.bytes).is_err());
    }

    #[test]
    fn image_tiff_preflight_modern_jpeg_checks_real_strips_tables_and_nested_sof_budget() {
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 100)
            .encode(&[128; 6], 3, 2, image::ExtendedColorType::L8)
            .unwrap();
        let mut spec = rgb();
        spec.samples = 1;
        spec.rows_per_strip = 2;
        spec.compression = 7;
        spec.pixels = vec![128; 6];
        let mut source = fixture(Endian::Little, &[spec]);
        let (_, photo) = source.tag(0, 262);
        source.set16(photo, 1);
        let (_, offsets) = source.tag(0, 273);
        let (_, counts) = source.tag(0, 279);
        let start = source.bytes.len();
        source.set32(offsets, start as u32);
        source.set32(counts, encoded.len() as u32);
        source.bytes.extend_from_slice(&encoded);
        assert!(preflight_metadata(&source.bytes).is_ok());
        let decoded = image::ImageReader::new(Cursor::new(&source.bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_luma8();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert!(decoded
            .as_raw()
            .iter()
            .all(|value| value.abs_diff(128) <= 1));
        let mut split_metadata = 0;
        assert!(preflight_jpeg(&encoded[..3], &encoded[3..], &mut split_metadata, 24).is_ok());
        assert!(preflight_metadata_with_budget(&source.bytes, 24).is_ok());
        assert!(preflight_metadata_with_budget(&source.bytes, 23).is_err());
        let sof = encoded
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc0])
            .unwrap();
        // 真实JPEG前缀拆成共享JPEGTables，后端重组应仍等于原码流。
        let mut tables = encoded[..sof].to_vec();
        tables.extend_from_slice(&[0xff, 0xd9]);
        let mut shared = source.bytes.clone();
        let table_start = shared.len();
        shared.extend_from_slice(&tables);
        let data_start = shared.len();
        shared.extend_from_slice(&[0xff, 0xd8]);
        shared.extend_from_slice(&encoded[sof..]);
        let (table_entry, table_value) = source.tag(0, 266);
        shared[table_entry..table_entry + 2].copy_from_slice(&347u16.to_le_bytes());
        shared[table_entry + 2..table_entry + 4].copy_from_slice(&7u16.to_le_bytes());
        shared[table_entry + 4..table_entry + 8]
            .copy_from_slice(&(tables.len() as u32).to_le_bytes());
        shared[table_value..table_value + 4].copy_from_slice(&(table_start as u32).to_le_bytes());
        shared[offsets..offsets + 4].copy_from_slice(&(data_start as u32).to_le_bytes());
        shared[counts..counts + 4]
            .copy_from_slice(&((2 + encoded.len() - sof) as u32).to_le_bytes());
        let workspace = preflight_workspace(&shared, shared.capacity() as u64, &token()).unwrap();
        assert!(workspace.constructor_bytes >= tables.len() as u64 * 64);
        assert!(workspace.decode_extra_bytes > workspace.native_bytes);
        let rebuilt = image::ImageReader::with_format(Cursor::new(&shared), ImageFormat::Tiff)
            .decode()
            .unwrap()
            .into_luma8();
        assert!(rebuilt
            .as_raw()
            .iter()
            .all(|value| value.abs_diff(128) <= 1));
        let mut oversized = source.bytes.clone();
        oversized[start + sof + 5..start + sof + 7].copy_from_slice(&u16::MAX.to_be_bytes());
        oversized[start + sof + 7..start + sof + 9].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(preflight_metadata(&oversized).is_err());
        let mut metadata_bytes = 0;
        let mut bloated = encoded[..2].to_vec();
        for _ in 0..17 {
            bloated.extend_from_slice(&[0xff, 0xe1, 0xff, 0xff]);
            bloated.extend_from_slice(&vec![0; 65_533]);
        }
        bloated.extend_from_slice(&encoded[2..]);
        assert!(preflight_jpeg(&[], &bloated, &mut metadata_bytes, 24).is_err());
    }

    struct Fixture {
        bytes: Vec<u8>,
        ifds: Vec<usize>,
        // 页内(tag, entry起点, 实际value起点)，供边界样本改写单个字段。
        tags: Vec<Vec<(u16, usize, usize)>>,
        pixels: Vec<Vec<usize>>,
        endian: Endian,
    }
    impl Fixture {
        fn tag(&self, page: usize, tag: u16) -> (usize, usize) {
            let (_, entry, value) = *self.tags[page]
                .iter()
                .find(|(id, _, _)| *id == tag)
                .unwrap();
            (entry, value)
        }
        fn set16(&mut self, position: usize, value: u16) {
            self.bytes[position..position + 2].copy_from_slice(&self.endian.put16(value));
        }
        fn set32(&mut self, position: usize, value: u32) {
            self.bytes[position..position + 4].copy_from_slice(&self.endian.put32(value));
        }
    }

    fn fixture(endian: Endian, pages: &[PageSpec]) -> Fixture {
        let mut result = Fixture {
            bytes: vec![0; 8],
            ifds: Vec::new(),
            tags: Vec::new(),
            pixels: Vec::new(),
            endian,
        };
        result.bytes[..2].copy_from_slice(match endian {
            Endian::Little => b"II",
            Endian::Big => b"MM",
        });
        result.set16(2, 42);
        let mut previous_next = 4;
        for page in pages {
            if result.bytes.len() % 2 != 0 {
                result.bytes.push(0);
            }
            let ifd = result.bytes.len();
            result.set32(previous_next, ifd as u32);
            result.ifds.push(ifd);
            let strip_count = page.height.div_ceil(page.rows_per_strip) as usize;
            let shorts = |values: &[u16]| -> Vec<u8> {
                values
                    .iter()
                    .flat_map(|value| endian.put16(*value))
                    .collect()
            };
            let longs = |values: &[u32]| -> Vec<u8> {
                values
                    .iter()
                    .flat_map(|value| endian.put32(*value))
                    .collect()
            };
            let counts: Vec<_> = (0..strip_count)
                .map(|strip| {
                    let first = strip as u32 * page.rows_per_strip;
                    (page.height - first).min(page.rows_per_strip)
                        * page.width
                        * u32::from(page.samples)
                })
                .collect();
            let mut tags = vec![
                (256, 4, longs(&[page.width])),
                (257, 4, longs(&[page.height])),
                (258, 3, shorts(&vec![page.bits; page.samples as usize])),
                (259, 3, shorts(&[page.compression])),
                (266, 3, shorts(&[page.fill_order])),
                (262, 3, shorts(&[2])),
                (273, 4, longs(&vec![0; strip_count])),
                (274, 3, shorts(&[page.orientation])),
                (277, 3, shorts(&[page.samples])),
                (278, 4, longs(&[page.rows_per_strip])),
                (279, 4, longs(&counts)),
                (284, 3, shorts(&[page.planar])),
                (339, 3, shorts(&vec![1; page.samples as usize])),
            ];
            if let Some(extra) = page.extra {
                tags.push((338, 3, shorts(&[extra])));
            }
            if page.sub_ifd {
                tags.push((330, 4, longs(&[0])));
            }
            tags.sort_by_key(|(tag, _, _)| *tag);
            result.bytes.resize(ifd + 2 + tags.len() * 12 + 4, 0);
            result.set16(ifd, tags.len() as u16);
            let mut locations = Vec::new();
            for (index, (tag, kind, data)) in tags.iter().enumerate() {
                let entry = ifd + 2 + index * 12;
                result.set16(entry, *tag);
                result.set16(entry + 2, *kind);
                result.set32(
                    entry + 4,
                    (data.len() / if *kind == 3 { 2 } else { 4 }) as u32,
                );
                let value = if data.len() <= 4 {
                    entry + 8
                } else {
                    let value = result.bytes.len();
                    result.set32(entry + 8, value as u32);
                    result.bytes.resize(value + data.len(), 0);
                    value
                };
                result.bytes[value..value + data.len()].copy_from_slice(data);
                locations.push((*tag, entry, value));
            }
            let offsets_value = locations.iter().find(|(tag, _, _)| *tag == 273).unwrap().2;
            let mut source = 0;
            let mut pixel_offsets = Vec::new();
            for (strip, count) in counts.iter().enumerate() {
                let offset = result.bytes.len();
                result.set32(offsets_value + strip * 4, offset as u32);
                pixel_offsets.push(offset);
                result
                    .bytes
                    .extend_from_slice(&page.pixels[source..source + *count as usize]);
                source += *count as usize;
            }
            result.tags.push(locations);
            result.pixels.push(pixel_offsets);
            previous_next = ifd + 2 + tags.len() * 12;
        }
        result
    }

    fn expected(spec: &PageSpec) -> Vec<u8> {
        let mut image = if spec.samples == 3 {
            DynamicImage::ImageRgb8(
                RgbImage::from_raw(spec.width, spec.height, spec.pixels.clone()).unwrap(),
            )
        } else {
            DynamicImage::ImageRgba8(
                RgbaImage::from_raw(spec.width, spec.height, spec.pixels.clone()).unwrap(),
            )
        };
        image.apply_orientation(Orientation::from_exif(spec.orientation as u8).unwrap());
        let mut bytes = image.into_rgba8().into_raw();
        for pixel in bytes.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        bytes
    }

    #[test]
    fn image_tiff_two_pages_in_both_byte_orders_preserve_orientation_alpha_and_bgra() {
        let mut first = rgb();
        first.orientation = 6;
        let second = rgba();
        for endian in [Endian::Little, Endian::Big] {
            let fixture = fixture(endian, &[first.clone(), second.clone()]);
            let length = fixture.bytes.len() as u64;
            let mut source = TiffSource::from_reader(Cursor::new(fixture.bytes), &token()).unwrap();
            assert_eq!(source.pages().len(), 2);
            assert!(source.pages().iter().all(|page| page.supported));
            for (index, spec) in [&first, &second].into_iter().enumerate() {
                let decoded = source.decode(index, 256, &token()).unwrap();
                let dimensions = (spec.height, spec.width);
                assert_eq!(decoded.metadata.format, ImageFormat::Tiff);
                assert_eq!(
                    decoded.metadata.encoded_dimensions,
                    Some((spec.width, spec.height))
                );
                assert_eq!(decoded.metadata.display_dimensions, dimensions);
                assert_eq!(
                    decoded.metadata.orientation.map(|value| value.to_exif()),
                    Some(spec.orientation as u8)
                );
                assert_eq!(decoded.metadata.encoded_bytes, Some(length));
                assert_eq!(
                    decoded.metadata.decoded_color,
                    Some(if spec.samples == 3 {
                        ColorType::Rgb8
                    } else {
                        ColorType::Rgba8
                    })
                );
                assert_eq!((decoded.image.width, decoded.image.height), dimensions);
                assert_eq!(decoded.image.bgra, expected(spec));
            }
            assert!(source.decode(2, 256, &token()).is_err());
        }
    }

    #[test]
    fn image_tiff_all_eight_orientations_match_the_normalized_image_contract() {
        for endian in [Endian::Little, Endian::Big] {
            for orientation in 1..=8 {
                let mut page = rgb();
                page.orientation = orientation;
                let bytes = fixture(endian, &[page.clone()]).bytes;
                let mut source = TiffSource::from_reader(Cursor::new(bytes), &token()).unwrap();
                assert_eq!(
                    source.decode(0, 24, &token()).unwrap().image.bgra,
                    expected(&page)
                );
            }
        }
    }

    #[test]
    fn image_tiff_unsupported_pages_remain_listed_and_do_not_poison_valid_selection() {
        let mut compression = rgb();
        compression.compression = 5;
        let mut associated = rgba();
        associated.extra = Some(1);
        let mut planar = rgb();
        planar.planar = 2;
        let mut depth = rgb();
        depth.bits = 16;
        let mut sub_ifd = rgb();
        sub_ifd.sub_ifd = true;
        let mut reverse_bits = rgb();
        reverse_bits.fill_order = 2;
        let good = rgb();
        let bytes = fixture(
            Endian::Little,
            &[
                compression,
                associated,
                planar,
                depth,
                sub_ifd,
                reverse_bits,
                good.clone(),
            ],
        )
        .bytes;
        let mut source = TiffSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(source.pages().len(), 7);
        for index in 0..6 {
            assert!(!source.pages()[index].supported);
            assert!(source.pages()[index].error.is_some());
            assert!(source.decode(index, 256, &token()).is_err());
        }
        assert_eq!(
            source.decode(6, 24, &token()).unwrap().image.bgra,
            expected(&good)
        );
    }

    #[test]
    fn image_tiff_ifd_cycles_ranges_counts_and_external_metadata_are_bounded() {
        let mut cycle = fixture(Endian::Little, &[rgb()]);
        let ifd = cycle.ifds[0];
        let count = cycle.endian.u16(&cycle.bytes[ifd..]) as usize;
        cycle.set32(ifd + 2 + count * 12, ifd as u32);
        assert!(TiffSource::from_reader(Cursor::new(cycle.bytes), &token()).is_err());
        let mut range = fixture(Endian::Little, &[rgb()]);
        range.set32(4, u32::MAX);
        assert!(TiffSource::from_reader(Cursor::new(range.bytes), &token()).is_err());
        let mut tags = fixture(Endian::Little, &[rgb()]);
        tags.set16(tags.ifds[0], 257);
        assert!(TiffSource::from_reader(Cursor::new(tags.bytes), &token()).is_err());
        let many = fixture(Endian::Little, &vec![rgb(); 129]);
        assert!(TiffSource::from_reader(Cursor::new(many.bytes), &token()).is_err());
        let mut metadata = fixture(Endian::Little, &[rgb()]);
        let (entry, _) = metadata.tag(0, 258);
        metadata.set32(entry + 4, 524_289);
        assert!(TiffSource::from_reader(Cursor::new(metadata.bytes), &token()).is_err());
        let mut big = fixture(Endian::Little, &[rgb()]);
        big.set16(2, 43);
        assert!(TiffSource::from_reader(Cursor::new(big.bytes), &token()).is_err());
    }

    #[test]
    fn image_tiff_strips_require_exact_rows_counts_and_ranges_but_other_pages_work() {
        let good = rgb();
        let mut data = fixture(Endian::Little, &vec![good.clone(); 5]);
        let (_, count) = data.tag(0, 279);
        data.set32(count, 1);
        let (_, offset) = data.tag(1, 273);
        data.set32(offset, u32::MAX);
        let (_, rows) = data.tag(2, 278);
        data.set32(rows, 0);
        let (offset_entry, _) = data.tag(3, 273);
        data.set32(offset_entry + 4, 1);
        let mut source = TiffSource::from_reader(Cursor::new(data.bytes), &token()).unwrap();
        for index in 0..4 {
            assert!(source.decode(index, 24, &token()).is_err());
        }
        assert_eq!(
            source.decode(4, 24, &token()).unwrap().image.bgra,
            expected(&good)
        );
    }

    struct MutableReader {
        bytes: Arc<Mutex<Vec<u8>>>,
        position: u64,
        reads: Arc<AtomicUsize>,
        cancel_on_read: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
    }
    impl Read for MutableReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let bytes = self.bytes.lock().unwrap();
            let position = (self.position as usize).min(bytes.len());
            let count = output.len().min(bytes.len() - position);
            output[..count].copy_from_slice(&bytes[position..position + count]);
            self.position += count as u64;
            if self.cancel_on_read.swap(false, Ordering::AcqRel) {
                self.cancel.store(true, Ordering::Release);
            }
            Ok(count)
        }
    }
    impl Seek for MutableReader {
        fn seek(&mut self, request: SeekFrom) -> io::Result<u64> {
            let target = match request {
                SeekFrom::Start(value) => i128::from(value),
                SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
                SeekFrom::End(delta) => {
                    self.bytes.lock().unwrap().len() as i128 + i128::from(delta)
                }
            };
            if target < 0 || target > u64::MAX as i128 {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            self.position = target as u64;
            Ok(self.position)
        }
    }
    fn mutable(bytes: Vec<u8>, cancel: Arc<AtomicBool>) -> MutableReader {
        MutableReader {
            bytes: Arc::new(Mutex::new(bytes)),
            position: 0,
            reads: Arc::new(AtomicUsize::new(0)),
            cancel_on_read: Arc::new(AtomicBool::new(false)),
            cancel,
        }
    }

    #[test]
    fn image_tiff_frozen_metadata_and_output_budget_do_not_reparse_mutable_source() {
        let good = rgb();
        let mut fixture = fixture(Endian::Little, &[good.clone()]);
        let (_, width) = fixture.tag(0, 256);
        let (_, offset) = fixture.tag(0, 273);
        let pixel = fixture.pixels[0][0];
        let length = fixture.bytes.len();
        let reader = mutable(fixture.bytes.clone(), Arc::new(token()));
        let bytes = reader.bytes.clone();
        let reads = reader.reads.clone();
        let mut source = TiffSource::from_reader(reader, &token()).unwrap();
        fixture.set32(width, u32::MAX);
        fixture.set32(offset, u32::MAX);
        fixture.bytes[pixel..pixel + 3].copy_from_slice(&[10, 20, 30]);
        fixture.bytes.extend_from_slice(&[0; 16]);
        *bytes.lock().unwrap() = fixture.bytes;
        reads.store(0, Ordering::Release);
        assert!(source.decode(0, 23, &token()).is_err());
        assert_eq!(reads.load(Ordering::Acquire), 0);
        let decoded = source.decode(0, 24, &token()).unwrap();
        assert_eq!(decoded.metadata.encoded_bytes, Some(length as u64));
        assert_eq!(decoded.metadata.encoded_dimensions, Some((3, 2)));
        assert_eq!(&decoded.image.bgra[..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn image_tiff_prefix_cancel_recovery_and_input_size_preserve_fixed_view() {
        let good = rgb();
        let data = fixture(Endian::Big, &[good.clone()]);
        let mut prefixed = vec![42; 11];
        prefixed.extend_from_slice(&data.bytes);
        let mut cursor = Cursor::new(prefixed);
        cursor.set_position(11);
        let mut source = TiffSource::from_reader(cursor, &token()).unwrap();
        assert_eq!(
            source.decode(0, 24, &token()).unwrap().image.bgra,
            expected(&good)
        );
        let cancel = Arc::new(token());
        let reader = mutable(data.bytes, cancel.clone());
        let armed = reader.cancel_on_read.clone();
        let mut source = TiffSource::from_reader(reader, &cancel).unwrap();
        armed.store(true, Ordering::Release);
        assert!(source.decode(0, 24, &cancel).is_err());
        assert_eq!(cancelled(&cancel).unwrap_err().kind(), io::ErrorKind::Other);
        cancel.store(false, Ordering::Release);
        assert_eq!(
            source.decode(0, 24, &cancel).unwrap().image.bgra,
            expected(&good)
        );
        struct Large(Cursor<Vec<u8>>);
        impl Read for Large {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.0.read(output)
            }
        }
        impl Seek for Large {
            fn seek(&mut self, request: SeekFrom) -> io::Result<u64> {
                if matches!(request, SeekFrom::End(0)) {
                    Ok(crate::image_decode::MAX_DECODE_BYTES + 1)
                } else {
                    self.0.seek(request)
                }
            }
        }
        assert!(TiffSource::from_reader(Large(Cursor::new(vec![])), &token()).is_err());
    }
}
