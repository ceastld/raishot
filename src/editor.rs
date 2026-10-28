//! 手势状态机：截图范围与对象文档分离，拖动预览不写入撤销历史。
use crate::{
    annotation::{Annotation, Bounds, HighlighterShape, Point, Style, Tool},
    document::{Document, ObjectId},
    geometry::Rect,
    interaction::{Hover, Parameter},
};
#[cfg(test)]
#[path = "editor_arrow_pending_tests.rs"]
mod arrow_pending_tests;
#[path = "editor_long_clip.rs"]
mod long_clip;
#[cfg(test)]
#[path = "editor_polyline_tests.rs"]
mod polyline_tests;
#[path = "editor_watermark.rs"]
mod watermark;

enum Gesture {
    Crop {
        before: Option<Rect>,
        start: (i32, i32),
        mode: CropGesture,
    },
    Draw(Annotation),
    PenLine(Annotation),
    Lasso {
        points: Vec<Point>,
        host: Rect,
    },
    ArrowPending(Annotation),
    Polyline(Annotation),
    Erase {
        ids: Vec<ObjectId>,
    },
    Group {
        before: Vec<(ObjectId, Annotation)>,
        current: Vec<(ObjectId, Annotation)>,
        point: Point,
        start: Point,
        mode: crate::selection_transform::Manipulation,
    },
    Marquee {
        start: Point,
        current: Point,
    },
    Object {
        id: ObjectId,
        before: Box<Annotation>,
        current: Annotation,
        start: Point,
        handle: Option<usize>,
        // Some 表示圆角手势，值为上一帧未限幅的鼠标投影；沿用对象预览与提交。
        radius_delta: Option<f32>,
        radius_anchor_projection: Option<f32>,
        radius_control: Option<crate::radius_handle::RadiusControl>,
        preview_style: Style,
    },
}
enum CropGesture {
    New,
    Move(Rect),
    Resize(Rect, u8),
    // 仅截图空文档 Edit 的内侧命中带；保留按下点到真实边界的距离。
    ResizeFromInside(Rect, u8),
}

pub struct TextDraft {
    pub id: Option<ObjectId>,
    pub annotation: Annotation,
}
/// 准备态不修改 Document；Scene 验证成功后宿主才能提交这一次增量。
pub(crate) struct SmartDocumentEdit {
    revision: u64,
    managed: Vec<(ObjectId, Annotation)>,
    target: Option<ObjectId>,
    after: Option<Annotation>,
    pub(crate) annotations: Vec<Annotation>,
    effect: Tool,
    style: Style,
}

#[cfg(test)]
mod smart_document_tests {
    use super::*;
    fn editor() -> Editor {
        let mut editor = Editor::new(
            Rect::full(300, 220),
            Some(Rect {
                x: 40,
                y: 30,
                width: 200,
                height: 150,
            }),
        );
        editor.select_tool(Tool::SmartMosaic);
        editor
    }
    fn regions() -> Vec<Bounds> {
        vec![
            Bounds {
                x: 60.,
                y: 50.,
                width: 50.,
                height: 15.,
            },
            Bounds {
                x: 160.,
                y: 90.,
                width: 50.,
                height: 15.,
            },
        ]
    }
    fn commit(editor: &mut Editor, regions: Vec<Bounds>) {
        let edit = editor
            .prepare_smart_regions(regions, Tool::Mosaic, editor.style)
            .unwrap()
            .unwrap();
        assert!(editor.commit_smart_regions(edit));
    }
    #[test]
    fn actual_payload_survives_clear_new_id_undo_insert_and_undo_clear() {
        let mut editor = editor();
        commit(&mut editor, regions());
        let original = editor.document.items()[0].id;
        editor.clear();
        assert!(editor.smart_applied_regions().is_empty());
        commit(&mut editor, regions());
        assert_ne!(editor.document.items()[0].id, original);
        editor.undo();
        assert!(editor.smart_applied_regions().is_empty());
        editor.undo();
        assert_eq!(editor.document.items()[0].id, original);
        assert_eq!(editor.smart_applied_regions(), regions());
        let revision = editor.document.revision();
        assert!(editor
            .prepare_smart_regions(regions(), Tool::Mosaic, editor.style)
            .unwrap()
            .is_none());
        assert_eq!(editor.document.revision(), revision);
    }
    #[test]
    fn preparation_and_stale_commit_do_not_consume_redo_or_publish_regions() {
        let mut editor = editor();
        commit(&mut editor, regions());
        editor.undo();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let context = editor.effect_context();
        for opacity in [f32::NAN, f32::INFINITY, -0.01, 1.01] {
            let invalid = Style {
                opacity,
                ..editor.style
            };
            assert!(editor
                .prepare_smart_regions(Vec::new(), Tool::Mosaic, invalid)
                .is_err());
            assert_eq!(editor.effect_context(), context);
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.document.edit_revision(), edit_revision);
            assert!(editor.document.can_redo());
            assert!(editor.smart_applied_regions().is_empty());
        }
        let edit = editor
            .prepare_smart_regions(regions(), Tool::Mosaic, editor.style)
            .unwrap()
            .unwrap();
        assert!(editor.document.can_redo());
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.smart_applied_regions().is_empty());
        editor.redo();
        assert!(!editor.commit_smart_regions(edit));
        assert_eq!(editor.smart_applied_regions(), regions());
    }
    #[test]
    fn compound_toggle_and_style_are_single_edits_and_manual_style_undo_restores_management() {
        let mut editor = editor();
        commit(&mut editor, regions());
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let style = Style {
            block_size: editor.style.block_size + 1,
            ..editor.style
        };
        let edit = editor
            .prepare_smart_regions(regions(), Tool::Blur, style)
            .unwrap()
            .unwrap();
        assert!(editor.commit_smart_regions(edit));
        assert!(editor.document.revision() > revision);
        assert_eq!(editor.document.edit_revision(), edit_revision + 1);
        assert_eq!(editor.effect_context(), (Tool::Blur, style));
        editor.undo();
        assert_eq!(editor.document.items()[0].annotation.tool, Tool::Mosaic);
        editor.redo();
        let id = editor.document.items()[0].id;
        editor.select_tool(Tool::Edit);
        editor.selected = Some(id);
        editor.style = style;
        editor.set_style(Style {
            block_size: style.block_size + 1,
            ..style
        });
        assert!(!editor.document.get(id).unwrap().effect_regions_managed());
        assert_eq!(
            editor.document.get(id).unwrap().effect_regions_world(),
            regions()
        );
        editor.undo();
        assert!(editor.document.get(id).unwrap().effect_regions_managed());
        editor.select_tool(Tool::SmartMosaic);
        commit(&mut editor, Vec::new());
        assert!(editor.document.items().is_empty());
        editor.undo();
        assert_eq!(editor.smart_applied_regions(), regions());
    }
    #[test]
    fn smart_outside_resize_preserves_regions_cancel_and_shift_proportion() {
        let original = Rect {
            x: 40,
            y: 30,
            width: 200,
            height: 150,
        };
        for down in [
            (10, 10),
            (140, 10),
            (270, 10),
            (270, 105),
            (270, 200),
            (140, 200),
            (10, 200),
            (10, 105),
        ] {
            let mut editor = editor();
            commit(&mut editor, regions());
            let revision = editor.document.revision();
            assert!(editor.begin_screenshot_crop_outside(down, 6.));
            let target = editor.constrained_pointer((down.0 + 12, down.1 + 7), true);
            editor.pointer_move(target);
            assert_eq!(editor.smart_applied_regions(), regions());
            editor.cancel_gesture();
            assert_eq!(editor.crop, Some(original));
            assert_eq!(editor.tool, Tool::SmartMosaic);
            assert_eq!(editor.document.revision(), revision);
        }
        let mut editor = editor();
        editor.pointer_down((70, 70), 6.);
        assert!(!editor.is_dragging());
        editor.set_smart_crop(None);
        editor.pointer_down((60, 50), 6.);
        assert_eq!(editor.crop_creation_before(), Some(None));
        editor.pointer_up((180, 130));
        assert_eq!(editor.tool, Tool::SmartMosaic);
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 60,
                y: 50,
                width: 120,
                height: 80
            })
        );
        editor.set_smart_crop(Some(original));
        assert_eq!(editor.tool, Tool::SmartMosaic);
        editor.select_tool(Tool::Rectangle);
        assert!(!editor.begin_screenshot_crop_outside((10, 10), 6.));
    }
}
/// 参数页打开时固定目标与版本；混合多选只提交具备圆角几何的对象。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RadiusSelection {
    pub selected_count: usize,
    pub targets: Vec<(ObjectId, f32)>,
    pub value: f32,
    pub mixed: bool,
    pub max_radius: f32,
    pub tool: Tool,
    selected: Option<ObjectId>,
    editor_tool: Tool,
    revision: u64,
    preview_revision: u64,
    selection: Vec<ObjectId>,
}
#[path = "editor_group_scope.rs"]
mod group_scope;
#[path = "editor_rectangle_radius.rs"]
mod rectangle_radius;
pub(crate) use rectangle_radius::{RectangleCornerDraft, RectangleCornerSelection};

