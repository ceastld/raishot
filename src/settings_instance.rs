//! 设置窗口的跨进程发现与短期创建锁；窗口本身不长期占用创建锁。
use crate::Result;
use std::{marker::PhantomData, rc::Rc};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{
            CloseHandle, HANDLE, HWND, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
        UI::WindowsAndMessaging::{
            AllowSetForegroundWindow, EnumWindows, GetPropW, GetWindowThreadProcessId, IsIconic,
            RemovePropW, SetForegroundWindow, SetPropW, SetWindowPos, ShowWindowAsync,
            HWND_NOTOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE,
        },
    },
};

const CREATE_TIMEOUT_MS: u32 = 2000;

fn property_name(scope: u64) -> Vec<u16> {
    format!("RustShot.Settings.v1.{scope:016x}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

/// 仅串行化“查找已有窗口／创建并注册”，且必须在取得锁的线程释放。
pub(super) struct CreationLock {
    handle: HANDLE,
    _ui_thread: PhantomData<Rc<()>>,
}
impl CreationLock {
    pub(super) fn acquire() -> Result<Self> {
        Self::for_scope(crate::pin_workspace::scope(), CREATE_TIMEOUT_MS)
    }

    fn for_scope(scope: u64, timeout_ms: u32) -> Result<Self> {
        let name: Vec<u16> = format!("Local\\RustShot.Settings.Create.v1.{scope:016x}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr()))? };
        let status = unsafe { WaitForSingleObject(handle, timeout_ms) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            return Ok(Self {
                handle,
                _ui_thread: PhantomData,
            });
        }
        // 先保留等待失败的错误，CloseHandle 可能改变 GetLastError。
        let error: Box<dyn std::error::Error> = if status == WAIT_TIMEOUT {
            tr!(
                "设置窗口正在打开，请稍后重试。",
                "Settings are opening. Please try again shortly."
            )
            .into()
        } else {
            windows::core::Error::from_win32().into()
        };
        unsafe {
            let _ = CloseHandle(handle);
        }
        Err(error)
    }
}
impl Drop for CreationLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

fn find_existing(scope: u64) -> Result<Option<HWND>> {
    struct Search {
        property: Vec<u16>,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        if search.found.is_none()
            && !unsafe { GetPropW(hwnd, PCWSTR(search.property.as_ptr())) }.is_invalid()
        {
            search.found = Some(hwnd);
        }
        // 保持枚举成功返回，避免把提前结束误判为 Win32 错误。
        windows::core::BOOL(1)
    }
    let mut search = Search {
        property: property_name(scope),
        found: None,
    };
    unsafe {
        EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize))?;
    }
    Ok(search.found)
}

const PAGE_REQUEST: PCWSTR = windows::core::w!("RustShot.Main.PageRequest");

pub(super) fn activate_existing(page: Option<super::MainPage>) -> Result<bool> {
    let scope = crate::pin_workspace::scope();
    if let Some(hwnd) = find_existing(scope)? {
        let property = property_name(scope);
        if unsafe { GetPropW(hwnd, PCWSTR(property.as_ptr())) }.is_invalid() {
            return Ok(false);
        }
        if let Some(page) = page {
            // 窗口激活时取走请求；属性只是整数，跨进程写入不涉及指针。
            unsafe {
                let _ = SetPropW(hwnd, PAGE_REQUEST, Some(HANDLE(page.wire() as *mut _)));
            }
        }
        activate(hwnd);
        // 枚举后窗口可能正好关闭；此时让调用方继续正常创建。
        Ok(!unsafe { GetPropW(hwnd, PCWSTR(property.as_ptr())) }.is_invalid())
    } else {
        Ok(false)
    }
}

/// 取走其它进程写入的切页请求。
pub(super) fn take_requested_page(hwnd: HWND) -> Option<super::MainPage> {
    let value = unsafe { RemovePropW(hwnd, PAGE_REQUEST) }.ok()?;
    super::MainPage::from_wire(value.0 as usize)
}

pub(super) fn activate(hwnd: HWND) {
    unsafe {
        let mut process = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process));
        if process != 0 {
            let _ = AllowSetForegroundWindow(process);
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_ASYNCWINDOWPOS,
        );
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 随设置视图释放；原生 HWND 销毁也会自动移除窗口属性。
pub(super) struct WindowRegistration {
    hwnd: HWND,
    property: Vec<u16>,
    token: Box<u8>,
    _ui_thread: PhantomData<Rc<()>>,
}
impl WindowRegistration {
    pub(super) fn new(hwnd: HWND) -> Result<Self> {
        Self::for_scope(hwnd, crate::pin_workspace::scope())
    }

    fn for_scope(hwnd: HWND, scope: u64) -> Result<Self> {
        let property = property_name(scope);
        // 不能只存 HWND：旧视图延迟释放时，系统可能已将同一 HWND 分配给新窗口。
        let token = Box::new(0_u8);
        let marker = HANDLE((&*token as *const u8).cast_mut().cast());
        unsafe {
            SetPropW(hwnd, PCWSTR(property.as_ptr()), Some(marker))?;
        }
        Ok(Self {
            hwnd,
            property,
            token,
            _ui_thread: PhantomData,
        })
    }
}
impl Drop for WindowRegistration {
    fn drop(&mut self) {
        unsafe {
            let marker = (&*self.token as *const u8).cast_mut().cast();
            if GetPropW(self.hwnd, PCWSTR(self.property.as_ptr())).0 == marker {
                let _ = RemovePropW(self.hwnd, PCWSTR(self.property.as_ptr()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    };
    use windows::{
        core::w,
        Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, IsWindowVisible, WINDOW_EX_STYLE, WS_POPUP,
        },
    };

    fn private_scope() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        (u64::from(std::process::id()) << 32) | NEXT.fetch_add(1, Ordering::Relaxed)
    }

    struct HiddenWindow(HWND);
    impl HiddenWindow {
        fn new() -> Self {
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("RustShot settings instance test"),
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
            Self(hwnd)
        }
    }
    impl Drop for HiddenWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    #[test]
    fn settings_instance_registration_is_scoped_and_can_reopen_after_close() {
        let scope = private_scope();
        let window = HiddenWindow::new();
        let registration = WindowRegistration::for_scope(window.0, scope).unwrap();
        assert_eq!(find_existing(scope).unwrap(), Some(window.0));
        assert_eq!(find_existing(private_scope()).unwrap(), None);
        drop(registration);
        assert_eq!(find_existing(scope).unwrap(), None);

        let registration = WindowRegistration::for_scope(window.0, scope).unwrap();
        assert_eq!(find_existing(scope).unwrap(), Some(window.0));
        drop(window);
        assert_eq!(find_existing(scope).unwrap(), None);
        drop(registration);

        let reopened = HiddenWindow::new();
        let _registration = WindowRegistration::for_scope(reopened.0, scope).unwrap();
        assert_eq!(find_existing(scope).unwrap(), Some(reopened.0));
    }

    #[test]
    fn settings_instance_drop_preserves_replaced_window_property() {
        let scope = private_scope();
        let window = HiddenWindow::new();
        let old = WindowRegistration::for_scope(window.0, scope).unwrap();
        let replacement = WindowRegistration::for_scope(window.0, scope).unwrap();
        let property = property_name(scope);
        let marker = unsafe { GetPropW(window.0, PCWSTR(property.as_ptr())) };
        drop(old);
        assert_eq!(
            unsafe { GetPropW(window.0, PCWSTR(property.as_ptr())) },
            marker
        );
        assert_eq!(find_existing(scope).unwrap(), Some(window.0));
        drop(replacement);
        assert_eq!(find_existing(scope).unwrap(), None);
    }

    #[test]
    fn settings_instance_creation_lock_times_out_then_allows_another_owner() {
        let scope = private_scope();
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _lock = CreationLock::for_scope(scope, CREATE_TIMEOUT_MS).unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();
        let blocked = CreationLock::for_scope(scope, 20);
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        assert!(blocked.is_err());
        let _next = CreationLock::for_scope(scope, CREATE_TIMEOUT_MS).unwrap();
    }
}
