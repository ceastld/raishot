//! 录制中独立编辑状态：只有显式绘图模式接受指针；帧只借用不可变 Scene。
use crate::{
    annotation::{HighlighterBlend, Point, Style, Tool},
    capture::Image,
    editor::Editor,
    geometry::{Rect, ScreenRect},
    scene::{Scene, SourcePixels},
    Result,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub(crate) fn accepts_pointer(drawing: bool, phase: super::Phase, stopping: bool) -> bool {
    drawing && phase.active() && !stopping
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveTool {
    Select,
    Rectangle,
    Arrow,
    Pen,
    Highlight,
}
impl LiveTool {
    pub(crate) fn tool(self) -> Tool {
        match self {
            Self::Select => Tool::Edit,
            Self::Rectangle => Tool::Rectangle,
            Self::Arrow => Tool::Arrow,
            Self::Pen => Tool::Pen,
            Self::Highlight => Tool::Highlighter,
        }
    }
}

/// 样式草稿票据：固定录制/Document/目标/输入来源；样式弹层每次点击生成并立即应用。
#[derive(Clone)]
pub(crate) struct PropertyDraft {
    session: Arc<()>,
    request: u64,
    epoch: u64,
    stamp: (u64, u64),
    selection: Vec<u64>,
    primary: Option<u64>,
    scope: (Vec<u64>, u64),
    before: Style,
    active_tool: Tool,
    dirty: (bool, bool, bool),
    pub(crate) tool: Tool,
    pub(crate) color: u32,
    pub(crate) width: f32,
    pub(crate) opacity: f32,
}
impl PropertyDraft {
    pub(crate) fn set_color(&mut self, color: u32) {
        self.color = color;
        self.dirty.0 = true;
    }
    #[cfg(test)]
    pub(crate) fn step(&mut self, parameter: crate::interaction::Parameter, steps: i32) {
        let style = parameter.adjusted(
            Style {
                width: self.width,
                opacity: self.opacity,
                ..self.before
            },
            self.tool,
            steps,
        );
        self.dirty.1 |= style.width != self.width;
        self.dirty.2 |= style.opacity != self.opacity;
        self.width = style.width;
        self.opacity = style.opacity;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKey {
    Undo,
    Redo,
    Delete,
    Clear,
}
/// 真实绘图宿主使用此映射；IME/焦点/弹层资格仍由宿主检查。
pub(crate) fn edit_key(
    key: &str,
    control: bool,
    shift: bool,
    alt: bool,
    platform: bool,
) -> Option<EditKey> {
    if alt || platform {
        return None;
    }
    match (key, control, shift) {
        ("z", true, false) => Some(EditKey::Undo),
        ("y", true, false) | ("z", true, true) => Some(EditKey::Redo),
        ("delete" | "backspace", false, false) => Some(EditKey::Delete),
        ("delete", false, true) => Some(EditKey::Clear),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Geometry {
    pub desktop: ScreenRect,
    pub region: ScreenRect,
    pub roi: Rect,
}
impl Geometry {
    pub(crate) fn client_global(origin: (i32, i32), client: Point, dpi: f32) -> Option<Point> {
        if !dpi.is_finite() || dpi <= 0. || !client.x.is_finite() || !client.y.is_finite() {
            return None;
        }
        let point = Point {
            x: origin.0 as f32 + client.x * dpi,
            y: origin.1 as f32 + client.y * dpi,
        };
        (point.x.is_finite() && point.y.is_finite()).then_some(point)
    }
    pub(crate) fn new(desktop: ScreenRect, region: ScreenRect) -> Result<Self> {
        let inside = region.left >= desktop.left
            && region.top >= desktop.top
            && i64::from(region.left) + i64::from(region.width)
                <= i64::from(desktop.left) + i64::from(desktop.width)
            && i64::from(region.top) + i64::from(region.height)
                <= i64::from(desktop.top) + i64::from(desktop.height);
        if !inside
            || [desktop.width, desktop.height, region.width, region.height]
                .iter()
                .any(|v| *v == 0 || *v > i32::MAX as u32)
        {
            return Err("录制标注画布或ROI无效".into());
        }
        Ok(Self {
            desktop,
            region,
            roi: region.local(desktop),
        })
    }
    fn local(self, global: Point, clamp: bool) -> Option<Point> {
        if !global.x.is_finite() || !global.y.is_finite() {
            return None;
        }
        let x = global.x - self.desktop.left as f32;
        let y = global.y - self.desktop.top as f32;
        let (left, top, right, bottom) = (
            self.roi.x as f32,
            self.roi.y as f32,
            (self.roi.x + self.roi.width) as f32,
            (self.roi.y + self.roi.height) as f32,
        );
        if !clamp && (x < left || y < top || x >= right || y >= bottom) {
            return None;
        }
        Some(Point {
            x: x.clamp(left, right),
            y: y.clamp(top, bottom),
        })
    }
}

pub(crate) struct FrameScene {
    pub geometry: Geometry,
    pub scene: Arc<Scene>,
    pub stamp: (u64, u64),
}
impl FrameScene {
    pub(crate) fn composite(&self, image: &mut Image, region: ScreenRect) -> Result<()> {
        if region != self.geometry.region {
            return Err("标注帧不能套用其它录制ROI".into());
        }
        self.scene.try_apply(image, self.geometry.roi)
    }
}
#[derive(Default)]
pub(crate) struct FrameState {
    scene: Mutex<Option<Arc<FrameScene>>>,
    present: AtomicBool,
    /// 隐藏时屏幕绘图层和编码都取不到标注帧；Document 与历史不变。
    hidden: AtomicBool,
}
impl FrameState {
    pub(crate) fn clear(&self) {
        self.present.store(false, Ordering::Release);
        self.hidden.store(false, Ordering::Release);
        *self.scene.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    pub(crate) fn snapshot(&self) -> Option<Arc<FrameScene>> {
        if !self.present.load(Ordering::Acquire) || self.hidden.load(Ordering::Acquire) {
            return None;
        }
        self.scene.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub(crate) fn hidden(&self) -> bool {
        self.hidden.load(Ordering::Acquire)
    }
    pub(crate) fn set_hidden(&self, hidden: bool) {
        self.hidden.store(hidden, Ordering::Release);
    }
}

pub(crate) struct LiveEditor {
    editor: Editor,
    pub(crate) geometry: Geometry,
    frames: Arc<FrameState>,
    drawing: bool,
    pointer: bool,
    cached_scene: Arc<Scene>,
    session: Arc<()>,
    input_epoch: u64,
    next_property: u64,
    property: Option<u64>,
}
impl LiveEditor {
    pub(crate) fn new(geometry: Geometry, frames: Arc<FrameState>) -> Self {
        frames.clear();
        let mut editor = Editor::new(
            Rect::full(geometry.desktop.width, geometry.desktop.height),
            Some(geometry.roi),
        );
        let mut highlight = crate::tool_presets::default_style(Tool::Highlighter);
        highlight.highlighter_blend = HighlighterBlend::Alpha;
        highlight.opacity = 0.35;
        highlight.color = 0xffd54f;
        highlight.width = 14.;
        editor.restore_tool_styles([(Tool::Highlighter, highlight)]);
        editor.select_tool(Tool::Rectangle);
        Self {
            editor,
            geometry,
            frames,
            drawing: false,
            pointer: false,
            cached_scene: Arc::new(Scene::default()),
            session: Arc::new(()),
            input_epoch: 0,
            next_property: 0,
            property: None,
        }
    }
    pub(crate) fn drawing(&self) -> bool {
        self.drawing
    }
    pub(crate) fn tool(&self) -> LiveTool {
        match self.editor.tool {
            Tool::Edit => LiveTool::Select,
            Tool::Arrow => LiveTool::Arrow,
            Tool::Pen => LiveTool::Pen,
            Tool::Highlighter => LiveTool::Highlight,
            _ => LiveTool::Rectangle,
        }
    }
    pub(crate) fn has_annotations(&self) -> bool {
        !self.editor.document.items().is_empty()
    }
    pub(crate) fn stamp(&self) -> (u64, u64) {
        self.editor.scene_stamp()
    }
    pub(crate) fn selected_bounds(&self) -> Option<crate::annotation::Bounds> {
        self.editor.selection_bounds()
    }
    pub(crate) fn set_drawing(&mut self, enabled: bool) {
        self.invalidate_properties();
        if !enabled {
            self.cancel();
        }
        self.drawing = enabled;
    }
    pub(crate) fn select(&mut self, tool: LiveTool) {
        self.cancel();
        self.editor.select_tool(tool.tool());
        if tool == LiveTool::Highlight {
            self.editor.style.highlighter_blend = HighlighterBlend::Alpha;
        }
        self.publish();
    }
    pub(crate) fn down(&mut self, global: Point, tolerance: f32, shift: bool) -> bool {
        if !self.drawing
            || self.pointer
            || self.property.is_some()
            || !tolerance.is_finite()
            || tolerance <= 0.
        {
            return false;
        }
        let Some(p) = self.geometry.local(global, false) else {
            return false;
        };
        self.invalidate_properties();
        // 隐藏时开始新的一笔先恢复显示，避免在看不见的标注上操作。
        self.frames.set_hidden(false);
        self.pointer = true;
        self.editor.pointer_down_at(p, tolerance, shift);
        self.publish();
        true
    }
    pub(crate) fn moved(&mut self, global: Point) -> bool {
        if !self.drawing || !self.pointer {
            return false;
        }
        let Some(p) = self.geometry.local(global, true) else {
            return false;
        };
        self.editor.pointer_move_at(p);
        self.publish();
        true
    }
    pub(crate) fn up(&mut self, global: Point) -> bool {
        if !self.drawing || !self.pointer {
            return false;
        }
        let Some(p) = self.geometry.local(global, true) else {
            self.cancel();
            return false;
        };
        self.pointer = false;
        self.editor.pointer_up_at(p);
        self.publish();
        true
    }
    pub(crate) fn cancel(&mut self) -> bool {
        self.invalidate_properties();
        let had_pointer = self.pointer;
        self.pointer = false;
        let changed = self.editor.cancel_gesture();
        self.publish();
        had_pointer || changed
    }
    pub(crate) fn undo(&mut self) {
        self.cancel();
        self.frames.set_hidden(false);
        self.editor.undo();
        self.publish();
    }
    pub(crate) fn clear(&mut self) {
        self.cancel();
        self.frames.set_hidden(false);
        self.editor.clear();
        self.publish();
    }
    pub(crate) fn redo(&mut self) {
        self.cancel();
        self.frames.set_hidden(false);
        self.editor.redo();
        self.publish();
    }
    pub(crate) fn delete_selected(&mut self) {
        self.cancel();
        self.frames.set_hidden(false);
        self.editor.delete_selected();
        self.publish();
    }
    pub(crate) fn has_selection(&self) -> bool {
        !self.editor.selected_ids().is_empty()
    }
    pub(crate) fn can_undo(&self) -> bool {
        self.editor.document.can_undo()
    }
    pub(crate) fn can_redo(&self) -> bool {
        self.editor.document.can_redo()
    }
    pub(crate) fn properties_open(&self) -> bool {
        self.property.is_some()
    }
    fn invalidate_properties(&mut self) {
        self.property = None;
        self.input_epoch = self.input_epoch.wrapping_add(1);
    }
    /// 样式按钮可用时返回其目标工具与当前样式：选中对象优先，否则为当前绘制工具。
    pub(crate) fn style_target(&self) -> Option<(Tool, Style)> {
        if !self.drawing || self.pointer || self.property.is_some() {
            return None;
        }
        let tool = self
            .editor
            .selected_annotation()
            .map_or(self.editor.tool, |a| a.tool);
        if !matches!(
            tool,
            Tool::Rectangle | Tool::Arrow | Tool::Pen | Tool::Highlighter | Tool::Edit
        ) || (tool == Tool::Edit && !self.has_selection())
        {
            return None;
        }
        let style = self
            .editor
            .selected_annotation()
            .map_or(self.editor.style, |a| a.style);
        Some((tool, style))
    }
    /// 样式弹层点一下即生效：沿用属性票据的校验，选中对象时为一次可撤销修改，无变化不入历史。
    pub(crate) fn apply_style(
        &mut self,
        color: Option<u32>,
        width: Option<f32>,
        opacity: Option<f32>,
    ) -> bool {
        let Some(mut draft) = self.begin_properties() else {
            return false;
        };
        if let Some(color) = color {
            draft.set_color(color);
        }
        if let Some(width) = width {
            let (min, max) = crate::interaction::Parameter::Width
                .limits(draft.tool)
                .unwrap();
            draft.width = width.clamp(min, max);
            draft.dirty.1 = draft.width != draft.before.width;
        }
        if let Some(opacity) = opacity {
            draft.opacity = opacity.clamp(0., 1.);
            draft.dirty.2 = draft.opacity != draft.before.opacity;
        }
        self.frames.set_hidden(false);
        if self.apply_properties(&draft) {
            return true;
        }
        self.cancel_properties(&draft);
        false
    }
    pub(crate) fn begin_properties(&mut self) -> Option<PropertyDraft> {
        let (tool, _) = self.style_target()?;
        self.next_property = self.next_property.checked_add(1)?;
        self.property = Some(self.next_property);
        let style = self.editor.style;
        Some(PropertyDraft {
            session: self.session.clone(),
            request: self.next_property,
            epoch: self.input_epoch,
            stamp: self.stamp(),
            selection: self.editor.selected_ids(),
            primary: self.editor.selected,
            scope: (
                self.editor.group_scope_path(),
                self.editor.group_scope_generation(),
            ),
            before: style,
            active_tool: self.editor.tool,
            dirty: (false, false, false),
            tool,
            color: style.color,
            width: style.width,
            opacity: style.opacity,
        })
    }
    fn owns_property(&self, draft: &PropertyDraft) -> bool {
        Arc::ptr_eq(&self.session, &draft.session) && self.property == Some(draft.request)
    }
    pub(crate) fn properties_current(&self, draft: &PropertyDraft) -> bool {
        self.owns_property(draft)
            && self.drawing
            && !self.pointer
            && self.input_epoch == draft.epoch
            && self.stamp() == draft.stamp
            && self.editor.tool == draft.active_tool
            && self.editor.style == draft.before
            && self.editor.selected_ids() == draft.selection
            && self.editor.selected == draft.primary
            && (
                self.editor.group_scope_path(),
                self.editor.group_scope_generation(),
            ) == draft.scope
    }
    pub(crate) fn cancel_properties(&mut self, draft: &PropertyDraft) -> bool {
        if !self.owns_property(draft) {
            return false;
        }
        self.property = None;
        true
    }
    pub(crate) fn apply_properties(&mut self, draft: &PropertyDraft) -> bool {
        if !self.properties_current(draft) {
            return false;
        }
        let (min, max) = crate::interaction::Parameter::Width
            .limits(draft.tool)
            .unwrap();
        if draft.color > 0xffffff
            || !draft.width.is_finite()
            || draft.width < min.min(draft.before.width)
            || draft.width > max.max(draft.before.width)
            || !draft.opacity.is_finite()
            || !(0. ..=1.).contains(&draft.opacity)
        {
            return false;
        }
        let mut style = draft.before;
        if draft.dirty.0 {
            style.color = draft.color;
        }
        if draft.dirty.1 {
            style.width = draft.width;
        }
        if draft.dirty.2 {
            style.opacity = draft.opacity;
        }
        if draft.tool == Tool::Highlighter {
            style.highlighter_blend = HighlighterBlend::Alpha;
        }
        self.property = None;
        // 真正字段/目标是否变化由Editor判断；取消和无变化不消费Redo。
        if self.editor.set_basic_style_fields(style, draft.dirty) {
            self.publish();
        }
        true
    }
    pub(crate) fn snapshot(&self) -> Option<Arc<FrameScene>> {
        self.frames.snapshot()
    }
    fn publish(&mut self) {
        let annotations = self.editor.annotations();
        let snapshot = if annotations.is_empty() {
            self.cached_scene = Arc::new(Scene::default());
            None
        } else {
            let source = SourcePixels {
                canvas: self.editor.canvas,
                fill: Some([0, 0, 0, 0]),
                planes: vec![],
                overlay: None,
            };
            let scene = self.cached_scene.updated_with_draft(
                &annotations,
                &source,
                self.editor.live_stroke_draft_index(),
            );
            self.cached_scene = Arc::new(scene);
            Some(Arc::new(FrameScene {
                geometry: self.geometry,
                scene: self.cached_scene.clone(),
                stamp: self.editor.scene_stamp(),
            }))
        };
        let present = snapshot.is_some();
        *self.frames.scene.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
        self.frames.present.store(present, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_client_projection_and_session_retirement_keep_roi_and_successor_isolated() {
        assert_eq!(
            Geometry::client_global((-600, -100), Point { x: 16., y: 8. }, 1.25),
            Some(Point { x: -580., y: -90. })
        );
        assert!(Geometry::client_global((-600, -100), Point { x: 16., y: 8. }, f32::NAN).is_none());
        let mut old = session();
        old.set_drawing(true);
        draw(&mut old);
        let pinned = old.snapshot().unwrap();
        old.frames.clear();
        assert!(old.snapshot().is_none());
        let successor = session();
        assert!(successor.snapshot().is_none());
        assert!(!Arc::ptr_eq(&old.frames, &successor.frames));
        assert!(!pinned.scene.layers.is_empty());
    }
    fn session() -> LiveEditor {
        let g = Geometry::new(
            ScreenRect {
                left: -800,
                top: -200,
                width: 1600,
                height: 900,
            },
            ScreenRect {
                left: -600,
                top: -100,
                width: 200,
                height: 120,
            },
        )
        .unwrap();
        LiveEditor::new(g, Arc::new(FrameState::default()))
    }
    #[test]
    fn live_input_phase_gate_only_accepts_explicit_current_active_recording() {
        for phase in [
            super::super::Phase::Starting,
            super::super::Phase::Recording,
            super::super::Phase::Paused,
            super::super::Phase::Finishing,
            super::super::Phase::Converting,
            super::super::Phase::Done,
            super::super::Phase::Failed,
        ] {
            assert!(!accepts_pointer(false, phase, false));
            assert!(!accepts_pointer(true, phase, true));
            assert_eq!(accepts_pointer(true, phase, false), phase.active());
        }
    }
    fn draw(s: &mut LiveEditor) {
        assert!(s.down(Point { x: -580., y: -80. }, 3., false));
        assert!(s.moved(Point { x: -510., y: -40. }));
        assert!(s.up(Point { x: -510., y: -40. }));
    }
    #[test]
    fn inactive_input_and_negative_roi_never_start_history_or_frame_overlay() {
        let mut s = session();
        assert!(!s.down(Point { x: -580., y: -80. }, 3., false));
        assert!(s.snapshot().is_none());
        s.set_drawing(true);
        assert!(!s.down(Point { x: -601., y: -80. }, 3., false));
        assert!(s.editor.document.items().is_empty());
        assert!(!s.editor.document.can_undo());
        draw(&mut s);
        let a = &s.editor.document.items()[0].annotation;
        assert_eq!(a.points[0], Point { x: 220., y: 120. });
        assert_eq!(
            s.snapshot().unwrap().geometry.roi,
            Rect {
                x: 200,
                y: 100,
                width: 200,
                height: 120
            }
        );
    }
    #[test]
    fn each_basic_tool_commits_one_gesture_and_clear_is_reversible() {
        for tool in [
            LiveTool::Rectangle,
            LiveTool::Arrow,
            LiveTool::Pen,
            LiveTool::Highlight,
        ] {
            let mut s = session();
            s.set_drawing(true);
            s.select(tool);
            draw(&mut s);
            assert_eq!(s.editor.document.items().len(), 1);
            assert_eq!(s.editor.document.items()[0].annotation.tool, tool.tool());
            if tool == LiveTool::Highlight {
                assert_eq!(
                    s.editor.document.items()[0]
                        .annotation
                        .style
                        .highlighter_blend,
                    HighlighterBlend::Alpha
                );
            }
            s.undo();
            assert!(s.editor.document.items().is_empty());
            assert!(s.snapshot().is_none());
            s.editor.redo();
            s.publish();
            assert_eq!(s.editor.document.items().len(), 1);
            s.clear();
            assert!(s.editor.document.items().is_empty());
            s.undo();
            assert_eq!(s.editor.document.items().len(), 1);
        }
    }
    #[test]
    fn canceled_and_no_change_selection_keep_redo_and_idle_input_passthrough() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        s.undo();
        let rev = s.editor.document.edit_revision();
        assert!(s.down(Point { x: -580., y: -80. }, 3., false));
        s.moved(Point { x: -570., y: -75. });
        s.set_drawing(false);
        assert!(!s.up(Point { x: -570., y: -75. }));
        assert_eq!(s.editor.document.edit_revision(), rev);
        assert!(s.editor.document.can_redo());
        assert!(s.snapshot().is_none());
        s.set_drawing(true);
        s.select(LiveTool::Select);
        assert!(s.down(Point { x: -580., y: -80. }, 3., false));
        assert!(s.up(Point { x: -580., y: -80. }));
        assert_eq!(s.editor.document.edit_revision(), rev);
        assert!(s.editor.document.can_redo());
    }
    #[test]
    fn pinned_scene_frame_and_preview_share_state_and_empty_restores_identity() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        let frame = s.snapshot().unwrap();
        let preview = s.snapshot().unwrap();
        assert!(Arc::ptr_eq(&frame, &preview));
        let mut image = Image {
            width: 200,
            height: 120,
            bgra: vec![255; 200 * 120 * 4],
            readback_ms: 0.,
        };
        frame.composite(&mut image, s.geometry.region).unwrap();
        assert!(image
            .bgra
            .chunks_exact(4)
            .any(|p| p[..3] != [255, 255, 255]));
        assert_eq!(&image.bgra[..4], &[255, 255, 255, 255]);
        let before = image.bgra.clone();
        assert!(frame
            .composite(
                &mut image,
                ScreenRect {
                    left: 0,
                    ..s.geometry.region
                }
            )
            .is_err());
        assert_eq!(image.bgra, before);
        s.undo();
        assert!(s.snapshot().is_none());
        assert!(!frame.scene.layers.is_empty());
    }
    #[test]
    fn live_edit_keys_map_redo_and_delete_without_modifier_aliases() {
        assert_eq!(
            edit_key("y", true, false, false, false),
            Some(EditKey::Redo)
        );
        assert_eq!(edit_key("z", true, true, false, false), Some(EditKey::Redo));
        assert_eq!(
            edit_key("z", true, false, false, false),
            Some(EditKey::Undo)
        );
        assert_eq!(
            edit_key("delete", false, false, false, false),
            Some(EditKey::Delete)
        );
        assert_eq!(
            edit_key("backspace", false, false, false, false),
            Some(EditKey::Delete)
        );
        for (key, c, s, a, p) in [
            ("y", false, false, false, false),
            ("y", true, true, false, false),
            ("delete", true, false, false, false),
            ("z", true, false, true, false),
            ("z", true, false, false, true),
        ] {
            assert!(edit_key(key, c, s, a, p).is_none());
        }
        assert_eq!(
            edit_key("delete", false, true, false, false),
            Some(EditKey::Clear)
        );
    }
    #[test]
    fn live_redo_delete_and_clear_use_real_incremental_history_and_noop_retains_redo() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        let original = s.editor.document.items().to_vec();
        s.undo();
        s.redo();
        assert_eq!(s.editor.document.items(), original);
        assert!(!s.has_selection()); // Redo clears UI selection.
        s.select(LiveTool::Select);
        assert!(s.down(Point { x: -545., y: -80. }, 3., false));
        assert!(s.up(Point { x: -545., y: -80. }));
        assert!(s.has_selection());
        let revision = s.editor.document.edit_revision();
        s.delete_selected();
        assert_eq!(s.editor.document.edit_revision(), revision + 1);
        assert!(s.editor.document.items().is_empty());
        s.undo();
        assert_eq!(s.editor.document.items(), original);
        s.clear();
        assert!(s.snapshot().is_none());
        s.undo();
        assert_eq!(s.editor.document.items(), original);
        s.clear();
        let revision = s.editor.document.edit_revision();
        s.clear();
        s.delete_selected();
        assert_eq!(s.editor.document.edit_revision(), revision);
        s.undo();
        assert_eq!(s.editor.document.items(), original);
        s.undo();
        assert!(s.can_redo());
        let revision = s.editor.document.edit_revision();
        s.delete_selected();
        s.clear();
        assert_eq!(s.editor.document.edit_revision(), revision);
        assert!(s.can_redo());
    }
    #[test]
    fn live_property_cancel_noop_and_invalid_values_preserve_real_redo_and_pixels() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        s.undo();
        let style = s.editor.style;
        let revision = s.editor.document.edit_revision();
        let mut canceled = s.begin_properties().unwrap();
        canceled.set_color(0x45bb80);
        canceled.step(crate::interaction::Parameter::Width, 3);
        assert!(s.cancel_properties(&canceled));
        assert_eq!(s.editor.style, style);
        assert_eq!(s.editor.document.edit_revision(), revision);
        assert!(s.can_redo());
        let unchanged = s.begin_properties().unwrap();
        assert!(s.apply_properties(&unchanged));
        assert_eq!(s.editor.document.edit_revision(), revision);
        assert!(s.can_redo());
        for (width, opacity, color) in [
            (f32::NAN, 0.5, 0x123456),
            (4., f32::INFINITY, 0x123456),
            (4., 0.5, 0x1000000),
        ] {
            let mut bad = s.begin_properties().unwrap();
            bad.width = width;
            bad.opacity = opacity;
            bad.color = color;
            assert!(!s.apply_properties(&bad));
            assert!(s.properties_current(&bad));
            assert_eq!(s.editor.style, style);
            assert_eq!(s.editor.document.edit_revision(), revision);
            assert!(s.can_redo());
            assert!(s.snapshot().is_none());
            assert!(s.cancel_properties(&bad));
        }
        s.redo();
        assert_eq!(s.editor.document.items().len(), 1);
    }
    #[test]
    fn live_property_selected_fields_commit_once_and_same_primary_color_unifies_targets() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        // Two genuine gestures; give the second its own color before drawing.
        s.select(LiveTool::Rectangle);
        let mut color = s.begin_properties().unwrap();
        color.set_color(0x4f9cff);
        assert!(s.apply_properties(&color));
        assert!(s.down(Point { x: -500., y: -75. }, 3., false));
        assert!(s.up(Point { x: -425., y: -20. }));
        let before = s.editor.document.items().to_vec();
        s.select(LiveTool::Select);
        let ids: Vec<_> = before.iter().map(|o| o.id).collect();
        s.editor.selected = ids.last().copied();
        s.editor.selection = ids.clone();
        s.editor.style = before[1].annotation.style;
        let mut draft = s.begin_properties().unwrap();
        draft.set_color(draft.color);
        let revision = s.editor.document.edit_revision();
        assert!(s.apply_properties(&draft));
        assert_eq!(s.editor.document.edit_revision(), revision + 1);
        for (after, old) in s.editor.document.items().iter().zip(&before) {
            assert_eq!(after.annotation.style.color, draft.color);
            assert_eq!(after.annotation.points, old.annotation.points);
            assert_eq!(after.annotation.style.width, old.annotation.style.width);
            assert_eq!(after.annotation.style.opacity, old.annotation.style.opacity);
        }
        s.undo();
        assert_eq!(s.editor.document.items(), before);
        s.redo();
        assert!(s
            .editor
            .document
            .items()
            .iter()
            .all(|o| o.annotation.style.color == draft.color));
    }
    #[test]
    fn live_property_pending_arrow_changes_draft_and_only_final_gesture_commits() {
        let mut s = session();
        s.set_drawing(true);
        s.select(LiveTool::Arrow);
        let start = Point { x: -580., y: -80. };
        assert!(s.down(start, 3., false));
        assert!(s.up(start));
        assert!(s.editor.has_arrow_draft());
        let revision = s.editor.document.edit_revision();
        let mut draft = s.begin_properties().unwrap();
        draft.set_color(0x45bb80);
        draft.step(crate::interaction::Parameter::Width, 2);
        assert!(!s.down(Point { x: -540., y: -60. }, 3., false));
        assert!(s.apply_properties(&draft));
        assert!(s.editor.has_arrow_draft());
        assert_eq!(s.editor.document.edit_revision(), revision);
        let end = Point { x: -510., y: -40. };
        assert!(s.down(end, 3., false));
        assert!(s.up(end));
        assert_eq!(s.editor.document.edit_revision(), revision + 1);
        let a = &s.editor.document.items()[0].annotation;
        assert_eq!(a.style.color, draft.color);
        assert_eq!(a.style.width, draft.width);
        s.undo();
        assert!(s.editor.document.items().is_empty());
    }
    #[test]
    fn live_property_ticket_rejects_changed_doc_selection_tool_mode_and_successor_without_clearing_new(
    ) {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        let mut old = s.begin_properties().unwrap();
        old.set_color(0x45bb80);
        assert!(s.cancel_properties(&old));
        let current = s.begin_properties().unwrap();
        assert!(!s.apply_properties(&old));
        assert!(!s.cancel_properties(&old));
        assert!(s.properties_current(&current));
        s.cancel_properties(&current);
        let stale_selection = s.begin_properties().unwrap();
        s.editor.selected = None;
        s.editor.selection.clear();
        assert!(!s.apply_properties(&stale_selection));
        assert!(s.cancel_properties(&stale_selection));
        let stale_tool = s.begin_properties().unwrap();
        s.editor.tool = Tool::Pen;
        assert!(!s.apply_properties(&stale_tool));
        assert!(s.cancel_properties(&stale_tool));
        let stale_doc = s.begin_properties().unwrap();
        s.editor.document.clear();
        assert!(!s.apply_properties(&stale_doc));
        assert!(s.cancel_properties(&stale_doc));
        let stale_mode = s.begin_properties().unwrap();
        s.set_drawing(false);
        s.set_drawing(true);
        let newest = s.begin_properties().unwrap();
        assert!(!s.apply_properties(&stale_mode));
        assert!(!s.cancel_properties(&stale_mode));
        assert!(s.properties_current(&newest));
        let mut next = LiveEditor::new(s.geometry, s.frames.clone());
        next.set_drawing(true);
        let next_draft = next.begin_properties().unwrap();
        assert!(!next.apply_properties(&newest));
        assert!(!next.cancel_properties(&newest));
        assert!(next.properties_current(&next_draft));
    }
    #[test]
    fn live_highlight_property_alpha_and_width_survive_tool_switch_and_match_frame_scene() {
        let mut s = session();
        s.set_drawing(true);
        s.select(LiveTool::Highlight);
        let mut draft = s.begin_properties().unwrap();
        draft.set_color(0x4f9cff);
        draft.step(crate::interaction::Parameter::Width, 4);
        draft.step(crate::interaction::Parameter::Opacity, 15);
        assert!(s.apply_properties(&draft));
        s.select(LiveTool::Pen);
        s.select(LiveTool::Highlight);
        assert_eq!(s.editor.style.width, draft.width);
        assert_eq!(s.editor.style.opacity, draft.opacity);
        assert_eq!(s.editor.style.color, draft.color);
        assert_eq!(s.editor.style.highlighter_blend, HighlighterBlend::Alpha);
        draw(&mut s);
        let frame = s.snapshot().unwrap();
        match &frame.scene.layers[0] {
            crate::scene::Layer::Vector { style, .. } => {
                assert_eq!(style.width, draft.width);
                assert_eq!(style.opacity, draft.opacity);
                assert_eq!(style.highlighter_blend, HighlighterBlend::Alpha);
            }
            _ => panic!("录制高亮必须是同份Alpha矢量层"),
        }
        let mut image = Image {
            width: 200,
            height: 120,
            bgra: vec![255; 200 * 120 * 4],
            readback_ms: 0.,
        };
        frame.composite(&mut image, s.geometry.region).unwrap();
        assert!(image
            .bgra
            .chunks_exact(4)
            .any(|p| p[..3] != [255, 255, 255]));
    }
    fn mesh(frame: &FrameScene, index: usize) -> &Arc<crate::vector_mesh::VectorMesh> {
        match &frame.scene.layers[index] {
            crate::scene::Layer::Vector { mesh, .. } => mesh,
            _ => panic!("预期真实矢量层"),
        }
    }
    #[test]
    fn live_scene_reuses_unchanged_mesh_prefix_and_old_frame_pixels_remain_immutable() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        let old = s.snapshot().unwrap();
        let blank = || Image {
            width: 200,
            height: 120,
            bgra: vec![255; 200 * 120 * 4],
            readback_ms: 0.,
        };
        let mut original = blank();
        old.composite(&mut original, s.geometry.region).unwrap();
        s.select(LiveTool::Pen);
        assert!(s.down(Point { x: -480., y: -70. }, 3., false));
        assert!(s.moved(Point { x: -440., y: -35. }));
        let draft_frame = s.snapshot().unwrap();
        assert!(Arc::ptr_eq(mesh(&old, 0), mesh(&draft_frame, 0)));
        assert!(s.up(Point { x: -435., y: -30. }));
        let committed = s.snapshot().unwrap();
        assert!(Arc::ptr_eq(mesh(&old, 0), mesh(&committed, 0)));
        assert!(!Arc::ptr_eq(mesh(&draft_frame, 1), mesh(&committed, 1)));
        s.select(LiveTool::Select);
        let on_pen = Point { x: -457., y: -50. };
        assert!(s.down(on_pen, 3., false));
        assert!(s.up(on_pen));
        assert!(s.has_selection());
        let mut property = s.begin_properties().unwrap();
        property.step(crate::interaction::Parameter::Width, 2);
        assert!(s.apply_properties(&property));
        let changed = s.snapshot().unwrap();
        assert!(Arc::ptr_eq(mesh(&old, 0), mesh(&changed, 0)));
        assert!(!Arc::ptr_eq(mesh(&committed, 1), mesh(&changed, 1)));
        let mut still_old = blank();
        old.composite(&mut still_old, s.geometry.region).unwrap();
        assert_eq!(still_old.bgra, original.bgra);
        s.clear();
        assert!(s.snapshot().is_none());
        assert!(s.cached_scene.layers.is_empty());
        s.undo();
        let restored = s.snapshot().unwrap();
        assert!(!Arc::ptr_eq(mesh(&changed, 0), mesh(&restored, 0)));
        let fresh = session();
        assert!(fresh.cached_scene.layers.is_empty());
        assert!(fresh.snapshot().is_none());
    }
    #[test]
    fn live_basic_style_intents_ignore_other_fields_and_report_target_or_default_change() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        let before = s.editor.document.items()[0].annotation.clone();
        let mut supplied = s.editor.style;
        supplied.color = 0x4f9cff;
        supplied.width = 7.;
        supplied.opacity = 0.75;
        supplied.radius = 40.;
        supplied.effect_radius = 50.;
        supplied.font_size = 62.;
        supplied.filled = true;
        supplied.highlighter_blend = HighlighterBlend::Multiply;
        let revision = s.editor.document.edit_revision();
        assert!(s
            .editor
            .set_basic_style_fields(supplied, (true, true, true)));
        assert_eq!(s.editor.document.edit_revision(), revision + 1);
        let changed = &s.editor.document.items()[0].annotation;
        let mut expected = before.clone();
        expected.style.color = supplied.color;
        expected.style.width = supplied.width;
        expected.style.opacity = supplied.opacity;
        assert_eq!(changed, &expected);
        assert_eq!(s.editor.style.radius, before.style.radius);
        assert_eq!(s.editor.style.filled, before.style.filled);
        let revision = s.editor.document.edit_revision();
        assert!(!s
            .editor
            .set_basic_style_fields(supplied, (true, true, true)));
        assert_eq!(s.editor.document.edit_revision(), revision);
        s.undo();
        assert_eq!(s.editor.document.items()[0].annotation, before);
        assert!(s.can_redo());
        let defaults = s.editor.style;
        let revision = s.editor.document.edit_revision();
        assert!(!s
            .editor
            .set_basic_style_fields(defaults, (false, false, false)));
        assert!(s.can_redo());
        supplied.color = 0xaa77ee;
        assert!(s
            .editor
            .set_basic_style_fields(supplied, (true, false, false)));
        assert_eq!(s.editor.document.edit_revision(), revision);
        assert!(s.can_redo());
        assert_eq!(s.editor.style.color, supplied.color);
        assert_eq!(s.editor.style.width, defaults.width);
    }
    #[test]
    fn live_property_scope_aba_and_same_selection_primary_change_reject_old_source() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        s.select(LiveTool::Rectangle);
        assert!(s.down(Point { x: -500., y: -75. }, 3., false));
        assert!(s.up(Point { x: -425., y: -20. }));
        s.select(LiveTool::Select);
        let ids: Vec<_> = s.editor.document.items().iter().map(|o| o.id).collect();
        s.editor.selection = ids.clone();
        s.editor.selected = Some(ids[0]);
        s.editor.style = s.editor.document.items()[0].annotation.style;
        let primary = s.begin_properties().unwrap();
        s.editor.selected = Some(ids[1]);
        assert!(!s.properties_current(&primary));
        assert!(!s.apply_properties(&primary));
        assert!(s.cancel_properties(&primary));
        s.editor.selected = Some(ids[0]);
        assert!(s.editor.group_selection(true));
        assert!(s.editor.enter_group_scope());
        let source = s.begin_properties().unwrap();
        let revisions = (
            s.editor.document.revision(),
            s.editor.document.edit_revision(),
        );
        let path = s.editor.group_scope_path();
        assert!(s.editor.exit_group_scope());
        assert!(s.editor.enter_group_scope());
        assert_eq!(s.editor.group_scope_path(), path);
        assert_eq!(
            (
                s.editor.document.revision(),
                s.editor.document.edit_revision()
            ),
            revisions
        );
        assert_ne!(s.stamp(), source.stamp);
        assert_ne!(s.editor.group_scope_generation(), source.scope.1);
        assert!(!s.properties_current(&source));
        assert!(!s.apply_properties(&source));
        assert!(s.cancel_properties(&source));
        let current_stamp = s.stamp();
        let fresh = s.begin_properties().unwrap();
        assert!(s.apply_properties(&fresh));
        assert_eq!(
            (
                s.editor.document.revision(),
                s.editor.document.edit_revision()
            ),
            revisions
        );
        assert_eq!(s.stamp(), current_stamp);
    }

    #[test]
    fn live_hidden_frames_skip_overlay_and_new_edits_show_again() {
        let mut s = session();
        s.set_drawing(true);
        draw(&mut s);
        assert!(s.snapshot().is_some());
        s.frames.set_hidden(true);
        assert!(s.snapshot().is_none());
        assert!(s.has_annotations());
        // 撤销、重做、新的一笔都先恢复显示；Document 与历史不受隐藏影响。
        s.undo();
        assert!(!s.frames.hidden());
        s.frames.set_hidden(true);
        s.redo();
        assert!(!s.frames.hidden() && s.snapshot().is_some());
        s.frames.set_hidden(true);
        // 在空白处画第二个矩形（落在第一个矩形边上会变成移动它）。
        assert!(s.down(Point { x: -450., y: -90. }, 3., false));
        assert!(!s.frames.hidden());
        assert!(s.moved(Point { x: -420., y: -60. }));
        assert!(s.up(Point { x: -420., y: -60. }));
        assert_eq!(s.editor.document.items().len(), 2);
        s.frames.set_hidden(true);
        s.frames.clear();
        assert!(!s.frames.hidden());
    }

    #[test]
    fn live_apply_style_commits_once_for_selection_and_noop_keeps_redo() {
        let mut s = session();
        s.set_drawing(true);
        assert_eq!(s.tool(), LiveTool::Rectangle);
        // 未选中时改的是当前工具样式，不写历史。
        assert!(s.apply_style(Some(0x4f9cff), Some(6.), None));
        assert!(!s.can_undo());
        draw(&mut s);
        let a = &s.editor.document.items()[0].annotation;
        assert_eq!((a.style.color, a.style.width), (0x4f9cff, 6.));
        s.select(LiveTool::Select);
        assert_eq!(s.tool(), LiveTool::Select);
        assert!(s.style_target().is_none());
        assert!(s.down(Point { x: -580., y: -80. }, 3., false));
        assert!(s.up(Point { x: -580., y: -80. }));
        assert!(s.has_selection());
        let (tool, style) = s.style_target().unwrap();
        assert_eq!((tool, style.color), (Tool::Rectangle, 0x4f9cff));
        let revision = s.editor.document.edit_revision();
        assert!(s.apply_style(Some(0xff4b5c), None, Some(0.5)));
        assert_ne!(s.editor.document.edit_revision(), revision);
        let a = &s.editor.document.items()[0].annotation;
        assert_eq!((a.style.color, a.style.opacity), (0xff4b5c, 0.5));
        s.undo();
        assert!(s.can_redo());
        // 撤销会清掉选中；重新选中不消费 Redo，同值点击也不消费；宽度按工具上下限夹住。
        assert!(s.down(Point { x: -580., y: -80. }, 3., false));
        assert!(s.up(Point { x: -580., y: -80. }));
        assert!(s.has_selection() && s.can_redo());
        assert!(s.apply_style(None, None, None));
        assert!(s.can_redo());
        assert!(s.apply_style(None, Some(500.), None));
        assert!(s.editor.document.items()[0].annotation.style.width <= 50.);
        assert!(!s.properties_open());
    }
}
