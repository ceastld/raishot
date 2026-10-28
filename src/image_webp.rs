//! 受控动态WebP来源：帧按静态VP8X视图解码，原色画布由本层合成。
//! 只缓存目录、必要元数据和有界交付帧；不收集全动画或复制整份压缩来源。
//! 混合采用8位原编码RGB，完整画布再转换颜色；本批未实现线性光空间的alpha混合。
//! 动画块语义见https://developers.google.com/speed/webp/docs/riff_container。
use crate::{
    capture::Image,
    image_color_management::FrameColorManager,
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
    image_export_metadata::SourceMetadata,
};
use image::ImageDecoder;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::File,
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_ICC: u64 = 4 * 1024 * 1024;
const MAX_FRAMES: usize = 20_000;
const MAX_METADATA_PARTS: usize = 4096;
const MAX_SPANS: usize = MAX_FRAMES * 12 + MAX_METADATA_PARTS * 3 + 16;
// 唯一静态帧视图、64KiB reader、哈希读入及小型目录；各阶段互不累积。
const FRAME_IO_RESERVE: u64 = 128 * 1024;
const OBSERVER_SCRATCH: u64 = 256 * 1024;
const VIEWER_CACHE_RESERVE: u64 = 8 * 1024 * 1024;
const CONSTRUCTOR_FIXED_BYTES: u64 = VIEWER_CACHE_RESERVE + MAX_METADATA + FRAME_IO_RESERVE;
// 当前/退场预览最多12P，worker显示副本4P和透明底合成4P；超长边另留tile原图。
const PRESENTATION_BPP: u64 = 20;

#[derive(Clone, Copy, Debug)]
pub(crate) struct WebpOptions {
    pub max_canvas_pixels: u64,
    /// 来源的像素/codec工作集及呈现预留；调用方已有输入、GPU和系统CMM缓存不在此数内。
    pub max_total_bytes: u64,
    pub max_input_bytes: u64,
    pub max_output_frames: usize,
    pub zero_delay: Duration,
    pub max_frame_encoded_bytes: u64,
}
impl Default for WebpOptions {
    fn default() -> Self {
        Self {
            max_canvas_pixels: 5_000_000,
            max_total_bytes: MAX_BYTES,
            max_input_bytes: MAX_BYTES,
            max_output_frames: 4,
            zero_delay: Duration::from_millis(100),
            max_frame_encoded_bytes: MAX_FRAME_BYTES,
        }
    }
}

#[derive(Debug)]
pub(crate) enum WebpError {
    Unsupported(String),
    Invalid(String),
    Io(io::Error),
    Decode(image::ImageError),
    Backpressure,
    Cancelled,
    RequiresRestart,
}
impl fmt::Display for WebpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message) | Self::Invalid(message) => f.write_str(message),
            Self::Io(error) => fmt::Display::fmt(error, f),
            Self::Decode(error) => fmt::Display::fmt(error, f),
            Self::Backpressure => {
                f.write_str(tr!("等待WebP交付帧释放", "Waiting for WebP frame slots"))
            }
            Self::Cancelled => f.write_str(tr!("WebP读取已取消", "WebP reading canceled")),
            Self::RequiresRestart => {
                f.write_str(tr!("WebP来源需要重启", "WebP source needs a restart"))
            }
        }
    }
}
impl std::error::Error for WebpError {}
impl From<io::Error> for WebpError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<image::ImageError> for WebpError {
    fn from(error: image::ImageError) -> Self {
        Self::Decode(error)
    }
}
type WebpResult<T> = Result<T, WebpError>;
fn invalid(zh: &'static str, en: &'static str) -> WebpError {
    WebpError::Invalid(crate::i18n::tr(zh, en).into())
}
fn unsupported(zh: &'static str, en: &'static str) -> WebpError {
    WebpError::Unsupported(crate::i18n::tr(zh, en).into())
}
fn cancelled(cancel: &AtomicBool) -> WebpResult<()> {
    if cancel.load(Ordering::Acquire) {
        Err(WebpError::Cancelled)
    } else {
        Ok(())
    }
}
fn classify(error: WebpError, cancel: &AtomicBool) -> WebpError {
    if cancel.load(Ordering::Acquire) {
        WebpError::Cancelled
    } else {
        error
    }
}
fn bad() -> WebpError {
    invalid(
        "WebP块、目录或范围无效",
        "Invalid WebP chunk, directory or range",
    )
}
fn allocation() -> WebpError {
    invalid("WebP缓冲分配失败", "WebP buffer allocation failed")
}
fn bytes(length: usize) -> WebpResult<Vec<u8>> {
    let mut value = Vec::new();
    value.try_reserve_exact(length).map_err(|_| allocation())?;
    value.resize(length, 0);
    Ok(value)
}
fn push_bounded<T>(values: &mut Vec<T>, value: T, limit: usize) -> WebpResult<()> {
    if values.len() >= limit {
        return Err(invalid(
            "WebP目录超过受控上限",
            "WebP directory exceeds its limit",
        ));
    }
    if values.len() == values.capacity() {
        let capacity = values.capacity().max(8).saturating_mul(2).min(limit);
        values
            .try_reserve_exact(capacity - values.len())
            .map_err(|_| allocation())?;
    }
    values.push(value);
    Ok(())
}

fn constructor_minimum(options: WebpOptions, cancel: &AtomicBool) -> WebpResult<()> {
    cancelled(cancel)?;
    options_valid(options)?;
    working_set(&[CONSTRUCTOR_FIXED_BYTES], options.max_total_bytes)?;
    Ok(())
}

fn vector_bytes<T>(capacity: usize) -> WebpResult<u64> {
    u64::try_from(capacity)
        .ok()
        .and_then(|count| count.checked_mul(std::mem::size_of::<T>() as u64))
        .ok_or_else(allocation)
}

