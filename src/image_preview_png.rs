//! 静态非交错 PNG 的行采样预览；预览像素不授予完整精度的编辑或导出资格。
use crate::{
    capture::Image,
    image_container_metadata::{SegmentedView, Summary},
    image_preview::PreviewGeometry,
    Result,
};
use std::{
    io::{self, BufReader, Read, Seek, SeekFrom},
    sync::atomic::{AtomicBool, Ordering},
};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_BYTES: u64 = crate::image_decode::MAX_DECODE_BYTES;
const MAX_PIXELS: u64 = crate::image_decode::MAX_DECODE_PIXELS;
const MAX_SIDE: u32 = 65_535;
const METADATA_WORKSPACE: u64 = 16 * 1024 * 1024;
const INPUT_BUFFER: usize = 8 * 1024;
const PRIVATE_FIXED: u64 = 1024 * 1024;

/// 只限制采样栅格；编码图可以超出完整解码的尺寸上限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewLimits {
    pub max_pixels: u64,
    pub max_side: u32,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_pixels: 4_000_000,
            max_side: 2048,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewSampling {
    PixelCenterNearest,
}

/// ICC和编码位深仍记录在source中；不能将这个8位副本声明为已转sRGB。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewColorStatus {
    NotColorManaged,
}

pub(crate) struct PreviewDecoded {
    pub image: Image,
    pub geometry: PreviewGeometry,
    pub source: Summary,
    pub sampling: PreviewSampling,
    pub color: PreviewColorStatus,
}

/// 两阶段额外工作区，不含宿主R和冻结输入实际预留；不代表进程RSS上限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewWorkspace {
    pub metadata_bytes: u64,
    /// 包含保留的分段视图、PNG私有缓冲、单行和唯一预览输出。
    pub decode_bytes: u64,
}

impl PreviewWorkspace {
    pub(crate) fn peak_bytes(self) -> u64 {
        self.metadata_bytes.max(self.decode_bytes)
    }
}

/// 来源借用在解码完成前保持冻结；不重新打开路径，也不复制完整压缩数据。
pub(crate) struct PreviewPlan<'a> {
    view: SegmentedView<'a>,
    header: Header,
    source: Summary,
    geometry: PreviewGeometry,
    workspace: PreviewWorkspace,
    source_reserved_bytes: u64,
    external_resident_bytes: u64,
}

impl PreviewPlan<'_> {
    pub(crate) fn geometry(&self) -> PreviewGeometry {
        self.geometry
    }

    pub(crate) fn workspace(&self) -> PreviewWorkspace {
        self.workspace
    }

    pub(crate) fn source(&self) -> Summary {
        self.source
    }
}

#[derive(Clone, Copy)]
struct Header {
    dimensions: (u32, u32),
    depth: u8,
    color: u8,
    transparency: bool,
    raw_row: u64,
    expanded_row: usize,
}

/// 可用于小图的确定性测试；宿主仅在完整解码超预算后选择这个独立预览入口。
pub(crate) fn inspect<'a>(
    bytes: &'a [u8],
    source_reserved_bytes: u64,
    external_resident_bytes: u64,
    limits: PreviewLimits,
    cancel: &AtomicBool,
) -> Result<PreviewPlan<'a>> {
    check_cancel(cancel)?;
    let result = inspect_inner(
        bytes,
        source_reserved_bytes,
        external_resident_bytes,
        limits,
        cancel,
    );
    check_cancel(cancel)?;
    result
}

fn inspect_inner<'a>(
    bytes: &'a [u8],
    source_reserved_bytes: u64,
    external_resident_bytes: u64,
    limits: PreviewLimits,
    cancel: &AtomicBool,
) -> Result<PreviewPlan<'a>> {
    if source_reserved_bytes < bytes.len() as u64 {
        return Err(budget());
    }
    ledger(external_resident_bytes, source_reserved_bytes, 0)?;
    let header = scan(bytes, cancel)?;
    let sampled = sampled_dimensions(header.dimensions, limits)?;
    let output = pixel_bytes(sampled)?;
    let private = private_workspace(header.raw_row)?;
    let base_decode = add(&[
        private,
        INPUT_BUFFER as u64,
        header.expanded_row as u64,
        output,
    ])?;
    // 在任何元数据/后端分配前同时核两阶段；元数据和解码峰不叠加。
    ledger(
        external_resident_bytes,
        source_reserved_bytes,
        METADATA_WORKSPACE.max(base_decode),
    )?;
    let inspected = crate::image_container_metadata::inspect_cancellable(bytes, cancel)?;
    check_cancel(cancel)?;
    let crate::image_container_metadata::InspectedImage {
        summary: source,
        icc_profile,
        supported_exif,
        view,
    } = inspected;
    // 此叶不色管、不导出元数据；解码阶段不保留ICC/EXIF的拥有副本。
    drop(icc_profile);
    drop(supported_exif);
    let geometry = PreviewGeometry::new(header.dimensions, sampled, source.orientation)?;
    let decode_bytes = add(&[base_decode, view.retained_bytes()?])?;
    let workspace = PreviewWorkspace {
        metadata_bytes: METADATA_WORKSPACE,
        decode_bytes,
    };
    ledger(
        external_resident_bytes,
        source_reserved_bytes,
        workspace.peak_bytes(),
    )?;
    Ok(PreviewPlan {
        view,
        header,
        source,
        geometry,
        workspace,
        source_reserved_bytes,
        external_resident_bytes,
    })
}

