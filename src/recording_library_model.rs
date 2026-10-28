//! 只读、顶层、有界录屏历史列表；媒体结构检查与窗口生命周期相互独立。

use std::{
    cmp::Ordering,
    collections::HashSet,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use crate::recording_library_metadata::{self, Probe, VideoInfo};

pub(crate) const MAX_DIRECTORY_ITEMS: usize = 4096;
pub(crate) const MAX_ENTRIES: usize = 200;
const CANCELLED: &str = "录屏历史扫描已取消";

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub modified: Option<SystemTime>,
    pub info: VideoInfo,
}

#[derive(Default, Debug)]
pub(crate) struct Scan {
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileDetails {
    regular: bool,
    reparse: bool,
    bytes: u64,
    modified: Option<SystemTime>,
}

impl FileDetails {
    fn eligible(self) -> bool {
        self.regular && !self.reparse && self.bytes != 0
    }
}

#[derive(Default)]
struct ScanNotes {
    directory_limit: bool,
    list_limit: bool,
    unreadable: usize,
    disappeared: usize,
    unavailable: usize,
    changed: usize,
    first_error: Option<String>,
}

impl ScanNotes {
    fn record_error(&mut self, error: io::Error) {
        if error.kind() == io::ErrorKind::NotFound {
            self.disappeared += 1;
        } else {
            self.unreadable += 1;
            // 只保存一个有界错误，避免目录项失败累积无限日志或路径。
            self.first_error
                .get_or_insert_with(|| error.to_string().chars().take(512).collect());
        }
    }

    fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.directory_limit {
            warnings.push(trf!(
                "目录检查已达 {MAX_DIRECTORY_ITEMS} 项上限；结果仅按已检查范围排序。",
                "The {MAX_DIRECTORY_ITEMS}-item scan limit was reached; results cover inspected items only."
            ));
        }
        if self.list_limit {
            warnings.push(trf!(
                "最多显示已检查范围内最新的 {MAX_ENTRIES} 个候选；不完整候选不会回补。",
                "Only the newest {MAX_ENTRIES} inspected candidates are shown; incomplete items aren't replaced."
            ));
        }
        if self.unavailable != 0 {
            let count = self.unavailable;
            warnings.push(trf!(
                "已跳过 {count} 个不完整、结构不合格或元数据检查超限的文件。",
                "Skipped {count} incomplete, invalid, or over-budget files."
            ));
        }
        if self.disappeared != 0 {
            let count = self.disappeared;
            warnings.push(trf!(
                "已移除 {count} 个已消失的文件。",
                "Removed {count} missing files."
            ));
        }
        if self.changed != 0 {
            let count = self.changed;
            warnings.push(trf!(
                "已跳过 {count} 个检查期间改变的文件；刷新可重新检查。",
                "Skipped {count} files that changed; refresh to check again."
            ));
        }
        if let Some(error) = &self.first_error {
            let count = self.unreadable;
            warnings.push(trf!(
                "{count} 项读取失败：{error}",
                "Couldn't read {count} items: {error}"
            ));
        }
        warnings
    }
}

/// 只从当前及曾用品牌的固定录屏目录读取；缺少目录等同空库，不创建目录。
pub(crate) fn scan(cancelled: &dyn Fn() -> bool) -> Result<Scan, String> {
    scan_with_probe(cancelled, &mut recording_library_metadata::probe)
}

/// 统一库可按文件身份复用容器元数据；候选范围、预算与前后版本检查仍由此处负责。
pub(crate) fn scan_with_probe<P>(
    cancelled: &dyn Fn() -> bool,
    probe: &mut P,
) -> Result<Scan, String>
where
    P: FnMut(&Path, &dyn Fn() -> bool) -> io::Result<Probe>,
{
    ensure_not_cancelled(cancelled)?;
    scan_directories_with_probe(&history_directories(), probe, cancelled)
}

fn history_directories() -> Vec<PathBuf> {
    directories_in(
        crate::recording::library_dir(),
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .as_deref(),
        std::env::var_os("RUSTSHOT_DATA_DIR").is_some(),
    )
}

