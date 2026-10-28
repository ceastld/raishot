//! GPUI 输入适配：恢复 Windows 滚轮刻度；光标图形不进入截图场景。
use crate::{
    annotation::Tool,
    capture_view::{Action, CaptureView},
    editor::Editor,
    interaction::{Hover, Parameter},
    toolbar_layout::{PanelRect, ToolbarPanel},
};
use gpui::{prelude::*, Context, CursorStyle, ScrollDelta, ScrollWheelEvent, Window};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// 普通截图专用的最终右键路线；调用前先处理工具、草稿与其它宿主的右键合同。
pub(crate) fn screenshot_right_click_action(editor: &Editor, point: (i32, i32)) -> Option<Action> {
    match editor.crop {
        None => Some(Action::Cancel),
        Some(crop)
            if crop.valid(editor.canvas.width, editor.canvas.height)
                && crop.contains(point)
                && !matches!(editor.tool, Tool::Crop | Tool::Edit) =>
        {
            None
        }
        Some(_) => Some(Action::Reselect),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScreenshotToolbarMenuCommand {
    Show,
    Reselect,
    Cancel,
}

pub(crate) enum ScreenshotToolbarMenuKey {
    Consumed,
    Command(ScreenshotToolbarMenuCommand),
}

#[derive(Clone, Copy)]
struct ScreenshotToolbarMenuPresentation {
    crop: crate::geometry::Rect,
    tool: Tool,
    rect: PanelRect,
    selected: u8,
}

/// 普通截图窗口内的临时菜单；不拥有窗口、焦点、选区或 Editor 手势。
#[derive(Default)]
pub(crate) struct ScreenshotToolbarMenu {
    presentation: Option<ScreenshotToolbarMenuPresentation>,
}

pub(crate) fn screenshot_toolbar_menu_ready(
    editor: &Editor,
    point: (i32, i32),
    ordinary: bool,
    toolbar_shown: bool,
    busy: bool,
) -> bool {
    ordinary
        && !toolbar_shown
        && !busy
        && screenshot_toolbar_menu_editor_idle(editor)
        && editor.crop.is_some_and(|crop| {
            crop.valid(editor.canvas.width, editor.canvas.height) && crop.contains(point)
        })
}

fn screenshot_toolbar_menu_editor_idle(editor: &Editor) -> bool {
    matches!(editor.tool, Tool::Crop | Tool::Edit)
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
        && editor.local_fill_candidate().is_none()
}

/// source.screen_rect 已是虚拟桌面内物理坐标；与同窗口 DIP viewport 求可见交集。
pub(crate) fn screenshot_toolbar_menu_bounds(
    screen: crate::geometry::Rect,
    viewport: (f32, f32),
    scale: f32,
) -> Option<PanelRect> {
    if ![viewport.0, viewport.1, scale]
        .into_iter()
        .all(f32::is_finite)
        || viewport.0 <= 0.
        || viewport.1 <= 0.
        || scale <= 0.
        || screen.width == 0
        || screen.height == 0
    {
        return None;
    }
    let x = screen.x as f32 / scale;
    let y = screen.y as f32 / scale;
    let right = (screen.x.checked_add(screen.width)? as f32 / scale).min(viewport.0);
    let bottom = (screen.y.checked_add(screen.height)? as f32 / scale).min(viewport.1);
    ([x, y, right, bottom].into_iter().all(f32::is_finite) && right > x && bottom > y).then_some(
        PanelRect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        },
    )
}

impl ScreenshotToolbarMenu {
    pub(crate) fn open(&mut self, editor: &Editor, anchor: (f32, f32), visible: PanelRect) -> bool {
        if ![
            anchor.0,
            anchor.1,
            visible.x,
            visible.y,
            visible.width,
            visible.height,
            visible.right(),
            visible.bottom(),
        ]
        .into_iter()
        .all(f32::is_finite)
            || visible.x < 0.
            || visible.y < 0.
            || visible.width <= 0.
            || visible.height <= 0.
            || visible.right() <= visible.x
            || visible.bottom() <= visible.y
        {
            return false;
        }
        let Some(crop) = editor.crop.filter(|crop| {
            crop.valid(editor.canvas.width, editor.canvas.height)
                && screenshot_toolbar_menu_editor_idle(editor)
        }) else {
            return false;
        };
        let width = 180_f32.min(visible.width);
        let height = 108_f32.min(visible.height);
        self.presentation = Some(ScreenshotToolbarMenuPresentation {
            crop,
            tool: editor.tool,
            rect: PanelRect {
                x: anchor
                    .0
                    .clamp(visible.x, (visible.right() - width).max(visible.x)),
                y: anchor
                    .1
                    .clamp(visible.y, (visible.bottom() - height).max(visible.y)),
                width,
                height,
            },
            selected: 0,
        });
        true
    }

    pub(crate) fn dismiss(&mut self) -> bool {
        self.presentation.take().is_some()
    }

    pub(crate) fn observe(
        &mut self,
        editor: &Editor,
        ordinary: bool,
        toolbar_shown: bool,
        busy: bool,
    ) -> bool {
        let live = self.presentation.is_some_and(|menu| {
            ordinary
                && !toolbar_shown
                && !busy
                && screenshot_toolbar_menu_editor_idle(editor)
                && editor.crop == Some(menu.crop)
                && editor.tool == menu.tool
                && menu.crop.valid(editor.canvas.width, editor.canvas.height)
        });
        if !live {
            self.dismiss();
        }
        live
    }

    pub(crate) fn presentation(&self) -> Option<(PanelRect, u8)> {
        self.presentation.map(|menu| (menu.rect, menu.selected))
    }

    pub(crate) fn select(&mut self, index: u8) -> bool {
        let Some(menu) = self.presentation.as_mut() else {
            return false;
        };
        if index >= 3 || menu.selected == index {
            return false;
        }
        menu.selected = index;
        true
    }

    pub(crate) fn click(
        &self,
        event: &gpui::ClickEvent,
        command: ScreenshotToolbarMenuCommand,
    ) -> Option<ScreenshotToolbarMenuCommand> {
        // backend 的 Click 可能由其它按钮 Up 触发；菜单只接受真实左键的完整配对。
        (self.presentation.is_some()
            && matches!(event, gpui::ClickEvent::Mouse(event)
                if event.down.button == gpui::MouseButton::Left
                    && event.up.button == gpui::MouseButton::Left))
        .then_some(command)
    }

    pub(crate) fn key(&mut self, event: &gpui::KeyDownEvent) -> Option<ScreenshotToolbarMenuKey> {
        let menu = self.presentation.as_mut()?;
        let plain = !event.keystroke.modifiers.modified();
        match event.keystroke.key.as_str() {
            "space" if plain && !event.is_held => Some(ScreenshotToolbarMenuKey::Command(
                ScreenshotToolbarMenuCommand::Show,
            )),
            "enter" if plain && !event.is_held => {
                Some(ScreenshotToolbarMenuKey::Command(match menu.selected {
                    1 => ScreenshotToolbarMenuCommand::Reselect,
                    2 => ScreenshotToolbarMenuCommand::Cancel,
                    _ => ScreenshotToolbarMenuCommand::Show,
                }))
            }
            "up" | "down" => {
                if plain {
                    menu.selected =
                        (menu.selected + if event.keystroke.key == "up" { 2 } else { 1 }) % 3;
                }
                Some(ScreenshotToolbarMenuKey::Consumed)
            }
            "enter" | "space" => Some(ScreenshotToolbarMenuKey::Consumed),
            "left" | "right" => Some(ScreenshotToolbarMenuKey::Consumed),
            _ => {
                // Esc 保留截图原取消；其它快捷键回原路，均先撤临时菜单。
                self.dismiss();
                None
            }
        }
    }
}

/// 内侧命中带与宿主、裸修饰键资格；Down 与 hover 共用同一纯入口。
pub(crate) fn screenshot_empty_crop_inside_target(
    editor: &Editor,
    point: (i32, i32),
    tolerance: f32,
    modifiers: gpui::Modifiers,
    host_ready: bool,
) -> Option<u8> {
    if !host_ready || modifiers.modified() {
        return None;
    }
    editor.screenshot_empty_crop_inside_edges(point, tolerance)
}

pub fn wheel_settings() -> (f32, f32) {
    let (mut lines, mut chars) = (3_u32, 3_u32);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&mut lines as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let _ = SystemParametersInfoW(
            SPI_GETWHEELSCROLLCHARS,
            0,
            Some((&mut chars as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    (lines.max(1) as f32, chars.max(1) as f32)
}
pub(crate) fn notches(event: &ScrollWheelEvent, settings: (f32, f32)) -> f32 {
    match event.delta {
        ScrollDelta::Lines(delta) if event.modifiers.shift && delta.y == 0. => delta.x / settings.1,
        ScrollDelta::Lines(delta) => delta.y / settings.0,
        ScrollDelta::Pixels(delta) => {
            let y = if event.modifiers.shift && f32::from(delta.y) == 0. {
                f32::from(delta.x)
            } else {
                f32::from(delta.y)
            };
            y / 40.
        }
    }
}

fn control_preview_wheel_notches(
    has_crop: bool,
    event: &ScrollWheelEvent,
    settings: (f32, f32),
) -> Option<f32> {
    // 预览阶段的滚轮包含 Ctrl/Shift；确认范围后继续走原参数滚轮。
    (!has_crop).then(|| notches(event, settings))
}

impl CaptureView {
    pub(crate) fn screenshot_toolbar_menu_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let (rect, selected) = self.screenshot_toolbar_menu.presentation()?;
        let mut menu = self
            .panel("screenshot-hidden-toolbar-menu", rect)
            .flex()
            .flex_col()
            .p_1()
            .gap_1()
            .overflow_hidden()
            .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                v.screenshot_toolbar_menu.dismiss();
                v.invalidate_click_sequence();
                v.wheel_input.reset();
                cx.stop_propagation();
                cx.notify();
            }))
            .on_mouse_up(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(gpui::MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation());
        for (index, id, label, command) in [
            (
                0,
                "screenshot-menu-show",
                tr!("显示工具栏 · Space", "Show toolbar · Space"),
                ScreenshotToolbarMenuCommand::Show,
            ),
            (
                1,
                "screenshot-menu-reselect",
                tr!("重新选择", "Reselect"),
                ScreenshotToolbarMenuCommand::Reselect,
            ),
            (
                2,
                "screenshot-menu-cancel",
                tr!("取消截图 · Esc", "Cancel capture · Esc"),
                ScreenshotToolbarMenuCommand::Cancel,
            ),
        ] {
            menu = menu.child(
                self.chip(id, label, selected == index)
                    .w_full()
                    .on_mouse_move(cx.listener(move |v, _, _, cx| {
                        if v.screenshot_toolbar_menu.select(index) {
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }))
                    .on_click(cx.listener(move |v, event, w, cx| {
                        if let Some(command) = v.screenshot_toolbar_menu.click(event, command) {
                            v.screenshot_toolbar_menu_command(command, w, cx);
                        }
                        cx.stop_propagation();
                    })),
            );
        }
        Some(menu.into_any_element())
    }
    pub(crate) fn wheel(
        &mut self,
        e: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refresh_request.is_some() {
            self.wheel_input.reset();
            cx.stop_propagation();
            return;
        }
        self.invalidate_click_sequence();
        if self.screenshot_toolbar_menu.dismiss() {
            self.wheel_input.reset();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.long_clip_wheel(e, window, cx) {
            self.wheel_input.reset();
            return;
        }
        if self.pin_mode {
            if !self.pin_editing
                || e.modifiers.control
                || e.modifiers.alt
                || (self.editor.tool == crate::annotation::Tool::Edit
                    && self.editor.selected_ids().is_empty()
                    && self.editor.text_draft.is_none()
                    && !self.editor.is_dragging())
            {
                return;
            }
            cx.stop_propagation();
        }

        // 结果层保留时图片仍可缩放；此专用门不放开底层参数滚轮。
        if !self.pin_mode
            && self.image_canvas.is_some()
            && self.recognition_result.is_some()
            && (e.modifiers.alt
                || (self.editor.tool == Tool::Edit
                    && self.editor.selected_ids().is_empty()
                    && self.editor.text_draft.is_none()))
        {
            cx.stop_propagation();
            self.wheel_input.reset();
            if !self.saving
                && !self.processing
                && !self.pin_handoff_pending
                && self.toolbar_panel == ToolbarPanel::None
                && self.toolbar_drag.is_none()
                && self.flyout.open().is_none()
                && self.result_editor.is_none()
                && self.input.marked.is_none()
                && !self.text_composing()
                && !self.watermark_input_focused
                && self.inline_ocr_interaction_idle()
                && !self.editor.is_dragging()
                && !self.editor.has_pending_drawing()
                && self.editor.text_draft.is_none()
                && !self.ocr_overlay_dragging()
                && self.pointer_down_start.is_none()
                && self.input.drag_anchor.is_none()
                && window.is_window_active()
                && self.focus.is_focused(window)
                && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() }
                    .0
                    .is_null()
            {
                let zoom = self
                    .image_canvas
                    .as_ref()
                    .expect("图片宿主已核验")
                    .viewport
                    .zoom
                    * 1.1_f32.powf(notches(e, self.wheel_settings));
                self.zoom_image(
                    zoom,
                    crate::image_editor::client_point(e.position, window),
                    cx,
                );
            }
            return;
        }
        if self.saving
            || self.pin_handoff_pending
            || self.processing
            || self.toolbar_panel != ToolbarPanel::None
            || self.toolbar_drag.is_some()
            || self.recognition_result.is_some()
        {
            if !self.pin_mode && self.image_canvas.is_none() {
                self.wheel_input.reset();
            }
            return;
        }
        self.pointer_sample = self.world_point(e.position, window);
        self.pointer = self.physical(e.position, window);
        self.invalidate_control_hover();
        let control_notches =
            control_preview_wheel_notches(self.editor.crop.is_some(), e, self.wheel_settings);
        if !self.pin_mode && self.image_canvas.is_none() && control_notches.is_some() {
            // 未消费的 Preview 尝试仍要退出中心，但原参数 fallback 半刻度必须保留。
            self.wheel_input.reset_selection_center();
        }
        if control_notches.is_some_and(|notches| self.navigate_control_selection(notches, cx)) {
            self.wheel_input.reset();
            cx.stop_propagation();
            return;
        }
        if !self.pin_mode
            && self.image_canvas.is_some()
            && (e.modifiers.alt
                || (self.editor.tool == crate::annotation::Tool::Edit
                    && self.editor.selected_ids().is_empty()
                    && self.editor.text_draft.is_none()))
        {
            if !self.editor.is_dragging()
                && !self.editor.has_pending_drawing()
                && self.image_canvas.as_ref().is_none_or(|s| s.pan.is_none())
            {
                let zoom = self.image_canvas.as_ref().unwrap().viewport.zoom
                    * 1.1_f32.powf(notches(e, self.wheel_settings));
                self.zoom_image(
                    zoom,
                    crate::image_editor::client_point(e.position, window),
                    cx,
                );
            }
            cx.stop_propagation();
            return;
        }
        if !self.pin_mode && self.image_canvas.is_none() && self.editor.crop.is_some() {
            let route = crate::selection_wheel::confirmed_route(
                &self.editor,
                self.pointer,
                e.modifiers.shift,
                e.modifiers.control,
                self.pointer_down_start.is_some() || self.toolbar_drag.is_some(),
                self.watermark_input_focused
                    || self.input.marked.is_some()
                    || self.text_composing(),
            );
            match route {
                Some(crate::selection_wheel::ConfirmedWheelRoute::Parameter(target)) => {
                    // 参数映射就是消费：半刻度和到限均不得继续扩选。
                    self.wheel_parameter(target, e, window, cx);
                }
                Some(crate::selection_wheel::ConfirmedWheelRoute::Center) => {
                    let (steps, _) = self.wheel_input.push_selection_center(
                        notches(e, self.wheel_settings),
                        std::time::Instant::now(),
                    );
                    if self.editor.adjust_crop_from_center(steps) {
                        self.smart_candidate_sync(window, cx);
                        cx.notify();
                    }
                    cx.stop_propagation();
                }
                None => {
                    self.wheel_input.reset();
                    cx.stop_propagation();
                }
            }
            return;
        }
        if let Some(target) = self
            .editor
            .wheel_parameter(e.modifiers.shift, e.modifiers.control)
        {
            self.wheel_parameter(target, e, window, cx);
        }
    }
    pub(crate) fn wheel_parameter(
        &mut self,
        target: Parameter,
        e: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_click_sequence();
        if self.saving
            || self.processing
            || (self.toolbar_panel != ToolbarPanel::None
                && !(matches!(
                    self.toolbar_panel,
                    ToolbarPanel::TextOutlineStyle | ToolbarPanel::TextBackgroundStyle
                ) && matches!(
                        target,
                        Parameter::TextOutline
                            | Parameter::TextBackgroundAlpha
                            | Parameter::TextBackgroundRadius
                    )))
        {
            return;
        }
        let (steps, group) = self.wheel_input.push(
            target,
            notches(e, self.wheel_settings),
            std::time::Instant::now(),
        );
        if self.smart_mode() {
            let (effect, style) = self.editor.effect_context();
            let style = target.adjusted(style, effect, steps);
            self.apply_smart_mosaic_style(style, cx);
            cx.stop_propagation();
            return;
        }
        if self.editor.adjust_parameter(
            target,
            steps,
            group,
            self.physical(e.position, window),
            6. * self.canvas_scale(window),
        ) {
            cx.notify();
        }
        cx.stop_propagation();
    }
    fn screenshot_crop_host_ready(&self) -> bool {
        !(self.pin_mode
            || self.image_canvas.is_some()
            || self.saving
            || self.processing
            || self.pin_handoff_pending
            || self.pointer_down_start.is_some()
            || self.toolbar_drag.is_some()
            || self.toolbar_panel != ToolbarPanel::None
            || self.recognition_result.is_some()
            || self.watermark_input_focused
            || self.input.marked.is_some()
            || self.text_composing())
    }
    /// Down 与 hover 共用截图宿主门；活动手势仍由 Editor 原有 hover 固定方向。
    pub(crate) fn screenshot_crop_outside_target(&self, tolerance: f32) -> Option<u8> {
        if !self.screenshot_crop_host_ready()
            || (matches!(self.editor.tool, Tool::Lasso | Tool::SmartMosaic)
                && (self.manual_preview.is_some() || !self.crop_adjust_cursor_down_ready()))
        {
            return None;
        }
        self.editor
            .screenshot_crop_outside_edges(self.pointer, tolerance)
    }
    pub(crate) fn screenshot_empty_crop_inside_target(
        &self,
        tolerance: f32,
        modifiers: gpui::Modifiers,
    ) -> Option<u8> {
        screenshot_empty_crop_inside_target(
            &self.editor,
            self.pointer,
            tolerance,
            modifiers,
            self.screenshot_crop_host_ready()
                && self.screenshot_toolbar_menu.presentation().is_none(),
        )
    }
    pub(crate) fn screenshot_empty_crop_center_target(
        &self,
        tolerance: f32,
        modifiers: gpui::Modifiers,
    ) -> bool {
        self.screenshot_crop_host_ready()
            && !modifiers.modified()
            && self
                .editor
                .screenshot_empty_crop_center_ready(self.pointer, tolerance)
    }
    pub(crate) fn canvas_hover(&self, tolerance: f32, modifiers: gpui::Modifiers) -> Hover {
        if let Some(edges) = self.screenshot_crop_outside_target(tolerance) {
            return Hover::edges(edges);
        }
        if let Some(edges) = self.screenshot_empty_crop_inside_target(tolerance, modifiers) {
            return Hover::edges(edges);
        }
        if self.screenshot_empty_crop_center_target(tolerance, modifiers) {
            return Hover::Move;
        }
        if self.smart_mode() && !self.editor.is_changing_crop() {
            return Hover::Crosshair;
        }
        if self.image_canvas.is_some() {
            self.editor.hover_at(self.pointer_sample, tolerance)
        } else {
            self.editor.hover(self.pointer, tolerance)
        }
    }
    pub(crate) fn canvas_cursor(
        &self,
        scale: f32,
        ui_scale: f32,
        modifiers: gpui::Modifiers,
    ) -> CursorStyle {
        if self.screenshot_toolbar_menu.presentation().is_some() {
            return CursorStyle::Arrow;
        }
        if self.image_canvas.is_some()
            && !(crate::annotation::Bounds {
                x: 0.,
                y: 0.,
                width: self.source.bounds.width as f32,
                height: self.source.bounds.height as f32,
            })
            .contains(self.pointer_sample, 0.)
        {
            return CursorStyle::Arrow;
        }
        if self.toolbar_panel != ToolbarPanel::None || self.recognition_result.is_some() {
            return CursorStyle::Arrow;
        }
        // 全部使用系统光标；笔刷尺寸圈也生成为原生光标，由系统绘制而不在画布上跟随指针。
        match self.canvas_hover(6. * scale, modifiers) {
            Hover::Arrow => CursorStyle::Arrow,
            Hover::Crosshair
                if self.editor.crop.is_some()
                    && (self.editor.tool.freehand()
                        || self.editor.tool == crate::annotation::Tool::Eraser) =>
            {
                crate::brush_cursor::brush_cursor(self.editor.style.width / scale * ui_scale)
            }
            Hover::Crosshair => CursorStyle::Crosshair,
            Hover::Text => CursorStyle::IBeam,
            Hover::Horizontal => CursorStyle::ResizeLeftRight,
            Hover::Vertical => CursorStyle::ResizeUpDown,
            Hover::DiagonalDown => CursorStyle::ResizeUpLeftDownRight,
            Hover::DiagonalUp => CursorStyle::ResizeUpRightDownLeft,
            Hover::Move => CursorStyle::ResizeAll,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};
    #[test]
    fn windows_scroll_settings_and_shift_axis_preserve_notches() {
        let mut event = ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(0., 6.)),
            ..Default::default()
        };
        assert_eq!(notches(&event, (3., 4.)), 2.);
        event.modifiers.shift = true;
        event.delta = ScrollDelta::Lines(point(4., 0.));
        assert_eq!(notches(&event, (3., 4.)), 1.);
        event.modifiers.shift = false;
        assert_eq!(notches(&event, (3., 4.)), 0.);
        event.delta = ScrollDelta::Pixels(point(px(0.), px(-20.)));
        assert_eq!(notches(&event, (3., 4.)), -0.5);
    }

    #[test]
    fn control_preview_wheel_modifiers_navigate_but_confirmed_crop_keeps_parameters() {
        let mut event = ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(0., 1.5)),
            ..Default::default()
        };
        for (control, shift) in [(false, false), (true, false), (false, true), (true, true)] {
            event.modifiers.control = control;
            event.modifiers.shift = shift;
            assert_eq!(
                control_preview_wheel_notches(false, &event, (3., 4.)),
                Some(0.5)
            );
            assert_eq!(control_preview_wheel_notches(true, &event, (3., 4.)), None);
        }
        event.modifiers.shift = true;
        event.delta = ScrollDelta::Lines(point(-2., 0.));
        assert_eq!(
            control_preview_wheel_notches(false, &event, (3., 4.)),
            Some(-0.5)
        );
        event.modifiers.shift = false;
        assert_eq!(
            control_preview_wheel_notches(false, &event, (3., 4.)),
            Some(0.)
        );
    }
}

