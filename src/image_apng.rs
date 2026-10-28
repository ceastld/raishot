//! 独立的有界APNG来源；首批只合成非交错、8位RGB/RGBA，不改变静态PNG或GIF。
use crate::{
    capture::Image,
    image_color_management::FrameColorManager,
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
    image_export_metadata::SourceMetadata,
};
use png::{BitDepth, BlendOp, ColorType, DisposeOp, FrameControl};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 8 * 1024 * 1024;
// 元数据原始块及其副本/扩容、PNG行与解压暂存、8MiB缩略缓存的保守预留。
// png::Limits另有累计行扣账，不能把该逻辑额度当成实际峰值内存。
const FIXED_RESERVE: u64 = 32 * 1024 * 1024;
const PRESENTATION_BYTES_PER_PIXEL: u64 = 20;
const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Copy, Debug)]
pub(crate) struct ApngOptions {
    pub max_canvas_pixels: u64,
    /// 来源的像素/解码工作集和呈现预留，不包含调用方已有输入字节或GPU缓存。
    pub max_total_bytes: u64,
    pub max_input_bytes: u64,
    pub max_output_frames: usize,
    pub zero_delay: Duration,
}

impl Default for ApngOptions {
    fn default() -> Self {
        Self {
            max_canvas_pixels: 5_000_000,
            max_total_bytes: MAX_BYTES,
            max_input_bytes: MAX_BYTES,
            max_output_frames: 4,
            zero_delay: Duration::from_millis(100),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ApngError {
    Unsupported(String),
    Invalid(String),
    Io(io::Error),
    Decode(png::DecodingError),
    Backpressure,
    Cancelled,
    RequiresRestart,
}

impl fmt::Display for ApngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message) | Self::Invalid(message) => f.write_str(message),
            Self::Io(error) => fmt::Display::fmt(error, f),
            Self::Decode(error) => fmt::Display::fmt(error, f),
            Self::Backpressure => {
                f.write_str(tr!("等待APNG交付帧释放", "Waiting for APNG frame slots"))
            }
            Self::Cancelled => f.write_str(tr!("APNG读取已取消", "APNG reading canceled")),
            Self::RequiresRestart => {
                f.write_str(tr!("APNG来源需要重启", "APNG source needs a restart"))
            }
        }
    }
}
impl std::error::Error for ApngError {}
impl From<io::Error> for ApngError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<png::DecodingError> for ApngError {
    fn from(error: png::DecodingError) -> Self {
        match error {
            png::DecodingError::IoError(error) if error.kind() == io::ErrorKind::Unsupported => {
                Self::Unsupported(error.to_string())
            }
            error => Self::Decode(error),
        }
    }
}
type ApngResult<T> = Result<T, ApngError>;

fn invalid(zh: &'static str, en: &'static str) -> ApngError {
    ApngError::Invalid(crate::i18n::tr(zh, en).into())
}

fn cancelled(cancel: &AtomicBool) -> ApngResult<()> {
    if cancel.load(Ordering::Acquire) {
        Err(ApngError::Cancelled)
    } else {
        Ok(())
    }
}

fn classify(error: ApngError, cancel: &AtomicBool) -> ApngError {
    if cancel.load(Ordering::Acquire) {
        ApngError::Cancelled
    } else {
        error
    }
}

fn read_error(zh: &'static str, en: &'static str) -> io::Error {
    io::Error::other(crate::i18n::tr(zh, en))
}

fn exif_restriction() -> &'static str {
    tr!(
        "APNG动画暂不处理EXIF元数据，请使用静态预览",
        "Use a static preview for APNG with EXIF metadata"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApngTiming {
    /// 文件中的原始分数，分母0保留原值；delay按分母100和零延时策略换算。
    pub numerator: u16,
    pub denominator: u16,
    pub delay: Duration,
}

impl ApngTiming {
    fn from_control(control: FrameControl, zero_delay: Duration) -> Self {
        let denominator = if control.delay_den == 0 {
            100
        } else {
            control.delay_den
        };
        let delay = if control.delay_num == 0 {
            zero_delay
        } else {
            Duration::from_nanos(
                u64::from(control.delay_num) * 1_000_000_000 / u64::from(denominator),
            )
        };
        Self {
            numerator: control.delay_num,
            denominator: control.delay_den,
            delay,
        }
    }
}

struct OutputPool {
    live: AtomicUsize,
    limit: usize,
}
struct OutputLease(Arc<OutputPool>);
impl OutputPool {
    fn acquire(self: &Arc<Self>) -> ApngResult<OutputLease> {
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.limit).then_some(live + 1)
            })
            .map_err(|_| ApngError::Backpressure)?;
        Ok(OutputLease(self.clone()))
    }
}
impl Drop for OutputLease {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::Release);
    }
}

