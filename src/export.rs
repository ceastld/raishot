use crate::{capture::Image, Result};
use std::{
    fs::File,
    io::{BufWriter, Write},
    mem::size_of,
    path::{Path, PathBuf},
};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HWND},
    Graphics::Gdi::*,
    System::{DataExchange::*, Memory::*},
};

pub fn save_png(image: Image, path: &Path) -> Result<()> {
    save_png_pixels(image.width, image.height, &image.bgra, path)
}

/// 借用共享底图写历史 PNG，整张长图只需额外分配通道转换的一行。
pub(crate) fn save_png_pixels(width: u32, height: u32, bgra: &[u8], path: &Path) -> Result<()> {
    pixel_bytes(width, height, bgra)?;
    let pending = PendingFile::new(path)?;
    let file = File::create(&pending.temp)?;
    write_png_pixels(
        width,
        height,
        bgra,
        BufWriter::new(file),
        png::Compression::Fast,
    )?;
    pending.commit(path, true)?;
    Ok(())
}

fn image_bytes(image: &Image) -> Result<usize> {
    pixel_bytes(image.width, image.height, &image.bgra)
}

fn pixel_bytes(width: u32, height: u32, bgra: &[u8]) -> Result<usize> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("图片尺寸溢出")?;
    if width == 0 || height == 0 || bgra.len() != expected {
        return Err("保存图片尺寸与像素数据不一致".into());
    }
    Ok(expected)
}

/// PNG 只暂存一行 RGBA，长图不再复制完整 BGRA 帧进行通道转换。
fn write_png_bgra<W: Write>(image: &Image, output: W, compression: png::Compression) -> Result<()> {
    write_png_pixels(image.width, image.height, &image.bgra, output, compression)
}

pub(crate) fn write_png_pixels<W: Write>(
    width: u32,
    height: u32,
    bgra: &[u8],
    output: W,
    compression: png::Compression,
) -> Result<()> {
    pixel_bytes(width, height, bgra)?;
    let mut encoder = png::Encoder::new(output, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(compression);
    let mut writer = encoder.write_header()?;
    let mut stream = writer.stream_writer()?;
    let row_bytes = width as usize * 4;
    let mut row = vec![0; row_bytes];
    for source in bgra.chunks_exact(row_bytes) {
        for (rgba, bgra) in row.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
            rgba.copy_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
        }
        stream.write_all(&row)?;
    }
    stream.finish()?;
    writer.finish()?;
    Ok(())
}

