//! 保存规则独立于 settings.json；成功输出才写最近目录，取消不产生磁盘变化。
use crate::{
    config_json::JsonFile,
    save_options::{validate_directory, PngCompression, SaveConfig, SaveFormat, DATE_FORMATS},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::HashSet,
    path::{Path, PathBuf},
};

pub(crate) const MAX_RECENT_DIRECTORIES: usize = 12;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone)]
pub(crate) struct SaveStore {
    pub config: SaveConfig,
    pub recent_directories: Vec<PathBuf>,
    // 仅保存本窗口实际改动的字段；旧窗口设置保存不能回写旧 MRU 或未修改规则。
    baseline: RefCell<SaveConfig>,
    config_read_failed: bool,
}
impl Default for SaveStore {
    fn default() -> Self {
        Self::from_snapshot(Snapshot::default())
    }
}
impl SaveStore {
    pub(crate) fn load() -> Self {
        Self::load_at(store_path())
    }
    pub(crate) fn load_at(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let (snapshot, config_read_failed) = match json_file(path).load() {
            Ok(file) => (
                file.map_or_else(Snapshot::default, SaveFile::snapshot),
                false,
            ),
            Err(error) => {
                crate::config_recovery::report_read_failure(path, &error);
                (Snapshot::default(), true)
            }
        };
        let mut store = Self::from_snapshot(snapshot);
        store.config_read_failed = config_read_failed;
        store
    }
    fn from_snapshot(snapshot: Snapshot) -> Self {
        Self {
            baseline: RefCell::new(snapshot.config.clone()),
            config: snapshot.config,
            recent_directories: snapshot.recent_directories,
            config_read_failed: false,
        }
    }
    pub(crate) fn save(&self) -> Result<()> {
        self.save_at(store_path())
    }
    /// 设置页修改规则；公开的 recent_directories 是读取快照，不由设置页回写。
    pub(crate) fn save_at(&self, path: impl AsRef<Path>) -> Result<()> {
        if self.config_read_failed {
            return Err(tr!(
                "上次读取保存规则失败，为保护原配置，请重新打开设置后再保存",
                "Save rules could not be read; reopen settings before saving to protect the original"
            )
            .into());
        }
        self.config.validate()?;
        let baseline = self.baseline.borrow().clone();
        if self.config == baseline {
            return Ok(());
        }
        let path = path.as_ref();
        let _lock = crate::config_recovery::lock(path)?;
        let mut latest = read_snapshot_locked(path)?;
        let previous = latest.clone();
        macro_rules! changed {
            ($($field:ident),+ $(,)?) => {
                $(if self.config.$field != baseline.$field {
                    latest.config.$field = self.config.$field.clone();
                })+
            };
        }
        changed!(directory, filename_template, date_format);
        macro_rules! encoding_changed {
            ($($field:ident),+ $(,)?) => {
                $(if self.config.encoding.$field != baseline.encoding.$field {
                    latest.config.encoding.$field = self.config.encoding.$field;
                })+
            };
        }
        encoding_changed!(format, jpeg_quality, png_compression);
        if latest != previous {
            write_snapshot(path, &latest)?;
        }
        self.baseline.replace(self.config.clone());
        Ok(())
    }

    /// 成功保存后调用。None 只更新 MRU；Some 是用户明确记住面板显示的整套保存规则。
    /// 取消、关闭或编码失败不得调用此入口。
    pub(crate) fn record_success(directory: &Path, remember: Option<SaveConfig>) -> Result<()> {
        Self::record_success_at(store_path(), directory, remember)
    }
    pub(crate) fn record_success_at(
        path: impl AsRef<Path>,
        directory: &Path,
        remember: Option<SaveConfig>,
    ) -> Result<()> {
        validate_directory(directory)?;
        if let Some(config) = &remember {
            config.validate()?;
        }
        let path = path.as_ref();
        let _lock = crate::config_recovery::lock(path)?;
        let mut latest = read_snapshot_locked(path)?;
        let previous = latest.clone();
        if let Some(config) = remember {
            latest.config = config;
        }
        latest
            .recent_directories
            .retain(|value| !same_directory(value, directory));
        latest.recent_directories.insert(0, directory.to_path_buf());
        latest.recent_directories.truncate(MAX_RECENT_DIRECTORIES);
        if latest != previous {
            write_snapshot(path, &latest)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    config: SaveConfig,
    recent_directories: Vec<PathBuf>,
}
fn store_path() -> PathBuf {
    crate::preferences::data_dir().join("save.json")
}
fn json_file(path: &Path) -> JsonFile<'_, SaveFile> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: |text| SaveFile::from(&parse_legacy_ini(text)),
    }
}

