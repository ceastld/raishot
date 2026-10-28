//! 有界ICO/CUR目录及按需尺寸读入；来源保留同一reader，不缓存各尺寸的像素。
use crate::image_decode::{DecodedImage, MAX_DECODE_BYTES};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_ENTRIES: usize = 256;
const SINGLE_HEADER_BYTES: u64 = 22;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContainerKind {
    Icon,
    Cursor,
}

impl ContainerKind {
    fn code(self) -> u16 {
        match self {
            Self::Icon => 1,
            Self::Cursor => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IcoEntry {
    pub index: usize,
    pub dimensions: (u32, u32),
    /// 目录声明值；0表示未声明，不据此宣称源像素精度。
    pub bits_per_pixel: u16,
    pub encoded_bytes: u64,
    /// CUR原图中的热点；仅作属性保留，不平移或改写图像像素。
    pub hotspot: Option<(u16, u16)>,
    offset: u64,
    raw: [u8; 16],
    error: Option<String>,
}

impl IcoEntry {
    fn score(&self) -> (u16, u64) {
        (
            self.bits_per_pixel,
            u64::from(self.dimensions.0) * u64::from(self.dimensions.1),
        )
    }
}

pub(crate) struct IcoSource<R: Read + Seek> {
    reader: R,
    start: u64,
    length: u64,
    kind: ContainerKind,
    entries: Vec<IcoEntry>,
}

fn message(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn cancelled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        // read_exact会重试Interrupted；取消必须使用不可重试的错误。
        Err(io::Error::other(tr!(
            "ICO读取已取消",
            "ICO reading canceled"
        )))
    } else {
        Ok(())
    }
}

fn seek_checked(
    reader: &mut impl Seek,
    position: SeekFrom,
    cancel: &AtomicBool,
) -> io::Result<u64> {
    cancelled(cancel)?;
    let position = reader.seek(position)?;
    cancelled(cancel)?;
    Ok(position)
}

fn read_checked(reader: &mut impl Read, output: &mut [u8], cancel: &AtomicBool) -> io::Result<()> {
    for mut part in output.chunks_mut(64 * 1024) {
        while !part.is_empty() {
            cancelled(cancel)?;
            match reader.read(part) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        tr!("ICO来源被截断", "Truncated ICO source"),
                    ));
                }
                Ok(count) => part = &mut part[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        cancelled(cancel)?;
    }
    cancelled(cancel)
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

fn dimension(value: u8) -> u32 {
    if value == 0 {
        256
    } else {
        u32::from(value)
    }
}

impl IcoSource<File> {
    pub(crate) fn open(path: &Path, cancel: &AtomicBool) -> Result<Self, String> {
        Self::open_kind(path, ContainerKind::Icon, cancel)
    }

    pub(crate) fn open_cursor(path: &Path, cancel: &AtomicBool) -> Result<Self, String> {
        Self::open_kind(path, ContainerKind::Cursor, cancel)
    }

    fn open_kind(path: &Path, kind: ContainerKind, cancel: &AtomicBool) -> Result<Self, String> {
        cancelled(cancel).map_err(|error| error.to_string())?;
        crate::image_collection::ensure_local_file(path)?;
        cancelled(cancel).map_err(|error| error.to_string())?;
        let file = File::open(path).map_err(|error| error.to_string())?;
        Self::from_reader_kind(file, kind, cancel)
    }
}

impl<R: Read + Seek> IcoSource<R> {
    /// 容器从reader当前位置开始，目录偏移相对于该位置；来源长度只在此处取得。
    pub(crate) fn from_reader(reader: R, cancel: &AtomicBool) -> Result<Self, String> {
        Self::from_reader_kind(reader, ContainerKind::Icon, cancel)
    }

    pub(crate) fn from_cursor_reader(reader: R, cancel: &AtomicBool) -> Result<Self, String> {
        Self::from_reader_kind(reader, ContainerKind::Cursor, cancel)
    }

    fn from_reader_kind(
        mut reader: R,
        kind: ContainerKind,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let start = seek_checked(&mut reader, SeekFrom::Current(0), cancel)
            .map_err(|error| error.to_string())?;
        let end = seek_checked(&mut reader, SeekFrom::End(0), cancel)
            .map_err(|error| error.to_string())?;
        let length = end
            .checked_sub(start)
            .ok_or_else(|| message("ICO来源范围无效", "Invalid ICO source range"))?;
        if length > MAX_DECODE_BYTES {
            return Err(message(
                "ICO文件超过256MiB读入上限",
                "ICO file exceeds the 256 MiB input limit",
            ));
        }
        if length < 6 {
            return Err(message("ICO目录头被截断", "Truncated ICO directory header"));
        }
        seek_checked(&mut reader, SeekFrom::Start(start), cancel)
            .map_err(|error| error.to_string())?;
        let mut header = [0; 6];
        read_checked(&mut reader, &mut header, cancel).map_err(|error| error.to_string())?;
        let count = usize::from(le16(&header[4..]));
        if le16(&header) != 0
            || le16(&header[2..]) != kind.code()
            || !(1..=MAX_ENTRIES).contains(&count)
        {
            return Err(message(
                "图标或光标目录头无效，仅支持1至256项",
                "Invalid icon or cursor directory; expected 1 to 256 entries",
            ));
        }
        let directory_end = (count as u64)
            .checked_mul(16)
            .and_then(|bytes| bytes.checked_add(6))
            .filter(|end| *end <= length)
            .ok_or_else(|| message("ICO目录被截断", "Truncated ICO directory"))?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(count)
            .map_err(|_| message("ICO目录分配失败", "ICO directory allocation failed"))?;
        for index in 0..count {
            let mut raw = [0; 16];
            read_checked(&mut reader, &mut raw, cancel).map_err(|error| error.to_string())?;
            let encoded_bytes = u64::from(le32(&raw[8..]));
            let offset = u64::from(le32(&raw[12..]));
            let dimensions = (dimension(raw[0]), dimension(raw[1]));
            let hotspot =
                (kind == ContainerKind::Cursor).then(|| (le16(&raw[4..]), le16(&raw[6..])));
            let bits_per_pixel = if kind == ContainerKind::Icon {
                le16(&raw[6..])
            } else {
                0
            };
            let error = if raw[3] != 0
                || (kind == ContainerKind::Icon && (le16(&raw[4..]) > 256 || bits_per_pixel > 256))
                || hotspot.is_some_and(|(x, y)| {
                    u32::from(x) >= dimensions.0 || u32::from(y) >= dimensions.1
                })
                || encoded_bytes == 0
                || offset < directory_end
                || offset
                    .checked_add(encoded_bytes)
                    .is_none_or(|end| end > length)
            {
                Some(message(
                    "图标或光标目录项范围或声明无效",
                    "Invalid icon or cursor entry range or declaration",
                ))
            } else {
                None
            };
            entries.push(IcoEntry {
                index,
                dimensions,
                bits_per_pixel,
                encoded_bytes,
                hotspot,
                offset,
                raw,
                error,
            });
        }
        Ok(Self {
            reader,
            start,
            length,
            kind,
            entries,
        })
    }

    pub(crate) fn entries(&self) -> &[IcoEntry] {
        &self.entries
    }

    pub(crate) fn kind(&self) -> ContainerKind {
        self.kind
    }

    /// ICO位深优先、其次面积；CUR位深固定为0，按面积选择，热点不参与评分。
    /// 平局保留统一ICO解码器原选择器的顺序。
    pub(crate) fn default_index(&self) -> usize {
        let mut best = self.entries.last().unwrap();
        for entry in &self.entries[..self.entries.len() - 1] {
            if entry.score() > best.score() {
                best = entry;
            }
        }
        best.index
    }

    /// 每次重新定位至所选负载，坏项或取消不使其它目录项失效。
    pub(crate) fn decode(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<DecodedImage, String> {
        self.decode_with_input_reservation(index, max_output_bytes, cancel, 0)
    }

    /// 借用内存容器时，外层压缩来源仍驻留；File来源可传0。
    pub(crate) fn decode_with_input_reservation(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        cancel: &AtomicBool,
        source_reserved_bytes: u64,
    ) -> Result<DecodedImage, String> {
        self.decode_with_budget(
            index,
            max_output_bytes,
            cancel,
            source_reserved_bytes,
            false,
        )
    }

    pub(crate) fn decode_with_shared_input_reservation(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        cancel: &AtomicBool,
        source_reserved_bytes: u64,
    ) -> Result<DecodedImage, String> {
        self.decode_with_budget(index, max_output_bytes, cancel, source_reserved_bytes, true)
    }

    fn decode_with_budget(
        &mut self,
        index: usize,
        max_output_bytes: u64,
        cancel: &AtomicBool,
        source_reserved_bytes: u64,
        shared: bool,
    ) -> Result<DecodedImage, String> {
        cancelled(cancel).map_err(|error| error.to_string())?;
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| message("ICO尺寸索引越界", "ICO size index is out of range"))?;
        if let Some(error) = &entry.error {
            return Err(error.clone());
        }
        let output_bytes = u64::from(entry.dimensions.0) * u64::from(entry.dimensions.1) * 4;
        if output_bytes > max_output_bytes {
            return Err(message(
                "ICO图像超过当前读取预算",
                "ICO image exceeds the current output budget",
            ));
        }
        let encoded_size = entry
            .encoded_bytes
            .checked_add(SINGLE_HEADER_BYTES)
            .filter(|bytes| *bytes <= MAX_DECODE_BYTES)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| {
                message(
                    "ICO所选负载超过256MiB上限",
                    "Selected ICO payload exceeds 256 MiB",
                )
            })?;
        self.input_reservation(source_reserved_bytes, encoded_size)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded_size)
            .map_err(|_| message("ICO负载分配失败", "ICO payload allocation failed"))?;
        let input_reserved_bytes =
            self.input_reservation(source_reserved_bytes, bytes.capacity())?;
        bytes.resize(encoded_size, 0);
        cancelled(cancel).map_err(|error| error.to_string())?;
        bytes[..6].copy_from_slice(&[0, 0, 1, 0, 1, 0]);
        bytes[6..22].copy_from_slice(&entry.raw);
        if self.kind == ContainerKind::Cursor {
            // CUR目录这四字节是热点，不能当成ICO的planes/位深交给解码器。
            bytes[10..12].copy_from_slice(&1u16.to_le_bytes());
            bytes[12..14].copy_from_slice(&0u16.to_le_bytes());
        }
        bytes[18..22].copy_from_slice(&(SINGLE_HEADER_BYTES as u32).to_le_bytes());
        let position = self
            .start
            .checked_add(entry.offset)
            .ok_or_else(|| message("ICO负载位置溢出", "ICO payload position overflow"))?;
        seek_checked(&mut self.reader, SeekFrom::Start(position), cancel)
            .map_err(|error| error.to_string())?;
        read_checked(&mut self.reader, &mut bytes[22..], cancel)
            .map_err(|error| error.to_string())?;
        validate_payload_dimensions(&bytes[22..], entry.dimensions)?;
        if self.kind == ContainerKind::Cursor {
            validate_cursor_payload(&bytes[22..])?;
        }
        cancelled(cancel).map_err(|error| error.to_string())?;
        // PNG负载先走统一元数据预检；图标尺寸/热点由目录决定，不应用嵌入EXIF方向。
        let decoded = if bytes[22..].starts_with(PNG_SIGNATURE) {
            let decode = if shared {
                crate::image_decode::decode_icon_png_with_shared_reservation
            } else {
                crate::image_decode::decode_icon_png_with_reservation
            };
            decode(&bytes[22..], input_reserved_bytes, max_output_bytes, cancel)
        } else {
            let decode = if shared {
                crate::image_decode::decode_icon_dib_with_shared_reservation
            } else {
                crate::image_decode::decode_icon_dib_with_reservation
            };
            decode(&bytes, input_reserved_bytes, max_output_bytes, cancel)
        };
        cancelled(cancel).map_err(|error| error.to_string())?;
        let mut decoded = decoded.map_err(|error| error.to_string())?;
        if decoded.metadata.encoded_dimensions != Some(entry.dimensions)
            || decoded.metadata.display_dimensions != entry.dimensions
            || (decoded.image.width, decoded.image.height) != entry.dimensions
        {
            return Err(message(
                "ICO图像尺寸与目录不一致",
                "ICO image dimensions do not match its directory",
            ));
        }
        restore_zero_rgb_alpha(&bytes[22..], &mut decoded.image, cancel)?;
        decoded.metadata.encoded_bytes = Some(self.length);
        decoded.metadata.format = image::ImageFormat::Ico.into();
        Ok(decoded)
    }

