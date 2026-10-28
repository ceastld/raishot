//! 启动首屏专项：先冻结自己的原生图，再验证启动层与 GPUI 交接。
//! 所有像素断言仅限这张图；不保存桌面或派发系统鼠标/剪贴板输入。
use crate::{
    annotation::Tool,
    capture::Image,
    capture_view::CaptureView,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::{hwnd, CaptureLock},
    gpui_smoke::{self, SMOKE_MOVE},
    save_panel::SavePanel,
    save_store::SaveStore,
    startup_surface::StartupSurface,
    Result,
};
use gpui::{AnyWindowHandle, App, AppContext, AsyncApp, Entity, RenderImage, WeakEntity, Window};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::Write,
    mem::size_of,
    path::PathBuf,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use windows::{
    core::{w, BOOL, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{
                CreateMutexW, GetCurrentProcess, GetGuiResources, OpenThread, WaitForSingleObject,
                GR_GDIOBJECTS, THREAD_SYNCHRONIZE,
            },
        },
        UI::{HiDpi::*, Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::*},
    },
};

const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;
const WAIT: Duration = Duration::from_secs(8);
const QA_CAPTURED_CONTROL: usize = 1 << 25;
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
fn report_root() -> PathBuf {
    std::env::var_os("RUSTSHOT_SURFACE_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "target/qa/startup-surface/worker-{}",
                std::process::id()
            ))
        })
}
fn record(case: &str, details: serde_json::Value, ok: bool) -> Result<()> {
    let root = report_root();
    std::fs::create_dir_all(&root)?;
    let item = serde_json::json!({"suite":"startup-surface","test":case,"pid":std::process::id(),
        "owned_only":true,"detail":details,"ok":ok});
    println!("{item}");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("ui.jsonl"))?;
    writeln!(file, "{item}")?;
    file.flush()?;
    Ok(())
}
fn pattern() -> Image {
    let mut bgra = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            bgra.extend_from_slice(&[
                (30 + (x * 7 + y * 3) % 171) as u8,
                (40 + (x * 3 + y * 11) % 151) as u8,
                (50 + (x * 13 + y * 5) % 131) as u8,
                255,
            ]);
        }
    }
    Image {
        width: WIDTH,
        height: HEIGHT,
        bgra,
        readback_ms: 0.,
    }
}
unsafe extern "system" fn fixture_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let cs = &*(lp.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(h, GWLP_USERDATA, cs.lpCreateParams as isize);
    }
    let source = GetWindowLongPtrW(h, GWLP_USERDATA) as *const Image;
    match message {
        WM_PAINT if !source.is_null() => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(h, &mut ps);
            let image = &*source;
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: WIDTH as i32,
                    biHeight: -(HEIGHT as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            StretchDIBits(
                dc,
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                Some(image.bgra.as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            let _ = GdiFlush();
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(h, GWLP_USERDATA, 0);
            DefWindowProcW(h, message, wp, lp)
        }
        _ => DefWindowProcW(h, message, wp, lp),
    }
}
struct Fixture {
    native: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
    region: ScreenRect,
    _image: Box<Image>,
}
impl Fixture {
    fn new() -> Result<Self> {
        Self::create(false)
    }
    fn create(focus_owner: bool) -> Result<Self> {
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            GetMonitorInfoW(monitor, &mut info).ok()?;
        }
        ensure(
            info.rcWork.right - info.rcWork.left >= WIDTH as i32 + 80
                && info.rcWork.bottom - info.rcWork.top >= HEIGHT as i32 + 80,
            "工作区不足以显示自有首屏验收图",
        )?;
        let region = ScreenRect {
            left: info.rcWork.left + 40,
            top: info.rcWork.top + 40,
            width: WIDTH,
            height: HEIGHT,
        };
        let image = Box::new(pattern());
        let class: Vec<u16> = format!(
            "RustShot.StartupSurfaceFixture.{}.{}",
            std::process::id(),
            unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
        )
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let instance = unsafe { GetModuleHandleW(None)? }.into();
        let atom = unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(fixture_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            })
        };
        ensure(atom != 0, "首屏验收图原生类注册失败")?;
        let created = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | if focus_owner {
                        WINDOW_EX_STYLE(0)
                    } else {
                        WS_EX_NOACTIVATE
                    },
                PCWSTR(class.as_ptr()),
                w!("RustShot · 自有首屏像素"),
                WS_POPUP,
                region.left,
                region.top,
                WIDTH as i32,
                HEIGHT as i32,
                None,
                None,
                Some(instance),
                Some((&*image as *const Image).cast()),
            )
        };
        let native = match created {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    let _ = UnregisterClassW(PCWSTR(class.as_ptr()), Some(instance));
                }
                return Err(e.into());
            }
        };
        let fixture = Self {
            native,
            class,
            instance,
            region,
            _image: image,
        };
        unsafe {
            SetWindowPos(
                native,
                Some(HWND_TOPMOST),
                region.left,
                region.top,
                WIDTH as i32,
                HEIGHT as i32,
                SWP_SHOWWINDOW | SWP_NOACTIVATE,
            )?;
            UpdateWindow(native).ok()?;
        }
        Ok(fixture)
    }
    fn freeze(&self) -> Result<Image> {
        own_window(self.native, self.region)?;
        let expected = pattern();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            ensure(unsafe { DwmFlush() } >= 0, "首屏验收 DWM 同步失败")?;
            let image = read_owned(self.region)?;
            if image.bgra == expected.bgra {
                return Ok(image);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("自有首屏图未完整显示；没有保存或输出不匹配的桌面像素".into())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.native);
            let _ = UnregisterClassW(PCWSTR(self.class.as_ptr()), Some(self.instance));
        }
    }
}

