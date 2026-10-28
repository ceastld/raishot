//! 用户导出快照只共享底图和 Scene；大图 CPU 重绘在后台完成。
use crate::{
    capture::Image,
    crop_mask::Polygon,
    geometry::Rect,
    output_projection::{window_opacity, OutputProjection},
    pin_presentation::PresentationOutputPlan,
    pin_view::PinView,
    scene::{PixelPlane, Scene, SourcePixels},
    Result,
};
use gpui::{App, Context, RenderImage, Window};
use std::sync::Arc;

/// 贴图复制带上当前位置，再贴图时回到原处：输出放大显示时已按倍率放大、缩小显示时保持原图分辨率，
/// 所以显示倍率是 `zoom.min(1)`；输出中心对齐窗口中心（裁剪、外观留白时也落在贴图上）。
fn copy_placement(
    hwnd: windows::Win32::Foundation::HWND,
    image: &Image,
    zoom: f32,
) -> Option<crate::export::ClipboardPlacement> {
    if !zoom.is_finite() || zoom <= 0. {
        return None;
    }
    let window = crate::pin_native::rect(hwnd).ok()?;
    let scale = zoom.min(1.);
    let origin = |start: i32, window: u32, extent: u32| {
        let shown = (f64::from(extent) * f64::from(scale)).round() as i64;
        (i64::from(start) + (i64::from(window) - shown) / 2) as i32
    };
    Some(crate::export::ClipboardPlacement::Exact {
        origin: (
            origin(window.x, window.width, image.width),
            origin(window.y, window.height, image.height),
        ),
        scale,
    })
}

pub(crate) struct UserExportSnapshot {
    pub(crate) projection: OutputProjection,
    pub(crate) opacity: u8,
    source: Arc<RenderImage>,
    /// 矢量文字贴图的纯色底；此时 source 只是占位，导出时才按区域生成像素。
    fill: Option<[u8; 4]>,
    canvas: Rect,
    scene: Arc<Scene>,
    mask: Option<Polygon>,
    /// 截图外观只保存几何计划；原图与 Scene 坐标始终是内容坐标。
    presentation: Option<PresentationOutputPlan>,
    capture_pixels: Option<Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
    cursor: Option<crate::cursor::CapturedCursor>,
}

