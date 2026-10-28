//! 有界立体声 PCM、原始采集时间锚与标称重采样相位；不替代设备时钟观测。
use super::OutputPhase;
use std::{collections::VecDeque, fmt};

const CHANNELS: usize = 2;
/// 单次最多返回 48 kHz 下十秒的 PCM；更长的消费请求由调用方分批处理。
pub(crate) const MAX_TAKE_FRAMES: usize = 48_000 * 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawAudioTime {
    pub endpoint_epoch: u64,
    pub device_position: u64,
    pub qpc_100ns: Option<u64>,
    pub device_rate: u32,
    pub input_frames: u32,
    pub discontinuity: bool,
}

/// 一个原始包产生的输出区间在本次消费中的片段。
/// raw_time 始终是原始包锚，裁剪不会移动它；output_phase 给出首输出相对原包的标称输入相位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureSegment {
    pub raw_time: RawAudioTime,
    pub output_phase: Option<OutputPhase>,
    pub packet_output_start: u64,
    pub packet_output_frames: u64,
    pub output_start: u64,
    pub frames: usize,
    /// 此片段在 CaptureRead.samples 中的立体声帧偏移，不包含尾部补零。
    pub sample_offset_frames: usize,
}

#[derive(Debug, PartialEq)]
pub(crate) struct CaptureRead {
    pub samples: Vec<f32>,
    pub real_frames: usize,
    pub segments: Vec<CaptureSegment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureBufferError {
    InvalidLimit,
    UnalignedSamples,
    InvalidPhase,
    TakeTooLarge { requested: usize },
    OrdinalOverflow,
    AllocationFailed,
}
impl fmt::Display for CaptureBufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::InvalidLimit => f.write_str(tr!(
                "音频缓存容量必须是可表示的正帧数",
                "Audio buffer capacity must be a representable positive frame count"
            )),
            Self::UnalignedSamples => f.write_str(tr!(
                "音频采样数不是完整立体声帧",
                "Audio samples do not contain complete stereo frames"
            )),
            Self::InvalidPhase => f.write_str(tr!(
                "音频重采样相位与原始包不匹配",
                "Audio resampling phase does not match its source packet"
            )),
            Self::TakeTooLarge { requested } => f.write_str(&trf!(
                "单次音频读取 {requested} 帧超过上限 {MAX_TAKE_FRAMES}",
                "Audio read of {requested} frames exceeds the limit of {MAX_TAKE_FRAMES}"
            )),
            Self::OrdinalOverflow => f.write_str(tr!(
                "累计音频输出帧序号溢出",
                "The cumulative audio output frame ordinal overflowed"
            )),
            Self::AllocationFailed => {
                f.write_str(tr!("无法分配音频缓存", "Cannot allocate the audio buffer"))
            }
        }
    }
}
impl std::error::Error for CaptureBufferError {}
type BufferResult<T> = std::result::Result<T, CaptureBufferError>;

#[derive(Clone, Copy)]
struct PacketSpan {
    raw_time: RawAudioTime,
    output_phase: Option<OutputPhase>,
    packet_output_start: u64,
    packet_output_frames: u64,
    output_start: u64,
    frames: usize,
}

pub(crate) struct CaptureBuffer {
    limit_frames: usize,
    samples: VecDeque<f32>,
    packets: VecDeque<PacketSpan>,
    produced_frames: u64,
}
impl CaptureBuffer {
    /// 不预分配；容量还须能表示立体声 f32 的字节数。
    pub(crate) fn new(limit_frames: usize) -> BufferResult<Self> {
        let max_samples = isize::MAX as usize / std::mem::size_of::<f32>();
        if limit_frames == 0
            || limit_frames
                .checked_mul(CHANNELS)
                .is_none_or(|samples| samples > max_samples)
        {
            return Err(CaptureBufferError::InvalidLimit);
        }
        Ok(Self {
            limit_frames,
            samples: VecDeque::new(),
            packets: VecDeque::new(),
            produced_frames: 0,
        })
    }

    pub(crate) fn len_frames(&self) -> usize {
        self.samples.len() / CHANNELS
    }

