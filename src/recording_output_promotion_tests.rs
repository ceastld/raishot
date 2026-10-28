//! 真实文件系统上的晋升交接回归，不编码媒体，也不模拟整个进程被终止。
//! 只在真实 NoReplace 移动返回边界注入错误，核验预登记与持久化失败门。
use super::*;
use std::{fs, fs::OpenOptions, io::Write};

const PAYLOAD: &[u8] = b"owned-promotion-transaction-bytes-not-playable-media";
const MANIFEST_NAME: &str = "recovery.json";
const LEASE_NAME: &str = "recovery.lease";
const SAVED_MANIFEST_NAME: &str = "registered-manifest.json";

struct Fixture {
    root: PathBuf,
    staging: Option<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::path::absolute("target/recording-promotion-recovery").unwrap();
        fs::create_dir_all(&parent).unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for _ in 0..DIRECTORY_ATTEMPTS {
            let root = parent.join(format!(
                "{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    return Self {
                        root,
                        staging: None,
                    };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("无法创建晋升回归独占目录：{error}"),
            }
        }
        panic!("晋升回归独占目录冲突过多");
    }

    fn registered_attempt(&mut self) -> (RecordingFiles, PathBuf, FileSnapshot) {
        let mut files = RecordingFiles::new_with(
            &self.root.join("final.mp4"),
            "promotion",
            &mut DiskOperations,
        )
        .unwrap();
        self.staging = Some(files.directory.clone());
        files.recovery = Some(RecoverySession::create(&files.directory).unwrap());
        let source = files.encoder_attempt_paths(1).unwrap().remove(0);
        write_new(&source, PAYLOAD);
        let snapshot = crate::file_snapshot::inspect(&source).unwrap().unwrap();
        files.register_encoder_attempt(0, snapshot.clone()).unwrap();
        files.mark_recording().unwrap();
        (files, source, snapshot)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 只列举本夹具可能创建的名称；不扫描或递归删除未知内容。
        if let Some(staging) = &self.staging {
            if staging.parent() == Some(self.root.as_path()) {
                for name in FILE_NAMES {
                    let _ = fs::remove_file(staging.join(name));
                }
                for name in MediaName::ENCODER_ATTEMPTS {
                    let _ = fs::remove_file(staging.join(name.basename()));
                }
                for name in [MANIFEST_NAME, LEASE_NAME] {
                    let _ = fs::remove_file(staging.join(name));
                }
                let _ = fs::remove_dir(staging);
            }
        }
        for name in ["final.mp4", SAVED_MANIFEST_NAME] {
            let _ = fs::remove_file(self.root.join(name));
        }
        let _ = fs::remove_dir(&self.root);
    }
}

fn write_new(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    file.sync_all().unwrap();
}

fn assert_lease_released(staging: &Path) {
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .open(staging.join(LEASE_NAME))
        .unwrap();
    lease.try_lock().expect("晋升回归结束后租约仍被占用");
    drop(lease);
}

struct PublishBoundary {
    publish_calls: usize,
    interrupt_after_move: bool,
}

impl FileOperations for PublishBoundary {
    fn create_directory(&mut self, path: &Path) -> io::Result<()> {
        DiskOperations.create_directory(path)
    }

    fn inspect(&mut self, path: &Path) -> Result<FileInfo> {
        DiskOperations.inspect(path)
    }

    fn publish(&mut self, source: &Path, destination: &Path) -> Result<()> {
        self.publish_calls += 1;
        DiskOperations.publish(source, destination)?;
        if self.interrupt_after_move {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "晋升回归在真实移动成功后的返回边界注入中断",
            )
            .into());
        }
        Ok(())
    }

    fn restore_identified(
        &mut self,
        source: &Path,
        destination: &Path,
        expected: &FileSnapshot,
    ) -> Result<()> {
        DiskOperations.restore_identified(source, destination, expected)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        DiskOperations.remove_file(path)
    }

    fn remove_directory(&mut self, path: &Path) -> io::Result<()> {
        DiskOperations.remove_directory(path)
    }
}

