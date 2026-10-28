//! 只画于 GPUI 预览的选区 OCR 反馈；不进入 Document/Scene 或输出合成。
use super::{
    ocr_task::TaskSlot,
    text_result::{InputTarget, OcrRequestKind, OcrTarget, RecognitionSession},
    CaptureView,
};
use crate::geometry::Rect;
use gpui::{
    canvas, fill, point, prelude::*, px, rgb, rgba, size, AnyElement, Bounds, Context, Window,
};
use std::time::Instant;

// 显式 QA 时钟只记阶段和数字身份；不保存图像/文字，不拥有或等待 OCR。
#[derive(Clone)]
pub(crate) struct ClickTrace(std::sync::Arc<ClickTraceState>);

struct ClickTraceState {
    id: u64,
    started: Instant,
    task: std::sync::OnceLock<TaskKey>,
    profile: std::sync::atomic::AtomicU8,
    scan_painted: std::sync::atomic::AtomicBool,
}

impl ClickTrace {
    pub(crate) fn start(phase: &'static str) -> Option<Self> {
        if std::env::var("RUSTSHOT_OCR_CLICK_TRACE").as_deref() != Ok("1") {
            return None;
        }
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let trace = Self(std::sync::Arc::new(ClickTraceState {
            id: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            started: Instant::now(),
            task: std::sync::OnceLock::new(),
            profile: std::sync::atomic::AtomicU8::new(0),
            scan_painted: std::sync::atomic::AtomicBool::new(false),
        }));
        trace.event(phase);
        Some(trace)
    }

    pub(crate) fn bind(&self, key: TaskKey, profile: crate::ppocr::OcrProfile) {
        self.0.task.set(key).expect("OCR 点击 Trace 重复绑定请求");
        self.0.profile.store(
            match profile {
                crate::ppocr::OcrProfile::Tiny => 1,
                crate::ppocr::OcrProfile::Small => 2,
            },
            std::sync::atomic::Ordering::Relaxed,
        );
        self.event("task_submitted");
    }

    pub(crate) fn event(&self, phase: &'static str) {
        self.record(phase, 0, None);
    }

    fn record(&self, phase: &'static str, serial: usize, dwm_status: Option<i32>) {
        crate::startup_probe::emit_trace_value(&serde_json::json!({
            "kind": "ocr_click_trace", "phase": phase, "pid": std::process::id(),
            "id": self.0.id, "task_id": self.0.task.get().map_or(0, |key| key.id),
            "task_kind": self.0.task.get().map_or(0, |key| match key.kind {
                TaskKind::Initial => 1, TaskKind::Retry => 2,
                TaskKind::Foreground => 3, TaskKind::Local => 4,
            }),
            "profile": self.0.profile.load(std::sync::atomic::Ordering::Relaxed),
            "elapsed_us": self.0.started.elapsed().as_micros(),
            "present_serial": serial, "dwm_status": dwm_status,
        }));
    }

    fn scan_paint(&self, key: TaskKey, window: &mut Window) {
        if !trace_scan_key_matches(self.0.task.get().copied(), key) {
            return;
        }
        if !self
            .0
            .scan_painted
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.record("scan_paint", present_serial(window), None);
        }
    }

    pub(crate) fn result_installed(
        &self,
        view: &CaptureView,
        window: &mut Window,
        cx: &Context<CaptureView>,
    ) {
        self.event("result_installed");
        poll_result_present(
            self.clone(),
            cx.entity().downgrade(),
            view.result_output_frame(),
            view.result_body_identity.clone(),
            ResultPresentObservation::default(),
            180,
            window,
        );
    }
}

