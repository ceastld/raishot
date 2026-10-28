//! 替换 GPUI 0.2.2 Windows 文件接收适配，直接将 HGLOBAL 作为 HDROP，延后交给视图。
use crate::{pin_view::PinView, Result};
use gpui::{App, WindowHandle};
use std::{cell::Cell, path::PathBuf};
use windows::{
    core::{implement, Ref},
    Win32::{
        Foundation::*,
        System::{Com::*, Ole::*},
        UI::Shell::*,
    },
};
#[implement(IDropTarget)]
struct PinDropTarget {
    owner: WindowHandle<PinView>,
    cx: gpui::AsyncApp,
    accepted: Cell<bool>,
}
fn paths(data: &IDataObject) -> windows::core::Result<Vec<PathBuf>> {
    unsafe {
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut medium = data.GetData(&format)?;
        let handle = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(handle, u32::MAX, None).min(8);
        let mut paths = Vec::new();
        for i in 0..count {
            let len = DragQueryFileW(handle, i, None);
            if len == 0 || len > 32767 {
                continue;
            }
            let mut name = vec![0; len as usize + 1];
            if DragQueryFileW(handle, i, Some(&mut name)) == len {
                paths.push(PathBuf::from(String::from_utf16_lossy(
                    &name[..len as usize],
                )));
            }
        }
        ReleaseStgMedium(&mut medium);
        Ok(paths)
    }
}
#[allow(non_snake_case)]
impl IDropTarget_Impl for PinDropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        _: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let accepted = data.ok().and_then(paths).is_ok_and(|p| !p.is_empty());
        self.accepted.set(accepted);
        unsafe {
            *effect = if accepted {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }
    fn DragOver(
        &self,
        _: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *effect = if self.accepted.get() {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        self.accepted.set(false);
        Ok(())
    }
    fn Drop(
        &self,
        data: Ref<IDataObject>,
        _: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = paths(data.ok()?)?;
        unsafe {
            *effect = if paths.is_empty() {
                DROPEFFECT_NONE
            } else {
                DROPEFFECT_COPY
            };
        }
        let owner = self.owner;
        self.cx
            .spawn(async move |cx| {
                let _ = owner.update(cx, |v, w, cx| v.drop_files(&paths, w, cx));
            })
            .detach();
        self.accepted.set(false);
        Ok(())
    }
}
pub(crate) fn install(owner: WindowHandle<PinView>, h: HWND, cx: &App) -> Result<IDropTarget> {
    let target: IDropTarget = PinDropTarget {
        owner,
        cx: cx.to_async(),
        accepted: Cell::new(false),
    }
    .into();
    unsafe {
        let _ = RevokeDragDrop(h);
        RegisterDragDrop(h, &target)?;
    }
    Ok(target)
}
