//! 图库的工具层：多选、选择模式、“工具”菜单与底部操作条。
//! 工具来源直接取自选区，不弹文件框；可用与否统一由 `tool_registry` 判断。
use super::{widgets, SettingsView};
use crate::{
    library::{Item, PinState, Source},
    tool_registry::{self, Availability, Selection, ToolId},
    toolbar_icons::{self, Icon},
    tray_request::{self, Request},
};
use gpui::{
    anchored, deferred, div, img, point, prelude::*, px, rgb, AnyElement, ClickEvent, Context,
    Corner, Div, Edges, ElementId, FontWeight, ObjectFit, SharedString, Stateful, Window,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// 同一份菜单可从工具栏、详情栏或底部操作条展开，各自锚在自己的按钮上。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuOrigin {
    Toolbar,
    Detail,
    Bar,
    /// 顶栏“新建 ▾”，不是工具菜单，但与它共用同一个展开状态，两者不会同时打开。
    New,
}

/// 工具先行：先选工具，再到图库里勾选来源。取消时恢复进入前的选区。
pub(super) struct Pick {
    pub(super) tool: ToolId,
    restore: (Option<String>, Vec<String>),
    /// 从工具弹层回来挑图：结束后把来源交回弹层，而不是另开工具。
    /// 里面是当前来源中不在图库里的外部文件，挑完接在后面。
    returning: Option<Vec<PathBuf>>,
}

#[derive(Default)]
pub(super) struct ToolState {
    pub(super) pick: Option<Pick>,
    pub(super) menu: Option<MenuOrigin>,
    /// 单图工具打开前的后台解码；新的打开或关窗即取消。
    loading: Option<Arc<AtomicBool>>,
}
impl Drop for ToolState {
    fn drop(&mut self) {
        if let Some(cancel) = &self.loading {
            cancel.store(true, Ordering::Release);
        }
    }
}

/// 有像素的条目：截图、外部图片与图片贴图；文字贴图没有底图，不进图片工具。
pub(super) fn is_image(item: &Item) -> bool {
    matches!(
        item.source,
        Source::Shot(_)
            | Source::File(_)
            | Source::Pin {
                image: Some(_),
                text: false,
                ..
            }
    )
}

pub(super) fn is_recording(item: &Item) -> bool {
    matches!(item.source, Source::Recording(_))
}

fn image_path(item: &Item) -> Option<PathBuf> {
    match &item.source {
        Source::Shot(entry) => Some(entry.path.clone()),
        Source::File(entry) => Some(entry.path.clone()),
        Source::Pin {
            image: Some(name),
            text: false,
            ..
        } => crate::pin_workspace::asset_path(name)
            .ok()
            .filter(|path| path.is_file()),
        _ => None,
    }
}

fn image_paths(items: &[Item]) -> Result<Vec<PathBuf>, String> {
    items
        .iter()
        .filter(|item| is_image(item))
        .map(|item| {
            image_path(item).ok_or_else(|| {
                crate::trf!(
                    "找不到“{}”的图片文件",
                    "The image file for \"{}\" is missing",
                    item.display_title()
                )
            })
        })
        .collect()
}

/// 图标加文字的按钮；主按钮用强调色。
/// 下拉菜单的外框；“工具”和“新建”共用。
pub(super) fn menu_panel(id: &'static str, cx: &gpui::App) -> Stateful<Div> {
    let surface = widgets::surface(cx);
    let background = if crate::theme::is_light(cx) {
        0xffffff
    } else {
        0x262d36
    };
    div()
        .id(id)
        .occlude()
        .w(px(300.))
        .p(px(6.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(surface.line))
        .bg(rgb(background))
        .shadow_lg()
        .flex()
        .flex_col()
}

/// 菜单的一行：图标、标题，右侧一句提示（缺什么或快捷键）；不可用时整行变灰但仍可点。
pub(super) fn menu_item(
    id: SharedString,
    icon: Icon,
    title: &'static str,
    hint: String,
    ready: bool,
    palette: crate::capture_toolbar::Palette,
) -> Stateful<Div> {
    let tone = if ready {
        palette.foreground
    } else {
        palette.muted
    };
    div()
        .id(id)
        .h(px(32.))
        .px(px(10.))
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(palette.hover)))
        .child(toolbar_icons::action(icon, tone).size(px(16.)))
        .child(
            div()
                .flex_1()
                .text_size(px(13.))
                .text_color(rgb(tone))
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(if ready { palette.accent } else { palette.muted }))
                .child(hint),
        )
}

