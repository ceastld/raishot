//! 自动长截图只向初始选区内绑定的窗口投递滚轮；不移动光标、不改变焦点。
use crate::{geometry::ScreenRect, long_auto::AUTO_WHEEL_DELTA, stitch::ScrollAxis, Result};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::ScreenToClient,
    UI::{Input::KeyboardAndMouse::IsWindowEnabled, WindowsAndMessaging::*},
};

/// 只读原始绑定；不是窗口当前有效性或目标实际消费滚轮的证明。
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct ScrollTargetProbe {
    #[serde(rename = "window")]
    pub(crate) window: usize,
    #[serde(rename = "root")]
    pub(crate) root: usize,
    #[serde(rename = "process")]
    pub(crate) process: u32,
    #[serde(rename = "thread")]
    pub(crate) thread: u32,
    #[serde(rename = "root_process")]
    pub(crate) root_process: u32,
    #[serde(rename = "root_thread")]
    pub(crate) root_thread: u32,
    #[serde(rename = "class")]
    pub(crate) class: String,
    #[serde(rename = "root_class")]
    pub(crate) root_class: String,
    #[serde(rename = "window_rect", serialize_with = "serialize_screen_rect")]
    pub(crate) window_rect: ScreenRect,
    #[serde(rename = "root_rect", serialize_with = "serialize_screen_rect")]
    pub(crate) root_rect: ScreenRect,
    #[serde(rename = "region", serialize_with = "serialize_screen_rect")]
    pub(crate) region: ScreenRect,
    #[serde(rename = "point")]
    pub(crate) point: (i32, i32),
}
fn serialize_screen_rect<S: serde::Serializer>(
    rect: &ScreenRect,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    let mut wire = serializer.serialize_struct("ScreenRect", 4)?;
    wire.serialize_field("left", &rect.left)?;
    wire.serialize_field("top", &rect.top)?;
    wire.serialize_field("width", &rect.width)?;
    wire.serialize_field("height", &rect.height)?;
    wire.end()
}
fn frozen_screen_rect(rect: RECT) -> ScreenRect {
    // bind 已由 covers / inside 证明两个冻结矩形非空；i32 端点差始终可容纳于 u32。
    ScreenRect {
        left: rect.left,
        top: rect.top,
        width: (i64::from(rect.right) - i64::from(rect.left)) as u32,
        height: (i64::from(rect.bottom) - i64::from(rect.top)) as u32,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedScrollTarget {
    window: usize,
    root: usize,
    process: u32,
    thread: u32,
    root_process: u32,
    root_thread: u32,
    class: String,
    root_class: String,
    window_rect: RECT,
    root_rect: RECT,
    region: ScreenRect,
    point: (i32, i32),
    label: String,
}
fn class_name(window: HWND) -> String {
    let mut buffer = [0_u16; 256];
    let length = unsafe { GetClassNameW(window, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length])
}
fn title(window: HWND) -> String {
    let mut buffer = [0_u16; 256];
    let length = unsafe { GetWindowTextW(window, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length])
}
fn owner(window: HWND) -> (u32, u32) {
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
    (process, thread)
}
fn controlled(window: HWND) -> bool {
    let class = class_name(window);
    class == "RustShot.StartupSurface"
        || class == "RustShot.MVP.Window"
        || matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd")
        || class == "Zed::Window"
            && (owner(window).0 == std::process::id() || title(window).starts_with(concat!(crate::brand_name!(), " ·")))
}
fn rect(window: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(window, &mut rect)?;
    }
    Ok(rect)
}
fn inside(rect: RECT, point: (i32, i32)) -> bool {
    point.0 >= rect.left && point.0 < rect.right && point.1 >= rect.top && point.1 < rect.bottom
}
fn covers(rect: RECT, region: ScreenRect) -> bool {
    region.width > 0
        && region.height > 0
        && region.left >= rect.left
        && region.top >= rect.top
        && region.left as i64 + region.width as i64 <= rect.right as i64
        && region.top as i64 + region.height as i64 <= rect.bottom as i64
}
fn packed_point(point: (i32, i32)) -> Result<LPARAM> {
    let x = i16::try_from(point.0).map_err(|_| {
        tr!(
            "选区坐标超出滚轮消息支持范围",
            "Selection coordinates are out of range for wheel messages"
        )
    })?;
    let y = i16::try_from(point.1).map_err(|_| {
        tr!(
            "选区坐标超出滚轮消息支持范围",
            "Selection coordinates are out of range for wheel messages"
        )
    })?;
    Ok(LPARAM(
        ((x as u16 as u32) | ((y as u16 as u32) << 16)) as i32 as isize,
    ))
}
fn wheel_message(axis: ScrollAxis, forward: bool, amount: i16) -> Result<(u32, WPARAM)> {
    if amount != AUTO_WHEEL_DELTA {
        return Err("自动滚动每步须为半格（60）".into());
    }
    let delta = amount
        * if (axis == ScrollAxis::Vertical) == forward {
            -1
        } else {
            1
        };
    let message = match axis {
        ScrollAxis::Vertical => WM_MOUSEWHEEL,
        ScrollAxis::Horizontal => WM_MOUSEHWHEEL,
    };
    Ok((message, WPARAM((delta as u16 as u32 as usize) << 16)))
}
fn deepest(window: HWND, point: (i32, i32)) -> HWND {
    let mut current = window;
    for _ in 0..32 {
        let mut local = POINT {
            x: point.0,
            y: point.1,
        };
        if !unsafe { ScreenToClient(current, &mut local) }.as_bool() {
            break;
        }
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                local,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == current {
            break;
        }
        current = child;
    }
    current
}
fn candidate_points(region: ScreenRect, cursor: Option<(i32, i32)>) -> Result<[(i32, i32); 3]> {
    let at = |numerator: u32, denominator: u32| -> Result<(i32, i32)> {
        Ok((
            i32::try_from(
                region.left as i64 + region.width as i64 * numerator as i64 / denominator as i64,
            )?,
            i32::try_from(
                region.top as i64 + region.height as i64 * numerator as i64 / denominator as i64,
            )?,
        ))
    };
    // 全局物理坐标采用左上包含、右下不包含；端点先提升到 i64，避免负屏溢出。
    let preferred = cursor.filter(|&(x, y)| {
        x as i64 >= region.left as i64
            && y as i64 >= region.top as i64
            && (x as i64) < region.left as i64 + region.width as i64
            && (y as i64) < region.top as i64 + region.height as i64
    });
    Ok([
        match preferred {
            Some(point) => point,
            None => at(1, 2)?,
        },
        at(1, 3)?,
        at(2, 3)?,
    ])
}
impl VerifiedScrollTarget {
    pub(crate) fn discover(region: ScreenRect) -> Result<Self> {
        let mut cursor = POINT::default();
        let cursor = unsafe { GetCursorPos(&mut cursor) }
            .ok()
            .map(|()| (cursor.x, cursor.y));
        let points = candidate_points(region, cursor)?;
        let mut reason = tr!(
            "选区内没有可自动滚动的目标窗口",
            "No scrollable window found in the selection"
        )
        .to_string();
        for point in points {
            let hit = unsafe {
                WindowFromPoint(POINT {
                    x: point.0,
                    y: point.1,
                })
            };
            if hit.is_invalid() {
                continue;
            }
            let root = unsafe { GetAncestor(hit, GA_ROOT) };
            if controlled(root) {
                continue;
            }
            match Self::bind(deepest(hit, point), region, point) {
                Ok(target) => return Ok(target),
                Err(error) => {
                    // STATIC 等子控件会被 WindowFromPoint 跳过；仍以现场命中的容器为准。
                    if let Ok(target) = Self::bind(hit, region, point) {
                        return Ok(target);
                    }
                    reason = error.to_string();
                }
            }
        }
        Err(trf!(
            "无法锁定自动滚动目标：{reason}；仍可手动滚动采集",
            "Can't lock an auto-scroll target: {reason}. You can still scroll manually"
        )
        .into())
    }
    fn bind(window: HWND, region: ScreenRect, point: (i32, i32)) -> Result<Self> {
        packed_point(point)?;
        let root = unsafe { GetAncestor(window, GA_ROOT) };
        let (process, thread) = owner(window);
        let (root_process, root_thread) = owner(root);
        if window.is_invalid()
            || root.is_invalid()
            || controlled(root)
            || process == 0
            || thread == 0
            || root_process == 0
            || root_thread == 0
        {
            return Err(tr!(
                "目标不是可滚动的原始页面窗口",
                "The target is not a scrollable page window"
            )
            .into());
        }
        let window_rect = rect(window)?;
        let root_rect = rect(root)?;
        if !covers(root_rect, region) || !inside(window_rect, point) {
            return Err(tr!(
                "选区跨越目标窗口或不位于目标内容内",
                "The selection spans windows or lies outside the target content"
            )
            .into());
        }
        let name = title(root);
        let class = class_name(window);
        let root_class = class_name(root);
        let target = Self {
            window: window.0 as usize,
            root: root.0 as usize,
            process,
            thread,
            root_process,
            root_thread,
            label: if name.is_empty() {
                format!("{root_class} · PID {process}")
            } else {
                name
            },
            class,
            root_class,
            window_rect,
            root_rect,
            region,
            point,
        };
        target.validate(region, ScrollAxis::Vertical)?;
        Ok(target)
    }
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
    pub(crate) fn probe(&self) -> ScrollTargetProbe {
        ScrollTargetProbe {
            window: self.window,
            root: self.root,
            process: self.process,
            thread: self.thread,
            root_process: self.root_process,
            root_thread: self.root_thread,
            class: self.class.clone(),
            root_class: self.root_class.clone(),
            window_rect: frozen_screen_rect(self.window_rect),
            root_rect: frozen_screen_rect(self.root_rect),
            region: self.region,
            point: self.point,
        }
    }
    /// 只交回绑定的原窗口；浮层关闭期间出现的其它窗口不能成为新焦点目标。
    pub(crate) fn restore_focus(
        &self,
        panel: HWND,
        region: ScreenRect,
        axis: ScrollAxis,
    ) -> Result<()> {
        self.validate(region, axis)?;
        unsafe {
            if IsWindow(Some(panel)).as_bool()
                && GetForegroundWindow() == panel
                && self.root_process != std::process::id()
            {
                let _ = SetForegroundWindow(HWND(self.root as *mut _));
            }
        }
        Ok(())
    }
    pub(crate) fn validate(&self, region: ScreenRect, _axis: ScrollAxis) -> Result<()> {
        let window = HWND(self.window as *mut _);
        let root = HWND(self.root as *mut _);
        if region != self.region
            || !unsafe { IsWindow(Some(window)) }.as_bool()
            || !unsafe { IsWindow(Some(root)) }.as_bool()
            || !unsafe { IsWindowVisible(window) }.as_bool()
            || !unsafe { IsWindowVisible(root) }.as_bool()
            || unsafe { IsIconic(root) }.as_bool()
            || !unsafe { IsWindowEnabled(window) }.as_bool()
            || !unsafe { IsWindowEnabled(root) }.as_bool()
            || owner(window) != (self.process, self.thread)
            || owner(root) != (self.root_process, self.root_thread)
            || unsafe { GetAncestor(window, GA_ROOT) }.0 as usize != self.root
            || class_name(window) != self.class
            || class_name(root) != self.root_class
            || rect(window)? != self.window_rect
            || rect(root)? != self.root_rect
        {
            return Err(tr!(
                "原目标已关闭、移动、改变尺寸或身份；自动滚动已停止",
                "The target was closed, moved, resized or changed; auto-scroll stopped"
            )
            .into());
        }
        let hit = unsafe {
            WindowFromPoint(POINT {
                x: self.point.0,
                y: self.point.1,
            })
        };
        if hit.is_invalid()
            || controlled(hit)
            || unsafe { GetAncestor(hit, GA_ROOT) }.0 as usize != self.root
            || hit != window && !unsafe { IsChild(window, hit) }.as_bool()
        {
            return Err(tr!(
                "原目标内容被其它窗口遮挡；自动滚动已停止",
                "The target is covered by another window; auto-scroll stopped"
            )
            .into());
        }
        Ok(())
    }
    /// 调用方先在队列门外校验目标；这里只投递消息，以便与下一次 read permit 原子排序。
    pub(crate) fn post_step_validated(
        &self,
        axis: ScrollAxis,
        forward: bool,
        amount: i16,
    ) -> Result<()> {
        let (message, keys) = wheel_message(axis, forward, amount)?;
        unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut _)),
                message,
                keys,
                packed_point(self.point)?,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_auto_cursor_inside_negative_screen_and_boundaries_are_preferred() {
        let region = ScreenRect {
            left: -1800,
            top: -900,
            width: 900,
            height: 600,
        };
        for cursor in [(-1700, -450), (-1800, -900), (-901, -301)] {
            let points = candidate_points(region, Some(cursor)).unwrap();
            assert_eq!(points, [cursor, (-1500, -700), (-1200, -500)]);
            let packed = packed_point(points[0]).unwrap().0 as u32;
            assert_eq!(packed as u16 as i16 as i32, cursor.0);
            assert_eq!((packed >> 16) as u16 as i16 as i32, cursor.1);
        }
    }
    #[test]
    fn long_auto_cursor_outside_or_read_failure_retains_center_and_fallbacks() {
        let region = ScreenRect {
            left: -1800,
            top: -900,
            width: 900,
            height: 600,
        };
        for cursor in [
            None,
            Some((-1801, -600)),
            Some((-1350, -901)),
            Some((-900, -600)),
            Some((-1350, -300)),
            Some((100, 100)),
        ] {
            assert_eq!(
                candidate_points(region, cursor).unwrap(),
                [(-1350, -600), (-1500, -700), (-1200, -500)]
            );
        }
    }
    #[test]
    fn long_auto_cursor_extreme_arithmetic_and_message_range_fail_closed() {
        let region = ScreenRect {
            left: i32::MIN,
            top: i32::MIN,
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(
            candidate_points(region, None).unwrap(),
            [
                (-1, -1),
                (-715_827_883, -715_827_883),
                (715_827_882, 715_827_882),
            ]
        );
        assert_eq!(
            candidate_points(region, Some((i32::MAX, 0))).unwrap()[0],
            (-1, -1)
        );
        for cursor in [(i32::MIN, i32::MIN), (i32::MAX - 1, i32::MAX - 1)] {
            let points = candidate_points(region, Some(cursor)).unwrap();
            assert_eq!(points[0], cursor);
            assert!(packed_point(points[0]).is_err());
        }
        assert!(candidate_points(
            ScreenRect {
                left: i32::MAX,
                top: 0,
                width: u32::MAX,
                height: 1,
            },
            None,
        )
        .is_err());
        let message_region = ScreenRect {
            left: i16::MIN as i32,
            top: i16::MIN as i32,
            width: 65536,
            height: 65536,
        };
        let cursor = (i16::MIN as i32, i16::MAX as i32);
        assert!(packed_point(candidate_points(message_region, Some(cursor)).unwrap()[0]).is_ok());
    }
    #[test]
    fn auto_native_signed_coordinates_and_region_coverage() {
        let point = packed_point((-1234, -456)).unwrap().0 as u32;
        assert_eq!(point as u16 as i16, -1234);
        assert_eq!((point >> 16) as u16 as i16, -456);
        assert!(packed_point((32768, 0)).is_err());
        let bounds = RECT {
            left: -1800,
            top: -400,
            right: 800,
            bottom: 1200,
        };
        assert!(covers(
            bounds,
            ScreenRect {
                left: -1600,
                top: -200,
                width: 320,
                height: 600
            }
        ));
        assert!(!covers(
            bounds,
            ScreenRect {
                left: 700,
                top: 0,
                width: 200,
                height: 100
            }
        ));
        assert!(!covers(
            bounds,
            ScreenRect {
                left: i32::MAX,
                top: 0,
                width: u32::MAX,
                height: 100
            }
        ));
    }
    #[test]
    fn long_auto_pacer_half_wheel_payload_keeps_axes_direction_and_modifiers() {
        for (axis, forward, message, delta) in [
            (ScrollAxis::Vertical, true, WM_MOUSEWHEEL, -60_i16),
            (ScrollAxis::Vertical, false, WM_MOUSEWHEEL, 60),
            (ScrollAxis::Horizontal, true, WM_MOUSEHWHEEL, 60),
            (ScrollAxis::Horizontal, false, WM_MOUSEHWHEEL, -60),
        ] {
            let (actual, wp) = wheel_message(axis, forward, 60).unwrap();
            assert_eq!(actual, message);
            assert_eq!((wp.0 >> 16) as u16 as i16, delta);
            assert_eq!(wp.0 & 0xffff, 0);
        }
        for wrong in [0, 1, 2, -60, 120, 240, i16::MAX] {
            assert!(wheel_message(ScrollAxis::Vertical, true, wrong).is_err());
        }
    }
}
