//! 当前像素快照及显式文件列表到 PDF；文件只在保存确认后的有界后台任务中读取。
use crate::{
    capture::Image,
    image_pdf::{PdfLimits, PdfOptions, PdfPageSize, SavedPdf},
    image_tool_view::{
        host::{self, HostedTool, ToolEvent, ToolHost},
        strip::{self, ThumbHost, Thumbs},
    },
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    settings_widgets::ButtonKind,
    tool_recent_captures::{RecentCaptures, RecentHost},
    tool_window_chrome::{self as chrome, Tone},
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, App, AppContext, Context, Entity, EventEmitter, FocusHandle,
    KeyDownEvent, PathPromptOptions, Render, Window, WindowHandle,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Sources,
    Save,
}

#[derive(Clone, Copy)]
enum Control {
    Add,
    Clear,
    Sort,
    Move(usize, usize),
    Remove(usize),
    Save,
    Cancel,
    Copy,
    Close,
}

struct ActiveSave {
    request_id: u64,
    cancel: Arc<AtomicBool>,
    /// 0..=来源数量为已准备页数，来源数量+1表示正在写 PDF。
    progress: Arc<AtomicUsize>,
}

#[derive(Clone)]
enum PdfInput {
    File(PathBuf),
    Current {
        image: Arc<ProcessedImage>,
        protected: Arc<Vec<PathBuf>>,
    },
}

impl PdfInput {
    fn path_hint(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Current { protected, .. } => protected.first().map(PathBuf::as_path),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Current { image, .. } => trf!(
                "当前结果快照 · {}×{} 像素",
                "Current result snapshot · {}×{} pixels",
                image.image().width,
                image.image().height
            ),
        }
    }
}

pub(crate) struct ImagePdfView {
    pages: Vec<PdfInput>,
    options: PdfOptions,
    request_id: u64,
    prompt: Option<PromptKind>,
    active: Option<ActiveSave>,
    progress: usize,
    saved: Option<SavedPdf>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    recent: RecentCaptures,
    host: ToolHost,
    thumbs: Thumbs,
}

impl ThumbHost for ImagePdfView {
    fn thumbs(&mut self) -> &mut Thumbs {
        &mut self.thumbs
    }
}

impl RecentHost for ImagePdfView {
    fn recent(&mut self) -> &mut RecentCaptures {
        &mut self.recent
    }
}

/// 本入口不读取源文件；空列表可在窗口内通过原生多选对话框补充。
pub(crate) fn open(paths: Vec<PathBuf>, cx: &mut App) -> Result<WindowHandle<ImagePdfView>> {
    if paths.len() > PdfLimits::default().max_pages {
        return Err(tr!("PDF 图片数量超过上限", "Too many PDF images").into());
    }
    open_pages(paths.into_iter().map(PdfInput::File).collect(), cx)
}

/// 快照共享不可变像素，不读回原文件，后续可在同一列表添加其它文件页。
pub(crate) fn open_processed(
    image: Arc<ProcessedImage>,
    protected: Arc<Vec<PathBuf>>,
    cx: &mut App,
) -> Result<WindowHandle<ImagePdfView>> {
    validate_input(image.image(), ToolLimits::default())?;
    if protected.len() > PdfLimits::default().max_pages {
        return Err(tr!("原片保护路径过多", "Too many protected source paths").into());
    }
    open_pages(vec![PdfInput::Current { image, protected }], cx)
}

fn open_pages(pages: Vec<PdfInput>, cx: &mut App) -> Result<WindowHandle<ImagePdfView>> {
    host::open_window(
        "pdf",
        brand_title!("图片转 PDF", "Images to PDF").into(),
        (1040., 760.),
        (640., 480.),
        cx,
        move |host, window, cx| ImagePdfView::new(pages, host, window, cx),
    )
}

/// 在主界面弹层里打开，每张一页，按图库选择顺序。
pub(crate) fn popup(
    paths: Vec<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) -> Result<Entity<ImagePdfView>> {
    if paths.len() > PdfLimits::default().max_pages {
        return Err(tr!("PDF 图片数量超过上限", "Too many PDF images").into());
    }
    let pages = paths.into_iter().map(PdfInput::File).collect();
    Ok(cx.new(|cx| ImagePdfView::new(pages, ToolHost::Popup, window, cx)))
}

