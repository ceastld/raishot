//! 录前输入检测的显式入口和只读状态；停止完成后再交接录制。
use super::*;

impl RecordingView {
    pub(super) fn stop_audio_input_check(&mut self) {
        if let Some(check) = self.audio_input_check.as_mut() {
            check.close();
            self.audio_input_stopping = true;
        }
    }

    pub(super) fn toggle_audio_input_check(&mut self, cx: &mut Context<Self>) {
        if self.closing
            || self.abandoned.is_some()
            || self.stage != Stage::Idle
            || self.options.preset.is_gif()
            || !crate::file_launch_command::experimental_media_enabled()
            || self.record_after_input_check
            || self.audio_input_stopping
        {
            return;
        }
        if self.audio_input_check.is_some() {
            self.stop_audio_input_check();
            cx.notify();
            return;
        }
        let mut sources = Vec::new();
        if self.options.system_audio {
            sources.push(AudioSource::System);
        }
        if self.options.microphone {
            sources.push(AudioSource::Microphone);
        }
        if sources.is_empty() {
            self.info(tr!(
                "请先勾选系统音频或麦克风",
                "Select system audio or microphone first"
            ));
            cx.notify();
            return;
        }
        match AudioInputCheck::start(
            sources,
            self.options.system_output.clone(),
            self.options.microphone_input.clone(),
        ) {
            Ok(check) => {
                self.audio_input_check = Some(check);
                self.audio_input_stopping = false;
                self.audio_input_revision = None;
            }
            Err(error) => self.fail(trf!(
                "无法检测音频输入：{error}",
                "Cannot check audio input: {error}"
            )),
        }
        cx.notify();
    }

