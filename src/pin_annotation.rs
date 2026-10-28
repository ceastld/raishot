//! 贴图直接承载同一个 CaptureView 矢量文档；悬浮栏只转发命令。
use crate::{
    annotation::{HighlighterShape, Style, Tool},
    capture::Image,
    capture_view::{Action, CaptureView},
    document::ObjectId,
    editor::Editor,
    pin_view::PinView,
    toolbar_config::ToolbarConfig,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AppContext, Context, Entity, Window};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};
#[cfg(test)]
#[path = "pin_right_click_tests.rs"]
mod right_click_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RightClickPreparation {
    Pass,
    Cancelled,
    Finished,
    Blocked,
}

/// 贴图右键只收尾绘制手势；空闲、文字和普通菜单不借此提交内容。
fn prepare_editor_right_click(
    editor: &mut Editor,
    editing: bool,
    composing: bool,
) -> RightClickPreparation {
    if !editing {
        return RightClickPreparation::Pass;
    }
    if editor.is_dragging() || editor.has_arrow_draft() {
        editor.cancel_gesture();
        return RightClickPreparation::Cancelled;
    }
    if editor.has_polyline() {
        if composing || !editor.finish_gesture() {
            return RightClickPreparation::Blocked;
        }
        return RightClickPreparation::Finished;
    }
    RightClickPreparation::Pass
}

