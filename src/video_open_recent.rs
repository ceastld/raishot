//! 仅在视频成功打开后更新的最近文件；不检查或读取所记录的视频路径。
//! 修改时在同一配置锁内重新读取并合并，窗口中的旧列表不能覆盖其它进程的新记录。
//! Windows按词法路径忽略大小写去重；统一普通分隔符和`.`，不合并`..`或verbatim别名。
use crate::config_json::JsonFile;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

pub(crate) const MAX_RECENT_VIDEOS: usize = 16;
const FILE_NAME: &str = "video-recent.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PATH_UNITS: usize = 32_767;

#[derive(Default, Deserialize, Serialize)]
struct FileBody {
    paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFile {
    schema_version: u32,
    paths: Vec<PathBuf>,
}

/// 路径是配置文件本身。实例不持有MRU快照，后台写入总以锁内读到的版本为准。
#[derive(Clone, Debug)]
pub(crate) struct VideoRecentStore {
    path: PathBuf,
}

impl Default for VideoRecentStore {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoRecentStore {
    pub(crate) fn new() -> Self {
        Self::at(crate::brand::local_data_root().join(FILE_NAME))
    }

    /// 显式配置路径，供测试或宿主注入；不访问媒体路径。
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// 只读有界配置，不创建文件；坏文件和未知版本返回错误，保持原字节不动。
    pub(crate) fn load(&self) -> io::Result<Vec<PathBuf>> {
        read_strict(&self.path)
    }

    /// 只能在播放器已成功提交首帧后调用；选择、预检和解码失败均不得记录。
    /// 本函数不验证媒体内容，失败由UI作为非阻断提示展示。
    pub(crate) fn record_opened(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let path = lexical_absolute(path)?;
        self.update(move |paths| {
            paths.retain(|previous| !same_path(previous, &path));
            paths.insert(0, path);
            paths.truncate(MAX_RECENT_VIDEOS);
        })
    }

    pub(crate) fn remove(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let path = lexical_absolute(path)?;
        self.update(move |paths| paths.retain(|previous| !same_path(previous, &path)))
    }

    pub(crate) fn clear(&self) -> io::Result<Vec<PathBuf>> {
        self.update(Vec::clear)
    }

    fn update(&self, change: impl FnOnce(&mut Vec<PathBuf>)) -> io::Result<Vec<PathBuf>> {
        // 此锁与JSON原子写入使用同一固定路径；必须覆盖reload、merge和落盘全过程。
        let _lock = crate::config_recovery::lock(&self.path)?;
        let mut latest = read_strict(&self.path)?;
        let previous = latest.clone();
        change(&mut latest);
        if latest != previous {
            json_file(&self.path).write_locked(&FileBody {
                paths: latest.clone(),
            })?;
        }
        Ok(latest)
    }
}

fn json_file(path: &Path) -> JsonFile<'_, FileBody> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        // 最近视频没有旧INI来源；只复用已持锁的原子写入，不调用迁移/宽松加载入口。
        legacy: |_| FileBody::default(),
    }
}

