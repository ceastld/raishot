//! 普通 OCR 完成后的纯结果路由；调用方负责验证请求目标和执行输出。
use super::ocr_task::TaskSlot;
use crate::recognition::task::TaskToken;

#[path = "ocr_readonly_navigation.rs"]
mod readonly_navigation;
#[cfg(test)]
#[path = "ocr_readonly_text_navigation_tests.rs"]
mod readonly_navigation_tests;

#[path = "ocr_readonly_visual_layout.rs"]
pub(crate) mod readonly_visual_layout;
#[path = "ocr_readonly_visual_navigation.rs"]
pub(crate) mod readonly_visual_navigation;
#[cfg(test)]
#[path = "ocr_readonly_visual_navigation_tests.rs"]
mod readonly_visual_navigation_tests;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Completion {
    CopyAndClose(String),
    ShowResult(String),
    KeepOpenWithError(String),
}

pub(super) fn route(
    result: Result<String, String>,
    copy_only: bool,
    target_current: bool,
) -> Completion {
    match result {
        Err(error) => Completion::KeepOpenWithError(error),
        Ok(_) if copy_only && !target_current => Completion::KeepOpenWithError(
            tr!(
                "识别期间截图范围或内容已改变，请重新识别文字",
                "The capture area or content changed during recognition; recognize the text again"
            )
            .into(),
        ),
        Ok(text) if copy_only && !text.trim().is_empty() => Completion::CopyAndClose(text),
        // 空状态仅由渲染层显示占位，不能成为复制或全文编辑的正文。
        Ok(text) => Completion::ShowResult(text),
    }
}

/// 自动复制在目标门通过后只发布一次；只有 writer 成功才授权关闭截图。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AutoCopyPublication<E> {
    Close,
    KeepResult { text: String, error: E },
}

pub(crate) fn publish_auto_copy<E>(
    text: String,
    write: impl FnOnce(&str) -> Result<(), E>,
) -> AutoCopyPublication<E> {
    match write(&text) {
        Ok(()) => AutoCopyPublication::Close,
        Err(error) => AutoCopyPublication::KeepResult { text, error },
    }
}

