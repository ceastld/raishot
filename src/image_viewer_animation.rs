//! 查看器的动画呈现桥；时钟、调度和所有权仍由唯一图片worker负责。
#[path = "image_viewer_webp.rs"]
mod webp;
use super::{
    frame::{FrameRef, ViewerFrame},
    thumbnail, LoadedImage,
};
use crate::{
    canvas_source::CanvasSource,
    capture::Image,
    geometry::{Rect, Rotation},
    image_animation::{GifOptions, GifSource},
    image_animation_playback::{
        AnimationPlayback, GifPlayback, PlaybackSnapshot, PlaybackSource, PlaybackState,
        RetainedPlaybackSource,
    },
    image_apng::{ApngOptions, ApngSource},
    image_apng_playback::ApngPlayback,
    image_metadata::ImageMetadata,
    image_viewer_rotation::ImageRotation,
};
use gpui::RenderImage;
use image::{metadata::Orientation, ImageFormat};
use std::{
    fs::File,
    io::{Read, Seek},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use webp::ActiveWebpAnimation;

/// 核心工作集外还需容纳窗口的当前画面、退役画面和一次交付画面。
// 来源、交付与显示的完整账本仍在打开后按实际尺寸复核。
const VIEWER_ANIMATION_PIXELS: u64 = 5_000_000;

fn resident_error() -> String {
    tr!(
        "动画显示超过总驻留预算",
        "Animation presentation exceeds the resident budget"
    )
    .into()
}

/// 来源自己的工作集另含显示副本；固定缩略采样暂存由查看器单独留出。
fn animation_budget(resident_bytes: u64, cancel: &AtomicBool) -> Result<u64, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(tr!("动画加载已取消", "Animation loading canceled").into());
    }
    crate::image_decode::MAX_DECODE_BYTES
        .checked_sub(resident_bytes)
        .and_then(|bytes| bytes.checked_sub(super::THUMBNAIL_WORKING_BYTES))
        .filter(|bytes| *bytes > 0)
        .ok_or_else(resident_error)
}

fn gif_options(resident_bytes: u64, cancel: &AtomicBool) -> Result<GifOptions, String> {
    // 核心GIF账本未含查看器缓存，和采样暂存一样在源额度之外留出。
    let max_total_bytes = animation_budget(resident_bytes, cancel)?
        .checked_sub(super::THUMBNAIL_CACHE_BYTES as u64)
        .filter(|bytes| *bytes > 0)
        .ok_or_else(resident_error)?;
    Ok(GifOptions {
        max_canvas_pixels: VIEWER_ANIMATION_PIXELS,
        max_total_bytes,
        ..GifOptions::default()
    })
}

fn check_gif_presentation_budget(
    (width, height): (u32, u32),
    source_reserved: u64,
    limit: u64,
) -> Result<(), String> {
    // GIF来源已有8P呈现预留，补到查看器的20P；长图再加4P切片重叠。
    let extra = if width > 16_384 || height > 16_384 {
        16
    } else {
        12
    };
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(extra))
        .and_then(|bytes| bytes.checked_add(source_reserved))
        .filter(|bytes| *bytes <= limit)
        .map(|_| ())
        .ok_or_else(resident_error)
}

#[derive(Clone, Copy)]
pub(super) enum Action {
    Toggle,
    Pause,
    Step,
    Restart,
    Input,
    Rotate(Rotation),
}

pub(super) struct Tick {
    pub image: Option<LoadedImage>,
    pub snapshot: PlaybackSnapshot,
    pub error: Option<String>,
}

pub(super) type ActiveAnimation<R = File> = ActiveFrames<GifSource<R>>;
pub(super) type ActiveApngAnimation<R = File> = ActiveFrames<ApngSource<R>>;

#[derive(Clone, Copy, Debug)]
pub(super) enum Kind {
    Gif,
    Apng,
    Webp,
}

pub(super) enum AnyAnimation {
    Gif(ActiveAnimation),
    Apng(ActiveApngAnimation),
    Webp(ActiveWebpAnimation),
}

impl AnyAnimation {
    /// 追加到UI驻留量；当前raw/sidecar/缩略Arc可与UI重复，作为保守上界。
    pub(super) fn retained_bytes(&self) -> u64 {
        match self {
            Self::Gif(animation) => animation.retained_bytes(),
            Self::Apng(animation) => animation.retained_bytes(),
            Self::Webp(animation) => animation.retained_bytes(),
        }
    }

