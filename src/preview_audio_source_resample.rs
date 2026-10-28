//! High 模式的原生 PCM→混音网格前级；只管理有界 PCM 与绝对 PTS，不持有解码器。
use super::resample_quality::{AudioResampler, ResampleQuality};
use crate::Result;
use std::{collections::VecDeque, time::Duration};

#[cfg(test)]
const OUTPUT_RATE: u32 = 48_000;
pub(super) const MAX_SOURCE_PUSH_FRAMES: usize = 480;
pub(super) const MAX_SOURCE_PULL_FRAMES: usize = 480;
pub(super) const MAX_SOURCE_POLL_ROUNDS: usize = 16;
const NANOS_PER_SECOND: u128 = 1_000_000_000;
type StereoFrame = [f32; 2];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SourcePush {
    /// 原始块已消费的前缀帧数，包含被较早块占用的重叠前缀。
    pub consumed_frames: usize,
    /// 此次补入的原生采样率静音帧；不消费调用者的原始块。
    pub silence_frames: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum SourcePoll {
    Frame(StereoFrame),
    /// 先喂当前块的未消费前缀；块已耗尽时才读下一块或 finish。
    NeedInput,
    /// 本次丢弃前卷的预算已用完；检查取消后以同一时间重试。
    Pending,
    Ended,
}

#[derive(Clone, Copy)]
struct PendingBlock {
    time: Duration,
    frames: usize,
    next_offset: usize,
}

pub(super) struct SourceResampler {
    native_rate: u32,
    channels: usize,
    output_rate: u32,
    resampler: AudioResampler,
    /// 第一块真实数据的 PTS 锚定原生网格；seek 前卷不改变其它来源的时间轴。
    anchor: Option<Duration>,
    origin_hint: Duration,
    pending: Option<PendingBlock>,
    last_real_end: Option<Duration>,
    eof_at: Option<Duration>,
    output_finished: bool,
    output: VecDeque<(u64, StereoFrame)>,
    last_request: Option<Duration>,
}

impl SourceResampler {
    /// 变调模式可提高混音网格，避免慢速播放前先丢掉会移入可听范围的原生高频。
    pub(super) fn new(native_rate: u32, channels: usize, output_rate: u32) -> Result<Self> {
        if !(1..=2).contains(&channels) {
            return Err(tr!(
                "前级仅接受单声道或立体声",
                "Source conversion requires mono or stereo"
            )
            .into());
        }
        let resampler = AudioResampler::new(native_rate, output_rate, ResampleQuality::High)?;
        let mut output = VecDeque::new();
        output.try_reserve_exact(MAX_SOURCE_PULL_FRAMES + 1)?;
        Ok(Self {
            native_rate,
            channels,
            output_rate,
            resampler,
            anchor: None,
            origin_hint: Duration::ZERO,
            pending: None,
            last_real_end: None,
            eof_at: None,
            output_finished: false,
            output,
            last_request: None,
        })
    }

    /// seek／模式切换显式清旧数据；相同采样率复用 High 滤波表。
    pub(super) fn reset(&mut self, origin_hint: Duration) -> Result<()> {
        self.resampler
            .reset(self.native_rate, self.output_rate, ResampleQuality::High)?;
        self.anchor = None;
        self.origin_hint = origin_hint;
        self.pending = None;
        self.last_real_end = None;
        self.eof_at = None;
        self.output_finished = false;
        self.output.clear();
        self.last_request = None;
        Ok(())
    }

    pub(super) fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// 同一块必须保留完整切片并按返回值推进 offset，不能用下一块覆盖尚未消费的余段。
    /// 每次最多消费或补零 MAX_SOURCE_PUSH_FRAMES 帧；返回全零进度表示需先 sample_at。
    pub(super) fn push_block(
        &mut self,
        time: Duration,
        samples: &[f32],
        offset_frames: usize,
    ) -> Result<SourcePush> {
        if self.eof_at.is_some() {
            return Err(tr!(
                "原生音频输入已经结束",
                "Source audio input is already finished"
            )
            .into());
        }
        if samples.len() % self.channels != 0 {
            return Err(tr!("原生音频块声道不完整", "Incomplete source audio channels").into());
        }
        let frames = samples.len() / self.channels;
        if offset_frames > frames {
            return Err(tr!("原生音频块偏移无效", "Invalid source audio block offset").into());
        }
        if let Some(pending) = self.pending {
            if (pending.time, pending.frames, pending.next_offset) != (time, frames, offset_frames)
            {
                return Err(tr!(
                    "必须先消费当前原生音频块",
                    "Consume the current source block first"
                )
                .into());
            }
        } else if offset_frames != 0 {
            return Err(tr!(
                "新原生音频块必须从零偏移开始",
                "A new source block must start at zero"
            )
            .into());
        }
        if frames == offset_frames {
            return Ok(SourcePush::default());
        }
        let anchor = self.anchor.unwrap_or(time);
        let ordinal = frame_ordinal(time, anchor, self.native_rate)?
            .checked_add(i128::try_from(offset_frames)?)
            .ok_or(tr!("原生音频帧数溢出", "Source audio frame count overflow"))?;
        let accepted = i128::from(self.resampler.input_frames());
        let available = (frames - offset_frames).min(MAX_SOURCE_PUSH_FRAMES);
        // 先检查本次可能接收的真实尾，失败不能消费调用者的原始块。
        frame_end(time, offset_frames + available, self.native_rate)?;
        if ordinal > accepted {
            u64::try_from(ordinal - accepted)?;
        }
        self.anchor = Some(anchor);
        self.pending = Some(PendingBlock {
            time,
            frames,
            next_offset: offset_frames,
        });
        if ordinal > accepted {
            let gap = u64::try_from(ordinal - accepted)?;
            let count = gap.min(MAX_SOURCE_PUSH_FRAMES as u64) as usize;
            let zeros = [[0.; 2]; MAX_SOURCE_PUSH_FRAMES];
            let silence_frames = self.resampler.push(&zeros[..count])?;
            return Ok(SourcePush {
                consumed_frames: 0,
                silence_frames,
            });
        }
        let consumed = if ordinal < accepted {
            // 重叠区先到块优先；只丢明确占用的前缀，不影响后续新数据。
            usize::try_from((accepted - ordinal).min(available as i128))?
        } else {
            let mut converted = [[0.; 2]; MAX_SOURCE_PUSH_FRAMES];
            for (index, frame) in converted[..available].iter_mut().enumerate() {
                let at = (offset_frames + index) * self.channels;
                *frame = [samples[at], samples[at + self.channels - 1]];
            }
            self.resampler.push(&converted[..available])?
        };
        if consumed > 0 {
            let next_offset = offset_frames + consumed;
            let end = frame_end(time, next_offset, self.native_rate)?;
            self.last_real_end = Some(self.last_real_end.map_or(end, |previous| previous.max(end)));
            if next_offset == frames {
                self.pending = None;
            } else {
                self.pending.as_mut().unwrap().next_offset = next_offset;
            }
        }
        Ok(SourcePush {
            consumed_frames: consumed,
            silence_frames: 0,
        })
    }

    /// 只有解码器 EOF 且调用者已消费最后一块时封口；空来源直接结束，不无限补零。
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Err(tr!(
                "原生音频块仍有未消费数据",
                "Source audio block still has unread data"
            )
            .into());
        }
        if self.eof_at.is_none() {
            self.resampler.finish()?;
            self.eof_at = Some(self.last_real_end.unwrap_or(self.origin_hint));
        }
        Ok(())
    }

    pub(super) fn end_time(&self) -> Option<Duration> {
        self.eof_at
    }

    /// 只接受 reset 后不早于 origin_hint 的单调时间；NeedInput/Pending 可同点重试。
    /// 输出 PTS 不由读取／滤波前瞻推进，seek 的长前卷每次最多处理固定轮数。
    pub(super) fn sample_at(&mut self, time: Duration) -> Result<SourcePoll> {
        if time < self.origin_hint || self.last_request.is_some_and(|previous| time < previous) {
            return Err(tr!(
                "原生音频取样时间倒退",
                "Source audio sampling time moved backward"
            )
            .into());
        }
        if self.eof_at.is_some_and(|end| time >= end) {
            self.last_request = Some(time);
            return Ok(SourcePoll::Ended);
        }
        let Some(anchor) = self.anchor else {
            self.last_request = Some(time);
            return Ok(SourcePoll::NeedInput);
        };
        if time < anchor {
            self.last_request = Some(time);
            return Ok(SourcePoll::Frame([0.; 2]));
        }
        let scaled = (time - anchor).as_nanos() * u128::from(self.output_rate);
        let ordinal = u64::try_from(scaled / NANOS_PER_SECOND)?;
        let fraction = (scaled % NANOS_PER_SECOND) as f64 / NANOS_PER_SECOND as f64;
        self.last_request = Some(time);
        for _ in 0..MAX_SOURCE_POLL_ROUNDS {
            while self.output.len() >= 2 && self.output[1].0 <= ordinal {
                self.output.pop_front();
            }
            if let Some(&(at, a)) = self.output.front() {
                if at == ordinal {
                    if fraction == 0. {
                        return Ok(SourcePoll::Frame(a));
                    }
                    if let Some(&(_, b)) = self.output.get(1) {
                        return Ok(SourcePoll::Frame(interpolate(a, b, fraction)));
                    }
                    if self.output_finished {
                        return Ok(SourcePoll::Frame(a));
                    }
                }
            }
            if self.output_finished {
                // round(N*out/in) 可少于覆盖真实尾所需的 ceil 帧数；仅在真实尾内延续末值。
                return Ok(SourcePoll::Frame(
                    self.output.back().map_or([0.; 2], |entry| entry.1),
                ));
            }
            // 上面已经把缓存缩到至多一帧，避免取样跨度或 gap 扩大输出缓存。
            debug_assert!(self.output.len() <= 1);
            let chunk = self.resampler.pull(MAX_SOURCE_PULL_FRAMES)?;
            self.output_finished = chunk.finished;
            let empty = chunk.samples.is_empty();
            for (offset, frame) in chunk.samples.into_iter().enumerate() {
                self.output
                    .push_back((chunk.output_start + offset as u64, frame));
            }
            if empty && chunk.needs_input {
                return Ok(SourcePoll::NeedInput);
            }
        }
        Ok(SourcePoll::Pending)
    }
}

