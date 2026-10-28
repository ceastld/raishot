//! 二级栏共用控件：图标按钮、分段、色块、数值框、下拉、拆分开关、文字按钮与悬停提示。
//! 截图属性栏和贴图属性栏（经 CaptureView::property_toolbar）共用；尺寸与 property_items 的估算一致。
use crate::{
    capture_toolbar::Palette,
    property_items::{CARET, FAMILY_MAX, ICON_BUTTON, SEGMENT, SWATCH},
    toolbar_icons::{self, Icon},
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, AnyView, App, Div, ElementId, PathBuilder,
    Render, SharedString, Stateful, Window,
};

/// 控件状态：开（选中）、弹层打开、不可用、删除类。
#[derive(Clone, Copy, Default)]
pub(crate) struct State {
    pub on: bool,
    pub open: bool,
    pub disabled: bool,
    pub danger: bool,
}
impl State {
    pub(crate) fn on(on: bool) -> Self {
        Self {
            on,
            ..Self::default()
        }
    }
    pub(crate) fn open(open: bool) -> Self {
        Self {
            open,
            ..Self::default()
        }
    }
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// 纯图标控件的悬停提示：名称、快捷键，可带一行说明（例如不可用的原因）。
pub(crate) struct Tip {
    title: SharedString,
    key: Option<SharedString>,
    note: Option<SharedString>,
}
impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        div()
            .max_w(px(260.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(6.))
            .bg(rgb(p.tip_bg))
            .text_color(rgb(p.tip_fg))
            .text_size(px(12.))
            .line_height(px(16.))
            .shadow_md()
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(self.title.clone())
                    .when_some(self.key.clone(), |row, key| {
                        row.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(p.tip_muted))
                                .child(key),
                        )
                    }),
            )
            .when_some(self.note.clone(), |tip, note| {
                tip.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(p.tip_muted))
                        .child(note),
                )
            })
    }
}

/// 生成 `.tooltip()` 回调；`key` 与 `note` 为空串时省略。
pub(crate) fn tip(
    title: impl Into<SharedString>,
    key: impl Into<SharedString>,
    note: impl Into<SharedString>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (title, key, note) = (title.into(), key.into(), note.into());
    move |_, cx| {
        let (title, key, note) = (title.clone(), key.clone(), note.clone());
        cx.new(|_| Tip {
            title,
            key: (!key.is_empty()).then_some(key),
            note: (!note.is_empty()).then_some(note),
        })
        .into()
    }
}

fn icon(icon: Icon, color: u32, size: f32) -> gpui::Svg {
    toolbar_icons::action(icon, color).size(px(size))
}

/// 图标按钮 28×28，圆角 6，图标 16：悬停 hover；开 selected + accent；不可用 35%；删除悬停 danger。
pub(crate) fn icon_button(
    id: &'static str,
    glyph: Icon,
    state: State,
    p: Palette,
) -> Stateful<Div> {
    let color = if state.on { p.accent } else { p.foreground };
    let danger = state.danger && !state.disabled;
    div()
        .id(id)
        .group(id)
        .relative()
        .size(px(ICON_BUTTON))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .when(state.on, |d| d.bg(rgb(p.selected)))
        .when(state.open && !state.on, |d| d.bg(rgb(p.hover)))
        .when(!state.disabled, |d| {
            d.cursor_pointer().when(!state.on, |d| {
                d.hover(move |s| {
                    s.bg(if danger {
                        rgba(p.danger_bg)
                    } else {
                        rgb(p.hover).into()
                    })
                })
            })
        })
        .when(state.disabled, |d| d.opacity(0.35))
        .child(icon(glyph, color, 16.).when(danger, |s| {
            s.group_hover(id, move |s| s.text_color(rgb(p.danger)))
        }))
}

/// 分段槽：track 底，内边距 2，圆角 8，段间距 2。
pub(crate) fn segments() -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(2.))
        .p(px(2.))
        .rounded(px(8.))
}
/// 设置槽底色；与 `segments` 分开以便调用方先加子项。
pub(crate) fn track(segments: Div, p: Palette) -> Div {
    segments.bg(rgb(p.track))
}