    pub(super) fn open(
        kind: Kind,
        path: &Path,
        resident_bytes: u64,
        cancel: Arc<AtomicBool>,
        now: Duration,
    ) -> std::result::Result<(Self, LoadedImage), String> {
        match kind {
            Kind::Gif => ActiveAnimation::open(path, resident_bytes, cancel, now)
                .map(|(animation, image)| (Self::Gif(animation), image)),
            Kind::Apng => ActiveApngAnimation::open(path, resident_bytes, cancel, now)
                .map(|(animation, image)| (Self::Apng(animation), image)),
            Kind::Webp => ActiveWebpAnimation::open(path, resident_bytes, cancel, now)
                .map(|(animation, image)| (Self::Webp(animation), image)),
        }
    }

    pub(super) fn cancellation(&self) -> Arc<AtomicBool> {
        match self {
            Self::Gif(animation) => animation.cancellation(),
            Self::Apng(animation) => animation.cancellation(),
            Self::Webp(animation) => animation.cancellation(),
        }
    }

    pub(super) fn snapshot(&self) -> PlaybackSnapshot {
        match self {
            Self::Gif(animation) => animation.snapshot(),
            Self::Apng(animation) => animation.snapshot(),
            Self::Webp(animation) => animation.snapshot(),
        }
    }

    pub(super) fn presentation_pending(&self) -> Option<u64> {
        match self {
            Self::Gif(animation) => animation.presentation_pending(),
            Self::Apng(animation) => animation.presentation_pending(),
            Self::Webp(animation) => animation.presentation_pending(),
        }
    }

    pub(super) fn deadline(&self) -> Option<Duration> {
        match self {
            Self::Gif(animation) => animation.deadline(),
            Self::Apng(animation) => animation.deadline(),
            Self::Webp(animation) => animation.deadline(),
        }
    }

    pub(super) fn begin(&mut self, now: Duration, generation: u64) -> Tick {
        match self {
            Self::Gif(animation) => animation.begin(now, generation),
            Self::Apng(animation) => animation.begin(now, generation),
            Self::Webp(animation) => animation.begin(now, generation),
        }
    }

    pub(super) fn tick(&mut self, now: Duration, force_present: bool) -> Tick {
        match self {
            Self::Gif(animation) => animation.tick(now, force_present),
            Self::Apng(animation) => animation.tick(now, force_present),
            Self::Webp(animation) => animation.tick(now, force_present),
        }
    }

    pub(super) fn control(&mut self, action: Action, now: Duration) -> Tick {
        match self {
            Self::Gif(animation) => animation.control(action, now),
            Self::Apng(animation) => animation.control(action, now),
            Self::Webp(animation) => animation.control(action, now),
        }
    }
}

pub(super) struct ActiveFrames<S: PlaybackSource> {
    playback: AnimationPlayback<S>,
    metadata: ImageMetadata,
    thumbnail: Arc<RenderImage>,
    deadline: Option<Duration>,
    failure: Option<String>,
    pending_presentation: Option<u64>,
    rotation: Rotation,
}

impl<S: RetainedPlaybackSource> ActiveFrames<S> {
    pub(super) fn retained_bytes(&self) -> u64 {
        self.playback
            .retained_bytes()
            .saturating_add(
                self.thumbnail
                    .as_bytes(0)
                    .map_or(0, |bytes| bytes.len() as u64),
            )
            .saturating_add(self.failure.as_ref().map_or(0, |error| error.capacity()) as u64)
    }
}

impl ActiveFrames<GifSource<File>> {
    pub(super) fn open(
        path: &Path,
        resident_bytes: u64,
        cancel: Arc<AtomicBool>,
        now: Duration,
    ) -> std::result::Result<(Self, LoadedImage), String> {
        let options = gif_options(resident_bytes, &cancel)?;
        let playback =
            GifPlayback::open(path, options, cancel, true).map_err(|error| error.to_string())?;
        check_gif_presentation_budget(
            playback.dimensions(),
            playback.source().reserved_bytes(),
            options.max_total_bytes,
        )?;
        let (mut animation, image) = Self::initialize(
            playback,
            std::fs::metadata(path).ok().map(|metadata| metadata.len()),
            now,
        )?;
        // 主窗口确认首帧已经进入渲染后才启动时钟，避免短帧在首次显示前被追帧跳过。
        animation.await_presentation(now)?;
        Ok((animation, image))
    }
}

