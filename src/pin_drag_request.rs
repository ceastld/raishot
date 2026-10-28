//! 每次拖出独立取消状态；原生回调只更新信号/自有窗口通知，不进入 GPUI 借用。
use crate::Result;
use std::{
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    thread::JoinHandle,
    time::Duration,
};
use windows::Win32::{
    Foundation::*,
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const PREPARING: u8 = 0;
const HANDOFF: u8 = 1;
const OLE: u8 = 2;
const CANCELLED: u8 = 3;
const FINISHED: u8 = 4;
static NEXT_SUBCLASS: AtomicUsize = AtomicUsize::new(0x5253_6000);
static HANDOFF_MESSAGE: OnceLock<u32> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct DragTicket(Arc<Signal>);
struct Signal {
    phase: AtomicU8,
    ole_thread: AtomicU32,
    released: AtomicBool,
    owner: usize,
    id: usize,
    handoff_message: u32,
    wake: Condvar,
    wait: Mutex<()>,
}
impl DragTicket {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new(0, 0, 0)
    }
    fn new(owner: usize, id: usize, handoff_message: u32) -> Self {
        Self(Arc::new(Signal {
            phase: AtomicU8::new(PREPARING),
            ole_thread: AtomicU32::new(0),
            released: AtomicBool::new(false),
            owner,
            id,
            handoff_message,
            wake: Condvar::new(),
            wait: Mutex::new(()),
        }))
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.0.phase.load(Ordering::Acquire) == CANCELLED
    }
    pub(crate) fn ole_active(&self) -> bool {
        self.0.phase.load(Ordering::Acquire) == OLE
            && self.0.ole_thread.load(Ordering::Acquire) != 0
    }
    pub(crate) fn cancel(&self) {
        self.0.phase.store(CANCELLED, Ordering::Release);
        self.wake_ole();
        self.0.wake.notify_all();
    }
    pub(crate) fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// 只允许当前请求的 STA 接管；取消是不可逆状态，重新按键不能恢复。
    pub(crate) fn arm_ole(&self, thread: u32) -> bool {
        if thread == 0
            || self
                .0
                .phase
                .compare_exchange(PREPARING, HANDOFF, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        self.0.ole_thread.store(thread, Ordering::Release);
        true
    }
    /// 只在 OLE 会话内运行。窗口关闭后仍能唤醒尚未收到首个 Query 的 OLE 循环。
    pub(crate) fn watch_ole(&self) -> std::io::Result<OleWatch> {
        if self.0.ole_thread.load(Ordering::Acquire) == 0 {
            return Err(std::io::Error::other("拖出线程尚未接管"));
        }
        let ticket = self.clone();
        let worker = std::thread::Builder::new()
            .name("pin-drag-cancel".into())
            .spawn(move || {
                while ticket.0.ole_thread.load(Ordering::Acquire) != 0 {
                    ticket.poll_ole_capture();
                    let lock = ticket.0.wait.lock().unwrap_or_else(|e| e.into_inner());
                    if ticket.0.ole_thread.load(Ordering::Acquire) != 0 {
                        drop(ticket.0.wake.wait_timeout(lock, Duration::from_millis(20)));
                    }
                }
            })?;
        Ok(OleWatch {
            ticket: self.clone(),
            worker: Some(worker),
        })
    }
    /// OLE 与 UI 可能属于不同输入队列，不能仅等 UI 的 WM_CAPTURECHANGED。
    pub(crate) fn observe_ole_capture(&self) -> bool {
        let thread = unsafe { GetCurrentThreadId() };
        let capture = unsafe { GetCapture() };
        self.record_ole_capture(capture, thread)
    }
    fn poll_ole_capture(&self) {
        let thread = self.0.ole_thread.load(Ordering::Acquire);
        if let Some(capture) = thread_capture(thread) {
            self.record_ole_capture(capture, thread);
        }
    }
    fn record_ole_capture(&self, capture: HWND, thread: u32) -> bool {
        if thread != self.0.ole_thread.load(Ordering::Acquire)
            || !owned_ole_capture(capture, thread)
        {
            return false;
        }
        if self
            .0
            .phase
            .compare_exchange(HANDOFF, OLE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && self.0.owner != 0
        {
            // 只投递，不跨线程释放捕获；消息含请求 ID，迟到消息不能释放新手势。
            let posted = unsafe {
                PostMessageW(
                    Some(HWND(self.0.owner as *mut _)),
                    self.0.handoff_message,
                    WPARAM(self.0.id),
                    LPARAM(0),
                )
            };
            if posted.is_err() {
                self.cancel();
            }
        }
        if self.cancelled() {
            self.wake_ole();
        }
        self.ole_active()
    }
    pub(crate) fn clear_ole_capture(&self) {
        self.0.ole_thread.store(0, Ordering::Release);
        // 保留取消锁存；已结束的成功/普通取消会话也不能接受迟到交接。
        for phase in [HANDOFF, OLE] {
            let _ =
                self.0
                    .phase
                    .compare_exchange(phase, FINISHED, Ordering::AcqRel, Ordering::Acquire);
        }
        self.0.wake.notify_all();
    }
    fn accepts_handoff(&self, message: u32, request_id: usize) -> bool {
        message == self.0.handoff_message && request_id == self.0.id && self.ole_active()
    }
    fn wake_ole(&self) {
        let thread = self.0.ole_thread.load(Ordering::Acquire);
        // 首个 Query 前没有缓存 HWND。查询真实捕获，且拒绝结束后的线程 ID / HWND。
        if let Some(capture) = thread_capture(thread) {
            // 原子取消本身不会唤醒 OLE 模态循环；只向本请求的自有窗口投递 Esc。
            unsafe {
                let _ = PostMessageW(
                    Some(capture),
                    WM_KEYDOWN,
                    WPARAM(usize::from(VK_ESCAPE.0)),
                    LPARAM(0),
                );
            }
        }
    }
    fn capture_changed(&self, next_thread: Option<u32>) {
        let phase = self.0.phase.load(Ordering::Acquire);
        if phase == HANDOFF
            && next_thread.is_some_and(|thread| {
                thread != 0 && thread == self.0.ole_thread.load(Ordering::Acquire)
            })
        {
            let _ =
                self.0
                    .phase
                    .compare_exchange(HANDOFF, OLE, Ordering::AcqRel, Ordering::Acquire);
        } else if phase != OLE {
            self.cancel();
        }
    }
    fn message(&self, message: u32, wparam: WPARAM) {
        match message {
            // OLE 接管后，正常放置的鼠标消息进入其捕获窗口；宿主收到的松开
            // 仍可能是接管前排队的旧事件，必须使原请求失效。
            WM_LBUTTONUP | WM_NCLBUTTONUP | WM_LBUTTONDOWN | WM_LBUTTONDBLCLK
            | WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK | WM_RBUTTONDOWN | WM_CANCELMODE | WM_CLOSE
            | WM_NCDESTROY => self.cancel(),
            WM_KEYDOWN | WM_SYSKEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => self.cancel(),
            WM_KILLFOCUS if self.0.phase.load(Ordering::Acquire) != OLE => self.cancel(),
            WM_ACTIVATE
                if wparam.0 & 0xffff == WA_INACTIVE as usize
                    && self.0.phase.load(Ordering::Acquire) != OLE =>
            {
                self.cancel()
            }
            WM_MOUSEMOVE if wparam.0 & 1 == 0 && self.0.phase.load(Ordering::Acquire) != OLE => {
                self.cancel()
            }
            _ => {}
        }
    }
}
pub(crate) struct OleWatch {
    ticket: DragTicket,
    worker: Option<JoinHandle<()>>,
}
impl Drop for OleWatch {
    fn drop(&mut self) {
        self.ticket.clear_ole_capture();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct SubclassData {
    ticket: DragTicket,
}
pub(crate) struct NativeDragRequest {
    hwnd: usize,
    id: usize,
    data: usize,
    ticket: DragTicket,
    // 安装、移除和捕获释放只能在 HWND 所属线程发生。
    _ui_thread: PhantomData<Rc<()>>,
}
impl NativeDragRequest {
    pub(crate) fn begin(hwnd: HWND) -> Result<Self> {
        if unsafe { GetCapture() } != hwnd {
            return Err("拖出手势已取消".into());
        }
        let handoff_message = *HANDOFF_MESSAGE.get_or_init(|| unsafe {
            RegisterWindowMessageW(windows::core::w!("RustShot.Pin.DragHandoff.v1"))
        });
        if handoff_message == 0 {
            return Err("无法注册贴图拖出交接通知".into());
        }
        let id = NEXT_SUBCLASS.fetch_add(1, Ordering::Relaxed);
        let ticket = DragTicket::new(hwnd.0 as usize, id, handoff_message);
        let data = Box::into_raw(Box::new(SubclassData {
            ticket: ticket.clone(),
        })) as usize;
        if !unsafe { SetWindowSubclass(hwnd, Some(request_proc), id, data) }.as_bool() {
            unsafe { drop(Box::from_raw(data as *mut SubclassData)) };
            return Err("无法跟踪贴图拖出手势".into());
        }
        Ok(Self {
            hwnd: hwnd.0 as usize,
            id,
            data,
            ticket,
            _ui_thread: PhantomData,
        })
    }
    pub(crate) fn ticket(&self) -> DragTicket {
        self.ticket.clone()
    }
}
fn owned_ole_capture(hwnd: HWND, thread: u32) -> bool {
    if hwnd.is_invalid() || thread == 0 {
        return false;
    }
    let mut process = 0;
    unsafe {
        IsWindow(Some(hwnd)).as_bool()
            && GetWindowThreadProcessId(hwnd, Some(&mut process)) == thread
            && process == GetCurrentProcessId()
    }
}
fn thread_capture(thread: u32) -> Option<HWND> {
    if thread == 0 {
        return None;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    (unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok()
        && owned_ole_capture(info.hwndCapture, thread))
    .then_some(info.hwndCapture)
}
unsafe fn owned(hwnd: HWND, id: usize, expected: usize) -> bool {
    let mut actual = 0;
    unsafe { GetWindowSubclass(hwnd, Some(request_proc), id, Some(&mut actual)) }.as_bool()
        && actual == expected
}
unsafe fn release(data: usize, ticket: &DragTicket) {
    if !ticket.0.released.swap(true, Ordering::AcqRel) {
        unsafe { drop(Box::from_raw(data as *mut SubclassData)) };
    }
}
impl Drop for NativeDragRequest {
    fn drop(&mut self) {
        self.ticket.cancel();
        let hwnd = HWND(self.hwnd as *mut _);
        if !self.ticket.0.released.load(Ordering::Acquire) {
            unsafe {
                if !owned(hwnd, self.id, self.data)
                    || RemoveWindowSubclass(hwnd, Some(request_proc), self.id).as_bool()
                {
                    release(self.data, &self.ticket);
                }
            }
        }
        unsafe {
            if GetCapture() == hwnd {
                let _ = ReleaseCapture();
            }
        }
    }
}
unsafe extern "system" fn request_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let ticket = unsafe { &*(data as *const SubclassData) }.ticket.clone();
    if message == ticket.0.handoff_message && wparam.0 == id {
        if ticket.accepts_handoff(message, wparam.0) && unsafe { GetCapture() } == hwnd {
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        return LRESULT(0);
    }
    ticket.message(message, wparam);
    if message == WM_CAPTURECHANGED {
        let next = HWND(lparam.0 as *mut _);
        let thread = (!next.is_invalid()).then(|| unsafe { GetWindowThreadProcessId(next, None) });
        ticket.capture_changed(thread);
    }
    if message == WM_NCDESTROY {
        let was_owned = unsafe { owned(hwnd, id, data) };
        if was_owned {
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(request_proc), id) };
        }
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        if was_owned {
            unsafe { release(data, &ticket) };
        }
        return result;
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pin_export_detail_cancel_cannot_be_revived_by_repress_or_new_request() {
        for event in [
            WM_LBUTTONUP,
            WM_LBUTTONDOWN,
            WM_CANCELMODE,
            WM_CLOSE,
            WM_NCDESTROY,
        ] {
            let old = DragTicket::for_test();
            old.message(event, WPARAM(0));
            old.message(WM_LBUTTONDOWN, WPARAM(1));
            assert!(old.cancelled());
            assert!(!old.arm_ole(72));
            let new = DragTicket::for_test();
            assert!(!old.same_request(&new));
            assert!(!new.cancelled());
        }
    }
    #[test]
    fn pin_export_detail_capture_transfer_only_accepts_the_requests_sta() {
        let external = DragTicket::for_test();
        external.capture_changed(Some(42));
        assert!(external.cancelled());
        let mismatched = DragTicket::for_test();
        assert!(mismatched.arm_ole(42));
        mismatched.capture_changed(Some(41));
        assert!(mismatched.cancelled());
        let correct = DragTicket::for_test();
        assert!(correct.arm_ole(42));
        correct.capture_changed(Some(42));
        assert_eq!(correct.0.phase.load(Ordering::Acquire), OLE);
        assert!(!correct.cancelled());
        // 接管前已排队的宿主松开仍须取消，不能变成一次正常放置。
        correct.message(WM_LBUTTONUP, WPARAM(0));
        assert!(correct.cancelled());
    }
    #[test]
    fn pin_export_detail_escape_and_focus_loss_cancel_before_ole() {
        for (event, value) in [
            (WM_KEYDOWN, usize::from(VK_ESCAPE.0)),
            (WM_KILLFOCUS, 0),
            (WM_ACTIVATE, WA_INACTIVE as usize),
            (WM_MOUSEMOVE, 0),
        ] {
            let ticket = DragTicket::for_test();
            ticket.message(event, WPARAM(value));
            assert!(ticket.cancelled());
        }
    }
    #[test]
    fn pin_export_detail_late_handoff_does_not_release_a_new_or_cancelled_request() {
        let old = DragTicket::new(0, 24, 0xc512);
        let current = DragTicket::new(0, 25, 0xc512);
        for ticket in [&old, &current] {
            assert!(ticket.arm_ole(42));
            ticket.capture_changed(Some(42));
        }
        assert!(old.accepts_handoff(0xc512, 24));
        assert!(!current.accepts_handoff(0xc512, 24));
        assert!(!current.accepts_handoff(0xc511, 25));
        assert!(current.accepts_handoff(0xc512, 25));
        current.cancel();
        assert!(!current.accepts_handoff(0xc512, 25));
        old.clear_ole_capture();
        assert_eq!(old.0.ole_thread.load(Ordering::Acquire), 0);
        assert!(!old.accepts_handoff(0xc512, 24));
        assert!(!old.arm_ole(43));
    }
}
