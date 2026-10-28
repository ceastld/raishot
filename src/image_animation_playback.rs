//! GIF单worker控制桥；格式合成留在来源，共用来源无关播放时钟。
#[path = "image_animation_clock.rs"]
mod clock;
use crate::image_animation::{AnimationError, GifFrame, GifOptions, GifSource};
#[cfg(test)]
use clock::OUTPUT_RETRY;
pub(crate) use clock::{
    AnimationPlayback, PlaybackFrame, PlaybackSnapshot, PlaybackSource, PlaybackState,
};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::{
    fs::File,
    io::{Read, Seek},
    path::Path,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

type PlaybackResult<T> = std::result::Result<T, AnimationError>;
pub(crate) type GifPlayback<R> = AnimationPlayback<GifSource<R>>;

/// 暂停来源的自有缓冲与当前帧分开计费，不把未使用的输出名额当成已驻留像素。
pub(crate) trait RetainedPlaybackSource: PlaybackSource {
    fn retained_bytes(&self) -> u64;
    fn frame_retained_bytes(frame: &Self::Frame) -> u64;
}

impl<S: RetainedPlaybackSource> AnimationPlayback<S> {
    /// 当前帧可能与UI共享；保守重复计费，避免未消费回执前漏掉worker的新帧。
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.source()
            .retained_bytes()
            .saturating_add(self.frame().as_deref().map_or(0, S::frame_retained_bytes))
    }
}

impl<R: Read + Seek> RetainedPlaybackSource for GifSource<R> {
    fn retained_bytes(&self) -> u64 {
        GifSource::retained_bytes(self)
    }
    fn frame_retained_bytes(frame: &Self::Frame) -> u64 {
        GifFrame::retained_bytes(frame)
    }
}

