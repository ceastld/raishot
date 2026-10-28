//! 人工小文件测试固定 profile/共享缓存合同；不触碰真实运行缓存，不请求网络。
use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};
use std::collections::BTreeMap;

static PROFILE_TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const ABCD_SHA: &str = "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589";

fn qa_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/qa/asset-profile-fixtures")
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let qa = qa_root();
        assert!(qa.is_absolute());
        fs::create_dir_all(&qa).unwrap();
        let path = qa.join(format!(
            "{}-{}",
            std::process::id(),
            PROFILE_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let path = fs::canonicalize(&self.0).unwrap();
        assert!(path.starts_with(fs::canonicalize(qa_root()).unwrap()));
        fs::remove_dir_all(path).unwrap();
    }
}

fn fixture_manifests() -> (Manifest, Manifest) {
    let mut tiny = manifest_for(OcrProfile::Tiny).unwrap();
    let mut small = manifest_for(OcrProfile::Small).unwrap();
    for manifest in [&mut tiny, &mut small] {
        for file in &mut manifest.runtime.files {
            file.bytes = 3;
            file.sha256 = ABC_SHA.into();
        }
        for asset in [
            &mut manifest.models.license,
            &mut manifest.models.classification,
            &mut manifest.models.detection,
            &mut manifest.models.recognition,
            &mut manifest.models.dictionary,
        ] {
            asset.bytes = 3;
            asset.sha256 = ABC_SHA.into();
        }
    }
    for asset in [
        &mut small.models.detection,
        &mut small.models.recognition,
        &mut small.models.dictionary,
    ] {
        asset.bytes = 4;
        asset.sha256 = ABCD_SHA.into();
    }
    (tiny, small)
}

fn fixture_locations(
    root: &Path,
    tiny: &Manifest,
    small: &Manifest,
) -> (CacheLocations, CacheLocations) {
    (
        cache_locations(OcrProfile::Tiny, tiny, None, root).unwrap(),
        cache_locations(OcrProfile::Small, small, Some(tiny), root).unwrap(),
    )
}

fn paths(manifest: &Manifest, locations: &CacheLocations) -> Vec<PathBuf> {
    manifest
        .runtime
        .files
        .iter()
        .map(|file| locations.runtime.parent().unwrap().join(&file.name))
        .chain(
            locations
                .model_files(&manifest.models)
                .into_iter()
                .map(|(path, _)| path.to_owned()),
        )
        .collect()
}

fn fill(manifest: &Manifest, locations: &CacheLocations) {
    for path in manifest
        .runtime
        .files
        .iter()
        .map(|file| locations.runtime.parent().unwrap().join(&file.name))
    {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"abc").unwrap();
    }
    for (path, asset) in locations.model_files(&manifest.models) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes: &[u8] = match asset.bytes {
            3 => b"abc",
            4 => b"abcd",
            _ => panic!("测试资产大小无效"),
        };
        fs::write(path, bytes).unwrap();
    }
}

fn snapshot(
    tiny: &Manifest,
    tiny_paths: &CacheLocations,
    small: &Manifest,
    small_paths: &CacheLocations,
) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    paths(tiny, tiny_paths)
        .into_iter()
        .chain(paths(small, small_paths))
        .map(|path| {
            let bytes = fs::read(&path).ok();
            (path, bytes)
        })
        .collect()
}

#[test]
fn profile_manifests_have_fixed_separate_models_and_default_manifest_is_tiny() {
    let tiny = manifest().unwrap();
    let small = manifest_for(OcrProfile::Small).unwrap();
    assert_eq!(tiny.models.profile, "PP-OCRv6-tiny");
    assert_eq!(tiny.cache_revision, "cpu-api24-tiny-1");
    assert_eq!(small.models.profile, "PP-OCRv6-small");
    assert_eq!(small.cache_revision, "cpu-api24-small-1");
    assert_eq!(small.models.detection.bytes, 9_880_512);
    assert_eq!(small.models.recognition.bytes, 21_159_378);
    assert_eq!(small.models.dictionary.name, "ppocrv6_dict.txt");
    assert_eq!(small.models.dictionary.bytes, 74_947);
    let provenance: serde_json::Value =
        serde_json::from_str(include_str!("../../dependencies/ppocr-small-cpu.json")).unwrap();
    for (kind, asset) in [
        ("detection", &small.models.detection),
        ("recognition", &small.models.recognition),
        ("dictionary", &small.models.dictionary),
    ] {
        let upstream = provenance["models"][kind]["upstream_url"].as_str().unwrap();
        assert_eq!(asset.upstream_url.as_deref(), Some(upstream));
        assert!(
            upstream.contains("/resolve/")
                || upstream.contains("/b03f46425e8ff4442b268ce449e3eef758146cd4/")
        );
        assert!(!upstream.contains("/main/"));
    }
    assert!(same_runtime(&tiny.runtime, &small.runtime));
}

