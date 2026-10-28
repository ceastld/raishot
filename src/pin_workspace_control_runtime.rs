//! 单张控制属性通道；只由持有租约的窗口在外层 tick 应用，无同步 Win32 状态回调。
use super::{control_model as model, hwnd, id_hash, pin_native, store, PinView, Result};
use gpui::{Context, Window};
use model::{Identity, InteractionAction, LiveControl, Outcome, Request};
use std::{collections::HashMap, time::Instant};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, WPARAM},
        System::SystemInformation::GetTickCount64,
        UI::WindowsAndMessaging::*,
    },
};

const OWNER: PCWSTR = w!("RustShot.Pin.ControlOwner");
const STATE: PCWSTR = w!("RustShot.Pin.ControlState");
const STATE_TIME: PCWSTR = w!("RustShot.Pin.ControlTime");
const REQUEST_TICKET: PCWSTR = w!("RustShot.Pin.ControlRequest");
const REQUEST_GENERATION: PCWSTR = w!("RustShot.Pin.ControlGeneration");
const REQUEST_SCOPE: PCWSTR = w!("RustShot.Pin.ControlScope");
const REQUEST_RECORD: PCWSTR = w!("RustShot.Pin.ControlRecord");
const REQUEST_PID: PCWSTR = w!("RustShot.Pin.ControlPid");
const REQUEST_THREAD: PCWSTR = w!("RustShot.Pin.ControlThread");
const REQUEST_ACTION: PCWSTR = w!("RustShot.Pin.ControlAction");
const REQUEST_DEADLINE: PCWSTR = w!("RustShot.Pin.ControlDeadline");
const ACK_TICKET: PCWSTR = w!("RustShot.Pin.ControlAck");
const ACK_RESULT: PCWSTR = w!("RustShot.Pin.ControlResult");
const READY: usize = 1;
const LOCKED: usize = 2;
const THROUGH: usize = 4;
const BUSY: usize = 8;
const CLOSING: usize = 16;
const FRESH_MS: u64 = 1_500;

fn now_ms() -> u64 {
    // 同一 Windows 启动内跨进程共用单调时钟；墙钟调整不能延长请求期限。
    unsafe { GetTickCount64() }
}
fn value(h: HWND, key: PCWSTR) -> usize {
    unsafe { GetPropW(h, key).0 as usize }
}
fn set(h: HWND, key: PCWSTR, value: u64) -> Result<()> {
    let value = usize::try_from(value).map_err(|_| "贴图控制属性超过平台范围")?;
    if value == 0 {
        if self::value(h, key) != 0 {
            unsafe {
                RemovePropW(h, key)?;
            }
        }
    } else {
        unsafe {
            SetPropW(h, key, Some(HANDLE(value as *mut _)))?;
        }
    }
    Ok(())
}
fn identity(h: HWND, key: u64) -> Option<Identity> {
    if !unsafe { IsWindow(Some(h)) }.as_bool()
        || value(h, w!("RustShot.Pin.v1")) != h.0 as usize
        || value(h, w!("RustShot.Pin.WorkspaceScope")) as u64 != store::scope()
        || value(h, w!("RustShot.Pin.WorkspaceId")) as u64 != key
    {
        return None;
    }
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    let generation = value(h, OWNER) as u64;
    (generation != 0 && pid != 0 && thread != 0).then_some(Identity {
        scope: store::scope(),
        record_key: key,
        generation,
        hwnd: h.0 as usize,
        pid,
        thread,
    })
}
fn native(identity: &Identity) -> HWND {
    HWND(identity.hwnd as *mut _)
}
fn observed(h: HWND, key: u64) -> Option<LiveControl> {
    let identity = identity(h, key)?;
    let before = value(h, STATE_TIME) as u64;
    let flags = value(h, STATE);
    let after = value(h, STATE_TIME) as u64;
    let now = now_ms();
    if before == 0
        || before != after
        || flags & READY == 0
        || before > now
        || now - before > FRESH_MS
        || self::identity(h, key) != Some(identity)
    {
        return None;
    }
    Some(LiveControl {
        identity,
        locked: flags & LOCKED != 0,
        click_through: flags & THROUGH != 0,
        busy: flags & BUSY != 0,
        closing: flags & CLOSING != 0,
    })
}

