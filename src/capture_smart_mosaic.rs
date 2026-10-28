//! 普通冻结截图的智能候选适配；候选 Chrome 不属于 Document 或 Scene。
use super::{ocr_task, Action, CaptureView};
use crate::{
    annotation::{Bounds, Style, Tool},
    scene::SourceIdentity,
    smart_mosaic::{OwnedRequest, RequestKey, SmartMosaic, SourceEpoch, TaskKind},
};
use gpui::{Context, KeyDownEvent, Window};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Candidates {
    pub(crate) core: SmartMosaic,
    identity: Option<SourceIdentity>,
    epoch: u64,
    profile: crate::ppocr::OcrProfile,
    observed_roi: Option<crate::geometry::Rect>,
    pub(crate) pointer: bool,
    wake: Option<gpui::Task<()>>,
    wake_at: Option<Instant>,
    detection: ocr_task::TaskSlot<OwnedRequest>,
}

impl Candidates {
    pub(crate) fn cancel_detection(&mut self, reason: &str) {
        if let Some(request) = self.detection.key() {
            self.core.complete_failure(request);
        }
        self.detection.cancel(reason);
    }

    /// 显式激活/新请求读取配置后调用；变档撤销 det Owner 和成功缓存，不动源/ROI。
    fn observe_profile(&mut self, profile: crate::ppocr::OcrProfile, now: Instant) -> bool {
        if self.profile == profile {
            return false;
        }
        self.cancel_detection("智能候选 OCR 档位已改变");
        self.profile = profile;
        // 换档是新的检测会话：复用现 epoch 清掉旧成功缓存，保留真实 identity/ROI。
        // 不重置 core 的请求 id，Tiny→Small→Tiny 也不能接回旧任务或旧缓存。
        self.epoch = self.epoch.checked_add(1).expect("检测档位会话计数溢出");
        self.core.source_changed(SourceEpoch(self.epoch), now);
        if let Some(key) = self.core.request_key().cloned() {
            self.core.schedule_refresh(
                detection_request_key_for_profile(SourceEpoch(self.epoch), key.roi, profile),
                now,
            );
        }
        self.core.cancel_pointer();
        self.pointer = false;
        self.wake.take();
        self.wake_at = None;
        true
    }

    /// 观察与工具是否活动无关；同 source/ROI leave 保留 Owner 与隐藏缓存。
    fn observe_source_roi(
        &mut self,
        identity: SourceIdentity,
        stable_roi: Option<Option<crate::geometry::Rect>>,
        now: Instant,
    ) {
        if self.identity.as_ref() != Some(&identity) {
            self.cancel_detection("智能候选冻结源已替换");
            self.epoch = self.epoch.checked_add(1).expect("冻结源会话计数溢出");
            self.identity = Some(identity);
            self.core.source_changed(SourceEpoch(self.epoch), now);
            self.observed_roi = None;
        }
        let Some(roi) = stable_roi else {
            return;
        };
        if self.observed_roi == roi {
            return;
        }
        self.cancel_detection("智能候选稳定选区已改变");
        self.observed_roi = roi;
        // 首次入场由 activate 保留立即请求。已有 key 在隐藏时也切换；
        // 空 ROI 用无效 key 清除旧成功缓存，不把旧范围误复用到后续选区。
        if self.core.request_key().is_some() {
            self.core.schedule_refresh(
                detection_request_key_for_profile(
                    SourceEpoch(self.epoch),
                    roi.unwrap_or(crate::geometry::Rect::full(0, 0)),
                    self.profile,
                ),
                now,
            );
        }
        self.core.cancel_pointer();
        self.pointer = false;
    }
}

#[cfg(test)]
fn line_regions(result: crate::recognition::RecognizedText) -> Vec<Bounds> {
    result
        .lines
        .into_iter()
        .filter_map(|line| line.rect)
        .map(|rect| Bounds {
            x: rect.x as f32,
            y: rect.y as f32,
            width: rect.width as f32,
            height: rect.height as f32,
        })
        .collect()
}

#[cfg(test)]
fn detection_request_key(source: SourceEpoch, roi: crate::geometry::Rect) -> RequestKey {
    detection_request_key_for_profile(source, roi, crate::ppocr::OcrProfile::Tiny)
}

fn detection_request_key_for_profile(
    source: SourceEpoch,
    roi: crate::geometry::Rect,
    profile: crate::ppocr::OcrProfile,
) -> RequestKey {
    RequestKey {
        source,
        roi,
        backend: format!("ppocr-{}-cpu", profile.as_str()),
        profile: format!("{}:det-only-raw-frozen", profile.as_str()),
        kind: TaskKind::Detection,
    }
}