struct PendingFile {
    temp: PathBuf,
}
impl PendingFile {
    fn new(path: &Path) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        path.file_name().ok_or("保存文件名无效")?;
        // 临时名不含目标名，长度恒定，不会让接近上限的合法文件名在创建时失败。
        let temp = path.with_file_name(format!(
            ".rustshot-{}-{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        Ok(Self { temp })
    }
    fn commit(self, path: &Path, overwrite: bool) -> Result<()> {
        self.commit_if(path, overwrite, || true).map(|_| ())
    }
    fn commit_if(
        self,
        path: &Path,
        overwrite: bool,
        can_commit: impl FnOnce() -> bool,
    ) -> Result<bool> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{core::PCWSTR, Win32::Storage::FileSystem::*};
        // std::fs 可创建深层文件；直接把普通路径传给 Win32 会重新触发 MAX_PATH。
        // 解析已存在的临时文件与目标父目录，保留 verbatim/UNC；目标不必已存在。
        let source = std::fs::canonicalize(&self.temp)?;
        let target = std::path::absolute(path)?;
        let target = std::fs::canonicalize(target.parent().ok_or("保存目标没有父目录")?)?
            .join(target.file_name().ok_or("保存目标没有文件名")?);
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut flags = MOVEFILE_WRITE_THROUGH;
        if overwrite {
            flags |= MOVEFILE_REPLACE_EXISTING;
        }
        if !can_commit() {
            return Ok(false);
        }
        // 快存不使用 replace：存在性检查与提交之间的新文件也不能被覆盖。
        unsafe { MoveFileExW(PCWSTR(source.as_ptr()), PCWSTR(target.as_ptr()), flags) }
            .map_err(|e| std::io::Error::from_raw_os_error((e.code().0 as u32 & 0xffff) as i32))?;
        Ok(true)
    }
}
impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
    }
}
/// 无 alpha 的保存、旧式剪贴板与识别共用白底合成，透明边缘不会变黑。
pub(crate) fn flatten_on_white(bytes: &mut [u8]) {
    for p in bytes.chunks_exact_mut(4) {
        let a = p[3] as u32;
        for c in &mut p[..3] {
            *c = ((*c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        }
        p[3] = 255;
    }
}
pub fn save_image(image: Image, path: &Path) -> Result<()> {
    let format = crate::save_options::SaveFormat::from_path(path).ok_or(tr!(
        "支持 PNG、JPG/JPEG、BMP 或无损 WebP，请保留正确扩展名",
        "Use a .png, .jpg/.jpeg, .bmp or lossless .webp extension"
    ))?;
    if format == crate::save_options::SaveFormat::Png {
        return save_png(image, path);
    }
    let encoding = crate::save_options::SaveEncoding {
        format,
        ..Default::default()
    };
    save_encoded(&encode_image(&image, encoding)?, path, true)
}

/// 编码与落盘分离：保存面板后台计算准确大小，确认后复用同一份字节。
pub(crate) fn encode_image(
    image: &Image,
    encoding: crate::save_options::SaveEncoding,
) -> Result<Vec<u8>> {
    encode_image_cancellable(image, encoding, &std::sync::atomic::AtomicBool::new(false))
}

/// 原生有损WebP在编码回调中响应取消；其它编码器在整个阶段前后响应。
pub(crate) fn encode_image_cancellable(
    image: &Image,
    encoding: crate::save_options::SaveEncoding,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<u8>> {
    let check_cancel = || -> Result<()> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(tr!("图片编码已取消", "Image encoding canceled").into());
        }
        Ok(())
    };
    check_cancel()?;
    let result = encode_image_with_cancel(image, encoding, cancel);
    check_cancel()?;
    result
}

fn encode_image_with_cancel(
    image: &Image,
    encoding: crate::save_options::SaveEncoding,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<u8>> {
    use crate::save_options::{PngCompression, SaveFormat};
    let expected = image_bytes(image)?;
    encoding.validate()?;
    let mut bytes = Vec::new();
    if encoding.format == SaveFormat::Png {
        write_png_bgra(
            image,
            &mut bytes,
            match encoding.png_compression {
                PngCompression::Fast => png::Compression::Fast,
                PngCompression::Balanced => png::Compression::Balanced,
                PngCompression::Small => png::Compression::High,
            },
        )?;
        return Ok(bytes);
    }
    if encoding.format == SaveFormat::Webp {
        if encoding.webp.mode == crate::image_webp_encode::WebpMode::Lossy {
            return crate::image_webp_encode::encode_lossy_bgra(
                image,
                encoding.webp.quality,
                0,
                128 * 1024 * 1024,
                cancel,
            );
        }
        // 无损WebP的格式单边上限和统一读回的像素预算，在RGBA整帧分配前检查。
        if image.width > 16_384
            || image.height > 16_384
            || u64::from(image.width) * u64::from(image.height)
                > crate::image_decode::MAX_DECODE_PIXELS
        {
            return Err(tr!(
                "无损 WebP 超过 16384 单边或 3200 万像素限制",
                "Lossless WebP exceeds 16384 px per side or 32 megapixels"
            )
            .into());
        }
        // 输入与输出均为直alpha；无损WebP不借用JPEG质量或白底合成规则。
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(expected)?;
        for pixel in image.bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        image::codecs::webp::WebPEncoder::new_lossless(&mut bytes).encode(
            &rgba,
            image.width,
            image.height,
            image::ExtendedColorType::Rgba8,
        )?;
        return Ok(bytes);
    }
    let mut rgb = Vec::with_capacity(expected / 4 * 3);
    for p in image.bgra.chunks_exact(4) {
        let a = u32::from(p[3]);
        for c in [p[2], p[1], p[0]] {
            rgb.push(((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8);
        }
    }
    if encoding.format == SaveFormat::Bmp {
        image::codecs::bmp::BmpEncoder::new(&mut bytes).encode(
            &rgb,
            image.width,
            image.height,
            image::ExtendedColorType::Rgb8,
        )?;
    } else {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, encoding.jpeg_quality)
            .encode(
                &rgb,
                image.width,
                image.height,
                image::ExtendedColorType::Rgb8,
            )?;
    }
    Ok(bytes)
}

pub(crate) fn save_encoded(bytes: &[u8], path: &Path, overwrite: bool) -> Result<()> {
    save_encoded_if(bytes, path, overwrite, || true).map(|_| ())
}

/// true 表示已经提交；false 表示在原子提交前取消，暂存由 Drop 清理。
/// 提交一旦成功就保持成功，迟到的取消不能改写已发生的结果。
pub(crate) fn save_encoded_cancellable(
    bytes: &[u8],
    path: &Path,
    overwrite: bool,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<bool> {
    save_encoded_if(bytes, path, overwrite, || {
        !cancel.load(std::sync::atomic::Ordering::Acquire)
    })
}

fn save_encoded_if(
    bytes: &[u8],
    path: &Path,
    overwrite: bool,
    mut can_commit: impl FnMut() -> bool,
) -> Result<bool> {
    if !can_commit() {
        return Ok(false);
    }
    let pending = PendingFile::new(path)?;
    let mut file = File::create(&pending.temp)?;
    for chunk in bytes.chunks(256 * 1024) {
        if !can_commit() {
            return Ok(false);
        }
        file.write_all(chunk)?;
    }
    if !can_commit() {
        return Ok(false);
    }
    file.sync_all()?;
    drop(file);
    pending.commit_if(path, overwrite, can_commit)
}

/// 统一检查实际路径；尚未创建的目标也先解析父目录，避免相对路径绕过原图保护。
pub(crate) fn is_same_path(a: &Path, b: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    fn resolved(path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok().or_else(|| {
            let absolute = std::path::absolute(path).ok()?;
            let parent = std::fs::canonicalize(absolute.parent()?).ok()?;
            Some(parent.join(absolute.file_name()?))
        })
    }
    let (Some(a), Some(b)) = (resolved(a), resolved(b)) else {
        return false;
    };
    let a: Vec<_> = a.as_os_str().encode_wide().collect();
    let b: Vec<_> = b.as_os_str().encode_wide().collect();
    unsafe {
        windows::Win32::Globalization::CompareStringOrdinal(&a, &b, true)
            == windows::Win32::Globalization::CSTR_EQUAL
    }
}
/// 文件与剪贴板解码共用的单边上限；内存由 256MB 分配与 3200 万像素共同约束。
pub(crate) const MAX_DECODE_SIDE: u32 = 65_535;
pub(crate) fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(256 * 1024 * 1024);
    limits
}
/// 带文件名的本地化解码错误，避免只显示 image 库的英文原文。
fn open_failed(path: &Path, error: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    trf!(
        "无法打开图片 {name}：{error}",
        "Can't open image {name}: {error}"
    )
    .into()
}
pub fn read_image(path: &Path) -> Result<Image> {
    crate::image_decode::decode_path(path)
        .map(|decoded| decoded.image)
        .map_err(|error| open_failed(path, error))
}

struct Clipboard;
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

// 格式在打开剪贴板之前编码并分配，失败不会提前清空用户的剪贴板。
struct ClipboardPayloads {
    png: GlobalImage,
    dibv5: GlobalImage,
    legacy: GlobalImage,
    placement: Option<GlobalImage>,
}

/// 后台准备、由窗口线程确认请求仍有效后消耗式发布的图片数据。
pub(crate) struct PreparedImageClipboard {
    payloads: ClipboardPayloads,
    _not_sync: std::marker::PhantomData<std::cell::Cell<()>>,
}

// 仅持有独占、未发布的 GMEM_MOVEABLE 内存；构造返回前所有 GlobalLock 已解除。
// 没有指针或借用跨线程，发布消耗所有权，未移交给系统的内存由 Drop 释放。
unsafe impl Send for PreparedImageClipboard {}

pub(crate) fn prepare_image_clipboard(image: &Image) -> Result<PreparedImageClipboard> {
    PreparedImageClipboard::at(image, None)
}

pub(crate) fn prepare_image_clipboard_cancellable(
    image: &Image,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<PreparedImageClipboard> {
    Ok(PreparedImageClipboard {
        payloads: ClipboardPayloads::at_with_cancel(image, None, Some(cancel))?,
        _not_sync: std::marker::PhantomData,
    })
}

const CLIPBOARD_MAX_BYTES: u64 = 256 * 1024 * 1024;
const CLIPBOARD_CODEC_RESERVE: u64 = 4 * 1024 * 1024;

fn clipboard_budget_error() -> Box<dyn std::error::Error> {
    tr!(
        "图片复制超过可见缓冲预算",
        "Image copy exceeds the visible-buffer budget"
    )
    .into()
}

#[derive(Clone, Copy)]
struct ClipboardBudget {
    resident_bytes: u64,
    max_bytes: u64,
    png_workspace: u64,
}

impl ClipboardBudget {
    fn new(image: &Image, resident_bytes: u64, max_bytes: u64) -> Result<Self> {
        image_bytes(image)?;
        if resident_bytes < image.bgra.capacity() as u64 {
            return Err(clipboard_budget_error());
        }
        // PNG 0.18的stream有三条4W行，本层另有4W通道行和4KiB chunk。
        // flate2另有32KiB writer；固定余量覆盖锁版miniz字典/哈希/输出数组，
        // 以及zlib-rs默认window=15、mem_level=8的窗口、符号与pending缓冲。
        let png_workspace = u64::from(image.width)
            .checked_mul(16)
            .and_then(|bytes| bytes.checked_add(4 * 1024))
            .and_then(|bytes| bytes.checked_add(CLIPBOARD_CODEC_RESERVE))
            .ok_or_else(clipboard_budget_error)?;
        let budget = Self {
            resident_bytes,
            max_bytes: max_bytes.min(CLIPBOARD_MAX_BYTES),
            png_workspace,
        };
        budget.png(0)?;
        // PNG至少还要两个DIB；这些确定长度先检查，后续再加实际PNG容量。
        budget.extra(Self::dib_bytes(image_bytes(image)?)?)?;
        Ok(budget)
    }

    fn extra(self, bytes: u64) -> Result<()> {
        self.resident_bytes
            .checked_add(bytes)
            .filter(|total| *total <= self.max_bytes)
            .ok_or_else(clipboard_budget_error)?;
        Ok(())
    }

    fn png(self, capacity: usize) -> Result<()> {
        let extra = self
            .png_workspace
            .checked_add(capacity as u64)
            .ok_or_else(clipboard_budget_error)?;
        self.extra(extra)
    }

    fn transfer(self, encoded_capacity: usize, native_bytes: u64) -> Result<()> {
        self.extra(
            (encoded_capacity as u64)
                .checked_add(native_bytes)
                .ok_or_else(clipboard_budget_error)?,
        )
    }

    fn dib_bytes(pixels: usize) -> Result<u64> {
        (pixels as u64)
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(size_of::<BITMAPV5HEADER>() as u64))
            .and_then(|bytes| bytes.checked_add(size_of::<BITMAPINFOHEADER>() as u64))
            .ok_or_else(clipboard_budget_error)
    }

    fn grow(self, output: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
        let required = output
            .len()
            .checked_add(bytes.len())
            .ok_or_else(clipboard_budget_error)?;
        if required <= output.capacity() {
            self.png(output.capacity())?;
            output.extend_from_slice(bytes);
            return Ok(());
        }
        // 显式保留旧/新Vec的短时共存；不把realloc能原地扩展作为预算假定。
        let available = self
            .max_bytes
            .checked_sub(self.resident_bytes)
            .and_then(|bytes| bytes.checked_sub(self.png_workspace))
            .and_then(|bytes| bytes.checked_sub(output.capacity() as u64))
            .ok_or_else(clipboard_budget_error)?;
        if required as u64 > available {
            return Err(clipboard_budget_error());
        }
        let wanted = required.max(output.capacity().saturating_mul(2));
        let wanted = wanted.min(usize::try_from(available).unwrap_or(usize::MAX));
        let mut grown = Vec::new();
        grown.try_reserve_exact(wanted)?;
        let simultaneous = output
            .capacity()
            .checked_add(grown.capacity())
            .ok_or_else(clipboard_budget_error)?;
        self.png(simultaneous)?;
        grown.extend_from_slice(output);
        grown.extend_from_slice(bytes);
        std::mem::swap(output, &mut grown);
        Ok(())
    }
}

/// resident_bytes包含借入image的实际capacity及调用方仍保留的所有像素/元数据。
/// 新入口使用无压缩的无损PNG，使有界writer的错误经Flate2 finish正常传播；
/// 旧Fast路径的fdeflate终结阶段会unwrap写出错误，因此保留旧入口、不借它做有界准备。
/// 本层核可见Vec/HGLOBAL与锁版codec工作区，不保证分配器或系统的内部峰值。
pub(crate) fn prepare_image_clipboard_with_budget_cancellable(
    image: &Image,
    cancel: &std::sync::atomic::AtomicBool,
    resident_bytes: u64,
    max_bytes: u64,
) -> Result<PreparedImageClipboard> {
    check_clipboard_prepare_cancel(Some(cancel))?;
    let budget = ClipboardBudget::new(image, resident_bytes, max_bytes)?;
    let payloads = ClipboardPayloads::at_with_budget(image, None, Some(cancel), Some(budget));
    check_clipboard_prepare_cancel(Some(cancel))?;
    Ok(PreparedImageClipboard {
        payloads: payloads?,
        _not_sync: std::marker::PhantomData,
    })
}

impl PreparedImageClipboard {
    fn at(image: &Image, placement: Option<ClipboardPlacement>) -> Result<Self> {
        Ok(Self {
            payloads: ClipboardPayloads::at(image, placement)?,
            _not_sync: std::marker::PhantomData,
        })
    }

    /// 只尝试一次打开剪贴板，不在窗口线程等待其它程序释放它。
    pub(crate) fn publish(self, owner: HWND) -> Result<()> {
        self.publish_with_attempts(owner, 1, None)
    }

    /// 保留准备期间用户在其它窗口复制的新内容；检查在剪贴板独占锁内完成。
    pub(crate) fn publish_if_unchanged(self, owner: HWND, sequence: u32) -> Result<()> {
        if sequence == 0 {
            return Err(tr!(
                "无法读取剪贴板状态，请重试",
                "Cannot read clipboard state; try again"
            )
            .into());
        }
        self.publish_with_attempts(owner, 1, Some(sequence))
    }

    fn publish_with_attempts(
        self,
        owner: HWND,
        attempts: usize,
        expected_sequence: Option<u32>,
    ) -> Result<()> {
        if owner.0.is_null()
            || !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(owner)) }.as_bool()
        {
            return Err(tr!(
                "图片复制窗口已失效",
                "The copy window is no longer available"
            )
            .into());
        }
        let ClipboardPayloads {
            png,
            dibv5,
            legacy,
            placement,
        } = self.payloads;
        let png_format = unsafe { RegisterClipboardFormatW(windows::core::w!("PNG")) };
        if png_format == 0 {
            return Err("无法注册 PNG 剪贴板格式".into());
        }
        let placement_format = placement.as_ref().map(|_| placement_format());
        if placement_format == Some(0) {
            return Err(tr!(
                "无法注册贴图位置剪贴板格式",
                "Can't register the pin position clipboard format"
            )
            .into());
        }
        let mut opened = false;
        for attempt in 0..attempts {
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                opened = true;
                break;
            }
            if attempt + 1 < attempts {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        if !opened {
            return Err("剪贴板正被其他程序占用，请重试".into());
        }
        let _clipboard = Clipboard;
        if expected_sequence
            .is_some_and(|expected| unsafe { GetClipboardSequenceNumber() } != expected)
        {
            return Err(tr!(
                "剪贴板内容已更新，未覆盖新内容；请重新复制当前帧",
                "Clipboard changed; copy the frame again"
            )
            .into());
        }
        unsafe {
            EmptyClipboard()?;
        }
        // 每种格式成功后由系统接管；后续格式失败时不能撤回已发布的格式。
        png.publish(png_format)?;
        dibv5.publish(17)?;
        legacy.publish(8)?;
        if let (Some(placement), Some(format)) = (placement, placement_format) {
            placement.publish(format)?;
        }
        Ok(())
    }
}

impl ClipboardPayloads {
    #[cfg(test)]
    fn new(image: &Image) -> Result<Self> {
        Self::at(image, None)
    }
    fn at(image: &Image, placement: Option<ClipboardPlacement>) -> Result<Self> {
        Self::at_with_cancel(image, placement, None)
    }
    fn at_with_cancel(
        image: &Image,
        placement: Option<ClipboardPlacement>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Self> {
        Self::at_with_budget(image, placement, cancel, None)
    }
    fn at_with_budget(
        image: &Image,
        placement: Option<ClipboardPlacement>,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        budget: Option<ClipboardBudget>,
    ) -> Result<Self> {
        check_clipboard_prepare_cancel(cancel)?;
        // 有界公开入口不带placement；旧位置元数据入口保持原行为。
        if budget.is_some() && placement.is_some() {
            return Err(clipboard_budget_error());
        }
        let expected = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("图像尺寸溢出")?;
        if image.width == 0
            || image.height == 0
            || image.width > i32::MAX as u32
            || image.height > i32::MAX as u32
            || expected != image.bgra.len()
            || expected > u32::MAX as usize
        {
            return Err("剪贴板图像尺寸或像素无效".into());
        }
        let mut encoded = Vec::new();
        write_png_bgra(
            image,
            ClipboardPrepareOutput {
                output: &mut encoded,
                cancel,
                budget,
            },
            if budget.is_some() {
                png::Compression::NoCompression
            } else {
                png::Compression::Fast
            },
        )?;
        check_clipboard_prepare_cancel(cancel)?;
        if let Some(budget) = budget {
            budget.png(encoded.capacity())?;
            budget.transfer(encoded.capacity(), encoded.len() as u64)?;
        }
        let png = GlobalImage::new(&encoded)?;
        let png_bytes = if let Some(budget) = budget {
            let actual = png.allocated_bytes()? as u64;
            budget.transfer(encoded.capacity(), actual)?;
            actual
        } else {
            0
        };
        let placement = placement
            .map(|placement| {
                GlobalImage::new(&encode_placement(
                    placement,
                    &encoded,
                    (image.width, image.height),
                )?)
            })
            .transpose()?;
        // PNG 是变长压缩结果；复制进最终 HGLOBAL 后即释放，不与两份 DIB 中间帧叠加。
        drop(encoded);
        check_clipboard_prepare_cancel(cancel)?;
        if let Some(budget) = budget {
            budget.extra(
                png_bytes
                    .checked_add(ClipboardBudget::dib_bytes(expected)?)
                    .ok_or_else(clipboard_budget_error)?,
            )?;
        }
        let v5 = BITMAPV5HEADER {
            bV5Size: size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: image.width as i32,
            bV5Height: -(image.height as i32),
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_BITFIELDS,
            bV5SizeImage: expected as u32,
            bV5RedMask: 0x00ff0000,
            bV5GreenMask: 0x0000ff00,
            bV5BlueMask: 0x000000ff,
            bV5AlphaMask: 0xff000000,
            bV5CSType: 0x73524742,
            bV5Intent: LCS_GM_IMAGES as u32,
            ..Default::default()
        };
        let dibv5 = GlobalImage::dib(&v5, &image.bgra, false)?;
        check_clipboard_prepare_cancel(cancel)?;
        let v5_bytes = if let Some(budget) = budget {
            let actual = dibv5.allocated_bytes()? as u64;
            budget.extra(
                png_bytes
                    .checked_add(actual)
                    .and_then(|bytes| bytes.checked_add(expected as u64))
                    .and_then(|bytes| bytes.checked_add(size_of::<BITMAPINFOHEADER>() as u64))
                    .ok_or_else(clipboard_budget_error)?,
            )?;
            actual
        } else {
            0
        };
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: image.width as i32,
            biHeight: -(image.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: expected as u32,
            ..Default::default()
        };
        let legacy = GlobalImage::dib(&header, &image.bgra, true)?;
        check_clipboard_prepare_cancel(cancel)?;
        if let Some(budget) = budget {
            let legacy_bytes = legacy.allocated_bytes()? as u64;
            budget.extra(
                png_bytes
                    .checked_add(v5_bytes)
                    .and_then(|bytes| bytes.checked_add(legacy_bytes))
                    .ok_or_else(clipboard_budget_error)?,
            )?;
        }
        Ok(Self {
            png,
            dibv5,
            legacy,
            placement,
        })
    }
}

fn check_clipboard_prepare_cancel(cancel: Option<&std::sync::atomic::AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire)) {
        Err(tr!("已取消图片复制", "Image copy canceled").into())
    } else {
        Ok(())
    }
}

/// PNG 流写出时检查取消，不必等整张大图编码完再丢弃。
struct ClipboardPrepareOutput<'a> {
    output: &'a mut Vec<u8>,
    cancel: Option<&'a std::sync::atomic::AtomicBool>,
    budget: Option<ClipboardBudget>,
}
impl Write for ClipboardPrepareOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        check_clipboard_prepare_cancel(self.cancel)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if let Some(budget) = self.budget {
            budget
                .grow(self.output, bytes)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            Ok(bytes.len())
        } else {
            self.output.write(bytes)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        check_clipboard_prepare_cancel(self.cancel)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.output.flush()
    }
}

