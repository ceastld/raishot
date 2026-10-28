//! 路径资源专项：只绘制和读取本进程合成窗口，不采集桌面或操作剪贴板。
use crate::{gpui_capture::hwnd, Result};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, App, AppContext, Application, AsyncApp,
    Bounds, Context, PathBuilder, Render, TitlebarOptions, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

const COLOR: u32 = 0x1e5080a6;
const TIMEOUT: Duration = Duration::from_secs(8);

struct Fixture {
    stage: u8,
    painted: Rc<Cell<u8>>,
}
#[derive(Default)]
struct CloseProof {
    requested: Cell<bool>,
    native: Cell<usize>,
    app_empty: Cell<bool>,
}
impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let stage = self.stage;
        let painted = self.painted.clone();
        div().size_full().bg(rgb(0xffffff)).child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            bounds.origin + point(px(12.), px(12.)),
                            size(px(24.), px(24.)),
                        ),
                        rgb(0x2468a3),
                    ));
                    if stage > 0 {
                        let p = |x, y| bounds.origin + point(px(x), px(y));
                        let mut line = PathBuilder::stroke(px(5.));
                        line.move_to(p(40., 70.));
                        line.line_to(p(310., 97.));
                        if let Ok(path) = line.build() {
                            window.paint_path(path, rgba(COLOR));
                        }
                        let mut curve = PathBuilder::stroke(px(5.));
                        curve.move_to(p(40., 180.));
                        curve.curve_to(p(310., 180.), p(175., 105.));
                        if let Ok(path) = curve.build() {
                            window.paint_path(path, rgba(COLOR));
                        }
                        if stage >= 3 {
                            let mut added = PathBuilder::stroke(px(5.));
                            added.move_to(p(70., 275.));
                            added.line_to(p(350., 312.));
                            if let Ok(path) = added.build() {
                                window.paint_path(path, rgba(COLOR));
                            }
                        }
                    }
                    painted.set(stage);
                },
            )
            .size_full(),
        )
    }
}

fn pass(name: &str, detail: &str) {
    println!("{{\"suite\":\"path-resources\",\"test\":\"{name}\",\"pid\":{},\"detail\":\"{detail}\",\"synthetic_only\":true,\"ok\":true}}", std::process::id());
}
fn ensure(ok: bool, error: impl Into<String>) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(error.into().into())
    }
}
async fn wait(
    cx: &mut AsyncApp,
    label: &str,
    mut ready: impl FnMut(&mut AsyncApp) -> Result<bool>,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if ready(cx)? {
            return Ok(());
        }
        ensure(
            started.elapsed() < TIMEOUT,
            format!("限时等待失败：{label}"),
        )?;
        cx.background_executor()
            .timer(Duration::from_millis(30))
            .await;
    }
}
fn flush() -> Result<()> {
    let code = unsafe { DwmFlush() };
    ensure(code >= 0, format!("DwmFlush 失败：{code:#x}"))
}
fn own(h: HWND) -> Result<()> {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    ensure(
        pid == std::process::id() && unsafe { IsWindow(Some(h)) }.as_bool(),
        "专项 HWND 已销毁或不属于本进程",
    )
}

struct Patch {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    left: f32,
    top: f32,
    scale: f32,
}
impl Patch {
    fn green(&self, x: u32, y: u32) -> u8 {
        self.pixels[((y * self.width + x) * 4 + 1) as usize]
    }
    fn representative_rgb(&self) -> [[u8; 3]; 3] {
        [
            (0, 0),
            (self.width / 2, self.height / 2),
            (self.width - 1, self.height - 1),
        ]
        .map(|(x, y)| {
            let i = ((y * self.width + x) * 4) as usize;
            [self.pixels[i + 2], self.pixels[i + 1], self.pixels[i]]
        })
    }
}

