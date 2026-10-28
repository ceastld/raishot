//! 从托盘按需启动图片、剪贴板和关闭恢复，待机进程不加载 GPUI。
use crate::{capture::Image, pin_view::PinView, Result};
use gpui::{App, Application};
use std::{cell::RefCell, rc::Rc};
fn decode(bytes: &[u8]) -> Result<Image> {
    let fail = |error: &dyn std::fmt::Display| -> Box<dyn std::error::Error> {
        trf!(
            "无法解码剪贴板图片：{error}",
            "Can't decode the clipboard image: {error}"
        )
        .into()
    };
    crate::image_decode::decode_bytes(bytes)
        .map(|decoded| decoded.image)
        .map_err(|error| fail(&error))
}
enum ClipboardPin {
    Image(Image, Option<crate::export::ClipboardPlacement>),
    Text(crate::pin_text_source::TextSource),
}

/// GPUI 的文件剪贴板会把路径拼成文字；先识别真实 CF_HDROP，避免误建路径文字贴图。
fn clipboard_image_file() -> Result<Option<std::path::PathBuf>> {
    use windows::Win32::{
        System::{DataExchange::*, Ole::CF_HDROP},
        UI::Shell::{DragQueryFileW, HDROP},
    };
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP.0)) }.is_err() {
        return Ok(None);
    }
    unsafe { OpenClipboard(None) }.map_err(|_| {
        tr!(
            "剪贴板正被其他程序占用，请重试",
            "Clipboard is busy; try again"
        )
    })?;
    struct ClipboardFiles;
    impl Drop for ClipboardFiles {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
    let _clipboard = ClipboardFiles;
    let handle = HDROP(unsafe { GetClipboardData(u32::from(CF_HDROP.0)) }?.0);
    let count = unsafe { DragQueryFileW(handle, u32::MAX, None) };
    for index in 0..count {
        let length = unsafe { DragQueryFileW(handle, index, None) };
        if length == 0 || length > 32_767 {
            continue;
        }
        let mut name = vec![0_u16; length as usize + 1];
        if unsafe { DragQueryFileW(handle, index, Some(&mut name)) } != length {
            continue;
        }
        let path = std::path::PathBuf::from(String::from_utf16_lossy(&name[..length as usize]));
        if image::ImageFormat::from_path(&path).is_ok() {
            if let Some(path) = local_image_file(&path) {
                return Ok(Some(path));
            }
        }
    }
    Err(tr!(
        "剪贴板文件中没有可直接读取的本地图片",
        "Clipboard files contain no available local image"
    )
    .into())
}

/// 仅读取路径和属性；先跳过云端占位，再解析链接并检查真实目标，不能为贴图触发下载。
fn local_image_file(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE,
        FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    };
    let ready = |path: &std::path::Path| {
        let attributes =
            unsafe { GetFileAttributesW(&windows::core::HSTRING::from(path.as_os_str())) };
        attributes != u32::MAX
            && attributes
                & (FILE_ATTRIBUTE_DIRECTORY
                    | FILE_ATTRIBUTE_OFFLINE
                    | FILE_ATTRIBUTE_RECALL_ON_OPEN
                    | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
                    .0
                == 0
    };
    if !ready(path) {
        return None;
    }
    let resolved = std::fs::canonicalize(path).ok()?;
    ready(&resolved).then_some(resolved)
}

/// 按剪贴板落点再贴图：原位倍率 1 保持导出像素位置与自然尺寸；
/// 复制时缩小显示的贴图按原倍率回到原处；长截图与直接贴图同一落点规则。
fn open_placed(
    image: Image,
    placement: crate::export::ClipboardPlacement,
    cx: &mut App,
) -> Result<()> {
    use crate::export::ClipboardPlacement;
    match placement {
        ClipboardPlacement::Exact { origin, scale } if scale >= 0.999 => {
            PinView::open_copied_capture(image, origin, cx).map(|_| ())
        }
        ClipboardPlacement::Exact { origin, scale } => {
            let state = crate::pin_model::PinState {
                zoom: scale,
                shadow_enabled: false,
                ..Default::default()
            };
            PinView::open_state(image, origin, state, cx).map(|_| ())
        }
        ClipboardPlacement::Scroll { .. } => {
            let origin =
                crate::long_output::pin_origin((image.width, image.height), Some(placement));
            PinView::open(image, origin, cx)
        }
    }
}

