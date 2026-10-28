//! 原视频音频流的显式选择；只使用播放器已有元数据，不在界面探测文件。
use super::{button, video_controls_details::stream_description, VideoView};
use crate::{media_foundation::MediaStreamInfo, preview_audio::AudioStreamSelection};
use gpui::{canvas, div, prelude::*, px, rgb, Context};
use windows::Win32::Media::MediaFoundation::MFMediaType_Audio;

const MAX_AUDIO_STREAMS_SHOWN: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioKey {
    Toggle,
    Previous,
    Next,
    First,
    Last,
    Apply,
    Close,
    Repeated,
}

fn audio_key(key: &gpui::Keystroke, visible: bool, held: bool) -> Option<AudioKey> {
    let modifiers = key.modifiers;
    if modifiers.control
        || modifiers.shift
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
    {
        return None;
    }
    let command = match key.key.as_str() {
        "a" => AudioKey::Toggle,
        "up" if visible => AudioKey::Previous,
        "down" if visible => AudioKey::Next,
        "home" if visible => AudioKey::First,
        "end" if visible => AudioKey::Last,
        "enter" if visible => AudioKey::Apply,
        "escape" if visible => AudioKey::Close,
        _ => return None,
    };
    Some(
        if held
            && matches!(
                command,
                AudioKey::Toggle | AudioKey::Apply | AudioKey::Close
            )
        {
            AudioKey::Repeated
        } else {
            command
        },
    )
}

fn audio_candidates(
    selection: Option<AudioStreamSelection>,
    streams: Option<&[MediaStreamInfo]>,
) -> Vec<AudioStreamSelection> {
    if selection.is_none() {
        return Vec::new();
    }
    let mut candidates = vec![AudioStreamSelection::Disabled, AudioStreamSelection::First];
    if let Some(streams) = streams {
        candidates.extend(
            streams
                .iter()
                .filter(|stream| stream.major_type == MFMediaType_Audio)
                .take(MAX_AUDIO_STREAMS_SHOWN)
                .map(|stream| AudioStreamSelection::Stream(stream.stream_index)),
        );
    }
    candidates
}

fn audio_cursor(
    current: Option<usize>,
    candidates: &[AudioStreamSelection],
    selection: Option<AudioStreamSelection>,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    if let Some(current) = current {
        return Some(current.min(candidates.len() - 1));
    }
    let selection = selection?;
    candidates
        .iter()
        .position(|candidate| *candidate == selection)
}

fn move_audio_cursor(current: Option<usize>, count: usize, command: AudioKey) -> Option<usize> {
    let last = count.checked_sub(1)?;
    let index = current.unwrap_or(0).min(last);
    Some(match command {
        AudioKey::Previous => index.saturating_sub(1),
        AudioKey::Next if current.is_some() => index.saturating_add(1).min(last),
        AudioKey::First | AudioKey::Next => 0,
        AudioKey::Last => last,
        _ => index,
    })
}

fn audio_visible_offset(
    viewport: gpui::Bounds<gpui::Pixels>,
    item: gpui::Bounds<gpui::Pixels>,
    current: gpui::Pixels,
    maximum: gpui::Pixels,
) -> Option<gpui::Pixels> {
    if viewport.size.height <= px(0.) || item.size.height <= px(0.) {
        return None;
    }
    let next = if item.size.height > viewport.size.height || item.top() + current < viewport.top() {
        viewport.top() - item.top()
    } else if item.bottom() + current > viewport.bottom() {
        viewport.bottom() - item.bottom()
    } else {
        current
    };
    Some(next.clamp(-maximum.max(px(0.)), px(0.)))
}

impl VideoView {
    fn toggle_audio_menu(&mut self) {
        self.finish_scrub(None, true);
        if self.show_audio {
            self.hide_audio();
        } else {
            self.show_audio = true;
            self.audio_keyboard_index = None;
            self.audio_keyboard_scroll.set(true);
            self.hide_recent();
            self.hide_queue();
            self.hide_gif();
            self.show_details = false;
        }
    }

