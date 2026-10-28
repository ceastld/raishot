//! 主界面请求托盘代启动贴图或编辑进程：由托盘登记子进程，批量隐藏、关闭等操作才能覆盖它们。
//! 参数经 WM_COPYDATA 传递，只接受白名单入口；窗口过程只入队并投递消息，启动在外层消息循环。
use crate::Result;
use std::{
    collections::VecDeque, ffi::OsString, os::windows::process::CommandExt, path::PathBuf,
    process::Command, sync::Mutex,
};
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::DataExchange::COPYDATASTRUCT,
        UI::WindowsAndMessaging::{
            AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW,
            SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_COPYDATA,
        },
    },
};

const MAGIC: usize = 0x5253_5051;
const MAX_BYTES: usize = 64 * 1024;
const MAX_QUEUE: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// 把图片文件贴出（截图历史“贴出”）。
    PinFile(PathBuf),
    /// 按工作区记录恢复一张贴图，含标注与撤销记录。
    PinRecord(String),
    /// 贴出并直接展开工具栏编辑（截图历史“编辑”）。
    PinEditFile(PathBuf),
}
impl Request {
    pub(crate) fn args(&self) -> Vec<OsString> {
        match self {
            Self::PinFile(path) => vec!["--pin-file".into(), path.into()],
            Self::PinRecord(id) => vec!["--pin-record".into(), id.into()],
            Self::PinEditFile(path) => vec!["--pin-edit-file".into(), path.into()],
        }
    }
    fn encode(&self) -> Vec<u8> {
        let (kind, payload) = match self {
            Self::PinFile(path) => ("pin-file", path.to_string_lossy().into_owned()),
            Self::PinRecord(id) => ("pin-record", id.clone()),
            Self::PinEditFile(path) => ("pin-edit-file", path.to_string_lossy().into_owned()),
        };
        format!("{kind}\n{payload}").into_bytes()
    }
    fn decode(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let (kind, payload) = text.split_once('\n')?;
        if payload.is_empty() || payload.contains(['\0', '\n']) {
            return None;
        }
        let path = || {
            let path = PathBuf::from(payload);
            path.is_absolute().then_some(path)
        };
        match kind {
            "pin-file" => path().map(Self::PinFile),
            "pin-edit-file" => path().map(Self::PinEditFile),
            "pin-record" if crate::pin_workspace::valid_pin_id(payload) => {
                Some(Self::PinRecord(payload.to_owned()))
            }
            _ => None,
        }
    }
}

static QUEUE: Mutex<VecDeque<Request>> = Mutex::new(VecDeque::new());

/// 托盘窗口过程调用：识别本协议后入队，并请外层循环启动。
pub(crate) unsafe fn accept(owner: HWND, data: &COPYDATASTRUCT) -> bool {
    let size = data.cbData as usize;
    if data.dwData != MAGIC || size > MAX_BYTES || data.lpData.is_null() {
        return false;
    }
    let bytes = std::slice::from_raw_parts(data.lpData.cast::<u8>(), size);
    let Some(request) = Request::decode(bytes) else {
        return false;
    };
    let Ok(mut queue) = QUEUE.lock() else {
        return false;
    };
    if queue.len() >= MAX_QUEUE {
        return false;
    }
    queue.push_back(request);
    drop(queue);
    PostMessageW(
        Some(owner),
        crate::ui::PIN_MESSAGE,
        WPARAM(crate::tray::PIN_REQUEST as usize),
        LPARAM(0),
    )
    .is_ok()
}

pub(crate) fn take() -> Vec<Request> {
    QUEUE
        .lock()
        .map(|mut queue| queue.drain(..).collect())
        .unwrap_or_default()
}

/// 有托盘时交给托盘启动；没有托盘（独立运行）时直接启动。
pub(crate) fn send(request: Request) -> Result<()> {
    unsafe {
        if let Ok(tray) = FindWindowW(w!("RustShot.MVP.Window"), None) {
            if !tray.0.is_null() {
                let mut process = 0;
                GetWindowThreadProcessId(tray, Some(&mut process));
                if process != 0 {
                    let _ = AllowSetForegroundWindow(process);
                }
                let bytes = request.encode();
                let data = COPYDATASTRUCT {
                    dwData: MAGIC,
                    cbData: bytes.len() as u32,
                    lpData: bytes.as_ptr() as *mut _,
                };
                let mut result = 0;
                SendMessageTimeoutW(
                    tray,
                    WM_COPYDATA,
                    WPARAM(0),
                    LPARAM(&data as *const COPYDATASTRUCT as isize),
                    SMTO_ABORTIFHUNG,
                    2_000,
                    Some(&mut result),
                );
                if result == 0 {
                    return Err(crate::tr!(
                        "托盘未接受请求，请稍后重试",
                        "The tray did not accept the request; try again"
                    )
                    .into());
                }
                return Ok(());
            }
        }
    }
    Command::new(std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE))
        .args(request.args())
        .creation_flags(0x08000000)
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_request_round_trips_whitelisted_requests_only() {
        let id = format!("pin-{:032x}-{:08x}-{:016x}", 1, 2, 3);
        for request in [
            Request::PinFile(PathBuf::from(r"C:\data\history\shot-1.png")),
            Request::PinEditFile(PathBuf::from(r"C:\data\history\shot-1.png")),
            Request::PinRecord(id.clone()),
        ] {
            assert_eq!(Request::decode(&request.encode()), Some(request));
        }
        for bad in [
            "pin-file\nrelative.png",
            "pin-file\n",
            "pin-record\nnot-a-pin",
            "settings\nC:\\x",
            "pin-file\nC:\\a\0b.png",
            "pin-file",
        ] {
            assert_eq!(Request::decode(bad.as_bytes()), None, "{bad:?}");
        }
        assert_eq!(
            Request::PinRecord(id.clone()).args(),
            vec![OsString::from("--pin-record"), OsString::from(id)]
        );
    }
}