/// 辅助栏只依赖控件状态；笔迹坐标与 Scene/paint revision 不属于这里。
/// 读取选中对象的元数据，不克隆 Annotation 或扫描图像。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ControlState {
    editor: EditorControlState,
    editing: bool,
    config: ToolbarConfig,
    panel: ToolbarPanel,
    light_theme: bool,
    saving: bool,
    processing: bool,
    color_picker_pending: bool,
    input_composing: bool,
    watermark_field: Option<gpui::EntityId>,
    watermark_binding: Option<crate::watermark_edit::BindingToken>,
    watermark_field_revision: Option<u64>,
    watermark_input_focused: bool,
}
impl ControlState {
    pub(crate) fn capture(v: &CaptureView) -> Self {
        Self {
            editor: EditorControlState::capture(&v.editor),
            editing: v.pin_editing,
            config: v.toolbar_config.clone(),
            panel: v.toolbar_panel,
            light_theme: v.light_theme,
            saving: v.saving,
            processing: v.processing,
            color_picker_pending: v.color_picker_pending,
            input_composing: v.input.marked.is_some(),
            watermark_field: v.watermark_field.as_ref().map(Entity::entity_id),
            watermark_binding: v.watermark_binding,
            watermark_field_revision: v.watermark_field_revision,
            watermark_input_focused: v.watermark_input_focused,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct EditorControlState {
    document_revision: u64,
    edit_revision: u64,
    undo: bool,
    redo: bool,
    tool: Tool,
    style: Style,
    primary: Option<ObjectId>,
    selected_count: usize,
    grouped: bool,
    group_scope: (Vec<u64>, u64),
    selected: Option<(Tool, Style, f32, bool)>,
    active_node: bool,
    radius: Option<(usize, f32, bool, f32)>,
    effect: (Tool, Style),
    effect_brush_mode: bool,
    highlighter_shape: HighlighterShape,
    text_draft: (bool, Option<ObjectId>),
    lasso_draft: bool,
    crop_mask: bool,
    watermark_binding: crate::watermark_edit::BindingToken,
    watermark_preview: bool,
}
impl EditorControlState {
    fn capture(editor: &Editor) -> Self {
        // 空选中是逐笔绘制的常态，避免为它遍历 Document 的分组。
        let ids = if editor.selected.is_none() && editor.selection.len() <= 1 {
            Vec::new()
        } else {
            editor.selected_ids()
        };
        let radius = (ids.len() > 1).then(|| {
            let selection = editor.radius_selection();
            (
                selection.targets.len(),
                selection.value,
                selection.mixed,
                selection.max_radius,
            )
        });
        Self {
            document_revision: editor.document.revision(),
            edit_revision: editor.document.edit_revision(),
            undo: editor.document.can_undo(),
            redo: editor.document.can_redo(),
            tool: editor.tool,
            style: editor.style,
            primary: editor.selected,
            selected_count: ids.len(),
            grouped: ids.iter().any(|id| editor.grouped_in_current_scope(*id)),
            group_scope: (editor.group_scope_path(), editor.group_scope_generation()),
            selected: editor.selected_annotation().map(|a| {
                (
                    a.tool,
                    a.style,
                    a.rotation,
                    a.tool == Tool::Arrow && a.points.len() >= 3,
                )
            }),
            active_node: editor.active_node().is_some(),
            radius,
            effect: editor.effect_context(),
            effect_brush_mode: editor.effect_brush_mode,
            highlighter_shape: editor.highlighter_shape(),
            text_draft: (
                editor.text_draft.is_some(),
                editor.text_draft.as_ref().and_then(|draft| draft.id),
            ),
            lasso_draft: editor.lasso_draft.is_some(),
            crop_mask: editor.document.crop_mask.is_some(),
            watermark_binding: editor.watermark.binding(),
            watermark_preview: editor.watermark.has_preview(),
        }
    }
}

/// 选字标记先准备完整 Scene，成功后调用者才允许一次性提交文档。
pub(crate) fn prepare_text_mark(
    ledger: &crate::ocr_markup::MarkLedger,
    document: &crate::document::Document,
    scene: &crate::scene::Scene,
    source: &crate::scene::SourcePixels<'_>,
    mark: crate::ocr_markup::TextMark,
    quads: &[[crate::annotation::Point; 4]],
    style: Style,
) -> Result<Option<(crate::ocr_markup::MarkEdit, crate::scene::Scene)>> {
    let edit = ledger.prepare(document, mark, quads, style)?;
    if edit.is_empty() {
        return Ok(None);
    }
    let scene = scene.updated(&edit.annotations(document), source);
    if let Some(error) = scene.first_error() {
        return Err(error.to_owned().into());
    }
    Ok(Some((edit, scene)))
}

/// 每张贴图的临时预览开关；跨其画布共享，不进入文档、处理快照或持久化。
#[derive(Clone)]
pub(crate) struct AnnotationPreviewVisibility(Rc<Cell<bool>>);
impl Default for AnnotationPreviewVisibility {
    fn default() -> Self {
        Self(Rc::new(Cell::new(true)))
    }
}
impl AnnotationPreviewVisibility {
    pub(crate) fn visible(&self) -> bool {
        self.0.get()
    }
    pub(crate) fn toggle_after_prepare(&self, prepared: Result<()>) -> Result<bool> {
        prepared?;
        let visible = !self.visible();
        self.0.set(visible);
        Ok(visible)
    }
    pub(crate) fn reveal_for_editing(&self) {
        self.0.set(true);
    }
    pub(crate) fn allows_editing(&self, requested: bool, locked: bool) -> bool {
        requested && !locked && self.visible()
    }
}

// 图像处理前先落下有效草稿，避免首个文字/箭头还没有撤销记录而被误当作空文档。
#[cfg(test)]
pub(crate) fn finish_annotation_content(editor: &mut crate::editor::Editor) -> bool {
    editor.finish_gesture();
    editor.finish_text();
    editor.document.can_undo()
}
#[derive(Clone)]
pub(crate) struct Checkpoint {
    original: Arc<gpui::RenderImage>,
    image: Arc<gpui::RenderImage>,
    original_size: (u32, u32),
    image_size: (u32, u32),
    presentation_style: Option<crate::selection_finish::SelectionFinishStyle>,
    vector_text: Option<bool>,
    adjustments: crate::pin_model::Adjustments,
    history: crate::pin_model::ProcessingHistory,
    canvas: Option<Entity<CaptureView>>,
    capture_pixels: Option<Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
    capture_source: Option<crate::pin_raster_document::capture_source::CaptureSourceMetadata>,
}
impl PinView {
    /// 消费编辑收尾的这一次右键；不认领右键菜单捕获，也不改变焦点。
    pub(crate) fn prepare_canvas_right_click(
        &mut self,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(canvas) = &self.canvas else {
            return false;
        };
        let allowed = !self.state.locked
            && !crate::pin_native::transparent(crate::gpui_capture::hwnd(w))
            && !self.saving
            && !self.processing
            && self.annotation_preview.visible();
        let (handled, changed) = canvas.update(cx, |view, cx| {
            let editing = allowed
                && view.pin_mode
                && view.pin_editing
                && !view.saving
                && !view.processing
                && !view.pin_handoff_pending
                && !view.color_picker_pending
                && !view.watermark_input_focused;
            let before = view.editor.document.edit_revision();
            let composing = view.text_composing() || view.input.marked.is_some();
            let prepared = prepare_editor_right_click(&mut view.editor, editing, composing);
            match prepared {
                RightClickPreparation::Pass => return (false, false),
                RightClickPreparation::Blocked => {
                    view.status = tr!(
                        "请先完成当前编辑或输入法组合文字",
                        "Finish the current edit or IME composition first"
                    )
                    .into();
                }
                RightClickPreparation::Cancelled | RightClickPreparation::Finished => {
                    view.wheel_input.reset();
                    view.clear_pointer_tracking();
                    if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() }
                        == crate::gpui_capture::hwnd(w)
                    {
                        view.release_capture();
                    }
                    if prepared == RightClickPreparation::Finished {
                        view.reset_input();
                    }
                    // 预览和导出在下次读取时都按新的文档／草稿版本准备 Scene。
                    view.scene_stamp = None;
                }
            }
            cx.notify();
            (true, before != view.editor.document.edit_revision())
        });
        if changed {
            self.redo_edits.clear();
        }
        handled
    }

    pub(crate) fn sample_content_patch(
        &mut self,
        viewport: crate::viewport::Viewport,
        pointer: crate::annotation::Point,
        cx: &mut Context<Self>,
    ) -> Result<Option<crate::pin_color_sample::ColorPatch>> {
        let presentation_style = self.presentation_style;
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |view, _| {
                // Pin 的原CPU图像与完整Scene；不使用会提交草稿的用户导出快照。
                if !view.source.is_image() || view.saving || view.processing {
                    return Err("贴图内容暂不可取色".into());
                }
                let scene = view.prepared_scene();
                let source = view.source_pixels();
                if let Some(style) = presentation_style {
                    let content = view.pin_content_rect.unwrap_or(source.canvas);
                    let origin = viewport.window_point(crate::annotation::Point {
                        x: content.x as f32,
                        y: content.y as f32,
                    });
                    let plan = crate::pin_presentation::PresentationPreviewPlan::new(
                        content,
                        style,
                        crate::pin_presentation::PhysicalPoint {
                            x: f64::from(origin.x),
                            y: f64::from(origin.y),
                        },
                        f64::from(viewport.zoom),
                    )?;
                    crate::pin_color_sample::sample_presentation_patch(
                        &source,
                        Some(&scene),
                        plan,
                        viewport,
                        pointer,
                    )
                } else {
                    crate::pin_color_sample::sample_patch(&source, Some(&scene), viewport, pointer)
                }
            })
        } else {
            let rect = crate::geometry::Rect::full(self.image_size.0, self.image_size.1);
            let fill = self.vector_text_fill();
            let source = crate::scene::SourcePixels {
                fill,
                canvas: rect,
                planes: if fill.is_some() {
                    Vec::new()
                } else {
                    let pixels = self.image.as_bytes(0).ok_or("贴图原始像素不可用")?;
                    vec![crate::scene::PixelPlane { rect, bgra: pixels }]
                },
                overlay: None,
            };
            if let Some(style) = presentation_style {
                let plan = crate::pin_presentation::PresentationPreviewPlan::new(
                    rect,
                    style,
                    crate::pin_presentation::PhysicalPoint {
                        x: f64::from(viewport.offset.x),
                        y: f64::from(viewport.offset.y),
                    },
                    f64::from(viewport.zoom),
                )?;
                crate::pin_color_sample::sample_presentation_patch(
                    &source, None, plan, viewport, pointer,
                )
            } else {
                crate::pin_color_sample::sample_patch(&source, None, viewport, pointer)
            }
        }
    }
    pub(crate) fn reconcile_preview_textures(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let (current, retired) = if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, _| {
                v.sync_viewport(w);
                let region = shared_preview_region(v, w);
                let density = v.canvas_viewport(w).zoom;
                match v.source.refresh_shared_preview_at(region, density) {
                    Ok(Some(preview)) => {
                        v.retired_raster_previews
                            .extend(std::mem::replace(&mut v.previews, vec![preview]));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        v.status = trf!(
                            "更新长图显示预览失败：{error}",
                            "Failed to update the long image preview: {error}"
                        )
                    }
                }
                // nested render 也会准备 Scene；先同步一次即可按这一帧真实效果收集候选。
                // 旧 canvas 离场时不一定再 render，退休不能只依赖它自己的队列。
                let scene = v.prepared_scene();
                let annotations_visible = !v.pin_mode
                    || v.pin_annotation_preview
                        .as_ref()
                        .is_none_or(|preview| preview.visible());
                if let Err(error) =
                    v.prepare_pin_presentation_preview(w, &scene, annotations_visible)
                {
                    // prepare 失败保留完整旧帧，当前缓存引用的 active 键继续有效。
                    v.status = trf!(
                        "更新贴图圆角预览失败：{error}",
                        "Failed to update the pin corner preview: {error}"
                    );
                }
                // 首次上传前就交给宿主追踪；封存后的旧画布不会再负责退休纹理。
                if let Err(error) = v.prepare_watermark_previews(w, &scene) {
                    v.status = trf!(
                        "更新水印预览失败：{error}",
                        "Failed to update the watermark preview: {error}"
                    );
                }
                if let Err(error) = v.prepare_text_previews(w, &scene) {
                    v.status = trf!(
                        "更新文字预览失败：{error}",
                        "Failed to update the text preview: {error}"
                    );
                }
                let current = v
                    .previews
                    .iter()
                    .cloned()
                    .chain(v.raster_previews.iter().map(|(_, preview)| preview.clone()))
                    .chain(
                        v.watermark_previews
                            .iter()
                            .map(|preview| preview.image.clone()),
                    )
                    .chain(v.text_previews.iter().map(|preview| preview.image.clone()))
                    .chain(
                        v.pin_presentation_preview
                            .body_tiles()
                            .iter()
                            .map(|tile| tile.image.clone()),
                    )
                    .chain(
                        v.pin_presentation_preview
                            .raster_previews()
                            .iter()
                            .map(|(_, image)| image.clone()),
                    )
                    .collect();
                // 失败回退仍使用上一帧实际 raster map；保留其退休日志和强引用。
                let (retained, retired): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut v.retired_raster_previews)
                        .into_iter()
                        .partition(|image| {
                            v.pin_presentation_preview
                                .raster_previews()
                                .iter()
                                .any(|(_, active)| active.id == image.id)
                        });
                v.retired_raster_previews = retained;
                (current, retired)
            })
        } else if self.vector_text.is_some()
            || self.image_size.0 > super::pin_view::MAX_PIN_WINDOW_SIDE
            || self.image_size.1 > super::pin_view::MAX_PIN_WINDOW_SIDE
        {
            // 后台处理期间超长图显示空白占位，不把未上传的整帧算作显示候选。
            (Vec::new(), Vec::new())
        } else {
            (vec![self.image.clone()], Vec::new())
        };
        if let Some(error) = self.preview_lifecycle.render(current, retired, w) {
            self.status = error;
        }
    }

    pub(crate) fn preview_texture_lifecycle(&self) -> (usize, usize, u64, u64) {
        self.preview_lifecycle.counts()
    }

    pub(crate) fn preview_texture_ids_for_qa(&self) -> Vec<usize> {
        self.preview_lifecycle.current_image_ids()
    }

    pub(crate) fn ensure_canvas(&mut self, w: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        self.ensure_document_canvas(w, cx)?;
        self.set_canvas_active(true, w, cx)?;
        Ok(())
    }
    /// 原位文字标记只需要文档，不能借创建画布激活工具或改变输入焦点。
    pub(crate) fn ensure_document_canvas(
        &mut self,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.require_processing_image_available()?;
        if self.canvas.is_none() {
            let (source, previews) = if let (Some(pixels), Some(metadata)) =
                (&self.capture_pixels, &self.capture_source)
            {
                crate::canvas_source::CanvasSource::capture_context(
                    pixels.clone(),
                    metadata.content_rect(),
                )?
            } else {
                match self.vector_text_fill() {
                    // 矢量文字贴图：纯色底不生成位图或预览纹理。
                    Some(fill) => (
                        crate::canvas_source::CanvasSource::solid(
                            self.image_size.0,
                            self.image_size.1,
                            fill,
                        )?,
                        Vec::new(),
                    ),
                    None if self.presentation_style.is_some() => {
                        crate::canvas_source::CanvasSource::shared_presentation_image(
                            self.image.clone(),
                            self.image_size,
                        )?
                    }
                    None => crate::canvas_source::CanvasSource::shared_pin_image(
                        self.image.clone(),
                        self.image_size,
                    )?,
                }
            };
            let full = source.full();
            let content = self
                .capture_source
                .as_ref()
                .map(|source| source.content_rect());
            let cursor = self
                .capture_source
                .as_ref()
                .and_then(|source| source.cursor_model());
            let focus = cx.focus_handle();
            let owner = cx.entity().downgrade();
            let pin_window = w.window_handle().downcast::<PinView>();
            let canvas = cx.new(|_| {
                let mut v = CaptureView::new(
                    source,
                    previews,
                    focus,
                    Some(content.unwrap_or(full)),
                    None,
                    cursor,
                    Vec::new(),
                );
                v.pin_mode = true;
                v.pin_presentation_style = self.presentation_style;
                v.pin_window_sizing = self.window_sizing;
                v.pin_content_rect = content;
                v.pin_annotation_preview = Some(self.annotation_preview.clone());
                v.reset_finish_for_image();
                v.pin_editing = false;
                v.magnifier = false;
                v.guides = false;
                v.show_guides = false;
                v.include_cursor = v.cursor.is_some() && content.is_some();
                v.image_canvas = Some(crate::image_editor::ImageCanvas {
                    path: None,
                    viewport: Default::default(),
                    auto_fit: false,
                    host: (0, 0),
                    pan: None,
                });
                v.pin_owner = Some(owner);
                v.pin_window = pin_window;
                v.editor.select_tool(crate::annotation::Tool::Pen);
                v
            });
            self.canvas_subscription = Some(Self::observe_canvas(&canvas, cx));
            self.canvas = Some(canvas);
        }
        Ok(())
    }
    fn observe_canvas(canvas: &Entity<CaptureView>, cx: &mut Context<Self>) -> gpui::Subscription {
        // 编辑提交与撤销/重做分开计数；选中、换工具和绘制预览不会破坏跨图像处理边界的重做。
        let revision = Cell::new(canvas.read(cx).editor.document.edit_revision());
        let controls = RefCell::new(ControlState::capture(canvas.read(cx)));
        cx.observe(canvas, move |v, canvas, cx| {
            let canvas = canvas.read(cx);
            let next = canvas.editor.document.edit_revision();
            if revision.replace(next) != next {
                v.redo_edits.clear();
            }
            let next_controls = ControlState::capture(canvas);
            let mut previous = controls.borrow_mut();
            if *previous != next_controls {
                *previous = next_controls;
                if let Some(bar) = v.toolbar {
                    cx.defer(move |cx| {
                        let _ = bar.update(cx, |_, _, cx| cx.notify());
                    });
                }
            }
            // 宿主仍逐事件重绘画布并收集预览纹理退休候选。
            cx.notify();
        })
    }
    pub(crate) fn set_canvas_active(
        &mut self,
        on: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if on && self.presentation_geometry_adjusting {
            return Err(tr!(
                "正在调整贴图尺寸以适应预览范围…",
                "Resizing pin to fit the preview area…"
            )
            .into());
        }
        if let Some(c) = &self.canvas {
            c.update(cx, |v, cx| -> Result<()> {
                let editing = self
                    .annotation_preview
                    .allows_editing(on, self.state.locked);
                if !editing {
                    v.try_finish_content()?;
                    v.pointer_down_start = None;
                    v.input.drag_anchor = None;
                    v.release_capture();
                }
                v.pin_editing = editing;
                if v.pin_editing {
                    w.focus(&v.focus);
                }
                cx.notify();
                Ok(())
            })?;
        }
        if !on {
            w.focus(&self.focus);
        }
        Ok(())
    }
    pub(crate) fn toggle_annotation_preview(
        &mut self,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if self.processing || self.drag_request.is_some() || self.gesture.is_some() {
            return Err("请先完成图像处理或贴图拖动".into());
        }
        let prepared = if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, cx| -> Result<()> {
                if v.saving || v.processing || v.watermark_input_focused {
                    return Err("请先完成当前编辑或文字输入".into());
                }
                v.try_finish_content()?;
                // 收尾成功后才停用对象命中；失败保留原手势、组合串和预览状态。
                v.clear_pointer_tracking();
                v.release_capture();
                v.pin_editing = false;
                v.toolbar_panel = crate::toolbar_layout::ToolbarPanel::None;
                cx.notify();
                Ok(())
            })
        } else {
            Ok(())
        };
        let visible = self.annotation_preview.toggle_after_prepare(prepared)?;
        w.focus(&self.focus);
        self.status = if visible {
            tr!("已显示可编辑标注", "Editable annotations shown")
        } else {
            tr!(
                "已隐藏可编辑标注（仅预览，复制与保存仍包含标注）",
                "Editable annotations hidden (preview only; copy and save still include them)"
            )
        }
        .into();
        Ok(())
    }
    /// 关闭、收栏、切换处理模式在修改宿主状态前通过同一准备门。
    pub(crate) fn finish_canvas_content(&mut self, cx: &mut App) -> Result<bool> {
        let Some(canvas) = &self.canvas else {
            return Ok(false);
        };
        canvas.update(cx, |v, cx| -> Result<bool> {
            let before = v.editor.document.edit_revision();
            v.try_finish_content()?;
            let changed = before != v.editor.document.edit_revision();
            if changed {
                cx.notify();
            }
            Ok(changed)
        })
    }
    pub(crate) fn prepare_canvas_local_fill(&mut self, cx: &mut App) -> Result<()> {
        if self.presentation_geometry_adjusting {
            return Err(tr!(
                "正在调整贴图尺寸以适应预览范围…",
                "Resizing pin to fit the preview area…"
            )
            .into());
        }
        let Some(canvas) = &self.canvas else {
            return Ok(());
        };
        canvas.update(cx, |v, cx| {
            let before = v.editor.document.edit_revision();
            let result = v.try_finish_local_fill();
            if let Err(error) = &result {
                v.status = error.to_string();
            }
            if result.is_err() || before != v.editor.document.edit_revision() {
                cx.notify();
            }
            result
        })
    }
    pub(crate) fn annotation(&mut self, a: Action, w: &mut Window, cx: &mut Context<Self>) {
        if self.presentation_geometry_adjusting || self.state.locked {
            return;
        }
        let prepared = if matches!(
            a,
            Action::Undo
                | Action::Redo
                | Action::Cancel
                | Action::Clear
                | Action::EnterGroup
                | Action::ExitGroup
                | Action::RootGroup
        ) {
            Ok(())
        } else {
            self.prepare_canvas_local_fill(cx)
        };
        if let Err(error) = prepared {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        if matches!(a, Action::Tool(_)) {
            self.text_mode = false;
            self.end_color_pick(cx);
            self.clear_text_selection();
        }
        if let Some(c) = &self.canvas {
            let changed = c.update(cx, |v, cx| {
                let before = v.editor.document.edit_revision();
                v.perform(a, w, cx);
                w.focus(&v.focus);
                before != v.editor.document.edit_revision()
            });
            if changed {
                self.redo_edits.clear();
            }
        }
        self.sync_toolbar(w, cx);
        cx.notify();
    }
    pub(crate) fn output(&mut self, cx: &mut App) -> Result<Image> {
        // 内部封存、处理、复制文档与持久化只渲染内容；截图外观由用户输出
        // UserExportSnapshot 在目标栅格合成，不能写回底图造成二次烘焙。
        self.require_processing_image_available()?;
        match &self.canvas {
            Some(c) => {
                let (image, changed) = c.update(cx, |v, cx| {
                    let before = v.editor.document.edit_revision();
                    if let Err(error) = v.try_finish_content() {
                        v.status = error.to_string();
                        cx.notify();
                        return (Err(error), false);
                    }
                    let changed = before != v.editor.document.edit_revision();
                    if changed {
                        cx.notify();
                    }
                    (v.selected_image(), changed)
                });
                if changed {
                    self.redo_edits.clear();
                }
                image
            }
            None => Ok(self.pixels()),
        }
    }
    pub(crate) fn snapshot(&self) -> Checkpoint {
        Checkpoint {
            original: self.original.clone(),
            image: self.image.clone(),
            original_size: self.original_size,
            image_size: self.image_size,
            presentation_style: self.presentation_style,
            vector_text: self.vector_text,
            adjustments: self.state.adjustments,
            history: self.history.clone(),
            canvas: self.canvas.clone(),
            capture_pixels: self.capture_pixels.clone(),
            capture_source: self.capture_source.clone(),
        }
    }
    pub(crate) fn restore_checkpoint(&mut self, s: Checkpoint, cx: &mut Context<Self>) {
        self.original = s.original;
        self.image = s.image;
        self.original_size = s.original_size;
        self.image_size = s.image_size;
        self.presentation_style = s.presentation_style;
        // 恢复的画布按其保存时的主题记录，下一帧 sync_text_theme 再映射到当前主题。
        self.vector_text = s.vector_text;
        self.state.adjustments = s.adjustments;
        self.history = s.history;
        self.canvas = s.canvas;
        self.capture_pixels = s.capture_pixels;
        self.capture_source = s.capture_source;
        self.canvas_subscription = self
            .canvas
            .as_ref()
            .map(|canvas| Self::observe_canvas(canvas, cx));
    }
    pub(crate) fn seal_annotations(
        &mut self,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let Some(c) = &self.canvas else {
            return Ok(());
        };
        let changed = c.update(cx, |v, cx| -> Result<bool> {
            v.try_finish_content()?;
            // 矢量文字贴图没有底图位图，图像处理前总要先渲染为普通贴图。
            let changed = v.editor.document.can_undo()
                || self.vector_text.is_some()
                || self.capture_source.is_some();
            v.pointer_down_start = None;
            v.input.drag_anchor = None;
            v.release_capture();
            cx.notify();
            Ok(changed)
        })?;
        if changed {
            let snapshot = self.snapshot();
            let image = self.output(cx)?;
            let dimensions = (image.width, image.height);
            let preview = super::pin_view::preview(image)?;
            // 撤销保留文档和原图即可，显示水印可按恢复后的视口重新生成。
            // 当前 GPU 键已由 PreviewLifecycle 持有，下一帧随旧画布离场退休。
            if let Some(canvas) = &self.canvas {
                canvas.update(cx, |v, cx| {
                    v.watermark_previews.clear();
                    cx.notify();
                });
            }
            self.undo_edits.push(snapshot);
            // 边界快照共享原图，按保守像素成本限制历史；至少保留最近的一次。
            while self.undo_edits.len() > 1
                && (self.undo_edits.len() > 8
                    || self
                        .undo_edits
                        .iter()
                        .map(|s| s.original_size.0 as u64 * s.original_size.1 as u64 * 16)
                        .sum::<u64>()
                        > 128 * 1024 * 1024)
            {
                self.undo_edits.remove(0);
            }
            self.original = preview.clone();
            self.image = preview;
            self.original_size = dimensions;
            self.image_size = dimensions;
            self.vector_text = None;
            self.capture_pixels = None;
            self.capture_source = None;
            self.state.adjustments = Default::default();
            self.history = Default::default();
        }
        self.canvas = None;
        self.canvas_subscription = None;
        self.sync_presentation_geometry(w, cx)?;
        Ok(())
    }
    /// 工具栏撤销/重做是否有可执行的历史（标注、图像处理或跨处理边界检查点）。
    pub(crate) fn history_available(&self, redo: bool, cx: &App) -> bool {
        let canvas = self.canvas.as_ref().is_some_and(|c| {
            let document = &c.read(cx).editor.document;
            if redo {
                document.can_redo()
            } else {
                document.can_undo()
            }
        });
        canvas
            || if redo {
                self.history.can_redo() || !self.redo_edits.is_empty()
            } else {
                self.history.can_undo() || !self.undo_edits.is_empty()
            }
    }
    pub(crate) fn annotation_history(
        &mut self,
        redo: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(c) = &self.canvas {
            let possible = if redo {
                c.read(cx).editor.document.can_redo()
            } else {
                c.read(cx).editor.document.can_undo()
            };
            if possible {
                let text_source = self.text_mark_history_source(cx);
                c.update(cx, |v, cx| {
                    if redo {
                        v.editor.redo();
                    } else {
                        v.editor.undo();
                    }
                    cx.notify();
                });
                self.rebase_text_marks_after_history(text_source, cx);
                return true;
            }
        }
        let mut next = self.state.adjustments;
        let mut history = self.history.clone();
        if if redo {
            history.redo(&mut next)
        } else {
            history.undo(&mut next)
        } {
            return false;
        }
        let candidate = if redo {
            self.redo_edits.last()
        } else {
            self.undo_edits.last()
        };
        if let Some(candidate) = candidate {
            // 完整未来窗口先验预算；拒绝不能消费检查点或改变原图/文档。
            if let Err(error) = self.validate_presentation_checkpoint(
                candidate
                    .capture_source
                    .as_ref()
                    .map_or(candidate.image_size, |source| source.storage_size()),
                candidate.presentation_style,
                w,
            ) {
                self.status = error.to_string();
                cx.notify();
                return true;
            }
            let snapshot = if redo {
                self.redo_edits.pop()
            } else {
                self.undo_edits.pop()
            }
            .expect("已只读核验检查点");
            let current = self.snapshot();
            let previous_zoom = self.state.zoom;
            self.restore_checkpoint(snapshot.clone(), cx);
            if let Err(error) = self.sync_presentation_geometry(w, cx) {
                self.restore_checkpoint(current, cx);
                self.state.zoom = previous_zoom;
                if redo {
                    self.redo_edits.push(snapshot);
                } else {
                    self.undo_edits.push(snapshot);
                }
                self.status = error.to_string();
                cx.notify();
                return true;
            }
            if redo {
                self.undo_edits.push(current);
            } else {
                self.redo_edits.push(current);
            }
            if let Err(error) = self.set_canvas_active(self.chrome, w, cx) {
                self.status = error.to_string();
            }
            // 快照是图像处理边界，同一次撤销继续到真正改变画面的命令。
            self.perform(
                if redo {
                    crate::pin_view::PinAction::Redo
                } else {
                    crate::pin_view::PinAction::Undo
                },
                w,
                cx,
            );
            true
        } else {
            self.canvas.is_some()
        }
    }
}

