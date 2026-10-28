//! 视频属性只读展示；仅使用播放器已经取得的元数据，不在render里访问媒体文件。
use crate::{media_foundation::MediaStreamInfo, recording_preview::Player};
use gpui::{div, prelude::*, px, rgb, App};
use std::path::Path;
use windows::Win32::Media::MediaFoundation::{
    MFAudioFormat_AAC, MFAudioFormat_Float, MFAudioFormat_MP3, MFAudioFormat_PCM,
    MFMediaType_Audio, MFMediaType_Subtitle, MFMediaType_Video, MFVideoFormat_AV1,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_MPEG2, MFVideoFormat_VP80,
    MFVideoFormat_VP90,
};

const MAX_STREAMS_SHOWN: usize = 128;

/// 仅描述来源报告的原生流，不推断解码能力或是否可以切换。
pub(super) fn stream_description(stream: &MediaStreamInfo) -> String {
    let kind = if stream.major_type == MFMediaType_Audio {
        tr!("音频流", "Audio stream")
    } else if stream.major_type == MFMediaType_Video {
        tr!("视频流", "Video stream")
    } else if stream.major_type == MFMediaType_Subtitle {
        tr!("字幕流", "Subtitle stream")
    } else {
        tr!("其它流", "Other stream")
    };
    let mut fields = vec![format!("{kind} #{}", stream.stream_index)];
    if stream.major_type != MFMediaType_Audio
        && stream.major_type != MFMediaType_Video
        && stream.major_type != MFMediaType_Subtitle
    {
        fields.push(trf!(
            "类型 GUID：{:?}",
            "Type GUID: {:?}",
            stream.major_type
        ));
    }
    for (label, value) in [
        (tr!("名称", "Name"), stream.name.as_deref()),
        (tr!("语言", "Language"), stream.language.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            fields.push(format!("{label}: {}", metadata_text(value, 256)));
        }
    }
    fields.push(match stream.codec {
        Some(codec) => match codec_name(codec) {
            Some(name) => trf!("编码：{name}", "Codec: {name}"),
            None => trf!("编码 GUID：{codec:?}", "Codec GUID: {codec:?}"),
        },
        None => tr!("编码未提供", "Codec not provided").to_string(),
    });
    if let Some(rate) = stream.sample_rate {
        fields.push(format!("{rate} Hz"));
    }
    if let Some(channels) = stream.channels {
        fields.push(trf!("{channels} 声道", "{channels} channels"));
    }
    fields.join(" · ")
}

fn codec_name(codec: windows::core::GUID) -> Option<&'static str> {
    // 只识别已声明的完整 GUID，不把任意子类型按字符编码猜成已知格式。
    [
        (MFVideoFormat_H264, "H.264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_AV1, "AV1"),
        (MFVideoFormat_VP80, "VP8"),
        (MFVideoFormat_VP90, "VP9"),
        (MFVideoFormat_MPEG2, "MPEG-2"),
        (MFAudioFormat_AAC, "AAC"),
        (MFAudioFormat_MP3, "MP3"),
        (MFAudioFormat_PCM, "PCM"),
        (MFAudioFormat_Float, "Float"),
    ]
    .into_iter()
    .find_map(|(known, name)| (codec == known).then_some(name))
}

fn metadata_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut text: String = chars
        .by_ref()
        .take(max_chars)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

