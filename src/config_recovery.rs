//! 配置字节读取与损坏恢复；只把非 UTF-8 和超过预算视为确定损坏。
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

const LOCK_ATTEMPTS: u32 = 20;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(15);

enum Contents {
    Missing,
    Text(String),
    Corrupt(&'static str),
}

/// 正常读取不创建文件；确定损坏后才加锁、复核并把原字节移到同目录备份。
pub(crate) fn load_text(path: &Path, max_bytes: u64) -> io::Result<Option<String>> {
    match inspect(path, max_bytes)? {
        Contents::Missing => Ok(None),
        Contents::Text(text) => Ok(Some(text)),
        Contents::Corrupt(_) => {
            let _lock = lock(path)?;
            read_text_locked(path, max_bytes)
        }
    }
}

/// 调用方必须持有 lock(path)，并保持到替换写入完成，避免另一个进程绕过恢复。
/// 任何读取或备份失败都返回错误，调用方不得继续覆盖配置。
pub(crate) fn read_text_locked(path: &Path, max_bytes: u64) -> io::Result<Option<String>> {
    match inspect(path, max_bytes)? {
        Contents::Missing => Ok(None),
        Contents::Text(text) => Ok(Some(text)),
        Contents::Corrupt(reason) => {
            preserve_unreadable_locked(path, reason)?;
            Ok(None)
        }
    }
}

/// 上层格式（如 JSON 语法错误）确认无法解析时调用；调用方必须持有 lock(path)。
/// 原字节移到同目录备份并提示一次，之后按缺失处理。
pub(crate) fn preserve_unreadable_locked(path: &Path, reason: &str) -> io::Result<()> {
    let backup = preserve_corrupt(path)?;
    report_once(
        path,
        NoticeKind::Recovered,
        trf!(
            "配置 {} 无法读取（{}），原文件已备份到 {}，本次使用默认设置。",
            "Could not read settings {} ({}). The original was backed up to {}; defaults are in use.",
            path.display(), reason, backup.display()
        ),
    );
    Ok(())
}

fn inspect(path: &Path, max_bytes: u64) -> io::Result<Contents> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Contents::Missing),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Ok(Contents::Corrupt(tr!("文件过长", "file is too large")));
    }
    Ok(match String::from_utf8(bytes) {
        Ok(text) => Contents::Text(text),
        Err(_) => Contents::Corrupt(tr!("不是有效的 UTF-8", "invalid UTF-8")),
    })
}

/// 锁文件固定为 `<名>.ini.lock`：x.json 与迁移前的 x.ini 共用同一把锁。
/// 文件锁在 File 析构时释放。
pub(crate) fn lock(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("ini.lock"))?;
    for attempt in 0..LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if attempt + 1 < LOCK_ATTEMPTS {
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        trf!(
            "配置正被另一个 {} 进程使用，请稍后重试",
            "Settings are in use by another {} process; try again shortly",
            crate::brand::NAME
        ),
    ))
}

fn preserve_corrupt(path: &Path) -> io::Result<PathBuf> {
    static BACKUP_ID: AtomicU64 = AtomicU64::new(0);
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing settings filename"))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..8 {
        let mut name = filename.to_os_string();
        name.push(format!(
            ".corrupt-{timestamp}-{}-{}.bak",
            std::process::id(),
            BACKUP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let backup = path.with_file_name(name);
        // create_new 预留名字，不覆盖此前的备份。移动前关闭预留句柄。
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&backup)
        {
            Ok(reserved) => drop(reserved),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        if let Err(error) = std::fs::rename(path, &backup) {
            let _ = std::fs::remove_file(&backup);
            return Err(error);
        }
        return Ok(backup);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Could not reserve a settings backup filename",
    ))
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum NoticeKind {
    Recovered,
    ReadFailed,
}

#[derive(Default)]
struct Notices {
    seen: HashSet<(PathBuf, NoticeKind)>,
    pending: Vec<String>,
}

fn notices() -> &'static Mutex<Notices> {
    static NOTICES: OnceLock<Mutex<Notices>> = OnceLock::new();
    NOTICES.get_or_init(|| Mutex::new(Notices::default()))
}

