//! 自有 Win32 色块的 GDI/DXGI 对照；桌面数据只暂存内存，不输出图片或实际桌面颜色。
use crate::{
    canvas_source::CanvasSource,
    capture::Image,
    desktop::DesktopCapture,
    fast_capture::FrozenDesktop,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, SourcePixels},
    Result,
};
use std::{mem::size_of, sync::Arc, time::Instant};
use windows::{
    core::{w, BOOL, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{CreateMutexW, GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS},
        },
        UI::{HiDpi::*, WindowsAndMessaging::*},
    },
};
const PATCH_W: i32 = 64;
const PATCH_H: i32 = 56;
const MARGIN: i32 = 8;
const WIDTH: i32 = PATCH_W * 4 + MARGIN * 2;
const HEIGHT: i32 = PATCH_H * 2 + MARGIN * 2;
const COLORS: [u32; 8] = [
    0xffffff, 0x202020, 0x404040, 0x808080, 0xc0c0c0, 0xff0000, 0x00ff00, 0x0000ff,
];
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}
struct PaintState {
    solid: Option<u32>,
}
unsafe fn fill(dc: HDC, rect: RECT, rgb: u32) {
    let brush = CreateSolidBrush(COLORREF(
        ((rgb & 255) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 255),
    ));
    let _ = FillRect(dc, &rect, brush);
    let _ = DeleteObject(brush.into());
}
unsafe extern "system" fn procedure(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let c = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(h, GWLP_USERDATA, c.lpCreateParams as isize);
            LRESULT(1)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(h, &mut paint);
            let mut client = RECT::default();
            let _ = GetClientRect(h, &mut client);
            let state = (GetWindowLongPtrW(h, GWLP_USERDATA) as *const PaintState).as_ref();
            let solid = state.and_then(|s| s.solid);
            fill(dc, client, solid.unwrap_or(0xffffff));
            if solid.is_none() {
                for (i, color) in COLORS.into_iter().enumerate() {
                    let left = MARGIN + i as i32 % 4 * PATCH_W;
                    let top = MARGIN + i as i32 / 4 * PATCH_H;
                    fill(
                        dc,
                        RECT {
                            left,
                            top,
                            right: left + PATCH_W,
                            bottom: top + PATCH_H,
                        },
                        color,
                    );
                }
            }
            let _ = EndPaint(h, &paint);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            DefWindowProcW(h, message, wp, lp)
        }
        _ => DefWindowProcW(h, message, wp, lp),
    }
}
struct FixtureWindow {
    native: HWND,
    rect: ScreenRect,
    _state: Box<PaintState>,
}
impl Drop for FixtureWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.native);
        }
    }
}
struct Fixtures {
    windows: Vec<FixtureWindow>,
    class: Vec<u16>,
    instance: HINSTANCE,
    foreground: HWND,
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        self.windows.clear();
        unsafe {
            let _ = UnregisterClassW(PCWSTR(self.class.as_ptr()), Some(self.instance));
            let current = GetForegroundWindow();
            let mut owner = 0;
            GetWindowThreadProcessId(current, Some(&mut owner));
            if owner == std::process::id() && IsWindow(Some(self.foreground)).as_bool() {
                let _ = SetForegroundWindow(self.foreground);
            }
        }
    }
}
impl Fixtures {
    fn new() -> Result<Self> {
        let class: Vec<u16> = format!("RustShot.FastCaptureFixture.{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let instance = unsafe { GetModuleHandleW(None)? }.into();
        let foreground = unsafe { GetForegroundWindow() };
        let atom = unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            })
        };
        if atom == 0 {
            return Err("无法注册自有色块窗口".into());
        }
        Ok(Self {
            windows: Vec::new(),
            class,
            instance,
            foreground,
        })
    }
    fn add(&mut self, rect: ScreenRect, solid: Option<u32>, layered: bool) -> Result<usize> {
        let state = Box::new(PaintState { solid });
        let native = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW
                    | if layered {
                        WS_EX_LAYERED
                    } else {
                        WINDOW_EX_STYLE(0)
                    },
                PCWSTR(self.class.as_ptr()),
                w!("RustShot · 自有截图色块"),
                WS_POPUP,
                rect.left,
                rect.top,
                rect.width as i32,
                rect.height as i32,
                None,
                None,
                Some(self.instance),
                Some((&*state as *const PaintState).cast()),
            )?
        };
        let index = self.windows.len();
        self.windows.push(FixtureWindow {
            native,
            rect,
            _state: state,
        });
        if layered {
            unsafe { SetLayeredWindowAttributes(native, COLORREF(0), 128, LWA_ALPHA)? };
        }
        unsafe {
            SetWindowPos(
                native,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.width as i32,
                rect.height as i32,
                SWP_SHOWWINDOW | SWP_NOACTIVATE,
            )?;
            UpdateWindow(native).ok()?;
        }
        Ok(index)
    }
}
fn work_areas() -> Result<Vec<RECT>> {
    unsafe extern "system" fn visit(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(m, &mut info).as_bool() {
            (*(data.0 as *mut Vec<RECT>)).push(info.rcWork);
        }
        BOOL(1)
    }
    let mut rects = Vec::<RECT>::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM(&mut rects as *mut _ as isize),
        )
        .ok()?;
    }
    if rects.is_empty() {
        return Err("未找到用于色块验收的活动显示器".into());
    }
    Ok(rects)
}
fn settle() -> Result<()> {
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        GdiFlush().ok()?;
        if DwmFlush() < 0 {
            return Err("自有色块 DWM 呈现失败".into());
        }
        Ok(())
    }
}
fn local(rect: ScreenRect, bounds: ScreenRect) -> Result<Rect> {
    let x = rect.left - bounds.left;
    let y = rect.top - bounds.top;
    if x < 0 || y < 0 {
        return Err("自有色块不在冻结桌面中".into());
    }
    let r = Rect {
        x: x as u32,
        y: y as u32,
        width: rect.width,
        height: rect.height,
    };
    if !r.valid(bounds.width, bounds.height) {
        return Err("自有色块超出冻结桌面".into());
    }
    Ok(r)
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    image.bgra[((y * image.width + x) * 4) as usize..][..4]
        .try_into()
        .expect("own fixture pixel")
}
fn bgra(rgb: u32) -> [u8; 4] {
    [rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8, 255]
}
#[derive(Default)]
struct ErrorStats {
    maximum: u16,
    total: u64,
    count: u64,
    nonopaque: usize,
}
impl ErrorStats {
    fn add(&mut self, actual: [u8; 4], expected: [u8; 4]) {
        for c in 0..3 {
            let d = actual[c].abs_diff(expected[c]) as u16;
            self.maximum = self.maximum.max(d);
            self.total += d as u64;
            self.count += 1;
        }
        self.nonopaque += usize::from(actual[3] != 255);
    }
    fn mean(&self) -> f64 {
        self.total as f64 / self.count.max(1) as f64
    }
}
fn sample(image: &Image, other: Option<&Image>) -> ErrorStats {
    let mut stats = ErrorStats::default();
    for (i, color) in COLORS.into_iter().enumerate() {
        let x = (MARGIN + i as i32 % 4 * PATCH_W + PATCH_W / 2) as u32;
        let y = (MARGIN + i as i32 / 4 * PATCH_H + PATCH_H / 2) as u32;
        for dy in 0..5 {
            for dx in 0..5 {
                let a = pixel(image, x + dx - 2, y + dy - 2);
                let b = other.map_or(bgra(color), |b| pixel(b, x + dx - 2, y + dy - 2));
                stats.add(a, b);
            }
        }
    }
    stats
}
fn gdi_resources() -> u32 {
    unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) }
}