pub(crate) fn card(
    path: Option<&Path>,
    player: Option<&Player>,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let p = crate::theme::palette(cx);
    let mut card = div()
        .id("video-details")
        .w_full()
        .min_h(px(0.))
        .max_h(px(140.))
        .overflow_y_scroll()
        .px(px(16.))
        .pb(px(8.))
        .bg(rgb(p.background))
        .border_b_1()
        .border_color(rgb(p.border));
    if let Some(path) = path {
        card = card.child(property(
            tr!("文件", "File"),
            path.display().to_string(),
            cx,
        ));
    }
    let Some(player) = player else {
        return card.child(div().text_size(px(12.)).text_color(rgb(p.muted)).child(tr!(
            "打开视频后显示属性",
            "Details appear when a video opens"
        )));
    };
    let (width, height) = player.size();
    card = card.child(property(
        tr!("解码画面", "Decoded frame"),
        if width == 0 || height == 0 {
            tr!("正在读取…", "Reading…").into()
        } else {
            format!("{width} × {height}")
        },
        cx,
    ));
    let (_, duration) = player.position();
    card = card.child(property(
        tr!("时长", "Duration"),
        if duration.is_zero() {
            tr!("未提供", "Not provided").into()
        } else {
            crate::video_shortcuts::format_time(duration)
        },
        cx,
    ));
    if let Some(info) = player.video_info() {
        let fps = info
            .frame_rate
            .filter(|(numerator, denominator)| *numerator > 0 && *denominator > 0)
            .map(|(numerator, denominator)| {
                format!("{:.3} fps", numerator as f64 / denominator as f64)
            })
            .unwrap_or_else(|| tr!("未提供", "Not provided").to_string());
        card = card
            .child(property(tr!("来源帧率", "Source frame rate"), fps, cx))
            .child(property(
                tr!("来源旋转", "Source rotation"),
                format!("{}°", info.rotation_degrees),
                cx,
            ));
        if let Some(error) = &info.stream_probe_error {
            card = card
                .child(property(
                    tr!("来源流", "Source streams"),
                    tr!("探测不完整，总数未知", "Probe incomplete; total unknown").into(),
                    cx,
                ))
                .child(property(
                    tr!("流探测错误", "Stream probe error"),
                    metadata_text(error, 512),
                    cx,
                ));
        } else {
            card = card.child(property(
                tr!("来源流", "Source streams"),
                trf!(
                    "媒体源报告 {} 路",
                    "{} reported by the media source",
                    info.streams.len()
                ),
                cx,
            ));
        }
        for stream in info.streams.iter().take(MAX_STREAMS_SHOWN) {
            // 独立文字行允许换行；不能用单行截断隐藏后面的编码或音频参数。
            card = card.child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .py(px(4.))
                    .text_size(px(11.))
                    .text_color(rgb(p.foreground))
                    .child(stream_description(stream)),
            );
        }
        if info
            .streams
            .iter()
            .any(|stream| stream.major_type == MFMediaType_Subtitle)
        {
            card = card.child(div().w_full().py(px(4.)).text_size(px(11.))
                .text_color(rgb(p.muted)).child(tr!(
                    "已探测到内嵌字幕，暂不显示；可载入本地 SRT/WebVTT 字幕。",
                    "Embedded subtitles detected; display is unsupported. Load a local SRT/VTT file."
                )));
        }
        if info.streams.len() > MAX_STREAMS_SHOWN {
            card = card.child(property(
                tr!("显示范围", "Display limit"),
                trf!(
                    "仅显示前 {MAX_STREAMS_SHOWN} 路",
                    "Showing the first {MAX_STREAMS_SHOWN} streams"
                ),
                cx,
            ));
        }
    } else {
        card = card.child(property(
            tr!("来源流", "Source streams"),
            tr!("正在读取…", "Reading…").into(),
            cx,
        ));
    }
    let audio = match player.audio_info() {
        None => tr!("正在读取…", "Reading…").to_string(),
        Some(audio) if audio.is_empty() => tr!("没有音频输出", "No audio output").to_string(),
        Some(audio) => audio
            .into_iter()
            .take(8)
            .map(|(rate, channels)| trf!("{} Hz / {} 声道", "{} Hz / {} channels", rate, channels))
            .collect::<Vec<_>>()
            .join(" · "),
    };
    card.child(property(tr!("当前音频", "Active audio"), audio, cx))
}

fn property(label: &'static str, value: String, cx: &App) -> gpui::Div {
    let p = crate::theme::palette(cx);
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .py(px(3.))
        .text_size(px(11.))
        .child(
            div()
                .w(px(112.))
                .flex_shrink_0()
                .text_color(rgb(p.muted))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_color(rgb(p.foreground))
                .child(value),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::GUID;

    fn stream(major_type: GUID) -> MediaStreamInfo {
        MediaStreamInfo {
            stream_index: 7,
            major_type,
            codec: Some(GUID::from_u128(0x12345678_1234_5678_9abc_def012345678)),
            sample_rate: Some(48_000),
            channels: Some(6),
            language: Some("zh-CN".into()),
            name: Some("原声\n多声道".into()),
        }
    }

    #[test]
    fn source_stream_description_preserves_reported_fields_and_unknown_guids() {
        let source = stream(GUID::from_u128(0x4321));
        let text = stream_description(&source);
        for value in [
            "#7".to_string(),
            "zh-CN".to_string(),
            "原声 多声道".to_string(),
            "48000 Hz".to_string(),
            format!("{:?}", source.major_type),
            format!("{:?}", source.codec.unwrap()),
        ] {
            assert!(text.contains(&value), "{text}");
        }
        assert!(text.starts_with(tr!("其它流", "Other stream")));
        assert!(!text.contains('\n'));
    }

    #[test]
    fn only_explicit_subtitle_major_type_is_labeled_as_subtitle() {
        use windows::Win32::Media::MediaFoundation::{MFMediaType_SAMI, MFMediaType_Script};
        assert!(stream_description(&stream(MFMediaType_Subtitle))
            .starts_with(tr!("字幕流", "Subtitle stream")));
        for major in [MFMediaType_SAMI, MFMediaType_Script, GUID::zeroed()] {
            assert!(stream_description(&stream(major)).starts_with(tr!("其它流", "Other stream")));
        }
        assert!(stream_description(&stream(MFMediaType_Audio))
            .starts_with(tr!("音频流", "Audio stream")));
        assert!(stream_description(&stream(MFMediaType_Video))
            .starts_with(tr!("视频流", "Video stream")));
    }

    #[test]
    fn known_audio_and_video_codec_names_do_not_replace_unknown_guids() {
        for (major, codec, name) in [
            (MFMediaType_Video, MFVideoFormat_H264, "H.264"),
            (MFMediaType_Audio, MFAudioFormat_AAC, "AAC"),
        ] {
            let mut source = stream(major);
            source.codec = Some(codec);
            let text = stream_description(&source);
            assert!(text.contains(name));
            assert!(!text.contains(&format!("{codec:?}")));
        }
        let unknown = stream(MFMediaType_Audio);
        assert!(stream_description(&unknown).contains(&format!("{:?}", unknown.codec.unwrap())));
    }
}
