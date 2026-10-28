//! 优先按解码时间交错拷贝原生 H.264 样本并编码混音；文件事务与 COM/MF 生命周期由调用方持有。
use super::{audio_type, open_file_source_reader, sample, AUDIO_CHANNELS, AUDIO_RATE};
use crate::Result;
use std::{path::Path, time::Duration};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::Media::MediaFoundation::*,
};

const MAX_PCM_SAMPLES: usize = 48_000;
const MAX_COMPRESSED_SAMPLE_BYTES: u32 = 64 * 1024 * 1024;
const MAX_EMPTY_VIDEO_READS: usize = 64;
const TICKS_PER_SECOND: u64 = 10_000_000;

struct VideoSample {
    interleave_key: i128,
    sample: IMFSample,
}

struct VideoInput {
    reader: IMFSourceReader,
    stream: u32,
    ended: bool,
}

impl VideoInput {
    fn next(&mut self, keep_going: &mut impl FnMut() -> bool) -> Result<Option<VideoSample>> {
        if self.ended {
            return Ok(None);
        }
        for _ in 0..MAX_EMPTY_VIDEO_READS {
            check_running(keep_going)?;
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            unsafe {
                self.reader.ReadSample(
                    self.stream,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )?;
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(tr!("读取视频压缩样本失败", "Failed to read compressed video").into());
            }
            if flags
                & (MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0
                    | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0) as u32
                != 0
            {
                return Err(tr!(
                    "暂不支持重混时视频格式发生变化",
                    "Video format changes during audio remix are not supported"
                )
                .into());
            }
            self.ended = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            if let Some(sample) = sample {
                let length = unsafe { sample.GetTotalLength()? };
                if length > MAX_COMPRESSED_SAMPLE_BYTES {
                    return Err(tr!(
                        "单个视频压缩样本超过 64 MiB",
                        "A compressed video sample exceeds 64 MiB"
                    )
                    .into());
                }
                if length != 0 {
                    let interleave_key = video_interleave_key(timestamp, unsafe {
                        sample.GetUINT64(&MFSampleExtension_DecodeTimestamp)
                    })?;
                    // EOS 可能与最后一个样本一起返回，原样本必须先交付。
                    return Ok(Some(VideoSample {
                        interleave_key,
                        sample,
                    }));
                }
            }
            if self.ended {
                return Ok(None);
            }
        }
        Err(tr!(
            "视频连续返回空的压缩样本",
            "The video source returned too many empty compressed samples"
        )
        .into())
    }
}

