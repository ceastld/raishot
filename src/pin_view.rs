//! 图片贴图宿主；只保存原图和当前预览，窗口操作与图像处理分别委托。
use crate::{
    capture::Image,
    gpui_capture::hwnd,
    ocr_markup::{MarkLedger, TextMark},
    pin_model::{
        self, CornerOverlayVisibility, PhysicalWindowGeometry, PinState, PinWindowSizing,
        Placement, ProcessingHistory, WireProcessingHistory,
    },
    pin_native as native, Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, rgba, size, App, AppContext, Bounds, Context,
    FocusHandle, MouseButton, Render, RenderImage, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use std::{sync::Arc, time::Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, VK_LBUTTON, VK_LSHIFT, VK_MBUTTON,
    VK_P, VK_RBUTTON, VK_RSHIFT, VK_SHIFT, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::*;
pub(crate) type TextSelectionStamp = (
    usize,
    Option<gpui::EntityId>,
    Option<u64>,
    Option<crate::geometry::Rect>,
    bool,
);
pub(crate) const MAX_PIN_WINDOW_SIDE: u32 = pin_model::MAX_PIN_WINDOW_SIDE;
const PIN_OPERATION_HINT_DURATION: std::time::Duration = std::time::Duration::from_millis(1500);
const PIN_NOTICE_FADE_IN: std::time::Duration = std::time::Duration::from_millis(120);
/// 淡出占用到期前的最后一段，不延长气泡的显示时长。
const PIN_NOTICE_FADE_OUT: std::time::Duration = std::time::Duration::from_millis(220);

pub(crate) struct CapturePinPresentation {
    pub record_history: bool,
    pub preserve_source_origin: bool,
    pub window_shadow_enabled: bool,
    pub style: Option<crate::selection_finish::SelectionFinishStyle>,
}

struct PinImageInput {
    image: Image,
    style: Option<crate::selection_finish::SelectionFinishStyle>,
    document: Option<crate::pin_raster_document::WireRasterDocument>,
    closed_window: Option<crate::pin_store::PresentationWindow>,
    processing_history: Option<WireProcessingHistory>,
    capture_pixels: Option<Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
}
impl From<Image> for PinImageInput {
    fn from(image: Image) -> Self {
        Self {
            image,
            style: None,
            document: None,
            closed_window: None,
            processing_history: None,
            capture_pixels: None,
        }
    }
}

#[path = "ocr_find_ui.rs"]
pub(crate) mod find_ui;
#[path = "pin_ocr_local_retry.rs"]
mod local_retry_host;
#[path = "pin_ocr_clipboard.rs"]
mod ocr_clipboard;
#[path = "pin_ocr_copy.rs"]
mod ocr_copy;
#[path = "pin_ocr_find.rs"]
mod ocr_find_host;
#[path = "pin_ocr_progress.rs"]
mod ocr_progress_host;
#[path = "pin_ocr_sensitive.rs"]
mod ocr_sensitive_host;
#[path = "pin_ocr_task.rs"]
mod ocr_task;
#[path = "pin_processing.rs"]
mod processing;
#[path = "pin_processing_noop.rs"]
pub(crate) mod processing_noop;
#[path = "ocr_table_copy_ui.rs"]
pub(crate) mod table_copy;
#[cfg(test)]
#[path = "pin_text_mark_tests.rs"]
mod text_mark_tests;
#[path = "pin_text_tools.rs"]
mod text_tools;

fn text_mark_history_eligible(
    before: Option<TextSelectionStamp>,
    current: TextSelectionStamp,
    underlay_matches: bool,
) -> bool {
    before.is_some_and(|before| {
        underlay_matches
            && before.0 == current.0
            && before.1 == current.1
            && before.3 == current.3
            && before.4 == current.4
    })
}

fn window_zoom(image: (u32, u32), requested: f32) -> f32 {
    let maximum = (MAX_PIN_WINDOW_SIDE as f32 / image.0.max(1) as f32)
        .min(MAX_PIN_WINDOW_SIDE as f32 / image.1.max(1) as f32)
        .min(5.);
    let requested = if requested.is_finite() { requested } else { 1. };
    requested.clamp(0.0001, maximum)
}

fn presentation_desktop() -> Placement {
    unsafe {
        Placement {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}
fn closed_pin_geometry(
    window: crate::pin_store::PresentationWindow,
    zoom: f32,
) -> Result<(PinWindowSizing, f32, Placement)> {
    window.validate()?;
    if !zoom.is_finite() || !(0.0001..=8.).contains(&zoom) {
        return Err("贴图关闭恢复倍率无效".into());
    }
    Ok((window.sizing, zoom, window.placement))
}
fn initial_pin_geometry(
    image: (u32, u32),
    requested_zoom: f32,
    work: Placement,
    preserve_source_origin: bool,
    restored: Option<Placement>,
) -> (PinWindowSizing, f32, (u32, u32)) {
    let mut zoom = window_zoom(
        image,
        if preserve_source_origin {
            1.
        } else {
            requested_zoom
        },
    );
    let sizing = if preserve_source_origin {
        PinWindowSizing::ExactCapture
    } else {
        restored.map_or(PinWindowSizing::General, |placement| {
            PinWindowSizing::restored(image, zoom, placement)
        })
    };
    if restored.is_none()
        && sizing == PinWindowSizing::General
        && (image.0 > MAX_PIN_WINDOW_SIDE || image.1 > MAX_PIN_WINDOW_SIDE)
    {
        // 普通长图保留工作区适应；截图仅遵守 GPU 上限，不再自动缩到工作区。
        zoom = zoom.min(pin_model::fit(image, work));
    }
    let restored = restored.filter(|p| {
        (1..=MAX_PIN_WINDOW_SIDE).contains(&p.width)
            && (1..=MAX_PIN_WINDOW_SIDE).contains(&p.height)
    });
    let dimensions = restored.map_or_else(|| sizing.size(image, zoom), |p| (p.width, p.height));
    if restored.is_some() && requested_zoom.is_finite() && (0.0001..=8.).contains(&requested_zoom) {
        // 已保存倍率和矩形分别保留；min24 留白的极小图实际投影可大于合法保存倍率。
        zoom = requested_zoom;
    }
    (sizing, zoom, dimensions)
}

// 与编辑画布共用客户区物理投影，避免 Contain 的 DIP 浮点误差再被 floor/ceil 放大成 1px。
#[cfg(test)]
fn image_projection(
    image: (u32, u32),
    logical_host: (f32, f32),
    dpi: f32,
) -> crate::viewport::Viewport {
    crate::pin_zoom::projection(
        image,
        (
            (logical_host.0 * dpi).round() as u32,
            (logical_host.1 * dpi).round() as u32,
        ),
    )
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum PinAction {
    Workspace,
    Copy,
    Save,
    QuickSave,
    Edit,
    Crop,
    Duplicate,
    Zoom(f32),
    WheelAnchor(crate::pin_zoom::ZoomAnchor),
    Fit,
    Original,
    Opacity(u8),
    Thumbnail,
    Topmost,
    Shadow,
    Annotations,
    Lock,
    ClickThrough,
    Hide,
    Close,
    Chrome,
    Rotate(i8),
    FlipX,
    FlipY,
    Gray,
    Invert,
    Brightness(i16),
    ResetImage,
    Undo,
    Redo,
    Text,
    Ocr,
    Qr,
    CopyText,
    CopySelectedText,
    CopyParagraphText,
    CopyTableText,
    MarkText(TextMark),
    TextMarkColor(u32),
    ClearTextSelection,
    Color,
    /// 原地编辑可视标题（右键菜单；工具栏收起时 F2）。
    EditTitle,
}
struct PinImages {
    original: Arc<RenderImage>,
    image: Arc<RenderImage>,
    original_size: (u32, u32),
    image_size: (u32, u32),
    /// Some 时 original/image 只是占位；恢复保留完整文档而非重造插入历史。
    text: Option<crate::pin_text_source::TextInstallation>,
    style: Option<crate::selection_finish::SelectionFinishStyle>,
    document: Option<crate::pin_raster_document::WireRasterDocument>,
    processing_history: Option<WireProcessingHistory>,
    capture_pixels: Option<Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
}
pub(crate) struct TextToolHint(pub(crate) &'static str);
impl Render for TextToolHint {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::capture_toolbar::ui_kit::tip_body(crate::theme::palette(cx), self.0)
    }
}
pub struct PinView {
    pub(crate) original: Arc<RenderImage>,
    pub(crate) image: Arc<RenderImage>,
    pub(crate) original_size: (u32, u32),
    pub(crate) image_size: (u32, u32),
    pub(crate) presentation_style: Option<crate::selection_finish::SelectionFinishStyle>,
    pub(crate) capture_pixels:
        Option<Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
    pub(crate) capture_source:
        Option<crate::pin_raster_document::capture_source::CaptureSourceMetadata>,
    /// 矢量文字贴图当前应用的主题（true 为浅色）；None 为普通位图贴图。
    pub(crate) vector_text: Option<bool>,
    pub(crate) state: PinState,
    pub(crate) history: ProcessingHistory,
    pub(crate) focus: FocusHandle,
    pub(crate) status: String,
    status_notice_text: String,
    status_notice_deadline: Option<Instant>,
    /// 左上角气泡的操作反馈：由可见状态变化生成，与状态消息共用位置，最新一条覆盖。
    operation_hint: String,
    operation_hint_deadline: Option<Instant>,
    operation_hint_state: Option<pin_model::OperationHintState>,
    /// 气泡共用一个定时器；出现更早的到期时间时换代重排。
    notice_timer_wake: Option<Instant>,
    notice_timer_generation: u64,
    /// 气泡本次从隐藏变为可见的时刻，用于淡入。
    notice_shown_at: Option<Instant>,
    pub(crate) hovered: bool,
    corner_overlays: CornerOverlayVisibility,
    status_hovered: bool,
    pub(crate) chrome: bool,
    pub(crate) drop_target: Option<windows::Win32::System::Ole::IDropTarget>,
    pub(crate) shadow: Option<crate::pin_shadow::PinShadow>,
    pub(crate) toolbar: Option<WindowHandle<crate::pin_toolbar::PinToolbar>>,
    pub(crate) context_menu: Option<WindowHandle<crate::pin_menu::PinMenu>>,
    pub(crate) context_menu_press: bool,
    pub(crate) canvas: Option<gpui::Entity<crate::capture_view::CaptureView>>,
    pub(crate) annotation_preview: crate::pin_annotation::AnnotationPreviewVisibility,
    pub(crate) canvas_subscription: Option<gpui::Subscription>,
    pub(crate) preview_lifecycle: crate::pin_preview_lifecycle::PreviewLifecycle,
    pub(crate) gesture: Option<crate::pin_interaction::Gesture>,
    pub(crate) post_drag_click: crate::pin_interaction::PostDragClick,
    pub(crate) drag_request: Option<crate::pin_drag_request::NativeDragRequest>,
    pub(crate) undo_edits: Vec<crate::pin_annotation::Checkpoint>,
    pub(crate) redo_edits: Vec<crate::pin_annotation::Checkpoint>,
    pub(crate) workspace: Option<crate::pin_workspace_runtime::LivePin>,
    pub(crate) pin_title: crate::pin_title::PinTitleState,
    persisted: bool,
    pub(crate) processing: bool,
    pub(crate) processing_image_unavailable: bool,
    processing_request: u64,
    ocr_task: ocr_task::TaskSlot<TextSelectionStamp>,
    ocr_progress: ocr_progress_host::ProgressState,
    processing_probe: processing::ProcessingProbe,
    processing_force_borrowed_geometry: bool,
    pub(crate) saving: bool,
    pending_placement: Option<Placement>,
    top_left_zoom_anchor: Option<crate::pin_zoom::TopLeftAnchor>,
    pointer_zoom_anchor: Option<crate::pin_zoom::PointerAnchor>,
    presentation_geometry_stamp: Option<(Placement, Placement)>,
    presentation_geometry_failed: Option<(Placement, Placement)>,
    pub(crate) presentation_geometry_adjusting: bool,
    physical_geometry: PhysicalWindowGeometry,
    placement_revision: u64,
    pub(crate) window_sizing: PinWindowSizing,
    last_scale_factor: f32,
    wheel: crate::interaction::WheelAccumulator,
    pub(crate) pin_zoom_anchor: crate::pin_zoom::ZoomAnchor,
    wheel_settings: (f32, f32),
    pub(crate) text: Option<Arc<crate::recognition::RecognizedText>>,
    pin_local_ocr: Option<local_retry_host::LocalOcrState>,
    pending_pin_local_input: Option<local_retry_host::FixedInput>,
    text_source: ocr_clipboard::TextSource,
    pub(crate) text_mode: bool,
    text_selection: Option<crate::pin_text_selection::SelectionText>,
    text_selection_stamp: Option<TextSelectionStamp>,
    copy_layout: Option<ocr_copy::ParagraphCopy<TextSelectionStamp>>,
    copy_layout_generation: u64,
    table_copy: Option<table_copy::TableCopy<TextSelectionStamp>>,
    ocr_find: Option<find_ui::FindUi<TextSelectionStamp>>,
    ocr_find_painted: std::rc::Rc<std::cell::RefCell<Option<(TextSelectionStamp, u64)>>>,
    ocr_find_click: std::rc::Rc<std::cell::RefCell<ocr_find_host::PinFindClickState>>,
    ocr_sensitive: ocr_sensitive_host::SensitiveState,
    selected: Option<std::ops::Range<usize>>,
    text_keyboard_selection: Option<crate::pin_text_selection::KeyboardSelection>,
    text_marks: MarkLedger,
    text_mark_color: Option<u32>,
    text_show_selection: bool,
    text_tools_ready: bool,
    text_anchor: Option<crate::pin_text_selection::SelectionAnchor>,
    text_capture: Option<windows::Win32::Foundation::HWND>,
    pub(crate) color_pick: bool,
    color_pick_alt: crate::capture_view::GuideToggleGesture,
    color_display: crate::pin_color_display::ColorDisplay,
    color_keys_task: Option<gpui::Task<()>>,
    color_magnifier: Option<WindowHandle<crate::pin_color_magnifier::PinColorMagnifier>>,
    color: String,
}
pub(crate) fn preview(image: Image) -> Result<Arc<RenderImage>> {
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(
        image::RgbaImage::from_raw(image.width, image.height, image.bgra)
            .ok_or(tr!("贴图尺寸错误", "Invalid pin size"))?,
    )])))
}
impl PinView {
    pub fn open(image: Image, origin: (i32, i32), cx: &mut App) -> Result<()> {
        let state = PinState {
            zoom: pin_model::fit((image.width, image.height), native::work_area(origin)),
            ..Default::default()
        };
        Self::open_state(image, origin, state, cx).map(|_| ())
    }
    /// 剪贴板截图已包含外观留白，原点就是导出像素左上角；不再适应单屏或挪回工作区。
    pub(crate) fn open_copied_capture(
        image: Image,
        origin: (i32, i32),
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let bounds = windows::Win32::Foundation::RECT {
            left: origin.0,
            top: origin.1,
            right: origin
                .0
                .saturating_add(image.width.min(i32::MAX as u32) as i32),
            bottom: origin
                .1
                .saturating_add(image.height.min(i32::MAX as u32) as i32),
        };
        let visible = !unsafe {
            windows::Win32::Graphics::Gdi::MonitorFromRect(
                &bounds,
                windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONULL,
            )
        }
        .is_invalid();
        let state = PinState {
            zoom: if visible {
                1.
            } else {
                pin_model::fit((image.width, image.height), native::work_area(origin))
            },
            // 截图外观已烘焙，不能再给透明外沿加一层矩形窗口阴影。
            shadow_enabled: false,
            ..Default::default()
        };
        Self::open_state_with_completion(image.into(), origin, state, None, visible, None, cx)
    }
    pub(crate) fn open_with_history(image: Image, origin: (i32, i32), cx: &mut App) -> Result<()> {
        Self::open_with_history_window(image, origin, cx).map(|_| ())
    }
    pub(crate) fn open_with_history_window(
        image: Image,
        origin: (i32, i32),
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let state = PinState {
            zoom: pin_model::fit((image.width, image.height), native::work_area(origin)),
            ..Default::default()
        };
        let window = Self::open_state(image, origin, state, cx)?;
        Self::record_history(window, cx)?;
        Ok(window)
    }
    /// 创建时仍保持隐藏；原截图的关闭和锁释放只能由首显完成回调决定。
    /// 回调在原生操作和 App / Window 借用结束后执行，失败保留调用方的截图。
    pub(crate) fn open_capture_handoff(
        payload: crate::capture_pin_output::document_handoff::CaptureDocumentPayload,
        origin: (i32, i32),
        presentation: CapturePinPresentation,
        capture: gpui::AnyWindowHandle,
        capture_hwnd: windows::Win32::Foundation::HWND,
        completion: impl FnOnce(
                std::result::Result<native::PinVisibleReady, String>,
                &mut gpui::AsyncApp,
            ) -> bool
            + 'static,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let (image, _, pixels, document) = payload.into_parts();
        let state = PinState {
            zoom: if presentation.preserve_source_origin {
                1.
            } else {
                pin_model::fit(
                    crate::pin_presentation_host::window_size(
                        (image.width, image.height),
                        presentation.style,
                    )?,
                    native::work_area(origin),
                )
            },
            shadow_enabled: presentation.window_shadow_enabled,
            ..Default::default()
        };
        let capture_owner = native::CaptureHandoffOwner::new(capture, capture_hwnd)?;
        let completed = Box::new(
            move |result: std::result::Result<native::PinVisibleReady, String>,
                  cx: &mut gpui::AsyncApp| {
                let pin = result
                    .as_ref()
                    .ok()
                    .and_then(|ready| ready.window.downcast::<Self>());
                let accepted = completion(result, cx);
                // 登记/落盘在真实首显和原截图交接之后；本回调不让出事件循环，
                // 工作区关闭合同在下一次输入前接好，关闭原截图无需等待历史。
                // 调用方取消时可以先 discard；已经回收的本次 Pin 不再生成历史。
                if accepted {
                    if let Some(pin) = pin {
                        let history = cx.update(|cx| {
                            if pin.read(cx).is_err() {
                                return Ok(());
                            }
                            pin.update(cx, |view, window, cx| {
                                view.attach_workspace_or_report(None, window, cx)
                            })??;
                            if presentation.record_history {
                                Self::record_history(pin, cx)
                            } else {
                                Ok(())
                            }
                        });
                        let error = match history {
                            Ok(Ok(())) => None,
                            Ok(Err(error)) => Some(error.to_string()),
                            Err(error) => Some(error.to_string()),
                        };
                        if let Some(error) = error {
                            let _ = pin.update(cx, |view, _, cx| {
                                view.status = trf!(
                                    "历史记录启动失败：{error}；贴图仍可使用。",
                                    "History failed to start: {error}; the pin still works."
                                );
                                cx.notify();
                            });
                        }
                    }
                }
            },
        );
        Self::open_state_with_completion(
            PinImageInput {
                image,
                style: presentation.style,
                document: Some(document),
                closed_window: None,
                processing_history: None,
                capture_pixels: Some(pixels),
            },
            origin,
            state,
            None,
            presentation.preserve_source_origin,
            Some((capture_owner, completed)),
            cx,
        )
    }
    fn record_history(window: WindowHandle<Self>, cx: &mut App) -> Result<()> {
        if crate::preferences::Preferences::load().history {
            let content = if window.read(cx)?.capture_source.is_some() {
                Some(window.update(cx, |view, _, cx| -> Result<_> {
                    view.user_export_snapshot(cx)?.content_only()
                })??)
            } else {
                None
            };
            let (source, (width, height)) = {
                let view = window.read(cx)?;
                (view.original.clone(), view.original_size)
            };
            // 成功创建窗口后只共享不可变原帧；后台记录无需复制一份长图。
            let task = cx.background_executor().spawn(async move {
                if let Some(content) = content {
                    let image = content.render().map_err(|error| error.to_string())?;
                    return crate::history::record_pixels(image.width, image.height, &image.bgra)
                        .map_err(|error| error.to_string());
                }
                let bytes = source.as_bytes(0).ok_or("贴图历史原始帧缺失")?;
                crate::history::record_pixels(width, height, bytes)
                    .map_err(|error| error.to_string())
            });
            cx.spawn(async move |cx| {
                if let Err(error) = task.await {
                    let _ = window.update(cx, |view, _, cx| {
                        view.status = trf!(
                            "历史记录保存失败：{error}；贴图仍可使用。",
                            "Failed to save history: {error}; the pin still works."
                        );
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        Ok(())
    }
    pub(crate) fn open_state(
        image: Image,
        origin: (i32, i32),
        state: PinState,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_state_with(image, origin, state, None, cx)
    }
    pub(crate) fn open_session(
        image: Image,
        mut session: crate::pin_workspace::Session,
        entry: crate::pin_workspace::PinEntry,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        if let Some(document) = &entry.text_document {
            document.validate()?;
            if (image.width, image.height) != (document.width, document.height) {
                return Err(tr!(
                    "文字贴图恢复数据与图片尺寸不一致",
                    "Text pin restore data doesn't match the image size"
                )
                .into());
            }
            // 渲染快照只作兜底；文字贴图按矢量文档恢复，不保留底图像素。
            let size = (document.width, document.height);
            let document = document.clone();
            drop(image);
            let origin = (entry.placement.x, entry.placement.y);
            let state = entry.state.clone();
            return Self::open_vector_text_with(
                size,
                crate::pin_text_source::TextInstallation::Restore(Box::new(document)),
                origin,
                state,
                Some((session, entry)),
                None,
                cx,
            );
        }
        let input = PinImageInput {
            image,
            style: entry.presentation_style,
            document: entry.raster_document.clone(),
            closed_window: None,
            processing_history: entry.processing_history.clone(),
            capture_pixels: session.capture_pixels.take(),
        };
        Self::open_state_with_completion(
            input,
            (entry.placement.x, entry.placement.y),
            entry.state.clone(),
            Some((session, entry)),
            false,
            None,
            cx,
        )
    }
    fn open_state_with(
        image: Image,
        origin: (i32, i32),
        state: PinState,
        restored: Option<(
            crate::pin_workspace::Session,
            crate::pin_workspace::PinEntry,
        )>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_state_with_completion(image.into(), origin, state, restored, false, None, cx)
    }
    /// 矢量文字贴图：没有底图位图，画布绘制主题纯色底，输出时才渲染。
    pub(crate) fn open_vector_text(
        size: (u32, u32),
        annotations: Vec<crate::annotation::Annotation>,
        origin: (i32, i32),
        state: PinState,
        restored: Option<(
            crate::pin_workspace::Session,
            crate::pin_workspace::PinEntry,
        )>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_vector_text_with(
            size,
            crate::pin_text_source::TextInstallation::New(annotations),
            origin,
            state,
            restored,
            None,
            cx,
        )
    }
    pub(crate) fn open_vector_text_with(
        size: (u32, u32),
        text: crate::pin_text_source::TextInstallation,
        origin: (i32, i32),
        mut state: PinState,
        restored: Option<(
            crate::pin_workspace::Session,
            crate::pin_workspace::PinEntry,
        )>,
        closed_window: Option<crate::pin_store::PresentationWindow>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        crate::pin_text_source::validate_size(size.0, size.1)?;
        if matches!(&text, crate::pin_text_source::TextInstallation::Restore(_))
            && state.adjustments != Default::default()
        {
            return Err("文字文档恢复不支持图像处理参数".into());
        }
        // 文字矢量文档不承载像素处理参数；旋转等处理会先渲染成普通贴图。
        state.adjustments = Default::default();
        let placeholder = preview(crate::pin_text_source::placeholder())?;
        let work = native::work_area(origin);
        let (window_sizing, zoom, placement) = if let Some(window) = closed_window {
            closed_pin_geometry(window, state.zoom)?
        } else {
            let (sizing, zoom, dimensions) = initial_pin_geometry(
                size,
                state.zoom,
                work,
                false,
                restored.as_ref().map(|(_, entry)| entry.placement),
            );
            let placement = pin_model::initial_placement(
                Placement {
                    x: origin.0,
                    y: origin.1,
                    width: dimensions.0,
                    height: dimensions.1,
                },
                work,
                false,
            );
            // 工作区记录明确保存窗口政策；旧缺字段为 General，不凭尺寸猜来源。
            let sizing = restored
                .as_ref()
                .map_or(sizing, |(_, entry)| entry.window_sizing);
            (sizing, zoom, placement)
        };
        state.zoom = zoom;
        Self::open_shared(
            PinImages {
                original: placeholder.clone(),
                image: placeholder,
                original_size: size,
                image_size: size,
                text: Some(text),
                style: None,
                document: None,
                processing_history: None,
                capture_pixels: None,
            },
            state,
            placement,
            window_sizing,
            restored,
            None,
            cx,
        )
    }
    pub(crate) fn open_raster_document(
        image: Image,
        origin: (i32, i32),
        state: PinState,
        style: impl Into<Option<crate::selection_finish::SelectionFinishStyle>>,
        document: crate::pin_raster_document::WireRasterDocument,
        processing_history: Option<WireProcessingHistory>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_state_with_completion(
            PinImageInput {
                image,
                style: style.into(),
                document: Some(document),
                closed_window: None,
                processing_history,
                capture_pixels: None,
            },
            origin,
            state,
            None,
            false,
            None,
            cx,
        )
    }
    /// 有界关闭记录携带完整客户区；恢复不重新 Fit 或套普通最小尺寸。
    pub(crate) fn open_closed_raster_document(
        image: Image,
        state: PinState,
        style: impl Into<Option<crate::selection_finish::SelectionFinishStyle>>,
        document: crate::pin_raster_document::WireRasterDocument,
        window: crate::pin_store::PresentationWindow,
        processing_history: Option<WireProcessingHistory>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        Self::open_state_with_completion(
            PinImageInput {
                image,
                style: style.into(),
                document: Some(document),
                closed_window: Some(window),
                processing_history,
                capture_pixels: None,
            },
            (window.placement.x, window.placement.y),
            state,
            None,
            false,
            None,
            cx,
        )
    }
    pub(crate) fn open_restored_capture_document(
        image: Image,
        pixels: Arc<crate::pin_raster_document::capture_source::CapturePixels>,
        document: crate::pin_raster_document::WireRasterDocument,
        restore: (
            PinState,
            (i32, i32),
            Option<crate::pin_store::PresentationWindow>,
        ),
        style: Option<crate::selection_finish::SelectionFinishStyle>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let (state, origin, closed_window) = restore;
        Self::open_state_with_completion(
            PinImageInput {
                image,
                style,
                document: Some(document),
                closed_window,
                processing_history: None,
                capture_pixels: Some(pixels),
            },
            origin,
            state,
            None,
            false,
            None,
            cx,
        )
    }
    fn open_state_with_completion(
        image: PinImageInput,
        origin: (i32, i32),
        mut state: PinState,
        restored: Option<(
            crate::pin_workspace::Session,
            crate::pin_workspace::PinEntry,
        )>,
        preserve_source_origin: bool,
        completion: Option<(native::CaptureHandoffOwner, native::InitialPinCompletion)>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let PinImageInput {
            image,
            style,
            document,
            closed_window,
            processing_history,
            capture_pixels,
        } = image;
        if image.width == 0
            || image.height == 0
            || image.width > 65_535
            || image.height > 65_535
            || image.width as u64 * image.height as u64 > 32_000_000
            || image.bgra.len() != image.width as usize * image.height as usize * 4
        {
            return Err(tr!(
                "贴图尺寸无效或超过 3200 万像素 / 65535 单边限制",
                "Pin size is invalid or exceeds the 32-megapixel / 65535-per-side limit"
            )
            .into());
        }
        if let Some(style) = style {
            crate::pin_workspace::validate_presentation_style(style)?;
        }
        if let Some(document) = &document {
            document.validate()?;
        }
        let closed_geometry = closed_window
            .map(|window| closed_pin_geometry(window, state.zoom))
            .transpose()?;
        let original_size = (image.width, image.height);
        let capture_source = document
            .as_ref()
            .and_then(|doc| doc.capture_source())
            .cloned();
        match (&capture_source, &capture_pixels) {
            (Some(source), Some(pixels))
                if source.matches(pixels)
                    && source.storage_size() == original_size
                    && state.adjustments == Default::default()
                    && processing_history.is_none() =>
            {
                pixels.validate()?;
                if !pixels.content_matches(source.content_rect(), &image.bgra) {
                    return Err("截图贴图兼容帧与不可变来源像素不一致".into());
                }
            }
            (None, None) => {}
            _ => return Err("截图贴图完整来源、兼容帧与文档不匹配".into()),
        }
        let image_size = capture_source.as_ref().map_or_else(
            || state.adjustments.dimensions(image.width, image.height),
            |source| (source.bounds().width, source.bounds().height),
        );
        let processed = if state.adjustments == Default::default() {
            None
        } else {
            Some(preview(state.adjustments.apply(&image)?)?)
        };
        let original = preview(image)?;
        let image = processed.unwrap_or_else(|| original.clone());
        let work = native::work_area(origin);
        let content_size = capture_source
            .as_ref()
            .map_or(image_size, |source| source.storage_size());
        let outer_size = crate::pin_presentation_host::window_size(content_size, style)?;
        if let Some(document) = &document {
            if document.size() != image_size {
                return Err("贴图文档与处理后的内容尺寸不一致".into());
            }
        }
        let (window_sizing, zoom, placement) = if let Some(geometry) = closed_geometry {
            geometry
        } else {
            let (sizing, zoom, dimensions) = initial_pin_geometry(
                outer_size,
                state.zoom,
                work,
                preserve_source_origin,
                restored.as_ref().map(|(_, entry)| entry.placement),
            );
            let placement = pin_model::initial_placement(
                Placement {
                    x: origin.0,
                    y: origin.1,
                    width: dimensions.0,
                    height: dimensions.1,
                },
                work,
                preserve_source_origin,
            );
            // 工作区记录明确保存窗口政策；旧缺字段为 General，不凭尺寸猜来源。
            let sizing = restored
                .as_ref()
                .map_or(sizing, |(_, entry)| entry.window_sizing);
            (sizing, zoom, placement)
        };
        state.zoom = zoom;
        Self::open_shared(
            PinImages {
                original,
                image,
                original_size,
                image_size,
                text: None,
                style,
                document,
                processing_history,
                capture_pixels,
            },
            state,
            placement,
            window_sizing,
            restored,
            completion,
            cx,
        )
    }
    fn attach_workspace_or_report(
        &mut self,
        restored: Option<(
            crate::pin_workspace::Session,
            crate::pin_workspace::PinEntry,
        )>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let restoring = restored.is_some();
        if let Err(error) = self.attach_workspace(restored, window, cx) {
            self.workspace = None;
            unsafe {
                let _ = RemovePropW(
                    hwnd(window),
                    windows::core::w!("RustShot.Pin.WorkspaceScope"),
                );
                let _ = RemovePropW(hwnd(window), windows::core::w!("RustShot.Pin.WorkspaceId"));
                let _ = RemovePropW(
                    hwnd(window),
                    windows::core::w!("RustShot.Pin.WorkspaceRefresh"),
                );
            }
            if restoring {
                return Err(error);
            }
            self.status = trf!(
                "工作区登记失败：{error}；本窗口仍可使用。",
                "Failed to register with the workspace: {error}; this window still works."
            );
            eprintln!("{}", self.status);
            cx.notify();
        }
        Ok(())
    }
    fn open_shared(
        images: PinImages,
        state: PinState,
        p: Placement,
        window_sizing: PinWindowSizing,
        restored: Option<(
            crate::pin_workspace::Session,
            crate::pin_workspace::PinEntry,
        )>,
        completion: Option<(native::CaptureHandoffOwner, native::InitialPinCompletion)>,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        let PinImages {
            original,
            image,
            original_size,
            image_size,
            text,
            style,
            document,
            processing_history,
            capture_pixels,
        } = images;
        let capture_source = document
            .as_ref()
            .and_then(|doc| doc.capture_source())
            .cloned();
        match (&capture_source, &capture_pixels) {
            (Some(source), Some(pixels)) if source.matches(pixels) => {
                source.validate()?;
                pixels.validate()?;
                if original_size != source.storage_size()
                    || image_size != (source.bounds().width, source.bounds().height)
                    || state.adjustments != Default::default()
                    || processing_history.is_some()
                    || text.is_some()
                    || original.frame_count() != 1
                    || image.frame_count() != 1
                    || !original
                        .as_bytes(0)
                        .is_some_and(|bytes| pixels.content_matches(source.content_rect(), bytes))
                    || !Arc::ptr_eq(&original, &image)
                        && !image.as_bytes(0).is_some_and(|bytes| {
                            pixels.content_matches(source.content_rect(), bytes)
                        })
                {
                    return Err("贴图共享安装未保留真实原图、完整采样域和文档来源".into());
                }
            }
            (None, None) => {}
            _ => return Err("贴图文档来源上下文未完整安装".into()),
        }
        let content_size = capture_source
            .as_ref()
            .map_or(image_size, |source| source.storage_size());
        let mut history = ProcessingHistory::default();
        if let Some(record) = processing_history {
            if text.is_some() {
                return Err("纯文字来源不能安装图像处理历史".into());
            }
            if original.frame_count() != 1 || image.frame_count() != 1 {
                return Err("贴图处理历史的原图或当前帧格式无效".into());
            }
            let original_frame = original.size(0);
            let current_frame = image.size(0);
            if (
                u32::from(original_frame.width),
                u32::from(original_frame.height),
            ) != original_size
                || (
                    u32::from(current_frame.width),
                    u32::from(current_frame.height),
                ) != image_size
            {
                return Err("贴图处理历史的原图或当前帧尺寸不一致".into());
            }
            record.restore_into(state.adjustments, original_size, image_size, &mut history)?;
        }
        if let Some(style) = style {
            crate::pin_presentation::validate_preview_geometry_with_sizing(
                content_size,
                style,
                p,
                presentation_desktop(),
                window_sizing,
            )?;
        }
        let vector_text = text.as_ref().map(|_| crate::theme::is_light(cx));
        let restoring = restored.is_some();
        let show = match restored.as_ref() {
            Some((_, entry)) => {
                !entry.hidden && crate::pin_workspace::load()?.current_group == entry.group_id
            }
            None => true,
        };
        let (display_id, initial_bounds) = native::initial_pin_bounds(p, cx);
        crate::pin_handoff_smoke::mark("pin_gpui_window_begin");
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(initial_bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                // HWND 样式、物理尺寸和首帧尚未接入，禁止平台先显示通用窗口。
                show: false,
                focus: false,
                is_resizable: false,
                is_minimizable: false,
                // 普通 PNG、处理后的图片与截图外观都保留逐像素 alpha。
                // 创建时统一使用透明合成，后续编辑不切换交换链或重建 HWND。
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                titlebar: Some(gpui::TitlebarOptions {
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(brand_title!("贴图", "Pin"));
                let h = hwnd(window);
                unsafe {
                    SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
                    let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
                    SetWindowLongPtrW(h, GWL_EXSTYLE, style | WS_EX_TOOLWINDOW.0 as isize);
                    let _ = SetWindowPos(
                        h,
                        None,
                        p.x,
                        p.y,
                        p.width as i32,
                        p.height as i32,
                        SWP_NOZORDER | SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_HIDEWINDOW,
                    );
                }
                let _ = native::mark(h);
                let _ = native::topmost(h, state.topmost);
                let _ = native::opacity(h, state.opacity);
                let focus = cx.focus_handle();
                window.focus(&focus);
                let physical_geometry = PhysicalWindowGeometry::new(
                    crate::pin_presentation_host::window_size(content_size, style)
                        .expect("创建前已校验外观尺寸"),
                    state.zoom,
                    (p.width, p.height),
                );
                let view = cx.new(|_| Self {
                    original,
                    image,
                    original_size,
                    image_size,
                    presentation_style: style,
                    capture_pixels,
                    capture_source,
                    vector_text,
                    state,
                    history,
                    focus,
                    status: String::new(),
                    status_notice_text: String::new(),
                    status_notice_deadline: None,
                    operation_hint: String::new(),
                    operation_hint_deadline: None,
                    operation_hint_state: None,
                    notice_timer_wake: None,
                    notice_timer_generation: 0,
                    notice_shown_at: None,
                    hovered: false,
                    corner_overlays: Default::default(),
                    status_hovered: false,
                    chrome: false,
                    drop_target: None,
                    shadow: None,
                    toolbar: None,
                    context_menu: None,
                    context_menu_press: false,
                    canvas: None,
                    annotation_preview: Default::default(),
                    canvas_subscription: None,
                    preview_lifecycle: Default::default(),
                    gesture: None,
                    post_drag_click: Default::default(),
                    drag_request: None,
                    undo_edits: Vec::new(),
                    redo_edits: Vec::new(),
                    workspace: None,
                    pin_title: Default::default(),
                    persisted: false,
                    processing: false,
                    processing_image_unavailable: false,
                    processing_request: 0,
                    ocr_task: Default::default(),
                    ocr_progress: Default::default(),
                    processing_probe: Default::default(),
                    processing_force_borrowed_geometry: false,
                    saving: false,
                    pending_placement: None,
                    top_left_zoom_anchor: None,
                    pointer_zoom_anchor: None,
                    presentation_geometry_stamp: None,
                    presentation_geometry_failed: None,
                    presentation_geometry_adjusting: false,
                    physical_geometry,
                    placement_revision: 0,
                    window_sizing,
                    last_scale_factor: window.scale_factor(),
                    wheel: Default::default(),
                    pin_zoom_anchor: crate::preferences::Preferences::load().pin_zoom_anchor,
                    wheel_settings: crate::capture_interaction::wheel_settings(),
                    text: None,
                    pin_local_ocr: None,
                    pending_pin_local_input: None,
                    text_source: ocr_clipboard::TextSource::None,
                    text_mode: false,
                    text_selection: None,
                    text_selection_stamp: None,
                    copy_layout: None,
                    copy_layout_generation: 0,
                    table_copy: None,
                    ocr_find: None,
                    ocr_find_painted: Default::default(),
                    ocr_find_click: Default::default(),
                    ocr_sensitive: Default::default(),
                    selected: None,
                    text_keyboard_selection: None,
                    text_marks: MarkLedger::default(),
                    text_mark_color: None,
                    text_show_selection: true,
                    text_tools_ready: false,
                    text_anchor: None,
                    text_capture: None,
                    color_pick: false,
                    color_pick_alt: Default::default(),
                    color_display: Default::default(),
                    color_keys_task: None,
                    color_magnifier: None,
                    color: String::new(),
                });
                let weak = view.downgrade();
                view.update(cx, |_, cx| {
                    cx.on_release(|v, cx| {
                        // 原生回收、初始化失败及工作区异步关闭也只取消本贴图的任务。
                        v.cancel_ocr_task();
                        v.end_color_pick(cx);
                        v.destroy_context_menu(cx);
                    })
                    .detach();
                    cx.observe_window_activation(window, |v, w, cx| {
                        if w.is_window_active() {
                            v.reload_zoom_anchor();
                        } else {
                            v.clear_pin_ocr_progress_clock();
                            v.cancel_pin_find_click();
                            v.cancel_pin_local_intent("贴图失焦，撤销局部候选");
                            v.color_pick_alt.reset_tracking();
                            v.end_color_pick(cx);
                            v.finish_text_selection_drag();
                            v.context_menu_press = false;
                            v.dismiss_context_menu(cx);
                        }
                        cx.notify();
                    })
                    .detach();
                });
                window.on_window_should_close(cx, move |w, cx| {
                    weak.update(cx, |v, cx| {
                        v.end_color_pick(cx);
                        if v.workspace.is_some() {
                            if !v.workspace_close(w, cx) {
                                return false;
                            }
                            v.cancel_ocr_task();
                            v.close_toolbar(cx);
                            return true;
                        }
                        if v.saving {
                            return false;
                        }
                        if let Err(error) = v.persist(w, cx) {
                            v.status = error.to_string();
                            cx.notify();
                            return false;
                        }
                        v.cancel_ocr_task();
                        v.close_toolbar(cx);
                        true
                    })
                    .unwrap_or(true)
                });
                view
            },
        )?;
        crate::pin_handoff_smoke::mark("pin_gpui_window_end");
        let initialized = handle.update(cx, |v, w, cx| -> Result<()> {
            crate::pin_native::begin_initial_pin(hwnd(w), show)?;
            crate::pin_handoff_smoke::mark("pin_shadow_begin");
            match crate::pin_shadow::PinShadow::new(
                hwnd(w),
                v.state.shadow_enabled,
                crate::theme::palette(cx).muted,
            ) {
                Ok(shadow) => v.shadow = Some(shadow),
                Err(error) => eprintln!("贴图阴影创建失败：{error}"),
            }
            crate::pin_handoff_smoke::mark("pin_shadow_end");
            crate::pin_handoff_smoke::mark("pin_drop_begin");
            v.drop_target = Some(crate::pin_drop::install(handle, hwnd(w), cx)?);
            crate::pin_handoff_smoke::mark("pin_drop_end");
            crate::pin_handoff_smoke::mark("pin_workspace_begin");
            if let Some(text) = text {
                // 首显前安装；恢复不经 insert，空 current 也保留纯色来源与 Redo。
                match text {
                    crate::pin_text_source::TextInstallation::New(annotations) => {
                        v.install_text_annotations(annotations, w, cx)?;
                    }
                    crate::pin_text_source::TextInstallation::Restore(document) => {
                        v.install_text_document(&document, w, cx)?;
                    }
                }
            }
            if let Some(document) = document {
                v.ensure_document_canvas(w, cx)?;
                if let Some(canvas) = &v.canvas {
                    canvas.update(cx, |view, cx| -> Result<()> {
                        document.restore_into(&mut view.editor)?;
                        view.scene_stamp = None;
                        view.sync_viewport(w);
                        cx.notify();
                        Ok(())
                    })?;
                }
            }
            if v.capture_source.is_some() {
                let canvas = v.canvas.as_ref().ok_or("截图贴图没有文档画布")?;
                canvas.update(cx, |view, _| -> Result<()> {
                    let scene = view.prepared_scene();
                    if !scene.errors.is_empty() {
                        return Err(scene.errors.join(";").into());
                    }
                    view.prepare_pin_presentation_preview(w, &scene, true)?
                        .ok_or("截图贴图真实内容预览未准备")?;
                    Ok(())
                })?;
            }
            // 截图交接的新窗口先呈现；accepted 回调立即登记，恢复窗口仍先核组显隐。
            if completion.is_none() || restoring {
                v.attach_workspace_or_report(restored, w, cx)?;
            }
            crate::pin_handoff_smoke::mark("pin_workspace_end");
            match completion {
                Some((capture_owner, completion)) => {
                    crate::pin_native::place_initial_pin_with_completion(
                        w.window_handle(),
                        hwnd(w),
                        p,
                        show && !restoring,
                        Some(capture_owner),
                        Some(completion),
                        cx,
                    )
                }
                None => crate::pin_native::place_initial_pin(
                    w.window_handle(),
                    hwnd(w),
                    p,
                    show && !restoring,
                    cx,
                ),
            }
            cx.observe_window_bounds(w, |v, w, cx| {
                v.dismiss_context_menu(cx);
                if v.correct_actual_presentation_geometry(w, cx) {
                    cx.notify();
                    return;
                }
                if v.last_scale_factor != w.scale_factor() {
                    v.last_scale_factor = w.scale_factor();
                    v.preserve_physical_zoom(w, cx);
                }
                v.sync_pin_canvas(w, cx);
                v.sync_toolbar(w, cx);
                v.pin_find_refresh(w, cx);
                cx.notify();
            })
            .detach();
            if v.presentation_style.is_some() {
                // 桌面拓扑可能只改变可见 ROI 而不产生本窗口 bounds 事件。
                // 仅读系统矩形，变化时才通知；不读取桌面、不创建图像或改变焦点。
                cx.spawn_in(w, async move |view, cx| loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                    let keep = view
                        .update_in(cx, |v, w, cx| {
                            let Ok(actual) = native::rect(hwnd(w)) else {
                                return true;
                            };
                            let stamp = (actual, presentation_desktop());
                            if v.presentation_geometry_stamp != Some(stamp) {
                                v.presentation_geometry_stamp = Some(stamp);
                                v.presentation_geometry_failed = None;
                                cx.notify();
                            }
                            true
                        })
                        .unwrap_or(false);
                    if !keep {
                        break;
                    }
                })
                .detach();
            }
            Ok(())
        });
        let failure = match initialized {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(error.into()),
        };
        if let Some(error) = failure {
            // 创建 HWND 后的接入失败同样需要回收临时窗口和租约，不消费原 Closed 记录。
            let _ = handle.update(cx, |v, w, cx| {
                v.workspace = None;
                v.close_toolbar(cx);
                w.remove_window();
            });
            return Err(error);
        }
        Ok(handle)
    }
    /// 内容始终以原图坐标编辑；尺寸只在窗口边界加一次外观留白。
    pub(crate) fn window_image_size(&self) -> (u32, u32) {
        crate::pin_presentation_host::window_size(self.content_size(), self.presentation_style)
            .expect("贴图创建和处理前已检查完整外框尺寸")
    }
    pub(crate) fn content_viewport_for(&self, host: (u32, u32)) -> crate::viewport::Viewport {
        crate::pin_presentation_host::source_content_viewport(
            self.content_rect(),
            host,
            self.presentation_style,
            self.window_sizing,
        )
    }
    pub(crate) fn content_rect(&self) -> crate::geometry::Rect {
        self.capture_source.as_ref().map_or_else(
            || crate::geometry::Rect::full(self.image_size.0, self.image_size.1),
            |source| source.content_rect(),
        )
    }
    pub(crate) fn content_size(&self) -> (u32, u32) {
        let rect = self.content_rect();
        (rect.width, rect.height)
    }
    pub(crate) fn initial_capture_content_ready(
        &self,
        host: (u32, u32),
        w: &Window,
        cx: &App,
    ) -> bool {
        let Some(metadata) = &self.capture_source else {
            return true;
        };
        let (Some(pixels), Some(style)) = (&self.capture_pixels, self.presentation_style) else {
            return false;
        };
        self.canvas.as_ref().is_some_and(|canvas| {
            let view = canvas.read(cx);
            let visible = !view.pin_mode
                || view
                    .pin_annotation_preview
                    .as_ref()
                    .is_none_or(|p| p.visible());
            view.pin_content_rect == Some(metadata.content_rect())
                && view.source.bounds == metadata.bounds()
                && view
                    .source
                    .capture_pixels()
                    .is_some_and(|installed| Arc::ptr_eq(installed, pixels))
                && view.scene_stamp == Some(view.editor.scene_stamp())
                && view
                    .pin_presentation_preview_plan(w, style)
                    .is_ok_and(|plan| {
                        view.pin_presentation_group_frame_matches(w, plan, &view.scene, visible)
                    })
                && crate::pin_raster_document::capture_source::content_presented(
                    &view.pin_content_drawn_size,
                    host,
                )
        })
    }
    pub(crate) fn content_viewport(&self, w: &Window) -> crate::viewport::Viewport {
        let logical = w.viewport_size();
        let dpi = w.scale_factor();
        self.content_viewport_for((
            (f32::from(logical.width) * dpi).round().max(1.) as u32,
            (f32::from(logical.height) * dpi).round().max(1.) as u32,
        ))
    }
    /// 在改变原生客户区/内容前核当前可见预览预算；仅读取桌面几何。
    pub(crate) fn validate_presentation_placement(&self, placement: Placement) -> Result<()> {
        self.validate_presentation_geometry(placement, self.state.zoom, self.window_sizing)
    }
    pub(crate) fn validate_presentation_geometry(
        &self,
        placement: Placement,
        requested_zoom: f32,
        sizing: PinWindowSizing,
    ) -> Result<()> {
        let Some(style) = self.presentation_style else {
            return Ok(());
        };
        let desktop = presentation_desktop();
        crate::pin_presentation::validate_preview_geometry_with_sizing(
            self.content_size(),
            style,
            placement,
            desktop,
            sizing,
        )?;
        if self.processing {
            let content = self
                .state
                .adjustments
                .dimensions(self.original_size.0, self.original_size.1);
            let outer = crate::pin_presentation_host::window_size(content, Some(style))?;
            let zoom = window_zoom(outer, requested_zoom);
            let (width, height) = sizing.size(outer, zoom);
            crate::pin_presentation::validate_preview_geometry_with_sizing(
                content,
                style,
                Placement {
                    width,
                    height,
                    ..placement
                },
                desktop,
                sizing,
            )?;
        }
        Ok(())
    }
    /// 接受后的外部 HWND 尺寸/桌面拓扑变化仍须先符合实际绘制预算。
    /// 无合规完整帧时只显示调整提示；候选原生定位完成前不接入画布输入。
    fn correct_actual_presentation_geometry(&mut self, w: &Window, cx: &mut Context<Self>) -> bool {
        let Some(style) = self.presentation_style else {
            self.presentation_geometry_adjusting = false;
            return false;
        };
        let actual = match native::rect(hwnd(w)) {
            Ok(actual) => actual,
            Err(error) => {
                self.status = error.to_string();
                if !self.presentation_geometry_adjusting {
                    self.close_toolbar(cx);
                    self.chrome = false;
                }
                self.presentation_geometry_adjusting = true;
                return true;
            }
        };
        let desktop = presentation_desktop();
        let stamp = (actual, desktop);
        if self.presentation_geometry_stamp != Some(stamp) {
            self.presentation_geometry_stamp = Some(stamp);
            self.presentation_geometry_failed = None;
        }
        let notice = tr!(
            "正在调整贴图尺寸以适应预览范围…",
            "Resizing pin to fit the preview area…"
        );
        if crate::pin_presentation::validate_preview_geometry_with_sizing(
            self.content_size(),
            style,
            actual,
            desktop,
            self.window_sizing,
        )
        .is_ok()
        {
            self.presentation_geometry_adjusting = false;
            self.presentation_geometry_failed = None;
            if self.status == notice {
                self.status.clear();
            }
            return false;
        }
        if !self.presentation_geometry_adjusting {
            // 属性/更多工具独立浮窗也暂停；不自动重开或改变内容焦点。
            self.close_toolbar(cx);
            self.chrome = false;
        }
        self.presentation_geometry_adjusting = true;
        self.cancel_drag(w);
        self.finish_text_selection_drag();
        if self.presentation_geometry_failed == Some(stamp) {
            return true;
        }
        if self
            .pending_placement
            .is_some_and(|pending| self.validate_presentation_placement(pending).is_ok())
        {
            return true;
        }
        let candidate = (|| -> Result<(Placement, f32)> {
            let mut target = crate::pin_presentation::fit_preview_placement_with_sizing(
                self.content_size(),
                style,
                actual,
                desktop,
                self.window_sizing,
            )?;
            // 正在处理时同时核未来内容；整数客户区缩小不改变当前文档坐标。
            for _ in 0..64 {
                let zoom = self
                    .content_viewport_for((target.width, target.height))
                    .zoom;
                if self
                    .validate_presentation_geometry(target, zoom, self.window_sizing)
                    .is_ok()
                {
                    return Ok((target, zoom));
                }
                let dimensions = (
                    ((u64::from(target.width) * 9) / 10).max(1) as u32,
                    ((u64::from(target.height) * 9) / 10).max(1) as u32,
                );
                if dimensions == (target.width, target.height) {
                    break;
                }
                target.width = dimensions.0;
                target.height = dimensions.1;
            }
            Err("显示区域变化后无法生成预算内贴图预览".into())
        })();
        match candidate {
            Ok((target, zoom)) => {
                self.state.zoom = zoom;
                self.status = notice.into();
                self.request_placement(target, w, cx);
            }
            Err(error) => {
                self.presentation_geometry_failed = Some(stamp);
                self.status = trf!(
                    "贴图预览调整失败：{error}",
                    "Could not resize pin preview: {error}"
                );
            }
        }
        true
    }
    pub(crate) fn validate_presentation_content_size(
        &self,
        content: (u32, u32),
        w: &Window,
    ) -> Result<()> {
        self.validate_presentation_checkpoint(content, self.presentation_style, w)
    }
    pub(crate) fn validate_presentation_checkpoint(
        &self,
        content: (u32, u32),
        style: Option<crate::selection_finish::SelectionFinishStyle>,
        w: &Window,
    ) -> Result<()> {
        let Some(style) = style else { return Ok(()) };
        let old = self.placement(w)?;
        let outer = crate::pin_presentation_host::window_size(content, Some(style))?;
        let zoom = window_zoom(outer, self.state.zoom);
        let (width, height) = self.window_sizing.size(outer, zoom);
        crate::pin_presentation::validate_preview_geometry_with_sizing(
            content,
            style,
            Placement {
                width,
                height,
                ..old
            },
            presentation_desktop(),
            self.window_sizing,
        )
    }
    /// 在丢弃画布、封存标注或改变处理历史之前核真正候选尺寸。
    fn validate_processing_action(&self, action: PinAction, w: &Window, cx: &App) -> Result<()> {
        let mut next = self.state.adjustments;
        if matches!(action, PinAction::Undo | PinAction::Redo) {
            let mut history = self.history.clone();
            let changed = if matches!(action, PinAction::Redo) {
                history.redo(&mut next)
            } else {
                history.undo(&mut next)
            };
            if changed {
                self.validate_presentation_content_size(
                    next.dimensions(self.original_size.0, self.original_size.1),
                    w,
                )?;
            }
            return Ok(());
        }
        let mut original = self.original_size;
        if let Some(canvas) = &self.canvas {
            let view = canvas.read(cx);
            if view.editor.document.can_undo() || self.vector_text.is_some() {
                let rect = view.output_rect()?;
                original = (rect.width, rect.height);
                next = Default::default();
            }
        }
        match action {
            PinAction::Rotate(delta) => next = next.rotated(delta),
            PinAction::FlipX => next.flip_x = !next.flip_x,
            PinAction::FlipY => next.flip_y = !next.flip_y,
            PinAction::Gray => next.grayscale = !next.grayscale,
            PinAction::Invert => next.invert = !next.invert,
            PinAction::Brightness(delta) => {
                next.brightness = (next.brightness + delta).clamp(-100, 100)
            }
            PinAction::ResetImage => next = Default::default(),
            _ => {}
        }
        self.validate_presentation_content_size(next.dimensions(original.0, original.1), w)
    }
    /// 处理/撤销成功后按完整外框更新；锁定只约束用户手势，不阻断内部恢复。
    pub(crate) fn sync_presentation_geometry(
        &mut self,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let outer = crate::pin_presentation_host::window_size(
            self.content_size(),
            self.presentation_style,
        )?;
        let old = self.placement(w)?;
        let zoom = window_zoom(outer, self.state.zoom);
        let (width, height) = self.window_sizing.size(outer, zoom);
        let placement = Placement {
            width,
            height,
            ..old
        };
        self.validate_presentation_geometry(placement, zoom, self.window_sizing)?;
        self.state.zoom = zoom;
        self.request_placement(placement, w, cx);
        self.sync_pin_canvas(w, cx);
        Ok(())
    }
    pub(crate) fn raster_document_snapshot(
        &self,
        cx: &App,
    ) -> Result<crate::pin_raster_document::WireRasterDocument> {
        if let Some(canvas) = &self.canvas {
            let view = canvas.read(cx);
            let editor = &view.editor;
            match &self.capture_source {
                Some(source) => {
                    let pixels = self
                        .capture_pixels
                        .as_ref()
                        .ok_or("截图文档缺少CPU采样上下文")?;
                    let metadata =
                        crate::pin_raster_document::capture_source::CaptureSourceMetadata::new(
                            pixels,
                            source.content_rect(),
                            view.cursor.as_ref().filter(|_| view.include_cursor),
                        )?;
                    crate::pin_raster_document::WireRasterDocument::from_capture_editor(
                        editor,
                        self.image_size,
                        metadata,
                    )
                }
                None => crate::pin_raster_document::WireRasterDocument::from_editor(
                    editor,
                    self.image_size,
                ),
            }
        } else {
            if self.capture_source.is_some() {
                return Err("截图贴图完整文档画布缺失，不能发布空文档".into());
            }
            crate::pin_raster_document::WireRasterDocument::empty(self.image_size)
        }
    }
    pub(crate) fn processing_history_snapshot(&self) -> Result<Option<WireProcessingHistory>> {
        if self.vector_text.is_some() || self.capture_source.is_some() {
            return Ok(None);
        }
        if self.processing_blocks_close() {
            return Err("图像处理尚未完成，不能保存处理历史".into());
        }
        self.require_processing_image_available()?;
        for (image, size) in [
            (&self.original, self.original_size),
            (&self.image, self.image_size),
        ] {
            if image.frame_count() != 1 {
                return Err("贴图处理历史的图像帧格式无效".into());
            }
            let actual = image.size(0);
            let length = usize::try_from(u64::from(size.0) * u64::from(size.1) * 4)?;
            if (u32::from(actual.width), u32::from(actual.height)) != size
                || image.as_bytes(0).map(<[u8]>::len) != Some(length)
            {
                return Err("贴图处理历史的实际像素帧与来源尺寸不一致".into());
            }
        }
        WireProcessingHistory::from_history(
            &self.history,
            self.state.adjustments,
            self.original_size,
            self.image_size,
        )
        .map(Some)
    }
    pub(crate) fn pixels(&self) -> Image {
        if self.capture_source.is_some() {
            return Image {
                width: self.original_size.0,
                height: self.original_size.1,
                bgra: self
                    .image
                    .as_bytes(0)
                    .expect("capture compatibility frame")
                    .to_vec(),
                readback_ms: 0.,
            };
        }
        if let Some(fill) = self.vector_text_fill() {
            return Self::filled(self.image_size, fill);
        }
        Image {
            width: self.image_size.0,
            height: self.image_size.1,
            bgra: self.image.as_bytes(0).unwrap().to_vec(),
            readback_ms: 0.,
        }
    }
    /// 矢量纯色底只在需要输出像素时生成。
    fn filled((width, height): (u32, u32), fill: [u8; 4]) -> Image {
        Image {
            width,
            height,
            bgra: fill.repeat(width as usize * height as usize),
            readback_ms: 0.,
        }
    }
    fn source_pixels(&self) -> Image {
        if let Some(fill) = self.vector_text_fill() {
            return Self::filled(self.original_size, fill);
        }
        Image {
            width: self.original_size.0,
            height: self.original_size.1,
            bgra: self.original.as_bytes(0).unwrap().to_vec(),
            readback_ms: 0.,
        }
    }
    fn persist(&mut self, w: &Window, cx: &mut App) -> Result<()> {
        self.finish_canvas_content(cx)?;
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, _| v.flush_tool_presets());
        }
        if self.persisted {
            return Ok(());
        }
        self.persisted = true;
        if !crate::preferences::Preferences::load().pin_restore {
            return Ok(());
        }
        if self.vector_text.is_some() {
            let result = (|| {
                let document = self
                    .text_document_snapshot(cx)?
                    .ok_or("文字贴图保存缺少完整文档")?;
                let p = native::rect(hwnd(w))?;
                // PNG 仅作兼容输出；恢复来源和两栈均由文字文档决定。
                let image = self.output(cx)?;
                crate::pin_store::save_text_document(
                    image,
                    &self.state,
                    p,
                    self.window_sizing,
                    document,
                )
            })();
            if result.is_err() {
                self.persisted = false;
            }
            return result;
        }
        // 普通图片也保存真正原图与处理参数；不能把处理后 PNG 当原图清空历史。
        let result = (|| {
            let p = native::rect(hwnd(w))?;
            let metadata = crate::pin_store::RasterClosedMetadata {
                style: self.presentation_style,
                document: self.raster_document_snapshot(cx)?,
                processing_history: self.processing_history_snapshot()?,
            };
            match &self.capture_pixels {
                Some(pixels) => crate::pin_store::save_capture_document(
                    self.source_pixels(),
                    &self.state,
                    p,
                    self.window_sizing,
                    metadata,
                    pixels.clone(),
                ),
                None => crate::pin_store::save_processed_document(
                    self.source_pixels(),
                    &self.state,
                    p,
                    self.window_sizing,
                    metadata,
                ),
            }
        })();
        if result.is_err() {
            self.persisted = false;
        }
        result
    }

    pub(crate) fn zoom(
        &mut self,
        value: f32,
        anchor: Option<(f32, f32)>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.zoom_with_sizing(value, anchor, false, false, w, cx)
    }
    fn user_zoom(
        &mut self,
        value: f32,
        anchor: Option<(f32, f32)>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.zoom_with_sizing(value, anchor, true, false, w, cx)
    }
    fn zoom_with_sizing(
        &mut self,
        value: f32,
        anchor: Option<(f32, f32)>,
        user_geometry: bool,
        top_left: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if self.state.locked || self.zoom_busy(cx) || !value.is_finite() || value <= 0. {
            return Ok(());
        }
        let old = self.placement(w)?;
        let pointer = anchor.is_some();
        let anchor = if let Some(anchor) = anchor {
            // 输入仍相对当前 HWND；同帧合并时需先换成待应用窗口的局部坐标。
            let actual = native::rect(hwnd(w))?;
            (
                anchor.0 + (actual.x - old.x) as f32,
                anchor.1 + (actual.y - old.y) as f32,
            )
        } else {
            (old.width as f32 / 2., old.height as f32 / 2.)
        };
        let sizing = self.window_sizing.after_qualified_zoom(user_geometry);
        let retained =
            self.physical_geometry
                .retained_size(self.window_image_size(), value, user_geometry);
        let zoom = if retained.is_some() {
            value
        } else {
            window_zoom(self.window_image_size(), value)
        };
        let next = retained.unwrap_or_else(|| sizing.size(self.window_image_size(), zoom));
        let (p, top_left_zoom_anchor, pointer_zoom_anchor) = if top_left || pointer {
            let before = crate::pin_presentation_host::content_viewport_with_sizing(
                self.content_size(),
                (old.width, old.height),
                self.presentation_style,
                self.window_sizing,
            );
            let after = crate::pin_presentation_host::content_viewport_with_sizing(
                self.content_size(),
                next,
                self.presentation_style,
                sizing,
            );
            if top_left {
                let (p, anchor) = crate::pin_zoom::TopLeftAnchor::place(
                    self.top_left_zoom_anchor,
                    old,
                    before.offset,
                    next,
                    after.offset,
                );
                (p, Some(anchor), None)
            } else {
                let (p, anchor) = crate::pin_zoom::PointerAnchor::place(
                    self.pointer_zoom_anchor,
                    old,
                    before,
                    next,
                    after,
                    anchor,
                );
                (p, None, Some(anchor))
            }
        } else {
            (
                crate::pin_presentation_host::anchored_placement(
                    self.content_size(),
                    self.presentation_style,
                    old,
                    next,
                    anchor,
                    (self.window_sizing, sizing),
                ),
                None,
                None,
            )
        };
        self.validate_presentation_geometry(p, zoom, sizing)?;
        self.state.zoom = zoom;
        self.window_sizing = sizing;
        self.request_placement(p, w, cx);
        self.top_left_zoom_anchor = top_left_zoom_anchor;
        self.pointer_zoom_anchor = pointer_zoom_anchor;
        Ok(())
    }
    fn zoom_busy(&self, cx: &App) -> bool {
        self.gesture.is_some()
            || self.canvas.as_ref().is_some_and(|canvas| {
                let editor = &canvas.read(cx).editor;
                editor.is_dragging() || editor.has_pending_drawing() || editor.text_draft.is_some()
            })
    }
    /// 工作区远程解除只在输入稳定时应用；此查询不提交草稿或改变焦点。
    pub(crate) fn workspace_control_input_busy(&self, cx: &App) -> bool {
        self.zoom_busy(cx)
            || self.drag_request.is_some()
            || self.pending_placement.is_some()
            || self.color_pick
            || self.text_anchor.is_some()
            || self.text_capture.is_some()
            || self.canvas.as_ref().is_some_and(|canvas| {
                let view = canvas.read(cx);
                view.saving
                    || view.processing
                    || view.color_picker_pending
                    || view.watermark_input_focused
                    || view.input.marked.is_some()
                    || view.input.drag_anchor.is_some()
                    || view.pointer_down_start.is_some()
                    || view.toolbar_drag.is_some()
                    || view.editor.lasso_draft.is_some()
                    || view.editor.watermark.has_preview()
            })
    }
    fn zoom_top_left(&mut self, value: f32, w: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        self.zoom_with_sizing(value, None, true, true, w, cx)
    }
    pub(crate) fn reset_zoom_wheel(&mut self) {
        self.wheel.reset();
    }
    fn reload_zoom_anchor(&mut self) {
        let anchor = crate::preferences::Preferences::load().pin_zoom_anchor;
        if self.pin_zoom_anchor != anchor {
            self.pin_zoom_anchor = anchor;
            self.reset_zoom_wheel();
        }
    }
    fn zoom_wheel(
        &mut self,
        value: f32,
        pointer: (f32, f32),
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        match self.pin_zoom_anchor {
            crate::pin_zoom::ZoomAnchor::TopLeft => self.zoom_top_left(value, w, cx),
            crate::pin_zoom::ZoomAnchor::Pointer => self.user_zoom(value, Some(pointer), w, cx),
            crate::pin_zoom::ZoomAnchor::Center => self.user_zoom(value, None, w, cx),
        }
    }
    fn preserve_physical_zoom(&self, w: &Window, cx: &mut Context<Self>) {
        // 等 WM_DPICHANGED 的 suggested-rect 链完整结束，再校正物理尺寸。
        // DPI 只改变 UI 的 DIP，不改变 100% = 一个原图像素对应一个屏幕像素。
        cx.spawn_in(w, async move |view, cx| {
            let _ = view.update_in(cx, |v, w, cx| {
                let Ok(actual) = v.placement(w) else { return };
                let desired = v.physical_geometry.size;
                if (actual.width, actual.height) != desired {
                    v.request_placement(
                        Placement {
                            width: desired.0,
                            height: desired.1,
                            ..actual
                        },
                        w,
                        cx,
                    );
                }
            });
        })
        .detach();
    }
    pub(crate) fn placement_pending(&self) -> bool {
        self.pending_placement.is_some()
    }
    pub(crate) fn placement(&self, w: &Window) -> Result<Placement> {
        self.pending_placement
            .map_or_else(|| native::rect(hwnd(w)), Ok)
    }
    fn sync_pin_canvas(&self, w: &Window, cx: &mut Context<Self>) {
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, cx| {
                v.pin_presentation_style = self.presentation_style;
                v.pin_window_sizing = self.window_sizing;
                v.sync_viewport(w);
                cx.notify();
            });
        }
    }
    /// 原生尺寸消息会同步回调 GPUI。离开 App/Window 借用后再定位，并合并同帧请求。
    pub(crate) fn request_placement(
        &mut self,
        placement: Placement,
        w: &Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_pin_find_click();
        if let Err(error) = self.validate_presentation_placement(placement) {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        self.dismiss_context_menu(cx);
        self.physical_geometry = PhysicalWindowGeometry::new(
            self.window_image_size(),
            self.state.zoom,
            (placement.width, placement.height),
        );
        self.pending_placement = Some(placement);
        // 移动、拖角、处理和其它缩放会建立新的内容原点；不能复用旧屏幕锚点。
        self.top_left_zoom_anchor = None;
        self.pointer_zoom_anchor = None;
        // 仅策略变化先同步全部消费者；普通定位仍只由原生完成回执同步。
        if self
            .canvas
            .as_ref()
            .is_some_and(|canvas| canvas.read(cx).pin_window_sizing != self.window_sizing)
        {
            self.sync_pin_canvas(w, cx);
        }
        self.placement_revision = self.placement_revision.wrapping_add(1);
        let revision = self.placement_revision;
        let handle = hwnd(w).0 as usize;
        cx.spawn_in(w, async move |view, cx| {
            let current = view.update_in(cx, |v, w, cx| {
                let placement = (v.placement_revision == revision)
                    .then_some(v.pending_placement)
                    .flatten()?;
                // 纯移动内容不变，照常随下一次垂直同步绘制；改尺寸才预备底图并立即重绘。
                let resized = native::rect(hwnd(w)).is_ok_and(|actual| {
                    (actual.width, actual.height) != (placement.width, placement.height)
                });
                if resized {
                    v.prestage_presentation(placement, w, cx);
                }
                // 改尺寸常伴随移动（按鼠标位置缩放）。先移动会让旧倍率内容随新原点平移一帧
                // 再跳到新倍率，形成抖动；能先按目标尺寸出帧时改为先画、后移动。
                let present_first = resized
                    .then(|| native::present_first_bounds(hwnd(w), placement))
                    .flatten();
                if present_first.is_some() {
                    v.set_canvas_render_placement(Some(placement), cx);
                }
                Some((placement, resized, present_first))
            });
            let Ok(Some((placement, resized, present_first))) = current else {
                return;
            };
            let h = windows::Win32::Foundation::HWND(handle as *mut _);
            let redraw_now = || unsafe {
                let _ = windows::Win32::Graphics::Gdi::RedrawWindow(
                    Some(h),
                    None,
                    None,
                    windows::Win32::Graphics::Gdi::RDW_INVALIDATE
                        | windows::Win32::Graphics::Gdi::RDW_UPDATENOW,
                );
            };
            // 视口先按目标尺寸生效；须离开本视图借用，bounds 观察者会同步画布与工具栏。
            let bounds_prepared = present_first.is_some_and(|bounds| {
                cx.update(|w, cx| w.prepare_content_bounds(bounds, cx).is_ok())
                    .unwrap_or(false)
            });
            let prepared = if bounds_prepared {
                crate::pin_handoff_smoke::mark("zoom_present_first_begin");
                // 新帧先只渲染不提交，等 GPU 画完；紧接一次合成之后再提交，
                // 并等 GPU 处理完这次 Present（DWM 此后才收得到新帧）再移动，
                // 二者离下一次合成都有近一帧余量。
                let gpu = std::time::Duration::from_millis(100);
                let _ = cx.update(|w, _| w.defer_next_present());
                redraw_now();
                let rendered = cx.update(|w, _| w.wait_for_gpu(gpu)).unwrap_or(false);
                if rendered {
                    native::wait_for_composition();
                }
                // 即使等待失败也清掉延迟标记，后续立即重绘不能再被推迟。
                let presented = cx.update(|w, _| w.present_deferred()).unwrap_or(false);
                let prepared =
                    rendered && presented && cx.update(|w, _| w.wait_for_gpu(gpu)).unwrap_or(false);
                if prepared {
                    crate::pin_handoff_smoke::mark("zoom_presented_before_place");
                }
                prepared
            } else {
                false
            };
            // 不可移入 update_in：否则 WM_SIZE 的 viewport 更新会因重入借用失败丢失。
            crate::pin_handoff_smoke::mark("zoom_place_begin");
            let result = if resized {
                native::place_window(h, placement)
            } else {
                native::place(h, placement)
            }
            .map_err(|error| error.to_string());
            crate::pin_handoff_smoke::mark("zoom_place_end");
            if prepared && result.is_ok() {
                // 新内容已在移动前出帧，阴影紧随移动重绘。
                crate::pin_shadow::sync_native(h);
                crate::pin_handoff_smoke::mark("zoom_shadow_synced");
            }
            if bounds_prepared && result.is_err() {
                // 窗口没动：视口与交换链回到实际客户区。
                if let Some(actual) = native::rect(h)
                    .ok()
                    .and_then(|actual| native::present_first_bounds(h, actual))
                {
                    let _ = cx.update(|w, cx| w.prepare_content_bounds(actual, cx));
                }
            }
            let redraw = resized && result.is_ok() && !prepared;
            let _ = view.update_in(cx, |v, w, cx| {
                let placed = result.is_ok();
                if present_first.is_some() {
                    v.set_canvas_render_placement(None, cx);
                }
                if v.placement_revision == revision {
                    v.pending_placement = None;
                    if let Err(error) = result {
                        v.top_left_zoom_anchor = None;
                        v.pointer_zoom_anchor = None;
                        v.status = error;
                        if let Ok(actual) = native::rect(hwnd(w)) {
                            if v.presentation_geometry_adjusting {
                                // 原生失败保留提示和旧缓存，同一失败几何不排重试风暴。
                                v.presentation_geometry_failed =
                                    Some((actual, presentation_desktop()));
                            }
                            v.state.zoom = crate::pin_zoom::projection(
                                v.window_image_size(),
                                (actual.width, actual.height),
                            )
                            .zoom;
                            v.physical_geometry = PhysicalWindowGeometry::new(
                                v.window_image_size(),
                                v.state.zoom,
                                (actual.width, actual.height),
                            );
                        }
                        v.cancel_drag(w);
                    }
                }
                v.sync_pin_canvas(w, cx);
                v.sync_toolbar(w, cx);
                // Up 的坐标属于移动前的客户区；原生定位完成后重新取当前屏幕点。
                if placed && v.pending_placement.is_none() && v.color_pick && v.gesture.is_none() {
                    if let Some(pointer) = Self::color_pointer(w) {
                        v.refresh_color(pointer, w, cx);
                    } else {
                        v.clear_color_preview(cx);
                    }
                }
                cx.notify();
            });
            if redraw {
                // 无法先出帧时（跨 DPI 等）不等下一次垂直同步：立即画改尺寸后的首帧，
                // 再重绘阴影，缩短窗口已变、内容未到的空档，并让阴影与新内容尽量同帧。
                redraw_now();
                crate::pin_handoff_smoke::mark("zoom_redrawn");
                crate::pin_shadow::sync_native(h);
                crate::pin_handoff_smoke::mark("zoom_shadow_synced");
            }
        })
        .detach();
    }
    fn set_canvas_render_placement(&self, placement: Option<Placement>, cx: &mut Context<Self>) {
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |view, _| view.pin_render_placement = placement);
        }
    }
    /// 改尺寸前按目标几何备好截图底图，让窗口与新内容尽量同帧出现。
    /// 预备失败只放弃预备，下一帧照常现采并在那里报告错误。
    fn prestage_presentation(&self, placement: Placement, w: &Window, cx: &mut Context<Self>) {
        if let Some(canvas) = self
            .canvas
            .as_ref()
            .filter(|_| self.presentation_style.is_some())
        {
            canvas.update(cx, |view, _| {
                let _ = view.prestage_pin_presentation_body(w, placement);
            });
        }
    }
    fn save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.export_user(crate::pin_export::UserExportAction::Save, w, cx);
    }
    pub(crate) fn open_save_image(&mut self, image: Image, w: &mut Window, cx: &mut Context<Self>) {
        let image = Arc::new(image);
        self.save_busy(true, cx);
        self.status = tr!(
            "选择保存格式、名称和位置…",
            "Choose format, name and location…"
        )
        .into();
        let owner = w.window_handle().downcast::<Self>().expect("贴图窗口类型");
        let opened = crate::save_panel::SavePanel::open(
            image,
            None,
            tr!("保存贴图", "Save pin"),
            move |outcome, cx| {
                let _ = owner.update(cx, |v, w, cx| {
                    v.save_busy(false, cx);
                    v.status = match outcome {
                        crate::save_panel::SaveOutcome::Cancelled => String::new(),
                        crate::save_panel::SaveOutcome::Saved { path, warnings } => {
                            crate::save_workflow::message(&path, &warnings)
                        }
                    };
                    crate::gpui_capture::foreground_auxiliary(w);
                    w.focus(&v.focus);
                    v.sync_toolbar(w, cx);
                    cx.notify();
                });
            },
            cx,
        );
        if let Err(e) = opened {
            self.save_busy(false, cx);
            self.status = e.to_string();
        }
    }
    pub(crate) fn save_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.saving = busy;
        if let Some(canvas) = &self.canvas {
            canvas.update(cx, |v, cx| {
                v.saving = busy;
                cx.notify();
            });
        }
    }
    fn quick_save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.export_user(crate::pin_export::UserExportAction::QuickSave, w, cx);
    }
    pub(crate) fn quick_save_image(
        &mut self,
        image: Image,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_busy(true, cx);
        self.status = tr!("正在快速保存…", "Quick saving…").into();
        let task = cx.background_executor().spawn(async move {
            crate::save_workflow::quick_save(&image, None, false).map_err(|e| e.to_string())
        });
        cx.spawn_in(w, async move |v, cx| {
            let result = task.await;
            let _ = v.update_in(cx, |v, w, cx| {
                v.save_busy(false, cx);
                v.status = match result {
                    Ok(report) => crate::save_workflow::message(&report.path, &report.warnings),
                    Err(e) => e,
                };
                v.sync_toolbar(w, cx);
                cx.notify();
            });
        })
        .detach();
    }
    fn process(
        &mut self,
        previous: pin_model::Adjustments,
        old_history: ProcessingHistory,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.processing_request = self.processing_request.wrapping_add(1);
        self.begin_processing_probe();
        if self.state.adjustments == Default::default() {
            if let Err(error) = self.validate_presentation_content_size(self.original_size, w) {
                self.state.adjustments = previous;
                self.history = old_history;
                self.processing_probe.completed = true;
                self.processing_probe.failed = true;
                self.status = error.to_string();
                self.sync_toolbar(w, cx);
                cx.notify();
                return;
            }
            // 回到原图直接复用已有像素与 GPU 缓存；预算拒绝不能提前替换当前图。
            let old_image = self.image.clone();
            let old_size = self.image_size;
            self.image = self.original.clone();
            self.image_size = self.original_size;
            if let Err(error) = self.sync_presentation_geometry(w, cx) {
                self.image = old_image;
                self.image_size = old_size;
                self.state.adjustments = previous;
                self.history = old_history;
                self.processing_probe.completed = true;
                self.processing_probe.failed = true;
                self.status = error.to_string();
                self.sync_toolbar(w, cx);
                cx.notify();
                return;
            }
            self.invalidate_text_overlay();
            self.end_color_pick(cx);
            self.finish_reset_processing_probe();
            self.status.clear();
            if self.chrome {
                let _ = self.ensure_canvas(w, cx);
            }
            self.sync_toolbar(w, cx);
            return;
        }
        self.invalidate_text_overlay();
        self.end_color_pick(cx);
        self.processing = true;
        let request = processing::ProcessingRequest::new(self, previous, old_history);
        cx.defer_in(w, move |v, w, cx| v.start_processing(request, w, cx));
    }
    /// OCR 不修改图像像素，工作区可继续真正关闭的最终保存；图像处理仍须等待。
    /// 工作区的三处关闭推进门消费此资格，拒绝关闭本身不取消 Owner。
    pub(crate) fn processing_blocks_close(&self) -> bool {
        ocr_task::blocks_close(self.processing, self.ocr_task.is_active())
    }
    fn cancel_ocr_task(&mut self) {
        self.clear_pin_ocr_progress();
        // 完整会话关闭／来源失效同时释放局部固定输入与独立 Owner。
        self.pin_local_ocr = None;
        self.pending_pin_local_input = None;
        if !self.ocr_task.is_active() {
            return;
        }
        let result = self.ocr_task.cancel();
        // 这里只释放本请求的 UI 资格，不宣称原生 Run、下载或 QR 已经退出。
        self.processing = false;
        if let Err(error) = result {
            eprintln!("贴图 OCR 取消控制失败：{error}");
        }
    }
    fn recognize(&mut self, qr: bool, copy: bool, w: &mut Window, cx: &mut Context<Self>) {
        self.recognize_mode(qr, copy, false, w, cx);
    }
    fn recognize_mode(
        &mut self,
        qr: bool,
        copy: bool,
        table: bool,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 只有新识别已完成前置检查，才替换本贴图旧 OCR；图像处理仍不可重入。
        if self.processing_blocks_close() {
            return;
        }
        if !self.annotation_preview.visible()
            && self
                .canvas
                .as_ref()
                .is_some_and(|canvas| !canvas.read(cx).editor.document.items().is_empty())
        {
            self.status = tr!(
                "请先显示标注再识别文字",
                "Show annotations before recognizing text"
            )
            .into();
            cx.notify();
            return;
        }
        if let Err(error) = self.finish_canvas_content(cx) {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        if let Err(error) = self.set_canvas_active(false, w, cx) {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        let image = match self.output(cx) {
            Ok(i) => i,
            Err(e) => {
                self.status = e.to_string();
                cx.notify();
                return;
            }
        };
        let stamp = self.text_selection_source(cx);
        let (origin, progress_roi) = if let Some(canvas) = &self.canvas {
            match canvas.read(cx).output_rect() {
                Ok(rect) => (
                    (rect.x as f32, rect.y as f32),
                    crate::geometry::Rect {
                        x: rect.x,
                        y: rect.y,
                        width: image.width,
                        height: image.height,
                    },
                ),
                Err(error) => {
                    self.status = error.to_string();
                    cx.notify();
                    return;
                }
            }
        } else {
            (
                (0., 0.),
                crate::geometry::Rect::full(image.width, image.height),
            )
        };
        // 输出准备失败不取消旧请求；来源失效和真正替换才撤销旧回执资格。
        let profile = if qr {
            crate::ppocr::OcrProfile::Tiny
        } else {
            // 同次请求只读一次；固定输入与后台 worker 共用，后继设置不改这批结果。
            crate::preferences::Preferences::load().ocr_profile
        };
        self.invalidate_text_overlay();
        self.end_color_pick(cx);
        let (request, token) = self.ocr_task.begin(stamp, self.copy_layout_generation);
        self.processing = true;
        self.begin_pin_ocr_progress(&request, progress_roi, qr);
        self.status = if qr {
            tr!("正在识别二维码…", "Scanning QR code…")
        } else if table {
            tr!(
                "正在识别表格，首次使用需下载依赖…",
                "Recognizing table; first use downloads required files…"
            )
        } else {
            tr!("正在本机识别文字…", "Recognizing text locally…")
        }
        .into();
        let provider = crate::recognition::OcrProvider::default();
        // 只保存实际即将进入同一个 OCR worker 的 Vec；QR 不借此生成局部资格。
        self.pending_pin_local_input = (!qr)
            .then(|| {
                local_retry_host::FixedInput::copy_with_profile(
                    &image,
                    stamp,
                    self.image_size,
                    provider,
                    profile,
                )
            })
            .flatten();
        let task = cx.background_executor().spawn(async move {
            let result = if qr {
                // QR 没有原生中断接口，只在调用前后检查；原始错误不被取消位覆盖。
                (|| -> Result<crate::recognition::RecognizedText> {
                    token.checkpoint()?;
                    let text = crate::recognition::qr(image)?;
                    token.checkpoint()?;
                    Ok(crate::recognition::RecognizedText {
                        text,
                        regions: Vec::new(),
                        words: Vec::new(),
                        lines: Vec::new(),
                    })
                })()
                .map(|text| (text, None, None, None))
            } else if table {
                crate::ppocr::table::recognize_cancellable(&image, profile, &token).map(|result| {
                    (
                        result.recognized,
                        result.grid,
                        result.region,
                        result.warning,
                    )
                })
            } else {
                crate::recognition::recognize_with_profile_cancellable(
                    image, provider, profile, &token,
                )
                .map(|text| (text, None, None, None))
            };
            // 后台先保留类型化取消回执，其它真实错误才转换为显示字符串。
            ocr_task::Outcome::from_result(result)
        });
        cx.spawn_in(w, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, w, cx| {
                let current = v.text_selection_source(cx);
                // finish 先核请求身份与来源，再释放 Owner；析构取消位不能否决正常结果。
                match v
                    .ocr_task
                    .finish(&request, current, v.copy_layout_generation)
                {
                    ocr_task::Receipt::Stale => return,
                    ocr_task::Receipt::SourceChanged => {
                        v.clear_pin_ocr_progress();
                        v.processing = false;
                        v.invalidate_text_overlay();
                        v.status = tr!(
                            "内容已改变，请重新识别文字",
                            "Content changed; recognize text again"
                        )
                        .into();
                        cx.notify();
                        return;
                    }
                    ocr_task::Receipt::Current => {
                        v.clear_pin_ocr_progress();
                        v.processing = false;
                    }
                }
                // 只有已确认 Current 的真实完整请求可取当前固定输入；旧回执不触及新槽。
                let local_input = v.pending_pin_local_input.take();
                match result {
                    ocr_task::Outcome::Completed((text, grid, region, warning)) => {
                        // 当前正文与局部 Binding 共享这一批实际 DTO；不镜像重建识别结果。
                        let text = Arc::new(text);
                        // 新增缓存仅持紧凑布局；不克隆识别 DTO 或截图像素。
                        v.copy_layout = (!qr && v.copy_layout_generation != u64::MAX).then(|| {
                            ocr_copy::ParagraphCopy::new(
                                text.copy_layout(),
                                stamp,
                                v.copy_layout_generation,
                            )
                        });
                        // 只有主动表格任务安装结构，普通文字 OCR 不推断或缓存表格。
                        v.table_copy =
                            (!qr && table && v.copy_layout_generation != u64::MAX).then(|| {
                                table_copy::TableCopy::with_structure(
                                    &text,
                                    stamp,
                                    v.copy_layout_generation,
                                    grid,
                                    region,
                                )
                            });
                        v.text_selection = (!qr).then(|| {
                            let mut selection =
                                crate::pin_text_selection::SelectionText::from_recognized(&text);
                            selection.translate(origin.0, origin.1);
                            selection
                        });
                        v.text_selection_stamp = Some(stamp);
                        v.text_source = if qr {
                            ocr_clipboard::TextSource::Qr
                        } else {
                            ocr_clipboard::TextSource::Ocr
                        };
                        v.text_mode = !qr;
                        v.status = if text.text.is_empty() {
                            tr!("未识别到文字", "No text recognized").into()
                        } else if qr {
                            trf!(
                                "{}  · Ctrl+Shift+C 复制",
                                "{}  · Ctrl+Shift+C copies",
                                text.text
                            )
                        } else {
                            crate::tray::shortcut_help::pin_ocr_selection_hint().into()
                        };
                        v.text = Some(Arc::clone(&text));
                        if !qr {
                            v.install_pin_sensitive(&text, progress_roi, cx);
                            v.install_pin_local_input(local_input, text, cx);
                        }
                        if table && copy {
                            // 模型和普通文字同批安装；复制失败仍保留文字及结构供重试。
                            v.status = v
                                .publish_cached_table(w, cx)
                                .map(|()| tr!("表格已复制", "Table copied").to_string())
                                .unwrap_or_else(|error| error.to_string());
                            v.status_notice_text.clear();
                        } else if copy {
                            let text = &v.text.as_ref().expect("识别结果已安装").text;
                            if qr {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    text.clone(),
                                ));
                            } else {
                                // 先安装完整 OCR 结果；发布失败只更新提示，重试复用原文。
                                v.status = Self::publish_ocr_text(text, w)
                                    .unwrap_or_else(|error| error.to_string());
                                v.status_notice_text.clear();
                            }
                        }
                        if let Some(warning) = warning.filter(|warning| !warning.is_empty()) {
                            v.status = format!("{}{}{}", v.status, tr!("；", "; "), warning);
                        }
                    }
                    ocr_task::Outcome::Cancelled => {
                        v.text_mode = false;
                        v.status = if qr {
                            tr!("二维码识别已取消", "QR scanning cancelled")
                        } else if table {
                            tr!("表格识别已取消", "Table recognition cancelled")
                        } else {
                            tr!("文字识别已取消", "Text recognition cancelled")
                        }
                        .into();
                    }
                    ocr_task::Outcome::Failed(e) => {
                        v.text_mode = false;
                        v.status = e;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(crate) fn selected_text(&self) -> Option<String> {
        if !self.text_mode {
            return None;
        }
        self.text_selection
            .as_ref()?
            .selected(self.selected.clone()?)
    }
    pub(crate) fn has_selected_text(&self) -> bool {
        self.selected_text().is_some()
    }
    pub(crate) fn can_copy_paragraph_text(&self, cx: &App) -> bool {
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || !self.text_mode
            || self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.color_pick
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || !self.text_mark_canvas_idle(cx)
        {
            return false;
        }
        let Some(selection) = &self.text_selection else {
            return false;
        };
        // 已编辑正文不得借旧 SelectionText 的相同来源 stamp 使用旧布局。
        if self
            .text
            .as_ref()
            .is_none_or(|result| result.text != selection.text)
        {
            return false;
        }
        self.copy_layout.as_ref().is_some_and(|layout| {
            layout.eligible(
                self.text_selection_stamp,
                self.text_selection_source(cx),
                self.copy_layout_generation,
                &selection.text,
                self.selected.as_ref(),
            )
        })
    }
    fn copy_paragraph_text(&self, cx: &App) -> Result<String> {
        if !self.text_mark_canvas_idle(cx)
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
        {
            return Err(tr!("请先结束当前输入或编辑", "Finish the current input or edit").into());
        }
        if !self.can_copy_paragraph_text(cx) {
            return Err(tr!("请先选中文字", "Select text first").into());
        }
        let selection = self.text_selection.as_ref().expect("当前 OCR 选字已核验");
        self.copy_layout
            .as_ref()
            .expect("当前 OCR 布局已核验")
            .copy(
                self.text_selection_stamp,
                self.text_selection_source(cx),
                self.copy_layout_generation,
                &selection.text,
                self.selected.as_ref(),
            )
    }
    /// 剪贴板发布只借用既有文字，不能提交草稿或改变选区、标记和历史。
    fn require_ocr_copy_idle(&self, cx: &App) -> Result<()> {
        if self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.color_pick
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || !self.text_mark_canvas_idle(cx)
        {
            return Err(tr!("请先结束当前输入或编辑", "Finish the current input or edit").into());
        }
        Ok(())
    }
    fn cached_ocr_text(&self, cx: &App) -> Option<&str> {
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || self.text_selection_stamp != Some(self.text_selection_source(cx))
        {
            return None;
        }
        self.text_selection
            .as_ref()
            .map(|selection| selection.text.as_str())
            .or_else(|| self.text.as_ref().map(|result| result.text.as_str()))
    }
    fn cached_table_text(&self, cx: &App) -> Option<&str> {
        let result = self.text.as_ref()?;
        let source = self.text_selection_source(cx);
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || self.text_selection_stamp != Some(source)
            || self
                .text_selection
                .as_ref()
                .is_none_or(|selection| selection.text != result.text)
        {
            return None;
        }
        self.table_copy
            .as_ref()?
            .text(&source, self.copy_layout_generation, &result.text)
    }
    fn current_table_structure_result(&self, cx: &App) -> bool {
        let Some(result) = &self.text else {
            return false;
        };
        self.text_source == ocr_clipboard::TextSource::Ocr
            && self.text_selection_stamp == Some(self.text_selection_source(cx))
            && self
                .text_selection
                .as_ref()
                .is_some_and(|selection| selection.text == result.text)
            && self.table_copy.as_ref().is_some_and(|cache| {
                cache.is_structure_result(
                    &self.text_selection_source(cx),
                    self.copy_layout_generation,
                    &result.text,
                )
            })
    }
    pub(crate) fn can_copy_table_text(&self, cx: &App) -> bool {
        self.require_ocr_copy_idle(cx).is_ok()
            && self.pending_placement.is_none()
            && self.text_capture.is_none()
            && (self.cached_table_text(cx).is_some()
                || (self.content_size().0 > 0
                    && self.content_size().1 > 0
                    && (self.annotation_preview.visible()
                        || self.canvas.as_ref().is_none_or(|canvas| {
                            canvas.read(cx).editor.document.items().is_empty()
                        }))))
    }
    /// 只发布当前代次的缓存；模型未得到表格时保留普通文字供选择复制。
    fn publish_cached_table(&self, w: &Window, cx: &App) -> Result<()> {
        self.require_ocr_copy_idle(cx)?;
        if self.pending_placement.is_some() || self.text_capture.is_some() {
            return Err(tr!("请先结束当前输入或编辑", "Finish the current input or edit").into());
        }
        let text = self.cached_table_text(cx).ok_or_else(|| {
            tr!(
                "未找到可靠表格，已显示文字",
                "No reliable table; text shown"
            )
            .to_string()
        })?;
        let source = self.text_selection_source(cx);
        let body = &self.text.as_ref().expect("表格来源已核验").text;
        if let Some(grid) = self
            .table_copy
            .as_ref()
            .and_then(|cache| cache.grid(&source, self.copy_layout_generation, body))
        {
            crate::table_clipboard::copy_table(grid, hwnd(w))
        } else {
            Self::publish_ocr_text(text, w).map(|_| ())
        }
    }
    fn publish_ocr_text(text: &str, window: &Window) -> Result<String> {
        Self::publish_ocr_content(text, ocr_clipboard::Content::Body, window)
    }
    fn publish_ocr_selected_text(text: &str, window: &Window) -> Result<String> {
        Self::publish_ocr_content(text, ocr_clipboard::Content::Selection, window)
    }
    fn publish_ocr_content(
        text: &str,
        content: ocr_clipboard::Content,
        window: &Window,
    ) -> Result<String> {
        match ocr_clipboard::publish_text(text, content, |text| {
            crate::export::copy_text(text, hwnd(window))
        }) {
            Ok(true) => Ok(tr!("文字已复制", "Text copied").into()),
            Ok(false) => Ok(tr!("没有可复制的文字", "No text to copy").into()),
            Err(error) => Err(trf!(
                "文字复制失败：{}；识别结果已保留，可重试",
                "Text copy failed: {}; recognized text kept, retry available",
                error
            )
            .into()),
        }
    }
    fn invalidate_text_copy_layout(&mut self) {
        self.copy_layout = None;
        self.table_copy = None;
        // 达到上界后停止缓存新布局，代次不能回绕而继承旧选择资格。
        self.copy_layout_generation = self.copy_layout_generation.saturating_add(1);
    }
    fn rebase_text_copy_layout(
        &mut self,
        before: Option<TextSelectionStamp>,
        after: TextSelectionStamp,
    ) {
        if before != Some(after) {
            // 自有标记／Undo 仍保留原会话，但固定采样不重绑新底层；防来源 ABA。
            self.pin_local_ocr = None;
        }
        let Some(selection) = &self.text_selection else {
            self.copy_layout = None;
            self.table_copy = None;
            return;
        };
        if self
            .text
            .as_ref()
            .is_none_or(|result| result.text != selection.text)
            || self.table_copy.as_mut().is_some_and(|cache| {
                !cache.rebase(
                    before.as_ref(),
                    after,
                    self.copy_layout_generation,
                    &selection.text,
                )
            })
        {
            self.table_copy = None;
        }
        if self
            .text
            .as_ref()
            .is_none_or(|result| result.text != selection.text)
            || self.copy_layout.as_mut().is_some_and(|layout| {
                !layout.rebase(before, after, self.copy_layout_generation, &selection.text)
            })
        {
            self.copy_layout = None;
        }
    }
    fn text_mark_canvas_idle(&self, cx: &App) -> bool {
        self.canvas.as_ref().is_none_or(|canvas| {
            let view = canvas.read(cx);
            !view.pin_editing
                && !view.saving
                && !view.processing
                && !view.color_picker_pending
                && !view.watermark_input_focused
                && view.input.marked.is_none()
                && view.input.drag_anchor.is_none()
                && view.pointer_down_start.is_none()
                && view.toolbar_drag.is_none()
                && view.toolbar_panel == crate::toolbar_layout::ToolbarPanel::None
                && !view.editor.is_dragging()
                && !view.editor.has_pending_drawing()
                && view.editor.text_draft.is_none()
                && view.editor.lasso_draft.is_none()
                && !view.editor.watermark.has_preview()
        })
    }
    fn text_mark_context_ready(&self, cx: &App) -> bool {
        self.text_mode
            && !self.state.locked
            && !self.saving
            && !self.processing
            && !self.processing_image_unavailable
            && !self.color_pick
            && self.pending_placement.is_none()
            && self.gesture.is_none()
            && self.drag_request.is_none()
            && self.text_anchor.is_none()
            && self.annotation_preview.visible()
            && self.text_selection_stamp == Some(self.text_selection_source(cx))
            && self.text_mark_canvas_idle(cx)
    }
    pub(crate) fn can_mark_selected_text(&self, cx: &App) -> bool {
        self.text_mark_context_ready(cx)
            && self.text_selection.as_ref().is_some_and(|text| {
                self.selected
                    .as_ref()
                    .is_some_and(|range| text.complete_marking_quads(range.clone()).is_ok())
            })
    }
    pub(crate) fn text_mark_history_source(&self, cx: &App) -> Option<TextSelectionStamp> {
        self.text_selection_stamp.filter(|stamp| {
            self.text_selection.is_some()
                && *stamp == self.text_selection_source(cx)
                && self.canvas.as_ref().is_some_and(|canvas| {
                    self.text_marks
                        .matches_underlay(&canvas.read(cx).editor.document)
                })
        })
    }
    pub(crate) fn rebase_text_marks_after_history(
        &mut self,
        before: Option<TextSelectionStamp>,
        cx: &App,
    ) {
        self.close_pin_sensitive();
        let current = self.text_selection_source(cx);
        let matches = self.canvas.as_ref().is_some_and(|canvas| {
            self.text_marks
                .matches_underlay(&canvas.read(cx).editor.document)
        });
        if text_mark_history_eligible(before, current, matches) {
            self.text_selection_stamp = Some(current);
            self.rebase_text_copy_layout(before, current);
        } else {
            self.validate_text_overlay(cx);
        }
    }
    fn mark_selected_text(
        &mut self,
        mark: TextMark,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if !self.text_mark_context_ready(cx) {
            return Err(tr!(
                "请先选中文字并结束当前操作，再应用标记",
                "Select text and finish the current action before marking"
            )
            .into());
        }
        let quads = self
            .text_selection
            .as_ref()
            .ok_or(tr!("请先选中文字", "Select text first"))?
            .complete_marking_quads(self.selected.clone().unwrap_or(0..0))?;
        self.mark_text_quads(mark, &quads, w, cx, |_, _, _| true)
    }
    fn mark_text_quads(
        &mut self,
        mark: TextMark,
        quads: &[[crate::annotation::Point; 4]],
        w: &Window,
        cx: &mut Context<Self>,
        authorize: impl FnOnce(&Self, &Window, &App) -> bool,
    ) -> Result<()> {
        let mut style = self
            .canvas
            .as_ref()
            .map_or_else(crate::annotation::Style::default, |canvas| {
                canvas.read(cx).editor.style
            });
        style.color = self
            .text_mark_color
            .unwrap_or(if mark == TextMark::Highlight {
                0xffd54f
            } else {
                style.color
            });
        let prepared = if let Some(canvas) = &self.canvas {
            let view = canvas.read(cx);
            crate::pin_annotation::prepare_text_mark(
                &self.text_marks,
                &view.editor.document,
                &view.scene,
                &view.source_pixels(),
                mark,
                quads,
                style,
            )?
        } else {
            // 在创建画布前验完计划和 Scene；失败不得因 canvas ID 改变丢失 OCR 层。
            let full = crate::geometry::Rect::full(self.image_size.0, self.image_size.1);
            let fill = self.vector_text_fill();
            let source = crate::scene::SourcePixels {
                fill,
                canvas: full,
                planes: if fill.is_some() {
                    Vec::new()
                } else {
                    vec![crate::scene::PixelPlane {
                        rect: full,
                        bgra: self.image.as_bytes(0).ok_or("贴图底图没有像素")?,
                    }]
                },
                overlay: None,
            };
            crate::pin_annotation::prepare_text_mark(
                &self.text_marks,
                &crate::document::Document::default(),
                &crate::scene::Scene::default(),
                &source,
                mark,
                quads,
                style,
            )?
        };
        let Some((edit, scene)) = prepared else {
            return Ok(());
        };
        // 查找整批在纯计划和 Scene 完成后、创建画布／提交历史前重新核授权。
        if !authorize(self, w, cx) {
            return Err(tr!(
                "识别结果已变化，请重新查找",
                "The recognized text changed; search again"
            )
            .into());
        }
        let created = self.canvas.is_none();
        self.ensure_document_canvas(w, cx)?;
        let canvas = self.canvas.as_ref().expect("标记文档已创建").clone();
        let committed = canvas.update(cx, |view, cx| {
            if !self.text_marks.commit(&mut view.editor.document, edit) {
                return false;
            }
            view.scene = Arc::new(scene);
            // 首个 render 仍负责创建 raster previews 与旧纹理退休；Scene 色块可复用。
            view.scene_stamp = None;
            cx.notify();
            true
        });
        if !committed {
            if created {
                self.canvas = None;
                self.canvas_subscription = None;
            }
            return Err("文字标记来源已改变，请重新选择".into());
        }
        self.redo_edits.clear();
        // 仅本次成功标记授权新 canvas/revision，选择位置和会话 ledger 保留。
        let before = self.text_selection_stamp;
        let current = self.text_selection_source(cx);
        self.text_selection_stamp = Some(current);
        self.rebase_text_copy_layout(before, current);
        self.text_show_selection = false;
        self.status = tr!(
            "标记已应用；可继续选字，Ctrl+Z 撤销",
            "Mark applied; select more text or use Ctrl+Z to undo"
        )
        .into();
        Ok(())
    }
    fn text_selection_source(&self, cx: &App) -> TextSelectionStamp {
        (
            self.image.id.0,
            self.canvas.as_ref().map(|canvas| canvas.entity_id()),
            self.canvas
                .as_ref()
                .map(|canvas| canvas.read(cx).editor.document.revision()),
            self.canvas.as_ref().and_then(|canvas| {
                let view = canvas.read(cx);
                if self.capture_source.is_some() {
                    view.output_rect().ok()
                } else {
                    view.editor.crop
                }
            }),
            self.annotation_preview.visible(),
        )
    }
    pub(crate) fn invalidate_text_overlay(&mut self) {
        self.close_pin_sensitive();
        self.ocr_sensitive = Default::default();
        self.cancel_pin_find_click();
        self.ocr_find = None;
        self.ocr_find_painted.borrow_mut().take();
        self.cancel_ocr_task();
        self.invalidate_text_copy_layout();
        self.text_marks.clear();
        self.text = None;
        self.text_source = ocr_clipboard::TextSource::None;
        self.text_selection = None;
        self.text_selection_stamp = None;
        self.text_mode = false;
        self.clear_text_selection();
    }
    fn validate_text_overlay(&mut self, cx: &App) {
        let current = self.text_selection_source(cx);
        // 尚在识别时没有 SelectionText stamp，必须单独核活动请求的源与代次。
        if self.ocr_task.is_active()
            && !self
                .ocr_task
                .source_matches(current, self.copy_layout_generation)
        {
            self.invalidate_text_overlay();
            self.status = tr!(
                "内容已改变，请重新识别文字",
                "Content changed; recognize text again"
            )
            .into();
        }
        if self
            .text_selection_stamp
            .is_some_and(|stamp| stamp != current)
        {
            self.invalidate_text_overlay();
        }
    }
    pub(crate) fn refresh_native_text_selection(&mut self, cx: &mut Context<Self>) -> Result<bool> {
        self.validate_text_overlay(cx);
        if !self.annotation_preview.visible() || self.vector_text.is_none() {
            return Ok(false);
        }
        let Some(canvas) = &self.canvas else {
            return Ok(false);
        };
        let view = canvas.read(cx);
        let items = view.editor.document.items();
        if view.editor.crop != Some(view.source.full())
            || view.editor.document.crop_mask.is_some()
            || items.is_empty()
            || items.len() > 64
            || items
                .iter()
                .any(|object| object.annotation.tool != crate::annotation::Tool::Text)
        {
            return Ok(false);
        }
        let annotations: Vec<_> = items
            .iter()
            .map(|object| object.annotation.clone())
            .collect();
        if !crate::pin_text_selection::SelectionText::native_region_eligible(
            &annotations,
            view.source.full(),
        ) {
            return Ok(false);
        }
        let selection = crate::pin_text_selection::SelectionText::from_annotations(&annotations)?;
        if !selection.has_geometry() {
            return Ok(false);
        }
        // 原生文字选择安装成功才替换 OCR 显示来源；失败保留原活动请求。
        self.cancel_ocr_task();
        self.clear_text_selection();
        self.text_marks.clear();
        self.invalidate_text_copy_layout();
        self.text_selection = Some(selection);
        self.text_selection_stamp = Some(self.text_selection_source(cx));
        self.text_source = ocr_clipboard::TextSource::Native;
        self.text_mode = true;
        Ok(true)
    }
    pub(crate) fn perform(&mut self, action: PinAction, w: &mut Window, cx: &mut Context<Self>) {
        // 无变化判断必须先于草稿收尾、seal、取消OCR请求及任何两栈/Redo写入。
        if processing_noop::should_skip(action, self.state.adjustments, || {
            let vector_text = self.vector_text.is_some();
            let capture_domain = self.capture_source.is_some();
            let editor = self.canvas.as_ref().map(|canvas| &canvas.read(cx).editor);
            processing_noop::requires_content_boundary(editor, vector_text, capture_domain)
        }) {
            if self.dismiss_context_menu(cx) {
                cx.notify();
            }
            return;
        }
        self.close_pin_sensitive();
        self.cancel_pin_find_click();
        if self.presentation_geometry_adjusting
            && !matches!(action, PinAction::Close | PinAction::Hide)
        {
            return;
        }
        self.validate_text_overlay(cx);
        if crate::watermark_toolbar_smoke::guard_pin_output(&action, w)
            || crate::watermark_long_memory_smoke::guard_pin_output(&action, w)
        {
            return;
        }
        self.dismiss_context_menu(cx);
        if self.saving {
            return;
        }
        if matches!(
            action,
            PinAction::Zoom(_) | PinAction::Original | PinAction::Fit | PinAction::Thumbnail
        ) && self.zoom_busy(cx)
        {
            return;
        }
        let result = (|| -> Result<()> {
            // 全文重试先走只读分支，不能借 CopyText 的编辑收尾改写历史。
            if matches!(action, PinAction::CopyText) {
                if let Some(text) = self.cached_ocr_text(cx) {
                    self.require_ocr_copy_idle(cx)?;
                    self.status = Self::publish_ocr_text(text, w)?;
                    self.status_notice_text.clear();
                    return Ok(());
                }
            }
            // 有当前结构时只读复制；没有结构则独立识别表格，保持图片与历史。
            if matches!(action, PinAction::CopyTableText) {
                if !self.can_copy_table_text(cx) {
                    return Err(tr!(
                        "没有识别出完整表格，或仍在输入",
                        "No complete table, or input is in progress"
                    )
                    .into());
                }
                if !self.current_table_structure_result(cx) {
                    self.recognize_mode(false, true, true, w, cx);
                    return Ok(());
                }
                self.publish_cached_table(w, cx)?;
                self.status = tr!("表格已复制", "Table copied").into();
                self.status_notice_text.clear();
                return Ok(());
            }
            // 菜单入口也先核草稿；不能借取色切换提交文字、续绘或套索。
            if matches!(action, PinAction::Color) && !self.color_mode_safe(cx) {
                return Err(tr!(
                    "请先结束当前编辑或输入，再切换取色",
                    "Finish the current edit or input before picking colors"
                )
                .into());
            }
            if matches!(
                action,
                PinAction::Duplicate | PinAction::Edit | PinAction::Crop
            ) {
                self.require_processing_image_available()?;
            }
            if matches!(
                action,
                PinAction::Hide
                    | PinAction::Lock
                    | PinAction::ClickThrough
                    | PinAction::Text
                    | PinAction::CopyText
                    | PinAction::Duplicate
                    | PinAction::Edit
                    | PinAction::Crop
                    | PinAction::Color
            ) {
                self.finish_canvas_content(cx)?;
            }
            match action {
                PinAction::WheelAnchor(anchor) => {
                    let mut preferences = crate::preferences::Preferences::load();
                    preferences.pin_zoom_anchor = anchor;
                    preferences.save()?;
                    self.pin_zoom_anchor = anchor;
                    self.reset_zoom_wheel();
                    cx.defer(move |cx| crate::pin_zoom::sync_open_preferences(anchor, cx));
                    self.status = trf!("滚轮缩放中心：{}", "Wheel zoom anchor: {}", anchor.label());
                }
                PinAction::EditTitle => self.begin_title_edit(w, cx),
                PinAction::Workspace => {
                    crate::pin_workspace_runtime::open_manager(
                        self.workspace.as_ref().map(|p| p.session.id.clone()),
                        cx,
                    )?;
                }
                PinAction::Close => {
                    self.end_color_pick(cx);
                    if self.workspace.is_some() {
                        if self.workspace_close(w, cx) {
                            self.cancel_ocr_task();
                        }
                        return Ok(());
                    }
                    self.persist(w, cx)?;
                    self.cancel_ocr_task();
                    self.close_toolbar(cx);
                    w.remove_window();
                }
                PinAction::Hide => unsafe {
                    self.end_color_pick(cx);
                    self.set_canvas_active(false, w, cx)?;
                    self.workspace_hide(cx)?;
                    self.close_toolbar(cx);
                    self.chrome = false;
                    crate::pin_shadow::hide_native(hwnd(w));
                    let _ = ShowWindowAsync(hwnd(w), SW_HIDE);
                },
                PinAction::Copy => {
                    if self.color_pick {
                        // 旁窗口菜单/工具栏保留的owner鼠标位置可能已过期。
                        if w.is_window_active() && w.is_window_hovered() {
                            if let Some(pointer) = Self::color_pointer(w) {
                                if self.refresh_color(pointer, w, cx) {
                                    self.copy_color_notice(cx);
                                }
                            } else {
                                self.clear_color_preview(cx);
                                self.status = tr!(
                                    "取色鼠标坐标不可用",
                                    "Pointer position unavailable for color picking"
                                )
                                .into();
                            }
                        } else {
                            self.clear_color_preview(cx);
                            self.status = tr!(
                                "移回图像后复制颜色",
                                "Move back over the image to copy the color"
                            )
                            .into();
                        }
                    } else if let Some(text) = self.selected_text() {
                        if self.text_source == ocr_clipboard::TextSource::Ocr {
                            self.require_ocr_copy_idle(cx)?;
                            self.status = Self::publish_ocr_selected_text(&text, w)?;
                            self.status_notice_text.clear();
                        } else {
                            self.copy_text_notice(text, cx);
                        }
                    } else {
                        self.export_user(crate::pin_export::UserExportAction::Copy, w, cx);
                    }
                }
                PinAction::Save => self.save(w, cx),
                PinAction::QuickSave => self.quick_save(w, cx),
                PinAction::CopySelectedText => {
                    if !self.text_mark_canvas_idle(cx) {
                        return Err(tr!(
                            "请先结束当前输入或编辑",
                            "Finish the current input or edit"
                        )
                        .into());
                    }
                    let text = self
                        .selected_text()
                        .ok_or(tr!("请先选中文字", "Select text first"))?;
                    if self.text_source == ocr_clipboard::TextSource::Ocr {
                        self.require_ocr_copy_idle(cx)?;
                        self.status = Self::publish_ocr_selected_text(&text, w)?;
                        self.status_notice_text.clear();
                    } else {
                        self.copy_text_notice(text, cx);
                    }
                }
                PinAction::CopyTableText => unreachable!("表格输出已走只读分支"),
                PinAction::CopyParagraphText => {
                    let text = self.copy_paragraph_text(cx)?;
                    self.status = Self::publish_ocr_selected_text(&text, w)?;
                    self.status_notice_text.clear();
                }
                PinAction::CopyText => {
                    if self
                        .canvas
                        .as_ref()
                        .is_some_and(|canvas| canvas.read(cx).text_composing())
                    {
                        return Err(tr!(
                            "请先完成输入法组合文字",
                            "Finish the IME composition first"
                        )
                        .into());
                    }
                    if self.text_selection_stamp == Some(self.text_selection_source(cx)) {
                        if let Some(text) =
                            self.text_selection.as_ref().map(|text| text.text.clone())
                        {
                            if self.text_source == ocr_clipboard::TextSource::Ocr {
                                self.require_ocr_copy_idle(cx)?;
                                self.status = Self::publish_ocr_text(&text, w)?;
                                self.status_notice_text.clear();
                            } else {
                                self.copy_text_notice(text, cx);
                            }
                            return Ok(());
                        }
                    }
                    let native = self.refresh_native_text_selection(cx)?;
                    if native {
                        if let Some(text) = self.text_selection.as_ref().map(|t| t.text.clone()) {
                            self.copy_text_notice(text, cx);
                        }
                        return Ok(());
                    }
                    if let Some(t) = &self.text {
                        // 缓存 OCR 仍属于宿主输出，不能绕过正在输入的组合串保护。
                        if self
                            .canvas
                            .as_ref()
                            .is_some_and(|canvas| canvas.read(cx).text_composing())
                        {
                            self.status =
                                tr!("请先完成输入法组合文字", "Finish the IME composition first")
                                    .into();
                            cx.notify();
                            return Ok(());
                        }
                        if self.text_source == ocr_clipboard::TextSource::Ocr {
                            self.require_ocr_copy_idle(cx)?;
                            self.status = Self::publish_ocr_text(&t.text, w)?;
                            self.status_notice_text.clear();
                        } else {
                            let text = t.text.clone();
                            self.copy_text_notice(text, cx);
                        }
                    } else {
                        self.recognize(false, true, w, cx);
                    }
                }
                PinAction::Topmost => {
                    self.state.topmost = !self.state.topmost;
                    native::topmost(hwnd(w), self.state.topmost)?;
                }
                PinAction::Shadow => {
                    self.state.shadow_enabled = !self.state.shadow_enabled;
                    if let Some(shadow) = &self.shadow {
                        shadow.set_presentation(
                            self.state.shadow_enabled,
                            w.is_window_active(),
                            self.state.locked,
                            crate::theme::palette(cx).muted,
                        )?;
                    }
                    self.status = if self.state.shadow_enabled {
                        tr!("窗口阴影已开启", "Window shadow on")
                    } else {
                        tr!("窗口阴影已关闭", "Window shadow off")
                    }
                    .into();
                }
                PinAction::Annotations => {
                    self.toggle_annotation_preview(w, cx)?;
                    self.invalidate_text_overlay();
                }
                PinAction::Lock => {
                    self.end_color_pick(cx);
                    self.state.locked = !self.state.locked;
                    self.reset_zoom_wheel();
                    self.finish_text_selection_drag();
                    self.cancel_drag(w);
                    self.set_canvas_active(self.chrome, w, cx)?;
                }
                PinAction::Chrome => self.toggle_toolbar(w, cx)?,
                PinAction::ClickThrough => {
                    self.end_color_pick(cx);
                    let enabled = !native::transparent(hwnd(w));
                    native::click_through(hwnd(w), enabled)?;
                    self.hovered = false;
                    self.chrome = false;
                    self.close_toolbar(cx);
                    self.set_canvas_active(false, w, cx)?;
                    if enabled {
                        self.status = tr!(
                            "鼠标已穿透 · 托盘右键 → 恢复贴图交互",
                            "Click-through on · tray right-click → Restore pin interaction"
                        )
                        .into();
                    } else {
                        self.status.clear();
                    }
                }
                PinAction::Opacity(value) => {
                    self.state.opacity = value.max(26);
                    native::opacity(hwnd(w), self.state.opacity)?;
                }
                PinAction::Ocr => self.recognize(false, false, w, cx),
                PinAction::Qr => self.recognize(true, false, w, cx),
                PinAction::MarkText(mark) => self.mark_selected_text(mark, w, cx)?,
                PinAction::TextMarkColor(color) => {
                    self.text_mark_color = Some(color & 0xffffff);
                    self.status = tr!(
                        "已选颜色；点击标记按钮应用",
                        "Color selected; click a mark button to apply"
                    )
                    .into();
                }
                PinAction::ClearTextSelection => self.clear_text_selection(),
                PinAction::Text => {
                    self.set_canvas_active(false, w, cx)?;
                    self.end_color_pick(cx);
                    let was_text_mode = self.text_mode;
                    if !was_text_mode
                        && self.text_selection.is_some()
                        && self.text_selection_stamp == Some(self.text_selection_source(cx))
                    {
                        self.text_mode = true;
                        self.clear_text_selection();
                        self.status.clear();
                    } else if !was_text_mode && self.refresh_native_text_selection(cx)? {
                        self.status.clear();
                    } else if self.text.is_none() && self.text_selection.is_none() {
                        self.recognize(false, false, w, cx);
                    } else {
                        self.text_mode = !was_text_mode;
                        self.clear_text_selection();
                        self.status.clear();
                    }
                }
                PinAction::Color => {
                    self.set_canvas_active(false, w, cx)?;
                    if self.processing {
                        return Ok(());
                    }
                    self.text_mode = false;
                    self.clear_text_selection();
                    if self.color_pick {
                        self.end_color_pick(cx);
                    } else {
                        self.start_color_pick(w, cx);
                    }
                }
                PinAction::Duplicate => {
                    let mut p = native::rect(hwnd(w))?;
                    p.x = p.x.checked_add(24).ok_or("复制贴图横坐标溢出")?;
                    p.y = p.y.checked_add(24).ok_or("复制贴图纵坐标溢出")?;
                    if self.vector_text.is_some() {
                        let document = self
                            .text_document_snapshot(cx)?
                            .ok_or("文字贴图复制缺少完整文档")?;
                        Self::open_text_document(&document, (p.x, p.y), self.state.clone(), cx)?;
                    } else {
                        Self::open_shared(
                            PinImages {
                                original: self.original.clone(),
                                image: self.image.clone(),
                                original_size: self.original_size,
                                image_size: self.image_size,
                                text: None,
                                style: self.presentation_style,
                                document: Some(self.raster_document_snapshot(cx)?),
                                processing_history: self.processing_history_snapshot()?,
                                capture_pixels: self.capture_pixels.clone(),
                            },
                            self.state.clone(),
                            p,
                            self.window_sizing,
                            None,
                            None,
                            cx,
                        )?;
                    }
                }
                _ if self.state.locked => {
                    self.status = tr!(
                        "已锁定交互；按 L 解锁",
                        "Interaction locked; press L to unlock"
                    )
                    .into()
                }
                PinAction::Zoom(z) => {
                    self.zoom_top_left(z, w, cx)?;
                    self.state.thumbnail = false;
                }
                PinAction::Original => {
                    self.zoom_top_left(1., w, cx)?;
                    self.state.thumbnail = false;
                }
                PinAction::Fit => {
                    let p = self.placement(w)?;
                    let work = native::work_area((p.x, p.y));
                    let zoom = window_zoom(
                        self.window_image_size(),
                        pin_model::fit(self.window_image_size(), work),
                    );
                    let sizing = self.window_sizing.after_qualified_zoom(true);
                    let (width, height) = pin_model::size(self.window_image_size(), zoom);
                    let placement = Placement {
                        x: work.x + (work.width as i32 - width as i32) / 2,
                        y: work.y + (work.height as i32 - height as i32) / 2,
                        width,
                        height,
                    };
                    self.validate_presentation_geometry(placement, zoom, sizing)?;
                    self.state.thumbnail = false;
                    self.state.zoom = zoom;
                    self.window_sizing = sizing;
                    self.request_placement(placement, w, cx);
                }
                PinAction::Thumbnail => {
                    if self.state.thumbnail {
                        self.user_zoom(self.state.normal_zoom, None, w, cx)?;
                        self.state.thumbnail = false;
                    } else {
                        let normal_zoom = self.state.zoom;
                        self.user_zoom(
                            (160.
                                / self.window_image_size().0.max(self.window_image_size().1)
                                    as f32)
                                .min(1.),
                            None,
                            w,
                            cx,
                        )?;
                        self.state.normal_zoom = normal_zoom;
                        self.state.thumbnail = true;
                    }
                }
                PinAction::Edit | PinAction::Crop => {
                    let handle = if let Some(style) = self.presentation_style {
                        crate::image_editor::open_pin_document(
                            self.pixels(),
                            style,
                            self.raster_document_snapshot(cx)?,
                            cx,
                        )?
                    } else {
                        crate::image_editor::open(self.output(cx)?, None, cx)?
                    };
                    if matches!(action, PinAction::Crop) {
                        handle.update(cx, |v, _, cx| {
                            v.editor.select_tool(crate::annotation::Tool::Crop);
                            cx.notify();
                        })?;
                    }
                }
                _ if self.processing => {}
                _ => {
                    if matches!(action, PinAction::Undo | PinAction::Redo) {
                        if self.annotation_history(matches!(action, PinAction::Redo), w, cx) {
                            return Ok(());
                        }
                        self.validate_processing_action(action, w, cx)?;
                        self.canvas = None;
                        self.canvas_subscription = None;
                    } else {
                        self.finish_canvas_content(cx)?;
                        self.validate_processing_action(action, w, cx)?;
                        self.seal_annotations(w, cx)?;
                        self.redo_edits.clear();
                    }
                    let old = self.state.adjustments;
                    let history = self.history.clone();
                    let mut next = old;
                    let changed = match action {
                        PinAction::Undo => self.history.undo(&mut self.state.adjustments),
                        PinAction::Redo => self.history.redo(&mut self.state.adjustments),
                        _ => {
                            match action {
                                PinAction::Rotate(delta) => {
                                    next = next.rotated(delta);
                                }
                                PinAction::FlipX => next.flip_x = !next.flip_x,
                                PinAction::FlipY => next.flip_y = !next.flip_y,
                                PinAction::Gray => next.grayscale = !next.grayscale,
                                PinAction::Invert => next.invert = !next.invert,
                                PinAction::Brightness(delta) => {
                                    next.brightness = (next.brightness + delta).clamp(-100, 100)
                                }
                                PinAction::ResetImage => next = Default::default(),
                                _ => {}
                            }
                            self.history.change(&mut self.state.adjustments, next)
                        }
                    };
                    if changed {
                        self.process(old, history, w, cx);
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.status = e.to_string();
            // 同一失败再次发生也应短暂显示，不能只依赖字符串变化。
            self.status_notice_text.clear();
        }
        self.sync_toolbar(w, cx);
        cx.notify();
    }
    fn clear_color_preview(&mut self, cx: &mut App) {
        self.color.clear();
        crate::pin_color_magnifier::close(self.color_magnifier.take(), cx);
    }
    pub(crate) fn end_color_pick(&mut self, cx: &mut App) {
        if self.color_pick {
            self.status.clear();
        }
        self.color_pick = false;
        self.color_keys_task.take();
        self.color_display.reset(false, false);
        self.clear_color_preview(cx);
    }
    fn color_keys() -> (bool, bool) {
        unsafe {
            (
                [VK_SHIFT, VK_LSHIFT, VK_RSHIFT]
                    .iter()
                    .any(|key| GetAsyncKeyState(key.0 as i32) < 0),
                GetAsyncKeyState(VK_P.0 as i32) < 0,
            )
        }
    }
    fn color_pointer(w: &Window) -> Option<gpui::Point<gpui::Pixels>> {
        let mut pointer = windows::Win32::Foundation::POINT::default();
        unsafe {
            GetCursorPos(&mut pointer).ok()?;
            if !windows::Win32::Graphics::Gdi::ScreenToClient(hwnd(w), &mut pointer).as_bool() {
                return None;
            }
        }
        Some(point(
            px(pointer.x as f32 / w.scale_factor()),
            px(pointer.y as f32 / w.scale_factor()),
        ))
    }
    fn color_keys_allowed(&self, w: &Window, cx: &App) -> bool {
        w.is_window_active()
            && self.color_mode_safe(cx)
            && !self.color_context_menu_open(cx)
            && !self.context_menu_press
            && !self.text_mode
            && self
                .canvas
                .as_ref()
                .is_none_or(|canvas| !canvas.read(cx).pin_editing)
    }
    fn color_context_menu_open(&self, cx: &App) -> bool {
        self.context_menu.is_some_and(|menu| {
            menu.read_with(cx, |view, _| view.is_open())
                .unwrap_or(false)
        })
    }
    fn observe_color_keys(&mut self, w: &Window, cx: &mut Context<Self>) {
        let (shift, p) = Self::color_keys();
        let allowed = self.color_keys_allowed(w, cx);
        if self.color_display.observe(shift, p, allowed) {
            if w.is_window_hovered() {
                if let Some(pointer) = Self::color_pointer(w) {
                    self.refresh_color(pointer, w, cx);
                } else {
                    self.clear_color_preview(cx);
                }
            } else {
                self.clear_color_preview(cx);
            }
            cx.notify();
        } else if !allowed && (!self.color.is_empty() || self.color_magnifier.is_some()) {
            self.clear_color_preview(cx);
            self.status = tr!(
                "取色暂停，结束当前操作后继续",
                "Color picking paused; it resumes after the current action"
            )
            .into();
            cx.notify();
        }
    }
    fn start_color_pick(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.color_pick = true;
        self.clear_color_preview(cx);
        let (shift, p) = Self::color_keys();
        self.color_display.reset(shift, p);
        self.status = tr!(
            "取色：Shift 格式，P 坐标，C 复制，Alt/Esc 退出",
            "Color picker: Shift format, P coordinates, C copy, Alt/Esc exit"
        )
        .into();
        if w.is_window_hovered() {
            if let Some(pointer) = Self::color_pointer(w) {
                self.refresh_color(pointer, w, cx);
            }
        }
        self.color_keys_task = Some(cx.spawn_in(w, async move |view, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let keep = view
                .update_in(cx, |v, w, cx| {
                    if !v.color_pick || !w.is_window_active() {
                        v.end_color_pick(cx);
                        return false;
                    }
                    v.observe_color_keys(w, cx);
                    true
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        }));
    }
    fn color_mode_safe(&self, cx: &App) -> bool {
        let busy = self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.state.locked
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some();
        self.canvas.as_ref().map_or(!busy, |canvas| {
            let view = canvas.read(cx);
            crate::pin_interaction::color_pick_alt_idle(
                Some(&view.editor),
                false,
                busy || view.saving
                    || view.processing
                    || view.pointer_down_start.is_some()
                    || view.toolbar_drag.is_some(),
                view.watermark_input_focused
                    || view.input.marked.is_some()
                    || view.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None,
            )
        })
    }
    fn refresh_color(
        &mut self,
        p: gpui::Point<gpui::Pixels>,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.color.clear();
        if !self.color_keys_allowed(w, cx) {
            self.clear_color_preview(cx);
            self.status = tr!(
                "贴图操作进行中，暂不可取色",
                "A pin action is in progress; color picking is unavailable"
            )
            .into();
            return false;
        }
        let viewport = self.content_viewport(w);
        let pointer = crate::annotation::Point {
            x: f32::from(p.x) * w.scale_factor(),
            y: f32::from(p.y) * w.scale_factor(),
        };
        match self.sample_content_patch(viewport, pointer, cx) {
            Ok(Some(patch)) => {
                let coordinates = (|| -> Result<_> {
                    let mut origin = windows::Win32::Foundation::POINT::default();
                    if !unsafe {
                        windows::Win32::Graphics::Gdi::ClientToScreen(hwnd(w), &mut origin)
                    }
                    .as_bool()
                    {
                        return Err(tr!(
                            "取色窗口坐标不可用",
                            "Window position unavailable for color picking"
                        )
                        .into());
                    }
                    let screen = (
                        origin
                            .x
                            .checked_add(pointer.x.round_ties_even() as i32)
                            .ok_or("取色屏幕坐标溢出")?,
                        origin
                            .y
                            .checked_add(pointer.y.round_ties_even() as i32)
                            .ok_or("取色屏幕坐标溢出")?,
                    );
                    let host_origin = (
                        origin
                            .x
                            .checked_add(viewport.offset.x.round_ties_even() as i32)
                            .ok_or("取色内容坐标溢出")?,
                        origin
                            .y
                            .checked_add(viewport.offset.y.round_ties_even() as i32)
                            .ok_or("取色内容坐标溢出")?,
                    );
                    Ok((screen, host_origin))
                })();
                let (screen, origin) = match coordinates {
                    Ok(value) => value,
                    Err(error) => {
                        self.clear_color_preview(cx);
                        self.status = error.to_string();
                        return false;
                    }
                };
                self.color = self.color_display.format_color(patch.sample.bgra);
                let coords = self.color_display.format_coords(screen, origin);
                self.status = trf!(
                    "{} · {} · C/单击复制",
                    "{} · {} · C/click copies",
                    self.color,
                    coords
                );
                let packet = crate::pin_color_magnifier::Packet {
                    patch,
                    color: self.color.clone(),
                    coords,
                    cursor: screen,
                };
                match crate::pin_color_magnifier::show(
                    self.color_magnifier.take(),
                    packet,
                    hwnd(w),
                    cx,
                ) {
                    Ok(view) => self.color_magnifier = Some(view),
                    Err(error) => {
                        self.status = trf!(
                            "{} · 放大镜：{error}",
                            "{} · Magnifier: {error}",
                            self.status
                        )
                    }
                }
                true
            }
            Ok(None) => {
                self.clear_color_preview(cx);
                self.status =
                    tr!("移回图像后取色", "Move back over the image to pick colors").into();
                false
            }
            Err(error) => {
                self.clear_color_preview(cx);
                self.status = trf!("取色失败：{error}", "Color picking failed: {error}");
                false
            }
        }
    }
    fn text_point(&self, p: gpui::Point<gpui::Pixels>, w: &Window) -> crate::annotation::Point {
        self.content_viewport(w)
            .image_point(crate::annotation::Point {
                x: f32::from(p.x) * w.scale_factor(),
                y: f32::from(p.y) * w.scale_factor(),
            })
    }
    fn text_drag_move(
        &mut self,
        e: &gpui::MouseMoveEvent,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.validate_text_overlay(cx);
        if self.text_anchor.is_none() {
            return false;
        }
        let captured = self
            .text_capture
            .is_some_and(|handle| unsafe { GetCapture() == handle });
        let point = self.text_point(e.position, w);
        if let Some(text) = &self.text_selection {
            // 仅接受本轮合法 Move；捕获丢失保留最后有效选区及其方向。
            if e.pressed_button == Some(MouseButton::Left) && captured {
                self.text_keyboard_selection = self
                    .text_anchor
                    .as_ref()
                    .and_then(|anchor| text.keyboard_selection_from_drag(anchor, point));
            }
            text.drag(
                &mut self.text_anchor,
                &mut self.selected,
                point,
                e.pressed_button == Some(MouseButton::Left),
                captured,
            );
        } else {
            self.text_anchor = None;
            self.text_keyboard_selection = None;
        }
        if self.text_anchor.is_none() {
            self.finish_text_selection_drag();
        }
        cx.notify();
        true
    }
    fn finish_text_selection_drag(&mut self) {
        self.text_anchor = None;
        if let Some(handle) = self.text_capture.take() {
            unsafe {
                if GetCapture() == handle {
                    let _ = ReleaseCapture();
                }
            }
        }
    }
    fn text_drag_up(&mut self, e: &gpui::MouseUpEvent, w: &Window, cx: &mut Context<Self>) -> bool {
        let active = self.text_anchor.is_some();
        self.validate_text_overlay(cx);
        if !active {
            return false;
        }
        // GPUI Windows 在派发 MouseUp 前已 ReleaseCapture；本轮本窗口的正常
        // 放开仍采样终点，另一个 HWND 拥有捕获时不提交旧手势。
        let owner = unsafe { GetCapture() };
        let accepted_release = self
            .text_capture
            .is_some_and(|handle| handle == hwnd(w) && (owner.0.is_null() || owner == handle));
        let point = self.text_point(e.position, w);
        if let Some(text) = &self.text_selection {
            if accepted_release {
                self.text_keyboard_selection = self
                    .text_anchor
                    .as_ref()
                    .and_then(|anchor| text.keyboard_selection_from_drag(anchor, point));
            }
            text.finish_drag(
                &mut self.text_anchor,
                &mut self.selected,
                point,
                accepted_release,
            );
        }
        self.finish_text_selection_drag();
        self.text_tools_ready = accepted_release && self.has_selected_text();
        cx.notify();
        true
    }
    fn text_navigation_key(
        key: &str,
        modifiers: gpui::Modifiers,
    ) -> Option<crate::pin_text_selection::NavigationKey> {
        use crate::pin_text_selection::NavigationKey;
        if modifiers.alt || modifiers.platform || modifiers.function {
            return None;
        }
        match (key, modifiers.control, modifiers.shift) {
            ("left", true, false) => Some(NavigationKey::PreviousWord),
            ("right", true, false) => Some(NavigationKey::NextWord),
            ("left", true, true) => Some(NavigationKey::ExtendPreviousWord),
            ("right", true, true) => Some(NavigationKey::ExtendNextWord),
            ("home", true, false) => Some(NavigationKey::DocumentHome),
            ("end", true, false) => Some(NavigationKey::DocumentEnd),
            ("home", true, true) => Some(NavigationKey::ExtendDocumentHome),
            ("end", true, true) => Some(NavigationKey::ExtendDocumentEnd),
            ("left", false, true) => Some(NavigationKey::ExtendPrevious),
            ("right", false, true) => Some(NavigationKey::ExtendNext),
            ("up", false, true) => Some(NavigationKey::ExtendPreviousLine),
            ("down", false, true) => Some(NavigationKey::ExtendNextLine),
            ("home", false, false) => Some(NavigationKey::Home),
            ("end", false, false) => Some(NavigationKey::End),
            ("home", false, true) => Some(NavigationKey::ExtendHome),
            ("end", false, true) => Some(NavigationKey::ExtendEnd),
            _ => None,
        }
    }
    fn text_navigation_menu_owns(&self, key: &str, modifiers: gpui::Modifiers, cx: &App) -> bool {
        Self::text_navigation_key(key, modifiers).is_some()
            && (self.context_menu_press || self.color_context_menu_open(cx))
    }
    /// 会话拥有按键不等于当前几何可导航；输入器优先，校验失败不能穿透移动。
    fn text_navigation_owned_by_session(
        key: &str,
        modifiers: gpui::Modifiers,
        source: ocr_clipboard::TextSource,
        text_mode: bool,
        native_input_owns: bool,
    ) -> Option<crate::pin_text_selection::NavigationKey> {
        if source != ocr_clipboard::TextSource::Ocr || !text_mode || native_input_owns {
            return None;
        }
        Self::text_navigation_key(key, modifiers)
    }
    /// 活动端仅为选字预览；不建立编辑光标、焦点或文档状态。
    fn text_caret_segment(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<[crate::annotation::Point; 2]> {
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || !self.text_mode
            || !self.text_show_selection
            || !window.is_window_active()
            || self.text_selection_stamp != Some(self.text_selection_source(cx))
            || self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.color_pick
            || self.pending_placement.is_some()
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || self.text_capture.is_some()
            || self.context_menu_press
            || self.color_context_menu_open(cx)
            || !self.text_mark_canvas_idle(cx)
        {
            return None;
        }
        let text = self.text_selection.as_ref()?;
        let cursor = self
            .text_keyboard_selection
            .filter(|cursor| self.selected.as_ref() == Some(&cursor.range()))?;
        text.caret_segment(cursor.caret_byte())
    }
    fn navigate_text_selection(
        &mut self,
        key: crate::pin_text_selection::NavigationKey,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || !self.text_mode
            || !window.is_window_active()
            || self.text_selection_stamp != Some(self.text_selection_source(cx))
            || self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.color_pick
            || self.pending_placement.is_some()
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || self.text_capture.is_some()
            || self.context_menu_press
            || self.color_context_menu_open(cx)
            || !self.text_mark_canvas_idle(cx)
        {
            return false;
        }
        let Some(text) = &self.text_selection else {
            return false;
        };
        let Some(cursor) = self
            .text_keyboard_selection
            .filter(|cursor| self.selected.as_ref() == Some(&cursor.range()))
        else {
            return false;
        };
        if text.keyboard_selection_from_range(cursor.range()).is_none() {
            return false;
        }
        let Some(next) = text.navigate_keyboard(cursor, key) else {
            // 当前停点合法但下一行缺少几何时停在原处，不能移动贴图。
            return true;
        };
        self.selected = Some(next.range());
        self.clear_find_preview();
        self.text_keyboard_selection = Some(next);
        self.text_show_selection = true;
        self.text_tools_ready = self.has_selected_text();
        cx.notify();
        // 合格端点无变化仍消费，不能回落为移动贴图。
        true
    }
    pub(crate) fn key(&mut self, e: &gpui::KeyDownEvent, w: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu_key(e, cx) {
            cx.stop_propagation();
            return;
        }
        // TextField 未消费的 IME／字符键也不能落到图片复制或窗口命令。
        if self.ocr_find_input_focused(w, cx) || self.title_editor_focused(w, cx) {
            return;
        }
        let m = e.keystroke.modifiers;
        let key = e.keystroke.key.as_str();
        let native_input_owns = self.canvas.as_ref().is_some_and(|canvas| {
            let view = canvas.read(cx);
            view.editor.text_draft.is_some()
                || view.watermark_input_focused
                || view.result_editor.is_some()
                || view.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None
                || view.ocr_overlay_host_blocked()
        });
        // 来源校验可能清空旧 OCR 状态；本次事件仍由校验前的会话拥有。
        let owned_navigation = Self::text_navigation_owned_by_session(
            key,
            m,
            self.text_source,
            self.text_mode,
            native_input_owns,
        );
        self.validate_text_overlay(cx);
        if self.text_navigation_menu_owns(key, m, cx) {
            cx.stop_propagation();
            return;
        }
        if self.dismiss_context_menu(cx) && key == "escape" {
            cx.stop_propagation();
            return;
        }
        if self
            .canvas
            .as_ref()
            .is_some_and(|c| c.read(cx).watermark_input_focused)
        {
            return;
        }
        if self.canvas.as_ref().is_some_and(|canvas| {
            let view = canvas.read(cx);
            view.editor.text_draft.is_some() && view.input.marked.is_some()
        }) {
            // 原生输入法先处理候选确认/取消；不进入宿主的复制、收栏或关闭。
            return;
        }
        if crate::text_contract::terminal_held(key, e.is_held) {
            cx.stop_propagation();
            return;
        }
        if self.saving || self.drag_request.is_some() {
            cx.stop_propagation();
            return;
        }
        if let Some(canvas) = &self.canvas {
            let panel = canvas.read(cx).toolbar_panel;
            if panel != crate::toolbar_layout::ToolbarPanel::None && !panel.more() {
                if key == "escape" {
                    canvas.update(cx, |v, cx| {
                        v.pin_key(e, w, cx);
                    });
                    cx.stop_propagation();
                    cx.notify();
                }
                // 参数小窗复用宿主键路由：数字/字母输入不能旋转、关闭或输出贴图。
                return;
            }
        }
        // 查找字段／原生 IME／参数栏已先处理；候选焦点不能穿透图片命令。
        if !native_input_owns && self.pin_local_candidate_key(e, w, cx) {
            return;
        }
        if key == "space" && e.is_held {
            return;
        }
        if key == "escape" && (self.color_pick || self.text_mode) {
            self.end_color_pick(cx);
            self.text_mode = false;
            self.clear_text_selection();
            self.status.clear();
            cx.notify();
            return;
        }
        if let Some(key) = owned_navigation {
            self.navigate_text_selection(key, w, cx);
            // 无 cursor、旧来源、忙态或缺几何都只拒绝导航，不改归属。
            cx.stop_propagation();
            return;
        }
        if self.text_mode && m.control && !m.alt && !m.platform && !m.function {
            if key == "a" {
                self.clear_find_preview();
                self.finish_text_selection_drag();
                if let Some(text) = &self.text_selection {
                    let range = 0..text.text.len();
                    self.text_keyboard_selection =
                        text.keyboard_selection_from_range(range.clone());
                    self.selected = Some(range);
                }
                self.text_show_selection = true;
                self.text_tools_ready = self.has_selected_text();
                cx.stop_propagation();
                cx.notify();
                return;
            }
            if key == "c" {
                if m.shift {
                    self.perform(PinAction::CopyText, w, cx);
                } else if self.selected_text().is_some() {
                    self.perform(PinAction::Copy, w, cx);
                }
                cx.stop_propagation();
                return;
            }
        }
        // 精确 Ctrl+Alt+C 在取色及泛 Ctrl+C 前处理；没有 OCR 合法选择也不回退输出。
        if ocr_copy::shortcut(key, m) {
            // 活跃标注文字仍交原生输入（包括 AltGr）；不能抢走其字符键。
            if self
                .canvas
                .as_ref()
                .is_some_and(|canvas| canvas.read(cx).editor.text_draft.is_some())
            {
                return;
            }
            if !e.is_held {
                self.perform(PinAction::CopyParagraphText, w, cx);
            }
            cx.stop_propagation();
            return;
        }
        // 取色路由晚于输入守卫、早于画布工具；P不能误选折线，Shift+C不触发OCR。
        if self.color_pick && self.color_keys_allowed(w, cx) {
            if key == "c"
                && (!m.modified()
                    || crate::host_shortcuts::shift(m)
                    || (crate::host_shortcuts::control(m) && !m.shift))
            {
                // 复制会刷新采样，先消费当前按键边沿，避免为同一次 C 合成两次。
                let (shift, p) = Self::color_keys();
                self.color_display.observe(shift, p, true);
                self.perform(PinAction::Copy, w, cx);
                cx.stop_propagation();
                return;
            }
            self.observe_color_keys(w, cx);
            if key == "p" && (!m.modified() || crate::host_shortcuts::shift(m)) {
                cx.stop_propagation();
                return;
            }
        }
        if self.chrome && !self.state.locked {
            if let Some(canvas) = &self.canvas {
                let handled = canvas.update(cx, |v, cx| v.pin_key(e, w, cx));
                if handled {
                    if canvas.read(cx).pin_editing {
                        self.end_color_pick(cx);
                        self.text_mode = false;
                        self.clear_text_selection();
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if canvas.read(cx).editor.text_draft.is_some() && !(m.control && key == "s") {
                    // 未消费的文字按键走原生字符输入，不执行贴图旋转/亮度/空格工具栏快捷键。
                    return;
                }
            }
        }
        // 展开主栏时数字只属于工具栏；锁定、无画布或修饰键也不能进入图片处理。
        // 原生正文输入继续让行；精确无修饰的空槽／不可编辑数字则消费。
        if self.chrome && crate::toolbar_groups::shortcut_digit(key).is_some() {
            let native_text = self
                .canvas
                .as_ref()
                .is_some_and(|canvas| canvas.read(cx).editor.text_draft.is_some());
            if !native_text && !m.shift && !m.control && !m.alt && !m.platform && !m.function {
                cx.stop_propagation();
            }
            return;
        }
        let native_text = self
            .canvas
            .as_ref()
            .is_some_and(|canvas| canvas.read(cx).editor.text_draft.is_some());
        // 工具栏展开且未锁定时 F2 已交给画布的对象编辑；只有收起（或锁定）时才编辑标题。
        if key == "f2" && !m.modified() && (!self.chrome || self.state.locked) && !native_text {
            if !e.is_held {
                self.perform(PinAction::EditTitle, w, cx);
            }
            cx.stop_propagation();
            return;
        }
        let action = crate::host_shortcuts::pin_action(key, m, self.chrome, native_text);
        if let Some(action) = action {
            // 新按下可真正重试，按键长按不能反复取消并替换仍在运行的 OCR。
            if e.is_held
                && (matches!(action, PinAction::Ocr)
                    || (matches!(action, PinAction::CopyText) && self.ocr_task.is_active()))
            {
                cx.stop_propagation();
                return;
            }
            if !crate::pin_keys::allow_held_shortcut(action, e.is_held) {
                cx.stop_propagation();
                return;
            }
            self.perform(action, w, cx);
        } else if let Some((dx, dy)) =
            crate::pin_interaction::keyboard_move_delta(key, m, self.state.locked)
        {
            if let Ok(mut p) = self.placement(w) {
                p.x += dx;
                p.y += dy;
                self.request_placement(p, w, cx);
            }
        }
    }
    pub(crate) fn clear_text_selection(&mut self) {
        self.clear_find_preview();
        self.selected = None;
        self.text_keyboard_selection = None;
        self.text_show_selection = true;
        self.text_tools_ready = false;
        self.finish_text_selection_drag();
    }
    fn color_pick_alt_allowed(&self, window: &Window, cx: &App) -> bool {
        let busy = !window.is_window_active()
            || self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.state.locked
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || self.color_context_menu_open(cx)
            || self.context_menu_press;
        if let Some(canvas) = &self.canvas {
            let view = canvas.read(cx);
            crate::pin_interaction::color_pick_alt_idle(
                Some(&view.editor),
                view.pin_editing,
                busy || view.saving
                    || view.processing
                    || view.pointer_down_start.is_some()
                    || view.toolbar_drag.is_some(),
                view.watermark_input_focused
                    || view.input.marked.is_some()
                    || view.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None,
            )
        } else {
            crate::pin_interaction::color_pick_alt_idle(None, false, busy, false)
        }
    }
}
impl PinView {
    fn render_text_tools(&self, w: &Window, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        if !self.text_tools_ready
            || !self.has_selected_text()
            || self.saving
            || self.processing
            || self.processing_image_unavailable
            || self.pending_placement.is_some()
            || self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || !self.text_mark_canvas_idle(cx)
            || self.text_selection_stamp != Some(self.text_selection_source(cx))
        {
            return None;
        }
        let text = self.text_selection.as_ref()?;
        let quads = text.highlights(self.selected.clone()?);
        let viewport = w.viewport_size();
        let dpi = w.scale_factor();
        let mut origin = windows::Win32::Foundation::POINT::default();
        if !unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd(w), &mut origin) }.as_bool()
        {
            return None;
        }
        let work = native::work_area((origin.x, origin.y));
        // 一级标注栏是独立 HWND；使用真正可见的当前位置，不用预测布局挤掉迷你栏。
        let toolbar_obstacle = self
            .toolbar
            .and_then(|bar| bar.read(cx).ok())
            .and_then(|bar| bar.visible_bounds())
            .and_then(|bounds| text_tools::client_obstacle(bounds, (origin.x, origin.y), dpi));
        let obstacles: Vec<_> = toolbar_obstacle.into_iter().collect();
        let layout = text_tools::strip_layout_avoiding(
            &quads,
            self.content_viewport(w),
            dpi,
            (f32::from(viewport.width), f32::from(viewport.height)),
            crate::annotation::Bounds {
                x: (i64::from(work.x) - i64::from(origin.x)) as f32 / dpi,
                y: (i64::from(work.y) - i64::from(origin.y)) as f32 / dpi,
                width: work.width as f32 / dpi,
                height: work.height as f32 / dpi,
            },
            &obstacles,
        )?;
        let palette = crate::theme::palette(cx);
        let can_mark = self.can_mark_selected_text(cx);
        let compact = layout.compact;
        let button_width = 30.;
        let control_height = if compact { 24. } else { 28. };
        let separator = || {
            div()
                .w(px(1.))
                .h(px(18.))
                .mx(px(4.))
                .flex_shrink_0()
                .bg(rgb(palette.border))
        };
        let style = self
            .canvas
            .as_ref()
            .map_or_else(crate::annotation::Style::default, |canvas| {
                canvas.read(cx).editor.style
            });
        let default_color = style.color;
        let selected_quads = can_mark
            .then(|| {
                text.complete_marking_quads(self.selected.clone().unwrap_or(0..0))
                    .ok()
            })
            .flatten();
        let mut actions = div().flex().items_center().gap(px(2.));
        for (index, (id, label, action)) in [
            (
                "pin-text-copy",
                tr!("复制所选文字", "Copy selected text"),
                PinAction::CopySelectedText,
            ),
            (
                "pin-text-highlight",
                TextMark::Highlight.label(),
                PinAction::MarkText(TextMark::Highlight),
            ),
            (
                "pin-text-underline",
                TextMark::Underline.label(),
                PinAction::MarkText(TextMark::Underline),
            ),
            (
                "pin-text-wave",
                TextMark::Wave.label(),
                PinAction::MarkText(TextMark::Wave),
            ),
            (
                "pin-text-strike",
                TextMark::Strike.label(),
                PinAction::MarkText(TextMark::Strike),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if index == 1 {
                actions = actions.child(separator());
            }
            let enabled = !matches!(action, PinAction::MarkText(_)) || can_mark;
            let label = if enabled {
                label
            } else {
                tr!(
                    "标记需要完整文字定位和可用输入；复制不需要文字定位",
                    "Needs full text positions and ready input; copy needs no positions"
                )
            };
            let active = match action {
                PinAction::MarkText(mark) if enabled => {
                    selected_quads.as_ref().is_some_and(|quads| {
                        self.canvas.as_ref().is_some_and(|canvas| {
                            let view = canvas.read(cx);
                            self.text_marks
                                .selection_fully_marked(
                                    &view.editor.document,
                                    mark,
                                    quads,
                                    crate::annotation::Style {
                                        color: self.text_mark_color.unwrap_or(
                                            if mark == TextMark::Highlight {
                                                crate::toolbar_icons::TEXT_MARK_COLORS[0]
                                            } else {
                                                default_color
                                            },
                                        ),
                                        ..style
                                    },
                                )
                                .unwrap_or(false)
                        })
                    })
                }
                _ => false,
            };
            let glyph = match action {
                PinAction::MarkText(mark) => crate::toolbar_icons::text_mark(
                    mark,
                    self.text_mark_color
                        .unwrap_or(if mark == TextMark::Highlight {
                            crate::toolbar_icons::TEXT_MARK_COLORS[0]
                        } else {
                            default_color
                        }),
                )
                .into_any_element(),
                _ => crate::toolbar_icons::action(
                    match action {
                        PinAction::CopySelectedText => crate::toolbar_icons::Icon::Copy,
                        PinAction::ClearTextSelection => crate::toolbar_icons::Icon::SelectionRange,
                        _ => crate::toolbar_icons::Icon::Close,
                    },
                    palette.foreground,
                )
                .size(px(16.))
                .into_any_element(),
            };
            actions = actions.child(
                div()
                    .id(id)
                    .w(px(button_width))
                    .h(px(control_height))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .opacity(if enabled { 1. } else { 0.4 })
                    .when(active, |b| {
                        b.bg(rgb(palette.selected)).text_color(rgb(palette.accent))
                    })
                    .tooltip(move |_, cx| cx.new(move |_| TextToolHint(label)).into())
                    .when(enabled, |button| {
                        button.cursor_pointer().hover(move |s| {
                            s.bg(rgb(if active {
                                palette.selected
                            } else {
                                palette.hover
                            }))
                        })
                    })
                    .child(glyph)
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if enabled {
                            v.perform(action, w, cx);
                        }
                        cx.stop_propagation();
                    })),
            );
        }
        let mut colors = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .h(px(control_height));
        for (color, label) in crate::toolbar_icons::TEXT_MARK_COLORS.into_iter().zip([
            tr!("黄色（下次标记）", "Yellow for the next mark"),
            tr!("红色（下次标记）", "Red for the next mark"),
            tr!("蓝色（下次标记）", "Blue for the next mark"),
            tr!("绿色（下次标记）", "Green for the next mark"),
        ]) {
            colors = colors.child(
                div()
                    .id(("pin-text-color", color))
                    .w(px(22.))
                    .h(px(control_height))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .size(px(14.))
                            .rounded_full()
                            .bg(rgb(color))
                            .border_2()
                            .border_color(rgb(if self.text_mark_color == Some(color) {
                                palette.foreground
                            } else {
                                palette.border
                            })),
                    )
                    .tooltip(move |_, cx| cx.new(move |_| TextToolHint(label)).into())
                    .on_click(cx.listener(move |v, _, w, cx| {
                        v.perform(PinAction::TextMarkColor(color), w, cx);
                        cx.stop_propagation();
                    })),
            );
        }
        let mut bar = div()
            .id("pin-selected-text-tools")
            .absolute()
            .left(px(layout.x))
            .top(px(layout.y))
            .w(px(layout.width))
            .h(px(layout.height))
            .p(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .gap(px(if compact { 4. } else { 2. }))
            .rounded_lg()
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.background))
            .shadow_md()
            .text_color(rgb(palette.foreground))
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .cursor(gpui::CursorStyle::Arrow)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        if compact {
            bar = bar.flex_col();
        }
        bar = bar.child(actions);
        if !compact {
            bar = bar.child(separator());
        }
        Some(bar.child(colors).into_any_element())
    }

    fn copy_text_notice(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.status = tr!("文字已复制", "Text copied").into();
        self.status_notice_text.clear();
        cx.notify();
    }

    fn copy_color_notice(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.color.clone()));
        self.status = trf!("已复制 {}", "Copied {}", self.color);
        self.status_notice_text.clear();
        cx.notify();
    }

    /// 不改变可见状态的操作（如锁定时滚轮）直接给出气泡反馈。
    pub(crate) fn flash_operation_hint(&mut self, text: String, cx: &mut Context<Self>) {
        self.operation_hint = text;
        self.operation_hint_deadline = Some(Instant::now() + PIN_OPERATION_HINT_DURATION);
        self.status_notice_deadline = None;
        cx.notify();
    }

    /// 定时器先在淡出开始时唤醒一次（开始逐帧淡出），再在到期时收起。
    fn notice_wake(&self, now: Instant) -> Option<Instant> {
        [self.status_notice_deadline, self.operation_hint_deadline]
            .into_iter()
            .flatten()
            .map(|deadline| {
                deadline
                    .checked_sub(PIN_NOTICE_FADE_OUT)
                    .filter(|fade| *fade > now)
                    .unwrap_or(deadline)
            })
            .min()
    }

    /// 左上角气泡本帧显示的文字与不透明度，以及左下角是否回看状态消息。
    /// 操作反馈约 1.5 秒、状态消息约 4 秒后自动隐藏；后到的一条覆盖先到的。
    fn notice_bubble(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (Option<(String, f32)>, bool) {
        let now = Instant::now();
        let state = pin_model::OperationHintState::of(&self.state, self.physical_geometry.size);
        // 首帧只记基线：恢复、首显和自动适应不算用户操作。
        if let Some(previous) = self.operation_hint_state.replace(state) {
            if let Some(text) = pin_model::operation_hint(previous, state) {
                self.operation_hint = text;
                self.operation_hint_deadline = Some(now + PIN_OPERATION_HINT_DURATION);
                self.status_notice_deadline = None;
            }
        }
        // 同帧的状态消息解释了原因（失败、自动缩小），优先于状态变化本身。
        if self.status_notice_text != self.status {
            self.status_notice_text.clone_from(&self.status);
            self.status_notice_deadline =
                (!self.status.is_empty()).then(|| now + std::time::Duration::from_secs(4));
            if self.status_notice_deadline.is_some() {
                self.operation_hint_deadline = None;
            }
        }
        if let Some(next) = self
            .notice_wake(now)
            .filter(|next| self.notice_timer_wake.is_none_or(|wake| *next < wake))
        {
            // 快速变化的取色消息只顺延到期时间，不为每个 Move 排任务；
            // 只有更早到期（状态消息被短反馈覆盖）时才换代重排。
            self.notice_timer_wake = Some(next);
            self.notice_timer_generation = self.notice_timer_generation.wrapping_add(1);
            let generation = self.notice_timer_generation;
            cx.spawn_in(window, async move |view, cx| loop {
                let delay = view
                    .update_in(cx, |v, _, cx| {
                        if v.notice_timer_generation != generation {
                            return None;
                        }
                        let now = Instant::now();
                        for deadline in [
                            &mut v.status_notice_deadline,
                            &mut v.operation_hint_deadline,
                        ] {
                            if deadline.is_some_and(|deadline| deadline <= now) {
                                *deadline = None;
                            }
                        }
                        // 每次唤醒不是淡出开始就是到期，都需要重绘。
                        cx.notify();
                        v.notice_timer_wake = v.notice_wake(now);
                        v.notice_timer_wake
                            .map(|wake| wake.saturating_duration_since(now))
                    })
                    .ok()
                    .flatten();
                let Some(delay) = delay else { break };
                cx.background_executor().timer(delay).await;
            })
            .detach();
        }
        let active = |deadline: Option<Instant>| deadline.is_some_and(|deadline| now < deadline);
        let dragging = self.status_dragging(cx);
        let status_hovered = self.status_hover_at(window.mouse_position(), window, cx);
        let review = !dragging && !self.status.is_empty() && status_hovered;
        // 拖边缩放时倍率反馈照常跟随；状态消息在实际拖绘时让出图片。
        let bubble = if active(self.operation_hint_deadline) && !self.operation_hint.is_empty() {
            self.operation_hint_deadline
                .map(|deadline| (self.operation_hint.clone(), deadline))
        } else if active(self.status_notice_deadline)
            && !review
            && !dragging
            && !self.status.is_empty()
        {
            self.status_notice_deadline
                .map(|deadline| (self.status.clone(), deadline))
        } else {
            None
        };
        let Some((text, deadline)) = bubble else {
            self.notice_shown_at = None;
            return (None, review);
        };
        // 从隐藏到出现才淡入；可见期间换文字或顺延到期不重新淡入。
        let shown = *self.notice_shown_at.get_or_insert(now);
        let fade_in =
            now.saturating_duration_since(shown).as_secs_f32() / PIN_NOTICE_FADE_IN.as_secs_f32();
        let fade_out = deadline.saturating_duration_since(now).as_secs_f32()
            / PIN_NOTICE_FADE_OUT.as_secs_f32();
        let opacity = fade_in.min(fade_out).clamp(0., 1.);
        if opacity < 1. {
            // 只在约 0.1–0.2 秒的渐变期间逐帧重绘。
            window.request_animation_frame();
        }
        (Some((text, opacity)), review)
    }

    fn status_dragging(&self, cx: &App) -> bool {
        self.gesture.is_some()
            || self.drag_request.is_some()
            || self.text_anchor.is_some()
            || self.canvas.as_ref().is_some_and(|canvas| {
                let canvas = canvas.read(cx);
                canvas.pointer_down_start.is_some()
                    || canvas.input.drag_anchor.is_some()
                    || canvas.editor.is_dragging()
            })
    }

    fn status_hover_at(
        &self,
        pointer: gpui::Point<gpui::Pixels>,
        window: &Window,
        cx: &App,
    ) -> bool {
        let viewport = window.viewport_size();
        // 参数/选字/取色模式也能悬浮回看错误；实际拖绘时仍让出图片。
        // 错误回看不沿操作栏的最小尺寸门，窄贴图同样可用。
        let (x, y) = (f32::from(pointer.x), f32::from(pointer.y));
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        self.hovered
            && !self.status_dragging(cx)
            && [x, y, width, height].into_iter().all(f32::is_finite)
            && x >= 0.
            && x < width.min(120.)
            && y >= 0.
            && y >= height - 28.
            && y < height
    }

    fn corner_overlay_at(
        &self,
        pointer: gpui::Point<gpui::Pixels>,
        window: &Window,
        cx: &App,
    ) -> CornerOverlayVisibility {
        let interacting = self.chrome
            || self.text_mode
            || self.color_pick
            || self.saving
            || self.gesture.is_some()
            || self.text_anchor.is_some()
            || self.drag_request.is_some()
            || self
                .canvas
                .as_ref()
                .is_some_and(|canvas| canvas.read(cx).pin_editing);
        let viewport = window.viewport_size();
        CornerOverlayVisibility::at(
            (f32::from(pointer.x), f32::from(pointer.y)),
            (f32::from(viewport.width), f32::from(viewport.height)),
            self.hovered,
            interacting,
        )
    }

    fn track_corner_overlays(
        &mut self,
        pointer: gpui::Point<gpui::Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.corner_overlay_at(pointer, window, cx);
        let status_hovered = self.status_hover_at(pointer, window, cx);
        if next != self.corner_overlays || self.status_hovered != status_hovered {
            self.corner_overlays = next;
            self.status_hovered = status_hovered;
            cx.notify();
        }
    }
}
impl Render for PinView {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_text_theme(cx);
        self.validate_text_overlay(cx);
        self.validate_ocr_find(cx);
        self.validate_pin_sensitive(w, cx);
        if !self.pin_sensitive_preview_open() {
            self.sync_pin_local_ocr(w, cx);
        }
        crate::theme::sync_window(w, cx);
        self.sync_pin_title(w, cx);
        let palette = crate::theme::palette(cx);
        if self.correct_actual_presentation_geometry(w, cx) {
            self.clear_pin_ocr_progress_clock();
            // 复用既有焦点身份，使原画布焦点下的 Escape 仍可到达；不设置焦点。
            let suspended_focus = w.focused(cx).unwrap_or_else(|| self.focus.clone());
            return div()
                .id("pin-image-root")
                .size_full()
                .relative()
                .overflow_hidden()
                .track_focus(&suspended_focus)
                .flex()
                .items_center()
                .justify_center()
                .on_key_down(cx.listener(|v, event: &gpui::KeyDownEvent, w, cx| {
                    if event.keystroke.key == "escape" {
                        v.perform(PinAction::Close, w, cx);
                    }
                    cx.stop_propagation();
                }))
                .child(
                    div()
                        .max_w(px(360.))
                        .m_2()
                        .p_3()
                        .rounded_md()
                        .bg(rgb(palette.background))
                        .text_color(rgb(palette.foreground))
                        .font_family("Microsoft YaHei UI")
                        .text_size(px(12.))
                        .child(self.status.clone()),
                );
        }
        let corner_overlays = self.corner_overlay_at(w.mouse_position(), w, cx);
        self.corner_overlays = corner_overlays;
        self.status_hovered = self.status_hover_at(w.mouse_position(), w, cx);
        if (self.presentation_style.is_some()
            || self.image_size.0 > MAX_PIN_WINDOW_SIDE
            || self.image_size.1 > MAX_PIN_WINDOW_SIDE)
            && self.canvas.is_none()
            && !self.processing
        {
            // 整图 Arc 只保存 CPU 原始像素；第一帧上传前接入分块画布。
            if let Err(error) = self.ensure_document_canvas(w, cx) {
                self.status = error.to_string();
            } else if let Err(error) = self.set_canvas_active(self.chrome, w, cx) {
                self.status = error.to_string();
            }
        }
        if !native::transparent(hwnd(w))
            && self
                .status
                .starts_with(tr!("鼠标已穿透", "Click-through on"))
        {
            self.status.clear();
        }
        self.reconcile_preview_textures(w, cx);
        let (notice_bubble, status_visible) = self.notice_bubble(w, cx);
        // 捕获手势要在窗口级接收越界移动/松开；GPUI 监听器必须在 paint 阶段注册。
        let view = cx.entity().downgrade();
        let capture_events = gpui::canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let weak = view.clone();
                window.on_modifiers_changed(move |event, window, cx| {
                    let _ = weak.update(cx, |v, cx| {
                        let mouse_held = event.physical_key.is_some_and(|key| {
                            matches!(
                                key.key,
                                gpui::ModifierKey::LeftAlt | gpui::ModifierKey::RightAlt
                            )
                        }) && unsafe {
                            [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
                                .iter()
                                .any(|key| GetAsyncKeyState(key.0 as i32) < 0)
                        };
                        let allowed = !mouse_held && v.color_pick_alt_allowed(window, cx);
                        if v.color_pick_alt.observe(event, allowed) {
                            v.perform(PinAction::Color, window, cx);
                        }
                        if v.color_pick {
                            v.observe_color_keys(window, cx);
                        }
                        if v.text_mode {
                            cx.notify();
                        }
                    });
                });
                let weak = view.clone();
                window.on_mouse_event(move |e: &gpui::ScrollWheelEvent, phase, w, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        let _ = weak.update(cx, |v, cx| {
                            v.color_pick_alt.reset();
                            if v.redirect_context_menu_wheel(e, w, cx) {
                                cx.stop_propagation();
                            }
                        });
                    }
                });
                let weak = view.clone();
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, w, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        let _ = weak.update(cx, |v, cx| {
                            v.color_pick_alt.reset();
                            if v.text_drag_move(e, w, cx) || v.drag_move(e, w, cx) {
                                cx.stop_propagation();
                            }
                        });
                    }
                });
                let weak = view.clone();
                window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, w, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        let _ = weak.update(cx, |v, cx| {
                            v.color_pick_alt.reset();
                            if e.button != MouseButton::Left {
                                return;
                            }
                            v.schedule_pin_find_release_cleanup(w, cx);
                            v.schedule_pin_sensitive_release_cleanup(w, cx);
                            if v.text_drag_up(e, w, cx) {
                                cx.stop_propagation();
                            } else if v.gesture.is_some() {
                                let accepted_release = v.finish_drag(e, w, cx);
                                if v.color_pick && accepted_release && v.pending_placement.is_none()
                                {
                                    v.refresh_color(e.position, w, cx);
                                } else if v.color_pick {
                                    v.clear_color_preview(cx);
                                }
                                cx.notify();
                                cx.stop_propagation();
                            }
                        });
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let presentation_plan = self.presentation_style.and_then(|style| {
            let viewport = self.content_viewport(w);
            let content = self.content_rect();
            let at = viewport.window_point(crate::annotation::Point {
                x: content.x as f32,
                y: content.y as f32,
            });
            match crate::pin_presentation::PresentationPreviewPlan::new(
                content,
                style,
                crate::pin_presentation::PhysicalPoint {
                    x: f64::from(at.x),
                    y: f64::from(at.y),
                },
                f64::from(viewport.zoom),
            ) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    self.status = error.to_string();
                    None
                }
            }
        });
        if let Some(shadow) = &self.shadow {
            // 窗口阴影沿外观轮廓（含外侧边框）的同一圆角渲染，圆角外的透明四角不露直角光晕。
            shadow.set_outline(presentation_plan.map(|plan| {
                let outline = plan.outline();
                crate::pin_shadow::ShadowOutline {
                    left: outline.bounds.left,
                    top: outline.bounds.top,
                    width: outline.bounds.width,
                    height: outline.bounds.height,
                    radius: outline.radius_x.min(outline.radius_y),
                }
            }));
            if let Err(error) = shadow.set_presentation(
                self.state.shadow_enabled,
                w.is_window_active(),
                self.state.locked,
                palette.muted,
            ) {
                eprintln!("贴图阴影更新失败：{error}");
            }
        }
        let mut root = div()
            .id("pin-image-root")
            .size_full()
            .relative()
            .overflow_hidden()
            .track_focus(&self.focus)
            .cursor(if self.text_mode {
                let cursor = crate::pin_interaction::ocr_cursor(
                    self.gesture.as_ref(),
                    w.modifiers(),
                    !self.saving && !self.processing && !self.state.locked && !self.color_pick,
                );
                if cursor == gpui::CursorStyle::IBeam
                    && self.text_anchor.is_none()
                    && !self.text_selection.as_ref().is_some_and(|text| {
                        text.has_text_at(self.text_point(w.mouse_position(), w))
                    })
                {
                    if self.state.locked {
                        gpui::CursorStyle::Arrow
                    } else {
                        gpui::CursorStyle::OpenHand
                    }
                } else {
                    cursor
                }
            } else if self.color_pick && self.gesture.is_none() {
                gpui::CursorStyle::Crosshair
            } else if self.state.locked {
                gpui::CursorStyle::Arrow
            } else if self.gesture.is_some() {
                gpui::CursorStyle::ClosedHand
            } else if let Some(crate::pin_interaction::Kind::Resize { left, top }) =
                self.corner(w.mouse_position(), w)
            {
                if left == top {
                    gpui::CursorStyle::ResizeUpLeftDownRight
                } else {
                    gpui::CursorStyle::ResizeUpRightDownLeft
                }
            } else {
                gpui::CursorStyle::OpenHand
            })
            .child(capture_events)
            .when_some(presentation_plan, |root, plan| {
                root.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            crate::pin_presentation::paint_shadow(
                                plan,
                                point(px(0.), px(0.)),
                                window,
                            );
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .child(if let Some(canvas) = &self.canvas {
                canvas.clone().into_any_element()
            } else if self.image_size.0 > MAX_PIN_WINDOW_SIDE
                || self.image_size.1 > MAX_PIN_WINDOW_SIDE
            {
                // 处理正在后台更新，或画布准备失败；不能回退上传超限的整图纹理。
                div().size_full().into_any_element()
            } else if let Some(light) = self.vector_text {
                // 矢量文字贴图没有底图纹理，画布就绪前也只绘制主题纯色底。
                div()
                    .size_full()
                    .bg(rgb(crate::pin_text_source::colors(light).0))
                    .into_any_element()
            } else {
                let image = self.image.clone();
                let image_size = self.image_size;
                let viewport = self.content_viewport(w);
                let radius = self.presentation_style.map_or(0., |style| {
                    style.geometry(image_size.0, image_size.1).radius as f32 * viewport.zoom
                });
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        // 与标注/OCR/输入共用包含外观留白的同一物理视口。
                        let dpi = window.scale_factor();
                        let bounds = Bounds::new(
                            point(px(viewport.offset.x / dpi), px(viewport.offset.y / dpi)),
                            size(
                                px(image_size.0 as f32 * viewport.zoom / dpi),
                                px(image_size.1 as f32 * viewport.zoom / dpi),
                            ),
                        );
                        if let Err(error) = window.paint_image_exact(
                            bounds,
                            px(radius / dpi).into(),
                            image.clone(),
                            0,
                            false,
                        ) {
                            eprintln!("绘制贴图预览失败：{error}");
                        }
                    },
                )
                .size_full()
                .into_any_element()
            })
            .when_some(presentation_plan, |root, plan| {
                root.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            crate::pin_presentation::paint_border(
                                plan,
                                point(px(0.), px(0.)),
                                window,
                            );
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            })
            .on_hover(cx.listener(|v, hovered: &bool, _, cx| {
                if *hovered && !v.hovered {
                    v.reload_zoom_anchor();
                }
                v.hovered = *hovered;
                if !*hovered && v.color_pick {
                    v.clear_color_preview(cx);
                    v.status =
                        tr!("移回图像后取色", "Move back over the image to pick colors").into();
                }
                cx.notify();
            }))
            .on_key_down(cx.listener(Self::key))
            .capture_key_down(cx.listener(|v, e: &gpui::KeyDownEvent, w, cx| {
                v.color_pick_alt.reset();
                if v.context_menu_key(e, cx) {
                    cx.stop_propagation();
                    return;
                }
                // 标题输入框获得焦点时，Enter／Esc 与字符都交给它，查找等快捷键不抢先。
                if v.title_editor_focused(w, cx) {
                    return;
                }
                // 不激活菜单仍可保留键盘所有权，不能先关闭它再执行文字导航。
                if v.text_navigation_menu_owns(e.keystroke.key.as_str(), e.keystroke.modifiers, cx)
                {
                    cx.stop_propagation();
                    return;
                }
                if v.ocr_find_capture_key(e, w, cx) {
                    cx.stop_propagation();
                    return;
                }
                if v.pin_sensitive_escape(e, cx) {
                    cx.stop_propagation();
                    return;
                }
                if v.dismiss_context_menu(cx) && e.keystroke.key == "escape" {
                    cx.stop_propagation();
                }
            }))
            .capture_key_up(cx.listener(|v, _: &gpui::KeyUpEvent, _, _| {
                v.color_pick_alt.reset();
            }))
            .capture_any_mouse_down(cx.listener(|v, e, _, cx| {
                v.discard_pin_find_release();
                v.discard_pin_sensitive_release();
                v.color_pick_alt.reset();
                v.finish_text_selection_drag();
                v.post_drag_click.mouse_down(e);
                v.context_menu_press = false;
                v.dismiss_context_menu(cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|v, _, w, cx| {
                    v.cancel_pin_find_click();
                    if v.saving {
                        return;
                    }
                    v.cancel_drag(w);
                    if v.prepare_canvas_right_click(w, cx) {
                        // 收尾消费本次 Down，后续 RightUp 不打开菜单或重提草稿。
                        v.context_menu_press = false;
                        cx.stop_propagation();
                        cx.notify();
                        return;
                    }
                    v.context_menu_press = true;
                    unsafe {
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd(w));
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|v, _, w, cx| {
                    let pressed = std::mem::take(&mut v.context_menu_press);
                    if !pressed || v.saving {
                        return;
                    }
                    if let Err(e) = v.open_context_menu(w, cx) {
                        eprintln!("贴图菜单：{e}");
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|v, _, _, cx| {
                    if std::mem::take(&mut v.context_menu_press) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    v.cancel_pin_find_click();
                    if v.saving || v.state.locked || v.zoom_busy(cx) {
                        return;
                    }
                    if let Err(error) = v.user_zoom(
                        1.,
                        Some((
                            f32::from(e.position.x) * w.scale_factor(),
                            f32::from(e.position.y) * w.scale_factor(),
                        )),
                        w,
                        cx,
                    ) {
                        v.status = error.to_string();
                    } else {
                        v.state.thumbnail = false;
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    if v.saving {
                        return;
                    }
                    w.focus(&v.focus);
                    if e.modifiers.control {
                        if v.color_pick {
                            v.clear_color_preview(cx);
                            v.status =
                                tr!("拖动结束后继续取色", "Color picking resumes after dragging")
                                    .into();
                        }
                        v.begin_drag(e, w, crate::pin_interaction::Kind::Export);
                        return;
                    }
                    if v.color_pick {
                        // 取色开启时 Alt 仍可临时移动；不复制颜色、不结束取色会话。
                        if e.modifiers.alt && !e.modifiers.control {
                            v.clear_color_preview(cx);
                            v.status =
                                tr!("移动结束后继续取色", "Color picking resumes after moving")
                                    .into();
                            if v.begin_drag(e, w, crate::pin_interaction::Kind::Move) {
                                cx.notify();
                            }
                            return;
                        }
                        if v.refresh_color(e.position, w, cx) {
                            v.copy_color_notice(cx);
                        }
                        cx.notify();
                        return;
                    }
                    if v.text_mode {
                        v.clear_find_preview();
                        v.finish_text_selection_drag();
                        v.text_tools_ready = false;
                        v.text_show_selection = true;
                        if crate::pin_interaction::ocr_alt_move_requested(e)
                            || (e.click_count != 1 && e.modifiers.alt && !e.modifiers.control)
                        {
                            if v.begin_drag(e, w, crate::pin_interaction::Kind::Move) {
                                v.clear_text_selection();
                                cx.notify();
                            }
                            return;
                        }
                        let selection = v.text_selection.as_ref().and_then(|text| {
                            text.begin(v.text_point(e.position, w), e.click_count)
                        });
                        if let Some((anchor, range)) = selection {
                            v.text_keyboard_selection = v
                                .text_selection
                                .as_ref()
                                .and_then(|text| text.keyboard_selection_from_range(range.clone()));
                            v.selected = Some(range);
                            unsafe {
                                let _ = SetCapture(hwnd(w));
                            }
                            if unsafe { GetCapture() == hwnd(w) } {
                                v.text_capture = Some(hwnd(w));
                                v.text_anchor = Some(anchor);
                            }
                        } else if v.begin_drag(e, w, crate::pin_interaction::Kind::Move) {
                            v.clear_text_selection();
                        }
                        cx.notify();
                        return;
                    }
                    if e.click_count == 2 {
                        if !v.post_drag_click.suppress_double_click() {
                            v.perform(PinAction::Thumbnail, w, cx);
                        }
                    } else if !v.state.locked {
                        let kind = v
                            .corner(e.position, w)
                            .unwrap_or(crate::pin_interaction::Kind::Move);
                        v.begin_drag(e, w, kind);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, w, cx| {
                v.track_corner_overlays(e.position, w, cx);
                if v.saving {
                    return;
                }
                if v.text_drag_move(e, w, cx) || v.drag_move(e, w, cx) {
                    return;
                }
                if v.text_mode {
                    cx.notify();
                }
                if v.color_pick {
                    v.refresh_color(e.position, w, cx);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseUpEvent, w, cx| {
                    if !v.text_drag_up(e, w, cx) {
                        let color_move = v.color_pick && v.gesture.is_some();
                        let accepted_release = v.finish_drag(e, w, cx);
                        if color_move {
                            if accepted_release && v.pending_placement.is_none() {
                                v.refresh_color(e.position, w, cx);
                            } else {
                                v.clear_color_preview(cx);
                            }
                            cx.notify();
                        }
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|v, e: &gpui::ScrollWheelEvent, w, cx| {
                v.cancel_pin_find_click();
                if v.redirect_context_menu_wheel(e, w, cx) {
                    return;
                }
                if v.saving {
                    return;
                }
                if v.state.locked {
                    // 锁定时滚轮不改窗口，但要让人知道为什么没反应。
                    v.flash_operation_hint(
                        tr!("已锁定 · L 解锁", "Locked · L to unlock").into(),
                        cx,
                    );
                    return;
                }
                // 未激活贴图也可能接收滚轮；另一个工作进程修改的全局偏好须在这次输入生效。
                // 只在输入边界读小型 INI，不在 render 或 GPU 预览循环中读取配置。
                if !e.modifiers.control {
                    v.reload_zoom_anchor();
                }
                let parameter = if e.modifiers.control {
                    crate::interaction::Parameter::Opacity
                } else {
                    crate::interaction::Parameter::Width
                };
                let (steps, _) = v.wheel.push(
                    parameter,
                    crate::capture_interaction::notches(e, v.wheel_settings),
                    Instant::now(),
                );
                if steps == 0 {
                    return;
                }
                // Shift 精细调整：缩放与不透明度都改为 1 个百分点一档。
                let fine = e.modifiers.shift;
                if e.modifiers.control {
                    let opacity = if fine {
                        crate::pin_zoom::next_opacity_fine(v.state.opacity, steps)
                    } else {
                        crate::pin_zoom::next_opacity(v.state.opacity, steps)
                    };
                    if opacity == v.state.opacity {
                        let percent = (f64::from(opacity) * 100. / 255.).round();
                        v.flash_operation_hint(
                            trf!("不透明度 {}% · 已到极限", "Opacity {}% · At limit", percent),
                            cx,
                        );
                        return;
                    }
                    v.perform(PinAction::Opacity(opacity), w, cx);
                } else if !v.state.locked && !v.zoom_busy(cx) {
                    let zoom = if fine {
                        crate::pin_zoom::next_zoom_fine(v.state.zoom, steps)
                    } else {
                        crate::pin_zoom::next_zoom(v.state.zoom, steps)
                    };
                    if zoom == v.state.zoom {
                        let current = pin_model::zoom_hint(zoom, v.physical_geometry.size);
                        v.flash_operation_hint(trf!("{} · 已到极限", "{} · At limit", current), cx);
                        return;
                    }
                    if let Err(error) = v.zoom_wheel(
                        zoom,
                        (
                            f32::from(e.position.x) * w.scale_factor(),
                            f32::from(e.position.y) * w.scale_factor(),
                        ),
                        w,
                        cx,
                    ) {
                        v.status = error.to_string();
                    } else {
                        v.state.thumbnail = false;
                    }
                    cx.notify();
                }
            }));
        if self.text_mode && self.text_show_selection {
            if let (Some(text), Some(range)) = (&self.text_selection, &self.selected) {
                let viewport = self.content_viewport(w);
                let dpi = w.scale_factor();
                let scale = viewport.zoom / dpi;
                let left = viewport.offset.x / dpi;
                let top = viewport.offset.y / dpi;
                let highlights = text.highlights(range.clone());
                let local_paint = self.pin_local_paint_probe(viewport, dpi, cx);
                root = root.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, cx| {
                            let screen = |p: crate::annotation::Point| {
                                point(px(left + p.x * scale), px(top + p.y * scale))
                            };
                            for quad in &highlights {
                                let points = quad.map(screen);
                                let mut path = gpui::Path::new(points[0]);
                                let uv = point(0., 1.);
                                path.push_triangle((points[0], points[1], points[2]), (uv, uv, uv));
                                path.push_triangle((points[0], points[2], points[3]), (uv, uv, uv));
                                window.paint_path(path, rgba(0x609dff55));
                            }
                            if !highlights.is_empty() {
                                if let Some(paint) = &local_paint {
                                    paint.record(window, cx);
                                }
                            }
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                );
            }
        }
        if let Some(segment) = self.text_caret_segment(w, cx) {
            let viewport = self.content_viewport(w);
            let dpi = w.scale_factor();
            root = root.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        if !window.is_window_active() {
                            return;
                        }
                        let points = segment.map(|p| {
                            let p = viewport.window_point(p);
                            point(px(p.x / dpi), px(p.y / dpi))
                        });
                        // 恒定 DIP 描边仅用于辅助预览；端点仍严格来自文字截面。
                        for (width, color) in [(4., palette.background), (2., palette.accent)] {
                            let mut path = gpui::PathBuilder::stroke(px(width));
                            path.move_to(points[0]);
                            path.line_to(points[1]);
                            if let Ok(path) = path.build() {
                                window.paint_path(path, rgb(color));
                            }
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        }
        if corner_overlays.actions {
            use crate::toolbar_icons::{self, Icon};
            let mut bar = div()
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .flex()
                .items_center()
                .gap_1()
                .p_1()
                .rounded_md()
                .bg(rgba((palette.background << 8) | 0xed))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
            for (id, icon, action) in [
                ("pin-copy", Icon::Copy, PinAction::Copy),
                ("pin-edit", Icon::Edit, PinAction::Chrome),
                ("pin-save", Icon::Save, PinAction::Save),
                ("pin-close", Icon::Close, PinAction::Close),
            ] {
                bar = bar.child(
                    div()
                        .id(id)
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(move |s| s.bg(rgb(palette.hover)))
                        .child(toolbar_icons::action(icon, palette.foreground))
                        .on_click(cx.listener(move |v, _, w, cx| v.perform(action, w, cx))),
                );
            }
            bar = bar.child(
                div()
                    .id("pin-more")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(toolbar_icons::action(Icon::More, palette.foreground))
                    .on_click(cx.listener(|v, _, w, cx| {
                        if let Err(error) = v.open_context_menu(w, cx) {
                            v.status = error.to_string();
                            cx.notify();
                        }
                    })),
            );
            root = root.child(bar);
        }
        if corner_overlays.info {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(4.))
                    .left(px(4.))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(rgba((palette.background << 8) | 0xda))
                    .text_color(rgb(palette.foreground))
                    .text_size(px(11.))
                    .child(format!(
                        "{}%{}{}",
                        (self.state.zoom * 100.).round(),
                        if self.state.locked {
                            tr!(" · 已锁定", " · Locked")
                        } else {
                            ""
                        },
                        if self.processing {
                            tr!(" · 处理中", " · Processing")
                        } else {
                            ""
                        }
                    )),
            );
        }
        if status_visible {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(30.))
                    .left(px(4.))
                    .right(px(4.))
                    .max_h(px(80.))
                    .overflow_hidden()
                    .line_clamp(3)
                    .p_2()
                    .rounded_md()
                    .bg(rgba((palette.background << 8) | 0xed))
                    .text_color(rgb(palette.foreground))
                    .text_size(px(12.))
                    .child(self.status.clone()),
            );
        }
        if let Some((text, opacity)) = notice_bubble {
            let viewport = w.viewport_size();
            let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
            let pointer = w.mouse_position();
            // 气泡不接收鼠标；指针、查找栏或敏感信息面板占着左上角时改到左下角。
            let top_left_busy = self.ocr_find.is_some()
                || self.pin_sensitive_preview_open()
                || (self.hovered && f32::from(pointer.x) < 176. && f32::from(pointer.y) < 44.);
            if width >= 48. && height >= 24. {
                let bubble = div()
                    .absolute()
                    .left(px(6.))
                    .max_w(px(width - 12.))
                    .overflow_hidden()
                    .line_clamp(3)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgba(palette.hint_border))
                    .bg(rgba(palette.hint_bg))
                    .text_color(rgba(palette.hint_fg))
                    .text_size(px(12.))
                    .opacity(opacity)
                    .child(text);
                root = root.child(if top_left_busy && height >= 88. {
                    bubble.bottom(px(6.))
                } else {
                    bubble.top(px(6.))
                });
            }
        }
        if let Some(tools) = self.render_text_tools(w, cx) {
            root = root.child(tools);
        }
        if let Some(find) = self.render_ocr_find(w, cx) {
            root = root.child(find);
        }
        if !self.pin_sensitive_preview_open() {
            if let Some(local) = self.render_pin_local_ocr(w, cx) {
                root = root.child(local);
            }
        }
        if let Some(sensitive) = self.render_pin_sensitive(w, cx) {
            root = root.child(sensitive);
        }
        if let Some(progress) = self.render_pin_ocr_progress(w, cx) {
            root = root.child(progress);
        }
        if let Some(editor) = self.render_title_editor(w, cx) {
            root = root.child(editor);
        }
        root
    }
}

#[cfg(test)]
mod text_navigation_key_tests {
    use super::{ocr_clipboard::TextSource, PinView};
    use crate::pin_text_selection::{NavigationKey, SelectionText};

    #[test]
    fn pin_text_navigation_shortcuts_preserve_move_keys_and_reject_other_modifiers() {
        for (key, shift, expected, ctrl_expected) in [
            ("left", false, None, Some(NavigationKey::PreviousWord)),
            ("right", false, None, Some(NavigationKey::NextWord)),
            (
                "left",
                true,
                Some(NavigationKey::ExtendPrevious),
                Some(NavigationKey::ExtendPreviousWord),
            ),
            (
                "right",
                true,
                Some(NavigationKey::ExtendNext),
                Some(NavigationKey::ExtendNextWord),
            ),
            (
                "home",
                false,
                Some(NavigationKey::Home),
                Some(NavigationKey::DocumentHome),
            ),
            (
                "end",
                false,
                Some(NavigationKey::End),
                Some(NavigationKey::DocumentEnd),
            ),
            (
                "home",
                true,
                Some(NavigationKey::ExtendHome),
                Some(NavigationKey::ExtendDocumentHome),
            ),
            (
                "end",
                true,
                Some(NavigationKey::ExtendEnd),
                Some(NavigationKey::ExtendDocumentEnd),
            ),
            ("up", true, Some(NavigationKey::ExtendPreviousLine), None),
            ("down", true, Some(NavigationKey::ExtendNextLine), None),
            ("up", false, None, None),
            ("down", false, None, None),
            ("c", true, None, None),
        ] {
            for other in 0..16 {
                let modifiers = gpui::Modifiers {
                    shift,
                    control: other & 1 != 0,
                    alt: other & 2 != 0,
                    platform: other & 4 != 0,
                    function: other & 8 != 0,
                };
                assert_eq!(
                    PinView::text_navigation_key(key, modifiers),
                    match other {
                        0 => expected,
                        1 => ctrl_expected,
                        _ => None,
                    },
                    "key={key}, modifiers={modifiers:?}"
                );
            }
        }
    }

    #[test]
    fn pin_ocr_navigation_ownership_survives_missing_geometry_and_no_cursor() {
        use crate::{
            geometry::Rect,
            recognition::{HorizontalDirection, RecognizedText, RecognizedWord},
        };
        let recognized = RecognizedText {
            text: "first\nlast".into(),
            regions: vec![],
            words: vec![RecognizedWord {
                text: "first".into(),
                rect: Rect {
                    x: 10,
                    y: 10,
                    width: 100,
                    height: 20,
                },
                line: 0,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: None,
            }],
            lines: vec![],
        };
        let text = SelectionText::from_recognized(&recognized);
        // 与实际 Ctrl+A 相同：完整正文保留，但没有末行证据，不能构造活动端。
        assert!(text
            .keyboard_selection_from_range(0..text.text.len())
            .is_none());
        let modifiers = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let owned = PinView::text_navigation_owned_by_session(
            "down",
            modifiers,
            TextSource::Ocr,
            true,
            false,
        );
        assert_eq!(owned, Some(NavigationKey::ExtendNextLine));
        // 缺几何的末行不进入 SelectionText；真实导航在最后可信行返回原停点。
        let first = text.keyboard_selection_from_range(0..0).unwrap();
        assert_eq!(text.navigate_keyboard(first, owned.unwrap()), Some(first));
        assert_eq!(text.text, recognized.text);
        assert_eq!(recognized.text, "first\nlast");
        // 证明原有窗口路线确实接收此键；宿主必须先消费上面的 OCR 归属。
        assert_eq!(
            crate::pin_interaction::keyboard_move_delta("down", modifiers, false),
            Some((0, 10))
        );
    }

    #[test]
    fn pin_ocr_navigation_ownership_keeps_input_priority_and_mode_outside_movement() {
        let modifiers = gpui::Modifiers {
            shift: true,
            ..Default::default()
        };
        for (source, text_mode, native_input_owns) in [
            (TextSource::None, true, false),
            (TextSource::Native, true, false),
            (TextSource::Qr, true, false),
            (TextSource::Ocr, false, false),
            (TextSource::Ocr, true, true),
        ] {
            assert_eq!(
                PinView::text_navigation_owned_by_session(
                    "up",
                    modifiers,
                    source,
                    text_mode,
                    native_input_owns,
                ),
                None,
                "source={source:?}, mode={text_mode}, input={native_input_owns}"
            );
        }
        // 非 OCR 模式继续实际旧移动函数；裸箭头也不扩大为选字所有权。
        assert_eq!(
            crate::pin_interaction::keyboard_move_delta("up", modifiers, false),
            Some((0, -10))
        );
        assert_eq!(
            PinView::text_navigation_owned_by_session(
                "up",
                gpui::Modifiers::default(),
                TextSource::Ocr,
                true,
                false,
            ),
            None
        );
        assert_eq!(
            PinView::text_navigation_owned_by_session(
                "up",
                gpui::Modifiers {
                    alt: true,
                    ..modifiers
                },
                TextSource::Ocr,
                true,
                false,
            ),
            None
        );
    }
}

#[cfg(test)]
mod physical_image_projection_tests {
    use super::*;
    use crate::annotation::Point;

    #[test]
    fn natural_capture_extreme_aspect_maximum_shadow_keeps_positive_anchored_geometry() {
        use crate::{
            geometry::Rect,
            pin_presentation::{PhysicalPoint, PresentationPreviewPlan},
            selection_finish::SelectionFinishStyle,
        };
        let style = SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 48.,
            ..Default::default()
        };
        let work = Placement {
            x: -1920,
            y: -1080,
            width: 3840,
            height: 2160,
        };
        for content in [(1, 1), (16_384, 1), (1, 16_384), (1, 900), (900, 1)] {
            let outer = crate::pin_presentation_host::window_size(content, Some(style)).unwrap();
            let (sizing, _, host) = initial_pin_geometry(outer, 1., work, true, None);
            assert_eq!(sizing, PinWindowSizing::ExactCapture);
            // 源单边合法时，最大padding的自然首显不会走旧小客户区恢复 fallback。
            assert!(host.0 > 48 && host.1 > 48);
            let viewport = crate::pin_presentation_host::content_viewport_with_sizing(
                content,
                host,
                Some(style),
                sizing,
            );
            assert_eq!(viewport.offset, Point { x: 48., y: 48. });
            assert!(viewport.zoom.is_finite() && viewport.zoom > 0. && viewport.zoom <= 1.);
            let plan = PresentationPreviewPlan::new(
                Rect::full(content.0, content.1),
                style,
                PhysicalPoint {
                    x: f64::from(viewport.offset.x),
                    y: f64::from(viewport.offset.y),
                },
                f64::from(viewport.zoom),
            )
            .unwrap();
            let outer = plan.outer_bounds();
            assert!(outer.left >= 0. && outer.top >= 0.);
            assert!(outer.left + outer.width <= f64::from(host.0));
            assert!(outer.top + outer.height <= f64::from(host.1));
        }
    }

    #[test]
    fn explicit_closed_capture_mode_restores_scaled_anchor_and_keeps_legacy_small_host() {
        use crate::selection_finish::SelectionFinishStyle;
        let style = Some(SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 8.,
            ..Default::default()
        });
        let saved = crate::pin_store::PresentationWindow {
            placement: Placement {
                x: -8,
                y: 0,
                width: 16_384,
                height: 899,
            },
            sizing: PinWindowSizing::ExactCapture,
        };
        let (sizing, _, placement) =
            closed_pin_geometry(saved, window_zoom((16_400, 900), 1.)).unwrap();
        assert_eq!(sizing, PinWindowSizing::ExactCapture);
        assert_eq!(placement, saved.placement);
        let viewport = crate::pin_presentation_host::content_viewport_with_sizing(
            (16_384, 884),
            (placement.width, placement.height),
            style,
            sizing,
        );
        assert_eq!(
            (
                f64::from(placement.x) + f64::from(viewport.offset.x),
                f64::from(placement.y) + f64::from(viewport.offset.y)
            ),
            (0., 8.)
        );
        let legacy_style = SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 48.,
            ..Default::default()
        };
        let legacy = crate::pin_store::PresentationWindow {
            placement: Placement {
                x: -25,
                y: 31,
                width: 27,
                height: 27,
            },
            sizing: PinWindowSizing::ExactCapture,
        };
        let (sizing, zoom, placement) = closed_pin_geometry(legacy, 0.25).unwrap();
        assert_eq!(zoom, 0.25);
        assert_eq!(placement, legacy.placement);
        let viewport = crate::pin_presentation_host::content_viewport_with_sizing(
            (10, 10),
            (placement.width, placement.height),
            Some(legacy_style),
            sizing,
        );
        assert_eq!(
            viewport,
            crate::pin_presentation_host::content_viewport(
                (10, 10),
                (placement.width, placement.height),
                Some(legacy_style)
            )
        );
        assert!(viewport.zoom.is_finite() && viewport.zoom > 0.);
        crate::pin_presentation::validate_preview_geometry_with_sizing(
            (10, 10),
            legacy_style,
            placement,
            Placement {
                x: -1920,
                y: -1080,
                width: 3840,
                height: 2160,
            },
            sizing,
        )
        .unwrap();
    }

    #[test]
    fn capture_handoff_at_desktop_limit_keeps_frozen_content_origin_and_shared_preview_plan() {
        use crate::{
            geometry::{Rect, ScreenRect},
            pin_presentation::{PhysicalPoint, PinPresentation, PresentationPreviewPlan},
            selection_finish::SelectionFinishStyle,
        };
        let style = SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 8.,
            ..Default::default()
        };
        let crop = Rect {
            x: 0,
            y: 8,
            width: 16_384,
            height: 884,
        };
        for desktop_origin in [(0, 0), (-16_384, -900), (100, 250)] {
            let bounds = ScreenRect {
                left: desktop_origin.0,
                top: desktop_origin.1,
                width: 16_384,
                height: 900,
            };
            let presentation = PinPresentation::new(bounds, crop, style).unwrap();
            let outer = presentation.outer_screen();
            assert_eq!((outer.width, outer.height), (16_400, 900));
            let (sizing, zoom, dimensions) = initial_pin_geometry(
                (outer.width, outer.height),
                1.,
                Placement {
                    x: bounds.left,
                    y: bounds.top,
                    width: bounds.width,
                    height: bounds.height,
                },
                true,
                None,
            );
            assert_eq!(sizing, PinWindowSizing::ExactCapture);
            assert!(zoom < 1.);
            assert_eq!(dimensions, (16_384, 899));
            let placement = pin_model::initial_placement(
                Placement {
                    x: outer.left,
                    y: outer.top,
                    width: dimensions.0,
                    height: dimensions.1,
                },
                Placement {
                    x: bounds.left,
                    y: bounds.top,
                    width: bounds.width,
                    height: bounds.height,
                },
                true,
            );
            for dpi in [1., 1.25, 1.5, 1.75, 2.] {
                let host = (
                    (dimensions.0 as f32 / dpi * dpi).round() as u32,
                    (dimensions.1 as f32 / dpi * dpi).round() as u32,
                );
                let viewport = crate::pin_presentation_host::content_viewport_with_sizing(
                    (crop.width, crop.height),
                    host,
                    Some(style),
                    sizing,
                );
                assert_eq!(
                    (
                        f64::from(placement.x) + f64::from(viewport.offset.x),
                        f64::from(placement.y) + f64::from(viewport.offset.y)
                    ),
                    (f64::from(bounds.left), f64::from(bounds.top) + 8.)
                );
                let plan = PresentationPreviewPlan::new(
                    Rect::full(crop.width, crop.height),
                    style,
                    PhysicalPoint {
                        x: f64::from(viewport.offset.x),
                        y: f64::from(viewport.offset.y),
                    },
                    f64::from(viewport.zoom),
                )
                .unwrap();
                assert_eq!(
                    (plan.content_bounds().left, plan.content_bounds().top),
                    (8., 8.)
                );
                crate::pin_presentation::validate_preview_geometry_with_sizing(
                    (crop.width, crop.height),
                    style,
                    placement,
                    Placement {
                        x: bounds.left,
                        y: bounds.top,
                        width: bounds.width,
                        height: bounds.height,
                    },
                    sizing,
                )
                .unwrap();
                assert_eq!(
                    crate::pin_presentation::fit_preview_placement_with_sizing(
                        (crop.width, crop.height),
                        style,
                        placement,
                        Placement {
                            x: bounds.left,
                            y: bounds.top,
                            width: bounds.width,
                            height: bounds.height
                        },
                        sizing
                    )
                    .unwrap(),
                    placement
                );
            }
        }
    }

    #[test]
    fn capture_initial_geometry_keeps_tiny_pixels_origin_and_narrow_workarea() {
        let work = Placement {
            x: -1920,
            y: -700,
            width: 1000,
            height: 800,
        };
        for image in [
            (22, 100),
            (23, 100),
            (100, 22),
            (100, 23),
            (21, 101),
            (1, 1),
            (7, 11),
            (1920, 1080),
        ] {
            let (sizing, zoom, dimensions) = initial_pin_geometry(image, 0.25, work, true, None);
            assert_eq!(sizing, PinWindowSizing::ExactCapture);
            assert_eq!(zoom, 1.);
            assert_eq!(dimensions, image);
            for origin in [(-1921, -701), (-1, -1), (2561, 121)] {
                let placement = pin_model::initial_placement(
                    Placement {
                        x: origin.0,
                        y: origin.1,
                        width: dimensions.0,
                        height: dimensions.1,
                    },
                    work,
                    true,
                );
                assert_eq!((placement.x, placement.y), origin);
                for dpi in [1., 1.25, 1.5, 1.75, 2.] {
                    let projection = image_projection(
                        image,
                        (dimensions.0 as f32 / dpi, dimensions.1 as f32 / dpi),
                        dpi,
                    );
                    assert_eq!(projection.zoom, 1.);
                    assert_eq!(projection.offset, Point { x: 0., y: 0. });
                    for pixel in [(0, 0), (image.0 - 1, image.1 - 1)] {
                        let center = Point {
                            x: pixel.0 as f32 + 0.5,
                            y: pixel.1 as f32 + 0.5,
                        };
                        let drawn = projection.window_point(center);
                        assert_eq!(drawn, center);
                        assert_eq!(
                            (
                                placement.x + drawn.x.floor() as i32,
                                placement.y + drawn.y.floor() as i32
                            ),
                            (origin.0 + pixel.0 as i32, origin.1 + pixel.1 as i32)
                        );
                    }
                }
            }
        }
        // 超出 swapchain 单边上限只按 GPU 安全上限缩小，不按工作区再缩放。
        for image in [(65_535, 1), (1, 65_535)] {
            let (sizing, zoom, dimensions) = initial_pin_geometry(image, 1., work, true, None);
            assert_eq!(sizing, PinWindowSizing::ExactCapture);
            assert!(zoom < 1.);
            assert!(dimensions.0 >= 1 && dimensions.0 <= MAX_PIN_WINDOW_SIDE);
            assert!(dimensions.1 >= 1 && dimensions.1 <= MAX_PIN_WINDOW_SIDE);
            assert_eq!(dimensions.0.max(dimensions.1), MAX_PIN_WINDOW_SIDE);
        }
        let (_, zoom, dimensions) = initial_pin_geometry((7, 11), 1., work, false, None);
        assert_eq!(zoom, 1.);
        assert_eq!(dimensions, (24, 24));
        let restored = Placement {
            width: 22,
            height: 100,
            ..work
        };
        assert_eq!(
            initial_pin_geometry((22, 100), 1., work, false, Some(restored)),
            (PinWindowSizing::ExactCapture, 1., (22, 100))
        );
        let (sizing, _, dimensions) = initial_pin_geometry((65_535, 1), 1., work, false, None);
        assert_eq!(sizing, PinWindowSizing::General);
        assert!(dimensions.0 <= work.width);
        assert_eq!(dimensions.1, 24);
    }

    #[test]
    fn contain_snap_turns_fractional_dpi_error_into_a_one_pixel_shift() {
        for (image, dpi) in [((25, 31), 1.25), ((27, 25), 1.5), ((27, 25), 1.75)] {
            let host = (image.0 as f32 / dpi, image.1 as f32 / dpi);
            let old = gpui::ObjectFit::Contain.get_bounds(
                Bounds::new(point(px(0.), px(0.)), size(px(host.0), px(host.1))),
                size(
                    gpui::DevicePixels(image.0 as i32),
                    gpui::DevicePixels(image.1 as i32),
                ),
            );
            let old_y = f32::from(old.origin.y) * dpi;
            assert!(old_y < 0. && old_y > -0.001, "{image:?}@{dpi}: {old_y}");
            assert_eq!(old_y.floor(), -1.);
            let exact = image_projection(image, host, dpi);
            assert_eq!(exact.zoom, 1.);
            assert_eq!(exact.offset, Point { x: 0., y: 0. });
        }
    }

    #[test]
    fn one_to_one_pin_preserves_pixel_centers_on_negative_and_mixed_dpi_screens() {
        for image in [(25, 31), (27, 25), (161, 99), (601, 359)] {
            for dpi in [1., 1.25, 1.5, 1.75, 2.] {
                let host = (image.0 as f32 / dpi, image.1 as f32 / dpi);
                let viewport = image_projection(image, host, dpi);
                assert_eq!(viewport.zoom, 1.);
                assert_eq!(viewport.offset, Point { x: 0., y: 0. });
                for origin in [(-1921, -777), (-1, -1), (1, 1), (2561, 121)] {
                    for pixel in [
                        (0, 0),
                        (1, 1),
                        (image.0 / 2, image.1 / 2),
                        (image.0 - 1, image.1 - 1),
                    ] {
                        let center = Point {
                            x: pixel.0 as f32 + 0.5,
                            y: pixel.1 as f32 + 0.5,
                        };
                        let physical = viewport.window_point(center);
                        assert_eq!(physical, center);
                        let logical = Point {
                            x: physical.x / dpi,
                            y: physical.y / dpi,
                        };
                        let pointer = Point {
                            x: logical.x * dpi,
                            y: logical.y * dpi,
                        };
                        let sampled = viewport.image_point(pointer);
                        assert_eq!((sampled.x as u32, sampled.y as u32), pixel);
                        assert_eq!(
                            origin.0 + physical.x.floor() as i32,
                            origin.0 + pixel.0 as i32
                        );
                        assert_eq!(
                            origin.1 + physical.y.floor() as i32,
                            origin.1 + pixel.1 as i32
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn physical_image_projection_keeps_letterbox_fit_and_the_24_pixel_minimum() {
        for (image, host) in [
            ((7, 11), (24, 24)),
            ((100, 100), (250, 125)),
            ((601, 359), (661, 395)),
            ((32000, 4), (3200, 24)),
        ] {
            let expected = crate::pin_zoom::projection(image, host);
            for dpi in [1., 1.25, 1.5, 1.75, 2.] {
                let viewport =
                    image_projection(image, (host.0 as f32 / dpi, host.1 as f32 / dpi), dpi);
                assert_eq!(viewport, expected);
                let center = viewport.window_point(Point {
                    x: image.0 as f32 / 2.,
                    y: image.1 as f32 / 2.,
                });
                assert!((center.x - host.0 as f32 / 2.).abs() < 0.001);
                assert!((center.y - host.1 as f32 / 2.).abs() < 0.001);
            }
        }
        assert_eq!(pin_model::size((7, 11), 1.), (24, 24));
    }
}

#[cfg(test)]
mod long_image_tests {
    use super::*;

    #[test]
    fn long_pin_fit_below_ten_percent_and_actual_zoom_stay_gpu_bounded() {
        let work = Placement {
            x: -1920,
            y: -120,
            width: 1920,
            height: 1040,
        };
        for image in [(100, 65535), (65535, 100)] {
            let fit = pin_model::fit(image, work);
            assert!(fit < 0.1);
            let dimensions = pin_model::size(image, window_zoom(image, fit));
            assert!(dimensions.0 <= work.width && dimensions.1 <= work.height);
            for zoom in [1., 8., f32::NAN] {
                let dimensions = pin_model::size(image, window_zoom(image, zoom));
                assert!(dimensions.0 <= MAX_PIN_WINDOW_SIDE && dimensions.1 <= MAX_PIN_WINDOW_SIDE);
            }
        }
        assert_eq!(window_zoom((800, 600), 1.5), 1.5);
    }
    #[test]
    fn restored_pin_keeps_independently_rounded_physical_size() {
        let work = Placement {
            x: -1920,
            y: -720,
            width: 1920,
            height: 1080,
        };
        for (image, dimensions) in [((601, 359), (240, 144)), ((359, 601), (144, 240))] {
            let zoom = crate::pin_zoom::projection(image, dimensions).zoom;
            // 单个 contain 比例不足以往返两轴独立 round 的窗口尺寸。
            assert_ne!(PinWindowSizing::General.size(image, zoom), dimensions);
            let placement = Placement {
                x: -1901,
                y: -701,
                width: dimensions.0,
                height: dimensions.1,
            };
            let (sizing, restored_zoom, restored_size) =
                initial_pin_geometry(image, zoom, work, false, Some(placement));
            assert_eq!(sizing, PinWindowSizing::General);
            assert_eq!(restored_zoom, zoom);
            assert_eq!(restored_size, dimensions);
            for dpi in [1., 1.25, 1.5, 1.75, 2., 3.] {
                let viewport = image_projection(
                    image,
                    (restored_size.0 as f32 / dpi, restored_size.1 as f32 / dpi),
                    dpi,
                );
                assert_eq!(viewport.zoom, zoom);
                assert_eq!(
                    viewport.offset,
                    crate::pin_zoom::projection(image, dimensions).offset
                );
            }
        }
        for dimensions in [(0, 144), (240, 0), (MAX_PIN_WINDOW_SIDE + 1, 144)] {
            let (_, zoom, size) = initial_pin_geometry(
                (601, 359),
                1.,
                work,
                false,
                Some(Placement {
                    width: dimensions.0,
                    height: dimensions.1,
                    ..work
                }),
            );
            assert_eq!(zoom, 1.);
            assert_eq!(size, (601, 359));
        }
        for (image, zoom, dimensions) in [
            ((1, 1), 1., (24, 24)),
            ((1, 2), 1., (24, 24)),
            ((65_535, 1), 0.0001, (1, 1)),
            ((30, 60), 6., (180, 360)),
        ] {
            let (_, restored_zoom, restored_size) = initial_pin_geometry(
                image,
                zoom,
                work,
                false,
                Some(Placement {
                    width: dimensions.0,
                    height: dimensions.1,
                    ..work
                }),
            );
            assert_eq!(restored_zoom, zoom);
            assert_eq!(restored_size, dimensions);
        }
    }
    #[test]
    fn closed_capture_keeps_saved_client_and_content_origin_without_fit_or_minimum() {
        for (content, style) in [
            (
                (10, 10),
                crate::selection_finish::SelectionFinishStyle::default(),
            ),
            (
                (1, 1),
                crate::selection_finish::SelectionFinishStyle {
                    border_enabled: true,
                    border_thickness: 1.,
                    ..Default::default()
                },
            ),
        ] {
            let outer = crate::pin_presentation_host::window_size(content, Some(style)).unwrap();
            let saved = Placement {
                x: -1901,
                y: -679,
                width: outer.0,
                height: outer.1,
            };
            let (sizing, zoom, placement) = closed_pin_geometry(
                crate::pin_store::PresentationWindow {
                    placement: saved,
                    sizing: PinWindowSizing::ExactCapture,
                },
                1.,
            )
            .unwrap();
            assert_eq!(sizing, PinWindowSizing::ExactCapture);
            assert_eq!(zoom, 1.);
            assert_eq!(placement, saved);
            let viewport = crate::pin_presentation_host::content_viewport(
                content,
                (placement.width, placement.height),
                Some(style),
            );
            assert_eq!(viewport.zoom, 1.);
            let padding = style.padding();
            assert_eq!(viewport.offset.x, padding.left as f32);
            assert_eq!(viewport.offset.y, padding.top as f32);
            assert_eq!(
                (
                    placement.x + viewport.offset.x as i32,
                    placement.y + viewport.offset.y as i32
                ),
                (-1901 + padding.left as i32, -679 + padding.top as i32),
            );
        }
    }
}
