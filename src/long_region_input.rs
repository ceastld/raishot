//! 长截图自有 HWND 的输入适配。回调只操作有界事件队列和原生 capture，不借用 GPUI。
use crate::recording_key_owner as key_owner;

use crate::{long_region::RegionHandle, Result};
use key_owner::KeyOwnerProbe;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::ClientToScreen,
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const QUEUE_LIMIT: usize = 64;
static NEXT_GESTURE: AtomicU64 = AtomicU64::new(1);
static NEXT_SUBCLASS: AtomicUsize = AtomicUsize::new(0x5253_a000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegionInputEvent {
    Down {
        id: u64,
        handle: RegionHandle,
        point: (i32, i32),
    },
    Move {
        id: u64,
        point: (i32, i32),
    },
    Up {
        id: u64,
        point: (i32, i32),
    },
    Cancel {
        id: u64,
    },
}

#[derive(Clone, Copy)]
struct Active {
    id: u64,
    hwnd: usize,
}
struct Buffer {
    active: Option<Active>,
    delivered: Option<u64>,
    events: VecDeque<RegionInputEvent>,
}
impl Buffer {
    fn push(&mut self, event: RegionInputEvent) -> bool {
        if let RegionInputEvent::Move { id, .. } = event {
            if let Some(last_event) = self.events.back_mut() {
                if matches!(last_event, RegionInputEvent::Move { id: last, .. } if *last == id) {
                    *last_event = event;
                    return true;
                }
            }
        }
        if self.events.len() >= QUEUE_LIMIT {
            // 消费者暂时未泵消息时仍有界；丢弃输入必须取消已交给 UI 的手势。
            self.events.clear();
            if let Some(id) = self.delivered {
                self.events.push_back(RegionInputEvent::Cancel { id });
            }
            if let Some(active) = self.active.take() {
                if self.delivered != Some(active.id) {
                    self.events
                        .push_back(RegionInputEvent::Cancel { id: active.id });
                }
            }
            return false;
        }
        self.events.push_back(event);
        true
    }
}
struct InputState {
    buffer: Mutex<Buffer>,
    panel: AtomicUsize,
    resize_enabled: AtomicBool,
    transition: AtomicUsize,
    escape_held: AtomicBool,
    escape_forwarded: AtomicBool,
}
impl InputState {
    fn active(&self) -> Option<Active> {
        self.buffer.lock().active
    }

    fn cancel(&self, only: Option<usize>) -> Option<usize> {
        let mut buffer = self.buffer.lock();
        let active = buffer.active?;
        if only.is_some_and(|hwnd| hwnd != active.hwnd) {
            return None;
        }
        // 先使请求失效，再 ReleaseCapture；同步 WM_CAPTURECHANGED 不能取消后来的 ID。
        buffer.active = None;
        buffer.push(RegionInputEvent::Cancel { id: active.id });
        Some(active.hwnd)
    }

    fn finish(&self, hwnd: usize, point: (i32, i32)) -> Option<usize> {
        let mut buffer = self.buffer.lock();
        let active = buffer.active?;
        if active.hwnd != hwnd {
            return None;
        }
        buffer.active = None;
        buffer.push(RegionInputEvent::Up {
            id: active.id,
            point,
        });
        Some(active.hwnd)
    }

    fn move_point(&self, hwnd: usize, point: (i32, i32)) -> Option<usize> {
        let mut buffer = self.buffer.lock();
        let active = buffer.active?;
        if active.hwnd != hwnd
            || buffer.push(RegionInputEvent::Move {
                id: active.id,
                point,
            })
        {
            None
        } else {
            Some(active.hwnd)
        }
    }

    fn begin(&self, hwnd: HWND, handle: RegionHandle, point: (i32, i32)) -> Result<u64> {
        let id = NEXT_GESTURE.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            return Err("长截图选区输入 ID 已耗尽".into());
        }
        {
            let mut buffer = self.buffer.lock();
            if buffer.active.is_some() {
                return Err("长截图选区手势尚未结束".into());
            }
            buffer.active = Some(Active {
                id,
                hwnd: hwnd.0 as usize,
            });
            if !buffer.push(RegionInputEvent::Down { id, handle, point }) {
                return Err("长截图选区输入队列已满，本次手势已取消".into());
            }
        }
        {
            let _transition = CaptureTransition::new(self);
            unsafe {
                SetCapture(hwnd);
            }
        }
        if unsafe { GetCapture() } != hwnd || self.active().is_none_or(|active| active.id != id) {
            release_capture(self, self.cancel(Some(hwnd.0 as usize)));
            return Err("无法捕获长截图选区拖动".into());
        }
        Ok(id)
    }
}

struct CaptureTransition<'a>(&'a InputState);
impl<'a> CaptureTransition<'a> {
    fn new(state: &'a InputState) -> Self {
        state.transition.fetch_add(1, Ordering::Relaxed);
        Self(state)
    }
}
impl Drop for CaptureTransition<'_> {
    fn drop(&mut self) {
        self.0.transition.fetch_sub(1, Ordering::Relaxed);
    }
}
fn release_capture(state: &InputState, capture: Option<usize>) {
    if capture.is_some_and(|hwnd| unsafe { GetCapture() }.0 as usize == hwnd) {
        let _transition = CaptureTransition::new(state);
        unsafe {
            let _ = ReleaseCapture();
        }
    }
}

