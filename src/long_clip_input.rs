//! 长图剪辑输入。主图、剪辑轨和裁边共用原图坐标；拖动候选不提交文档。
use crate::{
    annotation::{Point, Tool},
    capture_view::{Action, CaptureView},
    gpui_capture::hwnd,
    long_clip::{LongClip, SeamSide},
    long_clip_projection::LongClipProjection,
    long_clip_ui::{ClipDrag, ClipEdge},
    stitch::ScrollAxis,
    toolbar_layout::{PanelRect, ToolbarPanel},
};
use gpui::{
    Context, KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Window,
};
use std::{ops::Range, time::Duration};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, SetCapture};

fn along(axis: ScrollAxis, p: Point) -> (f32, f32) {
    match axis {
        ScrollAxis::Vertical => (p.y, p.x),
        ScrollAxis::Horizontal => (p.x, p.y),
    }
}
fn compose(axis: ScrollAxis, long: f32, cross: f32) -> Point {
    match axis {
        ScrollAxis::Vertical => Point { x: cross, y: long },
        ScrollAxis::Horizontal => Point { x: long, y: cross },
    }
}
fn lengths(clip: &LongClip) -> (u32, u32) {
    let (w, h) = clip.source_size();
    match clip.axis() {
        ScrollAxis::Vertical => (h, w),
        ScrollAxis::Horizontal => (w, h),
    }
}
fn retained_long(clip: &LongClip) -> Range<u32> {
    let length = lengths(clip).0;
    let start = clip
        .removed()
        .first()
        .filter(|r| r.start == 0)
        .map_or(0, |r| r.end);
    let end = clip
        .removed()
        .last()
        .filter(|r| r.end == length)
        .map_or(length, |r| r.start);
    start..end
}
fn range_intersects(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start < b.end && b.start < a.end
}

fn expansion_anchor(drag: &ClipDrag, axis: ScrollAxis, source: Point) -> Point {
    // 显示末端的 After 指向原图末端；裁尾必须锚实际保留边界，
    // 否则临时展开尾段会把隐藏远端拖到指针下，使底边先跳动再恢复整段。
    match drag {
        ClipDrag::Trim {
            edge: ClipEdge::LongStart,
            long_keep,
            ..
        } => compose(axis, long_keep.start as f32, along(axis, source).1),
        ClipDrag::Trim {
            edge: ClipEdge::LongEnd,
            long_keep,
            ..
        } => compose(axis, long_keep.end as f32, along(axis, source).1),
        _ => source,
    }
}

fn cross_drag_range(mut range: Range<u32>, edge: ClipEdge, value: u32, length: u32) -> Range<u32> {
    match edge {
        ClipEdge::CrossStart => range.start = value.min(range.end.saturating_sub(1)),
        ClipEdge::CrossEnd => range.end = value.min(length).max(range.start.saturating_add(1)),
        _ => {}
    }
    range
}

#[cfg(test)]
#[path = "long_clip_input_tests.rs"]
mod tests;

impl CaptureView {
    fn long_clip_command_ready(&self) -> bool {
        self.long_clip_host_idle() && self.toolbar_panel == ToolbarPanel::None
    }

    fn long_clip_host_idle(&self) -> bool {
        self.long_clip_host_available()
            && self.editor.lasso_draft.is_none()
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
            && !self.editor.watermark.has_preview()
    }

    fn long_clip_host_available(&self) -> bool {
        self.long_clip_ui.is_some()
            && !self
                .long_clip_ui
                .as_ref()
                .is_some_and(|ui| ui.parameters_pending)
            && self.editor.long_clip().is_some()
            && self.image_canvas.is_some()
            && !self.pin_mode
            && !self.saving
            && !self.processing
            && !self.pin_handoff_pending
            && !self.color_picker_pending
            && !self.watermark_input_focused
            && self.input.marked.is_none()
            && !self.text_composing()
            && !self.result_editor_composing()
            && self.editor.text_draft.is_none()
            && self.result_editor.is_none()
            && self.recognition_result.is_none()
            && self.ocr_overlay.is_none()
            && self.toolbar_drag.is_none()
            && self.finish_radius_drag.is_none()
            && self.selection_size_input.is_none()
            && self.screenshot_toolbar_menu.presentation().is_none()
    }

    fn long_clip_input_ready(&self, window: &Window) -> bool {
        self.long_clip_command_ready() && window.is_window_active() && self.focus.is_focused(window)
    }

    fn long_clip_action_ready(&self) -> bool {
        self.long_clip_command_ready()
            && self
                .long_clip_ui
                .as_ref()
                .is_some_and(|ui| ui.drag.is_none() && ui.draft.is_none())
    }

    fn long_clip_current(&self) -> Option<&LongClip> {
        self.long_clip_ui
            .as_ref()
            .and_then(|ui| ui.draft.as_ref())
            .or_else(|| self.editor.long_clip())
    }

    fn long_clip_source_at(&self, client: Point, window: &Window) -> Option<Point> {
        let projection = self.long_clip_projection()?;
        let p = self.canvas_viewport(window).image_point(client);
        let (w, h) = projection.display_size();
        projection.display_to_source(
            Point {
                x: p.x.clamp(0., w as f32),
                y: p.y.clamp(0., h as f32),
            },
            SeamSide::After,
        )
    }

    pub(crate) fn long_clip_source_center(&self, window: &Window) -> Option<Point> {
        let area = self.long_clip_view_area(window)?;
        self.long_clip_source_at(
            Point {
                x: area.x + area.width / 2.,
                y: area.y + area.height / 2.,
            },
            window,
        )
    }