/// 每个源行都要解压；取消在输入分段、行和采样循环边界生效，非随机行读取。
pub(crate) fn decode(plan: PreviewPlan<'_>, cancel: &AtomicBool) -> Result<PreviewDecoded> {
    check_cancel(cancel)?;
    let result = decode_inner(plan, cancel);
    check_cancel(cancel)?;
    result
}

fn decode_inner(plan: PreviewPlan<'_>, cancel: &AtomicBool) -> Result<PreviewDecoded> {
    let PreviewPlan {
        view,
        header,
        source,
        geometry,
        workspace,
        source_reserved_bytes,
        external_resident_bytes,
    } = plan;
    ledger(
        external_resident_bytes,
        source_reserved_bytes,
        workspace.decode_bytes,
    )?;
    let input = BufReader::with_capacity(
        INPUT_BUFFER,
        CancelReader {
            inner: view,
            cancel,
        },
    );
    let mut decoder = png::Decoder::new(input);
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    decoder.ignore_checksums(false);
    // Limits只作为后端局部拒绝门；总账由已核的行工作区模型承担。
    decoder.set_limits(png::Limits {
        bytes: usize::try_from(private_workspace(header.raw_row)?)?,
    });
    let mut reader = decoder.read_info()?;
    check_cancel(cancel)?;
    if reader.info().size() != header.dimensions || reader.info().interlaced {
        return Err(invalid());
    }
    let (color, depth) = reader.output_color_type();
    let expected_color = expanded_color(header);
    let expected_depth = if header.depth == 16 {
        png::BitDepth::Sixteen
    } else {
        png::BitDepth::Eight
    };
    let line_size = reader
        .output_line_size(header.dimensions.0)
        .ok_or_else(budget)?;
    if color != expected_color || depth != expected_depth || line_size != header.expanded_row {
        return Err(invalid());
    }
    let mut row = slot(header.expanded_row, cancel)?;
    let output_bytes = usize::try_from(pixel_bytes(geometry.raster_dimensions())?)?;
    let mut bgra = slot(output_bytes, cancel)?;
    let sampled = geometry.sampled_encoded_dimensions();
    let mut target_y = 0;
    let mut selected_y = center(0, header.dimensions.1, sampled.1);
    for source_y in 0..header.dimensions.1 {
        check_cancel(cancel)?;
        if reader.read_row(&mut row)?.is_none() {
            return Err(invalid());
        }
        check_cancel(cancel)?;
        if target_y == sampled.1 || source_y != selected_y {
            continue;
        }
        for target_x in 0..sampled.0 {
            if target_x % 1024 == 0 {
                check_cancel(cancel)?;
            }
            let source_x = center(target_x, header.dimensions.0, sampled.0);
            let pixel = sample(&row, source_x, color, depth)?;
            let (x, y) =
                oriented_pixel((target_x, target_y), sampled, geometry.source_orientation());
            let offset = (u64::from(y) * u64::from(geometry.raster_dimensions().0) + u64::from(x))
                .checked_mul(4)
                .ok_or_else(budget)?;
            let start = usize::try_from(offset)?;
            bgra.get_mut(start..start + 4)
                .ok_or_else(invalid)?
                .copy_from_slice(&pixel);
        }
        target_y += 1;
        if target_y < sampled.1 {
            selected_y = center(target_y, header.dimensions.1, sampled.1);
        }
    }
    if target_y != sampled.1 || reader.read_row(&mut row)?.is_some() {
        return Err(invalid());
    }
    reader.finish()?;
    check_cancel(cancel)?;
    // 行/后端/分段视图在交付前释放；唯一拥有的像素数组就是预览BGRA。
    drop(reader);
    drop(row);
    let (width, height) = geometry.raster_dimensions();
    Ok(PreviewDecoded {
        image: Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        },
        geometry,
        source,
        sampling: PreviewSampling::PixelCenterNearest,
        color: PreviewColorStatus::NotColorManaged,
    })
}

