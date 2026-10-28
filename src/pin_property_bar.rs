//! 贴图的第二级属性栏；窗口只负责布局，参数和事件直接复用 CaptureView。
use crate::{
    annotation::Tool,
    capture_view::CaptureView,
    gpui_capture::hwnd,
    pin_annotation::ControlState,
    pin_model::Placement,
    pin_native as native,
    pin_properties::{self, PinPropertyPopup, PopupAnchor},
    pin_toolbar_layout::PinToolbarLayout,
    pin_view::{PinAction, PinView},
    toolbar_layout::{PanelRect, ToolbarPanel},
    Result,
};
use gpui::{
    div, point, prelude::*, px, rgb, size, App, AppContext, Bounds, Context, Entity, FocusHandle,
    MouseButton, Render, Subscription, Window, WindowBounds, WindowHandle, WindowOptions,
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PropertyBarAnchor {
    pub primary: HWND,
    pub bounds: Placement,
    pub dpi: f32,
    pub topmost: bool,
    pub above: bool,
}
pub(crate) struct PinPropertyBar {
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    _canvas_subscription: Subscription,
    anchor: PropertyBarAnchor,
    controls: ControlState,
    synced: Option<PropertySyncState>,
    logical_size: (f32, f32),
    level: u8,
    placement: Placement,
    popup_probe: std::rc::Rc<std::cell::Cell<Option<PopupProbe>>>,
    tool: Tool,
    focus: FocusHandle,
    sync_pending: bool,
    pub(crate) popup: Option<WindowHandle<PinPropertyPopup>>,
    pub(crate) popup_panel: ToolbarPanel,
}
#[derive(Clone, PartialEq)]
struct PropertySyncState {
    controls: ControlState,
    geometry: PropertyGeometry,
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct PropertyGeometry {
    anchor: PropertyBarAnchor,
    bounds: Option<Placement>,
    work_area: Placement,
    native_topmost: bool,
    viewport: (f32, f32),
    dpi: f32,
    popup_point: Option<(f32, f32)>,
    light_theme: bool,
    english: bool,
}
impl PropertyGeometry {
    fn applied(self, target: Placement) -> bool {
        self.bounds == Some(target) && self.native_topmost == self.anchor.topmost
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct PopupProbe {
    panel: ToolbarPanel,
    point: Option<(f32, f32)>,
}

fn active_tool(v: &CaptureView) -> Tool {
    v.editor
        .selected_annotation()
        .map_or(v.editor.tool, |a| a.tool)
}
pub(crate) fn visible(v: &CaptureView) -> bool {
    v.pin_editing
        && (!v.editor.selected_ids().is_empty()
            || active_tool(v).draws()
            || matches!(active_tool(v), Tool::Eraser | Tool::Lasso))
}
/// 二级栏逻辑尺寸与收起级别；绘制按同一级别列控件。
fn dimensions(v: &CaptureView, anchor: PropertyBarAnchor) -> ((f32, f32), u8) {
    let bar = anchor.bounds;
    let work = native::work_area((bar.x + bar.width as i32 / 2, bar.y + bar.height as i32 / 2));
    let (width, height, level) = PinToolbarLayout::property_layout(
        work,
        anchor.dpi,
        active_tool(v),
        v.editor.selected_ids().len(),
        v.editor.text_draft.is_some(),
    );
    ((width, height), level)
}
fn placement(logical: (f32, f32), anchor: PropertyBarAnchor) -> Placement {
    let bar = anchor.bounds;
    let work = native::work_area((bar.x + bar.width as i32 / 2, bar.y + bar.height as i32 / 2));
    PinToolbarLayout::properties(bar, work, anchor.dpi, logical, anchor.above)
}
pub(crate) fn open(
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    anchor: PropertyBarAnchor,
    cx: &mut App,
) -> Result<WindowHandle<PinPropertyBar>> {
    let (logical_size, level) = dimensions(canvas.read(cx), anchor);
    let p = placement(logical_size, anchor);
    let tool = active_tool(canvas.read(cx));
    let dpi = anchor.dpi;
    let primary = anchor.primary.0 as usize;
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(p.x as f32 / dpi), px(p.y as f32 / dpi)),
                size(px(p.width as f32 / dpi), px(p.height as f32 / dpi)),
            ))),
            kind: gpui::WindowKind::PopUp,
            is_resizable: false,
            is_minimizable: false,
            focus: false,
            show: false,
            ..Default::default()
        },
        move |w, cx| {
            w.set_window_title(brand_title!("贴图属性栏", "Pin property bar"));
            unsafe {
                let h = hwnd(w);
                SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
                SetWindowLongPtrW(
                    h,
                    GWL_EXSTYLE,
                    GetWindowLongPtrW(h, GWL_EXSTYLE)
                        | WS_EX_TOOLWINDOW.0 as isize
                        | if tool == Tool::Watermark {
                            0
                        } else {
                            WS_EX_NOACTIVATE.0 as isize
                        },
                );
                SetWindowLongPtrW(h, GWLP_HWNDPARENT, primary as isize);
            }
            native::place_later(hwnd(w), p, Some(anchor.topmost), true, cx);
            let focus = cx.focus_handle();
            w.focus(&focus);
            let view = cx.new(|cx: &mut Context<PinPropertyBar>| {
                let controls = ControlState::capture(canvas.read(cx));
                let subscription = cx.observe_in(&canvas, w, |v, canvas, w, cx| {
                    let controls = ControlState::capture(canvas.read(cx));
                    if v.controls == controls {
                        return;
                    }
                    v.controls = controls;
                    v.schedule_sync(w, cx);
                    cx.notify();
                });
                PinPropertyBar {
                    owner,
                    canvas,
                    _canvas_subscription: subscription,
                    anchor,
                    controls,
                    synced: None,
                    logical_size,
                    level,
                    placement: p,
                    popup_probe: Default::default(),
                    tool,
                    focus,
                    sync_pending: false,
                    popup: None,
                    popup_panel: ToolbarPanel::None,
                }
            });
            let weak = view.downgrade();
            let closed = w.window_handle().downcast::<PinPropertyBar>();
            w.on_window_should_close(cx, move |_, cx| {
                // Alt+F4 统一结束宿主的编辑状态，再通过 GPUI 回收整组窗口。
                // 返回 false，不能先由原生关闭留下仍可消费空格的文字草稿。
                let accepted = weak
                    .update(cx, |v, cx| {
                        v.canvas.update(cx, |canvas, cx| {
                            let accepted = canvas.finish_content_or_status();
                            if !accepted {
                                cx.notify();
                            }
                            accepted
                        })
                    })
                    .unwrap_or(true);
                if !accepted {
                    return false;
                }
                let weak = weak.clone();
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |v, cx| {
                        v.close_watermark_input(cx);
                        v.close_children(cx);
                        v.canvas.update(cx, |v, cx| {
                            v.toolbar_panel = ToolbarPanel::None;
                            cx.notify();
                        });
                    });
                    let _ = owner.update(cx, |v, w, cx| {
                        if v.chrome {
                            v.perform(PinAction::Chrome, w, cx);
                        }
                    });
                    if let Some(closed) = closed {
                        let _ = closed.update(cx, |_, w, _| w.remove_window());
                    }
                });
                false
            });
            view
        },
    )?)
}
impl PinPropertyBar {
    pub(crate) fn close_watermark_input(&mut self, cx: &mut App) {
        self.canvas.update(cx, |v, cx| v.drop_watermark_field(cx));
    }
    pub(crate) fn close_children(&mut self, cx: &mut App) {
        if let Some(popup) = self.popup.take() {
            let _ = popup.update(cx, |_, w, _| w.remove_window());
        }
        self.popup_panel = ToolbarPanel::None;
    }
    fn schedule_sync(&mut self, w: &Window, cx: &mut Context<Self>) {
        if self.sync_pending {
            return;
        }
        let state = self.sync_state(self.anchor, w, cx);
        if self.synced.as_ref() == Some(&state) && state.geometry.applied(self.placement) {
            return;
        }
        self.sync_pending = true;
        cx.defer_in(w, |v, w, cx| {
            v.sync_pending = false;
            v.sync(v.anchor, w, cx);
        });
    }
    fn sync_state(&self, anchor: PropertyBarAnchor, w: &Window, cx: &App) -> PropertySyncState {
        let v = self.canvas.read(cx);
        let viewport = w.viewport_size();
        PropertySyncState {
            controls: ControlState::capture(v),
            geometry: PropertyGeometry {
                anchor,
                bounds: native::rect(hwnd(w)).ok(),
                work_area: native::work_area((
                    anchor.bounds.x + anchor.bounds.width as i32 / 2,
                    anchor.bounds.y + anchor.bounds.height as i32 / 2,
                )),
                native_topmost: unsafe { GetWindowLongPtrW(hwnd(w), GWL_EXSTYLE) }
                    & WS_EX_TOPMOST.0 as isize
                    != 0,
                viewport: (f32::from(viewport.width), f32::from(viewport.height)),
                dpi: w.scale_factor(),
                popup_point: v
                    .ui_positions
                    .borrow()
                    .get(v.toolbar_panel.anchor())
                    .copied(),
                light_theme: crate::theme::is_light(cx),
                english: crate::i18n::is_english(),
            },
        }
    }
    fn popup_anchor_probe(&self) -> impl IntoElement {
        let canvas = self.canvas.clone();
        let previous = self.popup_probe.clone();
        gpui::canvas(
            move |_, w, cx| {
                let v = canvas.read(cx);
                let probe = PopupProbe {
                    panel: v.toolbar_panel,
                    point: v
                        .ui_positions
                        .borrow()
                        .get(v.toolbar_panel.anchor())
                        .copied(),
                };
                if previous.replace(Some(probe)) != Some(probe) {
                    // 共享控件的 probe 在本次 prepaint 更新后，再定位独立参数窗口。
                    if let Some(bar) = w.window_handle().downcast::<PinPropertyBar>() {
                        cx.defer(move |cx| {
                            let _ = bar.update(cx, |v, w, cx| v.schedule_sync(w, cx));
                        });
                    }
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
    pub(crate) fn sync(
        &mut self,
        anchor: PropertyBarAnchor,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.sync_state(anchor, w, cx);
        if self.synced.as_ref() == Some(&state) && state.geometry.applied(self.placement) {
            return;
        }
        let controls_changed = self.controls != state.controls;
        self.controls = state.controls.clone();
        self.synced = Some(state);
        let (logical_size, level) = dimensions(self.canvas.read(cx), anchor);
        let resized_content = self.logical_size != logical_size || self.level != level;
        self.logical_size = logical_size;
        self.level = level;
        let p = placement(logical_size, anchor);
        self.placement = p;
        let moved = native::rect(hwnd(w)).map_or(true, |current| current != p);
        let topmost_changed =
            (unsafe { GetWindowLongPtrW(hwnd(w), GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize != 0)
                != anchor.topmost;
        if moved || topmost_changed {
            native::place_later(hwnd(w), p, Some(anchor.topmost), false, cx);
        }
        self.anchor = anchor;
        let tool = active_tool(self.canvas.read(cx));
        unsafe {
            let h = hwnd(w);
            let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
            let next = if tool == Tool::Watermark {
                style & !(WS_EX_NOACTIVATE.0 as isize)
            } else {
                style | WS_EX_NOACTIVATE.0 as isize
            };
            if style != next {
                SetWindowLongPtrW(h, GWL_EXSTYLE, next);
            }
        }
        if tool != self.tool {
            self.tool = tool;
            self.close_children(cx);
            self.canvas.update(cx, |v, cx| {
                if !v.toolbar_panel.more() {
                    v.toolbar_panel = ToolbarPanel::None;
                    cx.notify();
                }
            });
        }
        self.sync_popup(w, cx);
        if moved || controls_changed || resized_content {
            cx.notify();
        }
    }
    fn sync_popup(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let v = self.canvas.read(cx);
        let panel = v.toolbar_panel;
        let point = v.ui_positions.borrow().get(panel.anchor()).copied();
        let editing = v.pin_editing;
        if !editing || panel == ToolbarPanel::None || panel.more() {
            self.close_children(cx);
            if !editing && panel != ToolbarPanel::None && !panel.more() {
                self.canvas.update(cx, |v, cx| {
                    v.toolbar_panel = ToolbarPanel::None;
                    cx.notify();
                });
            }
            return;
        }
        let Ok(bounds) = native::rect(hwnd(w)) else {
            return;
        };
        let dpi = w.scale_factor();
        let topmost = self.anchor.topmost;
        if panel == self.popup_panel {
            if let Some(popup) = self.popup {
                if popup
                    .update(cx, |v, w, cx| {
                        v.reposition(bounds, dpi, point, topmost, w, cx);
                        cx.notify();
                    })
                    .is_ok()
                {
                    return;
                }
                // 原生关闭之后的旧 handle 不能阻止相同菜单再次创建。
                self.popup = None;
            } else {
                return;
            }
        }
        self.close_children(cx);
        self.popup_panel = panel;
        match pin_properties::open(
            self.owner,
            self.canvas.clone(),
            panel,
            PopupAnchor {
                toolbar: hwnd(w),
                bounds,
                dpi,
                point,
                topmost,
            },
            cx,
        ) {
            Ok(popup) => self.popup = Some(popup),
            Err(error) => {
                let message = error.to_string();
                let _ = self.owner.update(cx, |v, _, cx| {
                    v.status = message;
                    cx.notify();
                });
            }
        }
    }
}
impl Render for PinPropertyBar {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.schedule_sync(w, cx);
        let viewport = w.viewport_size();
        let rect = PanelRect {
            x: 0.,
            y: 0.,
            width: f32::from(viewport.width),
            height: self.logical_size.1,
        };
        let palette = crate::theme::palette(cx);
        let level = self.level;
        // 在 canvas 的 Context 中构建，所有按钮与滚轮监听继续操作同一个文档。
        let content = self.canvas.update(cx, |v, cx| {
            let tool = active_tool(v);
            if tool == Tool::Watermark && visible(v) {
                v.ensure_watermark_field(w, cx);
            } else {
                v.drop_watermark_field(cx);
            }
            if visible(v) {
                v.property_toolbar(tool, rect, level, cx).into_any_element()
            } else {
                div()
                    .p_2()
                    .child(tr!(
                        "点击绘制工具，或选中标注以编辑属性",
                        "Choose a drawing tool, or select an annotation to edit its properties"
                    ))
                    .into_any_element()
            }
        });
        div()
            .id("pin-property-bar-root")
            .relative()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .font_family("Microsoft YaHei UI")
            .text_size(px(12.))
            .track_focus(&self.focus)
            .capture_any_mouse_down(cx.listener(|v, _, _, cx| {
                let _ = v
                    .owner
                    .update(cx, |owner, _, cx| owner.dismiss_context_menu(cx));
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|v, e: &gpui::KeyDownEvent, _, cx| {
                if v.canvas.read(cx).watermark_input_focused {
                    return;
                }
                let _ = v.owner.update(cx, |v, w, cx| v.key(e, w, cx));
            }))
            .child(
                div()
                    .relative()
                    .w(px(rect.width))
                    .h(px(rect.height))
                    .flex_shrink_0()
                    .child(content),
            )
            .child(self.popup_anchor_probe())
    }
}

#[cfg(test)]
mod control_geometry_tests {
    use super::{PopupProbe, PropertyBarAnchor, PropertyGeometry};
    use crate::{pin_model::Placement, toolbar_layout::ToolbarPanel};
    use std::cell::Cell;
    use windows::Win32::Foundation::HWND;

    #[test]
    fn pin_control_geometry_properties_keep_popup_changes_and_unapplied_native_retry() {
        let target = Placement {
            x: 100,
            y: 300,
            width: 400,
            height: 80,
        };
        let baseline = PropertyGeometry {
            anchor: PropertyBarAnchor {
                primary: HWND::default(),
                bounds: Placement { y: 200, ..target },
                dpi: 1.,
                topmost: true,
                above: false,
            },
            bounds: Some(target),
            work_area: Placement {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            native_topmost: true,
            viewport: (400., 80.),
            dpi: 1.,
            popup_point: None,
            light_theme: true,
            english: false,
        };
        assert!(baseline.applied(target));
        let changes: [fn(&mut PropertyGeometry); 11] = [
            |g| g.anchor.bounds.x += 1,
            |g| g.anchor.dpi = 1.5,
            |g| g.anchor.topmost = false,
            |g| g.anchor.above = true,
            |g| g.bounds.as_mut().unwrap().y += 1,
            |g| g.work_area.height -= 40,
            |g| g.viewport.0 = 300.,
            |g| g.dpi = 1.5,
            |g| g.popup_point = Some((48., 20.)),
            |g| g.light_theme = false,
            |g| g.english = true,
        ];
        for change in changes {
            let mut next = baseline;
            change(&mut next);
            assert_ne!(next, baseline);
        }
        let missing_bounds = PropertyGeometry {
            bounds: None,
            ..baseline
        };
        assert!(!missing_bounds.applied(target));
        let missed_topmost = PropertyGeometry {
            native_topmost: false,
            ..baseline
        };
        // 即使下次读取 stamp 完全相同，原生目标尚未到达也必须允许再同步。
        assert!(!missed_topmost.applied(target));
        assert!(!baseline.applied(Placement { x: 101, ..target }));

        let probe = Cell::new(None);
        let first = Some(PopupProbe {
            panel: ToolbarPanel::Width,
            point: Some((48., 20.)),
        });
        assert_ne!(probe.replace(first), first);
        assert_eq!(probe.replace(first), first);
        let wrapped = Some(PopupProbe {
            panel: ToolbarPanel::Width,
            point: Some((24., 50.)),
        });
        assert_ne!(probe.replace(wrapped), wrapped);
        assert_eq!(probe.replace(wrapped), wrapped);
    }
}