    /// 与导航、适应宽度共用编辑区，返回窗口物理像素。
    fn long_clip_view_area(&self, window: &Window) -> Option<PanelRect> {
        let clip = self.long_clip_current()?;
        let size = window.viewport_size();
        let area = crate::long_clip_navigation::editing_area(
            clip.axis(),
            f32::from(size.width),
            f32::from(size.height),
            self.long_clip_side_open(),
        );
        let dpi = window.scale_factor();
        Some(PanelRect {
            x: area.x * dpi,
            y: area.y * dpi,
            width: area.width * dpi,
            height: area.height * dpi,
        })
    }

    fn long_clip_keep_anchor(&mut self, before: &LongClipProjection, source: Point) {
        let Some(after) = self.long_clip_projection() else {
            return;
        };
        let Some(delta) = before.anchor_delta(&after, source) else {
            return;
        };
        if let Some(state) = &mut self.image_canvas {
            state.viewport.offset.x -= delta.x * state.viewport.zoom;
            state.viewport.offset.y -= delta.y * state.viewport.zoom;
            state.auto_fit = false;
        }
    }

    fn long_clip_refresh(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.invalidate_inline_ocr_projection();
        if let Err(error) = self.refresh_editor_preview(window) {
            self.status = error.to_string();
        }
        cx.notify();
    }

    fn long_clip_pointer_value(&self, source: Point, window: &Window, alt: bool) -> Option<u32> {
        let clip = self.long_clip_current()?;
        let length = lengths(clip).0;
        let raw = along(clip.axis(), source).0.clamp(0., length as f32);
        let tolerance = 8. * window.scale_factor() / self.canvas_viewport(window).zoom.max(0.0001);
        Some(
            self.long_clip_ui
                .as_ref()?
                .snap_position(raw, tolerance, alt)
                .min(length),
        )
    }