#[cfg(test)]
mod screenshot_right_click_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style},
        crop_mask::Polygon,
        geometry::Rect,
    };

    fn object(start: (i32, i32), end: (i32, i32)) -> Annotation {
        let mut object =
            Annotation::new(Tool::Rectangle, Point::from_tuple(start), Style::default());
        object.update(Point::from_tuple(end));
        object
    }

    fn editor(tool: Tool) -> Editor {
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        editor.document.set_crop_mask(Polygon::new(&[
            Point { x: 100., y: 80. },
            Point { x: 220., y: 80. },
            Point { x: 220., y: 180. },
            Point { x: 100., y: 180. },
        ]));
        editor.document.insert(object((10, 10), (30, 30)));
        editor.document.insert(object((130, 100), (160, 140)));
        editor.document.undo();
        editor.select_tool(tool);
        editor
    }

    fn dispatch(editor: &mut Editor, point: (i32, i32)) -> Option<Action> {
        let action = screenshot_right_click_action(editor, point);
        // 最终 fallback 的 Reselect 继续走生产 Editor；Cancel 的窗口关闭不在纯模型范围。
        if matches!(action, Some(Action::Reselect)) {
            editor.set_crop(None);
        }
        action
    }

    #[test]
    fn inside_drawing_tools_keep_crop_mask_style_objects_and_both_history_directions() {
        for tool in [
            Tool::Rectangle,
            Tool::Ellipse,
            Tool::Arrow,
            Tool::Pen,
            Tool::Mosaic,
            Tool::Highlighter,
            Tool::HighlighterRect,
            Tool::Polyline,
            Tool::Text,
            Tool::Watermark,
            Tool::Serial,
            Tool::Blur,
            Tool::MosaicBrush,
            Tool::BlurBrush,
            Tool::LocalFill,
            Tool::Spotlight,
            Tool::Eraser,
            Tool::PixelEraser,
        ] {
            let mut editor = editor(tool);
            let crop = editor.crop;
            let mask = editor.document.crop_mask.clone();
            let items = editor.document.items().to_vec();
            let style = editor.style;
            let stamp = editor.scene_stamp();
            let edit_revision = editor.document.edit_revision();
            assert!(editor.document.can_undo());
            assert!(editor.document.can_redo());
            assert!(dispatch(&mut editor, (180, 150)).is_none(), "{tool:?}");
            assert_eq!(editor.crop, crop);
            assert_eq!(editor.document.crop_mask, mask);
            assert_eq!(editor.tool, tool);
            assert_eq!(editor.style, style);
            assert_eq!(editor.document.items(), items);
            assert_eq!(editor.scene_stamp(), stamp);
            assert_eq!(editor.document.edit_revision(), edit_revision);
            assert!(editor.document.can_undo());
            assert!(editor.document.can_redo());
            editor.document.redo();
            assert_eq!(editor.document.items().len(), 2);
            editor.document.undo();
            assert_eq!(editor.document.items(), items);
        }
    }

    #[test]
    fn outside_right_click_reselects_without_translating_objects_or_adding_object_undo() {
        for point in [
            (99, 130),
            (220, 130),
            (160, 79),
            (160, 180),
            (-20, 130),
            (450, 320),
        ] {
            let mut editor = editor(Tool::Rectangle);
            let items = editor.document.items().to_vec();
            let revision = editor.document.edit_revision();
            assert!(matches!(
                dispatch(&mut editor, point),
                Some(Action::Reselect)
            ));
            assert!(editor.crop.is_none());
            assert!(editor.document.crop_mask.is_none());
            assert_eq!(editor.tool, Tool::Crop);
            assert_eq!(editor.document.items(), items);
            assert_eq!(editor.document.edit_revision(), revision);
            editor.undo();
            assert!(editor.document.items().is_empty());
            editor.redo();
            assert_eq!(editor.document.items(), items);
        }
    }

    #[test]
    fn move_modes_inside_keep_original_reselect_and_missing_crop_keeps_cancel() {
        for tool in [Tool::Crop, Tool::Edit] {
            let mut editor = editor(tool);
            assert!(matches!(
                dispatch(&mut editor, (160, 130)),
                Some(Action::Reselect)
            ));
            assert!(editor.crop.is_none());
            assert_eq!(editor.tool, Tool::Crop);
        }
        let mut editor = Editor::new(Rect::full(400, 300), None);
        editor.select_tool(Tool::Rectangle);
        let stamp = editor.scene_stamp();
        assert!(matches!(
            dispatch(&mut editor, (160, 130)),
            Some(Action::Cancel)
        ));
        assert_eq!(editor.tool, Tool::Rectangle);
        assert_eq!(editor.scene_stamp(), stamp);
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn exact_inside_edges_and_tiny_crop_are_kept_but_invalid_ranges_reselect_safely() {
        let editor = editor(Tool::Rectangle);
        for point in [(100, 80), (219, 80), (100, 179), (219, 179)] {
            assert!(screenshot_right_click_action(&editor, point).is_none());
        }
        let mut tiny = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 1,
                height: 1,
            }),
        );
        tiny.select_tool(Tool::Rectangle);
        assert!(screenshot_right_click_action(&tiny, (100, 80)).is_none());
        assert!(matches!(
            screenshot_right_click_action(&tiny, (101, 80)),
            Some(Action::Reselect)
        ));
        for invalid in [
            Rect::full(0, 10),
            Rect {
                x: u32::MAX,
                y: 80,
                width: 120,
                height: 100,
            },
        ] {
            tiny.crop = Some(invalid);
            assert!(matches!(
                dispatch(&mut tiny, (160, 130)),
                Some(Action::Reselect)
            ));
            assert!(tiny.crop.is_none());
        }
    }
}

