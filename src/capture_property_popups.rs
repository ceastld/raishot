//! 二级栏收起后的弹层（颜色、单选菜单、样式）与对象弹层（角度、排列）。
//! 弹层圆角 10、内边距 12、行距 10；菜单内边距 4、行高 32。
use crate::{
    annotation::{HighlighterBlend, HighlighterShape, Style, Tool},
    arrow_geometry::ArrowPreset,
    capture_toolbar::Palette,
    capture_view::{Action, CaptureView},
    property_items::{self as items, PropertyContext},
    toolbar_icons::Icon,
    toolbar_layout::{ChoiceKind, ToolbarLayout, ToolbarPanel},
    toolbar_widgets::{self as w, Lead, State},
};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, SharedString};

/// 弹层里的快选按钮：高 28，track 底，选中 selected + accent。
fn preset_button(
    id: &'static str,
    on: bool,
    enabled: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .flex_1()
        .min_w(px(0.))
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(4.))
        .rounded(px(6.))
        .bg(rgb(if on { p.selected } else { p.track }))
        .text_color(rgb(if on { p.accent } else { p.foreground }))
        .when(enabled && !on, |d| {
            d.cursor_pointer().hover(move |s| s.bg(rgb(p.track_hover)))
        })
        .when(!enabled, |d| d.opacity(0.4))
}

/// 排列菜单：9 行、2 条分隔线，进入组合后多一行层级说明。
fn arrange_height(depth: usize) -> f32 {
    8. + if depth > 0 { 24. } else { 0. } + 9. * 33. + 2. * 8.
}

