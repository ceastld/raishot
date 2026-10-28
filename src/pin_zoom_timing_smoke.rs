//! 截图→中键贴图后逐档滚轮缩放计时。后台线程约 0.25ms 轮询自有 HWND 的窗口矩形、
//! GPUI Present 回执与内容完整标记，不读像素、不移动真实指针；报告数值 JSONL 和 stdout 摘要。
//! “错位”指窗口已是新尺寸、带完整标记的新内容尚未 Present 的时长。
use super::{corner_probe, corner_qa, ensure, mark, qpc, synthetic, tick, PROBE};
use crate::{
    capture_view::CaptureView, gpui_capture::hwnd, gpui_smoke::Input, pin_native,
    pin_view::PinView, Result,
};
use gpui::{AppContext, AsyncApp, WeakEntity};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*},
};

#[path = "pin_zoom_composite_probe.rs"]
mod composite;

#[derive(Clone, Copy, PartialEq)]
struct State {
    rect: [i32; 4],
    serial: usize,
    present: (u32, u32),
    marker: u64,
}
struct Sample {
    qpc: i64,
    state: State,
}

fn pack(size: (u32, u32)) -> u64 {
    (u64::from(size.0) << 32) | u64::from(size.1)
}

fn zoom_geometry(
    pin: gpui::WindowHandle<PinView>,
    cx: &mut AsyncApp,
) -> Result<(f32, crate::viewport::Viewport)> {
    pin.update(cx, |v, _, cx| {
        let viewport = v
            .canvas
            .as_ref()
            .and_then(|canvas| {
                canvas
                    .read(cx)
                    .image_canvas
                    .as_ref()
                    .map(|image| image.viewport)
            })
            .ok_or("缩放计时专项画布没有内容投影")?;
        Ok((v.state.zoom, viewport))
    })?
}

/// 每次都从当前 HWND 反算客户区坐标，模拟鼠标留在同一屏幕点；不移动真实指针。
fn wheel_at_screen(input: Input, screen: (i32, i32), delta: i16) -> Result<()> {
    let actual = pin_native::rect(HWND(input.handle as *mut _))?;
    input.wheel(
        (
            (screen.0 - actual.x) as f32 / input.scale,
            (screen.1 - actual.y) as f32 / input.scale,
        ),
        delta,
    )
}

/// 圆角专项：首显、放大、缩小后分别读贴图自有帧缓冲四角；只要有一次角不透明即失败。
async fn corner_scenarios(
    pin: gpui::WindowHandle<PinView>,
    pin_hwnd: HWND,
    input: Input,
    marker: Arc<AtomicU64>,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    for _ in 0..30 {
        tick(cx).await;
    }
    let first = pin_native::rect(pin_hwnd)?;
    let pointer = (first.x + 40, first.y + 40);
    let mut stages = vec![serde_json::json!({
        "stage": "first_show",
        "rect": [first.width, first.height],
        "probe": corner_probe(pin, cx).await?,
    })];
    for (name, delta) in [
        ("zoom_in", 120i16),
        ("zoom_out", -120),
        ("zoom_out_again", -120),
    ] {
        let before = pin_native::rect(pin_hwnd)?;
        wheel_at_screen(input, pointer, delta)?;
        let deadline = Instant::now() + Duration::from_secs(4);
        let now = loop {
            tick(cx).await;
            let now = pin_native::rect(pin_hwnd)?;
            let pending = pin.update(cx, |v, _, _| v.placement_pending())?;
            if (now.width, now.height) != (before.width, before.height)
                && !pending
                && marker.load(Ordering::Acquire) == pack((now.width, now.height))
            {
                break now;
            }
            ensure(
                Instant::now() < deadline,
                "缩放后 4 秒内没有完整新内容 Present",
            )?;
        };
        for _ in 0..10 {
            tick(cx).await;
        }
        stages.push(serde_json::json!({
            "stage": name,
            "rect": [now.width, now.height],
            "probe": corner_probe(pin, cx).await?,
        }));
    }
    let rounded = stages.iter().all(|stage| stage["probe"]["rounded"] == true);
    let row = serde_json::json!({"ok":rounded,"phase":"pin_corner","stages":stages});
    pin.update(cx, |v, w, cx| {
        v.workspace = None;
        v.close_toolbar(cx);
        w.remove_window();
    })?;
    ensure(rounded, &format!("贴图圆角未生效：{row}"))?;
    Ok(row)
}