fn video_interleave_key(
    read_timestamp: i64,
    decode_timestamp: windows::core::Result<u64>,
) -> Result<i128> {
    match decode_timestamp {
        // DTS 属性的原生类型为 UINT64；宽整数比较避免将高位误解释成负时间。
        Ok(timestamp) => Ok(i128::from(timestamp)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(i128::from(read_timestamp)),
        Err(error) => Err(trf!(
            "无法读取视频解码时间戳：{error}",
            "Cannot read the video decode timestamp: {error}"
        )
        .into()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PcmSpan {
    timestamp: i64,
    time: Duration,
    duration: Duration,
    end_frames: u64,
}

impl PcmSpan {
    fn new(written_frames: u64, samples: usize) -> Result<Self> {
        if samples == 0 || samples > MAX_PCM_SAMPLES || samples % AUDIO_CHANNELS as usize != 0 {
            return Err(tr!(
                "混音块必须包含 1–24000 个完整立体声帧",
                "An audio block must contain 1–24000 complete stereo frames"
            )
            .into());
        }
        let frames = u64::try_from(samples / AUDIO_CHANNELS as usize)?;
        let end_frames = written_frames
            .checked_add(frames)
            .ok_or(tr!("混音帧数溢出", "Audio remix frame count overflow"))?;
        let timestamp = frame_ticks(written_frames)?;
        let end = frame_ticks(end_frames)?;
        let duration = end
            .checked_sub(timestamp)
            .ok_or(tr!("混音时间范围无效", "Invalid audio remix time range"))?;
        Ok(Self {
            timestamp,
            time: tick_duration(timestamp as u64),
            // 相邻整数端点相减，分块方式不会累计每块的取整误差。
            duration: tick_duration(duration as u64),
            end_frames,
        })
    }
}

struct AudioBlock {
    pcm: Vec<i16>,
    span: PcmSpan,
}

fn frame_ticks(frames: u64) -> Result<i64> {
    let scaled = u128::from(frames)
        .checked_mul(u128::from(TICKS_PER_SECOND))
        .ok_or(tr!("混音时间溢出", "Audio remix time overflow"))?;
    let ticks = scaled / u128::from(AUDIO_RATE);
    i64::try_from(ticks).map_err(|_| tr!("混音时间溢出", "Audio remix time overflow").into())
}

fn tick_duration(ticks: u64) -> Duration {
    Duration::new(
        ticks / TICKS_PER_SECOND,
        (ticks % TICKS_PER_SECOND) as u32 * 100,
    )
}

fn check_running(keep_going: &mut impl FnMut() -> bool) -> Result<()> {
    if keep_going() {
        Ok(())
    } else {
        Err(tr!("混音已取消", "Audio remix cancelled").into())
    }
}

/// 调用线程须已初始化 COM/MF。target 是调用方拥有的暂存，失败或取消时不尝试 Finalize。
/// next_pcm 仅返回实际音频，Some 必须为非空且不超过半秒的完整立体声块，None 表示结束。
/// 本层只保留一个视频样本和一个 PCM 块，不据此承诺系统 Sink 的内部缓存固定。
pub(crate) fn remux_audio_streaming(
    source: &Path,
    target: &Path,
    mut next_pcm: impl FnMut() -> Result<Option<Vec<i16>>>,
    mut keep_going: impl FnMut() -> bool,
) -> Result<()> {
    check_running(&mut keep_going)?;
    unsafe {
        let reader = open_file_source_reader(source, None).map_err(|error| {
            trf!(
                "无法打开录屏文件：{error}",
                "Cannot open recording: {error}"
            )
        })?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
        // 未请求解码类型，Source Reader 返回原生压缩样本。
        let native = reader.GetNativeMediaType(stream, 0)?;
        if native.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_H264 {
            return Err(tr!(
                "音频重混只支持原生 H.264 视频",
                "Audio remix requires native H.264 video"
            )
            .into());
        }
        if reader
            .GetCurrentMediaType(stream)?
            .GetGUID(&MF_MT_SUBTYPE)?
            != MFVideoFormat_H264
        {
            return Err(tr!(
                "视频读取器没有返回原生 H.264 压缩格式",
                "The video reader did not select native compressed H.264"
            )
            .into());
        }
        check_running(&mut keep_going)?;
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes =
            attributes.ok_or(tr!("无法创建编码属性", "Cannot create encoding attributes"))?;
        attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        let out = HSTRING::from(target.as_os_str());
        let writer = MFCreateSinkWriterFromURL(PCWSTR(out.as_ptr()), None, &attributes).map_err(
            |error| {
                trf!(
                    "无法创建混音文件：{error}",
                    "Cannot create remixed file: {error}"
                )
            },
        )?;
        // 压缩输入和输出使用同一原生类型，不重新编码视频，也不重建样本属性。
        let video = writer.AddStream(&native)?;
        writer.SetInputMediaType(video, &native, None)?;
        let output = audio_type(&MFAudioFormat_AAC, 24_000)?;
        let audio = writer.AddStream(&output)?;
        let input = audio_type(&MFAudioFormat_PCM, AUDIO_RATE * AUDIO_CHANNELS * 2)?;
        input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, AUDIO_CHANNELS * 2)?;
        writer.SetInputMediaType(audio, &input, None)?;
        check_running(&mut keep_going)?;
        writer.BeginWriting()?;

        let mut video_input = VideoInput {
            reader,
            stream,
            ended: false,
        };
        let mut pending_video: Option<VideoSample> = None;
        let mut pending_audio: Option<AudioBlock> = None;
        let mut audio_ended = false;
        let mut written_audio_frames = 0;
        loop {
            if pending_video.is_none() && !video_input.ended {
                pending_video = video_input.next(&mut keep_going)?;
            }
            if pending_audio.is_none() && !audio_ended {
                check_running(&mut keep_going)?;
                match next_pcm()? {
                    Some(pcm) => {
                        let span = PcmSpan::new(written_audio_frames, pcm.len())?;
                        pending_audio = Some(AudioBlock { pcm, span });
                    }
                    None => audio_ended = true,
                }
            }
            let write_video = match (&pending_video, &pending_audio) {
                (Some(video), Some(audio)) => {
                    video.interleave_key <= i128::from(audio.span.timestamp)
                }
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            check_running(&mut keep_going)?;
            if write_video {
                let video_sample = pending_video
                    .take()
                    .ok_or(tr!("缺少待写入的视频样本", "Missing pending video sample"))?;
                writer.WriteSample(video, &video_sample.sample)?;
            } else {
                let block = pending_audio
                    .take()
                    .ok_or(tr!("缺少待写入的混音块", "Missing pending audio block"))?;
                let length = block
                    .pcm
                    .len()
                    .checked_mul(size_of::<i16>())
                    .ok_or(tr!("混音块长度溢出", "Audio remix block length overflow"))?;
                let bytes = std::slice::from_raw_parts(block.pcm.as_ptr().cast::<u8>(), length);
                let sample = sample(bytes, block.span.time, block.span.duration)?;
                check_running(&mut keep_going)?;
                writer.WriteSample(audio, &sample)?;
                written_audio_frames = block.span.end_frames;
            }
        }
        check_running(&mut keep_going)?;
        writer.Finalize().map_err(|error| {
            trf!(
                "无法完成混音文件：{error}",
                "Cannot finalize remixed file: {error}"
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_blocks_require_bounded_complete_stereo_frames() {
        for samples in [0, 1, 3, MAX_PCM_SAMPLES + 1, MAX_PCM_SAMPLES + 2] {
            assert!(PcmSpan::new(0, samples).is_err());
        }
        let short = PcmSpan::new(0, 2).unwrap();
        assert_eq!(short.end_frames, 1);
        assert_eq!(short.duration, Duration::from_nanos(20_800));
        let half_second = PcmSpan::new(0, MAX_PCM_SAMPLES).unwrap();
        assert_eq!(half_second.end_frames, 24_000);
        assert_eq!(half_second.duration, Duration::from_millis(500));
    }

    #[test]
    fn audio_endpoints_do_not_accumulate_chunk_rounding() {
        let mut frames = 0;
        let mut duration = Duration::ZERO;
        for _ in 0..48_000 {
            let span = PcmSpan::new(frames, 2).unwrap();
            assert_eq!(span.time, duration);
            frames = span.end_frames;
            duration += span.duration;
        }
        assert_eq!(frames, 48_000);
        assert_eq!(duration, Duration::from_secs(1));
        let long = PcmSpan::new(48_000 * 60 * 60 * 24 * 365, 2).unwrap();
        assert_eq!(long.timestamp, 31_536_000 * TICKS_PER_SECOND as i64);
    }

    #[test]
    fn count_and_native_timestamp_overflow_are_rejected() {
        assert!(PcmSpan::new(u64::MAX, 2).is_err());
        assert!(frame_ticks(u64::MAX).is_err());
        assert_eq!(frame_ticks(48_000).unwrap(), 10_000_000);
        assert_eq!(tick_duration(i64::MAX as u64).as_secs(), 922_337_203_685);
    }

    #[test]
    fn decode_time_orders_b_frames_and_only_missing_attributes_fall_back() {
        let read_timestamp = 1_200_000;
        let audio_timestamp = 800_000_i128;
        let key = video_interleave_key(read_timestamp, Ok(400_000)).unwrap();
        assert!(key <= audio_timestamp && i128::from(read_timestamp) > audio_timestamp);
        assert_eq!(
            video_interleave_key(
                -1,
                Err(windows::core::Error::from_hresult(MF_E_ATTRIBUTENOTFOUND))
            )
            .unwrap(),
            -1
        );
        assert_eq!(
            video_interleave_key(0, Ok(u64::MAX)).unwrap(),
            i128::from(u64::MAX)
        );
        assert!(
            video_interleave_key(0, Err(windows::core::Error::from_hresult(MF_E_INVALIDTYPE)))
                .is_err()
        );
    }
}
