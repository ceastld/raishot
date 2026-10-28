//! 贴图首显与自有阴影条带专项；不读取桌面、剪贴板或移动系统鼠标。
use crate::{
    capture::Image,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    long_output::{self, LongOutcome, LongOutput},
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_view::{PinAction, PinView},
    stitch::{ScrollAxis, StitchSegment, StitchSnapshot},
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, App, AppContext, Application, AsyncApp, Bounds,
    Context, Render, RenderImage, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        System::Threading::GetCurrentThreadId,
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
};

const WAIT: Duration = Duration::from_secs(20);
const PROPERTY: windows::core::PCWSTR = w!("RustShot.PinPresentation.Owned");
const EVENT_CREATE: u32 = 0x8000;
const EVENT_SHOW: u32 = 0x8002;
type EventCallback = unsafe extern "system" fn(isize, u32, HWND, i32, i32, u32, u32);
#[link(name = "user32")]
extern "system" {
    fn SetWinEventHook(
        first: u32,
        last: u32,
        module: isize,
        callback: Option<EventCallback>,
        pid: u32,
        tid: u32,
        flags: u32,
    ) -> isize;
    fn UnhookWinEvent(hook: isize) -> i32;
}
thread_local! {
    static EVENT_SINK: RefCell<Option<Rc<RefCell<Vec<Value>>>>> = const { RefCell::new(None) };
}
unsafe extern "system" fn event_callback(
    _: isize,
    event: u32,
    h: HWND,
    object: i32,
    child: i32,
    tid: u32,
    event_ms: u32,
) {
    if object != 0 || child != 0 || h.is_invalid() {
        return;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if pid != std::process::id() {
        return;
    }
    let mut title = [0u16; 256];
    let length = GetWindowTextW(h, &mut title).max(0) as usize;
    let probe = native::first_show_probe(h);
    let value = json!({"event":event,"hwnd":h.0 as usize,"pid":pid,"tid":tid,"event_tick_ms":event_ms,
        "observed_unix_ms":unix_ms(),"title":String::from_utf16_lossy(&title[..length]),"visible":IsWindowVisible(h).as_bool(),
        "style":GetWindowLongPtrW(h,GWL_STYLE) as u32,"pin_mark":GetPropW(h,w!("RustShot.Pin.v1")).0 as usize,"first_show":probe});
    EVENT_SINK.with(|sink| {
        if let Some(sink) = sink.borrow().as_ref() {
            let mut events = sink.borrow_mut();
            if events.len() < 256 {
                events.push(value);
            }
        }
    });
}
struct Events {
    hook: isize,
    rows: Rc<RefCell<Vec<Value>>>,
}
impl Events {
    fn start() -> Result<Self> {
        let rows = Rc::new(RefCell::new(Vec::new()));
        EVENT_SINK.with(|sink| *sink.borrow_mut() = Some(rows.clone()));
        // OUTOFCONTEXT，且只限本工作进程；它观测消息，不发送硬件输入。
        let hook = unsafe {
            SetWinEventHook(
                EVENT_CREATE,
                EVENT_SHOW,
                0,
                Some(event_callback),
                std::process::id(),
                0,
                0,
            )
        };
        ensure(hook != 0, "自有窗口 WinEvent 观测器创建失败")?;
        Ok(Self { hook, rows })
    }
    fn rows(&self) -> Vec<Value> {
        self.rows.borrow().clone()
    }
}
impl Drop for Events {
    fn drop(&mut self) {
        unsafe {
            UnhookWinEvent(self.hook);
        }
        EVENT_SINK.with(|sink| *sink.borrow_mut() = None);
    }
}
struct KeepAlive;
impl Render for KeepAlive {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}

const BORDER_AA_ROI: [u32; 4] = [16, 16, 176, 176];
#[derive(Clone, Copy)]
struct BorderAaCase {
    radius: f32,
    inside: bool,
    thickness: f32,
    zoom: f64,
    phase: f64,
    thin: bool,
}
impl BorderAaCase {
    fn style(self) -> crate::selection_finish::SelectionFinishStyle {
        crate::selection_finish::SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: self.radius,
            border_enabled: true,
            border_inside: self.inside,
            border_thickness: self.thickness,
            border_color: 0xffff_ffff,
            ..Default::default()
        }
    }
    fn origin(self) -> crate::pin_presentation::PhysicalPoint {
        crate::pin_presentation::PhysicalPoint {
            x: 24. + self.phase,
            y: 24. + self.phase,
        }
    }
    fn plan(self) -> Result<crate::pin_presentation::PresentationPreviewPlan> {
        crate::pin_presentation::PresentationPreviewPlan::new(
            Rect::full(100, 100),
            self.style(),
            self.origin(),
            self.zoom,
        )
    }
}
struct BorderAaFixture {
    plan: crate::pin_presentation::PresentationPreviewPlan,
}
fn border_aa_default_path_control(window: &mut Window, rect: [f32; 4]) {
    let dpi = window.scale_factor();
    let [left, top, right, bottom] = rect;
    let at = |x, y| point(px(x / dpi), px(y / dpi));
    let mut path = gpui::PathBuilder::fill();
    path.move_to(at(left, top));
    path.line_to(at(right, top));
    path.line_to(at(right, bottom));
    path.line_to(at(left, bottom));
    path.close();
    if let Ok(path) = path.build() {
        // 普通 paint_path 保持默认 MSAA，与边框的新路径渲染模式前后交错。
        window.paint_path(path, gpui::rgba(0xffff_ff80));
    }
}
impl Render for BorderAaFixture {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let background = crate::theme::palette(cx).background;
        let plan = self.plan;
        div().size_full().child(
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    let dpi = window.scale_factor();
                    let viewport = window.viewport_size();
                    let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
                    let [x, y, w, h] = BORDER_AA_ROI.map(|v| v as f32 / dpi);
                    // 只有采样孔透明；外围跟随主题。读回 alpha，避免桌面合成及 sRGB 推断。
                    for (left, top, w, h) in [
                        (0., 0., width, y),
                        (0., y, x, h),
                        (x + w, y, (width - x - w).max(0.), h),
                        (0., y + h, width, (height - y - h).max(0.)),
                    ] {
                        window.paint_quad(gpui::fill(
                            Bounds::new(point(px(left), px(top)), size(px(w), px(h))),
                            rgb(background),
                        ));
                    }
                    border_aa_default_path_control(window, [180., 178., 184., 182.]);
                    crate::pin_presentation::paint_border(plan, point(px(0.), px(0.)), window);
                    border_aa_default_path_control(window, [186., 184., 190., 188.]);
                },
            )
            .size_full(),
        )
    }
}

async fn border_aa_capture(
    fixture: WindowHandle<BorderAaFixture>,
    owned: &Owned,
    cx: &mut AsyncApp,
) -> Result<Vec<u8>> {
    owned.check()?;
    fixture.update(cx, |_, window, cx| {
        window.request_frame_capture(BORDER_AA_ROI)?;
        cx.notify();
        Ok::<_, Box<dyn std::error::Error>>(())
    })??;
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        owned.check()?;
        tick(cx).await;
        if let Some(result) = fixture.update(cx, |_, window, _| window.take_frame_capture())? {
            return result.map_err(|error| error.to_string().into());
        }
        ensure(Instant::now() < end, "圆角边框实际 GPU ROI 读回超时")?;
    }
}

