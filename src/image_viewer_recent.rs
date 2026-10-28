//! 仅当前查看窗口的最近图片，不访问配置、文件内容或持久化存储。
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, MouseButton, ScrollHandle};
use std::{
    collections::VecDeque,
    path::{Component, Path, PathBuf},
};

const MAX_RECENT_IMAGES: usize = 12;

pub(super) struct RecentImages {
    entries: VecDeque<PathBuf>,
    open: bool,
    selected: usize,
    scroll: ScrollHandle,
}

impl Default for RecentImages {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            open: false,
            selected: 0,
            scroll: ScrollHandle::new(),
        }
    }
}

impl RecentImages {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// 由宿主仅在本次图片确实加载成功后调用；绝对化和去重不解析文件。
    pub(super) fn record(&mut self, path: &Path) {
        let Some(path) = lexical_absolute(path) else {
            return;
        };
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| same_path(entry, &path))
        {
            self.entries.remove(index);
        }
        self.entries.push_front(path);
        self.entries.truncate(MAX_RECENT_IMAGES);
        self.selected = 0;
        self.scroll.scroll_to_item(0);
    }

    pub(super) fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.selected = 0;
            self.scroll.scroll_to_item(0);
        }
    }

    pub(super) fn forget(&mut self, path: &Path) {
        let Some(path) = lexical_absolute(path) else {
            return;
        };
        self.entries.retain(|entry| !same_path(entry, &path));
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.scroll.scroll_to_item(self.selected);
    }

    pub(super) fn renamed(&mut self, previous: &Path, next: &Path) {
        self.forget(previous);
        self.record(next);
    }

    pub(super) fn hide(&mut self) {
        self.open = false;
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.selected = if self.entries.is_empty() {
            0
        } else {
            self.selected
                .saturating_add_signed(delta)
                .min(self.entries.len() - 1)
        };
        self.scroll.scroll_to_item(self.selected);
    }

    pub(super) fn selected_path(&self) -> Option<PathBuf> {
        self.entries.get(self.selected).cloned()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.selected = 0;
        self.scroll.scroll_to_item(0);
    }

    pub(super) fn render(&self, cx: &mut Context<super::ImageViewer>) -> AnyElement {
        if !self.open {
            return div().absolute().into_any_element();
        }
        let palette = crate::theme::palette(cx);
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(tr!("最近（本窗口）", "Recent (this window)"))
            .child(
                div()
                    .id("image-recent-hide")
                    .px(px(6.))
                    .cursor_pointer()
                    .text_color(rgb(palette.muted))
                    .hover(|style| style.text_color(rgb(palette.foreground)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, _, cx| {
                            cx.stop_propagation();
                            view.recent.hide();
                            cx.notify();
                        }),
                    )
                    .child("×"),
            );
        let mut list = div()
            .id("image-recent-scroll")
            .max_h(px(320.))
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap(px(3.));
        if self.entries.is_empty() {
            list = list.child(
                div()
                    .p(px(10.))
                    .text_color(rgb(palette.muted))
                    .child(tr!("本窗口尚无最近图片", "No recent images in this window")),
            );
        }
        for (index, path) in self.entries.iter().enumerate() {
            let name = path
                .file_name()
                .unwrap_or_else(|| path.as_os_str())
                .to_string_lossy()
                .into_owned();
            let parent = path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default();
            let path = path.clone();
            let selected = index == self.selected;
            list = list.child(
                div()
                    .id(("image-recent-row", index))
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .cursor_pointer()
                    .when(selected, |row| row.bg(rgb(palette.selected)))
                    .hover(move |style| {
                        style.bg(rgb(if selected {
                            palette.selected
                        } else {
                            palette.hover
                        }))
                    })
                    .on_mouse_move(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        if view.recent.selected != index {
                            view.recent.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.recent.hide();
                            view.open_path(path.clone(), true, cx);
                        }),
                    )
                    .child(div().w_full().truncate().child(name))
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(rgb(palette.muted))
                            .child(parent),
                    ),
            );
        }
        div()
            .id("image-viewer-recent")
            .absolute()
            .top(px(super::toolbar::TOOLBAR_HEIGHT + 4.))
            .left(px(12.))
            .right(px(12.))
            .max_w(px(420.))
            .max_h(px(440.))
            .p(px(10.))
            .rounded(px(6.))
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .border_1()
            .border_color(rgb(palette.border))
            .flex()
            .flex_col()
            .gap(px(8.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(header)
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .text_size(px(10.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "仅此窗口会话；保留原文件",
                        "This window only; files stay intact"
                    ))
                    .child(
                        div()
                            .id("image-recent-clear")
                            .flex_shrink_0()
                            .px(px(6.))
                            .py(px(3.))
                            .rounded(px(3.))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(palette.hover)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, _, _, cx| {
                                    cx.stop_propagation();
                                    view.recent.clear();
                                    cx.notify();
                                }),
                            )
                            .child(tr!("清空列表", "Clear list")),
                    ),
            )
            .into_any_element()
    }
}