pub struct Editor {
    pub canvas: Rect,
    pub crop: Option<Rect>,
    pub document: Document,
    pub tool: Tool,
    pub style: Style,
    pub(crate) effect_brush_mode: bool,
    smart_effect: (Tool, Style),
    tool_styles: std::collections::HashMap<Tool, Style>,
    pub selected: Option<ObjectId>,
    selected_node: Option<(ObjectId, usize)>,
    pub selection: Vec<ObjectId>,
    group_scope: Vec<u64>,
    group_scope_generation: u64,
    pub text_draft: Option<TextDraft>,
    pub lasso_draft: Option<crate::crop_mask::Polygon>,
    gesture: Option<Gesture>,
    preview_revision: u64,
    style_group: Option<u64>,
    pub(crate) watermark: crate::watermark_edit::WatermarkEdit,
    watermark_clock: std::time::Instant,
    watermark_document_revision: u64,
}
impl Editor {
    pub fn new(canvas: Rect, crop: Option<Rect>) -> Self {
        Self {
            canvas,
            crop,
            document: Document::default(),
            tool: Tool::Crop,
            style: Style::default(),
            effect_brush_mode: false,
            smart_effect: (Tool::Mosaic, Style::default()),
            tool_styles: Default::default(),
            selected: None,
            selected_node: None,
            selection: Vec::new(),
            group_scope: Vec::new(),
            group_scope_generation: 0,
            text_draft: None,
            lasso_draft: None,
            gesture: None,
            preview_revision: 0,
            style_group: None,
            watermark: Default::default(),
            watermark_clock: std::time::Instant::now(),
            watermark_document_revision: 0,
        }
    }
    pub fn is_dragging(&self) -> bool {
        self.gesture
            .as_ref()
            .is_some_and(|g| !matches!(g, Gesture::Polyline(_) | Gesture::ArrowPending(_)))
    }
    pub fn restore_tool_styles(&mut self, styles: impl IntoIterator<Item = (Tool, Style)>) {
        self.tool_styles.extend(styles);
    }
    pub fn tool_styles(&self) -> impl Iterator<Item = (Tool, Style)> + '_ {
        self.tool_styles.iter().map(|(t, s)| (*t, *s)).chain(
            (self.tool.draws() || self.tool == Tool::Eraser)
                .then_some((self.tool, self.drawing_style())),
        )
    }
    fn drawing_style(&self) -> Style {
        let mut style = self.style;
        if self.tool == Tool::Highlighter {
            style.highlighter_shape = self.highlighter_shape();
        }
        // 高亮框的对象圆角不发布为下一笔；批量参数也不能改写绘制默认值。
        let drawing = matches!(&self.gesture, Some(Gesture::Draw(_)));
        let ids = self.selected_ids();
        if !drawing && (ids.len() > 1 || (self.tool.supports_radius() && !ids.is_empty())) {
            let radius = self
                .tool_styles
                .get(&self.tool)
                .copied()
                .unwrap_or_else(|| crate::tool_presets::default_style(self.tool))
                .radius_for(self.tool);
            style.set_radius_for(self.tool, radius);
            if self.tool == Tool::Rectangle {
                style.rectangle_radii = self
                    .tool_styles
                    .get(&self.tool)
                    .and_then(|s| s.rectangle_radii);
            }
        }
        style
    }
    /// 画法是下一笔设置；选中旧对象和正在拖动的笔迹不会转换形状。
    pub fn highlighter_shape(&self) -> HighlighterShape {
        self.tool_styles.get(&Tool::Highlighter).map_or_else(
            || {
                if self.tool == Tool::Highlighter {
                    self.style.highlighter_shape
                } else {
                    crate::tool_presets::default_style(Tool::Highlighter).highlighter_shape
                }
            },
            |style| style.highlighter_shape,
        )
    }
    pub fn set_highlighter_shape(&mut self, shape: HighlighterShape) {
        let defaults = if self.tool == Tool::Highlighter {
            self.drawing_style()
        } else {
            crate::tool_presets::default_style(Tool::Highlighter)
        };
        self.tool_styles
            .entry(Tool::Highlighter)
            .or_insert(defaults)
            .highlighter_shape = shape;
        self.style.highlighter_shape = shape;
    }
    pub(crate) fn radius_selection(&self) -> RadiusSelection {
        let drawing = match &self.gesture {
            Some(Gesture::Draw(a)) if a.supports_radius() => Some(a),
            _ => None,
        };
        let selection = self.selected_ids();
        let selected_count = if drawing.is_some() {
            0
        } else {
            selection.len()
        };
        let mut targets = Vec::new();
        let mut preferred = None;
        if drawing.is_none() {
            for &id in &selection {
                let a = match &self.gesture {
                    Some(Gesture::Object {
                        id: current_id,
                        current,
                        ..
                    }) if *current_id == id => Some(current),
                    Some(Gesture::Group { current, .. }) => current
                        .iter()
                        .find(|(other, _)| *other == id)
                        .map(|(_, a)| a),
                    _ => self.document.get(id),
                };
                if let Some(a) = a.filter(|a| a.supports_radius() && a.corner_radius().is_finite())
                {
                    targets.push((id, a.corner_radius()));
                    if preferred.is_none() || self.selected == Some(id) {
                        preferred = Some((a.tool, a.corner_radius()));
                    }
                }
            }
        }
        let (tool, value) = preferred.unwrap_or_else(|| {
            drawing.map_or((self.tool, self.style.radius_for(self.tool)), |a| {
                (a.tool, a.corner_radius())
            })
        });
        let max_radius = targets
            .iter()
            .fold(50_f32.max(value), |m, (_, radius)| m.max(*radius));
        RadiusSelection {
            selected_count,
            mixed: targets.iter().any(|(_, radius)| *radius != value)
                || self.selected_objects().iter().any(|(_, a)| {
                    a.tool == Tool::Rectangle
                        && a.style
                            .rectangle_radii
                            .is_some_and(|r| r.iter().any(|v| *v != value))
                }),
            targets,
            value,
            max_radius,
            tool,
            selected: self.selected,
            editor_tool: self.tool,
            revision: self.document.revision(),
            preview_revision: self.preview_revision,
            selection,
        }
    }
    pub(crate) fn apply_radius(
        &mut self,
        snapshot: &RadiusSelection,
        value: f32,
    ) -> Result<bool, &'static str> {
        if !value.is_finite() || value < 0. || value > snapshot.max_radius {
            return Err(tr!(
                "圆角超出当前可调范围",
                "The radius is outside the adjustable range"
            ));
        }
        if &self.radius_selection() != snapshot {
            return Err(tr!(
                "对象或选择已经改变，请重新打开圆角参数",
                "The object or selection changed; reopen the radius setting"
            ));
        }
        if snapshot.targets.is_empty() {
            if snapshot.selected_count != 0 || !snapshot.tool.supports_radius() {
                return Err(tr!(
                    "所选对象不支持圆角",
                    "The selected object doesn't support corner radius"
                ));
            }
            if self.style.radius_for(snapshot.tool) == value {
                return Ok(false);
            }
            let mut style = self.style;
            style.set_radius_for(snapshot.tool, value);
            self.set_style(style);
            return Ok(true);
        }
        if snapshot.targets.iter().all(|(_, radius)| *radius == value)
            && !self.selected_objects().iter().any(|(_, a)| {
                a.tool == Tool::Rectangle
                    && a.style
                        .rectangle_radii
                        .is_some_and(|r| r.iter().any(|v| *v != value))
            })
        {
            return Ok(false);
        }
        self.remember_single_radius(value);
        self.update_selected(|a| {
            if a.supports_radius() {
                a.set_corner_radius(value);
            }
        });
        if let Some(tool) = self
            .selected_annotation()
            .filter(|a| a.supports_radius())
            .map(|a| a.tool)
        {
            self.style.set_radius_for(tool, value);
        }
        Ok(true)
    }
    fn remember_single_radius(&mut self, radius: f32) {
        if self.gesture.is_some() || self.selected_ids().len() != 1 {
            return;
        }
        if let Some(a) = self.selected_annotation().filter(|a| {
            matches!(a.tool, Tool::Rectangle | Tool::Spotlight)
                && a.style.radius != radius
                && (a.tool != Tool::Rectangle || a.style.rectangle_radii.is_none())
        }) {
            let tool = a.tool;
            self.tool_styles
                .entry(tool)
                .or_insert_with(|| crate::tool_presets::default_style(tool))
                .radius = radius;
        }
    }
    pub fn drawing_freehand_pen(&self) -> bool {
        matches!(&self.gesture, Some(Gesture::Draw(a)) if a.tool == Tool::Pen)
    }
    pub fn is_changing_crop(&self) -> bool {
        matches!(self.gesture, Some(Gesture::Crop { .. }))
    }
    pub(crate) fn crop_creation_before(&self) -> Option<Option<Rect>> {
        match &self.gesture {
            Some(Gesture::Crop {
                before,
                mode: CropGesture::New,
                ..
            }) => Some(*before),
            _ => None,
        }
    }
    pub fn is_dragging_radius(&self) -> bool {
        matches!(
            self.gesture,
            Some(Gesture::Object {
                radius_delta: Some(_),
                ..
            })
        )
    }
    pub fn scene_stamp(&self) -> (u64, u64) {
        (self.document.revision(), self.preview_revision)
    }
    /// 填充准备只读取真实草稿；蓝色画笔预览不能参与取色。
    pub(crate) fn local_fill_candidate(&self) -> Option<Annotation> {
        match &self.gesture {
            Some(Gesture::Draw(a)) if a.tool == Tool::LocalFill => Some(a.clone()),
            _ => None,
        }
    }
    pub(crate) fn document_annotations(&self) -> Vec<Annotation> {
        self.document
            .items()
            .iter()
            .map(|o| o.annotation.clone())
            .collect()
    }
    /// 必须在源像素和下层场景准备成功后调用；过期意图不得消费草稿或 redo。
    pub(crate) fn commit_prepared_local_fill(&mut self, expected: &Annotation) -> bool {
        if self.local_fill_candidate().as_ref() != Some(expected) {
            return false;
        }
        let Some(Gesture::Draw(a)) = self.gesture.take() else {
            return false;
        };
        self.insert_annotation_in_group_scope(a);
        self.selected = None;
        self.selected_node = None;
        self.selection.clear();
        self.preview_revision += 1;
        true
    }
    /// 正在拖动、按点增长的自由笔迹草稿在 `annotations()` 中的下标；
    /// Scene 据此只为它生成逐帧快速网格，提交后按完整质量重建。
    pub(crate) fn live_stroke_draft_index(&self) -> Option<usize> {
        match &self.gesture {
            Some(Gesture::Draw(a))
                if a.valid() && matches!(a.tool, Tool::Pen | Tool::Highlighter) =>
            {
                Some(self.document.items().len())
            }
            _ => None,
        }
    }
    /// 正在拖动／变换的已提交对象在 `annotations()` 中的下标；Scene 只对它们用平移网格
    /// 逐帧预览。手势结束后不再列出，下一次更新按完整质量重建。
    pub(crate) fn live_object_indices(&self) -> Vec<usize> {
        let items = self.document.items();
        let index = |id: &ObjectId| items.iter().position(|o| o.id == *id);
        match &self.gesture {
            Some(Gesture::Object { id, .. }) => index(id).into_iter().collect(),
            Some(Gesture::Group { current, .. }) => {
                current.iter().filter_map(|(id, _)| index(id)).collect()
            }
            _ => Vec::new(),
        }
    }
    pub fn annotations(&self) -> Vec<Annotation> {
        let mut items: Vec<_> = self
            .document
            .items()
            .iter()
            .map(|o| o.annotation.clone())
            .collect();
        match &self.gesture {
            Some(
                Gesture::Draw(a)
                | Gesture::PenLine(a)
                | Gesture::Polyline(a)
                | Gesture::ArrowPending(a),
            ) if a.valid() => {
                let mut preview = a.clone();
                if preview.tool == Tool::LocalFill {
                    preview.tool = Tool::Pen;
                    preview.style.color = 0x3399ff;
                    preview.style.opacity = 0.4;
                    preview.style.dashed = false;
                }
                if preview.tool == Tool::Spotlight
                    && !self
                        .document
                        .items()
                        .iter()
                        .any(|o| o.annotation.tool == Tool::Spotlight)
                {
                    items.insert(0, preview);
                } else {
                    items.push(preview);
                }
            }
            Some(Gesture::Erase { ids }) => items.retain(|a| {
                !self
                    .document
                    .items()
                    .iter()
                    .any(|o| ids.contains(&o.id) && o.annotation == *a)
            }),
            Some(Gesture::Group { current, .. }) => {
                for (id, a) in current {
                    if let Some(i) = self.document.items().iter().position(|o| o.id == *id) {
                        items[i] = a.clone();
                    }
                }
            }
            Some(Gesture::Object { id, current, .. }) => {
                if let Some(index) = self.document.items().iter().position(|o| o.id == *id) {
                    items[index] = current.clone();
                }
            }
            _ => {}
        }
        if let Some(draft) = &self.text_draft {
            if draft.annotation.valid() {
                if let Some(i) = draft
                    .id
                    .and_then(|id| self.document.items().iter().position(|o| o.id == id))
                {
                    items[i] = draft.annotation.clone();
                } else {
                    items.push(draft.annotation.clone());
                }
            }
        }
        // 手势中的孔可预览整层参数，已提交对象保持不变；取消直接回到旧层。
        if let Some(style) = self.spotlight_gesture_style() {
            for a in &mut items {
                if a.tool == Tool::Spotlight {
                    copy_spotlight_layer_style(&mut a.style, style);
                }
            }
        }
        self.apply_watermark_preview(&mut items);
        items
    }
    fn spotlight_gesture_style(&self) -> Option<Style> {
        match &self.gesture {
            Some(Gesture::Draw(a) | Gesture::Object { current: a, .. })
                if a.tool == Tool::Spotlight =>
            {
                Some(a.style)
            }
            Some(Gesture::Group { current, .. }) => current
                .iter()
                .rev()
                .find(|(_, a)| a.tool == Tool::Spotlight)
                .map(|(_, a)| a.style),
            _ => None,
        }
    }
    fn spotlight_layer_style(&self) -> Option<Style> {
        self.spotlight_gesture_style().or_else(|| {
            self.document
                .items()
                .iter()
                .rev()
                .find(|o| o.annotation.tool == Tool::Spotlight)
                .map(|o| o.annotation.style)
        })
    }
    fn targets_spotlight(&self) -> bool {
        if self.gesture.is_some() {
            return self.spotlight_gesture_style().is_some();
        }
        if self.text_draft.is_some() {
            return false;
        }
        let ids = self.selected_ids();
        (ids.is_empty() && self.tool == Tool::Spotlight)
            || ids.iter().any(|id| {
                self.document
                    .get(*id)
                    .is_some_and(|a| a.tool == Tool::Spotlight)
            })
    }
    fn spotlight_style_update(&self, before: Style, after: Style) -> Option<Style> {
        if !self.targets_spotlight()
            || (before.color == after.color
                && before.width == after.width
                && before.opacity == after.opacity)
        {
            return None;
        }
        let mut layer = self
            .spotlight_layer_style()
            .unwrap_or_else(|| crate::tool_presets::default_style(Tool::Spotlight));
        // 混合多选只传播明确修改的字段，不能把其它工具的整套样式带入聚光灯。
        if before.color != after.color {
            layer.color = after.color;
        }
        if before.width != after.width {
            layer.width = after.width;
        }
        if before.opacity != after.opacity {
            layer.opacity = after.opacity;
        }
        Some(layer)
    }
    fn remember_spotlight_style(&mut self, layer: Option<Style>) {
        if !self.targets_spotlight() || layer.is_none() {
            return;
        }
        let preset = self
            .tool_styles
            .entry(Tool::Spotlight)
            .or_insert_with(|| crate::tool_presets::default_style(Tool::Spotlight));
        if let Some(layer) = layer {
            copy_spotlight_layer_style(preset, layer);
        }
    }
    fn spotlight_replacements(
        &self,
        mut objects: Vec<(ObjectId, Annotation)>,
        layer: Option<Style>,
    ) -> Vec<(ObjectId, Annotation)> {
        if let Some(layer) = layer {
            for o in self.document.items() {
                if o.annotation.tool != Tool::Spotlight {
                    continue;
                }
                if let Some((_, a)) = objects.iter_mut().find(|(id, _)| *id == o.id) {
                    copy_spotlight_layer_style(&mut a.style, layer);
                } else {
                    let mut a = o.annotation.clone();
                    copy_spotlight_layer_style(&mut a.style, layer);
                    if a != o.annotation {
                        objects.push((o.id, a));
                    }
                }
            }
        }
        objects
    }
    fn sync_spotlight_default(&mut self) {
        if let Some(layer) = self.spotlight_layer_style() {
            copy_spotlight_layer_style(
                self.tool_styles
                    .entry(Tool::Spotlight)
                    .or_insert_with(|| crate::tool_presets::default_style(Tool::Spotlight)),
                layer,
            );
            if self.tool == Tool::Spotlight {
                copy_spotlight_layer_style(&mut self.style, layer);
            }
        }
    }
    pub fn selected_ids(&self) -> Vec<ObjectId> {
        let ids = if self.selection.len() > 1 {
            self.selection.clone()
        } else {
            self.selected.into_iter().collect()
        };
        self.expand_selection(&ids)
    }
    fn enter_selection_mode(&mut self) {
        // 主选样式替换面板基准之前，保留原绘制工具的下一笔预设。
        if self.text_draft.is_none() && (self.tool.draws() || self.tool == Tool::Eraser) {
            self.tool_styles.insert(self.tool, self.drawing_style());
        }
        self.tool = Tool::Edit;
    }
    fn sync_primary_selection_style(&mut self) {
        if self.gesture.is_some() || self.text_draft.is_some() {
            return;
        }
        if self.selected_ids().len() != 1
            || self
                .selected_node
                .is_some_and(|(id, _)| self.selected != Some(id))
        {
            self.selected_node = None;
        }
        if let Some(a) = self.selected.and_then(|id| self.document.get(id)) {
            self.style = a.style;
        } else if self.selected.is_some() {
            self.selected = None;
            self.selected_node = None;
        }
    }
    pub fn selected_objects(&self) -> Vec<(ObjectId, Annotation)> {
        if let Some(Gesture::Group { current, .. }) = &self.gesture {
            return current.clone();
        }
        self.selected_ids()
            .into_iter()
            .filter_map(|id| {
                self.document.get(id).map(|a| {
                    (
                        id,
                        if self.selected == Some(id) {
                            self.selected_annotation().unwrap_or(a).clone()
                        } else {
                            a.clone()
                        },
                    )
                })
            })
            .collect()
    }
    pub fn selection_bounds(&self) -> Option<Bounds> {
        let objects = self.selected_objects();
        (!objects.is_empty()).then(|| crate::selection_transform::bounds(&objects))
    }
    pub fn rotation_control(
        &self,
        tolerance: f32,
    ) -> Option<crate::selection_transform::RotationControl> {
        if self.text_draft.is_some() {
            return None;
        }
        let objects = self.selected_objects();
        (!objects.is_empty()).then(|| {
            crate::selection_transform::rotation_control(&objects, self.canvas, tolerance * 4.)
        })
    }
    pub fn rotation_handle(&self, tolerance: f32) -> Option<Point> {
        self.rotation_control(tolerance).map(|c| c.position)
    }
    pub fn radius_controls(&self, tolerance: f32) -> Vec<crate::radius_handle::RadiusControl> {
        if self.text_draft.is_some()
            || self.selected_ids().len() != 1
            || matches!(
                self.tool,
                Tool::Crop | Tool::Pen | Tool::Eraser | Tool::PixelEraser | Tool::Lasso
            )
        {
            return Vec::new();
        }
        self.selected_annotation()
            .map_or_else(Vec::new, |a| a.radius_controls(tolerance))
    }
    fn begin_group(&mut self, start: Point, mode: crate::selection_transform::Manipulation) {
        let before = self.selected_objects();
        let primary = self
            .selected
            .filter(|id| before.iter().any(|(candidate, _)| candidate == id))
            .or_else(|| before.last().map(|(id, _)| *id));
        if let Some((_, a)) = before.iter().find(|(id, _)| Some(*id) == primary) {
            self.style = a.style;
        }
        self.selection = before.iter().map(|(id, _)| *id).collect();
        self.selected = primary;
        self.selected_node = None;
        self.gesture = Some(Gesture::Group {
            current: before.clone(),
            before,
            point: start,
            start,
            mode,
        });
    }
    pub fn group_selection(&mut self, grouped: bool) -> bool {
        if !self.finish_gesture() {
            return false;
        }
        self.finish_text();
        let ids = self.selected_ids();
        let scope = self.group_scope_path();
        let changed = if scope.is_empty() {
            self.document.set_group(&ids, grouped)
        } else {
            self.document.set_group_in_scope(&ids, &scope, grouped)
        };
        self.repair_group_scope();
        self.selection = ids;
        self.selected = self.selection.last().copied();
        self.sync_primary_selection_style();
        changed
    }
    pub fn rotate_selection(&mut self, degrees: f32) {
        if !self.finish_gesture() {
            return;
        }
        if self.tool == Tool::Watermark {
            self.rotate_watermark(Some(degrees));
            return;
        }
        self.finish_text();
        let mut objects = self.selected_objects();
        if objects.is_empty() || degrees.abs() < 0.0001 {
            return;
        }
        let pivot = crate::selection_transform::pivot(&objects);
        for (_, a) in &mut objects {
            a.rotate_about(pivot, degrees);
        }
        self.document.replace_many(&objects);
    }
    pub fn reset_rotation(&mut self) {
        if !self.finish_gesture() {
            return;
        }
        if self.tool == Tool::Watermark {
            self.rotate_watermark(None);
            return;
        }
        let mut objects = self.selected_objects();
        for (_, a) in &mut objects {
            let center = crate::transform::center(a.local_bounds());
            a.rotate_about(center, -a.rotation);
        }
        self.document.replace_many(&objects);
    }
    pub fn marquee(&self) -> Option<Bounds> {
        if let Some(Gesture::Marquee { start, current }) = &self.gesture {
            Some(Bounds {
                x: start.x.min(current.x),
                y: start.y.min(current.y),
                width: (current.x - start.x).abs(),
                height: (current.y - start.y).abs(),
            })
        } else {
            None
        }
    }
    pub fn selected_annotation(&self) -> Option<&Annotation> {
        if let Some(draft) = &self.text_draft {
            return Some(&draft.annotation);
        }
        let id = self.selected?;
        if !self.object_in_group_scope(id) {
            return None;
        }
        if let Some(Gesture::Object {
            id: editing,
            current,
            ..
        }) = &self.gesture
        {
            if *editing == id {
                return Some(current);
            }
        }
        if let Some(Gesture::Group { current, .. }) = &self.gesture {
            return current
                .iter()
                .find(|(other, _)| *other == id)
                .map(|(_, a)| a);
        }
        self.document.get(id)
    }
    /// 效果副栏与智能请求使用同一个对象上下文；普通选区仍沿用下一笔参数。
    pub(crate) fn effect_context(&self) -> (Tool, Style) {
        if self.tool == Tool::SmartMosaic {
            return self.smart_effect;
        }
        self.selected_annotation()
            .filter(|a| a.tool.effect())
            .map_or((self.tool, self.style), |a| (a.tool, a.style))
    }
    pub(crate) fn smart_applied_regions(&self) -> Vec<Bounds> {
        self.document
            .items()
            .iter()
            .filter(|object| object.annotation.effect_regions_managed())
            .flat_map(|object| object.annotation.effect_regions_world())
            .collect()
    }
    pub(crate) fn prepare_smart_regions(
        &self,
        regions: Vec<Bounds>,
        effect: Tool,
        style: Style,
    ) -> crate::Result<Option<SmartDocumentEdit>> {
        if self.tool != Tool::SmartMosaic
            || self.is_dragging()
            || !matches!(effect, Tool::Mosaic | Tool::Blur)
            || !style.opacity.is_finite()
            || !(0. ..=1.).contains(&style.opacity)
        {
            return Err("当前智能马赛克模式不能提交区域".into());
        }
        let managed: Vec<_> = self
            .document
            .items()
            .iter()
            .filter(|object| object.annotation.effect_regions_managed())
            .map(|object| (object.id, object.annotation.clone()))
            .collect();
        let target = managed.last().map(|(id, _)| *id);
        let after = if regions.is_empty() {
            None
        } else {
            let first = regions[0];
            let mut annotation = Annotation::new(
                effect,
                Point {
                    x: first.x,
                    y: first.y,
                },
                style,
            );
            annotation.set_effect_regions(regions, true)?;
            Some(annotation)
        };
        if (managed.is_empty() && after.is_none())
            || (managed.len() == 1 && after.as_ref() == Some(&managed[0].1))
        {
            return Ok(None);
        }
        let mut annotations = Vec::new();
        for object in self.document.items() {
            if Some(object.id) == target {
                annotations.extend(after.iter().cloned());
            } else if !object.annotation.effect_regions_managed() {
                annotations.push(object.annotation.clone());
            }
        }
        if target.is_none() {
            annotations.extend(after.iter().cloned());
        }
        Ok(Some(SmartDocumentEdit {
            revision: self.document.revision(),
            managed,
            target,
            after,
            annotations,
            effect,
            style,
        }))
    }
    pub(crate) fn commit_smart_regions(&mut self, edit: SmartDocumentEdit) -> bool {
        if self.tool != Tool::SmartMosaic
            || self.document.revision() != edit.revision
            || self.is_dragging()
            || edit
                .managed
                .iter()
                .any(|(id, before)| self.document.get(*id) != Some(before))
        {
            return false;
        }
        match (edit.target, edit.after) {
            (None, Some(after)) => {
                self.document.insert(after);
            }
            (Some(target), Some(after)) => {
                let removed: Vec<_> = edit
                    .managed
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| *id != target)
                    .collect();
                self.document
                    .replace_remove_many(&[(target, after)], &removed);
            }
            (_, None) => {
                self.document
                    .remove_many(&edit.managed.iter().map(|(id, _)| *id).collect::<Vec<_>>());
            }
        }
        self.smart_effect = (edit.effect, edit.style);
        self.style = edit.style;
        self.reset_group_scope();
        true
    }
    pub(crate) fn set_smart_effect_context(&mut self, effect: Tool, style: Style) {
        if self.tool == Tool::SmartMosaic && matches!(effect, Tool::Mosaic | Tool::Blur) {
            self.smart_effect = (effect, style);
            self.style = style;
        }
    }
    fn sync_smart_effect_from_document(&mut self) {
        if self.tool == Tool::SmartMosaic {
            if let Some(annotation) = self
                .document
                .items()
                .iter()
                .rev()
                .map(|object| &object.annotation)
                .find(|annotation| annotation.effect_regions_managed())
            {
                self.smart_effect = (annotation.tool, annotation.style);
                self.style = annotation.style;
            }
        }
    }
    pub fn pointer_down(&mut self, p: (i32, i32), tolerance: f32) {
        self.pointer_down_with_shift(p, tolerance, false);
    }
    pub fn pointer_down_with_shift(&mut self, p: (i32, i32), tolerance: f32, shift: bool) {
        self.pointer_down_at(Point::from_tuple(p), tolerance, shift);
    }
    /// 普通截图 Edit／Lasso 的宿主范围命中；图片/贴图默认输入不使用此入口。
    pub(crate) fn screenshot_crop_outside_edges(
        &self,
        p: (i32, i32),
        tolerance: f32,
    ) -> Option<u8> {
        if !matches!(self.tool, Tool::Edit | Tool::Lasso | Tool::SmartMosaic)
            || self.gesture.is_some()
            || self.text_draft.is_some()
            || (self.lasso_draft.is_some() && self.tool != Tool::Lasso)
            || (matches!(self.tool, Tool::Lasso | Tool::SmartMosaic)
                && !self.selected_ids().is_empty())
            || self.watermark.has_preview()
            || !self.canvas.contains(p)
        {
            return None;
        }
        let crop = self.crop.filter(|crop| {
            crop.valid(self.canvas.width, self.canvas.height) && !crop.contains(p)
        })?;
        // 临近外侧控制点仍以控制点为先；远处按九宫固定对应边。
        let handles = crop_edges(crop, p, tolerance);
        let edges = if handles != 0 {
            handles
        } else {
            crop_outside_edges(crop, p)
        };
        (edges != 0).then_some(edges)
    }
    /// 只开始既有 Crop resize；不结束草稿、不切工具、不改变对象选择或文档。
    pub(crate) fn begin_screenshot_crop_outside(&mut self, p: (i32, i32), tolerance: f32) -> bool {
        let Some(edges) = self.screenshot_crop_outside_edges(p, tolerance) else {
            return false;
        };
        let crop = self.crop.unwrap();
        self.gesture = Some(Gesture::Crop {
            before: self.crop,
            start: p,
            mode: CropGesture::Resize(crop, edges),
        });
        true
    }
    /// 只认宿主已接受的套索外侧 Resize，不把通用 Lasso 的 New／绘制冒充调整。
    pub(crate) fn is_lasso_crop_outside_resize(&self) -> bool {
        self.tool == Tool::Lasso
            && matches!(
                &self.gesture,
                Some(Gesture::Crop {
                    before: Some(before),
                    start,
                    mode: CropGesture::Resize(original, edges),
                }) if before == original && *edges != 0 && !original.contains(*start)
            )
    }
    /// 普通截图空文档 Edit 的内侧边/角；外侧九宫与中央移动仍使用各自入口。
    pub(crate) fn screenshot_empty_crop_inside_edges(
        &self,
        p: (i32, i32),
        tolerance: f32,
    ) -> Option<u8> {
        if self.tool != Tool::Edit
            || !self.document.items().is_empty()
            || self.gesture.is_some()
            || self.text_draft.is_some()
            || self.lasso_draft.is_some()
            || self.watermark.has_preview()
            || !self.canvas.contains(p)
        {
            return None;
        }
        let crop = self
            .crop
            .filter(|crop| crop.valid(self.canvas.width, self.canvas.height) && crop.contains(p))?;
        let edges = crop_edges(crop, p, tolerance);
        (edges != 0).then_some(edges)
    }
    pub(crate) fn begin_screenshot_empty_crop_inside(
        &mut self,
        p: (i32, i32),
        tolerance: f32,
    ) -> bool {
        let Some(edges) = self.screenshot_empty_crop_inside_edges(p, tolerance) else {
            return false;
        };
        self.gesture = Some(Gesture::Crop {
            before: self.crop,
            start: p,
            mode: CropGesture::ResizeFromInside(self.crop.unwrap(), edges),
        });
        true
    }
    /// 普通截图 Edit 空文档的中央移动资格；宿主与按键门由输入适配层共用。
    pub(crate) fn screenshot_empty_crop_center_ready(&self, p: (i32, i32), tolerance: f32) -> bool {
        if self.tool != Tool::Edit
            || !self.document.items().is_empty()
            || self.gesture.is_some()
            || self.text_draft.is_some()
            || self.lasso_draft.is_some()
            || self.watermark.has_preview()
            || !self.canvas.contains(p)
        {
            return false;
        }
        self.crop
            .filter(|crop| crop.valid(self.canvas.width, self.canvas.height))
            .is_some_and(|crop| crop.contains(p) && crop_edges(crop, p, tolerance) == 0)
    }
    /// 保持 Edit 与文档，只借用现有 Crop Move 的预览、完成和取消合同。
    pub(crate) fn begin_screenshot_empty_crop_center(
        &mut self,
        p: (i32, i32),
        tolerance: f32,
    ) -> bool {
        if !self.screenshot_empty_crop_center_ready(p, tolerance) {
            return false;
        }
        self.gesture = Some(Gesture::Crop {
            before: self.crop,
            start: p,
            mode: CropGesture::Move(self.crop.unwrap()),
        });
        true
    }
    pub fn pointer_down_at(&mut self, sample: Point, tolerance: f32, shift: bool) {
        self.repair_group_scope();
        if self.tool == Tool::Watermark {
            self.sync_watermark_from_document();
            return;
        }
        let p = sample.rounded();
        // 标注坐标不夹到画布边；选区矩形仍由 Rect::drag 留在画布内。
        let point = sample;
        if self.tool == Tool::SmartMosaic && self.crop.is_some() {
            // 候选内区归显式宿主；外侧只能走真实 screenshot_crop_outside 入口。
            return;
        }
        if self.tool == Tool::Lasso {
            if let Some(host) = self.crop.filter(|r| r.contains(p)) {
                self.lasso_draft = None;
                self.selected = None;
                self.selection.clear();
                self.gesture = Some(Gesture::Lasso {
                    points: vec![point],
                    host,
                });
                self.preview_revision += 1;
            } else {
                self.lasso_draft = None;
                self.gesture = Some(Gesture::Crop {
                    before: self.crop,
                    start: p,
                    mode: CropGesture::New,
                });
            }
            return;
        }
        if self.tool == Tool::PixelEraser && self.crop.is_some() {
            self.selected = None;
            self.selection.clear();
            self.gesture = Some(Gesture::Draw(Annotation::new(self.tool, point, self.style)));
            self.preview_revision += 1;
            return;
        }
        if matches!(self.gesture, Some(Gesture::ArrowPending(_))) {
            if let Some((id, handle)) = self.committed_path_hit(Tool::Arrow, point, tolerance) {
                self.cancel_gesture();
                self.selection = self.expand_selection(&[id]);
                self.selected = Some(id);
                if self.selection.len() > 1 {
                    self.begin_group(point, crate::selection_transform::Manipulation::Move);
                } else {
                    self.selection.clear();
                    self.begin_object(id, point, handle);
                }
            } else {
                // 命中旧箭头使用真实点；只有续画才以未提交箭头起点做 Shift 吸附。
                let point = self.constrained_point(point, shift);
                if let Some(Gesture::ArrowPending(mut a)) = self.gesture.take() {
                    a.update(point);
                    self.gesture = Some(Gesture::Draw(a));
                    self.preview_revision += 1;
                }
            }
            return;
        }
        // 已提交折线优先于续点：切换只丢弃未提交草稿，不写文档或撤销历史。
        // 命中使用真实指针；未命中时才以草稿最后一个固定节点做 Shift 吸附。
        if self.has_polyline() {
            if let Some((id, handle)) = self.committed_path_hit(Tool::Polyline, point, tolerance) {
                self.cancel_gesture();
                self.selection = self.expand_selection(&[id]);
                self.selected = Some(id);
                if self.selection.len() > 1 {
                    self.begin_group(point, crate::selection_transform::Manipulation::Move);
                } else {
                    self.selection.clear();
                    self.begin_object(id, point, handle);
                }
                return;
            }
        }
        let point = if self.has_polyline() {
            self.constrained_point(point, shift)
        } else {
            point
        };
        if let Some(Gesture::Polyline(a)) = &mut self.gesture {
            *a.points.last_mut().unwrap() = point;
            if a.points[a.points.len() - 2].distance(point) >= 2. {
                a.points.push(point);
            }
            self.preview_revision += 1;
            return;
        }
        if !self.finish_gesture() {
            return;
        }
        self.finish_text();
        let scope = self.group_scope_path();
        if matches!(self.tool, Tool::Pen | Tool::LocalFill) && self.crop.is_some() {
            // 铅笔只落笔；已有笔迹的选取和移动由显式对象编辑模式负责。
            self.selected = None;
            self.selected_node = None;
            self.selection.clear();
            self.begin_drawing(point, shift);
            return;
        }
        if self.tool == Tool::Crop || self.crop.is_none() {
            self.selected = None;
            self.selected_node = None;
            let mode = if let Some(r) = self.crop {
                let edges = crop_edges(r, p, tolerance);
                if edges != 0 {
                    CropGesture::Resize(r, edges)
                } else if r.contains(p) {
                    CropGesture::Move(r)
                } else {
                    CropGesture::Resize(r, crop_outside_edges(r, p))
                }
            } else {
                CropGesture::New
            };
            self.gesture = Some(Gesture::Crop {
                before: self.crop,
                start: p,
                mode,
            });
        } else {
            if self.tool == Tool::Eraser {
                self.gesture = Some(Gesture::Erase { ids: Vec::new() });
                self.pointer_move(p);
                return;
            }
            if !matches!(self.tool, Tool::Eraser | Tool::PixelEraser) {
                if let Some(control) = self
                    .rotation_control(tolerance)
                    .filter(|c| c.position.distance(point) <= tolerance)
                {
                    let pivot = control.pivot;
                    let angle = (point.y - pivot.y).atan2(point.x - pivot.x).to_degrees();
                    self.begin_group(
                        point,
                        crate::selection_transform::Manipulation::Rotate { pivot, angle },
                    );
                    return;
                }
                if self.selected_ids().len() > 1 {
                    if let Some(b) = self.selection_bounds() {
                        if let Some(handle) = [0, 2, 5, 7]
                            .into_iter()
                            .find(|&i| b.handles()[i].distance(point) <= tolerance)
                        {
                            self.begin_group(
                                point,
                                crate::selection_transform::Manipulation::Scale {
                                    bounds: b,
                                    handle,
                                },
                            );
                            return;
                        }
                    }
                    if self.document.items().iter().any(|o| {
                        self.selected_ids().contains(&o.id)
                            && o.annotation.hit(point, tolerance, true)
                    }) {
                        self.begin_group(point, crate::selection_transform::Manipulation::Move);
                        return;
                    }
                }
            }
            let drawing_style = self.drawing_style();
            self.selection.clear();
            if let Some(id) = self.selected {
                let a = self.document.get(id).unwrap();
                if let Some(handle) = nearest_handle(a, point, tolerance) {
                    self.begin_object(id, point, Some(handle));
                    return;
                }
                if let Some(control) = self
                    .radius_controls(tolerance)
                    .into_iter()
                    .find(|c| c.hit(point))
                {
                    self.begin_radius(id, point, control);
                    return;
                }
                if a.hit(point, tolerance, true) {
                    self.begin_object(id, point, None);
                    return;
                }
            }
            let hit = self
                .document
                .items()
                .iter()
                .rev()
                .find(|o| {
                    o.in_group_scope(&scope)
                        && (self.tool == Tool::Edit || self.tool == o.annotation.tool)
                        && o.annotation.hit(point, tolerance, self.tool == Tool::Edit)
                })
                .map(|o| o.id);
            self.selected = hit;
            if let Some(id) = hit {
                let group = self.expand_selection(&[id]);
                if group.len() > 1 {
                    self.selection = group;
                    self.begin_group(point, crate::selection_transform::Manipulation::Move);
                } else {
                    self.begin_object(id, point, None);
                }
            } else if self.tool == Tool::Edit {
                self.gesture = Some(Gesture::Marquee {
                    start: point,
                    current: point,
                });
            } else if self.tool.text() {
                self.begin_text(None, point);
            } else if self.tool == Tool::Serial {
                let mut a = Annotation::new(self.tool, point, self.style);
                a.text = (self
                    .document
                    .items()
                    .iter()
                    .filter(|o| o.annotation.tool == Tool::Serial)
                    .count()
                    + 1)
                .to_string();
                self.selected = Some(self.insert_annotation_in_group_scope(a));
            } else if self.tool == Tool::Polyline {
                let mut a = Annotation::new(self.tool, point, self.style);
                a.points.push(point);
                self.gesture = Some(Gesture::Polyline(a));
                self.preview_revision += 1;
            } else if self.tool.draws() {
                self.style = drawing_style;
                self.begin_drawing(point, shift);
            }
        }
    }
    fn begin_drawing(&mut self, point: Point, shift: bool) {
        self.style = self.drawing_style();
        if self.tool.supports_radius() {
            self.tool_styles.entry(self.tool).or_insert(self.style);
        }
        let a = Annotation::new(self.tool, point, self.style);
        self.gesture = Some(if self.tool == Tool::Pen && shift {
            Gesture::PenLine(a)
        } else {
            Gesture::Draw(a)
        });
        self.preview_revision += 1;
    }
    fn committed_path_hit(
        &self,
        tool: Tool,
        point: Point,
        tolerance: f32,
    ) -> Option<(ObjectId, Option<usize>)> {
        let scope = self.group_scope_path();
        self.document.items().iter().rev().find_map(|object| {
            if !object.in_group_scope(&scope) {
                return None;
            }
            let a = &object.annotation;
            if a.tool != tool {
                return None;
            }
            let handle = nearest_handle(a, point, tolerance);
            (handle.is_some() || a.hit(point, tolerance, true)).then_some((object.id, handle))
        })
    }
    fn begin_object(&mut self, id: ObjectId, start: Point, handle: Option<usize>) {
        let before = self.document.get(id).unwrap().clone();
        self.style = before.style;
        self.selected = Some(id);
        self.selected_node = if before.tool == Tool::Polyline {
            handle.map(|i| (id, i))
        } else {
            None
        };
        self.gesture = Some(Gesture::Object {
            id,
            current: before.clone(),
            preview_style: before.style,
            before: Box::new(before),
            start,
            handle,
            radius_delta: None,
            radius_anchor_projection: None,
            radius_control: None,
        });
    }
    fn begin_radius(
        &mut self,
        id: ObjectId,
        start: Point,
        control: crate::radius_handle::RadiusControl,
    ) {
        self.begin_object(id, start, None);
        if let Some(Gesture::Object {
            radius_delta,
            radius_control,
            ..
        }) = &mut self.gesture
        {
            *radius_delta = Some(0.);
            *radius_control = Some(control);
        }
    }
    pub fn constrained_pointer(&self, p: (i32, i32), shift: bool) -> (i32, i32) {
        self.constrained_point(Point::from_tuple(p), shift)
            .rounded()
    }
    pub fn constrained_point(&self, p: Point, shift: bool) -> Point {
        // 直线模式由按下时的 Shift 决定；松开 Shift 或按下中途 Shift 不改变笔迹类型。
        if let Some(Gesture::PenLine(a)) = &self.gesture {
            return pen_line_point(a.points[0], p);
        }
        if !shift {
            return p;
        }
        // 真实指针尚未越过节点拖动门时，Shift 不能把一次纯点击吸附成移动。
        // 草稿切到旧折线或箭头后，节点角度基准可能与草稿完全不同。
        if matches!(&self.gesture, Some(Gesture::Object {
            before, start, handle: Some(_), ..
        }) if matches!(before.tool, Tool::Arrow | Tool::Polyline) && p.distance(*start) < 4.)
        {
            return p;
        }
        let constrained = match &self.gesture {
            Some(Gesture::Object {
                radius_delta: Some(_),
                ..
            }) => p,
            Some(Gesture::Group {
                mode: crate::selection_transform::Manipulation::Rotate { pivot, angle },
                ..
            }) => {
                let (dx, dy) = (p.x - pivot.x, p.y - pivot.y);
                let delta = dy.atan2(dx).to_degrees() - angle;
                let next = (angle + (delta / 15.).round() * 15.).to_radians();
                let radius = dx.hypot(dy);
                Point {
                    x: pivot.x + radius * next.cos(),
                    y: pivot.y + radius * next.sin(),
                }
            }
            Some(Gesture::Draw(a) | Gesture::ArrowPending(a)) if a.tool == Tool::Arrow => {
                snap_angle(a.points[0], p)
            }
            Some(Gesture::Draw(a)) if a.is_highlighter_line() => {
                constrained_line_point(a.points[0], p, std::f32::consts::FRAC_PI_4)
            }
            Some(Gesture::Polyline(a)) => snap_angle(a.points[a.points.len() - 2], p),
            Some(Gesture::Draw(a))
                if matches!(
                    a.tool,
                    Tool::Rectangle
                        | Tool::Ellipse
                        | Tool::Triangle
                        | Tool::Diamond
                        | Tool::Star
                        | Tool::HighlighterRect
                        | Tool::Mosaic
                        | Tool::Blur
                        | Tool::Spotlight
                ) =>
            {
                aspect_point(a.points[0], p, 1., None)
            }
            Some(Gesture::Object {
                before,
                handle: Some(handle),
                ..
            }) => {
                if before.is_highlighter_line() && before.has_endpoint_handles() && *handle < 2 {
                    constrained_line_point(
                        before.to_world(before.points[1 - handle]),
                        p,
                        std::f32::consts::FRAC_PI_4,
                    )
                } else if before.has_endpoint_handles() && *handle < 2 {
                    pen_line_point(before.to_world(before.points[1 - handle]), p)
                } else if before.tool == Tool::Arrow && *handle < 2 {
                    snap_angle(before.to_world(before.points[1 - handle]), p)
                } else if before.tool == Tool::Arrow {
                    p
                } else if before.tool == Tool::Polyline {
                    let anchor = if *handle > 0 { handle - 1 } else { 1 };
                    snap_angle(before.to_world(before.points[anchor]), p)
                } else if before.rotation != 0. && [0, 2, 5, 7].contains(handle) {
                    let local = before.to_local(Point { x: p.x, y: p.y });
                    let b = before.local_bounds();
                    let anchor = b.handles()[7 - handle];
                    let (dx, dy) = (local.x - anchor.x, local.y - anchor.y);
                    let width = dx.abs().max(dy.abs() * b.width / b.height.max(1.));
                    before.to_world(Point {
                        x: anchor.x + width * dx.signum(),
                        y: anchor.y + width * b.height / b.width.max(1.) * dy.signum(),
                    })
                } else if [0, 2, 5, 7].contains(handle) {
                    let b = before.bounds();
                    aspect_point(b.handles()[7 - handle], p, b.width / b.height.max(1.), None)
                } else {
                    p
                }
            }
            Some(Gesture::Crop {
                start,
                mode: CropGesture::New,
                ..
            }) => aspect_point(
                Point {
                    x: start.0 as f32,
                    y: start.1 as f32,
                },
                p,
                1.,
                Some(self.canvas),
            ),
            Some(Gesture::Crop {
                start,
                mode: CropGesture::ResizeFromInside(r, edges),
                ..
            }) if edges & 3 != 0 && edges & 12 != 0 => {
                let anchor = Point {
                    x: if edges & 1 != 0 { r.x + r.width } else { r.x } as f32,
                    y: if edges & 4 != 0 { r.y + r.height } else { r.y } as f32,
                };
                let edge = crop_resize_inside_point(*r, *edges, *start, p.rounded());
                let constrained = aspect_point(
                    anchor,
                    Point::from_tuple(edge),
                    r.width as f32 / r.height.max(1) as f32,
                    Some(self.canvas),
                );
                // 真实边已由 aspect_point 夹紧；代理指针不能再被画布夹紧而丢失偏移。
                return Point {
                    x: p.x + constrained.x - edge.0 as f32,
                    y: p.y + constrained.y - edge.1 as f32,
                };
            }
            Some(Gesture::Crop {
                mode: CropGesture::Resize(r, edges),
                ..
            }) if edges & 3 != 0 && edges & 12 != 0 => {
                let anchor = Point {
                    x: if edges & 1 != 0 { r.x + r.width } else { r.x } as f32,
                    y: if edges & 4 != 0 { r.y + r.height } else { r.y } as f32,
                };
                aspect_point(
                    anchor,
                    p,
                    r.width as f32 / r.height.max(1) as f32,
                    Some(self.canvas),
                )
            }
            _ => p,
        };
        // 只有选区需要留在画布内；标注的 Shift 吸附可以越过边框。
        if matches!(self.gesture, Some(Gesture::Crop { .. })) {
            constrained.within(self.canvas)
        } else {
            constrained
        }
    }
    pub fn hover(&self, p: (i32, i32), tolerance: f32) -> Hover {
        self.hover_at(Point::from_tuple(p), tolerance)
    }
    pub fn hover_at(&self, sample: Point, tolerance: f32) -> Hover {
        let scope = self.group_scope_path();
        let p = sample.rounded();
        let point = sample;
        if self
            .text_draft
            .as_ref()
            .is_some_and(|d| d.annotation.bounds().contains(point, 0.))
        {
            return Hover::Text;
        }
        match &self.gesture {
            Some(Gesture::Object {
                before,
                radius_delta: Some(_),
                radius_control,
                ..
            }) => {
                return radius_control
                    .as_ref()
                    .map_or(Hover::Crosshair, |c| radius_hover(before, c))
            }
            Some(Gesture::Object { before, handle, .. }) => {
                return handle.map_or(Hover::Move, |h| {
                    if matches!(before.tool, Tool::Arrow | Tool::Polyline) {
                        Hover::Crosshair
                    } else {
                        resize_hover(before, h)
                    }
                })
            }
            Some(Gesture::Group { mode, .. }) => {
                return match mode {
                    crate::selection_transform::Manipulation::Move => Hover::Move,
                    crate::selection_transform::Manipulation::Scale { handle, .. } => {
                        Hover::handle(*handle)
                    }
                    crate::selection_transform::Manipulation::Rotate { .. } => Hover::Crosshair,
                }
            }
            Some(Gesture::Crop {
                mode: CropGesture::Resize(_, edges) | CropGesture::ResizeFromInside(_, edges),
                ..
            }) => return Hover::edges(*edges),
            Some(Gesture::Crop {
                mode: CropGesture::Move(_),
                ..
            }) => return Hover::Move,
            Some(Gesture::Polyline(_) | Gesture::ArrowPending(_)) => {
                let tool = if self.has_polyline() {
                    Tool::Polyline
                } else {
                    Tool::Arrow
                };
                return self.committed_path_hit(tool, point, tolerance).map_or(
                    Hover::Crosshair,
                    |(id, handle)| {
                        if handle.is_some() && self.expand_selection(&[id]).len() == 1 {
                            Hover::Crosshair
                        } else {
                            Hover::Move
                        }
                    },
                );
            }
            Some(_) => return Hover::Crosshair,
            None => {}
        }
        if self.tool == Tool::Crop || self.crop.is_none() {
            if let Some(r) = self.crop {
                let edges = crop_edges(r, p, tolerance);
                if edges != 0 {
                    return Hover::edges(edges);
                }
                if r.contains(p) {
                    return Hover::Move;
                }
            }
            return Hover::Crosshair;
        }
        if self.tool == Tool::Pen {
            return Hover::Crosshair;
        }
        if !matches!(self.tool, Tool::Eraser | Tool::PixelEraser) {
            if self
                .rotation_handle(tolerance)
                .is_some_and(|h| h.distance(point) <= tolerance)
            {
                return Hover::Crosshair;
            }
            if self.selected_ids().len() > 1 {
                if let Some(b) = self.selection_bounds() {
                    for i in [0, 2, 5, 7] {
                        if b.handles()[i].distance(point) <= tolerance {
                            return Hover::handle(i);
                        }
                    }
                }
            }
            if let Some(a) = self.selected_annotation() {
                if self.selection.len() <= 1 {
                    if let Some(h) = nearest_handle(a, point, tolerance) {
                        return if matches!(a.tool, Tool::Arrow | Tool::Polyline)
                            || a.is_highlighter_line()
                        {
                            Hover::Crosshair
                        } else {
                            resize_hover(a, h)
                        };
                    }
                    if let Some(c) = self
                        .radius_controls(tolerance)
                        .into_iter()
                        .find(|c| c.hit(point))
                    {
                        return radius_hover(a, &c);
                    }
                }
                if a.hit(point, tolerance, true) {
                    return Hover::Move;
                }
            }
            if self.document.items().iter().rev().any(|o| {
                o.in_group_scope(&scope)
                    && (self.tool == Tool::Edit || self.tool == o.annotation.tool)
                    && o.annotation.hit(point, tolerance, self.tool == Tool::Edit)
            }) {
                return Hover::Move;
            }
        }
        if self.tool.text() {
            Hover::Text
        } else if self.tool == Tool::Edit {
            Hover::Arrow
        } else {
            Hover::Crosshair
        }
    }
    pub fn wheel_parameter(&self, shift: bool, control: bool) -> Option<Parameter> {
        if shift && !self.radius_selection().targets.is_empty() {
            return Some(Parameter::Radius);
        }
        let selected = self.selected_annotation();
        let tool = selected.map_or(self.tool, |a| a.tool);
        // 复合效果与普通矩形共用 Tool；对象资格决定是否可用圆角。
        let shift = shift && selected.is_none_or(|a| a.supports_radius());
        Parameter::for_tool(tool, shift, control)
    }
    pub fn adjust_parameter(
        &mut self,
        parameter: Parameter,
        steps: i32,
        group: u64,
        p: (i32, i32),
        tolerance: f32,
    ) -> bool {
        if steps == 0 {
            return false;
        }
        if parameter == Parameter::Crop {
            return self.wheel_crop(p, tolerance, steps);
        }
        if parameter == Parameter::Radius {
            let snapshot = self.radius_selection();
            if snapshot.targets.is_empty() {
                let style = rectangle_radius::wheel_style(self.style, snapshot.tool, steps);
                if snapshot.selected_count != 0
                    || !snapshot.tool.supports_radius()
                    || style == self.style
                {
                    return false;
                }
                self.style_group = Some(group);
                self.set_style(style);
                self.style_group = None;
                return true;
            }
            let objects = self.selected_objects();
            let values: Vec<_> = objects
                .iter()
                .filter(|(_, a)| a.supports_radius())
                .map(|(id, a)| (*id, rectangle_radius::wheel_style(a.style, a.tool, steps)))
                .collect();
            if values.iter().all(|(id, after)| {
                objects
                    .iter()
                    .any(|(other, a)| other == id && a.style == *after)
            }) {
                return false;
            }
            if snapshot.selected_count == 1 {
                self.remember_single_radius(values[0].1.radius_for(snapshot.tool));
            }
            self.style_group = Some(group);
            self.update_selected(|a| {
                if a.supports_radius() {
                    let after = rectangle_radius::wheel_style(a.style, a.tool, steps);
                    a.set_corner_radius(after.radius_for(a.tool));
                    if a.tool == Tool::Rectangle {
                        a.style.rectangle_radii = after.rectangle_radii;
                    }
                }
            });
            self.style_group = None;
            if let Some((tool, style)) = self
                .selected_annotation()
                .filter(|a| a.supports_radius())
                .map(|a| (a.tool, a.style))
            {
                self.style.set_radius_for(tool, style.radius_for(tool));
                if tool == Tool::Rectangle {
                    self.style.rectangle_radii = style.rectangle_radii;
                }
            }
            return true;
        }
        let tool = self.selected_annotation().map_or(self.tool, |a| a.tool);
        let style = parameter.adjusted(self.style, tool, steps);
        if style == self.style {
            return false;
        }
        self.style_group = Some(group);
        self.set_style(style);
        self.style_group = None;
        true
    }
    /// 普通截图确认范围的中心扩缩；宿主路由在调用前判定，不提交草稿或改变对象选择。
    pub(crate) fn adjust_crop_from_center(&mut self, steps: i32) -> bool {
        if !crate::selection_wheel::center_ready(self, false, false) {
            return false;
        }
        let Some(before) = self.crop else {
            return false;
        };
        let Some(next) = crate::selection_wheel::crop_from_center(self.canvas, before, steps)
        else {
            return false;
        };
        if next == before {
            return false;
        }
        self.document.invalidate_crop_mask();
        self.crop = Some(next);
        true
    }
    fn wheel_crop(&mut self, p: (i32, i32), tolerance: f32, steps: i32) -> bool {
        if self.gesture.is_some() {
            return false;
        }
        let Some(r) = self.crop else {
            return false;
        };
        let edges = crop_edges(r, p, tolerance);
        if edges == 0 && !r.contains(p) {
            return false;
        }
        let (mut x, mut y, mut right, mut bottom) = (
            r.x as i32,
            r.y as i32,
            (r.x + r.width) as i32,
            (r.y + r.height) as i32,
        );
        if edges == 0 {
            let max = x
                .min(y)
                .min(self.canvas.width as i32 - right)
                .min(self.canvas.height as i32 - bottom);
            let min = -((r.width.min(r.height) as i32 - 1) / 2);
            let d = steps.clamp(min, max);
            x -= d;
            y -= d;
            right += d;
            bottom += d;
        } else {
            if edges & 1 != 0 {
                x = (x - steps).clamp(0, right - 1);
            }
            if edges & 2 != 0 {
                right = (right + steps).clamp(x + 1, self.canvas.width as i32);
            }
            if edges & 4 != 0 {
                y = (y - steps).clamp(0, bottom - 1);
            }
            if edges & 8 != 0 {
                bottom = (bottom + steps).clamp(y + 1, self.canvas.height as i32);
            }
        }
        let next = Rect {
            x: x as u32,
            y: y as u32,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        };
        if next != r {
            self.lasso_draft = None;
            self.document.invalidate_crop_mask();
        }
        self.crop = Some(next);
        next != r
    }
    pub fn pointer_move(&mut self, p: (i32, i32)) {
        self.pointer_move_at(Point::from_tuple(p));
    }
    pub fn pointer_move_at(&mut self, sample: Point) {
        let scope = self.group_scope_path();
        let p = sample.rounded();
        let point = sample;
        match &mut self.gesture {
            Some(Gesture::Lasso { points, host }) => {
                let next = sample.within(*host);
                if points.last().is_none_or(|p| p.distance(next) >= 0.5) {
                    points.push(next);
                    self.preview_revision += 1;
                }
            }
            Some(Gesture::Crop { start, mode, .. }) => {
                self.crop = Some(match mode {
                    CropGesture::New => {
                        Rect::drag(*start, p, self.canvas.width, self.canvas.height)
                    }
                    CropGesture::Move(r) => Rect {
                        x: (r.x as i32 + p.0 - start.0)
                            .clamp(0, (self.canvas.width - r.width) as i32)
                            as u32,
                        y: (r.y as i32 + p.1 - start.1)
                            .clamp(0, (self.canvas.height - r.height) as i32)
                            as u32,
                        ..*r
                    },
                    CropGesture::Resize(r, edges) => crop_resize(*r, *edges, p, self.canvas),
                    CropGesture::ResizeFromInside(r, edges) => crop_resize(
                        *r,
                        *edges,
                        crop_resize_inside_point(*r, *edges, *start, p),
                        self.canvas,
                    ),
                });
            }
            Some(Gesture::Polyline(a)) => {
                *a.points.last_mut().unwrap() = point;
                self.preview_revision += 1;
            }
            Some(Gesture::Marquee { current, .. }) => {
                *current = point;
            }
            Some(Gesture::Erase { ids }) => {
                for o in self.document.items() {
                    if o.in_group_scope(&scope)
                        && !ids.contains(&o.id)
                        && o.annotation
                            .hit(point, self.style.width.max(12.) / 2., true)
                    {
                        ids.push(o.id);
                    }
                }
                self.preview_revision += 1;
            }
            Some(Gesture::Group {
                before,
                current,
                start,
                mode,
                point: last,
            }) => {
                *last = point;
                crate::selection_transform::preview(before, current, *start, point, *mode);
                let primary = self.selected;
                if let Some((_, a)) = current
                    .iter()
                    .find(|(id, _)| Some(*id) == primary)
                    .or_else(|| current.last())
                {
                    self.style = a.style;
                }
                self.preview_revision += 1;
            }
            Some(Gesture::PenLine(a)) => {
                let next = pen_line_point(a.points[0], sample);
                a.points.truncate(1);
                if next.distance(a.points[0]) > 0.01 {
                    a.points.push(next);
                }
                self.preview_revision += 1;
            }
            Some(Gesture::Draw(a) | Gesture::ArrowPending(a)) => {
                a.update(point);
                self.preview_revision += 1;
            }
            Some(Gesture::Object {
                before,
                current,
                start,
                handle,
                radius_delta,
                radius_anchor_projection,
                radius_control,
                preview_style,
                ..
            }) => {
                let mut base = (**before).clone();
                let center = crate::transform::center(base.local_bounds());
                base.style = *preview_style;
                if preview_style.font_size != before.style.font_size {
                    fit_text_bounds(&mut base);
                }
                base.preserve_rotation_frame(center);
                *current = base.clone();
                if let Some(delta) = radius_delta {
                    let projection = radius_control
                        .map_or(0., |control| control.drag_delta(&base, *start, point));
                    let movement = match *radius_anchor_projection {
                        Some(anchor) => projection - anchor,
                        None if point.distance(*start) < 4. => 0.,
                        None => projection,
                    };
                    let corner = radius_control.map_or(0, |c| c.corner);
                    current.set_corner_handle_radius(
                        corner,
                        base.radius_after_corner_delta(corner, movement),
                    );
                    *delta = projection;
                    self.style
                        .set_radius_for(current.tool, current.corner_radius());
                    if current.tool == Tool::Rectangle {
                        self.style.rectangle_radii = current.style.rectangle_radii;
                    }
                    self.preview_revision += 1;
                    return;
                }
                if point.distance(*start) < 4. {
                    self.preview_revision += 1;
                    return;
                }
                if let Some(handle) = *handle {
                    let center = crate::transform::center(base.local_bounds());
                    let local = base.to_local(point);
                    if before.has_endpoint_handles() && handle < 2 {
                        current.points[handle] = local;
                        current.preserve_rotation_frame(center);
                    } else if before.tool == Tool::Arrow {
                        current.move_arrow_handle(handle, local);
                        current.preserve_rotation_frame(center);
                    } else if before.tool == Tool::Polyline {
                        current.points[handle] = local;
                        current.preserve_rotation_frame(center);
                    } else {
                        let bounds = if before.rotation == 0. {
                            resize_bounds(before.local_bounds(), handle, local)
                        } else {
                            resize_rotated_bounds(before.local_bounds(), handle, local)
                        };
                        *current = base.resized(bounds);
                        if preview_style.font_size != before.style.font_size {
                            current.style.font_size = preview_style.font_size;
                            fit_text_bounds(current);
                        }
                    }
                } else {
                    current.translate(point.x - start.x, point.y - start.y);
                }
                self.preview_revision += 1;
            }
            None => {}
        }
    }
    pub fn pointer_up(&mut self, p: (i32, i32)) {
        self.pointer_up_at(Point::from_tuple(p));
    }
    pub fn pointer_up_at(&mut self, p: Point) {
        self.pointer_move_at(p);
        if let Some(Gesture::Object {
            current,
            start,
            handle: Some(2),
            ..
        }) = &mut self.gesture
        {
            // 中点靠近直线时的自动拉直只属于实际拖动；纯点击不能改变曲线和 redo。
            if current.tool == Tool::Arrow && p.distance(*start) >= 4. {
                current.snap_arrow_midpoint();
            }
        }
        if self.gesture.as_ref().is_some_and(|g| matches!(g, Gesture::Draw(a) if a.tool == Tool::Arrow && a.points[0].distance(a.points[1]) < 4.)) {
            if let Some(Gesture::Draw(a)) = self.gesture.take() { self.gesture = Some(Gesture::ArrowPending(a)); }
            self.preview_revision += 1;
        } else if !matches!(self.gesture, Some(Gesture::Polyline(_) | Gesture::ArrowPending(_))) {
            self.finish_gesture();
        }
    }
    pub fn finish_gesture(&mut self) -> bool {
        // 核心背压：漏接 UI 门也不能先提交再因算法失败丢失草稿。
        if self.local_fill_candidate().is_some() {
            return false;
        }
        let scope = self.group_scope_path();
        match self.gesture.take() {
            Some(Gesture::Lasso { points, .. }) => {
                self.lasso_draft = crate::crop_mask::Polygon::new(&points);
                self.preview_revision += 1;
            }
            Some(Gesture::Polyline(mut a)) => {
                // 尾部始终为临时预览；Enter、右键、换工具只保留已点击的节点。
                a.points.pop();
                if a.valid() {
                    self.selected = Some(self.insert_annotation_in_group_scope(a));
                }
                self.preview_revision += 1;
            }
            Some(Gesture::Erase { ids }) => {
                self.document.remove_many(&ids);
                self.selected = None;
                self.selected_node = None;
                self.selection.clear();
                self.preview_revision += 1;
            }
            Some(Gesture::Group { current, .. }) => {
                let layer = current
                    .iter()
                    .rev()
                    .find(|(_, a)| a.tool == Tool::Spotlight)
                    .map(|(_, a)| a.style);
                let objects = self.spotlight_replacements(current, layer);
                self.document.replace_many(&objects);
                self.preview_revision += 1;
            }
            Some(Gesture::Marquee { start, current }) => {
                let b = Bounds {
                    x: start.x.min(current.x),
                    y: start.y.min(current.y),
                    width: (current.x - start.x).abs(),
                    height: (current.y - start.y).abs(),
                };
                self.selection = self
                    .document
                    .items()
                    .iter()
                    .filter(|o| {
                        if matches!(o.annotation.tool, Tool::LocalFill | Tool::Watermark)
                            || !o.in_group_scope(&scope)
                        {
                            return false;
                        }
                        let r = o.annotation.bounds();
                        r.x <= b.x + b.width
                            && r.x + r.width >= b.x
                            && r.y <= b.y + b.height
                            && r.y + r.height >= b.y
                    })
                    .map(|o| o.id)
                    .collect();
                self.selection = self.expand_selection(&self.selection);
                self.selected = self.selection.last().copied();
                self.sync_primary_selection_style();
            }
            Some(Gesture::Draw(a) | Gesture::PenLine(a) | Gesture::ArrowPending(a)) => {
                let b = a.bounds();
                let enough = a.tool.freehand()
                    || if a.tool == Tool::Arrow {
                        a.points[0].distance(a.points[1]) >= 4.
                    } else {
                        b.width >= 4. && b.height >= 4.
                    };
                if a.valid() && enough {
                    if a.supports_radius() {
                        self.tool_styles
                            .entry(a.tool)
                            .or_insert_with(|| crate::tool_presets::default_style(a.tool))
                            .set_radius_for(a.tool, a.corner_radius());
                        if a.tool == Tool::Rectangle {
                            self.tool_styles
                                .get_mut(&a.tool)
                                .expect("已保存矩形预设")
                                .rectangle_radii = a.style.rectangle_radii;
                        }
                    }
                    let select = !matches!(a.tool, Tool::Pen | Tool::PixelEraser | Tool::LocalFill);
                    let id = if a.tool == Tool::Spotlight {
                        let objects = self.spotlight_replacements(Vec::new(), Some(a.style));
                        let index = if self
                            .document
                            .items()
                            .iter()
                            .any(|o| o.annotation.tool == Tool::Spotlight)
                        {
                            self.document.items().len()
                        } else {
                            0
                        };
                        let scope = self.group_scope_path();
                        if scope.is_empty() {
                            if index == 0 {
                                self.document
                                    .insert_with_replacements_at(a, &objects, index)
                            } else {
                                self.document.insert_with_replacements(a, &objects)
                            }
                        } else {
                            self.document
                                .insert_with_replacements_in_scope_at(a, &objects, index, &scope)
                        }
                    } else {
                        self.insert_annotation_in_group_scope(a)
                    };
                    // 连续画笔完成后保持未选中；切到对象编辑后仍可编辑笔迹。
                    self.selected = select.then_some(id);
                }
                self.preview_revision += 1;
            }
            Some(Gesture::Object {
                id,
                current,
                before,
                radius_delta,
                preview_style,
                ..
            }) => {
                if current.valid() {
                    if radius_delta.is_some() {
                        self.style
                            .set_radius_for(current.tool, current.corner_radius());
                        if current.tool == Tool::Rectangle {
                            self.style.rectangle_radii = current.style.rectangle_radii;
                        }
                    }
                    if (radius_delta.is_some()
                        || preview_style.radius_for(before.tool) != before.corner_radius())
                        && current.corner_radius() != before.corner_radius()
                    {
                        self.remember_single_radius(current.corner_radius());
                    }
                    let shared_changed = current.style.color != before.style.color
                        || current.style.width != before.style.width
                        || current.style.opacity != before.style.opacity;
                    let layer = (current.tool == Tool::Spotlight
                        && (radius_delta.is_none() || shared_changed))
                        .then_some(current.style);
                    let objects = self.spotlight_replacements(vec![(id, current)], layer);
                    self.document.replace_many(&objects);
                }
                self.preview_revision += 1;
            }
            Some(Gesture::Crop { before, .. }) => {
                if self
                    .crop
                    .is_none_or(|r| !r.valid(self.canvas.width, self.canvas.height))
                {
                    self.crop = before;
                }
                if self.crop != before {
                    self.lasso_draft = None;
                    self.document.invalidate_crop_mask();
                }
            }
            None => {}
        }
        self.repair_group_scope();
        true
    }
    pub fn lasso_points(&self) -> Option<Vec<Point>> {
        if let Some(Gesture::Lasso { points, .. }) = &self.gesture {
            Some(points.clone())
        } else {
            self.lasso_draft
                .as_ref()
                .or(self.document.crop_mask.as_ref())
                .map(|p| p.points.to_vec())
        }
    }
    pub fn drawing_lasso(&self) -> bool {
        matches!(self.gesture, Some(Gesture::Lasso { .. }))
    }
    pub fn apply_lasso(&mut self) -> bool {
        // 范围手势尚未收尾，不能把原闭合草稿应用到正在变化的宿主范围。
        if self.is_changing_crop() {
            return false;
        }
        if let Some(draft) = self.lasso_draft.take() {
            self.document.set_crop_mask(Some(draft));
            self.preview_revision += 1;
            true
        } else {
            false
        }
    }
    pub fn clear_lasso(&mut self) {
        self.cancel_gesture();
        self.lasso_draft = None;
        self.document.set_crop_mask(None);
        self.preview_revision += 1;
    }
    pub fn cancel_gesture(&mut self) -> bool {
        // Crop 取消恢复原范围及其闭合草稿；独立套索取消仍沿原丢稿语义。
        let draft_cancelled = !self.is_changing_crop() && self.lasso_draft.take().is_some();
        if let Some(gesture) = self.gesture.take() {
            match gesture {
                Gesture::Crop { before, .. } => self.crop = before,
                Gesture::Object { before, .. } => {
                    self.style
                        .set_radius_for(before.tool, before.corner_radius());
                    if before.tool == Tool::Rectangle {
                        self.style.rectangle_radii = before.style.rectangle_radii;
                    }
                }
                Gesture::Group { .. } => {
                    // Group.before 是可重锚的缩放基准；取消恢复真正未提交的文档样式。
                    if let Some((tool, style)) = self
                        .selected
                        .and_then(|id| self.document.get(id))
                        .map(|a| (a.tool, a.style))
                    {
                        self.style.set_radius_for(tool, style.radius_for(tool));
                        if tool == Tool::Rectangle {
                            self.style.rectangle_radii = style.rectangle_radii;
                        }
                    }
                }
                _ => {}
            }
            self.preview_revision += 1;
            true
        } else {
            if draft_cancelled {
                self.preview_revision += 1;
            }
            draft_cancelled
        }
    }
    pub fn select_tool(&mut self, tool: Tool) {
        let preferred_watermark = self.selected;
        self.finish_watermark_input();
        let (effect_tool, effect_style) = self.effect_context();
        if self.has_arrow_draft() || (self.tool == Tool::Lasso && tool != Tool::Lasso) {
            self.cancel_gesture();
            // Crop 取消保稿只属于本次范围手势；离开套索工具仍丢弃未应用草稿。
            if self.tool == Tool::Lasso && tool != Tool::Lasso && self.lasso_draft.take().is_some()
            {
                self.preview_revision += 1;
            }
        }
        if !self.finish_gesture() {
            return;
        }
        self.finish_text();
        self.repair_group_scope();
        if self.tool.draws() || self.tool == Tool::Eraser {
            self.tool_styles.insert(self.tool, self.drawing_style());
        }
        self.tool = tool;
        if matches!(
            tool,
            Tool::Crop | Tool::Lasso | Tool::SmartMosaic | Tool::Watermark
        ) {
            self.reset_group_scope();
        }
        if tool == Tool::SmartMosaic {
            self.smart_effect = (
                if matches!(effect_tool, Tool::Blur | Tool::BlurBrush) {
                    Tool::Blur
                } else {
                    Tool::Mosaic
                },
                effect_style,
            );
            self.style = effect_style;
        }
        match tool {
            Tool::Mosaic | Tool::Blur => self.effect_brush_mode = false,
            Tool::MosaicBrush | Tool::BlurBrush => self.effect_brush_mode = true,
            Tool::LocalFill
                if matches!(
                    effect_tool,
                    Tool::Mosaic | Tool::Blur | Tool::MosaicBrush | Tool::BlurBrush
                ) =>
            {
                self.effect_brush_mode = effect_tool.freehand();
            }
            _ => {}
        }
        if tool.draws() || tool == Tool::Eraser {
            self.style = self
                .tool_styles
                .get(&tool)
                .copied()
                .unwrap_or_else(|| crate::tool_presets::default_style(tool));
        }
        if tool == Tool::Spotlight {
            self.sync_spotlight_default();
        }
        self.selected = None;
        self.selected_node = None;
        self.selection.clear();
        if tool == Tool::Watermark {
            self.activate_watermark(preferred_watermark);
        }
    }
    pub fn set_crop(&mut self, crop: Option<Rect>) {
        self.cancel_gesture();
        self.finish_text();
        self.selection.clear();
        if self.crop != crop {
            self.document.invalidate_crop_mask();
        }
        self.crop = crop;
        self.select_tool(Tool::Crop);
        self.selected = None;
        self.selected_node = None;
    }
    /// 精确尺寸只改已完成矩形范围；不结束编辑草稿，也不切换工具或对象选择。
    pub(crate) fn apply_precise_crop(
        &mut self,
        expected: Rect,
        requested: Rect,
    ) -> Result<bool, &'static str> {
        if self.crop != Some(expected) {
            return Err(tr!(
                "选区已改变，请重新打开尺寸输入",
                "Selection changed; reopen the size input"
            ));
        }
        if !requested.valid(self.canvas.width, self.canvas.height) {
            return Err(tr!(
                "选区必须完整位于桌面内，宽高须大于 0",
                "Selection must fit the desktop with positive width and height"
            ));
        }
        if self.gesture.is_some()
            || self.text_draft.is_some()
            || self.lasso_draft.is_some()
            || self.watermark.has_preview()
            || self.local_fill_candidate().is_some()
        {
            return Err(tr!(
                "请先完成当前编辑，再调整选区尺寸",
                "Finish the current edit before changing the selection size"
            ));
        }
        if expected == requested {
            return Ok(false);
        }
        self.document.invalidate_crop_mask();
        self.crop = Some(requested);
        self.preview_revision += 1;
        Ok(true)
    }
    /// 智能模式中的完成范围更新不切换到普通 Crop，仍保留真实 mask 失效口径。
    pub(crate) fn set_smart_crop(&mut self, crop: Option<Rect>) {
        if self.tool != Tool::SmartMosaic {
            return;
        }
        self.cancel_gesture();
        if self.crop != crop {
            self.document.invalidate_crop_mask();
        }
        self.crop = crop;
    }
    pub fn undo(&mut self) {
        self.finish_watermark_input();
        if self.drawing_lasso() || self.lasso_draft.is_some() {
            self.cancel_gesture();
            return;
        }
        self.cancel_gesture();
        self.cancel_text();
        self.selection.clear();
        self.document.undo();
        self.repair_group_scope();
        self.sync_smart_effect_from_document();
        self.selected = None;
        self.selected_node = None;
        self.sync_spotlight_default();
        self.sync_watermark_from_document();
    }
    /// 与外部历史绑定的命令：仅当它仍在栈顶时撤销，状态同步与普通撤销一致。
    pub(crate) fn undo_exact(&mut self, change: &crate::document::HistoryCommand) -> bool {
        if self.document.last_undo() != Some(change) {
            return false;
        }
        self.finish_watermark_input();
        self.cancel_gesture();
        self.cancel_text();
        self.selection.clear();
        self.document.undo_exact(change);
        self.repair_group_scope();
        self.sync_smart_effect_from_document();
        self.selected = None;
        self.selected_node = None;
        self.sync_spotlight_default();
        self.sync_watermark_from_document();
        true
    }
    pub fn redo(&mut self) {
        self.finish_watermark_input();
        self.cancel_gesture();
        self.cancel_text();
        self.selection.clear();
        self.document.redo();
        self.repair_group_scope();
        self.sync_smart_effect_from_document();
        self.selected = None;
        self.selected_node = None;
        self.sync_spotlight_default();
        self.sync_watermark_from_document();
    }
    pub fn clear(&mut self) {
        self.finish_watermark_input();
        self.cancel_gesture();
        self.lasso_draft = None;
        self.cancel_text();
        self.selection.clear();
        let scope = self.group_scope_path();
        if scope.is_empty() {
            self.document.clear();
        } else {
            let ids: Vec<_> = self
                .document
                .items()
                .iter()
                .filter(|object| object.in_group_scope(&scope))
                .map(|object| object.id)
                .collect();
            self.document.remove_many(&ids);
        }
        self.repair_group_scope();
        self.selected = None;
        self.selected_node = None;
        self.sync_watermark_from_document();
    }
    pub fn delete_selected(&mut self) {
        self.finish_watermark_input();
        self.cancel_gesture();
        let ids = if self.tool == Tool::Watermark {
            self.watermark
                .target()
                .filter(|id| self.object_in_group_scope(*id))
                .into_iter()
                .collect()
        } else {
            self.selected_ids()
        };
        if ids.len() == 1 {
            self.document.remove(ids[0]);
        } else {
            self.document.remove_many(&ids);
        }
        self.repair_group_scope();
        self.selected = None;
        self.selected_node = None;
        self.selection.clear();
        self.sync_watermark_from_document();
    }
    fn update_selected(&mut self, mut update: impl FnMut(&mut Annotation)) {
        self.update_selected_with_spotlight(&mut update, None);
    }
    fn update_selected_with_spotlight(
        &mut self,
        mut update: impl FnMut(&mut Annotation),
        layer: Option<Style>,
    ) {
        let mut apply = |a: &mut Annotation| {
            let before = a.effect_regions_managed().then(|| a.clone());
            let center = crate::transform::center(a.local_bounds());
            update(a);
            a.preserve_rotation_frame(center);
            if before.as_ref().is_some_and(|before| *a != *before) {
                a.clear_effect_regions_management();
            }
        };
        // 属性变更加入当前预览，抬起时和几何修改一起提交。
        match &mut self.gesture {
            Some(
                Gesture::Draw(a)
                | Gesture::PenLine(a)
                | Gesture::Polyline(a)
                | Gesture::ArrowPending(a),
            ) => {
                apply(a);
                if a.tool == Tool::Spotlight {
                    if let Some(layer) = layer {
                        copy_spotlight_layer_style(&mut a.style, layer);
                    }
                }
                self.preview_revision += 1;
                return;
            }
            Some(Gesture::Object {
                before,
                current,
                preview_style,
                radius_delta,
                radius_anchor_projection,
                ..
            }) => {
                let previous_radius = current.corner_radius();
                let previous_corners = current.style.rectangle_radii;
                let baseline_radius = preview_style.radius_for(current.tool);
                let baseline_corners = preview_style.rectangle_radii;
                apply(current);
                if current.tool == Tool::Spotlight {
                    if let Some(layer) = layer {
                        copy_spotlight_layer_style(&mut current.style, layer);
                    }
                }
                let mut base = (**before).clone();
                base.style = *preview_style;
                apply(&mut base);
                if let Some(projection) = *radius_delta {
                    if current.corner_radius() != previous_radius
                        || current.style.rectangle_radii != previous_corners
                    {
                        // 轮调或参数输入在当前指针重锚，基准始终为合法半径。
                        base.set_corner_radius(current.corner_radius());
                        if base.tool == Tool::Rectangle {
                            base.style.rectangle_radii = current.style.rectangle_radii;
                        }
                        *radius_anchor_projection = Some(projection);
                    } else {
                        // 闭包可能赋整套 Style；未修改圆角时不能将鼠标预览值写成基准。
                        base.set_corner_radius(baseline_radius);
                        if base.tool == Tool::Rectangle {
                            base.style.rectangle_radii = baseline_corners;
                        }
                    }
                }
                if base.tool == Tool::Spotlight {
                    if let Some(layer) = layer {
                        copy_spotlight_layer_style(&mut base.style, layer);
                    }
                }
                *preview_style = base.style;
                self.preview_revision += 1;
                return;
            }
            Some(Gesture::Group {
                before,
                current,
                mode,
                point,
                start,
            }) => {
                let factor = if point.distance(*start) < 4. {
                    1.
                } else {
                    mode.scale_factor(*point)
                };
                for ((_, a), (_, base)) in current.iter_mut().zip(before) {
                    apply(a);
                    if a.tool == Tool::Spotlight {
                        if let Some(layer) = layer {
                            copy_spotlight_layer_style(&mut a.style, layer);
                        }
                    }
                    let center = crate::transform::center(base.local_bounds());
                    let old_font = base.style.font_size;
                    base.style = a.style;
                    if base.tool != Tool::Spotlight {
                        base.style.width /= factor;
                    }
                    base.style.radius /= factor;
                    base.style.effect_radius /= factor;
                    if base.tool == Tool::Rectangle {
                        base.style.rectangle_radii =
                            base.style.rectangle_radii.map(|r| r.map(|v| v / factor));
                    }
                    if base.tool.text() || base.tool == Tool::Serial {
                        base.style.font_size /= factor;
                        base.style.text.outline_width /= factor;
                        base.style.text.background_radius /= factor;
                    }
                    if base.style.font_size != old_font {
                        fit_text_bounds(base);
                    }
                    base.preserve_rotation_frame(center);
                }
                self.preview_revision += 1;
                return;
            }
            Some(_) => return,
            None => {}
        }
        if let Some(draft) = &mut self.text_draft {
            apply(&mut draft.annotation);
            self.preview_revision += 1;
            return;
        }
        let objects: Vec<_> = self
            .selected_ids()
            .iter()
            .filter_map(|id| {
                self.document.get(*id).map(|a| {
                    let mut a = a.clone();
                    apply(&mut a);
                    (*id, a)
                })
            })
            .collect();
        let objects = self.spotlight_replacements(objects, layer);
        self.document
            .replace_many_grouped(&objects, self.style_group);
    }
    pub fn set_color(&mut self, color: u32) {
        let before = self.style;
        self.style.color = color;
        let layer = self.spotlight_style_update(before, self.style);
        self.remember_spotlight_style(layer);
        self.update_selected_with_spotlight(
            |a| {
                if !a.tool.effect() && a.tool != Tool::PixelEraser {
                    a.style.color = color;
                }
            },
            layer,
        );
    }
    pub fn set_width(&mut self, width: f32) {
        let before = self.style;
        self.style.width = width;
        let layer = self.spotlight_style_update(before, self.style);
        self.remember_spotlight_style(layer);
        self.update_selected_with_spotlight(
            |a| {
                if !a.tool.effect() || a.tool.freehand() {
                    a.style.width = width;
                }
            },
            layer,
        );
    }
    /// 强度改由数值弹层的统一参数事务提交，这里只给模型测试用。
    #[cfg(test)]
    pub fn set_block_size(&mut self, size: u32) {
        self.style.block_size = size.clamp(2, 64);
        self.update_selected(|a| {
            if a.tool.effect() {
                a.style.block_size = size.clamp(2, 64);
            }
        });
    }
    pub fn has_arrow_draft(&self) -> bool {
        self.gesture.as_ref().is_some_and(|g| {
            matches!(
                g,
                Gesture::ArrowPending(_)
                    | Gesture::Draw(Annotation {
                        tool: Tool::Arrow,
                        ..
                    })
            )
        })
    }
    pub fn has_pending_drawing(&self) -> bool {
        self.has_polyline() || matches!(self.gesture, Some(Gesture::ArrowPending(_)))
    }
    pub fn has_polyline(&self) -> bool {
        matches!(self.gesture, Some(Gesture::Polyline(_)))
    }
    pub fn active_node(&self) -> Option<usize> {
        let (id, index) = self.selected_node?;
        (self.selected == Some(id)
            && self.selection.len() <= 1
            && self
                .document
                .get(id)
                .is_some_and(|a| a.tool == Tool::Polyline && index < a.points.len()))
        .then_some(index)
    }
    pub fn remove_polyline_point(&mut self) -> bool {
        if let Some(Gesture::Polyline(a)) = &mut self.gesture {
            // 最后一个点是跟随鼠标的预览；只退回最近的固定顶点。
            if a.points.len() > 2 {
                a.points.remove(a.points.len() - 2);
                self.preview_revision += 1;
            }
            return true;
        }
        let Some(index) = self.active_node() else {
            return false;
        };
        if !self.finish_gesture() {
            return false;
        }
        let id = self.selected.unwrap();
        let mut a = self.document.get(id).unwrap().clone();
        if a.points.len() <= 2 {
            return false;
        }
        let center = crate::transform::center(a.local_bounds());
        a.points.remove(index);
        if a.points.len() < 3 {
            a.style.filled = false;
            self.style.filled = false;
        }
        a.preserve_rotation_frame(center);
        self.document.replace(id, a);
        self.selected_node = None;
        true
    }
    // 无修饰键的模型便捷入口；运行接线显式携带 Shift。
    #[cfg(test)]
    pub fn insert_polyline_node(&mut self, p: (i32, i32), tolerance: f32) -> bool {
        self.insert_polyline_node_at(Point::from_tuple(p), tolerance)
    }
    #[cfg(test)]
    pub fn insert_polyline_node_at(&mut self, p: Point, tolerance: f32) -> bool {
        self.insert_polyline_node_at_with_shift(p, tolerance, false)
    }
    pub fn insert_polyline_node_at_with_shift(
        &mut self,
        p: Point,
        tolerance: f32,
        shift: bool,
    ) -> bool {
        if self.has_polyline() {
            self.pointer_down_at(p, tolerance, shift);
            self.finish_gesture();
            return true;
        }
        if self.selection.len() > 1 {
            return false;
        }
        let Some(id) = self.selected else {
            return false;
        };
        let Some(a) = self.document.get(id) else {
            return false;
        };
        if a.tool != Tool::Polyline {
            return false;
        }
        // 双击顶点只选中该点；双击线段才插入，避免一次双击产生重叠节点。
        let point = p;
        if let Some(index) = nearest_handle(a, point, tolerance) {
            self.cancel_gesture();
            self.selected_node = Some((id, index));
            return true;
        }
        let Some((index, point)) = a.node_insertion(point, tolerance) else {
            return false;
        };
        let mut a = a.clone();
        self.cancel_gesture();
        let center = crate::transform::center(a.local_bounds());
        a.points.insert(index, point);
        a.preserve_rotation_frame(center);
        self.document.replace(id, a);
        self.selected_node = Some((id, index));
        true
    }
    pub fn straighten_arrow(&mut self) -> bool {
        if self.selection.len() > 1 {
            return false;
        }
        if !self.finish_gesture() {
            return false;
        }
        let Some(id) = self.selected else {
            return false;
        };
        let Some(a) = self.document.get(id) else {
            return false;
        };
        if a.tool != Tool::Arrow || a.points.len() < 3 {
            return false;
        }
        let mut a = a.clone();
        let center = crate::transform::center(a.local_bounds());
        a.points.truncate(2);
        a.preserve_rotation_frame(center);
        self.document.replace(id, a);
        true
    }
    pub fn straighten_arrow_at(&mut self, p: (i32, i32), tolerance: f32) -> bool {
        self.straighten_arrow_point(Point::from_tuple(p), tolerance)
    }
    pub fn straighten_arrow_point(&mut self, p: Point, tolerance: f32) -> bool {
        if self.selected_annotation().is_some_and(|a| {
            a.tool == Tool::Arrow
                && a.points.len() >= 3
                && a.to_world(a.arrow_midpoint()).distance(p) <= tolerance
        }) {
            self.straighten_arrow()
        } else {
            false
        }
    }
    /// 正文按下先选中原对象；进入文字编辑由真实拖选或 Up 决定。
    pub(crate) fn select_text_core(&mut self, id: ObjectId) -> bool {
        if !self.object_in_group_scope(id) {
            return false;
        }
        if self.tool != Tool::Text || self.gesture.is_some() || self.text_draft.is_some() {
            return false;
        }
        let Some(a) = self.document.get(id).filter(|a| a.tool == Tool::Text) else {
            return false;
        };
        self.style = a.style;
        self.selected = Some(id);
        self.selected_node = None;
        self.selection.clear();
        true
    }
    pub fn begin_text(&mut self, id: Option<ObjectId>, point: Point) {
        if id.is_some_and(|id| !self.object_in_group_scope(id)) {
            return;
        }
        if self.local_fill_candidate().is_some() {
            return;
        }
        self.finish_text();
        let mut a = id
            .and_then(|id| self.document.get(id))
            .cloned()
            .unwrap_or_else(|| Annotation::new(self.tool, point, self.style));
        if a.points.len() < 2 {
            a.points.push(Point {
                x: point.x + 300.,
                y: point.y + 60.,
            });
        }
        self.text_draft = Some(TextDraft { id, annotation: a });
        self.preview_revision += 1;
    }
    pub fn edit_text(&mut self) {
        if !self.finish_gesture() {
            return;
        }
        if let Some(id) = self.selected {
            if let Some(a) = self.document.get(id) {
                if a.tool == Tool::Watermark {
                    self.select_tool(Tool::Watermark);
                    return;
                }
                if a.tool.text() {
                    self.begin_text(Some(id), a.points[0]);
                }
            }
        }
    }
    /// 整串替换；局部样式按新旧串的公共前后缀推出编辑区间。
    pub fn set_text(&mut self, text: String) {
        if let Some(d) = &mut self.text_draft {
            if let Some(rich) = &mut d.annotation.rich {
                std::sync::Arc::make_mut(rich).rebase(&d.annotation.text, &text);
            }
            d.annotation.text = text;
            self.preview_revision += 1;
        }
        self.fit_text_height();
    }
    /// 输入框已知编辑区间时使用，插入文字继承插入点的局部样式。
    pub fn replace_text(&mut self, range: std::ops::Range<usize>, inserted: &str) {
        if let Some(d) = &mut self.text_draft {
            if let Some(rich) = &mut d.annotation.rich {
                std::sync::Arc::make_mut(rich).replace(&d.annotation.text, range.clone(), inserted);
            }
            d.annotation.text.replace_range(range, inserted);
            self.preview_revision += 1;
        }
        self.fit_text_height();
    }
    fn fit_text_height(&mut self) {
        if let Some(d) = &mut self.text_draft {
            let center = crate::transform::center(d.annotation.local_bounds());
            fit_text_bounds(&mut d.annotation);
            d.annotation.preserve_rotation_frame(center);
            self.preview_revision += 1;
        }
    }
    pub fn finish_text(&mut self) {
        if self.local_fill_candidate().is_some() {
            return;
        }
        if let Some(draft) = self.text_draft.take() {
            if draft.annotation.valid() {
                if let Some(id) = draft.id {
                    self.document.replace(id, draft.annotation);
                    self.selected = Some(id);
                } else {
                    self.selected = Some(self.insert_annotation_in_group_scope(draft.annotation));
                }
            } else if draft.annotation.tool == Tool::Text && draft.annotation.text.trim().is_empty()
            {
                // 普通文字结束空白编辑等同删除；水印保留旧语义，待独立副栏 VM 迁移。
                if let Some(id) = draft.id {
                    self.document.remove(id);
                }
                self.selected = None;
                self.selected_node = None;
                self.selection.clear();
            }
            self.preview_revision += 1;
            self.repair_group_scope();
        }
    }
    pub fn cancel_text(&mut self) -> bool {
        let had = self.text_draft.take().is_some();
        if had {
            self.preview_revision += 1;
        }
        had
    }
    /// 录制基本工具属性：只接收三字段意图，不能串改圆角/字体等其它Style字段。
    /// 目标统一走原批量事务；set_style仅同步默认值，同值replace不另增history。
    pub(crate) fn set_basic_style_fields(
        &mut self,
        values: Style,
        fields: (bool, bool, bool),
    ) -> bool {
        let mut next = self.style;
        if fields.0 {
            next.color = values.color;
        }
        if fields.1 {
            next.width = values.width;
        }
        if fields.2 {
            next.opacity = values.opacity;
        }
        let changed = |a: &Annotation| {
            (fields.0 && a.style.color != next.color)
                || (fields.1 && a.style.width != next.width)
                || (fields.2 && a.style.opacity != next.opacity)
        };
        let draft_changed = match &self.gesture {
            Some(
                Gesture::Draw(a)
                | Gesture::PenLine(a)
                | Gesture::Polyline(a)
                | Gesture::ArrowPending(a),
            ) => changed(a),
            _ => self
                .text_draft
                .as_ref()
                .is_some_and(|d| changed(&d.annotation)),
        };
        let targets_changed =
            draft_changed || self.selected_objects().iter().any(|(_, a)| changed(a));
        if next == self.style && !targets_changed {
            return false;
        }
        if targets_changed {
            self.update_selected(|a| {
                if fields.0 {
                    a.style.color = next.color;
                }
                if fields.1 {
                    a.style.width = next.width;
                }
                if fields.2 {
                    a.style.opacity = next.opacity;
                }
            });
        }
        self.set_style(next);
        true
    }
    pub fn set_style(&mut self, style: Style) {
        if self.tool == Tool::SmartMosaic {
            // Smart 的文档参数必须由宿主先准备 Scene 再提交；这里仅同步下一次上下文。
            self.smart_effect.1 = style;
            self.style = style;
            return;
        }
        let selected_watermark = self.selected.filter(|id| {
            self.document
                .get(*id)
                .is_some_and(|a| a.tool == Tool::Watermark)
                && self.selected_ids().len() == 1
        });
        if (self.tool == Tool::Watermark || selected_watermark.is_some())
            && self.text_draft.is_none()
            && self.gesture.is_none()
        {
            if selected_watermark.is_some() && selected_watermark != self.watermark.target() {
                self.activate_watermark(selected_watermark);
            }
            self.set_watermark_style(style);
            return;
        }
        let before = self.style;
        if before.highlighter_blend != style.highlighter_blend {
            let mut tools: Vec<_> = match &self.gesture {
                Some(Gesture::Draw(a))
                    if matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect) =>
                {
                    vec![a.tool]
                }
                _ => self
                    .selected_objects()
                    .into_iter()
                    .filter_map(|(_, a)| {
                        matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
                            .then_some(a.tool)
                    })
                    .collect(),
            };
            if tools.is_empty() && matches!(self.tool, Tool::Highlighter | Tool::HighlighterRect) {
                tools.push(self.tool);
            }
            for tool in tools {
                let preset = self
                    .tool_styles
                    .entry(tool)
                    .or_insert_with(|| crate::tool_presets::default_style(tool));
                preset.highlighter_blend = style.highlighter_blend;
                preset.opacity = style.opacity;
            }
        }
        let radius_tool = self.selected_annotation().map_or(self.tool, |a| a.tool);
        if before.radius_for(radius_tool) != style.radius_for(radius_tool) {
            self.remember_single_radius(style.radius_for(radius_tool));
        }
        let layer = self.spotlight_style_update(before, style);
        self.remember_spotlight_style(layer);
        self.style = style;
        self.update_selected_with_spotlight(
            |a| {
                if before.color != style.color {
                    a.style.color = style.color;
                }
                if before.width != style.width {
                    a.style.width = style.width;
                }
                if before.opacity != style.opacity {
                    a.style.opacity = style.opacity;
                }
                if before.highlighter_blend != style.highlighter_blend
                    && matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
                {
                    a.style.highlighter_blend = style.highlighter_blend;
                }
                if before.filled != style.filled {
                    a.style.filled = style.filled;
                }
                if a.supports_radius() && before.radius_for(a.tool) != style.radius_for(a.tool) {
                    a.set_corner_radius(style.radius_for(a.tool));
                }
                if a.tool == Tool::Rectangle && before.rectangle_radii != style.rectangle_radii {
                    a.style.rectangle_radii = style.rectangle_radii;
                }
                if before.font_size != style.font_size {
                    a.style.font_size = style.font_size;
                }
                if before.bold != style.bold {
                    a.style.bold = style.bold;
                }
                if before.text != style.text && (a.tool.text() || a.tool == Tool::Serial) {
                    a.style.text = a.style.text.updated(before.text, style.text);
                }
                a.clear_rich_overrides(before, style);
                if before.dashed != style.dashed {
                    a.style.dashed = style.dashed;
                }
                if before.pencil_smooth != style.pencil_smooth && a.tool == Tool::Pen {
                    a.style.pencil_smooth = style.pencil_smooth;
                }
                if before.polyline_smooth != style.polyline_smooth && a.tool == Tool::Polyline {
                    a.style.polyline_smooth = style.polyline_smooth;
                }
                if before.arrow_head != style.arrow_head && a.tool == Tool::Arrow {
                    a.style.arrow_head = style.arrow_head;
                }
                if before.arrow_start != style.arrow_start && a.tool == Tool::Arrow {
                    a.style.arrow_start = style.arrow_start;
                }
                if before.arrow_end != style.arrow_end && a.tool == Tool::Arrow {
                    a.style.arrow_end = style.arrow_end;
                }
                if before.block_size != style.block_size {
                    a.style.block_size = style.block_size;
                }
                if before.font_size != style.font_size
                    || before.bold != style.bold
                    || before.text.font != style.text.font
                    || before.text.italic != style.text.italic
                    || before.text.background != style.text.background
                {
                    fit_text_bounds(a);
                }
            },
            layer,
        );
        self.fit_text_height();
    }
    pub fn select_all_objects(&mut self) {
        if !self.finish_gesture() {
            return;
        }
        self.enter_selection_mode();
        let scope = self.group_scope_path();
        self.selection = self
            .document
            .items()
            .iter()
            .filter(|o| {
                !matches!(o.annotation.tool, Tool::LocalFill | Tool::Watermark)
                    && o.in_group_scope(&scope)
            })
            .map(|o| o.id)
            .collect();
        self.selected = self.selection.last().copied();
        self.selected_node = None;
        self.sync_primary_selection_style();
    }
    pub fn toggle_selection(&mut self, p: (i32, i32), tolerance: f32) {
        self.toggle_selection_at(Point::from_tuple(p), tolerance);
    }
    pub fn toggle_selection_at(&mut self, p: Point, tolerance: f32) {
        if !self.finish_gesture() {
            return;
        }
        let point = p;
        let scope = self.group_scope_path();
        if let Some(id) = self
            .document
            .items()
            .iter()
            .rev()
            .find(|o| o.in_group_scope(&scope) && o.annotation.hit(point, tolerance, true))
            .map(|o| o.id)
        {
            let mut ids = self.selected_ids();
            let group = self.expand_selection(&[id]);
            if ids.contains(&id) {
                ids.retain(|other| !group.contains(other));
            } else {
                ids.extend(group);
            }
            self.selected = ids.last().copied();
            self.selection = ids;
            self.enter_selection_mode();
            self.sync_primary_selection_style();
        }
    }
    pub fn duplicate_selected(&mut self) {
        if !self.finish_gesture() {
            return;
        }
        self.finish_text();
        let ids = self.selected_ids();
        let scope = self.group_scope_path();
        self.selection = if scope.is_empty() {
            self.document.duplicate(&ids, self.canvas)
        } else {
            self.document
                .duplicate_in_group_scope(&ids, self.canvas, &scope)
        };
        self.selected = self.selection.last().copied();
        self.sync_primary_selection_style();
    }
    pub fn reorder_selected(&mut self, front: bool) {
        if !self.finish_gesture() {
            return;
        }
        let ids = self.selected_ids();
        self.document.reorder(&ids, front);
    }
    pub fn nudge(&mut self, dx: f32, dy: f32) {
        if self
            .selected_ids()
            .iter()
            .all(|id| self.document.get(*id).is_none())
        {
            return;
        }
        self.update_selected(|a| a.translate(dx, dy));
    }
}