impl HostedTool for ImagePdfView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    fn working(&self) -> bool {
        self.locked()
    }
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        ImagePdfView::request_close(self, cx)
    }
    fn sources(&self) -> Vec<PathBuf> {
        self.pages
            .iter()
            .filter_map(|page| match page {
                PdfInput::File(path) => Some(path.clone()),
                PdfInput::Current { .. } => None,
            })
            .collect()
    }
    /// 换掉文件页；当前结果快照页保留在最前。
    fn set_sources(&mut self, paths: Vec<PathBuf>, _: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        let max_pages = PdfLimits::default().max_pages;
        self.pages
            .retain(|page| matches!(page, PdfInput::Current { .. }));
        let room = max_pages.saturating_sub(self.pages.len());
        self.pages
            .extend(paths.into_iter().take(room).map(PdfInput::File));
        self.invalidate();
        cx.notify();
    }
}

impl EventEmitter<ToolEvent> for ImagePdfView {}

impl ImagePdfView {
    fn new(
        pages: Vec<PdfInput>,
        host: ToolHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        RecentCaptures::load(cx);
        ImagePdfView {
            pages,
            options: PdfOptions::default(),
            request_id: 1,
            prompt: None,
            active: None,
            progress: 0,
            saved: None,
            notice: String::new(),
            failed: false,
            closing: false,
            focus,
            recent: RecentCaptures::default(),
            host,
            thumbs: Thumbs::default(),
        }
    }
}

impl ImagePdfView {
    fn locked(&self) -> bool {
        self.active.is_some() || self.prompt.is_some() || self.closing
    }