/// 单记录只认唯一窗口、活动清单和仍占用的独占租约；旧进程未发布状态时显示未知。
#[cfg(test)]
pub(crate) fn active_controls(workspace: &store::Workspace) -> HashMap<String, LiveControl> {
    active_controls_for(
        workspace
            .pins
            .iter()
            .map(|pin| (pin.id.as_str(), pin.status)),
        super::windows_in(workspace),
    )
}
pub(crate) fn active_controls_summary(
    workspace: &store::WorkspaceSummary,
) -> HashMap<String, LiveControl> {
    active_controls_for(
        workspace
            .pins
            .iter()
            .map(|pin| (pin.id.as_str(), pin.status)),
        super::windows_summary(workspace),
    )
}
fn active_controls_for<'a>(
    entries: impl Iterator<Item = (&'a str, store::PinStatus)>,
    found: Vec<(String, HWND)>,
) -> HashMap<String, LiveControl> {
    let mut windows = HashMap::<String, Vec<HWND>>::new();
    for (id, h) in found {
        windows.entry(id).or_default().push(h);
    }
    entries
        .filter_map(|(id, status)| {
            if status != store::PinStatus::Active || !store::lease_live(id) {
                return None;
            }
            let targets = windows.get(id)?;
            if targets.len() != 1 {
                return None;
            }
            observed(targets[0], id_hash(id)).map(|live| (id.to_owned(), live))
        })
        .collect()
}

pub(crate) fn request_control(
    id: &str,
    target: &LiveControl,
    action: InteractionAction,
) -> Result<Request> {
    if target.identity.record_key != id_hash(id) || target.busy || target.closing {
        return Err(tr!(
            "贴图正在忙碌或关闭，请稍后重试",
            "The pin is busy or closing; try again later"
        )
        .into());
    }
    store::with_control_ticket(id, |ticket| {
        let h = native(&target.identity);
        let current = observed(h, id_hash(id)).ok_or(tr!(
            "贴图窗口状态已失效，请刷新",
            "The pin state is unavailable; refresh first"
        ))?;
        if current.identity != target.identity || current.busy || current.closing {
            return Err(tr!(
                "贴图窗口已变化或忙碌，请刷新后重试",
                "The pin changed or is busy; refresh and retry"
            )
            .into());
        }
        let now = now_ms();
        let old_deadline = value(h, REQUEST_DEADLINE) as u64;
        if model::slot_reserved(value(h, REQUEST_TICKET) as u64, old_deadline, now) {
            return Err(tr!(
                "这张贴图已有待处理请求",
                "This pin already has a pending request"
            )
            .into());
        }
        let request = Request {
            identity: current.identity,
            ticket,
            action,
            deadline_ms: now
                .checked_add(model::MAX_REQUEST_MS)
                .ok_or("贴图控制期限溢出")?,
        };
        // 原工作区 try-lock 串行多属性写入；票据最后发布，接收端锁内复制。
        set(h, REQUEST_TICKET, 0)?;
        set(h, REQUEST_GENERATION, request.identity.generation)?;
        set(h, REQUEST_SCOPE, request.identity.scope)?;
        set(h, REQUEST_RECORD, request.identity.record_key)?;
        set(h, REQUEST_PID, u64::from(request.identity.pid))?;
        set(h, REQUEST_THREAD, u64::from(request.identity.thread))?;
        set(h, REQUEST_ACTION, action as u64)?;
        set(h, REQUEST_DEADLINE, request.deadline_ms)?;
        set(h, REQUEST_TICKET, ticket)?;
        if let Err(error) = unsafe { PostMessageW(Some(h), WM_NULL, WPARAM(0), LPARAM(0)) } {
            let _ = set(h, REQUEST_TICKET, 0);
            return Err(error.into());
        }
        Ok(request)
    })
}

