//! 全局触发器：在系统热键之外支持任意键盘／鼠标组合（按住若干键 + 按下一键或滚轮方向）。
//! 只用 Ctrl/Alt/Shift/Win 作前缀的普通组合仍交给系统热键注册；其余组合由常驻进程的
//! 低级输入监听匹配。录制、持久化、冲突检查与运行时匹配共用同一份描述。
use crate::hotkey::{HotkeyError, HotkeySpec, ALT, CTRL, SHIFT, WIN};
pub(crate) use omnitrigger_core::{Activation, Button, MouseButton, WheelDirection};
use omnitrigger_core::{Engine, RegistrationError, TriggerSpec};
use std::fmt;

/// 系统热键表达不了的组合，例如 `CapsLock + W`、`鼠标侧键 + 滚轮`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputTrigger {
    /// 已排序、去重的前缀。
    pub held: Vec<Button>,
    pub activation: Activation,
}

/// 一次录制或配置得到的触发方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    Hotkey(HotkeySpec),
    Input(InputTrigger),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerError {
    Hotkey(HotkeyError),
    PrimaryClickAlone,
    LeftButtonPrefix,
    BareWheel,
    TooManyHeld,
    Unsupported,
}

impl fmt::Display for TriggerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hotkey(error) => error.fmt(f),
            Self::PrimaryClickAlone => f.write_str(tr!(
                "鼠标左键或右键不能单独使用，请与其它键组合",
                "Left or right click can't be used alone; combine it with another key"
            )),
            Self::LeftButtonPrefix => f.write_str(tr!(
                "鼠标左键不能作为按住的前缀，否则点击和拖动会受影响",
                "The left mouse button can't be a held prefix; it would interfere with clicks and drags"
            )),
            Self::BareWheel => f.write_str(tr!(
                "滚轮需要搭配按住的键或鼠标按键，否则会拦截所有滚动",
                "The wheel needs a held key or mouse button; otherwise every scroll would be captured"
            )),
            Self::TooManyHeld => f.write_str(tr!(
                "最多同时按住 8 个前缀键",
                "At most 8 keys can be held as a prefix"
            )),
            Self::Unsupported => f.write_str(tr!(
                "组合包含不支持的按键，请重新录制",
                "The combination contains an unsupported key; record it again"
            )),
        }
    }
}

impl std::error::Error for TriggerError {}

fn modifier_bit(button: Button) -> Option<u32> {
    match button {
        Button::Key(0x10 | 0xa0 | 0xa1) => Some(SHIFT),
        Button::Key(0x11 | 0xa2 | 0xa3) => Some(CTRL),
        Button::Key(0x12 | 0xa4 | 0xa5) => Some(ALT),
        Button::Key(0x5b | 0x5c) => Some(WIN),
        _ => None,
    }
}

impl Trigger {
    /// 把录制结果归到最合适的后端：系统热键能表达的组合不安装输入监听。
    pub fn from_recorded(held: &[Button], activation: Activation) -> Result<Self, TriggerError> {
        if let Activation::Press(Button::Key(key)) = activation {
            if modifier_bit(Button::Key(key)).is_some() && held.is_empty() {
                return Err(TriggerError::Hotkey(HotkeyError::ModifierOnly));
            }
            let modifiers = held
                .iter()
                .map(|button| modifier_bit(*button))
                .try_fold(0, |bits, bit| bit.map(|bit| bits | bit));
            if let Some(modifiers) = modifiers.filter(|_| modifier_bit(Button::Key(key)).is_none())
            {
                let spec = HotkeySpec {
                    modifiers,
                    key: u32::from(key),
                };
                match spec.validate() {
                    Ok(()) => return Ok(Self::Hotkey(spec)),
                    // 普通键必须带修饰键，低级监听也不能把裸字母当全局触发器。
                    Err(HotkeyError::UnmodifiedPrintable) => {
                        return Err(TriggerError::Hotkey(HotkeyError::UnmodifiedPrintable))
                    }
                    // 系统热键表里没有的键（媒体键、小键盘等）交给输入监听。
                    Err(_) => {}
                }
            }
        }
        InputTrigger::new(held.to_vec(), activation).map(Self::Input)
    }

    pub fn parts(&self) -> Vec<String> {
        match self {
            Self::Hotkey(spec) => spec.parts(),
            Self::Input(trigger) => trigger.parts(),
        }
    }

    pub fn label(&self) -> String {
        self.parts().join("+")
    }
}

impl InputTrigger {
    pub fn new(mut held: Vec<Button>, activation: Activation) -> Result<Self, TriggerError> {
        held.sort_unstable();
        held.dedup();
        if held.contains(&Button::Mouse(MouseButton::Left)) {
            return Err(TriggerError::LeftButtonPrefix);
        }
        match activation {
            Activation::Press(Button::Mouse(MouseButton::Left | MouseButton::Right))
                if held.is_empty() =>
            {
                return Err(TriggerError::PrimaryClickAlone)
            }
            Activation::Wheel(_) if held.is_empty() => return Err(TriggerError::BareWheel),
            _ => {}
        }
        let trigger = Self { held, activation };
        match Engine::new(vec![trigger.spec(1)]) {
            Ok(_) => Ok(trigger),
            Err(RegistrationError::TooManyHeld { .. }) => Err(TriggerError::TooManyHeld),
            Err(_) => Err(TriggerError::Unsupported),
        }
    }