pub(crate) fn run() -> Result<()> {
    let started = Instant::now();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\RustShot.MVP.Capture"))? };
    let _lock = crate::gpui_capture::CaptureLock::new(mutex);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err("已有截图会话；未启动 GDI 色块验收".into());
    }
    let areas = work_areas()?;
    let mut fixtures = Fixtures::new()?;
    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor)? };
    for work in &areas {
        if work.right - work.left < WIDTH + 24 || work.bottom - work.top < HEIGHT + 24 {
            return Err("工作区不足以显示自有色块".into());
        }
        let left = work.left + 12;
        let mut top = work.top + 12;
        if cursor.x >= left && cursor.x < left + WIDTH && cursor.y >= top && cursor.y < top + HEIGHT
        {
            top = work.bottom - HEIGHT - 12;
        }
        fixtures.add(
            ScreenRect {
                left,
                top,
                width: WIDTH as u32,
                height: HEIGHT as u32,
            },
            None,
            false,
        )?;
    }
    settle()?;
    let before_gdi = gdi_resources();
    let frozen = FrozenDesktop::capture()?;
    let after_gdi = gdi_resources();
    if before_gdi != after_gdi {
        return Err(format!("GDI 冻结设备对象未回收：{before_gdi}→{after_gdi}").into());
    }
    let dxgi = DesktopCapture::capture()?;
    let previews: Vec<_> = frozen.screens.iter().map(|s| s.preview.clone()).collect();
    let (source, source_previews) = CanvasSource::frozen_desktop(frozen);
    if source.is_image() || source.screen_count() != source_previews.len() {
        return Err("GDI 桌面错误进入文件编辑语义".into());
    }
    for (i, p) in source_previews.iter().enumerate() {
        if !Arc::ptr_eq(p, &previews[i]) || !std::ptr::eq(source.original(p), p.as_ref()) {
            return Err("冻结桌面原始/预览未共享 Arc".into());
        }
    }
    let pixels = SourcePixels {
        fill: None,
        canvas: source.full(),
        overlay: None,
        planes: source_previews
            .iter()
            .enumerate()
            .map(|(i, p)| PixelPlane {
                rect: source.screen_rect(i),
                bgra: p.as_bytes(0).expect("frozen preview"),
            })
            .collect(),
    };
    for (i, fixture) in fixtures.windows.iter().enumerate() {
        let region = local(fixture.rect, source.bounds)?;
        let gdi_image = source.read_region(region)?;
        let dxgi_image = dxgi.read_region(local(fixture.rect, dxgi.bounds)?)?;
        if pixels.read(region).bgra != gdi_image.bgra {
            return Err("Scene/导出未复用同一份冻结字节".into());
        }
        let g = sample(&gdi_image, None);
        let d = sample(&dxgi_image, None);
        let cross = sample(&gdi_image, Some(&dxgi_image));
        let color_ok = g.maximum == 0 && g.nonopaque == 0;
        println!("{{\"phase\":\"fast_capture_owned_patches\",\"monitor\":{i},\"width\":{WIDTH},\"height\":{HEIGHT},\"patches\":8,\"gdi_max_error\":{},\"gdi_mean_error\":{:.6},\"dxgi_max_error\":{},\"dxgi_mean_error\":{:.6},\"cross_max_error\":{},\"cross_mean_error\":{:.6},\"gdi_nonopaque\":{},\"dxgi_nonopaque\":{},\"gdi_exact_expected\":{},\"dxgi_exact_expected\":{},\"source_arc_shared\":true,\"scene_export_bytes_equal\":true,\"gdi_objects_recycled\":true,\"gdi_capture_ms\":{:.3},\"dxgi_capture_ms\":{:.3},\"ok\":{color_ok}}}",g.maximum,g.mean(),d.maximum,d.mean(),cross.maximum,cross.mean(),g.nonopaque,d.nonopaque,g.maximum==0,d.maximum==0,source.capture_ms,dxgi.capture_ms);
        if !color_ok {
            return Err(format!(
                "自有色块 GDI 颜色/alpha 不一致：最大色差 {}，非不透明像素 {}",
                g.maximum, g.nonopaque
            )
            .into());
        }
    }
    drop(pixels);
    drop(source_previews);
    drop(previews);
    drop(source);
    drop(dxgi);
    // 小型半透明洋红窗只覆盖自有白色块。
    let first = fixtures.windows[0].rect;
    let patch = ScreenRect {
        left: first.left + MARGIN + PATCH_W / 2 - 12,
        top: first.top + MARGIN + PATCH_H / 2 - 12,
        width: 24,
        height: 24,
    };
    let layered = fixtures.add(patch, Some(0xff00ff), true)?;
    settle()?;
    let frozen = FrozenDesktop::capture()?;
    let image = frozen.read_region(local(patch, frozen.bounds)?)?;
    let actual = pixel(&image, 12, 12);
    let mut error = ErrorStats::default();
    error.add(actual, bgra(0xff7fff));
    let included = actual[1] < 230 && actual[0] > actual[1] && actual[2] > actual[1];
    println!("{{\"phase\":\"fast_capture_owned_layered\",\"included\":{included},\"expected_max_error\":{},\"alpha_opaque\":{},\"ok\":{}}}",error.maximum,actual[3]==255,included&&actual[3]==255);
    if !included || actual[3] != 255 {
        return Err("GDI 未包含自有半透明 layered 窗口".into());
    }
    drop(frozen);
    unsafe {
        SetLayeredWindowAttributes(
            fixtures.windows[layered].native,
            COLORREF(0),
            255,
            LWA_ALPHA,
        )?;
        SetWindowDisplayAffinity(fixtures.windows[layered].native, WDA_EXCLUDEFROMCAPTURE)?;
    }
    settle()?;
    let frozen = FrozenDesktop::capture()?;
    let image = frozen.read_region(local(patch, frozen.bounds)?)?;
    let actual = pixel(&image, 12, 12);
    let absent =
        !(actual[0] > actual[1].saturating_add(30) && actual[2] > actual[1].saturating_add(30));
    let restored = actual[..3] == [255, 255, 255];
    let black = actual[..3] == [0, 0, 0];
    println!("{{\"phase\":\"fast_capture_owned_excluded\",\"protected_patch_absent\":{absent},\"background_restored\":{restored},\"black_placeholder\":{black},\"alpha_opaque\":{},\"backend\":\"gdi_direct_no_dxgi_retry\",\"ok\":{}}}",actual[3]==255,absent&&actual[3]==255);
    if !absent || actual[3] != 255 {
        return Err("排除截图的自有窗口仍泄漏测试图案".into());
    }
    drop(frozen);
    drop(fixtures);
    println!("{{\"phase\":\"fast_capture_owned_recycle\",\"windows_destroyed\":true,\"elapsed_ms\":{:.3},\"ok\":true}}",started.elapsed().as_secs_f64()*1000.);
    Ok(())
}
