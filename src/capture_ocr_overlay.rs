//! 全屏冻结图上的识别文字层；临时选择只绘制，持久标记进入既有 Document/Scene。
use crate::{
    annotation::{Bounds as ShapeBounds, Point},
    capture_view::{text_result::OcrTarget, Action, CaptureView},
    gpui_capture::hwnd,
    ocr_markup::{MarkLedger, TextMark},
    pin_text_selection::{KeyboardSelection, NavigationKey, SelectionAnchor, SelectionText},
    recognition::{
        layout::{CopyLayoutMode, OcrCopyLayout},
        RecognizedText,
    },
    toolbar_layout::{PanelRect, ToolbarPanel},
};
#[path = "capture_ocr_find.rs"]
mod find_host;
pub(crate) use find_host::CaptureFindClick;
#[path = "inline_ocr_click.rs"]
pub(crate) mod inline_click;
#[path = "capture_ocr_local_retry.rs"]
pub(crate) mod local_retry_host;
#[path = "capture_ocr_panel_click.rs"]
mod panel_click_host;
pub(crate) use panel_click_host::{
    click_proof as pointer_click_proof, down_proof as pointer_down_proof,
};
#[path = "capture_ocr_filter.rs"]
mod filter_host;
#[path = "capture_ocr_panel.rs"]
mod panel_host;
#[path = "capture_ocr_sensitive.rs"]
mod sensitive_host;
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, AnyElement, Bounds, Context, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Window,
};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::Arc,
};
use windows::Win32::{
    Foundation::HWND,
    UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
};

pub(crate) struct OcrOverlay {
    filter: filter_host::FilterState,
    sensitive: sensitive_host::SensitiveState,
    text: SelectionText,
    copy_layout: OcrCopyLayout,
    target: OcrTarget,
    body_identity: Arc<()>,
    body_revision: u64,
    selected: Option<Range<usize>>,
    keyboard_selection: Option<KeyboardSelection>,
    painted_keyboard_target: Rc<RefCell<Option<OcrTarget>>>,
    painted_projection: Rc<Cell<Option<crate::ocr_projection::OcrProjection>>>,
    projection_epoch: Arc<()>,
    panel_click: Rc<RefCell<panel_click_host::PanelClickState>>,
    anchor: Option<SelectionAnchor>,
    capture: Option<HWND>,
    ledger: MarkLedger,
    show_selection: bool,
    color: Option<u32>,
    panel: panel_host::PanelState,
}

impl OcrOverlay {
    fn new(recognized: &RecognizedText, target: OcrTarget) -> Option<Self> {
        let mut text = SelectionText::from_recognized(recognized);
        if !text.has_geometry() || text.text.is_empty() {
            return None;
        }
        text.translate(target.selected.x as f32, target.selected.y as f32);
        let copy_layout = recognized.copy_layout();
        let panel = panel_host::PanelState::new(&text, &copy_layout);
        Some(Self {
            filter: filter_host::FilterState::new(recognized, &text),
            sensitive: sensitive_host::SensitiveState::new(recognized),
            text,
            copy_layout,
            panel,
            target,
            body_identity: Arc::new(()),
            body_revision: 0,
            selected: None,
            keyboard_selection: None,
            painted_keyboard_target: Rc::new(RefCell::new(None)),
            painted_projection: Rc::new(Cell::new(None)),
            projection_epoch: Arc::new(()),
            panel_click: Rc::new(RefCell::new(Default::default())),
            anchor: None,
            capture: None,
            ledger: MarkLedger::default(),
            show_selection: true,
            color: None,
        })
    }

    fn current(&self, target: Option<&OcrTarget>, text: Option<&str>) -> bool {
        target == Some(&self.target) && text == Some(self.text.text.as_str())
    }

    fn current_body(&self, identity: &Arc<()>, revision: u64) -> bool {
        Arc::ptr_eq(&self.body_identity, identity) && self.body_revision == revision
    }

    fn projection_painted(&self, projection: Option<crate::ocr_projection::OcrProjection>) -> bool {
        projection.is_some() && self.painted_projection.get() == projection
    }

    fn accepts_projection_epoch(&self, epoch: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.projection_epoch, epoch)
    }

    fn invalidate_projection(&mut self) {
        self.panel_click.borrow_mut().cancel();
        self.painted_keyboard_target.borrow_mut().take();
        self.painted_projection.set(None);
        self.projection_epoch = Arc::new(());
    }

    fn publish_projection_paint(
        &self,
        epoch: &Arc<()>,
        projection: crate::ocr_projection::OcrProjection,
        keyboard: bool,
    ) -> bool {
        if !self.accepts_projection_epoch(epoch) {
            return false;
        }
        self.painted_projection.set(Some(projection));
        *self.painted_keyboard_target.borrow_mut() = keyboard.then(|| self.target.clone());
        true
    }

    fn can_follow_history(
        &self,
        target: &OcrTarget,
        document: &crate::document::Document,
        text: Option<&str>,
    ) -> bool {
        self.target.canvas == target.canvas
            && self.target.selected == target.selected
            && self.target.identity == target.identity
            && text == Some(self.text.text.as_str())
            && self.ledger.matches_underlay(document)
    }

    fn end_drag(&mut self) {
        self.filter.cancel();
        self.anchor = None;
        if let Some(own) = self.capture.take() {
            unsafe {
                if GetCapture() == own {
                    let _ = ReleaseCapture();
                }
            }
        }
    }

    fn selected_text(&self) -> Option<String> {
        self.text.selected(self.selected.clone()?)
    }

    /// 原文可复制不代表有可持久标记的字形；按钮和执行共用实际当前四角。
    fn mark_selection_quads(&self) -> crate::Result<Vec<[Point; 4]>> {
        self.text
            .complete_marking_quads(self.selected.clone().unwrap_or(0..0))
    }

    /// 沿原位正文/选区取得既有 payload；只有真实发布成功才返回 true。
    /// 借用整个会话，不更改选择、来源、定位或持久标记，失败可直接重试。
    fn publish_copy(
        &self,
        scope: InlineCopyScope,
        write: impl FnOnce(&str) -> crate::Result<()>,
    ) -> crate::Result<bool> {
        if self.filter.lines_selected() || self.filter.model.has_hidden() {
            let selected_only = matches!(
                scope,
                InlineCopyScope::Selection | InlineCopyScope::Paragraphs
            );
            let text = self
                .filter
                .copy(&self.text.text, self.selected.clone(), selected_only);
            if text.trim().is_empty() {
                return Ok(false);
            }
            write(&text)?;
            return Ok(true);
        }
        match scope {
            InlineCopyScope::Full => {
                if self.text.text.trim().is_empty() {
                    return Ok(false);
                }
                write(&self.text.text)?;
            }
            InlineCopyScope::FullParagraphs => {
                if self.text.text.trim().is_empty() {
                    return Ok(false);
                }
                let text = self
                    .copy_layout
                    .copy(0..self.text.text.len(), CopyLayoutMode::Auto)?;
                write(&text)?;
            }
            InlineCopyScope::Selection => {
                let Some(text) = self.selected_text() else {
                    return Ok(false);
                };
                write(&text)?;
            }
            InlineCopyScope::Paragraphs => {
                let Some(range) = self.selected.clone().filter(|range| !range.is_empty()) else {
                    return Ok(false);
                };
                let text = self.copy_layout.copy(range, CopyLayoutMode::Auto)?;
                if text.is_empty() {
                    return Ok(false);
                }
                write(&text)?;
            }
        }
        Ok(true)
    }

    fn nonempty_selection(&self) -> bool {
        !self.filter.model.selected.is_empty()
            || self
                .selected
                .as_ref()
                .is_some_and(|range| !range.is_empty())
    }

    fn select_range(&mut self, range: Range<usize>) {
        self.filter.model.selected.clear();
        self.filter.changed();
        self.panel_click.borrow_mut().cancel();
        self.keyboard_selection = self.text.keyboard_selection_from_range(range.clone());
        self.selected = Some(range);
        self.show_selection = true;
    }

    fn clear_selection(&mut self) {
        self.filter.model.selected.clear();
        self.filter.changed();
        self.panel_click.borrow_mut().cancel();
        self.selected = None;
        self.keyboard_selection = None;
    }

    fn remember_drag_direction(&mut self, point: Point) {
        self.keyboard_selection = self
            .anchor
            .as_ref()
            .and_then(|anchor| self.text.keyboard_selection_from_drag(anchor, point));
    }

    fn keyboard_target_painted(&self) -> bool {
        self.painted_keyboard_target.borrow().as_ref() == Some(&self.target)
    }

    /// 只为当前真实 paint 读取来源截面，不把旧帧或排序后的范围当成活动端。
    fn caret_segment_for_paint(
        &self,
        target: &OcrTarget,
        instance: &Rc<RefCell<Option<OcrTarget>>>,
        current_target: Option<&OcrTarget>,
        current_text: Option<&str>,
        allowed: bool,
    ) -> Option<[Point; 2]> {
        if !allowed
            || !Rc::ptr_eq(&self.painted_keyboard_target, instance)
            || &self.target != target
            || !self.current(current_target, current_text)
            || !self.show_selection
            || self.anchor.is_some()
            || self.capture.is_some()
        {
            return None;
        }
        let cursor = self
            .keyboard_selection
            .filter(|cursor| self.selected.as_ref() == Some(&cursor.range()))?;
        self.text.caret_segment(cursor.caret_byte())
    }

    fn navigate_selection(&mut self, key: NavigationKey) -> bool {
        let Some(cursor) = self
            .keyboard_selection
            .filter(|cursor| self.selected.as_ref() == Some(&cursor.range()))
        else {
            return false;
        };
        let Some(next) = self.text.navigate_keyboard(cursor, key) else {
            return false;
        };
        self.panel_click.borrow_mut().cancel();
        self.selected = Some(next.range());
        self.keyboard_selection = Some(next);
        self.show_selection = true;
        true
    }
}

/// 菜单和原位层共用精确映射；裸左右保持既有画布/原位键路由。
pub(crate) fn navigation_key(key: &str, m: gpui::Modifiers) -> Option<NavigationKey> {
    if m.alt || m.platform || m.function {
        return None;
    }
    match (key, m.control, m.shift) {
        ("left", true, false) => Some(NavigationKey::PreviousWord),
        ("right", true, false) => Some(NavigationKey::NextWord),
        ("left", true, true) => Some(NavigationKey::ExtendPreviousWord),
        ("right", true, true) => Some(NavigationKey::ExtendNextWord),
        ("home", true, false) => Some(NavigationKey::DocumentHome),
        ("end", true, false) => Some(NavigationKey::DocumentEnd),
        ("home", true, true) => Some(NavigationKey::ExtendDocumentHome),
        ("end", true, true) => Some(NavigationKey::ExtendDocumentEnd),
        ("left", false, true) => Some(NavigationKey::ExtendPrevious),
        ("right", false, true) => Some(NavigationKey::ExtendNext),
        ("up", false, true) => Some(NavigationKey::ExtendPreviousLine),
        ("down", false, true) => Some(NavigationKey::ExtendNextLine),
        ("home", false, false) => Some(NavigationKey::Home),
        ("end", false, false) => Some(NavigationKey::End),
        ("home", false, true) => Some(NavigationKey::ExtendHome),
        ("end", false, true) => Some(NavigationKey::ExtendEnd),
        _ => None,
    }
}

