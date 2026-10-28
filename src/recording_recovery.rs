//! 未完成录制的清单、活动租约和有界发现；清单与扫描不读取媒体内容。
//! 显式完整回读由 verification 提供；不修复、发布或删除媒体。
//! 扫描结果只描述检查当时的状态；任何后续验证或导出必须重新取得租约并核验身份。
#[path = "recording_recovery_verify.rs"]
pub(crate) mod verification;

use crate::{
    config_json::JsonFile,
    file_snapshot::{self, FileSnapshot},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    ffi::OsStr,
    fmt,
    fs::{self, File, Metadata, OpenOptions, TryLockError},
    io::{self, Read},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

const SCHEMA_VERSION: u32 = 1;
const RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;
const DIRECTORY_PREFIX: &str = ".rustshot-recording-";
const MANIFEST_NAME: &str = "recovery.json";
const LEASE_NAME: &str = "recovery.lease";
const MAX_DIRECTORY_ENTRIES: usize = 2048;
const MAX_CANDIDATES: usize = 64;
const MAX_DIAGNOSTICS: usize = 128;
const MAX_METADATA_ENTRIES: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
// REPARSE_POINT、OFFLINE、RECALL_ON_OPEN、RECALL_ON_DATA_ACCESS。
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Created,
    Recording,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MediaName {
    #[serde(rename = "video.mp4")]
    Video,
    #[serde(rename = "result.gif")]
    Gif,
    #[serde(rename = "system.wav")]
    SystemAudio,
    #[serde(rename = "mic.wav")]
    MicrophoneAudio,
    #[serde(rename = "encoder-attempt-0.mp4")]
    EncoderAttempt0,
    #[serde(rename = "encoder-attempt-1.mp4")]
    EncoderAttempt1,
    #[serde(rename = "encoder-attempt-2.mp4")]
    EncoderAttempt2,
    #[serde(rename = "encoder-attempt-3.mp4")]
    EncoderAttempt3,
    #[serde(rename = "encoder-attempt-4.mp4")]
    EncoderAttempt4,
    #[serde(rename = "encoder-attempt-5.mp4")]
    EncoderAttempt5,
}
impl MediaName {
    pub(crate) const ENCODER_ATTEMPTS: [Self; 6] = [
        Self::EncoderAttempt0,
        Self::EncoderAttempt1,
        Self::EncoderAttempt2,
        Self::EncoderAttempt3,
        Self::EncoderAttempt4,
        Self::EncoderAttempt5,
    ];

    pub(crate) const ALL: [Self; 10] = [
        Self::Video,
        Self::Gif,
        Self::SystemAudio,
        Self::MicrophoneAudio,
        Self::EncoderAttempt0,
        Self::EncoderAttempt1,
        Self::EncoderAttempt2,
        Self::EncoderAttempt3,
        Self::EncoderAttempt4,
        Self::EncoderAttempt5,
    ];

    pub(crate) fn basename(self) -> &'static str {
        match self {
            Self::Video => "video.mp4",
            Self::Gif => "result.gif",
            Self::SystemAudio => "system.wav",
            Self::MicrophoneAudio => "mic.wav",
            Self::EncoderAttempt0 => "encoder-attempt-0.mp4",
            Self::EncoderAttempt1 => "encoder-attempt-1.mp4",
            Self::EncoderAttempt2 => "encoder-attempt-2.mp4",
            Self::EncoderAttempt3 => "encoder-attempt-3.mp4",
            Self::EncoderAttempt4 => "encoder-attempt-4.mp4",
            Self::EncoderAttempt5 => "encoder-attempt-5.mp4",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileIdentity {
    pub volume: u64,
    pub id: [u8; 16],
    pub created: u64,
}
impl From<&FileSnapshot> for FileIdentity {
    fn from(snapshot: &FileSnapshot) -> Self {
        Self {
            volume: snapshot.volume,
            id: snapshot.id,
            created: snapshot.created,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedFile {
    name: MediaName,
    identity: FileIdentity,
    bytes: u64,
    written: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    created_unix_seconds: u64,
    expires_unix_seconds: u64,
    phase: Phase,
    files: Vec<RecordedFile>,
}
impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            created_unix_seconds: 0,
            expires_unix_seconds: RETENTION_SECONDS,
            phase: Phase::Created,
            files: Vec::new(),
        }
    }
}
impl Manifest {
    fn new(created: u64) -> io::Result<Self> {
        Ok(Self {
            created_unix_seconds: created,
            expires_unix_seconds: created.checked_add(RETENTION_SECONDS).ok_or_else(|| {
                invalid(tr!(
                    "恢复清单保留期限溢出",
                    "Recovery retention time overflowed"
                ))
            })?,
            ..Self::default()
        })
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.created_unix_seconds.checked_add(RETENTION_SECONDS)
                != Some(self.expires_unix_seconds)
            || self.files.len() > MediaName::ALL.len()
        {
            return Err(invalid(tr!(
                "恢复清单版本或范围无效",
                "Invalid recovery manifest version or bounds"
            )));
        }
        for (index, file) in self.files.iter().enumerate() {
            if self.files[..index]
                .iter()
                .any(|previous| previous.name == file.name)
            {
                return Err(invalid(tr!(
                    "恢复清单包含重复文件",
                    "Recovery manifest contains duplicate files"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct MetadataError {
    pub path: PathBuf,
    pub error: Box<dyn Error>,
}
impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}
impl Error for MetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug)]
struct CreateError {
    error: Box<dyn Error>,
    cleanup: Vec<MetadataError>,
}
impl fmt::Display for CreateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;
        for error in &self.cleanup {
            write!(f, "; {error}")?;
        }
        Ok(())
    }
}
impl Error for CreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Drop 仅释放句柄；崩溃或未显式清理时，清单仍留给下一次只读发现。
#[derive(Debug)]
pub(crate) struct Session {
    directory: PathBuf,
    directory_guard: Option<File>,
    lease: Option<File>,
    lease_snapshot: Option<FileSnapshot>,
    manifest_snapshot: Option<FileSnapshot>,
    manifest: Manifest,
    cleanup_started: bool,
}
impl Session {
    pub(crate) fn create(staging_dir: &Path) -> Result<Self> {
        Self::create_at(staging_dir, SystemTime::now())
    }

    fn create_at(staging_dir: &Path, now: SystemTime) -> Result<Self> {
        if !staging_dir.file_name().is_some_and(staging_name) {
            return Err(invalid(tr!(
                "恢复会话目录名无效",
                "Invalid recovery session directory name"
            ))
            .into());
        }
        let manifest = Manifest::new(now.duration_since(UNIX_EPOCH)?.as_secs())?;
        let directory_guard = open_directory(staging_dir)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(staging_dir.join(LEASE_NAME))?;
        let mut session = Self {
            directory: staging_dir.to_path_buf(),
            directory_guard: Some(directory_guard),
            lease: Some(lease),
            lease_snapshot: None,
            manifest_snapshot: None,
            manifest,
            cleanup_started: false,
        };
        let created = (|| -> Result<()> {
            let lease = session.lease.as_ref().unwrap();
            session.lease_snapshot = Some(file_snapshot::snapshot(lease)?);
            lease.try_lock().map_err(lock_error)?;
            lease.sync_all()?;
            session.persist(session.manifest.clone())
        })();
        if let Err(error) = created {
            let cleanup = session.cleanup_metadata();
            return Err(Box::new(CreateError { error, cleanup }));
        }
        Ok(session)
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.directory.join(MANIFEST_NAME)
    }

    pub(crate) fn set_phase(&mut self, phase: Phase) -> Result<()> {
        let mut manifest = self.manifest.clone();
        manifest.phase = phase;
        self.persist(manifest)
    }

    /// 调用方登记已取得的文件身份；长度和写时间是一次观测，不否定同一 writer 后续增长。
    /// 登记不授予媒体删除、发布或恢复资格。
    pub(crate) fn register_file(&mut self, name: MediaName, snapshot: &FileSnapshot) -> Result<()> {
        let identity = FileIdentity::from(snapshot);
        if self
            .manifest
            .files
            .iter()
            .any(|file| file.name == name && file.identity != identity)
        {
            return Err(invalid(tr!(
                "已登记的录屏文件身份不同，拒绝重新认领", "The registered recording file has a different identity; refusing to adopt it again",
            )).into());
        }
        let current = inspect_file(&self.directory.join(name.basename()))?.ok_or_else(|| {
            invalid(tr!(
                "待登记的录屏文件已不存在",
                "The recording file to register is missing"
            ))
        })?;
        if FileIdentity::from(&current) != FileIdentity::from(snapshot) {
            return Err(invalid(tr!(
                "待登记的录屏文件身份已变化",
                "The recording file identity changed before registration"
            ))
            .into());
        }
        let record = RecordedFile {
            name,
            identity,
            bytes: snapshot.bytes,
            written: snapshot.written,
        };
        let mut manifest = self.manifest.clone();
        if let Some(previous) = manifest.files.iter_mut().find(|file| file.name == name) {
            *previous = record;
        } else {
            manifest.files.push(record);
        }
        self.persist(manifest)
    }

    /// 原子登记先于 NoReplace 移动，保留源条目覆盖移动两侧的中断窗口。
    /// 这里只做租约内预检，不是外部文件系统 CAS，也不移动或删除媒体。
    pub(crate) fn prepare_video_promotion(
        &mut self,
        source: MediaName,
        snapshot: &FileSnapshot,
    ) -> Result<()> {
        if !MediaName::ENCODER_ATTEMPTS.contains(&source) {
            return Err(invalid(tr!(
                "仅已登记的编码尝试可晋升为录像",
                "Only registered encoder attempts can be promoted to video"
            ))
            .into());
        }
        let identity = FileIdentity::from(snapshot);
        if !self
            .manifest
            .files
            .iter()
            .any(|file| file.name == source && file.identity == identity)
        {
            return Err(invalid(tr!(
                "待晋升的编码尝试未登记或身份不同",
                "The encoder attempt to promote is unregistered or has a different identity"
            ))
            .into());
        }
        let current = inspect_file(&self.directory.join(source.basename()))?.ok_or_else(|| {
            invalid(tr!(
                "待晋升的编码尝试已不存在",
                "The encoder attempt to promote is missing"
            ))
        })?;
        if FileIdentity::from(&current) != identity {
            return Err(invalid(tr!(
                "待晋升的编码尝试身份已变化",
                "The encoder attempt identity changed before promotion"
            ))
            .into());
        }
        if self
            .manifest
            .files
            .iter()
            .any(|file| file.name == MediaName::Video && file.identity != identity)
        {
            return Err(invalid(tr!(
                "录像已登记为另一文件，拒绝重新认领",
                "Video is already registered to another file; refusing to adopt it again"
            ))
            .into());
        }
        match fs::symlink_metadata(self.directory.join(MediaName::Video.basename())) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(invalid(tr!(
                    "录像晋升目标已存在，未认领或覆盖",
                    "The video promotion target already exists; it was not adopted or replaced"
                ))
                .into());
            }
            Err(error) => return Err(error.into()),
        }
        let record = RecordedFile {
            name: MediaName::Video,
            identity,
            bytes: snapshot.bytes,
            written: snapshot.written,
        };
        let mut manifest = self.manifest.clone();
        if let Some(previous) = manifest
            .files
            .iter_mut()
            .find(|file| file.name == MediaName::Video)
        {
            *previous = record;
        } else {
            manifest.files.push(record);
        }
        self.persist(manifest)
    }

    fn persist(&mut self, manifest: Manifest) -> Result<()> {
        if self.cleanup_started || self.lease.is_none() {
            return Err(invalid(tr!(
                "恢复会话租约已释放",
                "The recovery session lease was released"
            ))
            .into());
        }
        manifest.validate()?;
        if inspect_file(&self.directory.join(LEASE_NAME))?.as_ref() != self.lease_snapshot.as_ref()
        {
            return Err(invalid(tr!(
                "恢复会话租约文件已变化",
                "The recovery lease file changed"
            ))
            .into());
        }
        let path = self.manifest_path();
        let current = inspect_file(&path)?;
        if current.as_ref() != self.manifest_snapshot.as_ref() {
            return Err(invalid(tr!(
                "恢复清单已被替换，现有对象已保留",
                "The recovery manifest was replaced; the existing object is kept"
            ))
            .into());
        }
        // 不调用配置 load/lock/迁移；本会话整个生命周期的独占租约保护这次写入。
        JsonFile {
            path: &path,
            max_bytes: MAX_MANIFEST_BYTES,
            schema_version: SCHEMA_VERSION,
            legacy: |_| Manifest::default(),
        }
        .write_locked(&manifest)?;
        // 原子替换产生新对象；仅重新取得的新身份可以用于后续元数据清理。
        self.manifest_snapshot = Some(inspect_file(&path)?.ok_or_else(|| {
            invalid(tr!(
                "写入后无法取得恢复清单身份",
                "Cannot identify the recovery manifest after writing",
            ))
        })?);
        self.manifest = manifest;
        Ok(())
    }

    /// 先释放租约，再只清理身份完整匹配的自有元数据；单次调用，不删除媒体或目录。
    pub(crate) fn cleanup_metadata(&mut self) -> Vec<MetadataError> {
        if self.cleanup_started {
            return Vec::new();
        }
        self.cleanup_started = true;
        drop(self.lease.take());
        let mut errors = Vec::new();
        for (path, expected) in [
            (self.manifest_path(), self.manifest_snapshot.take()),
            (self.directory.join(LEASE_NAME), self.lease_snapshot.take()),
        ] {
            if let Err(error) = remove_metadata(&path, expected.as_ref()) {
                errors.push(MetadataError { path, error });
            }
        }
        // 原子写 helper 的临时名和句柄不向外暴露；仅报告未知残留，绝不按名称认领删除。
        match fs::read_dir(&self.directory) {
            Ok(mut entries) => {
                let mut checked = 0;
                loop {
                    let Some(entry) = entries.next() else { break };
                    if checked == MAX_METADATA_ENTRIES {
                        errors.push(MetadataError {
                            path: self.directory.clone(),
                            error: invalid(tr!(
                                "元数据残留检查达到上限，目录已保留",
                                "Metadata residue scan reached its limit; the directory is kept",
                            ))
                            .into(),
                        });
                        break;
                    }
                    checked += 1;
                    match entry {
                        Ok(entry) if atomic_temp_name(&entry.file_name()) => {
                            errors.push(MetadataError {
                                path: entry.path(),
                                error: invalid(tr!(
                                    "未知原子写临时文件已保留",
                                    "An unidentified atomic-write temporary file is kept",
                                ))
                                .into(),
                            })
                        }
                        Ok(_) => {}
                        Err(error) => errors.push(MetadataError {
                            path: self.directory.clone(),
                            error: error.into(),
                        }),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => errors.push(MetadataError {
                path: self.directory.clone(),
                error: error.into(),
            }),
        }
        // 父输出事务随后才能删除空目录；不能让本模块自己的目录句柄阻止该操作。
        drop(self.directory_guard.take());
        errors
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MediaState {
    Missing,
    Present,
    Skipped,
    InspectFailed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MediaFinding {
    pub name: MediaName,
    pub state: MediaState,
    pub bytes: Option<u64>,
    pub written: Option<u64>,
    pub identity_matches: Option<bool>,
    pub content_unchanged: Option<bool>,
}
impl MediaFinding {
    fn empty(name: MediaName, state: MediaState) -> Self {
        Self {
            name,
            state,
            bytes: None,
            written: None,
            identity_matches: None,
            content_unchanged: None,
        }
    }
    fn present(name: MediaName, snapshot: &FileSnapshot, record: Option<&RecordedFile>) -> Self {
        Self {
            name,
            state: MediaState::Present,
            bytes: Some(snapshot.bytes),
            written: Some(snapshot.written),
            identity_matches: record.map(|record| record.identity == FileIdentity::from(snapshot)),
            content_unchanged: record
                .map(|record| record.bytes == snapshot.bytes && record.written == snapshot.written),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Discovery {
    pub directory: PathBuf,
    pub phase: Phase,
    pub created_unix_seconds: u64,
    pub expires_unix_seconds: u64,
    pub expired: bool,
    pub unverified: bool,
    /// 扫描结束已经释放租约；结果不能作为后续操作的锁或身份资格。
    pub lease_held: bool,
    pub media: Vec<MediaFinding>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticKind {
    UnsafePath,
    DirectoryRead,
    LeaseMissing,
    LeaseCheck,
    ManifestRead,
    ManifestInvalid,
    MediaInspect,
    ChangedDuringScan,
    EntryLimit,
    CandidateLimit,
    Clock,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Diagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
    pub message: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ScanReport {
    pub checked_entries: usize,
    pub checked_candidates: usize,
    pub active_skipped: usize,
    pub truncated: bool,
    pub cancelled: bool,
    pub lease_held: bool,
    pub unverified: bool,
    pub sessions: Vec<Discovery>,
    pub diagnostics: Vec<Diagnostic>,
    pub omitted_diagnostics: u64,
}
impl ScanReport {
    fn note(&mut self, path: &Path, kind: DiagnosticKind, error: impl fmt::Display) {
        if self.diagnostics.len() < MAX_DIAGNOSTICS {
            self.diagnostics.push(Diagnostic {
                path: path.to_path_buf(),
                kind,
                message: error.to_string(),
            });
        } else {
            self.omitted_diagnostics = self.omitted_diagnostics.saturating_add(1);
        }
    }
}

/// 只扫描调用方明确指定的录屏历史目录一级；次数有界，不承诺底层 I/O 的墙钟上界。
pub(crate) fn scan(library: &Path, now: SystemTime, cancelled: &dyn Fn() -> bool) -> ScanReport {
    let mut report = ScanReport {
        unverified: true,
        ..ScanReport::default()
    };
    let now = match now.duration_since(UNIX_EPOCH) {
        Ok(now) => now.as_secs(),
        Err(error) => {
            report.note(library, DiagnosticKind::Clock, error);
            return report;
        }
    };
    let _root = match open_directory(library) {
        Ok(root) => root,
        Err(error) if not_found(error.as_ref()) => return report,
        Err(error) => {
            report.note(library, DiagnosticKind::UnsafePath, error);
            return report;
        }
    };
    let entries = match fs::read_dir(library) {
        Ok(entries) => entries,
        Err(error) => {
            report.note(library, DiagnosticKind::DirectoryRead, error);
            return report;
        }
    };
    let candidates = select_candidates(
        library,
        entries.map(|entry| entry.map(|entry| entry.file_name())),
        cancelled,
        &mut report,
    );
    if report.cancelled {
        return report;
    }
    for name in candidates {
        if cancelled() {
            report.cancelled = true;
            return report;
        }
        scan_session(&library.join(name), now, cancelled, &mut report);
        if report.cancelled {
            return report;
        }
    }
    report
}

fn select_candidates(
    library: &Path,
    entries: impl IntoIterator<Item = io::Result<std::ffi::OsString>>,
    cancelled: &dyn Fn() -> bool,
    report: &mut ScanReport,
) -> Vec<std::ffi::OsString> {
    let mut candidates = Vec::with_capacity(MAX_CANDIDATES);
    let mut entries = entries.into_iter();
    while report.checked_entries < MAX_DIRECTORY_ENTRIES {
        if cancelled() {
            report.cancelled = true;
            return candidates;
        }
        let Some(entry) = entries.next() else {
            return candidates;
        };
        report.checked_entries += 1;
        let name = match entry {
            Ok(name) => name,
            Err(error) => {
                report.note(library, DiagnosticKind::DirectoryRead, error);
                continue;
            }
        };
        if !staging_name(&name) {
            continue;
        }
        if candidates.len() == MAX_CANDIDATES {
            report.truncated = true;
            report.note(
                library,
                DiagnosticKind::CandidateLimit,
                tr!(
                    "恢复候选检查达到上限",
                    "Recovery candidate scan reached its limit"
                ),
            );
            return candidates;
        }
        report.checked_candidates += 1;
        candidates.push(name);
    }
    report.truncated = true;
    report.note(
        library,
        DiagnosticKind::EntryLimit,
        tr!(
            "恢复目录检查达到上限",
            "Recovery directory scan reached its limit"
        ),
    );
    candidates
}

fn scan_session(directory: &Path, now: u64, cancelled: &dyn Fn() -> bool, report: &mut ScanReport) {
    let _directory = match open_directory(directory) {
        Ok(directory) => directory,
        Err(error) => {
            report.note(directory, DiagnosticKind::UnsafePath, error);
            return;
        }
    };
    // 初始清单只在 owner 已获独占锁后发布；未发布前不争 shared lease，避免使启动失败。
    // 这里只预检可用性，后续仍在租约内重新读取并核验内容与身份。
    let manifest_path = directory.join(MANIFEST_NAME);
    match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata.is_file() && attributes_safe(&metadata) => {}
        Ok(_) => {
            report.note(
                &manifest_path,
                DiagnosticKind::ManifestInvalid,
                tr!(
                    "恢复清单不是可直接读取的普通文件",
                    "Recovery manifest is not an ordinary available file"
                ),
            );
            return;
        }
        Err(error) => {
            report.note(&manifest_path, DiagnosticKind::ManifestRead, error);
            return;
        }
    }
    let lease_path = directory.join(LEASE_NAME);
    let lease = match open_checked_file(&lease_path) {
        Ok(lease) => lease,
        Err(error) => {
            let kind = if not_found(error.as_ref()) {
                DiagnosticKind::LeaseMissing
            } else {
                DiagnosticKind::LeaseCheck
            };
            report.note(&lease_path, kind, error);
            return;
        }
    };
    match lease.try_lock_shared() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            report.active_skipped += 1;
            return;
        }
        Err(TryLockError::Error(error)) => {
            report.note(&lease_path, DiagnosticKind::LeaseCheck, error);
            return;
        }
    }
    let lease_before = match file_snapshot::snapshot(&lease) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report.note(&lease_path, DiagnosticKind::LeaseCheck, error);
            return;
        }
    };
    let mut file = match open_checked_file(&manifest_path) {
        Ok(file) => file,
        Err(error) => {
            report.note(&manifest_path, DiagnosticKind::ManifestRead, error);
            return;
        }
    };
    let before = match file_snapshot::snapshot(&file) {
        Ok(before) => before,
        Err(error) => {
            report.note(&manifest_path, DiagnosticKind::ManifestRead, error);
            return;
        }
    };
    if before.bytes > MAX_MANIFEST_BYTES {
        report.note(
            &manifest_path,
            DiagnosticKind::ManifestInvalid,
            tr!(
                "恢复清单超过大小上限",
                "Recovery manifest exceeds its size limit"
            ),
        );
        return;
    }
    let mut bytes = Vec::new();
    if let Err(error) = (&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        report.note(&manifest_path, DiagnosticKind::ManifestRead, error);
        return;
    }
    let manifest = match decode_manifest(&bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            report.note(&manifest_path, DiagnosticKind::ManifestInvalid, error);
            return;
        }
    };
    if let Err(error) = verify_metadata(&manifest_path, &before) {
        report.note(&manifest_path, DiagnosticKind::ChangedDuringScan, error);
        return;
    }
    let mut media = Vec::with_capacity(MediaName::ALL.len());
    for name in MediaName::ALL {
        if cancelled() {
            report.cancelled = true;
            return;
        }
        let path = directory.join(name.basename());
        match inspect_file(&path) {
            Ok(Some(snapshot)) => media.push(MediaFinding::present(
                name,
                &snapshot,
                manifest.files.iter().find(|file| file.name == name),
            )),
            Ok(None) => media.push(MediaFinding::empty(name, MediaState::Missing)),
            Err(error) => {
                let skipped = error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::InvalidData);
                media.push(MediaFinding::empty(
                    name,
                    if skipped {
                        MediaState::Skipped
                    } else {
                        MediaState::InspectFailed
                    },
                ));
                report.note(&path, DiagnosticKind::MediaInspect, error);
            }
        }
    }
    for (path, snapshot) in [(&manifest_path, &before), (&lease_path, &lease_before)] {
        if let Err(error) = verify_metadata(path, snapshot) {
            report.note(path, DiagnosticKind::ChangedDuringScan, error);
            return;
        }
    }
    // 租约只保护本次观察；返回后释放，后续动作必须重拿租约并重新核验。
    report.sessions.push(Discovery {
        directory: directory.to_path_buf(),
        phase: manifest.phase,
        created_unix_seconds: manifest.created_unix_seconds,
        expires_unix_seconds: manifest.expires_unix_seconds,
        expired: now >= manifest.expires_unix_seconds,
        unverified: true,
        lease_held: false,
        media,
    });
}

fn decode_manifest(bytes: &[u8]) -> io::Result<Manifest> {
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(invalid(tr!(
            "恢复清单超过大小上限",
            "Recovery manifest exceeds its size limit"
        )));
    }
    let manifest: Manifest = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    manifest.validate()?;
    Ok(manifest)
}

fn staging_name(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && name.to_str().is_some_and(|name| {
            name.starts_with(DIRECTORY_PREFIX) && name.len() > DIRECTORY_PREFIX.len()
        })
}

fn atomic_temp_name(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with("recovery.json-") && name.ends_with(".tmp"))
}

fn attributes_safe(metadata: &Metadata) -> bool {
    metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0 && !metadata.file_type().is_symlink()
}

fn open_directory(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || !attributes_safe(&metadata) {
        return Err(invalid(tr!(
            "恢复目录不是可直接读取的普通本地目录",
            "Recovery directory is not an ordinary available local directory"
        ))
        .into());
    }
    // 不共享 DELETE，避免持有时目录被重命名或删除；子文件仍各自核验属性和身份。
    let directory = OpenOptions::new()
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || !attributes_safe(&metadata) {
        return Err(invalid(tr!(
            "恢复目录属性发生变化",
            "Recovery directory attributes changed"
        ))
        .into());
    }
    Ok(directory)
}

fn open_checked_file(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || !attributes_safe(&metadata) {
        return Err(invalid(tr!(
            "恢复元数据不是可直接读取的普通文件",
            "Recovery metadata is not an ordinary available file"
        ))
        .into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let snapshot = file_snapshot::snapshot(&file)?;
    if snapshot.attributes & UNSAFE_ATTRIBUTES != 0 {
        return Err(invalid(tr!(
            "恢复元数据属性发生变化",
            "Recovery metadata attributes changed"
        ))
        .into());
    }
    Ok(file)
}

fn inspect_file(path: &Path) -> Result<Option<FileSnapshot>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || !attributes_safe(&metadata) {
        return Err(invalid(tr!(
            "录屏文件是重解析、云占位或非普通文件，已跳过",
            "Recording file is a reparse point, cloud placeholder or non-regular file; skipped"
        ))
        .into());
    }
    let snapshot = file_snapshot::inspect(path)?;
    if snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.attributes & UNSAFE_ATTRIBUTES != 0)
    {
        return Err(invalid(tr!(
            "录屏文件属性发生变化，已跳过",
            "Recording file attributes changed; skipped"
        ))
        .into());
    }
    Ok(snapshot)
}

fn remove_metadata(path: &Path, expected: Option<&FileSnapshot>) -> Result<()> {
    let Some(file) = checked_metadata_for_removal(path, expected)? else {
        return Ok(());
    };
    delete_metadata_handle(file)
}

fn checked_metadata_for_removal(
    path: &Path,
    expected: Option<&FileSnapshot>,
) -> Result<Option<File>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || !attributes_safe(&metadata) {
        return Err(invalid(tr!(
            "恢复元数据不是可直接删除的普通文件，已保留",
            "Recovery metadata is not an ordinary available file; kept"
        ))
        .into());
    }
    let Some(expected) = expected else {
        return Err(invalid(tr!(
            "未取得元数据身份，现有对象已保留",
            "Metadata identity was not acquired; the existing object is kept"
        ))
        .into());
    };
    let file = match OpenOptions::new()
        .read(true)
        // DELETE | FILE_READ_ATTRIBUTES；只共享 READ，核验至删除期间禁止改写或路径替换。
        .access_mode(0x0001_0000 | FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let current = file_snapshot::snapshot(&file)?;
    if current.attributes & UNSAFE_ATTRIBUTES != 0 || &current != expected {
        return Err(invalid(tr!(
            "元数据身份不匹配，现有对象已保留",
            "Metadata identity does not match; the existing object is kept"
        ))
        .into());
    }
    Ok(Some(file))
}

fn delete_metadata_handle(file: File) -> Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // 始终删除已核验的同一对象，不在核验后重新按路径查找待删文件。
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )?;
    }
    drop(file);
    Ok(())
}

fn verify_metadata(path: &Path, expected: &FileSnapshot) -> Result<()> {
    if inspect_file(path)?.as_ref() != Some(expected) {
        return Err(invalid(tr!(
            "恢复元数据检查期间发生变化",
            "Recovery metadata changed during scanning"
        ))
        .into());
    }
    Ok(())
}

fn lock_error(error: TryLockError) -> io::Error {
    match error {
        TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            tr!("恢复会话租约正被占用", "The recovery session lease is busy"),
        ),
        TryLockError::Error(error) => error,
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn not_found(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{cell::Cell, io::Write};

    fn snapshot() -> FileSnapshot {
        FileSnapshot {
            volume: 2,
            id: [3; 16],
            created: 4,
            written: 5,
            bytes: 6,
            attributes: 0,
        }
    }

    #[test]
    fn recording_recovery_manifest_rejects_paths_versions_duplicates_and_oversize() {
        let manifest = Manifest::new(10).unwrap();
        let valid = serde_json::to_value(&manifest).unwrap();
        assert!(decode_manifest(&serde_json::to_vec(&valid).unwrap()).is_ok());
        for name in [
            "../video.mp4",
            "C:\\video.mp4",
            "sub/video.mp4",
            "encoder-attempt-6.mp4",
        ] {
            let mut invalid = valid.clone();
            invalid["files"] = json!([{ "name": name, "identity": {"volume": 1, "id": vec![0_u8; 16], "created": 1}, "bytes": 1, "written": 1 }]);
            assert!(decode_manifest(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
        let mut invalid = valid.clone();
        invalid["schema_version"] = json!(2);
        assert!(decode_manifest(&serde_json::to_vec(&invalid).unwrap()).is_err());
        let record = RecordedFile {
            name: MediaName::Video,
            identity: FileIdentity::from(&snapshot()),
            bytes: 1,
            written: 1,
        };
        let mut duplicate = manifest;
        duplicate.files = vec![record.clone(), record];
        assert!(duplicate.validate().is_err());
        assert!(decode_manifest(&vec![b' '; MAX_MANIFEST_BYTES as usize + 1]).is_err());
        assert!(!staging_name(OsStr::new("../.rustshot-recording-x")));
        assert!(!staging_name(OsStr::new(".rustshot-recording-")));
        assert!(staging_name(OsStr::new(".rustshot-recording-x")));
    }

    #[test]
    fn recording_recovery_manifest_accepts_ten_fixed_names_and_six_attempts() {
        assert_eq!(MediaName::ALL.len(), 10);
        assert_eq!(MediaName::ENCODER_ATTEMPTS.len(), 6);
        for (index, name) in MediaName::ENCODER_ATTEMPTS.into_iter().enumerate() {
            let basename = format!("encoder-attempt-{index}.mp4");
            assert_eq!(name.basename(), basename);
            assert_eq!(serde_json::to_value(name).unwrap(), json!(basename));
            assert_eq!(MediaName::ALL[index + 4], name);
        }
        let mut manifest = Manifest::new(u64::MAX - RETENTION_SECONDS).unwrap();
        manifest.files = MediaName::ALL
            .into_iter()
            .map(|name| RecordedFile {
                name,
                identity: FileIdentity {
                    volume: u64::MAX,
                    id: [u8::MAX; 16],
                    created: u64::MAX,
                },
                bytes: u64::MAX,
                written: u64::MAX,
            })
            .collect();
        assert!(manifest.validate().is_ok());
        let encoded = serde_json::to_vec_pretty(&manifest).unwrap();
        assert!(encoded.len() as u64 <= MAX_MANIFEST_BYTES);
        assert_eq!(decode_manifest(&encoded).unwrap(), manifest);

        // 原先七个固定名称的 v1 清单继续接受；增加第十一项仍越过有界清单。
        let mut legacy = manifest.clone();
        legacy.files.truncate(7);
        assert_eq!(
            decode_manifest(&serde_json::to_vec(&legacy).unwrap()).unwrap(),
            legacy
        );
        manifest.files.push(manifest.files[0].clone());
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn recording_recovery_expiry_is_checked_and_does_not_grant_media_identity() {
        assert!(Manifest::new(u64::MAX - RETENTION_SECONDS + 1).is_err());
        let manifest = Manifest::new(u64::MAX - RETENTION_SECONDS).unwrap();
        assert_eq!(manifest.expires_unix_seconds, u64::MAX);
        assert!(manifest.validate().is_ok());
        let original = snapshot();
        let record = RecordedFile {
            name: MediaName::Video,
            identity: FileIdentity::from(&original),
            bytes: original.bytes,
            written: original.written,
        };
        let mut grown = original.clone();
        grown.bytes += 10;
        grown.written += 1;
        let found = MediaFinding::present(MediaName::Video, &grown, Some(&record));
        assert_eq!(found.identity_matches, Some(true));
        assert_eq!(found.content_unchanged, Some(false));
        grown.id[15] ^= 1;
        assert_eq!(
            MediaFinding::present(MediaName::Video, &grown, Some(&record)).identity_matches,
            Some(false)
        );
    }

    #[test]
    fn recording_recovery_discovery_limits_names_candidates_diagnostics_and_cancellation() {
        let directory = Path::new("library");
        let consumed = Cell::new(0);
        let mut report = ScanReport::default();
        let selected = select_candidates(
            directory,
            (0..MAX_DIRECTORY_ENTRIES + 10).map(|index| {
                consumed.set(consumed.get() + 1);
                Ok(format!("ordinary-{index}").into())
            }),
            &|| false,
            &mut report,
        );
        assert!(selected.is_empty());
        assert_eq!(consumed.get(), MAX_DIRECTORY_ENTRIES);
        assert!(report.truncated);

        let mut report = ScanReport::default();
        let selected = select_candidates(
            directory,
            (0..MAX_DIRECTORY_ENTRIES).map(|index| Ok(format!("{DIRECTORY_PREFIX}{index}").into())),
            &|| false,
            &mut report,
        );
        assert_eq!(selected.len(), MAX_CANDIDATES);
        assert_eq!(report.checked_candidates, MAX_CANDIDATES);
        assert_eq!(report.checked_entries, MAX_CANDIDATES + 1);
        assert!(report.truncated);
        for _ in 0..MAX_DIAGNOSTICS + 10 {
            report.note(directory, DiagnosticKind::DirectoryRead, "injected error");
        }
        assert_eq!(report.diagnostics.len(), MAX_DIAGNOSTICS);
        assert!(report.omitted_diagnostics >= 10);

        let consumed = Cell::new(0);
        let mut report = ScanReport::default();
        let selected = select_candidates(
            directory,
            std::iter::once_with(|| {
                consumed.set(1);
                Ok(format!("{DIRECTORY_PREFIX}never-read").into())
            }),
            &|| true,
            &mut report,
        );
        assert!(report.cancelled && selected.is_empty());
        assert_eq!(consumed.get(), 0);
    }

    #[test]
    fn recording_recovery_lease_blocks_discovery_until_release_and_cleanup_leaves_media() {
        let root = crate::config_json::TestDir::new("recording-recovery");
        let directory = root.0.join(".rustshot-recording-test");
        fs::create_dir(&directory).unwrap();
        let now = UNIX_EPOCH + std::time::Duration::from_secs(20);
        // 模拟 lease 已创建但初始清单尚未发布；scanner 不应打开或竞争该 lease。
        let unpublished = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(directory.join(LEASE_NAME))
            .unwrap();
        let initial = scan(&root.0, now, &|| false);
        assert_eq!(initial.active_skipped, 0);
        assert!(initial.sessions.is_empty());
        assert!(initial.diagnostics.iter().any(|note| {
            matches!(note.kind, DiagnosticKind::ManifestRead)
                && note.path == directory.join(MANIFEST_NAME)
        }));
        assert!(!initial
            .diagnostics
            .iter()
            .any(|note| matches!(note.kind, DiagnosticKind::LeaseCheck)));
        drop(unpublished);
        fs::remove_file(directory.join(LEASE_NAME)).unwrap();
        let mut session = Session::create_at(&directory, now).unwrap();
        let media = directory.join(MediaName::Video.basename());
        fs::write(&media, b"not validated media").unwrap();
        let snapshot = inspect_file(&media).unwrap().unwrap();
        session.register_file(MediaName::Video, &snapshot).unwrap();
        let mut writer = OpenOptions::new().append(true).open(&media).unwrap();
        writer.write_all(b"; same object grew").unwrap();
        drop(writer);
        let grown = inspect_file(&media).unwrap().unwrap();
        assert_eq!(FileIdentity::from(&snapshot), FileIdentity::from(&grown));
        session.register_file(MediaName::Video, &grown).unwrap();
        session.set_phase(Phase::Incomplete).unwrap();
        let active = scan(&root.0, now, &|| false);
        assert_eq!(active.active_skipped, 1);
        assert!(active.sessions.is_empty());
        drop(session);
        let found = scan(
            &root.0,
            now + std::time::Duration::from_secs(RETENTION_SECONDS),
            &|| false,
        );
        assert_eq!(found.sessions.len(), 1);
        let found = &found.sessions[0];
        assert!(found.expired && found.unverified && !found.lease_held);
        assert_eq!(found.media.len(), MediaName::ALL.len());
        assert_eq!(found.media[0].identity_matches, Some(true));
        // 另一会话不能以同名 lease 认领已有元数据。
        assert!(Session::create_at(&directory, now).is_err());

        let directory = root.0.join(".rustshot-recording-clean");
        fs::create_dir(&directory).unwrap();
        let mut session = Session::create_at(&directory, now).unwrap();
        let media = directory.join("video.mp4");
        fs::write(&media, b"keep").unwrap();
        let original = inspect_file(&media).unwrap().unwrap();
        session.register_file(MediaName::Video, &original).unwrap();
        fs::rename(&media, directory.join("original-kept.mp4")).unwrap();
        fs::write(&media, b"keep").unwrap();
        let replaced = inspect_file(&media).unwrap().unwrap();
        assert_ne!(FileIdentity::from(&original), FileIdentity::from(&replaced));
        assert!(session.register_file(MediaName::Video, &replaced).is_err());
        assert!(session.cleanup_metadata().is_empty());
        assert!(!directory.join(MANIFEST_NAME).exists() && !directory.join(LEASE_NAME).exists());
        assert_eq!(fs::read(media).unwrap(), b"keep");
        assert!(session.cleanup_metadata().is_empty());
    }

    #[test]
    fn recording_recovery_metadata_cleanup_preserves_replacements_and_unowned_atomic_temps() {
        let root = crate::config_json::TestDir::new("recording-recovery-metadata");
        let checked = root.0.join("checked-metadata.json");
        fs::write(&checked, b"owned metadata").unwrap();
        let expected = inspect_file(&checked).unwrap().unwrap();
        let writer = OpenOptions::new().write(true).open(&checked).unwrap();
        assert!(checked_metadata_for_removal(&checked, Some(&expected)).is_err());
        drop(writer);
        let file = checked_metadata_for_removal(&checked, Some(&expected))
            .unwrap()
            .unwrap();
        // 核验和删除之间仍持有同一句柄；不能改写，也不能先改名或移除再换入别的对象。
        assert!(OpenOptions::new().write(true).open(&checked).is_err());
        assert!(fs::rename(&checked, root.0.join("moved-metadata.json")).is_err());
        assert!(fs::remove_file(&checked).is_err());
        delete_metadata_handle(file).unwrap();
        assert!(!checked.exists());
        fs::write(&checked, b"replacement metadata").unwrap();
        assert!(remove_metadata(&checked, Some(&expected)).is_err());
        assert!(remove_metadata(&checked, None).is_err());
        assert_eq!(fs::read(&checked).unwrap(), b"replacement metadata");
        let directory = root.0.join(".rustshot-recording-test");
        fs::create_dir(&directory).unwrap();
        let mut session = Session::create(&directory).unwrap();
        let manifest = session.manifest_path();
        fs::rename(&manifest, directory.join("original-manifest-kept.json")).unwrap();
        fs::write(&manifest, b"foreign metadata").unwrap();
        let unowned = directory.join("recovery.json-unidentified.tmp");
        fs::write(&unowned, b"unknown").unwrap();
        let errors = session.cleanup_metadata();
        assert!(errors.iter().any(|error| error.path == manifest));
        assert!(errors.iter().any(|error| error.path == unowned));
        assert_eq!(fs::read(&manifest).unwrap(), b"foreign metadata");
        assert_eq!(fs::read(&unowned).unwrap(), b"unknown");
        assert!(!directory.join(LEASE_NAME).exists());
        assert!(session.cleanup_metadata().is_empty());
    }
}
