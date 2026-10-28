//! 登记过的录制 MP4 的有界只读回读；结论不授予稍后导出或删除的资格。
//! 所有 reader 与 MfRuntime 都留在调用线程，不保存解码后的像素或 PCM。
#[path = "recording_recovery_copy.rs"]
pub(crate) mod copy;

#[cfg(test)]
#[path = "recording_recovery_verify_native.rs"]
mod native_tests;

use super::{
    attributes_safe, decode_manifest, open_checked_file, open_directory, verify_metadata,
    FileIdentity, MediaName, RecordedFile, LEASE_NAME, MANIFEST_NAME, MAX_MANIFEST_BYTES,
    UNSAFE_ATTRIBUTES,
};
use crate::{
    file_snapshot::{self, FileSnapshot},
    media_foundation::{AudioReader, MfRuntime, VideoReader},
};
use serde::Serialize;
use std::{
    fmt::Display,
    fs::{self, File, OpenOptions, TryLockError},
    io::Read,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
// AudioReader 限制原始块为 4 MiB；PCM16 转成返回的 f32 Vec 时最多翻倍。
// 这里只在外层 next 前后检查预算，不声称逐次内部 native call 检查。
const MAX_AUDIO_BLOCK_BYTES: u64 = 8 * MIB;
const MAX_AUDIO_BLOCK_VALUES: u64 = MAX_AUDIO_BLOCK_BYTES / 4;
const SCOPE: &str = "system decode of the selected video and first audio stream through EOF; timestamps are reader-normalized; no original-float validity or audio/video synchronization claim; cooperative limits are checked between outer calls, video next also checks cancellation internally, audio next has at most 64 native reads; blocking native calls cannot be interrupted; non-verified outcomes are not permanent recoverability verdicts; later export must reacquire the lease and recheck identity";

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct VerificationLimits {
    pub max_elapsed: Duration,
    pub max_file_bytes: u64,
    pub max_video_frames: u64,
    pub max_decoded_bytes: u64,
    /// 交错 f32 值的数量，并非按声道合并后的音频帧数。
    pub max_audio_sample_values: u64,
    pub max_media_time: Duration,
    pub max_frame_pixels: u64,
    pub max_dimension: u32,
}

impl Default for VerificationLimits {
    fn default() -> Self {
        Self {
            max_elapsed: Duration::from_secs(30),
            max_file_bytes: 4 * GIB,
            max_video_frames: 108_000,
            max_decoded_bytes: 64 * GIB,
            max_audio_sample_values: 345_600_000,
            max_media_time: Duration::from_secs(60 * 60),
            max_frame_pixels: 32_000_000,
            max_dimension: 8192,
        }
    }
}

impl VerificationLimits {
    fn validate(self) -> CheckResult<()> {
        if self.max_elapsed.is_zero()
            || self.max_elapsed > Duration::from_secs(300)
            || self.max_file_bytes == 0
            || self.max_file_bytes > 32 * GIB
            || self.max_video_frames == 0
            || self.max_video_frames > 1_000_000
            || self.max_decoded_bytes == 0
            || self.max_decoded_bytes > 512 * GIB
            || self.max_audio_sample_values == 0
            || self.max_audio_sample_values > 2_000_000_000
            || self.max_media_time.is_zero()
            || self.max_media_time > Duration::from_secs(24 * 60 * 60)
            || self.max_frame_pixels == 0
            || self.max_frame_pixels > 32_000_000
            || self.max_dimension == 0
            || self.max_dimension > 8192
        {
            return Err(Issue::failed(
                VerificationStage::Request,
                "invalid_limits",
                "Verification limits are zero or exceed their finite hard caps",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationState {
    VerifiedReadable,
    CurrentBackendCannotVerify,
    Limited,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationStage {
    Request,
    Directory,
    Lease,
    Manifest,
    MediaGuard,
    MediaIdentity,
    Runtime,
    VideoOpen,
    VideoDecode,
    AudioOpen,
    AudioDecode,
    PostCheck,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AudioPresence {
    Unknown,
    Absent,
    Present,
}

#[derive(Debug, Serialize)]
pub(crate) struct VerificationReport {
    pub directory: PathBuf,
    pub name: MediaName,
    pub state: VerificationState,
    pub stage: VerificationStage,
    pub reason: Option<&'static str>,
    pub error: Option<String>,
    pub identity: Option<FileIdentity>,
    pub bytes: Option<u64>,
    pub written: Option<u64>,
    pub registered_bytes: Option<u64>,
    pub registered_written: Option<u64>,
    pub registration_content_unchanged: Option<bool>,
    pub registered_identity_matches: bool,
    pub metadata_stable: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codec: Option<String>,
    pub declared_frame_rate: Option<(u32, u32)>,
    pub declared_duration_ns: Option<u64>,
    pub video_frames: u64,
    pub video_first_pts_ns: Option<u64>,
    pub video_last_pts_ns: Option<u64>,
    pub video_eof: bool,
    pub audio_presence: AudioPresence,
    pub audio_rate: Option<u32>,
    pub audio_channels: Option<usize>,
    pub audio_blocks: u64,
    pub audio_frames: u64,
    pub audio_sample_values: u64,
    pub audio_first_pts_ns: Option<u64>,
    pub audio_last_pts_ns: Option<u64>,
    pub audio_max_end_ns: Option<u64>,
    pub audio_eof: bool,
    pub decoded_bytes: u64,
    pub elapsed_ms: u64,
    pub limits: VerificationLimits,
    /// 独立 verify 返回 false；副本准备回执存活时为 true，提交结束后为 false。
    /// 历史报告本身不授予另一次操作的资格。
    pub lease_held: bool,
    pub subsequent_export_requires_new_lease_and_identity: bool,
    pub scope: &'static str,
}

impl VerificationReport {
    fn new(directory: &Path, name: MediaName, limits: VerificationLimits) -> Self {
        Self {
            directory: directory.to_owned(),
            name,
            state: VerificationState::Failed,
            stage: VerificationStage::Request,
            reason: None,
            error: None,
            identity: None,
            bytes: None,
            written: None,
            registered_bytes: None,
            registered_written: None,
            registration_content_unchanged: None,
            registered_identity_matches: false,
            metadata_stable: false,
            width: None,
            height: None,
            codec: None,
            declared_frame_rate: None,
            declared_duration_ns: None,
            video_frames: 0,
            video_first_pts_ns: None,
            video_last_pts_ns: None,
            video_eof: false,
            audio_presence: AudioPresence::Unknown,
            audio_rate: None,
            audio_channels: None,
            audio_blocks: 0,
            audio_frames: 0,
            audio_sample_values: 0,
            audio_first_pts_ns: None,
            audio_last_pts_ns: None,
            audio_max_end_ns: None,
            audio_eof: false,
            decoded_bytes: 0,
            elapsed_ms: 0,
            limits,
            lease_held: false,
            subsequent_export_requires_new_lease_and_identity: true,
            scope: SCOPE,
        }
    }

    fn finish(&mut self, result: CheckResult<()>) {
        match result {
            Ok(()) if readable_gate(self) => {
                self.state = VerificationState::VerifiedReadable;
                self.stage = VerificationStage::Complete;
            }
            Ok(()) => self.finish(Err(Issue::failed(
                VerificationStage::Complete,
                "incomplete_verification",
                "The full-read and stable-identity requirements were not satisfied",
            ))),
            Err(issue) => {
                self.state = issue.state;
                self.stage = issue.stage;
                self.reason = Some(issue.reason);
                self.error = Some(issue.message);
            }
        }
    }
}

#[derive(Debug)]
struct Issue {
    state: VerificationState,
    stage: VerificationStage,
    reason: &'static str,
    message: String,
}
type CheckResult<T> = std::result::Result<T, Issue>;

impl Issue {
    fn new(
        state: VerificationState,
        stage: VerificationStage,
        reason: &'static str,
        message: impl Display,
    ) -> Self {
        // 系统错误只用于诊断，报告不无限收集来自媒体或后端的文本。
        let message: String = message.to_string().chars().take(2048).collect();
        Self {
            state,
            stage,
            reason,
            message,
        }
    }

    fn failed(stage: VerificationStage, reason: &'static str, message: impl Display) -> Self {
        Self::new(VerificationState::Failed, stage, reason, message)
    }

    fn backend(stage: VerificationStage, error: impl Display) -> Self {
        Self::new(
            VerificationState::CurrentBackendCannotVerify,
            stage,
            "current_backend_cannot_verify",
            error,
        )
    }

    fn limited(stage: VerificationStage, reason: &'static str) -> Self {
        Self::new(VerificationState::Limited, stage, reason, reason)
    }
}

/// 不修复或保存；调用方在后台线程同步调用，原生调用本身不能被强制中断。
pub(crate) fn verify(
    directory: &Path,
    name: MediaName,
    stop: &AtomicBool,
    limits: VerificationLimits,
) -> VerificationReport {
    let started = Instant::now();
    let mut report = VerificationReport::new(directory, name, limits);
    let result = verify_inner(directory, name, stop, started, &mut report);
    // verify_inner 已返回，租约、媒体 guard、所有 reader 和 MfRuntime 均已释放。
    report.finish(result);
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    report
}

fn verify_inner(
    directory: &Path,
    name: MediaName,
    stop: &AtomicBool,
    started: Instant,
    report: &mut VerificationReport,
) -> CheckResult<()> {
    report.limits.validate()?;
    checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::Request,
    )?;
    if !directory.file_name().is_some_and(super::staging_name) {
        return Err(Issue::failed(
            VerificationStage::Request,
            "invalid_staging_directory_name",
            "The recovery directory name does not match the staging-directory contract",
        ));
    }
    if !supported(name) {
        return Err(Issue::failed(
            VerificationStage::Request,
            "unsupported_media_name",
            "Only registered video and encoder-attempt MP4 files are supported",
        ));
    }
    let guards = ReadGuards::open(directory, name, report)?;
    let decoded = checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::Runtime,
    )
    .and_then(|()| decode(&guards.media_path, stop, started, report));
    // 即使解码失败、预算用尽或取消，也核验已持有对象；任何变化都不能得到 Verified。
    let post = guards.verify_unchanged();
    if let Err(mut issue) = post {
        if let Err(previous) = decoded {
            issue.message = format!(
                "{}; earlier {}: {}",
                issue.message, previous.reason, previous.message
            )
            .chars()
            .take(2048)
            .collect();
        }
        return Err(issue);
    }
    report.metadata_stable = true;
    decoded?;
    checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::Complete,
    )
}

fn supported(name: MediaName) -> bool {
    name == MediaName::Video || MediaName::ENCODER_ATTEMPTS.contains(&name)
}

struct ReadGuards {
    directory_path: PathBuf,
    directory: File,
    lease_path: PathBuf,
    lease: File,
    lease_before: FileSnapshot,
    manifest_path: PathBuf,
    manifest: File,
    manifest_before: FileSnapshot,
    media_path: PathBuf,
    media: File,
    media_before: FileSnapshot,
}

impl ReadGuards {
    fn open(
        directory_path: &Path,
        name: MediaName,
        report: &mut VerificationReport,
    ) -> CheckResult<Self> {
        let directory = open_directory(directory_path).map_err(|error| {
            Issue::failed(VerificationStage::Directory, "unsafe_directory", error)
        })?;
        // Session 先建立 lease、取得排他锁，才发布初始 manifest；预检避免在它
        // 尚未上锁的创建间隙抢到共享锁，导致正常录制启动失败。锁内仍重新读取。
        let manifest_path = directory_path.join(MANIFEST_NAME);
        let manifest_metadata = fs::symlink_metadata(&manifest_path).map_err(|error| {
            Issue::failed(
                VerificationStage::Manifest,
                "manifest_missing_or_unsafe",
                error,
            )
        })?;
        if !manifest_metadata.is_file() || !attributes_safe(&manifest_metadata) {
            return Err(Issue::failed(
                VerificationStage::Manifest,
                "manifest_missing_or_unsafe",
                "The recovery manifest is not an ordinary available local file",
            ));
        }
        let lease_path = directory_path.join(LEASE_NAME);
        let lease = open_checked_file(&lease_path).map_err(|error| {
            Issue::failed(VerificationStage::Lease, "lease_missing_or_unsafe", error)
        })?;
        match lease.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Issue::failed(
                    VerificationStage::Lease,
                    "active_session",
                    "The recovery lease is held by an active session",
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(Issue::failed(
                    VerificationStage::Lease,
                    "lease_lock_failed",
                    error,
                ));
            }
        }
        let lease_before = file_snapshot::snapshot(&lease)
            .map_err(|error| Issue::failed(VerificationStage::Lease, "lease_snapshot", error))?;
        let mut manifest_file = open_checked_file(&manifest_path).map_err(|error| {
            Issue::failed(
                VerificationStage::Manifest,
                "manifest_missing_or_unsafe",
                error,
            )
        })?;
        let manifest_before = file_snapshot::snapshot(&manifest_file).map_err(|error| {
            Issue::failed(VerificationStage::Manifest, "manifest_snapshot", error)
        })?;
        if manifest_before.bytes > MAX_MANIFEST_BYTES {
            return Err(Issue::failed(
                VerificationStage::Manifest,
                "manifest_too_large",
                "The recovery manifest exceeds 16 KiB",
            ));
        }
        let mut bytes = Vec::with_capacity(MAX_MANIFEST_BYTES as usize + 1);
        (&mut manifest_file)
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                Issue::failed(VerificationStage::Manifest, "manifest_read_failed", error)
            })?;
        let manifest = decode_manifest(&bytes).map_err(|error| {
            Issue::failed(VerificationStage::Manifest, "manifest_invalid", error)
        })?;
        stable_file(&manifest_file, &manifest_path, &manifest_before)?;
        let registered = manifest
            .files
            .iter()
            .find(|file| file.name == name)
            .ok_or_else(|| {
                Issue::failed(
                    VerificationStage::MediaIdentity,
                    "media_not_registered",
                    "The requested media has no registered file identity",
                )
            })?;
        report.registered_bytes = Some(registered.bytes);
        report.registered_written = Some(registered.written);
        let media_path = directory_path.join(name.basename());
        let metadata = fs::symlink_metadata(&media_path).map_err(|error| {
            Issue::failed(VerificationStage::MediaGuard, "media_metadata", error)
        })?;
        if !metadata.is_file() || !attributes_safe(&metadata) {
            return Err(Issue::failed(
                VerificationStage::MediaGuard,
                "unsafe_media",
                "The media is not an ordinary available local file",
            ));
        }
        // 同句柄持续持有到回读和后验结束；不共享 WRITE/DELETE，禁止写入和路径替换。
        let media = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&media_path)
            .map_err(|error| {
                Issue::failed(VerificationStage::MediaGuard, "media_read_guard", error)
            })?;
        let media_before = file_snapshot::snapshot(&media).map_err(|error| {
            Issue::failed(VerificationStage::MediaGuard, "media_snapshot", error)
        })?;
        report.identity = Some(FileIdentity::from(&media_before));
        report.bytes = Some(media_before.bytes);
        report.written = Some(media_before.written);
        report.registration_content_unchanged = Some(
            registered.bytes == media_before.bytes && registered.written == media_before.written,
        );
        check_registered(registered, &media_before)?;
        report.registered_identity_matches = true;
        if media_before.bytes == 0 {
            return Err(Issue::failed(
                VerificationStage::MediaGuard,
                "empty_media",
                "The registered media file is empty",
            ));
        }
        if media_before.bytes > report.limits.max_file_bytes {
            return Err(Issue::limited(
                VerificationStage::MediaGuard,
                "file_byte_limit",
            ));
        }
        let guards = Self {
            directory_path: directory_path.to_owned(),
            directory,
            lease_path,
            lease,
            lease_before,
            manifest_path,
            manifest: manifest_file,
            manifest_before,
            media_path,
            media,
            media_before,
        };
        // 建立媒体 guard 后再验元数据，避免在初始化解码前使用已变化的登记记录。
        guards.verify_unchanged()?;
        Ok(guards)
    }

    fn verify_unchanged(&self) -> CheckResult<()> {
        let metadata = self.directory.metadata().map_err(|error| {
            Issue::failed(
                VerificationStage::PostCheck,
                "directory_handle_changed",
                error,
            )
        })?;
        let path_metadata = fs::symlink_metadata(&self.directory_path).map_err(|error| {
            Issue::failed(
                VerificationStage::PostCheck,
                "directory_path_changed",
                error,
            )
        })?;
        if !metadata.is_dir()
            || !attributes_safe(&metadata)
            || !path_metadata.is_dir()
            || !attributes_safe(&path_metadata)
        {
            return Err(Issue::failed(
                VerificationStage::PostCheck,
                "unsafe_directory",
                "The guarded recovery directory is no longer safe",
            ));
        }
        stable_file(&self.lease, &self.lease_path, &self.lease_before)?;
        stable_file(&self.manifest, &self.manifest_path, &self.manifest_before)?;
        stable_file(&self.media, &self.media_path, &self.media_before)
    }
}

fn check_registered(registered: &RecordedFile, current: &FileSnapshot) -> CheckResult<()> {
    if current.attributes & UNSAFE_ATTRIBUTES != 0 {
        return Err(Issue::failed(
            VerificationStage::MediaIdentity,
            "unsafe_media_attributes",
            "The opened media has unsafe attributes",
        ));
    }
    if registered.identity != FileIdentity::from(current) {
        return Err(Issue::failed(
            VerificationStage::MediaIdentity,
            "registered_identity_changed",
            "The opened media does not match the registered file identity",
        ));
    }
    // bytes/written 是注册时的观察；同一录制 writer 后续增长不改变已登记身份。
    Ok(())
}

fn stable_file(file: &File, path: &Path, before: &FileSnapshot) -> CheckResult<()> {
    let after = file_snapshot::snapshot(file).map_err(|error| {
        Issue::failed(VerificationStage::PostCheck, "guard_snapshot_failed", error)
    })?;
    if &after != before || after.attributes & UNSAFE_ATTRIBUTES != 0 {
        return Err(Issue::failed(
            VerificationStage::PostCheck,
            "guarded_object_changed",
            "The guarded object's identity, bytes, timestamps or attributes changed",
        ));
    }
    verify_metadata(path, before)
        .map_err(|error| Issue::failed(VerificationStage::PostCheck, "guarded_path_changed", error))
}

fn decode(
    path: &Path,
    stop: &AtomicBool,
    started: Instant,
    report: &mut VerificationReport,
) -> CheckResult<()> {
    let _runtime =
        MfRuntime::new().map_err(|error| Issue::backend(VerificationStage::Runtime, error))?;
    checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::VideoOpen,
    )?;
    let mut video = VideoReader::open(path)
        .map_err(|error| Issue::backend(VerificationStage::VideoOpen, error))?;
    checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::VideoOpen,
    )?;
    let width = video.width;
    let height = video.height;
    let frame_bytes = frame_bytes(width, height, report.limits)?;
    report.width = Some(width);
    report.height = Some(height);
    report.codec = Some(format!("{:?}", video.info.codec));
    report.declared_frame_rate = video.info.frame_rate;
    report.declared_duration_ns = u64::try_from(video.duration.as_nanos()).ok();
    if video.duration > report.limits.max_media_time {
        return Err(Issue::limited(
            VerificationStage::VideoOpen,
            "declared_duration_limit",
        ));
    }
    loop {
        checkpoint(
            stop,
            started.elapsed(),
            report.limits,
            VerificationStage::VideoDecode,
        )?;
        if report.video_frames >= report.limits.max_video_frames {
            return Err(Issue::limited(
                VerificationStage::VideoDecode,
                "video_frame_limit_before_eof",
            ));
        }
        reserve_budget(
            report.decoded_bytes,
            frame_bytes,
            report.limits.max_decoded_bytes,
            VerificationStage::VideoDecode,
            "decoded_byte_limit_before_eof",
        )?;
        let next = video.next_cancellable(stop);
        // next_cancellable 在取消时也返回 None；须先检查再接受 EOF。
        checkpoint(
            stop,
            started.elapsed(),
            report.limits,
            VerificationStage::VideoDecode,
        )?;
        let Some(frame) =
            next.map_err(|error| Issue::backend(VerificationStage::VideoDecode, error))?
        else {
            report.video_eof = true;
            break;
        };
        if video.width != width || video.height != height || frame.bgra.len() as u64 != frame_bytes
        {
            return Err(Issue::failed(
                VerificationStage::VideoDecode,
                "video_layout_changed_or_invalid",
                "The decoded video layout changed or its BGRA byte count is invalid",
            ));
        }
        let pts = checked_pts(
            frame.time,
            report.video_last_pts_ns,
            true,
            report.limits,
            VerificationStage::VideoDecode,
        )?;
        report.video_first_pts_ns.get_or_insert(pts);
        report.video_last_pts_ns = Some(pts);
        report.video_frames = add_count(report.video_frames, 1, VerificationStage::VideoDecode)?;
        report.decoded_bytes = add_count(
            report.decoded_bytes,
            frame_bytes,
            VerificationStage::VideoDecode,
        )?;
    }
    if report.video_frames == 0 {
        return Err(Issue::backend(
            VerificationStage::VideoDecode,
            "No video frames were decoded before EOF",
        ));
    }
    drop(video);
    checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::AudioOpen,
    )?;
    let opened = AudioReader::open(path);
    checkpoint(
        stop,
        started.elapsed(),
        report.limits,
        VerificationStage::AudioOpen,
    )?;
    let Some(mut audio) =
        opened.map_err(|error| Issue::backend(VerificationStage::AudioOpen, error))?
    else {
        report.audio_presence = AudioPresence::Absent;
        report.audio_eof = true;
        return Ok(());
    };
    report.audio_presence = AudioPresence::Present;
    let rate = audio.rate;
    let channels = audio.channels;
    report.audio_rate = Some(rate);
    report.audio_channels = Some(channels);
    if !(1..=384_000).contains(&rate) || !(1..=32).contains(&channels) {
        return Err(Issue::failed(
            VerificationStage::AudioOpen,
            "invalid_audio_layout",
            "The decoded audio layout is invalid",
        ));
    }
    loop {
        checkpoint(
            stop,
            started.elapsed(),
            report.limits,
            VerificationStage::AudioDecode,
        )?;
        // 下一块长度在 native call 前未知，保守预留 reader 的最大块，绝不越过累计上限。
        reserve_budget(
            report.decoded_bytes,
            MAX_AUDIO_BLOCK_BYTES,
            report.limits.max_decoded_bytes,
            VerificationStage::AudioDecode,
            "decoded_byte_limit_before_eof",
        )?;
        reserve_budget(
            report.audio_sample_values,
            MAX_AUDIO_BLOCK_VALUES,
            report.limits.max_audio_sample_values,
            VerificationStage::AudioDecode,
            "audio_sample_limit_before_eof",
        )?;
        let next = audio.next();
        checkpoint(
            stop,
            started.elapsed(),
            report.limits,
            VerificationStage::AudioDecode,
        )?;
        let Some(block) =
            next.map_err(|error| Issue::backend(VerificationStage::AudioDecode, error))?
        else {
            report.audio_eof = true;
            break;
        };
        if audio.rate != rate
            || audio.channels != channels
            || block.samples.is_empty()
            || block.samples.len() % channels != 0
            || block.samples.len() as u64 > MAX_AUDIO_BLOCK_VALUES
        {
            return Err(Issue::failed(
                VerificationStage::AudioDecode,
                "audio_layout_changed_or_invalid",
                "The decoded audio block layout changed or is invalid",
            ));
        }
        let pts = checked_pts(
            block.time,
            report.audio_last_pts_ns,
            false,
            report.limits,
            VerificationStage::AudioDecode,
        )?;
        let values = block.samples.len() as u64;
        let frames = values / channels as u64;
        let end = audio_end_ns(pts, frames, rate, report.limits)?;
        report.audio_first_pts_ns.get_or_insert(pts);
        report.audio_last_pts_ns = Some(pts);
        report.audio_max_end_ns = Some(
            report
                .audio_max_end_ns
                .map_or(end, |previous| previous.max(end)),
        );
        report.audio_blocks = add_count(report.audio_blocks, 1, VerificationStage::AudioDecode)?;
        report.audio_frames =
            add_count(report.audio_frames, frames, VerificationStage::AudioDecode)?;
        report.audio_sample_values = add_count(
            report.audio_sample_values,
            values,
            VerificationStage::AudioDecode,
        )?;
        report.decoded_bytes = add_count(
            report.decoded_bytes,
            values * 4,
            VerificationStage::AudioDecode,
        )?;
    }
    if report.audio_frames == 0 {
        return Err(Issue::new(
            VerificationState::CurrentBackendCannotVerify,
            VerificationStage::AudioDecode,
            "empty_audio_decode",
            "An audio stream was present, but the current backend decoded no audio frames before EOF",
        ));
    }
    Ok(())
}

