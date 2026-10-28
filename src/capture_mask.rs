//! 套索辅助层独立于 Scene：裁剪提示和轮廓不会进入输出。
use crate::{capture_view::CaptureView, geometry::Rect};
use gpui::{canvas, point, prelude::*, px, rgb, rgba, PathBuilder};
impl CaptureView {
    pub(crate) fn lasso_chrome(&self, scale: f32) -> Option<gpui::AnyElement> {
        let host = self.editor.crop?;
        let points = self.editor.lasso_points()?;
        let live = self.editor.drawing_lasso();
        let draft = self.editor.lasso_draft.is_some();
        let mask = if live {
            None
        } else {
            self.editor
                .lasso_draft
                .clone()
                .or(self.editor.document.crop_mask.clone())
        };
        Some(
            canvas(
                |_, _, _| (),
                move |bounds, _, w, _| {
                    let screen = |p: crate::annotation::Point| {
                        bounds.origin + point(px(p.x / scale), px(p.y / scale))
                    };
                    if let Some(mask) = &mask {
                        let mut dim = PathBuilder::fill().with_style(gpui::PathStyle::Fill(
                            gpui::FillOptions::default().with_fill_rule(gpui::FillRule::EvenOdd),
                        ));
                        let Rect {
                            x,
                            y,
                            width,
                            height,
                        } = host;
                        dim.move_to(
                            bounds.origin + point(px(x as f32 / scale), px(y as f32 / scale)),
                        );
                        dim.line_to(
                            bounds.origin
                                + point(px((x + width) as f32 / scale), px(y as f32 / scale)),
                        );
                        dim.line_to(
                            bounds.origin
                                + point(
                                    px((x + width) as f32 / scale),
                                    px((y + height) as f32 / scale),
                                ),
                        );
                        dim.line_to(
                            bounds.origin
                                + point(px(x as f32 / scale), px((y + height) as f32 / scale)),
                        );
                        dim.close();
                        for triangle in mask.triangles.iter() {
                            dim.move_to(screen(triangle[0]));
                            dim.line_to(screen(triangle[1]));
                            dim.line_to(screen(triangle[2]));
                            dim.close();
                        }
                        if let Ok(dim) = dim.build() {
                            w.paint_path(dim, rgba(0x00000066));
                        }
                    }
                    if let Some(first) = points.first() {
                        let mut outline = PathBuilder::stroke(px(1.5));
                        if draft || live {
                            // 沿路径累计相位，拐角不会重启虚线，也不生成整屏位图。
                            let mut phase = 0_f32;
                            let mut previous = *first;
                            for next in points[1..].iter().chain((!live).then_some(first)) {
                                let length = previous.distance(*next) / scale;
                                if length > 0. {
                                    let mut cursor = 0.;
                                    while cursor < length {
                                        let ink = phase < 4.;
                                        let step = (if ink { 4. } else { 7. } - phase)
                                            .min(length - cursor);
                                        let at = |t: f32| crate::annotation::Point {
                                            x: previous.x + (next.x - previous.x) * t / length,
                                            y: previous.y + (next.y - previous.y) * t / length,
                                        };
                                        if ink {
                                            outline.move_to(screen(at(cursor)));
                                            outline.line_to(screen(at(cursor + step)));
                                        }
                                        cursor += step;
                                        phase = (phase + step) % 7.;
                                    }
                                }
                                previous = *next;
                            }
                        } else {
                            outline.move_to(screen(*first));
                            for p in &points[1..] {
                                outline.line_to(screen(*p));
                            }
                            outline.close();
                        }
                        if let Ok(outline) = outline.build() {
                            w.paint_path(
                                outline,
                                rgb(if draft || live { 0xffd34d } else { 0x39e0b6 }),
                            );
                        }
                    }
                },
            )
            .absolute()
            .left_0()
            .top_0()
            .size_full()
            .into_any_element(),
        )
    }
}