/// 只读候选允许连续导航；复制、全选和取消仍仅响应第一次按下。
/// 这只决定重复事件种类，调用者仍须核当前来源、焦点、实际 paint 与输入门。
pub(crate) fn candidate_key_repeat_allowed(key: &str, held: bool) -> bool {
    !held || matches!(key, "left" | "right" | "up" | "down" | "home" | "end")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResultFrame {
    pub(crate) generation: u64,
    pub(crate) revision: Option<u64>,
}

/// 表格按钮的真实 Down 身份；同字节 OCR 替换和全文模式切换也不继承旧点击。
#[derive(Clone)]
pub(crate) struct TableClickStamp<S> {
    pub(crate) source: S,
    pub(crate) frame: ResultFrame,
    pub(crate) body_identity: std::sync::Arc<()>,
    pub(crate) inline: bool,
}
impl<S: PartialEq> TableClickStamp<S> {
    pub(crate) fn matches(&self, current: &Self) -> bool {
        self.source == current.source
            && self.frame == current.frame
            && std::sync::Arc::ptr_eq(&self.body_identity, &current.body_identity)
            && self.inline == current.inline
    }
}

pub(super) fn result_frame_current(
    painted: Option<ResultFrame>,
    requested: ResultFrame,
    current: ResultFrame,
) -> bool {
    painted == Some(requested) && requested == current
}

#[derive(Clone, Copy, Debug)]
pub(super) struct OutputGate {
    pub(super) owns_result: bool,
    pub(super) editing: bool,
    pub(super) input_ready: bool,
    pub(super) frame_current: bool,
    pub(super) active: bool,
    pub(super) focused: bool,
}

impl OutputGate {
    pub(super) fn ready(self) -> bool {
        self.owns_result && self.input_ready && self.frame_current && self.active && self.focused
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputKey {
    Pass,
    Consume,
    CopySelection,
    SelectAll,
}

/// 在全文输入器及原位 OCR 之后执行；结果层拒绝输出时也不能穿透到图片复制。
pub(super) fn output_key(
    key: &str,
    modifiers: gpui::Modifiers,
    held: bool,
    gate: OutputGate,
) -> OutputKey {
    if !gate.owns_result {
        return OutputKey::Pass;
    }
    let bare = !modifiers.control
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function;
    let control_only = modifiers.control
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.platform
        && !modifiers.function;
    let action = match key {
        "enter" if bare => OutputKey::CopySelection,
        "enter" => return OutputKey::Consume,
        "c" if control_only => OutputKey::CopySelection,
        "a" if control_only => OutputKey::SelectAll,
        _ => return OutputKey::Pass,
    };
    if held || gate.editing || !gate.ready() {
        OutputKey::Consume
    } else {
        action
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyScope {
    Selection,
    Full,
}

pub(super) fn copy_payload(
    text: &str,
    selection: std::ops::Range<usize>,
    scope: CopyScope,
) -> Option<&str> {
    if text.trim().is_empty() {
        return None;
    }
    Some(match scope {
        CopyScope::Full => text,
        CopyScope::Selection => text
            .get(selection)
            .filter(|selected| !selected.is_empty())
            .unwrap_or(text),
    })
}

/// 只借用正文及选择；失败不修改会话，下一次仍能用同一输入重试。
pub(super) fn publish_text<E>(
    text: &str,
    selection: std::ops::Range<usize>,
    scope: CopyScope,
    write: impl FnOnce(&str) -> Result<(), E>,
) -> Result<bool, E> {
    let Some(text) = copy_payload(text, selection, scope) else {
        return Ok(false);
    };
    write(text)?;
    Ok(true)
}

#[cfg(test)]
#[path = "capture_result_output_tests.rs"]
mod output_key_tests;

/// OCR 读取的实际截图目标；结果重试不借用输入布局的代次。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OcrTarget {
    pub(crate) canvas: crate::geometry::ScreenRect,
    pub(crate) selected: crate::geometry::Rect,
    pub(crate) revisions: (u64, u64),
    pub(crate) stamp: (u64, u64),
    pub(crate) identity: crate::scene::SourceIdentity,
}

impl OcrTarget {
    fn same_source_region(&self, current: &Self, stable_roi: bool) -> bool {
        self.canvas == current.canvas
            && self.identity == current.identity
            && (!stable_roi || self.selected == current.selected)
    }

    fn same_content(&self, current: &Self, stable_roi: bool) -> bool {
        self.same_source_region(current, stable_roi)
            && self.revisions == current.revisions
            && self.stamp == current.stamp
    }
}

pub(crate) fn frozen_source_identity(
    mut source: crate::scene::SourcePixels<'_>,
) -> crate::scene::SourceIdentity {
    // 原始冻结输入不读取光标层；仅内容 OCR 的 SourceIdentity 包含光标。
    source.overlay = None;
    crate::scene::SourceIdentity::of(&source)
}

/// 内容 OCR 读取已绘制像素；UIA 补救只读取原始冻结底图。
#[derive(Clone)]
pub(crate) enum InputTarget {
    Content(OcrTarget),
    Frozen(OcrTarget),
}

impl InputTarget {
    pub(crate) fn is_current(&self, current: &OcrTarget, stable_roi: bool) -> bool {
        match self {
            Self::Content(target) => target.same_content(current, stable_roi),
            Self::Frozen(target) => target.same_source_region(current, stable_roi),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Origin {
    Ocr(OcrTarget),
    ControlText,
    Qr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OcrRequestKind {
    Initial,
    Retry,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct OcrRequest {
    pub(super) id: u64,
    pub(super) session: u64,
    pub(super) content_revision: u64,
    pub(super) target: OcrTarget,
    pub(super) kind: OcrRequestKind,
}

/// 展示会话与任务所有权独立于全文编辑开关；不读写宿主 processing。
#[derive(Default)]
pub(super) struct RecognitionSession {
    serial: u64,
    next_request: u64,
    origin: Option<Origin>,
    /// 收起时携带 ResultEditor.revision，再展开时交还，不另算文字版本。
    pub(super) content_revision: u64,
    initial: Option<OcrRequest>,
    retry: Option<OcrRequest>,
    initial_task: TaskSlot<OcrRequest>,
    retry_task: TaskSlot<OcrRequest>,
}

impl RecognitionSession {
    pub(super) fn show(&mut self, origin: Origin) -> bool {
        let release_initial = self.invalidate_initial();
        self.serial = self.serial.wrapping_add(1);
        self.origin = Some(origin);
        self.content_revision = 0;
        self.invalidate_retry();
        release_initial
    }

    pub(super) fn dismiss(&mut self) -> bool {
        let release_initial = self.invalidate_initial();
        self.serial = self.serial.wrapping_add(1);
        self.origin = None;
        self.content_revision = 0;
        self.invalidate_retry();
        release_initial
    }

    pub(super) fn ocr_target(&self) -> Option<&OcrTarget> {
        match &self.origin {
            Some(Origin::Ocr(target)) => Some(target),
            _ => None,
        }
    }

    fn request(
        &mut self,
        target: OcrTarget,
        content_revision: u64,
        kind: OcrRequestKind,
    ) -> OcrRequest {
        self.next_request = self
            .next_request
            .checked_add(1)
            .expect("OCR 结果请求身份计数耗尽");
        OcrRequest {
            id: self.next_request,
            session: self.serial,
            content_revision,
            target,
            kind,
        }
    }

    pub(super) fn begin_initial(&mut self, target: OcrTarget, content_revision: u64) -> OcrRequest {
        self.invalidate_requests();
        let request = self.request(target, content_revision, OcrRequestKind::Initial);
        self.initial_task.start(request.clone());
        self.initial = Some(request.clone());
        request
    }

    pub(super) fn finish_initial(&mut self, id: u64) -> Option<OcrRequest> {
        if self
            .initial
            .as_ref()
            .is_some_and(|request| request.id == id)
        {
            let task_id = self.initial_task.current_id()?;
            if self
                .initial_task
                .key()
                .is_none_or(|request| request.id != id)
                || !self.initial_task.finish(task_id)
            {
                return None;
            }
            // Worker 已分类回执，Owner 释放后的取消位不能重新否决成功。
            self.initial.take()
        } else {
            None
        }
    }

    pub(super) fn invalidate_initial(&mut self) -> bool {
        self.initial_task.cancel("结果会话初次识别失效");
        self.initial.take().is_some()
    }

    fn invalidate_retry(&mut self) {
        self.retry_task.cancel("结果重试失效，保留原文与标记");
        self.retry = None;
    }

    pub(super) fn invalidate_requests(&mut self) -> bool {
        self.invalidate_retry();
        self.invalidate_initial()
    }

    pub(super) fn begin_retry(
        &mut self,
        target: OcrTarget,
        content_revision: u64,
    ) -> Option<OcrRequest> {
        if self.initial.is_some() || self.retry.is_some() || self.ocr_target() != Some(&target) {
            return None;
        }
        let request = self.request(target, content_revision, OcrRequestKind::Retry);
        self.retry_task.start(request.clone());
        self.retry = Some(request.clone());
        Some(request)
    }

    pub(super) fn finish_retry(&mut self, id: u64) -> Option<OcrRequest> {
        if self.retry.as_ref().is_some_and(|request| request.id == id) {
            let task_id = self.retry_task.current_id()?;
            if self.retry_task.key().is_none_or(|request| request.id != id)
                || !self.retry_task.finish(task_id)
            {
                return None;
            }
            self.retry.take()
        } else {
            None
        }
    }

    pub(super) fn retry_busy(&self) -> bool {
        self.retry.is_some()
    }

    pub(super) fn initial_token(&self, id: u64) -> Option<TaskToken> {
        self.initial_task.key().filter(|request| request.id == id)?;
        self.initial_task.token()
    }

    pub(super) fn retry_token(&self, id: u64) -> Option<TaskToken> {
        self.retry_task.key().filter(|request| request.id == id)?;
        self.retry_task.token()
    }

    pub(super) fn has_pending(&self) -> bool {
        self.initial.is_some() || self.retry.is_some()
    }

    /// 仅借用既有请求身份；渲染层仍须核定对应任务令牌及当前来源。
    pub(super) fn pending_request(&self) -> Option<&OcrRequest> {
        self.initial.as_ref().or(self.retry.as_ref())
    }

    /// 真源、稳定 ROI、已提交内容或结果正文失效才撤销任务，不修改已有正文。
    pub(super) fn invalidate_stale(
        &mut self,
        target: Option<&OcrTarget>,
        content_revision: u64,
        stable_roi: bool,
    ) -> bool {
        let current = |request: &OcrRequest| {
            request.session == self.serial
                && request.content_revision == content_revision
                && target.is_some_and(|target| request.target.same_content(target, stable_roi))
                && (request.kind == OcrRequestKind::Initial
                    || self.ocr_target() == Some(&request.target))
        };
        let initial_stale = self
            .initial
            .as_ref()
            .is_some_and(|request| !current(request));
        let retry_stale = self.retry.as_ref().is_some_and(|request| !current(request));
        if retry_stale {
            self.invalidate_retry();
        }
        initial_stale && self.invalidate_initial()
    }

    pub(super) fn request_is_current(
        &self,
        request: &OcrRequest,
        target: &OcrTarget,
        content_revision: u64,
    ) -> bool {
        request.session == self.serial
            && request.content_revision == content_revision
            && request.target == *target
            && (request.kind == OcrRequestKind::Initial || self.ocr_target() == Some(target))
    }
}

/// 已完成结果的只读选择；字节位置由实际绘制的 GPUI TextLayout 提供。
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    range: std::ops::Range<usize>,
    drag_anchor: Option<usize>,
}

impl Selection {
    fn boundary(text: &str, index: usize) -> usize {
        let mut index = index.min(text.len());
        while !text.is_char_boundary(index) {
            index -= 1;
        }
        index
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn range(&self) -> std::ops::Range<usize> {
        self.range.clone()
    }

    pub(super) fn begin(&mut self, text: &str, index: usize) {
        let index = readonly_navigation::floor_boundary(text, index);
        self.range = index..index;
        self.drag_anchor = Some(index);
    }

    pub(super) fn extend(&mut self, text: &str, index: usize) {
        if let Some(anchor) = self.drag_anchor {
            let index = readonly_navigation::floor_boundary(text, index);
            self.range = anchor.min(index)..anchor.max(index);
        }
    }

    pub(super) fn dragging(&self) -> bool {
        self.drag_anchor.is_some()
    }

    pub(super) fn end_drag(&mut self) -> bool {
        self.drag_anchor.take().is_some()
    }

    pub(super) fn select_all(&mut self, text: &str) {
        self.range = 0..text.len();
        self.drag_anchor = None;
    }

    #[cfg(test)]
    pub(super) fn selected_or_full<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.range.clone())
            .filter(|selected| !selected.is_empty())
            .unwrap_or(text)
    }
}

/// 全文编辑的独立会话；输入、预编辑和撤销不引用标注文档。
#[derive(Debug)]
pub(crate) struct ResultEditor {
    pub(crate) text: String,
    pub(crate) input: crate::text_input::TextInputState,
    pub(crate) revision: u64,
    /// 真正正文写入／恢复都更换；数值版本回绕和编辑 Undo 不复活旧图上资格。
    pub(crate) body_identity: std::sync::Arc<()>,
    composition_before: Option<EditSnapshot>,
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
}

#[derive(Clone, Debug)]
struct EditSnapshot {
    text: String,
    input: crate::text_input::TextInputState,
}

impl ResultEditor {
    /// 只有发布成功才剪切；失败保留正文、选择、IME和两栈供显式重试。
    pub(crate) fn publish_text_copy(
        &mut self,
        cut: bool,
        write: impl FnOnce(&str) -> crate::Result<()>,
    ) -> crate::Result<bool> {
        if self.composing() || (cut && self.input.selection.is_empty()) {
            return Ok(false);
        }
        let cut_range = cut.then(|| self.input.deletion_range(&self.text, false));
        let text = if let Some(range) = &cut_range {
            self.text.get(range.clone()).unwrap_or("")
        } else {
            self.selected_or_full()
        };
        if text.is_empty() {
            return Ok(false);
        }
        write(text)?;
        if let Some(range) = cut_range {
            self.replace(
                Some(
                    crate::text_input::utf_offset(&self.text, range.start)
                        ..crate::text_input::utf_offset(&self.text, range.end),
                ),
                "",
                false,
                None,
            );
        }
        Ok(true)
    }
    pub(crate) fn new(text: String, range: std::ops::Range<usize>) -> Self {
        let start = Selection::boundary(&text, range.start);
        let end = Selection::boundary(&text, range.end);
        Self {
            text,
            input: crate::text_input::TextInputState {
                selection: start.min(end)..start.max(end),
                ..Default::default()
            },
            revision: 0,
            body_identity: std::sync::Arc::new(()),
            composition_before: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    fn snapshot(&self) -> EditSnapshot {
        let mut input = self.input.clone();
        input.marked = None;
        input.drag_anchor = None;
        EditSnapshot {
            text: self.text.clone(),
            input,
        }
    }

    fn remember(&mut self, before: EditSnapshot) {
        if before.text == self.text {
            return;
        }
        // 限制的是会话撤销条数；识别全文不受标注对象的 4096 字符限制。
        if self.undo.len() == 64 {
            self.undo.remove(0);
        }
        self.undo.push(before);
        self.redo.clear();
    }

    fn restore(&mut self, snapshot: EditSnapshot) {
        if self.text != snapshot.text {
            self.revision = self.revision.wrapping_add(1);
            self.body_identity = std::sync::Arc::new(());
        }
        self.text = snapshot.text;
        self.input = snapshot.input;
        self.input.marked = None;
        self.input.drag_anchor = None;
        self.composition_before = None;
    }

    pub(crate) fn caret(&self) -> usize {
        self.input.caret()
    }

    pub(crate) fn composing(&self) -> bool {
        self.input.marked.is_some()
    }

    pub(crate) fn selected_or_full(&self) -> &str {
        self.text
            .get(self.input.selection.clone())
            .filter(|selected| !selected.is_empty())
            .unwrap_or(&self.text)
    }

    pub(crate) fn select_all(&mut self) {
        self.input.select_to(0, self.text.len());
        self.input.drag_anchor = None;
    }

    pub(crate) fn move_to(&mut self, index: usize, shift: bool) {
        let (anchor, caret) = crate::text_input::edit_boundary::selection_to(
            &self.text,
            self.input.selection.clone(),
            self.input.reversed,
            index,
            shift,
        );
        self.input.select_to(anchor, caret);
        self.input.drag_anchor = None;
    }

    pub(crate) fn begin_drag(&mut self, index: usize) {
        let index = crate::text_input::edit_boundary::floor(&self.text, index);
        self.input.select_to(index, index);
        self.input.drag_anchor = Some(index);
    }

    pub(crate) fn extend_drag(&mut self, index: usize) {
        if let Some(anchor) = self.input.drag_anchor {
            self.input.select_to(
                anchor,
                crate::text_input::edit_boundary::floor(&self.text, index),
            );
        }
    }

    pub(crate) fn dragging(&self) -> bool {
        self.input.drag_anchor.is_some()
    }

    pub(crate) fn end_drag(&mut self) -> bool {
        self.input.drag_anchor.take().is_some()
    }

    pub(crate) fn navigate(&mut self, key: &str, shift: bool, control: bool) {
        self.input.navigate(&self.text, key, shift, control);
        self.input.drag_anchor = None;
    }

    /// 只读候选不借用可编辑输入导航；正文和编辑历史始终保持。
    /// Home/End 是完整硬换行的逻辑行，视觉换行及上下键另由实际布局处理。
    pub(crate) fn readonly_navigate(&mut self, key: &str, shift: bool, control: bool) -> bool {
        if self.composing() {
            return false;
        }
        let Some((anchor, caret)) = readonly_navigation::navigate(
            &self.text,
            self.input.selection.clone(),
            self.input.reversed,
            key,
            shift,
            control,
        ) else {
            return false;
        };
        self.input.select_to(anchor, caret);
        self.input.drag_anchor = None;
        true
    }

    /// GPUI TextLayout 返回的字节位置只在完整扩展字素处开始只读拖选。
    pub(crate) fn readonly_begin_drag(&mut self, index: usize) -> bool {
        if self.composing() {
            return false;
        }
        let index = readonly_navigation::floor_boundary(&self.text, index);
        self.input.select_to(index, index);
        self.input.drag_anchor = Some(index);
        true
    }

    pub(crate) fn readonly_extend_drag(&mut self, index: usize) -> bool {
        if self.composing() {
            return false;
        }
        let Some(anchor) = self.input.drag_anchor else {
            return false;
        };
        let index = readonly_navigation::floor_boundary(&self.text, index);
        self.input.select_to(anchor, index);
        true
    }

    /// 只读视觉行的实际布局停点；保留正文、身份、版本与全部编辑历史。
    pub(crate) fn readonly_set_selection(&mut self, anchor: usize, caret: usize) -> bool {
        if self.composing() {
            return false;
        }
        let anchor = readonly_navigation::floor_boundary(&self.text, anchor);
        let caret = readonly_navigation::floor_boundary(&self.text, caret);
        self.input.select_to(anchor, caret);
        self.input.drag_anchor = None;
        true
    }

    /// 范围由 Windows/GPUI 以 UTF-16 提供；内部只在 UTF-8 字符边界替换。
    pub(crate) fn replace(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        marked: bool,
        selected: Option<std::ops::Range<usize>>,
    ) -> bool {
        if !marked && range.is_none() && text.is_empty() && self.composing() {
            // Windows 空组合取消，不把原来被预编辑替换的选中文字丢掉。
            return self.cancel_composition();
        }
        let before = self.snapshot();
        if marked && self.composition_before.is_none() {
            self.composition_before = Some(before.clone());
        }
        let range = range
            .map(|range| {
                crate::text_input::byte_offset(&self.text, range.start)
                    ..crate::text_input::byte_offset(&self.text, range.end)
            })
            .or_else(|| self.input.marked.clone())
            .unwrap_or_else(|| self.input.selection.clone());
        let start = Selection::boundary(&self.text, range.start);
        let end = Selection::boundary(&self.text, range.end).max(start);
        let changed = self.text[start..end] != *text;
        self.text.replace_range(start..end, text);
        if changed {
            self.revision = self.revision.wrapping_add(1);
            self.body_identity = std::sync::Arc::new(());
        }
        self.input.selection = selected
            .map(|range| {
                start + crate::text_input::byte_offset(text, range.start)
                    ..start + crate::text_input::byte_offset(text, range.end)
            })
            .unwrap_or(start + text.len()..start + text.len());
        self.input.reversed = false;
        self.input.drag_anchor = None;
        self.input.marked = (marked && !text.is_empty()).then_some(start..start + text.len());
        if !marked {
            let before = self.composition_before.take().unwrap_or(before);
            self.remember(before);
        } else if text.is_empty() {
            self.cancel_composition();
        }
        true
    }

    /// 平台确认移除标记时保留已接受的文字；与取消/失活的回滚明确分开。
    pub(crate) fn unmark(&mut self) -> bool {
        let had_mark = self.input.marked.take().is_some();
        if let Some(before) = self.composition_before.take() {
            self.remember(before);
        }
        had_mark
    }

    pub(crate) fn cancel_composition(&mut self) -> bool {
        let had_mark = self.input.marked.is_some() || self.composition_before.is_some();
        if let Some(before) = self.composition_before.take() {
            self.restore(before);
        } else {
            self.input.marked = None;
        }
        had_mark
    }

    pub(crate) fn delete(&mut self, backward: bool) {
        let collapsed = self.input.selection.is_empty();
        let range = self.input.deletion_range(&self.text, backward);
        if !range.is_empty() {
            self.input.selection = range;
            if collapsed {
                // 与旧删除相同：Undo 的选择方向仍指向实际 Backspace/Delete 方向。
                self.input.reversed = backward;
            }
            self.replace(None, "", false, None);
        }
    }

    pub(crate) fn undo(&mut self) {
        self.cancel_composition();
        if let Some(before) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(before);
        }
    }

    pub(crate) fn redo(&mut self) {
        self.cancel_composition();
        if let Some(after) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(after);
        }
    }
}

#[cfg(test)]
mod result_edit_tests {
    use super::ResultEditor;

    #[test]
    fn result_edit_cut_from_native_partial_selection_publishes_whole_grapheme_then_commits_once() {
        for (grapheme, prefix) in [
            ("👨‍👩‍👧‍👦", "👨"),
            ("🇨🇳", "🇨"),
            ("👍🏽", "👍"),
            ("e\u{301}", "e"),
            ("\r\n", "\r"),
        ] {
            let original = format!("A{grapheme}Z");
            let selected = 1 + prefix.len()..1 + grapheme.len();
            let mut editor = ResultEditor::new(original.clone(), selected.clone());
            editor.input.reversed = true;
            let identity = editor.body_identity.clone();
            let before = editor.input.clone();
            assert!(editor
                .publish_text_copy(true, |text| {
                    assert_eq!(text, grapheme);
                    Err("模拟发布失败".into())
                })
                .is_err());
            assert_eq!(editor.text, original);
            assert_eq!(editor.input, before);
            assert!(std::sync::Arc::ptr_eq(&editor.body_identity, &identity));
            assert!(editor.undo.is_empty());
            assert!(editor.redo.is_empty());
            assert!(editor
                .publish_text_copy(true, |text| {
                    assert_eq!(text, grapheme);
                    Ok(())
                })
                .unwrap());
            assert_eq!(editor.text, "AZ");
            assert_eq!(editor.undo.len(), 1);
            editor.undo();
            assert_eq!(editor.text, original);
            assert_eq!(editor.input, before);
            editor.redo();
            assert_eq!(editor.text, "AZ");
        }
    }

    #[test]
    fn result_edit_user_delete_removes_one_grapheme_and_keeps_undo_redo_exact() {
        for grapheme in ["👨‍👩‍👧‍👦", "🇨🇳", "👍🏽", "e\u{301}", "\r\n"] {
            let original = format!("A{grapheme}Z");
            for backward in [false, true] {
                let caret = if backward { 1 + grapheme.len() } else { 1 };
                let mut edit = ResultEditor::new(original.clone(), caret..caret);
                let original_identity = edit.body_identity.clone();
                edit.delete(backward);
                assert_eq!(edit.text, "AZ", "{grapheme:?}, backward={backward}");
                assert_eq!(edit.input.selection, 1..1);
                assert_eq!(edit.undo.len(), 1);
                assert!(!std::sync::Arc::ptr_eq(
                    &edit.body_identity,
                    &original_identity
                ));
                edit.undo();
                assert_eq!(edit.text, original);
                assert_eq!(edit.input.reversed, backward);
                assert_eq!(edit.redo.len(), 1);
                edit.redo();
                assert_eq!(edit.text, "AZ");
                assert_eq!(edit.input.selection, 1..1);
            }
        }
        // IME 已结束但合法码点 caret 留在字素内部，两方向均删除该整个字素。
        for (grapheme, prefix) in [
            ("👨‍👩‍👧‍👦", "👨"),
            ("🇨🇳", "🇨"),
            ("👍🏽", "👍"),
            ("e\u{301}", "e"),
            ("\r\n", "\r"),
        ] {
            for backward in [false, true] {
                let original = format!("A{grapheme}Z");
                let caret = 1 + prefix.len();
                let mut edit = ResultEditor::new(original.clone(), caret..caret);
                assert_eq!(edit.caret(), caret);
                edit.delete(backward);
                assert_eq!(
                    edit.text, "AZ",
                    "internal {grapheme:?}, backward={backward}"
                );
                edit.undo();
                assert_eq!(edit.text, original);
                edit.redo();
                assert_eq!(edit.text, "AZ");
            }
        }
        let mut partial = ResultEditor::new("Ae\u{301}Z".into(), 2..4);
        partial.delete(false);
        assert_eq!(partial.text, "AZ");
        partial.undo();
        assert_eq!(partial.text, "Ae\u{301}Z");
        let mut edge = ResultEditor::new("e\u{301}".into(), 0..0);
        let identity = edge.body_identity.clone();
        edge.delete(true);
        assert_eq!(edge.text, "e\u{301}");
        assert_eq!(edge.revision, 0);
        assert!(std::sync::Arc::ptr_eq(&edge.body_identity, &identity));
        assert!(edge.undo.is_empty());
    }

    #[test]
    fn result_edit_mouse_and_keyboard_destinations_keep_complete_graphemes_without_history() {
        let text = "甲👨‍👩‍👧‍👦🇨🇳e\u{301}乙";
        let family_start = "甲".len();
        let flag_start = family_start + "👨‍👩‍👧‍👦".len();
        let accent_start = flag_start + "🇨🇳".len();
        let mut edit = ResultEditor::new(text.into(), 0..0);
        let identity = edit.body_identity.clone();
        edit.begin_drag(family_start + "👨".len());
        assert_eq!(edit.input.selection, family_start..family_start);
        edit.extend_drag(flag_start + "🇨".len());
        assert_eq!(edit.input.selection, family_start..flag_start);
        assert!(edit.end_drag());
        edit.move_to(accent_start + 1, true);
        assert_eq!(edit.input.selection, family_start..accent_start);
        edit.begin_drag(accent_start);
        edit.extend_drag(family_start + "👨".len());
        assert_eq!(edit.input.selection, family_start..accent_start);
        assert!(edit.input.reversed);
        edit.end_drag();
        edit.navigate("right", true, false);
        assert_eq!(edit.input.selection, flag_start..accent_start);
        edit.navigate("right", true, false);
        assert_eq!(edit.input.selection, accent_start..accent_start);
        edit.navigate("left", false, false);
        assert_eq!(edit.caret(), flag_start);
        edit.input.select_to(accent_start + 1, accent_start + 1);
        edit.move_to(family_start, true);
        assert_eq!(
            edit.input.selection,
            family_start..accent_start + "e\u{301}".len()
        );
        assert!(edit.input.reversed);
        edit.input.select_to(accent_start + 1, accent_start + 1);
        edit.move_to(text.len(), true);
        assert_eq!(edit.input.selection, accent_start..text.len());
        assert!(!edit.input.reversed);
        assert_eq!(edit.text, text);
        assert_eq!(edit.revision, 0);
        assert!(std::sync::Arc::ptr_eq(&edit.body_identity, &identity));
        assert!(edit.undo.is_empty());
        assert!(edit.redo.is_empty());
    }

    #[test]
    fn result_edit_native_partial_grapheme_replacement_and_mark_selection_stay_codepoint_exact() {
        let mut edit = ResultEditor::new("e\u{301}Z".into(), 0..0);
        edit.replace(Some(1..2), "\u{300}", false, None);
        assert_eq!(edit.text, "e\u{300}Z");
        assert_eq!(edit.input.selection, 3..3);
        edit.undo();
        assert_eq!(edit.text, "e\u{301}Z");
        edit.input.select_to(3, 1);
        edit.replace(Some(0..2), "e\u{300}", true, Some(1..1));
        assert_eq!(edit.text, "e\u{300}Z");
        assert_eq!(edit.input.marked, Some(0..3));
        assert_eq!(edit.input.selection, 1..1);
        assert!(edit.cancel_composition());
        assert_eq!(edit.text, "e\u{301}Z");
        assert_eq!(edit.input.selection, 1..3);
        assert!(edit.input.reversed);
        assert!(!edit.composing());
        // 代理对保护仍存在，而完整 ZWJ 族内部的合法 UTF-16 码点允许单独替换。
        let mut emoji = ResultEditor::new("👩‍💻Z".into(), 0..0);
        emoji.replace(Some(0..2), "👨", false, None);
        assert_eq!(emoji.text, "👨‍💻Z");
        assert_eq!(emoji.input.selection, "👨".len().."👨".len());
    }

    #[test]
    fn result_edit_multiline_utf16_replace_keeps_unicode_and_selection() {
        let mut edit = ResultEditor::new("甲🙂\n乙".into(), 0..0);
        edit.replace(Some(1..3), "😀\n中文", false, Some(2..3));
        assert_eq!(edit.text, "甲😀\n中文\n乙");
        assert_eq!(edit.input.selection, 7..8);
        assert_eq!(edit.selected_or_full(), "\n");
        edit.select_all();
        edit.replace(None, "第一行\r\n第二行\n😀", false, None);
        assert_eq!(edit.text, "第一行\r\n第二行\n😀");
        assert_eq!(edit.caret(), edit.text.len());
        assert_eq!(edit.selected_or_full(), edit.text);
    }

    #[test]
    fn result_edit_composition_commit_cancel_and_unmark_are_separate() {
        let original = "甲old\n乙";
        let mut edit = ResultEditor::new(original.into(), 3..6);
        edit.replace(None, "中", true, Some(1..1));
        edit.replace(None, "中文", true, Some(2..2));
        assert_eq!(edit.text, "甲中文\n乙");
        assert_eq!(edit.input.marked, Some(3..9));
        edit.replace(None, "", false, None);
        assert_eq!(edit.text, original);
        assert_eq!(edit.input.selection, 3..6);
        assert!(!edit.composing());
        edit.replace(None, "中", true, None);
        edit.replace(None, "国", false, None);
        assert_eq!(edit.text, "甲国\n乙");
        assert!(!edit.composing());
        edit.undo();
        assert_eq!(edit.text, original);
        edit.redo();
        assert_eq!(edit.text, "甲国\n乙");
        edit.replace(None, "文", true, None);
        assert!(edit.unmark());
        assert_eq!(edit.text, "甲国文\n乙");
        edit.undo();
        assert_eq!(edit.text, "甲国\n乙");
        edit.replace(None, "预编辑", true, None);
        assert!(edit.cancel_composition());
        assert_eq!(edit.text, "甲国\n乙");
    }

    #[test]
    fn result_edit_local_history_and_new_session_keep_modified_full_text() {
        let mut edit = ResultEditor::new("old\ntext".into(), 0..3);
        edit.replace(None, "新", false, None);
        edit.move_to(edit.text.len(), false);
        edit.replace(None, "\n长全文", false, None);
        assert_eq!(edit.selected_or_full(), "新\ntext\n长全文");
        edit.undo();
        assert_eq!(edit.text, "新\ntext");
        edit.undo();
        assert_eq!(edit.text, "old\ntext");
        edit.redo();
        edit.redo();
        assert_eq!(edit.text, "新\ntext\n长全文");
        let long = "中文".repeat(3000);
        edit.select_all();
        edit.replace(None, &long, false, None);
        assert_eq!(edit.text, long);
        let fresh = ResultEditor::new(edit.text.clone(), 0..0);
        assert_eq!(fresh.input.selection, 0..0);
        assert!(!fresh.composing());
        assert!(!fresh.dragging());
        assert_eq!(fresh.selected_or_full(), long);
    }

    #[test]
    fn result_edit_reverse_drag_navigation_and_deletion_keep_utf8_boundaries() {
        let mut edit = ResultEditor::new("甲😀乙\n第二行".into(), 0..0);
        edit.begin_drag(edit.text.len());
        edit.extend_drag(3);
        assert!(edit.input.reversed);
        assert_eq!(edit.input.selection, 3..edit.text.len());
        assert!(edit.end_drag());
        edit.navigate("right", true, false);
        assert_eq!(edit.input.selection, 7..edit.text.len());
        edit.navigate("left", true, false);
        assert_eq!(edit.input.selection, 3..edit.text.len());
        edit.move_to(5, false);
        assert_eq!(edit.caret(), 3);
        edit.delete(true);
        assert_eq!(edit.text, "😀乙\n第二行");
        edit.delete(false);
        assert_eq!(edit.text, "乙\n第二行");
        edit.undo();
        edit.undo();
        assert_eq!(edit.text, "甲😀乙\n第二行");
    }
}

#[cfg(test)]
mod selection_tests {
    use super::Selection;

    #[test]
    fn result_selection_reverse_multiline_keeps_chinese_emoji_and_newlines() {
        let text = "甲😀乙\r\n第二行\n尾";
        let start = text.find('😀').unwrap();
        let end = text.find('尾').unwrap();
        let mut selection = Selection::default();
        selection.begin(text, end);
        selection.extend(text, start);
        assert_eq!(selection.range(), start..end);
        assert_eq!(selection.selected_or_full(text), "😀乙\r\n第二行\n");
        assert!(selection.end_drag());
        selection.extend(text, text.len());
        assert_eq!(selection.range(), start..end);
        selection.begin(text, start);
        selection.extend(text, end);
        assert_eq!(selection.selected_or_full(text), "😀乙\r\n第二行\n");
    }

    #[test]
    fn result_selection_clamps_byte_boundaries_and_collapsed_copy_uses_full_text() {
        let text = "甲😀乙";
        let mut selection = Selection::default();
        selection.begin(text, 4);
        selection.extend(text, 6);
        assert_eq!(selection.range(), 3..3);
        assert_eq!(selection.selected_or_full(text), text);
        selection.extend(text, usize::MAX);
        assert_eq!(selection.selected_or_full(text), "😀乙");
        selection.begin(text, usize::MAX);
        assert_eq!(selection.range(), text.len()..text.len());
        assert_eq!(selection.selected_or_full(text), text);
    }

    #[test]
    fn result_selection_full_select_reset_and_lost_drag_preserve_readonly_text() {
        let text = "中文\n🙂 hello";
        let mut selection = Selection::default();
        selection.select_all(text);
        assert_eq!(selection.range(), 0..text.len());
        assert!(!selection.dragging());
        assert_eq!(selection.selected_or_full(text), text);
        selection.begin(text, 0);
        selection.extend(text, 6);
        assert!(selection.end_drag());
        selection.extend(text, text.len());
        assert_eq!(selection.selected_or_full(text), "中文");
        selection.reset();
        assert_eq!(selection.range(), 0..0);
        assert!(!selection.dragging());
        assert_eq!(selection.selected_or_full(text), text);
        selection.begin(text, 0);
        selection.reset();
        assert!(!selection.dragging());
        selection.select_all("");
        assert_eq!(selection.selected_or_full(""), "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_current_success_copies_the_exact_text_and_closes() {
        assert_eq!(
            route(Ok("第一行\n second line \n".into()), true, true),
            Completion::CopyAndClose("第一行\n second line \n".into())
        );
    }

    #[test]
    fn disabled_success_keeps_the_existing_result_panel_route() {
        assert_eq!(
            route(Ok("panel text".into()), false, true),
            Completion::ShowResult("panel text".into())
        );
        // 普通面板不受新自动输出目标门影响。
        assert_eq!(
            route(Ok("panel text".into()), false, false),
            Completion::ShowResult("panel text".into())
        );
    }

    #[test]
    fn empty_or_whitespace_copy_result_stays_open_without_copying() {
        for text in ["", " \t\r\n", "\u{3000}"] {
            assert_eq!(
                route(Ok(text.into()), true, true),
                Completion::ShowResult(text.into())
            );
        }
        assert_eq!(
            route(Ok(String::new()), false, true),
            Completion::ShowResult(String::new())
        );
        assert_eq!(
            route(Ok(" \t".into()), false, true),
            Completion::ShowResult(" \t".into())
        );
    }

    #[test]
    fn failure_keeps_the_window_open_and_preserves_the_actual_error() {
        for copy_only in [false, true] {
            for target_current in [false, true] {
                assert_eq!(
                    route(
                        Err("OCR language unavailable".into()),
                        copy_only,
                        target_current
                    ),
                    Completion::KeepOpenWithError("OCR language unavailable".into())
                );
            }
        }
    }

    #[test]
    fn changed_target_or_new_input_cannot_copy_or_close_a_success() {
        assert_eq!(
            route(Ok("late text".into()), true, false),
            Completion::KeepOpenWithError("识别期间截图范围或内容已改变，请重新识别文字".into())
        );
    }
}

#[cfg(test)]
mod recognition_session_tests {
    use super::super::ocr_task::Receipt;
    use super::{InputTarget, OcrRequestKind, OcrTarget, Origin, RecognitionSession, ResultEditor};
    use crate::{
        geometry::{Rect, ScreenRect},
        scene::{PixelPlane, SourceIdentity, SourcePixels},
    };

    static FROZEN: [u8; 128] = [0; 128];
    static REPLACEMENT: [u8; 128] = [1; 128];

    fn ocr_target(pixels: &'static [u8]) -> OcrTarget {
        let full = Rect::full(8, 4);
        OcrTarget {
            canvas: ScreenRect {
                left: -8,
                top: -2,
                width: 8,
                height: 4,
            },
            selected: Rect {
                x: 2,
                y: 1,
                width: 4,
                height: 2,
            },
            revisions: (4, 7),
            stamp: (11, 13),
            identity: SourceIdentity::of(&SourcePixels {
                fill: None,
                canvas: full,
                planes: vec![PixelPlane {
                    rect: full,
                    bgra: pixels,
                }],
                overlay: None,
            }),
        }
    }

    #[test]
    fn initial_replacement_cancels_old_owner_without_releasing_new_identity() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        let old = session.begin_initial(target.clone(), 0);
        let old_token = session.initial_token(old.id).unwrap();
        let new = session.begin_initial(target, 0);
        let new_token = session.initial_token(new.id).unwrap();
        assert!(old_token.checkpoint().is_err());
        assert!(session.finish_initial(old.id).is_none());
        assert!(session.has_pending());
        assert!(new_token.checkpoint().is_ok());
        assert!(session.initial_token(old.id).is_none());
    }

    #[test]
    fn completed_initial_receipt_survives_owner_drop_cancel_flag() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        let request = session.begin_initial(target.clone(), 0);
        let token = session.initial_token(request.id).unwrap();
        let receipt = Receipt::from_result(Ok(String::from("已识别")));
        let owned = session.finish_initial(request.id).unwrap();
        assert!(token.is_cancelled());
        assert!(session.request_is_current(&owned, &target, 0));
        assert_eq!(receipt, Receipt::Completed(String::from("已识别")));
        assert!(!session.has_pending());
    }

    #[test]
    fn old_retry_completion_does_not_take_a_new_retry_owner() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        session.show(Origin::Ocr(target.clone()));
        let old = session.begin_retry(target.clone(), 0).unwrap();
        let old_token = session.retry_token(old.id).unwrap();
        session.show(Origin::Ocr(target.clone()));
        let new = session.begin_retry(target, 0).unwrap();
        let new_token = session.retry_token(new.id).unwrap();
        assert!(old_token.is_cancelled());
        assert!(session.finish_retry(old.id).is_none());
        assert!(session.retry_busy());
        assert!(new_token.checkpoint().is_ok());
    }

    #[test]
    fn retry_cancellation_for_committed_text_edit_keeps_edited_body_and_origin() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        session.show(Origin::Ocr(target.clone()));
        let mut editor = ResultEditor::new("原文\n第二行".into(), 0..0);
        let request = session
            .begin_retry(target.clone(), editor.revision)
            .unwrap();
        let token = session.retry_token(request.id).unwrap();
        editor.select_all();
        editor.replace(None, "用户保留的修改", false, None);
        let kept = editor.text.clone();
        let revision = editor.revision;
        assert!(!session.invalidate_stale(Some(&target), revision, true));
        assert!(token.is_cancelled());
        assert!(!session.retry_busy());
        assert!(session.finish_retry(request.id).is_none());
        assert_eq!(editor.text, kept);
        assert_eq!(editor.revision, revision);
        assert!(session.ocr_target() == Some(&target));
    }

    #[test]
    fn retry_cancellation_preserves_origin_and_does_not_release_foreground_task() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        session.show(Origin::Ocr(target.clone()));
        let request = session.begin_retry(target.clone(), 0).unwrap();
        let token = session.retry_token(request.id).unwrap();
        let mut foreground = super::super::ocr_task::TaskSlot::default();
        let other = foreground.start(InputTarget::Frozen(target.clone()));
        assert!(!session.invalidate_requests());
        assert!(token.is_cancelled());
        assert!(foreground.owns(other.id));
        assert!(other.token.checkpoint().is_ok());
        assert!(session.ocr_target() == Some(&target));
    }

    #[test]
    fn stable_roi_change_cancels_initial_and_keeps_old_receipt_unowned() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        let request = session.begin_initial(target.clone(), 0);
        let token = session.initial_token(request.id).unwrap();
        let mut preview = target;
        preview.selected.x += 1;
        assert!(!session.invalidate_stale(Some(&preview), 0, false));
        assert!(token.checkpoint().is_ok());
        assert!(session.invalidate_stale(Some(&preview), 0, true));
        assert!(token.is_cancelled());
        assert!(session.finish_initial(request.id).is_none());
    }

    #[test]
    fn source_replacement_cancels_during_transient_roi_without_touching_other_window() {
        let target = ocr_target(&FROZEN);
        let mut a = RecognitionSession::default();
        let mut b = RecognitionSession::default();
        let request_a = a.begin_initial(target.clone(), 0);
        let request_b = b.begin_initial(target, 0);
        let token_a = a.initial_token(request_a.id).unwrap();
        let token_b = b.initial_token(request_b.id).unwrap();
        assert!(a.invalidate_stale(Some(&ocr_target(&REPLACEMENT)), 0, false));
        assert!(token_a.is_cancelled());
        assert!(token_b.checkpoint().is_ok());
        assert!(b.finish_initial(request_b.id).is_some());
    }

    #[test]
    fn content_revision_and_scene_change_cancel_only_the_matching_pending_owner() {
        for revision_change in [true, false] {
            let target = ocr_target(&FROZEN);
            let mut session = RecognitionSession::default();
            let request = session.begin_initial(target.clone(), 0);
            let token = session.initial_token(request.id).unwrap();
            let mut changed = target;
            if revision_change {
                changed.revisions.1 += 1;
            } else {
                changed.stamp.1 += 1;
            }
            assert!(session.invalidate_stale(Some(&changed), 0, true));
            assert!(token.is_cancelled());
            assert!(session.finish_initial(request.id).is_none());
        }
    }

    #[test]
    fn missing_confirmed_roi_releases_initial_and_retry_without_replacing_origin() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        let initial = session.begin_initial(target.clone(), 0);
        let initial_token = session.initial_token(initial.id).unwrap();
        assert!(session.invalidate_stale(None, 0, true));
        assert!(initial_token.is_cancelled());
        session.show(Origin::Ocr(target.clone()));
        let retry = session.begin_retry(target.clone(), 0).unwrap();
        let token = session.retry_token(retry.id).unwrap();
        assert!(!session.invalidate_stale(None, 0, true));
        assert!(token.is_cancelled());
        assert!(!session.has_pending());
        assert!(session.ocr_target() == Some(&target));
    }

    #[test]
    fn refused_close_retains_live_session_true_drop_cancels_its_initial_and_retry() {
        // Host 的 saving / finish-content 拒绝关闭保留 View；该模型不调用 invalidate。
        // 真关闭释放 View 时两个独立任务槽的 Drop 才发出本 Owner 取消请求。
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        let initial = session.begin_initial(target.clone(), 0);
        let token = session.initial_token(initial.id).unwrap();
        assert!(session.has_pending());
        assert!(token.checkpoint().is_ok());
        drop(session);
        assert!(token.is_cancelled());
        let mut session = RecognitionSession::default();
        session.show(Origin::Ocr(target.clone()));
        let retry = session.begin_retry(target, 0).unwrap();
        let token = session.retry_token(retry.id).unwrap();
        assert!(session.retry_busy());
        assert!(token.checkpoint().is_ok());
        drop(session);
        assert!(token.is_cancelled());
    }

    #[test]
    fn frozen_uia_target_ignores_annotation_versions_but_content_ocr_does_not() {
        let target = ocr_target(&FROZEN);
        let frozen = InputTarget::Frozen(target.clone());
        let content = InputTarget::Content(target.clone());
        let mut changed = target;
        changed.revisions.0 += 1;
        changed.stamp.1 += 1;
        assert!(frozen.is_current(&changed, true));
        assert!(!content.is_current(&changed, true));
        changed.selected.x += 1;
        assert!(!frozen.is_current(&changed, true));
        assert!(frozen.is_current(&changed, false));
        assert!(!frozen.is_current(&ocr_target(&REPLACEMENT), false));
    }

    #[test]
    fn raw_frozen_identity_ignores_cursor_overlay_but_content_identity_keeps_it() {
        let full = Rect::full(8, 4);
        let source = |cursor: bool| SourcePixels {
            fill: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &FROZEN,
            }],
            overlay: cursor.then_some(PixelPlane {
                rect: full,
                bgra: &REPLACEMENT,
            }),
        };
        assert!(SourceIdentity::of(&source(false)) != SourceIdentity::of(&source(true)));
        assert!(
            super::frozen_source_identity(source(false))
                == super::frozen_source_identity(source(true))
        );
    }

    #[test]
    fn uncommitted_result_navigation_keeps_retry_token_current() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        session.show(Origin::Ocr(target.clone()));
        let request = session.begin_retry(target.clone(), 0).unwrap();
        let token = session.retry_token(request.id).unwrap();
        let mut editor = ResultEditor::new("原文\n第二行".into(), 0..0);
        editor.select_all();
        editor.navigate("left", true, false);
        assert!(!session.invalidate_stale(Some(&target), editor.revision, true));
        assert!(token.checkpoint().is_ok());
        assert!(session.retry_busy());
    }

    #[test]
    fn ocr_request_owner_and_session_reject_late_initial_or_retry_completion() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        let old = session.begin_initial(target.clone(), 0);
        let new = session.begin_initial(target.clone(), 0);
        assert_eq!(old.kind, OcrRequestKind::Initial);
        assert_ne!(old.id, new.id);
        assert!(session.finish_initial(old.id).is_none());
        assert!(session.finish_initial(new.id).is_some());
        assert!(session.request_is_current(&new, &target, 0));
        assert!(session.finish_initial(new.id).is_none());

        let initial = session.begin_initial(target.clone(), 0);
        assert!(session.show(Origin::Ocr(target.clone())));
        assert!(session.finish_initial(initial.id).is_none());
        assert!(!session.request_is_current(&initial, &target, 0));
        let old_retry = session.begin_retry(target.clone(), 0).unwrap();
        assert_eq!(old_retry.kind, OcrRequestKind::Retry);
        assert!(session.retry_busy());
        assert!(!session.show(Origin::Ocr(target.clone())));
        assert!(!session.retry_busy());
        let new_retry = session.begin_retry(target.clone(), 0).unwrap();
        assert!(session.finish_retry(old_retry.id).is_none());
        assert!(session.retry_busy());
        assert!(!session.request_is_current(&old_retry, &target, 0));
        let accepted = session.finish_retry(new_retry.id).unwrap();
        assert!(accepted == new_retry);
        assert!(session.request_is_current(&accepted, &target, 0));
        assert!(!session.retry_busy());

        let cancelled = session.begin_retry(target.clone(), 0).unwrap();
        assert!(!session.dismiss());
        assert!(session.finish_retry(cancelled.id).is_none());
        assert!(!session.request_is_current(&cancelled, &target, 0));
        assert!(session.ocr_target().is_none());
        let initial = session.begin_initial(target.clone(), 0);
        assert!(session.invalidate_initial());
        assert!(!session.invalidate_initial());
        assert!(session.finish_initial(initial.id).is_none());

        // 另一类任务真正获准开始时，撤销 OCR 请求但保留旧全文的来源。
        session.show(Origin::Ocr(target.clone()));
        let retry = session.begin_retry(target.clone(), 0).unwrap();
        assert!(!session.invalidate_requests());
        assert!(session.finish_retry(retry.id).is_none());
        assert!(!session.retry_busy());
        assert!(session.ocr_target() == Some(&target));
        let initial = session.begin_initial(target.clone(), 0);
        assert!(session.invalidate_requests());
        assert!(session.finish_initial(initial.id).is_none());
        assert!(!session.invalidate_requests());
    }

    #[test]
    fn ocr_retry_requires_ocr_origin_exact_target_and_an_unowned_slot() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        assert!(session.begin_retry(target.clone(), 0).is_none());
        for origin in [Origin::ControlText, Origin::Qr] {
            session.show(origin);
            assert!(session.begin_retry(target.clone(), 0).is_none());
            // 显式工具栏 OCR 可以从其他来源发起，但完成仍核自己的会话和目标。
            let initial = session.begin_initial(target.clone(), 0);
            assert!(session.request_is_current(&initial, &target, 0));
            assert!(session.finish_initial(initial.id).is_some());
        }
        session.show(Origin::Ocr(target.clone()));
        let initial = session.begin_initial(target.clone(), 0);
        assert!(session.begin_retry(target.clone(), 0).is_none());
        assert!(session.finish_initial(initial.id).is_some());
        let retry = session.begin_retry(target.clone(), 0).unwrap();
        assert!(session.begin_retry(target.clone(), 0).is_none());
        assert!(session.finish_retry(retry.id).is_some());

        let mut changed = Vec::new();
        let mut canvas = target.clone();
        canvas.canvas.left -= 1;
        changed.push(canvas);
        let mut selected = target.clone();
        selected.selected.x += 1;
        changed.push(selected);
        for field in 0..2 {
            let mut revision = target.clone();
            if field == 0 {
                revision.revisions.0 += 1;
            } else {
                revision.revisions.1 += 1;
            }
            changed.push(revision);
            let mut scene = target.clone();
            if field == 0 {
                scene.stamp.0 += 1;
            } else {
                scene.stamp.1 += 1;
            }
            changed.push(scene);
        }
        changed.push(ocr_target(&REPLACEMENT));
        for changed in changed {
            assert!(session.begin_retry(changed.clone(), 0).is_none());
            assert!(!session.request_is_current(&retry, &changed, 0));
        }
        assert!(!session.request_is_current(&retry, &target, 1));
        assert!(session.request_is_current(&retry, &target, 0));
    }

    #[test]
    fn ocr_retry_uses_actual_editor_revision_across_navigation_toggle_and_text_aba() {
        let target = ocr_target(&FROZEN);
        let mut session = RecognitionSession::default();
        session.show(Origin::Ocr(target.clone()));
        let original = "甲🙂\n乙";
        let mut editor = ResultEditor::new(original.into(), 0..0);
        editor.revision = session.content_revision;
        let retry = session
            .begin_retry(target.clone(), editor.revision)
            .unwrap();
        editor.select_all();
        editor.move_to(editor.text.len(), false);
        editor.navigate("left", true, false);
        editor.begin_drag(0);
        editor.extend_drag(editor.text.len());
        editor.end_drag();
        assert_eq!(editor.revision, 0);
        assert!(session.request_is_current(&retry, &target, editor.revision));

        // 未改正文的收起/展开只转交实际 revision，不改变识别会话或任务。
        session.content_revision = editor.revision;
        let mut reopened = ResultEditor::new(editor.text.clone(), 0..0);
        reopened.revision = session.content_revision;
        assert!(session.request_is_current(&retry, &target, reopened.revision));
        reopened.select_all();
        reopened.replace(None, "改过\n全文", false, None);
        reopened.undo();
        assert_eq!(reopened.text, original);
        assert_eq!(reopened.revision, 2);
        assert!(!session.request_is_current(&retry, &target, reopened.revision));
        session.content_revision = reopened.revision;
        let mut reopened = ResultEditor::new(reopened.text.clone(), 0..0);
        reopened.revision = session.content_revision;
        assert!(!session.request_is_current(&retry, &target, reopened.revision));
        assert!(session.finish_retry(retry.id).is_some());

        let retry = session
            .begin_retry(target.clone(), reopened.revision)
            .unwrap();
        reopened.replace(None, "预编辑", true, None);
        reopened.cancel_composition();
        assert_eq!(reopened.text, original);
        assert_eq!(reopened.revision, 4);
        assert!(!session.request_is_current(&retry, &target, reopened.revision));
        // 新的同文结果仍是新会话，不能把版本归零变成旧请求的授权。
        session.show(Origin::Ocr(target.clone()));
        assert_eq!(session.content_revision, 0);
        assert!(!session.request_is_current(&retry, &target, 0));
    }
}

