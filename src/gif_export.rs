//! MP4 → GIF：短片缓存RGB555 keys；长片在同一系统解码器上seek后重放。
//! 两种策略共用原整段调色板和查找表，不为长片写入大量像素缓存。
//! 索引相同的帧合并延时；差异矩形用有界 LZW 工作线程压缩并按序写入。
use crate::{media_foundation::VideoReader, Result};
use rustshot_gif_codec::{Histogram, Scaler};
use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[path = "gif_frame_cache.rs"]
mod gif_frame_cache;
#[path = "gif_lzw.rs"]
mod gif_lzw;
#[path = "gif_output.rs"]
mod gif_output;
#[path = "video_gif_frames.rs"]
mod video_gif_frames;
#[path = "video_gif_options.rs"]
mod video_gif_options;
use gif_frame_cache::{CachedFrame, FrameCache, KEY_MEMORY_LIMIT};
#[cfg(test)]
use gif_lzw::{delay_centis, next_delay_chunk};
use gif_lzw::{LzwPipeline, Rectangle};
use video_gif_frames::RangeFrames;
pub(crate) use video_gif_options::GifOptions;

pub(crate) const MAX_GIF_FPS: u32 = 15;
/// GIF 体积随面积线性增长；超过此长边时等比缩小。
pub(crate) const MAX_GIF_SIDE: u32 = 1280;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

/// 共用同一调色板的两帧索引变化包围盒；完全相同返回 None。
pub(crate) fn diff_bounds(
    previous: &[u8],
    current: &[u8],
    width: u32,
    height: u32,
) -> Option<Rect> {
    rustshot_gif_codec::frames::diff_bounds(previous, current, width, height).map(|rect| Rect {
        left: rect.left,
        top: rect.top,
        width: rect.width,
        height: rect.height,
    })
}

pub(crate) fn output_size(width: u32, height: u32) -> (u32, u32) {
    output_size_with_limit(width, height, MAX_GIF_SIDE)
}

fn output_size_with_limit(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let side = width.max(height);
    if side <= max_side {
        return (width, height);
    }
    let scale = f64::from(max_side) / f64::from(side);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// 后台只生成本任务独占的暂存文件，窗口确认意图仍有效后再消耗式发布。
/// 未提交或提交失败时，TemporaryGif 自动删除本任务暂存，不触及已有成品。
pub(crate) struct PreparedGif {
    temporary: Option<gif_output::TemporaryGif>,
    source: PathBuf,
    target: PathBuf,
}

impl PreparedGif {
    pub(crate) fn commit(mut self, cancel: &AtomicBool) -> Result<PathBuf> {
        check_cancel(cancel)?;
        // 两条路径已经在后台固定为verbatim绝对路径；此处不重复查询目录或媒体。
        use std::os::windows::ffi::OsStrExt;
        let original: Vec<u16> = self.source.as_os_str().encode_wide().collect();
        let target: Vec<u16> = self.target.as_os_str().encode_wide().collect();
        if unsafe {
            windows::Win32::Globalization::CompareStringOrdinal(&original, &target, true)
                == windows::Win32::Globalization::CSTR_EQUAL
        } {
            return Err(tr!(
                "不能覆盖原视频文件",
                "The original video cannot be overwritten"
            )
            .into());
        }
        let staged = self
            .temporary
            .as_ref()
            .expect("owned GIF staging file")
            .path();
        let staged: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.into_iter().chain(Some(0)).collect();
        check_cancel(cancel)?;
        // 不带REPLACE_EXISTING/COPY_ALLOWED；检查后出现的源别名或成品也不能被替换。
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                windows::core::PCWSTR(staged.as_ptr()),
                windows::core::PCWSTR(target.as_ptr()),
                windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
            )
        }?;
        self.temporary.take();
        Ok(self.target.clone())
    }
}