/// 真正发起的 OwnedRequest key 是引擎选择证据，未知/不一致配置拒绝而不回退 Tiny。
fn detection_profile(key: &RequestKey) -> Option<crate::ppocr::OcrProfile> {
    use crate::ppocr::OcrProfile;
    match (key.backend.as_str(), key.profile.as_str(), key.kind) {
        ("ppocr-tiny-cpu", "tiny:det-only-raw-frozen", TaskKind::Detection) => {
            Some(OcrProfile::Tiny)
        }
        ("ppocr-small-cpu", "small:det-only-raw-frozen", TaskKind::Detection) => {
            Some(OcrProfile::Small)
        }
        _ => None,
    }
}

fn detection_regions(regions: Vec<crate::geometry::Rect>) -> Vec<Bounds> {
    regions
        .into_iter()
        .map(|rect| Bounds {
            x: rect.x as f32,
            y: rect.y as f32,
            width: rect.width as f32,
            height: rect.height as f32,
        })
        .collect()
}
fn candidate_input_ready(
    editor: &crate::editor::Editor,
    ordinary: bool,
    busy_other: bool,
    input_active: bool,
    owner_active: bool,
) -> bool {
    ordinary
        && owner_active
        && !busy_other
        && !input_active
        && editor.tool == Tool::SmartMosaic
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
        && editor.selected_ids().is_empty()
        && editor
            .crop
            .is_some_and(|rect| rect.valid(editor.canvas.width, editor.canvas.height))
}

