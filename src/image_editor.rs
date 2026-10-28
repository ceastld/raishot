//! 图片编辑窗口与视口接入；复用截图文档和 Scene，原图始终不改写。
use crate::{
    annotation::{Point, Tool},
    capture::Image,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    gpui_capture::hwnd,
    toolbar_layout::ToolbarPanel,
    viewport::Viewport,
    Result,
};
use gpui::{
    div, point, prelude::*, px, rgb, size, App, AppContext, Application, AsyncApp, Bounds, Context,
    MouseButton, MouseDownEvent, MouseUpEvent, PathPromptOptions, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

pub struct ImageCanvas {
    pub path: Option<PathBuf>,
    pub viewport: Viewport,
    pub auto_fit: bool,
    pub host: (u32, u32),
    pub pan: Option<(Point, Point)>,
}
/// 解码器移交给编辑器前的数字记录，不持有像素或图片引用。
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct ImageInputProbe {
    pub dimensions: (u32, u32),
    pub input_pointer: usize,
    pub input_length: usize,
    pub input_capacity: usize,
}
fn decode_file(
    path: PathBuf,
    executor: &gpui::BackgroundExecutor,
) -> gpui::Task<std::result::Result<(Image, ImageInputProbe), String>> {
    executor.spawn(async move {
        let image = crate::export::read_image(&path).map_err(|error| error.to_string())?;
        let probe = ImageInputProbe {
            dimensions: (image.width, image.height),
            input_pointer: image.bgra.as_ptr() as usize,
            input_length: image.bgra.len(),
            input_capacity: image.bgra.capacity(),
        };
        Ok((image, probe))
    })
}
/// 文件打开与受控验收共用实际后台解码、生产工厂和窗口入口。
pub(crate) async fn open_file(
    path: PathBuf,
    cx: &mut AsyncApp,
) -> Result<(WindowHandle<CaptureView>, ImageInputProbe)> {
    let (image, probe) = decode_file(path.clone(), cx.background_executor()).await?;
    let handle = cx
        .update(|cx| open(image, Some(path), cx))
        .map_err(|error| error.to_string())??;
    Ok((handle, probe))
}

/// 输出入口共用的空闲资格；只读取状态，不借输出提交草稿。
pub(crate) fn screenshot_middle_pin_idle(
    editor: &crate::editor::Editor,
    interaction_busy: bool,
    input_active: bool,
) -> bool {
    !interaction_busy
        && !input_active
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
}

/// 确认选区中键保持已有资格；预览范围由独立候选模型检查。
pub(crate) fn screenshot_middle_pin_ready(
    editor: &crate::editor::Editor,
    canvas: Rect,
    interaction_busy: bool,
    input_active: bool,
) -> bool {
    screenshot_middle_pin_idle(editor, interaction_busy, input_active)
        && editor
            .crop
            .is_some_and(|crop| crop.valid(canvas.width, canvas.height))
}

/// 原生首次交接只冻结这份空闲资格；不提交草稿，也不需要当前预览范围。
pub(crate) fn screenshot_preview_pin_idle(
    editor: &crate::editor::Editor,
    interaction_busy: bool,
    input_active: bool,
) -> bool {
    editor.crop.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
        && screenshot_middle_pin_idle(editor, interaction_busy, input_active)
}

fn screenshot_preview_range(
    editor: &crate::editor::Editor,
    canvas: Rect,
    manual: Option<Rect>,
    point: (i32, i32),
    presented: Option<Rect>,
    interaction_busy: bool,
    input_active: bool,
) -> Option<Rect> {
    if let Some(manual) = manual {
        crate::preview_middle_pin::confirm_candidate(
            editor,
            canvas,
            presented.filter(|rect| *rect == manual),
            interaction_busy,
            input_active,
        )
    } else {
        crate::preview_middle_pin::candidate(
            editor,
            canvas,
            point,
            presented,
            interaction_busy,
            input_active,
        )
    }
}

impl CaptureView {
    /// 原位 OCR 只接独立截图和真实文件图片宿主，贴图继续用自己的会话。
    pub(crate) fn inline_ocr_host(&self) -> bool {
        inline_ocr_host_allowed(
            self.pin_mode,
            self.image_canvas.is_some(),
            self.source.is_image(),
        ) && self.editor.long_clip().is_none()
    }

    pub(crate) fn inline_ocr_interaction_idle(&self) -> bool {
        self.image_canvas
            .as_ref()
            .is_none_or(|state| state.pan.is_none())
    }

    pub(crate) fn ocr_projection(
        &self,
        window: &Window,
    ) -> Option<crate::ocr_projection::OcrProjection> {
        let client = window.viewport_size();
        crate::ocr_projection::OcrProjection::new(
            self.canvas_viewport(window),
            window.scale_factor(),
            [f32::from(client.width), f32::from(client.height)],
        )
    }

    fn screenshot_output_flags(&self) -> (bool, bool) {
        self.screenshot_output_flags_for_panel(self.toolbar_panel)
    }
    fn screenshot_output_flags_for_panel(&self, panel: ToolbarPanel) -> (bool, bool) {
        (
            self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.pointer_down_start.is_some()
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || panel != ToolbarPanel::None
                || self.recognition_result.is_some(),
        )
    }
    pub(crate) fn screenshot_output_ready(&self) -> bool {
        self.screenshot_output_ready_for_panel(self.toolbar_panel)
    }
    fn screenshot_output_ready_for_panel(&self, panel: ToolbarPanel) -> bool {
        let (interaction_busy, input_active) = self.screenshot_output_flags_for_panel(panel);
        !self.pin_mode
            && self.image_canvas.is_none()
            && screenshot_middle_pin_ready(
                &self.editor,
                self.source.full(),
                interaction_busy,
                input_active,
            )
    }
    pub(crate) fn middle_pin_finish_panel_open(&self) -> bool {
        !self.pin_mode && self.image_canvas.is_none() && self.toolbar_panel == ToolbarPanel::Finish
    }
    /// 接收已保存的原生预览，仅核编辑资格；不要求当前 GPUI hover 或 pointer 等值。
    pub(crate) fn screenshot_preview_output_ready(
        &self,
        rect: Rect,
        original_point: (i32, i32),
    ) -> bool {
        if self.pin_mode || self.image_canvas.is_some() {
            return false;
        }
        let (interaction_busy, input_active) = self.screenshot_output_flags();
        crate::preview_middle_pin::candidate(
            &self.editor,
            self.source.full(),
            original_point,
            Some(rect),
            interaction_busy,
            input_active,
        ) == Some(rect)
    }
    pub(crate) fn screenshot_preview_confirm_ready(&self, rect: Rect) -> bool {
        if self.pin_mode || self.image_canvas.is_some() {
            return false;
        }
        if self.manual_preview.is_some_and(|manual| manual != rect) {
            return false;
        }
        let (interaction_busy, input_active) = self.screenshot_output_flags();
        crate::preview_middle_pin::confirm_candidate(
            &self.editor,
            self.source.full(),
            Some(rect),
            interaction_busy,
            input_active,
        ) == Some(rect)
    }
    /// 只更新普通图片编辑器的显示预览；Scene 与导出一直借用完整原图。
    pub(crate) fn refresh_editor_preview(&mut self, window: &Window) -> Result<()> {
        if self.editor.long_clip().is_some() {
            return self.prepare_long_clip_previews(window);
        }
        if self.image_canvas.is_none() || self.pin_mode || !self.source.is_projected_image() {
            return Ok(());
        }
        if self.previews.len() != 1 {
            return Err("长图编辑预览数量不一致".into());
        }
        let logical = window.viewport_size();
        let dpi = window.scale_factor();
        let width = f64::from(f32::from(logical.width)) * f64::from(dpi);
        let height = f64::from(f32::from(logical.height)) * f64::from(dpi);
        if !width.is_finite()
            || !height.is_finite()
            || width < 0.
            || height < 0.
            || width > f64::from(u32::MAX)
            || height > f64::from(u32::MAX)
        {
            return Err("图片编辑客户区尺寸无效".into());
        }
        let host = (width.round() as u32, height.round() as u32);
        if host.0 == 0 || host.1 == 0 {
            return Ok(());
        }
        let viewport = self.canvas_viewport(window);
        let Some(region) = self.canvas_visible_source_region(window)? else {
            return Ok(());
        };
        if let Some(preview) = self
            .source
            .refresh_shared_preview_at(region, viewport.zoom)?
        {
            let old = std::mem::replace(&mut self.previews[0], preview);
            let id = old.id.0;
            if !self.retired_editor_preview_ids.contains(&id) {
                self.retired_editor_preview_ids.push(id);
            }
            if !self
                .retired_raster_previews
                .iter()
                .any(|retired| std::sync::Arc::ptr_eq(retired, &old))
            {
                self.retired_raster_previews.push(old);
            }
            self.editor_preview_updates = self.editor_preview_updates.saturating_add(1);
        }
        Ok(())
    }
    pub(crate) fn canvas_viewport(&self, window: &Window) -> Viewport {
        if self.pin_mode {
            let size = window.viewport_size();
            let dpi = window.scale_factor();
            let content = self.pin_content_rect.unwrap_or_else(|| self.source.full());
            crate::pin_presentation_host::source_content_viewport(
                content,
                (
                    (f32::from(size.width) * dpi).round() as u32,
                    (f32::from(size.height) * dpi).round() as u32,
                ),
                self.pin_presentation_style,
                self.pin_window_sizing,
            )
        } else {
            self.image_canvas
                .as_ref()
                .map_or(Viewport::default(), |s| s.viewport)
        }
    }
    pub(crate) fn canvas_scale(&self, window: &Window) -> f32 {
        window.scale_factor() / self.canvas_viewport(window).zoom
    }
    pub(crate) fn canvas_offset(&self, window: &Window) -> gpui::Point<gpui::Pixels> {
        let p = self.canvas_viewport(window).offset;
        point(
            px(p.x / window.scale_factor()),
            px(p.y / window.scale_factor()),
        )
    }
    pub(crate) fn sync_viewport(&mut self, window: &Window) {
        self.sync_inline_ocr_projection(window);
        let size = window.viewport_size();
        let dpi = window.scale_factor();
        let host = (
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        if self
            .image_canvas
            .as_ref()
            .is_some_and(|state| state.host != host)
        {
            self.invalidate_inline_ocr_projection();
        }
        let fit_image = self.long_clip_projection().map_or_else(
            || self.source.full(),
            |projection| {
                let (width, height) = projection.display_size();
                Rect::full(width, height)
            },
        );
        let area = self
            .long_clip_fit_area(host, dpi)
            .unwrap_or_else(|| fit_area(host, dpi));
        let Some(state) = &mut self.image_canvas else {
            return;
        };
        if self.pin_mode {
            state.host = host;
            let content = self.pin_content_rect.unwrap_or_else(|| self.source.full());
            state.viewport = crate::pin_presentation_host::source_content_viewport(
                content,
                host,
                self.pin_presentation_style,
                self.pin_window_sizing,
            );
            return;
        }
        if host != state.host {
            state.host = host;
            if state.auto_fit {
                state.viewport.fit(fit_image, area);
            }
        }
    }
    pub(crate) fn fit_image(&mut self, window: &Window) {
        self.invalidate_inline_ocr_projection();
        self.sync_viewport(window);
        let image = self.long_clip_projection().map_or_else(
            || self.source.full(),
            |projection| {
                let (width, height) = projection.display_size();
                Rect::full(width, height)
            },
        );
        let host = self
            .image_canvas
            .as_ref()
            .map_or((1, 1), |state| state.host);
        let dpi = window.scale_factor();
        let area = self
            .long_clip_fit_area(host, dpi)
            .unwrap_or_else(|| fit_area(host, dpi));
        if let Some(state) = &mut self.image_canvas {
            state.auto_fit = true;
            state.viewport.fit(image, area);
        }
    }
    pub(crate) fn zoom_image(&mut self, zoom: f32, anchor: Point, cx: &mut Context<Self>) {
        self.invalidate_inline_ocr_projection();
        if let Some(state) = &mut self.image_canvas {
            state.viewport.set_zoom(zoom, anchor);
            state.auto_fit = false;
            cx.notify();
        }
    }
    pub(crate) fn zoom_center(&self, window: &Window) -> Point {
        let dpi = window.scale_factor();
        let host = self.image_canvas.as_ref().map_or((1, 1), |s| s.host);
        let area = self
            .long_clip_fit_area(host, dpi)
            .unwrap_or_else(|| fit_area(host, dpi));
        Point {
            x: area.x as f32 + area.width as f32 / 2.,
            y: area.y as f32 + area.height as f32 / 2.,
        }
    }
    pub(crate) fn ui_monitor(&self, index: usize) -> Rect {
        self.image_canvas.as_ref().map_or_else(
            || self.source.screen_rect(index),
            |s| Rect::full(s.host.0.max(1), s.host.1.max(1)),
        )
    }
    pub(crate) fn middle_down(
        &mut self,
        e: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.long_clip_down(e, window, cx) {
            return;
        }
        self.invalidate_click_sequence();
        if self.pin_mode {
            return;
        }
        if self.image_canvas.is_none() {
            let finish_panel = self.middle_pin_finish_panel_open();
            // 只让中键越过外观面板；预览重放和其它输出仍使用原来的面板门禁。
            let panel = if finish_panel {
                ToolbarPanel::None
            } else {
                self.toolbar_panel
            };
            if self.screenshot_output_ready_for_panel(panel) {
                cx.stop_propagation();
                if finish_panel {
                    self.toolbar_panel = ToolbarPanel::None;
                    window.focus(&self.focus);
                }
                self.perform(Action::Pin, window, cx);
                return;
            }
            // 原生早期事件尚未保存预览快照，不能套用交接后才出现的范围。
            if self.preview_middle_replay || self.editor.crop.is_some() {
                return;
            }
            let (interaction_busy, input_active) = self.screenshot_output_flags();
            if !screenshot_middle_pin_idle(&self.editor, interaction_busy, input_active) {
                return;
            }
            // 先固定真实已呈现范围，Middle 不借本次新点或晚到 hover 改选；原点仍保存。
            let preview = self.presented_control_selection(window);
            self.pointer_sample = self.world_point(e.position, window);
            self.pointer = self.physical(e.position, window);
            self.invalidate_control_hover();
            self.hover_window = self
                .window_targets
                .iter()
                .position(|target| target.rect.contains(self.pointer));
            let host = screenshot_preview_range(
                &self.editor,
                self.source.full(),
                self.manual_preview,
                self.pointer,
                preview,
                interaction_busy,
                input_active,
            );
            let Some(host) = host else {
                cx.notify();
                return;
            };
            match self.output_rect_for(host) {
                Ok(output_rect) => {
                    cx.stop_propagation();
                    self.pin_range(output_rect, window, cx);
                }
                Err(error) => {
                    self.status = error.to_string();
                    cx.notify();
                }
            }
            return;
        }
        if self.saving
            || self.processing
            || self.editor.is_dragging()
            || self.editor.has_pending_drawing()
            || self.editor.text_draft.is_some()
            || self.input.marked.is_some()
            || self.text_composing()
            || self.result_editor.is_some()
            || self.result_selection_dragging(self.result_selection_generation())
            || self.ocr_overlay_dragging()
            || self.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None
            || self.flyout.open().is_some()
            || self.watermark_input_focused
            || !window.is_window_active()
            || !self.focus.is_focused(window)
        {
            return;
        }
        self.invalidate_inline_ocr_projection();
        let p = client_point(e.position, window);
        if let Some(state) = &mut self.image_canvas {
            state.pan = Some((p, state.viewport.offset));
            state.auto_fit = false;
        }
        unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd(window));
        }
        self.observe_text_core_activation(window, cx);
        cx.notify();
    }
    pub(crate) fn middle_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.long_clip_up(event, window, cx) {
            return;
        }
        if self.cancel_image_pan(window) {
            cx.notify();
        }
    }
    pub(crate) fn cancel_image_pan(&mut self, window: &Window) -> bool {
        let cancelled = self
            .image_canvas
            .as_mut()
            .is_some_and(|state| state.pan.take().is_some());
        if cancelled {
            self.invalidate_inline_ocr_projection();
            if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == hwnd(window)
            {
                self.release_capture();
            }
        }
        cancelled
    }
    pub(crate) fn pan_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .image_canvas
            .as_ref()
            .is_some_and(|state| state.pan.is_some())
            && (event.pressed_button != Some(MouseButton::Middle)
                || !window.is_window_active()
                || !self.focus.is_focused(window)
                || self.saving
                || self.processing
                || self.pin_handoff_pending
                || self.input.marked.is_some()
                || self.text_composing()
                || self.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None
                || unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() }
                    != hwnd(window))
        {
            self.cancel_image_pan(window);
            cx.notify();
            return true;
        }
        let p = client_point(event.position, window);
        if self
            .image_canvas
            .as_ref()
            .is_some_and(|state| state.pan.is_some())
        {
            self.invalidate_inline_ocr_projection();
        }
        if let Some(state) = &mut self.image_canvas {
            if let Some((start, offset)) = state.pan {
                state.viewport.offset = Point {
                    x: offset.x + p.x - start.x,
                    y: offset.y + p.y - start.y,
                };
                cx.notify();
                return true;
            }
        }
        false
    }
    pub(crate) fn finish_output(&mut self, message: &str, window: &mut Window) {
        if self.editor.long_clip().is_some() {
            self.long_clip_output_done = true;
        }
        if self.image_canvas.is_some() {
            self.status = message.into();
            if self.image_saved.is_some() {
                self.image_saved = Some(self.image_edit_state());
            }
        } else {
            self.capture_lock.take();
            window.remove_window();
        }
    }
    fn image_edit_state(&self) -> (u64, Option<Rect>) {
        (self.editor.document.revision(), self.editor.crop)
    }
    /// 独立图片编辑窗口有未复制或保存的修改时，第一次关闭只提示；同一状态下再关一次才放弃。
    /// 贴图标注和截图不经过这里（`image_saved` 为空）。
    pub(crate) fn confirm_image_discard(&mut self) -> bool {
        let Some(saved) = self.image_saved else {
            return true;
        };
        let current = self.image_edit_state();
        if current == saved || self.image_discard_armed == Some(current) {
            return true;
        }
        self.image_discard_armed = Some(current);
        self.status = tr!(
            "有未保存的修改：再按一次 Esc 或再次关闭即放弃；Ctrl+C 复制，Ctrl+S 保存。",
            "Unsaved changes: press Esc or close again to discard; Ctrl+C copies, Ctrl+S saves."
        )
        .into();
        false
    }
    pub(crate) fn open_image_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.processing || self.saving {
            return;
        }
        let receiver = cx.prompt_for_paths(path_options());
        self.processing = true;
        self.status = tr!("请选择要编辑的图片…", "Choose an image to edit…").into();
        cx.spawn_in(window, async move |view, cx| {
            let chosen = receiver.await;
            let path = match chosen {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => None,
                // 通道被取消（窗口关闭等）视为用户取消，不提示错误。
                Err(_) => None,
                Ok(Err(error)) => {
                    let _ = view.update(cx, |v, cx| {
                        v.processing = false;
                        v.status =
                            trf!("打开图片失败：{error}", "Failed to open the image: {error}");
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                let _ = view.update(cx, |v, cx| {
                    v.processing = false;
                    v.status.clear();
                    cx.notify();
                });
                return;
            };
            let result = decode_file(path.clone(), cx.background_executor()).await;
            let _ = view.update(cx, |v, cx| {
                v.processing = false;
                match result {
                    Ok((image, _)) => match open(image, Some(path), cx) {
                        Ok(_) => v.status.clear(),
                        Err(e) => v.status = e.to_string(),
                    },
                    Err(e) => v.status = e,
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(crate) fn image_controls(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.image_canvas.as_ref().expect("image controls");
        let palette = crate::capture_toolbar::Palette::new(self.light_theme);
        let name = state
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or(tr!("图片", "Image").into(), |n| {
                n.to_string_lossy().into_owned()
            });
        let mut bar = div()
            .absolute()
            .top(px(8.))
            .left(px(12.))
            .right(px(12.))
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_2()
            .rounded_lg()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(div().flex_1().overflow_hidden().child(format!(
                "{name} · {} × {}",
                self.source.bounds.width, self.source.bounds.height
            )));
        for (id, label, action) in [
            (
                "image-open",
                tr!("打开图片", "Open image"),
                Action::OpenImage,
            ),
            ("image-fit", tr!("适应窗口", "Fit window"), Action::FitImage),
            ("image-actual", "100%", Action::ActualImage),
            ("image-zoom-out", "−", Action::ZoomImage(1. / 1.1)),
            ("image-zoom-in", "+", Action::ZoomImage(1.1)),
            ("image-full", tr!("完整图片", "Whole image"), Action::All),
        ] {
            bar = bar.child(
                div()
                    .id(id)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(palette.hover)))
                    .child(label)
                    .child(self.probe(id))
                    .on_click(cx.listener(move |v, _, w, cx| v.perform(action, w, cx))),
            );
        }
        bar.child(div().text_color(rgb(palette.muted)).child(trf!(
            "{:.0}% · 中键拖动 · Alt+滚轮缩放",
            "{:.0}% · middle-drag pans · Alt+wheel zooms",
            state.viewport.zoom * 100.
        )))
        .into_any_element()
    }
}

fn inline_ocr_host_allowed(pin: bool, image_editor: bool, source_is_image: bool) -> bool {
    !pin && (!image_editor || source_is_image)
}

#[cfg(test)]
mod inline_ocr_host_tests {
    use super::*;

    #[test]
    fn file_inline_ocr_host_accepts_original_image_and_keeps_pin_and_invalid_hosts_separate() {
        assert!(inline_ocr_host_allowed(false, false, false));
        assert!(inline_ocr_host_allowed(false, true, true));
        assert!(!inline_ocr_host_allowed(false, true, false));
        assert!(!inline_ocr_host_allowed(true, true, true));
        assert!(!inline_ocr_host_allowed(true, false, true));
    }
}
pub(crate) fn client_point(p: gpui::Point<gpui::Pixels>, window: &Window) -> Point {
    Point {
        x: f32::from(p.x) * window.scale_factor(),
        y: f32::from(p.y) * window.scale_factor(),
    }
}
fn fit_area(host: (u32, u32), dpi: f32) -> Rect {
    let margin = (24. * dpi).round() as u32;
    let top = (92. * dpi).round() as u32;
    let bottom = (if host.0 as f32 / dpi < 760. {
        260.
    } else {
        196.
    } * dpi)
        .round() as u32;
    Rect {
        x: margin.min(host.0 / 4),
        y: top.min(host.1 / 3),
        width: host.0.saturating_sub(margin * 2).max(1),
        height: host.1.saturating_sub(top + bottom).max(1),
    }
}
fn path_options() -> PathPromptOptions {
    PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(
            tr!(
                "选择图片（PNG / JPG / BMP）",
                "Choose an image (PNG / JPG / BMP)"
            )
            .into(),
        ),
    }
}
pub(crate) fn open(
    image: Image,
    path: Option<PathBuf>,
    cx: &mut App,
) -> Result<WindowHandle<CaptureView>> {
    open_with_document(image, path, None, Tool::Edit, None, cx)
}
/// 长截图裁剪与标注共享图片编辑器；会话只在返回采集或关闭编辑器时交还。
pub(crate) fn open_long(
    image: Image,
    axis: crate::stitch::ScrollAxis,
    placement: Option<crate::export::ClipboardPlacement>,
    on_done: impl FnOnce(crate::long_output::LongOutcome, &mut App) + 'static,
    cx: &mut App,
) -> Result<WindowHandle<CaptureView>> {
    let clip = crate::long_clip::LongClip::new(image.width, image.height, axis)?;
    let snaps = crate::long_clip::BlankSnaps::from_image(&image, axis)?;
    // 横向长图也需要给总览、裁边面板和原生标注栏留下纵向空间。
    let bounds = cx.primary_display().map(|display| {
        let size = display.bounds().size;
        Bounds::centered(
            None,
            gpui::size(
                px((f32::from(size.width) * 0.86).min(1280.)),
                px((f32::from(size.height) * 0.86).min(860.)),
            ),
            cx,
        )
    });
    let handle = open_with_document(image, None, None, Tool::Crop, bounds, cx)?;
    handle.update(cx, |view, window, cx| {
        view.editor.initialize_long_clip(clip)?;
        view.long_clip_ui = Some(crate::long_clip_ui::LongClipUi::new(snaps));
        view.toolbar_config = crate::toolbar_config::ToolbarConfig {
            tools: vec![
                Tool::Rectangle,
                Tool::Arrow,
                Tool::Pen,
                Tool::Text,
                Tool::Mosaic,
                Tool::PixelEraser,
                Tool::Lasso,
            ],
            grouped: true,
        };
        view.long_clip_on_done = Some(Box::new(on_done));
        view.long_clip_placement = placement;
        view.long_clip_fit_width(window, cx);
        view.image_saved = Some(view.image_edit_state());
        window.set_window_title(brand_title!("长截图编辑", "Long capture editor"));
        cx.notify();
        Ok::<_, Box<dyn std::error::Error>>(())
    })??;
    Ok(handle)
}
fn prepare_pin_document(
    image: &Image,
    style: crate::selection_finish::SelectionFinishStyle,
    document: &crate::pin_raster_document::WireRasterDocument,
) -> Result<crate::editor::Editor> {
    crate::pin_workspace::validate_presentation_style(style)?;
    crate::pin_text_source::validate_size(image.width, image.height)?;
    if document.size() != (image.width, image.height)
        || image.bgra.len() != image.width as usize * image.height as usize * 4
    {
        return Err("贴图编辑来源与完整内容文档尺寸不匹配".into());
    }
    let full = Rect::full(image.width, image.height);
    let mut editor = crate::editor::Editor::new(full, Some(full));
    document.restore_into(&mut editor)?;
    Ok(editor)
}
/// 输入只含当前处理后的内容底图；标注文档和选区外观在首帧前分别安装。
pub(crate) fn open_pin_document(
    image: Image,
    style: crate::selection_finish::SelectionFinishStyle,
    document: crate::pin_raster_document::WireRasterDocument,
    cx: &mut App,
) -> Result<WindowHandle<CaptureView>> {
    // 在创建预览纹理与窗口前完成文档检查，失败不会留下空编辑窗口。
    let editor = prepare_pin_document(&image, style, &document)?;
    open_with_document(image, None, Some((editor, style)), Tool::Edit, None, cx)
}
/// 按图片实际大小加上工具栏余量估计窗口，限制在主屏的 86% 以内并居中；
/// 小图也保留足够放下工具栏和属性栏的宽高。
fn initial_bounds(width: u32, height: u32, cx: &App) -> Bounds<gpui::Pixels> {
    let scale = (unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() } as f32 / 96.).max(1.);
    let (display_w, display_h) = cx
        .primary_display()
        .map(|d| {
            let b = d.bounds();
            (f32::from(b.size.width), f32::from(b.size.height))
        })
        .unwrap_or((1440., 900.));
    let fit = |value: f32, low: f32, high: f32| value.min(high).max(low.min(high));
    let window = size(
        px(fit(width as f32 / scale + 160., 860., display_w * 0.86)),
        px(fit(height as f32 / scale + 220., 600., display_h * 0.86)),
    );
    Bounds::centered(None, window, cx)
}
fn open_with_document(
    image: Image,
    path: Option<PathBuf>,
    document: Option<(
        crate::editor::Editor,
        crate::selection_finish::SelectionFinishStyle,
    )>,
    tool: Tool,
    bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<CaptureView>> {
    let bounds = bounds.unwrap_or_else(|| initial_bounds(image.width, image.height, cx));
    let title = match path.as_deref().and_then(|p| p.file_name()) {
        Some(name) => format!(
            "{} - {}",
            name.to_string_lossy(),
            brand_title!("图片编辑", "Image editor")
        ),
        None => brand_title!("图片编辑", "Image editor").to_string(),
    };
    let (source, previews) = crate::canvas_source::CanvasSource::editor_image(image)?;
    let full = source.full();
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                appears_transparent: false,
                ..Default::default()
            }),
            focus: true,
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(&title);
            let focus = cx.focus_handle();
            window.focus(&focus);
            let view = cx.new(|_| {
                let mut view =
                    CaptureView::new(source, previews, focus, Some(full), None, None, Vec::new());
                if let Some((mut editor, style)) = document {
                    editor.restore_tool_styles(view.editor.tool_styles());
                    view.editor = editor;
                    view.finish_style = style;
                    view.finish_preset = crate::selection_finish::FinishPreset::matching(style);
                    view.finish_looks.set(view.finish_preset, style);
                    view.finish_store.reset_session_style(style);
                    view.finish_store.reset_session_preset(view.finish_preset);
                    view.finish_store.reset_session_looks(view.finish_looks);
                }
                view.image_canvas = Some(ImageCanvas {
                    path,
                    viewport: Viewport::default(),
                    auto_fit: true,
                    host: (0, 0),
                    pan: None,
                });
                view.editor.select_tool(tool);
                view.include_cursor = false;
                view.magnifier = false;
                view.guides = false;
                view.show_guides = false;
                view.sync_viewport(window);
                view.image_saved = Some(view.image_edit_state());
                view
            });
            view.update(cx, |view, cx| {
                view.observe_text_core_activation(window, cx);
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |v, cx| {
                    if v.saving || (v.long_clip_ui.is_some() && v.pin_handoff_pending) {
                        return false;
                    }
                    if !v.finish_content_or_status() || !v.confirm_image_discard() {
                        cx.notify();
                        return false;
                    }
                    v.flush_tool_presets();
                    v.long_clip_complete_session(cx);
                    true
                })
                .unwrap_or(true)
            });
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd(window));
            }
            view
        },
    )?;
    Ok(handle)
}
#[cfg(test)]
mod pin_document_entry_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Style},
        editor::Editor,
        pin_raster_document::WireRasterDocument,
        selection_finish::SelectionFinishStyle,
    };
    #[test]
    fn pin_editor_document_prepare_preserves_content_geometry_marks_and_raw_pixels() {
        let full = Rect::full(31, 19);
        let crop = Rect {
            x: 3,
            y: 4,
            width: 25,
            height: 12,
        };
        let mut editor = Editor::new(full, Some(crop));
        let mut mark = Annotation::new(
            Tool::Rectangle,
            Point { x: 5., y: 6. },
            Style {
                filled: true,
                ..Default::default()
            },
        );
        mark.update(Point { x: 24., y: 13. });
        editor.document.insert(mark.clone());
        let document = WireRasterDocument::from_editor(&editor, (31, 19)).unwrap();
        let image = Image {
            width: 31,
            height: 19,
            bgra: [11, 33, 55, 97].repeat(31 * 19),
            readback_ms: 0.,
        };
        let style = SelectionFinishStyle {
            corner_enabled: true,
            shadow_enabled: true,
            border_enabled: true,
            ..Default::default()
        };
        let restored = prepare_pin_document(&image, style, &document).unwrap();
        assert_eq!(restored.canvas, full);
        assert_eq!(restored.crop, Some(crop));
        assert_eq!(restored.document.items()[0].annotation, mark);
        assert_eq!(image.bgra, [11, 33, 55, 97].repeat(31 * 19));
        assert_eq!(document.size(), (31, 19));
    }
    #[test]
    fn pin_editor_document_prepare_accepts_inner_border_and_rejects_mismatched_raw_before_window_creation(
    ) {
        let mut image = Image {
            width: 31,
            height: 19,
            bgra: vec![0; 31 * 19 * 4],
            readback_ms: 0.,
        };
        let document = WireRasterDocument::empty((31, 19)).unwrap();
        assert!(prepare_pin_document(
            &image,
            SelectionFinishStyle {
                border_enabled: true,
                border_inside: true,
                ..Default::default()
            },
            &document
        )
        .is_ok());
        assert!(prepare_pin_document(
            &image,
            SelectionFinishStyle::default(),
            &WireRasterDocument::empty((19, 31)).unwrap()
        )
        .is_err());
        image.bgra.pop();
        assert!(prepare_pin_document(&image, SelectionFinishStyle::default(), &document).is_err());
    }
}
pub fn run(path: Option<PathBuf>, smoke: bool) -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let errors = Rc::new(RefCell::new(None::<String>));
    let final_errors = errors.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty()
                    && !crate::pin_workspace_runtime::is_restoring(cx)
                    && !crate::pin_live_view::has_open_windows()
                {
                    cx.quit();
                }
            })
            .detach();
            let errors = errors.clone();
            if smoke {
                let result = crate::image_smoke::fixture()
                    .and_then(|(image, path)| open(image, Some(path), cx));
                match result {
                    Ok(handle) => {
                        let _ = handle.update(cx, |_, window, cx| {
                            let entity = cx.entity();
                            crate::image_smoke::schedule(&entity, window, cx, errors.clone())
                        });
                    }
                    Err(e) => {
                        *errors.borrow_mut() = Some(e.to_string());
                        cx.quit();
                    }
                }
            } else {
                let receiver = path.is_none().then(|| cx.prompt_for_paths(path_options()));
                cx.spawn(async move |cx| {
                    let result: std::result::Result<_, String> = async {
                        let path = if let Some(path) = path {
                            Some(path)
                        } else {
                            receiver
                                .expect("file picker")
                                .await
                                .map_err(|e| e.to_string())?
                                .map_err(|e| e.to_string())?
                                .and_then(|p| p.into_iter().next())
                        };
                        let Some(path) = path else {
                            return Ok(None);
                        };
                        let (handle, _) = open_file(path, cx)
                            .await
                            .map_err(|error| error.to_string())?;
                        Ok(Some(handle))
                    }
                    .await;
                    let _ = cx.update(|cx| match result {
                        Ok(Some(_)) => {}
                        Ok(None) => cx.quit(),
                        Err(e) => {
                            *errors.borrow_mut() = Some(e);
                            cx.quit();
                        }
                    });
                })
                .detach();
            }
            cx.activate(true);
        });
    let error = final_errors.borrow_mut().take();
    if let Some(error) = error {
        Err(error.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod monitor_preview_pin_tests {
    use super::*;
    use crate::{editor::Editor, preview_middle_pin::PaintedPreview};

    const CANVAS: Rect = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
    };
    const MONITOR: Rect = Rect {
        x: 0,
        y: 0,
        width: 200,
        height: 300,
    };

    #[test]
    fn monitor_preview_pin_preserves_present_gate_and_allows_pointer_outside_fixed_range() {
        let editor = Editor::new(CANVAS, None);
        let painted = PaintedPreview {
            rect: Some(MONITOR),
            previous_presented: None,
            before_present: 7,
        };
        let size = (CANVAS.width << 16) | CANVAS.height;
        let at = |serial, size, manual| {
            screenshot_preview_range(
                &editor,
                CANVAS,
                manual,
                (350, 150),
                painted.presented(CANVAS, serial, size),
                false,
                false,
            )
        };
        assert_eq!(at(0, size, Some(MONITOR)), None);
        assert_eq!(at(7, size, Some(MONITOR)), None);
        assert_eq!(at(8, size + 1, Some(MONITOR)), None);
        assert_eq!(at(8, size, Some(MONITOR)), Some(MONITOR));
        // 自动/固定 Preview 都贴已呈现框；固定来源仍单独核当前 manual 与回执相等。
        assert_eq!(at(8, size, None), Some(MONITOR));
        assert_eq!(editor.crop, None);
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn monitor_preview_pin_rejects_old_presented_range_when_manual_target_changes() {
        let editor = Editor::new(CANVAS, None);
        let next = PaintedPreview {
            rect: Some(CANVAS),
            previous_presented: Some(MONITOR),
            before_present: 9,
        };
        let size = (CANVAS.width << 16) | CANVAS.height;
        let at = |serial, busy, input| {
            screenshot_preview_range(
                &editor,
                CANVAS,
                Some(CANVAS),
                (100, 150),
                next.presented(CANVAS, serial, size),
                busy,
                input,
            )
        };
        assert_eq!(at(9, false, false), None);
        assert_eq!(at(10, true, false), None);
        assert_eq!(at(10, false, true), None);
        assert_eq!(at(10, false, false), Some(CANVAS));
        assert_eq!(editor.crop, None);
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn automatic_middle_keeps_presented_a_outside_event_point_until_b_is_committed() {
        let editor = Editor::new(CANVAS, None);
        let revisions = (editor.document.revision(), editor.document.edit_revision());
        let a = Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
        };
        let b = Rect {
            x: 240,
            y: 120,
            width: 100,
            height: 80,
        };
        let event_point = (260, 150);
        assert!(!a.contains(event_point));
        assert!(b.contains(event_point));
        let size = (CANVAS.width << 16) | CANVAS.height;
        let next = PaintedPreview {
            rect: Some(b),
            previous_presented: Some(a),
            before_present: 10,
        };
        let at = |frame: PaintedPreview, serial, manual, busy, input| {
            screenshot_preview_range(
                &editor,
                CANVAS,
                manual,
                event_point,
                frame.presented(CANVAS, serial, size),
                busy,
                input,
            )
        };
        // GPUI 的 dirty draw 还未 Present：点已经在 B，输出仍使用确实已显示的 A。
        assert_eq!(at(next, 10, None, false, false), Some(a));
        assert_eq!(at(next, 11, None, false, false), Some(b));
        assert_eq!(at(next, 0, None, false, false), None);
        assert_eq!(at(next, 10, None, true, false), None);
        assert_eq!(at(next, 10, None, false, true), None);
        // 自动已显示 A 不能借固定 B 的来源门；未呈现首帧也不能借 pointer/hover 补 B。
        assert_eq!(at(next, 10, Some(b), false, false), None);
        let first = PaintedPreview {
            previous_presented: None,
            ..next
        };
        assert_eq!(at(first, 10, None, false, false), None);
        let invalid = PaintedPreview {
            rect: Some(Rect { x: u32::MAX, ..b }),
            ..first
        };
        assert_eq!(at(invalid, 11, None, false, false), None);
        assert_eq!(editor.crop, None);
        assert_eq!(editor.document.revision(), revisions.0);
        assert_eq!(editor.document.edit_revision(), revisions.1);
        assert!(!editor.document.can_undo());
        assert!(!editor.document.can_redo());
    }

    #[test]
    fn monitor_snap_uses_presented_crop_until_later_preview_frame_is_committed() {
        let canvas = crate::geometry::ScreenRect {
            left: -200,
            top: -20,
            width: CANVAS.width,
            height: CANVAS.height,
        };
        let screens = [
            crate::geometry::ScreenRect {
                width: MONITOR.width,
                ..canvas
            },
            crate::geometry::ScreenRect {
                left: 0,
                width: MONITOR.width,
                ..canvas
            },
        ];
        let point = (-100, 130);
        let size = (CANVAS.width << 16) | CANVAS.height;
        let small_crop = Rect {
            x: 30,
            y: 40,
            width: 100,
            height: 80,
        };
        let crop_frame = PaintedPreview {
            rect: Some(MONITOR),
            previous_presented: Some(small_crop),
            before_present: 10,
        };
        let at = |painted: PaintedPreview, serial| {
            crate::capture_monitor_snap::snap_target(
                canvas,
                &screens,
                point,
                painted.presented(CANVAS, serial, size),
            )
        };
        // 未提交的新 Crop 不能提早变成“当前屏已选”，实际旧小框仍切到当前屏。
        assert_eq!(at(crop_frame, 10), Some(MONITOR));
        assert_eq!(at(crop_frame, 11), Some(CANVAS));
        let full_preview_frame = PaintedPreview {
            rect: Some(CANVAS),
            previous_presented: crop_frame.presented(CANVAS, 11, size),
            before_present: 11,
        };
        assert_eq!(at(full_preview_frame, 11), Some(CANVAS));
        assert_eq!(at(full_preview_frame, 12), Some(MONITOR));
    }
}

#[cfg(test)]
mod middle_pin_tests {
    use super::*;
    use crate::editor::Editor;

    fn selected_editor() -> Editor {
        Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        )
    }

    #[test]
    fn middle_pin_requires_an_existing_crop_inside_the_current_source() {
        let mut editor = selected_editor();
        assert!(screenshot_middle_pin_ready(
            &editor,
            Rect::full(400, 300),
            false,
            false,
        ));
        // 底图换成较小画布后，原选区不能被中键提交。
        assert!(!screenshot_middle_pin_ready(
            &editor,
            Rect::full(200, 150),
            false,
            false,
        ));
        editor.set_crop(None);
        assert!(!screenshot_middle_pin_ready(
            &editor,
            Rect::full(400, 300),
            false,
            false,
        ));
    }

    #[test]
    fn middle_pin_waits_for_crop_release_and_host_input_to_finish() {
        let mut editor = selected_editor();
        let canvas = Rect::full(400, 300);
        editor.pointer_down((160, 130), 4.);
        editor.pointer_move((180, 150));
        assert!(editor.crop.is_some());
        assert!(!screenshot_middle_pin_ready(&editor, canvas, false, false));
        editor.pointer_up((180, 150));
        // 编辑器已提交选区，但宿主左键锁存/交接仍未完成时不能输出。
        assert!(!screenshot_middle_pin_ready(&editor, canvas, true, false));
        // 参数/文字输入仍归其它输入区所有时同样不提前结束。
        assert!(!screenshot_middle_pin_ready(&editor, canvas, false, true));
        assert!(screenshot_middle_pin_ready(&editor, canvas, false, false));
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn middle_pin_does_not_commit_polyline_or_text_drafts() {
        let mut editor = selected_editor();
        let canvas = Rect::full(400, 300);
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((120, 100), 4.);
        editor.pointer_up((120, 100));
        assert!(!editor.is_dragging());
        assert!(editor.has_pending_drawing());
        assert!(!screenshot_middle_pin_ready(&editor, canvas, false, false));
        assert!(editor.has_pending_drawing());
        assert!(!editor.document.can_undo());
        assert!(editor.cancel_gesture());
        editor.select_tool(Tool::Text);
        editor.begin_text(None, Point { x: 120., y: 100. });
        editor.set_text("尚未提交".into());
        assert!(!editor.is_dragging());
        assert!(!editor.has_pending_drawing());
        assert!(!screenshot_middle_pin_ready(&editor, canvas, false, false));
        assert!(editor.text_draft.is_some());
        assert!(!editor.document.can_undo());
        assert!(editor.cancel_text());
        assert!(screenshot_middle_pin_ready(&editor, canvas, false, false));
    }
}
