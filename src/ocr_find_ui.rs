//! 两个 OCR 宿主共用的小查询栏；只在输入事件扫描，渲染借用缓存。
use crate::{
    ocr_markup::TextMark,
    pin_text_selection::{find::*, SelectionText},
    text_field::TextField,
};
use gpui::{div, prelude::*, px, rgb, App, Context, Entity, Subscription, Window};
use std::{cell::Cell, sync::Arc};

#[derive(Clone)]
pub(crate) struct FindQueryRefreshTicket {
    session: Arc<()>,
    revision: Arc<()>,
}
struct QueryRefresh {
    session: Arc<()>,
    revision: Arc<()>,
    pending: bool,
    queued: Cell<bool>,
}
impl QueryRefresh {
    fn new(session: Arc<()>, revision: Arc<()>) -> Self {
        Self {
            session,
            revision,
            pending: false,
            queued: Cell::new(false),
        }
    }
    fn mark(&mut self, revision: Arc<()>) {
        self.revision = revision;
        self.pending = true;
        self.queued.set(false);
    }
    fn ticket(&self) -> Option<FindQueryRefreshTicket> {
        (self.pending && !self.queued.get()).then(|| FindQueryRefreshTicket {
            session: Arc::clone(&self.session),
            revision: Arc::clone(&self.revision),
        })
    }
    fn current(&self, ticket: &FindQueryRefreshTicket) -> bool {
        self.pending
            && Arc::ptr_eq(&self.session, &ticket.session)
            && Arc::ptr_eq(&self.revision, &ticket.revision)
    }
    fn queue(&self, ticket: &FindQueryRefreshTicket) -> bool {
        self.current(ticket) && !self.queued.replace(true)
    }
    fn release(&self, ticket: &FindQueryRefreshTicket) {
        if self.current(ticket) {
            self.queued.set(false);
        }
    }
    fn consume(&mut self) {
        self.pending = false;
        self.queued.set(false);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FindClickPhase {
    Down,
    Up,
}

#[derive(Clone)]
pub(crate) struct FindButtonStamp<S> {
    pub(crate) action: FindAction,
    session: Arc<()>,
    revision: Arc<()>,
    current: Option<OcrFindTicket<S>>,
    all: Option<OcrFindBatchTicket<S>>,
    summary: OcrFindSummary,
    mark_choice: FindMarkStamp,
}

/// 类型和选择器开合各自换代；枚举改回不能恢复旧按钮意图。
#[derive(Clone)]
pub(crate) struct FindMarkStamp {
    kind: TextMark,
    identity: Arc<()>,
    picker_open: bool,
}
impl FindMarkStamp {
    pub(crate) fn kind(&self) -> TextMark {
        self.kind
    }
}
struct FindMarkChoice {
    stamp: FindMarkStamp,
}
impl Default for FindMarkChoice {
    fn default() -> Self {
        Self {
            stamp: FindMarkStamp {
                kind: TextMark::Highlight,
                identity: Arc::new(()),
                picker_open: false,
            },
        }
    }
}
impl FindMarkChoice {
    fn current(&self, stamp: &FindMarkStamp) -> bool {
        self.stamp.kind == stamp.kind
            && self.stamp.picker_open == stamp.picker_open
            && Arc::ptr_eq(&self.stamp.identity, &stamp.identity)
    }
    fn apply(&mut self, action: FindAction) -> bool {
        let (kind, picker_open) = match action {
            FindAction::ToggleMarkKinds => (self.stamp.kind, !self.stamp.picker_open),
            FindAction::SetMarkKind(kind) => (kind, false),
            _ => return false,
        };
        if (kind, picker_open) == (self.stamp.kind, self.stamp.picker_open) {
            return false;
        }
        self.stamp = FindMarkStamp {
            kind,
            picker_open,
            identity: Arc::new(()),
        };
        true
    }
}

#[derive(Clone)]
pub(crate) struct FindPaintTicket {
    token: Arc<()>,
    epoch: Arc<()>,
    paint: Arc<()>,
}
struct FindPending<T> {
    token: Arc<()>,
    payload: T,
    paint: Option<Arc<()>>,
}
pub(crate) struct FindClickIntent<T> {
    epoch: Arc<()>,
    armed: Option<FindPending<T>>,
    queued: Option<FindPending<T>>,
}
impl<T> Default for FindClickIntent<T> {
    fn default() -> Self {
        Self {
            epoch: Arc::new(()),
            armed: None,
            queued: None,
        }
    }
}
impl<T> FindClickIntent<T> {
    pub(crate) fn arm(&mut self, token: Arc<()>, payload: T) {
        self.queued = None;
        self.armed = Some(FindPending {
            token,
            payload,
            paint: None,
        });
    }
    pub(crate) fn payload(&self) -> Option<&T> {
        self.queued
            .as_ref()
            .or(self.armed.as_ref())
            .map(|p| &p.payload)
    }
    pub(crate) fn token(&self) -> Option<&Arc<()>> {
        self.queued
            .as_ref()
            .or(self.armed.as_ref())
            .map(|p| &p.token)
    }
    pub(crate) fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
    pub(crate) fn queue(&mut self) -> bool {
        let Some(armed) = self.armed.take() else {
            return false;
        };
        self.queued = Some(armed);
        true
    }
    pub(crate) fn begin_render(&mut self) -> Arc<()> {
        self.epoch = Arc::new(());
        if let Some(pending) = &mut self.queued {
            pending.paint = None;
        }
        Arc::clone(&self.epoch)
    }
    pub(crate) fn epoch_is_current(&self, epoch: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.epoch, epoch)
    }
    /// 只能由宿主释放后真正合格的 paint 调用；不能把 next-frame 本身当 paint。
    pub(crate) fn painted(&mut self, epoch: &Arc<()>) -> Option<FindPaintTicket> {
        if !self.epoch_is_current(epoch) {
            return None;
        }
        let pending = self.queued.as_mut()?;
        if pending.paint.is_some() {
            return None;
        }
        let paint = Arc::new(());
        pending.paint = Some(Arc::clone(&paint));
        Some(FindPaintTicket {
            token: Arc::clone(&pending.token),
            epoch: Arc::clone(epoch),
            paint,
        })
    }
    /// 先消费匹配意图，旧回调不能删除后继请求；宿主取得后仍重核全部实际门。
    pub(crate) fn take(&mut self, ticket: &FindPaintTicket) -> Option<T> {
        if !self.epoch_is_current(&ticket.epoch) {
            return None;
        }
        let pending = self.queued.as_ref()?;
        if !Arc::ptr_eq(&pending.token, &ticket.token)
            || !pending
                .paint
                .as_ref()
                .is_some_and(|p| Arc::ptr_eq(p, &ticket.paint))
        {
            return None;
        }
        self.queued.take().map(|pending| pending.payload)
    }
    pub(crate) fn cancel(&mut self) {
        self.armed = None;
        self.queued = None;
    }
    pub(crate) fn cancel_token(&mut self, token: &Arc<()>) {
        if self
            .token()
            .is_some_and(|current| Arc::ptr_eq(current, token))
        {
            self.cancel();
        }
    }
    pub(crate) fn release_outside(&mut self, token: &Arc<()>) {
        if self
            .armed
            .as_ref()
            .is_some_and(|p| Arc::ptr_eq(&p.token, token))
        {
            self.armed = None;
        }
    }
}

/// render 开始时必须已有同一真实 Up；旧 Down render 的延迟 paint 不授权。
pub(crate) fn release_current_for_render<T>(
    intent: &FindClickIntent<T>,
    released_at_render: Option<&Arc<()>>,
) -> bool {
    !intent.is_armed()
        && released_at_render
            .zip(intent.token())
            .is_some_and(|(rendered, current)| Arc::ptr_eq(rendered, current))
}

/// GPUI Left 是逻辑主键；异步状态读取对应当前系统映射的物理键高位。
pub(crate) fn primary_button_down() -> bool {
    use windows::Win32::UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON},
        WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON},
    };
    let button = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
        VK_RBUTTON
    } else {
        VK_LBUTTON
    };
    unsafe { GetAsyncKeyState(button.0 as i32) < 0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindAction {
    Open,
    Previous,
    Next,
    Select,
    MarkAll,
    ToggleMarkKinds,
    SetMarkKind(TextMark),
    Close,
}

/// 精确修饰键；输入框其它键留给 TextField（包括 Ctrl+C 和 IME）。
pub(crate) fn shortcut(key: &str, m: gpui::Modifiers, input: bool) -> Option<FindAction> {
    if m.alt || m.platform || m.function {
        return None;
    }
    match (key, m.control, m.shift) {
        ("f", true, false) => Some(FindAction::Open),
        ("f3", false, false) => Some(FindAction::Next),
        ("f3", false, true) => Some(FindAction::Previous),
        ("enter", false, false) if input => Some(FindAction::Next),
        ("enter", false, true) if input => Some(FindAction::Previous),
        ("escape", false, false) => Some(FindAction::Close),
        _ => None,
    }
}

pub(crate) struct FindUi<S> {
    pub(crate) field: Entity<TextField>,
    pub(crate) document: OcrFindDocument<S>,
    pub(crate) model: OcrFind<S>,
    pub(crate) subscriptions: Vec<Subscription>,
    pub(crate) error: String,
    query: String,
    last_clip: Option<crate::annotation::Bounds>,
    valid: bool,
    click_session: Arc<()>,
    click_revision: Arc<()>,
    query_refresh: QueryRefresh,
    mark_choice: FindMarkChoice,
}
fn stamp_matches<S: PartialEq>(
    stamp: &OcrFindStamp<S>,
    source: &S,
    generation: u64,
    revision: u64,
) -> bool {
    stamp.source == *source
        && stamp.result_generation == generation
        && stamp.content_revision == revision
}
fn body_matches(original: &str, text: &str) -> bool {
    original.as_bytes() == text.as_bytes()
}
fn needs_location(
    query_changed: bool,
    last_clip: Option<crate::annotation::Bounds>,
    clip: crate::annotation::Bounds,
) -> bool {
    query_changed || last_clip != Some(clip)
}
impl<S: PartialEq> FindUi<S> {
    /// 新建查找栏并让输入框只在自身内循环焦点；正文超出上限时不安装。
    pub(crate) fn open<V: 'static>(
        stamp: OcrFindStamp<S>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<V>,
    ) -> Result<Self, OcrFindError> {
        let field = cx.new(|cx| {
            TextField::new(
                String::new(),
                tr!("在识别文字中查找", "Find in recognized text"),
                cx,
            )
        });
        let ui = Self::new(stamp, text, field.clone())?;
        field.update(cx, |v, cx| {
            v.configure_focus(vec![field.downgrade()], window, cx)
        });
        Ok(ui)
    }
    pub(crate) fn new(
        stamp: OcrFindStamp<S>,
        text: &str,
        field: Entity<TextField>,
    ) -> Result<Self, OcrFindError> {
        let document = OcrFindDocument::new(stamp, Arc::from(text))?;
        let mut model = OcrFind::default();
        model.synchronize(&document);
        let click_session = Arc::new(());
        let click_revision = Arc::new(());
        let query_refresh =
            QueryRefresh::new(Arc::clone(&click_session), Arc::clone(&click_revision));
        Ok(Self {
            field,
            document,
            model,
            subscriptions: Vec::new(),
            error: String::new(),
            query: String::new(),
            last_clip: None,
            valid: true,
            click_session,
            click_revision,
            query_refresh,
            mark_choice: FindMarkChoice::default(),
        })
    }
    /// 渲染／paint 只读取身份；正文比较仅在输入和显式动作事件 refresh 中执行。
    pub(crate) fn current(&self, source: &S, generation: u64, revision: u64) -> bool {
        self.valid && stamp_matches(self.document.stamp(), source, generation, revision)
    }
    pub(crate) fn focused(&self, window: &Window, cx: &App) -> bool {
        self.field.read(cx).focus_handle.is_focused(window)
    }
    pub(crate) fn composing(&self, cx: &App) -> bool {
        self.field.read(cx).is_composing()
    }
    pub(crate) fn mark_kind(&self) -> TextMark {
        self.mark_choice.stamp.kind
    }
    pub(crate) fn mark_choice_stamp(&self) -> FindMarkStamp {
        self.mark_choice.stamp.clone()
    }
    pub(crate) fn mark_choice_current(&self, stamp: &FindMarkStamp) -> bool {
        self.valid && self.mark_choice.current(stamp)
    }
    /// 宿主在实际 Up 后继 paint 的完整来源门通过后调用；不触发查询扫描。
    pub(crate) fn apply_mark_choice(&mut self, action: FindAction) -> bool {
        self.valid && self.mark_choice.apply(action)
    }
    /// 每次实际字段通知都换代，包含忙碌/IME期间 A→B→A；不扫描正文。
    pub(crate) fn query_event(&mut self) {
        self.click_revision = Arc::new(());
        self.query_refresh.mark(Arc::clone(&self.click_revision));
    }
    pub(crate) fn query_refresh_ticket(&self) -> Option<FindQueryRefreshTicket> {
        self.valid.then(|| self.query_refresh.ticket()).flatten()
    }
    pub(crate) fn query_refresh_current(&self, ticket: &FindQueryRefreshTicket) -> bool {
        self.valid && self.query_refresh.current(ticket)
    }
    /// 仅合格实际 paint 调用；同查询只安排一个尚未完成的缓存刷新回调。
    pub(crate) fn queue_query_refresh(&self, ticket: &FindQueryRefreshTicket) -> bool {
        self.valid && self.query_refresh.queue(ticket)
    }
    /// 回调门暂时不满足时释放预约，后续真正合格 paint 可再次安排；旧票据不能清新预约。
    pub(crate) fn release_query_refresh(&self, ticket: &FindQueryRefreshTicket) {
        self.query_refresh.release(ticket);
    }
    pub(crate) fn button_stamp(
        &self,
        action: FindAction,
        access: OcrFindAccess,
    ) -> Option<FindButtonStamp<S>> {
        if !self.valid {
            return None;
        }
        let summary = self.model.summary(&self.document, access)?;
        let current = self.model.current_ticket(&self.document, access);
        let all = if action == FindAction::MarkAll {
            self.model.all_ticket(&self.document, access)
        } else {
            None
        };
        match action {
            FindAction::Previous | FindAction::Next if summary.stored > 0 => {}
            FindAction::Select if current.is_some() => {}
            FindAction::MarkAll if all.is_some() => {}
            FindAction::ToggleMarkKinds => {}
            FindAction::SetMarkKind(_) if self.mark_choice.stamp.picker_open => {}
            _ => return None,
        }
        Some(FindButtonStamp {
            action,
            session: Arc::clone(&self.click_session),
            revision: Arc::clone(&self.click_revision),
            current,
            all,
            summary,
            mark_choice: self.mark_choice_stamp(),
        })
    }
    pub(crate) fn button_current(
        &self,
        stamp: &FindButtonStamp<S>,
        access: OcrFindAccess,
        cx: &App,
    ) -> bool {
        self.valid
            && Arc::ptr_eq(&stamp.session, &self.click_session)
            && Arc::ptr_eq(&stamp.revision, &self.click_revision)
            && self.mark_choice_current(&stamp.mark_choice)
            && !self.composing(cx)
            && self.field.read(cx).text() == self.query
            && self.model.summary(&self.document, access) == Some(stamp.summary)
            && stamp.current.as_ref().is_none_or(|ticket| {
                self.model
                    .resolve_ticket(ticket, &self.document, access)
                    .is_ok()
            })
            && stamp.all.as_ref().is_none_or(|ticket| {
                self.model
                    .resolve_all_ticket(ticket, &self.document, access)
                    .is_ok()
            })
    }
    pub(crate) fn refresh(
        &mut self,
        selection: &SelectionText,
        clip: crate::annotation::Bounds,
        access: OcrFindAccess,
        cx: &App,
    ) {
        if !access.active || !access.idle || !access.source_current || access.composing {
            return;
        }
        // 仅越过原完整门才消耗；失败正文/超限查询也不在后续逐帧反复扫描。
        self.query_refresh.consume();
        if !body_matches(self.document.text(), &selection.text) {
            // 同 stamp 的异常正文替换也永久撤销旧几何，不因正文改回原字节复活。
            self.valid = false;
            self.model.invalidate();
            self.error = error_text(OcrFindError::StaleIntent).into();
            return;
        }
        let query = self.field.read(cx).text();
        let changed = query != self.query;
        if changed {
            self.query_event();
            self.query_refresh.consume();
            self.query = query.to_owned();
            self.error.clear();
            if let Err(error) = self.model.update(&self.document, query, access) {
                self.error = error_text(error).into();
                return;
            }
        }
        if needs_location(changed, self.last_clip, clip) {
            self.error.clear();
            self.locate(selection, clip, access);
        }
    }
    /// 上一处／下一处：清掉旧错误，移动当前项并在当前裁剪内定位。
    pub(crate) fn cycle(
        &mut self,
        next: bool,
        selection: &SelectionText,
        clip: crate::annotation::Bounds,
        access: OcrFindAccess,
    ) {
        self.error.clear();
        let direction = if next {
            OcrFindDirection::Next
        } else {
            OcrFindDirection::Previous
        };
        self.model.cycle(&self.document, direction, access);
        self.locate(selection, clip, access);
    }
    pub(crate) fn locate(
        &mut self,
        selection: &SelectionText,
        clip: crate::annotation::Bounds,
        access: OcrFindAccess,
    ) {
        self.last_clip = Some(clip);
        if let Some(ticket) = self.model.current_ticket(&self.document, access) {
            if let Err(error) =
                self.model
                    .prepare_location(&ticket, &self.document, selection, clip, access)
            {
                self.error = error_text(error).into();
            }
        }
    }
    pub(crate) fn clear_preview(&mut self) {
        self.query_event();
        self.model.clear_current();
        self.error.clear();
    }
}