/// 只读本进程窗口属性与原子标记；不发消息，不与 UI 线程争用借用。
fn poll(handle: usize, marker: Arc<AtomicU64>, stop: Arc<AtomicBool>) -> (Vec<Sample>, u64) {
    unsafe {
        let _ = windows::Win32::Media::timeBeginPeriod(1);
    }
    let h = HWND(handle as *mut _);
    let mut samples: Vec<Sample> = Vec::new();
    let mut polls = 0_u64;
    while !stop.load(Ordering::Acquire) {
        let rect = pin_native::rect(h).ok();
        let serial = unsafe { GetPropW(h, w!("RustShot.GpuiPresentSerial")) }.0 as usize;
        let size = unsafe { GetPropW(h, w!("RustShot.GpuiPresentSize")) }.0 as usize as u32;
        if let (Some(rect), Ok(now)) = (rect, qpc()) {
            let state = State {
                rect: [rect.x, rect.y, rect.width as i32, rect.height as i32],
                serial,
                present: (size >> 16, size & 0xffff),
                marker: marker.load(Ordering::Acquire),
            };
            if samples.last().is_none_or(|last| last.state != state) {
                samples.push(Sample { qpc: now, state });
            }
        }
        polls += 1;
        std::thread::sleep(Duration::from_micros(250));
    }
    unsafe {
        let _ = windows::Win32::Media::timeEndPeriod(1);
    }
    (samples, polls)
}

fn refresh_hz(h: HWND) -> u32 {
    unsafe {
        let monitor = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return 0;
        }
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if EnumDisplaySettingsW(
            windows::core::PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
        {
            mode.dmDisplayFrequency
        } else {
            0
        }
    }
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values[((values.len() - 1) as f64 * p).round() as usize]
}

fn fixture(view: &mut CaptureView, request: (u32, u32)) -> Result<crate::geometry::Rect> {
    synthetic(view)?;
    let monitor = view.source.screen_rect(view.source.current);
    let width = request.0.min(monitor.width * 3 / 4).max(64);
    let height = request.1.min(monitor.height * 3 / 4).max(64);
    let crop = crate::geometry::Rect {
        x: monitor.x + (monitor.width - width) / 2,
        y: monitor.y + (monitor.height - height) / 2,
        width,
        height,
    };
    view.editor.set_crop(Some(crop));
    if corner_qa() {
        // 一块盖满并略超出选区的实心矩形：角上有标注，圆角仍须由整体内容组裁出。
        use crate::annotation::{Annotation, Point, Style, Tool};
        let mut cover = Annotation::new(
            Tool::Rectangle,
            Point {
                x: crop.x as f32 - 16.,
                y: crop.y as f32 - 16.,
            },
            Style {
                filled: true,
                color: 0xd83030,
                opacity: 1.,
                ..Default::default()
            },
        );
        cover.update(Point {
            x: (crop.x + crop.width) as f32 + 16.,
            y: (crop.y + crop.height) as f32 + 16.,
        });
        view.editor.document.insert(cover);
    } else {
        view.finish_style = Default::default();
        if composite::enabled() {
            // 竖条、横条标记供合成帧检查判断内容倍率；检查点所在的左上区域保持底色。
            use crate::annotation::{Annotation, Point, Style, Tool};
            let bar = |from: (f32, f32), to: (f32, f32)| {
                let mut bar = Annotation::new(
                    Tool::Rectangle,
                    Point {
                        x: from.0,
                        y: from.1,
                    },
                    Style {
                        filled: true,
                        color: composite::BAR_COLOR,
                        opacity: 1.,
                        ..Default::default()
                    },
                );
                bar.update(Point { x: to.0, y: to.1 });
                bar
            };
            let (x, y) = (crop.x as f32, crop.y as f32);
            let (w, h) = (crop.width as f32, crop.height as f32);
            let half = (
                composite::BAR_WIDTH_FRACTION * w / 2.,
                composite::BAR_WIDTH_FRACTION * h / 2.,
            );
            let at = (
                x + composite::BAR_FRACTION * w,
                y + composite::BAR_FRACTION * h,
            );
            view.editor
                .document
                .insert(bar((at.0 - half.0, y), (at.0 + half.0, y + h)));
            view.editor
                .document
                .insert(bar((x, at.1 - half.1), (x + w, at.1 + half.1)));
        }
    }
    Ok(crop)
}

