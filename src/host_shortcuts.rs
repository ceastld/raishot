//! 原生输入和会话子层让行之后的宿主快捷键；修饰键不能重新解释为裸键。
use crate::{annotation::Tool, pin_view::PinAction};
use gpui::Modifiers;

pub(crate) fn control(m: Modifiers) -> bool {
    m.control && !m.alt && !m.platform && !m.function
}

pub(crate) fn shift(m: Modifiers) -> bool {
    m.shift && !m.control && !m.alt && !m.platform && !m.function
}

pub(crate) fn alt(m: Modifiers) -> bool {
    m.alt && !m.shift && !m.control && !m.platform && !m.function
}

pub(crate) fn drawing_tool(key: &str, m: Modifiers, screenshot: bool) -> Option<Tool> {
    if key == "e" && shift(m) {
        return Some(Tool::Eraser);
    }
    if m.modified() {
        return None;
    }
    Some(match key {
        "v" => Tool::Edit,
        "m" => Tool::Mosaic,
        "h" => Tool::Highlighter,
        "n" => Tool::Serial,
        "x" => Tool::Text,
        "b" => Tool::Blur,
        "p" => Tool::Polyline,
        "e" => Tool::PixelEraser,
        "o" if screenshot => Tool::Lasso,
        "s" if screenshot => Tool::Spotlight,
        _ => return None,
    })
}