struct BindingData {
    state: Arc<InputState>,
    handle: Option<RegionHandle>,
    // 仅原生柄独立观察 IME；panel 按下交给 GPUI 根的既有输入所有权门。
    key_owner: Option<KeyOwnerProbe>,
    released: AtomicBool,
}
struct NativeBinding {
    hwnd: usize,
    id: usize,
    data: usize,
    token: Rc<BindingData>,
}
impl NativeBinding {
    fn install(hwnd: HWND, handle: Option<RegionHandle>, state: Arc<InputState>) -> Result<Self> {
        let mut process = 0;
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool()
            || unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) }
                != unsafe { GetCurrentThreadId() }
            || process != unsafe { GetCurrentProcessId() }
        {
            return Err("长截图输入只能绑定当前线程的自有窗口".into());
        }
        let token = Rc::new(BindingData {
            state,
            handle,
            // 观察器失败只禁用柄的 Esc，不能连带禁用已有鼠标调整输入。
            key_owner: handle.and_then(|_| KeyOwnerProbe::install(hwnd).ok()),
            released: AtomicBool::new(false),
        });
        let data = Box::into_raw(Box::new(token.clone())) as usize;
        let id = NEXT_SUBCLASS.fetch_add(1, Ordering::Relaxed);
        if !unsafe { SetWindowSubclass(hwnd, Some(input_proc), id, data) }.as_bool() {
            unsafe {
                drop(Box::from_raw(data as *mut Rc<BindingData>));
            }
            return Err("无法绑定长截图选区输入".into());
        }
        Ok(Self {
            hwnd: hwnd.0 as usize,
            id,
            data,
            token,
        })
    }
}
unsafe fn owned(hwnd: HWND, id: usize, expected: usize) -> bool {
    let mut actual = 0;
    unsafe { GetWindowSubclass(hwnd, Some(input_proc), id, Some(&mut actual)) }.as_bool()
        && actual == expected
}
unsafe fn release_data(data: usize, token: &BindingData) {
    if !token.released.swap(true, Ordering::AcqRel) {
        unsafe {
            drop(Box::from_raw(data as *mut Rc<BindingData>));
        }
    }
}
impl Drop for NativeBinding {
    fn drop(&mut self) {
        if self.token.handle.is_none() {
            let _ = self.token.state.panel.compare_exchange(
                self.hwnd,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        release_capture(&self.token.state, self.token.state.cancel(Some(self.hwnd)));
        if !self.token.released.load(Ordering::Acquire) {
            let hwnd = HWND(self.hwnd as *mut _);
            unsafe {
                if !owned(hwnd, self.id, self.data)
                    || RemoveWindowSubclass(hwnd, Some(input_proc), self.id).as_bool()
                {
                    release_data(self.data, &self.token);
                }
            }
        }
    }
}

pub(crate) struct NativeRegionInput {
    state: Arc<InputState>,
    bindings: Vec<NativeBinding>,
    owner_thread: u32,
    _ui_thread: PhantomData<Rc<()>>,
}
impl NativeRegionInput {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(InputState {
                buffer: Mutex::new(Buffer {
                    active: None,
                    delivered: None,
                    events: VecDeque::with_capacity(QUEUE_LIMIT),
                }),
                panel: AtomicUsize::new(0),
                resize_enabled: AtomicBool::new(false),
                transition: AtomicUsize::new(0),
                escape_held: AtomicBool::new(false),
                escape_forwarded: AtomicBool::new(false),
            }),
            bindings: Vec::new(),
            owner_thread: unsafe { GetCurrentThreadId() },
            _ui_thread: PhantomData,
        }
    }
    fn check_thread(&self) -> Result<()> {
        if self.owner_thread != unsafe { GetCurrentThreadId() } {
            return Err("长截图选区输入只能由创建线程更新".into());
        }
        Ok(())
    }
    pub(crate) fn attach_handle(&mut self, hwnd: HWND, kind: RegionHandle) -> Result<()> {
        self.check_thread()?;
        self.bindings.push(NativeBinding::install(
            hwnd,
            Some(kind),
            self.state.clone(),
        )?);
        Ok(())
    }
    pub(crate) fn bind_panel(&mut self, hwnd: HWND) -> Result<()> {
        self.check_thread()?;
        if self.bindings.iter().any(|binding| {
            binding.token.handle.is_none()
                && binding.hwnd == hwnd.0 as usize
                && !binding.token.released.load(Ordering::Acquire)
        }) {
            self.state.panel.store(hwnd.0 as usize, Ordering::Release);
            return Ok(());
        }
        self.cancel();
        self.bindings
            .retain(|binding| binding.token.handle.is_some());
        self.bindings
            .push(NativeBinding::install(hwnd, None, self.state.clone())?);
        self.state.panel.store(hwnd.0 as usize, Ordering::Release);
        Ok(())
    }
    pub(crate) fn set_resize_enabled(&self, enabled: bool) -> Result<()> {
        self.check_thread()?;
        self.state.resize_enabled.store(enabled, Ordering::Release);
        Ok(())
    }
    pub(crate) fn begin_move_capture(&mut self, panel: HWND, point: (i32, i32)) -> Result<u64> {
        self.bind_panel(panel)?;
        self.state.begin(panel, RegionHandle::Move, point)
    }
    pub(crate) fn active_hwnd(&self) -> Option<HWND> {
        self.state
            .active()
            .map(|active| HWND(active.hwnd as *mut _))
    }
    pub(crate) fn cancel(&self) {
        if self.owner_thread == unsafe { GetCurrentThreadId() } {
            release_capture(&self.state, self.state.cancel(None));
        }
    }
    pub(crate) fn drain_events(&self) -> Vec<RegionInputEvent> {
        let mut buffer = self.state.buffer.lock();
        let events: Vec<_> = buffer.events.drain(..).collect();
        for event in &events {
            match *event {
                RegionInputEvent::Down { id, .. } => buffer.delivered = Some(id),
                RegionInputEvent::Up { id, .. } | RegionInputEvent::Cancel { id }
                    if buffer.delivered == Some(id) =>
                {
                    buffer.delivered = None
                }
                _ => {}
            }
        }
        events
    }
}
impl Drop for NativeRegionInput {
    fn drop(&mut self) {
        self.state.panel.store(0, Ordering::Release);
        self.cancel();
    }
}

