//! 只用真实 TaskToken、人工小流和 QA 隔离暂存目录；不访问运行缓存或网络。
use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};
use std::{
    io::{self, Cursor},
    sync::{mpsc, Arc},
    thread,
    time::UNIX_EPOCH,
};

const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let configured = PathBuf::from(
            std::env::var_os("RUSTSHOT_COLD_QA_ROOT")
                .expect("文件模型必须显式设置 RUSTSHOT_COLD_QA_ROOT"),
        );
        assert!(configured.is_absolute());
        let root = fs::canonicalize(configured).unwrap();
        assert_eq!(root.file_name().unwrap(), "test-temp");
        assert_eq!(root.parent().unwrap().file_name().unwrap(), "draft");
        assert_eq!(
            root.parent()
                .unwrap()
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            "ppocr-cold-preparation-cancel-20261011"
        );
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("model-{}-{sequence}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn assert_no_pending(&self) {
        for entry in fs::read_dir(&self.0).unwrap() {
            assert!(!entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".ocr-"));
        }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        // 路径来自已经核验的独立 draft/test-temp 下的本模型目录。
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct CountingReader {
    calls: usize,
}
impl Read for CountingReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        Ok(0)
    }
}

#[derive(Default)]
struct CountingWriter {
    calls: usize,
    bytes: Vec<u8>,
}
impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CancellingReader<'a, R> {
    inner: R,
    owner: &'a CancelOwner,
    calls: usize,
    cancel_on: usize,
    fail: bool,
}
impl<R: Read> Read for CancellingReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        let result = self.inner.read(bytes);
        if self.calls == self.cancel_on {
            self.owner.cancel().unwrap();
            if self.fail {
                return Err(io::Error::other("synthetic secret read error"));
            }
        }
        result
    }
}