    fn invalidate(&mut self) {
        self.request_id = self.request_id.wrapping_add(1);
        self.saved = None;
        self.progress = 0;
        self.notice.clear();
        self.failed = false;
    }

    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
            self.closing = true;
            self.notice = tr!("正在取消并关闭…", "Cancelling and closing…").into();
        } else if self.prompt.is_some() {
            self.closing = true;
            self.notice = tr!(
                "请先关闭文件对话框，随后退出此窗口",
                "Close the file dialog to finish closing this window"
            )
            .into();
        } else {
            return true;
        }
        self.failed = false;
        cx.notify();
        false
    }

    fn add_sources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        self.prompt = Some(PromptKind::Sources);
        let request_id = self.request_id;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr!("添加 PDF 图片页", "Add PDF image pages").into()),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if view.prompt != Some(PromptKind::Sources) {
                    return;
                }
                view.prompt = None;
                if view.closing {
                    host::finish_close(view.host, window, cx);
                    return;
                }
                if view.active.is_some() || view.request_id != request_id {
                    return;
                }
                match chosen {
                    Ok(Ok(Some(paths))) => {
                        let limit = PdfLimits::default().max_pages;
                        if paths.len() > limit.saturating_sub(view.pages.len()) {
                            view.notice = trf!(
                                "PDF 最多 {limit} 页，请减少本次选择",
                                "A PDF can contain at most {limit} pages; choose fewer files"
                            );
                            view.failed = true;
                        } else if !paths.is_empty() {
                            view.pages.extend(paths.into_iter().map(PdfInput::File));
                            view.invalidate();
                        }
                    }
                    Ok(Err(error)) => {
                        view.notice = error.to_string();
                        view.failed = true;
                    }
                    Ok(Ok(None)) | Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn prompt_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() || self.pages.is_empty() {
            return;
        }
        let directory = self
            .pages
            .first()
            .and_then(PdfInput::path_hint)
            .and_then(|path| path.parent())
            .filter(|path| !path.as_os_str().is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let stem = self
            .pages
            .first()
            .and_then(PdfInput::path_hint)
            .and_then(|path| path.file_stem())
            .map(|stem| stem.to_string_lossy())
            .unwrap_or_else(|| "Images".into());
        let name = format!("{stem}_pages.pdf");
        let request_id = self.request_id;
        self.prompt = Some(PromptKind::Save);
        self.notice.clear();
        self.failed = false;
        let receiver = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if view.prompt != Some(PromptKind::Save) {
                    return;
                }
                view.prompt = None;
                if view.closing {
                    host::finish_close(view.host, window, cx);
                    return;
                }
                if view.active.is_some() || view.request_id != request_id {
                    return;
                }
                match chosen {
                    Ok(Ok(Some(path))) => match pdf_path(path) {
                        Ok(path) => view.start_save(path, window, cx),
                        Err(error) => {
                            view.notice = error.to_string();
                            view.failed = true;
                        }
                    },
                    Ok(Err(error)) => {
                        view.notice = error.to_string();
                        view.failed = true;
                    }
                    Ok(Ok(None)) | Err(_) => {
                        view.notice = tr!("已取消保存", "Save cancelled").into();
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_save(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() || self.pages.is_empty() {
            return;
        }
        self.request_id = self.request_id.wrapping_add(1);
        let request_id = self.request_id;
        let pages = self.pages.clone();
        let options = self.options;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicUsize::new(0));
        self.active = Some(ActiveSave {
            request_id,
            cancel: cancel.clone(),
            progress: progress.clone(),
        });
        self.saved = None;
        self.progress = 0;
        self.notice.clear();
        self.failed = false;
        let work_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            save_pages(
                &pages,
                options,
                PdfLimits::default(),
                &path,
                &work_cancel,
                &progress,
            )
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.active.as_ref().is_some_and(|active| {
                    active.request_id == request_id && Arc::ptr_eq(&active.cancel, &cancel)
                }) {
                    return;
                }
                view.active = None;
                match result {
                    Ok(saved) => {
                        // 已提交的文件以核心返回的成功为准，晚到的取消不改写结果。
                        view.notice = trf!(
                            "已保存 {} 页 · {} 字节：{}",
                            "Saved {} pages · {} bytes: {}",
                            saved.pages,
                            saved.bytes,
                            saved.path.display()
                        );
                        view.saved = Some(saved);
                        view.failed = false;
                    }
                    Err(error) => {
                        view.notice = error.to_string();
                        view.failed = error != ToolError::Cancelled;
                    }
                }
                if view.closing {
                    host::finish_close(view.host, window, cx);
                    return;
                }
                cx.notify();
            });
        })
        .detach();
        cx.spawn_in(window, async move |weak, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let polling = weak
                .update_in(cx, |view, _, cx| {
                    let Some(active) = view.active.as_ref().filter(|a| a.request_id == request_id)
                    else {
                        return false;
                    };
                    let progress = active.progress.load(Ordering::Acquire);
                    if view.progress != progress {
                        view.progress = progress;
                        cx.notify();
                    }
                    true
                })
                .unwrap_or(false);
            if !polling {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    fn control(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        match control {
            Control::Close => {
                if self.request_close(cx) {
                    host::finish_close(self.host, window, cx);
                }
                return;
            }
            Control::Cancel => {
                if let Some(active) = &self.active {
                    active.cancel.store(true, Ordering::Release);
                    self.notice = tr!(
                        "正在取消，等待后台清理…",
                        "Cancelling; waiting for cleanup…"
                    )
                    .into();
                    self.failed = false;
                    cx.notify();
                }
                return;
            }
            Control::Copy => {
                if let Some(saved) = &self.saved {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        saved.path.to_string_lossy().into_owned(),
                    ));
                }
                return;
            }
            _ if self.locked() => return,
            Control::Add => {
                self.add_sources(window, cx);
                return;
            }
            Control::Save => {
                self.prompt_save(window, cx);
                return;
            }
            Control::Clear => self.pages.clear(),
            Control::Sort => self.pages.sort_by(|left, right| match (left, right) {
                (PdfInput::Current { .. }, PdfInput::Current { .. }) => std::cmp::Ordering::Equal,
                (PdfInput::Current { .. }, _) => std::cmp::Ordering::Less,
                (_, PdfInput::Current { .. }) => std::cmp::Ordering::Greater,
                (PdfInput::File(left), PdfInput::File(right)) => left
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(
                        &right
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_lowercase(),
                    )
                    .then_with(|| left.cmp(right)),
            }),
            Control::Move(from, before) => {
                if !strip::move_before(&mut self.pages, from, before) {
                    return;
                }
            }
            Control::Remove(index) => {
                if index >= self.pages.len() {
                    return;
                }
                self.pages.remove(index);
            }
        }
        self.invalidate();
        cx.notify();
    }

    fn set_paper(&mut self, paper: PdfPageSize, _: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        self.options.page_size = paper;
        self.invalidate();
        cx.notify();
    }

    fn set_margin(&mut self, margin: u8, _: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        self.options.margin_mm = f64::from(margin);
        self.invalidate();
        cx.notify();
    }

    /// 最近截图点一下加到末页，再点移出。
    fn toggle_capture(&mut self, path: PathBuf, _: &mut Window, cx: &mut Context<Self>) {
        if self.locked() {
            return;
        }
        let existing = self
            .pages
            .iter()
            .position(|page| matches!(page, PdfInput::File(file) if *file == path));
        if let Some(index) = existing {
            self.pages.remove(index);
        } else if self.pages.len() < PdfLimits::default().max_pages {
            self.pages.push(PdfInput::File(path));
        } else {
            return;
        }
        self.invalidate();
        cx.notify();
    }

    fn strip_move(
        &mut self,
        from: usize,
        before: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.control(Control::Move(from, before), window, cx);
    }

    fn strip_remove(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.control(Control::Remove(index), window, cx);
    }

    /// 弹层回图库挑图，独立窗口选文件。
    fn strip_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.locked() && !host::request_pick(self.host, cx) {
            self.control(Control::Add, window, cx);
        }
    }
}