/// 扫描只计当前目录、ICC与固定池；未来画布/交付/呈现不与目录扩容峰叠加。
struct ConstructorLedger {
    limit: u64,
    directories: u64,
    profile: u64,
    peak: u64,
}
impl ConstructorLedger {
    fn new(options: WebpOptions, cancel: &AtomicBool) -> WebpResult<Self> {
        constructor_minimum(options, cancel)?;
        Ok(Self {
            limit: options.max_total_bytes,
            directories: 0,
            profile: 0,
            peak: CONSTRUCTOR_FIXED_BYTES,
        })
    }
    fn account(&mut self, extra: u64) -> WebpResult<()> {
        let bytes = working_set(
            &[
                CONSTRUCTOR_FIXED_BYTES,
                self.directories,
                self.profile,
                extra,
            ],
            self.limit,
        )?;
        self.peak = self.peak.max(bytes);
        Ok(())
    }
    fn push<T>(
        &mut self,
        values: &mut Vec<T>,
        value: T,
        limit: usize,
        cancel: &AtomicBool,
    ) -> WebpResult<()> {
        cancelled(cancel)?;
        if values.len() >= limit {
            return Err(invalid(
                "WebP目录超过受控上限",
                "WebP directory exceeds its limit",
            ));
        }
        if values.len() == values.capacity() {
            let old = vector_bytes::<T>(values.capacity())?;
            let capacity = values.capacity().max(8).saturating_mul(2).min(limit);
            // directories仍包含旧Vec；请求与actual两道门均覆盖旧、新分配共存。
            self.account(vector_bytes::<T>(capacity)?)?;
            let reserved = values.try_reserve_exact(capacity - values.len());
            cancelled(cancel)?;
            reserved.map_err(|_| allocation())?;
            let actual = vector_bytes::<T>(values.capacity())?;
            self.account(actual)?;
            self.directories = self
                .directories
                .checked_sub(old)
                .and_then(|bytes| bytes.checked_add(actual))
                .ok_or_else(allocation)?;
        }
        values.push(value);
        cancelled(cancel)
    }
    fn profile_capacity(&mut self, capacity: usize) -> WebpResult<()> {
        if self.profile != 0 {
            return Err(bad());
        }
        let bytes = (capacity as u64).checked_mul(3).ok_or_else(allocation)?;
        self.account(bytes)?;
        self.profile = bytes;
        Ok(())
    }
    fn allocate_profile(&mut self, length: usize, cancel: &AtomicBool) -> WebpResult<Vec<u8>> {
        cancelled(cancel)?;
        let requested = (length as u64).checked_mul(3).ok_or_else(allocation)?;
        self.account(requested)?;
        let allocated = bytes(length);
        cancelled(cancel)?;
        let profile = allocated?;
        self.profile_capacity(profile.capacity())?;
        Ok(profile)
    }
    fn discard_profile(&mut self, profile: &mut Option<Vec<u8>>) {
        drop(profile.take());
        self.profile = 0;
    }
}
fn le24(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], 0])
}
fn le32(data: &[u8]) -> u32 {
    u32::from_le_bytes(data[..4].try_into().unwrap())
}
fn seek<R: Seek>(reader: &mut R, position: SeekFrom, cancel: &AtomicBool) -> WebpResult<u64> {
    cancelled(cancel)?;
    let position = reader.seek(position);
    cancelled(cancel)?;
    Ok(position?)
}
fn read<R: Read>(reader: &mut R, data: &mut [u8], cancel: &AtomicBool) -> WebpResult<()> {
    for part in data.chunks_mut(64 * 1024) {
        let mut written = 0;
        while written < part.len() {
            cancelled(cancel)?;
            let result = reader.read(&mut part[written..]);
            cancelled(cancel)?;
            let count = match result {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if count == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
            }
            written += count;
        }
    }
    Ok(())
}
fn read_at<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    offset: u64,
    data: &mut [u8],
    cancel: &AtomicBool,
) -> WebpResult<()> {
    let absolute = start.checked_add(offset).ok_or_else(bad)?;
    seek(reader, SeekFrom::Start(absolute), cancel)?;
    read(reader, data, cancel)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WebpTiming {
    pub raw_delay_ms: u32,
    pub delay: Duration,
}
struct OutputPool {
    live: AtomicUsize,
    limit: usize,
}
struct OutputLease(Arc<OutputPool>);
impl OutputPool {
    fn acquire(self: &Arc<Self>) -> WebpResult<OutputLease> {
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.limit).then_some(live + 1)
            })
            .map_err(|_| WebpError::Backpressure)?;
        Ok(OutputLease(self.clone()))
    }
}
impl Drop for OutputLease {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::Release);
    }
}
pub(crate) struct WebpFrame {
    image: Image,
    metadata_source: Arc<SourceMetadata>,
    warning: Option<String>,
    _lease: OutputLease,
    pub generation: u64,
    pub cycle: u64,
    pub index: u64,
    pub timing: WebpTiming,
}
impl WebpFrame {
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

#[derive(Clone, Copy)]
struct Chunk {
    kind: [u8; 4],
    offset: u64,
    length: u64,
    prefix: [u8; 64],
    prefix_len: usize,
}
#[derive(Clone, Copy)]
struct FrameSpec {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    duration: u32,
    blend: bool,
    dispose: bool,
    chunks: [Option<Chunk>; 2],
    control_start: usize,
    control_end: usize,
}
#[derive(Clone, Copy)]
struct ControlSpan {
    offset: u64,
    length: u64,
    digest: [u8; 32],
}
struct Scan {
    width: u32,
    height: u32,
    plays: u32,
    background: [u8; 4],
    frames: Vec<FrameSpec>,
    global_controls: Vec<ControlSpan>,
    controls: Vec<ControlSpan>,
    icc: FieldSummary,
    profile: Option<Vec<u8>>,
    max_workspace: u64,
    constructor_peak: u64,
}
impl Scan {
    fn source_summary(&self) -> Summary {
        Summary {
            icc: self.icc,
            exif: absent(),
            color_model: ColorModel::Rgb,
            bits_per_channel: Some(8),
            orientation: None,
        }
    }
    fn directory_bytes(&self) -> u64 {
        (self.frames.capacity() * std::mem::size_of::<FrameSpec>()
            + self.global_controls.capacity() * std::mem::size_of::<ControlSpan>()
            + self.controls.capacity() * std::mem::size_of::<ControlSpan>()) as u64
    }
}
fn absent() -> FieldSummary {
    FieldSummary {
        status: MetadataStatus::Absent,
        encoded_bytes: 0,
        decoded_bytes: None,
    }
}
fn remember(
    controls: &mut Vec<ControlSpan>,
    offset: u64,
    data: &[u8],
    ledger: &mut ConstructorLedger,
    cancel: &AtomicBool,
) -> WebpResult<()> {
    ledger.push(
        controls,
        ControlSpan {
            offset,
            length: data.len() as u64,
            digest: Sha256::digest(data).into(),
        },
        MAX_SPANS,
        cancel,
    )
}
fn remember_stream<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    offset: u64,
    length: u64,
    controls: &mut Vec<ControlSpan>,
    ledger: &mut ConstructorLedger,
    cancel: &AtomicBool,
) -> WebpResult<()> {
    seek(
        reader,
        SeekFrom::Start(start.checked_add(offset).ok_or_else(bad)?),
        cancel,
    )?;
    let mut digest = Sha256::new();
    let mut scratch = [0; 64 * 1024];
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(scratch.len() as u64) as usize;
        read(reader, &mut scratch[..count], cancel)?;
        digest.update(&scratch[..count]);
        remaining -= count as u64;
    }
    ledger.push(
        controls,
        ControlSpan {
            offset,
            length,
            digest: digest.finalize().into(),
        },
        MAX_SPANS,
        cancel,
    )
}
fn verify<'a, R: Read + Seek>(
    reader: &mut R,
    start: u64,
    length: u64,
    controls: impl Iterator<Item = &'a ControlSpan>,
    cancel: &AtomicBool,
) -> WebpResult<()> {
    let end = seek(reader, SeekFrom::End(0), cancel)?;
    if end.checked_sub(start) != Some(length) {
        return Err(invalid("WebP来源长度已变化", "WebP source length changed"));
    }
    let mut scratch = [0; 64 * 1024];
    for span in controls {
        seek(
            reader,
            SeekFrom::Start(start.checked_add(span.offset).ok_or_else(bad)?),
            cancel,
        )?;
        let mut digest = Sha256::new();
        let mut remaining = span.length;
        while remaining > 0 {
            let count = remaining.min(scratch.len() as u64) as usize;
            read(reader, &mut scratch[..count], cancel)?;
            digest.update(&scratch[..count]);
            remaining -= count as u64;
        }
        if <[u8; 32]>::from(digest.finalize()) != span.digest {
            return Err(invalid(
                "WebP元数据或帧声明已变化，请重新打开",
                "WebP metadata or frame declarations changed; reopen the image",
            ));
        }
    }
    cancelled(cancel)
}
fn options_valid(options: WebpOptions) -> WebpResult<()> {
    if options.max_canvas_pixels == 0
        || options.max_canvas_pixels > 32_000_000
        || options.max_total_bytes == 0
        || options.max_total_bytes > MAX_BYTES
        || options.max_input_bytes == 0
        || options.max_input_bytes > MAX_BYTES
        || !(1..=4).contains(&options.max_output_frames)
        || options.zero_delay.is_zero()
        || options.max_frame_encoded_bytes == 0
        || options.max_frame_encoded_bytes > MAX_FRAME_BYTES
    {
        return Err(invalid(
            "WebP预算或零延时策略无效",
            "Invalid WebP budget or zero-delay policy",
        ));
    }
    Ok(())
}
fn canvas_budget(scan: &Scan, options: WebpOptions, retained: usize) -> WebpResult<u64> {
    canvas_budget_with_capacity(
        scan,
        options,
        retained,
        u64::from(scan.width) * u64::from(scan.height) * 4,
    )
}
fn working_set(parts: &[u64], limit: u64) -> WebpResult<u64> {
    parts
        .iter()
        .try_fold(0u64, |sum, part| sum.checked_add(*part))
        .filter(|sum| *sum <= limit)
        .ok_or_else(|| {
            invalid(
                "WebP工作集超过总预算",
                "WebP working set exceeds the total budget",
            )
        })
}
fn color_pool(
    retained: usize,
    warning: usize,
    historical_warning: usize,
    color_reserved: usize,
    outputs: usize,
) -> WebpResult<usize> {
    let non_warning = retained.checked_sub(warning).ok_or_else(bad)?;
    let maximum = warning.max(historical_warning);
    let warning_bytes = outputs
        .checked_add(4)
        .and_then(|count| maximum.checked_mul(count));
    if non_warning > color_reserved || warning_bytes.is_none_or(|bytes| bytes as u64 > MAX_METADATA)
    {
        return Err(invalid(
            "WebP颜色元数据超过预留池",
            "WebP color metadata exceeds its reserved pool",
        ));
    }
    Ok(maximum)
}
fn source_base(
    scan: &Scan,
    options: WebpOptions,
    retained: usize,
    canvas_capacity: u64,
) -> WebpResult<u64> {
    let pixels = u64::from(scan.width) * u64::from(scan.height);
    if scan.width == 0
        || scan.height == 0
        || scan.width > 65_535
        || scan.height > 65_535
        || pixels > options.max_canvas_pixels
        || canvas_capacity < pixels * 4
    {
        return Err(invalid(
            "WebP画布超过尺寸预算",
            "WebP canvas exceeds its dimension budget",
        ));
    }
    let presentation = PRESENTATION_BPP
        + if scan.width > 16_384 || scan.height > 16_384 {
            4
        } else {
            0
        };
    let rows = FrameColorManager::row_buffer_bytes(scan.width).map_err(WebpError::Invalid)? as u64;
    // metadata是逻辑池，不同时保留一份8MiB原元数据Vec。目录/ICC另计；
    // 此池覆盖manager warning、全部交付clone、格式化临时份和UI两份文案。
    // 交付名额含当前将生成的完整帧；局部 codec 图与编码 Vec 另算，
    // 不把尚未使用的名额当作额外分配余量。系统CMM私有缓存不在此模型内。
    working_set(
        &[
            canvas_capacity,
            pixels * (4 * options.max_output_frames as u64 + presentation),
            VIEWER_CACHE_RESERVE,
            MAX_METADATA,
            scan.directory_bytes(),
            retained as u64,
            rows,
        ],
        options.max_total_bytes,
    )
}
fn canvas_budget_with_capacity(
    scan: &Scan,
    options: WebpOptions,
    retained: usize,
    canvas_capacity: u64,
) -> WebpResult<u64> {
    working_set(
        &[
            source_base(scan, options, retained, canvas_capacity)?,
            scan.max_workspace,
        ],
        options.max_total_bytes,
    )
}