struct CancellingWriter<'a> {
    owner: &'a CancelOwner,
    calls: usize,
    bytes: Vec<u8>,
    partial: bool,
    fail: bool,
}
impl Write for CancellingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        let count = if self.partial { 1 } else { bytes.len() };
        self.bytes.extend_from_slice(&bytes[..count]);
        self.owner.cancel().unwrap();
        if self.fail {
            Err(io::Error::other("synthetic write failure"))
        } else {
            Ok(count)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn staged(destination: &Path) -> Pending {
    let mut pending = Pending::new_cancellable(destination, None).unwrap();
    copy_checked_cancellable(
        &mut Cursor::new(b"abc"),
        pending.file.as_mut().unwrap(),
        3,
        ABC_SHA,
        None,
    )
    .unwrap();
    pending
}

fn assert_cancelled<T>(result: Result<T>) {
    match result {
        Ok(_) => panic!("应保留 OcrCancelled 类型"),
        Err(error) => assert!(is_cancelled(error.as_ref()), "错误类型不是取消：{error}"),
    }
}

#[test]
fn pre_cancelled_public_preparation_entries_stop_before_cache_or_network() {
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    assert_cancelled(ensure_runtime_cancellable(&task));
    assert_cancelled(ensure_det_cancellable(&task));
    assert_cancelled(ensure_cancellable(&task));
}

#[test]
fn pre_cancelled_copy_does_not_read_or_write() {
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    let mut reader = CountingReader::default();
    let mut writer = CountingWriter::default();
    assert_cancelled(copy_checked_cancellable(
        &mut reader,
        &mut writer,
        3,
        ABC_SHA,
        Some(&task),
    ));
    assert_eq!(reader.calls, 0);
    assert_eq!(writer.calls, 0);
}

#[test]
fn pre_cancelled_hash_does_not_read() {
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    let mut reader = CountingReader::default();
    assert_cancelled(hash_reader_cancellable(&mut reader, Some(&task)));
    assert_eq!(reader.calls, 0);
}

#[test]
fn pre_cancelled_uncontended_lock_is_not_acquired() {
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    let mutex = Mutex::new(5);
    assert_cancelled(wait_for_preparation(&mutex, Some(&task)));
    assert!(mutex.try_lock().is_ok());
}

#[test]
fn cancelled_queued_preparation_returns_while_lock_remains_held_and_other_task_survives() {
    let mutex = Arc::new(Mutex::new(()));
    let held = mutex.lock().unwrap();
    let owner = CancelOwner::new();
    let task = owner.token();
    let next_owner = CancelOwner::new();
    let next_task = next_owner.token();
    let worker_mutex = mutex.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        started_tx.send(()).unwrap();
        let cancelled = match wait_for_preparation(&worker_mutex, Some(&task)) {
            Ok(_) => false,
            Err(error) => is_cancelled(error.as_ref()),
        };
        finished_tx.send(cancelled).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        finished_rx.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    owner.cancel().unwrap();
    assert!(finished_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    next_task.checkpoint().unwrap();
    assert!(mutex.try_lock().is_err());
    drop(held);
    let _next = wait_for_preparation(&mutex, Some(&next_task)).unwrap();
}

#[test]
fn pre_cancelled_poisoned_lock_preserves_cancel_type() {
    let mutex = Arc::new(Mutex::new(()));
    let poisoned = mutex.clone();
    assert!(thread::spawn(move || {
        let _held = poisoned.lock().unwrap();
        panic!("模型制造锁中毒");
    })
    .join()
    .is_err());
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    assert_cancelled(wait_for_preparation(&mutex, Some(&task)));
}

#[test]
fn cancellation_during_read_stops_before_any_write() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut reader = CancellingReader {
        inner: Cursor::new(b"abc"),
        owner: &owner,
        calls: 0,
        cancel_on: 1,
        fail: false,
    };
    let mut writer = CountingWriter::default();
    assert_cancelled(copy_checked_cancellable(
        &mut reader,
        &mut writer,
        3,
        ABC_SHA,
        Some(&task),
    ));
    assert_eq!(reader.calls, 1);
    assert_eq!(writer.calls, 0);
}

#[test]
fn cancellation_during_failed_read_precedes_redacted_io_error() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut reader = CancellingReader {
        inner: Cursor::new(b"abc"),
        owner: &owner,
        calls: 0,
        cancel_on: 1,
        fail: true,
    };
    assert_cancelled(copy_checked_cancellable(
        &mut reader,
        &mut Vec::new(),
        3,
        ABC_SHA,
        Some(&task),
    ));
}

#[test]
fn ordinary_read_failure_remains_redacted_and_not_cancelled() {
    struct FailedRead;
    impl Read for FailedRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("http://user:secret@synthetic.invalid"))
        }
    }
    let owner = CancelOwner::new();
    let task = owner.token();
    let error = copy_checked_cancellable(&mut FailedRead, &mut Vec::new(), 3, ABC_SHA, Some(&task))
        .unwrap_err();
    assert!(!is_cancelled(error.as_ref()));
    for sensitive in ["user", "secret", "synthetic.invalid"] {
        assert!(!error.to_string().contains(sensitive));
    }
}

#[test]
fn cancellation_after_complete_write_does_not_read_again() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut writer = CancellingWriter {
        owner: &owner,
        calls: 0,
        bytes: Vec::new(),
        partial: false,
        fail: false,
    };
    let mut reader = CancellingReader {
        inner: Cursor::new(b"abc"),
        owner: &owner,
        calls: 0,
        cancel_on: usize::MAX,
        fail: false,
    };
    assert_cancelled(copy_checked_cancellable(
        &mut reader,
        &mut writer,
        3,
        ABC_SHA,
        Some(&task),
    ));
    assert_eq!(writer.calls, 1);
    assert_eq!(writer.bytes, b"abc");
    assert_eq!(reader.calls, 1);
    assert_eq!(reader.inner.position(), 3);
}

