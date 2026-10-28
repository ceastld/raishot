//! 有界立体声流式重采样；整数输出 ordinal 决定相位和尾长，设备提交方管理媒体时钟。
use crate::Result;
use std::{collections::VecDeque, f64::consts::PI};

pub(crate) const MAX_INPUT_FRAMES: usize = 12_000;
pub(crate) const MAX_PUSH_FRAMES: usize = 4_800;
pub(crate) const MAX_PULL_FRAMES: usize = 4_800;
const TAPS: usize = 64;
const PHASES: usize = 1_024;
const LEFT: u64 = 31;
const RIGHT: u64 = 32;
type StereoFrame = [f32; 2];
type FilterTable = Vec<[f64; TAPS]>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ResampleQuality {
    #[default]
    Standard,
    High,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResampleChunk {
    pub output_start: u64,
    pub samples: Vec<StereoFrame>,
    pub needs_input: bool,
    pub finished: bool,
}

pub(crate) struct AudioResampler {
    input_rate: u32,
    output_rate: u32,
    quality: ResampleQuality,
    filter: Option<FilterTable>,
    input: VecDeque<StereoFrame>,
    input_start: u64,
    accepted_input: u64,
    delivered_output: u64,
    final_output: Option<u64>,
    first: Option<StereoFrame>,
    last: Option<StereoFrame>,
}

impl AudioResampler {
    pub(crate) fn new(input_rate: u32, output_rate: u32, quality: ResampleQuality) -> Result<Self> {
        validate_rates(input_rate, output_rate)?;
        let filter = make_filter(input_rate, output_rate, quality)?;
        let mut input = VecDeque::new();
        input.try_reserve_exact(MAX_INPUT_FRAMES)?;
        Ok(Self {
            input_rate,
            output_rate,
            quality,
            filter,
            input,
            input_start: 0,
            accepted_input: 0,
            delivered_output: 0,
            final_output: None,
            first: None,
            last: None,
        })
    }

    /// 失败不改旧状态；相同配置复用滤波表，成功后清全部旧 PCM、边界值和 ordinal。
    pub(crate) fn reset(
        &mut self,
        input_rate: u32,
        output_rate: u32,
        quality: ResampleQuality,
    ) -> Result<()> {
        validate_rates(input_rate, output_rate)?;
        if (self.input_rate, self.output_rate, self.quality) != (input_rate, output_rate, quality) {
            self.filter = make_filter(input_rate, output_rate, quality)?;
        }
        self.input_rate = input_rate;
        self.output_rate = output_rate;
        self.quality = quality;
        self.input.clear();
        self.input_start = 0;
        self.accepted_input = 0;
        self.delivered_output = 0;
        self.final_output = None;
        self.first = None;
        self.last = None;
        Ok(())
    }

    /// 只消费返回帧数的前缀；0 表示背压。调用方保留余段，错误时没有消费。
    pub(crate) fn push(&mut self, frames: &[StereoFrame]) -> Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        if self.final_output.is_some() {
            return Err(tr!("重采样输入已经结束", "Resampler input is already finished").into());
        }
        let count = frames
            .len()
            .min(MAX_PUSH_FRAMES)
            .min(MAX_INPUT_FRAMES - self.input.len());
        let accepted = self.accepted_input.checked_add(count as u64).ok_or(tr!(
            "重采样输入帧数溢出",
            "Resampler input frame count overflow"
        ))?;
        for frame in &frames[..count] {
            let frame = frame.map(|value| if value.is_finite() { value } else { 0. });
            self.first.get_or_insert(frame);
            self.last = Some(frame);
            self.input.push_back(frame);
        }
        self.accepted_input = accepted;
        Ok(count)
    }

    /// 仅记尾界；滤波支持范围的首尾延拓不增加 round(N*out/in) 的最终输出帧数。
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.final_output.is_none() {
            self.final_output = Some(u64::try_from(self.available_output_length())?);
        }
        Ok(())
    }

    /// 单次最多 MAX_PULL_FRAMES，每帧 High 固定 64 taps；分配/溢出失败不消费状态。
    pub(crate) fn pull(&mut self, max_frames: usize) -> Result<ResampleChunk> {
        if max_frames == 0 || max_frames > MAX_PULL_FRAMES {
            return Err(tr!(
                "重采样输出请求超出范围",
                "Resampler output request is out of range"
            )
            .into());
        }
        self.delivered_output
            .checked_add(max_frames as u64)
            .ok_or(tr!(
                "重采样输出帧数溢出",
                "Resampler output frame count overflow"
            ))?;
        let mut samples = Vec::new();
        samples.try_reserve_exact(max_frames)?;
        let output_start = self.delivered_output;
        while samples.len() < max_frames {
            let ordinal = output_start + samples.len() as u64;
            if !self.can_produce(ordinal) {
                break;
            }
            samples.push(self.sample(ordinal));
        }
        self.delivered_output += samples.len() as u64;
        self.prune();
        Ok(ResampleChunk {
            output_start,
            samples,
            needs_input: self.final_output.is_none() && !self.can_produce(self.delivered_output),
            finished: self.final_output == Some(self.delivered_output),
        })
    }

    pub(crate) fn input_frames(&self) -> u64 {
        self.accepted_input
    }

    pub(crate) fn output_frames(&self) -> u64 {
        self.delivered_output
    }

    fn available_output_length(&self) -> u128 {
        (u128::from(self.accepted_input) * u128::from(self.output_rate)
            + u128::from(self.input_rate / 2))
            / u128::from(self.input_rate)
    }

    fn position(&self, ordinal: u64) -> (u128, u32) {
        let scaled = u128::from(ordinal) * u128::from(self.input_rate);
        (
            scaled / u128::from(self.output_rate),
            (scaled % u128::from(self.output_rate)) as u32,
        )
    }

    fn can_produce(&self, ordinal: u64) -> bool {
        if let Some(end) = self.final_output {
            return ordinal < end;
        }
        // 下采样不能提早输出首帧：极短输入的最终 round(N*out/in) 可能为零。
        if u128::from(ordinal) >= self.available_output_length() {
            return false;
        }
        let (base, remainder) = self.position(ordinal);
        let lookahead = if self.input_rate == self.output_rate {
            0
        } else if self.filter.is_some() {
            RIGHT
        } else {
            u64::from(remainder != 0)
        };
        base + u128::from(lookahead) < u128::from(self.accepted_input)
    }

    fn sample(&self, ordinal: u64) -> StereoFrame {
        let (base, remainder) = self.position(ordinal);
        let base = base as i128;
        if self.input_rate == self.output_rate {
            return self.frame_at(base);
        }
        if let Some(filter) = &self.filter {
            let phase =
                (u64::from(remainder) * PHASES as u64 / u64::from(self.output_rate)) as usize;
            let mut sum = [0_f64; 2];
            for (tap, coefficient) in filter[phase].iter().enumerate() {
                let frame = self.frame_at(base + tap as i128 - i128::from(LEFT));
                for channel in 0..2 {
                    sum[channel] += f64::from(frame[channel]) * coefficient;
                }
            }
            sum.map(finite_sample)
        } else {
            let a = self.frame_at(base);
            if remainder == 0 {
                return a;
            }
            let b = self.frame_at(base + 1);
            let fraction = f64::from(remainder) / f64::from(self.output_rate);
            std::array::from_fn(|channel| {
                finite_sample(
                    f64::from(a[channel]) * (1. - fraction) + f64::from(b[channel]) * fraction,
                )
            })
        }
    }

    fn frame_at(&self, ordinal: i128) -> StereoFrame {
        if ordinal < 0 {
            return self.first.unwrap_or([0.; 2]);
        }
        if ordinal >= i128::from(self.accepted_input) {
            return self.last.unwrap_or([0.; 2]);
        }
        (ordinal as u64)
            .checked_sub(self.input_start)
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| self.input.get(offset))
            .copied()
            .unwrap_or([0.; 2])
    }

    fn prune(&mut self) {
        if self.final_output == Some(self.delivered_output) {
            self.input.clear();
            self.input_start = self.accepted_input;
            return;
        }
        let (base, _) = self.position(self.delivered_output);
        let left = if self.filter.is_some() { LEFT } else { 0 };
        let keep = base
            .saturating_sub(u128::from(left))
            .min(u128::from(self.accepted_input)) as u64;
        let remove = keep
            .saturating_sub(self.input_start)
            .min(self.input.len() as u64) as usize;
        self.input.drain(..remove);
        self.input_start += remove as u64;
    }
}