    fn long_clip_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        unsafe {
            SetCapture(hwnd(window));
        }
        self.observe_text_core_activation(window, cx);
        if let Some(ui) = &mut self.long_clip_ui {
            ui.drag_epoch = ui.drag_epoch.wrapping_add(1);
        }
        let epoch = self.long_clip_ui.as_ref().map_or(0, |ui| ui.drag_epoch);
        cx.spawn_in(window, async move |view, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let keep = view
                .update_in(cx, |view, window, cx| {
                    view.long_clip_auto_scroll(epoch, window, cx)
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    /// 返回需由宿主消费的剪辑输入；普通标注仍交给已有 Editor。
    pub(crate) fn long_clip_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.parameters_pending)
        {
            // 画布首击只让数值框失焦并尝试提交；无效草稿仍保留门禁。
            if self
                .long_clip_ui
                .as_ref()
                .and_then(|ui| ui.trim_inputs.as_ref())
                .is_some_and(|input| input.read(cx).focused(window, cx))
            {
                window.focus(&self.focus);
            }
            cx.stop_propagation();
            return true;
        }
        if !self.long_clip_input_ready(window) {
            // 长图 Crop 不得回落到 Editor 的旧选区手势。面板首击仍只收起面板。
            if self.editor.long_clip().is_some() && self.editor.tool == Tool::Crop {
                if self.screenshot_toolbar_menu.dismiss() {
                    cx.notify();
                }
                if self.toolbar_panel != ToolbarPanel::None {
                    self.toolbar_panel = ToolbarPanel::None;
                    self.selection_size_input = None;
                    window.focus(&self.focus);
                    cx.notify();
                } else if window.is_window_active() && !self.focus.is_focused(window) {
                    window.focus(&self.focus);
                }
                cx.stop_propagation();
                return true;
            }
            return false;
        }
        if self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.drag.is_some())
        {
            return true;
        }
        let client = crate::image_editor::client_point(event.position, window);
        let Some(clip) = self.long_clip_current().cloned() else {
            return false;
        };
        let Some(projection) = self.long_clip_projection() else {
            return false;
        };
        let viewport = self.canvas_viewport(window);
        let display = viewport.image_point(client);
        let (display_w, display_h) = projection.display_size();
        let inside = display.x >= 0.
            && display.y >= 0.
            && display.x <= display_w as f32
            && display.y <= display_h as f32;
        let space = self.long_clip_ui.as_ref().is_some_and(|ui| ui.space);
        if event.button == MouseButton::Middle || event.button == MouseButton::Left && space {
            if let Some(ui) = &mut self.long_clip_ui {
                ui.drag = Some(ClipDrag::Pan {
                    button: event.button,
                    start: client,
                    offset: viewport.offset,
                });
                ui.pointer = Some(client);
            }
            self.long_clip_capture(window, cx);
            cx.stop_propagation();
            return true;
        }
        if event.button != MouseButton::Left {
            return false;
        }
        let Some(source) = self.long_clip_source_at(client, window) else {
            return false;
        };
        let Some(value) = self.long_clip_pointer_value(source, window, event.modifiers.alt) else {
            return false;
        };
        let axis = clip.axis();
        let (_, cross_len) = lengths(&clip);
        let (client_long, client_cross) = along(axis, client);
        let (origin_long, origin_cross) = along(axis, viewport.offset);
        let (display_long, _) = along(
            axis,
            Point {
                x: display_w as f32,
                y: display_h as f32,
            },
        );
        let dpi = window.scale_factor();
        let in_cross = client_cross >= origin_cross
            && client_cross <= origin_cross + cross_len as f32 * viewport.zoom;
        let in_long =
            client_long >= origin_long && client_long <= origin_long + display_long * viewport.zoom;
        let track = in_long
            && client_cross >= origin_cross - crate::long_clip_chrome::GUTTER * dpi
            && client_cross <= origin_cross - crate::long_clip_chrome::GUTTER_GAP * dpi;
        let boundary_position = |value: u32| {
            let point = compose(axis, value as f32, cross_len as f32 / 2.);
            projection
                .source_to_display_nearest(point)
                .map(|p| along(axis, viewport.window_point(p)).0)
        };
        let mut drag = None;
        if in_cross || track {
            if let Some(selection) = self
                .long_clip_ui
                .as_ref()
                .and_then(|ui| ui.selection.clone())
            {
                let start_distance = boundary_position(selection.start)
                    .map_or(f32::MAX, |p| (p - client_long).abs());
                let end_distance =
                    boundary_position(selection.end).map_or(f32::MAX, |p| (p - client_long).abs());
                if start_distance.min(end_distance) <= 6. * dpi {
                    let start_edge = start_distance <= end_distance;
                    if let Some(ui) = &mut self.long_clip_ui {
                        let before_end = ui.selection_end;
                        ui.selection_end = !start_edge;
                        drag = Some(ClipDrag::SelectionEdge {
                            start: client,
                            moved: false,
                            fixed: if start_edge {
                                selection.end
                            } else {
                                selection.start
                            },
                            before_selection: Some(selection),
                            before_anchor: ui.anchor,
                            before_end,
                        });
                    }
                }
            }
            if drag.is_none() {
                let expanded = self
                    .long_clip_ui
                    .as_ref()
                    .map(|ui| ui.expanded.clone())
                    .unwrap_or_default();
                for (index, range) in clip.removed().iter().enumerate() {
                    if !expanded.contains(range) {
                        continue;
                    }
                    let start_distance = boundary_position(range.start)
                        .map_or(f32::MAX, |p| (p - client_long).abs());
                    let end_distance =
                        boundary_position(range.end).map_or(f32::MAX, |p| (p - client_long).abs());
                    if start_distance.min(end_distance) <= 6. * dpi {
                        drag = Some(ClipDrag::Adjust {
                            start: client,
                            moved: false,
                            index,
                            start_edge: start_distance <= end_distance,
                            before: clip.clone(),
                            range: range.clone(),
                            expanded,
                        });
                        break;
                    }
                }
            }
        }
        if drag.is_none() {
            let keep = retained_long(&clip);
            let cross = clip.cross_range();
            let start_long = boundary_position(keep.start).unwrap_or(origin_long);
            let end_long =
                boundary_position(keep.end).unwrap_or(origin_long + display_long * viewport.zoom);
            let start_cross = origin_cross + cross.start as f32 * viewport.zoom;
            let end_cross = origin_cross + cross.end as f32 * viewport.zoom;
            let inside_hit = if self.editor.tool == Tool::Crop {
                6. * dpi
            } else {
                0.
            };
            let outside_hit = 5. * dpi;
            // 首尾把手画在边线外 7–13 DIP，命中区覆盖整只把手；左右仍贴边，避免压到剪辑轨。
            let long_outside_hit = 14. * dpi;
            let edge = if in_cross
                && client_long >= start_long - long_outside_hit
                && client_long <= start_long + inside_hit
            {
                Some(ClipEdge::LongStart)
            } else if in_cross
                && client_long >= end_long - inside_hit
                && client_long <= end_long + long_outside_hit
            {
                Some(ClipEdge::LongEnd)
            } else if in_long
                && client_cross >= start_cross - outside_hit
                && client_cross <= start_cross + inside_hit
            {
                Some(ClipEdge::CrossStart)
            } else if in_long
                && client_cross >= end_cross - inside_hit
                && client_cross <= end_cross + outside_hit
            {
                Some(ClipEdge::CrossEnd)
            } else {
                None
            };
            if let Some(edge) = edge {
                let expanded = self
                    .long_clip_ui
                    .as_ref()
                    .map(|ui| ui.expanded.clone())
                    .unwrap_or_default();
                drag = Some(ClipDrag::Trim {
                    start: client,
                    moved: false,
                    edge,
                    before: clip.clone(),
                    long_keep: keep,
                    cross_keep: cross,
                    expanded,
                });
            }
        }
        if drag.is_none() && (track || inside && self.editor.tool == Tool::Crop) {
            let ui = self.long_clip_ui.as_ref().unwrap();
            let anchor = if event.modifiers.shift {
                ui.anchor.unwrap_or(value)
            } else {
                value
            };
            drag = Some(ClipDrag::Select {
                start: client,
                anchor,
                // Shift 单击已在 Down 建立选段，但自动滚动仍必须等实际起拖。
                moved: false,
                before_selection: ui.selection.clone(),
                before_anchor: ui.anchor,
                before_end: ui.selection_end,
            });
            if let Some(ui) = &mut self.long_clip_ui {
                ui.set_selection(anchor, value);
            }
        }
        if drag.is_none() && !inside && !track {
            drag = Some(ClipDrag::Pan {
                button: MouseButton::Left,
                start: client,
                offset: viewport.offset,
            });
        }
        let Some(drag) = drag else {
            return false;
        };
        let before_projection = projection;
        let anchor = expansion_anchor(&drag, axis, source);
        if let Some(ui) = &mut self.long_clip_ui {
            match &drag {
                ClipDrag::Trim {
                    edge: ClipEdge::LongStart,
                    ..
                } => {
                    if let Some(range) = clip.removed().first().filter(|r| r.start == 0) {
                        if !ui.expanded.contains(range) {
                            ui.expanded.push(range.clone());
                        }
                    }
                    ui.draft = Some(clip.clone());
                }
                ClipDrag::Trim {
                    edge: ClipEdge::LongEnd,
                    ..
                } => {
                    if let Some(range) = clip.removed().last().filter(|r| r.end == lengths(&clip).0)
                    {
                        if !ui.expanded.contains(range) {
                            ui.expanded.push(range.clone());
                        }
                    }
                    ui.draft = Some(clip.clone());
                }
                ClipDrag::Trim { .. } | ClipDrag::Adjust { .. } => ui.draft = Some(clip.clone()),
                _ => {}
            }
            ui.drag = Some(drag);
            ui.pointer = Some(client);
            ui.alt = event.modifiers.alt;
        }
        self.long_clip_keep_anchor(&before_projection, anchor);
        self.long_clip_capture(window, cx);
        self.long_clip_refresh(window, cx);
        cx.stop_propagation();
        true
    }

