//! 冻结画面的控件选区适配：单个 MTA 查询会话，连续移动跟进最新点。
use crate::{
    capture_view::{ocr_task, text_result, Action, CaptureView},
    control_selection::{ControlPath, ControlPathState, ControlTarget},
    geometry::Rect,
};
use gpui::{Context, Window};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

const PATH_LEASE: Duration = Duration::from_millis(750);
const REFRESH_INTERVAL: Duration = Duration::from_millis(16);

/// UIA 无值/失败可补救；取消只在查询边界检查，不冒称中断 UIA。
/// 控件文字捕获的结果：UIA 或混合文字直接显示；没有任何控件文字且未绘制标注时交回
/// 标准 OCR，以安装带定位的原位文字层与结果面板（冻结底图与内容此时一致）。
enum ControlTextOutcome {
    Text(String, &'static str),
    StandardOcr(crate::ppocr::OcrProfile),
}

fn control_text_with_ocr_fallback<T>(
    token: &crate::recognition::task::TaskToken,
    query: impl FnOnce() -> crate::Result<Option<T>>,
    fallback: impl FnOnce() -> crate::Result<T>,
) -> crate::Result<T> {
    token.checkpoint()?;
    let native = query();
    token.checkpoint()?;
    let result = match native {
        Ok(Some(value)) => Ok(value),
        _ => fallback(),
    };
    // 真补救错误先传播；取消只覆盖成功，不把真实推理错误藏成用户取消。
    let value = result?;
    token.checkpoint()?;
    Ok(value)
}

#[cfg(test)]
mod control_text_cancellation_tests {
    use super::control_text_with_ocr_fallback;
    use crate::capture_view::ocr_task::Receipt;
    use crate::recognition::task::CancelOwner;
    use std::cell::Cell;

    #[test]
    fn cancelled_before_uia_does_not_query_or_start_ocr_fallback() {
        let owner = CancelOwner::new();
        let token = owner.token();
        owner.cancel().unwrap();
        let query_count = Cell::new(0);
        let fallback_count = Cell::new(0);
        let receipt = Receipt::from_result(control_text_with_ocr_fallback(
            &token,
            || {
                query_count.set(1);
                Ok(Some(String::from("UIA")))
            },
            || {
                fallback_count.set(1);
                Ok(String::from("OCR"))
            },
        ));
        assert_eq!(receipt, Receipt::Cancelled);
        assert_eq!(query_count.get(), 0);
        assert_eq!(fallback_count.get(), 0);
    }

    #[test]
    fn cancelled_during_uia_success_cannot_return_copyable_text_or_start_fallback() {
        let owner = CancelOwner::new();
        let token = owner.token();
        let fallback_count = Cell::new(0);
        let receipt = Receipt::from_result(control_text_with_ocr_fallback(
            &token,
            || {
                owner.cancel().unwrap();
                Ok(Some(String::from("已经读出的 UIA 文字")))
            },
            || {
                fallback_count.set(1);
                Ok(String::from("OCR"))
            },
        ));
        assert_eq!(receipt, Receipt::Cancelled);
        assert_eq!(fallback_count.get(), 0);
    }

    #[test]
    fn cancelled_during_successful_fallback_cannot_return_copyable_text() {
        let owner = CancelOwner::new();
        let token = owner.token();
        let receipt = Receipt::from_result(control_text_with_ocr_fallback(
            &token,
            || Ok(None),
            || {
                owner.cancel().unwrap();
                Ok(String::from("补救已识别文字"))
            },
        ));
        assert_eq!(receipt, Receipt::Cancelled);
    }

    #[test]
    fn uia_empty_or_unavailable_still_runs_the_existing_ocr_fallback() {
        for available in [false, true] {
            let owner = CancelOwner::new();
            let token = owner.token();
            let receipt = Receipt::from_result(control_text_with_ocr_fallback(
                &token,
                || {
                    if available {
                        Ok(None)
                    } else {
                        Err("UIA 提供者不可用".into())
                    }
                },
                || Ok(String::from("冻结画面文字")),
            ));
            assert_eq!(receipt, Receipt::Completed(String::from("冻结画面文字")));
        }
    }

