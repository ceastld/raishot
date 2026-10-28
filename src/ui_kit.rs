//! 弹出菜单与悬停提示的共用外观：截图、贴图与长截图窗口统一行高、间距和配色。
use super::Palette;
use gpui::{
    div, prelude::*, px, rgb, AnyElement, AnyView, App, Context, FontWeight, SharedString, Svg,
    Window,
};

/// 菜单行高；双列菜单的尺寸估算与绘制共用。
pub(crate) const MENU_ROW: f32 = 28.;
/// 分组标题高度（含上方留白）。
pub(crate) const MENU_SECTION: f32 = 24.;
/// 菜单内边距与列间距。
pub(crate) const MENU_PAD: f32 = 6.;
pub(crate) const MENU_GAP: f32 = 4.;

/// 悬停提示：首个“ · ”之前为名称，其后（快捷键、补充说明）用次要色。
pub(crate) struct Tip(pub(crate) SharedString);
impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tip_body(crate::theme::palette(cx), &self.0)
    }
}
/// 各窗口自有的提示视图也用这份外观，保证截图、贴图、长截图与设置一致。
pub(crate) fn tip_body(p: Palette, text: &str) -> gpui::Div {
    let (head, rest) = match text.split_once(" · ") {
        Some((head, rest)) => (head.to_owned(), Some(rest.to_owned())),
        None => (text.to_owned(), None),
    };
    div()
        .px(px(8.))
        .py(px(5.))
        .rounded(px(6.))
        .bg(rgb(p.background))
        .border_1()
        .border_color(rgb(p.border))
        .shadow_md()
        .text_size(px(12.))
        .text_color(rgb(p.foreground))
        .flex()
        .items_center()
        .gap(px(6.))
        .child(head)
        .when_some(rest, |d, rest| {
            d.child(div().text_color(rgb(p.muted)).child(rest))
        })
}
/// `.tooltip(ui_kit::tip(text))`。
pub(crate) fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}

/// 菜单分组标题。
pub(crate) fn section_title(p: Palette, text: impl Into<SharedString>) -> gpui::Div {
    div()
        .h(px(MENU_SECTION))
        .px(px(8.))
        .pb(px(4.))
        .flex()
        .items_end()
        .flex_shrink_0()
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(p.muted))
        .child(text.into())
}

/// 行尾快捷键或取值。
pub(crate) fn trailing_text(p: Palette, text: impl Into<SharedString>) -> AnyElement {
    div()
        .flex_shrink_0()
        .text_size(px(11.))
        .text_color(rgb(p.muted))
        .child(text.into())
        .into_any_element()
}

/// 菜单行：图标、名称（过长截断）、行尾内容。选中用强调色与选中底；
/// 不可用时变暗且无悬停与手型光标，调用方负责忽略点击。
pub(crate) fn menu_row(
    p: Palette,
    id: &'static str,
    icon: Option<Svg>,
    label: impl Into<SharedString>,
    trailing: Option<AnyElement>,
    active: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .relative()
        .id(id)
        .h(px(MENU_ROW))
        .min_w(px(0.))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(8.))
        .flex_shrink_0()
        .rounded(px(6.))
        .text_size(px(12.))
        .text_color(rgb(if active { p.accent } else { p.foreground }))
        .when(active, |d| d.bg(rgb(p.selected)))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(move |s| s.bg(rgb(if active { p.selected } else { p.hover })))
        })
        .when(!enabled, |d| d.opacity(0.38))
        .when_some(icon, |d, icon| d.child(icon.size(px(16.))))
        .child(div().flex_1().min_w(px(0.)).truncate().child(label.into()))
        .when_some(trailing, |d, trailing| d.child(trailing))
}

/// 双列网格：每行两个等宽单元，末行不足时补空位保持对齐。
pub(crate) fn two_columns(cells: Vec<AnyElement>) -> gpui::Div {
    let mut grid = div().flex().flex_col().gap(px(2.));
    let mut cells = cells.into_iter();
    while let Some(first) = cells.next() {
        let second = cells.next();
        grid = grid.child(
            div()
                .flex()
                .gap(px(MENU_GAP))
                .child(div().flex_1().min_w(px(0.)).flex().flex_col().child(first))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .when_some(second, |d, second| d.child(second)),
                ),
        );
    }
    grid
}
/// 双列网格高度，与 `two_columns` 绘制一致。
pub(crate) fn two_columns_height(count: usize) -> f32 {
    let rows = count.div_ceil(2) as f32;
    if rows == 0. {
        0.
    } else {
        rows * MENU_ROW + (rows - 1.) * 2.
    }
}
