//! 选区左上角的尺寸/样式条；只渲染和发送宿主动作，不写入文档或输出像素。
//!
//! 布局：`W × H ⌄` │ `[原样][圆角][卡片][描边][窗口] 调整` │ 刷新 ⋯。样式缩略图单击即换、
//! 悬停只在选区上预览、滚轮逐个切换；“调整”只打开当前样式的参数。空间紧张时样式收成
//! 一个“当前样式 ⌄”按钮，单击打开画廊。坐标与导出尺寸只放在悬停提示和面板里。
#[path = "capture_selection_bar_layout.rs"]
mod layout;
pub(crate) use layout::{SelectionBarDisplay, SelectionBarLayout, SelectionBarWidths};

use crate::{
    capture_toolbar::Palette,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    selection_finish::FinishPreset,
    selection_finish_view::{look_id, FinishSection},
    toolbar_icons::{self, Icon},
    toolbar_layout::ToolbarPanel,
};
use gpui::{div, prelude::*, px, rgb, Context, SharedString, Window};

/// 条内图标按钮与间距；宽度估算和渲染共用，避免两处数值漂移。
const BUTTON: f32 = 28.;
const GAP: f32 = 2.;
const SEPARATOR: f32 = 1. + 6.;
const ICON: f32 = 16.;
const SMALL_ICON: f32 = 12.;
/// 样式缩略图底槽宽 24，按钮左右各留 3。
const THUMB: f32 = 24.;
const LOOK: f32 = THUMB + 6.;

struct SelectionBarHint(SharedString);
impl gpui::Render for SelectionBarHint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::capture_toolbar::ui_kit::tip_body(crate::theme::palette(cx), &self.0)
    }
}

struct Labels {
    dimensions: String,
    coordinates: String,
    output: String,
}

