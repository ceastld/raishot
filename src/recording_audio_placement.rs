//! 按已映射的活动时间放置立体声样本；缺锚与间断补静音，不外推首尾声音。
//! 不读取设备时钟，也不自行拟合采样漂移；调用方负责映射、抖动等待和暂停清空。
use std::{collections::VecDeque, fmt, time::Duration};

const RATE: u128 = 48_000;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
pub(crate) const MAX_BUFFERED_POINTS: usize = 48_000;
pub(crate) const MAX_RENDER_FRAMES: usize = 480_000;
const MAX_UNCERTAINTY: Duration = Duration::from_millis(2);
const MAX_AHEAD: Duration = Duration::from_millis(500);
const MAX_PAIR_GAP: u128 = 2_000_000 * RATE;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TimedStereo {
    pub earliest: Duration,
    pub latest: Duration,
    pub continuity: u64,
    pub samples: [f32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementError {
    CapacityLimit,
    TimeOverflow,
    FrameOverflow,
    WindowTooLarge,
    WindowBackwards,
    AllocationFailed,
}
impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CapacityLimit => tr!("音频时间队列已满", "The timed audio queue is full"),
            Self::TimeOverflow => tr!("音频放置时间溢出", "Audio placement time overflowed"),
            Self::FrameOverflow => tr!(
                "音频放置帧序号溢出",
                "Audio placement frame ordinal overflowed"
            ),
            Self::WindowTooLarge => tr!(
                "单次音频放置窗口过大",
                "The audio placement window is too large"
            ),
            Self::WindowBackwards => tr!(
                "音频放置窗口不能后退",
                "The audio placement window cannot move backwards"
            ),
            Self::AllocationFailed => tr!(
                "无法分配音频放置缓存",
                "Cannot allocate the audio placement buffer"
            ),
        })
    }
}
impl std::error::Error for PlacementError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RejectionCounts {
    pub invalid_time_range: u64,
    pub non_finite: u64,
    pub uncertain_time: u64,
    pub non_increasing: u64,
    pub too_far_ahead: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlacementSummary {
    pub accepted_points: u64,
    pub rejected: RejectionCounts,
    pub interpolated_frames: u64,
    pub silent_frames: u64,
    pub buffered_points: usize,
    pub max_buffered_points: usize,
    pub committed_until_frame: Option<u64>,
    pub resets: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    // 单位是 ns × 48000，目标帧时间则为 frame × 10^9；比较时不舍入目标网格。
    grid_time: u128,
    continuity: u64,
    samples: [f32; 2],
}

#[derive(Debug, Default)]
pub(crate) struct PlacementBuffer {
    points: VecDeque<Point>,
    last_input_time: Option<u128>,
    committed_until: Option<u64>,
    accepted_points: u64,
    rejected: RejectionCounts,
    interpolated_frames: u64,
    silent_frames: u64,
    max_buffered_points: usize,
    resets: u64,
}
impl PlacementBuffer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 仅表示已有窗口尾部时间点，不保证中间连续或每帧可插值。
    pub(crate) fn has_end_anchor(&self, frame: u64) -> bool {
        let target = u128::from(frame) * NANOS_PER_SECOND;
        self.points
            .back()
            .is_some_and(|point| point.grid_time >= target)
    }