fn border_aa_measure(case: BorderAaCase, pixels: &[u8]) -> Result<Value> {
    let [left, top, width, height] = BORDER_AA_ROI;
    ensure(
        pixels.len() == (width * height * 4) as usize,
        "圆角边框 GPU ROI 长度不同",
    )?;
    let origin = case.origin();
    let geometry = case.style().geometry(100, 100);
    let (outer, _) = case.plan()?.border_contours();
    let mut levels = BTreeSet::new();
    let mut arc_pixels = 0_u32;
    let mut missing = 0_u32;
    let mut error_squared = 0.;
    let mut max_error: f64 = 0.;
    for y in 0..height {
        for x in 0..width {
            let px = f64::from(left + x) + 0.5;
            let py = f64::from(top + y) + 0.5;
            if px >= 178. && py >= 174. {
                continue;
            }
            let dx = (px - outer.bounds.left).min(outer.bounds.left + outer.bounds.width - px);
            let dy = (py - outer.bounds.top).min(outer.bounds.top + outer.bounds.height - py);
            if dx > outer.radius_x + 1. || dy > outer.radius_y + 1. {
                continue;
            }
            let alpha = pixels[((y * width + x) * 4 + 3) as usize];
            let actual = f64::from(alpha) / 255.;
            let expected = geometry.border_coverage_at(
                (px - origin.x) / case.zoom,
                (py - origin.y) / case.zoom,
                case.inside,
                f64::from(case.thickness),
                1. / case.zoom,
                1. / case.zoom,
            );
            if alpha > 1 && alpha < 254 {
                levels.insert(alpha);
            }
            if expected > 0.01 || actual > 0.01 {
                arc_pixels += 1;
                error_squared += (actual - expected).powi(2);
                max_error = max_error.max((actual - expected).abs());
                if expected >= 0.3 && actual <= 0.03 {
                    missing += 1;
                }
            }
        }
    }
    let projected_width = f64::from(case.thickness) * case.zoom;
    let (start, end) = if case.inside {
        (origin.x, origin.x + projected_width)
    } else {
        (origin.x - projected_width, origin.x)
    };
    let row = (origin.y + 50. * case.zoom).floor() as u32;
    let mut coverage_sum = 0.;
    for x in start.floor() as i32 - 3..end.ceil() as i32 + 3 {
        ensure(
            x >= left as i32 && x < (left + width) as i32,
            "直边采样越出自有 ROI",
        )?;
        let offset = (((row - top) * width + x as u32 - left) * 4 + 3) as usize;
        coverage_sum += f64::from(pixels[offset]) / 255.;
    }
    let width_error = (coverage_sum - projected_width).abs();
    let mut controls = Vec::new();
    let mut controls_ok = true;
    for (position, x, y) in [
        ("before-border", 182_u32, 180_u32),
        ("after-border", 188, 186),
    ] {
        let offset = (((y - top) * width + x - left) * 4) as usize;
        let actual = &pixels[offset..offset + 4];
        let ok = actual.iter().all(|value| value.abs_diff(128) <= 1);
        controls_ok &= ok;
        controls.push(json!({"position":position,"physical_pixel":[x,y],"actual_bgra":actual,
            "expected_bgra":[128,128,128,128],"tolerance":1,"default_MSAA_paint_path":true,"ok":ok}));
    }
    let ok = (case.thin || levels.len() >= 16)
        && missing == 0
        && controls_ok
        && width_error <= if case.thin { 0.02 } else { 0.08 };
    Ok(
        json!({"ok":ok,"radius":case.radius,"inside":case.inside,"thickness":case.thickness,
        "zoom":case.zoom,"phase":case.phase,"subpixel_case":case.thin,"roi":BORDER_AA_ROI,
        "arc_partial_alpha_levels":levels,"arc_partial_alpha_level_count":levels.len(),
        "arc_pixels":arc_pixels,"missing_arc_pixels":missing,
        "arc_alpha_rmse":(error_squared / f64::from(arc_pixels.max(1))).sqrt(),
        "arc_max_alpha_error":max_error,"reference":"CPU contour area; actual from GPU framebuffer",
        "straight_expected_coverage":projected_width,"straight_actual_coverage":coverage_sum,
        "straight_coverage_error":width_error,"straight_error_limit":if case.thin {0.02} else {0.08},
        "default_path_controls":controls,"default_path_controls_ok":controls_ok,
        "pixel_backend":"owned_gpu_framebuffer_premultiplied_bgra","metric_channel":"alpha",
        "production_painter":true,"desktop_read":false,"sRGB_color_inference":false}),
    )
}