/// save.json 的磁盘结构；枚举按固定 ID 存字符串，读入后逐项校验。
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct SaveFile {
    save: SaveRules,
    recent_directories: Vec<PathBuf>,
}
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct SaveRules {
    directory: PathBuf,
    format: String,
    jpeg_quality: u8,
    png_compression: String,
    filename_template: String,
    date_format: String,
}
impl Default for SaveFile {
    fn default() -> Self {
        Self::from(&Snapshot::default())
    }
}
impl Default for SaveRules {
    fn default() -> Self {
        Self::from(&SaveConfig::default())
    }
}
impl From<&SaveConfig> for SaveRules {
    fn from(config: &SaveConfig) -> Self {
        Self {
            directory: config.directory.clone(),
            format: config.encoding.format.id().into(),
            jpeg_quality: config.encoding.jpeg_quality,
            png_compression: config.encoding.png_compression.id().into(),
            filename_template: config.filename_template.clone(),
            date_format: config.date_format.clone(),
        }
    }
}
impl From<&Snapshot> for SaveFile {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            save: SaveRules::from(&snapshot.config),
            recent_directories: snapshot
                .recent_directories
                .iter()
                .take(MAX_RECENT_DIRECTORIES)
                .cloned()
                .collect(),
        }
    }
}
impl SaveFile {
    /// 与旧 INI 相同的逐项校验：无效值回到默认，相对目录不进入规则或最近列表。
    fn snapshot(self) -> Snapshot {
        let mut snapshot = Snapshot::default();
        let (config, rules) = (&mut snapshot.config, self.save);
        if validate_directory(&rules.directory).is_ok() {
            config.directory = rules.directory;
        }
        if let Some(format) = SaveFormat::from_id(&rules.format) {
            config.encoding.format = format;
        }
        if (1..=100).contains(&rules.jpeg_quality) {
            config.encoding.jpeg_quality = rules.jpeg_quality;
        }
        if let Some(compression) = PngCompression::from_id(&rules.png_compression) {
            config.encoding.png_compression = compression;
        }
        config.filename_template = rules.filename_template;
        if DATE_FORMATS.contains(&rules.date_format.as_str()) {
            config.date_format = rules.date_format;
        }
        finish_snapshot(&mut snapshot, self.recent_directories);
        snapshot
    }
}
fn finish_snapshot(snapshot: &mut Snapshot, recent: impl IntoIterator<Item = PathBuf>) {
    if snapshot
        .config
        .suggested_stem_at(100, 100, crate::save_options::SaveMoment::EXAMPLE)
        .is_err()
    {
        snapshot.config.filename_template = crate::save_options::default_template().into();
    }
    for directory in recent {
        if snapshot.recent_directories.len() < MAX_RECENT_DIRECTORIES
            && validate_directory(&directory).is_ok()
            && !snapshot
                .recent_directories
                .iter()
                .any(|value| same_directory(value, &directory))
        {
            snapshot.recent_directories.push(directory);
        }
    }
}
fn read_snapshot_locked(path: &Path) -> Result<Snapshot> {
    Ok(json_file(path)
        .read_locked()?
        .map_or_else(Snapshot::default, SaveFile::snapshot))
}
/// 旧 save.ini 解析器，只用于迁移；下个版本删除。
fn parse_legacy_ini(text: &str) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let mut section = "";
    let mut version = None;
    let mut seen = HashSet::new();
    let mut recent: [Option<PathBuf>; MAX_RECENT_DIRECTORIES] = Default::default();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            section = if line == "[save]" {
                "save"
            } else if line == "[recent]" {
                "recent"
            } else {
                "unknown"
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if !seen.insert((section, key)) {
            continue;
        }
        if section.is_empty() && key == "version" {
            version = Some(value.parse::<u32>().ok());
        } else if section == "save" {
            match key {
                "directory" => {
                    let directory = PathBuf::from(value);
                    if validate_directory(&directory).is_ok() {
                        snapshot.config.directory = directory;
                    }
                }
                "format" => {
                    if let Some(format) = SaveFormat::from_id(value) {
                        snapshot.config.encoding.format = format;
                    }
                }
                "jpegQuality" => {
                    if let Ok(quality @ 1..=100) = value.parse::<u8>() {
                        snapshot.config.encoding.jpeg_quality = quality;
                    }
                }
                "pngCompression" => {
                    if let Some(compression) = PngCompression::from_id(value) {
                        snapshot.config.encoding.png_compression = compression;
                    }
                }
                "filenameTemplate" => snapshot.config.filename_template = value.to_string(),
                "dateFormat" if DATE_FORMATS.contains(&value) => {
                    snapshot.config.date_format = value.to_string()
                }
                _ => {}
            }
        } else if section == "recent" {
            if let Some(index) = key
                .strip_prefix("directory")
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|index| *index < MAX_RECENT_DIRECTORIES)
            {
                if key != format!("directory{index}") {
                    continue;
                }
                let directory = PathBuf::from(value);
                if validate_directory(&directory).is_ok() {
                    recent[index] = Some(directory);
                }
            }
        }
    }
    if version.is_some_and(|version| version != Some(1)) {
        return Snapshot::default();
    }
    finish_snapshot(&mut snapshot, recent.into_iter().flatten());
    snapshot
}
fn same_directory(left: &Path, right: &Path) -> bool {
    // Windows 路径通常不区分大小写；尾分隔符不应重复占用 MRU 位置。
    left.to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .replace('/', "\\")
        .to_lowercase()
        == right
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .replace('/', "\\")
            .to_lowercase()
}
#[cfg(test)]
fn encode_legacy_ini(snapshot: &Snapshot) -> Result<String> {
    snapshot.config.validate()?;
    let config = &snapshot.config;
    let mut text = format!(
        "version=1\n[save]\ndirectory={}\nformat={}\njpegQuality={}\npngCompression={}\nfilenameTemplate={}\ndateFormat={}\n[recent]\n",
        config.directory.to_str().ok_or(tr!("保存目录必须是有效的 Unicode 路径", "The save folder must be a valid Unicode path"))?,
        config.encoding.format.id(), config.encoding.jpeg_quality, config.encoding.png_compression.id(),
        config.filename_template, config.date_format,
    );
    for (index, directory) in snapshot
        .recent_directories
        .iter()
        .take(MAX_RECENT_DIRECTORIES)
        .enumerate()
    {
        validate_directory(directory)?;
        text.push_str(&format!(
            "directory{index}={}\n",
            directory.to_str().ok_or(tr!(
                "保存目录必须是有效的 Unicode 路径",
                "The save folder must be a valid Unicode path"
            ))?
        ));
    }
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(tr!(
            "保存配置过长，请缩短目录或模板",
            "The save settings are too long; shorten the folder or template"
        )
        .into());
    }
    Ok(text)
}
/// 调用方持有锁；写入前与旧格式相同地校验规则和每个最近目录。
fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    snapshot.config.validate()?;
    for directory in &snapshot.recent_directories {
        validate_directory(directory)?;
    }
    Ok(json_file(path).write_locked(&SaveFile::from(snapshot))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        sync::atomic::{AtomicU64, Ordering},
    };
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/qa")
                .join(format!(
                    "save-store-{}-{}",
                    std::process::id(),
                    ID.fetch_add(1, Ordering::Relaxed)
                ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> PathBuf {
            self.0.join("save.json")
        }
        fn backups(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(".corrupt-"))
                        && path.extension().is_some_and(|extension| extension == "bak")
                })
                .collect()
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn save_store_config_recovery_preserves_bad_bytes_before_settings_write() {
        for bytes in [vec![0xff, 0xfe], vec![b'x'; MAX_FILE_BYTES as usize + 1]] {
            let dir = TestDirectory::new();
            let mut store = SaveStore::load_at(dir.config());
            std::fs::write(dir.config(), &bytes).unwrap();
            store.config.encoding.jpeg_quality = 71;
            store.save_at(dir.config()).unwrap();
            assert_eq!(
                SaveStore::load_at(dir.config())
                    .config
                    .encoding
                    .jpeg_quality,
                71
            );
            let backups = dir.backups();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read(&backups[0]).unwrap(), bytes);
        }
    }
    #[test]
    fn save_store_config_recovery_preserves_bad_bytes_before_recent_directory_write() {
        let dir = TestDirectory::new();
        let invalid = b"version=1\n[save]\nfilenameTemplate=\xff\n";
        std::fs::write(dir.config(), invalid).unwrap();
        SaveStore::record_success_at(dir.config(), &dir.0, None).unwrap();
        assert_eq!(
            SaveStore::load_at(dir.config()).recent_directories,
            vec![dir.0.clone()]
        );
        assert_eq!(std::fs::read(&dir.backups()[0]).unwrap(), invalid);
    }
    #[cfg(windows)]
    #[test]
    fn save_store_config_recovery_read_failure_blocks_settings_and_recent_writes() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = TestDirectory::new();
        let original = br#"{"schema_version":1,"save":{"jpeg_quality":73}}"#;
        std::fs::write(dir.config(), original).unwrap();
        let mut store = SaveStore::load_at(dir.config());
        let denied = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.config())
            .unwrap();
        let mut defaults = SaveStore::load_at(dir.config());
        assert_eq!(defaults.config, SaveConfig::default());
        store.config.encoding.jpeg_quality = 71;
        assert!(store.save_at(dir.config()).is_err());
        assert!(SaveStore::record_success_at(dir.config(), &dir.0, None).is_err());
        drop(denied);
        assert_eq!(std::fs::read(dir.config()).unwrap(), original);
        assert!(dir.backups().is_empty());
        defaults.config.encoding.jpeg_quality = 71;
        assert!(defaults.save_at(dir.config()).is_err());
        let mut reread = SaveStore::load_at(dir.config());
        reread.config.encoding.jpeg_quality = 71;
        reread.save_at(dir.config()).unwrap();
        assert_eq!(
            SaveStore::load_at(dir.config())
                .config
                .encoding
                .jpeg_quality,
            71
        );
    }
    #[test]
    fn ini_roundtrip_keeps_stable_fields_and_rejects_invalid_values() {
        let dir = TestDirectory::new();
        let mut store = SaveStore::load_at(dir.config());
        store.config.directory = dir.0.clone();
        store.config.encoding.format = SaveFormat::Jpeg;
        store.config.encoding.jpeg_quality = 73;
        store.config.encoding.png_compression = PngCompression::Small;
        store.config.filename_template = "截图_{date:yyyy_MM_dd}_{width}x{height}".into();
        store.config.date_format = "yyyy年M月d日".into();
        store.save_at(dir.config()).unwrap();
        assert_eq!(SaveStore::load_at(dir.config()).config, store.config);
        let defaults = SaveConfig::default();
        let parsed = parse_legacy_ini("version=1\n[future]\nformat=jpeg\n[save]\nformat=unknown\nformat=jpeg\njpegQuality=NaN\npngCompression=unknown\nfilenameTemplate={invalid}\ndateFormat=invalid\ndirectory=relative\n");
        assert_eq!(parsed.config, defaults);
        assert_eq!(
            parse_legacy_ini("version=99\n[save]\nformat=jpeg\n").config,
            defaults
        );
        assert_eq!(
            parse_legacy_ini("version=bad\n[save]\nformat=jpeg\n").config,
            defaults
        );
        std::fs::write(dir.config(), vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(SaveStore::load_at(dir.config()).config, defaults);
    }
    #[test]
    fn save_store_json_round_trip_keeps_mru_and_validates_fields() {
        let dir = TestDirectory::new();
        let path = dir.config();
        let mut store = SaveStore::load_at(&path);
        store.config.directory = dir.0.join("规则");
        store.config.encoding.format = SaveFormat::Jpeg;
        store.config.encoding.jpeg_quality = 66;
        store.config.date_format = "yyyy年M月d日".into();
        store.save_at(&path).unwrap();
        SaveStore::record_success_at(&path, &dir.0.join("a"), None).unwrap();
        SaveStore::record_success_at(&path, &dir.0.join("b"), None).unwrap();
        let loaded = SaveStore::load_at(&path);
        assert_eq!(loaded.config, store.config);
        assert_eq!(
            loaded.recent_directories,
            vec![dir.0.join("b"), dir.0.join("a")]
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schema_version\": 1") && text.contains("\"format\": \"jpeg\""));
        // 手改 JSON：相对目录、越界质量、未知格式逐项回到默认，其余字段保留。
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["save"]["directory"] = "relative".into();
        value["save"]["jpeg_quality"] = 0.into();
        value["save"]["format"] = "future".into();
        value["save"]["filename_template"] = "{invalid}".into();
        value["recent_directories"] =
            serde_json::json!(["relative", dir.0.join("c"), dir.0.join("C")]);
        std::fs::write(&path, value.to_string()).unwrap();
        let loaded = SaveStore::load_at(&path);
        let defaults = SaveConfig::default();
        assert_eq!(loaded.config.directory, defaults.directory);
        assert_eq!(
            loaded.config.encoding.jpeg_quality,
            defaults.encoding.jpeg_quality
        );
        assert_eq!(loaded.config.encoding.format, defaults.encoding.format);
        assert_eq!(loaded.config.filename_template, defaults.filename_template);
        assert_eq!(loaded.config.date_format, "yyyy年M月d日");
        assert_eq!(loaded.recent_directories, vec![dir.0.join("c")]);
    }
    #[test]
    fn save_store_migrates_legacy_ini_to_json() {
        let dir = TestDirectory::new();
        let path = dir.config();
        let mut snapshot = Snapshot::default();
        snapshot.config.directory = dir.0.join("旧目录");
        snapshot.config.encoding.format = SaveFormat::Bmp;
        snapshot.config.encoding.png_compression = PngCompression::Small;
        snapshot.recent_directories = vec![dir.0.join("x"), dir.0.join("y")];
        let legacy = encode_legacy_ini(&snapshot).unwrap();
        std::fs::write(dir.0.join("save.ini"), &legacy).unwrap();
        let loaded = SaveStore::load_at(&path);
        assert_eq!(loaded.config, snapshot.config);
        assert_eq!(loaded.recent_directories, snapshot.recent_directories);
        assert!(path.exists() && !dir.0.join("save.ini").exists());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("save.ini.bak")).unwrap(),
            legacy
        );
        // 写入路径同样先迁移：旧 INI 不会被新 JSON 无声遮住。
        std::fs::remove_file(&path).unwrap();
        std::fs::write(dir.0.join("save.ini"), &legacy).unwrap();
        SaveStore::record_success_at(&path, &dir.0.join("z"), None).unwrap();
        let loaded = SaveStore::load_at(&path);
        assert_eq!(loaded.config, snapshot.config);
        assert_eq!(
            loaded.recent_directories,
            vec![dir.0.join("z"), dir.0.join("x"), dir.0.join("y")]
        );
    }
    #[test]
    fn stale_settings_merge_only_changes_and_success_records_latest_mru() {
        let dir = TestDirectory::new();
        let path = dir.config();
        let mut old = SaveStore::load_at(&path);
        let mut new = SaveStore::load_at(&path);
        new.config.encoding.jpeg_quality = 61;
        new.config.filename_template = "新_{date}".into();
        new.save_at(&path).unwrap();
        SaveStore::record_success_at(&path, &dir.0.join("first"), None).unwrap();
        old.config.encoding.png_compression = PngCompression::Balanced;
        old.save_at(&path).unwrap();
        let saved = SaveStore::load_at(&path);
        assert_eq!(saved.config.encoding.jpeg_quality, 61);
        assert_eq!(saved.config.filename_template, "新_{date}");
        assert_eq!(
            saved.config.encoding.png_compression,
            PngCompression::Balanced
        );
        assert_eq!(saved.recent_directories, vec![dir.0.join("first")]);
        // 同一设置窗口再次保存未变化草稿也不能覆盖其它窗口的后续质量变化。
        new.config.encoding.jpeg_quality = 88;
        new.save_at(&path).unwrap();
        old.save_at(&path).unwrap();
        assert_eq!(SaveStore::load_at(&path).config.encoding.jpeg_quality, 88);
        let mut remember = saved.config;
        remember.directory = dir.0.join("remembered");
        remember.encoding.format = SaveFormat::Bmp;
        SaveStore::record_success_at(&path, &remember.directory.clone(), Some(remember.clone()))
            .unwrap();
        assert_eq!(SaveStore::load_at(&path).config, remember);
        let bytes = std::fs::read(&path).unwrap();
        assert!(SaveStore::record_success_at(&path, Path::new("relative"), None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    #[test]
    fn recent_directories_are_bounded_deduplicated_and_busy_write_preserves_file() {
        let dir = TestDirectory::new();
        let path = dir.config();
        for index in 0..15 {
            SaveStore::record_success_at(&path, &dir.0.join(index.to_string()), None).unwrap();
        }
        SaveStore::record_success_at(&path, &dir.0.join("8"), None).unwrap();
        let saved = SaveStore::load_at(&path);
        assert_eq!(saved.recent_directories.len(), MAX_RECENT_DIRECTORIES);
        assert_eq!(saved.recent_directories[0], dir.0.join("8"));
        assert_eq!(
            saved
                .recent_directories
                .iter()
                .filter(|path| *path == &dir.0.join("8"))
                .count(),
            1
        );
        let lock = File::open(path.with_extension("ini.lock")).unwrap();
        lock.try_lock().unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(SaveStore::record_success_at(&path, &dir.0.join("blocked"), None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(std::fs::read_dir(&dir.0).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .is_none_or(|ext| ext != "tmp")));
    }
}
