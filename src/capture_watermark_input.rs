//! 水印副栏复用同一个编辑器；这里只绑定输入、正文事件和自有窗口焦点。
use crate::{
    annotation::Tool,
    capture_view::CaptureView,
    gpui_capture::hwnd,
    text_field::TextField,
    watermark_input::{single_line, WatermarkFinishKey, WatermarkInputEvent},
};
use gpui::{AppContext, Context, Entity, Subscription, Window};
use windows::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{GetActiveWindow, GetFocus},
        WindowsAndMessaging::{
            GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, IsWindow, GA_ROOTOWNER,
        },
    },
};

impl CaptureView {
    fn activate_watermark_window(window: &mut Window) {
        let native = hwnd(window);
        // 实际前台、活动和系统焦点已经在此窗口时，只需更新文本逻辑焦点。
        // 重复 activate 会进入 GPUI Windows 的模拟 Alt 输入路径。
        let already_active = !native.0.is_null()
            && unsafe {
                let mut pid = 0;
                let tid = GetWindowThreadProcessId(native, Some(&mut pid));
                IsWindow(Some(native)).as_bool()
                    && pid == std::process::id()
                    && tid == GetCurrentThreadId()
                    && GetForegroundWindow() == native
                    && GetActiveWindow() == native
                    && GetFocus() == native
            };
        if !already_active {
            window.activate_window();
        }
    }
    fn watermark_edit_window_active(window: &Window) -> bool {
        if window.is_window_active() {
            return true;
        }
        // 自有字号弹窗和颜色对话框可获得原生焦点，仍属于同一编辑窗口组。
        unsafe {
            let foreground = GetForegroundWindow();
            let root = GetAncestor(hwnd(window), GA_ROOTOWNER);
            !foreground.0.is_null()
                && !root.0.is_null()
                && GetAncestor(foreground, GA_ROOTOWNER) == root
        }
    }

    fn watermark_field_focus_lost(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(field) = self.watermark_field.clone() {
            field.update(cx, |field, cx| field.cancel_watermark_composition(cx));
        }
        self.watermark_input_focused = false;
        if Self::watermark_edit_window_active(window) {
            if self.editor.watermark.has_preview() {
                let binding = self.editor.watermark.binding();
                self.editor.preview_watermark_text(binding, "", false);
            }
        } else {
            self.editor.finish_watermark_input();
        }
    }

    fn watermark_tool_active(&self) -> bool {
        self.editor
            .selected_annotation()
            .map_or(self.editor.tool, |annotation| annotation.tool)
            == Tool::Watermark
            && self.editor.selected_ids().len() <= 1
    }

    /// 其它宿主调用此同步入口，不在非输入窗口创建控件或抢焦点。
    pub(crate) fn sync_watermark_field(&mut self, cx: &mut Context<Self>) {
        if !self.watermark_tool_active() {
            self.drop_watermark_field(cx);
            return;
        }
        self.editor.sync_watermark_from_document();
        let selected = self.editor.selected.filter(|id| {
            self.editor
                .document
                .get(*id)
                .is_some_and(|annotation| annotation.tool == Tool::Watermark)
        });
        if selected.is_some() && selected != self.editor.watermark.target() {
            self.editor.activate_watermark(selected);
        }
        let binding = self.editor.watermark.binding();
        let revision = self.editor.document.revision();
        let rebound = self.watermark_binding != Some(binding);
        let changed = self.watermark_field_revision != Some(revision);
        if let Some(field) = self.watermark_field.clone() {
            let text = single_line(self.editor.watermark.committed_text());
            let content_changed = changed && field.read(cx).committed_text() != text.as_str();
            if rebound || content_changed {
                // 文档重绑先取消旧组合，再静默回填；旧 Preview 不能覆盖新的正文。
                field.update(cx, |field, cx| field.cancel_watermark_composition(cx));
                if rebound {
                    self.editor.finish_watermark_input();
                }
                if !field.read(cx).is_composing() && field.read(cx).text() != text.as_str() {
                    field.update(cx, |field, cx| field.set_text(text, cx));
                }
            } else if changed && field.read(cx).is_composing() {
                // 同目标颜色等修改保留 2 秒分组，并恢复仍有效的组合预览。
                let preview = field.read(cx).text().to_owned();
                self.editor.preview_watermark_text(binding, &preview, true);
            }
        }
        self.watermark_binding = Some(binding);
        self.watermark_field_revision = Some(revision);
    }