fn read_strict(path: &Path) -> io::Result<Vec<PathBuf>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid_data(tr!(
            "最近视频记录文件过大，未修改原文件",
            "The recent video file is too large and was not changed"
        )));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        invalid_data(tr!(
            "最近视频记录不是有效的UTF-8，未修改原文件",
            "The recent video file is not valid UTF-8 and was not changed"
        ))
    })?;
    let file: WireFile =
        serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|error| {
            invalid_data(trf!(
                "最近视频记录无法解析，未修改原文件：{error}",
                "Cannot parse recent videos; the file was not changed: {error}"
            ))
        })?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(invalid_data(tr!(
            "最近视频记录版本不受支持，未修改原文件",
            "The recent video file version is unsupported and was not changed"
        )));
    }
    if file.paths.len() > MAX_RECENT_VIDEOS {
        return Err(invalid_data(tr!(
            "最近视频记录项数超过上限，未修改原文件",
            "The recent video file has too many entries and was not changed"
        )));
    }
    let mut paths: Vec<PathBuf> = Vec::with_capacity(file.paths.len());
    for path in file.paths {
        let path = lexical_absolute(&path).map_err(|error| invalid_data(error.to_string()))?;
        // 文件中的重复项采用首项优先；Windows忽略大小写及普通分隔符差异。
        if !paths.iter().any(|previous| same_path(previous, &path)) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// 仅锚定完成的绝对路径可入库。组件重建只统一普通分隔符和`.`，不解析文件或链接。
fn lexical_absolute(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() || path.as_os_str().to_string_lossy().contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "最近视频记录需要有效的绝对路径",
                "Recent videos need valid absolute paths"
            ),
        ));
    }
    #[cfg(windows)]
    let units = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count()
    };
    #[cfg(not(windows))]
    let units = path.as_os_str().len();
    if units > MAX_PATH_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!("最近视频路径过长", "The recent video path is too long"),
        ));
    }
    Ok(path.components().collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_json::TestDir;

    fn media(name: &str) -> PathBuf {
        if cfg!(windows) {
            Path::new(r"C:\OneDrive\not-downloaded").join(name)
        } else {
            Path::new("/media/not-downloaded").join(name)
        }
    }

    fn store(directory: &TestDir) -> VideoRecentStore {
        VideoRecentStore::at(directory.join(FILE_NAME))
    }

    #[test]
    fn recent_list_is_bounded_and_record_does_not_open_media() {
        let directory = TestDir::new("video-recent-bound");
        let store = store(&directory);
        assert!(store.load().unwrap().is_empty());
        assert!(!store.path.exists());
        for number in 0..MAX_RECENT_VIDEOS + 4 {
            store
                .record_opened(&media(&format!("never-created-{number}.mp4")))
                .unwrap();
        }
        let paths = store.load().unwrap();
        assert_eq!(paths.len(), MAX_RECENT_VIDEOS);
        assert_eq!(paths[0], media("never-created-19.mp4"));
        assert_eq!(paths[MAX_RECENT_VIDEOS - 1], media("never-created-4.mp4"));
        assert!(store.record_opened(Path::new("relative.mp4")).is_err());
        assert_eq!(store.load().unwrap(), paths);
    }

    #[test]
    fn recent_reopen_moves_to_front_and_remove_and_clear_reload_latest() {
        let directory = TestDir::new("video-recent-merge");
        let first = store(&directory);
        let second = first.clone();
        first.record_opened(&media("a.mp4")).unwrap();
        let stale = first.load().unwrap();
        second.record_opened(&media("b.mp4")).unwrap();
        assert_eq!(stale, vec![media("a.mp4")]);
        assert_eq!(
            first.record_opened(&media("c.mp4")).unwrap(),
            vec![media("c.mp4"), media("b.mp4"), media("a.mp4")]
        );
        assert_eq!(
            second.record_opened(&media("a.mp4")).unwrap(),
            vec![media("a.mp4"), media("c.mp4"), media("b.mp4")]
        );
        assert_eq!(
            first.remove(&media("c.mp4")).unwrap(),
            vec![media("a.mp4"), media("b.mp4")]
        );
        assert!(second.clear().unwrap().is_empty());
        assert!(first.load().unwrap().is_empty());
    }

    #[test]
    fn malformed_future_and_invalid_path_files_are_not_replaced() {
        let directory = TestDir::new("video-recent-protect");
        let store = store(&directory);
        let cases = [
            b"{broken".as_slice(),
            b"{\"schema_version\":2,\"paths\":[]}".as_slice(),
            b"{\"schema_version\":1,\"paths\":[\"relative.mp4\"]}".as_slice(),
            b"{\"schema_version\":1,\"paths\":[],\"unknown\":true}".as_slice(),
            b"\xff\xfe".as_slice(),
        ];
        for original in cases {
            std::fs::write(&store.path, original).unwrap();
            assert!(store.load().is_err());
            assert!(store.record_opened(&media("new.mp4")).is_err());
            assert!(store.remove(&media("old.mp4")).is_err());
            assert!(store.clear().is_err());
            assert_eq!(std::fs::read(&store.path).unwrap(), original);
        }
    }

    #[test]
    fn oversized_config_is_read_bounded_and_never_overwritten() {
        let directory = TestDir::new("video-recent-large");
        let store = store(&directory);
        let bytes = vec![b' '; MAX_FILE_BYTES as usize + 32];
        std::fs::write(&store.path, &bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.record_opened(&media("new.mp4")).is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
    }

    #[cfg(windows)]
    #[test]
    fn windows_mru_deduplicates_case_separators_and_dot_components() {
        let directory = TestDir::new("video-recent-case");
        let store = store(&directory);
        store
            .record_opened(Path::new(r"C:\Media\Clip.mp4"))
            .unwrap();
        store
            .record_opened(Path::new(r"C:\Media\Other.mp4"))
            .unwrap();
        let paths = store
            .record_opened(Path::new("c:/MEDIA/./clip.mp4"))
            .unwrap();
        assert_eq!(paths.len(), 2);
        assert!(same_path(&paths[0], Path::new(r"c:\MEDIA\clip.mp4")));
        assert!(store.remove(Path::new(r"C:\media\CLIP.mp4")).unwrap().len() == 1);
    }
}