    fn input_reservation(&self, source: u64, virtual_capacity: usize) -> Result<u64, String> {
        let allocation = || {
            message(
                "ICO输入驻留量超过256MiB预算",
                "ICO input storage exceeds the 256 MiB budget",
            )
        };
        let directory = self
            .entries
            .capacity()
            .checked_mul(std::mem::size_of::<IcoEntry>())
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(allocation)?;
        let mut bytes = source
            .checked_add(directory)
            .and_then(|bytes| bytes.checked_add(u64::try_from(virtual_capacity).ok()?))
            .filter(|bytes| *bytes <= MAX_DECODE_BYTES)
            .ok_or_else(allocation)?;
        for entry in &self.entries {
            if let Some(reason) = &entry.error {
                bytes = bytes
                    .checked_add(u64::try_from(reason.capacity()).map_err(|_| allocation())?)
                    .filter(|bytes| *bytes <= MAX_DECODE_BYTES)
                    .ok_or_else(allocation)?;
            }
        }
        Ok(bytes)
    }
}

fn validate_cursor_payload(payload: &[u8]) -> Result<(), String> {
    let supported = if payload.starts_with(PNG_SIGNATURE) {
        payload.len() >= 33 && payload[24] == 8 && payload[25] == 6
    } else {
        payload.len() >= 40
            && matches!(le32(payload), 40 | 52 | 56 | 108 | 124)
            && le16(&payload[14..]) == 32
            && le32(&payload[16..]) == 0
    };
    if supported {
        Ok(())
    } else {
        Err(message(
            "CUR仅支持8位RGBA PNG或无压缩32位DIB；其它光标像素操作暂不支持",
            "CUR supports RGBA8 PNG or uncompressed 32-bit DIB only; other cursor pixel operations are unsupported",
        ))
    }
}

