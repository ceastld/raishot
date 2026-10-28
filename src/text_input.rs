//! GPUI 原生文本输入接口，UTF-16/UTF-8 转换保留中文 IME 和代理对边界。
use crate::capture_view::CaptureView;
use crate::text_contract::{decide, TextKeyAction};
use gpui::{
    point, px, size, Bounds, ClipboardItem, Context, EntityInputHandler, KeyDownEvent, Pixels,
    TextLayout, UTF16Selection, WeakEntity, Window,
};
use std::ops::Range;
#[path = "text_edit_boundary.rs"]
pub(crate) mod edit_boundary;
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct TextInputState {
    pub selection: Range<usize>,
    pub marked: Option<Range<usize>>,
    pub reversed: bool,
    pub drag_anchor: Option<usize>,
}
impl TextInputState {
    pub fn caret(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }
    pub fn select_to(&mut self, anchor: usize, caret: usize) {
        self.selection = anchor.min(caret)..anchor.max(caret);
        self.reversed = caret < anchor;
    }
    pub(crate) fn navigate(&mut self, text: &str, key: &str, shift: bool, control: bool) {
        let (anchor, caret) = edit_boundary::navigate(
            text,
            self.selection.clone(),
            self.reversed,
            key,
            shift,
            control,
        );
        self.select_to(anchor, caret);
    }
    pub(crate) fn deletion_range(&self, text: &str, backward: bool) -> Range<usize> {
        edit_boundary::deletion_range(text, self.selection.clone(), backward)
    }
}

pub(crate) fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units + c.len_utf16() > utf16 {
            return byte;
        }
        units += c.len_utf16();
    }
    text.len()
}
pub(crate) fn utf_offset(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

/// 只接收同一结果会话、同一文字版本的实际 paint 布局，坐标已包含滚动平移。
pub(crate) struct ResultInputLayout {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) layout: TextLayout,
}

/// 结果编辑使用独立输入实体，旧 IME 回调不会回落到标注的输入状态。
pub(crate) struct ResultInputHandler {
    view: WeakEntity<CaptureView>,
    generation: u64,
}

impl ResultInputHandler {
    pub(crate) fn new(view: WeakEntity<CaptureView>, generation: u64) -> Self {
        Self { view, generation }
    }

    fn with_view<R>(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        update: impl FnOnce(&mut CaptureView, &mut Context<CaptureView>) -> R,
    ) -> Option<R> {
        self.view
            .update(cx, |view, cx| {
                let painted = view
                    .result_input_layout
                    .borrow()
                    .as_ref()
                    .is_some_and(|receipt| receipt.generation == self.generation);
                (window.is_window_active()
                    && painted
                    && view.result_selection_generation() == self.generation
                    && view.result_editor_ready()
                    && view.focus.is_focused(window))
                .then(|| {
                    let result = update(view, cx);
                    if view.invalidate_translation_source() {
                        cx.notify();
                    }
                    result
                })
            })
            .ok()
            .flatten()
    }
}

