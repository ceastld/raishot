//! 普通截图／图片编辑原位面板按钮；真实 Down 来源、焦点生命周期与单次 Click 认领。
use super::{inline_click, panel_host::PanelClickContext, *};
use crate::{annotation::Style, ocr_panel::PanelView, pin_view::find_ui::FindAction};
use gpui::{ClickEvent, DispatchPhase, FocusHandle, ScrollWheelEvent, Subscription};

#[derive(Clone, PartialEq)]
pub(super) enum PanelIntent {
    Operation(Operation),
    /// 底栏主复制按钮：复制成功后结束截图，失败保留会话供重试。
    CopyAndClose(Operation),
    Find(FindAction),
    View(PanelView),
    Menu(bool),
    FilterMenu(bool),
    Layer(bool),
    /// 译文页是否显示原文（持久化）。
    ShowSource(bool),
    Retry,
    /// 译文页按段落翻译全文（首次或重试）。
    TranslateAll,
    /// 表格页没有可信表格时放宽规则整理。
    ForceTable,
    Rows(Range<usize>),
    Filter(super::filter_host::FilterAction),
}

#[derive(Clone, PartialEq)]
struct PanelContext {
    filter_revision: u64,
    panel: PanelClickContext,
    find_open: bool,
    local_open: bool,
    translation_pending: bool,
    retry_available: bool,
    retry_busy: bool,
}

type PanelFrame = inline_click::ClickFrame<
    OcrTarget,
    crate::ocr_projection::OcrProjection,
    KeyboardSelection,
    Style,
    PanelIntent,
    PanelContext,
>;

struct PanelClick {
    frame: PanelFrame,
    focus: FocusHandle,
    _blur: Subscription,
    _activation: Subscription,
}

#[derive(Default)]
pub(super) struct PanelClickState {
    slot: inline_click::ClickSlot<PanelClick>,
}

impl PanelClickState {
    pub(super) fn cancel(&mut self) {
        self.slot.cancel();
    }
}

pub(crate) fn down_proof(event: &MouseDownEvent) -> inline_click::DownProof {
    let m = event.modifiers;
    inline_click::DownProof {
        left: event.button == MouseButton::Left,
        position: [
            f32::from(event.position.x).to_bits(),
            f32::from(event.position.y).to_bits(),
        ],
        modifiers: u8::from(m.control)
            | (u8::from(m.alt) << 1)
            | (u8::from(m.shift) << 2)
            | (u8::from(m.platform) << 3)
            | (u8::from(m.function) << 4),
        click_count: event.click_count,
        first_mouse: event.first_mouse,
    }
}

pub(crate) fn click_proof(event: &ClickEvent) -> (Option<inline_click::DownProof>, bool) {
    match event {
        ClickEvent::Mouse(mouse) => (
            Some(down_proof(&mouse.down)),
            mouse.down.button == MouseButton::Left && mouse.up.button == MouseButton::Left,
        ),
        ClickEvent::Keyboard(_) => (None, false),
    }
}

/// 必须在原位 canvas 的真实 paint 调用。Capture 先于内部 ScrollHandle 消费滚轮。
/// 任意新按下／键／滚动都撤销旧槽，随后控件自己的 Down 可重新 arm 新意图。
pub(super) fn observe_panel_click_input(slot: &Rc<RefCell<PanelClickState>>, window: &mut Window) {
    let weak = Rc::downgrade(slot);
    window.on_mouse_event(move |_: &ScrollWheelEvent, phase, _, _| {
        if phase == DispatchPhase::Capture {
            if let Some(slot) = weak.upgrade() {
                slot.borrow_mut().cancel();
            }
        }
    });
    let weak = Rc::downgrade(slot);
    window.on_mouse_event(move |_: &MouseDownEvent, phase, _, _| {
        if phase == DispatchPhase::Capture {
            if let Some(slot) = weak.upgrade() {
                slot.borrow_mut().cancel();
            }
        }
    });
    let weak = Rc::downgrade(slot);
    window.on_key_event(move |_: &KeyDownEvent, phase, _, _| {
        if phase == DispatchPhase::Capture {
            if let Some(slot) = weak.upgrade() {
                slot.borrow_mut().cancel();
            }
        }
    });
}

impl CaptureView {
    pub(crate) fn cancel_ocr_panel_click(&self) {
        if let Some(overlay) = &self.ocr_overlay {
            overlay.panel_click.borrow_mut().cancel();
        }
    }