/// 像素与租约不能拆开；外部持有帧或其Arc时持续占用同一来源的交付名额。
pub(crate) struct ApngFrame {
    image: Image,
    metadata_source: Arc<SourceMetadata>,
    warning: Option<String>,
    _lease: OutputLease,
    pub generation: u64,
    pub cycle: u64,
    pub index: u64,
    pub timing: ApngTiming,
}
impl ApngFrame {
    pub(crate) fn image(&self) -> &Image {
        &self.image
    }
    pub(crate) fn retained_bytes(&self) -> u64 {
        (self.image.bgra.capacity() as u64)
            .saturating_add(self.metadata_source.retained_bytes() as u64)
            .saturating_add(
                self.warning
                    .as_ref()
                    .map_or(0, |warning| warning.capacity()) as u64,
            )
    }
    pub(crate) fn metadata_source(&self) -> &Arc<SourceMetadata> {
        &self.metadata_source
    }
    pub(crate) fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IccIdentity {
    offset: u64,
    length: u64,
    crc: u32,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IccLayout {
    first: Option<IccIdentity>,
    count: u32,
    digest: [u8; 32],
}
impl IccLayout {
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            first: None,
            count: 0,
            digest: Sha256::digest(b"").into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Scan {
    width: u32,
    height: u32,
    color: ColorType,
    frames: u32,
    plays: u32,
    poster: bool,
    metadata_bytes: u64,
    icc: FieldSummary,
    icc_layout: IccLayout,
}

impl Scan {
    fn source_summary(self) -> Summary {
        Summary {
            icc: self.icc,
            exif: absent_metadata(),
            color_model: ColorModel::Rgb,
            bits_per_channel: Some(8),
            orientation: None,
        }
    }
}

fn absent_metadata() -> FieldSummary {
    FieldSummary {
        status: MetadataStatus::Absent,
        encoded_bytes: 0,
        decoded_bytes: None,
    }
}

fn options_valid(options: ApngOptions) -> ApngResult<()> {
    if options.max_canvas_pixels == 0
        || options.max_canvas_pixels > 32_000_000
        || options.max_total_bytes == 0
        || options.max_total_bytes > MAX_BYTES
        || options.max_input_bytes == 0
        || options.max_input_bytes > MAX_BYTES
        || !(1..=4).contains(&options.max_output_frames)
        || options.zero_delay.is_zero()
    {
        return Err(invalid(
            "APNG预算或零延时策略无效",
            "Invalid APNG budget or zero-delay policy",
        ));
    }
    Ok(())
}

fn canvas_budget(width: u32, height: u32, options: ApngOptions) -> ApngResult<u64> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > 65_535
        || height > 65_535
        || pixels > options.max_canvas_pixels
    {
        return Err(invalid(
            "APNG画布超过尺寸预算",
            "APNG canvas exceeds the dimension budget",
        ));
    }
    // 画布、完整解码块、最坏Previous区域和交付帧：四个名额时合计28P字节。
    // 超长边会分块：整幅旋转/复制、分块原图和透明预览可短时同时存在。
    let presentation = PRESENTATION_BYTES_PER_PIXEL
        + if width > 16_384 || height > 16_384 {
            4
        } else {
            0
        };
    let bytes =
        pixels * (4 * (3 + options.max_output_frames as u64) + presentation) + FIXED_RESERVE;
    if bytes > options.max_total_bytes {
        return Err(invalid(
            "APNG缓冲超过总预算",
            "APNG buffers exceed the total budget",
        ));
    }
    Ok(bytes)
}

fn color_budget(scan: Scan, options: ApngOptions, retained_bytes: usize) -> ApngResult<u64> {
    let rows = FrameColorManager::row_buffer_bytes(scan.width).map_err(ApngError::Invalid)?;
    // 配置/失败sidecar的可见保留与两条BGR行另计；不声称限制系统CMM内部峰值。
    canvas_budget(scan.width, scan.height, options)?
        .checked_add(retained_bytes as u64)
        .and_then(|bytes| bytes.checked_add(rows as u64))
        .filter(|bytes| *bytes <= options.max_total_bytes)
        .ok_or_else(|| {
            invalid(
                "APNG颜色缓冲超过总预算",
                "APNG color buffers exceed the total budget",
            )
        })
}

fn check_capacity(capacity: usize, slot_bytes: usize) -> ApngResult<()> {
    if capacity > slot_bytes {
        return Err(invalid(
            "APNG缓冲超过总预算",
            "APNG buffers exceed the total budget",
        ));
    }
    Ok(())
}

fn profile_constructor_bytes(profile: Option<&Vec<u8>>) -> ApngResult<usize> {
    let Some(profile) = profile else {
        return Ok(0);
    };
    // 初次Vec转Arc保留原capacity；失败sidecar重建另按原Arc、复制Vec与新Arc预留。
    profile
        .capacity()
        .checked_add(profile.len())
        .zip(profile.len().checked_mul(3))
        .map(|(input, replacement)| input.max(replacement))
        .ok_or_else(|| {
            invalid(
                "APNG颜色缓冲超过总预算",
                "APNG color buffers exceed the total budget",
            )
        })
}

fn allocate(length: usize, cancel: &AtomicBool) -> ApngResult<Vec<u8>> {
    cancelled(cancel)?;
    let mut bytes = Vec::new();
    let reserved = bytes.try_reserve_exact(length);
    cancelled(cancel)?;
    reserved.map_err(|_| invalid("APNG缓冲分配失败", "APNG buffer allocation failed"))?;
    check_capacity(bytes.capacity(), length)?;
    bytes.resize(length, 0);
    cancelled(cancel)?;
    Ok(bytes)
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn add_metadata(total: &mut u64, kind: &[u8; 4], length: u64) -> ApngResult<()> {
    if kind != b"IDAT" && kind != b"fdAT" {
        *total = total
            .checked_add(length + 12)
            .ok_or_else(|| invalid("APNG元数据长度溢出", "APNG metadata length overflow"))?;
        if *total > MAX_METADATA_BYTES {
            return Err(invalid(
                "APNG元数据超过8MiB上限",
                "APNG metadata exceeds the 8 MiB limit",
            ));
        }
    }
    Ok(())
}

fn read_checked<R: Read>(reader: &mut R, output: &mut [u8], cancel: &AtomicBool) -> ApngResult<()> {
    let mut filled = 0;
    while filled < output.len() {
        cancelled(cancel)?;
        let end = (filled + 64 * 1024).min(output.len());
        match reader.read(&mut output[filled..end]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    cancelled(cancel)
}

#[cfg(test)]
fn scan_source<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    length: u64,
    options: ApngOptions,
    cancel: &AtomicBool,
) -> ApngResult<Scan> {
    scan_source_with_profile(reader, start, length, options, cancel, None).map(|(scan, _)| scan)
}

fn scan_source_with_profile<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    length: u64,
    options: ApngOptions,
    cancel: &AtomicBool,
    frozen: Option<Scan>,
) -> ApngResult<(Scan, Option<Vec<u8>>)> {
    cancelled(cancel)?;
    if length < SIGNATURE.len() as u64 {
        return Err(invalid("APNG文件头被截断", "Truncated APNG signature"));
    }
    reader.seek(SeekFrom::Start(start))?;
    let mut signature = [0; 8];
    read_checked(reader, &mut signature, cancel)?;
    if signature != SIGNATURE {
        return Err(invalid("APNG文件头无效", "Invalid APNG signature"));
    }
    let mut offset = 8u64;
    let mut dimensions = None;
    let mut animation = None;
    let mut frame_count = 0u32;
    let mut has_idat = false;
    let mut poster = false;
    let mut metadata_bytes = 0;
    let mut icc = absent_metadata();
    let mut profile = None;
    let mut first_icc = None;
    let mut icc_count = 0u32;
    let mut icc_digest = Sha256::new();
    loop {
        cancelled(cancel)?;
        if offset.checked_add(12).is_none_or(|end| end > length) {
            return Err(invalid("APNG块被截断", "Truncated APNG chunk"));
        }
        reader.seek(SeekFrom::Start(start + offset))?;
        let mut header = [0; 8];
        read_checked(reader, &mut header, cancel)?;
        let size = u64::from(be32(&header));
        let kind: [u8; 4] = header[4..8].try_into().unwrap();
        let end = offset
            .checked_add(size + 12)
            .filter(|end| *end <= length)
            .ok_or_else(|| {
                invalid(
                    "APNG块超出固定来源范围",
                    "APNG chunk exceeds the fixed source range",
                )
            })?;
        if size > 0x7fff_ffff {
            return Err(invalid("APNG块长度无效", "Invalid APNG chunk length"));
        }
        add_metadata(&mut metadata_bytes, &kind, size)?;
        if dimensions.is_none() && (offset != 8 || kind != *b"IHDR") {
            return Err(invalid(
                "APNG缺少首个IHDR",
                "APNG is missing the first IHDR",
            ));
        }
        match &kind {
            b"IHDR" => {
                if size != 13 || dimensions.is_some() {
                    return Err(invalid("APNG的IHDR无效", "Invalid APNG IHDR"));
                }
                let mut data = [0; 13];
                read_checked(reader, &mut data, cancel)?;
                let width = be32(&data);
                let height = be32(&data[4..]);
                canvas_budget(width, height, options)?;
                if data[8] != 8 || !matches!(data[9], 2 | 6) || data[12] != 0 {
                    return Err(ApngError::Unsupported(
                        tr!(
                            "APNG动画仅支持非交错8位RGB/RGBA",
                            "APNG animation supports non-interlaced 8-bit RGB/RGBA only"
                        )
                        .into(),
                    ));
                }
                if data[10] != 0 || data[11] != 0 {
                    return Err(invalid(
                        "APNG压缩或过滤方法无效",
                        "Invalid APNG compression or filter method",
                    ));
                }
                dimensions = Some((
                    width,
                    height,
                    if data[9] == 2 {
                        ColorType::Rgb
                    } else {
                        ColorType::Rgba
                    },
                ));
            }
            b"acTL" => {
                if size != 8 || animation.is_some() || has_idat {
                    return Err(invalid(
                        "APNG动画控制块无效",
                        "Invalid APNG animation control",
                    ));
                }
                let mut data = [0; 8];
                read_checked(reader, &mut data, cancel)?;
                let frames = be32(&data);
                let plays = be32(&data[4..]);
                if frames == 0 || frames > 0x7fff_ffff || plays > 0x7fff_ffff {
                    return Err(invalid(
                        "APNG帧数或循环次数无效",
                        "Invalid APNG frame or play count",
                    ));
                }
                animation = Some((frames, plays));
            }
            b"fcTL" => {
                if size != 26 || animation.is_none() {
                    return Err(invalid("APNG帧控制块无效", "Invalid APNG frame control"));
                }
                frame_count = frame_count
                    .checked_add(1)
                    .ok_or_else(|| invalid("APNG帧数溢出", "APNG frame count overflow"))?;
                if frame_count > animation.unwrap().0 {
                    return Err(invalid(
                        "APNG帧数与声明不一致",
                        "APNG frame count does not match its declaration",
                    ));
                }
                let mut data = [0; 26];
                read_checked(reader, &mut data, cancel)?;
                let (width, height, _) = dimensions.unwrap();
                let fw = be32(&data[4..]);
                let fh = be32(&data[8..]);
                let x = be32(&data[12..]);
                let y = be32(&data[16..]);
                if fw == 0
                    || fh == 0
                    || Some(fw) > width.checked_sub(x)
                    || Some(fh) > height.checked_sub(y)
                    || data[24] > 2
                    || data[25] > 1
                    || (!has_idat && (fw != width || fh != height || x != 0 || y != 0))
                {
                    return Err(invalid(
                        "APNG帧区域或合成方法无效",
                        "Invalid APNG frame rectangle or composition",
                    ));
                }
            }
            b"eXIf" if size != 0 => {
                return Err(ApngError::Unsupported(exif_restriction().into()));
            }
            b"iCCP" => {
                icc_count = icc_count.checked_add(1).ok_or_else(|| {
                    invalid("APNG颜色块数量溢出", "APNG color chunk count overflow")
                })?;
                icc.encoded_bytes += size;
                let mut digest = Sha256::new();
                // 只有首次预扫的首个前置块需要解压；重播只流式核对冻结的身份。
                let payload = if frozen.is_none() && icc_count == 1 && !has_idat {
                    let mut payload = allocate(size as usize, cancel)?;
                    read_checked(reader, &mut payload, cancel)?;
                    for part in payload.chunks(64 * 1024) {
                        cancelled(cancel)?;
                        digest.update(part);
                    }
                    Some(payload)
                } else {
                    let mut remaining = size;
                    let mut scratch = [0; 8192];
                    while remaining > 0 {
                        let count = remaining.min(scratch.len() as u64) as usize;
                        read_checked(reader, &mut scratch[..count], cancel)?;
                        digest.update(&scratch[..count]);
                        remaining -= count as u64;
                    }
                    None
                };
                let mut crc = [0; 4];
                read_checked(reader, &mut crc, cancel)?;
                digest.update(crc);
                let identity = IccIdentity {
                    offset,
                    length: size,
                    crc: be32(&crc),
                    digest: digest.finalize().into(),
                };
                if first_icc.is_none() {
                    first_icc = Some(identity);
                }
                icc_digest.update(offset.to_be_bytes());
                icc_digest.update(size.to_be_bytes());
                icc_digest.update(identity.digest);
                if frozen.is_none() {
                    if let Some(payload) = payload {
                        let (field, extracted) =
                            crate::image_container_metadata::inspect_png_icc_chunk(
                                &payload,
                                identity.crc,
                                cancel,
                            )
                            .map_err(ApngError::Invalid)?;
                        icc = field;
                        profile = extracted;
                    } else {
                        icc.status = MetadataStatus::Malformed;
                        icc.decoded_bytes = None;
                        profile = None;
                    }
                }
            }
            b"IDAT" => {
                if !has_idat {
                    poster = frame_count == 0;
                    has_idat = true;
                }
            }
            b"fdAT" if !has_idat || size < 4 => {
                return Err(invalid("APNG帧数据无效", "Invalid APNG frame data"))
            }
            b"IEND" => {
                if size != 0 || !has_idat || end != length {
                    return Err(invalid(
                        "APNG结束块或来源长度无效",
                        "Invalid APNG end chunk or source length",
                    ));
                }
                let (frames, plays) = animation.ok_or_else(|| {
                    ApngError::Unsupported(
                        tr!("PNG不包含APNG动画", "PNG contains no APNG animation").into(),
                    )
                })?;
                if frame_count != frames {
                    return Err(invalid(
                        "APNG帧数与声明不一致",
                        "APNG frame count does not match its declaration",
                    ));
                }
                let (width, height, color) = dimensions.unwrap();
                cancelled(cancel)?;
                let icc_layout = IccLayout {
                    first: first_icc,
                    count: icc_count,
                    digest: icc_digest.finalize().into(),
                };
                if let Some(frozen) = frozen {
                    if icc_layout != frozen.icc_layout {
                        return Err(invalid("APNG颜色配置已改变", "APNG color profile changed"));
                    }
                    icc = frozen.icc;
                }
                return Ok((
                    Scan {
                        width,
                        height,
                        color,
                        frames,
                        plays,
                        poster,
                        metadata_bytes,
                        icc,
                        icc_layout,
                    },
                    profile,
                ));
            }
            _ => {}
        }
        offset = end;
    }
}

/// 解码时再验块长度和元数据累计，防止同长文件在预扫后改变块头绕过预算。
struct GuardedRead<R> {
    reader: Arc<Mutex<R>>,
    start: u64,
    length: u64,
    physical: u64,
    pending: [u8; 8],
    pending_at: usize,
    pending_len: usize,
    body: u64,
    metadata: u64,
    frames: u32,
    expected_frames: u32,
    expected_icc: IccLayout,
    icc_count: u32,
    icc_digest: Sha256,
    icc_body: Option<(u64, u64, Sha256)>,
    started: bool,
    ended: bool,
    cancel: Arc<AtomicBool>,
}

impl<R: Read + Seek> GuardedRead<R> {
    fn new(
        reader: Arc<Mutex<R>>,
        start: u64,
        length: u64,
        frames: u32,
        expected_icc: IccLayout,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        reader
            .lock()
            .map_err(|_| read_error("APNG来源锁不可用", "APNG source lock unavailable"))?
            .seek(SeekFrom::Start(start))?;
        Ok(Self {
            reader,
            start,
            length,
            physical: 0,
            pending: [0; 8],
            pending_at: 0,
            pending_len: 0,
            body: 0,
            metadata: 0,
            frames: 0,
            expected_frames: frames,
            expected_icc,
            icc_count: 0,
            icc_digest: Sha256::new(),
            icc_body: None,
            started: false,
            ended: false,
            cancel,
        })
    }
    fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            // read_exact会重试Interrupted；取消必须使用不可重试的错误。
            Err(io::Error::other(tr!(
                "APNG读取已取消",
                "APNG reading canceled"
            )))
        } else {
            Ok(())
        }
    }
    fn raw_read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.check_cancel()?;
        let count = output
            .len()
            .min(64 * 1024)
            .min((self.length - self.physical) as usize);
        let read = self
            .reader
            .lock()
            .map_err(|_| read_error("APNG来源锁不可用", "APNG source lock unavailable"))?
            .read(&mut output[..count])?;
        self.physical += read as u64;
        self.check_cancel()?;
        Ok(read)
    }
    fn header(&mut self) -> io::Result<()> {
        let mut header = [0; 8];
        let mut filled = 0;
        while filled < 8 {
            let count = self.raw_read(&mut header[filled..])?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            filled += count;
        }
        if !self.started {
            if header != SIGNATURE {
                return Err(read_error("APNG文件头无效", "Invalid APNG signature"));
            }
            self.started = true;
        } else {
            let length = u64::from(be32(&header));
            let kind: [u8; 4] = header[4..8].try_into().unwrap();
            if length > 0x7fff_ffff
                || self
                    .physical
                    .checked_add(length + 4)
                    .is_none_or(|end| end > self.length)
            {
                return Err(read_error(
                    "APNG块超出固定来源范围",
                    "APNG chunk exceeds the fixed source range",
                ));
            }
            if self.icc_count == 0 {
                if let Some(expected) = self.expected_icc.first {
                    let offset = self.physical - 8;
                    let end = self.physical + length + 4;
                    // 冻结的ICC不能改名或被前一块吞入，否则首帧可能先套用旧配置。
                    if offset > expected.offset
                        || offset == expected.offset
                            && (kind != *b"iCCP" || length != expected.length)
                        || offset < expected.offset && end > expected.offset
                    {
                        return Err(read_error(
                            "APNG颜色配置已改变",
                            "APNG color profile changed",
                        ));
                    }
                }
            }
            add_metadata(&mut self.metadata, &kind, length)
                .map_err(|error| io::Error::other(error.to_string()))?;
            if kind == *b"eXIf" && length != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    exif_restriction(),
                ));
            }
            if kind == *b"iCCP" {
                self.icc_count = self.icc_count.checked_add(1).ok_or_else(|| {
                    read_error("APNG颜色块数量溢出", "APNG color chunk count overflow")
                })?;
                let offset = self.physical - 8;
                if self.icc_count > self.expected_icc.count
                    || self.icc_count == 1
                        && self.expected_icc.first.is_none_or(|expected| {
                            expected.offset != offset || expected.length != length
                        })
                {
                    return Err(read_error(
                        "APNG颜色配置已改变",
                        "APNG color profile changed",
                    ));
                }
                self.icc_body = Some((offset, length, Sha256::new()));
            }
            if kind == *b"fcTL" {
                self.frames = self
                    .frames
                    .checked_add(1)
                    .ok_or_else(|| read_error("APNG帧数溢出", "APNG frame count overflow"))?;
                if self.frames > self.expected_frames {
                    return Err(read_error("APNG帧数已改变", "APNG frame count changed"));
                }
            }
            if kind == *b"IEND" {
                if length != 0
                    || self.frames != self.expected_frames
                    || self.physical + 4 != self.length
                    || self.icc_count != self.expected_icc.count
                    || <[u8; 32]>::from(self.icc_digest.clone().finalize())
                        != self.expected_icc.digest
                {
                    return Err(read_error("APNG结束块无效", "Invalid APNG end chunk"));
                }
                self.ended = true;
            }
            self.body = length + 4;
        }
        self.pending = header;
        self.pending_at = 0;
        self.pending_len = 8;
        Ok(())
    }
    fn position(&self) -> u64 {
        self.physical - (self.pending_len - self.pending_at) as u64
    }
}

