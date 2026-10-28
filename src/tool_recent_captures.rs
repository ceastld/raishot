//! 工具窗口的“最近截图”来源：截图历史与带图片的贴图合在一起取最新几张，后台逐张生成缩略图；
//! 点选加入当前工具，再点移出。工具主要处理截图，选文件只作补充入口。
use crate::{
    history,
    library::{self, Source},
    pin_workspace,
    tool_window_chrome::Chrome,
};
use gpui::{div, img, prelude::*, px, rgb, Context, Div, ObjectFit, RenderImage, Window};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::SystemTime,
};

/// 最近的截图来源，按创建时间倒序：截图历史文件与贴图的图片资源（文字贴图没有图片，跳过）。
/// 截图历史可能被关闭，贴图仍是截图的主要去处，所以两处都取。
pub(crate) fn latest(limit: usize) -> Vec<(PathBuf, Source)> {
    let mut items: Vec<(SystemTime, PathBuf, Source)> = history::entries()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.modified, entry.path.clone(), Source::Shot(entry)))
        .collect();
    if let Ok(workspace) = pin_workspace::load() {
        for item in library::pin_items(&workspace, |_| false) {
            let Source::Pin {
                image: Some(name), ..
            } = &item.source
            else {
                continue;
            };
            if let Ok(path) = pin_workspace::asset_path(name) {
                if path.is_file() {
                    items.push((item.created, path, item.source.clone()));
                }
            }
        }
    }
    items.sort_by_key(|(created, ..)| std::cmp::Reverse(*created));
    items.dedup_by(|a, b| a.1 == b.1);
    items.truncate(limit);
    items
        .into_iter()
        .map(|(_, path, source)| (path, source))
        .collect()
}

/// 列出的截图数量；三列三行，参数栏不必滚动就能看全。
const LIMIT: usize = 9;
const COLUMNS: usize = 3;
const TILE_HEIGHT: f32 = 58.;

struct Capture {
    path: PathBuf,
    thumb: Option<Arc<RenderImage>>,
}

#[derive(Default)]
pub(crate) struct RecentCaptures {
    captures: Vec<Capture>,
    loaded: bool,
    cancel: Arc<AtomicBool>,
}

/// 持有“最近截图”的工具窗口。
pub(crate) trait RecentHost: Sized + 'static {
    fn recent(&mut self) -> &mut RecentCaptures;
}

