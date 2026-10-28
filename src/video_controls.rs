//! 视频窗口控制条的公共绘制与坐标换算。
use gpui::{canvas, div, prelude::*, px, rgb, App, Bounds, Div, Pixels, SharedString, Stateful};
use std::{cell::Cell, rc::Rc};

pub(crate) type SliderBounds = Rc<Cell<Option<Bounds<Pixels>>>>;

pub(crate) fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    let p = crate::theme::palette(cx);
    div()
        .id(id)
        .h(px(32.))
        .px(px(10.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(12.))
        .whitespace_nowrap()
        .text_color(rgb(if active { p.on_accent } else { p.foreground }))
        .bg(rgb(if active { p.accent } else { p.background }))
        .hover(move |style| style.bg(rgb(if active { p.accent_hover } else { p.hover })))
        .child(label.into())
}

pub(crate) fn slider(
    id: &'static str,
    ratio: f32,
    bounds: SliderBounds,
    cx: &App,
) -> Stateful<Div> {
    let p = crate::theme::palette(cx);
    let ratio = if ratio.is_finite() {
        ratio.clamp(0., 1.)
    } else {
        0.
    };
    div()
        .id(id)
        .relative()
        .h(px(24.))
        .w_full()
        .cursor_pointer()
        .flex()
        .items_center()
        .child(
            div()
                .relative()
                .w_full()
                .h(px(4.))
                .rounded_full()
                .bg(rgb(p.border))
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(gpui::relative(ratio))
                        .rounded_full()
                        .bg(rgb(p.accent)),
                ),
        )
        .child(
            canvas(
                move |layout, _, _| bounds.set(Some(layout)),
                |_, _, _, _| (),
            )
            .absolute()
            .size_full(),
        )
}

pub(crate) fn ratio_at(bounds: &SliderBounds, x: Pixels) -> Option<f32> {
    let bounds = bounds.get()?;
    let width = f32::from(bounds.size.width);
    (width > 0.).then(|| ((f32::from(x) - f32::from(bounds.origin.x)) / width).clamp(0., 1.))
}
