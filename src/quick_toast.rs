//! 快捷动作完成提示由常驻进程展示；截图进程退出后仍可读到结果。
//! 窗口回调只复制/转发事件，创建、绘制与销毁均在外层消息循环执行。
use crate::{geometry::ScreenRect, theme::ThemeMode, Result};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    mem::size_of,
    sync::{Mutex, OnceLock},
};
use windows::{
    core::w,
    Win32::{
        Foundation::{
            GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, ERROR_SUCCESS, HINSTANCE, HWND,
            LPARAM, LRESULT, RECT, WPARAM,
        },
        Graphics::Gdi::*,
        System::{
            DataExchange::COPYDATASTRUCT,
            LibraryLoader::GetModuleHandleW,
            Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD},
        },
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            WindowsAndMessaging::*,
        },
    },
};

pub const MESSAGE: u32 = WM_APP + 22;
const MAGIC: usize = 0x52535154;
const VERSION: u16 = 1;
const HEADER: usize = 26;
const MAX_CHARACTERS: usize = 256;
const MAX_PACKET: usize = HEADER + MAX_CHARACTERS * 4;
const MAX_PENDING: usize = 32;
const TIMER_ID: usize = 0x7254;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    text: String,
    screen: ScreenRect,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Notice(Notice),
    Paint(usize),
    Close(usize),
}
static PENDING: OnceLock<Mutex<VecDeque<(usize, Event)>>> = OnceLock::new();
thread_local! {static WINDOWS:RefCell<BTreeMap<usize,Toast>>=RefCell::new(BTreeMap::new());}
struct Toast {
    owner: usize,
    text: Vec<u16>,
    dpi: u32,
}

fn valid_screen(screen: ScreenRect) -> bool {
    screen.width > 0
        && screen.height > 0
        && screen.width <= 32_768
        && screen.height <= 32_768
        && screen.left.unsigned_abs() <= 1_000_000
        && screen.top.unsigned_abs() <= 1_000_000
        && screen.left.checked_add(screen.width as i32).is_some()
        && screen.top.checked_add(screen.height as i32).is_some()
}
fn encode(text: &str, screen: ScreenRect) -> Option<Vec<u8>> {
    if text.is_empty()
        || text.chars().count() > MAX_CHARACTERS
        || text.contains('\0')
        || !valid_screen(screen)
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(HEADER + text.len());
    bytes.extend_from_slice(&(MAGIC as u32).to_le_bytes());
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&screen.left.to_le_bytes());
    bytes.extend_from_slice(&screen.top.to_le_bytes());
    bytes.extend_from_slice(&screen.width.to_le_bytes());
    bytes.extend_from_slice(&screen.height.to_le_bytes());
    bytes.extend_from_slice(&(text.len() as u16).to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
    Some(bytes)
}
fn decode(bytes: &[u8]) -> Option<Notice> {
    if bytes.len() < HEADER || bytes.len() > MAX_PACKET {
        return None;
    }
    let u32_at = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let magic: u32 = u32_at(0)?;
    if magic != MAGIC as u32
        || u16::from_le_bytes(bytes[4..6].try_into().ok()?) != VERSION
        || bytes[6..8] != [0, 0]
    {
        return None;
    }
    let length = u16::from_le_bytes(bytes[24..26].try_into().ok()?) as usize;
    if bytes.len() != HEADER + length {
        return None;
    }
    let screen = ScreenRect {
        left: i32::from_le_bytes(bytes[8..12].try_into().ok()?),
        top: i32::from_le_bytes(bytes[12..16].try_into().ok()?),
        width: u32_at(16)?,
        height: u32_at(20)?,
    };
    let text = std::str::from_utf8(&bytes[HEADER..]).ok()?;
    encode(text, screen)?;
    Some(Notice {
        text: text.to_owned(),
        screen,
    })
}

fn encode_for_send(text: &str, screen: ScreenRect) -> Option<Vec<u8>> {
    // 保持接收端严格预算；发送端缩短长路径/警告，避免完成提示静默丢失。
    let shortened = text.char_indices().nth(MAX_CHARACTERS).map(|_| {
        let mut text: String = text.chars().take(MAX_CHARACTERS - 1).collect();
        text.push('…');
        text
    });
    encode(shortened.as_deref().unwrap_or(text), screen)
}

