//! APNG接入共用播放时钟；原始延时分数仍由合成来源完整保留。
use crate::{
    image_animation::AnimationError,
    image_animation_playback::{
        AnimationPlayback, PlaybackFrame, PlaybackSource, RetainedPlaybackSource,
    },
    image_apng::{ApngError, ApngFrame, ApngOptions, ApngSource},
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
pub(crate) type ApngPlayback<R> = AnimationPlayback<ApngSource<R>>;

impl<R: Read + Seek> RetainedPlaybackSource for ApngSource<R> {
    fn retained_bytes(&self) -> u64 {
        ApngSource::retained_bytes(self)
    }
    fn frame_retained_bytes(frame: &Self::Frame) -> u64 {
        ApngFrame::retained_bytes(frame)
    }
}

fn playback_error(error: ApngError) -> AnimationError {
    match error {
        ApngError::Cancelled => AnimationError::Cancelled,
        ApngError::Backpressure => AnimationError::Backpressure,
        ApngError::RequiresRestart => AnimationError::RequiresRestart,
        error => AnimationError::Invalid(error.to_string()),
    }
}

impl PlaybackFrame for ApngFrame {
    fn index(&self) -> u64 {
        self.index
    }
    fn cycle(&self) -> u64 {
        self.cycle
    }
    fn timeout(&self) -> Option<Duration> {
        // 这是播放器的最小10ms策略；不改来源中的分子、分母或实际换算延时。
        Some(self.timing.delay.max(MIN_FRAME_DELAY))
    }
    fn needs_user_input(&self) -> bool {
        false
    }
}

impl<R: Read + Seek> PlaybackSource for ApngSource<R> {
    type Frame = ApngFrame;
    fn dimensions(&self) -> (u32, u32) {
        ApngSource::dimensions(self)
    }
    fn next_frame(&mut self) -> PlaybackResult<Option<Self::Frame>> {
        ApngSource::next_frame(self).map_err(playback_error)
    }
    fn restart(&mut self, cancel: Arc<AtomicBool>) -> PlaybackResult<()> {
        ApngSource::restart(self, cancel).map_err(playback_error)
    }
}

impl AnimationPlayback<ApngSource<File>> {
    pub(crate) fn open(
        path: &Path,
        options: ApngOptions,
        cancel: Arc<AtomicBool>,
        autoplay: bool,
    ) -> PlaybackResult<Self> {
        Self::check_apng_options(options)?;
        let source = ApngSource::open(path, options, cancel.clone()).map_err(playback_error)?;
        Ok(Self::new(source, cancel, autoplay))
    }
}

impl<R: Read + Seek> AnimationPlayback<ApngSource<R>> {
    pub(crate) fn from_reader(
        reader: R,
        options: ApngOptions,
        cancel: Arc<AtomicBool>,
        autoplay: bool,
    ) -> PlaybackResult<Self> {
        Self::check_apng_options(options)?;
        let source =
            ApngSource::from_reader(reader, options, cancel.clone()).map_err(playback_error)?;
        Ok(Self::new(source, cancel, autoplay))
    }

    fn check_apng_options(options: ApngOptions) -> PlaybackResult<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_animation_playback::PlaybackState;
    use std::{io::Cursor, sync::atomic::Ordering};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn fixture(plays: u32, numerator: u16, denominator: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, plays).unwrap();
            encoder.set_frame_delay(numerator, denominator).unwrap();
            let mut writer = encoder.write_header().unwrap();
            for pixels in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                writer.write_image_data(&pixels).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    fn player(
        plays: u32,
        numerator: u16,
        denominator: u16,
        slots: usize,
    ) -> ApngPlayback<Cursor<Vec<u8>>> {
        ApngPlayback::from_reader(
            Cursor::new(fixture(plays, numerator, denominator)),
            ApngOptions {
                max_output_frames: slots,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
            true,
        )
        .unwrap()
    }

    #[test]
    fn image_apng_playback_short_fraction_keeps_raw_timing_and_uses_playback_minimum() {
        let mut player = player(1, 1, 1000, 4);
        let first = player.poll(Duration::ZERO).unwrap().unwrap();
        assert_eq!(first.image().bgra, [0, 0, 255, 255]);
        assert_eq!(
            (first.timing.numerator, first.timing.denominator),
            (1, 1000)
        );
        assert_eq!(first.timing.delay, ms(1));
        assert_eq!(player.next_wake(), Some(ms(10)));
        assert!(player.poll(ms(9)).unwrap().is_none());
        assert_eq!(
            player.poll(ms(10)).unwrap().unwrap().image().bgra,
            [255, 0, 0, 255]
        );
        assert!(player.poll(ms(20)).unwrap().is_none());
        assert_eq!(player.snapshot().state, PlaybackState::Ended);
        assert_eq!(player.frame().unwrap().index, 1);
        player.toggle(ms(30)).unwrap();
        let restarted = player.poll(ms(30)).unwrap().unwrap();
        assert_eq!((restarted.generation, restarted.index), (1, 0));
    }

    #[test]
    fn image_apng_playback_pause_step_and_restart_keep_cross_generation_backpressure() {
        let mut player = player(0, 1, 10, 2);
        let first = player.poll(Duration::ZERO).unwrap().unwrap();
        player.set_playing(false, ms(40)).unwrap();
        assert!(player.poll(ms(500)).unwrap().is_none());
        assert_eq!(player.snapshot().frame_elapsed, ms(40));
        player.set_playing(true, ms(500)).unwrap();
        assert!(player.poll(ms(559)).unwrap().is_none());
        let second = player.poll(ms(560)).unwrap().unwrap();
        player.step(ms(560)).unwrap();
        assert!(player.poll(ms(560)).unwrap().is_none());
        assert!(player.snapshot().waiting_for_outputs);
        assert_eq!(player.next_wake(), Some(ms(16)));
        player.restart(ms(570), false).unwrap();
        assert!(player.poll(ms(570)).unwrap().is_none());
        assert!(player.snapshot().waiting_for_outputs);
        drop(first);
        let restarted = player.poll(ms(586)).unwrap().unwrap();
        assert_eq!((restarted.generation, restarted.index), (1, 0));
        assert_eq!(player.snapshot().state, PlaybackState::Paused);
        assert_eq!(player.next_wake(), None);
        player.step(ms(600)).unwrap();
        assert!(player.poll(ms(600)).unwrap().is_none());
        drop(second);
        assert_eq!(player.poll(ms(616)).unwrap().unwrap().index, 1);
        assert_eq!(player.snapshot().state, PlaybackState::Paused);
    }

    #[test]
    fn image_apng_playback_cancel_maps_to_shared_state_and_restart_replaces_token() {
        let mut player = player(0, 1, 10, 4);
        drop(player.poll(Duration::ZERO).unwrap());
        let old = player.cancellation();
        old.store(true, Ordering::Relaxed);
        assert!(matches!(player.poll(ms(1)), Err(AnimationError::Cancelled)));
        assert_eq!(player.snapshot().state, PlaybackState::Cancelled);
        player.restart(ms(2), true).unwrap();
        assert!(!Arc::ptr_eq(&old, &player.cancellation()));
        assert_eq!(player.poll(ms(2)).unwrap().unwrap().generation, 1);
        assert_eq!(player.snapshot().state, PlaybackState::Playing);
        assert_eq!(player.next_wake(), Some(ms(100)));
    }

    #[test]
    fn image_apng_playback_rejects_one_slot_before_reading_source() {
        struct Unreadable;
        impl Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("one-slot playback must be rejected before reading")
            }
        }
        impl Seek for Unreadable {
            fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
                panic!("one-slot playback must be rejected before seeking")
            }
        }
        let result = ApngPlayback::from_reader(
            Unreadable,
            ApngOptions {
                max_output_frames: 1,
                ..Default::default()
            },
            Arc::new(AtomicBool::new(false)),
            true,
        );
        assert!(matches!(result, Err(AnimationError::Invalid(_))));
    }
}
