//! 自有恢复资源的 PNG 私有采样域；用户图片输出不调用此模块。
use super::{CapturePixels, CaptureSourceMetadata, OwnedPlane, MAX_CONTEXT_BYTES};
use crate::{capture::Image, Result};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

// ancillary / private / reserved uppercase / unsafe-to-copy。
const SOURCE_CHUNK: [u8; 4] = *b"rsSC";
const CHUNK_BYTES: usize = 256 * 1024;
const MAX_ARCHIVE_BYTES: u64 = MAX_CONTEXT_BYTES + 8 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 480 * 1024;
const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
const IEND: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 174, 66, 96, 130];

struct ArchiveTemp {
    path: PathBuf,
    file: Option<File>,
}
impl ArchiveTemp {
    fn new(target: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = target.parent().ok_or("截图来源资源缺少目录")?;
        let path = parent.join(format!(
            ".rustshot-source-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }
}
impl Drop for ArchiveTemp {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut crc = index as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
        *slot = crc;
    }
    table
}
fn crc_update(mut crc: u32, bytes: &[u8], table: &[u32; 256]) -> u32 {
    for &byte in bytes {
        crc = (crc >> 8) ^ table[((crc ^ u32::from(byte)) & 255) as usize];
    }
    crc
}

/// 仅用于自有关闭/工作区的未发布临时 PNG；普通复制、保存、拖出只使用 export。
pub(crate) fn save_png_pixels_with_context(
    image: &Image,
    metadata: &CaptureSourceMetadata,
    pixels: &CapturePixels,
    path: &Path,
) -> Result<()> {
    save_png_bytes_with_context(
        (image.width, image.height),
        &image.bgra,
        metadata,
        pixels,
        path,
    )
}
pub(crate) fn save_png_bytes_with_context(
    size: (u32, u32),
    bgra: &[u8],
    metadata: &CaptureSourceMetadata,
    pixels: &CapturePixels,
    path: &Path,
) -> Result<()> {
    metadata.validate()?;
    pixels.validate()?;
    if !metadata.matches(pixels)
        || size != metadata.storage_size()
        || !pixels.content_matches(metadata.content_rect(), bgra)
    {
        return Err("内部截图恢复帧与采样域不匹配".into());
    }
    let mut temp = ArchiveTemp::new(path)?;
    let mut archive = ZipWriter::new(temp.file.take().expect("new temp archive"));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    archive.start_file("source.json", options)?;
    let header = serde_json::to_vec(metadata)?;
    if header.len() as u64 > MAX_METADATA_BYTES {
        return Err("截图来源描述超过容量".into());
    }
    archive.write_all(&header)?;
    for (index, plane) in pixels.planes.iter().enumerate() {
        archive.start_file(format!("plane-{index:02}.bgra"), options)?;
        archive.write_all(plane.image.as_bytes(0).ok_or("截图采样平面没有CPU像素")?)?;
    }
    let mut archive = archive.finish()?;
    archive.sync_all()?;
    let archive_len = archive.metadata()?.len();
    if archive_len > MAX_ARCHIVE_BYTES {
        return Err("压缩截图采样资源超过256 MiB上下文预算".into());
    }
    crate::export::save_png_pixels(size.0, size.1, bgra, path)?;
    let mut png = OpenOptions::new().read(true).write(true).open(path)?;
    let png_len = png.metadata()?.len();
    if png_len < 20 {
        return Err("内部PNG过短".into());
    }
    png.seek(SeekFrom::End(-12))?;
    let mut end = [0; 12];
    png.read_exact(&mut end)?;
    if end != IEND {
        return Err("内部PNG尾部不是IEND".into());
    }
    png.set_len(png_len - 12)?;
    png.seek(SeekFrom::End(0))?;
    archive.seek(SeekFrom::Start(0))?;
    let table = crc_table();
    let mut buffer = vec![0; CHUNK_BYTES];
    let mut remaining = archive_len;
    while remaining > 0 {
        let count = remaining.min(CHUNK_BYTES as u64) as usize;
        archive.read_exact(&mut buffer[..count])?;
        png.write_all(&(count as u32).to_be_bytes())?;
        png.write_all(&SOURCE_CHUNK)?;
        png.write_all(&buffer[..count])?;
        let crc = crc_update(
            crc_update(u32::MAX, &SOURCE_CHUNK, &table),
            &buffer[..count],
            &table,
        ) ^ u32::MAX;
        png.write_all(&crc.to_be_bytes())?;
        remaining -= count as u64;
    }
    png.write_all(&IEND)?;
    png.sync_all()?;
    temp.file = Some(archive);
    Ok(())
}

struct SourcePart {
    virtual_start: u64,
    file_start: u64,
    len: u64,
}
struct SourceReader {
    file: File,
    parts: Vec<SourcePart>,
    len: u64,
    position: u64,
}
impl Read for SourceReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.position == self.len {
            return Ok(0);
        }
        let part = self
            .parts
            .iter()
            .find(|part| {
                self.position >= part.virtual_start && self.position < part.virtual_start + part.len
            })
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "截图来源分片不连续")
            })?;
        let local = self.position - part.virtual_start;
        let count = output.len().min((part.len - local) as usize);
        self.file.seek(SeekFrom::Start(part.file_start + local))?;
        self.file.read_exact(&mut output[..count])?;
        self.position += count as u64;
        Ok(count)
    }
}
impl Seek for SourceReader {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let next = match from {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
            SeekFrom::End(value) => i128::from(self.len) + i128::from(value),
        };
        if next < 0 || next > i128::from(self.len) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "截图来源归档seek越界",
            ));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