#[test]
fn cancellation_after_partial_write_stops_before_second_write() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut writer = CancellingWriter {
        owner: &owner,
        calls: 0,
        bytes: Vec::new(),
        partial: true,
        fail: false,
    };
    assert_cancelled(copy_checked_cancellable(
        &mut Cursor::new(b"abc"),
        &mut writer,
        3,
        ABC_SHA,
        Some(&task),
    ));
    assert_eq!(writer.calls, 1);
    assert_eq!(writer.bytes, b"a");
}

#[test]
fn cancellation_during_failed_write_preserves_cancel_type() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut writer = CancellingWriter {
        owner: &owner,
        calls: 0,
        bytes: Vec::new(),
        partial: false,
        fail: true,
    };
    assert_cancelled(copy_checked_cancellable(
        &mut Cursor::new(b"abc"),
        &mut writer,
        3,
        ABC_SHA,
        Some(&task),
    ));
}

#[test]
fn normal_short_writes_still_produce_exact_checked_payload() {
    #[derive(Default)]
    struct ShortWriter(Vec<u8>);
    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.push(bytes[0]);
            Ok(1)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let owner = CancelOwner::new();
    let mut writer = ShortWriter::default();
    copy_checked_cancellable(
        &mut Cursor::new(b"abc"),
        &mut writer,
        3,
        ABC_SHA,
        Some(&owner.token()),
    )
    .unwrap();
    assert_eq!(writer.0, b"abc");
}

#[test]
fn hash_cancellation_after_read_preserves_type_and_stops_reader() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut reader = CancellingReader {
        inner: Cursor::new(b"abc"),
        owner: &owner,
        calls: 0,
        cancel_on: 1,
        fail: false,
    };
    assert_cancelled(hash_reader_cancellable(&mut reader, Some(&task)));
    assert_eq!(reader.calls, 1);
}

#[test]
fn hash_cancellation_during_failed_read_precedes_io_error() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut reader = CancellingReader {
        inner: Cursor::new(b"abc"),
        owner: &owner,
        calls: 0,
        cancel_on: 1,
        fail: true,
    };
    assert_cancelled(hash_reader_cancellable(&mut reader, Some(&task)));
}

#[test]
fn hash_cancellation_after_previous_chunk_was_hashed_stops_next_chunk() {
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut reader = CancellingReader {
        inner: Cursor::new(vec![b'a'; 65_537]),
        owner: &owner,
        calls: 0,
        cancel_on: 2,
        fail: false,
    };
    assert_cancelled(hash_reader_cancellable(&mut reader, Some(&task)));
    assert_eq!(reader.calls, 2);
    assert_eq!(reader.inner.position(), 65_537);
}

#[test]
fn normal_hash_reader_returns_actual_bytes_and_digest() {
    assert_eq!(
        hash_reader_cancellable(&mut Cursor::new(b"abc"), None).unwrap(),
        (3, ABC_SHA.to_owned())
    );
}

#[test]
fn blocked_read_cannot_finish_until_read_returns_and_worker_is_joined() {
    struct BlockedReader {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        calls: usize,
    }
    impl Read for BlockedReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            self.started.send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| io::Error::other("模型阻塞流等待释放失败"))?;
            bytes[..3].copy_from_slice(b"abc");
            Ok(3)
        }
    }
    let owner = CancelOwner::new();
    let task = owner.token();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut reader = BlockedReader {
            started: started_tx,
            release: release_rx,
            calls: 0,
        };
        let mut writer = CountingWriter::default();
        let result = copy_checked_cancellable(&mut reader, &mut writer, 3, ABC_SHA, Some(&task));
        let cancelled = result
            .err()
            .is_some_and(|error| is_cancelled(error.as_ref()));
        finished_tx
            .send((cancelled, reader.calls, writer.calls))
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    owner.cancel().unwrap();
    let pending = finished_rx.recv_timeout(Duration::from_millis(30));
    assert!(matches!(pending, Err(mpsc::RecvTimeoutError::Timeout)));
    release_tx.send(()).unwrap();
    assert_eq!(
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        (true, 1, 0)
    );
    worker.join().unwrap();
}

