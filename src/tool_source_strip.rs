//! 多图工具的来源条：缩略图按使用顺序排开，拖动改顺序，悬停移除，末尾“添加”。
//! 缩略图按路径在后台有界解码一次并缓存；拖放只在同一视图内生效。
use crate::{
    tool_window_chrome::Chrome,
    toolbar_icons::{self, Icon},
};
use gpui::{
    div, img, prelude::*, px, rgb, AnyElement, Context, Div, EntityId, ObjectFit, Render,
    RenderImage, SharedString, Stateful, Window,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const TILE_WIDTH: f32 = 132.;
const THUMB_HEIGHT: f32 = 84.;
const THUMB_MAX: (u32, u32) = (264, 168);

/// 按路径缓存的缩略图；`None` 表示正在读取或读取失败，不重复排队。
#[derive(Default)]
pub(crate) struct Thumbs {
    slots: HashMap<PathBuf, Option<Arc<RenderImage>>>,
    cancel: Arc<AtomicBool>,
}

/// 持有来源条缩略图缓存的视图。
pub(crate) trait ThumbHost: Sized + 'static {
    fn thumbs(&mut self) -> &mut Thumbs;
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Thumbs {
    pub(crate) fn get(&self, path: &Path) -> Option<Arc<RenderImage>> {
        self.slots.get(path).cloned().flatten()
    }

    /// 为还没排过队的路径逐张后台解码；视图关闭后停止。
    pub(crate) fn request<V: ThumbHost>(view: &mut V, paths: &[PathBuf], cx: &mut Context<V>) {
        let thumbs = view.thumbs();
        let missing: Vec<PathBuf> = paths
            .iter()
            .filter(|path| !thumbs.slots.contains_key(*path))
            .cloned()
            .collect();
        if missing.is_empty() {
            return;
        }
        for path in &missing {
            thumbs.slots.insert(path.clone(), None);
        }
        let cancel = thumbs.cancel.clone();
        cx.spawn(async move |view, cx| {
            for path in missing {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                let stop = cancel.clone();
                let source = path.clone();
                let thumb = cx
                    .background_executor()
                    .spawn(async move { decode_thumb(&source, &stop) })
                    .await;
                let Some(thumb) = thumb else {
                    continue;
                };
                let updated = view.update(cx, |view, cx| {
                    view.thumbs().slots.insert(path, Some(thumb));
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

fn decode_thumb(path: &Path, cancel: &AtomicBool) -> Option<Arc<RenderImage>> {
    let limits = crate::image_tools::ToolLimits::default();
    let decoded = crate::image_decode::decode_path_with_budget_cancellable(
        path,
        limits.max_input_pixels.saturating_mul(4),
        cancel,
    )
    .ok()?;
    let image = decoded.image;
    let thumb = crate::library::downscale(
        image.width,
        image.height,
        &image.bgra,
        THUMB_MAX.0,
        THUMB_MAX.1,
    );
    Some(crate::canvas_source::render_image(
        thumb.width,
        thumb.height,
        thumb.bgra,
    ))
}

/// 来源条的一格。
pub(crate) struct Tile {
    pub title: SharedString,
    pub thumb: Option<Arc<RenderImage>>,
    /// 左下角的小状态，例如批量结果；颜色为 RGB。
    pub badge: Option<(SharedString, u32)>,
}

/// 拖动中的来源：只认同一视图（`owner`）里发起的拖动。
#[derive(Clone)]
pub(crate) struct SourceDrag {
    owner: EntityId,
    index: usize,
    label: SharedString,
}

struct DragGhost(SharedString);

impl Render for DragGhost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let palette = crate::theme::palette(cx);
        div()
            .max_w(px(360.))
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(palette.accent))
            .bg(rgb(palette.background))
            .text_size(px(12.))
            .text_color(rgb(palette.foreground))
            .truncate()
            .child(self.0.clone())
    }
}

/// 把 `from` 移到原列表的插入边界 `before` 之前（`len` 表示末尾）；只有顺序实际变化才返回 true。
pub(crate) fn move_before<T>(items: &mut [T], from: usize, before: usize) -> bool {
    if from >= items.len() || before > items.len() {
        return false;
    }
    let target = if before > from { before - 1 } else { before };
    if target == from {
        return false;
    }
    if from < target {
        items[from..=target].rotate_left(1);
    } else {
        items[target..=from].rotate_right(1);
    }
    true
}

/// 让任意一行或一格可拖动排序：拖起显示标签，放到别的格上插在它前面。
#[allow(clippy::too_many_arguments)]
pub(crate) fn draggable<V: 'static>(
    element: Stateful<Div>,
    index: usize,
    label: impl Into<SharedString>,
    editable: bool,
    c: Chrome,
    cx: &mut Context<V>,
    on_move: fn(&mut V, usize, usize, &mut Window, &mut Context<V>),
) -> Stateful<Div> {
    if !editable {
        return element;
    }
    let owner = cx.entity_id();
    let drag = SourceDrag {
        owner,
        index,
        label: label.into(),
    };
    element
        .cursor_grab()
        .on_drag(drag, |drag: &SourceDrag, _, _, cx| {
            cx.new(|_| DragGhost(drag.label.clone()))
        })
        .drag_over::<SourceDrag>(move |style, _, _, _| style.border_color(rgb(c.palette.accent)))
        .on_drop(cx.listener(move |view, drag: &SourceDrag, window, cx| {
            if drag.owner == owner {
                on_move(view, drag.index, index, window, cx);
            }
            cx.stop_propagation();
        }))
}

/// 列表末尾的放置区：拖到这里移到最后。
pub(crate) fn drop_end<V: 'static>(
    element: Stateful<Div>,
    end: usize,
    editable: bool,
    c: Chrome,
    cx: &mut Context<V>,
    on_move: fn(&mut V, usize, usize, &mut Window, &mut Context<V>),
) -> Stateful<Div> {
    if !editable {
        return element;
    }
    let owner = cx.entity_id();
    element
        .drag_over::<SourceDrag>(move |style, _, _, _| style.border_color(rgb(c.palette.accent)))
        .on_drop(cx.listener(move |view, drag: &SourceDrag, window, cx| {
            if drag.owner == owner {
                on_move(view, drag.index, end, window, cx);
            }
            cx.stop_propagation();
        }))
}

/// 来源条：一行放不下就换行；每格缩略图、序号、名称，悬停显示移除；末尾是添加格。
#[allow(clippy::too_many_arguments)]
pub(crate) fn strip<V: 'static>(
    id: &'static str,
    tiles: Vec<Tile>,
    editable: bool,
    add: Option<SharedString>,
    c: Chrome,
    cx: &mut Context<V>,
    on_move: fn(&mut V, usize, usize, &mut Window, &mut Context<V>),
    on_remove: fn(&mut V, usize, &mut Window, &mut Context<V>),
    on_add: fn(&mut V, &mut Window, &mut Context<V>),
) -> Div {
    let count = tiles.len();
    let mut strip = div().flex().flex_wrap().gap_2();
    for (index, tile) in tiles.into_iter().enumerate() {
        let group: SharedString = format!("{id}-{index}").into();
        let mut picture = div()
            .relative()
            .h(px(THUMB_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(c.surface.track));
        picture = match tile.thumb {
            Some(thumb) => picture.child(img(thumb).size_full().object_fit(ObjectFit::Contain)),
            None => {
                picture.child(toolbar_icons::action(Icon::Image, c.palette.border).size(px(22.)))
            }
        };
        picture = picture.child(
            div()
                .absolute()
                .top(px(4.))
                .left(px(4.))
                .min_w(px(20.))
                .h(px(20.))
                .px_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(rgb(c.palette.accent))
                .text_size(px(11.))
                .text_color(rgb(c.surface.on_accent))
                .child((index + 1).to_string()),
        );
        if let Some((badge, color)) = tile.badge {
            picture = picture.child(
                div()
                    .absolute()
                    .bottom(px(4.))
                    .left(px(4.))
                    .px_1()
                    .rounded_sm()
                    .bg(rgb(c.surface.card))
                    .text_size(px(11.))
                    .text_color(rgb(color))
                    .child(badge),
            );
        }
        if editable {
            picture = picture.child(
                div()
                    .id((id, 10_000 + index))
                    .absolute()
                    .top(px(4.))
                    .right(px(4.))
                    .size(px(22.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .bg(rgb(c.surface.card))
                    .cursor_pointer()
                    .invisible()
                    .group_hover(group.clone(), |style| style.visible())
                    .hover(move |style| style.bg(rgb(c.palette.hover)))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        on_remove(view, index, window, cx)
                    }))
                    .child(toolbar_icons::action(Icon::Close, c.palette.foreground).size(px(14.))),
            );
        }
        let cell = div()
            .id((id, index))
            .group(group)
            .w(px(TILE_WIDTH))
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .rounded_lg()
            .border_1()
            .border_color(rgb(c.surface.line))
            .bg(rgb(c.surface.card))
            .child(picture)
            .child(
                div()
                    .px_1()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgb(c.palette.muted))
                    .child(tile.title.clone()),
            );
        strip = strip.child(draggable(
            cell,
            index,
            format!("{}. {}", index + 1, tile.title),
            editable,
            c,
            cx,
            on_move,
        ));
    }
    if let Some(label) = add {
        let add = div()
            .id((id, usize::MAX))
            .w(px(TILE_WIDTH))
            .h(px(THUMB_HEIGHT + 34.))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(rgb(c.surface.line))
            .text_size(px(12.))
            .text_color(rgb(c.palette.muted))
            .child(toolbar_icons::action(Icon::Images, c.palette.muted).size(px(20.)))
            .child(label);
        let add = drop_end(add, count, editable, c, cx, on_move);
        strip = strip.child(if editable {
            add.cursor_pointer()
                .hover(move |style| style.bg(rgb(c.palette.hover)))
                .on_click(cx.listener(move |view, _, window, cx| on_add(view, window, cx)))
                .into_any_element()
        } else {
            add.opacity(0.45).into_any_element()
        });
    }
    strip
}

/// 行内小缩略图，用于长列表（批量导出）。
pub(crate) fn row_thumb(thumb: Option<Arc<RenderImage>>, c: Chrome) -> AnyElement {
    let frame = div()
        .size(px(36.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .overflow_hidden()
        .bg(rgb(c.surface.track));
    match thumb {
        Some(thumb) => frame
            .child(img(thumb).size_full().object_fit(ObjectFit::Cover))
            .into_any_element(),
        None => frame
            .child(toolbar_icons::action(Icon::Image, c.palette.border).size(px(14.)))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::move_before;

    #[test]
    fn tool_source_strip_moves_before_boundaries_and_to_the_end() {
        for (from, before, expected) in [
            (0, 2, ["b", "a", "c", "d"]),
            (2, 0, ["c", "a", "b", "d"]),
            (1, 4, ["a", "c", "d", "b"]),
            (3, 1, ["a", "d", "b", "c"]),
        ] {
            let mut items = ["a", "b", "c", "d"];
            assert!(move_before(&mut items, from, before));
            assert_eq!(items, expected);
        }
    }

    #[test]
    fn tool_source_strip_same_place_and_out_of_range_keep_order() {
        for (from, before) in [(0, 0), (0, 1), (1, 1), (1, 2), (3, 4), (0, 5), (4, 0)] {
            let mut items = ["a", "b", "c", "d"];
            assert!(!move_before(&mut items, from, before));
            assert_eq!(items, ["a", "b", "c", "d"]);
        }
        assert!(!move_before::<u8>(&mut [], 0, 0));
    }
}
