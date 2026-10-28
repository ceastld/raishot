//! 历史只保存用户开启后完成的合成 PNG，最多 50 张 / 256 MiB / 30 天。
use crate::{capture::Image, export, preferences, Result};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
    path::{Component, Path},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[path = "history_gallery.rs"]
pub(crate) mod gallery;

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
    pub dimensions: Option<(u32, u32)>,
}
pub fn entries() -> Result<Vec<Entry>> {
    let dir = preferences::data_dir().join("history");
    prune(&dir)?;
    entries_in(&dir)
}
fn entries_in(dir: &std::path::Path) -> Result<Vec<Entry>> {
    if !ordinary_directory(dir)? {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for file in std::fs::read_dir(dir)? {
        let file = file?;
        let path = file.path();
        let name = file.file_name().to_string_lossy().to_string();
        if !name.starts_with("shot-") || !name.ends_with(".png") || !file.file_type()?.is_file() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !ordinary_file(&metadata) {
            continue;
        }
        entries.push(Entry {
            path,
            bytes: metadata.len(),
            modified: metadata.modified()?,
            dimensions: None,
        });
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
    Ok(entries)
}

/// 元数据扫描也在后台；不把 PNG 解码或缓存写入磁盘。
pub(crate) fn scan(cancelled: &dyn Fn() -> bool) -> Result<Vec<Entry>> {
    if cancelled() {
        return Err("截图历史扫描已取消".into());
    }
    let mut list = entries()?;
    list.truncate(gallery::MAX_ENTRIES);
    for entry in &mut list {
        if cancelled() {
            return Err("截图历史扫描已取消".into());
        }
        // 固定 33-byte IHDR 探测，坏图仍保留元数据与删除入口。
        entry.dimensions = checked_file(entry, false).ok().and_then(|mut file| {
            let mut header = [0; 33];
            file.read_exact(&mut header).ok()?;
            png_dimensions(&header)
        });
    }
    if cancelled() {
        return Err("截图历史扫描已取消".into());
    }
    Ok(list)
}

pub(crate) fn decode_gate() -> &'static gallery::DecodeGate {
    static GATE: OnceLock<gallery::DecodeGate> = OnceLock::new();
    GATE.get_or_init(gallery::DecodeGate::default)
}
pub(crate) struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub(crate) fn thumbnail(entry: &Entry, cancelled: &dyn Fn() -> bool) -> Result<Thumbnail> {
    let (width, height, rgba) = decode_png(entry, true, cancelled)?;
    Ok(Thumbnail {
        width,
        height,
        rgba,
    })
}
pub(crate) fn open_image(entry: &Entry, cancelled: &dyn Fn() -> bool) -> Result<Image> {
    let (width, height, mut bgra) = decode_png(entry, false, cancelled)?;
    rgba_to_bgra(&mut bgra);
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}
pub(crate) fn rgba_to_bgra(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// 程序输出的 PNG 为非交错 RGBA8；逐行解码只保有有界 decoder 和一份输出。
/// 普通打开先核对 65535 边长／3200万像素，保持现有持有图像合同。
fn decode_png(
    entry: &Entry,
    small: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<(u32, u32, Vec<u8>)> {
    if cancelled() {
        return Err("截图历史解码已取消".into());
    }
    let file = checked_file(entry, false)?;
    let output = decode_rows(BufReader::new(file), entry.dimensions, small, cancelled)?;
    // 已打开句柄拒绝写共享；再核对路径身份，迟到任务另由窗口代次门拒绝。
    validate_entry(entry)?;
    Ok(output)
}
fn decode_rows<R: BufRead + Seek>(
    mut input: R,
    dimensions: Option<(u32, u32)>,
    small: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<(u32, u32, Vec<u8>)> {
    if cancelled() {
        return Err("截图历史解码已取消".into());
    }
    // 在 decoder 处理元数据/分配行缓冲之前，用固定 IHDR 核尺寸、CRC 和像素上限。
    let mut header = [0_u8; 33];
    input.read_exact(&mut header)?;
    let (width, height) = png_dimensions(&header).ok_or("历史 PNG 文件头无效")?;
    let thumb =
        gallery::thumbnail_size(width, height).ok_or("图片超过普通图像尺寸或3200万像素上限")?;
    if header[24..29] != [8, 6, 0, 0, 0] {
        return Err("历史图片不是程序输出的非交错 RGBA8 PNG".into());
    }
    if dimensions.is_some_and(|dimensions| dimensions != (width, height)) {
        return Err("历史图片尺寸已改变，请刷新".into());
    }
    input.seek(SeekFrom::Start(0))?;
    let mut decoder = png::Decoder::new_with_limits(
        input,
        png::Limits {
            bytes: gallery::DECODER_BYTES,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.ignore_checksums(false);
    let mut reader = decoder.read_info()?;
    if reader.info().interlaced
        || reader.info().animation_control.is_some()
        || reader.output_color_type() != (png::ColorType::Rgba, png::BitDepth::Eight)
    {
        return Err("历史图片不是程序输出的非交错 RGBA8 PNG".into());
    }
    let (out_width, out_height) = if small { thumb } else { (width, height) };
    let length = (u64::from(out_width) * u64::from(out_height) * 4) as usize;
    let mut output = Vec::new();
    output.try_reserve_exact(length)?;
    output.resize(length, 0);
    let mut out_y = 0_u32;
    for y in 0..height {
        if cancelled() {
            return Err("截图历史解码已取消".into());
        }
        let row = reader.next_row()?.ok_or("历史 PNG 行数据截断")?;
        if row.data().len() != width as usize * 4 {
            return Err("历史 PNG 行宽无效".into());
        }
        if small {
            if out_y < out_height
                && u64::from(out_y) * u64::from(height) / u64::from(out_height) == u64::from(y)
            {
                for x in 0..out_width {
                    let source_x =
                        (u64::from(x) * u64::from(width) / u64::from(out_width)) as usize;
                    let target = (out_y as usize * out_width as usize + x as usize) * 4;
                    output[target..target + 4]
                        .copy_from_slice(&row.data()[source_x * 4..source_x * 4 + 4]);
                }
                out_y += 1;
            }
        } else {
            let start = y as usize * width as usize * 4;
            output[start..start + width as usize * 4].copy_from_slice(row.data());
        }
    }
    reader.finish()?;
    if cancelled() {
        return Err("截图历史解码已取消".into());
    }
    Ok((out_width, out_height, output))
}

pub(crate) fn delete(entry: &Entry) -> Result<()> {
    let file = checked_file(entry, true)?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
            },
        };
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // 对已复核的同一文件句柄标记删除，避免把检查后的路径替换当成原选择。
        unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )?;
        }
    }
    #[cfg(not(windows))]
    {
        drop(file);
        std::fs::remove_file(&entry.path)?;
    }
    Ok(())
}

fn ordinary_directory(dir: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !metadata.is_dir() || reparse(&metadata) {
        return Err("截图历史目录不是普通目录".into());
    }
    Ok(true)
}
fn ordinary_file(metadata: &Metadata) -> bool {
    metadata.is_file() && !reparse(metadata)
}
fn reparse(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
fn owned_path(dir: &Path, path: &Path) -> bool {
    path.parent() == Some(dir)
        && path.file_name().is_some_and(|name| {
            let mut components = Path::new(name).components();
            matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none()
                && name.to_string_lossy().starts_with("shot-")
                && name.to_string_lossy().ends_with(".png")
        })
}
fn validate_entry(entry: &Entry) -> Result<()> {
    let dir = preferences::data_dir().join("history");
    if !owned_path(&dir, &entry.path) || !ordinary_directory(&dir)? {
        return Err("历史图片不属于当前截图历史目录".into());
    }
    let metadata = std::fs::symlink_metadata(&entry.path)?;
    if !source_matches(entry, &metadata) {
        return Err("历史图片已改变，请刷新后重试".into());
    }
    Ok(())
}
fn source_matches(entry: &Entry, metadata: &Metadata) -> bool {
    ordinary_file(metadata)
        && metadata.len() == entry.bytes
        && metadata.modified().ok() == Some(entry.modified)
}
fn checked_file(entry: &Entry, deleting: bool) -> Result<File> {
    validate_entry(entry)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // 不跟随末级重解析点；允许同一文件删除，但不允许写入。
        options.custom_flags(0x0020_0000).share_mode(0x1 | 0x4);
        if deleting {
            options.access_mode(0x0001_0000 | 0x0000_0080);
        }
    }
    #[cfg(not(windows))]
    let _ = deleting;
    let file = options.open(&entry.path)?;
    if !source_matches(entry, &file.metadata()?) {
        return Err("已打开的历史图片身份已改变，请刷新".into());
    }
    Ok(file)
}
fn png_dimensions(header: &[u8; 33]) -> Option<(u32, u32)> {
    if &header[..8] != b"\x89PNG\r\n\x1a\n"
        || header[8..12] != 13_u32.to_be_bytes()
        || &header[12..16] != b"IHDR"
    {
        return None;
    }
    let mut crc = 0xffff_ffff_u32;
    for &byte in &header[12..29] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
    }
    if !crc != u32::from_be_bytes(header[29..33].try_into().ok()?) {
        return None;
    }
    let w = u32::from_be_bytes(header[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(header[20..24].try_into().ok()?);
    (w != 0 && h != 0).then_some((w, h))
}
pub fn record(image: &Image) -> Result<()> {
    record_pixels(image.width, image.height, &image.bgra)
}

/// 完成输出后借用同一份像素写历史；调用方可持有共享 RenderImage 的 Arc。
pub(crate) fn record_pixels(width: u32, height: u32, bgra: &[u8]) -> Result<()> {
    if !preferences::Preferences::load().history {
        return Ok(());
    }
    let dir = preferences::data_dir().join("history");
    std::fs::create_dir_all(&dir)?;
    ordinary_directory(&dir)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let path = dir.join(format!(
        "shot-{timestamp}-{}-{}-{}x{}.png",
        std::process::id(),
        {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        },
        width,
        height
    ));
    export::save_png_pixels(width, height, bgra, &path)?;
    prune(&dir)
}
fn prune(dir: &std::path::Path) -> Result<()> {
    let mut bytes = 0_u64;
    let now = SystemTime::now();
    for (index, entry) in entries_in(dir)?.into_iter().enumerate() {
        bytes += entry.bytes;
        if index >= 50
            || bytes > 256 * 1024 * 1024
            || now.duration_since(entry.modified).unwrap_or_default()
                > Duration::from_secs(30 * 86400)
        {
            std::fs::remove_file(entry.path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png_fixture(width: u32, height: u32, pixels: Option<&[u8]>) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            if let Some(pixels) = pixels {
                writer.write_image_data(pixels).unwrap();
                writer.finish().unwrap();
            }
        }
        bytes
    }
    #[test]
    fn history_row_decoder_shrinks_and_preserves_color_alpha_without_full_thumbnail_output() {
        let (w, h) = (960_u32, 640_u32);
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x % 251) as u8, (y % 251) as u8, 33, 128]);
            }
        }
        let bytes = png_fixture(w, h, Some(&rgba));
        let (tw, th, thumb) =
            decode_rows(std::io::Cursor::new(&bytes), Some((w, h)), true, &|| false).unwrap();
        assert_eq!((tw, th, thumb.len()), (480, 320, 614400));
        assert_eq!(&thumb[..4], &[0, 0, 33, 128]);
        assert_eq!(
            &thumb[thumb.len() - 4..],
            &[(958 % 251) as u8, (638 % 251) as u8, 33, 128]
        );
        let (fw, fh, mut full) =
            decode_rows(std::io::Cursor::new(&bytes), None, false, &|| false).unwrap();
        assert_eq!((fw, fh), (w, h));
        assert_eq!(full, rgba);
        rgba_to_bgra(&mut full);
        assert_eq!(&full[..4], &[33, 0, 0, 128]);
    }
    #[test]
    fn history_rejects_oversized_ihdr_before_missing_body_and_changed_dimensions() {
        for (w, h) in [(65536, 1), (8001, 4000)] {
            let header = png_fixture(w, h, None);
            let error = decode_rows(std::io::Cursor::new(header), None, true, &|| false)
                .unwrap_err()
                .to_string();
            assert!(error.contains("3200万像素上限"));
        }
        let bytes = png_fixture(1, 1, Some(&[12, 34, 56, 78]));
        assert!(
            decode_rows(std::io::Cursor::new(bytes), Some((2, 1)), false, &|| false)
                .unwrap_err()
                .to_string()
                .contains("尺寸已改变")
        );
    }
    #[test]
    fn history_rejects_bad_header_tail_and_cancelled_rows() {
        let bytes = png_fixture(1, 2, Some(&[12, 34, 56, 78, 87, 65, 43, 21]));
        let header = <[u8; 33]>::try_from(&bytes[..33]).unwrap();
        assert_eq!(png_dimensions(&header), Some((1, 2)));
        let mut bad_header = header;
        bad_header[20] ^= 1;
        assert_eq!(png_dimensions(&bad_header), None);
        let mut bad_tail = bytes.clone();
        *bad_tail.last_mut().unwrap() ^= 1;
        assert!(decode_rows(std::io::Cursor::new(bad_tail), None, true, &|| false).is_err());
        let calls = std::cell::Cell::new(0);
        let error = decode_rows(std::io::Cursor::new(bytes), None, false, &|| {
            calls.set(calls.get() + 1);
            calls.get() >= 3
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("已取消"));
    }
    #[test]
    fn history_source_scope_and_metadata_reject_changed_bytes_and_non_files() {
        let root = Path::new("local-history");
        assert!(owned_path(root, &root.join("shot-one.png")));
        for path in [
            root.join("other.png"),
            root.join("child/shot-one.png"),
            root.join("../shot-one.png"),
            PathBuf::from("shot-one.png"),
        ] {
            assert!(!owned_path(root, &path));
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rustshot-history-source-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("shot-source.png");
        std::fs::write(&path, b"fixture").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let entry = Entry {
            path: path.clone(),
            bytes: metadata.len(),
            modified: metadata.modified().unwrap(),
            dimensions: None,
        };
        assert!(source_matches(&entry, &metadata));
        assert!(!source_matches(
            &entry,
            &std::fs::symlink_metadata(&dir).unwrap()
        ));
        std::fs::write(&path, b"changed fixture").unwrap();
        assert!(!source_matches(
            &entry,
            &std::fs::symlink_metadata(&path).unwrap()
        ));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn history_prunes_only_owned_pngs_and_never_loads_all_images() {
        let dir =
            std::env::temp_dir().join(format!("rustshot-history-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("leave-me.png"), b"unrelated").unwrap();
        for n in 0..55 {
            std::fs::write(dir.join(format!("shot-test-{n}.png")), b"metadata fixture").unwrap();
        }
        assert_eq!(entries_in(&dir).unwrap().len(), 55);
        prune(&dir).unwrap();
        assert_eq!(entries_in(&dir).unwrap().len(), 50);
        assert_eq!(
            std::fs::read(dir.join("leave-me.png")).unwrap(),
            b"unrelated"
        );
        for e in entries_in(&dir).unwrap() {
            std::fs::remove_file(e.path).unwrap();
        }
        std::fs::remove_file(dir.join("leave-me.png")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