fn validate_rates(input: u32, output: u32) -> Result<()> {
    if !(8_000..=384_000).contains(&input) || !(8_000..=384_000).contains(&output) {
        return Err(tr!(
            "重采样采样率必须为 8–384 kHz",
            "Resampler rates must be 8–384 kHz"
        )
        .into());
    }
    Ok(())
}

fn make_filter(input: u32, output: u32, quality: ResampleQuality) -> Result<Option<FilterTable>> {
    if quality == ResampleQuality::Standard || input == output {
        return Ok(None);
    }
    let cutoff = 0.47 * (f64::from(output) / f64::from(input)).min(1.);
    let mut table = Vec::new();
    table.try_reserve_exact(PHASES)?;
    for phase in 0..PHASES {
        let fraction = phase as f64 / PHASES as f64;
        let mut coefficients = [0_f64; TAPS];
        for (tap, coefficient) in coefficients.iter_mut().enumerate() {
            let distance = tap as f64 - LEFT as f64 - fraction;
            if distance.abs() >= RIGHT as f64 {
                continue;
            }
            let window = 0.5 * (1. + (PI * distance / RIGHT as f64).cos());
            let sinc = if distance.abs() < 1e-12 {
                2. * cutoff
            } else {
                (2. * PI * cutoff * distance).sin() / (PI * distance)
            };
            *coefficient = sinc * window;
        }
        let total: f64 = coefficients.iter().sum();
        if !total.is_finite() || total.abs() <= f64::EPSILON {
            return Err(tr!("重采样滤波器无效", "Invalid resampler filter").into());
        }
        for coefficient in &mut coefficients {
            *coefficient /= total;
        }
        table.push(coefficients);
    }
    Ok(Some(table))
}

