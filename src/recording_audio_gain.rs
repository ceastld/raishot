//! 录前增益仅用于下次成片混音；原始分轨与输入检测电平保持原值。
use super::*;

#[derive(Clone, Copy)]
enum GainAction {
    Decrease,
    Increase,
    ToggleMute,
}

impl RecordingView {
    fn can_edit_audio_initial_gain(&self) -> bool {
        self.stage == Stage::Idle
            && !self.closing
            && !self.options.preset.is_gif()
            && crate::file_launch_command::experimental_media_enabled()
            && !self.record_after_input_check
    }

    fn adjust_audio_initial_gain(
        &mut self,
        source: AudioSource,
        action: GainAction,
        cx: &mut Context<Self>,
    ) {
        let selected = match source {
            AudioSource::System => self.options.system_audio,
            AudioSource::Microphone => self.options.microphone,
        };
        if !selected || !self.can_edit_audio_initial_gain() {
            return;
        }
        let mut levels = self.options.initial_mix.normalized();
        let current = match source {
            AudioSource::System => &mut levels.system,
            AudioSource::Microphone => &mut levels.microphone,
        };
        *current = match action {
            GainAction::Decrease => current.saturating_sub(25),
            GainAction::Increase => current.saturating_add(25).min(MixLevels::MAX),
            GainAction::ToggleMute if *current > 0 => 0,
            GainAction::ToggleMute => match source {
                AudioSource::System => MixLevels::default().system,
                AudioSource::Microphone => MixLevels::default().microphone,
            },
        };
        if levels != self.options.initial_mix {
            self.options.initial_mix = levels;
            self.save_options();
            cx.notify();
        }
    }

    pub(super) fn audio_initial_gain_panel(&self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let enabled = self.can_edit_audio_initial_gain();
        let mut panel = div()
            .h(px(90.))
            .flex_shrink_0()
            .px(px(6.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .text_size(px(11.))
            .text_color(rgb(p.foreground))
            .child(
                div()
                    .h(px(16.))
                    .flex_shrink_0()
                    .text_color(rgb(p.muted))
                    .child(tr!("下次成片混音", "Next recording mix")),
            );
        let sources = [
            (AudioSource::System, self.options.system_audio),
            (AudioSource::Microphone, self.options.microphone),
        ];
        for (source, selected) in sources {
            if !selected {
                continue;
            }
            let (label, percent, ids) = match source {
                AudioSource::System => (
                    tr!("系统音频", "System"),
                    self.options.initial_mix.system.min(MixLevels::MAX),
                    [
                        "rec-gain-system-down",
                        "rec-gain-system-up",
                        "rec-gain-system-mute",
                    ],
                ),
                AudioSource::Microphone => (
                    tr!("麦克风", "Microphone"),
                    self.options.initial_mix.microphone.min(MixLevels::MAX),
                    ["rec-gain-mic-down", "rec-gain-mic-up", "rec-gain-mic-mute"],
                ),
            };
            let mut row = div()
                .h(px(26.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(3.))
                .child(div().flex_1().min_w(px(0.)).child(label))
                .child(
                    div()
                        .w(px(36.))
                        .flex_shrink_0()
                        .child(format!("{percent}%")),
                );
            for (index, (action, text, width, available)) in [
                (
                    GainAction::Decrease,
                    tr!("−", "−"),
                    20.,
                    enabled && percent > 0,
                ),
                (
                    GainAction::Increase,
                    tr!("+", "+"),
                    20.,
                    enabled && percent < MixLevels::MAX,
                ),
                (
                    GainAction::ToggleMute,
                    if percent == 0 {
                        tr!("恢复", "Restore")
                    } else {
                        tr!("静音", "Mute")
                    },
                    43.,
                    enabled,
                ),
            ]
            .into_iter()
            .enumerate()
            {
                row = row.child(
                    div()
                        .id(ids[index])
                        .w(px(width))
                        .h(px(24.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_color(rgb(if available { p.accent } else { p.muted }))
                        .opacity(if available { 1. } else { 0.5 })
                        .when(available, |button| {
                            button
                                .cursor_pointer()
                                .hover(move |button| button.bg(rgb(p.hover)))
                        })
                        .child(text)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if available {
                                view.adjust_audio_initial_gain(source, action, cx);
                            }
                        })),
                );
            }
            panel = panel.child(row);
        }
        panel.child(div().text_color(rgb(p.muted)).child(
            if self.options.system_audio || self.options.microphone {
                tr!("原始分轨不变", "Original tracks stay unchanged")
            } else {
                tr!(
                    "先选择音频来源以调整混音",
                    "Choose an audio source to set its mix"
                )
            },
        ))
    }
}
