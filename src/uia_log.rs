//! Debug 版 UIA 控件选区日志：每个截图进程一份，写到数据目录的 `diagnostics/uia/`。
//! Release 不写；文件在首条日志时创建，只保留最近若干份。
use std::{fmt, fs::File, io::Write, sync::Mutex, sync::OnceLock, time::Instant};

const KEEP_FILES: usize = 30;

struct Log {
    file: Option<File>,
    started: Instant,
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    cfg!(debug_assertions)
}

fn open() -> Mutex<Log> {
    let directory = crate::preferences::data_dir()
        .join("diagnostics")
        .join("uia");
    let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let name = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{}.log",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        std::process::id()
    );
    let file = std::fs::create_dir_all(&directory)
        .ok()
        .and_then(|()| {
            prune(&directory);
            File::create(directory.join(name)).ok()
        })
        .map(|mut file| {
            let _ = writeln!(
                file,
                "# RustShot UIA {:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} pid={}；坐标：point 为全局物理坐标，rect 相对画布",
                time.wYear,
                time.wMonth,
                time.wDay,
                time.wHour,
                time.wMinute,
                time.wSecond,
                time.wMilliseconds,
                std::process::id()
            );
            file
        });
    Mutex::new(Log {
        file,
        started: Instant::now(),
    })
}

fn prune(directory: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut logs: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
        .collect();
    if logs.len() < KEEP_FILES {
        return;
    }
    // 文件名以本地时间开头，按名排序即按时间排序。
    logs.sort();
    for path in &logs[..=logs.len() - KEEP_FILES] {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn write(args: fmt::Arguments<'_>) {
    let Ok(mut log) = LOG.get_or_init(open).lock() else {
        return;
    };
    let elapsed = log.started.elapsed().as_secs_f64() * 1000.;
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("?").to_owned();
    if let Some(file) = log.file.as_mut() {
        let _ = writeln!(file, "{elapsed:>9.1}ms [{thread}] {args}");
        let _ = file.flush();
    }
}

/// 仅 Debug 构建记录；参数在 Release 中不求值。
macro_rules! uia_log {
    ($($arg:tt)*) => {
        if $crate::uia_log::enabled() {
            $crate::uia_log::write(format_args!($($arg)*));
        }
    };
}
pub(crate) use uia_log;
