//! 截图开始前缓存顶层窗口范围；控件识别在 GPUI 交接后由独立后台任务处理。
use crate::{
    geometry::{Rect, ScreenRect},
    Result,
};
use windows::Win32::{
    Foundation::*,
    Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS},
    UI::WindowsAndMessaging::*,
};
#[derive(Clone)]
pub struct WindowTarget {
    /// 冻结时的可见窗口边框，裁剪到画布；不含透明调整边缘。
    pub rect: Rect,
    /// 冻结时 GetWindowRect 的全局物理四边；不能用裁剪后的 rect 代替身份复核。
    pub global_bounds: RECT,
    pub title: String,
    pub hwnd: usize,
    pub process_id: u32,
    pub thread_id: u32,
}

pub(crate) fn visible_window_bounds(hwnd: HWND, window_bounds: RECT) -> RECT {
    let mut frame = RECT::default();
    let frame = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut frame as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok()
    .then_some(frame);
    resolve_visible_window_bounds(window_bounds, frame)
}

fn resolve_visible_window_bounds(window_bounds: RECT, frame: Option<RECT>) -> RECT {
    // 原始窗口范围包含不可见的调整边缘；DWM 失败或返回空框时才退回它。
    frame
        .filter(|frame| frame.right > frame.left && frame.bottom > frame.top)
        .unwrap_or(window_bounds)
}

pub fn windows(canvas: ScreenRect) -> Vec<WindowTarget> {
    struct Snapshot {
        canvas: ScreenRect,
        items: Vec<WindowTarget>,
    }
    unsafe extern "system" fn callback(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        unsafe {
            let snapshot = &mut *(data.0 as *mut Snapshot);
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return windows::core::BOOL(1);
            }
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            // 菜单、无标题 popup、工具面板也是可见内容；只跳过真正透传输入的层。
            if ex & WS_EX_TRANSPARENT.0 as isize != 0 {
                return windows::core::BOOL(1);
            }
            let mut cloaked = 0_u32;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
            {
                return windows::core::BOOL(1);
            }
            let mut r = RECT::default();
            let mut process_id = 0;
            let thread_id = GetWindowThreadProcessId(hwnd, Some(&mut process_id));
            if process_id == 0 || process_id == std::process::id() || thread_id == 0 {
                return windows::core::BOOL(1);
            }
            if GetWindowRect(hwnd, &mut r).is_err() || r.right <= r.left || r.bottom <= r.top {
                return windows::core::BOOL(1);
            }
            let visible = visible_window_bounds(hwnd, r);
            let mut title = [0_u16; 256];
            let n = GetWindowTextW(hwnd, &mut title);
            let left = visible.left.max(snapshot.canvas.left);
            let top = visible.top.max(snapshot.canvas.top);
            let right = visible
                .right
                .min(snapshot.canvas.left + snapshot.canvas.width as i32);
            let bottom = visible
                .bottom
                .min(snapshot.canvas.top + snapshot.canvas.height as i32);
            if right > left && bottom > top {
                snapshot.items.push(WindowTarget {
                    rect: Rect {
                        x: (left - snapshot.canvas.left) as u32,
                        y: (top - snapshot.canvas.top) as u32,
                        width: (right - left) as u32,
                        height: (bottom - top) as u32,
                    },
                    global_bounds: r,
                    title: if n > 0 {
                        String::from_utf16_lossy(&title[..n as usize])
                    } else {
                        tr!("窗口", "Window").into()
                    },
                    hwnd: hwnd.0 as usize,
                    process_id,
                    thread_id,
                });
            }
            windows::core::BOOL(1)
        }
    }
    let mut snapshot = Snapshot {
        canvas,
        items: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(callback),
            LPARAM((&mut snapshot as *mut Snapshot) as isize),
        );
    }
    snapshot.items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_selection_excludes_invisible_side_and_bottom_resize_borders() {
        let raw = RECT {
            left: -1007,
            top: 50,
            right: -193,
            bottom: 657,
        };
        let visible = RECT {
            left: -1000,
            top: 50,
            right: -200,
            bottom: 650,
        };
        assert_eq!(resolve_visible_window_bounds(raw, Some(visible)), visible);
    }

    #[test]
    fn window_selection_keeps_native_bounds_when_extended_frame_is_unavailable_or_empty() {
        let raw = RECT {
            left: 20,
            top: -400,
            right: 620,
            bottom: -100,
        };
        for frame in [None, Some(RECT::default()), Some(RECT { right: 20, ..raw })] {
            assert_eq!(resolve_visible_window_bounds(raw, frame), raw);
        }
    }
}

pub fn sample(source: &crate::scene::SourcePixels<'_>, point: (i32, i32)) -> Result<u32> {
    let x = point.0.clamp(0, source.canvas.width as i32 - 1) as u32;
    let y = point.1.clamp(0, source.canvas.height as i32 - 1) as u32;
    let pixel = source.read(Rect {
        x,
        y,
        width: 1,
        height: 1,
    });
    Ok((pixel.bgra[2] as u32) << 16 | (pixel.bgra[1] as u32) << 8 | pixel.bgra[0] as u32)
}
