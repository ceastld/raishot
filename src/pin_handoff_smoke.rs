//! 真实 Action::Pin 的有限首显交接：合成全桌面源、自有消息与两个自有内容像素。
//! 只写数值 JSONL；仅读两个归属已验的自有合成内容屏幕点，不保存图像，不作为 <100ms 验收。
use crate::{
    canvas_source::CanvasSource,
    capture_view::CaptureView,
    editor::Editor,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::ScreenRect,
    gpui_capture::hwnd,
    pin_native,
    pin_view::PinView,
    Result,
};
use gpui::{App, AppContext, AsyncApp, Entity, RenderImage, WeakEntity, Window, WindowHandle};
use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        UI::{
            HiDpi::{
                AreDpiAwarenessContextsEqual, GetDpiForWindow, GetThreadDpiAwarenessContext,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
            WindowsAndMessaging::*,
        },
    },
};

#[path = "pin_zoom_timing_smoke.rs"]
mod zoom_timing;

const PROPERTY: windows::core::PCWSTR = w!("RustShot.PinHandoffSmoke.Owner");
const COLOR: [u8; 4] = [0x38, 0x74, 0xb8, 255];
struct Probe {
    capture: usize,
    capture_tid: u32,
    token: Rc<()>,
    stages: Vec<serde_json::Value>,
    ready: Option<serde_json::Value>,
}
thread_local! { static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) }; }
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}
fn qpc() -> Result<i64> {
    let mut value = 0;
    if unsafe { QueryPerformanceCounter(&mut value) } == 0 {
        return Err("贴图专项 QPC 失败".into());
    }
    Ok(value)
}
/// 未启动本专项时直接返回，不读钟、不分配诊断记录。
pub(crate) fn mark(stage: &str) {
    PROBE.with(|state| {
        if let Some(probe) = state.borrow_mut().as_mut() {
            match qpc() {
                Ok(qpc) => probe
                    .stages
                    .push(serde_json::json!({"stage":stage,"qpc":qpc})),
                Err(error) => probe
                    .stages
                    .push(serde_json::json!({"stage":stage,"clock_error":error.to_string()})),
            }
        }
    });
}
/// 生产回调在关闭原 Capture 之前调用；只记录原 HWND 和成功首显的标量证据。
pub(crate) fn observe_ready(ready: &pin_native::PinVisibleReady) {
    PROBE.with(|state| {
        if let Some(probe) = state.borrow_mut().as_mut() {
            let h = HWND(probe.capture as *mut _);
            let mut pid = 0;
            let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            let original_alive = pid == std::process::id() && tid == probe.capture_tid
                && unsafe { IsWindow(Some(h)) }.as_bool()
                && unsafe { GetPropW(h, PROPERTY) }.0 == Rc::as_ptr(&probe.token) as *mut _;
            probe.ready = Some(serde_json::json!({
                "qpc":qpc().ok(),"pin_hwnd":ready.hwnd,"pid":ready.process_id,"tid":ready.thread_id,
                "present_serial":ready.presented_serial,"dwm_flushed":ready.dwm_flushed,
                "rect":[ready.placement.x,ready.placement.y,ready.placement.width as i32,ready.placement.height as i32],
                "timing":ready.timing,"original_capture_alive_at_ready":original_alive,
                "original_capture_visible_at_ready":unsafe {IsWindowVisible(h)}.as_bool(),
                "occlusion_scope":"production gate checks content center; QA samples two owned content points"
            }));
        }
    });
}
fn ensure(value: bool, reason: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(reason.into())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(16))
        .await;
}
fn synthetic(view: &mut CaptureView) -> Result<([u32; 2], [u32; 4])> {
    let bounds = view.source.bounds;
    let current = view.source.current;
    ensure(
        bounds.width > 0
            && bounds.height > 0
            && bounds.width as u64 * bounds.height as u64 <= 32_000_000,
        "贴图专项虚拟桌面超过当前贴图合同",
    )?;
    let monitor = view.source.screen_rect(current);
    let screens = (0..view.source.screen_count())
        .map(|index| {
            let r = view.source.screen_rect(index);
            FrozenScreen {
                rect: ScreenRect {
                    left: bounds.left + r.x as i32,
                    top: bounds.top + r.y as i32,
                    width: r.width,
                    height: r.height,
                },
                preview: Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::from_pixel(r.width, r.height, image::Rgba(COLOR)),
                )])),
            }
        })
        .collect();
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        screens,
        bounds,
        current,
        capture_ms: 0.,
    });
    view.source = source;
    view.previews = previews;
    view.image_canvas = None;
    view.editor = Editor::new(view.source.full(), None);
    view.editor.set_crop(Some(view.source.full()));
    view.include_cursor = false;
    view.cursor = None;
    view.finish_style = if corner_qa() {
        crate::selection_finish::SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: CORNER_QA_RADIUS,
            ..Default::default()
        }
    } else {
        Default::default()
    };
    view.finish_effect_intent = None;
    view.finish_advanced = false;
    ensure(
        view.capture_lock.is_some(),
        "贴图专项要求真实截图 CaptureLock，不以图片编辑器替代",
    )?;
    Ok((
        [bounds.width, bounds.height],
        [monitor.x, monitor.y, monitor.width, monitor.height],
    ))
}
const CORNER_QA_RADIUS: f32 = 40.;
/// 显式开启圆角专项：截图开圆角后真实 Action::Pin，读贴图自有帧缓冲四角。
fn corner_qa() -> bool {
    std::env::var("RUSTSHOT_PIN_CORNER_QA").as_deref() == Ok("1")
}
/// 读贴图四角 48×48 的自有 GPU 帧（预乘 BGRA）：圆角外应全透明，内部应不透明。
async fn corner_probe(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    const N: u32 = 48;
    let h = pin.update(cx, |_, window, _| hwnd(window))?;
    let mut client = RECT::default();
    unsafe {
        GetClientRect(h, &mut client)?;
    }
    let (width, height) = (client.right as u32, client.bottom as u32);
    ensure(width > 2 * N && height > 2 * N, "贴图客户区过小，无法读取四角")?;
    let mut rows = Vec::new();
    for (name, x, y) in [
        ("top_left", 0, 0),
        ("top_right", width - N, 0),
        ("bottom_left", 0, height - N),
        ("bottom_right", width - N, height - N),
    ] {
        pin.update(cx, |_, w, _| w.request_frame_capture([x, y, N, N]))??;
        let mut frame = None;
        for _ in 0..80 {
            tick(cx).await;
            if let Some(result) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
                frame = Some(result?);
                break;
            }
        }
        let bytes = frame.ok_or("贴图四角帧未完成读回")?;
        ensure(bytes.len() == (N * N * 4) as usize, "贴图四角读回尺寸错误")?;
        let alpha = |px: u32, py: u32| bytes[((py * N + px) * 4 + 3) as usize];
        // 取靠近窗口角点与靠近内侧的像素；名称里的 outer/inner 以窗口角点为原点。
        let (corner, inner) = match name {
            "top_left" => ((1, 1), (N - 1, N - 1)),
            "top_right" => ((N - 2, 1), (0, N - 1)),
            "bottom_left" => ((1, N - 2), (N - 1, 0)),
            _ => ((N - 2, N - 2), (0, 0)),
        };
        rows.push(serde_json::json!({
            "corner": name,
            "alpha_at_window_corner": alpha(corner.0, corner.1),
            "alpha_inside": alpha(inner.0, inner.1),
        }));
    }
    // DWM 合成后的桌面像素：窗口角内 3px 若是圆角外侧应透出桌面，不等于贴图内部颜色。
    let placed = pin_native::rect(h)?;
    let (right, bottom) = (placed.x + placed.width as i32 - 1, placed.y + placed.height as i32 - 1);
    let center = (placed.x + placed.width as i32 / 2, placed.y + placed.height as i32 / 2);
    let dc = unsafe { GetDC(None) };
    ensure(!dc.is_invalid(), "专项屏幕点 DC 失败")?;
    let read = |x: i32, y: i32| {
        let color = unsafe { GetPixel(dc, x, y) };
        [(color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8]
    };
    let inside = read(center.0, center.1);
    let desktop = [
        ("top_left", read(placed.x + 3, placed.y + 3)),
        ("top_right", read(right - 3, placed.y + 3)),
        ("bottom_left", read(placed.x + 3, bottom - 3)),
        ("bottom_right", read(right - 3, bottom - 3)),
    ];
    unsafe {
        ReleaseDC(None, dc);
    }
    let desktop_rounded = desktop.iter().all(|(_, pixel)| *pixel != inside);
    let desktop_rows: Vec<_> = desktop
        .iter()
        .map(|(name, pixel)| serde_json::json!({"corner": name, "composited_rgb": pixel}))
        .collect();
    let rounded = desktop_rounded
        && rows.iter().all(|row| {
            row["alpha_at_window_corner"].as_u64() == Some(0)
                && row["alpha_inside"].as_u64() == Some(255)
        });
    Ok(serde_json::json!({
        "rounded": rounded, "radius_requested": CORNER_QA_RADIUS, "corners": rows,
        "composited_inside_rgb": inside, "composited_corners": desktop_rows,
        "composited_scope": "GDI GetPixel on screen DC after DWM; corner differs from pin inside colour",
    }))
}
fn owned_pin_pixel(
    pin: WindowHandle<PinView>,
    point: (i32, i32),
    cx: &mut AsyncApp,
) -> Result<[u8; 3]> {
    let h = pin.update(cx, |_, window, _| hwnd(window))?;
    let before_rect = pin_native::rect(h)?;
    let mut pid = 0;
    let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    ensure(
        pid == std::process::id()
            && tid != 0
            && unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 == h.0,
        "贴图像素原窗口身份不符",
    )?;
    let mut client = RECT::default();
    unsafe {
        GetClientRect(h, &mut client)?;
    }
    ensure(
        point.0 > 0 && point.1 > 0 && point.0 < client.right && point.1 < client.bottom,
        "像素不在贴图客户区内部",
    )?;
    let mut global = POINT {
        x: point.0,
        y: point.1,
    };
    unsafe {
        ClientToScreen(h, &mut global).ok()?;
    }
    ensure(
        unsafe { IsWindowVisible(h) }.as_bool() && unsafe { WindowFromPoint(global) } == h,
        "自有贴图内容像素被遮挡，停止读取",
    )?;
    // DComp 内容不保证存在于 HWND 的 GDI 重定向表面；仅读归属已验的全局点。
    let dc = unsafe { GetDC(None) };
    ensure(!dc.is_invalid(), "专项屏幕点 DC 失败")?;
    let color = unsafe { GetPixel(dc, global.x, global.y) };
    let released = unsafe { ReleaseDC(None, dc) };
    ensure(
        color.0 != u32::MAX && released == 1,
        "自有贴图 GetPixel/ReleaseDC 失败",
    )?;
    let same = pin.update(cx, |_, window, _| hwnd(window) == h)?;
    let mut final_pid = 0;
    let final_tid = unsafe { GetWindowThreadProcessId(h, Some(&mut final_pid)) };
    ensure(
        same && final_pid == pid
            && final_tid == tid
            && unsafe { IsWindowVisible(h) }.as_bool()
            && unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.0 == h.0
            && pin_native::rect(h)? == before_rect
            && unsafe { WindowFromPoint(global) } == h,
        "自有贴图像素读取期间身份/几何/覆盖改变",
    )?;
    Ok([(color.0 >> 16) as u8, (color.0 >> 8) as u8, color.0 as u8])
}
async fn verify(
    view: WeakEntity<CaptureView>,
    capture: gpui::AnyWindowHandle,
    original: usize,
    tid: u32,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let settings = std::fs::canonicalize(crate::preferences::data_dir())?;
    let qa = std::fs::canonicalize("target/qa")?;
    ensure(
        settings.starts_with(&qa),
        "贴图专项 DATA_DIR 必须预先隔离在 target/qa",
    )?;
    ensure(
        !crate::preferences::Preferences::load().history,
        "本轮贴图首显专项固定 history=false，不测历史落盘",
    )?;
    ensure(
        std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() == Ok("1"),
        "缺少专项计时开关",
    )?;
    let (dimensions, monitor) = view.update(cx, |v, cx| {
        let result = synthetic(v);
        cx.notify();
        result
    })??;
    // 合成源已完整建立；等待旧截图真实重画，随后只发送自有 T 键消息。
    cx.background_executor()
        .timer(Duration::from_millis(180))
        .await;
    let h = HWND(original as *mut _);
    let mut owner = 0;
    ensure(
        unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) } == tid
            && owner == std::process::id(),
        "原截图输入身份改变",
    )?;
    crate::gpui_smoke::isolate_input(h)?;
    mark("native_post_pin");
    unsafe {
        PostMessageW(Some(h), WM_KEYDOWN, WPARAM(0x54), LPARAM(0))?;
        PostMessageW(Some(h), WM_KEYUP, WPARAM(0x54), LPARAM(0))?;
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
        ensure(pins.len() <= 1, "一次确认创建了多个贴图窗口")?;
        if ready {
            if let Some(pin) = pins.first() {
                break *pin;
            }
        }
        ensure(Instant::now() < end, "真实 Action::Pin 未完成首显回调")?;
        tick(cx).await;
    };
    let (p, image_size) = pin.update(cx, |v, window, _| {
        (pin_native::rect(hwnd(window)), v.original_size)
    })?;
    let p = p?;
    ensure(
        image_size == (dimensions[0], dimensions[1]),
        "真实贴图源未保持全虚拟桌面尺寸",
    )?;
    let capture_gone = cx.update_window(capture, |_, _, _| ()).is_err();
    ensure(capture_gone, "首显回调完成后原截图窗口仍存在")?;
    // 点在已知显示器内部，避开虚拟桌面黑隙和所有贴图 chrome。
    let point = |numerator: u32| {
        (
            ((monitor[0] + monitor[2] * numerator / 4) as u64 * p.width as u64
                / dimensions[0] as u64) as i32,
            ((monitor[1] + monitor[3] / 2) as u64 * p.height as u64 / dimensions[1] as u64) as i32,
        )
    };
    let sample_begin = qpc()?;
    let pixels = [
        owned_pin_pixel(pin, point(1), cx)?,
        owned_pin_pixel(pin, point(3), cx)?,
    ];
    let sample_end = qpc()?;
    ensure(
        pixels.iter().all(|pixel| {
            pixel
                .iter()
                .zip(COLOR[..3].iter())
                .all(|(a, b)| a.abs_diff(*b) <= 5)
        }),
        "首显 Present 元数据通过但两个真实贴图内容像素不符",
    )?;
    let first = pin.update(cx, |_, window, _| {
        pin_native::first_show_probe(hwnd(window))
    })?;
    ensure(
        first.first_show_ready
            && first.first_show_serial > 0
            && !first.starting
            && first.visible
            && !first.aborted,
        "首显生产门没有保持成功",
    )?;
    let (ready, stages) = PROBE.with(|probe| {
        let probe = probe.borrow();
        let probe = probe.as_ref().expect("本轮专项状态");
        (probe.ready.clone(), probe.stages.clone())
    });
    let ready = ready.ok_or("专项缺少原生产 completion")?;
    ensure(
        ready["original_capture_alive_at_ready"] == true
            && ready["original_capture_visible_at_ready"] == true,
        "生产首显完成前原截图未被保留",
    )?;
    ensure(
        ready["dwm_flushed"] == true && ready["timing"].is_object(),
        "专项缺少 DWM/精确 QPC 回执",
    )?;
    for name in [
        "native_post_pin",
        "action_pin_entered",
        "selected_image_begin",
        "selected_image_end",
        "pin_open_begin",
        "pin_gpui_window_begin",
        "pin_gpui_window_end",
        "pin_shadow_begin",
        "pin_shadow_end",
        "pin_drop_begin",
        "pin_drop_end",
        "pin_workspace_begin",
        "pin_workspace_end",
        "pin_open_returned",
    ] {
        ensure(
            stages
                .iter()
                .filter(|value| value["stage"] == name && value["qpc"].is_i64())
                .count()
                == 1,
            "专项阶段缺失或重复",
        )?;
    }
    let mut frequency = 0;
    ensure(
        unsafe { QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0,
        "专项频率读取失败",
    )?;
    let trigger = stages
        .iter()
        .find(|value| value["stage"] == "native_post_pin")
        .and_then(|value| value["qpc"].as_i64())
        .ok_or("专项 trigger 缺失")?;
    let visible = ready["timing"]["visible_ready_qpc"]
        .as_i64()
        .ok_or("专项 visible QPC 缺失")?;
    ensure(
        visible >= trigger && visible <= sample_begin,
        "专项时间边界不单调",
    )?;
    ensure(
        ready["timing"]["frequency"].as_i64() == Some(frequency),
        "生产首显与专项 QPC 频率不一致",
    )?;
    let mut previous = trigger;
    for name in [
        "action_pin_entered",
        "selected_image_begin",
        "selected_image_end",
        "pin_open_begin",
        "pin_gpui_window_begin",
        "pin_gpui_window_end",
        "pin_shadow_begin",
        "pin_shadow_end",
        "pin_drop_begin",
        "pin_drop_end",
        "pin_workspace_begin",
        "pin_workspace_end",
        "pin_open_returned",
    ] {
        let value = stages
            .iter()
            .find(|value| value["stage"] == name)
            .and_then(|value| value["qpc"].as_i64())
            .ok_or("专项动作时钟缺失")?;
        ensure(value >= previous, "专项动作阶段时钟顺序错误")?;
        previous = value;
    }
    for name in [
        "placement_started_qpc",
        "hidden_placement_completed_qpc",
        "successful_present_observed_qpc",
        "show_started_qpc",
        "show_completed_qpc",
        "dwm_flush_completed_qpc",
        "visible_ready_qpc",
    ] {
        let value = ready["timing"][name]
            .as_i64()
            .ok_or("生产首显阶段时钟缺失")?;
        ensure(value >= previous, "生产首显阶段时钟顺序错误")?;
        previous = value;
    }
    let pin_hwnd = pin.update(cx, |_, window, _| hwnd(window).0 as usize)?;
    ensure(
        ready["pin_hwnd"].as_u64() == Some(pin_hwnd as u64)
            && ready["present_serial"].as_u64() == Some(first.first_show_serial as u64),
        "生产完成回执与实际像素采样原 Pin 身份/Present 不同",
    )?;
    let corner = if corner_qa() {
        Some(corner_probe(pin, cx).await?)
    } else {
        None
    };
    let mut row = serde_json::json!({"ok":true,"phase":"pin_handoff_complete","pid":std::process::id(),"frequency":frequency,
        "trigger_to_visible_ready_ms":(visible-trigger) as f64*1000./frequency as f64,
        "trigger_to_owned_pixels_ms":(sample_end-trigger) as f64*1000./frequency as f64,
        "pixel_sample_qpc":[sample_begin,sample_end],"pixel_samples":2,"desktop_pixel_reads":2,
        "pixel_read_scope":"two global screen points bound to original own Pin synthetic content; coverage check is not atomic",
        "pixel_oracle_pass":true,
        "ready":ready,"stages":stages,"first_show":first,"original_capture_closed_after_ready":capture_gone,
        "history_enabled":false,"source":"synthetic full virtual desktop; known monitor colors and black gaps",
        "launch_capture_scope":"normal startup capture before QA source replacement is outside this case's timing",
        "clipboard_touched":false,"global_input_sent":false,"saved_images":0,"hardware_input_verified":false,
        "dwm_scope":"calling application flush plus two finite owned content pixels; not an all-pixels unoccluded display proof",
        "performance_acceptance":false,"before_change_latency_measured":false});
    let corner_ok = corner.as_ref().is_none_or(|c| c["rounded"] == true);
    if let Some(corner) = corner {
        row["corner_probe"] = corner;
        row["ok"] = serde_json::Value::Bool(corner_ok);
    }
    // 只关闭本次成功 WindowHandle；应用退出是自有专项自然退出。
    pin.update(cx, |v, w, cx| {
        v.workspace = None;
        v.close_toolbar(cx);
        w.remove_window();
    })?;
    ensure(corner_ok, &format!("贴图圆角未生效：{}", row["corner_probe"]))?;
    Ok(row)
}
// 显式 QA 分支；旧 T 键/整桌面/两均色点专项默认保持原范围。
#[derive(Clone, Copy)]
struct AlignmentFixture {
    bounds: ScreenRect,
    crop: crate::geometry::Rect,
    pad: u32,
    expected: [i32; 4],
    display: usize,
    case: AlignmentCase,
    work: [i32; 4],
}
#[derive(Clone, Copy)]
enum AlignmentCase {
    Small25x31,
    Small27x25,
    Wide257x193,
    Pad1Edge,
    Pad3Wide,
}
impl AlignmentCase {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "small25x31" => Ok(Self::Small25x31),
            "small27x25" => Ok(Self::Small27x25),
            "wide257x193" => Ok(Self::Wide257x193),
            "pad1-edge" => Ok(Self::Pad1Edge),
            "pad3-wide" => Ok(Self::Pad3Wide),
            _ => Err("未知中键位置专项case".into()),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Small25x31 => "small25x31",
            Self::Small27x25 => "small27x25",
            Self::Wide257x193 => "wide257x193",
            Self::Pad1Edge => "pad1-edge",
            Self::Pad3Wide => "pad3-wide",
        }
    }
    fn shape(self) -> (u32, u32, u32) {
        match self {
            Self::Small25x31 => (25, 31, 0),
            Self::Small27x25 => (27, 25, 0),
            Self::Wide257x193 => (257, 193, 0),
            Self::Pad1Edge => (27, 25, 1),
            Self::Pad3Wide => (257, 193, 3),
        }
    }
}
fn alignment_pixel(x: u32, y: u32) -> [u8; 4] {
    // 灰度避免 RGBA/BGRA歧义；8×8 足以区分横/纵 ±1px，全部不透明。
    let value = if (u64::from(x) + 3 * u64::from(y)) % 7 < 3 {
        0
    } else {
        255
    };
    [value, value, value, 255]
}
fn alignment_fixture(view: &mut CaptureView, case: AlignmentCase) -> Result<AlignmentFixture> {
    let (width, height, pad) = case.shape();
    synthetic(view)?;
    let bounds = view.source.bounds;
    let count = view.source.screen_count();
    // 优先其它屏，不能假定其HDR状态；本专项仅读own framebuffer，不采桌面RGB。
    let display = (0..count)
        .rev()
        .find(|&index| {
            let screen = view.source.screen_rect(index);
            screen.width >= 420 && screen.height >= 360
        })
        .ok_or("没有容纳自有奇数选区的实际显示器")?;
    let monitor = view.source.screen_rect(display);
    let monitor_global = (
        i32::try_from(i64::from(bounds.left) + i64::from(monitor.x) + 61)?,
        i32::try_from(i64::from(bounds.top) + i64::from(monitor.y) + 67)?,
    );
    let work = pin_native::work_area(monitor_global);
    let content_x = work.x.checked_add(61).ok_or("专项内容x溢出")?;
    let content_y = if matches!(case, AlignmentCase::Pad1Edge) {
        work.y
    } else {
        work.y.checked_add(67).ok_or("专项内容y溢出")?
    };
    let crop = crate::geometry::Rect {
        x: u32::try_from(i64::from(content_x) - i64::from(bounds.left))?,
        y: u32::try_from(i64::from(content_y) - i64::from(bounds.top))?,
        width,
        height,
    };
    ensure(
        crop.valid(bounds.width, bounds.height),
        "专项选区不在冻结物理画布内",
    )?;
    ensure(
        crop.x >= monitor.x
            && crop.y >= monitor.y
            && u64::from(crop.x) + u64::from(width)
                <= u64::from(monitor.x) + u64::from(monitor.width)
            && u64::from(crop.y) + u64::from(height)
                <= u64::from(monitor.y) + u64::from(monitor.height),
        "专项内容必须完全位于同一实际显示器",
    )?;
    let origin_x = i32::try_from(i64::from(bounds.left) + i64::from(crop.x) - i64::from(pad))?;
    let origin_y = i32::try_from(i64::from(bounds.top) + i64::from(crop.y) - i64::from(pad))?;
    let expected = [
        origin_x,
        origin_y,
        (crop.width + 2 * pad) as i32,
        (crop.height + 2 * pad) as i32,
    ];
    ensure(
        i64::from(origin_x) >= i64::from(work.x)
            && i64::from(content_y) >= i64::from(work.y)
            && i64::from(origin_x) + i64::from(expected[2])
                <= i64::from(work.x) + i64::from(work.width)
            && i64::from(origin_y) + i64::from(expected[3])
                <= i64::from(work.y) + i64::from(work.height),
        "专项选区不在实际工作区，不能把钳位后的窗口反推为期望",
    )?;
    ensure(
        !matches!(case, AlignmentCase::Pad1Edge)
            || (content_y == work.y
                && origin_y == work.y.checked_sub(1).ok_or("工作区顶部减一溢出")?),
        "1px外边框边缘case必须独立预期在workTop上方1px，禁止期待钳位",
    )?;
    let screens = (0..count)
        .map(|index| {
            let r = view.source.screen_rect(index);
            let pixels = image::RgbaImage::from_fn(r.width, r.height, |x, y| {
                image::Rgba(alignment_pixel(r.x + x, r.y + y))
            });
            FrozenScreen {
                rect: ScreenRect {
                    left: bounds.left + r.x as i32,
                    top: bounds.top + r.y as i32,
                    width: r.width,
                    height: r.height,
                },
                preview: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
            }
        })
        .collect();
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        screens,
        bounds,
        current: display,
        capture_ms: 0.,
    });
    view.source = source;
    view.previews = previews;
    view.editor.set_crop(Some(crop));
    view.finish_style = crate::selection_finish::SelectionFinishStyle {
        border_enabled: pad != 0,
        border_thickness: pad as f32,
        border_inside: false,
        border_color: 0xffff_ffff,
        ..Default::default()
    };
    Ok(AlignmentFixture {
        bounds,
        crop,
        pad,
        expected,
        display,
        case,
        work: [work.x, work.y, work.width as i32, work.height as i32],
    })
}

