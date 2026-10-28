//! 只读的同目录图片列表；枚举前拒绝云目录、链接与可能触发下载的项目。
use std::{
    cmp::Ordering,
    fs::{self, Metadata},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
};

/// 大目录不取任意前缀，超过预算后退回显式打开的单图。
const MAX_DIRECTORY_ENTRIES: usize = 20_000;

#[derive(Clone, Debug)]
pub(crate) struct ImageCollection {
    paths: Vec<PathBuf>,
    index: usize,
    scan_warning: Option<String>,
}

impl ImageCollection {
    /// 显式源只读打开。无法安全枚举目录时仍允许查看已落地的显式源。
    pub(crate) fn scan(path: &Path) -> Result<Self, String> {
        static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);
        Self::scan_cancellable(path, &NEVER_CANCELLED)
    }

    /// 枚举与属性读取之间可取消；标准排序的单次调用不可抢占，前后分别复核。
    /// 取消始终返回错误，不能被目录不可枚举时的单图降级吞掉。
    pub(crate) fn scan_cancellable(path: &Path, cancel: &AtomicBool) -> Result<Self, String> {
        check_scan_cancel(cancel)?;
        let source = std::path::absolute(path);
        check_scan_cancel(cancel)?;
        let source = source.map_err(|error| error.to_string())?;
        let local = ensure_local_file(&source);
        check_scan_cancel(cancel)?;
        local?;
        let mut result = Self {
            paths: vec![source.clone()],
            index: 0,
            scan_warning: None,
        };
        let parent_result = result.scan_parent(&source, cancel);
        result.finish_scan(source, parent_result, cancel)
    }

    fn finish_scan(
        mut self,
        source: PathBuf,
        parent_result: Result<(), String>,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        check_scan_cancel(cancel)?;
        if let Err(error) = parent_result {
            self.paths = vec![source];
            self.index = 0;
            self.scan_warning = Some(error);
        }
        check_scan_cancel(cancel)?;
        Ok(self)
    }

    fn scan_parent(&mut self, source: &Path, cancel: &AtomicBool) -> Result<(), String> {
        check_scan_cancel(cancel)?;
        let Some(parent) = source.parent() else {
            return Ok(());
        };
        if parent
            .components()
            .any(|part| is_cloud_name(part.as_os_str()))
        {
            return Err(tr!(
                "云同步目录仅查看当前文件",
                "Only this file in a cloud folder"
            )
            .into());
        }
        check_scan_cancel(cancel)?;
        let local = ensure_available_path(parent, false);
        check_scan_cancel(cancel)?;
        local?;
        let entries = fs::read_dir(parent);
        check_scan_cancel(cancel)?;
        let mut entries = entries.map_err(|error| error.to_string())?;
        let mut count = 0usize;
        loop {
            check_scan_cancel(cancel)?;
            let entry = entries.next();
            check_scan_cancel(cancel)?;
            let Some(entry) = entry else { break };
            if count >= MAX_DIRECTORY_ENTRIES {
                return Err(tr!(
                    "目录过大，仅查看当前文件",
                    "Folder too large; showing this file"
                )
                .into());
            }
            count += 1;
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !is_candidate(&path) || same_path(&path, source) {
                continue;
            }
            // 父目录已经逐级检查；对子项使用不跟随链接的元数据。
            check_scan_cancel(cancel)?;
            let metadata = fs::symlink_metadata(&path);
            check_scan_cancel(cancel)?;
            if metadata.is_ok_and(|metadata| metadata.is_file() && metadata_is_available(&metadata))
            {
                self.paths.push(path);
            }
        }
        check_scan_cancel(cancel)?;
        self.paths.sort_by(|a, b| {
            natural_cmp(
                &a.file_name().unwrap_or_default().to_string_lossy(),
                &b.file_name().unwrap_or_default().to_string_lossy(),
            )
            .then_with(|| a.as_os_str().cmp(b.as_os_str()))
        });
        check_scan_cancel(cancel)?;
        self.index = self
            .paths
            .iter()
            .position(|path| same_path(path, source))
            .unwrap_or(0);
        check_scan_cancel(cancel)?;
        Ok(())
    }

    pub(crate) fn current(&self) -> &Path {
        &self.paths[self.index]
    }

    /// 从 0 开始的只读索引；查询不改变当前项，越界返回空。
    pub(crate) fn path_at(&self, index: usize) -> Option<&Path> {
        self.paths.get(index).map(PathBuf::as_path)
    }

    /// 一起返回从 1 开始的位置与总数，供窗口显示。
    pub(crate) fn position(&self) -> (usize, usize) {
        (self.index + 1, self.paths.len())
    }

    /// 只查询，不移动当前位置。越过首尾时不回绕，也不重复当前文件。
    pub(crate) fn neighbor(&self, delta: isize) -> Option<&Path> {
        let index = self.index.checked_add_signed(delta)?;
        (index != self.index)
            .then(|| self.paths.get(index).map(PathBuf::as_path))
            .flatten()
    }

    /// 首尾停止，不循环。失效文件由读取端显示错误，仍可继续翻图。
    pub(crate) fn navigate(&mut self, delta: isize) -> bool {
        let next = self
            .index
            .saturating_add_signed(delta)
            .min(self.paths.len() - 1);
        self.select(next)
    }

    pub(crate) fn first(&mut self) -> bool {
        self.select(0)
    }

    pub(crate) fn last(&mut self) -> bool {
        self.select(self.paths.len() - 1)
    }

    /// 从 0 开始定位；重复选择或越界都不改变当前项。
    pub(crate) fn select(&mut self, index: usize) -> bool {
        if index >= self.paths.len() || self.index == index {
            return false;
        }
        self.index = index;
        true
    }

    pub(crate) fn scan_warning(&self) -> Option<&str> {
        self.scan_warning.as_deref()
    }
}