impl Drop for RecentCaptures {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl RecentCaptures {
    /// 列出最近截图，再在后台逐张解码缩略图；窗口关闭后停止。
    pub(crate) fn load<V: RecentHost>(cx: &mut Context<V>) {
        cx.spawn(async move |view, cx| {
            let entries = cx
                .background_executor()
                .spawn(async { latest(LIMIT) })
                .await;
            let Ok(cancel) = view.update(cx, |view, cx| {
                let recent = view.recent();
                recent.loaded = true;
                recent.captures = entries
                    .iter()
                    .map(|(path, _)| Capture {
                        path: path.clone(),
                        thumb: None,
                    })
                    .collect();
                cx.notify();
                recent.cancel.clone()
            }) else {
                return;
            };
            for (path, source) in entries {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                let stop = cancel.clone();
                let thumb = cx
                    .background_executor()
                    .spawn(async move {
                        let thumb = library::thumbnail(&source, &|| stop.load(Ordering::Acquire))
                            .ok()??;
                        // GPUI RenderImage 的像素合同是 BGRA。
                        let pixels =
                            image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.bgra)?;
                        Some(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
                    })
                    .await;
                let updated = view.update(cx, |view, cx| {
                    if let Some(capture) = view
                        .recent()
                        .captures
                        .iter_mut()
                        .find(|capture| capture.path == path)
                    {
                        capture.thumb = thumb;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// 三列缩略图；已加入的显示强调边框和在当前列表中的序号。
    pub(crate) fn render<V: RecentHost>(
        &self,
        marked: &[PathBuf],
        enabled: bool,
        c: Chrome,
        cx: &mut Context<V>,
        pick: fn(&mut V, PathBuf, &mut Window, &mut Context<V>),
    ) -> Div {
        if self.loaded && self.captures.is_empty() {
            return crate::tool_window_chrome::hint(
                tr!(
                    "还没有截图或贴图；截图后会出现在这里。",
                    "No captures or pins yet; new captures appear here."
                ),
                c,
            );
        }
        let mut grid = div().flex().flex_col().gap_2();
        let mut tiles = self.captures.iter().enumerate().peekable();
        while tiles.peek().is_some() {
            let mut line = div().w_full().flex().gap_2();
            for _ in 0..COLUMNS {
                let Some((index, capture)) = tiles.next() else {
                    line = line.child(div().flex_1().flex_basis(px(0.)).min_w(px(0.)));
                    continue;
                };
                let order = marked.iter().position(|path| *path == capture.path);
                let path = capture.path.clone();
                let mut tile = div()
                    .id(("recent-capture", index))
                    .relative()
                    .flex_1()
                    .flex_basis(px(0.))
                    .min_w(px(0.))
                    .h(px(TILE_HEIGHT))
                    .rounded_md()
                    .overflow_hidden()
                    .border_2()
                    .border_color(rgb(if order.is_some() {
                        c.palette.accent
                    } else {
                        c.surface.line
                    }))
                    .bg(rgb(c.surface.track))
                    .when_some(capture.thumb.clone(), |tile, thumb| {
                        tile.child(img(thumb).size_full().object_fit(ObjectFit::Cover))
                    })
                    .when_some(order, |tile, order| {
                        tile.child(
                            div()
                                .absolute()
                                .top(px(3.))
                                .right(px(3.))
                                .min_w(px(18.))
                                .h(px(18.))
                                .px_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(rgb(c.palette.accent))
                                .text_size(px(11.))
                                .text_color(rgb(c.surface.on_accent))
                                .child((order + 1).to_string()),
                        )
                    });
                if enabled {
                    tile = tile
                        .cursor_pointer()
                        .hover(move |style| style.border_color(rgb(c.palette.accent)))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            pick(view, path.clone(), window, cx)
                        }));
                } else {
                    tile = tile.opacity(0.5);
                }
                line = line.child(tile);
            }
            grid = grid.child(line);
        }
        grid
    }
}

/// 列表里显示的名称与说明：截图与贴图文件名带 `YYYYMMDD-HHMMSS`，显示成时间和来源；
/// 其它文件显示文件名和所在文件夹。
pub(crate) fn describe(path: &std::path::Path) -> (String, Option<String>) {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let folder = path.parent().map(|parent| parent.display().to_string());
    let Some(time) = capture_time(&name) else {
        return (name, folder);
    };
    let inside = |dir: PathBuf| path.parent().is_some_and(|parent| parent.starts_with(&dir));
    let data = crate::preferences::data_dir();
    let kind = if inside(data.join("history")) {
        tr!("截图", "Capture")
    } else if inside(data.join("pin-workspace")) {
        tr!("贴图", "Pin")
    } else {
        return (name, folder);
    };
    (time, Some(kind.to_owned()))
}

/// 从 `shot-20261022-215949.png`、`20261022-215949.v2.png` 取出 `10-22 21:59:49`。
fn capture_time(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    (0..bytes.len().saturating_sub(14)).find_map(|start| {
        let part = bytes.get(start..start + 15)?;
        let digits = |range: std::ops::Range<usize>| part[range].iter().all(u8::is_ascii_digit);
        (digits(0..8) && part[8] == b'-' && digits(9..15)).then(|| {
            let text = &name[start..start + 15];
            format!(
                "{}-{} {}:{}:{}",
                &text[4..6],
                &text[6..8],
                &text[9..11],
                &text[11..13],
                &text[13..15]
            )
        })
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_recent_captures_reads_capture_time_from_names() {
        assert_eq!(
            super::capture_time("shot-20261022-215949.png").as_deref(),
            Some("10-22 21:59:49")
        );
        assert_eq!(
            super::capture_time("20261022-205352.v2.png").as_deref(),
            Some("10-22 20:53:52")
        );
        assert_eq!(super::capture_time("photo.png"), None);
        assert_eq!(super::capture_time("2026102-215949.png"), None);
    }
}
