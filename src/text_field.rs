//! 小型单行文本控件；保留中文 IME、UTF-16 选区和鼠标定位，不依赖标注文档。
use crate::text_input::edit_boundary;
use crate::watermark_input::{
    single_line, WatermarkFinishKey, WatermarkInputEvent, WatermarkInputState,
};
use gpui::{
    div, fill, point, prelude::*, px, relative, rgb, rgba, size, App, Bounds, ClipboardItem,
    Context, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    PaintQuad, Pixels, Point, Render, ShapedLine, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, WeakEntity, Window,
};
use std::ops::Range;

pub(crate) enum TextFieldEvent {
    Changed,
    Submit,
}
/// 仅 opt-in 设置字段保存组合前的正文和反向选区；普通输入器协议不变。
#[derive(Default)]
struct SettingsComposition {
    before: Option<(String, Range<usize>, bool)>,
    placeholder: Option<crate::i18n::Text>,
}
impl SettingsComposition {
    fn begin(&mut self, content: &str, selection: Range<usize>, reversed: bool) {
        if self.before.is_none() {
            self.before = Some((content.to_owned(), selection, reversed));
        }
    }
    fn cancel(&mut self) -> Option<(String, Range<usize>, bool)> {
        self.before.take()
    }
    fn confirm(&mut self) {
        self.before = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsKeyAction {
    Pass,
    Consume,
    Tab { backwards: bool },
}
fn settings_key_action(
    key: &str,
    modifiers: gpui::Modifiers,
    held: bool,
    composing: bool,
    focused_active: bool,
) -> SettingsKeyAction {
    if composing {
        return SettingsKeyAction::Consume;
    }
    if key != "tab"
        || modifiers.control
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
    {
        return SettingsKeyAction::Pass;
    }
    if held || !focused_active {
        SettingsKeyAction::Consume
    } else {
        SettingsKeyAction::Tab {
            backwards: modifiers.shift,
        }
    }
}

/// 三字段按显式组顺序导航，忽略不可用字段。
fn settings_tab_target(enabled: &[bool], current: usize, backwards: bool) -> Option<usize> {
    if current >= enabled.len() {
        return None;
    }
    let count = enabled.len();
    (1..count)
        .map(|step| {
            if backwards {
                (current + count - step) % count
            } else {
                (current + step) % count
            }
        })
        .find(|&index| enabled[index])
}

/// 取消组合后内部 focus 可仍保留；迟到原生回调不能再突变恢复后的设置草稿。
fn settings_native_input_allowed(window_active: bool, own_focus: bool) -> bool {
    window_active && own_focus
}

pub(crate) struct TextField {
    pub(crate) focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    drag_anchor: Option<usize>,
    last_line: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    enabled: bool,
    focus_group: Vec<WeakEntity<TextField>>,
    watermark: Option<WatermarkInputState>,
    settings: Option<SettingsComposition>,
    /// 无框紧凑样式：外框由宿主绘制（如长图剪辑的四边数值格）。
    bare: bool,
    _focus_subscriptions: Vec<gpui::Subscription>,
}
impl EventEmitter<TextFieldEvent> for TextField {}
impl EventEmitter<WatermarkInputEvent> for TextField {}
impl TextField {
    fn native_settings_input_allowed(&self, window: &Window) -> bool {
        // 默认 new/new_watermark 不进入新门；内部按键删除仍直接调用原 replace。
        self.settings.is_none()
            || settings_native_input_allowed(
                window.is_window_active(),
                self.focus_handle.is_focused(window),
            )
    }
    pub(crate) fn new(text: String, placeholder: &str, cx: &mut Context<Self>) -> Self {
        let len = text.len();
        Self {
            focus_handle: cx.focus_handle(),
            content: text,
            placeholder: placeholder.to_owned().into(),
            selection: 0..len,
            reversed: false,
            marked: None,
            drag_anchor: None,
            last_line: None,
            last_bounds: None,
            scroll_x: px(0.),
            enabled: true,
            focus_group: Vec::new(),
            watermark: None,
            settings: None,
            bare: false,
            _focus_subscriptions: Vec::new(),
        }
    }
    pub(crate) fn bare(mut self) -> Self {
        self.bare = true;
        self
    }
    /// 36px 设置字段仍用 TextFieldEvent 和 Enter Submit；仅组合取消与 Tab 为 opt-in。
    pub(crate) fn new_settings(
        text: String,
        placeholder: crate::i18n::Text,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut field = Self::new(text, placeholder.get(), cx);
        field.settings = Some(SettingsComposition {
            placeholder: Some(placeholder),
            ..Default::default()
        });
        field.focus_handle = field.focus_handle.tab_stop(true);
        field
    }
    /// 仅水印使用独立输入事件；现有参数框继续使用 new / TextFieldEvent。
    pub(crate) fn new_watermark(text: String, placeholder: &str, cx: &mut Context<Self>) -> Self {
        let text = single_line(&text);
        let watermark = WatermarkInputState::new(&text);
        let mut field = Self::new(text, placeholder, cx);
        field.watermark = Some(watermark);
        field
    }
    pub(crate) fn text(&self) -> &str {
        &self.content
    }
    pub(crate) fn committed_text(&self) -> &str {
        self.watermark
            .as_ref()
            .map_or(self.content.as_str(), WatermarkInputState::committed_text)
    }
    pub(crate) fn is_composing(&self) -> bool {
        self.marked.is_some()
    }
    pub(crate) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.enabled = enabled;
        self.drag_anchor = None;
        cx.notify();
    }
    pub(crate) fn configure_focus(
        &mut self,
        group: Vec<WeakEntity<TextField>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_group = group;
        self._focus_subscriptions
            .push(cx.on_blur(&self.focus_handle, window, |v, _, cx| {
                v.cancel_composition(cx);
                v.watermark_focus_changed(false, cx);
            }));
        if self.watermark.is_some() {
            self._focus_subscriptions
                .push(cx.on_focus(&self.focus_handle, window, |v, _, cx| {
                    v.watermark_focus_changed(true, cx);
                }));
        }
        self._focus_subscriptions
            .push(cx.observe_window_activation(window, |v, window, cx| {
                if !window.is_window_active() {
                    v.cancel_composition(cx);
                    v.watermark_focus_changed(false, cx);
                } else if v.focus_handle.is_focused(window) {
                    v.watermark_focus_changed(true, cx);
                }
            }));
    }
    fn watermark_focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self
            .watermark
            .as_mut()
            .is_some_and(|state| state.focus_changed(focused))
        {
            cx.emit(WatermarkInputEvent::FocusChanged { focused });
        }
    }
    /// 宿主负责原生窗口激活；此方法只设置该窗口内的输入焦点并全选。
    pub(crate) fn focus_watermark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.watermark.is_none() || !self.enabled {
            return;
        }
        window.focus(&self.focus_handle);
        if self.marked.is_none() {
            self.selection = 0..self.content.len();
            self.reversed = false;
            self.drag_anchor = None;
        }
        self.watermark_focus_changed(true, cx);
        cx.notify();
    }
    pub(crate) fn cancel_watermark_composition(&mut self, cx: &mut Context<Self>) {
        if self.watermark.is_some() {
            self.cancel_composition(cx);
        }
    }
    fn cancel_composition(&mut self, cx: &mut Context<Self>) {
        self.drag_anchor = None;
        if let Some(state) = self.watermark.as_mut() {
            if let Some((content, selection, reversed)) = state.cancel_composition() {
                self.content = content;
                self.selection = selection;
                self.reversed = reversed;
                self.marked = None;
                self.last_line = None;
                cx.emit(WatermarkInputEvent::Preview {
                    text: self.content.clone(),
                    composing: false,
                });
                cx.emit(TextFieldEvent::Changed);
                cx.notify();
            }
            return;
        }
        if let Some(settings) = self.settings.as_mut() {
            if let Some((content, selection, reversed)) = settings.cancel() {
                self.content = content;
                self.selection = selection;
                self.reversed = reversed;
                self.marked = None;
                self.last_line = None;
                cx.emit(TextFieldEvent::Changed);
                cx.notify();
            }
            return;
        }
        if let Some(marked) = self.marked.take() {
            self.content.replace_range(marked.clone(), "");
            self.selection = marked.start..marked.start;
            self.reversed = false;
            self.last_line = None;
            cx.emit(TextFieldEvent::Changed);
            cx.notify();
        }
    }
    /// 程序更新不发送 Changed，避免模板自动生成名称被当作用户手改。
    pub(crate) fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.content = if self.watermark.is_some() {
            single_line(&text)
        } else {
            text
        };
        if let Some(state) = self.watermark.as_mut() {
            state.set_text(&self.content);
        }
        if let Some(settings) = self.settings.as_mut() {
            settings.confirm();
        }
        self.selection = self.content.len()..self.content.len();
        self.marked = None;
        self.reversed = false;
        self.drag_anchor = None;
        self.last_line = None;
        cx.notify();
    }
    pub(crate) fn insert(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_some() {
            return;
        }
        self.replace(None, text, false, None, cx);
        window.focus(&self.focus_handle);
        self.watermark_focus_changed(true, cx);
    }
    fn caret(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }
    fn select_to(&mut self, anchor: usize, caret: usize) {
        self.selection = anchor.min(caret)..anchor.max(caret);
        self.reversed = caret < anchor;
    }
    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let (start, end) = (
            byte_offset(&self.content, range.start),
            byte_offset(&self.content, range.end),
        );
        start.min(end)..start.max(end)
    }
    fn replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        marked: bool,
        selected: Option<Range<usize>>,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            return;
        }
        let range = range
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        let text: String = if self.watermark.is_some() {
            single_line(text)
        } else {
            text.chars()
                .filter(|c| !c.is_control())
                .take(4096)
                .collect()
        };
        // Windows 将取消候选送成空 replace；不能提交空值或删掉组合前的选中文本。
        if self.watermark.is_some() && text.is_empty() {
            if self
                .watermark
                .as_ref()
                .is_some_and(WatermarkInputState::is_composing)
            {
                self.cancel_composition(cx);
                return;
            }
            if marked {
                return;
            }
        }
        // 普通设置字段取消候选不能把被替换的原文发布成空值。
        if self.settings.is_some() && text.is_empty() {
            if self.marked.is_some() {
                self.cancel_composition(cx);
                return;
            }
            if marked {
                return;
            }
        }
        let capacity = 4096usize.saturating_sub(
            self.content.chars().count() - self.content[range.clone()].chars().count(),
        );
        if marked && text.chars().count() > capacity {
            // 输入法组合不截断，避免候选串被拆开。
            return;
        }
        // 普通粘贴超长时截到剩余容量，不静默整段丢弃。
        let fitted = edit_boundary::fit_insert(&text, capacity);
        if fitted.is_empty() && !text.is_empty() {
            return;
        }
        let text = fitted.to_owned();
        if marked {
            if let Some(state) = self.watermark.as_mut() {
                state.begin_composition(self.selection.clone(), self.reversed);
            }
        }
        if let Some(settings) = self.settings.as_mut() {
            if marked {
                settings.begin(&self.content, self.selection.clone(), self.reversed);
            } else {
                settings.confirm();
            }
        }
        self.content.replace_range(range.clone(), &text);
        self.selection = selected
            .map(|r| {
                let (start, end) = (byte_offset(&text, r.start), byte_offset(&text, r.end));
                range.start + start.min(end)..range.start + start.max(end)
            })
            .unwrap_or(range.start + text.len()..range.start + text.len());
        self.marked = (marked && !text.is_empty()).then_some(range.start..range.start + text.len());
        self.reversed = false;
        self.drag_anchor = None;
        self.last_line = None;
        self.emit_watermark_change(marked, cx);
        cx.emit(TextFieldEvent::Changed);
        cx.notify();
    }
    fn emit_watermark_change(&mut self, composing: bool, cx: &mut Context<Self>) {
        let Some(state) = self.watermark.as_mut() else {
            return;
        };
        let committed = !composing && state.confirm(&self.content);
        cx.emit(WatermarkInputEvent::Preview {
            text: self.content.clone(),
            composing,
        });
        if committed {
            cx.emit(WatermarkInputEvent::Committed {
                text: self.content.clone(),
            });
        }
    }
    fn key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = e.keystroke.key.as_str();
        let m = e.keystroke.modifiers;
        let settings_action = self.settings.as_ref().map(|_| {
            settings_key_action(
                key,
                m,
                e.is_held,
                self.marked.is_some(),
                window.is_window_active() && self.focus_handle.is_focused(window),
            )
        });
        if settings_action == Some(SettingsKeyAction::Consume) {
            cx.stop_propagation();
            return;
        }
        // 合成期间交给原生输入法；重复确认也不能抢先结束候选输入。
        if self.marked.is_some() {
            if self.watermark.is_some() {
                // 正常原生路径已交给 IME；直接转发也不能继续到宿主的 Copy / Close。
                cx.stop_propagation();
            }
            return;
        }
        if crate::text_contract::terminal_held(key, e.is_held) {
            cx.stop_propagation();
            return;
        }
        if !self.enabled {
            cx.stop_propagation();
            return;
        }
        if let Some(SettingsKeyAction::Tab { backwards }) = settings_action {
            let own = cx.entity().entity_id();
            let current = self
                .focus_group
                .iter()
                .position(|field| field.entity_id() == own);
            let members: Vec<_> = self
                .focus_group
                .iter()
                .map(|field| {
                    if field.entity_id() == own {
                        None
                    } else {
                        field.upgrade()
                    }
                })
                .collect();
            let enabled: Vec<_> = members
                .iter()
                .map(|field| field.as_ref().is_some_and(|field| field.read(cx).enabled))
                .collect();
            let composing = members
                .iter()
                .filter_map(Option::as_ref)
                .any(|field| field.read(cx).is_composing());
            if let Some(index) = current
                .filter(|_| !composing)
                .and_then(|current| settings_tab_target(&enabled, current, backwards))
            {
                if let Some(field) = &members[index] {
                    let focus = field.read(cx).focus_handle.clone();
                    window.focus(&focus);
                }
            }
            cx.stop_propagation();
            return;
        }
        if self.watermark.is_some() && !m.alt && !m.platform {
            let finish = match key {
                "enter" => Some(WatermarkFinishKey::Enter),
                "escape" => Some(WatermarkFinishKey::Escape),
                _ => None,
            };
            if let Some(key) = finish {
                cx.emit(WatermarkInputEvent::Finish { key });
                cx.stop_propagation();
                return;
            }
        }
        match key {
            "enter" => cx.emit(TextFieldEvent::Submit),
            "a" if m.control => {
                self.selection = 0..self.content.len();
                self.reversed = false;
            }
            "c" | "x" if m.control => {
                let range = if key == "x" && !self.selection.is_empty() {
                    edit_boundary::deletion_range(&self.content, self.selection.clone(), false)
                } else {
                    self.selection.clone()
                };
                let published =
                    edit_boundary::publish_selected(&self.content, range.clone(), |selected| {
                        cx.write_to_clipboard(ClipboardItem::new_string(selected.to_owned()))
                    });
                if key == "x" && published {
                    self.replace(
                        Some(
                            utf_offset(&self.content, range.start)
                                ..utf_offset(&self.content, range.end),
                        ),
                        "",
                        false,
                        None,
                        cx,
                    );
                }
            }
            "v" if m.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.replace(None, &text, false, None, cx);
                }
            }
            "backspace" | "delete" => {
                let range = edit_boundary::deletion_range(
                    &self.content,
                    self.selection.clone(),
                    key == "backspace",
                );
                if !range.is_empty() {
                    self.selection = range;
                    self.replace(None, "", false, None, cx);
                }
            }
            "left" | "right" | "home" | "end" => {
                // 单行字段 Home/End 始终指向整段两端，原来的普通 Enter/水印合同不变。
                let (anchor, caret) = edit_boundary::navigate(
                    &self.content,
                    self.selection.clone(),
                    self.reversed,
                    key,
                    m.shift,
                    true,
                );
                self.select_to(anchor, caret);
            }
            _ => return,
        }
        let _ = window;
        cx.stop_propagation();
        cx.notify();
    }
    fn hit(&self, point: Point<Pixels>) -> usize {
        match (&self.last_line, self.last_bounds) {
            (Some(line), Some(bounds))
                if !self.content.is_empty() && line.text.as_ref() == self.content.as_str() =>
            {
                edit_boundary::floor(
                    &self.content,
                    line.closest_index_for_x(point.x - bounds.left() + self.scroll_x),
                )
            }
            _ => 0,
        }
    }
}
fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units + c.len_utf16() > utf16 {
            return byte;
        }
        units += c.len_utf16();
    }
    text.len()
}
fn floor_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}
fn utf_offset(text: &str, byte: usize) -> usize {
    text[..floor_boundary(text, byte)].encode_utf16().count()
}
impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(range);
        *adjusted =
            Some(utf_offset(&self.content, range.start)..utf_offset(&self.content, range.end));
        Some(self.content[range].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if !self.enabled {
            return None;
        }
        Some(UTF16Selection {
            range: utf_offset(&self.content, self.selection.start)
                ..utf_offset(&self.content, self.selection.end),
            reversed: self.reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|r| utf_offset(&self.content, r.start)..utf_offset(&self.content, r.end))
    }
    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.native_settings_input_allowed(window) {
            return;
        }
        if let Some(settings) = self.settings.as_mut() {
            settings.confirm();
        }
        self.marked = None;
        if self.watermark.is_some() {
            self.emit_watermark_change(false, cx);
        }
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.native_settings_input_allowed(window) {
            return;
        }
        self.replace(range, text, false, None, cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.native_settings_input_allowed(window) {
            return;
        }
        self.replace(range, text, true, selected, cx);
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_line.as_ref()?;
        let range = self.range_from_utf16(range);
        Some(Bounds::new(
            point(
                bounds.left() + line.x_for_index(range.start) - self.scroll_x,
                bounds.top(),
            ),
            size(
                (line.x_for_index(range.end) - line.x_for_index(range.start)).max(px(1.)),
                bounds.size.height,
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(utf_offset(&self.content, self.hit(point)))
    }
}
impl Focusable for TextField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for TextField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = crate::theme::palette(cx);
        let compact = self.watermark.is_some();
        div()
            .flex()
            .w_full()
            .h(px(if compact { 24. } else { 36. }))
            .px_2()
            .py_1()
            .when(compact, |field| field.px(px(4.)).py(px(1.)))
            .overflow_hidden()
            .rounded_md()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.foreground))
            .text_size(px(if compact { 12. } else { 14. }))
            .line_height(px(if compact { 20. } else { 26. }))
            .when(self.bare, |field| {
                field
                    .h(px(26.))
                    .px(px(2.))
                    .py(px(0.))
                    .bg(gpui::rgba(0))
                    .border_0()
                    .text_size(px(12.))
                    .line_height(px(26.))
            })
            .cursor(gpui::CursorStyle::IBeam)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    if !v.enabled {
                        cx.stop_propagation();
                        return;
                    }
                    let own_id = cx.entity().entity_id();
                    if v.marked.is_some()
                        || v.focus_group
                            .iter()
                            .filter(|field| field.entity_id() != own_id)
                            .filter_map(|field| field.upgrade())
                            .any(|field| field.read(cx).is_composing())
                    {
                        cx.stop_propagation();
                        return;
                    }
                    let first_watermark_focus =
                        v.watermark.is_some() && !v.focus_handle.is_focused(w);
                    w.focus(&v.focus_handle);
                    v.watermark_focus_changed(true, cx);
                    if first_watermark_focus && !e.modifiers.shift {
                        v.selection = 0..v.content.len();
                        v.reversed = false;
                        v.drag_anchor = None;
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    let caret = v.hit(e.position);
                    let (anchor, caret) = edit_boundary::selection_to(
                        &v.content,
                        v.selection.clone(),
                        v.reversed,
                        caret,
                        e.modifiers.shift,
                    );
                    v.select_to(anchor, caret);
                    v.drag_anchor = Some(anchor);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, _, cx| {
                if let Some(anchor) = v.drag_anchor {
                    v.select_to(anchor, v.hit(e.position));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|v, _, _, _| v.drag_anchor = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|v, _, _, _| v.drag_anchor = None),
            )
            .child(FieldText { input: cx.entity() })
            .into_any_element()
    }
}
struct FieldText {
    input: Entity<TextField>,
}
struct FieldPaint {
    line: ShapedLine,
    origin: Point<Pixels>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
}
impl IntoElement for FieldText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for FieldText {
    type RequestLayoutState = ();
    type PrepaintState = FieldPaint;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> FieldPaint {
        let v = self.input.read(cx);
        let style = window.text_style();
        let text: SharedString = if v.content.is_empty() {
            // 当前语言只改变占位文字，同一实体正文、选区与组合检查点全部保留。
            v.settings
                .as_ref()
                .and_then(|settings| settings.placeholder)
                .map_or_else(
                    || v.placeholder.clone(),
                    |placeholder| placeholder.get().into(),
                )
        } else {
            v.content.clone().into()
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color: if v.content.is_empty() {
                rgb(crate::theme::palette(cx).muted).into()
            } else {
                style.color
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = &v.marked {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect::<Vec<_>>()
        } else {
            vec![run]
        };
        let line = window.text_system().shape_line(
            text,
            style.font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        );
        let caret = line.x_for_index(v.caret());
        let width = bounds.size.width.max(px(1.));
        let mut scroll = v.scroll_x;
        if caret < scroll {
            scroll = caret;
        }
        if caret > scroll + width - px(3.) {
            scroll = caret - width + px(3.);
        }
        scroll = scroll
            .max(px(0.))
            .min((line.width - width + px(3.)).max(px(0.)));
        let origin = bounds.origin - point(scroll, px(0.));
        let selection = (!v.selection.is_empty() && v.focus_handle.is_focused(window)).then(|| {
            fill(
                Bounds::from_corners(
                    point(origin.x + line.x_for_index(v.selection.start), bounds.top()),
                    point(
                        origin.x + line.x_for_index(v.selection.end),
                        bounds.bottom(),
                    ),
                ),
                rgba(0x5796ff66),
            )
        });
        let cursor = (v.selection.is_empty() && v.focus_handle.is_focused(window)).then(|| {
            fill(
                Bounds::new(
                    point(origin.x + caret, bounds.top()),
                    size(px(1.5), bounds.size.height),
                ),
                rgb(crate::theme::palette(cx).accent),
            )
        });
        self.input.update(cx, |v, _| v.scroll_x = scroll);
        FieldPaint {
            line,
            origin,
            selection,
            cursor,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        paint: &mut FieldPaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(selection) = paint.selection.take() {
                window.paint_quad(selection);
            }
            let _ = paint
                .line
                .paint(paint.origin, window.line_height(), window, cx);
            if let Some(cursor) = paint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
        let line = paint.line.clone();
        self.input.update(cx, |v, _| {
            v.last_line = Some(line);
            v.last_bounds = Some(bounds);
        });
    }
}

#[cfg(test)]
mod settings_input_tests {
    use super::*;

    #[test]
    fn settings_native_input_gate_requires_active_window_and_own_focus_after_cancel() {
        let mut input = SettingsComposition::default();
        input.begin("日语", 0..6, true);
        let restored = input.cancel().unwrap();
        // 失活但内部 focus 保留，或本窗口已把焦点交给其它字段，三原生回调均拒绝。
        for (active, focus) in [(false, true), (true, false), (false, false)] {
            assert!(!settings_native_input_allowed(active, focus));
        }
        assert_eq!(restored, ("日语".into(), 0..6, true));
        assert!(input.cancel().is_none());
        assert!(settings_native_input_allowed(true, true));
        input.begin("日本語", 0..9, false);
        input.confirm();
        assert!(input.cancel().is_none());
    }

    #[test]
    fn cancelled_settings_composition_restores_original_text_and_reverse_selection() {
        let mut input = SettingsComposition::default();
        let text = "日语 e\u{301} 👩‍💻";
        let original_selection = 0..text.len();
        input.begin(text, original_selection.clone(), true);
        input.begin("ri", 0..2, false);
        assert_eq!(
            input.cancel(),
            Some((text.into(), original_selection, true))
        );
        assert!(input.cancel().is_none());
    }

    #[test]
    fn confirmed_or_programmatically_reset_settings_composition_cannot_restore_stale_text() {
        let mut input = SettingsComposition::default();
        input.begin("日语", 0..6, false);
        input.confirm();
        assert!(input.cancel().is_none());
        input.begin("日本語", 3..9, true);
        assert_eq!(input.cancel(), Some(("日本語".into(), 3..9, true)));
    }

    #[test]
    fn settings_tab_navigation_uses_order_and_preserves_ime_focus_and_held_key_ownership() {
        let modifiers = gpui::Modifiers::default();
        assert_eq!(
            settings_key_action("tab", modifiers, false, false, true),
            SettingsKeyAction::Tab { backwards: false }
        );
        assert_eq!(
            settings_key_action(
                "tab",
                gpui::Modifiers {
                    shift: true,
                    ..modifiers
                },
                false,
                false,
                true
            ),
            SettingsKeyAction::Tab { backwards: true }
        );
        for (held, composing, active) in [
            (true, false, true),
            (false, true, true),
            (false, false, false),
        ] {
            assert_eq!(
                settings_key_action("tab", modifiers, held, composing, active),
                SettingsKeyAction::Consume
            );
        }
        assert_eq!(
            settings_key_action("enter", modifiers, false, true, true),
            SettingsKeyAction::Consume
        );
        assert_eq!(
            settings_key_action("enter", modifiers, false, false, true),
            SettingsKeyAction::Pass
        );
        for modifiers in [
            gpui::Modifiers {
                control: true,
                ..modifiers
            },
            gpui::Modifiers {
                alt: true,
                ..modifiers
            },
            gpui::Modifiers {
                platform: true,
                ..modifiers
            },
            gpui::Modifiers {
                function: true,
                ..modifiers
            },
        ] {
            assert_eq!(
                settings_key_action("tab", modifiers, false, false, true),
                SettingsKeyAction::Pass
            );
        }
        let enabled = [true, true, true];
        assert_eq!(settings_tab_target(&enabled, 0, false), Some(1));
        assert_eq!(settings_tab_target(&enabled, 2, false), Some(0));
        assert_eq!(settings_tab_target(&enabled, 0, true), Some(2));
        assert_eq!(settings_tab_target(&[true, false, true], 0, false), Some(2));
        assert_eq!(settings_tab_target(&[true], 0, false), None);
    }
}
