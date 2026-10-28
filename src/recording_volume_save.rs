//! 录屏输出的文件事务。编码闭包须完整收尾并释放编码器的输入、输出句柄后返回。
//! 本模块只验证文件形态；非空不代表媒体内容完整。媒体完整性由编码闭包负责。
use crate::{
    file_snapshot::{snapshot, FileSnapshot},
    Result,
};
use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    core::PCWSTR,
    Win32::Storage::FileSystem::{
        MoveFileExW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    },
};

const DIRECTORY_ATTEMPTS: usize = 16;
const STAGED_NAME: &str = "audio.mp4";

/// 原生保存对话框返回之后立即捕获的目标资格；不在后台重新批准已有文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SaveRequest {
    target: PathBuf,
    existing: Option<FileSnapshot>,
}

impl SaveRequest {
    pub(crate) fn from_dialog(chosen: &Path, target: &Path) -> Result<Self> {
        if target.file_name().is_none() || target.as_os_str().encode_wide().any(|unit| unit == 0) {
            return Err(tr!("另存目标文件名无效", "Invalid file name for Save As").into());
        }
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            std::env::current_dir()?.join(target)
        };
        let existing = inspect_target(&target)?;
        // 补扩展名可能落到另一个已有文件；原对话框没有批准这个路径的覆盖。
        let approved_path = if chosen.is_absolute() {
            chosen.to_path_buf()
        } else {
            std::env::current_dir()?.join(chosen)
        };
        if existing.is_some() && approved_path != target {
            return Err(tr!(
                "补全扩展名后的文件已存在，请重新选择完整文件名并确认覆盖",
                "A file with the completed extension already exists; choose the full file name again and confirm the overwrite"
            )
            .into());
        }
        Ok(Self { target, existing })
    }

    /// 本程序新增成品：任何已有文件都没有覆盖资格，提交时仍以无REPLACE防竞态。
    pub(crate) fn for_new_output(target: &Path) -> Result<Self> {
        if !target.is_absolute()
            || target.file_name().is_none()
            || target.as_os_str().encode_wide().any(|unit| unit == 0)
        {
            return Err(tr!(
                "新录屏成品目标无效",
                "Invalid target for the new recording output"
            )
            .into());
        }
        if inspect_target(target)?.is_some() {
            return Err(tr!(
                "新录屏成品目标已存在，未覆盖，可重试",
                "The new output target exists; it was not overwritten. Retry."
            )
            .into());
        }
        Ok(Self {
            target: target.into(),
            existing: None,
        })
    }

    /// 内部关闭写回只接受本会话成功发布后取得、且仍匹配的原件。
    /// 不能用 from_dialog(path, path) 为内部任务伪造用户批准。
    pub(super) fn for_owned_writeback(owned: &OwnedFile, target: &Path) -> Result<Self> {
        if !owned.current(target)? {
            return Err(tr!(
                "本会话原录像已变化，未写回音量，现有文件已保留",
                "The session recording changed; volume was not written back and the file is kept"
            )
            .into());
        }
        Ok(Self {
            target: target.into(),
            existing: Some(owned.snapshot.clone()),
        })
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }
}

/// 只由宿主在自己的成功发布回执处取得；存在同名文件本身不授予所有权。
/// 属性检查与路径删除/替换不是外部文件系统 CAS，不能防任意外部最后换名。
#[derive(Debug)]
pub(super) struct OwnedFile {
    path: PathBuf,
    snapshot: FileSnapshot,
}