/// 随图片写进同一次剪贴板的贴图落点，坐标为虚拟桌面物理像素。
/// 再贴图（剪贴板贴图入口）按它还原位置；其它软件只看到普通图片格式。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ClipboardPlacement {
    /// 原位：`origin` 是图片左上角，`scale` 是复制时的显示倍率（贴图缩小显示时小于 1）。
    /// 截图复制为含外观留白的导出左上角、倍率 1；贴图复制为贴图当前位置与倍率。
    Exact { origin: (i32, i32), scale: f32 },
    /// 长截图：保存选区与滚动方向，贴图时按 `pin_model::scroll_origin` 现算落点，
    /// 与长截图直接贴图一致（滚动方向居中屏幕，另一方向保持选区位置）。
    Scroll {
        axis: crate::stitch::ScrollAxis,
        region: crate::geometry::ScreenRect,
    },
}

/// 剪贴板格式名不带版本，内容的 `version` 决定字段；读不懂的版本整条忽略。
const PLACEMENT_VERSION: u32 = 2;
const PLACEMENT_LIMIT: usize = 4096;

/// UTF-8 JSON；`png_bytes`／`png_sha256` 把落点绑定到同一份 PNG，宽高与 PNG 头一致。
#[derive(serde::Serialize, serde::Deserialize)]
struct PlacementWire {
    version: u32,
    png_bytes: u64,
    png_sha256: String,
    width: u32,
    height: u32,
    #[serde(flatten)]
    layout: LayoutWire,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "layout", rename_all = "snake_case")]
enum LayoutWire {
    Exact {
        origin: (i32, i32),
        scale: f32,
    },
    Scroll {
        axis: AxisWire,
        /// 左、上、宽、高。
        region: (i32, i32, u32, u32),
    },
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum AxisWire {
    Vertical,
    Horizontal,
}

fn png_sha256(png: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(png))
}

/// PNG 签名后紧跟 IHDR，宽高为大端 u32。
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    Some((
        u32::from_be_bytes(png[16..20].try_into().ok()?),
        u32::from_be_bytes(png[20..24].try_into().ok()?),
    ))
}

fn valid_placement(placement: ClipboardPlacement) -> bool {
    match placement {
        ClipboardPlacement::Exact { scale, .. } => scale.is_finite() && scale > 0. && scale <= 1.,
        ClipboardPlacement::Scroll { region, .. } => {
            region.width > 0
                && region.height > 0
                && region.width <= i32::MAX as u32
                && region.height <= i32::MAX as u32
        }
    }
}

