//! 长截图操作条：后台采集，界面只消费条带元数据和有界预览。
use crate::recording_key_owner as key_owner;
#[path = "long_capture_shortcuts.rs"]
mod shortcut_keys;

use crate::{
    geometry::ScreenRect,
    gpui_capture::{hwnd, CaptureLock},
    long_auto::{AutoDirection, AutoScrollOptions},
    long_auto_resume::{AutoResume, ResumeContext},
    long_capture_border::LongCaptureBorder,
    long_capture_preview::LongCapturePreview,
    long_capture_runtime::{
        AutoResumeTarget, FrameGate, FrameGateFactory, LongOptions, LongPhase, LongSession,
        LongSnapshot, MatchHook, RegionChange,
    },
    long_output::{self, LongOutcome, LongOutput},
    long_region::{RegionGesture, RegionHandle},
    long_region_input::RegionInputEvent,
    pin_model::Placement,
    stitch::{ScrollAxis, StitchSnapshot},
    text_field::TextField,
    toolbar_icons::{self, Icon},
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, rgba, AnyElement, App, AppContext, Context, Entity, FocusHandle,
    MouseButton, Render, SharedString, Window, WindowBounds, WindowHandle, WindowOptions,
};
use key_owner::KeyOwnerProbe;
use shortcut_keys::{
    Action as ShortcutAction, Context as ShortcutContext, Modifiers as ShortcutModifiers,
    Route as ShortcutRoute,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::{ClientToScreen, MonitorFromPoint, MONITOR_DEFAULTTONEAREST},
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::*,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FixedMode {
    None,
    Auto,
    Manual,
}
#[derive(Clone, Copy)]
enum Control {
    Pause,
    Undo,
    Restart,
    Axis(ScrollAxis),
    Fixed(FixedMode),
    Auto,
    AutoDirection(AutoDirection),
    Settings,
    Output(LongOutput),
    Close,
}
struct PendingEditor {
    action: LongOutput,
    resume: bool,
    generation: u64,
    allow_failed_result: bool,
}
struct RegionDrag {
    gesture: RegionGesture,
    draft: ScreenRect,
    resume: bool,
    /// 采集中沿滚动方向拖动：框随手移动、采集不停，拼接把移动当作滚动。
    live: bool,
}
#[derive(Clone, Copy)]
enum RegionStage {
    Pause(u64),
    Candidate(u64),
    Restore(u64),
}
struct PendingRegion {
    region: ScreenRect,
    change: RegionChange,
    resume: bool,
    stage: RegionStage,
}
struct ToolbarDrag {
    id: u64,
    start: (i32, i32),
    placement: Placement,
}

const PANEL_WIDTH: f32 = 374.;
const PANEL_PADDING: f32 = 6.;
const BUTTON_SIZE: f32 = 30.;
const PRIMARY_GAP: f32 = 2.;

pub struct LongCaptureView {
    pub(crate) region: ScreenRect,
    pub(crate) session: Option<LongSession>,
    pub(crate) capture_lock: Option<CaptureLock>,
    pub(crate) ui_positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
    /// 输出交接中：成品正在生成，或已交给编辑器、保存面板等出口。
    pub(crate) editor_open: bool,
    pub(crate) status: String,
    options: LongOptions,
    draft_axis: ScrollAxis,
    fixed_mode: FixedMode,
    fixed_start: Entity<TextField>,
    fixed_end: Entity<TextField>,
    focus: FocusHandle,
    key_owner: Option<KeyOwnerProbe>,
    key_owner_error: Option<String>,
    meta: LongSnapshot,
    preview_key: Option<(u64, u64)>,
    preview_toolbar: Option<Placement>,
    pub(crate) border: Option<LongCaptureBorder>,
    pub(crate) preview_window: Option<WindowHandle<LongCapturePreview>>,
    status_until: Option<Instant>,
    growth_note: Option<(Instant, String)>,
    setup_ready: bool,
    initializing: bool,
    closing: bool,
    pending_editor: Option<PendingEditor>,
    settings_open: bool,
    auto_options: AutoScrollOptions,
    auto_pending: bool,
    auto_pending_generation: Option<u64>,
    auto_request: u64,
    auto_resume: AutoResume<AutoResumeTarget>,
    popup_capture_resume: Option<ResumeContext<AutoResumeTarget>>,
    popup_manual_generation: Option<u64>,
    frame_gate: Option<FrameGate>,
    frame_gate_factory: Option<FrameGateFactory>,
    match_hook: Option<MatchHook>,
    region_drag: Option<RegionDrag>,
    pending_region: Option<PendingRegion>,
    toolbar_drag: Option<ToolbarDrag>,
    parameters_enabled: bool,
    /// 每显示帧执行一次的轻量循环：拖动输入、选区跟随与预览交接不等 50ms 轮询。
    frame_tick_epoch: u64,
    last_frame_tick: Option<Instant>,
    /// 预览与操作条当前跟随的选区（拖动中为草稿选区）。
    shown_region: ScreenRect,
}
impl LongCaptureView {
    pub fn open(region: ScreenRect, cx: &mut App) -> Result<WindowHandle<Self>> {
        Self::open_with_options(region, LongOptions::default(), cx)
    }
    pub(crate) fn open_with_options(
        region: ScreenRect,
        options: LongOptions,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_inner(region, options, None, None, None, cx)
    }
    /// 自有窗口专项走真实操作条和编辑入口，采集始终复核自有 ROI。
    pub(crate) fn open_guarded(
        region: ScreenRect,
        options: LongOptions,
        gate: FrameGate,
        match_hook: Option<MatchHook>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        gate()?;
        Self::open_inner(region, options, Some(gate), None, match_hook, cx)
    }
    pub(crate) fn open_region_guarded(
        region: ScreenRect,
        options: LongOptions,
        factory: FrameGateFactory,
        match_hook: Option<MatchHook>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let gate = factory(region)?;
        gate()?;
        Self::open_inner(region, options, None, Some(factory), match_hook, cx)
    }
    fn open_inner(
        region: ScreenRect,
        options: LongOptions,
        gate: Option<FrameGate>,
        gate_factory: Option<FrameGateFactory>,
        match_hook: Option<MatchHook>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        options.validate(region)?;
        let placement = panel_placement(region, false);
        let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                focus: false,
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(brand_title!("连续长截图", "Long capture"));
                let view = cx.new(|cx| Self::new(region, options, window, cx));
                view.update(cx, |v, _| {
                    v.frame_gate = gate;
                    v.frame_gate_factory = gate_factory;
                    v.match_hook = match_hook;
                });
                window.focus(&view.read(cx).focus);
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    weak.update(cx, |v, cx| {
                        v.request_close(cx);
                        // 请求停止不等于已释放 GPU；实际关闭由存活期间的 poll 完成。
                        v.session.as_mut().is_none_or(LongSession::try_join_stopped)
                    })
                    .unwrap_or(true)
                });
                view.update(cx, |v, cx| {
                    v.initialize_window(placement, window, cx);
                    cx.spawn_in(window, async move |view, cx| loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                        if view.update_in(cx, |v, w, cx| v.poll(w, cx)).is_err() {
                            break;
                        }
                    })
                    .detach();
                });
                view
            },
        )?)
    }
    fn new(
        region: ScreenRect,
        options: LongOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 先于逻辑聚焦安装；回调只观察输入信号，不借用此视图。
        let (key_owner, key_owner_error) = match KeyOwnerProbe::install(hwnd(window)) {
            Ok(owner) => (Some(owner), None),
            Err(error) => (
                None,
                Some(trf!(
                    "快捷键不可用，请使用按钮：{error}",
                    "Shortcuts unavailable; use the buttons: {error}"
                )),
            ),
        };
        let fixed_mode = if options.stitch.detect_fixed_borders {
            FixedMode::Auto
        } else if options.stitch.fixed_start.unwrap_or(0) > 0
            || options.stitch.fixed_end.unwrap_or(0) > 0
        {
            FixedMode::Manual
        } else {
            FixedMode::None
        };
        let fixed_start = cx
            .new(|cx| TextField::new(options.stitch.fixed_start.unwrap_or(0).to_string(), "0", cx));
        let fixed_end =
            cx.new(|cx| TextField::new(options.stitch.fixed_end.unwrap_or(0).to_string(), "0", cx));
        let fields = vec![fixed_start.downgrade(), fixed_end.downgrade()];
        for field in [&fixed_start, &fixed_end] {
            field.update(cx, |v, cx| v.configure_focus(fields.clone(), window, cx));
        }
        Self {
            region,
            meta: LongSnapshot::starting(region, &options),
            draft_axis: options.stitch.axis,
            options,
            fixed_mode,
            fixed_start,
            fixed_end,
            focus: cx.focus_handle(),
            key_owner,
            key_owner_error,
            session: None,
            capture_lock: None,
            ui_positions: Rc::new(RefCell::new(HashMap::new())),
            preview_key: None,
            preview_toolbar: None,
            border: None,
            preview_window: None,
            status_until: None,
            growth_note: None,
            setup_ready: false,
            initializing: true,
            closing: false,
            editor_open: false,
            pending_editor: None,
            settings_open: false,
            auto_options: AutoScrollOptions::default(),
            auto_pending: false,
            auto_pending_generation: None,
            auto_request: 0,
            auto_resume: AutoResume::default(),
            popup_capture_resume: None,
            popup_manual_generation: None,
            frame_gate: None,
            frame_gate_factory: None,
            match_hook: None,
            region_drag: None,
            pending_region: None,
            toolbar_drag: None,
            parameters_enabled: true,
            frame_tick_epoch: 0,
            last_frame_tick: None,
            shown_region: region,
            status: tr!("正在准备，请稍候…", "Preparing, please wait…").into(),
        }
    }
    pub(crate) fn snapshot(&self) -> LongSnapshot {
        self.session
            .as_ref()
            .map_or_else(|| self.meta.clone(), LongSession::snapshot)
    }
    pub(crate) fn displayed_snapshot(&self) -> LongSnapshot {
        self.meta.clone()
    }
    pub(crate) fn auto_popup_probe(&self) -> serde_json::Value {
        serde_json::json!({"settings_open":self.settings_open,
            "resume_requested":self.popup_capture_resume.is_some(),
            "auto_resume_requested":self.auto_resume.is_pending(),"auto_pending":self.auto_pending,
            "auto_pending_generation":self.auto_pending_generation,
            "manual_pending_generation":self.popup_manual_generation,
            "expected_generation":self.auto_resume.expected_generation()})
    }
    pub(crate) fn stitch_snapshot(&self) -> Option<StitchSnapshot> {
        self.session.as_ref()?.stitch_snapshot()
    }
    pub(crate) fn region_adjusting(&self) -> bool {
        self.region_drag.is_some() || self.pending_region.is_some()
    }
    fn composing(&self, cx: &App) -> bool {
        self.fixed_start.read(cx).is_composing()
            || self.fixed_end.read(cx).is_composing()
            || self
                .key_owner
                .as_ref()
                .is_some_and(KeyOwnerProbe::composing)
    }
    fn field_focused(&self, window: &Window, cx: &App) -> bool {
        self.fixed_start.read(cx).focus_handle.is_focused(window)
            || self.fixed_end.read(cx).focus_handle.is_focused(window)
    }
    fn panel_action_ready(&self) -> bool {
        self.setup_ready
            && !self.initializing
            && !self.closing
            && !self.editor_open
            && self.pending_editor.is_none()
            && !self.region_adjusting()
            && self.toolbar_drag.is_none()
    }
    fn output_ready(&self, meta: &LongSnapshot) -> bool {
        self.panel_action_ready()
            && self.session.is_some()
            && meta.result_available
            && meta.generation == meta.requested_generation
            && matches!(
                meta.phase,
                LongPhase::Running | LongPhase::Paused | LongPhase::Failed | LongPhase::Stopped
            )
            // Running 的逐帧忙碌由现有 Drain 排空；其它阶段不能越过暂停/恢复交接。
            && (!meta.busy || meta.phase == LongPhase::Running)
    }
    fn pause_ready(&self, meta: &LongSnapshot) -> bool {
        self.panel_action_ready()
            && self.session.is_some()
            && meta.worker_alive
            && meta.generation == meta.requested_generation
            && matches!(
                meta.phase,
                LongPhase::Running | LongPhase::Paused | LongPhase::Failed
            )
            && (!meta.busy || meta.phase == LongPhase::Running)
    }
    fn key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 字段/IME 的事件资格保留到本轮结束，排队的原生手势不能把它改成宿主键。
        let field_focused = self.field_focused(window, cx);
        let composing = self.composing(cx);
        // Down / Up 与终止键可能同轮到达，先落实队列再检查调整/输出屏障。
        self.process_region_input(window, cx);
        let meta = self.snapshot();
        let context = ShortcutContext {
            owns_keyboard: window.is_window_active()
                && self.focus.is_focused(window)
                && self
                    .key_owner
                    .as_ref()
                    .is_some_and(KeyOwnerProbe::owns_keyboard),
            ime_composing: composing
                || self.composing(cx)
                || self.key_owner.as_ref().is_none_or(KeyOwnerProbe::composing),
            field_focused: field_focused || self.field_focused(window, cx),
            closing: self.closing,
            editor_open: self.editor_open,
            pending_editor: self.pending_editor.is_some(),
            toolbar_dragging: self.toolbar_drag.is_some(),
            region_adjusting: self.region_adjusting(),
            output_ready: self.output_ready(&meta),
            pause_ready: self.pause_ready(&meta),
        };
        let modifiers = event.keystroke.modifiers;
        let route = shortcut_keys::route(
            event.keystroke.key.as_str(),
            ShortcutModifiers {
                control: modifiers.control,
                alt: modifiers.alt,
                shift: modifiers.shift,
                platform: modifiers.platform,
                function: modifiers.function,
            },
            event.is_held,
            context,
        );
        let action = match route {
            ShortcutRoute::Pass => return,
            ShortcutRoute::Consume => {
                cx.stop_propagation();
                return;
            }
            ShortcutRoute::Action(action) => action,
        };
        cx.stop_propagation();
        match action {
            ShortcutAction::CancelToolbar => {
                if let Some(border) = &self.border {
                    border.cancel_input();
                }
                // 消费既有 Cancel 事件，还原拖动前位置；不另造工具栏事务。
                self.process_region_input(window, cx);
                cx.notify();
            }
            ShortcutAction::CancelRegion => {
                self.cancel_region_adjustment(cx);
                cx.notify();
            }
            ShortcutAction::Copy => self.control(Control::Output(LongOutput::Copy), window, cx),
            ShortcutAction::Save => self.control(Control::Output(LongOutput::Save), window, cx),
            ShortcutAction::Finish => self.control(Control::Output(LongOutput::Copy), window, cx),
            ShortcutAction::Pause => self.control(Control::Pause, window, cx),
            ShortcutAction::Close => self.control(Control::Close, window, cx),
        }
    }
    fn initialize_window(&self, placement: Placement, window: &mut Window, cx: &mut Context<Self>) {
        let native = hwnd(window).0 as usize;
        let region = self.region;
        let own = window
            .window_handle()
            .downcast::<Self>()
            .expect("长截图面板类型");
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let valid = own
                    .update(&mut async_cx, |_, w, _| hwnd(w).0 as usize == native)
                    .unwrap_or(false);
                let result = if valid {
                    native_setup(HWND(native as *mut _), placement)
                        .and_then(|()| LongCaptureBorder::new(region).map_err(|e| e.to_string()))
                } else {
                    Err("长截图面板已关闭".into())
                };
                let _ = own.update(&mut async_cx, |v, _, cx| {
                    if v.closing {
                        return;
                    }
                    match result {
                        Ok(mut border) => {
                            if let Err(error) = border.bind_panel(HWND(native as *mut _)) {
                                v.initializing = false;
                                v.meta.phase = LongPhase::Failed;
                                v.status = error.to_string();
                                return;
                            }
                            v.border = Some(border);
                            match LongCapturePreview::open(
                                region,
                                v.options.stitch.axis,
                                placement,
                                cx,
                            ) {
                                Ok(preview) => {
                                    v.preview_window = Some(preview);
                                    v.setup_ready = true;
                                }
                                Err(error) => {
                                    v.border.take();
                                    v.initializing = false;
                                    v.meta.phase = LongPhase::Failed;
                                    v.status = error.to_string();
                                }
                            }
                        }
                        Err(e) => {
                            v.initializing = false;
                            v.meta.busy = false;
                            v.meta.phase = LongPhase::Failed;
                            v.meta.worker_alive = false;
                            v.meta.worker_exited = true;
                            v.status = e;
                        }
                    }
                    cx.notify();
                });
                async_cx
                    .background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                for _ in 0..40 {
                    let waiting = own
                        .update(&mut async_cx, |v, w, cx| {
                            if !v.setup_ready || v.closing {
                                return false;
                            }
                            let Some(preview) = v.preview_window else {
                                return false;
                            };
                            let (ready, error) = preview
                                .read(cx)
                                .map(|v| (v.ready, v.error.clone()))
                                .unwrap_or_else(|e| (false, Some(e.to_string())));
                            if let Some(error) = error {
                                v.initializing = false;
                                v.meta.phase = LongPhase::Failed;
                                v.status = error;
                                return false;
                            }
                            if ready {
                                v.start(cx);
                                v.return_scroll_focus(w, cx);
                                return false;
                            }
                            true
                        })
                        .unwrap_or(false);
                    if !waiting {
                        return;
                    }
                    async_cx
                        .background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                }
                let _ = own.update(&mut async_cx, |v, _, cx| {
                    v.initializing = false;
                    v.meta.phase = LongPhase::Failed;
                    v.status = tr!(
                        "实时预览初始化超时，未开始采集；可重试",
                        "Live preview timed out before capturing started; you can retry"
                    )
                    .into();
                    cx.notify();
                });
            })
            .detach();
    }
    fn start(&mut self, cx: &mut Context<Self>) {
        self.initializing = false;
        if let Some(preview) = self.preview_window {
            let (ready, error) = preview
                .read(cx)
                .map(|v| (v.ready, v.error.clone()))
                .unwrap_or_else(|e| (false, Some(e.to_string())));
            if !ready || error.is_some() {
                self.meta.phase = LongPhase::Failed;
                self.meta.busy = false;
                self.status = error.unwrap_or_else(|| {
                    tr!("实时预览尚未准备完成", "Live preview isn't ready yet").into()
                });
                cx.notify();
                return;
            }
        }
        let session = if let Some(factory) = self.frame_gate_factory.clone() {
            LongSession::start_region_guarded(
                self.region,
                self.options.clone(),
                factory,
                self.match_hook.clone(),
            )
        } else {
            match self.frame_gate.clone() {
                Some(gate) => LongSession::start_guarded_with_match_hook(
                    self.region,
                    self.options.clone(),
                    gate,
                    self.match_hook.clone(),
                ),
                None => LongSession::start(self.region, self.options.clone()),
            }
        };
        match session {
            Ok(session) => {
                if let Some(density) = self
                    .preview_window
                    .and_then(|p| p.read(cx).ok().map(|p| p.density))
                {
                    session.set_preview_scale(density);
                }
                self.session = Some(session);
                self.status.clear();
            }
            Err(error) => {
                self.meta.phase = LongPhase::Failed;
                self.meta.busy = false;
                self.meta.worker_alive = false;
                self.meta.worker_exited = true;
                self.status = error.to_string();
            }
        }
        cx.notify();
    }
    fn request_close(&mut self, cx: &mut Context<Self>) {
        self.cancel_popup_resume();
        self.closing = true;
        self.border.take();
        if let Some(preview) = self.preview_window.take() {
            let _ = preview.update(cx, |_, w, _| w.remove_window());
        }
        self.auto_pending = false;
        self.pending_editor = None;
        self.region_drag = None;
        self.pending_region = None;
        self.toolbar_drag = None;
        if let Some(session) = self.session.as_ref() {
            session.stop();
        }
        self.status = tr!(
            "正在停止并释放采集资源…",
            "Stopping and releasing capture resources…"
        )
        .into();
        cx.notify();
    }
    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing {
            if self
                .session
                .as_mut()
                .is_none_or(LongSession::try_join_stopped)
            {
                self.capture_lock.take();
                window.remove_window();
            }
            return;
        }
        // 面板跟随选区或被拖动；按当前位置决定可能被远控时能否不排除采集。
        crate::remote_display::sync_tool_window(hwnd(window), &[self.region, self.shown_region]);
        self.process_region_input(window, cx);
        if let Some(session) = self.session.as_ref() {
            if let Some(density) = self
                .preview_window
                .and_then(|p| p.read(cx).ok().map(|p| p.density))
            {
                session.set_preview_scale(density);
            }
            if let Some(error) = self
                .preview_window
                .and_then(|p| p.read(cx).ok()?.error.clone())
            {
                self.status = error;
                self.status_until = None;
                session.stop();
            }
            let next = session.snapshot();
            if next.result_available
                && (next.width > self.meta.width || next.height > self.meta.height)
            {
                self.growth_note = Some((
                    Instant::now(),
                    trf!(
                        "已拼接至 {} × {} · 继续滚动",
                        "Stitched to {} × {} · keep scrolling",
                        next.width,
                        next.height
                    ),
                ));
            }
            self.meta = next;
            // 实时拖动时边框跟手，已提交位置略滞后；松手时再对齐。
            if self.region_drag.is_none() {
                self.adopt_committed_region(self.meta.region);
            }
        }
        self.follow_region(window, cx);
        self.sync_preview(window, cx);
        // 隐藏期间窗口不出帧，逐帧循环会停；重新可见后由轮询补挂。
        // 编辑窗口打开时面板隐藏、采集暂停，不补挂，免得逐帧回调持续占用主线程。
        if !self.editor_open
            && self
                .last_frame_tick
                .is_none_or(|tick| tick.elapsed() > Duration::from_millis(250))
        {
            self.arm_frame_tick(window, cx);
        }
        self.advance_region_change(window, cx);
        if self.session.is_some() {
            self.meta = self.snapshot();
        }
        if let Some(generation) = self.auto_pending_generation {
            if self.meta.requested_generation != generation
                || (self.meta.generation == generation)
                || matches!(self.meta.phase, LongPhase::Failed | LongPhase::Stopped)
            {
                self.auto_pending = false;
                self.auto_pending_generation = None;
                self.cancel_popup_resume();
            }
        }
        self.advance_popup_resume(window, cx);
        self.set_parameters_enabled(
            !self.region_adjusting()
                && !self.editor_open
                && self.pending_editor.is_none()
                && !self.closing,
            cx,
        );
        let quiet = self.session.as_ref().is_some_and(|session| {
            let p = session.pipeline_probe();
            p.producer_quiesced
                && !p.capture_in_flight
                && !p.consumer_in_flight
                && p.queue_depth == 0
                && p.reserved_raw_bytes == 0
                && p.drain_fence.is_none()
        });
        if let Some(border) = self.border.as_mut() {
            let allow = !self.editor_open
                && self.pending_editor.is_none()
                && self.region_drag.is_none()
                && self.pending_region.is_none()
                && self.meta.phase == LongPhase::Paused
                && self.meta.generation == self.meta.requested_generation
                && !self.meta.busy
                && quiet;
            if let Err(error) = border.set_resize_enabled(allow) {
                self.status = error.to_string();
            }
        }
        if self
            .status_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.status.clear();
            self.status_until = None;
        }
        if self.pending_editor.is_some() {
            let pending = self.pending_editor.as_ref().expect("已检查编辑请求");
            if pending.generation != self.snapshot().requested_generation {
                self.pending_editor = None;
                self.status = tr!(
                    "采集操作已变更，本次编辑或输出已取消",
                    "The capture changed; this edit or output was canceled"
                )
                .into();
            } else if self.meta.phase == LongPhase::Failed && !pending.allow_failed_result {
                self.pending_editor = None;
                self.status = trf!(
                    "{}；可再次编辑或保存已有结果",
                    "{}; you can edit or save the existing result again",
                    self.meta.status
                );
            } else if let Some(snapshot) = self.stitch_snapshot() {
                let pending = self.pending_editor.take().expect("已检查编辑请求");
                self.launch_output(snapshot, pending, window, cx);
            } else if !self.meta.busy && !self.meta.result_available {
                self.pending_editor = None;
                self.status = tr!("还没有成功采集的图像", "Nothing has been captured yet").into();
            }
        }
        // 编辑期间面板隐藏，不必每 50ms 重绘；返回采集后轮询照常刷新。
        if !self.editor_open {
            cx.notify();
        }
    }
    /// 每个显示帧调度一次；纪元变化（重新挂起）即淘汰旧链，避免重复循环。
    fn arm_frame_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.frame_tick_epoch = self.frame_tick_epoch.wrapping_add(1);
        self.last_frame_tick = Some(Instant::now());
        Self::schedule_frame_tick(self.frame_tick_epoch, cx.entity().downgrade(), window);
    }
    fn schedule_frame_tick(epoch: u64, view: gpui::WeakEntity<Self>, window: &Window) {
        window.on_next_frame(move |window, cx| {
            let keep = view
                .update(cx, |v, cx| {
                    if v.frame_tick_epoch != epoch || v.closing || v.editor_open {
                        return false;
                    }
                    v.last_frame_tick = Some(Instant::now());
                    v.frame_tick(window, cx);
                    true
                })
                .unwrap_or(false);
            if keep {
                Self::schedule_frame_tick(epoch, view, window);
            }
        });
    }
    /// 轻量逐帧路径：只处理拖动输入、选区跟随、选区确认推进与预览交接；其余状态仍由 50ms 轮询维护。
    fn frame_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.process_region_input(window, cx);
        if self.pending_region.is_some() {
            if let Some(region) = self.session.as_ref().map(|s| s.snapshot().region) {
                self.adopt_committed_region(region);
            }
            self.advance_region_change(window, cx);
        }
        self.follow_region(window, cx);
        self.sync_preview(window, cx);
        if input {
            cx.notify();
        }
    }
    /// 已提交的物理 ROI 同时驱动边框、预览与下一次自动滚动目标。
    fn adopt_committed_region(&mut self, region: ScreenRect) {
        if region == self.region {
            return;
        }
        self.region = region;
        self.preview_key = None;
        if let Some(border) = self.border.as_mut() {
            if let Err(error) = border.update_region(region) {
                self.status = error.to_string();
                if let Some(session) = self.session.as_ref() {
                    session.stop();
                }
            }
        }
    }
    /// 预览与操作条显示的选区：拖动中的草稿、待确认的新选区，否则为已提交选区。
    fn displayed_region(&self) -> ScreenRect {
        self.region_drag
            .as_ref()
            .map(|drag| drag.draft)
            .or_else(|| self.pending_region.as_ref().map(|pending| pending.region))
            .unwrap_or(self.region)
    }
    /// 选区（含拖动草稿）变化时预览与操作条立即跟随，不做过渡。
    fn follow_region(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let region = self.displayed_region();
        if region == self.shown_region {
            return;
        }
        self.shown_region = region;
        let toolbar = panel_placement(region, self.settings_open);
        if self.toolbar_drag.is_none() {
            crate::pin_native::place_later(hwnd(window), toolbar, Some(true), false, cx);
        }
        let Some(preview) = self.preview_window else {
            return;
        };
        let error = match preview.update(cx, |v, w, cx| v.set_region(region, Some(toolbar), w, cx))
        {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(error) => error.to_string(),
        };
        self.status = error;
        if let Some(session) = self.session.as_ref() {
            session.stop();
        }
    }
    /// 后台每发布一版缩略图就交给预览窗口；逐帧检查，不受轮询间隔限制。
    fn sync_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(preview)) = (self.session.as_ref(), self.preview_window) else {
            return;
        };
        let Some(frame) = session.preview_frame() else {
            return;
        };
        let key = (frame.generation, frame.revision);
        let predicted = panel_placement(self.shown_region, self.settings_open);
        // 拖动或待确认期间操作条正随选区移动，实际窗口位置滞后一帧；按预测位置避让，
        // 避免预览在新旧两个位置之间来回跳。
        let toolbar = if self.region_drag.is_some() || self.pending_region.is_some() {
            predicted
        } else {
            crate::pin_native::rect(hwnd(window)).unwrap_or(predicted)
        };
        if self.preview_key == Some(key) && self.preview_toolbar == Some(toolbar) {
            return;
        }
        match preview.update(cx, |v, w, cx| v.set_frame(frame, toolbar, w, cx)) {
            Ok(Ok(())) => {
                self.preview_key = Some(key);
                self.preview_toolbar = Some(toolbar);
            }
            result => {
                self.status = match result {
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                    _ => unreachable!(),
                };
                self.status_until = None;
                session.stop();
            }
        }
    }
    fn move_region_input(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut point = POINT {
            x: (f32::from(event.position.x) * window.scale_factor()).round() as i32,
            y: (f32::from(event.position.y) * window.scale_factor()).round() as i32,
        };
        if !unsafe { ClientToScreen(hwnd(window), &mut point) }.as_bool() {
            self.status = tr!(
                "无法确定拖动起点",
                "Cannot determine where the drag started"
            )
            .into();
            return;
        }
        let result = self
            .border
            .as_mut()
            .ok_or("选区边框尚未准备完成")
            .and_then(|border| {
                border
                    .begin_move_capture(hwnd(window), (point.x, point.y))
                    .map_err(|_| "无法捕获选区拖动")
            });
        if let Err(error) = result {
            self.status = error.into();
        } else {
            self.process_region_input(window, cx);
        }
        cx.notify();
    }
    /// 返回本次是否处理了输入事件。
    fn process_region_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let events = self
            .border
            .as_ref()
            .map_or_else(Vec::new, LongCaptureBorder::drain_events);
        let handled = !events.is_empty();
        for event in events {
            if let Some(drag) = self.toolbar_drag.as_ref() {
                let id = drag.id;
                let point = match &event {
                    RegionInputEvent::Down { id: event_id, .. } if *event_id == id => continue,
                    RegionInputEvent::Move {
                        id: event_id,
                        point,
                    }
                    | RegionInputEvent::Up {
                        id: event_id,
                        point,
                    } if *event_id == id => Some(*point),
                    RegionInputEvent::Cancel { id: event_id } if *event_id == id => {
                        let placement = drag.placement;
                        self.toolbar_drag = None;
                        crate::pin_native::place_later(
                            hwnd(window),
                            placement,
                            Some(true),
                            false,
                            cx,
                        );
                        continue;
                    }
                    _ => None,
                };
                if let Some(point) = point {
                    let mut placement = drag.placement;
                    let work = crate::pin_native::work_area(point);
                    let x = i64::from(placement.x) + i64::from(point.0) - i64::from(drag.start.0);
                    let y = i64::from(placement.y) + i64::from(point.1) - i64::from(drag.start.1);
                    placement.x = x.clamp(
                        i64::from(work.x),
                        i64::from(work.x) + i64::from(work.width.saturating_sub(placement.width)),
                    ) as i32;
                    placement.y = y.clamp(
                        i64::from(work.y),
                        i64::from(work.y) + i64::from(work.height.saturating_sub(placement.height)),
                    ) as i32;
                    crate::pin_native::place_later(hwnd(window), placement, Some(true), false, cx);
                    if matches!(event, RegionInputEvent::Up { .. }) {
                        self.toolbar_drag = None;
                    }
                    continue;
                }
            }
            match event {
                RegionInputEvent::Down { id, handle, point } => {
                    let result = self.begin_region_drag(id, handle, point, cx);
                    if let Err(error) = result {
                        if let Some(border) = &self.border {
                            border.cancel_input();
                        }
                        self.status = error.to_string();
                        self.restore_panel_focus(window, cx);
                    } else {
                        window.focus(&self.focus);
                        self.set_parameters_enabled(false, cx);
                    }
                }
                RegionInputEvent::Move { id, point } => {
                    let Some(drag) = self.region_drag.as_mut().filter(|d| d.gesture.id() == id)
                    else {
                        continue;
                    };
                    match drag.gesture.update(point) {
                        Ok(region) if drag.live => {
                            if region == drag.draft {
                                continue;
                            }
                            let moved = match self.session.as_mut() {
                                Some(session) => session.move_region_live(region),
                                None => Err("采集器尚未准备完成".into()),
                            };
                            match moved {
                                Ok(()) => {
                                    drag.draft = region;
                                    if let Some(border) = self.border.as_mut() {
                                        if let Err(error) = border.update_region(region) {
                                            self.status = error.to_string();
                                        }
                                    }
                                }
                                Err(error) => self.status = error.to_string(),
                            }
                        }
                        Ok(region) => {
                            drag.draft = region;
                            if let Some(border) = self.border.as_mut() {
                                if let Err(error) = border.update_region(region) {
                                    self.status = error.to_string();
                                }
                            }
                            self.status = trf!(
                                "调整选区 {} × {} · 松开确认，Esc 取消",
                                "Adjusting area {} × {} · release to confirm, Esc cancels",
                                region.width,
                                region.height
                            );
                        }
                        Err(error) => self.status = error.to_string(),
                    }
                }
                RegionInputEvent::Up { id, point } => {
                    if self
                        .region_drag
                        .as_ref()
                        .is_none_or(|d| d.gesture.id() != id)
                    {
                        continue;
                    }
                    let drag = self.region_drag.take().expect("已检查手势");
                    if drag.live {
                        // 已逐步生效，松手只把界面的已提交选区对齐到最后一次移动。
                        self.adopt_committed_region(drag.draft);
                        self.status_until = None;
                        self.status =
                            tr!("选区已移动，继续采集", "Area moved; still capturing").into();
                        window.focus(&self.focus);
                        self.restore_panel_focus(window, cx);
                        continue;
                    }
                    let region = drag.gesture.update(point).unwrap_or(drag.draft);
                    self.pending_region = Some(PendingRegion {
                        region,
                        change: if drag.resume {
                            RegionChange::Continue
                        } else {
                            RegionChange::Reset
                        },
                        resume: drag.resume,
                        stage: RegionStage::Pause(self.snapshot().requested_generation),
                    });
                    self.status = tr!(
                        "正在等待采集暂停，随后验证新选区…",
                        "Waiting for capture to pause before checking the new area…"
                    )
                    .into();
                    window.focus(&self.focus);
                    self.restore_panel_focus(window, cx);
                }
                RegionInputEvent::Cancel { id } => {
                    if self
                        .region_drag
                        .as_ref()
                        .is_some_and(|d| d.gesture.id() == id)
                    {
                        self.cancel_region_adjustment(cx);
                        window.focus(&self.focus);
                        self.restore_panel_focus(window, cx);
                    }
                }
            }
        }
        handled
    }
    fn set_parameters_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.parameters_enabled != enabled {
            self.parameters_enabled = enabled;
            for field in [&self.fixed_start, &self.fixed_end] {
                field.update(cx, |v, cx| v.set_enabled(enabled, cx));
            }
        }
    }
    fn restore_panel_focus(&self, window: &Window, cx: &mut Context<Self>) {
        let native = hwnd(window).0 as usize;
        let mut allowed = self.border.as_ref().map_or_else(Vec::new, |border| {
            border
                .resize_handles()
                .iter()
                .map(|(_, h, _)| h.0 as usize)
                .collect()
        });
        allowed.push(native);
        let own = window
            .window_handle()
            .downcast::<Self>()
            .expect("长截图操作条");
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let alive = own
                    .update(&mut async_cx, |v, w, _| {
                        !v.closing && hwnd(w).0 as usize == native
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
                // 自有缩放柄结束后把键盘交回操作条；用户已切到别的应用时保持其选择。
                unsafe {
                    if !allowed.contains(&(GetForegroundWindow().0 as usize)) {
                        return;
                    }
                    let panel = HWND(native as *mut _);
                    if SetForegroundWindow(panel).as_bool() || GetForegroundWindow() == panel {
                        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(panel));
                    }
                }
            })
            .detach();
    }
    fn move_toolbar_input(&mut self, e: &gpui::MouseDownEvent, w: &Window, cx: &mut Context<Self>) {
        if self.region_adjusting() || self.toolbar_drag.is_some() {
            return;
        }
        let mut point = POINT {
            x: (f32::from(e.position.x) * w.scale_factor()).round() as i32,
            y: (f32::from(e.position.y) * w.scale_factor()).round() as i32,
        };
        if !unsafe { ClientToScreen(hwnd(w), &mut point) }.as_bool() {
            return;
        }
        let placement = match crate::pin_native::rect(hwnd(w)) {
            Ok(p) => p,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        if let Some(border) = self.border.as_mut() {
            match border.begin_move_capture(hwnd(w), (point.x, point.y)) {
                Ok(id) => {
                    self.toolbar_drag = Some(ToolbarDrag {
                        id,
                        start: (point.x, point.y),
                        placement,
                    })
                }
                Err(error) => self.status = error.to_string(),
            }
        }
        cx.notify();
    }
    fn begin_region_drag(
        &mut self,
        id: u64,
        handle: RegionHandle,
        point: (i32, i32),
        cx: &App,
    ) -> Result<()> {
        if self.closing
            || self.editor_open
            || self.pending_editor.is_some()
            || self.region_adjusting()
            || self.composing(cx)
        {
            return Err(tr!("请先完成当前操作", "Finish the current action first").into());
        }
        let meta = self.snapshot();
        if meta.generation != meta.requested_generation
            || (meta.busy && meta.phase == LongPhase::Paused)
            || !meta.result_available
            || !matches!(meta.phase, LongPhase::Running | LongPhase::Paused)
        {
            return Err(tr!(
                "请等待当前采集完成后调整选区",
                "Wait for the current capture to finish before adjusting the area"
            )
            .into());
        }
        let resume = meta.phase == LongPhase::Running;
        if resume && handle != RegionHandle::Move {
            return Err(tr!("暂停后可调整选区尺寸", "Pause to resize the area").into());
        }
        let reach = self
            .session
            .as_ref()
            .filter(|_| resume)
            .and_then(LongSession::live_move_reach);
        let live = reach.is_some();
        let min_along = self
            .options
            .stitch
            .fixed_start
            .unwrap_or(0)
            .saturating_add(self.options.stitch.fixed_end.unwrap_or(0))
            .saturating_add(64);
        let gesture = RegionGesture::new_for_axis(
            id,
            self.region,
            point,
            handle,
            resume.then_some(self.options.stitch.axis),
            reach.unwrap_or_else(virtual_bounds),
            self.options.stitch.axis,
            min_along,
        )?;
        let session = self.session.as_mut().ok_or("采集器尚未准备完成")?;
        if live {
            self.region_drag = Some(RegionDrag {
                gesture,
                draft: self.region,
                resume,
                live,
            });
            self.status_until = None;
            self.status = tr!(
                "沿滚动方向拖动选区，长图实时拼接 · Esc 回到原位",
                "Drag the area along the scroll direction; the long image keeps stitching · Esc returns"
            )
            .into();
            return Ok(());
        }
        self.auto_resume.invalidate();
        self.popup_capture_resume = None;
        self.auto_pending = false;
        if resume || meta.auto.active {
            session.set_paused(true)?;
        }
        self.region_drag = Some(RegionDrag {
            gesture,
            draft: self.region,
            resume,
            live,
        });
        self.status_until = None;
        self.status = if resume {
            tr!(
                "沿滚动方向移动选区 · 松开继续，Esc 取消",
                "Moving the area along the scroll direction · release to continue, Esc cancels"
            )
        } else {
            tr!(
                "调整选区 · 松开确认，Esc 取消",
                "Adjusting area · release to confirm, Esc cancels"
            )
        }
        .into();
        Ok(())
    }
    fn cancel_region_adjustment(&mut self, cx: &mut Context<Self>) {
        if let Some(border) = &self.border {
            border.cancel_input();
        }
        if self.region_drag.as_ref().is_some_and(|drag| drag.live) {
            // 实时拖动的取消就是把框移回原位，采集继续。
            let original = self
                .region_drag
                .take()
                .expect("已检查手势")
                .gesture
                .original();
            self.status_until = None;
            self.status = tr!("选区已回到原位，继续采集", "Area returned; still capturing").into();
            if let Some(session) = self.session.as_mut() {
                if let Err(error) = session.move_region_live(original) {
                    self.status = error.to_string();
                }
            }
            self.adopt_committed_region(original);
            if let Some(border) = self.border.as_mut() {
                let _ = border.update_region(original);
            }
            cx.notify();
            return;
        }
        if let Some(drag) = self.region_drag.take() {
            self.pending_region = Some(PendingRegion {
                region: drag.gesture.original(),
                change: RegionChange::Reset,
                resume: drag.resume,
                stage: RegionStage::Restore(self.snapshot().requested_generation),
            });
        } else if let Some(pending) = self.pending_region.as_mut() {
            if matches!(pending.stage, RegionStage::Candidate(_)) {
                match self
                    .session
                    .as_mut()
                    .expect("候选有采集器")
                    .cancel_region_change()
                {
                    Ok(()) => {}
                    Err(error) => {
                        self.status = error.to_string();
                        return;
                    }
                }
            }
            pending.region = self.region;
            pending.resume = false;
            pending.stage = RegionStage::Restore(
                self.session
                    .as_ref()
                    .expect("候选有采集器")
                    .snapshot()
                    .requested_generation,
            );
        }
        if let Some(border) = self.border.as_mut() {
            let _ = border.update_region(self.region);
        }
        self.status_until = None;
        self.status = tr!(
            "已取消调整，保留原长图",
            "Adjustment canceled; the original long image is kept"
        )
        .into();
        cx.notify();
    }
    fn advance_region_change(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_region.take() else {
            return;
        };
        let meta = self.snapshot();
        let quiet = self.session.as_ref().is_some_and(|session| {
            let p = session.pipeline_probe();
            p.producer_quiesced
                && !p.capture_in_flight
                && !p.consumer_in_flight
                && p.queue_depth == 0
                && p.reserved_raw_bytes == 0
                && p.drain_fence.is_none()
        });
        if meta.phase == LongPhase::Stopped && meta.worker_exited && quiet {
            // 预览/窗口错误也可能终止采集；实际退出后结束待办，仍可输出最后成功的长图。
            if let Some(border) = self.border.as_mut() {
                let _ = border.update_region(self.region);
            }
            self.status = trf!(
                "{}；采集已停止，保留最后成功的长图",
                "{}; capturing stopped, keeping the last good long image",
                self.status
            );
            cx.notify();
            return;
        }
        let generation = match pending.stage {
            RegionStage::Pause(g) | RegionStage::Candidate(g) | RegionStage::Restore(g) => g,
        };
        if meta.requested_generation != generation {
            if matches!(pending.stage, RegionStage::Candidate(_))
                && meta.pending_region.is_none()
                && meta.region == self.region
                && !meta.worker_exited
            {
                // 候选失败会自动推进恢复版本，继续等实际释放 ACK 才重新开放输出。
                pending.stage = RegionStage::Restore(meta.requested_generation);
                pending.resume = false;
                self.pending_region = Some(pending);
                if let Some(border) = self.border.as_mut() {
                    let _ = border.update_region(self.region);
                }
                self.status = trf!(
                    "{}；原长图和选区已保留",
                    "{}; the original long image and area are kept",
                    meta.status
                );
                return;
            }
            self.status = tr!(
                "采集请求已变化，选区调整已取消",
                "The capture request changed; the area adjustment was canceled"
            )
            .into();
            if let Some(border) = self.border.as_mut() {
                let _ = border.update_region(self.region);
            }
            return;
        }
        if meta.generation != generation
            || meta.busy
            || meta.pending_region.is_some()
            || !matches!(meta.phase, LongPhase::Paused | LongPhase::Failed)
        {
            self.pending_region = Some(pending);
            return;
        }
        if !quiet {
            self.pending_region = Some(pending);
            return;
        }
        match pending.stage {
            RegionStage::Pause(_)
                if pending.region != self.region && meta.phase == LongPhase::Paused =>
            {
                match self
                    .session
                    .as_mut()
                    .expect("选区调整有采集器")
                    .change_region(pending.region, pending.change)
                {
                    Ok(()) => {
                        pending.stage =
                            RegionStage::Candidate(self.snapshot().requested_generation);
                        self.pending_region = Some(pending);
                        self.status = tr!(
                            "正在验证新选区首帧… · Esc 保留原图",
                            "Checking the first frame of the new area… · Esc keeps the original"
                        )
                        .into();
                        return;
                    }
                    Err(error) => {
                        self.status = trf!(
                            "调整失败：{error}；原长图保留",
                            "Adjustment failed: {error}; the original long image is kept"
                        )
                    }
                }
            }
            RegionStage::Candidate(_) if meta.region != pending.region => {
                self.status = trf!(
                    "{}；原长图和选区已保留",
                    "{}; the original long image and area are kept",
                    meta.status
                );
                pending.resume = false;
            }
            RegionStage::Candidate(_) => {
                self.status = tr!("新选区已生效", "The new area is active").into();
                crate::pin_native::place_later(
                    hwnd(window),
                    panel_placement(self.region, self.settings_open),
                    Some(true),
                    false,
                    cx,
                );
            }
            _ => {}
        }
        if let Some(border) = self.border.as_mut() {
            let _ = border.update_region(self.region);
        }
        if pending.resume && meta.phase == LongPhase::Paused && (pending.region == self.region) {
            if let Err(error) = self
                .session
                .as_mut()
                .expect("已检查采集器")
                .set_paused(false)
            {
                self.status = error.to_string();
            }
        }
        self.return_scroll_focus(window, cx);
        cx.notify();
    }
    fn draft_options(&self, cx: &App) -> Result<LongOptions> {
        let mut options = self.options.clone();
        options.stitch.axis = self.draft_axis;
        match self.fixed_mode {
            FixedMode::None => {
                options.stitch.detect_fixed_borders = false;
                options.stitch.fixed_start = Some(0);
                options.stitch.fixed_end = Some(0);
            }
            FixedMode::Auto => {
                options.stitch.detect_fixed_borders = true;
                options.stitch.fixed_start = None;
                options.stitch.fixed_end = None;
            }
            FixedMode::Manual => {
                let start = self
                    .fixed_start
                    .read(cx)
                    .text()
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| {
                        tr!(
                            "固定首栏须填写非负像素数",
                            "The fixed leading band must be a non-negative number of pixels"
                        )
                    })?;
                let end = self
                    .fixed_end
                    .read(cx)
                    .text()
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| {
                        tr!(
                            "固定尾栏须填写非负像素数",
                            "The fixed trailing band must be a non-negative number of pixels"
                        )
                    })?;
                options.stitch.detect_fixed_borders = false;
                options.stitch.fixed_start = Some(start);
                options.stitch.fixed_end = Some(end);
            }
        }
        options.validate(self.region)?;
        Ok(options)
    }
    fn restart_from_draft(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let options = self.draft_options(cx)?;
        self.auto_pending = false;
        if self
            .session
            .as_ref()
            .is_some_and(|s| s.snapshot().worker_exited)
        {
            if !self
                .session
                .as_mut()
                .is_none_or(LongSession::try_join_stopped)
            {
                return Err(tr!(
                    "采集器正在释放资源，请稍候再试",
                    "The capturer is releasing resources; try again shortly"
                )
                .into());
            }
            self.session = None;
        }
        if let Some(session) = self.session.as_mut() {
            session.restart(options.clone())?;
        } else if self.setup_ready && !self.initializing {
            self.options = options.clone();
            self.start(cx);
        } else {
            return Err(tr!("采集器还在准备中", "The capturer is still preparing").into());
        }
        self.options = options;
        self.preview_key = None;
        Ok(())
    }
    fn return_scroll_focus(&self, window: &Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(context) = session.auto_resume_context() else {
            return;
        };
        let Ok(target) = session.auto_resume_target(&context.target) else {
            return;
        };
        let Some(own) = window.window_handle().downcast::<Self>() else {
            return;
        };
        let native = hwnd(window).0 as usize;
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let valid = own
                    .update(&mut async_cx, |v, w, _| {
                        !v.closing
                            && !v.editor_open
                            && v.pending_editor.is_none()
                            && !v.settings_open
                            && !v.region_adjusting()
                            && hwnd(w).0 as usize == native
                            && v.session.as_ref().is_some_and(|session| {
                                session.auto_resume_context().as_ref() == Some(&context)
                            })
                    })
                    .unwrap_or(false);
                if valid {
                    let _ =
                        target.restore_focus(HWND(native as *mut _), context.region, context.axis);
                }
            })
            .detach();
    }
    /// 先离开 App 借用交回滚动焦点，再启用严格绑定目标的后台驱动。
    fn begin_auto(&mut self, window: &Window, cx: &mut Context<Self>) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or(tr!("采集器还在准备中", "The capturer is still preparing"))?;
        let context = session.auto_resume_context().ok_or("采集上下文尚未确认")?;
        let target = session.auto_resume_target(&context.target)?;
        let options = self.auto_options;
        self.auto_request = self
            .auto_request
            .checked_add(1)
            .ok_or("自动滚动请求版本溢出")?;
        let request = self.auto_request;
        self.auto_pending = true;
        self.auto_pending_generation = None;
        let native = hwnd(window).0 as usize;
        let own = window
            .window_handle()
            .downcast::<Self>()
            .ok_or("长截图面板已关闭")?;
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let alive = own
                    .update(&mut async_cx, |v, w, _| {
                        v.auto_pending
                            && v.auto_request == request
                            && !v.settings_open
                            && !v.closing
                            && !v.editor_open
                            && !v.region_adjusting()
                            && v.session.as_ref().is_some_and(|session| {
                                session.auto_resume_context().as_ref() == Some(&context)
                            })
                            && hwnd(w).0 as usize == native
                    })
                    .unwrap_or(false);
                if !alive {
                    let _ = own.update(&mut async_cx, |v, _, cx| {
                        if v.auto_request == request && v.auto_pending && !v.settings_open {
                            v.auto_pending = false;
                            v.auto_pending_generation = None;
                            v.cancel_popup_resume();
                            cx.notify();
                        }
                    });
                    return;
                }
                let focus =
                    target.restore_focus(HWND(native as *mut _), context.region, context.axis);
                let _ = own.update(&mut async_cx, |v, _, cx| {
                    if !v.auto_pending
                        || v.auto_request != request
                        || v.settings_open
                        || v.closing
                        || v.editor_open
                        || v.pending_editor.is_some()
                        || v.region_adjusting()
                    {
                        return;
                    }
                    if v.auto_options != options
                        || !v.session.as_ref().is_some_and(|session| {
                            session.auto_resume_context().as_ref() == Some(&context)
                        })
                    {
                        v.auto_pending = false;
                        v.auto_pending_generation = None;
                        v.cancel_popup_resume();
                        return;
                    }
                    let result = focus.and_then(|()| {
                        let session = v.session.as_mut().ok_or("采集器已关闭")?;
                        session.validate_auto_resume_target(&context.target)?;
                        session.set_auto_scroll(Some(options))?;
                        Ok(session.snapshot().requested_generation)
                    });
                    match result {
                        Ok(generation) => {
                            let _ = v.auto_resume.take_ready(&context, true);
                            v.auto_pending_generation = Some(generation);
                        }
                        Err(error) => {
                            v.auto_pending = false;
                            v.auto_pending_generation = None;
                            v.cancel_popup_resume();
                            v.status = error.to_string();
                            v.status_until = None;
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        Ok(())
    }
    fn cancel_popup_resume(&mut self) {
        self.auto_resume.invalidate();
        self.popup_capture_resume = None;
        self.popup_manual_generation = None;
    }
    fn toggle_settings(&mut self, window: &Window, cx: &mut Context<Self>) -> Result<()> {
        if self.settings_open {
            self.settings_open = false;
            self.auto_resume.close();
        } else {
            let meta = self.snapshot();
            if self
                .popup_capture_resume
                .as_ref()
                .is_some_and(|context| context.generation == meta.requested_generation)
                && self.auto_pending_generation.is_none()
                && self.popup_manual_generation.is_none()
            {
                // 尚在等待同一次暂停 ACK；快速重开不能丢失意图或再发一个暂停代。
                self.auto_resume.reopen();
            } else {
                let pending = self.auto_pending || self.auto_resume.is_pending();
                let manual_pending = self.popup_manual_generation.is_some();
                self.cancel_popup_resume();
                if meta.phase == LongPhase::Running || pending || meta.auto.active || manual_pending
                {
                    let session = self.session.as_mut().ok_or("采集器尚未准备完成")?;
                    let opened = session.auto_resume_context().ok_or("采集上下文尚未确认")?;
                    session.set_paused(true)?;
                    let generation = session.snapshot().requested_generation;
                    self.auto_resume.suspend(
                        meta.auto.active,
                        pending,
                        self.auto_options,
                        opened.clone(),
                        generation,
                    )?;
                    self.popup_capture_resume = Some(ResumeContext {
                        generation,
                        ..opened
                    });
                }
            }
            self.auto_pending = false;
            self.auto_pending_generation = None;
            self.settings_open = true;
        }
        self.resize_settings(window, cx);
        Ok(())
    }
    fn advance_popup_resume(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(generation) = self.popup_manual_generation {
            let meta = self.snapshot();
            if meta.requested_generation != generation
                || meta.generation == generation
                || matches!(meta.phase, LongPhase::Failed | LongPhase::Stopped)
            {
                self.cancel_popup_resume();
            }
            return;
        }
        if self.auto_pending {
            return;
        }
        let Some(expected) = self.popup_capture_resume.as_ref() else {
            return;
        };
        let meta = self.snapshot();
        if self.closing
            || self.editor_open
            || self.pending_editor.is_some()
            || self.region_adjusting()
            || meta.requested_generation != expected.generation
            || meta.region != expected.region
            || meta.axis != expected.axis
            || matches!(meta.phase, LongPhase::Failed | LongPhase::Stopped)
        {
            self.cancel_popup_resume();
            return;
        }
        if self.settings_open
            || meta.generation != meta.requested_generation
            || meta.phase != LongPhase::Paused
            || meta.busy
        {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            self.cancel_popup_resume();
            return;
        };
        let probe = session.pipeline_probe();
        if !probe.producer_quiesced
            || probe.capture_in_flight
            || probe.consumer_in_flight
            || probe.queue_depth != 0
            || probe.reserved_raw_bytes != 0
            || probe.drain_fence.is_some()
        {
            return;
        }
        let Some(context) = session.auto_resume_context() else {
            self.cancel_popup_resume();
            return;
        };
        if context.target != expected.target {
            self.cancel_popup_resume();
            return;
        }
        let automatic = self.auto_resume.is_pending();
        if automatic {
            if let Err(error) = session.validate_auto_resume_target(&expected.target) {
                self.cancel_popup_resume();
                self.status = trf!(
                    "{error}；已保留长图，自动恢复已取消",
                    "{error}; the long image is kept and auto-resume was canceled"
                );
                self.status_until = None;
                return;
            }
        }
        let options = self.auto_resume.ready_options(&context, true);
        let result = if let Some(options) = options {
            self.auto_options = options;
            self.begin_auto(window, cx)
        } else if automatic {
            Err(tr!(
                "采集操作已变更，自动恢复已取消",
                "The capture changed; auto-resume was canceled"
            )
            .into())
        } else {
            let session = self.session.as_mut().expect("恢复仍有采集器");
            let target = session.auto_resume_target(&context.target).ok();
            let result = session.set_paused(false);
            if result.is_ok() {
                let generation = session.snapshot().requested_generation;
                self.popup_manual_generation = Some(generation);
                self.popup_capture_resume = Some(ResumeContext {
                    generation,
                    ..context.clone()
                });
                if let Some(target) = target {
                    let native = hwnd(window).0 as usize;
                    let own = window
                        .window_handle()
                        .downcast::<Self>()
                        .expect("长截图面板类型");
                    let mut async_cx = cx.to_async();
                    let expected = ResumeContext {
                        generation,
                        ..context
                    };
                    cx.foreground_executor()
                        .spawn(async move {
                            let alive = own
                                .update(&mut async_cx, |v, w, _| {
                                    !v.settings_open
                                        && !v.closing
                                        && !v.editor_open
                                        && v.pending_editor.is_none()
                                        && !v.region_adjusting()
                                        && hwnd(w).0 as usize == native
                                        && v.session.as_ref().is_some_and(|session| {
                                            session.auto_resume_context().as_ref()
                                                == Some(&expected)
                                        })
                                })
                                .unwrap_or(false);
                            if alive {
                                let _ = target.restore_focus(
                                    HWND(native as *mut _),
                                    expected.region,
                                    expected.axis,
                                );
                            }
                        })
                        .detach();
                }
            }
            result
        };
        if let Err(error) = result {
            self.auto_pending = false;
            self.auto_pending_generation = None;
            self.cancel_popup_resume();
            self.status = error.to_string();
            self.status_until = None;
        }
    }
    fn resize_settings(&self, window: &Window, cx: &mut Context<Self>) {
        let mut placement = panel_placement(self.region, self.settings_open);
        if let Ok(current) = crate::pin_native::rect(hwnd(window)) {
            let work = crate::pin_native::work_area((current.x, current.y));
            placement.x = current.x.clamp(
                work.x,
                work.x + work.width.saturating_sub(placement.width) as i32,
            );
            let y = if current.y < self.region.top {
                current.y as i64 + current.height as i64 - placement.height as i64
            } else {
                current.y as i64
            };
            placement.y = y.clamp(
                work.y as i64,
                work.y as i64 + work.height.saturating_sub(placement.height) as i64,
            ) as i32;
        }
        crate::pin_native::place_later(hwnd(window), placement, Some(true), false, cx);
    }
    fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(action, Control::Close) {
            self.request_close(cx);
            return;
        }
        // 原生把手先排队，点击操作可能早于下一次 poll；先落实手势，再检查操作屏障。
        self.process_region_input(window, cx);
        if self.closing
            || self.editor_open
            || self.pending_editor.is_some()
            || self.region_adjusting()
        {
            return;
        }
        if self.composing(cx) {
            self.status = tr!("请先确认中文输入", "Confirm the IME input first").into();
            cx.notify();
            return;
        }
        let meta = self.snapshot();
        if (matches!(action, Control::Output(_)) && !self.output_ready(&meta))
            || (matches!(action, Control::Pause) && !self.pause_ready(&meta))
        {
            return;
        }
        // 超限时按钮仍可点，点了说明原因和做法，而不是置灰后毫无反应。
        if matches!(action, Control::Output(_)) {
            if let Some(message) = long_output::limit_message(meta.width, meta.height) {
                self.status = message;
                cx.notify();
                return;
            }
        }
        window.focus(&self.focus);
        self.status.clear();
        if matches!(
            action,
            Control::Pause
                | Control::Undo
                | Control::Restart
                | Control::Output(_)
                | Control::Axis(_)
                | Control::AutoDirection(_)
        ) {
            self.cancel_popup_resume();
        }
        let result: Result<()> = (|| {
            match action {
                Control::Axis(axis) => {
                    if self.draft_axis != axis {
                        let before = self.draft_axis;
                        self.draft_axis = axis;
                        if let Err(e) = self.restart_from_draft(cx) {
                            self.draft_axis = before;
                            return Err(e);
                        }
                        self.status = tr!(
                            "已切换方向，从当前页面重新采集",
                            "Direction switched; capturing again from the current page"
                        )
                        .into();
                    }
                }
                Control::Fixed(mode) => {
                    self.fixed_mode = mode;
                }
                Control::Restart => {
                    self.restart_from_draft(cx)?;
                    self.status =
                        tr!("从当前页面重新采集", "Capture again from the current page").into();
                }
                Control::Pause => {
                    self.auto_pending = false;
                    let paused =
                        matches!(self.snapshot().phase, LongPhase::Paused | LongPhase::Failed);
                    self.session
                        .as_mut()
                        .ok_or("采集器尚未准备完成")?
                        .set_paused(!paused)?;
                }
                Control::Undo => {
                    self.auto_pending = false;
                    self.session.as_mut().ok_or("采集器尚未准备完成")?.undo()?;
                }
                Control::Output(action) => {
                    self.auto_pending = false;
                    let snapshot = self.snapshot();
                    let resume = snapshot.generation == snapshot.requested_generation
                        && matches!(snapshot.phase, LongPhase::Running | LongPhase::Starting);
                    let session = self.session.as_mut().ok_or("采集器尚未准备完成")?;
                    // 编辑/输出消费请求时已发起的尾帧，真实暂停 ACK 后才移交共享条带。
                    if snapshot.worker_alive && !snapshot.worker_exited {
                        session.drain_for_output()?;
                    }
                    self.pending_editor = Some(PendingEditor {
                        action,
                        resume,
                        generation: session.snapshot().requested_generation,
                        allow_failed_result: matches!(
                            snapshot.phase,
                            LongPhase::Failed | LongPhase::Stopped
                        ),
                    });
                    self.status = tr!(
                        "正在处理已采集的尾帧，随后打开编辑…",
                        "Processing the last frames, then opening the editor…"
                    )
                    .into();
                }
                Control::Auto => {
                    if self.auto_resume.is_pending()
                        || (self.popup_capture_resume.is_some() && self.auto_pending)
                    {
                        self.auto_resume.invalidate();
                        self.auto_pending = false;
                        self.auto_pending_generation = None;
                        let meta = self.snapshot();
                        if meta.phase != LongPhase::Paused
                            || meta.generation != meta.requested_generation
                        {
                            self.session
                                .as_mut()
                                .ok_or("采集器尚未准备完成")?
                                .set_paused(true)?;
                            if let Some(context) = self.popup_capture_resume.as_mut() {
                                context.generation = self
                                    .session
                                    .as_ref()
                                    .expect("停止仍有采集器")
                                    .snapshot()
                                    .requested_generation;
                            }
                        }
                        self.status = tr!("已停止自动滚动；关闭设置后可继续手动采集", "Auto-scroll stopped; close the settings to continue capturing manually").into();
                    } else if self.auto_pending || self.snapshot().auto.active {
                        self.auto_pending = false;
                        self.session
                            .as_mut()
                            .ok_or(tr!("采集器还在准备中", "The capturer is still preparing"))?
                            .set_auto_scroll(None)?;
                        self.status = tr!(
                            "已停止自动滚动，仍可手动滚动采集",
                            "Auto-scroll stopped; you can still scroll manually to capture"
                        )
                        .into();
                    } else {
                        self.cancel_popup_resume();
                        if self.settings_open {
                            self.settings_open = false;
                            self.resize_settings(window, cx);
                        }
                        self.begin_auto(window, cx)?;
                    }
                }
                Control::AutoDirection(direction) => {
                    self.auto_pending = false;
                    if let Some(session) = self.session.as_mut() {
                        session.set_auto_scroll(None)?;
                    }
                    self.auto_options.direction = direction;
                    self.status = tr!(
                        "方向已设置；点击自动滚动开始",
                        "Direction set; click Auto-scroll to start"
                    )
                    .into();
                }
                Control::Settings => {
                    self.toggle_settings(window, cx)?;
                }
                Control::Close => unreachable!(),
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.status = error.to_string();
            self.status_until = None;
        } else if !self.status.is_empty() {
            self.status_until = Some(Instant::now() + Duration::from_millis(1400));
        }
        if !matches!(action, Control::Output(_) | Control::Fixed(_))
            && !self.settings_open
            && self.popup_capture_resume.is_none()
            && !self.auto_pending
            && !matches!(action, Control::Auto)
        {
            self.return_scroll_focus(window, cx);
        }
        self.poll(window, cx);
        cx.notify();
    }
    /// 输出交接：后台生成成品期间面板留在原处显示进度、控件停用；成品就绪后隐藏边框、预览与面板，
    /// 再交给编辑器、剪贴板、保存面板或贴图。保存取消或失败时恢复显示，并按原运行意图继续。
    fn launch_output(
        &mut self,
        snapshot: StitchSnapshot,
        pending: PendingEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_context = self
            .session
            .as_ref()
            .and_then(LongSession::auto_resume_context)
            .filter(|context| context.generation == pending.generation);
        self.editor_open = true;
        self.status = tr!("正在生成长图…", "Building the long image…").into();
        cx.notify();
        let placement = crate::export::ClipboardPlacement::Scroll {
            axis: snapshot.axis,
            region: self.region,
        };
        let task = long_output::export(snapshot, pending.action, cx);
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, w, cx| {
                if v.closing {
                    return;
                }
                match result {
                    Ok(product) => v.hand_off(product, pending, placement, return_context, w, cx),
                    Err(error) => {
                        v.editor_open = false;
                        v.status = error;
                        v.show_after_editor(pending.resume, return_context, w, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn hand_off(
        &mut self,
        product: long_output::LongProduct,
        pending: PendingEditor,
        placement: crate::export::ClipboardPlacement,
        return_context: Option<ResumeContext<AutoResumeTarget>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(border) = &self.border {
            let _ = border.set_visible(false);
        }
        if let Some(preview) = self.preview_window {
            let _ = preview.update(cx, |v, w, cx| v.set_visible(false, w, cx));
        }
        self.status.clear();
        let own = window
            .window_handle()
            .downcast::<Self>()
            .expect("长截图面板类型");
        let native = hwnd(window).0 as usize;
        let (action, resume) = (pending.action, pending.resume);
        let mut async_cx = cx.to_async();
        // 隐藏窗口会同步派发消息，放到本轮更新之外。
        cx.foreground_executor()
            .spawn(async move {
                let alive = own
                    .update(&mut async_cx, |v, w, _| {
                        !v.closing && hwnd(w).0 as usize == native
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
                unsafe {
                    let _ = ShowWindow(HWND(native as *mut _), SW_HIDE);
                }
                let _ = async_cx.update(|cx| {
                    long_output::deliver(
                        product,
                        action,
                        HWND(native as *mut _),
                        Some(placement),
                        move |outcome, cx| {
                            let _ = own.update(cx, |v, w, cx| {
                                v.editor_open = false;
                                match outcome {
                                    LongOutcome::Finished => v.request_close(cx),
                                    LongOutcome::Return(message) => {
                                        if let Some(message) = message {
                                            v.status = message;
                                        }
                                        v.show_after_editor(resume, return_context, w, cx);
                                    }
                                }
                                cx.notify();
                            });
                        },
                        cx,
                    )
                });
            })
            .detach();
    }
    fn show_after_editor(
        &self,
        resume: bool,
        return_context: Option<ResumeContext<AutoResumeTarget>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(border) = &self.border {
            let _ = border.set_visible(true);
        }
        if let Some(preview) = self.preview_window {
            let _ = preview.update(cx, |v, w, cx| v.set_visible(true, w, cx));
        }
        let own = window
            .window_handle()
            .downcast::<Self>()
            .expect("长截图面板类型");
        let native = hwnd(window).0 as usize;
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let alive = own
                    .update(&mut async_cx, |v, w, _| {
                        !v.closing && hwnd(w).0 as usize == native
                    })
                    .unwrap_or(false);
                if !alive {
                    return;
                }
                unsafe {
                    let h = HWND(native as *mut _);
                    let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                    let _ = SetWindowPos(
                        h,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
                // 保存面板等出口关闭、桌面合成后，才重新读取选区。
                async_cx
                    .background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let Some(context) = return_context else {
                    return;
                };
                let target = own
                    .update(&mut async_cx, |v, w, _| {
                        if v.closing
                            || v.editor_open
                            || v.pending_editor.is_some()
                            || v.settings_open
                            || v.region_adjusting()
                            || hwnd(w).0 as usize != native
                        {
                            return None;
                        }
                        let session = v.session.as_ref()?;
                        if session.auto_resume_context().as_ref() != Some(&context) {
                            return None;
                        }
                        // 原窗口暂不可聚焦不妨碍手动采集；不能改找中心下的新窗口。
                        Some(session.auto_resume_target(&context.target).ok())
                    })
                    .ok()
                    .flatten();
                let Some(target) = target else {
                    return;
                };
                if let Some(target) = target {
                    let _ =
                        target.restore_focus(HWND(native as *mut _), context.region, context.axis);
                }
                let _ = own.update(&mut async_cx, |v, w, cx| {
                    if hwnd(w).0 as usize == native
                        && !v.closing
                        && !v.editor_open
                        && v.pending_editor.is_none()
                        && !v.settings_open
                        && !v.region_adjusting()
                        && resume
                    {
                        if let Some(session) = v.session.as_mut() {
                            if session.auto_resume_context().as_ref() == Some(&context) {
                                if let Err(error) = session.set_paused(false) {
                                    v.status = error.to_string();
                                }
                            }
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
    }
    fn probe(&self, id: &str) -> AnyElement {
        crate::save_panel::probe(self.ui_positions.clone(), id.to_owned())
    }
    fn move_button(&self, enabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = crate::theme::palette(cx);
        div()
            .id("long-move")
            .relative()
            .size(px(BUTTON_SIZE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.3 })
            .when(enabled, |d| {
                d.cursor(gpui::CursorStyle::ResizeAll)
                    .hover(move |s| s.bg(rgb(p.hover)))
            })
            .child(toolbar_icons::action(Icon::Move, p.foreground))
            .child(self.probe("long-move"))
            .tooltip(|_, cx| {
                cx.new(|_| LongHint(tr!("拖动选区 · 采集中沿滚动方向移动并实时拼接，暂停后自由移动", "Drag the area · while capturing it slides along the scroll direction and keeps stitching; freely when paused").into()))
                    .into()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, e, w, cx| {
                    cx.stop_propagation();
                    if enabled {
                        v.move_region_input(e, w, cx);
                    }
                }),
            )
            .into_any_element()
    }
    fn button(
        &self,
        id: &'static str,
        hint: &'static str,
        selected: bool,
        enabled: bool,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = crate::theme::palette(cx);
        let icon = match action {
            Control::Pause => {
                if matches!(self.meta.phase, LongPhase::Paused | LongPhase::Failed) {
                    Icon::Play
                } else {
                    Icon::Pause
                }
            }
            Control::Auto => self.direction_icon(self.auto_options.direction),
            Control::AutoDirection(direction) => self.direction_icon(direction),
            Control::Axis(ScrollAxis::Vertical) => Icon::AxisVertical,
            Control::Axis(ScrollAxis::Horizontal) => Icon::AxisHorizontal,
            Control::Undo => Icon::Undo,
            Control::Restart => Icon::Redo,
            Control::Settings => Icon::Settings,
            Control::Output(LongOutput::Crop) => Icon::Crop,
            Control::Output(LongOutput::Copy) => Icon::Confirm,
            Control::Output(LongOutput::Save) => Icon::Save,
            Control::Output(LongOutput::Pin) => Icon::Pin,
            Control::Close => Icon::Close,
            Control::Fixed(_) => unreachable!("参数使用值按钮"),
        };
        let primary = matches!(action, Control::Output(LongOutput::Copy));
        let foreground = if primary || selected {
            p.accent
        } else if matches!(action, Control::Close) {
            p.muted
        } else {
            p.foreground
        };
        div()
            .id(id)
            .group(id)
            .relative()
            .size(px(BUTTON_SIZE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.3 })
            .text_size(px(12.))
            .text_color(rgb(foreground))
            .bg(if selected && !primary {
                rgb(p.selected)
            } else {
                rgba(0)
            })
            .when(enabled, |el| {
                el.cursor_pointer().hover(move |s| {
                    if matches!(action, Control::Close) {
                        s.bg(rgba(p.danger_bg))
                    } else if selected {
                        s.bg(rgb(p.selected))
                    } else {
                        s.bg(rgb(p.hover))
                    }
                })
            })
            .child(
                toolbar_icons::action(icon, foreground)
                    .when(enabled && matches!(action, Control::Close), |icon| {
                        icon.group_hover(id, move |s| s.text_color(rgb(p.danger)))
                    }),
            )
            .child(self.probe(id))
            .tooltip(move |_, cx| cx.new(|_| LongHint(hint.into())).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.control(action, w, cx);
                }
            }))
            .into_any_element()
    }
    fn direction_icon(&self, direction: AutoDirection) -> Icon {
        match (self.draft_axis, direction) {
            (ScrollAxis::Vertical, AutoDirection::Forward) => Icon::AutoDown,
            (ScrollAxis::Vertical, AutoDirection::Reverse) => Icon::AutoUp,
            (ScrollAxis::Horizontal, AutoDirection::Forward) => Icon::AutoRight,
            (ScrollAxis::Horizontal, AutoDirection::Reverse) => Icon::AutoLeft,
        }
    }
    fn value_button(
        &self,
        id: &'static str,
        text: (&'static str, &'static str),
        selected: bool,
        enabled: bool,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, hint) = text;
        let p = crate::theme::palette(cx);
        div()
            .id(id)
            .relative()
            .h(px(28.))
            .min_w(px(58.))
            .px_2()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.3 })
            .text_color(rgb(if selected { p.accent } else { p.foreground }))
            .bg(if selected { rgb(p.selected) } else { rgba(0) })
            .when(enabled, |el| {
                el.cursor_pointer()
                    .when(!selected, |el| el.hover(move |s| s.bg(rgb(p.hover))))
            })
            .child(label)
            .child(self.probe(id))
            .tooltip(move |_, cx| cx.new(|_| LongHint(hint.into())).into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.control(action, w, cx);
                }
            }))
            .into_any_element()
    }
    fn separator(cx: &App) -> gpui::Div {
        div()
            .w(px(1.))
            .h(px(18.))
            .mx(px(4.))
            .flex_shrink_0()
            .bg(rgb(crate::theme::palette(cx).border))
    }
    fn grip(&self, enabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = crate::theme::palette(cx);
        div()
            .id("long-grip")
            .relative()
            .w(px(14.))
            .h(px(BUTTON_SIZE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor(gpui::CursorStyle::ResizeAll)
            .hover(move |s| s.bg(rgb(p.hover)))
            .child(
                toolbar_icons::action(Icon::Grip, p.muted)
                    .w(px(10.))
                    .h(px(18.)),
            )
            .tooltip(|_, cx| {
                cx.new(|_| LongHint(tr!("拖动工具栏", "Drag toolbar").into()))
                    .into()
            })
            .child(self.probe("long-grip"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, e, w, cx| {
                    if enabled {
                        v.move_toolbar_input(e, w, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
    fn primary_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let ready = self.session.is_some()
            && self.meta.worker_alive
            && !self.closing
            && !self.editor_open
            && self.pending_editor.is_none()
            && !self.region_adjusting();
        let enabled = !self.closing
            && !self.editor_open
            && self.pending_editor.is_none()
            && !self.region_adjusting();
        // 按钮与快捷键、执行边界共用资格；点击后仍会再次读取会话当前代。
        let meta = self.snapshot();
        let result = self.output_ready(&meta);
        let pause_ready = self.pause_ready(&meta);
        let paused = matches!(self.meta.phase, LongPhase::Paused | LongPhase::Failed);
        let auto = self.auto_pending || self.meta.auto.active || self.auto_resume.is_pending();
        let auto_ready = ready
            && self.meta.result_available
            && matches!(self.meta.phase, LongPhase::Running | LongPhase::Paused)
            && self.meta.generation == self.meta.requested_generation;
        div()
            .id("long-primary-toolbar")
            .flex()
            .flex_wrap()
            .gap(px(PRIMARY_GAP))
            .px(px(PANEL_PADDING))
            .py(px(6.))
            .items_center()
            .flex_shrink_0()
            .child(self.grip(enabled, cx))
            .child(self.move_button(
                ready
                    && self.meta.result_available
                    && self.meta.generation == self.meta.requested_generation,
                cx,
            ))
            .child(self.button(
                "long-pause",
                if paused {
                    tr!("继续采集 · 空格", "Resume · Space")
                } else {
                    tr!("暂停采集 · 空格", "Pause · Space")
                },
                paused,
                pause_ready,
                Control::Pause,
                cx,
            ))
            .child(self.button(
                "long-auto",
                if auto {
                    tr!("停止自动滚动", "Stop auto-scroll")
                } else {
                    tr!("自动滚动", "Auto-scroll")
                },
                auto,
                auto_ready || auto,
                Control::Auto,
                cx,
            ))
            .child(self.button(
                "long-undo",
                tr!("撤销最后一段", "Undo last segment"),
                false,
                ready && self.meta.can_undo,
                Control::Undo,
                cx,
            ))
            .child(self.button(
                "long-settings",
                tr!("方向与固定栏", "Direction and fixed bands"),
                self.settings_open,
                enabled,
                Control::Settings,
                cx,
            ))
            .child(Self::separator(cx))
            .child(self.button(
                "long-crop",
                tr!(
                    "裁剪：在长图调整窗中裁边、删段",
                    "Crop: trim edges or remove parts"
                ),
                false,
                result,
                Control::Output(LongOutput::Crop),
                cx,
            ))
            .child(self.button(
                "long-save",
                tr!("保存 · Ctrl+S", "Save · Ctrl+S"),
                false,
                result,
                Control::Output(LongOutput::Save),
                cx,
            ))
            .child(self.button(
                "long-pin",
                tr!("贴到屏幕上", "Pin to the screen"),
                false,
                result,
                Control::Output(LongOutput::Pin),
                cx,
            ))
            .child(Self::separator(cx))
            .child(self.button(
                "long-copy",
                tr!(
                    "完成并复制 · Enter / Ctrl+C",
                    "Finish and copy · Enter / Ctrl+C"
                ),
                true,
                result,
                Control::Output(LongOutput::Copy),
                cx,
            ))
            .child(self.button(
                "long-close",
                tr!("取消 · Esc", "Cancel · Esc"),
                false,
                !self.closing,
                Control::Close,
                cx,
            ))
            .into_any_element()
    }
    fn configuration(&self, above: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = crate::theme::palette(cx);
        let enabled = !self.closing
            && !self.editor_open
            && self.pending_editor.is_none()
            && !self.region_adjusting();
        let vertical = self.draft_axis == ScrollAxis::Vertical;
        let direction = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .child(
                div()
                    .min_w(px(64.))
                    .flex_none()
                    .text_color(rgb(p.muted))
                    .child(tr!("滚动方向", "Direction")),
            )
            .child(self.button(
                "long-axis-vertical",
                tr!(
                    "纵向截图 · 从当前页面重来",
                    "Vertical · restart from the current page"
                ),
                vertical,
                enabled,
                Control::Axis(ScrollAxis::Vertical),
                cx,
            ))
            .child(self.button(
                "long-axis-horizontal",
                tr!(
                    "横向截图 · 从当前页面重来",
                    "Horizontal · restart from the current page"
                ),
                !vertical,
                enabled,
                Control::Axis(ScrollAxis::Horizontal),
                cx,
            ))
            .child(Self::separator(cx))
            .child(self.button(
                "long-auto-forward",
                if vertical {
                    tr!("向下滚动", "Scroll down")
                } else {
                    tr!("向右滚动", "Scroll right")
                },
                self.auto_options.direction == AutoDirection::Forward,
                enabled,
                Control::AutoDirection(AutoDirection::Forward),
                cx,
            ))
            .child(self.button(
                "long-auto-reverse",
                if vertical {
                    tr!("向上滚动", "Scroll up")
                } else {
                    tr!("向左滚动", "Scroll left")
                },
                self.auto_options.direction == AutoDirection::Reverse,
                enabled,
                Control::AutoDirection(AutoDirection::Reverse),
                cx,
            ));
        let speed = div()
            .id("long-auto-adaptive")
            .relative()
            .flex()
            .flex_wrap()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .child(
                div()
                    .min_w(px(64.))
                    .flex_none()
                    .text_color(rgb(p.muted))
                    .child(tr!("自动调速", "Adaptive")),
            )
            .child(toolbar_icons::action(Icon::Speed, p.muted))
            .child(
                div()
                    .w(px(206.))
                    .min_h(px(36.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "从慢开始，按拼接质量自动调整",
                        "Starts slowly; adjusts to stitch quality"
                    )),
            )
            .child(self.probe("long-auto-adaptive"));
        let mut bands = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .child(
                div()
                    .min_w(px(64.))
                    .flex_none()
                    .text_color(rgb(p.muted))
                    .child(tr!("固定栏", "Fixed")),
            );
        for (id, label, hint, mode) in [
            (
                "long-fixed-none",
                tr!("无", "None"),
                tr!("不处理固定栏", "Ignore fixed bands"),
                FixedMode::None,
            ),
            (
                "long-fixed-auto",
                tr!("自动", "Auto"),
                tr!("自动识别固定栏", "Detect fixed bands automatically"),
                FixedMode::Auto,
            ),
            (
                "long-fixed-manual",
                tr!("手动", "Manual"),
                tr!("手动指定固定栏", "Set fixed bands manually"),
                FixedMode::Manual,
            ),
        ] {
            bands = bands.child(self.value_button(
                id,
                (label, hint),
                self.fixed_mode == mode,
                enabled,
                Control::Fixed(mode),
                cx,
            ));
        }
        bands = bands.child(self.button(
            "long-apply-settings",
            tr!(
                "应用固定栏设置并重新采集",
                "Apply fixed bands and capture again"
            ),
            false,
            enabled && self.setup_ready,
            Control::Restart,
            cx,
        ));
        let details = if self.fixed_mode == FixedMode::Manual {
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .items_center()
                .flex_shrink_0()
                .min_h(px(32.))
                .child(
                    div()
                        .min_w(px(54.))
                        .flex_none()
                        .text_color(rgb(p.muted))
                        .child(tr!("固定像素", "Pixels")),
                )
                .child(if vertical {
                    tr!("顶部", "Top")
                } else {
                    tr!("左侧", "Left")
                })
                .child(
                    div()
                        .relative()
                        .w(px(58.))
                        .capture_any_mouse_down(cx.listener(|v, _, w, cx| {
                            v.process_region_input(w, cx);
                            if !v.parameters_enabled {
                                cx.stop_propagation();
                            }
                        }))
                        .child(self.fixed_start.clone())
                        .child(self.probe("long-fixed-start")),
                )
                .child(if vertical {
                    tr!("底部", "Bottom")
                } else {
                    tr!("右侧", "Right")
                })
                .child(
                    div()
                        .relative()
                        .w(px(58.))
                        .capture_any_mouse_down(cx.listener(|v, _, w, cx| {
                            v.process_region_input(w, cx);
                            if !v.parameters_enabled {
                                cx.stop_propagation();
                            }
                        }))
                        .child(self.fixed_end.clone())
                        .child(self.probe("long-fixed-end")),
                )
        } else {
            div()
                .min_h(px(32.))
                .flex()
                .items_center()
                .flex_shrink_0()
                .text_color(rgb(p.muted))
                .child(tr!(
                    "修改固定栏后，点击右侧重来图标应用",
                    "Use the restart icon to apply fixed bands"
                ))
        };
        let mut bar = div()
            .id("long-secondary-toolbar")
            .flex()
            .flex_col()
            .gap(px(6.))
            .flex_shrink_0()
            .p(px(8.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.background));
        if above {
            bar = bar
                .child(details)
                .child(bands)
                .child(speed)
                .child(direction);
        } else {
            bar = bar
                .child(direction)
                .child(speed)
                .child(bands)
                .child(details);
        }
        bar.into_any_element()
    }
}
struct LongHint(SharedString);
impl Render for LongHint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::capture_toolbar::ui_kit::tip_body(crate::theme::palette(cx), &self.0)
    }
}
impl Render for LongCaptureView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        self.ui_positions.borrow_mut().clear();
        let p = crate::theme::palette(cx);
        let over_limit = self.meta.result_available
            && long_output::limit_message(self.meta.width, self.meta.height).is_some();
        let info = if self.meta.result_available {
            format!("{} × {}", self.meta.width, self.meta.height)
        } else {
            format!("{} × {}", self.region.width, self.region.height)
        };
        let failed = self.meta.phase == LongPhase::Failed
            || self.meta.auto.phase == crate::long_auto::AutoPhase::Blocked;
        let warning = over_limit
            || matches!(
                self.meta.last_result,
                crate::stitch::UpdateKind::Rejected | crate::stitch::UpdateKind::Limit
            )
            || matches!(
                self.meta.auto.phase,
                crate::long_auto::AutoPhase::Recovering
                    | crate::long_auto::AutoPhase::NoMotion
                    | crate::long_auto::AutoPhase::Unstable
            );
        let status = if self.session.is_none() && !self.status.is_empty() {
            self.status.clone()
        } else if self.meta.phase == LongPhase::Failed && !self.meta.status.is_empty() {
            self.meta.status.clone()
        } else if self.meta.auto.phase == crate::long_auto::AutoPhase::Blocked
            && !self.meta.auto.status.is_empty()
        {
            self.meta.auto.status.clone()
        } else if !self.status.is_empty() {
            self.status.clone()
        } else if let Some(error) = self.key_owner_error.as_ref() {
            error.clone()
        } else if over_limit {
            long_output::limit_message(self.meta.width, self.meta.height).unwrap_or_default()
        } else if !self.meta.auto.status.is_empty() {
            self.meta.auto.status.clone()
        } else if self.meta.phase == LongPhase::Running
            && self.meta.last_result == crate::stitch::UpdateKind::Unchanged
        {
            self.growth_note
                .as_ref()
                .filter(|(when, _)| when.elapsed() < Duration::from_millis(900))
                .map_or_else(|| self.meta.status.clone(), |(_, text)| text.clone())
        } else {
            self.meta.status.clone()
        };
        let phase = match self.meta.phase {
            LongPhase::Starting => tr!("准备中", "Preparing"),
            LongPhase::Running if self.meta.auto.active => tr!("自动滚动", "Auto-scroll"),
            LongPhase::Running => tr!("滚动采集", "Scrolling capture"),
            LongPhase::Pausing => tr!("正在暂停", "Pausing"),
            LongPhase::Paused => tr!("已暂停", "Paused"),
            LongPhase::Stopping => tr!("正在停止", "Stopping"),
            LongPhase::Stopped => tr!("已停止", "Stopped"),
            LongPhase::Failed => tr!("采集出错", "Capture error"),
        };
        let state_color = if failed || self.key_owner_error.is_some() {
            p.danger
        } else if warning {
            if crate::theme::is_light(cx) {
                0x997126
            } else {
                0xddbb7c
            }
        } else if self.meta.phase == LongPhase::Running {
            p.accent
        } else {
            p.muted
        };
        let message = if status.is_empty() {
            phase.to_owned()
        } else {
            status
        };
        let message_color = if failed || warning || self.key_owner_error.is_some() {
            state_color
        } else {
            p.muted
        };
        let above = crate::pin_native::rect(hwnd(window)).is_ok_and(|r| r.y < self.region.top);
        let primary = self.primary_toolbar(cx);
        let secondary = self.settings_open.then(|| self.configuration(above, cx));
        let information = div()
            .flex()
            .h(px(24.))
            .px(px(10.))
            .gap(px(6.))
            .items_center()
            .flex_shrink_0()
            .border_t_1()
            .border_color(rgb(p.border))
            .text_size(px(11.))
            .line_height(px(16.))
            .child(
                div()
                    .size(px(5.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(state_color)),
            )
            .child(
                div()
                    .id("long-status")
                    .min_w_0()
                    .flex_1()
                    .text_color(rgb(message_color))
                    .truncate()
                    .child(message.clone())
                    .tooltip(move |_, cx| cx.new(|_| LongHint(message.clone().into())).into()),
            )
            .when(!failed && !warning && self.pending_editor.is_none(), |d| {
                d.child(div().flex_shrink_0().text_color(rgb(p.muted)).child(info))
            });
        let chrome = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(rgb(p.background))
            .rounded(px(9.))
            .border_1()
            .border_color(rgb(p.border))
            .child(primary)
            .child(information);
        let mut panel = div()
            .id("long-capture-panel")
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(8.))
            .font_family("Microsoft YaHei UI")
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::key_down));
        // 主操作与提示行保持一张卡片；设置沿选区外侧展开，不插进两行之间。
        if above {
            if let Some(secondary) = secondary {
                panel = panel.child(secondary);
            }
            panel = panel.child(chrome);
        } else {
            panel = panel.child(chrome);
            if let Some(secondary) = secondary {
                panel = panel.child(secondary);
            }
        }
        panel
    }
}

fn panel_placement(region: ScreenRect, expanded: bool) -> Placement {
    let work = crate::pin_native::work_area((region.left, region.top));
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: region.left,
                y: region.top,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let (mut dpi_x, mut dpi_y) = (96, 96);
    let scale = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .is_ok()
        && dpi_x > 0
    {
        dpi_x as f32 / 96.
    } else {
        1.
    };
    let width = ((PANEL_WIDTH * scale).round() as u32).min(work.width);
    // 窄工作区允许图标换行，高度按实际可用 DIP 增长。
    let usable = (width as f32 / scale - 2. * PANEL_PADDING - 2.).max(32.);
    let main_rows = primary_rows(usable);
    let parameters = if expanded {
        configuration_height(width as f32 / scale)
    } else {
        0.
    };
    let height = (((68. + (BUTTON_SIZE + PRIMARY_GAP) * (main_rows - 1.) + parameters) * scale)
        .round() as u32)
        .min(work.height);
    let right = region.left as i64 + region.width as i64;
    let bottom = region.top as i64 + region.height as i64;
    let wx2 = work.x as i64 + work.width as i64;
    let wy2 = work.y as i64 + work.height as i64;
    let gap = (8. * scale).round() as i64;
    let x = (right - width as i64).clamp(work.x as i64, wx2 - width as i64);
    let y = if wy2 - bottom >= height as i64 + gap {
        bottom + gap
    } else if region.top as i64 - work.y as i64 >= height as i64 + gap {
        region.top as i64 - height as i64 - gap
    } else {
        wy2 - height as i64
    };
    Placement {
        x: x as i32,
        y: y.clamp(work.y as i64, wy2 - height as i64) as i32,
        width,
        height,
    }
}
fn primary_rows(usable: f32) -> f32 {
    // 纯图标主栏；分隔线包含两侧 4 DIP 外边距。
    let widths = [
        14.,
        BUTTON_SIZE,
        BUTTON_SIZE,
        BUTTON_SIZE,
        BUTTON_SIZE,
        BUTTON_SIZE,
        9.,
        BUTTON_SIZE,
        BUTTON_SIZE,
        BUTTON_SIZE,
        9.,
        BUTTON_SIZE,
        BUTTON_SIZE,
    ];
    wrapped_rows(usable, &widths, PRIMARY_GAP)
}
fn configuration_height(width: f32) -> f32 {
    let inner = (width - 18.).max(1.);
    // 参数卡内边距、行距与外侧间距；逐行计算，窄屏只增加实际换行的高度。
    44. + wrapped_height(
        inner,
        &[
            (64., 18.),
            (30., 30.),
            (30., 30.),
            (9., 18.),
            (30., 30.),
            (30., 30.),
        ],
        4.,
    ) + wrapped_height(inner, &[(64., 18.), (20., 20.), (206., 36.)], 4.)
        + wrapped_height(
            inner,
            &[(64., 18.), (58., 28.), (58., 28.), (58., 28.), (30., 30.)],
            4.,
        )
        + wrapped_height(
            inner,
            &[(54., 18.), (24., 18.), (58., 36.), (44., 18.), (58., 36.)],
            4.,
        )
}
fn wrapped_height(usable: f32, items: &[(f32, f32)], gap: f32) -> f32 {
    let (mut total, mut line_width, mut line_height) = (0., 0., 0_f32);
    for &(width, height) in items {
        let spacing = if line_width == 0. { 0. } else { gap };
        if line_width > 0. && line_width + spacing + width > usable {
            total += line_height + gap;
            line_width = width;
            line_height = height;
        } else {
            line_width += spacing + width;
            line_height = line_height.max(height);
        }
    }
    total + line_height
}
fn wrapped_rows(usable: f32, widths: &[f32], gap: f32) -> f32 {
    let mut rows = 1.;
    let mut line = 0.;
    for &width in widths {
        let spacing = if line == 0. { 0. } else { gap };
        if line > 0. && line + spacing + width > usable {
            rows += 1.;
            line = width;
        } else {
            line += spacing + width;
        }
    }
    rows
}
fn virtual_bounds() -> ScreenRect {
    unsafe {
        ScreenRect {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}
fn native_setup(h: HWND, placement: Placement) -> std::result::Result<(), String> {
    unsafe {
        if !IsWindow(Some(h)).as_bool() {
            return Err("长截图面板已关闭".into());
        }
        crate::remote_display::exclude_from_capture(h)
            .map_err(|e| format!("无法排除长截图面板，已停止采集：{e}"))?;
        SetWindowLongPtrW(h, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        SetWindowLongPtrW(
            h,
            GWL_EXSTYLE,
            GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_TOOLWINDOW.0 as isize,
        );
        if GetWindowLongPtrW(h, GWL_STYLE) & WS_POPUP.0 as isize == 0 {
            return Err("无法设置长截图面板样式".into());
        }
        SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            placement.x,
            placement.y,
            placement.width as i32,
            placement.height as i32,
            SWP_SHOWWINDOW | SWP_FRAMECHANGED | SWP_NOACTIVATE,
        )
        .map_err(|e| format!("无法放置长截图面板：{e}"))?;
    }
    Ok(())
}