fn copy_spotlight_layer_style(target: &mut Style, layer: Style) {
    target.color = layer.color;
    target.width = layer.width;
    target.opacity = layer.opacity;
}
fn fit_text_bounds(a: &mut Annotation) {
    if a.tool != Tool::Text || a.points.len() < 2 {
        return;
    }
    let height = crate::text_layout::TextLayout::new(a)
        .map_or(a.style.font_size * 1.5, |layout| layout.height);
    a.points[1].y = a.points[0].y + height;
}
fn nearest_handle(a: &Annotation, p: Point, tolerance: f32) -> Option<usize> {
    a.handles()
        .iter()
        .enumerate()
        .map(|(i, h)| (i, h.distance(p)))
        .filter(|(_, d)| *d <= tolerance)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}
fn pen_line_point(anchor: Point, p: Point) -> Point {
    constrained_line_point(anchor, p, std::f32::consts::PI / 8.)
}
fn constrained_line_point(anchor: Point, p: Point, step: f32) -> Point {
    let (dx, dy) = (p.x - anchor.x, p.y - anchor.y);
    let angle = (dy.atan2(dx) / step).round() * step;
    let length = dx.hypot(dy);
    Point {
        x: anchor.x + angle.cos() * length,
        y: anchor.y + angle.sin() * length,
    }
}
fn snap_angle(anchor: Point, p: Point) -> Point {
    let (dx, dy) = (p.x - anchor.x, p.y - anchor.y);
    let angle = (dy.atan2(dx) / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
    let length = dx.hypot(dy);
    Point {
        x: anchor.x + length * angle.cos(),
        y: anchor.y + length * angle.sin(),
    }
}
fn aspect_point(anchor: Point, p: Point, ratio: f32, canvas: Option<Rect>) -> Point {
    let (dx, dy) = (p.x - anchor.x, p.y - anchor.y);
    let sx = if dx < 0. { -1. } else { 1. };
    let sy = if dy < 0. { -1. } else { 1. };
    let mut width = dx.abs().max(dy.abs() * ratio);
    // 选区仍停在画布边上；标注传入 None，比例吸附可以伸出画布。
    if let Some(canvas) = canvas {
        let max_x = if sx < 0. {
            anchor.x
        } else {
            canvas.width as f32 - anchor.x
        };
        let max_y = if sy < 0. {
            anchor.y
        } else {
            canvas.height as f32 - anchor.y
        };
        width = width.min(max_x).min(max_y * ratio);
    }
    Point {
        x: anchor.x + sx * width,
        y: anchor.y + sy * width / ratio,
    }
}
fn crop_resize(r: Rect, edges: u8, p: (i32, i32), canvas: Rect) -> Rect {
    // 与原 Resize 分支相同；内部偏移与既有绝对点路线共用规范化和画布夹紧。
    Rect::drag(
        (
            if edges & 1 != 0 { p.0 } else { r.x as i32 },
            if edges & 4 != 0 { p.1 } else { r.y as i32 },
        ),
        (
            if edges & 2 != 0 {
                p.0
            } else {
                (r.x + r.width) as i32
            },
            if edges & 8 != 0 {
                p.1
            } else {
                (r.y + r.height) as i32
            },
        ),
        canvas.width,
        canvas.height,
    )
}
fn crop_resize_inside_point(r: Rect, edges: u8, start: (i32, i32), p: (i32, i32)) -> (i32, i32) {
    let x = if edges & 1 != 0 { r.x } else { r.x + r.width };
    let y = if edges & 4 != 0 { r.y } else { r.y + r.height };
    let delta = |edge: u32, from: i32, to: i32| {
        (i64::from(edge) + i64::from(to) - i64::from(from))
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    (delta(x, start.0, p.0), delta(y, start.1, p.1))
}
fn crop_edges(r: Rect, p: (i32, i32), t: f32) -> u8 {
    let (x, y, right, bottom) = (
        r.x as i32,
        r.y as i32,
        (r.x + r.width) as i32,
        (r.y + r.height) as i32,
    );
    let t = t.ceil() as i32;
    if p.0 < x - t || p.0 > right + t || p.1 < y - t || p.1 > bottom + t {
        return 0;
    }
    // 控制点已外置：外侧单边格不应因小选区的容差重叠变成角点。
    // 精确边界点仍沿用下面的近边命中；外侧采用内容的右/下开区间。
    if p.0 < x || p.0 > right || p.1 < y || p.1 > bottom {
        let horizontal = if p.0 < x {
            1
        } else if p.0 >= right {
            2
        } else {
            0
        };
        let vertical = if p.1 < y {
            4
        } else if p.1 >= bottom {
            8
        } else {
            0
        };
        return horizontal | vertical;
    }
    let mut edges = 0;
    // 小选区两侧命中容差可能重叠，按最近的边选择，避免右/下控制点被左/上抢走。
    if (p.0 - x).abs() <= t && (p.0 - x).abs() <= (p.0 - right).abs() {
        edges |= 1;
    } else if (p.0 - right).abs() <= t {
        edges |= 2;
    }
    if (p.1 - y).abs() <= t && (p.1 - y).abs() <= (p.1 - bottom).abs() {
        edges |= 4;
    } else if (p.1 - bottom).abs() <= t {
        edges |= 8;
    }
    edges
}
/// 外部八格固定被拖动的边；随后仍可收缩或穿过原对边。
fn crop_outside_edges(r: Rect, p: (i32, i32)) -> u8 {
    let horizontal = if p.0 <= r.x as i32 {
        1
    } else if p.0 >= (r.x + r.width) as i32 {
        2
    } else {
        0
    };
    let vertical = if p.1 <= r.y as i32 {
        4
    } else if p.1 >= (r.y + r.height) as i32 {
        8
    } else {
        0
    };
    horizontal | vertical
}

#[cfg(test)]
mod screenshot_lasso_outside_tests {
    use super::*;
    use crate::crop_mask::Polygon;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }
    fn mask() -> Polygon {
        Polygon::new(&[
            Point { x: 110., y: 90. },
            Point { x: 190., y: 90. },
            Point { x: 190., y: 160. },
            Point { x: 110., y: 160. },
        ])
        .unwrap()
    }
    fn editor() -> Editor {
        let mut editor = Editor::new(Rect::full(400, 300), Some(rect(100, 80, 120, 100)));
        editor.select_tool(Tool::Lasso);
        editor
    }
    fn object(x: f32) -> Annotation {
        let mut object = Annotation::new(Tool::Rectangle, Point { x, y: 30. }, Style::default());
        object.update(Point { x: x + 20., y: 50. });
        object
    }

    #[test]
    fn eight_outside_zones_and_near_handles_keep_original_resize_and_shift_aspect() {
        for (down, near, edges, up, expected) in [
            ((60, 40), (99, 79), 5, (80, 60), rect(80, 60, 140, 120)),
            ((160, 40), (160, 79), 4, (160, 60), rect(100, 60, 120, 120)),
            ((260, 40), (221, 79), 6, (240, 60), rect(100, 60, 140, 120)),
            ((60, 130), (99, 130), 1, (80, 130), rect(80, 80, 140, 100)),
            (
                (260, 130),
                (221, 130),
                2,
                (240, 130),
                rect(100, 80, 140, 100),
            ),
            ((60, 220), (99, 181), 9, (80, 200), rect(80, 80, 140, 120)),
            (
                (160, 220),
                (160, 181),
                8,
                (160, 200),
                rect(100, 80, 120, 120),
            ),
            (
                (260, 220),
                (221, 181),
                10,
                (240, 200),
                rect(100, 80, 140, 120),
            ),
        ] {
            for down_point in [down, near] {
                for shift in [false, true] {
                    let mut editor = editor();
                    let draft = mask();
                    editor.lasso_draft = Some(draft.clone());
                    let style = editor.style;
                    assert_eq!(editor.screenshot_crop_outside_edges(down, 6.), Some(edges));
                    assert_eq!(editor.screenshot_crop_outside_edges(near, 6.), Some(edges));
                    assert!(editor.begin_screenshot_crop_outside(down_point, 6.));
                    assert!(editor.is_lasso_crop_outside_resize());
                    assert_eq!(editor.crop_creation_before(), None);
                    let final_point = editor.constrained_pointer(up, shift);
                    editor.pointer_move(final_point);
                    assert!(matches!(&editor.gesture, Some(Gesture::Crop {
                    start, mode: CropGesture::Resize(original, actual_edges), ..
                }) if *start == down_point && *original == rect(100, 80, 120, 100) && *actual_edges == edges));
                    assert_eq!(editor.lasso_draft, Some(draft));
                    let actual = editor.crop.unwrap();
                    if shift && [5, 6, 9, 10].contains(&edges) {
                        assert!((actual.width as f32 - actual.height as f32 * 1.2).abs() <= 1.2);
                        assert_eq!(
                            if edges & 1 != 0 {
                                actual.x + actual.width
                            } else {
                                actual.x
                            },
                            if edges & 1 != 0 { 220 } else { 100 }
                        );
                        assert_eq!(
                            if edges & 4 != 0 {
                                actual.y + actual.height
                            } else {
                                actual.y
                            },
                            if edges & 4 != 0 { 180 } else { 80 }
                        );
                    } else {
                        assert_eq!(actual, expected);
                    }
                    editor.pointer_up(final_point);
                    assert_eq!(editor.crop, Some(actual));
                    assert_eq!(editor.tool, Tool::Lasso);
                    assert_eq!(editor.style, style);
                    assert!(!editor.is_lasso_crop_outside_resize());
                    assert!(editor.lasso_draft.is_none());
                    assert!(editor.document.items().is_empty());
                    assert!(!editor.document.can_undo());
                }
            }
        }
    }

    #[test]
    fn closed_draft_mask_cancel_return_and_changed_up_preserve_only_object_history() {
        let mut editor = editor();
        let first = object(20.);
        let second = object(60.);
        let later = object(250.);
        let first_id = editor.document.insert(first.clone());
        let second_id = editor.document.insert(second.clone());
        let applied = mask();
        editor.document.set_crop_mask(Some(applied.clone()));
        let later_id = editor.document.insert(later.clone());
        editor.document.undo();
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let crop = editor.crop;
        let draft = Polygon::new(&[
            Point { x: 120., y: 100. },
            Point { x: 180., y: 100. },
            Point { x: 180., y: 150. },
        ])
        .unwrap();
        editor.lasso_draft = Some(draft.clone());
        assert!(editor.document.can_undo() && editor.document.can_redo());
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        editor.pointer_move((40, 130));
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.lasso_draft, Some(draft.clone()));
        assert_eq!(editor.document.crop_mask, Some(applied.clone()));
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        assert!(editor.document.can_undo() && editor.document.can_redo());
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        editor.pointer_move((40, 130));
        editor.pointer_up((100, 130));
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.lasso_draft, Some(draft));
        assert_eq!(editor.document.crop_mask, Some(applied));
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        editor.pointer_up((80, 130));
        assert_eq!(editor.crop, Some(rect(80, 80, 140, 100)));
        assert!(editor.lasso_draft.is_none() && editor.document.crop_mask.is_none());
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision + 1);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.redo();
        assert_eq!(editor.document.get(later_id), Some(&later));
        editor.undo();
        assert_eq!(editor.document.items(), items);
        editor.undo();
        assert!(editor.document.get(second_id).is_none());
        assert_eq!(editor.document.get(first_id), Some(&first));
        editor.redo();
        assert_eq!(editor.document.items(), items);
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.crop, Some(rect(80, 80, 140, 100)));
    }

    #[test]
    fn active_crop_cannot_apply_draft_and_explicit_discard_paths_keep_their_contract() {
        let mut editor = editor();
        let draft = mask();
        editor.lasso_draft = Some(draft.clone());
        let crop = editor.crop;
        let revision = editor.document.revision();
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        editor.pointer_move((80, 130));
        assert!(!editor.apply_lasso());
        assert_eq!(editor.lasso_draft, Some(draft.clone()));
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.lasso_draft, Some(draft.clone()));
        assert!(editor.apply_lasso());
        assert_eq!(editor.document.crop_mask, Some(draft.clone()));
        assert!(editor.lasso_draft.is_none());
        assert!(!editor.apply_lasso());
        editor.lasso_draft = Some(draft.clone());
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        editor.pointer_move((80, 130));
        editor.select_tool(Tool::Edit);
        assert_eq!(editor.crop, crop);
        assert!(editor.lasso_draft.is_none());
        assert_eq!(editor.document.crop_mask, Some(draft.clone()));
        editor.select_tool(Tool::Lasso);
        editor.lasso_draft = Some(draft.clone());
        assert!(editor.cancel_gesture()); // 无 Crop 的第一次取消仍丢闭合稿。
        assert!(editor.lasso_draft.is_none());
        editor.pointer_down((120, 100), 6.);
        editor.pointer_move((170, 140));
        assert!(editor.drawing_lasso());
        assert!(editor.cancel_gesture());
        assert!(!editor.drawing_lasso());
        assert_eq!(editor.document.crop_mask, Some(draft.clone()));
        editor.lasso_draft = Some(draft.clone());
        assert!(editor.begin_screenshot_crop_outside((60, 130), 6.));
        editor.pointer_move((80, 130));
        editor.clear();
        assert_eq!(editor.crop, crop);
        assert!(editor.lasso_draft.is_none() && editor.document.crop_mask.is_none());
        editor.undo();
        assert_eq!(editor.document.crop_mask, Some(draft));

        // 旧 outside New 会先清稿；本轮新增的 Crop＋闭合稿状态保留独立操作合同。
        let prepare = || {
            let mut pending = self::editor();
            pending.document.set_crop_mask(Some(mask()));
            let first = object(20.);
            let first_id = pending.document.insert(first.clone());
            let later = object(250.);
            let later_id = pending.document.insert(later.clone());
            pending.document.undo();
            pending.lasso_draft = Some(mask());
            assert!(pending.document.can_undo() && pending.document.can_redo());
            assert!(pending.begin_screenshot_crop_outside((60, 130), 6.));
            pending.pointer_move((80, 130));
            assert!(pending.is_lasso_crop_outside_resize());
            (pending, first_id, later_id, first, later)
        };
        let (mut pending, first_id, later_id, first, later) = prepare();
        let old_draft = pending.lasso_draft.clone();
        let old_mask = pending.document.crop_mask.clone();
        let items = pending.document.items().to_vec();
        let revision = pending.document.revision();
        let edit_revision = pending.document.edit_revision();
        assert!(pending.selected_ids().is_empty());
        pending.delete_selected();
        assert_eq!(pending.crop, crop);
        assert!(!pending.is_changing_crop());
        assert_eq!(pending.lasso_draft, old_draft);
        assert_eq!(pending.document.crop_mask, old_mask);
        assert_eq!(pending.document.items(), items);
        assert_eq!(pending.document.get(first_id), Some(&first));
        assert!(pending.document.get(later_id).is_none());
        assert_eq!(pending.document.revision(), revision);
        assert_eq!(pending.document.edit_revision(), edit_revision);
        assert!(pending.document.can_undo() && pending.document.can_redo());
        pending.document.redo();
        assert_eq!(pending.document.get(later_id), Some(&later));
        pending.document.undo();
        assert_eq!(pending.document.items(), items);

        let (mut pending, first_id, later_id, first, _) = prepare();
        let old_draft = pending.lasso_draft.clone();
        let old_mask = pending.document.crop_mask.clone();
        let items = pending.document.items().to_vec();
        let revision = pending.document.revision();
        let edit_revision = pending.document.edit_revision();
        pending.undo();
        assert_eq!(pending.crop, crop);
        assert!(!pending.is_changing_crop());
        assert_eq!(pending.lasso_draft, old_draft);
        assert_eq!(pending.document.crop_mask, old_mask);
        assert_eq!(pending.document.items(), items);
        assert_eq!(pending.document.revision(), revision);
        assert_eq!(pending.document.edit_revision(), edit_revision);
        pending.undo();
        assert!(pending.lasso_draft.is_none());
        assert_eq!(pending.crop, crop);
        assert_eq!(pending.document.crop_mask, old_mask);
        assert_eq!(pending.document.items(), items);
        assert_eq!(pending.document.get(first_id), Some(&first));
        assert!(pending.document.get(later_id).is_none());
        assert_eq!(pending.document.revision(), revision);
        assert_eq!(pending.document.edit_revision(), edit_revision);
        assert!(pending.document.can_undo() && pending.document.can_redo());
        pending.undo(); // 第三次才进入原对象历史。
        assert!(pending.document.get(first_id).is_none());
        assert_eq!(pending.document.crop_mask, old_mask);
        assert_eq!(pending.document.revision(), revision + 1);
        assert_eq!(pending.document.edit_revision(), edit_revision);

        let (mut pending, first_id, later_id, first, later) = prepare();
        let old_draft = pending.lasso_draft.clone();
        let old_mask = pending.document.crop_mask.clone();
        let revision = pending.document.revision();
        let edit_revision = pending.document.edit_revision();
        pending.redo();
        assert_eq!(pending.crop, crop);
        assert!(!pending.is_changing_crop());
        assert_eq!(pending.lasso_draft, old_draft);
        assert_eq!(pending.document.crop_mask, old_mask);
        assert_eq!(pending.document.get(first_id), Some(&first));
        assert_eq!(pending.document.get(later_id), Some(&later));
        assert_eq!(pending.document.items().len(), 2);
        assert_eq!(pending.document.revision(), revision + 1);
        assert_eq!(pending.document.edit_revision(), edit_revision);
        assert!(pending.document.can_undo());
        assert!(!pending.document.can_redo());
    }

    #[test]
    fn inside_no_crop_shared_default_and_input_boundaries_do_not_become_outside_resize() {
        let mut lasso = editor();
        for point in [(100, 80), (160, 130), (-1, 130), (400, 130)] {
            assert_eq!(lasso.screenshot_crop_outside_edges(point, 6.), None);
        }
        lasso.pointer_down((110, 90), 6.);
        lasso.pointer_move((20, 280));
        assert!(lasso.drawing_lasso());
        assert!(!lasso.is_lasso_crop_outside_resize());
        assert_eq!(lasso.screenshot_crop_outside_edges((60, 130), 6.), None);
        assert_eq!(
            lasso.lasso_points().unwrap().last(),
            Some(&Point { x: 100., y: 180. })
        );
        assert!(lasso.cancel_gesture());
        lasso.crop = None;
        assert!(!lasso.begin_screenshot_crop_outside((60, 130), 6.));
        lasso.pointer_down((60, 130), 6.);
        assert_eq!(lasso.crop_creation_before(), Some(None));
        assert!(!lasso.is_lasso_crop_outside_resize());
        for crop in [None, Some(rect(100, 80, 120, 100))] {
            // 图片／Pin 沿通用 Editor Down；宿主专用 helper 的新增行为不泄漏进去。
            let mut shared = Editor::new(Rect::full(400, 300), crop);
            shared.select_tool(Tool::Lasso);
            shared.pointer_down((60, 130), 6.);
            assert_eq!(shared.crop_creation_before(), Some(crop));
            assert!(!shared.is_lasso_crop_outside_resize());
        }
        for tool in [Tool::Rectangle, Tool::Mosaic, Tool::Blur, Tool::LocalFill] {
            let mut drawing = editor();
            drawing.select_tool(tool);
            assert!(!drawing.begin_screenshot_crop_outside((60, 130), 6.));
        }
        let mut selected = editor();
        let id = selected.document.insert(object(20.));
        selected.selected = Some(id);
        assert!(!selected.begin_screenshot_crop_outside((60, 130), 6.));
        selected.selected = None;
        selected.text_draft = Some(TextDraft {
            id: None,
            annotation: Annotation::new(Tool::Text, Point { x: 30., y: 30. }, Style::default()),
        });
        assert!(!selected.begin_screenshot_crop_outside((60, 130), 6.));
        selected.text_draft = None;
        selected.select_tool(Tool::Edit);
        selected.lasso_draft = Some(mask());
        assert!(!selected.begin_screenshot_crop_outside((60, 130), 6.));
    }
}