/// GIF时长按10ms刻度量化；不足10ms的片段明确拒绝。
pub(crate) fn prepare_with_options(
    source: &Path,
    target: &Path,
    options: GifOptions,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<PreparedGif> {
    options.validate()?;
    if !progress(0.) {
        return Err(tr!("已取消 GIF 转换", "GIF conversion canceled").into());
    }
    crate::image_collection::ensure_local_file(source)?;
    // 媒体URL读取保留调用方的文件路径表达；规范路径只用于源身份与发布保护。
    // canonicalize新增的verbatim前缀会被URL读取器误当成网络地址。
    let reader_source = std::path::absolute(source)?;
    let source = std::fs::canonicalize(&reader_source)?;
    let target = gif_target(target)?;
    protect_source(&source, &target)?;
    ensure_new_target(&target)?;
    let temporary = gif_output::TemporaryGif::new(&target)?;
    let _runtime = crate::media_foundation::MfRuntime::new()?;
    convert::<false>(
        &reader_source,
        temporary.path(),
        options.fps,
        &mut progress,
        Some(KEY_MEMORY_LIMIT),
        &mut ConversionStats::default(),
        Some(options),
    )?;
    Ok(PreparedGif {
        temporary: Some(temporary),
        source,
        target,
    })
}

fn gif_target(chosen: &Path) -> Result<PathBuf> {
    let chosen = match chosen.extension() {
        None => chosen.with_extension("gif"),
        Some(extension) if extension.is_empty() => chosen.with_extension("gif"),
        Some(extension) if extension.eq_ignore_ascii_case("gif") => chosen.to_path_buf(),
        _ => return Err(tr!("请保存为 GIF 文件", "Save as a GIF file").into()),
    };
    let absolute = std::path::absolute(chosen)?;
    let parent = std::fs::canonicalize(absolute.parent().ok_or(tr!(
        "GIF 保存目标没有父目录",
        "The GIF target has no parent folder"
    ))?)?;
    let name = absolute
        .file_name()
        .ok_or(tr!("GIF 保存文件名无效", "The GIF file name is invalid"))?;
    // 不允许零字符或Windows命名流把一个新文件意图变成对现有文件的附加写入。
    if name.to_string_lossy().contains(['\0', ':']) {
        return Err(tr!("GIF 保存文件名无效", "The GIF file name is invalid").into());
    }
    Ok(parent.join(name))
}

fn protect_source(source: &Path, target: &Path) -> Result<()> {
    if crate::export::is_same_path(source, target) {
        return Err(tr!(
            "不能覆盖原视频文件",
            "The original video cannot be overwritten"
        )
        .into());
    }
    Ok(())
}

fn ensure_new_target(target: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(target) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            tr!(
                "目标文件已存在，请选择新文件名",
                "The file already exists; choose a new name"
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::other(tr!(
            "已取消 GIF 转换",
            "GIF conversion canceled"
        )))
    } else {
        Ok(())
    }
}

fn crop(indices: &[u8], width: u32, rect: Rect) -> Vec<u8> {
    rustshot_gif_codec::frames::crop(
        indices,
        width,
        rustshot_gif_codec::frames::Rect {
            left: rect.left,
            top: rect.top,
            width: rect.width,
            height: rect.height,
        },
    )
}

struct Pending {
    rect: Rect,
    indices: Vec<u8>,
    start: Duration,
}

enum Replay {
    Cached(FrameCache),
    Decoded(VideoReader),
}

enum ReplayFrame {
    Cached(CachedFrame),
    Decoded(crate::media_foundation::DecodedFrame),
}

/// 默认预算按保守采样上界选型；自定义预算保留原缓存路径，0可强制真实落盘。
/// SourceReader允许比minimum早1ms的帧，所以用更短的间隔估算，再多留一帧。
fn cache_keys_for_duration(
    duration: Duration,
    pixels: usize,
    fps: u32,
    cache_memory_limit: usize,
) -> bool {
    if cache_memory_limit != KEY_MEMORY_LIMIT {
        return true;
    }
    if duration.is_zero() || pixels == 0 {
        return false;
    }
    let fps = fps.clamp(1, MAX_GIF_FPS);
    let minimum_step_ns = 1_000_000_000_u128 / u128::from(fps) - 1_000_000;
    let frames = duration
        .as_nanos()
        .div_ceil(minimum_step_ns)
        .saturating_add(1);
    (pixels as u128)
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_mul(frames))
        .is_some_and(|bytes| bytes <= cache_memory_limit as u128)
}

/// progress 返回 false 时取消；只清理本任务的独占临时文件，完成后才发布成品。
pub(crate) fn mp4_to_gif(
    source: &Path,
    target: &Path,
    fps: u32,
    mut progress: impl FnMut(f32) -> bool,
) -> Result<()> {
    let temporary = gif_output::TemporaryGif::new(target)?;
    convert::<false>(
        source,
        temporary.path(),
        fps,
        &mut progress,
        Some(KEY_MEMORY_LIMIT),
        &mut ConversionStats::default(),
        None,
    )?;
    temporary.publish(target)?;
    Ok(())
}

/// CLI与后台基准共用真实产品策略；常用GUI入口以const关闭计时。
#[derive(Default, Debug, serde::Serialize)]
pub(crate) struct ConversionStats {
    pub strategy: &'static str,
    pub decode_passes: u32,
    pub width: u32,
    pub height: u32,
    pub sampled_frames: u64,
    pub decoder_open_ms: f64,
    pub decode_ms: f64,
    pub seek_ms: f64,
    pub histogram_sample_ms: f64,
    pub prepare_keys_ms: f64,
    pub cache_write_ms: f64,
    pub palette_ms: f64,
    pub cache_read_ms: f64,
    pub index_ms: f64,
    pub difference_ms: f64,
    pub submit_ms: f64,
    pub finish_ms: f64,
    pub wall_ms: f64,
    pub key_memory_peak_bytes: u64,
    pub key_spilled_bytes: u64,
}

