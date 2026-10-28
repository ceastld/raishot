//! 在不可变编码产物封存前选择颜色及EXIF白名单；方向、尺寸由当前输出重建。
//! 不复制未识别字段、GPS、缩略图或私有数据，不把白名单称为完整元数据保留。
//! 输入仅限本应用新编码、尚未附加元数据的PNG/JPEG/静态WebP/BMP；压缩像素保持原样。
//! PNG块顺序见https://www.w3.org/TR/png-3/；JPEG APP2见ICC.1 Annex B；
//! WebP扩展头见https://developers.google.com/speed/webp/docs/riff_container。
use crate::{
    image_color_management::{ColorConversion, ManagedMetadata, UnappliedReason},
    image_color_profile::{self, ProfileSummary},
    image_container_metadata::{ColorModel, MetadataStatus},
    image_exif::{ExifSummary, SupportedExif},
    save_options::SaveFormat,
};
use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const MAX_ENCODED_BYTES: usize = 128 * 1024 * 1024;
const MAX_PARTS: usize = 4096;
const JPEG_ICC_PART: usize = 65_519;
const MAX_EXIF_BYTES: usize = 256 * 1024;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const SRGB_PROFILE: &[u8] = include_bytes!("../assets/color/srgb.icc");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MetadataPolicy {
    #[default]
    Strip,
    /// 只保留适用于当前RGB像素的ICC；不代表保留EXIF或全部元数据。
    ColorOnly,
    /// 保留适用ICC及可重建的EXIF白名单，不包含未知、位置或私有字段。
    ColorAndSupportedExif,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceMetadata {
    facts: ManagedMetadata,
    // 只有未转换且确可复用的RGB源配置需要驻留；其它状态只保留事实。
    source_icc: Option<Arc<[u8]>>,
    exif: Option<Arc<SupportedExif>>,
    exif_error: Option<String>,
}

fn error(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn invalid() -> String {
    error(
        "元数据导出仅接受匹配尺寸的本应用新编码图片",
        "Metadata export requires a fresh application encoding with matching dimensions",
    )
}

fn cancel(flag: &AtomicBool) -> Result<(), String> {
    if flag.load(Ordering::Relaxed) {
        Err(error("元数据处理已取消", "Metadata processing canceled"))
    } else {
        Ok(())
    }
}

fn rgb_profile(profile: ProfileSummary) -> bool {
    profile.color_space == *b"RGB "
        && matches!(&profile.connection_space, b"XYZ " | b"Lab ")
        && matches!(&profile.device_class, b"scnr" | b"mntr" | b"prtr" | b"spac")
}

fn rgb_source(facts: ManagedMetadata) -> bool {
    matches!(
        facts.source.color_model,
        ColorModel::Rgb | ColorModel::Bgr | ColorModel::Palette
    ) && facts.source.icc.status == MetadataStatus::Present
        && facts.profile.is_some_and(rgb_profile)
}

impl SourceMetadata {
    /// 来自同次解码的事实和已提取配置；不接受路径，也不在保存时重新读原文件。
    /// 坏配置、超限配置及缺少字节的降级状态仍可创建上下文，供默认Strip使用。
    pub(crate) fn new(facts: ManagedMetadata, source_icc: Option<&[u8]>) -> Result<Self, String> {
        let mut retained = None;
        if let Some(bytes) = source_icc {
            if bytes.len() > image_color_profile::MAX_PROFILE_BYTES {
                return Err(error(
                    "导出源ICC超过4MiB",
                    "Export source ICC exceeds 4 MiB",
                ));
            }
            if facts.source.icc.status != MetadataStatus::Present
                || facts.source.icc.decoded_bytes != Some(bytes.len() as u64)
            {
                return Err(error(
                    "源ICC字节与读取事实不一致",
                    "Source ICC bytes do not match the inspection",
                ));
            }
            match image_color_profile::inspect(bytes) {
                Ok(profile) if facts.profile == Some(profile) => {
                    if facts.color_conversion
                        == ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
                        && rgb_source(facts)
                    {
                        let mut copy = Vec::new();
                        copy.try_reserve_exact(bytes.len()).map_err(|_| {
                            error("导出ICC分配失败", "Export ICC allocation failed")
                        })?;
                        copy.extend_from_slice(bytes);
                        retained = Some(Arc::from(copy));
                    }
                }
                Err(_)
                    if facts.profile.is_none()
                        && facts.color_conversion
                            == ColorConversion::EmbeddedUnapplied(
                                UnappliedReason::InvalidProfile,
                            ) => {}
                _ => {
                    return Err(error(
                        "源ICC摘要与字节不一致",
                        "Source ICC summary does not match its bytes",
                    ))
                }
            }
        }
        Ok(Self {
            facts,
            source_icc: retained,
            exif: None,
            exif_error: None,
        })
    }

    pub(crate) fn facts(&self) -> ManagedMetadata {
        self.facts
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.source_icc.as_ref().map_or(0, |profile| profile.len())
            + self.exif.as_ref().map_or(0, |exif| exif.retained_bytes())
            + self.exif_error.as_ref().map_or(0, String::capacity)
    }

    /// 白名单失败只禁用对应保留选项，不改变像素读取或默认Strip的结果。
    pub(crate) fn with_exif(mut self, exif: Result<SupportedExif, String>) -> Self {
        match exif {
            Ok(exif) if exif.retained_bytes() <= MAX_EXIF_BYTES => {
                self.exif = Some(Arc::new(exif));
                self.exif_error = None;
            }
            Ok(_) => {
                self.exif = None;
                self.exif_error = Some(error(
                    "EXIF白名单超过256KiB",
                    "Supported EXIF exceeds 256 KiB",
                ));
            }
            Err(reason) => {
                self.exif = None;
                self.exif_error = Some(reason);
            }
        }
        self
    }

    pub(crate) fn exif_summary(&self) -> Option<ExifSummary> {
        self.exif.as_ref().map(|exif| exif.summary())
    }

    pub(crate) fn exif_error(&self) -> Option<&str> {
        self.exif_error.as_deref()
    }

    fn encoded_exif(
        &self,
        dimensions: (u32, u32),
        srgb: bool,
        flag: &AtomicBool,
    ) -> Result<Vec<u8>, String> {
        let exif = self.exif.as_ref().ok_or_else(|| {
            self.exif_error.as_ref().map_or_else(
                || {
                    error(
                        "此图片没有可保留的EXIF白名单",
                        "This image has no supported EXIF to preserve",
                    )
                },
                |reason| {
                    trf!(
                        "EXIF白名单不可用：{reason}",
                        "Supported EXIF is unavailable: {reason}"
                    )
                },
            )
        })?;
        cancel(flag)?;
        let bytes = exif.encode(dimensions, srgb, flag)?;
        cancel(flag)?;
        if bytes.len() > MAX_EXIF_BYTES {
            return Err(error("重建EXIF超过256KiB", "Rebuilt EXIF exceeds 256 KiB"));
        }
        Ok(bytes)
    }

    fn profile(&self) -> Result<&[u8], String> {
        match self.facts.color_conversion {
            ColorConversion::AssumedSrgb
                if self.facts.source.icc.status == MetadataStatus::Absent
                    && self.facts.profile.is_none() =>
            {
                Ok(SRGB_PROFILE)
            }
            ColorConversion::ConvertedSrgb | ColorConversion::ConvertedSrgb16
                if rgb_source(self.facts) =>
            {
                Ok(SRGB_PROFILE)
            }
            ColorConversion::RenderedSrgb
                if self.facts.source.icc.status == MetadataStatus::Absent
                    && self.facts.profile.is_none()
                    && self.facts.source.bits_per_channel.is_none()
                    && matches!(
                        self.facts.source.color_model,
                        ColorModel::Rgb | ColorModel::Bgr
                    ) =>
            {
                Ok(SRGB_PROFILE)
            }
            ColorConversion::HdrMappedSrgb
                if self.facts.source.icc.status == MetadataStatus::Absent
                    && self.facts.profile.is_none()
                    && matches!(self.facts.source.bits_per_channel, Some(10 | 12 | 16))
                    && matches!(
                        self.facts.source.color_model,
                        ColorModel::Rgb | ColorModel::Bgr
                    ) =>
            {
                Ok(SRGB_PROFILE)
            }
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed)
                if rgb_source(self.facts) =>
            {
                self.source_icc.as_deref().ok_or_else(|| {
                    error(
                        "缺少匹配当前像素的源ICC",
                        "Source ICC matching the current pixels is unavailable",
                    )
                })
            }
            _ => Err(error(
                "当前颜色状态无法保留ICC，请选择移除元数据",
                "This color state cannot preserve ICC; choose Strip metadata",
            )),
        }
    }
}

enum Fresh {
    Png,
    Jpeg { insertion: usize },
    Webp { extended: bool, alpha: bool },
    Bmp,
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn fresh_png(bytes: &[u8], dimensions: (u32, u32), flag: &AtomicBool) -> Result<Fresh, String> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(invalid());
    }
    let mut position = 8usize;
    let mut idat = false;
    // 流式编码每4KiB写一个IDAT；像素块不分配目录，按有界源长度单调扫描。
    // 不能用元数据段数限制阻止本应用生成的较大PNG再次保存。
    while position < bytes.len() {
        cancel(flag)?;
        let header = bytes.get(position..position + 8).ok_or_else(invalid)?;
        let length = be32(header) as usize;
        let end = position
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|n| *n <= bytes.len())
            .ok_or_else(invalid)?;
        let data = &bytes[position + 8..end - 4];
        match &header[4..8] {
            b"IHDR" if position == 8 && data.len() == 13 => {
                if (be32(data), be32(&data[4..])) != dimensions
                    || data[8..] != [8, 6, 0, 0, 0]
                    || crc(&bytes[position + 4..end - 4]) != be32(&bytes[end - 4..])
                {
                    return Err(invalid());
                }
            }
            b"IDAT" if position > 8 && length > 0 => idat = true,
            b"IEND" if position > 8 && idat && length == 0 && end == bytes.len() => {
                return Ok(Fresh::Png)
            }
            _ => return Err(invalid()),
        }
        position = end;
    }
    Err(invalid())
}

