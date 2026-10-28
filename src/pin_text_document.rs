//! 矢量文字贴图的可重绘来源。磁盘字段独立于 Annotation，不保存机器内字体索引；
//! 颜色统一按浅色主题基准保存，打开时再映射到当前主题。
use crate::pin_raster_document::rich_wire::WireRichText;
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    editor::Editor,
    geometry::Rect,
    text_format::{Alignment, FontFamily, TextStyle},
    Result,
};
use serde::{Deserialize, Serialize};

#[path = "pin_text_history.rs"]
mod history;

#[path = "pin_mixed_text_document.rs"]
mod mixed;

const MAX_OBJECTS: usize = 64;
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct WireTextDocument {
    #[serde(rename = "version")]
    pub version: u32,
    #[serde(rename = "width")]
    pub width: u32,
    #[serde(rename = "height")]
    pub height: u32,
    #[serde(rename = "objects")]
    objects: Vec<WireTextObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history: Option<history::WireTextHistory>,
    // version 2 复用矢量对象与真实增量 DTO；仍无底图或 PNG 资产。
    #[serde(
        default,
        rename = "mixedDocument",
        skip_serializing_if = "Option::is_none"
    )]
    mixed_document: Option<crate::pin_raster_document::WireRasterDocument>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct WireTextObject {
    #[serde(rename = "text")]
    text: String,
    #[serde(rename = "start")]
    start: WirePoint,
    #[serde(rename = "end")]
    end: WirePoint,
    #[serde(rename = "rotation")]
    rotation: f32,
    #[serde(rename = "style")]
    style: WireTextStyle,
    #[serde(default, rename = "rich", skip_serializing_if = "Option::is_none")]
    rich: Option<WireRichText>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct WirePoint {
    #[serde(rename = "x")]
    x: f32,
    #[serde(rename = "y")]
    y: f32,
}
impl From<Point> for WirePoint {
    fn from(p: Point) -> Self {
        Self { x: p.x, y: p.y }
    }
}
impl From<WirePoint> for Point {
    fn from(p: WirePoint) -> Self {
        Self { x: p.x, y: p.y }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct WireTextStyle {
    #[serde(rename = "color")]
    color: u32,
    #[serde(rename = "width")]
    width: f32,
    #[serde(rename = "opacity")]
    opacity: f32,
    #[serde(rename = "fontSize")]
    font_size: f32,
    #[serde(rename = "bold")]
    bold: bool,
    #[serde(rename = "fontName")]
    font_name: String,
    #[serde(rename = "italic")]
    italic: bool,
    #[serde(rename = "underline")]
    underline: bool,
    #[serde(rename = "strike")]
    strike: bool,
    #[serde(rename = "alignment")]
    alignment: String,
    #[serde(rename = "outline")]
    outline: bool,
    #[serde(rename = "outlineColor")]
    outline_color: u32,
    #[serde(rename = "outlineWidth")]
    outline_width: f32,
    #[serde(rename = "background")]
    background: bool,
    #[serde(rename = "backgroundColor")]
    background_color: u32,
    #[serde(rename = "backgroundOpacity")]
    background_opacity: f32,
    #[serde(rename = "backgroundRadius")]
    background_radius: f32,
    // 不参与文字渲染的 Style 字段也参与真实 Change 等值，历史不能默认化它们。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extras: Option<WireTextStyleExtras>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTextStyleExtras {
    block_size: u32,
    filled: bool,
    radius: f32,
    arrow_head: String,
    arrow_start: bool,
    arrow_end: bool,
    dashed: bool,
    pencil_smooth: bool,
    polyline_smooth: bool,
    highlighter_blend: String,
    highlighter_shape: String,
}
impl From<Style> for WireTextStyleExtras {
    fn from(s: Style) -> Self {
        Self {
            block_size: s.block_size,
            filled: s.filled,
            radius: s.radius,
            arrow_head: match s.arrow_head {
                crate::arrow_geometry::ArrowHead::Open => "open",
                crate::arrow_geometry::ArrowHead::Triangle => "triangle",
                crate::arrow_geometry::ArrowHead::Dart => "dart",
            }
            .into(),
            arrow_start: s.arrow_start,
            arrow_end: s.arrow_end,
            dashed: s.dashed,
            pencil_smooth: s.pencil_smooth,
            polyline_smooth: s.polyline_smooth,
            highlighter_blend: match s.highlighter_blend {
                crate::annotation::HighlighterBlend::Alpha => "alpha",
                crate::annotation::HighlighterBlend::Multiply => "multiply",
            }
            .into(),
            highlighter_shape: match s.highlighter_shape {
                crate::annotation::HighlighterShape::Freehand => "freehand",
                crate::annotation::HighlighterShape::Line => "line",
            }
            .into(),
        }
    }
}
impl WireTextStyleExtras {
    fn validate(&self) -> Result<()> {
        if !self.radius.is_finite()
            || !(0. ..=65535.).contains(&self.radius)
            || !matches!(self.arrow_head.as_str(), "open" | "triangle" | "dart")
            || !matches!(self.highlighter_blend.as_str(), "alpha" | "multiply")
            || !matches!(self.highlighter_shape.as_str(), "freehand" | "line")
        {
            return Err("纯文字贴图的完整样式字段无效".into());
        }
        Ok(())
    }
    fn apply(&self, s: &mut Style) {
        s.block_size = self.block_size;
        s.filled = self.filled;
        s.radius = self.radius;
        s.arrow_head = match self.arrow_head.as_str() {
            "open" => crate::arrow_geometry::ArrowHead::Open,
            "dart" => crate::arrow_geometry::ArrowHead::Dart,
            _ => crate::arrow_geometry::ArrowHead::Triangle,
        };
        s.arrow_start = self.arrow_start;
        s.arrow_end = self.arrow_end;
        s.dashed = self.dashed;
        s.pencil_smooth = self.pencil_smooth;
        s.polyline_smooth = self.polyline_smooth;
        s.highlighter_blend = if self.highlighter_blend == "multiply" {
            crate::annotation::HighlighterBlend::Multiply
        } else {
            crate::annotation::HighlighterBlend::Alpha
        };
        s.highlighter_shape = if self.highlighter_shape == "line" {
            crate::annotation::HighlighterShape::Line
        } else {
            crate::annotation::HighlighterShape::Freehand
        };
    }
}
impl From<Style> for WireTextStyle {
    fn from(s: Style) -> Self {
        Self {
            color: s.color,
            width: s.width,
            opacity: s.opacity,
            font_size: s.font_size,
            bold: s.bold,
            font_name: s.text.font.name().into(),
            italic: s.text.italic,
            underline: s.text.underline,
            strike: s.text.strike,
            alignment: match s.text.alignment {
                Alignment::Left => "left",
                Alignment::Center => "center",
                Alignment::Right => "right",
            }
            .into(),
            outline: s.text.outline,
            outline_color: s.text.outline_color,
            outline_width: s.text.outline_width,
            background: s.text.background,
            background_color: s.text.background_color,
            background_opacity: s.text.background_opacity,
            background_radius: s.text.background_radius,
            extras: Some(s.into()),
        }
    }
}

fn named_font(name: &str) -> FontFamily {
    if name.eq_ignore_ascii_case("Microsoft YaHei UI") {
        FontFamily::default()
    } else {
        FontFamily::named(name)
    }
}
impl WireTextObject {
    fn from_annotation(a: &Annotation) -> Result<Self> {
        if a.tool != Tool::Text
            || a.points.len() != 2
            || !matches!(
                a.fill_geometry,
                crate::annotation::LocalFillGeometry::Stroke
            )
            || a.effect_regions.is_some()
        {
            return Err("纯文字历史包含非文字对象或复合效果".into());
        }
        let object = Self {
            text: a.text.clone(),
            start: a.points[0].into(),
            end: a.points[1].into(),
            rotation: a.rotation,
            style: a.style.into(),
            rich: a.rich.as_deref().map(WireRichText::from_model),
        };
        object.validate()?;
        Ok(object)
    }
    fn validate(&self) -> Result<()> {
        let s = &self.style;
        let in_range = |v: f32, low: f32, high: f32| v.is_finite() && (low..=high).contains(&v);
        if ![
            self.start.x,
            self.start.y,
            self.end.x,
            self.end.y,
            self.rotation,
        ]
        .iter()
        .all(|v| v.is_finite() && v.abs() <= 1_000_000.)
            || s.color > 0xffffff
            || s.outline_color > 0xffffff
            || s.background_color > 0xffffff
            || !in_range(s.width, 0., 4096.)
            || !in_range(s.font_size, 1., 16384.)
            || !in_range(s.opacity, 0., 1.)
            || !in_range(s.outline_width, 0., 4096.)
            || !in_range(s.background_radius, 0., 65535.)
            || !in_range(s.background_opacity, 0., 1.)
            || s.font_name.is_empty()
            || s.font_name.len() > 256
            || s.font_name.chars().any(char::is_control)
            || !matches!(s.alignment.as_str(), "left" | "center" | "right")
        {
            return Err("纯文字贴图的坐标、字体或样式无效".into());
        }
        if let Some(extras) = &s.extras {
            extras.validate()?;
        }
        if let Some(rich) = &self.rich {
            rich.validate(&self.text)?;
        }
        Ok(())
    }
    /// `rich_font` 解析局部样式里的字体名，与对象字体同一身份规则。
    fn model_with_fonts(
        &self,
        font: FontFamily,
        rich_font: &mut dyn FnMut(&str) -> FontFamily,
    ) -> Result<Annotation> {
        let s = &self.style;
        let mut style = Style {
            color: s.color,
            width: s.width,
            opacity: s.opacity,
            font_size: s.font_size,
            bold: s.bold,
            text: TextStyle {
                font,
                italic: s.italic,
                underline: s.underline,
                strike: s.strike,
                alignment: match s.alignment.as_str() {
                    "center" => Alignment::Center,
                    "right" => Alignment::Right,
                    _ => Alignment::Left,
                },
                outline: s.outline,
                outline_color: s.outline_color,
                outline_width: s.outline_width,
                background: s.background,
                background_color: s.background_color,
                background_opacity: s.background_opacity,
                background_radius: s.background_radius,
            },
            ..Default::default()
        };
        if let Some(extras) = &s.extras {
            extras.apply(&mut style);
        }
        let mut a = Annotation::new(Tool::Text, self.start.into(), style);
        a.update(self.end.into());
        a.text = self.text.clone();
        a.rotation = self.rotation;
        a.rich = self
            .rich
            .as_ref()
            .map(|r| r.model(&self.text, rich_font).map(std::sync::Arc::new))
            .transpose()?;
        Ok(a)
    }
}

impl WireTextDocument {
    /// 只克隆文字元数据；light 为画布当前主题，保存前把默认文字色还原为浅色基准。
    #[cfg(test)]
    pub(crate) fn from_text_editor(editor: &Editor, size: (u32, u32), light: bool) -> Option<Self> {
        Self::try_from_text_editor(editor, size, light).ok()
    }

    /// 保存宿主应使用 Result 入口；损坏或容量失败不能被 None 解释为烘焙回退。
    pub(crate) fn try_from_text_editor(
        editor: &Editor,
        size: (u32, u32),
        light: bool,
    ) -> Result<Self> {
        editor.document.ensure_no_long_clip()?;
        let full = Rect::full(size.0, size.1);
        let items = editor.document.items();
        if editor.canvas != full
            || editor.crop != Some(full)
            || editor.document.crop_mask.is_some()
            || editor.lasso_draft.is_some()
            || editor.text_draft.is_some()
            || editor.is_dragging()
            || editor.has_pending_drawing()
            || editor.watermark.has_preview()
            || items.len() > MAX_OBJECTS
            || items.iter().map(|o| o.annotation.text.len()).sum::<usize>() > MAX_TEXT_BYTES
            || items.iter().any(|o| {
                o.annotation.tool != Tool::Text
                    || o.annotation.points.len() != 2
                    || !matches!(
                        o.annotation.fill_geometry,
                        crate::annotation::LocalFillGeometry::Stroke
                    )
                    || o.annotation.effect_regions.is_some()
            })
        {
            return Err("纯文字贴图快照需要完整纯文字画布和已完成的草稿".into());
        }
        let snapshot = editor.document.bounded_history_snapshot_mapped(|a| {
            crate::pin_text_source::remap_theme(a, light, true);
        })?;
        let mut doc = Self {
            version: 1,
            width: size.0,
            height: size.1,
            objects: snapshot
                .items
                .iter()
                .map(|o| WireTextObject::from_annotation(&o.annotation))
                .collect::<Result<Vec<_>>>()?,
            history: Some(history::WireTextHistory::from_snapshot(&snapshot)?),
            mixed_document: None,
        };
        doc.version = doc.pure_version();
        history::fit_budget(&mut doc)?;
        doc.validate()?;
        Ok(doc)
    }

    /// 文字来源宿主的新保存入口：纯文字沿用 version 1，混合标注使用 version 2。
    /// 分类覆盖当前和两栈，不能因当前已删掉普通标记而丢失可撤销的混合内容。
    pub(crate) fn try_from_vector_text_editor(
        editor: &Editor,
        size: (u32, u32),
        light: bool,
    ) -> Result<Self> {
        let snapshot =
            crate::pin_raster_document::WireRasterDocument::trusted_vector_text_editor_snapshot(
                editor,
            )?;
        if mixed::contains_mixed(&snapshot) {
            mixed::from_editor(editor, size, light)
        } else {
            Self::try_from_text_editor(editor, size, light)
        }
    }

    /// 已验证文档的当前文字；混合标注只读当前对象，不解码历史与字体。
    pub(crate) fn current_texts(&self) -> impl Iterator<Item = &str> {
        self.objects.iter().map(|object| object.text.as_str()).chain(
            self.mixed_document
                .iter()
                .flat_map(|document| document.current_texts()),
        )
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_shape()?;
        if self.version == 2 {
            mixed::validate(self)?;
        } else {
            self.decode_document(true)?;
        }
        Ok(())
    }
    /// 纯文字：当前或两栈任一对象带局部样式时写 3，让不认识局部样式的旧版明确拒读。
    /// 混合文档的局部样式由内层矢量文档的版本表达。
    fn pure_version(&self) -> u32 {
        if self.objects.iter().any(|o| o.rich.is_some())
            || self
                .history
                .as_ref()
                .is_some_and(history::WireTextHistory::uses_rich)
        {
            3
        } else {
            1
        }
    }
    fn validate_shape(&self) -> Result<()> {
        if self.version == 2 {
            return mixed::validate_shape(self);
        }
        if self.version != self.pure_version()
            || self.mixed_document.is_some()
            || self.width == 0
            || self.height == 0
            || self.width > 65535
            || self.height > 65535
            || u64::from(self.width) * u64::from(self.height) > 32_000_000
            || (self.objects.is_empty() && self.history.is_none())
            || self.objects.len() > MAX_OBJECTS
            || self.objects.iter().map(|o| o.text.len()).sum::<usize>() > MAX_TEXT_BYTES
        {
            return Err("纯文字贴图来源版本、尺寸或内容超过限制".into());
        }
        for o in &self.objects {
            o.validate()?;
        }
        Ok(())
    }

    fn decode_document(&self, light: bool) -> Result<crate::document::Document> {
        if self.version == 2 {
            mixed::decode(self, light)
        } else {
            history::decode(self, light)
        }
    }

    /// 在改变目标前完成原始语义及当前主题／字体映射，安装过程不生成用户命令。
    pub(crate) fn restore_into(&self, editor: &mut Editor, light: bool) -> Result<()> {
        self.validate_shape()?;
        editor.document.ensure_no_long_clip()?;
        if editor.canvas != Rect::full(self.width, self.height)
            || !editor.document.items().is_empty()
            || editor.document.crop_mask.is_some()
            || editor.document.can_undo()
            || editor.document.can_redo()
            || editor.text_draft.is_some()
            || editor.lasso_draft.is_some()
            || editor.is_dragging()
            || editor.has_pending_drawing()
            || editor.watermark.has_preview()
        {
            return Err("纯文字贴图恢复目标不是同尺寸空画布".into());
        }
        let document = self.decode_document(light)?;
        editor.document = document;
        editor.crop = Some(Rect::full(self.width, self.height));
        Ok(())
    }
    /// 宿主按整清单剩余容量，仅裁当前记录最远的历史。
    pub(crate) fn trim_oldest_history(&mut self) -> bool {
        // 宿主须先完整 validate，不能以任何版本的容量裁剪消除语义错误。
        if self.version == 2 {
            return self
                .mixed_document
                .as_mut()
                .is_some_and(crate::pin_raster_document::WireRasterDocument::trim_oldest_history);
        }
        let trimmed = self
            .history
            .as_mut()
            .is_some_and(history::WireTextHistory::pop_oldest);
        if trimmed {
            self.version = self.pure_version();
        }
        trimmed
    }

    #[cfg(test)]
    pub(crate) fn annotations(&self) -> Result<Vec<Annotation>> {
        self.validate()?;
        if self.version == 2 {
            return Ok(self
                .decode_document(true)?
                .items()
                .iter()
                .map(|object| object.annotation.clone())
                .collect());
        }
        Ok(self
            .objects
            .iter()
            .map(|o| o.model_with_fonts(named_font(&o.style.font_name), &mut named_font))
            .collect::<Result<_>>()?)
    }
}

#[cfg(test)]
#[path = "pin_mixed_text_document_tests.rs"]
mod mixed_tests;

#[cfg(test)]
#[path = "pin_mixed_text_host_theme_tests.rs"]
mod pin_mixed_text_host_theme_tests;

#[cfg(test)]
#[path = "pin_text_undo_tests.rs"]
mod undo_tests;

#[cfg(test)]
#[path = "rich_text_store_tests.rs"]
mod rich_text_store_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let full = Rect::full(30, 20);
        let mut e = Editor::new(full, Some(full));
        let mut a = Annotation::new(Tool::Text, Point { x: 2., y: 3. }, Style::default());
        a.update(Point { x: 29., y: 19. });
        a.text = "中文  a\t🙂\r\n第二行".into();
        a.rotation = 30.;
        a.style.text.italic = true;
        a.style.text.outline = true;
        a.style.text.background = true;
        a.style.text.alignment = Alignment::Right;
        e.document.insert(a);
        e
    }

    #[test]
    fn pin_text_document_wire_preserves_unicode_layout_and_named_font() {
        let e = editor();
        let doc = WireTextDocument::from_text_editor(&e, (30, 20), true).unwrap();
        let wire = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            wire["objects"][0]["style"]["fontName"],
            "Microsoft YaHei UI"
        );
        assert!(wire["objects"][0]["style"].get("fontIndex").is_none());
        let restored: WireTextDocument = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, doc);
        assert_eq!(
            restored.annotations().unwrap()[0],
            e.document.items()[0].annotation
        );
    }

    #[test]
    fn pin_text_document_rejects_draft_mixed_and_cropped_sources() {
        let mut e = editor();
        e.text_draft = Some(crate::editor::TextDraft {
            id: None,
            annotation: e.document.items()[0].annotation.clone(),
        });
        assert!(WireTextDocument::from_text_editor(&e, (30, 20), true).is_none());
        e.text_draft = None;
        e.crop = Some(Rect::full(29, 20));
        assert!(WireTextDocument::from_text_editor(&e, (30, 20), true).is_none());
        e.crop = Some(Rect::full(30, 20));
        e.document.insert(Annotation::new(
            Tool::Rectangle,
            Point { x: 1., y: 1. },
            Style::default(),
        ));
        assert!(WireTextDocument::from_text_editor(&e, (30, 20), true).is_none());
    }

    #[test]
    fn pin_text_document_saves_dark_theme_default_color_as_light_baseline() {
        let (_, light_fg) = crate::pin_text_source::colors(true);
        let (_, dark_fg) = crate::pin_text_source::colors(false);
        let mut e = editor();
        let id = e.document.items()[0].id;
        let mut dark = e.document.items()[0].annotation.clone();
        dark.style.color = dark_fg;
        e.document.replace(id, dark);
        let doc = WireTextDocument::from_text_editor(&e, (30, 20), false).unwrap();
        assert_eq!(doc.annotations().unwrap()[0].style.color, light_fg);
    }

    #[test]
    fn pin_text_document_rejects_invalid_disk_metadata_before_rebuilding() {
        let e = editor();
        let mut doc = WireTextDocument::from_text_editor(&e, (30, 20), true).unwrap();
        doc.objects[0].style.opacity = f32::NAN;
        assert!(doc.annotations().is_err());
        doc.objects[0].style.opacity = 1.;
        doc.width = 65536;
        assert!(doc.validate().is_err());
    }

    fn nested_text_target() -> Editor {
        Editor::new(Rect::full(160, 120), Some(Rect::full(160, 120)))
    }

    fn nested_text_source() -> Editor {
        let mut source = nested_text_target();
        let ids = source.document.insert_many(
            (0..4)
                .map(|index| {
                    let mut text = Annotation::new(
                        Tool::Text,
                        Point {
                            x: 8. + index as f32 * 32.,
                            y: 12.,
                        },
                        Style {
                            color: if index == 0 {
                                crate::pin_text_source::colors(true).1
                            } else {
                                0x287358 + index as u32
                            },
                            radius: 8.,
                            ..Style::default()
                        },
                    );
                    text.update(Point {
                        x: 32. + index as f32 * 32.,
                        y: 45.,
                    });
                    text.text = format!("文字组 {index} 🙂\n第二行");
                    text
                })
                .collect(),
        );
        assert!(source.document.set_group(&ids[..2], true));
        assert!(source.document.set_group(&ids[2..], true));
        assert!(source.document.set_group(&[ids[0], ids[2]], true));
        let mut text = source.document.get(ids[0]).unwrap().clone();
        text.text.push_str(" 修改");
        source.document.replace(ids[0], text.clone());
        source.document.insert(text);
        source.document.undo();
        assert!(source.document.can_undo() && source.document.can_redo());
        source
    }

    fn nested_assert_bad_text(encoded: serde_json::Value) {
        let wire: WireTextDocument = serde_json::from_value(encoded).unwrap();
        let mut target = nested_text_target();
        let before = target.document.bounded_history_snapshot().unwrap();
        assert!(wire.validate().is_err());
        assert!(wire.restore_into(&mut target, false).is_err());
        assert_eq!(target.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(target.crop, Some(Rect::full(160, 120)));
        assert_eq!(target.document.revision(), 0);
        assert_eq!(target.document.edit_revision(), 0);
    }

    #[test]
    fn nested_pure_text_history_roundtrips_full_membership_and_theme_without_mixed_promotion() {
        let source = nested_text_source();
        let wire =
            WireTextDocument::try_from_vector_text_editor(&source, (160, 120), true).unwrap();
        let encoded = serde_json::to_value(&wire).unwrap();
        assert_eq!(encoded["version"], 1, "组元数据不把纯文字升级为混合画布");
        assert_eq!(encoded["history"]["version"], 2);
        assert_eq!(
            encoded["history"]["nestedGroups"].as_array().unwrap().len(),
            source.document.items().len()
        );
        assert!(encoded.get("mixedDocument").is_none());
        let decoded: WireTextDocument =
            serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        for light in [true, false] {
            let mut restored = nested_text_target();
            decoded.restore_into(&mut restored, light).unwrap();
            let expected = source
                .document
                .bounded_history_snapshot_mapped(|a| {
                    crate::pin_text_source::remap_theme(a, true, light);
                })
                .unwrap();
            assert_eq!(
                restored.document.bounded_history_snapshot().unwrap(),
                expected
            );
            assert_eq!(restored.document.revision(), 0);
            assert_eq!(restored.document.edit_revision(), 0);
            assert_eq!(
                WireTextDocument::try_from_vector_text_editor(&restored, (160, 120), light)
                    .unwrap(),
                wire
            );
            let mut expected_history =
                crate::document::Document::from_bounded_history_snapshot(expected).unwrap();
            while expected_history.can_undo() {
                expected_history.undo();
                restored.document.undo();
                assert_eq!(restored.document.items(), expected_history.items());
            }
            while expected_history.can_redo() {
                expected_history.redo();
                restored.document.redo();
                assert_eq!(restored.document.items(), expected_history.items());
            }
        }
        let mut trimmed = decoded;
        while trimmed.trim_oldest_history() {
            trimmed.validate().unwrap();
        }
        let encoded = serde_json::to_value(&trimmed).unwrap();
        assert_eq!(encoded["history"]["version"], 2);
        let mut restored = nested_text_target();
        trimmed.restore_into(&mut restored, true).unwrap();
        assert_eq!(restored.document.items(), source.document.items());
        assert!(!restored.document.can_undo() && !restored.document.can_redo());
    }

    #[test]
    fn nested_pure_text_invalid_arrays_versions_and_oldest_paths_reject_before_theme_mapping() {
        let source = nested_text_source();
        let base = serde_json::to_value(
            WireTextDocument::try_from_vector_text_editor(&source, (160, 120), true).unwrap(),
        )
        .unwrap();
        let mut old_version = base.clone();
        old_version["history"]["version"] = serde_json::json!(1);
        nested_assert_bad_text(old_version);
        let mut short_array = base.clone();
        short_array["history"]["nestedGroups"]
            .as_array_mut()
            .unwrap()
            .pop();
        nested_assert_bad_text(short_array);
        let mut missing_array = base.clone();
        missing_array["history"]
            .as_object_mut()
            .unwrap()
            .remove("nestedGroups");
        nested_assert_bad_text(missing_array);
        let mut too_deep = base.clone();
        too_deep["history"]["nestedGroups"][0] = serde_json::json!((1..=16).collect::<Vec<u64>>());
        nested_assert_bad_text(too_deep);
        let mut clock = base.clone();
        clock["history"]["nextGroup"] = serde_json::json!(0);
        nested_assert_bad_text(clock);
        let nested_command = base["history"]["undo"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["kind"] == "nestedGroups")
            .unwrap();
        let mut oldest_path = base.clone();
        oldest_path["history"]["undo"][nested_command]["before"][0][1]["nestedGroups"] =
            serde_json::json!([2]);
        nested_assert_bad_text(oldest_path);
        // 真正写入序列化的Insert对象字段，不能向未知字段注入而被serde忽略。
        assert_eq!(base["history"]["redo"][0]["kind"], "insert");
        // insert(text)产生未分组对象，合法空路径省略；先确认真实Insert载荷。
        assert!(base["history"]["redo"][0]["object"]
            .get("annotation")
            .is_some());
        assert!(base["history"]["redo"][0]["object"]
            .get("nested_groups")
            .is_none());
        let mut redo = base.clone();
        redo["history"]["redo"][0]["object"]["nestedGroups"] = serde_json::json!([0]);
        let bad_redo: WireTextDocument = serde_json::from_value(redo.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(bad_redo).unwrap()["history"]["redo"][0]["object"]["nestedGroups"],
            serde_json::json!([0]),
            "坏路径必须进入真实DTO，不能被当成未知字段忽略"
        );
        nested_assert_bad_text(redo);
        let mut null_array = base;
        null_array["history"]["nestedGroups"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<WireTextDocument>(null_array).is_err());

        let mut flat = editor();
        let second = flat
            .document
            .insert(flat.document.items()[0].annotation.clone());
        let first = flat.document.items()[0].id;
        assert!(flat.document.set_group(&[first, second], true));
        let old = WireTextDocument::try_from_vector_text_editor(&flat, (30, 20), true).unwrap();
        let mut legacy = serde_json::to_value(&old).unwrap();
        assert_eq!(legacy["history"]["version"], 1);
        assert!(legacy["history"].get("nestedGroups").is_none());
        let mut restored = Editor::new(Rect::full(30, 20), Some(Rect::full(30, 20)));
        old.restore_into(&mut restored, true).unwrap();
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            flat.document.bounded_history_snapshot().unwrap()
        );
        legacy["history"]["nestedGroups"] = serde_json::json!([]);
        let explicit_empty: WireTextDocument = serde_json::from_value(legacy.clone()).unwrap();
        let mut restored = Editor::new(Rect::full(30, 20), Some(Rect::full(30, 20)));
        explicit_empty.restore_into(&mut restored, true).unwrap();
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            flat.document.bounded_history_snapshot().unwrap(),
            "显式空数组与缺字段均表示全部对象没有内层路径"
        );
        legacy.as_object_mut().unwrap().remove("history");
        let legacy: WireTextDocument = serde_json::from_value(legacy).unwrap();
        let mut restored = Editor::new(Rect::full(30, 20), Some(Rect::full(30, 20)));
        legacy.restore_into(&mut restored, true).unwrap();
        assert_eq!(restored.document.items().len(), flat.document.items().len());
        assert!(restored
            .document
            .items()
            .iter()
            .all(|o| o.group.is_none() && o.nested_groups.is_empty()));
        assert!(!restored.document.can_undo() && !restored.document.can_redo());
    }
}