pub(crate) fn mp4_to_gif_with_stats(
    source: &Path,
    target: &Path,
    fps: u32,
    mut progress: impl FnMut(f32) -> bool,
    cache_memory_limit: usize,
) -> Result<ConversionStats> {
    let start = Instant::now();
    let mut stats = ConversionStats::default();
    let temporary = gif_output::TemporaryGif::new(target)?;
    convert::<true>(
        source,
        temporary.path(),
        fps,
        &mut progress,
        Some(cache_memory_limit),
        &mut stats,
        None,
    )?;
    temporary.publish(target)?;
    stats.wall_ms = start.elapsed().as_secs_f64() * 1000.;
    Ok(stats)
}

fn timer<const PROFILE: bool>() -> Option<Instant> {
    if PROFILE {
        Some(Instant::now())
    } else {
        None
    }
}
fn add_elapsed(start: Option<Instant>, total: &mut f64) {
    if let Some(start) = start {
        *total += start.elapsed().as_secs_f64() * 1000.;
    }
}

fn convert<const PROFILE: bool>(
    source: &Path,
    target: &Path,
    fps: u32,
    progress: &mut dyn FnMut(f32) -> bool,
    cache_memory_limit: Option<usize>,
    stats: &mut ConversionStats,
    options: Option<GifOptions>,
) -> Result<()> {
    if options.is_some() && !progress(0.) {
        return Err(tr!("已取消 GIF 转换", "GIF conversion canceled").into());
    }
    let opened = timer::<PROFILE>();
    let mut reader = VideoReader::open(source)?;
    add_elapsed(opened, &mut stats.decoder_open_ms);
    if reader.width == 0 || reader.height == 0 {
        return Err("GIF 来源尺寸无效".into());
    }
    let (out_w, out_h) = options.map_or_else(
        || output_size(reader.width, reader.height),
        |options| output_size_with_limit(reader.width, reader.height, options.max_side),
    );
    if out_w > u32::from(u16::MAX) || out_h > u32::from(u16::MAX) {
        return Err("GIF 尺寸超出格式上限".into());
    }
    let interval = Duration::from_secs_f64(1. / f64::from(fps.clamp(1, MAX_GIF_FPS)));
    let range = options
        .map(|options| options.range(reader.duration))
        .transpose()?;
    let duration = range.map_or(reader.duration, |(start, end)| end - start);
    if options.is_some() && duration < Duration::from_millis(10) {
        return Err(tr!(
            "GIF 片段至少需要 10 毫秒，时长按 10 毫秒量化",
            "A GIF segment must be at least 10 ms; timing is quantized to 10 ms"
        )
        .into());
    }
    let total = duration.max(Duration::from_millis(1));
    let mut range_frames = range
        .map(|(start, end)| RangeFrames::new(start, end, fps))
        .transpose()?;
    let size = (reader.width, reader.height);
    let output = (out_w, out_h);
    let scaler = Scaler::new(size, output);
    let mut histogram = Histogram::new();
    let pixels = (out_w as usize)
        .checked_mul(out_h as usize)
        .ok_or("GIF缓存尺寸溢出")?;
    // None显式选择二次解码；Some延续预算选型，零预算仍表示缓存落盘。
    let mut cache = cache_memory_limit
        .filter(|&limit| cache_keys_for_duration(duration, pixels, fps, limit))
        .map(|limit| FrameCache::new(target, pixels, limit))
        .transpose()?;
    if PROFILE {
        stats.strategy = if cache.is_some() {
            "cached-keys"
        } else {
            "two-pass-seek"
        };
        stats.decode_passes = if cache.is_some() { 1 } else { 2 };
    }
    let mut next_time = Duration::ZERO;
    let mut sampling_progress = 0.;
    if !progress(0.) {
        return Err("已取消 GIF 转换".into());
    }
    // 采样时钟和Histogram原样保留；只为预计能留在内存的短片物化keys。
    loop {
        let decoding = timer::<PROFILE>();
        let frame = match &mut range_frames {
            Some(frames) => frames.next(&mut reader, &mut || progress(sampling_progress))?,
            None => reader.next_at(next_time)?,
        };
        add_elapsed(decoding, &mut stats.decode_ms);
        let Some(frame) = frame else { break };
        if options.is_none() {
            next_time = frame.time + interval;
        }
        let value = (0.35 * frame.time.as_secs_f64() / total.as_secs_f64()).min(0.35) as f32;
        sampling_progress = value;
        if !progress(value) {
            return Err("已取消 GIF 转换".into());
        }
        if let Some(cache) = &mut cache {
            let preparing = timer::<PROFILE>();
            let keys = scaler
                .accumulate_keys_with(&mut histogram, &frame.bgra, &mut || progress(value))?;
            add_elapsed(preparing, &mut stats.prepare_keys_ms);
            let writing = timer::<PROFILE>();
            cache.push(
                CachedFrame {
                    time: frame.time,
                    keys,
                },
                &mut || progress(value),
            )?;
            add_elapsed(writing, &mut stats.cache_write_ms);
        } else {
            let sampling = timer::<PROFILE>();
            scaler.accumulate(&mut histogram, &frame.bgra);
            add_elapsed(sampling, &mut stats.histogram_sample_ms);
            if !progress(value) {
                return Err("已取消 GIF 转换".into());
            }
        }
        if PROFILE {
            stats.sampled_frames += 1;
        }
    }
    let mut replay = if let Some(cache) = cache {
        drop(reader);
        Replay::Cached(cache)
    } else {
        Replay::Decoded(reader)
    };
    let building = timer::<PROFILE>();
    let palette = match options {
        Some(options) => histogram
            .build_with_colors(options.colors, &mut |value| progress(0.35 + value * 0.1))?,
        None => histogram.build(&mut |value| progress(0.35 + value * 0.1))?,
    };
    add_elapsed(building, &mut stats.palette_ms);
    if PROFILE {
        stats.width = out_w;
        stats.height = out_h;
    }
    match &mut replay {
        Replay::Cached(cache) => {
            if PROFILE {
                stats.key_memory_peak_bytes = cache.payload_peak as u64;
                stats.key_spilled_bytes = cache.disk_bytes;
            }
            cache.begin_read()?;
        }
        Replay::Decoded(reader) => {
            if !progress(0.45) {
                return Err("已取消 GIF 转换".into());
            }
            let seeking = timer::<PROFILE>();
            let seek_result = reader.seek(Duration::ZERO);
            add_elapsed(seeking, &mut stats.seek_ms);
            // 不可seek的普通来源沿用重新打开解码器的兼容路径。
            if seek_result.is_err() {
                if options.is_some() {
                    crate::image_collection::ensure_local_file(source)?;
                }
                let opening = timer::<PROFILE>();
                let replacement = VideoReader::open(source)?;
                add_elapsed(opening, &mut stats.decoder_open_ms);
                if (replacement.width, replacement.height) != size {
                    return Err("GIF重放来源尺寸发生变化".into());
                }
                *reader = replacement;
                if PROFILE {
                    stats.strategy = "two-pass-reopen";
                }
            }
        }
    }
    next_time = Duration::ZERO;
    range_frames = range
        .map(|(start, end)| RangeFrames::new(start, end, fps))
        .transpose()?;
    let mut encoder = gif::Encoder::new(
        BufWriter::new(File::create(target)?),
        out_w as u16,
        out_h as u16,
        &palette.table,
    )?;
    encoder.set_repeat(gif::Repeat::Infinite)?;
    let mut compression = LzwPipeline::new()?;
    let mut previous: Option<Vec<u8>> = None;
    let mut pending: Option<Pending> = None;
    let mut last_time = Duration::ZERO;
    let mut last_progress = 0.45;
    loop {
        let frame = match &mut replay {
            Replay::Cached(cache) => {
                let reading = timer::<PROFILE>();
                let frame = cache.next(&mut || progress(last_progress))?;
                add_elapsed(reading, &mut stats.cache_read_ms);
                frame.map(ReplayFrame::Cached)
            }
            Replay::Decoded(reader) => {
                if !progress(last_progress) {
                    return Err("已取消 GIF 转换".into());
                }
                let decoding = timer::<PROFILE>();
                let frame = match &mut range_frames {
                    Some(frames) => frames.next(reader, &mut || progress(last_progress))?,
                    None => reader.next_at(next_time)?,
                };
                add_elapsed(decoding, &mut stats.decode_ms);
                if let Some(frame) = &frame {
                    if options.is_none() {
                        next_time = frame.time + interval;
                    }
                }
                frame.map(ReplayFrame::Decoded)
            }
        };
        let Some(frame) = frame else { break };
        let frame_time = match &frame {
            ReplayFrame::Cached(frame) => frame.time,
            ReplayFrame::Decoded(frame) => frame.time,
        };
        last_time = frame_time;
        let current_progress =
            (0.45 + 0.54 * frame_time.as_secs_f64() / total.as_secs_f64()).min(0.99) as f32;
        last_progress = current_progress;
        if !progress(current_progress) {
            return Err("已取消 GIF 转换".into());
        }
        let mapping = timer::<PROFILE>();
        let indices = match frame {
            ReplayFrame::Cached(frame) => palette.index_keys(&frame.keys),
            ReplayFrame::Decoded(frame) => scaler.index(&frame.bgra, &palette),
        };
        add_elapsed(mapping, &mut stats.index_ms);
        let difference = timer::<PROFILE>();
        let rect = match &previous {
            None => Some(Rect {
                left: 0,
                top: 0,
                width: out_w,
                height: out_h,
            }),
            Some(previous) => diff_bounds(previous, &indices, out_w, out_h),
        };
        add_elapsed(difference, &mut stats.difference_ms);
        if let Some(rect) = rect {
            if let Some(done) = pending.take() {
                let submit = timer::<PROFILE>();
                enqueue_frame(
                    &mut compression,
                    &mut encoder,
                    done,
                    frame_time,
                    &mut || progress(current_progress),
                )?;
                add_elapsed(submit, &mut stats.submit_ms);
            }
            pending = Some(Pending {
                rect,
                indices: crop(&indices, out_w, rect),
                start: frame_time,
            });
            previous = Some(indices);
        }
    }
    let Some(done) = pending.take() else {
        return Err("录屏没有可转换的画面".into());
    };
    // 优先媒体实际尾点，不把30fps尾帧再延长一个15fps间隔。未知/异常时长沿用有界尾帧回退。
    let end = if options.is_some() || duration > last_time.max(done.start) {
        duration
    } else {
        last_time.max(done.start) + interval
    };
    let finishing = timer::<PROFILE>();
    let short_tail = options
        .map(|_| quantized_tail_delay(done.start, end))
        .filter(|delay| *delay < 2);
    if let Some(delay) = short_tail {
        // 仅新范围最后一帧允许1cs；先排空旧流水线，避免改变旧录屏的最小2cs合同。
        compression.finish(&mut encoder, &mut || progress(0.99))?;
        if !progress(0.99) {
            return Err(tr!("已取消 GIF 转换", "GIF conversion canceled").into());
        }
        encoder.write_frame(&gif::Frame {
            left: done.rect.left as u16,
            top: done.rect.top as u16,
            width: done.rect.width as u16,
            height: done.rect.height as u16,
            delay: delay as u16,
            dispose: gif::DisposalMethod::Keep,
            buffer: std::borrow::Cow::Owned(done.indices),
            ..gif::Frame::default()
        })?;
    } else {
        enqueue_frame(&mut compression, &mut encoder, done, end, &mut || {
            progress(0.99)
        })?;
        compression.finish(&mut encoder, &mut || progress(0.99))?;
    }
    let mut output = encoder.into_inner()?;
    output.flush()?;
    drop(output);
    add_elapsed(finishing, &mut stats.finish_ms);
    if !progress(1.) {
        return Err("已取消 GIF 转换".into());
    }
    Ok(())
}

