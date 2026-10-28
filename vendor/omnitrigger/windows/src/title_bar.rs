//! 主窗口标题栏：界面延伸到窗口顶部，最小化、最大化、关闭按钮仍由 DWM 绘制和响应。
//!
//! 客户区覆盖原标题栏后，界面需在按钮位置保持透明，系统按钮才会显露。

use std::mem::size_of;
use windows::Win32::{
    Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DwmDefWindowProc, DwmExtendFrameIntoClientArea, DwmGetWindowAttribute,
            DwmSetWindowAttribute, DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CAPTION_COLOR,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
        },
        Gdi::{ClientToScreen, ScreenToClient},
    },
    UI::{
        Controls::MARGINS,
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            DefWindowProcW, GetClientRect, GetWindowLongPtrW, GetWindowRect, IsZoomed,
            SetWindowPos, GWL_STYLE, HTCAPTION, HTCLIENT, HTTOP, NCCALCSIZE_PARAMS,
            SM_CXPADDEDBORDER, SM_CYFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SWP_NOZORDER, WM_ACTIVATE, WM_DPICHANGED, WM_NCCALCSIZE, WM_NCDESTROY,
            WM_NCHITTEST, WM_SETTINGCHANGE, WS_CAPTION,
        },
    },
};

const SUBCLASS_ID: usize = 0x4f54_5442;

/// 系统标题按钮占据的区域，以客户区右上角为起点，单位为物理像素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionButtons {
    pub width: i32,
    pub height: i32,
}

pub(crate) fn extend_into_title_bar(hwnd: isize) -> Result<(), String> {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    unsafe {
        if !SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0).as_bool() {
            return Err("无法接管主窗口标题栏消息".into());
        }
        apply_frame(hwnd);
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .map_err(|error| format!("无法刷新主窗口边框：{error}"))
    }
}

pub(crate) fn caption_buttons(hwnd: isize) -> Option<CaptionButtons> {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let mut bounds = RECT::default();
    let mut window = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_BUTTON_BOUNDS,
            &mut bounds as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
        .ok()?;
        GetWindowRect(hwnd, &mut window).ok()?;
        GetClientRect(hwnd, &mut client).ok()?;
        if !ClientToScreen(hwnd, &mut origin).as_bool() {
            return None;
        }
    }
    // 按钮边界相对于窗口矩形，换算到客户区。
    let left = window.left + bounds.left - origin.x;
    let bottom = window.top + bounds.bottom - origin.y;
    (bounds.right > bounds.left && bottom > 0).then(|| CaptionButtons {
        width: (client.right - left).max(0),
        height: bottom,
    })
}

/// 设置按钮下方的 DWM 底色，与界面相邻区域一致。`rgb` 为 0xRRGGBB。
pub(crate) fn set_caption_color(hwnd: isize, rgb: u32) {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let color = COLORREF((rgb & 0xff) << 16 | (rgb & 0xff00) | (rgb >> 16 & 0xff));
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &color as *const COLORREF as *const _,
            size_of::<COLORREF>() as u32,
        );
    }
}

/// 整个窗口作为 DWM 框架；界面为深色，按钮固定用深色样式。
unsafe fn apply_frame(hwnd: HWND) {
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    let dark = 1i32;
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const i32 as *const _,
            size_of::<i32>() as u32,
        );
    }
}

/// 上边缘的缩放感应高度，与系统窗框一致。
unsafe fn resize_border(hwnd: HWND) -> i32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        // 系统按钮的命中、悬停、按下与贴靠布局均交给 DWM。
        let mut result = LRESULT(0);
        if DwmDefWindowProc(hwnd, msg, wparam, lparam, &mut result).as_bool() {
            return result;
        }
        match msg {
            // 全屏时 GPUI 会移除标题栏样式，此时沿用默认窗框计算。
            WM_NCCALCSIZE
                if wparam.0 != 0
                    && GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_CAPTION.0 == WS_CAPTION.0 =>
            {
                let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                // 保留左右下窗框用于缩放，只把标题栏并入客户区；最大化时窗口超出屏幕一圈窗框。
                DefWindowProcW(hwnd, msg, wparam, lparam);
                params.rgrc[0].top = top
                    + if IsZoomed(hwnd).as_bool() {
                        resize_border(hwnd)
                    } else {
                        0
                    };
                LRESULT(0)
            }
            WM_NCHITTEST => {
                let hit = DefSubclassProc(hwnd, msg, wparam, lparam);
                if matches!(hit.0 as u32, HTCLIENT | HTCAPTION) && !IsZoomed(hwnd).as_bool() {
                    let mut point = POINT {
                        x: (lparam.0 & 0xffff) as i16 as i32,
                        y: (lparam.0 >> 16 & 0xffff) as i16 as i32,
                    };
                    if ScreenToClient(hwnd, &mut point).as_bool() && point.y < resize_border(hwnd) {
                        return LRESULT(HTTOP as isize);
                    }
                }
                hit
            }
            // GPUI 会按系统明暗主题重设窗框，这里在其后恢复。
            WM_ACTIVATE | WM_DPICHANGED | WM_SETTINGCHANGE => {
                let result = DefSubclassProc(hwnd, msg, wparam, lparam);
                apply_frame(hwnd);
                result
            }
            WM_NCDESTROY => {
                let _ = RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID);
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}