fn checkpoint(
    stop: &AtomicBool,
    elapsed: Duration,
    limits: VerificationLimits,
    stage: VerificationStage,
) -> CheckResult<()> {
    if stop.load(Ordering::Acquire) {
        return Err(Issue::new(
            VerificationState::Cancelled,
            stage,
            "cancelled",
            "Verification was cancelled",
        ));
    }
    if elapsed >= limits.max_elapsed {
        return Err(Issue::limited(stage, "elapsed_time_limit"));
    }
    Ok(())
}

fn frame_bytes(width: u32, height: u32, limits: VerificationLimits) -> CheckResult<u64> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 {
        return Err(Issue::failed(
            VerificationStage::VideoOpen,
            "empty_video_layout",
            "The video dimensions are zero",
        ));
    }
    if width > limits.max_dimension
        || height > limits.max_dimension
        || pixels > limits.max_frame_pixels
    {
        return Err(Issue::limited(
            VerificationStage::VideoOpen,
            "video_layout_limit",
        ));
    }
    pixels.checked_mul(4).ok_or_else(|| {
        Issue::failed(
            VerificationStage::VideoOpen,
            "video_layout_overflow",
            "The video byte count overflowed",
        )
    })
}

fn checked_pts(
    time: Duration,
    previous: Option<u64>,
    strict: bool,
    limits: VerificationLimits,
    stage: VerificationStage,
) -> CheckResult<u64> {
    if time > limits.max_media_time {
        return Err(Issue::limited(stage, "media_time_limit"));
    }
    let ns = u64::try_from(time.as_nanos()).map_err(|_| {
        Issue::failed(
            stage,
            "timestamp_overflow",
            "The decoded timestamp overflowed",
        )
    })?;
    if previous.is_some_and(|previous| ns < previous || (strict && ns == previous)) {
        return Err(Issue::failed(
            stage,
            "non_monotonic_timestamp",
            "Decoded presentation timestamps did not advance in order",
        ));
    }
    Ok(ns)
}

