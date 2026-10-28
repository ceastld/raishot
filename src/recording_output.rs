//! 录屏会话独占暂存与防覆盖发布。收尾成功前不使用最终成品文件名。
//! 清理动作数量有界；文件系统和驱动调用的墙钟耗时不由本模块保证。
#[path = "recording_output_move.rs"]
mod identified_move;

use super::recovery::{MediaName, Phase as RecoveryPhase, Session as RecoverySession};
use crate::{file_snapshot::FileSnapshot, Result};
use std::{
    error::Error,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DIRECTORY_ATTEMPTS: usize = 16;
const MAX_ENCODER_ATTEMPTS: usize = MediaName::ENCODER_ATTEMPTS.len();
const FILE_NAMES: [&str; 4] = ["video.mp4", "result.gif", "system.wav", "mic.wav"];
const TEMP_LABELS: [&str; 4] = [
    "清理暂存视频",
    "清理暂存 GIF",
    "清理暂存系统音轨",
    "清理暂存麦克风音轨",
];
const PUBLISHED_LABELS: [&str; 4] = [
    "撤回已发布视频",
    "撤回已发布 GIF",
    "撤回已发布系统音轨",
    "撤回已发布麦克风音轨",
];
const RESTORE_LABELS: [&str; 4] = [
    "将已发布视频移回暂存",
    "将已发布 GIF 移回暂存",
    "将已发布系统音轨移回暂存",
    "将已发布麦克风音轨移回暂存",
];
type CleanupErrors = Vec<(&'static str, Box<dyn Error>)>;

#[derive(Debug)]
struct RestoreFailure {
    published: PathBuf,
    staged: PathBuf,
    error: Box<dyn Error>,
}

impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let published = self.published.display();
        let staged = self.staged.display();
        let error = &self.error;
        write!(
            f,
            "{}",
            trf!(
                "回移路径 {published} -> {staged}：{error}；现有文件保留",
                "Restore path {published} -> {staged}: {error}; existing files are kept"
            )
        )
    }
}

impl Error for RestoreFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug)]
struct EncoderAttemptFile {
    path: PathBuf,
    snapshot: Option<FileSnapshot>,
}

#[derive(Debug)]
pub(super) struct RecordingFiles {
    directory: PathBuf,
    staged: [PathBuf; 4],
    published: Vec<(usize, PathBuf, FileSnapshot)>,
    cleanup_started: bool,
    retained_incomplete: bool,
    encoder_attempts: Vec<EncoderAttemptFile>,
    promoted_video: Option<FileSnapshot>,
    recovery: Option<RecoverySession>,
    recovery_errors: CleanupErrors,
}

