//! 来源无关的单worker播放时钟；不创建解码线程或额外帧队列。
use crate::image_animation::AnimationError;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const MAX_CATCH_UP: usize = 8;
const MAX_PENDING_STEPS: u32 = 32;
pub(super) const OUTPUT_RETRY: Duration = Duration::from_millis(16);
pub(crate) type PlaybackResult<T> = std::result::Result<T, AnimationError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackState {
    Loading,
    Playing,
    Paused,
    WaitingForInput,
    Ended,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackSnapshot {
    pub state: PlaybackState,
    pub generation: u64,
    pub cycle: Option<u64>,
    pub frame_index: Option<u64>,
    pub frame_elapsed: Duration,
    pub waiting_for_outputs: bool,
    pub omitted_presentations: u64,
    pub error: Option<String>,
}

/// 帧时序与像素/处置分离；时钟只借用完整帧的播放条件。
pub(crate) trait PlaybackFrame {
    fn index(&self) -> u64;
    fn cycle(&self) -> u64;
    fn timeout(&self) -> Option<Duration>;
    fn needs_user_input(&self) -> bool;
    fn ready(&self, elapsed: Duration, user_input: bool) -> bool {
        (self.needs_user_input() && user_input)
            || self.timeout().is_some_and(|timeout| elapsed >= timeout)
    }
}

/// 来源完成格式自身的合成、循环与交付租约，背压不能消费下一帧。
pub(crate) trait PlaybackSource {
    type Frame: PlaybackFrame;
    fn dimensions(&self) -> (u32, u32);
    fn next_frame(&mut self) -> PlaybackResult<Option<Self::Frame>>;
    fn restart(&mut self, cancel: Arc<AtomicBool>) -> PlaybackResult<()>;
}

/// 由唯一解码worker驱动；所有时间来自同一单调时钟，暂停期间不累计帧时长。
pub(crate) struct AnimationPlayback<S: PlaybackSource> {
    source: S,
    cancel: Arc<AtomicBool>,
    current: Option<Arc<S::Frame>>,
    playing: bool,
    elapsed: Duration,
    last_tick: Option<Duration>,
    pending_steps: u32,
    pending_input: bool,
    generation: u64,
    ended: bool,
    cancelled: bool,
    failure: Option<String>,
    waiting_for_outputs: bool,
    omitted_presentations: u64,
}

impl<S: PlaybackSource> AnimationPlayback<S> {
    pub(crate) fn new(source: S, cancel: Arc<AtomicBool>, playing: bool) -> Self {
        Self {
            source,
            cancel,
            current: None,
            playing,
            elapsed: Duration::ZERO,
            last_tick: None,
            pending_steps: 0,
            pending_input: false,
            generation: 0,
            ended: false,
            cancelled: false,
            failure: None,
            waiting_for_outputs: false,
            omitted_presentations: 0,
        }
    }

    pub(crate) fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
    pub(crate) fn frame(&self) -> Option<Arc<S::Frame>> {
        self.current.clone()
    }
    pub(crate) fn dimensions(&self) -> (u32, u32) {
        self.source.dimensions()
    }
    pub(crate) fn source(&self) -> &S {
        &self.source
    }

    fn observe(&mut self, now: Duration) -> PlaybackResult<()> {
        if let Some(last) = self.last_tick {
            let delta = now.checked_sub(last).ok_or_else(|| {
                AnimationError::Invalid(
                    tr!("动画时钟回退", "Animation clock moved backwards").into(),
                )
            })?;
            if self.playing
                && self.current.is_some()
                && !self.ended
                && !self.cancelled
                && self.failure.is_none()
            {
                self.elapsed = self.elapsed.saturating_add(delta);
            }
        }
        self.last_tick = Some(now);
        Ok(())
    }

    fn usable(&self) -> PlaybackResult<()> {
        if self.cancelled || self.failure.is_some() || self.cancel.load(Ordering::Relaxed) {
            Err(AnimationError::RequiresRestart)
        } else {
            Ok(())
        }
    }

    pub(crate) fn set_playing(&mut self, playing: bool, now: Duration) -> PlaybackResult<()> {
        self.usable()?;
        self.observe(now)?;
        if playing && self.ended {
            return self.restart(now, true);
        }
        self.playing = playing;
        if !playing {
            self.pending_input = false;
            self.pending_steps = 0;
            self.waiting_for_outputs = false;
        }
        Ok(())
    }

    pub(crate) fn toggle(&mut self, now: Duration) -> PlaybackResult<()> {
        self.set_playing(!self.playing || self.ended, now)
    }

    /// 手动单步先暂停；请求有界计数，避免快速按键积压无限工作。
    pub(crate) fn step(&mut self, now: Duration) -> PlaybackResult<()> {
        self.usable()?;
        self.observe(now)?;
        self.playing = false;
        self.pending_input = false;
        if !self.ended {
            self.pending_steps = self.pending_steps.saturating_add(1).min(MAX_PENDING_STEPS);
        }
        Ok(())
    }

    pub(crate) fn user_input(&mut self, now: Duration) -> PlaybackResult<()> {
        self.usable()?;
        self.observe(now)?;
        if self.playing
            && self
                .current
                .as_ref()
                .is_some_and(|frame| frame.needs_user_input())
        {
            self.pending_input = true;
        }
        Ok(())
    }

    /// 新取消令牌隔离旧代次；旧画面/租约由调用方退役，核心继续计入预算。
    pub(crate) fn restart(&mut self, now: Duration, autoplay: bool) -> PlaybackResult<()> {
        self.observe(now)?;
        self.current = None;
        self.playing = false;
        self.pending_steps = 0;
        self.pending_input = false;
        self.elapsed = Duration::ZERO;
        self.ended = false;
        self.waiting_for_outputs = false;
        self.cancel = Arc::new(AtomicBool::new(false));
        if let Err(error) = self.source.restart(self.cancel.clone()) {
            self.failure = Some(error.to_string());
            self.cancelled = matches!(error, AnimationError::Cancelled);
            return Err(error);
        }
        self.generation = self.generation.checked_add(1).ok_or_else(|| {
            AnimationError::Invalid(tr!("动画代次溢出", "Animation generation overflow").into())
        })?;
        self.failure = None;
        self.cancelled = false;
        self.playing = autoplay;
        Ok(())
    }

    fn stop_cancelled(&mut self) {
        self.cancelled = true;
        self.playing = false;
        self.pending_steps = 0;
        self.pending_input = false;
        self.waiting_for_outputs = false;
        self.current = None;
    }

    /// 单次最多合成8帧，处置仍逐帧执行；仅返回最新画面，中间帧立即归还租约。
    pub(crate) fn poll(&mut self, now: Duration) -> PlaybackResult<Option<Arc<S::Frame>>> {
        self.observe(now)?;
        if self.cancelled {
            return Ok(None);
        }
        if self.cancel.load(Ordering::Relaxed) {
            self.stop_cancelled();
            return Err(AnimationError::Cancelled);
        }
        if self.failure.is_some() {
            return Err(AnimationError::RequiresRestart);
        }
        if self.ended {
            return Ok(None);
        }
        let mut newest = None;
        for _ in 0..MAX_CATCH_UP {
            let initial = self.current.is_none();
            let manual = self.pending_steps > 0;
            let ready = self.playing
                && self
                    .current
                    .as_ref()
                    .is_some_and(|frame| frame.ready(self.elapsed, self.pending_input));
            if !initial && !manual && !ready {
                break;
            }
            let carry = if initial || manual {
                Duration::ZERO
            } else {
                self.current
                    .as_ref()
                    .and_then(|frame| frame.timeout())
                    .map_or(Duration::ZERO, |timeout| {
                        self.elapsed.saturating_sub(timeout)
                    })
            };
            match self.source.next_frame() {
                Ok(Some(frame)) => {
                    let frame = Arc::new(frame);
                    if newest.is_some() {
                        self.omitted_presentations = self.omitted_presentations.saturating_add(1);
                    }
                    self.current = Some(frame.clone());
                    newest = Some(frame);
                    self.elapsed = carry;
                    self.waiting_for_outputs = false;
                    self.pending_input = false;
                    if manual {
                        self.pending_steps -= 1;
                    }
                    // 首帧和手动帧总有一次呈现机会，后续工作让出给主图/取消。
                    if initial || manual {
                        break;
                    }
                }
                Ok(None) => {
                    self.ended = true;
                    self.playing = false;
                    self.pending_steps = 0;
                    self.pending_input = false;
                    self.waiting_for_outputs = false;
                    break;
                }
                Err(AnimationError::Backpressure) => {
                    self.waiting_for_outputs = true;
                    break;
                }
                Err(AnimationError::Cancelled) => {
                    self.stop_cancelled();
                    return Err(AnimationError::Cancelled);
                }
                Err(error) => {
                    self.failure = Some(error.to_string());
                    self.playing = false;
                    self.pending_steps = 0;
                    self.pending_input = false;
                    return Err(error);
                }
            }
        }
        Ok(newest)
    }

    /// 相对等待时间；None表示暂停、等输入、结束或终态，避免等待输入时空转。
    pub(crate) fn next_wake(&self) -> Option<Duration> {
        if self.cancelled || self.failure.is_some() || self.ended {
            return None;
        }
        if self.waiting_for_outputs {
            return Some(OUTPUT_RETRY);
        }
        if self.current.is_none() || self.pending_steps > 0 {
            return Some(Duration::ZERO);
        }
        if !self.playing {
            return None;
        }
        let frame = self.current.as_ref()?;
        if frame.needs_user_input() && self.pending_input {
            return Some(Duration::ZERO);
        }
        frame
            .timeout()
            .map(|timeout| timeout.saturating_sub(self.elapsed))
    }

    pub(crate) fn snapshot(&self) -> PlaybackSnapshot {
        let state = if self.cancelled {
            PlaybackState::Cancelled
        } else if self.failure.is_some() {
            PlaybackState::Failed
        } else if self.ended {
            PlaybackState::Ended
        } else if self.current.is_none() {
            PlaybackState::Loading
        } else if !self.playing {
            PlaybackState::Paused
        } else if self
            .current
            .as_ref()
            .is_some_and(|frame| frame.needs_user_input() && frame.timeout().is_none())
        {
            PlaybackState::WaitingForInput
        } else {
            PlaybackState::Playing
        };
        PlaybackSnapshot {
            state,
            generation: self.generation,
            cycle: self.current.as_ref().map(|frame| frame.cycle()),
            frame_index: self.current.as_ref().map(|frame| frame.index()),
            frame_elapsed: self.elapsed,
            waiting_for_outputs: self.waiting_for_outputs,
            omitted_presentations: self.omitted_presentations,
            error: self.failure.clone(),
        }
    }
}
