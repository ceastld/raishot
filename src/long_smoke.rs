//! 自己的滚动测试窗口：实际 DWM/DXGI 捕获、采集排除和拼接验证，不操作其它应用。
use crate::{
    capture::{Image, Snapshot},
    export,
    geometry::ScreenRect,
    live_capture::LiveCapture,
    stitch::Stitcher,
    Result,
};
use std::{mem::size_of, sync::mpsc, thread, time::Duration};
use windows::{
    core::w,
    Win32::{
        Foundation::*, Graphics::Gdi::*, System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
};
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
const SCROLL: u32 = WM_APP + 31;
unsafe extern "system" fn window_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_CLOSE {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(h, message, wp, lp)
}
fn frame(offset: u32) -> Image {
    let mut bgra = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in offset..offset + HEIGHT {
        for x in 0..WIDTH {
            let n = ((y * 157 + x * 71) ^ (y * 17 + x * 113)) % 251;
            bgra.extend_from_slice(&[
                n as u8,
                ((y * 13 + x * 19) % 251) as u8,
                ((y * y + x * 41) % 251) as u8,
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
fn paint(h: HWND, image: &Image) -> Result<()> {
    unsafe {
        let dc = GetDC(Some(h));
        if dc.is_invalid() {
            return Err("测试窗口 DC 创建失败".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: image.width as i32,
                biHeight: -(image.height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let n = StretchDIBits(
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
        let flushed = GdiFlush().ok();
        ReleaseDC(Some(h), dc);
        let _ = ValidateRect(Some(h), None);
        flushed?;
        if n <= 0 {
            return Err("测试窗口绘制失败".into());
        }
        Ok(())
    }
}
struct Fixture {
    handle: usize,
    thread: Option<thread::JoinHandle<()>>,
    painted: mpsc::Receiver<std::result::Result<usize, String>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.handle as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Fixture {
    fn new(region: ScreenRect) -> Result<Self> {
        let (send, painted) = mpsc::channel();
        let thread = thread::spawn(move || {
            let result: Result<()> = (|| {
                unsafe {
                    let _ = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                        windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                    );
                    let instance: HINSTANCE = GetModuleHandleW(None)?.into();
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(window_proc),
                        hInstance: instance,
                        lpszClassName: w!("RustShot.LongSmoke.Fixture"),
                        ..Default::default()
                    };
                    RegisterClassW(&class);
                    let h = CreateWindowExW(
                        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                        class.lpszClassName,
                        w!("RustShot · 滚动测试图"),
                        WS_POPUP | WS_VISIBLE,
                        region.left,
                        region.top,
                        WIDTH as i32,
                        HEIGHT as i32,
                        None,
                        None,
                        Some(instance),
                        None,
                    )?;
                    paint(h, &frame(0))?;
                    // 面板覆盖源图中心；采集结果必须呈现被它遮挡的真实源图。
                    let panel = CreateWindowExW(
                        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                        class.lpszClassName,
                        w!("RustShot · 排除测试面板"),
                        WS_POPUP | WS_VISIBLE,
                        region.left + 80,
                        region.top + 40,
                        100,
                        70,
                        None,
                        None,
                        Some(instance),
                        None,
                    )?;
                    SetWindowDisplayAffinity(panel, WDA_EXCLUDEFROMCAPTURE)?;
                    let dc = GetDC(Some(panel));
                    let brush = CreateSolidBrush(COLORREF(0x00ffff));
                    FillRect(
                        dc,
                        &RECT {
                            left: 0,
                            top: 0,
                            right: 100,
                            bottom: 70,
                        },
                        brush,
                    );
                    let _ = DeleteObject(brush.into());
                    ReleaseDC(Some(panel), dc);
                    GdiFlush().ok()?;
                    send.send(Ok(h.0 as usize))?;
                    let mut offset = 0;
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                        match msg.message {
                            SCROLL => {
                                offset = msg.wParam.0 as u32;
                                paint(h, &frame(offset))?;
                                send.send(Ok(h.0 as usize))?;
                            }
                            WM_PAINT if msg.hwnd == h => paint(h, &frame(offset))?,
                            _ => {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                        }
                    }
                    let _ = DestroyWindow(panel);
                    let _ = DestroyWindow(h);
                    let _ = UnregisterClassW(class.lpszClassName, Some(instance));
                    Ok(())
                }
            })();
            if let Err(e) = result {
                let _ = send.send(Err(e.to_string()));
            }
        });
        let handle = painted
            .recv_timeout(Duration::from_secs(5))?
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        Ok(Self {
            handle,
            thread: Some(thread),
            painted,
        })
    }
    fn scroll(&self, offset: u32) -> Result<()> {
        unsafe {
            PostMessageW(
                Some(HWND(self.handle as *mut _)),
                SCROLL,
                WPARAM(offset as usize),
                LPARAM(0),
            )?;
        }
        self.painted
            .recv_timeout(Duration::from_secs(5))?
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        thread::sleep(Duration::from_millis(250));
        Ok(())
    }
}
pub fn run() -> Result<()> {
    let screen = Snapshot::capture()?.desktop;
    let region = ScreenRect {
        left: screen.left + 40,
        top: screen.top + 40,
        width: WIDTH,
        height: HEIGHT,
    };
    let fixture = Fixture::new(region)?;
    thread::sleep(Duration::from_millis(250));
    let live = LiveCapture::new()?;
    let first = live.read(region)?;
    if first.bgra != frame(0).bgra {
        return Err("DXGI 首帧与自绘源图不一致，或排除面板进入了画面".into());
    }
    let mut stitch = Stitcher::new(first);
    fixture.scroll(45)?;
    let second = live.read(region)?;
    if second.bgra != frame(45).bgra {
        return Err("滚动后的实时源图不一致".into());
    }
    if stitch.append(second)? != 45 || stitch.height != 225 {
        return Err("实际滚动图拼接偏移错误".into());
    }
    let path = std::path::Path::new("target/qa/v04-long.png");
    std::fs::create_dir_all(path.parent().unwrap())?;
    export::save_png(stitch.image(), path)?;
    if !stitch.undo() || stitch.image().bgra != frame(0).bgra {
        return Err("实际长截图撤销失败".into());
    }
    println!("{{\"test\":\"dxgi_scroll_stitch_excluded_panel_undo\",\"width\":320,\"height\":225,\"ok\":true}}");
    Ok(())
}