fn independent_https_sources(asset: &Asset) -> Vec<Source<'_>> {
    let sources = asset.sources();
    assert_eq!(sources.len(), 2, "{} 必须有两个独立下载源", asset.name);
    let hosts: Vec<_> = sources
        .iter()
        .map(|source| {
            source
                .url
                .strip_prefix("https://")
                .expect("下载源必须使用 HTTPS")
                .split('/')
                .next()
                .unwrap()
        })
        .collect();
    assert!(!hosts[0].is_empty() && !hosts[1].is_empty());
    assert_ne!(hosts[0], hosts[1], "{} 的下载源必须独立", asset.name);
    sources
}

#[test]
fn profile_download_sources_keep_independent_mirrors_and_pinned_identities() {
    let tiny = manifest().unwrap();
    let small = manifest_for(OcrProfile::Small).unwrap();
    for profile in [&tiny, &small] {
        for asset in profile.models.recognition_assets() {
            let sources = independent_https_sources(asset);
            assert_eq!(sources[0].name, sources[1].name);
            assert_eq!(sources[0].bytes, sources[1].bytes);
            assert_eq!(sources[0].sha256, sources[1].sha256);
            assert_eq!(Some(sources[1].url), asset.upstream_url.as_deref());
        }
        let runtime = independent_https_sources(&profile.runtime.archive);
        assert_eq!(runtime[0].name, "onnxruntime-win-x64-1.24.4-runtime.zip");
        assert_eq!(runtime[0].bytes, 5_294_996);
        assert_eq!(
            runtime[0].sha256,
            "a8622a7ebf7f560cade2707070a8005f12aa2b27d0b8a6055a21feef158bd2e8"
        );
        assert_eq!(runtime[1].name, "onnxruntime-win-x64-1.24.4.zip");
        assert_eq!(runtime[1].bytes, 74_442_783);
        assert_eq!(
            runtime[1].sha256,
            "d2319fddfb6ea4db99ccc4b60c85c517bcd855721f5daa6a06d40d7cb2ee2357"
        );
    }
    let tiny_runtime = tiny.runtime.archive.sources();
    let small_runtime = small.runtime.archive.sources();
    for (tiny, small) in tiny_runtime.iter().zip(small_runtime) {
        assert_eq!(tiny.url, small.url);
        assert_eq!(tiny.name, small.name);
        assert_eq!(tiny.bytes, small.bytes);
        assert_eq!(tiny.sha256, small.sha256);
    }
}

