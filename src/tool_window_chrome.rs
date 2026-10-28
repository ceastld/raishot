//! 独立工具窗口（二维码、拼接、批处理、PDF 等）共用的外框：页面底色、预览台、选项行与底部操作栏。
//! 与主界面共享设置页的层次底色和按钮外观，只管绘制；状态与事件由各窗口挂接。
use crate::{
    capture_toolbar::Palette,
    settings_widgets::{self as widgets, ButtonKind, Surface},
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, Div, ElementId, FontWeight, SharedString, Stateful,
};

#[derive(Clone, Copy)]
pub(crate) struct Chrome {
    pub palette: Palette,
    pub surface: Surface,
}

pub(crate) fn chrome(cx: &App) -> Chrome {
    Chrome {
        palette: crate::theme::palette(cx),
        surface: widgets::surface(cx),
    }
}

/// 状态行的语气：空闲为灰点，进行中与完成为强调色，失败为红色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Idle,
    Busy,
    Done,
    Failed,
}

/// 整窗外壳：主体在上可伸缩，操作栏贴底。
pub(crate) fn shell(c: Chrome) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(c.surface.page))
        .text_color(rgb(c.palette.foreground))
        .font_family("Microsoft YaHei UI")
        .text_size(px(14.))
}

/// 预览台：凹槽底色的圆角区域，内容居中。
pub(crate) fn stage(c: Chrome) -> Div {
    div()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(rgb(c.surface.line))
        .bg(rgb(c.surface.track))
}

/// 预览台空状态：淡色大图标和一句提示。
pub(crate) fn empty_state(
    icon: crate::toolbar_icons::Icon,
    text: impl Into<SharedString>,
    c: Chrome,
) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_3()
        .px_6()
        .child(crate::toolbar_icons::action(icon, c.palette.border).size(px(44.)))
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(c.palette.muted))
                .text_center()
                .child(text.into()),
        )
}

/// 左侧短标签、右侧控件的选项行。
pub(crate) fn option_row(label: impl Into<SharedString>, c: Chrome) -> Div {
    div().flex().items_center().gap_3().min_h(px(36.)).child(
        div()
            .min_w(px(64.))
            .flex_shrink_0()
            .whitespace_nowrap()
            .text_size(px(13.))
            .text_color(rgb(c.palette.muted))
            .child(label.into()),
    )
}

/// 分段选项；忙碌时整组置灰且不响应。
pub(crate) fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    enabled: bool,
    c: Chrome,
) -> Stateful<Div> {
    widgets::segment(id, label, active, c.palette, c.surface)
        .when(!enabled, |segment| segment.opacity(0.45).cursor_default())
}

/// 操作按钮；不可用时降低对比，调用者只在可用时挂接点击。
pub(crate) fn action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
    c: Chrome,
) -> Stateful<Div> {
    widgets::button(id, label, kind, c.palette, c.surface)
        .when(!enabled, |button| button.opacity(0.45).cursor_default())
}

/// 底部操作栏：左侧状态，右侧按钮（主操作放最右）。
pub(crate) fn footer(
    status: impl Into<SharedString>,
    tone: Tone,
    actions: impl IntoIterator<Item = AnyElement>,
    c: Chrome,
) -> Div {
    let dot = match tone {
        Tone::Idle => c.palette.border,
        Tone::Busy | Tone::Done => c.palette.accent,
        Tone::Failed => c.palette.danger,
    };
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_3()
        .px_5()
        .py_3()
        .border_t_1()
        .border_color(rgb(c.surface.line))
        .bg(rgb(c.surface.card))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .size(px(7.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(dot)),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(px(12.))
                        .text_color(rgb(if tone == Tone::Failed {
                            c.palette.danger
                        } else {
                            c.palette.muted
                        }))
                        .child(status.into()),
                ),
        )
        .child(div().flex().flex_shrink_0().gap_2().children(actions))
}

/// 字段标题：小号半粗，用于参数区分组。
pub(crate) fn field_label(text: impl Into<SharedString>, c: Chrome) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(c.palette.muted))
        .child(text.into())
}

/// 左侧参数栏：固定宽度、可纵向滚动，分组之间留白。
pub(crate) fn sidebar(id: &'static str, c: Chrome) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(SIDEBAR_WIDTH))
        .h_full()
        .flex_shrink_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap_5()
        .p_5()
        .border_r_1()
        .border_color(rgb(c.surface.line))
        .bg(rgb(c.surface.card))
}

pub(crate) const SIDEBAR_WIDTH: f32 = 312.;

