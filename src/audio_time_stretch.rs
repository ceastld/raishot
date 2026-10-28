//! 48 kHz 立体声流式 WSOLA；输出 ordinal 表示音频帧数，媒体时钟由设备提交方管理。
use crate::Result;
use std::collections::VecDeque;

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const MAX_INPUT_FRAMES: usize = 12_000;
pub(crate) const MAX_PUSH_FRAMES: usize = 4_800;
pub(crate) const MAX_PULL_FRAMES: usize = 4_800;
pub(crate) const MAX_HOPS_PER_PULL: usize = 8;
const WINDOW_FRAMES: usize = 960;
const HOP_FRAMES: usize = 480;
const SEARCH_FRAMES: u64 = 240;
const COARSE_STEP: u64 = 4;
const SCORE_STRIDE: usize = 4;

pub(crate) type StereoFrame = [f32; 2];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StretchChunk {
    /// 本次首帧在处理器输出中的 ordinal；只有设备提交成功后才能映射媒体时间。
    pub output_start: u64,
    pub samples: Vec<StereoFrame>,
    /// 尚未 finish，且当前缓冲不足以生成下一完整搜索窗口。
    pub needs_input: bool,
    /// 输入已封口，最终目标帧数已全部交付；后续 pull 保持空且 finished。
    pub finished: bool,
}

#[derive(Clone, Copy)]
struct Rate {
    numerator: u64,
    denominator: u64,
}
impl Rate {
    fn new(value: f64) -> Result<Self> {
        let ratio = [
            (0.5, (1, 2)),
            (0.75, (3, 4)),
            (1., (1, 1)),
            (1.25, (5, 4)),
            (1.5, (3, 2)),
            (2., (2, 1)),
        ]
        .into_iter()
        .find_map(|(rate, ratio)| (rate == value).then_some(ratio))
        .ok_or(tr!(
            "不支持此保调倍率",
            "Unsupported pitch-preserving playback rate"
        ))?;
        Ok(Self {
            numerator: ratio.0,
            denominator: ratio.1,
        })
    }

    fn normal(self) -> bool {
        self.numerator == self.denominator
    }

    fn input_at(self, output: u64) -> u128 {
        (u128::from(output) * u128::from(self.numerator) + u128::from(self.denominator / 2))
            / u128::from(self.denominator)
    }

    fn output_length(self, input: u64) -> Result<u64> {
        let frames = (u128::from(input) * u128::from(self.denominator)
            + u128::from(self.numerator / 2))
            / u128::from(self.numerator);
        Ok(u64::try_from(frames)?)
    }
}

pub(crate) struct AudioTimeStretch {
    rate: Rate,
    input: VecDeque<StereoFrame>,
    /// input.front 在原始 48 kHz 输入中的 ordinal。
    input_start: u64,
    accepted_input: u64,
    pending: VecDeque<StereoFrame>,
    tail: [StereoFrame; HOP_FRAMES],
    has_tail: bool,
    generated_output: u64,
    delivered_output: u64,
    final_output: Option<u64>,
}

impl AudioTimeStretch {
    pub(crate) fn new(rate: f64) -> Result<Self> {
        let rate = Rate::new(rate)?;
        let mut input = VecDeque::new();
        input.try_reserve_exact(MAX_INPUT_FRAMES)?;
        let mut pending = VecDeque::new();
        pending.try_reserve_exact(HOP_FRAMES)?;
        Ok(Self {
            rate,
            input,
            input_start: 0,
            accepted_input: 0,
            pending,
            tail: [[0.; 2]; HOP_FRAMES],
            has_tail: false,
            generated_output: 0,
            delivered_output: 0,
            final_output: None,
        })
    }

    /// 非法倍率不改变旧状态；成功后清空全部 PCM、重叠窗和 ordinal，复用已分配缓存。
    pub(crate) fn reset(&mut self, rate: f64) -> Result<()> {
        let rate = Rate::new(rate)?;
        self.rate = rate;
        self.input.clear();
        self.input_start = 0;
        self.accepted_input = 0;
        self.pending.clear();
        self.tail.fill([0.; 2]);
        self.has_tail = false;
        self.generated_output = 0;
        self.delivered_output = 0;
        self.final_output = None;
        Ok(())
    }

