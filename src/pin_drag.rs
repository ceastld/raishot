//! 原生文件拖出在独立 STA 线程执行，避免 OLE 消息循环重入 GPUI 的借用。
use crate::{capture::Image, export, pin_drag_request::DragTicket, pin_view::PinView, Result};
use gpui::{Context, Window};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{implement, BOOL, HRESULT, PCWSTR},
    Win32::{
        Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS},
        System::{
            Com::IDataObject, Ole::*, SystemServices::MODIFIERKEYS_FLAGS,
            Threading::GetCurrentThreadId,
        },
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON},
            Shell::*,
        },
    },
};
pub(crate) fn prepare(image: Image) -> Result<PathBuf> {
    let folder = crate::preferences::data_dir().join("drag-out");
    std::fs::create_dir_all(&folder)?;
    let now = SystemTime::now();
    for entry in std::fs::read_dir(&folder)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("pin-drag-")
            && name.ends_with(".png")
            && entry.file_type()?.is_file()
            && entry
                .metadata()?
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age.as_secs() > 86400)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let path = folder.join(format!(
        "pin-drag-{}-{}.png",
        std::process::id(),
        now.duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    export::save_png(image, &path)?;
    Ok(std::fs::canonicalize(path)?)
}
pub(crate) fn data_object(path: &Path) -> Result<IDataObject> {
    use std::os::windows::ffi::OsStrExt;
    // 文件系统规范路径带 \\?\ 前缀；Shell PIDL 接口需要 DOS / UNC 路径。
    let mut path: Vec<_> = path.as_os_str().encode_wide().collect();
    if path.starts_with(&[92, 92, 63, 92, 85, 78, 67, 92]) {
        path.splice(..8, [92, 92]);
    } else if path.starts_with(&[92, 92, 63, 92]) {
        path.drain(..4);
    }
    path.push(0);
    unsafe {
        let full = ILCreateFromPathW(PCWSTR(path.as_ptr()));
        if full.is_null() {
            return Err("拖出图片路径无效".into());
        }
        let parent = ILClone(full);
        if parent.is_null() {
            ILFree(Some(full));
            return Err("无法创建拖拽数据".into());
        }
        let child = ILFindLastID(full);
        let _ = ILRemoveLastID(Some(parent));
        let data = SHCreateDataObject(
            Some(parent),
            Some(&[child as *const _]),
            None::<&IDataObject>,
        );
        ILFree(Some(parent));
        ILFree(Some(full));
        Ok(data?)
    }
}
#[implement(IDropSource)]
struct PinDropSource {
    ticket: DragTicket,
}
#[allow(non_snake_case)]
impl IDropSource_Impl for PinDropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        self.ticket.observe_ole_capture();
        if self.ticket.cancelled() || escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & 1 == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
struct PreparedFile {
    path: PathBuf,
    retained: bool,
}
impl Drop for PreparedFile {
    fn drop(&mut self) {
        if !self.retained {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn run(image: Image, ticket: DragTicket) -> std::result::Result<bool, String> {
    std::thread::spawn(move || -> std::result::Result<bool, String> {
        if ticket.cancelled() {
            return Ok(false);
        }
        unsafe {
            OleInitialize(None).map_err(|e| e.to_string())?;
        }
        struct Ole(DragTicket);
        impl Drop for Ole {
            fn drop(&mut self) {
                self.0.clear_ole_capture();
                unsafe {
                    OleUninitialize();
                }
            }
        }
        let _ole = Ole(ticket.clone());
        let mut file = PreparedFile {
            path: prepare(image).map_err(|e| e.to_string())?,
            retained: false,
        };
        if ticket.cancelled() || unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 {
            return Ok(false);
        }
        let data = data_object(&file.path).map_err(|e| e.to_string())?;
        let source: IDropSource = PinDropSource {
            ticket: ticket.clone(),
        }
        .into();
        if !ticket.arm_ole(unsafe { GetCurrentThreadId() }) {
            return Ok(false);
        }
        let _watch = ticket.watch_ole().map_err(|e| e.to_string())?;
        if ticket.cancelled() {
            return Ok(false);
        }
        let copied = unsafe {
            SHDoDragDrop(None, &data, &source, DROPEFFECT_COPY)
                .map(|effect| effect == DROPEFFECT_COPY)
                .map_err(|e| e.to_string())
        }?;
        // Shell 可能延后读取成功拖出的文件，沿用24小时清理；取消的临时文件立即删除。
        file.retained = copied;
        Ok(copied)
    })
    .join()
    .map_err(|_| "图片拖出线程退出".to_owned())?
}
impl PinView {
    pub(crate) fn drag_out(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.export_user(crate::pin_export::UserExportAction::Drag, w, cx);
    }
    pub(crate) fn drag_out_image(&mut self, image: Image, w: &mut Window, cx: &mut Context<Self>) {
        if self.processing || self.saving {
            return;
        }
        let Some(ticket) = self.drag_request.as_ref().map(|request| request.ticket()) else {
            return;
        };
        if ticket.cancelled() {
            self.drag_request = None;
            self.status.clear();
            cx.notify();
            return;
        }
        self.processing = true;
        self.status = tr!(
            "拖出图片 · 松开放置，Esc 取消",
            "Dragging image · release to drop, Esc cancels"
        )
        .into();
        cx.notify();
        let callback_ticket = ticket.clone();
        let task = cx
            .background_executor()
            .spawn(async move { run(image, ticket) });
        cx.spawn_in(w, async move |v, cx| {
            let result = task.await;
            let _ = v.update(cx, |v, cx| {
                if !v
                    .drag_request
                    .as_ref()
                    .is_some_and(|request| request.ticket().same_request(&callback_ticket))
                {
                    return;
                }
                v.processing = false;
                v.drag_request = None;
                v.status = result.err().unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
    }
    pub(crate) fn drop_files(&mut self, paths: &[PathBuf], w: &mut Window, cx: &mut Context<Self>) {
        if self.processing || self.saving {
            return;
        }
        let paths: Vec<_> = paths.iter().take(8).cloned().collect();
        let origin = crate::pin_native::rect(crate::gpui_capture::hwnd(w))
            .map(|p| (p.x + 24, p.y + 24))
            .unwrap_or((100, 100));
        self.processing = true;
        let task = cx.background_executor().spawn(async move {
            paths
                .into_iter()
                .map(|p| export::read_image(&p).map_err(|e| e.to_string()))
                .collect::<Vec<_>>()
        });
        cx.spawn_in(w, async move |v, cx| {
            let images = task.await;
            let _ = v.update(cx, |v, cx| {
                v.processing = false;
                v.status.clear();
                for (i, image) in images.into_iter().enumerate() {
                    let result = image.and_then(|image| {
                        PinView::open(
                            image,
                            (origin.0 + i as i32 * 24, origin.1 + i as i32 * 24),
                            cx,
                        )
                        .map_err(|e| e.to_string())
                    });
                    if let Err(e) = result {
                        v.status = e;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pin_export_detail_com_source_cancellation_overrides_new_press_and_drop() {
        let ticket = DragTicket::for_test();
        let source: IDropSource = PinDropSource {
            ticket: ticket.clone(),
        }
        .into();
        unsafe {
            assert_eq!(
                source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(1)),
                HRESULT(0)
            );
            assert_eq!(
                source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)),
                DRAGDROP_S_DROP
            );
            assert_eq!(
                source.QueryContinueDrag(true, MODIFIERKEYS_FLAGS(1)),
                DRAGDROP_S_CANCEL
            );
            assert_eq!(
                source.GiveFeedback(DROPEFFECT_COPY),
                DRAGDROP_S_USEDEFAULTCURSORS
            );
            ticket.cancel();
            assert_eq!(
                source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(1)),
                DRAGDROP_S_CANCEL
            );
            assert_eq!(
                source.QueryContinueDrag(false, MODIFIERKEYS_FLAGS(0)),
                DRAGDROP_S_CANCEL
            );
        }
    }
    #[test]
    fn pin_export_detail_prepared_file_cancel_cleanup_keeps_successful_drop_file() {
        let folder = std::path::Path::new("target/qa/pin-export-details").join(format!(
            "file-cleanup-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        for retained in [false, true] {
            let path = folder.join(format!("{retained}.png"));
            std::fs::write(&path, b"owned cleanup fixture").unwrap();
            drop(PreparedFile {
                path: path.clone(),
                retained,
            });
            assert_eq!(path.exists(), retained);
            if retained {
                std::fs::remove_file(path).unwrap();
            }
        }
        std::fs::remove_dir(folder).unwrap();
    }
}