fn check_codec(chunk: &Chunk, width: u32, height: u32) -> WebpResult<()> {
    let prefix = &chunk.prefix[..chunk.prefix_len];
    match &chunk.kind {
        b"VP8L" => {
            if prefix.len() < 5 || prefix[0] != 0x2f {
                return Err(bad());
            }
            let header = le32(&prefix[1..]);
            if header >> 29 != 0
                || ((header & 0x3fff) + 1, ((header >> 14) & 0x3fff) + 1) != (width, height)
            {
                return Err(bad());
            }
            // 目录阶段只绑定固定头；完整变换/熵表在唯一冻结帧上观察。
            Ok(())
        }
        b"VP8 " => {
            if prefix.len() < 10 || prefix[0] & 1 != 0 || prefix[3..6] != [0x9d, 1, 0x2a] {
                return Err(bad());
            }
            let w = u16::from_le_bytes([prefix[6], prefix[7]]) & 0x3fff;
            let h = u16::from_le_bytes([prefix[8], prefix[9]]) & 0x3fff;
            if (u32::from(w), u32::from(h)) != (width, height) {
                return Err(invalid(
                    "WebP帧尺寸与VP8头不一致",
                    "WebP frame dimensions do not match the VP8 header",
                ));
            }
            let first_partition = le24(prefix) >> 5;
            if u64::from(first_partition) + 10 > chunk.length {
                return Err(bad());
            }
            Ok(())
        }
        b"ALPH" => {
            let info = *prefix.first().ok_or_else(bad)?;
            if info & 0xc0 != 0 || (info >> 4) & 3 > 1 {
                return Err(bad());
            }
            match info & 3 {
                0 if chunk.length == 1 + u64::from(width) * u64::from(height) => Ok(()),
                0 => Err(bad()),
                1 => Ok(()),
                _ => Err(unsupported(
                    "WebP alpha压缩方法暂不支持",
                    "Unsupported WebP alpha compression",
                )),
            }
        }
        _ => Err(bad()),
    }
}