impl CaptureView {
    pub(crate) fn smart_mode(&self) -> bool {
        !self.pin_mode && self.image_canvas.is_none() && self.editor.tool == Tool::SmartMosaic
    }
    pub(crate) fn cancel_smart_pointer(&mut self) {
        self.smart_candidates.core.cancel_pointer();
        self.smart_candidates.pointer = false;
    }
    fn smart_candidate_input_ready(&self, window: &Window) -> bool {
        candidate_input_ready(
            &self.editor,
            !self.pin_mode && self.image_canvas.is_none(),
            self.processing
                || self.saving
                || self.pin_handoff_pending
                || self.toolbar_drag.is_some(),
            self.watermark_input_focused
                || self.input.marked.is_some()
                || self.recognition_result.is_some()
                || self.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None
                || self.screenshot_toolbar_menu.presentation().is_some(),
            window.is_window_active() && self.smart_candidates.core.is_active(),
        )
    }
    pub(crate) fn smart_candidate_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_ocr_task_targets();
        self.observe_smart_candidate_target();
        let active = self.smart_mode()
            && window.is_window_active()
            && !self.pin_handoff_pending
            && !self.saving
            && !self.processing
            && self.recognition_result.is_none();
        if !active {
            if self.smart_candidates.core.is_active() {
                self.smart_candidates.core.leave();
            }
            self.smart_candidates.pointer = false;
            self.smart_candidates.wake.take();
            self.smart_candidates.wake_at = None;
            return;
        }
        let region_tolerance = 2. * self.canvas_scale(window);
        self.smart_candidates
            .core
            .set_region_tolerance(region_tolerance);
        // 只观察已经结束的真实范围；Resize 的预览、取消与远返不排识别。
        if self.editor.is_changing_crop() || self.editor.is_dragging() {
            return;
        }
        let Some(roi) = self
            .editor
            .crop
            .filter(|rect| rect.valid(self.source.full().width, self.source.full().height))
        else {
            self.smart_candidates.core.leave();
            return;
        };
        let now = Instant::now();
        let entering = !self.smart_candidates.core.is_active();
        if entering {
            // 包括隐藏缓存再入；正常 paint 和推理 busy 不反复读取 INI。
            let profile = crate::preferences::Preferences::load().ocr_profile;
            self.smart_candidates.observe_profile(profile, now);
        }
        let key = detection_request_key_for_profile(
            SourceEpoch(self.smart_candidates.epoch),
            roi,
            self.smart_candidates.profile,
        );
        if entering {
            self.smart_candidates.core.activate(key.clone(), now);
            self.smart_candidates.observed_roi = Some(roi);
        }
        if self.saving
            || self.processing
            || self.recognition_result.is_some()
            || self.watermark_input_focused
            || self.text_composing()
        {
            return;
        }
        if !entering
            && !self.smart_candidates.core.is_busy()
            && self
                .smart_candidates
                .core
                .pending_deadline()
                .is_some_and(|deadline| now >= deadline)
        {
            // 只在已经到期的新请求提交前读一次，档位变化会先取消并重排请求。
            let profile = crate::preferences::Preferences::load().ocr_profile;
            self.smart_candidates.observe_profile(profile, now);
        }
        if let Some(request) = self.smart_candidates.core.begin_request(now) {
            self.cancel_smart_pointer();
            let Some(profile) = detection_profile(&request.key) else {
                self.smart_candidates.core.complete_failure(&request);
                self.status = tr!("智能检测请求档位无效", "Invalid detection profile").into();
                return;
            };
            let image = match self.source.read_region(request.key.roi) {
                Ok(image) => image,
                Err(error) => {
                    self.smart_candidates.core.complete_failure(&request);
                    self.status = error.to_string();
                    return;
                }
            };
            let owned_task = self.smart_candidates.detection.start(request);
            let task_token = owned_task.token.clone();
            let task = cx.background_executor().spawn(async move {
                ocr_task::Receipt::from_result(
                    crate::recognition::detect_regions_with_profile_cancellable(
                        image,
                        profile,
                        &task_token,
                    ),
                )
                .map(detection_regions)
            });
            cx.spawn_in(window, async move |view, cx| {
                let result = task.await;
                let _ = view.update_in(cx, |view, window, cx| {
                    // 即使未 Render 或工具已离开，先观察旧源/稳定 ROI；
                    // 旧回执不得释放新 det Owner，也不读写普通 OCR processing。
                    view.observe_smart_candidate_target();
                    let Some(result) = view
                        .smart_candidates
                        .detection
                        .finish_receipt(owned_task.id, result)
                    else {
                        return;
                    };
                    let request = &owned_task.key;
                    match result {
                        ocr_task::Receipt::Completed(regions) => {
                            view.smart_candidates
                                .core
                                .complete_success(request, regions);
                        }
                        ocr_task::Receipt::Cancelled => {
                            view.smart_candidates.core.complete_failure(request);
                        }
                        ocr_task::Receipt::Failed(error) => {
                            if view.smart_candidates.core.complete_failure(request)
                                && view.smart_mode()
                            {
                                view.status = error;
                            }
                        }
                    }
                    view.smart_candidate_sync(window, cx);
                    cx.notify();
                });
            })
            .detach();
        }
        let deadline = if self.smart_candidates.core.is_busy() {
            None
        } else {
            self.smart_candidates.core.pending_deadline()
        };
        if deadline != self.smart_candidates.wake_at {
            self.smart_candidates.wake.take();
            self.smart_candidates.wake_at = deadline;
            if let Some(deadline) = deadline {
                let delay = deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1));
                self.smart_candidates.wake = Some(cx.spawn_in(window, async move |view, cx| {
                    cx.background_executor().timer(delay).await;
                    let _ = view.update_in(cx, |view, window, cx| {
                        view.smart_candidates.wake_at = None;
                        view.smart_candidate_sync(window, cx);
                        cx.notify();
                    });
                }));
            }
        }
    }
    fn observe_smart_candidate_target(&mut self) {
        let identity = self.frozen_source_identity();
        // 拖动/缩放的中间范围不是稳定 ROI，取消/恢复到原 ROI 仍可接同一回执。
        let stable_roi =
            (!self.editor.is_changing_crop() && !self.editor.is_dragging()).then(|| {
                self.editor
                    .crop
                    .filter(|rect| rect.valid(self.source.full().width, self.source.full().height))
            });
        self.smart_candidates
            .observe_source_roi(identity, stable_roi, Instant::now());
    }
    pub(crate) fn enter_smart_mosaic(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pin_mode || self.image_canvas.is_some() {
            self.recognize(true, window, cx);
            return;
        }
        if !self.finish_content_or_status() {
            return;
        }
        if self.editor.crop.is_none() {
            self.editor.set_crop(Some(self.source.full()));
        }
        self.clear_pointer_tracking();
        self.set_manual_preview(None);
        self.editor.select_tool(Tool::SmartMosaic);
        self.smart_candidates.core.leave();
        self.smart_candidate_sync(window, cx);
    }
    fn commit_smart_intent(
        &mut self,
        regions: Vec<Bounds>,
        effect: Tool,
        style: Style,
    ) -> crate::Result<bool> {
        let Some(edit) = self.editor.prepare_smart_regions(regions, effect, style)? else {
            let changed = self.editor.effect_context() != (effect, style);
            self.editor.set_smart_effect_context(effect, style);
            return Ok(changed);
        };
        let identity = SourceIdentity::of(&self.source_pixels());
        let scene = self.scene.updated(&edit.annotations, &self.source_pixels());
        if !scene.errors.is_empty() {
            return Err(scene.errors.join(";").into());
        }
        if SourceIdentity::of(&self.source_pixels()) != identity
            || !self.editor.commit_smart_regions(edit)
        {
            return Err("智能效果准备期间来源已经变化".into());
        }
        self.scene = Arc::new(scene);
        self.scene_stamp = None;
        Ok(true)
    }
    pub(crate) fn apply_smart_mosaic_style(
        &mut self,
        style: Style,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.smart_mode() || self.saving || self.processing || self.editor.is_dragging() {
            return false;
        }
        self.cancel_smart_pointer();
        let effect = self.editor.effect_context().0;
        match self.commit_smart_intent(self.editor.smart_applied_regions(), effect, style) {
            Ok(changed) => {
                cx.notify();
                changed
            }
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                false
            }
        }
    }
    pub(crate) fn change_smart_effect(&mut self, effect: Tool, cx: &mut Context<Self>) {
        if !self.smart_mode() || self.saving || self.processing || self.editor.is_dragging() {
            return;
        }
        self.cancel_smart_pointer();
        let style = self.editor.effect_context().1;
        if let Err(error) =
            self.commit_smart_intent(self.editor.smart_applied_regions(), effect, style)
        {
            self.status = error.to_string();
        }
        cx.notify();
    }
    pub(crate) fn smart_pointer_down(
        &mut self,
        ordinary_canvas_ready: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.smart_mode() || self.editor.crop.is_none() || self.editor.is_changing_crop() {
            return false;
        }
        self.selection_click_target = None;
        self.selection_pointer_dragged = false;
        let tolerance = 4. * self.canvas_scale(window);
        let host_accepted = self.smart_candidate_input_ready(window)
            && self
                .editor
                .crop
                .is_some_and(|rect| rect.contains(self.pointer));
        if !ordinary_canvas_ready || !host_accepted {
            self.invalidate_click_sequence();
        }
        self.smart_candidates.pointer = host_accepted
            && self
                .smart_candidates
                .core
                .pointer_down(self.pointer_sample, tolerance);
        // OCR 尚在加载或成功为空时，真实普通宿主点击仍可组成返回编辑的 double。
        // 不据此创建 Core 的候选手势；候选应用仍只沿 pointer_down 的真实结果。
        if self.smart_candidates.pointer || (ordinary_canvas_ready && host_accepted) {
            self.pointer_down_start = Some(self.pointer);
            unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(crate::gpui_capture::hwnd(
                    window,
                ));
            }
        } else {
            self.pointer_down_start = None;
        }
        cx.stop_propagation();
        cx.notify();
        true
    }
    pub(crate) fn smart_pointer_move(
        &mut self,
        left: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.smart_mode() || self.editor.is_changing_crop() {
            return false;
        }
        if self.pointer_down_start.is_some() {
            if left && self.smart_candidate_input_ready(window) {
                if self.smart_candidates.pointer {
                    self.smart_candidates.core.pointer_move(self.pointer_sample);
                }
            } else {
                self.clear_pointer_tracking();
                self.release_capture();
            }
        }
        cx.notify();
        true
    }
    pub(crate) fn smart_pointer_up(
        &mut self,
        event: &gpui::MouseUpEvent,
        click_pending: Option<crate::capture_double_click::CanvasClickDown>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.pointer_down_start.is_none() {
            return false;
        }
        if !self.smart_candidate_input_ready(window) {
            self.clear_pointer_tracking();
            self.release_capture();
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        let candidate_pointer = std::mem::replace(&mut self.smart_candidates.pointer, false);
        let applied = self.editor.smart_applied_regions();
        let intent = if candidate_pointer {
            self.smart_candidates
                .core
                .pointer_up(self.pointer_sample, &applied)
        } else {
            None
        };
        let owned = self.control_selection_ready && !self.selection_pointer_dragged;
        self.clear_pointer_tracking();
        self.release_capture();
        let mut completed = true;
        if let Some(intent) = intent {
            let (effect, style) = self.editor.effect_context();
            if let Err(error) = self.commit_smart_intent(intent.next_regions, effect, style) {
                self.status = error.to_string();
                completed = false;
            }
        }
        if let Some(pending) = click_pending {
            self.startup_click_seed = self.ordinary_clicks.finish_ordinary(
                pending,
                &self.editor,
                self.source.full(),
                self.pointer,
                crate::capture_double_click::CanvasClickUp {
                    screenshot: self.smart_mode(),
                    modified: event.modifiers.modified(),
                    busy: self.processing
                        || self.saving
                        || self.pin_handoff_pending
                        || self.toolbar_drag.is_some(),
                    input_active: self.watermark_input_focused
                        || self.input.marked.is_some()
                        || self.recognition_result.is_some(),
                    owned: owned && completed,
                },
            );
        }
        cx.stop_propagation();
        cx.notify();
        true
    }
    pub(crate) fn smart_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.smart_mode() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if key == "a" && m.control && !m.alt && !m.platform && !m.function {
            if !self.preview_cursor_replay && self.smart_candidate_input_ready(window) {
                if !event.is_held && self.smart_candidates.pointer {
                    self.clear_pointer_tracking();
                    self.release_capture();
                }
                if let Some(intent) = self
                    .smart_candidates
                    .core
                    .add_all(&self.editor.smart_applied_regions(), event.is_held)
                {
                    let (effect, style) = self.editor.effect_context();
                    if let Err(error) = self.commit_smart_intent(intent.next_regions, effect, style)
                    {
                        self.status = error.to_string();
                    }
                }
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if matches!(key, "left" | "right" | "up" | "down")
            && !m.control
            && !m.alt
            && !m.platform
            && !m.function
            && !self.editor.is_changing_crop()
            && !self.smart_candidates.pointer
        {
            if !self.preview_cursor_replay {
                if let Some(mut rect) = self.editor.crop {
                    let step = if m.shift { 10 } else { 1 };
                    let (dx, dy) = match key {
                        "left" => (-step, 0),
                        "right" => (step, 0),
                        "up" => (0, -step),
                        _ => (0, step),
                    };
                    rect.x = (rect.x as i32 + dx)
                        .clamp(0, (self.editor.canvas.width - rect.width) as i32)
                        as u32;
                    rect.y = (rect.y as i32 + dy)
                        .clamp(0, (self.editor.canvas.height - rect.height) as i32)
                        as u32;
                    self.editor.set_smart_crop(Some(rect));
                    self.smart_candidate_sync(window, cx);
                }
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if key == "escape" {
            let resizing = self.editor.is_changing_crop();
            self.cancel_smart_pointer();
            self.editor.cancel_gesture();
            self.clear_pointer_tracking();
            self.release_capture();
            if !resizing {
                self.perform(Action::Tool(Tool::Edit), window, cx);
            }
            cx.stop_propagation();
            cx.notify();
            return true;
        }
        if (key == "enter" && (self.editor.is_changing_crop() || self.smart_candidates.pointer))
            || (self.smart_candidates.pointer
                && !m.control
                && !m.alt
                && !m.platform
                && !m.function
                && matches!(
                    key,
                    "left" | "right" | "up" | "down" | "a" | "w" | "s" | "d"
                ))
        {
            cx.stop_propagation();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recognition::RecognizedText;
    static FROZEN: [u8; 128] = [0; 128];
    static REPLACEMENT: [u8; 128] = [1; 128];

    fn source_identity(pixels: &'static [u8]) -> SourceIdentity {
        let full = crate::geometry::Rect::full(8, 4);
        SourceIdentity::of(&crate::scene::SourcePixels {
            fill: None,
            canvas: full,
            planes: vec![crate::scene::PixelPlane {
                rect: full,
                bgra: pixels,
            }],
            overlay: None,
        })
    }

    fn pending_candidates() -> (Candidates, ocr_task::TaskRequest<OwnedRequest>, Instant) {
        let mut candidates = Candidates::default();
        let now = Instant::now();
        let roi = crate::geometry::Rect {
            x: 2,
            y: 1,
            width: 4,
            height: 2,
        };
        candidates.observe_source_roi(source_identity(&FROZEN), Some(Some(roi)), now);
        candidates.core.activate(
            detection_request_key(SourceEpoch(candidates.epoch), roi),
            now,
        );
        let request = candidates.core.begin_request(now).unwrap();
        let task = candidates.detection.start(request);
        (candidates, task, now)
    }

    #[test]
    fn same_source_roi_leave_keeps_owner_and_publishes_success_from_hidden_cache() {
        let (mut candidates, task, now) = pending_candidates();
        let key = task.key.key.clone();
        candidates.core.leave();
        candidates.observe_source_roi(source_identity(&FROZEN), Some(Some(key.roi)), now);
        assert!(task.token.checkpoint().is_ok());
        assert!(candidates.core.is_busy());
        let result = candidates
            .detection
            .finish_receipt(
                task.id,
                ocr_task::Receipt::Completed(vec![Bounds {
                    x: 0.,
                    y: 0.,
                    width: 2.,
                    height: 1.,
                }]),
            )
            .unwrap();
        assert!(task.token.is_cancelled());
        if let ocr_task::Receipt::Completed(regions) = result {
            assert!(candidates.core.complete_success(&task.key, regions));
        } else {
            panic!("已分类成功不能由 Owner Drop 否决");
        }
        assert!(candidates.core.candidates().is_empty());
        candidates.core.activate(key, now);
        assert_eq!(candidates.core.candidates().len(), 1);
        assert!(candidates.core.begin_request(now).is_none());
    }

    #[test]
    fn hidden_stable_roi_change_cancels_old_owner_before_inactive_return() {
        let (mut candidates, old, now) = pending_candidates();
        candidates.core.leave();
        let mut changed = old.key.key.roi;
        changed.x += 1;
        candidates.observe_source_roi(source_identity(&FROZEN), Some(Some(changed)), now);
        assert!(old.token.is_cancelled());
        assert!(!candidates.core.is_busy());
        assert!(!candidates.core.is_active());
        assert_eq!(candidates.core.request_key().unwrap().roi, changed);
        assert!(candidates
            .detection
            .finish_receipt(old.id, ocr_task::Receipt::<Vec<Bounds>>::Cancelled)
            .is_none());
        assert!(!candidates.core.complete_success(&old.key, vec![]));
    }

    #[test]
    fn old_det_receipt_cannot_clear_new_roi_owner_or_busy_state() {
        let (mut candidates, old, now) = pending_candidates();
        let mut changed = old.key.key.roi;
        changed.x += 1;
        candidates.observe_source_roi(source_identity(&FROZEN), Some(Some(changed)), now);
        let request = candidates
            .core
            .begin_request(now + crate::smart_mosaic::REFRESH_DELAY)
            .unwrap();
        let new = candidates.detection.start(request);
        assert!(candidates
            .detection
            .finish_receipt(old.id, ocr_task::Receipt::Completed(Vec::<Bounds>::new()))
            .is_none());
        assert!(!candidates.core.complete_failure(&old.key));
        assert!(new.token.checkpoint().is_ok());
        assert!(candidates.core.is_busy());
    }

    #[test]
    fn hidden_source_replacement_cancels_owner_even_during_resize_preview() {
        let (mut candidates, task, now) = pending_candidates();
        candidates.core.leave();
        candidates.observe_source_roi(source_identity(&REPLACEMENT), None, now);
        assert!(task.token.is_cancelled());
        assert!(!candidates.core.is_busy());
        assert_ne!(
            candidates.core.request_key().unwrap().source,
            task.key.key.source
        );
        assert!(candidates
            .detection
            .finish_receipt(task.id, ocr_task::Receipt::Completed(Vec::<Bounds>::new()))
            .is_none());
    }

    #[test]
    fn transient_roi_and_cancel_back_to_original_do_not_cancel_det_owner() {
        let (mut candidates, task, now) = pending_candidates();
        candidates.observe_source_roi(source_identity(&FROZEN), None, now);
        candidates.observe_source_roi(source_identity(&FROZEN), Some(Some(task.key.key.roi)), now);
        assert!(task.token.checkpoint().is_ok());
        assert!(candidates.detection.owns(task.id));
        assert!(candidates.core.is_busy());
    }

    #[test]
    fn invalid_stable_roi_cancels_owner_and_invalidates_cached_request_key() {
        let (mut candidates, task, now) = pending_candidates();
        candidates.core.leave();
        candidates.observe_source_roi(source_identity(&FROZEN), Some(None), now);
        assert!(task.token.is_cancelled());
        assert!(!candidates.core.is_busy());
        let key = candidates.core.request_key().unwrap();
        assert_eq!(key.roi.width, 0);
        assert!(candidates.core.pending_deadline().is_none());
        assert!(!candidates.core.complete_success(&task.key, vec![]));
    }

    #[test]
    fn true_termination_cancels_detection_only_and_refused_close_keeps_it_owned() {
        let (mut a, request_a, _) = pending_candidates();
        let (b, request_b, _) = pending_candidates();
        // saving / finish 拒绝关闭时仍保留整个 Candidates 与其 Owner。
        assert!(a.core.is_busy());
        assert!(request_a.token.checkpoint().is_ok());
        // 已批准关闭的 Action::Cancel / View Drop 才消费此宿主 Owner。
        a.core.terminate();
        a.cancel_detection("模型确认关闭");
        assert!(request_a.token.is_cancelled());
        assert!(request_b.token.checkpoint().is_ok());
        assert!(b.core.is_busy());
    }

    #[test]
    fn typed_det_cancel_is_not_successful_empty_cache_and_explicit_reentry_can_retry() {
        let (mut candidates, task, now) = pending_candidates();
        let receipt = candidates
            .detection
            .finish_receipt(task.id, ocr_task::Receipt::<Vec<Bounds>>::Cancelled)
            .unwrap();
        assert_eq!(receipt, ocr_task::Receipt::Cancelled);
        assert!(candidates.core.complete_failure(&task.key));
        candidates.core.leave();
        candidates.core.activate(task.key.key, now);
        assert!(candidates.core.begin_request(now).is_some());
    }
    #[test]
    fn detection_candidates_keep_independent_task_geometry_and_frozen_roi_offset() {
        let roi = crate::geometry::Rect {
            x: 100,
            y: 60,
            width: 200,
            height: 100,
        };
        let key = detection_request_key(SourceEpoch(3), roi);
        assert_eq!(key.kind, TaskKind::Detection);
        assert_eq!(key.backend, "ppocr-tiny-cpu");
        let mut core = SmartMosaic::default();
        let now = Instant::now();
        core.activate(key, now);
        let request = core.begin_request(now).unwrap();
        assert!(core.complete_success(
            &request,
            detection_regions(vec![crate::geometry::Rect {
                x: 12,
                y: 9,
                width: 70,
                height: 16,
            }]),
        ));
        assert_eq!(
            core.candidates(),
            &[Bounds {
                x: 112.,
                y: 69.,
                width: 70.,
                height: 16.,
            }],
        );
    }
    #[test]
    fn candidates_use_visual_lines_not_word_regions_or_uia_geometry() {
        let rect = crate::geometry::Rect {
            x: 12,
            y: 9,
            width: 70,
            height: 16,
        };
        let result = RecognizedText {
            text: "one two".into(),
            regions: vec![crate::geometry::Rect::full(1, 1)],
            words: vec![],
            lines: vec![
                crate::recognition::RecognizedLine {
                    graphemes: None,
                    horizontal_direction: Default::default(),
                    geometry: None,
                    text: "one two".into(),
                    rect: Some(rect),
                },
                crate::recognition::RecognizedLine {
                    graphemes: None,
                    horizontal_direction: Default::default(),
                    geometry: None,
                    text: "missing".into(),
                    rect: None,
                },
            ],
        };
        assert_eq!(
            line_regions(result),
            vec![Bounds {
                x: 12.,
                y: 9.,
                width: 70.,
                height: 16.
            }]
        );
    }
    #[test]
    fn candidate_admission_preserves_input_object_and_resize_priorities() {
        let mut editor = crate::editor::Editor::new(
            crate::geometry::Rect::full(300, 220),
            Some(crate::geometry::Rect::full(300, 220)),
        );
        editor.select_tool(Tool::SmartMosaic);
        assert!(candidate_input_ready(&editor, true, false, false, true));
        for (ordinary, busy, input, active) in [
            (false, false, false, true),
            (true, true, false, true),
            (true, false, true, true),
            (true, false, false, false),
        ] {
            assert!(!candidate_input_ready(
                &editor, ordinary, busy, input, active
            ));
        }
        let mut annotation = crate::annotation::Annotation::new(
            Tool::Mosaic,
            crate::annotation::Point { x: 10., y: 10. },
            Style::default(),
        );
        annotation.update(crate::annotation::Point { x: 40., y: 30. });
        let id = editor.document.insert(annotation);
        editor.selected = Some(id);
        assert!(!candidate_input_ready(&editor, true, false, false, true));
        editor.selected = None;
        editor.set_smart_crop(Some(crate::geometry::Rect {
            x: 50,
            y: 40,
            width: 120,
            height: 100,
        }));
        assert!(editor.begin_screenshot_crop_outside((20, 20), 6.));
        assert!(!candidate_input_ready(&editor, true, false, false, true));
        editor.cancel_gesture();
        assert!(candidate_input_ready(&editor, true, false, false, true));
        editor.select_tool(Tool::Mosaic);
        assert!(!candidate_input_ready(&editor, true, false, false, true));
    }

    #[test]
    fn detection_profile_keys_are_distinct_and_unknown_never_falls_back_to_tiny() {
        use crate::ppocr::OcrProfile;
        let roi = crate::geometry::Rect::full(4, 2);
        let tiny = detection_request_key_for_profile(SourceEpoch(9), roi, OcrProfile::Tiny);
        let small = detection_request_key_for_profile(SourceEpoch(9), roi, OcrProfile::Small);
        assert_ne!(tiny, small);
        assert_eq!(detection_profile(&tiny), Some(OcrProfile::Tiny));
        assert_eq!(detection_profile(&small), Some(OcrProfile::Small));
        let mut mismatched = small.clone();
        mismatched.backend = tiny.backend;
        assert_eq!(detection_profile(&mismatched), None);
        mismatched = small.clone();
        mismatched.profile = "future:det-only-raw-frozen".into();
        assert_eq!(detection_profile(&mismatched), None);
        mismatched = small;
        mismatched.kind = TaskKind::Recognition;
        assert_eq!(detection_profile(&mismatched), None);
    }

    #[test]
    fn hidden_tiny_cache_is_discarded_when_small_profile_is_selected() {
        use crate::ppocr::OcrProfile;
        let (mut candidates, task, now) = pending_candidates();
        candidates.core.leave();
        let receipt = candidates
            .detection
            .finish_receipt(
                task.id,
                ocr_task::Receipt::Completed(vec![Bounds {
                    x: 0.,
                    y: 0.,
                    width: 2.,
                    height: 1.,
                }]),
            )
            .unwrap();
        if let ocr_task::Receipt::Completed(regions) = receipt {
            assert!(candidates.core.complete_success(&task.key, regions));
        } else {
            panic!("应保留真实已分类成功");
        }
        assert!(candidates.observe_profile(OcrProfile::Small, now));
        let key = candidates.core.request_key().unwrap().clone();
        assert_eq!(key.source, SourceEpoch(candidates.epoch));
        assert_ne!(key.source, task.key.key.source);
        assert_eq!(key.roi, task.key.key.roi);
        assert_eq!(detection_profile(&key), Some(OcrProfile::Small));
        candidates.core.activate(key, now);
        assert!(candidates.core.candidates().is_empty());
        let request = candidates
            .core
            .begin_request(now + crate::smart_mosaic::REFRESH_DELAY)
            .unwrap();
        assert_eq!(detection_profile(&request.key), Some(OcrProfile::Small));
        assert!(candidates.core.complete_failure(&request));
        let switched = now + crate::smart_mosaic::REFRESH_DELAY;
        assert!(candidates.observe_profile(OcrProfile::Tiny, switched));
        let returned = candidates
            .core
            .begin_request(switched + crate::smart_mosaic::REFRESH_DELAY)
            .unwrap();
        assert_eq!(detection_profile(&returned.key), Some(OcrProfile::Tiny));
        assert_ne!(returned.key.source, task.key.key.source);
        assert_ne!(returned.id, task.key.id);
    }

    #[test]
    fn tiny_small_tiny_switch_cancels_old_slots_and_old_receipts_cannot_revive() {
        use crate::ppocr::OcrProfile;
        let (mut candidates, original, now) = pending_candidates();
        assert!(candidates.observe_profile(OcrProfile::Small, now));
        assert!(original.token.is_cancelled());
        let request = candidates
            .core
            .begin_request(now + crate::smart_mosaic::REFRESH_DELAY)
            .unwrap();
        let small = candidates.detection.start(request);
        assert_eq!(detection_profile(&small.key.key), Some(OcrProfile::Small));
        let switched = now + crate::smart_mosaic::REFRESH_DELAY;
        assert!(candidates.observe_profile(OcrProfile::Tiny, switched));
        assert!(small.token.is_cancelled());
        let request = candidates
            .core
            .begin_request(switched + crate::smart_mosaic::REFRESH_DELAY)
            .unwrap();
        let current = candidates.detection.start(request);
        assert_eq!(current.key.key.roi, original.key.key.roi);
        assert_eq!(detection_profile(&current.key.key), Some(OcrProfile::Tiny));
        assert_ne!(current.key.key.source, original.key.key.source);
        assert_ne!(current.id, original.id);
        assert_ne!(current.key.id, original.key.id);
        for old in [&original, &small] {
            assert!(candidates
                .detection
                .finish_receipt(old.id, ocr_task::Receipt::Completed(Vec::<Bounds>::new()),)
                .is_none());
            assert!(!candidates.core.complete_success(&old.key, vec![]));
        }
        assert!(candidates.detection.owns(current.id));
        assert!(current.token.checkpoint().is_ok());
        assert!(candidates.core.is_busy());
    }
}