    /// 累计真实重采样输出，包括后来丢弃的帧；消费补零不推进此序号。
    pub(crate) fn produced_frames(&self) -> u64 {
        self.produced_frames
    }

    /// PCM 只保存在 samples；每个非空包仅另存时间锚和序号区间。
    /// 失败时不消费旧数据、不推进序号；零输出包不占队列或元数据。
    pub(crate) fn push(&mut self, samples: &[f32], raw_time: RawAudioTime) -> BufferResult<()> {
        self.push_with_phase(samples, raw_time, None)
    }

    pub(crate) fn push_with_phase(
        &mut self,
        samples: &[f32],
        raw_time: RawAudioTime,
        output_phase: Option<OutputPhase>,
    ) -> BufferResult<()> {
        if samples.len() % CHANNELS != 0 {
            return Err(CaptureBufferError::UnalignedSamples);
        }
        let frames = samples.len() / CHANNELS;
        if let Some(phase) = output_phase {
            if !phase.is_valid()
                || phase.output_frames != frames
                || phase.input_rate != raw_time.device_rate
                || phase.output_rate != 48_000
                || raw_time.input_frames == 0
                || i128::from(phase.first_input_offset_num)
                    + (frames.saturating_sub(1) as i128) * i128::from(phase.input_rate)
                    > i128::from(raw_time.input_frames.saturating_sub(1))
                        * i128::from(phase.output_rate)
            {
                return Err(CaptureBufferError::InvalidPhase);
            }
        }
        if frames == 0 {
            return Ok(());
        }
        let packet_frames =
            u64::try_from(frames).map_err(|_| CaptureBufferError::OrdinalOverflow)?;
        let next_produced = self
            .produced_frames
            .checked_add(packet_frames)
            .ok_or(CaptureBufferError::OrdinalOverflow)?;
        let kept_new = frames.min(self.limit_frames);
        let skipped_new = frames - kept_new;
        let dropped_old = self
            .len_frames()
            .saturating_sub(self.limit_frames - kept_new);
        let desired_frames = self.len_frames() - dropped_old + kept_new;
        let desired_samples = desired_frames * CHANNELS;
        self.samples
            .try_reserve(desired_samples.saturating_sub(self.samples.len()))
            .map_err(|_| CaptureBufferError::AllocationFailed)?;
        let desired_packets = if frames >= self.limit_frames {
            1
        } else {
            self.packets.len().saturating_add(1).min(self.limit_frames)
        };
        self.packets
            .try_reserve(desired_packets.saturating_sub(self.packets.len()))
            .map_err(|_| CaptureBufferError::AllocationFailed)?;

        drop(self.samples.drain(..dropped_old * CHANNELS));
        self.consume_spans(dropped_old, |_| {});
        self.samples
            .extend(samples[skipped_new * CHANNELS..].iter().copied());
        self.packets.push_back(PacketSpan {
            raw_time,
            output_phase,
            packet_output_start: self.produced_frames,
            packet_output_frames: packet_frames,
            output_start: self.produced_frames + skipped_new as u64,
            frames: kept_new,
        });
        self.produced_frames = next_produced;
        Ok(())
    }

    /// 仅提取当前真实 PCM，不为缺口分配或补静音；可用于按目标时间放置的消费者。
    pub(crate) fn take_available(&mut self, max_frames: usize) -> BufferResult<CaptureRead> {
        if max_frames > MAX_TAKE_FRAMES {
            return Err(CaptureBufferError::TakeTooLarge {
                requested: max_frames,
            });
        }
        self.take(max_frames.min(self.len_frames()))
    }