fn source_reader(path: &Path) -> Result<SourceReader> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut signature = [0; 8];
    file.read_exact(&mut signature)?;
    if signature != PNG_SIGNATURE {
        return Err("截图来源资源不是PNG".into());
    }
    let table = crc_table();
    let mut parts = Vec::new();
    let mut total = 0u64;
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let len = u64::from(u32::from_be_bytes(header[..4].try_into()?));
        let file_start = file.stream_position()?;
        let end = file_start
            .checked_add(len)
            .and_then(|end| end.checked_add(4))
            .ok_or("PNG分片范围溢出")?;
        if end > file_len {
            return Err("PNG来源分片被截断".into());
        }
        let kind: [u8; 4] = header[4..].try_into()?;
        if kind == SOURCE_CHUNK {
            if len == 0 || len > CHUNK_BYTES as u64 || parts.len() >= 2048 {
                return Err("截图私有来源分片数量或尺寸超限".into());
            }
            total = total.checked_add(len).ok_or("截图归档容量溢出")?;
            if total > MAX_ARCHIVE_BYTES {
                return Err("截图来源归档超过解压预算".into());
            }
            file.read_exact(&mut buffer[..len as usize])?;
            let mut expected = [0; 4];
            file.read_exact(&mut expected)?;
            let actual = crc_update(
                crc_update(u32::MAX, &kind, &table),
                &buffer[..len as usize],
                &table,
            ) ^ u32::MAX;
            if actual != u32::from_be_bytes(expected) {
                return Err("截图来源私有PNG分片CRC不一致".into());
            }
            parts.push(SourcePart {
                virtual_start: total - len,
                file_start,
                len,
            });
        } else {
            file.seek(SeekFrom::Start(end))?;
        }
        if kind == *b"IEND" {
            if len != 0 || end != file_len {
                return Err("截图来源PNG尾部无效或含额外数据".into());
            }
            break;
        }
    }
    if parts.is_empty() {
        return Err("截图文档缺少不可变来源归档".into());
    }
    Ok(SourceReader {
        file,
        parts,
        len: total,
        position: 0,
    })
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("fixed header"))
}
fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("fixed header"))
}
fn checked_extra(reader: &mut SourceReader, len: usize) -> Result<()> {
    // 自身SimpleFileOptions默认不产生extra。拒绝NTFS等解析错误触发库回扫另一EOCD。
    let _ = reader;
    if len != 0 {
        return Err("截图来源V1不支持ZIP附加字段或ZIP64".into());
    }
    Ok(())
}
/// V1仅接受自身writer能够产生的ZIP32（<=264MiB，<=65项，不需ZIP64）。
/// ZipArchive会按声明项数预分配；必须在构造它前用固定小缓冲核整个目录与各local header。
fn preflight_zip32(reader: &mut SourceReader, metadata: &CaptureSourceMetadata) -> Result<()> {
    if reader.len < 22 {
        return Err("截图来源ZIP尾部不完整".into());
    }
    let end_at = reader.len - 22;
    reader.seek(SeekFrom::Start(end_at))?;
    let mut end = [0u8; 22];
    reader.read_exact(&mut end)?;
    let expected = metadata.plane_rects().count() + 1;
    if le32(&end, 0) != 0x06054b50
        || le16(&end, 4) != 0
        || le16(&end, 6) != 0
        || usize::from(le16(&end, 8)) != expected
        || usize::from(le16(&end, 10)) != expected
        || expected > 65
        || le16(&end, 20) != 0
    {
        return Err("截图来源ZIP目录数量、多盘或comment无效".into());
    }
    let directory = u64::from(le32(&end, 16));
    let directory_bytes = u64::from(le32(&end, 12));
    if directory_bytes > 64 * 1024 || directory + directory_bytes != end_at {
        return Err("截图来源ZIP目录范围无效或超出64KiB".into());
    }
    let mut position = directory;
    let mut data_end = 0u64;
    let rects: Vec<_> = metadata.plane_rects().collect();
    for index in 0..expected {
        if position + 46 > end_at {
            return Err("截图来源ZIP目录项头超出边界".into());
        }
        reader.seek(SeekFrom::Start(position))?;
        let mut central = [0u8; 46];
        reader.read_exact(&mut central)?;
        let flags = le16(&central, 8);
        let method = le16(&central, 10);
        let compressed = u64::from(le32(&central, 20));
        let bytes = u64::from(le32(&central, 24));
        let name_len = usize::from(le16(&central, 28));
        let extra = usize::from(le16(&central, 30));
        let comment = usize::from(le16(&central, 32));
        let offset = u64::from(le32(&central, 42));
        let next = position + 46 + name_len as u64 + extra as u64 + comment as u64;
        if le32(&central, 0) != 0x02014b50
            || le16(&central, 6) > 20
            || flags & !0x0806 != 0
            || method != 8
            || name_len > 64
            || extra != 0
            || comment != 0
            || le16(&central, 34) != 0
            || next > end_at
            || offset != data_end
        {
            return Err("截图来源ZIP目录项、压缩、加密或local范围无效".into());
        }
        if index == 0 && bytes > MAX_METADATA_BYTES
            || index > 0
                && bytes
                    != u64::from(rects[index - 1].width) * u64::from(rects[index - 1].height) * 4
        {
            return Err("截图来源ZIP声明解压尺寸与有界元数据/平面不符".into());
        }
        let expected_name = if index == 0 {
            "source.json".into()
        } else {
            format!("plane-{:02}.bgra", index - 1)
        };
        let mut name = [0u8; 64];
        reader.read_exact(&mut name[..name_len])?;
        if &name[..name_len] != expected_name.as_bytes() {
            return Err("截图来源ZIP目录名或顺序无效".into());
        }
        checked_extra(reader, extra)?;
        if offset + 30 > directory {
            return Err("截图来源ZIP local头越过目录".into());
        }
        reader.seek(SeekFrom::Start(offset))?;
        let mut local = [0u8; 30];
        reader.read_exact(&mut local)?;
        let local_extra = usize::from(le16(&local, 28));
        data_end = offset + 30 + name_len as u64 + local_extra as u64 + compressed;
        if le32(&local, 0) != 0x04034b50
            || le16(&local, 4) > 20
            || le16(&local, 6) != flags
            || le16(&local, 8) != method
            || le32(&local, 14) != le32(&central, 16)
            || le32(&local, 18) != le32(&central, 20)
            || le32(&local, 22) != le32(&central, 24)
            || usize::from(le16(&local, 26)) != name_len
            || local_extra != 0
            || data_end > directory
        {
            return Err("截图来源ZIP local像素段、尺寸或CRC与目录不一致".into());
        }
        let mut local_name = [0u8; 64];
        reader.read_exact(&mut local_name[..name_len])?;
        if local_name[..name_len] != name[..name_len] {
            return Err("截图来源ZIP local名称不一致".into());
        }
        checked_extra(reader, local_extra)?;
        position = next;
    }
    if position != end_at || data_end != directory {
        return Err("截图来源ZIP目录或像素段含额外布局".into());
    }
    reader.seek(SeekFrom::Start(0))?;
    Ok(())
}
/// 调用方必须已取得自有资源的租约；不用于外部普通PNG编辑或OCR文件输入。
pub(crate) fn load_context(
    path: &Path,
    metadata: &CaptureSourceMetadata,
    visible: &Image,
) -> Result<Arc<CapturePixels>> {
    metadata.validate()?;
    let mut reader = source_reader(path)?;
    preflight_zip32(&mut reader, metadata)?;
    let mut archive = ZipArchive::new(reader)?;
    let rects: Vec<_> = metadata.plane_rects().collect();
    if archive.len() != rects.len() + 1 {
        return Err("截图来源归档平面数量不匹配".into());
    }
    let mut header = archive.by_index(0)?;
    if header.name() != "source.json" || header.size() > MAX_METADATA_BYTES || header.is_dir() {
        return Err("截图来源归档描述无效".into());
    }
    let mut encoded = Vec::new();
    header
        .by_ref()
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut encoded)?;
    if encoded.len() as u64 > MAX_METADATA_BYTES {
        return Err("截图来源描述解压超出容量".into());
    }
    let stored: CaptureSourceMetadata = serde_json::from_slice(&encoded)?;
    stored.validate()?;
    if &stored != metadata {
        return Err("截图来源归档与文档元数据不一致".into());
    }
    drop(header);
    let mut planes = Vec::new();
    let mut total = 0u64;
    for (index, rect) in rects.into_iter().enumerate() {
        let expected = u64::from(rect.width) * u64::from(rect.height) * 4;
        total = total.checked_add(expected).ok_or("截图解压容量溢出")?;
        if total > MAX_CONTEXT_BYTES {
            return Err("截图CPU来源解压超过256 MiB".into());
        }
        let mut plane = archive.by_index(index + 1)?;
        if plane.name() != format!("plane-{index:02}.bgra")
            || plane.size() != expected
            || plane.is_dir()
        {
            return Err("截图来源平面描述或像素长度不匹配".into());
        }
        let mut bytes = vec![0; usize::try_from(expected)?];
        plane.read_exact(&mut bytes)?;
        let mut extra = [0; 1];
        if plane.read(&mut extra)? != 0 {
            return Err("截图来源平面含多余像素".into());
        }
        planes.push(OwnedPlane {
            rect,
            image: crate::canvas_source::render_image(rect.width, rect.height, bytes),
        });
    }
    let pixels = Arc::new(CapturePixels::new(
        metadata.bounds(),
        metadata.fill(),
        planes,
    )?);
    if !metadata.matches(&pixels)
        || (visible.width, visible.height) != metadata.storage_size()
        || !pixels.content_matches(metadata.content_rect(), &visible.bgra)
    {
        return Err("截图来源可见原像素与PNG兼容帧不一致".into());
    }
    Ok(pixels)
}

#[cfg(test)]
#[path = "pin_capture_source_store_tests.rs"]
mod tests;
