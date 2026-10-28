//! 源文件拖出只移交复制引用，不生成临时图，也不取得源文件的删除或移动权限。
//! 身份复核只覆盖检查时刻；Shell随后按路径取文件，不是文件系统内容CAS。
use crate::{file_snapshot::FileSnapshot, pin_drag_request::DragTicket};
use std::path::{Path, PathBuf};
use windows::{
    core::{implement, BOOL, HRESULT},
    Win32::{
        Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS},
        System::{Ole::*, SystemServices::MODIFIERKEYS_FLAGS, Threading::GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON},
            Shell::SHDoDragDrop,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DragOutcome {
    Copied,
    Cancelled,
}

/// 调用方须在后台任务调用，并在UI线程保留本次NativeDragRequest。
/// expected必须来自已载入图像的来源检查，不能在拖出开始时重新认领同路径的新文件。
pub(super) fn drag_out(
    path: PathBuf,
    expected: FileSnapshot,
    ticket: DragTicket,
) -> Result<DragOutcome, String> {
    // 即使线程创建或身份检查失败，旧票据也不能被迟到的按键重新接管。
    struct Finish(DragTicket);
    impl Drop for Finish {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _finish = Finish(ticket.clone());
    if ticket.cancelled() {
        return Ok(DragOutcome::Cancelled);
    }
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    let worker_ticket = ticket.clone();
    let worker = std::thread::Builder::new()
        .name("rustshot-image-file-drag".into())
        .spawn(move || run_sta(&path, &expected, worker_ticket));
    let worker = match worker {
        Ok(worker) => worker,
        Err(_) if ticket.cancelled() => return Ok(DragOutcome::Cancelled),
        Err(error) => {
            return Err(trf!(
                "无法启动文件拖出：{error}",
                "Could not start file drag: {error}"
            ))
        }
    };
    worker
        .join()
        .map_err(|_| tr!("文件拖出线程退出", "File drag worker exited unexpectedly").to_string())?
}

struct OleSession(DragTicket);
impl Drop for OleSession {
    fn drop(&mut self) {
        self.0.clear_ole_capture();
        // 只在本线程OleInitialize成功后构造；S_FALSE也持有本次初始化引用。
        unsafe { OleUninitialize() };
    }
}

#[implement(IDropSource)]
struct FileDropSource {
    ticket: DragTicket,
}

#[allow(non_snake_case)]
impl IDropSource_Impl for FileDropSource_Impl {
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

fn released(ticket: &DragTicket) -> bool {
    ticket.cancelled() || unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0
}

fn check_source(path: &Path, expected: &FileSnapshot, ticket: &DragTicket) -> Result<bool, String> {
    check_source_with(
        expected,
        ticket,
        || crate::image_collection::ensure_local_file(path),
        || crate::file_snapshot::inspect(path).map_err(|error| error.to_string()),
    )
}

/// 取消优先于同次元数据错误；逐级本地资格必须先于打开末级文件取得身份。
fn check_source_with(
    expected: &FileSnapshot,
    ticket: &DragTicket,
    local: impl FnOnce() -> Result<(), String>,
    inspect: impl FnOnce() -> Result<Option<FileSnapshot>, String>,
) -> Result<bool, String> {
    if ticket.cancelled() {
        return Ok(false);
    }
    let result = local();
    if ticket.cancelled() {
        return Ok(false);
    }
    result?;
    let actual = inspect();
    if ticket.cancelled() {
        return Ok(false);
    }
    if actual?.as_ref() != Some(expected) {
        return Err(tr!(
            "源文件已变化或不存在，请重新加载后拖出",
            "Source changed or is missing; reload it before dragging"
        )
        .into());
    }
    Ok(true)
}

fn run_sta(
    path: &Path,
    expected: &FileSnapshot,
    ticket: DragTicket,
) -> Result<DragOutcome, String> {
    if released(&ticket) || !check_source(path, expected, &ticket)? {
        return Ok(DragOutcome::Cancelled);
    }
    let initialized = unsafe { OleInitialize(None) };
    let _ole = match initialized {
        Ok(()) => OleSession(ticket.clone()),
        Err(_) if ticket.cancelled() => return Ok(DragOutcome::Cancelled),
        Err(error) => {
            return Err(trf!(
                "无法初始化文件拖出：{error}",
                "Could not initialize file drag: {error}"
            ))
        }
    };
    if released(&ticket) {
        return Ok(DragOutcome::Cancelled);
    }
    let data = crate::pin_drag::data_object(path);
    if released(&ticket) {
        return Ok(DragOutcome::Cancelled);
    }
    let data = data.map_err(|error| {
        trf!(
            "无法创建文件拖出引用：{error}",
            "Could not create file drag reference: {error}"
        )
    })?;
    // Shell构造PIDL后仍复核，不能把检查期间被替换的来源当成屏幕上的文件。
    if !check_source(path, expected, &ticket)? || released(&ticket) {
        return Ok(DragOutcome::Cancelled);
    }
    let source: IDropSource = FileDropSource {
        ticket: ticket.clone(),
    }
    .into();
    if !ticket.arm_ole(unsafe { GetCurrentThreadId() }) {
        return Ok(DragOutcome::Cancelled);
    }
    let watch = ticket.watch_ole();
    if ticket.cancelled() {
        return Ok(DragOutcome::Cancelled);
    }
    let _watch = watch.map_err(|error| {
        trf!(
            "无法跟踪文件拖出取消：{error}",
            "Could not track file drag cancellation: {error}"
        )
    })?;
    if released(&ticket) || !check_source(path, expected, &ticket)? {
        return Ok(DragOutcome::Cancelled);
    }
    let result = unsafe { SHDoDragDrop(None, &data, &source, DROPEFFECT_COPY) };
    match result {
        // 实际复制交付已经完成，不能因随后发生的关闭把成功改报为取消。
        Ok(effect) if effect == DROPEFFECT_COPY => Ok(DragOutcome::Copied),
        Ok(_) => Ok(DragOutcome::Cancelled),
        Err(_) if ticket.cancelled() => Ok(DragOutcome::Cancelled),
        Err(error) => Err(trf!("文件拖出失败：{error}", "File drag failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn identity() -> FileSnapshot {
        FileSnapshot {
            volume: 1,
            id: [2; 16],
            created: 3,
            written: 4,
            bytes: 5,
            attributes: 0x20,
        }
    }

    #[test]
    fn image_viewer_file_drag_requires_the_loaded_object_and_metadata() {
        let expected = identity();
        let ticket = DragTicket::for_test();
        assert!(check_source_with(&expected, &ticket, || Ok(()), || Ok(Some(identity()))).unwrap());
        let mut replacements = vec![None];
        for field in 0..6 {
            let mut changed = expected.clone();
            match field {
                0 => changed.volume += 1,
                1 => changed.id[0] += 1,
                2 => changed.created += 1,
                3 => changed.written += 1,
                4 => changed.bytes += 1,
                _ => changed.attributes ^= 1,
            }
            replacements.push(Some(changed));
        }
        for actual in replacements {
            assert!(check_source_with(&expected, &ticket, || Ok(()), || Ok(actual)).is_err());
        }
    }

    #[test]
    fn image_viewer_file_drag_checks_local_qualification_before_file_identity() {
        let ticket = DragTicket::for_test();
        let inspected = Cell::new(false);
        assert!(check_source_with(
            &identity(),
            &ticket,
            || Err("not local".into()),
            || {
                inspected.set(true);
                Ok(Some(identity()))
            },
        )
        .is_err());
        assert!(!inspected.get());
        assert!(check_source_with(
            &identity(),
            &ticket,
            || Ok(()),
            || Err("identity unavailable".into()),
        )
        .is_err());
    }

    #[test]
    fn image_viewer_file_drag_cancellation_wins_over_simultaneous_source_failure() {
        let ticket = DragTicket::for_test();
        ticket.cancel();
        assert!(!check_source_with(
            &identity(),
            &ticket,
            || panic!("canceled request must not touch the source"),
            || panic!("canceled request must not inspect identity"),
        )
        .unwrap());
        for stage in 0..2 {
            let ticket = DragTicket::for_test();
            assert!(!check_source_with(
                &identity(),
                &ticket,
                || {
                    if stage == 0 {
                        ticket.cancel();
                        Err("source became unavailable".into())
                    } else {
                        Ok(())
                    }
                },
                || {
                    ticket.cancel();
                    Err("identity read failed".into())
                },
            )
            .unwrap());
        }
    }
}