pub(super) async fn verify(
    view: WeakEntity<CaptureView>,
    capture: gpui::AnyWindowHandle,
    original: usize,
    tid: u32,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    ensure(
        std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() == Ok("1")
            && std::env::var("RUSTSHOT_PIN_ZOOM_TIMING_QA").as_deref() == Ok("1"),
        "缺少缩放计时专项开关",
    )?;
    let settings = std::fs::canonicalize(crate::preferences::data_dir())?;
    ensure(
        settings.starts_with(std::fs::canonicalize("target/qa")?)
            && !crate::preferences::Preferences::load().history,
        "专项settings须隔离且history=false",
    )?;
    let request = std::env::var("RUSTSHOT_PIN_ZOOM_TIMING_SIZE")
        .ok()
        .and_then(|value| {
            let (w, h) = value.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1920, 1080));
    let crop = view.update(cx, |v, cx| {
        let crop = fixture(v, request);
        cx.notify();
        crop
    })??;
    cx.background_executor()
        .timer(Duration::from_millis(180))
        .await;
    let h = HWND(original as *mut _);
    let mut pid = 0;
    ensure(
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) } == tid
            && pid == std::process::id()
            && unsafe { IsWindow(Some(h)) }.as_bool(),
        "缩放计时专项原Capture身份改变",
    )?;
    crate::gpui_smoke::isolate_input(h)?;
    let x = crop.x + crop.width / 2;
    let y = crop.y + crop.height / 2;
    ensure(
        x <= i16::MAX as u32 && y <= i16::MAX as u32,
        "自有Middle消息坐标超出Win32有符号16位",
    )?;
    let packed = ((y << 16) | x) as isize;
    mark("native_post_pin");
    unsafe {
        PostMessageW(Some(h), WM_MBUTTONDOWN, WPARAM(0x10), LPARAM(packed))?;
        PostMessageW(Some(h), WM_MBUTTONUP, WPARAM(0), LPARAM(packed))?;
    }
    let end = Instant::now() + Duration::from_secs(12);
    let pin = loop {
        let ready = PROBE.with(|probe| {
            probe
                .borrow()
                .as_ref()
                .is_some_and(|probe| probe.ready.is_some())
        });
        let pins = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<PinView>())
                .collect::<Vec<_>>()
        })?;
        if ready && pins.len() == 1 {
            break pins[0];
        }
        ensure(Instant::now() < end, "原生中键路径没有完成唯一Pin首显")?;
        tick(cx).await;
    };
    ensure(
        cx.update_window(capture, |_, _, _| ()).is_err(),
        "首显回调完成后原截图窗口仍存在",
    )?;
    let (handle, scale, marker) = pin.update(cx, |v, w, cx| {
        (
            hwnd(w).0 as usize,
            w.scale_factor(),
            v.canvas
                .as_ref()
                .map(|canvas| canvas.read(cx).pin_content_drawn_size.clone()),
        )
    })?;
    let marker = marker.ok_or("截图贴图没有文档画布")?;
    let pin_hwnd = HWND(handle as *mut _);
    crate::gpui_smoke::isolate_input(pin_hwnd)?;
    let input = Input { handle, scale };
    for _ in 0..6 {
        tick(cx).await;
    }
    if corner_qa() {
        return corner_scenarios(pin, pin_hwnd, input, marker, cx).await;
    }
    ensure(
        pin.update(cx, |v, _, _| {
            v.pin_zoom_anchor == crate::pin_zoom::ZoomAnchor::Pointer
        })?,
        "缩放计时专项需使用鼠标位置锚点",
    )?;
    let initial = pin_native::rect(pin_hwnd)?;
    let pointer = (initial.x + 40, initial.y + 40);
    let (_, viewport) = zoom_geometry(pin, cx)?;
    let source_point = (
        (40. - f64::from(viewport.offset.x)) / f64::from(viewport.zoom),
        (40. - f64::from(viewport.offset.y)) / f64::from(viewport.zoom),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let (marker, stop) = (marker.clone(), stop.clone());
        std::thread::spawn(move || poll(handle, marker, stop))
    };
    // 合成帧检查点：鼠标点与缩放前中心的中点，放大、缩小（每档 ≥0.5 倍）后都仍在贴图内。
    let check_point = |rect: crate::pin_model::Placement| {
        (
            pointer.0 + (rect.x + rect.width as i32 / 2 - pointer.0) / 2,
            pointer.1 + (rect.y + rect.height as i32 / 2 - pointer.1) / 2,
        )
    };
    let dwm = composite::enabled().then(|| {
        let point = Arc::new(AtomicI64::new(composite::pack(check_point(initial))));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (point, stop) = (point.clone(), stop.clone());
            std::thread::spawn(move || composite::record(point, stop))
        };
        (point, stop, thread)
    });
    if dwm.is_some() {
        // 桌面复制首帧即当前画面，作为缩放前的参考帧。
        cx.background_executor()
            .timer(Duration::from_millis(250))
            .await;
    }
    // 逐档：放大 5 档、缩小 9 档、放回 4 档；最后一组 3 个滚轮连发测合并。
    let mut plan: Vec<Vec<i16>> = Vec::new();
    plan.extend(std::iter::repeat_n(vec![120], 5));
    plan.extend(std::iter::repeat_n(vec![-120], 9));
    plan.extend(std::iter::repeat_n(vec![120], 4));
    plan.push(vec![120, 120, 120]);
    let mut steps = Vec::new();
    let mut failure = None;
    for deltas in &plan {
        let before = pin_native::rect(pin_hwnd)?;
        let before_zoom = pin.update(cx, |v, _, _| v.state.zoom)?;
        if let Some((point, _, _)) = &dwm {
            point.store(composite::pack(check_point(before)), Ordering::Release);
        }
        let posted = qpc()?;
        for &delta in deltas {
            wheel_at_screen(input, pointer, delta)?;
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        let settled = loop {
            tick(cx).await;
            let now = pin_native::rect(pin_hwnd)?;
            let changed = (now.width, now.height) != (before.width, before.height);
            let pending = pin.update(cx, |v, _, _| v.placement_pending())?;
            if changed
                && !pending
                && marker.load(Ordering::Acquire) == pack((now.width, now.height))
            {
                break Some(now);
            }
            if Instant::now() >= deadline {
                break None;
            }
        };
        // 稳定后再留几帧，让下一档从空闲状态开始。
        for _ in 0..8 {
            tick(cx).await;
        }
        let (zoom, viewport) = zoom_geometry(pin, cx)?;
        let anchor_error = settled.map(|placement| {
            [
                (f64::from(placement.x)
                    + f64::from(viewport.offset.x)
                    + source_point.0 * f64::from(viewport.zoom)
                    - f64::from(pointer.0))
                .abs(),
                (f64::from(placement.y)
                    + f64::from(viewport.offset.y)
                    + source_point.1 * f64::from(viewport.zoom)
                    - f64::from(pointer.1))
                .abs(),
            ]
        });
        steps.push((
            posted,
            deltas.clone(),
            before,
            before_zoom,
            settled,
            zoom,
            anchor_error,
        ));
        if settled.is_none() {
            failure = Some("缩放后 4 秒内没有完整新内容 Present".to_owned());
            break;
        }
        // HWND 最终取整每轴至多半个物理像素；额外 0.01px 容纳源坐标投影的 f32 误差。
        if anchor_error.is_some_and(|error| error.into_iter().any(|axis| axis > 0.51)) {
            failure = Some(format!("鼠标下原图点逐档漂移：{anchor_error:?}px"));
            break;
        }
    }
    let stopped = qpc()?;
    stop.store(true, Ordering::Release);
    let (samples, polls) = poller.join().map_err(|_| "缩放计时轮询线程失败")?;
    let mut frequency = 0;
    ensure(
        unsafe { super::QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0,
        "缩放计时频率读取失败",
    )?;
    let edges =
        |p: crate::pin_model::Placement| [p.x, p.y, p.x + p.width as i32, p.y + p.height as i32];
    let composite_report = match dwm {
        Some((_, stop, thread)) => {
            stop.store(true, Ordering::Release);
            let recording = thread.join().map_err(|_| "合成帧检查线程失败")?;
            let steps: Vec<_> = steps
                .iter()
                .map(|step| composite::Step {
                    posted: step.0,
                    before: edges(step.2),
                    after: step.4.map(edges),
                })
                .collect();
            Some(composite::analyze(&recording, &steps, stopped, frequency))
        }
        None => None,
    };
    let ms = |from: i64, to: i64| (to - from) as f64 * 1000. / frequency as f64;
    let hz = refresh_hz(pin_hwnd);
    let period_ms = if hz > 0 { 1000. / f64::from(hz) } else { 0. };
    let stages: Vec<(String, i64)> = PROBE.with(|probe| {
        probe.borrow().as_ref().map_or_else(Vec::new, |probe| {
            probe
                .stages
                .iter()
                .filter_map(|value| {
                    let stage = value["stage"].as_str()?;
                    let at = value["qpc"].as_i64()?;
                    stage.starts_with("zoom_").then(|| (stage.to_owned(), at))
                })
                .collect()
        })
    });
    let mut rows = Vec::new();
    let (mut gaps, mut contents, mut resizes, mut frames) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut anchor_errors = Vec::new();
    for (index, (posted, deltas, before, before_zoom, settled, zoom, anchor_error)) in
        steps.iter().enumerate()
    {
        let until = steps.get(index + 1).map_or(stopped, |step| step.0);
        let window: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.qpc > *posted && s.qpc < until)
            .collect();
        let Some(target) = settled else {
            rows.push(serde_json::json!({"deltas":deltas,"settled":false}));
            continue;
        };
        let size = (target.width, target.height);
        if let Some(error) = anchor_error {
            anchor_errors.extend(error.iter().copied());
        }
        let first_rect = window
            .iter()
            .find(|s| {
                (s.state.rect[2], s.state.rect[3]) != (before.width as i32, before.height as i32)
            })
            .map(|s| s.qpc);
        let first_target_rect = window
            .iter()
            .find(|s| (s.state.rect[2] as u32, s.state.rect[3] as u32) == size)
            .map(|s| s.qpc);
        let first_present = window
            .iter()
            .find(|s| s.state.present == size)
            .map(|s| s.qpc);
        let content = window
            .iter()
            .find(|s| s.state.marker == pack(size))
            .map(|s| s.qpc);
        // 窗口已变尺寸但仍未有完整新内容期间，自有 GPUI Present 的次数。
        let stale_presents = match (first_rect, content) {
            (Some(rect), Some(content)) => {
                let serial_at = |at: i64| {
                    samples
                        .iter()
                        .take_while(|s| s.qpc <= at)
                        .last()
                        .map_or(0, |s| s.state.serial)
                };
                serial_at(content)
                    .saturating_sub(serial_at(rect))
                    .saturating_sub(1)
            }
            _ => 0,
        };
        let stage = |name: &str| {
            stages
                .iter()
                .filter(|(stage, at)| stage == name && *at > *posted && *at < until)
                .map(|(_, at)| ms(*posted, *at))
                .collect::<Vec<_>>()
        };
        let resize_ms = first_rect.map(|at| ms(*posted, at));
        let content_ms = content.map(|at| ms(*posted, at));
        let gap_ms = match (first_rect, content) {
            (Some(rect), Some(content)) => Some(ms(rect, content)),
            _ => None,
        };
        if let (Some(r), Some(c), Some(g)) = (resize_ms, content_ms, gap_ms) {
            resizes.push(r);
            contents.push(c);
            gaps.push(g);
            if period_ms > 0. {
                frames.push((g.max(0.) / period_ms).ceil());
            }
        }
        rows.push(serde_json::json!({
            "deltas":deltas,"zoom":[before_zoom,zoom],
            "rect_before":[before.x,before.y,before.width,before.height],
            "rect_after":[target.x,target.y,target.width,target.height],
            "pointer_source_error_physical_px":anchor_error,
            "wheel_to_resize_ms":resize_ms,
            "wheel_to_target_rect_ms":first_target_rect.map(|at| ms(*posted, at)),
            "wheel_to_first_present_at_size_ms":first_present.map(|at| ms(*posted, at)),
            "wheel_to_content_ms":content_ms,
            "resize_to_content_ms":gap_ms,
            "mismatch_refreshes":gap_ms.filter(|_| period_ms > 0.).map(|g| (g.max(0.) / period_ms).ceil()),
            "stale_presents":stale_presents,
            "stages_ms":{
                "place_begin":stage("zoom_place_begin"),"place_end":stage("zoom_place_end"),
                "prestage_begin":stage("zoom_prestage_begin"),"prestage_end":stage("zoom_prestage_end"),
                "body_sample_begin":stage("zoom_body_sample_begin"),"body_sample_end":stage("zoom_body_sample_end"),
                "body_staged_reuse":stage("zoom_body_staged_reuse"),
                "present_first_begin":stage("zoom_present_first_begin"),
                "presented_before_place":stage("zoom_presented_before_place"),
                "redrawn":stage("zoom_redrawn"),"shadow_synced":stage("zoom_shadow_synced"),
            },
        }));
    }
    let summary = |values: &[f64]| {
        let mut values = values.to_vec();
        serde_json::json!({
            "median":percentile(&mut values, 0.5),
            "p90":percentile(&mut values, 0.9),
            "max":values.iter().copied().fold(0_f64, f64::max),
        })
    };
    pin.update(cx, |v, w, cx| {
        v.workspace = None;
        v.close_toolbar(cx);
        w.remove_window();
    })?;
    let summary = serde_json::json!({
        "wheel_to_resize_ms":summary(&resizes),
        "wheel_to_content_ms":summary(&contents),
        "resize_to_content_ms":summary(&gaps),
        "mismatch_refreshes":summary(&frames),
        "pointer_source_error_physical_px":summary(&anchor_errors),
        "stale_presents":rows.iter().filter_map(|row| row["stale_presents"].as_u64()).sum::<u64>(),
    });
    println!(
        "{}",
        serde_json::json!({
            "phase":"pin_zoom_timing_summary","ok":failure.is_none(),
            "anchor":"pointer","pointer_screen":pointer,"refresh_hz":hz,
            "steps":rows.len(),"summary":summary,
            "composite":composite_report.as_ref().map(|report| serde_json::json!({
                "ok":report["ok"],"frames":report["frames"],"missing":report["missing"],
                "rect_mismatch":report["rect_mismatch"],"content_mismatch":report["content_mismatch"],
                "skipped_frames":report["skipped_frames"],"error":report["error"],
                "recording_error":report["recording_error"],
            })),
        })
    );
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(serde_json::json!({
        "phase":"pin_zoom_timing_complete","ok":true,"pid":std::process::id(),
        "crop":[crop.x,crop.y,crop.width,crop.height],"scale_factor":scale,
        "refresh_hz":hz,"poll":{"count":polls,"changes":samples.len()},
        "logical_processors":std::thread::available_parallelism().map_or(0, |n| n.get()),
        "anchor":"pointer","pointer_screen":pointer,
        "pointer_source":source_point,"pointer_axis_tolerance_physical_px":0.51,
        "summary":summary,
        "steps":rows,
        "composite":composite_report,
        "scope":"own HWND rect + GPUI present props + pin content marker; no pixel reads; posted wheel messages only",
    }))
}
