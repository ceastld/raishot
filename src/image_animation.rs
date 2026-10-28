//! 有界GIF来源：同一读入句柄顺序合成，返回帧保留租约，播放层按显式时序推进。
use crate::capture::Image;
use gif::{ColorOutput, DecodeOptions, Decoder, DisposalMethod, MemoryLimit, Repeat};
use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    num::NonZeroU64,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const DECODER_BLOCK_BYTES: u64 = 512 * 1024;
// ICC/XMP各保留至多2B容量；一类增长时另有至多B的旧块短暂存活。
// 另留LZW及读入缓冲，呈现转换像素由画布预算单独计算。
const DECODER_RESERVED_BYTES: u64 = 5 * DECODER_BLOCK_BYTES + 256 * 1024;
const DECODER_RETAINED_BYTES: u64 = 4 * DECODER_BLOCK_BYTES + 256 * 1024;
const PRESENTATION_RESERVE_FRAMES: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundMode {
    /// 初始画布和Background处置都使用透明像素。
    TransparentCanvas,
    /// 使用逻辑屏的全局背景色；没有有效全局背景时明确拒绝。
    GlobalPaletteColor,
}
// 全局背景与透明索引跳写是独立语义；两种合成模式显式保留，不改静态解码。
// 规则依据：https://www.w3.org/Graphics/GIF/spec-gif89a.txt（逻辑屏、图形控制扩展）。

#[derive(Clone, Copy, Debug)]
pub(crate) struct GifOptions {
    pub background: BackgroundMode,
    pub max_canvas_pixels: u64,
    /// 本来源的像素/解码工作集与呈现预留，不包含调用方已有输入或GPU缓存。
    pub max_total_bytes: u64,
    pub max_input_bytes: u64,
    pub max_output_frames: usize,
    /// 普通零延时帧的产品策略；UserInput零延时仍无限等待输入。
    pub zero_delay: Duration,
}

impl Default for GifOptions {
    fn default() -> Self {
        Self {
            background: BackgroundMode::TransparentCanvas,
            max_canvas_pixels: 8_000_000,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_input_bytes: MAX_TOTAL_BYTES,
            max_output_frames: 4,
            zero_delay: Duration::from_millis(100),
        }
    }
}

#[derive(Debug)]
pub(crate) enum AnimationError {
    Cancelled,
    RequiresRestart,
    Backpressure,
    Invalid(String),
    Io(io::Error),
    Decode(gif::DecodingError),
}

impl fmt::Display for AnimationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => {
                formatter.write_str(tr!("动画读取已取消", "Animation reading canceled"))
            }
            Self::RequiresRestart => formatter.write_str(tr!(
                "动画来源需要重新开始",
                "Animation source needs a restart"
            )),
            Self::Backpressure => formatter.write_str(tr!(
                "请先释放旧动画帧",
                "Release old animation frames first"
            )),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Decode(error) => write!(formatter, "{error}"),
        }
    }
}
impl std::error::Error for AnimationError {}
impl From<io::Error> for AnimationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<gif::DecodingError> for AnimationError {
    fn from(error: gif::DecodingError) -> Self {
        Self::Decode(error)
    }
}
type AnimationResult<T> = std::result::Result<T, AnimationError>;

fn invalid(zh: &'static str, en: &'static str) -> AnimationError {
    AnimationError::Invalid(crate::i18n::tr(zh, en).into())
}

fn cancelled(cancel: &AtomicBool) -> AnimationResult<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(AnimationError::Cancelled)
    } else {
        Ok(())
    }
}

/// 只持有当前流的固定字节范围；文件之后增长也不会扩大本次读入范围。
struct BoundedRead<R> {
    inner: R,
    remaining: u64,
    cancel: Arc<AtomicBool>,
}

/// 来源独立于解码器持有，重建头信息失败也能换token重启同一句柄。
struct SharedRead<R>(Arc<Mutex<R>>);
impl<R: Read> Read for SharedRead<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other(tr!("GIF来源锁不可用", "GIF source lock unavailable")))?
            .read(output)
    }
}
impl<R: Read> Read for BoundedRead<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.cancel.load(Ordering::Relaxed) {
            // Interrupted可能被read_exact自动重试，取消用不可重试的错误。
            return Err(io::Error::other(tr!(
                "动画读取已取消",
                "Animation reading canceled"
            )));
        }
        let limit = self.remaining.min(output.len() as u64) as usize;
        if limit == 0 {
            return Ok(0);
        }
        let count = self.inner.read(&mut output[..limit])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameTiming {
    pub raw_delay_centis: u16,
    pub needs_user_input: bool,
    zero_delay: Duration,
}
impl FrameTiming {
    pub(crate) fn timeout(self) -> Option<Duration> {
        if self.raw_delay_centis > 0 {
            Some(Duration::from_millis(u64::from(self.raw_delay_centis) * 10))
        } else if self.needs_user_input {
            None
        } else {
            Some(self.zero_delay)
        }
    }

    pub(crate) fn ready(self, elapsed: Duration, user_input: bool) -> bool {
        (self.needs_user_input && user_input)
            || self.timeout().is_some_and(|timeout| elapsed >= timeout)
    }
}

struct OutputPool {
    live: AtomicUsize,
    limit: usize,
}
struct OutputLease(Arc<OutputPool>);
impl OutputPool {
    fn acquire(self: &Arc<Self>) -> AnimationResult<OutputLease> {
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.limit).then_some(live + 1)
            })
            .map_err(|_| AnimationError::Backpressure)?;
        Ok(OutputLease(self.clone()))
    }
}
impl Drop for OutputLease {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::Release);
    }
}