async fn border_aa_case(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let mut cases = Vec::new();
    for radius in [8., 20.] {
        for inside in [false, true] {
            for thickness in [1., 3.] {
                for zoom in [1., 1.375] {
                    for phase in [0.25, 0.75] {
                        cases.push(BorderAaCase {
                            radius,
                            inside,
                            thickness,
                            zoom,
                            phase,
                            thin: false,
                        });
                    }
                }
            }
        }
    }
    for inside in [false, true] {
        for phase in [0.25, 0.75] {
            cases.push(BorderAaCase {
                radius: 8.,
                inside,
                thickness: 1.25,
                zoom: 0.125,
                phase,
                thin: true,
            });
        }
    }
    let first = cases[0].plan()?;
    let fixture = cx.update(|cx| {
        let bounds = Bounds::centered(None, size(px(208.), px(208.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                kind: gpui::WindowKind::PopUp,
                window_background: WindowBackgroundAppearance::Transparent,
                show: false,
                focus: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("RustShot · 圆角边框 GPU 检查");
                cx.new(|_| BorderAaFixture { plan: first })
            },
        )
    })??;
    let h = fixture.update(cx, |_, window, _| hwnd(window))?;
    let result: Result<()> = async {
        let owned = Owned::of(h)?;
        owned.check()?;
        gpui_smoke::isolate_input(h)?;
        unsafe {
            let style = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            SetWindowLongPtrW(
                h,
                GWL_EXSTYLE,
                (style | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TRANSPARENT.0) as isize,
            );
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        let mut failed = Vec::new();
        for (index, case) in cases.iter().copied().enumerate() {
            let plan = case.plan()?;
            fixture.update(cx, |value, _, cx| {
                value.plan = plan;
                cx.notify();
            })?;
            let bytes = border_aa_capture(fixture, &owned, cx).await?;
            let mut detail = border_aa_measure(case, &bytes)?;
            let path = report.root.join(format!("border-aa-{index:02}.png"));
            // Framebuffer 是预乘 BGRA，PNG 转为 straight alpha，避免预览再次压暗圆弧。
            let mut straight = bytes;
            for pixel in straight.chunks_exact_mut(4) {
                if pixel[3] == 0 {
                    pixel.fill(0);
                } else {
                    for channel in 0..3 {
                        pixel[channel] = ((u32::from(pixel[channel]) * 255
                            + u32::from(pixel[3]) / 2)
                            / u32::from(pixel[3]))
                        .min(255) as u8;
                    }
                }
            }
            crate::export::save_png(
                Image {
                    width: BORDER_AA_ROI[2],
                    height: BORDER_AA_ROI[3],
                    bgra: straight,
                    readback_ms: 0.,
                },
                &path,
            )?;
            detail["path"] = json!(path);
            detail["actual_dpi"] = json!(unsafe { GetDpiForWindow(h) });
            if detail["ok"] != true {
                failed.push(index);
            }
            report.stage("border-aa-gpu", detail.clone())?;
            report.groups.push(detail);
        }
        ensure(
            failed.is_empty(),
            &format!("真实 GPU 圆角边框未通过，用例索引：{failed:?}"),
        )
    }
    .await;
    let _ = fixture.update(cx, |_, window, _| window.remove_window());
    result
}
fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}
fn placement(p: Placement) -> Value {
    json!({"x":p.x,"y":p.y,"width":p.width,"height":p.height})
}
struct Report {
    root: PathBuf,
    file: std::fs::File,
    stages: Vec<Value>,
    groups: Vec<Value>,
}
impl Report {
    fn stage(&mut self, name: &str, detail: Value) -> Result<()> {
        let row = json!({"name":name,"unix_ms":unix_ms(),"cpu":crate::process_memory::cpu_memory()?,"gpu":crate::process_memory::gpu_memory(),"detail":detail});
        serde_json::to_writer(&mut self.file, &row)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.stages.push(row);
        Ok(())
    }
}
struct Owned {
    h: HWND,
    pid: u32,
    tid: u32,
    token: Rc<()>,
}
impl Owned {
    fn of(h: HWND) -> Result<Self> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        ensure(
            pid == std::process::id() && tid == unsafe { GetCurrentThreadId() },
            "输入只能登记自有 UI HWND",
        )?;
        let token = Rc::new(());
        unsafe {
            SetPropW(h, PROPERTY, Some(HANDLE(Rc::as_ptr(&token) as *mut _)))?;
        }
        Ok(Self { h, pid, tid, token })
    }
    fn check(&self) -> Result<()> {
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(self.h, Some(&mut pid)) };
        ensure(
            pid == self.pid
                && tid == self.tid
                && unsafe {
                    IsWindow(Some(self.h)).as_bool()
                        && GetPropW(self.h, PROPERTY).0 == Rc::as_ptr(&self.token) as *mut _
                },
            "原自有 HWND/PID/TID/token 已失效",
        )
    }
    fn post(&self, message: u32, wp: usize, lp: isize) -> Result<()> {
        self.check()?;
        unsafe {
            PostMessageW(Some(self.h), message, WPARAM(wp), LPARAM(lp))?;
        }
        Ok(())
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            if GetPropW(self.h, PROPERTY).0 == Rc::as_ptr(&self.token) as *mut _ {
                let _ = RemovePropW(self.h, PROPERTY);
            }
        }
    }
}
fn fixture(width: u32, height: u32) -> Image {
    let mut bgra = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let at = (y as usize * width as usize + x as usize) * 4;
            bgra[at..at + 4].copy_from_slice(&fixture_pixel(x, y));
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}
fn fixture_pixel(x: u32, y: u32) -> [u8; 4] {
    let a = (x / 32) % 3;
    let b = (y / 32) % 3;
    [
        32 + (a * 53 + b * 17) as u8,
        48 + (a * 19 + b * 43) as u8,
        64 + (a * 37 + b * 29) as u8,
        255,
    ]
}
fn input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    Ok(pin.update(cx, |_, w, _| Input {
        handle: hwnd(w).0 as usize,
        scale: w.scale_factor(),
    })?)
}
async fn ready(pin: WindowHandle<PinView>, owned: &Owned, cx: &mut AsyncApp) -> Result<Value> {
    let end = Instant::now() + WAIT;
    loop {
        owned.check()?;
        let probe = native::first_show_probe(owned.h);
        ensure(!probe.aborted, "真实贴图首显被生产取消")?;
        if probe.first_show_ready && probe.first_show_serial > 0 && probe.visible && !probe.starting
        {
            let p = native::rect(owned.h)?;
            ensure(
                probe.prepared_serial > 0
                    && probe.first_show_serial == probe.prepared_serial
                    && probe.presented_size == [p.width, p.height],
                "首显前真实 Present 序号/客户区尺寸不一致",
            )?;
            ensure(
                probe.first_show_style & WS_CAPTION.0 == 0
                    && probe.first_show_style & WS_POPUP.0 != 0,
                "首显仍带编辑窗标题样式",
            )?;
            ensure(
                probe.prepared_unix_ms > 0 && probe.prepared_unix_ms <= probe.first_show_unix_ms,
                "成功 Present 准备时间晚于首显",
            )?;
            let state=pin.update(cx,|v,_,_|json!({"chrome":v.chrome,"toolbar":v.toolbar.is_some(),"canvas":v.canvas.is_some(),"image_size":[v.image_size.0,v.image_size.1]}))?;
            return Ok(
                json!({"first_show":probe,"native_rect":placement(p),"dpi":unsafe {GetDpiForWindow(owned.h)},"state":state}),
            );
        }
        ensure(Instant::now() < end, "贴图真实首显 ACK 超时")?;
        tick(cx).await;
    }
}
async fn frame(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    let done = Rc::new(RefCell::new(false));
    let latch = done.clone();
    pin.update(cx, |_, w, _| {
        w.on_next_frame(move |_, _| *latch.borrow_mut() = true);
        w.refresh();
    })?;
    let end = Instant::now() + WAIT;
    while !*done.borrow() {
        ensure(Instant::now() < end, "贴图实际 UI 帧等待超时")?;
        tick(cx).await;
    }
    Ok(())
}
async fn source_gpu(
    pin: WindowHandle<PinView>,
    owned: &Owned,
    report: &mut Report,
    name: &str,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (scale, size) = pin.update(cx, |v, _, _| (v.state.zoom, v.image_size))?;
    let p = native::rect(owned.h)?;
    ensure(
        scale == 1. && p.width == size.0 && p.height == size.1,
        "小贴图 GPU oracle 必须为实际 100% 客户区",
    )?;
    let roi = [48, 48, 8, 8];
    pin.update(cx, |_, w, cx| {
        w.request_frame_capture(roi)?;
        cx.notify();
        Ok::<_, Box<dyn std::error::Error>>(())
    })??;
    let end = Instant::now() + WAIT;
    let bytes = loop {
        owned.check()?;
        tick(cx).await;
        if let Some(result) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
            break result.map_err(|e| e.to_string())?;
        }
        ensure(Instant::now() < end, "自有贴图 GPU ROI 超时")?;
    };
    ensure(bytes.len() == 8 * 8 * 4, "自有 GPU ROI 长度错误")?;
    for (index, pixel) in bytes.chunks_exact(4).enumerate() {
        let expected = fixture_pixel(48 + index as u32 % 8, 48 + index as u32 / 8);
        ensure(
            pixel.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
            "非均色夹具实际 GPU ROI 与独立原色不符",
        )?;
    }
    let path = report.root.join(format!("{name}.png"));
    crate::export::save_png(
        Image {
            width: 8,
            height: 8,
            bgra: bytes,
            readback_ms: 0.,
        },
        &path,
    )?;
    Ok(
        json!({"path":path,"bytes":std::fs::metadata(&path)?.len(),"width":8,"height":8,"roi":roi,"all_roi_pixels_verified":true,"tolerance":1,"scope":"own GPUI client ROI; excludes outside native shadow"}),
    )
}
async fn button(pin: WindowHandle<PinView>, id: &'static str, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    let (bar, p) = loop {
        if let Some(bar) = pin.update(cx, |v, _, _| v.toolbar)? {
            if let Some(p) = bar.update(cx, |v, _, _| v.positions.borrow().get(id).copied())? {
                break (bar, p);
            }
        }
        ensure(Instant::now() < end, "两级贴图工具栏按钮未就绪")?;
        tick(cx).await;
    };
    let (h, scale) = bar.update(cx, |_, w, _| (hwnd(w), w.scale_factor()))?;
    let owned = Owned::of(h)?;
    gpui_smoke::isolate_input(h)?;
    let event = Input {
        handle: h.0 as usize,
        scale,
    };
    event.hover(((p.0 * scale).round() as u32, (p.1 * scale).round() as u32))?;
    tick(cx).await;
    owned.check()?;
    event.click(p);
    tick(cx).await;
    Ok(())
}
/// 普通 PNG 与剪贴板烘焙截图没有 style；首显、画布接管和缩放都必须保留真实 alpha。
async fn alpha_case(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let (width, height) = (256, 192);
    let mut image = Image {
        width,
        height,
        bgra: Vec::with_capacity((width * height * 4) as usize),
        readback_ms: 0.,
    };
    for _ in 0..height {
        for x in 0..width {
            image.bgra.extend_from_slice(&match x {
                0..64 => [240, 0, 240, 0],
                64..128 => [40, 60, 80, 128],
                _ => [40, 60, 80, 255],
            });
        }
    }
    let path = report.root.join("alpha-source.png");
    crate::export::save_png(image, &path)?;
    let image = crate::export::read_image(&path)?;
    let work = native::work_area((100, 100));
    let origin = (work.x + 96, work.y + 96);
    let pin = cx.update(|cx| PinView::open_copied_capture(image, origin, cx))??;
    let h = pin.update(cx, |_, w, _| hwnd(w))?;
    let owned = Owned::of(h)?;
    gpui_smoke::isolate_input(h)?;
    ready(pin, &owned, cx).await?;
    ensure(
        native::rect(h)?
            == Placement {
                x: origin.0,
                y: origin.1,
                width,
                height,
            },
        "剪贴板截图首显没有保留导出像素的原尺寸/物理位置",
    )?;
    let mut rows = Vec::new();
    for phase in ["plain-png", "canvas", "zoom"] {
        if phase == "canvas" {
            pin.update(cx, |v, w, cx| v.ensure_document_canvas(w, cx))??;
        } else if phase == "zoom" {
            pin.update(cx, |v, w, cx| v.perform(PinAction::Zoom(1.5), w, cx))?;
            let end = Instant::now() + WAIT;
            while pin.update(cx, |v, _, _| v.placement_pending())? {
                ensure(Instant::now() < end, "透明贴图缩放定位超时")?;
                tick(cx).await;
            }
        }
        frame(pin, cx).await?;
        for (x, expected) in [
            (16., [0, 0, 0, 0]),
            (96., [20, 30, 40, 128]),
            (200., [40, 60, 80, 255]),
        ] {
            let roi = pin.update(cx, |v, w, _| {
                let at = v
                    .content_viewport(w)
                    .window_point(crate::annotation::Point { x, y: 80. });
                [at.x.floor() as u32, at.y.floor() as u32, 1, 1]
            })?;
            pin.update(cx, |_, w, _| w.request_frame_capture(roi))??;
            let end = Instant::now() + WAIT;
            let bytes = loop {
                tick(cx).await;
                if let Some(result) = pin.update(cx, |_, w, _| w.take_frame_capture())? {
                    break result?;
                }
                ensure(Instant::now() < end, "透明贴图自有帧读回超时")?;
            };
            ensure(
                bytes.len() == 4 && bytes.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                &format!("透明贴图 {phase} 的预乘 BGRA 不符：{bytes:?} != {expected:?}"),
            )?;
            rows.push(json!({"phase":phase,"source_x":x,"premultiplied_bgra":bytes}));
        }
    }
    let result = json!({"name":"plain-png-alpha-and-copied-origin","ok":true,"samples":rows,
        "origin":[origin.0,origin.1],"clipboard_touched":false,"source":"PNG decode, no presentation style"});
    println!("{}", result);
    report.groups.push(result.clone());
    report.stage("plain-png-alpha", result)?;
    pin.update(cx, |v, w, cx| {
        v.workspace = None;
        v.close_toolbar(cx);
        w.remove_window();
    })?;
    Ok(())
}
async fn closed(
    pin: WindowHandle<PinView>,
    owned: &Owned,
    weaks: &[Weak<RenderImage>],
    extra: &[usize],
    cx: &mut AsyncApp,
) -> Result<Value> {
    let request = unix_ms();
    owned.post(WM_CLOSE, 0, 0)?;
    let end = Instant::now() + WAIT;
    loop {
        let removed = pin.update(cx, |_, _, _| ()).is_err();
        let native_gone = !unsafe { IsWindow(Some(owned.h)) }.as_bool();
        let children_gone = extra
            .iter()
            .all(|&h| !unsafe { IsWindow(Some(HWND(h as *mut _))) }.as_bool());
        let expired = weaks.iter().all(|w| w.upgrade().is_none());
        if removed && native_gone && children_gone && expired {
            let destroyed = unix_ms();
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            return Ok(
                json!({"hwnd":owned.h.0 as usize,"native_close_request_unix_ms":request,"hwnd_destroyed_observed_unix_ms":destroyed,"entity_removed":true,"owned_hwnd_destroyed":true,"extra_hwnds_destroyed":true,"all_observed_image_weaks_expired":true}),
            );
        }
        ensure(
            Instant::now() < end,
            "贴图关闭后原 HWND/子窗/CPU 弱引用仍存活",
        )?;
        tick(cx).await;
    }
}
async fn bars(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Vec<usize>> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(bar) = pin.update(cx, |v, _, _| v.toolbar)? {
            let (h, properties, count) = bar.update(cx, |v, w, _| {
                (hwnd(w), v.properties, v.positions.borrow().len())
            })?;
            if let Some(properties) = properties {
                let second = properties.update(cx, |_, w, _| hwnd(w))?;
                if count >= 14
                    && unsafe { IsWindowVisible(h).as_bool() && IsWindowVisible(second).as_bool() }
                {
                    Owned::of(h)?.check()?;
                    Owned::of(second)?.check()?;
                    return Ok(vec![h.0 as usize, second.0 as usize]);
                }
            }
        }
        ensure(Instant::now() < end, "Space 后真实两级工具栏未显示")?;
        tick(cx).await;
    }
}
async fn collapsed(
    pin: WindowHandle<PinView>,
    children: &[usize],
    cx: &mut AsyncApp,
) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let off = pin.update(cx, |v, _, _| !v.chrome && v.toolbar.is_none())?;
        if off
            && children
                .iter()
                .all(|&h| !unsafe { IsWindow(Some(HWND(h as *mut _))) }.as_bool())
        {
            return Ok(());
        }
        ensure(
            Instant::now() < end,
            "收栏后子窗的异步 DestroyWindow 未完成",
        )?;
        tick(cx).await;
    }
}
async fn wait_placement(
    pin: WindowHandle<PinView>,
    owned: &Owned,
    expected: Placement,
    cx: &mut AsyncApp,
) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        owned.check()?;
        if native::rect(owned.h)? == expected {
            frame(pin, cx).await?;
            return Ok(());
        }
        ensure(Instant::now() < end, "贴图原生移动/缩放位置没有 ACK")?;
        tick(cx).await;
    }
}
async fn native_key(owned: &Owned, key: usize, cx: &mut AsyncApp) -> Result<()> {
    owned.post(WM_KEYDOWN, key, 1)?;
    owned.post(WM_KEYUP, key, (1isize << 31) | (1isize << 30) | 1)?;
    tick(cx).await;
    Ok(())
}

