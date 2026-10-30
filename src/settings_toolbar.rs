//! 截图与贴图共享的主工具栏设置；按主栏上实际出现的按钮（槽位）编辑，隐藏的工具仍可从“更多工具”使用。
use crate::{
    annotation::Tool,
    settings_view::SettingsView,
    settings_widgets as widgets,
    toolbar_config::{tool_id, ToolbarConfig},
    toolbar_groups::{shortcut_number, Slot},
    toolbar_icons::{self, Icon},
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, Context, Div, FontWeight, SharedString, Window,
};

const ROW_GROUP: &str = "toolbar-slot-row";
const GRIP_GROUP: &str = "toolbar-slot-grip";

/// 拖动中的槽位：来自主栏行或“可添加”小块。
#[derive(Clone, Copy)]
struct SlotDrag(Slot);

/// 跟随鼠标的拖动预览：图标加名称。
struct SlotGhost(Slot);
impl Render for SlotGhost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 拖起后统一换成握拳光标（把手悬停是张开手，“可添加”小块是手形）；只在不同时设置以免反复刷新。
        let grabbing = crate::hand_cursor::closed();
        if cx.active_drag_cursor_style() != Some(grabbing) {
            cx.set_active_drag_cursor_style(grabbing, window);
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(32.))
            .px(px(10.))
            .rounded_md()
            .border_1()
            .border_color(rgb(palette.accent))
            .bg(rgb(surface.raised))
            .shadow_md()
            .text_size(px(13.))
            .text_color(rgb(palette.foreground))
            .child(toolbar_icons::icon(slot_icon(self.0), palette.foreground).size(px(16.)))
            .child(slot_label(self.0))
    }
}

/// 槽位的稳定标识，用于元素 ID 与界面探针。
fn slot_id(slot: Slot) -> &'static str {
    match slot {
        Slot::Tool(tool) => tool_id(tool).unwrap_or("tool"),
        Slot::Group(group) => group.id(),
    }
}

fn slot_label(slot: Slot) -> &'static str {
    match slot {
        Slot::Tool(tool) => tool.label(),
        Slot::Group(group) => group.label().get(),
    }
}

fn slot_icon(slot: Slot) -> Tool {
    match slot {
        Slot::Tool(tool) => tool,
        Slot::Group(group) => group.members()[0],
    }
}

/// 按可用宽度手动分行。不用 flex_wrap：最小内容宽度试算会把每项排成一行，
/// 卡片按该高度撑大，与滚动区测得的内容高度不一致，页面底部就滚不到。
fn wrap_rows(items: Vec<(f32, AnyElement)>, inner: f32, gap: f32) -> Vec<Div> {
    let row = || div().flex().items_center().gap(px(gap));
    let mut rows = vec![row()];
    let mut used = 0.;
    for (width, item) in items {
        if used > 0. && used + gap + width > inner {
            rows.push(row());
            used = 0.;
        }
        used += if used > 0. { gap } else { 0. } + width;
        let last = rows.pop().expect("至少一行");
        rows.push(last.child(item));
    }
    rows
}

impl SettingsView {
    fn change_toolbar(&mut self, change: impl FnOnce(&mut ToolbarConfig)) {
        let previous = self.toolbar_config.clone();
        change(&mut self.toolbar_config);
        self.toolbar_config = self.toolbar_config.normalized();
        self.status = match self.toolbar_config.save() {
            Ok(()) => tr!(
                "工具栏已保存。新的截图和重新打开的贴图工具栏使用同一配置。",
                "Toolbar saved. New captures and reopened pin toolbars use the same layout."
            )
            .into(),
            Err(error) => {
                self.toolbar_config = previous;
                trf!(
                    "工具栏未保存，保留原配置：{error}",
                    "Toolbar not saved; keeping the previous layout: {error}"
                )
            }
        };
    }

