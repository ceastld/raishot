//! 文字定位适配：光标、高亮和鼠标命中使用成品相同的排版结果。
use crate::{annotation::Annotation, text_layout::TextLayout};
use gpui::{point, px, size, Bounds, Pixels, Point};
use std::ops::Range;
pub struct TextGeometry {
    layout: TextLayout,
    text: String,
    scale: f32,
}
impl TextGeometry {
    pub fn new(a: &Annotation, scale: f32) -> crate::Result<Self> {
        Ok(Self {
            layout: TextLayout::new(a)?,
            text: a.text.clone(),
            scale,
        })
    }
    pub fn hit(&self, p: Point<Pixels>) -> usize {
        let index = self.layout.hit(crate::annotation::Point {
            x: f32::from(p.x) * self.scale,
            y: f32::from(p.y) * self.scale,
        });
        crate::text_input::edit_boundary::floor(&self.text, index)
    }
    pub fn visual_line_at(&self, p: Point<Pixels>) -> Option<Range<usize>> {
        self.layout
            .visual_line_at(crate::annotation::Point {
                x: f32::from(p.x) * self.scale,
                y: f32::from(p.y) * self.scale,
            })
            .map(|range| {
                crate::text_input::edit_boundary::floor(&self.text, range.start)
                    ..crate::text_input::edit_boundary::ceil(&self.text, range.end)
            })
    }
    pub fn caret(&self, index: usize, origin: Point<Pixels>) -> Bounds<Pixels> {
        let (p, h) = self.layout.position(index);
        Bounds::new(
            origin + point(px(p.x / self.scale), px(p.y / self.scale)),
            size(px(1.5), px(h / self.scale)),
        )
    }
    pub fn selection(&self, range: Range<usize>, origin: Point<Pixels>) -> Vec<Bounds<Pixels>> {
        self.layout
            .selection(range)
            .into_iter()
            .map(|(p, w, h)| {
                Bounds::new(
                    origin + point(px(p.x / self.scale), px(p.y / self.scale)),
                    size(px(w / self.scale), px(h / self.scale)),
                )
            })
            .collect()
    }
}
