//! 从一级截图按钮进入真实滚动采集、裁剪调整后返回并回收。输入只投递本进程自有窗口。
use crate::{
    capture::Image,
    capture_view::CaptureView,
    gpui_capture::hwnd,
    gpui_smoke::{isolate_input, Input},
    long_auto::AutoPhase,
    long_capture_runtime::{active_workers, LongPhase, LongSnapshot},
    long_capture_smoke::{self as fixtures, Fixture, FixturePattern},
    long_capture_view::LongCaptureView,
    stitch::ScrollAxis,
    Result,
};
use gpui::{
    div, App, AppContext, AsyncApp, Context, Entity, Render, Window, WindowHandle, WindowOptions,
};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::HWND,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

thread_local! {
    static FIXTURE: RefCell<Option<Fixture>> = const { RefCell::new(None) };
}
const WAIT: Duration = Duration::from_secs(20);
const CONTINUOUS_DOWN_STEPS: u32 = 15;
const REVERSE_STEPS: u32 = 3;
const CONTINUE_STEPS: u32 = 5;
const SCROLL_LIMIT_STEPS: u32 = 20;
struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
fn ensure(value: bool, text: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(text.to_owned().into())
    }
}
pub(crate) fn run() -> Result<()> {
    crate::remote_display::force_local_for_smoke();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let work = fixtures::work_area()?;
    let mut region = fixtures::source_region(work);
    // 靠下选区验证上方主栏与向外展开的二级栏，避免只覆盖默认下方布局。
    region.top = work.bottom - region.height as i32 - 32;
    let pattern = match std::env::var("RUSTSHOT_LONG_FIXTURE").as_deref() {
        Ok("sparse-page") => FixturePattern::SparsePage,
        Ok("noise") | Err(_) => FixturePattern::Noise,
        Ok(value) => return Err(format!("未知长截图验收场景：{value}").into()),
    };
    let fixture = Fixture::new_with_pattern(region, ScrollAxis::Vertical, (0, 0), None, pattern)?;
    fixture.set_limits(
        fixtures::ORIGIN,
        fixtures::ORIGIN + SCROLL_LIMIT_STEPS as i32 * fixtures::STEP,
    )?;
    FIXTURE.with(|s| *s.borrow_mut() = Some(fixture));
    let old_case = std::env::var_os("RUSTSHOT_SMOKE_CASE");
    std::env::set_var("RUSTSHOT_SMOKE_CASE", "long-workflow");
    let result = crate::gpui_capture::run(false, false, true);
    if let Some(old) = old_case {
        std::env::set_var("RUSTSHOT_SMOKE_CASE", old);
    } else {
        std::env::remove_var("RUSTSHOT_SMOKE_CASE");
    }
    FIXTURE.with(|s| {
        s.borrow_mut().take();
    });
    result
}
async fn step(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(50))
        .await;
}
async fn wait_panel(
    panel: WindowHandle<LongCaptureView>,
    label: &str,
    check: impl Fn(&LongSnapshot) -> bool,
    cx: &mut AsyncApp,
) -> Result<LongSnapshot> {
    let deadline = Instant::now() + WAIT;
    loop {
        let (s, ui_status) = panel
            .update(cx, |v, _, _| (v.snapshot(), v.status.clone()))
            .map_err(|e| e.to_string())?;
        if check(&s) {
            return Ok(s);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{label}超时：phase={:?},auto={:?},steps={},尺寸={}×{},status={},ui={ui_status}",
                s.phase, s.auto.phase, s.auto.steps, s.width, s.height, s.status
            )
            .into());
        }
        step(cx).await;
    }
}
async fn button(panel: WindowHandle<LongCaptureView>, id: &str, cx: &mut AsyncApp) -> Result<()> {
    // 等待 UI 的 50ms 消费轮询及实际绘制，不能点击上一帧尚未启用的控件。
    cx.background_executor()
        .timer(Duration::from_millis(160))
        .await;
    let deadline = Instant::now() + WAIT;
    loop {
        let (input, p) = panel
            .update(cx, |v, w, _| {
                (
                    Input {
                        handle: hwnd(w).0 as usize,
                        scale: w.scale_factor(),
                    },
                    v.ui_positions.borrow().get(id).copied(),
                )
            })
            .map_err(|e| e.to_string())?;
        if let Some(p) = p {
            isolate_input(HWND(input.handle as *mut _))?;
            input.click(p);
            cx.background_executor()
                .timer(Duration::from_millis(180))
                .await;
            return Ok(());
        }
        ensure(Instant::now() < deadline, &format!("找不到长截图按钮{id}"))?;
        step(cx).await;
    }
}
async fn preview_ready(
    panel: WindowHandle<LongCaptureView>,
    expected_height: u32,
    cx: &mut AsyncApp,
) -> Result<(u64, f32, f32)> {
    let deadline = Instant::now() + WAIT;
    loop {
        let ready = panel
            .update(cx, |v, _, cx| {
                let preview_handle = v.preview_window?;
                let preview = preview_handle.read(cx).ok()?;
                let p = preview.displayed.as_ref()?;
                let b = (*preview.preview_bounds.borrow())?;
                (p.height == expected_height
                    && p.image.width as f32 + 0.6 >= f32::from(b.size.width) * preview.density
                    && p.image.height as f32 + 0.6 >= f32::from(b.size.height) * preview.density
                    && (f32::from(b.size.height)
                        - f32::from(b.size.width) * p.image.height as f32 / p.image.width as f32)
                        .abs()
                        <= 1.1)
                    .then_some((
                        p.revision,
                        f32::from(b.size.width),
                        f32::from(b.size.height),
                    ))
            })
            .map_err(|e| e.to_string())?;
        if let Some((revision, width, height)) = ready {
            ensure(
                (190. ..=210.).contains(&width) && height >= 100.,
                "独立实时预览比例/尺寸不正确",
            )?;
            return Ok((revision, width, height));
        }
        ensure(Instant::now() < deadline, "界面未显示最新长图预览")?;
        step(cx).await;
    }
}
async fn preview_viewport(
    panel: WindowHandle<LongCaptureView>,
    expected: &LongSnapshot,
    previous_revision: u64,
    cx: &mut AsyncApp,
) -> Result<u64> {
    let deadline = Instant::now() + WAIT;
    loop {
        let ready = panel
            .update(cx, |v, _, cx| {
                let preview = v.preview_window?.read(cx).ok()?;
                let frame = preview.displayed.as_ref()?;
                let bounds = (*preview.preview_bounds.borrow())?;
                (frame.revision > previous_revision
                    && frame.viewport == expected.viewport
                    && frame.width == expected.width
                    && frame.height == expected.height
                    && f32::from(bounds.size.width) > 0.
                    && f32::from(bounds.size.height) > 0.)
                    .then_some(frame.revision)
            })
            .map_err(|e| e.to_string())?;
        if let Some(revision) = ready {
            return Ok(revision);
        }
        ensure(
            Instant::now() < deadline,
            &format!(
                "滚动后右侧预览未更新视口：预期 {:?}，上次 revision={previous_revision}",
                expected.viewport
            ),
        )?;
        step(cx).await;
    }
}
fn snapshot_log(snapshot: &LongSnapshot) -> serde_json::Value {
    serde_json::json!({
        "phase":format!("{:?}",snapshot.phase),
        "viewport":[snapshot.viewport.x,snapshot.viewport.y,snapshot.viewport.width,snapshot.viewport.height],
        "dims":[snapshot.width,snapshot.height],
        "revision":snapshot.preview_revision,
        "update":format!("{:?}",snapshot.last_result),
        "last_delta":snapshot.last_delta,
        "accepted_frames":snapshot.accepted_frames,
        "poll_count":snapshot.poll_count,
        "busy":snapshot.busy,
        "capture_in_flight":snapshot.capture_in_flight,
        "status":snapshot.status,
    })
}
fn check_fixture_pixels(
    panel: WindowHandle<LongCaptureView>,
    fixture: &Fixture,
    height: u32,
    label: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (meta, snapshot) = panel
        .update(cx, |v, _, _| {
            (
                v.snapshot(),
                v.session
                    .as_ref()
                    .and_then(|s| s.smoke_committed_snapshot()),
            )
        })
        .map_err(|e| e.to_string())?;
    let snapshot = snapshot.ok_or("连续滚动后没有已提交结果")?;
    ensure(
        snapshot.width == meta.width
            && snapshot.height == meta.height
            && snapshot.viewport == meta.viewport,
        "QA 已提交条带与当前视口日志不一致",
    )?;
    ensure(
        snapshot.width == fixture.region.width && snapshot.height == height,
        "QA 已提交结果尺寸与本次滚动预期不一致",
    )?;
    let image = fixtures::stitched_image(&snapshot)?;
    let expected = fixtures::scene_for_pattern(
        fixture.pattern,
        fixture.region.width,
        height,
        ScrollAxis::Vertical,
        (0, 0),
        (fixtures::ORIGIN, fixtures::ORIGIN),
        0,
    );
    fixtures::same_image(&image, &expected, label)
}
async fn save_panel_frame(
    panel: WindowHandle<LongCaptureView>,
    expanded: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (preview, toolbar) = panel
        .update(cx, |v, w, _| -> Result<_> {
            let size = w.viewport_size();
            for id in [
                "long-pause",
                "long-auto",
                "long-undo",
                "long-crop",
                "long-close",
                "long-copy",
                "long-save",
                "long-pin",
                "long-settings",
            ] {
                let &(x, y) = v
                    .ui_positions
                    .borrow()
                    .get(id)
                    .ok_or_else(|| format!("缺少长截图操作{id}"))?;
                ensure(
                    x > 0.
                        && y > 0.
                        && x + 16. < f32::from(size.width) - 4.
                        && y + 16. < f32::from(size.height) - 4.,
                    &format!("长截图操作被挤出浮动工具栏：{id}"),
                )?;
            }
            if expanded {
                let positions = v.ui_positions.borrow();
                for id in [
                    "long-axis-vertical",
                    "long-axis-horizontal",
                    "long-auto-forward",
                    "long-auto-reverse",
                    "long-auto-adaptive",
                    "long-fixed-none",
                    "long-fixed-auto",
                    "long-fixed-manual",
                    "long-apply-settings",
                    "long-fixed-start",
                    "long-fixed-end",
                ] {
                    let &(x, y) = positions
                        .get(id)
                        .ok_or_else(|| format!("缺少二级参数{id}"))?;
                    ensure(
                        x > 0.
                            && y > 0.
                            && x < f32::from(size.width)
                            && y + 16. < f32::from(size.height) - 4.,
                        &format!("二级参数被挤出浮动工具栏：{id}"),
                    )?;
                }
                let axis_y = positions["long-axis-vertical"].1;
                let adaptive_y = positions["long-auto-adaptive"].1;
                let fixed_y = positions["long-fixed-manual"].1;
                let input_y = positions["long-fixed-start"].1;
                let primary_y = positions["long-settings"].1;
                ensure(
                    crate::pin_native::rect(hwnd(w))?.y < v.region.top
                        && input_y < fixed_y
                        && fixed_y < adaptive_y
                        && adaptive_y < axis_y
                        && axis_y < primary_y,
                    "上方二级栏方向与自动调速说明没有靠近选区",
                )?;
            }
            Ok((
                v.preview_window.ok_or("缺少独立实时预览")?,
                w.window_handle(),
            ))
        })
        .map_err(|e| e.to_string())??;
    let preview = preview
        .update(cx, |_, w, _| w.window_handle())
        .map_err(|e| e.to_string())?;
    let windows = if expanded {
        vec![(toolbar, "properties.png")]
    } else {
        vec![(toolbar, "controls.png"), (preview, "live-preview.png")]
    };
    for (window, name) in windows {
        save_window_frame(window, name, None, cx).await?;
    }
    Ok(())
}
pub(crate) async fn save_window_frame(
    window: gpui::AnyWindowHandle,
    name: &str,
    height: Option<f32>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let dimensions = cx
        .update_window(window, |_, w, _| {
            let size = w.viewport_size();
            let scale = w.scale_factor();
            [
                (f32::from(size.width) * scale).round() as u32,
                (height
                    .unwrap_or(f32::from(size.height))
                    .min(f32::from(size.height))
                    * scale)
                    .round() as u32,
            ]
        })
        .map_err(|e| e.to_string())?;
    cx.update_window(window, |_, w, _| {
        w.request_frame_capture([0, 0, dimensions[0], dimensions[1]])
    })
    .map_err(|e| e.to_string())??;
    for _ in 0..100 {
        step(cx).await;
        if let Some(bytes) = cx
            .update_window(window, |_, w, _| w.take_frame_capture())
            .map_err(|e| e.to_string())?
        {
            let path = std::env::var_os("RUSTSHOT_LONG_WORKFLOW_REPORT")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "target/qa/long-workflow.json".into())
                .with_file_name(name);
            crate::export::save_png(
                Image {
                    width: dimensions[0],
                    height: dimensions[1],
                    bgra: bytes.map_err(|e| e.to_string())?,
                    readback_ms: 0.,
                },
                &path,
            )?;
            return Ok(());
        }
    }
    Err("自有长截图窗口GPU帧缓冲读回超时".into())
}
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _cx: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    let weak = view.downgrade();
    window.on_next_frame(move |window, cx| {
        // Windows 在最后一个可管理窗口销毁时结束事件循环；隐藏 QA 宿主留到回收检查完成。
        let _keep_alive = cx.open_window(WindowOptions { show: false, focus: false,
            ..Default::default() }, |_, cx| cx.new(|_| KeepAlive)).expect("长截图验收宿主");
        let input = Input { handle: hwnd(window).0 as usize, scale: window.scale_factor() };
        let fixture = FIXTURE.with(|s| s.borrow_mut().take()).expect("长截图自有场景");
        cx.spawn(async move |cx| {
            let mut fixture = fixture;
            // 失败也保存已经完成和正在等待的步骤，不能只留下最终超时文字。
            let continuous_scroll_steps = Rc::new(RefCell::new(Vec::<serde_json::Value>::new()));
            let undo_check = Rc::new(RefCell::new(serde_json::Value::Null));
            let result: Result<serde_json::Value> = async {
                isolate_input(HWND(input.handle as *mut _))?;
                let region = fixture.region;
                let (x, y) = weak.update(cx, |v, _| (
                    (region.left - v.source.bounds.left) as u32,
                    (region.top - v.source.bounds.top) as u32)).map_err(|e| e.to_string())?;
                input.drag((x, y), (x + region.width, y + region.height));
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let p = weak.update(cx, |v, _| v.ui_positions.borrow()
                    .get("long-capture-primary").copied()).map_err(|e| e.to_string())?
                    .ok_or("一级工具栏没有长截图按钮")?;
                input.click(p);
                let deadline = Instant::now() + WAIT;
                let panel = loop {
                    if let Some(panel) = cx.update(|cx| cx.windows().into_iter()
                        .find_map(|w| w.downcast::<LongCaptureView>())).map_err(|e| e.to_string())? {
                        break panel;
                    }
                    ensure(Instant::now() < deadline, "一级按钮没有打开长截图")?;
                    step(cx).await;
                };
                let initial = wait_panel(panel, "首次采集", |s| s.result_available
                    && s.phase == LongPhase::Running, cx).await?;
                ensure(!unsafe { IsWindow(Some(HWND(input.handle as *mut _))) }.as_bool(),
                    "冻结截图覆盖窗没有退场")?;
                ensure(initial.width == region.width && initial.height == region.height,
                    &format!("一级入口选区坐标不一致：实际{}×{}，预期{}×{}，选区{:?}",
                        initial.width, initial.height, region.width, region.height, initial.region))?;
                ensure(panel.update(cx, |v, _, _| v.capture_lock.is_some())
                    .map_err(|e| e.to_string())?, "采集锁没有移交")?;
                let border_handles = panel.update(cx, |v, _, _| -> Result<_> {
                    let border = v.border.as_ref().ok_or("进入长截图后没有选区边框")?;
                    let handles = border.handles();
                    for (h, region) in handles.into_iter().zip(border.regions()) {
                        let mut affinity = 0;
                        unsafe { GetWindowDisplayAffinity(h, &mut affinity)?; }
                        ensure(unsafe { IsWindowVisible(h) }.as_bool() && affinity == WDA_EXCLUDEFROMCAPTURE.0,
                            "边框未显示或未排除于采集")?;
                        let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) };
                        ensure(ex & (WS_EX_NOACTIVATE | WS_EX_TRANSPARENT).0 as isize == (WS_EX_NOACTIVATE | WS_EX_TRANSPARENT).0 as isize,
                            "选区边框会抢占焦点或鼠标")?;
                        let r = crate::pin_native::rect(h)?;
                        ensure(r.x == region.left && r.y == region.top && r.width == region.width && r.height == region.height,
                            "原生边框与所选物理范围不一致")?;
                    }
                    Ok(handles.map(|h| h.0 as usize))
                }).map_err(|e| e.to_string())??;
                let first_preview = preview_ready(panel, region.height, cx).await?;
                let preview = panel.update(cx, |v, _, _| v.preview_window).map_err(|e| e.to_string())?.ok_or("缺少独立预览窗口")?;
                let preview_handle = preview.update(cx, |_, w, _| -> Result<_> {
                    let h = hwnd(w);
                    let mut affinity = 0;
                    unsafe { GetWindowDisplayAffinity(h, &mut affinity)?; }
                    ensure(unsafe { IsWindowVisible(h) }.as_bool() && affinity == WDA_EXCLUDEFROMCAPTURE.0,
                        "侧预览未显示或未排除采集")?;
                    let placement = crate::pin_native::rect(h)?;
                    ensure(placement.x >= region.left + region.width as i32,
                        "有足够右侧空间时预览没有放到选区右边")?;
                    Ok(h.0 as usize)
                }).map_err(|e| e.to_string())??;
                let mut manual = initial.clone();
                let mut previous_revision = first_preview.0;
                let mut offset = 0_u32;
                let mut peak = 0_u32;
                let manual_steps = CONTINUOUS_DOWN_STEPS + REVERSE_STEPS + CONTINUE_STEPS;
                for index in 0..manual_steps {
                    let wheel = if (CONTINUOUS_DOWN_STEPS..CONTINUOUS_DOWN_STEPS + REVERSE_STEPS).contains(&index) { -1 } else { 1 };
                    offset = (offset as i32 + wheel as i32 * fixtures::STEP) as u32;
                    peak = peak.max(offset);
                    let expected_height = region.height + peak;
                    let before = manual.clone();
                    let grows = expected_height > before.height;
                    let direction = if wheel < 0 { "up" } else if grows { "down" } else { "down-revisit" };
                    let label = format!("连续滚动第{}步（{direction}，视口{offset}px）", index + 1);
                    continuous_scroll_steps.borrow_mut().push(serde_json::json!({
                        "step":index+1,"direction":direction,"wheel":wheel,"ok":false,
                        "expected_viewport_y":offset,"expected_height":expected_height,
                        "before":snapshot_log(&before),"log":"等待自有页面滚动与采集预览更新"
                    }));
                    let started = Instant::now();
                    fixture.wheel(ScrollAxis::Vertical, wheel)?;
                    let next = match wait_panel(panel, &label, |s| s.phase == LongPhase::Running
                        && !s.busy && s.width == region.width && s.height == expected_height
                        && s.viewport.x == 0 && s.viewport.y == offset
                        && s.viewport.width == region.width && s.viewport.height == region.height
                        && (!grows || s.accepted_frames > before.accepted_frames), cx).await {
                        Ok(next) => next,
                        Err(error) => {
                            let observed = panel.update(cx, |v, _, _| v.snapshot()).ok();
                            let mut steps = continuous_scroll_steps.borrow_mut();
                            steps[index as usize]["log"] = serde_json::json!(error.to_string());
                            steps[index as usize]["observed"] = observed.as_ref().map(snapshot_log).unwrap_or(serde_json::Value::Null);
                            return Err(error);
                        }
                    };
                    continuous_scroll_steps.borrow_mut()[index as usize]["observed"] = snapshot_log(&next);
                    continuous_scroll_steps.borrow_mut()[index as usize]["log"] = serde_json::json!("采集已到目标视口，等待右侧预览");
                    let revision = preview_viewport(panel, &next, previous_revision, cx).await?;
                    ensure(revision > previous_revision, "连续滚动后预览 revision 没有递增")?;
                    ensure(fixture.origin() == (fixtures::ORIGIN, fixtures::ORIGIN + offset as i32), "自有滚动页没有到达预定位置")?;
                    check_fixture_pixels(panel, &fixture, expected_height, &format!("{label}逐像素结果"), cx)?;
                    continuous_scroll_steps.borrow_mut()[index as usize] = serde_json::json!({
                        "step":index+1,"direction":direction,"wheel":wheel,"ok":true,
                        "viewport":[next.viewport.x,next.viewport.y,next.viewport.width,next.viewport.height],
                        "dims":[next.width,next.height],"height":next.height,"viewport_y":next.viewport.y,
                        "revision":revision,"preview_revision":revision,"previous_revision":previous_revision,
                        "update":format!("{:?}",next.last_result),"effect":if grows {"added"} else {"viewport-only"},
                        "accepted_frames_delta":next.accepted_frames.saturating_sub(before.accepted_frames),
                        "poll_count_delta":next.poll_count.saturating_sub(before.poll_count),
                        "fixture_origin":fixture.origin(),"received_wheels":fixture.wheel_count(),
                        "elapsed_ms":started.elapsed().as_millis(),"exact_pixels":true,
                        "log":next.status,"snapshot":snapshot_log(&next)
                    });
                    manual = next;
                    previous_revision = revision;
                }
                ensure(continuous_scroll_steps.borrow().len() == manual_steps as usize
                    && continuous_scroll_steps.borrow().iter().all(|step| step["ok"] == true),
                    "连续滚动验收没有覆盖全部步骤")?;
                let grown_preview = preview_ready(panel, manual.height, cx).await?;
                let preview_quality = preview.update(cx, |v, w, _| -> Result<_> {
                    let frame = v.displayed.as_ref().ok_or("缺少高清预览")?;
                    let bounds = (*v.preview_bounds.borrow()).ok_or("高清预览没有绘制")?;
                    let dpi = w.scale_factor();
                    let physical = [f32::from(bounds.size.width) * dpi, f32::from(bounds.size.height) * dpi];
                    ensure(frame.image.width as f32 + 0.6 >= physical[0]
                        && frame.image.height as f32 + 0.6 >= physical[1], "高 DPI 预览仍被放大")?;
                    for value in [f32::from(bounds.origin.x) * dpi, f32::from(bounds.origin.y) * dpi,
                        physical[0], physical[1]] {
                        ensure((value - value.round()).abs() < 0.02, "预览没有按设备像素对齐")?;
                    }
                    ensure(frame.image.width as u64 * frame.image.height as u64 <= 875_000, "预览超出固定内存预算")?;
                    Ok(serde_json::json!({"dpi_scale":dpi,"texture":[frame.image.width,frame.image.height],
                        "display_physical":physical,"no_upscale":true,"pixel_aligned":true}))
                }).map_err(|e| e.to_string())??;
                ensure(grown_preview.0 > first_preview.0, "手动滚动没有刷新右侧实时预览")?;
                save_panel_frame(panel, false, cx).await?;
                button(panel, "long-settings", cx).await?;
                button(panel, "long-fixed-manual", cx).await?;
                save_panel_frame(panel, true, cx).await?;
                button(panel, "long-fixed-auto", cx).await?;
                button(panel, "long-settings", cx).await?;
                // 动画可能产生多份中间帧，撤销量以真实上一份提交为准，不假定整轮32px。
                button(panel, "long-pause", cx).await?;
                let before_undo = wait_panel(panel, "撤销前暂停ACK", |s| s.phase == LongPhase::Paused
                    && !s.busy && !s.capture_in_flight && !s.auto.active, cx).await?;
                ensure(before_undo.can_undo && before_undo.height == manual.height, "连续滚动后没有真实撤销状态")?;
                *undo_check.borrow_mut() = serde_json::json!({"ok":false,"before":snapshot_log(&before_undo),"log":"等待撤销最后一次实际提交"});
                button(panel, "long-undo", cx).await?;
                let undone = wait_panel(panel, "连续滚动后真实撤销", |s| s.phase == LongPhase::Paused
                    && !s.busy && !s.capture_in_flight && s.height < before_undo.height, cx).await?;
                let undo_revision = preview_viewport(panel, &undone, previous_revision, cx).await?;
                check_fixture_pixels(panel, &fixture, undone.height, "撤销后精确保留此前条带", cx)?;
                (*undo_check.borrow_mut())["undone"] = snapshot_log(&undone);
                (*undo_check.borrow_mut())["log"] = serde_json::json!("撤销结果正确，等待单帧重采恢复");
                panel.update(cx, |v, _, _| v.session.as_mut().ok_or("无采集器")?.capture_once())
                    .map_err(|e| e.to_string())??;
                let restored = wait_panel(panel, "撤销后单帧重采恢复", |s| s.phase == LongPhase::Paused
                    && !s.busy && !s.capture_in_flight && s.width == manual.width && s.height == manual.height
                    && s.viewport == manual.viewport, cx).await?;
                let restore_revision = preview_viewport(panel, &restored, undo_revision, cx).await?;
                check_fixture_pixels(panel, &fixture, restored.height, "单帧重采后完整恢复像素", cx)?;
                *undo_check.borrow_mut() = serde_json::json!({"ok":true,"before":snapshot_log(&before_undo),
                    "undone":snapshot_log(&undone),"restored":snapshot_log(&restored),
                    "undo_revision":undo_revision,"restore_revision":restore_revision,
                    "exact_pixels":true,"fixture_received_wheels":fixture.wheel_count()});
                button(panel, "long-auto", cx).await?;
                wait_panel(panel, "自动滚动实际增长", |s| s.height > manual.height, cx).await?;
                button(panel, "long-pause", cx).await?;
                let paused = wait_panel(panel, "手动暂停ACK", |s| s.phase == LongPhase::Paused
                    && !s.busy && !s.capture_in_flight && !s.auto.active, cx).await?;
                let paused_wheels = fixture.wheel_count();
                cx.background_executor().timer(Duration::from_millis(750)).await;
                ensure(fixture.wheel_count() == paused_wheels, "暂停后仍发送滚轮")?;
                button(panel, "long-auto", cx).await?;
                let stopped = wait_panel(panel, "到边界只停止自动滚动", |s| s.phase == LongPhase::Running
                    && s.auto.phase == AutoPhase::NoMotion && !s.auto.active && !s.busy, cx).await?;
                let final_offset = SCROLL_LIMIT_STEPS * fixtures::STEP as u32;
                let maximum_auto_wheels = 2 * (SCROLL_LIMIT_STEPS - peak / fixtures::STEP as u32) + 4;
                ensure(stopped.height == region.height + final_offset
                    && stopped.viewport.y == final_offset
                    && fixture.origin() == (fixtures::ORIGIN, fixtures::ORIGIN + final_offset as i32)
                    && fixture.wheel_count() <= manual_steps as u64 + maximum_auto_wheels as u64,
                    "自动滚动没有完整增长或盲目重复滚动")?;
                button(panel, "long-pause", cx).await?;
                wait_panel(panel, "手动采集运行状态显式暂停后输出", |s| s.phase == LongPhase::Paused
                    && !s.busy && !s.capture_in_flight && !s.auto.active, cx).await?;
                let snapshot = panel.update(cx, |v, _, _| v.stitch_snapshot())
                    .map_err(|e| e.to_string())?.ok_or("停止后没有结果")?;
                let image = fixtures::stitched_image(&snapshot)?;
                let expected = fixtures::scene_for_pattern(fixture.pattern, region.width, region.height + final_offset,
                    ScrollAxis::Vertical, (0, 0), (fixtures::ORIGIN, fixtures::ORIGIN), 0);
                fixtures::same_image(&image, &expected, "长图像素/接缝/工具栏排除")?;
                // 限额内裁剪进入同窗剪辑，继续采集恢复原暂停会话、边框与侧预览。
                let closed_handle = panel.update(cx, |_, w, _| hwnd(w).0 as usize)
                    .map_err(|e| e.to_string())?;
                button(panel, "long-crop", cx).await?;
                let deadline = Instant::now() + WAIT;
                let editor = loop {
                    if let Some(editor) = cx.update(|cx| cx.windows().into_iter()
                        .find_map(|w| w.downcast::<CaptureView>())).map_err(|e| e.to_string())? {
                        break editor;
                    }
                    ensure(Instant::now() < deadline, "裁剪没有打开同窗长图编辑器")?;
                    step(cx).await;
                };
                crate::long_editor_smoke::prepare_clip_input(editor,"workflow-crop",cx).await?;
                let edited = editor.update(cx, |v, _, _| -> Result<_> {
                    ensure(v.editor.long_clip().is_some() && v.long_clip_ui.is_some(), "生产裁剪缺少同窗剪辑状态")?;
                    v.selected_content_image()
                }).map_err(|e| e.to_string())?;
                fixtures::same_image(&edited?, &image, "裁剪移交丢失长图")?;
                ensure(!unsafe { IsWindowVisible(HWND(preview_handle as *mut _)) }.as_bool(), "调整时侧预览仍覆盖屏幕")?;
                ensure(border_handles.iter().all(|h| !unsafe { IsWindowVisible(HWND(*h as *mut _)) }.as_bool()), "调整时边框仍覆盖屏幕")?;
                let deadline = Instant::now() + WAIT;
                loop {
                    let click = editor.update(cx, |v, w, _| v.ui_positions.borrow().get("long-clip-return").copied()
                        .map(|p| (Input { handle: hwnd(w).0 as usize, scale: w.scale_factor() }, p)))
                        .map_err(|e| e.to_string())?;
                    if let Some((input, position)) = click {
                        isolate_input(HWND(input.handle as *mut _))?;
                        input.click(position);
                        break;
                    }
                    ensure(Instant::now() < deadline, "同窗继续采集按钮没有绘制")?;
                    step(cx).await;
                }
                wait_panel(panel, "同窗继续采集保持暂停", |s| s.phase == LongPhase::Paused
                    && !s.auto.active && !s.busy, cx).await?;
                let deadline = Instant::now() + WAIT;
                while panel.update(cx, |v, _, _| v.editor_open).map_err(|e| e.to_string())? {
                    ensure(Instant::now() < deadline, "同窗长图编辑器没有返回")?;
                    step(cx).await;
                }
                ensure(border_handles.iter().all(|h| unsafe { IsWindowVisible(HWND(*h as *mut _)) }.as_bool()), "调整窗返回没有恢复选区边框")?;
                ensure(unsafe { IsWindowVisible(HWND(preview_handle as *mut _)) }.as_bool(), "调整窗返回没有恢复侧预览")?;
                button(panel, "long-close", cx).await?;
                let deadline = Instant::now() + WAIT;
                while active_workers() != 0 || unsafe { IsWindow(Some(HWND(closed_handle as *mut _))) }.as_bool() {
                    ensure(Instant::now() < deadline, "取消未回收GPU worker/窗口")?;
                    step(cx).await;
                }
                ensure(border_handles.iter().all(|h| !unsafe { IsWindow(Some(HWND(*h as *mut _))) }.as_bool()), "完成后残留选区边框")?;
                ensure(!unsafe { IsWindow(Some(HWND(preview_handle as *mut _))) }.as_bool(), "完成后残留侧预览")?;
                Ok(serde_json::json!({"ok":true,"primary_button":true,"frozen_window_closed":true,
                    "fixture":fixture.pattern.name(),"scroll_animation_frames":if fixture.pattern == FixturePattern::SparsePage {4} else {1},
                    "continuous_scroll_steps":continuous_scroll_steps.borrow().clone(),
                    "preview_scroll_history":continuous_scroll_steps.borrow().clone(),
                    "continuous_down_steps":CONTINUOUS_DOWN_STEPS,"continuous_reverse_steps":REVERSE_STEPS,
                    "continuous_resume_steps":CONTINUE_STEPS,"manual_received_wheels":manual_steps,
                    "final_scroll_offset":final_offset,"undo_after_continuous_scroll":undo_check.borrow().clone(),
                    "selection_border":true,"border_excluded_from_capture":true,"border_clickthrough":true,
                    "icon_primary_toolbar":true,"secondary_parameters_visible":true,"secondary_above_order":true,
                    "preview_on_right":true,"preview_width_dip":grown_preview.1,"preview_height_dip":grown_preview.2,
                    "preview_quality":preview_quality,
                    "manual_scroll_height":manual.height,"preview_revision_grew":true,"preview_gpu_framebuffer":"live-preview.png",
                    "capture_lock_transferred":true,"pause_height":paused.height,"pause_wheels":paused_wheels,
                    "auto_steps":stopped.auto.steps,"received_wheels":fixture.wheel_count(),
                    "auto_speed":stopped.auto.speed,
                    "manual_capture_kept_running":stopped.phase == LongPhase::Running,
                    "fixture_hwnd":fixture.native(),"width":image.width,"height":image.height,
                    "exact_pixels":true,"adjustment_window_handoff":true,"return_kept_paused":true,"workers":0}))
            }.await;
            let cleanup = cx.background_executor().spawn(async move {
                fixture.close().map_err(|e| e.to_string())
            }).await;
            let result = result.and_then(|value| cleanup.map(|_| value).map_err(Into::into));
            let report = result.as_ref().cloned().unwrap_or_else(|e|
                serde_json::json!({"ok":false,"error":e.to_string(),
                    "continuous_scroll_steps":continuous_scroll_steps.borrow().clone(),
                    "undo_after_continuous_scroll":undo_check.borrow().clone()}));
            let path = std::env::var_os("RUSTSHOT_LONG_WORKFLOW_REPORT")
                .map(std::path::PathBuf::from).unwrap_or_else(|| "target/qa/long-workflow.json".into());
            if let Err(e) = std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
                *errors.borrow_mut() = Some(e.to_string());
            }
            if let Err(e) = result { *errors.borrow_mut() = Some(e.to_string()); }
            let _ = cx.update(|cx| cx.quit());
        }).detach();
    });
}
