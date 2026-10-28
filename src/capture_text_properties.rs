//! 字体菜单、“文字样式”弹层与描边／底色细项的颜色和数值，复用二级栏的单一弹层状态。
use crate::{
    capture_toolbar::Palette,
    capture_view::{Action, CaptureView},
    interaction::Parameter,
    text_format::{self, FontFamily},
    toolbar_layout::{ColorTarget, ToolbarLayout, ToolbarPanel},
    toolbar_widgets::{self as w, Lead, State},
};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context};

impl CaptureView {
    pub(crate) fn font_family_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .text_size(px(12.5))
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(11.5))
                    .text_color(rgb(p.muted))
                    .child(tr!("系统字体", "System font")),
            );
        for (i, name) in text_format::fonts().iter().enumerate() {
            let on = self.editor.style.text.font == FontFamily(i);
            let mut item = w::menu_row(
                ("font-choice", i),
                None,
                name.clone(),
                None,
                State::on(on),
                p,
            )
            .font_family(name.clone())
            .on_click(cx.listener(move |v, _, _, cx| {
                let mut s = v.editor.style;
                s.text.font = FontFamily::named(name);
                v.editor.set_style(s);
                v.toolbar_panel = ToolbarPanel::None;
                cx.notify();
            }));
            if name == "Consolas" {
                item = item.child(self.probe("font-consolas"));
            }
            if name == "Microsoft YaHei UI" {
                item = item.child(self.probe("font-yahei"));
            }
            body = body.child(item);
        }
        self.popup_panel(ToolbarPanel::FontFamily, layout, anchor)
            .p(px(4.))
            .overflow_y_scroll()
            .child(body)
            .child(self.probe("toolbar-popup"))
    }

    /// 细项弹层里的数值框：单击开参数弹层（返回本细项），悬停滚轮调值。
    pub(crate) fn text_value_field(
        &self,
        id: &'static str,
        lead: Lead,
        value: String,
        reserve: f32,
        panel: ToolbarPanel,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let parameter = match panel {
            ToolbarPanel::TextOutline => Parameter::TextOutline,
            ToolbarPanel::TextBackgroundAlpha => Parameter::TextBackgroundAlpha,
            _ => Parameter::TextBackgroundRadius,
        };
        w::value_field(id, lead, value, reserve, self.toolbar_panel == panel, p)
            .on_scroll_wheel(cx.listener(move |v, e, w, cx| v.wheel_parameter(parameter, e, w, cx)))
            .on_click(cx.listener(move |v, _, w, cx| v.perform(Action::Panel(panel), w, cx)))
            .child(self.probe(id))
            .into_any_element()
    }

    /// 描边／底色的 4 个常用色 + 自定义；在细项里改颜色会顺带打开开关。
    pub(crate) fn text_colors(&self, outline: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = Palette::new(self.light_theme);
        let colors = if outline {
            [
                ("outline-white", 0xffffff),
                ("outline-black", 0x161b22),
                ("outline-red", 0xff4b5c),
                ("outline-yellow", 0xffd34d),
            ]
        } else {
            [
                ("background-white", 0xffffff),
                ("background-black", 0x161b22),
                ("background-yellow", 0xffd34d),
                ("background-blue", 0x5796ff),
            ]
        };
        let t = self.editor.style.text;
        let current = if outline {
            t.outline_color
        } else {
            t.background_color
        } & 0xff_ffff;
        let mut row = div().flex().flex_shrink_0().items_center();
        for (id, color) in colors {
            row = row.child(
                w::swatch(id, color, current == color, p)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        let mut s = v.editor.style;
                        if outline {
                            s.text.outline_color = color;
                            s.text.outline = true;
                        } else {
                            s.text.background_color = color;
                            s.text.background = true;
                        }
                        v.editor.set_style(s);
                        cx.notify();
                    }))
                    .child(self.probe(id)),
            );
        }
        let custom_id = if outline {
            "outline-custom"
        } else {
            "background-custom"
        };
        let known = colors.iter().any(|(_, c)| *c == current);
        row.child(
            w::custom_swatch(custom_id, (!known).then_some(current), p)
                .tooltip(w::tip(tr!("自定义颜色", "Custom color"), "", ""))
                .on_click(cx.listener(move |v, _, w, cx| {
                    let s = v.editor.style;
                    let (target, current) = if outline {
                        (ColorTarget::TextOutline, s.text.outline_color)
                    } else {
                        (ColorTarget::TextBackground, s.text.background_color)
                    };
                    v.open_color_picker(target, current, w, cx);
                }))
                .child(self.probe(custom_id)),
        )
        .into_any_element()
    }

    /// 第 1 级起的“文字样式”弹层：斜体、下划线、删除线与对齐。
    pub(crate) fn text_format_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .text_size(px(12.))
            .child(w::labeled_row(
                tr!("字形", "Style"),
                self.glyph_segments(cx),
                p,
            ))
            .child(w::labeled_row(
                tr!("对齐", "Align"),
                self.align_segments(cx),
                p,
            ))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "对齐只影响多行文字。",
                        "Alignment affects multi-line text."
                    )),
            );
        self.popup_panel(ToolbarPanel::TextFormat, layout, anchor)
            .p(px(12.))
            .child(body)
            .child(self.probe("toolbar-popup"))
    }
}