    pub(crate) fn spec(&self, id: u64) -> TriggerSpec {
        TriggerSpec {
            id,
            held: self.held.clone(),
            activation: self.activation,
        }
    }

    pub fn parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = self.held.iter().map(|b| button_label(*b)).collect();
        parts.push(match self.activation {
            Activation::Press(button) => button_label(button),
            Activation::Wheel(direction) => wheel_label(direction).to_string(),
        });
        parts
    }

    pub fn label(&self) -> String {
        self.parts().join("+")
    }

    /// 持久化线格式：`key:14 + key:57`、`mouse:x1 + wheel:up`；最后一项是触发键。
    pub fn to_wire(&self) -> String {
        let mut tokens: Vec<String> = self.held.iter().map(|b| button_wire(*b)).collect();
        tokens.push(match self.activation {
            Activation::Press(button) => button_wire(button),
            Activation::Wheel(direction) => format!("wheel:{}", wheel_wire(direction)),
        });
        tokens.join(" + ")
    }

    pub fn from_wire(text: &str) -> Option<Self> {
        let mut tokens: Vec<&str> = text.split('+').map(str::trim).collect();
        let last = tokens.pop()?;
        let activation = match last.strip_prefix("wheel:") {
            Some(direction) => Activation::Wheel(match direction {
                "up" => WheelDirection::Up,
                "down" => WheelDirection::Down,
                "left" => WheelDirection::Left,
                "right" => WheelDirection::Right,
                _ => return None,
            }),
            None => Activation::Press(parse_button(last)?),
        };
        let held = tokens
            .into_iter()
            .map(parse_button)
            .collect::<Option<Vec<_>>>()?;
        Self::new(held, activation).ok()
    }
}

/// 多个监听组合放在一起时是否可区分；重复或“目标键是另一组合前缀”都视为冲突。
pub fn input_triggers_conflict<'a>(triggers: impl IntoIterator<Item = &'a InputTrigger>) -> bool {
    let specs: Vec<_> = triggers
        .into_iter()
        .enumerate()
        .map(|(index, trigger)| trigger.spec(index as u64 + 1))
        .collect();
    Engine::new(specs).is_err()
}

fn button_wire(button: Button) -> String {
    match button {
        Button::Key(key) => format!("key:{key:02x}"),
        Button::Mouse(mouse) => format!(
            "mouse:{}",
            match mouse {
                MouseButton::Left => "left",
                MouseButton::Right => "right",
                MouseButton::Middle => "middle",
                MouseButton::X1 => "x1",
                MouseButton::X2 => "x2",
            }
        ),
    }
}

fn parse_button(token: &str) -> Option<Button> {
    if let Some(key) = token.strip_prefix("key:") {
        return u16::from_str_radix(key, 16).ok().map(Button::Key);
    }
    Some(Button::Mouse(match token.strip_prefix("mouse:")? {
        "left" => MouseButton::Left,
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        "x1" => MouseButton::X1,
        "x2" => MouseButton::X2,
        _ => return None,
    }))
}

fn wheel_wire(direction: WheelDirection) -> &'static str {
    match direction {
        WheelDirection::Up => "up",
        WheelDirection::Down => "down",
        WheelDirection::Left => "left",
        WheelDirection::Right => "right",
    }
}

fn wheel_label(direction: WheelDirection) -> &'static str {
    match direction {
        WheelDirection::Up => tr!("滚轮上", "Wheel up"),
        WheelDirection::Down => tr!("滚轮下", "Wheel down"),
        WheelDirection::Left => tr!("滚轮左", "Wheel left"),
        WheelDirection::Right => tr!("滚轮右", "Wheel right"),
    }
}

