//! 新鲜完整回读后准备一个全新 MP4 副本；源租约与只读 guard 沿准备、等待和提交连续持有。
//! 发布和暂存清理由既有保存事务负责；不修复、不转码、不覆盖、不删除恢复源。

use super::{
    checkpoint, decode, readable_gate, supported, Issue, MediaName, ReadGuards, VerificationLimits,
    VerificationReport, VerificationStage, VerificationState,
};
use crate::recording_view::volume_save::{self, SaveRequest, StagedSave};
use serde::Serialize;
use std::{
    error::Error,
    ffi::OsStr,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Component, Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::Instant,
};
use windows::Win32::{
    Foundation::HANDLE,
    Globalization::{CompareStringOrdinal, CSTR_EQUAL},
    Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ID_INFO,
    },
};

const COPY_BUFFER_BYTES: usize = 256 * 1024;
const MAX_PATH_UNITS: usize = 32_767;
const COPY_SCOPE: &str = "fresh full system decode followed by a byte-for-byte copy from the same guarded source handle; no repair or transcoding; the prepared receipt retains the source lease/read-only guards and a canonical destination-parent guard until commit or drop; the parent handle and its path are rechecked before publication, without an arbitrary ancestor-path or external-filesystem CAS guarantee; total cooperative budget includes host waiting; checks occur between outer native/I/O calls and cannot interrupt a blocking call; final publication uses the existing private-temp-directory NoReplace transaction; a historical report does not authorize a separate export";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CopyStage {
    Request,
    Verification,
    Copy,
    PostCopy,
    Commit,
}

/// 原始错误只留在调用线程；crate::Result 的 dyn Error 不承诺 Send。
/// 宿主可在 worker 内用 into_parts 提取可发送的诊断，再释放原始 source。
#[derive(Debug)]
pub(crate) struct RecoveryCopyFailure {
    pub stage: CopyStage,
    pub verification: Box<VerificationReport>,
    pub copied_bytes: u64,
    pub reason: &'static str,
    pub message: String,
    source: Option<Box<dyn Error>>,
}

impl RecoveryCopyFailure {
    pub(crate) fn into_parts(
        self,
    ) -> (
        CopyStage,
        Box<VerificationReport>,
        u64,
        &'static str,
        String,
        Option<Box<dyn Error>>,
    ) {
        (
            self.stage,
            self.verification,
            self.copied_bytes,
            self.reason,
            self.message,
            self.source,
        )
    }
}

impl fmt::Display for RecoveryCopyFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.reason, self.message)
    }
}

impl Error for RecoveryCopyFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref()
    }
}

type CopyResult<T> = std::result::Result<T, RecoveryCopyFailure>;
type WorkResult<T> = std::result::Result<T, Problem>;

#[derive(Debug)]
struct Problem {
    stage: CopyStage,
    issue: Issue,
    source: Option<Box<dyn Error>>,
}

impl Problem {
    fn verification(issue: Issue) -> Self {
        Self {
            stage: CopyStage::Verification,
            issue,
            source: None,
        }
    }

    fn logical(stage: CopyStage, reason: &'static str, message: impl fmt::Display) -> Self {
        Self {
            stage,
            issue: Issue::failed(VerificationStage::Complete, reason, message),
            source: None,
        }
    }

    fn io(stage: CopyStage, reason: &'static str, error: io::Error) -> Self {
        Self {
            stage,
            issue: Issue::failed(VerificationStage::Complete, reason, &error),
            source: Some(Box::new(error)),
        }
    }

    fn boxed(stage: CopyStage, reason: &'static str, error: Box<dyn Error>) -> Self {
        Self {
            stage,
            issue: Issue::failed(VerificationStage::Complete, reason, &error),
            source: Some(error),
        }
    }

    fn with_transaction_error(mut self, transaction: Box<dyn Error>) -> Self {
        // 保存事务可能附加暂存清理诊断；保留它，同时不抹掉本模块最初的 I/O source。
        self.issue.message = format!("{}; save transaction: {transaction}", self.issue.message)
            .chars()
            .take(2048)
            .collect();
        self.source = Some(Box::new(TransactionError {
            primary: self.source.take(),
            transaction,
        }));
        self
    }

    fn finish(self, mut report: VerificationReport, copied_bytes: u64) -> RecoveryCopyFailure {
        let reason = self.issue.reason;
        let message = self.issue.message.clone();
        if self.issue.stage == VerificationStage::PostCheck {
            report.metadata_stable = false;
        }
        report.finish(Err(self.issue));
        report.lease_held = false;
        RecoveryCopyFailure {
            stage: self.stage,
            verification: Box::new(report),
            copied_bytes,
            reason,
            message,
            source: self.source,
        }
    }
}

