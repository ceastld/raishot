//! GIF 成品仅在编码、关闭与最后取消检查完成后发布。
use std::{
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TemporaryGif {
    path: PathBuf,
}

impl TemporaryGif {
    pub(crate) fn new(target: &Path) -> io::Result<Self> {
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..128 {
            let path = parent.join(format!(
                ".rustshot-gif-{}-{}.part",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    drop(file);
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "无法创建独占 GIF 临时文件",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn publish(self, target: &Path) -> io::Result<()> {
        // MoveFileW 在目标存在时失败：并发任务不能覆盖较早发布的合法成品。
        // 同目录移动没有跨卷复制，不让 .gif 名称暴露仍在写入的文件。
        move_without_replace(&self.path, target)
    }
}

impl Drop for TemporaryGif {
    fn drop(&mut self) {
        // 仅删除本任务用 create_new 取得的路径，绝不删除已有成品。
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
fn move_without_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(existing: *const u16, new: *const u16) -> i32;
    }
    // canonicalize 保留 Windows verbatim 前缀，长路径也遵从 Rust 文件接口的合同。
    let source = fs::canonicalize(source)?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    if source.parent() != Some(parent.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "GIF 发布必须位于临时文件同目录",
        ));
    }
    let target = parent.join(
        target
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "GIF 目标缺少文件名"))?,
    );
    let wide = |path: &Path| -> io::Result<Vec<u16>> {
        let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "GIF 路径含零字符",
            ));
        }
        value.push(0);
        Ok(value)
    };
    let source = wide(&source)?;
    let target = wide(&target)?;
    if unsafe { MoveFileW(source.as_ptr(), target.as_ptr()) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn move_without_replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(source, target)?;
    fs::remove_file(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceled_or_competing_gif_keeps_existing_output() {
        let root = std::env::temp_dir().join(format!(
            "rustshot-gif-output-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("result.gif");
        fs::write(&target, b"existing").unwrap();
        let canceled = TemporaryGif::new(&target).unwrap();
        let canceled_path = canceled.path().to_owned();
        fs::write(&canceled_path, b"partial").unwrap();
        drop(canceled);
        assert!(!canceled_path.exists());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        let competing = TemporaryGif::new(&target).unwrap();
        let competing_path = competing.path().to_owned();
        fs::write(&competing_path, b"new").unwrap();
        assert!(competing.publish(&target).is_err());
        assert!(!competing_path.exists());
        assert_eq!(fs::read(&target).unwrap(), b"existing");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gif_is_visible_only_after_publication() {
        let root = std::env::temp_dir().join(format!(
            "rustshot-gif-publish-{}-{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("result.gif");
        let temporary = TemporaryGif::new(&target).unwrap();
        let temporary_path = temporary.path().to_owned();
        fs::write(&temporary_path, b"complete").unwrap();
        assert!(!target.exists());
        temporary.publish(&target).unwrap();
        assert!(!temporary_path.exists());
        assert_eq!(fs::read(&target).unwrap(), b"complete");
        fs::remove_dir_all(root).unwrap();
    }
}