/// 只投影屏幕可见的原图范围。Windows 全局坐标只用于几何，不读取桌面像素。
pub(crate) fn shared_preview_region(v: &CaptureView, w: &Window) -> crate::geometry::Rect {
    let viewport = v.canvas_viewport(w);
    let placement = match v.pin_render_placement {
        Some(placement) => placement,
        None => match super::pin_native::rect(crate::gpui_capture::hwnd(w)) {
            Ok(placement) => placement,
            Err(_) => return v.source.full(),
        },
    };
    shared_preview_region_at(v, viewport, placement)
}

/// 与实际帧同一公式；缩放定位前用目标窗口矩形与视口预先计算。
pub(crate) fn shared_preview_region_at(
    v: &CaptureView,
    viewport: crate::viewport::Viewport,
    placement: crate::pin_model::Placement,
) -> crate::geometry::Rect {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let full = v.source.full();
    let (x, y, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return full;
    };
    crate::pin_presentation::preview_visible_source_region(
        full,
        viewport,
        placement,
        crate::pin_model::Placement {
            x,
            y,
            width,
            height,
        },
    )
    .unwrap_or(full)
}

#[cfg(test)]
mod control_state_tests {
    use super::{ControlState, EditorControlState};
    use crate::{
        annotation::{Point, Tool},
        editor::Editor,
        geometry::Rect,
        toolbar_config::ToolbarConfig,
        toolbar_layout::ToolbarPanel,
    };