fn report_once(path: &Path, kind: NoticeKind, message: String) {
    let mut notices = notices().lock().unwrap_or_else(|error| error.into_inner());
    if notices.seen.insert((path.to_path_buf(), kind)) {
        eprintln!("{message}");
        notices.pending.push(message);
    }
}

pub(crate) fn report_read_failure(path: &Path, error: &io::Error) {
    report_once(
        path,
        NoticeKind::ReadFailed,
        trf!(
            "无法读取配置 {}：{}。本次暂用默认设置，保存前会再次检查并保护原文件。",
            "Could not read settings {}: {}. Defaults are temporarily in use; the original will be checked and protected before saving.",
            path.display(), error
        ),
    );
}

/// 供设置页或托盘在就绪后显示；同一进程同一配置的同类问题只报告一次。
pub(crate) fn take_notices() -> Vec<String> {
    std::mem::take(
        &mut notices()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            static TEST_ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rustshot-config-recovery-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.join("settings.ini")
        }

        fn backups(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
                .collect()
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn config_recovery_missing_is_a_read_only_default() {
        let dir = TestDirectory::new();
        let path = dir.0.join("missing/settings.ini");
        assert!(load_text(&path, 64).unwrap().is_none());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn config_recovery_valid_legacy_and_unknown_fields_are_not_corruption() {
        let dir = TestDirectory::new();
        let text = "version=99\nfutureSetting=中文\noldField=true\ntruncatedButValidText=";
        std::fs::write(dir.path(), text).unwrap();
        assert_eq!(load_text(&dir.path(), 1024).unwrap().as_deref(), Some(text));
        assert_eq!(std::fs::read(dir.path()).unwrap(), text.as_bytes());
        assert!(dir.backups().is_empty());
    }

    #[test]
    fn config_recovery_non_utf8_preserves_original_bytes_before_default() {
        let dir = TestDirectory::new();
        let original = b"version=1\nsecret=\xff\xfe\n";
        std::fs::write(dir.path(), original).unwrap();
        assert!(load_text(&dir.path(), 1024).unwrap().is_none());
        assert!(!dir.path().exists());
        let backups = dir.backups();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), original);
        assert!(load_text(&dir.path(), 1024).unwrap().is_none());
        assert_eq!(dir.backups().len(), 1);
    }

    #[test]
    fn config_recovery_oversize_preserves_complete_file_without_unbounded_read() {
        let dir = TestDirectory::new();
        let original = vec![b'x'; 4096];
        std::fs::write(dir.path(), &original).unwrap();
        let _lock = lock(&dir.path()).unwrap();
        assert!(read_text_locked(&dir.path(), 64).unwrap().is_none());
        assert_eq!(std::fs::read(&dir.backups()[0]).unwrap(), original);
        assert!(!dir.path().exists());
    }

    #[test]
    fn config_recovery_read_error_does_not_move_or_replace_original() {
        let dir = TestDirectory::new();
        std::fs::create_dir(dir.path()).unwrap();
        let marker = dir.path().join("original");
        std::fs::write(&marker, b"keep").unwrap();
        assert!(load_text(&dir.path(), 1024).is_err());
        let _lock = lock(&dir.path()).unwrap();
        assert!(read_text_locked(&dir.path(), 1024).is_err());
        assert_eq!(std::fs::read(marker).unwrap(), b"keep");
        assert!(dir.backups().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn config_recovery_sharing_denied_is_not_treated_as_corruption() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = TestDirectory::new();
        let original = b"version=1\ntheme=dark\n";
        std::fs::write(dir.path(), original).unwrap();
        let denied = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.path())
            .unwrap();
        assert!(load_text(&dir.path(), 1024).is_err());
        let _lock = lock(&dir.path()).unwrap();
        assert!(read_text_locked(&dir.path(), 1024).is_err());
        drop(denied);
        assert_eq!(std::fs::read(dir.path()).unwrap(), original);
        assert!(dir.backups().is_empty());
    }
}
