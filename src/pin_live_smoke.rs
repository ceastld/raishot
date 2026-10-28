//! 显式原生专项；默认测试不创建窗口、不采集用户桌面。
#[cfg(test)]
mod tests {
    use crate::geometry::ScreenRect;
    use crate::pin_live_dwm::Thumbnail;
    use crate::pin_live_model::SourceIdentity;
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    use windows::{
        core::{w, PCWSTR},
        Graphics::Capture::GraphicsCaptureSession,
        Win32::{
            Foundation::*,
            Graphics::{Dwm::DwmFlush, Gdi::*},
            System::{
                LibraryLoader::GetModuleHandleW,
                Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
                Threading::GetCurrentThreadId,
                WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
            },
            UI::{
                HiDpi::{
                    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
                    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                },
                WindowsAndMessaging::*,
            },
        },
    };

    #[test]
    #[ignore = "仅维护者显式运行；创建隐藏自有源窗和宿主，验证 DWM 注册/更新/释放，不证明可见合成像素"]
    fn pin_live_smoke_hidden_owned_windows_register_crop_and_release() {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0.is_null() {
            skip("隐藏专项无法取得物理像素坐标上下文");
            return;
        }
        let _coordinates = PhysicalCoordinates(previous);
        unsafe {
            let make = || {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("RustShot live pin smoke"),
                    WS_POPUP,
                    0,
                    0,
                    320,
                    240,
                    None,
                    None,
                    None,
                    None,
                )
                .map(OwnedWindow)
                .unwrap()
            };
            let source = make();
            let host = make();
            let result = (|| -> crate::Result<()> {
                SetWindowPos(
                    source.0,
                    None,
                    0,
                    0,
                    320,
                    240,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                )?;
                DwmFlush()?;
                let thumbnail = Thumbnail::register(host.0, source.0)?;
                let mut source_bounds = RECT::default();
                GetWindowRect(source.0, &mut source_bounds)?;
                let actual_size = (
                    u32::try_from(source_bounds.right - source_bounds.left)?,
                    u32::try_from(source_bounds.bottom - source_bounds.top)?,
                );
                assert_eq!(actual_size, (320, 240));
                // 本机尚未显示的 STATIC 源返回占位缩略图尺寸；隐藏专项只证明 API 生命周期。
                // 与实际源几何相等的合同放在已显示并完成合成的专项中核验。
                let thumbnail_size = thumbnail.size()?;
                assert!(thumbnail_size.0 >= 110 && thumbnail_size.1 >= 92);
                assert!(thumbnail_size.0 <= 16384 && thumbnail_size.1 <= 16384);
                thumbnail.update(
                    ScreenRect {
                        left: 10,
                        top: 12,
                        width: 100,
                        height: 80,
                    },
                    ScreenRect {
                        left: 0,
                        top: 0,
                        width: 100,
                        height: 80,
                    },
                    160,
                    false,
                )?;
                thumbnail.hide()?;
                drop(thumbnail);
                Ok(())
            })();
            result.unwrap();
        }
    }

    const SOURCE_W: i32 = 64;
    const SOURCE_H: i32 = 48;
    const PATCH_W: i32 = 152;
    const PATCH_H: i32 = 64;

    struct PhysicalCoordinates(DPI_AWARENESS_CONTEXT);
    impl Drop for PhysicalCoordinates {
        fn drop(&mut self) {
            unsafe {
                let _ = SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
    struct WinRtApartment;
    impl Drop for WinRtApartment {
        fn drop(&mut self) {
            unsafe {
                RoUninitialize();
            }
        }
    }

    struct OwnedWindow(HWND);
    impl Drop for OwnedWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
    struct OwnedClass {
        name: Vec<u16>,
        instance: HINSTANCE,
        brush: HBRUSH,
    }
    impl Drop for OwnedClass {
        fn drop(&mut self) {
            unsafe {
                let _ = UnregisterClassW(PCWSTR(self.name.as_ptr()), Some(self.instance));
                let _ = DeleteObject(self.brush.into());
            }
        }
    }
    impl OwnedClass {
        fn new(background: COLORREF) -> crate::Result<Self> {
            unsafe extern "system" fn procedure(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
                // 测试窗口没有业务事件；默认过程验证绘制区域，不发送输入或抢焦点。
                unsafe { DefWindowProcW(h, m, w, l) }
            }
            let name: Vec<u16> = format!("RustShot.LivePin.VisibleSmoke.{}", unsafe {
                GetCurrentThreadId()
            })
            .encode_utf16()
            .chain(Some(0))
            .collect();
            let instance: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
            let brush = unsafe { CreateSolidBrush(background) };
            let class = Self {
                name,
                instance,
                brush,
            };
            let descriptor = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance,
                lpszClassName: PCWSTR(class.name.as_ptr()),
                hbrBackground: brush,
                ..Default::default()
            };
            if unsafe { RegisterClassW(&descriptor) } == 0 {
                return Err(windows::core::Error::from_win32().into());
            }
            Ok(class)
        }
        fn window(&self, bounds: RECT, owner: Option<HWND>) -> crate::Result<OwnedWindow> {
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    PCWSTR(self.name.as_ptr()),
                    w!("RustShot live pin owned smoke"),
                    WS_POPUP,
                    bounds.left,
                    bounds.top,
                    bounds.right - bounds.left,
                    bounds.bottom - bounds.top,
                    owner,
                    None,
                    Some(self.instance),
                    None,
                )?
            };
            Ok(OwnedWindow(hwnd))
        }
    }

    fn skip(reason: &str) {
        // 直接写 stderr，让未加 --nocapture 的显式测试也能看见安全跳过原因。
        let _ = writeln!(std::io::stderr(), "pin_live_smoke 可见合成跳过：{reason}");
    }
    fn system_light() -> bool {
        let mut value = 0u32;
        let mut bytes = std::mem::size_of::<u32>() as u32;
        unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                w!("AppsUseLightTheme"),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut value as *mut u32).cast()),
                Some(&mut bytes),
            ) == ERROR_SUCCESS
                && value != 0
        }
    }
    fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
        COLORREF(u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16)
    }
    fn sample_points(r: RECT) -> [POINT; 9] {
        let xs = [r.left + 1, (r.left + r.right) / 2, r.right - 2];
        let ys = [r.top + 1, (r.top + r.bottom) / 2, r.bottom - 2];
        std::array::from_fn(|i| POINT {
            x: xs[i % 3],
            y: ys[i / 3],
        })
    }
    fn safe_patch() -> Option<RECT> {
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        let virtual_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let virtual_top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let virtual_right = virtual_left + unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let virtual_bottom = virtual_top + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        for seed in [
            (0, 0),
            (virtual_left, virtual_top),
            (virtual_right - 1, virtual_top),
            (virtual_left, virtual_bottom - 1),
            (virtual_right - 1, virtual_bottom - 1),
        ] {
            let area = crate::pin_native::work_area(seed);
            if area.width < (PATCH_W + 16) as u32 || area.height < (PATCH_H + 16) as u32 {
                continue;
            }
            for (x, y) in [
                (area.x + 8, area.y + 8),
                (area.x + area.width as i32 - PATCH_W - 8, area.y + 8),
                (area.x + 8, area.y + area.height as i32 - PATCH_H - 8),
                (
                    area.x + area.width as i32 - PATCH_W - 8,
                    area.y + area.height as i32 - PATCH_H - 8,
                ),
            ] {
                let bounds = RECT {
                    left: x,
                    top: y,
                    right: x + PATCH_W,
                    bottom: y + PATCH_H,
                };
                if cursor.x >= bounds.left - 32
                    && cursor.x < bounds.right + 32
                    && cursor.y >= bounds.top - 32
                    && cursor.y < bounds.bottom + 32
                {
                    continue;
                }
                let desktop_only = sample_points(bounds).into_iter().all(|point| {
                    let hwnd = unsafe { WindowFromPoint(point) };
                    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
                    let mut class = [0u16; 128];
                    let count = unsafe { GetClassNameW(root, &mut class) }.max(0) as usize;
                    matches!(
                        String::from_utf16_lossy(&class[..count]).as_str(),
                        "Progman" | "WorkerW"
                    )
                });
                if desktop_only {
                    return Some(bounds);
                }
            }
        }
        None
    }
    fn fill_window(hwnd: HWND, bounds: RECT, color: COLORREF) {
        unsafe {
            let dc = GetDC(Some(hwnd));
            let brush = CreateSolidBrush(color);
            FillRect(dc, &bounds, brush);
            let _ = DeleteObject(brush.into());
            ReleaseDC(Some(hwnd), dc);
        }
    }
    fn settle() {
        let until = Instant::now() + Duration::from_millis(40);
        while Instant::now() < until {
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        unsafe {
            let _ = DwmFlush();
        }
    }
    fn own_pixels(hwnd: HWND, bounds: RECT, points: &[POINT]) -> Option<Vec<COLORREF>> {
        let mut owner = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut owner));
        }
        if owner != std::process::id()
            || !sample_points(bounds)
                .into_iter()
                .all(|point| unsafe { WindowFromPoint(point) } == hwnd)
        {
            return None;
        }
        unsafe {
            let dc = GetDC(None);
            if dc.is_invalid() {
                return None;
            }
            let mut colors = Vec::new();
            for &point in points {
                // 每次只读一个像素，紧邻读回再核对其可见 HWND。
                if WindowFromPoint(point) != hwnd {
                    ReleaseDC(None, dc);
                    return None;
                }
                let color = GetPixel(dc, point.x, point.y);
                if color.0 == u32::MAX {
                    ReleaseDC(None, dc);
                    return None;
                }
                colors.push(color);
            }
            ReleaseDC(None, dc);
            Some(colors)
        }
    }
    fn close_color(actual: COLORREF, expected: COLORREF, tolerance: i32) -> bool {
        [0, 8, 16].into_iter().all(|shift| {
            (((actual.0 >> shift) & 255) as i32 - ((expected.0 >> shift) & 255) as i32).abs()
                <= tolerance
        })
    }
    fn blend(foreground: COLORREF, background: COLORREF, alpha: u32) -> COLORREF {
        let channel = |shift: u32| -> u32 {
            (((foreground.0 >> shift) & 255_u32) * alpha
                + ((background.0 >> shift) & 255_u32) * (255_u32 - alpha)
                + 127_u32)
                / 255_u32
        };
        COLORREF(channel(0_u32) | channel(8_u32) << 8_u32 | channel(16_u32) << 16_u32)
    }

    #[test]
    #[ignore = "显式运行：仅在远离光标的桌面空白角落展示主题色自有小窗，核验 DWM crop 与 Layered alpha 的自有合成像素；无安全位置直接明示跳过"]
    fn pin_live_smoke_visible_owned_crop_and_layered_alpha() {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0.is_null() {
            skip("无法取得物理像素坐标上下文");
            return;
        }
        let _coordinates = PhysicalCoordinates(previous);
        let Some(patch) = safe_patch() else {
            skip("未找到不遮挡应用且远离光标的桌面空白角落");
            return;
        };
        let background = if system_light() {
            rgb(208, 208, 208)
        } else {
            rgb(36, 36, 36)
        };
        let left = if system_light() {
            rgb(140, 112, 88)
        } else {
            rgb(84, 54, 42)
        };
        let right = if system_light() {
            rgb(104, 136, 160)
        } else {
            rgb(54, 96, 132)
        };
        let class = OwnedClass::new(background).unwrap();
        let backdrop = class.window(patch, None).unwrap();
        let source_bounds = RECT {
            left: patch.left + 8,
            top: patch.top + 8,
            right: patch.left + 8 + SOURCE_W,
            bottom: patch.top + 8 + SOURCE_H,
        };
        let host_bounds = RECT {
            left: patch.left + 80,
            top: patch.top + 8,
            right: patch.left + 80 + SOURCE_W,
            bottom: patch.top + 8 + SOURCE_H,
        };
        let source = class.window(source_bounds, Some(backdrop.0)).unwrap();
        let host = class.window(host_bounds, Some(backdrop.0)).unwrap();
        let paint = || {
            fill_window(
                backdrop.0,
                RECT {
                    left: 0,
                    top: 0,
                    right: PATCH_W,
                    bottom: PATCH_H,
                },
                background,
            );
            fill_window(
                source.0,
                RECT {
                    left: 0,
                    top: 0,
                    right: SOURCE_W / 2,
                    bottom: SOURCE_H,
                },
                left,
            );
            fill_window(
                source.0,
                RECT {
                    left: SOURCE_W / 2,
                    top: 0,
                    right: SOURCE_W,
                    bottom: SOURCE_H,
                },
                right,
            );
            fill_window(
                host.0,
                RECT {
                    left: 0,
                    top: 0,
                    right: SOURCE_W,
                    bottom: SOURCE_H,
                },
                background,
            );
        };
        paint();
        unsafe {
            for hwnd in [backdrop.0, source.0, host.0] {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
        settle();
        paint();
        unsafe {
            DwmFlush().unwrap();
        }
        let source_points = [
            POINT {
                x: source_bounds.left + 16,
                y: source_bounds.top + 24,
            },
            POINT {
                x: source_bounds.left + 48,
                y: source_bounds.top + 24,
            },
        ];
        let Some(source_colors) = own_pixels(source.0, source_bounds, &source_points) else {
            skip("源样本被遮挡或自有像素不可读");
            return;
        };
        assert!(
            close_color(source_colors[0], left, 3) && close_color(source_colors[1], right, 3),
            "自有源窗口样本未正确绘制：{source_colors:?}"
        );
        let thumbnail = Thumbnail::register(host.0, source.0).unwrap();
        assert_eq!(
            thumbnail.size().unwrap(),
            (SOURCE_W as u32, SOURCE_H as u32)
        );
        thumbnail
            .update(
                ScreenRect {
                    left: SOURCE_W / 2,
                    top: 8,
                    width: 32,
                    height: 32,
                },
                ScreenRect {
                    left: 8,
                    top: 8,
                    width: 32,
                    height: 32,
                },
                255,
                true,
            )
            .unwrap();
        settle();
        let points = [
            POINT {
                x: host_bounds.left + 16,
                y: host_bounds.top + 16,
            },
            POINT {
                x: host_bounds.left + 32,
                y: host_bounds.top + 32,
            },
        ];
        let Some(crop) = own_pixels(host.0, host_bounds, &points) else {
            skip("普通投影位置被遮挡或自有像素不可读");
            return;
        };
        assert!(
            crop.iter().all(|&color| close_color(color, right, 5)),
            "DWM crop 合成不符：实际 {crop:?}，预期 {right:?}"
        );
        crate::pin_native::opacity(host.0, 180).unwrap();
        settle();
        let Some(alpha) = own_pixels(host.0, host_bounds, &points) else {
            skip("Layered 投影位置被遮挡或自有像素不可读");
            return;
        };
        let expected = blend(right, background, 180);
        assert!(
            alpha.iter().all(|&color| close_color(color, expected, 6)),
            "DWM 与 Layered alpha 合成不符：实际 {alpha:?}，预期 {expected:?}"
        );
        assert!(
            ![backdrop.0, source.0, host.0].contains(&unsafe { GetForegroundWindow() }),
            "专项窗口取得前台焦点"
        );
        let _ = writeln!(
            std::io::stderr(),
            "pin_live_smoke：自有 DWM crop 与 Layered alpha 可见合成通过"
        );
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED).unwrap();
        }
        let _apartment = WinRtApartment;
        let supported = match GraphicsCaptureSession::IsSupported() {
            Ok(supported) => supported,
            Err(error) if matches!(error.code().0 as u32, 0x80040154 | 0x80040111) => {
                skip("自有 DWM 合成已通过；系统未提供 WGC 类型，仅跳过 WGC 单帧部分");
                return;
            }
            Err(error) => panic!("WGC 支持状态查询失败：{error}"),
        };
        if !supported {
            skip("自有 DWM 合成已通过；系统报告不支持 WGC，仅跳过 WGC 单帧部分");
            return;
        }
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(source.0, Some(&mut process_id)) };
        assert_eq!(process_id, std::process::id());
        assert_ne!(thread_id, 0);
        let region = ScreenRect {
            left: SOURCE_W / 2,
            top: 8,
            width: 32,
            height: 32,
        };
        let fallback = crate::capture::Image {
            width: 32,
            height: 32,
            bgra: vec![0; 32 * 32 * 4],
            readback_ms: 0.,
        };
        let (frame, latest) = crate::pin_live_snapshot::capture(
            SourceIdentity {
                hwnd: source.0 .0 as usize,
                process_id,
                thread_id,
            },
            region,
            &fallback,
        );
        assert!(latest, "系统支持 WGC，但自有源窗单帧失败并回退冻结图");
        assert_eq!((frame.width, frame.height), (32, 32));
        assert_eq!(frame.bgra.len(), 32 * 32 * 4);
        let index = (16 * frame.width as usize + 16) * 4;
        let pixel = &frame.bgra[index..index + 4];
        let color = rgb(pixel[2], pixel[1], pixel[0]);
        assert!(
            close_color(color, right, 6) && pixel[3] >= 250,
            "自有 WGC ROI 中心 BGRA 不符：实际 {pixel:?}，预期颜色 {right:?} / alpha255"
        );
        let _ = writeln!(
            std::io::stderr(),
            "pin_live_smoke：自有 WGC 单帧 ROI 尺寸与中心 BGRA 通过"
        );
        // thumbnail/host/source/backdrop/class 按逆序 RAII 释放，断言失败也关闭自有窗口。
    }
}