fn physical_point(hwnd: HWND, lparam: LPARAM) -> Result<(i32, i32)> {
    let bits = lparam.0 as u32;
    let mut point = POINT {
        x: (bits as u16 as i16) as i32,
        y: ((bits >> 16) as u16 as i16) as i32,
    };
    unsafe {
        ClientToScreen(hwnd, &mut point).ok()?;
    }
    Ok((point.x, point.y))
}
fn handle_cursor(handle: RegionHandle) -> windows::core::PCWSTR {
    match handle {
        RegionHandle::North | RegionHandle::South => IDC_SIZENS,
        RegionHandle::East | RegionHandle::West => IDC_SIZEWE,
        RegionHandle::NorthEast | RegionHandle::SouthWest => IDC_SIZENESW,
        RegionHandle::NorthWest | RegionHandle::SouthEast => IDC_SIZENWSE,
        RegionHandle::Move => IDC_SIZEALL,
    }
}

fn forward_idle_escape(state: &InputState, message: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    let panel = HWND(state.panel.load(Ordering::Acquire) as *mut _);
    let mut process = 0;
    if panel.is_invalid()
        || !unsafe { IsWindow(Some(panel)) }.as_bool()
        || unsafe { GetWindowThreadProcessId(panel, Some(&mut process)) }
            != unsafe { GetCurrentThreadId() }
        || process != unsafe { GetCurrentProcessId() }
    {
        return false;
    }
    // Up 到焦点交接之间仍由原生柄接收键盘。只投递自己的 panel，保留 held 位，
    // 不同步进入 GPUI；字符消息不转发，组合文本仍由 panel 的输入合同处理。
    unsafe { PostMessageW(Some(panel), message, wparam, lparam) }.is_ok()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EscapePress {
    held: bool,
    control: bool,
    alt: bool,
    shift: bool,
    platform: bool,
    alt_context: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeDownRoute {
    Pass,
    Consume,
    Cancel,
    Forward,
}

fn escape_new_press_retires_previous(
    has_press: bool,
    native_handle: bool,
    keyboard_owned: bool,
    ime_composing: bool,
    held: bool,
) -> bool {
    // 失焦后 Up 可能在外部收到。只有柄的新首按能证明上一按下已结束；
    // panel 收到的异步转发首 Down 仍属于旧责任，不能在这里将它退休。
    has_press && native_handle && keyboard_owned && !ime_composing && !held
}

fn escape_down_route(
    press: EscapePress,
    active: bool,
    handle: bool,
    canceled_press: bool,
    forwarded_press: bool,
    input_owned: bool,
    ime_composing: bool,
) -> EscapeDownRoute {
    // IME 拥有整个 Down 调度，包括已消费按下的 held。责任标记仍保留到 Up，
    // 因而组合结束后的重复消息也不能重新领取取消/转发。
    if ime_composing {
        return EscapeDownRoute::Pass;
    }
    // 已消费的同一次按下仍拥有 held；后来增减修饰键不能使它再次取消或穿透。
    if canceled_press {
        return EscapeDownRoute::Consume;
    }
    if forwarded_press {
        // 同一 state 也绑定 panel。投递的首 Down 必须通过 panel 子类链到达 GPUI；
        // 只有原生柄吞掉其物理 held，避免把已转发的按下当作新的区域取消。
        return if handle {
            EscapeDownRoute::Consume
        } else {
            EscapeDownRoute::Pass
        };
    }
    if !input_owned || press != EscapePress::default() {
        return EscapeDownRoute::Pass;
    }
    if active && handle {
        EscapeDownRoute::Cancel
    } else if handle {
        EscapeDownRoute::Forward
    } else {
        EscapeDownRoute::Pass
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeUpRoute {
    Pass,
    Consume,
    Forward,
}

fn escape_up_route(canceled_press: bool, forwarded_press: bool, handle: bool) -> EscapeUpRoute {
    if canceled_press {
        EscapeUpRoute::Consume
    } else if forwarded_press && handle {
        EscapeUpRoute::Forward
    } else {
        EscapeUpRoute::Pass
    }
}

fn escape_press(message: u32, lparam: LPARAM) -> EscapePress {
    EscapePress {
        held: lparam.0 & (1 << 30) != 0,
        control: unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0,
        alt: unsafe { GetKeyState(i32::from(VK_MENU.0)) } < 0,
        shift: unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0,
        platform: unsafe { GetKeyState(i32::from(VK_LWIN.0)) } < 0
            || unsafe { GetKeyState(i32::from(VK_RWIN.0)) } < 0,
        // WM_SYSKEYDOWN 的 Alt 上下文也拒绝，不能仅依赖随后读到的 VK_MENU。
        alt_context: message == WM_SYSKEYDOWN && lparam.0 & (1 << 29) != 0,
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
    // 默认链可同步销毁 HWND；先克隆独立 token，不能跨 DefSubclassProc 借用 Box。
    let token = unsafe { &*(data as *const Rc<BindingData>) }.clone();
    let state = &token.state;
    let native = hwnd.0 as usize;
    let active = state.active();
    let owns_active = active.is_some_and(|active| active.hwnd == native);
    let transitioning = state.transition.load(Ordering::Relaxed) != 0;
    let interacting = active.is_some() || transitioning;

    if let Some(handle) = token.handle {
        match message {
            WM_NCHITTEST => {
                return LRESULT(
                    if state.resize_enabled.load(Ordering::Acquire) || owns_active {
                        HTCLIENT as isize
                    } else {
                        HTTRANSPARENT as isize
                    },
                )
            }
            // 这里只返回标准激活策略；不在回调中借用 GPUI 或主动切前台。
            WM_MOUSEACTIVATE => return LRESULT(MA_ACTIVATE as isize),
            WM_SETCURSOR => {
                if let Ok(cursor) = unsafe { LoadCursorW(None, handle_cursor(handle)) } {
                    unsafe {
                        SetCursor(Some(cursor));
                    }
                }
                return LRESULT(1);
            }
            WM_LBUTTONDOWN if !interacting && state.resize_enabled.load(Ordering::Acquire) => {
                if let Ok(point) = physical_point(hwnd, lparam) {
                    let _ = state.begin(hwnd, handle, point);
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }

    match message {
        WM_MOUSEMOVE if interacting => {
            if owns_active {
                let release = if wparam.0 & 1 == 0 {
                    state.cancel(Some(native))
                } else if let Ok(point) = physical_point(hwnd, lparam) {
                    state.move_point(native, point)
                } else {
                    state.cancel(Some(native))
                };
                release_capture(state, release);
            }
            return LRESULT(0);
        }
        WM_LBUTTONUP if interacting => {
            if owns_active {
                let release = if let Ok(point) = physical_point(hwnd, lparam) {
                    state.finish(native, point)
                } else {
                    state.cancel(Some(native))
                };
                release_capture(state, release);
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if interacting => return LRESULT(0),
        WM_RBUTTONDOWN | WM_CANCELMODE if interacting => {
            release_capture(state, state.cancel(None));
            return LRESULT(0);
        }
        WM_CAPTURECHANGED if interacting => {
            if owns_active && lparam.0 as usize != native {
                state.cancel(Some(native));
            }
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            let press = escape_press(message, lparam);
            let keyboard_owned = token
                .key_owner
                .as_ref()
                .is_some_and(KeyOwnerProbe::owns_keyboard);
            let ime_composing = token
                .key_owner
                .as_ref()
                .is_none_or(KeyOwnerProbe::composing);
            if escape_new_press_retires_previous(
                state.escape_held.load(Ordering::Acquire)
                    || state.escape_forwarded.load(Ordering::Acquire),
                token.handle.is_some(),
                keyboard_owned,
                ime_composing,
                press.held,
            ) {
                // 不伪造遗失的 Up；按本次新的物理按下重新领取责任。
                state.escape_held.store(false, Ordering::Release);
                state.escape_forwarded.store(false, Ordering::Release);
            }
            match escape_down_route(
                press,
                active.is_some(),
                token.handle.is_some(),
                state.escape_held.load(Ordering::Acquire),
                state.escape_forwarded.load(Ordering::Acquire),
                keyboard_owned,
                ime_composing,
            ) {
                EscapeDownRoute::Pass => {}
                EscapeDownRoute::Consume => return LRESULT(0),
                EscapeDownRoute::Cancel => {
                    state.escape_held.store(true, Ordering::Release);
                    release_capture(state, state.cancel(None));
                    return LRESULT(0);
                }
                EscapeDownRoute::Forward => {
                    if forward_idle_escape(state, message, wparam, lparam) {
                        state.escape_forwarded.store(true, Ordering::Release);
                        return LRESULT(0);
                    }
                }
            }
        }
        WM_KEYUP | WM_SYSKEYUP if wparam.0 == usize::from(VK_ESCAPE.0) => {
            // 两种所有权均在 Up 消费，不重新检查修饰或 active；
            // Up 仍在柄则配对投递，焦点已交回 panel 则直接过默认链。
            match escape_up_route(
                state.escape_held.swap(false, Ordering::AcqRel),
                state.escape_forwarded.swap(false, Ordering::AcqRel),
                token.handle.is_some(),
            ) {
                EscapeUpRoute::Pass => {}
                EscapeUpRoute::Consume => return LRESULT(0),
                EscapeUpRoute::Forward => {
                    // Down 已成功投递，Up 投递失败也不能把同一次原生按下交给其它入口。
                    let _ = forward_idle_escape(state, message, wparam, lparam);
                    return LRESULT(0);
                }
            }
        }
        WM_CHAR | WM_SYSCHAR
            if wparam.0 == 27
                && (state.escape_held.load(Ordering::Acquire)
                    || state.escape_forwarded.load(Ordering::Acquire)) =>
        {
            return LRESULT(0)
        }
        WM_KILLFOCUS | WM_CLOSE | WM_DESTROY | WM_NCDESTROY => {
            release_capture(
                state,
                state.cancel(if token.handle.is_none() {
                    None
                } else {
                    Some(native)
                }),
            );
        }
        WM_ACTIVATE if wparam.0 & 0xffff == WA_INACTIVE as usize => {
            release_capture(state, state.cancel(None));
        }
        _ => {}
    }
    if message == WM_NCDESTROY {
        if token.handle.is_none() {
            let _ = state
                .panel
                .compare_exchange(native, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        let was_owned = unsafe { owned(hwnd, id, data) };
        if was_owned {
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(input_proc), id) };
        }
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        if was_owned {
            unsafe {
                release_data(data, &token);
            }
        }
        return result;
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod escape_tests {
    use super::{
        escape_down_route, escape_new_press_retires_previous, escape_up_route, EscapeDownRoute,
        EscapePress, EscapeUpRoute,
    };

    fn press(mask: u8, held: bool) -> EscapePress {
        EscapePress {
            held,
            control: mask & 1 != 0,
            alt: mask & 2 != 0,
            shift: mask & 4 != 0,
            platform: mask & 8 != 0,
            alt_context: mask & 16 != 0,
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Effect {
        Cancel,
        ForwardDown,
        ForwardUpAttempt,
        Consume,
        Pass,
    }

    #[derive(Default)]
    struct Trace {
        canceled_press: bool,
        forwarded_press: bool,
        effects: Vec<Effect>,
    }

    impl Trace {
        fn down(&mut self, press: EscapePress, active: bool, handle: bool, post_ok: bool) {
            self.down_with_owner(press, active, handle, post_ok, true, false);
        }

        fn down_with_owner(
            &mut self,
            press: EscapePress,
            active: bool,
            handle: bool,
            post_ok: bool,
            keyboard_owned: bool,
            ime_composing: bool,
        ) {
            if escape_new_press_retires_previous(
                self.canceled_press || self.forwarded_press,
                handle,
                keyboard_owned,
                ime_composing,
                press.held,
            ) {
                self.canceled_press = false;
                self.forwarded_press = false;
            }
            match escape_down_route(
                press,
                active,
                handle,
                self.canceled_press,
                self.forwarded_press,
                keyboard_owned,
                ime_composing,
            ) {
                EscapeDownRoute::Pass => self.effects.push(Effect::Pass),
                EscapeDownRoute::Consume => self.effects.push(Effect::Consume),
                EscapeDownRoute::Cancel => {
                    self.canceled_press = true;
                    self.effects.push(Effect::Cancel);
                }
                EscapeDownRoute::Forward if post_ok => {
                    self.forwarded_press = true;
                    self.effects.push(Effect::ForwardDown);
                }
                EscapeDownRoute::Forward => self.effects.push(Effect::Pass),
            }
        }

        fn up(&mut self, handle: bool) {
            let canceled = std::mem::take(&mut self.canceled_press);
            let forwarded = std::mem::take(&mut self.forwarded_press);
            match escape_up_route(canceled, forwarded, handle) {
                EscapeUpRoute::Pass => self.effects.push(Effect::Pass),
                EscapeUpRoute::Consume => self.effects.push(Effect::Consume),
                EscapeUpRoute::Forward => self.effects.push(Effect::ForwardUpAttempt),
            }
        }
    }

    #[test]
    fn long_output_native_escape_active_requires_plain_first_press() {
        for mask in 0..32 {
            for held in [false, true] {
                for handle in [false, true] {
                    let actual = escape_down_route(
                        press(mask, held),
                        true,
                        handle,
                        false,
                        false,
                        true,
                        false,
                    );
                    let expected = if mask == 0 && !held && handle {
                        EscapeDownRoute::Cancel
                    } else {
                        EscapeDownRoute::Pass
                    };
                    assert_eq!(actual, expected, "额外修饰或 held 不能取消区域");
                }
            }
        }
    }

    #[test]
    fn long_output_native_escape_idle_handle_forwards_only_plain_first_press() {
        for mask in 0..32 {
            for held in [false, true] {
                let actual =
                    escape_down_route(press(mask, held), false, true, false, false, true, false);
                let expected = if mask == 0 && !held {
                    EscapeDownRoute::Forward
                } else {
                    EscapeDownRoute::Pass
                };
                assert_eq!(actual, expected, "闲置柄不得转发额外修饰或 held");
            }
        }
    }

    #[test]
    fn long_output_native_escape_idle_panel_keeps_gpui_owner() {
        for mask in 0..32 {
            for held in [false, true] {
                assert_eq!(
                    escape_down_route(press(mask, held), false, false, false, false, true, false),
                    EscapeDownRoute::Pass,
                    "面板的正常按下仍交给 GPUI 路由"
                );
            }
        }
    }

    #[test]
    fn long_output_native_escape_alt_context_rejects_without_menu_key() {
        let only_context = EscapePress {
            alt_context: true,
            ..EscapePress::default()
        };
        assert!(!only_context.alt);
        for active in [false, true] {
            assert_eq!(
                escape_down_route(only_context, active, true, false, false, true, false),
                EscapeDownRoute::Pass,
                "SYSKEY 的 Alt 上下文不能由 VK_MENU 已释放绕过"
            );
        }
    }

    #[test]
    fn long_output_native_escape_cancel_does_not_repeat_or_cancel_next_gesture() {
        let mut trace = Trace::default();
        trace.down(press(0, false), true, true, true);
        // 首次已取消的同一按下不能因修饰变化或后来出现新手势再次 state.cancel。
        for mask in 0..32 {
            for active in [false, true] {
                trace.down(press(mask, true), active, true, true);
            }
        }
        assert_eq!(
            trace
                .effects
                .iter()
                .filter(|e| **e == Effect::Cancel)
                .count(),
            1
        );
        assert_eq!(
            trace
                .effects
                .iter()
                .filter(|e| **e == Effect::Consume)
                .count(),
            64
        );
        trace.up(true);
        trace.up(true);
        assert_eq!(
            &trace.effects[trace.effects.len() - 2..],
            &[Effect::Consume, Effect::Pass]
        );
    }

    #[test]
    fn long_output_native_escape_forward_does_not_repeat_or_cancel_next_gesture() {
        let mut trace = Trace::default();
        trace.down(press(0, false), false, true, true);
        for mask in 0..32 {
            for active in [false, true] {
                trace.down(press(mask, true), active, true, true);
            }
        }
        trace.up(true);
        trace.up(true);
        assert_eq!(
            trace
                .effects
                .iter()
                .filter(|e| **e == Effect::ForwardDown)
                .count(),
            1
        );
        assert_eq!(
            trace
                .effects
                .iter()
                .filter(|e| **e == Effect::Cancel)
                .count(),
            0
        );
        assert_eq!(
            trace
                .effects
                .iter()
                .filter(|e| **e == Effect::Consume)
                .count(),
            64
        );
        assert_eq!(
            &trace.effects[trace.effects.len() - 2..],
            &[Effect::ForwardUpAttempt, Effect::Pass]
        );
    }

    #[test]
    fn long_output_native_escape_failed_down_post_never_claims_press() {
        let mut trace = Trace::default();
        trace.down(press(0, false), false, true, false);
        trace.down(press(0, true), false, true, true);
        trace.up(true);
        assert_eq!(trace.effects, [Effect::Pass, Effect::Pass, Effect::Pass]);
        trace.down(press(0, false), false, true, true);
        trace.up(true);
        assert_eq!(
            &trace.effects[3..],
            &[Effect::ForwardDown, Effect::ForwardUpAttempt]
        );
    }

    #[test]
    fn long_output_native_escape_modified_press_cannot_become_plain_repeat() {
        for initial_mask in 1..32 {
            for active in [false, true] {
                let mut trace = Trace::default();
                trace.down(press(initial_mask, false), active, true, true);
                trace.down(press(0, true), active, true, true);
                trace.up(true);
                assert_eq!(trace.effects, [Effect::Pass, Effect::Pass, Effect::Pass]);
            }
        }
    }

    #[test]
    fn long_output_native_escape_up_releases_one_matched_responsibility() {
        // 独立 Up 合同：原生取消优先，已转发按下配对投递 Up，无责任则放行。
        let rows = [
            (false, false, EscapeUpRoute::Pass),
            (true, false, EscapeUpRoute::Consume),
            (false, true, EscapeUpRoute::Forward),
            (true, true, EscapeUpRoute::Consume),
        ];
        for (canceled, forwarded, expected) in rows {
            assert_eq!(escape_up_route(canceled, forwarded, true), expected);
            let mut trace = Trace {
                canceled_press: canceled,
                forwarded_press: forwarded,
                ..Trace::default()
            };
            trace.up(true);
            assert!(!trace.canceled_press && !trace.forwarded_press);
            trace.up(true);
            assert_eq!(trace.effects.last(), Some(&Effect::Pass));
        }
    }

    #[test]
    fn long_output_native_escape_new_first_press_works_after_paired_up() {
        let mut trace = Trace::default();
        trace.down(press(0, false), true, true, true);
        trace.up(true);
        trace.down(press(0, false), false, true, true);
        trace.up(true);
        trace.down(press(0, false), true, false, true);
        trace.up(false);
        assert_eq!(
            trace.effects,
            [
                Effect::Cancel,
                Effect::Consume,
                Effect::ForwardDown,
                Effect::ForwardUpAttempt,
                Effect::Pass,
                Effect::Pass
            ]
        );
    }

    #[test]
    fn long_output_native_escape_posted_down_passes_panel_subclass_once() {
        let mut trace = Trace::default();
        trace.down(press(0, false), false, true, true);
        // panel 子类不能吞掉已投递的 Down；即使随后出现 active，也不能再次原生取消。
        trace.down(press(0, false), true, false, true);
        trace.down(press(31, true), true, true, true);
        trace.up(true);
        trace.up(false);
        assert_eq!(
            trace.effects,
            [
                Effect::ForwardDown,
                Effect::Pass,
                Effect::Consume,
                Effect::ForwardUpAttempt,
                Effect::Pass
            ]
        );
    }

    #[test]
    fn long_output_native_escape_focus_handoff_up_passes_panel_without_self_post() {
        let mut trace = Trace::default();
        trace.down(press(0, false), false, true, true);
        // 焦点已交回 panel：物理 Up 直接过默认链，不向同一 HWND 再投递一次。
        trace.up(false);
        assert_eq!(trace.effects, [Effect::ForwardDown, Effect::Pass]);
        assert!(!trace.forwarded_press);
        trace.down(press(0, false), true, false, true);
        assert_eq!(trace.effects.last(), Some(&Effect::Pass));
    }

    #[test]
    fn long_output_native_escape_active_panel_defers_to_gpui_input_owner() {
        // panel 上的原生钩子不能在字段/IME guard 前取消活动手势。
        for mask in 0..32 {
            for held in [false, true] {
                for input_owned in [false, true] {
                    assert_eq!(
                        escape_down_route(
                            press(mask, held),
                            true,
                            false,
                            false,
                            false,
                            input_owned,
                            false
                        ),
                        EscapeDownRoute::Pass
                    );
                }
            }
        }
    }

    #[test]
    fn long_output_native_escape_handle_missing_owner_never_starts_action() {
        for mask in 0..32 {
            for held in [false, true] {
                for active in [false, true] {
                    assert_eq!(
                        escape_down_route(
                            press(mask, held),
                            active,
                            true,
                            false,
                            false,
                            false,
                            false
                        ),
                        EscapeDownRoute::Pass,
                        "IME、原生焦点或观察器资格不足时不能取消/转发新按下"
                    );
                }
            }
        }
    }

    #[test]
    fn long_output_native_escape_existing_press_finishes_after_owner_handoff() {
        for active in [false, true] {
            for mask in 0..32 {
                for (canceled, forwarded) in [(true, false), (false, true)] {
                    assert_eq!(
                        escape_down_route(
                            press(mask, true),
                            active,
                            true,
                            canceled,
                            forwarded,
                            false,
                            false
                        ),
                        EscapeDownRoute::Consume,
                        "已消费的按下仍只收束，不重复动作或凭后来输入所有权穿透"
                    );
                }
            }
        }
    }

    #[test]
    fn long_output_native_escape_ime_precedes_held_and_existing_press() {
        for mask in 0..32 {
            for held in [false, true] {
                for active in [false, true] {
                    for handle in [false, true] {
                        for input_owned in [false, true] {
                            for (canceled, forwarded) in
                                [(false, false), (true, false), (false, true), (true, true)]
                            {
                                assert_eq!(
                                    escape_down_route(
                                        press(mask, held),
                                        active,
                                        handle,
                                        canceled,
                                        forwarded,
                                        input_owned,
                                        true
                                    ),
                                    EscapeDownRoute::Pass,
                                    "IME Down 所有权高于 held 和已有按下责任"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn long_output_native_escape_ime_handoff_retains_press_until_up() {
        for canceled in [false, true] {
            let mut trace = Trace::default();
            trace.down(press(0, false), canceled, true, true);
            let responsibility = (trace.canceled_press, trace.forwarded_press);
            for mask in 0..32 {
                let route = escape_down_route(
                    press(mask, true),
                    true,
                    true,
                    trace.canceled_press,
                    trace.forwarded_press,
                    true,
                    true,
                );
                assert_eq!(route, EscapeDownRoute::Pass);
                assert_eq!(
                    (trace.canceled_press, trace.forwarded_press),
                    responsibility
                );
            }
            // END 后同一次 held 仍只消费，不能重新取消后来出现的活动手势。
            trace.down(press(0, true), true, true, true);
            trace.up(true);
            trace.up(true);
            let first = if canceled {
                Effect::Cancel
            } else {
                Effect::ForwardDown
            };
            let release = if canceled {
                Effect::Consume
            } else {
                Effect::ForwardUpAttempt
            };
            assert_eq!(
                trace.effects,
                [first, Effect::Consume, release, Effect::Pass]
            );
        }
    }

    #[test]
    fn long_output_native_escape_lifetime_lost_up_new_native_first_retires_previous_press() {
        for canceled in [false, true] {
            let mut trace = Trace::default();
            trace.down(press(0, false), canceled, true, true);
            // 失焦后在外部松开，自有 HWND 没有 Up；回来收到新的非 held 首按。
            trace.down(press(0, false), canceled, true, true);
            trace.down(press(0, true), true, true, true);
            trace.up(true);
            trace.up(true);
            let first = if canceled {
                Effect::Cancel
            } else {
                Effect::ForwardDown
            };
            let release = if canceled {
                Effect::Consume
            } else {
                Effect::ForwardUpAttempt
            };
            assert_eq!(
                trace.effects,
                [first, first, Effect::Consume, release, Effect::Pass]
            );
        }
    }

    #[test]
    fn long_output_native_escape_lifetime_held_internal_handoff_keeps_once_only_responsibility() {
        for canceled in [false, true] {
            let mut trace = Trace::default();
            trace.down(press(0, false), canceled, true, true);
            let responsibility = (trace.canceled_press, trace.forwarded_press);
            // 同一 held 从柄交到 panel 或另一柄，都不能把后来手势再取消一次。
            for handle in [false, true] {
                for mask in 0..32 {
                    trace.down(press(mask, true), true, handle, true);
                    assert_eq!(
                        (trace.canceled_press, trace.forwarded_press),
                        responsibility
                    );
                }
            }
            assert_eq!(
                trace
                    .effects
                    .iter()
                    .filter(|effect| { matches!(**effect, Effect::Cancel | Effect::ForwardDown) })
                    .count(),
                1
            );
            trace.up(false);
            assert!(!trace.canceled_press && !trace.forwarded_press);
        }
    }

    #[test]
    fn long_output_native_escape_lifetime_posted_panel_down_never_retires_press() {
        let mut trace = Trace::default();
        trace.down(press(0, false), false, true, true);
        // 投递到 panel 的首 Down 也为非 held，但不是新原生柄按下。
        trace.down(press(0, false), true, false, true);
        assert!(trace.forwarded_press);
        assert!(!trace.canceled_press);
        assert_eq!(trace.effects, [Effect::ForwardDown, Effect::Pass]);
        trace.down(press(0, true), true, true, true);
        trace.up(false);
        assert_eq!(
            trace.effects,
            [
                Effect::ForwardDown,
                Effect::Pass,
                Effect::Consume,
                Effect::Pass
            ]
        );
    }

    #[test]
    fn long_output_native_escape_lifetime_ime_or_missing_focus_preserves_previous_press() {
        for canceled in [false, true] {
            for (owned, composing) in [(false, false), (false, true), (true, true)] {
                for held in [false, true] {
                    for mask in 0..32 {
                        let mut trace = Trace::default();
                        trace.down(press(0, false), canceled, true, true);
                        let responsibility = (trace.canceled_press, trace.forwarded_press);
                        trace.down_with_owner(
                            press(mask, held),
                            true,
                            true,
                            true,
                            owned,
                            composing,
                        );
                        assert_eq!(
                            (trace.canceled_press, trace.forwarded_press),
                            responsibility
                        );
                        let expected = if composing {
                            Effect::Pass
                        } else {
                            Effect::Consume
                        };
                        assert_eq!(trace.effects.last(), Some(&expected));
                        // 所有权恢复后的同次 held 仍只收束，不重新领取动作。
                        trace.down(press(0, true), true, true, true);
                        assert_eq!(trace.effects.last(), Some(&Effect::Consume));
                    }
                }
            }
        }
    }

    #[test]
    fn long_output_native_escape_lifetime_modified_native_first_retires_without_new_action() {
        for canceled in [false, true] {
            for mask in 1..32 {
                let mut trace = Trace::default();
                trace.down(press(0, false), canceled, true, true);
                // 新物理按下即使带额外修饰，也不能继续持有上一按下的责任。
                trace.down(press(mask, false), true, true, true);
                assert!(!trace.canceled_press && !trace.forwarded_press);
                trace.down(press(0, true), true, true, true);
                trace.up(true);
                let first = if canceled {
                    Effect::Cancel
                } else {
                    Effect::ForwardDown
                };
                assert_eq!(
                    trace.effects,
                    [first, Effect::Pass, Effect::Pass, Effect::Pass]
                );
            }
        }
    }
}

#[cfg(test)]
mod binding_lifetime_tests {
    use super::*;

    fn token(input: &NativeRegionInput) -> Rc<BindingData> {
        Rc::new(BindingData {
            state: input.state.clone(),
            handle: None,
            key_owner: None,
            released: AtomicBool::new(false),
        })
    }

    #[test]
    fn callback_clone_keeps_binding_alive_after_native_payload_and_owner_release() {
        let input = NativeRegionInput::new();
        let owner = token(&input);
        let weak = Rc::downgrade(&owner);
        let data = Box::into_raw(Box::new(owner.clone())) as usize;
        // 与真实 input_proc 一样，先持有回调局部引用，允许同步销毁释放原生载荷。
        let callback = unsafe { &*(data as *const Rc<BindingData>) }.clone();
        unsafe { release_data(data, &callback) };
        drop(owner);
        assert!(weak.upgrade().is_some(), "回调返回前绑定必须存活");
        assert_eq!(Rc::strong_count(&callback), 1);
        assert_eq!(Arc::strong_count(&input.state), 2);
        drop(callback);
        assert!(weak.upgrade().is_none(), "最后一个回调引用释放后不保留绑定");
        assert_eq!(Arc::strong_count(&input.state), 1);
    }

    #[test]
    fn repeated_native_release_reclaims_payload_once_and_final_owner_releases_state() {
        let input = NativeRegionInput::new();
        let owner = token(&input);
        let weak = Rc::downgrade(&owner);
        let data = Box::into_raw(Box::new(owner.clone())) as usize;
        assert_eq!(Rc::strong_count(&owner), 2);
        unsafe { release_data(data, &owner) };
        assert!(owner.released.load(Ordering::Acquire));
        assert_eq!(Rc::strong_count(&owner), 1);
        // 第二次不会读取已释放的 data，也不会重复 Box::from_raw。
        unsafe { release_data(data, &owner) };
        assert_eq!(Rc::strong_count(&owner), 1);
        drop(owner);
        assert!(weak.upgrade().is_none());
        assert_eq!(Arc::strong_count(&input.state), 1);
    }
}
