//! 自有固定噪声窗口：实际工具条/八柄 Win32 事件与 ROI DXGI，全像素 oracle。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    gpui_capture::{hwnd, CaptureLock},
    gpui_smoke::{isolate_input, Input, SMOKE_MOVE},
    long_capture_runtime::{FrameGate, LongOptions, LongPhase, LongSnapshot, RegionGuardFactory},
    long_capture_smoke::{crop_image, scene_for_pattern, Fixture, FixturePattern},
    long_capture_view::LongCaptureView,
    long_document,
    long_pipeline_smoke::{ensure, MatchLatch, ReleaseLatch},
    process_memory::{cpu_memory, gpu_memory},
    stitch::{ScrollAxis, StitchOptions},
    Result,
};
use gpui::{
    div, App, AppContext, Application, AsyncApp, Context, Render, Window, WindowHandle,
    WindowOptions,
};
use std::{
    cell::RefCell,
    collections::HashSet,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Gdi::{ClientToScreen, ScreenToClient},
        System::Threading::CreateMutexW,
        UI::{
            Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
            WindowsAndMessaging::*,
        },
    },
};
const WAIT: Duration = Duration::from_secs(20);
const FIXTURE_WIDTH: u32 = 960;
const FIXTURE_HEIGHT: u32 = 640;
struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
struct Report {
    root: PathBuf,
    requested: Vec<String>,
    groups: Vec<serde_json::Value>,
    stages: Vec<serde_json::Value>,
    windows: Vec<usize>,
    active_mode: Option<String>,
}
impl Report {
    fn save(&self, complete: bool, error: Option<&str>) -> Result<()> {
        let destroyed = self
            .windows
            .iter()
            .all(|h| !unsafe { IsWindow(Some(HWND(*h as *mut _))) }.as_bool());
        std::fs::write(
            self.root.join("report.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "suite":"long-region","pid":std::process::id(),"ok":complete&&error.is_none(),"error":error,
                "requested_groups":self.requested,"groups":self.groups,"stages":self.stages,
                "active_mode":self.active_mode,
                "real_dxgi_capture":true,"full_desktop_freeze_used":false,"clipboard_untouched":true,"global_input_sent":false,
                "capture_threads_after_close":crate::long_capture_pipeline::active_capture_threads(),
                "consumer_sessions_after_close":crate::long_capture_runtime::active_workers(),"owned_windows_destroyed":destroyed,
                "limitations":["自有固定960×640噪声窗口、当前DPI；负原点/混合DPI由几何核心另验",
                    "实际向自有GPUI/八柄HWND投递原生消息，不证明硬件鼠标/系统前台权限",
                    "DXGI仅CPU读回自有ROI；归属检查到Copy仍有普通Win32竞态",
                    "原生上限验证受实际虚拟桌面限制；16384边长/32M预算极值由核心另验",
                    "100ms外部内存采样不是连续精确峰值，有限回合不证明长期无泄漏"],
                "preview_evidence":"实际View/Preview代数、来源尺寸、viewport及placement/native RECT同步；不含预览GPU像素读回",
            }))?,
        )?;
        Ok(())
    }
    fn record(&mut self, stage: &str, detail: serde_json::Value) -> Result<()> {
        self.stages.push(serde_json::json!({"stage":stage,"cpu":cpu_memory()?,"gpu":gpu_memory(),"detail":detail}));
        self.save(false, None)
    }
    fn pass(&mut self, name: &str, cases: Vec<serde_json::Value>) -> Result<()> {
        self.groups
            .push(serde_json::json!({"name":name,"ok":true,"cases":cases}));
        self.save(false, None)
    }
    fn start(&mut self, mode: &str) -> Result<()> {
        self.active_mode = Some(mode.into());
        self.record("subcase-start", serde_json::json!({"mode":mode}))
    }
}
struct State {
    meta: LongSnapshot,
    adjusting: bool,
    probe: serde_json::Value,
    preview: Option<serde_json::Value>,
}
fn meta_json(state: &State) -> serde_json::Value {
    serde_json::json!({"phase":format!("{:?}",state.meta.phase),"region":rect_json(state.meta.region),
        "pending_region":state.meta.pending_region.map(rect_json),"adjusting":state.adjusting,
        "busy":state.meta.busy,"generation":state.meta.generation,"requested_generation":state.meta.requested_generation,
        "width":state.meta.width,"height":state.meta.height,"result_available":state.meta.result_available,
        "auto_active":state.meta.auto.active,"status":state.meta.status,"probe":state.probe,"preview_ack":state.preview})
}
fn rect_json(r: ScreenRect) -> serde_json::Value {
    serde_json::json!({"left":r.left,"top":r.top,"width":r.width,"height":r.height})
}
fn image_rect_json(r: Rect) -> serde_json::Value {
    serde_json::json!({"x":r.x,"y":r.y,"width":r.width,"height":r.height})
}
fn native_rect(h: HWND) -> Result<ScreenRect> {
    own(h)?;
    let mut r = RECT::default();
    unsafe {
        GetWindowRect(h, &mut r)?;
    }
    Ok(ScreenRect {
        left: r.left,
        top: r.top,
        width: (r.right - r.left) as u32,
        height: (r.bottom - r.top) as u32,
    })
}
fn ui_context(
    panel: WindowHandle<LongCaptureView>,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let (mut detail, preview) = panel.update(cx, |v,w,_| -> Result<_> {
        let h = hwnd(w);
        let displayed = v.displayed_snapshot();
        Ok((serde_json::json!({"panel_hwnd":h.0 as usize,"panel_rect":rect_json(native_rect(h)?),
            "scale":w.scale_factor(),"mouse_position":[f32::from(w.mouse_position().x),f32::from(w.mouse_position().y)],
            "button_positions":v.ui_positions.borrow().clone(),"view_region":rect_json(v.region),
            "displayed":{"generation":displayed.generation,"requested_generation":displayed.requested_generation,
                "result_available":displayed.result_available,"region":rect_json(displayed.region),
                "width":displayed.width,"height":displayed.height,"viewport":image_rect_json(displayed.viewport)} }),v.preview_window))
    }).map_err(|e|e.to_string())??;
    if let Some(preview) = preview {
        detail["preview"] = preview.update(cx, |v,w,_| -> Result<_> {
            let h=hwnd(w);
            Ok(serde_json::json!({"hwnd":h.0 as usize,"ready":v.ready,"error":v.error,
                "native_visible":unsafe {IsWindowVisible(h)}.as_bool(),
                "native_rect":rect_json(native_rect(h)?),"placement":{"left":v.placement.x,"top":v.placement.y,
                    "width":v.placement.width,"height":v.placement.height},
                "displayed":v.displayed.as_ref().map(|f|serde_json::json!({"generation":f.generation,"revision":f.revision,
                    "width":f.width,"height":f.height,"viewport":image_rect_json(f.viewport),
                    "source_rect":image_rect_json(f.source_rect),"image_width":f.image.width,"image_height":f.image.height}))}))
        }).map_err(|e|e.to_string())??;
    }
    Ok(detail)
}
fn stage(
    report: &mut Report,
    mode: &str,
    label: &str,
    panel: WindowHandle<LongCaptureView>,
    cx: &mut AsyncApp,
) -> Result<()> {
    report.record(label,serde_json::json!({"mode":mode,"state":meta_json(&read(panel,cx)?),"ui":ui_context(panel,cx)?}))
}
async fn next_frame(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<()> {
    let done = Arc::new(AtomicBool::new(false));
    let marker = done.clone();
    panel
        .update(cx, |_, w, _| {
            w.on_next_frame(move |_, _| marker.store(true, Ordering::Release));
            w.refresh();
        })
        .map_err(|e| e.to_string())?;
    let end = Instant::now() + WAIT;
    while !done.load(Ordering::Acquire) {
        ensure(Instant::now() < end, "实际操作条绘制帧未完成")?;
        step(cx).await;
    }
    Ok(())
}
async fn displayed_ready(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let ready = panel
            .update(cx, |v, _, _| {
                let actual = v.snapshot();
                let shown = v.displayed_snapshot();
                shown.result_available
                    && shown.region == actual.region
                    && shown.generation == actual.requested_generation
                    && shown.generation == shown.requested_generation
                    && shown.width == actual.width
                    && shown.height == actual.height
            })
            .map_err(|e| e.to_string())?;
        if ready {
            break;
        }
        ensure(
            Instant::now() < end,
            &format!("初建操作条View缓存尚未同步：{}", ui_context(panel, cx)?),
        )?;
        step(cx).await;
    }
    next_frame(panel, cx).await
}
async fn hover_frame(
    panel: WindowHandle<LongCaptureView>,
    input: Input,
    p: (f32, f32),
    cx: &mut AsyncApp,
) -> Result<()> {
    let physical = (
        (p.0 * input.scale).round() as u32,
        (p.1 * input.scale).round() as u32,
    );
    input.hover(physical)?;
    let end = Instant::now() + WAIT;
    loop {
        let delivered = panel
            .update(cx, |_, w, _| {
                let at = w.mouse_position();
                (f32::from(at.x) - physical.0 as f32 / input.scale).abs() < 1.
                    && (f32::from(at.y) - physical.1 as f32 / input.scale).abs() < 1.
            })
            .map_err(|e| e.to_string())?;
        if delivered {
            break;
        }
        ensure(Instant::now() < end, "实际按钮正常hover未交付GPUI")?;
        step(cx).await;
    }
    next_frame(panel, cx).await
}
async fn preview_ack(
    panel: WindowHandle<LongCaptureView>,
    expected: &LongSnapshot,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let end = Instant::now() + WAIT;
    loop {
        let detail = ui_context(panel, cx)?;
        let displayed = &detail["displayed"];
        let preview = &detail["preview"];
        let frame = &preview["displayed"];
        if detail["view_region"] == rect_json(expected.region)
            && displayed["generation"] == expected.generation
            && displayed["region"] == rect_json(expected.region)
            && displayed["width"] == expected.width
            && displayed["height"] == expected.height
            && displayed["viewport"] == image_rect_json(expected.viewport)
            && preview["ready"] == true
            && preview["error"].is_null()
            && preview["native_visible"] == true
            && frame["generation"] == expected.generation
            && frame["width"] == expected.width
            && frame["height"] == expected.height
            && frame["viewport"] == image_rect_json(expected.viewport)
            && preview["placement"] == preview["native_rect"]
        {
            return Ok(
                serde_json::json!({"metadata_and_native_placement_verified":true,"gpu_pixels_read_back":false,"ui":detail}),
            );
        }
        ensure(
            Instant::now() < end,
            &format!(
                "已提交输出的预览/窗口同步超时：expected={}，ui={detail}",
                serde_json::json!({"generation":expected.generation,"region":rect_json(expected.region),"width":expected.width,
                "height":expected.height,"viewport":image_rect_json(expected.viewport)})
            ),
        )?;
        step(cx).await;
    }
}
fn stable(state: &State) -> bool {
    !state.meta.busy
        && !state.meta.capture_in_flight
        && state.meta.generation == state.meta.requested_generation
        && state.meta.pending_region.is_none()
        && !state.adjusting
        && state.probe["queue_depth"] == 0
        && state.probe["reserved_raw_bytes"] == 0
        && state.probe["producer_quiesced"] == true
        && state.probe["consumer_in_flight"] == false
}
async fn step(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(5))
        .await;
}
fn read(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<State> {
    panel
        .update(cx, |v, _, _| -> Result<_> {
            Ok(State {
                meta: v.snapshot(),
                adjusting: v.region_adjusting(),
                preview: None,
                probe: serde_json::to_value(
                    v.session
                        .as_ref()
                        .ok_or("实际面板尚未启动采集器")?
                        .pipeline_probe(),
                )?,
            })
        })
        .map_err(|e| e.to_string())?
}
async fn wait(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    allow_failed: bool,
    predicate: impl Fn(&State) -> bool,
    cx: &mut AsyncApp,
) -> Result<State> {
    let end = Instant::now() + WAIT;
    loop {
        let state = read(panel, cx)?;
        if predicate(&state) {
            return Ok(state);
        }
        ensure(
            allow_failed || state.meta.phase != LongPhase::Failed,
            &format!("{label}：{}", state.meta.status),
        )?;
        if Instant::now() >= end {
            return Err(format!(
                "{label}超时：{}；实际UI={}",
                meta_json(&state),
                ui_context(panel, cx)?
            )
            .into());
        }
        step(cx).await;
    }
}
fn own(h: HWND) -> Result<()> {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    ensure(
        pid == std::process::id() && unsafe { IsWindow(Some(h)) }.as_bool(),
        "原生输入HWND不属于自有进程",
    )
}
fn packed(p: (i32, i32)) -> Result<LPARAM> {
    let x = i16::try_from(p.0)?;
    let y = i16::try_from(p.1)?;
    Ok(LPARAM(
        ((x as u16 as u32) | ((y as u16 as u32) << 16)) as i32 as isize,
    ))
}
fn local(h: HWND, p: (i32, i32)) -> Result<(i32, i32)> {
    own(h)?;
    let mut point = POINT { x: p.0, y: p.1 };
    unsafe {
        ScreenToClient(h, &mut point).ok()?;
    }
    Ok((point.x, point.y))
}
fn mouse(h: HWND, message: u32, point: (i32, i32)) -> Result<()> {
    let data = packed(local(h, point)?)?;
    let buttons = if message == WM_LBUTTONUP { 0 } else { 1 };
    let tag = if message == WM_MOUSEMOVE {
        SMOKE_MOVE
    } else {
        0
    };
    unsafe {
        PostMessageW(Some(h), message, WPARAM(buttons | tag), data)?;
    }
    Ok(())
}
async fn button(panel: WindowHandle<LongCaptureView>, id: &str, cx: &mut AsyncApp) -> Result<()> {
    // runtime 的确认可能早于下一次绘制；只对正常呈现且已刷新的一帧发出一次点击。
    displayed_ready(panel, cx).await?;
    let (input, p) = button_location(panel, id, cx).await?;
    hover_frame(panel, input, p, cx).await?;
    own(HWND(input.handle as *mut _))?;
    input.click(p);
    Ok(())
}
async fn button_location(
    panel: WindowHandle<LongCaptureView>,
    id: &str,
    cx: &mut AsyncApp,
) -> Result<(Input, (f32, f32))> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(p) = panel
            .update(cx, |v, w, _| {
                v.ui_positions.borrow().get(id).copied().map(|p| {
                    (
                        Input {
                            handle: hwnd(w).0 as usize,
                            scale: w.scale_factor(),
                        },
                        p,
                    )
                })
            })
            .map_err(|e| e.to_string())?
        {
            return Ok(p);
        }
        ensure(Instant::now() < end, &format!("没有实际按钮{id}"))?;
        step(cx).await;
    }
}
async fn begin_move(
    panel: WindowHandle<LongCaptureView>,
    cx: &mut AsyncApp,
) -> Result<(HWND, (i32, i32))> {
    displayed_ready(panel, cx).await?;
    let (input, p) = button_location(panel, "long-move", cx).await?;
    hover_frame(panel, input, p, cx).await?;
    let h = HWND(input.handle as *mut _);
    own(h)?;
    let mut pt = POINT {
        x: (p.0 * input.scale).round() as i32,
        y: (p.1 * input.scale).round() as i32,
    };
    unsafe {
        ClientToScreen(h, &mut pt).ok()?;
    }
    let p = (pt.x, pt.y);
    mouse(h, WM_LBUTTONDOWN, p)?;
    wait(panel, "实际移动图标Down", false, |s| s.adjusting, cx).await?;
    wait(
        panel,
        "拖动前真实暂停ACK",
        false,
        |s| {
            s.meta.phase == LongPhase::Paused
                && !s.meta.busy
                && !s.meta.capture_in_flight
                && s.meta.generation == s.meta.requested_generation
                && s.probe["producer_quiesced"] == true
                && s.probe["consumer_in_flight"] == false
                && s.probe["queue_depth"] == 0
                && s.probe["reserved_raw_bytes"] == 0
        },
        cx,
    )
    .await?;
    Ok((h, p))
}
async fn move_gesture(
    panel: WindowHandle<LongCaptureView>,
    delta: (i32, i32),
    draft: ScreenRect,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (h, start) = begin_move(panel, cx).await?;
    let next = (start.0 + delta.0, start.1 + delta.1);
    mouse(h, WM_MOUSEMOVE, next)?;
    wait_border(panel, draft, cx).await?;
    mouse(h, WM_LBUTTONUP, next)
}
fn border_rect(panel: WindowHandle<LongCaptureView>, cx: &mut AsyncApp) -> Result<ScreenRect> {
    panel
        .update(cx, |v, _, _| -> Result<_> {
            let handles = v.border.as_ref().ok_or("实际边框尚未建立")?.handles();
            let mut left = i32::MAX;
            let mut top = i32::MAX;
            let mut right = i32::MIN;
            let mut bottom = i32::MIN;
            for h in handles {
                own(h)?;
                let mut r = RECT::default();
                unsafe {
                    GetWindowRect(h, &mut r)?;
                }
                left = left.min(r.left);
                top = top.min(r.top);
                right = right.max(r.right);
                bottom = bottom.max(r.bottom);
            }
            Ok(ScreenRect {
                left,
                top,
                width: u32::try_from(i64::from(right) - i64::from(left))?,
                height: u32::try_from(i64::from(bottom) - i64::from(top))?,
            })
        })
        .map_err(|e| e.to_string())?
}
async fn wait_border(
    panel: WindowHandle<LongCaptureView>,
    expected: ScreenRect,
    cx: &mut AsyncApp,
) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let actual = border_rect(panel, cx)?;
        if actual == expected {
            return Ok(());
        }
        ensure(
            Instant::now() < end,
            &format!("真实边框未跟随草稿：actual={actual:?} expected={expected:?}"),
        )?;
        step(cx).await;
    }
}
async fn resize_gesture(
    panel: WindowHandle<LongCaptureView>,
    kind: &str,
    delta: (i32, i32),
    draft: ScreenRect,
    cx: &mut AsyncApp,
) -> Result<()> {
    let end = Instant::now() + WAIT;
    let (h, r) = loop {
        let handles = panel
            .update(cx, |v, _, _| v.border.as_ref().map(|b| b.resize_handles()))
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        if let Some((_, h, r)) = handles
            .into_iter()
            .find(|(k, h, _)| format!("{k:?}") == kind && unsafe { IsWindowVisible(*h) }.as_bool())
        {
            break (h, r);
        }
        ensure(Instant::now() < end, &format!("暂停八柄没有真实{kind}"))?;
        step(cx).await;
    };
    own(h)?;
    isolate_input(h)?;
    let activation = unsafe {
        ensure(
            GetWindowLongPtrW(h, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize == 0,
            "暂停八柄仍然禁止标准鼠标激活",
        )?;
        SendMessageW(
            h,
            WM_MOUSEACTIVATE,
            Some(WPARAM(h.0 as usize)),
            Some(LPARAM(((WM_LBUTTONDOWN << 16) | HTCLIENT) as isize)),
        )
    };
    ensure(
        activation.0 == MA_ACTIVATE as isize,
        "八柄WM_MOUSEACTIVATE未允许标准激活",
    )?;
    let start = (r.left + (r.width / 2) as i32, r.top + (r.height / 2) as i32);
    mouse(h, WM_LBUTTONDOWN, start)?;
    wait(panel, "真实柄Down", false, |s| s.adjusting, cx).await?;
    let next = (start.0 + delta.0, start.1 + delta.1);
    mouse(h, WM_MOUSEMOVE, next)?;
    wait_border(panel, draft, cx).await?;
    mouse(h, WM_LBUTTONUP, next)
}
async fn committed(case: &Case, expected: ScreenRect, cx: &mut AsyncApp) -> Result<State> {
    wait(
        case.panel,
        "真实候选提交",
        false,
        |s| {
            s.meta.region == expected
                && s.meta.pending_region.is_none()
                && !s.adjusting
                && s.meta.generation == s.meta.requested_generation
        },
        cx,
    )
    .await?;
    pause(case, cx).await
}
fn options(axis: ScrollAxis) -> LongOptions {
    LongOptions {
        stitch: StitchOptions {
            axis,
            detect_fixed_borders: false,
            fixed_start: Some(0),
            fixed_end: Some(0),
        },
        poll_interval: Duration::from_millis(100),
    }
}
struct Case {
    fixture: Fixture,
    panel: WindowHandle<LongCaptureView>,
    start: ScreenRect,
    requests: Arc<Mutex<Vec<ScreenRect>>>,
    fail: Arc<AtomicBool>,
    latch: Arc<MatchLatch>,
    _release: ReleaseLatch,
    weaks: Vec<Weak<Image>>,
}
async fn open(
    fixture_rect: ScreenRect,
    axis: ScrollAxis,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<Case> {
    let fixture = Fixture::new_owned(fixture_rect, axis, (0, 0), FixturePattern::Noise)?;
    let _ = fixture.capture_gate_with_own_overlays()?;
    let factory = fixture.region_gate_factory()?;
    let start = ScreenRect {
        left: fixture_rect.left + 80,
        top: fixture_rect.top + 64,
        width: 512,
        height: 320,
    };
    let initial_gate = fixture.capture_gate_for_region(start)?;
    initial_gate()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let audit = requests.clone();
    let fail = Arc::new(AtomicBool::new(false));
    let fault = fail.clone();
    let factory: RegionGuardFactory = Arc::new(move |region| {
        audit.lock().unwrap_or_else(|e| e.into_inner()).push(region);
        let gate = factory(region)?;
        let fault = fault.clone();
        let checks = AtomicUsize::new(0);
        // 第一次调用是 change_region 的 UI preflight；随后 producer 的真实门才注入失败。
        let wrapped: FrameGate = Arc::new(move || {
            gate()?;
            let checked = checks.fetch_add(1, Ordering::AcqRel);
            ensure(
                !(fault.load(Ordering::Acquire) && region != start && checked > 0),
                "QA拒绝已证明归属的新ROI采集",
            )
        });
        Ok(wrapped)
    });
    let latch = Arc::new(MatchLatch::default());
    let hook = Some(latch.hook());
    let panel = cx
        .update(|cx| LongCaptureView::open_region_guarded(start, options(axis), factory, hook, cx))
        .map_err(|e| e.to_string())??;
    report.windows.push(fixture.native());
    let panel_native = panel
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    report.windows.push(panel_native.0 as usize);
    isolate_input(panel_native)?;
    let end = Instant::now() + WAIT;
    while !panel
        .update(cx, |v, _, _| v.session.is_some())
        .map_err(|e| e.to_string())?
    {
        ensure(Instant::now() < end, "真实面板初始化超时")?;
        step(cx).await;
    }
    wait(
        panel,
        "首个真实ROI",
        false,
        |s| s.meta.result_available && s.meta.width == start.width && s.meta.height == start.height,
        cx,
    )
    .await?;
    displayed_ready(panel, cx).await?;
    let (border_handles, preview) = panel
        .update(cx, |v, _, _| {
            let mut handles = vec![];
            if let Some(b) = &v.border {
                handles.extend(b.handles().into_iter().map(|h| h.0 as usize));
                handles.extend(b.resize_handles().into_iter().map(|(_, h, _)| h.0 as usize));
            }
            (handles, v.preview_window)
        })
        .map_err(|e| e.to_string())?;
    report.windows.extend(border_handles);
    if let Some(preview) = preview {
        report.windows.push(
            preview
                .update(cx, |_, w, _| hwnd(w).0 as usize)
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(Case {
        fixture,
        panel,
        start,
        requests,
        fail,
        _release: ReleaseLatch(latch.clone()),
        latch,
        weaks: vec![],
    })
}
async fn pause(case: &Case, cx: &mut AsyncApp) -> Result<State> {
    if !matches!(
        read(case.panel, cx)?.meta.phase,
        LongPhase::Paused | LongPhase::Failed
    ) {
        button(case.panel, "long-pause", cx).await?;
    }
    let mut state = wait(
        case.panel,
        "真实暂停与资源ACK",
        true,
        |s| stable(s) && matches!(s.meta.phase, LongPhase::Paused | LongPhase::Failed),
        cx,
    )
    .await?;
    state.preview = Some(preview_ack(case.panel, &state.meta, cx).await?);
    Ok(state)
}
fn oracle(
    case: &mut Case,
    source_rect: ScreenRect,
    world_origin: (i32, i32),
    cx: &mut AsyncApp,
) -> Result<usize> {
    let snapshot = case
        .panel
        .update(cx, |v, _, _| v.stitch_snapshot())
        .map_err(|e| e.to_string())?
        .ok_or("实际ACK没有共享图")?;
    ensure(
        snapshot.width == source_rect.width && snapshot.height == source_rect.height,
        "实际输出尺寸与ROI世界裁剪不同",
    )?;
    let actual = long_document::render_region(
        &snapshot,
        Rect::full(snapshot.width, snapshot.height),
        snapshot.width,
        snapshot.height,
    )?;
    let crop = Rect {
        x: (source_rect.left - case.fixture.region.left) as u32,
        y: (source_rect.top - case.fixture.region.top) as u32,
        width: source_rect.width,
        height: source_rect.height,
    };
    let world = scene_for_pattern(
        FixturePattern::Noise,
        FIXTURE_WIDTH,
        FIXTURE_HEIGHT.max(crop.y + crop.height),
        snapshot.axis,
        (0, 0),
        world_origin,
        0,
    );
    let expected = crop_image(&world, crop)?;
    ensure(
        actual.bgra == expected.bgra,
        "实际DXGI拼图与独立固定夹具世界噪声逐像素裁剪不同",
    )?;
    let mut unique = HashSet::new();
    for segment in &snapshot.segments {
        if unique.insert(Arc::as_ptr(&segment.image)) {
            case.weaks.push(Arc::downgrade(&segment.image));
        }
    }
    Ok(actual.bgra.len() / 4)
}
async fn close(mut case: Case, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    case.latch.release();
    button(case.panel, "long-close", cx).await?;
    let end = Instant::now() + WAIT;
    loop {
        if case.panel.update(cx, |_, _, _| ()).is_err() {
            break;
        }
        ensure(Instant::now() < end, "实际Close未销毁面板")?;
        step(cx).await;
    }
    while crate::long_capture_pipeline::active_capture_threads() != 0
        || crate::long_capture_runtime::active_workers() != 0
    {
        ensure(Instant::now() < end, "实际Close采集两端未退出")?;
        step(cx).await;
    }
    ensure(
        case.weaks.iter().all(|w| w.upgrade().is_none()),
        "关闭后可信源像素仍被持有",
    )?;
    let gate = case.fixture.capture_probe_snapshot();
    case.fixture.close()?;
    Ok(
        serde_json::json!({"capture_threads":0,"consumer_sessions":0,"source_weaks_expired":true,"fixture_thread_and_hwnd_destroyed":true,"gate":gate}),
    )
}

async fn grip(case: &Case, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    let (input, p) = button_location(case.panel, "long-grip", cx).await?;
    hover_frame(case.panel, input, p, cx).await?;
    let h = HWND(input.handle as *mut _);
    own(h)?;
    let mut before = RECT::default();
    unsafe {
        GetWindowRect(h, &mut before)?;
    }
    let mut at = POINT {
        x: (p.0 * input.scale).round() as i32,
        y: (p.1 * input.scale).round() as i32,
    };
    unsafe {
        ClientToScreen(h, &mut at).ok()?;
    }
    let start = (at.x, at.y);
    let next = (start.0 + 24, start.1 - 16);
    mouse(h, WM_LBUTTONDOWN, start)?;
    step(cx).await;
    mouse(h, WM_MOUSEMOVE, next)?;
    let end = Instant::now() + WAIT;
    let after = loop {
        let mut now = RECT::default();
        unsafe {
            GetWindowRect(h, &mut now)?;
        }
        if now.left == before.left + 24 && now.top == before.top - 16 {
            break now;
        }
        ensure(Instant::now() < end, "Grip原生窗口没有实际移动")?;
        step(cx).await;
    };
    mouse(h, WM_LBUTTONUP, next)?;
    step(cx).await;
    let state = read(case.panel, cx)?;
    ensure(
        state.meta.region == case.start && !state.adjusting && state.meta.pending_region.is_none(),
        "Grip改变了采集ROI",
    )?;
    Ok(
        serde_json::json!({"mode":"toolbar-grip","actual_native_move":true,"dx":after.left-before.left,"dy":after.top-before.top,"capture_region_unchanged":true}),
    )
}
async fn move_cases(rect: ScreenRect, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut cases = vec![];
    for mode in [
        "paused-xy-reset",
        "running-vertical-continue",
        "running-horizontal-continue",
    ] {
        report.start(mode)?;
        let axis = if mode == "running-horizontal-continue" {
            ScrollAxis::Horizontal
        } else {
            ScrollAxis::Vertical
        };
        let mut case = open(rect, axis, report, cx).await?;
        stage(report, mode, "case-opened", case.panel, cx)?;
        if mode == "paused-xy-reset" {
            pause(&case, cx).await?;
            stage(report, "toolbar-grip", "before-grip", case.panel, cx)?;
            cases.push(grip(&case, cx).await?);
            report.record("subcase-passed", cases.last().cloned().unwrap())?;
            let expected = ScreenRect {
                left: case.start.left + 32,
                top: case.start.top + 32,
                ..case.start
            };
            stage(report, mode, "before-move", case.panel, cx)?;
            move_gesture(case.panel, (32, 32), expected, cx).await?;
            let state = committed(&case, expected, cx).await?;
            let origin = case.fixture.origin();
            let checks = oracle(&mut case, expected, origin, cx)?;
            stage(report, mode, "move-committed-preview-ack", case.panel, cx)?;
            let requests = case
                .requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied()
                .map(rect_json)
                .collect::<Vec<_>>();
            let cleanup = close(case, cx).await?;
            cases.push(serde_json::json!({"mode":mode,"pixel_checks":checks,"state":meta_json(&state),"guard_regions":requests,"cleanup":cleanup}));
            report.record("subcase-passed", cases.last().cloned().unwrap())?;
        } else {
            let expected = match axis {
                ScrollAxis::Vertical => ScreenRect {
                    top: case.start.top + 64,
                    ..case.start
                },
                ScrollAxis::Horizontal => ScreenRect {
                    left: case.start.left + 64,
                    ..case.start
                },
            };
            let delta = match axis {
                ScrollAxis::Vertical => (32, 64),
                ScrollAxis::Horizontal => (64, 32),
            };
            let output = match axis {
                ScrollAxis::Vertical => ScreenRect {
                    height: case.start.height + 64,
                    ..case.start
                },
                ScrollAxis::Horizontal => ScreenRect {
                    width: case.start.width + 64,
                    ..case.start
                },
            };
            let origin = case.fixture.origin();
            stage(report, mode, "before-move", case.panel, cx)?;
            move_gesture(case.panel, delta, expected, cx).await?;
            let state = committed(&case, expected, cx).await?;
            let checks = oracle(&mut case, output, origin, cx)?;
            stage(report, mode, "move-committed-preview-ack", case.panel, cx)?;
            let cleanup = close(case, cx).await?;
            cases.push(serde_json::json!({"mode":mode,"pixel_checks":checks,"output":rect_json(output),"state":meta_json(&state),"cross_axis_unchanged":true,"cleanup":cleanup}));
            report.record("subcase-passed", cases.last().cloned().unwrap())?;
        }
    }
    report.pass("move", cases)
}
fn expanded(r: ScreenRect, kind: &str) -> Result<(ScreenRect, (i32, i32))> {
    let west = matches!(kind, "West" | "NorthWest" | "SouthWest");
    let east = matches!(kind, "East" | "NorthEast" | "SouthEast");
    let north = matches!(kind, "North" | "NorthWest" | "NorthEast");
    let south = matches!(kind, "South" | "SouthWest" | "SouthEast");
    ensure(west || east || north || south, "未识别实际八柄名称")?;
    Ok((
        ScreenRect {
            left: r.left - if west { 8 } else { 0 },
            top: r.top - if north { 8 } else { 0 },
            width: r.width + if west || east { 8 } else { 0 },
            height: r.height + if north || south { 8 } else { 0 },
        },
        (
            if west {
                -8
            } else if east {
                8
            } else {
                0
            },
            if north {
                -8
            } else if south {
                8
            } else {
                0
            },
        ),
    ))
}
async fn resize_cases(rect: ScreenRect, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut cases = vec![];
    report.start("resize-initialize")?;
    let mut case = open(rect, ScrollAxis::Vertical, report, cx).await?;
    pause(&case, cx).await?;
    let mut current = case.start;
    for kind in [
        "North",
        "NorthEast",
        "East",
        "SouthEast",
        "South",
        "SouthWest",
        "West",
        "NorthWest",
    ] {
        report.start(kind)?;
        stage(report, kind, "before-resize", case.panel, cx)?;
        let (expected, delta) = expanded(current, kind)?;
        resize_gesture(case.panel, kind, delta, expected, cx).await?;
        let state = committed(&case, expected, cx).await?;
        let origin = case.fixture.origin();
        let checks = oracle(&mut case, expected, origin, cx)?;
        cases.push(serde_json::json!({"mode":kind,"pixel_checks":checks,"state":meta_json(&state),
            "noactivate_style_absent":true,"mouseactivate_result":"MA_ACTIVATE","hardware_activation_tested":false}));
        report.record("subcase-passed", cases.last().cloned().unwrap())?;
        current = expected;
    }
    let narrow = ScreenRect {
        left: current.left + (current.width - 32) as i32,
        width: 32,
        ..current
    };
    report.start("minimum-cross-32")?;
    stage(report, "minimum-cross-32", "before-resize", case.panel, cx)?;
    resize_gesture(case.panel, "West", (1200, 0), narrow, cx).await?;
    let state = committed(&case, narrow, cx).await?;
    let origin = case.fixture.origin();
    let checks = oracle(&mut case, narrow, origin, cx)?;
    cases.push(serde_json::json!({"mode":"minimum-cross-32","pixel_checks":checks,"region":rect_json(narrow),"state":meta_json(&state)}));
    report.record("subcase-passed", cases.last().cloned().unwrap())?;
    let minimum = ScreenRect {
        top: narrow.top + (narrow.height - 64) as i32,
        height: 64,
        ..narrow
    };
    report.start("minimum-along-64")?;
    stage(report, "minimum-along-64", "before-resize", case.panel, cx)?;
    resize_gesture(case.panel, "North", (0, 1200), minimum, cx).await?;
    let state = committed(&case, minimum, cx).await?;
    let origin = case.fixture.origin();
    let checks = oracle(&mut case, minimum, origin, cx)?;
    cases.push(serde_json::json!({"mode":"minimum-along-64","pixel_checks":checks,"region":rect_json(minimum),"state":meta_json(&state)}));
    report.record("subcase-passed", cases.last().cloned().unwrap())?;
    let before = read(case.panel, cx)?.meta.requested_generation;
    let virtual_right =
        unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let limit = ScreenRect {
        width: (virtual_right as i64 - minimum.left as i64).min(16_384) as u32,
        ..minimum
    };
    ensure(limit.width > minimum.width, "当前桌面不足以验证扩展上限")?;
    report.start("desktop-limit-outside-own-fixture-rejected")?;
    stage(
        report,
        "desktop-limit-outside-own-fixture-rejected",
        "before-resize",
        case.panel,
        cx,
    )?;
    resize_gesture(case.panel, "East", (30_000, 0), limit, cx).await?;
    let state = wait(
        case.panel,
        "超自有ROI边界拒绝",
        true,
        |s| stable(s) && s.meta.region == minimum && !s.meta.status.is_empty(),
        cx,
    )
    .await?;
    ensure(
        state.meta.requested_generation >= before,
        "失败边界操作回退了generation",
    )?;
    let origin = case.fixture.origin();
    let checks = oracle(&mut case, minimum, origin, cx)?;
    let preview = preview_ack(case.panel, &state.meta, cx).await?;
    cases.push(serde_json::json!({"mode":"desktop-limit-outside-own-fixture-rejected","draft":rect_json(limit),"pixel_checks":checks,"old_result_preserved":true,"state":meta_json(&state),"preview_ack":preview,"foreign_pixels_read":false}));
    report.record("subcase-passed", cases.last().cloned().unwrap())?;
    let cleanup = close(case, cx).await?;
    cases.push(serde_json::json!({"mode":"cleanup","detail":cleanup}));
    report.pass("resize", cases)
}
async fn cancel_cases(
    rect: ScreenRect,
    loss: HWND,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut cases = vec![];
    let modes = [
        "escape-drag",
        "escape-running-drag",
        "cancelmode-drag",
        "capture-lost-drag",
        "candidate-gate-failed",
        "copied-candidate-cancel-stale",
        "idle-handle-up-escape-gap",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(
        [
            "North",
            "NorthEast",
            "East",
            "SouthEast",
            "South",
            "SouthWest",
            "West",
            "NorthWest",
        ]
        .into_iter()
        .map(|kind| format!("unconsumed-handle-{kind}-down-up-escape")),
    );
    for mode in modes {
        let mode = mode.as_str();
        report.start(mode)?;
        let unconsumed_handle = mode
            .strip_prefix("unconsumed-handle-")
            .and_then(|s| s.strip_suffix("-down-up-escape"));
        let mut case = open(rect, ScrollAxis::Vertical, report, cx).await?;
        let before = pause(&case, cx).await?;
        let start = case.start;
        let origin = case.fixture.origin();
        oracle(&mut case, start, origin, cx)?;
        if mode == "escape-running-drag" {
            button(case.panel, "long-pause", cx).await?;
            wait(
                case.panel,
                "为运行态取消恢复真实采集",
                false,
                |s| {
                    s.meta.phase == LongPhase::Running
                        && s.meta.generation == s.meta.requested_generation
                },
                cx,
            )
            .await?;
        }
        let candidate = if let Some(kind) = unconsumed_handle {
            expanded(start, kind)?.0
        } else if mode == "idle-handle-up-escape-gap" {
            ScreenRect {
                width: start.width + 24,
                ..start
            }
        } else if mode == "escape-running-drag" {
            ScreenRect {
                top: start.top + 24,
                ..start
            }
        } else {
            ScreenRect {
                left: start.left + 24,
                top: start.top + 24,
                ..start
            }
        };
        if mode == "candidate-gate-failed" {
            case.fail.store(true, Ordering::Release);
        }
        if matches!(
            mode,
            "copied-candidate-cancel-stale" | "idle-handle-up-escape-gap"
        ) || unconsumed_handle.is_some()
        {
            case.latch.arm();
        }
        stage(report, mode, "before-cancel-input", case.panel, cx)?;
        let (h, at) = if mode == "idle-handle-up-escape-gap" || unconsumed_handle.is_some() {
            let wanted = unconsumed_handle.unwrap_or("East");
            let end = Instant::now() + WAIT;
            let (_, h, r) = loop {
                let handles = case
                    .panel
                    .update(cx, |v, _, _| v.border.as_ref().map(|b| b.resize_handles()))
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                if let Some(found) = handles.into_iter().find(|(kind, h, _)| {
                    format!("{kind:?}") == wanted && unsafe { IsWindowVisible(*h) }.as_bool()
                }) {
                    break found;
                }
                ensure(Instant::now() < end, "实际交接缝测试柄尚未显示")?;
                step(cx).await;
            };
            own(h)?;
            isolate_input(h)?;
            let at = (r.left + (r.width / 2) as i32, r.top + (r.height / 2) as i32);
            mouse(h, WM_LBUTTONDOWN, at)?;
            if unconsumed_handle.is_none() {
                wait(case.panel, "交接缝实际柄Down", false, |s| s.adjusting, cx).await?;
            }
            (h, at)
        } else {
            begin_move(case.panel, cx).await?
        };
        let delta = if let Some(kind) = unconsumed_handle {
            expanded(start, kind)?.1
        } else {
            (
                24,
                if mode == "idle-handle-up-escape-gap" {
                    0
                } else {
                    24
                },
            )
        };
        let next = (at.0 + delta.0, at.1 + delta.1);
        mouse(h, WM_MOUSEMOVE, next)?;
        if unconsumed_handle.is_none() {
            wait_border(case.panel, candidate, cx).await?;
        }
        let mut copied_before_gap_cancel = false;
        match mode {
            "escape-drag" | "escape-running-drag" => unsafe {
                PostMessageW(Some(h), WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
                PostMessageW(Some(h), WM_KEYUP, WPARAM(0x1b), LPARAM(1 << 31))?;
            },
            "cancelmode-drag" => unsafe {
                PostMessageW(Some(h), WM_CANCELMODE, WPARAM(0), LPARAM(0))?;
            },
            "capture-lost-drag" => {
                own(loss)?;
                cx.update(|_| unsafe { SetCapture(loss) })
                    .map_err(|e| e.to_string())?;
            }
            "candidate-gate-failed" => {
                mouse(h, WM_LBUTTONUP, next)?;
            }
            m if m == "idle-handle-up-escape-gap" || unconsumed_handle.is_some() => {
                // 新的八柄子例在Down/Move/Up/Esc之间完全不await，不预消费手势队列。
                mouse(h, WM_LBUTTONUP, next)?;
                unsafe {
                    PostMessageW(Some(h), WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
                    PostMessageW(Some(h), WM_KEYDOWN, WPARAM(0x1b), LPARAM(1 << 30))?;
                    PostMessageW(Some(h), WM_KEYUP, WPARAM(0x1b), LPARAM(1 << 31))?;
                }
                next_frame(case.panel, cx).await?;
                wait(
                    case.panel,
                    "柄Up→Esc交接缝已取消",
                    true,
                    |s| {
                        s.meta.region == start
                            && s.meta.pending_region.is_none()
                            && (!s.adjusting
                                || s.meta.requested_generation > before.meta.requested_generation)
                    },
                    cx,
                )
                .await?;
                copied_before_gap_cancel = case.latch.entered();
                case.latch.release();
            }
            _ => {
                mouse(h, WM_LBUTTONUP, next)?;
                let end = Instant::now() + WAIT;
                while !case.latch.entered() {
                    ensure(Instant::now() < end, "真实候选复制后MatchHook未进入")?;
                    step(cx).await;
                }
                let state = read(case.panel, cx)?;
                ensure(
                    state.meta.pending_region == Some(candidate) && !state.meta.auto.active,
                    "已复制候选未保持single/AutoOff",
                )?;
                unsafe {
                    PostMessageW(Some(h), WM_KEYDOWN, WPARAM(0x1b), LPARAM(0))?;
                }
                unsafe {
                    PostMessageW(Some(h), WM_KEYDOWN, WPARAM(0x1b), LPARAM(1 << 30))?;
                    PostMessageW(Some(h), WM_KEYUP, WPARAM(0x1b), LPARAM(1 << 31))?;
                }
                wait(
                    case.panel,
                    "候选取消代数栅栏",
                    true,
                    |s| s.meta.requested_generation > state.meta.requested_generation,
                    cx,
                )
                .await?;
                case.latch.release();
            }
        }
        if mode == "escape-running-drag" {
            wait(
                case.panel,
                "取消运行拖动后实际恢复Running",
                false,
                |s| {
                    s.meta.phase == LongPhase::Running
                        && s.meta.region == start
                        && !s.adjusting
                        && s.meta.pending_region.is_none()
                        && s.meta.generation == s.meta.requested_generation
                },
                cx,
            )
            .await?;
            pause(&case, cx).await?;
        }
        let state = wait(
            case.panel,
            "实际取消/失败ACK",
            true,
            |s| {
                stable(s)
                    && s.meta.region == start
                    && matches!(s.meta.phase, LongPhase::Paused | LongPhase::Failed)
            },
            cx,
        )
        .await?;
        if mode == "capture-lost-drag" {
            cx.update(|_| unsafe {
                if GetCapture() == loss {
                    let _ = ReleaseCapture();
                }
            })
            .map_err(|e| e.to_string())?;
        }
        // 取消后的迟到 Up/Move 仍走同一原生入口，不能复活已取消的手势或旧候选。
        mouse(h, WM_MOUSEMOVE, next)?;
        mouse(h, WM_LBUTTONUP, next)?;
        let fence = state.meta.requested_generation;
        for _ in 0..20 {
            step(cx).await;
        }
        let after = read(case.panel, cx)?;
        ensure(
            stable(&after)
                && after.meta.region == start
                && after.meta.requested_generation == fence,
            "迟到Up/Move复活取消请求",
        )?;
        if matches!(
            mode,
            "candidate-gate-failed" | "copied-candidate-cancel-stale"
        ) {
            ensure(
                fence > before.meta.requested_generation,
                "真实候选失败/取消没有推进事务generation",
            )?;
        }
        let checks = oracle(&mut case, start, origin, cx)?;
        let preview = preview_ack(case.panel, &after.meta, cx).await?;
        stage(
            report,
            mode,
            "cancel-old-result-preview-ack",
            case.panel,
            cx,
        )?;
        let cleanup = close(case, cx).await?;
        cases.push(serde_json::json!({"mode":mode,"pixel_checks":checks,"old_result_preserved":true,"stale_up_ignored":true,
            "held_escape_kept_window":matches!(mode,"copied-candidate-cancel-stale"|"idle-handle-up-escape-gap")||unconsumed_handle.is_some(),
            "idle_handle_escape_bridge":mode=="idle-handle-up-escape-gap"||unconsumed_handle.is_some(),"copied_before_gap_cancel":copied_before_gap_cancel,
            "continuous_handle":unconsumed_handle,"no_wait_between_down_move_up_escape":unconsumed_handle.is_some(),
            "actual_widget_stayed_open":true,"state":meta_json(&after),"preview_ack":preview,"cleanup":cleanup}));
        report.record("subcase-passed", cases.last().cloned().unwrap())?;
    }
    report.pass("cancel-failed-stale", cases)
}
async fn auto_case(rect: ScreenRect, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    report.start("candidate-stops-old-auto-and-new-region-rebinds")?;
    let mut case = open(rect, ScrollAxis::Vertical, report, cx).await?;
    case.fixture.set_limits(
        crate::long_capture_smoke::ORIGIN,
        crate::long_capture_smoke::ORIGIN + crate::long_capture_smoke::STEP,
    )?;
    pause(&case, cx).await?;
    button(case.panel, "long-auto", cx).await?;
    let end = Instant::now() + WAIT;
    while case.fixture.wheel_count() == 0 {
        ensure(Instant::now() < end, "实际Auto按钮未向自有初始目标发送滚轮")?;
        step(cx).await;
    }
    wait(
        case.panel,
        "初始Auto真实位移",
        false,
        |s| s.meta.height == case.start.height + 32 && s.meta.auto.active,
        cx,
    )
    .await?;
    let candidate = ScreenRect {
        top: case.start.top + 64,
        ..case.start
    };
    stage(report, "auto-target", "before-move", case.panel, cx)?;
    let (h, at) = begin_move(case.panel, cx).await?;
    case.latch.arm();
    let next = (at.0 + 32, at.1 + 64);
    mouse(h, WM_MOUSEMOVE, next)?;
    wait_border(case.panel, candidate, cx).await?;
    mouse(h, WM_LBUTTONUP, next)?;
    let end = Instant::now() + WAIT;
    while !case.latch.entered() {
        ensure(Instant::now() < end, "Auto中移动的真实候选未进入MatchHook")?;
        step(cx).await;
    }
    let pending = read(case.panel, cx)?;
    ensure(
        pending.meta.pending_region == Some(candidate) && !pending.meta.auto.active,
        "候选继承旧Auto目标",
    )?;
    let wheels = case.fixture.wheel_count();
    let hold = Instant::now() + Duration::from_millis(650);
    while Instant::now() < hold {
        ensure(
            case.fixture.wheel_count() == wheels,
            "Single候选等待期间继续使用旧Auto目标发滚轮",
        )?;
        step(cx).await;
    }
    case.latch.release();
    committed(&case, candidate, cx).await?;
    let output = ScreenRect {
        height: case.start.height + 96,
        ..case.start
    };
    let checks = oracle(
        &mut case,
        output,
        (
            crate::long_capture_smoke::ORIGIN,
            crate::long_capture_smoke::ORIGIN,
        ),
        cx,
    )?;
    let requests = case
        .requests
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    ensure(
        requests.contains(&candidate),
        "新ROI仍使用原固定门，工厂未收到实际候选",
    )?;
    let before = case.fixture.wheel_count();
    button(case.panel, "long-auto", cx).await?;
    let end = Instant::now() + WAIT;
    while case.fixture.wheel_count() == before {
        ensure(Instant::now() < end, "提交新ROI后Auto未重新绑定目标")?;
        step(cx).await;
    }
    button(case.panel, "long-auto", cx).await?;
    pause(&case, cx).await?;
    let after = read(case.panel, cx)?;
    ensure(
        after.meta.region == candidate && !after.meta.auto.active,
        "新Auto停止后ROI/状态不符",
    )?;
    let preview = preview_ack(case.panel, &after.meta, cx).await?;
    let observed = case.fixture.wheel_count();
    let cleanup = close(case, cx).await?;
    report.pass("auto-target",vec![serde_json::json!({"mode":"candidate-stops-old-auto-and-new-region-rebinds","pixel_checks":checks,
        "candidate_hold_ms":650,"wheel_count_during_pending":wheels,"wheel_count_after_rebind":observed,
        "guard_regions":requests.into_iter().map(rect_json).collect::<Vec<_>>(),"pending":meta_json(&pending),"final":meta_json(&after),"preview_ack":preview,"cleanup":cleanup})])
}

async fn auto_speed_cases(rect: ScreenRect, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut cases = vec![];
    {
        let id = "adaptive-auto";
        report.start(id)?;
        let mut case = open(rect, ScrollAxis::Vertical, report, cx).await?;
        case.fixture.set_limits(
            crate::long_capture_smoke::ORIGIN,
            crate::long_capture_smoke::ORIGIN + crate::long_capture_smoke::STEP,
        )?;
        pause(&case, cx).await?;
        button(case.panel, "long-settings", cx).await?;
        stage(report, id, "adaptive-settings-opened", case.panel, cx)?;
        button(case.panel, "long-settings", cx).await?;
        button(case.panel, "long-auto", cx).await?;
        let stopped = wait(
            case.panel,
            "页尾仅停止自动输入",
            false,
            |s| {
                s.meta.phase == LongPhase::Running
                    && !s.meta.auto.active
                    && s.meta.auto.phase == crate::long_auto::AutoPhase::NoMotion
                    && s.meta.height == case.start.height + 32
            },
            cx,
        )
        .await?;
        ensure(
            (1.0..=10.0).contains(&stopped.meta.auto.speed),
            "生产Auto的自适应速度越界",
        )?;
        let events = case.fixture.wheel_events();
        ensure(
            events.len() == 4
                && events.iter().all(|event| event.delta == -60)
                && events
                    .iter()
                    .map(|event| event.completed_notches)
                    .collect::<Vec<_>>()
                    == [0, -1, 0, -1],
            "实际Auto未采用两次半格累计，或页尾继续盲发",
        )?;
        let revision = stopped.meta.preview_revision;
        let polls = stopped.meta.poll_count;
        case.fixture.wheel(ScrollAxis::Vertical, -1)?;
        let manual = wait(
            case.panel,
            "Auto停止后手动回滚仍采集",
            false,
            |s| {
                s.meta.phase == LongPhase::Running
                    && !s.meta.auto.active
                    && s.meta.viewport.y == 0
                    && s.meta.height == case.start.height + 32
                    && s.meta.poll_count > polls
                    && s.meta.preview_revision > revision
            },
            cx,
        )
        .await?;
        pause(&case, cx).await?;
        let output_rect = ScreenRect {
            height: case.start.height + 32,
            ..case.start
        };
        let checks = oracle(
            &mut case,
            output_rect,
            (
                crate::long_capture_smoke::ORIGIN,
                crate::long_capture_smoke::ORIGIN,
            ),
            cx,
        )?;
        let paused = read(case.panel, cx)?;
        let preview = preview_ack(case.panel, &paused.meta, cx).await?;
        stage(report, id, "manual-recovery-preview-ack", case.panel, cx)?;
        let cleanup = close(case, cx).await?;
        cases.push(
            serde_json::json!({"mode":id,"observed_speed":stopped.meta.auto.speed,"settings_opened":true,
            "wheel_events":events,"no_motion_kept_running":true,"manual_recovery_observed":true,
            "stopped":meta_json(&stopped),"manual":meta_json(&manual),"preview_ack":preview,
            "pixel_checks":checks,"cleanup":cleanup}),
        );
        report.record("subcase-passed", cases.last().cloned().unwrap())?;
    }
    report.pass("auto-speed", cases)
}
fn popup_probe(
    panel: WindowHandle<LongCaptureView>,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    panel
        .update(cx, |v, _, _| v.auto_popup_probe())
        .map_err(|e| e.to_string().into())
}
async fn popup_view_ack(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    predicate: impl Fn(&serde_json::Value) -> bool,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let end = Instant::now() + WAIT;
    loop {
        let probe = popup_probe(panel, cx)?;
        if predicate(&probe) {
            next_frame(panel, cx).await?;
            return Ok(probe);
        }
        ensure(Instant::now() < end, &format!("{label}未处理：{probe}"))?;
        step(cx).await;
    }
}
fn last_read_sequence(state: &State) -> u64 {
    state.probe["reads"]
        .as_array()
        .and_then(|reads| reads.last())
        .and_then(|read| read["sequence"].as_u64())
        .unwrap_or(0)
}
fn same_source_arcs(a: &crate::stitch::StitchSnapshot, b: &crate::stitch::StitchSnapshot) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.axis == b.axis
        && a.viewport == b.viewport
        && a.segments.len() == b.segments.len()
        && a.segments.iter().zip(&b.segments).all(|(a, b)| {
            Arc::ptr_eq(&a.image, &b.image) && a.source == b.source && a.origin == b.origin
        })
}
async fn popup_pause(case: &Case, cx: &mut AsyncApp) -> Result<State> {
    popup_view_ack(
        case.panel,
        "实际打开设置浮层",
        |v| v["settings_open"] == true,
        cx,
    )
    .await?;
    let paused = wait(
        case.panel,
        "设置浮层真实暂停 ACK",
        false,
        |s| stable(s) && s.meta.phase == LongPhase::Paused && !s.meta.auto.active,
        cx,
    )
    .await?;
    ensure(
        popup_probe(case.panel, cx)?["settings_open"] == true,
        "实际设置点击没有打开浮层",
    )?;
    next_frame(case.panel, cx).await?;
    Ok(paused)
}
async fn popup_quiet(case: &Case, paused: &State, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    let count = case.fixture.wheel_count();
    let end = Instant::now() + Duration::from_millis(650);
    while Instant::now() < end {
        let state = read(case.panel, cx)?;
        ensure(
            stable(&state)
                && state.meta.phase == LongPhase::Paused
                && !state.meta.auto.active
                && state.meta.generation == paused.meta.generation
                && state.meta.poll_count == paused.meta.poll_count
                && state.probe["last_issued_sequence"] == paused.probe["last_issued_sequence"]
                && state.probe["processed_sequence"] == paused.probe["processed_sequence"]
                && last_read_sequence(&state) == last_read_sequence(paused)
                && case.fixture.wheel_count() == count,
            &format!("浮层打开时仍有输入或采集：{}", meta_json(&state)),
        )?;
        step(cx).await;
    }
    Ok(serde_json::json!({"duration_ms":650,"wheel_count":count,
        "issued":paused.probe["last_issued_sequence"],"processed":paused.probe["processed_sequence"],
        "last_successful_read":last_read_sequence(paused),"paused":meta_json(paused),
        "view":popup_probe(case.panel,cx)?}))
}
async fn popup_start_auto(case: &Case, cx: &mut AsyncApp) -> Result<State> {
    case.fixture.set_limits(
        crate::long_capture_smoke::ORIGIN,
        crate::long_capture_smoke::ORIGIN + crate::long_capture_smoke::STEP * 16,
    )?;
    pause(case, cx).await?;
    button(case.panel, "long-auto", cx).await?;
    wait(
        case.panel,
        "打开浮层前 Auto 真实运动",
        false,
        |s| {
            s.meta.phase == LongPhase::Running
                && s.meta.auto.active
                && s.meta.height >= case.start.height + crate::long_capture_smoke::STEP as u32
                && case.fixture.wheel_count() >= 2
        },
        cx,
    )
    .await
}
async fn popup_pending_ack(
    case: &Case,
    cx: &mut AsyncApp,
) -> Result<(State, serde_json::Value, serde_json::Value)> {
    case.fixture.set_limits(
        crate::long_capture_smoke::ORIGIN,
        crate::long_capture_smoke::ORIGIN + crate::long_capture_smoke::STEP * 16,
    )?;
    case.latch.arm();
    let end = Instant::now() + WAIT;
    while !case.latch.entered() {
        ensure(Instant::now() < end, "待 ACK 夹具未持住实际旧匹配帧")?;
        step(cx).await;
    }
    button(case.panel, "long-auto", cx).await?;
    let (posted, view) = loop {
        let state = read(case.panel, cx)?;
        let view = popup_probe(case.panel, cx)?;
        if case.latch.entered()
            && state.meta.requested_generation > state.meta.generation
            && view["auto_pending"] == true
        {
            break (state, view);
        }
        ensure(
            Instant::now() < end,
            "实际 Auto 请求没有进入未 worker ACK 的代数窗",
        )?;
        step(cx).await;
    };
    let before_rect = ui_context(case.panel, cx)?["panel_rect"].clone();
    // 正常 button 会等待 requested 代绘制 ACK；本场景必须在旧匹配仍持有时直接点真实按钮。
    let (input, point) = button_location(case.panel, "long-settings", cx).await?;
    hover_frame(case.panel, input, point, cx).await?;
    own(HWND(input.handle as *mut _))?;
    input.click(point);
    popup_view_ack(
        case.panel,
        "待 ACK 时实际打开设置",
        |v| v["settings_open"] == true,
        cx,
    )
    .await?;
    let paused_request = read(case.panel, cx)?;
    ensure(
        case.latch.entered()
            && paused_request.meta.requested_generation > posted.meta.requested_generation
            && paused_request.meta.generation == posted.meta.generation
            && case.fixture.wheel_count() == 0,
        "待 ACK Auto 打开设置没有先发新暂停代",
    )?;
    let evidence = serde_json::json!({"old_match_hook_entered":true,"posted":meta_json(&posted),
        "posted_view":view,"settings_pause_request":meta_json(&paused_request),
        "settings_click_sent_before_worker_ack":true,"wheel_count_before_release":case.fixture.wheel_count()});
    case.latch.release();
    Ok((posted, before_rect, evidence))
}
async fn auto_popup_cases(rect: ScreenRect, report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut cases = vec![];
    for mode in [
        "popup-resumes-adaptive-auto",
        "popup-explicit-stop",
        "popup-target-closed",
        "popup-paused-remains-paused",
        "popup-pending-ack",
    ] {
        report.start(mode)?;
        let mut case = open(rect, ScrollAxis::Vertical, report, cx).await?;
        let (running, before_rect, pending_evidence) = if mode == "popup-pending-ack" {
            popup_pending_ack(&case, cx).await?
        } else {
            let before = if mode == "popup-paused-remains-paused" {
                pause(&case, cx).await?
            } else {
                popup_start_auto(&case, cx).await?
            };
            let before_rect = ui_context(case.panel, cx)?["panel_rect"].clone();
            button(case.panel, "long-settings", cx).await?;
            (before, before_rect, serde_json::Value::Null)
        };
        let paused = popup_pause(&case, cx).await?;
        let opened_rect = ui_context(case.panel, cx)?["panel_rect"].clone();
        ensure(
            opened_rect["height"].as_u64().unwrap_or(0)
                > before_rect["height"].as_u64().unwrap_or(u64::MAX),
            "实际设置浮层未扩展原生工具条高度",
        )?;
        let quiet = popup_quiet(&case, &paused, cx).await?;
        let opened_view = popup_probe(case.panel, cx)?;
        ensure(
            opened_view["auto_resume_requested"] == (mode != "popup-paused-remains-paused"),
            "浮层没有区分实际 Auto 与原已暂停的恢复来源",
        )?;
        let wheels = case.fixture.wheel_count();
        let saved = case
            .panel
            .update(cx, |v, _, _| v.stitch_snapshot())
            .map_err(|e| e.to_string())?
            .ok_or("浮层真实暂停后缺少可信输出")?;
        let saved_pixels = long_document::render_region(
            &saved,
            Rect::full(saved.width, saved.height),
            saved.width,
            saved.height,
        )?;
        stage(
            report,
            mode,
            "popup-paused-without-input-or-read",
            case.panel,
            cx,
        )?;
        let mut replacement = None;
        let result = match mode {
            "popup-resumes-adaptive-auto" | "popup-pending-ack" => {
                let selected = popup_pause(&case, cx).await?;
                ensure(
                    selected.meta.generation == paused.meta.generation
                        && selected.meta.poll_count == paused.meta.poll_count
                        && case.fixture.wheel_count() == wheels,
                    "浮层打开时擅自开始采集或改变配置代数",
                )?;
                // 只关闭设置；没有第二次点击 Auto。收件与追加像素证明恢复实际发生。
                button(case.panel, "long-settings", cx).await?;
                let resumed = wait(
                    case.panel,
                    "关闭设置恢复自适应滚动",
                    false,
                    |s| {
                        s.meta.phase == LongPhase::Running
                            && s.meta.auto.active
                            && (1.0..=10.0).contains(&s.meta.auto.speed)
                            && s.meta.height > paused.meta.height
                            && case.fixture.wheel_count() >= wheels + 2
                    },
                    cx,
                )
                .await?;
                ensure(
                    popup_probe(case.panel, cx)?["settings_open"] == false,
                    "自动恢复后设置浮层仍打开",
                )?;
                let detail = serde_json::json!({"resumed":meta_json(&resumed),
                    "wheel_count_before_close":wheels,"wheel_count_after_resume":case.fixture.wheel_count(),
                    "observed_speed":resumed.meta.auto.speed,"second_auto_click_sent":false});
                pause(&case, cx).await?;
                detail
            }
            "popup-explicit-stop" => {
                // 复用实际一级 Auto 按钮的停止入口，不能直接清恢复模型或 session flags。
                button(case.panel, "long-auto", cx).await?;
                let stop_view = popup_view_ack(
                    case.panel,
                    "实际显式停止按钮",
                    |v| v["settings_open"] == true && v["auto_resume_requested"] == false,
                    cx,
                )
                .await?;
                let stopped = popup_pause(&case, cx).await?;
                button(case.panel, "long-settings", cx).await?;
                let manual = wait(
                    case.panel,
                    "显式停止后关闭仅恢复手动采集",
                    false,
                    |s| {
                        s.meta.phase == LongPhase::Running
                            && !s.meta.auto.active
                            && s.meta.poll_count > stopped.meta.poll_count
                            && s.meta.generation == s.meta.requested_generation
                    },
                    cx,
                )
                .await?;
                ensure(
                    popup_probe(case.panel, cx)?["settings_open"] == false,
                    "显式停止后设置浮层没有关闭",
                )?;
                let end = Instant::now() + Duration::from_millis(650);
                while Instant::now() < end {
                    let state = read(case.panel, cx)?;
                    ensure(
                        state.meta.phase == LongPhase::Running
                            && !state.meta.auto.active
                            && case.fixture.wheel_count() == wheels,
                        "显式停止后关闭浮层复活了自动滚轮",
                    )?;
                    step(cx).await;
                }
                let detail = serde_json::json!({"stop":meta_json(&stopped),"stop_view":stop_view,
                    "manual":meta_json(&manual),"wheel_count_before_close":wheels,
                    "wheel_count_after_manual":case.fixture.wheel_count(),"no_auto_input_observed_ms":650});
                pause(&case, cx).await?;
                detail
            }
            "popup-target-closed" => {
                case.fixture.close()?;
                ensure(
                    !unsafe { IsWindow(Some(HWND(case.fixture.native() as *mut _))) }.as_bool(),
                    "设置浮层负例没有实际销毁目标 HWND",
                )?;
                // 所有 ROI 继续被新的自有不透明窗口覆盖；旧上下文仍只能校验已经关闭的目标。
                let other =
                    Fixture::new_owned(rect, ScrollAxis::Vertical, (0, 0), FixturePattern::Noise)?;
                own(HWND(other.native() as *mut _))?;
                report.windows.push(other.native());
                let panel_hwnd = case
                    .panel
                    .update(cx, |_, w, _| hwnd(w))
                    .map_err(|e| e.to_string())?;
                own(panel_hwnd)?;
                unsafe {
                    SetWindowPos(
                        HWND(other.native() as *mut _),
                        Some(panel_hwnd),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    )?;
                }
                replacement = Some(other);
                button(case.panel, "long-settings", cx).await?;
                let view = popup_view_ack(
                    case.panel,
                    "实际关闭失效目标设置",
                    |v| {
                        v["settings_open"] == false
                            && v["resume_requested"] == false
                            && v["auto_resume_requested"] == false
                            && v["auto_pending"] == false
                    },
                    cx,
                )
                .await?;
                let failed = wait(
                    case.panel,
                    "目标失效后关闭设置拒绝恢复",
                    true,
                    |s| {
                        stable(s)
                            && matches!(s.meta.phase, LongPhase::Paused | LongPhase::Failed)
                            && !s.meta.auto.active
                    },
                    cx,
                )
                .await?;
                let rejection = case
                    .panel
                    .update(cx, |v, _, _| v.status.clone())
                    .map_err(|e| e.to_string())?;
                ensure(
                    rejection.contains("恢复已取消") || failed.meta.phase == LongPhase::Failed,
                    "目标失效没有真实验证拒绝原因",
                )?;
                let end = Instant::now() + Duration::from_millis(650);
                while Instant::now() < end {
                    let state = read(case.panel, cx)?;
                    ensure(
                        stable(&state)
                            && matches!(state.meta.phase, LongPhase::Paused | LongPhase::Failed)
                            && !state.meta.auto.active
                            && case.fixture.wheel_count() == wheels
                            && last_read_sequence(&state) == last_read_sequence(&paused)
                            && replacement
                                .as_ref()
                                .is_some_and(|other| other.wheel_count() == 0),
                        "目标失效后设置关闭仍投递输入或成功读取新帧",
                    )?;
                    step(cx).await;
                }
                let after = case
                    .panel
                    .update(cx, |v, _, _| v.stitch_snapshot())
                    .map_err(|e| e.to_string())?
                    .ok_or("目标失效后旧输出丢失")?;
                ensure(
                    same_source_arcs(&saved, &after),
                    "目标失效后旧结果条带 Arc 被替换",
                )?;
                let after_pixels = long_document::render_region(
                    &after,
                    Rect::full(after.width, after.height),
                    after.width,
                    after.height,
                )?;
                ensure(
                    after_pixels.bgra == saved_pixels.bgra,
                    "目标失效后旧结果像素变化",
                )?;
                serde_json::json!({"terminal":meta_json(&failed),"view":view,
                    "rejection":rejection,
                    "closed_target_hwnd":case.fixture.native(),"wheel_count":wheels,
                    "last_successful_read_before_close":last_read_sequence(&paused),
                    "last_successful_read_after_close":last_read_sequence(&read(case.panel,cx)?),
                    "replacement_hwnd":replacement.as_ref().map(Fixture::native),
                    "replacement_wheel_count":replacement.as_ref().map(Fixture::wheel_count),
                    "old_result_same_arc":true,"old_result_all_pixels_unchanged":true,
                    "no_input_or_read_observed_ms":650,"guard":case.fixture.capture_probe_snapshot()})
            }
            "popup-paused-remains-paused" => {
                ensure(
                    paused.meta.generation == running.meta.generation && wheels == 0,
                    "原已暂停打开设置发出了新暂停代或自动输入",
                )?;
                button(case.panel, "long-settings", cx).await?;
                let view = popup_view_ack(
                    case.panel,
                    "原暂停设置实际关闭",
                    |v| {
                        v["settings_open"] == false
                            && v["resume_requested"] == false
                            && v["auto_resume_requested"] == false
                            && v["auto_pending"] == false
                    },
                    cx,
                )
                .await?;
                let end = Instant::now() + Duration::from_millis(650);
                while Instant::now() < end {
                    let state = read(case.panel, cx)?;
                    ensure(
                        stable(&state)
                            && state.meta.phase == LongPhase::Paused
                            && !state.meta.auto.active
                            && state.meta.generation == running.meta.generation
                            && state.meta.poll_count == paused.meta.poll_count
                            && state.probe["last_issued_sequence"]
                                == paused.probe["last_issued_sequence"]
                            && last_read_sequence(&state) == last_read_sequence(&paused)
                            && case.fixture.wheel_count() == 0,
                        "原已暂停关闭设置被错误唤醒",
                    )?;
                    step(cx).await;
                }
                let after = case
                    .panel
                    .update(cx, |v, _, _| v.stitch_snapshot())
                    .map_err(|e| e.to_string())?
                    .ok_or("原暂停关闭设置后输出丢失")?;
                ensure(
                    same_source_arcs(&saved, &after),
                    "原已暂停开关设置改变了可信条带",
                )?;
                serde_json::json!({"view":view,"generation_unchanged":running.meta.generation,
                    "wheel_count":0,"last_successful_read":last_read_sequence(&paused),
                    "kept_paused_observed_ms":650,"old_result_same_arc":true})
            }
            _ => unreachable!(),
        };
        let output = read(case.panel, cx)?;
        let output_rect = ScreenRect {
            width: output.meta.width,
            height: output.meta.height,
            ..case.start
        };
        let checks = oracle(
            &mut case,
            output_rect,
            (
                crate::long_capture_smoke::ORIGIN,
                crate::long_capture_smoke::ORIGIN,
            ),
            cx,
        )?;
        let preview = preview_ack(case.panel, &output.meta, cx).await?;
        let final_rect = ui_context(case.panel, cx)?["panel_rect"].clone();
        ensure(
            final_rect["height"] == before_rect["height"],
            "设置关闭未恢复原生工具条高度",
        )?;
        stage(
            report,
            mode,
            "popup-closed-and-production-output-verified",
            case.panel,
            cx,
        )?;
        drop(saved);
        let cleanup = close(case, cx).await?;
        let replacement_cleanup = if let Some(mut other) = replacement {
            ensure(other.wheel_count() == 0, "关闭面板时向替代目标发送了滚轮")?;
            other.close()?;
            Some(
                serde_json::json!({"hwnd":other.native(),"wheel_count":other.wheel_count(),
                "hwnd_destroyed":!unsafe{IsWindow(Some(HWND(other.native() as *mut _)))}.as_bool()}),
            )
        } else {
            None
        };
        cases.push(serde_json::json!({"mode":mode,"before":meta_json(&running),"quiet":quiet,
            "pending_ack_evidence":pending_evidence,"replacement_cleanup":replacement_cleanup,
            "closed":result,"before_panel_rect":before_rect,"opened_panel_rect":opened_rect,
            "closed_panel_rect":final_rect,"pixel_checks":checks,"preview_ack":preview,"cleanup":cleanup}));
        report.record("subcase-passed", cases.last().cloned().unwrap())?;
    }
    report.pass("auto-popup", cases)
}
async fn verify(
    rect: ScreenRect,
    loss: HWND,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<()> {
    report.record(
        "before-region",
        serde_json::json!({"fixture":rect_json(rect)}),
    )?;
    for group in report.requested.clone() {
        match group.as_str() {
            "move" => move_cases(rect, report, cx).await?,
            "resize" => resize_cases(rect, report, cx).await?,
            "cancel-failed-stale" => cancel_cases(rect, loss, report, cx).await?,
            "auto-target" => auto_case(rect, report, cx).await?,
            "auto-speed" => auto_speed_cases(rect, report, cx).await?,
            "auto-popup" => auto_popup_cases(rect, report, cx).await?,
            _ => unreachable!(),
        }
    }
    ensure(
        crate::long_capture_pipeline::active_capture_threads() == 0
            && crate::long_capture_runtime::active_workers() == 0,
        "专项结束仍有采集线程",
    )?;
    report.record(
        "after-region",
        serde_json::json!({"capture_threads":0,"consumer_sessions":0}),
    )
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let lock = unsafe { CreateMutexW(None, false, w!("Local\\RustShot.MVP.Capture"))? };
    let _lock = CaptureLock::new(lock);
    ensure(
        unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
        "已有截图会话，未启动ROI专项",
    )?;
    let root = std::env::var_os("RUSTSHOT_LONG_REGION_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join("target/qa/long-region"));
    std::fs::create_dir_all(&root)?;
    let requested = std::env::var("RUSTSHOT_LONG_REGION_GROUPS")
        .unwrap_or_else(|_| "move,resize,cancel-failed-stale,auto-target".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect::<Vec<_>>();
    ensure(
        !requested.is_empty()
            && requested.iter().all(|s| {
                matches!(
                    s.as_str(),
                    "move"
                        | "resize"
                        | "cancel-failed-stale"
                        | "auto-target"
                        | "auto-speed"
                        | "auto-popup"
                )
            })
            && requested.iter().collect::<HashSet<_>>().len() == requested.len(),
        "ROI专项组为空/重复/无效",
    )?;
    let work = crate::long_capture_smoke::work_area()?;
    ensure(
        work.right - work.left >= 1280 && work.bottom - work.top >= 800,
        "工作区无法容纳自有960×640夹具与工具条",
    )?;
    let rect = ScreenRect {
        left: work.left + 32,
        top: work.top + 32,
        width: FIXTURE_WIDTH,
        height: FIXTURE_HEIGHT,
    };
    let holder = Rc::new(RefCell::new(Some(Report {
        root,
        requested,
        groups: vec![],
        stages: vec![],
        windows: vec![],
        active_mode: None,
    })));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("ROI专项未执行完毕".to_string())));
    let final_error = error.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            let host = match cx.open_window(
                WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Host),
            ) {
                Ok(h) => h,
                Err(e) => {
                    *error.borrow_mut() = Some(e.to_string());
                    cx.quit();
                    return;
                }
            };
            let loss = match host.update(cx, |_, w, _| hwnd(w)) {
                Ok(h) => h,
                Err(e) => {
                    *error.borrow_mut() = Some(e.to_string());
                    cx.quit();
                    return;
                }
            };
            cx.spawn(async move |cx| {
                let mut report = holder.borrow_mut().take().expect("独占ROI报告");
                *error.borrow_mut() = verify(rect, loss, &mut report, cx)
                    .await
                    .err()
                    .map(|e| e.to_string());
                let _ = cx.update(|cx| {
                    for w in cx.windows() {
                        let _ = cx.update_window(w, |_, w, _| w.remove_window());
                    }
                    cx.quit();
                });
                *holder.borrow_mut() = Some(report);
            })
            .detach();
        });
    let report = final_holder.borrow_mut().take().ok_or("ROI报告丢失")?;
    let mut error = final_error.borrow_mut().take();
    if error.is_none()
        && (crate::long_capture_pipeline::active_capture_threads() != 0
            || crate::long_capture_runtime::active_workers() != 0)
    {
        error = Some("进程返回前采集线程残留".into());
    }
    if error.is_none()
        && report
            .windows
            .iter()
            .any(|h| unsafe { IsWindow(Some(HWND(*h as *mut _))) }.as_bool())
    {
        error = Some("专项返回时自有窗口仍存活".into());
    }
    report.save(true, error.as_deref())?;
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}