#[test]
fn profiles_share_only_equal_runtime_classifier_and_license_identities() {
    let dir = Directory::new();
    let tiny = manifest().unwrap();
    let small = manifest_for(OcrProfile::Small).unwrap();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    assert_eq!(tiny_paths.runtime, small_paths.runtime);
    assert_eq!(tiny_paths.classification, small_paths.classification);
    assert_eq!(tiny_paths.license, small_paths.license);
    for (small, tiny) in [
        (&small_paths.detection, &tiny_paths.detection),
        (&small_paths.recognition, &tiny_paths.recognition),
        (&small_paths.dictionary, &tiny_paths.dictionary),
    ] {
        assert_ne!(small, tiny);
        assert!(small.starts_with(dir.0.join("cpu-api24-small-1")));
    }
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn unequal_classifier_or_license_uses_small_directory_without_overwriting_tiny() {
    let dir = Directory::new();
    let (tiny, mut small) = fixture_manifests();
    small.models.classification.sha256 = ABCD_SHA.into();
    small.models.license.sha256 = ABCD_SHA.into();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    assert_ne!(tiny_paths.classification, small_paths.classification);
    assert_ne!(tiny_paths.license, small_paths.license);
    assert!(small_paths
        .classification
        .starts_with(dir.0.join(&small.cache_revision)));
    assert!(small_paths
        .license
        .starts_with(dir.0.join(&small.cache_revision)));
}

#[test]
fn invalid_runtime_shared_identity_is_rejected_before_filesystem_mutation() {
    let dir = Directory::new();
    let (tiny, mut small) = fixture_manifests();
    small.runtime.files[0].sha256 = ABCD_SHA.into();
    let error = cache_locations(OcrProfile::Small, &small, Some(&tiny), &dir.0).unwrap_err();
    assert!(error.to_string().contains("共享运行库固定身份不一致"));
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn same_small_revision_or_tiny_model_identity_is_rejected() {
    let dir = Directory::new();
    let tiny = manifest().unwrap();
    let mut small = manifest_for(OcrProfile::Small).unwrap();
    small.cache_revision = tiny.cache_revision.clone();
    assert!(cache_locations(OcrProfile::Small, &small, Some(&tiny), &dir.0).is_err());
    let mut small = manifest_for(OcrProfile::Small).unwrap();
    small.models.detection.bytes = tiny.models.detection.bytes;
    small.models.detection.sha256 = tiny.models.detection.sha256.clone();
    assert!(cache_locations(OcrProfile::Small, &small, Some(&tiny), &dir.0).is_err());
    assert!(cache_locations(OcrProfile::Small, &small, None, &dir.0).is_err());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn renamed_small_model_cannot_bind_tiny_content_identity() {
    let dir = Directory::new();
    let tiny = manifest().unwrap();
    for uppercase in [false, true] {
        for role in 0..3 {
            let mut small = manifest_for(OcrProfile::Small).unwrap();
            let (small_asset, tiny_asset) = match role {
                0 => (&mut small.models.detection, &tiny.models.detection),
                1 => (&mut small.models.recognition, &tiny.models.recognition),
                _ => (&mut small.models.dictionary, &tiny.models.dictionary),
            };
            if role < 2 {
                small_asset.name = format!("small-{}", small_asset.name);
            }
            assert_ne!(small_asset.name, tiny_asset.name);
            small_asset.bytes = tiny_asset.bytes;
            small_asset.sha256 = if uppercase {
                tiny_asset.sha256.to_ascii_uppercase()
            } else {
                tiny_asset.sha256.clone()
            };
            let error =
                cache_locations(OcrProfile::Small, &small, Some(&tiny), &dir.0).unwrap_err();
            assert!(error.to_string().contains("不能静默复用 Tiny 资产"));
        }
    }
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn explicit_profile_mismatch_and_relative_root_are_rejected() {
    let dir = Directory::new();
    let tiny = manifest().unwrap();
    assert!(cache_locations(OcrProfile::Small, &tiny, Some(&tiny), &dir.0).is_err());
    assert!(cache_locations(OcrProfile::Tiny, &tiny, None, Path::new("relative")).is_err());
}

#[test]
fn empty_small_readonly_probe_is_none_and_never_creates_directories() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (_, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    let owner = CancelOwner::new();
    assert!(
        cached_locations_cancellable(OcrProfile::Small, &small, &small_paths, &owner.token())
            .unwrap()
            .is_none()
    );
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
}

#[test]
fn tiny_only_cache_is_ready_for_tiny_but_never_satisfies_small() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    fill(&tiny, &tiny_paths);
    let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
    let owner = CancelOwner::new();
    let result = cached_locations_cancellable(OcrProfile::Tiny, &tiny, &tiny_paths, &owner.token())
        .unwrap()
        .unwrap();
    assert_eq!(result.profile, OcrProfile::Tiny);
    assert!(
        cached_locations_cancellable(OcrProfile::Small, &small, &small_paths, &owner.token())
            .unwrap()
            .is_none()
    );
    assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
    assert!(!dir.0.join(&small.cache_revision).exists());
}

#[test]
fn complete_small_probe_uses_shared_files_and_its_own_dictionary_without_mutation() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    fill(&tiny, &tiny_paths);
    fill(&small, &small_paths);
    let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
    assert_eq!(before.len(), 12);
    let owner = CancelOwner::new();
    let result =
        cached_locations_cancellable(OcrProfile::Small, &small, &small_paths, &owner.token())
            .unwrap()
            .unwrap();
    assert_eq!(result.profile, OcrProfile::Small);
    assert_eq!(result.runtime, tiny_paths.runtime);
    assert_eq!(result.classification, tiny_paths.classification);
    assert_eq!(result.dictionary, small_paths.dictionary);
    assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
}

#[test]
fn missing_each_small_asset_declines_without_mutation_and_tiny_remains_ready() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    let owner = CancelOwner::new();
    for path in [
        &small_paths.detection,
        &small_paths.recognition,
        &small_paths.dictionary,
    ] {
        fill(&tiny, &tiny_paths);
        fill(&small, &small_paths);
        fs::remove_file(path).unwrap();
        let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
        assert!(cached_locations_cancellable(
            OcrProfile::Small,
            &small,
            &small_paths,
            &owner.token()
        )
        .unwrap()
        .is_none());
        assert!(
            cached_locations_cancellable(OcrProfile::Tiny, &tiny, &tiny_paths, &owner.token())
                .unwrap()
                .is_some()
        );
        assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
    }
}

#[test]
fn corrupt_small_same_length_hash_reports_error_preserves_files_and_tiny() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    fill(&tiny, &tiny_paths);
    fill(&small, &small_paths);
    fs::write(&small_paths.recognition, b"abce").unwrap();
    let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
    let owner = CancelOwner::new();
    let error =
        cached_locations_cancellable(OcrProfile::Small, &small, &small_paths, &owner.token())
            .unwrap_err();
    assert!(error.to_string().contains("SHA256 不符"));
    assert!(
        cached_locations_cancellable(OcrProfile::Tiny, &tiny, &tiny_paths, &owner.token())
            .unwrap()
            .is_some()
    );
    assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
}