#[cfg(test)]
mod screenshot_empty_center_tests {
    use super::*;
    use crate::crop_mask::Polygon;

    fn editor() -> Editor {
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        editor.select_tool(Tool::Edit);
        editor
    }

    fn mask() -> Polygon {
        Polygon::new(&[
            Point { x: 100., y: 80. },
            Point { x: 220., y: 80. },
            Point { x: 220., y: 180. },
            Point { x: 100., y: 180. },
        ])
        .unwrap()
    }

    #[test]
    fn empty_edit_center_reuses_move_clamp_and_keeps_tool_style_and_object_history() {
        let mut editor = editor();
        let style = editor.style;
        let edit_revision = editor.document.edit_revision();
        assert!(editor.screenshot_empty_crop_center_ready((160, 130), 4.));
        assert!(editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        assert!(editor.is_changing_crop());
        assert_eq!(editor.crop_creation_before(), None);
        assert_eq!(editor.hover((160, 130), 4.), Hover::Move);
        editor.pointer_move((-200, -500));
        assert_eq!(
            editor.crop.unwrap(),
            Rect {
                x: 0,
                y: 0,
                width: 120,
                height: 100
            }
        );
        editor.pointer_move((900, 600));
        assert_eq!(
            editor.crop.unwrap(),
            Rect {
                x: 280,
                y: 200,
                width: 120,
                height: 100
            }
        );
        editor.pointer_up((200, 160));
        assert_eq!(
            editor.crop.unwrap(),
            Rect {
                x: 140,
                y: 110,
                width: 120,
                height: 100
            }
        );
        assert!(!editor.is_dragging());
        assert_eq!(editor.tool, Tool::Edit);
        assert_eq!(editor.style, style);
        assert!(editor.document.items().is_empty());
        assert_eq!(editor.document.edit_revision(), edit_revision);
        assert!(!editor.document.can_undo());
        assert!(!editor.document.can_redo());
    }

    #[test]
    fn center_cancel_and_return_preserve_mask_until_a_changed_move_finishes() {
        let mut editor = editor();
        editor.document.set_crop_mask(Some(mask()));
        let before = editor.crop;
        let before_mask = editor.document.crop_mask.clone();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        assert!(editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        editor.pointer_move((200, 160));
        assert_ne!(editor.crop, before);
        assert_eq!(editor.document.crop_mask, before_mask);
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, before);
        assert_eq!(editor.document.crop_mask, before_mask);
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.document.can_undo());

        assert!(editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        editor.pointer_move((200, 160));
        editor.pointer_up((160, 130));
        assert_eq!(editor.crop, before);
        assert_eq!(editor.document.crop_mask, before_mask);
        assert_eq!(editor.document.revision(), revision);

        assert!(editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        editor.pointer_up((200, 160));
        assert_ne!(editor.crop, before);
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document.revision(), revision + 1);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        assert!(!editor.document.can_undo());
        assert!(!editor.document.can_redo());
    }

    #[test]
    fn nonempty_document_retains_marquee_and_object_move_with_original_undo() {
        let mut editor = editor();
        let crop = editor.crop;
        let mut object = Annotation::new(
            Tool::Rectangle,
            Point { x: 120., y: 100. },
            Style::default(),
        );
        object.update(Point { x: 160., y: 140. });
        let id = editor.document.insert(object.clone());
        assert!(!editor.begin_screenshot_empty_crop_center((190, 150), 4.));
        editor.pointer_down((190, 150), 4.);
        assert!(!editor.is_changing_crop());
        editor.pointer_up((205, 165));
        assert!(editor.selected_ids().is_empty());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.get(id), Some(&object));

        assert!(!editor.begin_screenshot_empty_crop_center((140, 120), 2.));
        editor.pointer_down((140, 120), 2.);
        assert!(!editor.is_changing_crop());
        assert_eq!(editor.selected, Some(id));
        editor.pointer_up((150, 130));
        assert_ne!(editor.document.get(id), Some(&object));
        assert_eq!(editor.crop, crop);
        editor.undo();
        assert_eq!(editor.document.get(id), Some(&object));
        assert_eq!(editor.crop, crop);
    }

