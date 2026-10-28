//! Win32 注册专项：只使用自有隐藏 HWND 与另一个线程，不发送全局按键。
use crate::{hotkey_registration::HotkeyRegistration, Result};
use std::{
    cell::RefCell,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    core::{w, HRESULT},
    Win32::{
        Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, HWND},
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
};

const WAIT: Duration = Duration::from_secs(3);
const FIRST_KEY: u32 = 0x80; // F17
const LAST_KEY: u32 = 0x87; // F24
const MODIFIERS: HOT_KEY_MODIFIERS =
    HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_ALT.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0);
type NativeReply = std::result::Result<bool, String>;

struct HiddenWindow(HWND, RefCell<Vec<i32>>);
impl HiddenWindow {
    fn new() -> Result<Self> {
        let handle = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("RustShot · 热键注册自测"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )?
        };
        if unsafe { IsWindowVisible(handle) }.as_bool() {
            let _ = unsafe { DestroyWindow(handle) };
            return Err("热键自测窗口应保持隐藏".into());
        }
        Ok(Self(handle, RefCell::new(Vec::new())))
    }
    fn remember(&self, id: usize) {
        let mut ids = self.1.borrow_mut();
        if !ids.contains(&(id as i32)) {
            ids.push(id as i32);
        }
    }
}
impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // 失败路径也回收本窗口的测试 ID；正常 Drop case 在此清理之前验证生产释放行为。
        for &id in self.1.borrow().iter() {
            let _ = unsafe { UnregisterHotKey(Some(self.0), id) };
        }
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

enum Command {
    Acquire(u32),
    Release(u32),
    Stop,
}
struct Request {
    command: Command,
    reply: Sender<NativeReply>,
}

/// 竞争线程的注册不使用 HWND，不能以同线程的第二窗口替代这一占用证据。
struct Competitor {
    requests: Sender<Request>,
    worker: Option<JoinHandle<()>>,
}
impl Competitor {
    fn new() -> Result<Self> {
        let (requests, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("RustShot.hotkey-smoke".to_string())
            .spawn(move || competitor_loop(receiver))?;
        Ok(Self {
            requests,
            worker: Some(worker),
        })
    }
    fn ask(&self, command: Command) -> Result<bool> {
        let (reply, response) = mpsc::channel();
        self.requests.send(Request { command, reply })?;
        response
            .recv_timeout(WAIT)
            .map_err(|_| "热键竞争线程未在限时内响应")?
            .map_err(Into::into)
    }
    fn acquire(&self, key: u32) -> Result<bool> {
        self.ask(Command::Acquire(key))
    }
    fn release(&self, key: u32) -> Result<()> {
        ensure(self.ask(Command::Release(key))?, "竞争线程未持有待释放组合")
    }
    fn shutdown(&mut self) -> Result<()> {
        if self.worker.is_none() {
            return Ok(());
        }
        self.ask(Command::Stop)?;
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| "热键竞争线程异常终止")?;
        }
        Ok(())
    }
}
impl Drop for Competitor {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            // 放弃等待时不无限 join；发件端销毁/线程空闲超时仍会执行注册的 RAII 清理。
            self.worker.take();
        }
    }
}