struct TransactionError {
    primary: Option<Box<dyn Error>>,
    transaction: Box<dyn Error>,
}

impl fmt::Debug for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransactionError")
            .field("primary", &self.primary)
            .field("transaction", &self.transaction)
            .finish()
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(primary) = &self.primary {
            write!(formatter, "{primary}; ")?;
        }
        write!(formatter, "{}", self.transaction)
    }
}

impl Error for TransactionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.primary.as_deref().or(Some(self.transaction.as_ref()))
    }
}

/// 只有该回执持有连续源资格；report 本身不能用于另开一个复制/发布操作。
/// 默认字段析构先清自有 StagedSave，再释放源 guard，不实现另一套删除逻辑。
pub(crate) struct PreparedRecoveryCopy {
    staged: StagedSave,
    guards: ReadGuards,
    destination_guard: DestinationGuard,
    report: VerificationReport,
    destination: PathBuf,
    source_directory: PathBuf,
    stop: Arc<AtomicBool>,
    started: Instant,
    copied_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct CommittedRecoveryCopy {
    pub path: PathBuf,
    pub verification: VerificationReport,
    pub copied_bytes: u64,
    pub scope: &'static str,
}

impl PreparedRecoveryCopy {
    pub(crate) fn report(&self) -> &VerificationReport {
        &self.report
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    pub(crate) fn copied_bytes(&self) -> u64 {
        self.copied_bytes
    }

    /// 宿主须核对当前 job/会话和用户保存意图；迟到回执直接 Drop。
    pub(crate) fn commit(
        self,
        mut keep_current: impl FnMut() -> bool,
    ) -> CopyResult<CommittedRecoveryCopy> {
        let Self {
            staged,
            guards,
            destination_guard,
            mut report,
            destination,
            source_directory,
            stop,
            started,
            copied_bytes,
        } = self;
        let mut rejected = None;
        let published = staged.commit(|| {
            if rejected.is_some() {
                return false;
            }
            let result = (|| -> WorkResult<()> {
                check(&stop, started, report.limits, CopyStage::Commit)?;
                if !keep_current() {
                    return Err(stale_intent());
                }
                guards.verify_unchanged().map_err(|issue| Problem {
                    stage: CopyStage::Commit,
                    issue,
                    source: None,
                })?;
                recheck_destination(&source_directory, &destination, &destination_guard)?;
                check(&stop, started, report.limits, CopyStage::Commit)?;
                if !keep_current() {
                    return Err(stale_intent());
                }
                Ok(())
            })();
            match result {
                Ok(()) => true,
                Err(problem) => {
                    rejected = Some(problem);
                    false
                }
            }
        });
        // 所有会拒绝发布的校验都在 StagedSave.commit 的 NoReplace publish 之前。
        // 成功后不再因停止、源后验或清理诊断把已发布成品报成普通失败。
        drop(guards);
        drop(destination_guard);
        report.lease_held = false;
        report.elapsed_ms = elapsed_ms(started);
        match published {
            Ok(path) => Ok(CommittedRecoveryCopy {
                path,
                verification: report,
                copied_bytes,
                scope: COPY_SCOPE,
            }),
            Err(error) => {
                let problem = match rejected {
                    Some(problem) => problem.with_transaction_error(error),
                    None => Problem::boxed(CopyStage::Commit, "copy_publication_failed", error),
                };
                Err(problem.finish(report, copied_bytes))
            }
        }
    }
}

/// 只接受全新的绝对 .mp4 目标；每次自行建立 NoReplace 请求，不接受覆盖资格。
pub(crate) fn prepare(
    directory: &Path,
    name: MediaName,
    destination: &Path,
    stop: Arc<AtomicBool>,
    limits: VerificationLimits,
) -> CopyResult<PreparedRecoveryCopy> {
    let started = Instant::now();
    let mut report = VerificationReport::new(directory, name, limits);
    report.scope = COPY_SCOPE;
    let mut copied_bytes = 0;
    let outcome = prepare_inner(
        directory,
        name,
        destination,
        &stop,
        started,
        &mut report,
        &mut copied_bytes,
    );
    report.elapsed_ms = elapsed_ms(started);
    match outcome {
        Ok((staged, guards, destination, source_directory, destination_guard)) => {
            report.finish(Ok(()));
            report.lease_held = true;
            Ok(PreparedRecoveryCopy {
                staged,
                guards,
                destination_guard,
                report,
                destination,
                source_directory,
                stop,
                started,
                copied_bytes,
            })
        }
        // prepare_inner 的所有局部 guard 已释放；失败报告不声称继续持有租约。
        Err(problem) => Err(problem.finish(report, copied_bytes)),
    }
}

fn prepare_inner(
    directory: &Path,
    name: MediaName,
    destination: &Path,
    stop: &AtomicBool,
    started: Instant,
    report: &mut VerificationReport,
    copied_bytes: &mut u64,
) -> WorkResult<(StagedSave, ReadGuards, PathBuf, PathBuf, DestinationGuard)> {
    report.limits.validate().map_err(Problem::verification)?;
    check(stop, started, report.limits, CopyStage::Request)?;
    if !directory
        .file_name()
        .is_some_and(super::super::staging_name)
        || !supported(name)
    {
        return Err(Problem::logical(
            CopyStage::Request,
            "invalid_recovery_copy_source",
            "Only a registered video MP4 in a recovery staging directory can be copied",
        ));
    }
    validate_target_name(destination)?;
    let mut guards = ReadGuards::open(directory, name, report).map_err(Problem::verification)?;
    let source_directory = fs::canonicalize(directory)
        .map_err(|error| Problem::io(CopyStage::Request, "source_directory_resolution", error))?;
    let (destination, destination_guard) = resolve_destination(&source_directory, destination)?;
    // 不调用 from_dialog，不接收调用方提供的 SaveRequest 或任何已有目标覆盖资格。
    let request = SaveRequest::for_new_output(&destination)
        .map_err(|error| Problem::boxed(CopyStage::Request, "new_target_rejected", error))?;
    let decoded = check(stop, started, report.limits, CopyStage::Verification).and_then(|()| {
        decode(&guards.media_path, stop, started, report).map_err(Problem::verification)
    });
    let post = guards.verify_unchanged().map_err(Problem::verification);
    if let Err(problem) = post {
        report.metadata_stable = false;
        return Err(problem);
    }
    report.metadata_stable = true;
    decoded?;
    check(stop, started, report.limits, CopyStage::Verification)?;
    if !readable_gate(report) {
        return Err(Problem::logical(
            CopyStage::Verification,
            "incomplete_copy_verification",
            "Full video/audio EOF and stable registered identity were not verified",
        ));
    }
    let limits = report.limits;
    let expected_bytes = guards.media_before.bytes;
    let mut encode_problem = None;
    let staged = volume_save::prepare_with(
        &request,
        |staged_path| {
            let result = (|| -> WorkResult<()> {
                check(stop, started, limits, CopyStage::Copy)?;
                destination_guard.verify()?;
                guards.verify_unchanged().map_err(|issue| Problem {
                    stage: CopyStage::Copy,
                    issue,
                    source: None,
                })?;
                copy_to_staged(
                    &mut guards.media,
                    staged_path,
                    expected_bytes,
                    limits.max_file_bytes,
                    copied_bytes,
                    || check(stop, started, limits, CopyStage::Copy),
                )?;
                guards.verify_unchanged().map_err(|issue| Problem {
                    stage: CopyStage::PostCopy,
                    issue,
                    source: None,
                })?;
                check(stop, started, limits, CopyStage::PostCopy)
            })();
            match result {
                Ok(()) => Ok(()),
                Err(problem) => {
                    let message = problem.issue.message.clone();
                    encode_problem = Some(problem);
                    Err(io::Error::other(message).into())
                }
            }
        },
        || check(stop, started, limits, CopyStage::Copy).is_ok(),
    );
    let staged = match staged {
        Ok(staged) => staged,
        Err(error) => {
            let problem =
                encode_problem.or_else(|| check(stop, started, limits, CopyStage::Copy).err());
            return Err(match problem {
                Some(problem) => problem.with_transaction_error(error),
                None => Problem::boxed(CopyStage::Copy, "copy_staging_failed", error),
            });
        }
    };
    // prepare_with 已关闭并检查暂存 writer；这里不发布，回执继续持有同一组源 guard。
    guards.verify_unchanged().map_err(|issue| Problem {
        stage: CopyStage::PostCopy,
        issue,
        source: None,
    })?;
    recheck_destination(&source_directory, &destination, &destination_guard)?;
    check(stop, started, limits, CopyStage::PostCopy)?;
    if *copied_bytes != expected_bytes {
        return Err(Problem::logical(
            CopyStage::PostCopy,
            "copied_byte_count_mismatch",
            "The copied byte count does not match the guarded source",
        ));
    }
    Ok((
        staged,
        guards,
        destination,
        source_directory,
        destination_guard,
    ))
}

fn check(
    stop: &AtomicBool,
    started: Instant,
    limits: VerificationLimits,
    stage: CopyStage,
) -> WorkResult<()> {
    checkpoint(stop, started.elapsed(), limits, VerificationStage::Complete).map_err(|issue| {
        Problem {
            stage,
            issue,
            source: None,
        }
    })
}

fn stale_intent() -> Problem {
    Problem {
        stage: CopyStage::Commit,
        issue: Issue::new(
            VerificationState::Cancelled,
            VerificationStage::Complete,
            "copy_intent_no_longer_current",
            "The recovery-copy job or save intent is no longer current",
        ),
        source: None,
    }
}

fn copy_to_staged(
    source: &mut File,
    target: &Path,
    expected: u64,
    maximum: u64,
    copied: &mut u64,
    mut keep_going: impl FnMut() -> WorkResult<()>,
) -> WorkResult<()> {
    keep_going()?;
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(target)
        .map_err(|error| Problem::io(CopyStage::Copy, "staged_copy_create", error))?;
    copy_stream(
        source,
        &mut writer,
        expected,
        maximum,
        copied,
        &mut keep_going,
    )?;
    keep_going()?;
    writer
        .flush()
        .map_err(|error| Problem::io(CopyStage::Copy, "staged_copy_flush", error))?;
    keep_going()?;
    writer
        .sync_all()
        .map_err(|error| Problem::io(CopyStage::Copy, "staged_copy_sync", error))?;
    keep_going()?;
    drop(writer);
    Ok(())
}

fn copy_stream(
    source: &mut (impl Read + Seek),
    target: &mut impl Write,
    expected: u64,
    maximum: u64,
    copied: &mut u64,
    mut keep_going: impl FnMut() -> WorkResult<()>,
) -> WorkResult<()> {
    if expected == 0 || expected > maximum || *copied != 0 {
        return Err(Problem::logical(
            CopyStage::Copy,
            "invalid_copy_byte_budget",
            "The guarded source size or initial copy counter is invalid",
        ));
    }
    keep_going()?;
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| Problem::io(CopyStage::Copy, "source_copy_seek", error))?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(COPY_BUFFER_BYTES)
        .map_err(|error| Problem {
            stage: CopyStage::Copy,
            issue: Issue::failed(
                VerificationStage::Complete,
                "copy_buffer_allocation",
                &error,
            ),
            source: Some(Box::new(error)),
        })?;
    buffer.resize(COPY_BUFFER_BYTES, 0);
    loop {
        keep_going()?;
        // 最后只多读一个字节来确认 EOF；若超出已守住的大小，绝不把多余字节写入目标。
        let remaining = expected.checked_sub(*copied).ok_or_else(|| {
            Problem::logical(
                CopyStage::Copy,
                "copy_counter_overflow",
                "The copied count exceeds the guarded source size",
            )
        })?;
        let wanted = remaining.saturating_add(1).min(COPY_BUFFER_BYTES as u64) as usize;
        let read = source
            .read(&mut buffer[..wanted])
            .map_err(|error| Problem::io(CopyStage::Copy, "source_copy_read", error))?;
        keep_going()?;
        if read == 0 {
            break;
        }
        let next = copied.checked_add(read as u64).ok_or_else(|| {
            Problem::logical(
                CopyStage::Copy,
                "copy_counter_overflow",
                "The copy byte counter overflowed",
            )
        })?;
        if next > expected || next > maximum {
            return Err(Problem::logical(
                CopyStage::Copy,
                "source_size_changed",
                "Reading the guarded source exceeded its snapshotted size",
            ));
        }
        let mut written = 0;
        while written < read {
            keep_going()?;
            let count = target
                .write(&buffer[written..read])
                .map_err(|error| Problem::io(CopyStage::Copy, "staged_copy_write", error))?;
            if count == 0 || count > read - written {
                return Err(Problem::io(
                    CopyStage::Copy,
                    "staged_copy_write",
                    io::Error::new(
                        io::ErrorKind::WriteZero,
                        "The staged writer made no valid progress",
                    ),
                ));
            }
            written += count;
            *copied = copied.checked_add(count as u64).ok_or_else(|| {
                Problem::logical(
                    CopyStage::Copy,
                    "copy_counter_overflow",
                    "The copy byte counter overflowed",
                )
            })?;
            keep_going()?;
        }
        debug_assert_eq!(*copied, next);
    }
    if *copied != expected {
        return Err(Problem::logical(
            CopyStage::Copy,
            "copied_byte_count_mismatch",
            "The source reached EOF before its guarded byte count",
        ));
    }
    Ok(())
}

fn validate_target_name(target: &Path) -> WorkResult<()> {
    let valid = target.is_absolute()
        && target.as_os_str().encode_wide().count() <= MAX_PATH_UNITS
        && target.file_name().is_some_and(valid_mp4_basename);
    if !valid {
        return Err(Problem::logical(
            CopyStage::Request,
            "invalid_new_mp4_target",
            "The destination must be an absolute path with an ordinary new .mp4 file name",
        ));
    }
    Ok(())
}

fn valid_mp4_basename(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name.is_empty()
        || name.encode_utf16().count() > 255
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|value| value <= '\u{1f}' || "<>:\"/\\|?*".contains(value))
        || !Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        && !(stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
        && !["COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³"].contains(&stem.as_str())
}

fn staging_component(name: &OsStr) -> bool {
    // staging 前缀为 ASCII；按 Windows 大小写别名保守拒绝目标目录。
    let folded = name.to_string_lossy().to_ascii_lowercase();
    super::super::staging_name(OsStr::new(&folded))
}

#[derive(Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    volume: u64,
    id: [u8; 16],
    created: u64,
}