pub(crate) fn button_label(button: Button) -> String {
    let key = match button {
        Button::Mouse(mouse) => {
            return match mouse {
                MouseButton::Left => tr!("鼠标左键", "Left click"),
                MouseButton::Right => tr!("鼠标右键", "Right click"),
                MouseButton::Middle => tr!("鼠标中键", "Middle click"),
                MouseButton::X1 => tr!("侧键 1", "Mouse X1"),
                MouseButton::X2 => tr!("侧键 2", "Mouse X2"),
            }
            .to_string()
        }
        Button::Key(key) => key,
    };
    let name = match key {
        0x10 => "Shift",
        0x11 => "Ctrl",
        0x12 => "Alt",
        0xa0 => "LShift",
        0xa1 => "RShift",
        0xa2 => "LCtrl",
        0xa3 => "RCtrl",
        0xa4 => "LAlt",
        0xa5 => "RAlt",
        0x5b => "LWin",
        0x5c => "RWin",
        0x60..=0x69 => return format!("Num{}", key - 0x60),
        0x6a => "Num*",
        0x6b => "Num+",
        0x6d => "Num-",
        0x6e => "Num.",
        0x6f => "Num/",
        0xad => "VolumeMute",
        0xae => "VolumeDown",
        0xaf => "VolumeUp",
        0xb0 => "MediaNext",
        0xb1 => "MediaPrev",
        0xb2 => "MediaStop",
        0xb3 => "MediaPlay",
        _ => return crate::hotkey::key_label(u32::from(key)),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: Button = Button::Key(0x14);
    const W: Button = Button::Key(0x57);
    const LCTRL: Button = Button::Key(0xa2);
    const X1: Button = Button::Mouse(MouseButton::X1);

    #[test]
    fn modifier_only_prefixes_fall_back_to_system_hotkeys() {
        assert_eq!(
            Trigger::from_recorded(&[LCTRL], Activation::Press(Button::Key(0x31))),
            Ok(Trigger::Hotkey(HotkeySpec {
                modifiers: CTRL,
                key: 0x31
            }))
        );
        assert_eq!(
            Trigger::from_recorded(&[], Activation::Press(Button::Key(0x7b))),
            Ok(Trigger::Hotkey(HotkeySpec {
                modifiers: 0,
                key: 0x7b
            }))
        );
    }

    #[test]
    fn extended_combinations_use_input_triggers() {
        let caps = Trigger::from_recorded(&[CAPS], Activation::Press(W)).unwrap();
        assert!(matches!(caps, Trigger::Input(_)));
        assert_eq!(caps.label(), "CapsLock+W");
        let wheel = Trigger::from_recorded(&[X1], Activation::Wheel(WheelDirection::Up)).unwrap();
        assert!(matches!(wheel, Trigger::Input(_)));
        // 媒体键不在系统热键支持表里，仍可由监听匹配。
        assert!(matches!(
            Trigger::from_recorded(&[LCTRL], Activation::Press(Button::Key(0xb3))),
            Ok(Trigger::Input(_))
        ));
        assert!(matches!(
            Trigger::from_recorded(&[], Activation::Press(X1)),
            Ok(Trigger::Input(_))
        ));
    }

    #[test]
    fn unsafe_combinations_are_rejected() {
        let left = Button::Mouse(MouseButton::Left);
        assert_eq!(
            Trigger::from_recorded(&[], Activation::Press(left)),
            Err(TriggerError::PrimaryClickAlone)
        );
        assert_eq!(
            Trigger::from_recorded(&[left], Activation::Wheel(WheelDirection::Up)),
            Err(TriggerError::LeftButtonPrefix)
        );
        assert_eq!(
            Trigger::from_recorded(&[], Activation::Wheel(WheelDirection::Down)),
            Err(TriggerError::BareWheel)
        );
        assert_eq!(
            Trigger::from_recorded(&[], Activation::Press(Button::Key(0x41))),
            Err(TriggerError::Hotkey(HotkeyError::UnmodifiedPrintable))
        );
        assert_eq!(
            Trigger::from_recorded(&[], Activation::Press(LCTRL)),
            Err(TriggerError::Hotkey(HotkeyError::ModifierOnly))
        );
    }

    #[test]
    fn wire_format_round_trips_and_rejects_garbage() {
        for trigger in [
            InputTrigger::new(vec![W, CAPS], Activation::Press(Button::Key(0x51))).unwrap(),
            InputTrigger::new(vec![X1], Activation::Wheel(WheelDirection::Left)).unwrap(),
            InputTrigger::new(vec![], Activation::Press(Button::Mouse(MouseButton::X2))).unwrap(),
        ] {
            assert_eq!(InputTrigger::from_wire(&trigger.to_wire()), Some(trigger));
        }
        assert_eq!(
            InputTrigger::new(vec![CAPS], Activation::Press(W))
                .unwrap()
                .to_wire(),
            "key:14 + key:57"
        );
        for text in [
            "",
            "key:zz",
            "mouse:x3",
            "wheel:up",
            "key:14 + wheel:sideways",
        ] {
            assert_eq!(InputTrigger::from_wire(text), None, "{text}");
        }
    }

    #[test]
    fn overlapping_input_triggers_conflict() {
        let caps_w = InputTrigger::new(vec![CAPS], Activation::Press(W)).unwrap();
        let caps_q = InputTrigger::new(vec![CAPS], Activation::Press(Button::Key(0x51))).unwrap();
        let bare_caps = InputTrigger::new(vec![], Activation::Press(CAPS)).unwrap();
        assert!(!input_triggers_conflict([&caps_w, &caps_q]));
        assert!(input_triggers_conflict([&caps_w, &caps_w]));
        assert!(input_triggers_conflict([&caps_w, &bare_caps]));
    }
}
