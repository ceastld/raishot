//! 显式视频路径队列：有界、按输入顺序前进，不枚举目录，也不预解码媒体。
//! 重复项全部保留，大小写、点段或其它路径别名不合并，允许用户明确重复播放。
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_VIDEO_FILES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VideoFileState {
    Unchecked,
    /// 只通过本地普通文件的元数据检查，不表示容器或编解码已经可用。
    LocalFile,
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct VideoFileItem {
    path: PathBuf,
    state: VideoFileState,
}

impl VideoFileItem {
    /// 供列表显示；词法无效项仍返回其原始输入以便说明错误。
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// 交给实际读取端的词法绝对路径；语法失败项不能被重新解释后打开。
    pub(crate) fn open_path(&self) -> Option<&Path> {
        (self.path.is_absolute() && check_path_syntax(&self.path).is_ok())
            .then_some(self.path.as_path())
    }

    pub(crate) fn state(&self) -> &VideoFileState {
        &self.state
    }

    pub(crate) fn error(&self) -> Option<&str> {
        match &self.state {
            VideoFileState::Failed(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct VideoFileQueue {
    items: Vec<VideoFileItem>,
    index: Option<usize>,
}

impl VideoFileQueue {
    /// 只读取工作目录，不查询任何输入文件。超过上限时整批失败，不静默截断。
    /// 调用方应在构造成功后再替换旧队列，并自行取消旧来源的后台任务。
    pub(crate) fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, String> {
        let directory = std::env::current_dir().map_err(|error| error.to_string())?;
        Self::from_paths_in(paths, &directory)
    }

    /// 在输入时锚定相对路径，后续工作目录变化不会改变队列指向。
    /// 仅做 Path 词法拼接；不 canonicalize、不打开文件，也不折叠重复项。
    /// 空输入形成无当前项的空队列，供初始空窗使用；不是首尾循环的特殊项。
    pub(crate) fn from_paths_in(
        paths: impl IntoIterator<Item = PathBuf>,
        directory: &Path,
    ) -> Result<Self, String> {
        if !directory.is_absolute() || contains_nul(directory) {
            return Err(tr!(
                "视频队列的基准目录必须是有效的绝对路径",
                "The video queue needs a valid absolute base folder"
            )
            .into());
        }
        let mut items = Vec::new();
        for path in paths {
            // 至多消费第129项；即使传入无限迭代器，也不会无限收集或预检。
            if items.len() == MAX_VIDEO_FILES {
                return Err(trf!(
                    "一次最多打开 {MAX_VIDEO_FILES} 个视频文件",
                    "Open up to {MAX_VIDEO_FILES} video files at a time"
                ));
            }
            let (path, state) = match check_path_syntax(&path) {
                Ok(()) => {
                    let absolute = if path.is_absolute() {
                        path
                    } else {
                        directory.join(path)
                    };
                    (absolute, VideoFileState::Unchecked)
                }
                // 坏项保留在明确位置；由UI显示错误，导航不会悄悄跳过它。
                Err(error) => (path, VideoFileState::Failed(error)),
            };
            items.push(VideoFileItem { path, state });
        }
        let index = (!items.is_empty()).then_some(0);
        Ok(Self { items, index })
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn items(&self) -> &[VideoFileItem] {
        &self.items
    }

    pub(crate) fn item(&self, index: usize) -> Option<&VideoFileItem> {
        self.items.get(index)
    }

    pub(crate) fn index(&self) -> Option<usize> {
        self.index
    }

    /// UI显示从1开始的位置；空队列没有当前项。
    pub(crate) fn position(&self) -> Option<(usize, usize)> {
        self.index.map(|index| (index + 1, self.items.len()))
    }

    pub(crate) fn current(&self) -> Option<&VideoFileItem> {
        self.index.and_then(|index| self.items.get(index))
    }

    /// 首尾停住，不循环；错误项与普通项按同一顺序导航。
    pub(crate) fn navigate(&mut self, delta: isize) -> bool {
        let Some(index) = self.index else {
            return false;
        };
        let target = index.saturating_add_signed(delta).min(self.items.len() - 1);
        self.select(target)
    }

    pub(crate) fn previous(&mut self) -> bool {
        self.navigate(-1)
    }

    pub(crate) fn next(&mut self) -> bool {
        self.navigate(1)
    }

    pub(crate) fn first(&mut self) -> bool {
        self.select(0)
    }

    pub(crate) fn last(&mut self) -> bool {
        self.items
            .len()
            .checked_sub(1)
            .is_some_and(|index| self.select(index))
    }

    /// 越界或重复选中返回false，保留当前项与所有逐项错误。
    pub(crate) fn select(&mut self, index: usize) -> bool {
        if index >= self.items.len() || self.index == Some(index) {
            return false;
        }
        self.index = Some(index);
        true
    }

    /// 记录本地检查或后端打开错误，不改变当前项。
    /// 异步调用方必须先确认队列/请求身份仍有效，不能只凭索引应用旧队列的回执。
    pub(crate) fn record_result(&mut self, index: usize, result: Result<(), String>) -> bool {
        let Some(item) = self.items.get_mut(index) else {
            return false;
        };
        let state = match check_path_syntax(&item.path) {
            Err(error) => VideoFileState::Failed(error),
            Ok(()) => match result {
                Ok(()) => VideoFileState::LocalFile,
                Err(error) => VideoFileState::Failed(error),
            },
        };
        let changed = item.state != state;
        item.state = state;
        changed
    }

    /// 仅查询路径元数据，不读媒体内容；应在后台并在每次重开前调用。
    /// 成功不授予清理/写入权限，不保证随后打开时路径仍未被外部修改。
    pub(crate) fn check_local(&mut self, index: usize) -> Option<Result<(), String>> {
        self.check_local_with(index, crate::image_collection::ensure_local_file)
    }

    fn check_local_with(
        &mut self,
        index: usize,
        check: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Option<Result<(), String>> {
        let item = self.items.get(index)?;
        let result = check_path_syntax(&item.path).and_then(|()| check(&item.path));
        self.record_result(index, result.clone());
        Some(result)
    }
}

fn contains_nul(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().contains('\0')
}

fn check_path_syntax(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || contains_nul(path) {
        return Err(tr!(
            "视频文件路径为空或含无效字符",
            "The video path is empty or contains invalid characters"
        )
        .into());
    }
    // C:clip.mp4依赖各驱动器的隐式工作目录，不能用一个基准目录稳定锚定。
    // 普通相对路径、根相对路径、UNC和verbatim绝对路径保持Windows的词法语义。
    if !path.has_root() && matches!(path.components().next(), Some(Component::Prefix(_))) {
        return Err(tr!(
            "请使用完整视频路径或普通相对路径",
            "Use a full video path or a regular relative path"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn directory() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Media")
        } else {
            PathBuf::from("/media")
        }
    }

    fn queue(names: &[&str]) -> VideoFileQueue {
        VideoFileQueue::from_paths_in(names.iter().map(PathBuf::from), &directory()).unwrap()
    }

    #[test]
    fn explicit_order_and_duplicates_are_preserved() {
        let mut queue = queue(&["second.mp4", "first.mp4", "second.mp4"]);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.position(), Some((1, 3)));
        assert_eq!(
            queue.current().unwrap().path(),
            directory().join("second.mp4")
        );
        assert!(queue.next());
        assert_eq!(
            queue.current().unwrap().path(),
            directory().join("first.mp4")
        );
        assert!(queue.next());
        assert_eq!(
            queue.current().unwrap().path(),
            queue.item(0).unwrap().path()
        );
        assert_eq!(queue.items().len(), 3);
        assert!(queue
            .items()
            .iter()
            .all(|item| item.state() == &VideoFileState::Unchecked));
    }

    #[test]
    fn queue_bound_rejects_the_whole_batch_without_collecting_forever() {
        let accepted = VideoFileQueue::from_paths_in(
            (0..MAX_VIDEO_FILES).map(|index| PathBuf::from(format!("{index}.mp4"))),
            &directory(),
        )
        .unwrap();
        assert_eq!(accepted.len(), MAX_VIDEO_FILES);
        let consumed = Cell::new(0);
        let infinite = (0..).map(|index| {
            consumed.set(consumed.get() + 1);
            PathBuf::from(format!("{index}.mp4"))
        });
        assert!(VideoFileQueue::from_paths_in(infinite, &directory()).is_err());
        assert_eq!(consumed.get(), MAX_VIDEO_FILES + 1);
    }

    #[test]
    fn navigation_stops_at_edges_and_empty_queue_has_no_current() {
        let mut empty = queue(&[]);
        assert!(empty.is_empty());
        assert_eq!(empty.current(), None);
        assert_eq!(empty.index(), None);
        assert_eq!(empty.position(), None);
        assert!(!empty.next());
        assert!(!empty.first());
        assert!(!empty.last());
        let mut queue = queue(&["a.mp4", "b.mp4", "c.mp4"]);
        assert!(!queue.previous());
        assert!(queue.navigate(isize::MAX));
        assert_eq!(queue.index(), Some(2));
        assert!(!queue.next());
        assert!(!queue.select(99));
        assert_eq!(queue.index(), Some(2));
        assert!(queue.navigate(isize::MIN));
        assert_eq!(queue.index(), Some(0));
        assert!(queue.last());
        assert!(queue.first());
    }

    #[test]
    fn bad_items_and_their_errors_are_never_skipped() {
        let mut queue = queue(&["good.mp4", "", "later.mp4"]);
        assert!(queue.next());
        assert_eq!(queue.index(), Some(1));
        assert!(queue.current().unwrap().error().is_some());
        assert_eq!(queue.current().unwrap().open_path(), None);
        assert!(!queue.record_result(1, Ok(())));
        assert_eq!(queue.current().unwrap().open_path(), None);
        assert!(queue
            .check_local_with(1, |_| panic!("invalid path must not query metadata"))
            .unwrap()
            .is_err());
        assert!(queue.next());
        assert!(queue.record_result(2, Err("decoder could not open the file".into())));
        assert_eq!(queue.index(), Some(2));
        assert_eq!(
            queue.current().unwrap().error(),
            Some("decoder could not open the file")
        );
        assert!(queue.previous());
        assert!(queue.current().unwrap().error().is_some());
    }

    #[test]
    fn local_checks_only_update_the_requested_item_and_can_be_retried() {
        let mut queue = queue(&["missing.mp4", "good.mp4"]);
        assert!(queue
            .check_local_with(0, |_| Err("missing".into()))
            .unwrap()
            .is_err());
        assert_eq!(queue.current().unwrap().error(), Some("missing"));
        assert_eq!(queue.check_local_with(1, |_| Ok(())), Some(Ok(())));
        assert_eq!(queue.index(), Some(0));
        assert_eq!(queue.item(1).unwrap().state(), &VideoFileState::LocalFile);
        assert_eq!(queue.check_local_with(0, |_| Ok(())), Some(Ok(())));
        assert_eq!(queue.current().unwrap().error(), None);
        assert_eq!(
            queue.check_local_with(99, |_| panic!("out of bounds")),
            None
        );
    }

    #[test]
    fn paths_are_anchored_without_metadata_or_directory_scans() {
        let queue = queue(&[
            "OneDrive/not-downloaded.mp4",
            "../sibling.mp4",
            "bad\0name.mp4",
        ]);
        assert_eq!(
            queue.item(0).unwrap().path(),
            directory().join("OneDrive/not-downloaded.mp4")
        );
        assert_eq!(
            queue.item(1).unwrap().path(),
            directory().join("../sibling.mp4")
        );
        assert!(queue.item(2).unwrap().error().is_some());
        assert!(VideoFileQueue::from_paths_in(
            [PathBuf::from("clip.mp4")],
            Path::new("relative-base")
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_aliases_stay_explicit_and_drive_relative_paths_keep_their_error() {
        let queue = queue(&[
            r"C:\Media\clip.mp4",
            r"c:\MEDIA\clip.mp4",
            r"C:\Media\.\clip.mp4",
            r"C:clip.mp4",
            r"\other\clip.mp4",
        ]);
        assert_eq!(queue.len(), 5);
        assert_eq!(
            queue.item(0).unwrap().path(),
            Path::new(r"C:\Media\clip.mp4")
        );
        assert_eq!(
            queue.item(1).unwrap().path(),
            Path::new(r"c:\MEDIA\clip.mp4")
        );
        assert!(queue.item(3).unwrap().error().is_some());
        assert_eq!(queue.item(3).unwrap().open_path(), None);
        assert_eq!(
            queue.item(4).unwrap().path(),
            Path::new(r"C:\other\clip.mp4")
        );
    }
}
