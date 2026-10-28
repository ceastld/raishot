//! 工具栏拖动的 GPUI 事件适配；落点和吸附判定交给纯模型。
use super::CaptureView;
use crate::toolbar_drag::{SnapTarget, ToolbarDrag};
use gpui::{Pixels, Point, Window};

impl CaptureView {
    pub(crate) fn begin_toolbar_drag(
        &mut self,
        position: Point<Pixels>,
        origin: (f32, f32),
        window: &Window,
    ) {
        let scale = window.scale_factor();
        let threshold = unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDRAG, SM_CYDRAG};
            (
                GetSystemMetrics(SM_CXDRAG) as f32,
                GetSystemMetrics(SM_CYDRAG) as f32,
            )
        };
        self.toolbar_snap_preview = None;
        self.toolbar_drag = Some(ToolbarDrag::new(
            (f32::from(position.x) * scale, f32::from(position.y) * scale),
            origin,
            self.toolbar_position,
            self.toolbar_manual_monitor.unwrap_or(self.toolbar_monitor),
            scale,
            threshold,
        ));
    }

    pub(crate) fn follow_toolbar(&mut self) {
        self.toolbar_drag = None;
        self.toolbar_position = None;
        self.toolbar_manual_monitor = None;
        self.toolbar_snap_preview = None;
        self.ui_positions.borrow_mut().clear();
    }

    pub(crate) fn cancel_toolbar_drag(&mut self) {
        if let Some(drag) = self.toolbar_drag.take() {
            let original = drag.cancel();
            self.toolbar_position = original.position;
            self.toolbar_manual_monitor = original.position.map(|_| original.monitor);
        }
        self.toolbar_snap_preview = None;
    }

    pub(crate) fn move_toolbar_drag(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
        finished: bool,
    ) {
        let Some(mut drag) = self.toolbar_drag.take() else {
            return;
        };
        let scale = window.scale_factor();
        let pointer = (f32::from(position.x) * scale, f32::from(position.y) * scale);
        let selected = self.editor.crop.unwrap_or(self.source.full());
        let target = SnapTarget {
            monitor: self.toolbar_monitor,
            rect: self
                .toolbar_layout_at(selected, scale, None, self.toolbar_monitor)
                .primary,
        };
        let pointer_monitor = self.source.monitor_at(self.physical(position, window));
        let place = |monitor, origin| {
            self.toolbar_layout_at(selected, scale, Some(origin), monitor)
                .primary
        };
        let update = if finished {
            drag.finish(pointer, pointer_monitor, target, place)
        } else {
            let update = drag.update(pointer, pointer_monitor, target, place);
            self.toolbar_drag = Some(drag);
            update
        };
        self.toolbar_position = update.position;
        self.toolbar_manual_monitor = update.position.map(|_| update.monitor);
        self.toolbar_snap_preview = update.snap_preview;
        self.ui_positions.borrow_mut().clear();
    }
}