/// 锁忙是暂时不可读，调用方保留同一请求到硬截止；绝不重发或改写目标状态。
pub(crate) fn control_result(request: &Request) -> Result<Option<Outcome>> {
    store::with_control_lock(|| {
        let h = native(&request.identity);
        if identity(h, request.identity.record_key) != Some(request.identity) {
            return Ok(Some(Outcome::Stale));
        }
        if value(h, ACK_TICKET) as u64 == request.ticket {
            let outcome = model::parse_outcome(value(h, ACK_RESULT)).unwrap_or(Outcome::Failed);
            // 已完成但未读的回执仍占原槽；读取后才让第二个管理器排队。
            // 过期旧请求即使迟读，也不能清掉已替换为另一票据的新请求。
            if model::ack_owns_slot(
                value(h, REQUEST_TICKET) as u64,
                value(h, ACK_TICKET) as u64,
                request.ticket,
            ) {
                set(h, REQUEST_TICKET, 0)?;
            }
            set(h, ACK_TICKET, 0)?;
            set(h, ACK_RESULT, 0)?;
            return Ok(Some(outcome));
        }
        let now = now_ms();
        if now >= request.deadline_ms || request.deadline_ms - now > model::MAX_REQUEST_MS {
            return Ok(Some(Outcome::Expired));
        }
        Ok(None)
    })
}

struct RestoreThrough {
    started: Instant,
    attempts: u8,
}
pub(super) struct OwnerControls {
    last_ticket: u64,
    pending_ack: Option<(Request, Outcome)>,
    restore_through: Option<RestoreThrough>,
}
impl OwnerControls {
    pub(super) fn new(restore_through: bool) -> Self {
        Self {
            last_ticket: 0,
            pending_ack: None,
            restore_through: restore_through.then(|| RestoreThrough {
                started: Instant::now(),
                attempts: 0,
            }),
        }
    }
}
fn read_request(live: &LiveControl) -> Result<Option<Request>> {
    store::with_control_lock(|| {
        let h = native(&live.identity);
        let ticket = value(h, REQUEST_TICKET) as u64;
        if ticket == 0
            || value(h, ACK_TICKET) as u64 == ticket
            || identity(h, live.identity.record_key) != Some(live.identity)
        {
            return Ok(None);
        }
        // 不完整字段无法执行；复制时保持请求票据直到最终 ACK，禁止并发管理器覆盖。
        let action = model::parse_action(value(h, REQUEST_ACTION)).ok_or("贴图控制动作无效")?;
        Ok(Some(Request {
            identity: Identity {
                scope: value(h, REQUEST_SCOPE) as u64,
                record_key: value(h, REQUEST_RECORD) as u64,
                generation: value(h, REQUEST_GENERATION) as u64,
                hwnd: live.identity.hwnd,
                pid: u32::try_from(value(h, REQUEST_PID)).unwrap_or(0),
                thread: u32::try_from(value(h, REQUEST_THREAD)).unwrap_or(0),
            },
            ticket,
            action,
            deadline_ms: value(h, REQUEST_DEADLINE) as u64,
        }))
    })
}
fn acknowledge(request: &Request, outcome: Outcome, actual: &Identity) -> Result<()> {
    store::with_control_lock(|| {
        let h = native(actual);
        if identity(h, actual.record_key) != Some(*actual)
            || value(h, REQUEST_TICKET) as u64 != request.ticket
        {
            return Ok(());
        }
        set(h, ACK_RESULT, outcome as u64)?;
        // 保留请求槽，直到原管理器读走 ACK 或 5 秒期限允许替换。
        set(h, ACK_TICKET, request.ticket)
    })
}

