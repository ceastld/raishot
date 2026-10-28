//! 片段采样只保留覆盖起点的画面和一个前瞻帧，输出时钟从零开始。
use crate::{
    media_foundation::{DecodedFrame, VideoReader},
    Result,
};
use std::time::Duration;

pub(super) struct RangeFrames {
    start: Duration,
    end: Duration,
    interval: Duration,
    next_time: Duration,
    initialized: bool,
    lookahead: Option<DecodedFrame>,
}

impl RangeFrames {
    pub(super) fn new(start: Duration, end: Duration, fps: u32) -> Result<Self> {
        if start >= end || !(1..=15).contains(&fps) {
            return Err(tr!(
                "GIF 片段采样参数无效",
                "Invalid GIF segment sampling options"
            )
            .into());
        }
        Ok(Self {
            start,
            end,
            // 来源PTS为100ns刻度；向下取同一刻度，避免30fps的66.6666ms帧
            // 因略早于浮点间隔而误丢，使15fps实际退化为10fps。
            interval: Duration::from_nanos(u64::from(10_000_000 / fps) * 100),
            next_time: start,
            initialized: false,
            lookahead: None,
        })
    }

    pub(super) fn next(
        &mut self,
        reader: &mut VideoReader,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<Option<DecodedFrame>> {
        self.next_with(&mut |minimum| reader.next_at(minimum), proceed)
    }

    fn next_with(
        &mut self,
        read: &mut dyn FnMut(Duration) -> Result<Option<DecodedFrame>>,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<Option<DecodedFrame>> {
        if !self.initialized {
            let mut covering = None;
            // 顺序读取到起点右侧，不能用 next_at(start) 丢掉仍覆盖起点的前一帧。
            // 每个系统读取之间都检查取消，不积攒起点之前的画面。
            loop {
                check_proceed(proceed)?;
                match read(Duration::ZERO)? {
                    Some(frame) if frame.time <= self.start => covering = Some(frame),
                    next => {
                        self.lookahead = next;
                        break;
                    }
                }
            }
            check_proceed(proceed)?;
            let mut frame = covering.ok_or(tr!(
                "GIF 片段起点没有可用画面",
                "No video frame covers the GIF segment start"
            ))?;
            self.initialized = true;
            self.next_time = self.start.checked_add(self.interval).unwrap_or(self.end);
            frame.time = Duration::ZERO;
            return Ok(Some(frame));
        }
        if self.next_time >= self.end {
            return Ok(None);
        }
        loop {
            check_proceed(proceed)?;
            let next = match self.lookahead.take() {
                Some(frame) => Some(frame),
                None => read(self.next_time)?,
            };
            let Some(mut frame) = next else {
                return Ok(None);
            };
            if frame.time >= self.end {
                return Ok(None);
            }
            // 系统读取允许minimum之前1ms；新范围接口严格遵守所选采样间隔。
            if frame.time < self.next_time {
                continue;
            }
            self.next_time = frame.time.checked_add(self.interval).unwrap_or(self.end);
            frame.time -= self.start;
            return Ok(Some(frame));
        }
    }
}

fn check_proceed(proceed: &mut dyn FnMut() -> bool) -> Result<()> {
    if proceed() {
        Ok(())
    } else {
        Err(tr!("已取消 GIF 转换", "GIF conversion canceled").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn source(times: &[u64]) -> impl FnMut(Duration) -> Result<Option<DecodedFrame>> {
        let mut frames: VecDeque<_> = times
            .iter()
            .map(|&time| DecodedFrame {
                time: Duration::from_millis(time),
                bgra: vec![(time / 100) as u8],
            })
            .collect();
        move |minimum| {
            while let Some(frame) = frames.pop_front() {
                if frame.time + Duration::from_millis(1) >= minimum {
                    return Ok(Some(frame));
                }
            }
            Ok(None)
        }
    }

    #[test]
    fn gif_range_keeps_covering_frame_rebases_and_excludes_end() {
        let mut frames =
            RangeFrames::new(Duration::from_millis(125), Duration::from_millis(800), 5).unwrap();
        let mut read = source(&[0, 100, 200, 300, 400, 500, 600, 700, 800, 900]);
        let mut selected = Vec::new();
        while let Some(frame) = frames.next_with(&mut read, &mut || true).unwrap() {
            selected.push((frame.time.as_millis(), frame.bgra[0]));
        }
        assert_eq!(selected, [(0, 1), (275, 4), (475, 6)]);
    }

    #[test]
    fn gif_range_inside_one_source_frame_holds_it_for_the_whole_segment() {
        let mut frames =
            RangeFrames::new(Duration::from_millis(125), Duration::from_millis(150), 15).unwrap();
        let mut read = source(&[0, 100, 200]);
        let frame = frames.next_with(&mut read, &mut || true).unwrap().unwrap();
        assert_eq!(frame.time, Duration::ZERO);
        assert_eq!(frame.bgra, [1]);
        assert!(frames.next_with(&mut read, &mut || true).unwrap().is_none());
    }

    #[test]
    fn gif_range_checks_cancel_while_locating_start_and_rejects_missing_cover() {
        let mut frames =
            RangeFrames::new(Duration::from_secs(10), Duration::from_secs(11), 15).unwrap();
        let mut read = source(&[0, 100, 200, 300]);
        let mut calls = 0;
        assert!(frames
            .next_with(&mut read, &mut || {
                calls += 1;
                calls < 3
            })
            .is_err());
        let mut frames = RangeFrames::new(Duration::ZERO, Duration::from_secs(1), 15).unwrap();
        assert!(frames.next_with(&mut source(&[100]), &mut || true).is_err());
    }

    #[test]
    fn gif_range_source_ticks_keep_every_second_thirty_fps_frame() {
        let mut source: VecDeque<_> = (0..30_u64)
            .map(|index| DecodedFrame {
                time: Duration::from_nanos((index * 10_000_000 / 30) * 100),
                bgra: vec![index as u8],
            })
            .collect();
        let mut read = |minimum| {
            while let Some(frame) = source.pop_front() {
                if frame.time + Duration::from_millis(1) >= minimum {
                    return Ok(Some(frame));
                }
            }
            Ok(None)
        };
        let mut frames = RangeFrames::new(Duration::ZERO, Duration::from_secs(1), 15).unwrap();
        let mut selected = Vec::new();
        while let Some(frame) = frames.next_with(&mut read, &mut || true).unwrap() {
            selected.push(frame.bgra[0]);
        }
        assert_eq!(selected, (0..30_u8).step_by(2).collect::<Vec<_>>());
    }
}