#[cfg(test)]
mod auto_copy_publication_tests {
    use super::{publish_auto_copy, AutoCopyPublication};
    use std::cell::Cell;

    #[test]
    fn successful_write_authorizes_close_after_exact_text_publication() {
        let original = " 第一行😀\r\n second\tline \n";
        let publication = publish_auto_copy(original.into(), |text| {
            assert_eq!(text, original);
            Ok::<(), &'static str>(())
        });
        assert_eq!(publication, AutoCopyPublication::Close);
    }

    #[test]
    fn failed_write_returns_owned_unicode_whitespace_and_original_error() {
        let original = "\t 中文e\u{301}😀\r\n \u{3000}\0 ";
        assert_eq!(
            publish_auto_copy(original.into(), |_| Err("剪贴板占用")),
            AutoCopyPublication::KeepResult {
                text: original.into(),
                error: "剪贴板占用",
            }
        );
    }

    #[test]
    fn failed_writer_is_called_once_without_automatic_retry() {
        let calls = Cell::new(0);
        let publication = publish_auto_copy("原文".into(), |_| {
            calls.set(calls.get() + 1);
            Err("busy")
        });
        assert!(matches!(
            publication,
            AutoCopyPublication::KeepResult { .. }
        ));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn successful_writer_is_called_once() {
        let calls = Cell::new(0);
        let publication = publish_auto_copy("original".into(), |_| {
            calls.set(calls.get() + 1);
            Ok::<(), &'static str>(())
        });
        assert_eq!(publication, AutoCopyPublication::Close);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn failed_owned_text_can_publish_later_with_only_one_close_authorization() {
        let original = " 中文\r\n unchanged 😀 ";
        let calls = Cell::new(0);
        let first = publish_auto_copy(original.into(), |_| {
            calls.set(calls.get() + 1);
            Err("busy")
        });
        let AutoCopyPublication::KeepResult { text, error } = first else {
            panic!("失败不得授权关闭");
        };
        assert_eq!(error, "busy");
        assert_eq!(text, original);
        let next = publish_auto_copy(text, |text| {
            calls.set(calls.get() + 1);
            assert_eq!(text, original);
            Ok::<(), &'static str>(())
        });
        // 首次失败没有 Close；仅显式后一次发布成功产生一份关闭授权。
        assert_eq!(next, AutoCopyPublication::Close);
        assert_eq!(calls.get(), 2);
    }
}

#[cfg(test)]
mod local_retry_body_identity_tests {
    use super::ResultEditor;
    use std::sync::Arc;

    #[test]
    fn real_replace_undo_redo_cannot_restore_installed_body_identity() {
        let original = "甲 e\u{301}\r\n 乙";
        let mut editor = ResultEditor::new(original.into(), 0..0);
        let installed = Arc::clone(&editor.body_identity);
        editor.select_all();
        assert!(editor.replace(None, "候选", false, None));
        assert!(!Arc::ptr_eq(&installed, &editor.body_identity));
        editor.undo();
        assert_eq!(editor.text, original);
        assert!(!Arc::ptr_eq(&installed, &editor.body_identity));
        let undone = Arc::clone(&editor.body_identity);
        editor.redo();
        assert_eq!(editor.text, "候选");
        assert!(!Arc::ptr_eq(&undone, &editor.body_identity));
    }

    #[test]
    fn real_revision_wrap_and_composition_cancel_keep_body_aba_distinct() {
        let mut editor = ResultEditor::new("原文".into(), 0..0);
        let installed = Arc::clone(&editor.body_identity);
        editor.revision = u64::MAX;
        editor.select_all();
        assert!(editor.replace(None, "预编辑", true, None));
        assert_eq!(editor.revision, 0);
        assert!(editor.cancel_composition());
        assert_eq!(editor.text, "原文");
        assert!(!Arc::ptr_eq(&installed, &editor.body_identity));
    }

    #[test]
    fn real_selection_navigation_and_equal_replace_preserve_body_identity() {
        let mut editor = ResultEditor::new("甲\r\n e\u{301}".into(), 0..0);
        let installed = Arc::clone(&editor.body_identity);
        editor.begin_drag(0);
        editor.extend_drag(editor.text.len());
        editor.end_drag();
        editor.navigate("left", true, false);
        editor.select_all();
        let unchanged = editor.text.clone();
        // replace 表示输入已处理，等字节替换仍会把选择折叠到尾部。
        assert!(editor.replace(None, &unchanged, false, None));
        assert_eq!(editor.text, unchanged);
        assert!(editor.undo.is_empty());
        assert!(Arc::ptr_eq(&installed, &editor.body_identity));
        assert_eq!(editor.revision, 0);
    }
}

#[cfg(test)]
mod table_click_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn table_down_cannot_copy_replacement_result_or_other_presentation() {
        let pressed = TableClickStamp {
            source: 7,
            frame: ResultFrame {
                generation: 3,
                revision: None,
            },
            body_identity: Arc::new(()),
            inline: true,
        };
        assert!(pressed.matches(&pressed.clone()));
        let mut current = pressed.clone();
        current.body_identity = Arc::new(());
        assert!(!pressed.matches(&current));
        current = pressed.clone();
        current.frame.generation += 1;
        assert!(!pressed.matches(&current));
        current = pressed.clone();
        current.source += 1;
        assert!(!pressed.matches(&current));
        current = pressed.clone();
        current.inline = false;
        assert!(!pressed.matches(&current));
    }

    #[test]
    fn table_down_rejects_real_edit_undo_aba_even_with_restored_bytes() {
        let mut editor = ResultEditor::new("甲🙂\n乙".into(), 0..0);
        let original = editor.text.clone();
        let pressed = TableClickStamp {
            source: 7,
            frame: ResultFrame {
                generation: 3,
                revision: Some(editor.revision),
            },
            body_identity: Arc::clone(&editor.body_identity),
            inline: false,
        };
        editor.replace(Some(0..1), "丙", false, None);
        editor.undo();
        assert_eq!(editor.text.as_bytes(), original.as_bytes());
        let current = TableClickStamp {
            source: 7,
            frame: ResultFrame {
                generation: 3,
                revision: Some(editor.revision),
            },
            body_identity: Arc::clone(&editor.body_identity),
            inline: false,
        };
        assert!(!pressed.matches(&current));
    }
}

#[cfg(test)]
mod candidate_key_repeat_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn held_candidate_keys_only_repeat_readonly_navigation() {
        for key in ["left", "right", "up", "down", "home", "end"] {
            assert!(candidate_key_repeat_allowed(key, true));
        }
        for key in [
            "c",
            "a",
            "escape",
            "enter",
            "backspace",
            "delete",
            "v",
            "x",
            "z",
        ] {
            assert!(!candidate_key_repeat_allowed(key, true));
            assert!(candidate_key_repeat_allowed(key, false));
        }
    }