    pub(super) fn audio_shortcut(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.prompt_pending {
            return false;
        }
        let Some(command) = audio_key(&event.keystroke, self.show_audio, event.is_held) else {
            return false;
        };
        cx.stop_propagation();
        if command == AudioKey::Repeated {
            return true;
        }
        self.finish_scrub(None, true);
        match command {
            AudioKey::Toggle => self.toggle_audio_menu(),
            AudioKey::Close => self.hide_audio(),
            _ => {
                let selection = self
                    .player
                    .as_ref()
                    .and_then(|player| player.audio_selection());
                let info = self.player.as_ref().and_then(|player| player.video_info());
                let candidates =
                    audio_candidates(selection, info.as_ref().map(|info| info.streams.as_slice()));
                let current = audio_cursor(self.audio_keyboard_index, &candidates, selection);
                if command == AudioKey::Apply {
                    if let Some((index, stream)) = current
                        .and_then(|index| candidates.get(index).map(|stream| (index, *stream)))
                    {
                        self.audio_keyboard_index = Some(index);
                        self.audio_keyboard_scroll.set(true);
                        if let Some(player) = &self.player {
                            // 每次Enter都提交，允许失败后重试同一音轨。
                            match stream {
                                AudioStreamSelection::Disabled => player.select_audio_stream(None),
                                AudioStreamSelection::First => player.select_default_audio_stream(),
                                AudioStreamSelection::Stream(index) => {
                                    player.select_audio_stream(Some(index))
                                }
                            }
                        }
                    }
                } else {
                    self.audio_keyboard_index =
                        move_audio_cursor(current, candidates.len(), command);
                    self.audio_keyboard_scroll.set(true);
                }
            }
        }
        cx.notify();
        true
    }

