//! 外部图片进图库：拖进主窗口、Ctrl+O 或“工具”菜单的“打开图片文件…”。
//! 条目只在本次窗口里列在最前的“外部文件”组，不写历史、不复制文件；加入后即被选中，
//! 之后贴出、编辑、工具都走和截图同一条路。
use super::{render_thumb, SettingsView};
use crate::{
    library::{self, Filter, Item},
    settings_view::main_page::MainPage,
};
use gpui::{point, px, Context, PathPromptOptions};
use std::path::{Path, PathBuf};

/// 一次最多加入的图片；更多的按顺序截断并提示。
const MAX_FILES: usize = 200;

/// 单独拖进一段视频时交给内置播放器，其余非图片文件跳过。
fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "wmv"
            )
        })
}

type Loaded = Result<(Item, library::Thumb), (PathBuf, String)>;

impl SettingsView {
    /// 一批外部路径：图片在后台逐张读出尺寸和缩略图后加入图库并选中；
    /// 只拖进一段视频时在内置播放器里打开；其它文件跳过并在状态栏说明。
    pub(crate) fn history_add_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let total = paths.len();
        let mut images: Vec<PathBuf> = paths
            .iter()
            .filter(|path| crate::image_collection::is_candidate(path) && path.is_file())
            .cloned()
            .collect();
        if images.is_empty() {
            match paths.as_slice() {
                [path] if is_video(path) && path.is_file() => {
                    self.open_media_popup(path.clone(), true, cx)
                }
                [] => {}
                _ => {
                    self.status = crate::tr!(
                        "没有可加入图库的图片；支持 PNG、JPEG、WebP、GIF、BMP、TIFF、HEIC 等",
                        "No images to add; PNG, JPEG, WebP, GIF, BMP, TIFF, HEIC and more are supported"
                    )
                    .into();
                    cx.notify();
                }
            }
            return;
        }
        let truncated = images.len() > MAX_FILES;
        images.truncate(MAX_FILES);
        let skipped = total - images.len();
        if self.page != MainPage::History {
            self.show_page(MainPage::History, cx);
        }
        self.status = crate::trf!("正在读取 {} 张图片…", "Reading {} images…", images.len());
        let task = cx.background_executor().spawn(async move {
            images
                .into_iter()
                .map(|path| {
                    library::external_item(&path).map_err(|error| (path, error.to_string()))
                })
                .collect::<Vec<Loaded>>()
        });
        cx.spawn(async move |this, cx| {
            let loaded = task.await;
            let _ = this.update(cx, |view, cx| {
                view.history_external_loaded(loaded, skipped, truncated, cx)
            });
        })
        .detach();
        cx.notify();
    }

    /// 读好的图片按拖入顺序排到最前，重复路径换成新读到的版本；随后全部选中。
    fn history_external_loaded(
        &mut self,
        loaded: Vec<Loaded>,
        skipped: usize,
        truncated: bool,
        cx: &mut Context<Self>,
    ) {
        let mut added = Vec::new();
        let mut failed = Vec::new();
        for result in loaded {
            match result {
                Ok((item, thumb)) => {
                    let page = &mut self.history;
                    page.external.retain(|old| old.key != item.key);
                    page.items.retain(|old| old.key != item.key);
                    page.thumb_failed.remove(&item.key);
                    page.thumb_revisions
                        .insert(item.key.clone(), library::thumbnail_revision(&item.source));
                    match render_thumb(thumb) {
                        Some(image) => {
                            page.thumbs.insert(item.key.clone(), image);
                        }
                        None => {
                            page.thumbs.remove(&item.key);
                        }
                    }
                    added.push(item);
                }
                Err((path, error)) => failed.push((path, error)),
            }
        }
        // 外部条目始终是 `items` 的开头一段，与 `external` 同序。
        for (index, item) in added.iter().enumerate() {
            self.history.external.insert(index, item.clone());
            self.history.items.insert(index, item.clone());
        }
        let keys: Vec<String> = added.iter().map(|item| item.key.clone()).collect();
        if !keys.is_empty() {
            if self.history.filter != Filter::All {
                self.history.filter = Filter::All;
            }
            self.history.search.update(cx, |field, cx| {
                if !field.text().is_empty() {
                    field.set_text(String::new(), cx);
                }
            });
            if let Some(tool) = self.history.tools.pick.as_ref().map(|pick| pick.tool) {
                // 正在选择模式里：新图直接勾上，仍受工具的张数上限约束。
                for key in keys.iter().cloned() {
                    if !self.history.chosen.contains(&key) {
                        self.history_pick_toggle(tool, key, cx);
                    }
                }
            } else {
                self.history_set_single(keys.first().cloned(), cx);
                self.history.chosen = keys.clone();
            }
            self.history.scroll.set_offset(point(px(0.), px(0.)));
        }
        let mut status = if keys.is_empty() {
            String::new()
        } else {
            crate::trf!(
                "已加入 {} 张外部图片（仅本次显示）",
                "Added {} external images (this session only)",
                keys.len()
            )
        };
        let mut note = |text: String| {
            if !status.is_empty() {
                status.push_str(crate::tr!("；", "; "));
            }
            status.push_str(&text);
        };
        if let Some((path, error)) = failed.first() {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            note(crate::trf!(
                "{} 张无法读取（{name}：{error}）",
                "{} could not be read ({name}: {error})",
                failed.len()
            ));
        }
        if skipped > 0 {
            note(if truncated {
                crate::trf!(
                    "一次最多加入 {} 张，其余 {} 个未加入",
                    "At most {} at a time; {} more not added",
                    MAX_FILES,
                    skipped
                )
            } else {
                crate::trf!(
                    "跳过 {} 个非图片文件",
                    "Skipped {} non-image files",
                    skipped
                )
            });
        }
        self.status = status;
        cx.notify();
    }

    /// Ctrl+O 与“打开图片文件…”：多选图片加入图库，不直接开查看器。
    pub(crate) fn history_choose_files(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(crate::tr!("加入图库", "Add to gallery").into()),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |view, cx| match result {
                Ok(Ok(Some(paths))) => view.history_add_files(paths, cx),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    view.status =
                        crate::trf!("无法选择文件：{error}", "Cannot choose files: {error}");
                    cx.notify();
                }
                Err(error) => {
                    view.status =
                        crate::trf!("无法选择文件：{error}", "Cannot choose files: {error}");
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