impl ActiveFrames<ApngSource<File>> {
    pub(super) fn open(
        path: &Path,
        resident_bytes: u64,
        cancel: Arc<AtomicBool>,
        now: Duration,
    ) -> std::result::Result<(Self, LoadedImage), String> {
        let options = ApngOptions {
            max_total_bytes: animation_budget(resident_bytes, &cancel)?,
            ..ApngOptions::default()
        };
        let playback =
            ApngPlayback::open(path, options, cancel, true).map_err(|error| error.to_string())?;
        let (mut animation, image) = Self::initialize_format(
            playback,
            std::fs::metadata(path).ok().map(|metadata| metadata.len()),
            now,
            ImageFormat::Png,
        )?;
        animation.await_presentation(now)?;
        Ok((animation, image))
    }
}

impl<R: Read + Seek> ActiveFrames<GifSource<R>> {
    // GIF内存回归沿用原入口，格式无关呈现逻辑只保留一份。
    fn initialize(
        playback: GifPlayback<R>,
        encoded_bytes: Option<u64>,
        now: Duration,
    ) -> std::result::Result<(Self, LoadedImage), String> {
        Self::initialize_format(playback, encoded_bytes, now, ImageFormat::Gif)
    }
}

impl<S> ActiveFrames<S>
where
    S: PlaybackSource,
    S::Frame: ViewerFrame + 'static,
{
    pub(super) fn presentation_pending(&self) -> Option<u64> {
        self.pending_presentation
    }

    pub(super) fn begin(&mut self, now: Duration, generation: u64) -> Tick {
        if self.pending_presentation != Some(generation)
            || self.playback.snapshot().generation != generation
            || self.failure.is_some()
        {
            // 旧首帧或重复确认不能重置时钟，也不能意外推进当前代次。
            return self.unchanged();
        }
        self.pending_presentation = None;
        if let Err(error) = self.playback.set_playing(true, now) {
            return self.failed(error.to_string());
        }
        self.tick(now, false)
    }

    fn await_presentation(&mut self, now: Duration) -> std::result::Result<(), String> {
        self.playback
            .set_playing(false, now)
            .map_err(|error| error.to_string())?;
        self.pending_presentation = Some(self.playback.snapshot().generation);
        self.deadline = None;
        Ok(())
    }

    fn unchanged(&self) -> Tick {
        Tick {
            image: None,
            snapshot: self.snapshot(),
            error: self.failure.clone(),
        }
    }

    fn initialize_format(
        mut playback: AnimationPlayback<S>,
        encoded_bytes: Option<u64>,
        now: Duration,
        format: ImageFormat,
    ) -> std::result::Result<(Self, LoadedImage), String> {
        let frame = playback
            .poll(now)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| {
                tr!("动画没有可显示的帧", "Animation has no displayable frames").to_string()
            })?;
        let frame: FrameRef = frame;
        let dimensions = playback.dimensions();
        let metadata = ImageMetadata {
            format: format.into(),
            encoded_dimensions: Some(dimensions),
            display_dimensions: dimensions,
            orientation: Some(Orientation::NoTransforms),
            encoded_bytes,
            decoded_color: None,
            details: None,
        };
        let thumbnail = thumbnail(frame.image())?;
        let image = present(
            frame,
            metadata,
            thumbnail.clone(),
            Rotation::Identity,
            &playback.cancellation(),
        )?;
        let mut animation = Self {
            playback,
            metadata,
            thumbnail,
            deadline: None,
            failure: None,
            pending_presentation: None,
            rotation: Rotation::Identity,
        };
        animation.schedule(now);
        Ok((animation, image))
    }

    pub(super) fn cancellation(&self) -> Arc<AtomicBool> {
        self.playback.cancellation()
    }

    pub(super) fn snapshot(&self) -> PlaybackSnapshot {
        let mut snapshot = self.playback.snapshot();
        if self.pending_presentation == Some(snapshot.generation)
            && snapshot.state == PlaybackState::Paused
        {
            // 自动播放意图保留；真实时钟等窗口确认这一代首帧已经进入渲染才启动。
            snapshot.state = PlaybackState::Playing;
        }
        if let Some(error) = &self.failure {
            if snapshot.state != PlaybackState::Cancelled {
                snapshot.state = PlaybackState::Failed;
            }
            snapshot.error = Some(error.clone());
        }
        snapshot
    }

    /// 绝对worker时刻；调用方扣除解码、转换和处理其它任务已经花掉的时间。
    pub(super) fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    fn schedule(&mut self, now: Duration) {
        let awaiting_generated_frame = self.pending_presentation.is_some_and(|generation| {
            self.playback
                .frame()
                .is_some_and(|frame| frame.generation() == generation)
        });
        self.deadline = if self.failure.is_some() || awaiting_generated_frame {
            None
        } else {
            // 旧代租约占满时本代还没有首帧可确认，继续按背压退让重试；生成后才等呈现。
            self.playback
                .next_wake()
                .map(|wait| now.saturating_add(wait))
        };
    }

    fn failed(&mut self, error: String) -> Tick {
        self.failure = Some(error.clone());
        self.deadline = None;
        self.pending_presentation = None;
        Tick {
            image: None,
            snapshot: self.snapshot(),
            error: Some(error),
        }
    }

    pub(super) fn tick(&mut self, now: Duration, force_present: bool) -> Tick {
        if let Some(error) = &self.failure {
            return Tick {
                image: None,
                snapshot: self.snapshot(),
                error: Some(error.clone()),
            };
        }
        let frame = match self.playback.poll(now) {
            Ok(frame) => frame.or_else(|| force_present.then(|| self.playback.frame()).flatten()),
            Err(error) => return self.failed(error.to_string()),
        };
        let image = match frame
            .map(|frame| {
                present(
                    frame,
                    self.metadata,
                    self.thumbnail.clone(),
                    self.rotation,
                    &self.playback.cancellation(),
                )
            })
            .transpose()
        {
            Ok(image) => image,
            Err(error) => return self.failed(error),
        };
        // 固定在poll采样时刻，不因呈现转换花时而再次推迟下一帧。
        self.schedule(now);
        Tick {
            image,
            snapshot: self.snapshot(),
            error: None,
        }
    }

    pub(super) fn control(&mut self, action: Action, now: Duration) -> Tick {
        let awaiting_presentation = self.pending_presentation.is_some();
        if matches!(action, Action::Pause | Action::Step | Action::Restart)
            || matches!(action, Action::Toggle) && awaiting_presentation
        {
            self.pending_presentation = None;
        }
        if self.failure.is_some() && !matches!(action, Action::Restart) {
            return self.tick(now, false);
        }
        let previous_generation = self.playback.snapshot().generation;
        let result = match action {
            Action::Toggle if awaiting_presentation => self.playback.set_playing(false, now),
            Action::Toggle => self.playback.toggle(now),
            Action::Pause => self.playback.set_playing(false, now),
            Action::Step => self.playback.step(now),
            Action::Restart => self.playback.restart(now, true),
            Action::Input => self.playback.user_input(now),
            Action::Rotate(rotation) => {
                self.rotation = rotation;
                Ok(())
            }
        };
        if let Err(error) = result {
            return self.failed(error.to_string());
        }
        self.failure = None;
        if self.playback.snapshot().generation != previous_generation {
            // Restart和Ended后的Toggle都会换代；每一代的首帧都必须获得呈现机会。
            if let Err(error) = self.await_presentation(now) {
                return self.failed(error);
            }
        }
        // 旧epoch的交付可能已丢弃；暂停也要把来源真正停住的完整帧重新交付。
        self.tick(now, true)
    }
}

