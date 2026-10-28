//! 有界容器元数据读取与借用解码视图；不把已剥除的配置再次交给像素解码器。
use image::metadata::Orientation;
use std::{
    borrow::Cow,
    cell::Cell,
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_ICC_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXIF_BYTES: usize = 256 * 1024;
const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PARTS: usize = 4096;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const ICC_PREFIX: &[u8; 12] = b"ICC_PROFILE\0";
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);
type CancelCheck<'a> = &'a dyn Fn() -> Result<(), String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetadataStatus {
    NotInspected,
    Absent,
    Present,
    TooLarge,
    Malformed,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FieldSummary {
    pub status: MetadataStatus,
    /// 容器中对应元数据的负载字节数，不含容器的长度、类型与CRC字段。
    pub encoded_bytes: u64,
    /// 解包后确知的字节数；膨胀被提前停止时为None。
    pub decoded_bytes: Option<u64>,
}

impl FieldSummary {
    const fn new(status: MetadataStatus) -> Self {
        Self {
            status,
            encoded_bytes: 0,
            decoded_bytes: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorModel {
    Bgr,
    Rgb,
    Gray,
    Cmyk,
    Palette,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub icc: FieldSummary,
    pub exif: FieldSummary,
    /// 编码通道模型的证据，不根据已经转换后的Rgb8倒推源模型。
    pub color_model: ColorModel,
    pub bits_per_channel: Option<u8>,
    /// 来自已冻结EXIF首IFD；调用者应用一次，视图中不再包含原EXIF。
    pub orientation: Option<Orientation>,
}

impl Summary {
    fn new(status: MetadataStatus) -> Self {
        Self {
            icc: FieldSummary::new(status),
            exif: FieldSummary::new(status),
            color_model: ColorModel::Unknown,
            bits_per_channel: None,
            orientation: None,
        }
    }
}

pub(crate) struct InspectedImage<'a> {
    pub summary: Summary,
    /// 仅是提取出的配置；Present不代表ICC结构有效或已做颜色变换。
    pub icc_profile: Option<Vec<u8>>,
    /// 已冻结的白名单字段；解析不可用与严格方向错误、取消分别处理。
    pub supported_exif: Option<Result<crate::image_exif::SupportedExif, String>>,
    pub view: SegmentedView<'a>,
}

struct Segment<'a> {
    start: u64,
    data: Cow<'a, [u8]>,
}

/// 仅拥有少量替换头与段目录；像素压缩数据始终借用原输入。
pub(crate) struct SegmentedView<'a> {
    segments: Vec<Segment<'a>>,
    length: u64,
    position: u64,
}

impl<'a> SegmentedView<'a> {
    fn borrowed(bytes: &'a [u8]) -> Self {
        Self {
            segments: vec![Segment {
                start: 0,
                data: Cow::Borrowed(bytes),
            }],
            length: bytes.len() as u64,
            position: 0,
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.length
    }

    /// 仅计分段目录与自有替换头；借用的源压缩字节由调用方另计。
    pub(crate) fn retained_bytes(&self) -> Result<u64, String> {
        let allocation = || {
            error(
                "图片分段驻留量超过预算",
                "Image view storage exceeds the budget",
            )
        };
        let mut bytes = self
            .segments
            .capacity()
            .checked_mul(std::mem::size_of::<Segment<'_>>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
            .ok_or_else(allocation)?;
        for segment in &self.segments {
            if let Cow::Owned(data) = &segment.data {
                bytes = bytes
                    .checked_add(u64::try_from(data.capacity()).map_err(|_| allocation())?)
                    .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
                    .ok_or_else(allocation)?;
            }
        }
        Ok(bytes)
    }
}

impl Read for SegmentedView<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.length {
            return Ok(0);
        }
        let mut written = 0;
        while written < output.len() && self.position < self.length {
            let index = self
                .segments
                .partition_point(|segment| segment.start <= self.position)
                - 1;
            let segment = &self.segments[index];
            let offset = (self.position - segment.start) as usize;
            let count = (segment.data.len() - offset).min(output.len() - written);
            output[written..written + count].copy_from_slice(&segment.data[offset..offset + count]);
            written += count;
            self.position += count as u64;
        }
        Ok(written)
    }
}

impl Seek for SegmentedView<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(offset) => i128::from(self.length) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
        };
        self.position = u64::try_from(position).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                error("图片分段定位无效", "Invalid image view seek"),
            )
        })?;
        Ok(self.position)
    }
}

struct Edit {
    range: Range<usize>,
    replacement: Option<Vec<u8>>,
}

impl Edit {
    fn remove(range: Range<usize>) -> Self {
        Self {
            range,
            replacement: None,
        }
    }

    fn replace(range: Range<usize>, replacement: Vec<u8>) -> Self {
        Self {
            range,
            replacement: Some(replacement),
        }
    }
}

#[cfg(test)]
fn edited_view(bytes: &[u8], edits: Vec<Edit>) -> Result<SegmentedView<'_>, String> {
    edited_view_checked(bytes, edits, &|| Ok(()))
}

fn edited_view_checked<'a>(
    bytes: &'a [u8],
    mut edits: Vec<Edit>,
    cancel: CancelCheck<'_>,
) -> Result<SegmentedView<'a>, String> {
    cancel()?;
    if edits.is_empty() {
        return Ok(SegmentedView::borrowed(bytes));
    }
    edits.sort_unstable_by_key(|edit| edit.range.start);
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(edits.len() * 2 + 1)
        .map_err(|_| error("图片分段目录分配失败", "Image segment allocation failed"))?;
    let mut previous = 0;
    let mut length = 0u64;
    for edit in edits {
        cancel()?;
        if edit.range.start < previous
            || edit.range.start > edit.range.end
            || edit.range.end > bytes.len()
        {
            return Err(error("图片分段范围无效", "Invalid image segment range"));
        }
        if previous < edit.range.start {
            let slice = &bytes[previous..edit.range.start];
            segments.push(Segment {
                start: length,
                data: Cow::Borrowed(slice),
            });
            length += slice.len() as u64;
        }
        if let Some(replacement) = edit.replacement.filter(|bytes| !bytes.is_empty()) {
            length += replacement.len() as u64;
            segments.push(Segment {
                start: length - replacement.len() as u64,
                data: Cow::Owned(replacement),
            });
        }
        previous = edit.range.end;
    }
    if previous < bytes.len() {
        segments.push(Segment {
            start: length,
            data: Cow::Borrowed(&bytes[previous..]),
        });
        length += (bytes.len() - previous) as u64;
    }
    Ok(SegmentedView {
        segments,
        length,
        position: 0,
    })
}

fn error(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn malformed() -> String {
    error(
        "图片容器元数据范围无效",
        "Invalid image container metadata range",
    )
}

fn metadata_budget(total: &mut u64, bytes: usize) -> Result<(), String> {
    *total = total.checked_add(bytes as u64).ok_or_else(malformed)?;
    if *total > MAX_METADATA_BYTES {
        return Err(error(
            "图片元数据超过8MiB上限",
            "Image metadata exceeds the 8 MiB limit",
        ));
    }
    Ok(())
}

fn count_part(count: &mut usize) -> Result<(), String> {
    *count += 1;
    if *count > MAX_PARTS {
        return Err(error(
            "图片容器项目超过4096项上限",
            "Image container exceeds the 4096-part limit",
        ));
    }
    Ok(())
}

fn copy_profile(bytes: &[u8], cancel: CancelCheck<'_>) -> Result<Vec<u8>, String> {
    cancel()?;
    let mut result = Vec::new();
    result
        .try_reserve_exact(bytes.len())
        .map_err(|_| error("ICC配置分配失败", "ICC profile allocation failed"))?;
    for part in bytes.chunks(64 * 1024) {
        cancel()?;
        result.extend_from_slice(part);
    }
    Ok(result)
}

/// 这里只限制探测缓冲；不把它声称为后端或系统颜色变换的总峰值上限。
pub(crate) fn inspect(bytes: &[u8]) -> Result<InspectedImage<'_>, String> {
    inspect_cancellable(bytes, &NEVER_CANCELLED)
}