    #[test]
    fn genuine_ocr_failure_wins_when_cancel_occurs_during_fallback() {
        let owner = CancelOwner::new();
        let token = owner.token();
        let receipt: Receipt<String> = Receipt::from_result(control_text_with_ocr_fallback(
            &token,
            || Ok(None),
            || {
                owner.cancel().unwrap();
                Err("真实推理形状错误".into())
            },
        ));
        assert_eq!(receipt, Receipt::Failed(String::from("真实推理形状错误")));
    }
}

struct CachedPath {
    canvas: crate::geometry::ScreenRect,
    window: crate::selection_assist::WindowTarget,
    path: ControlPath,
    received: Instant,
}

#[derive(Default)]
pub(crate) struct ControlTracking {
    worker: Option<Arc<crate::control_probe_worker::ControlProbeWorker>>,
    feedback: crate::control_feedback::ControlFeedback,
    cache: VecDeque<CachedPath>,
    epoch: u64,
    paused: bool,
    timer_pending: bool,
    last_request_at: Option<Instant>,
    /// 已安排的展示到期唤醒；更早的到期会另起一个。
    wake_at: Option<Instant>,
    /// 仅供 Debug 日志去重：上次记录的展示范围。
    logged_shown: Option<Option<Rect>>,
}

pub(crate) struct ControlHover {
    pub(crate) path: ControlPath,
    pub(crate) level: usize,
    window_hwnd: usize,
    received: Instant,
    navigated: bool,
    retained_for_query: bool,
    owner: ControlHoverOwner,
}

struct ControlPreviewTarget {
    target: ControlTarget,
    /// 冻结时已知的窗口范围，可先显示，但不代表 UIA 已完成。
    window_fallback: bool,
}

struct ControlHoverOwner {
    canvas: crate::geometry::ScreenRect,
    window: crate::selection_assist::WindowTarget,
    epoch: u64,
}

struct ControlHoverPointContext<'a> {
    canvas: crate::geometry::ScreenRect,
    window: Option<&'a crate::selection_assist::WindowTarget>,
    epoch: u64,
    point: (i32, i32),
    auto_adsorb: bool,
    eligible: bool,
    now: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum ControlHoverPointDecision {
    Keep,
    RetainedRange(usize),
    Clear,
}

fn control_known_range_owner_is_current(
    hover: &ControlHover,
    context: &ControlHoverPointContext<'_>,
) -> bool {
    let Some(window) = context
        .window
        .filter(|window| window.hwnd == hover.window_hwnd)
    else {
        return false;
    };
    context.auto_adsorb
        && context.eligible
        && hover.path.state != ControlPathState::WindowOnly
        && hover.owner.canvas == context.canvas
        && hover.owner.epoch == context.epoch
        && same_window(&hover.owner.window, window)
        && context.now.saturating_duration_since(hover.received) < PATH_LEASE
        && context.canvas.width <= i32::MAX as u32
        && context.canvas.height <= i32::MAX as u32
        && Rect::full(context.canvas.width, context.canvas.height).contains(context.point)
        && hover.path.targets.get(hover.level).is_some()
        && hover.path.targets.last().map(|target| target.rect) == Some(window.rect)
        && hover.path.targets.iter().all(|target| {
            target
                .rect
                .valid(context.canvas.width, context.canvas.height)
        })
}

fn control_feedback_candidate(
    target: ControlTarget,
    hover: Option<&ControlHover>,
    context: &ControlHoverPointContext<'_>,
) -> crate::control_feedback::FeedbackCandidate {
    let retained_for_query = hover.is_some_and(|hover| {
        hover.retained_for_query
            && control_known_range_owner_is_current(hover, context)
            && hover.path.targets.get(hover.level).is_some_and(|current| {
                current.rect == target.rect && current.rect.contains(context.point)
            })
    });
    crate::control_feedback::FeedbackCandidate {
        target,
        window: context.window.map_or(0, |window| window.hwnd),
        refined: hover.is_some_and(|hover| hover.path.state != ControlPathState::WindowOnly),
        terminal: hover.is_some_and(|hover| hover.level == 0 && hover.path.terminal),
        complete: hover.is_some_and(|hover| hover.path.complete),
        leaf: hover.is_some_and(control_hover_is_point_leaf),
        navigated: hover.is_some_and(|hover| hover.navigated),
        retained_for_query,
        window_fallback: false,
        // 结果的查询点（画布坐标）；与当前点不同说明是旧点结果。
        sample: hover.and_then(|hover| {
            hover
                .path
                .point
                .0
                .checked_sub(context.canvas.left)
                .zip(hover.path.point.1.checked_sub(context.canvas.top))
        }),
    }
}

fn control_display_feedback_candidate(
    target: ControlTarget,
    hover: Option<&ControlHover>,
    context: &ControlHoverPointContext<'_>,
    monitors: impl IntoIterator<Item = Rect>,
) -> crate::control_feedback::FeedbackCandidate {
    // 用真实路径范围先核验 owner；显示降为单屏不撤销已知父范围的即时反馈资格。
    let mut candidate = control_feedback_candidate(target, hover, context);
    if !candidate.navigated {
        candidate.target.rect = crate::startup_preview::auto_target_rect(
            candidate.target.rect,
            monitors,
            context.point,
        );
    }
    candidate
}

fn control_hover_point_decision(
    hover: &ControlHover,
    context: &ControlHoverPointContext<'_>,
) -> ControlHoverPointDecision {
    let current_window = context
        .window
        .filter(|window| window.hwnd == hover.window_hwnd);
    // 当前层仍包含鼠标时保留既有手动 hold；新增 owner 只约束离开后的已知范围回退。
    if current_window.is_some()
        && control_level_contains_point(&hover.path.targets, hover.level, context.point)
        && (hover.path.state != ControlPathState::WindowOnly
            || context
                .canvas
                .left
                .checked_add(context.point.0)
                .zip(context.canvas.top.checked_add(context.point.1))
                == Some(hover.path.point))
    {
        return ControlHoverPointDecision::Keep;
    }
    if !control_known_range_owner_is_current(hover, context) {
        return ControlHoverPointDecision::Clear;
    }
    // UIA 子级矩形未必嵌套于父级；因此按叶到根查整个已知路径。
    (0..hover.path.targets.len())
        .find(|&level| control_level_contains_point(&hover.path.targets, level, context.point))
        .map_or(
            ControlHoverPointDecision::Clear,
            ControlHoverPointDecision::RetainedRange,
        )
}

/// 与鼠标、滚轮和点击入口共用的纯宿主更新；不修改 Document 或合成结果回执。
fn update_control_hover_point(
    hover: &mut Option<ControlHover>,
    pending_navigation: &mut i32,
    wheel_remainder: &mut f32,
    last_request: &mut Option<(i32, i32)>,
    context: &ControlHoverPointContext<'_>,
) -> ControlHoverPointDecision {
    let decision = hover
        .as_ref()
        .map_or(ControlHoverPointDecision::Clear, |hover| {
            control_hover_point_decision(hover, context)
        });
    match decision {
        ControlHoverPointDecision::Keep => {}
        ControlHoverPointDecision::RetainedRange(level) => {
            let hover = hover.as_mut().expect("范围回退决策保留原路径");
            hover.level = level;
            hover.navigated = false;
            hover.retained_for_query = true;
            *pending_navigation = 0;
            *wheel_remainder = 0.;
            // 旧点的已请求记录不能挡住新点查询；在途任务完成后仍沿原 latest-point 入口。
            *last_request = None;
        }
        ControlHoverPointDecision::Clear => {
            *hover = None;
            *pending_navigation = 0;
            *wheel_remainder = 0.;
        }
    }
    decision
}

fn control_hover_is_fresh(hover: &ControlHover, point: (i32, i32), now: Instant) -> bool {
    hover.path.complete
        && hover.path.terminal
        && control_level_contains_point(&hover.path.targets, 0, point)
        && (hover.navigated || hover.level == 0 && !hover.retained_for_query)
        && now.saturating_duration_since(hover.received) < PATH_LEASE
}

/// 当前点完整查询到的最深元素。鼠标位于容器空白处（不落在任何子元素上）时，
/// 最深元素就是该容器本身；它与叶控件同样是真实范围，不能因控件类型不是叶而不显示。
fn control_hover_is_point_leaf(hover: &ControlHover) -> bool {
    hover.level == 0 && hover.path.complete && !hover.retained_for_query
}

/// UIA 结果只接受当前点的最深元素；快速窗口范围另从冻结缓存取得。
fn control_hover_preview_ready(hover: &ControlHover, auto_adsorb: bool) -> bool {
    !auto_adsorb || hover.navigated || control_hover_is_point_leaf(hover)
}

/// 已解析到叶控件或用户主动导航；容器结果在移入已访问子叶时仍让位给缓存的细叶。
fn control_hover_terminal_ready(hover: &ControlHover) -> bool {
    hover.navigated
        || hover.path.state != ControlPathState::WindowOnly
            && control_hover_is_point_leaf(hover)
            && hover.path.terminal
}

/// 同点刷新（含窗口级占位与重试）不能撤销用户已滚轮选中的范围。
fn control_refresh_keeps_navigation(hover: Option<&ControlHover>, pending_steps: i32) -> bool {
    pending_steps != 0 || hover.is_some_and(|hover| hover.navigated)
}

fn control_navigated_range(hover: Option<&ControlHover>) -> Option<(usize, Rect)> {
    let hover = hover.filter(|hover| hover.navigated)?;
    hover
        .path
        .targets
        .get(hover.level)
        .map(|target| (hover.window_hwnd, target.rect))
}

fn control_hover_summary(hover: Option<&ControlHover>) -> String {
    hover.map_or_else(
        || "无".into(),
        |hover| {
            format!(
                "state={:?} level={}/{} navigated={} retained={} complete={} terminal={} rect={:?}",
                hover.path.state,
                hover.level,
                hover.path.targets.len(),
                hover.navigated,
                hover.retained_for_query,
                hover.path.complete,
                hover.path.terminal,
                hover
                    .path
                    .targets
                    .get(hover.level)
                    .map(|target| target.rect)
            )
        },
    )
}

/// 新点回退只是临时范围；旧点结果即使覆盖新点，也不能完成这次恢复查询。
fn control_retained_query_is_current(
    hover: Option<&ControlHover>,
    point: (i32, i32),
    requested: (i32, i32),
) -> bool {
    hover.is_none_or(|hover| !hover.retained_for_query || point == requested)
}

fn control_probe_request_ready(
    hover: Option<&ControlHover>,
    point: (i32, i32),
    last_request: Option<(i32, i32)>,
    manual: bool,
    retry: bool,
    pending: bool,
    now: Instant,
) -> bool {
    if pending || !manual && hover.is_some_and(|hover| control_hover_is_fresh(hover, point, now)) {
        return false;
    }
    let expired =
        hover.is_some_and(|hover| now.saturating_duration_since(hover.received) >= PATH_LEASE);
    last_request != Some(point) || manual || retry || expired
}

fn same_window(
    a: &crate::selection_assist::WindowTarget,
    b: &crate::selection_assist::WindowTarget,
) -> bool {
    a.hwnd == b.hwnd
        && a.process_id == b.process_id
        && a.thread_id == b.thread_id
        && a.rect == b.rect
        && a.global_bounds.left == b.global_bounds.left
        && a.global_bounds.top == b.global_bounds.top
        && a.global_bounds.right == b.global_bounds.right
        && a.global_bounds.bottom == b.global_bounds.bottom
}

/// 细路径按空间有效性接受；窗口回退仅属于原采样点。
fn path_contains_current_point(
    path: &ControlPath,
    point: (i32, i32),
    requested: (i32, i32),
) -> bool {
    path.targets
        .first()
        .is_some_and(|target| target.rect.contains(point))
        && (path.state != ControlPathState::WindowOnly || point == requested)
}

fn control_level_contains_point(
    targets: &[ControlTarget],
    level: usize,
    point: (i32, i32),
) -> bool {
    targets
        .get(level)
        .is_some_and(|target| target.rect.contains(point))
}

/// 已缓存路径只进入包含新点的层；缺少新点子级时保持当前范围。
fn step_control_level(
    targets: &[ControlTarget],
    level: usize,
    point: (i32, i32),
    steps: i32,
) -> Option<usize> {
    step_control_level_with_remainder(targets, level, point, steps).map(|(level, _)| level)
}

fn step_control_level_with_remainder(
    targets: &[ControlTarget],
    mut level: usize,
    point: (i32, i32),
    mut steps: i32,
) -> Option<(usize, i32)> {
    if !control_level_contains_point(targets, level, point) {
        return None;
    }
    while steps != 0 {
        let direction = steps.signum();
        let next = if direction > 0 {
            ((level + 1)..targets.len()).find(|&level| targets[level].rect.contains(point))
        } else {
            (0..level)
                .rev()
                .find(|&level| targets[level].rect.contains(point))
        };
        let Some(next) = next else { break };
        level = next;
        steps -= direction;
    }
    Some((level, steps))
}

#[derive(Debug, PartialEq, Eq)]
struct ControlWheelNavigation {
    level: usize,
    pending_steps: i32,
    refresh: bool,
}

/// 显式下滚不能把部分路径的末层当作真实叶；先保持父范围，再按需补子路径。
fn control_wheel_navigation(
    path: &ControlPath,
    level: usize,
    point: (i32, i32),
    steps: i32,
    received: Instant,
    now: Instant,
) -> Option<ControlWheelNavigation> {
    let sampled_here = control_level_contains_point(&path.targets, 0, point);
    let (level, remaining) = step_control_level_with_remainder(&path.targets, level, point, steps)?;
    let retry_boundary = remaining < 0
        && (!path.complete
            || !path.terminal
            || now.saturating_duration_since(received) >= PATH_LEASE);
    let refresh = !sampled_here || retry_boundary;
    Some(ControlWheelNavigation {
        level,
        pending_steps: if refresh { remaining.min(0) } else { 0 },
        refresh,
    })
}

/// 只保持同窗口的几何范围，不将矩形相同冒称同一语义控件。
fn remap_control_level(
    previous: Option<(usize, Rect)>,
    window_hwnd: usize,
    targets: &[ControlTarget],
    point: (i32, i32),
) -> Option<usize> {
    previous
        .filter(|(previous_hwnd, _)| *previous_hwnd == window_hwnd)
        .and_then(|(_, selected)| {
            targets
                .iter()
                .position(|target| target.rect == selected && target.rect.contains(point))
        })
}

/// 新点的子路径只能从仍存在的当前几何范围继续，不把旧范围的滚轮转给另一范围。
fn refreshed_control_level(
    previous: Option<(usize, Rect)>,
    window_hwnd: usize,
    targets: &[ControlTarget],
    point: (i32, i32),
    auto_adsorb: bool,
    pending_steps: i32,
) -> Option<usize> {
    let last = targets.len().checked_sub(1)?;
    let remapped = remap_control_level(previous, window_hwnd, targets, point);
    let base = remapped.unwrap_or(if auto_adsorb { 0 } else { last });
    let steps = if previous.is_some() && remapped.is_none() {
        0
    } else {
        pending_steps
    };
    step_control_level(targets, base, point, steps)
}

fn control_preview_is_idle(
    editor: &crate::editor::Editor,
    pointer_down: Option<(i32, i32)>,
) -> bool {
    !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && pointer_down.is_none()
}

fn control_wheel_steps(remainder: &mut f32, notches: f32) -> Option<i32> {
    if !notches.is_finite() {
        return None;
    }
    *remainder = (*remainder + notches.clamp(-128., 128.)).clamp(-128., 128.);
    let steps = remainder.trunc() as i32;
    *remainder -= steps as f32;
    Some(steps)
}

/// 原异步接收条件：界面状态、请求点、画布和原顶层窗口必须都仍匹配。
fn control_probe_is_current(
    can_probe: bool,
    point_is_valid: bool,
    canvas_unchanged: bool,
    current_window_hwnd: Option<usize>,
    requested_window_hwnd: usize,
    epoch_unchanged: bool,
) -> bool {
    can_probe
        && point_is_valid
        && canvas_unchanged
        && current_window_hwnd == Some(requested_window_hwnd)
        && epoch_unchanged
}

fn control_text_copy_target_is_current(
    editor: &crate::editor::Editor,
    requested_version: (u64, u64),
    interface_idle: bool,
) -> bool {
    interface_idle
        && !editor.watermark.has_preview()
        && editor.lasso_draft.is_none()
        && (editor.document.revision(), editor.document.edit_revision()) == requested_version
}

impl CaptureView {
    pub(crate) fn capture_control_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_control_text_in(None, window, cx);
    }
    pub(crate) fn capture_control_text_in(
        &mut self,
        saved: Option<crate::geometry::Rect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.image_canvas.is_some() || self.pin_mode {
            self.perform(Action::Ocr, window, cx);
            return;
        }
        if self.processing
            || self.saving
            || self.editor.is_dragging()
            || self.editor.has_pending_drawing()
        {
            return;
        }
        if self.editor.crop.is_none() {
            let Some(rect) = saved.or_else(|| self.click_selection()) else {
                self.status = tr!("请先选择文字范围", "Select a text range first").into();
                cx.notify();
                return;
            };
            if saved.is_some() && !rect.valid(self.source.bounds.width, self.source.bounds.height) {
                return;
            }
            // 范围先固定在局部变量，退出 Preview 后仍使用同一份文字捕获 ROI。
            self.set_manual_preview(None);
            self.clear_pointer_tracking();
            self.release_capture();
            self.editor.set_crop(Some(rect));
            self.toolbar_monitor = self.source.monitor_at(if saved.is_some() {
                (
                    (rect.x + rect.width / 2) as i32,
                    (rect.y + rect.height / 2) as i32,
                )
            } else {
                self.pointer
            });
        }
        let rect = match self.selected() {
            Ok(rect) => rect,
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
        };
        if saved.is_some_and(|saved| saved != rect) {
            return;
        }
        self.set_manual_preview(None);
        // 与控件坐标对齐，文字捕获读取冻结底图，不包含标注、尺寸标签或输出装饰。
        let image = match self.source.read_region(rect) {
            Ok(image) => image,
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
        };
        let canvas = self.source.bounds;
        let targets = if self.source.is_image() {
            Vec::new()
        } else {
            self.window_targets.clone()
        };
        // UIA 优先链提交时固定一次；查询耗时或设置变化不改变后续 OCR 补救档位。
        let preferences = crate::preferences::Preferences::load();
        let copy_only = preferences.text_capture_copy_only;
        let profile = preferences.ocr_profile;
        let requested_version = (
            self.editor.document.revision(),
            self.editor.document.edit_revision(),
        );
        let standard_ocr = self.editor.document.items().is_empty();
        self.invalidate_ocr_display_request();
        let foreground_request = self.foreground_ocr.start(text_result::InputTarget::Frozen(
            self.frozen_ocr_target_in(rect),
        ));
        let task_token = foreground_request.token.clone();
        self.processing = true;
        self.status = tr!("正在捕获选区文字…", "Capturing text in the selection…").into();
        let task = cx.background_executor().spawn(async move {
            let query_targets = targets.clone();
            let result = control_text_with_ocr_fallback(
                &task_token,
                || {
                    crate::control_selection::text(canvas, query_targets, rect).map(|text| {
                        text.filter(|text| !text.trim().is_empty()).map(|text| {
                            ControlTextOutcome::Text(
                                text,
                                tr!("控件文字（当前值）", "Control text (current value)"),
                            )
                        })
                    })
                },
                || {
                    task_token.checkpoint()?;
                    let native = if targets.is_empty() {
                        Vec::new()
                    } else {
                        crate::control_selection::text_blocks(canvas, targets, rect)
                            .unwrap_or_default()
                    };
                    // UIA 空值/失败仍允许原来的 OCR 补救；用户取消禁止进入补救。
                    task_token.checkpoint()?;
                    if native.is_empty() && standard_ocr {
                        return Ok(ControlTextOutcome::StandardOcr(profile));
                    }
                    crate::recognition::recognize_with_profile_cancellable(
                        image,
                        crate::recognition::OcrProvider::default(),
                        profile,
                        &task_token,
                    )
                    .map(|result| {
                        let merged = crate::hybrid_text::merge(rect, &native, &result);
                        let title = match (merged.native_lines > 0, merged.ocr_lines > 0) {
                            (true, true) => tr!(
                                "控件当前值与冻结画面文字",
                                "Control value and text from the frozen screen"
                            ),
                            (true, false) => {
                                tr!("控件文字（当前值）", "Control text (current value)")
                            }
                            _ => tr!("冻结画面文字", "Text from the frozen screen"),
                        };
                        ControlTextOutcome::Text(merged.text, title)
                    })
                },
            );
            ocr_task::Receipt::from_result(result)
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                let Some(result) = view.foreground_ocr.finish_receipt(foreground_request.id, result) else {
                    return;
                };
                view.processing = false;
                let result = match result {
                    ocr_task::Receipt::Completed(value) => Ok(value),
                    ocr_task::Receipt::Failed(error) => Err(error),
                    ocr_task::Receipt::Cancelled => {
                        if view.status == tr!("正在捕获选区文字…", "Capturing text in the selection…") {
                            view.status.clear();
                        }
                        cx.notify();
                        return;
                    }
                };
                if view.source.bounds != canvas || view.editor.crop != Some(rect)
                    || !view.current_frozen_ocr_target().is_some_and(|current| {
                        foreground_request.key.is_current(&current, true)
                    })
                {
                    view.status = tr!("选区已改变，请重新捕获文字", "The selection changed; capture the text again").into();
                } else {
                    let target_current = !copy_only
                        || control_text_copy_target_is_current(
                            &view.editor,
                            requested_version,
                            view.text_copy_completion_is_idle()
                                && view.screenshot_toolbar_menu.presentation().is_none()
                                && view.recognition_result.is_none(),
                        );
                    match result {
                        Ok(_) if !target_current => {
                            view.status = tr!("文字捕获期间截图范围或内容已改变，请重新捕获文字", "The capture area or content changed while capturing text; capture it again").into();
                        }
                        Ok(ControlTextOutcome::StandardOcr(profile)) => {
                            // 交回标准 OCR：沿用提交 UIA 时的模型和自动复制设置。
                            view.recognize_with_profile_and_copy_policy(
                                false, copy_only, None, Some(profile), window, cx,
                            );
                        }
                        Ok(ControlTextOutcome::Text(text, title))
                            if !text.trim().is_empty() && copy_only =>
                        {
                            match text_result::publish_auto_copy(text, |text| {
                                crate::export::copy_text(text, crate::gpui_capture::hwnd(window))
                            }) {
                                text_result::AutoCopyPublication::Close => {
                                    view.perform(Action::Cancel, window, cx);
                                }
                                text_result::AutoCopyPublication::KeepResult { text, error } => {
                                    view.show_control_text_result(text, title);
                                    view.retain_auto_copy_failure(&error, foreground_request.key.clone());
                                }
                            }
                        }
                        Ok(ControlTextOutcome::Text(text, title)) => {
                            view.status.clear();
                            view.show_control_text_result(text, title);
                        }
                        Err(error) => view.status = error,
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn can_probe_controls(&self) -> bool {
        self.control_selection_ready
            && self.image_canvas.is_none()
            && !self.pin_mode
            && self.recognition_result.is_none()
            && self.editor.crop.is_none()
            && self.manual_preview.is_none()
            && control_preview_is_idle(&self.editor, self.pointer_down_start)
            && !self.saving
            && !self.pin_handoff_pending
            && !self.processing
            && self.toolbar_panel == crate::toolbar_layout::ToolbarPanel::None
    }

    fn current_window_target(&self) -> Option<&crate::selection_assist::WindowTarget> {
        self.window_targets
            .iter()
            .find(|target| target.rect.contains(self.pointer))
    }

    fn usable_control_hover(&self) -> Option<&ControlHover> {
        let hover = self.control_hover.as_ref()?;
        (control_level_contains_point(&hover.path.targets, hover.level, self.pointer)
            && (hover.path.state != ControlPathState::WindowOnly
                || self
                    .source
                    .bounds
                    .left
                    .checked_add(self.pointer.0)
                    .zip(self.source.bounds.top.checked_add(self.pointer.1))
                    == Some(hover.path.point))
            && self.current_window_target()?.hwnd == hover.window_hwnd)
            .then_some(hover)
    }

    pub(crate) fn hover_selection(&self) -> Option<ControlTarget> {
        let navigated = self
            .usable_control_hover()
            .is_some_and(|hover| hover.navigated);
        self.raw_hover_selection().and_then(|preview| {
            if self.auto_adsorb && preview.window_fallback {
                // 查询重试时展示可短暂保留细框；点击采用同一范围，不能点成后方窗口。
                if let Some(target) = self.control_tracking.feedback.retained_automatic_target(
                    self.pointer,
                    self.current_window_target().map(|window| window.hwnd),
                    Instant::now(),
                ) {
                    // 已显示范围已做过单屏降级；显式跨屏父范围不能再被裁回单屏。
                    return Some(target);
                }
                if !self.control_tracking.feedback.automatic_window_allowed(
                    self.current_window_target().map(|window| window.hwnd),
                    Instant::now(),
                ) {
                    return None;
                }
            }
            let mut target = preview.target;
            // 滚轮主动选到的上级保留；自动目标不以“所有屏幕”作首个预览。
            if !navigated {
                target.rect = crate::startup_preview::auto_target_rect(
                    target.rect,
                    (0..self.source.screen_count()).map(|i| self.source.screen_rect(i)),
                    self.pointer,
                );
            }
            Some(target)
        })
    }

    fn raw_hover_selection(&self) -> Option<ControlPreviewTarget> {
        if let Some(hover) = self
            .usable_control_hover()
            .filter(|hover| control_hover_preview_ready(hover, self.auto_adsorb))
        {
            return hover
                .path
                .targets
                .get(hover.level)
                .cloned()
                .map(|mut target| {
                    if target.title.trim().is_empty() {
                        target.title = match hover.path.state {
                            ControlPathState::WindowOnly => tr!("窗口", "Window").into(),
                            ControlPathState::ControlView | ControlPathState::RawView => {
                                tr!("界面控件", "UI control").into()
                            }
                            ControlPathState::NativeChild => {
                                tr!("窗口控件", "Window control").into()
                            }
                        };
                    }
                    ControlPreviewTarget {
                        target,
                        window_fallback: false,
                    }
                });
        }
        // Down 可先于 Move；已访问的有效细元素仍可以按实际事件点立即命中。
        if let Some(target) = self.cached_terminal_target() {
            return Some(ControlPreviewTarget {
                target,
                window_fallback: false,
            });
        }
        // 窗口命中只查冻结缓存，不等待 provider、最深元素或重试。
        self.current_window_target()
            .map(|target| ControlPreviewTarget {
                target: ControlTarget {
                    rect: target.rect,
                    title: target.title.clone(),
                },
                window_fallback: true,
            })
    }

    fn cached_terminal_target(&self) -> Option<ControlTarget> {
        if !self.auto_adsorb {
            return None;
        }
        let window = self.current_window_target()?;
        self.control_tracking
            .cache
            .iter()
            .rev()
            .filter(|cached| {
                cached.canvas == self.source.bounds
                    && same_window(&cached.window, window)
                    && cached.received.elapsed() < PATH_LEASE
                    && cached.path.complete
                    && cached.path.terminal
                    && control_level_contains_point(&cached.path.targets, 0, self.pointer)
            })
            .min_by_key(|cached| {
                cached.path.targets[0].rect.width as u64 * cached.path.targets[0].rect.height as u64
            })
            .and_then(|cached| cached.path.targets.first().cloned())
    }

    pub(crate) fn click_selection(&self) -> Option<Rect> {
        if self.image_canvas.is_some() || self.pin_mode || self.editor.crop.is_some() {
            return None;
        }
        if let Some(rect) = self.manual_preview {
            return Some(rect);
        }
        self.hover_selection()
            .map(|target| target.rect)
            .or_else(|| {
                if self.auto_adsorb {
                    return None;
                }
                let rect = self
                    .source
                    .screen_rect(self.source.monitor_at(self.pointer));
                rect.contains(self.pointer).then_some(rect)
            })
    }

    pub(crate) fn presented_control_selection(&self, window: &Window) -> Option<Rect> {
        self.presented_capture_rect(window, &self.painted_preview)
    }

    pub(crate) fn presented_selection(&self, window: &Window) -> Option<Rect> {
        self.presented_capture_rect(window, &self.painted_selection)
    }

    fn presented_capture_rect(
        &self,
        window: &Window,
        receipt: &std::cell::Cell<Option<crate::preview_middle_pin::PaintedPreview>>,
    ) -> Option<Rect> {
        if !crate::preview_middle_pin::physical_size_matches(
            self.source.full(),
            (
                f32::from(window.viewport_size().width),
                f32::from(window.viewport_size().height),
            ),
            window.scale_factor(),
        ) {
            return None;
        }
        let hwnd = crate::gpui_capture::hwnd(window);
        let serial = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetPropW(
                hwnd,
                windows::core::w!("RustShot.GpuiPresentSerial"),
            )
        }
        .0 as usize;
        let size = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetPropW(
                hwnd,
                windows::core::w!("RustShot.GpuiPresentSize"),
            )
        }
        .0 as usize as u32;
        receipt
            .get()
            .and_then(|painted| painted.presented(self.source.full(), serial, size))
    }

    pub(crate) fn display_control_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ControlTarget> {
        if let Some(rect) = self.manual_preview.filter(|rect| {
            self.image_canvas.is_none()
                && !self.pin_mode
                && self.recognition_result.is_none()
                && self.editor.crop.is_none()
                && !self.editor.is_dragging()
                && !self.editor.has_pending_drawing()
                && rect.valid(self.source.bounds.width, self.source.bounds.height)
        }) {
            self.control_tracking.feedback.clear();
            return Some(ControlTarget {
                rect,
                title: tr!("屏幕范围", "Screen area").into(),
            });
        }
        if self.image_canvas.is_some()
            || self.pin_mode
            || self.recognition_result.is_some()
            || self.editor.crop.is_some()
            || !control_preview_is_idle(&self.editor, self.pointer_down_start)
        {
            self.control_tracking.feedback.clear();
            return None;
        }
        let current_window = self.current_window_target();
        let window_hwnd = current_window.map(|target| target.hwnd);
        let now = Instant::now();
        let context = ControlHoverPointContext {
            canvas: self.source.bounds,
            window: current_window,
            epoch: self.control_tracking.epoch,
            point: self.pointer,
            auto_adsorb: self.auto_adsorb,
            eligible: self.can_probe_controls(),
            now,
        };
        let hover = self
            .usable_control_hover()
            .filter(|hover| control_hover_preview_ready(hover, self.auto_adsorb));
        let candidate = self.raw_hover_selection().map(|preview| {
            let mut candidate = control_display_feedback_candidate(
                preview.target,
                hover,
                &context,
                (0..self.source.screen_count()).map(|i| self.source.screen_rect(i)),
            );
            candidate.window_fallback = preview.window_fallback;
            if self.auto_adsorb && hover.is_none() && !preview.window_fallback {
                // 只有已完成的终端缓存有细元素资格，冻结窗口不能冒称查询完成。
                candidate.refined = true;
                candidate.terminal = true;
                candidate.complete = true;
                candidate.leaf = true;
            }
            candidate
        });
        let hover_ready = hover.is_some();
        let (shown, active) = if self.auto_adsorb {
            self.control_tracking.feedback.update_automatic(
                candidate,
                self.pointer,
                window_hwnd,
                now,
            )
        } else {
            self.control_tracking
                .feedback
                .update(candidate, self.pointer, window_hwnd, now)
        };
        if active {
            window.request_animation_frame();
        }
        if let Some(at) = self.control_tracking.feedback.wake_at() {
            self.schedule_feedback_wake(at, cx);
        }
        if crate::uia_log::enabled() {
            let rect = shown.as_ref().map(|target| target.rect);
            if self.control_tracking.logged_shown != Some(rect) {
                self.control_tracking.logged_shown = Some(rect);
                crate::uia_log::uia_log!(
                    "显示 {:?} pointer={:?} hwnd={:?} auto={} hover[{}] ready={}",
                    rect,
                    self.pointer,
                    window_hwnd,
                    self.auto_adsorb,
                    control_hover_summary(self.control_hover.as_ref()),
                    hover_ready
                );
            }
        }
        shown
    }

    pub(crate) fn suspend_control_tracking(&mut self) {
        crate::uia_log::uia_log!("暂停控件跟踪 epoch={}", self.control_tracking.epoch);
        self.control_tracking.epoch = self.control_tracking.epoch.wrapping_add(1);
        self.control_tracking.paused = true;
        self.control_tracking.feedback.clear();
        if let Some(worker) = &self.control_tracking.worker {
            worker.cancel();
        }
        self.last_control_request = None;
        self.control_retry_after = None;
        self.control_retries_at_point = 0;
        self.control_pending_navigation = 0;
        self.control_wheel_remainder = 0.;
    }

    pub(crate) fn invalidate_capture_controls(&mut self) {
        self.suspend_control_tracking();
        self.control_tracking.cache.clear();
        self.control_hover = None;
        self.hover_window = None;
    }

    /// 只接收已固定的物理范围；Native handoff 不在这里改 crop 或重采鼠标。
    pub(crate) fn set_manual_preview(&mut self, rect: Option<Rect>) {
        let rect =
            rect.filter(|rect| rect.valid(self.source.bounds.width, self.source.bounds.height));
        if self.manual_preview == rect {
            return;
        }
        self.manual_preview = rect;
        self.manual_preview_click = None;
        self.manual_preview_dragged = false;
        // 范围变化后的旧 paint/Present 不能冒充新范围已经可见。
        self.painted_preview.set(None);
        self.suspend_control_tracking();
        self.control_hover = None;
        self.hover_window = None;
    }

    /// 保留旧框/放大待确认到期时只唤醒一次重绘，不逐帧请求。
    fn schedule_feedback_wake(&mut self, at: Instant, cx: &mut Context<Self>) {
        if self
            .control_tracking
            .wake_at
            .is_some_and(|scheduled| scheduled <= at)
        {
            return;
        }
        self.control_tracking.wake_at = Some(at);
        let delay = at.saturating_duration_since(Instant::now());
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            executor.timer(delay).await;
            let _ = view.update(cx, |view, cx| {
                if view.control_tracking.wake_at == Some(at) {
                    view.control_tracking.wake_at = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn schedule_control_refresh(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if self.control_tracking.timer_pending {
            return;
        }
        self.control_tracking.timer_pending = true;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            executor.timer(delay).await;
            let _ = view.update(cx, |view, cx| {
                view.control_tracking.timer_pending = false;
                view.request_control_probe_inner(view.control_pending_navigation != 0, cx);
            });
        })
        .detach();
    }

    pub(crate) fn request_control_probe(&mut self, cx: &mut Context<Self>) {
        self.request_control_probe_inner(false, cx);
    }

    fn request_control_probe_inner(&mut self, manual: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        let current_window = self.current_window_target().cloned();
        // 即使旧请求仍在途，也先发布真实最新点，让其在下一查询边界停止旧遍历。
        if let Some(worker) = &self.control_tracking.worker {
            if let Some(global) = self
                .source
                .bounds
                .left
                .checked_add(self.pointer.0)
                .zip(self.source.bounds.top.checked_add(self.pointer.1))
            {
                worker.observe(self.source.bounds, current_window.as_ref(), global);
            } else {
                worker.cancel();
            }
        }
        self.control_tracking.feedback.observe_pointer(
            self.pointer,
            current_window.as_ref().map(|window| window.hwnd),
            now,
        );
        if !self.can_probe_controls() {
            if !self.control_tracking.paused {
                self.suspend_control_tracking();
            }
            return;
        }
        self.control_tracking.paused = false;
        if !self.auto_adsorb && !manual {
            return;
        }
        let Some(target) = current_window else {
            return;
        };
        let canvas = self.source.bounds;
        let point = self.pointer;
        self.control_tracking.cache.retain(|cached| {
            now.saturating_duration_since(cached.received) < PATH_LEASE && cached.canvas == canvas
        });
        if self
            .usable_control_hover()
            .is_none_or(|hover| self.auto_adsorb && !control_hover_terminal_ready(hover))
        {
            let cached = self
                .control_tracking
                .cache
                .iter()
                .rev()
                .filter(|cached| {
                    same_window(&cached.window, &target)
                        && cached.path.complete
                        && cached.path.terminal
                        && control_level_contains_point(&cached.path.targets, 0, point)
                })
                .min_by_key(|cached| {
                    cached.path.targets[0].rect.width as u64
                        * cached.path.targets[0].rect.height as u64
                });
            if let Some(cached) = cached {
                self.control_hover = Some(ControlHover {
                    path: cached.path.clone(),
                    level: 0,
                    window_hwnd: target.hwnd,
                    received: cached.received,
                    navigated: false,
                    retained_for_query: false,
                    owner: ControlHoverOwner {
                        canvas,
                        window: target.clone(),
                        epoch: self.control_tracking.epoch,
                    },
                });
                cx.notify();
            }
        }
        let same_point = self.last_control_request == Some(point);
        let retry = self.control_retry_after.is_some_and(|after| now >= after)
            && self.control_retries_at_point == 0;
        if !control_probe_request_ready(
            self.usable_control_hover(),
            point,
            self.last_control_request,
            manual,
            retry,
            self.control_request_pending,
            now,
        ) {
            return;
        }
        if !same_point {
            self.control_retry_after = None;
            self.control_retries_at_point = 0;
        } else if retry {
            self.control_retry_after = None;
            self.control_retries_at_point += 1;
        }
        // 间隔只限制 provider 请求；timer 到期采当时最新点，绝不先锁旧点再等待。
        if let Some(last) = self.control_tracking.last_request_at {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < REFRESH_INTERVAL {
                self.schedule_control_refresh(REFRESH_INTERVAL - elapsed, cx);
                return;
            }
        }
        let Some(global) = canvas
            .left
            .checked_add(point.0)
            .zip(canvas.top.checked_add(point.1))
        else {
            return;
        };
        let worker = match &self.control_tracking.worker {
            Some(worker) => worker.clone(),
            None => match crate::control_probe_worker::ControlProbeWorker::new() {
                Ok(worker) => {
                    let worker = Arc::new(worker);
                    self.control_tracking.worker = Some(worker.clone());
                    worker
                }
                Err(error) => {
                    crate::uia_log::uia_log!("控件查询线程创建失败：{error}");
                    return;
                }
            },
        };
        let epoch = self.control_tracking.epoch;
        crate::uia_log::uia_log!(
            "请求 point={:?} global={:?} manual={} hwnd={:#x} pending_nav={} hover[{}]",
            point,
            global,
            manual,
            target.hwnd,
            self.control_pending_navigation,
            control_hover_summary(self.control_hover.as_ref())
        );
        worker.observe(canvas, Some(&target), global);
        let window_hwnd = target.hwnd;
        let frozen_window = target.clone();
        self.control_request_pending = true;
        self.last_control_request = Some(point);
        self.control_tracking.last_request_at = Some(now);
        let task = worker.probe(canvas, target, global);
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                view.control_request_pending = false;
                let same_context = view.can_probe_controls()
                    && view.source.bounds == canvas
                    && view.control_tracking.epoch == epoch
                    && control_retained_query_is_current(
                        view.control_hover.as_ref(),
                        view.pointer,
                        point,
                    )
                    && view
                        .current_window_target()
                        .is_some_and(|current| same_window(current, &frozen_window));
                let mut refined = false;
                if !same_context || result.as_ref().is_ok_and(|path| path.point != global) {
                    crate::uia_log::uia_log!(
                        "结果丢弃 point={:?} 当前={:?} same_context={} can_probe={} epoch={}→{}",
                        point,
                        view.pointer,
                        same_context,
                        view.can_probe_controls(),
                        epoch,
                        view.control_tracking.epoch
                    );
                }
                if let Ok(path) = result {
                    if same_context && path.point == global && !path.targets.is_empty() {
                        // 完成于旧点的合法细路径仍可在短租约内复用，不把COM对象传给UI。
                        if path.complete
                            && path.terminal
                            && path.state != ControlPathState::WindowOnly
                        {
                            view.control_tracking.cache.push_back(CachedPath {
                                canvas,
                                window: frozen_window.clone(),
                                path: path.clone(),
                                received: Instant::now(),
                            });
                            while view.control_tracking.cache.len() > 32 {
                                view.control_tracking.cache.pop_front();
                            }
                        }
                        if control_probe_is_current(
                            view.can_probe_controls(),
                            path_contains_current_point(&path, view.pointer, point),
                            view.source.bounds == canvas,
                            view.current_window_target().map(|target| target.hwnd),
                            window_hwnd,
                            view.control_tracking.epoch == epoch,
                        ) {
                            // 安全部分路径不等于完成细化，同点停住仍须有一次恢复查询。
                            refined = path.state != ControlPathState::WindowOnly && path.complete;
                            // 首屏窗口已被滚轮选中时，细化也从该窗口层继续。
                            let previous = control_navigated_range(view.usable_control_hover());
                            let navigated = previous.is_some()
                                || control_refresh_keeps_navigation(
                                    view.usable_control_hover(),
                                    view.control_pending_navigation,
                                );
                            if let Some(level) = refreshed_control_level(
                                previous,
                                window_hwnd,
                                &path.targets,
                                view.pointer,
                                view.auto_adsorb,
                                view.control_pending_navigation,
                            ) {
                                view.control_hover = Some(ControlHover {
                                    path,
                                    level,
                                    window_hwnd,
                                    received: Instant::now(),
                                    navigated,
                                    retained_for_query: false,
                                    owner: ControlHoverOwner {
                                        canvas,
                                        window: frozen_window.clone(),
                                        epoch,
                                    },
                                });
                                view.control_pending_navigation = 0;
                                crate::uia_log::uia_log!(
                                    "采用 hover[{}]",
                                    control_hover_summary(view.control_hover.as_ref())
                                );
                            } else {
                                crate::uia_log::uia_log!("结果无可用层 pointer={:?}", view.pointer);
                            }
                        } else {
                            crate::uia_log::uia_log!(
                                "结果过期 point={:?} 当前={:?}",
                                point,
                                view.pointer
                            );
                        }
                    }
                }
                if same_context
                    && view.pointer == point
                    && !refined
                    && view.control_retries_at_point == 0
                    && view.auto_adsorb
                {
                    view.control_retry_after = Some(Instant::now() + Duration::from_millis(120));
                    view.schedule_control_refresh(Duration::from_millis(125), cx);
                }
                // 在途时的所有Move由view.pointer合并；旧请求完成立即启动最新点，不再等60ms。
                if view.can_probe_controls() && (view.pointer != point || !same_context) {
                    view.request_control_probe_inner(view.control_pending_navigation != 0, cx);
                } else if !view.can_probe_controls() {
                    view.control_pending_navigation = 0;
                }
                cx.notify();
            });
        })
        .detach();
    }
    /// Preview 中上滚选择父范围、下滚返回当前点的子范围；不改变已确认选区。
    pub(crate) fn navigate_control_selection(
        &mut self,
        notches: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.can_probe_controls() || !notches.is_finite() {
            return false;
        }
        if self.usable_control_hover().is_none() {
            let Some(target) = self.current_window_target().cloned() else {
                return false;
            };
            let Some(global) = self
                .source
                .bounds
                .left
                .checked_add(self.pointer.0)
                .zip(self.source.bounds.top.checked_add(self.pointer.1))
            else {
                return false;
            };
            // 冻结窗口几何仅用于保留同点的半刻度，不冒称完成了 UIA 查询。
            self.control_hover = Some(ControlHover {
                owner: ControlHoverOwner {
                    canvas: self.source.bounds,
                    window: target.clone(),
                    epoch: self.control_tracking.epoch,
                },
                path: ControlPath {
                    point: global,
                    targets: vec![ControlTarget {
                        rect: target.rect,
                        title: target.title,
                    }],
                    state: ControlPathState::WindowOnly,
                    terminal: false,
                    complete: false,
                },
                level: 0,
                window_hwnd: target.hwnd,
                received: Instant::now(),
                navigated: true,
                retained_for_query: false,
            });
        }
        let steps = control_wheel_steps(&mut self.control_wheel_remainder, notches)
            .expect("已验证的有限滚轮刻度");
        crate::uia_log::uia_log!(
            "滚轮 notches={notches} steps={steps} pointer={:?} hover[{}]",
            self.pointer,
            control_hover_summary(self.control_hover.as_ref())
        );
        if steps == 0 {
            return true;
        }
        let hover = self.control_hover.as_mut().expect("已验证的控件范围");
        let navigation = control_wheel_navigation(
            &hover.path,
            hover.level,
            self.pointer,
            self.control_pending_navigation.saturating_add(steps),
            hover.received,
            Instant::now(),
        )
        .expect("当前层必须包含当前点");
        let refresh = navigation.refresh || hover.path.state == ControlPathState::WindowOnly;
        hover.level = navigation.level;
        hover.navigated = true;
        self.control_pending_navigation = navigation.pending_steps;
        cx.notify();
        if refresh {
            // 窗口已是最外层，不累计上滚余量；未知子级在细化后从当前范围继续。
            // 手动滚轮也允许在自动吸附关闭时按需检测。
            self.last_control_request = None;
            self.request_control_probe_inner(true, cx);
        }
        true
    }

    pub(crate) fn invalidate_control_hover(&mut self) {
        let window = self.current_window_target().cloned();
        let now = Instant::now();
        self.control_tracking.feedback.observe_pointer(
            self.pointer,
            window.as_ref().map(|target| target.hwnd),
            now,
        );
        let context = ControlHoverPointContext {
            canvas: self.source.bounds,
            window: window.as_ref(),
            epoch: self.control_tracking.epoch,
            point: self.pointer,
            auto_adsorb: self.auto_adsorb,
            eligible: self.can_probe_controls(),
            now,
        };
        let had_hover = self.control_hover.is_some();
        let decision = update_control_hover_point(
            &mut self.control_hover,
            &mut self.control_pending_navigation,
            &mut self.control_wheel_remainder,
            &mut self.last_control_request,
            &context,
        );
        if had_hover && decision != ControlHoverPointDecision::Keep {
            crate::uia_log::uia_log!(
                "移动 {:?} pointer={:?} hover[{}]",
                decision,
                self.pointer,
                control_hover_summary(self.control_hover.as_ref())
            );
        }
    }
}

#[cfg(test)]
mod control_text_copy_completion_tests {
    use super::*;
    use crate::{annotation::Tool, capture_view::text_copy_editor_is_idle, editor::Editor};