    /// `width` 为设置正文列的实际宽度，用于预览条与可添加列表手动分行。
    pub(crate) fn toolbar_settings(
        &self,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let slots = self.toolbar_config.slots();
        let grouped = self.toolbar_config.grouped;

        let shown = slots
            .iter()
            .enumerate()
            .map(|(index, &slot)| self.toolbar_slot_row(slot, index, cx))
            .collect::<Vec<_>>();
        let shown_card = if shown.is_empty() {
            widgets::card(
                surface,
                [div()
                    .px_3()
                    .py_4()
                    .text_size(px(13.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "主栏上没有绘制工具，全部收在“更多工具”菜单中。从下方添加。",
                        "No drawing tools on the toolbar; all of them are in the \"More tools\" menu. Add some below."
                    ))
                    .into_any_element()],
            )
        } else {
            widgets::card(surface, shown)
        };

        let mut page = div()
            .w_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_6()
            .child(
                widgets::section(tr!("预览", "Preview"), palette).child(widgets::card(
                    surface,
                    [
                        self.toolbar_preview(&slots, width, cx),
                        self.toolbar_layout_row(grouped, cx),
                    ],
                )),
            )
            .child(
                widgets::section(
                    trf!("主栏按钮（{}）", "Toolbar buttons ({})", slots.len()),
                    palette,
                )
                .child(shown_card)
                .child(widgets::note(
                    tr!(
                        "按住左侧把手拖动调整顺序。数字键 1–9 依次切换这些按钮，0 回到鼠标。选区与对象编辑固定在最前；窗口太窄时，放不下的按钮收进“更多工具”。",
                        "Drag the handle on the left to reorder. Keys 1–9 pick these buttons in order; 0 returns to the pointer. Selection and object editing always come first; buttons that don't fit move into \"More tools\"."
                    ),
                    palette,
                )),
            );

        let addable = self.toolbar_config.addable_slots();
        if !addable.is_empty() {
            let chips = addable
                .iter()
                .map(|&slot| self.toolbar_add_chip(slot, window, cx))
                .collect();
            // 卡片边框与内边距 10，内容区 px_3 左右 24。
            let rows = wrap_rows(chips, (width - 34.).max(80.), 8.);
            page = page.child(
                widgets::section(trf!("可添加（{}）", "Available ({})", addable.len()), palette)
                    .child(
                        widgets::card(
                            surface,
                            [div()
                                .px_3()
                                .py_3()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .children(rows)
                                .into_any_element()],
                        )
                        // 主栏行拖回这里即移出主栏。
                        .drag_over::<SlotDrag>(move |style, _, _, _| {
                            style.border_color(rgb(palette.accent))
                        })
                        .on_drop(cx.listener(|v, drag: &SlotDrag, _, cx| {
                            v.change_toolbar(|config| config.remove_slot(drag.0));
                            cx.notify();
                        })),
                    )
                    .child(widgets::note(
                        tr!(
                            "点击添加到末尾，或拖到上方列表中的位置；把主栏按钮拖回这里即可移除。没有添加的工具仍可从“更多工具”菜单使用。",
                            "Click to add at the end, or drag into place in the list above; drag a toolbar button back here to remove it. Tools not added stay available in the \"More tools\" menu."
                        ),
                        palette,
                    )),
            );
        }

        page
    }

    /// 页眉按钮宽度（两种文案取宽者），供标题行拖动层让位。
    pub(crate) fn toolbar_reset_width(window: &Window) -> f32 {
        widgets::header_button_width(
            window,
            &[
                tr!("恢复默认", "Restore defaults"),
                tr!("已是默认", "Using defaults"),
            ],
        )
    }