impl CaptureView {
    fn popup_tool(&self) -> Tool {
        self.editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool)
    }
    /// 颜色弹层只在第 3 级（不透明度从栏上收起）时并入不透明度。
    fn colors_with_opacity(&self) -> bool {
        self.property_level.get() >= items::MAX_LEVEL && self.popup_tool() != Tool::Spotlight
    }
    /// 二级栏弹层的外框尺寸（含内边距）：截图按锚点放置，贴图小窗按它定窗口大小。
    pub(crate) fn property_popup_size(&self, panel: ToolbarPanel) -> Option<(f32, f32)> {
        Some(match panel {
            ToolbarPanel::Colors => (
                236.,
                if self.colors_with_opacity() {
                    198.
                } else {
                    160.
                },
            ),
            ToolbarPanel::Choice(_) => (164., 2. * 33. + 8.),
            ToolbarPanel::ToolStyle => {
                let rows = items::style_rows(&self.property_context(self.popup_tool())).len();
                (316., rows.max(1) as f32 * 38. - 10. + 24.)
            }
            ToolbarPanel::Angle => (
                236.,
                if self.editor.tool == Tool::Watermark {
                    136.
                } else {
                    170.
                },
            ),
            ToolbarPanel::Arrange => (240., arrange_height(self.editor.group_scope_depth())),
            ToolbarPanel::TextOutlineStyle => (252., 24. + 32. + 10. + 28. + 38.),
            ToolbarPanel::TextBackgroundStyle => (252., 24. + 32. + 10. + 28. + 2. * 38.),
            ToolbarPanel::TextFormat => (236., 116.),
            ToolbarPanel::FontFamily => (240., 320.),
            ToolbarPanel::Arrow => (196., ArrowPreset::ALL.len() as f32 * 33. + 8.),
            ToolbarPanel::CustomColor(_) => (
                crate::capture_color_dialog::PICKER_WIDTH + 24.,
                crate::capture_color_dialog::PICKER_HEIGHT + 24.,
            ),
            _ => return None,
        })
    }
    /// 按弹层尺寸放在锚点旁；圆角 10。
    pub(crate) fn popup_panel(
        &self,
        panel: ToolbarPanel,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
    ) -> gpui::Stateful<gpui::Div> {
        let (width, height) = self.property_popup_size(panel).unwrap_or((236., 120.));
        self.panel("toolbar-popup", layout.popup(width, height, anchor, true))
            .rounded(px(10.))
    }

    /// 第 2 级起的颜色弹层：10 色、自定义；第 3 级把不透明度并进来。
    pub(crate) fn colors_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let current = self.editor.style.color & 0xff_ffff;
        let tool = self.popup_tool();
        let with_opacity = self.colors_with_opacity();
        let mut grid = div().flex().flex_col().gap(px(2.));
        for (row, chunk) in crate::capture_color_dialog::PRESETS.chunks(5).enumerate() {
            let mut line = div().flex().justify_between();
            for (column, &color) in chunk.iter().enumerate() {
                line = line.child(
                    w::swatch(
                        ("color-preset", row * 5 + column),
                        color,
                        color == current,
                        p,
                    )
                    .tooltip(w::tip(format!("#{color:06X}"), "", ""))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.editor.set_color(color);
                        cx.notify();
                    })),
                );
            }
            grid = grid.child(line);
        }
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .min_h(px(26.))
                    .child(w::popup_title(tr!("颜色", "Color"), p))
                    .child(
                        div()
                            .text_color(rgb(p.muted))
                            .font_family("Cascadia Mono")
                            .text_size(px(11.5))
                            .child(format!("#{current:06X}")),
                    ),
            )
            .child(grid)
            .child(
                w::menu_row(
                    "custom-color",
                    Some(w::color_wheel(16.).into_any_element()),
                    tr!("自定义颜色…", "Custom color…"),
                    None,
                    State::default(),
                    p,
                )
                .on_click(cx.listener(|v, _, w, cx| v.perform(Action::PickColor, w, cx)))
                .child(self.probe("custom-color")),
            );
        if with_opacity {
            body = body.child(w::labeled_row(
                tr!("不透明度", "Opacity"),
                self.property_item(items::Item::Opacity, &self.property_context(tool), cx),
                p,
            ));
        }
        self.popup_panel(ToolbarPanel::Colors, layout, anchor)
            .p(px(12.))
            .child(body)
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }

    /// 分段收起后的单选菜单。
    pub(crate) fn choice_popup(
        &self,
        kind: ChoiceKind,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let style = self.editor.style;
        type Update = fn(&mut CaptureView);
        let options: Vec<(&'static str, Icon, &'static str, bool, Update)> = match kind {
            ChoiceKind::LineStyle => vec![
                (
                    "choice-line-solid",
                    Icon::LineSolid,
                    tr!("实线", "Solid"),
                    !style.dashed,
                    |v| set_style(v, |s| s.dashed = false),
                ),
                (
                    "choice-dashed",
                    Icon::LineDash,
                    tr!("虚线", "Dashed"),
                    style.dashed,
                    |v| set_style(v, |s| s.dashed = true),
                ),
            ],
            ChoiceKind::Fill => vec![
                (
                    "choice-fill-stroke",
                    Icon::FillStroke,
                    tr!("描边", "Outline"),
                    !style.filled,
                    |v| set_style(v, |s| s.filled = false),
                ),
                (
                    "choice-filled",
                    Icon::FillSolid,
                    tr!("填充", "Fill"),
                    style.filled,
                    |v| set_style(v, |s| s.filled = true),
                ),
            ],
            ChoiceKind::Blend => vec![
                (
                    "choice-blend-multiply",
                    Icon::BlendMultiply,
                    tr!("正片叠底", "Multiply"),
                    style.highlighter_blend == HighlighterBlend::Multiply,
                    |v| set_blend(v, HighlighterBlend::Multiply),
                ),
                (
                    "choice-blend-alpha",
                    Icon::BlendAlpha,
                    tr!("透明叠加", "Overlay"),
                    style.highlighter_blend == HighlighterBlend::Alpha,
                    |v| set_blend(v, HighlighterBlend::Alpha),
                ),
            ],
            ChoiceKind::Stroke => {
                let shape = self.editor.highlighter_shape();
                vec![
                    (
                        "choice-stroke-freehand",
                        Icon::Smooth,
                        tr!("手绘", "Freehand"),
                        shape == HighlighterShape::Freehand,
                        |v| v.editor.set_highlighter_shape(HighlighterShape::Freehand),
                    ),
                    (
                        "choice-stroke-line",
                        Icon::Line,
                        tr!("拉直", "Straight"),
                        shape == HighlighterShape::Line,
                        |v| v.editor.set_highlighter_shape(HighlighterShape::Line),
                    ),
                ]
            }
        };
        let mut menu = div().flex().flex_col().gap(px(1.)).text_size(px(12.5));
        for (id, icon, label, on, update) in options {
            let color = if on { p.accent } else { p.foreground };
            menu = menu.child(
                w::menu_row(
                    id,
                    Some(w::menu_icon(icon, color)),
                    label,
                    None,
                    State::on(on),
                    p,
                )
                .on_click(cx.listener(move |v, _, _, cx| {
                    update(v);
                    v.toolbar_panel = ToolbarPanel::None;
                    cx.notify();
                }))
                .child(self.probe(id)),
            );
        }
        self.popup_panel(ToolbarPanel::Choice(kind), layout, anchor)
            .p(px(4.))
            .child(menu)
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }

    /// 第 3 级的“样式”弹层：逐行标签 + 展开形态的专属控件。
    pub(crate) fn tool_style_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let ctx: PropertyContext = self.property_context(self.popup_tool());
        let rows = items::style_rows(&ctx);
        let mut body = div().flex().flex_col().gap(px(10.)).text_size(px(12.));
        for item in &rows {
            body = body.child(w::labeled_row(
                crate::capture_properties::item_label(*item),
                self.property_item(*item, &ctx, cx),
                p,
            ));
        }
        self.popup_panel(ToolbarPanel::ToolStyle, layout, anchor)
            .p(px(12.))
            .child(body)
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }

    /// 角度弹层：当前值、常用角度、±15° 与 90°；水印用自己的倾斜角范围。
    pub(crate) fn angle_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        // 水印工具下角度作用于文档里唯一的水印，否则作用于选中对象。
        let watermark = self.editor.tool == Tool::Watermark;
        let angle = if watermark {
            self.editor.watermark_rotation()
        } else {
            (!self.editor.selected_ids().is_empty())
                .then(|| self.editor.selected_annotation().map_or(0., |a| a.rotation))
        };
        let enabled = angle.is_some();
        let current = angle.unwrap_or(0.);
        let presets: [(&'static str, f32); 5] = if watermark {
            [
                ("angle-preset-0", -45.),
                ("angle-preset-1", -30.),
                ("reset-rotation", 0.),
                ("angle-preset-3", 30.),
                ("angle-preset-4", 45.),
            ]
        } else {
            [
                ("angle-preset-0", -90.),
                ("angle-preset-1", -45.),
                ("reset-rotation", 0.),
                ("angle-preset-3", 45.),
                ("angle-preset-4", 90.),
            ]
        };
        let mut absolute = div().flex().gap(px(4.));
        for (id, value) in presets {
            let on = enabled && (current - value).abs() < 0.5;
            absolute = absolute.child(
                preset_button(id, on, enabled, p)
                    .child(format!("{value:.0}°"))
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if enabled {
                            let action = if value == 0. {
                                Action::ResetRotation
                            } else {
                                Action::Rotate(value - current)
                            };
                            v.perform(action, w, cx);
                        }
                    }))
                    .child(self.probe(id)),
            );
        }
        let mut relative = div().flex().gap(px(4.));
        for (id, label, delta, icon) in [
            ("rotate-left", "15°", -15., Some(Icon::RotateLeft)),
            ("rotate-right", "15°", 15., Some(Icon::RotateRight)),
            ("rotate-90", "90°", 90., None),
        ] {
            let mut button = preset_button(id, false, enabled, p);
            if let Some(icon) = icon {
                button = button.child(w::menu_icon(icon, p.foreground));
            }
            relative = relative.child(
                button
                    .child(label)
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if enabled {
                            v.perform(Action::Rotate(delta), w, cx);
                        }
                    }))
                    .child(self.probe(id)),
            );
        }
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .min_h(px(26.))
                    .child(w::popup_title(
                        if watermark {
                            tr!("水印角度", "Watermark angle")
                        } else {
                            tr!("角度", "Angle")
                        },
                        p,
                    ))
                    .child(format!("{current:.0}°")),
            )
            .child(absolute)
            .child(relative)
            .when(!watermark, |body| {
                body.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(p.muted))
                        .child(tr!(
                            "画布上拖旋转柄更快，按住 Shift 吸附 15°。",
                            "Drag the rotate handle on canvas; hold Shift to snap 15°."
                        )),
                )
            });
        self.popup_panel(ToolbarPanel::Angle, layout, anchor)
            .p(px(12.))
            .child(body)
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }

    /// 排列与组合菜单：图标 + 名称 + 快捷键，删除用 danger。
    pub(crate) fn arrange_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let ids = self.editor.selected_ids();
        let depth = self.editor.group_scope_depth();
        let any = !ids.is_empty();
        type Row = (
            &'static str,
            Icon,
            &'static str,
            &'static str,
            Action,
            bool,
            bool,
        );
        let groups: [&[Row]; 3] = [
            &[
                (
                    "group-selection",
                    Icon::Group,
                    tr!("组合", "Group"),
                    "Ctrl+G",
                    Action::Group,
                    self.editor.can_group_selection(),
                    false,
                ),
                (
                    "ungroup-selection",
                    Icon::Ungroup,
                    tr!("拆分", "Ungroup"),
                    "Ctrl+Shift+G",
                    Action::Ungroup,
                    ids.iter()
                        .any(|id| self.editor.grouped_in_current_scope(*id)),
                    false,
                ),
                (
                    "enter-group",
                    Icon::EnterGroup,
                    tr!("进入组合", "Enter group"),
                    "",
                    Action::EnterGroup,
                    self.editor.can_enter_group_scope(),
                    false,
                ),
                (
                    "exit-group",
                    Icon::ParentGroup,
                    tr!("返回上层", "Parent group"),
                    "Esc",
                    Action::ExitGroup,
                    depth > 0,
                    false,
                ),
                (
                    "root-group",
                    Icon::ParentGroup,
                    tr!("最外层", "Root level"),
                    "",
                    Action::RootGroup,
                    depth > 0,
                    false,
                ),
            ],
            &[
                (
                    "front-selection",
                    Icon::BringFront,
                    tr!("置顶层", "Bring to front"),
                    "Ctrl+]",
                    Action::Front,
                    any,
                    false,
                ),
                (
                    "back-selection",
                    Icon::SendBack,
                    tr!("置底层", "Send to back"),
                    "Ctrl+[",
                    Action::Back,
                    any,
                    false,
                ),
            ],
            &[
                (
                    "duplicate-selection",
                    Icon::Duplicate,
                    tr!("复制对象", "Duplicate"),
                    "Ctrl+D",
                    Action::Duplicate,
                    any,
                    false,
                ),
                (
                    "delete-selection",
                    Icon::Trash,
                    tr!("删除", "Delete"),
                    "Del",
                    Action::Delete,
                    any,
                    true,
                ),
            ],
        ];
        let mut menu = div().flex().flex_col().gap(px(1.)).text_size(px(12.5));
        if depth > 0 {
            menu = menu.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(11.5))
                    .text_color(rgb(p.muted))
                    .child(SharedString::from(trf!(
                        "组合 · 第 {depth} 层",
                        "Group · level {depth}"
                    ))),
            );
        }
        for (index, group) in groups.iter().enumerate() {
            if index > 0 {
                menu = menu.child(w::menu_divider(p));
            }
            for &(id, icon, label, key, action, enabled, danger) in group.iter() {
                let color = if danger { p.danger } else { p.foreground };
                menu = menu.child(
                    w::menu_row(
                        id,
                        Some(w::menu_icon(icon, color)),
                        label,
                        (!key.is_empty()).then(|| w::menu_key(key, p)),
                        State {
                            danger,
                            ..State::default().disabled(!enabled)
                        },
                        p,
                    )
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if enabled {
                            v.toolbar_panel = ToolbarPanel::None;
                            v.perform(action, w, cx);
                        }
                    }))
                    .child(self.probe(id)),
                );
            }
        }
        self.popup_panel(ToolbarPanel::Arrange, layout, anchor)
            .p(px(4.))
            .child(menu)
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }

    /// 文字描边、底色细项：开关、颜色、粗细（描边）或不透明度与圆角（底色）。
    pub(crate) fn text_detail_popup(
        &self,
        outline: bool,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let t = self.editor.style.text;
        let on = if outline { t.outline } else { t.background };
        let toggle = w::menu_row(
            if outline {
                "text-outline-switch"
            } else {
                "text-background-switch"
            },
            None,
            if outline {
                tr!("描边", "Outline")
            } else {
                tr!("底色", "Background")
            },
            Some(w::switch(on, p).into_any_element()),
            State::default(),
            p,
        )
        .on_click(cx.listener(move |v, _, _, cx| {
            let mut s = v.editor.style;
            if outline {
                s.text.outline = !s.text.outline;
            } else {
                s.text.background = !s.text.background;
            }
            v.editor.set_style(s);
            cx.notify();
        }));
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .text_size(px(12.))
            .child(toggle)
            .child(w::labeled_row(
                tr!("颜色", "Color"),
                self.text_colors(outline, cx),
                p,
            ));
        let field = |id: &'static str, lead: Lead, value: String, reserve: f32, panel, title| {
            (id, lead, value, reserve, panel, title)
        };
        let fields = if outline {
            vec![field(
                "text-outline-width",
                Lead::Dot(t.outline_width),
                crate::capture_properties::value_label(t.outline_width),
                crate::property_items::value_text_width(&["99", "9.5"]),
                ToolbarPanel::TextOutline,
                tr!("粗细", "Width"),
            )]
        } else {
            vec![
                field(
                    "text-background-alpha",
                    Lead::Icon(Icon::Opacity),
                    format!("{:.0}%", t.background_opacity * 100.),
                    crate::property_items::percent_text_width(),
                    ToolbarPanel::TextBackgroundAlpha,
                    tr!("不透明度", "Opacity"),
                ),
                field(
                    "text-background-radius",
                    Lead::Icon(Icon::Radius),
                    crate::capture_properties::value_label(t.background_radius),
                    crate::property_items::value_text_width(&["99", "9.5"]),
                    ToolbarPanel::TextBackgroundRadius,
                    tr!("圆角", "Radius"),
                ),
            ]
        };
        for (id, lead, value, reserve, panel, title) in fields {
            body = body.child(w::labeled_row(
                title,
                self.text_value_field(id, lead, value, reserve, panel, cx),
                p,
            ));
        }
        let panel = if outline {
            ToolbarPanel::TextOutlineStyle
        } else {
            ToolbarPanel::TextBackgroundStyle
        };
        self.popup_panel(panel, layout, anchor)
            .p(px(12.))
            .child(body)
            .child(self.probe("toolbar-popup"))
            .into_any_element()
    }
}

fn set_style(v: &mut CaptureView, update: impl FnOnce(&mut Style)) {
    let mut s = v.editor.style;
    update(&mut s);
    if s != v.editor.style {
        v.editor.set_style(s);
    }
}

fn set_blend(v: &mut CaptureView, blend: HighlighterBlend) {
    set_style(v, |s| {
        if s.highlighter_blend != blend {
            s.highlighter_blend = blend;
            s.opacity = match blend {
                HighlighterBlend::Alpha => 96. / 255.,
                HighlighterBlend::Multiply => 1.,
            };
        }
    });
}