/// 普通目录预检及不共享 DELETE 的句柄；本事务创建子目录会改变父目录写时间。
struct DestinationGuard {
    path: PathBuf,
    file: File,
    identity: DirectoryIdentity,
}

impl DestinationGuard {
    fn verify(&self) -> WorkResult<()> {
        if directory_identity(&self.file)? != self.identity {
            return Err(Problem::logical(
                CopyStage::Commit,
                "target_parent_handle_changed",
                "The guarded destination parent identity changed",
            ));
        }
        let by_path = super::super::open_directory(&self.path)
            .map_err(|error| Problem::boxed(CopyStage::Commit, "unsafe_target_parent", error))?;
        if directory_identity(&by_path)? != self.identity {
            return Err(Problem::logical(
                CopyStage::Commit,
                "target_parent_identity_changed",
                "The destination parent path no longer names the guarded directory",
            ));
        }
        let canonical = fs::canonicalize(&self.path)
            .map_err(|error| Problem::io(CopyStage::Commit, "target_parent_resolution", error))?;
        if !windows_equal(canonical.as_os_str(), self.path.as_os_str()) {
            return Err(Problem::logical(
                CopyStage::Commit,
                "target_parent_changed",
                "The destination parent no longer resolves to its prepared path",
            ));
        }
        Ok(())
    }
}