impl PlaybackFrame for GifFrame {
    fn index(&self) -> u64 {
        self.index
    }
    fn cycle(&self) -> u64 {
        self.cycle
    }
    fn timeout(&self) -> Option<Duration> {
        self.timing.timeout()
    }
    fn needs_user_input(&self) -> bool {
        self.timing.needs_user_input
    }
    fn ready(&self, elapsed: Duration, user_input: bool) -> bool {
        self.timing.ready(elapsed, user_input)
    }
}
impl<R: Read + Seek> PlaybackSource for GifSource<R> {
    type Frame = GifFrame;
    fn dimensions(&self) -> (u32, u32) {
        GifSource::dimensions(self)
    }
    fn next_frame(&mut self) -> PlaybackResult<Option<Self::Frame>> {
        GifSource::next_frame(self)
    }
    fn restart(&mut self, cancel: Arc<AtomicBool>) -> PlaybackResult<()> {
        GifSource::restart(self, cancel)
    }
}
impl AnimationPlayback<GifSource<File>> {
    pub(crate) fn open(
        path: &Path,
        options: GifOptions,
        cancel: Arc<AtomicBool>,
        autoplay: bool,
    ) -> PlaybackResult<Self> {
        Self::check_options(options)?;
        let source = GifSource::open(path, options, cancel.clone())?;
        Ok(Self::new(source, cancel, autoplay))
    }
}
impl<R: Read + Seek> AnimationPlayback<GifSource<R>> {
    pub(crate) fn from_reader(
        reader: R,
        options: GifOptions,
        cancel: Arc<AtomicBool>,
        autoplay: bool,
    ) -> PlaybackResult<Self> {
        Self::check_options(options)?;
        let source = GifSource::from_reader(reader, options, cancel.clone())?;
        Ok(Self::new(source, cancel, autoplay))
    }
    fn check_options(options: GifOptions) -> PlaybackResult<()> {
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
    use std::{borrow::Cow, io::Cursor};

    fn fixture(repeat: Option<gif::Repeat>, wait_for_input: bool) -> Vec<u8> {
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
                        delay: if index == 0 && wait_for_input { 0 } else { 10 },
                        needs_user_input: index == 0 && wait_for_input,
                        dispose: gif::DisposalMethod::Keep,
                        buffer: Cow::Owned(vec![index as u8]),
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        bytes
    }

    fn player(
        repeat: Option<gif::Repeat>,
        waiting: bool,
        autoplay: bool,
    ) -> GifPlayback<Cursor<Vec<u8>>> {
        GifPlayback::from_reader(
            Cursor::new(fixture(repeat, waiting)),
            GifOptions::default(),
            Arc::new(AtomicBool::new(false)),
            autoplay,
        )
        .unwrap()
    }
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn image_animation_playback_real_source_eof_preserves_frame_and_play_restarts() {
        let mut player = player(None, false, true);
        assert_eq!(
            player.poll(ms(0)).unwrap().unwrap().image().bgra,
            [0, 0, 255, 255]
        );
        assert!(player.poll(ms(99)).unwrap().is_none());
        assert_eq!(
            player.poll(ms(100)).unwrap().unwrap().image().bgra,
            [255, 0, 0, 255]
        );
        assert!(player.poll(ms(200)).unwrap().is_none());
        assert_eq!(player.snapshot().state, PlaybackState::Ended);
        assert_eq!(player.frame().unwrap().index, 1);
        assert_eq!(player.next_wake(), None);
        player.toggle(ms(300)).unwrap();
        let first = player.poll(ms(300)).unwrap().unwrap();
        assert_eq!((first.generation, first.index), (1, 0));
    }

    #[test]
    fn image_animation_playback_pause_resume_excludes_paused_wall_time() {
        let mut player = player(Some(gif::Repeat::Infinite), false, true);
        drop(player.poll(ms(0)).unwrap());
        player.set_playing(false, ms(40)).unwrap();
        assert!(player.poll(ms(500)).unwrap().is_none());
        assert_eq!(player.snapshot().frame_elapsed, ms(40));
        player.set_playing(true, ms(500)).unwrap();
        assert!(player.poll(ms(559)).unwrap().is_none());
        assert_eq!(player.poll(ms(560)).unwrap().unwrap().index, 1);
    }

    #[test]
    fn image_animation_playback_manual_steps_are_paused_and_counted() {
        let mut player = player(Some(gif::Repeat::Infinite), false, false);
        assert_eq!(player.poll(ms(0)).unwrap().unwrap().index, 0);
        player.step(ms(10)).unwrap();
        player.step(ms(10)).unwrap();
        assert_eq!(player.poll(ms(10)).unwrap().unwrap().index, 1);
        let next = player.poll(ms(10)).unwrap().unwrap();
        assert_eq!((next.cycle, next.index), (1, 0));
        assert_eq!(player.snapshot().state, PlaybackState::Paused);
        assert!(player.poll(ms(5000)).unwrap().is_none());
    }

    #[test]
    fn image_animation_playback_user_input_wait_has_no_busy_timer() {
        let mut player = player(None, true, true);
        drop(player.poll(ms(0)).unwrap());
        assert_eq!(player.snapshot().state, PlaybackState::WaitingForInput);
        assert_eq!(player.next_wake(), None);
        assert!(player.poll(ms(86_400_000)).unwrap().is_none());
        player.user_input(ms(86_400_000)).unwrap();
        assert_eq!(player.next_wake(), Some(Duration::ZERO));
        assert_eq!(player.poll(ms(86_400_000)).unwrap().unwrap().index, 1);
        assert_eq!(player.next_wake(), Some(ms(100)));
    }

    #[test]
    fn image_animation_playback_catchup_yields_and_does_not_skip_composition() {
        let mut player = player(Some(gif::Repeat::Infinite), false, true);
        drop(player.poll(ms(0)).unwrap());
        let frame = player.poll(ms(1000)).unwrap().unwrap();
        assert_eq!((frame.cycle, frame.index), (4, 0));
        assert_eq!(player.snapshot().omitted_presentations, 7);
        assert_eq!(player.next_wake(), Some(Duration::ZERO));
        let frame = player.poll(ms(1000)).unwrap().unwrap();
        assert_eq!((frame.cycle, frame.index), (5, 0));
        assert_eq!(player.snapshot().omitted_presentations, 8);
    }

    #[test]
    fn image_animation_playback_output_backpressure_recovers_without_poison() {
        let mut player = player(Some(gif::Repeat::Infinite), false, true);
        let mut held = Vec::new();
        for tick in [0, 100, 200, 300] {
            held.push(player.poll(ms(tick)).unwrap().unwrap());
        }
        assert!(player.poll(ms(400)).unwrap().is_none());
        assert!(player.snapshot().waiting_for_outputs);
        assert_eq!(player.next_wake(), Some(OUTPUT_RETRY));
        player.set_playing(false, ms(400)).unwrap();
        assert_eq!(player.next_wake(), None);
        player.set_playing(true, ms(400)).unwrap();
        drop(held.remove(0));
        let frame = player.poll(ms(400)).unwrap().unwrap();
        assert_eq!((frame.cycle, frame.index), (2, 0));
        assert!(!player.snapshot().waiting_for_outputs);
        assert_eq!(player.snapshot().state, PlaybackState::Playing);
    }

    #[test]
    fn image_animation_playback_cancel_and_restart_isolate_old_token() {
        let mut player = player(Some(gif::Repeat::Infinite), false, true);
        drop(player.poll(ms(0)).unwrap());
        let old_cancel = player.cancellation();
        old_cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            player.poll(ms(10)),
            Err(AnimationError::Cancelled)
        ));
        assert!(player.poll(ms(20)).unwrap().is_none());
        player.restart(ms(30), false).unwrap();
        assert!(!Arc::ptr_eq(&old_cancel, &player.cancellation()));
        assert_eq!(player.poll(ms(30)).unwrap().unwrap().generation, 1);
        assert_eq!(player.snapshot().state, PlaybackState::Paused);
        assert!(player.poll(ms(40)).unwrap().is_none());
    }

    #[test]
    fn image_animation_playback_clock_regression_does_not_consume_source() {
        let mut player = player(None, false, true);
        drop(player.poll(ms(10)).unwrap());
        assert!(matches!(
            player.poll(ms(9)),
            Err(AnimationError::Invalid(_))
        ));
        assert_eq!(player.frame().unwrap().index, 0);
        assert_eq!(player.snapshot().state, PlaybackState::Playing);
        assert_eq!(player.poll(ms(110)).unwrap().unwrap().index, 1);
    }

    #[test]
    fn image_animation_playback_rejects_one_output_slot_before_decoding() {
        let options = GifOptions {
            max_output_frames: 1,
            ..Default::default()
        };
        let result = GifPlayback::from_reader(
            Cursor::new(fixture(None, false)),
            options,
            Arc::new(AtomicBool::new(false)),
            true,
        );
        assert!(matches!(result, Err(AnimationError::Invalid(_))));
    }
}
