//! 一次打开系统剪贴板，发布 TSV 与含合并格的 CF_HTML。
use crate::{table_export, table_grid::TableGrid, Result};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
            SetClipboardData,
        },
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
    },
};

struct Clipboard;
impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}
struct Payload(Option<HGLOBAL>);
impl Payload {
    fn new(bytes: &[u8]) -> Result<Self> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len())? };
        let payload = Self(Some(handle));
        let pointer = unsafe { GlobalLock(handle).cast::<u8>() };
        if pointer.is_null() {
            return Err("无法分配表格剪贴板内存".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            let _ = GlobalUnlock(handle);
        }
        Ok(payload)
    }
    fn publish(mut self, format: u32) -> Result<()> {
        unsafe {
            SetClipboardData(format, Some(HANDLE(self.0.unwrap().0)))?;
        }
        self.0 = None;
        Ok(())
    }
}
impl Drop for Payload {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            unsafe {
                let _ = GlobalFree(Some(handle));
            }
        }
    }
}

pub fn copy_table(grid: &TableGrid, owner: HWND) -> Result<()> {
    if owner.0.is_null() {
        return Err("表格复制窗口已失效".into());
    }
    let text = table_export::tsv(grid).ok_or("表格结构无效")?;
    let mut text_bytes = Vec::with_capacity(text.len() * 2 + 2);
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        text_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut html = table_export::cf_html(grid)
        .ok_or("表格结构无效")?
        .into_bytes();
    html.push(0);
    // 打开前完成所有分配，避免占用剪贴板时失败或等待。
    let text = Payload::new(&text_bytes)?;
    let html = Payload::new(&html)?;
    let format = unsafe { RegisterClipboardFormatW(windows::core::w!("HTML Format")) };
    if format == 0 {
        return Err("无法注册 HTML 剪贴板格式".into());
    }
    unsafe {
        OpenClipboard(Some(owner)).map_err(|_| "剪贴板正被其他程序占用，请重试")?;
    }
    let _clipboard = Clipboard;
    unsafe {
        EmptyClipboard()?;
    }
    text.publish(13)?;
    html.publish(format)?;
    Ok(())
}