fn quantized_tail_delay(start: Duration, end: Duration) -> u64 {
    let centis = |time: Duration| {
        ((time.as_nanos() + 5_000_000) / 10_000_000).min(u128::from(u64::MAX)) as u64
    };
    // 极短尾帧至少占一个GIF刻度，总尾点误差不超过一个10ms刻度。
    centis(end).saturating_sub(centis(start)).max(1)
}

fn enqueue_frame(
    compression: &mut LzwPipeline,
    encoder: &mut gif::Encoder<BufWriter<File>>,
    pending: Pending,
    end: Duration,
    proceed: &mut dyn FnMut() -> bool,
) -> Result<()> {
    compression.submit(
        encoder,
        Rectangle {
            left: pending.rect.left as u16,
            top: pending.rect.top as u16,
            width: pending.rect.width as u16,
            height: pending.rect.height as u16,
        },
        pending.indices,
        pending.start,
        end,
        proceed,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rustshot-video-gif-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn staged(source: &Path, target: &Path) -> PreparedGif {
        let target = gif_target(target).unwrap();
        let temporary = gif_output::TemporaryGif::new(&target).unwrap();
        std::fs::write(temporary.path(), b"prepared GIF bytes").unwrap();
        PreparedGif {
            temporary: Some(temporary),
            source: std::fs::canonicalize(source).unwrap(),
            target,
        }
    }

    #[test]
    fn gif_preparation_is_owned_and_commit_is_cancelable_without_replacing() {
        fn is_send<T: Send>() {}
        is_send::<PreparedGif>();
        let root = TestDirectory::new();
        let source = root.0.join("原片.mp4");
        let target = root.0.join("输出.gif");
        std::fs::write(&source, b"original").unwrap();
        let prepared = staged(&source, &target);
        let temporary = prepared.temporary.as_ref().unwrap().path().to_path_buf();
        assert!(!target.exists());
        drop(prepared);
        assert!(!temporary.exists());
        let prepared = staged(&source, &target);
        let temporary = prepared.temporary.as_ref().unwrap().path().to_path_buf();
        assert!(prepared.commit(&AtomicBool::new(true)).is_err());
        assert!(!temporary.exists());
        assert!(!target.exists());
        let prepared = staged(&source, &target);
        let temporary = prepared.temporary.as_ref().unwrap().path().to_path_buf();
        std::fs::write(&target, b"competing output").unwrap();
        assert!(prepared.commit(&AtomicBool::new(false)).is_err());
        assert!(!temporary.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"competing output");
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
        let published = staged(&source, &root.0.join("new"))
            .commit(&AtomicBool::new(false))
            .unwrap();
        assert!(published.is_absolute());
        assert_eq!(published.extension().unwrap(), "gif");
        assert_eq!(std::fs::read(published).unwrap(), b"prepared GIF bytes");
    }

    #[test]
    fn gif_prepare_rejects_source_alias_existing_target_and_cancel_before_reading() {
        let root = TestDirectory::new();
        let missing = root.0.join("not-present.mp4");
        assert!(prepare_with_options(
            &missing,
            &root.0.join("out.gif"),
            GifOptions::default(),
            |_| false
        )
        .is_err());
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
        let source = root.0.join("原片.gif");
        std::fs::write(&source, b"original media").unwrap();
        assert!(prepare_with_options(&source, &source, GifOptions::default(), |_| true).is_err());
        let alias = root.0.join("hard-link.gif");
        std::fs::hard_link(&source, &alias).unwrap();
        assert!(prepare_with_options(&source, &alias, GifOptions::default(), |_| true).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"original media");
        assert_eq!(std::fs::read(&alias).unwrap(), b"original media");
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 2);
        assert!(gif_target(&root.0.join("out.png")).is_err());
        assert!(gif_target(&root.0.join("source:stream.gif")).is_err());
        assert_eq!(
            gif_target(&root.0.join("Chinese 空格.GIF"))
                .unwrap()
                .extension()
                .unwrap(),
            "GIF"
        );
    }

    #[test]
    fn gif_commit_rechecks_fixed_source_and_atomic_publication_protects_late_alias() {
        let root = TestDirectory::new();
        let source = root.0.join("source.gif");
        let target = root.0.join("output.gif");
        std::fs::write(&source, b"original").unwrap();
        assert!(staged(&source, &source)
            .commit(&AtomicBool::new(false))
            .is_err());
        let prepared = staged(&source, &target);
        std::fs::hard_link(&source, &target).unwrap();
        assert!(prepared.commit(&AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
    }

    #[test]
    fn gif_new_range_tail_uses_one_centisecond_without_changing_legacy_minimum() {
        assert_eq!(
            quantized_tail_delay(Duration::from_millis(675), Duration::from_millis(685)),
            1
        );
        assert_eq!(
            quantized_tail_delay(Duration::from_millis(675), Duration::from_millis(676)),
            1
        );
        assert_eq!(
            quantized_tail_delay(Duration::ZERO, Duration::from_millis(10)),
            1
        );
        assert_eq!(
            quantized_tail_delay(Duration::ZERO, Duration::from_millis(750)),
            75
        );
        assert_eq!(delay_centis(Duration::ZERO, Duration::from_millis(10)), 2);
        assert_eq!(output_size_with_limit(320, 240, 128), (128, 96));
        assert_eq!(output_size_with_limit(64, 48, 128), (64, 48));
    }

    #[test]
    #[ignore = "显式生成自有静音MP4并解码GIF；不打开窗口、不读取桌面"]
    fn native_gif_options_range_palette_timing_and_default_compatibility() -> Result<()> {
        use crate::media_foundation::{MfRuntime, Mp4Writer};
        use std::collections::BTreeSet;
        let root = TestDirectory::new();
        let source = root.0.join("片段 with spaces.mp4");
        let _runtime = MfRuntime::new()?;
        let mut writer = Mp4Writer::new(&source, 320, 240, 10, 800_000, false)?;
        let mut pixels = vec![0; 320 * 240 * 4];
        for frame in 0..10_u64 {
            for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
                let x = index % 320;
                let y = index / 320;
                // 黑白标记让400ms和600ms的绿色画面仍可区分，核对真实采样时刻。
                let bgra = if x < 160 && y < 40 {
                    if frame % 4 < 2 {
                        [0, 0, 0, 255]
                    } else {
                        [255, 255, 255, 255]
                    }
                } else if x < 160 {
                    match frame {
                        0 | 1 => [16, 20, 220, 255],
                        2..=7 => [16, 220, 20, 255],
                        _ => [220, 20, 16, 255],
                    }
                } else {
                    [
                        (x * 255 / 319) as u8,
                        (y * 255 / 239) as u8,
                        ((x + y) % 256) as u8,
                        255,
                    ]
                };
                pixel.copy_from_slice(&bgra);
            }
            writer.write_video(
                &pixels,
                320,
                Duration::from_millis(frame * 100),
                Duration::from_millis(100),
            )?;
        }
        writer.finish()?;
        let original = std::fs::read(&source)?;
        for colors in [64, 128] {
            let target = root.0.join(format!("range-{colors}.gif"));
            let options = GifOptions {
                start: Duration::from_millis(125),
                end: Some(Duration::from_millis(810)),
                fps: 5,
                max_side: 128,
                colors,
            };
            let mut last_progress = 0.;
            let prepared = prepare_with_options(&source, &target, options, |value| {
                assert!((last_progress..=1.).contains(&value));
                last_progress = value;
                true
            })?;
            assert_eq!(last_progress, 1.);
            assert!(!target.exists());
            let published = prepared.commit(&AtomicBool::new(false))?;
            let mut decoder = gif::DecodeOptions::new().read_info(File::open(&published)?)?;
            assert_eq!((decoder.width(), decoder.height()), (128, 96));
            let palette = decoder.global_palette().ok_or("缺少全局调色板")?.to_vec();
            assert_eq!(palette.len(), 256 * 3);
            let mut canvas = vec![0; 128 * 96];
            let mut used = BTreeSet::new();
            let mut delays = Vec::new();
            let mut markers = Vec::new();
            let mut first = None;
            let mut last = [0; 3];
            while let Some(frame) = decoder.read_next_frame()? {
                for (y, row) in frame
                    .buffer
                    .chunks_exact(usize::from(frame.width))
                    .enumerate()
                {
                    let at = (usize::from(frame.top) + y) * 128 + usize::from(frame.left);
                    canvas[at..at + row.len()].copy_from_slice(row);
                    used.extend(row.iter().copied());
                }
                let index = usize::from(canvas[48 * 128 + 32]) * 3;
                last.copy_from_slice(&palette[index..index + 3]);
                first.get_or_insert(last);
                let marker = usize::from(canvas[8 * 128 + 32]) * 3;
                markers.push(palette[marker..marker + 3].to_vec());
                delays.push(frame.delay);
            }
            // 起点125ms仍由100ms红帧覆盖，800ms蓝尾帧不可被丢掉或强制延长到2cs。
            let first = first.ok_or("GIF没有画面")?;
            assert!(first[0] > 150 && first[2] < 100);
            assert!(last[2] > 150 && last[0] < 100);
            assert_eq!(delays, [28, 20, 20, 1]);
            assert!(markers[1].iter().all(|&channel| channel < 80));
            assert!(markers[2].iter().all(|&channel| channel > 180));
            let total =
                Duration::from_millis(delays.iter().map(|&delay| u64::from(delay) * 10).sum());
            assert!(total.abs_diff(Duration::from_millis(685)) <= Duration::from_millis(10));
            assert!(used.len() > colors / 2 && used.len() <= colors);
            assert!(used.iter().all(|&index| usize::from(index) < colors));
            // 同一范围强制二次解码重放，不能只验证短片缓存分支的采样时钟。
            let replayed = root.0.join(format!("range-{colors}-replayed.gif"));
            let mut replay_stats = ConversionStats::default();
            convert::<true>(
                &source,
                &replayed,
                options.fps,
                &mut |_| true,
                None,
                &mut replay_stats,
                Some(options),
            )?;
            assert_eq!(replay_stats.decode_passes, 2);
            assert_eq!(std::fs::read(&published)?, std::fs::read(replayed)?);
        }
        let legacy = root.0.join("legacy.gif");
        mp4_to_gif(&source, &legacy, 15, |_| true)?;
        let defaults = prepare_with_options(
            &source,
            &root.0.join("defaults.gif"),
            GifOptions::default(),
            |_| true,
        )?
        .commit(&AtomicBool::new(false))?;
        assert_eq!(std::fs::read(legacy)?, std::fs::read(defaults)?);
        let before = std::fs::read_dir(&root.0)?.count();
        let canceled = root.0.join("canceled.gif");
        assert!(
            prepare_with_options(&source, &canceled, GifOptions::default(), |value| value
                < 0.5)
            .is_err()
        );
        assert!(!canceled.exists());
        assert_eq!(std::fs::read_dir(&root.0)?.count(), before);
        assert!(prepare_with_options(
            &source,
            &root.0.join("too-short.gif"),
            GifOptions {
                end: Some(Duration::from_millis(9)),
                ..GifOptions::default()
            },
            |_| true
        )
        .is_err());
        assert_eq!(std::fs::read(&source)?, original);
        Ok(())
    }

    #[test]
    fn gif_diff_bounds_find_changed_rectangle() {
        let (w, h) = (6, 4);
        let a = vec![10_u8; w * h];
        let mut b = a.clone();
        assert_eq!(diff_bounds(&a, &b, w as u32, h as u32), None);
        b[2 * w + 1] = 99;
        b[3 * w + 4] = 1;
        assert_eq!(
            diff_bounds(&a, &b, w as u32, h as u32),
            Some(Rect {
                left: 1,
                top: 2,
                width: 4,
                height: 2
            })
        );
    }

    #[test]
    fn gif_output_size_caps_long_side_and_scales_pixels() {
        assert_eq!(output_size(800, 600), (800, 600));
        assert_eq!(output_size(2560, 1440), (1280, 720));
        let bgra = [0, 0, 200, 255, 0, 0, 100, 255];
        assert_eq!(Scaler::new((2, 1), (1, 1)).color(&bgra, 0, 0), [150, 0, 0]);
        assert_eq!(Scaler::new((2, 1), (2, 1)).color(&bgra, 0, 0), [200, 0, 0]);
    }

    #[test]
    fn gif_indexed_crop_keeps_rectangle_and_interior_pixels() {
        let pixels = (0..24_u8).collect::<Vec<_>>();
        let rect = Rect {
            left: 1,
            top: 2,
            width: 4,
            height: 2,
        };
        assert_eq!(crop(&pixels, 6, rect), [13, 14, 15, 16, 19, 20, 21, 22]);
    }

    #[test]
    fn gif_absolute_delays_preserve_15fps_timeline_and_long_static_span() {
        let time = |index: u64| Duration::from_nanos(index * 1_000_000_000 / 15);
        let total = (0..150)
            .map(|index| delay_centis(time(index), time(index + 1)))
            .sum::<u64>();
        assert_eq!(total, 1000);
        let mut remaining = delay_centis(Duration::ZERO, Duration::from_millis(655_360));
        let mut chunks = Vec::new();
        while remaining != 0 {
            let chunk = next_delay_chunk(remaining);
            chunks.push(chunk);
            remaining -= u64::from(chunk);
        }
        assert_eq!(chunks, [65_534, 2]);
        assert_eq!(chunks.into_iter().map(u64::from).sum::<u64>(), 65_536);
    }

    // 策略边界模型：只验证选型，不重新执行未变化的像素/输出模型。
    #[test]
    fn gif_replay_strategy_keeps_short_720p_keys_and_replays_long_or_unknown() {
        let pixels = 1280 * 720;
        assert!(cache_keys_for_duration(
            Duration::from_secs(2),
            pixels,
            15,
            KEY_MEMORY_LIMIT
        ));
        assert!(!cache_keys_for_duration(
            Duration::from_secs(12),
            pixels,
            15,
            KEY_MEMORY_LIMIT
        ));
        assert!(!cache_keys_for_duration(
            Duration::ZERO,
            pixels,
            15,
            KEY_MEMORY_LIMIT
        ));
        // 特殊预算保留已存在的真实落盘合同，不被默认自动选型旁路。
        for limit in [0, 1, 64] {
            assert!(cache_keys_for_duration(Duration::ZERO, pixels, 15, limit));
            assert!(cache_keys_for_duration(
                Duration::from_secs(12),
                pixels,
                15,
                limit
            ));
        }
    }

    #[test]
    fn gif_replay_strategy_bounds_integer_budget_fps_and_extreme_metadata() {
        // 1ns来源保守估为2帧，等于预算时缓存，多一个像素时重放。
        let boundary_pixels = KEY_MEMORY_LIMIT / 4;
        assert!(cache_keys_for_duration(
            Duration::from_nanos(1),
            boundary_pixels,
            15,
            KEY_MEMORY_LIMIT
        ));
        assert!(!cache_keys_for_duration(
            Duration::from_nanos(1),
            boundary_pixels + 1,
            15,
            KEY_MEMORY_LIMIT
        ));
        assert!(!cache_keys_for_duration(
            Duration::MAX,
            usize::MAX,
            15,
            KEY_MEMORY_LIMIT
        ));
        assert!(!cache_keys_for_duration(
            Duration::from_secs(1),
            0,
            15,
            KEY_MEMORY_LIMIT
        ));
        for duration in [
            Duration::from_nanos(1),
            Duration::from_secs(2),
            Duration::MAX,
        ] {
            for pixels in [1, 1280 * 720, usize::MAX] {
                assert_eq!(
                    cache_keys_for_duration(duration, pixels, 0, KEY_MEMORY_LIMIT),
                    cache_keys_for_duration(duration, pixels, 1, KEY_MEMORY_LIMIT)
                );
                assert_eq!(
                    cache_keys_for_duration(duration, pixels, u32::MAX, KEY_MEMORY_LIMIT),
                    cache_keys_for_duration(duration, pixels, 15, KEY_MEMORY_LIMIT)
                );
            }
        }
    }
}
