//! 显式物理范围的共用导出和贴图首显；不借预览输出提交编辑状态。
use crate::{
    canvas_source::CanvasSource,
    capture::Image,
    capture_view::CaptureView,
    crop_mask::Polygon,
    cursor::CapturedCursor,
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    pin_presentation::PinPresentation,
    scene::Scene,
    selection_finish::SelectionFinishStyle,
    Result,
};
use gpui::{AppContext, Context, Window};
use std::time::Instant;
use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::GetWindowThreadProcessId};
#[path = "capture_pin_document.rs"]
pub(crate) mod document_handoff;
#[cfg(test)]
#[path = "capture_long_clip_tests.rs"]
mod long_clip_tests;

/// 新截图来源可保留完整crop历史，但用户出口始终约束在固定可见内容域。
pub(crate) fn selected_host(
    canvas: Rect,
    crop: Option<Rect>,
    content: Option<Rect>,
) -> Result<Rect> {
    let host = crop
        .or(content)
        .filter(|rect| rect.valid(canvas.width, canvas.height))
        .ok_or_else(|| tr!("请先选择截图范围", "Select a capture area first"))?;
    match content {
        Some(content) if content.valid(canvas.width, canvas.height) => {
            host.intersection(content).ok_or_else(|| {
                tr!(
                    "选区与贴图内容没有交集",
                    "The selection does not overlap the pin content"
                )
                .into()
            })
        }
        Some(_) => Err(tr!(
            "贴图内容范围无效，请重新选择",
            "The pin content area is invalid; select again"
        )
        .into()),
        None => Ok(host),
    }
}

pub(crate) fn output_rect(canvas: Rect, host: Rect, mask: Option<&Polygon>) -> Result<Rect> {
    if !host.valid(canvas.width, canvas.height) {
        return Err(tr!(
            "截图范围超出冻结画面",
            "The capture area is outside the frozen screen"
        )
        .into());
    }
    mask.map_or(Ok(host), |mask| {
        mask.output_rect(host).ok_or_else(|| {
            tr!(
                "套索与选区没有交集",
                "The lasso does not overlap the selection"
            )
            .into()
        })
    })
}

pub(crate) fn content_output_rect(
    canvas: Rect,
    crop: Option<Rect>,
    content: Option<Rect>,
    mask: Option<&Polygon>,
) -> Result<Rect> {
    output_rect(canvas, selected_host(canvas, crop, content)?, mask)
}

pub(crate) fn content_image(
    source: &CanvasSource,
    scene: &Scene,
    selected: Rect,
    cursor: Option<&CapturedCursor>,
    mask: Option<&Polygon>,
) -> Result<Image> {
    if !scene.errors.is_empty() {
        return Err(scene.errors.join(";").into());
    }
    let mut image = source.read_region(selected)?;
    if let Some(cursor) = cursor {
        cursor.apply(&mut image, selected);
    }
    scene.try_apply(&mut image, selected)?;
    if let Some(mask) = mask {
        mask.apply(&mut image, selected);
    }
    Ok(image)
}

/// 剪辑始终最后作用于完整原图场景；套索透明度也先按原坐标计算。
pub(crate) fn clipped_content_image(
    source: &CanvasSource,
    scene: &Scene,
    clip: &crate::long_clip::LongClip,
    mask: Option<&Polygon>,
) -> Result<Image> {
    clip.check_output()?;
    if clip.source_size() != (source.bounds.width, source.bounds.height) {
        return Err("剪辑来源与文档画布尺寸不一致".into());
    }
    let image = content_image(source, scene, source.full(), None, mask)?;
    crate::long_clip_export::compose(&image, clip)
}

pub(crate) fn finished_origin(
    bounds: ScreenRect,
    content: Rect,
    style: SelectionFinishStyle,
) -> (i32, i32) {
    let pad = style.padding();
    (
        bounds
            .left
            .saturating_add(content.x as i32)
            .saturating_sub(pad.left as i32),
        bounds
            .top
            .saturating_add(content.y as i32)
            .saturating_sub(pad.top as i32),
    )
}