    /// 只接收返回值指出的前缀。缓存满时返回 0，调用方须保留余下输入并先 pull。
    /// 单次最多接收 MAX_PUSH_FRAMES；错误时不消费输入。非有限采样按零处理。
    pub(crate) fn push(&mut self, frames: &[StereoFrame]) -> Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        if self.final_output.is_some() {
            return Err(tr!(
                "音频伸缩输入已经结束",
                "Time-stretch input is already finished"
            )
            .into());
        }
        let count = frames
            .len()
            .min(MAX_PUSH_FRAMES)
            .min(MAX_INPUT_FRAMES - self.input.len());
        let accepted = self.accepted_input.checked_add(count as u64).ok_or(tr!(
            "音频伸缩输入帧数溢出",
            "Time-stretch input frame count overflow"
        ))?;
        for frame in &frames[..count] {
            self.input
                .push_back(frame.map(|value| if value.is_finite() { value } else { 0. }));
        }
        self.accepted_input = accepted;
        Ok(count)
    }

    /// 仅确定 round(真实输入帧数 / 倍率) 的尾界；尾部仍由有工作量上限的 pull 生成。
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.final_output.is_none() {
            let end = self.rate.output_length(self.accepted_input)?;
            if end < self.generated_output {
                return Err(tr!(
                    "音频伸缩尾界早于已生成输出",
                    "Time-stretch end precedes generated output"
                )
                .into());
            }
            self.final_output = Some(end);
        }
        Ok(())
    }

    /// 单次最多生成八个 hop；可能少于请求帧数，返回状态区分待输入与已完成。
    pub(crate) fn pull(&mut self, max_frames: usize) -> Result<StretchChunk> {
        if max_frames == 0 || max_frames > MAX_PULL_FRAMES {
            return Err(tr!(
                "音频伸缩输出请求超出范围",
                "Time-stretch output request is out of range"
            )
            .into());
        }
        // 所有可能失败的检查和分配都在消费 pending 或生成新 hop 之前完成。
        self.delivered_output
            .checked_add(max_frames as u64)
            .ok_or(tr!(
                "音频伸缩输出帧数溢出",
                "Time-stretch output frame count overflow"
            ))?;
        self.generated_output
            .checked_add((MAX_HOPS_PER_PULL * HOP_FRAMES) as u64)
            .ok_or(tr!(
                "音频伸缩输出帧数溢出",
                "Time-stretch output frame count overflow"
            ))?;
        let mut samples = Vec::new();
        samples.try_reserve_exact(max_frames)?;
        let output_start = self.delivered_output;
        let mut hops = 0;
        while samples.len() < max_frames {
            while samples.len() < max_frames {
                let Some(frame) = self.pending.pop_front() else {
                    break;
                };
                samples.push(frame);
            }
            if samples.len() == max_frames || hops == MAX_HOPS_PER_PULL || !self.can_generate() {
                break;
            }
            self.generate_hop();
            hops += 1;
        }
        self.delivered_output += samples.len() as u64;
        Ok(StretchChunk {
            output_start,
            samples,
            needs_input: self.final_output.is_none()
                && self.pending.is_empty()
                && !self.can_generate(),
            finished: self.final_output == Some(self.delivered_output) && self.pending.is_empty(),
        })
    }

    pub(crate) fn input_frames(&self) -> u64 {
        self.accepted_input
    }

    pub(crate) fn output_frames(&self) -> u64 {
        self.delivered_output
    }

    fn can_generate(&self) -> bool {
        if let Some(end) = self.final_output {
            return self.generated_output < end;
        }
        if self.rate.normal() {
            return !self.input.is_empty();
        }
        let required = if self.has_tail {
            self.rate.input_at(self.generated_output)
                + u128::from(SEARCH_FRAMES)
                + WINDOW_FRAMES as u128
        } else {
            WINDOW_FRAMES as u128
        };
        // 收齐整个搜索范围才生成，避免 push 的分块方式改变候选范围。
        required <= u128::from(self.accepted_input)
    }

    fn generate_hop(&mut self) {
        let count = self.final_output.map_or(HOP_FRAMES, |end| {
            (end - self.generated_output).min(HOP_FRAMES as u64) as usize
        });
        if self.rate.normal() {
            let count = count.min(self.input.len());
            for _ in 0..count {
                if let Some(frame) = self.input.pop_front() {
                    self.pending.push_back(frame);
                }
            }
            self.input_start += count as u64;
            self.generated_output += count as u64;
            return;
        }
        let start = if self.has_tail { self.find_offset() } else { 0 };
        for index in 0..count {
            let next = self.frame_at(start.saturating_add(index as u64));
            let frame = if self.has_tail {
                let weight = index as f64 / (HOP_FRAMES - 1) as f64;
                std::array::from_fn(|channel| {
                    let value = f64::from(self.tail[index][channel]) * (1. - weight)
                        + f64::from(next[channel]) * weight;
                    value.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
                })
            } else {
                next
            };
            self.pending.push_back(frame);
        }
        for index in 0..HOP_FRAMES {
            self.tail[index] = self.frame_at(start.saturating_add((HOP_FRAMES + index) as u64));
        }
        self.has_tail = true;
        self.generated_output += count as u64;
        if self.final_output == Some(self.generated_output) {
            self.input.clear();
            self.input_start = self.accepted_input;
        } else {
            let keep_from = self
                .rate
                .input_at(self.generated_output)
                .saturating_sub(u128::from(SEARCH_FRAMES))
                .min(u128::from(self.accepted_input)) as u64;
            let remove = (keep_from - self.input_start).min(self.input.len() as u64) as usize;
            self.input.drain(..remove);
            self.input_start += remove as u64;
        }
    }

    fn frame_at(&self, ordinal: u64) -> StereoFrame {
        // 只有 finish 后的分析窗可越过真实尾界；这些零不增加最终输出帧数。
        ordinal
            .checked_sub(self.input_start)
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| self.input.get(offset))
            .copied()
            .unwrap_or([0.; 2])
    }

    fn find_offset(&self) -> u64 {
        let center = self
            .rate
            .input_at(self.generated_output)
            .min(u128::from(self.accepted_input.saturating_sub(1))) as u64;
        let low = center.saturating_sub(SEARCH_FRAMES).max(self.input_start);
        let high = center
            .saturating_add(SEARCH_FRAMES)
            .min(self.accepted_input.saturating_sub(1));
        let nominal = center.clamp(low, high);
        let mut best = nominal;
        let mut best_score = self.score(best, SCORE_STRIDE);
        let mut candidate = low;
        loop {
            choose_candidate(
                candidate,
                self.score(candidate, SCORE_STRIDE),
                nominal,
                &mut best,
                &mut best_score,
            );
            if high - candidate < COARSE_STEP {
                break;
            }
            candidate += COARSE_STEP;
        }
        // 固定 4 帧粗搜，再只精搜最优点两侧各 4 帧，单 hop 候选数有界。
        let refine_low = best.saturating_sub(COARSE_STEP).max(low);
        let refine_high = best.saturating_add(COARSE_STEP).min(high);
        best_score = self.score(best, 1);
        for candidate in refine_low..=refine_high {
            choose_candidate(
                candidate,
                self.score(candidate, 1),
                nominal,
                &mut best,
                &mut best_score,
            );
        }
        best
    }

    fn score(&self, start: u64, stride: usize) -> Option<f64> {
        let mut product = 0_f64;
        let mut reference_energy = 0_f64;
        let mut candidate_energy = 0_f64;
        for index in (0..HOP_FRAMES).step_by(stride) {
            let candidate = self.frame_at(start.saturating_add(index as u64));
            for channel in 0..2 {
                // 联合声道能量，反相立体声也有有效相关值；两个声道始终共用 offset。
                let reference = f64::from(self.tail[index][channel]);
                let candidate = f64::from(candidate[channel]);
                product += reference * candidate;
                reference_energy += reference * reference;
                candidate_energy += candidate * candidate;
            }
        }
        (reference_energy > 0. && candidate_energy > 0.)
            .then(|| product / (reference_energy.sqrt() * candidate_energy.sqrt()))
    }
}