fn directories_in(current: PathBuf, local: Option<&Path>, custom: bool) -> Vec<PathBuf> {
    let mut directories = vec![current];
    if !custom {
        if let Some(local) = local {
            for name in crate::brand::LEGACY_NAMES.iter().rev() {
                let directory = local.join(name).join("recordings");
                if !directories.contains(&directory) {
                    directories.push(directory);
                }
            }
        }
    }
    directories
}

fn scan_directories_with_probe<P>(
    directories: &[PathBuf],
    probe: &mut P,
    cancelled: &dyn Fn() -> bool,
) -> Result<Scan, String>
where
    P: FnMut(&Path, &dyn Fn() -> bool) -> io::Result<Probe>,
{
    let mut combined = Scan::default();
    let mut seen_directories = HashSet::new();
    let mut seen_paths = HashSet::new();
    for directory in directories {
        ensure_not_cancelled(cancelled)?;
        if !seen_directories.insert(directory) {
            continue;
        }
        match scan_directory_with_probe(directory, probe, cancelled) {
            Ok(scan) => {
                combined.truncated |= scan.truncated;
                combined.warnings.extend(scan.warnings);
                combined.entries.extend(
                    scan.entries
                        .into_iter()
                        .filter(|entry| seen_paths.insert(entry.path.clone())),
                );
            }
            Err(_) if cancelled() => return Err(CANCELLED.into()),
            Err(error) => combined.warnings.push(trf!(
                "录屏目录读取失败（{}）：{error}",
                "Recording folder failed to load ({}): {error}",
                directory.display()
            )),
        }
    }
    combined.entries.sort_by(order_entries);
    if combined.entries.len() > MAX_ENTRIES {
        combined.truncated = true;
        combined.entries.truncate(MAX_ENTRIES);
    }
    ensure_not_cancelled(cancelled)?;
    Ok(combined)
}

fn scan_directory_with_probe<P>(
    directory: &Path,
    probe: &mut P,
    cancelled: &dyn Fn() -> bool,
) -> Result<Scan, String>
where
    P: FnMut(&Path, &dyn Fn() -> bool) -> io::Result<Probe>,
{
    ensure_not_cancelled(cancelled)?;
    let root = match fs::symlink_metadata(directory) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure_not_cancelled(cancelled)?;
            return Ok(Scan::default());
        }
        Err(error) => return Err(format!("无法读取录屏历史目录：{error}")),
    };
    if !root.is_dir() || metadata_is_reparse(&root) {
        return Err("录屏历史目录不是普通目录".into());
    }
    crate::image_collection::ensure_local_directory(directory)?;
    let items = fs::read_dir(directory)
        .map_err(|error| format!("无法列出录屏历史目录：{error}"))?
        .map(|item| item.map(|item| item.file_name()));
    let mut local_probe = |path: &Path, cancelled: &dyn Fn() -> bool| {
        if crate::image_collection::ensure_local_file(path).is_err() {
            return Ok(Probe::Unavailable);
        }
        probe(path, cancelled)
    };
    scan_with(
        directory,
        items,
        &mut inspect_file,
        &mut local_probe,
        cancelled,
    )
}