    fn editor() -> Editor {
        let full = Rect::full(600, 400);
        Editor::new(full, Some(full))
    }
    fn controls(editor: &Editor) -> ControlState {
        ControlState {
            editor: EditorControlState::capture(editor),
            editing: true,
            config: ToolbarConfig::default(),
            panel: ToolbarPanel::None,
            light_theme: true,
            saving: false,
            processing: false,
            color_picker_pending: false,
            input_composing: false,
            watermark_field: None,
            watermark_binding: None,
            watermark_field_revision: None,
            watermark_input_focused: false,
        }
    }
    fn rectangle(editor: &mut Editor, x: i32) {
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((x, 40), 4.);
        editor.pointer_up((x + 100, 140));
    }

    #[test]
    fn pin_control_stamp_ignores_freehand_points_until_commit() {
        for tool in [Tool::Pen, Tool::Highlighter, Tool::PixelEraser] {
            let mut editor = editor();
            editor.select_tool(tool);
            editor.pointer_down((30, 40), 4.);
            let stamp = EditorControlState::capture(&editor);
            let scene = editor.scene_stamp();
            for step in 1..64 {
                editor.pointer_move((30 + step, 40 + step % 7));
                assert_eq!(EditorControlState::capture(&editor), stamp, "{tool:?}");
            }
            assert_ne!(editor.scene_stamp(), scene);
            editor.pointer_up((94, 40));
            assert!(editor.document.can_undo());
            assert_ne!(EditorControlState::capture(&editor), stamp);
        }
    }

