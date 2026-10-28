//! JSON 配置文件：serde 读写叠在 config_recovery 的锁、大小上限、损坏备份之上。
//! 每个领域一个文件、一把锁（多个进程会同时写设置）；顶层带 schema_version。
//! 没有 JSON 但有同名旧 INI 时，首次读取用旧解析器迁移并把 INI 改名为 .ini.bak。
use crate::config_recovery;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Map, Value};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) const SCHEMA_KEY: &str = "schema_version";

/// 一个 JSON 配置文件的读写契约；T 是该文件的完整内容，缺字段取 T::default()。
pub(crate) struct JsonFile<'a, T> {
    pub path: &'a Path,
    pub max_bytes: u64,
    pub schema_version: u32,
    /// 旧 INI 解析器，只用于迁移（下个版本删除）。
    pub legacy: fn(&str) -> T,
}

impl<T: Serialize + DeserializeOwned + Default> JsonFile<'_, T> {
    pub(crate) fn legacy_path(&self) -> PathBuf {
        self.path.with_extension("ini")
    }

    pub(crate) fn lock(&self) -> io::Result<File> {
        config_recovery::lock(self.path)
    }

    /// 正常读取不加锁、不建文件；JSON 无法解析或需要迁移旧 INI 时才加锁处理。
    pub(crate) fn load(&self) -> io::Result<Option<T>> {
        match config_recovery::load_text(self.path, self.max_bytes)? {
            Some(text) => {
                if let Ok(value) = self.parse(&text) {
                    return Ok(Some(value));
                }
            }
            None if !self.legacy_path().try_exists()? => return Ok(None),
            None => {}
        }
        let _lock = self.lock()?;
        self.read_locked()
    }

    /// 调用方持有 lock()，并保持到写入完成。读取、备份或迁移失败都返回错误，
    /// 调用方不得继续覆盖配置。
    pub(crate) fn read_locked(&self) -> io::Result<Option<T>> {
        if let Some(text) = config_recovery::read_text_locked(self.path, self.max_bytes)? {
            return match self.parse(&text) {
                Ok(value) => Ok(Some(value)),
                Err(reason) => {
                    config_recovery::preserve_unreadable_locked(self.path, &reason)?;
                    Ok(None)
                }
            };
        }
        self.migrate_locked()
    }

    /// 旧 INI → JSON；INI 无法读取时 config_recovery 已备份它，按缺失处理。
    fn migrate_locked(&self) -> io::Result<Option<T>> {
        let legacy = self.legacy_path();
        let Some(text) = config_recovery::read_text_locked(&legacy, self.max_bytes)? else {
            return Ok(None);
        };
        let value = (self.legacy)(&text);
        self.write_locked(&value)?;
        // JSON 已落盘并优先读取；改名失败只会留下一个不再读取的旧文件。
        if let Err(error) = std::fs::rename(&legacy, legacy.with_extension("ini.bak")) {
            eprintln!("Could not rename {}: {error}", legacy.display());
        }
        Ok(Some(value))
    }

    /// 整份替换：加锁，先让损坏文件或旧 INI 按规则备份／迁移，再写入。
    pub(crate) fn replace(&self, value: &T) -> io::Result<()> {
        let _lock = self.lock()?;
        self.read_locked()?;
        self.write_locked(value)
    }

    /// 调用方持有 lock()；同目录临时文件写完并落盘后原子替换，失败不改原文件。
    pub(crate) fn write_locked(&self, value: &T) -> io::Result<()> {
        write_atomic(self.path, self.encode(value)?.as_bytes())
    }

    pub(crate) fn encode(&self, value: &T) -> io::Result<String> {
        let Value::Object(body) = serde_json::to_value(value).map_err(io::Error::other)? else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Settings must serialize to a JSON object",
            ));
        };
        let mut root = Map::new();
        root.insert(SCHEMA_KEY.into(), self.schema_version.into());
        root.extend(body);
        let mut text =
            serde_json::to_string_pretty(&Value::Object(root)).map_err(io::Error::other)?;
        text.push('\n');
        if text.len() as u64 > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!("配置内容过长", "The settings are too long"),
            ));
        }
        Ok(text)
    }

    /// 语法错误或顶层不是对象才算损坏；单个字段类型不符或取值未知只让该字段取默认。
    /// 更高的 schema_version 也按已知字段读取，未知字段忽略。
    pub(crate) fn parse(&self, text: &str) -> Result<T, String> {
        parse_object(text).map(|file| overlay(T::default(), file))
    }
}

pub(crate) fn parse_object(text: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
        Ok(Value::Object(mut map)) => {
            map.remove(SCHEMA_KEY);
            Ok(map)
        }
        Ok(_) => Err(tr!("顶层不是对象", "the top level is not an object").into()),
        Err(error) => Err(error.to_string()),
    }
}

/// 把文件字段叠到 base 上；嵌套对象逐项合并。某字段无法反序列化时保留 base 的值，
/// 不连累同文件的其它字段。
pub(crate) fn overlay<T: Serialize + DeserializeOwned>(base: T, file: Map<String, Value>) -> T {
    let Ok(Value::Object(base_map)) = serde_json::to_value(&base) else {
        return base;
    };
    let mut merged = Value::Object(base_map.clone());
    deep_merge(&mut merged, Value::Object(file.clone()));
    if let Ok(value) = T::deserialize(&merged) {
        return value;
    }
    let mut root = Value::Object(base_map);
    merge_valid::<T>(&mut root, &mut Vec::new(), file);
    T::deserialize(&root).unwrap_or(base)
}

