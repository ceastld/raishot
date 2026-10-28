//! 主界面的工具弹层：图库里打开的工具在主窗口内全区弹出，不另起顶层窗口。
//! 同一时间只有一个工具；关闭走工具自己的流程（先取消后台任务），完成后回到原选区。
//! 多图工具的“添加”收起弹层回图库挑图，挑完带着新来源回来；单图工具在顶栏互切，
//! 共用已经解码的源图，不重读文件。
use super::{main_page::MainPage, titlebar, widgets, SettingsView};
use crate::{
    capture::Image,
    image_tool_view::{
        host::{HostedTool, ToolEvent, ToolHost},
        single::{SingleImage, TOOLS as SINGLE_TOOLS},
    },
    tool_registry::ToolId,
};
use gpui::{
    div, prelude::*, px, rgb, rgba, AnyElement, AnyView, App, Context, Entity, FocusHandle,
    FontWeight, KeyDownEvent, MouseButton, SharedString, Subscription, Window,
};
use std::{path::PathBuf, sync::Arc};

type Build = Box<dyn FnOnce(&mut Window, &mut Context<SettingsView>) -> crate::Result<Hosted>>;

/// 弹层里正在用的工具；按工具类型擦除，只留宿主需要的几个入口。
struct Hosted {
    tool: Option<ToolId>,
    title: SharedString,
    view: AnyView,
    working: Box<dyn Fn(&App) -> bool>,
    request_close: Box<dyn Fn(&mut App) -> bool>,
    sources: Box<dyn Fn(&App) -> Vec<PathBuf>>,
    set_sources: Box<dyn Fn(Vec<PathBuf>, &mut Window, &mut App)>,
    focus: Box<dyn Fn(&App) -> Option<FocusHandle>>,
    _subscription: Subscription,
}

/// 单图工具共用的已解码源图。
#[derive(Clone)]
struct SingleSource {
    image: Arc<Image>,
    path: PathBuf,
}

#[derive(Default)]
pub(super) struct ToolPopup {
    pending: Option<(Build, Option<SingleSource>)>,
    content: Option<Hosted>,
    single: Option<SingleSource>,
    focus: Option<FocusHandle>,
    return_focus: Option<FocusHandle>,
    origin_page: Option<MainPage>,
    /// 用户点了关闭但工具还在取消后台任务；归还后自动收起。
    closing: bool,
    /// 回图库挑图期间收起弹层，工具和它的参数原样保留。
    picking: bool,
    /// 挑图结束：`Some` 是新来源，`None` 是取消；下一帧交还焦点并应用。
    returned: Option<Option<Vec<PathBuf>>>,
    pub(super) dismiss_requested: bool,
}

impl ToolPopup {
    pub(super) fn is_open(&self) -> bool {
        self.pending.is_some() || self.content.is_some()
    }
}

impl SettingsView {
    /// 在下一帧建好工具视图并弹出；已有工具时先请求它关闭，正在处理就保留原工具并提示。
    pub(super) fn open_tool_popup<V: HostedTool>(
        &mut self,
        tool: Option<ToolId>,
        title: impl Into<SharedString>,
        build: impl FnOnce(&mut Window, &mut App) -> crate::Result<Entity<V>> + 'static,
        cx: &mut Context<Self>,
    ) {
        self.queue_tool_popup(tool, title.into(), build, None, cx);
    }

    /// 单图工具：源图已在图库后台解码，切换工具时直接复用。
    pub(super) fn open_single_tool_popup(
        &mut self,
        tool: ToolId,
        image: Arc<Image>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        use crate::image_tool_view::{barcode_view, measure_view, split_view};
        let single = Some(SingleSource {
            image: image.clone(),
            path: path.clone(),
        });
        let source = SingleImage::Shared(image);
        let title: SharedString = tool.title().into();
        match tool {
            ToolId::Split => self.queue_tool_popup(
                Some(tool),
                title,
                move |window, cx| {
                    split_view::embedded(source, Arc::new(vec![path]), ToolHost::Popup, window, cx)
                },
                single,
                cx,
            ),
            ToolId::Measure => self.queue_tool_popup(
                Some(tool),
                title,
                move |window, cx| Ok(measure_view::embedded(source, ToolHost::Popup, window, cx)),
                single,
                cx,
            ),
            _ => self.queue_tool_popup(
                Some(tool),
                title,
                move |window, cx| barcode_view::embedded(source, ToolHost::Popup, window, cx),
                single,
                cx,
            ),
        }
    }

