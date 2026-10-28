//! 请求收据前的原始 PCM 门；不创建时间轴，不修改包锚或重采样相位。
use super::clock::{ActiveSpan, ClockPoint, RecordingClock};
use crate::{
    audio_capture::{CaptureRead, CaptureSegment, RawAudioTime},
    Result,
};
use std::{
    collections::VecDeque,
    mem::size_of,
    time::{Duration, Instant},
};

pub(crate) const MAX_CONTROL_FRAMES: usize = 96_000;
pub(crate) const MAX_CONTROL_BYTES: usize = 4 * 1024 * 1024;
const RATE: u128 = 48_000;

pub(crate) struct ControlRead {
    pub span: Instant,
    pub read: CaptureRead,
}
pub(crate) struct FifoRead {
    pub samples: Vec<f32>,
    pub real_frames: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ControlSummary {
    pub raw_frames: usize,
    pub ready_frames: usize,
    /// Vec/Deque 实际 capacity 的载荷及槽位；不含分配器私有头或调用者持有的返回值。
    pub retained_bytes: usize,
    pub selected_frames: u64,
    pub missing_anchor: u64,
    pub invalid_phase: u64,
    pub untrusted: u64,
    pub outside_span: u64,
    pub beyond_cap: u64,
    pub non_finite: u64,
    pub invalid_clock: u64,
    /// 异常缺失；不含正常暂停区、闭段冗余、补零和等待未来 fence。
    pub rejected_frames: u64,
    pub padding_ignored: u64,
    /// 重复 fence 可重复遇到同一未来点，这是等待次数，不是唯一帧数。
    pub future_waits: u64,
    pub closed_extra_frames: u64,
    pub fifo_silent_frames: u64,
}

struct ReadCursor {
    read: CaptureRead,
    frame: usize,
    segment: usize,
}
struct Ready {
    span: Instant,
    samples: Vec<f32>,
    offset: usize,
}

#[derive(Default)]
pub(crate) struct ControlTrack {
    raw: VecDeque<ReadCursor>,
    ready: VecDeque<Ready>,
    fifo_until: Option<u64>,
    counts: ControlSummary,
}

fn fail(message: &str) -> Box<dyn std::error::Error> {
    message.to_owned().into()
}
fn frame_at(time: Duration) -> Result<u64> {
    u64::try_from(
        time.as_nanos()
            .checked_mul(RATE)
            .ok_or_else(|| fail("音频控制帧位置溢出"))?
            / 1_000_000_000,
    )
    .map_err(Into::into)
}
fn read_bytes(read: &CaptureRead) -> usize {
    read.samples
        .capacity()
        .saturating_mul(size_of::<f32>())
        .saturating_add(
            read.segments
                .capacity()
                .saturating_mul(size_of::<CaptureSegment>()),
        )
}
fn output_bytes(output: &Vec<ControlRead>) -> usize {
    output
        .capacity()
        .saturating_mul(size_of::<ControlRead>())
        .saturating_add(
            output
                .iter()
                .map(|part| read_bytes(&part.read))
                .sum::<usize>(),
        )
}

fn validate(read: &CaptureRead) -> Result<()> {
    if read.samples.len() % 2 != 0
        || read.samples.len() / 2 > MAX_CONTROL_FRAMES
        || read.real_frames > read.samples.len() / 2
        || read_bytes(read) > MAX_CONTROL_BYTES
    {
        return Err(fail("原始音频控制块超过帧数/实际容量限制或格式无效"));
    }
    let mut covered = 0_usize;
    for segment in &read.segments {
        let end = segment.output_start.checked_add(segment.frames as u64);
        let packet_end = segment
            .packet_output_start
            .checked_add(segment.packet_output_frames);
        if segment.frames == 0
            || segment.sample_offset_frames != covered
            || segment.packet_output_frames == 0
            || segment.output_start < segment.packet_output_start
            || end
                .zip(packet_end)
                .is_none_or(|(end, packet_end)| end > packet_end)
        {
            return Err(fail("原始音频控制块的片段/包序号无效"));
        }
        covered = covered
            .checked_add(segment.frames)
            .ok_or_else(|| fail("音频片段长度溢出"))?;
        if covered > read.real_frames {
            return Err(fail("音频片段超过真实 PCM"));
        }
    }
    if covered != read.real_frames {
        return Err(fail("真实 PCM 缺少完整片段元数据"));
    }
    Ok(())
}

fn valid_phase(segment: CaptureSegment) -> bool {
    segment.output_phase.is_some_and(|phase| {
        let last = (segment.packet_output_frames.saturating_sub(1) as i128)
            .checked_mul(i128::from(phase.input_rate))
            .and_then(|value| value.checked_add(i128::from(phase.first_input_offset_num)));
        phase.is_valid()
            && phase.input_rate == segment.raw_time.device_rate
            && phase.output_rate == RATE as u32
            && phase.output_frames as u64 == segment.packet_output_frames
            && segment.raw_time.input_frames != 0
            && last.is_some_and(|last| {
                last <= i128::from(segment.raw_time.input_frames - 1)
                    * i128::from(phase.output_rate)
            })
    })
}

impl ControlTrack {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn summary(&self) -> ControlSummary {
        ControlSummary {
            // 帧数只计未消费部分；被 cursor 保留的原 Vec 前缀仍计入实际容量账。
            raw_frames: self
                .raw
                .iter()
                .map(|part| part.read.real_frames - part.frame)
                .sum(),
            ready_frames: self
                .ready
                .iter()
                .map(|part| part.samples.len() / 2 - part.offset)
                .sum(),
            retained_bytes: self
                .raw
                .capacity()
                .saturating_mul(size_of::<ReadCursor>())
                .saturating_add(self.ready.capacity().saturating_mul(size_of::<Ready>()))
                .saturating_add(
                    self.raw
                        .iter()
                        .map(|part| read_bytes(&part.read))
                        .sum::<usize>(),
                )
                .saturating_add(
                    self.ready
                        .iter()
                        .map(|part| part.samples.capacity().saturating_mul(size_of::<f32>()))
                        .sum::<usize>(),
                ),
            rejected_frames: self
                .counts
                .missing_anchor
                .saturating_add(self.counts.invalid_phase)
                .saturating_add(self.counts.untrusted)
                .saturating_add(self.counts.beyond_cap)
                .saturating_add(self.counts.non_finite)
                .saturating_add(self.counts.invalid_clock),
            ..self.counts
        }
    }