/// 参数栏中的一组：小标题在上，控件在下。
pub(crate) fn field(label: impl Into<SharedString>, c: Chrome) -> Div {
    div().flex().flex_col().gap_2().child(field_label(label, c))
}

/// 参数组的补充说明。
pub(crate) fn hint(text: impl Into<SharedString>, c: Chrome) -> Div {
    div()
        .text_size(px(12.))
        .line_height(px(18.))
        .text_color(rgb(c.palette.muted))
        .child(text.into())
}

/// 单选分段组：选中值高亮，点击交给 `pick`；不可用时整组置灰。
pub(crate) fn choice<T: 'static, V: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(&'static str, V)],
    current: V,
    enabled: bool,
    c: Chrome,
    cx: &mut gpui::Context<T>,
    pick: fn(&mut T, V, &mut gpui::Window, &mut gpui::Context<T>),
) -> Div {
    let mut group = widgets::segmented(c.surface);
    for (index, (label, value)) in options.iter().copied().enumerate() {
        let item = segment((id, index), label, value == current, enabled, c);
        group = group.child(if enabled {
            item.on_click(cx.listener(move |view, _, window, cx| pick(view, value, window, cx)))
        } else {
            item
        });
    }
    group
}

/// 数值步进：[−] 值 [+]；到边界的一侧置灰。
#[allow(clippy::too_many_arguments)]
pub(crate) fn stepper<T: 'static>(
    id: &'static str,
    value: impl Into<SharedString>,
    can_decrease: bool,
    can_increase: bool,
    c: Chrome,
    cx: &mut gpui::Context<T>,
    decrease: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
    increase: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> Div {
    let step = |index: usize, label: &'static str, enabled: bool| {
        div()
            .id((id, index))
            .size(px(28.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(15.))
            .text_color(rgb(if enabled {
                c.palette.foreground
            } else {
                c.palette.border
            }))
            .when(enabled, |step| {
                step.cursor_pointer()
                    .hover(move |style| style.bg(rgb(c.palette.hover)))
            })
            .child(label)
    };
    let minus = step(0, "−", can_decrease);
    let plus = step(1, "+", can_increase);
    widgets::segmented(c.surface)
        .flex_nowrap()
        .child(if can_decrease {
            minus.on_click(cx.listener(move |view, _, window, cx| decrease(view, window, cx)))
        } else {
            minus
        })
        .child(
            div()
                .min_w(px(56.))
                .px_2()
                .flex()
                .justify_center()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(value.into()),
        )
        .child(if can_increase {
            plus.on_click(cx.listener(move |view, _, window, cx| increase(view, window, cx)))
        } else {
            plus
        })
}

/// 预览台上方的细条：左侧说明，右侧放少量导航或视图按钮。
pub(crate) fn stage_bar(c: Chrome) -> Div {
    div()
        .flex_shrink_0()
        .h(px(36.))
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(12.))
        .text_color(rgb(c.palette.muted))
}

/// 有序列表的一行：序号、名称与次要说明；调用者在右侧追加操作。
pub(crate) fn list_row(
    id: impl Into<ElementId>,
    number: usize,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    c: Chrome,
) -> Stateful<Div> {
    list_row_with(id, number, None, title, detail, c)
}

/// 同 [`list_row`]，序号后可放一个小缩略图。
pub(crate) fn list_row_with(
    id: impl Into<ElementId>,
    number: usize,
    leading: Option<AnyElement>,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    c: Chrome,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(40.))
        .px_2()
        .rounded_md()
        .hover(move |style| style.bg(rgb(c.surface.nav_hover)))
        .child(
            div()
                .min_w(px(22.))
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(rgb(c.palette.muted))
                .child(number.to_string()),
        )
        .children(leading)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(div().truncate().text_size(px(13.)).child(title.into()))
                .when_some(detail, |text, detail| {
                    text.child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(c.palette.muted))
                            .child(detail),
                    )
                }),
        )
}

/// 列表行右侧的小图标按钮。
pub(crate) fn row_icon<T: 'static>(
    id: impl Into<ElementId>,
    icon: crate::toolbar_icons::Icon,
    enabled: bool,
    c: Chrome,
    cx: &mut gpui::Context<T>,
    on_click: impl Fn(&mut T, &mut gpui::Window, &mut gpui::Context<T>) + 'static,
) -> AnyElement {
    let button = widgets::icon_button(id, icon, enabled, c.palette);
    if enabled {
        button
            .on_click(cx.listener(move |view, _, window, cx| on_click(view, window, cx)))
            .into_any_element()
    } else {
        button.into_any_element()
    }
}
