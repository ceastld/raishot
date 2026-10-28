use std::time::Duration;

/// 视频查看窗口消费的命令；文件对话框及其它输入控件不转发按键到此处。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoShortcut {
    TogglePlayback,
    StepForward,
    SeekSeconds(i32),
    AdjustVolume(i32),
    ToggleMuted,
    ToggleFullscreen,
    ExitFullscreen,
    OpenFile,
    Close,
    ToStart,
    ToEnd,
    CopyFrame,
    SaveFrame,
    EditFrame,
    PinFrame,
    PreviousFile,
    NextFile,
    LoadSubtitles,
    SubtitleOffsetMillis(i32),
}

pub(crate) fn resolve(key: &gpui::Keystroke) -> Option<VideoShortcut> {
    let modifiers = key.modifiers;
    let bare = !modifiers.control
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function;
    let control_only = modifiers.control
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function;
    match key.key.as_str() {
        "o" if control_only => Some(VideoShortcut::OpenFile),
        "w" if control_only => Some(VideoShortcut::Close),
        "c" if control_only => Some(VideoShortcut::CopyFrame),
        "s" if control_only => Some(VideoShortcut::SaveFrame),
        "e" if bare => Some(VideoShortcut::EditFrame),
        "p" if bare => Some(VideoShortcut::PinFrame),
        "l" if bare => Some(VideoShortcut::LoadSubtitles),
        "g" if bare => Some(VideoShortcut::SubtitleOffsetMillis(-500)),
        "h" if bare => Some(VideoShortcut::SubtitleOffsetMillis(500)),
        "pageup" if bare => Some(VideoShortcut::PreviousFile),
        "pagedown" if bare => Some(VideoShortcut::NextFile),
        "space" if bare => Some(VideoShortcut::TogglePlayback),
        "n" if bare => Some(VideoShortcut::StepForward),
        "left" if bare => Some(VideoShortcut::SeekSeconds(-5)),
        "right" if bare => Some(VideoShortcut::SeekSeconds(5)),
        "up" if bare => Some(VideoShortcut::AdjustVolume(5)),
        "down" if bare => Some(VideoShortcut::AdjustVolume(-5)),
        "m" if bare => Some(VideoShortcut::ToggleMuted),
        "f" | "f11" if bare => Some(VideoShortcut::ToggleFullscreen),
        "escape" if bare => Some(VideoShortcut::ExitFullscreen),
        "home" if bare => Some(VideoShortcut::ToStart),
        "end" if bare => Some(VideoShortcut::ToEnd),
        _ => None,
    }
}

/// 内嵌预览只消费播放控制；Esc供结束拖动，不授予宿主窗口或文件操作权限。
pub(crate) fn resolve_embedded(key: &gpui::Keystroke) -> Option<VideoShortcut> {
    resolve(key).filter(|command| {
        matches!(
            command,
            VideoShortcut::TogglePlayback
                | VideoShortcut::StepForward
                | VideoShortcut::SeekSeconds(_)
                | VideoShortcut::ToStart
                | VideoShortcut::ToEnd
                | VideoShortcut::AdjustVolume(_)
                | VideoShortcut::ToggleMuted
                | VideoShortcut::ExitFullscreen
        )
    })
}

/// 无已知时长时不发送跳转；边界跳转保持在可用时长范围内。
pub(crate) fn seek_ratio(
    position: Duration,
    duration: Duration,
    delta_seconds: i32,
) -> Option<f32> {
    if duration.is_zero() {
        return None;
    }
    let target =
        (position.as_secs_f64() + f64::from(delta_seconds)).clamp(0., duration.as_secs_f64());
    Some((target / duration.as_secs_f64()) as f32)
}

