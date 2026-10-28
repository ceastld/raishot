//! 截图与贴图共享的主工具栏设置；隐藏的工具仍可从“更多工具”使用。
use crate::{
    annotation::Tool,
    settings_view::SettingsView,
    settings_widgets::{self as widgets, ButtonKind},
    toolbar_config::{tool_id, ToolbarConfig, AVAILABLE_TOOLS},
    toolbar_groups::Slot,
    toolbar_icons::{self, Icon},
};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, SharedString};

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

    /// `width` 为设置正文列的实际宽度，用于预览条手动分行。
    pub(crate) fn toolbar_settings(&self, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
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
        // 预览与主栏同一组槽位：分组只显示首项图标并带下拉箭头。
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
        for slot in self.toolbar_config.slots() {
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
        // 不用 flex_wrap：最小内容宽度试算会把每个图标排成一行，卡片按该高度撑大，
        // 与滚动区测得的内容高度不一致，页面底部就滚不到。按正文宽度手动分行，
        // 扣除卡片、预览与预览条的内边距和边框共 44。
        let inner = (width - 44.).max(32.);
        let mut rows = vec![div().flex().items_center().gap_1()];
        let mut used = 0.;
        for (item_width, item) in items {
            if used > 0. && used + 4. + item_width > inner {
                rows.push(div().flex().items_center().gap_1());
                used = 0.;
            }
            used += if used > 0. { 4. } else { 0. } + item_width;
            let row = rows.pop().expect("至少一行");
            rows.push(row.child(item));
        }
        let strip = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_1()
            .rounded_lg()
            .border_1()
            .border_color(rgb(surface.line))
            .bg(rgb(surface.raised))
            .shadow_sm()
            .children(rows);
        let preview = div()
            .px_3()
            .py_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(strip)
            .child(
                div()
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(rgb(palette.muted))
                    .child(tr!("选区与对象编辑固定在最前；窄屏上按可用宽度收纳，溢出的工具仍在“更多工具”菜单中。数字 1–9 依次对应主栏上的工具按钮，0 回到鼠标。", "Selection and object editing always come first; on narrow screens tools collapse to fit, and overflow stays in the \"More tools\" menu. Keys 1–9 pick the toolbar buttons in order; 0 returns to the pointer.")),
            );
        let grouped = self.toolbar_config.grouped;
        let group_toggle = div()
            .id("toolbar-grouped")
            .relative()
            .h(px(52.))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded_md()
            .cursor_pointer()
            .hover(move |s| s.bg(rgb(palette.hover)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(tr!("合并同类工具", "Group similar tools"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(palette.muted))
                            .child(tr!(
                                "形状、画笔、文字、打码、橡皮擦各合为一个按钮，悬停展开；再按同一数字键轮换组内工具。",
                                "Shapes, pens, text, redaction and erasers each share one button that opens on hover; pressing its number again cycles through the group."
                            )),
                    ),
            )
            .child(widgets::switch(grouped, palette, surface))
            .child(self.settings_probe("toolbar-grouped"))
            .on_click(cx.listener(|v, _, _, cx| {
                v.change_toolbar(|config| config.grouped = !config.grouped);
                cx.notify();
            }));

        let enabled_len = self.toolbar_config.tools.len();
        let shown = self
            .toolbar_config
            .tools
            .iter()
            .enumerate()
            .filter_map(|(index, &tool)| self.toolbar_row(tool, Some((index, enabled_len)), cx))
            .collect::<Vec<_>>();
        let hidden = AVAILABLE_TOOLS
            .iter()
            .copied()
            .filter(|tool| !self.toolbar_config.tools.contains(tool))
            .filter_map(|tool| self.toolbar_row(tool, None, cx))
            .collect::<Vec<_>>();

        let mut page = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_6()
            .child(
                widgets::section(tr!("预览", "Preview"), palette).child(widgets::card(
                    surface,
                    [preview.into_any_element(), group_toggle.into_any_element()],
                )),
            )
            .child(
                widgets::section(trf!("显示在主栏（{}）", "Shown on the toolbar ({})", shown.len()), palette)
                    .child(widgets::card(surface, shown))
                    .child(widgets::note(
                        tr!("用箭头调整顺序。关闭只把工具移出主栏，仍可在“更多工具”菜单中选择。", "Use the arrows to reorder. Turning a tool off only removes it from the toolbar; it stays in the \"More tools\" menu."),
                        palette,
                    )),
            );
        if !hidden.is_empty() {
            page = page.child(
                widgets::section(trf!("已隐藏（{}）", "Hidden ({})", hidden.len()), palette)
                    .child(widgets::card(surface, hidden)),
            );
        }
        page.child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    widgets::button(
                        "toolbar-reset",
                        tr!("恢复默认工具栏", "Restore default toolbar"),
                        ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .child(self.settings_probe("toolbar-reset"))
                    .on_click(cx.listener(|v, _, _, cx| {
                        v.change_toolbar(|config| *config = ToolbarConfig::default());
                        cx.notify();
                    })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(px(12.))
                        .text_color(rgb(palette.muted))
                        .child(tr!("设置不会改变二级属性栏和截图输出操作。", "These settings don't change the property bar or the capture output actions.")),
                ),
        )
    }

    /// `order` 为主栏中的位置与主栏工具数；隐藏的工具不显示排序按钮。
    fn toolbar_row(
        &self,
        tool: Tool,
        order: Option<(usize, usize)>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = tool_id(tool)?;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let enabled = order.is_some();
        let toggle_id = format!("toolbar-toggle-{id}");
        let toggle = div()
            .id(SharedString::from(toggle_id.clone()))
            .relative()
            .flex_1()
            .min_w(px(0.))
            .h(px(44.))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .rounded_md()
            .cursor_pointer()
            .hover(move |s| s.bg(rgb(palette.hover)))
            .child(
                toolbar_icons::icon(
                    tool,
                    if enabled {
                        palette.foreground
                    } else {
                        palette.muted
                    },
                )
                .size(px(18.)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_color(rgb(if enabled {
                        palette.foreground
                    } else {
                        palette.muted
                    }))
                    .child(tool.label()),
            )
            .child(widgets::switch(enabled, palette, surface))
            .child(self.settings_probe(toggle_id))
            .on_click(cx.listener(move |v, _, _, cx| {
                v.change_toolbar(|config| {
                    if config.tools.contains(&tool) {
                        config.tools.retain(|candidate| *candidate != tool);
                    } else {
                        config.tools.push(tool);
                    }
                });
                cx.notify();
            }));
        let mut row = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap_1()
            .child(toggle);
        if let Some((index, len)) = order {
            let mut arrows = div().flex().items_center().gap(px(2.)).pr_1();
            for (direction, icon, permitted) in [
                ("up", Icon::ChevronUp, index > 0),
                ("down", Icon::Chevron, index + 1 < len),
            ] {
                let button_id = format!("toolbar-{direction}-{id}");
                let mut button = widgets::icon_button(
                    SharedString::from(button_id.clone()),
                    icon,
                    permitted,
                    palette,
                )
                .child(self.settings_probe(button_id));
                if permitted {
                    button = button.on_click(cx.listener(move |v, _, _, cx| {
                        v.change_toolbar(|config| {
                            let Some(index) =
                                config.tools.iter().position(|candidate| *candidate == tool)
                            else {
                                return;
                            };
                            let destination = if direction == "up" {
                                index.checked_sub(1)
                            } else {
                                index.checked_add(1)
                            };
                            if let Some(destination) =
                                destination.filter(|destination| *destination < config.tools.len())
                            {
                                config.tools.swap(index, destination);
                            }
                        });
                        cx.notify();
                    }));
                }
                arrows = arrows.child(button);
            }
            row = row.child(arrows);
        }
        Some(row.into_any_element())
    }
}
