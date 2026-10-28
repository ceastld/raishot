//! 全局热键纯模型。位定义独立于 Win32 和 UI 框架，持久化只使用这四位与 VK。
use std::fmt;

pub const CTRL: u32 = 1;
pub const ALT: u32 = 2;
pub const SHIFT: u32 = 4;
pub const WIN: u32 = 8;
const MODIFIERS: u32 = CTRL | ALT | SHIFT | WIN;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HotkeyTarget {
    #[default]
    Capture,
    ClipboardPin,
    CurrentScreen,
    AllScreens,
    PinToggleGroup,
    PinRestore,
    PinWorkspace,
    PinFile,
    PinInteract,
    History,
    Settings,
    PinNextGroup,
    PinPreviousGroup,
}

impl HotkeyTarget {
    pub const ALL: [Self; 13] = [
        Self::Capture,
        Self::ClipboardPin,
        Self::CurrentScreen,
        Self::AllScreens,
        Self::PinToggleGroup,
        Self::PinRestore,
        Self::PinWorkspace,
        Self::PinFile,
        Self::PinInteract,
        Self::History,
        Self::Settings,
        Self::PinNextGroup,
        Self::PinPreviousGroup,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Capture => "capture.region",
            Self::ClipboardPin => "pin.clipboard",
            Self::CurrentScreen => "capture.current_screen",
            Self::AllScreens => "capture.all_screens",
            Self::PinToggleGroup => "pin.group.toggle_visibility",
            Self::PinRestore => "pin.restore",
            Self::PinWorkspace => "pin.workspace",
            Self::PinFile => "pin.file",
            Self::PinInteract => "pin.restore_interaction",
            Self::History => "app.gallery",
            Self::Settings => "app.settings",
            Self::PinNextGroup => "pin.group.next",
            Self::PinPreviousGroup => "pin.group.previous",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Capture => tr!("截图", "Capture"),
            Self::ClipboardPin => tr!("剪贴板贴图", "Clipboard pin"),
            Self::CurrentScreen => tr!("当前屏幕截图", "Capture current screen"),
            Self::AllScreens => tr!("所有屏幕截图", "Capture all screens"),
            Self::PinToggleGroup => tr!("显示／隐藏当前分组贴图", "Show / hide current group"),
            Self::PinRestore => tr!("恢复最近关闭的贴图", "Restore last closed pin"),
            Self::PinWorkspace => tr!("管理贴图", "Manage pins"),
            Self::PinFile => tr!("打开图片贴图", "Pin image file"),
            Self::PinInteract => tr!("恢复贴图交互", "Restore pin interaction"),
            Self::History => tr!("打开图库", "Open gallery"),
            Self::Settings => tr!("打开设置", "Open settings"),
            Self::PinNextGroup => tr!("下一个贴图分组", "Next pin group"),
            Self::PinPreviousGroup => tr!("上一个贴图分组", "Previous pin group"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Capture => tr!("开始框选截图。", "Start a region capture."),
            Self::ClipboardPin => tr!(
                "把剪贴板图片或纯文本贴到屏幕上；两者都有时用图片。",
                "Pin the clipboard image or text; an image takes priority."
            ),
            Self::CurrentScreen => tr!(
                "截取鼠标所在屏幕并进入编辑。",
                "Capture the screen under the pointer and edit it."
            ),
            Self::AllScreens => tr!(
                "截取全部屏幕并进入编辑。",
                "Capture all screens and edit them."
            ),
            Self::PinToggleGroup => tr!(
                "本组有贴图显示时全部隐藏，否则全部显示；不恢复已关闭贴图。",
                "Hide open pins in this group, or show them when all are hidden."
            ),
            Self::PinRestore => tr!(
                "恢复最近关闭的贴图。",
                "Reopen the most recently closed pin."
            ),
            Self::PinWorkspace => tr!("打开贴图管理页面。", "Open the pin management page."),
            Self::PinFile => tr!(
                "选择一张本地图片并贴到屏幕上。",
                "Choose a local image to pin on screen."
            ),
            Self::PinInteract => tr!(
                "显示贴图并关闭鼠标穿透，恢复鼠标操作。",
                "Show pins and turn off click-through to restore mouse control."
            ),
            Self::History => tr!(
                "打开图库，浏览已有截图和媒体。",
                "Open the gallery to browse captures and media."
            ),
            Self::Settings => tr!("打开应用设置。", "Open application settings."),
            Self::PinNextGroup => tr!(
                "按分组顺序循环切换到下一组。",
                "Cycle to the next pin group."
            ),
            Self::PinPreviousGroup => tr!(
                "按分组顺序循环切换到上一组。",
                "Cycle to the previous pin group."
            ),
        }
    }