impl Drop for ImagePdfView {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImagePdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let c = chrome::chrome(cx);
        let unlocked = !self.locked();
        let max_pages = PdfLimits::default().max_pages;

        // —— 参数栏 ——
        let marked: Vec<PathBuf> = self
            .pages
            .iter()
            .filter_map(|page| match page {
                PdfInput::File(path) => Some(path.clone()),
                PdfInput::Current { .. } => None,
            })
            .collect();
        let can_add = unlocked && self.pages.len() < max_pages;
        let add = chrome::action(
            "pdf-add",
            tr!("添加图片文件…", "Add image files…"),
            ButtonKind::Secondary,
            can_add,
            c,
        );
        let sources = (self.host == ToolHost::Window).then(|| {
            chrome::field(tr!("最近截图", "Recent captures"), c)
                .child(
                    self.recent
                        .render(&marked, unlocked, c, cx, Self::toggle_capture),
                )
                .child(if can_add {
                    add.on_click(
                        cx.listener(|view, _, window, cx| view.control(Control::Add, window, cx)),
                    )
                } else {
                    add
                })
        });
        let paper = chrome::field(tr!("纸张", "Paper"), c).child(chrome::choice(
            "pdf-paper",
            &[
                (
                    tr!("原尺寸", "Actual"),
                    PdfPageSize::ImagePixels { dpi: 96. },
                ),
                (tr!("A4 竖向", "A4 tall"), PdfPageSize::A4Portrait),
                (tr!("A4 横向", "A4 wide"), PdfPageSize::A4Landscape),
            ],
            self.options.page_size,
            unlocked,
            c,
            cx,
            Self::set_paper,
        ));
        let margin_mm = self.options.margin_mm.round() as u8;
        let margins = chrome::field(tr!("页边距", "Margins"), c).child(chrome::choice(
            "pdf-margin",
            &[("0 mm", 0_u8), ("10 mm", 10), ("20 mm", 20)],
            margin_mm,
            unlocked,
            c,
            cx,
            Self::set_margin,
        ));
        let sidebar = chrome::sidebar("pdf-sidebar", c)
            .children(sources)
            .child(paper)
            .child(margins)
            .child(chrome::hint(
                tr!(
                    "每张图片一页，等比居中不裁剪，透明处为白色；原尺寸按 96 dpi 排版。最多 128 页。",
                    "One image per page, centered without cropping; transparency becomes white. Image size uses 96 dpi. Up to 128 pages."
                ),
                c,
            ));