fn shadow_probe(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Value> {
    pin.update(cx, |v, _, _| -> Result<Value> {
        Ok(serde_json::to_value(
            v.shadow.as_ref().ok_or("原 Pin 没有阴影 RAII")?.probe()?,
        )?)
    })?
}
type ShadowObservation = (
    Value,
    Arc<crate::pin_shadow::PinShadowLedger>,
    std::rc::Weak<crate::pin_shadow::PinShadowCore>,
    Vec<usize>,
);
async fn shadow_check(
    pin: WindowHandle<PinView>,
    report: &mut Report,
    name: &str,
    inspect_dib: bool,
    cx: &mut AsyncApp,
) -> Result<ShadowObservation> {
    let samples = Rc::new(RefCell::new(Vec::<Value>::new()));
    let observations = samples.clone();
    let folder = report.root.clone();
    let prefix = name.to_owned();
    let guard = if inspect_dib {
        Some(crate::pin_shadow::observe_uploads_for_qa(Rc::new(
            move |probe, bytes| {
                use crate::pin_shadow::ShadowRect;
                let r = probe.rect;
                let c = probe.content_rect;
                ensure(
                    bytes.len() == r.width as usize * r.height as usize * 4
                        && probe.premultiplied_bgra,
                    "实际阴影 DIB 容量/预乘元数据不符",
                )?;
                let inside = |x: i64, y: i64, rect: ShadowRect| {
                    x >= rect.x as i64
                        && y >= rect.y as i64
                        && x < rect.x as i64 + rect.width as i64
                        && y < rect.y as i64 + rect.height as i64
                };
                let mut inner_pixels = 0usize;
                let mut changed = 0usize;
                for (index, pixel) in bytes.chunks_exact(4).enumerate() {
                    ensure(
                        pixel[..3].iter().all(|&channel| channel <= pixel[3]),
                        "实际上传阴影 DIB 不是预乘 BGRA",
                    )?;
                    let x = r.x as i64 + (index % r.width as usize) as i64;
                    let y = r.y as i64 + (index / r.width as usize) as i64;
                    if inside(x, y, c) {
                        inner_pixels += 1;
                        ensure(pixel[3] == 0, "实际阴影 DIB 覆盖了贴图内部")?;
                    }
                    if pixel[3] > 0 {
                        changed += 1;
                    }
                }
                ensure(changed > 0, "实际阴影条带全透明")?;
                let (horizontal, forward) = match probe.part {
                    "top" => (false, false),
                    "bottom" => (false, true),
                    "left" => (true, false),
                    "right" => (true, true),
                    _ => return Err("未知阴影条带".into()),
                };
                let cross = if horizontal {
                    (c.y as i64 + c.height as i64 / 2 - r.y as i64).clamp(0, r.height as i64 - 1)
                        as u32
                } else {
                    (c.x as i64 + c.width as i64 / 2 - r.x as i64).clamp(0, r.width as i64 - 1)
                        as u32
                };
                let length = if horizontal { r.width } else { r.height };
                let mut alpha = Vec::with_capacity(length as usize);
                for distance in 0..length {
                    let along = if forward {
                        distance
                    } else {
                        length - 1 - distance
                    };
                    let (x, y) = if horizontal {
                        (along, cross)
                    } else {
                        (cross, along)
                    };
                    alpha.push(bytes[(y as usize * r.width as usize + x as usize) * 4 + 3]);
                }
                ensure(
                    alpha.len() >= 4
                        && alpha[0] > 0
                        && alpha.windows(2).all(|a| a[0] >= a[1])
                        && alpha.last().copied().unwrap_or(255) < alpha[0],
                    "实际阴影法线 alpha 未从接触边单调渐隐",
                )?;
                ensure(
                    alpha.windows(2).filter(|a| a[0] > a[1]).count() >= 2
                        && alpha.last().copied().unwrap_or(255) <= alpha[0] / 4,
                    "阴影没有可观测的柔和接触/远端衰减",
                )?;
                let side = length.min(12);
                let (width, height) = if horizontal { (side, 1) } else { (1, side) };
                let mut patch = Vec::with_capacity(side as usize * 4);
                for distance in 0..side {
                    let along = if forward {
                        distance
                    } else {
                        length - 1 - distance
                    };
                    let (x, y) = if horizontal {
                        (along, cross)
                    } else {
                        (cross, along)
                    };
                    let at = (y as usize * r.width as usize + x as usize) * 4;
                    patch.extend_from_slice(&bytes[at..at + 4]);
                }
                let path = folder.join(format!("{prefix}-shadow-{}-uploaded.png", probe.part));
                ensure(!path.exists(), "阴影样本文件已存在")?;
                crate::export::save_png(
                    Image {
                        width,
                        height,
                        bgra: patch,
                        readback_ms: 0.,
                    },
                    &path,
                )?;
                observations.borrow_mut().push(json!({"upload":probe,"source":"actual-successful-UpdateLayeredWindow-DIB","alpha_contact_to_far":alpha,"nonzero_alpha_pixels":changed,"interior_pixels":inner_pixels,"premultiplied_verified":true,"inner_alpha_absent_verified":true,"alpha_monotonic_verified":true,"contact_far_falloff_verified":true,"png":{"path":path,"bytes":std::fs::metadata(&path)?.len(),"width":width,"height":height},"desktop_composition_read_back":false}));
                Ok(())
            },
        ))?)
    } else {
        None
    };
    let (probe, ledger, weak) = pin.update(cx, |v, _, _| -> Result<_> {
        let shadow = v.shadow.as_ref().ok_or("原 Pin 没有阴影")?;
        shadow.refresh_for_qa()?;
        Ok((shadow.probe()?, shadow.ledger(), shadow.weak()))
    })??;
    drop(guard);
    let mut total = 0usize;
    let mut parts = Vec::new();
    let required = (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE).0;
    ensure(
        probe.parts.len() == 4
            && probe.owner_pid == std::process::id()
            && probe.owner_tid == unsafe { GetCurrentThreadId() },
        "阴影不是原自有 UI HWND 的四条带",
    )?;
    let original = native::rect(HWND(probe.owner as *mut _))?;
    ensure(
        (
            probe.content_rect.x,
            probe.content_rect.y,
            probe.content_rect.width,
            probe.content_rect.height,
        ) == (original.x, original.y, original.width, original.height),
        "阴影内容边界偏离实际原 Pin HWND",
    )?;
    for part in &probe.parts {
        let h = HWND(part.hwnd as *mut _);
        let mut pid = 0;
        let tid = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        let mut affinity = 0;
        unsafe {
            GetWindowDisplayAffinity(h, &mut affinity)?;
        }
        ensure(
            pid == probe.owner_pid
                && tid == probe.owner_tid
                && part.pid == pid
                && part.tid == tid
                && part.owner == probe.owner
                && part.class == "RustShot.Pin.Shadow.v1"
                && part.token != 0,
            "阴影 HWND 原身份元数据不符",
        )?;
        ensure(
            unsafe { GetPropW(h, w!("RustShot.Pin.Shadow.Instance.v1")) }.0 as usize == part.token
                && unsafe { GetWindow(h, GW_OWNER) }? == HWND(probe.owner as *mut _),
            "阴影 native token/owner 不符",
        )?;
        let style = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
        ensure(
            style & required == required
                && part.exstyle == style
                && affinity == WDA_EXCLUDEFROMCAPTURE.0
                && part.affinity == affinity,
            "阴影穿透/不激活/排除采集实际属性不符",
        )?;
        let hit = unsafe { SendMessageW(h, WM_NCHITTEST, Some(WPARAM(0)), Some(LPARAM(0))) };
        let activate = unsafe {
            SendMessageW(
                h,
                WM_MOUSEACTIVATE,
                Some(WPARAM(probe.owner)),
                Some(LPARAM(0)),
            )
        };
        ensure(
            hit.0 == HTTRANSPARENT as isize && activate.0 == MA_NOACTIVATE as isize,
            "阴影真实 WndProc 命中/激活返回错误",
        )?;
        if let Some(rect) = part.rect {
            let native = native::rect(h)?;
            ensure(
                (rect.x, rect.y, rect.width, rect.height)
                    == (native.x, native.y, native.width, native.height),
                "阴影实际 HWND 矩形不是上传条带",
            )?;
            ensure(
                rect.width <= probe.extent * 2 || rect.height <= probe.extent,
                "阴影使用了全图矩形而不是薄条带",
            )?;
            let no_overlap = rect.x as i64 + rect.width as i64 <= original.x as i64
                || rect.x as i64 >= original.x as i64 + original.width as i64
                || rect.y as i64 + rect.height as i64 <= original.y as i64
                || rect.y as i64 >= original.y as i64 + original.height as i64;
            ensure(no_overlap, "阴影条带占用了原图内部")?;
            ensure(
                part.visible == probe.owner_visible,
                "阴影可见状态没有跟随原 Pin",
            )?;
            total += rect.width as usize * rect.height as usize * 4;
        } else {
            ensure(!part.visible, "屏幕外阴影条带仍显示")?;
        }
        parts.push(part.hwnd);
    }
    let life = ledger.snapshot();
    ensure(
        total == probe.visible_perimeter_bytes
            && total <= probe.byte_budget
            && probe.byte_budget == crate::pin_shadow::SHADOW_BYTE_BUDGET,
        "阴影实际条带容量不在8MB预算",
    )?;
    ensure(
        life.current_staging_bytes == 0
            && life.live_dcs == 0
            && life.live_dibs == 0
            && life.peak_staging_bytes <= probe.byte_budget
            && life.successful_uploads > 0
            && life.errors.is_empty(),
        "阴影上传后仍保留临时 DIB/DC 或存在失败",
    )?;
    if inspect_dib {
        ensure(samples.borrow().len() == 4, "小贴图未取得四条实际上传 DIB")?;
    }
    let value = serde_json::to_value(&probe)?;
    report.stage("actual-shadow-strip-probe",json!({"mode":name,"shadow":value,"ledger":life,"uploaded_dib_samples":samples.borrow().clone(),"actual_native_metadata_verified":true,"actual_uploaded_dib_verified":inspect_dib,"desktop_shadow_composition_verified":false}))?;
    Ok((value, ledger, weak, parts))
}

async fn cold_case(report: &mut Report, events: &Events, cx: &mut AsyncApp) -> Result<()> {
    let work = native::work_area((100, 100));
    let pin = cx.update(|cx| {
        PinView::open_state(
            fixture(384, 256),
            (work.x + 96, work.y + 96),
            PinState::default(),
            cx,
        )
    })??;
    let h = pin.update(cx, |_, w, _| hwnd(w))?;
    let owned = Owned::of(h)?;
    let initial = native::first_show_probe(h);
    ensure(
        initial.starting && !initial.visible && !initial.first_show_ready,
        "工厂返回前平台已经显示贴图",
    )?;
    gpui_smoke::isolate_input(h)?;
    let weaks = pin.update(cx, |v, _, _| {
        vec![Arc::downgrade(&v.original), Arc::downgrade(&v.image)]
    })?;
    report.stage("cold-pin-hidden-factory",json!({"hwnd":h.0 as usize,"first_show":initial,"default_no_chrome":pin.update(cx,|v,_,_|!v.chrome&&v.toolbar.is_none()&&v.canvas.is_none())?}))?;
    let first = ready(pin, &owned, cx).await?;
    ensure(
        first["state"]["chrome"] == false
            && first["state"]["toolbar"] == false
            && first["state"]["canvas"] == false,
        "默认贴图误展开 Chrome/画布",
    )?;
    let gpu = source_gpu(pin, &owned, report, "cold-pin-client", cx).await?;
    let (shadow_initial, shadow_life, shadow_weak, shadow_hwnds) =
        shadow_check(pin, report, "cold", true, cx).await?;
    let event_deadline = Instant::now() + WAIT;
    while !events
        .rows()
        .iter()
        .any(|r| r["event"] == EVENT_SHOW && r["hwnd"] == h.0 as usize)
    {
        ensure(
            Instant::now() < event_deadline,
            "原 Pin SHOW WinEvent 未送达",
        )?;
        tick(cx).await;
    }
    let show_events = events.rows();
    ensure(
        show_events
            .iter()
            .any(|r| r["event"] == EVENT_SHOW && r["hwnd"] == h.0 as usize),
        "没有观察到原 Pin 的真实 WinEvent SHOW",
    )?;
    for row in show_events
        .iter()
        .filter(|r| r["event"] == EVENT_SHOW && r["hwnd"] == h.0 as usize)
    {
        ensure(
            row["first_show"]["prepared_serial"]
                .as_u64()
                .is_some_and(|n| n > 0)
                && row["first_show"]["first_show_ready"] == true,
            "原 Pin 的 SHOW 事件发生时尚未准备成功 Present",
        )?;
        let style = row["style"].as_u64().ok_or("SHOW 样式缺失")? as u32;
        ensure(
            style & WS_CAPTION.0 == 0 && style & WS_POPUP.0 != 0,
            "原 Pin 的 SHOW 事件包含通用标题栏",
        )?;
    }
    let first_upload = shadow_life.snapshot();
    ensure(
        !first_upload.first_upload_owner_visible
            && first_upload.first_successful_upload_unix_ms > 0
            && first_upload.first_successful_upload_unix_ms
                <= first["first_show"]["first_show_unix_ms"]
                    .as_u64()
                    .ok_or("首显时间缺失")?,
        "阴影第一次实际上传未在隐藏首显阶段完成",
    )?;
    ensure(
        !show_events.iter().any(|r| {
            r["event"] == EVENT_SHOW
                && (r["title"] == concat!(crate::brand_name!(), " · 调整长截图")
                    || r["title"] == concat!(crate::brand_name!(), " · 图片编辑"))
        }),
        "首显期间观察到中转编辑窗",
    )?;
    report.stage("cold-pin-first-visible",json!({"hwnd":h.0 as usize,"first":first,"gpu":gpu,"shadow":shadow_initial,"own_window_events":show_events,"initial_source_path":"PinView::open_state common factory; synthetic source","clipboard_decode_verified":false}))?;
    native_key(&owned, 0x20, cx).await?;
    // 默认 Edit 没有属性栏；通过真实主栏按钮选择 Pen，第二层复用正式属性视图。
    button(pin, "pen", cx).await?;
    let children = bars(pin, cx).await?;
    report.stage("actual-space-two-level-bars",json!({"pin_hwnd":h.0 as usize,"bar_hwnds":children,"real_native_key_route":true,"real_primary_pen_button":true}))?;
    native_key(&owned, 0x20, cx).await?;
    collapsed(pin, &children, cx).await?;
    let original = native::rect(h)?;
    let event = input(pin, cx)?;
    owned.check()?;
    event.drag((112, 112), (144, 132));
    let moved = Placement {
        x: original.x + 32,
        y: original.y + 20,
        ..original
    };
    wait_placement(pin, &owned, moved, cx).await?;
    let moved_shadow = shadow_check(pin, report, "moved", false, cx).await?.0;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Zoom(1.25), w, cx))?;
    let end = Instant::now() + WAIT;
    loop {
        if native::rect(h)?.width == 480 {
            break;
        }
        ensure(Instant::now() < end, "Zoom 1.25 未作用于实际 Pin HWND")?;
        tick(cx).await;
    }
    frame(pin, cx).await?;
    let zoom_shadow = shadow_check(pin, report, "zoomed", false, cx).await?.0;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Opacity(160), w, cx))?;
    frame(pin, cx).await?;
    let opacity_shadow = shadow_check(pin, report, "opacity", false, cx).await?.0;
    ensure(
        opacity_shadow["owner"]["effective_alpha"] == 160
            || opacity_shadow["effective_alpha"] == 160,
        "阴影未跟随实际贴图透明度",
    )?;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Hide, w, cx))?;
    let end = Instant::now() + WAIT;
    loop {
        let state = shadow_probe(pin, cx)?;
        if !unsafe { IsWindowVisible(h) }.as_bool()
            && state["parts"]
                .as_array()
                .is_some_and(|p| p.iter().all(|p| p["visible"] == false))
        {
            break;
        }
        ensure(Instant::now() < end, "Hide 后自有阴影仍显示")?;
        tick(cx).await;
    }
    owned.check()?;
    unsafe {
        ShowWindowAsync(h, SW_SHOWNOACTIVATE).ok()?;
    }
    let end = Instant::now() + WAIT;
    while !unsafe { IsWindowVisible(h) }.as_bool() {
        ensure(Instant::now() < end, "自有 Pin Show ACK 超时")?;
        tick(cx).await;
    }
    frame(pin, cx).await?;
    let shown_shadow = shadow_check(pin, report, "shown", false, cx).await?.0;
    let negative = negative_monitor_case(pin, &owned, report, cx).await?;
    report.stage("cold-pin-move-zoom-opacity-hide-show",json!({"moved":placement(moved),"move_shadow":moved_shadow,"zoom_shadow":zoom_shadow,"opacity_shadow":opacity_shadow,"shown_shadow":shown_shadow,"negative_monitor":negative}))?;
    let close = closed(pin, &owned, &weaks, &shadow_hwnds, cx).await?;
    ensure(
        shadow_weak.upgrade().is_none(),
        "Pin 已关闭却阴影 Core 仍被强引用",
    )?;
    let life = shadow_life.snapshot();
    check_shadow_closed(&life)?;
    report.stage(
        "cold-pin-closed",
        json!({"close":close,"shadow_lifecycle":life,"shadow_core_weak_expired":true}),
    )?;
    report.groups.push(json!({"name":"cold-pin-presentation-and-shadow","ok":true,"first_visible_pin_verified":true,"default_no_chrome":true,"actual_space_two_level_bars":true,"collapse_move_zoom_opacity_hide_show":true,"actual_uploaded_shadow_dib_verified":true,"shadow_pixel_scope":"successful UpdateLayeredWindow input DIB; excludes desktop composition","desktop_shadow_gpu_pixels_verified":false,"close":close,"shadow_lifecycle":life}));
    Ok(())
}