fn clipboard(cx: &mut App) -> Result<ClipboardPin> {
    if let Some((png, placement)) = crate::export::clipboard_png()? {
        return decode(&png).map(|image| ClipboardPin::Image(image, placement));
    }
    let item = cx.read_from_clipboard();
    if let Some(image) = item.as_ref().and_then(|item| {
        item.entries().iter().find_map(|e| {
            if let gpui::ClipboardEntry::Image(i) = e {
                Some(i)
            } else {
                None
            }
        })
    }) {
        return decode(image.bytes()).map(|image| ClipboardPin::Image(image, None));
    }
    if let Some(path) = clipboard_image_file()? {
        return crate::export::read_image(&path).map(|image| ClipboardPin::Image(image, None));
    }
    let item = item.ok_or(tr!(
        "剪贴板没有图片或文本",
        "Clipboard has no image or text"
    ))?;
    let text = item.text().filter(|t| !t.trim().is_empty()).ok_or(tr!(
        "剪贴板没有图片或文本",
        "Clipboard has no image or text"
    ))?;
    crate::pin_text_source::prepare(text).map(ClipboardPin::Text)
}
pub fn run(mode: &str) -> Result<()> {
    run_with(mode, None, None, None)
}

pub(crate) fn run_file(path: std::path::PathBuf, origin: Option<(i32, i32)>) -> Result<()> {
    run_with("--pin-file", Some(path), origin, None)
}

pub(crate) fn run_clipboard(origin: Option<(i32, i32)>) -> Result<()> {
    run_with("--pin-clipboard", None, origin, None)
}

/// 按工作区记录恢复指定贴图（主界面历史页“恢复贴图”）。
pub(crate) fn run_record(id: String) -> Result<()> {
    if !crate::pin_workspace::valid_pin_id(&id) {
        return Err("贴图记录标识无效".into());
    }
    run_with("--pin-record", None, None, Some(id))
}