impl CaptureView {
    pub(crate) fn window_pin_started(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pin_mode
            || self.image_canvas.is_some()
            || self.processing
            || self.saving
            || self.pin_handoff_pending
        {
            return;
        }
        if !self.finish_content_or_status() {
            return;
        }
        let rect = match self.selected() {
            Ok(rect) => rect,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let point = (
            rect.x.saturating_add(rect.width / 2),
            rect.y.saturating_add(rect.height / 2),
        );
        let target = self
            .window_targets
            .iter()
            .find(|target| {
                target.process_id != std::process::id()
                    && target.rect.contains((point.0 as i32, point.1 as i32))
            })
            .cloned();
        let Some(target) = target else {
            self.status = tr!(
                "选区未匹配到源窗口，已转普通贴图",
                "No source window; creating a static pin"
            )
            .into();
            self.pin_range(rect, window, cx);
            return;
        };
        let fallback = match self.content_image_in(rect) {
            Ok(image) => image,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let selection = ScreenRect {
            left: self.source.bounds.left.saturating_add(rect.x as i32),
            top: self.source.bounds.top.saturating_add(rect.y as i32),
            width: rect.width,
            height: rect.height,
        };
        match crate::pin_live_view::open(&target, selection, fallback, cx) {
            Ok(()) => {
                self.remember_last_region();
                self.flush_tool_presets();
                self.capture_lock.take();
                window.remove_window();
            }
            Err(error) => {
                self.status = trf!(
                    "实时贴图创建失败：{error}，截图已保留",
                    "Live pin failed: {error}; capture kept"
                );
            }
        }
        cx.notify();
    }
    /// 范围已在当前输入时固定；不走会提交草稿的 Action::Pin 准备分支。
    pub(crate) fn pin_range(&mut self, rect: Rect, window: &mut Window, cx: &mut Context<Self>) {
        crate::pin_handoff_smoke::mark("action_pin_entered");
        let started =
            (std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() == Ok("1")).then(Instant::now);
        self.pin_range_started(rect, started, window, cx);
        cx.notify();
    }
    pub(crate) fn pin_range_started(
        &mut self,
        rect: Rect,
        started: Option<Instant>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.processing || self.saving || self.pin_handoff_pending {
            return;
        }
        if self.editor.document.long_clip().is_some() {
            // 剪辑后的成品已有新的片段排列；用统一输出像素建立贴图，
            // 不向尚未支持剪辑历史的贴图文档格式传递原图坐标。
            match self.selected_image().and_then(|image| {
                let origin = crate::long_output::pin_origin((image.width, image.height), self.long_clip_placement);
                crate::pin_view::PinView::open_with_history_window(image, origin, cx)
            }) {
                Ok(pin) => {
                    self.pin_handoff_pending = true;
                    let capture = window.window_handle().downcast::<Self>().expect("剪辑编辑器");
                    crate::long_output::wait_pin_shown(pin, move |outcome, cx| {
                        let _ = capture.update(cx, |view, window, cx| {
                            view.pin_handoff_pending = false;
                            match outcome {
                                crate::long_output::LongOutcome::Finished => view.finish_output(tr!("已贴图，可继续编辑", "Pinned; keep editing"), window),
                                crate::long_output::LongOutcome::Return(error) => view.status = error.unwrap_or_default(),
                            }
                            cx.notify();
                        });
                    }, cx);
                },
                Err(error) => self.status = error.to_string(),
            }
            return;
        }
        let rect = match selected_host(
            self.source.full(),
            Some(rect),
            self.pin_content_rect.filter(|_| self.pin_mode),
        ) {
            Ok(rect) => rect,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let finish = self.effective_finish_style();
        // 先验证完整外观预算及有符号屏幕边界，失败不能读选区或消费首显资格。
        let presentation = match PinPresentation::new(self.source.bounds, rect, finish) {
            Ok(presentation) => presentation,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let outer = presentation.outer_screen();
        let origin = (outer.left, outer.top);
        let capture_window = window.window_handle();
        let capture_hwnd = hwnd(window).0 as usize;
        let mut capture_pid = 0;
        let capture_tid = unsafe {
            GetWindowThreadProcessId(HWND(capture_hwnd as *mut _), Some(&mut capture_pid))
        };
        if capture_pid != std::process::id() || capture_tid == 0 {
            self.status = tr!(
                "截图窗口身份已变化，请重新截图",
                "The capture window changed; please capture again"
            )
            .into();
            return;
        }
        let close_capture = self.image_canvas.is_none() && !self.pin_mode;
        let record_history = self.quick_action.record_history();
        let capture = cx.entity().downgrade();
        self.pin_handoff_pending = true;
        crate::pin_handoff_smoke::mark("selected_image_begin");
        let scene = self.prepared_scene();
        match document_handoff::CaptureDocumentPayload::prepare(
            &self.source, &self.previews, &self.editor, &scene, rect,
            self.cursor.as_ref().filter(|_| self.include_cursor), finish,
        ).and_then(|payload| {
            let presentation = payload.presentation();
            let style = presentation.effective_style();
            crate::pin_handoff_smoke::mark("selected_image_end");
            let image_ready_ms = started.map(|started| started.elapsed().as_secs_f64() * 1000.);
            crate::pin_handoff_smoke::mark("pin_open_begin");
            crate::pin_view::PinView::open_capture_handoff(
                payload,
                origin,
                crate::pin_view::CapturePinPresentation {
                    record_history,
                    preserve_source_origin: !self.source.is_image(),
                    window_shadow_enabled: !style.shadow_enabled,
                    style: Some(style),
                },
                capture_window,
                HWND(capture_hwnd as *mut _),
                move |result, async_cx| {
                    let timing = std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() == Ok("1");
                    match result {
                        Ok(ready) => {
                            crate::pin_handoff_smoke::observe_ready(&ready);
                            let total_ms = started.map(|started| started.elapsed().as_secs_f64() * 1000.);
                            let accepted = async_cx.update_window(capture_window, |_, window, cx| {
                                let mut owner = 0;
                                let tid = unsafe { GetWindowThreadProcessId(hwnd(window), Some(&mut owner)) };
                                if hwnd(window).0 as usize != capture_hwnd || owner != capture_pid || tid != capture_tid {
                                    return false;
                                }
                                capture.update(cx, |view, cx| {
                                    if !view.pin_handoff_pending { return false; }
                                    view.pin_handoff_pending = false;
                                    view.remember_last_region();
                                    if record_history { view.quick_action.history_succeeded(); }
                                    view.quick_action.fail();
                                    view.notify_quick_completion();
                                    view.status.clear();
                                    if close_capture {
                                        // 预览输出成功后才保存参数，失败不消耗外观保存意图。
                                        view.flush_tool_presets();
                                        view.capture_lock.take();
                                        window.remove_window();
                                    }
                                    cx.notify();
                                    true
                                }).unwrap_or(false)
                            }).unwrap_or(false);
                            if timing {
                                crate::startup_probe::emit_trace_value(&serde_json::json!({
                                    "phase":"capture_pin_handoff", "ok":accepted,
                                    "trigger_to_visible_ms":total_ms, "image_ready_ms":image_ready_ms,
                                    "capture_hwnd":capture_hwnd, "capture_tid":capture_tid,
                                    "pin_hwnd":ready.hwnd, "pin_tid":ready.thread_id,
                                    "presented_serial":ready.presented_serial, "dwm_flushed":ready.dwm_flushed,
                                    "first_show_timing":ready.timing,
                                    "capture_removed_after_visible":accepted && close_capture,
                                }));
                            }
                            if !accepted { let _ = ready.discard(async_cx); }
                            accepted
                        }
                        Err(error) => {
                            let _ = async_cx.update_window(capture_window, |_, window, cx| {
                                let mut owner = 0;
                                let tid = unsafe { GetWindowThreadProcessId(hwnd(window), Some(&mut owner)) };
                                if hwnd(window).0 as usize != capture_hwnd || owner != capture_pid || tid != capture_tid { return; }
                                let _ = capture.update(cx, |view, cx| {
                                    if !view.pin_handoff_pending { return; }
                                    view.pin_handoff_pending = false;
                                    view.quick_action.fail();
                                    view.status = trf!("贴图创建失败：{error}；截图已保留。", "Failed to create the pin: {error}; the capture is kept.");
                                    cx.notify();
                                });
                            });
                            if timing { crate::startup_probe::emit_trace_value(&serde_json::json!({
                                "phase":"capture_pin_handoff", "ok":false, "error":error,
                                "capture_removed_after_visible":false,
                            })); }
                            false
                        }
                    }
                }, cx)
        }) {
            Ok(_) => { crate::pin_handoff_smoke::mark("pin_open_returned"); },
            Err(e) => { self.pin_handoff_pending = false; self.status = e.to_string(); },
        }
    }
}