        // —— 页面列表 ——
        let mut header = div().flex().items_center().gap_2().flex_shrink_0().child(
            div()
                .flex_1()
                .text_size(px(14.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(trf!(
                    "页面 · {} / {max_pages}",
                    "Pages · {} / {max_pages}",
                    self.pages.len()
                )),
        );
        for (id, label, control, enabled) in [
            (
                "pdf-sort",
                tr!("按文件名排序", "Sort by name"),
                Control::Sort,
                unlocked && self.pages.len() > 1,
            ),
            (
                "pdf-clear",
                tr!("清空", "Clear"),
                Control::Clear,
                unlocked && !self.pages.is_empty(),
            ),
        ] {
            let button = chrome::action(id, label, ButtonKind::Secondary, enabled, c);
            header = header.child(if enabled {
                button.on_click(
                    cx.listener(move |view, _, window, cx| view.control(control, window, cx)),
                )
            } else {
                button
            });
        }
        Thumbs::request(self, &marked, cx);
        let tiles = self
            .pages
            .iter()
            .map(|page| match page {
                PdfInput::File(path) => strip::Tile {
                    title: crate::tool_recent_captures::describe(path).0.into(),
                    thumb: self.thumbs.get(path),
                    badge: None,
                },
                input @ PdfInput::Current { .. } => strip::Tile {
                    title: input.label().into(),
                    thumb: None,
                    badge: None,
                },
            })
            .collect();
        let add_label = (self.pages.len() < max_pages).then(|| {
            if self.host == ToolHost::Popup {
                tr!("从图库添加", "Add from gallery")
            } else {
                tr!("添加图片…", "Add images…")
            }
            .into()
        });
        let list = div()
            .id("pdf-pages")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(rgb(c.surface.line))
            .bg(rgb(c.surface.track))
            .child(strip::strip(
                "pdf-tile",
                tiles,
                unlocked,
                add_label,
                c,
                cx,
                Self::strip_move,
                Self::strip_remove,
                Self::strip_add,
            ))
            .when(self.pages.len() > 1, |list| {
                list.child(div().pt_3().child(chrome::hint(
                    tr!(
                        "每格一页，拖动调整页序。",
                        "One page per tile; drag to reorder."
                    ),
                    c,
                )))
            });
        let main = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(header)
            .child(list);

        // —— 操作栏 ——
        let (status, tone): (String, Tone) = if let Some(active) = &self.active {
            let text = if active.cancel.load(Ordering::Acquire) {
                tr!(
                    "正在取消，等待后台任务退出…",
                    "Cancelling; waiting for the task to finish…"
                )
                .into()
            } else if self.progress > self.pages.len() {
                tr!("正在编码并保存 PDF…", "Encoding and saving PDF…").into()
            } else {
                trf!(
                    "正在准备图片页 {}/{}",
                    "Preparing pages {}/{}",
                    self.progress,
                    self.pages.len()
                )
            };
            (text, Tone::Busy)
        } else if !self.notice.is_empty() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Done
                },
            )
        } else if self.pages.is_empty() {
            (
                tr!("选择截图或添加图片文件", "Pick captures or add image files").into(),
                Tone::Idle,
            )
        } else {
            (
                trf!(
                    "共 {} 页，保存时才读取图片",
                    "{} pages; images are read when saving",
                    self.pages.len()
                ),
                Tone::Idle,
            )
        };
        let mut actions = Vec::new();
        if self.saved.is_some() {
            actions.push(
                chrome::action(
                    "pdf-copy",
                    tr!("复制路径", "Copy path"),
                    ButtonKind::Secondary,
                    true,
                    c,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Copy, window, cx)),
                )
                .into_any_element(),
            );
        }
        if self.active.is_some() {
            actions.push(
                chrome::action(
                    "pdf-cancel",
                    tr!("取消", "Cancel"),
                    ButtonKind::Secondary,
                    true,
                    c,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Cancel, window, cx)),
                )
                .into_any_element(),
            );
        }
        let can_save = unlocked && !self.pages.is_empty();
        let save = chrome::action(
            "pdf-save",
            tr!("另存为 PDF…", "Save PDF as…"),
            ButtonKind::Primary,
            can_save,
            c,
        );
        actions.push(
            if can_save {
                save.on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Save, window, cx)),
                )
            } else {
                save
            }
            .into_any_element(),
        );

        chrome::shell(c)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    if !event.is_held && view.prompt.is_none() {
                        view.control(
                            if view.active.is_some() {
                                Control::Cancel
                            } else {
                                Control::Close
                            },
                            window,
                            cx,
                        );
                    }
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(sidebar)
                    .child(main),
            )
            .child(chrome::footer(status, tone, actions, c))
            .into_any_element()
    }
}