pub(super) fn menu_separator(surface: widgets::Surface) -> Div {
    div().h(px(1.)).mx_1().my_1().bg(rgb(surface.line))
}

pub(super) fn action_button(
    id: impl Into<ElementId>,
    icon: Option<Icon>,
    label: impl Into<SharedString>,
    primary: bool,
    palette: crate::capture_toolbar::Palette,
    surface: widgets::Surface,
) -> Stateful<Div> {
    let color = if primary {
        surface.on_accent
    } else {
        palette.foreground
    };
    div()
        .id(id)
        .h(px(32.))
        .px(px(12.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .rounded_md()
        .cursor_pointer()
        .text_size(px(13.))
        .whitespace_nowrap()
        .border_1()
        .when(primary, |button| {
            button
                .border_color(rgb(palette.accent))
                .bg(rgb(palette.accent))
                .text_color(rgb(surface.on_accent))
                .font_weight(FontWeight::SEMIBOLD)
                .hover(|style| style.opacity(0.9))
        })
        .when(!primary, |button| {
            button
                .border_color(rgb(surface.line))
                .bg(rgb(surface.raised))
                .text_color(rgb(palette.foreground))
                .hover(move |style| style.bg(rgb(palette.hover)))
        })
        .when_some(icon, |button, icon| {
            button.child(toolbar_icons::action(icon, color).size(px(16.)))
        })
        .child(label.into())
}

impl SettingsView {
    /// 选区按选择顺序；没有多选记录时就是当前项。
    pub(super) fn history_chosen(&self) -> Vec<&Item> {
        let find = |key: &String| self.history.items.iter().find(|item| &item.key == key);
        if self.history.chosen.is_empty() {
            return self.history.selected.iter().filter_map(find).collect();
        }
        self.history.chosen.iter().filter_map(find).collect()
    }

    pub(super) fn history_selection(&self) -> Selection {
        let chosen = self.history_chosen();
        Selection {
            images: chosen.iter().filter(|item| is_image(item)).count(),
            recordings: chosen.iter().filter(|item| is_recording(item)).count(),
        }
    }

    pub(super) fn history_multi(&self) -> bool {
        self.history.chosen.len() > 1
    }

    /// 单选：方向键、撤销、删除顺延等改变当前项时一并重置选区。
    pub(super) fn history_set_single(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        if self.history.selected != key || self.history_multi() {
            self.history_clear_video_preview(cx);
        }
        self.history.chosen = key.iter().cloned().collect();
        self.history.selected = key;
    }

    /// 单击单选，Ctrl 增减，Shift 按当前显示顺序选一段；选择模式下单击即勾选。
    pub(super) fn history_card_click(
        &mut self,
        key: String,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.history.focus);
        self.history.tools.menu = None;
        if let Some(tool) = self.history.tools.pick.as_ref().map(|pick| pick.tool) {
            self.history_pick_toggle(tool, key, cx);
            return;
        }
        let modifiers = event.modifiers();
        if modifiers.shift {
            if let Some(anchor) = self.history.selected.clone() {
                let keys: Vec<String> = self
                    .history_visible(cx)
                    .into_iter()
                    .map(|index| self.history.items[index].key.clone())
                    .collect();
                let from = keys.iter().position(|k| *k == anchor);
                let to = keys.iter().position(|k| *k == key);
                if let (Some(from), Some(to)) = (from, to) {
                    self.history.chosen = keys[from.min(to)..=from.max(to)].to_vec();
                    self.history_clear_video_preview(cx);
                    cx.notify();
                    return;
                }
            }
        }
        if modifiers.control || modifiers.platform {
            if self.history.chosen.is_empty() {
                self.history.chosen.extend(self.history.selected.clone());
            }
            if let Some(index) = self.history.chosen.iter().position(|k| *k == key) {
                self.history.chosen.remove(index);
                self.history.selected = self.history.chosen.last().cloned();
            } else {
                self.history.chosen.push(key.clone());
                self.history.selected = Some(key);
            }
            self.history_clear_video_preview(cx);
            cx.notify();
            return;
        }
        self.history_set_single(Some(key), cx);
        if event.click_count() >= 2 {
            self.history_primary(cx);
        }
        cx.notify();
    }

    /// 选择模式下只全选工具接受的条目，且不超过上限。
    pub(super) fn history_select_all(&mut self, cx: &mut Context<Self>) {
        let pick = self.history.tools.pick.as_ref().map(|pick| pick.tool);
        let mut keys: Vec<String> = self
            .history_visible(cx)
            .into_iter()
            .map(|index| &self.history.items[index])
            .filter(|item| pick.is_none_or(|tool| tool.takes(is_image(item), is_recording(item))))
            .map(|item| item.key.clone())
            .collect();
        if let Some(tool) = pick {
            keys.truncate(tool.limit());
            self.history.selected = keys.last().cloned();
        } else if self.history.selected.is_none() {
            self.history.selected = keys.first().cloned();
        }
        self.history.chosen = keys;
        self.history_clear_video_preview(cx);
        cx.notify();
    }

    /// 选区够用就直接执行；不够就进入选择模式，保留已选中且被接受的项。
    pub(super) fn history_tool(&mut self, tool: ToolId, cx: &mut Context<Self>) {
        self.history.tools.menu = None;
        if tool_registry::availability(tool, self.history_selection()).ready() {
            self.history_run_tool(tool, cx);
            cx.notify();
            return;
        }
        let restore = (self.history.selected.clone(), self.history.chosen.clone());
        let mut kept: Vec<String> = self
            .history_chosen()
            .into_iter()
            .filter(|item| tool.takes(is_image(item), is_recording(item)))
            .map(|item| item.key.clone())
            .collect();
        kept.truncate(tool.limit());
        self.history_clear_video_preview(cx);
        self.history.selected = kept.last().cloned();
        self.history.chosen = kept;
        self.history.tools.pick = Some(Pick {
            tool,
            restore,
            returning: None,
        });
        cx.notify();
    }

    pub(super) fn history_pick_toggle(
        &mut self,
        tool: ToolId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.history.items.iter().find(|item| item.key == key) else {
            return;
        };
        if !tool.takes(is_image(item), is_recording(item)) {
            return;
        }
        let chosen = &mut self.history.chosen;
        if let Some(index) = chosen.iter().position(|k| *k == key) {
            chosen.remove(index);
        } else {
            if chosen.len() >= tool.limit() {
                chosen.pop();
            }
            chosen.push(key);
        }
        self.history.selected = self.history.chosen.last().cloned();
        cx.notify();
    }

    /// 图库列表的键盘焦点；工具弹层收起挑图时交给它。
    pub(crate) fn history_focus_handle(&self) -> gpui::FocusHandle {
        self.history.focus.clone()
    }

    /// 工具弹层要更多来源：按当前来源预先勾选，进入该工具的选择模式。
    pub(crate) fn history_pick_for_popup(
        &mut self,
        tool: ToolId,
        current: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.history.tools.menu = None;
        let restore = (self.history.selected.clone(), self.history.chosen.clone());
        let mut chosen = Vec::new();
        let mut external = Vec::new();
        for path in current {
            let key = self
                .history
                .items
                .iter()
                .find(|item| is_image(item) && image_path(item).as_ref() == Some(&path))
                .map(|item| item.key.clone());
            match key {
                Some(key) if !chosen.contains(&key) => chosen.push(key),
                Some(_) => {}
                None => external.push(path),
            }
        }
        chosen.truncate(tool.limit());
        self.history_clear_video_preview(cx);
        self.history.selected = chosen.last().cloned();
        self.history.chosen = chosen;
        self.history.tools.pick = Some(Pick {
            tool,
            restore,
            returning: Some(external),
        });
        cx.notify();
    }

    pub(super) fn history_pick_go(&mut self, cx: &mut Context<Self>) {
        let Some(tool) = self.history.tools.pick.as_ref().map(|pick| pick.tool) else {
            return;
        };
        if !tool_registry::availability(tool, self.history_selection()).ready() {
            return;
        }
        let pick = self.history.tools.pick.take().expect("pick mode");
        if let Some(external) = pick.returning {
            let mut paths: Vec<PathBuf> = self
                .history_chosen()
                .into_iter()
                .filter_map(|item| image_path(item))
                .collect();
            for path in external {
                if paths.len() < tool.limit() && !paths.contains(&path) {
                    paths.push(path);
                }
            }
            let (selected, chosen) = pick.restore;
            self.history.selected = selected;
            self.history.chosen = chosen;
            self.finish_tool_pick(Some(paths), cx);
        } else {
            self.history_run_tool(tool, cx);
        }
        cx.notify();
    }

    pub(crate) fn history_pick_cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(pick) = self.history.tools.pick.take() {
            let (selected, chosen) = pick.restore;
            self.history.selected = selected;
            self.history.chosen = chosen;
            self.history_clear_video_preview(cx);
            if pick.returning.is_some() {
                self.finish_tool_pick(None, cx);
            }
            cx.notify();
        }
    }

    /// 只关闭弹出的菜单或选择模式；都没有时返回 false，交给原有 Esc 逻辑。
    pub(super) fn history_tools_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.history.tools.menu.take().is_some() {
            cx.notify();
            return true;
        }
        if self.history.tools.pick.is_some() {
            self.history_pick_cancel(cx);
            return true;
        }
        false
    }

    fn history_run_tool(&mut self, tool: ToolId, cx: &mut Context<Self>) {
        let items: Vec<Item> = self.history_chosen().into_iter().cloned().collect();
        let result: Result<String, String> = match tool {
            ToolId::View | ToolId::Gif => match items.first() {
                Some(item) if is_recording(item) => {
                    let path = item.path().map(|path| path.to_path_buf());
                    if let Some(path) = path {
                        self.open_media_popup(path, true, cx);
                    }
                    Ok(if tool == ToolId::Gif {
                        crate::tr!(
                            "在播放器里选好片段，点 GIF 导出",
                            "Pick a range in the player, then export GIF"
                        )
                        .into()
                    } else {
                        String::new()
                    })
                }
                Some(item) => match image_path(item) {
                    Some(path) => {
                        self.open_media_popup(path, false, cx);
                        Ok(String::new())
                    }
                    None => image_paths(&items).map(|_| String::new()),
                },
                None => Ok(String::new()),
            },
            ToolId::Pin => self.history_pin_items(&items, cx),
            // 编辑即贴出并展开工具栏，在贴图上原位标注。
            ToolId::Edit => image_paths(&items).and_then(|paths| {
                let path = paths.into_iter().next().unwrap_or_default();
                tray_request::send(Request::PinEditFile(path))
                    .map(|_| String::new())
                    .map_err(|error| error.to_string())
            }),
            // 多图工具在主窗口弹层里打开，来源按选择顺序。
            ToolId::Compare => image_paths(&items).map(|paths| {
                self.open_tool_popup(
                    Some(tool),
                    tool.title(),
                    move |window, cx| {
                        crate::image_tool_view::compare_view::popup(paths, window, cx)
                    },
                    cx,
                );
                String::new()
            }),
            ToolId::Collage => image_paths(&items).map(|paths| {
                self.open_tool_popup(
                    Some(tool),
                    tool.title(),
                    move |window, cx| {
                        crate::image_tool_view::collage_view::popup(paths, window, cx)
                    },
                    cx,
                );
                String::new()
            }),
            ToolId::Pdf => image_paths(&items).map(|paths| {
                self.open_tool_popup(
                    Some(tool),
                    tool.title(),
                    move |window, cx| crate::image_tool_view::pdf_view::popup(paths, window, cx),
                    cx,
                );
                String::new()
            }),
            ToolId::Batch => image_paths(&items).map(|paths| {
                self.open_tool_popup(
                    Some(tool),
                    tool.title(),
                    move |window, cx| crate::image_tool_view::batch_view::popup(paths, window, cx),
                    cx,
                );
                String::new()
            }),
            ToolId::Split | ToolId::Measure | ToolId::Barcode => image_paths(&items).map(|paths| {
                if let Some(path) = paths.into_iter().next() {
                    self.history_open_decoded(tool, path, cx);
                }
                crate::tr!("正在读取图片…", "Loading image…").into()
            }),
        };
        match result {
            Ok(message) if message.is_empty() => {}
            Ok(message) => self.status = message,
            Err(error) => {
                self.status = crate::trf!(
                    "无法打开“{}”：{error}",
                    "Cannot open {}: {error}",
                    tool.title()
                )
            }
        }
    }

    /// 截图贴出，关闭的贴图恢复原记录；已在屏幕上的贴图跳过。
    fn history_pin_items(
        &mut self,
        items: &[Item],
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let mut sent = 0;
        let mut restored = false;
        for item in items {
            let request = match &item.source {
                Source::Shot(entry) => Request::PinFile(entry.path.clone()),
                Source::File(entry) => Request::PinFile(entry.path.clone()),
                Source::Pin {
                    state: PinState::Live,
                    ..
                }
                | Source::Recording(_) => continue,
                Source::Pin { id, .. } => {
                    restored = true;
                    Request::PinRecord(id.clone())
                }
            };
            tray_request::send(request).map_err(|error| error.to_string())?;
            sent += 1;
        }
        if restored {
            self.history_rescan_later(cx);
        }
        Ok(if sent == 0 {
            crate::tr!(
                "所选贴图已在屏幕上",
                "The selected pins are already on screen"
            )
            .into()
        } else {
            crate::trf!("已贴出 {} 张", "Pinned {}", sent)
        })
    }

    /// 分割、测量、条码要整张源像素；后台解码后再开窗口，期间可被新的打开取消。
    fn history_open_decoded(&mut self, tool: ToolId, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(previous) = self.history.tools.loading.take() {
            previous.store(true, Ordering::Release);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.history.tools.loading = Some(cancel.clone());
        let loading_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            crate::image_tool_view::decode_tool_source(&path, &loading_cancel)
                .map(|decoded| (path, decoded))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                if view
                    .history
                    .tools
                    .loading
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &cancel))
                {
                    view.history.tools.loading = None;
                }
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let opened = result.map(|(path, decoded)| {
                    view.open_single_tool_popup(tool, Arc::new(decoded.image), path, cx);
                    decoded.warning
                });
                view.status = match opened {
                    Ok(Some(warning)) => warning,
                    Ok(None) => String::new(),
                    Err(error) => crate::trf!(
                        "无法打开“{}”：{error}",
                        "Cannot open {}: {error}",
                        tool.title()
                    ),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// “工具 ▾”按钮连同展开后的菜单；菜单外按下即关闭。
    pub(super) fn history_tools_trigger(&self, origin: MenuOrigin, cx: &mut Context<Self>) -> Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let label = if origin == MenuOrigin::Toolbar
            || tool_registry::recommended(self.history_selection()).is_empty()
        {
            crate::tr!("工具", "Tools")
        } else {
            crate::tr!("更多工具", "More tools")
        };
        let open = self.history.tools.menu == Some(origin);
        let button = action_button(
            SharedString::from(format!("history-tools-{origin:?}")),
            Some(Icon::Apps),
            label,
            false,
            palette,
            surface,
        )
        .child(
            toolbar_icons::action(
                if open { Icon::ChevronUp } else { Icon::Chevron },
                palette.muted,
            )
            .size(px(12.)),
        )
        .when(origin == MenuOrigin::Detail, |button| button.w_full())
        .on_click(cx.listener(move |view, _, _, cx| {
            view.history.tools.menu = if view.history.tools.menu == Some(origin) {
                None
            } else {
                Some(origin)
            };
            cx.notify();
        }));
        let menu = open.then(|| self.history_tools_menu(cx).into_any_element());
        self.history_menu_slot(origin, button, menu, cx)
    }

    /// 菜单按钮连同展开后的菜单：菜单外按下即关闭，菜单按来源贴在按钮的一角。
    pub(super) fn history_menu_slot(
        &self,
        origin: MenuOrigin,
        button: Stateful<Div>,
        menu: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> Div {
        let (corner, offset, holder) = match origin {
            MenuOrigin::Toolbar | MenuOrigin::New => {
                (Corner::TopRight, 4., div().absolute().right_0().bottom_0())
            }
            MenuOrigin::Detail => (Corner::TopLeft, 4., div().absolute().left_0().bottom_0()),
            MenuOrigin::Bar => (Corner::BottomLeft, -4., div().absolute().left_0().top_0()),
        };
        div()
            .relative()
            .flex_shrink_0()
            .when(origin == MenuOrigin::Detail, |slot| {
                slot.w_full().flex().flex_col()
            })
            .child(button)
            .when_some(menu, |slot, menu| {
                slot.child(deferred(
                    anchored()
                        .position(point(px(0.), px(0.)))
                        .snap_to_window()
                        .child(
                            div()
                                .id("history-tools-backdrop")
                                .w(px(8000.))
                                .h(px(8000.))
                                .occlude()
                                .on_any_mouse_down(cx.listener(|view, _, _, cx| {
                                    view.history.tools.menu = None;
                                    cx.notify();
                                })),
                        ),
                ))
                .child(
                    holder.child(
                        deferred(
                            anchored()
                                .anchor(corner)
                                .offset(point(px(0.), px(offset)))
                                .snap_to_window_with_margin(Edges::all(px(8.)))
                                .child(menu),
                        )
                        .with_priority(1),
                    ),
                )
            })
    }

    fn history_tools_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let selection = self.history_selection();
        let count = self.history_chosen().len();
        let item =
            |id: SharedString, icon: Icon, title: &'static str, hint: String, ready: bool| {
                menu_item(id, icon, title, hint, ready, palette)
            };
        let separator = || menu_separator(surface);
        let mut menu = menu_panel("history-tools-menu", cx).child(
            div()
                .px(px(10.))
                .pt_1()
                .pb_1()
                .text_size(px(11.))
                .text_color(rgb(palette.muted))
                .child(if count > 0 {
                    crate::trf!("对已选 {} 项", "For {} selected", count)
                } else {
                    crate::tr!("先选工具，再在图库里挑图", "Pick a tool, then pick items").into()
                }),
        );
        for entry in tool_registry::MENU {
            let Some(tool) = entry else {
                menu = menu.child(separator());
                continue;
            };
            let availability = tool_registry::availability(tool, selection);
            let hint = match availability {
                Availability::Ready { partial: true } => {
                    crate::trf!("{} 张图片", "{} images", selection.images)
                }
                Availability::Ready { partial: false } => String::new(),
                need => tool_registry::need_text(need),
            };
            menu = menu.child(
                item(
                    SharedString::from(format!("history-tool-{}", tool.id())),
                    tool.icon(),
                    tool.title(),
                    hint,
                    availability.ready() || count == 0,
                )
                .on_click(cx.listener(move |view, _, _, cx| view.history_tool(tool, cx))),
            );
        }
        menu.child(separator()).child(
            item(
                "history-tool-open-video".into(),
                Icon::Video,
                crate::tr!("打开视频文件…", "Open video file…"),
                String::new(),
                true,
            )
            .on_click(cx.listener(|view, _, _, cx| {
                view.history.tools.menu = None;
                view.choose_media_popup(true, cx);
            })),
        )
    }

    /// 选择模式横幅：说清楚要选什么、已选几项，Enter 开始、Esc 取消。
    pub(super) fn history_pick_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tool = self.history.tools.pick.as_ref()?.tool;
        let returning = self
            .history
            .tools
            .pick
            .as_ref()
            .is_some_and(|pick| pick.returning.is_some());
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let count = self.history_chosen().len();
        let ready = tool_registry::availability(tool, self.history_selection()).ready();
        let counter = if tool.limit() == usize::MAX {
            crate::trf!("已选 {}", "{} picked", count)
        } else {
            crate::trf!("已选 {}/{}", "{}/{} picked", count, tool.limit())
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_lg()
                .border_1()
                .border_color(rgb(palette.accent))
                .bg(rgb(palette.selected))
                .child(
                    div()
                        .size(px(30.))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .bg(rgb(palette.accent))
                        .child(toolbar_icons::action(tool.icon(), surface.on_accent).size(px(16.))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(tool.title()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(palette.muted))
                                .child(tool.goal()),
                        ),
                )
                .child(div().text_size(px(13.)).child(counter))
                .child(
                    action_button(
                        "history-pick-cancel",
                        None,
                        crate::tr!("取消 Esc", "Cancel Esc"),
                        false,
                        palette,
                        surface,
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.history_pick_cancel(cx))),
                )
                .child(
                    action_button(
                        "history-pick-go",
                        None,
                        if returning {
                            crate::tr!("完成 Enter", "Done Enter")
                        } else {
                            crate::tr!("开始 Enter", "Start Enter")
                        },
                        ready,
                        palette,
                        surface,
                    )
                    .when(!ready, |button| button.opacity(0.5))
                    .on_click(cx.listener(|view, _, _, cx| view.history_pick_go(cx))),
                )
                .into_any_element(),
        )
    }

    /// 多选时浮在列表底部：推荐动作、更多工具、删除与取消选择。
    /// `detail` 为真时详情栏已列出“已选 N 项”和顺序，操作条只留动作，窄窗口也放得下。
    pub(super) fn history_action_bar(
        &self,
        detail: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.history.tools.pick.is_some() || !self.history_multi() {
            return None;
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let selection = self.history_selection();
        let count = self.history_chosen().len();
        let summary = if selection.images > 0 && selection.recordings > 0 {
            crate::trf!(
                "{} 张图片 · {} 段录屏",
                "{} images · {} recordings",
                selection.images,
                selection.recordings
            )
        } else {
            crate::tr!("角标是选择顺序", "Badges show pick order").into()
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap_2()
            .pl(px(if detail { 8. } else { 16. }))
            .pr_2()
            .py_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(surface.line))
            .bg(rgb(surface.raised))
            .shadow_lg()
            // 窄窗口里说明先缩成省略号，按钮保持完整，整条不超出图库栏。
            .max_w_full()
            .when(!detail, |bar| {
                bar.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_nowrap()
                        .child(crate::trf!("已选 {} 项", "{} selected", count)),
                )
                .child(
                    div()
                        .mr_2()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .text_color(rgb(palette.muted))
                        .whitespace_nowrap()
                        .child(summary),
                )
            });
        for (index, &tool) in tool_registry::recommended(selection).iter().enumerate() {
            bar = bar.child(
                action_button(
                    SharedString::from(format!("history-bar-{}", tool.id())),
                    Some(tool.icon()),
                    tool.title(),
                    index == 0,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| view.history_tool(tool, cx))),
            );
        }
        Some(
            bar.child(self.history_tools_trigger(MenuOrigin::Bar, cx))
                .child(div().w(px(1.)).h(px(20.)).mx_1().bg(rgb(surface.line)))
                .child(
                    widgets::icon_button("history-bar-delete", Icon::Trash, true, palette)
                        .on_click(cx.listener(|view, _, _, cx| view.history_delete(cx))),
                )
                .child(
                    widgets::icon_button("history-bar-clear", Icon::Close, true, palette).on_click(
                        cx.listener(|view, _, _, cx| {
                            let current = view.history.selected.clone();
                            view.history_set_single(current, cx);
                            cx.notify();
                        }),
                    ),
                )
                .into_any_element(),
        )
    }

    /// 多选时的详情栏：数量、选择顺序和前几项缩略图。
    pub(super) fn history_detail_multi(
        &self,
        pane: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let chosen = self.history_chosen();
        let selection = self.history_selection();
        let mut list = div().flex().flex_col().gap_2();
        for (index, item) in chosen.iter().take(8).enumerate() {
            let thumb = div()
                .w(px(64.))
                .h(px(40.))
                .flex_shrink_0()
                .rounded_md()
                .overflow_hidden()
                .bg(rgb(surface.track))
                .when_some(self.history.thumbs.get(&item.key), |slot, image| {
                    slot.child(img(image.clone()).size_full().object_fit(ObjectFit::Cover))
                });
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .child(thumb)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(format!("{}. {}", index + 1, item.display_title())),
                    ),
            );
        }
        if chosen.len() > 8 {
            list = list.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(crate::trf!("另有 {} 项", "{} more", chosen.len() - 8)),
            );
        }
        pane.pt_4()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(crate::trf!("已选 {} 项", "{} selected", chosen.len())),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(crate::trf!(
                        "图片 {} 张 · 录屏 {} 段。拼接与 PDF 按这里的顺序排列；动作在底部操作条。",
                        "{} images · {} recordings. Collage and PDF follow this order; actions are in the bar below.",
                        selection.images,
                        selection.recordings
                    )),
            )
            .child(list)
            .into_any_element()
    }
}
