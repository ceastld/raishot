//! 参数菜单是工具栏拥有的独立小窗；复用截图的属性视图和文档，不改变主栏尺寸。
use crate::{
    capture_view::CaptureView,
    geometry::Rect,
    gpui_capture::hwnd,
    pin_model::Placement,
    pin_native as native,
    pin_view::PinView,
    toolbar_layout::{PanelRect, ToolbarLayout, ToolbarPanel},
    Result,
};
use gpui::{
    div, point, prelude::*, px, rgb, size, App, AppContext, Bounds, Context, Entity, FocusHandle,
    MouseButton, Render, Window, WindowBounds, WindowHandle, WindowOptions,
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

#[derive(Clone, Copy)]
pub(crate) struct PopupAnchor {
    pub toolbar: HWND,
    pub bounds: Placement,
    pub dpi: f32,
    pub point: Option<(f32, f32)>,
    pub topmost: bool,
}
pub(crate) struct PinPropertyPopup {
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    panel: ToolbarPanel,
    logical_size: (f32, f32),
    focus: FocusHandle,
    activated: bool,
}
fn dimensions(panel: ToolbarPanel, v: &CaptureView) -> (f32, f32) {
    let tool = crate::parameter_control::ParameterSpec::for_view(panel, v)
        .map(|spec| spec.tool)
        .unwrap_or_else(|| {
            v.editor
                .selected_annotation()
                .map_or(v.editor.tool, |a| a.tool)
        });
    if let Some(size) = crate::toolbar_layout::parameter_popup_size(panel, tool) {
        return size;
    }
    if let Some(size) = v.property_popup_size(panel) {
        return size;
    }
    match panel {
        ToolbarPanel::Font => (196., 48.),
        ToolbarPanel::Opacity | ToolbarPanel::Radius | ToolbarPanel::Block => (152., 48.),
        _ => (240., 48.),
    }
}
fn placement(size: (f32, f32), anchor: PopupAnchor) -> Placement {
    popup_placement(anchor, (size.0 + 24., size.1 + 24.), 28.)
}
/// 工具栏弹层位置：优先贴在工具栏下方，放不下翻到上方；`left` 是锚点到弹层左缘的逻辑距离。
pub(crate) fn popup_placement(anchor: PopupAnchor, logical: (f32, f32), left: f32) -> Placement {
    let bar = anchor.bounds;
    let work = native::work_area((bar.x + bar.width as i32 / 2, bar.y + bar.height as i32 / 2));
    let width = ((logical.0 * anchor.dpi).ceil() as u32).min(work.width);
    let height = ((logical.1 * anchor.dpi).ceil() as u32).min(work.height);
    let x = anchor.point.map_or(bar.x, |p| {
        bar.x + ((p.0 - left) * anchor.dpi).round() as i32
    });
    let gap = (6. * anchor.dpi).ceil() as i32;
    let after = bar.y + bar.height as i32 + gap;
    let y = if after + height as i32 <= work.y + work.height as i32 {
        after
    } else {
        bar.y - height as i32 - gap
    };
    Placement {
        x: x.clamp(work.x, work.x + (work.width - width) as i32),
        y: y.clamp(work.y, work.y + (work.height - height) as i32),
        width,
        height,
    }
}
/// 把 GPUI 弹出窗改成工具栏拥有的无边框工具窗。
pub(crate) fn own_popup(w: &Window, toolbar: usize) {
    unsafe {
        let h = hwnd(w);
        SetWindowLongPtrW(h, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        SetWindowLongPtrW(
            h,
            GWL_EXSTYLE,
            GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_TOOLWINDOW.0 as isize,
        );
        SetWindowLongPtrW(h, GWLP_HWNDPARENT, toolbar as isize);
    }
}
pub(crate) fn open(
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    panel: ToolbarPanel,
    anchor: PopupAnchor,
    cx: &mut App,
) -> Result<WindowHandle<PinPropertyPopup>> {
    let logical_size = dimensions(panel, canvas.read(cx));
    let p = placement(logical_size, anchor);
    let dpi = anchor.dpi;
    let toolbar = anchor.toolbar.0 as usize;
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(p.x as f32 / dpi), px(p.y as f32 / dpi)),
                size(px(p.width as f32 / dpi), px(p.height as f32 / dpi)),
            ))),
            kind: gpui::WindowKind::PopUp,
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        },
        move |w, cx| {
            w.set_window_title(brand_title!("贴图参数", "Pin settings"));
            own_popup(w, toolbar);
            native::place_later(hwnd(w), p, Some(anchor.topmost), true, cx);
            let focus = cx.focus_handle();
            w.focus(&focus);
            let view = cx.new(|cx: &mut Context<PinPropertyPopup>| {
                // 参数变化只刷新这一小窗；选定档位、换工具或 Esc 关闭当前菜单。
                cx.observe_in(&canvas, w, |v, canvas, w, cx| {
                    if canvas.read(cx).toolbar_panel != v.panel {
                        restore_editor_focus(v.owner, cx);
                        w.remove_window();
                    } else {
                        cx.notify();
                    }
                })
                .detach();
                cx.observe_window_activation(w, |v, w, cx| {
                    if w.is_window_active() {
                        v.activated = true;
                    } else if v.activated && !v.canvas.read(cx).color_picker_pending {
                        v.dismiss(w, cx);
                    }
                })
                .detach();
                PinPropertyPopup {
                    owner,
                    canvas,
                    panel,
                    logical_size,
                    focus,
                    activated: false,
                }
            });
            let weak = view.downgrade();
            w.on_window_should_close(cx, move |_, cx| {
                restore_editor_focus(owner, cx);
                let _ = weak.update(cx, |v, cx| {
                    v.canvas.update(cx, |canvas, cx| {
                        if canvas.toolbar_panel == v.panel {
                            canvas.toolbar_panel = ToolbarPanel::None;
                            cx.notify();
                        }
                    });
                });
                true
            });
            view
        },
    )?)
}
struct NativeEditorFocus {
    target: usize,
    root: usize,
    thread: u32,
    process: u32,
}
impl NativeEditorFocus {
    fn capture(target: HWND, root: HWND) -> Option<Self> {
        let mut process = 0;
        let thread = unsafe { GetWindowThreadProcessId(target, Some(&mut process)) };
        let focus = Self {
            target: target.0 as usize,
            root: root.0 as usize,
            thread,
            process,
        };
        focus.matches().then_some(focus)
    }
    fn matches(&self) -> bool {
        unsafe {
            let target = HWND(self.target as *mut _);
            let root = HWND(self.root as *mut _);
            let mut process = 0;
            let mut root_process = 0;
            self.thread != 0
                && self.process == std::process::id()
                && IsWindow(Some(target)).as_bool()
                && IsWindow(Some(root)).as_bool()
                && GetWindowThreadProcessId(target, Some(&mut process)) == self.thread
                && process == self.process
                && GetWindowThreadProcessId(root, Some(&mut root_process)) == self.thread
                && root_process == self.process
                && GetAncestor(target, GA_ROOTOWNER) == root
                && GetAncestor(root, GA_ROOTOWNER) == root
                && GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) == root
        }
    }
}
enum EditorFocusPlan {
    Canvas(NativeEditorFocus),
    Watermark {
        root: usize,
        canvas: gpui::WeakEntity<CaptureView>,
        window: gpui::AnyWindowHandle,
        field: gpui::WeakEntity<crate::text_field::TextField>,
    },
}