    fn editor() -> Editor {
        let canvas = Rect::full(400, 300);
        Editor::new(canvas, Some(canvas))
    }

    fn version(editor: &Editor) -> (u64, u64) {
        (editor.document.revision(), editor.document.edit_revision())
    }

    fn current(editor: &Editor, requested: (u64, u64)) -> bool {
        control_text_copy_target_is_current(editor, requested, text_copy_editor_is_idle(editor))
    }

    #[test]
    fn control_text_copy_unchanged_idle_target_remains_eligible_without_output_transforms() {
        let mut editor = editor();
        // 已提交套索不改变文字捕获读取原始 ROI 的语义。
        editor
            .document
            .set_crop_mask(crate::crop_mask::Polygon::new(&[
                crate::annotation::Point { x: 20., y: 20. },
                crate::annotation::Point { x: 200., y: 20. },
                crate::annotation::Point { x: 100., y: 160. },
            ]));
        let requested = version(&editor);
        let crop = editor.crop;
        let mask = editor.document.crop_mask.clone();
        assert!(mask.is_some());
        assert!(current(&editor, requested));
        assert!(!control_text_copy_target_is_current(
            &editor, requested, false
        ));
        assert_eq!(version(&editor), requested);
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, mask);
        assert!(editor.document.items().is_empty());
    }