    #[test]
    fn pin_control_stamp_keeps_live_rotation_and_style_without_move_noise() {
        let mut editor = editor();
        rectangle(&mut editor, 80);
        editor.select_tool(Tool::Edit);
        editor.pointer_down((130, 90), 4.);
        editor.pointer_up((130, 90));
        editor.pointer_down((130, 90), 4.);
        let stamp = EditorControlState::capture(&editor);
        let revision = editor.document.revision();
        editor.pointer_move((150, 110));
        assert_eq!(EditorControlState::capture(&editor), stamp);
        editor.set_width(9.);
        assert_eq!(editor.document.revision(), revision);
        assert_ne!(EditorControlState::capture(&editor), stamp);
        assert!(editor.cancel_gesture());

        let handle = editor.rotation_control(4.).unwrap();
        editor.pointer_down_at(handle.position, 4., false);
        let stamp = EditorControlState::capture(&editor);
        let distance = handle.position.distance(handle.pivot);
        editor.pointer_move_at(Point {
            x: handle.pivot.x + distance,
            y: handle.pivot.y,
        });
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.selected_annotation().unwrap().rotation.abs() > 1.);
        assert_ne!(EditorControlState::capture(&editor), stamp);
        assert!(editor.cancel_gesture());
        assert_eq!(editor.document.revision(), revision);
    }

    #[test]
    fn pin_control_stamp_tracks_primary_grouping_and_history_navigation() {
        let mut editor = editor();
        for x in [30, 210, 390] {
            rectangle(&mut editor, x);
        }
        editor.select_tool(Tool::Edit);
        editor.pointer_down((60, 80), 4.);
        editor.pointer_up((60, 80));
        let first = EditorControlState::capture(&editor);
        editor.pointer_down((240, 80), 4.);
        editor.pointer_up((240, 80));
        let second = EditorControlState::capture(&editor);
        assert_eq!(first.selected_count, second.selected_count);
        assert_ne!(first.primary, second.primary);
        assert_ne!(first, second);
        editor.select_all_objects();
        let selection = EditorControlState::capture(&editor);
        assert_eq!(selection.selected_count, 3);
        assert!(!selection.grouped);
        assert!(editor.group_selection(true));
        let grouped = EditorControlState::capture(&editor);
        assert!(grouped.grouped);
        assert_ne!(grouped, selection);
        editor.undo();
        let undone = EditorControlState::capture(&editor);
        assert!(undone.undo && undone.redo);
        editor.undo();
        let twice = EditorControlState::capture(&editor);
        assert!(twice.undo && twice.redo);
        assert_eq!(twice.edit_revision, undone.edit_revision);
        assert_ne!(twice, undone);
        editor.redo();
        assert_ne!(EditorControlState::capture(&editor), twice);
    }

    #[test]
    fn pin_control_stamp_tracks_tool_textdraft_effect_and_watermark_input() {
        let mut editor = editor();
        let initial = EditorControlState::capture(&editor);
        editor.select_tool(Tool::Text);
        assert_ne!(EditorControlState::capture(&editor), initial);
        let tool = EditorControlState::capture(&editor);
        editor.begin_text(None, Point { x: 30., y: 40. });
        let draft = EditorControlState::capture(&editor);
        assert_ne!(draft, tool);
        // 正文与 IME 像素由宿主/输入实体绘制，参数栏只依赖草稿资格和样式。
        editor.set_text("文字预览".into());
        assert_eq!(EditorControlState::capture(&editor), draft);
        editor.select_tool(Tool::SmartMosaic);
        let smart = EditorControlState::capture(&editor);
        editor.set_smart_effect_context(Tool::Blur, editor.style);
        assert_ne!(EditorControlState::capture(&editor), smart);

        editor.select_tool(Tool::Watermark);
        let binding = editor.watermark.binding();
        let watermark = EditorControlState::capture(&editor);
        editor.preview_watermark_text(binding, "组合", true);
        let composing = EditorControlState::capture(&editor);
        assert_ne!(composing, watermark);
        editor.preview_watermark_text(binding, "组合文字", true);
        assert_eq!(EditorControlState::capture(&editor), composing);
        assert!(editor.set_watermark_text(binding, "水印"));
        assert_ne!(EditorControlState::capture(&editor), composing);
    }

    #[test]
    fn pin_control_stamp_tracks_editing_config_panel_and_input_controls() {
        let baseline = controls(&editor());
        let changes: [fn(&mut ControlState); 11] = [
            |s| s.editing = false,
            |s| s.config.tools.rotate_left(1),
            |s| s.config.grouped = !s.config.grouped,
            |s| s.panel = ToolbarPanel::Width,
            |s| s.light_theme = false,
            |s| s.saving = true,
            |s| s.processing = true,
            |s| s.color_picker_pending = true,
            |s| s.input_composing = true,
            |s| s.watermark_field_revision = Some(1),
            |s| s.watermark_input_focused = true,
        ];
        for change in changes {
            let mut changed = baseline.clone();
            change(&mut changed);
            assert_ne!(changed, baseline);
        }
        assert_eq!(baseline.clone(), baseline);
    }

    #[test]
    fn pin_control_stamp_tracks_scope_identity_when_other_controls_and_document_are_unchanged() {
        let mut editor = editor();
        for x in [30, 160, 290, 420] {
            rectangle(&mut editor, x);
        }
        let ids: Vec<_> = editor.document.items().iter().map(|o| o.id).collect();
        assert!(editor.document.set_group(&ids[..2], true));
        assert!(editor.document.set_group(&ids[2..], true));
        assert!(editor.document.set_group(&[ids[0], ids[2]], true));
        editor.document.remove_many(&ids[1..]);
        editor.select_tool(Tool::Edit);
        editor.selected = Some(ids[0]);
        let snapshot = editor.document.bounded_history_snapshot().unwrap();
        let before = EditorControlState::capture(&editor);
        assert!(before.grouped);
        assert_eq!(before.selected_count, 1);
        assert!(editor.enter_group_scope());
        let inside = EditorControlState::capture(&editor);
        assert_eq!(inside.document_revision, before.document_revision);
        assert_eq!(inside.edit_revision, before.edit_revision);
        assert_eq!(inside.primary, before.primary);
        assert_eq!(inside.selected_count, before.selected_count);
        assert_eq!(inside.grouped, before.grouped);
        assert_eq!(inside.style, before.style);
        assert_ne!(inside, before);
        let mut without_scope_identity = inside.clone();
        without_scope_identity.group_scope = before.group_scope.clone();
        assert_eq!(without_scope_identity, before);
        assert!(editor.exit_group_scope());
        let returned = EditorControlState::capture(&editor);
        assert_eq!(returned.group_scope.0, before.group_scope.0);
        assert_ne!(returned.group_scope.1, before.group_scope.1);
        assert_ne!(returned, before);
        assert_eq!(
            editor.document.bounded_history_snapshot().unwrap(),
            snapshot
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{finish_annotation_content, AnnotationPreviewVisibility};
    use crate::{
        annotation::{Point, Tool},
        editor::Editor,
        geometry::Rect,
    };

    fn editor() -> Editor {
        let full = Rect::full(600, 400);
        Editor::new(full, Some(full))
    }

    #[test]
    fn pin_processing_keeps_first_uncommitted_text_and_its_undo() {
        let mut editor = editor();
        editor.select_tool(Tool::Text);
        editor.begin_text(None, Point { x: 30., y: 40. });
        editor.set_text("首个尚未提交的文字".into());
        assert!(!editor.document.can_undo());
        assert!(finish_annotation_content(&mut editor));
        assert!(editor.text_draft.is_none());
        assert_eq!(editor.document.items().len(), 1);
        assert_eq!(
            editor.document.items()[0].annotation.text,
            "首个尚未提交的文字"
        );
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        editor.redo();
        assert_eq!(editor.document.items().len(), 1);
    }

    #[test]
    fn pin_processing_keeps_pending_arrow_once_and_ignores_empty_text() {
        let mut editor = editor();
        editor.select_tool(Tool::Arrow);
        editor.pointer_down((30, 40), 4.);
        editor.pointer_up((30, 40));
        editor.pointer_move((120, 80));
        assert!(editor.has_pending_drawing());
        assert!(!editor.document.can_undo());
        assert!(finish_annotation_content(&mut editor));
        assert!(!editor.has_pending_drawing());
        assert_eq!(editor.document.items().len(), 1);
        assert_eq!(editor.document.items()[0].annotation.tool, Tool::Arrow);
        let revision = editor.document.edit_revision();
        assert!(finish_annotation_content(&mut editor));
        assert_eq!(editor.document.edit_revision(), revision);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());

        let mut empty = self::editor();
        empty.select_tool(Tool::Text);
        empty.begin_text(None, Point { x: 30., y: 40. });
        assert!(!finish_annotation_content(&mut empty));
        assert!(empty.document.items().is_empty());
    }

    #[test]
    fn pin_edit_revision_preserves_boundary_redo_for_non_content_actions() {
        let mut editor = editor();
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((30, 40), 4.);
        editor.pointer_up((120, 80));
        let revision = editor.document.edit_revision();
        assert!(revision > 0);
        editor.undo();
        editor.select_tool(Tool::Pen);
        editor.set_color(0x579aff);
        editor.set_width(7.);
        editor.pointer_down((200, 100), 4.);
        editor.pointer_move((250, 120));
        editor.cancel_gesture();
        assert_eq!(editor.document.edit_revision(), revision);
        assert!(editor.document.can_redo());
        editor.redo();
        assert_eq!(editor.document.edit_revision(), revision);
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((200, 100), 4.);
        editor.pointer_up((250, 160));
        assert!(editor.document.edit_revision() > revision);
    }
    #[test]
    fn pin_annotation_preview_finishes_pending_content_once_and_keeps_undo() {
        let mut editor = editor();
        editor.select_tool(Tool::Arrow);
        editor.pointer_down((30, 40), 4.);
        editor.pointer_up((30, 40));
        editor.pointer_move((120, 80));
        assert!(editor.has_pending_drawing());
        assert!(!editor.document.can_undo());
        let visibility = AnnotationPreviewVisibility::default();
        // 生产切换先走同一 Editor 收尾，再传入准备结果；开关本身不写文档。
        assert!(finish_annotation_content(&mut editor));
        let revision = editor.document.edit_revision();
        let id = editor.document.items()[0].id;
        assert!(!visibility.toggle_after_prepare(Ok(())).unwrap());
        assert!(!visibility.allows_editing(true, false));
        assert_eq!(editor.document.edit_revision(), revision);
        assert!(!editor.has_pending_drawing());
        assert_eq!(editor.document.items().len(), 1);
        assert!(visibility.toggle_after_prepare(Ok(())).unwrap());
        assert_eq!(editor.document.edit_revision(), revision);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        assert!(editor.document.can_redo());
        editor.redo();
        assert_eq!(editor.document.items().len(), 1);
        assert_eq!(editor.document.items()[0].id, id);
    }

    #[test]
    fn pin_annotation_preview_rejects_failed_prepare_and_shares_only_with_own_canvas() {
        let visibility = AnnotationPreviewVisibility::default();
        let canvas = visibility.clone();
        let reopened = AnnotationPreviewVisibility::default();
        assert!(visibility
            .toggle_after_prepare(Err("请先完成输入法组合文字".into()))
            .is_err());
        assert!(canvas.visible());
        assert!(!visibility.toggle_after_prepare(Ok(())).unwrap());
        assert!(!canvas.visible());
        assert!(reopened.visible());
        assert!(!canvas.allows_editing(true, false));
        canvas.reveal_for_editing();
        assert!(visibility.visible());
        assert!(visibility.allows_editing(true, false));
        assert!(!visibility.allows_editing(false, false));
        assert!(!visibility.allows_editing(true, true));
    }

    #[test]
    fn pin_annotation_preview_preserves_scene_export_and_redo_when_hidden() {
        use crate::{
            capture::Image,
            scene::{PixelPlane, Scene, SourcePixels},
        };
        let full = Rect::full(64, 48);
        let mut editor = Editor::new(full, Some(full));
        editor.select_tool(Tool::Rectangle);
        editor.set_color(0xff0000);
        editor.pointer_down((8, 8), 4.);
        editor.pointer_up((40, 28));
        let original = Image {
            width: 64,
            height: 48,
            bgra: vec![255; 64 * 48 * 4],
            readback_ms: 0.,
        };
        let source = SourcePixels {
            fill: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &original.bgra,
            }],
            overlay: None,
        };
        let scene = Scene::prepare(&editor.annotations(), &source);
        let mut before = source.read(full);
        scene.apply(&mut before, full);
        assert_ne!(before.bgra, original.bgra);
        let visibility = AnnotationPreviewVisibility::default();
        let revision = editor.document.edit_revision();
        assert!(!visibility.toggle_after_prepare(Ok(())).unwrap());
        let mut hidden_export = source.read(full);
        scene.apply(&mut hidden_export, full);
        assert_eq!(hidden_export.bgra, before.bgra);
        assert_eq!(editor.document.edit_revision(), revision);
        editor.undo();
        assert!(editor.document.can_redo());
        let undone_revision = editor.document.edit_revision();
        visibility.reveal_for_editing();
        assert_eq!(editor.document.edit_revision(), undone_revision);
        assert!(editor.document.can_redo());
        editor.redo();
        let mut redone = source.read(full);
        Scene::prepare(&editor.annotations(), &source).apply(&mut redone, full);
        assert_eq!(redone.bgra, before.bgra);
        assert_eq!(original.bgra, vec![255; 64 * 48 * 4]);
    }
}