fn restore_editor_focus(owner: WindowHandle<PinView>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let plan = owner.update(cx, |v, w, cx| {
            let Some(canvas) = &v.canvas else {
                return None;
            };
            let editor = canvas.read(cx);
            if !editor.pin_editing || editor.toolbar_panel != ToolbarPanel::None {
                return None;
            }
            let root = hwnd(w);
            // 只在本贴图的 owned 窗口链内恢复输入，用户切去其他应用时不抢焦点。
            unsafe {
                if GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) != root {
                    return None;
                }
            }
            let tool = editor
                .editor
                .selected_annotation()
                .map_or(editor.editor.tool, |annotation| annotation.tool);
            if tool == crate::annotation::Tool::Watermark && editor.editor.selected_ids().len() <= 1
            {
                if let (Some(field), Some(input_window)) = (
                    editor.watermark_field.as_ref(),
                    editor.watermark_input_window,
                ) {
                    if input_window
                        .downcast::<crate::pin_property_bar::PinPropertyBar>()
                        .is_some()
                        && editor.pin_window == Some(owner)
                        && editor.watermark_binding == Some(editor.editor.watermark.binding())
                    {
                        return Some(EditorFocusPlan::Watermark {
                            root: root.0 as usize,
                            canvas: canvas.downgrade(),
                            window: input_window,
                            field: field.downgrade(),
                        });
                    }
                }
            }
            let focus = editor.focus.clone();
            let native = NativeEditorFocus::capture(root, root)?;
            w.focus(&focus);
            cx.notify();
            Some(EditorFocusPlan::Canvas(native))
        });
        let Ok(Some(plan)) = plan else {
            return;
        };
        let native = match plan {
            EditorFocusPlan::Canvas(native) => Some(native),
            EditorFocusPlan::Watermark {
                root,
                canvas,
                window,
                field,
            } => {
                // 顺序更新实际副栏窗口，不能在 PinView 的 update 内嵌套借用它。
                let same_canvas = owner
                    .update(cx, |view, _, _| {
                        view.canvas
                            .as_ref()
                            .is_some_and(|current| current.entity_id() == canvas.entity_id())
                    })
                    .unwrap_or(false);
                if !same_canvas {
                    return;
                }
                cx.update_window(window, |_, window, cx| {
                    let canvas = canvas.upgrade()?;
                    let field = field.upgrade()?;
                    let editor = canvas.read(cx);
                    if !editor.pin_editing
                        || editor.toolbar_panel != ToolbarPanel::None
                        || editor.pin_window != Some(owner)
                        || editor.watermark_input_window != Some(window.window_handle())
                        || editor
                            .watermark_field
                            .as_ref()
                            .is_none_or(|current| current.entity_id() != field.entity_id())
                        || editor.watermark_binding != Some(editor.editor.watermark.binding())
                        || editor
                            .editor
                            .selected_annotation()
                            .map_or(editor.editor.tool, |annotation| annotation.tool)
                            != crate::annotation::Tool::Watermark
                        || editor.editor.selected_ids().len() > 1
                    {
                        return None;
                    }
                    let focus = field.read(cx).focus_handle.clone();
                    let native = NativeEditorFocus::capture(hwnd(window), HWND(root as *mut _))?;
                    // 只恢复 FocusHandle，保留输入框的 caret / selection。
                    window.focus(&focus);
                    Some(native)
                })
                .ok()
                .flatten()
            }
        };
        let Some(native) = native else { return };
        // 原生激活会同步回调窗口；在 App/Window 借用结束后执行。
        unsafe {
            let h = HWND(native.target as *mut _);
            if native.matches() {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow(h);
                // 激活同步回调可能关闭窗口或使用户切去外部应用，再核一次身份。
                if native.matches() {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(h));
                }
            }
        }
    })
    .detach();
}
impl PinPropertyPopup {
    pub(crate) fn reposition(
        &self,
        bar: Placement,
        dpi: f32,
        point: Option<(f32, f32)>,
        topmost: bool,
        w: &Window,
        cx: &App,
    ) {
        let p = placement(
            self.logical_size,
            PopupAnchor {
                toolbar: HWND::default(),
                bounds: bar,
                dpi,
                point,
                topmost,
            },
        );
        if native::rect(hwnd(w)).is_ok_and(|current| current != p)
            || (unsafe { GetWindowLongPtrW(hwnd(w), GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize != 0)
                != topmost
        {
            native::place_later(hwnd(w), p, Some(topmost), false, cx);
        }
    }
    fn dismiss(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.canvas.update(cx, |v, cx| {
            // 旧菜单的失焦事件不能把刚切换的新菜单关闭。
            if v.toolbar_panel == self.panel {
                v.toolbar_panel = ToolbarPanel::None;
                cx.notify();
            }
        });
        w.remove_window();
    }
}
impl Render for PinPropertyPopup {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = f32::from(w.viewport_size().width).max(48.);
        let height = f32::from(w.viewport_size().height).max(48.);
        let palette = crate::theme::palette(cx);
        let content = self.canvas.update(cx, |v, cx| {
            // 共享布局在本小窗中固定从 12px 内边距开始，保留其尺寸和窄屏夹紧规则。
            let mut layout = ToolbarLayout::new(
                Rect::full(width.ceil() as u32, height.ceil() as u32),
                Rect::full(width.ceil() as u32, 1),
                1.,
                Some((12., 12.)),
                v.editor.tool,
            );
            layout.primary = PanelRect {
                x: 12.,
                y: -height,
                width: width - 24.,
                height: 0.,
            };
            layout.properties = layout.primary;
            v.property_popup(layout, Some((28., 0.)), None, cx)
        });
        div()
            .size_full()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .track_focus(&self.focus)
            .capture_any_mouse_down(cx.listener(|v, _, _, cx| {
                let _ = v
                    .owner
                    .update(cx, |owner, _, cx| owner.dismiss_context_menu(cx));
            }))
            .font_family("Microsoft YaHei UI")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, w, cx| v.dismiss(w, cx)),
            )
            .on_key_down(cx.listener(|v, e: &gpui::KeyDownEvent, _, cx| {
                let _ = v.owner.update(cx, |v, w, cx| v.key(e, w, cx));
            }))
            .child(content)
    }
}