    #[test]
    fn center_qualification_excludes_handles_invalid_crop_drafts_and_shared_default_input() {
        let mut editor = editor();
        for point in [
            (100, 130),
            (103, 130),
            (217, 130),
            (160, 80),
            (160, 177),
            (50, 130),
            (-1, 130),
        ] {
            assert!(
                !editor.screenshot_empty_crop_center_ready(point, 4.),
                "{point:?}"
            );
            assert!(
                !editor.begin_screenshot_empty_crop_center(point, 4.),
                "{point:?}"
            );
            assert!(!editor.is_dragging());
        }
        let crop = editor.crop;
        for invalid in [
            None,
            Some(Rect::full(0, 10)),
            Some(Rect {
                x: u32::MAX,
                y: 80,
                width: 120,
                height: 100,
            }),
        ] {
            editor.crop = invalid;
            assert!(!editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        }
        editor.crop = crop;
        editor.lasso_draft = Some(mask());
        assert!(!editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        assert!(editor.lasso_draft.is_some());
        editor.lasso_draft = None;
        editor.text_draft = Some(TextDraft {
            id: None,
            annotation: Annotation::new(Tool::Text, Point { x: 140., y: 110. }, Style::default()),
        });
        assert!(!editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        assert!(editor.text_draft.is_some());
        editor.text_draft = None;
        assert!(editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        assert!(!editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        editor.cancel_gesture();
        editor.select_tool(Tool::Crop);
        assert!(!editor.begin_screenshot_empty_crop_center((160, 130), 4.));
        editor.pointer_down((160, 130), 4.);
        assert!(editor.is_changing_crop());
        editor.cancel_gesture();
        editor.select_tool(Tool::Edit);
        // 共享图片/贴图入口仍做原空对象框选，只有截图宿主显式选择新入口。
        editor.pointer_down((160, 130), 4.);
        assert!(!editor.is_changing_crop());
        editor.pointer_up((200, 160));
        assert_eq!(editor.crop, crop);
    }
}
#[cfg(test)]
mod screenshot_empty_inside_edge_tests {
    use super::*;
    use crate::crop_mask::Polygon;

    fn editor() -> Editor {
        let mut editor = Editor::new(
            Rect::full(300, 200),
            Some(Rect {
                x: 30,
                y: 20,
                width: 100,
                height: 80,
            }),
        );
        editor.select_tool(Tool::Edit);
        editor
    }
    fn mask() -> Polygon {
        Polygon::new(&[
            Point { x: 30., y: 20. },
            Point { x: 130., y: 20. },
            Point { x: 130., y: 100. },
            Point { x: 30., y: 100. },
        ])
        .unwrap()
    }
    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn eight_inside_edges_use_pointer_delta_and_preserve_edit_style_and_history() {
        for (point, edges, expected) in [
            ((31, 60), 1, rect(40, 20, 90, 80)),
            ((129, 60), 2, rect(30, 20, 110, 80)),
            ((80, 21), 4, rect(30, 30, 100, 70)),
            ((80, 99), 8, rect(30, 20, 100, 90)),
            ((31, 21), 5, rect(40, 30, 90, 70)),
            ((129, 21), 6, rect(30, 30, 110, 70)),
            ((31, 99), 9, rect(40, 20, 90, 90)),
            ((129, 99), 10, rect(30, 20, 110, 90)),
            ((30, 60), 1, rect(40, 20, 90, 80)),
            ((80, 20), 4, rect(30, 30, 100, 70)),
        ] {
            let mut editor = editor();
            let style = editor.style;
            let revision = editor.document.revision();
            let edit_revision = editor.document.edit_revision();
            assert_eq!(
                editor.screenshot_empty_crop_inside_edges(point, 6.),
                Some(edges)
            );
            assert!(editor.begin_screenshot_empty_crop_inside(point, 6.));
            assert_eq!(editor.hover(point, 6.), Hover::edges(edges));
            assert!(editor.is_changing_crop());
            assert_eq!(editor.crop_creation_before(), None);
            editor.pointer_up((point.0 + 10, point.1 + 10));
            assert_eq!(editor.crop, Some(expected));
            assert_eq!(editor.tool, Tool::Edit);
            assert_eq!(editor.style, style);
            assert!(editor.document.items().is_empty());
            assert_eq!(editor.document.revision(), revision + 1);
            assert_eq!(editor.document.edit_revision(), edit_revision);
            assert!(!editor.document.can_undo());
            assert!(!editor.document.can_redo());
            assert!(!editor.is_dragging());
        }
    }

    #[test]
    fn inside_edge_reverse_clamp_and_physical_tolerance_keep_existing_outside_contract() {
        for tolerance in [6., 9., 12.] {
            let mut editor = editor();
            assert_eq!(
                editor.screenshot_empty_crop_inside_edges((31, 21), tolerance),
                Some(5)
            );
            assert!(editor.begin_screenshot_empty_crop_inside((31, 21), tolerance));
            editor.pointer_move((201, 151));
            assert_eq!(editor.crop, Some(rect(130, 100, 70, 50)));
            editor.pointer_move((-900, -800));
            assert_eq!(editor.crop, Some(rect(0, 0, 130, 100)));
            editor.pointer_move((900, 800));
            assert_eq!(editor.crop, Some(rect(130, 100, 170, 100)));
            editor.cancel_gesture();
        }
        let mut tiny = Editor::new(Rect::full(10, 10), Some(rect(3, 4, 1, 1)));
        tiny.select_tool(Tool::Edit);
        assert_eq!(tiny.screenshot_empty_crop_inside_edges((3, 4), 6.), Some(5));
        assert!(tiny.begin_screenshot_empty_crop_inside((3, 4), 6.));
        tiny.pointer_up((3, 4));
        assert_eq!(tiny.crop, Some(rect(3, 4, 1, 1)));
        let mut outside = editor();
        assert_eq!(
            outside.screenshot_empty_crop_inside_edges((29, 60), 6.),
            None
        );
        assert!(outside.begin_screenshot_crop_outside((29, 60), 6.));
        outside.pointer_up((20, 60));
        assert_eq!(outside.crop, Some(rect(20, 20, 110, 80)));
        let mut crop_tool = editor();
        crop_tool.select_tool(Tool::Crop);
        crop_tool.pointer_down((31, 60), 6.);
        crop_tool.pointer_up((31, 60));
        assert_eq!(crop_tool.crop, Some(rect(31, 20, 99, 80)));
        let mut corner = editor();
        assert!(corner.begin_screenshot_empty_crop_inside((31, 21), 6.));
        let constrained = corner.constrained_pointer((61, 71), true);
        corner.pointer_up(constrained);
        let crop = corner.crop.unwrap();
        assert!((crop.width as f32 / crop.height as f32 - 1.25).abs() < 0.03);
        // 真实边夹紧后，反映射代理点可以越出画布；不能再次夹紧代理而破坏比例。
        for (down, up, expected) in [
            ((35, 25), (299, 199), rect(130, 100, 125, 100)),
            ((125, 25), (1, 199), rect(0, 100, 30, 24)),
        ] {
            let mut corner = editor();
            assert!(corner.begin_screenshot_empty_crop_inside(down, 6.));
            let constrained = corner.constrained_pointer(up, true);
            corner.pointer_up(constrained);
            assert_eq!(corner.crop, Some(expected));
            assert!(corner.crop.unwrap().valid(300, 200));
        }
    }

    #[test]
    fn inside_edge_noop_return_and_cancel_preserve_mask_and_only_changed_finish_invalidates() {
        let mut editor = editor();
        let object = Annotation::new(Tool::Rectangle, Point { x: 40., y: 40. }, Style::default());
        let id = editor.document.insert(object.clone());
        editor.document.remove(id);
        editor.document.set_crop_mask(Some(mask()));
        let crop = editor.crop;
        let crop_mask = editor.document.crop_mask.clone();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        assert!(editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.pointer_up((31, 60));
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, crop_mask);
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.pointer_move((10, 60));
        editor.pointer_up((31, 60));
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, crop_mask);
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.pointer_move((10, 60));
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, crop_mask);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        assert!(editor.document.can_undo());
        assert!(editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.pointer_up((21, 60));
        assert_eq!(editor.crop, Some(rect(20, 20, 110, 80)));
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document.revision(), revision + 1);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        // 只移除失效 mask 的历史；原对象删除/插入撤销与重做仍可用。
        editor.document.undo();
        assert_eq!(editor.document.get(id), Some(&object));
        editor.document.redo();
        assert!(editor.document.items().is_empty());
        assert_eq!(editor.crop, Some(rect(20, 20, 110, 80)));
    }

    #[test]
    fn inside_edge_qualification_keeps_empty_center_objects_drafts_and_shared_input_routes() {
        let mut editor = editor();
        for p in [(80, 60), (29, 60), (130, 60), (80, 100), (-1, 60)] {
            assert_eq!(editor.screenshot_empty_crop_inside_edges(p, 6.), None);
            assert!(!editor.begin_screenshot_empty_crop_inside(p, 6.));
        }
        assert!(editor.begin_screenshot_empty_crop_center((80, 60), 6.));
        assert_eq!(
            editor.screenshot_empty_crop_inside_edges((31, 60), 6.),
            None
        );
        editor.cancel_gesture();
        assert!(
            crate::capture_interaction::screenshot_empty_crop_inside_target(
                &editor,
                (31, 60),
                6.,
                gpui::Modifiers::default(),
                true,
            )
            .is_some()
        );
        assert!(
            crate::capture_interaction::screenshot_empty_crop_inside_target(
                &editor,
                (31, 60),
                6.,
                gpui::Modifiers::default(),
                false,
            )
            .is_none()
        );
        for modifiers in [
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
            gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
            gpui::Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            assert!(
                crate::capture_interaction::screenshot_empty_crop_inside_target(
                    &editor,
                    (31, 60),
                    6.,
                    modifiers,
                    true,
                )
                .is_none()
            );
        }
        let crop = editor.crop;
        for invalid in [
            None,
            Some(Rect::full(0, 10)),
            Some(rect(u32::MAX, 20, 100, 80)),
        ] {
            editor.crop = invalid;
            assert!(!editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        }
        editor.crop = crop;
        editor.lasso_draft = Some(mask());
        assert!(!editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.lasso_draft = None;
        editor.text_draft = Some(TextDraft {
            id: None,
            annotation: Annotation::new(Tool::Text, Point { x: 40., y: 40. }, Style::default()),
        });
        assert!(!editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.text_draft = None;
        editor.select_tool(Tool::Rectangle);
        assert!(!editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.select_tool(Tool::Edit);
        // 共享图片/贴图入口继续原 Edit Marquee，不自动加入截图内侧调整。
        editor.pointer_down((31, 60), 6.);
        assert!(!editor.is_changing_crop());
        editor.pointer_up((21, 60));
        assert_eq!(editor.crop, crop);
        let id = editor.document.insert(Annotation::new(
            Tool::Rectangle,
            Point { x: 60., y: 40. },
            Style::default(),
        ));
        assert!(!editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
        editor.document.remove(id);
        assert!(editor.begin_screenshot_empty_crop_inside((31, 60), 6.));
    }
}

fn resize_bounds(r: Bounds, handle: usize, p: Point) -> Bounds {
    let (mut x, mut y, mut right, mut bottom) = (r.x, r.y, r.x + r.width, r.y + r.height);
    if [0, 3, 5].contains(&handle) {
        x = p.x;
    }
    if [2, 4, 7].contains(&handle) {
        right = p.x;
    }
    if [0, 1, 2].contains(&handle) {
        y = p.y;
    }
    if [5, 6, 7].contains(&handle) {
        bottom = p.y;
    }
    let bx = x.min(right);
    let by = y.min(bottom);
    Bounds {
        x: bx,
        y: by,
        width: (right - x).abs().max(2.),
        height: (bottom - y).abs().max(2.),
    }
}

/// 圆角柄用与该角同向的对角光标；旋转后按柄相对中心的屏幕方向取。
fn radius_hover(a: &Annotation, control: &crate::radius_handle::RadiusControl) -> Hover {
    let center = crate::transform::center(a.local_bounds());
    if (control.position.x - center.x) * (control.position.y - center.y) >= 0. {
        Hover::DiagonalDown
    } else {
        Hover::DiagonalUp
    }
}
fn resize_hover(a: &Annotation, index: usize) -> Hover {
    if a.rotation == 0. {
        return Hover::handle(index);
    }
    let p = a.handles()[index];
    let center = crate::transform::center(a.local_bounds());
    let angle = ((p.y - center.y).atan2(p.x - center.x).to_degrees() / 45.).round() as i32;
    match angle.rem_euclid(4) {
        0 => Hover::Horizontal,
        1 => Hover::DiagonalDown,
        2 => Hover::Vertical,
        _ => Hover::DiagonalUp,
    }
}
fn resize_rotated_bounds(r: Bounds, handle: usize, p: Point) -> Bounds {
    let (mut x, mut y, mut right, mut bottom) = (r.x, r.y, r.x + r.width, r.y + r.height);
    if [0, 3, 5].contains(&handle) {
        x = p.x;
    }
    if [2, 4, 7].contains(&handle) {
        right = p.x;
    }
    if [0, 1, 2].contains(&handle) {
        y = p.y;
    }
    if [5, 6, 7].contains(&handle) {
        bottom = p.y;
    }
    Bounds {
        x: x.min(right),
        y: y.min(bottom),
        width: (right - x).abs().max(2.),
        height: (bottom - y).abs().max(2.),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn editor() -> Editor {
        Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 100,
                width: 200,
                height: 150,
            }),
        )
    }
    fn draw(e: &mut Editor, tool: Tool, a: (i32, i32), b: (i32, i32)) {
        e.select_tool(tool);
        e.pointer_down(a, 4.);
        e.pointer_move(b);
        e.pointer_up(b);
    }
    #[test]
    fn highlighter_line_drag_commits_two_points_horizontal_vertical_and_dot_once() {
        for (start, end) in [
            ((20, 40), (180, 40)),
            ((70, 20), (70, 180)),
            ((50, 50), (50, 50)),
        ] {
            let mut e = editor();
            e.select_tool(Tool::Highlighter);
            e.set_highlighter_shape(HighlighterShape::Line);
            e.pointer_down(start, 4.);
            e.pointer_move((90, 110));
            e.pointer_move((130, 60));
            e.pointer_up(end);
            let line = &e.document.items()[0].annotation;
            assert_eq!(
                line.points,
                vec![Point::from_tuple(start), Point::from_tuple(end)]
            );
            assert!(line.has_endpoint_handles());
            assert_eq!(e.document.edit_revision(), 1);
            e.undo();
            assert!(e.document.items().is_empty());
            assert!(!e.document.can_undo());
            e.redo();
            assert_eq!(e.document.items()[0].annotation.points.len(), 2);
        }
        let mut e = editor();
        e.select_tool(Tool::Highlighter);
        e.set_highlighter_shape(HighlighterShape::Line);
        e.pointer_down((40, 40), 4.);
        let diagonal = e.constrained_point(Point { x: 150., y: 130. }, true);
        assert!((diagonal.x - diagonal.y).abs() < 0.001);
        e.pointer_up_at(diagonal);
        let a = &e.document.items()[0].annotation;
        assert!((a.points[1].x - a.points[1].y).abs() < 0.001);
        let edge = constrained_line_point(
            Point { x: 390., y: 280. },
            Point { x: 480., y: 380. },
            std::f32::consts::FRAC_PI_4,
        );
        assert!((edge.x - 390. - (edge.y - 280.)).abs() < 0.001);
        assert!(edge.x > 400. && edge.y > 300.);
    }
    #[test]
    fn highlighter_line_rotated_endpoint_and_move_keep_opposite_world_point_and_undo() {
        let mut e = editor();
        let mut line = Annotation::new(
            Tool::Highlighter,
            Point { x: 60., y: 90. },
            Style {
                highlighter_shape: HighlighterShape::Line,
                ..Style::default()
            },
        );
        line.update(Point { x: 220., y: 90. });
        line.rotation = 27.;
        let id = e.document.insert(line.clone());
        e.select_tool(Tool::Edit);
        e.selected = Some(id);
        e.style = line.style;
        let ends = line.handles();
        assert_eq!(e.hover_at(ends[0], 4.), Hover::Crosshair);
        e.pointer_down_at(ends[0], 4., false);
        let target = Point {
            x: ends[0].x + 25.,
            y: ends[0].y + 30.,
        };
        e.pointer_up_at(target);
        let changed = e.document.get(id).unwrap();
        assert!(changed.handles()[0].distance(target) < 0.001);
        assert!(changed.handles()[1].distance(ends[1]) < 0.001);
        assert_eq!(changed.points.len(), 2);
        let edited = changed.clone();
        let center = Point {
            x: (target.x + ends[1].x) / 2.,
            y: (target.y + ends[1].y) / 2.,
        };
        e.pointer_down_at(center, 4., false);
        e.pointer_up_at(Point {
            x: center.x + 18.,
            y: center.y - 12.,
        });
        let moved = e.document.get(id).unwrap();
        for (now, before) in moved.handles().iter().zip(edited.handles()) {
            assert!(
                now.distance(Point {
                    x: before.x + 18.,
                    y: before.y - 12.
                }) < 0.001
            );
        }
        e.undo();
        assert_eq!(e.document.get(id), Some(&edited));
        e.undo();
        assert_eq!(e.document.get(id), Some(&line));
        assert!(e.document.can_redo());
        e.selected = Some(id);
        e.pointer_down_at(ends[1], 4., false);
        let snapped = e.constrained_point(
            Point {
                x: 280.,
                y: ends[0].y + 8.,
            },
            true,
        );
        assert!((snapped.y - ends[0].y).abs() < 0.001);
        e.pointer_move_at(snapped);
        e.cancel_gesture();
        assert_eq!(e.document.get(id), Some(&line));
        assert!(e.document.can_redo());
    }
    #[test]
    fn highlighter_line_shape_changes_only_next_stroke_and_preserves_active_mode_redo() {
        let mut e = editor();
        e.select_tool(Tool::Highlighter);
        e.set_highlighter_shape(HighlighterShape::Line);
        e.pointer_down((30, 40), 4.);
        e.pointer_up((170, 40));
        let first = e.document.items()[0].annotation.clone();
        let revision = e.document.edit_revision();
        e.set_highlighter_shape(HighlighterShape::Freehand);
        assert_eq!(e.document.items()[0].annotation, first);
        assert_eq!(e.document.edit_revision(), revision);
        e.pointer_down((30, 160), 4.);
        e.pointer_move((70, 140));
        e.set_highlighter_shape(HighlighterShape::Line);
        e.pointer_move((110, 170));
        e.pointer_up((150, 160));
        let second = &e.document.items()[1].annotation;
        assert_eq!(second.style.highlighter_shape, HighlighterShape::Freehand);
        assert_eq!(second.points.len(), 4);
        e.undo();
        assert!(e.document.can_redo());
        let revision = e.document.edit_revision();
        e.set_highlighter_shape(HighlighterShape::Freehand);
        assert_eq!(e.document.edit_revision(), revision);
        assert!(e.document.can_redo());
        e.set_highlighter_shape(HighlighterShape::Line);
        e.select_tool(Tool::Rectangle);
        e.select_tool(Tool::Highlighter);
        assert_eq!(e.style.highlighter_shape, HighlighterShape::Line);
        e.pointer_down((210, 190), 4.);
        e.pointer_move((270, 220));
        e.pointer_up((340, 190));
        assert_eq!(e.document.items()[1].annotation.points.len(), 2);
    }
    #[test]
    fn highlighter_line_blend_edit_is_one_transaction_and_updates_next_mode_only_for_highlights() {
        use crate::annotation::HighlighterBlend;
        let mut e = editor();
        draw(&mut e, Tool::Highlighter, (20, 40), (180, 50));
        let before = e.document.items()[0].annotation.clone();
        e.select_tool(Tool::Edit);
        e.selected = Some(e.document.items()[0].id);
        e.style = before.style;
        let revision = e.document.edit_revision();
        let mut alpha = e.style;
        alpha.highlighter_blend = HighlighterBlend::Alpha;
        alpha.opacity = 96. / 255.;
        e.set_style(alpha);
        assert_eq!(e.document.edit_revision(), revision + 1);
        assert_eq!(
            e.document.items()[0].annotation.style.highlighter_blend,
            HighlighterBlend::Alpha
        );
        assert_eq!(e.document.items()[0].annotation.points, before.points);
        assert_eq!(
            e.document.items()[0].annotation.style.highlighter_shape,
            before.style.highlighter_shape
        );
        e.set_style(alpha);
        assert_eq!(e.document.edit_revision(), revision + 1);
        e.undo();
        assert_eq!(e.document.items()[0].annotation, before);
        e.select_tool(Tool::Highlighter);
        assert_eq!(e.style.highlighter_blend, HighlighterBlend::Alpha);
        assert_eq!(e.style.opacity, 96. / 255.);
        assert!(e.document.can_redo());
        e.redo();
        assert_eq!(
            e.document.items()[0].annotation.style.highlighter_blend,
            HighlighterBlend::Alpha
        );

        draw(&mut e, Tool::Rectangle, (220, 140), (320, 220));
        e.select_all_objects();
        let revision = e.document.edit_revision();
        let mut multiply = e.style;
        multiply.highlighter_blend = HighlighterBlend::Multiply;
        e.set_style(multiply);
        assert_eq!(e.document.edit_revision(), revision + 1);
        assert_eq!(
            e.document.items()[0].annotation.style.highlighter_blend,
            HighlighterBlend::Multiply
        );
        assert_eq!(
            e.document.items()[1].annotation.style.highlighter_blend,
            HighlighterBlend::Alpha
        );
        assert_eq!(e.document.items()[0].annotation.points, before.points);
    }
    #[test]
    fn switching_tools_preserves_each_style_and_group_duplicate_preserves_spacing() {
        let mut e = editor();
        e.select_tool(Tool::Rectangle);
        let mut style = e.style;
        style.filled = true;
        style.radius = 20.;
        style.opacity = 0.65;
        e.set_style(style);
        e.select_tool(Tool::Mosaic);
        assert!(!e.style.filled);
        assert_eq!(e.style.opacity, 1.);
        e.select_tool(Tool::Highlighter);
        assert_eq!(e.style.width, 24.);
        assert_eq!(e.style.opacity, 1.);
        e.set_width(48.);
        e.select_tool(Tool::Rectangle);
        assert!(e.style.filled);
        assert_eq!(e.style.radius, 20.);
        assert_eq!(e.style.opacity, 0.65);
        e.select_tool(Tool::Highlighter);
        assert_eq!(e.style.width, 48.);
        draw(&mut e, Tool::Rectangle, (20, 20), (80, 80));
        draw(&mut e, Tool::Rectangle, (350, 20), (399, 80));
        e.select_all_objects();
        e.duplicate_selected();
        let items = e.document.items();
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[3].annotation.bounds().x - items[2].annotation.bounds().x,
            330.
        );
        e.undo();
        assert_eq!(e.document.items().len(), 2);
    }
    #[test]
    fn mixed_selection_property_change_preserves_unrelated_styles() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (10, 10), (80, 80));
        e.set_width(8.);
        draw(&mut e, Tool::Text, (120, 20), (120, 20));
        e.set_text("中文".into());
        e.finish_text();
        e.select_all_objects();
        let before: Vec<_> = e
            .document
            .items()
            .iter()
            .map(|o| o.annotation.style)
            .collect();
        e.set_color(0x123456);
        for (i, o) in e.document.items().iter().enumerate() {
            let mut expected = before[i];
            expected.color = 0x123456;
            assert_eq!(o.annotation.style, expected);
        }
        e.undo();
        for (i, o) in e.document.items().iter().enumerate() {
            assert_eq!(o.annotation.style, before[i]);
        }
    }
    #[test]
    fn full_canvas_drawing_and_crop_changes_preserve_objects() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (20, 30), (160, 160));
        let original = e.document.items()[0].clone();
        assert_eq!(original.annotation.points[0], Point { x: 20., y: 30. });
        e.select_tool(Tool::Crop);
        e.pointer_down((200, 200), 4.);
        e.pointer_up((220, 210));
        assert_eq!(e.crop.unwrap().x, 120);
        assert_eq!(e.document.items()[0], original);
        e.set_crop(None);
        e.pointer_down((10, 10), 4.);
        e.pointer_up((380, 280));
        assert_eq!(e.document.items()[0], original);
        e.undo();
        assert!(e.document.items().is_empty());
        e.redo();
        assert_eq!(e.document.items()[0], original);
    }
    #[test]
    fn precise_crop_preserves_world_objects_selection_tool_and_object_history() {
        let original = Rect::full(120, 90);
        let mut e = Editor::new(Rect::full(400, 300), Some(original));
        let first = e.document.insert(Annotation::new(
            Tool::Rectangle,
            Point { x: 250., y: 200. },
            Style::default(),
        ));
        let second = e.document.insert(Annotation::new(
            Tool::Ellipse,
            Point { x: 330., y: 240. },
            Style::default(),
        ));
        e.document.undo();
        e.select_tool(Tool::Edit);
        e.selected = Some(first);
        e.selection.push(first);
        let objects = e.document.items().to_vec();
        let style = e.style;
        let stamp = e.scene_stamp();
        assert!(!e.apply_precise_crop(original, original).unwrap());
        assert_eq!(e.scene_stamp(), stamp);
        assert!(e
            .apply_precise_crop(
                original,
                Rect {
                    x: 390,
                    y: 0,
                    width: 40,
                    height: 10
                },
            )
            .is_err());
        assert_eq!(e.scene_stamp(), stamp);
        let resized = Rect {
            x: 120,
            y: 100,
            width: 80,
            height: 75,
        };
        assert!(e.apply_precise_crop(original, resized).unwrap());
        assert_eq!(e.document.items(), objects);
        assert_eq!(e.selected, Some(first));
        assert_eq!(e.selection, vec![first]);
        assert_eq!(e.tool, Tool::Edit);
        assert_eq!(e.style, style);
        assert!(e.document.can_undo() && e.document.can_redo());
        e.document.redo();
        assert_eq!(e.document.items()[0].id, first);
        assert_eq!(e.document.items()[1].id, second);
        e.document.undo();
        e.document.undo();
        assert!(e.document.items().is_empty());
        assert_eq!(e.crop, Some(resized));
        assert!(!e.document.can_undo());
    }

    #[test]
    fn precise_crop_rejects_stale_range_and_live_drawing_without_cancelling_it() {
        let original = Rect::full(120, 90);
        let changed = Rect::full(160, 100);
        let mut e = Editor::new(Rect::full(400, 300), Some(original));
        e.select_tool(Tool::Rectangle);
        e.pointer_down((30, 30), 3.);
        let stamp = e.scene_stamp();
        assert!(e.apply_precise_crop(original, changed).is_err());
        assert_eq!(e.scene_stamp(), stamp);
        assert!(e.is_dragging());
        assert_eq!(e.crop, Some(original));
        e.cancel_gesture();
        assert!(e.apply_precise_crop(original, changed).unwrap());
        let stamp = e.scene_stamp();
        assert!(e.apply_precise_crop(original, original).is_err());
        assert_eq!(e.scene_stamp(), stamp);
        assert_eq!(e.crop, Some(changed));
    }
    #[test]
    fn object_drag_resize_style_and_delete_are_atomic_undo_operations() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (20, 30), (160, 160));
        e.select_tool(Tool::Edit);
        e.pointer_down((70, 70), 4.);
        for x in 71..110 {
            e.pointer_move((x, 80));
        }
        e.pointer_up((110, 80));
        assert_eq!(e.document.items()[0].annotation.bounds().x, 60.);
        e.undo();
        assert_eq!(e.document.items()[0].annotation.bounds().x, 20.);
        e.redo();
        e.pointer_down((100, 100), 4.);
        e.pointer_up((100, 100));
        e.pointer_down((200, 170), 4.);
        e.pointer_up((240, 200));
        assert_eq!(e.document.items()[0].annotation.bounds().width, 180.);
        e.set_color(0x123456);
        assert_eq!(e.document.items()[0].annotation.style.color, 0x123456);
        e.undo();
        assert_eq!(
            e.document.items()[0].annotation.style.color,
            Style::default().color
        );
        e.pointer_down((100, 100), 4.);
        e.pointer_up((100, 100));
        e.delete_selected();
        assert!(e.document.items().is_empty());
        e.undo();
        assert_eq!(e.document.items().len(), 1);
    }
    #[test]
    fn cancelled_edits_do_not_commit_or_destroy_redo() {
        let mut e = editor();
        draw(&mut e, Tool::Mosaic, (20, 20), (80, 80));
        draw(&mut e, Tool::Arrow, (200, 20), (300, 80));
        e.undo();
        e.select_tool(Tool::Edit);
        e.pointer_down((40, 40), 4.);
        e.pointer_move((100, 100));
        assert!(e.cancel_gesture());
        assert_eq!(e.document.items()[0].annotation.bounds().x, 20.);
        assert!(e.document.can_redo());
        e.pointer_down((40, 40), 4.);
        e.pointer_up((40, 40));
        assert!(e.document.can_redo());
    }
    #[test]
    fn topmost_hit_and_arrow_endpoint_edit_use_canvas_coordinates() {
        let mut e = editor();
        draw(&mut e, Tool::Mosaic, (20, 20), (80, 80));
        draw(&mut e, Tool::Rectangle, (30, 30), (70, 70));
        e.select_tool(Tool::Edit);
        e.pointer_down((40, 40), 4.);
        e.pointer_up((40, 40));
        assert_eq!(e.selected, Some(e.document.items()[1].id));
        draw(&mut e, Tool::Arrow, (200, 20), (300, 80));
        e.pointer_down((300, 80), 4.);
        e.pointer_up((350, 120));
        assert_eq!(
            e.document.items()[2].annotation.points[1],
            Point { x: 350., y: 120. }
        );
        e.undo();
        assert_eq!(
            e.document.items()[2].annotation.points[1],
            Point { x: 300., y: 80. }
        );
    }
    #[test]
    fn resize_draw_and_nudge_extend_past_the_canvas() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (380, 280), (450, 340));
        let drawn = e.document.items()[0].annotation.bounds();
        assert!((drawn.x - 380.).abs() < 0.001);
        assert!((drawn.y - 280.).abs() < 0.001);
        assert!((drawn.width - 70.).abs() < 0.001);
        assert!((drawn.height - 60.).abs() < 0.001);
        e.delete_selected();
        draw(&mut e, Tool::Mosaic, (350, 250), (400, 300));
        e.pointer_down((350, 250), 4.);
        e.pointer_up((500, 500));
        let b = e.document.items()[0].annotation.bounds();
        assert!((b.x - 400.).abs() < 0.001 && (b.y - 300.).abs() < 0.001);
        assert!((b.width - 100.).abs() < 0.001 && (b.height - 200.).abs() < 0.001);
        assert!(b.x + b.width > 400. && b.y + b.height > 300.);
        e.nudge(10., 10.);
        let b = e.document.items()[0].annotation.bounds();
        assert!((b.x - 410.).abs() < 0.001 && (b.y - 310.).abs() < 0.001);
        assert!(b.width.is_finite() && b.height.is_finite() && b.width > 0. && b.height > 0.);
    }
    #[test]
    fn selected_rectangle_moves_in_its_drawing_tool_without_creating_another_object() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (20, 30), (160, 160));
        e.pointer_down((70, 70), 4.);
        e.pointer_up((90, 80));
        assert_eq!(e.document.items().len(), 1);
        assert_eq!(e.document.items()[0].annotation.bounds().x, 40.);
        e.undo();
        assert_eq!(e.document.items()[0].annotation.bounds().x, 20.);
    }
}

