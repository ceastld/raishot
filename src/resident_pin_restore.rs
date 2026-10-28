//! 托盘进入消息循环后在后台迁移已有工作区并检查恢复；进程仍由主线程登记。
use std::io;
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};

pub(super) const TIMER: usize = 15;
pub(super) const READY: u32 = WM_APP + 30;
pub(super) const DELAY_MS: u32 = 1000;

#[derive(Default)]
enum Phase {
    #[default]
    Inactive,
    Scheduled,
    Pending,
    Complete,
}

#[derive(Default)]
pub(super) struct Check {
    phase: Phase,
}

impl Check {
    pub(super) fn schedule(&mut self) -> bool {
        if !matches!(self.phase, Phase::Inactive) {
            return false;
        }
        self.phase = Phase::Scheduled;
        true
    }

    pub(super) fn begin(&mut self) -> bool {
        if !matches!(self.phase, Phase::Scheduled) {
            return false;
        }
        self.phase = Phase::Pending;
        true
    }

    /// 只消费本次后台检查的一条回执；重复消息和未启动检查的消息均忽略。
    pub(super) fn complete(&mut self, needed: bool) -> bool {
        if !matches!(self.phase, Phase::Pending) {
            return false;
        }
        self.phase = Phase::Complete;
        needed
    }
}

pub(super) fn start_check(hwnd: HWND) -> io::Result<()> {
    // 不跨线程借用 NativeState，也不经窗口消息传递 Result 或堆指针。
    let window = hwnd.0 as usize;
    std::thread::Builder::new()
        .name("rustshot-pin-restore-check".into())
        .spawn(move || {
            let needed = match crate::pin_workspace::prepare_existing_workspace()
                .and_then(|_| crate::pin_workspace::startup_restore_needed())
            {
                Ok(needed) => needed,
                Err(error) => {
                    eprintln!("贴图工作区后台准备或恢复条件检查失败：{error}");
                    false
                }
            };
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(window as *mut _)),
                    READY,
                    WPARAM(usize::from(needed)),
                    LPARAM(0),
                );
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resident_pin_restore_waits_for_timer_and_consumes_one_ready_result() {
        let mut check = Check::default();
        assert!(!check.complete(true));
        assert!(!check.begin());
        assert!(check.schedule());
        assert!(!check.schedule());
        assert!(!check.complete(true));
        assert!(check.begin());
        assert!(!check.begin());
        assert!(check.complete(true));
        assert!(!check.complete(true));
        assert!(!check.schedule());
    }

    #[test]
    fn resident_pin_restore_empty_or_failed_check_stays_complete() {
        let mut check = Check::default();
        assert!(check.schedule());
        assert!(check.begin());
        assert!(!check.complete(false));
        assert!(!check.complete(true));
        assert!(!check.begin());
    }
}
