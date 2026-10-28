//! WebP动画复用单worker播放时钟；来源中的原始毫秒延时保持不变。
use super::webp_source::{WebpError, WebpFrame, WebpOptions, WebpSource};
use crate::{
    image_animation::AnimationError,
    image_animation_playback::{
        AnimationPlayback, PlaybackFrame, PlaybackSource, RetainedPlaybackSource,
    },
};
use std::{
    fs::File,
    io::{Read, Seek},
    path::Path,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

const MIN_FRAME_DELAY: Duration = Duration::from_millis(10);
type PlaybackResult<T> = std::result::Result<T, AnimationError>;
pub(crate) type WebpPlayback<R> = AnimationPlayback<WebpSource<R>>;

impl<R: Read + Seek> RetainedPlaybackSource for WebpSource<R> {
    fn retained_bytes(&self) -> u64 {
        WebpSource::retained_bytes(self)
    }
    fn frame_retained_bytes(frame: &Self::Frame) -> u64 {
        WebpFrame::retained_bytes(frame)
    }
}

fn playback_error(error: WebpError) -> AnimationError {
    match error {
        WebpError::Cancelled => AnimationError::Cancelled,
        WebpError::Backpressure => AnimationError::Backpressure,
        WebpError::RequiresRestart => AnimationError::RequiresRestart,
        error => AnimationError::Invalid(error.to_string()),
    }
}

impl PlaybackFrame for WebpFrame {
    fn index(&self) -> u64 {
        self.index
    }

    fn cycle(&self) -> u64 {
        self.cycle
    }

    fn timeout(&self) -> Option<Duration> {
        // 最小10ms只约束播放时钟，不改来源的raw_delay_ms及delay。
        Some(self.timing.delay.max(MIN_FRAME_DELAY))
    }

    fn needs_user_input(&self) -> bool {
        false
    }
}

impl<R: Read + Seek> PlaybackSource for WebpSource<R> {
    type Frame = WebpFrame;

    fn dimensions(&self) -> (u32, u32) {
        WebpSource::dimensions(self)
    }

    fn next_frame(&mut self) -> PlaybackResult<Option<Self::Frame>> {
        WebpSource::next_frame(self).map_err(playback_error)
    }

    fn restart(&mut self, cancel: Arc<AtomicBool>) -> PlaybackResult<()> {
        WebpSource::restart(self, cancel).map_err(playback_error)
    }
}

impl AnimationPlayback<WebpSource<File>> {
    pub(crate) fn open(
        path: &Path,
        options: WebpOptions,
        cancel: Arc<AtomicBool>,
        autoplay: bool,
    ) -> PlaybackResult<Self> {
        Self::check_webp_options(options)?;
        let source = WebpSource::open(path, options, cancel.clone()).map_err(playback_error)?;
        Ok(Self::new(source, cancel, autoplay))
    }
}

impl<R: Read + Seek> AnimationPlayback<WebpSource<R>> {
    pub(crate) fn from_reader(
        reader: R,
        options: WebpOptions,
        cancel: Arc<AtomicBool>,
        autoplay: bool,
    ) -> PlaybackResult<Self> {
        Self::check_webp_options(options)?;
        let source =
            WebpSource::from_reader(reader, options, cancel.clone()).map_err(playback_error)?;
        Ok(Self::new(source, cancel, autoplay))
    }

    fn check_webp_options(options: WebpOptions) -> PlaybackResult<()> {
        if options.max_output_frames < 2 {
            Err(AnimationError::Invalid(
                tr!(
                    "动画播放至少需要两个交付帧名额",
                    "Animation playback needs at least two frame slots"
                )
                .into(),
            ))
        } else {
            Ok(())
        }
    }
}