fn fresh_jpeg(bytes: &[u8], dimensions: (u32, u32), flag: &AtomicBool) -> Result<Fresh, String> {
    if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(invalid());
    }
    let mut position = 2usize;
    let mut insertion = 2;
    let mut sof = false;
    for _ in 0..MAX_PARTS {
        cancel(flag)?;
        let header = bytes.get(position..position + 4).ok_or_else(invalid)?;
        if header[0] != 0xff {
            return Err(invalid());
        }
        let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let end = position
            .checked_add(2)
            .and_then(|n| n.checked_add(length))
            .filter(|n| *n <= bytes.len())
            .ok_or_else(invalid)?;
        if length < 2 {
            return Err(invalid());
        }
        let data = &bytes[position + 4..end];
        match header[1] {
            0xe0 if position == 2
                && data.len() == 14
                && data.starts_with(b"JFIF\0")
                && data[12..] == [0, 0] =>
            {
                insertion = end
            }
            0xc0 if !sof && data.len() == 15 && data[0] == 8 && data[5] == 3 => {
                let size = (
                    u32::from(u16::from_be_bytes([data[3], data[4]])),
                    u32::from(u16::from_be_bytes([data[1], data[2]])),
                );
                if size != dimensions {
                    return Err(invalid());
                }
                sof = true;
            }
            0xdb | 0xc4 | 0xdd => {}
            0xda if sof && data.len() == 10 && data[0] == 3 && end + 2 <= bytes.len() => {
                return Ok(Fresh::Jpeg { insertion })
            }
            _ => return Err(invalid()),
        }
        position = end;
    }
    Err(invalid())
}

fn fresh_webp_chunk(bytes: &[u8], position: usize) -> Result<([u8; 4], &[u8], usize), String> {
    let data_start = position.checked_add(8).ok_or_else(invalid)?;
    let header = bytes.get(position..data_start).ok_or_else(invalid)?;
    let length = le32(&header[4..]) as usize;
    let data_end = data_start.checked_add(length).ok_or_else(invalid)?;
    let end = data_end.checked_add(length & 1).ok_or_else(invalid)?;
    let data = bytes.get(data_start..data_end).ok_or_else(invalid)?;
    if end > bytes.len() || (length & 1 != 0 && bytes[end - 1] != 0) {
        return Err(invalid());
    }
    Ok((header[..4].try_into().unwrap(), data, end))
}

fn fresh_webp(bytes: &[u8], dimensions: (u32, u32)) -> Result<Fresh, String> {
    if bytes.len() < 25
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WEBP"
        || u64::from(le32(&bytes[4..])) + 8 != bytes.len() as u64
    {
        return Err(invalid());
    }
    let extended = bytes.get(12..16) == Some(b"VP8X");
    let mut position = if extended {
        if bytes.len() < 43
            || le32(&bytes[16..]) != 10
            || bytes[20] & !0x10 != 0
            || bytes[21..24] != [0, 0, 0]
            || (
                le32(&[bytes[24], bytes[25], bytes[26], 0]) + 1,
                le32(&[bytes[27], bytes[28], bytes[29], 0]) + 1,
            ) != dimensions
        {
            return Err(invalid());
        }
        30
    } else {
        12
    };
    let (mut kind, mut data, mut end) = fresh_webp_chunk(bytes, position)?;
    let alpha_chunk = kind == *b"ALPH";
    if alpha_chunk {
        // 新有损编码保留独立直alpha；扩展头、ALPH、VP8严格相邻，不能夹带旧元数据。
        if !extended || bytes[20] & 0x10 == 0 || data.len() < 2 {
            return Err(invalid());
        }
        let flags = data[0];
        if flags & 0xc0 != 0
            || flags & 3 > 1
            || (flags >> 4) & 3 > 1
            || (flags & 3 == 0
                && data.len() as u64 != 1 + u64::from(dimensions.0) * u64::from(dimensions.1))
        {
            return Err(invalid());
        }
        position = end;
        (kind, data, end) = fresh_webp_chunk(bytes, position)?;
    }
    let alpha = match &kind {
        b"VP8L" if !alpha_chunk && data.len() >= 5 && data[0] == 0x2f => {
            let bits = le32(&data[1..]);
            if bits >> 29 != 0 || ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1) != dimensions {
                return Err(invalid());
            }
            bits & (1 << 28) != 0
        }
        b"VP8 " if data.len() >= 10 => {
            let tag = le32(&[data[0], data[1], data[2], 0]);
            let first_partition_bytes = (tag >> 5) as usize;
            let width = u16::from_le_bytes([data[6], data[7]]);
            let height = u16::from_le_bytes([data[8], data[9]]);
            if tag & 1 != 0
                || (tag >> 1) & 7 > 3
                || tag & 0x10 == 0
                || first_partition_bytes == 0
                || first_partition_bytes > data.len() - 10
                || data[3..6] != [0x9d, 1, 0x2a]
                || width & 0xc000 != 0
                || height & 0xc000 != 0
                || (u32::from(width), u32::from(height)) != dimensions
            {
                return Err(invalid());
            }
            alpha_chunk
        }
        _ => return Err(invalid()),
    };
    if end != bytes.len() || (extended && (bytes[20] & 0x10 != 0) != alpha) {
        return Err(invalid());
    }
    Ok(Fresh::Webp { extended, alpha })
}