    pub(super) fn audio_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let disabled = matches!(
            self.player
                .as_ref()
                .and_then(|player| player.audio_selection()),
            Some(AudioStreamSelection::Disabled)
        );
        let label = if disabled {
            tr!("音轨关闭 (A)", "Audio off (A)")
        } else {
            tr!("音轨 (A)", "Audio (A)")
        };
        button("video-audio", label, self.show_audio, cx).on_click(cx.listener(|view, _, _, cx| {
            cx.stop_propagation();
            if view.prompt_pending {
                return;
            }
            view.toggle_audio_menu();
            cx.notify();
        }))
    }

    pub(super) fn hide_audio(&mut self) {
        self.show_audio = false;
        self.audio_keyboard_index = None;
        self.audio_keyboard_scroll.set(false);
    }

    fn audio_menu_rows(
        &self,
        mut children: Vec<gpui::AnyElement>,
        candidate_rows: &[usize],
        cursor: Option<usize>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if self.audio_keyboard_scroll.get() {
            if let Some(row) = cursor.and_then(|index| candidate_rows.get(index)) {
                let row = *row;
                let source_generation = self.source_generation;
                let weak = cx.entity().downgrade();
                children.push(
                    canvas(
                        move |_, _, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                if !view.show_audio
                                    || view.source_generation != source_generation
                                    || !view.audio_keyboard_scroll.get()
                                {
                                    return;
                                }
                                // 根prepaint已更新本轮child/viewport边界，且已释放offset借用。
                                let Some(item) = view.audio_scroll.bounds_for_item(row) else {
                                    return;
                                };
                                let viewport = view.audio_scroll.bounds();
                                let mut offset = view.audio_scroll.offset();
                                let maximum = view.audio_scroll.max_offset().height;
                                let Some(next) =
                                    audio_visible_offset(viewport, item, offset.y, maximum)
                                else {
                                    return;
                                };
                                if next == offset.y {
                                    view.audio_keyboard_scroll.set(false);
                                    return;
                                }
                                offset.y = next;
                                view.audio_scroll.set_offset(offset);
                                // 跨帧只保留逻辑滚动意图；下一轮重新解析候选行并确认可见。
                                let notify = cx.entity().downgrade();
                                cx.defer(move |cx| {
                                    let _ = notify.update(cx, |_, cx| cx.notify());
                                });
                            });
                        },
                        |_, _, _, _| (),
                    )
                    .absolute()
                    .w(px(0.))
                    .h(px(0.))
                    .into_any_element(),
                );
            }
        }
        let p = crate::theme::palette(cx);
        div()
            .id("video-audio-menu")
            .relative()
            .w_full()
            .max_h(px(120.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.audio_scroll)
            .flex_shrink()
            .px(px(16.))
            .py(px(6.))
            .bg(rgb(p.background))
            .text_color(rgb(p.muted))
            .border_b_1()
            .border_color(rgb(p.border))
            .children(children)
            .into_any_element()
    }

    pub(super) fn audio_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.show_audio {
            return None;
        }
        let p = crate::theme::palette(cx);
        let mut children = vec![audio_note(tr!(
            "A 开关 · ↑↓ 选择 · Enter 应用 · Esc 关闭",
            "A toggle · ↑↓ choose · Enter apply · Esc close"
        ))
        .into_any_element()];
        let mut candidate_rows = Vec::new();
        let Some(player) = &self.player else {
            children.push(
                audio_note(if self.opening.is_some() {
                    tr!("正在打开视频…", "Opening video…")
                } else {
                    tr!(
                        "打开视频后选择音轨",
                        "Open a video to choose an audio track"
                    )
                })
                .into_any_element(),
            );
            return Some(self.audio_menu_rows(children, &[], None, cx));
        };
        if let Some(error) = player.audio_error() {
            children.push(
                audio_note(trf!("音频不可用：{error}", "Audio unavailable: {error}"))
                    .text_color(rgb(p.danger))
                    .into_any_element(),
            );
        }
        let Some(selection) = player.audio_selection() else {
            children.push(
                audio_note(tr!(
                    "当前来源暂不支持切换音轨",
                    "Audio track selection is unavailable for this source"
                ))
                .into_any_element(),
            );
            return Some(self.audio_menu_rows(children, &[], None, cx));
        };
        let info = player.video_info();
        let candidates = audio_candidates(
            Some(selection),
            info.as_ref().map(|info| info.streams.as_slice()),
        );
        let cursor = audio_cursor(self.audio_keyboard_index, &candidates, Some(selection));
        let selected_track = match selection {
            AudioStreamSelection::First => {
                tr!("所选音轨：默认音轨", "Selected track: default").to_string()
            }
            AudioStreamSelection::Stream(index) => {
                trf!(
                    "所选音轨：音频流 #{index}",
                    "Selected track: audio stream #{index}"
                )
            }
            AudioStreamSelection::Disabled => {
                tr!("所选音轨：关闭", "Selected track: off").to_string()
            }
        };
        let source_generation = self.source_generation;
        children.push(audio_note(selected_track).into_any_element());
        children.push(self.audio_processing_controls(cx));
        for choice in [AudioStreamSelection::Disabled, AudioStreamSelection::First] {
            let candidate_index = candidate_rows.len();
            let selected = selection == choice;
            let focused = cursor == Some(candidate_index);
            let (id, label) = match choice {
                AudioStreamSelection::Disabled => (
                    "video-disable-audio",
                    if focused {
                        tr!("› 禁用音轨", "› Disable audio")
                    } else {
                        tr!("禁用音轨", "Disable audio")
                    },
                ),
                AudioStreamSelection::First => (
                    "video-default-audio",
                    if focused {
                        tr!("› 默认音轨", "› Default audio")
                    } else {
                        tr!("默认音轨", "Default audio")
                    },
                ),
                AudioStreamSelection::Stream(_) => unreachable!(),
            };
            let source = self.path.clone();
            candidate_rows.push(children.len());
            children.push(
                button(id, label, selected, cx)
                    .w_full()
                    .justify_start()
                    .border_1()
                    .border_color(rgb(if focused {
                        if selected {
                            p.on_accent
                        } else {
                            p.accent
                        }
                    } else if selected {
                        p.accent
                    } else {
                        p.background
                    }))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        if view.prompt_pending
                            || !view.show_audio
                            || view.path != source
                            || view.source_generation != source_generation
                        {
                            return;
                        }
                        view.finish_scrub(None, true);
                        if let Some(player) = &view.player {
                            if player.audio_selection().is_some() {
                                match choice {
                                    AudioStreamSelection::Disabled => {
                                        player.select_audio_stream(None)
                                    }
                                    AudioStreamSelection::First => {
                                        player.select_default_audio_stream()
                                    }
                                    AudioStreamSelection::Stream(_) => unreachable!(),
                                }
                                view.audio_keyboard_index = Some(candidate_index);
                                view.audio_keyboard_scroll.set(true);
                                cx.notify();
                            }
                        }
                    }))
                    .into_any_element(),
            );
        }
        if player.audio_switching() {
            children.push(
                audio_note(tr!("正在切换音轨…", "Switching audio tracks…")).into_any_element(),
            );
        }
        let Some(info) = info else {
            let message = player.error().map_or_else(
                || tr!("正在读取原视频音轨…", "Reading source audio tracks…").to_string(),
                |error| {
                    trf!(
                        "无法读取音轨信息：{error}",
                        "Cannot read audio track information: {error}"
                    )
                },
            );
            children.push(audio_note(message).into_any_element());
            return Some(self.audio_menu_rows(children, &candidate_rows, cursor, cx));
        };
        if let Some(error) = &info.stream_probe_error {
            children.push(
                audio_note(trf!(
                    "音轨信息不完整：{error}",
                    "Audio track information is incomplete: {error}"
                ))
                .text_color(rgb(p.danger))
                .into_any_element(),
            );
        }
        let mut shown = 0;
        for stream in info
            .streams
            .iter()
            .filter(|stream| stream.major_type == MFMediaType_Audio)
            .take(MAX_AUDIO_STREAMS_SHOWN)
        {
            shown += 1;
            let index = stream.stream_index;
            let selected = selection == AudioStreamSelection::Stream(index);
            let candidate_index = candidate_rows.len();
            let focused = cursor == Some(candidate_index);
            let source = self.path.clone();
            candidate_rows.push(children.len());
            children.push(
                div()
                    .id(("video-audio-stream", index as usize))
                    .w_full()
                    .min_w(px(0.))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if focused {
                        if selected {
                            p.on_accent
                        } else {
                            p.accent
                        }
                    } else if selected {
                        p.accent
                    } else {
                        p.background
                    }))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(if selected { p.on_accent } else { p.foreground }))
                    .bg(rgb(if selected {
                        p.accent
                    } else if focused {
                        p.hover
                    } else {
                        p.background
                    }))
                    .hover(move |style| {
                        style.bg(rgb(if selected { p.accent_hover } else { p.hover }))
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        if view.prompt_pending
                            || !view.show_audio
                            || view.path != source
                            || view.source_generation != source_generation
                        {
                            return;
                        }
                        view.finish_scrub(None, true);
                        let Some(player) = &mut view.player else {
                            return;
                        };
                        let available = player.audio_selection().is_some()
                            && player.video_info().is_some_and(|info| {
                                info.streams.iter().any(|stream| {
                                    stream.stream_index == index
                                        && stream.major_type == MFMediaType_Audio
                                })
                            });
                        if available {
                            player.select_audio_stream(Some(index));
                            view.audio_keyboard_index = Some(candidate_index);
                            cx.notify();
                        }
                    }))
                    .child(format!(
                        "{}{}",
                        if focused { "› " } else { "" },
                        stream_description(stream)
                    ))
                    .into_any_element(),
            );
        }
        if shown == 0 {
            children.push(
                audio_note(if info.stream_probe_error.is_some() {
                    tr!(
                        "尚未取得可选音轨，不能确认原视频无音轨",
                        "No selectable tracks reported; the source may still contain audio"
                    )
                } else {
                    tr!("原视频没有音频流", "The source video has no audio streams")
                })
                .into_any_element(),
            );
        }
        Some(self.audio_menu_rows(children, &candidate_rows, cursor, cx))
    }
}