fn run_with(
    mode: &str,
    initial_path: Option<std::path::PathBuf>,
    requested_origin: Option<(i32, i32)>,
    record: Option<String>,
) -> Result<()> {
    if mode == "--pin-smoke" {
        if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("menu") {
            std::env::set_var(
                "RUSTSHOT_DATA_DIR",
                crate::pin_menu_smoke::isolate_data_dir()?,
            );
        } else {
            std::env::set_var(
                "RUSTSHOT_DATA_DIR",
                std::env::current_dir()?.join(
                    match std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() {
                        Ok("details") => "target/qa/pin-details/settings",
                        Ok("toolbar") => "target/qa/pin-toolbar/settings",
                        Ok("zoom") => "target/qa/pin-zoom/settings",
                        Ok("zoom_preferences") => "target/qa/pin-zoom-preferences/settings",
                        Ok("enlarged_export") => "target/qa/pin-enlarged-export/settings",
                        Ok("export_details") => "target/qa/pin-export-details/settings",
                        Ok("local_fill") => "target/qa/local-fill/settings",
                        Ok("text_contract") => "target/qa/text-contract/settings",
                        Ok("spotlight") => "target/qa/spotlight/pin-settings",
                        Ok("radius") => "target/qa/radius-handle/pin-settings",
                        Ok("parameter") => "target/qa/parameter/pin-settings",
                        Ok("highlight_parameter") => "target/qa/highlight-radius/pin-settings",
                        Ok("finish_text_parameter") => {
                            "target/qa/finish-text-parameter/pin-settings"
                        }
                        Ok("radius_bulk_parameter") => "target/qa/radius-bulk/pin-settings",
                        _ => "target/qa/pin/settings",
                    },
                ),
            );
        }
    }
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    // “始终”档位让贴图宿主也预热 OCR；Owner 活到本进程 GPUI 退出，先让出首显。
    let preferences = crate::preferences::Preferences::load();
    let _ocr_warmup = (mode != "--pin-smoke"
        && preferences.ocr_warmup == crate::ppocr::WarmupMode::Always)
        .then(|| {
            crate::ppocr::start_cached_profile_warmup(
                preferences.ocr_profile,
                std::time::Duration::from_secs(1),
            )
        })
        .flatten();
    let errors = Rc::new(RefCell::new(None::<String>));
    let final_errors = errors.clone();
    let mode = mode.to_owned();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            let mut cursor = windows::Win32::Foundation::POINT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor);
            }
            let origin = requested_origin.unwrap_or((cursor.x, cursor.y));
            if mode == "--pin-smoke" {
                *errors.borrow_mut() = Some("贴图专项未完成全部步骤".into());
                if matches!(
                    std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref(),
                    Ok("spotlight"
                        | "radius"
                        | "parameter"
                        | "highlight_parameter"
                        | "finish_text_parameter"
                        | "radius_bulk_parameter")
                ) {
                    crate::spotlight_smoke::schedule_pin(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("details") {
                    crate::pin_detail_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("toolbar") {
                    crate::pin_toolbar_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("interaction") {
                    crate::pin_interaction_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("menu") {
                    crate::pin_menu_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("zoom") {
                    crate::pin_zoom_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref()
                    == Ok("zoom_preferences")
                {
                    crate::pin_zoom_preferences_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref()
                    == Ok("enlarged_export")
                {
                    crate::pin_export_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref()
                    == Ok("export_details")
                {
                    crate::pin_export_detail_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("local_fill") {
                    crate::local_fill_smoke::schedule(cx, errors);
                } else if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("text_contract")
                {
                    crate::text_contract_smoke::schedule(cx, errors);
                } else {
                    crate::pin_smoke::schedule(cx, errors);
                }
                return;
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty()
                    && !crate::pin_workspace_runtime::is_restoring(cx)
                    && !crate::pin_live_view::has_open_windows()
                {
                    cx.quit();
                }
            })
            .detach();
            if let Some(id) = record.as_deref().filter(|_| mode == "--pin-record") {
                if let Err(error) = crate::pin_workspace_runtime::restore_record(id, cx) {
                    *errors.borrow_mut() = Some(error.to_string());
                    cx.quit();
                }
                return;
            }
            if mode == "--pin-workspace" {
                // 托盘旧入口：转去主界面贴图页，本进程不留窗口。
                if let Err(error) = crate::pin_workspace_runtime::open_manager(None, cx) {
                    *errors.borrow_mut() = Some(error.to_string());
                }
                cx.quit();
                return;
            }
            if mode == "--pin-session-restore" {
                crate::pin_workspace_runtime::spawn_restore_active(cx);
                return;
            }
            if mode == "--pin-file" {
                let receiver = initial_path.is_none().then(|| {
                    cx.prompt_for_paths(gpui::PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: false,
                        prompt: Some(tr!("选择要贴图的图片", "Choose an image to pin").into()),
                    })
                });
                cx.spawn(async move |cx| {
                    let result: std::result::Result<(), String> = async {
                        let chosen = if let Some(path) = initial_path {
                            Some(path)
                        } else {
                            receiver
                                .ok_or("文件选择任务缺失")?
                                .await
                                .map_err(|e| e.to_string())?
                                .map_err(|e| e.to_string())?
                                .and_then(|p| p.into_iter().next())
                        };
                        if let Some(path) = chosen {
                            let image = cx
                                .background_executor()
                                .spawn(async move {
                                    crate::export::read_image(&path).map_err(|e| e.to_string())
                                })
                                .await?;
                            cx.update(|cx| PinView::open(image, origin, cx))
                                .map_err(|e| e.to_string())?
                                .map_err(|e| e.to_string())?;
                        } else {
                            cx.update(|cx| cx.quit()).map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(e) = result {
                        *errors.borrow_mut() = Some(e);
                        let _ = cx.update(|cx| cx.quit());
                    }
                })
                .detach();
            } else {
                let result = (|| -> Result<()> {
                    if mode == "--pin-restore" {
                        crate::pin_workspace_runtime::restore_last_closed(cx)?;
                    } else {
                        match clipboard(cx)? {
                            ClipboardPin::Image(image, Some(placement))
                                if requested_origin.is_none() =>
                            {
                                open_placed(image, placement, cx)?;
                            }
                            ClipboardPin::Image(image, _) => PinView::open(image, origin, cx)?,
                            ClipboardPin::Text(source) => {
                                PinView::open_text_source(source, origin, cx)?;
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    *errors.borrow_mut() = Some(e.to_string());
                    cx.quit();
                }
            }
        });
    let result = final_errors.borrow_mut().take();
    if let Some(e) = result {
        Err(e.into())
    } else {
        Ok(())
    }
}