/// 扫描、校验和有界解压在分段边界响应取消，失败时不返回可发布的解码视图。
pub(crate) fn inspect_cancellable<'a>(
    bytes: &'a [u8],
    cancel: &AtomicBool,
) -> Result<InspectedImage<'a>, String> {
    inspect_checked(bytes, &|| cancellation(cancel))
}

fn cancellation(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(error(
            "图片元数据读取已取消",
            "Image metadata reading canceled",
        ))
    } else {
        Ok(())
    }
}

fn inspect_checked<'a>(
    bytes: &'a [u8],
    cancel: CancelCheck<'_>,
) -> Result<InspectedImage<'a>, String> {
    cancel()?;
    if bytes.len() as u64 > crate::image_decode::MAX_DECODE_BYTES {
        return Err(error(
            "图片文件超过256MiB上限",
            "Image file exceeds the 256 MiB input limit",
        ));
    }
    if bytes.starts_with(PNG_SIGNATURE) {
        inspect_png(bytes, cancel)
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        inspect_jpeg(bytes, cancel)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        inspect_webp(bytes, cancel)
    } else {
        Ok(InspectedImage {
            summary: Summary::new(MetadataStatus::NotInspected),
            icc_profile: None,
            supported_exif: None,
            view: SegmentedView::borrowed(bytes),
        })
    }
}

fn exif(
    summary: &mut Summary,
    payload: &[u8],
    cancel: CancelCheck<'_>,
) -> Result<Result<crate::image_exif::SupportedExif, String>, String> {
    cancel()?;
    if summary.exif.status != MetadataStatus::Absent {
        return Err(error("EXIF元数据重复", "Duplicate EXIF metadata"));
    }
    if payload.len() > MAX_EXIF_BYTES {
        return Err(error(
            "EXIF超过256KiB上限",
            "EXIF exceeds the 256 KiB limit",
        ));
    }
    let tiff = payload.strip_prefix(b"Exif\0\0").unwrap_or(payload);
    summary.orientation = exif_orientation_checked(tiff, cancel)?;
    summary.exif = FieldSummary {
        status: MetadataStatus::Present,
        encoded_bytes: payload.len() as u64,
        decoded_bytes: Some(tiff.len() as u64),
    };
    // 白名单解析错误不影响已经确认的方向；回调观察到的取消仍使整次检查失败。
    let observed_cancel = Cell::new(false);
    let checked = || {
        let result = cancel();
        if result.is_err() {
            observed_cancel.set(true);
        }
        result
    };
    let fields = crate::image_exif::inspect_checked(tiff, &checked);
    if observed_cancel.get() {
        return Err(error(
            "图片元数据读取已取消",
            "Image metadata reading canceled",
        ));
    }
    cancel()?;
    Ok(fields)
}

/// 复用严格首IFD方向检查；输入直接从TIFF头开始，不负责容器包装或元数据保留。
pub(crate) fn exif_orientation_cancellable(
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<Option<Orientation>, String> {
    let result = exif_orientation_checked(bytes, &|| cancellation(cancel));
    cancellation(cancel)?;
    result
}

/// 仅读取首IFD及它直接声明的数据范围，不遍历缩略图、子IFD或厂商私有结构。
#[cfg(test)]
fn exif_orientation(bytes: &[u8]) -> Result<Option<Orientation>, String> {
    exif_orientation_checked(bytes, &|| Ok(()))
}

fn exif_orientation_checked(
    bytes: &[u8],
    cancel: CancelCheck<'_>,
) -> Result<Option<Orientation>, String> {
    cancel()?;
    let bad = || error("EXIF首目录无效", "Invalid first EXIF directory");
    if bytes.len() < 8 {
        return Err(bad());
    }
    let little = match &bytes[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(bad()),
    };
    let u16_at = |offset: usize| -> Result<u16, String> {
        let value: [u8; 2] = bytes
            .get(offset..offset.checked_add(2).ok_or_else(bad)?)
            .ok_or_else(bad)?
            .try_into()
            .unwrap();
        Ok(if little {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        })
    };
    let u32_at = |offset: usize| -> Result<u32, String> {
        let value: [u8; 4] = bytes
            .get(offset..offset.checked_add(4).ok_or_else(bad)?)
            .ok_or_else(bad)?
            .try_into()
            .unwrap();
        Ok(if little {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        })
    };
    if u16_at(2)? != 42 {
        return Err(bad());
    }
    let first = u32_at(4)? as usize;
    if first < 8 {
        return Err(bad());
    }
    let count = usize::from(u16_at(first)?);
    if count > 256 {
        return Err(bad());
    }
    let end = first
        .checked_add(2)
        .and_then(|offset| offset.checked_add(count * 12))
        .and_then(|offset| offset.checked_add(4))
        .filter(|end| *end <= bytes.len())
        .ok_or_else(bad)?;
    let mut orientation = None;
    for offset in (first + 2..end - 4).step_by(12) {
        cancel()?;
        let tag = u16_at(offset)?;
        let kind = u16_at(offset + 2)?;
        let count = u32_at(offset + 4)? as usize;
        let width = match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return Err(bad()),
        };
        let length = count.checked_mul(width).ok_or_else(bad)?;
        if length > 4 {
            let start = u32_at(offset + 8)? as usize;
            if start
                .checked_add(length)
                .is_none_or(|end| end > bytes.len())
            {
                return Err(bad());
            }
        }
        if tag == 274 {
            if orientation.is_some() || kind != 3 || count != 1 {
                return Err(bad());
            }
            orientation = Some(
                u8::try_from(u16_at(offset + 8)?)
                    .ok()
                    .and_then(Orientation::from_exif)
                    .ok_or_else(bad)?,
            );
        }
    }
    Ok(orientation)
}