    fn long_clip_update_drag(&mut self, client: Point, alt: bool, window: &Window) {
        let Some(drag) = self.long_clip_ui.as_ref().and_then(|ui| ui.drag.clone()) else {
            return;
        };
        if let ClipDrag::Pan { start, offset, .. } = drag {
            if let Some(state) = &mut self.image_canvas {
                state.viewport.offset = Point {
                    x: offset.x + client.x - start.x,
                    y: offset.y + client.y - start.y,
                };
                state.auto_fit = false;
            }
            return;
        }
        let edge_threshold = match &drag {
            ClipDrag::SelectionEdge { start, moved, .. }
            | ClipDrag::Adjust { start, moved, .. }
            | ClipDrag::Trim { start, moved, .. } => {
                Some(*moved || client.distance(*start) >= 3. * window.scale_factor())
            }
            _ => None,
        };
        if edge_threshold == Some(false) {
            return;
        }
        if let Some(ui) = &mut self.long_clip_ui {
            match &mut ui.drag {
                Some(ClipDrag::SelectionEdge { moved, .. })
                | Some(ClipDrag::Adjust { moved, .. })
                | Some(ClipDrag::Trim { moved, .. }) => *moved = true,
                _ => {}
            }
        }
        let before_projection = self.long_clip_projection();
        let Some(source) = self.long_clip_source_at(client, window) else {
            return;
        };
        let Some(value) = self.long_clip_pointer_value(source, window, alt) else {
            return;
        };
        match drag {
            ClipDrag::Select {
                start,
                anchor,
                moved,
                ..
            } => {
                let moved = moved || client.distance(start) >= 3. * window.scale_factor();
                if let Some(ui) = &mut self.long_clip_ui {
                    if let Some(ClipDrag::Select { moved: state, .. }) = &mut ui.drag {
                        *state = moved;
                    }
                    if moved {
                        ui.set_selection(anchor, value);
                    }
                }
            }
            ClipDrag::SelectionEdge { fixed, .. } => {
                if let Some(ui) = &mut self.long_clip_ui {
                    ui.set_selection(fixed, value);
                }
            }
            ClipDrag::Adjust {
                index,
                start_edge,
                before,
                range,
                expanded,
                ..
            } => {
                let adjusted = if start_edge {
                    value.min(range.end)..range.end
                } else {
                    range.start..value.max(range.start)
                };
                let mut candidate = before;
                if candidate.adjust(index, adjusted.clone()).is_ok() {
                    if let Some(ui) = &mut self.long_clip_ui {
                        ui.expanded = expanded;
                        ui.retain_expanded(&candidate);
                        for item in candidate
                            .removed()
                            .iter()
                            .filter(|item| range_intersects(item, &adjusted))
                        {
                            if !ui.expanded.contains(item) {
                                ui.expanded.push(item.clone());
                            }
                        }
                        ui.draft = Some(candidate);
                    }
                }
            }
            ClipDrag::Trim {
                edge,
                before,
                mut long_keep,
                mut cross_keep,
                expanded,
                ..
            } => {
                let (length, cross) = lengths(&before);
                match edge {
                    ClipEdge::LongStart => {
                        long_keep.start = value.min(long_keep.end.saturating_sub(1))
                    }
                    ClipEdge::LongEnd => {
                        long_keep.end = value.max(long_keep.start.saturating_add(1))
                    }
                    ClipEdge::CrossStart | ClipEdge::CrossEnd => {
                        let value = along(before.axis(), source).1.round().max(0.) as u32;
                        cross_keep = cross_drag_range(cross_keep, edge, value, cross);
                    }
                }
                long_keep.end = long_keep.end.min(length);
                let mut candidate = before;
                if candidate.trim(long_keep, cross_keep).is_ok() {
                    if let Some(ui) = &mut self.long_clip_ui {
                        ui.expanded = expanded;
                        ui.retain_expanded(&candidate);
                        let boundary = match edge {
                            ClipEdge::LongStart => {
                                candidate.removed().first().filter(|r| r.start == 0)
                            }
                            ClipEdge::LongEnd => {
                                candidate.removed().last().filter(|r| r.end == length)
                            }
                            _ => None,
                        };
                        if let Some(range) = boundary {
                            if !ui.expanded.contains(range) {
                                ui.expanded.push(range.clone());
                            }
                        }
                        ui.draft = Some(candidate);
                    }
                }
            }
            ClipDrag::Pan { .. } => {}
        }
        if let Some(projection) = before_projection {
            self.long_clip_keep_anchor(&projection, source);
        }
    }

