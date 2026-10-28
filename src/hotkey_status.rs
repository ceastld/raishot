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
    let wire = |trigger: &Option<crate::trigger::InputTrigger>| {
        trigger.as_ref().map(|t| t.to_wire()).unwrap_or_default()
    };
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}",
        preferences.hotkey,
        preferences.custom_hotkey.modifiers,
        preferences.custom_hotkey.key,
        preferences.pin_hotkey,
        preferences.custom_pin_hotkey.modifiers,
        preferences.custom_pin_hotkey.key,
        wire(&preferences.input_trigger),
        wire(&preferences.pin_input_trigger)
    )
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
        for target in [HotkeyTarget::Capture, HotkeyTarget::ClipboardPin] {
            let disabled = original.with_hotkey_change(target, 4, None);
            assert_ne!(signature(&disabled), base);
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
}