#[cfg(test)]
#[path = "pin_export_presentation_tests.rs"]
mod presentation_tests;
impl UserExportSnapshot {
    /// 历史仅保存选区内容缓存；没有外观、窗口alpha或完整来源私有资源。
    pub(crate) fn content_only(mut self) -> Result<Self> {
        self.projection = OutputProjection::new(self.projection.source, 1.)?;
        self.presentation = None;
        self.opacity = 255;
        Ok(self)
    }
    pub(crate) fn render(&self) -> Result<Image> {
        let mut source = if let Some(pixels) = &self.capture_pixels {
            pixels.source_pixels()
        } else {
            SourcePixels {
                fill: self.fill,
                canvas: self.canvas,
                planes: if self.fill.is_some() {
                    Vec::new()
                } else {
                    vec![PixelPlane {
                        rect: self.canvas,
                        bgra: self.source.as_bytes(0).ok_or("导出底图没有像素")?,
                    }]
                },
                overlay: None,
            }
        };
        source.overlay = self
            .cursor
            .as_ref()
            .map(crate::cursor::CapturedCursor::plane);
        if let Some(presentation) = &self.presentation {
            let mut image = presentation.render(&source, &self.scene, self.mask.as_ref())?;
            // 外观、内容、标注完整合成后只应用一次窗口透明度。
            window_opacity(&mut image, self.opacity);
            return Ok(image);
        }
        let mut image = if self.projection.identity() {
            source.read(self.projection.source)
        } else {
            self.projection.background(&source)?
        };
        if self.projection.identity() {
            self.scene.try_apply(&mut image, self.projection.source)?;
            if let Some(mask) = &self.mask {
                mask.apply(&mut image, self.projection.source);
            }
        } else {
            self.scene
                .apply_projected(&mut image, &source, &self.projection)?;
            if let Some(mask) = &self.mask {
                mask.apply_projected(&mut image, &self.projection);
            }
        }
        window_opacity(&mut image, self.opacity);
        Ok(image)
    }
}
#[derive(Clone, Copy)]
pub(crate) enum UserExportAction {
    Copy,
    Save,
    QuickSave,
    Drag,
}
impl PinView {
    pub(crate) fn user_export_snapshot(&mut self, cx: &mut App) -> Result<UserExportSnapshot> {
        self.snapshot_for_export(cx, false)
    }
    fn snapshot_for_export(
        &mut self,
        cx: &mut App,
        retain_drag_capture: bool,
    ) -> Result<UserExportSnapshot> {
        self.require_processing_image_available()?;
        if self.capture_source.is_some() && self.canvas.is_none() {
            return Err("截图贴图完整文档尚未安装，未降级为原始像素输出".into());
        }
        let full = Rect::full(self.image_size.0, self.image_size.1);
        let preliminary = self.canvas.as_ref().map_or(Ok(full), |c| {
            let v = c.read(cx);
            if v.editor.drawing_lasso() {
                let host = v.selected()?;
                let pending = v
                    .editor
                    .lasso_points()
                    .and_then(|points| Polygon::new(&points));
                pending
                    .as_ref()
                    .or(v.editor.document.crop_mask.as_ref())
                    .map_or(Ok(host), |mask| {
                        mask.output_rect(host).ok_or_else(|| {
                            tr!(
                                "套索与选区没有交集",
                                "The lasso doesn't overlap the selection"
                            )
                            .into()
                        })
                    })
            } else {
                v.output_rect()
            }
        })?;
        // 必须先检查预算。失败不得提交文字、清空 redo 或采样/分配整图。
        self.export_projection(preliminary)?;
        let (rect, scene, mask) = if let Some(c) = &self.canvas {
            let (result, changed) = c.update(cx, |v, cx| {
                let before = v.editor.document.edit_revision();
                if let Err(error) = v.try_finish_content() {
                    v.status = error.to_string();
                    cx.notify();
                    return (Err(error), false);
                }
                if retain_drag_capture {
                    v.clear_pointer_tracking();
                } else {
                    v.cancel_pointer();
                }
                let changed = before != v.editor.document.edit_revision();
                if changed {
                    cx.notify();
                }
                let result = (|| -> Result<_> {
                    let rect = v.output_rect()?;
                    v.flush_tool_presets();
                    let scene = v.prepared_scene();
                    if !scene.errors.is_empty() {
                        return Err(scene.errors.join(";").into());
                    }
                    let mask = v
                        .editor
                        .lasso_draft
                        .as_ref()
                        .or(v.editor.document.crop_mask.as_ref())
                        .cloned();
                    Ok((rect, scene, mask))
                })();
                (result, changed)
            });
            if changed {
                self.redo_edits.clear();
            }
            let (rect, scene, mask) = result?;
            (rect, scene, mask)
        } else {
            (full, Arc::new(Scene::default()), None)
        };
        let (projection, presentation) = self.export_projection(rect)?;
        Ok(UserExportSnapshot {
            projection,
            opacity: self.state.opacity,
            source: self.image.clone(),
            fill: self.vector_text_fill(),
            canvas: full,
            scene,
            mask,
            presentation,
            capture_pixels: self.capture_pixels.clone(),
            cursor: self.canvas.as_ref().and_then(|canvas| {
                let view = canvas.read(cx);
                view.cursor
                    .as_ref()
                    .filter(|_| view.include_cursor)
                    .map(|cursor| crate::cursor::CapturedCursor {
                        rect: cursor.rect,
                        image: Image {
                            width: cursor.image.width,
                            height: cursor.image.height,
                            bgra: cursor.image.bgra.clone(),
                            readback_ms: 0.,
                        },
                    })
            }),
        })
    }
    /// 预检和快照共享同一整张外框栅格预算；不分别舍入内容与装饰宽高。
    fn export_projection(
        &self,
        rect: Rect,
    ) -> Result<(OutputProjection, Option<PresentationOutputPlan>)> {
        match self.presentation_style {
            Some(style) => {
                let presentation = PresentationOutputPlan::new(rect, style, self.state.zoom)?;
                Ok((presentation.scene_projection(), Some(presentation)))
            }
            None => Ok((OutputProjection::new(rect, self.state.zoom)?, None)),
        }
    }
    pub(crate) fn export_user(
        &mut self,
        action: UserExportAction,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.processing || self.saving {
            return;
        }
        let drag_ticket = if matches!(action, UserExportAction::Drag) {
            match crate::pin_drag_request::NativeDragRequest::begin(crate::gpui_capture::hwnd(w)) {
                Ok(request) => {
                    let ticket = request.ticket();
                    self.drag_request = Some(request);
                    Some(ticket)
                }
                Err(error) => {
                    self.cancel_drag(w);
                    self.status = error.to_string();
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let snapshot = match self.snapshot_for_export(cx, drag_ticket.is_some()) {
            Ok(s) => s,
            Err(e) => {
                self.drag_request = None;
                self.status = e.to_string();
                cx.notify();
                return;
            }
        };
        self.save_busy(true, cx);
        self.status = tr!("正在生成贴图输出…", "Generating pin output…").into();
        cx.notify();
        let rendering_ticket = drag_ticket.clone();
        let task = cx.background_executor().spawn(async move {
            if rendering_ticket
                .as_ref()
                .is_some_and(|ticket| ticket.cancelled())
            {
                return Ok(None);
            }
            snapshot.render().map(Some).map_err(|e| e.to_string())
        });
        cx.spawn_in(w, async move |v, cx| {
            let result = task.await;
            let _ = v.update_in(cx, |v, w, cx| {
                if let Some(ticket) = &drag_ticket {
                    let current = v.drag_request.as_ref().map(|request| request.ticket());
                    if !current
                        .as_ref()
                        .is_some_and(|current| current.same_request(ticket))
                    {
                        return;
                    }
                    if ticket.cancelled() {
                        v.save_busy(false, cx);
                        v.drag_request = None;
                        v.status.clear();
                        v.sync_toolbar(w, cx);
                        cx.notify();
                        return;
                    }
                }
                v.save_busy(false, cx);
                match result {
                    Err(e) => {
                        v.drag_request = None;
                        v.status = e;
                    }
                    Ok(None) => {
                        v.drag_request = None;
                        v.status.clear();
                    }
                    Ok(Some(image)) => match action {
                        UserExportAction::Copy => {
                            let hwnd = crate::gpui_capture::hwnd(w);
                            let placement = copy_placement(hwnd, &image, v.state.zoom);
                            v.status = crate::export::copy_image_with(&image, hwnd, placement)
                                .err()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| tr!("已复制贴图", "Pin copied").into())
                        }
                        UserExportAction::Save => v.open_save_image(image, w, cx),
                        UserExportAction::QuickSave => v.quick_save_image(image, w, cx),
                        UserExportAction::Drag => v.drag_out_image(image, w, cx),
                    },
                }
                v.sync_toolbar(w, cx);
                cx.notify();
            });
        })
        .detach();
    }
}