async fn long_output_case(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let done = Rc::new(RefCell::new(None));
    let callback = done.clone();
    let source = Arc::new(fixture(320, 960));
    let source_weak = Arc::downgrade(&source);
    let snapshot = StitchSnapshot {
        width: 320,
        height: 960,
        axis: ScrollAxis::Vertical,
        viewport: Rect {
            x: 0,
            y: 720,
            width: 320,
            height: 240,
        },
        segments: vec![StitchSegment {
            image: source,
            source: Rect::full(320, 960),
            origin: (0, 0),
        }],
    };
    // 生产输出入口：后台物化一次，再直接交给贴图，不经任何中转窗口。
    let image = cx
        .update(|cx| long_output::export(snapshot, LongOutput::Pin, cx))?
        .await?;
    cx.update(|cx| {
        long_output::deliver(
            image,
            LongOutput::Pin,
            Default::default(),
            None,
            move |outcome, _| {
                *callback.borrow_mut() = Some(matches!(outcome, LongOutcome::Finished))
            },
            cx,
        )
    })?;
    let end = Instant::now() + WAIT;
    let pin = loop {
        let pins = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|w| w.downcast::<PinView>())
                .collect::<Vec<_>>()
        })?;
        if *done.borrow() == Some(true) && pins.len() == 1 {
            break pins[0];
        }
        ensure(
            *done.borrow() != Some(false),
            "有限 LongSnapshot autoPin 未完成 Finished",
        )?;
        ensure(Instant::now() < end, "有限 LongSnapshot autoPin ACK 超时")?;
        tick(cx).await;
    };
    let h = pin.update(cx, |_, w, _| hwnd(w))?;
    let owned = Owned::of(h)?;
    let first = ready(pin, &owned, cx).await?;
    let (weaks, source_exact) = pin.update(cx, |v, _, _| {
        let bytes = v.original.as_bytes(0).unwrap();
        let exact = (0..960).all(|y| {
            (0..320).all(|x| {
                let at = (y * 320 + x) * 4;
                bytes[at..at + 4] == fixture_pixel(x as u32, y as u32)
            })
        });
        (
            vec![Arc::downgrade(&v.original), Arc::downgrade(&v.image)],
            v.original_size == (320, 960) && exact,
        )
    })?;
    ensure(
        source_exact,
        "finite autoPin 实际原图像素与独立非均色夹具不符",
    )?;
    let (_, ledger, weak, parts) = shadow_check(pin, report, "finite-long", false, cx).await?;
    let close = closed(pin, &owned, &weaks, &parts, cx).await?;
    ensure(
        weak.upgrade().is_none() && source_weak.upgrade().is_none(),
        "finite autoPin 关闭后源/阴影仍持有",
    )?;
    check_shadow_closed(&ledger.snapshot())?;
    report.stage("finite-long-auto-pin-closed",json!({"first_show":first,"actual_original_pixels_verified":true,"snapshot_source_weak_expired":true,"close":close,"shadow_lifecycle":ledger.snapshot()}))?;
    report.groups.push(json!({"name":"finite-long-auto-pin-no-editor","ok":true,"snapshot":[320,960],"real_long_output_pin_entry":true,"synthetic_snapshot":true,"desktop_capture_verified":false,"close":close}));
    Ok(())
}