    /// 不足部分仅在返回值末尾补零，没有时间片段，也不计入 real_frames。
    /// 超过上限或分配失败返回错误，不分配无限静音、不消费已缓存数据。
    pub(crate) fn take(&mut self, frames: usize) -> BufferResult<CaptureRead> {
        if frames > MAX_TAKE_FRAMES {
            return Err(CaptureBufferError::TakeTooLarge { requested: frames });
        }
        let count = frames
            .checked_mul(CHANNELS)
            .ok_or(CaptureBufferError::TakeTooLarge { requested: frames })?;
        let real_frames = frames.min(self.len_frames());
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(count)
            .map_err(|_| CaptureBufferError::AllocationFailed)?;
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(real_frames.min(self.packets.len()))
            .map_err(|_| CaptureBufferError::AllocationFailed)?;
        samples.extend(self.samples.drain(..real_frames * CHANNELS));
        samples.resize(count, 0.);
        self.consume_spans(real_frames, |segment| segments.push(segment));
        Ok(CaptureRead {
            samples,
            real_frames,
            segments,
        })
    }

    /// 暂停丢弃声音与元数据，保留输出序号；后续数据不能冒充暂停前的包。
    pub(crate) fn discard(&mut self) {
        self.samples.clear();
        self.packets.clear();
    }

    fn consume_spans(&mut self, mut frames: usize, mut visit: impl FnMut(CaptureSegment)) {
        let mut sample_offset_frames = 0;
        while frames > 0 {
            let packet = self
                .packets
                .front_mut()
                .expect("PCM frames and packet spans must remain aligned");
            let count = frames.min(packet.frames);
            visit(CaptureSegment {
                raw_time: packet.raw_time,
                output_phase: packet.output_phase,
                packet_output_start: packet.packet_output_start,
                packet_output_frames: packet.packet_output_frames,
                output_start: packet.output_start,
                frames: count,
                sample_offset_frames,
            });
            packet.output_start += count as u64;
            packet.frames -= count;
            if packet.frames == 0 {
                self.packets.pop_front();
            }
            frames -= count;
            sample_offset_frames += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_buffer_available_reads_do_not_pad_or_consume_after_limit_error() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        buffer.push(&pcm(0, 2), time(0)).unwrap();
        assert_eq!(
            buffer.take_available(MAX_TAKE_FRAMES + 1),
            Err(CaptureBufferError::TakeTooLarge {
                requested: MAX_TAKE_FRAMES + 1
            })
        );
        assert_eq!(buffer.len_frames(), 2);
        let first = buffer.take_available(1).unwrap();
        assert_eq!(first.samples, pcm(0, 1));
        assert_eq!(first.real_frames, 1);
        let remaining = buffer.take_available(MAX_TAKE_FRAMES).unwrap();
        assert_eq!(remaining.samples, pcm(1, 1));
        assert_eq!(remaining.real_frames, 1);
        assert_eq!(remaining.segments[0].output_start, 1);
        let empty = buffer.take_available(MAX_TAKE_FRAMES).unwrap();
        assert!(empty.samples.is_empty());
        assert!(empty.segments.is_empty());
        assert_eq!(empty.real_frames, 0);
    }

    #[test]
    fn capture_buffer_trimming_retains_original_output_phase_and_anchor() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        let raw = RawAudioTime {
            endpoint_epoch: 1,
            device_position: 0,
            qpc_100ns: Some(1_000_000),
            device_rate: 48_000,
            input_frames: 6,
            discontinuity: false,
        };
        let phase = OutputPhase {
            first_input_offset_num: 0,
            input_rate: 48_000,
            output_rate: 48_000,
            output_frames: 6,
        };
        buffer
            .push_with_phase(&pcm(0, 6), raw, Some(phase))
            .unwrap();
        let first = buffer.take(2).unwrap();
        let segment = first.segments[0];
        assert_eq!(segment.raw_time, raw);
        assert_eq!(segment.output_phase, Some(phase));
        assert_eq!((segment.packet_output_start, segment.output_start), (0, 2));
        assert_eq!(
            phase.qpc_bounds(
                raw.qpc_100ns.unwrap(),
                (segment.output_start - segment.packet_output_start) as usize
            ),
            Some((1_000_416, 1_000_417))
        );
        let second = buffer.take(2).unwrap();
        assert_eq!(second.segments[0].output_phase, Some(phase));
        assert_eq!(second.segments[0].output_start, 4);
        assert_eq!(buffer.produced_frames(), 6);
    }