    /// 截图主窗或贴图二级栏的实际窗口每帧调用；只有第一次生成才激活。
    pub(crate) fn ensure_watermark_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_watermark_field(cx);
        if !self.watermark_tool_active() {
            return;
        }
        let input_window = window.window_handle();
        if self.watermark_field.is_some() && self.watermark_input_window != Some(input_window) {
            self.drop_watermark_field(cx);
            self.sync_watermark_field(cx);
        }
        if self.watermark_field.is_none() {
            let text = self.editor.watermark.committed_text().to_owned();
            let field = cx.new(|cx| {
                TextField::new_watermark(text, tr!("输入水印文字", "Enter watermark text"), cx)
            });
            let group = vec![field.downgrade()];
            field.update(cx, |field, cx| field.configure_focus(group, window, cx));
            let input_subscription = cx.subscribe_in(
                &field,
                window,
                |view, source, event: &WatermarkInputEvent, window, cx| {
                    view.watermark_field_event(source, event, window, cx);
                },
            );
            let activation_subscription =
                cx.observe_window_activation(window, |view, window, cx| {
                    if view.watermark_input_window != Some(window.window_handle())
                        || view.watermark_field.is_none()
                    {
                        return;
                    }
                    if !window.is_window_active() {
                        // 内部按钮已先 blur 时，仍需观察真正的窗口失活。
                        view.watermark_field_focus_lost(window, cx);
                    } else {
                        view.watermark_input_focused = view
                            .watermark_field
                            .as_ref()
                            .is_some_and(|field| field.read(cx).focus_handle.is_focused(window));
                    }
                    cx.notify();
                });
            self.watermark_subscription = Some(Subscription::new(move || {
                drop(input_subscription);
                drop(activation_subscription);
            }));
            self.watermark_field = Some(field.clone());
            self.watermark_input_window = Some(input_window);
            self.watermark_binding = Some(self.editor.watermark.binding());
            self.watermark_field_revision = Some(self.editor.document.revision());
            Self::activate_watermark_window(window);
            field.update(cx, |field, cx| field.focus_watermark(window, cx));
        }
        // 原生活跃状态可能晚于第一次 Focus 事件；后续只观察，不重复激活。
        let focused = window.is_window_active()
            && self
                .watermark_field
                .as_ref()
                .is_some_and(|field| field.read(cx).focus_handle.is_focused(window));
        if (self.watermark_input_focused && !focused) || !Self::watermark_edit_window_active(window)
        {
            self.watermark_field_focus_lost(window, cx);
        }
        self.watermark_input_focused = focused;
    }

    fn watermark_field_event(
        &mut self,
        source: &Entity<TextField>,
        event: &WatermarkInputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .watermark_field
            .as_ref()
            .is_none_or(|field| field.entity_id() != source.entity_id())
            || self.watermark_input_window != Some(window.window_handle())
        {
            return;
        }
        self.sync_watermark_field(cx);
        if !self.watermark_tool_active() || self.watermark_field.is_none() {
            return;
        }
        let focused = window.is_window_active() && source.read(cx).focus_handle.is_focused(window);
        if let WatermarkInputEvent::FocusChanged { focused: gained } = event {
            // 迟到的 gained/lost 通知不能取代窗口当前真实焦点。
            if *gained != focused {
                return;
            }
            self.watermark_input_focused = focused;
            if !self.watermark_input_focused {
                self.watermark_field_focus_lost(window, cx);
            }
            cx.notify();
            return;
        }
        if !focused {
            self.watermark_field_focus_lost(window, cx);
            cx.notify();
            return;
        }
        let Some(binding) = self.watermark_binding else {
            return;
        };
        if binding != self.editor.watermark.binding() {
            return;
        }
        match event {
            WatermarkInputEvent::Preview { text, composing } => {
                if source.read(cx).text() != text.as_str()
                    || source.read(cx).is_composing() != *composing
                {
                    return;
                }
                self.editor
                    .preview_watermark_text(binding, text, *composing);
            }
            WatermarkInputEvent::Committed { text } => {
                if source.read(cx).committed_text() != text.as_str() {
                    return;
                }
                self.editor.set_watermark_text(binding, text);
                // 首次非空输入创建对象后，ACK 将 None 目标换为新的 ObjectId。
                self.watermark_binding = Some(self.editor.watermark.binding());
                self.watermark_field_revision = Some(self.editor.document.revision());
            }
            WatermarkInputEvent::Finish { key } => match key {
                WatermarkFinishKey::Enter | WatermarkFinishKey::Escape => {
                    self.finish_watermark_field(window, cx);
                }
            },
            WatermarkInputEvent::FocusChanged { .. } => unreachable!(),
        }
        cx.notify();
    }

    pub(crate) fn finish_watermark_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(field) = self.watermark_field.clone() {
            field.update(cx, |field, cx| field.cancel_watermark_composition(cx));
        }
        self.editor.finish_watermark_input();
        self.watermark_binding = Some(self.editor.watermark.binding());
        self.watermark_field_revision = Some(self.editor.document.revision());
        self.watermark_input_focused = false;
        if self.pin_mode {
            window.blur();
            if let Some(pin_window) = self.pin_window {
                let canvas_id = cx.entity().entity_id();
                cx.defer(move |cx| {
                    let _ = pin_window.update(cx, |view, window, cx| {
                        if view.chrome
                            && view
                                .canvas
                                .as_ref()
                                .is_some_and(|canvas| canvas.entity_id() == canvas_id)
                        {
                            Self::activate_watermark_window(window);
                            window.focus(&view.focus);
                            cx.notify();
                        }
                    });
                });
            }
        } else {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    pub(crate) fn clear_watermark_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_watermark_field(window, cx);
        let Some(field) = self.watermark_field.clone() else {
            return;
        };
        field.update(cx, |field, cx| {
            field.cancel_watermark_composition(cx);
            field.set_text(String::new(), cx);
        });
        let binding = self.editor.watermark.binding();
        self.editor.set_watermark_text(binding, "");
        self.watermark_binding = Some(self.editor.watermark.binding());
        self.watermark_field_revision = Some(self.editor.document.revision());
        Self::activate_watermark_window(window);
        field.update(cx, |field, cx| field.focus_watermark(window, cx));
        self.watermark_input_focused = window.is_window_active();
        cx.notify();
    }

    pub(crate) fn drop_watermark_field(&mut self, cx: &mut Context<Self>) {
        self.watermark_subscription = None;
        let field = self.watermark_field.take();
        let had_binding = self.watermark_binding.take().is_some();
        self.watermark_field_revision = None;
        self.watermark_input_focused = false;
        self.watermark_input_window = None;
        let had_field = field.is_some();
        if let Some(field) = field {
            field.update(cx, |field, cx| field.cancel_watermark_composition(cx));
        }
        if had_field || had_binding || self.editor.watermark.has_preview() {
            self.editor.finish_watermark_input();
        }
    }
}