    #[test]
    fn held_readonly_extension_preserves_graphemes_body_and_history() {
        let text = "e\u{301}👩‍💻\r\n中文。end";
        let mut editor = ResultEditor::new(text.into(), 0..0);
        let identity = Arc::clone(&editor.body_identity);
        for end in [
            "e\u{301}".len(),
            "e\u{301}👩‍💻".len(),
            "e\u{301}👩‍💻\r\n".len(),
            "e\u{301}👩‍💻\r\n中".len(),
        ] {
            assert!(candidate_key_repeat_allowed("right", true));
            assert!(editor.readonly_navigate("right", true, false));
            assert_eq!(editor.input.selection, 0..end);
            assert_eq!(editor.caret(), end);
        }
        assert_eq!(editor.text.as_bytes(), text.as_bytes());
        assert!(Arc::ptr_eq(&identity, &editor.body_identity));
        assert_eq!(editor.revision, 0);
        assert!(editor.undo.is_empty() && editor.redo.is_empty());
        assert!(!editor.composing());
    }

    #[test]
    fn image_fulltext_clipboard_failure_preserves_body_selection_and_undo_redo() {
        let mut editor = ResultEditor::new("甲😀乙".into(), 0.."甲😀".len());
        assert!(editor.replace(Some(0..1), "丙", false, None));
        editor.undo();
        let text = editor.text.clone();
        let selection = editor.input.selection.clone();
        let identity = Arc::clone(&editor.body_identity);
        let revision = editor.revision;
        let history = (editor.undo.len(), editor.redo.len());
        for cut in [false, true] {
            assert!(editor
                .publish_text_copy(cut, |_| Err("模拟剪贴板被占用".into()))
                .is_err());
            assert_eq!(editor.text, text);
            assert_eq!(editor.input.selection, selection);
            assert_eq!(editor.revision, revision);
            assert!(Arc::ptr_eq(&identity, &editor.body_identity));
            assert_eq!((editor.undo.len(), editor.redo.len()), history);
        }
    }

    #[test]
    fn image_fulltext_cut_commits_once_only_after_success_and_empty_cut_never_copies_full() {
        let original = "甲😀乙";
        let mut editor = ResultEditor::new(original.into(), 0.."甲😀".len());
        let mut published = Vec::new();
        assert!(editor
            .publish_text_copy(true, |text| {
                published.push(text.to_owned());
                Ok(())
            })
            .unwrap());
        assert_eq!(published, ["甲😀"]);
        assert_eq!(editor.text, "乙");
        assert_eq!(editor.undo.len(), 1);
        editor.undo();
        assert_eq!(editor.text, original);
        editor.input.selection = 0..0;
        assert!(!editor
            .publish_text_copy(true, |_| panic!("空选区不能剪切全文"))
            .unwrap());
        assert_eq!(editor.text, original);
        assert_eq!(editor.redo.len(), 1);
    }
}