fn directory_identity(file: &File) -> WorkResult<DirectoryIdentity> {
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(|error| {
        Problem::boxed(CopyStage::Commit, "target_parent_snapshot", error.into())
    })?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || info.dwFileAttributes & super::super::UNSAFE_ATTRIBUTES != 0
    {
        return Err(Problem::logical(
            CopyStage::Commit,
            "unsafe_target_parent",
            "The guarded destination parent is not an ordinary available directory",
        ));
    }
    let mut identity = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut identity as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|error| Problem::boxed(CopyStage::Commit, "target_parent_identity", error.into()))?;
    Ok(DirectoryIdentity {
        volume: identity.VolumeSerialNumber,
        id: identity.FileId.Identifier,
        created: u64::from(info.ftCreationTime.dwHighDateTime) << 32
            | u64::from(info.ftCreationTime.dwLowDateTime),
    })
}

fn resolve_destination(
    source_directory: &Path,
    target: &Path,
) -> WorkResult<(PathBuf, DestinationGuard)> {
    validate_target_name(target)?;
    let parent = target.parent().ok_or_else(|| {
        Problem::logical(
            CopyStage::Request,
            "missing_target_parent",
            "The destination has no parent directory",
        )
    })?;
    let parent_guard = super::super::open_directory(parent)
        .map_err(|error| Problem::boxed(CopyStage::Request, "unsafe_target_parent", error))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| Problem::io(CopyStage::Request, "target_parent_resolution", error))?;
    let canonical = canonical_parent.join(target.file_name().expect("validated file name"));
    check_destination_location(source_directory, &canonical)?;
    ensure_target_absent(&canonical, CopyStage::Request)?;
    // canonicalize 前已有普通目录预检；规范化之后再次拒绝不安全目录，不读取其它内容。
    let canonical_guard = super::super::open_directory(&canonical_parent).map_err(|error| {
        Problem::boxed(CopyStage::Request, "unsafe_canonical_target_parent", error)
    })?;
    let identity = directory_identity(&parent_guard)?;
    if directory_identity(&canonical_guard)? != identity {
        return Err(Problem::logical(
            CopyStage::Request,
            "target_parent_changed",
            "The destination parent changed while resolving its canonical path",
        ));
    }
    drop(parent_guard);
    Ok((
        canonical,
        DestinationGuard {
            path: canonical_parent,
            file: canonical_guard,
            identity,
        },
    ))
}