fn inspect_file(path: &Path) -> io::Result<FileDetails> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(FileDetails {
        regular: metadata.is_file(),
        reparse: metadata_is_reparse(&metadata),
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_REPARSE_POINT；symlink_metadata 不跟随重解析目标。
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn ensure_not_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    if cancelled() {
        Err(CANCELLED.into())
    } else {
        Ok(())
    }
}

/// 目录枚举、文件属性和媒体探针均可注入；纯模型不访问真实库或媒体。
fn scan_with<I, M, P>(
    directory: &Path,
    items: I,
    metadata: &mut M,
    probe: &mut P,
    cancelled: &dyn Fn() -> bool,
) -> Result<Scan, String>
where
    I: IntoIterator<Item = io::Result<OsString>>,
    M: FnMut(&Path) -> io::Result<FileDetails>,
    P: FnMut(&Path, &dyn Fn() -> bool) -> io::Result<Probe>,
{
    ensure_not_cancelled(cancelled)?;
    let mut notes = ScanNotes::default();
    let mut candidates: Vec<Entry> = Vec::with_capacity(MAX_ENTRIES);
    let mut iterator = items.into_iter();
    let mut checked = 0;
    while checked < MAX_DIRECTORY_ITEMS {
        ensure_not_cancelled(cancelled)?;
        let Some(item) = iterator.next() else { break };
        checked += 1;
        let name = match item {
            Ok(name) => name,
            Err(error) => {
                notes.record_error(error);
                continue;
            }
        };
        if !published_name(&name) {
            continue;
        }
        let path = directory.join(&name);
        let details = match metadata(&path) {
            Ok(details) => details,
            Err(error) => {
                notes.record_error(error);
                continue;
            }
        };
        if !details.eligible() {
            continue;
        }
        let candidate = Entry {
            path,
            name: name.to_string_lossy().into_owned(),
            bytes: details.bytes,
            modified: details.modified,
            info: VideoInfo::default(),
        };
        let position = candidates
            .binary_search_by(|entry| order_entries(entry, &candidate))
            .unwrap_or_else(|position| position);
        if candidates.len() == MAX_ENTRIES {
            notes.list_limit = true;
            if position >= MAX_ENTRIES {
                continue;
            }
            // 先弹出末项，始终最多持有 200 个候选。
            candidates.pop();
        }
        candidates.insert(position, candidate);
    }
    // 不为探测第 4097 项而继续枚举；恰好到上限时也保守提示范围可能不完整。
    notes.directory_limit = checked == MAX_DIRECTORY_ITEMS;
    let mut entries = Vec::with_capacity(candidates.len());
    for mut entry in candidates {
        ensure_not_cancelled(cancelled)?;
        let info = match probe(&entry.path, cancelled) {
            Ok(Probe::Complete(info)) => info,
            Ok(Probe::Unavailable) => {
                notes.unavailable += 1;
                continue;
            }
            Ok(Probe::Cancelled) => return Err(CANCELLED.into()),
            Err(error) => {
                notes.record_error(error);
                continue;
            }
        };
        ensure_not_cancelled(cancelled)?;
        // 拒绝检查期间消失、被替换成重解析点或仍在写入的文件。
        let current = match metadata(&entry.path) {
            Ok(current) => current,
            Err(error) => {
                notes.record_error(error);
                continue;
            }
        };
        if !current.eligible() || current.bytes != entry.bytes || current.modified != entry.modified
        {
            notes.changed += 1;
            continue;
        }
        entry.info = info;
        entries.push(entry);
    }
    ensure_not_cancelled(cancelled)?;
    Ok(Scan {
        entries,
        truncated: notes.directory_limit || notes.list_limit,
        warnings: notes.warnings(),
    })
}

fn published_name(name: &std::ffi::OsStr) -> bool {
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return false;
    }
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
    {
        return false;
    }
    let name = name.to_string_lossy().to_lowercase();
    if name.starts_with(".rustshot-")
        || name.ends_with(".gif.mp4")
        || name.ends_with(".mix.mp4")
        || name.ends_with(".wav.mp4")
        || ["缓存", "预览", "混音", "未完成", "临时"]
            .iter()
            .any(|part| name.contains(part))
    {
        return false;
    }
    let stem = name.strip_suffix(".mp4").unwrap_or(&name);
    !stem.split(['.', '_', '-', ' ']).any(|part| {
        matches!(
            part,
            "partial"
                | "temp"
                | "tmp"
                | "incomplete"
                | "unfinished"
                | "cache"
                | "preview"
                | "mix"
                | "mixed"
                | "mixing"
        )
    })
}

fn order_entries(left: &Entry, right: &Entry) -> Ordering {
    right
        .modified
        .cmp(&left.modified)
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.path.cmp(&right.path))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenAction {
    Open,
    Locate,
    Folder,
}

/// 用系统应用打开成片、在资源管理器中选中，或打开录屏目录。
pub(crate) fn open(path: &Path, action: OpenAction) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    checked_target(path, action)?;
    let mut command = std::process::Command::new("explorer.exe");
    if matches!(action, OpenAction::Locate) {
        // Windows 文件名不能含引号；标准参数转义会错误地包住 /select 开关。
        command.raw_arg(format!("/select,\"{}\"", path.display()));
    } else {
        command.arg(path);
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// 校验成片来源后在独立内置播放器中打开，不阻塞历史窗口。
pub(crate) fn play(path: &Path) -> Result<(), String> {
    checked_target(path, OpenAction::Open)?;
    crate::file_launch_command::open_video(path).map_err(|error| error.to_string())
}

/// 把成片移到回收站；只接受当前及曾用品牌录屏目录顶层的成片。
pub(crate) fn recycle(path: &Path) -> crate::Result<()> {
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };
    checked_target(path, OpenAction::Locate)?;
    // pFrom 需要双 NUL 结尾。
    let from: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
        .chain([0, 0])
        .collect();
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut operation) };
    if code != 0 || operation.fAnyOperationsAborted.as_bool() {
        return Err(crate::trf!(
            "移到回收站失败（{code}）",
            "Couldn't move to the Recycle Bin ({code})"
        )
        .into());
    }
    Ok(())
}