fn audio_end_ns(pts: u64, frames: u64, rate: u32, limits: VerificationLimits) -> CheckResult<u64> {
    let stage = VerificationStage::AudioDecode;
    if rate == 0 {
        return Err(Issue::failed(
            stage,
            "invalid_audio_rate",
            "The decoded audio rate is zero",
        ));
    }
    // 向上取整到纳秒，尾部不得因整数截断落回预算内。
    let duration_ns = (u128::from(frames) * 1_000_000_000).div_ceil(u128::from(rate));
    let end = u128::from(pts).checked_add(duration_ns).ok_or_else(|| {
        Issue::failed(
            stage,
            "audio_time_overflow",
            "The audio end timestamp overflowed",
        )
    })?;
    if end > limits.max_media_time.as_nanos() {
        return Err(Issue::limited(stage, "media_time_limit"));
    }
    u64::try_from(end).map_err(|_| {
        Issue::failed(
            stage,
            "audio_time_overflow",
            "The audio end timestamp overflowed",
        )
    })
}

fn add_count(value: u64, increment: u64, stage: VerificationStage) -> CheckResult<u64> {
    value.checked_add(increment).ok_or_else(|| {
        Issue::failed(
            stage,
            "counter_overflow",
            "A decoded-media counter overflowed",
        )
    })
}