impl EntityInputHandler for ResultInputHandler {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.with_view(window, cx, |view, _| {
            let editor = view.result_editor.as_ref().unwrap();
            let start = byte_offset(&editor.text, range.start.min(range.end));
            let end = byte_offset(&editor.text, range.start.max(range.end));
            *adjusted = Some(utf_offset(&editor.text, start)..utf_offset(&editor.text, end));
            editor.text[start..end].to_owned()
        })
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.with_view(window, cx, |view, _| {
            let editor = view.result_editor.as_ref().unwrap();
            UTF16Selection {
                range: utf_offset(&editor.text, editor.input.selection.start)
                    ..utf_offset(&editor.text, editor.input.selection.end),
                reversed: editor.input.reversed,
            }
        })
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.with_view(window, cx, |view, _| {
            let editor = view.result_editor.as_ref().unwrap();
            editor.input.marked.as_ref().map(|range| {
                utf_offset(&editor.text, range.start)..utf_offset(&editor.text, range.end)
            })
        })
        .flatten()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.with_view(window, cx, |view, cx| {
            view.result_editor.as_mut().unwrap().unmark();
            view.result_reveal_caret.set(true);
            cx.notify();
        });
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.with_view(window, cx, |view, cx| {
            view.result_editor
                .as_mut()
                .unwrap()
                .replace(range, text, false, None);
            view.result_reveal_caret.set(true);
            cx.notify();
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.with_view(window, cx, |view, cx| {
            view.result_editor
                .as_mut()
                .unwrap()
                .replace(range, text, true, selected);
            view.result_reveal_caret.set(true);
            cx.notify();
        });
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.with_view(window, cx, |view, _| {
            let editor = view.result_editor.as_ref().unwrap();
            let receipt = view.result_input_layout.borrow();
            let receipt = receipt.as_ref()?;
            if receipt.generation != self.generation || receipt.revision != editor.revision {
                return None;
            }
            let index = byte_offset(&editor.text, range.start);
            let origin = receipt.layout.position_for_index(index)?;
            // GPUI 提供窗口内 DIP，Windows backend 再乘 DPI；不能再叠全局原点或滚动量。
            Some(Bounds::new(
                origin,
                size(px(1.0), receipt.layout.line_height()),
            ))
        })
        .flatten()
    }

    fn character_index_for_point(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.with_view(window, cx, |view, _| {
            let editor = view.result_editor.as_ref().unwrap();
            let receipt = view.result_input_layout.borrow();
            let receipt = receipt.as_ref()?;
            if receipt.generation != self.generation || receipt.revision != editor.revision {
                return None;
            }
            let index = receipt
                .layout
                .index_for_position(position)
                .unwrap_or_else(|index| index)
                .min(editor.text.len());
            let index = edit_boundary::floor(&editor.text, index);
            Some(utf_offset(&editor.text, index))
        })
        .flatten()
    }
}

impl CaptureView {
    pub(crate) fn result_editor_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.result_editor_ready() {
            return false;
        }
        let Some(writer_command) = self.begin_result_key_command(window, cx) else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if m.alt || m.platform || m.function {
            return false;
        }
        let control_only = m.control && !m.shift;
        let mut reveal = false;
        match key {
            "a" | "c" | "x" | "v" if control_only => {
                if !event.is_held {
                    let editor = self.result_editor.as_mut().unwrap();
                    match key {
                        "a" => {
                            editor.select_all();
                            reveal = true;
                        }
                        "c" | "x" => {
                            let cut = key == "x";
                            match editor.publish_text_copy(cut, |text| {
                                crate::export::copy_text(text, crate::gpui_capture::hwnd(window))
                            }) {
                                Ok(published) => {
                                    reveal = cut && published;
                                }
                                Err(error) => {
                                    self.status = trf!(
                                        "文字复制失败：{error}；正文和选择已保留",
                                        "Copy failed: {error}; text and selection kept"
                                    );
                                }
                            }
                        }
                        "v" => {
                            if let Some(text) =
                                cx.read_from_clipboard().and_then(|item| item.text())
                            {
                                editor.replace(None, &text, false, None);
                                reveal = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "z" | "y" if m.control => {
                if !event.is_held {
                    let editor = self.result_editor.as_mut().unwrap();
                    if key == "y" || m.shift {
                        editor.redo();
                    } else {
                        editor.undo();
                    }
                    reveal = true;
                }
            }
            "enter" if !m.control => {
                self.result_editor
                    .as_mut()
                    .unwrap()
                    .replace(None, "\n", false, None);
                reveal = true;
            }
            "backspace" | "delete" => {
                self.result_editor
                    .as_mut()
                    .unwrap()
                    .delete(key == "backspace");
                reveal = true;
            }
            "left" | "right" => {
                self.result_editor
                    .as_mut()
                    .unwrap()
                    .navigate(key, m.shift, m.control);
                reveal = true;
            }
            "home" | "end" if m.control => {
                let editor = self.result_editor.as_mut().unwrap();
                editor.move_to(if key == "home" { 0 } else { editor.text.len() }, m.shift);
                reveal = true;
            }
            "up" | "down" | "home" | "end" => {
                // 视觉行使用同一份已绘制布局；未 paint 或已失效的回执不得退回旧排版。
                let target = self
                    .result_writer_geometry_current(window, cx)
                    .then(|| {
                        let editor = self.result_editor.as_ref().unwrap();
                        let receipt = self.result_input_layout.borrow();
                        receipt
                            .as_ref()
                            .filter(|receipt| {
                                receipt.generation == self.result_selection_generation()
                                    && receipt.revision == editor.revision
                            })
                            .and_then(|receipt| {
                                let origin = receipt.layout.position_for_index(editor.caret())?;
                                let height = receipt.layout.line_height();
                                let bounds = receipt.layout.bounds();
                                let position = match key {
                                    "up" => point(origin.x, origin.y - height + height / 2.0),
                                    "down" => point(origin.x, origin.y + height + height / 2.0),
                                    "home" => {
                                        point(bounds.left() - px(1.0), origin.y + height / 2.0)
                                    }
                                    _ => point(bounds.right() + px(1.0), origin.y + height / 2.0),
                                };
                                Some(
                                    receipt
                                        .layout
                                        .index_for_position(position)
                                        .unwrap_or_else(|index| index),
                                )
                            })
                    })
                    .flatten();
                if let Some(index) = target {
                    self.result_editor.as_mut().unwrap().move_to(index, m.shift);
                    reveal = true;
                }
            }
            _ => return false,
        }
        self.finish_result_key_command(writer_command, window, cx);
        self.invalidate_translation_source();
        if reveal {
            self.result_reveal_caret.set(true);
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    pub(crate) fn text_composing(&self) -> bool {
        self.editor.text_draft.is_some() && self.input.marked.is_some()
    }
    fn input_text(&self) -> &str {
        self.editor
            .text_draft
            .as_ref()
            .map_or("", |d| d.annotation.text.as_str())
    }
    pub fn reset_input(&mut self) {
        let len = self.input_text().len();
        self.input.selection = 0..len;
        self.input.marked = None;
        self.input.reversed = false;
        self.input.drag_anchor = None;
    }
    fn input_range(&self, range: Range<usize>) -> Range<usize> {
        let text = self.input_text();
        byte_offset(text, range.start)..byte_offset(text, range.end)
    }
    fn input_replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        marked: bool,
        selected: Option<Range<usize>>,
    ) {
        if self.editor.text_draft.is_none() {
            return;
        }
        let range = range
            .map(|r| self.input_range(r))
            .or(self.input.marked.clone())
            .unwrap_or(self.input.selection.clone());
        let content = self.input_text();
        let start = range.start.min(content.len());
        let end = range.end.min(content.len()).max(start);
        // 限制单对象内容，防止粘贴巨量文本阻塞截图输入。
        let kept = content.chars().count() - content[start..end].chars().count();
        let capacity = 4096usize.saturating_sub(kept);
        if marked && text.chars().count() > capacity {
            // 输入法组合不截断，避免候选串被拆开。
            return;
        }
        // 普通粘贴超长时截到剩余容量，不静默整段丢弃。
        let fitted = edit_boundary::fit_insert(text, capacity);
        if fitted.is_empty() && !text.is_empty() {
            return;
        }
        let text = fitted.to_owned();
        self.editor.replace_text(start..end, &text);
        self.input.selection = selected
            .map(|r| start + byte_offset(&text, r.start)..start + byte_offset(&text, r.end))
            .unwrap_or(start + text.len()..start + text.len());
        self.input.reversed = false;
        self.input.drag_anchor = None;
        self.input.marked = if marked && !text.is_empty() {
            Some(start..start + text.len())
        } else {
            None
        };
    }
    pub fn text_key(
        &mut self,
        e: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.editor.text_draft.is_none() {
            return false;
        }
        let key = e.keystroke.key.as_str();
        let m = e.keystroke.modifiers;
        match decide(
            key,
            m,
            e.is_held,
            self.text_composing(),
            self.input_text().is_empty(),
        ) {
            TextKeyAction::Ime | TextKeyAction::Consume => {
                cx.stop_propagation();
                return true;
            }
            TextKeyAction::Finish | TextKeyAction::DeleteEmpty => {
                self.finish_content_or_status();
                cx.stop_propagation();
                cx.notify();
                return true;
            }
            TextKeyAction::LineBreak => {
                self.input_replace(None, "\n", false, None);
                cx.stop_propagation();
                cx.notify();
                return true;
            }
            TextKeyAction::Pass if m.alt || m.platform => return false,
            TextKeyAction::Pass => {}
        }
        let text = self.input_text().to_owned();
        let range = self.input.selection.clone();
        match key {
            "a" if m.control => {
                self.input.selection = 0..text.len();
                self.input.reversed = false;
            }
            "c" | "x" if m.control => {
                let range = if key == "x" && !range.is_empty() {
                    self.input.deletion_range(&text, false)
                } else {
                    range
                };
                let published = edit_boundary::publish_selected(&text, range.clone(), |selected| {
                    cx.write_to_clipboard(ClipboardItem::new_string(selected.to_owned()));
                });
                if key == "x" && published {
                    self.input_replace(
                        Some(utf_offset(&text, range.start)..utf_offset(&text, range.end)),
                        "",
                        false,
                        None,
                    );
                }
            }
            "v" if m.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.input_replace(None, &text, false, None);
                }
            }
            "backspace" | "delete" => {
                let range = self.input.deletion_range(&text, key == "backspace");
                if !range.is_empty() {
                    self.input.selection = range;
                    self.input_replace(None, "", false, None);
                }
            }
            "left" | "right" | "home" | "end" | "up" | "down" => {
                self.input.navigate(&text, key, m.shift, m.control);
            }
            _ => return false,
        }
        cx.stop_propagation();
        cx.notify();
        let _ = window;
        true
    }
}
impl EntityInputHandler for CaptureView {
    fn text_for_range(
        &mut self,
        r: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let r = self.input_range(r);
        *adjusted =
            Some(utf_offset(self.input_text(), r.start)..utf_offset(self.input_text(), r.end));
        Some(self.input_text()[r].into())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.editor.text_draft.as_ref()?;
        Some(UTF16Selection {
            range: utf_offset(self.input_text(), self.input.selection.start)
                ..utf_offset(self.input_text(), self.input.selection.end),
            reversed: self.input.reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.input
            .marked
            .as_ref()
            .map(|r| utf_offset(self.input_text(), r.start)..utf_offset(self.input_text(), r.end))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.input.marked = None;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        r: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_replace(r, text, false, None);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        r: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_replace(r, text, true, selected);
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(utf_offset(self.input_text(), self.input.caret()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reverse_selection_keeps_anchor_across_unicode_and_direction_changes() {
        let text = "a中文🙂";
        let mut input = TextInputState {
            selection: text.len()..text.len(),
            ..Default::default()
        };
        input.navigate(text, "left", true, false);
        assert_eq!(input.selection, 7..11);
        assert!(input.reversed);
        input.navigate(text, "left", true, false);
        assert_eq!(input.selection, 4..11);
        input.navigate(text, "right", true, false);
        assert_eq!(input.selection, 7..11);
        input.navigate(text, "right", false, false);
        assert_eq!(input.selection, 11..11);
        input.select_to(4, 1);
        assert_eq!(input.selection, 1..4);
        assert!(input.reversed);
    }
    #[test]
    fn vertical_and_line_navigation_preserve_unicode_boundaries() {
        let text = "中文abc\n🙂xy\nend";
        let mut input = TextInputState {
            selection: 6..6,
            ..Default::default()
        };
        input.navigate(text, "down", false, false);
        assert_eq!(input.caret(), 15);
        input.navigate(text, "home", false, false);
        assert_eq!(input.caret(), 10);
        input.navigate(text, "end", true, false);
        assert_eq!(input.selection, 10..16);
        input.navigate(text, "home", false, true);
        assert_eq!(input.caret(), 0);
        input.navigate(text, "end", false, true);
        assert_eq!(input.caret(), text.len());
    }
    #[test]
    fn utf16_ranges_do_not_split_chinese_or_surrogate_pairs() {
        let s = "中文🙂a";
        assert_eq!(byte_offset(s, 2), 6);
        assert_eq!(byte_offset(s, 3), 6);
        assert_eq!(byte_offset(s, 4), 10);
        assert_eq!(utf_offset(s, 10), 4);
        assert_eq!(byte_offset(s, 99), s.len());
    }

    #[test]
    fn editable_input_navigation_keeps_extended_graphemes_and_reverse_anchor() {
        let pieces = ["A", "👨‍👩‍👧‍👦", "🇨🇳", "👍🏽", "e\u{301}", "\r\n", "Z"];
        let text = pieces.concat();
        let mut stops = vec![0];
        for piece in pieces {
            stops.push(stops.last().unwrap() + piece.len());
        }
        let mut input = TextInputState::default();
        for &expected in &stops[1..] {
            input.navigate(&text, "right", false, false);
            assert_eq!(input.selection, expected..expected);
        }
        for &expected in stops[..stops.len() - 1].iter().rev() {
            input.navigate(&text, "left", false, false);
            assert_eq!(input.selection, expected..expected);
        }
        input.select_to(text.len(), text.len());
        input.navigate(&text, "left", true, false);
        input.navigate(&text, "left", true, false);
        assert_eq!(input.selection, stops[5]..text.len());
        assert!(input.reversed);
        input.navigate(&text, "right", true, false);
        assert_eq!(input.selection, stops[6]..text.len());
        input.navigate(&text, "right", true, false);
        assert_eq!(input.selection, text.len()..text.len());
        assert!(!input.reversed);
        input.select_to(stops[2], stops[4]);
        input.navigate(&text, "left", false, false);
        assert_eq!(input.selection, stops[2]..stops[2]);
        input.select_to(stops[4], stops[2]);
        input.navigate(&text, "right", false, false);
        assert_eq!(input.selection, stops[4]..stops[4]);
        // Ctrl+方向仍沿普通输入原合同走一个字素，不新增只读 OCR 的词跳转。
        input.navigate(&text, "left", false, true);
        assert_eq!(input.selection, stops[3]..stops[3]);
    }

    #[test]
    fn editable_input_vertical_short_lines_and_crlf_have_no_partial_stop() {
        let text = "甲👨‍👩‍👧‍👦e\u{301}Z\r\n👍🏽\r\n🇨🇳a\n尾";
        let first = "甲👨‍👩‍👧‍👦e\u{301}Z".len();
        let second = first + 2 + "👍🏽".len();
        let third_start = second + 2;
        let mut input = TextInputState {
            selection: first - 1..first - 1,
            ..Default::default()
        };
        input.navigate(text, "down", false, false);
        assert_eq!(input.caret(), second);
        input.navigate(text, "down", false, false);
        assert_eq!(input.caret(), third_start + "🇨🇳".len());
        input.navigate(text, "up", false, false);
        assert_eq!(input.caret(), second);
        input.navigate(text, "up", false, false);
        assert_eq!(input.caret(), "甲".len());
        input.navigate(text, "end", false, false);
        assert_eq!(input.caret(), first);
        input.navigate(text, "right", true, false);
        assert_eq!(input.selection, first..first + 2);
        input.navigate(text, "left", true, false);
        assert_eq!(input.selection, first..first);
        input.navigate(text, "home", false, true);
        assert_eq!(input.caret(), 0);
        input.navigate(text, "end", false, true);
        assert_eq!(input.caret(), text.len());
        input.navigate(text, "home", true, false);
        assert_eq!(input.selection, text.len() - "尾".len()..text.len());
        assert!(input.reversed);
    }

    #[test]
    fn editable_input_user_deletion_expands_native_partial_selection_but_utf16_stays_exact() {
        let text = "Ae\u{301}👩🏽‍💻B";
        let mut input = TextInputState {
            selection: 2..4,
            ..Default::default()
        };
        assert_eq!(input.deletion_range(text, false), 1..4);
        input.navigate(text, "left", false, false);
        assert_eq!(input.selection, 1..1);
        input.select_to(2, 2);
        assert_eq!(input.deletion_range(text, true), 1..4);
        assert_eq!(input.deletion_range(text, false), 1..4);
        input.navigate(text, "right", false, false);
        assert_eq!(input.selection, 4..4);
        input.select_to(2, 2);
        input.navigate(text, "left", true, false);
        assert_eq!(input.selection, 1..4);
        assert!(input.reversed);
        input.select_to(2, 2);
        input.navigate(text, "right", true, false);
        assert_eq!(input.selection, 1..4);
        assert!(!input.reversed);
        // IME 可独立命中/替换附加音标或 ZWJ 中间码点，桥不收宽成字素。
        assert_eq!(byte_offset(text, 2), 2);
        assert_eq!(byte_offset(text, 3), 4);
        assert_eq!(utf_offset(text, 2), 2);
        assert_eq!(byte_offset(text, 5), 8);
    }
}
