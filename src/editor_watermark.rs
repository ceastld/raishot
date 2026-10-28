//! 水印实时编辑只提交 Annotation 元数据；预编辑独立于可撤销文档。
use super::Editor;
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    document::ObjectId,
    watermark_edit::{choose_target, BindingToken, EditKind, PreparedEdit},
};
use std::time::Duration;

impl Editor {
    pub(crate) fn activate_watermark(&mut self, preferred: Option<ObjectId>) {
        let ids: Vec<_> = self
            .document
            .items()
            .iter()
            .filter(|o| o.annotation.tool == Tool::Watermark)
            .map(|o| o.id)
            .collect();
        let target = choose_target(preferred, self.watermark.target(), &ids);
        let text = target
            .and_then(|id| self.document.get(id))
            .map_or("", |a| a.text.as_str());
        if let Some(a) = target
            .and_then(|id| self.document.get(id))
            .filter(|_| self.tool == Tool::Watermark)
        {
            self.style = a.style;
        }
        self.watermark.bind(target, text);
        self.watermark_document_revision = self.document.revision();
    }

    /// 水印角度入口的当前值；还没有水印对象时为 None。
    pub(crate) fn watermark_rotation(&self) -> Option<f32> {
        self.watermark
            .target()
            .and_then(|id| self.document.get(id))
            .map(|a| a.rotation)
    }

    /// 水印不进入选择框：角度直接改文档里绑定的水印，绕画布中心转整张平铺，一次一步撤销。
    /// `None` 表示归零。
    pub(crate) fn rotate_watermark(&mut self, degrees: Option<f32>) {
        self.finish_watermark_input();
        let Some(id) = self.watermark.target() else {
            return;
        };
        let Some(mut a) = self.document.get(id).cloned() else {
            return;
        };
        let delta = degrees.unwrap_or(-a.rotation);
        if delta.abs() < 0.0001 {
            return;
        }
        let center = crate::transform::center(a.local_bounds());
        a.rotate_about(center, delta);
        self.document.replace_many(&[(id, a)]);
        self.activate_watermark(Some(id));
        self.preview_revision += 1;
    }

    pub(crate) fn sync_watermark_from_document(&mut self) {
        if self.watermark_document_revision != self.document.revision() {
            self.activate_watermark(None);
            self.preview_revision += 1;
        }
    }

    pub(crate) fn finish_watermark_input(&mut self) {
        let binding = self.watermark.binding();
        if self.watermark.has_preview() {
            let _ = self.watermark.cancel_preview(binding);
            self.preview_revision += 1;
        }
        self.watermark.break_history();
    }

    pub(crate) fn preview_watermark_text(
        &mut self,
        binding: BindingToken,
        text: &str,
        composing: bool,
    ) {
        let result = if composing {
            self.watermark.set_preview(binding, text)
        } else {
            self.watermark.cancel_preview(binding)
        };
        if result.is_ok() {
            self.preview_revision += 1;
        }
    }

    pub(crate) fn set_watermark_text(&mut self, binding: BindingToken, text: &str) -> bool {
        self.set_watermark_text_at(binding, text, self.watermark_clock.elapsed())
    }

    pub(crate) fn set_watermark_text_at(
        &mut self,
        binding: BindingToken,
        text: &str,
        now: Duration,
    ) -> bool {
        // 其它命令、Undo 和文档替换后必须先回填控件，再接受新的输入。
        self.sync_watermark_from_document();
        let Ok(prepared) = self.watermark.prepare_text(binding, text, now) else {
            return false;
        };
        let Some(prepared) = prepared else {
            self.preview_watermark_text(binding, text, false);
            return false;
        };
        let mut annotation = self.watermark_annotation();
        annotation.text = prepared.text.as_ref().expect("text edit has text").clone();
        self.commit_watermark(prepared, annotation)
    }

    pub(crate) fn set_watermark_style(&mut self, style: Style) {
        self.set_watermark_style_at(style, self.watermark_clock.elapsed());
    }
    pub(crate) fn set_watermark_style_at(&mut self, style: Style, now: Duration) {
        self.sync_watermark_from_document();
        let mut annotation = self.watermark_annotation();
        let changed = annotation.style != style;
        annotation.style = style;
        self.style = style;
        self.tool_styles.insert(Tool::Watermark, style);
        let Ok(Some(prepared)) =
            self.watermark
                .prepare_style(self.watermark.binding(), changed, now)
        else {
            return;
        };
        self.commit_watermark(prepared, annotation);
    }

    fn commit_watermark(&mut self, prepared: PreparedEdit, annotation: Annotation) -> bool {
        if !self.watermark.accepts(&prepared) {
            return false;
        }
        let created = match prepared.kind {
            EditKind::Create => Some(
                self.document
                    .insert_at(annotation, self.watermark_layer_index()),
            ),
            EditKind::Replace => {
                let Some(id) = self.watermark.target() else {
                    return false;
                };
                if self
                    .document
                    .get(id)
                    .is_none_or(|a| a.tool != Tool::Watermark)
                {
                    return false;
                }
                self.document
                    .replace_watermark_grouped(id, annotation, prepared.history);
                None
            }
        };
        self.watermark
            .acknowledge(prepared, created)
            .expect("synchronous checked watermark commit");
        self.watermark_document_revision = self.document.revision();
        self.preview_revision += 1;
        true
    }

    fn watermark_layer_index(&self) -> usize {
        // Spotlight 在 Scene 中于第一个孔建立唯一遮罩；水印紧随该共享层。
        self.document
            .items()
            .iter()
            .position(|o| o.annotation.tool == Tool::Spotlight)
            .map_or(0, |index| index + 1)
    }

    fn watermark_annotation(&self) -> Annotation {
        if let Some(a) = self.watermark.target().and_then(|id| self.document.get(id)) {
            return a.clone();
        }
        let mut a = Annotation::new(
            Tool::Watermark,
            Point {
                x: self.canvas.x as f32,
                y: self.canvas.y as f32,
            },
            self.style,
        );
        a.points.push(Point {
            x: (self.canvas.x + self.canvas.width) as f32,
            y: (self.canvas.y + self.canvas.height) as f32,
        });
        a.text = self.watermark.committed_text().to_owned();
        a
    }

    pub(super) fn apply_watermark_preview(&self, items: &mut Vec<Annotation>) {
        if !self.watermark.has_preview() {
            return;
        }
        let mut a = self.watermark_annotation();
        a.text = self.watermark.display_text().to_owned();
        if let Some(index) = self
            .watermark
            .target()
            .and_then(|id| self.document.items().iter().position(|o| o.id == id))
        {
            items[index] = a;
        } else if !a.text.trim().is_empty() {
            items.insert(self.watermark_layer_index().min(items.len()), a);
        }
    }
}