fn geometry(h: HWND, scale: f32) -> Result<String> {
    let (mut client, mut outer) = (RECT::default(), RECT::default());
    let mut origin = POINT::default();
    unsafe {
        GetClientRect(h, &mut client)?;
        GetWindowRect(h, &mut outer)?;
        ClientToScreen(h, &mut origin).ok()?;
    }
    Ok(format!(
        "client=({},{},{},{}),outer=({},{},{},{}),client_screen_origin=({},{}),scale={scale}",
        client.left,
        client.top,
        client.right,
        client.bottom,
        outer.left,
        outer.top,
        outer.right,
        outer.bottom,
        origin.x,
        origin.y
    ))
}
// 从桌面 DC 读取合成窗口客户区的小块，验证 DWM 实际呈现；不写图像文件。
fn patch(h: HWND, scale: f32, rect: (f32, f32, f32, f32)) -> Result<Patch> {
    own(h)?;
    ensure(
        unsafe { GetForegroundWindow() } == h,
        "QA 合成窗口被外部窗口打断",
    )?;
    let (left, top) = (
        (rect.0 * scale).round() as i32,
        (rect.1 * scale).round() as i32,
    );
    let (width, height) = (
        (rect.2 * scale).round().max(1.) as u32,
        (rect.3 * scale).round().max(1.) as u32,
    );
    let mut client = RECT::default();
    unsafe {
        GetClientRect(h, &mut client)?;
    }
    ensure(
        left >= 0
            && top >= 0
            && left + width as i32 <= client.right
            && top + height as i32 <= client.bottom,
        "像素探针越出自有客户区",
    )?;
    let mut origin = POINT { x: left, y: top };
    unsafe {
        ClientToScreen(h, &mut origin).ok()?;
    }
    struct Surface {
        source: HDC,
        dc: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
    }
    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                if !self.old.is_invalid() {
                    SelectObject(self.dc, self.old);
                }
                if !self.bitmap.is_invalid() {
                    let _ = DeleteObject(self.bitmap.into());
                }
                if !self.dc.is_invalid() {
                    let _ = DeleteDC(self.dc);
                }
                if !self.source.is_invalid() {
                    ReleaseDC(None, self.source);
                }
            }
        }
    }
    let mut surface = Surface {
        source: unsafe { GetDC(None) },
        dc: HDC::default(),
        bitmap: HBITMAP::default(),
        old: HGDIOBJ::default(),
    };
    ensure(!surface.source.is_invalid(), "GetDC 失败")?;
    surface.dc = unsafe { CreateCompatibleDC(Some(surface.source)) };
    ensure(!surface.dc.is_invalid(), "CreateCompatibleDC 失败")?;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    surface.bitmap = unsafe {
        CreateDIBSection(
            Some(surface.source),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )?
    };
    ensure(!bits.is_null(), "DIB 像素为空")?;
    surface.old = unsafe { SelectObject(surface.dc, surface.bitmap.into()) };
    ensure(!surface.old.is_invalid(), "SelectObject 失败")?;
    unsafe {
        BitBlt(
            surface.dc,
            0,
            0,
            width as i32,
            height as i32,
            Some(surface.source),
            origin.x,
            origin.y,
            SRCCOPY,
        )?;
        GdiFlush().ok()?;
    }
    let pixels = unsafe {
        std::slice::from_raw_parts(bits.cast::<u8>(), width as usize * height as usize * 4)
    }
    .to_vec();
    Ok(Patch {
        pixels,
        width,
        height,
        left: left as f32 / scale,
        top: top as f32 / scale,
        scale,
    })
}

fn verify_ink(patch: &Patch, curve: bool, added: bool) -> Result<usize> {
    let mut levels = BTreeSet::new();
    for x in 8..patch.width.saturating_sub(8) {
        let global_x = patch.left + (x as f32 + 0.5) / patch.scale;
        let y = if curve {
            let t = (global_x - 40.) / 270.;
            180. - 150. * t * (1. - t)
        } else if added {
            275. + (global_x - 70.) * 37. / 280.
        } else {
            70. + (global_x - 40.) * 27. / 270.
        };
        let center = ((y - patch.top) * patch.scale) as i32;
        let radius = (6. * patch.scale).ceil() as i32;
        let mut darkest = 255;
        for row in center - radius..=center + radius {
            if row < 0 || row >= patch.height as i32 {
                continue;
            }
            let green = patch.green(x, row as u32);
            darkest = darkest.min(green);
            if green > 151 && green < 250 {
                levels.insert(green);
            }
        }
        // 独立计算白底 alpha 混合：80*(166/255)+255*(89/255)≈141。
        ensure(
            (135..=148).contains(&darkest),
            format!("路径缺口或透明度错误：x={global_x:.1}, green={darkest}"),
        )?;
    }
    ensure(
        levels.len() >= 3,
        format!("路径边缘仍是硬阶梯：仅 {} 个中间覆盖值", levels.len()),
    )?;
    Ok(levels.len())
}