fn pdf_path(mut path: PathBuf) -> ToolResult<PathBuf> {
    match path.extension().and_then(|extension| extension.to_str()) {
        None => {
            path.set_extension("pdf");
        }
        Some(extension) if extension.eq_ignore_ascii_case("pdf") => {}
        _ => {
            return Err(ToolError::Invalid(
                tr!("请使用 .pdf 扩展名", "Use the .pdf extension").into(),
            ))
        }
    }
    Ok(path)
}

enum PdfPixels {
    File(Image),
    Current(Arc<ProcessedImage>),
}

impl PdfPixels {
    fn image(&self) -> &Image {
        match self {
            Self::File(image) => image,
            Self::Current(image) => image.image(),
        }
    }
}

fn protected_sources(pages: &[PdfInput]) -> ToolResult<Vec<PathBuf>> {
    let mut protected = Vec::new();
    let limit = PdfLimits::default().max_pages;
    for page in pages {
        let sources = match page {
            PdfInput::File(path) => std::slice::from_ref(path),
            PdfInput::Current { protected, .. } => protected.as_slice(),
        };
        for source in sources {
            if protected.contains(source) {
                continue;
            }
            if protected.len() >= limit {
                return Err(ToolError::Invalid(
                    tr!("原片保护路径过多", "Too many protected source paths").into(),
                ));
            }
            protected.try_reserve(1).map_err(|_| {
                ToolError::Failed(
                    tr!(
                        "无法分配原片保护列表",
                        "Unable to allocate source protection list"
                    )
                    .into(),
                )
            })?;
            protected.push(source.clone());
        }
    }
    Ok(protected)
}

