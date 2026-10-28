//! 截图贴图的有界文档快照。底图、图像处理与外观分别存储；字体按名字恢复。
use crate::{
    annotation::{
        Annotation, Bounds, EffectRegions, HighlighterBlend, HighlighterShape, LocalFillGeometry,
        Point, Style, Tool,
    },
    arrow_geometry::ArrowHead,
    crop_mask::Polygon,
    editor::Editor,
    geometry::Rect,
    text_format::{Alignment, FontFamily, TextStyle},
    Result,
};
use serde::{Deserialize, Serialize};

#[path = "pin_capture_source.rs"]
pub(crate) mod capture_source;
#[path = "pin_raster_history.rs"]
mod history;
#[path = "rich_text_wire.rs"]
pub(crate) mod rich_wire;
#[path = "pin_raster_text_bridge.rs"]
mod text_bridge;

const MAX_OBJECTS: usize = 1024;
const MAX_POINTS: usize = 65536;
const MAX_TEXT: usize = 256 * 1024;
pub(crate) const MAX_NESTED_GROUP_REFERENCES: usize = 32768;

/// 与核心 membership 分离的共用 Wire DTO；原始状态拓扑仍由 Document 验证。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireGroupMembership {
    pub(crate) group: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) nested_groups: Vec<u64>,
}
impl From<&crate::document::GroupMembership> for WireGroupMembership {
    fn from(membership: &crate::document::GroupMembership) -> Self {
        Self {
            group: membership.group,
            nested_groups: membership.nested_groups.clone(),
        }
    }
}
impl WireGroupMembership {
    pub(crate) fn model(&self) -> Result<crate::document::GroupMembership> {
        validate_group_path(self.group, &self.nested_groups)?;
        Ok(crate::document::GroupMembership {
            group: self.group,
            nested_groups: self.nested_groups.clone(),
        })
    }
}
pub(crate) fn validate_group_path(group: Option<u64>, inner: &[u64]) -> Result<()> {
    if inner.len() >= 16
        || group.is_none() && !inner.is_empty()
        || !inner.is_empty() && group == Some(0)
        || inner.contains(&0)
    {
        return Err("贴图嵌套组路径缺根、过深或包含零组号".into());
    }
    let mut ids = std::collections::HashSet::new();
    if group
        .into_iter()
        .chain(inner.iter().copied())
        .any(|id| !ids.insert(id))
    {
        return Err("贴图嵌套组路径包含重复祖先".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireRasterDocument {
    version: u32,
    width: u32,
    height: u32,
    crop: Option<[u32; 4]>,
    mask: Option<Vec<[f32; 2]>>,
    objects: Vec<WireObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history: Option<history::WireHistory>,
    #[serde(
        default,
        deserialize_with = "capture_source::deserialize_source",
        skip_serializing_if = "Option::is_none"
    )]
    capture_source: Option<capture_source::CaptureSourceMetadata>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireObject {
    group: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nested_groups: Vec<u64>,
    tool: String,
    points: Vec<[f32; 2]>,
    style: WireStyle,
    text: String,
    rotation: f32,
    fill_regions: Option<Vec<[f32; 4]>>,
    effect_regions: Option<Vec<[f32; 4]>>,
    effect_managed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rich: Option<rich_wire::WireRichText>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStyle {
    color: u32,
    width: f32,
    block_size: u32,
    opacity: f32,
    filled: bool,
    radius: f32,
    #[serde(default)]
    effect_radius: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rectangle_radii: Option<[f32; 4]>,
    font_size: f32,
    bold: bool,
    font_name: String,
    italic: bool,
    underline: bool,
    strike: bool,
    alignment: String,
    outline: bool,
    outline_color: u32,
    outline_width: f32,
    background: bool,
    background_color: u32,
    background_opacity: f32,
    background_radius: f32,
    arrow_head: String,
    arrow_start: bool,
    arrow_end: bool,
    dashed: bool,
    pencil_smooth: bool,
    polyline_smooth: bool,
    highlighter_blend: String,
    highlighter_shape: String,
}
impl From<Style> for WireStyle {
    fn from(s: Style) -> Self {
        Self {
            color: s.color,
            width: s.width,
            block_size: s.block_size,
            opacity: s.opacity,
            filled: s.filled,
            radius: s.radius,
            effect_radius: s.effect_radius,
            rectangle_radii: s.rectangle_radii,
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
            arrow_head: match s.arrow_head {
                ArrowHead::Open => "open",
                ArrowHead::Triangle => "triangle",
                ArrowHead::Dart => "dart",
            }
            .into(),
            arrow_start: s.arrow_start,
            arrow_end: s.arrow_end,
            dashed: s.dashed,
            pencil_smooth: s.pencil_smooth,
            polyline_smooth: s.polyline_smooth,
            highlighter_blend: match s.highlighter_blend {
                HighlighterBlend::Alpha => "alpha",
                HighlighterBlend::Multiply => "multiply",
            }
            .into(),
            highlighter_shape: match s.highlighter_shape {
                HighlighterShape::Freehand => "freehand",
                HighlighterShape::Line => "line",
            }
            .into(),
        }
    }
}
impl WireStyle {
    fn validate(&self) -> Result<()> {
        if ![
            self.width,
            self.opacity,
            self.radius,
            self.effect_radius,
            self.font_size,
            self.outline_width,
            self.background_opacity,
            self.background_radius,
        ]
        .iter()
        .all(|x| x.is_finite())
            || self.width < 0.
            || self.radius < 0.
            || self.effect_radius < 0.
            || self
                .rectangle_radii
                .is_some_and(|r| r.iter().any(|v| !v.is_finite() || *v < 0.))
            || self.font_size <= 0.
            || self.outline_width < 0.
            || self.background_radius < 0.
            || !(0. ..=1.).contains(&self.opacity)
            || !(0. ..=1.).contains(&self.background_opacity)
            || self.font_name.len() > 256
            || self.font_name.is_empty()
            || !matches!(self.alignment.as_str(), "left" | "center" | "right")
            || !matches!(self.arrow_head.as_str(), "open" | "triangle" | "dart")
            || !matches!(self.highlighter_blend.as_str(), "alpha" | "multiply")
            || !matches!(self.highlighter_shape.as_str(), "freehand" | "line")
        {
            return Err("截图贴图标注样式无效".into());
        }
        Ok(())
    }
    fn model(&self) -> Result<Style> {
        self.validate()?;
        self.model_with_font(FontFamily::named(&self.font_name))
    }
    fn model_with_font(&self, font: FontFamily) -> Result<Style> {
        self.validate()?;
        Ok(Style {
            color: self.color,
            width: self.width,
            block_size: self.block_size,
            opacity: self.opacity,
            filled: self.filled,
            radius: self.radius,
            effect_radius: self.effect_radius,
            rectangle_radii: self.rectangle_radii,
            font_size: self.font_size,
            bold: self.bold,
            text: TextStyle {
                font,
                italic: self.italic,
                underline: self.underline,
                strike: self.strike,
                alignment: match self.alignment.as_str() {
                    "left" => Alignment::Left,
                    "center" => Alignment::Center,
                    _ => Alignment::Right,
                },
                outline: self.outline,
                outline_color: self.outline_color,
                outline_width: self.outline_width,
                background: self.background,
                background_color: self.background_color,
                background_opacity: self.background_opacity,
                background_radius: self.background_radius,
            },
            arrow_head: match self.arrow_head.as_str() {
                "open" => ArrowHead::Open,
                "triangle" => ArrowHead::Triangle,
                _ => ArrowHead::Dart,
            },
            arrow_start: self.arrow_start,
            arrow_end: self.arrow_end,
            dashed: self.dashed,
            pencil_smooth: self.pencil_smooth,
            polyline_smooth: self.polyline_smooth,
            highlighter_blend: if self.highlighter_blend == "multiply" {
                HighlighterBlend::Multiply
            } else {
                HighlighterBlend::Alpha
            },
            highlighter_shape: if self.highlighter_shape == "line" {
                HighlighterShape::Line
            } else {
                HighlighterShape::Freehand
            },
        })
    }
}
fn tool_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Rectangle => "rectangle",
        Tool::Ellipse => "ellipse",
        Tool::Triangle => "triangle",
        Tool::Diamond => "diamond",
        Tool::Star => "star",
        Tool::Arrow => "arrow",
        Tool::Pen => "pen",
        Tool::Mosaic => "mosaic",
        Tool::Highlighter => "highlighter",
        Tool::HighlighterRect => "highlighterRect",
        Tool::Polyline => "polyline",
        Tool::Text => "text",
        Tool::Watermark => "watermark",
        Tool::Serial => "serial",
        Tool::Blur => "blur",
        Tool::MosaicBrush => "mosaicBrush",
        Tool::BlurBrush => "blurBrush",
        Tool::LocalFill => "localFill",
        Tool::Spotlight => "spotlight",
        Tool::PixelEraser => "pixelEraser",
        Tool::Crop => "crop",
        Tool::Edit => "edit",
        Tool::Eraser => "eraser",
        Tool::Lasso => "lasso",
        Tool::SmartMosaic => "smartMosaic",
    }
}
fn tool_model(name: &str) -> Result<Tool> {
    Ok(match name {
        "rectangle" => Tool::Rectangle,
        "ellipse" => Tool::Ellipse,
        "triangle" => Tool::Triangle,
        "diamond" => Tool::Diamond,
        "star" => Tool::Star,
        "arrow" => Tool::Arrow,
        "pen" => Tool::Pen,
        "mosaic" => Tool::Mosaic,
        "highlighter" => Tool::Highlighter,
        "highlighterRect" => Tool::HighlighterRect,
        "polyline" => Tool::Polyline,
        "text" => Tool::Text,
        "watermark" => Tool::Watermark,
        "serial" => Tool::Serial,
        "blur" => Tool::Blur,
        "mosaicBrush" => Tool::MosaicBrush,
        "blurBrush" => Tool::BlurBrush,
        "localFill" => Tool::LocalFill,
        "spotlight" => Tool::Spotlight,
        "pixelEraser" => Tool::PixelEraser,
        _ => return Err("截图贴图包含不支持的文档工具".into()),
    })
}
fn bounds_wire(b: Bounds) -> [f32; 4] {
    [b.x, b.y, b.width, b.height]
}
fn bounds_model(b: [f32; 4]) -> Bounds {
    Bounds {
        x: b[0],
        y: b[1],
        width: b[2],
        height: b[3],
    }
}
impl WireObject {
    fn from_annotation(a: &Annotation, group: Option<u64>, nested_groups: &[u64]) -> Self {
        Self {
            group,
            nested_groups: nested_groups.to_vec(),
            tool: tool_name(a.tool).into(),
            points: a.points.iter().map(|p| [p.x, p.y]).collect(),
            style: a.style.into(),
            text: a.text.clone(),
            rotation: a.rotation,
            fill_regions: match &a.fill_geometry {
                LocalFillGeometry::Stroke => None,
                LocalFillGeometry::Regions(r) => Some(r.iter().copied().map(bounds_wire).collect()),
            },
            effect_regions: a
                .effect_regions
                .as_ref()
                .map(|r| r.regions.iter().copied().map(bounds_wire).collect()),
            effect_managed: a.effect_regions.as_ref().is_some_and(|r| r.managed),
            rich: a.rich.as_deref().map(rich_wire::WireRichText::from_model),
        }
    }
    fn validate(&self) -> Result<()> {
        self.style.validate()?;
        if let Some(rich) = &self.rich {
            if self.tool != "text" {
                return Err("只有文字标注可以带局部样式".into());
            }
            rich.validate(&self.text)?;
        }
        validate_group_path(self.group, &self.nested_groups)?;
        tool_model(&self.tool)?;
        if !self.rotation.is_finite()
            || self.points.is_empty()
            || self.points.len() > MAX_POINTS
            || self.points.iter().flatten().any(|x| !x.is_finite())
        {
            return Err("截图贴图标注坐标无效".into());
        }
        for regions in [&self.fill_regions, &self.effect_regions]
            .into_iter()
            .flatten()
        {
            if regions.is_empty()
                || regions.len() > 4096
                || regions.iter().any(|r| {
                    r.iter().any(|x| !x.is_finite())
                        || r[2] <= 0.
                        || r[3] <= 0.
                        || !(r[0] + r[2]).is_finite()
                        || !(r[1] + r[3]).is_finite()
                })
            {
                return Err("截图贴图复合效果区域无效".into());
            }
        }
        if self.fill_regions.is_some() && self.tool != "localFill"
            || self.effect_regions.is_some() && !matches!(self.tool.as_str(), "mosaic" | "blur")
            || self.effect_managed && self.effect_regions.is_none()
        {
            return Err("截图贴图效果区域与工具不匹配".into());
        }
        Ok(())
    }
    fn model(&self) -> Result<Annotation> {
        self.validate()?;
        self.model_with_fonts(self.style.model()?.text.font, &mut FontFamily::named)
    }
    /// `rich_font` 解析局部样式里的字体名，与对象字体同一身份规则。
    fn model_with_fonts(
        &self,
        font: FontFamily,
        rich_font: &mut dyn FnMut(&str) -> FontFamily,
    ) -> Result<Annotation> {
        self.validate()?;
        let a = Annotation {
            tool: tool_model(&self.tool)?,
            points: self
                .points
                .iter()
                .map(|p| Point { x: p[0], y: p[1] })
                .collect(),
            style: self.style.model_with_font(font)?,
            text: self.text.clone(),
            rotation: self.rotation,
            fill_geometry: self
                .fill_regions
                .as_ref()
                .map_or(LocalFillGeometry::Stroke, |r| {
                    LocalFillGeometry::Regions(
                        r.iter()
                            .copied()
                            .map(bounds_model)
                            .collect::<Vec<_>>()
                            .into(),
                    )
                }),
            effect_regions: self.effect_regions.as_ref().map(|r| EffectRegions {
                regions: r
                    .iter()
                    .copied()
                    .map(bounds_model)
                    .collect::<Vec<_>>()
                    .into(),
                managed: self.effect_managed,
            }),
            rich: self
                .rich
                .as_ref()
                .map(|r| r.model(&self.text, rich_font).map(std::sync::Arc::new))
                .transpose()?,
        };
        if !a.valid() {
            return Err("截图贴图标注不能恢复为有效对象".into());
        }
        Ok(a)
    }
}
impl WireRasterDocument {
    pub(crate) fn empty(size: (u32, u32)) -> Result<Self> {
        let doc = Self {
            version: 1,
            width: size.0,
            height: size.1,
            crop: Some([0, 0, size.0, size.1]),
            mask: None,
            objects: Vec::new(),
            history: None,
            capture_source: None,
        };
        doc.validate()?;
        Ok(doc)
    }
    pub(crate) fn from_editor(editor: &Editor, size: (u32, u32)) -> Result<Self> {
        Self::from_editor_source(editor, size, None)
    }
    pub(crate) fn from_capture_editor(
        editor: &Editor,
        size: (u32, u32),
        source: capture_source::CaptureSourceMetadata,
    ) -> Result<Self> {
        Self::from_editor_source(editor, size, Some(source))
    }
    fn from_editor_source(
        editor: &Editor,
        size: (u32, u32),
        source: Option<capture_source::CaptureSourceMetadata>,
    ) -> Result<Self> {
        editor.document.ensure_no_long_clip()?;
        if editor.canvas != Rect::full(size.0, size.1)
            || editor.text_draft.is_some()
            || editor.lasso_draft.is_some()
            || editor.is_dragging()
            || editor.has_pending_drawing()
            || editor.watermark.has_preview()
        {
            return Err("截图贴图工作区快照需要完成内容草稿".into());
        }
        let snapshot = if source.is_some() {
            editor.document.exact_history_snapshot()?
        } else {
            editor.document.bounded_history_snapshot()?
        };
        let mut doc = Self {
            version: 1,
            width: size.0,
            height: size.1,
            crop: editor.crop.map(|r| [r.x, r.y, r.width, r.height]),
            mask: editor
                .document
                .crop_mask
                .as_ref()
                .map(|m| m.points.iter().map(|p| [p.x, p.y]).collect()),
            objects: editor
                .document
                .items()
                .iter()
                .map(|o| WireObject::from_annotation(&o.annotation, o.group, &o.nested_groups))
                .collect(),
            history: Some(history::WireHistory::from_snapshot(&snapshot)?),
            capture_source: source,
        };
        doc.version = doc.required_version();
        if doc.capture_source.is_some() {
            if serde_json::to_vec(&doc)?.len() > history::MAX_WIRE_BYTES {
                return Err("截图完整文档和两栈超过480 KiB；原截图已保留".into());
            }
        } else {
            history::fit_budget(&mut doc)?;
        }
        doc.validate()?;
        Ok(doc)
    }
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    /// 已验证文档的当前文字；列表预览不恢复字体、对象身份或撤销两栈。
    pub(crate) fn current_texts(&self) -> impl Iterator<Item = &str> {
        self.objects
            .iter()
            .filter(|object| object.tool == "text")
            .map(|object| object.text.as_str())
    }
    pub(crate) fn capture_source(&self) -> Option<&capture_source::CaptureSourceMetadata> {
        self.capture_source.as_ref()
    }
    /// 采样画布可以比可见来源大；PNG兼容帧与窗口使用固定contentRect尺寸。
    pub(crate) fn storage_size(&self) -> (u32, u32) {
        self.capture_source
            .as_ref()
            .map_or(self.size(), |source| source.storage_size())
    }
    /// 工作区按清单剩余容量仅裁当前记录的最早历史，当前对象及身份保持不变。
    pub(crate) fn trim_oldest_history(&mut self) -> bool {
        if self.capture_source.is_some() {
            return false;
        }
        if let Some(history) = &mut self.history {
            let trimmed = history.pop_oldest();
            if trimmed {
                history.refresh_version(&self.objects);
                self.version = self.required_version();
            }
            trimmed
        } else {
            false
        }
    }
    /// 当前或两栈任一对象带局部样式时写 2，让不认识局部样式的旧版明确拒读。
    fn required_version(&self) -> u32 {
        if self.objects.iter().any(|o| o.rich.is_some())
            || self
                .history
                .as_ref()
                .is_some_and(history::WireHistory::uses_rich)
        {
            2
        } else {
            1
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_shape()?;
        history::decode(self)?;
        Ok(())
    }
    fn validate_shape(&self) -> Result<()> {
        if self.version != self.required_version()
            || self.width == 0
            || self.height == 0
            || self.width > 65535
            || self.height > 65535
            || self.capture_source.is_none()
                && u64::from(self.width) * u64::from(self.height) > 32_000_000
            || self.objects.len() > MAX_OBJECTS
        {
            return Err("截图贴图文档版本、尺寸或对象数量无效".into());
        }
        if let Some(source) = &self.capture_source {
            if self.history.is_none() {
                return Err("截图完整来源缺少真实对象身份与两栈；未降级为旧平层文档".into());
            }
            source.validate()?;
            let bounds = source.bounds();
            if (bounds.width, bounds.height) != self.size() {
                return Err("截图文档与完整CPU采样域尺寸不一致".into());
            }
        }
        if self.crop.is_some_and(|r| {
            !Rect {
                x: r[0],
                y: r[1],
                width: r[2],
                height: r[3],
            }
            .valid(self.width, self.height)
        }) {
            return Err("截图贴图文档选区超出内容画布".into());
        }
        let points = self.objects.iter().try_fold(0_usize, |sum, o| {
            sum.checked_add(o.points.len()).ok_or("截图贴图点数溢出")
        })?;
        let text = self.objects.iter().try_fold(0_usize, |sum, o| {
            sum.checked_add(o.text.len()).ok_or("截图贴图文字长度溢出")
        })?;
        if points > MAX_POINTS || text > MAX_TEXT {
            return Err("截图贴图文档超过点数或文字容量".into());
        }
        for object in &self.objects {
            object.validate()?;
        }
        if self.history.is_none() && self.objects.iter().any(|o| !o.nested_groups.is_empty()) {
            return Err("嵌套贴图文档缺少身份与历史外壳".into());
        }
        if let Some(mask) = &self.mask {
            if mask.len() > MAX_POINTS
                || mask.iter().flatten().any(|x| !x.is_finite())
                || Polygon::new(
                    &mask
                        .iter()
                        .map(|p| Point { x: p[0], y: p[1] })
                        .collect::<Vec<_>>(),
                )
                .is_none()
            {
                return Err("截图贴图套索轮廓无效".into());
            }
        }
        Ok(())
    }
    /// 在修改目标前完成全部解码及历史连续性校验；恢复本身不提交用户命令。
    pub(crate) fn restore_into(&self, editor: &mut Editor) -> Result<()> {
        self.validate_shape()?;
        editor.document.ensure_no_long_clip()?;
        if editor.canvas != Rect::full(self.width, self.height)
            || !editor.document.items().is_empty()
            || editor.document.crop_mask.is_some()
            || editor.document.can_undo()
            || editor.document.can_redo()
            || editor.text_draft.is_some()
            || editor.lasso_draft.is_some()
            || editor.has_pending_drawing()
            || editor.is_dragging()
            || editor.watermark.has_preview()
        {
            return Err("截图贴图文档恢复目标不是同尺寸空画布".into());
        }
        let document = history::decode(self)?;
        editor.document = document;
        editor.crop = self.crop.map(|r| Rect {
            x: r[0],
            y: r[1],
            width: r[2],
            height: r[3],
        });
        Ok(())
    }
}

#[cfg(test)]
#[path = "pin_undo_restore_tests.rs"]
mod undo_restore_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn effect_radius_history_source(rounded: bool) -> Editor {
        let mut editor = Editor::new(Rect::full(80, 60), Some(Rect::full(80, 60)));
        let mut mosaic = Annotation::new(
            Tool::Mosaic,
            Point { x: 4., y: 5. },
            Style {
                radius: 8.,
                effect_radius: if rounded { 3.5 } else { 0. },
                ..Style::default()
            },
        );
        mosaic.update(Point { x: 35., y: 29. });
        let id = editor.document.insert(mosaic);
        let mut after = editor.document.get(id).unwrap().clone();
        after.style.color = 0x123456;
        if rounded {
            after.style.effect_radius = 7.25;
        }
        editor.document.replace(id, after);
        let mut blur = Annotation::new(
            Tool::Blur,
            Point { x: 41., y: 7. },
            Style {
                radius: 19.,
                effect_radius: if rounded { 8192. } else { 0. },
                ..Style::default()
            },
        );
        blur.update(Point { x: 71., y: 49. });
        editor.document.insert(blur);
        editor.undo();
        assert!(editor.document.can_undo() && editor.document.can_redo());
        editor
    }

    fn remove_effect_radius_fields(value: &mut serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(fields) => {
                let removed = usize::from(fields.remove("effectRadius").is_some());
                removed
                    + fields
                        .values_mut()
                        .map(remove_effect_radius_fields)
                        .sum::<usize>()
            }
            serde_json::Value::Array(items) => {
                items.iter_mut().map(remove_effect_radius_fields).sum()
            }
            _ => 0,
        }
    }

    fn assert_effect_radius_history_matches(source: &mut Editor, restored: &mut Editor) {
        assert_eq!(restored.document.items(), source.document.items());
        for redo in [true, false, false, false, true, true, true] {
            if redo {
                source.redo();
                restored.redo();
            } else {
                source.undo();
                restored.undo();
            }
            assert_eq!(restored.document.items(), source.document.items());
            assert_eq!(restored.document.can_undo(), source.document.can_undo());
            assert_eq!(restored.document.can_redo(), source.document.can_redo());
        }
    }

    #[test]
    fn legacy_effect_radius_missing_keeps_old_radius_and_both_history_stacks() {
        let mut source = effect_radius_history_source(false);
        let wire = WireRasterDocument::from_editor(&source, (80, 60)).unwrap();
        let mut json = serde_json::to_value(wire).unwrap();
        assert_eq!(remove_effect_radius_fields(&mut json), 5);
        assert_eq!(json["objects"][0]["style"]["radius"], 8.);
        let bytes = serde_json::to_vec(&json).unwrap();
        assert!(!String::from_utf8(bytes.clone())
            .unwrap()
            .contains("effectRadius"));
        let legacy: WireRasterDocument = serde_json::from_slice(&bytes).unwrap();
        legacy.validate().unwrap();
        let mut restored = Editor::new(Rect::full(80, 60), Some(Rect::full(80, 60)));
        legacy.restore_into(&mut restored).unwrap();
        let style = restored.document.items()[0].annotation.style;
        assert_eq!(style.radius, 8.);
        assert_eq!(style.effect_radius, 0.);
        assert_effect_radius_history_matches(&mut source, &mut restored);
    }

    #[test]
    fn effect_radius_json_roundtrips_current_undo_redo_and_scaled_values() {
        let mut source = effect_radius_history_source(true);
        let wire = WireRasterDocument::from_editor(&source, (80, 60)).unwrap();
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["objects"][0]["style"]["effectRadius"], 7.25);
        assert_eq!(
            json["history"]["undo"][1]["before"]["style"]["effectRadius"],
            3.5
        );
        assert_eq!(
            json["history"]["undo"][1]["after"]["style"]["effectRadius"],
            7.25
        );
        assert_eq!(
            json["history"]["redo"][0]["object"]["annotation"]["style"]["effectRadius"],
            8192.
        );
        let bytes = serde_json::to_vec(&wire).unwrap();
        let decoded: WireRasterDocument = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, wire);
        let mut restored = Editor::new(Rect::full(80, 60), Some(Rect::full(80, 60)));
        decoded.restore_into(&mut restored).unwrap();
        assert_effect_radius_history_matches(&mut source, &mut restored);
    }

    #[test]
    fn effect_radius_bad_current_or_history_rejects_before_restore_mutation() {
        let source = effect_radius_history_source(true);
        let wire = WireRasterDocument::from_editor(&source, (80, 60)).unwrap();
        for location in 0..4 {
            for invalid in [-1., f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut bad = wire.clone();
                match location {
                    0 => bad.objects[0].style.effect_radius = invalid,
                    1 | 2 => {
                        let history::WireCommand::Replace { before, after, .. } =
                            &mut bad.history.as_mut().unwrap().undo[1]
                        else {
                            panic!("fixture must contain an actual Replace")
                        };
                        if location == 1 {
                            before.style.effect_radius = invalid;
                        } else {
                            after.style.effect_radius = invalid;
                        }
                    }
                    _ => {
                        let history::WireCommand::Insert { object, .. } =
                            &mut bad.history.as_mut().unwrap().redo[0]
                        else {
                            panic!("fixture must contain an actual Insert")
                        };
                        object.annotation.style.effect_radius = invalid;
                    }
                }
                assert!(bad.validate().is_err());
                let mut target = Editor::new(Rect::full(80, 60), Some(Rect::full(80, 60)));
                let before = WireRasterDocument::from_editor(&target, (80, 60)).unwrap();
                assert!(bad.restore_into(&mut target).is_err());
                assert_eq!(
                    WireRasterDocument::from_editor(&target, (80, 60)).unwrap(),
                    before
                );
            }
        }
        for value in [serde_json::Value::Null, serde_json::json!("NaN")] {
            let mut json = serde_json::to_value(&wire).unwrap();
            json["objects"][0]["style"]["effectRadius"] = value;
            assert!(serde_json::from_value::<WireRasterDocument>(json).is_err());
        }
        let mut invalid_source = effect_radius_history_source(true);
        let object = invalid_source.document.items()[0].clone();
        let mut after = object.annotation;
        after.style.effect_radius = -1.;
        invalid_source.document.replace(object.id, after);
        assert!(WireRasterDocument::from_editor(&invalid_source, (80, 60)).is_err());
    }
    #[test]
    fn raster_document_roundtrips_marks_groups_mask_crop_and_every_style_field() {
        let size = (81, 63);
        let mut editor = Editor::new(
            Rect::full(size.0, size.1),
            Some(Rect {
                x: 3,
                y: 5,
                width: 73,
                height: 53,
            }),
        );
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 8., y: 9. },
            Style {
                color: 0x112233,
                width: 2.5,
                opacity: 0.4,
                filled: true,
                radius: 3.,
                effect_radius: 12.5,
                font_size: 19.,
                bold: true,
                arrow_head: ArrowHead::Dart,
                arrow_start: true,
                arrow_end: false,
                dashed: true,
                pencil_smooth: false,
                polyline_smooth: true,
                highlighter_blend: HighlighterBlend::Multiply,
                highlighter_shape: HighlighterShape::Line,
                text: TextStyle {
                    italic: true,
                    underline: true,
                    strike: true,
                    alignment: Alignment::Right,
                    outline: true,
                    background: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        a.update(Point { x: 42., y: 35. });
        a.rotation = 31.;
        let first = editor.document.insert(a.clone());
        let second = editor.document.insert(a.clone());
        assert!(editor.document.set_group(&[first, second], true));
        editor.document.set_crop_mask(Polygon::new(&[
            Point { x: 3., y: 5. },
            Point { x: 76., y: 5. },
            Point { x: 76., y: 58. },
            Point { x: 3., y: 58. },
        ]));
        let doc = WireRasterDocument::from_editor(&editor, size).unwrap();
        let bytes = serde_json::to_vec(&doc).unwrap();
        let decoded: WireRasterDocument = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, doc);
        let mut restored =
            Editor::new(Rect::full(size.0, size.1), Some(Rect::full(size.0, size.1)));
        decoded.restore_into(&mut restored).unwrap();
        assert_eq!(restored.crop, editor.crop);
        assert_eq!(restored.document.crop_mask, editor.document.crop_mask);
        assert_eq!(restored.document.items()[0].annotation, a);
        assert_eq!(
            restored
                .document
                .expand_groups(&[restored.document.items()[0].id])
                .len(),
            2
        );
        assert!(decoded.restore_into(&mut restored).is_err());
    }
    #[test]
    fn raster_document_preserves_local_fill_and_managed_effect_regions() {
        let size = (40, 30);
        let mut editor = Editor::new(Rect::full(40, 30), Some(Rect::full(40, 30)));
        let region = Bounds {
            x: 4.,
            y: 5.,
            width: 9.,
            height: 7.,
        };
        let mut fill = Annotation::new(Tool::LocalFill, Point { x: 4., y: 5. }, Style::default());
        fill.fill_geometry = LocalFillGeometry::Regions(vec![region].into());
        editor.document.insert(fill.clone());
        let mut effect = Annotation::new(Tool::Mosaic, Point { x: 4., y: 5. }, Style::default());
        effect
            .set_effect_regions(vec![region, Bounds { x: 24., ..region }], true)
            .unwrap();
        editor.document.insert(effect.clone());
        let doc = WireRasterDocument::from_editor(&editor, size).unwrap();
        let mut restored = Editor::new(Rect::full(40, 30), Some(Rect::full(40, 30)));
        doc.restore_into(&mut restored).unwrap();
        assert_eq!(restored.document.items()[0].annotation, fill);
        assert_eq!(restored.document.items()[1].annotation, effect);
        assert!(restored.document.items()[1]
            .annotation
            .effect_regions_managed());
    }
    #[test]
    fn raster_document_rejects_invalid_wire_and_keeps_target_untouched() {
        let mut doc = WireRasterDocument::empty((31, 19)).unwrap();
        doc.crop = Some([30, 0, 2, 19]);
        let mut target = Editor::new(Rect::full(31, 19), Some(Rect::full(31, 19)));
        assert!(doc.restore_into(&mut target).is_err());
        assert!(target.document.items().is_empty());
        assert_eq!(target.crop, Some(Rect::full(31, 19)));
        doc.crop = None;
        doc.version = 2;
        assert!(doc.validate().is_err());
        assert!(WireRasterDocument::empty((0, 19)).is_err());
    }

    fn nested_storage_source(text_only: bool) -> (Editor, Vec<u64>) {
        let mut source = Editor::new(Rect::full(160, 120), Some(Rect::full(160, 120)));
        let ids = source.document.insert_many(
            (0..4)
                .map(|index| {
                    let tool = if text_only {
                        Tool::Text
                    } else {
                        [Tool::Rectangle, Tool::Ellipse, Tool::Mosaic, Tool::Blur][index]
                    };
                    let mut mark = Annotation::new(
                        tool,
                        Point {
                            x: 8. + index as f32 * 32.,
                            y: 15.,
                        },
                        Style {
                            color: if text_only && index == 0 {
                                crate::pin_text_source::colors(true).1
                            } else {
                                0x28516a + index as u32
                            },
                            radius: 8.,
                            effect_radius: if matches!(tool, Tool::Mosaic | Tool::Blur) {
                                3.5
                            } else {
                                0.
                            },
                            ..Style::default()
                        },
                    );
                    mark.update(Point {
                        x: 33. + index as f32 * 32.,
                        y: 48.,
                    });
                    mark.text = format!("层次{index} 🙂");
                    mark
                })
                .collect(),
        );
        assert!(source.document.set_group(&ids[..2], true));
        assert!(source.document.set_group(&ids[2..], true));
        assert!(source.document.set_group(&[ids[0], ids[2]], true));
        assert!(source
            .document
            .items()
            .iter()
            .all(|o| o.nested_groups.len() == 1));
        let mut after = source.document.get(ids[0]).unwrap().clone();
        after.text.push_str(" 已修改");
        source.document.replace(ids[0], after);
        source.document.remove(ids[1]);
        source.document.undo();
        assert!(source.document.can_undo() && source.document.can_redo());
        (source, ids)
    }

    fn nested_restore_target() -> Editor {
        Editor::new(Rect::full(160, 120), Some(Rect::full(160, 120)))
    }

    fn nested_assert_bad_raster(encoded: serde_json::Value) {
        let wire: WireRasterDocument = serde_json::from_value(encoded).unwrap();
        let mut target = nested_restore_target();
        let before = target.document.bounded_history_snapshot().unwrap();
        let crop = target.crop;
        assert!(wire.validate().is_err());
        assert!(wire.restore_into(&mut target).is_err());
        assert_eq!(target.document.bounded_history_snapshot().unwrap(), before);
        assert_eq!(target.crop, crop);
        assert_eq!(target.document.revision(), 0);
        assert_eq!(target.document.edit_revision(), 0);
    }

    #[test]
    fn nested_raster_current_and_two_stacks_roundtrip_full_membership_without_new_edits() {
        let (mut source, ids) = nested_storage_source(false);
        let expected = source.document.bounded_history_snapshot().unwrap();
        let wire = WireRasterDocument::from_editor(&source, (160, 120)).unwrap();
        let encoded = serde_json::to_value(&wire).unwrap();
        assert_eq!(encoded["version"], 1);
        assert_eq!(encoded["history"]["version"], 2);
        assert!(encoded["history"]["undo"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "nestedGroups"));
        let decoded: WireRasterDocument =
            serde_json::from_slice(&serde_json::to_vec(&wire).unwrap()).unwrap();
        let mut restored = nested_restore_target();
        decoded.restore_into(&mut restored).unwrap();
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            expected
        );
        assert_eq!(restored.document.revision(), 0);
        assert_eq!(restored.document.edit_revision(), 0);
        restored.document.redo();
        source.document.redo();
        assert_eq!(restored.document.items(), source.document.items());
        while source.document.can_undo() {
            source.document.undo();
            restored.document.undo();
            assert_eq!(restored.document.items(), source.document.items());
        }
        while source.document.can_redo() {
            source.document.redo();
            restored.document.redo();
            assert_eq!(restored.document.items(), source.document.items());
        }
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            source.document.bounded_history_snapshot().unwrap()
        );
        assert_eq!(
            restored.document.get(ids[2]).unwrap().style.effect_radius,
            3.5
        );
        assert_eq!(restored.document.get(ids[2]).unwrap().style.radius, 8.);
        source.document.clear();
        let mut pending_mark =
            Annotation::new(Tool::Text, Point { x: 10., y: 10. }, Style::default());
        pending_mark.update(Point { x: 40., y: 35. });
        pending_mark.text = "空current后新文本".into();
        let pending = source.document.insert(pending_mark);
        let mut mark = source.document.get(pending).unwrap().clone();
        mark.text = "空current后待重做".into();
        source.document.replace(pending, mark);
        source.document.undo();
        source.document.undo();
        assert!(source.document.items().is_empty());
        let empty = WireRasterDocument::from_editor(&source, (160, 120)).unwrap();
        assert_eq!(
            serde_json::to_value(&empty).unwrap()["history"]["version"],
            2
        );
        let mut empty_restored = nested_restore_target();
        empty.restore_into(&mut empty_restored).unwrap();
        assert_eq!(
            empty_restored.document.bounded_history_snapshot().unwrap(),
            source.document.bounded_history_snapshot().unwrap()
        );
        source.document.undo();
        empty_restored.document.undo();
        assert_eq!(empty_restored.document.items(), source.document.items());
    }

    #[test]
    fn nested_raster_legacy_fields_versions_and_bad_current_history_reject_atomically() {
        let (source, _) = nested_storage_source(false);
        let base =
            serde_json::to_value(WireRasterDocument::from_editor(&source, (160, 120)).unwrap())
                .unwrap();
        let mut bad_version = base.clone();
        bad_version["history"]["version"] = serde_json::json!(1);
        nested_assert_bad_raster(bad_version);
        let mut no_history = base.clone();
        no_history.as_object_mut().unwrap().remove("history");
        nested_assert_bad_raster(no_history);
        for inner in [
            serde_json::json!([0]),
            serde_json::json!([base["objects"][0]["group"].as_u64().unwrap()]),
            serde_json::json!(vec![1_u64; 16]),
        ] {
            let mut bad = base.clone();
            bad["objects"][0]["nestedGroups"] = inner;
            nested_assert_bad_raster(bad);
        }
        let mut bad_clock = base.clone();
        bad_clock["objects"][0]["nestedGroups"] = serde_json::json!([u64::MAX]);
        nested_assert_bad_raster(bad_clock);
        let group_command = base["history"]["undo"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["kind"] == "nestedGroups")
            .unwrap();
        let mut bad_old_undo = base.clone();
        bad_old_undo["history"]["undo"][group_command]["before"][0][1]["nestedGroups"] =
            serde_json::json!([2]);
        nested_assert_bad_raster(bad_old_undo);
        let mut bad_redo = base.clone();
        bad_redo["history"]["redo"][0]["object"]["annotation"]["nestedGroups"] =
            serde_json::json!([0]);
        nested_assert_bad_raster(bad_redo);
        let replace = base["history"]["undo"]
            .as_array()
            .unwrap()
            .iter()
            .position(|c| c["kind"] == "replace")
            .unwrap();
        let mut bad_replace = base.clone();
        bad_replace["history"]["undo"][replace]["before"]["nestedGroups"] = serde_json::json!([1]);
        nested_assert_bad_raster(bad_replace);
        let mut bad_type = base;
        bad_type["objects"][0]["nestedGroups"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<WireRasterDocument>(bad_type).is_err());

        let mut flat = nested_restore_target();
        let mut rectangle =
            Annotation::new(Tool::Rectangle, Point { x: 2., y: 3. }, Style::default());
        rectangle.update(Point { x: 25., y: 30. });
        let a = flat.document.insert(rectangle.clone());
        let b = flat.document.insert(rectangle);
        assert!(flat.document.set_group(&[a, b], true));
        let old = WireRasterDocument::from_editor(&flat, (160, 120)).unwrap();
        let mut encoded = serde_json::to_value(&old).unwrap();
        assert_eq!(encoded["history"]["version"], 1);
        assert!(encoded["objects"][0].get("nestedGroups").is_none());
        let mut target = nested_restore_target();
        old.restore_into(&mut target).unwrap();
        assert_eq!(
            target.document.bounded_history_snapshot().unwrap(),
            flat.document.bounded_history_snapshot().unwrap()
        );
        encoded.as_object_mut().unwrap().remove("history");
        for object in encoded["objects"].as_array_mut().unwrap() {
            object["group"] = serde_json::json!(999);
        }
        let legacy: WireRasterDocument = serde_json::from_value(encoded).unwrap();
        let mut target = nested_restore_target();
        legacy.restore_into(&mut target).unwrap();
        assert!(target
            .document
            .items()
            .iter()
            .all(|o| o.group == Some(1) && o.nested_groups.is_empty()));
        assert!(!target.document.can_undo() && !target.document.can_redo());
    }

    #[test]
    fn nested_raster_budget_and_history_trimming_preserve_current_and_version() {
        let (mut source, ids) = nested_storage_source(true);
        for id in &ids {
            let mut text = source.document.get(*id).unwrap().clone();
            text.text = "x".repeat(8192);
            source.document.replace(*id, text);
        }
        for color in 0..140 {
            let mut text = source.document.get(ids[0]).unwrap().clone();
            text.style.color = color;
            source.document.replace(ids[0], text);
        }
        for _ in 0..5 {
            source.document.undo();
        }
        let current = source.document.items().to_vec();
        let snapshot = source.document.bounded_history_snapshot().unwrap();
        let mut wire = WireRasterDocument::from_editor(&source, (160, 120)).unwrap();
        let encoded = serde_json::to_value(&wire).unwrap();
        assert_eq!(encoded["history"]["version"], 2);
        let kept_undo = encoded["history"]["undo"].as_array().unwrap().len();
        let kept_redo = encoded["history"]["redo"].as_array().unwrap().len();
        assert!(kept_undo + kept_redo < snapshot.undo.len() + snapshot.redo.len());
        assert!(kept_undo > 0 && kept_redo > 0);
        assert!(serde_json::to_vec(&wire).unwrap().len() <= history::MAX_WIRE_BYTES);
        let mut target = nested_restore_target();
        wire.restore_into(&mut target).unwrap();
        assert_eq!(target.document.items(), current);
        for _ in 0..kept_undo {
            source.document.undo();
            target.document.undo();
            assert_eq!(target.document.items(), source.document.items());
        }
        while wire.trim_oldest_history() {
            wire.validate().unwrap();
        }
        let encoded = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            encoded["history"]["version"], 2,
            "裁空两栈仍需保护嵌套current版本"
        );
        assert!(encoded["history"]["undo"].as_array().unwrap().is_empty());
        assert!(encoded["history"]["redo"].as_array().unwrap().is_empty());
        let mut target = nested_restore_target();
        wire.restore_into(&mut target).unwrap();
        assert_eq!(target.document.items(), current);
        assert!(!target.document.can_undo() && !target.document.can_redo());
    }

    #[test]
    fn nested_mixed_text_bridge_keeps_membership_before_and_after_theme_mapping() {
        let (mut source, ids) = nested_storage_source(true);
        let mut rectangle = Annotation::new(
            Tool::Rectangle,
            Point { x: 60., y: 75. },
            Style {
                color: 0x2574a8,
                ..Style::default()
            },
        );
        rectangle.update(Point { x: 95., y: 108. });
        let marker = source.document.insert(rectangle);
        assert!(source.document.set_group(&[ids[0], marker], true));
        source.document.remove(marker);
        source.document.undo();
        let wire = crate::pin_text_document::WireTextDocument::try_from_vector_text_editor(
            &source,
            (160, 120),
            true,
        )
        .unwrap();
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["version"], 2);
        assert_eq!(json["mixedDocument"]["history"]["version"], 2);
        let expected = source
            .document
            .bounded_history_snapshot_mapped(|a| {
                crate::pin_text_source::remap_theme(a, true, false);
            })
            .unwrap();
        let mut restored = nested_restore_target();
        wire.restore_into(&mut restored, false).unwrap();
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            expected
        );
        assert_eq!(restored.document.get(marker).unwrap().style.color, 0x2574a8);
        assert_eq!(
            restored.document.get(ids[0]).unwrap().style.color,
            crate::pin_text_source::colors(false).1
        );
        assert_eq!(restored.document.edit_revision(), 0);
        while source.document.can_undo() {
            source.document.undo();
            restored.document.undo();
            let expected = source
                .document
                .bounded_history_snapshot_mapped(|a| {
                    crate::pin_text_source::remap_theme(a, true, false);
                })
                .unwrap();
            assert_eq!(restored.document.items(), expected.items);
        }
        while source.document.can_redo() {
            source.document.redo();
            restored.document.redo();
            let expected = source
                .document
                .bounded_history_snapshot_mapped(|a| {
                    crate::pin_text_source::remap_theme(a, true, false);
                })
                .unwrap();
            assert_eq!(restored.document.items(), expected.items);
        }
        let mut invalid = json;
        invalid["mixedDocument"]["history"]["version"] = serde_json::json!(1);
        let bad: crate::pin_text_document::WireTextDocument =
            serde_json::from_value(invalid).unwrap();
        let mut target = nested_restore_target();
        let before = target.document.bounded_history_snapshot().unwrap();
        assert!(bad.restore_into(&mut target, false).is_err());
        assert_eq!(target.document.bounded_history_snapshot().unwrap(), before);
    }
}

