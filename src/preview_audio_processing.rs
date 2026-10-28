//! 有界音频处理级联；解码前瞻与已交付设备的媒体时间分别计数。
use super::resample_quality::{AudioResampler, ResampleQuality};
use super::time_stretch::{AudioTimeStretch, SAMPLE_RATE};
use super::{current, State, Stream};
use crate::Result;
use std::{
    sync::{atomic::AtomicBool, Mutex},
    time::Duration,
};

const INPUT_BATCH: usize = 480;
const MAX_PROCESS_FRAMES: usize = 4_800;
const MAX_RENDER_ROUNDS: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PitchMode {
    /// 改变采样播放速度，音高随倍率变化。
    #[default]
    Resample,
    PitchPreserving,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioProcessing {
    pub pitch: PitchMode,
    /// 原生 PCM 到混音网格、混音到输出设备的重采样质量。
    pub quality: ResampleQuality,
}

pub(super) fn effective_input_rate(
    settings: AudioProcessing,
    rate: f64,
    source_grid_rate: u32,
) -> Result<u32> {
    settings.validate(rate)?;
    if !(8_000..=384_000).contains(&source_grid_rate)
        || settings.pitch == PitchMode::PitchPreserving && source_grid_rate != SAMPLE_RATE
    {
        return Err(tr!("音频处理网格不受支持", "Unsupported audio processing grid").into());
    }
    let effective = if settings.pitch == PitchMode::PitchPreserving {
        f64::from(SAMPLE_RATE)
    } else {
        (f64::from(source_grid_rate) * rate).round()
    };
    if !effective.is_finite() || !(8_000.0..=384_000.0).contains(&effective) {
        return Err(tr!(
            "有效音频采样率超出范围",
            "Effective audio sample rate is out of range"
        )
        .into());
    }
    Ok(effective as u32)
}
impl AudioProcessing {
    pub(super) fn active(self) -> bool {
        self != Self::default()
    }

    pub(super) fn validate(self, rate: f64) -> Result<()> {
        if !self.active() {
            return Ok(());
        }
        if !rate.is_finite() || !(0.5..=2.).contains(&rate) {
            return Err(tr!(
                "此音频处理模式支持 0.5–2 倍速",
                "This audio processing mode supports rates from 0.5 to 2"
            )
            .into());
        }
        if self.pitch == PitchMode::PitchPreserving
            && ![0.5, 0.75, 1., 1.25, 1.5, 2.].contains(&rate)
        {
            return Err(tr!(
                "不支持此保调倍率",
                "Unsupported pitch-preserving playback rate"
            )
            .into());
        }
        Ok(())
    }
}

pub(super) struct ProcessedSamples {
    pub samples: Vec<[f32; 2]>,
    /// 只由交付的输出 ordinal 映射；解码前瞻不能推进设备时钟。
    pub media_end: Duration,
    pub finished: bool,
}

pub(super) struct ProcessingStream {
    stretch: Option<AudioTimeStretch>,
    resample: AudioResampler,
    origin: Duration,
    rate: f64,
    output_rate: u32,
    source_grid_rate: u32,
    input_frames: u64,
    source_finished: bool,
    resample_finished: bool,
    raw_pending: Vec<[f32; 2]>,
    raw_offset: usize,
    stretch_pending: Vec<[f32; 2]>,
    stretch_offset: usize,
    stretch_finished: bool,
}

impl ProcessingStream {
    #[cfg(test)]
    pub(super) fn new(
        settings: AudioProcessing,
        rate: f64,
        output_rate: u32,
        origin: Duration,
    ) -> Result<Self> {
        Self::new_with_source_rate(settings, rate, SAMPLE_RATE, output_rate, origin)
    }

    pub(super) fn new_with_source_rate(
        settings: AudioProcessing,
        rate: f64,
        source_grid_rate: u32,
        output_rate: u32,
        origin: Duration,
    ) -> Result<Self> {
        let input_rate = effective_input_rate(settings, rate, source_grid_rate)?;
        let stretch = match settings.pitch {
            PitchMode::PitchPreserving => Some(AudioTimeStretch::new(rate)?),
            PitchMode::Resample => None,
        };
        Ok(Self {
            stretch,
            resample: AudioResampler::new(input_rate, output_rate, settings.quality)?,
            origin,
            rate,
            output_rate,
            source_grid_rate,
            input_frames: 0,
            source_finished: false,
            resample_finished: false,
            raw_pending: Vec::new(),
            raw_offset: 0,
            stretch_pending: Vec::new(),
            stretch_offset: 0,
            stretch_finished: false,
        })
    }

    fn source_time(&self) -> Result<Duration> {
        self.origin
            .checked_add(Duration::from_secs(
                self.input_frames / u64::from(self.source_grid_rate),
            ))
            .and_then(|time| {
                time.checked_add(Duration::from_nanos(
                    self.input_frames % u64::from(self.source_grid_rate) * 1_000_000_000
                        / u64::from(self.source_grid_rate),
                ))
            })
            .ok_or_else(|| tr!("音频媒体时间溢出", "Audio media time overflow").into())
    }

    /// 返回 None 表示旧控制代次取消；输入余段和处理器余段在本对象内有界保留。
    pub(super) fn render(
        &mut self,
        streams: &mut [Stream],
        state: &Mutex<State>,
        stop: &AtomicBool,
        snapshot: &State,
        max_frames: usize,
    ) -> Result<Option<ProcessedSamples>> {
        let mut result = self.render_with_source(state, stop, snapshot, max_frames, |time| {
            let position = time.as_secs_f64();
            if streams.iter().all(|stream| stream.exhausted(position)) {
                return Ok(None);
            }
            let mut mixed = [0.; 2];
            for stream in streams.iter_mut() {
                let Some(frame) = stream.sample(position, state, stop, snapshot.generation)? else {
                    return Ok(None);
                };
                let gain = stream
                    .source
                    .map_or(1., |source| snapshot.levels.gain(source));
                for channel in 0..2 {
                    mixed[channel] += frame[channel] * gain;
                }
            }
            Ok((!streams.iter().all(|stream| stream.exhausted(position))).then_some(mixed))
        })?;
        // 已解码前缀的真实尾只会向前扩展；提前约束网格 ceil，EOF 时队列上界也不会倒退。
        if let Some(actual_end) = streams.iter().filter_map(Stream::actual_end).max() {
            self.clamp_source_end(&mut result, actual_end);
        }
        Ok(result)
    }

    fn clamp_source_end(&self, result: &mut Option<ProcessedSamples>, actual_end: Duration) {
        if let Some(processed) = result {
            processed.media_end = processed.media_end.min(actual_end.max(self.origin));
        }
    }

    fn render_with_source(
        &mut self,
        state: &Mutex<State>,
        stop: &AtomicBool,
        snapshot: &State,
        max_frames: usize,
        mut source: impl FnMut(Duration) -> Result<Option<[f32; 2]>>,
    ) -> Result<Option<ProcessedSamples>> {
        if max_frames == 0 || max_frames > 384_000 / 20 {
            return Err(tr!(
                "音频处理输出请求超出范围",
                "Audio processing output request is out of range"
            )
            .into());
        }
        let mut samples = Vec::new();
        samples.try_reserve_exact(max_frames)?;
        let mut finished = false;
        let mut output_end = self.resample.output_frames();
        for _ in 0..MAX_RENDER_ROUNDS {
            if !current(state, stop, snapshot.generation) {
                return Ok(None);
            }
            let chunk = self
                .resample
                .pull((max_frames - samples.len()).min(MAX_PROCESS_FRAMES))?;
            output_end = chunk
                .output_start
                .checked_add(chunk.samples.len() as u64)
                .ok_or(tr!("音频输出帧数溢出", "Audio output frame count overflow"))?;
            samples.extend(chunk.samples);
            finished = chunk.finished;
            if finished || samples.len() == max_frames {
                break;
            }
            if !chunk.needs_input {
                continue;
            }
            if self.stretch.is_some() {
                if self.stretch_offset < self.stretch_pending.len() {
                    let count = self
                        .resample
                        .push(&self.stretch_pending[self.stretch_offset..])?;
                    self.stretch_offset += count;
                    continue;
                }
                self.stretch_pending.clear();
                self.stretch_offset = 0;
                if self.stretch_finished {
                    self.finish_resampler()?;
                    continue;
                }
                let chunk = self.stretch.as_mut().unwrap().pull(MAX_PROCESS_FRAMES)?;
                self.stretch_finished = chunk.finished;
                self.stretch_pending = chunk.samples;
                if !self.stretch_pending.is_empty() || self.stretch_finished || !chunk.needs_input {
                    continue;
                }
            }
            if self.raw_offset < self.raw_pending.len() {
                let count = match &mut self.stretch {
                    Some(stretch) => stretch.push(&self.raw_pending[self.raw_offset..])?,
                    None => self.resample.push(&self.raw_pending[self.raw_offset..])?,
                };
                self.raw_offset += count;
                continue;
            }
            self.raw_pending.clear();
            self.raw_offset = 0;
            if self.source_finished {
                if let Some(stretch) = &mut self.stretch {
                    stretch.finish()?;
                } else {
                    self.finish_resampler()?;
                }
                continue;
            }
            self.raw_pending.try_reserve_exact(INPUT_BATCH)?;
            for _ in 0..INPUT_BATCH {
                if !current(state, stop, snapshot.generation) {
                    return Ok(None);
                }
                let Some(frame) = source(self.source_time()?)? else {
                    if !current(state, stop, snapshot.generation) {
                        return Ok(None);
                    }
                    self.source_finished = true;
                    break;
                };
                self.raw_pending.push(frame);
                self.input_frames = self
                    .input_frames
                    .checked_add(1)
                    .ok_or(tr!("音频输入帧数溢出", "Audio input frame count overflow"))?;
            }
        }
        let mut media_end = self
            .origin
            .checked_add(
                Duration::try_from_secs_f64(
                    output_end as f64 / f64::from(self.output_rate) * self.rate,
                )
                .map_err(|_| tr!("音频媒体时间无效", "Invalid audio media time"))?,
            )
            .ok_or(tr!("音频媒体时间溢出", "Audio media time overflow"))?;
        // 两级整数尾长各有一次舍入；已知真实 EOF 后，最后一个输出帧也不能
        // 把媒体边界推进到真实源尾之后，否则下一次排空会让队列时间倒退。
        if self.source_finished {
            media_end = media_end.min(self.source_time()?);
        }
        Ok(Some(ProcessedSamples {
            samples,
            media_end,
            finished,
        }))
    }

    fn finish_resampler(&mut self) -> Result<()> {
        if !self.resample_finished {
            self.resample.finish()?;
            self.resample_finished = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{device_output_sample, lock_state, tests::control_audio};
    use super::super::{
        processing_stream, source_grid_rate,
        source_resample::{SourcePoll, SourceResampler},
    };
    use super::*;
    use crate::audio_capture::MixLevels;

    fn snapshot() -> State {
        State {
            playing: true,
            position: None,
            rate: 1.,
            levels: MixLevels::default(),
            volume: 1.,
            muted: false,
            processing: AudioProcessing::default(),
            generation: 7,
            transport_generation: 9,
            published: None,
            drained: None,
            projection: None,
            file: None,
        }
    }

    fn run_pipeline(
        settings: AudioProcessing,
        rate: f64,
        output_rate: u32,
        muted: bool,
    ) -> Vec<[f32; 2]> {
        let origin = Duration::from_secs(7);
        let mut processor = ProcessingStream::new(settings, rate, output_rate, origin).unwrap();
        let mut snapshot = snapshot();
        snapshot.rate = rate;
        snapshot.muted = muted;
        let state = Mutex::new(snapshot.clone());
        let stop = AtomicBool::new(false);
        let input_frames = 12_347_u64;
        let source_end = origin + Duration::from_nanos(input_frames * 1_000_000_000 / 48_000);
        let mut output = Vec::new();
        let mut last_media = origin;
        let mut saw_lookahead = false;
        for _ in 0..2_000 {
            let chunk = processor
                .render_with_source(
                    &state,
                    &stop,
                    &snapshot,
                    (output_rate / 20) as usize,
                    |time| {
                        let index =
                            ((time - origin).as_nanos() * 48_000).div_ceil(1_000_000_000) as u64;
                        if index >= input_frames {
                            return Ok(None);
                        }
                        let value = (std::f64::consts::TAU * 440. * index as f64 / 48_000.).sin()
                            as f32
                            * 0.25;
                        Ok(Some([value, -value]))
                    },
                )
                .unwrap()
                .unwrap();
            assert!(chunk.samples.len() <= (output_rate / 20) as usize);
            assert!(chunk.media_end >= last_media && chunk.media_end <= source_end);
            if !chunk.samples.is_empty() && !chunk.finished {
                saw_lookahead |= processor.source_time().unwrap() > chunk.media_end;
            }
            last_media = chunk.media_end;
            output.extend(chunk.samples);
            if chunk.finished {
                assert_eq!(processor.input_frames, input_frames);
                let expected = if settings.pitch == PitchMode::PitchPreserving {
                    ((input_frames as f64 / rate).round() * f64::from(output_rate) / 48_000.)
                        .round()
                } else {
                    (input_frames as f64 * f64::from(output_rate) / (48_000. * rate)).round()
                } as usize;
                assert_eq!(output.len(), expected);
                assert!(saw_lookahead);
                return output;
            }
        }
        panic!("有界级联未能排空");
    }

    #[test]
    fn preview_audio_processing_cascade_drains_without_counting_lookahead_as_played() {
        for (pitch, rate, output_rate) in [
            (PitchMode::PitchPreserving, 0.5, 44_100),
            (PitchMode::PitchPreserving, 2., 48_000),
            (PitchMode::Resample, 1.25, 96_000),
        ] {
            let output = run_pipeline(
                AudioProcessing {
                    pitch,
                    quality: ResampleQuality::High,
                },
                rate,
                output_rate,
                false,
            );
            assert!(output
                .iter()
                .all(|frame| frame[0].is_finite() && (frame[0] + frame[1]).abs() < 1e-6));
        }
    }

    fn native_cascade(rate: f64, frequency: f64) -> Vec<[f32; 2]> {
        let origin = Duration::from_secs(7);
        let mut snapshot = snapshot();
        snapshot.processing = AudioProcessing {
            pitch: PitchMode::Resample,
            quality: ResampleQuality::High,
        };
        snapshot.rate = rate;
        let state = Mutex::new(snapshot.clone());
        let stop = AtomicBool::new(false);
        let grid = source_grid_rate(snapshot.processing, rate, 48_000, 96_000).unwrap();
        let mut processor = processing_stream(&snapshot, 48_000, grid, origin)
            .unwrap()
            .unwrap();
        assert_eq!(processor.source_grid_rate, grid);
        let mut source = SourceResampler::new(96_000, 2, grid).unwrap();
        source.reset(origin).unwrap();
        let native_frames = 24_000;
        let mut native = Vec::new();
        for index in 0..native_frames {
            let value =
                (std::f64::consts::TAU * frequency * index as f64 / 96_000.).sin() as f32 * 0.25;
            native.extend([value, -value]);
        }
        let mut block_start = 0;
        let mut block_offset = 0;
        let mut output = Vec::new();
        let mut last_media = origin;
        for _ in 0..2_000 {
            let mut result = processor
                .render_with_source(&state, &stop, &snapshot, 2_400, |at| {
                    for _ in 0..2_000 {
                        match source.sample_at(at)? {
                            SourcePoll::Frame(frame) => return Ok(Some(frame)),
                            SourcePoll::Ended => return Ok(None),
                            SourcePoll::Pending => {}
                            SourcePoll::NeedInput => {
                                if block_start == native_frames {
                                    source.finish()?;
                                } else {
                                    let end = (block_start + 997).min(native_frames);
                                    let pts = origin
                                        + Duration::from_nanos(
                                            block_start as u64 * 1_000_000_000 / 96_000,
                                        );
                                    let progress = source.push_block(
                                        pts,
                                        &native[block_start * 2..end * 2],
                                        block_offset,
                                    )?;
                                    block_offset += progress.consumed_frames;
                                    if block_offset == end - block_start {
                                        block_start = end;
                                        block_offset = 0;
                                    }
                                }
                            }
                        }
                    }
                    panic!("真实原生 SRC 没有在单点预算内前进")
                })
                .unwrap();
            if let Some(actual_end) = source.end_time() {
                processor.clamp_source_end(&mut result, actual_end);
            }
            let processed = result.take().unwrap();
            assert!(processed.media_end >= last_media);
            assert!(processed.media_end <= origin + Duration::from_millis(250));
            last_media = processed.media_end;
            output.extend(processed.samples);
            if processed.finished {
                assert_eq!(block_start, native_frames);
                assert_eq!(block_offset, 0);
                assert_eq!(output.len(), (12_000. / rate).round() as usize);
                return output;
            }
        }
        panic!("真实原生 SRC→ProcessingStream 未能有限排空")
    }

    fn signal_amplitude(input: &[[f32; 2]], frequency: f64) -> f64 {
        let mut sine = 0.;
        let mut cosine = 0.;
        let interior = &input[100..input.len() - 100];
        for (index, frame) in interior.iter().enumerate() {
            let phase = std::f64::consts::TAU * frequency * (index + 100) as f64 / 48_000.;
            sine += f64::from(frame[0]) * phase.sin();
            cosine += f64::from(frame[0]) * phase.cos();
        }
        2. * sine.hypot(cosine) / interior.len() as f64
    }

    #[test]
    fn preview_audio_processing_production_grid_cascade_keeps_slow_native_high_tones() {
        let slow = native_cascade(0.5, 30_000.);
        assert!((signal_amplitude(&slow, 15_000.) - 0.25).abs() < 0.01);
        assert!(signal_amplitude(&slow, 9_000.) < 0.01);
        let normal = native_cascade(1., 30_000.);
        assert!(signal_amplitude(&normal, 18_000.) < 0.01);
        let fast = native_cascade(2., 1_000.);
        assert!((signal_amplitude(&fast, 2_000.) - 0.25).abs() < 0.01);
    }

    #[test]
    fn preview_audio_processing_applies_mute_after_stretch_and_filter_tail() {
        let output = run_pipeline(
            AudioProcessing {
                pitch: PitchMode::PitchPreserving,
                quality: ResampleQuality::High,
            },
            1.5,
            44_100,
            true,
        );
        // run_pipeline 已排尽真实源和两级 DSP 尾部，静音不能提前抹掉这些原始 PCM。
        assert!(!output.is_empty() && output.iter().any(|frame| frame[0].abs() > 0.01));
        let mut latest = snapshot();
        latest.muted = true;
        assert!(output.iter().all(|frame| {
            (0..2).all(|channel| device_output_sample(*frame, 2, channel, &latest) == 0.)
        }));
    }

    #[test]
    fn preview_audio_processing_commit_uses_latest_gain_without_changing_transport() {
        let audio = control_audio();
        let before = lock_state(&audio.state).clone();
        assert!(!before.muted && before.volume == 1.);
        let mut processor = ProcessingStream::new(
            AudioProcessing {
                pitch: PitchMode::PitchPreserving,
                quality: ResampleQuality::Standard,
            },
            1.,
            48_000,
            Duration::ZERO,
        )
        .unwrap();
        let processed = processor
            .render_with_source(&audio.state, &audio.stop, &before, 2400, |_| {
                Ok(Some([2., -2.]))
            })
            .unwrap()
            .unwrap();
        assert!(!processed.samples.is_empty());
        assert!(processed.samples.iter().all(|frame| *frame == [2., -2.]));

        // 模拟 MF/DSP 在锁外完成后、GetBuffer 前用户切换静音。
        audio.set_muted(true);
        {
            let latest = lock_state(&audio.state);
            assert_eq!(
                (latest.generation, latest.transport_generation),
                (before.generation, before.transport_generation)
            );
            assert!(processed.samples.iter().all(|frame| {
                (0..2).all(|channel| device_output_sample(*frame, 2, channel, &latest) == 0.)
            }));
        }
        audio.set_volume(0.25);
        audio.set_muted(false);
        let latest = lock_state(&audio.state);
        assert_eq!(
            (latest.generation, latest.transport_generation),
            (before.generation, before.transport_generation)
        );
        assert!(processed.samples.iter().all(|frame| {
            device_output_sample(*frame, 2, 0, &latest) == 0.5
                && device_output_sample(*frame, 2, 1, &latest) == -0.5
        }));
        assert_eq!(
            device_output_sample([f32::NAN, f32::INFINITY], 2, 0, &latest),
            0.
        );
        assert_eq!(device_output_sample([f32::MAX; 2], 1, 0, &latest), 1.);
    }

    #[test]
    fn preview_audio_processing_cancels_before_consuming_or_publishing_old_pcm() {
        let snapshot = snapshot();
        let state = Mutex::new(State {
            generation: 8,
            ..snapshot.clone()
        });
        let stop = AtomicBool::new(false);
        let mut processor = ProcessingStream::new(
            AudioProcessing {
                pitch: PitchMode::PitchPreserving,
                quality: ResampleQuality::Standard,
            },
            2.,
            48_000,
            Duration::ZERO,
        )
        .unwrap();
        assert!(processor
            .render_with_source(&state, &stop, &snapshot, 2400, |_| panic!(
                "旧代不能读取来源"
            ))
            .unwrap()
            .is_none());
        assert_eq!(processor.input_frames, 0);
        assert_eq!(processor.resample.output_frames(), 0);
    }
}