    pub fn category(self) -> &'static str {
        match self {
            Self::Capture | Self::CurrentScreen | Self::AllScreens => tr!("截图", "Capture"),
            Self::History | Self::Settings => tr!("应用", "Application"),
            _ => tr!("贴图与分组", "Pins and groups"),
        }
    }

    pub fn default_binding(self) -> Option<HotkeySpec> {
        let key = match self {
            Self::Capture => b'1',
            Self::ClipboardPin => b'2',
            _ => return None,
        };
        Some(HotkeySpec {
            modifiers: CTRL,
            key: u32::from(key),
        })
    }

    /// 仅供已有的两个默认动作及旧配置兼容调用；其它动作使用 default_binding。
    pub fn default_spec(self) -> HotkeySpec {
        self.default_binding().expect("该操作默认未绑定快捷键")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeySpec {
    pub modifiers: u32,
    pub key: u32,
}

impl Default for HotkeySpec {
    fn default() -> Self {
        HotkeyTarget::Capture.default_spec()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyError {
    UnknownModifiers,
    ModifierOnly,
    UnknownKey,
    UnmodifiedPrintable,
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownModifiers => tr!("热键包含不支持的修饰键", "The hotkey contains an unsupported modifier"),
            Self::ModifierOnly => tr!("请再按一个主键，不能只使用修饰键", "Press a main key too; modifiers alone are not allowed"),
            Self::UnknownKey => tr!("不支持这个按键，请使用字母、数字、功能键或常见导航键", "This key is not supported; use a letter, digit, function key or common navigation key"),
            Self::UnmodifiedPrintable => tr!("字母、数字和标点热键需要搭配 Ctrl、Alt、Shift 或 Win", "Letter, digit and punctuation hotkeys need Ctrl, Alt, Shift or Win"),
        })
    }
}

impl std::error::Error for HotkeyError {}

impl HotkeySpec {
    pub fn validate(&self) -> std::result::Result<(), HotkeyError> {
        if self.modifiers & !MODIFIERS != 0 {
            return Err(HotkeyError::UnknownModifiers);
        }
        if matches!(self.key, 0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5) {
            return Err(HotkeyError::ModifierOnly);
        }
        if !supported_key(self.key) {
            return Err(HotkeyError::UnknownKey);
        }
        if self.modifiers == 0 && printable_key(self.key) {
            return Err(HotkeyError::UnmodifiedPrintable);
        }
        Ok(())
    }