fn lexical_absolute(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Some(normalized)
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

    #[test]
    fn image_viewer_recent_rename_and_recycle_remove_only_the_explicit_source() {
        let root = std::path::absolute("recent-file-actions-fixture").unwrap();
        let previous = root.join("old.png");
        let next = root.join("renamed.png");
        let other = root.join("other.png");
        let mut recent = RecentImages::default();
        for path in [&previous, &other, &next] {
            recent.record(path);
        }
        recent.renamed(&previous, &next);
        assert_eq!(
            recent.entries.iter().cloned().collect::<Vec<_>>(),
            vec![next.clone(), other.clone()]
        );
        recent.move_selection(isize::MAX);
        recent.forget(&next);
        assert_eq!(recent.selected_path(), Some(other.clone()));
        recent.forget(&other);
        assert!(recent.entries.is_empty());
        assert_eq!(recent.selected, 0);
    }

    #[test]
    fn image_viewer_recent_mru_is_bounded_and_lexically_deduplicated_without_files() {
        let root = std::path::absolute("recent-model-fixture").unwrap();
        let mut recent = RecentImages::default();
        for index in 0..13 {
            recent.record(&root.join(format!("image-{index}.png")));
        }
        assert_eq!(recent.entries.len(), 12);
        assert_eq!(recent.entries.front(), Some(&root.join("image-12.png")));
        assert_eq!(recent.entries.back(), Some(&root.join("image-1.png")));
        recent.record(&root.join("unused").join("..").join("image-5.png"));
        assert_eq!(recent.entries.front(), Some(&root.join("image-5.png")));
        assert_eq!(recent.entries.len(), 12);
        assert_eq!(recent.entries.back(), Some(&root.join("image-1.png")));
        #[cfg(windows)]
        {
            recent.record(&root.join("IMAGE-5.PNG"));
            assert_eq!(recent.entries.len(), 12);
            assert_eq!(recent.entries.front(), Some(&root.join("IMAGE-5.PNG")));
            assert_eq!(recent.entries.back(), Some(&root.join("image-1.png")));
        }
        assert_eq!(recent.selected, 0);
        recent.move_selection(isize::MAX);
        assert_eq!(recent.selected, 11);
        assert_eq!(recent.selected_path(), recent.entries.back().cloned());
        recent.move_selection(1);
        assert_eq!(recent.selected, 11);
        recent.move_selection(isize::MIN);
        recent.move_selection(-1);
        assert_eq!(recent.selected, 0);
        recent.move_selection(4);
        recent.toggle();
        assert!(recent.is_open());
        assert_eq!(recent.selected, 0);
        recent.move_selection(2);
        recent.record(&root.join("image-5.png"));
        assert_eq!(recent.selected, 0);
        recent.clear();
        assert_eq!(recent.selected_path(), None);
        recent.move_selection(isize::MAX);
        assert_eq!(recent.selected, 0);
    }
}