#[test]
fn cancelled_small_probe_preserves_typed_cancel_and_tiny_files() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    fill(&tiny, &tiny_paths);
    fill(&small, &small_paths);
    let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
    let owner = CancelOwner::new();
    owner.cancel().unwrap();
    let error =
        cached_locations_cancellable(OcrProfile::Small, &small, &small_paths, &owner.token())
            .unwrap_err();
    assert!(is_cancelled(error.as_ref()));
    assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
}

#[test]
fn pre_cancelled_small_public_entries_stop_before_cache_or_network() {
    let owner = CancelOwner::new();
    owner.cancel().unwrap();
    let task = owner.token();
    for result in [
        ensure_profile_cancellable(OcrProfile::Small, &task).map(|_| ()),
        ensure_det_profile_cancellable(OcrProfile::Small, &task).map(|_| ()),
        cached_profile_cancellable(OcrProfile::Small, &task).map(|_| ()),
    ] {
        assert!(is_cancelled(result.unwrap_err().as_ref()));
    }
}

#[test]
fn small_detection_plan_excludes_classifier_recognizer_and_dictionary() {
    let small = manifest_for(OcrProfile::Small).unwrap();
    assert_eq!(
        small
            .models
            .detection_assets()
            .map(|asset| asset.name.as_str()),
        ["LICENSE-PaddleOCR.txt", "det.onnx"]
    );
}

#[test]
fn shared_small_download_entries_return_verified_tiny_files_without_network() {
    let dir = Directory::new();
    let (tiny, mut small) = fixture_manifests();
    small.models.classification.url = "not-a-network-url".into();
    small.models.license.url = "not-a-network-url".into();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    fill(&tiny, &tiny_paths);
    let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
    let owner = CancelOwner::new();
    for (path, asset) in [
        (&small_paths.classification, &small.models.classification),
        (&small_paths.license, &small.models.license),
    ] {
        assert_eq!(
            download_at_cancellable(path, asset, Some(&owner.token())).unwrap(),
            *path
        );
    }
    assert!(!dir.0.join(&small.cache_revision).exists());
    assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
}

#[test]
fn cancelled_small_pending_publication_releases_candidate_and_preserves_tiny() {
    let dir = Directory::new();
    let (tiny, small) = fixture_manifests();
    let (tiny_paths, small_paths) = fixture_locations(&dir.0, &tiny, &small);
    fill(&tiny, &tiny_paths);
    fs::create_dir_all(small_paths.recognition.parent().unwrap()).unwrap();
    let before = snapshot(&tiny, &tiny_paths, &small, &small_paths);
    let owner = CancelOwner::new();
    let task = owner.token();
    {
        let mut candidate =
            Pending::new_cancellable(&small_paths.recognition, Some(&task)).unwrap();
        candidate.file.as_mut().unwrap().write_all(b"abcd").unwrap();
        owner.cancel().unwrap();
        let error = candidate
            .publish_cancellable(
                &small_paths.recognition,
                small.models.recognition.bytes,
                &small.models.recognition.sha256,
                Some(&task),
            )
            .unwrap_err();
        assert!(is_cancelled(error.as_ref()));
    }
    assert_eq!(before, snapshot(&tiny, &tiny_paths, &small, &small_paths));
    assert_eq!(
        fs::read_dir(small_paths.recognition.parent().unwrap())
            .unwrap()
            .count(),
        0
    );
    let live_owner = CancelOwner::new();
    assert!(cached_locations_cancellable(
        OcrProfile::Tiny,
        &tiny,
        &tiny_paths,
        &live_owner.token()
    )
    .unwrap()
    .is_some());
}