    /// 接受 UI 常见按键名。Shift 后的数字/标点符号会补回被 UI 归一化掉的 Shift。
    pub fn from_key_name(name: &str, modifier_bits: u32) -> std::result::Result<Self, HotkeyError> {
        let name = if name == " " { "space" } else { name.trim() };
        let name = name.to_ascii_lowercase();
        let (key, shifted) = if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() {
            (u32::from(name.as_bytes()[0].to_ascii_uppercase()), false)
        } else if let Some(number) = name.strip_prefix('f').and_then(|s| s.parse::<u32>().ok()) {
            if !(1..=24).contains(&number) {
                return Err(HotkeyError::UnknownKey);
            }
            (0x6f + number, false)
        } else {
            match name.as_str() {
                "ctrl" | "control" | "lctrl" | "rctrl" | "leftcontrol" | "rightcontrol" | "alt"
                | "lalt" | "ralt" | "option" | "shift" | "lshift" | "rshift" | "win"
                | "windows" | "lwin" | "rwin" | "super" | "meta" | "cmd" | "command" => {
                    return Err(HotkeyError::ModifierOnly)
                }
                "backspace" | "back-space" => (0x08, false),
                "tab" => (0x09, false),
                "enter" | "return" => (0x0d, false),
                "pause" | "break" => (0x13, false),
                "capslock" | "caps-lock" => (0x14, false),
                "escape" | "esc" => (0x1b, false),
                "space" | "spacebar" => (0x20, false),
                "pageup" | "page-up" | "pgup" | "prior" => (0x21, false),
                "pagedown" | "page-down" | "pgdn" | "next" => (0x22, false),
                "end" => (0x23, false),
                "home" => (0x24, false),
                "left" | "arrowleft" | "arrow-left" => (0x25, false),
                "up" | "arrowup" | "arrow-up" => (0x26, false),
                "right" | "arrowright" | "arrow-right" => (0x27, false),
                "down" | "arrowdown" | "arrow-down" => (0x28, false),
                "printscreen" | "print-screen" | "prtsc" | "prtscr" | "snapshot" => (0x2c, false),
                "insert" | "ins" => (0x2d, false),
                "delete" | "del" => (0x2e, false),
                "menu" | "apps" | "contextmenu" => (0x5d, false),
                "numlock" | "num-lock" => (0x90, false),
                "scrolllock" | "scroll-lock" => (0x91, false),
                "back" | "browserback" => (0xa6, false),
                "forward" | "browserforward" => (0xa7, false),
                "semicolon" | "oem1" | ";" => (0xba, false),
                ":" => (0xba, true),
                "equal" | "equals" | "oemplus" | "=" => (0xbb, false),
                "plus" | "+" => (0xbb, true),
                "comma" | "oemcomma" | "," => (0xbc, false),
                "<" => (0xbc, true),
                "minus" | "hyphen" | "oemminus" | "-" => (0xbd, false),
                "_" => (0xbd, true),
                "period" | "dot" | "oemperiod" | "." => (0xbe, false),
                ">" => (0xbe, true),
                "slash" | "oem2" | "/" => (0xbf, false),
                "?" => (0xbf, true),
                "backtick" | "grave" | "oem3" | "`" => (0xc0, false),
                "~" => (0xc0, true),
                "bracketleft" | "leftbracket" | "oem4" | "[" => (0xdb, false),
                "{" => (0xdb, true),
                "backslash" | "oem5" | "\\" => (0xdc, false),
                "|" => (0xdc, true),
                "bracketright" | "rightbracket" | "oem6" | "]" => (0xdd, false),
                "}" => (0xdd, true),
                "quote" | "apostrophe" | "oem7" | "'" => (0xde, false),
                "\"" => (0xde, true),
                "oem8" => (0xdf, false),
                "oem102" | "intlbackslash" => (0xe2, false),
                "!" => (u32::from(b'1'), true),
                "@" => (u32::from(b'2'), true),
                "#" => (u32::from(b'3'), true),
                "$" => (u32::from(b'4'), true),
                "%" => (u32::from(b'5'), true),
                "^" => (u32::from(b'6'), true),
                "&" => (u32::from(b'7'), true),
                "*" => (u32::from(b'8'), true),
                "(" => (u32::from(b'9'), true),
                ")" => (u32::from(b'0'), true),
                _ => return Err(HotkeyError::UnknownKey),
            }
        };
        let spec = Self {
            modifiers: modifier_bits | if shifted { SHIFT } else { 0 },
            key,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn label(&self) -> String {
        self.parts().join("+")
    }

    pub fn parts(&self) -> Vec<String> {
        let mut parts = Vec::with_capacity(5);
        for (bit, name) in [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
            if self.modifiers & bit != 0 {
                parts.push(name.to_string());
            }
        }
        parts.push(key_label(self.key));
        parts
    }
}

fn supported_key(key: u32) -> bool {
    matches!(key,
        0x08 | 0x09 | 0x0d | 0x13 | 0x14 | 0x1b | 0x20..=0x28 |
        0x2c..=0x2e | 0x30..=0x39 | 0x41..=0x5a | 0x5d |
        0x70..=0x87 | 0x90 | 0x91 | 0xa6 | 0xa7 | 0xba..=0xc0 | 0xdb..=0xdf | 0xe2)
}

fn printable_key(key: u32) -> bool {
    matches!(key, 0x20 | 0x30..=0x39 | 0x41..=0x5a | 0xba..=0xc0 | 0xdb..=0xdf | 0xe2)
}

pub(crate) fn key_label(key: u32) -> String {
    if matches!(key, 0x30..=0x39 | 0x41..=0x5a) {
        return char::from_u32(key).unwrap_or('?').to_string();
    }
    if (0x70..=0x87).contains(&key) {
        return format!("F{}", key - 0x6f);
    }
    let name = match key {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0d => "Enter",
        0x13 => "Pause",
        0x14 => "CapsLock",
        0x1b => "Esc",
        0x20 => "Space",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2c => "PrintScreen",
        0x2d => "Insert",
        0x2e => "Delete",
        0x5d => "Menu",
        0x90 => "NumLock",
        0x91 => "ScrollLock",
        0xa6 => "BrowserBack",
        0xa7 => "BrowserForward",
        0xba => ";",
        0xbb => "=",
        0xbc => ",",
        0xbd => "-",
        0xbe => ".",
        0xbf => "/",
        0xc0 => "`",
        0xdb => "[",
        0xdc => "\\",
        0xdd => "]",
        0xde => "'",
        0xdf => "OEM8",
        0xe2 => "OEM102",
        _ => return format!("VK 0x{key:02X}"),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_shifted_symbols_preserve_the_actual_combination() {
        let question = HotkeySpec::from_key_name("?", CTRL).unwrap();
        assert_eq!(question.modifiers, CTRL | SHIFT);
        assert_eq!(question.key, 0xbf);
        assert_eq!(question.label(), "Ctrl+Shift+/");
        let digit = HotkeySpec::from_key_name("!", ALT).unwrap();
        assert_eq!(digit.key, u32::from(b'1'));
        assert_eq!(digit.label(), "Alt+Shift+1");
        assert_eq!(
            HotkeySpec::from_key_name("S", CTRL | ALT | SHIFT).unwrap(),
            HotkeySpec {
                modifiers: CTRL | ALT | SHIFT,
                key: u32::from(b'S')
            }
        );
    }

    #[test]
    fn default_action_bindings_are_distinct_valid_control_digits() {
        let capture = HotkeyTarget::Capture.default_spec();
        let pin = HotkeyTarget::ClipboardPin.default_spec();
        assert_eq!(HotkeySpec::default(), capture);
        assert_eq!(capture, HotkeySpec::from_key_name("1", CTRL).unwrap());
        assert_eq!(pin, HotkeySpec::from_key_name("2", CTRL).unwrap());
        assert_eq!(capture.label(), "Ctrl+1");
        assert_eq!(pin.label(), "Ctrl+2");
        assert_ne!(capture, pin);
    }

    #[test]
    fn command_catalog_has_stable_unique_ids_and_only_two_default_bindings() {
        let mut ids = std::collections::HashSet::new();
        for target in HotkeyTarget::ALL {
            assert!(ids.insert(target.id()));
            assert!(!target.label().is_empty());
            assert!(!target.description().is_empty());
            assert!(!target.category().is_empty());
            assert_eq!(
                target.default_binding().is_some(),
                matches!(target, HotkeyTarget::Capture | HotkeyTarget::ClipboardPin)
            );
        }
        assert_eq!(
            HotkeyTarget::PinToggleGroup.id(),
            "pin.group.toggle_visibility"
        );
    }

    #[test]
    fn unsafe_or_unrepresentable_bindings_are_rejected() {
        for name in ["s", "7", "/", "space"] {
            assert_eq!(
                HotkeySpec::from_key_name(name, 0),
                Err(HotkeyError::UnmodifiedPrintable)
            );
        }
        for name in ["ctrl", "shift", "win", "alt"] {
            assert_eq!(
                HotkeySpec::from_key_name(name, CTRL),
                Err(HotkeyError::ModifierOnly)
            );
        }
        for key in [0x10, 0x5c, 0xa1, 0xa5] {
            assert_eq!(
                HotkeySpec {
                    modifiers: CTRL,
                    key
                }
                .validate(),
                Err(HotkeyError::ModifierOnly)
            );
        }
        assert_eq!(
            HotkeySpec::from_key_name("a", 0x10 | CTRL),
            Err(HotkeyError::UnknownModifiers)
        );
        for name in ["f0", "f25", "unknown", "截图"] {
            assert_eq!(
                HotkeySpec::from_key_name(name, CTRL),
                Err(HotkeyError::UnknownKey)
            );
        }
    }

    #[test]
    fn functions_navigation_and_print_screen_have_stable_vk_labels() {
        for number in 1..=24 {
            let spec = HotkeySpec::from_key_name(&format!("f{number}"), 0).unwrap();
            assert_eq!(spec.key, 0x6f + number);
            assert_eq!(spec.label(), format!("F{number}"));
        }
        for (name, key, label) in [
            ("printscreen", 0x2c, "PrintScreen"),
            ("pageup", 0x21, "PageUp"),
            ("arrowleft", 0x25, "Left"),
            ("menu", 0x5d, "Menu"),
            ("back", 0xa6, "BrowserBack"),
        ] {
            let spec = HotkeySpec::from_key_name(name, 0).unwrap();
            assert_eq!(spec.key, key);
            assert_eq!(spec.label(), label);
        }
        assert_eq!(
            HotkeySpec::from_key_name("oem102", WIN).unwrap().label(),
            "Win+OEM102"
        );
    }
}