    #[test]
    fn control_text_copy_completed_edit_and_undo_cannot_reuse_the_old_request() {
        let mut editor = editor();
        let requested = version(&editor);
        let crop = editor.crop;
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((40, 40), 4.);
        editor.pointer_up((140, 100));
        assert_eq!(editor.document.items().len(), 1);
        assert!(text_copy_editor_is_idle(&editor));
        assert_eq!(editor.crop, crop);
        assert!(!current(&editor, requested));
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert_eq!(editor.crop, crop);
        assert!(!current(&editor, requested));
        editor.redo();
        assert_eq!(editor.document.items().len(), 1);
        assert!(!current(&editor, requested));
    }

    #[test]
    fn control_text_copy_uncommitted_input_blocks_completion_without_document_changes() {
        let mut editor = editor();
        let requested = version(&editor);
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((40, 40), 4.);
        editor.pointer_move((140, 100));
        assert!(editor.is_dragging());
        assert_eq!(version(&editor), requested);
        assert!(!current(&editor, requested));
        editor.cancel_gesture();

        editor.select_tool(Tool::Arrow);
        editor.pointer_down((40, 40), 4.);
        editor.pointer_up((40, 40));
        assert!(!editor.is_dragging());
        assert!(editor.has_pending_drawing());
        assert_eq!(version(&editor), requested);
        assert!(!current(&editor, requested));
        editor.cancel_gesture();

        editor.select_tool(Tool::Text);
        editor.begin_text(None, crate::annotation::Point::from_tuple((40, 40)));
        assert!(editor.text_draft.is_some());
        assert_eq!(version(&editor), requested);
        assert!(!current(&editor, requested));
        editor.cancel_text();

        let binding = editor.watermark.binding();
        editor.watermark.set_preview(binding, "未提交水印").unwrap();
        assert!(text_copy_editor_is_idle(&editor));
        assert_eq!(version(&editor), requested);
        assert!(!current(&editor, requested));
        editor.watermark.cancel_preview(binding).unwrap();

        editor.select_tool(Tool::Lasso);
        editor.pointer_down((40, 40), 4.);
        editor.pointer_move((140, 40));
        editor.pointer_move((140, 100));
        editor.pointer_up((40, 100));
        assert!(!editor.is_dragging());
        assert!(editor.lasso_draft.is_some());
        assert!(text_copy_editor_is_idle(&editor));
        assert_eq!(version(&editor), requested);
        assert!(!current(&editor, requested));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn targets(rects: &[Rect]) -> Vec<ControlTarget> {
        rects
            .iter()
            .map(|&rect| ControlTarget {
                rect,
                title: String::new(),
            })
            .collect()
    }

    fn old_path() -> Vec<ControlTarget> {
        targets(&[
            rect(10, 10, 20, 20),
            rect(0, 0, 200, 200),
            rect(0, 0, 400, 300),
        ])
    }

    fn new_path() -> Vec<ControlTarget> {
        targets(&[
            rect(70, 70, 20, 20),
            rect(50, 50, 100, 100),
            rect(0, 0, 200, 200),
            rect(0, 0, 400, 300),
        ])
    }

    #[test]
    fn control_parent_hold_cached_leaf_reuses_path_across_points() {
        let path = old_path();
        let original_sample = (20, 20);
        for point in [(11, 11), (24, 18), (29, 29)] {
            assert_ne!(point, original_sample);
            assert!(control_level_contains_point(&path, 0, point));
            assert_eq!(step_control_level(&path, 1, point, -1), Some(0));
        }
        for point in [(30, 20), (80, 80), (199, 199)] {
            assert!(control_level_contains_point(&path, 1, point));
            assert!(!control_level_contains_point(&path, 0, point));
            assert_eq!(step_control_level(&path, 1, point, -1), Some(1));
        }
    }

    #[test]
    fn control_parent_hold_parent_contains_new_point_outside_leaf() {
        let path = old_path();
        assert!(!control_level_contains_point(&path, 0, (80, 80)));
        assert!(control_level_contains_point(&path, 1, (80, 80)));
        assert_eq!(step_control_level(&path, 1, (80, 80), 0), Some(1));
    }

    #[test]
    fn control_parent_hold_leaving_parent_is_rejected() {
        let path = old_path();
        for point in [(-1, 80), (200, 80), (80, 200), (250, 250)] {
            assert!(!control_level_contains_point(&path, 1, point));
            assert_eq!(step_control_level(&path, 1, point, -1), None);
        }
        assert!(!control_level_contains_point(&path, path.len(), (80, 80)));
    }

    #[test]
    fn control_parent_hold_child_navigation_never_enters_old_leaf() {
        let path = old_path();
        assert_eq!(step_control_level(&path, 1, (80, 80), -1), Some(1));
        assert_eq!(step_control_level(&path, 1, (80, 80), -5), Some(1));
        assert_eq!(step_control_level(&path, 1, (20, 20), -1), Some(0));
        assert_eq!(step_control_level(&path, 1, (80, 80), 1), Some(2));
    }

    #[test]
    fn control_parent_hold_refresh_remaps_same_window_parent_rect() {
        let old = old_path();
        let refreshed = new_path();
        assert_eq!(
            remap_control_level(Some((11, old[1].rect)), 11, &refreshed, (80, 80)),
            Some(2)
        );
        assert_ne!(old[1].rect, refreshed[1].rect);
    }

    #[test]
    fn control_parent_hold_remap_rejects_foreign_window_and_absent_rect() {
        let old = old_path();
        let refreshed = new_path();
        assert_eq!(
            remap_control_level(Some((11, old[1].rect)), 12, &refreshed, (80, 80)),
            None
        );
        assert_eq!(
            remap_control_level(Some((11, old[0].rect)), 11, &refreshed, (80, 80)),
            None
        );
        assert_eq!(
            remap_control_level(Some((11, old[1].rect)), 11, &refreshed, (250, 250)),
            None
        );
        assert_eq!(remap_control_level(None, 11, &refreshed, (80, 80)), None);
    }

    #[test]
    fn control_parent_hold_completion_uses_latest_selected_range() {
        let old = old_path();
        let refreshed = new_path();
        let selected_before_request = old[1].rect;
        let selected_at_completion = old[2].rect;
        assert_eq!(
            remap_control_level(Some((11, selected_at_completion)), 11, &refreshed, (80, 80)),
            Some(3)
        );
        assert_eq!(
            remap_control_level(
                Some((11, selected_before_request)),
                11,
                &refreshed,
                (80, 80)
            ),
            Some(2)
        );
    }

    #[test]
    fn control_parent_hold_late_result_rejects_point_canvas_window_and_disabled() {
        assert!(control_probe_is_current(
            true,
            true,
            true,
            Some(11),
            11,
            true
        ));
        assert!(!control_probe_is_current(
            false,
            true,
            true,
            Some(11),
            11,
            true
        ));
        assert!(!control_probe_is_current(
            true,
            false,
            true,
            Some(11),
            11,
            true
        ));
        assert!(!control_probe_is_current(
            true,
            true,
            false,
            Some(11),
            11,
            true
        ));
        assert!(!control_probe_is_current(
            true,
            true,
            true,
            Some(12),
            11,
            true
        ));
        assert!(!control_probe_is_current(true, true, true, None, 11, true));
        assert!(!control_probe_is_current(
            true,
            true,
            true,
            Some(11),
            11,
            false
        ));
    }

    #[test]
    fn control_hover_completed_leaf_accepts_motion_inside_and_rejects_sibling() {
        let path = ControlPath {
            point: (-100, 20),
            targets: old_path(),
            state: ControlPathState::ControlView,
            terminal: true,
            complete: true,
        };
        assert!(path_contains_current_point(&path, (21, 22), (20, 20)));
        assert!(!path_contains_current_point(&path, (80, 80), (20, 20)));
        let window = ControlPath {
            state: ControlPathState::WindowOnly,
            targets: targets(&[rect(0, 0, 400, 300)]),
            ..path
        };
        assert!(path_contains_current_point(&window, (20, 20), (20, 20)));
        assert!(!path_contains_current_point(&window, (21, 20), (20, 20)));
    }

    #[test]
    fn control_parent_hold_step_is_bounded_at_hierarchy_edges() {
        let path = old_path();
        assert_eq!(step_control_level(&path, 1, (20, 20), i32::MIN), Some(0));
        assert_eq!(step_control_level(&path, 1, (20, 20), i32::MAX), Some(2));
        assert_eq!(step_control_level(&path, 0, (20, 20), 0), Some(0));
        assert_eq!(step_control_level(&[], 0, (20, 20), -1), None);
    }

    #[test]
    fn control_parent_hold_fresh_children_become_navigable_after_refresh() {
        let old = old_path();
        let refreshed = new_path();
        assert_eq!(step_control_level(&old, 1, (80, 80), -1), Some(1));
        let retained = remap_control_level(Some((11, old[1].rect)), 11, &refreshed, (80, 80))
            .expect("同窗口父几何范围仍在");
        assert_eq!(retained, 2);
        assert_eq!(
            step_control_level(&refreshed, retained, (80, 80), -1),
            Some(1)
        );
        assert_eq!(
            step_control_level(&refreshed, retained, (80, 80), -2),
            Some(0)
        );
    }

    #[test]
    fn control_preview_wheel_first_child_notch_is_consumed_after_new_point_refresh() {
        let old = old_path();
        let point = (80, 80);
        let (held, pending) =
            step_control_level_with_remainder(&old, 1, point, -1).expect("新点仍在父范围内");
        assert_eq!((held, pending), (1, -1));
        assert_eq!(
            refreshed_control_level(
                Some((11, old[held].rect)),
                11,
                &new_path(),
                point,
                true,
                pending
            ),
            Some(1)
        );
    }

    #[test]
    fn control_preview_wheel_multiple_child_notches_keep_only_unconsumed_steps() {
        let old = old_path();
        let point = (80, 80);
        // 最外范围先进入已知父范围；仅剩余两步等本次新点的子路径。
        let (held, pending) =
            step_control_level_with_remainder(&old, 2, point, -3).expect("路径范围有效");
        assert_eq!((held, pending), (1, -2));
        assert_eq!(
            refreshed_control_level(
                Some((11, old[held].rect)),
                11,
                &new_path(),
                point,
                true,
                pending
            ),
            Some(0)
        );
        assert_eq!(
            step_control_level_with_remainder(&old, 1, point, 1),
            Some((2, 0))
        );
    }

    #[test]
    fn control_preview_wheel_pending_child_and_opposite_notch_cancel_before_refresh() {
        let old = old_path();
        let point = (80, 80);
        let (held, pending) = step_control_level_with_remainder(&old, 1, point, -1).unwrap();
        let (held, pending) =
            step_control_level_with_remainder(&old, held, point, pending + 1).unwrap();
        assert_eq!((held, pending), (1, 0));
        assert_eq!(
            refreshed_control_level(
                Some((11, old[held].rect)),
                11,
                &new_path(),
                point,
                true,
                pending
            ),
            Some(2)
        );
    }

    #[test]
    fn control_preview_wheel_refresh_never_applies_child_intent_to_absent_parent() {
        let old = old_path();
        let fresh = new_path();
        assert_eq!(
            refreshed_control_level(Some((11, old[0].rect)), 11, &fresh, (80, 80), false, -1),
            Some(3)
        );
        assert_eq!(
            refreshed_control_level(Some((10, old[1].rect)), 11, &fresh, (80, 80), false, -1),
            Some(3)
        );
        assert_eq!(
            refreshed_control_level(Some((11, old[1].rect)), 11, &fresh, (250, 250), false, -1),
            Some(3)
        );
        assert_eq!(
            refreshed_control_level(None, 11, &[], (80, 80), true, -1),
            None
        );
    }

    #[test]
    fn control_preview_wheel_manual_window_navigation_keeps_parent_child_direction() {
        let fresh = new_path();
        assert_eq!(
            refreshed_control_level(None, 11, &fresh, (80, 80), false, -1),
            Some(2)
        );
        assert_eq!(
            refreshed_control_level(None, 11, &fresh, (80, 80), false, -2),
            Some(1)
        );
        assert_eq!(
            refreshed_control_level(None, 11, &fresh, (80, 80), true, 1),
            Some(1)
        );
        assert_eq!(
            refreshed_control_level(None, 11, &fresh, (80, 80), true, -1),
            Some(0)
        );
    }

    #[test]
    fn control_preview_wheel_fractional_notches_accumulate_and_invalid_input_is_ignored() {
        let mut remainder = 0.;
        assert_eq!(control_wheel_steps(&mut remainder, 0.5), Some(0));
        assert_eq!(remainder, 0.5);
        assert_eq!(control_wheel_steps(&mut remainder, 0.5), Some(1));
        assert_eq!(remainder, 0.);
        assert_eq!(control_wheel_steps(&mut remainder, -0.5), Some(0));
        assert_eq!(control_wheel_steps(&mut remainder, f32::NAN), None);
        assert_eq!(remainder, -0.5);
        assert_eq!(control_wheel_steps(&mut remainder, -0.5), Some(-1));
        assert_eq!(remainder, 0.);
        assert_eq!(control_wheel_steps(&mut remainder, f32::INFINITY), None);
        assert_eq!(control_wheel_steps(&mut remainder, 1024.), Some(128));
        assert_eq!(control_wheel_steps(&mut remainder, -1024.), Some(-128));
    }

    #[test]
    fn control_preview_wheel_drafts_and_pointer_gestures_block_geometry_navigation() {
        use crate::annotation::{Point, Tool};
        let mut editor = crate::editor::Editor::new(Rect::full(400, 300), None);
        assert!(control_preview_is_idle(&editor, None));
        assert!(!control_preview_is_idle(&editor, Some((80, 80))));
        editor.pointer_down((20, 20), 6.);
        assert!(editor.is_dragging());
        assert!(!control_preview_is_idle(&editor, None));
        editor.cancel_gesture();
        editor.tool = Tool::Polyline;
        editor.crop = Some(Rect::full(400, 300));
        editor.pointer_down((20, 20), 6.);
        assert!(!editor.is_dragging());
        assert!(editor.has_pending_drawing());
        assert!(!control_preview_is_idle(&editor, None));
        editor.cancel_gesture();
        editor.begin_text(None, Point::from_tuple((20, 20)));
        assert!(editor.text_draft.is_some());
        assert!(!control_preview_is_idle(&editor, None));
    }
}

#[cfg(test)]
mod control_manual_boundary_refine_tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn path(rects: &[Rect], complete: bool, terminal: bool) -> ControlPath {
        ControlPath {
            point: (-1360, -120),
            targets: rects
                .iter()
                .map(|&rect| ControlTarget {
                    rect,
                    title: String::new(),
                })
                .collect(),
            state: ControlPathState::ControlView,
            complete,
            terminal,
        }
    }

