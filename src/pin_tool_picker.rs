//! 临时绘制工具选择窗；只转发宿主命令，预览、参数与文档仍由同一 CaptureView 管理。
use crate::{
    annotation::Tool,
    capture_toolbar::{shortcut_hints, ui_kit, Palette},
    capture_view::{Action, CaptureView},
    gpui_capture::hwnd,
    pin_model::Placement,
    pin_native as native,
    pin_properties::{own_popup, popup_placement, PopupAnchor},
    pin_view::{PinAction, PinView},
    toolbar_icons,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{
    div, point, prelude::*, px, rgb, size, App, AppContext, Bounds, Context, Entity, FocusHandle,
    MouseButton, Render, SharedString, Window, WindowBounds, WindowHandle, WindowOptions,
};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy)]
enum Choice {
    Tool(Tool),
    CropCopy,
}
// 固定 UI 标识与截图工具一致；列表只负责可达性，不定义另一个工具或属性模型。
use crate::i18n::Text;
type ToolItem = (&'static str, Text, Tool);
type ToolGroup = (Text, &'static [ToolItem]);
const GROUPS: &[ToolGroup] = &[
    (
        Text("线条与形状", "Lines and shapes"),
        &[
            ("pen", Text("画笔", "Pen"), Tool::Pen),
            ("polyline", Text("折线", "Polyline"), Tool::Polyline),
            ("rectangle", Text("矩形", "Rectangle"), Tool::Rectangle),
            ("ellipse", Text("椭圆", "Ellipse"), Tool::Ellipse),
            ("triangle", Text("三角形", "Triangle"), Tool::Triangle),
            ("diamond", Text("菱形", "Diamond"), Tool::Diamond),
            ("star", Text("五角星", "Star"), Tool::Star),
            ("arrow", Text("箭头", "Arrow"), Tool::Arrow),
            ("serial", Text("序号", "Number"), Tool::Serial),
        ],
    ),
    (
        Text("文字与强调", "Text and emphasis"),
        &[
            ("text", Text("文字", "Text"), Tool::Text),
            ("watermark", Text("水印", "Watermark"), Tool::Watermark),
            (
                "highlighter",
                Text("荧光笔", "Highlighter"),
                Tool::Highlighter,
            ),
            (
                "highlighter_rect",
                Text("高亮框", "Highlight box"),
                Tool::HighlighterRect,
            ),
            ("spotlight", Text("聚光灯", "Spotlight"), Tool::Spotlight),
        ],
    ),
    (
        Text("遮挡与恢复", "Redact and restore"),
        &[
            ("mosaic", Text("马赛克", "Mosaic"), Tool::Mosaic),
            ("blur", Text("模糊", "Blur"), Tool::Blur),
            (
                "mosaic_brush",
                Text("打码笔", "Mosaic brush"),
                Tool::MosaicBrush,
            ),
            ("blur_brush", Text("模糊笔", "Blur brush"), Tool::BlurBrush),
            ("local-fill", Text("填充", "Fill"), Tool::LocalFill),
            (
                "pixel-eraser",
                Text("局部橡皮擦", "Pixel eraser"),
                Tool::PixelEraser,
            ),
            ("eraser", Text("对象橡皮擦", "Object eraser"), Tool::Eraser),
        ],
    ),
    (
        Text("编辑与裁剪", "Edit and crop"),
        &[
            ("edit", Text("对象编辑", "Edit objects"), Tool::Edit),
            ("lasso", Text("套索裁剪", "Lasso crop"), Tool::Lasso),
        ],
    ),
];
const SIZE: (f32, f32) = (356., 468.);

pub(crate) struct PinToolPicker {
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    focus: FocusHandle,
    activated: bool,
    pub(crate) positions:
        std::rc::Rc<std::cell::RefCell<std::collections::HashMap<&'static str, (f32, f32)>>>,
}
fn placement(anchor: PopupAnchor) -> Placement {
    popup_placement(anchor, SIZE, SIZE.0 / 2.)
}
pub(crate) fn open(
    owner: WindowHandle<PinView>,
    canvas: Entity<CaptureView>,
    anchor: PopupAnchor,
    cx: &mut App,
) -> Result<WindowHandle<PinToolPicker>> {
    let p = placement(anchor);
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
            w.set_window_title(brand_title!("更多绘制工具", "More drawing tools"));
            own_popup(w, toolbar);
            native::place_later(hwnd(w), p, Some(anchor.topmost), true, cx);
            let focus = cx.focus_handle();
            w.focus(&focus);
            let view = cx.new(|cx: &mut Context<PinToolPicker>| {
                cx.observe_in(&canvas, w, |v, canvas, w, cx| {
                    if canvas.read(cx).toolbar_panel != ToolbarPanel::Tools {
                        restore_canvas_focus(v.owner, cx);
                        w.remove_window();
                    } else {
                        cx.notify();
                    }
                })
                .detach();
                cx.observe_window_activation(w, |v, w, cx| {
                    if w.is_window_active() {
                        v.activated = true;
                    } else if v.activated {
                        v.dismiss(w, cx);
                    }
                })
                .detach();
                PinToolPicker {
                    owner,
                    canvas,
                    focus,
                    activated: false,
                    positions: Default::default(),
                }
            });
            let weak = view.downgrade();
            w.on_window_should_close(cx, move |_, cx| {
                restore_canvas_focus(owner, cx);
                let _ = weak.update(cx, |v, cx| v.clear_panel(cx));
                true
            });
            view
        },
    )?)
}
/// 仅在本贴图的 owned 窗口链内回到画布；切到外部应用时不抢焦点。
fn restore_canvas_focus(owner: WindowHandle<PinView>, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = owner.update(cx, |v, w, cx| {
            let Some(canvas) = &v.canvas else {
                return;
            };
            let editor = canvas.read(cx);
            if !editor.pin_editing || editor.toolbar_panel != ToolbarPanel::None {
                return;
            }
            let focus = editor.focus.clone();
            unsafe {
                if GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) != hwnd(w) {
                    return;
                }
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow(hwnd(w));
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd(w)));
            }
            w.focus(&focus);
            cx.notify();
        });
    });
}
impl PinToolPicker {
    fn clear_panel(&self, cx: &mut App) {
        self.canvas.update(cx, |v, cx| {
            if v.toolbar_panel == ToolbarPanel::Tools {
                v.toolbar_panel = ToolbarPanel::None;
                cx.notify();
            }
        });
    }
    pub(crate) fn dismiss(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.clear_panel(cx);
        self.close_window(w, cx);
    }
    pub(crate) fn close_window(&self, w: &mut Window, cx: &mut Context<Self>) {
        restore_canvas_focus(self.owner, cx);
        w.remove_window();
    }
    pub(crate) fn reposition(&self, anchor: PopupAnchor, w: &Window, cx: &App) {
        let p = placement(anchor);
        if native::rect(hwnd(w)).map_or(true, |current| current != p)
            || (unsafe { GetWindowLongPtrW(hwnd(w), GWL_EXSTYLE) } & WS_EX_TOPMOST.0 as isize != 0)
                != anchor.topmost
        {
            native::place_later(hwnd(w), p, Some(anchor.topmost), false, cx);
        }
    }
    fn choose(&mut self, choice: Choice, w: &mut Window, cx: &mut Context<Self>) {
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
        self.dismiss(w, cx);
        let _ = self.owner.update(cx, |v, w, cx| match choice {
            Choice::Tool(tool) => {
                if let Err(error) = v.set_canvas_active(true, w, cx) {
                    v.status = error.to_string();
                } else {
                    v.annotation(Action::Tool(tool), w, cx);
                }
            }
            Choice::CropCopy => v.perform(PinAction::Crop, w, cx),
        });
    }
    fn probe(&self, id: &'static str) -> impl IntoElement {
        let positions = self.positions.clone();
        gpui::canvas(
            move |bounds, _, _| {
                positions.borrow_mut().insert(
                    id,
                    (f32::from(bounds.center().x), f32::from(bounds.center().y)),
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
    fn item(
        &self,
        id: &'static str,
        label: &'static str,
        choice: Choice,
        active: bool,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let color = if active {
            palette.accent
        } else {
            palette.muted
        };
        let icon = match choice {
            Choice::Tool(tool) => toolbar_icons::icon(tool, color),
            Choice::CropCopy => toolbar_icons::icon(Tool::Crop, color),
        };
        let key = match choice {
            Choice::Tool(tool) => shortcut_hints::letter(tool, shortcut_hints::Context::Pin),
            Choice::CropCopy => None,
        };
        let hint: SharedString = match choice {
            Choice::Tool(tool) => shortcut_hints::tooltip(
                label,
                shortcut_hints::for_tool(
                    tool,
                    shortcut_hints::Context::Pin,
                    &self.canvas.read(cx).toolbar_config,
                ),
            )
            .into(),
            Choice::CropCopy => label.into(),
        };
        ui_kit::menu_row(
            palette,
            id,
            Some(icon),
            label,
            key.map(|key| ui_kit::trailing_text(palette, key)),
            active,
            true,
        )
        .tooltip(ui_kit::tip(hint))
        .child(self.probe(id))
        .on_click(cx.listener(move |v, _, w, cx| v.choose(choice, w, cx)))
    }
}
impl Render for PinToolPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.positions.borrow_mut().clear();
        let canvas = self.canvas.read(cx);
        let palette = crate::theme::palette(cx);
        let active_tool = canvas.pin_editing.then_some(canvas.editor.tool);
        let mut groups = div()
            .id("pin-tool-picker-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(ui_kit::MENU_PAD))
            .pb(px(ui_kit::MENU_PAD))
            .flex()
            .flex_col();
        for (index, &(title, items)) in GROUPS.iter().enumerate() {
            let mut cells = Vec::with_capacity(items.len() + 1);
            for &(id, label, tool) in items {
                cells.push(
                    self.item(
                        id,
                        label.get(),
                        Choice::Tool(tool),
                        active_tool == Some(tool),
                        palette,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            if index == GROUPS.len() - 1 {
                cells.push(
                    self.item(
                        "crop-copy",
                        tr!("裁剪副本…", "Crop a copy…"),
                        Choice::CropCopy,
                        false,
                        palette,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            groups = groups
                .child(ui_kit::section_title(palette, title.get()))
                .child(ui_kit::two_columns(cells));
        }
        div()
            .id("pin-tool-picker-root")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(palette.background))
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
            .on_key_down(cx.listener(|v, e: &gpui::KeyDownEvent, w, cx| {
                if e.keystroke.key == "escape" {
                    v.dismiss(w, cx);
                    cx.stop_propagation();
                } else {
                    let _ = v.owner.update(cx, |v, w, cx| v.key(e, w, cx));
                }
            }))
            .child(
                div()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(rgb(palette.border))
                    .child(tr!("更多绘制工具", "More drawing tools"))
                    .child(
                        div()
                            .id("close-picker")
                            .relative()
                            .size(px(24.))
                            .rounded_md()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .hover(move |s| s.bg(rgb(palette.hover)))
                            .child(
                                toolbar_icons::action(toolbar_icons::Icon::Close, palette.muted)
                                    .size(px(16.)),
                            )
                            .child(self.probe("close-picker"))
                            .on_click(cx.listener(|v, _, w, cx| v.dismiss(w, cx))),
                    ),
            )
            .child(groups)
            .child(
                div()
                    .h(px(26.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_size(px(10.))
                    .text_color(rgb(palette.muted))
                    .child(tr!("选中后返回贴图 · Esc 收起 · 参数在下方属性栏调整", "Returns to the pin after choosing · Esc closes · adjust settings in the property bar below")),
            )
    }
}