fn text_width(window: &Window, text: &str) -> f32 {
    let line = window.text_system().shape_line(
        text.to_owned().into(),
        px(12.),
        &[gpui::TextRun {
            len: text.len(),
            font: window.text_style().font(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    f32::from(line.width).ceil() + 1.
}

fn separator(p: Palette) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(1.))
        .h(px(16.))
        .mx(px(3.))
        .bg(rgb(p.border))
}

impl CaptureView {
    fn selection_bar_labels(&self, selected: Rect) -> Labels {
        let output = self
            .effective_finish_style()
            .layout(selected.width, selected.height)
            .ok()
            .filter(|layout| {
                layout.output_width != selected.width || layout.output_height != selected.height
            })
            .map_or_else(String::new, |layout| {
                format!("{} × {}", layout.output_width, layout.output_height)
            });
        Labels {
            dimensions: format!("{} × {}", selected.width, selected.height),
            coordinates: format!(
                "({}, {})",
                self.source.bounds.left + selected.x as i32,
                self.source.bounds.top + selected.y as i32
            ),
            output,
        }
    }

    pub(crate) fn selection_bar_widths(
        &self,
        selected: Rect,
        window: &Window,
    ) -> SelectionBarWidths {
        let labels = self.selection_bar_labels(selected);
        // 面板左右 padding 3 + 边框 1；样式按钮 padding 12，子项（空标签、缩略图、
        // 样式名、箭头）之间 gap 5。另留 6px 余量，宁可略宽也不能挤压内容。
        let frame = 2. * (3. + 1.) + 6.;
        let size = self.selection_size_chip_width(&labels, window);
        let refresh = if self.selection_bar_refresh() {
            BUTTON + GAP
        } else {
            0.
        };
        let fixed = frame + size + 2. * SEPARATOR + BUTTON + 4. * GAP + refresh;
        // 收起按钮按最长的样式名取宽，换样式时条不跟着伸缩。
        let name = FinishPreset::ALL
            .into_iter()
            .map(|look| text_width(window, look.label()))
            .fold(0., f32::max);
        let compact = fixed + 12. + THUMB + name + SMALL_ICON + 3. * 5.;
        let looks = FinishPreset::ALL.len() as f32;
        let details = fixed + looks * LOOK + looks * GAP + BUTTON;
        SelectionBarWidths {
            details,
            dimensions: details,
            compact,
        }
    }

    fn selection_size_chip_width(&self, labels: &Labels, window: &Window) -> f32 {
        text_width(window, &labels.dimensions) + 14. + SMALL_ICON + 4.
    }

    /// 样式组相对条左缘的 x；外观面板左缘对齐这里。
    pub(crate) fn selection_bar_style_x(&self, selected: Rect, window: &Window) -> f32 {
        let labels = self.selection_bar_labels(selected);
        // 边框 1 + padding 3，尺寸胶囊，分隔线两侧各一个 gap。
        4. + self.selection_size_chip_width(&labels, window) + 2. * GAP + SEPARATOR
    }

    /// 打开外观面板的某种内容；同一种已打开时关闭，另一种已打开时直接切换。
    fn toggle_finish_section(
        &mut self,
        section: FinishSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.toolbar_panel == ToolbarPanel::Finish {
            if self.finish_section == section {
                self.perform(Action::Panel(ToolbarPanel::None), window, cx);
            } else {
                self.finish_section = section;
                cx.notify();
            }
        } else {
            self.perform(Action::Panel(ToolbarPanel::Finish), window, cx);
            self.finish_section = section;
        }
    }

    /// 截图时条右侧带刷新；图片编辑和贴图没有可刷新的冻结桌面。
    fn selection_bar_refresh(&self) -> bool {
        self.image_canvas.is_none() && !self.pin_mode
    }

    pub(crate) fn selection_bar(
        &self,
        selected: Rect,
        layout: SelectionBarLayout,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let labels = self.selection_bar_labels(selected);
        let p = Palette::new(self.light_theme);
        let hint = |text: String| {
            move |_: &mut Window, cx: &mut gpui::App| -> gpui::AnyView {
                let text = text.clone();
                cx.new(|_| SelectionBarHint(text.into())).into()
            }
        };
        let size = self
            .chip(
                "selection-size-menu",
                labels.dimensions.clone(),
                self.toolbar_panel == ToolbarPanel::SelectionSize,
            )
            .px(px(7.))
            .gap(px(4.))
            .child(toolbar_icons::action(Icon::Chevron, p.muted).size(px(SMALL_ICON)))
            .tooltip(hint(trf!(
                "{} · {} · 点击输入精确位置和尺寸",
                "{} · {} · Click to enter exact position and size",
                labels.coordinates,
                labels.dimensions
            )))
            .on_click(cx.listener(|view, _, window, cx| {
                view.perform(Action::Panel(ToolbarPanel::SelectionSize), window, cx);
            }));
        let more_active = self.toolbar_panel.more()
            || !matches!(
                self.editor.tool,
                crate::annotation::Tool::Crop | crate::annotation::Tool::Edit
            ) && !self
                .toolbar_config
                .slots()
                .iter()
                .any(|slot| slot.contains(self.editor.tool));
        let current = self.finish_preset;
        let export = if labels.output.is_empty() {
            String::new()
        } else {
            trf!(" · 导出 {}", " · exports {}", labels.output)
        };
        let available = self.finish_form_available();
        let panel_open = self.toolbar_panel == ToolbarPanel::Finish;
        let style = if layout.display == SelectionBarDisplay::Compact {
            // 空间紧张只留当前样式一个按钮：单击打开画廊，滚轮仍逐个切换。
            let open = panel_open && self.finish_section == FinishSection::Gallery;
            div().child(
                self.chip_enabled("selection-style-menu", "", open, available)
                    .px(px(6.))
                    .gap(px(5.))
                    .child(self.finish_thumb(current, false))
                    .child(current.label())
                    .child(toolbar_icons::action(Icon::Chevron, p.muted).size(px(SMALL_ICON)))
                    .tooltip(hint(trf!(
                        "外观：{}{} · 单击选择样式，滚轮切换",
                        "Style: {}{} · click to choose, wheel to switch",
                        current.label(),
                        export
                    )))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.toggle_finish_section(FinishSection::Gallery, window, cx);
                    })),
            )
        } else {
            // 五个样式缩略图：单击即换，悬停在选区上预览；右侧“调整”只调当前样式。
            let mut group = div().flex().items_center().gap(px(GAP));
            for look in FinishPreset::ALL {
                let on = look == current;
                let id = look_id(look, false);
                let tip = if on {
                    trf!("{}（当前）{}", "{} (current){}", look.label(), export)
                } else {
                    trf!(
                        "{} · 单击应用，悬停预览",
                        "{} · click to apply, hover to preview",
                        look.label()
                    )
                };
                group = group.child(
                    div()
                        .id(id)
                        .relative()
                        .flex_shrink_0()
                        .w(px(LOOK))
                        .h(px(BUTTON))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .when(on, |d| {
                            d.bg(rgb(p.selected)).border_1().border_color(rgb(p.accent))
                        })
                        .when(!available, |d| d.opacity(0.4))
                        .when(available && !on, |d| {
                            d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
                        })
                        .child(self.finish_thumb(look, false))
                        .tooltip(hint(tip))
                        .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
                            view.hover_finish_look(look, *hovered, cx);
                        }))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.select_finish_look(look, cx);
                        }))
                        .child(self.probe(id)),
                );
            }
            let open = panel_open && self.finish_section == FinishSection::Tune;
            group.child(
                self.chip_enabled("selection-style-tune", "", open, available)
                    .w(px(BUTTON))
                    .child(toolbar_icons::action(Icon::Sliders, p.foreground).size(px(ICON)))
                    .tooltip(hint(trf!("调整“{}”", "Adjust {}", current.label())))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.toggle_finish_section(FinishSection::Tune, window, cx);
                    })),
            )
        }
        .id("selection-style-group")
        // 样式组上滚轮逐个切换样式，也不能落到画布缩放。
        .on_scroll_wheel(cx.listener(|view, event, _, cx| view.finish_look_wheel(event, cx)));
        self.panel("selection-bar", layout.panel)
            .px(px(3.))
            .flex()
            .items_center()
            .gap(px(GAP))
            .whitespace_nowrap()
            .child(size)
            .child(separator(p))
            .child(style)
            .child(separator(p))
            .when(self.selection_bar_refresh(), |row| {
                let enabled = self.can_refresh_capture();
                row.child(
                    self.chip_enabled("selection-bar-refresh", "", false, enabled)
                        .w(px(BUTTON))
                        .child(toolbar_icons::action(Icon::Refresh, p.foreground).size(px(ICON)))
                        .tooltip(hint(
                            tr!(
                                "刷新截图 · F5 · 保留选区和标注",
                                "Refresh · F5 · keep selection and annotations"
                            )
                            .into(),
                        ))
                        .on_click(cx.listener(|view, _, window, cx| {
                            if view.can_refresh_capture() {
                                view.perform(Action::Refresh, window, cx);
                            }
                        })),
                )
            })
            .child(
                self.chip("selection-bar-more", "", more_active)
                    .w(px(BUTTON))
                    .child(toolbar_icons::action(Icon::More, p.foreground).size(px(ICON)))
                    .tooltip(hint(
                        tr!("全部工具与更多操作", "All tools and more actions").into(),
                    ))
                    .on_click(cx.listener(|view, _, window, cx| {
                        let panel = if view.toolbar_panel.more() {
                            ToolbarPanel::None
                        } else {
                            ToolbarPanel::Actions
                        };
                        view.perform(Action::Panel(panel), window, cx);
                    })),
            )
    }
}
