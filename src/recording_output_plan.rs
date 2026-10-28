//! 录屏输出请求的纯预检；不访问文件、枚举编码器或创建媒体。
//! codec_order 始终描述 MP4 编码阶段，GIF 的最终格式与中间 MP4 分别记录。

use crate::{recording_encoder_caps::VideoCodec, recording_options::Preset, Result};
use serde::Serialize;
use std::path::Path;

const SCOPE: &str = "output request contract only; codec_order describes the MP4 encoding stage, including the H.264 intermediate for GIF; planned embedded audio is a request, not evidence that capture started or an audio track exists; HEVC requests are experimental; no local encoder availability, negotiation, completed media, container probing, playback or remix support is proved; no filesystem or native calls are made";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinalFormat {
    Mp4,
    Gif,
}

impl FinalFormat {
    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Gif => "gif",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlannedEmbeddedAudio {
    None,
    Aac,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OutputPlan {
    pub final_format: FinalFormat,
    /// MP4 成片或 GIF 中间 MP4 的请求顺序；不表示任何候选实际可用。
    pub codec_order: Vec<VideoCodec>,
    /// 编码 MP4 阶段及最终成片的音轨请求；GIF 两者均为 None。
    pub planned_embedded_audio: PlannedEmbeddedAudio,
    /// 保留调用方原始请求；实际采集失败不能回写为未请求。
    pub capture_audio_requested: bool,
    /// 仅当 GIF 抑制了原本请求的音频时为 true。
    pub gif_audio_suppressed: bool,
    /// GIF 使用 MP4 中间文件；普通 MP4 没有另一个中间格式。
    pub intermediate_format: Option<FinalFormat>,
    pub experimental: bool,
    pub scope: &'static str,
}

impl OutputPlan {
    pub(crate) fn new(
        preset: Preset,
        codec_order: &[VideoCodec],
        capture_audio_requested: bool,
        destination: &Path,
    ) -> Result<Self> {
        let final_format = match preset {
            Preset::HighQualityMp4 | Preset::CompactMp4 => FinalFormat::Mp4,
            Preset::Gif => FinalFormat::Gif,
        };
        // 只解码扩展名；Windows 文件名可以包含无法表示为 Unicode 的其它部分。
        if !destination
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case(final_format.extension()))
        {
            return Err(tr!(
                "录屏目标扩展名必须与请求的 MP4 或 GIF 格式一致",
                "The recording destination extension must match the requested MP4 or GIF format"
            )
            .into());
        }
        if !matches!(
            codec_order,
            [VideoCodec::H264] | [VideoCodec::Hevc] | [VideoCodec::Hevc, VideoCodec::H264]
        ) {
            return Err(tr!(
                "录屏视频编码顺序只能为 H264、HEVC 或 HEVC 后退至 H264",
                "The recording codec order must be H264, HEVC, or HEVC followed by H264"
            )
            .into());
        }
        let gif = final_format == FinalFormat::Gif;
        if gif && codec_order != [VideoCodec::H264] {
            return Err(tr!(
                "GIF 仅允许无音轨的 H264 中间 MP4",
                "GIF requires a silent H264 intermediate MP4"
            )
            .into());
        }
        Ok(Self {
            final_format,
            codec_order: codec_order.to_vec(),
            planned_embedded_audio: if capture_audio_requested && !gif {
                PlannedEmbeddedAudio::Aac
            } else {
                PlannedEmbeddedAudio::None
            },
            capture_audio_requested,
            gif_audio_suppressed: gif && capture_audio_requested,
            intermediate_format: gif.then_some(FinalFormat::Mp4),
            experimental: codec_order.contains(&VideoCodec::Hevc),
            scope: SCOPE,
        })
    }
}

/// 仅验证现有重混路由的请求门；observed_video_codec 必须由调用方提供实际观测。
/// 不把请求的首选编码、扩展名或已有历史报告当成本次源文件的观测或读取资格。
pub(crate) fn validate_remix(
    final_format: FinalFormat,
    observed_video_codec: Option<VideoCodec>,
) -> Result<()> {
    if final_format != FinalFormat::Mp4 {
        return Err(tr!(
            "音频重混只支持 MP4 成片，GIF 不支持音轨",
            "Audio remix requires an MP4 recording; GIF has no audio track"
        )
        .into());
    }
    match observed_video_codec {
        Some(VideoCodec::H264) => Ok(()),
        Some(VideoCodec::Hevc) => Err(tr!(
            "当前音频重混尚不支持 HEVC 视频",
            "Audio remix does not yet support HEVC video"
        )
        .into()),
        Some(VideoCodec::Av1) => Err(tr!(
            "当前音频重混尚不支持 AV1 视频",
            "Audio remix does not yet support AV1 video"
        )
        .into()),
        None => Err(tr!(
            "尚未确认源视频编码，不能执行音频重混",
            "Audio remix requires an observed source video codec"
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_output_plan_preserves_mp4_codec_order_and_audio_request() {
        for preset in [Preset::HighQualityMp4, Preset::CompactMp4] {
            for order in [
                &[VideoCodec::H264][..],
                &[VideoCodec::Hevc][..],
                &[VideoCodec::Hevc, VideoCodec::H264][..],
            ] {
                for audio_requested in [false, true] {
                    let plan =
                        OutputPlan::new(preset, order, audio_requested, Path::new("unopened.mp4"))
                            .unwrap();
                    assert_eq!(plan.final_format, FinalFormat::Mp4);
                    assert_eq!(plan.codec_order, order);
                    assert_eq!(plan.capture_audio_requested, audio_requested);
                    assert_eq!(
                        plan.planned_embedded_audio,
                        if audio_requested {
                            PlannedEmbeddedAudio::Aac
                        } else {
                            PlannedEmbeddedAudio::None
                        }
                    );
                    assert_eq!(plan.experimental, order.contains(&VideoCodec::Hevc));
                    assert!(!plan.gif_audio_suppressed);
                    assert_eq!(plan.intermediate_format, None);
                    let report = serde_json::to_value(&plan).unwrap();
                    assert_eq!(report["capture_audio_requested"], audio_requested);
                    assert_eq!(report["final_format"], "mp4");
                    assert!(report.get("actual_audio").is_none());
                    assert!(report.get("supported").is_none());
                }
            }
        }
    }

    #[test]
    fn recording_output_plan_rejects_noncanonical_codec_sequences_without_mutation() {
        for order in [
            &[][..],
            &[VideoCodec::H264, VideoCodec::Hevc][..],
            &[VideoCodec::H264, VideoCodec::H264][..],
            &[VideoCodec::Hevc, VideoCodec::Hevc][..],
            &[VideoCodec::Hevc, VideoCodec::H264, VideoCodec::H264][..],
            &[VideoCodec::Av1][..],
            &[VideoCodec::Av1, VideoCodec::H264][..],
        ] {
            let original = order.to_vec();
            assert!(
                OutputPlan::new(Preset::CompactMp4, order, false, Path::new("clip.mp4")).is_err()
            );
            assert_eq!(order, original);
        }
    }

    #[test]
    fn recording_output_plan_requires_exact_extension_but_not_unicode_basename() {
        for name in ["clip.mp4", "clip.MP4", "clip.mP4", "文件.mp4"] {
            assert!(OutputPlan::new(
                Preset::CompactMp4,
                &[VideoCodec::H264],
                false,
                Path::new(name)
            )
            .is_ok());
        }
        for name in ["clip.gif", "clip", ".mp4", "clip.mp4.tmp", "clip.mp4."] {
            assert!(OutputPlan::new(
                Preset::CompactMp4,
                &[VideoCodec::H264],
                false,
                Path::new(name)
            )
            .is_err());
        }
        assert!(OutputPlan::new(
            Preset::Gif,
            &[VideoCodec::H264],
            false,
            Path::new("clip.GIF")
        )
        .is_ok());
        assert!(OutputPlan::new(
            Preset::Gif,
            &[VideoCodec::H264],
            false,
            Path::new("clip.mp4")
        )
        .is_err());
        #[cfg(windows)]
        {
            use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};
            let destination = PathBuf::from(OsString::from_wide(&[
                0xd800,
                b'.' as u16,
                b'm' as u16,
                b'p' as u16,
                b'4' as u16,
            ]));
            assert!(destination.to_str().is_none());
            assert!(
                OutputPlan::new(Preset::CompactMp4, &[VideoCodec::H264], false, &destination)
                    .is_ok()
            );
        }
    }

    #[test]
    fn recording_output_plan_gif_retains_requested_audio_but_plans_silent_h264() {
        for audio_requested in [false, true] {
            let plan = OutputPlan::new(
                Preset::Gif,
                &[VideoCodec::H264],
                audio_requested,
                Path::new("clip.gif"),
            )
            .unwrap();
            assert_eq!(plan.final_format, FinalFormat::Gif);
            assert_eq!(plan.codec_order, [VideoCodec::H264]);
            assert_eq!(plan.planned_embedded_audio, PlannedEmbeddedAudio::None);
            assert_eq!(plan.capture_audio_requested, audio_requested);
            assert_eq!(plan.gif_audio_suppressed, audio_requested);
            assert_eq!(plan.intermediate_format, Some(FinalFormat::Mp4));
            assert!(!plan.experimental);
        }
        for order in [
            &[VideoCodec::Hevc][..],
            &[VideoCodec::Hevc, VideoCodec::H264][..],
        ] {
            assert!(OutputPlan::new(Preset::Gif, order, true, Path::new("clip.gif")).is_err());
        }
    }

    #[test]
    fn recording_output_plan_remix_requires_observed_h264_mp4() {
        assert!(validate_remix(FinalFormat::Mp4, Some(VideoCodec::H264)).is_ok());
        assert!(validate_remix(FinalFormat::Mp4, Some(VideoCodec::Hevc)).is_err());
        assert!(validate_remix(FinalFormat::Mp4, Some(VideoCodec::Av1)).is_err());
        assert!(validate_remix(FinalFormat::Mp4, None).is_err());
        for observed in [
            None,
            Some(VideoCodec::H264),
            Some(VideoCodec::Hevc),
            Some(VideoCodec::Av1),
        ] {
            assert!(validate_remix(FinalFormat::Gif, observed).is_err());
        }
    }
}