    fn coarse(complete: bool, terminal: bool) -> ControlPath {
        path(
            &[rect(0, 0, 200, 200), rect(0, 0, 400, 300)],
            complete,
            terminal,
        )
    }

    fn refined() -> ControlPath {
        path(
            &[
                rect(70, 70, 20, 20),
                rect(50, 50, 100, 100),
                rect(0, 0, 200, 200),
                rect(0, 0, 400, 300),
            ],
            true,
            true,
        )
    }

    #[test]
    fn partial_same_point_child_boundary_requests_refine_and_remaps_original_parent() {
        let now = Instant::now();
        let point = (80, 80);
        for state in [
            ControlPathState::ControlView,
            ControlPathState::RawView,
            ControlPathState::NativeChild,
        ] {
            for (complete, terminal) in [(false, false), (true, false)] {
                let mut old = coarse(complete, terminal);
                old.state = state;
                let navigation = control_wheel_navigation(&old, 0, point, -1, now, now).unwrap();
                assert_eq!(
                    navigation,
                    ControlWheelNavigation {
                        level: 0,
                        pending_steps: -1,
                        refresh: true
                    }
                );
                assert_eq!(old.targets[navigation.level].rect, rect(0, 0, 200, 200));
                // 自动吸附关闭也只走用户剩余的一步，不直接跳到新最深叶。
                assert_eq!(
                    refreshed_control_level(
                        Some((11, old.targets[navigation.level].rect)),
                        11,
                        &refined().targets,
                        point,
                        false,
                        navigation.pending_steps,
                    ),
                    Some(1)
                );
            }
        }
    }