fn checked_chunk_end(position: u64, length: u64, source_end: u64) -> WebpResult<u64> {
    position
        .checked_add(8)
        .and_then(|n| n.checked_add(length))
        .and_then(|n| n.checked_add(length & 1))
        .filter(|n| *n <= source_end)
        .ok_or_else(bad)
}
fn padding<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    offset: u64,
    size: u64,
    controls: &mut Vec<ControlSpan>,
    ledger: &mut ConstructorLedger,
    cancel: &AtomicBool,
) -> WebpResult<()> {
    if size & 1 != 0 {
        let mut byte = [0];
        read_at(reader, start, offset + size, &mut byte, cancel)?;
        if byte[0] != 0 {
            return Err(bad());
        }
        remember(controls, offset + size, &byte, ledger, cancel)?;
    }
    Ok(())
}
fn scan<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    length: u64,
    options: WebpOptions,
    cancel: &AtomicBool,
) -> WebpResult<Scan> {
    let mut ledger = ConstructorLedger::new(options, cancel)?;
    if !(30..=options.max_input_bytes).contains(&length) {
        return Err(invalid(
            "WebP来源超过输入预算或被截断",
            "WebP source exceeds its input budget or is truncated",
        ));
    }
    let mut riff = [0; 12];
    read_at(reader, start, 0, &mut riff, cancel)?;
    if &riff[..4] != b"RIFF" || &riff[8..] != b"WEBP" || u64::from(le32(&riff[4..])) + 8 != length {
        return Err(bad());
    }
    let mut result = Scan {
        width: 0,
        height: 0,
        plays: 0,
        background: [0; 4],
        frames: Vec::new(),
        global_controls: Vec::new(),
        controls: Vec::new(),
        icc: absent(),
        profile: None,
        max_workspace: 0,
        constructor_peak: 0,
    };
    remember(&mut result.global_controls, 0, &riff, &mut ledger, cancel)?;
    let mut position = 12;
    let mut flags = 0;
    let mut anim = false;
    let mut icc_count = 0;
    let mut metadata = 0u64;
    let mut metadata_parts = 0;
    while position < length {
        if length - position < 8 {
            return Err(bad());
        }
        let mut header = [0; 8];
        read_at(reader, start, position, &mut header, cancel)?;
        let kind: [u8; 4] = header[..4].try_into().unwrap();
        let size = u64::from(le32(&header[4..]));
        let end = checked_chunk_end(position, size, length)?;
        let frame_control_start = result.controls.len();
        if kind == *b"ANMF" {
            remember(&mut result.controls, position, &header, &mut ledger, cancel)?;
        } else {
            remember(
                &mut result.global_controls,
                position,
                &header,
                &mut ledger,
                cancel,
            )?;
        }
        let data = position + 8;
        if kind != *b"ANMF" {
            metadata_parts += 1;
            metadata = metadata
                .checked_add(size)
                .filter(|n| *n <= MAX_METADATA)
                .ok_or_else(|| invalid("WebP元数据超过8MiB", "WebP metadata exceeds 8 MiB"))?;
            if metadata_parts > MAX_METADATA_PARTS {
                return Err(invalid(
                    "WebP元数据块超过4096项",
                    "WebP metadata chunks exceed 4096",
                ));
            }
        }
        match &kind {
            b"VP8X" if position == 12 && size == 10 => {
                let mut raw = [0; 10];
                read_at(reader, start, data, &mut raw, cancel)?;
                remember(&mut result.global_controls, data, &raw, &mut ledger, cancel)?;
                flags = raw[0];
                if flags & 0xc1 != 0 || flags & 2 == 0 || raw[1..4] != [0, 0, 0] {
                    return Err(unsupported(
                        "此WebP不是受控动画",
                        "This WebP is not a supported animation",
                    ));
                }
                result.width = le24(&raw[4..]) + 1;
                result.height = le24(&raw[7..]) + 1;
                let pixels = u64::from(result.width) * u64::from(result.height);
                if result.width > 65_535
                    || result.height > 65_535
                    || pixels > options.max_canvas_pixels
                {
                    return Err(invalid(
                        "WebP画布超过尺寸预算",
                        "WebP canvas exceeds its dimension budget",
                    ));
                }
            }
            _ if position == 12 => return Err(bad()),
            b"VP8X" => return Err(bad()),
            b"ANIM" if !anim && result.frames.is_empty() && size == 6 => {
                let mut raw = [0; 6];
                read_at(reader, start, data, &mut raw, cancel)?;
                remember(&mut result.global_controls, data, &raw, &mut ledger, cancel)?;
                result.background.copy_from_slice(&raw[..4]);
                result.plays = u32::from(u16::from_le_bytes([raw[4], raw[5]]));
                anim = true;
            }
            b"ANIM" => return Err(bad()),
            b"ANMF" => {
                if !anim || size < 24 || size > options.max_frame_encoded_bytes {
                    return Err(invalid(
                        "WebP动画帧声明无效或超过单帧编码预算",
                        "Invalid WebP frame or per-frame encoded budget exceeded",
                    ));
                }
                let mut raw = [0; 16];
                read_at(reader, start, data, &mut raw, cancel)?;
                remember(&mut result.controls, data, &raw, &mut ledger, cancel)?;
                let mut frame = FrameSpec {
                    x: le24(&raw) * 2,
                    y: le24(&raw[3..]) * 2,
                    width: le24(&raw[6..]) + 1,
                    height: le24(&raw[9..]) + 1,
                    duration: le24(&raw[12..]),
                    blend: raw[15] & 2 == 0,
                    dispose: raw[15] & 1 != 0,
                    chunks: [None, None],
                    control_start: frame_control_start,
                    control_end: 0,
                };
                if raw[15] & !3 != 0
                    || frame.width > 16_384
                    || frame.height > 16_384
                    || u64::from(frame.x) + u64::from(frame.width) > u64::from(result.width)
                    || u64::from(frame.y) + u64::from(frame.height) > u64::from(result.height)
                {
                    return Err(bad());
                }
                let mut child_position = data + 16;
                let mut count = 0;
                while child_position < data + size {
                    if data + size - child_position < 8 {
                        return Err(bad());
                    }
                    let mut child = [0; 8];
                    read_at(reader, start, child_position, &mut child, cancel)?;
                    let child_size = u64::from(le32(&child[4..]));
                    let child_end = checked_chunk_end(child_position, child_size, data + size)?;
                    let kind: [u8; 4] = child[..4].try_into().unwrap();
                    remember(
                        &mut result.controls,
                        child_position,
                        &child,
                        &mut ledger,
                        cancel,
                    )?;
                    match &kind {
                        b"ALPH" | b"VP8 " | b"VP8L" => {
                            if count == frame.chunks.len() {
                                return Err(bad());
                            }
                            let mut chunk = Chunk {
                                kind,
                                offset: child_position + 8,
                                length: child_size,
                                prefix: [0; 64],
                                prefix_len: child_size.min(64) as usize,
                            };
                            read_at(
                                reader,
                                start,
                                chunk.offset,
                                &mut chunk.prefix[..chunk.prefix_len],
                                cancel,
                            )?;
                            check_codec(&chunk, frame.width, frame.height)?;
                            remember(
                                &mut result.controls,
                                chunk.offset,
                                &chunk.prefix[..chunk.prefix_len],
                                &mut ledger,
                                cancel,
                            )?;
                            frame.chunks[count] = Some(chunk);
                            count += 1;
                        }
                        b"VP8X" | b"ANIM" | b"ANMF" | b"ICCP" | b"EXIF" | b"XMP " => {
                            return Err(unsupported(
                                "WebP已知容器块不能作为帧扩展忽略",
                                "Known WebP container chunks cannot be ignored as frame extensions",
                            ));
                        }
                        // 宽容读取帧首或已知块之间的未知扩展；已知重建块顺序仍须成立。
                        // 不读取/哈希无像素作用的payload，header及padding仍冻结为控制指纹。
                        _ => {}
                    }
                    padding(
                        reader,
                        start,
                        child_position + 8,
                        child_size,
                        &mut result.controls,
                        &mut ledger,
                        cancel,
                    )?;
                    child_position = child_end;
                }
                let valid = match (&frame.chunks[0], &frame.chunks[1]) {
                    (Some(first), None) => matches!(&first.kind, b"VP8 " | b"VP8L"),
                    (Some(first), Some(second)) => {
                        first.kind == *b"ALPH" && second.kind == *b"VP8 "
                    }
                    _ => false,
                };
                if !valid || child_position != data + size {
                    return Err(bad());
                }
                frame.control_end = result.controls.len();
                ledger.push(&mut result.frames, frame, MAX_FRAMES, cancel)?;
            }
            b"ICCP" => {
                icc_count += 1;
                result.icc.encoded_bytes += size;
                let mut frozen = false;
                if icc_count > 1 {
                    result.icc.status = MetadataStatus::Malformed;
                    result.icc.decoded_bytes = None;
                    ledger.discard_profile(&mut result.profile);
                } else if size > MAX_ICC {
                    result.icc.status = MetadataStatus::TooLarge;
                    result.icc.decoded_bytes = Some(size);
                } else if size == 0 {
                    result.icc.status = MetadataStatus::Malformed;
                } else {
                    let mut profile = ledger.allocate_profile(size as usize, cancel)?;
                    read_at(reader, start, data, &mut profile, cancel)?;
                    result.icc.status = MetadataStatus::Present;
                    result.icc.decoded_bytes = Some(size);
                    // 指纹必须来自真正交给颜色管理器的同一份字节，不能另读一次来源。
                    remember(
                        &mut result.global_controls,
                        data,
                        &profile,
                        &mut ledger,
                        cancel,
                    )?;
                    frozen = true;
                    result.profile = Some(profile);
                }
                if !frozen {
                    remember_stream(
                        reader,
                        start,
                        data,
                        size,
                        &mut result.global_controls,
                        &mut ledger,
                        cancel,
                    )?;
                }
            }
            b"EXIF" if size != 0 => {
                return Err(unsupported(
                    "WebP动画暂不处理EXIF，请使用静态预览",
                    "Use a static preview for WebP animation with EXIF",
                ))
            }
            b"VP8 " | b"VP8L" | b"ALPH" => return Err(bad()),
            _ => remember_stream(
                reader,
                start,
                data,
                size,
                &mut result.global_controls,
                &mut ledger,
                cancel,
            )?,
        }
        // 合法ANMF由16B声明和带偶数padding的子块组成，块体必为偶数。
        if kind == *b"ANMF" {
            if size & 1 != 0 {
                return Err(bad());
            }
        } else {
            padding(
                reader,
                start,
                data,
                size,
                &mut result.global_controls,
                &mut ledger,
                cancel,
            )?;
        }
        position = end;
    }
    if position != length || !anim || result.frames.is_empty() {
        return Err(bad());
    }
    if flags & 0x20 != 0 && icc_count == 0 {
        result.icc.status = MetadataStatus::Malformed;
    }
    if result.directory_bytes() != ledger.directories {
        return Err(bad());
    }
    canvas_budget(
        &result,
        options,
        usize::try_from(ledger.profile).map_err(|_| allocation())?,
    )?;
    ledger.account(0)?;
    result.constructor_peak = ledger.peak;
    cancelled(cancel)?;
    Ok(result)
}