fn audio_note(text: impl Into<gpui::SharedString>) -> gpui::Div {
    let text: gpui::SharedString = text.into();
    div().py(px(3.)).text_size(px(11.)).child(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::MediaFoundation::MFMediaType_Video;

    fn key(name: &str) -> gpui::Keystroke {
        gpui::Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: name.into(),
            key_char: None,
        }
    }

    fn stream(index: u32, major_type: windows::core::GUID) -> MediaStreamInfo {
        MediaStreamInfo {
            stream_index: index,
            major_type,
            codec: None,
            sample_rate: None,
            channels: None,
            language: None,
            name: None,
        }
    }

    fn row(top: f32, height: f32) -> gpui::Bounds<gpui::Pixels> {
        gpui::Bounds::new(
            gpui::point(px(0.), px(top)),
            gpui::size(px(400.), px(height)),
        )
    }

    fn menu_layout(
        error: bool,
        busy: bool,
    ) -> (Vec<gpui::Bounds<gpui::Pixels>>, Vec<usize>, gpui::Pixels) {
        let mut heights = vec![18.];
        if error {
            heights.push(24.);
        }
        heights.push(18.);
        heights.push(44.);
        let mut candidates = vec![heights.len()];
        heights.push(32.);
        candidates.push(heights.len());
        heights.push(32.);
        if busy {
            heights.push(18.);
        }
        for _ in 0..8 {
            candidates.push(heights.len());
            heights.push(28.);
        }
        let mut top = 100.;
        let bounds = heights
            .into_iter()
            .map(|height| {
                let bounds = row(top, height);
                top += height;
                bounds
            })
            .collect();
        (bounds, candidates, px((top - 100. - 120.).max(0.)))
    }

    #[test]
    fn audio_scroll_resolves_middle_and_last_candidates_after_status_rows_change() {
        let viewport = row(100., 120.);
        let (_, busy_rows, _) = menu_layout(false, true);
        let (_, settled_rows, _) = menu_layout(false, false);
        assert_eq!(busy_rows[1], settled_rows[1]);
        for candidate in [1, 5, 9] {
            if candidate >= 2 {
                assert_ne!(busy_rows[candidate], settled_rows[candidate]);
            }
            let mut offset = px(0.);
            for (error, busy) in [
                (false, true),
                (false, false),
                (true, false),
                (false, false),
                (true, true),
            ] {
                let (bounds, rows, maximum) = menu_layout(error, busy);
                // 与根prepaint一致，先按新内容高度夹取已有偏移。
                offset = offset.clamp(-maximum, px(0.));
                let item = bounds[rows[candidate]];
                offset = audio_visible_offset(viewport, item, offset, maximum).unwrap();
                assert!(item.top() + offset >= viewport.top());
                assert!(item.bottom() + offset <= viewport.bottom());
                // 稳定布局确认无需再改偏移，不会持续请求重绘。
                assert_eq!(
                    audio_visible_offset(viewport, item, offset, maximum),
                    Some(offset)
                );
            }
        }
    }

    #[test]
    fn audio_scroll_waits_for_real_bounds_and_moves_only_as_far_as_needed() {
        let viewport = row(100., 120.);
        assert_eq!(
            audio_visible_offset(row(100., 0.), row(140., 28.), px(0.), px(100.)),
            None
        );
        assert_eq!(
            audio_visible_offset(viewport, row(140., 0.), px(0.), px(100.)),
            None
        );
        assert_eq!(
            audio_visible_offset(viewport, row(140., 28.), px(0.), px(100.)),
            Some(px(0.))
        );
        assert_eq!(
            audio_visible_offset(viewport, row(300., 28.), px(0.), px(200.)),
            Some(px(-108.))
        );
        assert_eq!(
            audio_visible_offset(viewport, row(160., 28.), px(-100.), px(200.)),
            Some(px(-60.))
        );
        assert_eq!(
            audio_visible_offset(viewport, row(300., 200.), px(0.), px(400.)),
            Some(px(-200.))
        );
    }

    #[test]
    fn audio_keys_leave_modifiers_and_global_keys_alone_and_suppress_action_repeats() {
        for modifiers in [
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
            gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
            gpui::Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            for name in ["a", "up", "down", "home", "end", "enter", "escape"] {
                let mut key = key(name);
                key.modifiers = modifiers;
                assert_eq!(audio_key(&key, true, false), None);
            }
        }
        assert_eq!(audio_key(&key("a"), false, false), Some(AudioKey::Toggle));
        assert_eq!(audio_key(&key("a"), false, true), Some(AudioKey::Repeated));
        for name in ["enter", "escape"] {
            assert_eq!(audio_key(&key(name), true, true), Some(AudioKey::Repeated));
            assert_eq!(audio_key(&key(name), false, false), None);
        }
        assert_eq!(audio_key(&key("up"), true, true), Some(AudioKey::Previous));
        assert_eq!(audio_key(&key("down"), true, true), Some(AudioKey::Next));
        for name in ["space", "left", "right", "up", "down", "home", "end"] {
            assert_eq!(audio_key(&key(name), false, false), None);
        }
        assert_eq!(audio_key(&key("space"), true, false), None);
    }

    #[test]
    fn audio_navigation_stops_at_edges_and_repairs_an_outdated_cursor() {
        for command in [
            AudioKey::Previous,
            AudioKey::Next,
            AudioKey::First,
            AudioKey::Last,
        ] {
            assert_eq!(move_audio_cursor(Some(0), 0, command), None);
        }
        assert_eq!(move_audio_cursor(None, 3, AudioKey::Next), Some(0));
        assert_eq!(move_audio_cursor(Some(0), 3, AudioKey::Previous), Some(0));
        assert_eq!(move_audio_cursor(Some(2), 3, AudioKey::Next), Some(2));
        assert_eq!(
            move_audio_cursor(Some(usize::MAX), 3, AudioKey::First),
            Some(0)
        );
        assert_eq!(move_audio_cursor(Some(0), 3, AudioKey::Last), Some(2));
        assert_eq!(
            move_audio_cursor(Some(usize::MAX), 3, AudioKey::Previous),
            Some(1)
        );
    }

    #[test]
    fn audio_candidates_keep_default_distinct_from_enumerated_streams() {
        let streams = [
            stream(0, MFMediaType_Video),
            stream(7, MFMediaType_Audio),
            stream(42, MFMediaType_Audio),
        ];
        let first = Some(AudioStreamSelection::First);
        let candidates = audio_candidates(first, Some(&streams));
        assert_eq!(
            candidates,
            [
                AudioStreamSelection::Disabled,
                AudioStreamSelection::First,
                AudioStreamSelection::Stream(7),
                AudioStreamSelection::Stream(42),
            ]
        );
        assert_eq!(audio_cursor(None, &candidates, first), Some(1));
        assert_eq!(
            audio_cursor(None, &candidates, Some(AudioStreamSelection::Disabled),),
            Some(0)
        );
        assert_eq!(
            audio_cursor(None, &candidates, Some(AudioStreamSelection::Stream(7)),),
            Some(2)
        );
        assert_eq!(audio_cursor(Some(99), &candidates, first), Some(3));
        assert!(audio_candidates(None, Some(&streams)).is_empty());
        let many: Vec<_> = (0..MAX_AUDIO_STREAMS_SHOWN as u32 + 1)
            .map(|index| stream(index, MFMediaType_Audio))
            .collect();
        let bounded = audio_candidates(first, Some(&many));
        assert_eq!(bounded.len(), MAX_AUDIO_STREAMS_SHOWN + 2);
        assert_eq!(
            bounded.last(),
            Some(&AudioStreamSelection::Stream(
                MAX_AUDIO_STREAMS_SHOWN as u32 - 1
            ))
        );
    }

    #[test]
    fn audio_default_remains_selectable_without_reported_audio_streams() {
        let video_only = [stream(0, MFMediaType_Video)];
        for streams in [None, Some(&[][..]), Some(&video_only[..])] {
            let disabled = Some(AudioStreamSelection::Disabled);
            let candidates = audio_candidates(disabled, streams);
            assert_eq!(
                candidates,
                [AudioStreamSelection::Disabled, AudioStreamSelection::First]
            );
            let current = audio_cursor(None, &candidates, disabled);
            assert_eq!(current, Some(0));
            let restored = move_audio_cursor(current, candidates.len(), AudioKey::Next).unwrap();
            assert_eq!(candidates[restored], AudioStreamSelection::First);
            // 元数据缺失时默认项仍有独立光标，同项Enter可再次提交重试。
            assert_eq!(
                audio_cursor(None, &candidates, Some(AudioStreamSelection::First)),
                Some(restored)
            );
        }
    }
}