    fn queue_tool_popup<V: HostedTool>(
        &mut self,
        tool: Option<ToolId>,
        title: SharedString,
        build: impl FnOnce(&mut Window, &mut App) -> crate::Result<Entity<V>> + 'static,
        single: Option<SingleSource>,
        cx: &mut Context<Self>,
    ) {
        self.media_popup.dismiss_requested = true;
        self.tool_popup.dismiss_requested = false;
        let build: Build = Box::new(move |window, cx| {
            let entity = build(window, cx)?;
            let subscription =
                cx.subscribe_in(&entity, window, |view, _, event: &ToolEvent, window, cx| {
                    match event {
                        ToolEvent::Close => view.drop_tool_popup(window, cx),
                        ToolEvent::PickSources => view.begin_tool_pick(window, cx),
                    }
                });
            let weak = entity.downgrade();
            let read = entity.downgrade();
            let sources = entity.downgrade();
            let set = entity.downgrade();
            let focus = entity.downgrade();
            Ok(Hosted {
                tool,
                title,
                view: entity.into(),
                working: Box::new(move |cx| {
                    read.upgrade().is_some_and(|view| view.read(cx).working())
                }),
                request_close: Box::new(move |cx| {
                    weak.update(cx, |view, cx| view.request_close(cx))
                        .unwrap_or(true)
                }),
                sources: Box::new(move |cx| {
                    sources
                        .upgrade()
                        .map(|view| view.read(cx).sources())
                        .unwrap_or_default()
                }),
                set_sources: Box::new(move |paths, window, cx| {
                    let _ = set.update(cx, |view, cx| view.set_sources(paths, window, cx));
                }),
                focus: Box::new(move |cx| focus.upgrade().map(|view| view.read(cx).focus_handle())),
                _subscription: subscription,
            })
        });
        self.tool_popup.pending = Some((build, single));
        cx.notify();
    }