/// 仅后台调用；整个集合受总像素预算限制，快照只增加 Arc 引用，不重读来源文件。
fn save_pages(
    pages: &[PdfInput],
    options: PdfOptions,
    limits: PdfLimits,
    path: &Path,
    cancel: &AtomicBool,
    progress: &AtomicUsize,
) -> ToolResult<SavedPdf> {
    check_cancel(cancel)?;
    if pages.is_empty() || pages.len() > limits.max_pages {
        return Err(ToolError::Invalid(
            tr!(
                "PDF 图片数量无效或超过上限",
                "Invalid or excessive PDF image count"
            )
            .into(),
        ));
    }
    let output =
        std::path::absolute(path).map_err(|error| ToolError::Invalid(error.to_string()))?;
    let parent = output
        .parent()
        .ok_or_else(|| ToolError::Invalid(tr!("输出目录无效", "Invalid output folder").into()))?;
    crate::image_collection::ensure_local_directory(parent).map_err(ToolError::Invalid)?;
    crate::image_collection::ensure_local_file_or_missing(&output).map_err(ToolError::Invalid)?;
    let protected = protected_sources(pages)?;
    for page in pages {
        check_cancel(cancel)?;
        if let PdfInput::File(path) = page {
            crate::image_collection::ensure_local_file(path).map_err(ToolError::Invalid)?;
        }
    }
    for source in &protected {
        check_cancel(cancel)?;
        crate::image_collection::ensure_local_file_or_missing(source)
            .map_err(ToolError::Invalid)?;
        if crate::export::is_same_path(source, &output) {
            return Err(ToolError::Invalid(
                tr!(
                    "请另存为新文件，保留原图",
                    "Save to a new file to preserve the source"
                )
                .into(),
            ));
        }
    }
    match std::fs::symlink_metadata(&output) {
        Ok(_) => {
            return Err(ToolError::Invalid(
                tr!(
                    "目标文件已存在，请选择新文件名",
                    "The file already exists; choose a new name"
                )
                .into(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ToolError::Invalid(error.to_string())),
    }
    let mut images: Vec<PdfPixels> = Vec::new();
    images.try_reserve_exact(pages.len()).map_err(|_| {
        ToolError::Failed(
            tr!(
                "没有足够内存处理图片",
                "Not enough memory to process the images"
            )
            .into(),
        )
    })?;
    let mut pixels = 0_u64;
    for (index, source) in pages.iter().enumerate() {
        check_cancel(cancel)?;
        let remaining = remaining_decode_bytes(pixels, limits.max_total_pixels)?;
        let loaded = match source {
            PdfInput::File(path) => {
                let decoded = crate::image_decode::decode_path_with_budget_cancellable(
                    path, remaining, cancel,
                );
                check_cancel(cancel)?;
                PdfPixels::File(
                    decoded
                        .map_err(|error| {
                            ToolError::Failed(trf!(
                                "第 {} 张图片读取失败：{} · {error}",
                                "Image {} could not be read: {} · {error}",
                                index + 1,
                                path.display()
                            ))
                        })?
                        .image,
                )
            }
            PdfInput::Current { image, .. } => {
                validate_input(
                    image.image(),
                    ToolLimits {
                        max_input_pixels: remaining / 4,
                        ..ToolLimits::default()
                    },
                )?;
                PdfPixels::Current(image.clone())
            }
        };
        check_cancel(cancel)?;
        let decoded = loaded.image();
        pixels = pixels
            .checked_add(u64::from(decoded.width) * u64::from(decoded.height))
            .filter(|sum| *sum <= limits.max_total_pixels)
            .ok_or_else(|| {
                ToolError::Invalid(
                    tr!(
                        "图片总像素超过 PDF 预算",
                        "Total image pixels exceed the PDF limit"
                    )
                    .into(),
                )
            })?;
        // 复用核心布局校验，失败信息可定位具体图片；不生成第二份像素。
        crate::image_pdf::page_layout(decoded.width, decoded.height, options).map_err(|error| {
            ToolError::Invalid(trf!(
                "第 {} 页无法排版：{error}",
                "Page {} could not be laid out: {error}",
                index + 1
            ))
        })?;
        images.push(loaded);
        progress.store(index + 1, Ordering::Release);
    }
    check_cancel(cancel)?;
    progress.store(pages.len() + 1, Ordering::Release);
    let references: Vec<&Image> = images.iter().map(PdfPixels::image).collect();
    crate::image_pdf::save(&references, options, limits, &output, &protected, cancel)
}

fn remaining_decode_bytes(pixels: u64, max_pixels: u64) -> ToolResult<u64> {
    max_pixels
        .checked_sub(pixels)
        .filter(|remaining| *remaining > 0)
        .and_then(|remaining| remaining.checked_mul(4))
        .ok_or_else(|| {
            ToolError::Invalid(
                tr!(
                    "图片总像素超过 PDF 预算",
                    "Total image pixels exceed the PDF limit"
                )
                .into(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rustshot-pdf-snapshot-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot() -> Arc<ProcessedImage> {
        Arc::new(ProcessedImage::new(
            Image {
                width: 2,
                height: 1,
                bgra: vec![0, 0, 255, 255, 0, 255, 0, 0],
                readback_ms: 0.,
            },
            17,
        ))
    }

    #[test]
    fn image_pdf_view_snapshot_uses_current_pixels_and_mixes_with_file_pages_in_order() {
        let directory = TestDirectory::new();
        let original_path = directory.0.join("moved-original.png");
        let current = snapshot();
        let file_path = directory.0.join("blue.png");
        let file_image = Image {
            width: 1,
            height: 1,
            bgra: vec![255, 0, 0, 255],
            readback_ms: 0.,
        };
        let original_file_bytes =
            crate::export::encode_image(&file_image, crate::save_options::SaveEncoding::default())
                .unwrap();
        std::fs::write(&file_path, &original_file_bytes).unwrap();
        let pages = vec![
            PdfInput::Current {
                image: current.clone(),
                protected: Arc::new(vec![original_path.clone()]),
            },
            PdfInput::File(file_path.clone()),
        ];
        let output = directory.0.join("mixed.pdf");
        let progress = AtomicUsize::new(0);
        let saved = save_pages(
            &pages,
            PdfOptions::default(),
            PdfLimits::default(),
            &output,
            &AtomicBool::new(false),
            &progress,
        )
        .unwrap();
        assert_eq!((saved.pages, progress.load(Ordering::Acquire)), (2, 3));
        let bytes = std::fs::read(&saved.path).unwrap();
        assert_eq!(saved.bytes, bytes.len());
        let first_marker = b"/DeviceRGB /BitsPerComponent 8 /Length 6 >>\nstream\n";
        let second_marker = b"/DeviceRGB /BitsPerComponent 8 /Length 3 >>\nstream\n";
        let first = bytes
            .windows(first_marker.len())
            .position(|item| item == first_marker)
            .unwrap()
            + first_marker.len();
        let second = bytes
            .windows(second_marker.len())
            .position(|item| item == second_marker)
            .unwrap()
            + second_marker.len();
        assert!(first < second);
        assert_eq!(&bytes[first..first + 6], &[255, 0, 0, 255, 255, 255]);
        assert_eq!(&bytes[second..second + 3], &[0, 0, 255]);
        assert_eq!(current.image().bgra, [0, 0, 255, 255, 0, 255, 0, 0]);
        assert!(!original_path.exists());
        assert_eq!(std::fs::read(file_path).unwrap(), original_file_bytes);
    }

    #[test]
    fn image_pdf_view_snapshot_budget_cancel_and_source_protection_commit_nothing() {
        let directory = TestDirectory::new();
        let current = snapshot();
        let page = PdfInput::Current {
            image: current,
            protected: Arc::new(Vec::new()),
        };
        let output = directory.0.join("budget.pdf");
        let progress = AtomicUsize::new(0);
        let limits = PdfLimits {
            max_total_pixels: 3,
            ..PdfLimits::default()
        };
        assert!(save_pages(
            &[page.clone(), page.clone()],
            PdfOptions::default(),
            limits,
            &output,
            &AtomicBool::new(false),
            &progress
        )
        .is_err());
        assert_eq!(progress.load(Ordering::Acquire), 1);
        assert!(!output.exists());
        assert_eq!(
            save_pages(
                std::slice::from_ref(&page),
                PdfOptions::default(),
                PdfLimits::default(),
                &output,
                &AtomicBool::new(true),
                &progress
            )
            .unwrap_err(),
            ToolError::Cancelled
        );
        assert!(!output.exists());
        let protected = PdfInput::Current {
            image: snapshot(),
            protected: Arc::new(vec![output.clone()]),
        };
        assert!(save_pages(
            &[protected],
            PdfOptions::default(),
            PdfLimits::default(),
            &output,
            &AtomicBool::new(false),
            &progress
        )
        .is_err());
        assert!(!output.exists());
    }

    #[test]
    fn image_pdf_view_aggregate_budget_is_applied_before_each_decode() {
        assert_eq!(
            remaining_decode_bytes(31_000_000, 32_000_000).unwrap(),
            4_000_000
        );
        assert!(remaining_decode_bytes(32_000_000, 32_000_000).is_err());
        assert!(remaining_decode_bytes(32_000_001, 32_000_000).is_err());
        assert!(remaining_decode_bytes(0, u64::MAX).is_err());
    }

    #[test]
    fn image_pdf_view_output_extension_is_explicit() {
        assert_eq!(
            pdf_path(PathBuf::from("pages")).unwrap(),
            PathBuf::from("pages.pdf")
        );
        assert_eq!(
            pdf_path(PathBuf::from("pages.PDF")).unwrap(),
            PathBuf::from("pages.PDF")
        );
        assert!(pdf_path(PathBuf::from("pages.png")).is_err());
    }
}