    pub(super) fn poll_audio_input_check(&mut self, cx: &mut Context<Self>) {
        if self.closing || self.abandoned.is_some() {
            self.record_after_input_check = false;
        }
        let in_audio_panel = matches!(
            self.flyout,
            Some(Flyout::Audio | Flyout::OutputDevice | Flyout::InputDevice)
        );
        if self.closing
            || self.abandoned.is_some()
            || self.stage != Stage::Idle
            || self.options.preset.is_gif()
            || !crate::file_launch_command::experimental_media_enabled()
            || !in_audio_panel
        {
            self.stop_audio_input_check();
        }
        let mut changed = false;
        let mut remove = false;
        if let Some(check) = self.audio_input_check.as_ref() {
            let revision = check.snapshot().revision;
            if self.audio_input_revision != Some(revision) {
                self.audio_input_revision = Some(revision);
                changed = true;
            }
            remove = self.audio_input_stopping && check.is_closed();
        }
        if remove {
            self.audio_input_check = None;
            self.audio_input_stopping = false;
            self.audio_input_revision = None;
            changed = true;
        }
        if self.record_after_input_check && self.audio_input_check.is_none() {
            self.record_after_input_check = false;
            if self.stage == Stage::Idle && !self.closing && self.abandoned.is_none() {
                self.toggle_record(cx);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    pub(super) fn audio_input_check_panel(&self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let snapshot = self
            .audio_input_check
            .as_ref()
            .map(AudioInputCheck::snapshot);
        let ended = self
            .audio_input_check
            .as_ref()
            .is_some_and(AudioInputCheck::is_closed)
            || snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.error.is_some());
        let label = if self.audio_input_stopping {
            tr!("正在停止检测…", "Stopping check…")
        } else if ended {
            tr!("清除检测结果", "Clear check results")
        } else if self.audio_input_check.is_some() {
            tr!("停止输入检测", "Stop input check")
        } else {
            tr!("检测音频输入", "Check audio input")
        };
        let enabled = !self.closing
            && self.abandoned.is_none()
            && self.stage == Stage::Idle
            && !self.options.preset.is_gif()
            && crate::file_launch_command::experimental_media_enabled()
            && !self.audio_input_stopping
            && !self.record_after_input_check;
        let button = div()
            .id("rec-check-audio-input")
            .h(px(28.))
            .flex_shrink_0()
            .px(px(6.))
            .flex()
            .items_center()
            .rounded_md()
            .text_size(px(11.))
            .text_color(rgb(if enabled { p.accent } else { p.muted }))
            .when(enabled, |row| {
                row.cursor_pointer().hover(move |row| row.bg(rgb(p.hover)))
            })
            .child(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                if enabled {
                    view.toggle_audio_input_check(cx);
                }
            }));
        let mut body = div()
            .id("rec-audio-input-status")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(px(6.))
            .flex()
            .flex_col()
            .gap(px(5.))
            .text_size(px(11.))
            .text_color(rgb(p.foreground));
        if self.audio_input_stopping {
            body = body.child(div().text_color(rgb(p.muted)).child(
                if self.record_after_input_check {
                    tr!(
                        "释放设备后开始录制；再次按开始可取消",
                        "Recording starts after release; press Record again to cancel"
                    )
                } else {
                    tr!("正在释放音频设备…", "Releasing audio devices…")
                },
            ));
        }
        if let Some(snapshot) = snapshot.as_ref() {
            if snapshot.starting {
                body = body.child(div().text_color(rgb(p.muted)).child(tr!(
                    "正在启动输入检测，等待设备就绪…",
                    "Starting input check; waiting for devices…"
                )));
            } else if ended && !self.audio_input_stopping {
                body = body.child(div().text_color(rgb(p.muted)).child(tr!(
                    "检测已结束，以下为最后观测",
                    "Check ended; last observations below"
                )));
            }
            if let Some(error) = &snapshot.error {
                body = body.child(div().text_color(rgb(RED)).child(error.clone()));
            }
            for warning in &snapshot.warnings {
                if !snapshot
                    .sources
                    .iter()
                    .any(|source| source.warning.as_ref() == Some(warning))
                {
                    body = body.child(div().text_color(rgb(p.muted)).child(warning.clone()));
                }
            }
            for source in &snapshot.sources {
                let source_label = match source.source {
                    AudioSource::System => tr!("系统音频", "System audio"),
                    AudioSource::Microphone => tr!("麦克风", "Microphone"),
                };
                let state_label = match source.state {
                    CaptureSourceState::Starting => tr!("启动中", "Starting"),
                    CaptureSourceState::Running => tr!("运行中", "Running"),
                    CaptureSourceState::Stopped => tr!("已停止", "Stopped"),
                    CaptureSourceState::Failed => tr!("设备错误", "Device error"),
                };
                let state_text = if ended || self.audio_input_stopping {
                    trf!(
                        "{source_label} · 最后状态：{state_label}",
                        "{source_label} · Last: {state_label}"
                    )
                } else {
                    trf!(
                        "{source_label} · {state_label}",
                        "{source_label} · {state_label}"
                    )
                };
                let mut card = div()
                    .flex_shrink_0()
                    .py(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .border_t_1()
                    .border_color(rgb(p.border))
                    .child(
                        div()
                            .text_color(rgb(if source.state == CaptureSourceState::Failed {
                                RED
                            } else {
                                p.foreground
                            }))
                            .child(state_text),
                    );
                if let Some(endpoint) = &source.endpoint {
                    let name = device_choices::option_label(
                        endpoint,
                        self.device_choices.entries(endpoint.kind),
                    );
                    card = card.child(
                        div()
                            .text_color(rgb(p.muted))
                            .child(trf!("实际设备：{name}", "Device: {name}")),
                    );
                }
                if let Some(reason) = source.fallback {
                    card = card.child(div().text_color(rgb(p.muted)).child(trf!(
                        "已回退默认设备：{reason}",
                        "Using default device: {reason}"
                    )));
                }
                if let Some(warning) = &source.warning {
                    card = card.child(div().text_color(rgb(p.muted)).child(warning.clone()));
                }
                if let Some(error) = &source.error {
                    card = card.child(div().text_color(rgb(RED)).child(error.clone()));
                }
                if let Some(level) = source.level {
                    let percent = match source.source {
                        AudioSource::System => self.options.initial_mix.system,
                        AudioSource::Microphone => self.options.initial_mix.microphone,
                    }
                    .min(MixLevels::MAX);
                    card = card.child(div().text_color(rgb(p.muted)).child(if percent == 0 {
                        tr!(
                            "成片已静音；下方仍显示原始输入",
                            "Mix muted; raw input shown below"
                        )
                        .to_string()
                    } else {
                        trf!(
                            "原始输入 · 成片增益 {percent}%",
                            "Raw input · Mix gain {percent}%"
                        )
                    }));
                    let live = !ended
                        && !self.audio_input_stopping
                        && source.state == CaptureSourceState::Running;
                    card = card.child(input_level(level, live, p));
                } else {
                    card = card.child(div().text_color(rgb(p.muted)).child(
                        if source.state == CaptureSourceState::Running {
                            tr!("尚未收到输入包", "No input packets received")
                        } else if source.state == CaptureSourceState::Starting {
                            tr!(
                                "等待来源启动，尚无输入数据",
                                "Waiting for source; no input data yet"
                            )
                        } else {
                            tr!("没有观测到输入数据", "No input data observed")
                        },
                    ));
                }
                body = body.child(card);
            }
        } else {
            body = body.child(div().text_color(rgb(p.muted)).child(tr!(
                "点击检测才开始采集所选来源；不播放或保存声音。",
                "Check starts capture of selected sources; no playback or saving."
            )));
        }
        div()
            .h(px(180.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(p.background))
            .child(button)
            .child(body)
    }
}

fn input_level(level: CaptureLevel, live: bool, p: Palette) -> gpui::Div {
    let fresh = level.is_fresh();
    let status = if !live {
        tr!("最后观测电平", "Last input level").to_string()
    } else if !fresh {
        let millis = level.age.as_millis();
        trf!(
            "输入已过期（{millis} ms 前）",
            "Stale input ({millis} ms ago)"
        )
    } else if level.silent_packet {
        tr!("收到设备静音包", "Device supplied a silent packet").to_string()
    } else if level.peak.iter().all(|peak| *peak == 0.) {
        tr!(
            "收到输入包，当前为零电平",
            "Input received; current level is zero"
        )
        .to_string()
    } else {
        tr!("正在接收输入", "Receiving input").to_string()
    };
    let peak_left = input_db(level.peak[0]);
    let peak_right = input_db(level.peak[1]);
    let rms_left = input_db(level.rms[0]);
    let rms_right = input_db(level.rms[1]);
    let peak = level.peak[0].max(level.peak[1]).clamp(0., 1.);
    let color = if !live || !fresh {
        p.muted
    } else if level.clipped {
        RED
    } else {
        p.accent
    };
    let mut meter = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(div().text_color(rgb(p.muted)).child(status))
        .child(
            div()
                .w(px(180.))
                .h(px(3.))
                .rounded_full()
                .bg(rgb(p.border))
                .child(
                    div()
                        .w(px(180. * peak))
                        .h_full()
                        .rounded_full()
                        .bg(rgb(color)),
                ),
        )
        .child(trf!(
            "峰值 L {peak_left} / R {peak_right} dBFS",
            "Peak L {peak_left} / R {peak_right} dBFS"
        ))
        .child(trf!(
            "均方根 L {rms_left} / R {rms_right} dBFS",
            "RMS L {rms_left} / R {rms_right} dBFS"
        ));
    if level.clipped {
        meter = meter.child(div().text_color(rgb(RED)).child(tr!(
            "输入达到数字满幅，可能削波",
            "Input reaches full scale; clipping possible"
        )));
    }
    meter
}

fn input_db(amplitude: f32) -> String {
    if amplitude.is_finite() && amplitude > 0. {
        format!("{:.1}", 20. * f64::from(amplitude).log10())
    } else {
        "−∞".into()
    }
}
