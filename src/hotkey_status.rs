//! 托盘给设置页的有界本机回执；签名、PID 和跨进程 QPC 排除旧请求的状态。
use crate::preferences::{self, Preferences};
use std::io::Read;
use windows::{core::w, Win32::UI::WindowsAndMessaging::*};

#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(counter: *mut i64) -> i32;
}

/// QPC 在同一台机器的进程间可比较；失败返回 0，读回执时拒绝未知时序。
pub(crate) fn request_stamp() -> u64 {
    let mut counter = 0_i64;
    if unsafe { QueryPerformanceCounter(&mut counter) } != 0 && counter > 0 {
        counter as u64
    } else {
        0
    }
}

fn signature(preferences: &Preferences) -> String {
    crate::hotkey::HotkeyTarget::ALL
        .into_iter()
        .map(|target| {
            let spec = preferences.custom_hotkey_for(target);
            let wire = preferences
                .custom_input_trigger_for(target)
                .as_ref()
                .map(|trigger| trigger.to_wire())
                .unwrap_or_default();
            format!(
                "{}:{}:{}:{}:{}",
                target.id(),
                preferences.hotkey_mode(target),
                spec.modifiers,
                spec.key,
                wire
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}
pub(crate) fn write(requested: &Preferences, success: bool, message: &str) {
    let directory = preferences::data_dir();
    let message: String = message
        .replace(['\r', '\n'], " ")
        .chars()
        .take(1024)
        .collect();
    let text = format!(
        "version=1\npid={}\nrequested={}\nackCounter={}\nok={}\nmessage={}\n",
        std::process::id(),
        signature(requested),
        request_stamp(),
        success,
        message
    );
    let temp = directory.join(format!("hotkey-status-{}.tmp", std::process::id()));
    let result = std::fs::create_dir_all(&directory)
        .and_then(|_| std::fs::write(&temp, text))
        .and_then(|_| std::fs::rename(&temp, directory.join("hotkey-status.ini")));
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
}
pub(crate) fn read(requested: &Preferences, requested_after: u64) -> Option<(bool, String)> {
    if requested_after == 0 {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(preferences::data_dir().join("hotkey-status.ini"))
        .ok()?
        .take(8193)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 8192 {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let field = |key: &str| {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    };
    if field("requested")? != signature(requested) {
        return None;
    }
    let acknowledged: u64 = field("ackCounter")?.parse().ok()?;
    if acknowledged <= requested_after {
        return None;
    }
    let expected: u32 = field("pid")?.parse().ok()?;
    let mut owner = 0;
    unsafe {
        let tray = FindWindowW(w!("RustShot.MVP.Window"), None).ok()?;
        GetWindowThreadProcessId(tray, Some(&mut owner));
    }
    if owner == 0 || owner != expected {
        return None;
    }
    Some((field("ok")? == "true", field("message")?.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{HotkeySpec, HotkeyTarget, ALT};

    #[test]
    fn receipt_signature_tracks_each_action_and_retained_custom_values() {
        let original = Preferences::default();
        let base = signature(&original);
        for target in HotkeyTarget::ALL {
            let disabled = original.with_hotkey_change(target, 4, None);
            if target.default_binding().is_some() {
                assert_ne!(signature(&disabled), base);
            } else {
                assert_eq!(signature(&disabled), base);
            }
            let custom = original.with_hotkey_change(
                target,
                3,
                Some(HotkeySpec::from_key_name("F9", ALT).unwrap()),
            );
            assert_ne!(signature(&custom), base);
            assert_ne!(signature(&custom), signature(&disabled));
            let disabled_custom = custom.with_hotkey_change(target, 4, None);
            assert_ne!(signature(&disabled_custom), signature(&disabled));
        }
    }

    #[test]
    fn receipt_signature_tracks_disabled_input_combinations_for_new_commands() {
        let original = Preferences::default();
        let trigger = crate::trigger::InputTrigger::from_wire("key:14 + key:48").unwrap();
        let configured = original.with_trigger_change(
            HotkeyTarget::PinToggleGroup,
            crate::trigger::Trigger::Input(trigger),
        );
        let disabled = configured.with_hotkey_change(HotkeyTarget::PinToggleGroup, 4, None);
        assert_ne!(signature(&configured), signature(&disabled));
        assert_ne!(signature(&original), signature(&disabled));
        assert!(signature(&configured).contains("pin.group.toggle_visibility"));
    }
}