fn check_scan_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(AtomicOrdering::Acquire) {
        Err(tr!("图片目录扫描已取消", "Image folder scan canceled").into())
    } else {
        Ok(())
    }
}

/// 候选只用于目录列表；实际格式仍由文件头与当前构建的解码器确定。
pub(crate) fn is_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "bmp"
                    | "gif"
                    | "webp"
                    | "svg"
                    | "psd"
                    | "tif"
                    | "tiff"
                    | "ico"
                    | "cur"
                    | "avif"
                    | "avifs"
                    | "heic"
                    | "heics"
                    | "heif"
                    | "heifs"
                    | "hif"
            )
        })
}

/// 显式用户源可在云同步目录，但每个祖先与文件必须已落地且不是链接。
/// 此检查不读文件内容、不规范化链接目标；读取端须在每次重新打开前调用。
pub(crate) fn ensure_local_file(path: &Path) -> Result<(), String> {
    ensure_available_path(path, true)
}

/// 目录及其祖先都必须现存、已落地且不是链接；不允许缺失路径。
pub(crate) fn ensure_local_directory(path: &Path) -> Result<(), String> {
    ensure_available_path(path, false)
}

/// 内存快照的旧源路径资格：缺失即可结束，现存部分仍须逐级安全。
/// 仅供不再读取源文件的操作使用；重新打开文件必须走ensure_local_file。
pub(crate) fn ensure_local_file_or_missing(path: &Path) -> Result<(), String> {
    ensure_available_path_with(path, true, true, |candidate| {
        fs::symlink_metadata(candidate)
    })
}

fn ensure_available_path(path: &Path, require_file: bool) -> Result<(), String> {
    ensure_available_path_with(path, require_file, false, |candidate| {
        fs::symlink_metadata(candidate)
    })
}

fn ensure_available_path_with(
    path: &Path,
    require_file: bool,
    allow_missing: bool,
    mut metadata: impl FnMut(&Path) -> std::io::Result<Metadata>,
) -> Result<(), String> {
    let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
    let ancestors: Vec<_> = absolute.ancestors().collect();
    for candidate in ancestors.into_iter().rev() {
        let metadata = match metadata(candidate) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(())
            }
            Err(error) => return Err(error.to_string()),
        };
        if !metadata_is_available(&metadata) {
            return Err(tr!(
                "跳过链接或尚未下载的文件/目录",
                "Skipped a link or a file/folder not downloaded"
            )
            .into());
        }
        let leaf = candidate == absolute;
        if (leaf && require_file && !metadata.is_file())
            || ((!leaf || !require_file) && !metadata.is_dir())
        {
            return Err(tr!("路径类型不正确", "Unexpected path type").into());
        }
    }
    Ok(())
}

fn metadata_is_available(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // REPARSE_POINT 同时覆盖目录联接；另外拒绝所有可能触发云端回读的属性。
        const UNAVAILABLE: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;
        metadata.file_attributes() & UNAVAILABLE == 0
    }
    #[cfg(not(windows))]
    {
        true
    }
}

