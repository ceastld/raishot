//! 录屏工具栏自有 HWND 的输入所有权观察器。
//! 必须在新窗口开始接收输入前安装；只观察原生信号，不借用 GPUI 视图。
//! IME 空组合仍由 START/COMPOSITION 到 END 的区间拥有键盘，不能据空串放行。
use crate::Result;
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{GetActiveWindow, GetFocus, VK_PROCESSKEY},
        Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetWindowThreadProcessId, IsWindow, WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION,
            WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_NCDESTROY, WM_SYSKEYDOWN,
            WM_SYSKEYUP,
        },
    },
};

static NEXT_SUBCLASS: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    Start,
    Composition,
    End,
    Destroy,
    ProcessKey,
    Other,
}

fn decode_signal(message: u32, key: usize) -> Signal {
    match message {
        WM_IME_STARTCOMPOSITION => Signal::Start,
        WM_IME_COMPOSITION => Signal::Composition,
        WM_IME_ENDCOMPOSITION => Signal::End,
        WM_NCDESTROY => Signal::Destroy,
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP if key as u16 == VK_PROCESSKEY.0 => {
            Signal::ProcessKey
        }
        _ => Signal::Other,
    }
}

fn dispatch_owns_ime(message: u32, signal: Signal, composing_at_entry: bool) -> bool {
    // END 清除持续组合，但其默认链中的嵌套键仍属于此次输入法交接。
    // 普通键进入时已有组合也要保持所有权，不能被同步 END 提前放行。
    matches!(
        signal,
        Signal::Start | Signal::Composition | Signal::End | Signal::ProcessKey
    ) || (composing_at_entry
        && matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP))
}

struct Signals {
    alive: AtomicBool,
    composition: AtomicBool,
    dispatch_owner: AtomicBool,
}

impl Signals {
    fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
            composition: AtomicBool::new(false),
            dispatch_owner: AtomicBool::new(false),
        }
    }

    fn observe(&self, signal: Signal) {
        match signal {
            // COMPOSITION 可能带空串、结果串或取消信息，均等待 END 明确结束。
            Signal::Start | Signal::Composition => {
                self.composition.store(true, Ordering::Release);
            }
            Signal::End => self.composition.store(false, Ordering::Release),
            Signal::Destroy => self.deactivate(),
            Signal::ProcessKey | Signal::Other => {}
        }
    }

    fn deactivate(&self) {
        self.alive.store(false, Ordering::Release);
    }

    fn composing(&self) -> bool {
        self.composition.load(Ordering::Acquire) || self.dispatch_owner.load(Ordering::Acquire)
    }

    fn begin_dispatch(&self, message: u32, key: usize) -> (Signal, Option<ImeDispatchGuard<'_>>) {
        let signal = decode_signal(message, key);
        let keep_owner = dispatch_owns_ime(message, signal, self.composing());
        self.observe(signal);
        let guard = keep_owner.then(|| self.enter_ime_dispatch());
        (signal, guard)
    }

    fn enter_ime_dispatch(&self) -> ImeDispatchGuard<'_> {
        ImeDispatchGuard {
            signals: self,
            previous: self.dispatch_owner.swap(true, Ordering::AcqRel),
        }
    }
}

/// 原生默认链可同步嵌套消息；内层 END 或返回不能清除外层的输入法负门。
/// 所有调用在绑定的 UI 线程按栈顺序进入/退出，无跨线程视图回调。
struct ImeDispatchGuard<'a> {
    signals: &'a Signals,
    previous: bool,
}

