//! 工具栏拖动与重新吸附；指针使用物理像素，布局和吸附距离使用 DIP。
use crate::toolbar_layout::PanelRect;

const SNAP_ENTER_DIP: f32 = 16.;
const SNAP_LEAVE_DIP: f32 = 24.;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SnapTarget {
    pub monitor: usize,
    /// 用跟随选区状态（手动位置为 None）的真实布局计算。
    pub rect: PanelRect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DragUpdate {
    pub position: Option<(f32, f32)>,
    pub monitor: usize,
    pub snap_preview: Option<PanelRect>,
    pub dragging: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolbarDrag {
    start_pointer_px: (f32, f32),
    start_origin_dip: (f32, f32),
    initial_position: Option<(f32, f32)>,
    initial_monitor: usize,
    scale: f32,
    threshold_px: (f32, f32),
    dragging: bool,
    snap_active: bool,
}

impl ToolbarDrag {
    /// 指针必须来自同一客户坐标系：GPUI 客户 DIP 乘窗口倍率，不能转成图片世界坐标。
    /// `threshold_px` 由调用方提供 Windows 拖动阈值；倍率在本次手势内固定。
    pub(crate) fn new(
        pointer_px: (f32, f32),
        origin_dip: (f32, f32),
        initial_position: Option<(f32, f32)>,
        initial_monitor: usize,
        scale: f32,
        threshold_px: (f32, f32),
    ) -> Self {
        Self {
            start_pointer_px: pointer_px,
            start_origin_dip: origin_dip,
            initial_position,
            initial_monitor,
            scale: if scale.is_finite() && scale > 0. {
                scale
            } else {
                1.
            },
            threshold_px: (threshold_px.0.abs().max(1.), threshold_px.1.abs().max(1.)),
            dragging: false,
            snap_active: false,
        }
    }

    /// 手动布局回调复用 ToolbarLayout 的屏内夹紧、折行和属性栏空间预算。
    /// 距离比较使用夹紧后的实际主栏，预览期间仍保留手动位置；抬起才恢复跟随。
    pub(crate) fn update(
        &mut self,
        pointer_px: (f32, f32),
        pointer_monitor: usize,
        follow_target: SnapTarget,
        place_manual: impl FnOnce(usize, (f32, f32)) -> PanelRect,
    ) -> DragUpdate {
        let delta_px = (
            pointer_px.0 - self.start_pointer_px.0,
            pointer_px.1 - self.start_pointer_px.1,
        );
        if !self.dragging {
            self.dragging =
                delta_px.0.abs() >= self.threshold_px.0 || delta_px.1.abs() >= self.threshold_px.1;
            if !self.dragging {
                return self.initial();
            }
        }
        let desired = (
            self.start_origin_dip.0 + delta_px.0 / self.scale,
            self.start_origin_dip.1 + delta_px.1 / self.scale,
        );
        let rect = place_manual(pointer_monitor, desired);
        let distance = (rect.x - follow_target.rect.x).hypot(rect.y - follow_target.rect.y);
        let limit = if self.snap_active {
            SNAP_LEAVE_DIP
        } else {
            SNAP_ENTER_DIP
        };
        self.snap_active = pointer_monitor == follow_target.monitor && distance <= limit;
        DragUpdate {
            position: Some((rect.x, rect.y)),
            monitor: pointer_monitor,
            snap_preview: self.snap_active.then_some(follow_target.rect),
            dragging: true,
        }
    }

    /// Up 可能早于最后一个 Move，必须再以最终指针位置结算一次。
    pub(crate) fn finish(
        mut self,
        pointer_px: (f32, f32),
        pointer_monitor: usize,
        follow_target: SnapTarget,
        place_manual: impl FnOnce(usize, (f32, f32)) -> PanelRect,
    ) -> DragUpdate {
        let mut update = self.update(pointer_px, pointer_monitor, follow_target, place_manual);
        if update.snap_preview.take().is_some() {
            update.position = None;
            update.monitor = follow_target.monitor;
        }
        update
    }

    pub(crate) fn cancel(self) -> DragUpdate {
        self.initial()
    }

    fn initial(&self) -> DragUpdate {
        DragUpdate {
            position: self.initial_position,
            monitor: self.initial_monitor,
            snap_preview: None,
            dragging: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{annotation::Tool, geometry::Rect, toolbar_layout::ToolbarLayout};

    fn rect(x: f32, y: f32) -> PanelRect {
        PanelRect {
            x,
            y,
            width: 700.,
            height: 48.,
        }
    }

    fn target(monitor: usize, x: f32, y: f32) -> SnapTarget {
        SnapTarget {
            monitor,
            rect: rect(x, y),
        }
    }

    fn unclamped(_: usize, position: (f32, f32)) -> PanelRect {
        rect(position.0, position.1)
    }

    #[test]
    fn grip_jitter_preserves_following_and_manual_placement() {
        for initial_position in [None, Some((80., 400.))] {
            let mut drag = ToolbarDrag::new(
                (1000., 1000.),
                (100., 500.),
                initial_position,
                2,
                1.25,
                (8., 8.),
            );
            let update = drag.update((1005., 1003.), 1, target(0, 100., 500.), |_, _| {
                panic!("阈值内不应进入手动布局")
            });
            assert_eq!(update.position, initial_position);
            assert_eq!(update.monitor, 2);
            assert!(!update.dragging);
            assert!(update.snap_preview.is_none());
        }
    }

    #[test]
    fn physical_threshold_and_negative_pointer_convert_to_dip_once() {
        let mut drag = ToolbarDrag::new((-1200., -400.), (-700., -200.), None, 0, 1.5, (8., 8.));
        assert!(drag
            .update((-1193., -393.), 0, target(0, 900., 500.), unclamped)
            .position
            .is_none());
        let update = drag.update((-1140., -370.), 1, target(0, 900., 500.), unclamped);
        assert_eq!(update.position, Some((-660., -180.)));
        assert_eq!(update.monitor, 1);
        assert!(update.dragging);
    }

    #[test]
    fn release_uses_final_pointer_instead_of_last_move() {
        let mut drag = ToolbarDrag::new((100., 100.), (200., 200.), None, 0, 1., (4., 4.));
        let follow = target(0, 900., 500.);
        let moved = drag.update((140., 140.), 0, follow, unclamped);
        assert_eq!(moved.position, Some((240., 240.)));
        let released = drag.finish((160., 180.), 0, follow, unclamped);
        assert_eq!(released.position, Some((260., 280.)));
        assert!(released.snap_preview.is_none());
    }

    #[test]
    fn release_can_start_drag_without_a_move_event() {
        let drag = ToolbarDrag::new((100., 100.), (200., 200.), None, 0, 2., (8., 8.));
        let released = drag.finish((120., 140.), 1, target(0, 900., 500.), unclamped);
        assert_eq!(released.position, Some((210., 220.)));
        assert_eq!(released.monitor, 1);
        assert!(released.dragging);
    }

    #[test]
    fn snap_preview_has_hysteresis_and_release_restores_following() {
        let mut drag = ToolbarDrag::new((1000., 1000.), (100., 500.), None, 0, 1., (4., 4.));
        let follow = target(0, 100., 500.);
        assert!(drag
            .update((1030., 1000.), 0, follow, unclamped)
            .snap_preview
            .is_none());
        let enter = drag.update((1015., 1000.), 0, follow, unclamped);
        assert_eq!(enter.snap_preview, Some(follow.rect));
        assert_eq!(enter.position, Some((115., 500.)));
        assert!(drag
            .update((1020., 1000.), 0, follow, unclamped)
            .snap_preview
            .is_some());
        assert!(drag
            .update((1025., 1000.), 0, follow, unclamped)
            .snap_preview
            .is_none());
        let released = drag.finish((1015., 1000.), 0, follow, unclamped);
        assert_eq!(released.position, None);
        assert_eq!(released.monitor, follow.monitor);
        assert!(released.snap_preview.is_none());
    }

    #[test]
    fn manual_drag_migrates_and_clamps_using_actual_toolbar_layout() {
        let screens = [
            Rect::full(1920, 1080),
            Rect {
                x: 1920,
                y: 0,
                width: 1280,
                height: 1024,
            },
        ];
        let crop = Rect {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let slots = crate::toolbar_config::ToolbarConfig::default().slots();
        let follow = SnapTarget {
            monitor: 0,
            rect: ToolbarLayout::new_capture_with_tools(
                screens[0],
                crop,
                1.,
                None,
                Tool::Rectangle,
                &slots,
            )
            .primary,
        };
        let drag = ToolbarDrag::new((500., 500.), (100., 500.), None, 0, 1., (4., 4.));
        let released = drag.finish((5000., 2000.), 1, follow, |monitor, position| {
            let layout = ToolbarLayout::new_capture_with_tools(
                screens[monitor],
                crop,
                1.,
                Some(position),
                Tool::Rectangle,
                &slots,
            );
            for panel in [layout.primary, layout.properties] {
                assert!(panel.x >= screens[monitor].x as f32);
                assert!(panel.right() <= (screens[monitor].x + screens[monitor].width) as f32);
                assert!(panel.y >= screens[monitor].y as f32);
                assert!(panel.bottom() <= (screens[monitor].y + screens[monitor].height) as f32);
            }
            layout.primary
        });
        assert_eq!(released.monitor, 1);
        let position = released.position.unwrap();
        assert!(position.0 >= screens[1].x as f32);
        assert!(position.0 < 3200.);
        assert!(position.1 < screens[1].height as f32);
    }

    #[test]
    fn other_monitor_never_reattaches_and_cancel_restores_original_state() {
        let initial = Some((100., 500.));
        let mut drag = ToolbarDrag::new((1000., 1000.), (100., 500.), initial, 2, 1., (4., 4.));
        let update = drag.update((1030., 1000.), 1, target(0, 130., 500.), unclamped);
        assert!(update.snap_preview.is_none());
        let cancelled = drag.cancel();
        assert_eq!(cancelled.position, initial);
        assert_eq!(cancelled.monitor, 2);
        assert!(!cancelled.dragging);
    }
}
