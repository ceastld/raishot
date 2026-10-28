//! 设置界面的卡片、设置行、开关、分段、按钮与键帽外观；状态和事件由各设置页挂接。
use crate::capture_toolbar::Palette;
use gpui::{
    div, prelude::*, px, rgb, rgba, AnyElement, App, Div, ElementId, FontWeight, SharedString,
    Stateful,
};

/// 设置窗口专用的层次底色；只影响辅助界面，不参与截图像素。
#[derive(Clone, Copy)]
pub(crate) struct Surface {
    pub page: u32,
    pub card: u32,
    pub raised: u32,
    pub line: u32,
    pub track: u32,
    pub nav_hover: u32,
    pub nav_selected: u32,
    pub on_accent: u32,
}

pub(crate) fn surface(cx: &App) -> Surface {
    if crate::theme::is_light(cx) {
        Surface {
            page: 0xf3f6f9,
            card: 0xffffff,
            raised: 0xffffff,
            line: 0xe3e8ed,
            track: 0xeef2f5,
            nav_hover: 0xe9eef2,
            nav_selected: 0xe1e8ee,
            on_accent: 0xffffff,
        }
    } else {
        Surface {
            page: 0x171b21,
            card: 0x20262e,
            raised: 0x37404b,
            line: 0x2f3741,
            track: 0x191d23,
            nav_hover: 0x1f252c,
            nav_selected: 0x27303a,
            on_accent: 0x0d1f18,
        }
    }
}

/// 用设置窗口字体测量单行文字宽度（向上取整留出亚像素余量）。
pub(crate) fn text_width(
    window: &gpui::Window,
    text: &'static str,
    size: f32,
    weight: FontWeight,
) -> f32 {
    let font = gpui::Font {
        weight,
        ..gpui::font("Microsoft YaHei UI")
    };
    let line = window.text_system().shape_line(
        text.into(),
        px(size),
        &[gpui::TextRun {
            len: text.len(),
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    f32::from(line.width).ceil() + 1.
}

/// 分组标题及其下方内容。
pub(crate) fn section(title: impl Into<SharedString>, palette: Palette) -> Div {
    div().flex().flex_col().flex_shrink_0().gap_2().child(
        div()
            .px_1()
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(palette.foreground))
            .child(title.into()),
    )
}

/// 圆角卡片；行之间插入内缩分隔线。
pub(crate) fn card(surface: Surface, rows: impl IntoIterator<Item = AnyElement>) -> Div {
    let mut card = div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(rgb(surface.line))
        .bg(rgb(surface.card));
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            card = card.child(div().h(px(1.)).mx_3().bg(rgb(surface.line)));
        }
        card = card.child(row);
    }
    card
}

/// 左侧标题与说明、右侧控件的设置行；调用者追加控件和事件。
pub(crate) fn row(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    palette: Palette,
) -> Div {
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
                .child(title.into()),
        );
    if let Some(description) = description {
        text = text.child(
            div()
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(rgb(palette.muted))
                .child(description),
        );
    }
    div()
        .relative()
        .flex()
        .items_center()
        .gap_4()
        .min_h(px(56.))
        .px_3()
        .py_2()
        .rounded_md()
        .child(text)
}

/// 纵向排列的表单块：小标题在上，输入或选项在下。
pub(crate) fn field_block(title: impl Into<SharedString>, palette: Palette) -> Div {
    div().flex().flex_col().gap_2().px_3().py_3().child(
        div()
            .text_size(px(13.))
            .text_color(rgb(palette.foreground))
            .child(title.into()),
    )
}

pub(crate) fn note(text: impl Into<SharedString>, palette: Palette) -> Div {
    div()
        .px_1()
        .text_size(px(12.))
        .line_height(px(18.))
        .text_color(rgb(palette.muted))
        .child(text.into())
}

/// 40×20 开关；只负责绘制，切换由所在行处理。
pub(crate) fn switch(checked: bool, palette: Palette, surface: Surface) -> Div {
    div()
        .relative()
        .w(px(40.))
        .h(px(20.))
        .flex_shrink_0()
        .rounded_full()
        .border_1()
        .border_color(rgb(if checked {
            palette.accent
        } else {
            palette.muted
        }))
        .bg(if checked {
            rgb(palette.accent)
        } else {
            rgba(0)
        })
        .child(
            div()
                .absolute()
                .top(px(if checked { 3. } else { 4. }))
                .left(px(if checked { 23. } else { 4. }))
                .size(px(if checked { 12. } else { 10. }))
                .rounded_full()
                .bg(rgb(if checked {
                    surface.on_accent
                } else {
                    palette.muted
                })),
        )
}

pub(crate) fn segmented(surface: Surface) -> Div {
    div()
        .flex()
        .flex_wrap()
        .flex_shrink_0()
        .items_center()
        .gap(px(2.))
        .p(px(3.))
        .rounded_lg()
        .border_1()
        .border_color(rgb(surface.line))
        .bg(rgb(surface.track))
}

pub(crate) fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    palette: Palette,
    surface: Surface,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .h(px(28.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(13.))
        .when(active, |segment| {
            segment
                .bg(rgb(surface.raised))
                .text_color(rgb(palette.foreground))
                .font_weight(FontWeight::SEMIBOLD)
                .shadow_sm()
        })
        .when(!active, |segment| {
            segment
                .text_color(rgb(palette.muted))
                .hover(move |style| style.text_color(rgb(palette.foreground)))
        })
        .child(label.into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonKind {
    Primary,
    Secondary,
}

pub(crate) fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    palette: Palette,
    surface: Surface,
) -> Stateful<Div> {
    let primary = kind == ButtonKind::Primary;
    div()
        .id(id)
        .relative()
        .h(px(32.))
        .px(px(14.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(13.))
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
        .child(label.into())
}

/// 可选中的小标签，用于预设、插入变量与日期格式。
pub(crate) fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    palette: Palette,
    surface: Surface,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .h(px(28.))
        .px(px(10.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(12.))
        .border_1()
        .border_color(rgb(if active { palette.accent } else { surface.line }))
        .bg(rgb(if active {
            palette.selected
        } else {
            surface.raised
        }))
        .text_color(rgb(if active {
            palette.accent
        } else {
            palette.foreground
        }))
        .hover(move |style| {
            if active {
                style
            } else {
                style.bg(rgb(palette.hover))
            }
        })
        .child(label.into())
}

/// 快捷键各段显示为键帽。
pub(crate) fn keycaps(parts: &[String], palette: Palette, surface: Surface) -> Div {
    let mut keys = div().flex().flex_shrink_0().items_center().gap_1();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            keys = keys.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child("+"),
            );
        }
        keys = keys.child(
            div()
                .h(px(26.))
                .min_w(px(26.))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_b_2()
                .border_color(rgb(surface.line))
                .bg(rgb(surface.raised))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(palette.foreground))
                .child(part.clone()),
        );
    }
    keys
}

pub(crate) fn badge(text: impl Into<SharedString>, palette: Palette, surface: Surface) -> Div {
    div()
        .h(px(22.))
        .px_2()
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded_md()
        .bg(rgb(surface.track))
        .text_size(px(12.))
        .text_color(rgb(palette.muted))
        .child(text.into())
}

/// 28×28 图标按钮；不可用时降低对比且不响应悬停。
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    icon: crate::toolbar_icons::Icon,
    enabled: bool,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size(px(28.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(move |style| style.bg(rgb(palette.hover)))
        })
        .child(
            crate::toolbar_icons::action(
                icon,
                if enabled {
                    palette.foreground
                } else {
                    palette.border
                },
            )
            .size(px(16.)),
        )
}
