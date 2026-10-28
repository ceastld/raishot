//! 失败后才写入的本机诊断；成功首屏之前没有文件 IO，也不包含截图像素。
use std::time::SystemTime;

const MESSAGE_LIMIT: usize = 2048;
const PANIC_BACKTRACE_LIMIT: usize = 8192;

fn bounded_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn event(stage: &'static str) -> serde_json::Value {
    let unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    serde_json::json!({
        "schema_version": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "unix_ms": unix_ms,
        "stage": stage,
        "contains_capture_pixels": false,
    })
}

/// 诊断目录跟随数据目录（含 `RUSTSHOT_DATA_DIR`）；两者都没有时不写，
/// 避免落到临时目录的兜底路径。
pub(crate) fn diagnostics_dir() -> Option<std::path::PathBuf> {
    if std::env::var_os("RUSTSHOT_DATA_DIR").is_none() && std::env::var_os("LOCALAPPDATA").is_none()
    {
        return None;
    }
    Some(crate::preferences::data_dir().join("diagnostics"))
}

pub(crate) fn record(stage: &'static str, message: &str) {
    let Some(directory) = diagnostics_dir() else {
        return;
    };
    // 错误必须有界；按字符裁剪保证中文与 Windows 错误正文仍是有效 UTF-8。
    let mut event = event(stage);
    event["message"] = bounded_text(message, MESSAGE_LIMIT).into();
    event["request_qpc"] = request_qpc();
    write_event(
        &directory,
        "last-capture-failure.json",
        "capture-failures.jsonl",
        &event,
    );
}

fn request_qpc() -> serde_json::Value {
    std::env::var("RUSTSHOT_STARTUP_REQUEST_QPC")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .into()
}

fn write_event(
    directory: &std::path::Path,
    last_name: &str,
    history_name: &str,
    event: &serde_json::Value,
) {
    // 记录失败不能替换原错误，目录不可写时沿用原调用者的失败处理。
    if let Ok(bytes) = serde_json::to_vec_pretty(&event) {
        if std::fs::create_dir_all(&directory).is_ok() {
            let _ = std::fs::write(directory.join(last_name), bytes);
            append_history(&directory.join(history_name), &event.to_string());
        }
    }
}

fn panic_event(
    message: &str,
    thread: &str,
    location: Option<&str>,
    backtrace: &str,
) -> serde_json::Value {
    let mut event = event("panic");
    event["request_qpc"] = request_qpc();
    event["message"] = bounded_text(message, MESSAGE_LIMIT).into();
    event["thread"] = bounded_text(thread, 128).into();
    event["location"] = location.map(|value| bounded_text(value, 1024)).into();
    event["backtrace"] = bounded_text(backtrace, PANIC_BACKTRACE_LIMIT).into();
    event
}

/// 安装时只替换内存中的回调；只有真正 panic 时才抓取回溯并写盘。
pub(crate) fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Some(directory) = diagnostics_dir() {
                let message = info
                    .payload()
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("非字符串 panic");
                let thread = std::thread::current();
                let location = info.location().map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                });
                let backtrace = std::backtrace::Backtrace::force_capture().to_string();
                let event = panic_event(
                    message,
                    thread.name().unwrap_or("<unnamed>"),
                    location.as_deref(),
                    &backtrace,
                );
                write_event(
                    &directory,
                    "last-capture-failure.json",
                    "capture-failures.jsonl",
                    &event,
                );
            }
            previous(info);
        }));
    });
}

fn worker_exit_event(child_pid: u32, exit_code: Option<i32>) -> serde_json::Value {
    let mut event = event("worker_exit");
    event["worker_pid"] = child_pid.into();
    event["exit_code"] = exit_code.into();
    event["exit_code_hex"] = exit_code
        .map(|code| format!("0x{:08X}", code as u32))
        .into();
    event
}

/// 父进程补记无法由子进程留档的退出，独立文件保留子进程的具体失败阶段。
pub(crate) fn record_worker_exit(child_pid: u32, exit_code: Option<i32>) {
    let Some(directory) = diagnostics_dir() else {
        return;
    };
    write_event(
        &directory,
        "last-capture-worker-exit.json",
        "capture-worker-exits.jsonl",
        &worker_exit_event(child_pid, exit_code),
    );
}

/// 最近若干次失败的单行记录，便于判断偶发问题的频率与种类；只保留尾部，文件有界。
const HISTORY_LIMIT: usize = 200;

pub(crate) fn append_history(path: &std::path::Path, line: &str) {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<&str> = existing.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.push(line);
    let start = lines.len().saturating_sub(HISTORY_LIMIT);
    let mut text = lines[start..].join("\n");
    text.push('\n');
    let _ = std::fs::write(path, text);
}

#[cfg(test)]
mod tests {
    #[test]
    fn worker_exit_preserves_signed_and_windows_status_codes() {
        let event = super::worker_exit_event(42, Some(0xC000_0005_u32 as i32));
        assert_eq!(event["worker_pid"], 42);
        assert_eq!(event["exit_code"], -1_073_741_819_i32);
        assert_eq!(event["exit_code_hex"], "0xC0000005");
        assert_eq!(event["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(event["contains_capture_pixels"], false);
        let absent = super::worker_exit_event(43, None);
        assert!(absent["exit_code"].is_null());
        assert!(absent["exit_code_hex"].is_null());
    }

    #[test]
    fn panic_fields_are_bounded_without_breaking_unicode() {
        let event = super::panic_event(
            &"错".repeat(super::MESSAGE_LIMIT + 1),
            &"线".repeat(129),
            Some(&"位".repeat(1025)),
            &"栈".repeat(super::PANIC_BACKTRACE_LIMIT + 1),
        );
        assert_eq!(event["stage"], "panic");
        for (field, limit) in [
            ("message", super::MESSAGE_LIMIT),
            ("thread", 128),
            ("location", 1024),
            ("backtrace", super::PANIC_BACKTRACE_LIMIT),
        ] {
            assert_eq!(event[field].as_str().unwrap().chars().count(), limit);
        }
    }

    #[test]
    fn worker_exit_does_not_replace_capture_failure() {
        let directory =
            std::env::temp_dir().join(format!("rustshot-worker-exit-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let failure = directory.join("last-capture-failure.json");
        std::fs::write(&failure, b"original failure").unwrap();
        super::write_event(
            &directory,
            "last-capture-worker-exit.json",
            "capture-worker-exits.jsonl",
            &super::worker_exit_event(42, Some(1)),
        );
        assert_eq!(std::fs::read(&failure).unwrap(), b"original failure");
        let exit: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("last-capture-worker-exit.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(exit["exit_code_hex"], "0x00000001");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn history_keeps_only_the_newest_lines() {
        let dir = std::env::temp_dir().join(format!("rustshot-failure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("capture-failures.jsonl");
        let _ = std::fs::remove_file(&path);
        for i in 0..super::HISTORY_LIMIT + 5 {
            super::append_history(&path, &format!("{{\"i\":{i}}}"));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), super::HISTORY_LIMIT);
        assert_eq!(lines[0], "{\"i\":5}");
        assert_eq!(
            *lines.last().unwrap(),
            format!("{{\"i\":{}}}", super::HISTORY_LIMIT + 4)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