fn trace_scan_key_matches(bound: Option<TaskKey>, current: TaskKey) -> bool {
    bound == Some(current) && current.kind != TaskKind::Local
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultPresentStep {
    Pending,
    PaintObserved,
    Presented,
}

#[derive(Clone, Copy, Default)]
struct ResultPresentObservation {
    paint_serial: Option<usize>,
}

impl ResultPresentObservation {
    /// 首次观察有效真实 paint 后，仍须等待严格后继 Present。
    /// 串号相等可代表纯 draw 或失败帧；paint 门失效后不能沿用旧观察点。
    fn observe(&mut self, painted: bool, serial: usize) -> ResultPresentStep {
        if !painted {
            self.paint_serial = None;
            return ResultPresentStep::Pending;
        }
        match self.paint_serial {
            None => {
                self.paint_serial = Some(serial);
                ResultPresentStep::PaintObserved
            }
            Some(baseline) if serial > baseline => ResultPresentStep::Presented,
            Some(_) => ResultPresentStep::Pending,
        }
    }
}

fn present_serial(window: &Window) -> usize {
    use windows::{core::w, Win32::UI::WindowsAndMessaging::GetPropW};
    unsafe {
        GetPropW(
            crate::gpui_capture::hwnd(window),
            w!("RustShot.GpuiPresentSerial"),
        )
    }
    .0 as usize
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

#[allow(clippy::too_many_arguments)]
fn poll_result_present(
    trace: ClickTrace,
    owner: gpui::WeakEntity<CaptureView>,
    frame: super::text_result::ResultFrame,
    body: std::sync::Arc<()>,
    mut observed: ResultPresentObservation,
    remaining: u16,
    window: &mut Window,
) {
    // on_next_frame 先于 draw；首次观察同代真实 paint 时记录当时串号，
    // 再等严格后继成功 Present + DwmFlush。观测会多一个 QA 帧，
    // 只给观测后呈现上界，不冒称结果最早一帧或回调本身已经呈现。
    window.on_next_frame(move |window, cx| {
        let Some(owner_now) = owner.upgrade() else {
            trace.event("result_view_dropped");
            return;
        };
        let view = owner_now.read(cx);
        if view.recognition_result.is_none()
            || view.result_output_frame() != frame
            || !std::sync::Arc::ptr_eq(&view.result_body_identity, &body)
        {
            trace.event("result_invalidated");
            return;
        }
        if !window.is_window_active() || !view.focus.is_focused(window) {
            trace.event("result_inactive");
            return;
        }
        // 普通截图的原位词带隐藏全文面板，不能等待该面板的输出 Cell。
        // 原位门核当前 overlay 独有 Rc 的真实 paint、源/正文及 focus/idle；
        // 外层 ResultFrame + Arc 正文身份使旧代或同字节替换不能沿用回执。
        let painted = if view.ocr_overlay.is_some() && view.result_editor.is_none() {
            view.ocr_overlay_text_output_ready(window)
        } else {
            view.painted_result_output.get() == Some(frame)
        };
        let serial = present_serial(window);
        let step = observed.observe(painted, serial);
        if step == ResultPresentStep::PaintObserved {
            trace.record("result_paint_observed", serial, None);
        }
        if step == ResultPresentStep::Presented {
            let status = unsafe { DwmFlush() };
            trace.record(
                if status >= 0 {
                    "result_present_upper_bound"
                } else {
                    "result_dwm_failed"
                },
                serial,
                Some(status),
            );
            return;
        }
        if remaining == 0 || trace.0.started.elapsed() > std::time::Duration::from_secs(10) {
            trace.event("result_present_timeout");
            return;
        }
        poll_result_present(trace, owner, frame, body, observed, remaining - 1, window);
    });
    // 回执和 on_next_frame 均在 render/paint 之外，没有当前渲染视图。
    // refresh 直接安排窗口 draw，不调用依赖 rendered_entity_stack 的动画接口。
    window.refresh();
}

#[path = "capture_ocr_progress_model.rs"]
mod model;
pub(crate) use model::{local_roi, ProgressClock, TaskKey, TaskKind};
use model::{DisplayRect, ScanFrame};

fn session_task(
    session: &RecognitionSession,
    current: Option<&OcrTarget>,
    revision: u64,
) -> Option<(TaskKey, Rect)> {
    let request = session.pending_request()?;
    let (kind, token) = match request.kind {
        OcrRequestKind::Initial => (TaskKind::Initial, session.initial_token(request.id)),
        OcrRequestKind::Retry => (TaskKind::Retry, session.retry_token(request.id)),
    };
    if token?.is_cancelled() || !session.request_is_current(request, current?, revision) {
        return None;
    }
    Some((
        TaskKey {
            kind,
            id: request.id,
        },
        request.target.selected,
    ))
}

fn foreground_task(
    task: &TaskSlot<InputTarget>,
    current: Option<&OcrTarget>,
) -> Option<(TaskKey, Rect)> {
    let current = current?;
    let id = task.current_id()?;
    if task.token()?.is_cancelled() || !task.key()?.is_current(current, true) {
        return None;
    }
    Some((
        TaskKey {
            kind: TaskKind::Foreground,
            id,
        },
        current.selected,
    ))
}

impl CaptureView {
    fn ocr_progress_task(&self) -> Option<(TaskKey, Rect)> {
        // 范围手势的中间态不把旧 ROI 的动效画到新选区，亦不改变任务的旧取消合同。
        if self.pin_mode || self.editor.is_changing_crop() {
            return None;
        }
        if let Some((id, selected)) = self.local_ocr_progress() {
            return Some((
                TaskKey {
                    kind: TaskKind::Local,
                    id,
                },
                selected,
            ));
        }
        let current = self.current_ocr_target();
        if let Some(task) = session_task(
            &self.recognition_session,
            current.as_ref(),
            self.result_content_revision(),
        ) {
            return Some(task);
        }
        let target = self.foreground_ocr.key()?;
        let current = match target {
            InputTarget::Content(_) => current,
            InputTarget::Frozen(_) => self.current_frozen_ocr_target(),
        };
        foreground_task(&self.foreground_ocr, current.as_ref())
    }

    pub(crate) fn ocr_progress_replaces_status(&self) -> bool {
        self.status == tr!("正在本机识别文字…", "Recognizing text locally…")
            || self.status == tr!("正在本机重新识别文字…", "Recognizing text again locally…")
            || self.status == tr!("正在捕获选区文字…", "Capturing text in the selection…")
    }

    pub(crate) fn render_ocr_progress(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Instant::now();
        let task = window
            .is_window_active()
            .then(|| self.ocr_progress_task())
            .flatten();
        let offset = self.canvas_offset(window);
        let projected = task.and_then(|(key, selected)| {
            DisplayRect::project(
                selected,
                self.source.full(),
                self.canvas_scale(window),
                (f32::from(offset.x), f32::from(offset.y)),
            )
            .map(|roi| (key, roi))
        });
        let elapsed = self
            .ocr_progress_clock
            .observe(projected.map(|(key, _)| key), now)?;
        let (key, roi) = projected?;
        let scan = ScanFrame::new(roi, elapsed);
        let click_trace = self.ocr_click_trace.clone();
        let owner = cx.entity().downgrade();
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, window, cx| {
                    // 延后 paint 仍只画同一真实任务；旧帧不为新任务排队动画刷新。
                    let current = owner.upgrade().is_some_and(|owner| {
                        owner
                            .read(cx)
                            .ocr_progress_task()
                            .is_some_and(|(actual, _)| actual == key)
                    });
                    if !current || !window.is_window_active() {
                        return;
                    }
                    if let Some(trace) = &click_trace {
                        trace.scan_paint(key, window);
                    }
                    let palette = crate::theme::palette(cx);
                    let bounds = Bounds::new(
                        point(px(roi.x), px(roi.y)),
                        size(px(roi.width), px(roi.height)),
                    );
                    window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                        let inset = 1.5_f32.min(roi.width / 4.).min(roi.height / 4.);
                        let edge = Bounds::new(
                            bounds.origin + point(px(inset), px(inset)),
                            size(px(roi.width - inset * 2.), px(roi.height - inset * 2.)),
                        );
                        window.paint_quad(
                            gpui::outline(
                                edge,
                                rgba((palette.background << 8) | 0xb0),
                                gpui::BorderStyle::default(),
                            )
                            .border_widths(px(2.)),
                        );
                        window.paint_quad(gpui::outline(
                            edge,
                            rgba((palette.accent << 8) | u32::from(scan.edge_alpha)),
                            gpui::BorderStyle::default(),
                        ));
                        // 12 个淡色条构成光带，不铺整块暗色遮罩，原图始终可辨。
                        for strip in 0..12 {
                            let level = 1. - ((strip as f32 + 0.5) / 6. - 1.).abs();
                            let y = scan.center_y - scan.band_height / 2.
                                + strip as f32 * scan.band_height / 12.;
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(px(roi.x), px(y)),
                                    size(px(roi.width), px(scan.band_height / 12.)),
                                ),
                                rgba((palette.accent << 8) | (level * 30.) as u32),
                            ));
                        }
                        window.paint_quad(fill(
                            Bounds::new(
                                point(px(roi.x), px(scan.center_y)),
                                size(px(roi.width), px(1.)),
                            ),
                            rgba((palette.accent << 8) | 0x80),
                        ));
                        // 缺依赖时先联网下载，徽标改报下载百分比；放不下完整文案时退到短文案。
                        let labels = match crate::ppocr::download_progress::current() {
                            Some(progress) => progress.labels().map(gpui::SharedString::from),
                            None => [tr!("识别中", "Recognizing").into(), "".into()],
                        };
                        let fitted =
                            labels
                                .into_iter()
                                .filter(|text| !text.is_empty())
                                .find_map(|text| {
                                    let line = window.text_system().shape_line(
                                        text.clone(),
                                        px(12.),
                                        &[gpui::TextRun {
                                            len: text.len(),
                                            font: window.text_style().font(),
                                            color: rgb(palette.foreground).into(),
                                            background_color: None,
                                            underline: None,
                                            strikethrough: None,
                                        }],
                                        None,
                                    );
                                    roi.badge(f32::from(line.width)).map(|badge| (line, badge))
                                });
                        if let Some((line, badge)) = fitted {
                            let bounds = Bounds::new(
                                point(px(badge.x), px(badge.y)),
                                size(px(badge.width), px(badge.height)),
                            );
                            window.paint_quad(
                                fill(bounds, rgba((palette.background << 8) | 0xe8))
                                    .corner_radii(px(13.))
                                    .border_widths(px(1.))
                                    .border_color(rgba((palette.accent << 8) | 0x60)),
                            );
                            window.paint_quad(
                                fill(
                                    Bounds::new(
                                        bounds.origin + point(px(8.), px(11.)),
                                        size(px(4.), px(4.)),
                                    ),
                                    rgb(palette.accent),
                                )
                                .corner_radii(px(2.)),
                            );
                            let _ = line.paint(
                                bounds.origin + point(px(18.), px(0.)),
                                px(26.),
                                window,
                                cx,
                            );
                        }
                    });
                    window.request_animation_frame();
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .into_any_element(),
        )
    }
}

#[cfg(test)]
#[path = "capture_ocr_progress_tests.rs"]
mod tests;