fn restore_zero_rgb_alpha(
    payload: &[u8],
    image: &mut crate::capture::Image,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if payload.starts_with(PNG_SIGNATURE) || payload.len() < 40 {
        return Ok(());
    }
    let header = le32(payload) as usize;
    if !matches!(header, 40 | 52 | 56 | 108 | 124)
        || le16(&payload[14..]) != 32
        || le32(&payload[16..]) != 0
    {
        return Ok(());
    }
    let width = image.width as usize;
    let height = image.height as usize;
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| message("ICO像素行长度溢出", "ICO pixel row length overflow"))?;
    let xor_bytes = row_bytes
        .checked_mul(height)
        .ok_or_else(|| message("ICO像素范围溢出", "ICO pixel range overflow"))?;
    let xor_end = header
        .checked_add(xor_bytes)
        .ok_or_else(|| message("ICO像素范围溢出", "ICO pixel range overflow"))?;
    let xor = payload
        .get(header..xor_end)
        .ok_or_else(|| message("ICO像素负载被截断", "Truncated ICO pixel payload"))?;
    for row in xor.chunks_exact(row_bytes) {
        cancelled(cancel).map_err(|error| error.to_string())?;
        if row.chunks_exact(4).any(|pixel| pixel[3] != 0) {
            return Ok(());
        }
    }
    // 只有无颜色表/外置配置的0RGB布局可无歧义地恢复alpha；不猜测附加数据的位置。
    if le32(&payload[32..]) != 0
        || (header >= 124 && (le32(&payload[112..]) != 0 || le32(&payload[116..]) != 0))
    {
        return Err(message(
            "ICO零alpha兼容格式不支持附加颜色表或配置",
            "Zero-alpha ICO compatibility does not support extra color tables or profiles",
        ));
    }
    let mask_stride = width
        .checked_add(31)
        .map(|width| width / 32)
        .and_then(|words| words.checked_mul(4))
        .ok_or_else(|| message("ICO掩码行长度溢出", "ICO mask row length overflow"))?;
    let mask_bytes = mask_stride
        .checked_mul(height)
        .ok_or_else(|| message("ICO掩码范围溢出", "ICO mask range overflow"))?;
    let tail = &payload[xor_end..];
    let mask = if tail.is_empty() {
        None
    } else {
        Some(
            tail.get(..mask_bytes)
                .ok_or_else(|| message("ICO的AND掩码被截断", "Truncated ICO AND mask"))?,
        )
    };
    if image.bgra.len() != xor_bytes {
        return Err(message(
            "ICO像素缓冲长度不一致",
            "ICO pixel buffer length mismatch",
        ));
    }
    // 正高度DIB的XOR与AND都从最底行开始；显示缓冲已归一为从顶行开始。
    for (y, row) in image.bgra.chunks_exact_mut(row_bytes).enumerate() {
        cancelled(cancel).map_err(|error| error.to_string())?;
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let transparent = mask.is_some_and(|mask| {
                mask[(height - 1 - y) * mask_stride + x / 8] & (0x80 >> (x % 8)) != 0
            });
            pixel[3] = if transparent { 0 } else { 255 };
        }
    }
    Ok(())
}