/// 缩略图沿用成片打开的固定目录资格；实际读取者仍须持有自己的文件身份保护。
pub(crate) fn check_thumbnail_source(path: &Path) -> Result<(), String> {
    checked_target(path, OpenAction::Open)
}

/// 点击时重新检查固定根，不能沿新出现的 junction 打开旧条目。
fn checked_target(path: &Path, action: OpenAction) -> Result<(), String> {
    checked_target_in(&history_directories(), path, action)
}

fn checked_target_in(
    directories: &[PathBuf],
    path: &Path,
    action: OpenAction,
) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    let directory = directories
        .iter()
        .find(|directory| match action {
            OpenAction::Folder => path == directory.as_path(),
            OpenAction::Open | OpenAction::Locate => path.parent() == Some(directory.as_path()),
        })
        .ok_or_else(|| {
            crate::tr!(
                "目标不属于固定录屏历史目录中的成片",
                "The target is not a finished recording in the recording history folder"
            )
            .to_owned()
        })?;
    crate::image_collection::ensure_local_directory(directory)?;
    let root = fs::symlink_metadata(directory).map_err(|e| e.to_string())?;
    validate_open_scope(
        directory,
        path,
        action,
        root.is_dir(),
        root.file_attributes() & 0x400 != 0,
    )?;
    if matches!(action, OpenAction::Folder) {
        crate::image_collection::ensure_local_directory(path)?;
    } else {
        crate::image_collection::ensure_local_file(path)?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if metadata.file_attributes() & 0x400 != 0 {
        return Err(
            crate::tr!("目标已经变为重解析项", "The target became a reparse point").to_owned(),
        );
    }
    if if matches!(action, OpenAction::Folder) {
        !metadata.is_dir()
    } else {
        !metadata.is_file() || metadata.len() == 0
    } {
        return Err(crate::tr!("目标类型已经变化", "The target type changed").to_owned());
    }
    Ok(())
}

