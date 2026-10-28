//! 选区条共用的弹窗放置与鼠标模式切换；几何使用窗口逻辑坐标。
use crate::{annotation::Tool, toolbar_layout::PanelRect};

const AVOID_GAP: f32 = 8.;

/// 单个鼠标按钮复用既有工具模式；手势/输入的提交门仍由 CaptureView/Editor 处理。
pub(crate) fn mouse_mode_tool(current: Tool) -> Tool {
    if current == Tool::Edit {
        Tool::Crop
    } else {
        Tool::Edit
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SelectionActionLayout {
    pub panel: PanelRect,
    monitor: PanelRect,
}

impl SelectionActionLayout {
    /// 新尺寸/外观条复用弹窗放置；两个矩形均已是窗口 DIP，不再次投影或缩放。
    pub(crate) fn anchored(panel: PanelRect, monitor: PanelRect) -> Self {
        Self { panel, monitor }
    }

    /// 弹窗与选区条使用同一显示器和 DPI，优先放右侧，空间不足时放左侧。
    pub fn popup(self, width: f32, height: f32) -> PanelRect {
        let bounded = |value: f32, maximum: f32| {
            if value.is_finite() {
                value.clamp(0., maximum)
            } else {
                maximum
            }
        };
        let width = bounded(width, self.monitor.width);
        let height = bounded(height, self.monitor.height);
        let right = self.panel.right() + AVOID_GAP;
        let left = self.panel.x - width - AVOID_GAP;
        let x = if right + width <= self.monitor.right() {
            right
        } else if left >= self.monitor.x {
            left
        } else {
            right.clamp(
                self.monitor.x,
                (self.monitor.right() - width).max(self.monitor.x),
            )
        };
        let y = (self.panel.y + self.panel.height / 2. - height / 2.).clamp(
            self.monitor.y,
            (self.monitor.bottom() - height).max(self.monitor.y),
        );
        PanelRect {
            x,
            y,
            width,
            height,
        }
    }

    /// 与面板绘制共用逻辑坐标和右/下开区间，不接受物理像素或桌面全局坐标。
    #[cfg(test)]
    pub fn contains(self, point: (f32, f32)) -> bool {
        point.0 >= self.panel.x
            && point.1 >= self.panel.y
            && point.0 < self.panel.right()
            && point.1 < self.panel.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        editor::Editor,
        geometry::{Rect, ScreenRect},
    };

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() < 0.01, "{a} != {b}");
    }

    fn inside(panel: PanelRect, monitor: Rect, scale: f32) {
        assert!(panel.x >= monitor.x as f32 / scale - 0.01);
        assert!(panel.y >= monitor.y as f32 / scale - 0.01);
        assert!(panel.right() <= (monitor.x as f32 + monitor.width as f32) / scale + 0.01);
        assert!(panel.bottom() <= (monitor.y as f32 + monitor.height as f32) / scale + 0.01);
        assert!(panel.width >= 0. && panel.height >= 0.);
    }

    fn crop(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn anchored_panel_hit_testing_uses_dip_and_excludes_right_and_bottom_edges() {
        let monitor = PanelRect::from_physical(Rect::full(1000, 800), 1.);
        let panel = PanelRect {
            x: 100.,
            y: 200.,
            width: 300.,
            height: 34.,
        };
        let layout = SelectionActionLayout::anchored(panel, monitor);
        assert!(layout.contains((100., 200.)));
        assert!(layout.contains((399.9, 233.9)));
        assert!(!layout.contains((400., 220.)));
        assert!(!layout.contains((200., 234.)));
        assert!(!layout.contains((f32::NAN, 220.)));
    }

    #[test]
    fn popup_screen_edges_choose_right_left_or_clamped_monitor_and_limit_size() {
        let monitor = Rect::full(1000, 800);
        for (panel, x, y) in [
            (crop(100, 200, 200, 34), 308., 101.),
            (crop(800, 0, 200, 34), 492., 0.),
            (crop(800, 766, 200, 34), 492., 568.),
        ] {
            let popup = SelectionActionLayout::anchored(
                PanelRect::from_physical(panel, 1.),
                PanelRect::from_physical(monitor, 1.),
            )
            .popup(300., 232.);
            close(popup.x, x);
            close(popup.y, y);
            inside(popup, monitor, 1.);
        }
        let small = Rect::full(500, 300);
        let popup = SelectionActionLayout::anchored(
            PanelRect::from_physical(crop(150, 100, 100, 34), 1.),
            PanelRect::from_physical(small, 1.),
        )
        .popup(440., 232.);
        close(popup.x, 60.);
        inside(popup, small, 1.);
        let popup = SelectionActionLayout::anchored(
            PanelRect::from_physical(crop(0, 0, 1000, 34), 1.),
            PanelRect::from_physical(monitor, 1.),
        )
        .popup(1200., 1000.);
        close(popup.x, 0.);
        close(popup.y, 0.);
        close(popup.width, 1000.);
        close(popup.height, 800.);
    }

    #[test]
    fn popup_high_dpi_uses_same_local_monitor_and_bar_center_without_rescaling_content() {
        let desktop = ScreenRect {
            left: -1920,
            top: -240,
            width: 4480,
            height: 1680,
        };
        let monitor = ScreenRect {
            left: 0,
            top: 0,
            width: 2560,
            height: 1440,
        }
        .local(desktop);
        let selected = ScreenRect {
            left: -100,
            top: 160,
            width: 700,
            height: 350,
        }
        .local(desktop);
        for scale in [1., 1.25, 2.5, 4.] {
            let layout = SelectionActionLayout::anchored(
                PanelRect {
                    height: 34.,
                    ..PanelRect::from_physical(selected, scale)
                },
                PanelRect::from_physical(monitor, scale),
            );
            let popup = layout.popup(300., 232.);
            close(popup.x, layout.panel.right() + AVOID_GAP);
            close(
                popup.y,
                (layout.panel.y + layout.panel.height / 2. - 116.).max(monitor.y as f32 / scale),
            );
            close(popup.width, 300.);
            close(popup.height, 232.);
            inside(popup, monitor, scale);
        }
    }

    fn editor_with_redo() -> (Editor, Vec<crate::document::Object>) {
        let mut editor = Editor::new(Rect::full(400, 300), Some(crop(100, 100, 200, 150)));
        for (tool, start, end) in [
            (Tool::Rectangle, (120, 120), (190, 190)),
            (Tool::Ellipse, (240, 130), (280, 180)),
        ] {
            editor.select_tool(tool);
            editor.pointer_down(start, 6.);
            editor.pointer_move(end);
            editor.pointer_up(end);
        }
        let complete = editor.document.items().to_vec();
        editor.undo();
        assert_eq!(editor.document.items().len(), 1);
        assert!(editor.document.can_undo() && editor.document.can_redo());
        (editor, complete)
    }

    #[test]
    fn mouse_mode_switches_preserve_crop_canvas_objects_and_both_history_directions() {
        let (mut editor, complete) = editor_with_redo();
        let canvas = editor.canvas;
        let crop = editor.crop;
        let objects = editor.document.items().to_vec();
        let revision = editor.document.edit_revision();
        for expected in [Tool::Edit, Tool::Crop, Tool::Edit] {
            editor.select_tool(mouse_mode_tool(editor.tool));
            assert_eq!(editor.tool, expected);
            assert_eq!(editor.canvas, canvas);
            assert_eq!(editor.crop, crop);
            assert_eq!(editor.document.items(), objects);
            assert_eq!(editor.document.edit_revision(), revision);
            assert!(editor.document.can_undo() && editor.document.can_redo());
        }
        editor.redo();
        assert_eq!(editor.document.items(), complete);
        editor.undo();
        assert_eq!(editor.document.items(), objects);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
        editor.redo();
        editor.redo();
        assert_eq!(editor.document.items(), complete);
        assert_eq!(editor.canvas, canvas);
        assert_eq!(editor.crop, crop);
    }

    #[test]
    fn cancelled_object_and_crop_gestures_then_mouse_switch_do_not_commit_an_increment() {
        let (mut editor, complete) = editor_with_redo();
        editor.select_tool(Tool::Edit);
        let canvas = editor.canvas;
        let crop = editor.crop;
        let objects = editor.document.items().to_vec();
        let revision = editor.document.edit_revision();
        editor.pointer_down((150, 150), 6.);
        editor.pointer_move((175, 160));
        assert!(editor.is_dragging());
        assert!(editor.cancel_gesture());
        editor.select_tool(mouse_mode_tool(editor.tool));
        assert_eq!(editor.tool, Tool::Crop);
        editor.pointer_down((210, 220), 6.);
        editor.pointer_move((250, 240));
        assert!(editor.is_changing_crop());
        assert_ne!(editor.crop, crop);
        assert!(editor.cancel_gesture());
        editor.select_tool(mouse_mode_tool(editor.tool));
        assert_eq!(editor.tool, Tool::Edit);
        assert!(!editor.is_dragging());
        assert_eq!(editor.canvas, canvas);
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.items(), objects);
        assert_eq!(editor.document.edit_revision(), revision);
        assert!(editor.document.can_undo() && editor.document.can_redo());
        editor.redo();
        assert_eq!(editor.document.items(), complete);
        editor.undo();
        assert_eq!(editor.document.items(), objects);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
    }
}