    #[test]
    fn fresh_terminal_boundary_is_consumed_without_refine_or_pending_work() {
        let received = Instant::now();
        let old = refined();
        let point = (80, 80);
        for elapsed in [Duration::ZERO, PATH_LEASE - Duration::from_millis(1)] {
            for steps in [-1, -128] {
                let navigation =
                    control_wheel_navigation(&old, 0, point, steps, received, received + elapsed)
                        .unwrap();
                assert_eq!(
                    navigation,
                    ControlWheelNavigation {
                        level: 0,
                        pending_steps: 0,
                        refresh: false
                    }
                );
                assert_eq!(
                    refreshed_control_level(
                        Some((11, old.targets[0].rect)),
                        11,
                        &old.targets,
                        point,
                        false,
                        navigation.pending_steps,
                    ),
                    Some(0)
                );
            }
        }
        let parent = control_wheel_navigation(&old, 0, point, 1, received, received).unwrap();
        assert_eq!(
            parent,
            ControlWheelNavigation {
                level: 1,
                pending_steps: 0,
                refresh: false
            }
        );
    }

    #[test]
    fn expired_boundary_refines_only_after_explicit_child_steps() {
        let received = Instant::now();
        let old = refined();
        let point = (80, 80);
        for elapsed in [PATH_LEASE, PATH_LEASE + Duration::from_millis(1)] {
            let now = received + elapsed;
            let navigation = control_wheel_navigation(&old, 0, point, -1, received, now).unwrap();
            assert_eq!(
                navigation,
                ControlWheelNavigation {
                    level: 0,
                    pending_steps: -1,
                    refresh: true
                }
            );
            assert_eq!(
                refreshed_control_level(
                    Some((11, old.targets[0].rect)),
                    11,
                    &old.targets,
                    point,
                    false,
                    navigation.pending_steps,
                ),
                Some(0)
            );
            for (level, steps) in [(0, 0), (3, 1)] {
                assert_eq!(
                    control_wheel_navigation(&old, level, point, steps, received, now).unwrap(),
                    ControlWheelNavigation {
                        level,
                        pending_steps: 0,
                        refresh: false
                    }
                );
            }
        }
    }

    #[test]
    fn pending_child_refine_preserves_opposite_cancellation_and_rejects_absent_parent() {
        let now = Instant::now();
        let old = coarse(false, false);
        let fresh = refined();
        let point = (80, 80);
        let first = control_wheel_navigation(&old, 0, point, -2, now, now).unwrap();
        assert_eq!(first.pending_steps, -2);
        let one_left =
            control_wheel_navigation(&old, first.level, point, first.pending_steps + 1, now, now)
                .unwrap();
        assert_eq!(
            one_left,
            ControlWheelNavigation {
                level: 0,
                pending_steps: -1,
                refresh: true
            }
        );
        let cancelled = control_wheel_navigation(
            &old,
            one_left.level,
            point,
            one_left.pending_steps + 1,
            now,
            now,
        )
        .unwrap();
        assert_eq!(
            cancelled,
            ControlWheelNavigation {
                level: 0,
                pending_steps: 0,
                refresh: false
            }
        );
        let parent = old.targets[0].rect;
        assert_eq!(
            refreshed_control_level(
                Some((11, parent)),
                11,
                &fresh.targets,
                point,
                false,
                cancelled.pending_steps
            ),
            Some(2)
        );
        assert_eq!(
            refreshed_control_level(
                Some((11, parent)),
                11,
                &fresh.targets,
                point,
                false,
                one_left.pending_steps
            ),
            Some(1)
        );
        let missing_parent = path(
            &[
                rect(70, 70, 20, 20),
                rect(50, 50, 100, 100),
                rect(0, 0, 400, 300),
            ],
            true,
            true,
        );
        assert_eq!(
            refreshed_control_level(
                Some((11, parent)),
                11,
                &missing_parent.targets,
                point,
                false,
                one_left.pending_steps
            ),
            Some(2)
        );
        assert_eq!(
            refreshed_control_level(
                Some((11, parent)),
                12,
                &fresh.targets,
                point,
                false,
                one_left.pending_steps
            ),
            Some(3)
        );
        assert_eq!(
            refreshed_control_level(
                Some((11, parent)),
                11,
                &fresh.targets,
                (250, 250),
                false,
                one_left.pending_steps
            ),
            Some(3)
        );
        assert!(control_wheel_navigation(&old, 0, (250, 250), -1, now, now).is_none());
        assert!(!control_probe_is_current(
            true,
            true,
            true,
            Some(11),
            11,
            false
        ));
        assert!(!control_probe_is_current(
            true,
            true,
            true,
            Some(12),
            11,
            true
        ));
        assert!(!control_probe_is_current(
            true,
            false,
            true,
            Some(11),
            11,
            true
        ));
    }
}

#[cfg(test)]
mod control_known_range_retention_tests {
    use super::*;
    use crate::{geometry::ScreenRect, selection_assist::WindowTarget};

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn fixture() -> (ControlHover, ScreenRect, WindowTarget, Instant) {
        let canvas = ScreenRect {
            left: -1920,
            top: -240,
            width: 640,
            height: 480,
        };
        let window = WindowTarget {
            rect: rect(0, 0, 400, 300),
            global_bounds: windows::Win32::Foundation::RECT {
                left: canvas.left,
                top: canvas.top,
                right: canvas.left + 400,
                bottom: canvas.top + 300,
            },
            title: "窗口".into(),
            hwnd: 11,
            process_id: 22,
            thread_id: 33,
        };
        let received = Instant::now();
        let hover = ControlHover {
            path: ControlPath {
                point: (canvas.left + 20, canvas.top + 20),
                targets: [
                    rect(10, 10, 20, 20),
                    rect(0, 0, 50, 50),
                    rect(0, 0, 200, 200),
                    window.rect,
                ]
                .into_iter()
                .map(|rect| ControlTarget {
                    rect,
                    title: String::new(),
                })
                .collect(),
                state: ControlPathState::ControlView,
                terminal: true,
                complete: true,
            },
            level: 1,
            window_hwnd: window.hwnd,
            received,
            navigated: true,
            retained_for_query: false,
            owner: ControlHoverOwner {
                canvas,
                window: window.clone(),
                epoch: 7,
            },
        };
        (hover, canvas, window, received)
    }