fn finite_sample(value: f64) -> f32 {
    value.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn render(
        input_rate: u32,
        output_rate: u32,
        quality: ResampleQuality,
        input: &[StereoFrame],
        push_size: usize,
    ) -> Vec<StereoFrame> {
        let mut resampler = AudioResampler::new(input_rate, output_rate, quality).unwrap();
        let mut position = 0;
        let mut closed = false;
        let mut output = Vec::new();
        for _ in 0..20_000 {
            let end = (position + push_size).min(input.len());
            let accepted = resampler.push(&input[position..end]).unwrap();
            position += accepted;
            if position == input.len() && !closed {
                resampler.finish().unwrap();
                closed = true;
            }
            let chunk = resampler.pull(MAX_PULL_FRAMES).unwrap();
            assert_eq!(chunk.output_start, output.len() as u64);
            assert!(resampler.input.len() <= MAX_INPUT_FRAMES);
            assert!(chunk.samples.len() <= MAX_PULL_FRAMES);
            let progress = accepted > 0 || !chunk.samples.is_empty() || chunk.finished;
            output.extend_from_slice(&chunk.samples);
            if chunk.finished {
                assert_eq!(resampler.input_frames(), input.len() as u64);
                assert_eq!(resampler.output_frames(), output.len() as u64);
                assert!(resampler.pull(7).unwrap().samples.is_empty());
                assert!(resampler.pull(7).unwrap().finished);
                return output;
            }
            assert!(progress, "resampler stalled");
        }
        panic!("resampler did not finish within test bound");
    }

    fn tone(frequency: f64, frames: usize) -> Vec<StereoFrame> {
        (0..frames)
            .map(|index| {
                let value = (TAU * frequency * index as f64 / 48_000.).sin() as f32 * 0.7;
                [value, -value]
            })
            .collect()
    }

    fn rms(input: &[StereoFrame]) -> f64 {
        let input = &input[100..input.len() - 100];
        (input
            .iter()
            .map(|frame| f64::from(frame[0]).powi(2))
            .sum::<f64>()
            / input.len() as f64)
            .sqrt()
    }

    #[test]
    fn resample_quality_identity_passes_finite_samples_exactly_in_both_modes() {
        let input: Vec<_> = (0..5_003)
            .map(|i| [i as f32 / 9_000., -(i as f32) / 8_000.])
            .collect();
        for quality in [ResampleQuality::Standard, ResampleQuality::High] {
            assert_eq!(render(48_000, 48_000, quality, &input, 137), input);
        }
    }

    #[test]
    fn resample_quality_high_reduces_downsample_alias_and_preserves_low_frequency() {
        let high_tone = tone(6_000., 4_800);
        let standard = render(48_000, 8_000, ResampleQuality::Standard, &high_tone, 911);
        let high = render(48_000, 8_000, ResampleQuality::High, &high_tone, 911);
        assert_eq!(standard.len(), 800);
        assert_eq!(high.len(), 800);
        assert!(rms(&standard) > 0.45);
        assert!(rms(&high) < rms(&standard) * 0.2);
        let low = render(48_000, 8_000, ResampleQuality::High, &tone(500., 4_800), 73);
        assert!((rms(&low) - 0.7 / 2_f64.sqrt()).abs() < 0.03);
        assert!(low.iter().all(|frame| (frame[0] + frame[1]).abs() < 1e-6));
    }

    #[test]
    fn resample_quality_rates_have_exact_lengths_and_independent_chunk_boundaries() {
        let input = tone(440., 4_800);
        for quality in [ResampleQuality::Standard, ResampleQuality::High] {
            for (rate, length) in [(44_100, 4_410), (48_000, 4_800), (96_000, 9_600)] {
                let small = render(48_000, rate, quality, &input, 1);
                let large = render(48_000, rate, quality, &input, 997);
                assert_eq!(small.len(), length);
                assert_eq!(small, large);
            }
            assert_eq!(
                render(8_000, 384_000, quality, &[[0.25, -0.5]; 17], 3).len(),
                816
            );
            assert_eq!(
                render(384_000, 8_000, quality, &[[0.25, -0.5]; 96], 11).len(),
                2
            );
        }
    }

    #[test]
    fn resample_quality_short_downsample_does_not_overproduce_before_finish() {
        for quality in [ResampleQuality::Standard, ResampleQuality::High] {
            let mut resampler = AudioResampler::new(48_000, 8_000, quality).unwrap();
            resampler.push(&[[1., -1.]]).unwrap();
            let chunk = resampler.pull(100).unwrap();
            assert!(chunk.samples.is_empty() && chunk.needs_input && !chunk.finished);
            resampler.finish().unwrap();
            assert!(resampler.pull(100).unwrap().finished);
            for frames in [0, 1, 17, 33] {
                let input = vec![[0.25, -0.5]; frames];
                let output = render(48_000, 96_000, quality, &input, 7);
                assert_eq!(output.len(), frames * 2);
                assert!(output.iter().flatten().all(|value| value.is_finite()));
            }
        }
    }

    #[test]
    fn resample_quality_backpressure_and_invalid_requests_preserve_owned_input() {
        let mut resampler = AudioResampler::new(48_000, 44_100, ResampleQuality::Standard).unwrap();
        let input = vec![[0.25, -0.5]; MAX_INPUT_FRAMES + 1];
        let mut position = 0;
        loop {
            let count = resampler.push(&input[position..]).unwrap();
            position += count;
            if count == 0 {
                break;
            }
        }
        assert_eq!(position, MAX_INPUT_FRAMES);
        assert!(resampler.pull(0).is_err());
        assert!(resampler.pull(MAX_PULL_FRAMES + 1).is_err());
        assert_eq!(resampler.input_frames(), position as u64);
        assert_eq!(resampler.pull(1).unwrap().samples, vec![[0.25, -0.5]]);
        assert_eq!(resampler.push(&input[position..]).unwrap(), 1);
        resampler.finish().unwrap();
        assert!(resampler.push(&[[1., 1.]]).is_err());
        assert_eq!(resampler.input_frames(), input.len() as u64);
    }

    #[test]
    fn resample_quality_reset_clears_filter_history_and_invalid_configuration_is_transactional() {
        let mut resampler = AudioResampler::new(48_000, 44_100, ResampleQuality::High).unwrap();
        resampler.push(&[[1., -1.]; 100]).unwrap();
        resampler.pull(1).unwrap();
        let before = (
            resampler.input_frames(),
            resampler.output_frames(),
            resampler.input.len(),
        );
        assert!(resampler
            .reset(0, 48_000, ResampleQuality::Standard)
            .is_err());
        assert_eq!(
            before,
            (
                resampler.input_frames(),
                resampler.output_frames(),
                resampler.input.len()
            )
        );
        resampler
            .reset(96_000, 44_100, ResampleQuality::High)
            .unwrap();
        resampler.push(&[[0.; 2]; 17]).unwrap();
        resampler.finish().unwrap();
        let chunk = resampler.pull(100).unwrap();
        assert_eq!(chunk.output_start, 0);
        assert_eq!(chunk.samples, vec![[0.; 2]; 8]);
        assert!(chunk.finished);
        resampler
            .reset(48_000, 48_000, ResampleQuality::Standard)
            .unwrap();
        resampler.push(&[[f32::NAN, f32::INFINITY]]).unwrap();
        resampler.finish().unwrap();
        assert_eq!(resampler.pull(1).unwrap().samples, vec![[0.; 2]]);
        for (input, output) in [(7_999, 48_000), (48_000, 384_001), (384_001, 8_000)] {
            assert!(AudioResampler::new(input, output, ResampleQuality::High).is_err());
        }
    }
}