pub(crate) fn format_time(time: Duration) -> String {
    let seconds = time.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> gpui::Keystroke {
        gpui::Keystroke {
            key: name.into(),
            key_char: None,
            modifiers: Default::default(),
        }
    }

    #[test]
    fn modified_keys_do_not_seek_or_toggle() {
        for name in [
            "space", "left", "right", "up", "down", "m", "f", "e", "p", "n", "g", "h",
        ] {
            for modifier in 0..5 {
                let mut chord = key(name);
                match modifier {
                    0 => chord.modifiers.control = true,
                    1 => chord.modifiers.alt = true,
                    2 => chord.modifiers.shift = true,
                    3 => chord.modifiers.platform = true,
                    _ => chord.modifiers.function = true,
                }
                assert_eq!(resolve(&chord), None);
            }
        }
        let mut open = key("o");
        open.modifiers.control = true;
        assert_eq!(resolve(&open), Some(VideoShortcut::OpenFile));
        open.modifiers.alt = true;
        assert_eq!(resolve(&open), None);
        for (name, command) in [
            ("c", VideoShortcut::CopyFrame),
            ("s", VideoShortcut::SaveFrame),
        ] {
            let mut chord = key(name);
            chord.modifiers.control = true;
            assert_eq!(resolve(&chord), Some(command));
            chord.modifiers.alt = true;
            assert_eq!(resolve(&chord), None);
        }
    }

    #[test]
    fn bare_n_steps_forward() {
        assert_eq!(resolve(&key("n")), Some(VideoShortcut::StepForward));
    }

    #[test]
    fn embedded_controls_require_bare_keys() {
        for (name, command) in [
            ("space", VideoShortcut::TogglePlayback),
            ("n", VideoShortcut::StepForward),
            ("left", VideoShortcut::SeekSeconds(-5)),
            ("right", VideoShortcut::SeekSeconds(5)),
            ("home", VideoShortcut::ToStart),
            ("end", VideoShortcut::ToEnd),
            ("up", VideoShortcut::AdjustVolume(5)),
            ("down", VideoShortcut::AdjustVolume(-5)),
            ("m", VideoShortcut::ToggleMuted),
            ("escape", VideoShortcut::ExitFullscreen),
        ] {
            assert_eq!(resolve_embedded(&key(name)), Some(command));
            for modifier in 0..5 {
                let mut chord = key(name);
                match modifier {
                    0 => chord.modifiers.control = true,
                    1 => chord.modifiers.alt = true,
                    2 => chord.modifiers.shift = true,
                    3 => chord.modifiers.platform = true,
                    _ => chord.modifiers.function = true,
                }
                assert_eq!(resolve_embedded(&chord), None);
            }
        }
    }

    #[test]
    fn embedded_preview_rejects_window_file_and_subtitle_commands() {
        for name in ["f", "f11", "pageup", "pagedown", "e", "p", "l", "g", "h"] {
            assert!(resolve(&key(name)).is_some());
            assert_eq!(resolve_embedded(&key(name)), None);
        }
        for name in ["w", "o", "c", "s"] {
            let mut chord = key(name);
            chord.modifiers.control = true;
            assert!(resolve(&chord).is_some());
            assert_eq!(resolve_embedded(&chord), None);
        }
    }

    #[test]
    fn bare_g_and_h_adjust_subtitle_offset_by_half_a_second() {
        assert_eq!(
            resolve(&key("g")),
            Some(VideoShortcut::SubtitleOffsetMillis(-500))
        );
        assert_eq!(
            resolve(&key("h")),
            Some(VideoShortcut::SubtitleOffsetMillis(500))
        );
    }

    #[test]
    fn seek_clamps_and_rejects_unknown_duration() {
        let seconds = Duration::from_secs;
        assert_eq!(seek_ratio(seconds(3), Duration::ZERO, 5), None);
        assert_eq!(seek_ratio(seconds(3), seconds(10), -5), Some(0.));
        assert_eq!(seek_ratio(seconds(8), seconds(10), 5), Some(1.));
        assert_eq!(seek_ratio(seconds(8), seconds(10), -5), Some(0.3));
        assert_eq!(seek_ratio(seconds(20), seconds(10), -5), Some(1.));
    }

    #[test]
    fn time_display_keeps_hour_and_minute_boundaries() {
        assert_eq!(format_time(Duration::ZERO), "0:00");
        assert_eq!(format_time(Duration::from_secs(3599)), "59:59");
        assert_eq!(format_time(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_time(Duration::from_secs(360_001)), "100:00:01");
    }
}