fn small_archive() -> Vec<u8> {
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file(
            "native/synthetic.dll",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
    archive.write_all(b"abc").unwrap();
    archive.finish().unwrap().into_inner()
}

#[test]
fn real_zip_member_copy_cancellation_cleans_actual_pending_file() {
    let directory = TestDirectory::new();
    let destination = directory.file("synthetic.dll");
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut archive = zip::ZipArchive::new(Cursor::new(small_archive())).unwrap();
    let member = archive.by_name("native/synthetic.dll").unwrap();
    assert_eq!(member.size(), 3);
    let mut member = CancellingReader {
        inner: member,
        owner: &owner,
        calls: 0,
        cancel_on: 1,
        fail: false,
    };
    let mut pending = Pending::new_cancellable(&destination, Some(&task)).unwrap();
    let pending_path = pending.path.clone();
    assert_cancelled(copy_checked_cancellable(
        &mut member,
        pending.file.as_mut().unwrap(),
        3,
        ABC_SHA,
        Some(&task),
    ));
    drop(pending);
    assert!(!pending_path.exists());
    assert!(!destination.exists());
    directory.assert_no_pending();
}

#[test]
fn real_zip_member_verified_publish_is_available_to_next_task() {
    let directory = TestDirectory::new();
    let destination = directory.file("synthetic.dll");
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut archive = zip::ZipArchive::new(Cursor::new(small_archive())).unwrap();
    let mut member = archive.by_name("native/synthetic.dll").unwrap();
    let mut pending = Pending::new_cancellable(&destination, Some(&task)).unwrap();
    copy_checked_cancellable(
        &mut member,
        pending.file.as_mut().unwrap(),
        3,
        ABC_SHA,
        Some(&task),
    )
    .unwrap();
    pending
        .publish_cancellable(&destination, 3, ABC_SHA, Some(&task))
        .unwrap();
    owner.cancel().unwrap();
    let next_owner = CancelOwner::new();
    assert!(verified_cancellable(&destination, 3, ABC_SHA, Some(&next_owner.token())).unwrap());
    assert_eq!(fs::read(&destination).unwrap(), b"abc");
    directory.assert_no_pending();
}

#[test]
fn pre_cancelled_pending_creation_leaves_directory_empty() {
    let directory = TestDirectory::new();
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    assert_cancelled(Pending::new_cancellable(
        &directory.file("new.bin"),
        Some(&task),
    ));
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
}

#[test]
fn dropping_pending_closes_and_removes_its_actual_file() {
    let directory = TestDirectory::new();
    let destination = directory.file("new.bin");
    let pending = staged(&destination);
    let pending_path = pending.path.clone();
    assert!(pending_path.exists());
    drop(pending);
    assert!(!pending_path.exists());
    assert!(!destination.exists());
}

#[test]
fn truncated_stream_cleans_pending_preserves_cache_and_allows_retry() {
    let directory = TestDirectory::new();
    let destination = directory.file("new.bin");
    let legal_cache = directory.file("existing.bin");
    fs::write(&legal_cache, b"abc").unwrap();
    let mut pending = Pending::new_cancellable(&destination, None).unwrap();
    assert!(copy_checked_cancellable(
        &mut Cursor::new(b"ab"),
        pending.file.as_mut().unwrap(),
        3,
        ABC_SHA,
        None,
    )
    .is_err());
    drop(pending);
    assert!(!destination.exists());
    directory.assert_no_pending();
    staged(&destination)
        .publish_cancellable(&destination, 3, ABC_SHA, None)
        .unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"abc");
    assert_eq!(fs::read(legal_cache).unwrap(), b"abc");
    directory.assert_no_pending();
}