fn recheck_destination(
    source_directory: &Path,
    target: &Path,
    guard: &DestinationGuard,
) -> WorkResult<()> {
    guard.verify()?;
    check_destination_location(source_directory, target)?;
    ensure_target_absent(target, CopyStage::Commit)?;
    if !target
        .parent()
        .is_some_and(|parent| windows_equal(parent.as_os_str(), guard.path.as_os_str()))
    {
        return Err(Problem::logical(
            CopyStage::Commit,
            "target_parent_changed",
            "The destination does not have the guarded canonical parent",
        ));
    }
    Ok(())
}

fn ensure_target_absent(target: &Path, stage: CopyStage) -> WorkResult<()> {
    // 已存在就拒绝，无需打开既有目标（也不为检查新目标资格读取云占位内容）。
    match fs::symlink_metadata(target) {
        Ok(_) => Err(Problem::logical(
            stage,
            "new_target_exists",
            "The recovery-copy destination already exists and cannot be overwritten",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Problem::io(stage, "new_target_metadata", error)),
    }
}

fn check_destination_location(source_directory: &Path, target: &Path) -> WorkResult<()> {
    if windows_within(target, source_directory)
        || target.parent().is_some_and(|parent| {
            parent.components().any(
                |component| matches!(component, Component::Normal(name) if staging_component(name)),
            )
        })
    {
        return Err(Problem::logical(
            CopyStage::Request,
            "target_inside_recovery_staging",
            "A recovery copy must be outside all recovery staging directories",
        ));
    }
    Ok(())
}

fn windows_equal(left: &OsStr, right: &OsStr) -> bool {
    let left: Vec<u16> = left.encode_wide().collect();
    let right: Vec<u16> = right.encode_wide().collect();
    unsafe { CompareStringOrdinal(&left, &right, true) == CSTR_EQUAL }
}

fn windows_within(path: &Path, directory: &Path) -> bool {
    let path: Vec<u16> = path.as_os_str().encode_wide().collect();
    let directory: Vec<u16> = directory.as_os_str().encode_wide().collect();
    let mut length = directory.len();
    while length > 0 && matches!(directory[length - 1], 47 | 92) {
        length -= 1;
    }
    length > 0
        && path.len() >= length
        && unsafe {
            CompareStringOrdinal(&path[..length], &directory[..length], true) == CSTR_EQUAL
        }
        && (path.len() == length || matches!(path[length], 47 | 92))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// 回执跨线程只携带文件 guard；所有 COM reader 在 prepare_inner 返回前已析构。
fn _prepared_receipt_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<PreparedRecoveryCopy>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn recording_recovery_copy_target_names_and_staging_locations_are_restricted() {
        assert!(valid_mp4_basename(OsStr::new("录像.MP4")));
        for name in [
            "clip.gif",
            "clip.mp4.",
            "clip.mp4 ",
            "video:stream.mp4",
            "NUL.mp4",
            "COM1.mp4",
            "LPT¹.mp4",
        ] {
            assert!(!valid_mp4_basename(OsStr::new(name)), "{name}");
        }
        let source = Path::new(r"C:\work\.rustshot-recording-original");
        assert!(check_destination_location(
            source,
            Path::new(r"c:\WORK\.RUSTSHOT-RECORDING-original\copy.mp4")
        )
        .is_err());
        assert!(check_destination_location(
            source,
            Path::new(r"C:\other\.rustshot-recording-else\copy.mp4")
        )
        .is_err());
        assert!(check_destination_location(source, Path::new(r"C:\work\copy.mp4")).is_ok());
        assert!(!windows_within(
            Path::new(r"C:\work\abc-extra\file.mp4"),
            Path::new(r"c:\WORK\abc")
        ));
    }

    #[test]
    fn recording_recovery_copy_stream_is_exact_bounded_and_does_not_append_excess_bytes() {
        let bytes: Vec<u8> = (0..COPY_BUFFER_BYTES + 9)
            .map(|index| index as u8)
            .collect();
        let mut source = Cursor::new(bytes.clone());
        source.set_position(7);
        let mut output = Vec::new();
        let mut copied = 0;
        assert!(copy_stream(
            &mut source,
            &mut output,
            bytes.len() as u64,
            bytes.len() as u64,
            &mut copied,
            || Ok(())
        )
        .is_ok());
        assert_eq!(output, bytes);
        assert_eq!(copied, bytes.len() as u64);
        let mut output = Vec::new();
        let mut copied = 0;
        assert!(copy_stream(
            &mut Cursor::new(b"extra"),
            &mut output,
            4,
            4,
            &mut copied,
            || Ok(())
        )
        .is_err());
        assert!(output.is_empty());
        let mut copied = 0;
        assert!(copy_stream(
            &mut Cursor::new(b"short"),
            &mut output,
            6,
            6,
            &mut copied,
            || Ok(())
        )
        .is_err());
        assert_eq!(copied, 5);

        struct PartialWriter(Vec<u8>);
        impl Write for PartialWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0.len() == 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "short-write model failure",
                    ));
                }
                let count = bytes.len().min(2);
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = PartialWriter(Vec::new());
        let mut copied = 0;
        assert!(copy_stream(
            &mut Cursor::new(b"data"),
            &mut writer,
            4,
            4,
            &mut copied,
            || Ok(())
        )
        .is_err());
        assert_eq!(copied, 2);
        assert_eq!(writer.0, b"da");
    }

    #[test]
    fn recording_recovery_copy_cancellation_after_read_stops_before_write_and_keeps_typed_report() {
        let mut checks = 0;
        let mut output = Vec::new();
        let mut copied = 0;
        let result = copy_stream(
            &mut Cursor::new(b"bytes"),
            &mut output,
            5,
            5,
            &mut copied,
            || {
                checks += 1;
                if checks >= 3 {
                    Err(stale_intent())
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert!(output.is_empty());
        assert_eq!(copied, 0);
        let failed = match prepare(
            Path::new("not-a-staging-directory"),
            MediaName::Video,
            Path::new("relative.mp4"),
            Arc::new(AtomicBool::new(true)),
            VerificationLimits::default(),
        ) {
            Ok(_) => panic!("pre-cancelled copy unexpectedly prepared"),
            Err(failure) => failure,
        };
        assert_eq!(failed.verification.state, VerificationState::Cancelled);
        assert!(!failed.verification.lease_held);
    }

    #[test]
    fn recording_recovery_copy_failure_keeps_original_source_for_worker_diagnostics() {
        let report = VerificationReport::new(
            Path::new("unused"),
            MediaName::Video,
            VerificationLimits::default(),
        );
        let failure = Problem::io(
            CopyStage::Copy,
            "read",
            io::Error::new(io::ErrorKind::PermissionDenied, "original read error"),
        )
        .with_transaction_error(io::Error::other("transaction cleanup detail").into())
        .finish(report, 0);
        let transaction = failure.source().unwrap();
        assert_eq!(
            transaction
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(failure.message.contains("transaction cleanup detail"));
        let (_, report, _, _, _, source) = failure.into_parts();
        assert!(!report.lease_held);
        assert!(source.is_some());
    }

    struct FileModelDirectory(PathBuf);

    impl FileModelDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::path::absolute("target/recording-recovery-copy-model").unwrap();
            fs::create_dir_all(&root).unwrap();
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = root.join(format!(
                "{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for FileModelDirectory {
        fn drop(&mut self) {
            // 只清模型测试自己创建的已知文件；未知内容不会递归删除。
            let _ = fs::remove_file(self.0.join("source.mp4"));
            let _ = fs::remove_file(self.0.join("copy.mp4"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn recording_recovery_copy_reuses_no_replace_transaction_and_drop_keeps_source() {
        let directory = FileModelDirectory::new();
        let source = directory.0.join("source.mp4");
        let target = directory.0.join("copy.mp4");
        // 仅文件事务模型字节，不声称它是可解码的 MP4 或完整恢复验证。
        fs::write(&source, b"original model bytes").unwrap();
        let request = SaveRequest::for_new_output(&target).unwrap();
        let staged = volume_save::prepare_with(
            &request,
            |path| {
                fs::write(path, b"new model bytes")?;
                Ok(())
            },
            || true,
        )
        .unwrap();
        drop(staged);
        assert!(!target.exists());
        assert_eq!(fs::read(&source).unwrap(), b"original model bytes");
        let staged = volume_save::prepare_with(
            &request,
            |path| {
                fs::write(path, b"new model bytes")?;
                Ok(())
            },
            || true,
        )
        .unwrap();
        fs::write(&target, b"race winner").unwrap();
        assert!(staged.commit(|| true).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"race winner");
        assert_eq!(fs::read(&source).unwrap(), b"original model bytes");
        assert!(SaveRequest::for_new_output(&target).is_err());
    }

    struct RegisteredModelSource(PathBuf);

    impl Drop for RegisteredModelSource {
        fn drop(&mut self) {
            // 只涉及本测试独占目录内的三个已知自有文件；不递归清理未知内容。
            for name in ["video.mp4", "recovery.json", "recovery.lease"] {
                let _ = fs::remove_file(self.0.join(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    fn model_receipt(root: &FileModelDirectory) -> (RegisteredModelSource, PreparedRecoveryCopy) {
        use super::super::super::Session;
        let source = RegisteredModelSource(root.0.join(".rustshot-recording-copy-model"));
        fs::create_dir(&source.0).unwrap();
        let media = source.0.join("video.mp4");
        let mut session = Session::create(&source.0).unwrap();
        fs::write(&media, b"guarded source model bytes").unwrap();
        let snapshot = crate::file_snapshot::inspect(&media).unwrap().unwrap();
        session.register_file(MediaName::Video, &snapshot).unwrap();
        drop(session);
        let mut report =
            VerificationReport::new(&source.0, MediaName::Video, VerificationLimits::default());
        let mut guards = ReadGuards::open(&source.0, MediaName::Video, &mut report).unwrap();
        let source_directory = fs::canonicalize(&source.0).unwrap();
        let (destination, destination_guard) =
            resolve_destination(&source_directory, &root.0.join("copy.mp4")).unwrap();
        let request = SaveRequest::for_new_output(&destination).unwrap();
        let mut copied_bytes = 0;
        let staged = volume_save::prepare_with(
            &request,
            |path| {
                copy_to_staged(
                    &mut guards.media,
                    path,
                    snapshot.bytes,
                    report.limits.max_file_bytes,
                    &mut copied_bytes,
                    || Ok(()),
                )
                .map_err(|problem| -> Box<dyn Error> {
                    io::Error::other(problem.issue.message).into()
                })
            },
            || true,
        )
        .unwrap();
        // 明确的文件事务模型：只模拟媒体门已通过，完全没有调用 decode，也不产生验证报告文件。
        report.metadata_stable = true;
        report.video_frames = 1;
        report.video_eof = true;
        report.audio_presence = super::super::AudioPresence::Absent;
        report.audio_eof = true;
        report.finish(Ok(()));
        report.lease_held = true;
        report.scope = "file-transaction model only; no native media decode was performed";
        (
            source,
            PreparedRecoveryCopy {
                staged,
                guards,
                destination_guard,
                report,
                destination,
                source_directory,
                stop: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
                copied_bytes,
            },
        )
    }

    fn assert_model_source_released(source: &RegisteredModelSource) {
        let lease =
            super::super::super::open_checked_file(&source.0.join("recovery.lease")).unwrap();
        lease.try_lock().unwrap();
        drop(lease);
        // 只打开验证共享权限，不向源文件写入任何字节。
        drop(
            OpenOptions::new()
                .write(true)
                .open(source.0.join("video.mp4"))
                .unwrap(),
        );
        assert_eq!(
            fs::read(source.0.join("video.mp4")).unwrap(),
            b"guarded source model bytes"
        );
    }

    #[test]
    fn recording_recovery_copy_receipt_file_model_retains_guards_and_releases_on_all_outcomes() {
        for outcome in ["drop", "stale", "cancel", "expired", "commit"] {
            let root = FileModelDirectory::new();
            let (source, mut prepared) = model_receipt(&root);
            assert!(prepared.report().lease_held);
            assert_eq!(
                prepared.copied_bytes(),
                b"guarded source model bytes".len() as u64
            );
            let target = prepared.destination().to_owned();
            let media = source.0.join("video.mp4");
            assert!(OpenOptions::new().write(true).open(&media).is_err());
            assert!(fs::rename(&media, source.0.join("moved.mp4")).is_err());
            let lease =
                super::super::super::open_checked_file(&source.0.join("recovery.lease")).unwrap();
            assert!(matches!(
                lease.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            drop(lease);
            let temporary = fs::read_dir(&root.0)
                .unwrap()
                .filter_map(|entry| {
                    let path = entry.unwrap().path();
                    path.file_name()
                        .is_some_and(|name| {
                            name.to_string_lossy().starts_with(".rustshot-volume-save-")
                        })
                        .then_some(path)
                })
                .collect::<Vec<_>>();
            assert_eq!(temporary.len(), 1);
            match outcome {
                "drop" => drop(prepared),
                "stale" => {
                    let failed = prepared
                        .commit(|| false)
                        .err()
                        .expect("stale receipt published");
                    assert_eq!(failed.verification.state, VerificationState::Cancelled);
                    assert!(!failed.verification.lease_held);
                }
                "cancel" => {
                    prepared.stop.store(true, Ordering::Release);
                    let failed = prepared
                        .commit(|| true)
                        .err()
                        .expect("cancelled receipt published");
                    assert_eq!(failed.verification.state, VerificationState::Cancelled);
                    assert!(!failed.verification.lease_held);
                }
                "expired" => {
                    prepared.started = Instant::now()
                        .checked_sub(prepared.report.limits.max_elapsed)
                        .unwrap();
                    let failed = prepared
                        .commit(|| true)
                        .err()
                        .expect("expired receipt published");
                    assert_eq!(failed.verification.state, VerificationState::Limited);
                    assert!(!failed.verification.lease_held);
                }
                "commit" => {
                    let committed = prepared.commit(|| true).unwrap();
                    assert_eq!(committed.path, target);
                    assert!(!committed.verification.lease_held);
                    assert_eq!(fs::read(&target).unwrap(), b"guarded source model bytes");
                }
                _ => unreachable!(),
            }
            assert_eq!(target.exists(), outcome == "commit");
            assert!(temporary.iter().all(|path| !path.exists()));
            assert_model_source_released(&source);
        }
    }
}