    /// 画布滚轮与前端稿一致：滚轮沿长轴浏览，Shift 沿跨轴，Ctrl 围绕指针缩放。
    /// 正在绘制或拖动标注时交回原生参数滚轮。
    pub(crate) fn long_clip_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.long_clip_ui.is_none()
            || self.pin_mode
            || self.editor.is_dragging()
            || self.editor.has_pending_drawing()
        {
            return false;
        }
        cx.stop_propagation();
        if self.saving || self.processing || self.pin_handoff_pending {
            return true;
        }
        if event.modifiers.control {
            let zoom = self.canvas_viewport(window).zoom
                * 1.1_f32.powf(crate::capture_interaction::notches(
                    event,
                    self.wheel_settings,
                ));
            let anchor = crate::image_editor::client_point(event.position, window);
            self.zoom_image(zoom, anchor, cx);
            return true;
        }
        let Some(projection) = self.long_clip_projection() else {
            return true;
        };
        let dpi = window.scale_factor();
        let (dx, dy) = match event.delta {
            gpui::ScrollDelta::Lines(d) => (d.x * 40. * dpi, d.y * 40. * dpi),
            gpui::ScrollDelta::Pixels(d) => (f32::from(d.x) * dpi, f32::from(d.y) * dpi),
        };
        let (wheel_long, wheel_cross) = if event.modifiers.shift {
            (0., if dx != 0. { dx } else { dy })
        } else {
            (dy, dx)
        };
        let size = window.viewport_size();
        let host = (
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        let Some(area) = self.long_clip_fit_area(host, dpi) else {
            return true;
        };
        let (display_w, display_h) = projection.display_size();
        let (delta_x, delta_y) = match projection.axis() {
            ScrollAxis::Vertical => (wheel_cross, wheel_long),
            ScrollAxis::Horizontal => (wheel_long, wheel_cross),
        };
        // 内容放得下的方向不滚动；放不下时首尾都不越过编辑区的留白。
        let clamp = |offset: f32, delta: f32, extent: f32, start: f32, length: f32| {
            if extent <= length || delta == 0. {
                offset
            } else {
                (offset + delta).clamp(start + length - extent, start)
            }
        };
        if let Some(state) = &mut self.image_canvas {
            let zoom = state.viewport.zoom;
            let offset = state.viewport.offset;
            state.viewport.offset = Point {
                x: clamp(
                    offset.x,
                    delta_x,
                    display_w as f32 * zoom,
                    area.x as f32,
                    area.width as f32,
                ),
                y: clamp(
                    offset.y,
                    delta_y,
                    display_h as f32 * zoom,
                    area.y as f32,
                    area.height as f32,
                ),
            };
            state.auto_fit = false;
        }
        self.invalidate_inline_ocr_projection();
        cx.notify();
        true
    }

