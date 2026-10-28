//! 仅设置窗的热键录制适配；窗口回调只收集 VK，不借用 GPUI App 或视图。
use crate::{
    hotkey::{HotkeySpec, ALT, CTRL, SHIFT, WIN},
    Result,
};
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use windows::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const QUEUE_LIMIT: usize = 8;
static NEXT_SUBCLASS: AtomicUsize = AtomicUsize::new(0x5253_1000);

struct RecorderState {
    active: AtomicBool,
    released: AtomicBool,
    queue: Mutex<VecDeque<HotkeySpec>>,
    snapshot_down: AtomicBool,
}
impl RecorderState {
    fn disable(&self) {
        self.active.store(false, Ordering::Release);
        self.queue.lock().clear();
        self.snapshot_down.store(false, Ordering::Release);
    }
    fn push(&self, key: u32, modifiers: u32) {
        let mut queue = self.queue.lock();
        if queue.len() < QUEUE_LIMIT {
            queue.push_back(HotkeySpec { key, modifiers });
        }
    }
}
struct SubclassData {
    state: Arc<RecorderState>,
}

pub(crate) struct NativeHotkeyRecorder {
    hwnd: usize,
    subclass_id: usize,
    data: usize,
    state: Arc<RecorderState>,
    _ui_thread: PhantomData<Rc<()>>,
}
impl NativeHotkeyRecorder {
    /// 安装/移除及窗口回调都运行在 HWND 所属的 GPUI UI 线程。
    pub(crate) fn install(hwnd: HWND) -> Result<Self> {
        let state = Arc::new(RecorderState {
            active: AtomicBool::new(false),
            released: AtomicBool::new(false),
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_LIMIT)),
            snapshot_down: AtomicBool::new(false),
        });
        let data = Box::into_raw(Box::new(SubclassData {
            state: state.clone(),
        })) as usize;
        let subclass_id = NEXT_SUBCLASS.fetch_add(1, Ordering::Relaxed);
        if !unsafe { SetWindowSubclass(hwnd, Some(recorder_proc), subclass_id, data) }.as_bool() {
            unsafe {
                drop(Box::from_raw(data as *mut SubclassData));
            }
            return Err(tr!(
                "无法安装设置窗口按键录制适配",
                "Cannot install key recording for the settings window"
            )
            .into());
        }
        Ok(Self {
            hwnd: hwnd.0 as usize,
            subclass_id,
            data,
            state,
            _ui_thread: PhantomData,
        })
    }
    pub(crate) fn activate(&self) {
        self.state.queue.lock().clear();
        self.state.snapshot_down.store(false, Ordering::Release);
        self.state.active.store(true, Ordering::Release);
    }
    pub(crate) fn deactivate(&self) {
        self.state.disable();
    }
    pub(crate) fn is_active(&self) -> bool {
        self.state.active.load(Ordering::Acquire) && !self.state.released.load(Ordering::Acquire)
    }
    pub(crate) fn take_keys(&self) -> Vec<HotkeySpec> {
        self.state.queue.lock().drain(..).collect()
    }
}

unsafe fn owned_reference(hwnd: HWND, id: usize, expected: usize) -> bool {
    let mut actual = 0;
    unsafe { GetWindowSubclass(hwnd, Some(recorder_proc), id, Some(&mut actual)) }.as_bool()
        && actual == expected
}

unsafe fn release_data(data: usize, state: &RecorderState) {
    if !state.released.swap(true, Ordering::AcqRel) {
        unsafe {
            drop(Box::from_raw(data as *mut SubclassData));
        }
    }
}
impl Drop for NativeHotkeyRecorder {
    fn drop(&mut self) {
        self.state.disable();
        if self.state.released.load(Ordering::Acquire) {
            return;
        }
        let hwnd = HWND(self.hwnd as *mut _);
        unsafe {
            if owned_reference(hwnd, self.subclass_id, self.data) {
                if RemoveWindowSubclass(hwnd, Some(recorder_proc), self.subclass_id).as_bool() {
                    release_data(self.data, &self.state);
                }
            } else {
                // 该 ID 已经不指向我们，回调无法再访问此 Box；销毁路径通过 released 避免二次释放。
                release_data(self.data, &self.state);
            }
        }
    }
}

fn modifier_key(key: u32) -> bool {
    matches!(key, 0x10..=0x12 | 0x5b..=0x5c | 0xa0..=0xa5)
}
unsafe fn modifier_bits(lparam: LPARAM) -> u32 {
    let pressed = |key: VIRTUAL_KEY| unsafe { GetKeyState(i32::from(key.0)) } < 0;
    (u32::from(pressed(VK_CONTROL)) * CTRL)
        | (u32::from(pressed(VK_MENU) || (lparam.0 as usize & (1 << 29)) != 0) * ALT)
        | (u32::from(pressed(VK_SHIFT)) * SHIFT)
        | (u32::from(pressed(VK_LWIN) || pressed(VK_RWIN)) * WIN)
}

unsafe extern "system" fn recorder_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    data: usize,
) -> LRESULT {
    let state = unsafe { &*(data as *const SubclassData) }.state.clone();
    if message == WM_NCDESTROY {
        state.disable();
        let owned = unsafe { owned_reference(hwnd, subclass_id, data) };
        if owned {
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(recorder_proc), subclass_id) };
        }
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        // NCDESTROY 是该 HWND 的最后一条销毁通知，默认链返回后无回调再使用此数据。
        if owned {
            unsafe {
                release_data(data, &state);
            }
        }
        return result;
    }
    if message == WM_KILLFOCUS
        || message == WM_ACTIVATE && wparam.0 & 0xffff == WA_INACTIVE as usize
    {
        state.disable();
    }
    if !state.active.load(Ordering::Acquire) || unsafe { GetForegroundWindow() } != hwnd {
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    }
    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let key = wparam.0 as u32;
            if key == u32::from(VK_SNAPSHOT.0) {
                state.snapshot_down.store(true, Ordering::Release);
            }
            if lparam.0 as usize & (1 << 30) == 0 && !modifier_key(key) {
                state.push(key, unsafe { modifier_bits(lparam) });
            }
            LRESULT(0)
        }
        WM_KEYUP | WM_SYSKEYUP => {
            // PrintScreen 在部分输入路径只有释放事件；收到按下时则不重复入队。
            if wparam.0 == usize::from(VK_SNAPSHOT.0)
                && !state.snapshot_down.swap(false, Ordering::AcqRel)
            {
                state.push(u32::from(VK_SNAPSHOT.0), unsafe { modifier_bits(lparam) });
            }
            LRESULT(0)
        }
        WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR => LRESULT(0),
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}