    fn context(
        canvas: ScreenRect,
        window: &WindowTarget,
        now: Instant,
    ) -> ControlHoverPointContext<'_> {
        ControlHoverPointContext {
            canvas,
            window: Some(window),
            epoch: 7,
            point: (80, 80),
            auto_adsorb: true,
            eligible: true,
            now,
        }
    }

    #[test]
    fn automatic_preview_requires_leaf_but_keeps_explicit_parent_navigation() {
        let (mut hover, _, _, _) = fixture();
        assert!(control_hover_preview_ready(&hover, true));
        hover.navigated = false;
        assert!(!control_hover_preview_ready(&hover, true));
        hover.level = 0;
        assert!(control_hover_preview_ready(&hover, true));
        hover.retained_for_query = true;
        assert!(!control_hover_preview_ready(&hover, true));
        hover.retained_for_query = false;
        hover.path.complete = false;
        assert!(!control_hover_preview_ready(&hover, true));
        hover.path.complete = true;
        // 鼠标在容器空白处时完整路径的最深元素是容器本身，也立即显示。
        hover.path.terminal = false;
        assert!(control_hover_preview_ready(&hover, true));
        assert!(!control_hover_terminal_ready(&hover));
        hover.path.terminal = true;
        assert!(control_hover_terminal_ready(&hover));
        hover.path.state = ControlPathState::WindowOnly;
        // 当前点完整查询只有窗口时也要自动显示，但不能当作终端控件缓存复用。
        assert!(control_hover_preview_ready(&hover, true));
        assert!(!control_hover_terminal_ready(&hover));
        hover.path.complete = false;
        assert!(!control_hover_preview_ready(&hover, true));
        assert!(control_hover_preview_ready(&hover, false));
        hover.navigated = true;
        assert!(control_hover_preview_ready(&hover, true));
    }

    #[test]
    fn window_only_wheel_selection_survives_same_point_refresh() {
        let (mut hover, _, _, _) = fixture();
        hover.path.state = ControlPathState::WindowOnly;
        hover.path.complete = false;
        hover.path.terminal = false;
        // 滚轮占位已被用户选中：重试结果回来时不能退回成未导航的自动结果。
        assert!(control_refresh_keeps_navigation(Some(&hover), 0));
        hover.navigated = false;
        assert!(!control_refresh_keeps_navigation(Some(&hover), 0));
        assert!(control_refresh_keeps_navigation(Some(&hover), -1));
        assert!(!control_refresh_keeps_navigation(None, 0));
    }

    #[test]
    fn complete_window_only_result_displays_without_wheel_after_a_detail() {
        let (mut hover, canvas, window, now) = fixture();
        hover.level = 0;
        hover.navigated = false;
        let mut current = context(canvas, &window, now);
        current.point = (20, 20);
        let mut feedback = crate::control_feedback::ControlFeedback::default();
        let detail =
            control_feedback_candidate(hover.path.targets[0].clone(), Some(&hover), &current);
        feedback.update_automatic(Some(detail), current.point, Some(window.hwnd), now);

        hover.path.state = ControlPathState::WindowOnly;
        hover.path.terminal = false;
        hover.path.targets = vec![ControlTarget {
            rect: window.rect,
            title: window.title.clone(),
        }];
        assert!(control_hover_preview_ready(&hover, true));
        assert!(!control_hover_terminal_ready(&hover));
        let candidate =
            control_feedback_candidate(hover.path.targets[0].clone(), Some(&hover), &current);
        assert!(!candidate.refined && candidate.complete && candidate.leaf && !candidate.navigated);
        let (shown, active) = feedback.update_automatic(
            Some(candidate),
            current.point,
            Some(window.hwnd),
            now + Duration::from_millis(1),
        );
        assert_eq!(shown.unwrap().rect, window.rect);
        assert!(!active && feedback.wake_at().is_none());
    }

    #[test]
    fn window_first_preview_refines_automatically_but_wheel_keeps_window_anchor() {
        let (mut hover, _, window, _) = fixture();
        let refined = hover.path.targets.clone();
        hover.path.state = ControlPathState::WindowOnly;
        hover.path.complete = false;
        hover.path.terminal = false;
        hover.path.targets = vec![ControlTarget {
            rect: window.rect,
            title: window.title.clone(),
        }];
        hover.level = 0;
        let point = (20, 20);
        // 未导航的首屏窗口不固定层级，完整结果到达后直接采用最深元素。
        hover.navigated = false;
        assert_eq!(control_navigated_range(Some(&hover)), None);
        assert_eq!(
            refreshed_control_level(None, window.hwnd, &refined, point, true, 0),
            Some(0)
        );
        // 用户已滚轮操作：上滚保持最外层，下滚从窗口逐层向内。
        hover.navigated = true;
        let previous = control_navigated_range(Some(&hover));
        assert_eq!(previous, Some((window.hwnd, window.rect)));
        for auto_adsorb in [true, false] {
            for (steps, level) in [(0, 3), (1, 3), (-1, 2), (-2, 1)] {
                assert_eq!(
                    refreshed_control_level(
                        previous,
                        window.hwnd,
                        &refined,
                        point,
                        auto_adsorb,
                        steps,
                    ),
                    Some(level)
                );
            }
        }
        // 窗口顶层的上滚立即消耗，下滚仍可进入；反向滚轮可以取消未完成的下滚。
        for (notches, expected) in [([1, -1], -1), ([-2, 1], -1), ([-1, 1], 0)] {
            let mut pending = 0;
            for steps in notches {
                pending = control_wheel_navigation(
                    &hover.path,
                    0,
                    point,
                    pending + steps,
                    hover.received,
                    hover.received,
                )
                .unwrap()
                .pending_steps;
            }
            assert_eq!(pending, expected);
        }
    }

    #[test]
    fn known_path_retains_nearest_range_and_continues_wheel_at_new_point() {
        for state in [
            ControlPathState::ControlView,
            ControlPathState::RawView,
            ControlPathState::NativeChild,
        ] {
            let (mut hover, canvas, window, received) = fixture();
            hover.path.state = state;
            hover.path.complete = false;
            hover.path.terminal = false;
            let original_point = hover.path.point;
            let original_rects: Vec<_> = hover
                .path
                .targets
                .iter()
                .map(|target| target.rect)
                .collect();
            let mut hover = Some(hover);
            let mut pending = -2;
            let mut remainder = 0.5;
            let mut last_request = Some((20, 20));
            let context = context(canvas, &window, received + Duration::from_millis(20));
            assert_eq!(
                update_control_hover_point(
                    &mut hover,
                    &mut pending,
                    &mut remainder,
                    &mut last_request,
                    &context,
                ),
                ControlHoverPointDecision::RetainedRange(2)
            );
            let hover = hover.unwrap();
            assert_eq!(hover.path.targets[hover.level].rect, rect(0, 0, 200, 200));
            assert!(control_level_contains_point(
                &hover.path.targets,
                hover.level,
                context.point
            ));
            assert_eq!(hover.path.point, original_point);
            assert_eq!(
                hover
                    .path
                    .targets
                    .iter()
                    .map(|target| target.rect)
                    .collect::<Vec<_>>(),
                original_rects
            );
            assert_eq!(hover.received, received);
            assert_eq!(hover.owner.canvas, canvas);
            assert!(same_window(&hover.owner.window, &window));
            assert_eq!(hover.owner.epoch, context.epoch);
            assert!(!hover.navigated);
            assert_eq!((pending, remainder, last_request), (0, 0., None));
            assert!(control_probe_request_ready(
                Some(&hover),
                context.point,
                last_request,
                false,
                false,
                false,
                context.now
            ));
            // 上滚仍选择包含新点的窗口；下滚不把原点的 C/P 当作新点的子范围。
            assert_eq!(
                control_wheel_navigation(
                    &hover.path,
                    hover.level,
                    context.point,
                    1,
                    received,
                    context.now
                ),
                Some(ControlWheelNavigation {
                    level: 3,
                    pending_steps: 0,
                    refresh: true
                })
            );
            assert_eq!(
                control_wheel_navigation(
                    &hover.path,
                    hover.level,
                    context.point,
                    -1,
                    received,
                    context.now
                ),
                Some(ControlWheelNavigation {
                    level: 2,
                    pending_steps: -1,
                    refresh: true
                })
            );
        }

        let (mut hover, canvas, window, received) = fixture();
        // 合法 UIA 子矩形可以伸出父矩形；不能只扫描当前层之后的祖先。
        hover.path.targets[0].rect = rect(10, 10, 100, 100);
        let context = context(canvas, &window, received);
        assert_eq!(
            control_hover_point_decision(&hover, &context),
            ControlHoverPointDecision::RetainedRange(0)
        );
        assert!(!hover.path.targets[1].rect.contains(context.point));
        assert!(hover.path.targets[0].rect.contains(context.point));
        let mut hover = Some(hover);
        let mut pending = -2;
        let mut remainder = 0.5;
        let mut last_request = Some((20, 20));
        assert_eq!(
            update_control_hover_point(
                &mut hover,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &context,
            ),
            ControlHoverPointDecision::RetainedRange(0)
        );
        let hover = hover.unwrap();
        assert!(hover.path.complete && hover.path.terminal);
        assert!(hover.retained_for_query && !hover.navigated);
        assert!(!control_hover_is_fresh(&hover, context.point, context.now));
        assert!(control_probe_request_ready(
            Some(&hover),
            context.point,
            last_request,
            false,
            false,
            false,
            context.now,
        ));
    }

    #[test]
    fn retained_range_rejects_new_owner_mismatches_without_restricting_current_hold() {
        for case in 0..18 {
            let (mut hover, mut canvas, mut window, received) = fixture();
            let mut epoch = 7;
            let mut point = (80, 80);
            let mut auto_adsorb = true;
            let mut eligible = true;
            let mut now = received;
            match case {
                0 => auto_adsorb = false,
                1 => eligible = false,
                2 => hover.path.state = ControlPathState::WindowOnly,
                3 => canvas.left += 1,
                4 => epoch += 1,
                5 => window.hwnd += 1,
                6 => window.process_id += 1,
                7 => window.thread_id += 1,
                8 => window.global_bounds.left += 1,
                9 => window.rect.width -= 1,
                10 => now += PATH_LEASE,
                11 => hover.level = usize::MAX,
                12 => hover.path.targets[0].rect.width = 1000,
                13 => hover.path.targets.last_mut().unwrap().rect.width -= 1,
                14 => point = (450, 350),
                15 => {}
                16 => hover.path.targets.clear(),
                17 => canvas.width = u32::MAX,
                _ => unreachable!(),
            }
            let context = ControlHoverPointContext {
                canvas,
                window: (case != 15).then_some(&window),
                epoch,
                point,
                auto_adsorb,
                eligible,
                now,
            };
            let mut hover = Some(hover);
            let mut pending = -2;
            let mut remainder = 0.5;
            let mut last_request = Some((20, 20));
            assert_eq!(
                update_control_hover_point(
                    &mut hover,
                    &mut pending,
                    &mut remainder,
                    &mut last_request,
                    &context,
                ),
                ControlHoverPointDecision::Clear,
                "new retention gate {case}"
            );
            assert!(hover.is_none());
            assert_eq!((pending, remainder, last_request), (0, 0., Some((20, 20))));
        }

        let (mut hover, canvas, window, received) = fixture();
        let near_expiry = context(
            canvas,
            &window,
            received + PATH_LEASE - Duration::from_millis(1),
        );
        assert_eq!(
            control_hover_point_decision(&hover, &near_expiry),
            ControlHoverPointDecision::RetainedRange(2)
        );
        hover.owner.epoch += 1;
        hover.owner.canvas.left += 1;
        hover.owner.window.process_id += 1;
        let mut context = context(canvas, &window, received + PATH_LEASE);
        context.point = (20, 20);
        context.auto_adsorb = false;
        context.eligible = false;
        let mut hover = Some(hover);
        let mut pending = -2;
        let mut remainder = 0.5;
        let mut last_request = Some(context.point);
        assert_eq!(
            update_control_hover_point(
                &mut hover,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &context,
            ),
            ControlHoverPointDecision::Keep
        );
        assert!(hover.as_ref().unwrap().navigated);
        assert_eq!(hover.as_ref().unwrap().level, 1);
        assert_eq!(
            (pending, remainder, last_request),
            (-2, 0.5, Some(context.point))
        );
    }

    #[test]
    fn retained_ancestor_returning_to_leaf_reprobes_but_manual_parent_keeps_hold() {
        let (hover, canvas, window, received) = fixture();
        let mut hover = Some(hover);
        let mut pending = -2;
        let mut remainder = 0.5;
        let mut last_request = Some((20, 20));
        let mut context = context(canvas, &window, received);
        assert_eq!(
            update_control_hover_point(
                &mut hover,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &context,
            ),
            ControlHoverPointDecision::RetainedRange(2)
        );
        context.point = (20, 20);
        last_request = Some((80, 80));
        assert_eq!(
            update_control_hover_point(
                &mut hover,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &context,
            ),
            ControlHoverPointDecision::Keep
        );
        let hover = hover.unwrap();
        assert_eq!(hover.level, 2);
        assert!(!hover.navigated);
        assert!(!control_hover_is_fresh(&hover, context.point, context.now));
        assert!(control_probe_request_ready(
            Some(&hover),
            context.point,
            last_request,
            false,
            false,
            false,
            context.now,
        ));
        assert_eq!(
            refreshed_control_level(
                None,
                window.hwnd,
                &hover.path.targets,
                context.point,
                true,
                pending
            ),
            Some(0)
        );

        let (manual, _, _, _) = fixture();
        assert!(manual.navigated);
        assert!(control_hover_is_fresh(
            &manual,
            context.point,
            manual.received
        ));
        assert!(!control_probe_request_ready(
            Some(&manual),
            context.point,
            Some(context.point),
            false,
            false,
            false,
            manual.received,
        ));
        assert!(control_probe_request_ready(
            Some(&manual),
            context.point,
            Some(context.point),
            true,
            false,
            false,
            manual.received,
        ));
        assert_eq!(
            refreshed_control_level(
                Some((manual.window_hwnd, manual.path.targets[manual.level].rect)),
                window.hwnd,
                &manual.path.targets,
                context.point,
                true,
                0,
            ),
            Some(1)
        );

        let (mut ordinary, _, _, _) = fixture();
        ordinary.level = 0;
        ordinary.navigated = false;
        assert!(!ordinary.retained_for_query);
        assert!(control_hover_is_fresh(
            &ordinary,
            context.point,
            ordinary.received
        ));
        assert!(!control_probe_request_ready(
            Some(&ordinary),
            context.point,
            None,
            false,
            false,
            false,
            ordinary.received,
        ));
        assert!(control_retained_query_is_current(
            Some(&ordinary),
            (80, 80),
            (20, 20),
        ));
    }

    #[test]
    fn retained_range_inflight_old_result_is_rejected_then_latest_point_can_probe() {
        let (hover, canvas, window, received) = fixture();
        let old_result = hover.path.clone();
        let mut hover = Some(hover);
        let mut pending = -2;
        let mut remainder = 0.5;
        let mut last_request = Some((20, 20));
        let context = context(canvas, &window, received);
        assert_eq!(
            update_control_hover_point(
                &mut hover,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &context,
            ),
            ControlHoverPointDecision::RetainedRange(2)
        );
        let hover = hover.unwrap();
        assert_eq!(last_request, None);
        // 同时仍只允许一个 provider 任务；旧点结果不能替换当前 Q。
        assert!(!control_probe_request_ready(
            Some(&hover),
            context.point,
            last_request,
            false,
            false,
            true,
            context.now,
        ));
        assert!(!control_probe_is_current(
            true,
            path_contains_current_point(&old_result, context.point, (20, 20)),
            true,
            Some(window.hwnd),
            window.hwnd,
            true,
        ));
        assert!(!control_retained_query_is_current(
            Some(&hover),
            context.point,
            (20, 20),
        ));
        // 旧任务自然完成后，pending 释放；被清除的 last_request 允许 latest-point 接续。
        assert!(control_probe_request_ready(
            Some(&hover),
            context.point,
            last_request,
            false,
            false,
            false,
            context.now,
        ));
        let mut fresh = old_result;
        fresh.point = (canvas.left + context.point.0, canvas.top + context.point.1);
        fresh.targets = [rect(70, 70, 20, 20), rect(0, 0, 200, 200), window.rect]
            .into_iter()
            .map(|rect| ControlTarget {
                rect,
                title: String::new(),
            })
            .collect();
        assert!(control_probe_is_current(
            true,
            path_contains_current_point(&fresh, context.point, context.point),
            true,
            Some(window.hwnd),
            window.hwnd,
            true,
        ));
        assert!(control_retained_query_is_current(
            Some(&hover),
            context.point,
            context.point,
        ));
        assert_eq!(
            refreshed_control_level(
                None,
                window.hwnd,
                &fresh.targets,
                context.point,
                true,
                pending
            ),
            Some(0)
        );
        for gate in 0..5 {
            assert!(!control_probe_is_current(
                gate != 0,
                gate != 1,
                gate != 2,
                Some(if gate == 3 {
                    window.hwnd + 1
                } else {
                    window.hwnd
                }),
                window.hwnd,
                gate != 4,
            ));
        }

        let (mut leaf, _, _, _) = fixture();
        leaf.path.targets[0].rect = rect(10, 10, 100, 100);
        let old_leaf_result = leaf.path.clone();
        let mut leaf = Some(leaf);
        assert_eq!(
            update_control_hover_point(
                &mut leaf,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &context,
            ),
            ControlHoverPointDecision::RetainedRange(0)
        );
        // 旧 C 同样包含新点也必须拒绝：只在最新点查询返回后才恢复普通缓存资格。
        assert!(path_contains_current_point(
            &old_leaf_result,
            context.point,
            (20, 20)
        ));
        assert!(!control_retained_query_is_current(
            leaf.as_ref(),
            context.point,
            (20, 20),
        ));
        assert!(control_probe_request_ready(
            leaf.as_ref(),
            context.point,
            last_request,
            false,
            false,
            false,
            context.now,
        ));
    }

    #[test]
    fn retained_feedback_uses_host_owner_without_pinning_navigation_or_query() {
        for state in [
            ControlPathState::ControlView,
            ControlPathState::RawView,
            ControlPathState::NativeChild,
        ] {
            let (mut hover, canvas, window, received) = fixture();
            hover.path.state = state;
            hover.path.complete = false;
            hover.path.terminal = false;
            let mut initial = context(canvas, &window, received);
            initial.point = (20, 20);
            let old = control_feedback_candidate(
                hover.path.targets[hover.level].clone(),
                Some(&hover),
                &initial,
            );
            assert!(old.navigated && !old.retained_for_query);
            let mut feedback = crate::control_feedback::ControlFeedback::default();
            feedback.update(
                Some(old.clone()),
                initial.point,
                Some(window.hwnd),
                received,
            );

            let mut hover = Some(hover);
            let mut pending = -2;
            let mut remainder = 0.5;
            let mut last_request = Some(initial.point);
            let moved = context(canvas, &window, received + Duration::from_millis(20));
            assert_eq!(
                update_control_hover_point(
                    &mut hover,
                    &mut pending,
                    &mut remainder,
                    &mut last_request,
                    &moved,
                ),
                ControlHoverPointDecision::RetainedRange(2)
            );
            let hover = hover.as_ref().unwrap();
            assert!(!hover.navigated && hover.retained_for_query);
            assert_eq!((pending, remainder, last_request), (0, 0., None));
            assert!(!control_hover_is_fresh(hover, moved.point, moved.now));
            assert!(control_probe_request_ready(
                Some(hover),
                moved.point,
                last_request,
                false,
                false,
                false,
                moved.now,
            ));
            assert!(!control_retained_query_is_current(
                Some(hover),
                moved.point,
                initial.point,
            ));
            let retained = control_feedback_candidate(
                hover.path.targets[hover.level].clone(),
                Some(hover),
                &moved,
            );
            assert!(retained.retained_for_query && !retained.navigated);
            assert!(!old.target.rect.contains(moved.point));
            feedback.update(
                Some(retained.clone()),
                moved.point,
                Some(window.hwnd),
                moved.now,
            );
            for millis in [16, 32] {
                let mut moving =
                    context(canvas, &window, moved.now + Duration::from_millis(millis));
                moving.point.0 += millis as i32;
                let candidate = control_feedback_candidate(
                    hover.path.targets[hover.level].clone(),
                    Some(hover),
                    &moving,
                );
                assert!(candidate.retained_for_query && !candidate.navigated);
                let (shown, active) =
                    feedback.update(Some(candidate), moving.point, Some(window.hwnd), moving.now);
                let shown = shown.unwrap().rect;
                if millis == 16 {
                    assert!(
                        shown.width > old.target.rect.width
                            && shown.width < retained.target.rect.width
                    );
                    assert!(active);
                } else {
                    assert_eq!(shown, retained.target.rect);
                    assert!(!active);
                }
                assert!(!feedback.settled(moving.now));
            }
        }
    }

    #[test]
    fn retained_multi_monitor_parent_keeps_immediate_feedback_and_manual_navigation_range() {
        let (mut hover, canvas, mut window, received) = fixture();
        let monitors = [rect(0, 0, 320, 480), rect(320, 0, 320, 480)];
        window.rect = Rect::full(canvas.width, canvas.height);
        window.global_bounds.right = canvas.left + canvas.width as i32;
        window.global_bounds.bottom = canvas.top + canvas.height as i32;
        hover.owner.window = window.clone();
        hover.path.targets = vec![
            ControlTarget {
                rect: rect(10, 10, 20, 20),
                title: "图标".into(),
            },
            ControlTarget {
                rect: window.rect,
                title: "桌面".into(),
            },
        ];
        hover.level = 0;
        let original_path = hover.path.targets.clone();
        let mut initial = context(canvas, &window, received);
        initial.point = (20, 20);
        let old = control_display_feedback_candidate(
            hover.path.targets[hover.level].clone(),
            Some(&hover),
            &initial,
            monitors,
        );
        let old_rect = old.target.rect;
        let mut feedback = crate::control_feedback::ControlFeedback::default();
        feedback.update(Some(old), initial.point, Some(window.hwnd), received);

        let mut hover = Some(hover);
        let mut pending = 0;
        let mut remainder = 0.;
        let mut last_request = Some(initial.point);
        let moved = context(canvas, &window, received + Duration::from_millis(20));
        assert_eq!(
            update_control_hover_point(
                &mut hover,
                &mut pending,
                &mut remainder,
                &mut last_request,
                &moved,
            ),
            ControlHoverPointDecision::RetainedRange(1)
        );
        let hover = hover.as_mut().unwrap();
        let retained = control_display_feedback_candidate(
            hover.path.targets[hover.level].clone(),
            Some(hover),
            &moved,
            monitors,
        );
        assert_eq!(retained.target.rect, monitors[0]);
        assert!(retained.retained_for_query && !retained.navigated);
        feedback.update(Some(retained), moved.point, Some(window.hwnd), moved.now);
        for millis in [16, 32] {
            let mut moving = context(canvas, &window, moved.now + Duration::from_millis(millis));
            moving.point.0 += millis as i32;
            let candidate = control_display_feedback_candidate(
                hover.path.targets[hover.level].clone(),
                Some(hover),
                &moving,
                monitors,
            );
            assert!(candidate.retained_for_query && !candidate.navigated);
            let (shown, active) =
                feedback.update(Some(candidate), moving.point, Some(window.hwnd), moving.now);
            let shown = shown.unwrap().rect;
            if millis == 16 {
                assert!(shown.width > old_rect.width && shown.width < monitors[0].width);
                assert!(active);
            } else {
                assert_eq!(shown, monitors[0]);
                assert!(!active);
            }
            assert!(!feedback.settled(moving.now));
        }
        hover.navigated = true;
        hover.retained_for_query = false;
        let navigated = control_display_feedback_candidate(
            hover.path.targets[hover.level].clone(),
            Some(hover),
            &moved,
            monitors,
        );
        assert_eq!(navigated.target.rect, window.rect);
        assert!(navigated.navigated && !navigated.retained_for_query);
        assert_eq!(hover.path.targets, original_path);
    }

    #[test]
    fn retained_feedback_rejects_ineligible_owner_and_missing_range() {
        for case in 0..21 {
            let (mut hover, mut canvas, mut window, received) = fixture();
            hover.level = 2;
            hover.navigated = false;
            hover.retained_for_query = true;
            let mut target = hover.path.targets[hover.level].clone();
            let mut epoch = 7;
            let mut auto_adsorb = true;
            let mut eligible = true;
            let mut point = (80, 80);
            let mut now = received + Duration::from_millis(20);
            match case {
                0 => auto_adsorb = false,
                1 => eligible = false,
                2 => hover.path.state = ControlPathState::WindowOnly,
                3 => canvas.left += 1,
                4 => epoch += 1,
                5 => window.hwnd += 1,
                6 => window.process_id += 1,
                7 => window.thread_id += 1,
                8 => window.global_bounds.left += 1,
                9 => window.rect.width -= 1,
                10 => now = received + PATH_LEASE,
                11 => hover.level = usize::MAX,
                12 => hover.path.targets[0].rect.width = 1000,
                13 => hover.path.targets.last_mut().unwrap().rect.width -= 1,
                14 => point = (250, 250),
                15 => {}
                16 => hover.path.targets.clear(),
                17 => canvas.width = u32::MAX,
                18 => hover.retained_for_query = false,
                19 => {}
                20 => target.rect.width -= 1,
                _ => unreachable!(),
            }
            let context = ControlHoverPointContext {
                canvas,
                window: (case != 15).then_some(&window),
                epoch,
                point,
                auto_adsorb,
                eligible,
                now,
            };
            let candidate =
                control_feedback_candidate(target, (case != 19).then_some(&hover), &context);
            assert!(!candidate.retained_for_query, "feedback owner gate {case}");
            assert!(!candidate.navigated);
        }
        let (mut hover, canvas, window, received) = fixture();
        hover.retained_for_query = true;
        let target = hover.path.targets[hover.level].clone();
        let mut context = context(canvas, &window, received + PATH_LEASE);
        context.point = (20, 20);
        context.auto_adsorb = false;
        context.eligible = false;
        assert_eq!(
            control_hover_point_decision(&hover, &context),
            ControlHoverPointDecision::Keep
        );
        let candidate = control_feedback_candidate(target, Some(&hover), &context);
        assert!(candidate.navigated);
        assert!(!candidate.retained_for_query);
    }
}

#[cfg(test)]
mod control_text_profile_tests {
    use super::{control_text_with_ocr_fallback, ControlTextOutcome};
    use crate::{capture_view::ocr_task::Receipt, ppocr::OcrProfile};
    use std::cell::Cell;

    #[test]
    fn empty_uia_handoff_keeps_submitted_profile_when_settings_change() {
        let setting = Cell::new(OcrProfile::Small);
        let requested_profile = setting.get();
        let owner = crate::recognition::task::CancelOwner::new();
        let receipt = Receipt::from_result(control_text_with_ocr_fallback(
            &owner.token(),
            || {
                setting.set(OcrProfile::Tiny);
                Ok(None)
            },
            || Ok(ControlTextOutcome::StandardOcr(requested_profile)),
        ));
        assert_eq!(setting.get(), OcrProfile::Tiny);
        assert!(matches!(
            receipt,
            Receipt::Completed(ControlTextOutcome::StandardOcr(OcrProfile::Small))
        ));
    }
}
