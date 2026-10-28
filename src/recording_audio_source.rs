//! 将带原始包相位的音频映射到活动时间点；未知时间、暂停区与间断不贴到窗口起点。
use super::{
    audio_placement::{PlacementBuffer, PlacementSummary, TimedStereo},
    clock::ActiveRange,
};
use crate::{
    audio_capture::{CaptureRead, OutputPhase, RawAudioTime, MAX_TAKE_FRAMES},
    Result,
};
use std::{fmt, time::Duration};

const RATE: u128 = 48_000;
const MAX_FUTURE: Duration = Duration::from_millis(20);
const STALE_NANOS: u128 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceError {
    InvalidSamples,
    InvalidSegments,
    InvalidOrdinals,
    InvalidPhase,
    PacketChanged,
    ContinuityOverflow,
    TimeOverflow,
}
impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidSamples => tr!(
                "音频采集样本布局无效",
                "Invalid captured audio sample layout"
            ),
            Self::InvalidSegments => tr!(
                "音频采集片段范围无效",
                "Invalid captured audio segment range"
            ),
            Self::InvalidOrdinals => tr!(
                "音频采集输出序号无效",
                "Invalid captured audio output ordinal"
            ),
            Self::InvalidPhase => tr!(
                "音频采集相位与原始包不一致",
                "Captured audio phase does not match its source packet"
            ),
            Self::PacketChanged => tr!(
                "同一音频包的时间元数据发生变化",
                "Timing metadata changed within an audio packet"
            ),
            Self::ContinuityOverflow => tr!(
                "音频连续区段序号溢出",
                "Audio continuity ordinal overflowed"
            ),
            Self::TimeOverflow => tr!("音频活动时间范围溢出", "Audio active-time range overflowed"),
        })
    }
}
impl std::error::Error for SourceError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceDiagnostics {
    pub real_frames: u64,
    pub padding_ignored: u64,
    pub missing_qpc: u64,
    pub missing_phase: u64,
    pub untrusted_raw: u64,
    pub phase_errors: u64,
    pub outside_window: u64,
    pub invalid_window: u64,
    pub stale: u64,
    pub future: u64,
    pub model_rejected: u64,
    pub continuity_breaks: u64,
    pub clock_resets: u64,
    pub invalid_reads: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimedTrackSummary {
    pub placement: PlacementSummary,
    pub input: SourceDiagnostics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Packet {
    raw: RawAudioTime,
    phase: Option<OutputPhase>,
    start: u64,
    frames: u64,
}
impl Packet {
    fn same_key(self, other: Self) -> bool {
        self.raw.endpoint_epoch == other.raw.endpoint_epoch && self.start == other.start
    }
}

#[derive(Debug, Default)]
pub(crate) struct TimedTrack {
    buffer: PlacementBuffer,
    last_packet: Option<Packet>,
    next_output: Option<u64>,
    continuity: u64,
    continuity_open: bool,
    clock_generation: Option<u64>,
    diagnostics: SourceDiagnostics,
}
impl TimedTrack {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 暂停时清除未提交声音；已经提交的网格和累计统计由模型保留。
    pub(crate) fn reset(&mut self) {
        self.buffer.reset();
        self.clear_input();
        self.clock_generation = None;
    }

    /// 设备时钟重新建立锚后，旧代尚未提交的点不再发声。
    pub(crate) fn sync_clock_generation(&mut self, generation: u64) {
        if self
            .clock_generation
            .is_some_and(|previous| previous != generation)
        {
            self.buffer.reset();
            self.clear_input();
            self.diagnostics.clock_resets = self.diagnostics.clock_resets.saturating_add(1);
        }
        self.clock_generation = Some(generation);
    }

    fn clear_input(&mut self) {
        self.last_packet = None;
        self.next_output = None;
        self.continuity_open = false;
    }

    pub(crate) fn ingest(
        &mut self,
        read: &CaptureRead,
        allowed_until: Duration,
        committed_frame: u64,
        mut accept_raw: impl FnMut(RawAudioTime) -> bool,
        mut map_window: impl FnMut(u64, u64) -> Option<ActiveRange>,
    ) -> Result<()> {
        if let Err(error) = validate_read(read, self.last_packet) {
            self.diagnostics.invalid_reads = self.diagnostics.invalid_reads.saturating_add(1);
            return Err(error.into());
        }
        let future_limit = allowed_until
            .checked_add(MAX_FUTURE)
            .ok_or(SourceError::TimeOverflow)?;
        let committed_frame =
            committed_frame.max(self.buffer.summary().committed_until_frame.unwrap_or(0));
        let stale_before = u128::from(committed_frame)
            .checked_mul(1_000_000_000)
            .ok_or(SourceError::TimeOverflow)?
            .checked_div(RATE)
            .ok_or(SourceError::TimeOverflow)?
            .saturating_sub(STALE_NANOS);
        self.diagnostics.real_frames = self
            .diagnostics
            .real_frames
            .saturating_add(read.real_frames as u64);
        self.diagnostics.padding_ignored = self
            .diagnostics
            .padding_ignored
            .saturating_add((read.samples.len() / 2 - read.real_frames) as u64);
        for segment in &read.segments {
            let packet = Packet {
                raw: segment.raw_time,
                phase: segment.output_phase,
                start: segment.packet_output_start,
                frames: segment.packet_output_frames,
            };
            let new_packet = self
                .last_packet
                .is_none_or(|previous| !previous.same_key(packet));
            let device_gap = self.last_packet.is_some_and(|previous| {
                previous.raw.endpoint_epoch != packet.raw.endpoint_epoch
                    || previous.raw.device_rate != packet.raw.device_rate
                    || previous
                        .raw
                        .device_position
                        .checked_add(u64::from(previous.raw.input_frames))
                        != Some(packet.raw.device_position)
            });
            if (new_packet && (packet.raw.discontinuity || device_gap))
                || self
                    .next_output
                    .is_some_and(|next| next != segment.output_start)
            {
                self.cut_continuity()?;
            }
            self.last_packet = Some(packet);
            self.next_output = Some(segment.output_start + segment.frames as u64);
            let count = segment.frames as u64;
            let Some(raw_qpc) = packet.raw.qpc_100ns else {
                self.diagnostics.missing_qpc = self.diagnostics.missing_qpc.saturating_add(count);
                self.cut_continuity()?;
                continue;
            };
            let Some(phase) = packet.phase else {
                self.diagnostics.missing_phase =
                    self.diagnostics.missing_phase.saturating_add(count);
                self.cut_continuity()?;
                continue;
            };
            if !accept_raw(packet.raw) {
                self.diagnostics.untrusted_raw =
                    self.diagnostics.untrusted_raw.saturating_add(count);
                self.cut_continuity()?;
                continue;
            }
            let first = (segment.output_start - packet.start) as usize;
            for offset in 0..segment.frames {
                let (qpc_lower, qpc_upper) = match phase.qpc_bounds_checked(raw_qpc, first + offset)
                {
                    Ok(bounds) => bounds,
                    Err(_) => {
                        self.diagnostics.phase_errors =
                            self.diagnostics.phase_errors.saturating_add(1);
                        self.cut_continuity()?;
                        continue;
                    }
                };
                let Some(active) = map_window(qpc_lower, qpc_upper) else {
                    self.diagnostics.outside_window =
                        self.diagnostics.outside_window.saturating_add(1);
                    self.cut_continuity()?;
                    continue;
                };
                if active.earliest > active.latest {
                    self.diagnostics.invalid_window =
                        self.diagnostics.invalid_window.saturating_add(1);
                    self.cut_continuity()?;
                    continue;
                }
                let midpoint = active
                    .earliest
                    .as_nanos()
                    .checked_add((active.latest - active.earliest).as_nanos() / 2)
                    .ok_or(SourceError::TimeOverflow)?;
                if midpoint > future_limit.as_nanos() {
                    self.diagnostics.future = self.diagnostics.future.saturating_add(1);
                    self.cut_continuity()?;
                    continue;
                }
                if midpoint < stale_before {
                    self.diagnostics.stale = self.diagnostics.stale.saturating_add(1);
                    self.cut_continuity()?;
                    continue;
                }
                let sample = (segment.sample_offset_frames + offset) * 2;
                let accepted = self.buffer.push(
                    TimedStereo {
                        earliest: active.earliest,
                        latest: active.latest,
                        continuity: self.continuity,
                        samples: [read.samples[sample], read.samples[sample + 1]],
                    },
                    allowed_until,
                )?;
                if accepted {
                    self.continuity_open = true;
                } else {
                    self.diagnostics.model_rejected =
                        self.diagnostics.model_rejected.saturating_add(1);
                    self.cut_continuity()?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn render(&mut self, start_frame: u64, frames: usize) -> Result<Vec<f32>> {
        Ok(self.buffer.render(start_frame, frames)?)
    }

    pub(crate) fn has_end_anchor(&self, frame: u64) -> bool {
        self.buffer.has_end_anchor(frame)
    }

    pub(crate) fn summary(&self) -> TimedTrackSummary {
        TimedTrackSummary {
            placement: self.buffer.summary(),
            input: self.diagnostics,
        }
    }

    fn cut_continuity(&mut self) -> Result<()> {
        if self.continuity_open {
            self.continuity = self
                .continuity
                .checked_add(1)
                .ok_or(SourceError::ContinuityOverflow)?;
            self.continuity_open = false;
            self.diagnostics.continuity_breaks =
                self.diagnostics.continuity_breaks.saturating_add(1);
        }
        Ok(())
    }
}

fn validate_read(
    read: &CaptureRead,
    mut previous: Option<Packet>,
) -> std::result::Result<(), SourceError> {
    if read.samples.len() % 2 != 0
        || read.samples.len() / 2 > MAX_TAKE_FRAMES
        || read.real_frames > read.samples.len() / 2
    {
        return Err(SourceError::InvalidSamples);
    }
    let mut covered = 0;
    for segment in &read.segments {
        if segment.frames == 0 || segment.sample_offset_frames != covered {
            return Err(SourceError::InvalidSegments);
        }
        covered = covered
            .checked_add(segment.frames)
            .ok_or(SourceError::InvalidSegments)?;
        if covered > read.real_frames {
            return Err(SourceError::InvalidSegments);
        }
        let frames = u64::try_from(segment.frames).map_err(|_| SourceError::InvalidOrdinals)?;
        let packet_end = segment
            .packet_output_start
            .checked_add(segment.packet_output_frames)
            .ok_or(SourceError::InvalidOrdinals)?;
        let end = segment
            .output_start
            .checked_add(frames)
            .ok_or(SourceError::InvalidOrdinals)?;
        if segment.packet_output_frames == 0
            || segment.output_start < segment.packet_output_start
            || end > packet_end
        {
            return Err(SourceError::InvalidOrdinals);
        }
        let raw = segment.raw_time;
        if let Some(phase) = segment.output_phase {
            let count = usize::try_from(segment.packet_output_frames)
                .map_err(|_| SourceError::InvalidPhase)?;
            let last_phase = i128::try_from(count.saturating_sub(1))
                .ok()
                .and_then(|last| last.checked_mul(i128::from(phase.input_rate)))
                .and_then(|last| last.checked_add(i128::from(phase.first_input_offset_num)));
            let last_input =
                i128::from(raw.input_frames.saturating_sub(1)) * i128::from(phase.output_rate);
            if phase.validate().is_err()
                || phase.output_frames != count
                || phase.input_rate != raw.device_rate
                || phase.output_rate != RATE as u32
                || raw.input_frames == 0
                || last_phase.is_none_or(|last| last > last_input)
            {
                return Err(SourceError::InvalidPhase);
            }
        }
        let packet = Packet {
            raw,
            phase: segment.output_phase,
            start: segment.packet_output_start,
            frames: segment.packet_output_frames,
        };
        if previous.is_some_and(|previous| previous.same_key(packet) && previous != packet) {
            return Err(SourceError::PacketChanged);
        }
        previous = Some(packet);
    }
    if covered != read.real_frames {
        return Err(SourceError::InvalidSegments);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_capture::CaptureSegment;
    use std::cell::Cell;

    fn raw(position: u64, qpc: u64, frames: u32) -> RawAudioTime {
        RawAudioTime {
            endpoint_epoch: 0,
            device_position: position,
            qpc_100ns: Some(qpc),
            device_rate: 48_000,
            input_frames: frames,
            discontinuity: false,
        }
    }

    fn segment(raw_time: RawAudioTime, packet_start: u64, packet_frames: usize) -> CaptureSegment {
        CaptureSegment {
            raw_time,
            output_phase: Some(OutputPhase {
                first_input_offset_num: 0,
                input_rate: raw_time.device_rate,
                output_rate: 48_000,
                output_frames: packet_frames,
            }),
            packet_output_start: packet_start,
            packet_output_frames: packet_frames as u64,
            output_start: packet_start,
            frames: packet_frames,
            sample_offset_frames: 0,
        }
    }

    fn read(segment: CaptureSegment, values: &[f32]) -> CaptureRead {
        CaptureRead {
            samples: values.iter().flat_map(|value| [*value, -*value]).collect(),
            real_frames: values.len(),
            segments: vec![segment],
        }
    }

    fn map(lower: u64, upper: u64) -> Option<ActiveRange> {
        Some(ActiveRange {
            earliest: Duration::from_nanos(lower.checked_sub(1_000)? * 100),
            latest: Duration::from_nanos(upper.checked_sub(1_000)? * 100),
        })
    }

    #[test]
    fn recording_audio_source_packet_prefix_fragments_keep_phase_and_sample_offsets() {
        let mut raw = raw(0, 1_000, 4);
        raw.discontinuity = true;
        let mut first = segment(raw, 0, 4);
        first.output_start = 1;
        first.frames = 1;
        let mut second = first;
        second.output_start = 2;
        second.frames = 2;
        second.sample_offset_frames = 1;
        let read = CaptureRead {
            samples: vec![1., -1., 2., -2., 3., -3., 0., 0.],
            real_frames: 3,
            segments: vec![first, second],
        };
        let mut track = TimedTrack::new();
        let mut observed = Vec::new();
        track
            .ingest(
                &read,
                Duration::ZERO,
                0,
                |_| true,
                |lower, upper| {
                    observed.push((lower, upper));
                    map(lower, upper)
                },
            )
            .unwrap();
        assert_eq!(
            observed,
            vec![(1_208, 1_209), (1_416, 1_417), (1_625, 1_625)]
        );
        assert_eq!(track.summary().placement.accepted_points, 3);
        assert_eq!(track.summary().input.padding_ignored, 1);
        assert_eq!(track.summary().input.continuity_breaks, 0);
        assert_eq!(track.render(0, 1).unwrap(), vec![0., 0.]);
        let output = track.render(2, 1).unwrap();
        assert!((output[0] - 2.).abs() < 0.01);
        assert!((output[1] + 2.).abs() < 0.01);
        assert!(track.has_end_anchor(3));
    }

    #[test]
    fn recording_audio_source_unknown_or_invalid_anchors_do_not_appear_at_zero() {
        for mode in 0..6 {
            let mut segment = segment(raw(0, 1_000, 2), 0, 2);
            if mode == 0 {
                segment.raw_time.qpc_100ns = None;
            }
            if mode == 1 {
                segment.output_phase = None;
            }
            if mode == 5 {
                segment.raw_time.qpc_100ns = Some(0);
                segment.raw_time.input_frames = 1;
                segment.packet_output_frames = 1;
                segment.frames = 1;
                let phase = segment.output_phase.as_mut().unwrap();
                phase.first_input_offset_num = -48_000;
                phase.output_frames = 1;
            }
            let count = segment.frames;
            let read = read(segment, &vec![1.; count]);
            let mut track = TimedTrack::new();
            track
                .ingest(
                    &read,
                    Duration::ZERO,
                    0,
                    |_| mode != 2,
                    |lower, upper| match mode {
                        3 => None,
                        4 => Some(ActiveRange {
                            earliest: Duration::from_millis(1),
                            latest: Duration::ZERO,
                        }),
                        _ => map(lower, upper),
                    },
                )
                .unwrap();
            assert_eq!(track.summary().placement.accepted_points, 0, "mode {mode}");
            assert!(track
                .render(0, 100)
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.));
            let input = track.summary().input;
            let rejected = match mode {
                0 => input.missing_qpc,
                1 => input.missing_phase,
                2 => input.untrusted_raw,
                3 => input.outside_window,
                4 => input.invalid_window,
                _ => input.phase_errors,
            };
            assert_eq!(rejected, count as u64, "mode {mode}");
        }
    }

    #[test]
    fn recording_audio_source_packet_gaps_cut_once_and_fragments_stay_continuous() {
        for mode in 0..5 {
            let mut track = TimedTrack::new();
            track
                .ingest(
                    &read(segment(raw(0, 1_000, 2), 0, 2), &[1., 1.]),
                    Duration::ZERO,
                    0,
                    |_| true,
                    map,
                )
                .unwrap();
            let mut second_raw = raw(2, 1_500, 2);
            if mode == 0 {
                second_raw.discontinuity = true;
            }
            if mode == 1 {
                second_raw.device_position = 7;
            }
            if mode == 3 {
                second_raw.endpoint_epoch = 1;
            }
            if mode == 4 {
                second_raw.device_rate = 44_100;
            }
            let mut first = segment(second_raw, if mode == 2 { 7 } else { 2 }, 2);
            first.frames = 1;
            let mut second = first;
            second.output_start += 1;
            second.sample_offset_frames = 1;
            let read = CaptureRead {
                samples: vec![2., -2., 2., -2.],
                real_frames: 2,
                segments: vec![first, second],
            };
            track
                .ingest(&read, Duration::ZERO, 0, |_| true, map)
                .unwrap();
            assert_eq!(track.summary().input.continuity_breaks, 1, "mode {mode}");
            let output = track.render(2, 2).unwrap();
            assert_eq!(&output[..2], &[0., 0.], "mode {mode}");
            assert!((output[2] - 2.).abs() < 1e-6, "mode {mode}");
        }
    }

    #[test]
    fn recording_audio_source_invalid_read_is_rejected_before_any_point_is_accepted() {
        for mode in 0..6 {
            let first = segment(raw(0, 1_000, 1), 0, 1);
            let mut second = segment(raw(1, 1_208, 1), 1, 1);
            second.sample_offset_frames = 1;
            match mode {
                0 => second.sample_offset_frames = 2,
                1 => second.packet_output_start = u64::MAX,
                2 => second.output_phase.as_mut().unwrap().output_frames = 2,
                5 => second.output_phase.as_mut().unwrap().output_rate = 44_100,
                _ => {}
            }
            let mut read = CaptureRead {
                samples: vec![1., -1., 2., -2.],
                real_frames: 2,
                segments: vec![first, second],
            };
            if mode == 3 {
                read.samples.pop();
            }
            if mode == 4 {
                read.real_frames = 3;
            }
            let calls = Cell::new(0);
            let mut track = TimedTrack::new();
            assert!(track
                .ingest(
                    &read,
                    Duration::ZERO,
                    0,
                    |_| {
                        calls.set(calls.get() + 1);
                        true
                    },
                    |lower, upper| {
                        calls.set(calls.get() + 1);
                        map(lower, upper)
                    }
                )
                .is_err());
            assert_eq!(calls.get(), 0);
            assert_eq!(track.summary().placement.accepted_points, 0);
            assert_eq!(track.summary().input.invalid_reads, 1);
        }
        let mut track = TimedTrack::new();
        track.continuity = u64::MAX;
        track.continuity_open = true;
        let mut missing = segment(raw(0, 1_000, 1), 0, 1);
        missing.raw_time.qpc_100ns = None;
        assert!(track
            .ingest(&read(missing, &[1.]), Duration::ZERO, 0, |_| true, map)
            .is_err());
        assert_eq!(track.continuity, u64::MAX);
    }

    #[test]
    fn recording_audio_source_limits_future_and_stale_points_and_keeps_near_predecessor() {
        let mut track = TimedTrack::new();
        let read = read(segment(raw(0, 1_000, 2), 0, 2), &[0., 1.]);
        track
            .ingest(
                &read,
                Duration::ZERO,
                0,
                |_| true,
                |_, _| {
                    Some(ActiveRange {
                        earliest: Duration::from_nanos(20_000_001),
                        latest: Duration::from_nanos(20_000_001),
                    })
                },
            )
            .unwrap();
        assert_eq!(track.summary().input.future, 2);
        assert_eq!(track.summary().placement.accepted_points, 0);
        track.reset();
        track
            .ingest(
                &read,
                Duration::from_millis(10),
                480,
                |_| true,
                |_, _| {
                    Some(ActiveRange {
                        earliest: Duration::from_millis(7),
                        latest: Duration::from_millis(7),
                    })
                },
            )
            .unwrap();
        assert_eq!(track.summary().input.stale, 2);
        track.reset();
        let mut index = 0;
        track
            .ingest(
                &read,
                Duration::from_millis(10),
                480,
                |_| true,
                |_, _| {
                    let time = Duration::from_micros(9_500 + index * 1_000);
                    index += 1;
                    Some(ActiveRange {
                        earliest: time,
                        latest: time,
                    })
                },
            )
            .unwrap();
        let output = track.render(480, 1).unwrap();
        assert!((output[0] - 0.5).abs() < 1e-6);
        let before_reset = track.summary();
        track.reset();
        assert_eq!(track.summary().input, before_reset.input);
        assert_eq!(track.summary().placement.committed_until_frame, Some(481));
        assert_eq!(track.summary().placement.buffered_points, 0);
        assert!(track.render(480, 1).is_err());
        assert_eq!(track.render(481, 1).unwrap(), vec![0., 0.]);
    }

    #[test]
    fn recording_audio_source_clock_generation_retires_only_uncommitted_old_points() {
        let mut track = TimedTrack::new();
        track.sync_clock_generation(10);
        let read = read(segment(raw(0, 1_000, 2), 0, 2), &[1., 1.]);
        let mut index = 0;
        track
            .ingest(
                &read,
                Duration::from_millis(2),
                0,
                |_| true,
                |_, _| {
                    index += 1;
                    let time = Duration::from_millis(index);
                    Some(ActiveRange {
                        earliest: time,
                        latest: time,
                    })
                },
            )
            .unwrap();
        assert!(track
            .render(0, 48)
            .unwrap()
            .iter()
            .all(|sample| *sample == 0.));
        assert_eq!(track.summary().placement.buffered_points, 2);
        track.sync_clock_generation(10);
        assert_eq!(track.summary().placement.buffered_points, 2);
        assert!(track.has_end_anchor(96));
        assert_eq!(track.summary().placement.resets, 0);
        track.sync_clock_generation(11);
        assert_eq!(track.summary().placement.buffered_points, 0);
        assert_eq!(track.summary().placement.committed_until_frame, Some(48));
        assert_eq!(track.summary().input.clock_resets, 1);
        assert!(track
            .render(48, 48)
            .unwrap()
            .iter()
            .all(|sample| *sample == 0.));
        assert!(track.render(47, 1).is_err());
        track.reset();
        let resets = track.summary().placement.resets;
        track.sync_clock_generation(11);
        assert_eq!(track.summary().placement.resets, resets);
        assert_eq!(track.summary().placement.committed_until_frame, Some(96));
        assert!(track.render(95, 1).is_err());
        assert_eq!(track.render(96, 1).unwrap(), vec![0., 0.]);
    }
}