    fn ocr_panel_click_frame(&self, intent: PanelIntent, window: &Window) -> Option<PanelFrame> {
        let overlay = self.ocr_overlay.as_ref()?;
        let (panel_epoch, panel) = overlay.panel.click_state();
        Some(PanelFrame {
            body: Arc::clone(&overlay.body_identity),
            body_version: self.ocr_find_result_identity(),
            projection_epoch: Arc::clone(&overlay.projection_epoch),
            panel_epoch,
            target: overlay.target.clone(),
            projection: self.ocr_projection(window)?,
            selection_generation: self.result_selection_generation(),
            selection: overlay.selected.clone(),
            cursor: overlay.keyboard_selection,
            color: overlay.color,
            style: self.editor.style,
            action: intent,
            context: PanelContext {
                filter_revision: overlay.filter.revision,
                panel,
                find_open: self.ocr_find.is_some(),
                local_open: self.local_ocr.is_some(),
                translation_pending: self.translation_pending(),
                retry_available: self.result_retry_available(),
                retry_busy: self.result_retry_busy(),
            },
        })
    }

    fn ocr_panel_click_access(&self, window: &Window, cx: &gpui::App, held: bool) -> bool {
        let capture = unsafe { GetCapture() };
        window.is_window_active()
            && self.ocr_find_focus_owned(window, cx)
            && !self.local_ocr_candidate_focused(window)
            && self.input.marked.is_none()
            && !self.ocr_find.as_ref().is_some_and(|ui| ui.composing(cx))
            && self.ocr_overlay_ready()
            && self.ocr_overlay_projection_painted(window)
            && self.result_selection_pointer_ready()
            && !self.result_selection_dragging(self.result_selection_generation())
            && self.text_copy_completion_is_idle()
            && !self.preview_text_replay
            && !self.color_picker_pending
            && (capture.0.is_null()
                || held
                    && capture == hwnd(window)
                    && crate::pin_view::find_ui::primary_button_down())
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay.anchor.is_none()
                    && overlay.capture.is_none()
                    && overlay.keyboard_target_painted()
                    && overlay.current(
                        self.current_ocr_target().as_ref(),
                        self.current_result_text(),
                    )
            })
    }

    fn ocr_panel_frame_current(
        &self,
        frame: &PanelFrame,
        window: &Window,
        cx: &gpui::App,
        held: bool,
    ) -> bool {
        self.ocr_panel_click_access(window, cx, held)
            && self
                .ocr_panel_click_frame(frame.action.clone(), window)
                .is_some_and(|current| &current == frame)
    }

    pub(super) fn refresh_ocr_panel_click(&self, window: &Window, cx: &gpui::App) {
        let Some(overlay) = &self.ocr_overlay else {
            return;
        };
        let invalid = overlay
            .panel_click
            .borrow()
            .slot
            .payload()
            .is_some_and(|click| {
                !click.focus.is_focused(window)
                    || !self.ocr_panel_frame_current(&click.frame, window, cx, true)
            });
        if invalid {
            overlay.panel_click.borrow_mut().cancel();
        }
    }

    fn begin_ocr_panel_click(
        &self,
        widget: &'static str,
        frame: Option<&PanelFrame>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_ocr_panel_click();
        let Some(frame) = frame else { return };
        if event.button != MouseButton::Left
            || event.first_mouse
            || !self.ocr_panel_frame_current(frame, window, cx, true)
        {
            return;
        }
        let Some(focus) = window.focused(cx) else {
            return;
        };
        let Some(overlay) = &self.ocr_overlay else {
            return;
        };
        let token = Arc::new(());
        let weak = Rc::downgrade(&overlay.panel_click);
        let blur_token = Arc::clone(&token);
        let blur = cx.on_blur(&focus, window, move |_, _, _| {
            if let Some(slot) = weak.upgrade() {
                slot.borrow_mut().slot.cancel_token(&blur_token);
            }
        });
        let weak = Rc::downgrade(&overlay.panel_click);
        let activation_token = Arc::clone(&token);
        let activation = cx.observe_window_activation(window, move |_, window, _| {
            if !window.is_window_active() {
                if let Some(slot) = weak.upgrade() {
                    slot.borrow_mut().slot.cancel_token(&activation_token);
                }
            }
        });
        overlay.panel_click.borrow_mut().slot.arm(
            token,
            widget,
            down_proof(event),
            PanelClick {
                frame: frame.clone(),
                focus,
                _blur: blur,
                _activation: activation,
            },
        );
    }

    fn claim_ocr_panel_click(
        &self,
        widget: &'static str,
        frame: Option<&PanelFrame>,
        event: &ClickEvent,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<PanelIntent> {
        let overlay = self.ocr_overlay.as_ref()?;
        let (down, left_pair) = click_proof(event);
        let claimed =
            overlay
                .panel_click
                .borrow_mut()
                .slot
                .claim(widget, down, left_pair, |saved| {
                    frame == Some(&saved.frame)
                        && saved.focus.is_focused(window)
                        && self.ocr_panel_frame_current(&saved.frame, window, cx, false)
                        && !crate::pin_view::find_ui::primary_button_down()
                });
        claimed.map(|click| click.frame.action)
    }

    pub(super) fn bind_ocr_panel_click(
        &self,
        button: gpui::Stateful<gpui::Div>,
        widget: &'static str,
        intent: PanelIntent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let frame = self.ocr_panel_click_frame(intent, window);
        let down = frame.clone();
        button
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    view.begin_ocr_panel_click(widget, down.as_ref(), event, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if let Some(intent) =
                    view.claim_ocr_panel_click(widget, frame.as_ref(), event, window, cx)
                {
                    if !matches!(intent, PanelIntent::Filter(_) | PanelIntent::FilterMenu(_)) {
                        if let Some(overlay) = &mut view.ocr_overlay {
                            overlay.panel.set_filter_open(false);
                        }
                    }
                    match intent {
                        PanelIntent::Operation(operation) => {
                            view.ocr_overlay_operation(operation, window, cx);
                        }
                        PanelIntent::CopyAndClose(operation) => {
                            view.ocr_overlay_copy_and_close(operation, window, cx);
                        }
                        PanelIntent::Find(action) => view.ocr_find_action(action, window, cx),
                        PanelIntent::View(mode) => view.set_ocr_panel_view(mode, cx),
                        PanelIntent::Menu(open) => {
                            if let Some(overlay) = &mut view.ocr_overlay {
                                overlay.panel.set_menu(open);
                            }
                            cx.notify();
                        }
                        PanelIntent::FilterMenu(open) => {
                            if let Some(overlay) = &mut view.ocr_overlay {
                                overlay.panel.set_filter_open(open);
                            }
                            cx.notify();
                        }
                        PanelIntent::Layer(visible) => {
                            if let Some(overlay) = &mut view.ocr_overlay {
                                overlay.panel.set_layer(visible);
                            }
                            cx.notify();
                        }
                        PanelIntent::ShowSource(show) => view.set_translation_show_source(show, cx),
                        PanelIntent::Retry => {
                            if let Some(overlay) = &mut view.ocr_overlay {
                                overlay.panel.close_menu();
                            }
                            view.retry_recognition_result(window, cx);
                        }
                        PanelIntent::TranslateAll => {
                            view.translate_ocr_full_text(cx);
                            cx.notify();
                        }
                        PanelIntent::ForceTable => {
                            view.force_table(window, cx);
                            cx.notify();
                        }
                        PanelIntent::Rows(_) => {}
                        PanelIntent::Filter(action) => view.ocr_filter_action(action, window, cx),
                    }
                }
            }))
    }

    /// 行选择在 Down 立即执行，也只认本次渲染的来源、原图投影与行范围；双击复制该行后关闭。
    pub(super) fn bind_ocr_panel_row_down(
        &self,
        row: gpui::Stateful<gpui::Div>,
        rows: Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let frame = self.ocr_panel_click_frame(PanelIntent::Rows(rows.clone()), window);
        row.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                view.cancel_ocr_panel_click();
                if !event.first_mouse
                    && frame
                        .as_ref()
                        .is_some_and(|frame| view.ocr_panel_frame_current(frame, window, cx, true))
                {
                    // Ctrl／Alt 点击列表行加入或移出整行多选；普通点击仍选中该行文字。
                    if event.modifiers.control || event.modifiers.alt {
                        view.ocr_filter_rows(rows.clone(), event.modifiers, window, cx);
                        return;
                    }
                    if event.click_count >= 2 && !event.modifiers.shift {
                        view.ocr_panel_copy_rows_and_close(rows.clone(), window, cx);
                    } else {
                        view.ocr_panel_select_rows(rows.clone(), event.modifiers.shift, window, cx);
                    }
                }
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_gpui_mouse_proof_keeps_negative_subpixel_position_and_all_down_fields() {
        let down = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(-30.25), px(12.5)),
            modifiers: gpui::Modifiers {
                control: true,
                alt: true,
                shift: true,
                platform: true,
                function: true,
            },
            click_count: 2,
            first_mouse: true,
        };
        let proof = down_proof(&down);
        assert_eq!(proof.position, [(-30.25f32).to_bits(), 12.5f32.to_bits()]);
        assert_eq!(proof.modifiers, 31);
        assert_eq!(proof.click_count, 2);
        assert!(proof.first_mouse);
        assert!(proof.left);
    }

    #[test]
    fn actual_gpui_click_mapping_requires_both_mouse_buttons_and_rejects_keyboard() {
        let down = MouseDownEvent {
            button: MouseButton::Left,
            ..Default::default()
        };
        let click = ClickEvent::Mouse(gpui::MouseClickEvent {
            down: down.clone(),
            up: MouseUpEvent {
                button: MouseButton::Left,
                ..Default::default()
            },
        });
        assert_eq!(click_proof(&click), (Some(down_proof(&down)), true));
        let mixed = ClickEvent::Mouse(gpui::MouseClickEvent {
            down,
            up: MouseUpEvent {
                button: MouseButton::Right,
                ..Default::default()
            },
        });
        assert!(!click_proof(&mixed).1);
        assert_eq!(click_proof(&ClickEvent::default()), (None, false));
    }
}
