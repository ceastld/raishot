//! 与贴图分开的浮窗：小贴图不裁切工具栏，按钮回到宿主窗口执行同一文档命令。
use crate::{
    annotation::Tool,
    capture_toolbar::{shortcut_hints, Palette},
    capture_view::{Action, CaptureView},
    gpui_capture::hwnd,
    pin_annotation::ControlState,
    pin_keys::toolbar_shortcuts,
    pin_model::Placement,
    pin_native as native,
    pin_properties::{PinPropertyPopup, PopupAnchor},
    pin_property_bar::{self, PinPropertyBar, PropertyBarAnchor},
    pin_tool_picker::{self, PinToolPicker},
    pin_toolbar_layout::PinToolbarLayout,
    pin_view::{PinAction, PinView},
    toolbar_config::ToolbarConfig,
    toolbar_icons::{self, Icon},
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{
    div, point, prelude::*, px, rgb, size, App, AppContext, Bounds, Context, Entity, FocusHandle,
    MouseButton, Render, SharedString, Subscription, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
#[derive(Clone, Copy)]
enum Command {
    Draw(Action),
    Pin(PinAction),
    Move,
    MoreTools,
}
pub(crate) struct PinToolbar {
    owner: WindowHandle<PinView>,
    native_window: HWND,
    _bounds_subscription: Option<Subscription>,
    last_visible_bounds: Option<Placement>,
    focus: FocusHandle,
    canvas: Option<Entity<CaptureView>>,
    canvas_subscription: Option<Subscription>,
    controls: Option<ControlState>,
    properties_geometry: Option<PropertiesGeometry>,
    pub(crate) properties: Option<WindowHandle<PinPropertyBar>>,
    pub(crate) tool_picker: Option<WindowHandle<PinToolPicker>>,
    properties_above: bool,
    properties_pending: bool,
    properties_failed: bool,
    tool_count: usize,
    pub(crate) positions:
        std::rc::Rc<std::cell::RefCell<std::collections::HashMap<&'static str, (f32, f32)>>>,
    picker_probe: std::rc::Rc<std::cell::Cell<Option<(f32, f32)>>>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct PropertiesGeometry {
    bounds: Option<Placement>,
    work_area: Option<Placement>,
    dpi: f32,
    topmost: bool,
    above: bool,
    picker_point: Option<(f32, f32)>,
    light_theme: bool,
    english: bool,
}
struct Hint(SharedString);
impl Render for Hint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::capture_toolbar::ui_kit::tip_body(crate::theme::palette(cx), &self.0)
    }
}
impl PinView {
    pub(crate) fn close_toolbar(&mut self, cx: &mut App) {
        self.dismiss_context_menu(cx);
        if let Some(bar) = self.toolbar.take() {
            cx.defer(move |cx| {
                let _ = bar.update(cx, |v, w, cx| {
                    v.close_children(cx);
                    if let Some(canvas) = &v.canvas {
                        canvas.update(cx, |v, cx| {
                            v.toolbar_panel = ToolbarPanel::None;
                            cx.notify();
                        });
                    }
                    w.remove_window();
                });
            });
        }
    }
    pub(crate) fn toggle_toolbar(&mut self, w: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        if self.chrome {
            self.set_canvas_active(false, w, cx)?;
            self.chrome = false;
            self.close_toolbar(cx);
        } else {
            if self.state.locked {
                self.status = tr!(
                    "已锁定，按 L 解锁后标注",
                    "Locked; press L to unlock before annotating"
                )
                .into();
                return Ok(());
            }
            self.cancel_drag(w);
            self.clear_text_selection();
            self.text_mode = false;
            self.end_color_pick(cx);
            self.status.clear();
            self.ensure_canvas(w, cx)?;
            // 每次重新展开读取一次设置，常驻绘制/render 不进行磁盘读取。
            if let Some(canvas) = &self.canvas {
                let config = ToolbarConfig::load();
                canvas.update(cx, |v, cx| {
                    v.toolbar_config = config;
                    cx.notify();
                });
            }
            let owner = w
                .window_handle()
                .downcast::<PinView>()
                .ok_or("贴图宿主不存在")?;
            let canvas = self.canvas.clone().ok_or("贴图标注画布不存在")?;
            // 子窗会在 open_window 内立即绘制；此时宿主仍被当前事件可变借用。
            // 直接传入已准备的画布，不能依赖子窗反向 read(owner) 的临时失败结果。
            self.toolbar = Some(open(owner, canvas, self.state.topmost, w, cx)?);
            self.chrome = true;
        }
        Ok(())
    }
    pub(crate) fn sync_toolbar(&self, w: &Window, cx: &mut App) {
        if let Some(bar) = self.toolbar {
            let owner = hwnd(w).0 as usize;
            let dpi = w.scale_factor();
            let topmost = self.state.topmost;
            cx.defer(move |cx| {
                let _ = bar.update(cx, |v, w, cx| {
                    if let Ok(p) = native::rect(HWND(owner as *mut _)) {
                        let count = v.canvas.as_ref().map_or(v.tool_count, |canvas| {
                            toolbar_shortcuts::visible_tools(&canvas.read(cx).toolbar_config).len()
                        });
                        v.tool_count = count;
                        let layout = placement(p, dpi, count);
                        v.properties_above = layout.properties_above;
                        let target = layout.primary;
                        if native::rect(hwnd(w)).is_ok_and(|current| current != target)
                            || (unsafe { GetWindowLongPtrW(hwnd(w), GWL_EXSTYLE) }
                                & WS_EX_TOPMOST.0 as isize
                                != 0)
                                != topmost
                        {
                            native::place_later(hwnd(w), target, Some(topmost), false, cx);
                        }
                    }
                    cx.notify();
                });
            });
        }
    }
}
fn placement(pin: Placement, dpi: f32, count: usize) -> PinToolbarLayout {
    let work = native::work_area((pin.x + pin.width as i32 / 2, pin.y + pin.height as i32 / 2));
    PinToolbarLayout::new_with_tool_count(pin, work, dpi, count)
}
fn open(
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    topmost: bool,
    w: &Window,
    cx: &mut App,
) -> Result<WindowHandle<PinToolbar>> {
    let owner_hwnd = hwnd(w).0 as usize;
    let tool_count = toolbar_shortcuts::visible_tools(&canvas.read(cx).toolbar_config).len();
    let dpi = w.scale_factor();
    let layout = placement(native::rect(hwnd(w))?, dpi, tool_count);
    let p = layout.primary;
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                point(px(p.x as f32 / dpi), px(p.y as f32 / dpi)),
                size(px(p.width as f32 / dpi), px(p.height as f32 / dpi)),
            ))),
            kind: gpui::WindowKind::PopUp,
            // 圆角外侧由桌面合成透明角区，不能用不透明 HWND 背景填回方角。
            window_background: gpui::WindowBackgroundAppearance::Transparent,
            is_resizable: false,
            is_minimizable: false,
            focus: false,
            // Windows 会先按 show 显示 HWND，再进入构造视图；先隐藏设置无激活样式。
            show: false,
            ..Default::default()
        },
        move |w, cx| {
            w.set_window_title(brand_title!("贴图标注工具栏", "Pin annotation toolbar"));
            let h = hwnd(w);
            unsafe {
                SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
                SetWindowLongPtrW(
                    h,
                    GWL_EXSTYLE,
                    GetWindowLongPtrW(h, GWL_EXSTYLE)
                        | (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0 as isize,
                );
                SetWindowLongPtrW(h, GWLP_HWNDPARENT, owner_hwnd as isize);
            }
            native::place_later(h, p, Some(topmost), true, cx);
            let focus = cx.focus_handle();
            w.focus(&focus);
            let view = cx.new(|cx| {
                let mut view = PinToolbar {
                    owner,
                    native_window: h,
                    _bounds_subscription: None,
                    last_visible_bounds: None,
                    focus,
                    canvas: None,
                    canvas_subscription: None,
                    controls: None,
                    properties_geometry: None,
                    properties: None,
                    tool_picker: None,
                    properties_above: layout.properties_above,
                    properties_pending: false,
                    properties_failed: false,
                    tool_count,
                    positions: Default::default(),
                    picker_probe: Default::default(),
                };
                view._bounds_subscription =
                    Some(cx.observe_window_bounds(w, |view: &mut PinToolbar, _, cx| {
                        view.notify_owner_geometry_change(cx);
                    }));
                view.bind_canvas(Some(canvas), w, cx);
                view
            });
            let weak = view.downgrade();
            let closed = w.window_handle().downcast::<PinToolbar>();
            w.on_window_should_close(cx, move |_, cx| {
                let accepted = weak
                    .update(cx, |v, cx| {
                        if v.canvas.as_ref().is_some_and(|canvas| {
                            !canvas.update(cx, |v, cx| {
                                let ok = v.finish_content_or_status();
                                if !ok {
                                    cx.notify();
                                }
                                ok
                            })
                        }) {
                            return false;
                        }
                        v.close_children(cx);
                        if let Some(canvas) = &v.canvas {
                            canvas.update(cx, |v, cx| {
                                v.toolbar_panel = ToolbarPanel::None;
                                cx.notify();
                            });
                        }
                        true
                    })
                    .unwrap_or(true);
                if !accepted {
                    return false;
                }
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |v, w, cx| {
                        if v.toolbar == closed {
                            if let Err(error) = v.set_canvas_active(false, w, cx) {
                                v.status = error.to_string();
                                cx.notify();
                                return;
                            }
                            v.toolbar = None;
                            v.chrome = false;
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
impl PinToolbar {
    /// 同实例持有的工具栏实际可见窗口矩形；只读，不移动、显示或请求焦点。
    pub(crate) fn visible_bounds(&self) -> Option<Placement> {
        unsafe { IsWindowVisible(self.native_window) }
            .as_bool()
            .then(|| native::rect(self.native_window).ok())
            .flatten()
    }

    fn notify_owner_geometry_change(&mut self, cx: &mut Context<Self>) {
        let current = self.visible_bounds();
        if self.last_visible_bounds == current {
            return;
        }
        self.last_visible_bounds = current;
        let owner = self.owner;
        // 原生SetWindowPos会同步回调；等当前窗口/实体借用结束后只让宿主重算遮挡。
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, _, cx| cx.notify());
        });
    }

    fn palette(&self, cx: &App) -> Palette {
        crate::theme::palette(cx)
    }
    fn command(&mut self, command: Command, w: &mut Window, cx: &mut Context<Self>) {
        if matches!(command, Command::MoreTools) {
            self.toggle_picker(w, cx);
            return;
        }
        if !matches!(command, Command::Pin(PinAction::Undo | PinAction::Redo)) {
            let allowed = self
                .owner
                .update(cx, |v, _, cx| {
                    if let Err(error) = v.prepare_canvas_local_fill(cx) {
                        v.status = error.to_string();
                        cx.notify();
                        false
                    } else {
                        true
                    }
                })
                .unwrap_or(false);
            if !allowed {
                return;
            }
        }
        self.close_picker(cx);
        let _ = self.owner.update(cx, |v, w, cx| match command {
            Command::Draw(a) => {
                if let Err(error) = v.set_canvas_active(true, w, cx) {
                    v.status = error.to_string();
                } else {
                    v.annotation(a, w, cx);
                }
            }
            Command::Pin(a) => v.perform(a, w, cx),
            Command::Move => {
                if let Err(error) = v.set_canvas_active(false, w, cx) {
                    v.status = error.to_string();
                }
            }
            Command::MoreTools => {}
        });
        cx.notify();
    }
    fn close_children(&mut self, cx: &mut App) {
        self.close_picker(cx);
        self.close_properties(cx);
    }
    fn close_picker(&mut self, cx: &mut App) {
        if let Some(picker) = self.tool_picker.take() {
            cx.defer(move |cx| {
                let _ = picker.update(cx, |v, w, cx| v.close_window(w, cx));
            });
        }
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, cx| {
                if v.toolbar_panel == ToolbarPanel::Tools {
                    v.toolbar_panel = ToolbarPanel::None;
                    cx.notify();
                }
            });
        }
    }
    fn picker_anchor(&self, w: &Window, cx: &App) -> Result<PopupAnchor> {
        Ok(PopupAnchor {
            toolbar: hwnd(w),
            bounds: native::rect(hwnd(w))?,
            dpi: w.scale_factor(),
            point: self.positions.borrow().get("more-tools").copied(),
            topmost: self.owner.read(cx).is_ok_and(|v| v.state.topmost),
        })
    }
    fn toggle_picker(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self
            .tool_picker
            .is_some_and(|picker| picker.read(cx).is_ok())
        {
            self.close_picker(cx);
            cx.notify();
            return;
        }
        self.tool_picker = None;
        let Some(canvas) = self.canvas.clone() else {
            return;
        };
        // 更多工具与参数菜单共用 Popup 状态，但不成为第三条常驻工具栏。
        if let Some(properties) = self.properties {
            let _ = properties.update(cx, |v, _, cx| v.close_children(cx));
        }
        canvas.update(cx, |v, cx| {
            v.toolbar_panel = ToolbarPanel::Tools;
            cx.notify();
        });
        let opened = self
            .picker_anchor(w, cx)
            .and_then(|anchor| pin_tool_picker::open(self.owner, canvas.clone(), anchor, cx));
        match opened {
            Ok(picker) => self.tool_picker = Some(picker),
            Err(error) => {
                canvas.update(cx, |v, cx| {
                    v.toolbar_panel = ToolbarPanel::None;
                    cx.notify();
                });
                let message = error.to_string();
                let _ = self.owner.update(cx, |v, _, cx| {
                    v.status = message;
                    cx.notify();
                });
            }
        }
        cx.notify();
    }
    fn sync_picker(&mut self, w: &Window, cx: &mut Context<Self>) {
        let visible = self
            .canvas
            .as_ref()
            .is_some_and(|canvas| canvas.read(cx).toolbar_panel == ToolbarPanel::Tools);
        if !visible {
            self.close_picker(cx);
            return;
        }
        if let (Some(picker), Ok(anchor)) = (self.tool_picker, self.picker_anchor(w, cx)) {
            if picker
                .update(cx, |v, w, cx| v.reposition(anchor, w, cx))
                .is_err()
            {
                self.tool_picker = None;
                if let Some(canvas) = &self.canvas {
                    canvas.update(cx, |v, cx| {
                        if v.toolbar_panel == ToolbarPanel::Tools {
                            v.toolbar_panel = ToolbarPanel::None;
                            cx.notify();
                        }
                    });
                }
            }
        }
    }
    fn close_properties(&mut self, cx: &mut App) {
        if let Some(properties) = self.properties.take() {
            cx.defer(move |cx| {
                let _ = properties.update(cx, |v, w, cx| {
                    v.close_watermark_input(cx);
                    v.close_children(cx);
                    w.remove_window();
                });
            });
        }
    }
    pub(crate) fn popup(&self, cx: &App) -> Option<WindowHandle<PinPropertyPopup>> {
        self.properties?.read(cx).ok()?.popup
    }
    fn schedule_properties(&mut self, w: &Window, cx: &mut Context<Self>) {
        if !self.properties_pending {
            self.properties_pending = true;
            cx.defer_in(w, |v, w, cx| {
                v.properties_pending = false;
                v.sync_properties(w, cx);
            });
        }
    }
    fn schedule_properties_if_changed(&mut self, w: &Window, cx: &mut Context<Self>) {
        let controls = self
            .canvas
            .as_ref()
            .map(|canvas| ControlState::capture(canvas.read(cx)));
        let bounds = native::rect(hwnd(w)).ok();
        let geometry = PropertiesGeometry {
            bounds,
            work_area: bounds.map(|bar| {
                native::work_area((bar.x + bar.width as i32 / 2, bar.y + bar.height as i32 / 2))
            }),
            dpi: w.scale_factor(),
            topmost: self.owner.read(cx).is_ok_and(|v| v.state.topmost),
            above: self.properties_above,
            picker_point: self.picker_probe.get(),
            light_theme: crate::theme::is_light(cx),
            english: crate::i18n::is_english(),
        };
        if self.controls != controls || self.properties_geometry != Some(geometry) {
            self.controls = controls;
            self.properties_geometry = Some(geometry);
            self.properties_failed = false;
            self.schedule_properties(w, cx);
        }
    }
    fn bind_canvas(
        &mut self,
        canvas: Option<Entity<CaptureView>>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.canvas.as_ref().map(Entity::entity_id) == canvas.as_ref().map(Entity::entity_id) {
            return;
        }
        self.close_children(cx);
        self.properties_failed = false;
        self.controls = None;
        self.properties_geometry = None;
        self.canvas_subscription = canvas.as_ref().map(|canvas| {
            cx.observe_in(canvas, w, |v, canvas, w, cx| {
                let controls = ControlState::capture(canvas.read(cx));
                if v.controls.as_ref() == Some(&controls) {
                    return;
                }
                v.controls = Some(controls);
                v.properties_failed = false;
                v.schedule_properties(w, cx);
                cx.notify();
            })
        });
        self.canvas = canvas;
    }
    fn sync_properties(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.sync_picker(w, cx);
        let Some(canvas) = self.canvas.clone() else {
            self.close_properties(cx);
            return;
        };
        let v = canvas.read(cx);
        if !pin_property_bar::visible(v) {
            // 二级栏只管理属性菜单，不能关闭一级栏的更多工具。
            if v.toolbar_panel != ToolbarPanel::None && !v.toolbar_panel.more() {
                canvas.update(cx, |v, cx| {
                    v.toolbar_panel = ToolbarPanel::None;
                    cx.notify();
                });
            }
            self.properties_failed = false;
            self.close_properties(cx);
            return;
        }
        let Ok(bounds) = native::rect(hwnd(w)) else {
            return;
        };
        let anchor = PropertyBarAnchor {
            primary: hwnd(w),
            bounds,
            dpi: w.scale_factor(),
            topmost: self.owner.read(cx).is_ok_and(|v| v.state.topmost),
            above: self.properties_above,
        };
        if let Some(properties) = self.properties {
            if properties
                .update(cx, |v, w, cx| v.sync(anchor, w, cx))
                .is_ok()
            {
                return;
            }
            self.properties = None;
        }
        if self.properties_failed {
            return;
        }
        match pin_property_bar::open(self.owner, canvas, anchor, cx) {
            Ok(properties) => self.properties = Some(properties),
            Err(error) => {
                self.properties_failed = true;
                let message = error.to_string();
                let _ = self.owner.update(cx, |v, _, cx| {
                    v.status = message;
                    cx.notify();
                });
            }
        }
    }
    fn probe(&self, id: &'static str) -> impl IntoElement {
        let positions = self.positions.clone();
        let picker_probe = self.picker_probe.clone();
        gpui::canvas(
            move |b, w, cx| {
                let point = (f32::from(b.center().x), f32::from(b.center().y));
                positions.borrow_mut().insert(id, point);
                if id == "more-tools" && picker_probe.replace(Some(point)) != Some(point) {
                    // 首次绘制/换行后的新锚点在 prepaint 才可用；无需靠下一次闲置 render 补同步。
                    if let Some(bar) = w.window_handle().downcast::<PinToolbar>() {
                        cx.defer(move |cx| {
                            let _ = bar.update(cx, |v, w, cx| {
                                v.schedule_properties_if_changed(w, cx);
                            });
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
    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        icon: Icon,
        command: Command,
        active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.tool_button(id, label, icon, command, active, None, true, cx)
    }
    #[allow(clippy::too_many_arguments)]
    fn tool_button(
        &self,
        id: &'static str,
        label: &'static str,
        icon: Icon,
        command: Command,
        active: bool,
        digit: Option<u8>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = self.palette(cx);
        let hint: SharedString = match command {
            Command::Draw(Action::Tool(tool)) => {
                let shortcut = self.canvas.as_ref().map_or_else(
                    || {
                        shortcut_hints::for_tool(
                            tool,
                            shortcut_hints::Context::Pin,
                            &ToolbarConfig::default(),
                        )
                    },
                    |canvas| {
                        shortcut_hints::for_tool(
                            tool,
                            shortcut_hints::Context::Pin,
                            &canvas.read(cx).toolbar_config,
                        )
                    },
                );
                shortcut_hints::tooltip(label, shortcut).into()
            }
            _ => label.into(),
        };
        div()
            .id(id)
            .relative()
            .group("pin-toolbar-button")
            .w(px(32.))
            .h(px(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .opacity(if enabled { 1. } else { 0.3 })
            .when(enabled, |d| {
                d.cursor_pointer().hover(move |s| s.bg(rgb(palette.hover)))
            })
            .bg(rgb(if active {
                palette.selected
            } else {
                palette.background
            }))
            .tooltip(move |_, cx| cx.new(|_| Hint(hint.clone())).into())
            .child(match command {
                Command::Draw(Action::Tool(tool)) => toolbar_icons::icon(
                    tool,
                    if active {
                        palette.accent
                    } else {
                        palette.foreground
                    },
                )
                .into_any_element(),
                _ => toolbar_icons::action(
                    icon,
                    if active {
                        palette.accent
                    } else {
                        palette.foreground
                    },
                )
                .into_any_element(),
            })
            .when_some(digit, |button, digit| {
                button.child(
                    // 与共享主栏一致：数字仅在本按钮悬浮时显示，不扩大命中或布局范围。
                    div()
                        .absolute()
                        .right(px(1.))
                        .bottom(px(0.))
                        .text_size(px(9.))
                        .line_height(px(10.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(palette.muted))
                        .opacity(0.)
                        .group_hover("pin-toolbar-button", |badge| badge.opacity(1.))
                        .child(digit.to_string()),
                )
            })
            .child(self.probe(id))
            .on_click(cx.listener(move |v, _, w, cx| {
                if enabled {
                    v.command(command, w, cx);
                }
            }))
    }
}
impl Render for PinToolbar {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 初次同步绘制或宿主重绘中 read 可能失败；失败不代表宿主移除了画布。
        // 仅成功读取后跟随真正的 Entity 更换（包括处理撤销边界的 None）。
        if let Ok(canvas) = self.owner.read(cx).map(|v| v.canvas.clone()) {
            self.bind_canvas(canvas, w, cx);
        }
        self.schedule_properties_if_changed(w, cx);
        let (tool, editing, configured) = self.canvas.as_ref().map_or_else(
            || {
                (
                    None,
                    false,
                    toolbar_shortcuts::visible_tools(&ToolbarConfig::default()),
                )
            },
            |c| {
                let v = c.read(cx);
                (
                    Some(v.editor.tool),
                    v.pin_editing,
                    toolbar_shortcuts::visible_tools(&v.toolbar_config),
                )
            },
        );
        if configured.len() != self.tool_count {
            self.tool_count = configured.len();
            // 设置改变按钮数量时由宿主在下一次安全消息边界调整整组 HWND。
            let _ = self.owner.update(cx, |v, w, cx| v.sync_toolbar(w, cx));
        }
        self.positions.borrow_mut().clear();
        self.notify_owner_geometry_change(cx);
        let palette = self.palette(cx);
        let mut tools = div()
            .id("pin-tools")
            .flex()
            .flex_wrap()
            .gap(px(2.))
            .items_center()
            .w_full()
            .flex_shrink_0();
        tools = tools.child(self.button(
            "move",
            tr!("拖动贴图", "Drag pin"),
            Icon::Grip,
            Command::Move,
            !editing,
            cx,
        ));
        tools = tools.child(self.tool_button(
            "select",
            tr!("编辑对象", "Edit objects"),
            Icon::Edit,
            Command::Draw(Action::Tool(Tool::Edit)),
            editing && tool == Some(Tool::Edit),
            Some(0),
            true,
            cx,
        ));
        for (index, &tool_kind) in configured.iter().enumerate() {
            let (id, label) = crate::capture_toolbar::tool_description(tool_kind);
            tools = tools.child(self.tool_button(
                id,
                label,
                Icon::Edit, // Draw 分支使用该工具的共享矢量图，不维护第二份图标映射。
                Command::Draw(Action::Tool(tool_kind)),
                editing && tool == Some(tool_kind),
                toolbar_shortcuts::number(index),
                true,
                cx,
            ));
        }
        tools = tools.child(self.button(
            "more-tools",
            tr!("更多绘制工具", "More drawing tools"),
            Icon::More,
            Command::MoreTools,
            self.canvas.as_ref().is_some_and(|v| {
                v.read(cx).toolbar_panel == ToolbarPanel::Tools
                    || editing && tool.is_some_and(|t| t != Tool::Edit && !configured.contains(&t))
            }),
            cx,
        ));
        tools = tools.child(
            div()
                .w(px(1.))
                .h(px(20.))
                .mx(px(3.))
                .flex_shrink_0()
                .bg(rgb(palette.border)),
        );
        for (id, label, icon, a) in [
            (
                "undo",
                tr!("撤销 · Ctrl+Z", "Undo · Ctrl+Z"),
                Icon::Undo,
                PinAction::Undo,
            ),
            (
                "redo",
                tr!("重做 · Ctrl+Y", "Redo · Ctrl+Y"),
                Icon::Redo,
                PinAction::Redo,
            ),
            (
                "copy",
                tr!("复制 · Ctrl+C", "Copy · Ctrl+C"),
                Icon::Copy,
                PinAction::Copy,
            ),
            (
                "save",
                tr!("保存 · Ctrl+S", "Save · Ctrl+S"),
                Icon::Save,
                PinAction::Save,
            ),
            (
                "done",
                tr!("完成标注 · 空格 / Esc", "Finish annotating · Space / Esc"),
                Icon::Confirm,
                PinAction::Chrome,
            ),
        ] {
            if id == "copy" {
                tools = tools.child(
                    div()
                        .w(px(1.))
                        .h(px(20.))
                        .mx(px(3.))
                        .flex_shrink_0()
                        .bg(rgb(palette.border)),
                );
            }
            // 读取失败（宿主重绘中）时保持可点，不误禁用。
            let enabled = match a {
                PinAction::Undo | PinAction::Redo => self.owner.read(cx).map_or(true, |v| {
                    v.history_available(matches!(a, PinAction::Redo), cx)
                }),
                _ => true,
            };
            tools = tools.child(self.tool_button(
                id,
                label,
                icon,
                Command::Pin(a),
                false,
                None,
                enabled,
                cx,
            ));
        }
        div()
            .id("pin-toolbar-root")
            .size_full()
            .p(px(7.))
            .flex()
            .flex_col()
            .items_center()
            // 极矮工作区会缩小一级 viewport；保留完整按钮行并允许滚动。
            .overflow_y_scroll()
            .bg(rgb(palette.background))
            .rounded_lg()
            .border_1()
            .border_color(rgb(palette.border))
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
                let _ = v.owner.update(cx, |v, w, cx| v.key(e, w, cx));
            }))
            .child(tools)
    }
}

#[cfg(test)]
mod control_geometry_tests {
    use super::PropertiesGeometry;
    use crate::pin_model::Placement;
    use std::cell::Cell;

    #[test]
    fn pin_control_geometry_primary_tracks_layout_theme_language_and_first_probe() {
        let baseline = PropertiesGeometry {
            bounds: Some(Placement {
                x: 100,
                y: 200,
                width: 500,
                height: 80,
            }),
            work_area: Some(Placement {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            dpi: 1.,
            topmost: true,
            above: false,
            picker_point: None,
            light_theme: true,
            english: false,
        };
        let changes: [fn(&mut PropertiesGeometry); 9] = [
            |g| g.bounds.as_mut().unwrap().x += 1,
            |g| g.bounds.as_mut().unwrap().height += 1,
            |g| g.work_area.as_mut().unwrap().height -= 40,
            |g| g.dpi = 1.5,
            |g| g.topmost = false,
            |g| g.above = true,
            |g| g.picker_point = Some((42., 18.)),
            |g| g.light_theme = false,
            |g| g.english = true,
        ];
        for change in changes {
            let mut next = baseline;
            change(&mut next);
            assert_ne!(next, baseline);
        }
        let probe = Cell::new(None);
        let first = Some((42., 18.));
        assert_ne!(probe.replace(first), first);
        assert_eq!(probe.replace(first), first);
        let wrapped = Some((10., 54.));
        assert_ne!(probe.replace(wrapped), wrapped);
        assert_eq!(probe.replace(wrapped), wrapped);
    }
}