fn validate_payload_dimensions(payload: &[u8], expected: (u32, u32)) -> Result<(), String> {
    let actual = if payload.starts_with(PNG_SIGNATURE) {
        if payload.len() < 33
            || payload[8..12] != 13u32.to_be_bytes()
            || &payload[12..16] != b"IHDR"
        {
            return Err(message("ICO内PNG头无效", "Invalid PNG header inside ICO"));
        }
        (
            u32::from_be_bytes(payload[16..20].try_into().unwrap()),
            u32::from_be_bytes(payload[20..24].try_into().unwrap()),
        )
    } else {
        if payload.len() < 4 {
            return Err(message("ICO图像负载被截断", "Truncated ICO image payload"));
        }
        let header_size = le32(payload);
        if !matches!(header_size, 12 | 40 | 52 | 56 | 108 | 124)
            || payload.len() < header_size as usize
        {
            return Err(message(
                "ICO内DIB头不受支持或被截断",
                "Unsupported or truncated ICO DIB header",
            ));
        }
        let (width, height) = if header_size == 12 {
            (
                u32::from(le16(&payload[4..])),
                u32::from(le16(&payload[6..])),
            )
        } else {
            let width = i32::from_le_bytes(payload[4..8].try_into().unwrap());
            let height = i32::from_le_bytes(payload[8..12].try_into().unwrap());
            if width <= 0 || height <= 0 {
                return Err(message(
                    "ICO内DIB须为正向尺寸",
                    "ICO DIB dimensions must be positive",
                ));
            }
            (width as u32, height as u32)
        };
        if height == 0 || height % 2 != 0 {
            return Err(message(
                "ICO内DIB高度必须为正偶数",
                "ICO DIB height must be positive and even",
            ));
        }
        (width, height / 2)
    };
    if actual != expected || actual.0 == 0 || actual.1 == 0 || actual.0 > 256 || actual.1 > 256 {
        return Err(message(
            "ICO图像尺寸与目录不一致",
            "ICO image dimensions do not match its directory",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Cursor, sync::Arc};

    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(rgba).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn ico(entries: &[((u32, u32), u16, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = vec![0, 0, 1, 0];
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let mut offset = 6 + entries.len() * 16;
        for &((width, height), bits, ref payload) in entries {
            assert!((1..=256).contains(&width) && (1..=256).contains(&height));
            bytes.extend_from_slice(&[width as u8, height as u8, 0, 0]);
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&bits.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += payload.len();
        }
        for (_, _, payload) in entries {
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    fn cursor(entries: &[((u32, u32), (u16, u16), Vec<u8>)]) -> Vec<u8> {
        let icons: Vec<_> = entries
            .iter()
            .map(|(dimensions, _, payload)| (*dimensions, 0, payload.clone()))
            .collect();
        let mut bytes = ico(&icons);
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
        for (index, (_, (x, y), _)) in entries.iter().enumerate() {
            let offset = 6 + index * 16 + 4;
            bytes[offset..offset + 2].copy_from_slice(&x.to_le_bytes());
            bytes[offset + 2..offset + 4].copy_from_slice(&y.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn image_ico_input_reservation_counts_outer_source_directory_and_virtual_storage() {
        let bytes = ico(&[((2, 1), 32, png(2, 1, &[10, 20, 30, 128, 40, 50, 60, 255]))]);
        let outer_capacity = bytes.capacity() as u64;
        let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        let directory = source.input_reservation(0, 0).unwrap();
        assert!(directory >= (source.entries.capacity() * std::mem::size_of::<IcoEntry>()) as u64);
        assert_eq!(
            source
                .input_reservation(MAX_DECODE_BYTES - directory - 17, 17)
                .unwrap(),
            MAX_DECODE_BYTES
        );
        assert!(source
            .input_reservation(MAX_DECODE_BYTES - directory - 16, 17)
            .is_err());
        let position = source.reader.position();
        assert!(source
            .decode_with_input_reservation(0, 8, &token(), MAX_DECODE_BYTES)
            .is_err());
        assert!(source
            .decode_with_input_reservation(0, 8, &token(), u64::MAX)
            .is_err());
        assert_eq!(source.reader.position(), position);
        let decoded = source
            .decode_with_input_reservation(0, 8, &token(), outer_capacity)
            .unwrap();
        assert_eq!(decoded.image.bgra, [30, 20, 10, 128, 60, 50, 40, 255]);
    }

    #[test]
    fn image_cursor_keeps_distinct_dimensions_hotspots_and_strict_constructors() {
        let bytes = cursor(&[
            (
                (2, 1),
                (1, 0),
                png(2, 1, &[10, 20, 30, 128, 40, 50, 60, 64]),
            ),
            ((1, 2), (0, 1), png(1, 2, &[1, 2, 3, 255, 4, 5, 6, 32])),
        ]);
        assert!(IcoSource::from_reader(Cursor::new(bytes.clone()), &token()).is_err());
        let mut source = IcoSource::from_cursor_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(source.kind(), ContainerKind::Cursor);
        assert_eq!(source.entries()[0].dimensions, (2, 1));
        assert_eq!(source.entries()[0].hotspot, Some((1, 0)));
        assert_eq!(source.entries()[1].dimensions, (1, 2));
        assert_eq!(source.entries()[1].hotspot, Some((0, 1)));
        assert!(source
            .entries()
            .iter()
            .all(|entry| entry.bits_per_pixel == 0));
        let first = source.decode(0, 8, &token()).unwrap();
        assert_eq!(first.image.bgra, [30, 20, 10, 128, 60, 50, 40, 64]);
        assert_eq!(first.metadata.format, image::ImageFormat::Ico);
        let second = source.decode(1, 8, &token()).unwrap();
        assert_eq!((second.image.width, second.image.height), (1, 2));
        assert_eq!(second.image.bgra, [3, 2, 1, 255, 6, 5, 4, 32]);
        let bytes = ico(&[((1, 1), 32, png(1, 1, &[1, 2, 3, 255]))]);
        assert!(IcoSource::from_cursor_reader(Cursor::new(bytes.clone()), &token()).is_err());
        let source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(source.kind(), ContainerKind::Icon);
        assert_eq!(source.entries()[0].hotspot, None);
    }

    #[test]
    fn image_cursor_default_uses_area_without_treating_hotspot_y_as_depth() {
        let bytes = cursor(&[
            ((16, 1), (15, 0), png(16, 1, &[10, 20, 30, 255].repeat(16))),
            ((1, 8), (0, 7), png(1, 8, &[40, 50, 60, 255].repeat(8))),
        ]);
        let source = IcoSource::from_cursor_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(source.default_index(), 0);
        assert_eq!(source.entries()[1].hotspot, Some((0, 7)));
        assert_eq!(source.entries()[1].bits_per_pixel, 0);
    }

    #[test]
    fn image_cursor_invalid_hotspot_is_local_and_does_not_read_bad_entry() {
        for hotspot in [(2, 0), (0, 1)] {
            let bytes = cursor(&[
                ((2, 1), hotspot, png(2, 1, &[1, 2, 3, 255].repeat(2))),
                ((1, 2), (0, 1), png(1, 2, &[11, 22, 33, 44].repeat(2))),
            ]);
            let mut source = IcoSource::from_cursor_reader(Cursor::new(bytes), &token()).unwrap();
            assert_eq!(source.entries().len(), 2);
            assert_eq!(source.entries()[0].hotspot, Some(hotspot));
            let position = source.reader.position();
            assert!(source.decode(0, 8, &token()).is_err());
            assert_eq!(source.reader.position(), position);
            let decoded = source.decode(1, 8, &token()).unwrap();
            assert_eq!((decoded.image.width, decoded.image.height), (1, 2));
            assert_eq!(&decoded.image.bgra[..4], &[33, 22, 11, 44]);
        }
    }

    #[test]
    fn image_cursor_virtual_ico_keeps_rgba_and_rgb32_masks_but_rejects_monochrome_invert() {
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&4i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&16u32.to_le_bytes());
        dib.extend_from_slice(&[255, 0, 0, 0, 0, 255, 255, 0, 0, 0, 255, 0, 0, 255, 0, 0]);
        dib.extend_from_slice(&[0x80, 0, 0, 0, 0x40, 0, 0, 0]);
        let mut real_alpha = dib.clone();
        real_alpha[43] = 128;
        let mut mono = vec![0; 40];
        mono[..4].copy_from_slice(&40u32.to_le_bytes());
        mono[4..8].copy_from_slice(&1i32.to_le_bytes());
        mono[8..12].copy_from_slice(&2i32.to_le_bytes());
        mono[12..14].copy_from_slice(&1u16.to_le_bytes());
        mono[14..16].copy_from_slice(&1u16.to_le_bytes());
        mono[20..24].copy_from_slice(&4u32.to_le_bytes());
        mono[32..36].copy_from_slice(&2u32.to_le_bytes());
        mono.extend_from_slice(&[0, 0, 0, 0, 255, 255, 255, 0]);
        // AND=1/XOR=1的屏幕反转不能降为RGBA透明。
        mono.extend_from_slice(&[0x80, 0, 0, 0, 0x80, 0, 0, 0]);
        let mut rgb24 = dib[..40].to_vec();
        rgb24[8..12].copy_from_slice(&2i32.to_le_bytes());
        rgb24[14..16].copy_from_slice(&24u16.to_le_bytes());
        rgb24[20..24].copy_from_slice(&8u32.to_le_bytes());
        rgb24.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0, 0x40, 0, 0, 0]);
        let bytes = cursor(&[
            (
                (2, 1),
                (1, 0),
                png(2, 1, &[10, 20, 30, 128, 40, 50, 60, 64]),
            ),
            ((2, 2), (1, 1), dib),
            ((2, 2), (0, 0), real_alpha),
            ((1, 1), (0, 0), mono),
            ((2, 1), (0, 0), rgb24),
        ]);
        let mut source = IcoSource::from_cursor_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(
            source.decode(0, 8, &token()).unwrap().image.bgra,
            [30, 20, 10, 128, 60, 50, 40, 64]
        );
        let rgba = source.decode(1, 16, &token()).unwrap();
        assert_eq!((rgba.image.width, rgba.image.height), (2, 2));
        let alpha: Vec<_> = rgba
            .image
            .bgra
            .chunks_exact(4)
            .map(|pixel| pixel[3])
            .collect();
        assert_eq!(alpha, [255, 0, 0, 255]);
        assert_eq!(&rgba.image.bgra[..4], &[0, 0, 255, 255]);
        assert!(source
            .decode(2, 16, &token())
            .unwrap()
            .image
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 0));
        assert!(source.decode(3, 4, &token()).is_err());
        assert!(source.decode(4, 8, &token()).is_err());
        assert_eq!(source.decode(0, 8, &token()).unwrap().image.bgra[3], 128);
    }

    #[test]
    fn image_ico_selects_dimensions_and_preserves_alpha_bgra_and_container_size() {
        let bytes = ico(&[
            ((2, 1), 32, png(2, 1, &[10, 20, 30, 128, 200, 100, 50, 0])),
            ((1, 2), 32, png(1, 2, &[1, 2, 3, 255, 4, 5, 6, 64])),
        ]);
        let length = bytes.len() as u64;
        let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(source.entries().len(), 2);
        assert_eq!(source.entries()[0].dimensions, (2, 1));
        assert_eq!(source.entries()[1].dimensions, (1, 2));
        assert_eq!(source.default_index(), 1);
        assert!(source.decode(0, 7, &token()).is_err());
        let first = source.decode(0, 8, &token()).unwrap();
        assert_eq!((first.image.width, first.image.height), (2, 1));
        assert_eq!(first.image.bgra, [30, 20, 10, 128, 50, 100, 200, 0]);
        assert_eq!(first.metadata.format, image::ImageFormat::Ico);
        assert_eq!(first.metadata.encoded_bytes, Some(length));
        let second = source.decode(1, 8, &token()).unwrap();
        assert_eq!((second.image.width, second.image.height), (1, 2));
        assert_eq!(second.image.bgra, [3, 2, 1, 255, 6, 5, 4, 64]);
        assert!(source.decode(2, MAX_DECODE_BYTES, &token()).is_err());
    }

    #[test]
    fn image_ico_default_keeps_bit_depth_area_and_existing_tie_order() {
        for (entries, expected) in [
            (vec![((4, 4), 8), ((1, 1), 32), ((2, 1), 32)], 2),
            (vec![((2, 1), 32), ((2, 1), 32), ((1, 1), 32)], 0),
            (vec![((2, 1), 32), ((1, 1), 32), ((2, 1), 32)], 2),
        ] {
            let entries: Vec<_> = entries
                .into_iter()
                .map(|(size, bits)| (size, bits, vec![1]))
                .collect();
            let source = IcoSource::from_reader(Cursor::new(ico(&entries)), &token()).unwrap();
            assert_eq!(source.default_index(), expected);
        }
    }

    #[test]
    fn image_ico_bad_payload_does_not_poison_another_valid_entry() {
        let bytes = ico(&[
            ((2, 1), 32, png(1, 1, &[1, 2, 3, 255])),
            ((1, 1), 32, vec![0xff; 4]),
            ((1, 1), 32, png(1, 1, &[7, 8, 9, 100])),
        ]);
        let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        assert!(source.decode(0, MAX_DECODE_BYTES, &token()).is_err());
        assert!(source.decode(1, MAX_DECODE_BYTES, &token()).is_err());
        assert_eq!(
            source.decode(2, 4, &token()).unwrap().image.bgra,
            [9, 8, 7, 100]
        );
    }

    #[test]
    fn image_ico_directory_requires_icon_type_and_fixed_payload_bounds() {
        let valid = ico(&[((1, 1), 32, png(1, 1, &[1, 2, 3, 255]))]);
        let mut cases = Vec::new();
        let mut bytes = valid.clone();
        bytes[0] = 1;
        cases.push(bytes);
        let mut bytes = valid.clone();
        bytes[2] = 2;
        cases.push(bytes);
        let mut bytes = valid.clone();
        bytes[4..6].copy_from_slice(&257u16.to_le_bytes());
        cases.push(bytes);
        cases.push(valid[..21].to_vec());
        for bytes in cases {
            assert!(IcoSource::from_reader(Cursor::new(bytes), &token()).is_err());
        }
        let mut entry_cases = Vec::new();
        let mut bytes = valid.clone();
        bytes[9] = 1;
        entry_cases.push(bytes);
        let mut bytes = valid.clone();
        bytes[18..22].copy_from_slice(&21u32.to_le_bytes());
        entry_cases.push(bytes);
        let mut bytes = valid.clone();
        bytes[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        entry_cases.push(bytes);
        let mut bytes = valid.clone();
        bytes[14..18].copy_from_slice(&0u32.to_le_bytes());
        entry_cases.push(bytes);
        for bytes in entry_cases {
            let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
            let position = source.reader.position();
            assert!(source.decode(0, MAX_DECODE_BYTES, &token()).is_err());
            assert_eq!(source.reader.position(), position);
        }
        struct LargeSource(Cursor<Vec<u8>>);
        impl Read for LargeSource {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.0.read(output)
            }
        }
        impl Seek for LargeSource {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                if matches!(position, SeekFrom::End(0)) {
                    self.0.set_position(MAX_DECODE_BYTES + 1);
                    Ok(MAX_DECODE_BYTES + 1)
                } else {
                    self.0.seek(position)
                }
            }
        }
        // 小目录及所选负载不能绕过来源总长度限制；不用分配大文件。
        assert!(IcoSource::from_reader(LargeSource(Cursor::new(valid)), &token()).is_err());
    }

    #[test]
    fn image_ico_invalid_entry_ranges_leave_valid_png_selectable_without_blind_reads() {
        let valid = ico(&[
            ((1, 1), 32, png(1, 1, &[1, 2, 3, 255])),
            ((1, 1), 32, png(1, 1, &[11, 22, 33, 44])),
        ]);
        for (offset, length) in [(u32::MAX, 1), (6, 1), (38, u32::MAX), (38, 0)] {
            let mut bytes = valid.clone();
            bytes[14..18].copy_from_slice(&length.to_le_bytes());
            bytes[18..22].copy_from_slice(&offset.to_le_bytes());
            let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
            assert_eq!(source.entries().len(), 2);
            let position = source.reader.position();
            assert!(source.decode(0, 4, &token()).is_err());
            assert_eq!(source.reader.position(), position);
            assert_eq!(
                source.decode(1, 4, &token()).unwrap().image.bgra,
                [33, 22, 11, 44]
            );
        }
    }

    #[test]
    fn image_ico_zero_dimension_is_256_and_payload_dimensions_are_exact() {
        let pixels = [10, 20, 30, 255].repeat(256);
        let bytes = ico(&[
            ((256, 1), 32, png(256, 1, &pixels)),
            ((256, 1), 32, png(257, 1, &[1, 2, 3, 255].repeat(257))),
        ]);
        let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        assert_eq!(source.entries()[0].dimensions, (256, 1));
        assert_eq!(source.decode(0, 1024, &token()).unwrap().image.width, 256);
        assert!(source.decode(1, MAX_DECODE_BYTES, &token()).is_err());
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&4i32.to_le_bytes());
        assert!(validate_payload_dimensions(&dib, (2, 2)).is_ok());
        dib[8..12].copy_from_slice(&5i32.to_le_bytes());
        assert!(validate_payload_dimensions(&dib, (2, 2)).is_err());
        dib[8..12].copy_from_slice(&(-4i32).to_le_bytes());
        assert!(validate_payload_dimensions(&dib, (2, 2)).is_err());
    }

    #[test]
    fn image_ico_dib_and_mask_keeps_visible_color_and_clears_masked_alpha() {
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        // DIB高度同时计入一行XOR像素和一行AND掩码。
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib[20..24].copy_from_slice(&8u32.to_le_bytes());
        // BGR红/绿各三字节，像素行按四字节边界补齐至8B。
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        // AND掩码从最高位起：首像素可见、次像素透明，掩码行补齐至4B。
        dib.extend_from_slice(&[0x40, 0, 0, 0]);
        let bytes = ico(&[((2, 1), 24, dib)]);
        let mut source = IcoSource::from_reader(Cursor::new(bytes), &token()).unwrap();
        let decoded = source.decode(0, 8, &token()).unwrap();
        assert_eq!((decoded.image.width, decoded.image.height), (2, 1));
        assert_eq!(&decoded.image.bgra[..4], &[0, 0, 255, 255]);
        assert_eq!(decoded.image.bgra[7], 0);
    }

    #[test]
    fn image_ico_zero_alpha_rgb32_uses_bottom_up_and_mask_without_changing_real_alpha_or_png() {
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&4i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[20..24].copy_from_slice(&16u32.to_le_bytes());
        // 编码按底行蓝/黄、顶行红/绿排列，四个保留字节都为0。
        dib.extend_from_slice(&[255, 0, 0, 0, 0, 255, 255, 0, 0, 0, 255, 0, 0, 255, 0, 0]);
        let mixed_mask = vec![0x80, 0, 0, 0, 0x40, 0, 0, 0];
        for (mask, expected_alpha) in [
            (vec![0; 8], [255; 4]),
            (mixed_mask.clone(), [255, 0, 0, 255]),
            (Vec::new(), [255; 4]),
        ] {
            let mut payload = dib.clone();
            payload.extend_from_slice(&mask);
            let mut source =
                IcoSource::from_reader(Cursor::new(ico(&[((2, 2), 32, payload)])), &token())
                    .unwrap();
            let decoded = source.decode(0, 16, &token()).unwrap();
            let alpha: Vec<_> = decoded
                .image
                .bgra
                .chunks_exact(4)
                .map(|pixel| pixel[3])
                .collect();
            assert_eq!(alpha, expected_alpha);
            assert_eq!(&decoded.image.bgra[..4], &[0, 0, 255, 255]);
            assert_eq!(&decoded.image.bgra[12..16], &[0, 255, 255, 255]);
        }
        // 非0源alpha即使恰被AND清除，也不能被误认为0RGB而恢复其它像素。
        dib[43] = 128;
        dib.extend_from_slice(&mixed_mask);
        let mut source = IcoSource::from_reader(
            Cursor::new(ico(&[
                ((2, 2), 32, dib),
                ((1, 1), 32, png(1, 1, &[10, 20, 30, 0])),
            ])),
            &token(),
        )
        .unwrap();
        assert!(source
            .decode(0, 16, &token())
            .unwrap()
            .image
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 0));
        assert_eq!(source.decode(1, 4, &token()).unwrap().image.bgra[3], 0);
    }

    #[test]
    fn image_ico_cancelled_read_is_other_and_source_can_seek_again() {
        struct CancelRead {
            bytes: Cursor<Vec<u8>>,
            cancel: Arc<AtomicBool>,
            armed: Arc<AtomicBool>,
        }
        impl Read for CancelRead {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = self.bytes.read(output)?;
                if self.armed.swap(false, Ordering::AcqRel) {
                    self.cancel.store(true, Ordering::Release);
                }
                Ok(count)
            }
        }
        impl Seek for CancelRead {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.bytes.seek(position)
            }
        }
        let cancel = Arc::new(token());
        let armed = Arc::new(token());
        let reader = CancelRead {
            bytes: Cursor::new(ico(&[((1, 1), 32, png(1, 1, &[11, 22, 33, 44]))])),
            cancel: cancel.clone(),
            armed: armed.clone(),
        };
        let mut source = IcoSource::from_reader(reader, &cancel).unwrap();
        armed.store(true, Ordering::Release);
        assert!(source.decode(0, 4, &cancel).is_err());
        assert_eq!(cancelled(&cancel).unwrap_err().kind(), io::ErrorKind::Other);
        cancel.store(false, Ordering::Release);
        assert_eq!(
            source.decode(0, 4, &cancel).unwrap().image.bgra,
            [33, 22, 11, 44]
        );
    }
}