/// 帧不可拆出像素所有权；持有帧或其Arc时一直占用交付预算。
pub(crate) struct GifFrame {
    image: Image,
    _lease: OutputLease,
    pub generation: u64,
    pub cycle: u64,
    pub index: u64,
    pub timing: FrameTiming,
}
impl GifFrame {
    pub(crate) fn image(&self) -> &Image {
        &self.image
    }
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.image.bgra.capacity() as u64
    }
}

#[derive(Clone, Copy)]
struct FrameRect {
    left: usize,
    top: usize,
    width: usize,
    height: usize,
}
#[derive(Clone, Copy)]
struct FrameInfo {
    rect: FrameRect,
    disposal: DisposalMethod,
    transparent: Option<u8>,
    delay: u16,
    needs_user_input: bool,
}
struct PendingDisposal {
    rect: FrameRect,
    method: DisposalMethod,
}

pub(crate) struct GifSource<R: Read + Seek> {
    reader: Arc<Mutex<R>>,
    decoder: Option<Decoder<BoundedRead<SharedRead<R>>>>,
    options: GifOptions,
    cancel: Arc<AtomicBool>,
    source_start: u64,
    source_length: u64,
    width: u32,
    height: u32,
    reserved_bytes: u64,
    background: [u8; 4],
    canvas: Vec<u8>,
    indices: Vec<u8>,
    restore: Vec<u8>,
    pending: Option<PendingDisposal>,
    outputs: Arc<OutputPool>,
    generation: u64,
    cycle: u64,
    index: u64,
    ended: bool,
    failed: bool,
}

fn options_valid(options: GifOptions) -> AnimationResult<()> {
    if options.max_canvas_pixels == 0
        || options.max_canvas_pixels > 32_000_000
        || options.max_total_bytes == 0
        || options.max_total_bytes > MAX_TOTAL_BYTES
        || options.max_input_bytes == 0
        || options.max_input_bytes > MAX_TOTAL_BYTES
        || !(1..=4).contains(&options.max_output_frames)
        || options.zero_delay.is_zero()
    {
        return Err(invalid(
            "GIF预算或零延时策略无效",
            "Invalid GIF budget or zero-delay policy",
        ));
    }
    Ok(())
}

fn canvas_budget(width: u32, height: u32, options: GifOptions) -> AnimationResult<u64> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > 65_535
        || height > 65_535
        || pixels > options.max_canvas_pixels
    {
        return Err(invalid(
            "GIF画布尺寸超过动画预算",
            "GIF canvas exceeds the animation budget",
        ));
    }
    // 合成画布、最坏Previous区域、索引块、活交付帧、呈现转换预留及解码器元数据。
    let bytes = pixels * 4 * (2 + options.max_output_frames as u64 + PRESENTATION_RESERVE_FRAMES)
        + pixels
        + DECODER_RESERVED_BYTES;
    if bytes > options.max_total_bytes {
        return Err(invalid(
            "GIF总缓冲超过动画预算",
            "GIF buffers exceed the animation budget",
        ));
    }
    Ok(bytes)
}

fn check_capacity(capacity: usize, slot_bytes: usize) -> AnimationResult<()> {
    if capacity > slot_bytes {
        return Err(invalid(
            "GIF总缓冲超过动画预算",
            "GIF buffers exceed the animation budget",
        ));
    }
    Ok(())
}

fn allocate(length: usize, cancel: &AtomicBool) -> AnimationResult<Vec<u8>> {
    cancelled(cancel)?;
    let mut bytes = Vec::new();
    let reserved = bytes.try_reserve_exact(length);
    cancelled(cancel)?;
    reserved.map_err(|_| invalid("GIF缓冲分配失败", "GIF buffer allocation failed"))?;
    check_capacity(bytes.capacity(), length)?;
    bytes.resize(length, 0);
    cancelled(cancel)?;
    Ok(bytes)
}

fn make_decoder<R: Read>(
    reader: R,
    length: u64,
    cancel: Arc<AtomicBool>,
) -> AnimationResult<Decoder<BoundedRead<R>>> {
    let mut options = DecodeOptions::new();
    options.set_color_output(ColorOutput::Indexed);
    options.set_memory_limit(MemoryLimit::Bytes(
        NonZeroU64::new(DECODER_BLOCK_BYTES).unwrap(),
    ));
    options.check_frame_consistency(true);
    options.check_lzw_end_code(true);
    Ok(options.read_info(BoundedRead {
        inner: reader,
        remaining: length,
        cancel,
    })?)
}

fn background<R: Read>(decoder: &Decoder<R>, mode: BackgroundMode) -> AnimationResult<[u8; 4]> {
    if mode == BackgroundMode::TransparentCanvas {
        return Ok([0; 4]);
    }
    let index = decoder.bg_color().ok_or_else(|| {
        invalid(
            "GIF没有有效全局背景色",
            "GIF has no valid global background color",
        )
    })?;
    let palette = decoder
        .global_palette()
        .ok_or_else(|| invalid("GIF没有全局调色板", "GIF has no global palette"))?;
    let offset = index * 3;
    let rgb = palette
        .get(offset..offset + 3)
        .ok_or_else(|| invalid("GIF背景色索引无效", "Invalid GIF background index"))?;
    Ok([rgb[2], rgb[1], rgb[0], 255])
}

impl GifSource<File> {
    pub(crate) fn open(
        path: &Path,
        options: GifOptions,
        cancel: Arc<AtomicBool>,
    ) -> AnimationResult<Self> {
        crate::image_collection::ensure_local_file(path).map_err(AnimationError::Invalid)?;
        Self::from_reader(File::open(path)?, options, cancel)
    }
}

