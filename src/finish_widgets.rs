//! 选区外观面板的 GPUI 视觉组件；业务状态、标签和事件由调用者挂接。
use crate::capture_toolbar::Palette;
use gpui::{div, prelude::*, px, rgb, rgba, Div, Stateful};

/// 分段控件内的28px选项；选中使用平整面板色和细边框，不铺大面积强调色。
pub(crate) fn segment(
    id: &'static str,
    label: &'static str,
    active: bool,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px(px(10.))
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .rounded_md()
        .border_1()
        .border_color(if active { rgb(palette.border) } else { rgba(0) })
        .bg(if active {
            rgb(palette.background)
        } else {
            rgba(0)
        })
        .text_size(px(12.))
        .text_color(rgb(if active {
            palette.foreground
        } else {
            palette.muted
        }))
        .cursor_pointer()
        .focusable()
        .tab_index(0)
        .hover(move |style| {
            style
                .bg(rgb(palette.hover))
                .text_color(rgb(palette.foreground))
        })
        .focus(move |style| style.border_color(rgb(palette.accent)))
        .child(label)
}

/// 26px色块；传入0xAARRGGBB，保留alpha并在主题衬底上显示，外环表达选中。
pub(crate) fn swatch(
    id: &'static str,
    color: u32,
    selected: bool,
    palette: Palette,
) -> Stateful<Div> {
    let color_face = div()
        .relative()
        .size_full()
        .rounded_full()
        .border_1()
        .border_color(rgb(palette.muted))
        .bg(rgb(palette.hover))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded_full()
                // gpui::rgba接受0xRRGGBBAA，ARGB左旋8位保留全部颜色与透明度。
                .bg(rgba(color.rotate_left(8))),
        );
    div()
        .id(id)
        .size(px(26.))
        .flex_shrink_0()
        .p(px(1.))
        .rounded_full()
        .border_2()
        .border_color(rgb(if selected {
            palette.accent
        } else {
            palette.border
        }))
        .bg(rgb(palette.background))
        .cursor_pointer()
        .focusable()
        .tab_index(0)
        .hover(move |style| {
            style.border_color(rgb(if selected {
                palette.accent
            } else {
                palette.muted
            }))
        })
        .focus(move |style| style.border_color(rgb(palette.accent)))
        .child(color_face)
}
