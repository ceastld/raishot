//! 箭头样式菜单与端点示意直接预览共享几何，避免图标与真实绘制产生两套规则。
use crate::{
    annotation::{Point, Style},
    arrow_geometry::ArrowPreset,
    capture_toolbar::Palette,
    capture_view::CaptureView,
    toolbar_layout::ToolbarLayout,
    toolbar_widgets::{self as w, State},
};
use gpui::{canvas, div, point, prelude::*, px, rgb, Context, PathBuilder};

/// 箭头端点示意：用真实箭头几何画一条 `width`×`height` 的水平线。
pub(crate) fn arrow_preview(style: Style, width: f32, height: f32, color: u32) -> impl IntoElement {
    let style = Style {
        width: 2.,
        color,
        dashed: false,
        ..style
    };
    let y = height / 2.;
    let path = crate::arrow_geometry::path(
        Point { x: 3., y },
        Point { x: width - 3., y },
        Point { x: width / 2., y },
        style,
    );
    canvas(
        |_, _, _| (),
        move |bounds, _, w, _| {
            let screen = |p: Point| bounds.origin + point(px(p.x), px(p.y));
            let mut stroke = PathBuilder::stroke(px(1.6));
            let mut previous = None;
            for &span in &path.spans {
                if previous != Some(span.start()) {
                    stroke.move_to(screen(span.start()));
                }
                match span {
                    crate::vector_path::Span::Line { end, .. } => stroke.line_to(screen(end)),
                    crate::vector_path::Span::Cubic {
                        control1,
                        control2,
                        end,
                        ..
                    } => stroke.cubic_bezier_to(screen(end), screen(control1), screen(control2)),
                };
                previous = Some(span.end());
            }
            if !path.spans.is_empty() {
                if let Ok(stroke) = stroke.build() {
                    w.paint_path(stroke, rgb(color));
                }
            }
            for contour in &path.fills {
                let mut fill = PathBuilder::fill();
                if let Some(first) = contour.first() {
                    fill.move_to(screen(*first));
                    for p in &contour[1..] {
                        fill.line_to(screen(*p));
                    }
                    fill.close();
                    if let Ok(fill) = fill.build() {
                        w.paint_path(fill, rgb(color));
                    }
                }
            }
        },
    )
    .w(px(width))
    .h(px(height))
    .flex_shrink_0()
}

impl CaptureView {
    /// 第 1 级起“端点＋头型”合成一个下拉：列出六种预设。
    pub(crate) fn arrow_popup(
        &self,
        layout: ToolbarLayout,
        anchor: Option<(f32, f32)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = Palette::new(self.light_theme);
        let selected = ArrowPreset::of(self.editor.style);
        let mut rows = div().flex().flex_col().gap(px(1.)).text_size(px(12.5));
        for preset in ArrowPreset::ALL {
            let mut style = Style::default();
            preset.apply(&mut style);
            let on = selected == preset;
            rows = rows.child(
                w::menu_row(
                    preset.id(),
                    Some(
                        arrow_preview(style, 30., 16., if on { p.accent } else { p.foreground })
                            .into_any_element(),
                    ),
                    preset.label(),
                    None,
                    State::on(on),
                    p,
                )
                .child(self.probe(preset.id()))
                .on_click(cx.listener(move |v, _, _, cx| {
                    let mut s = v.editor.style;
                    preset.apply(&mut s);
                    v.editor.set_style(s);
                    v.toolbar_panel = crate::toolbar_layout::ToolbarPanel::None;
                    cx.notify();
                })),
            );
        }
        self.popup_panel(crate::toolbar_layout::ToolbarPanel::Arrow, layout, anchor)
            .p(px(4.))
            .child(rows)
            .child(self.probe("toolbar-popup"))
    }
}