fn scan(bytes: &[u8], cancel: &AtomicBool) -> Result<Header> {
    check_cancel(cancel)?;
    if bytes.get(..8) != Some(SIGNATURE.as_slice()) {
        return Err(invalid());
    }
    let mut offset = 8usize;
    let mut header = None;
    let mut transparency = false;
    let mut idat = false;
    let mut count = 0usize;
    while offset < bytes.len() {
        check_cancel(cancel)?;
        let prefix = bytes
            .get(offset..offset.checked_add(8).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let length = u32::from_be_bytes(prefix[..4].try_into()?) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid)?;
        let kind = &prefix[4..8];
        if kind != b"IDAT" {
            count += 1;
            if count > 4096 {
                return Err(unsupported(
                    "PNG预览的块目录过大",
                    "PNG preview chunk directory is too large",
                ));
            }
        }
        if offset == 8 && (kind != b"IHDR" || length != 13) {
            return Err(invalid());
        }
        match kind {
            b"IHDR" => {
                if header.is_some() || length != 13 {
                    return Err(invalid());
                }
                let data = &bytes[offset + 8..end - 4];
                let width = u32::from_be_bytes(data[..4].try_into()?);
                let height = u32::from_be_bytes(data[4..8].try_into()?);
                let depth = data[8];
                let color = data[9];
                let valid_depth = match color {
                    0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                    2 | 4 | 6 => matches!(depth, 8 | 16),
                    3 => matches!(depth, 1 | 2 | 4 | 8),
                    _ => false,
                };
                if width == 0
                    || height == 0
                    || width > i32::MAX as u32
                    || height > i32::MAX as u32
                    || !valid_depth
                    || data[10] != 0
                    || data[11] != 0
                    || data[12] > 1
                {
                    return Err(invalid());
                }
                if data[12] != 0 {
                    return Err(unsupported(
                        "暂不支持交错PNG的行预览",
                        "Interlaced PNG row previews are not supported yet",
                    ));
                }
                header = Some(((width, height), depth, color));
            }
            b"acTL" | b"fcTL" | b"fdAT" => {
                return Err(unsupported(
                    "动画PNG不使用静态行预览",
                    "Animated PNG does not use static row previews",
                ));
            }
            b"tRNS" => transparency = true,
            b"IDAT" => idat = true,
            b"IEND" => {
                if length != 0 || !idat || end != bytes.len() {
                    return Err(invalid());
                }
                let (dimensions, depth, color) = header.ok_or_else(invalid)?;
                let raw_channels = match color {
                    0 | 3 => 1u64,
                    2 => 3,
                    4 => 2,
                    6 => 4,
                    _ => return Err(invalid()),
                };
                let row_bits = u64::from(dimensions.0)
                    .checked_mul(raw_channels)
                    .and_then(|n| n.checked_mul(u64::from(depth)))
                    .ok_or_else(budget)?;
                let raw_row = row_bits
                    .checked_add(7)
                    .and_then(|n| (n / 8).checked_add(1))
                    .ok_or_else(budget)?;
                let mut result = Header {
                    dimensions,
                    depth,
                    color,
                    transparency,
                    raw_row,
                    expanded_row: 0,
                };
                let channels = expanded_color(result).samples() as u64;
                result.expanded_row = usize::try_from(
                    u64::from(dimensions.0)
                        .checked_mul(channels)
                        .and_then(|n| n.checked_mul(if depth == 16 { 2 } else { 1 }))
                        .ok_or_else(budget)?,
                )?;
                return Ok(result);
            }
            _ => {}
        }
        offset = end;
    }
    Err(invalid())
}