pub(crate) fn error_text(error: OcrFindError) -> &'static str {
    match error {
        OcrFindError::TextTooLarge
        | OcrFindError::QueryTooLarge
        | OcrFindError::GeometryTooLarge => tr!("查找超出上限", "Find limit exceeded"),
        OcrFindError::AccessDenied | OcrFindError::StaleIntent => {
            tr!("请结束操作后重试", "Finish the action and retry")
        }
        _ => tr!(
            "这处结果无法在图上定位",
            "This match can't be located in the image"
        ),
    }
}

pub(crate) struct FindBarBindings<'a, T, S> {
    pub(crate) available_height: f32,
    /// 嵌入宿主面板的布局流时不使用绝对定位，宽度跟随容器。
    pub(crate) docked: bool,
    pub(crate) action: fn(&mut T, FindAction, &mut Window, &mut Context<T>),
    pub(crate) key: fn(&mut T, &gpui::KeyDownEvent, &mut Window, &mut Context<T>) -> bool,
    pub(crate) retained: Option<&'a FindButtonStamp<S>>,
    pub(crate) click: fn(&mut T, FindClickPhase, &FindButtonStamp<S>, &mut Window, &mut Context<T>),
}

pub(crate) fn bar<T: 'static, S: Clone + PartialEq + 'static>(
    ui: &FindUi<S>,
    access: OcrFindAccess,
    position: (f32, f32, f32),
    can_mark: bool,
    cx: &Context<T>,
    bindings: FindBarBindings<'_, T, S>,
) -> gpui::AnyElement {
    let FindBarBindings {
        available_height,
        docked,
        action,
        key,
        retained,
        click,
    } = bindings;
    let palette = crate::theme::palette(cx);
    let summary = ui.model.summary(&ui.document, access);
    let count = summary.map_or_else(
        || tr!("暂不可操作", "Temporarily unavailable").to_owned(),
        |s| {
            if s.truncated {
                trf!(
                    "{} / {}（仅前512项）",
                    "{} / {} (first 512)",
                    s.current.unwrap_or(0),
                    s.total
                )
            } else {
                format!("{} / {}", s.current.unwrap_or(0), s.total)
            }
        },
    );
    let found = summary.is_some_and(|s| s.stored > 0);
    let current = summary.is_some_and(|s| s.current.is_some());
    let complete = can_mark && summary.is_some_and(|s| s.total > 0 && !s.truncated);
    let button = |id: &'static str,
                  label: String,
                  command: FindAction,
                  enabled: bool,
                  hint: &'static str| {
        let stamp = enabled
            .then(|| ui.button_stamp(command, access))
            .flatten()
            .or_else(|| retained.filter(|s| s.action == command).cloned());
        let listening = enabled || stamp.is_some();
        let mut button = div()
            .id(id)
            .min_w(px(0.))
            .max_w(px((position.2 - 16.).max(1.)))
            .px_2()
            .py_1()
            .rounded_sm()
            .text_size(px(11.))
            .text_color(rgb(if enabled {
                palette.foreground
            } else {
                palette.muted
            }))
            .overflow_hidden()
            .whitespace_nowrap()
            .tooltip(move |_, cx| cx.new(move |_| crate::pin_view::TextToolHint(hint)).into())
            .child(div().min_w(px(0.)).truncate().child(label));
        if listening {
            button = button
                .cursor_pointer()
                .hover(move |s| s.bg(rgb(palette.hover)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener({
                        let stamp = stamp.clone();
                        move |v, _, w, cx| {
                            if let Some(stamp) = &stamp {
                                click(v, FindClickPhase::Down, stamp, w, cx);
                            }
                            cx.stop_propagation();
                        }
                    }),
                );
            button = if command == FindAction::Close {
                button.on_click(cx.listener(move |v, _, w, cx| action(v, command, w, cx)))
            } else {
                button.on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(move |v, _, w, cx| {
                        if let Some(stamp) = &stamp {
                            click(v, FindClickPhase::Up, stamp, w, cx);
                        }
                        cx.stop_propagation();
                    }),
                )
            };
        }
        button
    };
    let all_label = match ui.mark_kind() {
        TextMark::Highlight => tr!("全部高亮", "Highlight all"),
        TextMark::Underline => tr!("全部下划线", "Underline all"),
        TextMark::Wave => tr!("全部波浪线", "Wave all"),
        TextMark::Strike => tr!("全部删除线", "Strike all"),
    };
    let mut controls = div().flex().flex_wrap().flex_shrink_0().gap(px(3.));
    for (id, label, command, enabled) in [
        (
            "find-prev",
            tr!("上一个", "Prev"),
            FindAction::Previous,
            found,
        ),
        ("find-next", tr!("下一个", "Next"), FindAction::Next, found),
        (
            "find-select",
            tr!("选中", "Select"),
            FindAction::Select,
            current,
        ),
        ("find-mark-all", all_label, FindAction::MarkAll, complete),
        ("find-close", tr!("关闭", "Close"), FindAction::Close, true),
    ] {
        controls = controls.child(button(id, label.to_owned(), command, enabled, label));
    }
    let kind_row = button(
        "find-mark-kind",
        trf!("类型：{} ▾", "Type: {} ▾", ui.mark_kind().label()),
        FindAction::ToggleMarkKinds,
        summary.is_some(),
        ui.mark_kind().label(),
    )
    .w_full()
    .flex_shrink_0();
    let mut kinds = div().flex().flex_col().flex_shrink_0().gap(px(3.));
    if ui.mark_choice.stamp.picker_open {
        let choices = [
            ("find-kind-highlight", TextMark::Highlight),
            ("find-kind-underline", TextMark::Underline),
            ("find-kind-wave", TextMark::Wave),
            ("find-kind-strike", TextMark::Strike),
        ];
        // 较窄贴图切成单列，英文类型名称也不挤在半宽按钮内。
        for row in choices.chunks(if position.2 < 220. { 1 } else { 2 }) {
            let mut options = div().flex().flex_shrink_0().gap(px(3.));
            for &(id, kind) in row {
                let mut option = button(
                    id,
                    kind.label().to_owned(),
                    FindAction::SetMarkKind(kind),
                    summary.is_some(),
                    kind.label(),
                )
                .flex_1()
                .min_w(px(0.));
                if kind == ui.mark_kind() {
                    option = option
                        .bg(rgb(palette.hover))
                        .border_1()
                        .border_color(rgb(palette.border));
                }
                options = options.child(option);
            }
            kinds = kinds.child(options);
        }
    }
    div()
        .id("ocr-find-bar")
        .when(!docked, |bar| {
            bar.absolute()
                .left(px(position.0))
                .top(px(position.1))
                .w(px(position.2))
        })
        .when(docked, |bar| bar.w_full().flex_shrink_0())
        .max_h(px(available_height.max(0.)))
        .flex()
        .flex_col()
        .gap(px(3.))
        .p_2()
        .rounded_md()
        .bg(rgb(palette.background))
        .border_1()
        .border_color(rgb(palette.border))
        .text_color(rgb(palette.foreground))
        .overflow_y_scroll()
        .capture_key_down(cx.listener(move |v, e, w, cx| {
            if key(v, e, w, cx) {
                cx.stop_propagation();
            }
        }))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .id("ocr-find-query")
                .w_full()
                .flex_shrink_0()
                .child(ui.field.clone()),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(rgb(palette.muted))
                .child(count),
        )
        .child(kind_row)
        .child(kinds)
        .child(controls)
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(11.))
                .child(ui.error.clone()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn held_render_cannot_supply_release_identity_to_later_bulk_mark_up() {
        let mut click = FindClickIntent::default();
        let token = Arc::new(());
        click.arm(Arc::clone(&token), FindAction::MarkAll);
        let held_epoch = click.begin_render();
        let released_at_held_render = (!click.is_armed())
            .then(|| click.token().cloned())
            .flatten();
        assert!(released_at_held_render.is_none());
        assert!(click.queue());
        // render 时仍是 Down 的画面，延迟 paint 即使 epoch 尚当前，也无真实 Up 身份。
        assert!(click.epoch_is_current(&held_epoch));
        assert!(!release_current_for_render(
            &click,
            released_at_held_render.as_ref()
        ));
        let successor_epoch = click.begin_render();
        let released_at_render = click.token().cloned().unwrap();
        assert!(!click.is_armed());
        assert!(Arc::ptr_eq(&token, &released_at_render));
        assert!(release_current_for_render(
            &click,
            Some(&released_at_render)
        ));
        let paint = click.painted(&successor_epoch).unwrap();
        assert_eq!(click.take(&paint), Some(FindAction::MarkAll));
        assert!(click.take(&paint).is_none());
    }
    #[test]
    fn bulk_mark_type_and_picker_aba_invalidate_real_release_and_paint_intents() {
        let mut choice = FindMarkChoice::default();
        let old = choice.stamp.clone();
        let mut click = FindClickIntent::default();
        click.arm(Arc::new(()), old.clone());
        let down_render = click.begin_render();
        assert!(click.painted(&down_render).is_none());
        assert!(choice.apply(FindAction::SetMarkKind(TextMark::Underline)));
        assert!(choice.apply(FindAction::SetMarkKind(TextMark::Highlight)));
        assert!(!choice.current(&old));
        assert!(click.queue());
        let up_render = click.begin_render();
        let old_paint = click.painted(&up_render).unwrap();
        let released = click.take(&old_paint).unwrap();
        assert!(!choice.current(&released));
        assert!(click.take(&old_paint).is_none());

        let before_picker = choice.stamp.clone();
        assert!(choice.apply(FindAction::ToggleMarkKinds));
        assert!(choice.apply(FindAction::ToggleMarkKinds));
        assert!(!choice.current(&before_picker));
        let current = choice.stamp.clone();
        assert!(!choice.apply(FindAction::SetMarkKind(TextMark::Highlight)));
        assert!(choice.current(&current));
        assert!(!choice.apply(FindAction::MarkAll));
        click.arm(Arc::new(()), current);
        assert!(click.queue());
        let render = click.begin_render();
        let paint = click.painted(&render).unwrap();
        assert!(choice.current(&click.take(&paint).unwrap()));
        assert!(click.take(&paint).is_none());
    }
    #[test]
    fn bulk_mark_letters_remain_query_text_with_no_hidden_shortcuts() {
        for key in ["h", "u", "w", "d", "H", "U", "W", "D"] {
            for modifiers in [
                gpui::Modifiers::default(),
                gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
                gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ] {
                assert_eq!(shortcut(key, modifiers, true), None);
            }
        }
    }
    #[test]
    fn busy_find_query_retains_pending_and_consumes_after_one_eligible_refresh() {
        let mut pending = QueryRefresh::new(Arc::new(()), Arc::new(()));
        assert!(pending.ticket().is_none());
        pending.mark(Arc::new(()));
        let ticket = pending.ticket().unwrap();
        assert!(pending.current(&ticket));
        assert!(pending.queue(&ticket));
        assert!(!pending.queue(&ticket));
        assert!(pending.ticket().is_none());
        // 回调仍忙碌：仅释放预约，查询仍待刷新，不制造已扫描结果。
        pending.release(&ticket);
        assert!(pending.current(&ticket));
        let retry = pending.ticket().unwrap();
        assert!(pending.queue(&retry));
        pending.consume();
        assert!(!pending.current(&ticket));
        assert!(pending.ticket().is_none());
        assert!(!pending.queue(&retry));
    }
    #[test]
    fn find_query_refresh_aba_and_old_release_cannot_resurrect_or_clear_successor() {
        let session = Arc::new(());
        let mut pending = QueryRefresh::new(Arc::clone(&session), Arc::new(()));
        pending.mark(Arc::new(()));
        let old = pending.ticket().unwrap();
        assert!(pending.queue(&old));
        pending.mark(Arc::new(()));
        pending.mark(Arc::new(()));
        assert!(!pending.current(&old));
        let next = pending.ticket().unwrap();
        assert!(pending.queue(&next));
        pending.release(&old);
        assert!(pending.ticket().is_none());
        assert!(pending.current(&next));
        let mut other = QueryRefresh::new(Arc::new(()), Arc::clone(&next.revision));
        other.mark(Arc::clone(&next.revision));
        assert!(!other.current(&next));
        pending.consume();
        pending.release(&next);
        assert!(pending.ticket().is_none());
    }
    #[test]
    fn held_find_click_survives_render_and_executes_only_after_current_paint_once() {
        let mut click = FindClickIntent::default();
        click.arm(Arc::new(()), FindAction::Next);
        let held = click.begin_render();
        assert!(click.is_armed());
        assert_eq!(click.payload(), Some(&FindAction::Next));
        assert!(click.painted(&held).is_none());
        assert!(click.queue());
        assert!(!click.queue());
        let released = click.begin_render();
        assert!(click.painted(&held).is_none());
        let ticket = click.painted(&released).expect("实际当前 paint 才能授权");
        assert!(click.painted(&released).is_none());
        assert_eq!(click.take(&ticket), Some(FindAction::Next));
        assert!(click.take(&ticket).is_none());
    }
    #[test]
    fn find_click_repaint_and_old_callbacks_cannot_consume_successor_intent() {
        let mut click = FindClickIntent::default();
        let old_token = Arc::new(());
        click.arm(Arc::clone(&old_token), FindAction::Select);
        assert!(click.queue());
        let first = click.begin_render();
        let old_ticket = click.painted(&first).unwrap();
        let repaint = click.begin_render();
        assert!(click.take(&old_ticket).is_none());
        let current_ticket = click.painted(&repaint).unwrap();
        click.arm(Arc::new(()), FindAction::MarkAll);
        click.cancel_token(&old_token);
        click.release_outside(&old_token);
        assert!(click.take(&current_ticket).is_none());
        assert!(click.is_armed());
        assert_eq!(click.payload(), Some(&FindAction::MarkAll));
        assert!(click.queue());
        let epoch = click.begin_render();
        let ticket = click.painted(&epoch).unwrap();
        assert_eq!(click.take(&ticket), Some(FindAction::MarkAll));
    }
    #[test]
    fn find_click_cancellation_and_release_outside_do_not_execute_or_revive() {
        let mut click = FindClickIntent::default();
        let token = Arc::new(());
        click.arm(Arc::clone(&token), FindAction::Previous);
        click.release_outside(&token);
        assert!(!click.queue());
        assert!(click.payload().is_none());
        click.arm(Arc::clone(&token), FindAction::Previous);
        assert!(click.queue());
        click.release_outside(&token);
        assert_eq!(click.payload(), Some(&FindAction::Previous));
        let epoch = click.begin_render();
        let ticket = click.painted(&epoch).unwrap();
        click.cancel_token(&token);
        assert!(click.take(&ticket).is_none());
        assert!(click.payload().is_none());
        assert!(click.painted(&epoch).is_none());
    }
    #[test]
    fn find_shortcuts_are_exact_and_query_copy_is_never_an_output_action() {
        let m = gpui::Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(shortcut("f", m, false), Some(FindAction::Open));
        for key in ["c", "s", "a", "x", "v"] {
            assert_eq!(shortcut(key, m, true), None);
        }
        for changed in [
            gpui::Modifiers { shift: true, ..m },
            gpui::Modifiers { alt: true, ..m },
            gpui::Modifiers {
                platform: true,
                ..m
            },
            gpui::Modifiers {
                function: true,
                ..m
            },
        ] {
            assert_eq!(shortcut("f", changed, false), None);
        }
    }
    #[test]
    fn find_enter_only_belongs_to_query_and_shift_reverses_navigation() {
        assert_eq!(shortcut("enter", Default::default(), false), None);
        assert_eq!(
            shortcut("enter", Default::default(), true),
            Some(FindAction::Next)
        );
        let shift = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(shortcut("enter", shift, true), Some(FindAction::Previous));
        assert_eq!(shortcut("f3", shift, false), Some(FindAction::Previous));
        assert_eq!(shortcut("escape", shift, true), None);
    }
    #[test]
    fn find_source_generation_revision_and_original_bytes_are_all_required() {
        let stamp = OcrFindStamp {
            source: 7,
            result_generation: 3,
            content_revision: 9,
        };
        assert!(stamp_matches(&stamp, &7, 3, 9));
        assert!(!stamp_matches(&stamp, &8, 3, 9));
        assert!(!stamp_matches(&stamp, &7, 4, 9));
        assert!(!stamp_matches(&stamp, &7, 3, 10));
        assert!(body_matches("甲\r\n🙂", "甲\r\n🙂"));
        assert!(!body_matches("甲\r\n🙂", "甲\n🙂"));
    }
    #[test]
    fn actual_result_editor_text_aba_cannot_reuse_old_find_geometry() {
        use crate::capture_view::text_result::ResultEditor;
        let mut editor = ResultEditor::new("甲🙂\n乙".into(), 0..0);
        let original = editor.text.clone();
        let stamp = OcrFindStamp {
            source: 7,
            result_generation: 3,
            content_revision: editor.revision,
        };
        assert!(stamp_matches(&stamp, &7, 3, editor.revision));
        assert!(body_matches(&original, &editor.text));
        editor.replace(Some(0..1), "丙", false, None);
        editor.undo();
        assert_eq!(editor.text.as_bytes(), original.as_bytes());
        assert!(!stamp_matches(&stamp, &7, 3, editor.revision));
    }
    #[test]
    fn unchanged_query_relocates_only_when_event_clip_changes() {
        let clip = crate::annotation::Bounds {
            x: 0.,
            y: 0.,
            width: 100.,
            height: 50.,
        };
        assert!(needs_location(false, None, clip));
        assert!(!needs_location(false, Some(clip), clip));
        assert!(needs_location(true, Some(clip), clip));
        let changed = crate::annotation::Bounds { width: 80., ..clip };
        assert!(needs_location(false, Some(clip), changed));
    }
}