enum PieceData {
    Frozen(Vec<u8>),
    Source { offset: u64, length: u64 },
}
struct Piece {
    start: u64,
    data: PieceData,
}
struct FrameView<'a, R> {
    reader: &'a mut R,
    source_start: u64,
    pieces: Vec<Piece>,
    position: u64,
    length: u64,
    cancel: &'a AtomicBool,
}
impl<'a, R: Read + Seek> FrameView<'a, R> {
    fn new(
        reader: &'a mut R,
        source_start: u64,
        frame: FrameSpec,
        cancel: &'a AtomicBool,
    ) -> WebpResult<Self> {
        let alpha = frame.chunks[0].is_some_and(|chunk| chunk.kind != *b"VP8 ");
        let size = frame
            .chunks
            .iter()
            .flatten()
            .map(|chunk| 8 + chunk.length + (chunk.length & 1))
            .sum::<u64>();
        let mut header = bytes(30)?;
        header[..4].copy_from_slice(b"RIFF");
        header[4..8].copy_from_slice(&((22 + size) as u32).to_le_bytes());
        header[8..16].copy_from_slice(b"WEBPVP8X");
        header[16..20].copy_from_slice(&10u32.to_le_bytes());
        header[20] = if alpha { 0x10 } else { 0 };
        header[24..27].copy_from_slice(&(frame.width - 1).to_le_bytes()[..3]);
        header[27..30].copy_from_slice(&(frame.height - 1).to_le_bytes()[..3]);
        let mut view = Self {
            reader,
            source_start,
            pieces: Vec::new(),
            position: 0,
            length: 0,
            cancel,
        };
        view.frozen(header)?;
        for chunk in frame.chunks.iter().flatten() {
            let mut prefix = bytes(8 + chunk.prefix_len)?;
            prefix[..4].copy_from_slice(&chunk.kind);
            prefix[4..8].copy_from_slice(&(chunk.length as u32).to_le_bytes());
            prefix[8..].copy_from_slice(&chunk.prefix[..chunk.prefix_len]);
            view.frozen(prefix)?;
            let remaining = chunk.length - chunk.prefix_len as u64;
            if remaining != 0 {
                let start = view.length;
                push_bounded(
                    &mut view.pieces,
                    Piece {
                        start,
                        data: PieceData::Source {
                            offset: chunk.offset + chunk.prefix_len as u64,
                            length: remaining,
                        },
                    },
                    10,
                )?;
                view.length += remaining;
            }
            if chunk.length & 1 != 0 {
                view.frozen(vec![0])?;
            }
        }
        Ok(view)
    }
    fn frozen(&mut self, bytes: Vec<u8>) -> WebpResult<()> {
        let start = self.length;
        let length = bytes.len() as u64;
        push_bounded(
            &mut self.pieces,
            Piece {
                start,
                data: PieceData::Frozen(bytes),
            },
            10,
        )?;
        self.length += length;
        Ok(())
    }
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(io::Error::other(tr!(
                "WebP读取已取消",
                "WebP reading canceled"
            )))
        } else {
            Ok(())
        }
    }
}
impl<R: Read + Seek> Read for FrameView<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        if output.is_empty() || self.position == self.length {
            return Ok(0);
        }
        let index = self
            .pieces
            .partition_point(|piece| piece.start <= self.position)
            - 1;
        let piece = &self.pieces[index];
        let within = self.position - piece.start;
        let count = match &piece.data {
            PieceData::Frozen(bytes) => {
                let count = output.len().min(bytes.len() - within as usize);
                output[..count].copy_from_slice(&bytes[within as usize..within as usize + count]);
                count
            }
            PieceData::Source { offset, length } => {
                let absolute = self
                    .source_start
                    .checked_add(*offset)
                    .and_then(|n| n.checked_add(within))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
                let result = self.reader.seek(SeekFrom::Start(absolute));
                self.check()?;
                result?;
                let count = output.len().min((length - within) as usize).min(64 * 1024);
                let result = self.reader.read(&mut output[..count]);
                self.check()?;
                let count = result?;
                if count == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                count
            }
        };
        self.check()?;
        self.position += count as u64;
        Ok(count)
    }
}
impl<R: Read + Seek> Seek for FrameView<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let position = match position {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.length) + i128::from(n),
        };
        if position < 0 || position > i128::from(self.length) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}