#[cfg(test)]
mod screenshot_toolbar_menu_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style},
        geometry::Rect,
    };
    use gpui::{KeyDownEvent, Keystroke, Modifiers};

    fn editor() -> Editor {
        Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        )
    }

    fn key(key: &str, modifiers: Modifiers, held: bool) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                key: key.into(),
                key_char: None,
                modifiers,
            },
            is_held: held,
        }
    }

    fn open(menu: &mut ScreenshotToolbarMenu, editor: &Editor) -> bool {
        screenshot_toolbar_menu_ready(editor, (160, 130), true, false, false)
            && menu.open(
                editor,
                (160., 130.),
                screenshot_toolbar_menu_bounds(Rect::full(400, 300), (400., 300.), 1.).unwrap(),
            )
    }

    #[test]
    fn hidden_inside_move_modes_require_idle_and_keep_other_right_click_routes() {
        let mut editor = editor();
        for tool in [Tool::Crop, Tool::Edit] {
            editor.select_tool(tool);
            assert!(screenshot_toolbar_menu_ready(
                &editor,
                (100, 80),
                true,
                false,
                false
            ));
            for (point, ordinary, shown, busy) in [
                ((220, 130), true, false, false),
                ((-1, 130), true, false, false),
                ((160, 130), false, false, false),
                ((160, 130), true, true, false),
                ((160, 130), true, false, true),
            ] {
                assert!(!screenshot_toolbar_menu_ready(
                    &editor, point, ordinary, shown, busy
                ));
            }
        }
        editor.select_tool(Tool::Rectangle);
        assert!(!screenshot_toolbar_menu_ready(
            &editor,
            (160, 130),
            true,
            false,
            false
        ));
        assert!(screenshot_right_click_action(&editor, (160, 130)).is_none());
        editor.select_tool(Tool::Crop);
        editor.pointer_down((160, 130), 3.);
        assert!(editor.is_changing_crop());
        assert!(!screenshot_toolbar_menu_ready(
            &editor,
            (160, 130),
            true,
            false,
            false
        ));
        editor.cancel_gesture();
        editor.select_tool(Tool::Text);
        editor.pointer_down((140, 110), 3.);
        assert!(editor.text_draft.is_some());
        editor.tool = Tool::Crop;
        assert!(!screenshot_toolbar_menu_ready(
            &editor,
            (160, 130),
            true,
            false,
            false
        ));
        editor.finish_text();
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((140, 110), 3.);
        editor.pointer_up((140, 110));
        assert!(editor.has_pending_drawing());
        editor.tool = Tool::Edit;
        assert!(!screenshot_toolbar_menu_ready(
            &editor,
            (160, 130),
            true,
            false,
            false
        ));
    }

    #[test]
    fn menu_keys_navigate_activate_and_never_send_enter_or_arrows_to_canvas() {
        let editor = editor();
        let mut menu = ScreenshotToolbarMenu::default();
        assert!(open(&mut menu, &editor));
        assert!(matches!(
            menu.key(&key("up", Modifiers::default(), false)),
            Some(ScreenshotToolbarMenuKey::Consumed)
        ));
        assert!(matches!(
            menu.key(&key("enter", Modifiers::default(), false)),
            Some(ScreenshotToolbarMenuKey::Command(
                ScreenshotToolbarMenuCommand::Cancel
            ))
        ));
        assert!(menu.select(1));
        assert!(matches!(
            menu.key(&key("enter", Modifiers::default(), false)),
            Some(ScreenshotToolbarMenuKey::Command(
                ScreenshotToolbarMenuCommand::Reselect
            ))
        ));
        for event in [
            key("enter", Modifiers::default(), true),
            key("space", Modifiers::default(), true),
            key(
                "down",
                Modifiers {
                    shift: true,
                    ..Default::default()
                },
                false,
            ),
        ] {
            assert!(matches!(
                menu.key(&event),
                Some(ScreenshotToolbarMenuKey::Consumed)
            ));
        }
        assert_eq!(menu.presentation().unwrap().1, 1);
        for key_name in ["left", "right"] {
            assert!(matches!(
                menu.key(&key(key_name, Modifiers::default(), false)),
                Some(ScreenshotToolbarMenuKey::Consumed)
            ));
            assert_eq!(menu.presentation().unwrap().1, 1);
        }
        for (down, up) in [
            (gpui::MouseButton::Left, gpui::MouseButton::Middle),
            (gpui::MouseButton::Left, gpui::MouseButton::Right),
            (gpui::MouseButton::Right, gpui::MouseButton::Left),
        ] {
            let event = gpui::ClickEvent::Mouse(gpui::MouseClickEvent {
                down: gpui::MouseDownEvent {
                    button: down,
                    ..Default::default()
                },
                up: gpui::MouseUpEvent {
                    button: up,
                    ..Default::default()
                },
            });
            assert!(menu
                .click(&event, ScreenshotToolbarMenuCommand::Cancel)
                .is_none());
        }
        let click = gpui::ClickEvent::Mouse(gpui::MouseClickEvent {
            down: gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                ..Default::default()
            },
            up: gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
                ..Default::default()
            },
        });
        assert_eq!(
            menu.click(&click, ScreenshotToolbarMenuCommand::Show),
            Some(ScreenshotToolbarMenuCommand::Show)
        );
        assert!(menu
            .click(
                &gpui::ClickEvent::default(),
                ScreenshotToolbarMenuCommand::Cancel
            )
            .is_none());
        assert!(matches!(
            menu.key(&key("space", Modifiers::default(), false)),
            Some(ScreenshotToolbarMenuKey::Command(
                ScreenshotToolbarMenuCommand::Show
            ))
        ));
        assert!(menu
            .key(&key("escape", Modifiers::default(), false))
            .is_none());
        assert!(menu.presentation().is_none());
        assert!(menu
            .click(&click, ScreenshotToolbarMenuCommand::Show)
            .is_none());
        assert!(menu
            .key(&key("enter", Modifiers::default(), false))
            .is_none());
    }

    #[test]
    fn dismiss_and_new_crop_tool_busy_boundaries_preserve_content_and_object_history() {
        let mut editor = editor();
        let mut object =
            Annotation::new(Tool::Rectangle, Point { x: 10., y: 10. }, Style::default());
        object.update(Point { x: 40., y: 30. });
        editor.document.insert(object);
        editor
            .document
            .set_crop_mask(crate::crop_mask::Polygon::new(&[
                Point { x: 100., y: 80. },
                Point { x: 220., y: 80. },
                Point { x: 220., y: 180. },
                Point { x: 100., y: 180. },
            ]));
        let crop = editor.crop;
        let mask = editor.document.crop_mask.clone();
        let items = editor.document.items().to_vec();
        let stamp = editor.scene_stamp();
        let mut menu = ScreenshotToolbarMenu::default();
        assert!(open(&mut menu, &editor));
        assert!(menu.dismiss());
        assert!(!menu.dismiss());
        assert!(!editor.is_dragging());
        assert_eq!(editor.crop, crop);
        assert_eq!(editor.document.crop_mask, mask);
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.scene_stamp(), stamp);
        editor.undo();
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document.items(), items);
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items(), items);
        assert!(editor.document.crop_mask.is_none());
        editor.redo();
        assert_eq!(editor.document.crop_mask, mask);
        assert_eq!(editor.document.items(), items);
        assert!(open(&mut menu, &editor));
        assert!(!menu.observe(&editor, true, false, true));
        assert!(open(&mut menu, &editor));
        assert!(!menu.observe(&editor, true, true, false));
        assert!(open(&mut menu, &editor));
        assert!(!menu.observe(&editor, false, false, false));
        assert!(open(&mut menu, &editor));
        editor.crop = Some(Rect {
            x: 101,
            ..crop.unwrap()
        });
        assert!(!menu.observe(&editor, true, false, false));
        editor.crop = crop;
        assert!(open(&mut menu, &editor));
        editor.select_tool(Tool::Edit);
        assert!(!menu.observe(&editor, true, false, false));
        assert!(open(&mut menu, &editor));
        assert!(menu.key(&key("2", Modifiers::default(), false)).is_none());
        assert!(menu.presentation().is_none());
    }

    #[test]
    fn dip_menu_bounds_clamp_edges_and_reject_non_finite_viewports() {
        let editor = editor();
        let mut menu = ScreenshotToolbarMenu::default();
        for (anchor, viewport) in [
            ((-10., -20.), (400., 300.)),
            ((395., 295.), (400., 300.)),
            ((200., 200.), (80., 60.)),
        ] {
            let visible =
                screenshot_toolbar_menu_bounds(Rect::full(400, 300), viewport, 1.).unwrap();
            assert!(menu.open(&editor, anchor, visible));
            let (rect, _) = menu.presentation().unwrap();
            assert!(rect.x >= 0. && rect.y >= 0.);
            assert!(rect.right() <= viewport.0 && rect.bottom() <= viewport.1);
        }
        // 右侧屏比左侧矮：桌面原点为负，右屏在本地 x/y 均非零，其下有120px黑隙。
        let desktop = crate::geometry::ScreenRect {
            left: -1920,
            top: -240,
            width: 4480,
            height: 1440,
        };
        let screen = crate::geometry::ScreenRect {
            left: 0,
            top: 0,
            width: 2560,
            height: 1080,
        }
        .local(desktop);
        assert_eq!(
            screen,
            Rect {
                x: 1920,
                y: 240,
                width: 2560,
                height: 1080
            }
        );
        let multi_editor = Editor::new(Rect::full(desktop.width, desktop.height), Some(screen));
        for scale in [1.25, 1.5, 2.] {
            let viewport = (desktop.width as f32 / scale, desktop.height as f32 / scale);
            let visible = screenshot_toolbar_menu_bounds(screen, viewport, scale).unwrap();
            assert!(menu.open(&multi_editor, (4460. / scale, 1310. / scale), visible));
            let (rect, _) = menu.presentation().unwrap();
            assert!(rect.x >= visible.x && rect.y >= visible.y);
            assert!(rect.right() <= visible.right() + 0.001);
            assert!(rect.bottom() <= visible.bottom() + 0.001);
            assert!((rect.bottom() * scale - 1320.).abs() < 0.01);
            assert!(rect.bottom() < viewport.1); // 不落到短屏下面的虚拟桌面黑隙。
            assert!(menu.open(&multi_editor, (0., 0.), visible));
            let (rect, _) = menu.presentation().unwrap();
            assert_eq!((rect.x, rect.y), (1920. / scale, 240. / scale));
            let clipped =
                screenshot_toolbar_menu_bounds(screen, (4000. / scale, 1200. / scale), scale)
                    .unwrap();
            assert!((clipped.right() - 4000. / scale).abs() < 0.001);
            assert!((clipped.bottom() - 1200. / scale).abs() < 0.001);
            assert!(menu.open(&multi_editor, (4460. / scale, 1310. / scale), clipped));
            let (rect, _) = menu.presentation().unwrap();
            assert!(
                rect.right() <= clipped.right() + 0.001
                    && rect.bottom() <= clipped.bottom() + 0.001
            );
        }
        menu.dismiss();
        for (viewport, scale) in [
            ((f32::INFINITY, 300.), 1.),
            ((0., 300.), 1.),
            ((400., -1.), 1.),
            ((400., 300.), 0.),
            ((400., 300.), f32::NAN),
        ] {
            assert!(screenshot_toolbar_menu_bounds(screen, viewport, scale).is_none());
        }
        assert!(screenshot_toolbar_menu_bounds(
            Rect {
                x: u32::MAX,
                y: 0,
                width: 1,
                height: 1
            },
            (400., 300.),
            1.
        )
        .is_none());
        assert!(screenshot_toolbar_menu_bounds(screen, (100., 100.), 1.).is_none());
        let visible =
            screenshot_toolbar_menu_bounds(Rect::full(400, 300), (400., 300.), 1.).unwrap();
        for (anchor, bounds) in [
            ((f32::NAN, 10.), visible),
            (
                (10., 10.),
                PanelRect {
                    width: f32::INFINITY,
                    ..visible
                },
            ),
            ((10., 10.), PanelRect { x: -1., ..visible }),
            (
                (10., 10.),
                PanelRect {
                    height: 0.,
                    ..visible
                },
            ),
        ] {
            assert!(!menu.open(&editor, anchor, bounds));
            assert!(menu.presentation().is_none());
        }
    }
}