impl<R: Read + Seek> Read for GuardedRead<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        self.check_cancel()?;
        if self.pending_at < self.pending_len {
            let count = output.len().min(self.pending_len - self.pending_at);
            output[..count]
                .copy_from_slice(&self.pending[self.pending_at..self.pending_at + count]);
            self.pending_at += count;
            return Ok(count);
        }
        if self.body > 0 {
            let length = output.len().min(self.body as usize);
            let count = self.raw_read(&mut output[..length])?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.body -= count as u64;
            if let Some((_, _, digest)) = self.icc_body.as_mut() {
                digest.update(&output[..count]);
            }
            if self.body == 0 {
                if let Some((offset, length, digest)) = self.icc_body.take() {
                    let digest: [u8; 32] = digest.finalize().into();
                    if self.icc_count == 1
                        && self
                            .expected_icc
                            .first
                            .is_none_or(|expected| expected.digest != digest)
                    {
                        return Err(read_error(
                            "APNG颜色配置已改变",
                            "APNG color profile changed",
                        ));
                    }
                    self.icc_digest.update(offset.to_be_bytes());
                    self.icc_digest.update(length.to_be_bytes());
                    self.icc_digest.update(digest);
                }
            }
            return Ok(count);
        }
        if self.ended {
            return Ok(0);
        }
        self.header()?;
        self.read(output)
    }
}

