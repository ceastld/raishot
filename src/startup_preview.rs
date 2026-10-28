//! 原生预览几何和已完成绘制回执的纯状态；Win32 成功门由原生外层线程检查。
use crate::{
    geometry::{Rect, ScreenRect},
    Result,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PresentedWindowPreview {
    pub(crate) rect: Rect,
    /// 同一个原生请求内真实完成的绘制序号；0 和溢出都不能成为回执。
    pub(crate) serial: u64,
}

#[derive(Default)]
pub(crate) struct PreviewPresentation {
    presented: Option<PresentedWindowPreview>,
    last_serial: u64,
}

impl PreviewPresentation {
    /// 开始改变画面前撤销当前可见回执。不能持有回执锁调用 Win32。
    pub(crate) fn begin_paint(&mut self) {
        self.presented = None;
    }

    /// 仅供外层在真实 draw、GDI/DC 回收及必要 DWM 提交均成功后调用。
    /// 不查询或伪造这些 API 结果；无范围的成功画面同样推进绘制序号。
    /// 拖拽新建选区途中可短暂出现零宽／零高范围（抬起时由 Editor 回退），
    /// 这类不可确认的范围只是不产生回执，不能让整个截图会话失败。
    pub(crate) fn publish_completed(&mut self, canvas: Rect, rect: Option<Rect>) -> Result<()> {
        self.presented = None;
        let rect = rect.filter(|rect| rect.valid(canvas.width, canvas.height));
        let serial = self
            .last_serial
            .checked_add(1)
            .ok_or("原生窗口预览回执序号溢出")?;
        self.last_serial = serial;
        self.presented = rect.map(|rect| PresentedWindowPreview { rect, serial });
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Option<PresentedWindowPreview> {
        self.presented
    }
}

/// 仅命中本次已缓存窗口，遵守缓存 Z 序；没有真实点或有效目标就没有预览。
pub(crate) fn window_preview(
    canvas: Rect,
    point: Option<(i32, i32)>,
    windows: impl IntoIterator<Item = Rect>,
    eligible: bool,
) -> Option<Rect> {
    if !eligible
        || !canvas.valid(canvas.width, canvas.height)
        || canvas.width > i32::MAX as u32
        || canvas.height > i32::MAX as u32
    {
        return None;
    }
    let point = point.filter(|point| canvas.contains(*point))?;
    windows
        .into_iter()
        .find(|rect| rect.valid(canvas.width, canvas.height) && rect.contains(point))
}

/// 桌面（Progman／图标视图）等完整覆盖两块以上屏幕的自动目标降为指针所在屏幕；
/// 首帧不以“所有屏幕”作自动预览，避免整片高亮再收缩造成的闪烁。手动导航不经过这里。
pub(crate) fn auto_target_rect(
    rect: Rect,
    monitors: impl IntoIterator<Item = Rect>,
    point: (i32, i32),
) -> Rect {
    let mut covered = 0_usize;
    let mut current = None;
    for monitor in monitors {
        if monitor.width == 0 || monitor.height == 0 || rect.intersection(monitor) != Some(monitor)
        {
            continue;
        }
        covered += 1;
        if current.is_none() && monitor.contains(point) {
            current = Some(monitor);
        }
    }
    match current {
        Some(monitor) if covered >= 2 => monitor,
        _ => rect,
    }
}

/// WM_MOUSEMOVE/按钮 LPARAM 是 signed 客户区物理点；预览不把越界点夹到画面边缘。
pub(crate) fn client_point(value: isize) -> (i32, i32) {
    (
        value as u16 as i16 as i32,
        (value >> 16) as u16 as i16 as i32,
    )
}

/// 首次 GetCursorPos 和滚轮点使用 Windows 全局物理坐标，保留负屏幕原点。
pub(crate) fn local_point(bounds: ScreenRect, global: (i32, i32)) -> Option<(i32, i32)> {
    global
        .0
        .checked_sub(bounds.left)
        .zip(global.1.checked_sub(bounds.top))
}

#[cfg(test)]
mod startup_preview_tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn preview_uses_first_valid_cached_window_without_monitor_fallback() {
        let canvas = Rect::full(640, 480);
        let front = rect(100, 80, 100, 120);
        let back = rect(40, 20, 300, 300);
        let invalid = rect(u32::MAX - 1, 80, 4, 120);
        assert_eq!(
            window_preview(canvas, Some((140, 120)), [invalid, front, back], true),
            Some(front)
        );
        assert_eq!(
            window_preview(canvas, Some((250, 120)), [front, back], true),
            Some(back)
        );
        assert_eq!(
            window_preview(canvas, Some((500, 400)), [front, back], true),
            None
        );
        assert_eq!(window_preview(canvas, Some((140, 120)), [], true), None);
    }

    #[test]
    fn multi_monitor_desktop_span_demotes_to_pointer_monitor() {
        let left = rect(0, 0, 1920, 1080);
        let right = rect(1920, 0, 2560, 1440);
        let monitors = [left, right];
        let desktop = rect(0, 0, 4480, 1440);
        assert_eq!(auto_target_rect(desktop, monitors, (100, 100)), left);
        assert_eq!(auto_target_rect(desktop, monitors, (3000, 1200)), right);
        // 点在两屏之外的空白区：保留原范围，不伪造屏幕。
        assert_eq!(auto_target_rect(desktop, monitors, (100, 1300)), desktop);
        // 仅覆盖一块屏幕或普通窗口不变。
        let maximized = rect(1920, 0, 2560, 1440);
        assert_eq!(
            auto_target_rect(maximized, monitors, (3000, 100)),
            maximized
        );
        let window = rect(1800, 100, 400, 300);
        assert_eq!(auto_target_rect(window, monitors, (1900, 200)), window);
        // 单屏桌面不变。
        assert_eq!(auto_target_rect(left, [left], (10, 10)), left);
    }

    #[test]
    fn missing_busy_or_unclamped_outside_point_has_no_fake_preview() {
        let canvas = Rect::full(640, 480);
        let window = rect(0, 0, 200, 200);
        for point in [
            None,
            Some((-1, 40)),
            Some((40, -1)),
            Some((640, 40)),
            Some((40, 480)),
        ] {
            assert_eq!(window_preview(canvas, point, [window], true), None);
        }
        assert_eq!(
            window_preview(canvas, Some((40, 40)), [window], false),
            None
        );
        assert_eq!(
            window_preview(canvas, Some((0, 0)), [window], true),
            Some(window)
        );
    }

    #[test]
    fn signed_client_and_negative_global_coordinates_keep_the_actual_point() {
        let packed = (u32::from((-7_i16) as u16) | (u32::from(19_u16) << 16)) as isize;
        assert_eq!(client_point(packed), (-7, 19));
        let bounds = ScreenRect {
            left: -1920,
            top: -240,
            width: 640,
            height: 480,
        };
        assert_eq!(local_point(bounds, (-1900, -200)), Some((20, 40)));
        assert_eq!(local_point(bounds, (-1921, -200)), Some((-1, 40)));
        assert_eq!(local_point(bounds, (i32::MAX, -200)), None);
    }

    #[test]
    fn middle_snapshot_cannot_be_filled_by_a_later_paint_and_keeps_its_original_rect() {
        let canvas = Rect::full(640, 480);
        let a = rect(20, 30, 100, 80);
        let b = rect(200, 100, 120, 90);
        let mut presentation = PreviewPresentation::default();
        // 纯状态模型；publish_completed 不是 Win32 绘制证明。
        presentation.publish_completed(canvas, Some(a)).unwrap();
        let old_middle = presentation.snapshot().unwrap();
        presentation.begin_paint();
        let during_paint_middle = presentation.snapshot();
        presentation.publish_completed(canvas, Some(b)).unwrap();
        assert_eq!(during_paint_middle, None);
        assert_eq!(old_middle.rect, a);
        let after = presentation.snapshot().unwrap();
        assert_eq!(after.rect, b);
        assert!(after.serial > old_middle.serial);
    }

    #[test]
    fn failed_or_invalid_publication_cannot_restore_a_previous_painted_rect() {
        let canvas = Rect::full(640, 480);
        let a = rect(20, 30, 100, 80);
        let mut presentation = PreviewPresentation::default();
        presentation.publish_completed(canvas, Some(a)).unwrap();
        presentation.begin_paint();
        // 真实绘制失败时调用方不会 publish；回执保持空。
        assert_eq!(presentation.snapshot(), None);
        // 越界或拖拽途中的零宽／零高范围不产生回执，也不终止会话。
        for invalid in [
            rect(u32::MAX, 0, 2, 2),
            rect(20, 30, 0, 80),
            rect(20, 30, 100, 0),
        ] {
            presentation
                .publish_completed(canvas, Some(invalid))
                .unwrap();
            assert_eq!(presentation.snapshot(), None);
        }
        presentation.publish_completed(canvas, None).unwrap();
        assert_eq!(presentation.snapshot(), None);
        presentation.publish_completed(canvas, Some(a)).unwrap();
        assert_eq!(presentation.snapshot().unwrap().serial, 6);
    }

    #[test]
    fn paint_serial_overflow_fails_closed_and_cannot_reuse_the_old_receipt() {
        let canvas = Rect::full(640, 480);
        let a = rect(20, 30, 100, 80);
        let mut presentation = PreviewPresentation {
            presented: Some(PresentedWindowPreview {
                rect: a,
                serial: u64::MAX,
            }),
            last_serial: u64::MAX,
        };
        assert!(presentation.publish_completed(canvas, Some(a)).is_err());
        assert_eq!(presentation.snapshot(), None);
    }
}