async fn ink_ready(
    h: HWND,
    scale: f32,
    rect: (f32, f32, f32, f32),
    curve: bool,
    added: bool,
    cx: &mut AsyncApp,
) -> Result<usize> {
    let mut levels = 0;
    let mut last_error = String::new();
    let result = wait(cx, "路径 AA 与透明度实际呈现", |_| {
        flush()?;
        let pixels = patch(h, scale, rect)?;
        match verify_ink(&pixels, curve, added) {
            Ok(found) => {
                levels = found;
                Ok(true)
            }
            Err(error) => {
                last_error = format!("{error}；roi_rgb={:?}", pixels.representative_rgb());
                Ok(false)
            }
        }
    })
    .await;
    if let Err(error) = result {
        return Err(format!(
            "{error}；最后像素判定：{last_error}；{}",
            geometry(h, scale)?
        )
        .into());
    }
    Ok(levels)
}

async fn painted(
    window: WindowHandle<Fixture>,
    stage: u8,
    expected: Option<(f32, f32)>,
    cx: &mut AsyncApp,
) -> Result<(HWND, f32)> {
    wait(cx, "合成窗口绘制、焦点及物理视口", |cx| {
        window
            .update(cx, |v, w, _| -> Result<bool> {
                let h = hwnd(w);
                let mut client = RECT::default();
                unsafe {
                    GetClientRect(h, &mut client)?;
                }
                let scale = w.scale_factor();
                let viewport = w.viewport_size();
                let aligned = (f32::from(viewport.width) * scale - client.right as f32).abs() <= 1.
                    && (f32::from(viewport.height) * scale - client.bottom as f32).abs() <= 1.;
                let resized = expected.is_none_or(|(width, height)| {
                    (f32::from(viewport.width) - width).abs() <= 1.
                        && (f32::from(viewport.height) - height).abs() <= 1.
                });
                Ok(v.painted.get() == stage
                    && aligned
                    && resized
                    && w.is_window_active()
                    && unsafe { GetForegroundWindow() } == h)
            })
            .map_err(|e| e.to_string())?
    })
    .await?;
    flush()?;
    window
        .update(cx, |_, w, _| (hwnd(w), w.scale_factor()))
        .map_err(|e| e.to_string().into())
}