impl<R: Read + Seek> Seek for GuardedRead<R> {
    fn seek(&mut self, request: SeekFrom) -> io::Result<u64> {
        self.check_cancel()?;
        let target = match request {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::Current(delta) => i128::from(self.position()) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        if target < 0 || target > i128::from(self.length) {
            return Err(read_error(
                "APNG定位超出固定来源范围",
                "APNG seek exceeds the fixed source range",
            ));
        }
        let target = target as u64;
        if target == self.position() {
            return Ok(target);
        }
        self.reader
            .lock()
            .map_err(|_| read_error("APNG来源锁不可用", "APNG source lock unavailable"))?
            .seek(SeekFrom::Start(self.start))?;
        self.physical = 0;
        self.pending_at = 0;
        self.pending_len = 0;
        self.body = 0;
        self.metadata = 0;
        self.frames = 0;
        self.icc_count = 0;
        self.icc_digest = Sha256::new();
        self.icc_body = None;
        self.started = false;
        self.ended = false;
        // PNG当前顺读；若调用者重定位，重放块校验而不绕开元数据范围检查。
        let mut discard = [0; 8192];
        while self.position() < target {
            let length = (target - self.position()).min(discard.len() as u64) as usize;
            if self.read(&mut discard[..length])? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(target)
    }
}

type Decoder<R> = png::Reader<BufReader<GuardedRead<R>>>;

fn decoder_limit(scan: Scan) -> ApngResult<usize> {
    // png0.18每次进入图像数据都会累计扣一行；补偿所有帧和可选poster，避免虚累计耗尽。
    let rows = u64::from(scan.frames) + u64::from(scan.poster);
    let compensation = u64::from(scan.width)
        .checked_mul(4)
        .and_then(|row| row.checked_mul(rows))
        .and_then(|bytes| bytes.checked_add(FIXED_RESERVE))
        .ok_or_else(|| invalid("APNG解码逻辑额度溢出", "APNG decoder accounting overflow"))?;
    usize::try_from(compensation)
        .map_err(|_| invalid("APNG解码逻辑额度溢出", "APNG decoder accounting overflow"))
}

fn make_decoder<R: Read + Seek>(
    reader: Arc<Mutex<R>>,
    start: u64,
    length: u64,
    scan: Scan,
    cancel: Arc<AtomicBool>,
) -> ApngResult<Decoder<R>> {
    cancelled(&cancel)?;
    let input = GuardedRead::new(reader, start, length, scan.frames, scan.icc_layout, cancel)?;
    let mut decoder = png::Decoder::new_with_limits(
        BufReader::new(input),
        png::Limits {
            bytes: decoder_limit(scan)?,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND);
    let info = decoder.read_header_info()?;
    if info.width != scan.width
        || info.height != scan.height
        || info.color_type != scan.color
        || info.bit_depth != BitDepth::Eight
        || info.interlaced
    {
        return Err(invalid("APNG来源头已改变", "APNG source header changed"));
    }
    let reader = decoder.read_info()?;
    let info = reader.info();
    if info.animation_control.map(|a| (a.num_frames, a.num_plays))
        != Some((scan.frames, scan.plays))
        || info.frame_control.is_none() != scan.poster
    {
        return Err(invalid(
            "APNG动画声明已改变",
            "APNG animation declaration changed",
        ));
    }
    Ok(reader)
}

pub(crate) struct ApngSource<R: Read + Seek> {
    reader: Arc<Mutex<R>>,
    decoder: Option<Decoder<R>>,
    options: ApngOptions,
    cancel: Arc<AtomicBool>,
    start: u64,
    length: u64,
    scan: Scan,
    color: FrameColorManager,
    reserved: u64,
    canvas: Vec<u8>,
    decoded: Vec<u8>,
    restore: Vec<u8>,
    pending: Option<FrameControl>,
    outputs: Arc<OutputPool>,
    generation: u64,
    cycle: u64,
    index: u64,
    ended: bool,
    failed: bool,
}

impl ApngSource<File> {
    pub(crate) fn open(
        path: &Path,
        options: ApngOptions,
        cancel: Arc<AtomicBool>,
    ) -> ApngResult<Self> {
        let opened = (|| {
            cancelled(&cancel)?;
            crate::image_collection::ensure_local_file(path).map_err(ApngError::Invalid)?;
            Self::from_reader(File::open(path)?, options, cancel.clone())
        })();
        opened.map_err(|error| classify(error, &cancel))
    }
}

impl<R: Read + Seek> ApngSource<R> {
    pub(crate) fn from_reader(
        reader: R,
        options: ApngOptions,
        cancel: Arc<AtomicBool>,
    ) -> ApngResult<Self> {
        // 底层读取/定位可同时置取消并返回错误；统一在结果返回前决定取消优先级。
        Self::from_reader_inner(reader, options, cancel.clone())
            .map_err(|error| classify(error, &cancel))
    }

    fn from_reader_inner(
        mut reader: R,
        options: ApngOptions,
        cancel: Arc<AtomicBool>,
    ) -> ApngResult<Self> {
        options_valid(options)?;
        cancelled(&cancel)?;
        let start = reader.stream_position()?;
        let length = reader
            .seek(SeekFrom::End(0))?
            .checked_sub(start)
            .ok_or_else(|| invalid("APNG来源范围无效", "Invalid APNG source range"))?;
        if length > options.max_input_bytes {
            return Err(invalid(
                "APNG来源超过读入预算",
                "APNG source exceeds the input budget",
            ));
        }
        let (scan, profile) =
            scan_source_with_profile(&mut reader, start, length, options, &cancel, None)
                .map_err(|e| classify(e, &cancel))?;
        // 配置提取发生在整幅像素分配前；活动配置及失败sidecar短暂副本均先预留。
        let profile_budget = profile_constructor_bytes(profile.as_ref())?;
        color_budget(scan, options, profile_budget)?;
        let color = FrameColorManager::new(scan.source_summary(), profile, &cancel)
            .map_err(|error| classify(ApngError::Invalid(error), &cancel))?;
        let reserved = color_budget(scan, options, color.retained_bytes())?;
        let reader = Arc::new(Mutex::new(reader));
        let decoder = make_decoder(reader.clone(), start, length, scan, cancel.clone())
            .map_err(|e| classify(e, &cancel))?;
        let bytes = scan.width as usize * scan.height as usize * 4;
        cancelled(&cancel)?;
        let canvas = allocate(bytes, &cancel)?;
        let decoded = allocate(bytes, &cancel)?;
        Ok(Self {
            reader,
            decoder: Some(decoder),
            options,
            cancel,
            start,
            length,
            scan,
            color,
            reserved,
            canvas,
            decoded,
            restore: Vec::new(),
            pending: None,
            outputs: Arc::new(OutputPool {
                live: AtomicUsize::new(0),
                limit: options.max_output_frames,
            }),
            generation: 0,
            cycle: 0,
            index: 0,
            ended: false,
            failed: false,
        })
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.scan.width, self.scan.height)
    }
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.reserved
    }
    /// 来源自有Vec取实际容量，PNG内部保留沿用既有固定上界；不采用累计行逻辑额度。
    /// 配置上界允许与交付帧sidecar重复计费，不包含reader自身的输入内存。
    pub(crate) fn retained_bytes(&self) -> u64 {
        (self.canvas.capacity() as u64)
            .saturating_add(self.decoded.capacity() as u64)
            .saturating_add(self.restore.capacity() as u64)
            .saturating_add(self.color.retained_bytes() as u64)
            .saturating_add(if self.decoder.is_some() {
                FIXED_RESERVE
            } else {
                0
            })
    }
    /// APNG有限循环值是总播放次数；0表示无限。
    pub(crate) fn num_plays(&self) -> u32 {
        self.scan.plays
    }

    fn rewind(&mut self) -> ApngResult<()> {
        self.decoder = None;
        let scan = {
            let mut reader = self
                .reader
                .lock()
                .map_err(|_| invalid("APNG来源锁不可用", "APNG source lock unavailable"))?;
            cancelled(&self.cancel)?;
            if reader.seek(SeekFrom::End(0))?.checked_sub(self.start) != Some(self.length) {
                return Err(invalid("APNG来源长度已改变", "APNG source length changed"));
            }
            scan_source_with_profile(
                &mut *reader,
                self.start,
                self.length,
                self.options,
                &self.cancel,
                Some(self.scan),
            )?
            .0
        };
        if scan != self.scan {
            return Err(invalid(
                "APNG来源声明已改变",
                "APNG source declaration changed",
            ));
        }
        self.decoder = Some(make_decoder(
            self.reader.clone(),
            self.start,
            self.length,
            scan,
            self.cancel.clone(),
        )?);
        self.pending = None;
        self.restore = Vec::new();
        self.index = 0;
        for row in self.canvas.chunks_exact_mut(self.scan.width as usize * 4) {
            cancelled(&self.cancel)?;
            row.fill(0);
        }
        Ok(())
    }

    pub(crate) fn restart(&mut self, cancel: Arc<AtomicBool>) -> ApngResult<()> {
        self.cancel = cancel;
        self.failed = true;
        self.rewind()
            .map_err(|error| classify(error, &self.cancel))?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("APNG代次溢出", "APNG generation overflow"))?;
        self.cycle = 0;
        self.ended = false;
        self.failed = false;
        Ok(())
    }