/// PTS 只做相对锚点的整数换算；每块独立定位，纳秒量化不会逐块积累成 gap。
fn frame_ordinal(time: Duration, anchor: Duration, rate: u32) -> Result<i128> {
    let magnitude = (time.abs_diff(anchor).as_nanos() * u128::from(rate) + NANOS_PER_SECOND / 2)
        / NANOS_PER_SECOND;
    let ordinal = i128::try_from(magnitude)?;
    Ok(if time >= anchor { ordinal } else { -ordinal })
}

fn frame_end(time: Duration, frames: usize, rate: u32) -> Result<Duration> {
    let frames = u64::try_from(frames)?;
    let duration = Duration::new(
        frames / u64::from(rate),
        (frames % u64::from(rate) * 1_000_000_000 / u64::from(rate)) as u32,
    );
    time.checked_add(duration)
        .ok_or_else(|| tr!("原生音频 PTS 溢出", "Source audio PTS overflow").into())
}

fn interpolate(a: StereoFrame, b: StereoFrame, fraction: f64) -> StereoFrame {
    std::array::from_fn(|channel| {
        let value = f64::from(a[channel]) * (1. - fraction) + f64::from(b[channel]) * fraction;
        if value.is_finite() {
            value.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
        } else {
            0.
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    struct Packet {
        time: Duration,
        samples: Vec<f32>,
    }

    fn time(frame: usize, rate: u32) -> Duration {
        frame_end(Duration::ZERO, frame, rate).unwrap()
    }

    /// 只驱动真实 SourceResampler/High FIR；原始包被模块确认消费后才换下一包。
    fn render(
        rate: u32,
        channels: usize,
        packets: &[Packet],
        origin: Duration,
    ) -> (Vec<StereoFrame>, Duration) {
        render_at_rate(rate, channels, OUTPUT_RATE, packets, origin)
    }

    fn render_at_rate(
        rate: u32,
        channels: usize,
        output_rate: u32,
        packets: &[Packet],
        origin: Duration,
    ) -> (Vec<StereoFrame>, Duration) {
        let mut source = SourceResampler::new(rate, channels, output_rate).unwrap();
        source.reset(origin).unwrap();
        let mut packet = 0;
        let mut offset = 0;
        let mut output = Vec::new();
        for index in 0..48_000 {
            let at = origin + time(index, output_rate);
            let mut completed = false;
            for _ in 0..20_000 {
                match source.sample_at(at).unwrap() {
                    SourcePoll::Frame(frame) => {
                        output.push(frame);
                        completed = true;
                        break;
                    }
                    SourcePoll::Ended => {
                        assert!(source.output.len() <= MAX_SOURCE_PULL_FRAMES + 1);
                        let end = source.end_time().unwrap();
                        assert_eq!(source.sample_at(at).unwrap(), SourcePoll::Ended);
                        return (output, end);
                    }
                    SourcePoll::Pending => {}
                    SourcePoll::NeedInput => {
                        if let Some(block) = packets.get(packet) {
                            let progress = source
                                .push_block(block.time, &block.samples, offset)
                                .unwrap();
                            assert!(progress.consumed_frames <= MAX_SOURCE_PUSH_FRAMES);
                            assert!(progress.silence_frames <= MAX_SOURCE_PUSH_FRAMES);
                            offset += progress.consumed_frames;
                            if offset == block.samples.len() / channels {
                                packet += 1;
                                offset = 0;
                            }
                        } else {
                            source.finish().unwrap();
                        }
                    }
                }
                assert!(source.output.len() <= MAX_SOURCE_PULL_FRAMES + 1);
            }
            assert!(completed, "原生前级单点未能在有界驱动内前进");
        }
        panic!("原生前级未能在一秒内排空合成样本")
    }

    fn tone_packets(frequency: f64, chunk: usize) -> Vec<Packet> {
        let frames = 24_000;
        (0..frames)
            .step_by(chunk)
            .map(|start| {
                let mut samples = Vec::new();
                for index in start..(start + chunk).min(frames) {
                    let value = (TAU * frequency * index as f64 / 96_000.).sin() as f32 * 0.25;
                    samples.extend([value, -value]);
                }
                Packet {
                    time: time(start, 96_000),
                    samples,
                }
            })
            .collect()
    }

    fn rms(input: &[StereoFrame]) -> f64 {
        let interior = &input[100..input.len() - 100];
        (interior
            .iter()
            .map(|frame| f64::from(frame[0]).powi(2))
            .sum::<f64>()
            / interior.len() as f64)
            .sqrt()
    }

    fn to_device(input: &[StereoFrame], effective_rate: u32) -> Vec<StereoFrame> {
        let mut resampler =
            AudioResampler::new(effective_rate, 48_000, ResampleQuality::High).unwrap();
        let mut offset = 0;
        let mut closed = false;
        let mut output = Vec::new();
        for _ in 0..10_000 {
            if offset < input.len() {
                offset += resampler
                    .push(&input[offset..(offset + 480).min(input.len())])
                    .unwrap();
            } else if !closed {
                resampler.finish().unwrap();
                closed = true;
            }
            let chunk = resampler.pull(480).unwrap();
            output.extend(chunk.samples);
            if chunk.finished {
                assert_eq!(offset, input.len());
                return output;
            }
        }
        panic!("真实后级滤波未能在固定预算内排空")
    }

    fn amplitude(input: &[StereoFrame], frequency: f64) -> f64 {
        let mut sine = 0.;
        let mut cosine = 0.;
        let interior = &input[100..input.len() - 100];
        for (offset, frame) in interior.iter().enumerate() {
            let phase = TAU * frequency * (offset + 100) as f64 / 48_000.;
            sine += f64::from(frame[0]) * phase.sin();
            cosine += f64::from(frame[0]) * phase.cos();
        }
        2. * sine.hypot(cosine) / interior.len() as f64
    }

    #[test]
    fn source_resample_rate_aware_grid_preserves_slow_high_tones_and_filters_fast_output() {
        let high_packets = tone_packets(30_000., 997);
        // 0.5x：96k 网格保留 30k，再以实际 48k 设备速率交付，真实频率为 15k。
        let (slow_grid, _) = render_at_rate(96_000, 2, 96_000, &high_packets, Duration::ZERO);
        let slow = to_device(&slow_grid, 48_000);
        assert!((amplitude(&slow, 15_000.) - 0.25).abs() < 0.01);
        assert!(amplitude(&slow, 9_000.) < 0.01);

        // 1x：48k 网格必须在原生前级消除 30k，而非先把它折叠到 18k。
        let (normal_grid, _) = render_at_rate(96_000, 2, 48_000, &high_packets, Duration::ZERO);
        let normal = to_device(&normal_grid, 48_000);
        assert!(rms(&normal) < 0.01);

        // 2x：混音网格 48k、有效输入率 96k，真实后级仍以正确截止保留低音频。
        let (fast_grid, _) = render_at_rate(
            96_000,
            2,
            48_000,
            &tone_packets(1_000., 997),
            Duration::ZERO,
        );
        let fast = to_device(&fast_grid, 96_000);
        assert!((amplitude(&fast, 2_000.) - 0.25).abs() < 0.01);
    }

    #[test]
    fn source_resample_native_96k_alias_is_filtered_before_the_48k_mix() {
        let (low, _) = render(96_000, 2, &tone_packets(1_000., 997), Duration::ZERO);
        let (high, _) = render(96_000, 2, &tone_packets(30_000., 997), Duration::ZERO);
        assert!((rms(&low) - 0.25 / 2_f64.sqrt()).abs() < 0.01);
        // 不经原生前级时，30k 在 48k mix 的偶数抽样中折叠为满幅 18k。
        assert!(rms(&high) < rms(&low) * 0.05);
        assert!(low
            .iter()
            .chain(&high)
            .all(|frame| (frame[0] + frame[1]).abs() < 1e-6));
    }

    #[test]
    fn source_resample_native_packet_boundaries_do_not_change_the_filter() {
        let (single, end) = render(96_000, 2, &tone_packets(1_000., 1), Duration::ZERO);
        let (large, large_end) = render(96_000, 2, &tone_packets(1_000., 997), Duration::ZERO);
        assert_eq!(single.len(), 12_000);
        assert_eq!(single, large);
        assert!(end.abs_diff(large_end) <= Duration::from_nanos(1));
    }

    #[test]
    fn source_resample_pts_gaps_and_overlap_keep_first_arrival_and_real_tail() {
        let packets = [
            Packet {
                time: Duration::ZERO,
                samples: vec![1.; 1_920],
            },
            Packet {
                time: Duration::from_millis(30),
                samples: vec![2.; 1_920],
            },
            Packet {
                time: Duration::from_millis(40),
                samples: vec![-3.; 1_920],
            },
        ];
        let (output, end) = render(96_000, 1, &packets, Duration::ZERO);
        assert_eq!(end, Duration::from_millis(60));
        assert_eq!(output.len(), 2_880);
        for (ms, expected) in [(10, 1.), (25, 0.), (45, 2.), (55, -3.)] {
            let frame = output[ms * 48];
            assert!((frame[0] - expected).abs() < 1e-4, "{ms}ms");
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn source_resample_seek_in_gap_and_reset_clear_old_filter_history() {
        let next = [Packet {
            time: Duration::from_millis(50),
            samples: vec![0.5; 960],
        }];
        let origin = Duration::from_millis(35);
        let (output, end) = render(96_000, 1, &next, origin);
        assert_eq!(end, Duration::from_millis(60));
        assert_eq!(output.len(), 1_200);
        assert!(output[..15 * 48].iter().all(|frame| *frame == [0.; 2]));
        assert!((output[20 * 48][0] - 0.5).abs() < 1e-5);

        let mut source = SourceResampler::new(96_000, 1, OUTPUT_RATE).unwrap();
        let old = vec![1.; 960];
        let old_prefix = source.push_block(Duration::ZERO, &old, 0).unwrap();
        assert_eq!(old_prefix.consumed_frames, MAX_SOURCE_PUSH_FRAMES);
        assert!(source.pending.is_some());
        assert!(matches!(
            source.sample_at(Duration::ZERO).unwrap(),
            SourcePoll::Frame(_)
        ));
        source.reset(Duration::from_secs(7)).unwrap();
        assert!(source.pending.is_none() && source.output.is_empty());
        assert_eq!(
            source.sample_at(Duration::from_secs(7)).unwrap(),
            SourcePoll::NeedInput
        );
        source
            .push_block(Duration::from_secs(7), &[0.; 100], 0)
            .unwrap();
        source.finish().unwrap();
        assert_eq!(
            source.sample_at(Duration::from_secs(7)).unwrap(),
            SourcePoll::Frame([0.; 2])
        );
        assert!(source.sample_at(Duration::ZERO).is_err());
    }

    #[test]
    fn source_resample_never_discards_pending_data_or_allocates_an_entire_gap() {
        let mut source = SourceResampler::new(96_000, 1, OUTPUT_RATE).unwrap();
        let block = vec![0.25; 961];
        let first = source.push_block(Duration::ZERO, &block, 0).unwrap();
        assert_eq!(first.consumed_frames, MAX_SOURCE_PUSH_FRAMES);
        assert!(source.finish().is_err());
        assert!(source
            .push_block(Duration::from_secs(1), &block, 0)
            .is_err());
        assert!(source.push_block(Duration::ZERO, &block, 0).is_err());
        let mut offset = first.consumed_frames;
        for _ in 0..3 {
            if offset == block.len() {
                break;
            }
            let progress = source.push_block(Duration::ZERO, &block, offset).unwrap();
            assert!(progress.consumed_frames > 0);
            offset += progress.consumed_frames;
        }
        assert_eq!(offset, block.len());
        let gap = source
            .push_block(Duration::from_secs(3_600), &[0.5], 0)
            .unwrap();
        assert_eq!(
            gap,
            SourcePush {
                consumed_frames: 0,
                silence_frames: MAX_SOURCE_PUSH_FRAMES
            }
        );
        assert_eq!(
            source.resampler.input_frames(),
            (block.len() + MAX_SOURCE_PUSH_FRAMES) as u64
        );
        assert!(source.finish().is_err());
        source.reset(Duration::ZERO).unwrap();
        source.finish().unwrap();
        assert_eq!(source.sample_at(Duration::ZERO).unwrap(), SourcePoll::Ended);
        assert!(source.push_block(Duration::ZERO, &[0.], 0).is_err());
    }

    #[test]
    fn source_resample_rejects_invalid_input_without_consuming_it() {
        assert!(SourceResampler::new(0, 1, OUTPUT_RATE).is_err());
        assert!(SourceResampler::new(96_000, 3, OUTPUT_RATE).is_err());
        assert!(SourceResampler::new(96_000, 1, 0).is_err());
        let mut source = SourceResampler::new(96_000, 2, OUTPUT_RATE).unwrap();
        assert!(source.push_block(Duration::ZERO, &[1.], 0).is_err());
        assert!(source.push_block(Duration::ZERO, &[1.; 4], 1).is_err());
        assert!(source.push_block(Duration::MAX, &[1.; 4], 0).is_err());
        assert!(source.anchor.is_none() && source.pending.is_none());
        assert_eq!(source.resampler.input_frames(), 0);
    }

    #[test]
    fn source_resample_empty_and_subframe_tails_end_at_the_real_pts() {
        for (rate, frames) in [(96_000, 0), (192_000, 1), (96_000, 31), (44_100, 997)] {
            let packets = if frames == 0 {
                Vec::new()
            } else {
                vec![Packet {
                    time: Duration::ZERO,
                    samples: vec![0.5; frames],
                }]
            };
            let (output, end) = render(rate, 1, &packets, Duration::ZERO);
            assert_eq!(end, time(frames, rate));
            assert_eq!(
                output.len(),
                (frames * OUTPUT_RATE as usize).div_ceil(rate as usize)
            );
            assert!(output
                .iter()
                .all(|frame| frame[0].is_finite() && frame[0] == frame[1]));
        }
    }
}