#[derive(Default)]
struct ThreadRegistrations(Vec<u32>);
impl ThreadRegistrations {
    fn acquire(&mut self, key: u32) -> NativeReply {
        if !(FIRST_KEY..=LAST_KEY).contains(&key) || self.0.contains(&key) {
            return Err("竞争线程收到无效或重复的注册请求".to_string());
        }
        match unsafe { RegisterHotKey(None, thread_id(key), MODIFIERS, key) } {
            Ok(()) => {
                self.0.push(key);
                Ok(true)
            }
            Err(error)
                if error.code() == HRESULT::from_win32(ERROR_HOTKEY_ALREADY_REGISTERED.0) =>
            {
                Ok(false)
            }
            Err(error) => Err(format!("竞争线程注册失败：{error}")),
        }
    }
    fn release(&mut self, key: u32) -> NativeReply {
        let Some(index) = self.0.iter().position(|held| *held == key) else {
            return Ok(false);
        };
        unsafe { UnregisterHotKey(None, thread_id(key)) }
            .map_err(|error| format!("竞争线程释放失败：{error}"))?;
        self.0.swap_remove(index);
        Ok(true)
    }
    fn clear(&mut self) -> NativeReply {
        while let Some(key) = self.0.last().copied() {
            self.release(key)?;
        }
        Ok(true)
    }
}
impl Drop for ThreadRegistrations {
    fn drop(&mut self) {
        for key in self.0.drain(..) {
            let _ = unsafe { UnregisterHotKey(None, thread_id(key)) };
        }
    }
}
fn thread_id(key: u32) -> i32 {
    100 + (key - FIRST_KEY) as i32
}
fn competitor_loop(requests: Receiver<Request>) {
    // 保证 None-HWND 注册所属线程具有消息队列；不合成或发送 WM_HOTKEY。
    let _ = unsafe { PeekMessageW(&mut MSG::default(), None, 0, 0, PM_NOREMOVE) };
    let mut held = ThreadRegistrations::default();
    while let Ok(request) = requests.recv_timeout(WAIT) {
        let (response, stop) = match request.command {
            Command::Acquire(key) => (held.acquire(key), false),
            Command::Release(key) => (held.release(key), false),
            Command::Stop => (held.clear(), true),
        };
        let _ = request.reply.send(response);
        if stop {
            break;
        }
    }
}