fn frame_virtual_length(frame: FrameSpec) -> WebpResult<u64> {
    let mut length = 30u64;
    for chunk in frame.chunks.iter().flatten() {
        length = length
            .checked_add(8)
            .and_then(|n| n.checked_add(chunk.length))
            .and_then(|n| n.checked_add(chunk.length & 1))
            .ok_or_else(bad)?;
    }
    // 仅复制已知像素块；无未知块时等于ANMF块体+14B，否则更小。
    // 原ANMF块体仍先受单帧上限约束，虚拟RIFF不会超过该上限+14B。
    if length > MAX_FRAME_BYTES + 14 {
        return Err(bad());
    }
    Ok(length)
}

fn freeze_frame<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    frame: FrameSpec,
    base: u64,
    limit: u64,
    cancel: &AtomicBool,
) -> WebpResult<Vec<u8>> {
    cancelled(cancel)?;
    let length = frame_virtual_length(frame)?;
    working_set(&[base, FRAME_IO_RESERVE, length], limit)?;
    let result = bytes(usize::try_from(length).map_err(|_| bad())?);
    cancelled(cancel)?;
    let mut frozen = result?;
    working_set(&[base, FRAME_IO_RESERVE, frozen.capacity() as u64], limit)?;
    let mut view = FrameView::new(reader, start, frame, cancel)?;
    if view.length != length {
        return Err(bad());
    }
    read(&mut view, &mut frozen, cancel)?;
    drop(view);
    cancelled(cancel)?;
    Ok(frozen)
}

fn inspect_frame(
    frozen: &Vec<u8>,
    frame: FrameSpec,
    base: u64,
    limit: u64,
    cancel: &AtomicBool,
) -> WebpResult<(crate::image_decode::WebpWorkspace, u64)> {
    let external = working_set(&[base, FRAME_IO_RESERVE, frozen.capacity() as u64], limit)?;
    let result =
        crate::image_decode::inspect_webp_workspace_with_limit(frozen, external, limit, cancel);
    cancelled(cancel)?;
    let workspace = result.map_err(|error| WebpError::Invalid(error.to_string()))?;
    let native = u64::from(frame.width) * u64::from(frame.height) * 4;
    if workspace.dimensions != (frame.width, frame.height) || workspace.native_bytes != native {
        return Err(bad());
    }
    let decode = working_set(
        &[workspace.native_bytes, workspace.decode_extra_bytes],
        limit,
    )?;
    // 观察器最多同时保留一幅aux图（palette≤256像素，其它图≤F）。
    // 小帧也必须计固定代码表/栈余量，不能仅公告native+codec峰值。
    let observation = OBSERVER_SCRATCH
        .checked_add(native.max(1024))
        .ok_or_else(bad)?;
    let phase = workspace.constructor_bytes.max(decode).max(observation);
    let extra = working_set(&[FRAME_IO_RESERVE, frozen.capacity() as u64, phase], limit)?;
    working_set(&[base, extra], limit)?;
    Ok((workspace, extra))
}

fn inspect_frames<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    length: u64,
    scan: &Scan,
    options: WebpOptions,
    retained: usize,
    cancel: &AtomicBool,
) -> WebpResult<u64> {
    let base = source_base(
        scan,
        options,
        retained,
        u64::from(scan.width) * u64::from(scan.height) * 4,
    )?;
    verify(
        reader,
        start,
        length,
        scan.global_controls.iter().chain(scan.controls.iter()),
        cancel,
    )?;
    let mut maximum = 0;
    for &frame in &scan.frames {
        let frozen = freeze_frame(reader, start, frame, base, options.max_total_bytes, cancel)?;
        let (_, extra) = inspect_frame(&frozen, frame, base, options.max_total_bytes, cancel)?;
        maximum = maximum.max(extra);
        drop(frozen);
    }
    verify(
        reader,
        start,
        length,
        scan.global_controls.iter().chain(scan.controls.iter()),
        cancel,
    )?;
    Ok(maximum)
}

struct FrozenReader<'a> {
    bytes: Cursor<&'a [u8]>,
    cancel: &'a AtomicBool,
}
impl FrozenReader<'_> {
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(io::Error::other(tr!(
                "WebP读取已取消",
                "WebP reading canceled"
            )))
        } else {
            Ok(())
        }
    }
}
impl Read for FrozenReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let count = output.len().min(64 * 1024);
        let result = self.bytes.read(&mut output[..count]);
        self.check()?;
        result
    }
}
impl Seek for FrozenReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let result = self.bytes.seek(position);
        self.check()?;
        result
    }
}

