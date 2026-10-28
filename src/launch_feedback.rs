//! 尽早结束 Windows 的进程启动光标反馈；不表示冻结桌面或 UI 已就绪。
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP,
    },
};

/// 仅在 GUI 主线程创建窗口前调用。先建立队列并投递私有消息，保证 GetMessage
/// 不等待用户输入，也不消费其它消息；Windows 在第一次 GetMessage 后关闭启动反馈。
pub(crate) fn finish() {
    const LAUNCH_ACK: u32 = WM_APP + 0x7f;
    unsafe {
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        if PostThreadMessageW(GetCurrentThreadId(), LAUNCH_ACK, WPARAM(0), LPARAM(0)).is_ok() {
            let _ = GetMessageW(&mut message, None, LAUNCH_ACK, LAUNCH_ACK);
        }
    }
}