    pub(crate) fn long_clip_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.long_clip_navigation_move(event, window, cx) {
            return true;
        }
        let Some(drag) = self.long_clip_ui.as_ref().and_then(|ui| ui.drag.as_ref()) else {
            return false;
        };
        let button = match drag {
            ClipDrag::Pan { button, .. } => *button,
            _ => MouseButton::Left,
        };
        if event.pressed_button != Some(button)
            || !self.long_clip_input_ready(window)
            || unsafe { GetCapture() } != hwnd(window)
        {
            self.long_clip_cancel_drag(window, cx);
            cx.stop_propagation();
            return true;
        }
        let client = crate::image_editor::client_point(event.position, window);
        if let Some(ui) = &mut self.long_clip_ui {
            ui.pointer = Some(client);
            ui.alt = event.modifiers.alt;
        }
        self.long_clip_update_drag(client, event.modifiers.alt, window);
        self.long_clip_refresh(window, cx);
        cx.stop_propagation();
        true
    }

    pub(crate) fn long_clip_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.long_clip_navigation_up(event, window, cx) {
            return true;
        }
        let Some(drag) = self.long_clip_ui.as_ref().and_then(|ui| ui.drag.clone()) else {
            return false;
        };
        let button = match drag {
            ClipDrag::Pan { button, .. } => button,
            _ => MouseButton::Left,
        };
        if event.button != button {
            return false;
        }
        // Windows 在分发真实 Up 前会释放捕获；核对保存的手势与编辑资格即可。
        if !self.long_clip_input_ready(window) {
            self.long_clip_cancel_drag(window, cx);
            cx.stop_propagation();
            return true;
        }
        let client = crate::image_editor::client_point(event.position, window);
        self.long_clip_update_drag(client, event.modifiers.alt, window);
        let projection = self.long_clip_projection();
        let anchor = self.long_clip_source_at(client, window);
        let (drag, candidate) = {
            let ui = self.long_clip_ui.as_mut().unwrap();
            ui.drag_epoch = ui.drag_epoch.wrapping_add(1);
            (ui.drag.take(), ui.draft.take())
        };
        let geometry = matches!(drag, Some(ClipDrag::Trim { .. } | ClipDrag::Adjust { .. }));
        if geometry {
            if let Some(candidate) = candidate {
                let result = self.editor.set_long_clip(Some(candidate.clone()));
                if let Err(error) = result {
                    self.status = error.to_string();
                }
                if let Some(ui) = &mut self.long_clip_ui {
                    if let Some(ClipDrag::Trim {
                        edge,
                        before,
                        expanded,
                        ..
                    }) = &drag
                    {
                        ui.expanded = expanded.clone();
                        if matches!(edge, ClipEdge::LongStart | ClipEdge::LongEnd) {
                            let length = lengths(before).0;
                            let old = if *edge == ClipEdge::LongStart {
                                before.removed().first().filter(|r| r.start == 0)
                            } else {
                                before.removed().last().filter(|r| r.end == length)
                            };
                            if old.is_some_and(|range| expanded.contains(range)) {
                                let new = if *edge == ClipEdge::LongStart {
                                    candidate.removed().first().filter(|r| r.start == 0)
                                } else {
                                    candidate.removed().last().filter(|r| r.end == length)
                                };
                                if let Some(range) = new {
                                    ui.expanded.push(range.clone());
                                }
                            }
                        }
                    }
                    if let Some(clip) = self.editor.long_clip() {
                        ui.retain_expanded(clip);
                    }
                    ui.selection = None;
                    ui.anchor = None;
                }
            }
        }
        if let (Some(projection), Some(anchor)) = (projection, anchor) {
            self.long_clip_keep_anchor(&projection, anchor);
        }
        if unsafe { GetCapture() } == hwnd(window) {
            self.release_capture();
        }
        self.long_clip_refresh(window, cx);
        cx.stop_propagation();
        true
    }

    pub(crate) fn long_clip_cancel_drag(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let projection = self.long_clip_projection();
        let anchor = self.long_clip_source_center(window);
        let Some(ui) = &mut self.long_clip_ui else {
            return false;
        };
        let Some(drag) = ui.drag.take() else {
            return false;
        };
        ui.drag_epoch = ui.drag_epoch.wrapping_add(1);
        ui.draft = None;
        match drag {
            ClipDrag::Select {
                before_selection,
                before_anchor,
                before_end,
                ..
            }
            | ClipDrag::SelectionEdge {
                before_selection,
                before_anchor,
                before_end,
                ..
            } => {
                ui.selection = before_selection;
                ui.anchor = before_anchor;
                ui.selection_end = before_end;
            }
            ClipDrag::Trim { expanded, .. } | ClipDrag::Adjust { expanded, .. } => {
                ui.expanded = expanded
            }
            // 平移只改变浏览位置，取消只是停止跟随，不把视口弹回按下处。
            ClipDrag::Pan { .. } => {}
        }
        if let (Some(projection), Some(anchor)) = (projection, anchor) {
            self.long_clip_keep_anchor(&projection, anchor);
        }
        if unsafe { GetCapture() } == hwnd(window) {
            self.release_capture();
        }
        self.long_clip_refresh(window, cx);
        true
    }

    /// 失焦与宿主取消只清理临时态，不把拖动候选借机提交到文档。
    pub(crate) fn long_clip_cancel_gesture(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let projection = self.long_clip_projection();
        let anchor = self.long_clip_source_center(window);
        let navigation = self.long_clip_navigation_cancel(window);
        let dragging = self.long_clip_cancel_drag(window, cx);
        let Some(ui) = &mut self.long_clip_ui else {
            return navigation || dragging;
        };
        let changed = navigation
            || dragging
            || ui.space
            || ui.alt
            || ui.draft.is_some()
            || ui.selection.is_some()
            || ui.anchor.is_some();
        ui.drag_epoch = ui.drag_epoch.wrapping_add(1);
        ui.drag = None;
        ui.draft = None;
        ui.selection = None;
        ui.anchor = None;
        ui.space = false;
        ui.alt = false;
        if !dragging {
            if let (Some(projection), Some(anchor)) = (projection, anchor) {
                self.long_clip_keep_anchor(&projection, anchor);
            }
        }
        if changed {
            self.long_clip_refresh(window, cx);
        }
        changed
    }

    fn long_clip_auto_scroll(
        &mut self,
        epoch: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ui) = &self.long_clip_ui else {
            return false;
        };
        if ui.drag_epoch != epoch
            || ui.drag.is_none()
            || matches!(ui.drag, Some(ClipDrag::Pan { .. }))
        {
            return false;
        }
        if !self.long_clip_input_ready(window) || unsafe { GetCapture() } != hwnd(window) {
            self.long_clip_cancel_drag(window, cx);
            return false;
        }
        if matches!(
            ui.drag,
            Some(
                ClipDrag::Select { moved: false, .. }
                    | ClipDrag::SelectionEdge { moved: false, .. }
                    | ClipDrag::Adjust { moved: false, .. }
                    | ClipDrag::Trim { moved: false, .. }
            )
        ) {
            return true;
        }
        let pointer = ui.pointer;
        let alt = ui.alt;
        let Some(pointer) = pointer else {
            return true;
        };
        let Some(clip) = self.long_clip_current() else {
            return false;
        };
        let axis = clip.axis();
        let Some(area) = self.long_clip_view_area(window) else {
            return false;
        };
        let dpi = window.scale_factor();
        let value = along(axis, pointer).0;
        let low = along(
            axis,
            Point {
                x: area.x,
                y: area.y,
            },
        )
        .0;
        let high = low
            + along(
                axis,
                Point {
                    x: area.width,
                    y: area.height,
                },
            )
            .0;
        let margin = 38. * dpi;
        let speed = if value < low + margin {
            ((low + margin - value) / 3.).min(20. * dpi)
        } else if value > high - margin {
            -((value - high + margin) / 3.).min(20. * dpi)
        } else {
            0.
        };
        if speed == 0. {
            return true;
        }
        let content = self
            .long_clip_projection()
            .map(|p| p.display_size())
            .unwrap_or((1, 1));
        if let Some(state) = &mut self.image_canvas {
            let (old, cross) = along(axis, state.viewport.offset);
            let length = along(
                axis,
                Point {
                    x: content.0 as f32,
                    y: content.1 as f32,
                },
            )
            .0 * state.viewport.zoom;
            let padding = 28. * dpi;
            let next = if length < high - low - 2. * padding {
                old
            } else {
                (old + speed).clamp(high - padding - length, low + padding)
            };
            if (next - old).abs() < 0.01 {
                return true;
            }
            state.viewport.offset = compose(axis, next, cross);
            state.auto_fit = false;
        }
        self.long_clip_update_drag(pointer, alt, window);
        self.long_clip_refresh(window, cx);
        true
    }

    pub(crate) fn long_clip_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.trim_inputs.as_ref())
            .is_some_and(|input| input.read(cx).focused(window, cx))
        {
            return false;
        }
        if self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.parameters_pending)
        {
            if event.keystroke.key == "escape" {
                self.long_clip_cancel_parameters(window, cx);
            }
            cx.stop_propagation();
            return true;
        }
        // 数字工具键仍沿用核心的草稿结束策略，但不能回落到旧截图的槽位编号。
        // 文字、组合输入和参数控件继续拥有自己的数字；其它剪辑输入仍要求空闲。
        let tool_digit = (self.toolbar_panel == ToolbarPanel::None || self.toolbar_panel.more())
            && !event.keystroke.modifiers.modified()
            && matches!(
                event.keystroke.key.as_str(),
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
            )
            && self.long_clip_host_available()
            && window.is_window_active()
            && self.focus.is_focused(window);
        if !self.long_clip_input_ready(window) && !tool_digit {
            return false;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let direction = matches!(key, "left" | "right" | "up" | "down");
        let has_selection = self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.selection.is_some());
        if key == "alt" {
            if let Some(ui) = &mut self.long_clip_ui {
                ui.alt = true;
            }
            return true;
        }
        if modifiers.control || modifiers.platform {
            if self
                .long_clip_ui
                .as_ref()
                .is_some_and(|ui| ui.drag.is_some())
            {
                self.long_clip_cancel_drag(window, cx);
            }
            return false;
        }
        if modifiers.alt || modifiers.function {
            return (self.editor.tool == Tool::Crop || has_selection) && direction;
        }
        if key == "space" && !modifiers.shift {
            if let Some(ui) = &mut self.long_clip_ui {
                ui.space = true;
            }
            cx.stop_propagation();
            return true;
        }
        if key == "escape" {
            if self.long_clip_navigation_cancel(window) {
                cx.notify();
                return true;
            }
            if self
                .long_clip_ui
                .as_mut()
                .is_some_and(|ui| ui.tool_palette.take().is_some())
            {
                cx.notify();
                return true;
            }
            if self.long_clip_cancel_drag(window, cx) {
                return true;
            }
            if let Some(ui) = &mut self.long_clip_ui {
                let selection = ui.selection.take().is_some();
                let anchor = ui.anchor.take().is_some();
                if selection || anchor {
                    cx.notify();
                    return true;
                }
            }
            return false;
        }
        if !modifiers.shift
            && matches!(
                key,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
            )
        {
            if !event.is_held {
                self.long_clip_cancel_drag(window, cx);
                if key == "0" {
                    // 工具库在左侧工具栏旁展开，与前端稿一致。
                    if let Some(ui) = &mut self.long_clip_ui {
                        ui.tool_palette = (ui.tool_palette != Some(None)).then_some(None);
                    }
                    cx.notify();
                    return true;
                }
                let action = match key {
                    "1" => Action::Tool(Tool::Crop),
                    "2" => Action::Tool(Tool::Edit),
                    "9" => Action::Tool(Tool::Lasso),
                    _ => {
                        let index = key.as_bytes()[0] as usize - b'3' as usize;
                        let group = crate::toolbar_groups::GROUPS[index];
                        Action::Tool(self.tool_groups.preferred(group, false))
                    }
                };
                self.perform(action, window, cx);
            }
            return true;
        }
        if self
            .long_clip_ui
            .as_ref()
            .is_some_and(|ui| ui.drag.is_some())
        {
            return matches!(
                key,
                "enter" | "delete" | "backspace" | "up" | "down" | "left" | "right"
            ) || key.eq_ignore_ascii_case("h")
                || key.eq_ignore_ascii_case("k");
        }
        if key.eq_ignore_ascii_case("h") {
            if !event.is_held {
                self.long_clip_toggle_all(window, cx);
            }
            return true;
        }
        let selection = self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.selection.clone());
        if matches!(key, "enter" | "delete" | "backspace") {
            if selection.is_some() {
                if !event.is_held {
                    self.long_clip_apply_selection(false, window, cx);
                }
                return true;
            }
            return self.editor.tool == Tool::Crop;
        }
        if key.eq_ignore_ascii_case("k") && (selection.is_some() || self.editor.tool == Tool::Crop)
        {
            if !event.is_held && selection.is_some() {
                self.long_clip_apply_selection(true, window, cx);
            }
            return true;
        }
        let axis = self.editor.long_clip().unwrap().axis();
        let negative = match axis {
            ScrollAxis::Vertical => key == "up",
            ScrollAxis::Horizontal => key == "left",
        };
        let positive = match axis {
            ScrollAxis::Vertical => key == "down",
            ScrollAxis::Horizontal => key == "right",
        };
        if (negative || positive) && selection.is_some() {
            let range = selection.unwrap();
            let ui = self.long_clip_ui.as_ref().unwrap();
            let end = ui.selection_end;
            let fixed = if end { range.start } else { range.end };
            let moving = if end { range.end } else { range.start };
            let step = if modifiers.shift { 10_i64 } else { 1_i64 } * if negative { -1 } else { 1 };
            let length = lengths(self.editor.long_clip().unwrap()).0;
            let next = (i64::from(moving) + step).clamp(0, i64::from(length)) as u32;
            if let Some(ui) = &mut self.long_clip_ui {
                ui.set_selection(fixed, next);
            }
            cx.notify();
            return true;
        }
        if (self.editor.tool == Tool::Crop || has_selection) && direction {
            return true;
        }
        if matches!(key, "home" | "end" | "pageup" | "pagedown") {
            let Some(projection) = self.long_clip_projection() else {
                return false;
            };
            let Some(area) = self.long_clip_view_area(window) else {
                return false;
            };
            let start = along(
                axis,
                Point {
                    x: area.x,
                    y: area.y,
                },
            )
            .0;
            let host = along(
                axis,
                Point {
                    x: area.width,
                    y: area.height,
                },
            )
            .0;
            let display = projection.display_size();
            let length = along(
                axis,
                Point {
                    x: display.0 as f32,
                    y: display.1 as f32,
                },
            )
            .0;
            if let Some(state) = &mut self.image_canvas {
                let (old, cross) = along(axis, state.viewport.offset);
                let last = (start + host - length * state.viewport.zoom).min(start);
                let next = match key {
                    "home" => start,
                    "end" => last,
                    "pageup" => (old + host * 0.8).min(start),
                    _ => (old - host * 0.8).max(last),
                };
                state.viewport.offset = compose(axis, next, cross);
                state.auto_fit = false;
            }
            self.long_clip_refresh(window, cx);
            return true;
        }
        false
    }

    pub(crate) fn long_clip_key_up(
        &mut self,
        event: &KeyUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ui) = &mut self.long_clip_ui else {
            return false;
        };
        match event.keystroke.key.as_str() {
            "space" if ui.space => {
                ui.space = false;
                cx.notify();
                true
            }
            "alt" if ui.alt => {
                ui.alt = false;
                cx.notify();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn long_clip_apply_selection(
        &mut self,
        keep: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.long_clip_action_ready() {
            return;
        }
        let Some(range) = self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.selection.clone())
        else {
            return;
        };
        let before = self.long_clip_projection();
        let axis = self.editor.long_clip().unwrap().axis();
        let cross = self.editor.long_clip().unwrap().cross_range();
        let anchor = compose(
            axis,
            range.start as f32,
            (cross.start + (cross.end - cross.start) / 2) as f32,
        );
        let result = if keep {
            self.editor.keep_long_clip_range(range)
        } else {
            self.editor.delete_long_clip_range(range)
        };
        self.long_clip_complete_command(result, before, anchor, window, cx);
    }

    pub(crate) fn long_clip_restore(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.long_clip_action_ready() {
            return;
        }
        let before = self.long_clip_projection();
        let anchor = self
            .long_clip_source_center(window)
            .unwrap_or(Point { x: 0., y: 0. });
        let result = self.editor.restore_long_clip_range(index);
        self.long_clip_complete_command(result, before, anchor, window, cx);
    }

    pub(crate) fn long_clip_restore_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.long_clip_action_ready() {
            return;
        }
        let before = self.long_clip_projection();
        let anchor = self
            .long_clip_source_center(window)
            .unwrap_or(Point { x: 0., y: 0. });
        let result = self.editor.clear_long_clip();
        self.long_clip_complete_command(result, before, anchor, window, cx);
    }

    pub(crate) fn long_clip_complete_command(
        &mut self,
        result: crate::Result<bool>,
        before: Option<LongClipProjection>,
        anchor: Point,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_) => {
                if let Some(ui) = &mut self.long_clip_ui {
                    ui.selection = None;
                    ui.anchor = None;
                    ui.draft = None;
                    if let Some(clip) = self.editor.long_clip() {
                        ui.retain_expanded(clip);
                    }
                }
                self.status.clear();
                if let Some(before) = before {
                    self.long_clip_keep_anchor(&before, anchor);
                }
            }
            Err(error) => self.status = error.to_string(),
        }
        self.long_clip_refresh(window, cx);
    }

    pub(crate) fn long_clip_toggle(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.long_clip_action_ready() {
            return;
        }
        let Some(range) = self
            .editor
            .long_clip()
            .and_then(|clip| clip.removed().get(index))
            .cloned()
        else {
            return;
        };
        let before = self.long_clip_projection();
        let anchor = self
            .long_clip_source_center(window)
            .unwrap_or(Point { x: 0., y: 0. });
        if let Some(ui) = &mut self.long_clip_ui {
            if ui.expanded.contains(&range) {
                ui.expanded.retain(|r| r != &range);
            } else {
                ui.expanded.push(range);
            }
        }
        if let Some(before) = before {
            self.long_clip_keep_anchor(&before, anchor);
        }
        self.long_clip_refresh(window, cx);
    }

    pub(crate) fn long_clip_toggle_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.long_clip_action_ready() {
            return;
        }
        let before = self.long_clip_projection();
        let anchor = self
            .long_clip_source_center(window)
            .unwrap_or(Point { x: 0., y: 0. });
        if let (Some(ui), Some(clip)) = (&mut self.long_clip_ui, self.editor.long_clip()) {
            if clip
                .removed()
                .iter()
                .all(|range| ui.expanded.contains(range))
            {
                ui.expanded.clear();
            } else {
                ui.expanded = clip.removed().to_vec();
            }
        }
        if let Some(before) = before {
            self.long_clip_keep_anchor(&before, anchor);
        }
        self.long_clip_refresh(window, cx);
    }

    pub(crate) fn long_clip_locate(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.long_clip_action_ready() {
            return;
        }
        let Some(clip) = self.editor.long_clip() else {
            return;
        };
        let Some(range) = clip.removed().get(index) else {
            return;
        };
        let axis = clip.axis();
        let point = compose(
            axis,
            (range.start + (range.end - range.start) / 2) as f32,
            lengths(clip).1 as f32 / 2.,
        );
        let Some(projected) = self
            .long_clip_projection()
            .and_then(|p| p.source_to_display_nearest(point))
        else {
            return;
        };
        let Some(area) = self.long_clip_view_area(window) else {
            return;
        };
        let center = along(
            axis,
            Point {
                x: area.x + area.width / 2.,
                y: area.y + area.height / 2.,
            },
        )
        .0;
        if let Some(state) = &mut self.image_canvas {
            let (_, cross) = along(axis, state.viewport.offset);
            state.viewport.offset = compose(
                axis,
                center - along(axis, projected).0 * state.viewport.zoom,
                cross,
            );
            state.auto_fit = false;
        }
        if let Some(ui) = &mut self.long_clip_ui {
            ui.hover = Some(index);
        }
        self.long_clip_refresh(window, cx);
    }
}