fn encode_placement(
    placement: ClipboardPlacement,
    png: &[u8],
    size: (u32, u32),
) -> Result<Vec<u8>> {
    if !valid_placement(placement) {
        return Err("贴图落点无效".into());
    }
    let layout = match placement {
        ClipboardPlacement::Exact { origin, scale } => LayoutWire::Exact { origin, scale },
        ClipboardPlacement::Scroll { axis, region } => LayoutWire::Scroll {
            axis: match axis {
                crate::stitch::ScrollAxis::Vertical => AxisWire::Vertical,
                crate::stitch::ScrollAxis::Horizontal => AxisWire::Horizontal,
            },
            region: (region.left, region.top, region.width, region.height),
        },
    };
    Ok(serde_json::to_vec(&PlacementWire {
        version: PLACEMENT_VERSION,
        png_bytes: png.len() as u64,
        png_sha256: png_sha256(png),
        width: size.0,
        height: size.1,
        layout,
    })?)
}

/// 落点只属于同一份 PNG；剪贴板管理器保留旧私有格式时不能误用旧落点。
/// 返回落点与原始 PNG 长度（GlobalSize 可能带分配器尾部对齐字节）。
fn decode_placement(bytes: &[u8], png: &[u8]) -> Option<(ClipboardPlacement, usize)> {
    let json = bytes.split(|&byte| byte == 0).next()?;
    let wire: PlacementWire = serde_json::from_slice(json).ok()?;
    if wire.version != PLACEMENT_VERSION {
        return None;
    }
    let length = usize::try_from(wire.png_bytes).ok()?;
    let encoded = png.get(..length).filter(|png| !png.is_empty())?;
    if png_sha256(encoded) != wire.png_sha256
        || png_dimensions(encoded) != Some((wire.width, wire.height))
    {
        return None;
    }
    let placement = match wire.layout {
        LayoutWire::Exact { origin, scale } => ClipboardPlacement::Exact { origin, scale },
        LayoutWire::Scroll { axis, region } => ClipboardPlacement::Scroll {
            axis: match axis {
                AxisWire::Vertical => crate::stitch::ScrollAxis::Vertical,
                AxisWire::Horizontal => crate::stitch::ScrollAxis::Horizontal,
            },
            region: crate::geometry::ScreenRect {
                left: region.0,
                top: region.1,
                width: region.2,
                height: region.3,
            },
        },
    };
    valid_placement(placement).then_some((placement, length))
}

fn placement_format() -> u32 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("RustShot.PinPlacement")) }
}

// 旧版只写截图左上角的定长记录；仍读取，便于新旧版本混用时原位再贴图。
const SCREEN_POSITION_MAGIC: &[u8; 8] = b"RSTPOS01";
const SCREEN_POSITION_BYTES: usize = 56;

#[cfg(test)]
fn encode_screen_position(origin: (i32, i32), png: &[u8]) -> [u8; SCREEN_POSITION_BYTES] {
    use sha2::{Digest, Sha256};
    let mut bytes = [0; SCREEN_POSITION_BYTES];
    bytes[..8].copy_from_slice(SCREEN_POSITION_MAGIC);
    bytes[8..12].copy_from_slice(&origin.0.to_le_bytes());
    bytes[12..16].copy_from_slice(&origin.1.to_le_bytes());
    bytes[16..24].copy_from_slice(&(png.len() as u64).to_le_bytes());
    bytes[24..].copy_from_slice(&Sha256::digest(png));
    bytes
}

fn decode_screen_position(bytes: &[u8], png: &[u8]) -> Option<((i32, i32), usize)> {
    use sha2::{Digest, Sha256};
    if bytes.len() < SCREEN_POSITION_BYTES || &bytes[..8] != SCREEN_POSITION_MAGIC {
        return None;
    }
    let length = usize::try_from(u64::from_le_bytes(bytes[16..24].try_into().ok()?)).ok()?;
    let encoded = png.get(..length).filter(|png| !png.is_empty())?;
    if Sha256::digest(encoded).as_slice() != &bytes[24..SCREEN_POSITION_BYTES] {
        return None;
    }
    Some((
        (
            i32::from_le_bytes(bytes[8..12].try_into().ok()?),
            i32::from_le_bytes(bytes[12..16].try_into().ok()?),
        ),
        length,
    ))
}

fn screen_position_format() -> u32 {
    unsafe { RegisterClipboardFormatW(windows::core::w!("RustShot.ScreenPosition.v1")) }
}

/// 在同一次剪贴板锁内读取 PNG 和它的落点，避免两次读取之间被其它程序替换。
/// PNG 优先保留原始 alpha；其它格式继续交给现有 GPUI 入口。
pub(crate) fn clipboard_png() -> Result<Option<(Vec<u8>, Option<ClipboardPlacement>)>> {
    let png_format = unsafe { RegisterClipboardFormatW(windows::core::w!("PNG")) };
    if png_format == 0 || unsafe { OpenClipboard(None) }.is_err() {
        return Ok(None);
    }
    let _clipboard = Clipboard;
    if unsafe { IsClipboardFormatAvailable(png_format) }.is_err() {
        return Ok(None);
    }
    let mut png = clipboard_bytes(png_format, 256 * 1024 * 1024).ok_or(tr!(
        "无法读取剪贴板 PNG 图片，或图片超过 256 MB 上限",
        "Can't read the clipboard PNG, or it exceeds the 256 MB limit"
    ))?;
    let placement = clipboard_bytes(placement_format(), PLACEMENT_LIMIT)
        .and_then(|bytes| decode_placement(&bytes, &png))
        .or_else(|| {
            clipboard_bytes(screen_position_format(), PLACEMENT_LIMIT)
                .and_then(|bytes| decode_screen_position(&bytes, &png))
                .map(|(origin, length)| (ClipboardPlacement::Exact { origin, scale: 1. }, length))
        });
    if let Some((_, length)) = placement {
        png.truncate(length);
    }
    Ok(Some((png, placement.map(|(placement, _)| placement))))
}

/// 调用方持有 Clipboard；只复制有上限的 HGLOBAL，读取期间由 RAII 解锁。
fn clipboard_bytes(format: u32, limit: usize) -> Option<Vec<u8>> {
    if format == 0 {
        return None;
    }
    unsafe {
        let allocation = windows::Win32::Foundation::HGLOBAL(GetClipboardData(format).ok()?.0);
        let length = GlobalSize(allocation);
        if length == 0 || length > limit {
            return None;
        }
        let ptr = GlobalLock(allocation).cast::<u8>();
        if ptr.is_null() {
            return None;
        }
        let _lock = LockedGlobal(allocation);
        Some(std::slice::from_raw_parts(ptr, length).to_vec())
    }
}

struct GlobalImage {
    handle: Option<windows::Win32::Foundation::HGLOBAL>,
    #[cfg(test)]
    length: usize,
}
struct LockedGlobal(windows::Win32::Foundation::HGLOBAL);
impl Drop for LockedGlobal {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.0);
        }
    }
}
impl GlobalImage {
    fn allocated_bytes(&self) -> Result<usize> {
        let handle = self.handle.ok_or_else(clipboard_budget_error)?;
        let bytes = unsafe { GlobalSize(handle) };
        if bytes == 0 {
            return Err(tr!(
                "无法核实剪贴板缓冲容量",
                "Cannot verify clipboard buffer capacity"
            )
            .into());
        }
        Ok(bytes)
    }
    fn new(bytes: &[u8]) -> Result<Self> {
        Self::allocate(bytes.len(), |target| target.copy_from_slice(bytes))
    }
    fn dib<T>(header: &T, pixels: &[u8], white: bool) -> Result<Self> {
        let length = size_of::<T>()
            .checked_add(pixels.len())
            .ok_or("位图尺寸溢出")?;
        Self::allocate(length, |target| {
            // 标准 Windows 位图头没有 padding；数据紧随头部，无 BITMAPFILEHEADER。
            let header_bytes = unsafe {
                std::slice::from_raw_parts((header as *const T).cast::<u8>(), size_of::<T>())
            };
            target[..size_of::<T>()].copy_from_slice(header_bytes);
            let destination = &mut target[size_of::<T>()..];
            destination.copy_from_slice(pixels);
            if white {
                flatten_on_white(destination);
            }
        })
    }
    fn allocate(length: usize, fill: impl FnOnce(&mut [u8])) -> Result<Self> {
        unsafe {
            let allocation = GlobalAlloc(GMEM_MOVEABLE, length)?;
            let owned = Self {
                handle: Some(allocation),
                #[cfg(test)]
                length,
            };
            let ptr = GlobalLock(allocation).cast::<u8>();
            if ptr.is_null() {
                return Err("无法分配剪贴板图像".into());
            }
            let lock = LockedGlobal(allocation);
            fill(std::slice::from_raw_parts_mut(ptr, length));
            drop(lock);
            Ok(owned)
        }
    }
    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        unsafe {
            let allocation = self.handle.expect("未发布的私有 HGLOBAL");
            let ptr = GlobalLock(allocation).cast::<u8>();
            assert!(!ptr.is_null());
            let _lock = LockedGlobal(allocation);
            std::slice::from_raw_parts(ptr, self.length).to_vec()
        }
    }
    fn publish(mut self, format: u32) -> Result<()> {
        unsafe {
            SetClipboardData(format, Some(HANDLE(self.handle.unwrap().0)))?;
        }
        self.handle = None;
        Ok(())
    }
}
impl Drop for GlobalImage {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            unsafe {
                let _ = GlobalFree(Some(h));
            }
        }
    }
}
fn unicode_text_bytes(text: &str) -> Result<Vec<u8>> {
    if text.contains('\0') {
        return Err("文字包含剪贴板无法表示的空字符".into());
    }
    let mut bytes = Vec::new();
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(bytes)
}

