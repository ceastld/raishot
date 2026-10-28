//! 截图会话的唯一内核句柄与原生取消 / GPUI 开窗之间的交接门。
//!
//! 捕获互斥体以命名对象是否存在作为 Busy 标记，不使用 mutex 的线程所有权。
//! 句柄只由 CaptureLock 强持有；原生线程的 token 只持有 Weak，不能延长会话。
use std::{
    os::windows::io::{FromRawHandle, OwnedHandle},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex, Weak,
    },
};
use windows::Win32::Foundation::HANDLE;

const NATIVE_ACTIVE: u8 = 0;
const GPUI_OPENING: u8 = 1;
const CANCELLED_CLOSED: u8 = 2;

struct SharedLock {
    state: AtomicU8,
    cancelled: AtomicBool,
    handle: Mutex<Option<OwnedHandle>>,
    #[cfg(test)]
    closes: std::sync::atomic::AtomicUsize,
}

impl SharedLock {
    fn release(&self) {
        // Drop 与 native cancel 可能并行，但只能取走一次原句柄。
        // 不按名称重新打开对象，也不复制 raw HANDLE 给其它析构路径。
        let mut slot = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 持有 guard 到真正 CloseHandle 完成；并行 release 不能在另一个线程
        // 仅 take 了句柄、尚未关闭命名对象时就返回“已释放”。
        if let Some(handle) = slot.take() {
            drop(handle);
            #[cfg(test)]
            self.closes.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 强 owner 不可 Clone；转入 CaptureView 后仍由该 view 的退出路径释放。
pub(crate) struct CaptureLock {
    shared: Arc<SharedLock>,
}

impl CaptureLock {
    /// 收走调用方新取得的唯一有效 HANDLE；调用方此后不得 CloseHandle 或重复包装它。
    pub(crate) fn new(handle: HANDLE) -> Self {
        assert!(!handle.is_invalid(), "截图锁需要有效的唯一句柄");
        // CreateMutex 返回的内核句柄可跨线程关闭；OwnedHandle 只在 release 的唯一赢家析构。
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Self {
            shared: Arc::new(SharedLock {
                state: AtomicU8::new(NATIVE_ACTIVE),
                cancelled: AtomicBool::new(false),
                handle: Mutex::new(Some(handle)),
                #[cfg(test)]
                closes: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    pub(crate) fn native_cancel_token(&self) -> NativeCaptureCancelToken {
        NativeCaptureCancelToken {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// 紧贴 GPUI 截图窗口创建之前调用。只有首次 CAS 成功才准许进入 open_window。
    /// 不可在昂贵的 Application::new 之前 claim，否则原生早取消仍须等待其完成。
    pub(crate) fn claim_gpui_open(&self) -> bool {
        !self.is_cancelled()
            && self
                .shared
                .state
                .compare_exchange(
                    NATIVE_ACTIVE,
                    GPUI_OPENING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.shared.cancelled.load(Ordering::Acquire)
    }
}

impl Drop for CaptureLock {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.state.store(CANCELLED_CLOSED, Ordering::Release);
        self.shared.release();
    }
}

#[derive(Clone)]
pub(crate) struct NativeCaptureCancelToken {
    shared: Weak<SharedLock>,
}

impl NativeCaptureCancelToken {
    /// 只观察现有 owner 存活及取消位；GPUI_OPENING 不等于取消。
    /// Native target/stop/finish/handed_off 等阶段门由创建线程另行核对。
    pub(crate) fn session_is_live(&self) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|shared| !shared.cancelled.load(Ordering::Acquire))
    }
    /// 只在终态 Native Cancel 且创建线程已核验原窗口成功关闭之后调用。
    /// 普通 dismiss、成功 handoff、关闭失败或仅 snapshot.cancelled 均不满足合同。
    /// 返回 true 表示原生取消赢得门并已回收原句柄；GPUI 已赢得门时仅标取消，
    /// 返回 false 并保留锁，由 GPUI 正常关闭或原 owner 退出负责释放。
    pub(crate) fn native_cancelled_after_close(&self) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        shared.cancelled.store(true, Ordering::Release);
        if shared
            .state
            .compare_exchange(
                NATIVE_ACTIVE,
                CANCELLED_CLOSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        shared.release();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{atomic::AtomicU64, Barrier},
        time::{SystemTime, UNIX_EPOCH},
    };
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{
                CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS,
                ERROR_FILE_NOT_FOUND, ERROR_SUCCESS,
            },
            System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS},
        },
    };

    #[test]
    fn read_only_token_live_check_preserves_claim_state_and_does_not_extend_owner() {
        // 纯内存 owner；无 HANDLE、Mutex 名称、Window 或 OS API。
        let shared = Arc::new(SharedLock {
            state: AtomicU8::new(NATIVE_ACTIVE),
            cancelled: AtomicBool::new(false),
            handle: Mutex::new(None),
            closes: std::sync::atomic::AtomicUsize::new(0),
        });
        let token = NativeCaptureCancelToken {
            shared: Arc::downgrade(&shared),
        };
        assert!(token.session_is_live());
        assert_eq!(Arc::strong_count(&shared), 1);
        assert_eq!(shared.state.load(Ordering::Acquire), NATIVE_ACTIVE);
        shared.state.store(GPUI_OPENING, Ordering::Release);
        assert!(token.session_is_live());
        assert_eq!(shared.state.load(Ordering::Acquire), GPUI_OPENING);
        shared.cancelled.store(true, Ordering::Release);
        assert!(!token.session_is_live());
        assert_eq!(shared.closes.load(Ordering::Relaxed), 0);
        drop(shared);
        assert!(!token.session_is_live());
    }

    // 仅使用自有随机名字；既不打开生产 CaptureMutex，也不创建窗口或读取桌面。
    struct PrivateMutex(Vec<u16>);
    impl PrivateMutex {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(
                format!(
                    "Local\\RustShot.QA.CaptureCancel.{}.{}.{}",
                    std::process::id(),
                    nonce,
                    NEXT.fetch_add(1, Ordering::Relaxed)
                )
                .encode_utf16()
                .chain(Some(0))
                .collect(),
            )
        }
        fn create(&self) -> CaptureLock {
            unsafe {
                SetLastError(ERROR_SUCCESS);
                let handle = CreateMutexW(None, false, PCWSTR(self.0.as_ptr())).unwrap();
                let existing = GetLastError() == ERROR_ALREADY_EXISTS;
                assert!(!existing, "旧会话的命名对象尚未释放");
                CaptureLock::new(handle)
            }
        }
        fn present(&self) -> bool {
            unsafe {
                match OpenMutexW(
                    SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
                    false,
                    PCWSTR(self.0.as_ptr()),
                ) {
                    Ok(handle) => {
                        CloseHandle(handle).unwrap();
                        true
                    }
                    Err(error) => {
                        assert_eq!(
                            error.code(),
                            windows::core::HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
                        );
                        false
                    }
                }
            }
        }
    }

    #[test]
    fn native_cancel_allows_next_named_generation_while_old_initialization_is_alive() {
        let name = PrivateMutex::new();
        let old = name.create();
        let cancel = old.native_cancel_token();
        assert!(name.present());
        assert!(cancel.native_cancelled_after_close());
        assert!(old.is_cancelled());
        assert!(!name.present());
        // old 仍活着，代表不可取消的旧 Application 初始化尚未返回。
        let next = name.create();
        assert!(name.present());
        assert!(!old.claim_gpui_open());
        drop(old);
        assert!(name.present());
        drop(next);
        assert!(!name.present());
    }

    #[test]
    fn late_gpui_open_is_rejected_after_native_cancel() {
        let name = PrivateMutex::new();
        let lock = name.create();
        let token = lock.native_cancel_token();
        assert!(token.native_cancelled_after_close());
        assert!(!lock.claim_gpui_open());
        assert!(!lock.claim_gpui_open());
        assert_eq!(lock.shared.state.load(Ordering::Acquire), CANCELLED_CLOSED);
        assert!(!name.present());
    }

    #[test]
    fn gpui_open_winner_keeps_busy_until_the_view_owner_drops() {
        let name = PrivateMutex::new();
        let lock = name.create();
        let token = lock.native_cancel_token();
        assert!(lock.claim_gpui_open());
        assert!(!lock.claim_gpui_open());
        assert!(!token.native_cancelled_after_close());
        assert!(lock.is_cancelled());
        assert!(name.present());
        assert_eq!(lock.shared.closes.load(Ordering::Relaxed), 0);
        drop(lock);
        assert!(!name.present());
    }

    #[test]
    fn old_drop_and_late_native_token_do_not_close_the_next_generations_handle() {
        let name = PrivateMutex::new();
        let old = name.create();
        let token = old.native_cancel_token();
        let old_shared = Arc::clone(&old.shared); // 仅测试记录 close 次数，不是产品 owner。
        assert!(token.native_cancelled_after_close());
        let next = name.create();
        assert!(!token.native_cancelled_after_close());
        assert!(name.present());
        drop(old);
        assert!(name.present());
        assert!(!token.native_cancelled_after_close());
        assert_eq!(old_shared.closes.load(Ordering::Relaxed), 1);
        drop(old_shared);
        assert!(!token.native_cancelled_after_close());
        assert!(name.present());
        drop(next);
        assert!(!name.present());
    }

    #[test]
    fn weak_native_token_does_not_extend_the_capture_lifetime() {
        let name = PrivateMutex::new();
        let lock = name.create();
        let token = lock.native_cancel_token();
        let duplicate_token = token.clone();
        assert_eq!(Arc::strong_count(&lock.shared), 1);
        drop(lock);
        assert!(token.shared.upgrade().is_none());
        assert!(!token.native_cancelled_after_close());
        assert!(!duplicate_token.native_cancelled_after_close());
        assert!(!name.present());
    }

    #[test]
    fn concurrent_native_cancel_and_owner_drop_close_exactly_once() {
        for _ in 0..64 {
            let name = PrivateMutex::new();
            let lock = name.create();
            let shared = Arc::clone(&lock.shared);
            let barrier = Arc::new(Barrier::new(3));
            let token = lock.native_cancel_token();
            let cancel_barrier = Arc::clone(&barrier);
            let cancel = std::thread::spawn(move || {
                cancel_barrier.wait();
                token.native_cancelled_after_close()
            });
            let drop_barrier = Arc::clone(&barrier);
            let owner = std::thread::spawn(move || {
                drop_barrier.wait();
                drop(lock);
            });
            barrier.wait();
            let _ = cancel.join().unwrap();
            owner.join().unwrap();
            assert_eq!(shared.closes.load(Ordering::Relaxed), 1);
            assert!(!name.present());
            let next = name.create();
            drop(shared);
            assert!(name.present());
            drop(next);
        }
    }

    #[test]
    fn native_cancel_and_gpui_open_have_one_linearized_winner() {
        for _ in 0..64 {
            let name = PrivateMutex::new();
            let lock = name.create();
            let token = lock.native_cancel_token();
            let barrier = Barrier::new(3);
            let (gpui_won, native_won) = std::thread::scope(|scope| {
                let gpui = scope.spawn(|| {
                    barrier.wait();
                    lock.claim_gpui_open()
                });
                let native = scope.spawn(|| {
                    barrier.wait();
                    token.native_cancelled_after_close()
                });
                barrier.wait();
                (gpui.join().unwrap(), native.join().unwrap())
            });
            assert_ne!(gpui_won, native_won);
            assert!(lock.is_cancelled());
            assert_eq!(name.present(), gpui_won);
            assert_eq!(
                lock.shared.closes.load(Ordering::Relaxed),
                usize::from(native_won)
            );
            drop(lock);
            assert!(!name.present());
        }
    }

    #[test]
    fn owner_error_exit_without_native_cancel_releases_the_named_guard() {
        let name = PrivateMutex::new();
        let lock = name.create();
        let token = lock.native_cancel_token();
        drop(lock);
        assert!(!name.present());
        assert!(!token.native_cancelled_after_close());
        let next = name.create();
        assert!(next.claim_gpui_open());
        drop(next);
        assert!(!name.present());
    }
}
