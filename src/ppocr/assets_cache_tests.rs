//! 缓存预热只读探测，人工小文件与真实任务令牌；不触碰运行缓存或网络。
use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};
static CACHE_TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
fn qa_root() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(workspace.is_absolute());
    workspace.join("target/qa/ppocr-cache-tests")
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let qa_root = qa_root();
        fs::create_dir_all(&qa_root).unwrap();
        let root = qa_root.join(format!(
            "{}-{}",
            std::process::id(),
            CACHE_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn paths(&self, m: &Manifest) -> Vec<PathBuf> {
        m.runtime
            .files
            .iter()
            .map(|f| self.0.join(&m.runtime.directory).join(&f.name))
            .chain(
                m.models
                    .recognition_assets()
                    .into_iter()
                    .map(|a| self.0.join(&m.models.directory).join(&a.name)),
            )
            .collect()
    }
    fn fill(&self, m: &Manifest) {
        for path in self.paths(m) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"abc").unwrap();
        }
    }
    fn snapshot(&self, m: &Manifest) -> Vec<Option<Vec<u8>>> {
        self.paths(m)
            .into_iter()
            .map(|p| fs::read(p).ok())
            .collect()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let qa = qa_root();
        let resolved = fs::canonicalize(&self.0).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(qa).unwrap()));
        let _ = fs::remove_dir_all(resolved);
    }
}
fn small_manifest() -> Manifest {
    let mut m = manifest().unwrap();
    for f in &mut m.runtime.files {
        f.bytes = 3;
        f.sha256 = ABC_SHA.into();
    }
    for a in [
        &mut m.models.license,
        &mut m.models.detection,
        &mut m.models.classification,
        &mut m.models.recognition,
        &mut m.models.dictionary,
    ] {
        a.bytes = 3;
        a.sha256 = ABC_SHA.into();
    }
    m
}
#[test]
fn cache_only_empty_cache_is_none_and_does_not_create_directories() {
    let dir = Directory::new();
    let m = small_manifest();
    let owner = CancelOwner::new();
    assert!(cached_at_cancellable(&m, &dir.0, &owner.token())
        .unwrap()
        .is_none());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}
#[test]
fn cache_only_all_nine_files_produce_absolute_paths_without_mutation() {
    let dir = Directory::new();
    let m = small_manifest();
    dir.fill(&m);
    let before = dir.snapshot(&m);
    let owner = CancelOwner::new();
    let a = cached_at_cancellable(&m, &dir.0, &owner.token())
        .unwrap()
        .unwrap();
    assert_eq!(dir.paths(&m).len(), 9);
    for p in [
        &a.runtime,
        &a.detection,
        &a.classification,
        &a.recognition,
        &a.dictionary,
    ] {
        assert!(p.is_absolute());
        assert!(p.starts_with(&dir.0));
    }
    assert_eq!(before, dir.snapshot(&m));
}
#[test]
fn cache_only_any_missing_runtime_model_dictionary_or_license_declines_without_repair() {
    let dir = Directory::new();
    let m = small_manifest();
    let owner = CancelOwner::new();
    for path in dir.paths(&m) {
        dir.fill(&m);
        fs::remove_file(&path).unwrap();
        let before = dir.snapshot(&m);
        assert!(cached_at_cancellable(&m, &dir.0, &owner.token())
            .unwrap()
            .is_none());
        assert_eq!(before, dir.snapshot(&m));
        assert!(!path.exists());
    }
}
#[test]
fn cache_only_size_corruption_is_error_and_never_overwritten() {
    let dir = Directory::new();
    let m = small_manifest();
    let owner = CancelOwner::new();
    for path in dir.paths(&m) {
        dir.fill(&m);
        fs::write(&path, b"ab").unwrap();
        let before = dir.snapshot(&m);
        let e = cached_at_cancellable(&m, &dir.0, &owner.token()).unwrap_err();
        assert!(e.to_string().contains("大小或类型不符"));
        assert_eq!(before, dir.snapshot(&m));
    }
}
#[test]
fn cache_only_same_size_hash_corruption_is_error_and_never_overwritten() {
    let dir = Directory::new();
    let m = small_manifest();
    let owner = CancelOwner::new();
    for path in dir.paths(&m) {
        dir.fill(&m);
        fs::write(&path, b"abd").unwrap();
        let before = dir.snapshot(&m);
        let e = cached_at_cancellable(&m, &dir.0, &owner.token()).unwrap_err();
        assert!(e.to_string().contains("SHA256 不符"));
        assert_eq!(before, dir.snapshot(&m));
    }
}
#[test]
fn cache_only_directory_in_place_of_file_is_error_and_not_removed() {
    let dir = Directory::new();
    let m = small_manifest();
    dir.fill(&m);
    let path = dir.paths(&m).remove(0);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let owner = CancelOwner::new();
    assert!(cached_at_cancellable(&m, &dir.0, &owner.token()).is_err());
    assert!(path.is_dir());
}
#[test]
fn cache_only_cancelled_probe_preserves_typed_cancel_and_existing_files() {
    let dir = Directory::new();
    let m = small_manifest();
    dir.fill(&m);
    let before = dir.snapshot(&m);
    let owner = CancelOwner::new();
    owner.cancel().unwrap();
    let e = cached_at_cancellable(&m, &dir.0, &owner.token()).unwrap_err();
    assert!(is_cancelled(e.as_ref()));
    assert_eq!(before, dir.snapshot(&m));
}
#[test]
fn cache_only_relative_root_rejected_before_file_io() {
    let owner = CancelOwner::new();
    let e = cached_at_cancellable(
        &small_manifest(),
        Path::new("relative-cache"),
        &owner.token(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("绝对路径"));
}
#[test]
fn cache_only_cancel_while_preparation_lock_is_busy_preserves_typed_cancel() {
    let mutex = Mutex::new(());
    let held = mutex.lock().unwrap();
    let owner = CancelOwner::new();
    let token = owner.token();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            wait_for_preparation(&mutex, Some(&token))
                .err()
                .map(|e| is_cancelled(e.as_ref()))
        });
        owner.cancel().unwrap();
        assert_eq!(worker.join().unwrap(), Some(true));
    });
    drop(held);
}