fn inspect_jpeg<'a>(
    bytes: &'a [u8],
    cancel: CancelCheck<'_>,
) -> Result<InspectedImage<'a>, String> {
    let mut summary = Summary::new(MetadataStatus::Absent);
    let mut supported_exif = None;
    let mut edits = Vec::new();
    let mut icc_parts: Vec<(u8, u8, &[u8])> = Vec::new();
    let mut icc_length = 0usize;
    let mut metadata = 0;
    let mut parts = 0;
    let mut offset = 2;
    let mut entropy = false;
    let mut ended = false;
    let mut seen_sof = false;
    while offset < bytes.len() {
        cancel()?;
        if entropy {
            let mut found = None;
            for part in bytes[offset..].chunks(64 * 1024) {
                cancel()?;
                if let Some(index) = part.iter().position(|byte| *byte == 0xff) {
                    found = Some(offset + index);
                    break;
                }
                offset += part.len();
            }
            offset = found.ok_or_else(malformed)?;
        }
        let start = offset;
        if bytes.get(offset) != Some(&0xff) {
            return Err(malformed());
        }
        while bytes.get(offset) == Some(&0xff) {
            if offset % (64 * 1024) == 0 {
                cancel()?;
            }
            offset += 1;
        }
        let marker = *bytes.get(offset).ok_or_else(malformed)?;
        offset += 1;
        if entropy && marker == 0 {
            continue;
        }
        // 熵流中的restart/TEM不承载元数据，也不增加段目录或分配。
        if marker == 1 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        count_part(&mut parts)?;
        if marker == 0xd9 {
            ended = true;
            break;
        }
        if marker == 0 || marker == 0xd8 {
            return Err(malformed());
        }
        let length = bytes.get(offset..offset + 2).ok_or_else(malformed)?;
        let length = usize::from(u16::from_be_bytes(length.try_into().unwrap()));
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(malformed)?;
        if length < 2 {
            return Err(malformed());
        }
        let data = &bytes[offset + 2..end];
        if (0xe0..=0xef).contains(&marker) || marker == 0xfe {
            metadata_budget(&mut metadata, data.len())?;
        }
        if marker == 0xe1 && data.starts_with(b"Exif") {
            if !data.starts_with(b"Exif\0\0") {
                return Err(error("EXIF标识无效", "Invalid EXIF signature"));
            }
            supported_exif = Some(exif(&mut summary, data, cancel)?);
            edits.push(Edit::remove(start..end));
        }
        if marker == 0xe2 && data.starts_with(b"ICC_PROFILE") {
            summary.icc.encoded_bytes += data.len() as u64;
            edits.push(Edit::remove(start..end));
            if data.len() <= 14 || !data.starts_with(ICC_PREFIX) {
                summary.icc.status = MetadataStatus::Malformed;
            } else {
                icc_length += data.len() - 14;
                icc_parts.push((data[12], data[13], &data[14..]));
            }
        }
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if seen_sof || data.len() < 6 || data.len() != 6 + usize::from(data[5]) * 3 {
                return Err(malformed());
            }
            seen_sof = true;
            summary.bits_per_channel = Some(data[0]);
            summary.color_model = match data[5] {
                1 => ColorModel::Gray,
                3 => ColorModel::Rgb,
                4 => ColorModel::Cmyk,
                _ => ColorModel::Unknown,
            };
        }
        entropy = marker == 0xda || (entropy && marker == 0xdc);
        offset = end;
    }
    if !ended {
        return Err(malformed());
    }
    let mut profile = None;
    if summary.icc.encoded_bytes != 0 {
        summary.icc.decoded_bytes = Some(icc_length as u64);
        if icc_length > MAX_ICC_BYTES {
            summary.icc.status = MetadataStatus::TooLarge;
        } else if summary.icc.status != MetadataStatus::Malformed {
            let count = icc_parts.len();
            let mut ordered = [None; 256];
            let mut valid = (1..=255).contains(&count);
            for (sequence, declared, data) in icc_parts {
                cancel()?;
                if usize::from(declared) != count
                    || sequence == 0
                    || usize::from(sequence) > count
                    || ordered[usize::from(sequence)].is_some()
                {
                    valid = false;
                } else {
                    ordered[usize::from(sequence)] = Some(data);
                }
            }
            if valid {
                let mut output = Vec::new();
                output
                    .try_reserve_exact(icc_length)
                    .map_err(|_| error("ICC配置分配失败", "ICC profile allocation failed"))?;
                for part in &ordered[1..=count] {
                    cancel()?;
                    if let Some(part) = part {
                        output.extend_from_slice(part);
                    } else {
                        valid = false;
                    }
                }
                if valid {
                    summary.icc.status = MetadataStatus::Present;
                    profile = Some(output);
                }
            }
            if !valid {
                summary.icc.status = MetadataStatus::Malformed;
            }
        }
    }
    Ok(InspectedImage {
        summary,
        icc_profile: profile,
        supported_exif,
        view: edited_view_checked(bytes, edits, cancel)?,
    })
}

#[cfg(test)]
fn crc32(bytes: &[u8]) -> u32 {
    crc32_checked(bytes, &|| Ok(())).unwrap()
}

fn crc32_checked(bytes: &[u8], cancel: CancelCheck<'_>) -> Result<u32, String> {
    crc32_parts_checked(&[bytes], cancel)
}

fn crc32_parts_checked(parts: &[&[u8]], cancel: CancelCheck<'_>) -> Result<u32, String> {
    cancel()?;
    let mut crc = !0u32;
    for bytes in parts {
        for part in bytes.chunks(64 * 1024) {
            cancel()?;
            for byte in part {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
                }
            }
        }
    }
    cancel()?;
    Ok(!crc)
}

fn inflate_profile(
    bytes: &[u8],
    cancel: CancelCheck<'_>,
) -> Result<Result<Vec<u8>, MetadataStatus>, String> {
    let mut decoder = flate2::bufread::ZlibDecoder::new(bytes);
    let mut output = Vec::new();
    let mut scratch = [0u8; 32 * 1024];
    loop {
        cancel()?;
        let remaining = MAX_ICC_BYTES + 1 - output.len();
        let read = decoder.read(&mut scratch[..remaining.min(32 * 1024)]);
        cancel()?;
        let count = match read {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => return Ok(Err(MetadataStatus::Malformed)),
        };
        let needed = output.len() + count;
        if needed > output.capacity() {
            let target = needed
                .max(output.capacity().saturating_mul(2))
                .min(MAX_ICC_BYTES + 1);
            output
                .try_reserve_exact(target - output.len())
                .map_err(|_| error("ICC配置分配失败", "ICC profile allocation failed"))?;
        }
        output.extend_from_slice(&scratch[..count]);
        if output.len() > MAX_ICC_BYTES {
            return Ok(Err(MetadataStatus::TooLarge));
        }
    }
    if output.is_empty() || !decoder.get_ref().is_empty() {
        return Ok(Err(MetadataStatus::Malformed));
    }
    Ok(Ok(output))
}

/// 单个iCCP负载的有界提取；重复块和块顺序由对应容器处理。
/// CRC覆盖类型和负载，不为校验拼接另一份压缩数据。
pub(crate) fn inspect_png_icc_chunk(
    payload: &[u8],
    expected_crc: u32,
    cancel: &AtomicBool,
) -> Result<(FieldSummary, Option<Vec<u8>>), String> {
    inspect_png_icc_chunk_checked(payload, expected_crc, &|| cancellation(cancel))
}

fn inspect_png_icc_chunk_checked(
    payload: &[u8],
    expected_crc: u32,
    cancel: CancelCheck<'_>,
) -> Result<(FieldSummary, Option<Vec<u8>>), String> {
    cancel()?;
    let mut total = 0;
    metadata_budget(&mut total, payload.len())?;
    let mut field = FieldSummary {
        status: MetadataStatus::Malformed,
        encoded_bytes: payload.len() as u64,
        decoded_bytes: None,
    };
    let mut profile = None;
    if crc32_parts_checked(&[b"iCCP", payload], cancel)? == expected_crc {
        let compressed = payload[..payload.len().min(80)]
            .iter()
            .position(|byte| *byte == 0)
            .filter(|index| (1..=79).contains(index))
            .and_then(|index| (payload.get(index + 1) == Some(&0)).then(|| &payload[index + 2..]));
        if let Some(compressed) = compressed {
            match inflate_profile(compressed, cancel)? {
                Ok(bytes) => {
                    field.status = MetadataStatus::Present;
                    field.decoded_bytes = Some(bytes.len() as u64);
                    profile = Some(bytes);
                }
                Err(status) => field.status = status,
            }
        }
    }
    cancel()?;
    Ok((field, profile))
}