impl OwnedFile {
    pub(super) fn from_published(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err("本会话发布路径必须为绝对路径".into());
        }
        let snapshot = inspect_target(path)?.ok_or("本会话发布文件已经不存在")?;
        if snapshot.bytes == 0 {
            return Err("本会话发布文件为空，未取得所有权".into());
        }
        Ok(Self {
            path: path.into(),
            snapshot,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn current(&self, path: &Path) -> Result<bool> {
        Ok(path == self.path.as_path() && inspect_target(path)?.as_ref() == Some(&self.snapshot))
    }

    /// 只在这次获准 Save 的旧目标确为本对象、且本会话路径确指向本次
    /// 成功发布对象时刷新；硬链接别名被 Replace 不会重新批准被外部替换的原件。
    pub(super) fn refresh_after_save(
        &mut self,
        request: &SaveRequest,
        published: &Path,
    ) -> Result<bool> {
        if request.target() != published || request.existing.as_ref() != Some(&self.snapshot) {
            return Ok(false);
        }
        let published = inspect_target(published)?.ok_or("刚保存的成品已不存在")?;
        if inspect_target(&self.path)?.as_ref() == Some(&published) {
            self.snapshot = published;
            return Ok(true);
        }
        Ok(false)
    }

    /// 不删除未知同名或已经被 Save/其它操作替换的文件。
    pub(super) fn remove_if_current(self) -> Result<bool> {
        if !self.current(&self.path)? {
            return Ok(false);
        }
        std::fs::remove_file(&self.path)?;
        Ok(true)
    }
}

/// 闭包只可创建/修改传入的暂存文件，并须成功完成媒体收尾且关闭文件后返回 Ok。
/// 本阶段从不发布：真实宿主在收取回执后核对会话、源和保存意图，再调用 commit。
pub(crate) fn prepare_with(
    request: &SaveRequest,
    encode: impl FnOnce(&Path) -> Result<()>,
    keep_going: impl FnMut() -> bool,
) -> Result<StagedSave> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let session = format!("{}-{time}-{sequence}", std::process::id());
    prepare_with_session_impl(request, &session, encode, keep_going)
}

/// 原始MP4/GIF的自有复制快照；仍先暂存，只有宿主当前意图可以提交。
pub(crate) fn prepare_copy(
    request: &SaveRequest,
    source: &Path,
    keep_going: impl FnMut() -> bool,
) -> Result<StagedSave> {
    prepare_with(
        request,
        |staged| {
            std::fs::copy(source, staged)?;
            Ok(())
        },
        keep_going,
    )
}

fn prepare_with_session_impl(
    request: &SaveRequest,
    session: &str,
    encode: impl FnOnce(&Path) -> Result<()>,
    mut keep_going: impl FnMut() -> bool,
) -> Result<StagedSave> {
    if !keep_going() {
        return Err(tr!(
            "已取消音量另存，原文件仍可重试",
            "Volume save canceled; the original file can be retried"
        )
        .into());
    }
    let mut temporary = TemporarySave::new(request.target(), session)?;
    let outcome = (|| -> Result<()> {
        encode(&temporary.staged)?;
        if !keep_going() {
            return Err(tr!(
                "已取消音量另存，原文件仍可重试",
                "Volume save canceled; the original file can be retried"
            )
            .into());
        }
        // 独占打开也检查编码器未遗留句柄；验证句柄在提交前立即关闭。
        let output = open_info(&temporary.staged, false)?;
        let info = snapshot(&output)?;
        let bytes = info.bytes;
        temporary.completed = Some(info);
        if bytes == 0 {
            return Err(tr!(
                "音量另存输出为空，未替换原文件",
                "The volume-adjusted output is empty; the original file was not replaced"
            )
            .into());
        }
        drop(output);
        if !keep_going() {
            return Err(tr!(
                "已取消音量另存，原文件仍可重试",
                "Volume save canceled; the original file can be retried"
            )
            .into());
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        return Err(with_cleanup(error, temporary.cleanup()));
    }
    Ok(StagedSave {
        request: request.clone(),
        temporary,
    })
}

/// 仅持有未发布的自有暂存。撤销/迟到回执直接 Drop，不会碰源文件或最终目标。
#[derive(Debug)]
pub(crate) struct StagedSave {
    request: SaveRequest,
    temporary: TemporarySave,
}

impl StagedSave {
    pub(crate) fn request(&self) -> &SaveRequest {
        &self.request
    }

    /// 宿主还须核对实际 job、会话 Arc 身份、源和 Save 目标意图。
    /// 函数只在意图仍当前、目标快照仍匹配且同程序提交锁可取得时发布。
    pub(crate) fn commit(mut self, mut current_intent: impl FnMut() -> bool) -> Result<PathBuf> {
        let outcome = (|| -> Result<PathBuf> {
            if !current_intent() {
                return Err(tr!(
                    "保存已取消，原文件未改动",
                    "Save was cancelled; the original file is unchanged"
                )
                .into());
            }
            let _lock = CommitLock::new(&self.request)?;
            if !current_intent() {
                return Err(tr!(
                    "保存已取消，原文件未改动",
                    "Save was cancelled; the original file is unchanged"
                )
                .into());
            }
            if inspect_target(self.request.target())? != self.request.existing {
                return Err(tr!(
                    "另存目标在编码期间发生变化，未覆盖，请重新选择后重试",
                    "The target changed during encoding and was not overwritten; choose it again and retry"
                )
                .into());
            }
            if !current_intent() {
                return Err(tr!(
                    "保存已取消，原文件未改动",
                    "Save was cancelled; the original file is unchanged"
                )
                .into());
            }
            // 沿用准备时同一输出句柄取得的证明，不从当前路径重新认领文件。
            // 独占验证句柄须先关闭才能改名；此检查不是外部文件系统 CAS。
            let output = open_info(&self.temporary.staged, false)?;
            if self.temporary.completed.as_ref() != Some(&snapshot(&output)?) {
                return Err(tr!(
                    "另存暂存文件在准备后发生变化，未提交，原文件仍保留",
                    "The staged file changed after preparation and was not committed; the original file is kept"
                )
                .into());
            }
            drop(output);
            publish(
                &self.temporary.staged,
                self.request.target(),
                self.request.existing.is_some(),
            )
            .map_err(|error| {
                trf!(
                    "无法提交音量另存，原文件仍保留；请关闭占用文件的程序后重试：{error}",
                    "Couldn't commit the volume save; the original file is kept. Close programs using the file and retry: {error}"
                )
            })?;
            self.temporary.published = true;
            Ok(self.request.target.clone())
        })();
        let cleanup = self.temporary.cleanup();
        match outcome {
            Err(error) => Err(with_cleanup(error, cleanup)),
            Ok(path) => {
                // 提交成功后不能因未知文件阻止空目录清理而把已发布成品报成失败。
                if !cleanup.is_empty() {
                    eprintln!("音量另存已提交，暂存清理提示：{}", cleanup.join("；"));
                }
                Ok(path)
            }
        }
    }
}

fn with_cleanup(
    error: Box<dyn std::error::Error>,
    cleanup: Vec<String>,
) -> Box<dyn std::error::Error> {
    if cleanup.is_empty() {
        error
    } else {
        // 迟到或取消回执可能不再显示错误；保留实际残留路径的诊断。
        eprintln!("音量另存暂存清理提示：{}", cleanup.join("；"));
        trf!(
            "{error}；部分暂存已保留：{}",
            "{error}; some temporary files were kept: {}",
            cleanup.join(tr!("；", "; "))
        )
        .into()
    }
}

/// 只协调本程序在同一父目录、同一个已批准原 FileID 上的提交；不是外部文件系统 CAS。
struct CommitLock(Option<PathBuf>);

impl CommitLock {
    fn new(request: &SaveRequest) -> Result<Self> {
        let Some(existing) = &request.existing else {
            return Ok(Self(None));
        };
        let parent = request.target().parent().ok_or_else(|| {
            tr!(
                "另存目标没有父目录",
                "The Save As target has no parent folder"
            )
        })?;
        let path = parent.join(format!(
            ".rustshot-volume-commit-{:016x}-{:032x}",
            existing.volume,
            u128::from_le_bytes(existing.id)
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => Ok(Self(Some(path))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(tr!(
                    "另存目标已有提交锁，未覆盖；请稍后重试，遗留锁目录需要手动检查",
                    "The target is locked by another save and was not overwritten; retry later, and check any leftover lock folder manually"
                )
                .into())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for CommitLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            // 只删除自己 create_dir 成功的空锁目录；冲突或未知内容保留。
            if let Err(error) = std::fs::remove_dir(path) {
                eprintln!("音量另存提交锁未能清理 {}：{error}", path.display());
            }
        }
    }
}

fn inspect_target(path: &Path) -> Result<Option<FileSnapshot>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(snapshot(&open_info(path, true)?)?))
        }
        Ok(_) => Err(tr!(
            "另存目标不是普通文件，未执行覆盖",
            "The Save As target is not a regular file; nothing was overwritten"
        )
        .into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn open_info(path: &Path, shared: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    if shared {
        options
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0);
    } else {
        // 属性访问不参与读/写共享检查；输出验证必须真正请求 GENERIC_READ。
        options.share_mode(0);
    }
    options.open(path)
}

fn publish(source: &Path, target: &Path, replace: bool) -> Result<()> {
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    // 同目录、无 COPY_ALLOWED，不做复制后删除；新目标从不带 REPLACE_EXISTING。
    unsafe { MoveFileExW(PCWSTR(source.as_ptr()), PCWSTR(target.as_ptr()), flags) }?;
    Ok(())
}

#[derive(Debug)]
struct TemporarySave {
    directory: PathBuf,
    staged: PathBuf,
    completed: Option<FileSnapshot>,
    published: bool,
    cleanup_started: bool,
}

impl TemporarySave {
    fn new(target: &Path, session: &str) -> Result<Self> {
        let parent = target.parent().ok_or_else(|| {
            tr!(
                "另存目标没有父目录",
                "The Save As target has no parent folder"
            )
        })?;
        for attempt in 0..DIRECTORY_ATTEMPTS {
            let directory = parent.join(format!(".rustshot-volume-save-{session}-{attempt}"));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        staged: directory.join(STAGED_NAME),
                        directory,
                        completed: None,
                        published: false,
                        cleanup_started: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(tr!(
            "音量另存暂存目录冲突过多，原文件仍可重试",
            "Too many staging folder conflicts; the original file can be retried"
        )
        .into())
    }

    fn cleanup(&mut self) -> Vec<String> {
        if self.cleanup_started {
            return Vec::new();
        }
        self.cleanup_started = true;
        let mut errors = Vec::new();
        if !self.published {
            match inspect_target(&self.staged) {
                Ok(None) => {}
                Ok(Some(current)) if self.completed.as_ref() == Some(&current) => {
                    collect_cleanup(
                        &mut errors,
                        &self.staged,
                        std::fs::remove_file(&self.staged),
                    );
                }
                Ok(Some(_)) => errors.push(trf!(
                    "未确认身份的暂存文件已保留：{}",
                    "Staged file kept because its ownership could not be confirmed: {}",
                    self.staged.display()
                )),
                Err(error) => errors.push(trf!(
                    "无法检查暂存文件，已保留 {}：{error}",
                    "Couldn't inspect staged file; kept {}: {error}",
                    self.staged.display()
                )),
            }
        }
        // 仅移除自己 create_dir 成功的空目录，未知内容或冲突目录从不递归删除。
        collect_cleanup(
            &mut errors,
            &self.directory,
            std::fs::remove_dir(&self.directory),
        );
        errors
    }
}

impl Drop for TemporarySave {
    fn drop(&mut self) {
        let errors = self.cleanup();
        if !errors.is_empty() {
            eprintln!("音量另存暂存清理提示：{}", errors.join("；"));
        }
    }
}

fn collect_cleanup(errors: &mut Vec<String>, path: &Path, result: io::Result<()>) {
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::NotFound {
            errors.push(trf!("{}：{error}", "{}: {error}", path.display()));
        }
    }
}

#[cfg(all(test, windows))]
#[path = "recording_volume_save_identity_tests.rs"]
mod identity_tests;

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::path::absolute("target/qa/volume-identity-model").unwrap();
            let path = root.join(format!(
                "{}-{time}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let root = std::path::absolute("target/qa/volume-identity-model").unwrap();
            if self.0.is_absolute() && self.0.parent() == Some(root.as_path()) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn full_identity_high_half_rejects_save_writeback_and_owned_cleanup() {
        let directory = TestDirectory::new();
        let target = directory.0.join("original.mp4");
        std::fs::write(&target, b"original bytes").unwrap();
        let approved = SaveRequest::from_dialog(&target, &target).unwrap();
        let actual = approved.existing.as_ref().unwrap().clone();
        let mut previous = approved.clone();
        previous.existing.as_mut().unwrap().id[8] ^= 1;
        let previous_stamp = previous.existing.as_ref().unwrap();
        assert_eq!(&previous_stamp.id[..8], &actual.id[..8]);
        assert_ne!(&previous_stamp.id[8..], &actual.id[8..]);
        assert_eq!(previous_stamp.volume, actual.volume);
        assert_eq!(previous_stamp.created, actual.created);
        assert_eq!(previous_stamp.written, actual.written);
        assert_eq!(previous_stamp.bytes, actual.bytes);
        assert_eq!(previous_stamp.attributes, actual.attributes);

        // DTO 只改变先前授权快照；真实文件检查、提交与清理仍走生产实现。
        let staged = prepare_with(
            &previous,
            |path| {
                std::fs::write(path, b"new output")?;
                Ok(())
            },
            || true,
        )
        .unwrap();
        let temporary = staged.temporary.directory.clone();
        assert!(staged.commit(|| true).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original bytes");
        assert!(!temporary.exists());

        let mut owned = OwnedFile::from_published(&target).unwrap();
        assert!(owned.current(&target).unwrap());
        assert!(!owned.refresh_after_save(&previous, &target).unwrap());
        owned.snapshot = previous.existing.unwrap();
        assert!(!owned.current(&target).unwrap());
        assert!(SaveRequest::for_owned_writeback(&owned, &target).is_err());
        assert!(!owned.remove_if_current().unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"original bytes");
        assert_eq!(inspect_target(&target).unwrap(), Some(actual));
    }

    #[test]
    fn full_identity_commit_lock_uses_high_file_id_and_volume_bits() {
        let directory = TestDirectory::new();
        let target = directory.0.join("approved.mp4");
        std::fs::write(&target, b"approved bytes").unwrap();
        let approved = SaveRequest::from_dialog(&target, &target).unwrap();
        let alias = directory.0.join("approved-alias.mp4");
        std::fs::hard_link(&target, &alias).unwrap();
        let alias_request = SaveRequest::from_dialog(&alias, &alias).unwrap();
        assert_eq!(approved.existing, alias_request.existing);
        let first = CommitLock::new(&approved).unwrap();
        assert!(CommitLock::new(&approved.clone()).is_err());
        assert!(CommitLock::new(&alias_request).is_err());

        let mut high_id = approved.clone();
        high_id.existing.as_mut().unwrap().id[8] ^= 1;
        let id_lock = CommitLock::new(&high_id).unwrap();
        assert_ne!(first.0, id_lock.0);
        assert!(CommitLock::new(&high_id).is_err());

        let mut high_volume = approved.clone();
        high_volume.existing.as_mut().unwrap().volume ^= 1u64 << 32;
        let volume_lock = CommitLock::new(&high_volume).unwrap();
        assert_ne!(first.0, volume_lock.0);
        assert_ne!(id_lock.0, volume_lock.0);
        assert!(CommitLock::new(&high_volume).is_err());
        for lock in [&first, &id_lock, &volume_lock] {
            assert!(lock.0.as_ref().unwrap().is_dir());
        }
        let paths: Vec<_> = [&first, &id_lock, &volume_lock]
            .into_iter()
            .map(|lock| lock.0.clone().unwrap())
            .collect();
        drop((first, id_lock, volume_lock));
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(std::fs::read(&target).unwrap(), b"approved bytes");
    }

    #[test]
    fn shared_full_snapshot_keeps_hardlink_identity_and_exclusive_read_validation() {
        let directory = TestDirectory::new();
        let target = directory.0.join("output.mp4");
        let alias = directory.0.join("alias.mp4");
        std::fs::write(&target, b"finished output").unwrap();
        std::fs::hard_link(&target, &alias).unwrap();
        let captured = inspect_target(&target).unwrap().unwrap();
        assert_eq!(inspect_target(&alias).unwrap(), Some(captured.clone()));
        let shared = open_info(&target, true).unwrap();
        assert_eq!(snapshot(&shared).unwrap(), captured);
        drop(shared);

        // 属性共享检查可读；真正 GENERIC_READ/share0 验证必须拒绝未释放的编码句柄。
        let writer = OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        let attributes = open_info(&target, true).unwrap();
        assert_eq!(snapshot(&attributes).unwrap(), captured);
        assert!(open_info(&target, false).is_err());
        drop((attributes, writer));
        let exclusive = open_info(&target, false).unwrap();
        assert_eq!(snapshot(&exclusive).unwrap(), captured);
        drop(exclusive);
    }
}