async fn shadow_budget_case(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let work = native::work_area((100, 100));
    let pin = cx.update(|cx| {
        PinView::open_state(
            fixture(960, 32000),
            (work.x + 120, work.y + 60),
            PinState::default(),
            cx,
        )
    })??;
    let h = pin.update(cx, |_, w, _| hwnd(w))?;
    let owned = Owned::of(h)?;
    ready(pin, &owned, cx).await?;
    let weaks = pin.update(cx, |v, _, _| {
        vec![Arc::downgrade(&v.original), Arc::downgrade(&v.image)]
    })?;
    pin.update(cx, |v, w, cx| v.perform(PinAction::Zoom(0.5), w, cx))?;
    let end = Instant::now() + WAIT;
    while native::rect(h)?.height != 16000 {
        ensure(Instant::now() < end, "32k shadow-only Pin zoom 未达到 0.5")?;
        tick(cx).await;
    }
    frame(pin, cx).await?;
    let (probe, ledger, weak, parts) = shadow_check(pin, report, "long-budget", false, cx).await?;
    let p = native::rect(h)?;
    report.stage("long-pin-shadow-allocation",json!({"source":[960,32000],"source_bgra_bytes":122880000u64,"actual_pin_rect":placement(p),"shadow":probe,"scope":"shadow strips and native surfaces only; no image processing, PNG export or prior memory-suite repetition"}))?;
    let close = closed(pin, &owned, &weaks, &parts, cx).await?;
    ensure(weak.upgrade().is_none(), "32k shadow-only Core 未回收")?;
    check_shadow_closed(&ledger.snapshot())?;
    report.stage(
        "long-pin-shadow-closed",
        json!({"close":close,"shadow_lifecycle":ledger.snapshot()}),
    )?;
    report.groups.push(json!({"name":"long-pin-shadow-allocation","ok":true,"source":[960,32000],"actual_pin_size":[p.width,p.height],"shadow_budget_verified":true,"full_image_processing_verified":false,"three_cycle_32000_memory_verified":false,"close":close}));
    Ok(())
}
fn check_shadow_closed(life: &crate::pin_shadow::PinShadowLedgerProbe) -> Result<()> {
    ensure(
        life.closed
            && life.created_windows == 4
            && life.destroyed_or_already_gone_windows == 4
            && life.current_staging_bytes == 0
            && life.live_dcs == 0
            && life.live_dibs == 0
            && life.errors.is_empty(),
        "自有阴影关闭 ledger 仍有窗口/DIB/DC 或释放错误",
    )
}
async fn negative_monitor_case(
    pin: WindowHandle<PinView>,
    owned: &Owned,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let (left, top) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let work = native::work_area((left + 80, top + 80));
    if work.x >= 0 && work.y >= 0 {
        return Ok(
            json!({"available":false,"native_negative_origin_verified":false,"reason":"current monitor metadata has no negative work-area origin","mixed_dpi_verified":false}),
        );
    }
    let old = native::rect(owned.h)?;
    let target = Placement {
        x: work.x + 48,
        y: work.y + 48,
        ..old
    };
    ensure(target.x < 0 || target.y < 0, "负屏幕候选实际上非负")?;
    owned.check()?;
    // 仅定位已登记的自有 HWND；没有移动用户鼠标，也不伪称物理拖动跨屏。
    cx.update(|cx| native::place_later(owned.h, target, None, false, cx))?;
    wait_placement(pin, owned, target, cx).await?;
    let probe = shadow_check(pin, report, "negative-monitor", false, cx)
        .await?
        .0;
    Ok(
        json!({"available":true,"native_negative_origin_verified":true,"actual_rect":placement(native::rect(owned.h)?),"actual_dpi":unsafe{GetDpiForWindow(owned.h)},"shadow":probe,"hardware_drag_verified":false,"mixed_dpi_verified":false}),
    )
}