fn inspect_png<'a>(bytes: &'a [u8], cancel: CancelCheck<'_>) -> Result<InspectedImage<'a>, String> {
    let mut summary = Summary::new(MetadataStatus::Absent);
    let mut supported_exif = None;
    let mut profile = None;
    let mut edits = Vec::new();
    let mut metadata = 0;
    let mut count = 0;
    let mut offset = 8;
    let mut seen_idat = false;
    let mut ended = false;
    while offset < bytes.len() {
        cancel()?;
        let header = bytes.get(offset..offset + 8).ok_or_else(malformed)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let kind = &header[4..8];
        // 像素压缩流可分成很多IDAT/fdAT，不为它们建目录或消耗元数据段配额。
        if !matches!(kind, b"IDAT" | b"fdAT") {
            count_part(&mut count)?;
        }
        let end = offset
            .checked_add(12)
            .and_then(|end| end.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(malformed)?;
        let data = &bytes[offset + 8..end - 4];
        if !matches!(kind, b"IHDR" | b"PLTE" | b"IDAT" | b"fdAT" | b"IEND") {
            metadata_budget(&mut metadata, data.len())?;
        }
        if offset == 8 && (kind != b"IHDR" || length != 13) {
            return Err(malformed());
        }
        match kind {
            b"IHDR" => {
                if offset != 8 || length != 13 {
                    return Err(malformed());
                }
                summary.bits_per_channel = Some(data[8]);
                summary.color_model = match data[9] {
                    0 | 4 => ColorModel::Gray,
                    2 | 6 => ColorModel::Rgb,
                    3 => ColorModel::Palette,
                    _ => ColorModel::Unknown,
                };
            }
            b"iCCP" => {
                edits.push(Edit::remove(offset..end));
                let encoded_bytes = summary
                    .icc
                    .encoded_bytes
                    .checked_add(data.len() as u64)
                    .ok_or_else(malformed)?;
                if summary.icc.status != MetadataStatus::Absent || seen_idat {
                    summary.icc.status = MetadataStatus::Malformed;
                    summary.icc.decoded_bytes = None;
                    profile = None;
                } else {
                    let expected_crc = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
                    (summary.icc, profile) =
                        inspect_png_icc_chunk_checked(data, expected_crc, cancel)?;
                }
                summary.icc.encoded_bytes = encoded_bytes;
            }
            b"eXIf" => {
                if crc32_checked(&bytes[offset + 4..end - 4], cancel)?
                    != u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap())
                {
                    return Err(error("EXIF校验失败", "EXIF checksum mismatch"));
                }
                supported_exif = Some(exif(&mut summary, data, cancel)?);
                edits.push(Edit::remove(offset..end));
            }
            b"tEXt" | b"iTXt" | b"zTXt" => edits.push(Edit::remove(offset..end)),
            b"IDAT" => seen_idat = true,
            b"IEND" => {
                if length != 0 {
                    return Err(malformed());
                }
                ended = true;
            }
            _ => {}
        }
        offset = end;
        if ended {
            break;
        }
    }
    if !ended || !seen_idat || offset != bytes.len() {
        return Err(malformed());
    }
    Ok(InspectedImage {
        summary,
        icc_profile: profile,
        supported_exif,
        view: edited_view_checked(bytes, edits, cancel)?,
    })
}