#[test]
fn later_read_cancellation_cleans_written_chunk_and_new_task_can_retry() {
    let directory = TestDirectory::new();
    let destination = directory.file("new.bin");
    let legal_cache = directory.file("existing.bin");
    fs::write(&legal_cache, b"abc").unwrap();
    let owner = CancelOwner::new();
    let task = owner.token();
    let mut reader = CancellingReader {
        inner: Cursor::new(vec![b'a'; 65_537]),
        owner: &owner,
        calls: 0,
        cancel_on: 2,
        fail: false,
    };
    let mut pending = Pending::new_cancellable(&destination, Some(&task)).unwrap();
    let pending_path = pending.path.clone();
    assert_cancelled(copy_checked_cancellable(
        &mut reader,
        pending.file.as_mut().unwrap(),
        65_537,
        ABC_SHA,
        Some(&task),
    ));
    assert_eq!(
        pending.file.as_ref().unwrap().metadata().unwrap().len(),
        65_536
    );
    drop(pending);
    assert!(!pending_path.exists());
    assert!(!destination.exists());
    let next_owner = CancelOwner::new();
    staged(&destination)
        .publish_cancellable(&destination, 3, ABC_SHA, Some(&next_owner.token()))
        .unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"abc");
    assert_eq!(fs::read(legal_cache).unwrap(), b"abc");
    directory.assert_no_pending();
}

#[test]
fn cancelled_before_publish_cleans_pending_and_does_not_replace_legal_cache() {
    let directory = TestDirectory::new();
    let destination = directory.file("cache.bin");
    fs::write(&destination, b"abc").unwrap();
    let pending = staged(&destination);
    let pending_path = pending.path.clone();
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    assert_cancelled(pending.publish_cancellable(&destination, 3, ABC_SHA, Some(&task)));
    assert!(!pending_path.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"abc");
    directory.assert_no_pending();
}

#[test]
fn cancel_after_publish_preserves_committed_cache_for_new_request() {
    let directory = TestDirectory::new();
    let destination = directory.file("cache.bin");
    let owner = CancelOwner::new();
    let task = owner.token();
    staged(&destination)
        .publish_cancellable(&destination, 3, ABC_SHA, Some(&task))
        .unwrap();
    owner.cancel().unwrap();
    assert_cancelled(verified_cancellable(&destination, 3, ABC_SHA, Some(&task)));
    let next_owner = CancelOwner::new();
    assert!(verified_cancellable(&destination, 3, ABC_SHA, Some(&next_owner.token())).unwrap());
    assert_eq!(fs::read(destination).unwrap(), b"abc");
}

#[test]
fn pre_cancelled_verification_preserves_existing_file_even_if_invalid() {
    let directory = TestDirectory::new();
    let destination = directory.file("cache.bin");
    fs::write(&destination, b"abd").unwrap();
    let owner = CancelOwner::new();
    let task = owner.token();
    owner.cancel().unwrap();
    assert_cancelled(verified_cancellable(&destination, 3, ABC_SHA, Some(&task)));
    assert_eq!(fs::read(destination).unwrap(), b"abd");
}

#[test]
fn invalid_cache_verification_reports_failure_without_modifying_file() {
    let directory = TestDirectory::new();
    let destination = directory.file("cache.bin");
    fs::write(&destination, b"abd").unwrap();
    assert!(verified_cancellable(&destination, 3, ABC_SHA, None).is_err());
    assert_eq!(fs::read(destination).unwrap(), b"abd");
    directory.assert_no_pending();
}