fn deep_merge(target: &mut Value, incoming: Value) {
    match (target, incoming) {
        (Value::Object(target), Value::Object(incoming)) => {
            for (key, value) in incoming {
                match target.get_mut(&key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        deep_merge(existing, value)
                    }
                    _ => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, incoming) => *target = incoming,
    }
}

fn merge_valid<T: DeserializeOwned>(
    root: &mut Value,
    path: &mut Vec<String>,
    incoming: Map<String, Value>,
) {
    for (key, value) in incoming {
        let pointer: String = path
            .iter()
            .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
            .collect();
        let Some(Value::Object(target)) = root.pointer_mut(&pointer) else {
            return;
        };
        if let Value::Object(child) = value {
            if target.get(&key).is_some_and(Value::is_object) {
                path.push(key);
                merge_valid::<T>(root, path, child);
                path.pop();
                continue;
            }
            let previous = target.insert(key.clone(), Value::Object(child));
            restore_if_invalid::<T>(root, &pointer, key, previous);
            continue;
        }
        let previous = target.insert(key.clone(), value);
        restore_if_invalid::<T>(root, &pointer, key, previous);
    }
}

fn restore_if_invalid<T: DeserializeOwned>(
    root: &mut Value,
    pointer: &str,
    key: String,
    previous: Option<Value>,
) {
    if T::deserialize(&*root).is_ok() {
        return;
    }
    if let Some(Value::Object(target)) = root.pointer_mut(pointer) {
        match previous {
            Some(previous) => target.insert(key, previous),
            None => target.remove(&key),
        };
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static TEMP_ID: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "json-{}-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed),
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    let write = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let write = write.and_then(|()| std::fs::rename(&temp, path));
    if write.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    write
}

/// 测试用临时目录：只放在系统临时目录下，析构时整体删除。
#[cfg(test)]
pub(crate) struct TestDir(pub PathBuf);
#[cfg(test)]
impl TestDir {
    pub(crate) fn new(tag: &str) -> Self {
        static ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rustshot-{tag}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Sample {
        count: u32,
        flag: bool,
        name: String,
        nested: Nested,
    }
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Nested {
        level: u8,
        mode: Mode,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum Mode {
        Fast,
        Slow,
    }
    impl Default for Sample {
        fn default() -> Self {
            Self {
                count: 3,
                flag: true,
                name: "默认".into(),
                nested: Nested::default(),
            }
        }
    }
    impl Default for Nested {
        fn default() -> Self {
            Self {
                level: 1,
                mode: Mode::Fast,
            }
        }
    }
    fn legacy(text: &str) -> Sample {
        Sample {
            count: text.trim().parse().unwrap_or(0),
            ..Sample::default()
        }
    }
    fn file(path: &Path) -> JsonFile<'_, Sample> {
        JsonFile {
            path,
            max_bytes: 4096,
            schema_version: 1,
            legacy,
        }
    }

    #[test]
    fn config_json_invalid_fields_fall_back_individually() {
        let json = file(Path::new("unused.json"));
        let parsed = json
            .parse(r#"{"schema_version":9,"count":"many","flag":false,"future":1,"nested":{"level":300,"mode":"slow"}}"#)
            .unwrap();
        assert_eq!(
            parsed,
            Sample {
                flag: false,
                nested: Nested {
                    level: 1,
                    mode: Mode::Slow
                },
                ..Sample::default()
            }
        );
        let parsed = json
            .parse(r#"{"nested":{"mode":"future"},"name":"新"}"#)
            .unwrap();
        assert_eq!(parsed.nested, Nested::default());
        assert_eq!(parsed.name, "新");
        assert!(json.parse("[1]").is_err());
        assert!(json.parse("{\"count\":").is_err());
    }

    #[test]
    fn config_json_round_trip_writes_schema_version_first() {
        let dir = TestDir::new("config-json");
        let path = dir.join("sample.json");
        let json = file(&path);
        let value = Sample {
            count: 7,
            flag: false,
            name: "路径\\\"引号\"\n换行".into(),
            nested: Nested {
                level: 9,
                mode: Mode::Slow,
            },
        };
        let _lock = json.lock().unwrap();
        json.write_locked(&value).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n  \"schema_version\": 1,\n  \"count\": 7"));
        assert_eq!(json.load().unwrap(), Some(value));
    }

    #[test]
    fn config_json_migrates_legacy_ini_once_and_keeps_backup() {
        let dir = TestDir::new("config-json");
        let path = dir.join("sample.json");
        let json = file(&path);
        assert_eq!(json.load().unwrap(), None);
        assert!(!path.exists());
        std::fs::write(dir.join("sample.ini"), "42").unwrap();
        assert_eq!(json.load().unwrap().unwrap().count, 42);
        assert!(path.exists() && !dir.join("sample.ini").exists());
        assert_eq!(std::fs::read(dir.join("sample.ini.bak")).unwrap(), b"42");
        // 再出现旧 INI 也不覆盖已有 JSON。
        std::fs::write(dir.join("sample.ini"), "5").unwrap();
        assert_eq!(json.load().unwrap().unwrap().count, 42);
    }

    #[test]
    fn config_json_syntax_error_preserves_bytes_then_defaults() {
        let dir = TestDir::new("config-json");
        let path = dir.join("sample.json");
        let json = file(&path);
        std::fs::write(&path, "{\"count\": 5,").unwrap();
        assert_eq!(json.load().unwrap(), None);
        assert!(!path.exists());
        let backups: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "bak"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{\"count\": 5,");
    }
}