impl PinView {
    pub(super) fn workspace_interaction_click_through(&self, window: &Window) -> bool {
        self.workspace.as_ref().map_or_else(
            || pin_native::transparent(hwnd(window)),
            |live| {
                // 首次 Present 尚未完成时不能打开 WS_EX_TRANSPARENT；若此时
                // 关闭，保留待恢复设置，不能将临时原生 false 写回偏好。
                if live.controls.restore_through.is_some() {
                    live.entry.click_through
                } else {
                    pin_native::transparent(hwnd(window))
                }
            },
        )
    }
    fn control_live(&self, window: &Window, cx: &gpui::App) -> Option<LiveControl> {
        let owner = self.workspace.as_ref()?;
        let h = hwnd(window);
        let identity = identity(h, id_hash(&owner.session.id))?;
        if identity.generation != owner.session.owner_generation
            || identity.pid != std::process::id()
        {
            return None;
        }
        let queued = value(h, REQUEST_TICKET) as u64;
        let unread_ack = queued != 0
            && value(h, ACK_TICKET) as u64 == queued
            && model::slot_reserved(queued, value(h, REQUEST_DEADLINE) as u64, now_ms());
        Some(LiveControl {
            identity,
            locked: self.state.locked,
            click_through: pin_native::transparent(h),
            closing: owner.pending_close.is_some() || owner.close_finished,
            busy: owner.busy
                || owner.controls.restore_through.is_some()
                || owner.controls.pending_ack.is_some()
                || unread_ack
                || self.saving
                || self.processing
                || self.workspace_control_input_busy(cx)
                || value(h, w!("RustShot.Pin.Starting")) != 0,
        })
    }
    fn publish_controls(&self, window: &Window, cx: &gpui::App) -> Result<()> {
        let live = self
            .control_live(window, cx)
            .ok_or("贴图控制窗口身份不可用")?;
        let flags = READY
            | if live.locked { LOCKED } else { 0 }
            | if live.click_through { THROUGH } else { 0 }
            | if live.busy { BUSY } else { 0 }
            | if live.closing { CLOSING } else { 0 };
        let h = native(&live.identity);
        set(h, STATE_TIME, 0)?;
        set(h, STATE, flags as u64)?;
        set(h, STATE_TIME, now_ms().max(1))
    }
    pub(super) fn workspace_controls_attach(&self, window: &Window, cx: &gpui::App) -> Result<()> {
        let live = self.workspace.as_ref().ok_or("贴图控制缺少工作区租约")?;
        set(hwnd(window), OWNER, live.session.owner_generation)?;
        self.publish_controls(window, cx)
    }
    fn restore_click_through(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let h = hwnd(window);
        let input_busy = self.saving
            || self.processing
            || self.workspace_control_input_busy(cx)
            || self.workspace.as_ref().is_some_and(|live| live.busy);
        let Some(live) = &mut self.workspace else {
            return false;
        };
        let Some(restore) = &mut live.controls.restore_through else {
            return false;
        };
        if restore.started.elapsed().as_millis() < u128::from(model::MAX_REQUEST_MS)
            && (value(h, w!("RustShot.Pin.Starting")) != 0 || input_busy)
        {
            return true;
        }
        if live.pending_close.is_some() {
            return true;
        }
        if restore.started.elapsed().as_millis() >= u128::from(model::MAX_REQUEST_MS) {
            live.controls.restore_through = None;
            self.status = tr!(
                "恢复穿透超时；贴图保留可交互状态",
                "Click-through restore timed out; the pin stays interactive"
            )
            .into();
            cx.notify();
            return false;
        }
        restore.attempts += 1;
        match pin_native::click_through(h, true).and_then(|()| {
            if pin_native::transparent(h) {
                Ok(())
            } else {
                Err("穿透样式未生效".into())
            }
        }) {
            Ok(()) => {
                live.controls.restore_through = None;
                false
            }
            Err(error) => {
                let _ = pin_native::click_through(h, false);
                if restore.attempts >= 3 {
                    live.controls.restore_through = None;
                    self.status = trf!(
                        "恢复穿透失败：{error}",
                        "Click-through restore failed: {error}"
                    );
                    cx.notify();
                    false
                } else {
                    true
                }
            }
        }
    }
    fn apply_control(
        &mut self,
        request: &Request,
        actual: &LiveControl,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let (locked, through) = model::desired(request.action, actual.locked, actual.click_through);
        let live = self.workspace.as_ref().ok_or("贴图控制租约缺失")?;
        live.session.persist_interaction(locked, through)?;
        // 持久化可能受慢磁盘影响；超时不再向原生窗口应用迟到的交互变化。
        if now_ms() >= request.deadline_ms {
            live.session
                .persist_interaction(actual.locked, actual.click_through)?;
            return Err("贴图控制保存后已超时，交互状态未应用".into());
        }
        if matches!(
            request.action,
            InteractionAction::ClearClickThrough | InteractionAction::EnableClickThrough
        ) {
            let h = hwnd(window);
            if let Err(error) = pin_native::click_through(h, through).and_then(|()| {
                if pin_native::transparent(h) == through {
                    Ok(())
                } else {
                    Err("穿透样式未生效".into())
                }
            }) {
                // 部分 Win32 更新失败时恢复此前样式/持久值；失败仍如实回执。
                let native_rollback = pin_native::click_through(h, actual.click_through);
                let saved_rollback = live
                    .session
                    .persist_interaction(actual.locked, actual.click_through);
                let rollback_errors: Vec<String> = [
                    native_rollback.err().map(|e| e.to_string()),
                    saved_rollback.err().map(|e| e.to_string()),
                ]
                .into_iter()
                .flatten()
                .collect();
                let message = if rollback_errors.is_empty() {
                    trf!(
                        "切换穿透失败：{error}",
                        "Couldn't change click-through: {error}"
                    )
                } else {
                    let rollback = rollback_errors.join("；");
                    trf!(
                        "切换穿透失败：{error}（恢复原状态也失败：{rollback}）",
                        "Couldn't change click-through: {error} (restoring the previous state also failed: {rollback})"
                    )
                };
                return Err(message.into());
            }
        }
        self.state.locked = locked;
        if request.action == InteractionAction::Locate {
            // 已在置顶层；再提到置顶层最前，不抢前台焦点。
            unsafe {
                let _ = SetWindowPos(
                    hwnd(window),
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            self.flash_operation_hint(tr!("在这里", "Here").into(), cx);
        }
        if matches!(
            request.action,
            InteractionAction::Unlock | InteractionAction::Lock
        ) {
            self.reset_zoom_wheel();
            let editing = self.annotation_preview.allows_editing(self.chrome, locked);
            if let Some(canvas) = &self.canvas {
                // 只改事件门；自然点击再获得焦点，管理器解除不能抢前台。
                canvas.update(cx, |view, cx| {
                    view.pin_editing = editing;
                    cx.notify();
                });
            }
        }
        if let Some(live) = &mut self.workspace {
            live.entry.state.locked = locked;
            live.entry.click_through = through;
            live.state = None;
        }
        cx.notify();
        Ok(())
    }
    pub(super) fn workspace_controls_tick(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let restoring = self.restore_click_through(window, cx);
        let Some(actual) = self.control_live(window, cx) else {
            return restoring;
        };
        let pending = self
            .workspace
            .as_ref()
            .and_then(|live| live.controls.pending_ack);
        if let Some((request, outcome)) = pending {
            let done = acknowledge(&request, outcome, &actual.identity).is_ok()
                || now_ms() >= request.deadline_ms;
            if done {
                if let Some(live) = &mut self.workspace {
                    live.controls.pending_ack = None;
                }
            }
            let _ = self.publish_controls(window, cx);
            return restoring;
        }
        if let Ok(Some(request)) = read_request(&actual) {
            let last = self
                .workspace
                .as_ref()
                .map_or(0, |live| live.controls.last_ticket);
            let outcome = match model::authorize(&request, &actual, now_ms(), last) {
                Err(outcome) => outcome,
                Ok(()) => match self.apply_control(&request, &actual, window, cx) {
                    Ok(()) => Outcome::Success,
                    Err(error) => {
                        // 部分保存／回退失败后，下一稳定 tick 按实际状态补存；
                        // 不能让旧缓存资格掩盖磁盘与窗口的不一致。
                        if let Some(live) = &mut self.workspace {
                            live.state = None;
                        }
                        self.status = trf!("贴图控制失败：{error}", "Pin control failed: {error}");
                        cx.notify();
                        Outcome::Failed
                    }
                },
            };
            if let Some(live) = &mut self.workspace {
                live.controls.last_ticket = live.controls.last_ticket.max(request.ticket);
                live.controls.pending_ack = Some((request, outcome));
            }
            if acknowledge(&request, outcome, &actual.identity).is_ok() {
                if let Some(live) = &mut self.workspace {
                    live.controls.pending_ack = None;
                }
            }
        }
        let _ = self.publish_controls(window, cx);
        restoring
    }
}