pub(crate) fn pin_action(
    key: &str,
    m: Modifiers,
    chrome: bool,
    text_draft: bool,
) -> Option<PinAction> {
    if control(m) {
        return Some(match key {
            "c" if m.shift => PinAction::CopyText,
            "c" => PinAction::Copy,
            "s" if m.shift => PinAction::QuickSave,
            "s" => PinAction::Save,
            "z" if m.shift => PinAction::Redo,
            "z" => PinAction::Undo,
            "y" if !m.shift => PinAction::Redo,
            "w" if !m.shift && !text_draft => PinAction::Close,
            "d" if !m.shift => PinAction::Duplicate,
            "t" if !m.shift => PinAction::ClickThrough,
            "0" if !m.shift => PinAction::Original,
            _ => return None,
        });
    }
    if m.modified() {
        return None;
    }
    Some(match key {
        "escape" => PinAction::Close,
        "enter" if chrome => PinAction::Copy,
        "y" if !text_draft => PinAction::Shadow,
        "h" if !text_draft => PinAction::Annotations,
        "space" => PinAction::Chrome,
        "l" => PinAction::Lock,
        "t" => PinAction::Topmost,
        "tab" => PinAction::Thumbnail,
        "f" => PinAction::Fit,
        "e" => PinAction::Text,
        "o" => PinAction::Ocr,
        "1" => PinAction::Rotate(1),
        "2" => PinAction::Rotate(-1),
        "3" => PinAction::FlipX,
        "4" => PinAction::FlipY,
        "5" => PinAction::Gray,
        "6" => PinAction::Invert,
        "7" => PinAction::Brightness(10),
        "8" => PinAction::Brightness(-10),
        "0" => PinAction::ResetImage,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(bits: u8) -> Modifiers {
        Modifiers {
            shift: bits & 1 != 0,
            control: bits & 2 != 0,
            alt: bits & 4 != 0,
            platform: bits & 8 != 0,
            function: bits & 16 != 0,
        }
    }

    #[test]
    fn real_drawing_tool_routes_keep_shift_eraser_and_reject_modified_letters() {
        for screenshot in [false, true] {
            for (key, tool) in [
                ("v", Tool::Edit),
                ("m", Tool::Mosaic),
                ("h", Tool::Highlighter),
                ("n", Tool::Serial),
                ("x", Tool::Text),
                ("b", Tool::Blur),
                ("p", Tool::Polyline),
                ("e", Tool::PixelEraser),
                ("o", Tool::Lasso),
                ("s", Tool::Spotlight),
            ] {
                let available = screenshot || !matches!(key, "o" | "s");
                assert_eq!(
                    drawing_tool(key, modifiers(0), screenshot),
                    available.then_some(tool)
                );
                for bits in 1..32 {
                    let expected = (key == "e" && bits == 1).then_some(Tool::Eraser);
                    assert_eq!(
                        drawing_tool(key, modifiers(bits), screenshot),
                        expected,
                        "{key} {bits}"
                    );
                }
            }
        }
    }

    #[test]
    fn real_pin_host_routes_keep_plain_commands_and_explicit_control_variants() {
        for (key, expected) in [
            ("l", PinAction::Lock),
            ("t", PinAction::Topmost),
            ("tab", PinAction::Thumbnail),
            ("f", PinAction::Fit),
            ("e", PinAction::Text),
            ("o", PinAction::Ocr),
            ("1", PinAction::Rotate(1)),
            ("2", PinAction::Rotate(-1)),
            ("3", PinAction::FlipX),
            ("4", PinAction::FlipY),
            ("5", PinAction::Gray),
            ("6", PinAction::Invert),
            ("7", PinAction::Brightness(10)),
            ("8", PinAction::Brightness(-10)),
            ("0", PinAction::ResetImage),
            ("space", PinAction::Chrome),
        ] {
            assert_eq!(
                format!("{:?}", pin_action(key, modifiers(0), false, false)),
                format!("{:?}", Some(expected))
            );
            for bits in 1..32 {
                let action = pin_action(key, modifiers(bits), false, false);
                match (key, bits) {
                    ("t", 2) => assert!(matches!(action, Some(PinAction::ClickThrough))),
                    ("0", 2) => assert!(matches!(action, Some(PinAction::Original))),
                    _ => assert!(action.is_none(), "{key} {bits}: {action:?}"),
                }
            }
        }
        for (key, bits, expected) in [
            ("c", 2, PinAction::Copy),
            ("c", 3, PinAction::CopyText),
            ("s", 2, PinAction::Save),
            ("s", 3, PinAction::QuickSave),
            ("z", 2, PinAction::Undo),
            ("z", 3, PinAction::Redo),
            ("y", 2, PinAction::Redo),
            ("d", 2, PinAction::Duplicate),
            ("w", 2, PinAction::Close),
        ] {
            assert_eq!(
                format!("{:?}", pin_action(key, modifiers(bits), false, false)),
                format!("{:?}", Some(expected))
            );
        }
        // AltGr、Windows/Fn及未定义的额外修饰不能借 Ctrl 分支输出或改历史。
        for key in ["c", "s", "z", "y", "d", "w"] {
            for bits in 4..32 {
                assert!(
                    pin_action(key, modifiers(bits), true, false).is_none(),
                    "{key} {bits}"
                );
            }
        }
    }

    #[test]
    fn pin_host_routes_preserve_native_text_and_collapsed_enter_boundaries() {
        assert!(matches!(
            pin_action("w", modifiers(2), false, false),
            Some(PinAction::Close)
        ));
        assert!(crate::pin_keys::allow_held_shortcut(
            PinAction::Close,
            false
        ));
        assert!(!crate::pin_keys::allow_held_shortcut(
            PinAction::Close,
            true
        ));
        assert!(pin_action("enter", modifiers(0), false, false).is_none());
        assert!(matches!(
            pin_action("enter", modifiers(0), true, false),
            Some(PinAction::Copy)
        ));
        for bits in 1..32 {
            assert!(pin_action("enter", modifiers(bits), true, false).is_none());
        }
        for (key, bits) in [("w", 2), ("y", 0), ("h", 0)] {
            assert!(pin_action(key, modifiers(bits), true, true).is_none());
        }
        assert!(matches!(
            pin_action("y", modifiers(2), true, true),
            Some(PinAction::Redo)
        ));
        assert!(matches!(
            pin_action("s", modifiers(2), true, true),
            Some(PinAction::Save)
        ));
    }
}