/// 没有常驻窗口或接收超时时返回 false；长文本按字符边界缩短，不能让完成截图等待提示窗口。
pub fn send(text: &str, screen: ScreenRect) -> bool {
    let Some(bytes) = encode_for_send(text, screen) else {
        return false;
    };
    unsafe {
        let Ok(resident) = FindWindowW(w!("RustShot.MVP.Window"), None) else {
            return false;
        };
        if resident.0.is_null() {
            return false;
        }
        let data = COPYDATASTRUCT {
            dwData: MAGIC,
            cbData: bytes.len() as u32,
            lpData: bytes.as_ptr().cast_mut().cast(),
        };
        let mut accepted = 0usize;
        SendMessageTimeoutW(
            resident,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM((&data as *const COPYDATASTRUCT) as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            50,
            Some(&mut accepted),
        )
        .0 != 0
            && accepted != 0
    }
}

fn post(owner: HWND, event: Event) -> bool {
    if owner.0.is_null() {
        return false;
    }
    let pending = PENDING.get_or_init(|| Mutex::new(VecDeque::new()));
    let Ok(mut pending) = pending.lock() else {
        return false;
    };
    let key = owner.0 as usize;
    if !matches!(&event, Event::Notice(_))
        && pending
            .iter()
            .any(|(old_owner, old_event)| *old_owner == key && *old_event == event)
    {
        return true;
    }
    if pending.len() >= MAX_PENDING {
        return false;
    }
    pending.push_back((key, event));
    // PostMessage 不同步进入窗口回调；队列锁在回调读取之前释放。
    if unsafe { PostMessageW(Some(owner), MESSAGE, WPARAM(0), LPARAM(0)) }.is_err() {
        pending.pop_back();
        return false;
    }
    true
}

/// 仅从真实 WM_COPYDATA 回调调用；系统保证 lpData 在回调返回前可读。
pub unsafe fn accept(owner: HWND, data: &COPYDATASTRUCT) -> bool {
    let size = data.cbData as usize;
    if data.dwData != MAGIC || !(HEADER..=MAX_PACKET).contains(&size) || data.lpData.is_null() {
        return false;
    }
    let bytes = std::slice::from_raw_parts(data.lpData.cast::<u8>(), size);
    decode(bytes).is_some_and(|notice| post(owner, Event::Notice(notice)))
}

/// 在常驻消息循环收到 MESSAGE 后调用；可重复调用，空队列没有副作用。
pub fn show(owner: HWND, theme: ThemeMode) -> Result<()> {
    let Some(pending) = PENDING.get() else {
        return Ok(());
    };
    let events = {
        let mut pending = pending.lock().map_err(|_| "提示事件队列已失效")?;
        let mut events = Vec::new();
        let mut other = VecDeque::new();
        while let Some((target, event)) = pending.pop_front() {
            if target == owner.0 as usize {
                events.push(event);
            } else {
                other.push_back((target, event));
            }
        }
        *pending = other;
        events
    };
    for event in events {
        match event {
            Event::Notice(notice) => create(owner, notice, theme)?,
            Event::Paint(window) => paint(HWND(window as *mut _), theme)?,
            Event::Close(window) => close(HWND(window as *mut _)),
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let owner = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut _);
    match message {
        WM_PAINT => {
            let _ = post(owner, Event::Paint(hwnd.0 as usize));
            DefWindowProcW(hwnd, message, wp, lp)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER if wp.0 == TIMER_ID => {
            let _ = post(owner, Event::Close(hwnd.0 as usize));
            LRESULT(0)
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            let _ = post(owner, Event::Paint(hwnd.0 as usize));
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = post(owner, Event::Close(hwnd.0 as usize));
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        _ => DefWindowProcW(hwnd, message, wp, lp),
    }
}

fn create(owner: HWND, notice: Notice, theme: ThemeMode) -> Result<()> {
    let old = WINDOWS.with(|windows| {
        windows
            .borrow()
            .iter()
            .filter_map(|(window, toast)| (toast.owner == owner.0 as usize).then_some(*window))
            .collect::<Vec<_>>()
    });
    for window in old {
        close(HWND(window as *mut _));
    }
    unsafe {
        let instance = HINSTANCE(GetModuleHandleW(None)?.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("RustShot.QuickToast.v1"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err("无法注册完成提示窗口".into());
        }
        let requested = RECT {
            left: notice.screen.left,
            top: notice.screen.top,
            right: notice.screen.left + notice.screen.width as i32,
            bottom: notice.screen.top + notice.screen.height as i32,
        };
        let monitor = MonitorFromRect(&requested, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcWork
        } else {
            requested
        };
        let mut dpi = 96;
        let mut dpi_y = 96;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        dpi = dpi.clamp(96, 384);
        let unit = |value: i32| value * dpi as i32 / 96;
        let margin = unit(20);
        let width = unit(400).min((work.right - work.left - margin * 2).max(unit(80)));
        let height = unit(76).min((work.bottom - work.top - margin * 2).max(unit(40)));
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            class.lpszClassName,
            w!(""),
            WS_POPUP,
            work.right - margin - width,
            work.bottom - margin - height,
            width,
            height,
            Some(owner),
            None,
            Some(instance),
            None,
        )?;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, owner.0 as isize);
        if SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA).is_err() {
            let _ = DestroyWindow(hwnd);
            return Err("无法设置完成提示显示属性".into());
        }
        WINDOWS.with(|windows| {
            windows.borrow_mut().insert(
                hwnd.0 as usize,
                Toast {
                    owner: owner.0 as usize,
                    text: notice.text.encode_utf16().collect(),
                    dpi,
                },
            )
        });
        if SetTimer(Some(hwnd), TIMER_ID, 1500, None) == 0 {
            close(hwnd);
            return Err("无法设置完成提示定时器".into());
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if let Err(error) = paint(hwnd, theme) {
            close(hwnd);
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn system_is_light() -> bool {
    let mut value = 0u32;
    let mut bytes = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut bytes),
        ) == ERROR_SUCCESS
            && value != 0
    }
}
fn palette(theme: ThemeMode) -> (COLORREF, COLORREF, COLORREF) {
    let light = match theme {
        ThemeMode::Light => true,
        ThemeMode::Dark => false,
        ThemeMode::System => system_is_light(),
    };
    if light {
        (
            COLORREF(0x00F4F4F4),
            COLORREF(0x00222222),
            COLORREF(0x00C0C0C0),
        )
    } else {
        (
            COLORREF(0x00282828),
            COLORREF(0x00EEEEEE),
            COLORREF(0x00555555),
        )
    }
}
fn paint(hwnd: HWND, theme: ThemeMode) -> Result<()> {
    let state = WINDOWS.with(|windows| {
        windows
            .borrow()
            .get(&(hwnd.0 as usize))
            .map(|toast| (toast.text.clone(), toast.dpi))
    });
    let Some((mut text, dpi)) = state else {
        return Ok(());
    };
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect)?;
        let dc = GetDC(Some(hwnd));
        if dc.is_invalid() {
            return Err("完成提示窗口已失效".into());
        }
        let (background, foreground, border) = palette(theme);
        let brush = CreateSolidBrush(background);
        let frame = CreateSolidBrush(border);
        FillRect(dc, &rect, brush);
        FrameRect(dc, &rect, frame);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let _ = DeleteObject(HGDIOBJ(frame.0));
        let font = CreateFontW(
            -((16 * dpi / 96) as i32),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let previous = SelectObject(dc, HGDIOBJ(font.0));
        let _ = SetTextColor(dc, foreground);
        let _ = SetBkMode(dc, TRANSPARENT);
        let inset = (16 * dpi / 96) as i32;
        rect.left += inset;
        rect.right -= inset;
        rect.top += (13 * dpi / 96) as i32;
        rect.bottom -= inset;
        DrawTextW(
            dc,
            &mut text,
            &mut rect,
            DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        let _ = SelectObject(dc, previous);
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = ReleaseDC(Some(hwnd), dc);
    }
    Ok(())
}
fn close(hwnd: HWND) {
    let existed = WINDOWS.with(|windows| windows.borrow_mut().remove(&(hwnd.0 as usize)).is_some());
    if existed {
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
            let _ = DestroyWindow(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn screen() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: 0,
            width: 1920,
            height: 1080,
        }
    }
    #[test]
    fn quick_toast_codec_preserves_unicode_and_negative_monitor_origin() {
        let text = "已复制表格 ✓";
        let encoded = encode(text, screen()).unwrap();
        assert_eq!(
            decode(&encoded),
            Some(Notice {
                text: text.to_owned(),
                screen: screen()
            })
        );
    }
    #[test]
    fn quick_toast_codec_rejects_wrong_version_length_and_utf8() {
        let bytes = encode("a", screen()).unwrap();
        let mut version = bytes.clone();
        version[4] = 2;
        assert!(decode(&version).is_none());
        let mut length = bytes.clone();
        length[24] = 2;
        assert!(decode(&length).is_none());
        let mut utf8 = bytes;
        utf8[HEADER] = 255;
        assert!(decode(&utf8).is_none());
        assert!(decode(&[0; MAX_PACKET + 1]).is_none());
    }
    #[test]
    fn quick_toast_codec_applies_character_and_screen_budgets() {
        assert!(encode(&"中".repeat(256), screen()).is_some());
        assert!(encode(&"中".repeat(257), screen()).is_none());
        assert!(encode("a\0b", screen()).is_none());
        let mut bad = screen();
        bad.width = u32::MAX;
        assert!(encode("a", bad).is_none());
    }
    #[test]
    fn quick_toast_sender_truncates_multibyte_paths_with_emoji_without_relaxing_decoder() {
        let text = format!("已保存：C:/{}/截图.png", "中文🙂".repeat(100));
        let bytes = encode_for_send(&text, screen()).unwrap();
        let notice = decode(&bytes).unwrap();
        assert_eq!(notice.text.chars().count(), MAX_CHARACTERS);
        assert_eq!(
            notice.text,
            format!("{}…", text.chars().take(255).collect::<String>())
        );
        assert!(notice.text.contains('🙂'));
        assert!(encode(&text, screen()).is_none());
        let oversized_text = "中".repeat(MAX_CHARACTERS + 1);
        let mut oversized = encode("a", screen()).unwrap();
        oversized.truncate(HEADER);
        oversized[24..26].copy_from_slice(&(oversized_text.len() as u16).to_le_bytes());
        oversized.extend_from_slice(oversized_text.as_bytes());
        assert!(oversized.len() <= MAX_PACKET);
        assert!(decode(&oversized).is_none());
        let exact = "🙂".repeat(MAX_CHARACTERS);
        assert_eq!(
            decode(&encode_for_send(&exact, screen()).unwrap())
                .unwrap()
                .text,
            exact
        );
    }
}