    pub(crate) fn next_frame(&mut self) -> ApngResult<Option<ApngFrame>> {
        if self.failed {
            return Err(ApngError::RequiresRestart);
        }
        if let Err(error) = cancelled(&self.cancel) {
            self.failed = true;
            return Err(error);
        }
        if self.ended {
            return Ok(None);
        }
        // 取得交付名额之后才推进reader或改变合成画布；背压不消费任何源数据。
        let lease = self.outputs.acquire()?;
        match self.next_inner(lease) {
            Ok(frame) => Ok(frame),
            Err(error) => {
                self.failed = true;
                Err(classify(error, &self.cancel))
            }
        }
    }

    fn next_inner(&mut self, lease: OutputLease) -> ApngResult<Option<ApngFrame>> {
        let canvas_bytes = self.scan.width as usize * self.scan.height as usize * 4;
        check_capacity(self.canvas.capacity(), canvas_bytes)?;
        check_capacity(self.decoded.capacity(), canvas_bytes)?;
        check_capacity(self.restore.capacity(), canvas_bytes)?;
        if self.index == u64::from(self.scan.frames) {
            self.decoder
                .as_mut()
                .ok_or(ApngError::RequiresRestart)?
                .finish()?;
            if self.scan.plays != 0 && self.cycle + 1 >= u64::from(self.scan.plays) {
                // 不处置末帧，调用方仍可保留最后的合成画面。
                self.ended = true;
                return Ok(None);
            }
            self.rewind()?;
            self.cycle = self
                .cycle
                .checked_add(1)
                .ok_or_else(|| invalid("APNG循环次数溢出", "APNG cycle overflow"))?;
        }
        let decoder = self.decoder.as_mut().ok_or(ApngError::RequiresRestart)?;
        if self.index == 0 && self.scan.poster {
            decoder.next_frame(&mut self.decoded)?;
            cancelled(&self.cancel)?;
        }
        let mut control = if self.index == 0 && !self.scan.poster {
            decoder
                .info()
                .frame_control
                .ok_or_else(|| invalid("APNG缺少首帧控制", "APNG first frame control is missing"))?
        } else {
            *decoder.next_frame_info()?
        };
        validate_control(control, self.scan.width, self.scan.height)?;
        if self.index == 0 && control.dispose_op == DisposeOp::Previous {
            control.dispose_op = DisposeOp::Background;
        }
        if let Some(previous) = self.pending.take() {
            dispose(
                &mut self.canvas,
                self.scan.width,
                previous,
                &self.restore,
                &self.cancel,
            )?;
        }
        if control.dispose_op == DisposeOp::Previous {
            let bytes = control.width as usize * control.height as usize * 4;
            // 丢掉旧分配后再增长，避免realloc瞬间同时持有两份最坏Previous区域。
            if self.restore.capacity() < bytes {
                self.restore = Vec::new();
                self.restore = allocate(bytes, &self.cancel)?;
            } else {
                self.restore.resize(bytes, 0);
            }
            for row in 0..control.height as usize {
                cancelled(&self.cancel)?;
                let from = ((control.y_offset as usize + row) * self.scan.width as usize
                    + control.x_offset as usize)
                    * 4;
                let to = row * control.width as usize * 4;
                self.restore[to..to + control.width as usize * 4]
                    .copy_from_slice(&self.canvas[from..from + control.width as usize * 4]);
            }
        } else {
            self.restore.clear();
        }
        let output = decoder.next_frame(&mut self.decoded)?;
        let channels = match (output.color_type, output.bit_depth) {
            (ColorType::Rgb, BitDepth::Eight) => 3,
            (ColorType::Rgba, BitDepth::Eight) => 4,
            _ => {
                return Err(ApngError::Unsupported(
                    tr!(
                        "APNG解码颜色类型不受支持",
                        "Unsupported APNG decoded color type"
                    )
                    .into(),
                ))
            }
        };
        if output.width != control.width
            || output.height != control.height
            || output.line_size != control.width as usize * channels
        {
            return Err(invalid(
                "APNG子帧尺寸与缓冲不一致",
                "APNG subframe dimensions do not match its buffer",
            ));
        }
        composite(
            &mut self.canvas,
            self.scan.width,
            control,
            &self.decoded,
            channels,
            &self.cancel,
        )?;
        self.pending = Some(control);
        // 合成与Previous始终留在编码RGB；颜色变换直接写此租约的唯一交付图。
        // 管理器失败会先丢弃部分输出，再从未改动的canvas生成完整降级图。
        let managed = self
            .color
            .render(
                (self.scan.width, self.scan.height),
                &self.canvas,
                &self.cancel,
            )
            .map_err(|error| classify(ApngError::Invalid(error), &self.cancel))?;
        cancelled(&self.cancel)?;
        check_capacity(managed.image.bgra.capacity(), canvas_bytes)?;
        let frame = ApngFrame {
            image: managed.image,
            metadata_source: managed.metadata_source,
            warning: managed.warning,
            _lease: lease,
            generation: self.generation,
            cycle: self.cycle,
            index: self.index,
            timing: ApngTiming::from_control(control, self.options.zero_delay),
        };
        self.index += 1;
        Ok(Some(frame))
    }
}