    /// 不合格时间或样本返回 Ok(false)；容量、分配和算术失败不接纳该点。
    pub(crate) fn push(
        &mut self,
        point: TimedStereo,
        allowed_until: Duration,
    ) -> Result<bool, PlacementError> {
        if point.earliest > point.latest {
            self.rejected.invalid_time_range = self.rejected.invalid_time_range.saturating_add(1);
            return Ok(false);
        }
        if point.samples.iter().any(|sample| !sample.is_finite()) {
            self.rejected.non_finite = self.rejected.non_finite.saturating_add(1);
            return Ok(false);
        }
        let width = point.latest - point.earliest;
        if width > MAX_UNCERTAINTY {
            self.rejected.uncertain_time = self.rejected.uncertain_time.saturating_add(1);
            return Ok(false);
        }
        let allowed = allowed_until
            .checked_add(MAX_AHEAD)
            .ok_or(PlacementError::TimeOverflow)?;
        // 使用差值中点，避免先相加两个长时 Duration。
        let midpoint = point
            .earliest
            .as_nanos()
            .checked_add(width.as_nanos() / 2)
            .ok_or(PlacementError::TimeOverflow)?;
        if midpoint > allowed.as_nanos() {
            self.rejected.too_far_ahead = self.rejected.too_far_ahead.saturating_add(1);
            return Ok(false);
        }
        let grid_time = midpoint
            .checked_mul(RATE)
            .ok_or(PlacementError::TimeOverflow)?;
        if self
            .last_input_time
            .is_some_and(|previous| grid_time <= previous)
        {
            self.rejected.non_increasing = self.rejected.non_increasing.saturating_add(1);
            return Ok(false);
        }
        if self.points.len() >= MAX_BUFFERED_POINTS {
            return Err(PlacementError::CapacityLimit);
        }
        // 分批扩容，最多请求上限剩余空间；不为每个样本重新分配。
        if self.points.len() == self.points.capacity() {
            let additional = self
                .points
                .capacity()
                .max(256)
                .min(MAX_BUFFERED_POINTS - self.points.len());
            self.points
                .try_reserve_exact(additional)
                .map_err(|_| PlacementError::AllocationFailed)?;
        }
        self.points.push_back(Point {
            grid_time,
            continuity: point.continuity,
            samples: point.samples,
        });
        self.last_input_time = Some(grid_time);
        self.accepted_points = self.accepted_points.saturating_add(1);
        self.max_buffered_points = self.max_buffered_points.max(self.points.len());
        Ok(true)
    }

    /// 完整校验和分配成功后才消费；输出是固定 48kHz 交错立体声，窗口为 [start,end)。
    pub(crate) fn render(
        &mut self,
        start_frame: u64,
        frames: usize,
    ) -> Result<Vec<f32>, PlacementError> {
        if frames > MAX_RENDER_FRAMES {
            return Err(PlacementError::WindowTooLarge);
        }
        if self.committed_until.is_some_and(|end| start_frame < end) {
            return Err(PlacementError::WindowBackwards);
        }
        let count = u64::try_from(frames).map_err(|_| PlacementError::FrameOverflow)?;
        let end_frame = start_frame
            .checked_add(count)
            .ok_or(PlacementError::FrameOverflow)?;
        let mut target = u128::from(start_frame)
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(PlacementError::TimeOverflow)?;
        let end_time = u128::from(end_frame)
            .checked_mul(NANOS_PER_SECOND)
            .ok_or(PlacementError::TimeOverflow)?;
        let samples = frames.checked_mul(2).ok_or(PlacementError::FrameOverflow)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(samples)
            .map_err(|_| PlacementError::AllocationFailed)?;
        output.resize(samples, 0.);
        if frames == 0 {
            return Ok(output);
        }

        let mut interpolated = 0_u64;
        for frame in output.chunks_exact_mut(2) {
            self.drop_before(target);
            let value = self
                .points
                .get(0)
                .zip(self.points.get(1))
                .and_then(|(left, right)| interpolate(*left, *right, target))
                .or_else(|| {
                    // 精确落在一个连续段的起点时，可使用它右侧的有效相邻对。
                    self.points
                        .get(1)
                        .filter(|point| point.grid_time == target)
                        .zip(self.points.get(2))
                        .and_then(|(left, right)| interpolate(*left, *right, target))
                });
            if let Some(value) = value {
                frame.copy_from_slice(&value);
                interpolated += 1;
            }
            // end_time 已验证且本轮至多推进到它，不存在未验证的加法。
            target += NANOS_PER_SECOND;
        }
        self.drop_before(end_time);
        self.committed_until = Some(end_frame);
        self.interpolated_frames = self.interpolated_frames.saturating_add(interpolated);
        self.silent_frames = self.silent_frames.saturating_add(count - interpolated);
        Ok(output)
    }

    /// 暂停或时间区段变化时丢弃所有旧点；已提交窗口与累计计数仍保持单调。
    pub(crate) fn reset(&mut self) {
        self.points.clear();
        self.last_input_time = None;
        self.resets = self.resets.saturating_add(1);
    }