/// 单个分段 26×24，圆角 6：未选 muted；选中 selected + accent；不可用 30%。
pub(crate) fn segment(id: &'static str, glyph: Icon, state: State, p: Palette) -> Stateful<Div> {
    let color = if state.on { p.accent } else { p.muted };
    div()
        .id(id)
        .group(id)
        .relative()
        .h(px(24.))
        .min_w(px(SEGMENT))
        .px(px(5.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .when(state.on, |d| d.bg(rgb(p.selected)))
        .when(!state.disabled, |d| d.cursor_pointer())
        .when(!state.disabled && !state.on, |d| {
            d.hover(move |s| s.bg(rgb(p.track_hover)))
        })
        .when(state.disabled, |d| d.opacity(0.3))
        .child(
            icon(glyph, color, 16.).when(!state.disabled && !state.on, |s| {
                s.group_hover(id, move |s| s.text_color(rgb(p.foreground)))
            }),
        )
}

/// 色点 Ø18，1px 内描边；选中时外加 Ø26 的 1.5px ring 外环（偶数尺寸，居中不落半像素）。
pub(crate) fn color_dot(color: u32, selected: bool, p: Palette) -> Div {
    let dot = div()
        .size(px(18.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(color & 0xff_ffff))
        .border_1()
        .border_color(rgba(p.swatch_edge));
    if selected {
        div()
            .size(px(26.))
            .flex_shrink_0()
            .rounded_full()
            .border(px(1.5))
            .border_color(rgb(p.ring))
            .flex()
            .items_center()
            .justify_center()
            .child(dot)
    } else {
        dot
    }
}

/// 色块槽 26×28，悬停 hover 底；选中时不再显示悬停底。
pub(crate) fn swatch(
    id: impl Into<ElementId>,
    color: u32,
    selected: bool,
    p: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .w(px(SWATCH))
        .h(px(28.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .when(!selected, |d| d.hover(move |s| s.bg(rgb(p.hover))))
        .child(color_dot(color, selected, p))
}

/// 色环（七色扇形拼接）；GPUI 没有锥形渐变，用 42 个扇面近似。
pub(crate) fn color_wheel(diameter: f32) -> impl IntoElement {
    const STOPS: [u32; 7] = [
        0xff4b5c, 0xffd34d, 0x39c996, 0x2bb8c8, 0x5796ff, 0xbc8bff, 0xff4b5c,
    ];
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let radius = f32::from(bounds.size.width.min(bounds.size.height)) / 2.;
            let slices = 42;
            for i in 0..slices {
                let t = i as f32 / slices as f32;
                let segment = (t * 6.).floor() as usize;
                let local = t * 6. - segment as f32;
                let mix = |a: u32, b: u32, shift: u32| {
                    let a = ((a >> shift) & 0xff) as f32;
                    let b = ((b >> shift) & 0xff) as f32;
                    ((a + (b - a) * local).round() as u32) << shift
                };
                let (a, b) = (STOPS[segment], STOPS[segment + 1]);
                let color = mix(a, b, 16) | mix(a, b, 8) | mix(a, b, 0);
                // 稍微重叠，避免扇面之间露出底色缝。
                let start = (t - 0.002) * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                let end = (t + 1. / slices as f32 + 0.002) * std::f32::consts::TAU
                    - std::f32::consts::FRAC_PI_2;
                let at = |angle: f32| {
                    point(
                        center.x + px(radius * angle.cos()),
                        center.y + px(radius * angle.sin()),
                    )
                };
                let mut path = PathBuilder::fill();
                path.move_to(center);
                path.line_to(at(start));
                path.line_to(at((start + end) / 2.));
                path.line_to(at(end));
                path.close();
                if let Ok(path) = path.build() {
                    window.paint_path(path, rgb(color));
                }
            }
        },
    )
    .size(px(diameter))
    .flex_shrink_0()
}

/// 自定义色块：没用过时是色环（中心挖空）；当前色是自定义色时中心显示该色并带外环。
pub(crate) fn custom_swatch(id: &'static str, current: Option<u32>, p: Palette) -> Stateful<Div> {
    let selected = current.is_some();
    let center = match current {
        Some(color) => div()
            .absolute()
            .size(px(15.))
            .rounded_full()
            .border(px(1.5))
            .border_color(rgb(p.background))
            .bg(rgb(color & 0xff_ffff)),
        None => div()
            .absolute()
            .size(px(10.))
            .rounded_full()
            .bg(rgb(p.background)),
    };
    let wheel = div()
        .relative()
        .size(px(18.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .overflow_hidden()
        .child(color_wheel(18.))
        .child(center);
    let body = if selected {
        div()
            .size(px(26.))
            .flex_shrink_0()
            .rounded_full()
            .border(px(1.5))
            .border_color(rgb(p.ring))
            .flex()
            .items_center()
            .justify_center()
            .child(wheel)
    } else {
        wheel
    };
    div()
        .id(id)
        .relative()
        .w(px(SWATCH))
        .h(px(28.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .when(!selected, |d| d.hover(move |s| s.bg(rgb(p.hover))))
        .child(body)
}

/// 9px 下拉角。
pub(crate) fn chevron(p: Palette) -> gpui::Svg {
    icon(Icon::Chevron, p.muted, 9.).opacity(0.8)
}

/// 数值框、下拉、颜色按钮的共同外形：高 28，track 底，悬停 track_hover，打开时 1px accent 内描边。
fn field(id: &'static str, state: State, p: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .h(px(28.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(5.))
        .pl(px(6.))
        .pr(px(5.))
        .rounded(px(6.))
        .bg(rgb(p.track))
        .border_1()
        .border_color(if state.open {
            rgb(p.accent).into()
        } else {
            gpui::transparent_black()
        })
        .when(!state.disabled, |d| {
            d.cursor_pointer().hover(move |s| s.bg(rgb(p.track_hover)))
        })
        .when(state.disabled, |d| d.opacity(0.35))
}

/// 数值框左侧：图标说明是什么；粗细类用实心圆点直接示意。
#[derive(Clone, Copy)]
pub(crate) enum Lead {
    Icon(Icon),
    Dot(f32),
}
fn lead(lead: Lead, p: Palette) -> gpui::AnyElement {
    match lead {
        Lead::Icon(glyph) => icon(glyph, p.muted, 16.).into_any_element(),
        Lead::Dot(diameter) => {
            let d = diameter.clamp(2., 14.);
            div()
                .size(px(16.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(d)).rounded_full().bg(rgb(p.foreground)))
                .into_any_element()
        }
    }
}

/// 数值框：图标 + 数字（按该项最宽文本预留 `reserve` 并右对齐）+ 下拉角。
pub(crate) fn value_field(
    id: &'static str,
    leading: Lead,
    value: impl Into<SharedString>,
    reserve: f32,
    open: bool,
    p: Palette,
) -> Stateful<Div> {
    field(id, State::open(open), p)
        .child(lead(leading, p))
        .child(
            div()
                .min_w(px(reserve))
                .flex()
                .justify_end()
                .whitespace_nowrap()
                .child(value.into()),
        )
        .child(chevron(p))
}

/// 下拉：内容（图标或文字）+ 下拉角；文字最宽 96 后省略。
pub(crate) fn dropdown_icon(
    id: &'static str,
    glyph: Icon,
    state: State,
    p: Palette,
) -> Stateful<Div> {
    let color = if state.disabled {
        p.muted
    } else {
        p.foreground
    };
    field(id, state, p)
        .child(icon(glyph, color, 16.))
        .child(chevron(p))
}
pub(crate) fn dropdown_text(
    id: &'static str,
    text: impl Into<SharedString>,
    state: State,
    p: Palette,
) -> Stateful<Div> {
    field(id, state, p)
        .child(
            div()
                .max_w(px(FAMILY_MAX))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(text.into()),
        )
        .child(chevron(p))
}
/// 任意内容的下拉（例如箭头端点示意）。
pub(crate) fn dropdown_with(
    id: &'static str,
    content: impl IntoElement,
    state: State,
    p: Palette,
) -> Stateful<Div> {
    field(id, state, p).child(content).child(chevron(p))
}

/// 收起后的颜色按钮：色点 + 可选不透明度 + 下拉角。
pub(crate) fn color_button(
    id: &'static str,
    color: u32,
    opacity: Option<String>,
    open: bool,
    p: Palette,
) -> Stateful<Div> {
    field(id, State::open(open), p)
        .child(color_dot(color, false, p))
        .when_some(opacity, |d, text| {
            d.child(
                div()
                    .min_w(px(crate::property_items::percent_text_width()))
                    .flex()
                    .justify_end()
                    .child(text),
            )
        })
        .child(chevron(p))
}

/// 拆分开关：左半 28×28 开关，右半 14 宽 ▾ 打开细项；悬停两半一起高亮，开时都用 selected。
pub(crate) fn split_toggle(
    main: Stateful<Div>,
    caret: Stateful<Div>,
    group: &'static str,
    on: bool,
    open: bool,
    p: Palette,
) -> Div {
    let half = move |d: Stateful<Div>| {
        d.when(on, |d| d.bg(rgb(p.selected)))
            .when(!on, |d| d.group_hover(group, move |s| s.bg(rgb(p.hover))))
    };
    div()
        .group(group)
        .flex()
        .flex_shrink_0()
        .rounded(px(6.))
        .child(half(main).rounded_r(px(0.)))
        .child(
            half(caret)
                .when(open && !on, |d| d.bg(rgb(p.hover)))
                .rounded_l(px(0.)),
        )
}
pub(crate) fn split_main(id: &'static str, glyph: Icon, on: bool, p: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size(px(ICON_BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded_l(px(6.))
        .cursor_pointer()
        .child(icon(glyph, if on { p.accent } else { p.foreground }, 16.))
}
pub(crate) fn split_caret(id: &'static str, on: bool, p: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .w(px(CARET))
        .h(px(ICON_BUTTON))
        .flex()
        .items_center()
        .justify_center()
        .rounded_r(px(6.))
        .cursor_pointer()
        .child(icon(Icon::Chevron, if on { p.accent } else { p.muted }, 9.))
}

/// 文字按钮：高 28，左右 10，1px border；主按钮 accent 底，悬停 accent_hover。
pub(crate) fn text_button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
    enabled: bool,
    p: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .h(px(28.))
        .px(px(10.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .border_1()
        .whitespace_nowrap()
        .when(primary, |d| {
            d.bg(rgb(p.accent))
                .border_color(gpui::transparent_black())
                .text_color(rgb(p.on_accent))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .when(enabled, |d| d.hover(move |s| s.bg(rgb(p.accent_hover))))
        })
        .when(!primary, |d| {
            d.border_color(rgb(p.border))
                .when(enabled, |d| d.hover(move |s| s.bg(rgb(p.hover))))
        })
        .when(enabled, |d| d.cursor_pointer())
        .when(!enabled, |d| d.opacity(0.4))
        .child(label.into())
}

/// 栏内说明文字。
pub(crate) fn hint(text: impl Into<SharedString>, p: Palette) -> Div {
    div()
        .px(px(6.))
        .flex_shrink_0()
        .whitespace_nowrap()
        .text_color(rgb(p.muted))
        .child(text.into())
}

/// 组间竖线 1×18，两侧各 4。
pub(crate) fn separator(p: Palette) -> Div {
    div()
        .w(px(1.))
        .h(px(18.))
        .mx(px(4.))
        .flex_shrink_0()
        .bg(rgb(p.border))
}

/// 弹层里的菜单行：高 32，左图标位 30，选中 accent 并带对勾。
pub(crate) fn menu_row(
    id: impl Into<ElementId>,
    lead: Option<gpui::AnyElement>,
    label: impl Into<SharedString>,
    trailing: Option<gpui::AnyElement>,
    state: State,
    p: Palette,
) -> Stateful<Div> {
    let color = if state.danger {
        p.danger
    } else if state.on {
        p.accent
    } else {
        p.foreground
    };
    let danger = state.danger;
    div()
        .id(id)
        .relative()
        .h(px(32.))
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .pl(px(4.))
        .pr(px(8.))
        .rounded(px(6.))
        .text_color(rgb(color))
        .when(!state.disabled, |d| {
            d.cursor_pointer().hover(move |s| {
                s.bg(if danger {
                    rgba(p.danger_bg)
                } else {
                    rgb(p.hover).into()
                })
            })
        })
        .when(state.disabled, |d| d.opacity(0.35))
        .child(
            div()
                .w(px(30.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when_some(lead, |d, lead| d.child(lead)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .child(label.into()),
        )
        .when_some(trailing, |d, t| d.child(t))
        .when(state.on && !state.danger, |d| {
            d.child(icon(Icon::Check, p.accent, 16.))
        })
}
/// 菜单行图标（继承行色）。
pub(crate) fn menu_icon(glyph: Icon, color: u32) -> gpui::AnyElement {
    icon(glyph, color, 16.).into_any_element()
}
/// 菜单行右侧快捷键。
pub(crate) fn menu_key(text: &'static str, p: Palette) -> gpui::AnyElement {
    div()
        .text_size(px(11.))
        .text_color(rgb(p.muted))
        .child(text)
        .into_any_element()
}
/// 菜单分隔线。
pub(crate) fn menu_divider(p: Palette) -> Div {
    div()
        .h(px(1.))
        .mx(px(6.))
        .my(px(3.))
        .flex_shrink_0()
        .bg(rgb(p.border))
}

/// 弹层里“标签 + 控件”一行：标签列 52，行高至少 28。
pub(crate) fn labeled_row(
    label: impl Into<SharedString>,
    control: impl IntoElement,
    p: Palette,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .min_h(px(28.))
        .child(
            div()
                .w(px(52.))
                .flex_shrink_0()
                .text_color(rgb(p.muted))
                .child(label.into()),
        )
        .child(control)
}

/// 开关（28×16）：开 accent，关 track_hover。
pub(crate) fn switch(on: bool, p: Palette) -> Div {
    div()
        .w(px(28.))
        .h(px(16.))
        .flex_shrink_0()
        .rounded(px(8.))
        .bg(rgb(if on { p.accent } else { p.track_hover }))
        .relative()
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(if on { 14. } else { 2. }))
                .size(px(12.))
                .rounded_full()
                .bg(rgb(0xffffff))
                .shadow_sm(),
        )
}

/// 弹层标题（muted、半粗）。
pub(crate) fn popup_title(text: impl Into<SharedString>, p: Palette) -> Div {
    div()
        .text_size(px(12.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(p.muted))
        .child(text.into())
}