    #[test]
    fn capture_buffer_invalid_phase_does_not_consume_or_replace_pcm() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        buffer.push(&pcm(0, 2), time(0)).unwrap();
        let raw = RawAudioTime {
            input_frames: 1,
            ..time(1)
        };
        let phase = OutputPhase {
            first_input_offset_num: 0,
            input_rate: raw.device_rate,
            output_rate: 48_000,
            output_frames: 2,
        };
        assert_eq!(
            buffer.push_with_phase(&pcm(2, 2), raw, Some(phase)),
            Err(CaptureBufferError::InvalidPhase)
        );
        assert_eq!(buffer.produced_frames(), 2);
        assert_eq!(buffer.take(2).unwrap().samples, pcm(0, 2));
    }

    fn time(packet: u64) -> RawAudioTime {
        RawAudioTime {
            endpoint_epoch: 0,
            device_position: packet * 441,
            qpc_100ns: Some(10_000_000 + packet * 100_000),
            device_rate: 44_100,
            input_frames: 441,
            discontinuity: false,
        }
    }

    fn pcm(start: usize, frames: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|frame| [frame as f32, -(frame as f32)])
            .collect()
    }

    #[test]
    fn capture_buffer_overflow_trims_pcm_and_packet_spans_by_whole_frames() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        buffer.push(&pcm(0, 6), time(0)).unwrap();
        buffer.push(&pcm(6, 2), time(1)).unwrap();
        assert_eq!((buffer.len_frames(), buffer.produced_frames()), (4, 8));
        let read = buffer.take(4).unwrap();
        assert_eq!(read.samples, pcm(4, 4));
        assert_eq!(read.real_frames, 4);
        assert_eq!(read.segments.len(), 2);
        assert_eq!(
            read.segments[0],
            CaptureSegment {
                raw_time: time(0),
                output_phase: None,
                packet_output_start: 0,
                packet_output_frames: 6,
                output_start: 4,
                frames: 2,
                sample_offset_frames: 0,
            }
        );
        assert_eq!(
            read.segments[1],
            CaptureSegment {
                raw_time: time(1),
                output_phase: None,
                packet_output_start: 6,
                packet_output_frames: 2,
                output_start: 6,
                frames: 2,
                sample_offset_frames: 2,
            }
        );
        assert!(buffer.packets.is_empty());
    }

    #[test]
    fn capture_buffer_partial_takes_preserve_original_anchor_and_output_span() {
        let mut buffer = CaptureBuffer::new(8).unwrap();
        buffer.push(&pcm(0, 3), time(0)).unwrap();
        buffer.push(&pcm(3, 3), time(1)).unwrap();
        let first = buffer.take(2).unwrap();
        assert_eq!(first.samples, pcm(0, 2));
        assert_eq!(
            (first.segments[0].output_start, first.segments[0].frames),
            (0, 2)
        );
        let second = buffer.take(3).unwrap();
        assert_eq!(second.samples, pcm(2, 3));
        assert_eq!(second.segments.len(), 2);
        let (tail, next) = (second.segments[0], second.segments[1]);
        assert_eq!(
            (
                tail.raw_time,
                tail.packet_output_start,
                tail.packet_output_frames
            ),
            (time(0), 0, 3)
        );
        assert_eq!(
            (tail.output_start, tail.frames, tail.sample_offset_frames),
            (2, 1, 0)
        );
        assert_eq!(
            (
                next.raw_time,
                next.packet_output_start,
                next.packet_output_frames
            ),
            (time(1), 3, 3)
        );
        assert_eq!(
            (next.output_start, next.frames, next.sample_offset_frames),
            (3, 2, 1)
        );
        assert_eq!((buffer.len_frames(), buffer.produced_frames()), (1, 6));
    }

    #[test]
    fn capture_buffer_padding_is_separate_from_real_zero_samples() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        buffer.push(&pcm(0, 2), time(0)).unwrap();
        let read = buffer.take(5).unwrap();
        assert_eq!(read.samples, [0., 0., 1., -1., 0., 0., 0., 0., 0., 0.]);
        assert_eq!(read.real_frames, 2);
        assert_eq!(read.segments.len(), 1);
        assert_eq!(read.segments[0].frames, 2);
        let empty = buffer.take(2).unwrap();
        assert_eq!(empty.samples, [0.; 4]);
        assert_eq!(empty.real_frames, 0);
        assert!(empty.segments.is_empty());
        assert_eq!(buffer.produced_frames(), 2);
    }

    #[test]
    fn capture_buffer_discard_preserves_ordinal_and_new_endpoint_raw_time() {
        let mut buffer = CaptureBuffer::new(4).unwrap();
        buffer.push(&pcm(0, 3), time(0)).unwrap();
        buffer.discard();
        assert_eq!((buffer.len_frames(), buffer.produced_frames()), (0, 3));
        assert!(buffer.packets.is_empty());
        let next_time = RawAudioTime {
            endpoint_epoch: 1,
            device_position: 0,
            qpc_100ns: None,
            discontinuity: true,
            ..time(0)
        };
        buffer.push(&pcm(3, 2), next_time).unwrap();
        let read = buffer.take(3).unwrap();
        assert_eq!(read.real_frames, 2);
        assert_eq!(read.segments[0].raw_time, next_time);
        assert_eq!(
            (
                read.segments[0].packet_output_start,
                read.segments[0].output_start
            ),
            (3, 3)
        );
        assert_eq!(buffer.produced_frames(), 5);
    }

    #[test]
    fn capture_buffer_zero_output_uses_no_metadata_and_nonempty_packets_stay_bounded() {
        let mut buffer = CaptureBuffer::new(3).unwrap();
        for packet in 0..100 {
            buffer.push(&[], time(packet)).unwrap();
        }
        assert_eq!(
            (
                buffer.len_frames(),
                buffer.produced_frames(),
                buffer.packets.len()
            ),
            (0, 0, 0)
        );
        for packet in 0..20 {
            buffer.push(&pcm(packet as usize, 1), time(packet)).unwrap();
            buffer.push(&[], time(99)).unwrap();
            assert!(buffer.packets.len() <= buffer.limit_frames);
        }
        let read = buffer.take(3).unwrap();
        assert_eq!(read.samples, pcm(17, 3));
        assert_eq!(read.segments.len(), 3);
        assert_eq!(read.segments[0].raw_time, time(17));
        assert_eq!(read.segments[0].output_start, 17);
        assert_eq!(buffer.produced_frames(), 20);
    }

    #[test]
    fn capture_buffer_rejects_unaligned_oversized_and_overflowing_requests_transactionally() {
        assert!(matches!(
            CaptureBuffer::new(0),
            Err(CaptureBufferError::InvalidLimit)
        ));
        assert!(matches!(
            CaptureBuffer::new(usize::MAX),
            Err(CaptureBufferError::InvalidLimit)
        ));
        let mut buffer = CaptureBuffer::new(2).unwrap();
        buffer.push(&pcm(0, 1), time(0)).unwrap();
        assert_eq!(
            buffer.push(&[1.], time(1)),
            Err(CaptureBufferError::UnalignedSamples)
        );
        for requested in [MAX_TAKE_FRAMES + 1, usize::MAX] {
            assert_eq!(
                buffer.take(requested),
                Err(CaptureBufferError::TakeTooLarge { requested })
            );
        }
        assert_eq!((buffer.len_frames(), buffer.produced_frames()), (1, 1));
        assert_eq!(buffer.take(1).unwrap().samples, pcm(0, 1));
        buffer.produced_frames = u64::MAX - 1;
        assert_eq!(
            buffer.push(&pcm(0, 2), time(1)),
            Err(CaptureBufferError::OrdinalOverflow)
        );
        assert_eq!(
            (
                buffer.len_frames(),
                buffer.produced_frames(),
                buffer.packets.len()
            ),
            (0, u64::MAX - 1, 0)
        );
        buffer.push(&pcm(0, 1), time(1)).unwrap();
        assert_eq!(
            buffer.take(1).unwrap().segments[0].output_start,
            u64::MAX - 1
        );
    }
}