fn is_cloud_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.starts_with("onedrive") || name == "dropbox" || name.starts_with("icloud")
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let left: Vec<_> = left.as_os_str().encode_wide().collect();
        let right: Vec<_> = right.as_os_str().encode_wide().collect();
        unsafe {
            windows::Win32::Globalization::CompareStringOrdinal(&left, &right, true)
                == windows::Win32::Globalization::CSTR_EQUAL
        }
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// 数字段按有效位数比较，避免超长编号溢出；相同数值时短字段在前。
fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        if left[a].is_ascii_digit() && right[b].is_ascii_digit() {
            let (start_a, start_b) = (a, b);
            while a < left.len() && left[a].is_ascii_digit() {
                a += 1;
            }
            while b < right.len() && right[b].is_ascii_digit() {
                b += 1;
            }
            let digits_a = &left[start_a..a];
            let digits_b = &right[start_b..b];
            let value_a = digits_a
                .iter()
                .position(|byte| *byte != b'0')
                .unwrap_or(digits_a.len());
            let value_b = digits_b
                .iter()
                .position(|byte| *byte != b'0')
                .unwrap_or(digits_b.len());
            let ordering = (digits_a.len() - value_a)
                .cmp(&(digits_b.len() - value_b))
                .then_with(|| digits_a[value_a..].cmp(&digits_b[value_b..]))
                .then_with(|| digits_a.len().cmp(&digits_b.len()));
            if ordering != Ordering::Equal {
                return ordering;
            }
        } else {
            let ordering = left[a].cmp(&right[b]);
            if ordering != Ordering::Equal {
                return ordering;
            }
            a += 1;
            b += 1;
        }
    }
    (left.len() - a).cmp(&(right.len() - b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_collection_scan_cancel_precedes_source_checks_and_single_file_fallback() {
        let cancel = AtomicBool::new(true);
        let expected = check_scan_cancel(&cancel).unwrap_err();
        assert_eq!(
            ImageCollection::scan_cancellable(Path::new("\0"), &cancel).unwrap_err(),
            expected
        );
        let source = PathBuf::from("source.png");
        let pending = || ImageCollection {
            paths: vec![source.clone(), "neighbor.png".into()],
            index: 1,
            scan_warning: None,
        };
        assert_eq!(
            pending()
                .finish_scan(source.clone(), Err("parent unavailable".into()), &cancel)
                .unwrap_err(),
            expected
        );
        cancel.store(false, AtomicOrdering::Release);
        let single = pending()
            .finish_scan(source.clone(), Err("parent unavailable".into()), &cancel)
            .unwrap();
        assert_eq!(single.current(), source.as_path());
        assert_eq!(single.position(), (1, 1));
        assert_eq!(single.scan_warning(), Some("parent unavailable"));
    }

    #[cfg(windows)]
    #[test]
    fn image_collection_path_identity_preserves_unicode_and_unpaired_utf16() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        assert!(same_path(
            Path::new(r"C:\photos\Ä2.PNG"),
            Path::new(r"c:\PHOTOS\ä2.png")
        ));
        assert!(!same_path(
            Path::new(r"C:\photos\Ä2.png"),
            Path::new(r"C:\photos\A2.png")
        ));
        let mut left: Vec<u16> = r"C:\photos\".encode_utf16().collect();
        left.extend_from_slice(&[0xd800, b'.' as u16, b'p' as u16, b'n' as u16, b'g' as u16]);
        let mut right = left.clone();
        right[r"C:\photos\".encode_utf16().count()] = 0xd801;
        let left = PathBuf::from(OsString::from_wide(&left));
        let right = PathBuf::from(OsString::from_wide(&right));
        assert_eq!(left.to_string_lossy(), right.to_string_lossy());
        assert!(same_path(&left, &left));
        assert!(!same_path(&left, &right));
    }

    #[test]
    fn natural_order_handles_zeroes_unicode_and_unbounded_numbers() {
        let mut names = [
            "图10.png",
            "图02.png",
            "图2.png",
            "图1.png",
            "图000.png",
            "图0.png",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            [
                "图0.png",
                "图000.png",
                "图1.png",
                "图2.png",
                "图02.png",
                "图10.png"
            ]
        );
        assert_eq!(natural_cmp("Image9.PNG", "image10.png"), Ordering::Less);
        assert_eq!(
            natural_cmp("9".repeat(100).as_str(), "1".repeat(101).as_str()),
            Ordering::Less
        );
    }

    #[test]
    fn navigation_stops_at_edges_and_reports_one_based_positions() {
        let mut collection = ImageCollection {
            paths: vec!["1.png".into(), "2.png".into(), "10.png".into()],
            index: 0,
            scan_warning: None,
        };
        assert!(!collection.navigate(-1));
        assert_eq!(collection.neighbor(1), Some(Path::new("2.png")));
        assert_eq!(collection.neighbor(-1), None);
        assert_eq!(collection.neighbor(0), None);
        assert!(collection.navigate(isize::MAX));
        assert_eq!(collection.position(), (3, 3));
        assert!(!collection.last());
        assert_eq!(collection.neighbor(1), None);
        assert_eq!(collection.neighbor(-1), Some(Path::new("2.png")));
        assert!(collection.navigate(isize::MIN));
        assert_eq!(collection.current(), Path::new("1.png"));
        assert!(!collection.first());
    }

    #[test]
    fn indexed_lookup_and_selection_preserve_position_on_invalid_indices() {
        let mut collection = ImageCollection {
            paths: vec!["1.png".into(), "2.png".into(), "10.png".into()],
            index: 1,
            scan_warning: None,
        };
        assert_eq!(collection.path_at(0), Some(Path::new("1.png")));
        assert_eq!(collection.path_at(2), Some(Path::new("10.png")));
        assert_eq!(collection.path_at(3), None);
        assert_eq!(collection.path_at(usize::MAX), None);
        assert_eq!(collection.position(), (2, 3));
        assert!(!collection.select(1));
        assert!(!collection.select(3));
        assert!(!collection.select(usize::MAX));
        assert_eq!(collection.current(), Path::new("2.png"));
        assert!(collection.select(2));
        assert_eq!(collection.position(), (3, 3));
        assert!(collection.select(0));
        assert_eq!(collection.current(), Path::new("1.png"));
    }

    #[test]
    fn candidates_and_cloud_names_are_explicit() {
        assert!(is_candidate(Path::new("a.JPEG")));
        assert!(is_candidate(Path::new("a.CUR")));
        assert!(is_candidate(Path::new("a.SVG")));
        assert!(!is_candidate(Path::new("a.svgz")));
        for extension in ["AVIF", "avifs", "heic", "heics", "heif", "heifs", "hif"] {
            assert!(is_candidate(Path::new(&format!("a.{extension}"))));
        }
        assert!(!is_candidate(Path::new("a.jpg.lnk")));
        assert!(!is_candidate(Path::new("folder")));
        for name in ["OneDrive", "OneDrive - team", "Dropbox", "iCloudDrive"] {
            assert!(is_cloud_name(name.as_ref()));
        }
        assert!(!is_cloud_name("pictures".as_ref()));
    }

    struct PathFixture(std::path::PathBuf);
    impl PathFixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "rustshot-image-path-{stamp}-{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("source.bin"), b"snapshot").unwrap();
            fs::create_dir(root.join("folder")).unwrap();
            Self(root)
        }
    }
    impl Drop for PathFixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0.join("source.bin"));
            let _ = fs::remove_dir(self.0.join("folder"));
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn image_collection_snapshot_path_allows_only_missing_parts() {
        let fixture = PathFixture::new();
        let file = fixture.0.join("source.bin");
        assert!(ensure_local_file(&file).is_ok());
        assert!(ensure_local_file_or_missing(&file).is_ok());
        assert!(ensure_local_directory(&fixture.0).is_ok());
        assert!(ensure_local_directory(&fixture.0.join("folder")).is_ok());
        assert!(ensure_local_directory(&file).is_err());
        for missing in [
            fixture.0.join("missing.bin"),
            fixture.0.join("missing").join("child").join("source.bin"),
        ] {
            assert!(ensure_local_file_or_missing(&missing).is_ok());
            assert!(ensure_local_file(&missing).is_err());
            assert!(ensure_local_directory(&missing).is_err());
        }
        for wrong_type in [
            fixture.0.join("folder"),
            file.join("missing").join("source.bin"),
        ] {
            assert!(ensure_local_file_or_missing(&wrong_type).is_err());
            assert!(ensure_local_file(&wrong_type).is_err());
        }
    }

    #[test]
    fn image_collection_snapshot_path_stops_at_first_missing_ancestor() {
        let fixture = PathFixture::new();
        let first_missing = fixture.0.join("missing");
        let leaf = first_missing.join("child").join("source.bin");
        let mut checked = Vec::new();
        assert!(ensure_available_path_with(&leaf, true, true, |candidate| {
            checked.push(candidate.to_path_buf());
            fs::symlink_metadata(candidate)
        })
        .is_ok());
        assert_eq!(checked.last(), Some(&first_missing));
        assert!(!checked.contains(&leaf));
    }

    #[test]
    fn image_collection_snapshot_path_rejects_other_metadata_errors() {
        let fixture = PathFixture::new();
        let leaf = fixture.0.join("source.bin");
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotADirectory,
            std::io::ErrorKind::InvalidInput,
        ] {
            assert!(ensure_available_path_with(&leaf, true, true, |candidate| {
                if candidate == leaf {
                    Err(std::io::Error::new(kind, "metadata rejected"))
                } else {
                    fs::symlink_metadata(candidate)
                }
            })
            .is_err());
        }
    }
}