fn validate_control(control: FrameControl, width: u32, height: u32) -> ApngResult<()> {
    if control.width == 0
        || control.height == 0
        || Some(control.width) > width.checked_sub(control.x_offset)
        || Some(control.height) > height.checked_sub(control.y_offset)
    {
        return Err(invalid(
            "APNG帧区域越界",
            "APNG frame rectangle is outside the canvas",
        ));
    }
    Ok(())
}

fn dispose(
    canvas: &mut [u8],
    width: u32,
    control: FrameControl,
    restore: &[u8],
    cancel: &AtomicBool,
) -> ApngResult<()> {
    if control.dispose_op == DisposeOp::None {
        return Ok(());
    }
    let row_bytes = control.width as usize * 4;
    for row in 0..control.height as usize {
        cancelled(cancel)?;
        let offset =
            ((control.y_offset as usize + row) * width as usize + control.x_offset as usize) * 4;
        let target = &mut canvas[offset..offset + row_bytes];
        if control.dispose_op == DisposeOp::Background {
            target.fill(0);
        } else {
            let source = restore
                .get(row * row_bytes..(row + 1) * row_bytes)
                .ok_or_else(|| invalid("APNG Previous缓冲无效", "Invalid APNG Previous buffer"))?;
            target.copy_from_slice(source);
        }
    }
    Ok(())
}