fn fresh_bmp(bytes: &[u8], dimensions: (u32, u32)) -> Result<Fresh, String> {
    if bytes.len() < 54
        || &bytes[..2] != b"BM"
        || le32(&bytes[2..]) as usize != bytes.len()
        || le32(&bytes[10..]) != 54
        || le32(&bytes[14..]) != 40
        || (le32(&bytes[18..]), le32(&bytes[22..])) != dimensions
        || bytes[26..30] != [1, 0, 24, 0]
        || le32(&bytes[30..]) != 0
        || le32(&bytes[46..]) != 0
    {
        return Err(invalid());
    }
    let row = (u64::from(dimensions.0) * 3 + 3) & !3;
    if row * u64::from(dimensions.1) + 54 != bytes.len() as u64 {
        return Err(invalid());
    }
    Ok(Fresh::Bmp)
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}
fn crc(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = crc_table();
    let mut result = u32::MAX;
    for byte in bytes {
        result = TABLE[((result ^ u32::from(*byte)) & 255) as usize] ^ (result >> 8);
    }
    !result
}

struct CompressedProfile(Vec<u8>);
impl Write for CompressedProfile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let limit = image_color_profile::MAX_PROFILE_BYTES + 64 * 1024;
        let needed = self
            .0
            .len()
            .checked_add(bytes.len())
            .filter(|n| *n <= limit)
            .ok_or_else(|| {
                io::Error::other(error("ICC压缩数据超限", "Compressed ICC exceeds its limit"))
            })?;
        if needed > self.0.capacity() {
            let target = needed
                .max(self.0.capacity().max(4096).saturating_mul(2))
                .min(limit);
            self.0
                .try_reserve_exact(target - self.0.len())
                .map_err(|_| io::Error::other(error("ICC分配失败", "ICC allocation failed")))?;
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn png_icc(profile: &[u8], flag: &AtomicBool) -> Result<Vec<u8>, String> {
    let mut encoder =
        flate2::write::ZlibEncoder::new(CompressedProfile(Vec::new()), flate2::Compression::fast());
    for part in profile.chunks(64 * 1024) {
        cancel(flag)?;
        encoder.write_all(part).map_err(|error| error.to_string())?;
    }
    let compressed = encoder.finish().map_err(|error| error.to_string())?.0;
    cancel(flag)?;
    let mut chunk = Vec::new();
    chunk
        .try_reserve_exact(12 + 9 + compressed.len())
        .map_err(|_| error("ICC块分配失败", "ICC chunk allocation failed"))?;
    chunk.extend_from_slice(&((9 + compressed.len()) as u32).to_be_bytes());
    chunk.extend_from_slice(b"iCCPprofile\0\0");
    chunk.extend_from_slice(&compressed);
    chunk.extend_from_slice(&crc(&chunk[4..]).to_be_bytes());
    Ok(chunk)
}

fn append_jpeg_icc(chunks: &mut Vec<u8>, profile: &[u8], flag: &AtomicBool) -> Result<(), String> {
    let count = profile.len().div_ceil(JPEG_ICC_PART);
    if !(1..=255).contains(&count) {
        return Err(invalid());
    }
    chunks
        .try_reserve_exact(profile.len() + count * 18)
        .map_err(|_| error("ICC分片分配失败", "ICC segment allocation failed"))?;
    for (index, part) in profile.chunks(JPEG_ICC_PART).enumerate() {
        cancel(flag)?;
        chunks.extend_from_slice(&[0xff, 0xe2]);
        chunks.extend_from_slice(&((part.len() + 16) as u16).to_be_bytes());
        chunks.extend_from_slice(b"ICC_PROFILE\0");
        chunks.extend_from_slice(&[(index + 1) as u8, count as u8]);
        chunks.extend_from_slice(part);
    }
    Ok(())
}

fn append_png_exif(chunks: &mut Vec<u8>, tiff: &[u8]) -> Result<(), String> {
    chunks
        .try_reserve_exact(tiff.len() + 12)
        .map_err(|_| error("EXIF块分配失败", "EXIF chunk allocation failed"))?;
    let start = chunks.len();
    chunks.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
    chunks.extend_from_slice(b"eXIf");
    chunks.extend_from_slice(tiff);
    let checksum = crc(&chunks[start + 4..]);
    chunks.extend_from_slice(&checksum.to_be_bytes());
    Ok(())
}

fn append_jpeg_exif(chunks: &mut Vec<u8>, tiff: &[u8]) -> Result<(), String> {
    if tiff.len() > 65_533 - 6 {
        return Err(error(
            "EXIF白名单超过JPEG单APP1容量",
            "Supported EXIF exceeds one JPEG APP1 segment",
        ));
    }
    chunks
        .try_reserve_exact(tiff.len() + 10)
        .map_err(|_| error("EXIF分片分配失败", "EXIF segment allocation failed"))?;
    chunks.extend_from_slice(&[0xff, 0xe1]);
    chunks.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    chunks.extend_from_slice(b"Exif\0\0");
    chunks.extend_from_slice(tiff);
    Ok(())
}

fn append_webp_exif(encoded: &mut Vec<u8>, tiff: &[u8], flag: &AtomicBool) -> Result<(), String> {
    cancel(flag)?;
    let addition = 8 + tiff.len() + (tiff.len() & 1);
    expanded_length(encoded.len(), addition)?;
    encoded
        .try_reserve_exact(addition)
        .map_err(|_| error("EXIF块分配失败", "EXIF chunk allocation failed"))?;
    encoded.extend_from_slice(b"EXIF");
    encoded.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
    encoded.extend_from_slice(tiff);
    if tiff.len() & 1 != 0 {
        encoded.push(0);
    }
    cancel(flag)
}

fn webp_icc(
    profile: &[u8],
    dimensions: (u32, u32),
    extended: bool,
    alpha: bool,
) -> Result<Vec<u8>, String> {
    let mut chunks = Vec::new();
    chunks
        .try_reserve_exact(profile.len() + (profile.len() & 1) + 26)
        .map_err(|_| error("ICC块分配失败", "ICC chunk allocation failed"))?;
    if !extended {
        chunks.extend_from_slice(b"VP8X\x0a\0\0\0");
        chunks.extend_from_slice(&[0x20 | if alpha { 0x10 } else { 0 }, 0, 0, 0]);
        chunks.extend_from_slice(&(dimensions.0 - 1).to_le_bytes()[..3]);
        chunks.extend_from_slice(&(dimensions.1 - 1).to_le_bytes()[..3]);
    }
    chunks.extend_from_slice(b"ICCP");
    chunks.extend_from_slice(&(profile.len() as u32).to_le_bytes());
    chunks.extend_from_slice(profile);
    if profile.len() & 1 != 0 {
        chunks.push(0);
    }
    Ok(chunks)
}

fn expanded_length(original: usize, addition: usize) -> Result<usize, String> {
    original
        .checked_add(addition)
        .filter(|n| *n <= MAX_ENCODED_BYTES)
        .ok_or_else(|| {
            error(
                "附加元数据后图片超过128MiB",
                "Image with metadata exceeds 128 MiB",
            )
        })
}

fn insert(
    bytes: &mut Vec<u8>,
    position: usize,
    addition: &[u8],
    flag: &AtomicBool,
) -> Result<(), String> {
    let original = bytes.len();
    let length = expanded_length(original, addition.len())?;
    cancel(flag)?;
    bytes
        .try_reserve_exact(length - original)
        .map_err(|_| error("编码元数据分配失败", "Encoded metadata allocation failed"))?;
    bytes.resize(length, 0);
    bytes.copy_within(position..original, position + addition.len());
    bytes[position..position + addition.len()].copy_from_slice(addition);
    cancel(flag)
}

/// 必须在EncodedImage封存及统计准确大小之前调用；之后预览和保存共用返回的同一份字节。
/// 不解码/重编码压缩像素。Strip不读取source，验证新编码容器后原样返回其Vec。
pub(crate) fn apply(
    mut encoded: Vec<u8>,
    format: SaveFormat,
    dimensions: (u32, u32),
    policy: MetadataPolicy,
    source: Option<&SourceMetadata>,
    flag: &AtomicBool,
) -> Result<Vec<u8>, String> {
    cancel(flag)?;
    if encoded.len() > MAX_ENCODED_BYTES {
        return Err(error("编码图片超过128MiB", "Encoded image exceeds 128 MiB"));
    }
    if dimensions.0 == 0
        || dimensions.1 == 0
        || dimensions.0 > 65_535
        || dimensions.1 > 65_535
        || u64::from(dimensions.0) * u64::from(dimensions.1)
            > crate::image_decode::MAX_DECODE_PIXELS
    {
        return Err(invalid());
    }
    let fresh = match format {
        SaveFormat::Png => fresh_png(&encoded, dimensions, flag)?,
        SaveFormat::Jpeg => fresh_jpeg(&encoded, dimensions, flag)?,
        SaveFormat::Webp => fresh_webp(&encoded, dimensions)?,
        SaveFormat::Bmp => fresh_bmp(&encoded, dimensions)?,
    };
    if policy == MetadataPolicy::Strip {
        cancel(flag)?;
        return Ok(encoded);
    }
    if matches!(fresh, Fresh::Bmp) {
        return Err(error(
            "BMP暂不支持保留ICC或EXIF，请移除元数据",
            "BMP cannot preserve ICC or EXIF; choose Strip metadata",
        ));
    }
    let source = source.ok_or_else(|| {
        error(
            "此图片没有可核实的颜色来源",
            "This image has no verified color source",
        )
    })?;
    let profile = source.profile()?;
    let exif = if policy == MetadataPolicy::ColorAndSupportedExif {
        Some(source.encoded_exif(dimensions, profile == SRGB_PROFILE, flag)?)
    } else {
        None
    };
    match fresh {
        Fresh::Png => {
            let mut addition = png_icc(profile, flag)?;
            if let Some(tiff) = &exif {
                append_png_exif(&mut addition, tiff)?;
            }
            insert(&mut encoded, 33, &addition, flag)?;
        }
        Fresh::Jpeg { insertion } => {
            let mut addition = Vec::new();
            if let Some(tiff) = &exif {
                append_jpeg_exif(&mut addition, tiff)?;
            }
            append_jpeg_icc(&mut addition, profile, flag)?;
            insert(&mut encoded, insertion, &addition, flag)?;
        }
        Fresh::Webp { extended, alpha } => {
            let addition = webp_icc(profile, dimensions, extended, alpha)?;
            let exif_length = exif
                .as_ref()
                .map_or(0, |tiff| 8 + tiff.len() + (tiff.len() & 1));
            expanded_length(encoded.len(), addition.len() + exif_length)?;
            insert(
                &mut encoded,
                if extended { 30 } else { 12 },
                &addition,
                flag,
            )?;
            encoded[20] |= 0x20 | if exif.is_some() { 0x08 } else { 0 };
            if let Some(tiff) = &exif {
                append_webp_exif(&mut encoded, tiff, flag)?;
            }
            let length = (encoded.len() - 8) as u32;
            encoded[4..8].copy_from_slice(&length.to_le_bytes());
        }
        Fresh::Bmp => unreachable!(),
    }
    cancel(flag)?;
    Ok(encoded)
}

#[cfg(test)]
#[path = "image_export_metadata_webp_tests.rs"]
mod webp_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::Image, image_container_metadata::FieldSummary, save_options::SaveEncoding,
    };
    use std::io::Cursor;

    const LINEAR: &[u8] = include_bytes!("../tests/data/image_color/linear-rgb.icc");

    fn pixels() -> Image {
        Image {
            width: 3,
            height: 2,
            bgra: vec![
                10, 30, 220, 255, 90, 20, 70, 128, 210, 100, 40, 0, 70, 190, 130, 64, 200, 230, 80,
                192, 30, 90, 220, 255,
            ],
            readback_ms: 0.,
        }
    }

    fn encode(format: SaveFormat) -> Vec<u8> {
        crate::export::encode_image(
            &pixels(),
            SaveEncoding {
                format,
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn rgba(bytes: &[u8]) -> Vec<u8> {
        image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .into_rgba8()
            .into_raw()
    }

    fn assumed() -> SourceMetadata {
        let bytes = encode(SaveFormat::Png);
        let inspected = crate::image_container_metadata::inspect(&bytes).unwrap();
        SourceMetadata::new(
            ManagedMetadata {
                source: inspected.summary,
                profile: None,
                color_conversion: ColorConversion::AssumedSrgb,
            },
            None,
        )
        .unwrap()
    }

    fn facts(profile: &[u8], conversion: ColorConversion) -> ManagedMetadata {
        let bytes = encode(SaveFormat::Jpeg);
        let mut source = crate::image_container_metadata::inspect(&bytes)
            .unwrap()
            .summary;
        source.icc = FieldSummary {
            status: MetadataStatus::Present,
            encoded_bytes: profile.len() as u64 + 14,
            decoded_bytes: Some(profile.len() as u64),
        };
        ManagedMetadata {
            source,
            profile: image_color_profile::inspect(profile).ok(),
            color_conversion: conversion,
        }
    }

    fn exif_fixture() -> Vec<u8> {
        fn entry(bytes: &mut [u8], offset: usize, tag: u16, kind: u16, count: u32, value: u32) {
            bytes[offset..offset + 2].copy_from_slice(&tag.to_le_bytes());
            bytes[offset + 2..offset + 4].copy_from_slice(&kind.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&count.to_le_bytes());
            bytes[offset + 8..offset + 12].copy_from_slice(&value.to_le_bytes());
        }
        let mut bytes = vec![0; 182];
        bytes[..8].copy_from_slice(b"II*\0\x08\0\0\0");
        bytes[8..10].copy_from_slice(&7u16.to_le_bytes());
        for (index, (tag, kind, count, value)) in [
            (0x0100, 4, 1, 99),
            (0x0101, 4, 1, 77),
            (0x010f, 2, 8, 164),
            (0x0112, 3, 1, 6),
            (0x8769, 4, 1, 98),
            (0x8825, 4, 1, 152),
            (0xc001, 7, 4, u32::from_le_bytes(*b"HIDE")),
        ]
        .into_iter()
        .enumerate()
        {
            entry(&mut bytes, 10 + index * 12, tag, kind, count, value);
        }
        bytes[94..98].copy_from_slice(&158u32.to_le_bytes());
        bytes[98..100].copy_from_slice(&4u16.to_le_bytes());
        for (index, (tag, kind, count, value)) in [
            (0x927c, 7, 10, 172),
            (0xa001, 3, 1, 65_535),
            (0xa002, 4, 1, 99),
            (0xa003, 4, 1, 77),
        ]
        .into_iter()
        .enumerate()
        {
            entry(&mut bytes, 100 + index * 12, tag, kind, count, value);
        }
        // GPS和缩略图目录为空但结构有效；两者都必须从白名单输出中排除。
        bytes[164..172].copy_from_slice(b"Example\0");
        bytes[172..182].copy_from_slice(b"private!!\0");
        bytes
    }

    fn with_supported_exif(mut facts: ManagedMetadata, icc: Option<&[u8]>) -> SourceMetadata {
        let tiff = exif_fixture();
        facts.source.exif = FieldSummary {
            status: MetadataStatus::Present,
            encoded_bytes: tiff.len() as u64,
            decoded_bytes: Some(tiff.len() as u64),
        };
        facts.source.orientation = Some(image::metadata::Orientation::Rotate90);
        SourceMetadata::new(facts, icc)
            .unwrap()
            .with_exif(crate::image_exif::inspect(&tiff, &AtomicBool::new(false)))
    }

    fn raw_exif(bytes: &[u8], format: SaveFormat) -> &[u8] {
        match format {
            SaveFormat::Png => {
                let mut position = 8;
                while position < bytes.len() {
                    let length = be32(&bytes[position..]) as usize;
                    if &bytes[position + 4..position + 8] == b"eXIf" {
                        return &bytes[position + 8..position + 8 + length];
                    }
                    position += length + 12;
                }
            }
            SaveFormat::Jpeg => {
                let mut position = 2;
                while bytes[position + 1] != 0xda {
                    let length = usize::from(u16::from_be_bytes([
                        bytes[position + 2],
                        bytes[position + 3],
                    ]));
                    if bytes[position + 1] == 0xe1 {
                        assert_eq!(&bytes[position + 4..position + 10], b"Exif\0\0");
                        return &bytes[position + 10..position + 2 + length];
                    }
                    position += length + 2;
                }
            }
            SaveFormat::Webp => {
                let mut position = 12;
                while position < bytes.len() {
                    let length = le32(&bytes[position + 4..]) as usize;
                    if &bytes[position..position + 4] == b"EXIF" {
                        return &bytes[position + 8..position + 8 + length];
                    }
                    position += length + 8 + (length & 1);
                }
            }
            _ => {}
        }
        panic!("EXIF is missing");
    }

    fn tiff_u16(bytes: &[u8], offset: usize) -> u16 {
        let value = bytes[offset..offset + 2].try_into().unwrap();
        if &bytes[..2] == b"II" {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        }
    }

    fn tiff_u32(bytes: &[u8], offset: usize) -> u32 {
        let value = bytes[offset..offset + 4].try_into().unwrap();
        if &bytes[..2] == b"II" {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        }
    }

    fn tiff_tag(bytes: &[u8], ifd: usize, wanted: u16) -> Option<(u16, usize)> {
        for index in 0..usize::from(tiff_u16(bytes, ifd)) {
            let entry = ifd + 2 + index * 12;
            if tiff_u16(bytes, entry) == wanted {
                let kind = tiff_u16(bytes, entry + 2);
                let width = match kind {
                    3 => 2,
                    4 => 4,
                    _ => 1,
                };
                let length = tiff_u32(bytes, entry + 4) as usize * width;
                let offset = if length <= 4 {
                    entry + 8
                } else {
                    tiff_u32(bytes, entry + 8) as usize
                };
                assert!(offset + length <= bytes.len());
                return Some((kind, offset));
            }
        }
        None
    }

    fn tiff_number(bytes: &[u8], ifd: usize, tag: u16) -> u32 {
        let (kind, offset) = tiff_tag(bytes, ifd, tag).expect("expected output TIFF tag");
        match kind {
            3 => u32::from(tiff_u16(bytes, offset)),
            4 => tiff_u32(bytes, offset),
            _ => panic!("expected SHORT or LONG"),
        }
    }

    #[test]
    fn image_export_metadata_color_only_roundtrips_real_encodings_without_changing_pixels() {
        let source = assumed();
        assert_eq!(source.retained_bytes(), 0);
        for format in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Webp] {
            let base = encode(format);
            let expected = rgba(&base);
            let output = apply(
                base.clone(),
                format,
                (3, 2),
                MetadataPolicy::ColorOnly,
                Some(&source),
                &AtomicBool::new(false),
            )
            .unwrap();
            let inspected = crate::image_container_metadata::inspect(&output).unwrap();
            assert_eq!(inspected.summary.icc.status, MetadataStatus::Present);
            assert_eq!(inspected.summary.exif.status, MetadataStatus::Absent);
            assert_eq!(inspected.icc_profile.as_deref(), Some(SRGB_PROFILE));
            assert_eq!(rgba(&output), expected);
            assert_eq!(
                image::ImageReader::new(Cursor::new(&output))
                    .with_guessed_format()
                    .unwrap()
                    .into_dimensions()
                    .unwrap(),
                (3, 2)
            );
            match format {
                SaveFormat::Png => {
                    let insertion = output.len() - base.len();
                    assert_eq!(&output[..33], &base[..33]);
                    assert_eq!(&output[33 + insertion..], &base[33..]);
                }
                SaveFormat::Jpeg => {
                    let position = match fresh_jpeg(&base, (3, 2), &AtomicBool::new(false)).unwrap()
                    {
                        Fresh::Jpeg { insertion } => insertion,
                        _ => unreachable!(),
                    };
                    assert_eq!(&output[..position], &base[..position]);
                    assert_eq!(
                        &output[position + output.len() - base.len()..],
                        &base[position..]
                    );
                }
                SaveFormat::Webp => {
                    assert_eq!(&output[12..16], b"VP8X");
                    assert_eq!(output[20] & 0x20, 0x20);
                    assert_eq!(u64::from(le32(&output[4..])) + 8, output.len() as u64);
                    assert_eq!(&output[output.len() - (base.len() - 12)..], &base[12..]);
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn image_export_metadata_converted_srgb_never_reembeds_the_source_profile() {
        for conversion in [
            ColorConversion::ConvertedSrgb,
            ColorConversion::ConvertedSrgb16,
        ] {
            let mut facts = facts(LINEAR, conversion);
            if conversion == ColorConversion::ConvertedSrgb16 {
                facts.source.bits_per_channel = Some(16);
            }
            let source = SourceMetadata::new(facts, Some(LINEAR)).unwrap();
            assert_eq!(source.facts(), facts);
            assert_eq!(source.retained_bytes(), 0);
            let output = apply(
                encode(SaveFormat::Png),
                SaveFormat::Png,
                (3, 2),
                MetadataPolicy::ColorOnly,
                Some(&source),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                crate::image_container_metadata::inspect(&output)
                    .unwrap()
                    .icc_profile
                    .as_deref(),
                Some(SRGB_PROFILE)
            );
        }
        assert_ne!(SRGB_PROFILE, LINEAR);
    }

    #[test]
    fn image_export_metadata_failed_rgb_conversion_keeps_only_the_matching_valid_source_icc() {
        let mut profile = LINEAR.to_vec();
        let facts = facts(
            &profile,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed),
        );
        let source = SourceMetadata::new(facts, Some(&profile)).unwrap();
        assert_eq!(source.retained_bytes(), LINEAR.len());
        profile.fill(0); // 上下文拥有副本，不借用可被改写的输入。
        for format in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Webp] {
            let base = encode(format);
            let expected = rgba(&base);
            let output = apply(
                base,
                format,
                (3, 2),
                MetadataPolicy::ColorOnly,
                Some(&source),
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                crate::image_container_metadata::inspect(&output)
                    .unwrap()
                    .icc_profile
                    .as_deref(),
                Some(LINEAR)
            );
            assert_eq!(rgba(&output), expected);
        }
        assert!(SourceMetadata::new(facts, Some(SRGB_PROFILE)).is_err());
        let missing = SourceMetadata::new(facts, None).unwrap();
        assert!(missing.profile().is_err());
    }

    #[test]
    fn image_export_metadata_jpeg_multisegment_icc_preserves_jfif_and_reassembles_exactly() {
        let mut large = LINEAR.to_vec();
        large.resize((JPEG_ICC_PART * 2 + 11) & !3, 0);
        let length = large.len() as u32;
        large[..4].copy_from_slice(&length.to_be_bytes());
        let source = SourceMetadata::new(
            facts(
                &large,
                ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed),
            ),
            Some(&large),
        )
        .unwrap();
        let base = encode(SaveFormat::Jpeg);
        let output = apply(
            base.clone(),
            SaveFormat::Jpeg,
            (3, 2),
            MetadataPolicy::ColorOnly,
            Some(&source),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(&output[..6], &base[..6]);
        assert_eq!(&output[..4], &[0xff, 0xd8, 0xff, 0xe0]);
        let mut position = 4 + usize::from(u16::from_be_bytes([output[4], output[5]]));
        for sequence in 1..=3 {
            assert_eq!(&output[position..position + 2], &[0xff, 0xe2]);
            let length = usize::from(u16::from_be_bytes([
                output[position + 2],
                output[position + 3],
            ]));
            assert_eq!(&output[position + 4..position + 16], b"ICC_PROFILE\0");
            assert_eq!(&output[position + 16..position + 18], &[sequence, 3]);
            if sequence < 3 {
                assert_eq!(length, 65_535);
            }
            position += length + 2;
        }
        assert_eq!(
            crate::image_container_metadata::inspect(&output)
                .unwrap()
                .icc_profile
                .as_deref(),
            Some(large.as_slice())
        );
        assert_eq!(rgba(&output), rgba(&base));
    }

    #[test]
    fn image_export_metadata_strip_is_default_keeps_the_allocation_and_needs_no_source() {
        assert_eq!(MetadataPolicy::default(), MetadataPolicy::Strip);
        for format in [
            SaveFormat::Png,
            SaveFormat::Jpeg,
            SaveFormat::Bmp,
            SaveFormat::Webp,
        ] {
            let encoded = encode(format);
            let pointer = encoded.as_ptr();
            let expected = encoded.clone();
            let output = apply(
                encoded,
                format,
                (3, 2),
                MetadataPolicy::Strip,
                None,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(output, expected);
            assert_eq!(output.as_ptr(), pointer);
        }
    }

    #[test]
    fn image_export_metadata_bad_unknown_cmyk_and_missing_profiles_allow_strip_but_reject_color() {
        let invalid_profile = b"bad ICC bytes";
        let invalid_facts = facts(
            invalid_profile,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::InvalidProfile),
        );
        let invalid_source = SourceMetadata::new(invalid_facts, Some(invalid_profile)).unwrap();
        assert_eq!(invalid_source.retained_bytes(), 0);
        let mut unknown = assumed().facts();
        unknown.source.icc.status = MetadataStatus::NotInspected;
        unknown.color_conversion = ColorConversion::NotInspected;
        let mut cmyk = facts(
            LINEAR,
            ColorConversion::EmbeddedUnapplied(UnappliedReason::UnsupportedSource),
        );
        cmyk.source.color_model = crate::image_container_metadata::inspect(include_bytes!(
            "../tests/data/image_decode/adobe_cmyk.jpg"
        ))
        .unwrap()
        .summary
        .color_model;
        assert_eq!(cmyk.source.color_model, ColorModel::Cmyk);
        let mut too_large = assumed().facts();
        too_large.source.icc.status = MetadataStatus::TooLarge;
        too_large.source.icc.decoded_bytes =
            Some(image_color_profile::MAX_PROFILE_BYTES as u64 + 1);
        too_large.color_conversion = ColorConversion::EmbeddedUnapplied(UnappliedReason::TooLarge);
        for source in [
            invalid_source,
            SourceMetadata::new(unknown, None).unwrap(),
            SourceMetadata::new(cmyk, Some(LINEAR)).unwrap(),
            SourceMetadata::new(too_large, None).unwrap(),
        ] {
            let encoded = encode(SaveFormat::Png);
            assert_eq!(
                apply(
                    encoded.clone(),
                    SaveFormat::Png,
                    (3, 2),
                    MetadataPolicy::Strip,
                    Some(&source),
                    &AtomicBool::new(false)
                )
                .unwrap(),
                encoded
            );
            assert!(apply(
                encoded,
                SaveFormat::Png,
                (3, 2),
                MetadataPolicy::ColorOnly,
                Some(&source),
                &AtomicBool::new(false)
            )
            .is_err());
        }
    }

    #[test]
    fn image_export_metadata_hdr_mapping_attaches_target_srgb_without_changing_pixels() {
        let mut metadata = assumed().facts();
        metadata.source.color_model = ColorModel::Rgb;
        metadata.source.bits_per_channel = Some(10);
        metadata.source.orientation = image::metadata::Orientation::from_exif(6);
        metadata.color_conversion = ColorConversion::HdrMappedSrgb;
        let source = SourceMetadata::new(metadata, None).unwrap();
        assert_eq!(source.retained_bytes(), 0);
        assert_eq!(source.profile().unwrap(), SRGB_PROFILE);
        assert_eq!(source.facts().source.bits_per_channel, Some(10));
        let input = encode(SaveFormat::Png);
        let expected_pixels = rgba(&input);
        let output = apply(
            input,
            SaveFormat::Png,
            (3, 2),
            MetadataPolicy::ColorOnly,
            Some(&source),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(rgba(&output), expected_pixels);
        let inspected = crate::image_container_metadata::inspect(&output).unwrap();
        assert_eq!(inspected.icc_profile.as_deref(), Some(SRGB_PROFILE));
        assert_eq!(inspected.summary.bits_per_channel, Some(8));
        assert_eq!(inspected.summary.exif.status, MetadataStatus::Absent);
    }

    #[test]
    fn image_export_metadata_hdr_mapping_rejects_unknown_or_conflicting_source_facts() {
        let mut valid = assumed().facts();
        valid.source.color_model = ColorModel::Rgb;
        valid.source.bits_per_channel = Some(10);
        valid.color_conversion = ColorConversion::HdrMappedSrgb;
        let mut unknown_precision = valid;
        unknown_precision.source.bits_per_channel = None;
        let mut eight_bit = valid;
        eight_bit.source.bits_per_channel = Some(8);
        let mut unknown_icc = valid;
        unknown_icc.source.icc.status = MetadataStatus::NotInspected;
        let mut bad_icc = valid;
        bad_icc.source.icc.status = MetadataStatus::Malformed;
        let mut gray = valid;
        gray.source.color_model = ColorModel::Gray;
        let with_icc = facts(LINEAR, ColorConversion::HdrMappedSrgb);
        for metadata in [
            unknown_precision,
            eight_bit,
            unknown_icc,
            bad_icc,
            gray,
            with_icc,
        ] {
            let source = SourceMetadata::new(metadata, None).unwrap();
            assert!(source.profile().is_err());
            let encoded = encode(SaveFormat::Png);
            assert_eq!(
                apply(
                    encoded.clone(),
                    SaveFormat::Png,
                    (3, 2),
                    MetadataPolicy::Strip,
                    Some(&source),
                    &AtomicBool::new(false),
                )
                .unwrap(),
                encoded,
            );
            assert!(apply(
                encoded,
                SaveFormat::Png,
                (3, 2),
                MetadataPolicy::ColorOnly,
                Some(&source),
                &AtomicBool::new(false),
            )
            .is_err());
        }
    }

    #[test]
    fn image_export_metadata_vector_render_uses_target_srgb_and_no_source_exif() {
        let mut metadata = assumed().facts();
        metadata.source.color_model = ColorModel::Rgb;
        metadata.source.bits_per_channel = None;
        metadata.source.orientation = None;
        metadata.color_conversion = ColorConversion::RenderedSrgb;
        let source = SourceMetadata::new(metadata, None).unwrap();
        assert_eq!(source.profile().unwrap(), SRGB_PROFILE);
        let encoded = encode(SaveFormat::Png);
        let expected = rgba(&encoded);
        let output = apply(
            encoded,
            SaveFormat::Png,
            (3, 2),
            MetadataPolicy::ColorOnly,
            Some(&source),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(rgba(&output), expected);
        let inspected = crate::image_container_metadata::inspect(&output).unwrap();
        assert_eq!(inspected.icc_profile.as_deref(), Some(SRGB_PROFILE));
        assert_eq!(inspected.summary.exif.status, MetadataStatus::Absent);
        assert!(apply(
            encode(SaveFormat::Png),
            SaveFormat::Png,
            (3, 2),
            MetadataPolicy::ColorAndSupportedExif,
            Some(&source),
            &AtomicBool::new(false)
        )
        .is_err());
        for (model, precision) in [
            (ColorModel::Unknown, None),
            (ColorModel::Gray, None),
            (ColorModel::Rgb, Some(8)),
            (ColorModel::Rgb, Some(16)),
        ] {
            let mut invalid = metadata;
            invalid.source.color_model = model;
            invalid.source.bits_per_channel = precision;
            assert!(SourceMetadata::new(invalid, None)
                .unwrap()
                .profile()
                .is_err());
        }
    }

    #[test]
    fn image_export_metadata_validates_fresh_format_dimensions_and_cancellation_before_mutation() {
        let source = assumed();
        for format in [
            SaveFormat::Png,
            SaveFormat::Jpeg,
            SaveFormat::Bmp,
            SaveFormat::Webp,
        ] {
            assert!(apply(
                encode(format),
                format,
                (2, 3),
                MetadataPolicy::Strip,
                None,
                &AtomicBool::new(false)
            )
            .is_err());
            assert!(apply(
                encode(format),
                format,
                (3, 2),
                MetadataPolicy::Strip,
                None,
                &AtomicBool::new(true)
            )
            .is_err());
            assert!(apply(
                encode(format),
                format,
                (3, 2),
                MetadataPolicy::ColorOnly,
                None,
                &AtomicBool::new(false)
            )
            .is_err());
        }
        assert!(apply(
            encode(SaveFormat::Bmp),
            SaveFormat::Bmp,
            (3, 2),
            MetadataPolicy::ColorOnly,
            Some(&source),
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(apply(
            encode(SaveFormat::Png),
            SaveFormat::Jpeg,
            (3, 2),
            MetadataPolicy::Strip,
            None,
            &AtomicBool::new(false)
        )
        .is_err());
        let tagged = apply(
            encode(SaveFormat::Png),
            SaveFormat::Png,
            (3, 2),
            MetadataPolicy::ColorOnly,
            Some(&source),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(apply(
            tagged,
            SaveFormat::Png,
            (3, 2),
            MetadataPolicy::Strip,
            None,
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(expanded_length(MAX_ENCODED_BYTES, 1).is_err());
        assert!(expanded_length(usize::MAX, 1).is_err());
        assert_eq!(
            expanded_length(MAX_ENCODED_BYTES - 1, 1).unwrap(),
            MAX_ENCODED_BYTES
        );
        let mut original = vec![1, 2, 3];
        assert!(insert(&mut original, 0, &[0], &AtomicBool::new(true)).is_err());
        assert_eq!(original, [1, 2, 3]);
    }

    #[test]
    fn image_export_metadata_many_small_png_pixel_chunks_do_not_consume_metadata_quota() {
        let mut seed = 0x1234_5678u32;
        let bgra = (0..64 * 64 * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect();
        let base = crate::export::encode_image(
            &Image {
                width: 64,
                height: 64,
                bgra,
                readback_ms: 0.,
            },
            SaveEncoding::default(),
        )
        .unwrap();
        let mut split = PNG_SIGNATURE.to_vec();
        let mut position = 8;
        let mut idat_count = 0;
        while position < base.len() {
            let length = be32(&base[position..]) as usize;
            let end = position + 12 + length;
            if &base[position + 4..position + 8] == b"IDAT" {
                for byte in &base[position + 8..end - 4] {
                    split.extend_from_slice(&1u32.to_be_bytes());
                    let data = [b'I', b'D', b'A', b'T', *byte];
                    split.extend_from_slice(&data);
                    split.extend_from_slice(&crc(&data).to_be_bytes());
                    idat_count += 1;
                }
            } else {
                split.extend_from_slice(&base[position..end]);
            }
            position = end;
        }
        assert!(idat_count > MAX_PARTS);
        let expected = rgba(&base);
        assert_eq!(rgba(&split), expected);
        let source = assumed();
        let output = apply(
            split,
            SaveFormat::Png,
            (64, 64),
            MetadataPolicy::ColorOnly,
            Some(&source),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(rgba(&output), expected);
    }

    #[test]
    fn image_export_metadata_supported_exif_real_formats_rebuild_direction_dimensions_and_safe_fields(
    ) {
        let source = with_supported_exif(assumed().facts(), None);
        let summary = source.exif_summary().expect("valid supported EXIF fixture");
        assert_eq!(summary.retained_fields, 1);
        assert!(summary.dropped_fields > 0);
        assert!(summary.excluded_gps && summary.excluded_thumbnail && summary.excluded_private);
        assert!(source.retained_bytes() > 0);
        assert!(source.exif_error().is_none());
        for format in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Webp] {
            let base = encode(format);
            let output = apply(
                base.clone(),
                format,
                (3, 2),
                MetadataPolicy::ColorAndSupportedExif,
                Some(&source),
                &AtomicBool::new(false),
            )
            .unwrap();
            let inspected = crate::image_container_metadata::inspect(&output).unwrap();
            assert_eq!(inspected.icc_profile.as_deref(), Some(SRGB_PROFILE));
            assert_eq!(inspected.summary.exif.status, MetadataStatus::Present);
            assert_eq!(inspected.summary.orientation.unwrap().to_exif(), 1);
            assert_eq!(rgba(&output), rgba(&base));
            let tiff = raw_exif(&output, format);
            let first = tiff_u32(tiff, 4) as usize;
            assert_eq!(tiff_number(tiff, first, 0x0112), 1);
            assert!(tiff_tag(tiff, first, 0x0100).is_none());
            assert!(tiff_tag(tiff, first, 0x0101).is_none());
            let (_, make) = tiff_tag(tiff, first, 0x010f).unwrap();
            assert_eq!(&tiff[make..make + 8], b"Example\0");
            assert!(tiff_tag(tiff, first, 0x8825).is_none());
            assert!(tiff_tag(tiff, first, 0xc001).is_none());
            assert_eq!(
                tiff_u32(tiff, first + 2 + usize::from(tiff_u16(tiff, first)) * 12),
                0
            );
            let photo = tiff_number(tiff, first, 0x8769) as usize;
            assert_eq!(tiff_number(tiff, photo, 0xa001), 1);
            assert_eq!(tiff_number(tiff, photo, 0xa002), 3);
            assert_eq!(tiff_number(tiff, photo, 0xa003), 2);
            assert!(tiff_tag(tiff, photo, 0x927c).is_none());
            assert!(!tiff.windows(9).any(|bytes| bytes == b"private!!"));
            match format {
                SaveFormat::Png => {
                    assert_eq!(&output[output.len() - base.len() + 33..], &base[33..]);
                    assert_eq!(&output[37..41], b"iCCP");
                }
                SaveFormat::Jpeg => {
                    let insertion =
                        match fresh_jpeg(&base, (3, 2), &AtomicBool::new(false)).unwrap() {
                            Fresh::Jpeg { insertion } => insertion,
                            _ => unreachable!(),
                        };
                    assert_eq!(&output[insertion..insertion + 2], &[0xff, 0xe1]);
                    assert_eq!(
                        &output[insertion + output.len() - base.len()..],
                        &base[insertion..]
                    );
                }
                SaveFormat::Webp => {
                    let mut kinds = Vec::new();
                    let mut position = 12;
                    while position < output.len() {
                        kinds.push(&output[position..position + 4]);
                        let length = le32(&output[position + 4..]) as usize;
                        position += 8 + length + (length & 1);
                    }
                    assert_eq!(
                        kinds,
                        [
                            b"VP8X".as_slice(),
                            b"ICCP".as_slice(),
                            b"VP8L".as_slice(),
                            b"EXIF".as_slice()
                        ]
                    );
                    assert_eq!(output[20] & 0x28, 0x28);
                    assert_eq!(le32(&output[4..]) as usize + 8, output.len());
                    let image_start = 30 + 8 + SRGB_PROFILE.len() + (SRGB_PROFILE.len() & 1);
                    assert_eq!(
                        &output[image_start..image_start + base.len() - 12],
                        &base[12..]
                    );
                }
                _ => unreachable!(),
            }
            for policy in [MetadataPolicy::Strip, MetadataPolicy::ColorOnly] {
                let output = apply(
                    base.clone(),
                    format,
                    (3, 2),
                    policy,
                    Some(&source),
                    &AtomicBool::new(false),
                )
                .unwrap();
                let inspected = crate::image_container_metadata::inspect(&output).unwrap();
                assert_eq!(inspected.summary.exif.status, MetadataStatus::Absent);
                assert_eq!(
                    inspected.summary.icc.status,
                    if policy == MetadataPolicy::Strip {
                        MetadataStatus::Absent
                    } else {
                        MetadataStatus::Present
                    }
                );
            }
        }
    }

    #[test]
    fn image_export_metadata_supported_exif_color_space_matches_the_actual_output_profile() {
        for (conversion, expected_profile, expected_color) in [
            (ColorConversion::ConvertedSrgb, SRGB_PROFILE, 1),
            (
                ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed),
                LINEAR,
                65_535,
            ),
        ] {
            let source = with_supported_exif(facts(LINEAR, conversion), Some(LINEAR));
            for format in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Webp] {
                let output = apply(
                    encode(format),
                    format,
                    (3, 2),
                    MetadataPolicy::ColorAndSupportedExif,
                    Some(&source),
                    &AtomicBool::new(false),
                )
                .unwrap();
                let inspected = crate::image_container_metadata::inspect(&output).unwrap();
                assert_eq!(inspected.icc_profile.as_deref(), Some(expected_profile));
                let tiff = raw_exif(&output, format);
                let first = tiff_u32(tiff, 4) as usize;
                let photo = tiff_number(tiff, first, 0x8769) as usize;
                assert_eq!(tiff_number(tiff, photo, 0xa001), expected_color);
            }
        }
    }

    #[test]
    fn image_export_metadata_bad_or_missing_exif_only_disables_explicit_exif_preservation() {
        let missing = assumed();
        let failed = assumed().with_exif(crate::image_exif::inspect(
            b"bad source",
            &AtomicBool::new(false),
        ));
        assert!(failed.exif_summary().is_none());
        assert!(failed.exif_error().is_some());
        assert_eq!(
            failed.retained_bytes(),
            failed.exif_error.as_ref().unwrap().capacity()
        );
        let mut parsed = with_supported_exif(assumed().facts(), None);
        assert!(parsed.exif_summary().is_some());
        let mut reason = String::with_capacity(4096);
        reason.push_str("bad EXIF origin");
        let error_capacity = reason.capacity();
        assert!(error_capacity > reason.len());
        parsed = parsed.with_exif(Err(reason));
        assert!(parsed.exif_summary().is_none());
        assert_eq!(parsed.exif_error(), Some("bad EXIF origin"));
        assert_eq!(parsed.retained_bytes(), error_capacity);
        for source in [&missing, &failed, &parsed] {
            for format in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Webp] {
                assert!(apply(
                    encode(format),
                    format,
                    (3, 2),
                    MetadataPolicy::ColorAndSupportedExif,
                    Some(source),
                    &AtomicBool::new(false)
                )
                .is_err());
                for policy in [MetadataPolicy::Strip, MetadataPolicy::ColorOnly] {
                    assert!(apply(
                        encode(format),
                        format,
                        (3, 2),
                        policy,
                        Some(source),
                        &AtomicBool::new(false)
                    )
                    .is_ok());
                }
            }
        }
        let valid = with_supported_exif(assumed().facts(), None);
        for format in [SaveFormat::Png, SaveFormat::Jpeg, SaveFormat::Webp] {
            assert!(apply(
                encode(format),
                format,
                (3, 2),
                MetadataPolicy::ColorAndSupportedExif,
                Some(&valid),
                &AtomicBool::new(true)
            )
            .is_err());
        }
        assert!(apply(
            encode(SaveFormat::Bmp),
            SaveFormat::Bmp,
            (3, 2),
            MetadataPolicy::ColorAndSupportedExif,
            Some(&valid),
            &AtomicBool::new(false)
        )
        .is_err());
    }

    #[test]
    fn image_export_metadata_exif_container_lengths_and_padding_are_bounded() {
        let mut jpeg = Vec::new();
        append_jpeg_exif(&mut jpeg, &vec![0; 65_527]).unwrap();
        assert_eq!(&jpeg[..4], &[0xff, 0xe1, 0xff, 0xff]);
        assert_eq!(jpeg.len(), 65_537);
        let mut refused = vec![1, 2, 3];
        assert!(append_jpeg_exif(&mut refused, &vec![0; 65_528]).is_err());
        assert_eq!(refused, [1, 2, 3]);
        let mut webp = encode(SaveFormat::Webp);
        let start = webp.len();
        append_webp_exif(&mut webp, b"odd", &AtomicBool::new(false)).unwrap();
        assert_eq!(&webp[start..], b"EXIF\x03\0\0\0odd\0");
        let mut canceled = encode(SaveFormat::Webp);
        let original = canceled.clone();
        assert!(append_webp_exif(&mut canceled, b"odd", &AtomicBool::new(true)).is_err());
        assert_eq!(canceled, original);
    }
}