struct AlignmentOwner {
    hwnd: HWND,
    tid: u32,
    token: Rc<()>,
}
impl AlignmentOwner {
    fn new(hwnd: HWND) -> Result<Self> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        ensure(
            pid == std::process::id() && tid != 0 && unsafe { IsWindow(Some(hwnd)) }.as_bool(),
            "专项Pin不属于原产品",
        )?;
        let token = Rc::new(());
        unsafe {
            SetPropW(
                hwnd,
                w!("RustShot.PinAlignment.Owner"),
                Some(HANDLE(Rc::as_ptr(&token) as *mut _)),
            )?;
        }
        let owner = Self { hwnd, tid, token };
        owner.check()?;
        Ok(owner)
    }
    fn check(&self) -> Result<()> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(self.hwnd, Some(&mut pid)) };
        ensure(
            pid == std::process::id()
                && tid == self.tid
                && tid != 0
                && unsafe { IsWindow(Some(self.hwnd)) }.as_bool()
                && unsafe { GetPropW(self.hwnd, w!("RustShot.Pin.v1")) }.0 == self.hwnd.0
                && unsafe { GetPropW(self.hwnd, w!("RustShot.PinAlignment.Owner")) }.0
                    == Rc::as_ptr(&self.token) as *mut _,
            "专项原Pin HWND/PID/TID/nonce已改变",
        )
    }
}
impl Drop for AlignmentOwner {
    fn drop(&mut self) {
        if self.check().is_ok() {
            unsafe {
                let _ = RemovePropW(self.hwnd, w!("RustShot.PinAlignment.Owner"));
            }
        }
    }
}