impl Drop for ImeDispatchGuard<'_> {
    fn drop(&mut self) {
        self.signals
            .dispatch_owner
            .store(self.previous, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
struct KeyboardFacts {
    alive: bool,
    bound: bool,
    focus: bool,
    active: bool,
}

fn keyboard_owned(facts: KeyboardFacts) -> bool {
    facts.alive && facts.bound && facts.focus && facts.active
}

struct BindingData {
    signals: Signals,
    released: AtomicBool,
}

impl BindingData {
    fn new() -> Self {
        Self {
            signals: Signals::new(),
            released: AtomicBool::new(false),
        }
    }

    fn claim_release(&self) -> bool {
        !self.released.swap(true, Ordering::AcqRel)
    }
}

/// 自有工具栏的原生输入资格；根视图仍须检查精确 GPUI 焦点和自身 busy 状态。
/// 非 Send/Sync：安装、移除和消息转发始终属于 HWND 的 UI 线程。
pub(super) struct KeyOwnerProbe {
    hwnd: usize,
    id: usize,
    data: usize,
    token: Arc<BindingData>,
    _ui_thread: PhantomData<Rc<()>>,
}

impl KeyOwnerProbe {
    pub(super) fn install(hwnd: HWND) -> Result<Self> {
        let mut process = 0;
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool()
            || unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) }
                != unsafe { GetCurrentThreadId() }
            || process != unsafe { GetCurrentProcessId() }
        {
            return Err("录屏输入观察器只能绑定当前线程的自有窗口".into());
        }
        let id = NEXT_SUBCLASS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| "无法分配录屏输入观察器标识")?;
        let token = Arc::new(BindingData::new());
        let data = Box::into_raw(Box::new(token.clone())) as usize;
        if !unsafe { SetWindowSubclass(hwnd, Some(input_proc), id, data) }.as_bool() {
            token.signals.deactivate();
            unsafe { release_data(data, &token) };
            return Err("无法绑定录屏输入观察器".into());
        }
        Ok(Self {
            hwnd: hwnd.0 as usize,
            id,
            data,
            token,
            _ui_thread: PhantomData,
        })
    }

    pub(super) fn owns_keyboard(&self) -> bool {
        let alive = self.token.signals.alive.load(Ordering::Acquire)
            && !self.token.released.load(Ordering::Acquire);
        if !alive {
            return false;
        }
        let hwnd = HWND(self.hwnd as *mut _);
        keyboard_owned(KeyboardFacts {
            alive,
            bound: unsafe { owns_binding(hwnd, self.id, self.data) },
            focus: unsafe { GetFocus() } == hwnd,
            active: unsafe { GetActiveWindow() } == hwnd,
        })
    }

    pub(super) fn composing(&self) -> bool {
        self.token.signals.composing()
    }
}

unsafe fn owns_binding(hwnd: HWND, id: usize, data: usize) -> bool {
    let mut actual = 0;
    unsafe { GetWindowSubclass(hwnd, Some(input_proc), id, Some(&mut actual)) }.as_bool()
        && actual == data
}

unsafe fn release_data(data: usize, token: &BindingData) {
    if token.claim_release() {
        unsafe { drop(Box::from_raw(data as *mut Arc<BindingData>)) };
    }
}

impl Drop for KeyOwnerProbe {
    fn drop(&mut self) {
        self.token.signals.deactivate();
        if self.token.released.load(Ordering::Acquire) {
            return;
        }
        let hwnd = HWND(self.hwnd as *mut _);
        unsafe {
            if !owns_binding(hwnd, self.id, self.data)
                || RemoveWindowSubclass(hwnd, Some(input_proc), self.id).as_bool()
            {
                release_data(self.data, &self.token);
            }
        }
        // 若原生移除失败且仍绑定，不能释放默认链仍可访问的 Box；
        // 窗口之后的 NCDESTROY 接管一次释放，alive=false 已拒绝快捷键。
    }
}

