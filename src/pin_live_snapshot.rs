//! 用户明确请求转静态/关闭时才采一帧，只读回锁定的客户区 ROI。
use crate::{
    capture::Image,
    geometry::ScreenRect,
    pin_live_dwm::probe,
    pin_live_model::{source_region, SourceIdentity, SourceState},
    Result,
};
use std::time::{Duration, Instant};
use windows::{
    core::Interface,
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::IDXGIDevice,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
        },
    },
};

pub(crate) fn copy(image: &Image) -> Image {
    Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: image.readback_ms,
    }
}

pub(crate) fn capture(
    identity: SourceIdentity,
    region: ScreenRect,
    fallback: &Image,
) -> (Image, bool) {
    match capture_wgc(identity, region) {
        Ok(image) => (image, true),
        Err(error) => {
            eprintln!("窗口贴图单帧不可用，保留冻结画面：{error}");
            (copy(fallback), false)
        }
    }
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
struct CaptureObjects {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}
impl Drop for CaptureObjects {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

fn capture_wgc(identity: SourceIdentity, region: ScreenRect) -> Result<Image> {
    if probe(identity).state != SourceState::Live {
        return Err("源窗口当前不可采集".into());
    }
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
    }
    let _apartment = Apartment;
    if !GraphicsCaptureSession::IsSupported()? {
        return Err("系统不支持窗口采集".into());
    }
    let started = Instant::now();
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe {
        interop.CreateForWindow(windows::Win32::Foundation::HWND(identity.hwnd as *mut _))?
    };
    let size = item.Size()?;
    if size.Width <= 0 || size.Height <= 0 || size.Width > 16384 || size.Height > 16384 {
        return Err("窗口采集尺寸超出预算".into());
    }
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or("窗口采集未返回设备")?;
    let context = context.ok_or("窗口采集未返回上下文")?;
    let dxgi: IDXGIDevice = device.cast()?;
    let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    // 普通 SDR 窗口保留系统提供的 8 位输出；窗口源纹理不能按各屏桌面白点分段换算。
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        1,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let objects = CaptureObjects { pool, session };
    let _ = objects.session.SetIsCursorCaptureEnabled(false);
    // Win10/未获 borderless 权限时保留系统边框，失败不影响单帧采集。
    let _ = objects.session.SetIsBorderRequired(false);
    objects.session.StartCapture()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    let frame = loop {
        if probe(identity).state != SourceState::Live {
            return Err("源窗口已离开可采集状态".into());
        }
        if let Ok(frame) = objects.pool.TryGetNextFrame() {
            break frame;
        }
        if Instant::now() >= deadline {
            return Err("窗口采集首帧超时".into());
        }
        std::thread::sleep(Duration::from_millis(8));
    };
    let frame_result = (|| -> Result<Image> {
        let surface: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { surface.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        let actual = frame.ContentSize()?;
        let source = probe(identity);
        let roi = source_region(
            region,
            source.client,
            source.window,
            source.extended,
            (actual.Width as u32, actual.Height as u32),
        )
        .ok_or("窗口尺寸变化或采集边框口径不一致")?;
        if roi.left < 0
            || roi.top < 0
            || u64::from(roi.width) * u64::from(roi.height) > 32_000_000
            || i64::from(roi.left) + i64::from(roi.width) > i64::from(desc.Width)
            || i64::from(roi.top) + i64::from(roi.height) > i64::from(desc.Height)
        {
            return Err("窗口采集 ROI 无效".into());
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: roi.width,
            Height: roi.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut staging = None;
        unsafe {
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        }
        let staging = staging.ok_or("窗口采集未返回读回纹理")?;
        let area = D3D11_BOX {
            left: roi.left as u32,
            top: roi.top as u32,
            front: 0,
            right: roi.left as u32 + roi.width,
            bottom: roi.top as u32 + roi.height,
            back: 1,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, 0, Some(&area));
            context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let row_bytes = roi.width as usize * 4;
        let mut bgra = vec![0; row_bytes * roi.height as usize];
        if mapped.pData.is_null() || (mapped.RowPitch as usize) < row_bytes {
            unsafe {
                context.Unmap(&staging, 0);
            }
            return Err("窗口采集读回行距无效".into());
        }
        for (y, row) in bgra.chunks_exact_mut(row_bytes).enumerate() {
            unsafe {
                row.copy_from_slice(std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                    row_bytes,
                ));
            }
        }
        unsafe {
            context.Unmap(&staging, 0);
        }
        Ok(Image {
            width: roi.width,
            height: roi.height,
            bgra,
            readback_ms: started.elapsed().as_secs_f64() * 1000.,
        })
    })();
    let _ = frame.Close();
    frame_result
}

#[cfg(test)]
#[path = "pin_live_snapshot_gpu_tests.rs"]
mod gpu_tests;

#[cfg(test)]
mod tests {
    use super::{gpu_tests as gpu, *};
    use windows::{
        core::{w, PCWSTR},
        Win32::{
            Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
            Graphics::{
                Dxgi::{Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, *},
                Gdi::*,
            },
            System::LibraryLoader::GetModuleHandleW,
            UI::{HiDpi::*, WindowsAndMessaging::*},
        },
    };

    // 深色测试窗：64 级 0–189 的灰、红、绿、蓝四条色带，不出现亮底。
    const STEP: i32 = 8;
    const BAND: i32 = 32;
    const WIDTH: i32 = STEP * 64;
    const HEIGHT: i32 = BAND * 4;

    struct TestWindow(HWND);
    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmFlush() -> i32;
    }

    /// 测试窗 (x, y) 处应有的 BGR。
    fn expected(x: i32, y: i32) -> [u8; 3] {
        let v = (x / STEP * 3) as u8;
        match y / BAND {
            0 => [v, v, v],
            1 => [0, 0, v],
            2 => [0, v, 0],
            _ => [v, 0, 0],
        }
    }

    unsafe extern "system" fn procedure(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match message {
            WM_ERASEBKGND => LRESULT(1),
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(h, &mut paint);
                for y in (0..HEIGHT).step_by(BAND as usize) {
                    for x in (0..WIDTH).step_by(STEP as usize) {
                        let [b, g, r] = expected(x, y);
                        let brush = CreateSolidBrush(COLORREF(
                            r as u32 | (g as u32) << 8 | (b as u32) << 16,
                        ));
                        let rect = RECT {
                            left: x,
                            top: y,
                            right: x + STEP,
                            bottom: y + BAND,
                        };
                        let _ = FillRect(dc, &rect, brush);
                        let _ = DeleteObject(brush.into());
                    }
                }
                let _ = EndPaint(h, &paint);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, message, wp, lp),
        }
    }

    fn settle() {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let _ = GdiFlush();
            DwmFlush();
        }
    }

    fn gdi_screen(area: ScreenRect) -> Vec<u8> {
        unsafe {
            let screen = GetDC(None);
            let memory = CreateCompatibleDC(Some(screen));
            let info = crate::gdi_surface::bitmap_info(area.width, area.height);
            let mut bits = std::ptr::null_mut();
            let bitmap =
                CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old = SelectObject(memory, bitmap.into());
            BitBlt(
                memory,
                0,
                0,
                area.width as i32,
                area.height as i32,
                Some(screen),
                area.left,
                area.top,
                SRCCOPY,
            )
            .unwrap();
            let len = (area.width * area.height * 4) as usize;
            let pixels = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
            SelectObject(memory, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            pixels
        }
    }

    fn outputs() -> Vec<(RECT, bool)> {
        let mut found = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                let mut oi = 0;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    let output6: IDXGIOutput6 = output.cast().unwrap();
                    let desc = output6.GetDesc1().unwrap();
                    if desc.AttachedToDesktop.as_bool()
                        && !found.iter().any(|(r, _)| *r == desc.DesktopCoordinates)
                    {
                        let hdr = desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                        found.push((desc.DesktopCoordinates, hdr));
                    }
                    oi += 1;
                }
                ai += 1;
            }
        }
        found
    }

    /// 与已知像素逐通道 ±2 一致的像素数。
    fn matching(bgra: &[u8], expected: &[u8]) -> usize {
        bgra.chunks_exact(4)
            .zip(expected.chunks_exact(4))
            .filter(|(p, e)| (0..3).all(|c| p[c].abs_diff(e[c]) <= 2))
            .count()
    }

    /// 窗口依次移屏、跨相邻屏边界（两边分别占多数），最后回到起点。
    fn placements(monitors: &[(RECT, bool)]) -> Vec<(i32, i32, String)> {
        let mut positions = Vec::new();
        for (monitor, hdr) in monitors {
            if monitor.right - monitor.left >= WIDTH && monitor.bottom - monitor.top >= HEIGHT {
                positions.push((
                    monitor.left + (monitor.right - monitor.left - WIDTH) / 2,
                    monitor.bottom - HEIGHT - (monitor.bottom - monitor.top - HEIGHT).min(48),
                    format!("{},{} HDR={hdr}", monitor.left, monitor.top),
                ));
            }
        }
        let single_count = positions.len();
        for (index, (a, ah)) in monitors.iter().enumerate() {
            for (b, bh) in &monitors[index + 1..] {
                let top = a.top.max(b.top);
                let bottom = a.bottom.min(b.bottom);
                let left = a.left.max(b.left);
                let right = a.right.min(b.right);
                for part in 1..=3 {
                    if (a.right == b.left || b.right == a.left) && bottom - top >= HEIGHT {
                        let (edge, lh, rh) = if a.right == b.left {
                            (a.right, ah, bh)
                        } else {
                            (b.right, bh, ah)
                        };
                        positions.push((
                            edge - WIDTH * part / 4,
                            top + (bottom - top - HEIGHT) / 2,
                            format!("左右跨屏 HDR={lh}/{rh}，左侧占 {part}/4"),
                        ));
                    }
                    if (a.bottom == b.top || b.bottom == a.top) && right - left >= WIDTH {
                        let (edge, th, bh) = if a.bottom == b.top {
                            (a.bottom, ah, bh)
                        } else {
                            (b.bottom, bh, ah)
                        };
                        positions.push((
                            left + (right - left - WIDTH) / 2,
                            edge - HEIGHT * part / 4,
                            format!("上下跨屏 HDR={th}/{bh}，上侧占 {part}/4"),
                        ));
                    }
                }
            }
        }
        println!(
            "单屏位置 {single_count}，跨屏位置 {}",
            positions.len() - single_count
        );
        if positions.len() == single_count {
            println!("没有能容纳测试窗的共享屏幕边界，跳过跨屏检查");
        }
        if let Some((x, y, name)) = positions.first().cloned() {
            positions.push((x, y, format!("移回 {name}")));
        }
        positions
    }

    /// 实机：同一不抢焦点的深色自有窗口移屏、跨屏，分别由 GDI 和 GPU 绘制。
    /// WGC 单帧应与已知像素一致，并记录桌面 GDI 对照
    /// （GDI 在 HDR 屏是正确参考；过曝时灰带中间调会抬高几十级）。
    #[test]
    #[ignore = "在每块屏短暂显示自有深色窗口"]
    fn wgc_window_snapshot_matches_gdi_on_every_output() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            // 测试进程里没有别的线程持有 MTA：采集末尾的 RoUninitialize 会拆掉 MTA、
            // 卸载仍有后台线程在跑的采集组件，随后进程访问冲突。整段测试持有一份。
            RoInitialize(RO_INIT_MULTITHREADED).unwrap();
        }
        let _apartment = Apartment;
        let class: Vec<u16> = format!("RustShot.WgcFixture.{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let instance = unsafe { GetModuleHandleW(None).unwrap() }.into();
        let atom = unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            })
        };
        assert_ne!(atom, 0);
        let monitors = outputs();
        assert!(!monitors.is_empty(), "没有可采集的显示器");
        if !monitors.iter().any(|(_, hdr)| *hdr) {
            println!("当前没有 HDR 输出，本轮只验证 SDR 行为");
        }
        let positions = placements(&monitors);
        assert!(!positions.is_empty(), "显示器无法容纳测试窗");
        let total = (WIDTH * HEIGHT) as usize;
        let mut failures = Vec::new();
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                PCWSTR(class.as_ptr()),
                w!("RustShot · WGC 对照"),
                WS_POPUP,
                positions[0].0,
                positions[0].1,
                WIDTH,
                HEIGHT,
                None,
                None,
                Some(instance),
                None,
            )
            .unwrap()
        };
        let fixture = TestWindow(window);
        unsafe {
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        }
        for use_gpu in [false, true] {
            let backend = if use_gpu { "GPU SDR" } else { "GDI" };
            let gpu = if use_gpu {
                match gpu::SdrSurface::new(window, WIDTH as u32, HEIGHT as u32) {
                    Ok(surface) => Some(surface),
                    Err(error) => {
                        failures.push(format!("GPU 测试窗初始化失败：{error}"));
                        continue;
                    }
                }
            } else {
                None
            };
            for (index, (x, y, position)) in positions.iter().enumerate() {
                let name = format!("{backend} {position}");
                // GPU 图案与 WM_PAINT 不同，并在每次移动后改变，避免旧帧/GDI 回退假通过。
                let shift = if use_gpu {
                    16 + (index % 2) as i32 * 16
                } else {
                    0
                };
                let mut pixels = Vec::with_capacity(total * 4);
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        pixels.extend_from_slice(&expected((x + shift * STEP) % WIDTH, y));
                        pixels.push(255);
                    }
                }
                let result = (|| -> Result<_> {
                    unsafe {
                        SetWindowPos(
                            window,
                            None,
                            *x,
                            *y,
                            WIDTH,
                            HEIGHT,
                            SWP_NOACTIVATE | SWP_NOZORDER,
                        )?;
                        let _ = UpdateWindow(window);
                    }
                    if let Some(gpu) = &gpu {
                        gpu.present(&pixels)?;
                    }
                    settle();
                    std::thread::sleep(Duration::from_millis(50));
                    settle();
                    let mut process_id = 0;
                    let thread_id =
                        unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
                    let identity = SourceIdentity {
                        hwnd: window.0 as usize,
                        process_id,
                        thread_id,
                    };
                    let gdi = gdi_screen(probe(identity).client);
                    let region = ScreenRect {
                        left: 0,
                        top: 0,
                        width: WIDTH as u32,
                        height: HEIGHT as u32,
                    };
                    capture_wgc(identity, region).map(|wgc| (gdi, wgc))
                })();
                let (gdi, wgc) = match result {
                    Ok(pair) => pair,
                    Err(error) => {
                        failures.push(format!("{name} 采集失败：{error}"));
                        continue;
                    }
                };
                assert_eq!(gdi.len(), total * 4);
                assert_eq!((wgc.width, wgc.height), (WIDTH as u32, HEIGHT as u32));
                let (gdi_ok, wgc_ok) = (matching(&gdi, &pixels), matching(&wgc.bgra, &pixels));
                println!(
                    "输出 {name}：GDI {gdi_ok}/{total}，WGC {wgc_ok}/{total} 像素与绘制值一致"
                );
                // 灰带若干级：绘制值 → GDI → WGC。
                for level in [15, 30, 45, 81, 111, 150, 189] {
                    let x = level / 3 * STEP + STEP / 2;
                    let at = |p: &[u8]| p[(BAND / 2 * WIDTH + x) as usize * 4];
                    let drawn = at(&pixels);
                    println!(
                        "    {drawn:>3} → GDI {:>3} WGC {:>3}",
                        at(&gdi),
                        at(&wgc.bgra)
                    );
                }
                if gdi_ok * 100 < total * 99 {
                    println!("    GDI 参考与绘制值不符，可能被其它置顶窗口遮挡");
                }
                if wgc_ok * 100 < total * 99 {
                    failures.push(name);
                }
            }
        }
        drop(fixture);
        unsafe {
            let _ = UnregisterClassW(PCWSTR(class.as_ptr()), Some(instance));
        }
        assert!(failures.is_empty(), "WGC 与绘制值不一致：{failures:?}");
    }
}