fn alignment_client(owner: &AlignmentOwner, expected: [i32; 4]) -> Result<serde_json::Value> {
    owner.check()?;
    ensure(
        unsafe {
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            )
        }
        .as_bool(),
        "专项客户区位置必须从原PMv2线程查询，不能冒称虚拟化矩形为物理像素",
    )?;
    let mut client = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        GetClientRect(owner.hwnd, &mut client)?;
        ClientToScreen(owner.hwnd, &mut origin).ok()?;
    }
    let outer = pin_native::rect(owner.hwnd)?;
    let actual = [
        origin.x,
        origin.y,
        client.right - client.left,
        client.bottom - client.top,
    ];
    owner.check()?;
    ensure(
        client.left == 0 && client.top == 0 && actual == expected,
        &format!("中键Pin物理客户区错位：actual={actual:?}, independently_expected={expected:?}"),
    )?;
    ensure(
        [outer.x, outer.y, outer.width as i32, outer.height as i32] == expected,
        "中键Pin外框不等于独立预期物理范围",
    )?;
    let center = POINT {
        x: origin.x + expected[2] / 2,
        y: origin.y + expected[3] / 2,
    };
    // 只保留opaque命中结果；不读取外国窗口属性，也不要求移走Picker。
    ensure(
        unsafe { IsWindowVisible(owner.hwnd) }.as_bool()
            && unsafe { WindowFromPoint(center) } == owner.hwnd
            && unsafe { WindowFromPhysicalPoint(center) } == owner.hwnd,
        "专项实际Pin中心被遮挡，保留失败，不放宽位置或像素门",
    )?;
    Ok(
        serde_json::json!({"owner_pid":std::process::id(),"owner_tid":owner.tid,
        "owner_hwnd":owner.hwnd.0 as usize,"client_global":actual,"outer_global":[outer.x,outer.y,outer.width,outer.height],
        "expected_global":expected,"delta":[0,0,0,0],"center_two_hits_own":true,
        "dpi":unsafe {GetDpiForWindow(owner.hwnd)},"owner_verified":true}),
    )
}