impl RecordingFiles {
    pub(super) fn new(output: &Path) -> Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let session = format!("{}-{time}-{sequence}", std::process::id());
        let mut files = Self::new_with(output, &session, &mut DiskOperations)?;
        if crate::file_launch_command::experimental_media_enabled() {
            match RecoverySession::create(&files.directory) {
                Ok(recovery) => files.recovery = Some(recovery),
                Err(error) => {
                    let cleanup = files.cleanup(true);
                    return super::stop::after_cleanup(Err(error), cleanup);
                }
            }
        }
        Ok(files)
    }

    fn new_with(
        output: &Path,
        session: &str,
        operations: &mut impl FileOperations,
    ) -> Result<Self> {
        if output.file_name().is_none() {
            return Err("录屏成品文件名无效".into());
        }
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for attempt in 0..DIRECTORY_ATTEMPTS {
            let directory = parent.join(format!(".rustshot-recording-{session}-{attempt}"));
            match operations.create_directory(&directory) {
                Ok(()) => {
                    let staged = FILE_NAMES.map(|name| directory.join(name));
                    return Ok(Self {
                        directory,
                        staged,
                        published: Vec::with_capacity(FILE_NAMES.len()),
                        cleanup_started: false,
                        retained_incomplete: false,
                        encoder_attempts: Vec::new(),
                        promoted_video: None,
                        recovery: None,
                        recovery_errors: Vec::new(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "录屏暂存目录冲突过多").into())
    }

    pub(super) fn video_path(&self) -> &Path {
        &self.staged[0]
    }

    pub(super) fn mark_recording(&mut self) -> Result<()> {
        if let Some(recovery) = &mut self.recovery {
            recovery.set_phase(RecoveryPhase::Recording)?;
        }
        Ok(())
    }

    pub(super) fn register_video(&mut self) -> Result<()> {
        self.register_staged(MediaName::Video)
    }

    pub(super) fn register_gif(&mut self) -> Result<()> {
        self.register_staged(MediaName::Gif)
    }

    pub(super) fn register_track(&mut self, mic: bool) -> Result<()> {
        self.register_staged(if mic {
            MediaName::MicrophoneAudio
        } else {
            MediaName::SystemAudio
        })
    }

    fn register_staged(&mut self, name: MediaName) -> Result<()> {
        let Some(recovery) = &mut self.recovery else {
            return Ok(());
        };
        let path = recovery.directory().join(name.basename());
        let snapshot = crate::file_snapshot::inspect(&path)?.ok_or(tr!(
            "待登记的录屏文件已不存在",
            "The recording file to register is missing"
        ))?;
        recovery.register_file(name, &snapshot)
    }

    pub(super) fn encoder_attempt_paths(&mut self, count: usize) -> Result<Vec<PathBuf>> {
        self.encoder_attempt_paths_with(count, &mut DiskOperations)
    }

    fn encoder_attempt_paths_with(
        &mut self,
        count: usize,
        operations: &mut impl FileOperations,
    ) -> Result<Vec<PathBuf>> {
        if self.cleanup_started
            || !self.encoder_attempts.is_empty()
            || count == 0
            || count > MAX_ENCODER_ATTEMPTS
        {
            return Err("编码尝试暂存申请无效".into());
        }
        let paths: Vec<_> = MediaName::ENCODER_ATTEMPTS
            .iter()
            .take(count)
            .map(|name| self.directory.join(name.basename()))
            .collect();
        for path in &paths {
            match operations.inspect(path) {
                Err(error) if is_not_found(error.as_ref()) => {}
                Err(error) => return Err(error),
                Ok(_) => return Err("编码尝试路径已存在，现有文件已保留".into()),
            }
        }
        self.encoder_attempts = paths
            .iter()
            .map(|path| EncoderAttemptFile {
                path: path.clone(),
                snapshot: None,
            })
            .collect();
        Ok(paths)
    }

    pub(super) fn register_encoder_attempt(
        &mut self,
        index: usize,
        snapshot: FileSnapshot,
    ) -> Result<()> {
        self.register_encoder_attempt_with(index, snapshot, &mut DiskOperations)
    }

    fn register_encoder_attempt_with(
        &mut self,
        index: usize,
        snapshot: FileSnapshot,
        operations: &mut impl FileOperations,
    ) -> Result<()> {
        if self.cleanup_started {
            return Err("录屏暂存会话已清理".into());
        }
        let name = MediaName::ENCODER_ATTEMPTS
            .get(index)
            .copied()
            .ok_or("编码尝试不属于本录屏会话")?;
        let slot = self
            .encoder_attempts
            .get_mut(index)
            .ok_or("编码尝试不属于本录屏会话")?;
        let current = operations.inspect(&slot.path)?;
        if !current
            .snapshot
            .as_ref()
            .is_some_and(|current| same_object(current, &snapshot))
        {
            return Err("编码尝试文件已被替换，未取得清理资格".into());
        }
        slot.snapshot = Some(snapshot.clone());
        if let Some(recovery) = &mut self.recovery {
            recovery.register_file(name, &snapshot)?;
        }
        Ok(())
    }

    pub(super) fn discard_encoder_attempt(&mut self, index: usize) -> Result<()> {
        self.discard_encoder_attempt_with(index, &mut DiskOperations)
    }

    fn discard_encoder_attempt_with(
        &mut self,
        index: usize,
        operations: &mut impl FileOperations,
    ) -> Result<()> {
        if self.cleanup_started {
            return Err("录屏暂存会话已清理".into());
        }
        let slot = self
            .encoder_attempts
            .get_mut(index)
            .ok_or("编码尝试不属于本录屏会话")?;
        // 清理资格只消费一次；失败保留对象，不让后续 cleanup 自动再试同一路径。
        let snapshot = slot
            .snapshot
            .take()
            .ok_or("编码尝试没有自有文件身份，不能清理")?;
        remove_identified(&slot.path, &snapshot, false, operations)
    }

    pub(super) fn promote_encoder_attempt(&mut self, index: usize) -> Result<()> {
        self.promote_encoder_attempt_with(index, &mut DiskOperations)
    }

    fn promote_encoder_attempt_with(
        &mut self,
        index: usize,
        operations: &mut impl FileOperations,
    ) -> Result<()> {
        if self.cleanup_started || self.promoted_video.is_some() {
            return Err("编码尝试不能重复晋升".into());
        }
        let slot = self
            .encoder_attempts
            .get_mut(index)
            .ok_or("编码尝试不属于本录屏会话")?;
        let owned = slot.snapshot.as_ref().ok_or("编码尝试没有自有文件身份")?;
        let current = operations.inspect(&slot.path)?;
        validate_info(&current)?;
        let snapshot = current.snapshot.ok_or("无法核验编码尝试身份")?;
        if !same_object(owned, &snapshot) {
            return Err("编码尝试文件已被替换，不能晋升".into());
        }
        if let Some(recovery) = &mut self.recovery {
            let source = MediaName::ENCODER_ATTEMPTS.get(index).copied().ok_or(tr!(
                "编码尝试不属于本录屏会话",
                "The encoder attempt does not belong to this recording session"
            ))?;
            // 先持久化新名称的同源身份，移动后中断也能按新名称发现。
            recovery.prepare_video_promotion(source, &snapshot)?;
        }
        operations.publish(&slot.path, &self.staged[0])?;
        slot.snapshot = None;
        // 先登记预期对象，再核移动结果；失败清理不能认领被替换的目标。
        self.promoted_video = Some(snapshot.clone());
        if operations.inspect(&self.staged[0])?.snapshot.as_ref() != Some(&snapshot) {
            return Err("编码晋升目标已变化，现有文件已保留".into());
        }
        if let Some(recovery) = &mut self.recovery {
            recovery.register_file(MediaName::Video, &snapshot)?;
        }
        Ok(())
    }

    pub(super) fn gif_path(&self) -> &Path {
        &self.staged[1]
    }

    pub(super) fn track_path(&self, mic: bool) -> &Path {
        &self.staged[if mic { 3 } else { 2 }]
    }

    pub(super) fn publish_file(&mut self, source: &Path, destination: &Path) -> Result<()> {
        self.publish_with(source, destination, &mut DiskOperations)
    }

    fn publish_with(
        &mut self,
        source: &Path,
        destination: &Path,
        operations: &mut impl FileOperations,
    ) -> Result<()> {
        if self.cleanup_started {
            return Err("录屏暂存会话已清理，不能再发布".into());
        }
        let index = self
            .staged
            .iter()
            .position(|path| path == source)
            .ok_or("只能发布本录屏会话的暂存文件")?;
        if destination.file_name().is_none()
            || destination.starts_with(&self.directory)
            || destination.as_os_str().to_string_lossy().contains('\0')
        {
            return Err("录屏发布目标无效".into());
        }
        if self
            .published
            .iter()
            .any(|(published_index, path, _)| *published_index == index || path == destination)
        {
            return Err("录屏文件不能重复发布".into());
        }
        // 发布资格在移动之前来自已完成的自有暂存，不能在目标路径事后认领替换物。
        let info = operations.inspect(source)?;
        validate_info(&info)?;
        let snapshot = info.snapshot.ok_or(tr!(
            "无法确认录屏临时文件，未保存",
            "Couldn't verify the temporary recording file; nothing was saved"
        ))?;
        // 在移动前取得路径所有权；已成功移动后不再为回滚记录分配内存。
        let owned_destination = destination.to_path_buf();
        operations.publish(source, destination)?;
        self.published
            .push((index, owned_destination, snapshot.clone()));
        // 这里只核验源侧资格，不从目标重新认领对象；变化或读取失败须走失败回滚。
        if operations.inspect(destination)?.snapshot.as_ref() != Some(&snapshot) {
            return Err(tr!(
                "目标录屏文件已被其它程序修改，现有文件已保留",
                "The target recording was changed by another program; the existing file is kept"
            )
            .into());
        }
        Ok(())
    }

    /// 为失败产物取得保留资格，不验证媒体可读性，也不在此移动文件。
    pub(super) fn retain_incomplete(&mut self) -> Option<PathBuf> {
        self.retain_incomplete_with(&mut DiskOperations)
    }

    fn retain_incomplete_with(&mut self, operations: &mut impl FileOperations) -> Option<PathBuf> {
        if self.cleanup_started {
            return None;
        }
        if self.retained_incomplete {
            return Some(self.directory.clone());
        }
        // 公开路径只有仍精确匹配发布记录时才是自有保留证据；检查失败保守保留。
        // cleanup(true) 随后按同一身份将其移回暂存，不删除失败产物的唯一副本。
        let should_retain = self
            .staged
            .iter()
            .chain(
                self.encoder_attempts
                    .iter()
                    .take(MAX_ENCODER_ATTEMPTS)
                    .map(|attempt| &attempt.path),
            )
            .any(|path| match operations.inspect(path) {
                Ok(info) => info.is_file && info.bytes != 0,
                Err(error) => !is_not_found(error.as_ref()),
            })
            || self
                .published
                .iter()
                .any(|(_, path, snapshot)| match operations.inspect(path) {
                    Ok(info) => {
                        info.is_file && info.bytes != 0 && info.snapshot.as_ref() == Some(snapshot)
                    }
                    Err(error) => !is_not_found(error.as_ref()),
                });
        if should_retain {
            self.retained_incomplete = true;
            if let Some(recovery) = &mut self.recovery {
                if let Err(error) = recovery.set_phase(RecoveryPhase::Incomplete) {
                    self.recovery_errors
                        .push((tr!("记录未完成录制", "record incomplete session"), error));
                }
            }
            Some(self.directory.clone())
        } else {
            None
        }
    }

    /// 只调用一次；仅 rollback 为 true 时撤回成功发布的目标。
    /// rollback 为 false 要求调用方已完成全部发布；副清理失败保留已提交媒体。
    /// 保留失败会话时将自有发布前缀移回暂存，租约覆盖全部回移。
    /// 不递归删除目录，未知文件或清理失败保留并返回副错误。
    /// 调用后不再允许发布；重复调用不会重试，也不能追加一次回滚。
    pub(super) fn cleanup(&mut self, rollback: bool) -> CleanupErrors {
        self.cleanup_with(rollback, &mut DiskOperations)
    }

    fn cleanup_with(
        &mut self,
        rollback: bool,
        operations: &mut impl FileOperations,
    ) -> CleanupErrors {
        if self.cleanup_started {
            return Vec::new();
        }
        self.cleanup_started = true;
        let mut errors = std::mem::take(&mut self.recovery_errors);
        let restore_published = rollback && self.retained_incomplete;
        if !restore_published {
            for (index, (path, label)) in self.staged.iter().zip(TEMP_LABELS).enumerate() {
                if index == 0 && !self.encoder_attempts.is_empty() && self.promoted_video.is_none()
                {
                    // 策略尝试尚未晋升时，固定 video.mp4 并非本会话创建，不能按名字清理。
                    continue;
                }
                if index == 0 && self.promoted_video.is_some() {
                    if let Err(error) = remove_identified(
                        path,
                        self.promoted_video.as_ref().unwrap(),
                        true,
                        operations,
                    ) {
                        errors.push((label, error));
                    }
                } else {
                    collect_cleanup_error(&mut errors, label, operations.remove_file(path));
                }
            }
            for slot in &mut self.encoder_attempts {
                if let Some(snapshot) = slot.snapshot.take() {
                    if let Err(error) = remove_identified(&slot.path, &snapshot, false, operations)
                    {
                        errors.push(("清理编码尝试", error));
                    }
                }
            }
            if let Some(mut recovery) = self.recovery.take() {
                // 媒体清理失败时保留恢复证据；释放租约后允许后续只读发现。
                if errors.is_empty() {
                    for error in recovery.cleanup_metadata() {
                        errors.push((
                            tr!("清理恢复元数据", "recovery metadata cleanup"),
                            Box::new(error) as Box<dyn Error>,
                        ));
                    }
                }
            }
            collect_cleanup_error(
                &mut errors,
                "清理录屏暂存目录",
                operations.remove_directory(&self.directory),
            );
        }
        // 已提交媒体不能因副清理错误被删除；只有显式回滚仍按原身份策略撤回。
        if rollback {
            for (index, path, snapshot) in &self.published {
                let result = (|| -> Result<()> {
                    match operations.inspect(path) {
                        Ok(info) if info.snapshot.as_ref() == Some(snapshot) => {
                            if restore_published {
                                // NoReplace：目标占用或回移失败时不删除任何一处现存对象。
                                let staged = &self.staged[*index];
                                operations.restore_identified(path, staged, snapshot)?;
                                if operations.inspect(staged)?.snapshot.as_ref() != Some(snapshot) {
                                    return Err(tr!(
                                        "回移后的录屏暂存文件身份已变化，现有文件已保留",
                                        "The restored recording file identity changed; existing files are kept"
                                    )
                                    .into());
                                }
                            } else {
                                operations.remove_file(path)?;
                            }
                            Ok(())
                        }
                        // 已被外部替换或改写的路径不再具有旧发布对象的删除或回移资格。
                        Ok(_) => Ok(()),
                        Err(error)
                            if error
                                .downcast_ref::<io::Error>()
                                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
                        {
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                })();
                if let Err(error) = result {
                    if restore_published
                        || error
                            .downcast_ref::<io::Error>()
                            .is_none_or(|error| error.kind() != io::ErrorKind::NotFound)
                    {
                        let (label, error) = if restore_published {
                            (
                                RESTORE_LABELS[*index],
                                Box::new(RestoreFailure {
                                    published: path.clone(),
                                    staged: self.staged[*index].clone(),
                                    error,
                                }) as Box<dyn Error>,
                            )
                        } else {
                            (PUBLISHED_LABELS[*index], error)
                        };
                        errors.push((label, error));
                    }
                }
            }
        }
        if restore_published {
            // 回移完成或明确失败后才释放租约；Drop 不删除清单或媒体。
            drop(self.recovery.take());
        }
        errors
    }
}

fn same_object(left: &FileSnapshot, right: &FileSnapshot) -> bool {
    left.volume == right.volume && left.id == right.id && left.created == right.created
}

fn is_not_found(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn remove_identified(
    path: &Path,
    snapshot: &FileSnapshot,
    exact: bool,
    operations: &mut impl FileOperations,
) -> Result<()> {
    let current = match operations.inspect(path) {
        Ok(current) => current,
        Err(error) if is_not_found(error.as_ref()) => return Ok(()),
        Err(error) => return Err(error),
    };
    let matches = current.snapshot.as_ref().is_some_and(|current| {
        if exact {
            current == snapshot
        } else {
            same_object(current, snapshot)
        }
    });
    if !matches {
        return Err("录屏暂存文件已被替换，现有文件已保留".into());
    }
    operations.remove_file(path)?;
    Ok(())
}

pub(super) fn validate_file(path: &Path) -> Result<u64> {
    validate_with(path, &mut DiskOperations)
}

fn validate_with(path: &Path, operations: &mut impl FileOperations) -> Result<u64> {
    let file = operations.inspect(path)?;
    validate_info(&file)
}

fn validate_info(file: &FileInfo) -> Result<u64> {
    if !file.is_file {
        return Err("录屏输出不是普通文件".into());
    }
    if file.bytes == 0 {
        return Err("录屏输出文件为空".into());
    }
    Ok(file.bytes)
}

fn collect_cleanup_error(errors: &mut CleanupErrors, label: &'static str, result: io::Result<()>) {
    if let Err(error) = result {
        if error.kind() != io::ErrorKind::NotFound {
            errors.push((label, Box::new(error)));
        }
    }
}

struct FileInfo {
    is_file: bool,
    bytes: u64,
    snapshot: Option<FileSnapshot>,
}

/// 策略由生产和纯模型共用；模型只替换底层操作，不改发布或清理分支。
trait FileOperations {
    fn create_directory(&mut self, path: &Path) -> io::Result<()>;
    fn inspect(&mut self, path: &Path) -> Result<FileInfo>;
    fn publish(&mut self, source: &Path, destination: &Path) -> Result<()>;
    fn restore_identified(
        &mut self,
        source: &Path,
        destination: &Path,
        expected: &FileSnapshot,
    ) -> Result<()>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
    fn remove_directory(&mut self, path: &Path) -> io::Result<()>;
}

struct DiskOperations;

impl FileOperations for DiskOperations {
    fn create_directory(&mut self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn inspect(&mut self, path: &Path) -> Result<FileInfo> {
        let snapshot = crate::file_snapshot::inspect(path)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "录屏文件已经不存在"))?;
        Ok(FileInfo {
            is_file: true,
            bytes: snapshot.bytes,
            snapshot: Some(snapshot),
        })
    }

    fn publish(&mut self, source: &Path, destination: &Path) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH},
        };
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(destination.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        }?;
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn restore_identified(
        &mut self,
        source: &Path,
        destination: &Path,
        expected: &FileSnapshot,
    ) -> Result<()> {
        identified_move::restore_identified(source, destination, expected)
    }

    fn remove_directory(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }
}

#[cfg(test)]
#[path = "recording_output_promotion_tests.rs"]
mod promotion_recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    struct ModelOperations {
        directories: BTreeSet<PathBuf>,
        files: BTreeMap<PathBuf, u64>,
        identities: BTreeMap<PathBuf, FileSnapshot>,
        next_identity: u64,
        inspect_errors: BTreeMap<PathBuf, io::ErrorKind>,
        inspected: Vec<PathBuf>,
        replace_source_on_publish: Option<PathBuf>,
        replace_target_after_publish: Option<PathBuf>,
        created: Vec<PathBuf>,
        moves: Vec<(PathBuf, PathBuf)>,
        removed: Vec<PathBuf>,
        removed_directories: Vec<PathBuf>,
        create_error: Option<io::ErrorKind>,
        publish_errors: BTreeMap<PathBuf, io::ErrorKind>,
        remove_errors: BTreeMap<PathBuf, io::ErrorKind>,
    }

    fn model_error(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "注入文件系统失败")
    }

    impl ModelOperations {
        fn replace(&mut self, path: &Path, bytes: u64) {
            self.files.insert(path.into(), bytes);
            self.identities.remove(path);
        }
    }

    impl FileOperations for ModelOperations {
        fn create_directory(&mut self, path: &Path) -> io::Result<()> {
            self.created.push(path.to_path_buf());
            if let Some(kind) = self.create_error {
                return Err(model_error(kind));
            }
            if !self.directories.insert(path.to_path_buf()) {
                return Err(model_error(io::ErrorKind::AlreadyExists));
            }
            Ok(())
        }

        fn inspect(&mut self, path: &Path) -> Result<FileInfo> {
            self.inspected.push(path.into());
            if let Some(kind) = self.inspect_errors.get(path) {
                return Err(model_error(*kind).into());
            }
            if self.directories.contains(path) {
                return Ok(FileInfo {
                    is_file: false,
                    bytes: 0,
                    snapshot: None,
                });
            }
            let bytes = self
                .files
                .get(path)
                .copied()
                .ok_or_else(|| model_error(io::ErrorKind::NotFound))?;
            if !self.identities.contains_key(path) {
                self.next_identity += 1;
                let mut id = [0; 16];
                id[..8].copy_from_slice(&self.next_identity.to_le_bytes());
                self.identities.insert(
                    path.into(),
                    FileSnapshot {
                        volume: 1,
                        id,
                        created: 1,
                        written: 1,
                        bytes,
                        attributes: 0,
                    },
                );
            }
            let mut snapshot = self.identities[path].clone();
            snapshot.bytes = bytes;
            Ok(FileInfo {
                is_file: true,
                bytes,
                snapshot: Some(snapshot),
            })
        }

        fn publish(&mut self, source: &Path, destination: &Path) -> Result<()> {
            self.moves
                .push((source.to_path_buf(), destination.to_path_buf()));
            if let Some(kind) = self.publish_errors.get(source) {
                return Err(model_error(*kind).into());
            }
            if self.files.contains_key(destination) || self.directories.contains(destination) {
                return Err(model_error(io::ErrorKind::AlreadyExists).into());
            }
            if self.replace_source_on_publish.as_deref() == Some(source) {
                let bytes = self.files[source];
                self.replace(source, bytes);
                // 模拟移动前换成同bytes／时间的新对象；身份由原生inspect接口形态读取。
                self.inspect(source)?;
            }
            let bytes = self
                .files
                .remove(source)
                .ok_or_else(|| model_error(io::ErrorKind::NotFound))?;
            self.files.insert(destination.to_path_buf(), bytes);
            if let Some(snapshot) = self.identities.remove(source) {
                self.identities.insert(destination.to_path_buf(), snapshot);
            }
            if self.replace_target_after_publish.as_deref() == Some(destination) {
                self.replace(destination, bytes);
            }
            Ok(())
        }

        fn remove_file(&mut self, path: &Path) -> io::Result<()> {
            self.removed.push(path.to_path_buf());
            if let Some(kind) = self.remove_errors.get(path) {
                return Err(model_error(*kind));
            }
            self.identities.remove(path);
            self.files
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| model_error(io::ErrorKind::NotFound))
        }

        fn restore_identified(
            &mut self,
            source: &Path,
            destination: &Path,
            expected: &FileSnapshot,
        ) -> Result<()> {
            if self.replace_source_on_publish.as_deref() == Some(source) {
                // 模拟换身只发生在移动句柄核验之前；publish 不再二次触发同一事件。
                self.replace_source_on_publish = None;
                let bytes = self
                    .files
                    .get(source)
                    .copied()
                    .ok_or_else(|| model_error(io::ErrorKind::NotFound))?;
                self.replace(source, bytes);
            }
            if self.inspect(source)?.snapshot.as_ref() != Some(expected) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "回移源身份已变化").into());
            }
            self.publish(source, destination)
        }

        fn remove_directory(&mut self, path: &Path) -> io::Result<()> {
            self.removed_directories.push(path.to_path_buf());
            if let Some(kind) = self.remove_errors.get(path) {
                return Err(model_error(*kind));
            }
            if self.files.keys().any(|file| file.starts_with(path)) {
                return Err(model_error(io::ErrorKind::DirectoryNotEmpty));
            }
            self.directories
                .remove(path)
                .then_some(())
                .ok_or_else(|| model_error(io::ErrorKind::NotFound))
        }
    }

    fn session(operations: &mut ModelOperations) -> RecordingFiles {
        RecordingFiles::new_with(Path::new("library/final.mp4"), "model", operations).unwrap()
    }

    #[test]
    fn recording_incomplete_output_retains_failed_staging_without_deleting_or_publishing() {
        for selected in 0..FILE_NAMES.len() + MAX_ENCODER_ATTEMPTS {
            for inspect_fails in [false, true] {
                let mut operations = ModelOperations::default();
                let mut files = session(&mut operations);
                let attempts = files
                    .encoder_attempt_paths_with(MAX_ENCODER_ATTEMPTS, &mut operations)
                    .unwrap();
                let paths: Vec<_> = files.staged.iter().cloned().chain(attempts).collect();
                let path = &paths[selected];
                operations.files.insert(path.clone(), 17);
                if inspect_fails {
                    operations
                        .inspect_errors
                        .insert(path.clone(), io::ErrorKind::PermissionDenied);
                }
                let inspected_before = operations.inspected.len();
                assert_eq!(
                    files.retain_incomplete_with(&mut operations),
                    Some(files.directory.clone())
                );
                assert!(operations.inspected.len() - inspected_before <= paths.len());
                assert!(files.cleanup_with(true, &mut operations).is_empty());
                assert_eq!(operations.files[path], 17);
                assert!(operations.directories.contains(&files.directory));
                assert!(operations.removed.is_empty());
                assert!(operations.removed_directories.is_empty());
                assert!(operations.moves.is_empty());
                assert!(files.retain_incomplete_with(&mut operations).is_none());
                assert!(files.cleanup_with(true, &mut operations).is_empty());
                assert!(operations.removed.is_empty());
            }
        }

        // 保留资格不改变成功路径：rollback=false 仍按原清理策略执行。
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        operations.files.insert(files.video_path().into(), 17);
        assert!(files.retain_incomplete_with(&mut operations).is_some());
        assert!(files.cleanup_with(false, &mut operations).is_empty());
        assert!(operations.files.is_empty());
        assert!(!operations.directories.contains(&files.directory));
    }

    #[test]
    fn recording_incomplete_output_cleans_missing_or_empty_staging() {
        for strict in [false, true] {
            for create_empty_files in [false, true] {
                let mut operations = ModelOperations::default();
                let mut files = session(&mut operations);
                let attempts = if strict {
                    files
                        .encoder_attempt_paths_with(MAX_ENCODER_ATTEMPTS, &mut operations)
                        .unwrap()
                } else {
                    Vec::new()
                };
                if create_empty_files {
                    // 未晋升的严格策略不拥有固定 video.mp4；其余暂存仍沿原契约清理。
                    for path in files.staged.iter().skip(usize::from(strict)) {
                        operations.files.insert(path.clone(), 0);
                    }
                    for (index, path) in attempts.iter().enumerate() {
                        operations.files.insert(path.clone(), 0);
                        let snapshot = operations.inspect(path).unwrap().snapshot.unwrap();
                        files
                            .register_encoder_attempt_with(index, snapshot, &mut operations)
                            .unwrap();
                    }
                }
                let inspected_before = operations.inspected.len();
                assert!(files.retain_incomplete_with(&mut operations).is_none());
                assert_eq!(
                    operations.inspected.len() - inspected_before,
                    FILE_NAMES.len() + attempts.len()
                );
                assert!(!files.retained_incomplete);
                assert!(files.cleanup_with(true, &mut operations).is_empty());
                assert!(operations.files.is_empty());
                assert!(!operations.directories.contains(&files.directory));
            }
        }
    }

    #[test]
    fn recording_incomplete_output_retains_unpublished_video_and_restores_published_prefix() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let video = files.video_path().to_path_buf();
        operations.files.insert(video.clone(), 17);
        let source = files.track_path(true).to_path_buf();
        let target = Path::new("library/partial.wav");
        operations.files.insert(source.clone(), 9);
        files
            .publish_with(&source, target, &mut operations)
            .unwrap();
        let inspected_before = operations.inspected.len();
        assert_eq!(
            files.retain_incomplete_with(&mut operations),
            Some(files.directory.clone())
        );
        assert_eq!(&operations.inspected[inspected_before..], &[video.clone()]);
        assert!(files.retained_incomplete);
        let published_snapshot = files.published[0].2.clone();
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.files.len(), 2);
        assert_eq!(operations.files[&video], 17);
        assert_eq!(operations.files[&source], 9);
        assert_eq!(operations.identities[&source], published_snapshot);
        assert!(!operations.files.contains_key(target));
        assert!(operations.directories.contains(&files.directory));
        assert_eq!(
            operations.moves,
            [
                (source.clone(), target.to_path_buf()),
                (target.to_path_buf(), source),
            ]
        );
        assert!(operations.removed.is_empty());
        assert!(operations.removed_directories.is_empty());
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.moves.len(), 2);
    }

    #[test]
    fn recording_incomplete_output_restores_owned_published_files_without_staging() {
        for empty_staging in [false, true] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            for (source, target, bytes) in [
                (
                    files.video_path().to_path_buf(),
                    Path::new("library/final.mp4"),
                    17,
                ),
                (
                    files.track_path(true).to_path_buf(),
                    Path::new("library/partial.wav"),
                    9,
                ),
            ] {
                operations.files.insert(source.clone(), bytes);
                files
                    .publish_with(&source, target, &mut operations)
                    .unwrap();
            }
            if empty_staging {
                operations.files.insert(files.gif_path().into(), 0);
            }
            let inspected_before = operations.inspected.len();
            assert_eq!(
                files.retain_incomplete_with(&mut operations),
                Some(files.directory.clone())
            );
            let inspected = &operations.inspected[inspected_before..];
            assert_eq!(&inspected[..FILE_NAMES.len()], files.staged.as_slice());
            assert_eq!(inspected.last().unwrap(), Path::new("library/final.mp4"));
            assert!(files.retained_incomplete);
            assert!(files.cleanup_with(true, &mut operations).is_empty());
            assert_eq!(operations.files[files.video_path()], 17);
            assert_eq!(operations.files[files.track_path(true)], 9);
            assert!(!operations
                .files
                .contains_key(Path::new("library/final.mp4")));
            assert!(!operations
                .files
                .contains_key(Path::new("library/partial.wav")));
            assert!(operations.directories.contains(&files.directory));
            assert!(operations.removed.is_empty());
            assert!(operations.removed_directories.is_empty());
            for (index, _, snapshot) in &files.published {
                assert_eq!(&operations.identities[&files.staged[*index]], snapshot);
            }
        }
    }

    #[test]
    fn recording_incomplete_output_restore_failure_keeps_both_paths_and_consumes_cleanup_once() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::NotFound,
            io::ErrorKind::AlreadyExists,
        ] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let video = files.video_path().to_path_buf();
            operations.files.insert(video.clone(), 17);
            let source = files.track_path(true).to_path_buf();
            let target = Path::new("library/partial.wav");
            operations.files.insert(source.clone(), 9);
            files
                .publish_with(&source, target, &mut operations)
                .unwrap();
            if kind == io::ErrorKind::AlreadyExists {
                operations.files.insert(source.clone(), 33);
            } else {
                operations.publish_errors.insert(target.into(), kind);
            }
            assert!(files.retain_incomplete_with(&mut operations).is_some());
            let errors = files.cleanup_with(true, &mut operations);
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].0, RESTORE_LABELS[3]);
            let message = errors[0].1.to_string();
            assert!(message.contains(&target.display().to_string()));
            assert!(message.contains(&source.display().to_string()));
            assert_eq!(
                errors[0]
                    .1
                    .source()
                    .unwrap()
                    .downcast_ref::<io::Error>()
                    .unwrap()
                    .kind(),
                kind
            );
            assert_eq!(operations.files[&video], 17);
            assert_eq!(operations.files[target], 9);
            if kind == io::ErrorKind::AlreadyExists {
                assert_eq!(operations.files[&source], 33);
            } else {
                assert!(!operations.files.contains_key(&source));
            }
            assert!(operations.removed.is_empty() && operations.removed_directories.is_empty());
            let moves = operations.moves.clone();
            operations.publish_errors.clear();
            assert!(files.cleanup_with(true, &mut operations).is_empty());
            assert_eq!(operations.moves, moves);
            assert_eq!(operations.files[target], 9);
        }
    }

    #[test]
    fn recording_incomplete_output_never_adopts_or_restores_replaced_publications() {
        for has_unpublished_video in [false, true] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            if has_unpublished_video {
                operations.files.insert(files.video_path().into(), 17);
            }
            let source = files.track_path(true).to_path_buf();
            let target = Path::new("library/partial.wav");
            operations.files.insert(source.clone(), 9);
            files
                .publish_with(&source, target, &mut operations)
                .unwrap();
            let original = files.published[0].2.clone();
            operations.replace(target, 9);
            let replacement = operations.inspect(target).unwrap().snapshot.unwrap();
            assert_ne!(original.id, replacement.id);
            assert_eq!(
                files.retain_incomplete_with(&mut operations).is_some(),
                has_unpublished_video
            );
            assert!(files.cleanup_with(true, &mut operations).is_empty());
            assert_eq!(operations.files[target], 9);
            assert_eq!(operations.identities[target], replacement);
            assert!(!operations.files.contains_key(&source));
            assert!(!operations.removed.iter().any(|path| path == target));
            assert_eq!(operations.moves.len(), 1);
        }
    }

    #[test]
    fn recording_incomplete_output_rejects_source_replacement_before_identified_restore() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let video = files.video_path().to_path_buf();
        operations.files.insert(video.clone(), 17);
        let staged = files.track_path(true).to_path_buf();
        let published = Path::new("library/partial.wav");
        operations.files.insert(staged.clone(), 9);
        files
            .publish_with(&staged, published, &mut operations)
            .unwrap();
        let expected = files.published[0].2.clone();
        // 事件在外层 inspect 之后、底层取得移动句柄身份之前发生。
        operations.replace_source_on_publish = Some(published.into());
        assert!(files.retain_incomplete_with(&mut operations).is_some());
        let errors = files.cleanup_with(true, &mut operations);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, RESTORE_LABELS[3]);
        assert_eq!(
            errors[0]
                .1
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_ne!(operations.identities[published].id, expected.id);
        assert_eq!(operations.files[published], 9);
        assert_eq!(operations.files[&video], 17);
        assert!(!operations.files.contains_key(&staged));
        assert!(operations.replace_source_on_publish.is_none());
        assert_eq!(operations.moves.len(), 1);
        assert!(operations.removed.is_empty() && operations.removed_directories.is_empty());
        let inspected = operations.inspected.len();
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.inspected.len(), inspected);
        assert_eq!(operations.moves.len(), 1);
        assert_eq!(operations.files[published], 9);
    }

    #[test]
    fn recording_incomplete_output_post_publish_inspection_error_retains_the_only_video() {
        for persistent_error in [false, true] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let source = files.video_path().to_path_buf();
            let target = Path::new("library/final.mp4");
            operations.files.insert(source.clone(), 17);
            operations
                .inspect_errors
                .insert(target.into(), io::ErrorKind::PermissionDenied);
            assert!(files
                .publish_with(&source, target, &mut operations)
                .is_err());
            assert!(!operations.files.contains_key(&source));
            assert_eq!(files.published.len(), 1);
            assert_eq!(
                files.retain_incomplete_with(&mut operations),
                Some(files.directory.clone())
            );
            if !persistent_error {
                operations.inspect_errors.clear();
            }
            let errors = files.cleanup_with(true, &mut operations);
            if persistent_error {
                assert_eq!(errors.len(), 1);
                assert_eq!(errors[0].0, RESTORE_LABELS[0]);
                let message = errors[0].1.to_string();
                assert!(message.contains(&target.display().to_string()));
                assert!(message.contains(&source.display().to_string()));
                assert_eq!(
                    errors[0]
                        .1
                        .source()
                        .unwrap()
                        .downcast_ref::<io::Error>()
                        .unwrap()
                        .kind(),
                    io::ErrorKind::PermissionDenied
                );
                assert_eq!(operations.files[target], 17);
                assert!(!operations.files.contains_key(&source));
            } else {
                assert!(errors.is_empty());
                assert_eq!(operations.files[&source], 17);
                assert!(!operations.files.contains_key(target));
                assert_eq!(operations.identities[&source], files.published[0].2);
            }
            assert!(operations.directories.contains(&files.directory));
            assert!(operations.removed.is_empty() && operations.removed_directories.is_empty());
        }
    }

    #[test]
    fn recording_encoder_output_attempt_slots_are_bounded_and_preserve_requested_count() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let inspected_before = operations.inspected.len();
        for count in [0, 7, usize::MAX] {
            assert!(files
                .encoder_attempt_paths_with(count, &mut operations)
                .is_err());
            assert!(files.encoder_attempts.is_empty());
            assert_eq!(operations.inspected.len(), inspected_before);
        }
        for count in 1..=6 {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let paths = files
                .encoder_attempt_paths_with(count, &mut operations)
                .unwrap();
            assert_eq!(paths.len(), count);
            assert_eq!(files.encoder_attempts.len(), count);
            for (index, path) in paths.iter().enumerate() {
                assert_eq!(
                    path,
                    &files.directory.join(format!("encoder-attempt-{index}.mp4"))
                );
            }
            assert!(files
                .encoder_attempt_paths_with(1, &mut operations)
                .is_err());
            operations.files.insert(paths[0].clone(), 11);
            let snapshot = operations.inspect(&paths[0]).unwrap().snapshot.unwrap();
            let inspected_before = operations.inspected.len();
            for index in [count, usize::MAX] {
                assert!(files
                    .register_encoder_attempt_with(index, snapshot.clone(), &mut operations)
                    .is_err());
                assert_eq!(operations.inspected.len(), inspected_before);
            }
        }
    }

    #[test]
    fn recording_encoder_output_promotes_only_selected_owned_attempt() {
        for selected in 0..6 {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let paths = files
                .encoder_attempt_paths_with(6, &mut operations)
                .unwrap();
            for (index, path) in paths.iter().enumerate() {
                operations.files.insert(path.clone(), 11 + index as u64);
                let snapshot = operations.inspect(path).unwrap().snapshot.unwrap();
                files
                    .register_encoder_attempt_with(index, snapshot, &mut operations)
                    .unwrap();
            }
            // 同一 writer 继续写入可改变长度；对象身份不变，收尾后才能晋升。
            operations.files.insert(paths[selected].clone(), 73);
            files
                .promote_encoder_attempt_with(selected, &mut operations)
                .unwrap();
            assert_eq!(operations.files[files.video_path()], 73);
            assert!(!operations.files.contains_key(&paths[selected]));
            for (index, path) in paths.iter().enumerate() {
                if index != selected {
                    assert_eq!(operations.files[path], 11 + index as u64);
                }
            }
            let source = files.video_path().to_path_buf();
            files
                .publish_with(&source, Path::new("library/final.mp4"), &mut operations)
                .unwrap();
            assert!(files.cleanup_with(false, &mut operations).is_empty());
            assert_eq!(operations.files.len(), 1);
            assert_eq!(operations.files[Path::new("library/final.mp4")], 73);
            assert!(!operations.directories.contains(&files.directory));
            for (index, path) in paths.iter().enumerate() {
                assert_eq!(operations.removed.contains(path), index != selected);
            }
        }
    }

    #[test]
    fn recording_encoder_output_preserves_replaced_attempt_and_consumes_cleanup_once() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let paths = files
            .encoder_attempt_paths_with(1, &mut operations)
            .unwrap();
        operations.files.insert(paths[0].clone(), 11);
        let snapshot = operations.inspect(&paths[0]).unwrap().snapshot.unwrap();
        files
            .register_encoder_attempt_with(0, snapshot, &mut operations)
            .unwrap();
        operations.replace(&paths[0], 11);
        assert!(files
            .discard_encoder_attempt_with(0, &mut operations)
            .is_err());
        assert!(files.encoder_attempts[0].snapshot.is_none());
        assert!(!files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.files[&paths[0]], 11);
        assert!(!operations.removed.contains(&paths[0]));
        assert!(files.cleanup_with(true, &mut operations).is_empty());
    }

    #[test]
    fn recording_encoder_output_preserves_preexisting_or_replaced_promotion_target() {
        for replaced_during_move in [false, true] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let paths = files
                .encoder_attempt_paths_with(1, &mut operations)
                .unwrap();
            operations.files.insert(paths[0].clone(), 11);
            let snapshot = operations.inspect(&paths[0]).unwrap().snapshot.unwrap();
            files
                .register_encoder_attempt_with(0, snapshot, &mut operations)
                .unwrap();
            let video = files.video_path().to_path_buf();
            if replaced_during_move {
                operations.replace_target_after_publish = Some(video.clone());
            } else {
                operations.files.insert(video.clone(), 99);
            }
            assert!(files
                .promote_encoder_attempt_with(0, &mut operations)
                .is_err());
            assert!(!files.cleanup_with(true, &mut operations).is_empty());
            assert!(operations.files.contains_key(&video));
            assert!(!operations.removed.contains(&video));
        }
    }

    #[test]
    fn reservation_never_touches_existing_output_and_collisions_are_bounded() {
        let mut operations = ModelOperations {
            create_error: Some(io::ErrorKind::AlreadyExists),
            ..Default::default()
        };
        let output = Path::new("library/final.mp4");
        operations.files.insert(output.into(), 17);
        let error = RecordingFiles::new_with(output, "model", &mut operations).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(operations.created.len(), DIRECTORY_ATTEMPTS);
        assert!(operations
            .created
            .iter()
            .all(|path| path.parent() == output.parent() && path != output));
        assert_eq!(operations.files[output], 17);
        assert!(operations.removed.is_empty() && operations.moves.is_empty());
    }

    #[test]
    fn reservation_retries_only_collisions_and_owns_only_new_directory() {
        let mut operations = ModelOperations::default();
        let existing = PathBuf::from("library/.rustshot-recording-model-0");
        operations.directories.insert(existing.clone());
        let mut files = session(&mut operations);
        assert_eq!(operations.created.len(), 2);
        assert_ne!(files.directory, existing);
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert!(operations.directories.contains(&existing));
        operations.create_error = Some(io::ErrorKind::PermissionDenied);
        operations.created.clear();
        let error =
            RecordingFiles::new_with(Path::new("library/other.mp4"), "denied", &mut operations)
                .unwrap_err();
        assert_eq!(operations.created.len(), 1);
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn failed_publication_preserves_existing_target_and_does_not_own_it() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let source = files.video_path().to_path_buf();
        let target = Path::new("library/final.mp4");
        operations.files.insert(source.clone(), 41);
        operations.files.insert(target.into(), 97);
        let error = files
            .publish_with(&source, target, &mut operations)
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(files.published.is_empty());
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.files[target], 97);
        assert!(!operations.removed.iter().any(|path| path == target));
    }

    #[test]
    fn publication_rejects_foreign_empty_or_repeated_sources_before_move() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let target = Path::new("library/final.mp4");
        let source = files.video_path().to_path_buf();
        assert!(files
            .publish_with(Path::new("foreign.mp4"), target, &mut operations)
            .is_err());
        assert!(files
            .publish_with(&source, Path::new("library/bad\0.mp4"), &mut operations,)
            .is_err());
        operations.files.insert(source.clone(), 0);
        assert!(files
            .publish_with(&source, target, &mut operations)
            .is_err());
        assert!(operations.moves.is_empty());
        operations.files.insert(source.clone(), 11);
        files
            .publish_with(&source, target, &mut operations)
            .unwrap();
        assert!(files
            .publish_with(&source, Path::new("library/second.mp4"), &mut operations)
            .is_err());
        assert_eq!(operations.moves.len(), 1);
        assert_eq!(files.published.len(), 1);
    }

    #[test]
    fn rollback_removes_only_successful_publications_with_fixed_operation_bound() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let sources = [
            files.video_path().to_path_buf(),
            files.gif_path().to_path_buf(),
            files.track_path(false).to_path_buf(),
            files.track_path(true).to_path_buf(),
        ];
        for (index, source) in sources.iter().enumerate() {
            operations.files.insert(source.clone(), index as u64 + 1);
            let target = PathBuf::from(format!("library/owned-{index}"));
            files
                .publish_with(source, &target, &mut operations)
                .unwrap();
        }
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.removed.len(), 8);
        assert_eq!(operations.removed_directories.len(), 1);
        assert_eq!(
            operations.removed.iter().collect::<BTreeSet<_>>().len(),
            operations.removed.len()
        );
        assert!(operations.files.is_empty());
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.removed.len(), 8);
        assert!(files
            .publish_with(&sources[0], Path::new("library/late"), &mut operations)
            .is_err());
        assert_eq!(operations.moves.len(), 4);
    }

    #[test]
    fn successful_cleanup_keeps_published_file_and_never_appends_late_rollback() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let source = files.video_path().to_path_buf();
        let target = Path::new("library/final.mp4");
        operations.files.insert(source.clone(), 101);
        files
            .publish_with(&source, target, &mut operations)
            .unwrap();
        assert!(files.cleanup_with(false, &mut operations).is_empty());
        assert_eq!(operations.removed.len(), FILE_NAMES.len());
        assert_eq!(operations.files[target], 101);
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.files[target], 101);
        assert_eq!(operations.removed.len(), FILE_NAMES.len());
    }

    #[test]
    fn successful_publication_survives_temporary_or_directory_cleanup_failure() {
        for directory_fails in [false, true] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let source = files.video_path().to_path_buf();
            let target = Path::new("library/final.mp4");
            operations.files.insert(source.clone(), 101);
            files
                .publish_with(&source, target, &mut operations)
                .unwrap();
            assert!(!operations.files.contains_key(&source));
            let (failed_path, label) = if directory_fails {
                (files.directory.clone(), "清理录屏暂存目录")
            } else {
                (files.track_path(false).to_path_buf(), TEMP_LABELS[2])
            };
            operations
                .remove_errors
                .insert(failed_path, io::ErrorKind::PermissionDenied);
            let errors = files.cleanup_with(false, &mut operations);
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].0, label);
            assert_eq!(
                errors[0].1.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(operations.files[target], 101);
            assert!(!operations.removed.iter().any(|path| path == target));
            assert_eq!(operations.removed.len(), FILE_NAMES.len());
            assert_eq!(operations.removed_directories.len(), 1);
            assert_eq!(
                operations.directories.contains(&files.directory),
                directory_fails
            );
            let counts = (
                operations.inspected.len(),
                operations.removed.len(),
                operations.removed_directories.len(),
            );
            operations.remove_errors.clear();
            for rollback in [true, false] {
                assert!(files.cleanup_with(rollback, &mut operations).is_empty());
                assert_eq!(operations.files[target], 101);
                assert_eq!(
                    (
                        operations.inspected.len(),
                        operations.removed.len(),
                        operations.removed_directories.len(),
                    ),
                    counts
                );
            }
        }
    }

    #[test]
    fn cleanup_continues_after_errors_and_preserves_unknown_directory_contents() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let source = files.video_path().to_path_buf();
        operations.files.insert(source.clone(), 13);
        operations
            .remove_errors
            .insert(source, io::ErrorKind::PermissionDenied);
        let unknown = files.directory.join("unknown.file");
        operations.files.insert(unknown.clone(), 29);
        let errors = files.cleanup_with(true, &mut operations);
        assert_eq!(operations.removed.len(), FILE_NAMES.len());
        assert_eq!(operations.removed_directories.len(), 1);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].0, TEMP_LABELS[0]);
        assert_eq!(
            errors[0].1.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            errors[1].1.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        assert_eq!(operations.files[&unknown], 29);
        assert!(!operations.removed.contains(&unknown));
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(operations.removed.len(), FILE_NAMES.len());
    }

    #[test]
    fn validation_keeps_metadata_errors_and_requires_nonempty_regular_file() {
        let mut operations = ModelOperations::default();
        let path = Path::new("missing");
        let error = validate_with(path, &mut operations).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        operations.directories.insert(path.into());
        assert!(validate_with(path, &mut operations).is_err());
        operations.directories.remove(path);
        operations.files.insert(path.into(), 0);
        assert!(validate_with(path, &mut operations).is_err());
        operations.files.insert(path.into(), 37);
        assert_eq!(validate_with(path, &mut operations).unwrap(), 37);
    }

    #[test]
    fn publication_rollback_preserves_foreign_replacement_after_first_success() {
        for same_low_id in [false, true] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let first_source = files.track_path(false).to_path_buf();
            let next_source = files.track_path(true).to_path_buf();
            let first = Path::new("library/final.mp4.system.wav");
            let collision = Path::new("library/final.mp4.mic.wav");
            operations.files.insert(first_source.clone(), 41);
            operations.files.insert(next_source.clone(), 37);
            operations.files.insert(collision.into(), 99);
            files
                .publish_with(&first_source, first, &mut operations)
                .unwrap();
            let published_stamp = operations.identities[first].clone();
            operations.replace(first, 41);
            if same_low_id {
                // 同volume／低64-bit／全部元数据，高64-bit不同；实际回滚比较共享Snapshot。
                let mut foreign = published_stamp.clone();
                foreign.id[8] ^= 1;
                operations.identities.insert(first.into(), foreign);
            }
            let error = files
                .publish_with(&next_source, collision, &mut operations)
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert!(files.cleanup_with(true, &mut operations).is_empty());
            let replacement = &operations.identities[first];
            assert_ne!(published_stamp.id, replacement.id);
            assert_eq!(published_stamp.volume, replacement.volume);
            assert_eq!(published_stamp.created, replacement.created);
            assert_eq!(published_stamp.bytes, replacement.bytes);
            assert_eq!(published_stamp.written, replacement.written);
            assert_eq!(published_stamp.attributes, replacement.attributes);
            if same_low_id {
                assert_eq!(published_stamp.id[..8], replacement.id[..8]);
                assert_ne!(published_stamp.id[8..], replacement.id[8..]);
            }
            assert_eq!(operations.files[first], 41);
            assert_eq!(operations.files[collision], 99);
            assert!(!operations
                .removed
                .iter()
                .any(|path| path == first || path == collision));
            assert!(!operations.files.contains_key(&next_source));
        }
    }

    #[test]
    fn publication_rejects_source_or_target_replacement_during_move() {
        for replace_source in [true, false] {
            let mut operations = ModelOperations::default();
            let mut files = session(&mut operations);
            let source = files.video_path().to_path_buf();
            let target = Path::new("library/final.mp4");
            operations.files.insert(source.clone(), 43);
            if replace_source {
                operations.replace_source_on_publish = Some(source.clone());
            } else {
                operations.replace_target_after_publish = Some(target.into());
            }
            assert!(files
                .publish_with(&source, target, &mut operations)
                .is_err());
            assert_eq!(operations.moves.len(), 1);
            assert_eq!(files.published.len(), 1);
            assert!(files.cleanup_with(true, &mut operations).is_empty());
            assert_eq!(operations.files[target], 43);
            assert!(!operations.removed.iter().any(|path| path == target));
            assert_ne!(files.published[0].2, operations.identities[target]);
        }
    }

    #[test]
    fn publication_capture_or_rollback_inspection_failure_preserves_unknown_files() {
        let mut operations = ModelOperations::default();
        let mut files = session(&mut operations);
        let source = files.video_path().to_path_buf();
        let target = Path::new("library/final.mp4");
        operations.files.insert(source.clone(), 47);
        operations
            .inspect_errors
            .insert(source.clone(), io::ErrorKind::PermissionDenied);
        assert!(files
            .publish_with(&source, target, &mut operations)
            .is_err());
        assert!(operations.moves.is_empty() && files.published.is_empty());
        operations.inspect_errors.clear();
        files
            .publish_with(&source, target, &mut operations)
            .unwrap();
        operations
            .inspect_errors
            .insert(target.into(), io::ErrorKind::PermissionDenied);
        let errors = files.cleanup_with(true, &mut operations);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, PUBLISHED_LABELS[0]);
        assert_eq!(
            errors[0].1.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(operations.files[target], 47);
        assert!(!operations.removed.iter().any(|path| path == target));
        let counts = (operations.inspected.len(), operations.removed.len());
        operations.inspect_errors.clear();
        assert!(files.cleanup_with(true, &mut operations).is_empty());
        assert_eq!(
            counts,
            (operations.inspected.len(), operations.removed.len())
        );
        assert_eq!(operations.files[target], 47);
    }

    /// 固定文件名的测试夹具；只删除自己新建target/qa目录内的列举文件，不递归清理。
    struct DiskFixture {
        root: PathBuf,
        staging: Option<PathBuf>,
    }

    impl DiskFixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent =
                std::path::absolute("target/qa/recording-output-file-snapshot-model").unwrap();
            std::fs::create_dir_all(&parent).unwrap();
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            for _ in 0..16 {
                let root = parent.join(format!(
                    "{}-{time}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&root) {
                    Ok(()) => {
                        return Self {
                            root,
                            staging: None,
                        };
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("不能创建独立文件模型夹具：{error}"),
                }
            }
            panic!("独立文件模型夹具冲突过多");
        }
    }

    impl Drop for DiskFixture {
        fn drop(&mut self) {
            for name in ["first.wav", "later.wav", "held.wav", "final.mp4"] {
                let _ = std::fs::remove_file(self.root.join(name));
            }
            if let Some(staging) = &self.staging {
                if staging.parent() == Some(self.root.as_path()) {
                    for name in FILE_NAMES {
                        let _ = std::fs::remove_file(staging.join(name));
                    }
                    for name in ["recovery.json", "recovery.lease"] {
                        let _ = std::fs::remove_file(staging.join(name));
                    }
                    let _ = std::fs::remove_dir(staging);
                }
            }
            let _ = std::fs::remove_dir(&self.root);
        }
    }

    #[test]
    fn recording_recovery_output_success_and_cancel_release_metadata_before_directory() {
        for rollback in [false, true] {
            let mut fixture = DiskFixture::new();
            let destination = fixture.root.join("final.mp4");
            let mut files =
                RecordingFiles::new_with(&destination, "metadata", &mut DiskOperations).unwrap();
            fixture.staging = Some(files.directory.clone());
            files.recovery = Some(RecoverySession::create(&files.directory).unwrap());
            let source = files.video_path().to_path_buf();
            std::fs::write(&source, b"output-transaction-only").unwrap();
            files.register_video().unwrap();
            files.mark_recording().unwrap();
            let active = super::super::recovery::scan(&fixture.root, SystemTime::now(), &|| false);
            assert_eq!(active.active_skipped, 1);
            assert!(active.sessions.is_empty());
            if !rollback {
                files.publish_file(&source, &destination).unwrap();
            }
            assert!(files.cleanup(rollback).is_empty());
            assert!(!files.directory.exists());
            assert_eq!(destination.exists(), !rollback);
            if !rollback {
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    b"output-transaction-only"
                );
            }
        }
    }

    #[test]
    fn recording_recovery_output_retained_failure_after_track_collision_becomes_discoverable() {
        let mut fixture = DiskFixture::new();
        let destination = fixture.root.join("final.mp4");
        let mut files =
            RecordingFiles::new_with(&destination, "retained", &mut DiskOperations).unwrap();
        fixture.staging = Some(files.directory.clone());
        files.recovery = Some(RecoverySession::create(&files.directory).unwrap());
        let source = files.video_path().to_path_buf();
        std::fs::write(&source, b"incomplete-not-playability-evidence").unwrap();
        files.register_video().unwrap();
        let system = files.track_path(false).to_path_buf();
        let mic = files.track_path(true).to_path_buf();
        std::fs::write(&system, b"system-audio").unwrap();
        std::fs::write(&mic, b"microphone-audio").unwrap();
        files.register_track(false).unwrap();
        files.register_track(true).unwrap();
        files.mark_recording().unwrap();
        let first_target = fixture.root.join("first.wav");
        let collision_target = fixture.root.join("later.wav");
        std::fs::write(&collision_target, b"unrelated-existing-audio").unwrap();
        files.publish_file(&system, &first_target).unwrap();
        assert!(files.publish_file(&mic, &collision_target).is_err());
        assert_eq!(files.retain_incomplete(), Some(files.directory.clone()));
        assert!(files.cleanup(true).is_empty());
        assert!(!destination.exists());
        assert!(!first_target.exists());
        assert_eq!(
            std::fs::read(&collision_target).unwrap(),
            b"unrelated-existing-audio"
        );
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"incomplete-not-playability-evidence"
        );
        assert_eq!(std::fs::read(&mic).unwrap(), b"microphone-audio");
        assert_eq!(std::fs::read(&system).unwrap(), b"system-audio");
        let discovered = super::super::recovery::scan(&fixture.root, SystemTime::now(), &|| false);
        assert_eq!(discovered.active_skipped, 0);
        assert_eq!(discovered.sessions.len(), 1);
        let session = &discovered.sessions[0];
        assert_eq!(session.phase, RecoveryPhase::Incomplete);
        assert!(session.unverified);
        assert!(!session.lease_held);
        let video = session
            .media
            .iter()
            .find(|file| file.name == MediaName::Video)
            .unwrap();
        assert_eq!(video.identity_matches, Some(true));
        assert_eq!(
            video.bytes,
            Some(b"incomplete-not-playability-evidence".len() as u64)
        );
        let system = session
            .media
            .iter()
            .find(|file| file.name == MediaName::SystemAudio)
            .unwrap();
        assert_eq!(system.state, super::super::recovery::MediaState::Present);
        assert_eq!(system.identity_matches, Some(true));
        assert_eq!(system.bytes, Some(b"system-audio".len() as u64));
        let mic = session
            .media
            .iter()
            .find(|file| file.name == MediaName::MicrophoneAudio)
            .unwrap();
        assert_eq!(mic.identity_matches, Some(true));
    }

    #[test]
    fn recording_recovery_output_retained_restore_collision_keeps_existing_files() {
        let mut fixture = DiskFixture::new();
        let destination = fixture.root.join("final.mp4");
        let mut files =
            RecordingFiles::new_with(&destination, "restore-collision", &mut DiskOperations)
                .unwrap();
        fixture.staging = Some(files.directory.clone());
        files.recovery = Some(RecoverySession::create(&files.directory).unwrap());
        let staged = files.track_path(false).to_path_buf();
        let published = fixture.root.join("first.wav");
        std::fs::write(&staged, b"original-system-track").unwrap();
        files.register_track(false).unwrap();
        files.mark_recording().unwrap();
        files.publish_file(&staged, &published).unwrap();
        let expected = files.published[0].2.clone();
        std::fs::write(&staged, b"foreign-stage-occupant").unwrap();
        assert_eq!(files.retain_incomplete(), Some(files.directory.clone()));
        let errors = files.cleanup(true);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, RESTORE_LABELS[2]);
        let message = errors[0].1.to_string();
        assert!(message.contains(&published.display().to_string()));
        assert!(message.contains(&staged.display().to_string()));
        assert!(errors[0].1.source().is_some());
        assert_eq!(std::fs::read(&published).unwrap(), b"original-system-track");
        assert_eq!(std::fs::read(&staged).unwrap(), b"foreign-stage-occupant");
        assert_eq!(
            crate::file_snapshot::inspect(&published).unwrap(),
            Some(expected)
        );
        assert!(files.recovery.is_none());
        let discovered = super::super::recovery::scan(&fixture.root, SystemTime::now(), &|| false);
        assert_eq!(discovered.active_skipped, 0);
        assert_eq!(discovered.sessions.len(), 1);
        let system = discovered.sessions[0]
            .media
            .iter()
            .find(|file| file.name == MediaName::SystemAudio)
            .unwrap();
        assert_eq!(system.state, super::super::recovery::MediaState::Present);
        assert_eq!(system.identity_matches, Some(false));
        assert!(!discovered.sessions[0].lease_held);
    }

    #[test]
    fn disk_publication_rollback_keeps_replacement_after_later_target_collision() {
        let mut fixture = DiskFixture::new();
        let mut files = RecordingFiles::new_with(
            &fixture.root.join("final.mp4"),
            "native",
            &mut DiskOperations,
        )
        .unwrap();
        assert_eq!(files.directory.parent(), Some(fixture.root.as_path()));
        fixture.staging = Some(files.directory.clone());
        let source = files.track_path(false).to_path_buf();
        let later_source = files.track_path(true).to_path_buf();
        let first = fixture.root.join("first.wav");
        let later = fixture.root.join("later.wav");
        let held = fixture.root.join("held.wav");
        std::fs::write(&source, b"AAA").unwrap();
        std::fs::write(&later_source, b"MMM").unwrap();
        std::fs::write(&later, b"keep later").unwrap();
        files.publish_file(&source, &first).unwrap();
        let stamp = files.published[0].2.clone();
        // 保留A文件在另一名字下，避免删除重建时恰好复用相同完整FileId。
        std::fs::rename(&first, &held).unwrap();
        std::fs::write(&first, b"BBB").unwrap();
        let replacement = crate::file_snapshot::inspect(&first).unwrap().unwrap();
        assert_eq!(stamp.bytes, replacement.bytes);
        assert_ne!(stamp.id, replacement.id);
        assert!(files.publish_file(&later_source, &later).is_err());
        assert!(files.cleanup(true).is_empty());
        assert_eq!(std::fs::read(&first).unwrap(), b"BBB");
        assert_eq!(std::fs::read(&later).unwrap(), b"keep later");
        assert_eq!(std::fs::read(&held).unwrap(), b"AAA");
        assert!(!later_source.exists());
        assert!(!files.directory.exists());
        assert!(files.cleanup(true).is_empty());
        assert_eq!(std::fs::read(&first).unwrap(), b"BBB");
    }
}
