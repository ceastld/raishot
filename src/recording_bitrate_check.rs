//! 只读扫描短 H.264/HEVC/AV1 成片的压缩样本，区分声明码率与实际视频负载码率。
//! 不解码、不收集样本负载；初始化参数集单独有界观测。
//! 时间不完整、未读到 EOF 或类型变化时不推算码率。
//! clean-point 只记录 Reader 的样本 metadata，不解析 IDR 或认定精确 GOP 周期。
#[path = "recording_h264_header.rs"]
mod h264_header;
#[path = "recording_h264_header_observation.rs"]
mod h264_header_observation;

use crate::{
    media_foundation::{open_file_source_reader, MfRuntime},
    recording_encoder_caps::VideoCodec,
    Result,
};
use serde_json::{json, Value};
use std::path::Path;
use windows::{core::GUID, Win32::Media::MediaFoundation::*};

const TICKS_PER_SECOND: u64 = 10_000_000;
const MAX_READ_CALLS: u64 = 100_000;
const MAX_SAMPLES: u64 = 60_000;
const MAX_SPAN_TICKS: u64 = 600 * TICKS_PER_SECOND;
const MAX_SAMPLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_CLEAN_POINT_RECORDS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CleanPointStats {
    marked_samples: u64,
    unmarked_samples: u64,
    unknown_samples: u64,
    records: Vec<(u64, Option<i64>)>,
    omitted_records: u64,
}
impl CleanPointStats {
    /// 仅对已经通过负载统计校验的样本调用；索引以 Reader 返回样本的零基顺序计。
    fn observe(&mut self, sample_index: u64, pts: Option<i64>, marked: Option<bool>) {
        match marked {
            Some(true) => {
                self.marked_samples = self.marked_samples.saturating_add(1);
                if self.records.len() < MAX_CLEAN_POINT_RECORDS {
                    self.records.push((sample_index, pts));
                } else {
                    self.omitted_records = self.omitted_records.saturating_add(1);
                }
            }
            Some(false) => self.unmarked_samples = self.unmarked_samples.saturating_add(1),
            None => self.unknown_samples = self.unknown_samples.saturating_add(1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatsError {
    Overflow,
    SampleLimit,
    SampleSizeLimit,
    PayloadLimit,
    SpanLimit,
}
impl StatsError {
    fn message(self) -> &'static str {
        match self {
            Self::Overflow => tr!(
                "视频负载统计数值溢出",
                "Video payload statistics overflowed"
            ),
            Self::SampleLimit => tr!(
                "视频负载统计达到样本上限",
                "Video payload sample limit reached"
            ),
            Self::SampleSizeLimit => tr!(
                "单个视频压缩样本过大",
                "A compressed video sample is too large"
            ),
            Self::PayloadLimit => tr!(
                "视频负载统计达到字节上限",
                "Video payload byte limit reached"
            ),
            Self::SpanLimit => tr!(
                "视频负载统计超过短片时间上限",
                "Video payload scan exceeded the short-clip time limit"
            ),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PayloadStats {
    samples: u64,
    payload_bytes: u64,
    minimum_pts: Option<i64>,
    maximum_pts: Option<i64>,
    maximum_end: Option<i64>,
    missing_timestamps: u64,
    unknown_durations: u64,
    invalid_durations: u64,
}
impl PayloadStats {
    /// 先验证全部算术与上限，再接纳一个样本；乱序 PTS 不按解码顺序累加时长。
    fn add(
        &mut self,
        bytes: u64,
        pts: Option<i64>,
        duration: Option<i64>,
    ) -> std::result::Result<(), StatsError> {
        let samples = self.samples.checked_add(1).ok_or(StatsError::Overflow)?;
        let payload_bytes = self
            .payload_bytes
            .checked_add(bytes)
            .ok_or(StatsError::Overflow)?;
        if samples > MAX_SAMPLES {
            return Err(StatsError::SampleLimit);
        }
        if bytes > MAX_SAMPLE_BYTES {
            return Err(StatsError::SampleSizeLimit);
        }
        if payload_bytes > MAX_PAYLOAD_BYTES {
            return Err(StatsError::PayloadLimit);
        }
        let minimum_pts = match (self.minimum_pts, pts) {
            (Some(old), Some(next)) => Some(old.min(next)),
            (old, next) => old.or(next),
        };
        let maximum_pts = match (self.maximum_pts, pts) {
            (Some(old), Some(next)) => Some(old.max(next)),
            (old, next) => old.or(next),
        };
        let end = match (pts, duration) {
            (Some(pts), Some(duration)) if duration > 0 => {
                if duration as u64 > MAX_SPAN_TICKS {
                    return Err(StatsError::SpanLimit);
                }
                Some(pts.checked_add(duration).ok_or(StatsError::Overflow)?)
            }
            _ => None,
        };
        let maximum_end = match (self.maximum_end, end) {
            (Some(old), Some(next)) => Some(old.max(next)),
            (old, next) => old.or(next),
        };
        let latest_observed = match (maximum_pts, maximum_end) {
            (Some(pts), Some(end)) => Some(pts.max(end)),
            (pts, end) => pts.or(end),
        };
        if let (Some(first), Some(last)) = (minimum_pts, latest_observed) {
            if i128::from(last) - i128::from(first) > i128::from(MAX_SPAN_TICKS) {
                return Err(StatsError::SpanLimit);
            }
        }
        self.samples = samples;
        self.payload_bytes = payload_bytes;
        self.minimum_pts = minimum_pts;
        self.maximum_pts = maximum_pts;
        self.maximum_end = maximum_end;
        self.missing_timestamps += u64::from(pts.is_none());
        self.unknown_durations += u64::from(duration.is_none_or(|duration| duration == 0));
        self.invalid_durations += u64::from(duration.is_some_and(|duration| duration < 0));
        Ok(())
    }

    fn span_ticks(&self) -> Option<u64> {
        let span = i128::from(self.maximum_end?) - i128::from(self.minimum_pts?);
        (span > 0).then(|| u64::try_from(span).ok()).flatten()
    }

    fn bitrate_bps(&self, complete: bool) -> Option<f64> {
        if !complete
            || self.samples == 0
            || self.missing_timestamps != 0
            || self.unknown_durations != 0
            || self.invalid_durations != 0
        {
            return None;
        }
        Some(self.payload_bytes as f64 * 8. * TICKS_PER_SECOND as f64 / self.span_ticks()? as f64)
    }
}

/// 只用于内部短片验证。初始化/格式失败返回 Err，扫描中止则返回含部分统计的报告。
pub(crate) fn inspect(path: &Path) -> Result<Value> {
    inspect_cancellable(path, &|| false)
}

/// 在压缩样本读取之间响应取消或预算；不能中断正在执行的原生调用。
pub(crate) fn inspect_cancellable(path: &Path, cancelled: &dyn Fn() -> bool) -> Result<Value> {
    inspect_for_codec_cancellable(path, VideoCodec::H264, cancelled)
}

fn matches_video_codec(major_type: GUID, subtype: GUID, codec: VideoCodec) -> bool {
    major_type == MFMediaType_Video && subtype == codec.subtype()
}

/// 只接受请求编码的精确原生压缩类型；不接收可跨帧或含半帧的 HEVC_ES。
/// samples 仍是压缩样本数，不能代替独立解码的帧数证据。
pub(crate) fn inspect_for_codec_cancellable(
    path: &Path,
    codec: VideoCodec,
    cancelled: &dyn Fn() -> bool,
) -> Result<Value> {
    if cancelled() {
        return Err(tr!("视频负载统计已取消", "Video payload scanning canceled").into());
    }
    if !std::fs::metadata(path)?.is_file() {
        return Err(tr!(
            "视频负载统计只接受本地文件",
            "Video payload scanning requires a local file"
        )
        .into());
    }
    let _runtime = MfRuntime::new()?;
    let codec_name = match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::Hevc => "HEVC",
        VideoCodec::Av1 => "AV1",
    };
    // 精确原生压缩类型直接取源数据；没有请求 RGB/NV12 类型，也没有插入解码器。
    let reader = open_file_source_reader(path, None)?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
    }
    let native = unsafe { reader.GetNativeMediaType(stream, 0) }?;
    let native_subtype = unsafe { native.GetGUID(&MF_MT_SUBTYPE) }?;
    if !matches_video_codec(
        unsafe { native.GetGUID(&MF_MT_MAJOR_TYPE) }?,
        native_subtype,
        codec,
    ) {
        return Err(trf!(
            "视频负载统计需要原生 {codec_name} 视频流",
            "Video payload scanning requires a native {codec_name} video stream"
        )
        .into());
    }
    let declared_bitrate = unsafe { native.GetUINT32(&MF_MT_AVG_BITRATE) }.ok();
    unsafe { reader.SetCurrentMediaType(stream, None, &native) }?;
    let current = unsafe { reader.GetCurrentMediaType(stream) }?;
    let current_subtype = unsafe { current.GetGUID(&MF_MT_SUBTYPE) }?;
    if !matches_video_codec(
        unsafe { current.GetGUID(&MF_MT_MAJOR_TYPE) }?,
        current_subtype,
        codec,
    ) {
        return Err(trf!(
            "视频负载读取未保持 {codec_name} 压缩类型",
            "Video payload reading did not retain the {codec_name} compressed type"
        )
        .into());
    }

    let h264_sequence_header = h264_header_observation::observe(&native, codec);
    let mut stats = PayloadStats::default();
    let mut clean_points = CleanPointStats::default();
    let mut complete = false;
    let mut error = None;
    let mut sample_attribute_error = None;
    let mut clean_point_attribute_error = None;
    let mut read_calls = 0_u64;
    let mut actual_stream_index = None;
    let mut native_type_changes = 0_u64;
    let mut current_type_changes = 0_u64;
    while read_calls < MAX_READ_CALLS {
        if cancelled() {
            error = Some(tr!("视频负载统计已取消", "Video payload scanning canceled").to_owned());
            break;
        }
        read_calls += 1;
        let mut flags = 0_u32;
        let mut actual = u32::MAX;
        let mut sample = None;
        if let Err(failure) = unsafe {
            reader.ReadSample(
                stream,
                0,
                Some(&mut actual),
                Some(&mut flags),
                None,
                Some(&mut sample),
            )
        } {
            error = Some(trf!(
                "读取视频压缩样本失败：{failure}",
                "Cannot read compressed video samples: {failure}"
            ));
            break;
        }
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            error = Some(
                tr!(
                    "视频负载读取报告错误",
                    "Video payload reading reported an error"
                )
                .to_owned(),
            );
            break;
        }
        native_type_changes +=
            u64::from(flags & MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0 as u32 != 0);
        current_type_changes +=
            u64::from(flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0);
        if native_type_changes != 0 || current_type_changes != 0 {
            error = Some(
                tr!(
                    "视频负载读取期间媒体类型变化",
                    "The media type changed during video payload reading"
                )
                .to_owned(),
            );
            break;
        }
        if actual != u32::MAX {
            if actual_stream_index.is_some_and(|previous| previous != actual) {
                error = Some(
                    tr!(
                        "视频负载读取期间流索引变化",
                        "The stream index changed during video payload reading"
                    )
                    .to_owned(),
                );
                break;
            }
            actual_stream_index = Some(actual);
        }
        if let Some(sample) = sample {
            let bytes = match unsafe { sample.GetTotalLength() } {
                Ok(bytes) => bytes,
                Err(failure) => {
                    error = Some(trf!(
                        "读取视频负载长度失败：{failure}",
                        "Cannot read video payload length: {failure}"
                    ));
                    break;
                }
            };
            let pts = unsafe { sample.GetSampleTime() }
                .map_err(|failure| {
                    sample_attribute_error.get_or_insert_with(|| failure.to_string());
                })
                .ok();
            let duration = unsafe { sample.GetSampleDuration() }
                .map_err(|failure| {
                    sample_attribute_error.get_or_insert_with(|| failure.to_string());
                })
                .ok();
            if let Err(failure) = stats.add(u64::from(bytes), pts, duration) {
                error = Some(failure.message().to_owned());
                break;
            }
            let marked = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
                .map(|value| value != 0)
                .map_err(|failure| {
                    clean_point_attribute_error.get_or_insert_with(|| failure.to_string());
                })
                .ok();
            clean_points.observe(stats.samples - 1, pts, marked);
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            complete = true;
            break;
        }
    }
    if !complete && error.is_none() {
        error = Some(
            tr!(
                "视频负载统计达到读取次数上限",
                "Video payload read-call limit reached"
            )
            .to_owned(),
        );
    }
    Ok(json!({
        "codec": codec_name,
        "native_subtype": format!("{native_subtype:?}"),
        "current_subtype": format!("{current_subtype:?}"),
        "declared_mean_bitrate_bps": declared_bitrate,
        "h264_sequence_header": h264_sequence_header,
        "selected_stream_index": actual_stream_index,
        "complete": complete,
        "error": error,
        "read_calls": read_calls,
        "samples": stats.samples,
        "payload_bytes": stats.payload_bytes,
        "minimum_pts_100ns": stats.minimum_pts,
        "maximum_pts_100ns": stats.maximum_pts,
        "maximum_end_100ns": stats.maximum_end,
        "observed_span_100ns": stats.span_ticks(),
        "missing_timestamps": stats.missing_timestamps,
        "unknown_durations": stats.unknown_durations,
        "invalid_durations": stats.invalid_durations,
        "first_sample_attribute_error": sample_attribute_error,
        "native_type_changes": native_type_changes,
        "current_type_changes": current_type_changes,
        "actual_payload_bitrate_bps": stats.bitrate_bps(complete),
        "clean_points": {
            "marked_samples": clean_points.marked_samples,
            "unmarked_samples": clean_points.unmarked_samples,
            "unknown_samples": clean_points.unknown_samples,
            "records": clean_points.records.iter().map(|&(sample_index, pts)| json!({
                "sample_index": sample_index, "pts_100ns": pts,
            })).collect::<Vec<_>>(),
            "omitted_records": clean_points.omitted_records,
            "first_attribute_error": clean_point_attribute_error,
            "record_limit": MAX_CLEAN_POINT_RECORDS,
            "sample_index_base": 0,
            "scope": "Source Reader MFSampleExtension_CleanPoint metadata only; missing or unreadable attributes remain unknown; not parsed IDR evidence or an exact GOP interval; compressed sample indexes do not prove display frame indexes, including AV1",
        },
        "byte_scope": "compressed samples returned by Source Reader; not raw container sample bytes",
        "sample_count_proves_display_frame_count": false,
        "payload_scope": format!("valid bytes in native {codec_name} samples; excludes container and other streams"),
        "limits": {
            "read_calls": MAX_READ_CALLS,
            "samples": MAX_SAMPLES,
            "span_100ns": MAX_SPAN_TICKS,
            "sample_bytes": MAX_SAMPLE_BYTES,
            "payload_bytes": MAX_PAYLOAD_BYTES,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_bitrate_clean_point_metadata_is_bounded_and_preserves_unknown() {
        let mut stats = CleanPointStats::default();
        stats.observe(0, None, None);
        stats.observe(1, Some(10), Some(false));
        for index in 2..72 {
            stats.observe(index, (index != 2).then_some(index as i64 * 10), Some(true));
        }
        assert_eq!(stats.unknown_samples, 1);
        assert_eq!(stats.unmarked_samples, 1);
        assert_eq!(stats.marked_samples, 70);
        assert_eq!(stats.records.len(), MAX_CLEAN_POINT_RECORDS);
        assert_eq!(stats.omitted_records, 6);
        assert_eq!(stats.records.first(), Some(&(2, None)));
        assert_eq!(stats.records.last(), Some(&(65, Some(650))));
    }

    #[test]
    fn recording_bitrate_requires_exact_video_codec_without_elementary_stream_chunks() {
        for (codec, subtype) in [
            (VideoCodec::H264, MFVideoFormat_H264),
            (VideoCodec::Hevc, MFVideoFormat_HEVC),
            (VideoCodec::Av1, MFVideoFormat_AV1),
        ] {
            assert!(matches_video_codec(MFMediaType_Video, subtype, codec));
            assert!(!matches_video_codec(MFMediaType_Audio, subtype, codec));
            assert!(!matches_video_codec(
                MFMediaType_Video,
                MFVideoFormat_HEVC_ES,
                codec
            ));
        }
        assert!(!matches_video_codec(
            MFMediaType_Video,
            MFVideoFormat_H264,
            VideoCodec::Hevc
        ));
        assert!(!matches_video_codec(
            MFMediaType_Video,
            MFVideoFormat_HEVC,
            VideoCodec::H264
        ));
        for codec in [VideoCodec::H264, VideoCodec::Hevc] {
            assert!(!matches_video_codec(
                MFMediaType_Video,
                MFVideoFormat_AV1,
                codec
            ));
            assert!(!matches_video_codec(
                MFMediaType_Video,
                codec.subtype(),
                VideoCodec::Av1
            ));
        }
    }

    #[test]
    fn recording_bitrate_uses_presentation_range_for_reordered_samples() {
        let mut stats = PayloadStats::default();
        stats.add(100, Some(0), Some(333_333)).unwrap();
        stats.add(100, Some(666_666), Some(333_334)).unwrap();
        stats.add(100, Some(333_333), Some(333_333)).unwrap();
        assert_eq!(stats.span_ticks(), Some(1_000_000));
        assert_eq!(stats.bitrate_bps(true), Some(24_000.));
        assert_eq!(stats.bitrate_bps(false), None);
    }

    #[test]
    fn recording_bitrate_does_not_invent_missing_or_zero_sample_duration() {
        for (pts, duration) in [
            (Some(0), None),
            (Some(0), Some(0)),
            (None, Some(100)),
            (Some(0), Some(-1)),
        ] {
            let mut stats = PayloadStats::default();
            stats.add(100, Some(100), Some(100)).unwrap();
            stats.add(100, pts, duration).unwrap();
            assert_eq!(stats.payload_bytes, 200);
            assert_eq!(stats.bitrate_bps(true), None);
        }
    }

    #[test]
    fn recording_bitrate_rejects_overflow_and_limits_before_accepting_a_sample() {
        let mut stats = PayloadStats::default();
        let before = stats.clone();
        assert_eq!(
            stats.add(1, Some(i64::MAX), Some(1)),
            Err(StatsError::Overflow)
        );
        assert_eq!(stats, before);
        assert_eq!(
            stats.add(1, Some(0), Some(MAX_SPAN_TICKS as i64 + 1)),
            Err(StatsError::SpanLimit)
        );
        assert_eq!(stats, before);
        stats.add(1, Some(0), None).unwrap();
        let before = stats.clone();
        assert_eq!(
            stats.add(1, Some(MAX_SPAN_TICKS as i64 + 1), None),
            Err(StatsError::SpanLimit)
        );
        assert_eq!(stats, before);
        stats.payload_bytes = u64::MAX;
        let before = stats.clone();
        assert_eq!(stats.add(1, Some(0), Some(1)), Err(StatsError::Overflow));
        assert_eq!(stats, before);
    }
}