    /// 页眉右侧的“恢复默认”：不用滚到底就能找到；已是默认时变灰、不响应。
    pub(crate) fn toolbar_reset_button(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let changed = self.toolbar_config.normalized() != ToolbarConfig::default();
        let label = if changed {
            tr!("恢复默认", "Restore defaults")
        } else {
            tr!("已是默认", "Using defaults")
        };
        let button = widgets::header_button(
            "toolbar-reset",
            Icon::Refresh,
            label,
            width,
            changed,
            palette,
            surface,
        )
        .child(self.settings_probe("toolbar-reset"));
        if !changed {
            return button.into_any_element();
        }
        button
            .on_click(cx.listener(|v, _, _, cx| {
                v.change_toolbar(|config| *config = ToolbarConfig::default());
                // 保存失败时 change_toolbar 已回滚并写入错误，保留错误提示。
                if v.toolbar_config != ToolbarConfig::default() {
                    cx.notify();
                    return;
                }
                v.status = tr!(
                    "工具栏已恢复默认：按类别合并，矩形、箭头、画笔、文字、椭圆、马赛克、局部橡皮擦。",
                    "Toolbar restored to defaults: grouped, with rectangle, arrow, pen, text, ellipse, mosaic and pixel eraser."
                )
                .into();
                cx.notify();
            }))
            .into_any_element()
    }

    /// 与主栏同一组槽位：固定入口、分隔线，再是绘制按钮；分组带下拉箭头。
    fn toolbar_preview(&self, slots: &[Slot], width: f32, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let tile = |tool: Tool| {
            div()
                .size(px(32.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_md()
                .child(toolbar_icons::icon(tool, palette.foreground).size(px(18.)))
        };
        let mut items: Vec<(f32, AnyElement)> = vec![
            (32., tile(Tool::Crop).into_any_element()),
            (32., tile(Tool::Edit).into_any_element()),
            (
                9.,
                div()
                    .w(px(1.))
                    .h(px(20.))
                    .mx_1()
                    .bg(rgb(surface.line))
                    .into_any_element(),
            ),
        ];
        for &slot in slots {
            items.push(match slot {
                Slot::Tool(tool) => (32., tile(tool).into_any_element()),
                Slot::Group(group) => (
                    42.,
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .child(tile(group.members()[0]))
                        .child(toolbar_icons::action(Icon::Chevron, palette.muted).size(px(10.)))
                        .into_any_element(),
                ),
            });
        }
        // 扣除卡片、预览与预览条的内边距和边框共 44。
        let rows = wrap_rows(items, (width - 44.).max(32.), 4.);
        div()
            .px_3()
            .pt_4()
            .pb_3()
            .flex()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(surface.line))
                    .bg(rgb(surface.raised))
                    .shadow_sm()
                    .children(rows),
            )
            .into_any_element()
    }