/// 结果层的可失败文字输出；先准备私有内存，再打开和发布，不自行关闭截图。
pub(crate) fn copy_text(text: &str, owner: HWND) -> Result<()> {
    if owner.0.is_null() {
        return Err("文字复制窗口已失效".into());
    }
    let payload = GlobalImage::new(&unicode_text_bytes(text)?)?;
    unsafe {
        // 不在 UI 线程等待其它应用释放剪贴板；保留正文供用户重试。
        OpenClipboard(Some(owner)).map_err(|_| "剪贴板正被其他程序占用，请重试")?;
    }
    let _clipboard = Clipboard;
    unsafe {
        EmptyClipboard()?;
    }
    payload.publish(13)?; // CF_UNICODETEXT，使用现有 windows 运行依赖。
    Ok(())
}

pub fn copy_image(image: &Image, owner: HWND) -> Result<()> {
    copy_image_at(image, owner, None)
}

/// origin 是包含外观留白后的导出图左上角，单位为虚拟桌面物理像素。
pub(crate) fn copy_image_at(image: &Image, owner: HWND, origin: Option<(i32, i32)>) -> Result<()> {
    copy_image_with(
        image,
        owner,
        origin.map(|origin| ClipboardPlacement::Exact { origin, scale: 1. }),
    )
}

/// 图片与落点在同一次剪贴板写入里发布（同一个数据项）；其它软件只按图片格式粘贴。
pub(crate) fn copy_image_with(
    image: &Image,
    owner: HWND,
    placement: Option<ClipboardPlacement>,
) -> Result<()> {
    PreparedImageClipboard::at(image, placement)?.publish_with_attempts(owner, 8, None)
}
#[cfg(test)]
mod unicode_clipboard_encoding_tests {
    use super::*;

    #[test]
    fn text_payload_keeps_exact_unicode_whitespace_and_adds_one_terminator() {
        let text = " 中文😀\r\n\t ";
        let bytes = unicode_text_bytes(text).unwrap();
        let units = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        assert_eq!(units.last(), Some(&0));
        assert_eq!(String::from_utf16(&units[..units.len() - 1]).unwrap(), text);
        assert_eq!(unicode_text_bytes("").unwrap(), [0, 0]);
    }

