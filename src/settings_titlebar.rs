//! 设置窗口内容延伸进标题栏：侧栏顶部放品牌，页眉与窗口按钮同行；
//! 命中区域交给 GPUI 的 Windows 原生非客户区处理。
use crate::{settings_widgets, toolbar_icons::Icon};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, AnyElement, App, AppContext, Context, FontWeight,
    PathBuilder, Render, Window, WindowControlArea,
};

/// 标题行高度：品牌、页标题与窗口按钮的中线对齐在 HEIGHT / 2。
pub(super) const HEIGHT: f32 = 48.;
const BUTTON_WIDTH: f32 = 46.;
pub(super) const CAPTION_WIDTH: f32 = BUTTON_WIDTH * 3.;

struct CaptionHint(&'static str);
impl Render for CaptionHint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        div()
            .p_2()
            .rounded_md()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .text_size(px(12.))
            .child(self.0)
    }
}

#[derive(Clone, Copy)]
enum CaptionIcon {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn icon(kind: CaptionIcon) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let origin = bounds.origin;
            let mut path = PathBuilder::stroke(px(1.));
            let mut line = |from: (f32, f32), to: (f32, f32)| {
                path.move_to(origin + point(px(from.0), px(from.1)));
                path.line_to(origin + point(px(to.0), px(to.1)));
            };
            match kind {
                CaptionIcon::Minimize => line((1., 6.), (11., 6.)),
                CaptionIcon::Maximize => {
                    line((1.5, 1.5), (10.5, 1.5));
                    line((10.5, 1.5), (10.5, 10.5));
                    line((10.5, 10.5), (1.5, 10.5));
                    line((1.5, 10.5), (1.5, 1.5));
                }
                CaptionIcon::Restore => {
                    line((3.5, 1.5), (10.5, 1.5));
                    line((10.5, 1.5), (10.5, 8.5));
                    line((10.5, 8.5), (8.5, 8.5));
                    line((3.5, 1.5), (3.5, 3.5));
                    line((1.5, 3.5), (8.5, 3.5));
                    line((8.5, 3.5), (8.5, 10.5));
                    line((8.5, 10.5), (1.5, 10.5));
                    line((1.5, 10.5), (1.5, 3.5));
                }
                CaptionIcon::Close => {
                    line((1.5, 1.5), (10.5, 10.5));
                    line((10.5, 1.5), (1.5, 10.5));
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, window.text_style().color);
            }
        },
    )
    .size(px(12.))
    .flex_shrink_0()
}

fn button(
    id: &'static str,
    area: WindowControlArea,
    glyph: CaptionIcon,
    label: &'static str,
    band: f32,
    cx: &App,
) -> AnyElement {
    let palette = crate::theme::palette(cx);
    let surface = settings_widgets::surface(cx);
    let close = area == WindowControlArea::Close;
    // 不注册鼠标处理器：原生 NC down/up 负责点击、取消和最大化/还原。
    // 悬停底色铺到窗口顶边；命中区让出顶部缩放带，原生 HTTOP 仍可用。
    div()
        .id(id)
        .relative()
        .w(px(BUTTON_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(palette.foreground))
        .hover(move |style| {
            if close {
                style.bg(rgb(0xc42b1c)).text_color(rgb(0xffffff))
            } else {
                style.bg(rgb(surface.nav_hover))
            }
        })
        .tooltip(move |_, cx| cx.new(|_| CaptionHint(label)).into())
        .child(icon(glyph))
        .child(
            div()
                .absolute()
                .top(px(band))
                .left_0()
                .right_0()
                .bottom_0()
                .window_control_area(area),
        )
        .into_any_element()
}

/// 普通窗口顶边留给原生缩放；最大化时整行都可拖动或点击。
pub(super) fn resize_band(window: &Window) -> f32 {
    if window.is_maximized() {
        0.
    } else {
        8.
    }
}

/// 透明拖动层，叠在标题行内容之上；`right` 为右侧让给窗口按钮的宽度。
pub(super) fn drag_layer(right: f32, band: f32) -> AnyElement {
    div()
        .absolute()
        .top(px(band))
        .left_0()
        .bottom_0()
        .right(px(right))
        .window_control_area(WindowControlArea::Drag)
        .into_any_element()
}

/// 侧栏顶部的品牌行，与右侧标题行同高，整体可拖动窗口。
pub(super) fn brand(band: f32, cx: &App) -> AnyElement {
    let palette = crate::theme::palette(cx);
    div()
        .relative()
        .h(px(HEIGHT))
        .w_full()
        .flex_shrink_0()
        .px_5()
        .flex()
        .items_center()
        .gap_2()
        .child(crate::toolbar_icons::action(Icon::Capture, palette.accent).size(px(18.)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_ellipsis()
                .text_size(px(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(crate::brand::NAME),
        )
        .child(drag_layer(0., band))
        .into_any_element()
}

/// 内容区右上角的原生窗口按钮组，绝对定位在页眉之上。
pub(super) fn caption_buttons(band: f32, window: &Window, cx: &App) -> AnyElement {
    let maximized = window.is_maximized();
    div()
        .id("settings-titlebar")
        .absolute()
        .top_0()
        .right_0()
        .h(px(HEIGHT))
        .flex()
        .child(button(
            "settings-window-minimize",
            WindowControlArea::Min,
            CaptionIcon::Minimize,
            tr!("最小化", "Minimize"),
            band,
            cx,
        ))
        .child(button(
            "settings-window-maximize",
            WindowControlArea::Max,
            if maximized {
                CaptionIcon::Restore
            } else {
                CaptionIcon::Maximize
            },
            if maximized {
                tr!("还原", "Restore")
            } else {
                tr!("最大化", "Maximize")
            },
            band,
            cx,
        ))
        .child(button(
            "settings-window-close",
            WindowControlArea::Close,
            CaptionIcon::Close,
            tr!("关闭", "Close"),
            band,
            cx,
        ))
        .into_any_element()
}