fn reserve_budget(
    value: u64,
    increment: u64,
    maximum: u64,
    stage: VerificationStage,
    reason: &'static str,
) -> CheckResult<()> {
    if add_count(value, increment, stage)? > maximum {
        return Err(Issue::limited(stage, reason));
    }
    Ok(())
}

fn readable_gate(report: &VerificationReport) -> bool {
    report.registered_identity_matches
        && report.metadata_stable
        && report.video_frames > 0
        && report.video_eof
        && match report.audio_presence {
            AudioPresence::Absent => true,
            AudioPresence::Present => report.audio_frames > 0,
            AudioPresence::Unknown => false,
        }
        && report.audio_eof
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_recovery_verification_limits_and_mp4_selection_are_finite() {
        let limits = VerificationLimits::default();
        assert!(limits.validate().is_ok());
        assert!(VerificationLimits {
            max_elapsed: Duration::ZERO,
            ..limits
        }
        .validate()
        .is_err());
        assert!(VerificationLimits {
            max_file_bytes: u64::MAX,
            ..limits
        }
        .validate()
        .is_err());
        assert!(supported(MediaName::Video));
        for name in MediaName::ENCODER_ATTEMPTS {
            assert!(supported(name));
        }
        assert!(!supported(MediaName::Gif));
        assert!(!supported(MediaName::SystemAudio));
        assert!(!supported(MediaName::MicrophoneAudio));
        assert!(frame_bytes(960, 540, limits).is_ok());
        assert!(frame_bytes(0, 540, limits).is_err());
        assert_eq!(
            frame_bytes(8192, 8192, limits).unwrap_err().state,
            VerificationState::Limited
        );
    }

    #[test]
    fn recording_recovery_verification_cancel_precedes_timeout_and_never_reads_a_path() {
        let stop = AtomicBool::new(true);
        let limits = VerificationLimits::default();
        assert_eq!(
            checkpoint(
                &stop,
                limits.max_elapsed,
                limits,
                VerificationStage::Request
            )
            .unwrap_err()
            .state,
            VerificationState::Cancelled
        );
        let report = verify(
            Path::new("nonexistent-recovery-verification-fixture"),
            MediaName::Video,
            &stop,
            limits,
        );
        assert_eq!(report.state, VerificationState::Cancelled);
        assert_eq!(report.stage, VerificationStage::Request);
        assert!(!report.lease_held);
        stop.store(false, Ordering::Release);
        assert_eq!(
            checkpoint(
                &stop,
                limits.max_elapsed,
                limits,
                VerificationStage::Request
            )
            .unwrap_err()
            .state,
            VerificationState::Limited
        );
    }

    #[test]
    fn recording_recovery_verification_timestamps_and_counters_are_checked() {
        let limits = VerificationLimits::default();
        let stage = VerificationStage::VideoDecode;
        assert_eq!(
            checked_pts(Duration::ZERO, None, true, limits, stage).unwrap(),
            0
        );
        assert!(checked_pts(Duration::ZERO, Some(0), true, limits, stage).is_err());
        assert!(checked_pts(Duration::ZERO, Some(0), false, limits, stage).is_ok());
        assert!(checked_pts(Duration::from_nanos(5), Some(6), false, limits, stage).is_err());
        assert_eq!(
            checked_pts(
                limits.max_media_time + Duration::from_nanos(1),
                None,
                true,
                limits,
                stage
            )
            .unwrap_err()
            .state,
            VerificationState::Limited
        );
        assert_eq!(audio_end_ns(0, 1, 48_000, limits).unwrap(), 20_834);
        assert!(audio_end_ns(0, u64::MAX, 1, limits).is_err());
        assert!(audio_end_ns(0, 1, 0, limits).is_err());
        assert!(add_count(u64::MAX, 1, stage).is_err());
        assert!(reserve_budget(u64::MAX, 1, u64::MAX, stage, "test").is_err());
        assert!(reserve_budget(9, 1, 10, stage, "test").is_ok());
        assert_eq!(
            reserve_budget(10, 1, 10, stage, "test").unwrap_err().state,
            VerificationState::Limited
        );
    }

    #[test]
    fn recording_recovery_verification_registration_checks_identity_not_historical_size() {
        let mut snapshot = FileSnapshot {
            volume: 1,
            id: [2; 16],
            created: 3,
            written: 4,
            bytes: 10,
            attributes: 0,
        };
        let registered = RecordedFile {
            name: MediaName::Video,
            identity: FileIdentity::from(&snapshot),
            bytes: 10,
            written: 4,
        };
        snapshot.bytes = 20;
        snapshot.written = 5;
        assert!(check_registered(&registered, &snapshot).is_ok());
        snapshot.id[15] = 9;
        assert!(check_registered(&registered, &snapshot).is_err());
        snapshot.id = [2; 16];
        snapshot.attributes = UNSAFE_ATTRIBUTES;
        assert!(check_registered(&registered, &snapshot).is_err());
    }

    #[test]
    fn recording_recovery_verification_readable_gate_requires_real_eof_and_stable_identity() {
        let mut report = VerificationReport::new(
            Path::new("unused"),
            MediaName::Video,
            VerificationLimits::default(),
        );
        report.registered_identity_matches = true;
        report.metadata_stable = true;
        report.video_frames = 1;
        report.audio_presence = AudioPresence::Absent;
        report.audio_eof = true;
        assert!(!readable_gate(&report));
        report.video_eof = true;
        assert!(readable_gate(&report));
        report.audio_presence = AudioPresence::Present;
        report.audio_eof = false;
        assert!(!readable_gate(&report));
        report.audio_eof = true;
        assert!(!readable_gate(&report));
        report.audio_frames = 1;
        assert!(readable_gate(&report));
        report.metadata_stable = false;
        assert!(!readable_gate(&report));
        report.metadata_stable = true;
        report.registered_identity_matches = false;
        assert!(!readable_gate(&report));
        report.registered_identity_matches = true;
        report.video_frames = 0;
        assert!(!readable_gate(&report));
        report.video_frames = 1;
        report.finish(Err(Issue::new(
            VerificationState::Cancelled,
            VerificationStage::Complete,
            "cancelled",
            "cancelled",
        )));
        assert_eq!(report.state, VerificationState::Cancelled);
    }
}