    /// 分组方式只有两种，用分段控件直接对比，说明随当前选择变化。
    fn toolbar_layout_row(&self, grouped: bool, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let description = if grouped {
            tr!(
                "形状、线条、标记、文字、打码、橡皮擦各占一个按钮，悬停展开；再按同一数字键在组内轮换。",
                "Shapes, lines, markup, text, redaction and erasers each take one button that opens on hover; press its number again to cycle."
            )
        } else {
            tr!(
                "每个工具单独占一个按钮。",
                "Every tool gets its own button."
            )
        };
        let mut segments = widgets::segmented(surface);
        for (value, id, label) in [
            (true, "toolbar-grouped-on", tr!("同类合并", "Grouped")),
            (false, "toolbar-grouped-off", tr!("逐个显示", "Separate")),
        ] {
            segments = segments.child(
                widgets::segment(id, label, grouped == value, palette, surface)
                    .child(self.settings_probe(id))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if v.toolbar_config.grouped != value {
                            v.change_toolbar(|config| config.grouped = value);
                            cx.notify();
                        }
                    })),
            );
        }
        widgets::row(tr!("排列方式", "Layout"), Some(description.into()), palette)
            .child(segments)
            .into_any_element()
    }

    /// 主栏中的一个按钮：拖动把手、快捷数字、图标、名称（分组列出组内工具），右侧移除。
    fn toolbar_slot_row(&self, slot: Slot, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let id = slot_id(slot);
        let number = div()
            .size(px(22.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(palette.muted))
            .when_some(shortcut_number(index), |badge, number| {
                badge
                    .bg(rgb(surface.track))
                    .border_1()
                    .border_color(rgb(surface.line))
                    .child(number.to_string())
            });
        let mut text = div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(palette.foreground))
                    .child(slot_label(slot)),
            );
        if let Slot::Group(group) = slot {
            let members = group
                .options(false)
                .map(Tool::label)
                .collect::<Vec<_>>()
                .join(tr!("、", ", "));
            text = text.child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(rgb(palette.muted))
                    .child(members),
            );
        }
        let remove_id = format!("toolbar-remove-{id}");
        let remove = widgets::icon_button(
            SharedString::from(remove_id.clone()),
            Icon::Close,
            true,
            palette,
        )
        .child(self.settings_probe(remove_id))
        .on_click(cx.listener(move |v, _, _, cx| {
            v.change_toolbar(|config| config.remove_slot(slot));
            cx.notify();
        }));
        // 拖动只从把手开始：悬停张开手，拖动中握拳。行悬停时把手提亮，提示从这里拖。
        let handle = div()
            .id(SharedString::from(format!("toolbar-grip-{id}")))
            .w(px(24.))
            .h(px(36.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor(crate::hand_cursor::open())
            .group(GRIP_GROUP)
            .hover(move |style| style.bg(rgb(surface.track)))
            .child(
                // grip.svg 为 10×20，按原比例绘制，压成正方形会只剩几个小点。
                toolbar_icons::action(Icon::Grip, palette.muted)
                    .w(px(10.))
                    .h(px(20.))
                    .group_hover(ROW_GROUP, move |style| {
                        style.text_color(rgb(palette.foreground))
                    })
                    .group_hover(GRIP_GROUP, move |style| {
                        style.text_color(rgb(palette.accent))
                    }),
            )
            .on_drag(SlotDrag(slot), |drag: &SlotDrag, _, _, cx: &mut App| {
                cx.new(|_| SlotGhost(drag.0))
            });
        // 拖到另一行上即占据该行位置；放到“可添加”区域即移出主栏。
        div()
            .id(SharedString::from(format!("toolbar-slot-{id}")))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .min_h(px(52.))
            .pl_1()
            .pr_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .group(ROW_GROUP)
            .hover(move |style| style.bg(rgb(palette.hover)))
            .child(handle)
            .child(number)
            .child(toolbar_icons::icon(slot_icon(slot), palette.foreground).size(px(18.)))
            .child(text)
            .child(remove)
            .drag_over::<SlotDrag>(move |style, _, _, _| {
                style
                    .bg(rgb(palette.selected))
                    .border_color(rgb(palette.accent))
            })
            .on_drop(cx.listener(move |v, drag: &SlotDrag, _, cx| {
                if drag.0 != slot {
                    v.change_toolbar(|config| config.move_slot_to(drag.0, slot));
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    /// 可添加的按钮做成图标加名称的小块；返回实测宽度供手动分行。
    fn toolbar_add_chip(
        &self,
        slot: Slot,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (f32, AnyElement) {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let label = slot_label(slot);
        let chip_id = format!("toolbar-add-{}", slot_id(slot));
        // 左右内边距 10、图标 16、间距 6、边框 2。
        let width = widgets::text_width(window, label, 13., FontWeight::NORMAL) + 44.;
        let chip = div()
            .id(SharedString::from(chip_id.clone()))
            .relative()
            .h(px(32.))
            .px(px(10.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .rounded_md()
            .cursor_pointer()
            .border_1()
            .border_color(rgb(surface.line))
            .bg(rgb(surface.raised))
            .text_size(px(13.))
            .text_color(rgb(palette.foreground))
            .hover(move |style| {
                style
                    .bg(rgb(palette.hover))
                    .border_color(rgb(palette.accent))
            })
            .child(toolbar_icons::icon(slot_icon(slot), palette.foreground).size(px(16.)))
            .child(label)
            .child(self.settings_probe(chip_id))
            .on_click(cx.listener(move |v, _, _, cx| {
                v.change_toolbar(|config| config.add_slot(slot));
                cx.notify();
            }))
            .on_drag(SlotDrag(slot), |drag: &SlotDrag, _, _, cx: &mut App| {
                cx.new(|_| SlotGhost(drag.0))
            });
        (width, chip.into_any_element())
    }
}