#[cfg(test)]
mod rectangle_corner_tests {
    use super::*;
    fn fixture() -> (Editor, u64) {
        let canvas = Rect::full(400, 300);
        let mut e = Editor::new(canvas, Some(canvas));
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 30., y: 40. },
            Style {
                radius: 3.,
                rectangle_radii: Some([3., 8., 15., 22.]),
                filled: true,
                ..Default::default()
            },
        );
        a.update(Point { x: 180., y: 160. });
        a.rotation = 37.;
        let id = e.document.insert(a.clone());
        a.style.rectangle_radii = Some([3., 13., 15., 22.]);
        e.document.replace(id, a);
        let mut second = Annotation::new(
            Tool::Rectangle,
            Point { x: 190., y: 70. },
            Style {
                radius: 8192.,
                rectangle_radii: Some([8192., 4096., 2048., 1024.]),
                ..Default::default()
            },
        );
        second.update(Point { x: 370., y: 220. });
        e.document.insert(second);
        e.document.undo();
        (e, id)
    }
    #[test]
    fn rectangle_corner_wire_roundtrips_current_both_stacks_and_legacy_missing_without_pixel_migration(
    ) {
        let (mut source, _) = fixture();
        let wire = WireRasterDocument::from_editor(&source, (400, 300)).unwrap();
        let bytes = serde_json::to_vec(&wire).unwrap();
        let decoded: WireRasterDocument = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, wire);
        let canvas = Rect::full(400, 300);
        let mut restored = Editor::new(canvas, Some(canvas));
        decoded.restore_into(&mut restored).unwrap();
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            source.document.bounded_history_snapshot().unwrap()
        );
        source.document.redo();
        restored.document.redo();
        assert_eq!(source.document.items(), restored.document.items());
        while source.document.can_undo() {
            source.document.undo();
            restored.document.undo();
            assert_eq!(source.document.items(), restored.document.items());
        }
        let mut legacy = Editor::new(canvas, Some(canvas));
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 30., y: 40. },
            Style {
                radius: 17.,
                filled: true,
                ..Default::default()
            },
        );
        a.update(Point { x: 180., y: 160. });
        a.rotation = 37.;
        legacy.document.insert(a.clone());
        let wire = WireRasterDocument::from_editor(&legacy, (400, 300)).unwrap();
        let bytes = serde_json::to_vec(&wire).unwrap();
        assert!(!String::from_utf8(bytes.clone())
            .unwrap()
            .contains("rectangleRadii"));
        let decoded: WireRasterDocument = serde_json::from_slice(&bytes).unwrap();
        let mut restored = Editor::new(canvas, Some(canvas));
        decoded.restore_into(&mut restored).unwrap();
        let back = &restored.document.items()[0].annotation;
        assert_eq!(back.style.rectangle_radii, None);
        assert_eq!(back, &a);
        let (old_rect, old_image) = crate::raster::shape(&a, canvas).unwrap();
        let (back_rect, back_image) = crate::raster::shape(back, canvas).unwrap();
        assert_eq!(back_rect, old_rect);
        assert_eq!(
            (back_image.width, back_image.height),
            (old_image.width, old_image.height)
        );
        assert_eq!(back_image.bgra, old_image.bgra);
    }

    #[test]
    fn rectangle_corner_wire_invalid_current_and_history_are_rejected_atomically() {
        let (source, _) = fixture();
        let wire = WireRasterDocument::from_editor(&source, (400, 300)).unwrap();
        for location in 0..4 {
            for invalid in [-1., f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut bad = wire.clone();
                let values = Some([3., invalid, 15., 22.]);
                match location {
                    0 => bad.objects[0].style.rectangle_radii = values,
                    1 | 2 => {
                        let history::WireCommand::Replace { before, after, .. } =
                            &mut bad.history.as_mut().unwrap().undo[1]
                        else {
                            panic!("真实夹具需要Replace")
                        };
                        if location == 1 {
                            before.style.rectangle_radii = values;
                        } else {
                            after.style.rectangle_radii = values;
                        }
                    }
                    _ => {
                        let history::WireCommand::Insert { object, .. } =
                            &mut bad.history.as_mut().unwrap().redo[0]
                        else {
                            panic!("真实夹具需要Insert")
                        };
                        object.annotation.style.rectangle_radii = values;
                    }
                }
                assert!(bad.validate().is_err());
                let mut target = Editor::new(source.canvas, Some(source.canvas));
                let before = target.document.bounded_history_snapshot().unwrap();
                let revision = target.document.edit_revision();
                let crop = target.crop;
                assert!(bad.restore_into(&mut target).is_err());
                assert_eq!(target.document.bounded_history_snapshot().unwrap(), before);
                assert_eq!(target.document.edit_revision(), revision);
                assert_eq!(target.crop, crop);
            }
        }
        for values in [
            serde_json::json!([1, 2, 3]),
            serde_json::json!([1, 2, 3, 4, 5]),
            serde_json::json!([1, "NaN", 3, 4]),
        ] {
            let mut json = serde_json::to_value(&wire).unwrap();
            json["objects"][0]["style"]["rectangleRadii"] = values;
            assert!(serde_json::from_value::<WireRasterDocument>(json).is_err());
        }
    }

    #[test]
    fn rectangle_corner_mixed_text_bridge_keeps_geometry_and_complete_history_across_theme_restore()
    {
        let (mut source, id) = fixture();
        let canvas = Rect::full(400, 300);
        let mut text = Annotation::new(Tool::Text, Point { x: 20., y: 230. }, Style::default());
        text.update(Point { x: 180., y: 275. });
        text.text = "四角文档".into();
        source.document.insert(text);
        let wire = crate::pin_text_document::WireTextDocument::try_from_vector_text_editor(
            &source,
            (400, 300),
            true,
        )
        .unwrap();
        let expected = source
            .document
            .bounded_history_snapshot_mapped(|a| {
                crate::pin_text_source::remap_theme(a, true, false);
            })
            .unwrap();
        let mut target = Editor::new(canvas, Some(canvas));
        wire.restore_into(&mut target, false).unwrap();
        assert_eq!(
            target.document.bounded_history_snapshot().unwrap(),
            expected
        );
        assert_eq!(
            target.document.get(id).unwrap().style.rectangle_radii,
            Some([3., 13., 15., 22.])
        );
        while source.document.can_undo() {
            source.document.undo();
            target.document.undo();
            let expected = source
                .document
                .bounded_history_snapshot_mapped(|a| {
                    crate::pin_text_source::remap_theme(a, true, false);
                })
                .unwrap();
            assert_eq!(target.document.items(), expected.items);
        }
        while source.document.can_redo() {
            source.document.redo();
            target.document.redo();
            let expected = source
                .document
                .bounded_history_snapshot_mapped(|a| {
                    crate::pin_text_source::remap_theme(a, true, false);
                })
                .unwrap();
            assert_eq!(target.document.items(), expected.items);
        }
    }
}
