//! 历史详情中的紧凑播放表面，复用独立窗口的播放器、画布与拖动状态。
use super::*;
use gpui::{AnyElement, Entity};

impl VideoView {
    /// 创建暂停、静音的只读预览，不夺取宿主焦点。
    /// 调用方换选、隐藏详情或离开页面时须先 shutdown_embedded，再释放实体。
    pub(crate) fn embedded(
        path: PathBuf,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let queue = VideoFileQueue::from_paths(vec![path])?;
        let opener = video_open::VideoOpener::new()?;
        let view = cx
            .new(|cx| Self::new_session(queue, opener, cx.focus_handle(), Presentation::Embedded));
        Self::watch_session(&view, window, cx);
        view.update(cx, |view, cx| {
            view.start_current();
            cx.notify();
        });
        Ok(view)
    }

    /// 与历史页快捷键隔离；父页可在处理删除、撤销等动作前检查。
    pub(crate) fn embedded_has_focus(&self, window: &Window) -> bool {
        self.presentation == Presentation::Embedded && self.focus.is_focused(window)
    }

    /// 历史页的空格入口可显式交给当前预览，随后按键由预览自身消费。
    pub(crate) fn toggle_embedded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.presentation == Presentation::Embedded {
            window.focus(&self.focus);
            self.toggle_playback();
            cx.notify();
        }
    }

    /// 先停止工作，再由父页 take 实体；旧场景短暂持有引用也不能继续播放。
    pub(crate) fn shutdown_embedded(&mut self, cx: &mut Context<Self>) {
        if self.presentation != Presentation::Embedded {
            return;
        }
        self.source_generation = self.source_generation.wrapping_add(1);
        self.minimize.cancel_resume();
        self.opener.cancel();
        self.opening = None;
        self.finish_scrub(None, false);
        self.cancel_frame_output(false);
        self.reset_gif_for_source();
        self.clear_subtitles();
        self.player = None;
        self.path = None;
        self.clear_frame();
        cx.notify();
    }

    fn open_embedded_window(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        self.finish_scrub(None, false);
        self.minimize.cancel_resume();
        if let Some(player) = &mut self.player {
            player.set_playing(false);
        }
        self.notice = crate::file_launch_command::open_video(&path)
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    fn embedded_shortcut(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match video_shortcuts::resolve_embedded(&event.keystroke) {
            Some(VideoShortcut::ExitFullscreen) if event.is_held => cx.stop_propagation(),
            // 没有拖动时交给历史页，允许Esc取消选择并收起预览。
            Some(VideoShortcut::ExitFullscreen) if self.scrub.is_none() => {}
            Some(_) => self.shortcut(event, window, cx),
            None => {
                let modifiers = event.keystroke.modifiers;
                let search = event.keystroke.key == "f"
                    && modifiers.control
                    && !modifiers.shift
                    && !modifiers.alt
                    && !modifiers.platform
                    && !modifiers.function;
                // 搜索仍由历史页负责；其它按键不能冒泡删除/打开当前条目。
                if !search {
                    cx.stop_propagation();
                }
            }
        }
    }

    pub(super) fn render_embedded(
        &self,
        picture: impl IntoElement,
        seek: impl IntoElement,
        position: Duration,
        duration: Duration,
        playing: bool,
        ended: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = crate::theme::palette(cx);
        let play_label = if playing {
            tr!("暂停", "Pause")
        } else if ended {
            tr!("重播", "Replay")
        } else {
            tr!("播放", "Play")
        };
        let row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(4.))
            .child(
                button("video-embedded-play", play_label, false, cx).on_click(cx.listener(
                    |view, _, _, cx| {
                        view.toggle_playback();
                        cx.notify();
                    },
                )),
            )
            .child(
                button(
                    "video-embedded-mute",
                    if self.muted {
                        tr!("静音", "Muted")
                    } else {
                        tr!("声音", "Sound")
                    },
                    self.muted,
                    cx,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.toggle_muted();
                    cx.notify();
                })),
            )
            .child(
                button(
                    "video-embedded-open",
                    tr!("独立打开", "Open viewer"),
                    false,
                    cx,
                )
                .on_click(cx.listener(|view, _, _, cx| view.open_embedded_window(cx))),
            );
        let mut controls = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .px(px(6.))
            .pb(px(4.))
            .border_t_1()
            .border_color(rgb(p.border))
            .child(seek)
            .child(row)
            .child(
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(format!(
                        "{} / {}",
                        video_shortcuts::format_time(position),
                        video_shortcuts::format_time(duration)
                    )),
            );
        if let Some(warning) = self
            .notice
            .clone()
            .or_else(|| self.frame_draw_error.clone())
            .or_else(|| self.player.as_ref().and_then(Player::audio_error))
        {
            controls = controls.child(
                div()
                    .max_h(px(32.))
                    .overflow_hidden()
                    .line_clamp(2)
                    .text_size(px(11.))
                    .text_color(rgb(p.danger))
                    .child(warning),
            );
        }
        div()
            .id("video-embedded")
            .relative()
            .track_focus(&self.focus)
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(rgb(p.background))
            .text_color(rgb(p.foreground))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, _| window.focus(&view.focus)),
            )
            .on_key_down(cx.listener(Self::embedded_shortcut))
            .child(Self::scrub_gestures(cx))
            .child(picture)
            .child(controls)
            .into_any_element()
    }

    /// 独立窗口与内嵌表面共用原生拖动捕获，不复制seek实现。
    pub(super) fn scrub_gestures(cx: &mut Context<Self>) -> AnyElement {
        let gesture_view = cx.entity().downgrade();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let weak = gesture_view.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture {
                        return;
                    }
                    let _ = weak.update(cx, |view, cx| {
                        if view.scrub.is_none() {
                            return;
                        }
                        let lost_capture = view.scrub_capture.is_some_and(|owner| unsafe {
                            windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() != owner
                        });
                        if event.pressed_button == Some(MouseButton::Left) && !lost_capture {
                            view.scrub_at(event.position.x, false);
                        } else {
                            view.finish_scrub(None, true);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    });
                });
                let weak = gesture_view.clone();
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Capture || event.button != MouseButton::Left {
                        return;
                    }
                    let _ = weak.update(cx, |view, cx| {
                        if view.scrub.is_some() {
                            view.finish_scrub(Some(event.position.x), true);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    });
                });
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }
}