async fn exercise(
    window: WindowHandle<Fixture>,
    close: &CloseProof,
    cx: &mut AsyncApp,
) -> Result<()> {
    let native = window
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    own(native)?;
    unsafe {
        SetWindowPos(
            native,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
        )?;
    }
    let (h, scale) = painted(window, 0, None, cx).await?;
    let mut last_quad_state = (false, false);
    let mut last_rgb = String::new();
    let first_quad = wait(cx, "首帧合成色块实际呈现", |_| {
        flush()?;
        let background = patch(h, scale, (80., 45., 10., 10.))?;
        let marker = patch(h, scale, (18., 18., 8., 8.))?;
        last_rgb = format!(
            "white_roi_rgb={:?},marker_roi_rgb={:?}",
            background.representative_rgb(),
            marker.representative_rgb()
        );
        last_quad_state = (
            background
                .pixels
                .chunks_exact(4)
                .all(|p| p[..3] == [255, 255, 255]),
            marker
                .pixels
                .chunks_exact(4)
                .all(|p| p[1].abs_diff(104) <= 1),
        );
        Ok(last_quad_state.0 && last_quad_state.1)
    })
    .await;
    if let Err(error) = first_quad {
        return Err(format!(
            "{error}；白底通过={}，彩色标记通过={}；{last_rgb}；{}",
            last_quad_state.0,
            last_quad_state.1,
            geometry(h, scale)?
        )
        .into());
    }
    pass("path_resource_quad_first", "首帧仅色块，没有路径");

    window
        .update(cx, |v, _, cx| {
            v.stage = 1;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    let (h, scale) = painted(window, 1, None, cx).await?;
    let line_levels = ink_ready(h, scale, (55., 60., 240., 45.), false, false, cx).await?;
    let curve_levels = ink_ready(h, scale, (65., 135., 220., 54.), true, false, cx).await?;
    pass(
        "path_resource_first_use_aa_alpha",
        &format!("斜线覆盖值{line_levels}，曲线覆盖值{curve_levels}，白底透明度正确"),
    );

    window
        .update(cx, |v, _, cx| {
            v.stage = 2;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    // 此处是前台任务，未借用 App/Window；原生 WM_SIZE 能正常回调 GPUI。
    own(h)?;
    let (mut outer, mut client) = (RECT::default(), RECT::default());
    unsafe {
        GetWindowRect(h, &mut outer)?;
        GetClientRect(h, &mut client)?;
    }
    let width = (500. * scale).round() as i32 + outer.right - outer.left - client.right;
    let height = (360. * scale).round() as i32 + outer.bottom - outer.top - client.bottom;
    unsafe {
        SetWindowPos(
            h,
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    let (h, scale) = painted(window, 2, Some((500., 360.)), cx).await?;
    ink_ready(h, scale, (55., 60., 240., 45.), false, false, cx).await?;
    ink_ready(h, scale, (65., 135., 220., 54.), true, false, cx).await?;
    window
        .update(cx, |v, _, cx| {
            v.stage = 3;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    let (h, scale) = painted(window, 3, Some((500., 360.)), cx).await?;
    let levels = ink_ready(h, scale, (90., 270., 230., 54.), false, true, cx).await?;
    pass(
        "path_resource_resize_rebuild_and_new_path",
        &format!("真实WM_SIZE后旧路径与新路径保留AA，新路径覆盖值{levels}"),
    );

    own(h)?;
    close.native.set(h.0 as usize);
    close.requested.set(true);
    unsafe {
        PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0))?;
    }
    Ok(())
}

pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    std::env::set_var(
        "RUSTSHOT_DATA_DIR",
        std::env::current_dir()?.join("target/qa/path-resources/settings"),
    );
    let errors = Rc::new(RefCell::new(None::<String>));
    let final_errors = errors.clone();
    let complete = Rc::new(Cell::new(false));
    let final_complete = complete.clone();
    let close = Rc::new(CloseProof::default());
    let final_close = close.clone();
    Application::new().run(move |cx: &mut App| {
        let observed_close = close.clone();
        let observed_complete = complete.clone();
        cx.on_window_closed(move |cx| {
            if cx.windows().is_empty() {
                observed_close.app_empty.set(true);
                if observed_close.requested.get() { observed_complete.set(true); }
            }
        }).detach();
        let painted = Rc::new(Cell::new(u8::MAX));
        let bounds = Bounds::centered(None, size(px(420.), px(260.)), cx);
        let opened = cx.open_window(WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), kind: gpui::WindowKind::PopUp, titlebar: Some(TitlebarOptions { appears_transparent: false, ..Default::default() }), ..Default::default() }, move |w, cx| {
            w.set_window_title("RustShot · 合成路径资源专项");
            cx.new(|_| Fixture { stage: 0, painted })
        });
        let window = match opened { Ok(window) => window, Err(error) => { *errors.borrow_mut() = Some(error.to_string()); cx.quit(); return; } };
        let setup = window.update(cx, |_, w, _| -> Result<()> { crate::gpui_smoke::isolate_input(hwnd(w))?; w.activate_window(); Ok(()) });
        if let Err(error) = setup.map_err(|e| e.to_string()).and_then(|result| result.map_err(|e| e.to_string())) { *errors.borrow_mut() = Some(error); cx.quit(); return; }
        let watch_errors = errors.clone();
        let watch_complete = complete.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(40)).await;
            if !watch_complete.get() { let _ = cx.update(|cx| { *watch_errors.borrow_mut() = Some("路径资源专项全程超时".into()); cx.quit(); }); }
        }).detach();
        cx.spawn(async move |cx| {
            if let Err(error) = exercise(window, &close, cx).await {
                    let detail = error.to_string();
                    println!("{{\"suite\":\"path-resources\",\"test\":\"path_resource_failed\",\"detail\":{:?},\"ok\":false}}", detail);
                    *errors.borrow_mut() = Some(detail);
                    let _ = window.update(cx, |_, w, _| w.remove_window());
                    let _ = cx.update(|cx| cx.quit());
            }
        }).detach();
        cx.activate(true);
    });
    if let Some(error) = final_errors.borrow_mut().take() {
        return Err(error.into());
    }
    ensure(
        final_close.requested.get(),
        "路径资源专项未发出正常关闭请求即退出",
    )?;
    ensure(
        final_complete.get() && final_close.app_empty.get(),
        "关闭后GPUI窗口列表未确认清空",
    )?;
    ensure(
        final_close.native.get() != 0
            && !unsafe { IsWindow(Some(HWND(final_close.native.get() as *mut _))) }.as_bool(),
        "Application退出后自有HWND仍存在",
    )?;
    pass(
        "path_resource_window_recycled",
        "正常关闭请求、GPUI列表清空及Application退出后HWND销毁均已确认",
    );
    Ok(())
}