#[test]
fn real_download_cached_fast_path_preserves_cache_on_cancel_and_new_task_reuses_it() {
    let directory = TestDirectory::new();
    let destination = directory.file("cached.bin");
    fs::write(&destination, b"abc").unwrap();
    let asset = Asset {
        name: "cached.bin".into(),
        // 缓存验证分支在创建 agent 或调用 URL 前返回；禁止删除此合法缓存。
        url: "https://synthetic.invalid/must-not-be-requested".into(),
        bytes: 3,
        sha256: ABC_SHA.into(),
        upstream_url: None,
        upstream_bytes: None,
        upstream_sha256: None,
    };
    let old_owner = CancelOwner::new();
    let old_task = old_owner.token();
    old_owner.cancel().unwrap();
    assert_cancelled(download_cancellable(&directory.0, &asset, Some(&old_task)));
    assert_eq!(fs::read(&destination).unwrap(), b"abc");
    let new_owner = CancelOwner::new();
    let resolved = download_cancellable(&directory.0, &asset, Some(&new_owner.token())).unwrap();
    assert_eq!(resolved, destination);
    assert!(resolved.is_absolute());
    assert_eq!(fs::read(resolved).unwrap(), b"abc");
    directory.assert_no_pending();
}

#[test]
fn verified_external_winner_is_reused_without_replacing_its_timestamp() {
    let directory = TestDirectory::new();
    let destination = directory.file("winner.bin");
    let pending = staged(&destination);
    fs::write(&destination, b"abc").unwrap();
    let timestamp = UNIX_EPOCH + Duration::from_secs(123_456);
    File::options()
        .write(true)
        .open(&destination)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(timestamp))
        .unwrap();
    let actual_timestamp = fs::metadata(&destination).unwrap().modified().unwrap();
    pending
        .publish_cancellable(&destination, 3, ABC_SHA, None)
        .unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"abc");
    assert_eq!(
        fs::metadata(&destination).unwrap().modified().unwrap(),
        actual_timestamp
    );
    directory.assert_no_pending();
}

#[test]
fn invalid_external_winner_is_not_overwritten_and_staging_is_cleaned() {
    let directory = TestDirectory::new();
    let destination = directory.file("winner.bin");
    let pending = staged(&destination);
    fs::write(&destination, b"abd").unwrap();
    assert!(pending
        .publish_cancellable(&destination, 3, ABC_SHA, None)
        .is_err());
    assert_eq!(fs::read(destination).unwrap(), b"abd");
    directory.assert_no_pending();
}

#[test]
fn missing_parent_creation_failure_can_retry_with_same_destination() {
    let directory = TestDirectory::new();
    let parent = directory.file("later");
    let destination = parent.join("cache.bin");
    assert!(Pending::new_cancellable(&destination, None).is_err());
    fs::create_dir(&parent).unwrap();
    staged(&destination)
        .publish_cancellable(&destination, 3, ABC_SHA, None)
        .unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"abc");
}

#[test]
fn old_cancelled_pending_cannot_take_new_publish_or_delete_new_winner() {
    let directory = TestDirectory::new();
    let destination = directory.file("cache.bin");
    let old_pending = staged(&destination);
    let new_pending = staged(&destination);
    assert_ne!(old_pending.path, new_pending.path);
    let old_owner = CancelOwner::new();
    let old_task = old_owner.token();
    let new_owner = CancelOwner::new();
    let new_task = new_owner.token();
    old_owner.cancel().unwrap();
    new_pending
        .publish_cancellable(&destination, 3, ABC_SHA, Some(&new_task))
        .unwrap();
    assert_cancelled(old_pending.publish_cancellable(&destination, 3, ABC_SHA, Some(&old_task)));
    new_task.checkpoint().unwrap();
    assert!(verified_cancellable(&destination, 3, ABC_SHA, Some(&new_task)).unwrap());
    directory.assert_no_pending();
}

#[test]
fn non_cancellable_compatibility_wrappers_still_copy_verify_and_publish() {
    let directory = TestDirectory::new();
    let destination = directory.file("cache.bin");
    let mut pending = Pending::new(&destination).unwrap();
    copy_checked(
        &mut Cursor::new(b"abc"),
        pending.file.as_mut().unwrap(),
        3,
        ABC_SHA,
    )
    .unwrap();
    pending.publish(&destination, 3, ABC_SHA).unwrap();
    assert!(verified(&destination, 3, ABC_SHA).unwrap());
    directory.assert_no_pending();
}
