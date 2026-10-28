//! 显式录制绘图层：排除采集，Scene矢量预览；被动模式跨进程鼠标穿透。
use crate::{
    annotation::{Point, Style, Tool},
    gpui_capture::hwnd,
    pin_model::Placement,
    recording::live_annotation::{
        accepts_pointer, edit_key, EditKey, Geometry, LiveEditor, LiveTool,
    },
    recording_key_owner::KeyOwnerProbe,
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, App, AppContext, Context, FocusHandle, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use std::sync::Arc;
use windows::Win32::{
    Foundation::{COLORREF, HWND, POINT},
    Graphics::Gdi::ClientToScreen,
    UI::{
        Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

pub(super) struct DrawingView {
    live: LiveEditor,
    recording: Arc<crate::recording::Recording>,
    focus: FocusHandle,
    captured: bool,
    previous: HWND,
    key_owner: Option<KeyOwnerProbe>,
}
impl DrawingView {
    pub(super) fn open(
        geometry: Geometry,
        recording: Arc<crate::recording::Recording>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let region = geometry.region;
        let placement = Placement {
            x: region.left,
            y: region.top,
            width: region.width,
            height: region.height,
        };
        let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
        let previous = unsafe { GetForegroundWindow() };
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                show: false,
                focus: false,
                is_movable: false,
                is_resizable: false,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |w, cx| {
                w.set_window_title(brand_title!("录制中绘图", "Draw while recording"));
                let focus = cx.focus_handle();
                let view = cx.new(|_| Self {
                    live: LiveEditor::new(geometry, recording.annotations.clone()),
                    recording,
                    focus,
                    captured: false,
                    previous,
                    key_owner: None,
                });
                let weak = view.downgrade();
                // 绘图层的Close只退出输入，Document保留到宿主停止/结束会话。
                w.on_window_should_close(cx, move |w, cx| {
                    weak.update(cx, |v, cx| {
                        let _ = v.set_drawing(false, w, cx);
                        false
                    })
                    .unwrap_or(true)
                });
                view
            },
        )?;
        let setup = handle.update(cx, |v, w, cx| -> Result<()> {
            let h = hwnd(w);
            v.key_owner = Some(KeyOwnerProbe::install(h)?);
            unsafe {
                // 先排除，核实后才允许显示绘图和接收指针；失败回收自有窗口。
                crate::remote_display::exclude_from_capture(h)?;
                let mut affinity = 0;
                GetWindowDisplayAffinity(h, &mut affinity)?;
                if affinity != crate::remote_display::capture_affinity().0 {
                    return Err(tr!(
                        "录制绘图层未被排除采集",
                        "Drawing layer could not be excluded from capture"
                    )
                    .into());
                }
                SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
                SetWindowLongPtrW(
                    h,
                    GWL_EXSTYLE,
                    GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_TOOLWINDOW.0 as isize,
                );
                SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    region.left,
                    region.top,
                    region.width as i32,
                    region.height as i32,
                    SWP_FRAMECHANGED | SWP_NOACTIVATE,
                )?;
            }
            v.set_drawing(true, w, cx)?;
            unsafe {
                SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                )?;
                let _ = SetForegroundWindow(h);
            }
            w.focus(&v.focus);
            Ok(())
        });
        match setup {
            Ok(Ok(())) => Ok(handle),
            other => {
                let _ = handle.update(cx, |_, w, _| w.remove_window());
                match other {
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(e.into()),
                    _ => unreachable!(),
                }
            }
        }
    }
    pub(super) fn drawing(&self) -> bool {
        self.live.drawing()
    }
    fn accepting(&self) -> bool {
        accepts_pointer(
            self.drawing(),
            self.recording.phase(),
            self.recording.is_stopping(),
        )
    }
    fn input_ready(&self) -> bool {
        self.accepting()
            && !self.live.properties_open()
            && self.key_owner.as_ref().is_some_and(|p| !p.composing())
    }
    pub(super) fn edit_state(&self) -> (bool, bool, bool) {
        (
            self.live.can_undo(),
            self.live.can_redo(),
            self.live.has_selection(),
        )
    }
    fn native_input(&self, on: bool, w: &Window) -> Result<()> {
        let h = hwnd(w);
        unsafe {
            let old = GetWindowLongPtrW(h, GWL_EXSTYLE);
            let flags = (WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_LAYERED).0 as isize;
            SetWindowLongPtrW(h, GWL_EXSTYLE, if on { old & !flags } else { old | flags });
            if !on {
                SetLayeredWindowAttributes(h, COLORREF(0), 255, LWA_ALPHA)?;
            }
            SetWindowPos(
                h,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )?;
        }
        Ok(())
    }
    pub(super) fn tool(&self) -> LiveTool {
        self.live.tool()
    }
    pub(super) fn has_annotations(&self) -> bool {
        self.live.has_annotations()
    }
    /// 样式弹层的目标工具与当前样式；不可改时为 None（如编辑工具未选中对象）。
    pub(super) fn style(&self) -> Option<(Tool, Style)> {
        if !self.input_ready() || self.captured {
            return None;
        }
        self.live.style_target()
    }
    /// 颜色、粗细、透明度点一下即生效；选中对象时为一次可撤销修改。
    pub(super) fn apply_style(
        &mut self,
        color: Option<u32>,
        width: Option<f32>,
        opacity: Option<f32>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.input_ready() || self.captured {
            return false;
        }
        let applied = self.live.apply_style(color, width, opacity);
        cx.notify();
        applied
    }
    fn release(&mut self, w: &Window) {
        if self.captured {
            unsafe {
                if GetCapture() == hwnd(w) {
                    let _ = ReleaseCapture();
                }
            }
            self.captured = false;
        }
    }
    pub(super) fn set_drawing(
        &mut self,
        on: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let h = hwnd(w);
        if on && !accepts_pointer(true, self.recording.phase(), self.recording.is_stopping()) {
            return Err(tr!(
                "录制已停止，不能进入绘图",
                "Recording stopped; drawing unavailable"
            )
            .into());
        }
        if !on {
            self.release(w);
            self.live.set_drawing(false);
        }
        self.native_input(on, w)?;
        unsafe {
            if on {
                let foreground = GetForegroundWindow();
                if foreground != h {
                    self.previous = foreground;
                }
                let _ = SetForegroundWindow(h);
                w.focus(&self.focus);
            } else if GetForegroundWindow() == h && IsWindow(Some(self.previous)).as_bool() {
                let _ = SetForegroundWindow(self.previous);
            }
        }
        self.live.set_drawing(on);
        cx.notify();
        Ok(())
    }
    pub(super) fn choose(&mut self, tool: LiveTool, cx: &mut Context<Self>) {
        if self.input_ready() {
            self.live.select(tool);
            cx.notify();
        }
    }
    pub(super) fn undo(&mut self, cx: &mut Context<Self>) {
        if self.input_ready() {
            self.live.undo();
            cx.notify();
        }
    }
    pub(super) fn redo(&mut self, cx: &mut Context<Self>) {
        if self.input_ready() {
            self.live.redo();
            cx.notify();
        }
    }
    pub(super) fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.input_ready() {
            self.live.delete_selected();
            cx.notify();
        }
    }
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        if self.input_ready() {
            self.live.clear();
            cx.notify();
        }
    }
    /// 由真实RecordingView.poll接入，不在WindowProc借用Rust可变状态。
    pub(super) fn check_input(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.captured && unsafe { GetCapture() } != hwnd(w) {
            self.captured = false;
            self.live.cancel();
            cx.notify();
        }
        if self.drawing()
            && !accepts_pointer(true, self.recording.phase(), self.recording.is_stopping())
        {
            let _ = self.set_drawing(false, w, cx);
        }
    }
    pub(super) fn close(&mut self, w: &mut Window) {
        self.release(w);
        self.live.set_drawing(false);
        self.recording.annotations.clear();
        w.remove_window();
    }
    fn global(p: gpui::Point<gpui::Pixels>, w: &Window) -> Option<Point> {
        let mut origin = POINT::default();
        if !unsafe { ClientToScreen(hwnd(w), &mut origin) }.as_bool() {
            return None;
        }
        Geometry::client_global(
            (origin.x, origin.y),
            Point {
                x: f32::from(p.x),
                y: f32::from(p.y),
            },
            w.scale_factor(),
        )
    }
    fn down(&mut self, e: &MouseDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        if !self.input_ready() {
            return;
        }
        if let Some(p) = Self::global(e.position, w) {
            if self.captured {
                return;
            }
            unsafe {
                SetCapture(hwnd(w));
            }
            self.captured = unsafe { GetCapture() } == hwnd(w);
            if !self.captured {
                return;
            }
            if self.live.down(p, 5. * w.scale_factor(), e.modifiers.shift) {
                cx.stop_propagation();
                cx.notify();
            } else {
                self.release(w);
            }
        }
    }
    fn moved(&mut self, e: &MouseMoveEvent, w: &mut Window, cx: &mut Context<Self>) {
        if self.captured && unsafe { GetCapture() } != hwnd(w) {
            self.captured = false;
            self.live.cancel();
            cx.notify();
            return;
        }
        if !self.input_ready() {
            if self.captured || !self.accepting() {
                self.release(w);
                self.live.cancel();
            }
            return;
        }
        if let Some(p) = Self::global(e.position, w) {
            if self.live.moved(p) {
                cx.stop_propagation();
                cx.notify();
            }
        }
    }
    fn up(&mut self, e: &MouseUpEvent, w: &mut Window, cx: &mut Context<Self>) {
        if self.captured && unsafe { GetCapture() } != hwnd(w) {
            self.captured = false;
            self.live.cancel();
            cx.notify();
            return;
        }
        if !self.input_ready() {
            if self.captured || !self.accepting() {
                self.release(w);
                self.live.cancel();
            }
            return;
        }
        if let Some(p) = Self::global(e.position, w) {
            if self.live.up(p) {
                cx.stop_propagation();
                cx.notify();
            }
        }
        self.release(w);
    }
    fn key(&mut self, e: &KeyDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        if !self.input_ready()
            || e.is_held
            || !self.focus.is_focused(w)
            || !w.is_window_active()
            || self.key_owner.as_ref().is_none_or(|p| !p.owns_keyboard())
        {
            return;
        }
        let m = e.keystroke.modifiers;
        if e.keystroke.key == "escape" && !m.control && !m.alt && !m.platform {
            self.release(w);
            if !self.live.cancel() {
                if let Err(error) = self.set_drawing(false, w, cx) {
                    eprintln!("退出录制绘图失败：{error}");
                }
            }
            cx.stop_propagation();
            cx.notify();
        } else if !m.control && !m.alt && !m.platform && !m.shift && e.keystroke.key == "f9" {
            self.recording.request_stop();
            self.release(w);
            self.live.cancel();
            let _ = self.set_drawing(false, w, cx);
            cx.stop_propagation();
        } else if !m.control && !m.alt && !m.platform && !m.shift && e.keystroke.key == "f10" {
            self.recording.toggle_pause();
            cx.stop_propagation();
        } else {
            if m.function {
                return;
            }
            let Some(action) = edit_key(&e.keystroke.key, m.control, m.shift, m.alt, m.platform)
            else {
                return;
            };
            self.release(w);
            match action {
                EditKey::Undo => self.undo(cx),
                EditKey::Redo => self.redo(cx),
                EditKey::Delete => self.delete_selected(cx),
                EditKey::Clear => self.clear(cx),
            }
            cx.stop_propagation();
        }
    }
}
impl Render for DrawingView {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self
            .live
            .snapshot()
            .filter(|s| s.stamp == self.live.stamp());
        let scale = w.scale_factor();
        let roi = self.live.geometry.roi;
        let mut root = div()
            .size_full()
            .relative()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::down))
            .on_mouse_move(cx.listener(Self::moved))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::up))
            .on_key_down(cx.listener(Self::key))
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, w, _| {
                        let Some(frame) = &snapshot else {
                            return;
                        };
                        let mut paths = crate::vector_paint::FramePaths::new(
                            frame.scene.layers.len(),
                            point(px(-(roi.x as f32) / scale), px(-(roi.y as f32) / scale)),
                            scale,
                            w.scale_factor(),
                        );
                        for (i, layer) in frame.scene.layers.iter().enumerate() {
                            if let crate::scene::Layer::Vector { mesh, style, .. } = layer {
                                crate::vector_paint::paint(
                                    paths.get(
                                        i,
                                        crate::vector_paint::PathPart::Main,
                                        mesh,
                                        *style,
                                        false,
                                    ),
                                    *style,
                                    w,
                                );
                            }
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        if self.drawing() && !self.recording.annotations.hidden() {
            if let Some(b) = self.live.selected_bounds() {
                let p = crate::theme::palette(cx);
                root = root.child(
                    div()
                        .absolute()
                        .left(px((b.x - roi.x as f32) / scale))
                        .top(px((b.y - roi.y as f32) / scale))
                        .w(px(b.width / scale))
                        .h(px(b.height / scale))
                        .border_1()
                        .border_color(rgb(p.accent)),
                );
            }
        }
        root
    }
}