fn inspect_webp<'a>(
    bytes: &'a [u8],
    cancel: CancelCheck<'_>,
) -> Result<InspectedImage<'a>, String> {
    if bytes.len() < 12
        || u64::from(u32::from_le_bytes(bytes[4..8].try_into().unwrap())) + 8 != bytes.len() as u64
    {
        return Err(malformed());
    }
    let mut summary = Summary::new(MetadataStatus::Absent);
    let mut supported_exif = None;
    summary.color_model = ColorModel::Rgb;
    summary.bits_per_channel = Some(8);
    let mut profile = None;
    let mut edits = Vec::new();
    let mut metadata = 0;
    let mut count = 0;
    let mut offset = 12;
    let mut removed = 0usize;
    let mut extended = false;
    let mut flags = 0u8;
    while offset < bytes.len() {
        cancel()?;
        count_part(&mut count)?;
        let header = bytes.get(offset..offset + 8).ok_or_else(malformed)?;
        let kind = &header[..4];
        let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        let end = offset
            .checked_add(8)
            .and_then(|end| end.checked_add(length))
            .and_then(|end| end.checked_add(length & 1))
            .filter(|end| *end <= bytes.len())
            .ok_or_else(malformed)?;
        let data = &bytes[offset + 8..offset + 8 + length];
        if !matches!(kind, b"VP8 " | b"VP8L" | b"ALPH" | b"ANMF") {
            metadata_budget(&mut metadata, data.len())?;
        }
        match kind {
            b"VP8X" => {
                if extended || count != 1 || length != 10 {
                    return Err(malformed());
                }
                extended = true;
                flags = data[0];
                if data[0] & 0x28 != 0 {
                    edits.push(Edit::replace(offset + 8..offset + 9, vec![data[0] & !0x28]));
                }
            }
            b"ICCP" => {
                edits.push(Edit::remove(offset..end));
                removed += end - offset;
                summary.icc.encoded_bytes += data.len() as u64;
                if summary.icc.status != MetadataStatus::Absent {
                    summary.icc.status = MetadataStatus::Malformed;
                    summary.icc.decoded_bytes = None;
                    profile = None;
                } else if data.len() > MAX_ICC_BYTES {
                    summary.icc.status = MetadataStatus::TooLarge;
                    summary.icc.decoded_bytes = Some(data.len() as u64);
                } else if data.is_empty() {
                    summary.icc.status = MetadataStatus::Malformed;
                } else {
                    summary.icc.status = MetadataStatus::Present;
                    summary.icc.decoded_bytes = Some(data.len() as u64);
                    profile = Some(copy_profile(data, cancel)?);
                }
            }
            b"EXIF" => {
                supported_exif = Some(exif(&mut summary, data, cancel)?);
                edits.push(Edit::remove(offset..end));
                removed += end - offset;
            }
            _ => {}
        }
        offset = end;
    }
    if flags & 0x08 != 0 && summary.exif.status == MetadataStatus::Absent {
        return Err(error("WebP声明的EXIF缺失", "Declared WebP EXIF is missing"));
    }
    if flags & 0x20 != 0 && summary.icc.status == MetadataStatus::Absent {
        summary.icc.status = MetadataStatus::Malformed;
    }
    if removed != 0 {
        edits.push(Edit::replace(
            4..8,
            ((bytes.len() - removed - 8) as u32).to_le_bytes().to_vec(),
        ));
    }
    Ok(InspectedImage {
        summary,
        icc_profile: profile,
        supported_exif,
        view: edited_view_checked(bytes, edits, cancel)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::{BufReader, Write};

    const PROFILE: &[u8] = include_bytes!("../assets/color/srgb.icc");

    fn pixels() -> Vec<u8> {
        vec![
            10, 20, 30, 0, 40, 50, 60, 128, 70, 80, 90, 255, 110, 120, 130, 64, 140, 150, 160, 192,
            170, 180, 190, 255,
        ]
    }

    fn png() -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut output, 3, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels()).unwrap();
            writer.finish().unwrap();
        }
        output
    }

    fn png_many_idat() -> Vec<u8> {
        let original = png();
        let mut output = original[..8].to_vec();
        let mut offset = 8;
        let mut inserted = false;
        while offset < original.len() {
            let length =
                u32::from_be_bytes(original[offset..offset + 4].try_into().unwrap()) as usize;
            let end = offset + length + 12;
            if !inserted && &original[offset + 4..offset + 8] == b"IDAT" {
                // 连续零长度IDAT合法；原压缩流完整保留，样本无需生成大噪声位图。
                for _ in 0..MAX_PARTS + 1 {
                    output.extend_from_slice(&png_chunk(b"IDAT", &[]));
                }
                inserted = true;
            }
            output.extend_from_slice(&original[offset..end]);
            offset = end;
        }
        assert!(inserted);
        output
    }

    fn jpeg() -> Vec<u8> {
        let mut output = Vec::new();
        let rgb: Vec<_> = pixels()
            .chunks_exact(4)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .collect();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 100)
            .encode(&rgb, 3, 2, image::ExtendedColorType::Rgb8)
            .unwrap();
        output
    }

    fn webp() -> Vec<u8> {
        let mut output = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut output)
            .encode(&pixels(), 3, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        output
    }

    fn decoded(view: SegmentedView<'_>) -> Vec<u8> {
        let decoder = image::ImageReader::new(BufReader::new(view))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        image::DynamicImage::from_decoder(decoder)
            .unwrap()
            .into_rgba8()
            .into_raw()
    }

    fn flatten(mut view: SegmentedView<'_>) -> Vec<u8> {
        let mut output = Vec::new();
        view.read_to_end(&mut output).unwrap();
        output
    }

    fn tiff_orientation(value: u16, little: bool) -> Vec<u8> {
        let mut output = Vec::new();
        let u16_bytes = |value: u16| {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };
        let u32_bytes = |value: u32| {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };
        output.extend_from_slice(if little { b"II" } else { b"MM" });
        output.extend_from_slice(&u16_bytes(42));
        output.extend_from_slice(&u32_bytes(8));
        output.extend_from_slice(&u16_bytes(1));
        output.extend_from_slice(&u16_bytes(274));
        output.extend_from_slice(&u16_bytes(3));
        output.extend_from_slice(&u32_bytes(1));
        output.extend_from_slice(&u16_bytes(value));
        output.extend_from_slice(&[0; 2]);
        output.extend_from_slice(&[0; 4]);
        output
    }

    fn whitelist_tiff(bad_subdirectory: bool) -> Vec<u8> {
        let mut entries = vec![
            (271u16, 2u16, b"RabShot Camera\0".to_vec()),
            (272, 2, b"Model One\0".to_vec()),
            (274, 3, 6u16.to_le_bytes().to_vec()),
        ];
        if bad_subdirectory {
            entries.push((34665, 4, u32::MAX.to_le_bytes().to_vec()));
        }
        let mut output = vec![0; 8 + 2 + entries.len() * 12 + 4];
        output[..8].copy_from_slice(b"II\x2a\x00\x08\x00\x00\x00");
        output[8..10].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        for (index, (tag, kind, data)) in entries.iter().enumerate() {
            let offset = 10 + index * 12;
            output[offset..offset + 2].copy_from_slice(&tag.to_le_bytes());
            output[offset + 2..offset + 4].copy_from_slice(&kind.to_le_bytes());
            let count = data.len()
                / if *kind == 3 {
                    2
                } else if *kind == 4 {
                    4
                } else {
                    1
                };
            output[offset + 4..offset + 8].copy_from_slice(&(count as u32).to_le_bytes());
            if data.len() <= 4 {
                output[offset + 8..offset + 8 + data.len()].copy_from_slice(data);
            } else {
                if output.len() & 1 != 0 {
                    output.push(0);
                }
                let position = output.len() as u32;
                output[offset + 8..offset + 12].copy_from_slice(&position.to_le_bytes());
                output.extend_from_slice(data);
            }
        }
        output
    }

    fn exif_variants(raw_tiff: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let jpeg = jpeg();
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(raw_tiff);
        let with_jpeg = jpeg_with(&jpeg, &[(0xe1, app1)]);
        let png = png();
        let with_png = [
            &png[..png.len() - 12],
            png_chunk(b"eXIf", raw_tiff).as_slice(),
            &png[png.len() - 12..],
        ]
        .concat();
        let webp = webp();
        let mut vp8x = [0; 10];
        vp8x[0] = 0x18;
        vp8x[4] = 2;
        vp8x[7] = 1;
        let with_webp = riff(&[
            riff_chunk(b"VP8X", &vp8x),
            riff_chunk(b"EXIF", raw_tiff),
            webp[12..].to_vec(),
        ]);
        vec![(with_jpeg, jpeg), (with_png, png), (with_webp, webp)]
    }

    fn jpeg_with(base: &[u8], markers: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut output = base[..2].to_vec();
        for (marker, data) in markers {
            output.extend_from_slice(&[0xff, *marker]);
            output.extend_from_slice(&u16::try_from(data.len() + 2).unwrap().to_be_bytes());
            output.extend_from_slice(data);
        }
        output.extend_from_slice(&base[2..]);
        output
    }

    fn icc_part(sequence: u8, count: u8, bytes: &[u8]) -> Vec<u8> {
        let mut output = ICC_PREFIX.to_vec();
        output.extend_from_slice(&[sequence, count]);
        output.extend_from_slice(bytes);
        output
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut output = (data.len() as u32).to_be_bytes().to_vec();
        output.extend_from_slice(kind);
        output.extend_from_slice(data);
        output.extend_from_slice(&crc32(&output[4..]).to_be_bytes());
        output
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn png_icc(bytes: &[u8]) -> Vec<u8> {
        let mut data = b"RGB\0\0".to_vec();
        data.extend_from_slice(&zlib(bytes));
        png_chunk(b"iCCP", &data)
    }

    #[test]
    fn image_container_metadata_view_counts_owned_capacity_without_borrowed_source() {
        let source = Vec::with_capacity(1024);
        let borrowed = SegmentedView::borrowed(&source);
        assert_eq!(
            borrowed.retained_bytes().unwrap(),
            (borrowed.segments.capacity() * std::mem::size_of::<Segment<'_>>()) as u64
        );
        let mut owned = Vec::with_capacity(64);
        owned.extend_from_slice(b"H");
        let owned_capacity = owned.capacity() as u64;
        let mut segments = Vec::with_capacity(4);
        segments.push(Segment {
            start: 0,
            data: Cow::Borrowed(b"body"),
        });
        segments.push(Segment {
            start: 4,
            data: Cow::Owned(owned),
        });
        let directory = (segments.capacity() * std::mem::size_of::<Segment<'_>>()) as u64;
        let mixed = SegmentedView {
            segments,
            length: 5,
            position: 0,
        };
        assert_eq!(mixed.retained_bytes().unwrap(), directory + owned_capacity);
    }

    #[test]
    fn image_container_metadata_png_icc_chunk_matches_static_extraction_and_split_crc() {
        let token = AtomicBool::new(false);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(
            crc32_parts_checked(&[b"1", b"234", b"56789"], &|| Ok(())).unwrap(),
            0xcbf4_3926
        );
        let chunk = png_icc(PROFILE);
        let payload = &chunk[8..chunk.len() - 4];
        let checksum = u32::from_be_bytes(chunk[chunk.len() - 4..].try_into().unwrap());
        let (field, profile) = inspect_png_icc_chunk(payload, checksum, &token).unwrap();
        assert_eq!(field.status, MetadataStatus::Present);
        assert_eq!(field.encoded_bytes, payload.len() as u64);
        assert_eq!(field.decoded_bytes, Some(PROFILE.len() as u64));
        assert_eq!(profile.as_deref(), Some(PROFILE));
        let base = png();
        let bytes = [&base[..33], chunk.as_slice(), &base[33..]].concat();
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.icc, field);
        assert_eq!(inspected.icc_profile, profile);
        assert_eq!(flatten(inspected.view), base);
    }

    #[test]
    fn image_container_metadata_png_icc_chunk_rejects_bad_payload_and_raw_budget() {
        let token = AtomicBool::new(false);
        let compressed = zlib(PROFILE);
        let mut bad_method = b"RGB\0\x01".to_vec();
        bad_method.extend_from_slice(&compressed);
        let mut long_name = vec![b'x'; 80];
        long_name.extend_from_slice(b"\0\0");
        long_name.extend_from_slice(&compressed);
        let mut trailing = b"RGB\0\0".to_vec();
        trailing.extend_from_slice(&compressed);
        trailing.push(0);
        for payload in [
            Vec::new(),
            bad_method,
            long_name,
            b"RGB\0\0broken zlib".to_vec(),
            trailing,
        ] {
            let chunk = png_chunk(b"iCCP", &payload);
            let crc = u32::from_be_bytes(chunk[chunk.len() - 4..].try_into().unwrap());
            let (field, profile) = inspect_png_icc_chunk(&payload, crc, &token).unwrap();
            assert_eq!(field.status, MetadataStatus::Malformed);
            assert_eq!(field.encoded_bytes, payload.len() as u64);
            assert_eq!(field.decoded_bytes, None);
            assert!(profile.is_none());
        }
        let chunk = png_icc(PROFILE);
        let crc = u32::from_be_bytes(chunk[chunk.len() - 4..].try_into().unwrap()) ^ 1;
        let (field, profile) =
            inspect_png_icc_chunk(&chunk[8..chunk.len() - 4], crc, &token).unwrap();
        assert_eq!(field.status, MetadataStatus::Malformed);
        assert!(profile.is_none());
        assert!(
            inspect_png_icc_chunk(&vec![0; MAX_METADATA_BYTES as usize + 1], 0, &token).is_err()
        );
    }

    #[test]
    fn image_container_metadata_png_icc_chunk_cancellation_is_never_malformed() {
        let chunk = png_icc(PROFILE);
        let payload = &chunk[8..chunk.len() - 4];
        let crc = u32::from_be_bytes(chunk[chunk.len() - 4..].try_into().unwrap());
        let token = AtomicBool::new(true);
        assert!(inspect_png_icc_chunk(payload, crc, &token).is_err());
        // 分别在分片CRC和解压读回的checkpoint取消，不依赖线程调度。
        for stop in [4, 7] {
            let checks = Cell::new(0);
            let check = || {
                checks.set(checks.get() + 1);
                if checks.get() == stop {
                    Err("iCCP cancellation checkpoint".to_string())
                } else {
                    Ok(())
                }
            };
            let error = inspect_png_icc_chunk_checked(payload, crc, &check).unwrap_err();
            assert_eq!(error, "iCCP cancellation checkpoint");
            assert_eq!(checks.get(), stop);
        }
        token.store(false, Ordering::Release);
        let (field, bytes) = inspect_png_icc_chunk(payload, crc, &token).unwrap();
        assert_eq!(field.status, MetadataStatus::Present);
        assert_eq!(bytes.as_deref(), Some(PROFILE));
    }

    fn riff_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut output = kind.to_vec();
        output.extend_from_slice(&(data.len() as u32).to_le_bytes());
        output.extend_from_slice(data);
        if data.len() & 1 != 0 {
            output.push(0);
        }
        output
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let length = 4 + chunks.iter().map(Vec::len).sum::<usize>();
        let mut output = b"RIFF".to_vec();
        output.extend_from_slice(&(length as u32).to_le_bytes());
        output.extend_from_slice(b"WEBP");
        for chunk in chunks {
            output.extend_from_slice(chunk);
        }
        output
    }

    #[test]
    fn image_container_metadata_segmented_seek_borrows_large_source_and_keeps_unknown_uninspected()
    {
        let source = b"0123456789abcdef";
        let mut view = edited_view(
            source,
            vec![
                Edit::remove(2..5),
                Edit::replace(8..10, vec![b'X', b'Y', b'Z']),
            ],
        )
        .unwrap();
        let expected = b"01567XYZabcdef";
        assert_eq!(view.len(), expected.len() as u64);
        let mut prefix = [0; 6];
        view.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, &expected[..6]);
        view.seek(SeekFrom::Current(-3)).unwrap();
        let mut cross = [0; 7];
        view.read_exact(&mut cross).unwrap();
        assert_eq!(&cross, &expected[3..10]);
        view.seek(SeekFrom::End(-4)).unwrap();
        assert_eq!(flatten(view), b"cdef");
        let mut view = SegmentedView::borrowed(source);
        assert!(view.seek(SeekFrom::End(-17)).is_err());
        assert_eq!(view.seek(SeekFrom::Start(100)).unwrap(), 100);
        assert_eq!(view.read(&mut prefix).unwrap(), 0);
        let result = inspect(source).unwrap();
        assert_eq!(result.summary.icc.status, MetadataStatus::NotInspected);
        assert_eq!(result.view.segments.len(), 1);
        assert!(matches!(result.view.segments[0].data, Cow::Borrowed(_)));
        assert_eq!(result.view.segments[0].data.as_ptr(), source.as_ptr());
        assert_eq!(flatten(result.view), source);
    }

    #[test]
    fn image_container_metadata_many_idat_do_not_consume_metadata_quota_or_allocate_a_segment_per_chunk(
    ) {
        let bytes = png_many_idat();
        assert!(bytes.len() < 64 * 1024);
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.icc.status, MetadataStatus::Absent);
        assert_eq!(inspected.view.segments.len(), 1);
        assert!(matches!(inspected.view.segments[0].data, Cow::Borrowed(_)));
        assert_eq!(flatten(inspected.view), bytes);
        assert_eq!(decoded(inspect(&bytes).unwrap().view), pixels());
        // IDAT不计数也不能越过必须位于首块的IHDR。
        let misplaced = [
            PNG_SIGNATURE.as_slice(),
            png_chunk(b"IDAT", &[]).as_slice(),
            &bytes[8..],
        ]
        .concat();
        assert!(inspect(&misplaced).is_err());
    }

    #[test]
    fn image_container_metadata_cancellation_interrupts_container_scans_and_icc_inflation() {
        use std::cell::Cell;
        let png = png_many_idat();
        let jpeg = jpeg();
        let webp = webp();
        let cancelled = AtomicBool::new(true);
        for bytes in [
            png.as_slice(),
            jpeg.as_slice(),
            webp.as_slice(),
            b"uninspected",
        ] {
            assert!(inspect_cancellable(bytes, &cancelled).is_err());
        }
        cancelled.store(false, Ordering::Release);
        assert!(inspect_cancellable(&png, &cancelled).is_ok());

        // 注入checkpoint触发同一AtomicBool取消，确定性验证扫描中途退出，不依赖调度或sleep。
        let many_webp = riff(&[
            webp[12..].to_vec(),
            riff_chunk(b"JUNK", &[]),
            riff_chunk(b"JUNK", &[]),
        ]);
        for (bytes, stop) in [
            (png.as_slice(), 64),
            (jpeg.as_slice(), 3),
            (many_webp.as_slice(), 4),
        ] {
            let checks = Cell::new(0usize);
            let token = AtomicBool::new(false);
            let check = || {
                checks.set(checks.get() + 1);
                if checks.get() == stop {
                    token.store(true, Ordering::Release);
                }
                cancellation(&token)
            };
            assert!(inspect_checked(bytes, &check).is_err());
            assert_eq!(checks.get(), stop);
            assert!(token.load(Ordering::Acquire));
        }
        let compressed = zlib(PROFILE);
        let checks = Cell::new(0usize);
        let token = AtomicBool::new(false);
        let check = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                token.store(true, Ordering::Release);
            }
            cancellation(&token)
        };
        assert!(inflate_profile(&compressed, &check).is_err());
        assert_eq!(checks.get(), 2);
        // 普通入口不沿用上一次调用的取消状态。
        assert!(inspect(&png).is_ok());
    }

    #[test]
    fn image_container_metadata_jpeg_reorders_icc_strips_exif_and_preserves_pixel_stream() {
        let base = jpeg();
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(&tiff_orientation(6, true));
        let bytes = jpeg_with(
            &base,
            &[
                (0xe2, icc_part(2, 2, &PROFILE[100..])),
                (0xe1, exif),
                (0xe2, icc_part(1, 2, &PROFILE[..100])),
            ],
        );
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.icc.status, MetadataStatus::Present);
        assert_eq!(inspected.icc_profile.as_deref(), Some(PROFILE));
        assert_eq!(inspected.summary.orientation, Some(Orientation::Rotate90));
        assert_eq!(inspected.summary.color_model, ColorModel::Rgb);
        assert_eq!(inspected.summary.bits_per_channel, Some(8));
        assert_eq!(flatten(inspected.view), base);
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(
            decoded(inspected.view),
            decoded(SegmentedView::borrowed(&base))
        );
        let cmyk = inspect(include_bytes!("../tests/data/image_decode/adobe_cmyk.jpg")).unwrap();
        assert_eq!(cmyk.summary.color_model, ColorModel::Cmyk);
        assert_eq!(cmyk.summary.bits_per_channel, Some(8));
        let progressive = inspect(include_bytes!(
            "../tests/data/image_decode/progressive_rgb.jpg"
        ))
        .unwrap();
        assert_eq!(progressive.summary.color_model, ColorModel::Rgb);
    }

    #[test]
    fn image_container_metadata_jpeg_bad_or_oversized_icc_is_removed_without_reassembly() {
        let base = jpeg();
        for markers in [
            vec![(0xe2, icc_part(1, 2, b"one"))],
            vec![
                (0xe2, icc_part(1, 2, b"one")),
                (0xe2, icc_part(1, 2, b"two")),
            ],
            vec![(0xe2, icc_part(0, 1, b"zero"))],
        ] {
            let bytes = jpeg_with(&base, &markers);
            let inspected = inspect(&bytes).unwrap();
            assert_eq!(inspected.summary.icc.status, MetadataStatus::Malformed);
            assert!(inspected.icc_profile.is_none());
            assert_eq!(flatten(inspected.view), base);
        }
        let part = vec![7; 64_000];
        let markers: Vec<_> = (1..=66)
            .map(|index| (0xe2, icc_part(index, 66, &part)))
            .collect();
        let bytes = jpeg_with(&base, &markers);
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.icc.status, MetadataStatus::TooLarge);
        assert!(inspected.icc_profile.is_none());
        assert_eq!(flatten(inspected.view), base);
    }

    #[test]
    fn image_container_metadata_png_post_idat_direction_and_text_are_frozen_then_removed() {
        let base = png();
        let mut bytes = base[..33].to_vec();
        bytes.extend_from_slice(&png_icc(PROFILE));
        bytes.extend_from_slice(&png_chunk(
            b"zTXt",
            b"invalid compressed text is never inflated",
        ));
        bytes.extend_from_slice(&base[33..base.len() - 12]);
        bytes.extend_from_slice(&png_chunk(b"eXIf", &tiff_orientation(6, false)));
        bytes.extend_from_slice(&png_chunk(b"iTXt", b"unparsed international text"));
        bytes.extend_from_slice(&png_chunk(b"tEXt", b"key\0text"));
        bytes.extend_from_slice(&base[base.len() - 12..]);
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.icc.status, MetadataStatus::Present);
        assert_eq!(inspected.icc_profile.as_deref(), Some(PROFILE));
        assert_eq!(inspected.summary.orientation, Some(Orientation::Rotate90));
        assert_eq!(inspected.summary.exif.status, MetadataStatus::Present);
        assert_eq!(inspected.summary.color_model, ColorModel::Rgb);
        assert_eq!(flatten(inspected.view), base);
        assert_eq!(decoded(inspect(&bytes).unwrap().view), pixels());
        let mut gray16 = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut gray16, 1, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0x80, 0x00]).unwrap();
            writer.finish().unwrap();
        }
        let inspected = inspect(&gray16).unwrap();
        assert_eq!(inspected.summary.color_model, ColorModel::Gray);
        assert_eq!(inspected.summary.bits_per_channel, Some(16));
    }

    #[test]
    fn image_container_metadata_png_inflation_limit_bad_crc_and_duplicate_icc_stay_sanitized() {
        let base = png();
        let large = png_icc(&vec![1; MAX_ICC_BYTES + 1]);
        let large_crc = u32::from_be_bytes(large[large.len() - 4..].try_into().unwrap());
        let (field, profile) = inspect_png_icc_chunk(
            &large[8..large.len() - 4],
            large_crc,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(field.status, MetadataStatus::TooLarge);
        assert_eq!(field.decoded_bytes, None);
        assert!(profile.is_none());
        let mut bad_crc = png_icc(PROFILE);
        *bad_crc.last_mut().unwrap() ^= 1;
        let duplicate = [png_icc(PROFILE), png_icc(PROFILE)].concat();
        for (chunk, expected) in [
            (large, MetadataStatus::TooLarge),
            (bad_crc, MetadataStatus::Malformed),
            (duplicate, MetadataStatus::Malformed),
        ] {
            let bytes = [&base[..33], chunk.as_slice(), &base[33..]].concat();
            let inspected = inspect(&bytes).unwrap();
            assert_eq!(inspected.summary.icc.status, expected);
            assert!(inspected.icc_profile.is_none());
            assert_eq!(flatten(inspected.view), base);
        }
    }

    #[test]
    fn image_container_metadata_webp_rewrites_riff_flags_and_preserves_encoded_pixels() {
        let base = webp();
        let mut vp8x = [0; 10];
        vp8x[0] = 0x3c;
        vp8x[4] = 2;
        vp8x[7] = 1;
        let pixel_chunk = base[12..].to_vec();
        let bytes = riff(&[
            riff_chunk(b"VP8X", &vp8x),
            riff_chunk(b"ICCP", PROFILE),
            riff_chunk(b"EXIF", &tiff_orientation(8, true)),
            riff_chunk(b"XMP ", b"abc"),
            pixel_chunk.clone(),
        ]);
        let inspected = inspect(&bytes).unwrap();
        assert_eq!(inspected.summary.orientation, Some(Orientation::Rotate270));
        assert_eq!(inspected.icc_profile.as_deref(), Some(PROFILE));
        let sanitized = flatten(inspected.view);
        assert_eq!(sanitized[20], 0x14);
        assert_eq!(
            u32::from_le_bytes(sanitized[4..8].try_into().unwrap()) as usize + 8,
            sanitized.len()
        );
        assert!(sanitized.ends_with(&pixel_chunk));
        assert_eq!(
            decoded(inspect(&bytes).unwrap().view),
            decoded(SegmentedView::borrowed(&base))
        );
        let duplicate = riff(&[
            riff_chunk(b"VP8X", &vp8x),
            riff_chunk(b"ICCP", PROFILE),
            riff_chunk(b"ICCP", PROFILE),
            riff_chunk(b"EXIF", &tiff_orientation(8, true)),
            pixel_chunk.clone(),
        ]);
        let inspected = inspect(&duplicate).unwrap();
        assert_eq!(inspected.summary.icc.status, MetadataStatus::Malformed);
        assert!(inspected.icc_profile.is_none());
        assert!(flatten(inspected.view).ends_with(&pixel_chunk));
        let duplicate_exif = riff(&[
            riff_chunk(b"VP8X", &vp8x),
            riff_chunk(b"EXIF", &tiff_orientation(1, true)),
            riff_chunk(b"EXIF", &tiff_orientation(1, true)),
            pixel_chunk,
        ]);
        assert!(inspect(&duplicate_exif).is_err());
        let mut truncated = bytes;
        truncated.pop();
        assert!(inspect(&truncated).is_err());
    }

    #[test]
    fn image_container_metadata_exif_eight_directions_and_bad_ifd_are_strict() {
        for little in [true, false] {
            for direction in 1..=8 {
                assert_eq!(
                    exif_orientation(&tiff_orientation(direction, little))
                        .unwrap()
                        .unwrap()
                        .to_exif(),
                    direction as u8
                );
            }
        }
        let valid = tiff_orientation(6, true);
        let mut cases = vec![
            valid[..valid.len() - 1].to_vec(),
            tiff_orientation(0, true),
            tiff_orientation(9, true),
        ];
        let mut bytes = valid.clone();
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push(bytes);
        let mut bytes = valid.clone();
        bytes[8..10].copy_from_slice(&257u16.to_le_bytes());
        cases.push(bytes);
        let mut bytes = valid;
        bytes[14..18].copy_from_slice(&3u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push(bytes);
        for bytes in cases {
            assert!(exif_orientation(&bytes).is_err());
            let base = png();
            let bytes = [
                &base[..33],
                png_chunk(b"eXIf", &bytes).as_slice(),
                &base[33..],
            ]
            .concat();
            assert!(inspect(&bytes).is_err());
        }
    }

    #[test]
    fn image_container_metadata_supported_exif_is_extracted_from_three_containers_without_retaining_the_blob(
    ) {
        for (bytes, base) in exif_variants(&whitelist_tiff(false)) {
            let inspected = inspect(&bytes).unwrap();
            assert_eq!(inspected.summary.orientation, Some(Orientation::Rotate90));
            assert_eq!(inspected.summary.exif.status, MetadataStatus::Present);
            let fields = inspected.supported_exif.as_ref().unwrap().as_ref().unwrap();
            assert_eq!(fields.summary().retained_fields, 2);
            assert!(fields.retained_bytes() <= 32 * 1024);
            let rewritten = fields
                .encode((2, 3), true, &AtomicBool::new(false))
                .unwrap();
            assert!(rewritten
                .windows(b"RabShot Camera\0".len())
                .any(|bytes| bytes == b"RabShot Camera\0"));
            assert!(rewritten
                .windows(b"Model One\0".len())
                .any(|bytes| bytes == b"Model One\0"));
            assert_eq!(
                exif_orientation(&rewritten).unwrap(),
                Some(Orientation::NoTransforms)
            );
            assert_eq!(
                crate::image_exif::inspect(&rewritten, &AtomicBool::new(false))
                    .unwrap()
                    .summary()
                    .retained_fields,
                2
            );
            assert_eq!(
                decoded(inspected.view),
                decoded(SegmentedView::borrowed(&base))
            );
            assert!(inspect(&base).unwrap().supported_exif.is_none());
        }
        assert!(inspect(b"uninspected").unwrap().supported_exif.is_none());
    }

    #[test]
    fn image_container_metadata_unsupported_exif_structure_keeps_pixels_but_cancellation_cannot_be_downgraded(
    ) {
        let damaged = whitelist_tiff(true);
        assert_eq!(
            exif_orientation(&damaged).unwrap(),
            Some(Orientation::Rotate90)
        );
        for (bytes, base) in exif_variants(&damaged) {
            let inspected = inspect(&bytes).unwrap();
            assert_eq!(inspected.summary.orientation, Some(Orientation::Rotate90));
            assert!(inspected.supported_exif.as_ref().unwrap().is_err());
            assert_eq!(
                decoded(inspected.view),
                decoded(SegmentedView::borrowed(&base))
            );
        }
        let valid = whitelist_tiff(false);
        let strict_checks = Cell::new(0usize);
        exif_orientation_checked(&valid, &|| {
            strict_checks.set(strict_checks.get() + 1);
            Ok(())
        })
        .unwrap();
        let stop = strict_checks.get() + 2;
        let checks = Cell::new(0usize);
        let cancel_once = || {
            checks.set(checks.get() + 1);
            if checks.get() == stop {
                Err("Canceled during supported EXIF extraction".to_string())
            } else {
                Ok(())
            }
        };
        let mut summary = Summary::new(MetadataStatus::Absent);
        assert!(exif(&mut summary, &valid, &cancel_once).is_err());
        // 严格方向已通过，取消发生在白名单读取阶段，仍然终止整个容器检查。
        assert_eq!(summary.exif.status, MetadataStatus::Present);
        assert_eq!(summary.orientation, Some(Orientation::Rotate90));
        assert!(checks.get() >= stop);
    }

    #[test]
    fn image_container_metadata_metadata_and_chunk_budgets_precede_backend_reads() {
        let base = png();
        let huge_text = png_chunk(b"tEXt", &vec![0; MAX_METADATA_BYTES as usize + 1]);
        let bytes = [&base[..33], huge_text.as_slice(), &base[33..]].concat();
        assert!(inspect(&bytes).is_err());
        let mut bytes = base[..33].to_vec();
        for _ in 0..MAX_PARTS {
            bytes.extend_from_slice(&png_chunk(b"tEXt", &[]));
        }
        bytes.extend_from_slice(&base[33..]);
        assert!(inspect(&bytes).is_err());
        let bytes = [
            &base[..33],
            png_chunk(b"eXIf", &vec![0; MAX_EXIF_BYTES + 1]).as_slice(),
            &base[33..],
        ]
        .concat();
        assert!(inspect(&bytes).is_err());
        // 仅检查熵流标记的预算分类，不把这个边界流声称为可解码JPEG样本。
        let mut entropy = vec![0xff, 0xd8, 0xff, 0xda, 0, 2];
        for index in 0..MAX_PARTS + 1 {
            entropy.extend_from_slice(&[0xff, 0xd0 + (index % 8) as u8]);
        }
        entropy.extend_from_slice(&[0xff, 0xd9]);
        assert_eq!(flatten(inspect(&entropy).unwrap().view), entropy);
    }
}