unsafe extern "system" fn input_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // 默认链可能同步销毁 HWND/视图并释放 Box，先克隆独立 token。
    let token = unsafe { &*(data as *const Arc<BindingData>) }.clone();
    let (signal, _ime_dispatch) = token.signals.begin_dispatch(message, wparam.0);
    if signal == Signal::Destroy {
        let was_owned = unsafe { owns_binding(hwnd, id, data) };
        if was_owned {
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(input_proc), id) };
        }
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        if was_owned {
            unsafe { release_data(data, &token) };
        }
        return result;
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_preview_key_owner_empty_start_stays_owned_until_end() {
        let signals = Signals::new();
        signals.observe(decode_signal(WM_IME_STARTCOMPOSITION, 0));
        assert!(signals.composing());
        // 空 COMPOSITION、候选变化和普通键均不是组合结束证据。
        for signal in [Signal::Composition, Signal::Other, Signal::Composition] {
            signals.observe(signal);
            assert!(signals.composing());
        }
        signals.observe(decode_signal(WM_IME_ENDCOMPOSITION, 0));
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_composition_without_start_is_conservative() {
        let signals = Signals::new();
        signals.observe(Signal::Composition);
        assert!(signals.composing());
        signals.observe(Signal::End);
        assert!(!signals.composing());
        signals.observe(Signal::Start);
        assert!(signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_process_key_covers_default_chain_only() {
        let signals = Signals::new();
        assert!(!signals.composing());
        {
            let _default_chain = signals.enter_ime_dispatch();
            assert!(signals.composing());
        }
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_nested_process_key_keeps_outer_owner() {
        let signals = Signals::new();
        {
            let _outer_default_chain = signals.enter_ime_dispatch();
            {
                let _inner_default_chain = signals.enter_ime_dispatch();
                assert!(signals.composing());
            }
            assert!(signals.composing());
        }
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_end_inside_process_key_cannot_release_edge() {
        let signals = Signals::new();
        signals.observe(Signal::Start);
        {
            let _default_chain = signals.enter_ime_dispatch();
            signals.observe(Signal::End);
            assert!(signals.composing());
        }
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_start_inside_process_key_survives_return() {
        let signals = Signals::new();
        {
            let _default_chain = signals.enter_ime_dispatch();
            signals.observe(Signal::Start);
        }
        assert!(signals.composing());
        signals.observe(Signal::End);
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_plain_enter_keeps_entry_owner_after_nested_end() {
        let signals = Signals::new();
        signals.observe(Signal::Start);
        {
            let (_, _ordinary_enter) = signals.begin_dispatch(WM_KEYDOWN, 0x0d);
            {
                let (_, _ime_end) = signals.begin_dispatch(WM_IME_ENDCOMPOSITION, 0);
                assert!(signals.composing());
            }
            // GPUI 在默认链稍后派发原来的普通 Enter 时，仍须交输入法。
            assert!(signals.composing());
        }
        assert!(!signals.composing());
        let (_, _next_enter) = signals.begin_dispatch(WM_KEYDOWN, 0x0d);
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_plain_and_system_edges_keep_entry_owner() {
        for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
            for key in [0x0d, b'C' as usize, b'S' as usize, 0x1b] {
                let signals = Signals::new();
                signals.observe(Signal::Composition);
                {
                    let (_, _key_chain) = signals.begin_dispatch(message, key);
                    {
                        let (_, _ime_end) = signals.begin_dispatch(WM_IME_ENDCOMPOSITION, 0);
                    }
                    assert!(signals.composing(), "消息 {message:#x} / 键 {key:#x}");
                }
                assert!(!signals.composing());
            }
        }
    }

    #[test]
    fn recording_preview_key_owner_end_default_chain_denies_nested_plain_keys() {
        let signals = Signals::new();
        signals.observe(Signal::Start);
        {
            let (_, _ime_end) = signals.begin_dispatch(WM_IME_ENDCOMPOSITION, 0);
            // END 的持续状态已清除，作用域所有权仍覆盖同步嵌套的原生键。
            assert!(!signals.composition.load(Ordering::Acquire));
            for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
                {
                    let (_, _nested_key) = signals.begin_dispatch(message, 0x0d);
                    assert!(signals.composing());
                }
                assert!(signals.composing());
            }
        }
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_each_ime_default_chain_holds_nested_end_and_key() {
        for message in [
            WM_IME_STARTCOMPOSITION,
            WM_IME_COMPOSITION,
            WM_IME_ENDCOMPOSITION,
        ] {
            let signals = Signals::new();
            {
                let (_, _ime_chain) = signals.begin_dispatch(message, 0);
                {
                    let (_, _nested_end) = signals.begin_dispatch(WM_IME_ENDCOMPOSITION, 0);
                }
                {
                    let (_, _nested_enter) = signals.begin_dispatch(WM_KEYDOWN, 0x0d);
                    assert!(signals.composing());
                }
                assert!(signals.composing());
            }
            assert!(!signals.composing());
        }
    }

    #[test]
    fn recording_preview_key_owner_idle_ordinary_key_is_not_claimed_by_observer() {
        let signals = Signals::new();
        for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
            let (_, _ordinary_key) = signals.begin_dispatch(message, 0x0d);
            assert!(!signals.composing());
        }
        {
            let (_, _process_key) = signals.begin_dispatch(WM_KEYDOWN, VK_PROCESSKEY.0 as usize);
            assert!(signals.composing());
        }
        assert!(!signals.composing());
    }

    #[test]
    fn recording_preview_key_owner_process_native_and_system_edges_are_denied() {
        for message in [WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP] {
            assert_eq!(
                decode_signal(message, VK_PROCESSKEY.0 as usize),
                Signal::ProcessKey
            );
            assert_eq!(decode_signal(message, 0x0d), Signal::Other);
        }
        assert_eq!(decode_signal(0, VK_PROCESSKEY.0 as usize), Signal::Other);
    }

    #[test]
    fn recording_preview_key_owner_focus_active_binding_and_liveness_all_required() {
        let ready = KeyboardFacts {
            alive: true,
            bound: true,
            focus: true,
            active: true,
        };
        assert!(keyboard_owned(ready));
        // 分别代表销毁、观察器脱离、原生子输入/保存窗和另一活动窗口。
        for denied in [
            KeyboardFacts {
                alive: false,
                ..ready
            },
            KeyboardFacts {
                bound: false,
                ..ready
            },
            KeyboardFacts {
                focus: false,
                ..ready
            },
            KeyboardFacts {
                active: false,
                ..ready
            },
        ] {
            assert!(!keyboard_owned(denied));
        }
    }

    #[test]
    fn recording_preview_key_owner_destroy_cannot_be_revived_by_later_ime_end() {
        let signals = Signals::new();
        signals.observe(Signal::Start);
        signals.observe(Signal::Destroy);
        signals.observe(Signal::End);
        assert!(!signals.alive.load(Ordering::Acquire));
        assert!(!keyboard_owned(KeyboardFacts {
            alive: signals.alive.load(Ordering::Acquire),
            bound: true,
            focus: true,
            active: true,
        }));
    }

    #[test]
    fn recording_preview_key_owner_drop_and_destroy_release_box_once() {
        for destroy_first in [false, true] {
            let token = Arc::new(BindingData::new());
            let boxed = Box::into_raw(Box::new(token.clone())) as usize;
            assert_eq!(Arc::strong_count(&token), 2);
            if destroy_first {
                token.signals.observe(Signal::Destroy);
            } else {
                token.signals.deactivate();
            }
            unsafe { release_data(boxed, &token) };
            assert_eq!(Arc::strong_count(&token), 1);
            // 另一条收尾路径只能读取仍活着的 token，不能二次访问已释放 Box。
            unsafe { release_data(boxed, &token) };
            assert_eq!(Arc::strong_count(&token), 1);
            assert!(token.released.load(Ordering::Acquire));
        }
    }

    #[test]
    fn recording_preview_key_owner_nested_destroy_keeps_callback_token_alive() {
        let probe_token = Arc::new(BindingData::new());
        let boxed = Box::into_raw(Box::new(probe_token.clone())) as usize;
        // 与原生回调相同：必须在默认链前克隆，之后不再借用原始 Box。
        let callback_token = unsafe { &*(boxed as *const Arc<BindingData>) }.clone();
        let weak = Arc::downgrade(&callback_token);
        {
            let _outer_default_chain = callback_token.signals.enter_ime_dispatch();
            callback_token.signals.observe(Signal::Destroy);
            unsafe { release_data(boxed, &probe_token) };
            drop(probe_token);
            assert!(weak.upgrade().is_some());
            assert!(callback_token.signals.composing());
            assert!(!callback_token.signals.alive.load(Ordering::Acquire));
            unsafe { release_data(boxed, &callback_token) };
        }
        drop(callback_token);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn recording_preview_key_owner_detach_failure_retains_callback_responsibility() {
        let probe_token = Arc::new(BindingData::new());
        let boxed = Box::into_raw(Box::new(probe_token.clone())) as usize;
        let weak = Arc::downgrade(&probe_token);
        probe_token.signals.deactivate();
        // 原生仍可能调用此观察器；模拟移除失败时不得执行 release_data。
        drop(probe_token);
        let callback_token = unsafe { &*(boxed as *const Arc<BindingData>) }.clone();
        assert!(weak.upgrade().is_some());
        assert!(!callback_token.signals.alive.load(Ordering::Acquire));
        callback_token.signals.observe(Signal::Destroy);
        unsafe { release_data(boxed, &callback_token) };
        drop(callback_token);
        assert!(weak.upgrade().is_none());
    }
}