fn ensure(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn active_id(registration: &HotkeyRegistration) -> Result<usize> {
    let id = registration.active_id().ok_or("热键没有活跃 ID")?;
    let id = usize::try_from(id).map_err(|_| "热键 ID 不能为负数")?;
    ensure(registration.accepts(id), "活跃 ID 不被注册器接受")?;
    Ok(id)
}
fn event_payload(key: u32, modifiers: HOT_KEY_MODIFIERS) -> usize {
    // WM_HOTKEY 的低字是 Ctrl/Alt/Shift/Win；MOD_NOREPEAT 不是事件修饰位。
    ((key as usize) << 16) | (modifiers.0 as usize & 0xf)
}
fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
fn case(name: &str, run: impl FnOnce() -> Result<()>) -> Result<()> {
    let result = run();
    let detail = match &result {
        Ok(()) => String::new(),
        Err(error) => format!(",\"error\":{}", quoted(&error.to_string())),
    };
    println!(
        "{{\"suite\":\"hotkey-registration\",\"case\":{},\"ok\":{}{detail}}}",
        quoted(name),
        result.is_ok()
    );
    result
}

pub(crate) fn run() -> Result<()> {
    let window = HiddenWindow::new()?;
    let mut competitor = Competitor::new()?;
    let mut available = Vec::with_capacity(2);
    for key in FIRST_KEY..=LAST_KEY {
        if competitor.acquire(key)? {
            available.push(key);
            if available.len() == 2 {
                break;
            }
        }
    }
    ensure(
        available.len() == 2,
        "找不到两个空闲的 Ctrl+Alt+Shift+F17..F24 组合；未覆盖任何已有热键",
    )?;
    let [first, second] = [available[0], available[1]];
    println!(
        "{{\"suite\":\"hotkey-registration\",\"case\":\"isolated-free-combinations\",\"ok\":true,\"keys\":[{},{}]}}",
        first, second
    );
    competitor.release(first)?;
    let initial = Some((MODIFIERS, first));
    let replacement = Some((MODIFIERS, second));
    let mut registration = HotkeyRegistration::new(window.0);
    ensure(registration.set(initial)?, "自有隐藏窗口未取得初始组合")?;
    let first_id = active_id(&registration)?;
    window.remember(first_id);

    case("conflict-retains-old-system-registration", || {
        let error = registration
            .set(replacement)
            .err()
            .ok_or("竞争线程持有新组合时，切换应返回失败")?;
        ensure(
            error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|error| {
                    error.code() == HRESULT::from_win32(ERROR_HOTKEY_ALREADY_REGISTERED.0)
                }),
            "切换失败应来自真实 Win32 热键冲突",
        )?;
        ensure(registration.binding() == initial, "冲突改写了旧组合")?;
        ensure(active_id(&registration)? == first_id, "冲突改写了旧 ID")?;
        ensure(
            !competitor.acquire(first)?,
            "切换失败后旧组合已被系统释放，而非继续可用",
        )
    })?;

    case("identical-set-does-not-replace-or-release", || {
        ensure(!registration.set(initial)?, "重复设置应报告未变化")?;
        ensure(active_id(&registration)? == first_id, "重复设置切换了 ID")?;
        ensure(!competitor.acquire(first)?, "重复设置丢失了系统注册")
    })?;

    case("disable-releases-to-another-thread", || {
        ensure(registration.set(None)?, "禁用应报告变化")?;
        ensure(registration.binding().is_none(), "禁用后仍保留组合")?;
        ensure(
            registration.active_id().is_none()
                && !registration.accepts(first_id)
                && !registration.accepts_event(first_id, event_payload(first, MODIFIERS)),
            "禁用后仍接受排队的热键 ID",
        )?;
        ensure(!registration.set(None)?, "重复禁用应报告未变化")?;
        ensure(competitor.acquire(first)?, "禁用后系统仍占用旧组合")
    })?;

    competitor.release(first)?;
    competitor.release(second)?;
    ensure(registration.set(initial)?, "重启初始组合失败")?;
    let previous_id = active_id(&registration)?;
    window.remember(previous_id);
    ensure(
        previous_id > first_id
            && !registration.accepts(first_id)
            && !registration.accepts_event(first_id, event_payload(first, MODIFIERS)),
        "禁用后恢复复用了旧 ID，旧排队消息可能再次生效",
    )?;
    case(
        "successive-switches-reject-all-old-ids-and-wrong-payload",
        || {
            ensure(registration.set(replacement)?, "切换新组合应报告变化")?;
            let replacement_id = active_id(&registration)?;
            window.remember(replacement_id);
            ensure(registration.binding() == replacement, "切换后组合不正确")?;
            ensure(
                replacement_id > previous_id && !registration.accepts(previous_id),
                "成功切换后仍接受旧 ID",
            )?;
            ensure(competitor.acquire(first)?, "切换后系统未释放旧组合")?;
            ensure(!competitor.acquire(second)?, "切换后系统没有持有新组合")?;

            competitor.release(first)?;
            ensure(registration.set(initial)?, "再次切换组合应报告变化")?;
            let newest_id = active_id(&registration)?;
            window.remember(newest_id);
            ensure(newest_id > replacement_id, "连续切换未使用递增 ID")?;
            for (old_id, old_key) in [
                (first_id, first),
                (previous_id, first),
                (replacement_id, second),
            ] {
                ensure(
                    !registration.accepts(old_id)
                        && !registration.accepts_event(old_id, event_payload(old_key, MODIFIERS)),
                    "两次切换后仍接受已经失效的旧热键 ID",
                )?;
            }
            ensure(
                registration.accepts_event(newest_id, event_payload(first, MODIFIERS)),
                "当前 ID 和正确 WM_HOTKEY 参数应被接受",
            )?;
            ensure(
                !registration.accepts_event(newest_id, event_payload(second, MODIFIERS))
                    && !registration
                        .accepts_event(newest_id, event_payload(first, MOD_ALT | MOD_SHIFT)),
                "相同 ID 但主键或修饰位不匹配的消息仍被接受",
            )?;
            ensure(competitor.acquire(second)?, "第二次切换未释放旧组合")?;
            ensure(!competitor.acquire(first)?, "第二次切换后系统未保留新组合")
        },
    )?;

    drop(registration);
    case("drop-releases-to-another-thread", || {
        ensure(competitor.acquire(first)?, "Drop 后系统仍占用新组合")
    })?;
    competitor.shutdown()?;
    drop(window);
    Ok(())
}