    pub(super) fn reconcile_tool_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.tool_popup.dismiss_requested) {
            self.close_tool_popup(window, cx);
        }
        if let Some(returned) = self.tool_popup.returned.take() {
            if let Some(content) = &self.tool_popup.content {
                if let Some(paths) = returned {
                    (content.set_sources)(paths, window, cx);
                }
                if let Some(focus) = (content.focus)(cx) {
                    window.focus(&focus);
                }
            }
        }
        let Some((build, single)) = self.tool_popup.pending.take() else {
            return;
        };
        if let Some(content) = &self.tool_popup.content {
            if !(content.request_close)(cx) {
                self.status = crate::tr!(
                    "当前工具正在处理，完成后再打开新的工具",
                    "The current tool is still working; open another one when it finishes"
                )
                .into();
                self.tool_popup.closing = true;
                return;
            }
            self.tool_popup.content = None;
        } else {
            self.tool_popup.return_focus = if self.page == MainPage::History {
                self.history_popup_return_focus(window, cx)
            } else {
                window.focused(cx)
            };
            self.tool_popup.origin_page = Some(self.page);
        }
        self.history_clear_video_preview(cx);
        match build(window, cx) {
            Ok(content) => {
                self.tool_popup.content = Some(content);
                self.tool_popup.single = single;
                self.tool_popup.closing = false;
                // 工具视图构造时已经取得焦点；没有时由弹层接住按键。
                if window.focused(cx).is_none() {
                    let focus = cx.focus_handle();
                    window.focus(&focus);
                    self.tool_popup.focus = Some(focus);
                }
            }
            Err(error) => {
                self.status = error.to_string();
                self.tool_popup.single = None;
                self.restore_tool_focus(window, cx);
            }
        }
    }

    /// 关闭按钮、Esc 和切页：工具空闲就立即收起，否则等它取消完再收起。
    pub(super) fn close_tool_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tool_popup.pending = None;
        let Some(content) = &self.tool_popup.content else {
            return;
        };
        if (content.request_close)(cx) {
            self.drop_tool_popup(window, cx);
        } else {
            self.tool_popup.closing = true;
            cx.notify();
        }
    }

    fn drop_tool_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.tool_popup.picking) {
            self.history_pick_cancel(cx);
        }
        self.tool_popup.content = None;
        self.tool_popup.single = None;
        self.tool_popup.returned = None;
        self.tool_popup.closing = false;
        self.tool_popup.focus = None;
        self.restore_tool_focus(window, cx);
        cx.notify();
    }

    fn restore_tool_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.tool_popup.return_focus.take();
        if self.tool_popup.origin_page.take() == Some(self.page) {
            if let Some(focus) = focus {
                window.focus(&focus);
            }
        }
        let _ = cx;
    }

    /// 工具要更多来源：收起弹层，图库按当前来源进入该工具的选择模式。
    fn begin_tool_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(content) = &self.tool_popup.content else {
            return;
        };
        let Some(tool) = content.tool else {
            return;
        };
        let current = (content.sources)(cx);
        self.tool_popup.picking = true;
        if self.page != MainPage::History {
            self.show_page(MainPage::History, cx);
        }
        self.history_pick_for_popup(tool, current, cx);
        // 弹层收起后工具不在界面上，Enter／Esc 要交给图库的选择模式。
        window.focus(&self.history_focus_handle());
        cx.notify();
    }

    /// 图库选择模式结束：`Some` 带回按挑选顺序的来源，`None` 表示取消；弹层随即展开。
    pub(super) fn finish_tool_pick(&mut self, paths: Option<Vec<PathBuf>>, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.tool_popup.picking) {
            return;
        }
        self.tool_popup.returned = Some(paths);
        cx.notify();
    }

    pub(super) fn tool_popup_picking(&self) -> bool {
        self.tool_popup.picking
    }

    /// 单图工具之间互切：当前工具空闲才换，源图共用。
    fn switch_single_tool(&mut self, tool: ToolId, cx: &mut Context<Self>) {
        let (Some(content), Some(single)) = (&self.tool_popup.content, &self.tool_popup.single)
        else {
            return;
        };
        if content.tool == Some(tool) {
            return;
        }
        if (content.working)(cx) {
            self.status = crate::tr!(
                "当前工具正在处理，完成后再切换",
                "The current tool is still working; switch when it finishes"
            )
            .into();
            cx.notify();
            return;
        }
        let single = single.clone();
        self.open_single_tool_popup(tool, single.image, single.path, cx);
    }

    fn tool_popup_title(&self, content: &Hosted, cx: &mut Context<Self>) -> AnyElement {
        let switchable = self.tool_popup.single.is_some()
            && content
                .tool
                .is_some_and(|tool| SINGLE_TOOLS.contains(&tool));
        if !switchable {
            return div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(content.title.clone())
                .into_any_element();
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut group = widgets::segmented(surface);
        for (index, tool) in SINGLE_TOOLS.into_iter().enumerate() {
            let segment = widgets::segment(
                ("main-tool-popup-switch", index),
                tool.title(),
                content.tool == Some(tool),
                palette,
                surface,
            );
            group = group
                .child(segment.on_click(
                    cx.listener(move |view, _, _, cx| view.switch_single_tool(tool, cx)),
                ));
        }
        group.into_any_element()
    }

    pub(super) fn tool_popup_element(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.tool_popup.picking {
            return None;
        }
        let content = self.tool_popup.content.as_ref()?;
        let surface = widgets::surface(cx);
        let palette = crate::theme::palette(cx);
        let viewport = window.viewport_size();
        let inset = if f32::from(viewport.width) < 900. {
            8.
        } else {
            16.
        };
        let title = self.tool_popup_title(content, cx);
        Some(
            div()
                .id("main-tool-popup-backdrop")
                .absolute()
                .top(px(titlebar::HEIGHT))
                .bottom_0()
                .left_0()
                .right_0()
                .occlude()
                .bg(rgba(0x00000099))
                .p(px(inset))
                // 工具里常有未保存的参数，点遮罩不关闭，只吞掉事件。
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .id("main-tool-popup")
                        .when_some(self.tool_popup.focus.as_ref(), |popup, focus| {
                            popup.track_focus(focus)
                        })
                        .size_full()
                        .flex()
                        .flex_col()
                        .rounded_xl()
                        .border_1()
                        .border_color(rgb(surface.line))
                        .bg(rgb(surface.page))
                        .shadow_lg()
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                        .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" && !event.is_held {
                                view.close_tool_popup(window, cx);
                            }
                            cx.stop_propagation();
                        }))
                        .child(
                            div()
                                .h(px(48.))
                                .flex_shrink_0()
                                .px_3()
                                .flex()
                                .items_center()
                                .gap_2()
                                .border_b_1()
                                .border_color(rgb(surface.line))
                                .bg(rgb(surface.card))
                                .child(
                                    widgets::icon_button(
                                        "main-tool-popup-back",
                                        crate::toolbar_icons::Icon::ChevronLeft,
                                        true,
                                        palette,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, window, cx| view.close_tool_popup(window, cx),
                                    )),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(rgb(palette.muted))
                                        .child(crate::tr!("图库", "Gallery")),
                                )
                                .child(div().text_color(rgb(palette.border)).child("/"))
                                .child(title)
                                .child(div().flex_1())
                                .when(self.tool_popup.closing, |bar| {
                                    bar.child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(rgb(palette.muted))
                                            .child(crate::tr!(
                                                "正在取消，完成后关闭…",
                                                "Cancelling; closes when done…"
                                            )),
                                    )
                                })
                                .child(
                                    widgets::button(
                                        "main-tool-popup-close",
                                        crate::tr!("关闭 · Esc", "Close · Esc"),
                                        widgets::ButtonKind::Secondary,
                                        palette,
                                        surface,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, window, cx| view.close_tool_popup(window, cx),
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_h(px(0.))
                                .w_full()
                                .overflow_hidden()
                                .child(content.view.clone()),
                        ),
                )
                .into_any_element(),
        )
    }
}