impl<R: Read + Seek> GifSource<R> {
    pub(crate) fn from_reader(
        mut reader: R,
        options: GifOptions,
        cancel: Arc<AtomicBool>,
    ) -> AnimationResult<Self> {
        options_valid(options)?;
        cancelled(&cancel)?;
        // 首帧前的应用扩展在尺寸可用前已被解码器消费，先守住固定构造峰值。
        if options.max_total_bytes < DECODER_RESERVED_BYTES {
            return Err(invalid(
                "GIF总缓冲超过动画预算",
                "GIF buffers exceed the animation budget",
            ));
        }
        let source_start = reader.stream_position()?;
        let source_length = reader
            .seek(SeekFrom::End(0))?
            .checked_sub(source_start)
            .ok_or_else(|| invalid("GIF读入范围无效", "Invalid GIF input range"))?;
        if source_length > options.max_input_bytes {
            return Err(invalid(
                "GIF来源超过读入预算",
                "GIF source exceeds the input budget",
            ));
        }
        reader.seek(SeekFrom::Start(source_start))?;
        let reader = Arc::new(Mutex::new(reader));
        let decoder = make_decoder(SharedRead(reader.clone()), source_length, cancel.clone())
            .map_err(|error| {
                if cancel.load(Ordering::Relaxed) {
                    AnimationError::Cancelled
                } else {
                    error
                }
            })?;
        let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
        let reserved_bytes = canvas_budget(width, height, options)?;
        let background = background(&decoder, options.background)?;
        let canvas = allocate(width as usize * height as usize * 4, &cancel)?;
        let mut source = Self {
            reader,
            decoder: Some(decoder),
            options,
            cancel,
            source_start,
            source_length,
            width,
            height,
            reserved_bytes,
            background,
            canvas,
            indices: Vec::new(),
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
        };
        source.clear_canvas()?;
        Ok(source)
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
    /// 暂停时仍拥有的来源缓冲；不计尚未分配的输出名额或呈现预留。
    /// 不包括泛型reader自身的输入内存；查看器的File来源没有整份输入Vec。
    pub(crate) fn retained_bytes(&self) -> u64 {
        (self.canvas.capacity() as u64)
            .saturating_add(self.indices.capacity() as u64)
            .saturating_add(self.restore.capacity() as u64)
            .saturating_add(if self.decoder.is_some() {
                DECODER_RETAINED_BYTES
            } else {
                0
            })
    }
    pub(crate) fn repeat(&self) -> Option<Repeat> {
        self.decoder.as_ref().map(|decoder| decoder.repeat())
    }

    fn clear_canvas(&mut self) -> AnimationResult<()> {
        for row in self.canvas.chunks_exact_mut(self.width as usize * 4) {
            cancelled(&self.cancel)?;
            for pixel in row.chunks_exact_mut(4) {
                pixel.copy_from_slice(&self.background);
            }
        }
        Ok(())
    }

    fn rewind(&mut self) -> AnimationResult<()> {
        // 丢弃旧预读缓冲后seek；主来源仍由self持有，不随解码器失败而丢失。
        self.decoder = None;
        {
            let mut reader = self
                .reader
                .lock()
                .map_err(|_| invalid("GIF来源锁不可用", "GIF source lock unavailable"))?;
            let end = reader.seek(SeekFrom::End(0))?;
            if end.checked_sub(self.source_start) != Some(self.source_length) {
                return Err(invalid("GIF来源长度已改变", "GIF source length changed"));
            }
            reader.seek(SeekFrom::Start(self.source_start))?;
        }
        let decoder = make_decoder(
            SharedRead(self.reader.clone()),
            self.source_length,
            self.cancel.clone(),
        )?;
        if (u32::from(decoder.width()), u32::from(decoder.height())) != (self.width, self.height) {
            return Err(invalid(
                "GIF来源尺寸已改变",
                "GIF source dimensions changed",
            ));
        }
        self.background = background(&decoder, self.options.background)?;
        self.decoder = Some(decoder);
        self.pending = None;
        self.restore.clear();
        self.indices.clear();
        self.index = 0;
        self.clear_canvas()
    }

    /// 旧帧租约在重启后继续计数；取消后换新token，从头重建合成状态。
    pub(crate) fn restart(&mut self, cancel: Arc<AtomicBool>) -> AnimationResult<()> {
        self.cancel = cancel;
        self.failed = true;
        cancelled(&self.cancel)?;
        if let Err(error) = self.rewind() {
            return Err(if self.cancel.load(Ordering::Relaxed) {
                AnimationError::Cancelled
            } else {
                error
            });
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("GIF代次溢出", "GIF generation overflow"))?;
        self.cycle = 0;
        self.ended = false;
        self.failed = false;
        Ok(())
    }

    /// 顺序取下一帧或手动单步；自动播放调用方必须尊重上一帧的FrameTiming。
    pub(crate) fn next_frame(&mut self) -> AnimationResult<Option<GifFrame>> {
        if self.failed {
            return Err(AnimationError::RequiresRestart);
        }
        if let Err(error) = cancelled(&self.cancel) {
            self.failed = true;
            return Err(error);
        }
        if self.ended {
            return Ok(None);
        }
        let lease = self.outputs.acquire()?;
        let result = self.next_frame_inner(lease);
        if result.is_err() {
            self.failed = true;
            if self.cancel.load(Ordering::Relaxed) {
                return Err(AnimationError::Cancelled);
            }
        }
        result
    }

    fn next_frame_inner(&mut self, lease: OutputLease) -> AnimationResult<Option<GifFrame>> {
        let canvas_bytes = self.width as usize * self.height as usize * 4;
        check_capacity(self.canvas.capacity(), canvas_bytes)?;
        check_capacity(self.indices.capacity(), canvas_bytes / 4)?;
        check_capacity(self.restore.capacity(), canvas_bytes)?;
        let info = loop {
            cancelled(&self.cancel)?;
            let decoder = self
                .decoder
                .as_mut()
                .ok_or(AnimationError::RequiresRestart)?;
            let info = decoder.next_frame_info()?.map(|frame| FrameInfo {
                rect: FrameRect {
                    left: usize::from(frame.left),
                    top: usize::from(frame.top),
                    width: usize::from(frame.width),
                    height: usize::from(frame.height),
                },
                disposal: frame.dispose,
                transparent: frame.transparent,
                delay: frame.delay,
                needs_user_input: frame.needs_user_input,
            });
            if let Some(info) = info {
                break info;
            }
            let repeat = self.repeat().ok_or(AnimationError::RequiresRestart)?;
            if self.index == 0
                || matches!(repeat, Repeat::Finite(count) if self.cycle >= u64::from(count))
            {
                self.ended = true;
                return Ok(None);
            }
            self.cycle = self
                .cycle
                .checked_add(1)
                .ok_or_else(|| invalid("GIF循环次数溢出", "GIF loop count overflow"))?;
            self.rewind()?;
        };
        let rect = info.rect;
        if rect.width == 0
            || rect.height == 0
            || rect.left + rect.width > self.width as usize
            || rect.top + rect.height > self.height as usize
        {
            return Err(invalid(
                "GIF局部帧超出画布",
                "GIF frame lies outside the canvas",
            ));
        }
        let count = rect.width * rect.height;
        if self.indices.capacity() < count {
            // 旧索引已消费；先释放再增长，避免realloc瞬时同时占两块整帧索引。
            self.indices = Vec::new();
            self.indices = allocate(count, &self.cancel)?;
        }
        self.indices.resize(count, 0);
        let decoder = self
            .decoder
            .as_mut()
            .ok_or(AnimationError::RequiresRestart)?;
        decoder.read_into_buffer(&mut self.indices)?;
        let palette = decoder.palette()?.to_vec();
        if palette.is_empty() || palette.len() > 768 || palette.len() % 3 != 0 {
            return Err(invalid("GIF调色板无效", "Invalid GIF palette"));
        }
        self.apply_disposal()?;
        self.restore.clear();
        if info.disposal == DisposalMethod::Previous {
            if self.restore.capacity() < count * 4 {
                self.restore = Vec::new();
            }
            let reserved = self.restore.try_reserve_exact(count * 4);
            cancelled(&self.cancel)?;
            reserved.map_err(|_| invalid("GIF恢复区分配失败", "GIF restore allocation failed"))?;
            check_capacity(self.restore.capacity(), canvas_bytes)?;
            for y in 0..rect.height {
                cancelled(&self.cancel)?;
                let start = ((rect.top + y) * self.width as usize + rect.left) * 4;
                self.restore
                    .extend_from_slice(&self.canvas[start..start + rect.width * 4]);
            }
        }
        for y in 0..rect.height {
            cancelled(&self.cancel)?;
            for x in 0..rect.width {
                let index = self.indices[y * rect.width + x];
                if Some(index) == info.transparent {
                    continue;
                }
                let color = usize::from(index) * 3;
                let rgb = palette.get(color..color + 3).ok_or_else(|| {
                    invalid(
                        "GIF像素索引超出调色板",
                        "GIF pixel index exceeds the palette",
                    )
                })?;
                let target = ((rect.top + y) * self.width as usize + rect.left + x) * 4;
                self.canvas[target..target + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
            }
        }
        self.pending = Some(PendingDisposal {
            rect,
            method: info.disposal,
        });
        let mut bgra = Vec::new();
        let reserved = bgra.try_reserve_exact(self.canvas.len());
        cancelled(&self.cancel)?;
        reserved.map_err(|_| invalid("GIF交付帧分配失败", "GIF output allocation failed"))?;
        check_capacity(bgra.capacity(), canvas_bytes)?;
        for row in self.canvas.chunks_exact(self.width as usize * 4) {
            cancelled(&self.cancel)?;
            bgra.extend_from_slice(row);
        }
        let index = self.index;
        self.index = self
            .index
            .checked_add(1)
            .ok_or_else(|| invalid("GIF帧号溢出", "GIF frame index overflow"))?;
        Ok(Some(GifFrame {
            image: Image {
                width: self.width,
                height: self.height,
                bgra,
                readback_ms: 0.,
            },
            _lease: lease,
            generation: self.generation,
            cycle: self.cycle,
            index,
            timing: FrameTiming {
                raw_delay_centis: info.delay,
                needs_user_input: info.needs_user_input,
                zero_delay: self.options.zero_delay,
            },
        }))
    }

    fn apply_disposal(&mut self) -> AnimationResult<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let rect = pending.rect;
        if !matches!(
            pending.method,
            DisposalMethod::Background | DisposalMethod::Previous
        ) {
            return Ok(());
        }
        for y in 0..rect.height {
            cancelled(&self.cancel)?;
            let start = ((rect.top + y) * self.width as usize + rect.left) * 4;
            let row = &mut self.canvas[start..start + rect.width * 4];
            if pending.method == DisposalMethod::Previous {
                let saved = y * rect.width * 4;
                row.copy_from_slice(&self.restore[saved..saved + rect.width * 4]);
            } else {
                for pixel in row.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&self.background);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{borrow::Cow, io::Cursor};

    const PALETTE: [u8; 18] = [
        17, 23, 31, 255, 0, 0, 0, 0, 255, 0, 255, 0, 255, 255, 0, 99, 11, 77,
    ];

    fn frame(
        left: u16,
        top: u16,
        width: u16,
        height: u16,
        indices: &[u8],
        dispose: DisposalMethod,
    ) -> gif::Frame<'static> {
        gif::Frame {
            left,
            top,
            width,
            height,
            dispose,
            delay: 3,
            buffer: Cow::Owned(indices.to_vec()),
            ..Default::default()
        }
    }

    fn encode(
        width: u16,
        height: u16,
        repeat: Option<Repeat>,
        frames: Vec<gif::Frame<'static>>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, width, height, &PALETTE).unwrap();
            if let Some(repeat) = repeat {
                encoder.set_repeat(repeat).unwrap();
            }
            for frame in frames {
                encoder.write_frame(&frame).unwrap();
            }
        }
        bytes
    }

    fn append_applications(bytes: &mut Vec<u8>, blocks: usize) {
        for (identifier, block_length, value) in
            [(b"ICCRGBG1012", 255u8, 17u8), (b"XMP DataXMP", 254u8, 23u8)]
        {
            bytes.extend_from_slice(&[0x21, 0xff, 11]);
            bytes.extend_from_slice(identifier);
            for _ in 0..blocks {
                bytes.push(block_length);
                bytes.resize(bytes.len() + usize::from(block_length), value);
            }
            bytes.push(0);
        }
    }

    fn metadata_fixture(leading_blocks: usize, following_blocks: usize) -> Vec<u8> {
        let first = encode(
            1,
            1,
            None,
            vec![frame(0, 0, 1, 1, &[1], DisposalMethod::Keep)],
        );
        let second = encode(
            1,
            1,
            None,
            vec![frame(0, 0, 1, 1, &[2], DisposalMethod::Keep)],
        );
        let header = 13 + 3 * (1usize << (usize::from(first[10] & 7) + 1));
        let mut bytes = first[..header].to_vec();
        append_applications(&mut bytes, leading_blocks);
        bytes.extend_from_slice(&first[header..first.len() - 1]);
        if following_blocks != 0 {
            append_applications(&mut bytes, following_blocks);
        }
        bytes.extend_from_slice(&second[header..second.len() - 1]);
        bytes.push(0x3b);
        bytes
    }

    fn token() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
    fn source(bytes: Vec<u8>, background: BackgroundMode) -> GifSource<Cursor<Vec<u8>>> {
        GifSource::from_reader(
            Cursor::new(bytes),
            GifOptions {
                background,
                ..Default::default()
            },
            token(),
        )
        .unwrap()
    }
    fn color(index: u8) -> [u8; 4] {
        let offset = usize::from(index) * 3;
        [
            PALETTE[offset + 2],
            PALETTE[offset + 1],
            PALETTE[offset],
            255,
        ]
    }
    fn colors(indices: &[u8], background: BackgroundMode) -> Vec<u8> {
        indices
            .iter()
            .flat_map(|index| {
                if *index == 0 && background == BackgroundMode::TransparentCanvas {
                    [0; 4]
                } else {
                    color(*index)
                }
            })
            .collect()
    }

    #[test]
    fn image_animation_local_disposal_and_previous_capture_follow_frame_order() {
        for mode in [
            BackgroundMode::GlobalPaletteColor,
            BackgroundMode::TransparentCanvas,
        ] {
            let mut previous = frame(0, 0, 2, 1, &[3, 5], DisposalMethod::Previous);
            previous.transparent = Some(5);
            let bytes = encode(
                3,
                2,
                None,
                vec![
                    frame(0, 0, 3, 2, &[1; 6], DisposalMethod::Keep),
                    frame(0, 0, 2, 1, &[2, 2], DisposalMethod::Background),
                    previous,
                    frame(2, 1, 1, 1, &[4], DisposalMethod::Keep),
                ],
            );
            let mut source = source(bytes, mode);
            for (index, expected) in [
                [1, 1, 1, 1, 1, 1],
                [2, 2, 1, 1, 1, 1],
                [3, 0, 1, 1, 1, 1],
                [0, 0, 1, 1, 1, 4],
            ]
            .iter()
            .enumerate()
            {
                let output = source.next_frame().unwrap().unwrap();
                assert_eq!(output.index, index as u64);
                assert_eq!(output.image().bgra, colors(expected, mode));
            }
            assert!(source.next_frame().unwrap().is_none());
        }
    }

    #[test]
    fn image_animation_initial_background_local_palette_and_eof_are_explicit() {
        for mode in [
            BackgroundMode::GlobalPaletteColor,
            BackgroundMode::TransparentCanvas,
        ] {
            let mut local = frame(1, 0, 1, 1, &[0], DisposalMethod::Background);
            local.palette = Some(vec![41, 101, 211, 0, 0, 0]);
            let bytes = encode(
                3,
                1,
                None,
                vec![local, frame(2, 0, 1, 1, &[4], DisposalMethod::Background)],
            );
            let mut source = source(bytes, mode);
            {
                let first = source.next_frame().unwrap().unwrap();
                let mut expected = colors(&[0, 0, 0], mode);
                expected[4..8].copy_from_slice(&[211, 101, 41, 255]);
                assert_eq!(first.image().bgra, expected);
            }
            let expected = colors(&[0, 0, 4], mode);
            {
                let second = source.next_frame().unwrap().unwrap();
                assert_eq!(second.image().bgra, expected);
            }
            assert!(source.next_frame().unwrap().is_none());
            // EOF不执行最后一帧的Background，有限动画保留最后所见画面。
            assert_eq!(source.canvas, expected);
        }
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 1, 1, &[]).unwrap();
            let mut local = frame(0, 0, 1, 1, &[0], DisposalMethod::Keep);
            local.palette = Some(vec![255, 0, 0, 0, 0, 0]);
            encoder.write_frame(&local).unwrap();
        }
        // 空调色板仍被编码器补成两项黑色GCT；同时移除表和标志才是无GCT来源。
        assert_eq!(&bytes[..6], b"GIF89a");
        assert_ne!(bytes[10] & 0x80, 0);
        let global_palette_bytes = 3 * (2usize << (bytes[10] & 0x07));
        bytes.drain(13..13 + global_palette_bytes);
        bytes[10] &= !0x80;
        let decoder = gif::DecodeOptions::new()
            .read_info(Cursor::new(bytes.as_slice()))
            .unwrap();
        assert!(decoder.global_palette().is_none());
        drop(decoder);
        assert!(GifSource::from_reader(
            Cursor::new(bytes.clone()),
            GifOptions {
                background: BackgroundMode::GlobalPaletteColor,
                ..Default::default()
            },
            token()
        )
        .is_err());
        let mut transparent = source(bytes, BackgroundMode::TransparentCanvas);
        assert_eq!(
            transparent.next_frame().unwrap().unwrap().image().bgra,
            [0, 0, 255, 255]
        );
    }

    fn loop_fixture(repeat: Option<Repeat>) -> Vec<u8> {
        encode(
            2,
            1,
            repeat,
            vec![
                frame(0, 0, 1, 1, &[1], DisposalMethod::Keep),
                frame(1, 0, 1, 1, &[2], DisposalMethod::Keep),
            ],
        )
    }

    #[test]
    fn image_animation_finite_infinite_and_empty_loops_reset_canvas() {
        for (repeat, count) in [
            (None, 2),
            (Some(Repeat::Finite(1)), 4),
            (Some(Repeat::Infinite), 6),
        ] {
            let mut source = source(loop_fixture(repeat), BackgroundMode::GlobalPaletteColor);
            for index in 0..count {
                let output = source.next_frame().unwrap().unwrap();
                assert_eq!(output.cycle, index / 2);
                assert_eq!(output.index, index % 2);
                assert_eq!(
                    output.image().bgra,
                    colors(
                        if index % 2 == 0 { &[1, 0] } else { &[1, 2] },
                        BackgroundMode::GlobalPaletteColor
                    )
                );
            }
            if repeat != Some(Repeat::Infinite) {
                assert!(source.next_frame().unwrap().is_none());
            }
        }
        let bytes = encode(1, 1, Some(Repeat::Infinite), Vec::new());
        assert!(source(bytes, BackgroundMode::TransparentCanvas)
            .next_frame()
            .unwrap()
            .is_none());
    }

    #[test]
    fn image_animation_zero_delay_and_user_input_keep_distinct_timing() {
        for (delay, input, timeout) in [
            (0, false, Some(100)),
            (1, false, Some(10)),
            (0, true, None),
            (2, true, Some(20)),
        ] {
            let mut frame = frame(0, 0, 1, 1, &[1], DisposalMethod::Keep);
            frame.delay = delay;
            frame.needs_user_input = input;
            let mut source = source(
                encode(1, 1, None, vec![frame]),
                BackgroundMode::TransparentCanvas,
            );
            let output = source.next_frame().unwrap().unwrap();
            assert_eq!(output.timing.raw_delay_centis, delay);
            assert_eq!(output.timing.needs_user_input, input);
            assert_eq!(output.timing.timeout(), timeout.map(Duration::from_millis));
            assert_eq!(output.timing.ready(Duration::ZERO, true), input);
            if let Some(timeout) = output.timing.timeout() {
                assert!(!output
                    .timing
                    .ready(timeout - Duration::from_millis(1), false));
                assert!(output.timing.ready(timeout, false));
            } else {
                assert!(!output.timing.ready(Duration::from_secs(86_400), false));
            }
        }
    }

    #[test]
    fn image_animation_cancel_poison_and_restart_keep_same_source_offset() {
        let bytes = loop_fixture(None);
        let mut prefixed = b"prefix".to_vec();
        prefixed.extend(bytes);
        let mut reader = Cursor::new(prefixed);
        reader.set_position(6);
        let cancel = token();
        let mut source =
            GifSource::from_reader(reader, GifOptions::default(), cancel.clone()).unwrap();
        assert_eq!(source.next_frame().unwrap().unwrap().generation, 0);
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::Cancelled)
        ));
        cancel.store(false, Ordering::Relaxed);
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::RequiresRestart)
        ));
        source.restart(token()).unwrap();
        let first = source.next_frame().unwrap().unwrap();
        assert_eq!((first.generation, first.cycle, first.index), (1, 0, 0));
        assert_eq!(
            first.image().bgra,
            colors(&[1, 0], BackgroundMode::TransparentCanvas)
        );
    }

    #[test]
    fn image_animation_delivery_leases_apply_backpressure_across_restart() {
        let mut source = source(
            loop_fixture(Some(Repeat::Infinite)),
            BackgroundMode::TransparentCanvas,
        );
        let mut held = Vec::new();
        for _ in 0..4 {
            let frame = source.next_frame().unwrap().unwrap();
            assert!(frame.image().bgra.capacity() <= frame.image().bgra.len());
            held.push(frame);
        }
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::Backpressure)
        ));
        drop(held.remove(0));
        let next = source.next_frame().unwrap().unwrap();
        assert_eq!((next.cycle, next.index), (2, 0));
        held.push(next);
        source.restart(token()).unwrap();
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::Backpressure)
        ));
        held.clear();
        assert_eq!(source.next_frame().unwrap().unwrap().index, 0);
    }

    #[test]
    fn image_animation_capacity_checks_measure_storage_and_cancel_before_allocation() {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        assert!(bytes.capacity() > bytes.len());
        assert!(matches!(
            check_capacity(bytes.capacity(), bytes.len()),
            Err(AnimationError::Invalid(_))
        ));
        check_capacity(bytes.capacity(), bytes.capacity()).unwrap();
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            allocate(usize::MAX, &cancel),
            Err(AnimationError::Cancelled)
        ));
    }

    #[test]
    fn image_animation_smaller_previous_reuses_canvas_slots_and_failed_lease_is_released() {
        let mut source = source(
            encode(
                2,
                2,
                Some(Repeat::Infinite),
                vec![
                    frame(0, 0, 2, 2, &[1; 4], DisposalMethod::Previous),
                    frame(1, 1, 1, 1, &[2], DisposalMethod::Previous),
                ],
            ),
            BackgroundMode::TransparentCanvas,
        );
        let first = source.next_frame().unwrap().unwrap();
        let indices_capacity = source.indices.capacity();
        let restore_capacity = source.restore.capacity();
        let second = source.next_frame().unwrap().unwrap();
        assert_eq!(source.indices.capacity(), indices_capacity);
        assert_eq!(source.restore.capacity(), restore_capacity);
        assert!(indices_capacity > source.indices.len());
        assert!(restore_capacity > source.restore.len());
        assert_eq!(
            second.image().bgra,
            colors(&[0, 0, 0, 2], BackgroundMode::TransparentCanvas)
        );

        // 长度合法但实际驻留超过索引槽，必须在推进下一轮前失败并退还新租约。
        source.indices = Vec::with_capacity(8);
        source.indices.push(1);
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::Invalid(_))
        ));
        assert_eq!(source.index, 2);
        assert_eq!(source.outputs.live.load(Ordering::Acquire), 2);
        source.indices = Vec::new();
        source.restart(token()).unwrap();
        let restarted = source.next_frame().unwrap().unwrap();
        assert_eq!(
            (restarted.generation, restarted.cycle, restarted.index),
            (1, 0, 0)
        );
        assert_eq!(source.outputs.live.load(Ordering::Acquire), 3);
        drop((first, second, restarted));
        assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
    }

    #[test]
    fn image_animation_prechecks_canvas_input_and_total_budget() {
        let options = GifOptions::default();
        assert!(canvas_budget(4000, 2000, options).unwrap() <= 256 * 1024 * 1024);
        assert!(canvas_budget(
            8000,
            4000,
            GifOptions {
                max_canvas_pixels: 32_000_000,
                ..options
            }
        )
        .is_err());
        let mut bytes = loop_fixture(None);
        let input_length = bytes.len() as u64;
        assert!(GifSource::from_reader(
            Cursor::new(bytes.clone()),
            GifOptions {
                max_input_bytes: input_length - 1,
                ..options
            },
            token()
        )
        .is_err());
        bytes[6..8].copy_from_slice(&10_000u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&10_000u16.to_le_bytes());
        assert!(GifSource::from_reader(Cursor::new(bytes), options, token()).is_err());
        assert!(options_valid(GifOptions {
            zero_delay: Duration::ZERO,
            ..options
        })
        .is_err());
    }

    #[test]
    fn image_animation_application_constructor_peak_is_checked_before_reading() {
        struct CountingReader {
            cursor: Cursor<Vec<u8>>,
            reads: Arc<AtomicUsize>,
            seeks: Arc<AtomicUsize>,
        }
        impl Read for CountingReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                self.cursor.read(output)
            }
        }
        impl Seek for CountingReader {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.seeks.fetch_add(1, Ordering::Relaxed);
                self.cursor.seek(position)
            }
        }
        let blocks = DECODER_BLOCK_BYTES as usize / 255;
        let bytes = metadata_fixture(blocks, 0);
        let reads = Arc::new(AtomicUsize::new(0));
        let seeks = Arc::new(AtomicUsize::new(0));
        for pre_cancelled in [false, true] {
            let result = GifSource::from_reader(
                CountingReader {
                    cursor: Cursor::new(bytes.clone()),
                    reads: reads.clone(),
                    seeks: seeks.clone(),
                },
                GifOptions {
                    max_total_bytes: DECODER_RESERVED_BYTES - 1,
                    ..Default::default()
                },
                Arc::new(AtomicBool::new(pre_cancelled)),
            );
            if pre_cancelled {
                assert!(matches!(result, Err(AnimationError::Cancelled)));
            } else {
                assert!(matches!(result, Err(AnimationError::Invalid(_))));
            }
            assert_eq!(reads.load(Ordering::Relaxed), 0);
            assert_eq!(seeks.load(Ordering::Relaxed), 0);
        }

        let required = canvas_budget(1, 1, GifOptions::default()).unwrap();
        assert_eq!(required, 33 + 5 * DECODER_BLOCK_BYTES + 256 * 1024);
        let mut source = GifSource::from_reader(
            Cursor::new(bytes),
            GifOptions {
                max_total_bytes: required,
                background: BackgroundMode::TransparentCanvas,
                ..Default::default()
            },
            token(),
        )
        .unwrap();
        let decoder = source.decoder.as_ref().unwrap();
        assert_eq!(decoder.icc_profile().unwrap().len(), blocks * 255);
        assert_eq!(decoder.xmp_metadata().unwrap().len(), blocks * 255);
        assert_eq!(source.reserved_bytes(), required);
        assert_eq!(source.retained_bytes(), 4 + DECODER_RETAINED_BYTES);
        assert_eq!(source.next_frame().unwrap().unwrap().image().bgra, color(1));
        assert_eq!(source.next_frame().unwrap().unwrap().image().bgra, color(2));
    }

    #[test]
    fn image_animation_later_applications_keep_initial_metadata_inside_the_fixed_peak() {
        let blocks = DECODER_BLOCK_BYTES as usize / 255;
        let required = canvas_budget(1, 1, GifOptions::default()).unwrap();
        let mut source = GifSource::from_reader(
            Cursor::new(metadata_fixture(1, blocks)),
            GifOptions {
                max_total_bytes: required,
                background: BackgroundMode::TransparentCanvas,
                ..Default::default()
            },
            token(),
        )
        .unwrap();
        assert_eq!(
            source
                .decoder
                .as_ref()
                .unwrap()
                .icc_profile()
                .unwrap()
                .len(),
            255
        );
        let first = source.next_frame().unwrap().unwrap();
        let second = source.next_frame().unwrap().unwrap();
        assert_eq!(first.image().bgra, color(1));
        assert_eq!(second.image().bgra, color(2));
        // 交付首帧后仍遍历大应用块，但已初始化的来源元数据不被后置块替换。
        assert!(source.reader.lock().unwrap().position() > (blocks * 255 * 2) as u64);
        let decoder = source.decoder.as_ref().unwrap();
        assert_eq!(decoder.icc_profile().unwrap().len(), 255);
        assert_eq!(decoder.xmp_metadata().unwrap().len(), 255);
        assert_eq!(source.reserved_bytes(), required);
        assert_eq!(source.retained_bytes(), 5 + DECODER_RETAINED_BYTES);
    }

    #[test]
    fn image_animation_invalid_rect_and_truncation_require_restart() {
        let mut bytes = loop_fixture(None);
        let descriptor = bytes.iter().position(|byte| *byte == 0x2c).unwrap();
        bytes[descriptor + 1..descriptor + 3].copy_from_slice(&u16::MAX.to_le_bytes());
        let mut invalid = source(bytes, BackgroundMode::TransparentCanvas);
        assert!(invalid.next_frame().is_err());
        assert!(matches!(
            invalid.next_frame(),
            Err(AnimationError::RequiresRestart)
        ));
        let mut bytes = loop_fixture(None);
        let descriptor = bytes.iter().position(|byte| *byte == 0x2c).unwrap();
        bytes.truncate(descriptor + 12);
        let mut truncated = source(bytes, BackgroundMode::TransparentCanvas);
        assert!(truncated.next_frame().is_err());
        assert!(matches!(
            truncated.next_frame(),
            Err(AnimationError::RequiresRestart)
        ));
    }

    #[test]
    fn image_animation_indexed_interlacing_returns_normal_rows() {
        let mut interlaced = frame(
            0,
            0,
            2,
            5,
            &[1, 1, 0, 0, 3, 3, 2, 2, 4, 4],
            DisposalMethod::Keep,
        );
        interlaced.interlaced = true;
        let mut source = source(
            encode(2, 5, None, vec![interlaced]),
            BackgroundMode::GlobalPaletteColor,
        );
        let output = source.next_frame().unwrap().unwrap();
        assert_eq!(
            output.image().bgra,
            colors(
                &[1, 1, 2, 2, 3, 3, 4, 4, 0, 0],
                BackgroundMode::GlobalPaletteColor
            )
        );
    }

    #[test]
    fn image_animation_input_bound_and_io_cancellation_are_not_retried() {
        let mut bounded = BoundedRead {
            inner: Cursor::new(vec![1, 2, 3, 4, 5]),
            remaining: 3,
            cancel: token(),
        };
        let mut output = [0; 16];
        assert_eq!(bounded.read(&mut output).unwrap(), 3);
        assert_eq!(bounded.read(&mut output).unwrap(), 0);
        assert_eq!(bounded.inner.position(), 3);

        struct ArmedReader {
            cursor: Cursor<Vec<u8>>,
            armed: Arc<AtomicBool>,
            cancel: Arc<AtomicBool>,
        }
        impl Read for ArmedReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.armed.load(Ordering::Relaxed) {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                let length = output.len().min(1);
                self.cursor.read(&mut output[..length])
            }
        }
        impl Seek for ArmedReader {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                self.cursor.seek(position)
            }
        }
        let armed = token();
        let cancel = token();
        let reader = ArmedReader {
            cursor: Cursor::new(loop_fixture(None)),
            armed: armed.clone(),
            cancel: cancel.clone(),
        };
        let mut source =
            GifSource::from_reader(reader, GifOptions::default(), cancel.clone()).unwrap();
        drop(source.next_frame().unwrap().unwrap());
        armed.store(true, Ordering::Relaxed);
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::Cancelled)
        ));
        cancel.store(false, Ordering::Relaxed);
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::RequiresRestart)
        ));
    }

    #[test]
    fn image_animation_rewind_cancellation_keeps_source_for_restart() {
        struct CancelOnRewind {
            cursor: Cursor<Vec<u8>>,
            armed: Arc<AtomicBool>,
            cancel: Arc<AtomicBool>,
        }
        impl Read for CancelOnRewind {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.cursor.read(output)
            }
        }
        impl Seek for CancelOnRewind {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                let offset = self.cursor.seek(position)?;
                if position == SeekFrom::Start(0) && self.armed.swap(false, Ordering::Relaxed) {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                Ok(offset)
            }
        }
        let bytes = encode(
            1,
            1,
            Some(Repeat::Infinite),
            vec![frame(0, 0, 1, 1, &[1], DisposalMethod::Keep)],
        );
        let armed = token();
        let cancel = token();
        let reader = CancelOnRewind {
            cursor: Cursor::new(bytes),
            armed: armed.clone(),
            cancel: cancel.clone(),
        };
        let mut source = GifSource::from_reader(reader, GifOptions::default(), cancel).unwrap();
        assert_eq!(source.dimensions(), (1, 1));
        assert!(source.reserved_bytes() <= GifOptions::default().max_total_bytes);
        drop(source.next_frame().unwrap().unwrap());
        armed.store(true, Ordering::Relaxed);
        assert!(matches!(
            source.next_frame(),
            Err(AnimationError::Cancelled)
        ));
        assert!(source.decoder.is_none());
        source.restart(token()).unwrap();
        let output = source.next_frame().unwrap().unwrap();
        assert_eq!((output.generation, output.cycle, output.index), (1, 0, 0));
        assert_eq!(output.image().bgra, color(1));
    }
}