    fn budget(&self, extra_frames: usize, extra_bytes: usize) -> Result<()> {
        let used = self.summary();
        if used
            .raw_frames
            .saturating_add(used.ready_frames)
            .saturating_add(extra_frames)
            > MAX_CONTROL_FRAMES
            || used.retained_bytes.saturating_add(extra_bytes) > MAX_CONTROL_BYTES
        {
            return Err(fail("音频控制暂存超过每源 96000 帧/4MiB 实际容量预算"));
        }
        Ok(())
    }

    pub(crate) fn queue(&mut self, read: CaptureRead) -> Result<()> {
        let result = self.queue_inner(read);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn queue_inner(&mut self, read: CaptureRead) -> Result<()> {
        validate(&read)?;
        self.budget(
            read.samples.len() / 2,
            read_bytes(&read).saturating_add(size_of::<ReadCursor>()),
        )?;
        self.counts.padding_ignored = self
            .counts
            .padding_ignored
            .saturating_add((read.samples.len() / 2 - read.real_frames) as u64);
        if read.real_frames == 0 {
            return Ok(());
        }
        self.raw.try_reserve_exact(1)?;
        self.budget(read.samples.len() / 2, read_bytes(&read))?;
        self.raw.push_back(ReadCursor {
            read,
            frame: 0,
            segment: 0,
        });
        Ok(())
    }

    /// 只放行整个 QPC 窗口早于 fence 且属于一个已知活动段的点。
    /// 尚在未来的原块和 cursor 保留；缺锚/不可信/跨暂停的点不会转为 FIFO 声音。
    /// 错误表示本次不可继续提交，调用者应结束录制；已返回的前次提交不回滚。
    pub(crate) fn select(
        &mut self,
        clock: &RecordingClock,
        fence: ClockPoint,
        active_cap: Duration,
        mut accept_raw: impl FnMut(RawAudioTime) -> bool,
    ) -> Result<Vec<ControlRead>> {
        let result = self.select_inner(clock, fence, active_cap, &mut accept_raw);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn select_inner(
        &mut self,
        clock: &RecordingClock,
        fence: ClockPoint,
        active_cap: Duration,
        accept_raw: &mut impl FnMut(RawAudioTime) -> bool,
    ) -> Result<Vec<ControlRead>> {
        let mut output = Vec::new();
        // 每个已排队原块本次最多访问一次；未来块不能造成无限重试。
        let reads = self.raw.len();
        for _ in 0..reads {
            let mut cursor = self.raw.pop_front().unwrap();
            while cursor.frame < cursor.read.real_frames {
                let segment = cursor.read.segments[cursor.segment];
                if cursor.frame == segment.sample_offset_frames + segment.frames {
                    cursor.segment += 1;
                    continue;
                }
                let offset = cursor.frame - segment.sample_offset_frames;
                let raw = segment.raw_time;
                let eligible = if !accept_raw(raw) {
                    self.counts.untrusted = self.counts.untrusted.saturating_add(1);
                    None
                } else if let (Some(anchor), Some(fence_qpc)) = (
                    raw.qpc_100ns,
                    fence.qpc.filter(|window| window.earliest <= window.latest),
                ) {
                    match segment
                        .output_phase
                        .filter(|_| valid_phase(segment))
                        .and_then(|phase| {
                            phase.qpc_bounds(
                                anchor,
                                (segment.output_start - segment.packet_output_start) as usize
                                    + offset,
                            )
                        }) {
                        Some((_lower, upper)) if upper >= fence_qpc.earliest => {
                            self.counts.future_waits = self.counts.future_waits.saturating_add(1);
                            break;
                        }
                        Some((lower, upper)) => match clock.map_control_window(lower, upper) {
                            Some((span, active)) if active.latest <= active_cap => Some(span.key),
                            Some(_) => {
                                self.counts.beyond_cap = self.counts.beyond_cap.saturating_add(1);
                                None
                            }
                            None => {
                                let summary = clock.summary();
                                if summary.missing_anchors != 0 || summary.invalid_anchors != 0 {
                                    self.counts.invalid_clock =
                                        self.counts.invalid_clock.saturating_add(1);
                                } else {
                                    self.counts.outside_span =
                                        self.counts.outside_span.saturating_add(1);
                                }
                                None
                            }
                        },
                        None => {
                            self.counts.invalid_phase = self.counts.invalid_phase.saturating_add(1);
                            None
                        }
                    }
                } else {
                    self.counts.missing_anchor = self.counts.missing_anchor.saturating_add(1);
                    None
                };
                if let Some(span) = eligible {
                    let samples = &cursor.read.samples[cursor.frame * 2..cursor.frame * 2 + 2];
                    if samples.iter().all(|sample| sample.is_finite()) {
                        self.append(&mut output, &cursor, span, segment, offset, samples)?;
                        self.counts.selected_frames = self.counts.selected_frames.saturating_add(1);
                    } else {
                        self.counts.non_finite = self.counts.non_finite.saturating_add(1);
                    }
                }
                cursor.frame += 1;
            }
            if cursor.frame < cursor.read.real_frames {
                self.raw.push_back(cursor);
            }
        }
        Ok(output)
    }

    fn append(
        &self,
        output: &mut Vec<ControlRead>,
        cursor: &ReadCursor,
        span: Instant,
        mut segment: CaptureSegment,
        offset: usize,
        samples: &[f32],
    ) -> Result<()> {
        let active_bytes = read_bytes(&cursor.read);
        if output.last().is_none_or(|part| part.span != span) {
            self.budget(
                0,
                active_bytes
                    .saturating_add(output_bytes(output))
                    .saturating_add(size_of::<ControlRead>()),
            )?;
            output.try_reserve_exact(1)?;
            output.push(ControlRead {
                span,
                read: CaptureRead {
                    samples: Vec::new(),
                    real_frames: 0,
                    segments: Vec::new(),
                },
            });
        }
        let last = output.last_mut().unwrap();
        segment.output_start += offset as u64;
        segment.frames = 1;
        segment.sample_offset_frames = last.read.real_frames;
        let merge = last.read.segments.last().is_some_and(|previous| {
            previous.raw_time == segment.raw_time
                && previous.output_phase == segment.output_phase
                && previous.packet_output_start == segment.packet_output_start
                && previous.packet_output_frames == segment.packet_output_frames
                && previous.output_start + previous.frames as u64 == segment.output_start
        });
        let extra = if last.read.samples.capacity() - last.read.samples.len() < 2 {
            512 * size_of::<f32>()
        } else {
            0
        } + if !merge && last.read.segments.capacity() == last.read.segments.len() {
            size_of::<CaptureSegment>()
        } else {
            0
        };
        // 不能在持有 last 的可变借用时扫描 output；先按其当前容量取得总账。
        let _ = last;
        self.budget(
            0,
            active_bytes
                .saturating_add(output_bytes(output))
                .saturating_add(extra),
        )?;
        let last = output.last_mut().unwrap();
        if last.read.samples.capacity() - last.read.samples.len() < 2 {
            last.read.samples.try_reserve_exact(512)?;
        }
        if !merge {
            last.read.segments.try_reserve_exact(1)?;
            last.read.segments.push(segment);
        } else {
            last.read.segments.last_mut().unwrap().frames += 1;
        }
        last.read.samples.extend_from_slice(samples);
        last.read.real_frames += 1;
        self.budget(0, active_bytes.saturating_add(output_bytes(output)))
    }

    pub(crate) fn push_ready(&mut self, part: ControlRead) -> Result<()> {
        let result = self.push_ready_inner(part);
        if result.is_err() {
            self.clear();
        }
        result
    }

    fn push_ready_inner(&mut self, part: ControlRead) -> Result<()> {
        validate(&part.read)?;
        if part.read.real_frames != part.read.samples.len() / 2 {
            return Err(fail("FIFO 合格块不能含无锚补零"));
        }
        self.budget(
            part.read.real_frames,
            part.read
                .samples
                .capacity()
                .saturating_mul(size_of::<f32>())
                .saturating_add(size_of::<Ready>()),
        )?;
        self.ready.try_reserve_exact(1)?;
        self.budget(
            part.read.real_frames,
            part.read
                .samples
                .capacity()
                .saturating_mul(size_of::<f32>()),
        )?;
        if part.read.real_frames > 0 {
            self.ready.push_back(Ready {
                span: part.span,
                samples: part.read.samples,
                offset: 0,
            });
        }
        Ok(())
    }

    fn discard_closed(&mut self, spans: &[ActiveSpan], at: u64) -> Result<()> {
        let mut index = 0;
        while index < self.ready.len() {
            let part = &self.ready[index];
            let old = spans
                .iter()
                .find(|span| span.key == part.span)
                .map(|span| {
                    span.end
                        .map(frame_at)
                        .transpose()
                        .map(|end| end.is_some_and(|end| end <= at))
                })
                .transpose()?
                .unwrap_or(true);
            if old {
                self.counts.closed_extra_frames = self
                    .counts
                    .closed_extra_frames
                    .saturating_add((part.samples.len() / 2 - part.offset) as u64);
                self.ready.remove(index);
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    /// 每轨从第一次指定的起点连续提交；不足按段补零，不借用恢复后的 PCM 填旧段。
    pub(crate) fn render_fifo(
        &mut self,
        spans: &[ActiveSpan],
        start_frame: u64,
        frames: usize,
    ) -> Result<FifoRead> {
        if frames > MAX_CONTROL_FRAMES || self.fifo_until.is_some_and(|until| until != start_frame)
        {
            return Err(fail("FIFO 音频提交窗口超限或不连续"));
        }
        let end = start_frame
            .checked_add(frames as u64)
            .ok_or_else(|| fail("FIFO 帧窗口溢出"))?;
        let windows = spans
            .iter()
            .map(|span| {
                Ok((
                    span.key,
                    frame_at(span.start)?,
                    span.end.map(frame_at).transpose()?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if windows
            .iter()
            .any(|(_, start, end)| end.is_some_and(|end| end < *start))
            || windows
                .windows(2)
                .any(|pair| pair[0].1 > pair[1].1 || pair[0].2.is_none_or(|end| end > pair[1].1))
        {
            return Err(fail("FIFO 活动区段重叠或逆序"));
        }
        let mut samples = Vec::new();
        samples.try_reserve_exact(frames * 2)?;
        samples.resize(frames * 2, 0.);
        let mut real_frames = 0;
        self.discard_closed(spans, start_frame)?;
        for (index, target) in samples.chunks_exact_mut(2).enumerate() {
            let at = start_frame + index as u64;
            if let Some((key, _, _)) = windows
                .iter()
                .find(|(_, start, end)| at >= *start && end.is_none_or(|end| at < end))
            {
                if let Some(ready) = self.ready.iter().position(|part| part.span == *key) {
                    let part = &mut self.ready[ready];
                    target.copy_from_slice(&part.samples[part.offset * 2..part.offset * 2 + 2]);
                    part.offset += 1;
                    real_frames += 1;
                    if part.offset == part.samples.len() / 2 {
                        self.ready.remove(ready);
                    }
                }
            }
            // 跨界时只撤销已闭合段的多余声音；下一段的 ready 不受影响。
            if windows.iter().any(|(_, _, end)| *end == Some(at + 1)) {
                self.discard_closed(spans, at + 1)?;
            }
        }
        if frames > 0 {
            self.fifo_until = Some(end);
        }
        self.counts.fifo_silent_frames = self
            .counts
            .fifo_silent_frames
            .saturating_add((frames - real_frames) as u64);
        Ok(FifoRead {
            samples,
            real_frames,
        })
    }

    /// 释放所有未提交 raw/ready；保留 FIFO 提交前缀及累计诊断。
    pub(crate) fn clear(&mut self) {
        self.raw = VecDeque::new();
        self.ready = VecDeque::new();
    }
}

#[cfg(test)]
mod tests {
    use super::super::audio_source::TimedTrack;
    use super::super::clock::QpcWindow;
    use super::*;
    use crate::audio_capture::OutputPhase;

    const BASE: u64 = 1_000_000;
    fn point(base: Instant, millis: u64) -> ClockPoint {
        let qpc = BASE + millis * 10_000;
        ClockPoint {
            instant: base + Duration::from_millis(millis),
            qpc: Some(QpcWindow {
                earliest: qpc,
                latest: qpc,
            }),
        }
    }
    fn read(values: impl IntoIterator<Item = f32>, qpc: Option<u64>) -> CaptureRead {
        let samples = values
            .into_iter()
            .flat_map(|value| [value, -value])
            .collect::<Vec<_>>();
        let frames = samples.len() / 2;
        CaptureRead {
            samples,
            real_frames: frames,
            segments: if frames == 0 {
                Vec::new()
            } else {
                vec![CaptureSegment {
                    raw_time: RawAudioTime {
                        endpoint_epoch: 0,
                        device_position: 0,
                        qpc_100ns: qpc,
                        device_rate: 48_000,
                        input_frames: frames as u32,
                        discontinuity: false,
                    },
                    output_phase: Some(OutputPhase {
                        first_input_offset_num: 0,
                        input_rate: 48_000,
                        output_rate: 48_000,
                        output_frames: frames,
                    }),
                    packet_output_start: 0,
                    packet_output_frames: frames as u64,
                    output_start: 0,
                    frames,
                    sample_offset_frames: 0,
                }]
            },
        }
    }

    #[test]
    fn recording_audio_control_delayed_receipts_preserve_both_active_parts_without_pause_pulse() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0), Duration::ZERO);
        clock.pause(point(base, 200));
        clock.resume(point(base, 300), Duration::from_millis(200));
        clock.pause(point(base, 400));
        let cap = Duration::from_millis(300);
        let mut control = ControlTrack::new();
        control
            .queue(read(
                (0..19_200).map(|index| {
                    if index < 9_600 {
                        1.
                    } else if index < 14_400 {
                        99.
                    } else {
                        2.
                    }
                }),
                Some(BASE),
            ))
            .unwrap();
        let selected = control
            .select(&clock, point(base, 400), cap, |_| true)
            .unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].read.real_frames, 9_600);
        assert_eq!(selected[1].read.real_frames, 4_800);
        assert!(selected
            .iter()
            .flat_map(|part| &part.read.samples)
            .all(|value| value.abs() != 99.));
        // 真正的TimedTrack同时验证筛后片段的原包锚、相位、ordinal和PCM偏移没有被重写。
        let mut timed = TimedTrack::new();
        for part in &selected {
            timed
                .ingest(
                    &part.read,
                    cap,
                    0,
                    |_| true,
                    |lo, hi| clock.map_window(lo, hi),
                )
                .unwrap();
        }
        assert_eq!(timed.summary().input.real_frames, 14_400);
        let placed = timed.render(0, 14_400).unwrap();
        assert!(placed.iter().all(|value| value.abs() <= 2.));
        assert_eq!(&placed[100 * 2..100 * 2 + 2], &[1., -1.]);
        assert_eq!(&placed[10_000 * 2..10_000 * 2 + 2], &[2., -2.]);
        for part in selected {
            control.push_ready(part).unwrap();
        }
        let fifo = control
            .render_fifo(&clock.active_spans(), 0, 14_400)
            .unwrap();
        assert_eq!(fifo.real_frames, 14_400);
        assert!(fifo.samples[..9_600 * 2]
            .chunks_exact(2)
            .all(|value| value == [1., -1.]));
        assert!(fifo.samples[9_600 * 2..]
            .chunks_exact(2)
            .all(|value| value == [2., -2.]));
        assert_eq!(control.summary().outside_span, 4_800);
        assert_eq!(control.summary().rejected_frames, 0, "正常暂停不是设备告警");
    }

    #[test]
    fn recording_audio_control_future_cursor_keeps_original_packet_until_next_fence() {
        let base = Instant::now();
        let clock = RecordingClock::start(point(base, 0), Duration::ZERO);
        let mut control = ControlTrack::new();
        control
            .queue(read([1., 2., 3., 4., 5., 6.], Some(BASE)))
            .unwrap();
        let fence = ClockPoint {
            instant: base + Duration::from_nanos(62_500),
            qpc: Some(QpcWindow {
                earliest: BASE + 625,
                latest: BASE + 625,
            }),
        };
        let first = control
            .select(&clock, fence, Duration::from_nanos(62_500), |_| true)
            .unwrap();
        assert_eq!(first[0].read.samples, [1., -1., 2., -2., 3., -3.]);
        assert_eq!(control.summary().raw_frames, 3);
        let second = control
            .select(&clock, point(base, 1), Duration::from_millis(1), |_| true)
            .unwrap();
        assert_eq!(second[0].read.samples, [4., -4., 5., -5., 6., -6.]);
        let segment = second[0].read.segments[0];
        assert_eq!(
            (
                segment.packet_output_start,
                segment.packet_output_frames,
                segment.output_start,
                segment.sample_offset_frames
            ),
            (0, 6, 3, 0)
        );
        assert_eq!(segment.raw_time.qpc_100ns, Some(BASE));
        assert_eq!(segment.output_phase.unwrap().output_frames, 6);
        let mut timed = TimedTrack::new();
        for part in first.iter().chain(&second) {
            timed
                .ingest(
                    &part.read,
                    Duration::from_millis(1),
                    0,
                    |_| true,
                    |lo, hi| clock.map_window(lo, hi),
                )
                .unwrap();
        }
        assert_eq!(timed.summary().placement.accepted_points, 6);
        assert_eq!(control.summary().raw_frames, 0);
    }

    #[test]
    fn recording_audio_control_missing_untrusted_and_cross_pause_windows_do_not_pass() {
        let base = Instant::now();
        let mut clock = RecordingClock::start(point(base, 0), Duration::ZERO);
        clock.pause(ClockPoint {
            instant: base + Duration::from_micros(100),
            qpc: Some(QpcWindow {
                earliest: BASE + 1_000,
                latest: BASE + 1_002,
            }),
        });
        clock.resume(
            ClockPoint {
                instant: base + Duration::from_micros(200),
                qpc: Some(QpcWindow {
                    earliest: BASE + 2_000,
                    latest: BASE + 2_000,
                }),
            },
            Duration::from_micros(100),
        );
        let mut control = ControlTrack::new();
        control.queue(read([7.], None)).unwrap();
        let mut crossed = read([8.], Some(BASE + 999));
        crossed.segments[0].raw_time.input_frames = 2;
        crossed.segments[0]
            .output_phase
            .as_mut()
            .unwrap()
            .first_input_offset_num = 1;
        control.queue(crossed).unwrap();
        assert!(control
            .select(&clock, point(base, 1), Duration::from_millis(1), |_| true)
            .unwrap()
            .is_empty());
        assert_eq!(control.summary().missing_anchor, 1);
        assert_eq!(control.summary().outside_span, 1);
        control.queue(read([9.], Some(BASE))).unwrap();
        assert!(control
            .select(&clock, point(base, 1), Duration::from_millis(1), |_| false)
            .unwrap()
            .is_empty());
        assert_eq!(control.summary().untrusted, 1);
        let mut invalid = read([10.], Some(BASE));
        invalid.segments[0].output_phase = None;
        control.queue(invalid).unwrap();
        assert!(control
            .select(&clock, point(base, 1), Duration::from_millis(1), |_| true)
            .unwrap()
            .is_empty());
        assert_eq!(control.summary().invalid_phase, 1);
        assert_eq!(control.summary().rejected_frames, 3);
        // 控制起点自身缺QPC也须形成异常诊断，不能把整段静音误说成正常暂停过滤。
        let no_anchor = RecordingClock::start(
            ClockPoint {
                instant: base,
                qpc: None,
            },
            Duration::ZERO,
        );
        control.queue(read([11.], Some(BASE))).unwrap();
        assert!(control
            .select(&no_anchor, point(base, 1), Duration::from_millis(1), |_| {
                true
            })
            .unwrap()
            .is_empty());
        assert_eq!(control.summary().invalid_clock, 1);
        assert_eq!(control.summary().rejected_frames, 4);
    }

    fn media_frame(frame: u64) -> Duration {
        Duration::from_nanos((frame * 1_000_000_000).div_ceil(48_000))
    }
    #[test]
    fn recording_audio_control_fifo_silences_old_tail_and_gaps_without_borrowing_later_span() {
        let base = Instant::now();
        let spans = [
            ActiveSpan {
                key: base,
                start: Duration::ZERO,
                end: Some(media_frame(2)),
            },
            ActiveSpan {
                key: base + Duration::from_secs(1),
                start: media_frame(4),
                end: Some(media_frame(6)),
            },
        ];
        let mut control = ControlTrack::new();
        control
            .push_ready(ControlRead {
                span: spans[0].key,
                read: read([1.], Some(BASE)),
            })
            .unwrap();
        control
            .push_ready(ControlRead {
                span: spans[1].key,
                read: read([2., 3.], Some(BASE)),
            })
            .unwrap();
        let out = control.render_fifo(&spans, 0, 6).unwrap();
        assert_eq!(out.real_frames, 3);
        assert_eq!(
            out.samples,
            [1., -1., 0., 0., 0., 0., 0., 0., 2., -2., 3., -3.]
        );
        assert_eq!(control.summary().fifo_silent_frames, 3);
        let mut extra = ControlTrack::new();
        extra
            .push_ready(ControlRead {
                span: spans[0].key,
                read: read([1., 2., 99.], Some(BASE)),
            })
            .unwrap();
        extra.render_fifo(&spans, 0, 2).unwrap();
        assert_eq!(extra.summary().closed_extra_frames, 1);
        assert!(extra.render_fifo(&spans, 1, 1).is_err());
    }

    #[test]
    fn recording_audio_control_caps_real_capacity_and_combined_retained_frames() {
        let mut control = ControlTrack::new();
        let mut oversized = read([1.], Some(BASE));
        oversized.samples = Vec::with_capacity(MAX_CONTROL_BYTES / size_of::<f32>() + 1);
        oversized.samples.extend([1., -1.]);
        assert!(control.queue(oversized).is_err());
        control
            .queue(read(
                std::iter::repeat_n(1., MAX_CONTROL_FRAMES),
                Some(BASE),
            ))
            .unwrap();
        assert!(control
            .push_ready(ControlRead {
                span: Instant::now(),
                read: read([1.], Some(BASE))
            })
            .is_err());
        assert_eq!(
            control.summary().retained_bytes,
            0,
            "超限明确失败并释放未提交暂存"
        );
        assert!(control
            .queue(read(
                std::iter::repeat_n(1., MAX_CONTROL_FRAMES + 1),
                Some(BASE)
            ))
            .is_err());
    }

    #[test]
    fn recording_audio_control_zero_frame_cleanup_does_not_grow_ready_capacity() {
        let base = Instant::now();
        let spans = [ActiveSpan {
            key: base,
            start: Duration::ZERO,
            end: None,
        }];
        let mut control = ControlTrack::new();
        // 非二次幂的合法精确容量：清理不能重新按几何增长分配相同 ready。
        control.ready = VecDeque::with_capacity(1_025);
        for _ in 0..1_025 {
            control.ready.push_back(Ready {
                span: base,
                samples: vec![1., -1.],
                offset: 0,
            });
        }
        control.budget(0, 0).unwrap();
        let before = control.summary().retained_bytes;
        let empty = control.render_fifo(&spans, 0, 0).unwrap();
        assert!(empty.samples.is_empty());
        assert_eq!(control.summary().retained_bytes, before);
        assert_eq!(control.summary().ready_frames, 1_025);
        let first = control.render_fifo(&spans, 0, 1).unwrap();
        assert_eq!(first.samples, [1., -1.]);
        assert_eq!(first.real_frames, 1);
        assert!(control.summary().retained_bytes <= before);
    }

    #[test]
    fn recording_audio_control_partial_future_block_and_fifo_share_the_frame_budget() {
        let base = Instant::now();
        let clock = RecordingClock::start(point(base, 0), Duration::ZERO);
        let mut control = ControlTrack::new();
        control
            .queue(read(
                std::iter::repeat_n(1., MAX_CONTROL_FRAMES),
                Some(BASE),
            ))
            .unwrap();
        let selected = control
            .select(&clock, point(base, 1_000), Duration::from_secs(1), |_| true)
            .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|part| part.read.real_frames)
                .sum::<usize>(),
            48_000
        );
        for part in selected {
            control.push_ready(part).unwrap();
        }
        let held = control.summary();
        assert_eq!((held.raw_frames, held.ready_frames), (48_000, 48_000));
        assert!(held.retained_bytes <= MAX_CONTROL_BYTES);
        let output = control
            .render_fifo(&clock.active_spans(), 0, 48_000)
            .unwrap();
        assert_eq!(output.real_frames, 48_000);
        assert_eq!(control.summary().raw_frames, 48_000);
    }

    #[test]
    fn recording_audio_control_clear_drops_old_epoch_pcm_but_keeps_rejection_diagnostics() {
        let base = Instant::now();
        let clock = RecordingClock::start(point(base, 0), Duration::ZERO);
        let mut control = ControlTrack::new();
        control.queue(read([7.], None)).unwrap();
        assert!(control
            .select(&clock, point(base, 1), Duration::from_millis(1), |_| true)
            .unwrap()
            .is_empty());
        control.queue(read([8.], Some(BASE + 20_000))).unwrap();
        assert!(control
            .select(&clock, point(base, 1), Duration::from_millis(1), |_| true)
            .unwrap()
            .is_empty());
        control
            .push_ready(ControlRead {
                span: base,
                read: read([99.], Some(BASE)),
            })
            .unwrap();
        control.clear();
        let held = control.summary();
        assert_eq!(
            (
                held.raw_frames,
                held.ready_frames,
                held.retained_bytes,
                held.rejected_frames
            ),
            (0, 0, 0, 1)
        );
        assert_eq!(
            control
                .render_fifo(&clock.active_spans(), 0, 1)
                .unwrap()
                .samples,
            [0., 0.]
        );
        control
            .push_ready(ControlRead {
                span: base,
                read: read([2.], Some(BASE)),
            })
            .unwrap();
        assert_eq!(
            control
                .render_fifo(&clock.active_spans(), 1, 1)
                .unwrap()
                .samples,
            [2., -2.]
        );
        assert_eq!(control.summary().rejected_frames, 1);
    }
}