    pub(crate) fn summary(&self) -> PlacementSummary {
        PlacementSummary {
            accepted_points: self.accepted_points,
            rejected: self.rejected,
            interpolated_frames: self.interpolated_frames,
            silent_frames: self.silent_frames,
            buffered_points: self.points.len(),
            max_buffered_points: self.max_buffered_points,
            committed_until_frame: self.committed_until,
            resets: self.resets,
        }
    }

    fn drop_before(&mut self, target: u128) {
        while self
            .points
            .get(1)
            .is_some_and(|point| point.grid_time < target)
        {
            self.points.pop_front();
        }
    }
}

fn interpolate(left: Point, right: Point, target: u128) -> Option<[f32; 2]> {
    if left.continuity != right.continuity || target < left.grid_time || target > right.grid_time {
        return None;
    }
    let gap = right.grid_time.checked_sub(left.grid_time)?;
    if gap == 0 || gap > MAX_PAIR_GAP {
        return None;
    }
    let weight = (target - left.grid_time) as f64 / gap as f64;
    // f64 中计算差值，避免两个有限 f32 端点的相减先溢出。
    Some(std::array::from_fn(|channel| {
        let left = f64::from(left.samples[channel]);
        let right = f64::from(right.samples[channel]);
        (left + (right - left) * weight) as f32
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(nanos: u64, continuity: u64, value: f32) -> TimedStereo {
        TimedStereo {
            earliest: Duration::from_nanos(nanos),
            latest: Duration::from_nanos(nanos),
            continuity,
            samples: [value, -value],
        }
    }
    fn push(buffer: &mut PlacementBuffer, nanos: u64, continuity: u64, value: f32) {
        assert_eq!(
            buffer.push(point(nanos, continuity, value), Duration::from_secs(2)),
            Ok(true)
        );
    }

    #[test]
    fn recording_audio_placement_late_first_points_keep_their_time_and_silent_prefix() {
        let mut buffer = PlacementBuffer::new();
        push(&mut buffer, 1_000_000, 1, 1.);
        push(&mut buffer, 2_000_000, 1, 1.);
        let output = buffer.render(0, 120).unwrap();
        assert!(output[..48 * 2].iter().all(|sample| *sample == 0.));
        assert_eq!(&output[48 * 2..49 * 2], &[1., -1.]);
        assert!(output[97 * 2..].iter().all(|sample| *sample == 0.));
        assert_eq!(buffer.summary().interpolated_frames, 49);
        assert_eq!(buffer.summary().silent_frames, 71);
    }

    #[test]
    fn recording_audio_placement_interpolates_only_close_continuous_pairs() {
        let mut buffer = PlacementBuffer::new();
        push(&mut buffer, 0, 1, 0.);
        push(&mut buffer, 1_000_000, 1, 1.);
        let output = buffer.render(0, 49).unwrap();
        assert!((output[24 * 2] - 0.5).abs() < 1e-6);
        assert_eq!(output[48 * 2], 1.);

        let mut gap = PlacementBuffer::new();
        push(&mut gap, 0, 1, 1.);
        push(&mut gap, 3_000_000, 1, 1.);
        assert!(gap
            .render(0, 145)
            .unwrap()
            .iter()
            .all(|sample| *sample == 0.));

        let mut changed = PlacementBuffer::new();
        push(&mut changed, 0, 1, 1.);
        push(&mut changed, 1_000_000, 2, 2.);
        push(&mut changed, 2_000_000, 2, 2.);
        let output = changed.render(0, 49).unwrap();
        assert!(output[..48 * 2].iter().all(|sample| *sample == 0.));
        assert_eq!(output[48 * 2], 2.);
    }

    #[test]
    fn recording_audio_placement_adjacent_windows_keep_the_predecessor_and_do_not_extrapolate() {
        let mut buffer = PlacementBuffer::new();
        assert!(!buffer.has_end_anchor(0));
        push(&mut buffer, 0, 1, 0.);
        push(&mut buffer, 1_000_000, 1, 1.);
        push(&mut buffer, 2_000_000, 1, 2.);
        assert!(buffer.has_end_anchor(96));
        assert!(!buffer.has_end_anchor(97));
        assert_eq!(buffer.render(0, 48).unwrap().len(), 96);
        assert_eq!(buffer.summary().buffered_points, 3);
        let output = buffer.render(48, 48).unwrap();
        assert_eq!(output[0], 1.);
        assert!((output[47 * 2] - (1. + 47. / 48.)).abs() < 1e-6);
        assert_eq!(buffer.render(96, 1).unwrap(), vec![2., -2.]);
        assert_eq!(buffer.render(97, 1).unwrap(), vec![0., 0.]);
        buffer.reset();
        assert_eq!(buffer.summary().buffered_points, 0);
        assert!(!buffer.has_end_anchor(96));
        assert_eq!(buffer.render(0, 1), Err(PlacementError::WindowBackwards));
    }

    #[test]
    fn recording_audio_placement_invalid_windows_do_not_consume_or_commit() {
        let mut buffer = PlacementBuffer::new();
        push(&mut buffer, 0, 1, 1.);
        push(&mut buffer, 1_000_000, 1, 1.);
        let before = buffer.summary();
        let points = buffer.points.clone();
        assert_eq!(
            buffer.render(0, MAX_RENDER_FRAMES + 1),
            Err(PlacementError::WindowTooLarge)
        );
        assert_eq!(
            buffer.render(u64::MAX, 1),
            Err(PlacementError::FrameOverflow)
        );
        assert_eq!(buffer.render(100, 0), Ok(Vec::new()));
        assert_eq!(buffer.summary(), before);
        assert_eq!(buffer.points, points);
        assert_eq!(buffer.render(0, 1).unwrap(), vec![1., -1.]);

        let seconds = 120 * 3600;
        let mut long = PlacementBuffer::new();
        let center = Duration::from_secs(seconds);
        for (time, value) in [
            (center - Duration::from_millis(1), -1.),
            (center + Duration::from_millis(1), 1.),
        ] {
            assert_eq!(
                long.push(
                    TimedStereo {
                        earliest: time,
                        latest: time,
                        continuity: 1,
                        samples: [value, value]
                    },
                    center
                ),
                Ok(true)
            );
        }
        assert!(long
            .render(seconds * RATE as u64, 1)
            .unwrap()
            .iter()
            .all(|sample| sample.abs() < 1e-6));
    }

    #[test]
    fn recording_audio_placement_rejects_bad_points_and_enforces_capacity_before_accepting() {
        let mut buffer = PlacementBuffer::new();
        push(&mut buffer, 5_000_000, 1, 1.);
        assert_eq!(
            buffer.push(point(5_000_000, 1, 1.), Duration::ZERO),
            Ok(false)
        );
        assert_eq!(
            buffer.push(point(4_000_000, 1, 1.), Duration::ZERO),
            Ok(false)
        );
        assert_eq!(
            buffer.push(point(600_000_000, 1, 1.), Duration::ZERO),
            Ok(false)
        );
        let mut invalid = point(6_000_000, 1, 1.);
        invalid.latest = Duration::from_millis(5);
        assert_eq!(buffer.push(invalid, Duration::ZERO), Ok(false));
        let mut uncertain = point(6_000_000, 1, 1.);
        uncertain.latest = Duration::from_millis(9);
        assert_eq!(buffer.push(uncertain, Duration::ZERO), Ok(false));
        assert_eq!(
            buffer.push(point(6_000_000, 1, f32::NAN), Duration::ZERO),
            Ok(false)
        );
        assert_eq!(
            buffer.push(point(6_000_000, 1, 1.), Duration::MAX),
            Err(PlacementError::TimeOverflow)
        );
        assert_eq!(buffer.summary().buffered_points, 1);
        assert_eq!(
            buffer.summary().rejected,
            RejectionCounts {
                invalid_time_range: 1,
                non_finite: 1,
                uncertain_time: 1,
                non_increasing: 2,
                too_far_ahead: 1
            }
        );

        let mut full = PlacementBuffer::new();
        for index in 0..MAX_BUFFERED_POINTS {
            push(&mut full, index as u64 * 20_833, 1, 1.);
        }
        let before = full.summary();
        assert_eq!(
            full.push(point(1_000_000_000, 1, 1.), Duration::from_secs(2)),
            Err(PlacementError::CapacityLimit)
        );
        assert_eq!(full.summary(), before);
        assert_eq!(before.max_buffered_points, MAX_BUFFERED_POINTS);
    }
}