#[cfg(test)]
mod edit_behavior_tests {
    use super::*;
    fn editor() -> Editor {
        Editor::new(Rect::full(640, 480), Some(Rect::full(640, 480)))
    }
    fn draw(e: &mut Editor, tool: Tool, a: (i32, i32), b: (i32, i32)) {
        e.select_tool(tool);
        e.pointer_down(a, 4.);
        e.pointer_up(b);
    }
    #[test]
    fn text_commit_cancel_and_edit_are_one_transaction() {
        let mut e = editor();
        e.select_tool(Tool::Text);
        e.pointer_down((20, 20), 4.);
        e.set_text("中文🙂".into());
        assert!(e.document.items().is_empty());
        e.finish_text();
        let id = e.selected.unwrap();
        assert_eq!(e.document.get(id).unwrap().text, "中文🙂");
        e.edit_text();
        e.set_text("取消".into());
        e.cancel_text();
        assert_eq!(e.document.get(id).unwrap().text, "中文🙂");
        e.edit_text();
        e.set_text("修改\n第二行".into());
        e.finish_text();
        e.undo();
        assert_eq!(e.document.get(id).unwrap().text, "中文🙂");
        e.undo();
        assert!(e.document.items().is_empty());
        e.redo();
        assert_eq!(e.document.get(id).unwrap().text, "中文🙂");
    }
    #[test]
    fn eraser_stroke_removes_all_hits_with_one_undo() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (20, 20), (80, 80));
        draw(&mut e, Tool::Ellipse, (120, 20), (180, 80));
        e.select_tool(Tool::Eraser);
        e.pointer_down((40, 40), 4.);
        e.pointer_move((140, 40));
        assert_eq!(e.document.items().len(), 2);
        e.pointer_up((140, 40));
        assert!(e.document.items().is_empty());
        e.undo();
        assert_eq!(e.document.items().len(), 2);
        e.redo();
        assert!(e.document.items().is_empty());
    }
    #[test]
    fn polyline_clicks_commit_only_when_finished() {
        let mut e = editor();
        e.select_tool(Tool::Polyline);
        for p in [(20, 20), (60, 30), (100, 70)] {
            e.pointer_down(p, 4.);
            e.pointer_up(p);
        }
        assert!(e.document.items().is_empty());
        assert!(e.has_polyline());
        e.finish_gesture();
        assert_eq!(e.document.items()[0].annotation.points.len(), 3);
        e.undo();
        assert!(e.document.items().is_empty());
    }
    #[test]
    fn batch_move_properties_delete_duplicate_and_order_are_reversible() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (20, 20), (80, 80));
        draw(&mut e, Tool::Rectangle, (120, 20), (180, 80));
        e.select_all_objects();
        e.pointer_down((40, 40), 4.);
        e.pointer_up((60, 50));
        assert_eq!(e.document.items()[0].annotation.bounds().x, 40.);
        assert_eq!(e.document.items()[1].annotation.bounds().x, 140.);
        e.undo();
        assert_eq!(e.document.items()[1].annotation.bounds().x, 120.);
        e.select_all_objects();
        e.set_color(0x123456);
        e.undo();
        assert_eq!(
            e.document.items()[0].annotation.style.color,
            Style::default().color
        );
        e.select_all_objects();
        e.duplicate_selected();
        assert_eq!(e.document.items().len(), 4);
        e.undo();
        assert_eq!(e.document.items().len(), 2);
        let ids: Vec<_> = e.document.items().iter().map(|o| o.id).collect();
        e.document.reorder(&[ids[0]], true);
        assert_eq!(e.document.items()[1].id, ids[0]);
        e.undo();
        assert_eq!(e.document.items()[0].id, ids[0]);
    }
    #[test]
    fn serial_at_corner_and_constrained_shapes_never_panic() {
        let mut e = editor();
        draw(&mut e, Tool::Serial, (0, 0), (0, 0));
        e.nudge(-10., -10.);
        e.select_tool(Tool::Rectangle);
        e.pointer_down((100, 100), 4.);
        let p = e.constrained_pointer((150, 120), true);
        e.pointer_up(p);
        let b = e.document.items()[1].annotation.bounds();
        assert_eq!(b.width, b.height);
    }
    #[test]
    fn wheel_during_draw_preserves_gesture_and_commits_once() {
        let mut e = editor();
        e.select_tool(Tool::Pen);
        e.pointer_down((30, 30), 4.);
        e.pointer_move((60, 50));
        assert!(e.adjust_parameter(Parameter::Width, 3, 1, (60, 50), 4.));
        assert!(e.is_dragging());
        assert!(e.document.items().is_empty());
        e.pointer_move((80, 80));
        e.pointer_up((100, 90));
        assert_eq!(e.document.items().len(), 1);
        assert_eq!(e.document.items()[0].annotation.style.width, 7.);
        assert_eq!(
            e.document.items()[0].annotation.points.last().unwrap().x,
            100.
        );
        e.undo();
        assert!(e.document.items().is_empty());
        assert!(!e.document.can_undo());
    }
    #[test]
    fn wheel_during_object_drag_preserves_style_and_original_undo() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (40, 40), (140, 100));
        let original = e.document.items()[0].annotation.clone();
        e.pointer_down((90, 70), 4.);
        e.adjust_parameter(Parameter::Width, 2, 2, (90, 70), 4.);
        assert!(e.is_dragging());
        e.pointer_move((100, 80));
        e.pointer_up((110, 90));
        assert_eq!(e.document.items()[0].annotation.style.width, 6.);
        assert_eq!(e.document.items()[0].annotation.bounds().x, 60.);
        e.undo();
        assert_eq!(e.document.items()[0].annotation, original);
        e.undo();
        assert!(e.document.items().is_empty());
    }
    #[test]
    fn wheel_burst_merges_and_noop_at_limit_keeps_redo() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (40, 40), (140, 100));
        e.adjust_parameter(Parameter::Radius, 2, 3, (0, 0), 4.);
        e.adjust_parameter(Parameter::Radius, 3, 3, (0, 0), 4.);
        assert_eq!(e.document.items()[0].annotation.style.radius, 5.);
        e.undo();
        assert_eq!(e.document.items()[0].annotation.style.radius, 0.);
        e.redo();
        assert_eq!(e.document.items()[0].annotation.style.radius, 5.);
        e.undo();
        e.style.radius = 50.;
        assert!(!e.adjust_parameter(Parameter::Radius, 1, 4, (0, 0), 4.));
        assert!(e.document.can_redo());
    }
    #[test]
    fn crop_wheel_adjusts_center_edges_and_stays_inside_canvas() {
        let mut e = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 100,
                width: 200,
                height: 150,
            }),
        );
        let old = e.crop.unwrap();
        assert!(e.adjust_parameter(Parameter::Crop, 1, 1, (180, 170), 4.));
        let expanded = e.crop.unwrap();
        assert_eq!(expanded.x, old.x - 1);
        assert_eq!(expanded.width, old.width + 2);
        let anchor = expanded.x;
        e.adjust_parameter(
            Parameter::Crop,
            3,
            1,
            ((expanded.x + expanded.width) as i32, 170),
            4.,
        );
        assert_eq!(e.crop.unwrap().x, anchor);
        assert_eq!(e.crop.unwrap().width, expanded.width + 3);
        assert!(!e.adjust_parameter(Parameter::Crop, 1, 1, (0, 0), 4.));
        e.adjust_parameter(Parameter::Crop, -500, 1, (180, 170), 4.);
        assert!(e.crop.unwrap().width >= 1 && e.crop.unwrap().height >= 1);
        e.crop = Some(Rect::full(400, 300));
        assert!(!e.adjust_parameter(Parameter::Crop, 1, 1, (180, 170), 4.));
        assert!(!e.document.can_undo());
    }
    #[test]
    fn shift_constraints_cover_crop_boundary_resize_and_arrow_endpoint() {
        let mut e = Editor::new(Rect::full(400, 300), None);
        e.crop = None;
        e.pointer_down((390, 290), 4.);
        let p = e.constrained_pointer((500, 350), true);
        assert_eq!(p, (400, 300));
        e.pointer_up(p);
        assert_eq!(e.crop.unwrap().width, e.crop.unwrap().height);
        draw(&mut e, Tool::Rectangle, (100, 100), (200, 150));
        e.pointer_down((200, 150), 4.);
        let p = e.constrained_pointer((300, 250), true);
        assert_eq!(p, (400, 250));
        e.pointer_up(p);
        let b = e.document.items()[0].annotation.bounds();
        assert_eq!(b.width, b.height * 2.);
        draw(&mut e, Tool::Arrow, (30, 30), (130, 30));
        e.pointer_down((130, 30), 4.);
        let p = e.constrained_pointer((150, 90), true);
        assert_eq!(p.0 - 30, p.1 - 30);
        e.pointer_up(p);
    }
    #[test]
    fn nearest_small_handle_and_micro_drag_do_not_jump() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (50, 50), (54, 54));
        assert_eq!(e.hover((54, 52), 6.), Hover::Horizontal);
        e.pointer_down((54, 52), 6.);
        e.pointer_up((80, 52));
        let b = e.document.items()[0].annotation.bounds();
        assert_eq!(b.width, 30.);
        assert_eq!(b.height, 4.);
        e.pointer_down((65, 52), 1.);
        e.pointer_up((66, 53));
        assert_eq!(e.document.items()[0].annotation.bounds(), b);
        e.undo();
        assert_eq!(e.document.items()[0].annotation.bounds().width, 4.);
        draw(&mut e, Tool::Ellipse, (200, 200), (203, 203));
        assert_eq!(e.document.items().len(), 1);
    }
    #[test]
    fn ellipse_and_serial_hit_use_visible_geometry() {
        let mut a = Annotation::new(Tool::Ellipse, Point { x: 40., y: 40. }, Style::default());
        a.update(Point { x: 140., y: 100. });
        assert!(!a.hit(Point { x: 42., y: 42. }, 0., true));
        assert!(a.hit(Point { x: 90., y: 70. }, 0., true));
        let a = Annotation::new(Tool::Serial, Point { x: 90., y: 70. }, Style::default());
        assert!(!a.hit(Point { x: 68., y: 48. }, 0., true));
        assert!(a.hit(Point { x: 90., y: 70. }, 0., true));
    }
    #[test]
    fn committed_text_wheel_grows_bounds_and_undo_restores_size() {
        let mut e = editor();
        e.select_tool(Tool::Text);
        e.pointer_down((40, 40), 4.);
        e.set_text("中文字号".into());
        e.finish_text();
        let original = e.selected_annotation().unwrap().clone();
        e.adjust_parameter(Parameter::Font, 8, 1, (40, 40), 4.);
        let resized = e.selected_annotation().unwrap();
        assert_eq!(resized.bounds().width, original.bounds().width);
        assert!(resized.bounds().height > original.bounds().height);
        e.undo();
        assert_eq!(e.document.items()[0].annotation, original);
    }
    #[test]
    fn returning_to_drag_origin_invalidates_preview_and_does_not_commit() {
        let mut e = editor();
        draw(&mut e, Tool::Rectangle, (40, 40), (140, 100));
        let original = e.document.items()[0].annotation.clone();
        e.pointer_down((90, 70), 4.);
        e.pointer_move((110, 90));
        let stamp = e.scene_stamp();
        e.pointer_move((91, 71));
        assert_ne!(e.scene_stamp(), stamp);
        assert_eq!(e.selected_annotation().unwrap(), &original);
        e.pointer_up((91, 71));
        e.undo();
        assert!(e.document.items().is_empty());
    }
    #[test]
    fn arrow_two_clicks_resume_drag_and_commit_one_undo() {
        let mut e = editor();
        e.select_tool(Tool::Arrow);
        e.pointer_down((30, 30), 4.);
        e.pointer_up((30, 30));
        assert!(!e.is_dragging());
        assert!(e.has_pending_drawing());
        assert!(!e.document.can_undo());
        e.pointer_move((160, 80));
        e.adjust_parameter(Parameter::Width, 2, 1, (160, 80), 4.);
        assert!(e.has_pending_drawing());
        assert!(e.document.items().is_empty());
        let tip = e.constrained_pointer((160, 80), true);
        e.pointer_down(tip, 4.);
        assert!(e.is_dragging());
        e.pointer_up((180, 90));
        assert!(!e.has_pending_drawing());
        assert_eq!(e.document.items().len(), 1);
        let a = &e.document.items()[0].annotation;
        assert_eq!(a.points[0], Point { x: 30., y: 30. });
        assert_eq!(a.points[1], Point { x: 180., y: 90. });
        assert_eq!(a.style.width, 6.);
        e.undo();
        assert!(e.document.items().is_empty());
        assert!(!e.document.can_undo());
    }
    #[test]
    fn pending_arrow_cancel_switch_and_hit_existing_never_create_ghost() {
        let mut e = editor();
        draw(&mut e, Tool::Arrow, (100, 100), (200, 100));
        let original = e.document.items()[0].annotation.clone();
        e.pointer_down((300, 300), 4.);
        e.pointer_up((300, 300));
        e.pointer_move((400, 400));
        assert!(e.cancel_gesture());
        assert_eq!(e.document.items().len(), 1);
        e.pointer_down((300, 300), 4.);
        e.pointer_up((300, 300));
        e.pointer_move((400, 400));
        e.select_tool(Tool::Rectangle);
        assert!(!e.has_pending_drawing());
        assert_eq!(e.document.items().len(), 1);
        e.select_tool(Tool::Arrow);
        e.pointer_down((300, 300), 4.);
        e.pointer_up((300, 300));
        e.pointer_down((150, 100), 4.);
        e.pointer_up((150, 100));
        assert!(!e.has_pending_drawing());
        assert_eq!(e.document.items()[0].annotation, original);
        e.undo();
        assert!(e.document.items().is_empty());
    }
    #[test]
    fn local_fill_preparation_gate_preserves_redo_tool_and_draft_until_success() {
        let canvas = Rect::full(160, 120);
        let mut e = Editor::new(canvas, Some(canvas));
        let mut rectangle =
            Annotation::new(Tool::Rectangle, Point { x: 12., y: 12. }, Style::default());
        rectangle.update(Point { x: 35., y: 35. });
        e.document.insert(rectangle.clone());
        e.document.undo();
        assert!(e.document.can_redo());
        e.select_tool(Tool::MosaicBrush);
        e.select_tool(Tool::LocalFill);
        e.pointer_down((60, 60), 4.);
        e.pointer_move((80, 60));
        e.pointer_up((90, 60));
        let draft = e.local_fill_candidate().unwrap();
        let stamp = e.scene_stamp();
        let style = e.style;
        let presets = e.tool_styles.clone();
        assert!(!e.finish_gesture());
        e.select_tool(Tool::Mosaic);
        e.pointer_down((110, 90), 4.);
        e.duplicate_selected();
        e.rotate_selection(30.);
        e.select_all_objects();
        assert_eq!(e.tool, Tool::LocalFill);
        assert!(e.effect_brush_mode);
        assert_eq!(e.style, style);
        assert_eq!(e.tool_styles, presets);
        assert_eq!(e.scene_stamp(), stamp);
        assert_eq!(e.local_fill_candidate(), Some(draft.clone()));
        assert!(e.document.items().is_empty());
        assert!(e.document.can_redo());
        // redo 仍恢复失败前的真实对象，证明门未消费历史记录。
        e.document.redo();
        assert_eq!(e.document.items()[0].annotation, rectangle);
        e.document.undo();
        let pixels = vec![180; canvas.width as usize * canvas.height as usize * 4];
        let source = crate::scene::SourcePixels {
            fill: None,
            canvas,
            planes: vec![crate::scene::PixelPlane {
                rect: canvas,
                bgra: &pixels,
            }],
            overlay: None,
        };
        let below = crate::scene::Scene::default().updated(&e.document_annotations(), &source);
        let prepared = crate::local_fill_scene::prepare(&draft, &source, &below)
            .unwrap()
            .unwrap();
        let ready = below.with_prepared_fill(&draft, prepared, &source).unwrap();
        assert!(e.commit_prepared_local_fill(&draft));
        assert_eq!(e.document.items().len(), 1);
        assert!(!e.document.can_redo());
        assert!(e.selected_ids().is_empty());
        assert_eq!(ready.annotations, e.document_annotations());
        e.select_all_objects();
        assert!(e.selected_ids().is_empty());
    }
    #[test]
    fn local_fill_consecutive_commits_remain_unselected_and_use_one_undo_each() {
        let canvas = Rect::full(160, 120);
        let pixels = vec![190; canvas.width as usize * canvas.height as usize * 4];
        let source = crate::scene::SourcePixels {
            fill: None,
            canvas,
            planes: vec![crate::scene::PixelPlane {
                rect: canvas,
                bgra: &pixels,
            }],
            overlay: None,
        };
        let mut e = Editor::new(canvas, Some(canvas));
        e.select_tool(Tool::LocalFill);
        for (from, to) in [((30, 30), (50, 30)), ((90, 80), (120, 80))] {
            e.pointer_down(from, 4.);
            e.pointer_up(to);
            let candidate = e.local_fill_candidate().unwrap();
            let below = crate::scene::Scene::default().updated(&e.document_annotations(), &source);
            crate::local_fill_scene::prepare(&candidate, &source, &below)
                .unwrap()
                .unwrap();
            let mut changed = candidate.clone();
            changed.style.width += 1.;
            assert!(!e.commit_prepared_local_fill(&changed));
            assert!(e.commit_prepared_local_fill(&candidate));
            assert!(e.selected_ids().is_empty());
            assert_eq!(e.tool, Tool::LocalFill);
        }
        assert_eq!(e.document.items().len(), 2);
        e.undo();
        assert_eq!(e.document.items().len(), 1);
        e.undo();
        assert!(e.document.items().is_empty());
        e.redo();
        e.redo();
        assert_eq!(e.document.items().len(), 2);
    }
    #[test]
    fn local_fill_invalid_metadata_cannot_be_consumed_by_bare_finish_or_switch() {
        let mut e = editor();
        e.select_tool(Tool::LocalFill);
        e.pointer_down((40, 40), 4.);
        if let Some(Gesture::Draw(a)) = &mut e.gesture {
            a.style.width = f32::NAN;
        }
        let before = e.scene_stamp();
        assert!(!e.finish_gesture());
        e.select_tool(Tool::Pen);
        assert_eq!(e.tool, Tool::LocalFill);
        assert_eq!(e.scene_stamp(), before);
        assert!(e.local_fill_candidate().unwrap().style.width.is_nan());
        let source = crate::scene::SourcePixels {
            fill: None,
            canvas: e.canvas,
            planes: Vec::new(),
            overlay: None,
        };
        let below = crate::scene::Scene::default().updated(&e.document_annotations(), &source);
        assert!(crate::local_fill_scene::prepare(
            &e.local_fill_candidate().unwrap(),
            &source,
            &below
        )
        .is_err());
        assert!(e.document.items().is_empty());
    }
    #[test]
    fn local_fill_selected_effect_context_restores_shape_source_and_failed_preflight_keeps_it() {
        use crate::interaction::DrawingEffect;
        for selected_tool in [Tool::Mosaic, Tool::Blur, Tool::MosaicBrush, Tool::BlurBrush] {
            let canvas = Rect::full(32, 32);
            let mut e = Editor::new(canvas, Some(canvas));
            let selected_style = Style {
                width: 7.,
                block_size: 13,
                ..Style::default()
            };
            let mut selected =
                Annotation::new(selected_tool, Point { x: 4., y: 8. }, selected_style);
            selected.update(Point { x: 28., y: 24. });
            e.document.insert(selected.clone());
            // 先使用相反的入口，防止旧来源恰好掩盖选中对象的矩形/笔刷来源。
            e.select_tool(if selected_tool.freehand() {
                Tool::Mosaic
            } else {
                Tool::MosaicBrush
            });
            e.select_tool(Tool::Edit);
            e.pointer_down((16, 16), 1.);
            e.pointer_up((16, 16));
            assert_eq!(e.selected_annotation(), Some(&selected));
            e.style.block_size = 2;
            assert_eq!(e.effect_context(), (selected_tool, selected_style));
            e.select_tool(Tool::LocalFill);
            assert_eq!(e.effect_brush_mode, selected_tool.freehand());
            let expected_mosaic = if selected_tool.freehand() {
                Tool::MosaicBrush
            } else {
                Tool::Mosaic
            };
            let expected_blur = if selected_tool.freehand() {
                Tool::BlurBrush
            } else {
                Tool::Blur
            };
            assert_eq!(
                DrawingEffect::Mosaic.next_tool(e.tool, e.effect_brush_mode),
                expected_mosaic
            );
            assert_eq!(
                DrawingEffect::Blur.next_tool(e.tool, e.effect_brush_mode),
                expected_blur
            );
            e.select_tool(expected_mosaic);
            assert_eq!(e.tool, expected_mosaic);
            e.select_tool(Tool::LocalFill);
            e.set_width(50.);
            e.pointer_down((2, 16), 1.);
            e.pointer_move((30, 16));
            e.pointer_up((30, 16));
            let draft = e.local_fill_candidate().unwrap();
            let stamp = e.scene_stamp();
            let style = e.style;
            let presets = e.tool_styles.clone();
            let pixels = [80, 90, 100, 255].repeat(32 * 32);
            let source = crate::scene::SourcePixels {
                fill: None,
                canvas,
                planes: vec![crate::scene::PixelPlane {
                    rect: canvas,
                    bgra: &pixels,
                }],
                overlay: None,
            };
            let below = crate::scene::Scene::default().updated(&e.document_annotations(), &source);
            assert!(crate::local_fill_scene::prepare(&draft, &source, &below).is_err());
            e.select_tool(if selected_tool.freehand() {
                Tool::Blur
            } else {
                Tool::BlurBrush
            });
            assert_eq!(e.tool, Tool::LocalFill);
            assert_eq!(e.effect_brush_mode, selected_tool.freehand());
            assert_eq!(e.effect_context(), (Tool::LocalFill, style));
            assert_eq!(e.local_fill_candidate(), Some(draft));
            assert_eq!(e.scene_stamp(), stamp);
            assert_eq!(e.tool_styles, presets);
            assert_eq!(e.document.items()[0].annotation, selected);
        }
    }
    #[test]
    fn pencil_drawing_edit_mode_changes_only_derived_path_and_is_reversible() {
        let mut e = editor();
        e.select_tool(Tool::Pen);
        e.pointer_down((30, 30), 4.);
        for i in 1..50 {
            e.pointer_move((30 + i, 30 + if i % 2 == 0 { 1 } else { -1 }));
        }
        e.pointer_up((80, 30));
        assert!(e.selected_ids().is_empty());
        e.select_tool(Tool::Edit);
        e.pointer_down((55, 30), 4.);
        e.pointer_up((55, 30));
        let raw = e.selected_annotation().unwrap().points.clone();
        let fitted = e.selected_annotation().unwrap().segments();
        let mut style = e.style;
        style.pencil_smooth = false;
        e.set_style(style);
        assert_eq!(e.selected_annotation().unwrap().points, raw);
        assert_eq!(
            e.selected_annotation().unwrap().segments(),
            raw.windows(2).map(|p| (p[0], p[1])).collect::<Vec<_>>()
        );
        e.undo();
        assert_eq!(e.document.items()[0].annotation.segments(), fitted);
        e.redo();
        assert!(!e.document.items()[0].annotation.style.pencil_smooth);
    }
}