#[test]
fn recording_recovery_output_promotion_move_error_keeps_registered_video_discoverable() {
    let mut fixture = Fixture::new();
    let (mut files, source, before) = fixture.registered_attempt();
    let staging = files.directory.clone();
    let video_path = files.video_path().to_path_buf();
    let mut operations = PublishBoundary {
        publish_calls: 0,
        interrupt_after_move: true,
    };
    let error = files
        .promote_encoder_attempt_with(0, &mut operations)
        .unwrap_err();
    assert_eq!(operations.publish_calls, 1);
    assert_eq!(
        error.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::Interrupted)
    );
    assert!(!files.cleanup_started);
    assert!(!source.try_exists().unwrap());
    assert_eq!(fs::read(&video_path).unwrap(), PAYLOAD);
    assert_eq!(
        crate::file_snapshot::inspect(&video_path).unwrap().as_ref(),
        Some(&before)
    );

    // 不执行 cleanup；释放会话，保留移动返回边界上的真实目录状态。
    drop(files);
    let discovered = super::super::recovery::scan(&fixture.root, SystemTime::now(), &|| false);
    assert!(
        !discovered.truncated && !discovered.cancelled,
        "{discovered:?}"
    );
    assert_eq!(discovered.active_skipped, 0, "{discovered:?}");
    assert_eq!(discovered.sessions.len(), 1, "{discovered:?}");
    let session = &discovered.sessions[0];
    assert_eq!(session.directory, staging);
    assert_eq!(session.phase, RecoveryPhase::Recording);
    assert!(session.unverified && !session.lease_held);
    let video = session
        .media
        .iter()
        .find(|item| item.name == MediaName::Video)
        .unwrap();
    assert_eq!(video.state, super::super::recovery::MediaState::Present);
    assert_eq!(video.identity_matches, Some(true));
    assert_eq!(video.content_unchanged, Some(true));
    assert_eq!(video.bytes, Some(PAYLOAD.len() as u64));
    let attempt = session
        .media
        .iter()
        .find(|item| item.name == MediaName::ENCODER_ATTEMPTS[0])
        .unwrap();
    assert_eq!(attempt.state, super::super::recovery::MediaState::Missing);
    assert_eq!(fs::read(&video_path).unwrap(), PAYLOAD);
    assert_eq!(
        crate::file_snapshot::inspect(&video_path).unwrap(),
        Some(before)
    );
    assert_lease_released(&staging);
}

#[test]
fn recording_recovery_output_promotion_manifest_replacement_prevents_move() {
    let mut fixture = Fixture::new();
    let (mut files, source, before) = fixture.registered_attempt();
    let staging = files.directory.clone();
    let video_path = files.video_path().to_path_buf();
    let manifest = files.recovery.as_ref().unwrap().manifest_path();
    let original_manifest = fs::read(&manifest).unwrap();
    let saved_manifest = fixture.root.join(SAVED_MANIFEST_NAME);
    fs::rename(&manifest, &saved_manifest).unwrap();
    let replacement = b"owned replacement manifest rejected before promotion";
    write_new(&manifest, replacement);
    let replacement_before = crate::file_snapshot::inspect(&manifest).unwrap().unwrap();
    let mut operations = PublishBoundary {
        publish_calls: 0,
        interrupt_after_move: false,
    };

    assert!(files
        .promote_encoder_attempt_with(0, &mut operations)
        .is_err());
    assert_eq!(operations.publish_calls, 0);
    assert!(!files.cleanup_started);
    assert_eq!(
        crate::file_snapshot::inspect(&source).unwrap(),
        Some(before)
    );
    assert_eq!(fs::read(&source).unwrap(), PAYLOAD);
    assert!(!video_path.try_exists().unwrap());
    assert_eq!(fs::read(&manifest).unwrap(), replacement);
    assert_eq!(
        crate::file_snapshot::inspect(&manifest).unwrap(),
        Some(replacement_before)
    );
    assert_eq!(fs::read(&saved_manifest).unwrap(), original_manifest);
    drop(files);
    assert_lease_released(&staging);
}