/// 仅读取指定的、自有窗口完整覆盖区域。调用方必须先核查 native rect。
fn read_owned(region: ScreenRect) -> Result<Image> {
    unsafe {
        let screen = GetDC(None);
        ensure(!screen.is_invalid(), "不能读取自有原生窗口")?;
        let memory = CreateCompatibleDC(Some(screen));
        if memory.is_invalid() {
            ReleaseDC(None, screen);
            return Err("不能创建自有像素读回 DC".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: region.width as i32,
                biHeight: -(region.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0);
        let bitmap = match bitmap {
            Ok(b) => b,
            Err(e) => {
                let _ = DeleteDC(memory);
                ReleaseDC(None, screen);
                return Err(e.into());
            }
        };
        let previous = SelectObject(memory, bitmap.into());
        let captured = BitBlt(
            memory,
            0,
            0,
            region.width as i32,
            region.height as i32,
            Some(screen),
            region.left,
            region.top,
            SRCCOPY,
        );
        let flushed = GdiFlush().ok();
        let image = if captured.is_ok() && flushed.is_ok() && !bits.is_null() {
            let mut bgra = std::slice::from_raw_parts(
                bits as *const u8,
                region.width as usize * region.height as usize * 4,
            )
            .to_vec();
            for p in bgra.chunks_exact_mut(4) {
                p[3] = 255;
            }
            Some(Image {
                width: region.width,
                height: region.height,
                bgra,
                readback_ms: 0.,
            })
        } else {
            None
        };
        SelectObject(memory, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        captured?;
        flushed?;
        image.ok_or_else(|| "自有窗口像素读回无效".into())
    }
}
/// root 的专项准备分支使用真实自绘窗口冻结结果，避免载入第三方桌面图。
pub(crate) fn fixture_desktop() -> Result<FrozenDesktop> {
    let gdi_before = gdi_objects();
    let fixture = Fixture::new()?;
    let gdi_after_paint = gdi_objects();
    let image = fixture.freeze()?;
    let gdi_after_readback = gdi_objects();
    let bounds = fixture.region;
    let buffer =
        image::RgbaImage::from_raw(WIDTH, HEIGHT, image.bgra).ok_or("自绘冻结像素尺寸无效")?;
    let preview = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
    drop(fixture);
    record(
        "surface_owned_fixture_resources",
        serde_json::json!({"gdi_before":gdi_before,"gdi_after_paint":gdi_after_paint,
            "gdi_after_readback":gdi_after_readback,"gdi_after_destroy":gdi_objects()}),
        true,
    )?;
    record(
        "surface_owned_fixture_frozen",
        serde_json::json!({"width":WIDTH,"height":HEIGHT,"exact_pixels":true}),
        true,
    )?;
    Ok(FrozenDesktop {
        screens: vec![FrozenScreen {
            rect: bounds,
            preview,
        }],
        bounds,
        current: 0,
        capture_ms: 0.,
    })
}

fn pack(p: (i32, i32)) -> LPARAM {
    LPARAM((((p.1 as u16 as u32) << 16) | (p.0 as u16 as u32)) as isize)
}
fn own_window(h: HWND, region: ScreenRect) -> Result<()> {
    ensure(unsafe { IsWindow(Some(h)) }.as_bool(), "启动层窗口不存在")?;
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    ensure(pid == std::process::id(), "首屏输入目标不是本测试进程")?;
    let (mut outer, mut client) = (RECT::default(), RECT::default());
    unsafe {
        GetWindowRect(h, &mut outer)?;
        GetClientRect(h, &mut client)?;
    }
    ensure(
        outer.left == region.left
            && outer.top == region.top
            && outer.right - outer.left == region.width as i32
            && outer.bottom - outer.top == region.height as i32
            && client.right - client.left == region.width as i32
            && client.bottom - client.top == region.height as i32,
        "启动层未精确覆盖冻结图的物理范围",
    )
}
fn gdi_objects() -> u32 {
    unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) }
}
fn complete_report(stage: &str) -> Result<()> {
    let item = serde_json::json!({"phase":"startup_surface_smoke_complete","stage":stage,
        "pid":std::process::id(),"report":report_root().join("ui.jsonl"),"owned_only":true,"ok":true});
    println!("{item}");
    Ok(())
}

const START: (i32, i32) = (360, 240);
const END: (i32, i32) = (80, 64);
const ACTIVE_END: (i32, i32) = (60, 48);
fn expected_crop(end: (i32, i32)) -> Rect {
    Rect {
        x: end.0 as u32,
        y: end.1 as u32,
        width: (START.0 - end.0) as u32,
        height: (START.1 - end.1) as u32,
    }
}
fn native_bounds(h: HWND) -> Result<ScreenRect> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(h, &mut rect)?;
    }
    Ok(ScreenRect {
        left: rect.left,
        top: rect.top,
        width: (rect.right - rect.left) as u32,
        height: (rect.bottom - rect.top) as u32,
    })
}
fn post(h: HWND, message: u32, key: usize, p: (i32, i32)) -> Result<()> {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(h, Some(&mut pid));
    }
    ensure(pid == std::process::id(), "拒绝向非自有窗口派发输入")?;
    unsafe {
        PostMessageW(Some(h), message, WPARAM(key), pack(p))?;
    }
    Ok(())
}
fn begin_drag(surface: &StartupSurface) -> Result<()> {
    let h = surface.handle();
    post(h, WM_LBUTTONDOWN, 1, START)?;
    post(h, WM_MOUSEMOVE, SMOKE_MOVE | 1, END)?;
    wait_native(
        || {
            let state = surface.snapshot();
            Ok(state.dragging && state.crop == Some(expected_crop(END)) && capture_owner(h)? == h)
        },
        "原生反向拖动没有建立连续 mouse capture",
    )
}
fn release_drag(surface: &StartupSurface, end: (i32, i32)) -> Result<()> {
    let h = surface.handle();
    post(h, WM_MOUSEMOVE, SMOKE_MOVE | 1, end)?;
    post(h, WM_LBUTTONUP, 0, end)?;
    wait_native(
        || {
            let state = surface.snapshot();
            Ok(!state.dragging && state.crop == Some(expected_crop(end)) && capture_owner(h)? != h)
        },
        "原生抬起事件没有提交选区/释放 mouse capture",
    )
}
fn wait_native(mut ready: impl FnMut() -> Result<bool>, message: &str) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if ready()? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(message.to_owned().into())
}
fn capture_owner(h: HWND) -> Result<HWND> {
    let tid = unsafe { GetWindowThreadProcessId(h, None) };
    ensure(tid != 0, "原生首屏输入线程不存在")?;
    let mut gui = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetGUIThreadInfo(tid, &mut gui)?;
    }
    Ok(gui.hwndCapture)
}
struct NativeThread(HANDLE);
impl NativeThread {
    fn of(h: HWND) -> Result<Self> {
        let tid = unsafe { GetWindowThreadProcessId(h, None) };
        ensure(tid != 0, "不能观测首屏工作线程")?;
        Ok(Self(unsafe { OpenThread(THREAD_SYNCHRONIZE, false, tid)? }))
    }
    fn exited(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) == WAIT_OBJECT_0
    }
}
impl Drop for NativeThread {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Foreground(HWND);
impl Foreground {
    fn preserve() -> Self {
        Self(unsafe { GetForegroundWindow() })
    }
    fn restore_if_own(&self) {
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
            if pid == std::process::id() && IsWindow(Some(self.0)).as_bool() {
                let _ = SetForegroundWindow(self.0);
            }
        }
    }
}
impl Drop for Foreground {
    fn drop(&mut self) {
        self.restore_if_own();
    }
}
struct OwnerState {
    native: AtomicUsize,
    closing: AtomicBool,
}
/// 独立消息线程接收被拥有窗口销毁时的同步激活事件，QA主线程不在join中阻塞它。
struct FocusOwner {
    state: Arc<OwnerState>,
    worker: Option<JoinHandle<()>>,
}
impl FocusOwner {
    fn new() -> Result<Self> {
        let state = Arc::new(OwnerState {
            native: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
        });
        let shared = state.clone();
        let (ready, receiver) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("surface-qa-focus-owner".into())
            .spawn(move || {
                let fixture = match Fixture::create(true) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = ready.send(Err(e.to_string()));
                        return;
                    }
                };
                let result = (|| -> Result<()> {
                    own_window(fixture.native, fixture.region)?;
                    shared
                        .native
                        .store(fixture.native.0 as usize, Ordering::Release);
                    if shared.closing.load(Ordering::Acquire) {
                        return Err("首屏QA父窗口初始化已取消".into());
                    }
                    unsafe {
                        SetForegroundWindow(fixture.native).ok()?;
                    }
                    ensure(
                        unsafe { GetForegroundWindow() } == fixture.native,
                        "自有QA父窗口没有获得前台",
                    )?;
                    Ok(())
                })();
                if let Err(e) = result {
                    let _ = ready.send(Err(e.to_string()));
                    return;
                }
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while !shared.closing.load(Ordering::Acquire)
                    && unsafe { IsWindow(Some(fixture.native)) }.as_bool()
                {
                    let mut message = MSG::default();
                    let received = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
                    if received <= 0 {
                        break;
                    }
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                drop(fixture);
                shared.native.store(0, Ordering::Release);
            })?;
        let mut owner = Self {
            state,
            worker: Some(worker),
        };
        match receiver.recv_timeout(WAIT) {
            Ok(Ok(())) => Ok(owner),
            Ok(Err(error)) => {
                let _ = owner.close();
                Err(error.into())
            }
            Err(error) => {
                let _ = owner.close();
                Err(format!("首屏QA父窗口准备失败：{error}").into())
            }
        }
    }
    fn handle(&self) -> HWND {
        HWND(self.state.native.load(Ordering::Acquire) as *mut _)
    }
    fn close(&mut self) -> Result<()> {
        self.state.closing.store(true, Ordering::Release);
        let native = self.handle();
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(native, Some(&mut pid));
        }
        if pid == std::process::id() && unsafe { IsWindow(Some(native)) }.as_bool() {
            post(native, WM_CLOSE, 0, (0, 0))?;
        }
        let deadline = Instant::now() + WAIT;
        while self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            ensure(Instant::now() < deadline, "首屏QA父窗口线程未按期回收")?;
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "首屏QA父窗口线程异常")?;
        }
        ensure(
            !unsafe { IsWindow(Some(native)) }.as_bool(),
            "首屏QA父窗口未销毁",
        )
    }
}
impl Drop for FocusOwner {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
fn bind_surface_owner(surface: &StartupSurface, owner: &FocusOwner) -> Result<()> {
    let native = surface.handle();
    let parent = owner.handle();
    for h in [native, parent] {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(h, Some(&mut pid));
        }
        ensure(
            pid == std::process::id() && unsafe { IsWindow(Some(h)) }.as_bool(),
            "拒绝给非自有窗口绑定QA父窗口",
        )?;
    }
    unsafe {
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(native, GWLP_HWNDPARENT, parent.0 as isize);
        ensure(
            previous != 0 || GetLastError() == WIN32_ERROR(0),
            "不能设置自有首屏QA父窗口",
        )?;
        ensure(
            GetWindowLongPtrW(native, GWLP_HWNDPARENT) == parent.0 as isize,
            "首屏QA父窗口绑定未生效",
        )?;
    }
    Ok(())
}
fn expected_pixel(x: u32, y: u32) -> [u8; 4] {
    [
        (30 + (x * 7 + y * 3) % 171) as u8,
        (40 + (x * 3 + y * 11) % 151) as u8,
        (50 + (x * 13 + y * 5) % 131) as u8,
        255,
    ]
}
fn image_pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra[i..i + 4].try_into().expect("自有图像素")
}
/// 底部 hint 和选区轮廓属于 UI；仅检查它们之外的自有冻结图矩形。无选区首屏与 GPUI
/// 一致整屏暗化，像素恰为原图一次遮罩（±1 舍入）；预热纯黑或未暗化都不能通过。
fn wait_initial_pixels(h: HWND, region: ScreenRect) -> Result<()> {
    own_window(h, region)?;
    wait_native(
        || {
            ensure(unsafe { DwmFlush() } >= 0, "首屏原图 DwmFlush 失败")?;
            let image = read_owned(region)?;
            Ok((16..176).all(|y| {
                (16..WIDTH - 16).all(|x| {
                    let actual = image_pixel(&image, x, y);
                    let original = expected_pixel(x, y);
                    actual[..3]
                        .iter()
                        .zip(&original[..3])
                        .all(|(&value, &original)| {
                            value.abs_diff(crate::startup_surface::masked_channel(original)) <= 1
                        })
                })
            }))
        },
        "原生首屏未按整屏遮罩显示自有冻结图",
    )
}
fn wait_selection_pixels(h: HWND, region: ScreenRect) -> Result<()> {
    own_window(h, region)?;
    wait_native(
        || {
            ensure(unsafe { DwmFlush() } >= 0, "选区像素 DwmFlush 失败")?;
            let image = read_owned(region)?;
            let inside = (110..160)
                .all(|y| (120..180).all(|x| image_pixel(&image, x, y) == expected_pixel(x, y)));
            let dim = image_pixel(&image, 24, 24);
            let original = expected_pixel(24, 24);
            Ok(inside && dim[..3].iter().zip(&original[..3]).all(|(a, b)| a < b))
        },
        "原生选区内原图或选区外遮罩像素不正确",
    )
}
fn start_own_surface() -> Result<(StartupSurface, ScreenRect)> {
    let frozen = fixture_desktop()?;
    let bounds = frozen.bounds;
    let planes = frozen
        .screens
        .into_iter()
        .map(|s| (s.rect.local(bounds), s.preview))
        .collect();
    let surface = StartupSurface::start(bounds, planes, 0, None, Vec::new(), Instant::now())?;
    own_window(surface.handle(), bounds)?;
    Ok((surface, bounds))
}
/// 不创建 GPUI Application：真实原生画面、反向框选、早期取消与线程回收。
pub(crate) fn native_run() -> Result<()> {
    let _foreground = Foreground::preserve();
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\RustShot.MVP.Capture"))? };
    let lock = CaptureLock::new(mutex);
    ensure(
        unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
        "已有截图会话，未启动原生首屏验收",
    )?;
    let owner_gdi_before = gdi_objects();
    let mut focus_owner = FocusOwner::new()?;
    let owner_native = focus_owner.handle().0 as usize;
    record(
        "surface_gdi_focus_owner_observation",
        serde_json::json!({"measurement_only":true,
        "gdi_before_owner":owner_gdi_before,"gdi_after_owner":gdi_objects(),"owned_foreground":true}),
        true,
    )?;
    // 冷首次是独立测量，不把保留的 GDI 对象自动判为缓存或忽略后续增长。
    // 原三个案例仍分别使用当轮 before，并维持 after <= before 的严格要求。
    let cold_before = gdi_objects();
    let (cold_surface, _) = start_own_surface()?;
    bind_surface_owner(&cold_surface, &focus_owner)?;
    let cold_native = cold_surface.handle();
    let cold_thread = NativeThread::of(cold_native)?;
    let cold_created = gdi_objects();
    cold_surface.dismiss()?;
    ensure(
        !unsafe { IsWindow(Some(cold_native)) }.as_bool() && cold_thread.exited(),
        "冷首次观察关闭后仍有原生窗口或工作线程",
    )?;
    let cold_exited = gdi_objects();
    std::thread::sleep(Duration::from_millis(200));
    let cold_after = gdi_objects();
    record(
        "surface_gdi_cold_observation",
        serde_json::json!({
        "measurement_only":true,"gdi_before":cold_before,"gdi_native_created":cold_created,
        "gdi_thread_exited":cold_exited,"gdi_after_200ms":cold_after,
        "delta":cold_after as i64-cold_before as i64,"native_closed":true,"thread_exited":true,
        "following_cases_require_no_growth":true}),
        true,
    )?;
    drop(cold_thread);
    drop(cold_surface);
    for case in ["reverse", "escape", "right"] {
        let before = gdi_objects();
        let (surface, region) = start_own_surface()?;
        bind_surface_owner(&surface, &focus_owner)?;
        let native = surface.handle();
        let thread = NativeThread::of(native)?;
        let at_created = gdi_objects();
        let result = (|| -> Result<()> {
            wait_initial_pixels(native, region)?;
            ensure(unsafe { GetDpiForWindow(native) } > 0, "首屏窗口 DPI 无效")?;
            let metrics = surface.metrics().ok_or("首屏缺少 ready 时序")?;
            ensure(
                metrics.ready_ms.is_finite() && metrics.ready_ms >= 0.,
                "首屏 ready 时序无效",
            )?;
            if case == "reverse" {
                record(
                    "native_surface_exact_pixels",
                    serde_json::json!({"width":WIDTH,"height":HEIGHT,"ready_ms":metrics.ready_ms,"dpi":unsafe{GetDpiForWindow(native)}}),
                    true,
                )?;
            }
            begin_drag(&surface)?;
            if case == "reverse" {
                release_drag(&surface, END)?;
                wait_selection_pixels(native, region)?;
                record(
                    "native_surface_reverse_drag",
                    serde_json::json!({"x":END.0,"y":END.1,"width":START.0-END.0,"height":START.1-END.1,"capture_released":true,"inside_exact":true,"outside_dark":true}),
                    true,
                )?;
            } else {
                if case == "escape" {
                    post(native, WM_KEYDOWN, 0x1b, (0, 0))?;
                } else {
                    post(native, WM_RBUTTONDOWN, 2, END)?;
                    post(native, WM_RBUTTONUP, 0, END)?;
                }
                wait_native(
                    || {
                        let state = surface.snapshot();
                        Ok(!state.cancelled
                            && !state.dragging
                            && state.crop.is_none()
                            && capture_owner(native)? != native)
                    },
                    "首次取消未回退进行中的手势并释放 capture",
                )?;
                record(
                    &format!("native_surface_{case}_cancel_gesture"),
                    serde_json::json!({"window_retained":true,"crop_cleared":true,"capture_released":true}),
                    true,
                )?;
                if case == "escape" {
                    post(native, WM_KEYDOWN, 0x1b, (0, 0))?;
                } else {
                    post(native, WM_RBUTTONDOWN, 2, END)?;
                }
                wait_native(|| Ok(surface.snapshot().cancelled), "首屏空选区取消未处理")?;
            }
            Ok(())
        })();
        let cleanup = surface.dismiss();
        result?;
        cleanup?;
        ensure(
            !unsafe { IsWindow(Some(native)) }.as_bool() && thread.exited(),
            "首屏 dismiss 返回后仍存在窗口或工作线程",
        )?;
        let at_exit = gdi_objects();
        // 真实窗口已经销毁、线程已经 signaled；再给 GDI 延迟删除一次固定观察期。
        std::thread::sleep(Duration::from_millis(200));
        let after = gdi_objects();
        record(
            &format!("native_surface_{case}_gdi_cleanup"),
            serde_json::json!({"gdi_before":before,"gdi_native_created":at_created,
                "gdi_thread_exited":at_exit,"gdi_after_200ms":after,
                "delta":after as i64-before as i64,"native_closed":true,"thread_exited":true}),
            after <= before,
        )?;
        ensure(after <= before, &format!("首屏销毁后 GDI 对象未回收：case={case} before={before} native_created={at_created} thread_exited={at_exit} after_200ms={after} delta={}",after as i64-before as i64))?;
        record(
            &format!("native_surface_{case}_recycled"),
            serde_json::json!({"gdi_before":before,"gdi_after":after,"native_closed":true,"thread_exited":true}),
            true,
        )?;
    }
    _foreground.restore_if_own();
    focus_owner.close()?;
    record(
        "native_surface_focus_owner_recycled",
        serde_json::json!({"native_closed":
        !unsafe{IsWindow(Some(HWND(owner_native as *mut _)))}.as_bool(),"thread_exited":true,
        "gdi_after_owner_close":gdi_objects(),"measurement_only":true}),
        true,
    )?;
    drop(lock);
    record(
        "native_surface_mutex_recycled",
        serde_json::json!({"released":mutex_released()}),
        mutex_released(),
    )?;
    ensure(mutex_released(), "原生首屏验收结束后互斥体仍存在")?;
    complete_report("native")
}
/// 在 Application::new 尚在并行初始化期间驱动真实原生首屏。
pub(crate) fn prepare(surface: &StartupSurface) -> Result<()> {
    let case = std::env::var("RUSTSHOT_SURFACE_QA_CASE").unwrap_or_else(|_| "complete".into());
    ensure(
        matches!(
            case.as_str(),
            "complete" | "active" | "early_cancel" | "queued_keys" | "modifiers"
        ),
        "未知快速首屏专项 case",
    )?;
    let region = native_bounds(surface.handle())?;
    ensure(
        region.width == WIDTH && region.height == HEIGHT,
        "首屏专项没有使用自有冻结图",
    )?;
    wait_initial_pixels(surface.handle(), region)?;
    if case == "early_cancel" {
        let native = surface.handle();
        let thread = NativeThread::of(native)?;
        ensure(
            surface.snapshot().crop.is_none() && !surface.snapshot().dragging,
            "早期取消需要干净空选区",
        )?;
        let saved = EarlyRecycle {
            surface_native: native.0 as usize,
            thread: thread.0 .0 as usize,
        };
        std::mem::forget(thread);
        *EARLY.lock().map_err(|_| "早期取消诊断锁无效")? = Some(saved);
        post(native, WM_KEYDOWN, 0x1b, (0, 0))?;
        wait_native(
            || Ok(surface.snapshot().cancelled && !unsafe { IsWindow(Some(native)) }.as_bool()),
            "GPU 初始化期间原生 Escape 没有关闭首屏",
        )?;
        return record(
            "surface_early_cancel_before_gpui",
            serde_json::json!({"native_closed":true,
            "cancelled":true,"crop_none":true}),
            true,
        );
    }
    begin_drag(surface)?;
    if case != "active" {
        release_drag(surface, END)?;
    }
    if case == "queued_keys" {
        post(surface.handle(), WM_KEYDOWN, 0x32, (0, 0))?;
        post(surface.handle(), WM_KEYUP, 0x32, (0, 0))?;
        wait_native(
            || {
                let state = surface.snapshot();
                Ok(state
                    .pending_keys
                    .iter()
                    .any(|&(m, k, _)| m == WM_KEYDOWN && k == 0x32)
                    && state
                        .pending_keys
                        .iter()
                        .any(|&(m, k, _)| m == WM_KEYUP && k == 0x32))
            },
            "原生首屏没有按序暂存矩形快捷键",
        )?;
    }
    if case == "modifiers" {
        prepare_modifier_baseline()?;
        post(
            surface.handle(),
            WM_KEYDOWN,
            0x53 | QA_CAPTURED_CONTROL,
            (0, 0),
        )?;
        post(
            surface.handle(),
            WM_KEYUP,
            0x53 | QA_CAPTURED_CONTROL,
            (0, 0),
        )?;
        // 恢复的只是暂存队列里的修饰快照，不修改系统或任何线程的 keyboard state。
        post(surface.handle(), WM_KEYUP, 0x11, (0, 0))?;
        wait_native(
            || {
                Ok(surface.snapshot().pending_keystrokes.iter().any(|key| {
                    key.key == "s"
                        && key.modifiers.control
                        && !key.modifiers.shift
                        && !key.modifiers.alt
                }))
            },
            "原生键位快照丢失 Ctrl 修饰符",
        )?;
        record(
            "surface_modifier_snapshot",
            serde_json::json!({"key":"s","control":true,
            "injection":"owned_message_snapshot_flag","system_keyboard_unchanged":true}),
            true,
        )?;
    }
    record(
        "surface_pre_gpui_gesture",
        serde_json::json!({"case":case,"surface_native":surface.handle().0 as usize,
        "surface_token":unsafe{GetPropW(surface.handle(),w!("RustShot.StartupSurface.Token"))}.0 as usize,
        "dragging":surface.snapshot().dragging,"crop_matches":surface.snapshot().crop==Some(expected_crop(END)),"native_visible":unsafe{IsWindowVisible(surface.handle())}.as_bool()}),
        true,
    )
}
fn mutex_released() -> bool {
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};
    match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
            false,
            w!("Local\\RustShot.MVP.Capture"),
        )
    } {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            false
        }
        Err(error) => error.code() == windows::core::HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0),
    }
}
fn fail(
    native: HWND,
    surface: &StartupSurface,
    errors: &RefCell<Option<String>>,
    error: impl ToString,
) {
    let message = error.to_string();
    let _ = record(
        "surface_failure",
        serde_json::json!({"error":message}),
        false,
    );
    *errors.borrow_mut() = Some(message);
    let _ = surface.dismiss();
    let _ = post(native, WM_CLOSE, 0, (0, 0));
}
struct Recycle {
    native: usize,
    surface_native: usize,
    surface_token: usize,
    thread: Rc<NativeThread>,
    case: String,
    handoff_verified: bool,
    cancel_requested: bool,
}
thread_local! {static RECYCLE:RefCell<Option<Recycle>>=const{RefCell::new(None)};}
struct EarlyRecycle {
    surface_native: usize,
    thread: usize,
}
impl Drop for EarlyRecycle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(HANDLE(self.thread as *mut _));
        }
    }
}
static EARLY: Mutex<Option<EarlyRecycle>> = Mutex::new(None);
struct ModifierBaseline {
    data: PathBuf,
    output: PathBuf,
    files: BTreeMap<PathBuf, Vec<u8>>,
}
static MODIFIER_BASELINE: Mutex<Option<ModifierBaseline>> = Mutex::new(None);
fn isolated_files(root: &std::path::Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn collect(
        root: &std::path::Path,
        path: &std::path::Path,
        result: &mut BTreeMap<PathBuf, Vec<u8>>,
    ) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure(!kind.is_symlink(), "隔离配置中出现外部路径链接")?;
            if kind.is_dir() {
                collect(root, &entry.path(), result)?;
            } else if kind.is_file() {
                ensure(
                    entry.metadata()?.len() <= 1024 * 1024,
                    "隔离配置出现过大文件",
                )?;
                result.insert(
                    entry.path().strip_prefix(root)?.to_path_buf(),
                    std::fs::read(entry.path())?,
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    collect(root, root, &mut files)?;
    Ok(files)
}
fn prepare_modifier_baseline() -> Result<()> {
    let explicit =
        std::env::var_os("RUSTSHOT_DATA_DIR").ok_or("修饰键专项需要显式隔离 RUSTSHOT_DATA_DIR")?;
    let data = PathBuf::from(explicit);
    ensure(
        data.starts_with(report_root()),
        "修饰键专项配置必须位于自有报告目录内",
    )?;
    std::fs::create_dir_all(&data)?;
    let output = report_root()
        .canonicalize()?
        .join("cancel-must-not-create-output");
    ensure(!output.exists(), "修饰键取消验收输出目录必须不存在")?;
    let mut store = SaveStore::load();
    store.config.directory = output.clone();
    store.save()?;
    let files = isolated_files(&data)?;
    *MODIFIER_BASELINE.lock().map_err(|_| "修饰键诊断锁无效")? = Some(ModifierBaseline {
        data,
        output,
        files,
    });
    Ok(())
}
fn full_present(window: &Window) -> bool {
    let native = hwnd(window);
    let serial = unsafe { GetPropW(native, w!("RustShot.GpuiPresentSerial")) }.0 as usize;
    let dimensions = unsafe { GetPropW(native, w!("RustShot.GpuiPresentSize")) }.0 as usize as u32;
    serial != 0 && dimensions == (WIDTH << 16) | HEIGHT
}
async fn modifier_panel_case(
    view: &WeakEntity<CaptureView>,
    owner: AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    let panel = loop {
        let panel = cx
            .update(|cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|w| w.downcast::<SavePanel>())
            })
            .map_err(|e| e.to_string())?;
        if let Some(panel) = panel {
            break panel;
        }
        ensure(
            Instant::now() < deadline,
            "暂存 Ctrl+S 未打开保存面板；可能被退化为裸 S",
        )?;
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    };
    let native = panel
        .update(cx, |panel, window, _| -> Result<usize> {
            ensure(!panel.saving, "重放 Ctrl+S 意外开始写盘")?;
            Ok(hwnd(window).0 as usize)
        })
        .map_err(|e| e.to_string())??;
    let original = view
        .update(cx, |v, _| -> Result<()> {
            ensure(
                v.saving
                    && v.editor.tool == Tool::Crop
                    && v.editor.crop == Some(expected_crop(END)),
                "Ctrl+S 重放没有进入保存域，或裸 S 误选聚光灯工具",
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    original?;
    let deadline = Instant::now() + WAIT;
    loop {
        let drawn = panel
            .update(cx, |p, _, _| {
                p.ui_positions.borrow().contains_key("save-cancel")
            })
            .map_err(|e| e.to_string())?;
        if drawn {
            break;
        }
        ensure(Instant::now() < deadline, "保存面板没有实际绘制取消控件")?;
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    }
    gpui_smoke::isolate_input(HWND(native as *mut _))?;
    post(HWND(native as *mut _), WM_KEYDOWN, 0x1b, (0, 0))?;
    post(HWND(native as *mut _), WM_KEYUP, 0x1b, (0, 0))?;
    let deadline = Instant::now() + WAIT;
    loop {
        let restored = cx
            .update_window(owner, |_, window, cx| -> Result<bool> {
                let capture_ready = view.update(cx, |v, _| {
                    !v.saving
                        && v.editor.tool == Tool::Crop
                        && v.editor.crop == Some(expected_crop(END))
                })?;
                Ok(capture_ready
                    && !unsafe { IsWindow(Some(HWND(native as *mut _))) }.as_bool()
                    && unsafe { GetForegroundWindow() } == hwnd(window)
                    && unsafe { GetFocus() } == hwnd(window))
            })
            .map_err(|e| e.to_string())??;
        if restored {
            break;
        }
        ensure(
            Instant::now() < deadline,
            "保存面板 Escape 没有取消并恢复原截图焦点",
        )?;
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    }
    let baseline = MODIFIER_BASELINE
        .lock()
        .map_err(|_| "修饰键诊断锁无效")?
        .take()
        .ok_or("修饰键专项缺少隔离配置 baseline")?;
    ensure(
        isolated_files(&baseline.data)? == baseline.files && !baseline.output.exists(),
        "仅取消的修饰键保存面板改写了配置、历史或输出",
    )?;
    record(
        "surface_modifier_save_cancel",
        serde_json::json!({"shortcut":"ctrl-s","save_panel_seen":true,
        "tool":"crop","crop_retained":true,"cancel_method":"owned_escape","disk_unchanged":true,
        "foreground_restored":true}),
        true,
    )
}
unsafe extern "system" fn count_gpui(h: HWND, context: LPARAM) -> BOOL {
    let mut pid = 0;
    GetWindowThreadProcessId(h, Some(&mut pid));
    if pid == std::process::id() {
        let mut title = [0_u16; 80];
        let length = GetWindowTextW(h, &mut title);
        if String::from_utf16_lossy(&title[..length.max(0) as usize]) == concat!(crate::brand_name!(), " · GPUI") {
            *(context.0 as *mut usize) += 1;
        }
    }
    BOOL(1)
}
/// App 正常退出之后验收；不依赖最后一扇窗口关闭后继续调度异步任务。
pub(crate) fn recycled() -> Result<()> {
    if let Some(state) = EARLY.lock().map_err(|_| "早期取消诊断锁无效")?.take() {
        let mut count = 0_usize;
        unsafe {
            EnumWindows(
                Some(count_gpui),
                LPARAM((&mut count as *mut usize) as isize),
            )?;
        }
        ensure(
            count == 0
                && !unsafe { IsWindow(Some(HWND(state.surface_native as *mut _))) }.as_bool(),
            "早期取消后又创建了 GPUI 截图窗口或首屏仍存在",
        )?;
        ensure(
            unsafe { WaitForSingleObject(HANDLE(state.thread as *mut _), 0) } == WAIT_OBJECT_0,
            "早期取消后首屏线程仍存活",
        )?;
        ensure(mutex_released(), "早期取消后 CaptureMutex 未释放")?;
        record(
            "surface_early_cancel_recycled",
            serde_json::json!({"gpui_windows":count,"native_closed":true,
            "surface_thread_exited":true,"capture_mutex_released":true}),
            true,
        )?;
        return complete_report("early_cancel");
    }
    let state = RECYCLE
        .with(|pending| pending.borrow_mut().take())
        .ok_or("首屏专项未进入 GPUI 会话")?;
    ensure(
        state.handoff_verified && state.cancel_requested,
        "首屏专项未完成真实交接/原生 Escape",
    )?;
    ensure(
        !unsafe { IsWindow(Some(HWND(state.native as *mut _))) }.as_bool()
            && !unsafe { IsWindow(Some(HWND(state.surface_native as *mut _))) }.as_bool(),
        "首屏专项退出后仍存在自有窗口",
    )?;
    ensure(state.thread.exited(), "首屏专项退出后原生绘制线程仍存活")?;
    ensure(mutex_released(), "首屏专项退出后仍持有 CaptureMutex")?;
    record(
        "surface_gpui_input_cancel_recycled",
        serde_json::json!({"case":state.case,"surface_native":state.surface_native,
        "surface_token":state.surface_token,"native_closed":true,
        "gpui_closed":true,"capture_mutex_released":true,"surface_thread_exited":true}),
        true,
    )?;
    complete_report(&state.case)
}
/// 不调用 handoff；只观察生产交接，再向原生窗口投递仍未结束的手势抬起。
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    surface: StartupSurface,
    errors: Rc<RefCell<Option<String>>>,
) {
    let weak = view.downgrade();
    let surface_native = surface.handle().0 as usize;
    let surface_token =
        unsafe { GetPropW(surface.handle(), w!("RustShot.StartupSurface.Token")) }.0 as usize;
    let thread = match NativeThread::of(surface.handle()) {
        Ok(thread) => Rc::new(thread),
        Err(error) => {
            fail(hwnd(window), &surface, &errors, error);
            return;
        }
    };
    RECYCLE.with(|pending| {
        *pending.borrow_mut() = Some(Recycle {
            native: hwnd(window).0 as usize,
            surface_native,
            surface_token,
            thread: thread.clone(),
            case: std::env::var("RUSTSHOT_SURFACE_QA_CASE").unwrap_or_else(|_| "complete".into()),
            handoff_verified: false,
            cancel_requested: false,
        })
    });
    window.on_next_frame(move|window,_|{
        if let Err(error)=gpui_smoke::isolate_input(hwnd(window)){fail(hwnd(window),&surface,&errors,error);return;}
        window.on_next_frame(move|window,cx|{
            let native=hwnd(window).0 as usize;
            let own=window.window_handle();
            let case=std::env::var("RUSTSHOT_SURFACE_QA_CASE").unwrap_or_else(|_|"complete".into());
            let active=case=="active";
            let first=(||->Result<()>{
                ensure(unsafe{DwmFlush()}>=0,"GPUI 首帧 DwmFlush 失败")?;
                Ok(())
            })();
            if let Err(error)=first{fail(HWND(native as *mut _),&surface,&errors,error);return;}
            cx.spawn(async move|cx|{
                let result:Result<()>=async{
                    let expected=expected_crop(if active{ACTIVE_END}else{END});
                    if active {
                        let deadline=Instant::now()+WAIT;
                        loop {
                            let present=cx.update_window(own,|_,window,_|full_present(window)).map_err(|e|e.to_string())?;
                            if present{break;}
                            ensure(Instant::now()<deadline,"GPUI 尚未真正呈现完整首帧")?;
                            cx.background_executor().timer(Duration::from_millis(16)).await;
                        }
                        ensure(unsafe{DwmFlush()}>=0,"active GPUI present DwmFlush 失败")?;
                        let h=HWND(surface_native as *mut _);let state=surface.snapshot();
                        ensure(unsafe{IsWindow(Some(h))}.as_bool()&&state.dragging&&capture_owner(h)?==h,
                            "GPUI 首帧出现时提前消耗了原生进行中的手势")?;
                        ensure(unsafe{GetForegroundWindow()}==h,"GPUI 在原生拖动结束前抢走前台")?;
                        record("surface_active_survives_gpui_present",serde_json::json!({"dragging":true,
                            "native_capture":true,"native_foreground":true,"full_size_present":true}),true)?;
                        post(h,WM_MOUSEMOVE,SMOKE_MOVE|1,ACTIVE_END)?;
                        post(h,WM_LBUTTONUP,0,ACTIVE_END)?;
                    }
                    let deadline=Instant::now()+WAIT;
                    loop {
                        let ready=cx.update_window(own,|_,window,cx|->Result<bool>{
                            let target=hwnd(window);
                            let state=surface.snapshot();
                            let good=weak.update(cx,|v,_| v.editor.crop==Some(expected)&&!v.editor.is_dragging()
                                &&(case!="queued_keys"||v.editor.tool==Tool::Rectangle)
                                &&(case!="modifiers"||(v.saving&&v.editor.tool==Tool::Crop)))?;
                            Ok(good&&!state.dragging&&!unsafe{IsWindow(Some(HWND(surface_native as *mut _)))}.as_bool()
                                &&thread.exited()&&(case=="modifiers"||
                                    (unsafe{GetForegroundWindow()}==target&&unsafe{GetFocus()}==target)))
                        }).map_err(|e|e.to_string())??;
                        if ready{break;}
                        ensure(Instant::now()<deadline,"原生首屏选区未完整导入 GPUI/焦点或线程尚未回收")?;
                        cx.background_executor().timer(Duration::from_millis(16)).await;
                    }
                    if case=="modifiers"{modifier_panel_case(&weak,own,cx).await?;}
                    // 窗口激活必须尊重最终物理全屏，而不能恢复 show:false 初建的小尺寸。
                    cx.update_window(own,|_,window,_|window.activate_window()).map_err(|e|e.to_string())?;
                    let deadline=Instant::now()+WAIT;
                    loop{
                        let correct=cx.update_window(own,|_,window,cx|->Result<bool>{
                            let bounds=weak.update(cx,|v,_|v.source.bounds)?;
                            let scale=window.scale_factor();let viewport=window.viewport_size();
                            Ok(own_window(hwnd(window),bounds).is_ok()
                                &&(f32::from(viewport.width)*scale-WIDTH as f32).abs()<=1.
                                &&(f32::from(viewport.height)*scale-HEIGHT as f32).abs()<=1.
                                &&(scale-unsafe{GetDpiForWindow(hwnd(window))} as f32/96.).abs()<0.001)
                        }).map_err(|e|e.to_string())??;
                        if correct{break;}
                        ensure(Instant::now()<deadline,"激活窗口后恢复了 bootstrap 尺寸或 DPI/viewport 缓存错误")?;
                        cx.background_executor().timer(Duration::from_millis(16)).await;
                    }
                    cx.update_window(own,|_,window,cx|->Result<()>{
                        let target=hwnd(window);let scale=window.scale_factor();let viewport=window.viewport_size();
                        let bounds=weak.update(cx,|v,_|->Result<ScreenRect>{
                            ensure(!v.source.is_image()&&v.source.bounds.width==WIDTH&&v.source.bounds.height==HEIGHT,
                                "首屏专项意外切换到文件编辑或真实桌面")?;
                            ensure(v.editor.document.items().is_empty(),"首屏交接污染了标注文档")?;
                            let image=v.source.read_region(expected)?;
                            ensure(image.width==expected.width&&image.height==expected.height,
                                "交接选区原始像素尺寸不正确")?;
                            ensure((0..image.height).all(|y|(0..image.width).all(|x|
                                image_pixel(&image,x,y)==expected_pixel(x+expected.x,y+expected.y))),
                                "交接后的原始选区像素不是冻结自有图")?;
                            Ok(v.source.bounds)
                        })??;
                        own_window(target,bounds)?;
                        ensure((f32::from(viewport.width)*scale-WIDTH as f32).abs()<=1.
                            &&(f32::from(viewport.height)*scale-HEIGHT as f32).abs()<=1.
                            &&(scale-unsafe{GetDpiForWindow(target)} as f32/96.).abs()<0.001,
                            "交接后 GPUI viewport/DPI 与原生客户区不一致")?;
                        ensure(capture_owner(target)?!=target,"交接后 GPUI 残留原生 mouse capture")?;
                        record("surface_gpui_handoff",serde_json::json!({"case":case,"width":WIDTH,"height":HEIGHT,
                            "surface_native":surface_native,"surface_token":surface_token,
                            "crop_x":expected.x,"crop_y":expected.y,"crop_width":expected.width,"crop_height":expected.height,
                            "scale":scale,"raw_pixels_exact":true,"native_thread_exited":true,"foreground_focus":true,
                            "activated_again":true,"post_activation_physical_bounds":true}),true)?;
                        if case=="queued_keys" {
                            record("surface_queued_keys_replayed",serde_json::json!({"shortcut":"2","tool":"rectangle",
                                "crop_retained":true,"queued_before_handoff":true}),true)?;
                        }
                        Ok(())
                    }).map_err(|e|e.to_string())??;
                    RECYCLE.with(|pending|{
                        if let Some(state)=pending.borrow_mut().as_mut(){state.handoff_verified=true;state.cancel_requested=true;}
                    });
                    post(HWND(native as *mut _),WM_KEYDOWN,0x1b,(0,0))?;
                    post(HWND(native as *mut _),WM_KEYUP,0x1b,(0,0))?;
                    // 最后一个 Window 关闭会退出 App；真正回收由外层 recycled 检查。
                    cx.background_executor().timer(Duration::from_millis(800)).await;
                    Err("首屏专项原生 Escape 未结束 GPUI 会话".into())
                }.await;
                if let Err(error)=result{fail(HWND(native as *mut _),&surface,&errors,error);}
            }).detach();
        });
        window.refresh();
    });
    window.refresh();
}