fn present(
    frame: FrameRef,
    mut metadata: ImageMetadata,
    thumbnail: Arc<RenderImage>,
    rotation: Rotation,
    cancel: &AtomicBool,
) -> std::result::Result<LoadedImage, String> {
    let metadata_source = frame.metadata_source().cloned();
    metadata.details = metadata_source.as_ref().map(|source| source.facts());
    let warning = frame.warning().map(str::to_owned);
    let raw = frame.image();
    // CanvasSource消费这一个显示副本；原始完整合成帧随租约留给快照与退役管理。
    let check_cancel = || {
        if cancel.load(Ordering::Relaxed) {
            Err(tr!("动画呈现已取消", "Animation presentation canceled").to_string())
        } else {
            Ok(())
        }
    };
    check_cancel()?;
    let (width, height, bgra) = match rotation {
        Rotation::Identity => (raw.width, raw.height, raw.bgra.clone()),
        _ => {
            let rotated = ImageRotation::new((raw.width, raw.height), rotation)?.rotate_tile(
                Rect::full(raw.width, raw.height),
                &raw.bgra,
                crate::image_decode::MAX_DECODE_BYTES,
                || cancel.load(Ordering::Relaxed),
            )?;
            (rotated.rect.width, rotated.rect.height, rotated.bgra)
        }
    };
    check_cancel()?;
    let image = Image {
        width,
        height,
        bgra,
        readback_ms: raw.readback_ms,
    };
    let (source, previews) = CanvasSource::image(image).map_err(|error| error.to_string())?;
    let tiles = previews
        .into_iter()
        .enumerate()
        .map(|(index, image)| (source.screen_rect(index), image))
        .collect();
    check_cancel()?;
    Ok(LoadedImage {
        metadata_source,
        metadata,
        preview_geometry: None,
        preview_source: None,
        tiles,
        thumbnail,
        raw_frame: Some(frame),
        raw_page: None,
        warning,
        rotation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{borrow::Cow, io::Cursor};

    mod resident {
        include!("image_viewer_animation_resident_tests.rs");
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn animation(wait_for_input: bool) -> (ActiveAnimation<Cursor<Vec<u8>>>, LoadedImage) {
        animation_with_timing(wait_for_input, 10, Some(gif::Repeat::Infinite))
    }

    fn animation_with_timing(
        wait_for_input: bool,
        delay: u16,
        repeat: Option<gif::Repeat>,
    ) -> (ActiveAnimation<Cursor<Vec<u8>>>, LoadedImage) {
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, 1, 1, &[255, 0, 0, 0, 0, 255]).unwrap();
            if let Some(repeat) = repeat {
                encoder.set_repeat(repeat).unwrap();
            }
            for index in 0..2 {
                encoder
                    .write_frame(&gif::Frame {
                        width: 1,
                        height: 1,
                        delay: if index == 0 && wait_for_input {
                            0
                        } else {
                            delay
                        },
                        needs_user_input: index == 0 && wait_for_input,
                        buffer: Cow::Owned(vec![index]),
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        let bytes_len = bytes.len() as u64;
        let playback = GifPlayback::from_reader(
            Cursor::new(bytes),
            GifOptions::default(),
            Arc::new(AtomicBool::new(false)),
            true,
        )
        .unwrap();
        ActiveAnimation::initialize(playback, Some(bytes_len), Duration::ZERO).unwrap()
    }

    fn apng_animation(
        numerator: u16,
        denominator: u16,
    ) -> (ActiveApngAnimation<Cursor<Vec<u8>>>, LoadedImage) {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 1).unwrap();
            encoder.set_frame_delay(numerator, denominator).unwrap();
            let mut writer = encoder.write_header().unwrap();
            for pixels in [
                [255, 0, 0, 255, 0, 255, 0, 255],
                [0, 0, 255, 255, 255, 255, 0, 255],
            ] {
                writer.write_image_data(&pixels).unwrap();
            }
            writer.finish().unwrap();
        }
        let length = bytes.len() as u64;
        let playback = ApngPlayback::from_reader(
            Cursor::new(bytes),
            ApngOptions::default(),
            Arc::new(AtomicBool::new(false)),
            true,
        )
        .unwrap();
        let (mut animation, image) = ActiveApngAnimation::initialize_format(
            playback,
            Some(length),
            Duration::ZERO,
            ImageFormat::Png,
        )
        .unwrap();
        animation.await_presentation(Duration::ZERO).unwrap();
        (animation, image)
    }

    #[test]
    fn image_viewer_animation_deadline_and_pause_keep_the_source_frame() {
        let (mut animation, first) = animation(false);
        assert_eq!(animation.deadline(), Some(ms(100)));
        assert_eq!(first.raw_frame.as_ref().unwrap().index(), 0);
        assert_eq!(
            first.raw_frame.as_ref().unwrap().image().bgra,
            [0, 0, 255, 255]
        );
        assert!(animation.tick(ms(20), false).image.is_none());
        assert_eq!(animation.deadline(), Some(ms(100)));
        let second = animation.tick(ms(100), false).image.unwrap();
        assert_eq!(second.raw_frame.as_ref().unwrap().index(), 1);
        assert!(Arc::ptr_eq(&first.thumbnail, &second.thumbnail));
        let paused = animation.control(Action::Toggle, ms(125));
        assert_eq!(paused.snapshot.state, PlaybackState::Paused);
        assert_eq!(paused.image.unwrap().raw_frame.as_ref().unwrap().index(), 1);
        assert_eq!(animation.deadline(), None);
        let resumed = animation.control(Action::Toggle, ms(1000));
        assert_eq!(resumed.snapshot.state, PlaybackState::Playing);
        assert_eq!(animation.deadline(), Some(ms(1075)));
    }

    #[test]
    fn image_viewer_animation_waits_for_input_and_restarts_with_new_cancellation() {
        let (mut animation, _first) = animation(true);
        assert_eq!(animation.snapshot().state, PlaybackState::WaitingForInput);
        assert_eq!(animation.deadline(), None);
        let entered = animation.control(Action::Input, ms(100));
        assert_eq!(
            entered.image.unwrap().raw_frame.as_ref().unwrap().index(),
            1
        );
        assert_eq!(animation.deadline(), Some(ms(200)));
        let old_cancel = animation.cancellation();
        let restart = animation.control(Action::Restart, ms(150));
        assert_eq!(
            restart.image.unwrap().raw_frame.as_ref().unwrap().index(),
            0
        );
        assert_eq!(animation.deadline(), None);
        assert!(!Arc::ptr_eq(&old_cancel, &animation.cancellation()));
    }

    #[test]
    fn image_viewer_animation_failure_stops_scheduling_until_restart() {
        let (mut animation, _first) = animation(false);
        assert!(animation.tick(ms(100), false).image.is_some());
        let failed = animation.tick(ms(50), false);
        assert_eq!(failed.snapshot.state, PlaybackState::Failed);
        assert!(failed.error.is_some());
        assert_eq!(animation.deadline(), None);
        let restart = animation.control(Action::Restart, ms(101));
        assert!(restart.error.is_none());
        assert_eq!(restart.snapshot.state, PlaybackState::Playing);
        assert!(restart.image.is_some());
        assert_eq!(animation.deadline(), None);
        assert_eq!(animation.presentation_pending(), Some(1));
        assert!(animation.begin(ms(201), 1).error.is_none());
        assert_eq!(animation.deadline(), Some(ms(301)));
    }

    #[test]
    fn image_viewer_animation_snapshot_pause_is_idempotent_after_queued_toggle() {
        let (mut animation, _first) = animation(false);
        assert_eq!(
            animation.control(Action::Toggle, ms(10)).snapshot.state,
            PlaybackState::Paused
        );
        assert_eq!(
            animation.control(Action::Pause, ms(11)).snapshot.state,
            PlaybackState::Paused
        );
        assert_eq!(animation.deadline(), None);
        assert_eq!(
            animation
                .control(Action::Pause, ms(500))
                .snapshot
                .frame_index,
            Some(0)
        );
    }

    #[test]
    fn image_viewer_animation_short_first_frames_wait_for_their_own_presentation() {
        let (mut animation, _first) = animation_with_timing(false, 1, Some(gif::Repeat::Infinite));
        animation.await_presentation(Duration::ZERO).unwrap();
        assert_eq!(animation.presentation_pending(), Some(0));
        assert_eq!(animation.snapshot().state, PlaybackState::Playing);
        assert_eq!(animation.deadline(), None);
        assert!(animation.tick(ms(100), false).image.is_none());
        assert_eq!(animation.snapshot().frame_index, Some(0));
        assert!(animation.begin(ms(100), 99).image.is_none());
        assert_eq!(animation.presentation_pending(), Some(0));
        animation.begin(ms(100), 0);
        assert_eq!(animation.deadline(), Some(ms(110)));
        assert!(animation.tick(ms(109), false).image.is_none());
        assert_eq!(animation.tick(ms(110), false).snapshot.frame_index, Some(1));

        let restart = animation.control(Action::Restart, ms(200));
        assert_eq!(restart.snapshot.frame_index, Some(0));
        assert_eq!(animation.presentation_pending(), Some(1));
        assert_eq!(animation.deadline(), None);
        assert!(animation.tick(ms(300), false).image.is_none());
        animation.begin(ms(300), 0);
        assert_eq!(animation.presentation_pending(), Some(1));
        assert_eq!(animation.deadline(), None);
        animation.begin(ms(300), 1);
        assert_eq!(animation.deadline(), Some(ms(310)));
        // 重复确认不能重置已启动的帧时钟。
        animation.begin(ms(305), 1);
        assert_eq!(animation.deadline(), Some(ms(310)));
        assert_eq!(animation.tick(ms(310), false).snapshot.frame_index, Some(1));
    }

    #[test]
    fn image_viewer_animation_pause_toggle_and_step_cancel_pending_autoplay() {
        for action in [Action::Pause, Action::Toggle, Action::Step] {
            let (mut animation, _first) =
                animation_with_timing(false, 1, Some(gif::Repeat::Infinite));
            animation.await_presentation(Duration::ZERO).unwrap();
            let controlled = animation.control(action, ms(5));
            assert_eq!(controlled.snapshot.state, PlaybackState::Paused);
            let expected_index = u64::from(matches!(action, Action::Step));
            assert_eq!(controlled.snapshot.frame_index, Some(expected_index));
            assert_eq!(animation.presentation_pending(), None);
            assert_eq!(animation.deadline(), None);
            assert_eq!(
                animation.begin(ms(100), 0).snapshot.state,
                PlaybackState::Paused
            );
            assert!(animation.tick(ms(200), false).image.is_none());
            assert_eq!(animation.snapshot().frame_index, Some(expected_index));
        }
    }

    #[test]
    fn image_viewer_animation_ended_toggle_waits_for_the_restarted_generation() {
        let (mut animation, _first) = animation_with_timing(false, 1, None);
        assert_eq!(animation.tick(ms(10), false).snapshot.frame_index, Some(1));
        assert_eq!(
            animation.tick(ms(20), false).snapshot.state,
            PlaybackState::Ended
        );
        let restarted = animation.control(Action::Toggle, ms(30));
        assert_eq!(restarted.snapshot.generation, 1);
        assert_eq!(restarted.snapshot.frame_index, Some(0));
        assert_eq!(restarted.snapshot.state, PlaybackState::Playing);
        assert_eq!(animation.presentation_pending(), Some(1));
        assert_eq!(animation.deadline(), None);
        assert!(animation.tick(ms(100), false).image.is_none());
        animation.begin(ms(100), 0);
        assert_eq!(animation.presentation_pending(), Some(1));
        animation.begin(ms(100), 1);
        assert_eq!(animation.deadline(), Some(ms(110)));
        assert_eq!(animation.tick(ms(110), false).snapshot.frame_index, Some(1));
    }

    #[test]
    fn image_viewer_animation_restart_retries_until_a_first_frame_slot_is_free() {
        let (mut animation, first) = animation(false);
        let mut held = vec![first];
        for now in 1..=3 {
            let tick = animation.control(Action::Step, ms(now));
            assert!(tick.error.is_none());
            held.push(tick.image.unwrap());
        }
        assert_eq!(held.len(), 4);
        let restarted = animation.control(Action::Restart, ms(4));
        assert!(restarted.error.is_none());
        assert!(restarted.image.is_none());
        assert!(restarted.snapshot.waiting_for_outputs);
        assert_eq!(animation.presentation_pending(), Some(1));
        assert_eq!(animation.deadline(), Some(ms(20)));

        drop(held.remove(0));
        let first = animation.tick(ms(20), false);
        assert!(first.error.is_none());
        assert!(!first.snapshot.waiting_for_outputs);
        let frame = first.image.as_ref().unwrap().raw_frame.as_ref().unwrap();
        assert_eq!((frame.generation(), frame.index()), (1, 0));
        assert_eq!(animation.presentation_pending(), Some(1));
        assert_eq!(animation.deadline(), None);
        assert!(animation.tick(ms(100), false).image.is_none());
        assert!(animation.begin(ms(100), 1).error.is_none());
        assert_eq!(animation.presentation_pending(), None);
        assert_eq!(animation.deadline(), Some(ms(200)));
    }

    #[test]
    fn image_viewer_animation_rotation_rebuilds_display_from_the_unrotated_frame() {
        let mut bytes = Vec::new();
        {
            let palette: Vec<u8> = (1..=6).flat_map(|value| [value, 0, 0]).collect();
            let mut encoder = gif::Encoder::new(&mut bytes, 2, 3, &palette).unwrap();
            encoder
                .write_frame(&gif::Frame {
                    width: 2,
                    height: 3,
                    delay: 10,
                    buffer: Cow::Owned(vec![0, 1, 2, 3, 4, 5]),
                    ..Default::default()
                })
                .unwrap();
        }
        let playback = GifPlayback::from_reader(
            Cursor::new(bytes),
            GifOptions::default(),
            Arc::new(AtomicBool::new(false)),
            true,
        )
        .unwrap();
        let (mut animation, first) =
            ActiveAnimation::initialize(playback, None, Duration::ZERO).unwrap();
        for (rotation, dimensions, expected) in [
            (Rotation::R90, (3, 2), [5, 3, 1, 6, 4, 2]),
            (Rotation::R180, (2, 3), [6, 5, 4, 3, 2, 1]),
            (Rotation::R270, (3, 2), [2, 4, 6, 1, 3, 5]),
            (Rotation::Identity, (2, 3), [1, 2, 3, 4, 5, 6]),
        ] {
            let tick = animation.control(Action::Rotate(rotation), Duration::ZERO);
            assert!(tick.error.is_none());
            let image = tick.image.unwrap();
            let rect = image.tiles[0].0;
            assert_eq!((rect.width, rect.height), dimensions);
            let displayed: Vec<_> = image.tiles[0]
                .1
                .as_bytes(0)
                .unwrap()
                .chunks_exact(4)
                .map(|pixel| pixel[2])
                .collect();
            assert_eq!(displayed, expected);
            assert_eq!(image.metadata.display_dimensions, (2, 3));
            assert!(Arc::ptr_eq(&image.thumbnail, &first.thumbnail));
            assert!(Arc::ptr_eq(
                image.raw_frame.as_ref().unwrap(),
                first.raw_frame.as_ref().unwrap()
            ));
            assert_eq!(animation.deadline(), Some(ms(100)));
        }
    }

    #[test]
    fn image_viewer_animation_apng_first_frame_ack_and_rotation_keep_the_raw_source() {
        let (mut animation, first) = apng_animation(1, 1000);
        assert_eq!(first.metadata.format, ImageFormat::Png);
        assert_eq!(first.metadata.decoded_color, None);
        assert!(first.metadata.encoded_bytes.is_some());
        let metadata_source = first.metadata_source.as_ref().unwrap();
        assert_eq!(first.metadata.details, Some(metadata_source.facts()));
        assert_eq!(
            metadata_source.facts().color_conversion,
            crate::image_color_management::ColorConversion::AssumedSrgb
        );
        assert!(Arc::ptr_eq(
            metadata_source,
            first.raw_frame.as_ref().unwrap().metadata_source().unwrap()
        ));
        assert_eq!(animation.snapshot().state, PlaybackState::Playing);
        assert_eq!(animation.presentation_pending(), Some(0));
        assert_eq!(animation.deadline(), None);
        assert!(animation.tick(ms(30), false).image.is_none());
        let rotated = animation.control(Action::Rotate(Rotation::R90), ms(30));
        assert!(rotated.error.is_none());
        let rotated = rotated.image.unwrap();
        assert_eq!(
            (rotated.tiles[0].0.width, rotated.tiles[0].0.height),
            (1, 2)
        );
        assert!(Arc::ptr_eq(
            rotated.raw_frame.as_ref().unwrap(),
            first.raw_frame.as_ref().unwrap()
        ));
        let raw = rotated.raw_frame.as_ref().unwrap();
        assert!(Arc::ptr_eq(
            metadata_source,
            rotated.metadata_source.as_ref().unwrap()
        ));
        assert_eq!((raw.image().width, raw.image().height), (2, 1));
        assert_eq!(raw.image().bgra, [0, 0, 255, 255, 0, 255, 0, 255]);
        assert!(raw.raw_delay_label().contains("1/1000"));
        assert_eq!(raw.timeout(), Some(ms(10)));
        animation.begin(ms(30), 99);
        assert_eq!(animation.deadline(), None);
        animation.begin(ms(30), 0);
        assert_eq!(animation.deadline(), Some(ms(40)));
        assert!(animation.tick(ms(39), false).image.is_none());
        let second = animation.tick(ms(40), false).image.unwrap();
        assert_eq!(second.raw_frame.as_ref().unwrap().index(), 1);
        assert!(Arc::ptr_eq(
            metadata_source,
            second.metadata_source.as_ref().unwrap()
        ));
        assert_eq!(second.metadata.details, Some(metadata_source.facts()));
        assert_eq!((second.tiles[0].0.width, second.tiles[0].0.height), (1, 2));
        assert_eq!(second.metadata.display_dimensions, (2, 1));
    }

    #[test]
    fn image_viewer_animation_apng_zero_denominator_and_restart_pause_keep_intent() {
        let (mut animation, first) = apng_animation(1, 0);
        let label = first.raw_frame.as_ref().unwrap().raw_delay_label();
        assert!(label.contains("/0") && label.contains("100"));
        animation.begin(Duration::ZERO, 0);
        let restarted = animation.control(Action::Restart, ms(1));
        assert_eq!(restarted.snapshot.generation, 1);
        assert_eq!(animation.presentation_pending(), Some(1));
        assert_eq!(animation.deadline(), None);
        let paused = animation.control(Action::Pause, ms(2));
        assert_eq!(paused.snapshot.state, PlaybackState::Paused);
        assert_eq!(animation.presentation_pending(), None);
        assert_eq!(
            animation.begin(ms(30), 1).snapshot.state,
            PlaybackState::Paused
        );
        assert!(animation.tick(ms(100), false).image.is_none());
    }
}