    #[test]
    fn text_with_embedded_null_fails_before_native_clipboard_access() {
        assert!(unicode_text_bytes("before\0after").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_save_cancel_before_commit_preserves_target_and_cleans_staging() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/export-cancel-unit")
            .join(format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("preserve.png");
        std::fs::write(&target, b"original").unwrap();
        let pending = PendingFile::new(&target).unwrap();
        let staged = pending.temp.clone();
        let mut file = File::create(&staged).unwrap();
        file.write_all(b"replacement").unwrap();
        file.sync_all().unwrap();
        drop(file);
        // 模拟写盘完成后、原子提交前收到取消，不依赖磁盘速度或线程竞争。
        let cancel = AtomicBool::new(true);
        assert!(!pending
            .commit_if(&target, true, || !cancel.load(Ordering::Acquire))
            .unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert!(!staged.exists());
        let new_target = directory.join("new.png");
        assert!(!save_encoded_cancellable(b"new", &new_target, false, &cancel).unwrap());
        assert!(!new_target.exists());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        cancel.store(false, Ordering::Release);
        assert!(save_encoded_cancellable(b"new", &new_target, false, &cancel).unwrap());
        assert_eq!(std::fs::read(&new_target).unwrap(), b"new");
        std::fs::remove_file(target).unwrap();
        std::fs::remove_file(new_target).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn temp_file_name_is_independent_of_target_name_length() {
        let dir = std::env::temp_dir().join(format!("rustshot-tempname-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long = format!("{}.png", "长".repeat(236));
        let pending = PendingFile::new(&dir.join(&long)).unwrap();
        let name = pending
            .temp
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with(".rustshot-") && name.ends_with(".tmp"));
        assert!(name.encode_utf16().count() < 40);
        drop(pending);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn save_encoding_preserves_png_alpha_and_applies_white_background_and_quality() {
        use crate::save_options::{PngCompression, SaveEncoding, SaveFormat};
        let source = Image {
            width: 3,
            height: 1,
            bgra: vec![20, 80, 160, 255, 40, 60, 180, 128, 0, 0, 0, 0],
            readback_ms: 0.,
        };
        for compression in [
            PngCompression::Fast,
            PngCompression::Balanced,
            PngCompression::Small,
        ] {
            let bytes = encode_image(
                &source,
                SaveEncoding {
                    png_compression: compression,
                    ..Default::default()
                },
            )
            .unwrap();
            let rgba = image::load_from_memory(&bytes)
                .unwrap()
                .into_rgba8()
                .into_raw();
            assert_eq!(rgba, vec![160, 80, 20, 255, 180, 60, 40, 128, 0, 0, 0, 0]);
        }
        let bytes = encode_image(
            &source,
            SaveEncoding {
                format: SaveFormat::Bmp,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            image::load_from_memory(&bytes)
                .unwrap()
                .into_rgba8()
                .into_raw(),
            vec![160, 80, 20, 255, 217, 157, 147, 255, 255, 255, 255, 255]
        );
        let mut pixels = Vec::new();
        for y in 0..96_u32 {
            for x in 0..128_u32 {
                pixels.extend([
                    ((x * 31 + y * 13) % 256) as u8,
                    ((x * 17 + y * 23) % 256) as u8,
                    ((x * 7 + y * 47) % 256) as u8,
                    255,
                ]);
            }
        }
        let gradient = Image {
            width: 128,
            height: 96,
            bgra: pixels,
            readback_ms: 0.,
        };
        let encode = |quality| {
            encode_image(
                &gradient,
                SaveEncoding {
                    format: SaveFormat::Jpeg,
                    jpeg_quality: quality,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let low = encode(20);
        let high = encode(95);
        assert!(high.len() > low.len());
        assert_eq!(image::load_from_memory(&high).unwrap().width(), 128);
    }

    #[test]
    fn atomic_save_without_overwrite_resolves_commit_races_and_cleans_temporary_files() {
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/qa/save-unit")
            .join(format!(
                "atomic-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("same.png");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for i in 0..8_u8 {
            let path = target.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                match save_encoded(&[i; 128], &path, false) {
                    Ok(()) => Some(i),
                    Err(error) => {
                        assert_eq!(
                            error.downcast_ref::<std::io::Error>().unwrap().kind(),
                            std::io::ErrorKind::AlreadyExists
                        );
                        None
                    }
                }
            }));
        }
        let winners: Vec<_> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1);
        assert_eq!(std::fs::read(&target).unwrap(), vec![winners[0]; 128]);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert!(is_same_path(&target, &directory.join("SAME.PNG")));
        assert!(!is_same_path(&target, &directory.join("new.png")));
        save_encoded(b"confirmed replacement", &target, true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"confirmed replacement");
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn encoded_png_jpeg_bmp_roundtrip_and_failed_output_preserve_destination() {
        let directory =
            std::env::temp_dir().join(format!("rustshot-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for ext in ["png", "jpg", "bmp", "webp"] {
            let path = directory.join(format!("roundtrip.{ext}"));
            let image = Image {
                width: 16,
                height: 12,
                bgra: [20, 80, 160, 255].repeat(16 * 12),
                readback_ms: 0.,
            };
            save_image(image, &path).unwrap();
            let result = read_image(&path).unwrap();
            assert_eq!((result.width, result.height), (16, 12));
            if ext != "jpg" {
                assert_eq!(&result.bgra[..4], &[20, 80, 160, 255]);
            }
            std::fs::remove_file(path).unwrap();
        }
        let path = directory.join("preserve.png");
        std::fs::write(&path, b"existing file").unwrap();
        let malformed = Image {
            width: 16,
            height: 12,
            bgra: vec![1, 2],
            readback_ms: 0.,
        };
        assert!(save_png(malformed, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing file");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn encoded_webp_lossless_preserves_alpha_and_visible_source_colors() {
        let image = Image {
            width: 3,
            height: 2,
            bgra: vec![
                19, 41, 201, 255, 221, 99, 53, 0, 97, 183, 11, 128, 1, 254, 88, 1, 251, 1, 177,
                254, 32, 64, 128, 255,
            ],
            readback_ms: 0.,
        };
        let encoding = crate::save_options::SaveEncoding {
            format: crate::save_options::SaveFormat::Webp,
            ..Default::default()
        };
        let encoded = encode_image(&image, encoding).unwrap();
        assert_eq!(&encoded[..4], b"RIFF");
        assert_eq!(&encoded[8..12], b"WEBP");
        let decoded = crate::image_decode::decode_bytes(&encoded).unwrap();
        assert_eq!(decoded.metadata.format, image::ImageFormat::WebP);
        assert_eq!((decoded.image.width, decoded.image.height), (3, 2));
        for (actual, expected) in decoded
            .image
            .bgra
            .chunks_exact(4)
            .zip(image.bgra.chunks_exact(4))
        {
            assert_eq!(actual[3], expected[3]);
            if expected[3] != 0 {
                assert_eq!(&actual[..3], &expected[..3]);
            }
        }
        // JPEG质量不影响WebP的模式或alpha。
        assert_eq!(
            encoded,
            encode_image(
                &image,
                crate::save_options::SaveEncoding {
                    jpeg_quality: 1,
                    ..encoding
                }
            )
            .unwrap()
        );
    }

    #[test]
    fn encoded_webp_rejects_a_long_side_before_encoding() {
        let image = Image {
            width: 16_385,
            height: 1,
            bgra: vec![0; 16_385 * 4],
            readback_ms: 0.,
        };
        let encoding = crate::save_options::SaveEncoding {
            format: crate::save_options::SaveFormat::Webp,
            ..Default::default()
        };
        assert!(encode_image(&image, encoding).is_err());
    }

    #[test]
    fn encoded_webp_lossy_routes_independent_quality_and_preserves_alpha() {
        use crate::{
            image_webp_encode::{WebpEncoding, WebpMode},
            save_options::{SaveEncoding, SaveFormat},
        };
        let source = Image {
            width: 64,
            height: 48,
            bgra: (0..64 * 48)
                .flat_map(|i| {
                    [
                        (i * 31 % 256) as u8,
                        (i * 17 % 256) as u8,
                        (i * 7 % 256) as u8,
                        [0, 1, 128, 255][i % 4],
                    ]
                })
                .collect(),
            readback_ms: 0.,
        };
        let encoding = SaveEncoding {
            format: SaveFormat::Webp,
            webp: WebpEncoding {
                mode: WebpMode::Lossy,
                quality: 35,
            },
            ..Default::default()
        };
        let low = encode_image(&source, encoding).unwrap();
        assert_eq!(
            low,
            encode_image(
                &source,
                SaveEncoding {
                    jpeg_quality: 1,
                    ..encoding
                }
            )
            .unwrap()
        );
        let high = encode_image(
            &source,
            SaveEncoding {
                webp: WebpEncoding {
                    quality: 95,
                    ..encoding.webp
                },
                ..encoding
            },
        )
        .unwrap();
        assert_ne!(low, high);
        for bytes in [low, high] {
            let decoded = crate::image_decode::decode_bytes(&bytes).unwrap().image;
            assert_eq!(
                (decoded.width, decoded.height),
                (source.width, source.height)
            );
            assert_eq!(
                decoded
                    .bgra
                    .iter()
                    .skip(3)
                    .step_by(4)
                    .copied()
                    .collect::<Vec<_>>(),
                source
                    .bgra
                    .iter()
                    .skip(3)
                    .step_by(4)
                    .copied()
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn encoded_webp_cancellable_rejects_cancel_before_all_format_work() {
        use crate::save_options::{SaveEncoding, SaveFormat};
        let source = Image {
            width: 1,
            height: 1,
            bgra: vec![1, 2, 3, 255],
            readback_ms: 0.,
        };
        let cancel = std::sync::atomic::AtomicBool::new(true);
        for format in [
            SaveFormat::Png,
            SaveFormat::Jpeg,
            SaveFormat::Bmp,
            SaveFormat::Webp,
        ] {
            assert!(encode_image_cancellable(
                &source,
                SaveEncoding {
                    format,
                    ..Default::default()
                },
                &cancel
            )
            .is_err());
        }
        let lossy = SaveEncoding {
            format: SaveFormat::Webp,
            webp: crate::image_webp_encode::WebpEncoding {
                mode: crate::image_webp_encode::WebpMode::Lossy,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(encode_image_cancellable(&source, lossy, &cancel).is_err());
    }
}

#[cfg(test)]
mod alpha_clipboard_tests {
    use super::*;
    #[test]
    fn clipboard_stream_cancel_rejects_further_writes_without_retrying_forever() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut bytes = Vec::new();
        {
            let mut output = ClipboardPrepareOutput {
                output: &mut bytes,
                cancel: Some(&cancel),
                budget: None,
            };
            output.write_all(&[1, 2]).unwrap();
            cancel.store(true, std::sync::atomic::Ordering::Release);
            let error = output.write_all(&[3, 4]).unwrap_err();
            assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
            assert!(output.flush().is_err());
        }
        assert_eq!(bytes, [1, 2]);
        assert!(prepare_image_clipboard_cancellable(
            &Image {
                width: 1,
                height: 1,
                bgra: vec![0; 4],
                readback_ms: 0.,
            },
            &cancel,
        )
        .is_err());
    }

    #[test]
    fn prepared_clipboard_crosses_threads_without_publishing_or_losing_alpha() {
        let payload = std::thread::spawn(|| {
            prepare_image_clipboard(&Image {
                width: 2,
                height: 1,
                bgra: vec![20, 80, 160, 255, 40, 60, 180, 128],
                readback_ms: 0.,
            })
            .map_err(|error| error.to_string())
        })
        .join()
        .unwrap()
        .unwrap();
        let decoded = image::load_from_memory_with_format(
            &payload.payloads.png.bytes(),
            image::ImageFormat::Png,
        )
        .unwrap()
        .into_rgba8();
        assert_eq!(decoded.into_raw(), [160, 80, 20, 255, 180, 60, 40, 128]);
        // 空窗口在 OpenClipboard / EmptyClipboard 之前拒绝，不更改真实剪贴板。
        assert!(payload.publish(HWND::default()).is_err());
    }

    #[test]
    fn clipboard_png_v5_and_white_legacy_preserve_dimensions_and_alpha() {
        let image = Image {
            width: 3,
            height: 1,
            bgra: vec![20, 80, 160, 255, 40, 60, 180, 128, 0, 0, 0, 0],
            readback_ms: 0.,
        };
        let payload = ClipboardPayloads::new(&image).unwrap();
        let png = payload.png.bytes();
        let dibv5 = payload.dibv5.bytes();
        let legacy = payload.legacy.bytes();
        let rgba = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(
            rgba.into_raw(),
            vec![160, 80, 20, 255, 180, 60, 40, 128, 0, 0, 0, 0]
        );
        let field = |offset| u32::from_le_bytes(dibv5[offset..offset + 4].try_into().unwrap());
        assert_eq!(field(0), 124);
        assert_eq!(field(4), 3);
        assert_eq!(field(8), (-1_i32) as u32);
        assert_eq!(field(16), BI_BITFIELDS.0);
        assert_eq!(field(40), 0xff0000);
        assert_eq!(field(52), 0xff000000);
        assert_eq!(field(56), 0x73524742);
        assert_eq!(&dibv5[124..], &image.bgra);
        assert_eq!(&legacy[40..44], &image.bgra[..4]);
        assert_eq!(&legacy[48..52], &[255, 255, 255, 255]);
        assert!((legacy[44] as i32 - 147).abs() <= 1);
    }
    #[test]
    fn malformed_clipboard_image_fails_before_native_allocation_or_publish() {
        assert!(ClipboardPayloads::new(&Image {
            width: 2,
            height: 2,
            bgra: vec![0; 3],
            readback_ms: 0.
        })
        .is_err());
        assert!(ClipboardPayloads::new(&Image {
            width: 0,
            height: 1,
            bgra: Vec::new(),
            readback_ms: 0.
        })
        .is_err());
    }
}

#[cfg(test)]
mod clipboard_budget_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn pixel() -> Image {
        Image {
            width: 1,
            height: 1,
            bgra: vec![23, 61, 107, 80],
            readback_ms: 0.,
        }
    }

    #[test]
    fn clipboard_budget_checks_each_phase_and_actual_capacity_without_native_allocation() {
        let budget = ClipboardBudget {
            resident_bytes: 10,
            max_bytes: 100,
            png_workspace: 20,
        };
        budget.png(70).unwrap();
        assert!(budget.png(71).is_err());
        budget.transfer(40, 50).unwrap();
        assert!(budget.transfer(40, 51).is_err());
        budget.extra(90).unwrap();
        assert!(budget.extra(91).is_err());
        assert!(budget.extra(u64::MAX).is_err());
        assert_eq!(ClipboardBudget::dib_bytes(4).unwrap(), 172);
        let image = pixel();
        let resident = image.bgra.capacity() as u64;
        let checked = ClipboardBudget::new(&image, resident, u64::MAX).unwrap();
        assert_eq!(checked.max_bytes, CLIPBOARD_MAX_BYTES);
        assert_eq!(
            checked.png_workspace,
            16 + 4 * 1024 + CLIPBOARD_CODEC_RESERVE
        );
        assert!(ClipboardBudget::new(&image, resident - 1, u64::MAX).is_err());
        assert!(ClipboardBudget::new(&image, CLIPBOARD_MAX_BYTES, u64::MAX).is_err());
    }

    #[test]
    fn clipboard_budget_writer_reserves_simultaneous_old_and_new_capacity_before_growing() {
        let cancel = AtomicBool::new(false);
        let budget = ClipboardBudget {
            resident_bytes: 8,
            max_bytes: 80,
            png_workspace: 16,
        };
        let mut bytes = Vec::new();
        let mut writer = ClipboardPrepareOutput {
            output: &mut bytes,
            cancel: Some(&cancel),
            budget: Some(budget),
        };
        writer.write_all(&[7; 32]).unwrap();
        // 常驻32→33虽够，但旧32与新33共存已经超过剩余56，必须在分配前拒绝。
        let error = writer.write_all(&[9]).unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(writer.output.as_slice(), &[7; 32]);
        cancel.store(true, Ordering::Release);
        assert!(writer.write_all(&[10]).is_err());
        assert!(writer.flush().is_err());
        assert_eq!(writer.output.as_slice(), &[7; 32]);
    }

    #[test]
    fn clipboard_budget_png_finish_propagates_bounded_writer_failure_without_panicking() {
        let image = pixel();
        let resident = image.bgra.capacity() as u64;
        let mut budget = ClipboardBudget::new(&image, resident, CLIPBOARD_MAX_BYTES).unwrap();
        // PNG头可以写完，IDAT/终结阶段没有扩张余量；必须返回错误而不是codec unwrap。
        budget.max_bytes = resident + budget.png_workspace + 66;
        let mut bytes = Vec::new();
        let cancel = AtomicBool::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_png_bgra(
                &image,
                ClipboardPrepareOutput {
                    output: &mut bytes,
                    cancel: Some(&cancel),
                    budget: Some(budget),
                },
                png::Compression::NoCompression,
            )
        }));
        assert!(result.is_ok(), "bounded PNG writer errors must not panic");
        assert!(result.unwrap().is_err());
        assert!(!bytes.is_empty());
        budget.png(bytes.capacity()).unwrap();
    }

    #[test]
    fn clipboard_budget_prepare_cancel_and_budget_fail_before_native_payloads() {
        let image = pixel();
        let cancelled = AtomicBool::new(true);
        let reason = prepare_image_clipboard_with_budget_cancellable(&image, &cancelled, 0, 0)
            .err()
            .unwrap()
            .to_string();
        assert!(
            reason.contains("取消") || reason.contains("canceled"),
            "{reason}"
        );
        let cancel = AtomicBool::new(false);
        assert!(prepare_image_clipboard_with_budget_cancellable(&image, &cancel, 4, 4).is_err());
        assert!(prepare_image_clipboard_with_budget_cancellable(
            &image,
            &cancel,
            u64::MAX,
            u64::MAX,
        )
        .is_err());
    }

    #[test]
    #[ignore = "仅分配和读回私有HGLOBAL；需显式运行，不发布用户剪贴板"]
    fn clipboard_budget_native_payloads_keep_png_alpha_and_account_for_global_sizes() {
        let image = Image {
            width: 2,
            height: 1,
            bgra: vec![20, 80, 160, 255, 40, 60, 180, 128],
            readback_ms: 0.,
        };
        let resident = image.bgra.capacity() as u64;
        let max = 8 * 1024 * 1024;
        let prepared = prepare_image_clipboard_with_budget_cancellable(
            &image,
            &AtomicBool::new(false),
            resident,
            max,
        )
        .unwrap();
        let payloads = &prepared.payloads;
        let actual = payloads.png.allocated_bytes().unwrap() as u64
            + payloads.dibv5.allocated_bytes().unwrap() as u64
            + payloads.legacy.allocated_bytes().unwrap() as u64;
        assert!(resident + actual <= max);
        let decoded =
            image::load_from_memory_with_format(&payloads.png.bytes(), image::ImageFormat::Png)
                .unwrap()
                .into_rgba8();
        assert_eq!(decoded.into_raw(), [160, 80, 20, 255, 180, 60, 40, 128]);
        assert_eq!(&payloads.dibv5.bytes()[124..], &image.bgra);
    }
}

#[cfg(test)]
mod clipboard_position_tests {
    use super::*;

    fn fixture() -> Image {
        Image {
            width: 16,
            height: 12,
            bgra: [20, 80, 160, 255].repeat(16 * 12),
            readback_ms: 0.,
        }
    }

    #[test]
    fn clipboard_position_binds_signed_export_origin_to_the_exact_png() {
        let style = crate::selection_finish::SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 4.1,
            shadow_opacity: 0.5,
            border_enabled: true,
            border_thickness: 2.2,
            ..Default::default()
        };
        let pad = style.padding();
        let origin = crate::capture_pin_output::finished_origin(
            crate::geometry::ScreenRect {
                left: -1920,
                top: -240,
                width: 1920,
                height: 1080,
            },
            crate::geometry::Rect {
                x: 8,
                y: 6,
                width: 16,
                height: 12,
            },
            style,
        );
        let image = style.compose(fixture()).unwrap();
        let exact = ClipboardPlacement::Exact { origin, scale: 1. };
        let payload = ClipboardPayloads::at(&image, Some(exact)).unwrap();
        let png = payload.png.bytes();
        let placement = payload.placement.unwrap().bytes();
        assert_eq!(decode_placement(&placement, &png), Some((exact, png.len())));
        let mut padded = placement.clone();
        padded.extend_from_slice(&[0; 8]);
        assert_eq!(decode_placement(&padded, &png), Some((exact, png.len())));
        let metadata = encode_screen_position(origin, &png);
        assert_eq!(origin.0 + pad.left as i32, -1912);
        assert_eq!(origin.1 + pad.top as i32, -234);
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.dimensions(), (image.width, image.height));
        assert!(decoded.pixels().any(|pixel| pixel.0[3] < 255));

        // Windows 的全局内存分配器可能在实际 PNG 后面补齐尾部空间。
        let mut aligned = png.clone();
        aligned.extend_from_slice(&[0; 8]);
        assert_eq!(
            decode_screen_position(&metadata, &aligned),
            Some((origin, png.len()))
        );
        for signed_origin in [(i32::MIN, i32::MAX), (-3840, -1080), (0, 0)] {
            let metadata = encode_screen_position(signed_origin, &png);
            assert_eq!(
                decode_screen_position(&metadata, &png),
                Some((signed_origin, png.len()))
            );
        }
    }

    #[test]
    fn clipboard_position_rejects_stale_unknown_and_truncated_metadata() {
        let first = ClipboardPayloads::new(&fixture()).unwrap().png.bytes();
        let metadata = encode_screen_position((-1920, -240), &first);
        let mut changed = fixture();
        changed.bgra[3] = 0;
        let second = ClipboardPayloads::new(&changed).unwrap().png.bytes();
        assert!(decode_screen_position(&metadata, &second).is_none());
        assert!(decode_screen_position(&metadata, &first[..first.len() - 1]).is_none());
        assert!(decode_screen_position(&metadata, &[]).is_none());
        for length in [0, 8, 24, SCREEN_POSITION_BYTES - 1] {
            assert!(decode_screen_position(&metadata[..length], &first).is_none());
        }
        for offset in [0, 7, 24, SCREEN_POSITION_BYTES - 1] {
            let mut invalid = metadata;
            invalid[offset] ^= 1;
            assert!(decode_screen_position(&invalid, &first).is_none());
        }
        let mut invalid = metadata;
        invalid[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_screen_position(&invalid, &first).is_none());
        let mut empty = metadata;
        empty[16..24].fill(0);
        assert!(decode_screen_position(&empty, &first).is_none());
    }

    #[test]
    fn clipboard_image_without_capture_position_has_no_position_payload() {
        assert!(ClipboardPayloads::new(&fixture())
            .unwrap()
            .placement
            .is_none());
    }

    #[test]
    fn clipboard_placement_roundtrips_scroll_and_scaled_pin_layouts() {
        let image = fixture();
        for placement in [
            ClipboardPlacement::Scroll {
                axis: crate::stitch::ScrollAxis::Vertical,
                region: crate::geometry::ScreenRect {
                    left: -1600,
                    top: 120,
                    width: 16,
                    height: 400,
                },
            },
            ClipboardPlacement::Scroll {
                axis: crate::stitch::ScrollAxis::Horizontal,
                region: crate::geometry::ScreenRect {
                    left: 40,
                    top: -300,
                    width: 900,
                    height: 12,
                },
            },
            ClipboardPlacement::Exact {
                origin: (-20, 30),
                scale: 0.5,
            },
        ] {
            let payload = ClipboardPayloads::at(&image, Some(placement)).unwrap();
            let png = payload.png.bytes();
            let bytes = payload.placement.unwrap().bytes();
            assert_eq!(decode_placement(&bytes, &png), Some((placement, png.len())));
            // 字段名是公开约定，第三方可按 JSON 读取。
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["version"], 2);
            assert_eq!(json["width"], 16);
            assert_eq!(json["height"], 12);
            assert_eq!(json["png_bytes"], png.len());
            assert!(json["layout"] == "scroll" || json["layout"] == "exact");
        }
    }

    #[test]
    fn clipboard_placement_rejects_stale_invalid_and_future_records() {
        let image = fixture();
        let first = ClipboardPayloads::new(&image).unwrap().png.bytes();
        let size = (image.width, image.height);
        let exact = ClipboardPlacement::Exact {
            origin: (0, 0),
            scale: 1.,
        };
        let bytes = encode_placement(exact, &first, size).unwrap();
        let mut changed = fixture();
        changed.bgra[3] = 0;
        let second = ClipboardPayloads::new(&changed).unwrap().png.bytes();
        assert!(decode_placement(&bytes, &second).is_none());
        assert!(decode_placement(&bytes, &first[..first.len() - 1]).is_none());
        assert!(decode_placement(&bytes, &[]).is_none());
        assert!(decode_placement(&bytes[..bytes.len() - 1], &first).is_none());
        // 宽高与 PNG 头不一致、未来版本、倍率越界都整条忽略。
        let wrong_size = encode_placement(exact, &first, (12, 16)).unwrap();
        assert!(decode_placement(&wrong_size, &first).is_none());
        let future = String::from_utf8(bytes.clone())
            .unwrap()
            .replace("\"version\":2", "\"version\":3");
        assert!(decode_placement(future.as_bytes(), &first).is_none());
        for scale in [0., 1.5, f32::NAN] {
            assert!(encode_placement(
                ClipboardPlacement::Exact {
                    origin: (0, 0),
                    scale
                },
                &first,
                size
            )
            .is_err());
        }
        let zoomed = String::from_utf8(bytes)
            .unwrap()
            .replace("\"scale\":1.0", "\"scale\":2.0");
        assert!(zoomed.contains("\"scale\":2.0"));
        assert!(decode_placement(zoomed.as_bytes(), &first).is_none());
    }
}

#[cfg(test)]
mod long_output_memory_tests {
    use super::*;

    fn fixture(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            bgra: (0..width * height)
                .flat_map(|i| {
                    [
                        i.wrapping_mul(17) as u8,
                        (i / width).wrapping_mul(29) as u8,
                        i.wrapping_mul(7).wrapping_add(3) as u8,
                        i.wrapping_mul(53) as u8,
                    ]
                })
                .collect(),
            readback_ms: 0.,
        }
    }

    fn borrowed_png_directory(name: &str) -> PathBuf {
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/qa/long-history-unit")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn long_path_atomic_png_and_encoded_saves_preserve_replace_and_create_only_contracts() {
        use std::os::windows::ffi::OsStrExt;
        struct TestDir {
            path: PathBuf,
            parents: Vec<PathBuf>,
        }
        impl Drop for TestDir {
            fn drop(&mut self) {
                // 只删除本模型自有的平铺文件和空目录，不递归删除或跟随链接。
                if let Ok(entries) = std::fs::read_dir(&self.path) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
                let _ = std::fs::remove_dir(self.path.join("blocked.png"));
                let _ = std::fs::remove_dir(&self.path);
                for parent in self.parents.iter().rev() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
        }
        let mut directory = borrowed_png_directory("atomic-long-path");
        let mut parents = Vec::new();
        while directory.as_os_str().encode_wide().count() <= 280 {
            parents.push(directory.clone());
            directory = directory.join("long-export-segment-".repeat(4));
        }
        std::fs::create_dir_all(&directory).unwrap();
        let dir = TestDir {
            path: directory,
            parents,
        };
        let path = dir.path.join("current.png");
        assert!(path.as_os_str().encode_wide().count() > 260);
        assert!(
            !path.to_string_lossy().starts_with(r"\\?\"),
            "真实入口接受普通深路径，不由用例预加前缀"
        );
        let first = [11, 22, 33, 255];
        let second = [44, 55, 66, 127];
        save_png_pixels(1, 1, &first, &path).unwrap();
        assert_eq!(read_image(&path).unwrap().bgra, first);
        save_png_pixels(1, 1, &second, &path).unwrap();
        assert_eq!(read_image(&path).unwrap().bgra, second);
        let encoded = dir.path.join("create-only.dat");
        save_encoded(b"first", &encoded, false).unwrap();
        assert!(save_encoded(b"must not replace", &encoded, false).is_err());
        assert_eq!(std::fs::read(&encoded).unwrap(), b"first");
        save_encoded(b"replace", &encoded, true).unwrap();
        assert_eq!(std::fs::read(&encoded).unwrap(), b"replace");
        let current = std::env::current_dir().unwrap();
        save_encoded(b"relative", encoded.strip_prefix(&current).unwrap(), true).unwrap();
        assert_eq!(std::fs::read(&encoded).unwrap(), b"relative");
        let blocked = dir.path.join("blocked.png");
        std::fs::create_dir(&blocked).unwrap();
        assert!(save_png_pixels(1, 1, &first, &blocked).is_err());
        assert!(blocked.is_dir());
        assert_eq!(read_image(&path).unwrap().bgra, second);
        assert_eq!(
            std::fs::read_dir(&dir.path).unwrap().count(),
            3,
            "成功或失败提交均清理自己的临时文件"
        );
    }

    #[test]
    fn borrowed_png_pixels_roundtrip_preserves_all_rows_alpha_and_input() {
        let source = fixture(7, 32_001);
        let allocation = source.bgra.as_ptr();
        let capacity = source.bgra.capacity();
        let directory = borrowed_png_directory("roundtrip");
        let path = directory.join("history.png");
        save_png_pixels(source.width, source.height, &source.bgra, &path).unwrap();
        let decoded = image::open(&path).unwrap().into_rgba8();
        assert_eq!(decoded.dimensions(), (7, 32_001));
        for (i, (rgba, bgra)) in decoded
            .as_raw()
            .chunks_exact(4)
            .zip(source.bgra.chunks_exact(4))
            .enumerate()
        {
            let i = i as u32;
            let expected = [
                i.wrapping_mul(17) as u8,
                (i / source.width).wrapping_mul(29) as u8,
                i.wrapping_mul(7).wrapping_add(3) as u8,
                i.wrapping_mul(53) as u8,
            ];
            assert_eq!(bgra, expected);
            assert_eq!(rgba, [expected[2], expected[1], expected[0], expected[3]]);
        }
        assert_eq!(source.bgra.as_ptr(), allocation);
        assert_eq!(source.bgra.capacity(), capacity);
        assert_eq!(source.bgra.len(), 7 * 32_001 * 4);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn borrowed_png_pixels_failures_preserve_destination_input_and_remove_temps() {
        let directory = borrowed_png_directory("failure");
        let path = directory.join("preserve.png");
        std::fs::write(&path, b"existing file").unwrap();
        let pixels = [11, 22, 33, 44];
        for (width, height, bytes) in [
            (0, 1, pixels.as_slice()),
            (1, 0, pixels.as_slice()),
            (1, 2, pixels.as_slice()),
            (1, 1, &pixels[..3]),
            (u32::MAX, u32::MAX, pixels.as_slice()),
        ] {
            assert!(save_png_pixels(width, height, bytes, &path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"existing file");
            assert_eq!(pixels, [11, 22, 33, 44]);
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        }
        // 编码成功但提交失败时也清理临时文件，不把历史写入错误变成目标破坏。
        let blocked = directory.join("blocked.png");
        std::fs::create_dir(&blocked).unwrap();
        assert!(save_png_pixels(1, 1, &pixels, &blocked).is_err());
        assert!(blocked.is_dir());
        assert_eq!(pixels, [11, 22, 33, 44]);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing file");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        std::fs::remove_dir(blocked).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn streamed_long_png_decodes_all_rows_alpha_and_keeps_input_bgra() {
        let image = fixture(7, 32_001);
        let original = image.bgra.clone();
        for compression in [
            png::Compression::Fast,
            png::Compression::Balanced,
            png::Compression::High,
        ] {
            let mut encoded = Vec::new();
            write_png_bgra(&image, &mut encoded, compression).unwrap();
            let decoded = image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)
                .unwrap()
                .into_rgba8();
            assert_eq!(decoded.dimensions(), (7, 32_001));
            for (actual, source) in decoded
                .as_raw()
                .chunks_exact(4)
                .zip(original.chunks_exact(4))
            {
                assert_eq!(actual, &[source[2], source[1], source[0], source[3]]);
            }
            assert_eq!(image.bgra, original);
        }
    }

    #[test]
    fn native_payloads_keep_top_down_rows_straight_alpha_and_exact_white_blend() {
        // 仅分配和读回私有 HGLOBAL，不打开或发布用户剪贴板。
        let image = fixture(5, 19);
        let original = image.bgra.clone();
        let payload = ClipboardPayloads::new(&image).unwrap();
        let dib = payload.dibv5.bytes();
        let legacy = payload.legacy.bytes();
        assert_eq!(dib.len(), 124 + original.len());
        assert_eq!(legacy.len(), 40 + original.len());
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), -19);
        assert_eq!(i32::from_le_bytes(legacy[8..12].try_into().unwrap()), -19);
        assert_eq!(&dib[124..], &original);
        for (actual, source) in legacy[40..].chunks_exact(4).zip(original.chunks_exact(4)) {
            let alpha = u32::from(source[3]);
            for c in 0..3 {
                assert_eq!(
                    actual[c],
                    ((u32::from(source[c]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
                );
            }
            assert_eq!(actual[3], 255);
        }
        assert_eq!(image.bgra, original);
        // 常驻输出仅两份 DIB 和一份 PNG；没有 white/packed 全帧暂存。
        let native_bytes = payload.png.length + payload.dibv5.length + payload.legacy.length;
        assert_eq!(native_bytes, payload.png.length + 2 * original.len() + 164);
    }

    #[test]
    fn long_clipboard_stage_model_accounts_for_compressed_capacity_and_row_scratch() {
        // 以真实编码容量和私有 HGLOBAL 请求尺寸推算，不把模型当成实测 RSS。
        let image = fixture(1000, 31);
        let source = image.bgra.len();
        let mut encoded = Vec::new();
        write_png_bgra(&image, &mut encoded, png::Compression::Fast).unwrap();
        let payload = ClipboardPayloads::new(&image).unwrap();
        assert_eq!(payload.png.length, encoded.len());
        let row = image.width as usize * 4;
        let new_png_transfer = source + encoded.capacity() + payload.png.length;
        let new_final_outputs =
            source + payload.png.length + payload.dibv5.length + payload.legacy.length;
        let new_peak = new_png_transfer.max(new_final_outputs) + row;
        let old_publish_peak = 5 * source + encoded.capacity() + payload.png.length + 164;
        assert!(old_publish_peak - new_peak > source);
        assert!(row < source / 30);
        // 同宽32K图的转换行仍相同；完整输入是128MB，不另分配这份合成fixture。
        assert_eq!(32_000_usize * row, 128_000_000);
    }
}