pub(crate) fn run() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("RUSTSHOT_PIN_PRESENTATION_QA_DIR").ok_or("缺少贴图首显隔离 QA 目录")?,
    );
    ensure(root.is_absolute(), "贴图首显 QA 目录必须为绝对路径")?;
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;
    let settings = root.join("settings");
    std::fs::create_dir_all(&settings)?;
    if let Some(actual) = std::env::var_os("RUSTSHOT_DATA_DIR") {
        ensure(
            std::fs::canonicalize(actual)? == std::fs::canonicalize(&settings)?,
            "贴图首显 settings 必须在本次 artifact 内",
        )?;
    }
    std::env::set_var("RUSTSHOT_DATA_DIR", &settings);
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    let report = Report {
        file: OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("stages.jsonl"))?,
        root,
        stages: Vec::new(),
        groups: Vec::new(),
    };
    let holder = Rc::new(RefCell::new(Some(report)));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("贴图首显专项未完成".to_string())));
    let final_error = error.clone();
    let initial_foreground = unsafe { GetForegroundWindow() }.0 as usize;
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move|cx:&mut App|{
        let events=match Events::start(){Ok(events)=>events,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}};
        let keepalive=match cx.open_window(WindowOptions{show:false,focus:false,..Default::default()},|_,cx|cx.new(|_|KeepAlive)){Ok(window)=>window,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}};
        cx.spawn(async move|cx|{
            let mut report=holder.borrow_mut().take().expect("独占首显报告");
            let result:Result<()>=async{
                report.stage("cold-own-application",json!({"pid":std::process::id(),"hidden_keepalive":true,"original_foreground":initial_foreground,"desktop_read":false,"clipboard_touched":false,"global_input_sent":false}))?;
                if std::env::var("RUSTSHOT_PIN_BORDER_AA_QA").as_deref()==Ok("1") {border_aa_case(&mut report,cx).await?;} else if std::env::var("RUSTSHOT_PIN_ALPHA_QA").as_deref()==Ok("1") {alpha_case(&mut report,cx).await?;} else {cold_case(&mut report,&events,cx).await?;long_output_case(&mut report,cx).await?;shadow_budget_case(&mut report,cx).await?;}
                let foreground=unsafe{GetForegroundWindow()};let mut foreground_pid=0;unsafe{GetWindowThreadProcessId(foreground,Some(&mut foreground_pid));}
                ensure(foreground.0 as usize==initial_foreground||foreground_pid!=std::process::id(),"QA 把用户前台窗口切换到自己的贴图")?;
                report.stage("all-cases-closed",json!({"own_window_events":events.rows(),"current_foreground":foreground.0 as usize,"original_foreground":initial_foreground,"foreground_was_not_explicitly_changed":true,"capture_threads":crate::long_capture_pipeline::active_capture_threads(),"consumer_sessions":crate::long_capture_runtime::active_workers()}))?;Ok(())
            }.await;
            let failure=result.err().map(|e|e.to_string());
            if failure.is_some(){if let Ok(windows)=cx.update(|cx|cx.windows()){for window in windows{if let Some(pin)=window.downcast::<PinView>(){let _=pin.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx));}}}let until=Instant::now()+Duration::from_secs(2);while Instant::now()<until{tick(cx).await;}}
            drop(events);*error.borrow_mut()=failure;*holder.borrow_mut()=Some(report);let _=keepalive.update(cx,|_,w,_|w.remove_window());let _=cx.update(|cx|cx.quit());
        }).detach();
    });
    let failure = final_error.borrow_mut().take();
    if let Some(mut report) = final_holder.borrow_mut().take() {
        report.stage("after-application",json!({"application_returned_normally":true,"capture_threads":crate::long_capture_pipeline::active_capture_threads(),"consumer_sessions":crate::long_capture_runtime::active_workers(),"forced_cleanup":false}))?;
        let path = report.root.join("report.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let border_aa = std::env::var("RUSTSHOT_PIN_BORDER_AA_QA").as_deref() == Ok("1");
        let requested_groups = if border_aa {
            json!(["border-aa-owned-gpu-framebuffer"])
        } else {
            json!([
                "cold-pin-presentation-and-shadow",
                "finite-long-auto-pin-no-editor",
                "long-pin-shadow-allocation"
            ])
        };
        serde_json::to_writer_pretty(
            &mut file,
            &json!({"suite":"pin-presentation-native","pid":std::process::id(),"ok":failure.is_none(),"error":failure,"groups":report.groups,"stages":report.stages,"data_dir":crate::preferences::data_dir(),"requested_groups":requested_groups,"synthetic_owned_images":true,"actual_native_window_messages":!border_aa,"desktop_read":false,"clipboard_touched":false,"global_input_sent":false,"hardware_mouse_or_keyboard_verified":false,"file_dialog_verified":false,"real_clipboard_decode_verified":false,"actual_shadow_upload_dib_verified":failure.is_none()&&!border_aa,"border_gpu_framebuffer_verified":failure.is_none()&&border_aa,"desktop_shadow_composition_verified":false,"shadow_gpu_fbo_verified":false,"three_cycle_32000_memory_verified":false,"all_gpu_or_com_resources_verified":false,"application_returned_normally":true,"forced_cleanup":false,"broader_goal_complete":false}),
        )?;
        file.flush()?;
        println!(
            "{}",
            json!({"phase":"pin_presentation_smoke_complete","pid":std::process::id(),"ok":failure.is_none(),"report":path})
        );
    }
    if let Some(error) = failure {
        Err(error.into())
    } else {
        Ok(())
    }
}