fn decode_frame(
    frozen: &[u8],
    frame: FrameSpec,
    workspace: crate::image_decode::WebpWorkspace,
    external: u64,
    limit: u64,
    cancel: &AtomicBool,
) -> WebpResult<Vec<u8>> {
    cancelled(cancel)?;
    working_set(&[external, workspace.constructor_bytes], limit)?;
    let reader = FrozenReader {
        bytes: Cursor::new(frozen),
        cancel,
    };
    let mut decoder =
        image::codecs::webp::WebPDecoder::new(BufReader::with_capacity(64 * 1024, reader))?;
    if decoder.has_animation() || decoder.dimensions() != (frame.width, frame.height) {
        return Err(bad());
    }
    let alpha = match decoder.color_type() {
        image::ColorType::Rgba8 => true,
        image::ColorType::Rgb8 => false,
        _ => return Err(bad()),
    };
    let pixels = frame.width as usize * frame.height as usize;
    let output = usize::try_from(decoder.total_bytes()).map_err(|_| bad())?;
    if output != pixels * if alpha { 4 } else { 3 } {
        return Err(bad());
    }
    // facade沿用同一静态codec；默认set_limits只核尺寸，分配上限仍由本层预检负责。
    // 虚拟视图仅含像素子块，不调用ICC/EXIF/XMP或orientation的metadata getters。
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(frame.width);
    limits.max_image_height = Some(frame.height);
    decoder.set_limits(limits)?;
    let mut decoded = bytes(pixels * 4)?;
    working_set(
        &[
            external,
            decoded.capacity() as u64,
            workspace.decode_extra_bytes,
        ],
        limit,
    )?;
    // 库CPU解码不可强制中断；读操作以及调用前后均检查取消，不发布部分帧。
    decoder.read_image(&mut decoded[..output])?;
    cancelled(cancel)?;
    if alpha {
        for row in decoded.chunks_exact_mut(frame.width as usize * 4) {
            cancelled(cancel)?;
            for pixel in row.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
    } else {
        for index in (0..pixels).rev() {
            if index % frame.width as usize == 0 {
                cancelled(cancel)?;
            }
            let rgb = [
                decoded[index * 3],
                decoded[index * 3 + 1],
                decoded[index * 3 + 2],
            ];
            decoded[index * 4..index * 4 + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    Ok(decoded)
}
fn fill(
    canvas: &mut [u8],
    width: u32,
    frame: FrameSpec,
    color: [u8; 4],
    cancel: &AtomicBool,
) -> WebpResult<()> {
    for y in frame.y..frame.y + frame.height {
        cancelled(cancel)?;
        let start = (u64::from(y) * u64::from(width) + u64::from(frame.x)) as usize * 4;
        for pixel in canvas[start..start + frame.width as usize * 4].chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }
    Ok(())
}
fn composite(
    canvas: &mut [u8],
    width: u32,
    frame: FrameSpec,
    pixels: &[u8],
    cancel: &AtomicBool,
) -> WebpResult<()> {
    for y in 0..frame.height {
        cancelled(cancel)?;
        let from = y as usize * frame.width as usize * 4;
        let to = (u64::from(frame.y + y) * u64::from(width) + u64::from(frame.x)) as usize * 4;
        for (source, target) in pixels[from..from + frame.width as usize * 4]
            .chunks_exact(4)
            .zip(canvas[to..to + frame.width as usize * 4].chunks_exact_mut(4))
        {
            let alpha = u32::from(source[3]);
            if !frame.blend || alpha == 255 {
                target.copy_from_slice(source);
            } else if alpha != 0 {
                let destination = u32::from(target[3]);
                let remain = 255 - alpha;
                let denominator = alpha * 255 + destination * remain;
                for channel in 0..3 {
                    let numerator = u32::from(source[channel]) * alpha * 255
                        + u32::from(target[channel]) * destination * remain;
                    target[channel] = ((numerator + denominator / 2) / denominator) as u8;
                }
                target[3] = ((denominator + 127) / 255) as u8;
            }
        }
    }
    Ok(())
}

pub(crate) struct WebpSource<R> {
    reader: R,
    start: u64,
    length: u64,
    scan: Scan,
    options: WebpOptions,
    cancel: Arc<AtomicBool>,
    canvas: Vec<u8>,
    color: FrameColorManager,
    color_reserved: usize,
    max_warning_capacity: usize,
    outputs: Arc<OutputPool>,
    reserved: u64,
    generation: u64,
    cycle: u64,
    index: usize,
    previous: Option<FrameSpec>,
    failed: bool,
    ended: bool,
}
impl WebpSource<File> {
    pub(crate) fn open(
        path: &Path,
        options: WebpOptions,
        cancel: Arc<AtomicBool>,
    ) -> WebpResult<Self> {
        constructor_minimum(options, &cancel)?;
        let local = crate::image_collection::ensure_local_file(path);
        cancelled(&cancel)?;
        local.map_err(WebpError::Invalid)?;
        let file = File::open(path);
        cancelled(&cancel)?;
        Self::from_reader(file?, options, cancel)
    }
}
impl<R: Read + Seek> WebpSource<R> {
    pub(crate) fn from_reader(
        mut reader: R,
        options: WebpOptions,
        cancel: Arc<AtomicBool>,
    ) -> WebpResult<Self> {
        constructor_minimum(options, &cancel)?;
        let start = seek(&mut reader, SeekFrom::Current(0), &cancel)?;
        let end = seek(&mut reader, SeekFrom::End(0), &cancel)?;
        let length = end.checked_sub(start).ok_or_else(bad)?;
        let mut scan = scan(&mut reader, start, length, options, &cancel)?;
        let profile_reserve = scan
            .profile
            .as_ref()
            .map_or(Some(0), |profile| profile.capacity().checked_mul(3))
            .ok_or_else(allocation)?;
        canvas_budget(&scan, options, profile_reserve)?;
        scan.max_workspace = inspect_frames(
            &mut reader,
            start,
            length,
            &scan,
            options,
            profile_reserve,
            &cancel,
        )?;
        canvas_budget(&scan, options, profile_reserve)?;
        let color = FrameColorManager::new(scan.source_summary(), scan.profile.take(), &cancel)
            .map_err(|error| classify(WebpError::Invalid(error), &cancel))?;
        let max_warning_capacity = color_pool(
            color.retained_bytes(),
            color.warning_capacity(),
            0,
            profile_reserve,
            options.max_output_frames,
        )?;
        canvas_budget(&scan, options, profile_reserve)?;
        let mut canvas = bytes(scan.width as usize * scan.height as usize * 4)?;
        let reserved =
            canvas_budget_with_capacity(&scan, options, profile_reserve, canvas.capacity() as u64)?
                .max(scan.constructor_peak);
        for row in canvas.chunks_exact_mut(scan.width as usize * 4) {
            cancelled(&cancel)?;
            for pixel in row.chunks_exact_mut(4) {
                pixel.copy_from_slice(&scan.background);
            }
        }
        verify(
            &mut reader,
            start,
            length,
            scan.global_controls.iter().chain(scan.controls.iter()),
            &cancel,
        )?;
        Ok(Self {
            reader,
            start,
            length,
            scan,
            options,
            cancel,
            canvas,
            color,
            color_reserved: profile_reserve,
            max_warning_capacity,
            outputs: Arc::new(OutputPool {
                live: AtomicUsize::new(0),
                limit: options.max_output_frames,
            }),
            reserved,
            generation: 0,
            cycle: 0,
            index: 0,
            previous: None,
            failed: false,
            ended: false,
        })
    }
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.scan.width, self.scan.height)
    }
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.reserved
    }
    /// 帧codec及其压缩暂存已在返回前释放；暂停只保画布、目录和颜色上下文。
    /// 不计尚未分配的输出名额/下一帧工作区，泛型reader的输入内存由调用方另计。
    pub(crate) fn retained_bytes(&self) -> u64 {
        (self.canvas.capacity() as u64)
            .saturating_add(self.scan.directory_bytes())
            .saturating_add(self.scan.profile.as_ref().map_or(0, Vec::capacity) as u64)
            .saturating_add(self.color.retained_bytes() as u64)
            .saturating_add(64 * 1024)
    }
    pub(crate) fn num_plays(&self) -> u32 {
        self.scan.plays
    }
    pub(crate) fn restart(&mut self, cancel: Arc<AtomicBool>) -> WebpResult<()> {
        self.cancel = cancel;
        self.failed = true;
        verify(
            &mut self.reader,
            self.start,
            self.length,
            self.scan
                .global_controls
                .iter()
                .chain(self.scan.controls.iter()),
            &self.cancel,
        )
        .map_err(|error| classify(error, &self.cancel))?;
        self.reset_canvas()?;
        self.generation = self.generation.checked_add(1).ok_or_else(bad)?;
        self.cycle = 0;
        self.index = 0;
        self.previous = None;
        self.ended = false;
        self.failed = false;
        Ok(())
    }
    fn reset_canvas(&mut self) -> WebpResult<()> {
        for row in self.canvas.chunks_exact_mut(self.scan.width as usize * 4) {
            cancelled(&self.cancel)?;
            for pixel in row.chunks_exact_mut(4) {
                pixel.copy_from_slice(&self.scan.background);
            }
        }
        Ok(())
    }
    pub(crate) fn next_frame(&mut self) -> WebpResult<Option<WebpFrame>> {
        if self.failed {
            return Err(WebpError::RequiresRestart);
        }
        if let Err(error) = cancelled(&self.cancel) {
            self.failed = true;
            return Err(error);
        }
        if self.ended {
            return Ok(None);
        }
        if self.index == self.scan.frames.len()
            && self.scan.plays != 0
            && self.cycle + 1 >= u64::from(self.scan.plays)
        {
            self.ended = true;
            return Ok(None);
        }
        let lease = self.outputs.acquire()?;
        match self.next_inner(lease) {
            Ok(frame) => Ok(Some(frame)),
            Err(error) => {
                self.failed = true;
                Err(classify(error, &self.cancel))
            }
        }
    }
    fn next_inner(&mut self, lease: OutputLease) -> WebpResult<WebpFrame> {
        self.max_warning_capacity = color_pool(
            self.color.retained_bytes(),
            self.color.warning_capacity(),
            self.max_warning_capacity,
            self.color_reserved,
            self.options.max_output_frames,
        )?;
        if self.index == self.scan.frames.len() {
            verify(
                &mut self.reader,
                self.start,
                self.length,
                self.scan
                    .global_controls
                    .iter()
                    .chain(self.scan.controls.iter()),
                &self.cancel,
            )?;
            self.reset_canvas()?;
            self.cycle = self.cycle.checked_add(1).ok_or_else(bad)?;
            self.index = 0;
            self.previous = None;
        }
        let frame = self.scan.frames[self.index];
        let controls = &self.scan.controls[frame.control_start..frame.control_end];
        verify(
            &mut self.reader,
            self.start,
            self.length,
            self.scan.global_controls.iter().chain(controls.iter()),
            &self.cancel,
        )?;
        let base = source_base(
            &self.scan,
            self.options,
            self.color_reserved,
            self.canvas.capacity() as u64,
        )?;
        let frozen = freeze_frame(
            &mut self.reader,
            self.start,
            frame,
            base,
            self.reserved,
            &self.cancel,
        )?;
        let (workspace, _) = inspect_frame(&frozen, frame, base, self.reserved, &self.cancel)?;
        let external = working_set(
            &[base, FRAME_IO_RESERVE, frozen.capacity() as u64],
            self.reserved,
        )?;
        let decoded = decode_frame(
            &frozen,
            frame,
            workspace,
            external,
            self.reserved,
            &self.cancel,
        )?;
        drop(frozen);
        working_set(
            &[base, decoded.capacity() as u64, FRAME_IO_RESERVE],
            self.reserved,
        )?;
        verify(
            &mut self.reader,
            self.start,
            self.length,
            self.scan.global_controls.iter().chain(controls.iter()),
            &self.cancel,
        )?;
        if let Some(previous) = self.previous.filter(|previous| previous.dispose) {
            fill(
                &mut self.canvas,
                self.scan.width,
                previous,
                self.scan.background,
                &self.cancel,
            )?;
        }
        composite(
            &mut self.canvas,
            self.scan.width,
            frame,
            &decoded,
            &self.cancel,
        )?;
        drop(decoded);
        let managed = self
            .color
            .render(
                (self.scan.width, self.scan.height),
                &self.canvas,
                &self.cancel,
            )
            .map_err(WebpError::Invalid)?;
        cancelled(&self.cancel)?;
        self.max_warning_capacity = color_pool(
            self.color.retained_bytes(),
            self.color.warning_capacity(),
            self.max_warning_capacity
                .max(managed.warning.as_ref().map_or(0, String::capacity)),
            self.color_reserved,
            self.options.max_output_frames,
        )?;
        // 名额按完整画布4P公告；超额capacity不能由尚未用到的其它名额垫付。
        let output_bytes = u64::from(self.scan.width) * u64::from(self.scan.height) * 4;
        if managed.image.bgra.capacity() as u64 > output_bytes {
            return Err(invalid(
                "WebP交付缓冲容量超过租约预算",
                "WebP output capacity exceeds its frame-slot budget",
            ));
        }
        self.previous = Some(frame);
        let index = self.index as u64;
        self.index += 1;
        let delay = if frame.duration == 0 {
            self.options.zero_delay
        } else {
            Duration::from_millis(u64::from(frame.duration))
        };
        Ok(WebpFrame {
            image: managed.image,
            metadata_source: managed.metadata_source,
            warning: managed.warning,
            _lease: lease,
            generation: self.generation,
            cycle: self.cycle,
            index,
            timing: WebpTiming {
                raw_delay_ms: frame.duration,
                delay,
            },
        })
    }
}

#[cfg(test)]
#[path = "image_webp_tests.rs"]
mod tests;