/// 纯路径与属性合同；不规范化或跟随重解析路径，也不读取真实录屏库。
fn validate_open_scope(
    directory: &Path,
    path: &Path,
    action: OpenAction,
    root_is_directory: bool,
    root_is_reparse: bool,
) -> Result<(), String> {
    if !root_is_directory || root_is_reparse {
        return Err(crate::tr!(
            "录屏历史目录不是普通目录",
            "The recording history folder is not a regular folder"
        )
        .to_owned());
    }
    match action {
        OpenAction::Folder if path == directory => Ok(()),
        OpenAction::Open | OpenAction::Locate
            if path.parent() == Some(directory) && path.file_name().is_some_and(published_name) =>
        {
            Ok(())
        }
        _ => Err(crate::tr!(
            "目标不属于固定录屏历史目录中的成片",
            "The target is not a finished recording in the recording history folder"
        )
        .to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
        time::{Duration, UNIX_EPOCH},
    };

    struct DirectoryFixture(PathBuf);
    impl DirectoryFixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "rustshot-recording-library-roots-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn video(&self, directory: &Path, name: &str) -> PathBuf {
            assert!(directory.starts_with(&self.0));
            fs::create_dir_all(directory).unwrap();
            let path = directory.join(name);
            fs::write(&path, b"owned metadata-probe fixture").unwrap();
            path
        }
    }
    impl Drop for DirectoryFixture {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            assert!(self
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rustshot-recording-library-roots-"));
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recording_library_brand_roots_include_previous_brands_only_for_default_storage() {
        let base = Path::new("local-data");
        let current = base.join(crate::brand::DATA_DIR).join("recordings");
        let roots = directories_in(current.clone(), Some(base), false);
        assert_eq!(roots.first(), Some(&current));
        for name in crate::brand::LEGACY_NAMES {
            assert!(roots.contains(&base.join(name).join("recordings")));
        }
        assert_eq!(
            directories_in(current.clone(), Some(base), true),
            [current.clone()]
        );
        assert_eq!(directories_in(current.clone(), None, false), [current]);
    }

    #[test]
    fn recording_library_previous_directory_survives_missing_current_and_partial_failure() {
        let fixture = DirectoryFixture::new();
        let current = fixture.0.join(crate::brand::DATA_DIR).join("recordings");
        let previous = fixture
            .0
            .join(crate::brand::LEGACY_NAMES.last().unwrap())
            .join("recordings");
        let old_file = fixture.video(&previous, "Recording_20261022.mp4");
        let mut roots = directories_in(current.clone(), Some(&fixture.0), false);
        let mut probe = |_: &Path, _: &dyn Fn() -> bool| Ok(Probe::Complete(VideoInfo::default()));
        let scan = scan_directories_with_probe(&roots, &mut probe, &|| false).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].path, old_file);
        assert!(!current.exists());
        for action in [OpenAction::Open, OpenAction::Locate] {
            assert!(checked_target_in(&roots, &old_file, action).is_ok());
        }
        assert!(checked_target_in(&roots, &previous, OpenAction::Folder).is_ok());
        let custom_roots = directories_in(current.clone(), Some(&fixture.0), true);
        assert!(checked_target_in(&custom_roots, &old_file, OpenAction::Open).is_err());
        assert!(
            scan_directories_with_probe(&custom_roots, &mut probe, &|| false)
                .unwrap()
                .entries
                .is_empty()
        );

        let new_file = fixture.video(&current, "Recording_20261022.mp4");
        roots.push(previous);
        let bad_root = fixture.0.join("unreadable-recordings");
        fs::write(&bad_root, b"not a directory").unwrap();
        roots.push(bad_root);
        let scan = scan_directories_with_probe(&roots, &mut probe, &|| false).unwrap();
        assert_eq!(scan.entries.len(), 2);
        assert!(scan.entries.iter().any(|entry| entry.path == old_file));
        assert!(scan.entries.iter().any(|entry| entry.path == new_file));
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan_directories_with_probe(&roots, &mut probe, &|| true).is_err());
    }

    #[test]
    fn recording_library_combined_brand_roots_keep_global_list_limit() {
        let fixture = DirectoryFixture::new();
        let roots = [fixture.0.join("current"), fixture.0.join("previous")];
        for root in &roots {
            for index in 0..110 {
                fixture.video(root, &format!("Recording_{index:03}.mp4"));
            }
        }
        let scan = scan_directories_with_probe(
            &roots,
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| false,
        )
        .unwrap();
        assert_eq!(scan.entries.len(), MAX_ENTRIES);
        assert!(scan.truncated);
        assert!(scan
            .entries
            .windows(2)
            .all(|pair| order_entries(&pair[0], &pair[1]) != Ordering::Greater));
    }

    fn file(seconds: Option<u64>) -> FileDetails {
        FileDetails {
            regular: true,
            reparse: false,
            bytes: 100,
            modified: seconds.map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds)),
        }
    }

    fn entry(name: &str, seconds: Option<u64>) -> Entry {
        let details = file(seconds);
        Entry {
            path: PathBuf::from("library").join(name),
            name: name.into(),
            bytes: details.bytes,
            modified: details.modified,
            info: VideoInfo::default(),
        }
    }

    fn scan_names(names: &[&str], details: FileDetails) -> Scan {
        scan_with(
            Path::new("library"),
            names.iter().map(|name| Ok(OsString::from(*name))),
            &mut |_| Ok(details),
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| false,
        )
        .unwrap()
    }

    // 普通 MP4 与大小写扩展名可纳入。
    #[test]
    fn recording_library_ordinary_mp4_names_are_case_insensitive() {
        let scan = scan_names(
            &["录屏_20261010.mp4", "Recording.MP4", "attempt.mp4"],
            file(Some(1)),
        );
        assert_eq!(scan.entries.len(), 3);
    }

    // 暂存、预览、混音和音轨名称不进入历史。
    #[test]
    fn recording_library_temporary_preview_mix_and_audio_names_are_rejected() {
        let names = [
            ".rustshot-recording-1.mp4",
            "recording.gif.mp4",
            "recording.mix.mp4",
            "foo.partial.mp4",
            "foo_tmp.mp4",
            "temp-file.MP4",
            "incomplete.mp4",
            "unfinished.mp4",
            "preview-cache.mp4",
            "mixed.mp4",
            "预览_1.mp4",
            "混音缓存.mp4",
            "system.wav",
            "mic.wav",
            "system.wav.mp4",
            "picture.gif",
            "../escape.mp4",
        ];
        assert!(scan_names(&names, file(Some(1))).entries.is_empty());
    }

    // 目录、链接、重解析点与空文件不进入历史。
    #[test]
    fn recording_library_directories_symlinks_reparse_points_and_empty_files_are_rejected() {
        for details in [
            FileDetails {
                regular: false,
                ..file(Some(1))
            },
            FileDetails {
                reparse: true,
                ..file(Some(1))
            },
            FileDetails {
                bytes: 0,
                ..file(Some(1))
            },
        ] {
            assert!(scan_names(&["video.mp4"], details).entries.is_empty());
        }
    }

    // 缓存名称不触发文件属性或媒体读取。
    #[test]
    fn recording_library_cache_names_do_not_trigger_metadata_or_probe() {
        let metadata_calls = Cell::new(0);
        let probe_calls = Cell::new(0);
        let scan = scan_with(
            Path::new("library"),
            [Ok(OsString::from("foo.mix.mp4"))],
            &mut |_| {
                metadata_calls.set(metadata_calls.get() + 1);
                Ok(file(Some(1)))
            },
            &mut |_, _| {
                probe_calls.set(probe_calls.get() + 1);
                Ok(Probe::Complete(VideoInfo::default()))
            },
            &|| false,
        )
        .unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(metadata_calls.get(), 0);
        assert_eq!(probe_calls.get(), 0);
    }

    // 修改时间倒序，未知日期在后，同时间按名称稳定排序。
    #[test]
    fn recording_library_newest_first_unknown_dates_last_and_names_break_ties() {
        let names = ["z.mp4", "B.mp4", "a.mp4", "missing-date.mp4", "new.mp4"];
        let scan = scan_with(
            Path::new("library"),
            names.map(|name| Ok(OsString::from(name))),
            &mut |path| {
                Ok(match path.file_name().unwrap().to_str().unwrap() {
                    "missing-date.mp4" => file(None),
                    "new.mp4" => file(Some(3)),
                    _ => file(Some(1)),
                })
            },
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| false,
        )
        .unwrap();
        assert_eq!(
            scan.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["new.mp4", "a.mp4", "B.mp4", "z.mp4", "missing-date.mp4"]
        );
    }

    // 候选数量先限制为 200，探针只读取这一有限集合。
    #[test]
    fn recording_library_top_candidates_are_bounded_before_probe() {
        let probes = Cell::new(0);
        let names = (0..350).map(|index| Ok(OsString::from(format!("{index:04}.mp4"))));
        let scan = scan_with(
            Path::new("library"),
            names,
            &mut |path| {
                Ok(file(Some(
                    path.file_stem().unwrap().to_str().unwrap().parse().unwrap(),
                )))
            },
            &mut |_, _| {
                probes.set(probes.get() + 1);
                Ok(Probe::Complete(VideoInfo::default()))
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(scan.entries.len(), MAX_ENTRIES);
        assert_eq!(probes.get(), MAX_ENTRIES);
        assert_eq!(scan.entries.first().unwrap().name, "0349.mp4");
        assert_eq!(scan.entries.last().unwrap().name, "0150.mp4");
        assert!(scan.truncated);
        assert!(!scan.warnings.is_empty());
    }

    // 目录检查到 4096 停止，不消费第 4097 项。
    #[test]
    fn recording_library_directory_limit_never_consumes_item_4097() {
        let consumed = Cell::new(0);
        let names = std::iter::from_fn(|| {
            consumed.set(consumed.get() + 1);
            Some(Ok(OsString::from("cache.mp4")))
        });
        let scan = scan_with(
            Path::new("library"),
            names,
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| false,
        )
        .unwrap();
        assert_eq!(consumed.get(), MAX_DIRECTORY_ITEMS);
        assert!(scan.truncated);
        assert!(!scan.warnings.is_empty());
    }

    // 完整容器可保留未知的视频元数据。
    #[test]
    fn recording_library_unknown_video_fields_are_accepted_only_when_container_is_complete() {
        let scan = scan_names(&["video.mp4"], file(Some(1)));
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.entries[0].info, VideoInfo::default());
    }

    // 不能确认完整的容器跳过并留下可见说明。
    #[test]
    fn recording_library_unavailable_container_is_skipped_with_visible_warning() {
        let scan = scan_with(
            Path::new("library"),
            [Ok(OsString::from("video.mp4"))],
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| Ok(Probe::Unavailable),
            &|| false,
        )
        .unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(scan.warnings.len(), 1);
    }

    // 媒体探测期间消失的文件从回执移除。
    #[test]
    fn recording_library_disappearing_file_during_probe_is_removed() {
        let scan = scan_with(
            Path::new("library"),
            [Ok(OsString::from("video.mp4"))],
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| Err(io::Error::from(io::ErrorKind::NotFound)),
            &|| false,
        )
        .unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(scan.warnings.len(), 1);
    }

    // 媒体探测后消失的文件从回执移除。
    #[test]
    fn recording_library_disappearing_file_after_probe_is_removed() {
        let calls = Cell::new(0);
        let scan = scan_with(
            Path::new("library"),
            [Ok(OsString::from("video.mp4"))],
            &mut |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Ok(file(Some(1)))
                } else {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                }
            },
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| false,
        )
        .unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(scan.warnings.len(), 1);
    }

    // 探测后大小、时间或路径资格改变的文件不纳入。
    #[test]
    fn recording_library_changed_size_time_or_reparse_after_probe_is_rejected() {
        for changed in [
            FileDetails {
                bytes: 101,
                ..file(Some(1))
            },
            file(Some(2)),
            FileDetails {
                reparse: true,
                ..file(Some(1))
            },
        ] {
            let calls = Cell::new(0);
            let scan = scan_with(
                Path::new("library"),
                [Ok(OsString::from("video.mp4"))],
                &mut |_| {
                    calls.set(calls.get() + 1);
                    Ok(if calls.get() == 1 {
                        file(Some(1))
                    } else {
                        changed
                    })
                },
                &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
                &|| false,
            )
            .unwrap();
            assert!(scan.entries.is_empty());
            assert_eq!(scan.warnings.len(), 1);
        }
    }

    // 读取错误可见，数量汇总与错误文本占用有界。
    #[test]
    fn recording_library_read_errors_are_visible_and_bounded() {
        let scan = scan_with(
            Path::new("library"),
            (0..100).map(|_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "x".repeat(1000),
                ))
            }),
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| false,
        )
        .unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].chars().count() < 600);
    }

    // 开始前取消不枚举目录或读取媒体。
    #[test]
    fn recording_library_pre_cancel_does_not_enumerate_or_probe() {
        let consumed = Cell::new(0);
        let items = std::iter::from_fn(|| {
            consumed.set(consumed.get() + 1);
            Some(Ok(OsString::from("video.mp4")))
        });
        let result = scan_with(
            Path::new("library"),
            items,
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| Ok(Probe::Complete(VideoInfo::default())),
            &|| true,
        );
        assert_eq!(result.unwrap_err(), CANCELLED);
        assert_eq!(consumed.get(), 0);
    }

    // 探针取消不能将部分列表回报成功。
    #[test]
    fn recording_library_probe_cancel_cannot_return_partial_list_as_success() {
        let result = scan_with(
            Path::new("library"),
            [Ok(OsString::from("video.mp4"))],
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| Ok(Probe::Cancelled),
            &|| false,
        );
        assert_eq!(result.unwrap_err(), CANCELLED);
    }

    // 探针后的取消不能将部分列表回报成功。
    #[test]
    fn recording_library_stop_after_probe_cannot_return_partial_list_as_success() {
        let cancelled = Cell::new(false);
        let result = scan_with(
            Path::new("library"),
            [Ok(OsString::from("video.mp4"))],
            &mut |_| Ok(file(Some(1))),
            &mut |_, _| {
                cancelled.set(true);
                Ok(Probe::Complete(VideoInfo::default()))
            },
            &|| cancelled.get(),
        );
        assert_eq!(result.unwrap_err(), CANCELLED);
    }

    // 候选结构不完整时不扩大探测范围回补旧文件。
    #[test]
    fn recording_library_incomplete_top_candidates_are_not_replaced_by_unprobed_older_items() {
        let probes = Cell::new(0);
        let names = (0..250).map(|index| Ok(OsString::from(format!("{index:04}.mp4"))));
        let scan = scan_with(
            Path::new("library"),
            names,
            &mut |path| {
                Ok(file(Some(
                    path.file_stem().unwrap().to_str().unwrap().parse().unwrap(),
                )))
            },
            &mut |_, _| {
                probes.set(probes.get() + 1);
                Ok(Probe::Unavailable)
            },
            &|| false,
        )
        .unwrap();
        assert!(scan.entries.is_empty());
        assert_eq!(probes.get(), MAX_ENTRIES);
        assert!(scan.truncated);
        assert_eq!(scan.warnings.len(), 2);
    }

    // 同时间和同名称仍按原名与路径稳定破除平局。
    #[test]
    fn recording_library_equal_dates_and_case_folded_names_have_stable_path_tie_breaks() {
        let mut entries = [
            entry("a.mp4", Some(1)),
            entry("A.mp4", Some(1)),
            entry("a.mp4", Some(1)),
        ];
        entries[2].path = PathBuf::from("other/a.mp4");
        entries.sort_by(order_entries);
        assert_eq!(entries[0].name, "A.mp4");
        assert_eq!(entries[1].path, PathBuf::from("library/a.mp4"));
        assert_eq!(entries[2].path, PathBuf::from("other/a.mp4"));
    }

    #[test]
    fn recording_library_open_scope_rechecks_changed_root() {
        let directory = Path::new("library");
        let target = directory.join("Recording.mp4");
        for action in [OpenAction::Open, OpenAction::Locate, OpenAction::Folder] {
            let path = if matches!(action, OpenAction::Folder) {
                directory
            } else {
                target.as_path()
            };
            // 旧列表即使仍有条目，根变 junction 或普通文件后也不能打开。
            assert!(validate_open_scope(directory, path, action, true, true).is_err());
            assert!(validate_open_scope(directory, path, action, false, false).is_err());
        }
    }

    #[test]
    fn recording_library_open_scope_rejects_external_nested_and_unpublished_targets() {
        let directory = Path::new("library");
        for name in [
            "nested/Recording.mp4",
            "../Recording.mp4",
            "nested/../Recording.mp4",
            ".rustshot-recording-1/video.mp4",
            ".rustshot-recording-1.mp4",
            "Recording.gif.mp4",
            "Recording.mix.mp4",
            "Recording.wav.mp4",
            "Recording_partial.mp4",
            "preview.mp4",
            "混音_1.mp4",
            "Recording.gif",
            "Recording.mp4:stream",
        ] {
            let target = directory.join(name);
            for action in [OpenAction::Open, OpenAction::Locate] {
                assert!(
                    validate_open_scope(directory, &target, action, true, false).is_err(),
                    "unexpected target: {name}"
                );
            }
        }
        let external = Path::new("other/Recording.mp4");
        assert!(validate_open_scope(directory, external, OpenAction::Open, true, false).is_err());
        assert!(validate_open_scope(directory, external, OpenAction::Locate, true, false).is_err());
        assert!(validate_open_scope(
            directory,
            Path::new("other"),
            OpenAction::Folder,
            true,
            false
        )
        .is_err());
        assert!(validate_open_scope(directory, directory, OpenAction::Open, true, false).is_err());
    }

    #[test]
    fn recording_library_open_scope_accepts_single_level_mp4_and_only_exact_folder() {
        let directory = Path::new("library");
        for name in ["录屏_20261010.mp4", "Recording.MP4", "attempt.Mp4"] {
            let target = directory.join(name);
            for action in [OpenAction::Open, OpenAction::Locate] {
                assert!(validate_open_scope(directory, &target, action, true, false).is_ok());
            }
            assert!(
                validate_open_scope(directory, &target, OpenAction::Folder, true, false).is_err()
            );
        }
        assert!(validate_open_scope(directory, directory, OpenAction::Folder, true, false).is_ok());
    }
}
