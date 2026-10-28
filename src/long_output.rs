//! 长截图输出：完成即复制；裁剪进入同窗剪辑与标注，超大原图暂回退条带调整窗。
use crate::{capture::Image, long_document::LongDocument, stitch::StitchSnapshot};
use gpui::{App, WindowHandle};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};
use windows::Win32::Foundation::HWND;

const MAX_SIDE: u32 = 65535;
const MAX_PIXELS: u64 = 32_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LongOutput {
    /// 裁剪：进入图片编辑器的剪辑工具。
    Crop,
    Copy,
    Save,
    Pin,
}

pub(crate) enum LongOutcome {
    /// 已交给编辑器、剪贴板、文件或贴图，采集会话可以结束。
    Finished,
    /// 没有完成（保存取消或失败），回到采集面板；带原因时显示在面板上。
    Return(Option<String>),
}

/// 成品超过单边或总像素上限时给出原因与做法；未超限返回 None。
pub(crate) fn limit_message(width: u32, height: u32) -> Option<String> {
    if width <= MAX_SIDE && height <= MAX_SIDE && width as u64 * height as u64 <= MAX_PIXELS {
        return None;
    }
    let pixels = width as u64 * height as u64;
    let amount = if crate::i18n::is_english() {
        format!("{:.1} MP", pixels as f64 / 1e6)
    } else {
        format!("{} 万像素", pixels.div_ceil(10_000))
    };
    Some(trf!(
        "长图 {} × {}（约 {}）超过 3200 万像素或单边 65535：撤销最后几段或缩小选区宽度后再完成",
        "The image is {} × {} (about {}), over the 32 MP / 65535 px limit: undo the last segments or narrow the area first",
        width,
        height,
        amount
    ))
}

/// 交给出口的内容：调整窗直接用条带，其余用物化后的成品。
pub(crate) enum LongProduct {
    Image(Image),
    EditorImage { image: Image, axis: crate::stitch::ScrollAxis },
    Strip(StitchSnapshot),
}

/// 后台物化成品（裁剪不物化）；返回的任务在前台交给 `deliver`。
pub(crate) fn export(
    snapshot: StitchSnapshot,
    action: LongOutput,
    cx: &App,
) -> gpui::Task<std::result::Result<LongProduct, String>> {
    cx.background_executor().spawn(async move {
        if action == LongOutput::Crop {
            if limit_message(snapshot.width, snapshot.height).is_some() {
                return Ok(LongProduct::Strip(snapshot));
            }
            let axis = snapshot.axis;
            return LongDocument::new(snapshot)
                .and_then(|document| document.export())
                .map(|image| LongProduct::EditorImage { image, axis })
                .map_err(|error| error.to_string());
        }
        LongDocument::new(snapshot)
            .and_then(|document| document.export())
            .map(LongProduct::Image)
            .map_err(|error| error.to_string())
    })
}