impl Drop for OcrOverlay {
    fn drop(&mut self) {
        self.end_drag();
    }
}

#[derive(Clone, Copy)]
enum InlineCopyScope {
    Full,
    FullParagraphs,
    Selection,
    Paragraphs,
}

fn inline_copy_status(scope: InlineCopyScope, result: crate::Result<bool>) -> String {
    match (scope, result) {
        (InlineCopyScope::Full, Ok(true)) => tr!(
            "已复制全文，可继续标记图片",
            "Full text copied; continue marking the image"
        )
        .into(),
        (InlineCopyScope::FullParagraphs, Ok(true)) => tr!(
            "已按段落复制全文，可继续标记图片",
            "Full text copied as paragraphs; continue marking the image"
        )
        .into(),
        (InlineCopyScope::Selection, Ok(true)) => tr!(
            "已复制选中文字，可继续标记图片",
            "Selected text copied; continue marking the image"
        )
        .into(),
        (InlineCopyScope::Paragraphs, Ok(true)) => tr!(
            "已按段落复制选中文字，原图选字与标记位置保持不变",
            "Selected text copied with joined lines; image positions preserved"
        )
        .into(),
        (InlineCopyScope::Full | InlineCopyScope::FullParagraphs, Ok(false)) => {
            tr!("没有可复制的文字", "No text to copy").into()
        }
        (_, Ok(false)) => tr!("请先在原图上选中文字", "Select text on the image first").into(),
        (InlineCopyScope::Full | InlineCopyScope::FullParagraphs, Err(error)) => trf!(
            "全文复制失败：{error}；识别结果已保留，可按 Ctrl+Shift+C 重试",
            "Full text copy failed: {error}; press Ctrl+Shift+C to retry"
        ),
        (InlineCopyScope::Selection, Err(error)) => trf!(
            "选字复制失败：{error}；选区与标记已保留，可按 Ctrl+C 重试",
            "Selected text copy failed: {error}; press Ctrl+C to retry"
        ),
        (InlineCopyScope::Paragraphs, Err(error)) => trf!(
            "段落复制失败：{error}；选区与标记已保留，可按 Ctrl+Alt+C 重试",
            "Paragraph copy failed: {error}; press Ctrl+Alt+C to retry"
        ),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Operation {
    CopyFull,
    CopyFullParagraphs,
    CopyText,
    CopyParagraphs,
    Mark(TextMark),
    Color(u32),
    FullText,
    Back,
    CopyImage,
    Translate,
    TranslateImage,
}

impl CaptureView {
    pub(crate) fn install_ocr_overlay(&mut self, recognized: &RecognizedText, target: OcrTarget) {
        self.clear_ocr_overlay();
        if !self.inline_ocr_host()
            || self.current_ocr_target().as_ref() != Some(&target)
            || self.current_result_text() != Some(recognized.text.as_str())
        {
            return;
        }
        self.ocr_overlay = OcrOverlay::new(recognized, target);
        // 独立表格直接打开表格页；其余默认段落页（正文中的表格内嵌显示）。
        let view = if self.cached_table_grid().is_some() && self.cached_table_region().is_none() {
            crate::ocr_panel::PanelView::Table
        } else {
            crate::ocr_panel::PanelView::Paragraphs
        };
        if let Some(overlay) = &mut self.ocr_overlay {
            overlay.panel.set_view(view);
        }
        let body = self.current_result_body_identity();
        let revision = self.ocr_find_result_identity().1;
        if let Some(overlay) = &mut self.ocr_overlay {
            overlay.body_identity = body;
            overlay.body_revision = revision;
        }
        self.bind_ocr_sensitive_installation();
        if self.ocr_overlay.is_some() {
            self.status = tr!(
                "拖选文字，空白处拖框选整行；Shift+方向扩选；H/U/W/D 标记，Delete 排除，Ctrl+C 原文，Enter 图片",
                "Drag text to select, drag from blank for lines; Shift+arrows extends; H/U/W/D marks, Delete excludes, Ctrl+C text, Enter image"
            )
            .into();
        }
    }

    /// 图片翻译读取当前原位结果的段落块（画布坐标）及其识别范围。
    pub(crate) fn ocr_overlay_translation_blocks(
        &self,
    ) -> Option<(
        crate::geometry::Rect,
        Vec<crate::image_translate::SourceBlock>,
    )> {
        let overlay = self.ocr_overlay.as_ref()?;
        Some((overlay.target.selected, overlay.panel.translation_blocks()))
    }

    pub(crate) fn clear_ocr_overlay(&mut self) {
        self.cancel_ocr_panel_click();
        self.capture_find_click.borrow_mut().cancel();
        self.ocr_find = None;
        self.local_ocr = None;
        if let Some(mut overlay) = self.ocr_overlay.take() {
            overlay.ledger.clear();
            // Drop 只释放本 HWND 仍持有的捕获，不影响其它应用。
        }
    }

    pub(crate) fn end_ocr_overlay_drag(&mut self) {
        if let Some(overlay) = &mut self.ocr_overlay {
            overlay.end_drag();
        }
    }

    /// 每次真实视口变更都撤销旧点击/paint租约；A→B→A也不能借旧Down复活。
    pub(crate) fn invalidate_inline_ocr_projection(&mut self) {
        self.invalidate_ocr_result_projection();
        self.cancel_ocr_panel_click();
        self.end_ocr_overlay_drag();
        if let Some(overlay) = &mut self.ocr_overlay {
            overlay.invalidate_projection();
        }
        self.capture_find_click.borrow_mut().cancel();
        self.cancel_pending_local_retry_click();
        self.clear_ocr_sensitive_input_receipts();
    }

    /// DPI/client-only 变化也产生新代次，不只观察物理 host 尺寸。
    pub(crate) fn sync_inline_ocr_projection(&mut self, window: &Window) {
        let current = self.ocr_projection(window);
        if self.inline_ocr_host() && self.observed_ocr_projection != current {
            self.invalidate_inline_ocr_projection();
            self.observed_ocr_projection = current;
        }
    }

    fn ocr_overlay_body_current(&self) -> bool {
        self.ocr_overlay.as_ref().is_some_and(|overlay| {
            overlay.current_body(
                &self.current_result_body_identity(),
                self.ocr_find_result_identity().1,
            )
        })
    }

    fn ocr_overlay_projection_painted(&self, window: &Window) -> bool {
        self.ocr_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.projection_painted(self.ocr_projection(window)))
    }
    pub(crate) fn ocr_inline_projection_ready(&self, window: &Window) -> bool {
        self.ocr_overlay_ready() && self.ocr_overlay_projection_painted(window)
    }

    fn ocr_overlay_point(
        &self,
        position: gpui::Point<gpui::Pixels>,
        window: &Window,
    ) -> Option<Point> {
        Some(self.ocr_projection(window)?.source_point(Point {
            x: f32::from(position.x),
            y: f32::from(position.y),
        }))
    }

    pub(crate) fn ocr_overlay_dragging(&self) -> bool {
        self.ocr_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.anchor.is_some() || overlay.filter.dragging())
    }

    pub(crate) fn ocr_overlay_has_selection(&self) -> bool {
        self.ocr_overlay
            .as_ref()
            .is_some_and(OcrOverlay::nonempty_selection)
    }

    pub(crate) fn validate_ocr_overlay(&mut self) -> bool {
        let valid = self.inline_ocr_host()
            && self.ocr_overlay_body_current()
            && self.has_ocr_result()
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay.current(
                    self.current_ocr_target().as_ref(),
                    self.current_result_text(),
                )
            });
        if !valid {
            self.clear_ocr_overlay();
        }
        valid
    }

    /// 仅供明确的 Document Undo/Redo 后调用；普通编辑仍走严格来源失效。
    /// 撤销本会话标记不改变识别下层，已核实的字形/选区/ledger 可继续使用。
    pub(crate) fn refresh_ocr_overlay_after_history(&mut self) -> bool {
        self.cancel_ocr_panel_click();
        self.cancel_ocr_sensitive_preview();
        self.cancel_pending_local_retry_click();
        self.end_ocr_overlay_drag();
        // 新识别的 owner 仍占用 session；不能为了旧定位重建 session，
        // 否则后台回包失去 initial owner，processing 将无法正常清除。
        if self.processing || self.saving || self.pin_handoff_pending {
            self.clear_ocr_overlay();
            return false;
        }
        let Some(target) = self.current_ocr_target() else {
            self.clear_ocr_overlay();
            return false;
        };
        let current = self
            .ocr_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.current(Some(&target), self.current_result_text()));
        let keep = self.inline_ocr_host()
            && self.ocr_overlay_body_current()
            && self.has_ocr_result()
            && (current
                || self.ocr_overlay.as_ref().is_some_and(|overlay| {
                    overlay.can_follow_history(
                        &target,
                        &self.editor.document,
                        self.current_result_text(),
                    )
                }));
        if !keep {
            self.clear_ocr_overlay();
            return false;
        }
        if !current {
            self.ocr_overlay.as_mut().expect("原位文字层已核验").target = target.clone();
            self.refresh_ocr_result_target(target);
        }
        true
    }

    fn ocr_overlay_ready(&self) -> bool {
        self.ocr_overlay.is_some()
            && self.inline_ocr_host()
            && self.inline_ocr_interaction_idle()
            && self.ocr_overlay_body_current()
            && self.result_editor.is_none()
            && !self.ocr_overlay_host_blocked()
            && !self.processing
            && !self.saving
            && !self.pin_handoff_pending
            && self.toolbar_panel == ToolbarPanel::None
            && self.toolbar_drag.is_none()
            && !self.watermark_input_focused
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
            && self.editor.text_draft.is_none()
    }

    fn ocr_overlay_text_output_idle(&self, window: &Window, held: bool) -> bool {
        window.is_window_active()
            && self.ocr_overlay_projection_painted(window)
            && self.focus.is_focused(window)
            && self.has_ocr_result()
            && self.ocr_overlay_ready()
            && self.result_selection_pointer_ready()
            && self.text_copy_completion_is_idle()
            && !self.preview_text_replay
            && !self.color_picker_pending
            && self.ocr_typed_pointer_capture_current(window, held)
    }

    /// 真实原位层全文／表格输出共用的只读门，不要求选区非空。
    pub(crate) fn ocr_overlay_text_output_ready(&self, window: &Window) -> bool {
        self.ocr_overlay_text_output_ready_for(window, false)
    }
    pub(crate) fn ocr_overlay_projection_paint(
        &self,
        window: &Window,
    ) -> Option<crate::ocr_projection::ProjectionPaint> {
        Some(crate::ocr_projection::ProjectionPaint {
            epoch: Arc::clone(&self.ocr_overlay.as_ref()?.projection_epoch),
            projection: self.ocr_projection(window)?,
        })
    }
    pub(crate) fn ocr_overlay_projection_paint_current(
        &self,
        paint: &crate::ocr_projection::ProjectionPaint,
        window: &Window,
    ) -> bool {
        self.ocr_overlay.as_ref().is_some_and(|overlay| {
            paint.current(&overlay.projection_epoch, self.ocr_projection(window))
        })
    }
    pub(crate) fn ocr_overlay_text_output_down_ready(&self, window: &Window) -> bool {
        self.ocr_overlay_text_output_ready_for(window, true)
    }
    fn ocr_overlay_text_output_ready_for(&self, window: &Window, held: bool) -> bool {
        self.ocr_overlay_text_output_idle(window, held)
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay.anchor.is_none()
                    && overlay.capture.is_none()
                    && overlay.keyboard_target_painted()
                    && overlay.current(
                        self.current_ocr_target().as_ref(),
                        self.current_result_text(),
                    )
            })
    }

    pub(crate) fn ocr_overlay_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.validate_ocr_overlay();
        if !self.ocr_overlay_ready() || event.button != MouseButton::Left {
            self.end_ocr_overlay_drag();
            return false;
        }
        let candidate_was_focused = self.local_ocr_candidate_focused(window);
        self.cancel_ocr_sensitive_preview();
        self.clear_find_preview();
        cx.stop_propagation();
        self.end_ocr_overlay_drag();
        if !window.is_window_active() {
            return true;
        }
        if candidate_was_focused {
            // 用户点击回原图选字；丢弃独立候选后输入归属也回原 OCR。
            window.focus(&self.focus);
        }
        if !self.ocr_overlay_projection_painted(window) {
            return true;
        }
        let Some(p) = self.ocr_overlay_point(event.position, window) else {
            return true;
        };
        self.pointer_sample = p;
        self.pointer = p.rounded();
        self.pointer_down_start = None;
        let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
        overlay.show_selection = true;
        overlay.panel.close_menu();
        // 文字上按下是选字；空白处（或已有整行多选时按 Ctrl／Alt）按下是拖框选整行。
        let m = event.modifiers;
        if !overlay.text.has_text_at(p) || (overlay.filter.lines_selected() && (m.control || m.alt))
        {
            if m.control || m.shift || m.alt {
                if let Some(range) = overlay.selected.clone() {
                    let rows = overlay.filter.rows_in(&range);
                    overlay.filter.model.replace_selection(rows);
                }
            }
            overlay.selected = None;
            overlay.keyboard_selection = None;
            overlay.filter.begin(p, event.modifiers);
            let own = hwnd(window);
            unsafe {
                let _ = SetCapture(own);
                if GetCapture() == own {
                    overlay.capture = Some(own);
                } else {
                    overlay.filter.cancel();
                }
            }
            cx.notify();
            return true;
        }
        if let Some((anchor, selected)) = overlay.text.begin(p, event.click_count) {
            overlay.anchor = Some(anchor);
            overlay.select_range(selected);
            let own = hwnd(window);
            unsafe {
                let _ = SetCapture(own);
                if GetCapture() == own {
                    overlay.capture = Some(own);
                } else {
                    overlay.anchor = None;
                }
            }
        } else {
            // 原位模式中的空白点击只清选字，不重裁冻结截图。
            overlay.clear_selection();
        }
        cx.notify();
        true
    }

    pub(crate) fn ocr_overlay_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.validate_ocr_overlay();
        if !self.ocr_overlay_ready() {
            self.end_ocr_overlay_drag();
            return false;
        }
        if !self.ocr_overlay_projection_painted(window) {
            self.end_ocr_overlay_drag();
            return true;
        }
        let Some(p) = self.ocr_overlay_point(event.position, window) else {
            return true;
        };
        self.pointer_sample = p;
        self.pointer = p.rounded();
        let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
        let owns_capture = window.is_window_active()
            && overlay
                .capture
                .is_some_and(|own| unsafe { GetCapture() == own });
        if overlay.filter.dragging() {
            if owns_capture && event.pressed_button == Some(MouseButton::Left) {
                overlay.filter.update(p, false);
            } else {
                overlay.end_drag();
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if owns_capture && event.pressed_button == Some(MouseButton::Left) {
            overlay.remember_drag_direction(p);
        }
        overlay.text.drag(
            &mut overlay.anchor,
            &mut overlay.selected,
            p,
            event.pressed_button == Some(MouseButton::Left),
            owns_capture,
        );
        if overlay.anchor.is_none() {
            overlay.end_drag();
            self.ocr_panel_pointer_moved(p, event.position, window, cx);
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn ocr_overlay_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.validate_ocr_overlay();
        if event.button != MouseButton::Left || !self.ocr_overlay_dragging() {
            return false;
        }
        // 菜单/IME/保存等可在 Down 后出现；只结束已有选字捕获，不把旧 Up
        // 交给截图选区，也不提交被其它交互打断的最后一个坐标。
        if !self.ocr_overlay_ready() {
            self.end_ocr_overlay_drag();
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if !self.ocr_overlay_projection_painted(window) {
            self.end_ocr_overlay_drag();
            cx.stop_propagation();
            return true;
        }
        let Some(p) = self.ocr_overlay_point(event.position, window) else {
            self.end_ocr_overlay_drag();
            return true;
        };
        let own = hwnd(window);
        let current = unsafe { GetCapture() };
        let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
        // Windows backend 可在派发 Up 前释放；其他 HWND 拥有捕获时不提交终点。
        let accepted = window.is_window_active()
            && overlay.capture == Some(own)
            && (current.0.is_null() || current == own);
        if overlay.filter.dragging() {
            if accepted {
                overlay.filter.update(p, true);
            }
            overlay.end_drag();
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if accepted {
            overlay.remember_drag_direction(p);
        }
        overlay
            .text
            .finish_drag(&mut overlay.anchor, &mut overlay.selected, p, accepted);
        overlay.end_drag();
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn ocr_overlay_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.cancel_ocr_panel_click();
        if self.ocr_sensitive_key(event, window, cx) {
            return true;
        }
        let navigation = navigation_key(event.keystroke.key.as_str(), event.keystroke.modifiers);
        // 输入器/菜单仍先拥有自己的键；其余已归属原位层的导航即使来源失效也消费。
        let owns_navigation = navigation.is_some()
            && self.ocr_overlay.is_some()
            && self.result_editor.is_none()
            && self.editor.text_draft.is_none()
            && !self.watermark_input_focused
            && self.toolbar_panel == ToolbarPanel::None
            && !self.ocr_overlay_host_blocked();
        self.validate_ocr_overlay();
        if owns_navigation {
            cx.stop_propagation();
            if !window.is_window_active() {
                if let Some(overlay) = &self.ocr_overlay {
                    overlay.painted_keyboard_target.borrow_mut().take();
                }
                self.end_ocr_overlay_drag();
                return true;
            }
            if !self.ocr_overlay_ready()
                || !self.focus.is_focused(window)
                || !self.result_selection_pointer_ready()
                || !self.text_copy_completion_is_idle()
                || self.preview_text_replay
                || self.color_picker_pending
                || !unsafe { GetCapture() }.0.is_null()
            {
                return true;
            }
            let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
            if overlay.anchor.is_some()
                || overlay.capture.is_some()
                || !overlay.keyboard_target_painted()
            {
                return true;
            }
            if overlay.navigate_selection(navigation.expect("导航映射已核验")) {
                self.clear_find_preview();
                cx.notify();
            }
            // 无合法停点、边界不变或目标缺几何都不能回落为移动冻结截图。
            return true;
        }
        if !self.ocr_overlay_ready() {
            self.end_ocr_overlay_drag();
            return false;
        }
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        let plain = !m.modified();
        let control = m.control && !m.alt && !m.platform && !m.function;
        let paragraphs = m.control && m.alt && !m.shift && !m.platform && !m.function;
        cx.stop_propagation();
        if !window.is_window_active() {
            self.end_ocr_overlay_drag();
            return true;
        }
        if self.image_canvas.is_some()
            && (!self.focus.is_focused(window)
                || !self.ocr_overlay_projection_painted(window)
                || !self.text_copy_completion_is_idle())
        {
            return true;
        }
        if event.is_held
            && matches!(
                key,
                "escape" | "enter" | "c" | "z" | "y" | "h" | "u" | "w" | "d"
            )
        {
            return true;
        }
        match key {
            "escape" if plain => {
                self.clear_find_preview();
                let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
                if overlay.filter.dragging() {
                    overlay.end_drag();
                } else if overlay.panel.filter_open() || overlay.panel.menu_open() {
                    overlay.panel.close_menu();
                } else if overlay.nonempty_selection() {
                    overlay.end_drag();
                    overlay.clear_selection();
                } else {
                    self.ocr_overlay_operation(Operation::Back, window, cx);
                }
            }
            "a" if control => {
                self.clear_find_preview();
                let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
                overlay.end_drag();
                overlay.select_range(0..overlay.text.text.len());
            }
            "c" if paragraphs => self.ocr_overlay_operation(Operation::CopyParagraphs, window, cx),
            "c" if control && m.shift => {
                self.copy_ocr_overlay_text(InlineCopyScope::Full, window);
            }
            "c" if control => self.ocr_overlay_operation(Operation::CopyText, window, cx),
            "h" | "u" | "w" | "d" if plain => {
                let mark = match key {
                    "h" => TextMark::Highlight,
                    "u" => TextMark::Underline,
                    "w" => TextMark::Wave,
                    _ => TextMark::Strike,
                };
                self.ocr_overlay_operation(Operation::Mark(mark), window, cx);
            }
            "enter" if plain => self.ocr_overlay_operation(Operation::CopyImage, window, cx),
            "y" if plain => self.ocr_overlay_operation(Operation::TranslateImage, window, cx),
            "z" if control => {
                self.end_ocr_overlay_drag();
                // 排除绑定的填充命令在栈顶时，选字模式也要连同排除一起撤销。
                if !m.shift
                    && self.ocr_overlay.as_ref().is_some_and(|o| {
                        matches!(
                            o.filter.undo_target(&self.editor.document),
                            filter_host::UndoTarget::Filter { .. }
                        )
                    })
                {
                    self.ocr_filter_action(filter_host::FilterAction::Undo, window, cx);
                    return true;
                }
                self.perform(
                    if m.shift { Action::Redo } else { Action::Undo },
                    window,
                    cx,
                );
                self.refresh_ocr_overlay_after_history();
            }
            "y" if control => {
                self.end_ocr_overlay_drag();
                self.perform(Action::Redo, window, cx);
                self.refresh_ocr_overlay_after_history();
            }
            "s" if control => self.perform(
                if m.shift {
                    Action::QuickSave
                } else {
                    Action::Save
                },
                window,
                cx,
            ),
            "delete" | "backspace" if plain => {
                if self
                    .ocr_overlay
                    .as_ref()
                    .is_some_and(|o| o.nonempty_selection())
                {
                    self.ocr_filter_action(filter_host::FilterAction::Exclude, window, cx);
                }
            }
            _ => {} // 原位文字模式不把未处理按键交给底层绘图工具。
        }
        cx.notify();
        true
    }

    /// 返回是否真实写入了剪贴板。
    fn copy_ocr_overlay_text(&mut self, scope: InlineCopyScope, window: &Window) -> bool {
        let result = self.ocr_overlay.as_ref().map_or(Ok(false), |overlay| {
            overlay.publish_copy(scope, |text| crate::export::copy_text(text, hwnd(window)))
        });
        let copied = matches!(result, Ok(true));
        self.status = inline_copy_status(scope, result);
        copied
    }

    fn ocr_overlay_operation(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_ocr_overlay_operation(operation, window, cx);
    }

    /// 面板主复制按钮与双击行：复制成功即结束截图；失败保留会话、正文和提示供重试。
    /// 键盘 Ctrl+C 仍走 `ocr_overlay_operation`，复制后保持打开以便继续标记。
    fn ocr_overlay_copy_and_close(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.run_ocr_overlay_operation(operation, window, cx) {
            self.perform(Action::Cancel, window, cx);
        }
    }

    /// 返回复制类操作是否真实写入了剪贴板；其它操作恒为 false。
    fn run_ocr_overlay_operation(
        &mut self,
        operation: Operation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.cancel_ocr_panel_click();
        self.validate_ocr_overlay();
        if !self.ocr_overlay_ready() {
            return false;
        }
        self.end_ocr_overlay_drag();
        if !matches!(operation, Operation::Color(_)) {
            self.cancel_ocr_sensitive_preview();
        }
        if let Some(overlay) = self.ocr_overlay.as_mut() {
            overlay.panel.close_menu();
        }
        let mut copied = false;
        match operation {
            Operation::CopyFull => {
                copied = self.copy_ocr_overlay_text(InlineCopyScope::Full, window);
            }
            Operation::CopyFullParagraphs => match self.ocr_mixed_text().filter(|_| {
                !self
                    .ocr_overlay
                    .as_ref()
                    .is_some_and(|o| o.filter.lines_selected() || o.filter.model.has_hidden())
            }) {
                // 有表格时表格部分按制表符分隔，粘到表格软件仍成表。
                Some(text) => {
                    self.status = match crate::export::copy_text(&text, hwnd(window)) {
                        Ok(()) => {
                            copied = true;
                            tr!(
                                "全文已复制，表格部分为制表符分隔",
                                "Copied all text; the table part is tab-separated"
                            )
                            .into()
                        }
                        Err(error) => trf!("复制失败：{}", "Copy failed: {}", error),
                    };
                }
                None => {
                    copied = self.copy_ocr_overlay_text(InlineCopyScope::FullParagraphs, window);
                }
            },
            Operation::CopyText => {
                copied = self.copy_ocr_overlay_text(InlineCopyScope::Selection, window);
            }
            Operation::CopyParagraphs => {
                copied = self.copy_ocr_overlay_text(InlineCopyScope::Paragraphs, window);
            }
            Operation::Color(color) => {
                self.ocr_overlay.as_mut().expect("原位文字层已核验").color = Some(color);
                self.status = tr!(
                    "已选择标记颜色，点击标记按钮应用",
                    "Color selected; click a mark button to apply it"
                )
                .into();
            }
            Operation::FullText => {
                self.cancel_pending_local_retry_click();
                self.toggle_result_editor(window, cx);
            }
            Operation::Translate => {
                // 有选字翻译所选，否则按段落翻译全文；结果都在面板“译文”页。
                match self
                    .ocr_overlay
                    .as_ref()
                    .and_then(|overlay| overlay.selected_text())
                    .filter(|text| !text.trim().is_empty())
                {
                    Some(text) => self.start_translation(text, cx),
                    None => self.translate_ocr_full_text(cx),
                }
                self.set_ocr_panel_view(crate::ocr_panel::PanelView::Translation, cx);
            }
            Operation::TranslateImage => self.translate_image(window, cx),
            Operation::Back => {
                self.clear_ocr_overlay();
                self.dismiss_recognition_result();
                self.status.clear();
            }
            Operation::CopyImage => {
                // 原位蓝层和操作条不进入 Scene，输出前保留会话供失败后继续标记与重试。
                // 普通截图复制完整成功后仍由既有关闭／Drop 路线清理。
                self.perform(Action::Copy, window, cx);
            }
            Operation::Mark(mark) => {
                let result = self.apply_ocr_overlay_mark(mark);
                if let Err(error) = result {
                    self.status = error.to_string();
                }
            }
        }
        cx.notify();
        copied
    }

    fn apply_ocr_overlay_mark(&mut self, mark: TextMark) -> crate::Result<()> {
        let Some(overlay) = &self.ocr_overlay else {
            return Ok(());
        };
        let quads = overlay.mark_selection_quads()?;
        self.apply_ocr_overlay_quads(mark, &quads, |_| true)
    }
    fn apply_ocr_overlay_quads(
        &mut self,
        mark: TextMark,
        quads: &[[Point; 4]],
        authorize: impl FnOnce(&Self) -> bool,
    ) -> crate::Result<()> {
        let Some(overlay) = &self.ocr_overlay else {
            return Ok(());
        };
        let mut style = self.editor.style;
        style.color = overlay.color.unwrap_or(if mark == TextMark::Highlight {
            0xffd54f
        } else {
            style.color
        });
        let edit = overlay
            .ledger
            .prepare(&self.editor.document, mark, quads, style)?;
        if edit.is_empty() {
            return Ok(());
        }
        let all = edit.annotations(&self.editor.document);
        let scene = self.prepared_scene().updated(&all, &self.source_pixels());
        if !scene.errors.is_empty() {
            return Err(scene.errors.join(";").into());
        }
        if !authorize(self) {
            return Err(tr!(
                "识别结果已变化，请重新查找",
                "The recognized text changed; search again"
            )
            .into());
        }
        let overlay = self.ocr_overlay.as_mut().expect("原位文字层已核验");
        if overlay.ledger.commit(&mut self.editor.document, edit) {
            self.editor.reset_group_scope();
            self.scene = Arc::new(scene);
            self.scene_stamp = None;
            if let Some(target) = self.current_ocr_target() {
                self.ocr_overlay.as_mut().expect("原位文字层已核验").target = target.clone();
                self.refresh_ocr_result_target(target);
            }
        }
        self.ocr_overlay
            .as_mut()
            .expect("原位文字层已核验")
            .show_selection = false;
        self.status = tr!(
            "文字标记已应用；可继续选字，Enter 复制图片",
            "Text mark applied; select more text or press Enter to copy the image"
        )
        .into();
        Ok(())
    }

    pub(crate) fn render_ocr_overlay(
        &mut self,
        scale: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.refresh_ocr_panel_click(window, cx);
        self.validate_ocr_overlay();
        self.validate_ocr_sensitive_preview();
        self.validate_ocr_find();
        self.sync_local_ocr(window, cx);
        if !window.is_window_active() {
            self.end_ocr_overlay_drag();
        }
        if !self.ocr_overlay_ready() || !scale.is_finite() || scale <= 0. {
            self.cancel_ocr_panel_click();
            if let Some(overlay) = &self.ocr_overlay {
                overlay.painted_keyboard_target.borrow_mut().take();
                overlay.painted_projection.set(None);
            }
            self.end_ocr_overlay_drag();
            return None;
        }
        let overlay = self.ocr_overlay.as_ref()?;
        let projection = self.ocr_projection(window)?;
        let range = overlay.selected.clone().unwrap_or(0..0);
        let quads = if overlay.filter.lines_selected() {
            overlay.filter.quads(&overlay.text, true)
        } else if overlay.show_selection {
            overlay.text.highlights(range.clone())
        } else {
            Vec::new()
        };
        // 识别层淡色带在下、悬停行在中、选中在上；都只绘制在界面上。
        // 表格页把识别层换成单元格网格，悬停的单元格加深，直观对照识别结构。
        // 段落页的内嵌表格同样联动悬停单元格。
        let table = self.table_overlay_quads(overlay.target.selected);
        let mut layers = match table {
            Some((borders, hover))
                if overlay.panel.view() == crate::ocr_panel::PanelView::Table =>
            {
                vec![
                    (borders, panel_host::TABLE_BORDER),
                    (hover, panel_host::HOVER_FILL),
                    (quads, 0x609dff55),
                ]
            }
            table => vec![
                (
                    overlay.panel.layer_quads(&overlay.filter, &overlay.text),
                    panel_host::LAYER_FILL,
                ),
                (overlay.panel.hover_quads(), panel_host::HOVER_FILL),
                (
                    table.map(|(_, hover)| hover).unwrap_or_default(),
                    panel_host::HOVER_FILL,
                ),
                (quads, 0x609dff55),
            ],
        };
        layers.push((
            overlay
                .filter
                .review_quads(&overlay.text, overlay.panel.filter_open()),
            0xffb45444,
        ));
        layers.push((overlay.filter.marquee(), 0x609dff44));
        let roi = overlay.target.selected;
        let projected_roi = projection.rect(roi);
        let keyboard_target = overlay.target.clone();
        let painted_keyboard_target = overlay.painted_keyboard_target.clone();
        let painted_projection = overlay.painted_projection.clone();
        let projection_epoch = Arc::clone(&overlay.projection_epoch);
        let panel_click = Rc::clone(&overlay.panel_click);
        let keyboard_owner = cx.entity().downgrade();
        let mut root = div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .cursor(gpui::CursorStyle::Arrow);
        if overlay.text.has_text_at(self.pointer_sample) {
            root = root.cursor(gpui::CursorStyle::IBeam);
        }
        root = root.child(
            canvas(
                |_, _, _| (),
                move |_, _, window, cx| {
                    // 真实变更产生独立代次；旧 A 帧不能在 A→B→A 后重授予回执。
                    let geometry_current = keyboard_owner.upgrade().is_some_and(|owner| {
                        let view = owner.read(cx);
                        view.ocr_projection(window) == Some(projection)
                            && view.ocr_overlay_ready()
                            && view.ocr_overlay.as_ref().is_some_and(|overlay| {
                                overlay.accepts_projection_epoch(&projection_epoch)
                                    && Rc::ptr_eq(&overlay.painted_projection, &painted_projection)
                                    && overlay.current(
                                        view.current_ocr_target().as_ref(),
                                        view.current_result_text(),
                                    )
                            })
                    });
                    if !geometry_current {
                        // 旧闭包不能清除同 Rc 上由新代次发布的回执。
                        return;
                    }
                    panel_click_host::observe_panel_click_input(&panel_click, window);
                    window.with_content_mask(
                        Some(gpui::ContentMask {
                            bounds: Bounds::new(
                                point(px(projected_roi.x), px(projected_roi.y)),
                                size(px(projected_roi.width), px(projected_roi.height)),
                            ),
                        }),
                        |window| {
                            for (quads, color) in &layers {
                                for quad in quads {
                                    let vertices = quad.map(|p| {
                                        let p = projection.point(p);
                                        point(px(p.x), px(p.y))
                                    });
                                    let mut path = gpui::Path::new(vertices[0]);
                                    let uv = point(0., 1.);
                                    path.push_triangle(
                                        (vertices[0], vertices[1], vertices[2]),
                                        (uv, uv, uv),
                                    );
                                    path.push_triangle(
                                        (vertices[0], vertices[2], vertices[3]),
                                        (uv, uv, uv),
                                    );
                                    window.paint_path(path, rgba(*color));
                                }
                            }
                        },
                    );
                    // 只在真实蓝层 canvas paint 完成后记录同一 overlay 的真实来源；
                    // 选择范围不绑定此回执，重复扩选无需等待每一次高亮重绘。
                    let (geometry_current, caret) =
                        keyboard_owner.upgrade().map_or((false, None), |owner| {
                            let view = owner.read(cx);
                            let current_target = view.current_ocr_target();
                            let current_text = view.current_result_text();
                            let geometry_current = view.ocr_projection(window) == Some(projection)
                                && view.ocr_overlay_ready()
                                && view.ocr_overlay.as_ref().is_some_and(|overlay| {
                                    overlay.accepts_projection_epoch(&projection_epoch)
                                        && Rc::ptr_eq(
                                            &overlay.painted_keyboard_target,
                                            &painted_keyboard_target,
                                        )
                                        && overlay.target == keyboard_target
                                        && overlay.current(current_target.as_ref(), current_text)
                                });
                            let current = geometry_current
                                && window.is_window_active()
                                && view.ocr_find_focus_owned(window, cx);
                            if geometry_current {
                                if let Some(overlay) = &view.ocr_overlay {
                                    overlay.publish_projection_paint(
                                        &projection_epoch,
                                        projection,
                                        current,
                                    );
                                }
                            }
                            let allowed =
                                current && view.ocr_overlay_text_output_idle(window, false);
                            let caret = view.ocr_overlay.as_ref().and_then(|overlay| {
                                overlay.caret_segment_for_paint(
                                    &keyboard_target,
                                    &painted_keyboard_target,
                                    current_target.as_ref(),
                                    current_text,
                                    allowed,
                                )
                            });
                            (geometry_current, caret)
                        });
                    if !geometry_current {
                        return;
                    }
                    if let Some(segment) = caret {
                        let palette = crate::theme::palette(cx);
                        window.with_content_mask(
                            Some(gpui::ContentMask {
                                bounds: Bounds::new(
                                    point(px(projected_roi.x), px(projected_roi.y)),
                                    size(px(projected_roi.width), px(projected_roi.height)),
                                ),
                            }),
                            |window| {
                                let points = segment.map(|p| {
                                    let p = projection.point(p);
                                    point(px(p.x), px(p.y))
                                });
                                // 恒定 DIP 辅助描边；长度和活动端仍来自原文字截面。
                                for (width, color) in
                                    [(4., palette.background), (2., palette.accent)]
                                {
                                    let mut path = gpui::PathBuilder::stroke(px(width));
                                    path.move_to(points[0]);
                                    path.line_to(points[1]);
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, rgb(color));
                                    }
                                }
                            },
                        );
                    }
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        let work = self.ocr_ui_work(window)?;
        if !range.is_empty() && overlay.anchor.is_none() {
            // 换行可复制，但其蓝尾标记不成为持久标记几何。
            let mark_quads = overlay.mark_selection_quads().ok();
            let selected = mark_quads
                .as_deref()
                .and_then(quad_bounds)
                .or_else(|| quad_bounds(&overlay.text.highlights(range)))
                .unwrap_or(ShapeBounds {
                    x: roi.x as f32,
                    y: roi.y as f32,
                    width: roi.width as f32,
                    height: roi.height as f32,
                });
            let selected = projection.bounds(selected);
            if let Some(bar) =
                self.render_ocr_mark_bar(work, selected, mark_quads.is_some(), window, cx)
            {
                root = root.child(bar);
            }
        }
        let layout = self.ocr_panel_frame(window);
        let dock = layout.map(|layout| {
            let sheet = layout.sheet();
            (sheet.x, sheet.y, sheet.width, sheet.height * 0.6)
        });
        let mut find_bar = None;
        if let Some((hook, bar)) = self.render_ocr_find(scale, window, cx, dock) {
            root = root.child(hook);
            find_bar = Some(bar);
        }
        match layout {
            Some(layout) => {
                if let Some(panel) = self.render_ocr_panel(layout, find_bar, window, cx) {
                    root = root.child(panel);
                }
            }
            // 没有可停靠的选区时查找栏退回显示器左上角。
            None => {
                if let Some(bar) = find_bar {
                    root = root.child(
                        div()
                            .absolute()
                            .left(px(work.x + 8.))
                            .top(px(work.y + 8.))
                            .w(px((work.width - 16.).clamp(1., 420.)))
                            .child(bar),
                    );
                }
            }
        }
        if let Some(candidate) = self.render_local_ocr_candidate(scale, window, cx) {
            root = root.child(candidate);
        }
        if let Some(paint) = self.render_local_retry_click_paint(window, cx) {
            root = root.child(paint);
        }
        if let Some(sensitive) = self.render_ocr_sensitive(scale, window, cx) {
            root = root.child(sensitive);
        }
        Some(root.into_any_element())
    }
}

fn quad_bounds(quads: &[[Point; 4]]) -> Option<ShapeBounds> {
    let mut min = Point {
        x: f32::INFINITY,
        y: f32::INFINITY,
    };
    let mut max = Point {
        x: f32::NEG_INFINITY,
        y: f32::NEG_INFINITY,
    };
    for p in quads.iter().flatten() {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }
    (min.x < max.x && min.y < max.y).then_some(ShapeBounds {
        x: min.x,
        y: min.y,
        width: max.x - min.x,
        height: max.y - min.y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> OcrTarget {
        OcrTarget {
            canvas: crate::geometry::ScreenRect {
                left: -320,
                top: -40,
                width: 320,
                height: 200,
            },
            selected: crate::geometry::Rect {
                x: 40,
                y: 60,
                width: 80,
                height: 40,
            },
            revisions: (4, 7),
            stamp: (11, 13),
            identity: crate::scene::SourceIdentity::of(&crate::scene::SourcePixels {
                fill: None,
                canvas: crate::geometry::Rect::full(320, 200),
                planes: Vec::new(),
                overlay: None,
            }),
        }
    }

    #[test]
    fn overlay_keyboard_mapping_keeps_bare_arrows_and_other_modifiers_out() {
        for (key, shift, expected, ctrl_expected) in [
            ("left", false, None, Some(NavigationKey::PreviousWord)),
            ("right", false, None, Some(NavigationKey::NextWord)),
            (
                "left",
                true,
                Some(NavigationKey::ExtendPrevious),
                Some(NavigationKey::ExtendPreviousWord),
            ),
            (
                "right",
                true,
                Some(NavigationKey::ExtendNext),
                Some(NavigationKey::ExtendNextWord),
            ),
            (
                "home",
                false,
                Some(NavigationKey::Home),
                Some(NavigationKey::DocumentHome),
            ),
            (
                "end",
                false,
                Some(NavigationKey::End),
                Some(NavigationKey::DocumentEnd),
            ),
            (
                "home",
                true,
                Some(NavigationKey::ExtendHome),
                Some(NavigationKey::ExtendDocumentHome),
            ),
            (
                "end",
                true,
                Some(NavigationKey::ExtendEnd),
                Some(NavigationKey::ExtendDocumentEnd),
            ),
            ("up", true, Some(NavigationKey::ExtendPreviousLine), None),
            ("down", true, Some(NavigationKey::ExtendNextLine), None),
            ("up", false, None, None),
            ("down", false, None, None),
            ("enter", false, None, None),
        ] {
            for other in 0..16 {
                let m = gpui::Modifiers {
                    shift,
                    control: other & 1 != 0,
                    alt: other & 2 != 0,
                    platform: other & 4 != 0,
                    function: other & 8 != 0,
                };
                assert_eq!(
                    navigation_key(key, m),
                    match other {
                        0 => expected,
                        1 => ctrl_expected,
                        _ => None,
                    }
                );
            }
        }
    }

    #[test]
    fn overlay_keyboard_paint_receipt_requires_its_current_source_and_instance() {
        let mut old = OcrOverlay::new(&recognized(), target()).unwrap();
        let replacement = OcrOverlay::new(&recognized(), target()).unwrap();
        assert!(!old.keyboard_target_painted());
        // 纯回执模型显式注入；不能当作真实 canvas paint 已发生。
        *old.painted_keyboard_target.borrow_mut() = Some(old.target.clone());
        assert!(old.keyboard_target_painted());
        assert!(!replacement.keyboard_target_painted());
        assert!(!Rc::ptr_eq(
            &old.painted_keyboard_target,
            &replacement.painted_keyboard_target,
        ));
        old.select_range(0..0);
        assert!(old.navigate_selection(NavigationKey::ExtendNext));
        // 回执绑定来源而非每次临时选择，合法连按不必等待高亮重绘。
        assert!(old.keyboard_target_painted());
        assert!(old.navigate_selection(NavigationKey::ExtendNext));
        assert!(old.keyboard_target_painted());
        old.target.revisions.0 += 1;
        assert!(!old.keyboard_target_painted());
        old.painted_keyboard_target.borrow_mut().take();
        assert!(!old.keyboard_target_painted());
    }

    #[test]
    fn overlay_keyboard_navigation_preserves_text_geometry_and_owned_mark_history() {
        let (mut overlay, document) = marked_overlay();
        let original = overlay.text.text.clone();
        let source = overlay.target.clone();
        let geometry = overlay.text.highlights(0..original.len());
        let revisions = (document.revision(), document.edit_revision());
        overlay.select_range(0..original.len());
        assert!(overlay.navigate_selection(NavigationKey::Home));
        assert_eq!(overlay.selected, Some(7..7));
        assert!(overlay.navigate_selection(NavigationKey::ExtendEnd));
        assert_eq!(overlay.selected_text().as_deref(), Some("next"));
        assert_eq!(overlay.text.text, original);
        assert!(overlay.target == source);
        assert_eq!(overlay.text.highlights(0..original.len()), geometry);
        assert_eq!((document.revision(), document.edit_revision()), revisions);
        assert!(overlay.ledger.matches_underlay(&document));
        assert!(overlay.show_selection);
        overlay.clear_selection();
        assert!(overlay.keyboard_selection.is_none());
        assert!(overlay.selected.is_none());
        assert!(!overlay.navigate_selection(NavigationKey::ExtendNext));
    }

    #[test]
    fn overlay_keyboard_reverse_drag_shrinks_and_crosses_the_fixed_anchor() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        let (anchor, selected) = overlay.text.begin(Point { x: 55., y: 85. }, 1).unwrap();
        overlay.select_range(selected);
        overlay.anchor = Some(anchor);
        let end = Point { x: 45., y: 85. };
        overlay.remember_drag_direction(end);
        overlay
            .text
            .finish_drag(&mut overlay.anchor, &mut overlay.selected, end, true);
        assert_eq!(overlay.selected_text().as_deref(), Some("ne"));
        assert!(overlay.navigate_selection(NavigationKey::ExtendNext));
        assert_eq!(overlay.selected_text().as_deref(), Some("e"));
        assert!(overlay.navigate_selection(NavigationKey::ExtendNext));
        assert!(overlay.selected_text().is_none());
        assert!(overlay.navigate_selection(NavigationKey::ExtendNext));
        assert_eq!(overlay.selected_text().as_deref(), Some("x"));
    }

    fn current_caret(overlay: &OcrOverlay) -> Option<[Point; 2]> {
        overlay.caret_segment_for_paint(
            &overlay.target,
            &overlay.painted_keyboard_target,
            Some(&overlay.target),
            Some(overlay.text.text.as_str()),
            true,
        )
    }

    fn assert_caret(actual: Option<[Point; 2]>, expected: [(f32, f32); 2]) {
        let actual = actual.expect("合法活动端须有实际来源截面");
        for (point, (x, y)) in actual.into_iter().zip(expected) {
            assert!((point.x - x).abs() < 0.001 && (point.y - y).abs() < 0.001);
        }
    }

    fn projected_caret_text(width: u32, height: u32, quad: [(f32, f32); 4]) -> RecognizedText {
        RecognizedText {
            text: "AB".into(),
            regions: Vec::new(),
            words: vec![crate::recognition::RecognizedWord {
                text: "AB".into(),
                rect: crate::geometry::Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                line: 0,
                horizontal_direction: Default::default(),
                geometry: Some(crate::recognition::TextGeometry {
                    quad: quad.map(|(x, y)| Point { x, y }),
                }),
            }],
            lines: Vec::new(),
        }
    }

    #[test]
    fn overlay_caret_current_paint_keeps_collapsed_end_and_read_only_history() {
        let (mut overlay, document) = marked_overlay();
        let text = overlay.text.text.clone();
        let target = overlay.target.clone();
        let highlights = overlay.text.highlights(0..text.len());
        let marks = overlay.text.marking_quads(0..text.len());
        let items = document.items().to_vec();
        let revisions = (document.revision(), document.edit_revision());
        overlay.select_range(0..6);
        assert_caret(current_caret(&overlay), [(85., 65.), (85., 75.)]);
        assert_eq!(overlay.selected_text().as_deref(), Some("中e\u{301}"));
        assert!(overlay.navigate_selection(NavigationKey::Home));
        assert_eq!(overlay.selected, Some(0..0));
        assert_caret(current_caret(&overlay), [(45., 65.), (45., 75.)]);
        assert!(overlay.navigate_selection(NavigationKey::End));
        assert_eq!(overlay.selected, Some(6..6));
        assert_caret(current_caret(&overlay), [(85., 65.), (85., 75.)]);
        assert_eq!(overlay.text.text, text);
        assert!(overlay.target == target);
        assert_eq!(overlay.text.highlights(0..text.len()), highlights);
        assert_eq!(overlay.text.marking_quads(0..text.len()), marks);
        assert_eq!(document.items(), items);
        assert_eq!((document.revision(), document.edit_revision()), revisions);
        assert!(overlay.ledger.matches_underlay(&document));
        // 本模型调用真实几何/显示门，不把纯调用冒称实际 canvas paint。
        assert!(overlay.painted_keyboard_target.borrow().is_none());
    }

    #[test]
    fn overlay_caret_display_gate_hides_nonidle_selection_drag_and_range_mismatch() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        assert!(current_caret(&overlay).is_none());
        overlay.select_range(0..6);
        assert!(current_caret(&overlay).is_some());
        assert!(overlay
            .caret_segment_for_paint(
                &overlay.target,
                &overlay.painted_keyboard_target,
                Some(&overlay.target),
                Some(overlay.text.text.as_str()),
                false,
            )
            .is_none());
        overlay.show_selection = false;
        assert!(current_caret(&overlay).is_none());
        overlay.show_selection = true;
        let (anchor, _) = overlay.text.begin(Point { x: 55., y: 70. }, 1).unwrap();
        overlay.anchor = Some(anchor);
        assert!(current_caret(&overlay).is_none());
        overlay.anchor = None;
        overlay.selected = Some(0..3);
        assert!(current_caret(&overlay).is_none());
        overlay.select_range(0..6);
        assert!(current_caret(&overlay).is_some());
        overlay.clear_selection();
        assert!(current_caret(&overlay).is_none());
    }

    #[test]
    fn overlay_caret_paint_rejects_changed_target_text_and_replacement_instance() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        overlay.select_range(0..0);
        let painted_target = overlay.target.clone();
        let painted_instance = overlay.painted_keyboard_target.clone();
        let text = overlay.text.text.clone();
        let query = |target: Option<&OcrTarget>, text: Option<&str>| {
            overlay.caret_segment_for_paint(&painted_target, &painted_instance, target, text, true)
        };
        assert!(query(Some(&painted_target), Some(&text)).is_some());
        let mut changed = painted_target.clone();
        changed.selected.x += 1;
        assert!(query(Some(&changed), Some(&text)).is_none());
        changed = painted_target.clone();
        changed.revisions.0 += 1;
        assert!(query(Some(&changed), Some(&text)).is_none());
        changed = painted_target.clone();
        changed.stamp.0 += 1;
        assert!(query(Some(&changed), Some(&text)).is_none());
        changed = painted_target.clone();
        changed.canvas.left -= 1;
        assert!(query(Some(&changed), Some(&text)).is_none());
        changed = painted_target.clone();
        changed.identity = crate::scene::SourceIdentity::of(&crate::scene::SourcePixels {
            fill: None,
            canvas: crate::geometry::Rect::full(319, 200),
            planes: Vec::new(),
            overlay: None,
        });
        assert!(query(Some(&changed), Some(&text)).is_none());
        assert!(query(Some(&painted_target), Some("changed")).is_none());
        assert!(query(None, Some(&text)).is_none());
        assert!(query(Some(&painted_target), None).is_none());
        let mut replacement = OcrOverlay::new(&recognized(), painted_target.clone()).unwrap();
        replacement.select_range(0..0);
        assert!(replacement
            .caret_segment_for_paint(
                &painted_target,
                &painted_instance,
                Some(&painted_target),
                Some(&text),
                true,
            )
            .is_none());
        overlay.target.revisions.0 += 1;
        assert!(overlay
            .caret_segment_for_paint(
                &painted_target,
                &painted_instance,
                Some(&overlay.target),
                Some(&text),
                true,
            )
            .is_none());
    }

    #[test]
    fn overlay_caret_projected_reverse_drag_uses_true_active_end_not_sorted_range() {
        let value = projected_caret_text(40, 20, [(40., 20.), (0., 20.), (0., 0.), (40., 0.)]);
        let mut overlay = OcrOverlay::new(&value, target()).unwrap();
        let (anchor, selected) = overlay.text.begin(Point { x: 45., y: 70. }, 1).unwrap();
        overlay.select_range(selected);
        overlay.anchor = Some(anchor);
        let end = Point { x: 75., y: 70. };
        overlay.remember_drag_direction(end);
        overlay
            .text
            .finish_drag(&mut overlay.anchor, &mut overlay.selected, end, true);
        assert_eq!(overlay.selected, Some(0..2));
        assert_eq!(overlay.keyboard_selection.unwrap().caret_byte(), 0);
        assert_caret(current_caret(&overlay), [(80., 80.), (80., 60.)]);
        assert!(overlay.navigate_selection(NavigationKey::ExtendNext));
        assert_eq!(overlay.selected, Some(1..2));
        assert_caret(current_caret(&overlay), [(60., 80.), (60., 60.)]);
        assert!(overlay.navigate_selection(NavigationKey::End));
        assert_eq!(overlay.selected, Some(2..2));
        assert_caret(current_caret(&overlay), [(40., 80.), (40., 60.)]);
    }

    #[test]
    fn overlay_caret_vertical_slanted_and_perspective_keep_source_cross_sections() {
        for (width, height, quad, expected) in [
            (
                20,
                40,
                [(20., 0.), (20., 40.), (0., 40.), (0., 0.)],
                [(60., 80.), (40., 80.)],
            ),
            (
                20,
                40,
                [(0., 40.), (0., 0.), (20., 0.), (20., 40.)],
                [(40., 80.), (60., 80.)],
            ),
            (
                60,
                40,
                [(10., 0.), (60., 20.), (50., 40.), (0., 20.)],
                [(75., 70.), (65., 90.)],
            ),
            (
                60,
                20,
                [(0., 0.), (60., 0.), (40., 20.), (10., 20.)],
                [(70., 60.), (65., 80.)],
            ),
        ] {
            let mut overlay =
                OcrOverlay::new(&projected_caret_text(width, height, quad), target()).unwrap();
            overlay.select_range(0..1);
            assert_caret(current_caret(&overlay), expected);
            assert_eq!(overlay.selected, Some(0..1));
            assert_eq!(overlay.selected_text().as_deref(), Some("A"));
        }
    }

    #[test]
    fn overlay_caret_missing_and_conflicting_word_geometry_never_interpolates() {
        let mut value = projected_caret_text(40, 20, [(0., 0.), (40., 0.), (40., 20.), (0., 20.)]);
        value.text = "A  B".into();
        value.words[0].text = "A".into();
        value.words[0].rect.width = 10;
        value.words[0].geometry = None;
        value.words.push(crate::recognition::RecognizedWord {
            text: "B".into(),
            rect: crate::geometry::Rect {
                x: 50,
                ..value.words[0].rect
            },
            line: 0,
            horizontal_direction: Default::default(),
            geometry: None,
        });
        let mut overlay = OcrOverlay::new(&value, target()).unwrap();
        overlay.select_range(2..2);
        assert!(current_caret(&overlay).is_none());
        overlay.select_range(1..1);
        assert_caret(current_caret(&overlay), [(50., 60.), (50., 80.)]);
        overlay.select_range(3..3);
        assert_caret(current_caret(&overlay), [(90., 60.), (90., 80.)]);
        value.text = "AB".into();
        let mut seam = OcrOverlay::new(&value, target()).unwrap();
        seam.select_range(1..1);
        assert!(current_caret(&seam).is_none());
    }

    #[test]
    fn inline_copy_full_body_preserves_unicode_and_crlf_without_a_selection() {
        for body in ["中e\u{301}\nnext", "中e\u{301}\r\nnext"] {
            let mut value = recognized();
            value.text = body.into();
            let overlay = OcrOverlay::new(&value, target()).unwrap();
            assert!(overlay.selected.is_none());
            let mut published = Vec::new();
            let result = overlay.publish_copy(InlineCopyScope::Full, |text| {
                published.push(text.to_owned());
                Ok(())
            });
            let status = inline_copy_status(InlineCopyScope::Full, result);
            assert_eq!(published, [body]);
            assert_eq!(
                status,
                tr!(
                    "已复制全文，可继续标记图片",
                    "Full text copied; continue marking the image"
                )
            );
            assert_eq!(overlay.text.text, body);
            assert!(overlay.selected.is_none());
        }
    }

    #[test]
    fn inline_copy_selection_preserves_combining_text_and_selected_whitespace() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        for (range, expected) in [(3..6, "e\u{301}"), (6..7, "\n")] {
            overlay.select_range(range.clone());
            let mut published = Vec::new();
            assert!(overlay
                .publish_copy(InlineCopyScope::Selection, |text| {
                    published.push(text.to_owned());
                    Ok(())
                })
                .unwrap());
            assert_eq!(published, [expected]);
            assert_eq!(overlay.selected, Some(range));
            assert_eq!(overlay.text.text, "中e\u{301}\nnext");
        }
    }

    #[test]
    fn inline_copy_paragraphs_uses_existing_geometry_layout_without_rewriting_body() {
        let mut value = recognized();
        // 上一行写满 10 倍行高宽的一栏，下一行紧随其下：可信续行。
        value.words[0].rect.width = 100;
        value.words[1].rect.width = 40;
        value.words[1].rect.y = 18;
        value.lines[0].rect = Some(value.words[0].rect);
        value.lines[1].rect = Some(value.words[1].rect);
        let mut overlay = OcrOverlay::new(&value, target()).unwrap();
        overlay.select_range(3..value.text.len());
        let original_geometry = overlay.text.highlights(0..value.text.len());
        let original_target = overlay.target.clone();
        let original_keyboard = overlay.keyboard_selection;
        let mut published = Vec::new();
        let result = overlay.publish_copy(InlineCopyScope::Paragraphs, |text| {
            published.push(text.to_owned());
            Ok(())
        });
        assert_eq!(published, ["e\u{301} next"]);
        assert_eq!(
            inline_copy_status(InlineCopyScope::Paragraphs, result),
            tr!(
                "已按段落复制选中文字，原图选字与标记位置保持不变",
                "Selected text copied with joined lines; image positions preserved"
            )
        );
        assert_eq!(overlay.text.text, value.text);
        assert_eq!(overlay.copy_layout.text(), value.text);
        assert_eq!(overlay.selected, Some(3..value.text.len()));
        assert_eq!(overlay.keyboard_selection, original_keyboard);
        assert!(overlay.target == original_target);
        assert_eq!(
            overlay.text.highlights(0..value.text.len()),
            original_geometry
        );
    }

    #[test]
    fn inline_copy_failed_publication_keeps_all_three_payloads_selection_and_mark_history() {
        for (scope, range, expected, shortcut) in [
            (
                InlineCopyScope::Full,
                7..11,
                "中e\u{301}\nnext",
                "Ctrl+Shift+C",
            ),
            (InlineCopyScope::Selection, 3..6, "e\u{301}", "Ctrl+C"),
            (
                InlineCopyScope::Paragraphs,
                3..11,
                "e\u{301}\nnext",
                "Ctrl+Alt+C",
            ),
        ] {
            let (mut overlay, mut document) = marked_overlay();
            overlay.select_range(range.clone());
            let body = overlay.text.text.clone();
            let target = overlay.target.clone();
            let keyboard = overlay.keyboard_selection;
            let show_selection = overlay.show_selection;
            let geometry = overlay.text.highlights(0..body.len());
            let items = document.items().to_vec();
            let revisions = (document.revision(), document.edit_revision());
            let mut failed_payload = Vec::new();
            let error = overlay
                .publish_copy(scope, |text| {
                    failed_payload.push(text.to_owned());
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "inline publication denied",
                    )
                    .into())
                })
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let failed_status = inline_copy_status(scope, Err(error));
            assert!(failed_status.contains("inline publication denied"));
            assert!(failed_status.contains(shortcut));
            assert_eq!(failed_payload, [expected]);
            let mut retry_payload = Vec::new();
            let retry = overlay.publish_copy(scope, |text| {
                retry_payload.push(text.to_owned());
                Ok(())
            });
            let copied_status = inline_copy_status(scope, retry);
            assert_ne!(failed_status, copied_status);
            assert_eq!(retry_payload, failed_payload);
            assert_eq!(overlay.text.text, body);
            assert_eq!(overlay.copy_layout.text(), body);
            assert_eq!(overlay.selected, Some(range));
            assert_eq!(overlay.keyboard_selection, keyboard);
            assert_eq!(overlay.show_selection, show_selection);
            assert!(overlay.target == target);
            assert_eq!(overlay.text.highlights(0..body.len()), geometry);
            assert_eq!(document.items(), items);
            assert_eq!((document.revision(), document.edit_revision()), revisions);
            assert!(document.can_undo());
            assert!(!document.can_redo());
            assert!(overlay.ledger.matches_underlay(&document));
            document.undo();
            assert!(document.items().is_empty());
            document.redo();
            assert_eq!(document.items(), items);
            assert!(overlay.ledger.matches_underlay(&document));
        }
    }

    #[test]
    fn inline_copy_empty_body_and_missing_selection_never_publish_or_report_success() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        for selected in [None, Some(0..0)] {
            overlay.selected = selected;
            for scope in [InlineCopyScope::Selection, InlineCopyScope::Paragraphs] {
                let result = overlay.publish_copy(scope, |_| -> crate::Result<()> {
                    panic!("没有选区不能发布全文或空 payload")
                });
                assert_eq!(
                    inline_copy_status(scope, result),
                    tr!("请先在原图上选中文字", "Select text on the image first")
                );
            }
        }
        for selected in [1..2, 0..overlay.text.text.len() + 1] {
            overlay.selected = Some(selected);
            assert!(!overlay
                .publish_copy(InlineCopyScope::Selection, |_| -> crate::Result<()> {
                    panic!("非法 UTF-8 选区不能回退发布全文")
                })
                .unwrap());
        }
        // 独立核发布函数的空正文边界，不声称这些状态通过宿主来源/定位门。
        for body in ["", " \r\n\t"] {
            overlay.text.text = body.into();
            let result = overlay.publish_copy(InlineCopyScope::Full, |_| -> crate::Result<()> {
                panic!("空白正文不能触发发布")
            });
            assert_eq!(
                inline_copy_status(InlineCopyScope::Full, result),
                tr!("没有可复制的文字", "No text to copy")
            );
            assert_eq!(overlay.text.text, body);
        }
    }

    #[test]
    fn inline_copy_paragraph_layout_failure_keeps_range_and_skips_publication() {
        let (mut overlay, document) = marked_overlay();
        overlay.selected = Some(1..2);
        let body = overlay.text.text.clone();
        let target = overlay.target.clone();
        let items = document.items().to_vec();
        let revisions = (document.revision(), document.edit_revision());
        let result = overlay.publish_copy(InlineCopyScope::Paragraphs, |_| -> crate::Result<()> {
            panic!("排版拒绝原文范围时不能发布")
        });
        assert!(result.is_err());
        let status = inline_copy_status(InlineCopyScope::Paragraphs, result);
        assert!(status.contains("Ctrl+Alt+C"));
        assert_eq!(overlay.selected, Some(1..2));
        assert_eq!(overlay.text.text, body);
        assert!(overlay.target == target);
        assert_eq!(document.items(), items);
        assert_eq!((document.revision(), document.edit_revision()), revisions);
        assert!(overlay.ledger.matches_underlay(&document));
    }

    #[test]
    fn mark_eligibility_keeps_lf_crlf_and_spaces_copyable_without_marking() {
        for (body, range, payload) in [
            ("中e\u{301}\nnext", 6..7, "\n"),
            ("中e\u{301}\r\nnext", 6..8, "\r\n"),
            ("中e\u{301} \nnext", 6..7, " "),
        ] {
            let mut value = recognized();
            value.text = body.into();
            value.lines[0].text = body
                .split('\n')
                .next()
                .unwrap()
                .trim_end_matches('\r')
                .into();
            let mut overlay = OcrOverlay::new(&value, target()).unwrap();
            overlay.select_range(range.clone());
            let source = overlay.target.clone();
            let keyboard = overlay.keyboard_selection;
            assert!(overlay.mark_selection_quads().is_err());
            for scope in [InlineCopyScope::Selection, InlineCopyScope::Paragraphs] {
                let mut published = Vec::new();
                assert!(overlay
                    .publish_copy(scope, |text| {
                        published.push(text.to_owned());
                        Ok(())
                    })
                    .unwrap());
                assert_eq!(published, [payload]);
            }
            assert_eq!(overlay.text.text, body);
            assert_eq!(overlay.selected, Some(range));
            assert_eq!(overlay.keyboard_selection, keyboard);
            assert!(overlay.target == source);
        }
    }

    #[test]
    fn mark_eligibility_missing_line_geometry_preserves_the_find_selected_payload() {
        let mut value = recognized();
        value.words[1].rect.width = 0;
        let mut overlay = OcrOverlay::new(&value, target()).unwrap();
        assert!(overlay.text.has_geometry());
        // 实际 Find Select 调用的原位入口允许缺几何 byte 选择；不冒称运行了查询 UI。
        overlay.select_range(7..11);
        let error = overlay.mark_selection_quads().unwrap_err();
        assert_eq!(
            error.to_string(),
            tr!(
                "当前选区没有可标记的字形定位；文字仍可复制，请重选或重新识别",
                "No markable glyphs; text can still be copied. Reselect or retry OCR."
            )
        );
        let mut published = Vec::new();
        assert!(overlay
            .publish_copy(InlineCopyScope::Selection, |text| {
                published.push(text.to_owned());
                Ok(())
            })
            .unwrap());
        assert_eq!(published, ["next"]);
        assert_eq!(overlay.selected, Some(7..11));
        assert_eq!(overlay.text.text, value.text);
        assert!(overlay.keyboard_selection.is_none());
        // 不保留上一份选择的按钮资格，执行时重新取得实际四角。
        overlay.select_range(0..6);
        assert!(!overlay.mark_selection_quads().unwrap().is_empty());
        overlay.select_range(7..11);
        assert!(overlay.mark_selection_quads().is_err());
    }

    #[test]
    fn mark_eligibility_partial_line_geometry_rejects_all_four_marks_before_history() {
        let mut value = recognized();
        value.words[1].rect.width = 0;
        let mut overlay = OcrOverlay::new(&value, target()).unwrap();
        let (_, mut document) = marked_overlay();
        document.undo();
        overlay.select_range(0..value.text.len());
        let selected = overlay.selected.clone();
        let source = overlay.target.clone();
        let items = document.items().to_vec();
        let revisions = (document.revision(), document.edit_revision());
        let history = (document.can_undo(), document.can_redo());
        let underlay_matches = overlay.ledger.matches_underlay(&document);
        for mark in [
            TextMark::Highlight,
            TextMark::Underline,
            TextMark::Wave,
            TextMark::Strike,
        ] {
            let result = overlay.mark_selection_quads().and_then(|quads| {
                overlay
                    .ledger
                    .prepare(&document, mark, &quads, crate::annotation::Style::default())
            });
            assert!(result.is_err());
            assert_eq!(document.items(), items);
            assert_eq!((document.revision(), document.edit_revision()), revisions);
            assert_eq!((document.can_undo(), document.can_redo()), history);
            assert_eq!(overlay.ledger.matches_underlay(&document), underlay_matches);
        }
        let mut copied = Vec::new();
        assert!(overlay
            .publish_copy(InlineCopyScope::Selection, |text| {
                copied.push(text.to_owned());
                Ok(())
            })
            .unwrap());
        assert_eq!(copied, [value.text]);
        assert_eq!(overlay.selected, selected);
        assert!(overlay.target == source);
        assert!(document.can_redo());
    }

    #[test]
    fn mark_eligibility_missing_selection_and_partial_combining_character_never_mark() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        assert!(overlay.mark_selection_quads().is_err());
        overlay.select_range(3..4);
        assert_eq!(overlay.selected_text().as_deref(), Some("e"));
        assert!(overlay.mark_selection_quads().is_err());
        overlay.select_range(3..6);
        assert_eq!(overlay.mark_selection_quads().unwrap().len(), 1);
    }

    #[test]
    fn mark_eligibility_valid_quads_keep_four_marks_as_one_undo_each() {
        for mark in [
            TextMark::Highlight,
            TextMark::Underline,
            TextMark::Wave,
            TextMark::Strike,
        ] {
            let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
            overlay.select_range(0..overlay.text.text.len());
            let quads = overlay.mark_selection_quads().unwrap();
            assert_eq!(quads.len(), 2);
            let mut document = crate::document::Document::default();
            let edit = overlay
                .ledger
                .prepare(&document, mark, &quads, crate::annotation::Style::default())
                .unwrap();
            assert!(!edit.is_empty());
            assert!(overlay.ledger.commit(&mut document, edit));
            let items = document.items().to_vec();
            assert!(!items.is_empty());
            assert_eq!(document.edit_revision(), 1);
            assert!(document.can_undo());
            document.undo();
            assert!(document.items().is_empty());
            assert!(!document.can_undo());
            document.redo();
            assert_eq!(document.items(), items);
            assert_eq!(overlay.selected, Some(0..overlay.text.text.len()));
            assert!(overlay.ledger.matches_underlay(&document));
        }
    }

    #[test]
    fn mark_eligibility_rejection_preserves_owned_marks_selection_and_redo() {
        for undo_first in [false, true] {
            let (mut overlay, mut document) = marked_overlay();
            if undo_first {
                document.undo();
                overlay.target = target_after_history(&overlay, &document);
            }
            overlay.select_range(6..7);
            let body = overlay.text.text.clone();
            let target = overlay.target.clone();
            let geometry = overlay.text.highlights(0..body.len());
            let keyboard = overlay.keyboard_selection;
            let show_selection = overlay.show_selection;
            let items = document.items().to_vec();
            let revisions = (document.revision(), document.edit_revision());
            let history = (document.can_undo(), document.can_redo());
            assert!(overlay.mark_selection_quads().is_err());
            assert_eq!(overlay.selected_text().as_deref(), Some("\n"));
            assert_eq!(overlay.text.text, body);
            assert_eq!(overlay.copy_layout.text(), body);
            assert_eq!(overlay.selected, Some(6..7));
            assert_eq!(overlay.keyboard_selection, keyboard);
            assert_eq!(overlay.show_selection, show_selection);
            assert!(overlay.target == target);
            assert_eq!(overlay.text.highlights(0..body.len()), geometry);
            assert_eq!(document.items(), items);
            assert_eq!((document.revision(), document.edit_revision()), revisions);
            assert_eq!((document.can_undo(), document.can_redo()), history);
            assert!(overlay.ledger.matches_underlay(&document));
            overlay.select_range(0..body.len());
            assert!(!overlay.mark_selection_quads().unwrap().is_empty());
            assert_eq!(document.items(), items);
            assert_eq!((document.can_undo(), document.can_redo()), history);
        }
    }

    fn recognized() -> RecognizedText {
        RecognizedText {
            text: "中e\u{301}\nnext".into(),
            regions: Vec::new(),
            words: vec![
                crate::recognition::RecognizedWord {
                    text: "中e\u{301}".into(),
                    rect: crate::geometry::Rect {
                        x: 5,
                        y: 5,
                        width: 40,
                        height: 10,
                    },
                    line: 0,
                    horizontal_direction: Default::default(),
                    geometry: None,
                },
                crate::recognition::RecognizedWord {
                    text: "next".into(),
                    rect: crate::geometry::Rect {
                        x: 5,
                        y: 20,
                        width: 20,
                        height: 10,
                    },
                    line: 1,
                    horizontal_direction: Default::default(),
                    geometry: None,
                },
            ],
            lines: vec![
                crate::recognition::RecognizedLine {
                    graphemes: None,
                    text: "中e\u{301}".into(),
                    rect: None,
                    horizontal_direction: Default::default(),
                    geometry: None,
                },
                crate::recognition::RecognizedLine {
                    graphemes: None,
                    text: "next".into(),
                    rect: None,
                    horizontal_direction: Default::default(),
                    geometry: None,
                },
            ],
        }
    }

    fn marked_overlay() -> (OcrOverlay, crate::document::Document) {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        overlay.selected = Some(0..overlay.text.text.len());
        let mut document = crate::document::Document::default();
        let edit = overlay
            .ledger
            .prepare(
                &document,
                TextMark::Highlight,
                &overlay
                    .text
                    .marking_quads(overlay.selected.clone().unwrap()),
                crate::annotation::Style::default(),
            )
            .unwrap();
        assert!(overlay.ledger.commit(&mut document, edit));
        overlay.target.revisions = (document.revision(), document.edit_revision());
        overlay.show_selection = false;
        (overlay, document)
    }

    fn target_after_history(
        overlay: &OcrOverlay,
        document: &crate::document::Document,
    ) -> OcrTarget {
        let mut target = overlay.target.clone();
        target.revisions = (document.revision(), document.edit_revision());
        target.stamp.0 = document.revision();
        target
    }

    #[test]
    fn own_mark_undo_and_redo_keep_the_original_ocr_range_and_ledger() {
        let (mut overlay, mut document) = marked_overlay();
        let selected = overlay.selected.clone();
        let text = overlay.text.text.clone();
        document.undo();
        let target = target_after_history(&overlay, &document);
        assert!(!overlay.current(Some(&target), Some(&text)));
        assert!(overlay.can_follow_history(&target, &document, Some(&text)));
        overlay.target = target;
        assert_eq!(overlay.selected, selected);
        assert_eq!(overlay.selected_text().as_deref(), Some(text.as_str()));
        document.redo();
        let target = target_after_history(&overlay, &document);
        assert!(overlay.can_follow_history(&target, &document, Some(&text)));
        overlay.target = target;
        assert_eq!(overlay.selected, selected);
        assert!(!overlay.show_selection);
    }

    #[test]
    fn history_does_not_preserve_geometry_across_crop_source_or_text_changes() {
        let (overlay, mut document) = marked_overlay();
        document.undo();
        let original = target_after_history(&overlay, &document);
        let text = overlay.text.text.as_str();
        let mut changed = original.clone();
        changed.selected.x += 1;
        assert!(!overlay.can_follow_history(&changed, &document, Some(text)));
        changed = original.clone();
        changed.canvas.left -= 1;
        assert!(!overlay.can_follow_history(&changed, &document, Some(text)));
        changed = original.clone();
        changed.identity = crate::scene::SourceIdentity::of(&crate::scene::SourcePixels {
            fill: None,
            canvas: crate::geometry::Rect::full(319, 200),
            planes: Vec::new(),
            overlay: None,
        });
        assert!(!overlay.can_follow_history(&changed, &document, Some(text)));
        assert!(!overlay.can_follow_history(&original, &document, Some("edited text")));
        assert!(!overlay.can_follow_history(&original, &document, None));
    }

    #[test]
    fn changed_owned_annotation_or_unknown_duplicate_invalidates_history_rebase() {
        let (overlay, mut document) = marked_overlay();
        let object = document.items()[0].clone();
        let mut changed = object.annotation.clone();
        changed.style.color = 0x123456;
        document.replace(object.id, changed);
        let target = target_after_history(&overlay, &document);
        assert!(!overlay.can_follow_history(&target, &document, Some(&overlay.text.text)));
        document.undo();
        document.insert(object.annotation);
        let target = target_after_history(&overlay, &document);
        assert!(!overlay.can_follow_history(&target, &document, Some(&overlay.text.text)));
    }

    #[test]
    fn history_rejects_a_changed_lasso_with_the_same_rectangular_target() {
        let (overlay, mut document) = marked_overlay();
        document.set_crop_mask(crate::crop_mask::Polygon::new(&[
            Point { x: 40., y: 60. },
            Point { x: 120., y: 60. },
            Point { x: 40., y: 100. },
        ]));
        let target = target_after_history(&overlay, &document);
        assert!(!overlay.can_follow_history(&target, &document, Some(&overlay.text.text)));
        document.undo();
        let target = target_after_history(&overlay, &document);
        assert!(overlay.can_follow_history(&target, &document, Some(&overlay.text.text)));
    }

    #[test]
    fn overlay_hits_and_copies_original_graphemes_at_the_crop_origin() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        assert!(overlay.text.has_text_at(Point { x: 46., y: 66. }));
        assert!(!overlay.text.has_text_at(Point { x: 6., y: 6. }));
        let (_, selected) = overlay.text.begin(Point { x: 46., y: 66. }, 2).unwrap();
        overlay.selected = Some(selected);
        assert_eq!(overlay.selected_text().as_deref(), Some("中e\u{301}"));
        overlay.selected = Some(0..overlay.text.text.len());
        assert_eq!(overlay.selected_text().as_deref(), Some("中e\u{301}\nnext"));
    }

    #[test]
    fn overlay_rejects_changed_crop_document_or_edited_full_text() {
        let overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        assert!(overlay.current(Some(&target()), Some("中e\u{301}\nnext")));
        let mut changed = target();
        changed.selected.x += 1;
        assert!(!overlay.current(Some(&changed), Some("中e\u{301}\nnext")));
        changed = target();
        changed.revisions.0 += 1;
        assert!(!overlay.current(Some(&changed), Some("中e\u{301}\nnext")));
        assert!(!overlay.current(Some(&target()), Some("中e\u{301}\nnext!")));
        assert!(!overlay.current(None, Some("中e\u{301}\nnext")));
        assert!(!overlay.current(Some(&target()), None));
    }

    #[test]
    fn text_without_valid_geometry_keeps_the_full_text_fallback() {
        let mut value = recognized();
        value.words.clear();
        assert!(OcrOverlay::new(&value, target()).is_none());
    }

    #[test]
    fn image_ocr_body_edit_then_undo_same_bytes_does_not_restore_old_geometry() {
        let value = recognized();
        let mut overlay = OcrOverlay::new(&value, target()).unwrap();
        let mut editor =
            crate::capture_view::text_result::ResultEditor::new(value.text.clone(), 0..0);
        overlay.body_identity = Arc::clone(&editor.body_identity);
        overlay.body_revision = editor.revision;
        assert!(overlay.current_body(&editor.body_identity, editor.revision));
        assert!(editor.replace(Some(0..1), "甲", false, None));
        editor.undo();
        assert_eq!(editor.text, value.text);
        assert!(overlay.current(Some(&target()), Some(&editor.text)));
        assert!(!overlay.current_body(&editor.body_identity, editor.revision));
        assert!(!Arc::ptr_eq(&overlay.body_identity, &editor.body_identity));
    }

    #[test]
    fn image_ocr_viewport_aba_rejects_old_paint_without_changing_geometry_or_marks() {
        let (mut overlay, document) = marked_overlay();
        let text = overlay.text.text.clone();
        let quads = overlay.text.highlights(0..text.len());
        let origin = overlay.target.clone();
        let revisions = (document.revision(), document.edit_revision());
        let projection = crate::ocr_projection::OcrProjection::new(
            crate::viewport::Viewport::default(),
            1.5,
            [800., 600.],
        )
        .unwrap();
        let old_epoch = Arc::clone(&overlay.projection_epoch);
        *overlay.painted_keyboard_target.borrow_mut() = Some(origin.clone());
        overlay.painted_projection.set(Some(projection));
        assert!(overlay.projection_painted(Some(projection)));
        overlay.invalidate_projection();
        overlay.invalidate_projection();
        assert!(!overlay.accepts_projection_epoch(&old_epoch));
        assert!(!overlay.projection_painted(Some(projection)));
        assert!(!overlay.keyboard_target_painted());
        assert_eq!(overlay.text.text, text);
        assert_eq!(overlay.text.highlights(0..text.len()), quads);
        assert!(overlay.target == origin);
        assert_eq!((document.revision(), document.edit_revision()), revisions);
        assert!(overlay.ledger.matches_underlay(&document));
    }

    #[test]
    fn image_ocr_old_paint_cannot_clear_new_epoch_keyboard_or_projection_receipts() {
        let mut overlay = OcrOverlay::new(&recognized(), target()).unwrap();
        let old = Arc::clone(&overlay.projection_epoch);
        overlay.invalidate_projection();
        let current = Arc::clone(&overlay.projection_epoch);
        let projection = crate::ocr_projection::OcrProjection::new(
            crate::viewport::Viewport::default(),
            1.25,
            [800., 600.],
        )
        .unwrap();
        assert!(overlay.publish_projection_paint(&current, projection, true));
        assert!(!overlay.publish_projection_paint(&old, projection, false));
        assert!(overlay.projection_painted(Some(projection)));
        assert!(overlay.keyboard_target_painted());
    }

    #[test]
    fn annotation_anchor_rejects_invalid_and_empty_quads() {
        assert!(quad_bounds(&[]).is_none());
        assert!(quad_bounds(&[[Point { x: f32::NAN, y: 2. }; 4]]).is_none());
        let result = quad_bounds(&[[
            Point { x: -10., y: 10. },
            Point { x: 20., y: 0. },
            Point { x: 25., y: 15. },
            Point { x: -5., y: 25. },
        ]])
        .unwrap();
        assert_eq!(
            result,
            ShapeBounds {
                x: -10.,
                y: 0.,
                width: 35.,
                height: 25.
            }
        );
    }
}