fn choose_candidate(
    candidate: u64,
    score: Option<f64>,
    nominal: u64,
    best: &mut u64,
    best_score: &mut Option<f64>,
) {
    let Some(score) = score else {
        return;
    };
    if best_score.is_none_or(|current| {
        score > current + 1e-12
            || ((score - current).abs() <= 1e-12
                && candidate.abs_diff(nominal) < best.abs_diff(nominal))
    }) {
        *best = candidate;
        *best_score = Some(score);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn render(rate: f64, input: &[StereoFrame], push_size: usize) -> Vec<StereoFrame> {
        let mut stretch = AudioTimeStretch::new(rate).unwrap();
        let mut position = 0;
        let mut finished = false;
        let mut output = Vec::new();
        for _ in 0..10_000 {
            let end = (position + push_size).min(input.len());
            let accepted = stretch.push(&input[position..end]).unwrap();
            position += accepted;
            if position == input.len() && !finished {
                stretch.finish().unwrap();
                finished = true;
            }
            let chunk = stretch.pull(MAX_PULL_FRAMES).unwrap();
            assert_eq!(chunk.output_start, output.len() as u64);
            assert!(stretch.input.len() <= MAX_INPUT_FRAMES);
            assert!(stretch.pending.len() <= HOP_FRAMES);
            assert!(chunk.samples.len() <= MAX_PULL_FRAMES);
            let progress = accepted > 0 || !chunk.samples.is_empty() || chunk.finished;
            output.extend_from_slice(&chunk.samples);
            if chunk.finished {
                assert_eq!(stretch.input_frames(), input.len() as u64);
                assert_eq!(stretch.output_frames(), output.len() as u64);
                assert!(stretch.pull(17).unwrap().finished);
                assert!(stretch.pull(17).unwrap().samples.is_empty());
                return output;
            }
            assert!(progress, "streaming processor stalled");
        }
        panic!("streaming processor did not finish within the test bound");
    }

    fn tone(frames: usize) -> Vec<StereoFrame> {
        (0..frames)
            .map(|index| {
                let value = (TAU * 440. * index as f64 / f64::from(SAMPLE_RATE)).sin() as f32 * 0.5;
                [value, -value]
            })
            .collect()
    }

    fn strongest_frequency(input: &[StereoFrame]) -> usize {
        let input = &input[1_200..input.len() - 1_200];
        let mut best = (0_f64, 0_usize);
        for frequency in (100..=1_100).step_by(5) {
            let angle = TAU * frequency as f64 / f64::from(SAMPLE_RATE);
            let (step_sin, step_cos) = angle.sin_cos();
            let (mut sine, mut cosine) = (0_f64, 1_f64);
            let (mut real, mut imaginary) = (0_f64, 0_f64);
            for frame in input {
                real += f64::from(frame[0]) * cosine;
                imaginary += f64::from(frame[0]) * sine;
                (sine, cosine) = (
                    sine * step_cos + cosine * step_sin,
                    cosine * step_cos - sine * step_sin,
                );
            }
            let energy = real * real + imaginary * imaginary;
            if energy > best.0 {
                best = (energy, frequency);
            }
        }
        best.1
    }

    #[test]
    fn time_stretch_one_x_passes_through_exactly_across_push_boundaries() {
        let input: Vec<_> = (0..10_003)
            .map(|index| [index as f32 / 20_000., -(index as f32) / 30_000.])
            .collect();
        assert_eq!(render(1., &input, 137), input);
    }

    #[test]
    fn time_stretch_half_and_double_keep_dominant_frequency_and_exact_length() {
        let input = tone(24_000);
        for (rate, expected) in [(0.5, 48_000), (2., 12_000)] {
            let output = render(rate, &input, 2_003);
            assert_eq!(output.len(), expected);
            assert!(strongest_frequency(&output).abs_diff(440) <= 5);
            assert!(output.iter().all(|frame| {
                frame[0].is_finite() && frame[1].is_finite() && (frame[0] + frame[1]).abs() < 1e-6
            }));
        }
    }

    #[test]
    fn time_stretch_other_rates_share_channel_offsets_and_integer_tail_lengths() {
        let input = tone(5_003);
        for rate in [0.75, 1.25, 1.5] {
            let output = render(rate, &input, 379);
            assert_eq!(output.len(), (input.len() as f64 / rate).round() as usize);
            assert!(output
                .iter()
                .all(|frame| (frame[0] + frame[1]).abs() < 1e-6));
        }
    }

    #[test]
    fn time_stretch_short_and_empty_finish_have_finite_tails() {
        for frames in [0, 1, 17, 479, 480, 959] {
            let input = vec![[0.25, -0.5]; frames];
            for rate in [0.5, 0.75, 1., 1.25, 1.5, 2.] {
                let output = render(rate, &input, 101);
                assert_eq!(output.len(), (frames as f64 / rate).round() as usize);
                assert!(output.iter().flatten().all(|value| value.is_finite()));
            }
        }
    }

    #[test]
    fn time_stretch_backpressure_and_pull_work_are_bounded_without_dropping_input() {
        let mut stretch = AudioTimeStretch::new(0.5).unwrap();
        let input = vec![[0.25, -0.5]; MAX_INPUT_FRAMES + 1];
        let mut accepted = 0;
        loop {
            let count = stretch.push(&input[accepted..]).unwrap();
            accepted += count;
            if count == 0 {
                break;
            }
        }
        assert_eq!(accepted, MAX_INPUT_FRAMES);
        assert_eq!(stretch.input_frames(), accepted as u64);
        let chunk = stretch.pull(MAX_PULL_FRAMES).unwrap();
        assert_eq!(chunk.samples.len(), MAX_HOPS_PER_PULL * HOP_FRAMES);
        assert!(!chunk.finished && !chunk.needs_input);
        assert_eq!(stretch.push(&input[accepted..]).unwrap(), 1);
        stretch.finish().unwrap();
        assert!(stretch.push(&[[1., 1.]]).is_err());
        assert_eq!(stretch.input_frames(), input.len() as u64);
        assert!(stretch.pull(0).is_err());
        assert!(stretch.pull(MAX_PULL_FRAMES + 1).is_err());
    }

    #[test]
    fn time_stretch_reset_removes_old_samples_and_rejects_invalid_rate_transactionally() {
        let mut stretch = AudioTimeStretch::new(0.5).unwrap();
        stretch.push(&[[1., -1.]; 2_000]).unwrap();
        stretch.pull(1).unwrap();
        let before = (
            stretch.input_frames(),
            stretch.output_frames(),
            stretch.pending.len(),
        );
        assert!(stretch.reset(f64::NAN).is_err());
        assert_eq!(
            before,
            (
                stretch.input_frames(),
                stretch.output_frames(),
                stretch.pending.len()
            )
        );
        stretch.reset(2.).unwrap();
        assert_eq!((stretch.input_frames(), stretch.output_frames()), (0, 0));
        assert!(stretch.pending.is_empty() && stretch.input.is_empty());
        stretch.push(&[[0.; 2]; 17]).unwrap();
        stretch.finish().unwrap();
        let chunk = stretch.pull(100).unwrap();
        assert_eq!(chunk.output_start, 0);
        assert_eq!(chunk.samples, vec![[0.; 2]; 9]);
        assert!(chunk.finished);
        for rate in [0., 0.1, 0.9, 4., f64::INFINITY, f64::NAN] {
            assert!(AudioTimeStretch::new(rate).is_err());
        }
    }
}