/// 把成品交给对应出口；`owner` 是剪贴板所有者窗口。
/// `placement` 是完成时的选区与滚动方向：复制写进剪贴板、贴图据此落点，两者再贴图位置一致。
pub(crate) fn deliver(
    product: LongProduct,
    action: LongOutput,
    owner: HWND,
    placement: Option<crate::export::ClipboardPlacement>,
    on_done: impl FnOnce(LongOutcome, &mut App) + 'static,
    cx: &mut App,
) {
    let image = match product {
        LongProduct::Image(image) => image,
        LongProduct::EditorImage { image, axis } => {
            let done = Rc::new(RefCell::new(Some(on_done)));
            let callback = done.clone();
            if let Err(error) = crate::image_editor::open_long(
                image, axis, placement,
                move |outcome, cx| {
                    if let Some(done) = callback.borrow_mut().take() { done(outcome, cx); }
                }, cx,
            ) {
                if let Some(done) = done.borrow_mut().take() {
                    done(LongOutcome::Return(Some(error.to_string())), cx);
                }
            }
            return;
        }
        LongProduct::Strip(snapshot) => {
            // 调整窗的“继续采集”或关闭回到面板，输出／标注后结束会话。
            let done = Rc::new(RefCell::new(Some(on_done)));
            let callback = done.clone();
            let result = crate::long_editor::LongEditor::open(
                snapshot,
                move |outcome, cx| {
                    if let Some(done) = callback.borrow_mut().take() {
                        done(
                            match outcome {
                                crate::long_editor::LongEditorOutcome::Finished => {
                                    LongOutcome::Finished
                                }
                                crate::long_editor::LongEditorOutcome::Return => {
                                    LongOutcome::Return(None)
                                }
                            },
                            cx,
                        );
                    }
                },
                cx,
            );
            if let Err(error) = result {
                let done = done.borrow_mut().take();
                if let Some(done) = done {
                    done(LongOutcome::Return(Some(error.to_string())), cx);
                }
            }
            return;
        }
    };
    match action {
        LongOutput::Crop => unreachable!("裁剪已交给编辑器"),
        LongOutput::Copy => match crate::export::copy_image_with(&image, owner, placement) {
            Ok(()) => {
                cx.background_executor()
                    .spawn(async move {
                        // 会话随后结束，无界面可回执；至少记录到日志，不静默吞掉。
                        if let Err(error) = crate::history::record(&image) {
                            eprintln!("长图复制后历史保存失败：{error}");
                        }
                    })
                    .detach();
                on_done(LongOutcome::Finished, cx);
            }
            Err(error) => on_done(LongOutcome::Return(Some(error.to_string())), cx),
        },
        LongOutput::Save => {
            let done = Rc::new(RefCell::new(Some(on_done)));
            let callback = done.clone();
            let result = crate::save_panel::SavePanel::open(
                Arc::new(image),
                None,
                tr!("保存长截图", "Save long capture"),
                move |outcome, cx| {
                    let Some(done) = callback.borrow_mut().take() else {
                        return;
                    };
                    match outcome {
                        crate::save_panel::SaveOutcome::Cancelled => {
                            done(LongOutcome::Return(None), cx)
                        }
                        crate::save_panel::SaveOutcome::Saved { .. } => {
                            done(LongOutcome::Finished, cx)
                        }
                    }
                },
                cx,
            );
            if let Err(error) = result {
                let done = done.borrow_mut().take();
                if let Some(done) = done {
                    done(LongOutcome::Return(Some(error.to_string())), cx);
                }
            }
        }
        LongOutput::Pin => {
            let origin = pin_origin((image.width, image.height), placement);
            match crate::pin_view::PinView::open_with_history_window(image, origin, cx) {
                Ok(pin) => wait_pin_shown(pin, on_done, cx),
                Err(error) => on_done(LongOutcome::Return(Some(error.to_string())), cx),
            }
        }
    }
}

/// 长图贴图窗口左上角（直接贴图与剪贴板再贴图共用）；没有选区信息（验收入口）时退回主屏左上角附近。
pub(crate) fn pin_origin(
    image: (u32, u32),
    placement: Option<crate::export::ClipboardPlacement>,
) -> (i32, i32) {
    match placement {
        Some(crate::export::ClipboardPlacement::Scroll { axis, region }) => {
            let center = (
                region.left.saturating_add((region.width / 2) as i32),
                region.top.saturating_add((region.height / 2) as i32),
            );
            crate::pin_model::scroll_origin(
                image,
                axis,
                region,
                crate::pin_native::work_area(center),
            )
        }
        Some(crate::export::ClipboardPlacement::Exact { origin, .. }) => origin,
        None => (100, 100),
    }
}

/// 贴图首帧真正呈现后才结束采集会话，首帧失败时回到面板。
pub(crate) fn wait_pin_shown(
    pin: WindowHandle<crate::pin_view::PinView>,
    on_done: impl FnOnce(LongOutcome, &mut App) + 'static,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let since = std::time::Instant::now();
        let result = loop {
            let probe = pin
                .update(cx, |_, window, _| {
                    crate::pin_native::first_show_probe(crate::gpui_capture::hwnd(window))
                })
                .map_err(|error| error.to_string());
            match probe {
                Ok(probe) if probe.aborted => break Err("贴图首帧准备失败".to_owned()),
                Ok(probe)
                    if !probe.starting
                        && probe.prepared_serial != 0
                        && (!probe.requested_visible || probe.first_show_ready) =>
                {
                    break Ok(())
                }
                Ok(_) if since.elapsed() < Duration::from_secs(6) => {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                }
                Ok(_) => break Err("贴图首帧准备超时".to_owned()),
                Err(error) => break Err(format!("贴图首帧窗口已结束：{error}")),
            }
        };
        let _ = cx.update(move |cx| match result {
            Ok(()) => on_done(LongOutcome::Finished, cx),
            Err(error) => on_done(LongOutcome::Return(Some(error)), cx),
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_message_only_when_over_limit() {
        assert!(limit_message(1920, 16_000).is_none());
        assert!(limit_message(1920, 16_667).is_some());
        assert!(limit_message(400, 65_536).is_some());
        assert!(limit_message(65_535, 400).is_none());
    }
}
