//! 当前播放器的实验音频处理选项；显示请求设置，切换来源后使用默认值。
use super::{button, VideoView};
use crate::recording_preview::{AudioProcessing, PitchMode, ResampleQuality};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Div, Stateful};

#[derive(Clone, Copy)]
enum ProcessingChange {
    Pitch(PitchMode),
    Quality(ResampleQuality),
}

impl VideoView {
    pub(super) fn audio_processing_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = crate::theme::palette(cx);
        let controls = div()
            .id("video-audio-processing")
            .w_full()
            .flex()
            .flex_col()
            .gap(px(5.))
            .px(px(8.))
            .py(px(6.))
            .rounded_md()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.background))
            .text_size(px(12.))
            .text_color(rgb(p.foreground));
        let Some(player) = self
            .player
            .as_ref()
            .filter(|player| player.audio_selection().is_some())
        else {
            return controls
                .text_color(rgb(p.muted))
                .child(tr!(
                    "当前来源没有可用的音频处理选项",
                    "Audio processing options are unavailable for this source"
                ))
                .into_any_element();
        };
        let settings = player.audio_processing();
        let pitch_label = match settings.pitch {
            PitchMode::Resample => tr!("变调", "Change pitch"),
            PitchMode::PitchPreserving => tr!("保调", "Preserve pitch"),
        };
        let quality_label = match settings.quality {
            ResampleQuality::Standard => tr!("标准", "Standard"),
            ResampleQuality::High => tr!("高质量", "High quality"),
        };
        let mut pitch = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(5.))
            .child(tr!("倍速音高", "Speed pitch"));
        for (value, id, label) in [
            (
                PitchMode::Resample,
                "video-audio-pitch-resample",
                tr!("变调", "Change pitch"),
            ),
            (
                PitchMode::PitchPreserving,
                "video-audio-pitch-preserving",
                tr!("保调", "Preserve pitch"),
            ),
        ] {
            pitch = pitch.child(self.audio_processing_option(
                id,
                label,
                settings.pitch == value,
                ProcessingChange::Pitch(value),
                cx,
            ));
        }
        let mut quality = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(5.))
            .child(tr!("设备重采样", "Device resampling"));
        for (value, id, label) in [
            (
                ResampleQuality::Standard,
                "video-audio-quality-standard",
                tr!("标准", "Standard"),
            ),
            (
                ResampleQuality::High,
                "video-audio-quality-high",
                tr!("高质量", "High quality"),
            ),
        ] {
            quality = quality.child(self.audio_processing_option(
                id,
                label,
                settings.quality == value,
                ProcessingChange::Quality(value),
                cx,
            ));
        }
        controls
            .child(tr!("音频处理（实验）", "Audio processing (experimental)"))
            .child(
                div()
                    .text_color(rgb(p.muted))
                    .child(trf!("所选设置：{pitch_label} · {quality_label}", "Selected settings: {pitch_label} · {quality_label}")),
            )
            .child(pitch)
            .child(quality)
            .child(div().text_color(rgb(p.muted)).child(tr!(
                "质量用于 48 kHz 混音到输出设备的重采样；切换视频后恢复默认。",
                "Quality applies to resampling the 48 kHz mix for the output device. Switching videos resets these settings."
            )))
            .into_any_element()
    }

    fn audio_processing_option(
        &self,
        id: &'static str,
        label: &'static str,
        selected: bool,
        change: ProcessingChange,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let source_generation = self.source_generation;
        let source = self.path.clone();
        button(id, label, selected, cx)
            .h(px(28.))
            .opacity(if self.prompt_pending { 0.45 } else { 1. })
            .on_click(cx.listener(move |view, _, _, cx| {
                cx.stop_propagation();
                if view.prompt_pending
                    || !view.show_audio
                    || view.source_generation != source_generation
                    || view.path != source
                    || view
                        .player
                        .as_ref()
                        .is_none_or(|player| player.audio_selection().is_none())
                {
                    return;
                }
                view.finish_scrub(None, true);
                if let Some(player) = &view.player {
                    let current = player.audio_processing();
                    let settings = match change {
                        ProcessingChange::Pitch(pitch) => AudioProcessing { pitch, ..current },
                        ProcessingChange::Quality(quality) => {
                            AudioProcessing { quality, ..current }
                        }
                    };
                    if let Err(error) = player.set_audio_processing(settings) {
                        view.notice = Some(trf!(
                            "无法设置音频处理：{error}",
                            "Cannot set audio processing: {error}"
                        ));
                    }
                    cx.notify();
                }
            }))
    }
}