fn sampled_dimensions(source: (u32, u32), limits: PreviewLimits) -> Result<(u32, u32)> {
    if limits.max_pixels == 0
        || limits.max_pixels > MAX_PIXELS
        || limits.max_side == 0
        || limits.max_side > MAX_SIDE
    {
        return Err(budget());
    }
    let dimensions = |long: u32| {
        if source.0 >= source.1 {
            (
                long,
                ((u64::from(source.1) * u64::from(long) / u64::from(source.0)) as u32).max(1),
            )
        } else {
            (
                ((u64::from(source.0) * u64::from(long) / u64::from(source.1)) as u32).max(1),
                long,
            )
        }
    };
    let mut low = 1;
    let mut high = source.0.max(source.1).min(limits.max_side);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let d = dimensions(middle);
        if u64::from(d.0) * u64::from(d.1) <= limits.max_pixels {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(dimensions(low))
}

fn private_workspace(raw_row: u64) -> Result<u64> {
    // data_stream的resize目标≤6r+256KiB。amortized增长时old<目标、new≤2目标，
    // 三倍覆盖旧新共存；另6r覆盖两份prev-row scratch及增长。非交错read_row
    // 不创建Reader整行scratch。固定项覆盖两张≤15bit次级Huffman表、主表与小块。
    let window = raw_row
        .checked_mul(6)
        .and_then(|n| n.checked_add(256 * 1024))
        .ok_or_else(budget)?;
    add(&[
        window.checked_mul(3).ok_or_else(budget)?,
        raw_row.checked_mul(6).ok_or_else(budget)?,
        PRIVATE_FIXED,
    ])
}

fn expanded_color(header: Header) -> png::ColorType {
    match (header.color, header.transparency) {
        (0, false) => png::ColorType::Grayscale,
        (0, true) | (4, _) => png::ColorType::GrayscaleAlpha,
        (2 | 3, false) => png::ColorType::Rgb,
        _ => png::ColorType::Rgba,
    }
}

fn sample(row: &[u8], x: u32, color: png::ColorType, depth: png::BitDepth) -> Result<[u8; 4]> {
    let channels = color.samples();
    let stride = channels
        * if depth == png::BitDepth::Sixteen {
            2
        } else {
            1
        };
    let start = (x as usize).checked_mul(stride).ok_or_else(budget)?;
    let p = row.get(start..start + stride).ok_or_else(invalid)?;
    let channel = |i: usize| {
        if depth == png::BitDepth::Sixteen {
            let value = u16::from_be_bytes([p[2 * i], p[2 * i + 1]]);
            ((u32::from(value) * 255 + 32_767) / 65_535) as u8
        } else {
            p[i]
        }
    };
    Ok(match color {
        png::ColorType::Grayscale => [channel(0), channel(0), channel(0), 255],
        png::ColorType::GrayscaleAlpha => [channel(0), channel(0), channel(0), channel(1)],
        png::ColorType::Rgb => [channel(2), channel(1), channel(0), 255],
        png::ColorType::Rgba => [channel(2), channel(1), channel(0), channel(3)],
        png::ColorType::Indexed => return Err(invalid()),
    })
}

/// 与PreviewGeometry相同的整数像素中心采样，不把边界坐标当像素索引。
fn center(index: u32, source: u32, sampled: u32) -> u32 {
    ((u64::from(index) * 2 + 1) * u64::from(source) / (2 * u64::from(sampled))) as u32
}

fn oriented_pixel(
    (x, y): (u32, u32),
    (width, height): (u32, u32),
    orientation: Option<image::metadata::Orientation>,
) -> (u32, u32) {
    match orientation.map_or(1, |o| o.to_exif()) {
        2 => (width - 1 - x, y),
        3 => (width - 1 - x, height - 1 - y),
        4 => (x, height - 1 - y),
        5 => (y, x),
        6 => (height - 1 - y, x),
        7 => (height - 1 - y, width - 1 - x),
        8 => (y, width - 1 - x),
        _ => (x, y),
    }
}

fn slot(length: usize, cancel: &AtomicBool) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    let mut bytes = Vec::new();
    let reserved = bytes.try_reserve_exact(length);
    check_cancel(cancel)?;
    reserved.map_err(|_| budget())?;
    // 不把try_reserve_exact的请求值冒充实际Vec容量；超出已批准slot不resize。
    if bytes.capacity() > length {
        return Err(budget());
    }
    bytes.resize(length, 0);
    check_cancel(cancel)?;
    Ok(bytes)
}

fn pixel_bytes(dimensions: (u32, u32)) -> Result<u64> {
    u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(budget)
}

fn add(parts: &[u64]) -> Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |sum, part| sum.checked_add(*part))
        .ok_or_else(budget)
}

fn ledger(external: u64, source: u64, additional: u64) -> Result<()> {
    if add(&[external, source, additional])? > MAX_BYTES {
        Err(budget())
    } else {
        Ok(())
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(crate::tr!("PNG预览已取消", "PNG preview canceled").into())
    } else {
        Ok(())
    }
}

fn budget() -> Box<dyn std::error::Error> {
    crate::tr!(
        "PNG预览工作区超过内存预算",
        "PNG preview exceeds the memory budget"
    )
    .into()
}

fn invalid() -> Box<dyn std::error::Error> {
    crate::tr!(
        "PNG预览来源结构无效或已截断",
        "Invalid or truncated PNG preview source"
    )
    .into()
}

fn unsupported(zh: &'static str, en: &'static str) -> Box<dyn std::error::Error> {
    crate::i18n::tr(zh, en).into()
}

struct CancelReader<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R> CancelReader<'_, R> {
    fn checkpoint(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(io::Error::other(crate::tr!(
                "PNG预览已取消",
                "PNG preview canceled"
            )))
        } else {
            Ok(())
        }
    }
}

impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.checkpoint()?;
        let result = self.inner.read(buffer);
        self.checkpoint()?;
        result
    }
}

impl<R: Seek> Seek for CancelReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.checkpoint()?;
        let result = self.inner.seek(position);
        self.checkpoint()?;
        result
    }
}

#[path = "image_preview_png_region.rs"]
pub(crate) mod region;

#[cfg(test)]
#[path = "image_preview_png_tests.rs"]
mod tests;