async fn verify_alignment(
    view: WeakEntity<CaptureView>,
    capture: gpui::AnyWindowHandle,
    original: usize,
    tid: u32,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    ensure(
        std::env::var("RUSTSHOT_PIN_HANDOFF_QA").as_deref() == Ok("1")
            && std::env::var("RUSTSHOT_PIN_ALIGNMENT_QA").as_deref() == Ok("1"),
        "缺少显式中键位置专项开关",
    )?;
    let settings = std::fs::canonicalize(crate::preferences::data_dir())?;
    ensure(
        settings.starts_with(std::fs::canonicalize("target/qa")?)
            && !crate::preferences::Preferences::load().history,
        "专项settings须隔离且history=false",
    )?;
    let case = AlignmentCase::parse(&std::env::var("RUSTSHOT_PIN_ALIGNMENT_CASE")?)?;
    let fixture = view.update(cx, |v, cx| {
        let fixture = alignment_fixture(v, case);
        cx.notify();
        fixture
    })??;
    cx.background_executor()
        .timer(Duration::from_millis(180))
        .await;
    let h = HWND(original as *mut _);
    let mut pid = 0;
    let nonce_matches = PROBE.with(|probe| {
        probe.borrow().as_ref().is_some_and(|probe| {
            probe.capture == original
                && probe.capture_tid == tid
                && unsafe { GetPropW(h, PROPERTY) }.0 == Rc::as_ptr(&probe.token) as *mut _
        })
    });
    ensure(
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) } == tid
            && pid == std::process::id()
            && unsafe { IsWindow(Some(h)) }.as_bool()
            && nonce_matches,
        "中键专项原Capture身份改变",
    )?;
    crate::gpui_smoke::isolate_input(h)?;
    // 所有坐标从独立fixture获得，未移动全局指针；原截图MiddleDown走真实Action::Pin。
    let x = fixture.crop.x + fixture.crop.width / 2;
    let y = fixture.crop.y + fixture.crop.height / 2;
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
    let own_hwnd = pin.update(cx, |_, w, _| hwnd(w))?;
    let owner = AlignmentOwner::new(own_hwnd)?;
    let before = alignment_client(&owner, fixture.expected)?;
    let expected_size = (fixture.expected[2] as u32, fixture.expected[3] as u32);
    let cpu_pixels = pin.update(cx, |v, w, _| -> Result<_> {
        ensure(
            v.original_size == expected_size && v.image_size == expected_size && v.state.zoom == 1.,
            "中键专项图片/100%缩放与独立奇数尺寸不同",
        )?;
        let viewport = w.viewport_size();
        let scale = w.scale_factor();
        ensure(
            (f32::from(viewport.width) * scale).round() as u32 == expected_size.0
                && (f32::from(viewport.height) * scale).round() as u32 == expected_size.1,
            "中键专项GPUI viewport与独立物理客户区不同",
        )?;
        let bytes = v.original.as_bytes(0).ok_or("中键Pin原始冻结图缺失")?;
        ensure(
            bytes.len() == expected_size.0 as usize * expected_size.1 as usize * 4,
            "中键Pin原图长度不同",
        )?;
        for y in 0..fixture.crop.height {
            for x in 0..fixture.crop.width {
                let offset = (((y + fixture.pad) * expected_size.0 + x + fixture.pad) * 4) as usize;
                ensure(
                    bytes[offset..offset + 4]
                        == alignment_pixel(fixture.crop.x + x, fixture.crop.y + y),
                    "中键selected_image内容texel不等于独立冻结选区",
                )?;
            }
        }
        Ok((
            fixture.crop.width as u64 * fixture.crop.height as u64,
            scale,
        ))
    })??;
    let mut samples = Vec::new();
    let sample_begin = qpc()?;
    for (x, y) in [
        (1, 1),
        (fixture.crop.width / 2 - 4, fixture.crop.height / 2 - 4),
        (fixture.crop.width - 9, fixture.crop.height - 9),
    ] {
        let roi = [fixture.pad + x, fixture.pad + y, 8, 8];
        alignment_client(&owner, fixture.expected)?;
        pin.update(cx, |_, w, cx| {
            w.request_frame_capture(roi)?;
            cx.notify();
            Ok::<_, Box<dyn std::error::Error>>(())
        })??;
        let end = Instant::now() + Duration::from_secs(5);
        let bytes = loop {
            owner.check()?;
            tick(cx).await;
            if let Some(result) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
                break result.map_err(|error| error.to_string())?;
            }
            ensure(Instant::now() < end, "中键Pin self framebuffer小ROI超时")?;
        };
        ensure(bytes.len() == 8 * 8 * 4, "中键Pin self framebuffer长度不同")?;
        let mut max_difference = 0;
        for (index, pixel) in bytes.chunks_exact(4).enumerate() {
            let expected = alignment_pixel(
                fixture.crop.x + x + index as u32 % 8,
                fixture.crop.y + y + index as u32 / 8,
            );
            for (&actual, expected) in pixel.iter().zip(expected) {
                max_difference = max_difference.max(actual.abs_diff(expected));
            }
        }
        ensure(
            max_difference <= 1,
            &format!("中键Pin texel物理错位：ROI={roi:?}, max_difference={max_difference}"),
        )?;
        let after = alignment_client(&owner, fixture.expected)?;
        samples.push(serde_json::json!({"roi":roi,"texels":64,"max_difference":max_difference,
            "tolerance":1,"expected_source_canvas":[fixture.crop.x+x,fixture.crop.y+y],"after":after}));
    }
    let sample_end = qpc()?;
    let mut frequency = 0;
    ensure(
        unsafe { QueryPerformanceFrequency(&mut frequency) } != 0 && frequency > 0,
        "中键专项QPC频率未知",
    )?;
    let first = pin_native::first_show_probe(owner.hwnd);
    let (ready, stages) = PROBE.with(|probe| {
        let probe = probe.borrow();
        let probe = probe.as_ref().expect("alignment probe");
        (probe.ready.clone(), probe.stages.clone())
    });
    let ready = ready.ok_or("中键专项缺少原生产回执")?;
    ensure(
        first.first_show_ready
            && first.visible
            && !first.starting
            && !first.aborted
            && ready["pin_hwnd"].as_u64() == Some(owner.hwnd.0 as u64)
            && ready["pid"].as_u64() == Some(std::process::id() as u64)
            && ready["tid"].as_u64() == Some(owner.tid as u64)
            && ready["dwm_flushed"] == true
            && ready["original_capture_alive_at_ready"] == true
            && ready["original_capture_visible_at_ready"] == true,
        "中键Pin原生产身份/Present/DWM/原Capture保留门不同",
    )?;
    let capture_gone = cx.update_window(capture, |_, _, _| ()).is_err();
    ensure(capture_gone, "中键Pin Ready后原Capture未退出")?;
    let after = alignment_client(&owner, fixture.expected)?;
    let row = serde_json::json!({"phase":"pin_alignment_complete","ok":true,"pid":std::process::id(),
        "alignment_case":fixture.case.name(),"actual_work_area":fixture.work,"fixed_zoom":1,
        "frequency":frequency,"self_framebuffer_sample_qpc":[sample_begin,sample_end],
        "trigger":"owned_WM_MBUTTONDOWN_UP","fixture_bounds":[fixture.bounds.left,fixture.bounds.top,fixture.bounds.width,fixture.bounds.height],
        "crop_local":[fixture.crop.x,fixture.crop.y,fixture.crop.width,fixture.crop.height],"finish_padding":[fixture.pad,fixture.pad,fixture.pad,fixture.pad],
        "expected_client_global":fixture.expected,"display_index":fixture.display,"before":before,"after":after,
        "cpu_content_texels_checked":cpu_pixels.0,"gpui_scale":cpu_pixels.1,"self_framebuffer_samples":samples,
        "self_framebuffer_texels_checked":192,"independent_expected_not_from_actual_rect":true,
        "geometry_error_tolerance_px":0,"pixel_tolerance":1,"original_capture_closed_after_ready":capture_gone,
        "ready":ready,"first_show":first,"stages":stages,"desktop_pixel_reads":0,"saved_images":0,
        "clipboard_touched":false,"global_input_sent":false,"history_enabled":false,"performance_acceptance":false,
        "hdr_legacy_screen_rgb_used":false,"scope":"own synthetic freeze to real middle-button Pin; exact physical client and own self framebuffer; no hardware assertion"});
    owner.check()?;
    pin.update(cx, |v, w, cx| {
        v.workspace = None;
        v.close_toolbar(cx);
        w.remove_window();
    })?;
    Ok(row)
}

pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    let alignment = std::env::var("RUSTSHOT_PIN_ALIGNMENT_QA").as_deref() == Ok("1");
    let zoom_timing = std::env::var("RUSTSHOT_PIN_ZOOM_TIMING_QA").as_deref() == Ok("1");
    let phase = if zoom_timing {
        "pin_zoom_timing"
    } else if alignment {
        "pin_alignment"
    } else {
        "pin_handoff"
    };
    *errors.borrow_mut() = Some("贴图交接专项未完成".into());
    let weak = view.downgrade();
    let capture = window.window_handle();
    let h = hwnd(window);
    let handle = h.0 as usize;
    let mut owner = 0;
    let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
    let setup = (|| -> Result<PathBuf> {
        ensure(
            owner == std::process::id() && tid != 0,
            "贴图专项初始窗口身份不符",
        )?;
        let token = Rc::new(());
        unsafe {
            SetPropW(h, PROPERTY, Some(HANDLE(Rc::as_ptr(&token) as *mut _)))?;
        }
        let folder = PathBuf::from("target/qa/pin-handoff");
        std::fs::create_dir_all(&folder)?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = folder.join(format!("{}-{nonce}.jsonl", std::process::id()));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        PROBE.with(|probe| {
            *probe.borrow_mut() = Some(Probe {
                capture: handle,
                capture_tid: tid,
                token,
                stages: Vec::new(),
                ready: None,
            })
        });
        Ok(path)
    })();
    window.on_next_frame(move|_,cx|{
        cx.spawn(async move|cx|{
            let (path,result)=match setup {Ok(path)=>{let result=if zoom_timing {zoom_timing::verify(weak,capture,handle,tid,cx).await} else if alignment {verify_alignment(weak,capture,handle,tid,cx).await} else {verify(weak,capture,handle,tid,cx).await};(Some(path),result)},Err(error)=>(None,Err(error))};
            let mut error=result.as_ref().err().map(|error|error.to_string());
            if let Some(path)=path {
                let row=match result {Ok(row)=>row,Err(_)=>serde_json::json!({"phase":format!("{phase}_complete"),"ok":false,"error":error,"performance_acceptance":false})};
                let write=(||->Result<()>{let mut file=OpenOptions::new().append(true).open(&path)?;serde_json::to_writer(&mut file,&row)?;file.write_all(b"\n")?;file.flush()?;Ok(())})();
                if let Err(write_error)=write {error=Some(format!("贴图专项记录失败：{write_error}"));}
                println!("{}",serde_json::json!({"phase":format!("{phase}_artifact"),"path":path,"ok":error.is_none()}));
            }
            PROBE.with(|probe|{if let Some(probe)=probe.borrow_mut().take(){let h=HWND(probe.capture as *mut _);if unsafe {GetPropW(h,PROPERTY)}.0==Rc::as_ptr(&probe.token) as *mut _{unsafe {let _=RemovePropW(h,PROPERTY);}}}});
            *errors.borrow_mut()=error;let _=cx.update(|cx|cx.quit());
        }).detach();
    });
}