fn composite(
    canvas: &mut [u8],
    width: u32,
    control: FrameControl,
    decoded: &[u8],
    channels: usize,
    cancel: &AtomicBool,
) -> ApngResult<()> {
    for row in 0..control.height as usize {
        cancelled(cancel)?;
        let from = row * control.width as usize * channels;
        let to =
            ((control.y_offset as usize + row) * width as usize + control.x_offset as usize) * 4;
        let source = &decoded[from..from + control.width as usize * channels];
        let target = &mut canvas[to..to + control.width as usize * 4];
        for (rgb, bgra) in source
            .chunks_exact(channels)
            .zip(target.chunks_exact_mut(4))
        {
            let alpha = if channels == 4 { rgb[3] } else { 255 };
            if control.blend_op == BlendOp::Source || alpha == 255 {
                bgra.copy_from_slice(&[rgb[2], rgb[1], rgb[0], alpha]);
            } else if alpha != 0 {
                let sa = u32::from(alpha);
                let da = u32::from(bgra[3]);
                let a = sa * 255 + da * (255 - sa);
                for (channel, color) in [rgb[2], rgb[1], rgb[0]].into_iter().enumerate() {
                    let value =
                        u32::from(color) * sa * 255 + u32::from(bgra[channel]) * da * (255 - sa);
                    bgra[channel] = ((value + a / 2) / a) as u8;
                }
                bgra[3] = ((a + 127) / 255) as u8;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "image_apng_tests.rs"]
mod tests;

#[cfg(test)]
mod guard_tests {
    use super::*;
    use std::io::Cursor;

    fn token() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    // 这些仅用于头预扫/读取边界测试，CRC留空；真实像素小样由编码器生成。
    fn chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) -> usize {
        let offset = bytes.len();
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(&[0; 4]);
        offset
    }

    fn raw_headers(idat_length: usize) -> (Vec<u8>, usize) {
        let mut bytes = SIGNATURE.to_vec();
        chunk(
            &mut bytes,
            b"IHDR",
            &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0],
        );
        chunk(&mut bytes, b"acTL", &[0, 0, 0, 1, 0, 0, 0, 1]);
        let mut control = [0; 26];
        control[7] = 1;
        control[11] = 1;
        chunk(&mut bytes, b"fcTL", &control);
        let idat = chunk(&mut bytes, b"IDAT", &vec![0; idat_length]);
        chunk(&mut bytes, b"IEND", &[]);
        (bytes, idat)
    }

    #[test]
    fn image_apng_budget_includes_outputs_presentation_and_fixed_reserve() {
        let options = ApngOptions::default();
        assert_eq!(
            canvas_budget(3, 2, options).unwrap(),
            6 * 48 + FIXED_RESERVE
        );
        assert!(canvas_budget(2000, 2400, options).is_ok());
        assert!(canvas_budget(2500, 2000, options).is_err());
        assert!(canvas_budget(65_536, 1, options).is_err());
        assert!(options_valid(ApngOptions {
            max_output_frames: 5,
            ..options
        })
        .is_err());
        assert!(options_valid(ApngOptions {
            zero_delay: Duration::ZERO,
            ..options
        })
        .is_err());
    }

    #[test]
    fn image_apng_prescan_rejects_large_canvas_before_image_data() {
        let mut bytes = SIGNATURE.to_vec();
        let mut ihdr = [0; 13];
        ihdr[..4].copy_from_slice(&32_000u32.to_be_bytes());
        ihdr[4..8].copy_from_slice(&32_000u32.to_be_bytes());
        ihdr[8] = 8;
        ihdr[9] = 6;
        chunk(&mut bytes, b"IHDR", &ihdr);
        let length = bytes.len() as u64;
        assert!(matches!(
            scan_source(
                &mut Cursor::new(bytes),
                0,
                length,
                ApngOptions::default(),
                &token()
            ),
            Err(ApngError::Invalid(_))
        ));
    }

    #[test]
    fn image_apng_runtime_guard_rechecks_same_length_metadata_mutation() {
        let (mut bytes, image_header) = raw_headers(MAX_METADATA_BYTES as usize + 1);
        let length = bytes.len() as u64;
        let scan = scan_source(
            &mut Cursor::new(&bytes),
            0,
            length,
            ApngOptions::default(),
            &token(),
        )
        .unwrap();
        assert!(scan.metadata_bytes < 1024);
        // 来源长度相同，但原图像数据块被改成巨大元数据；在内容交给解码器前拒绝。
        bytes[image_header + 4..image_header + 8].copy_from_slice(b"eXIf");
        let reader = Arc::new(Mutex::new(Cursor::new(bytes)));
        let mut input = GuardedRead::new(reader, 0, length, 1, scan.icc_layout, token()).unwrap();
        let error = io::copy(&mut input, &mut io::sink())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("metadata") || error.contains("元数据"),
            "{error}"
        );
    }

    #[test]
    fn image_apng_cancelled_read_is_not_retryable_interrupted() {
        let (bytes, _) = raw_headers(0);
        let length = bytes.len() as u64;
        let cancel = token();
        let reader = Arc::new(Mutex::new(Cursor::new(bytes)));
        let mut input =
            GuardedRead::new(reader, 0, length, 1, IccLayout::empty(), cancel.clone()).unwrap();
        cancel.store(true, Ordering::Release);
        assert_eq!(
            input.read(&mut [0; 8]).unwrap_err().kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn image_apng_exif_uses_oriented_static_fallback_and_runtime_rejects_insertion() {
        let exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(ColorType::Rgba);
            encoder.set_depth(BitDepth::Eight);
            encoder.set_animated(1, 1).unwrap();
            let mut writer = encoder.write_header().unwrap();
            writer.write_chunk(png::chunk::eXIf, exif).unwrap();
            writer
                .write_image_data(&[255, 0, 0, 255, 0, 0, 255, 255])
                .unwrap();
            writer.finish().unwrap();
        }
        let decoded = crate::image_decode::decode_bytes(&bytes).unwrap();
        assert_eq!(
            decoded.metadata.orientation.map(|value| value.to_exif()),
            Some(6)
        );
        assert_eq!((decoded.image.width, decoded.image.height), (1, 2));
        assert_eq!(decoded.image.bgra, [0, 0, 255, 255, 255, 0, 0, 255]);
        assert!(matches!(
            ApngSource::from_reader(Cursor::new(bytes), ApngOptions::default(), token()),
            Err(ApngError::Unsupported(_))
        ));

        let (mut bytes, image_header) = raw_headers(exif.len());
        let length = bytes.len() as u64;
        scan_source(
            &mut Cursor::new(&bytes),
            0,
            length,
            ApngOptions::default(),
            &token(),
        )
        .unwrap();
        bytes[image_header + 4..image_header + 8].copy_from_slice(b"eXIf");
        bytes[image_header + 8..image_header + 8 + exif.len()].copy_from_slice(exif);
        let mut input = GuardedRead::new(
            Arc::new(Mutex::new(Cursor::new(bytes))),
            0,
            length,
            1,
            IccLayout::empty(),
            token(),
        )
        .unwrap();
        let error = io::copy(&mut input, &mut io::sink()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(matches!(
            ApngError::from(png::DecodingError::IoError(error)),
            ApngError::Unsupported(_)
        ));
    }

    #[test]
    fn image_apng_long_sequence_does_not_exhaust_cumulative_row_accounting() {
        let width = 32_768;
        let frames = 300;
        assert!(u64::from(width) * 4 * u64::from(frames) > FIXED_RESERVE);
        let pixels: Vec<_> = [1, 2, 3, 255]
            .into_iter()
            .cycle()
            .take(width as usize * 4)
            .collect();
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, 1);
            encoder.set_color(ColorType::Rgba);
            encoder.set_depth(BitDepth::Eight);
            encoder.set_animated(frames, 1).unwrap();
            let mut writer = encoder.write_header().unwrap();
            for _ in 0..frames {
                writer.write_image_data(&pixels).unwrap();
            }
            writer.finish().unwrap();
        }
        let mut source =
            ApngSource::from_reader(Cursor::new(bytes), ApngOptions::default(), token()).unwrap();
        for index in 0..frames {
            let frame = source.next_frame().unwrap().unwrap();
            assert_eq!(frame.index, u64::from(index));
            assert_eq!(&frame.image().bgra[..4], &[3, 2, 1, 255]);
        }
        assert!(source.next_frame().unwrap().is_none());
    }
}
