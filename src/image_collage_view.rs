//! 图片拼接窗：左侧选截图与参数，右侧自动预览。来源与参数在路径对话框和保存期间锁定；
//! 预览进行中改动会中止本次预览并按新参数重来。
use crate::{
    capture::Image,
    image_collage::{self, CollageAlignment, CollageDirection, CollageOptions, CollageSizing},
    image_resize::{resize, ResizeFilter},
    image_tools::{
        check_cancel, validate_input, ProcessedImage, ToolError, ToolLimits, ToolResult,
    },
    save_options::{SaveEncoding, SaveFormat},
    settings_widgets::ButtonKind,
    tool_recent_captures::{RecentCaptures, RecentHost},
    tool_window_chrome::{self as chrome, Tone},
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    div, img, prelude::*, px, rgb, size, App, AppContext, Bounds, Context, EntityId, FocusHandle,
    KeyDownEvent, ObjectFit, PathPromptOptions, Render, RenderImage, Task, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

/// 改动停顿多久后自动预览；连续点选参数只做最后一次。
const AUTO_PREVIEW_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Row,
    Column,
    Grid,
}

const MAX_SOURCES: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActivityKind {
    PickSources,
    Preview,
    PickSave,
    Save,
}

#[derive(Clone)]
struct Activity {
    kind: ActivityKind,
    request_id: u64,
    cancel: Arc<AtomicBool>,
}

struct Preview {
    processed: Arc<ProcessedImage>,
    image: Arc<RenderImage>,
}

#[derive(Clone)]
struct SourceReorderDrag {
    owner: EntityId,
    request_id: u64,
    source_index: usize,
    source_path: PathBuf,
    label: String,
}

struct SourceDragGhost(String);

impl Render for SourceDragGhost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let palette = crate::theme::palette(cx);
        div()
            .max_w(px(360.))
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(palette.accent))
            .bg(rgb(palette.background))
            .text_size(px(12.))
            .text_color(rgb(palette.foreground))
            .truncate()
            .child(self.0.clone())
    }
}

#[derive(Clone, Copy)]
enum Control {
    Add,
    Up(usize),
    Down(usize),
    Remove(usize),
    Save,
    Cancel,
    Close,
}

pub(crate) struct ImageCollageView {
    paths: Vec<PathBuf>,
    options: CollageOptions,
    filter: ResizeFilter,
    encoding: SaveEncoding,
    request_id: u64,
    activity: Option<Activity>,
    result: Option<Arc<ProcessedImage>>,
    preview: Option<Arc<RenderImage>>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    recent: RecentCaptures,
    preview_pending: bool,
    auto: Option<Task<()>>,
}

impl RecentHost for ImageCollageView {
    fn recent(&mut self) -> &mut RecentCaptures {
        &mut self.recent
    }
}

pub(crate) fn open(paths: Vec<PathBuf>, cx: &mut App) -> Result<WindowHandle<ImageCollageView>> {
    let paths = absolute_paths(paths)?;
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(1120.), px(780.)), |display| {
            let bounds = display.bounds();
            size(
                px(1120_f32.min((f32::from(bounds.size.width) - 48.).max(640.))),
                px(780_f32.min((f32::from(bounds.size.height) - 72.).max(480.))),
            )
        });
    let bounds = Bounds::centered(None, dimensions, cx);
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(brand_title!("图片拼接", "Image collage"));
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                RecentCaptures::load(cx);
                let preview_pending = !paths.is_empty();
                ImageCollageView {
                    paths,
                    options: CollageOptions {
                        alignment: CollageAlignment::Center,
                        ..Default::default()
                    },
                    filter: ResizeFilter::Bilinear,
                    encoding: SaveEncoding::default(),
                    request_id: 1,
                    activity: None,
                    result: None,
                    preview: None,
                    retiring: Vec::new(),
                    retired: Vec::new(),
                    notice: String::new(),
                    failed: false,
                    closing: false,
                    focus,
                    recent: RecentCaptures::default(),
                    preview_pending,
                    auto: None,
                }
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            view
        },
    )?)
}

impl ImageCollageView {
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.closing = true;
        if let Some(activity) = &self.activity {
            activity.cancel.store(true, Ordering::Release);
            if matches!(activity.kind, ActivityKind::Preview | ActivityKind::Save) {
                self.notice = tr!("正在取消并关闭…", "Cancelling and closing…").into();
                self.failed = false;
                cx.notify();
                return false;
            }
        }
        true
    }

    fn busy(&self) -> bool {
        self.closing || self.activity.is_some()
    }

    fn begin(&mut self, kind: ActivityKind) -> Option<Activity> {
        if self.busy() {
            return None;
        }
        let activity = Activity {
            kind,
            request_id: self.request_id,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        self.activity = Some(activity.clone());
        self.notice.clear();
        self.failed = false;
        Some(activity)
    }

    fn finish(&mut self, activity: &Activity) -> bool {
        if !self
            .activity
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.cancel, &activity.cancel))
        {
            return false;
        }
        self.activity = None;
        true
    }

    fn clear_result(&mut self) {
        self.result = None;
        if let Some(image) = self.preview.take() {
            self.retiring.push(image);
        }
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.request_id = self.request_id.wrapping_add(1);
        self.clear_result();
        self.notice.clear();
        self.failed = false;
        self.auto = None;
        self.preview_pending = !self.paths.is_empty();
        cx.notify();
    }

    fn preview_running(&self) -> bool {
        self.activity
            .as_ref()
            .is_some_and(|activity| activity.kind == ActivityKind::Preview)
    }

    /// 预览进行中仍可改来源和参数；对话框和保存期间锁定。
    fn editable(&self) -> bool {
        !self.closing && (self.activity.is_none() || self.preview_running())
    }

    /// 改来源或参数：中止正在生成的预览，结果作废，停顿后按新内容自动预览。
    fn edit(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Self) -> bool) {
        if !self.editable() || !change(self) {
            return;
        }
        if let Some(activity) = self.activity.as_ref().filter(|_| self.preview_running()) {
            activity.cancel.store(true, Ordering::Release);
        }
        self.invalidate(cx);
    }

    fn drop_source(&mut self, drag: &SourceReorderDrag, before: usize, cx: &mut Context<Self>) {
        let owner = cx.entity_id();
        self.edit(cx, |view| {
            reorder_before(&mut view.paths, drag, owner, view.request_id, before)
        });
    }

    /// 最近截图点一下加入末尾，再点移出。
    fn toggle_capture(&mut self, path: PathBuf, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            if let Some(index) = view.paths.iter().position(|added| *added == path) {
                view.paths.remove(index);
                true
            } else if view.paths.len() < MAX_SOURCES {
                view.paths.push(path);
                true
            } else {
                false
            }
        });
    }

    fn set_layout(&mut self, layout: Layout, _: &mut Window, cx: &mut Context<Self>) {
        let columns = match self.options.direction {
            CollageDirection::Grid { columns } => columns,
            _ => 2,
        };
        self.edit(cx, |view| {
            view.options.direction = match layout {
                Layout::Row => CollageDirection::Horizontal,
                Layout::Column => CollageDirection::Vertical,
                Layout::Grid => CollageDirection::Grid { columns },
            };
            true
        });
    }

    fn set_columns(&mut self, columns: u32, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            view.options.direction = CollageDirection::Grid { columns };
            true
        });
    }

    fn set_gap(&mut self, gap: u32, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            view.options.gap = gap;
            true
        });
    }

    fn set_sizing(&mut self, sizing: CollageSizing, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            view.options.sizing = sizing;
            true
        });
    }

    fn set_filter(&mut self, filter: ResizeFilter, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            view.filter = filter;
            true
        });
    }

    fn set_alignment(
        &mut self,
        alignment: CollageAlignment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit(cx, |view| {
            view.options.alignment = alignment;
            true
        });
    }

    fn set_background(&mut self, background: [u8; 4], _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            view.options.background_bgra = background;
            true
        });
    }

    fn set_format(&mut self, format: SaveFormat, _: &mut Window, cx: &mut Context<Self>) {
        self.edit(cx, |view| {
            view.encoding.format = format;
            true
        });
    }

    fn save_ready(&self) -> bool {
        !self.busy()
            && self
                .result
                .as_ref()
                .is_some_and(|image| image.request_id() == self.request_id)
    }

    fn choose_sources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(activity) = self.begin(ActivityKind::PickSources) else {
            return;
        };
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(tr!("添加拼接图片", "Add collage images").into()),
        });
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, _, cx| {
                if !view.finish(&activity) {
                    return;
                }
                if activity.cancel.load(Ordering::Acquire) || activity.request_id != view.request_id
                {
                    view.notice = tr!("已取消选择", "Selection cancelled").into();
                    cx.notify();
                    return;
                }
                match chosen {
                    Ok(Ok(Some(paths))) if !paths.is_empty() => {
                        let added = paths.len();
                        if added > MAX_SOURCES.saturating_sub(view.paths.len()) {
                            view.notice = source_count_error().to_string();
                            view.failed = true;
                        } else {
                            match absolute_paths(paths) {
                                Ok(paths) => {
                                    view.paths.extend(paths);
                                    view.invalidate(cx);
                                }
                                Err(error) => {
                                    view.notice = error.to_string();
                                    view.failed = true;
                                }
                            }
                        }
                    }
                    Ok(Ok(_)) | Err(_) => {}
                    Ok(Err(error)) => {
                        view.notice = error.to_string();
                        view.failed = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.paths.is_empty() {
            return;
        }
        self.clear_result();
        let Some(activity) = self.begin(ActivityKind::Preview) else {
            return;
        };
        let paths = self.paths.clone();
        let options = self.options;
        let filter = self.filter;
        let encoding = self.encoding;
        let work = activity.clone();
        let task = cx.background_executor().spawn(async move {
            let images = decode_sources(&paths, &work.cancel)?;
            check_cancel(&work.cancel)?;
            let references: Vec<_> = images.iter().collect();
            let processed = image_collage::compose_with_filter(
                &references,
                options,
                filter,
                limits(),
                work.request_id,
                &work.cancel,
            )?;
            // 拼接结束即释放所有源大帧，再生成结果的小预览。
            drop(references);
            drop(images);
            let image = preview_image(processed.image(), encoding, &work.cancel)?;
            check_cancel(&work.cancel)?;
            Ok::<_, ToolError>(Preview {
                processed: Arc::new(processed),
                image,
            })
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.finish(&activity) {
                    return;
                }
                if view.closing {
                    drop(result);
                    window.remove_window();
                    return;
                }
                if activity.request_id != view.request_id {
                    // 预览期间改了来源或参数：丢弃旧结果，自动预览会按新内容重来。
                    drop(result);
                } else if activity.cancel.load(Ordering::Acquire) {
                    drop(result);
                    view.notice = tr!("已取消预览", "Preview cancelled").into();
                    view.failed = false;
                } else {
                    match result {
                        Ok(result) => {
                            view.result = Some(result.processed);
                            view.preview = Some(result.image);
                        }
                        Err(error) => {
                            view.notice = error.to_string();
                            view.failed = error != ToolError::Cancelled;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn prompt_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_ready() {
            return;
        }
        let encoding = self.encoding;
        let Some(activity) = self.begin(ActivityKind::PickSave) else {
            return;
        };
        let directory = self
            .paths
            .first()
            .and_then(|path| path.parent())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let name = format!(
            "{}.{}",
            tr!("拼接结果", "Collage"),
            encoding.format.extension()
        );
        let receiver = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = receiver.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.finish(&activity) {
                    return;
                }
                if activity.cancel.load(Ordering::Acquire) {
                    view.notice = tr!("已取消保存", "Save cancelled").into();
                } else if activity.request_id != view.request_id
                    || view.encoding != encoding
                    || !view.save_ready()
                {
                    view.notice = tr!(
                        "来源或参数已变化，请等新预览完成后再保存",
                        "Sources or options changed; save after the new preview"
                    )
                    .into();
                    view.failed = true;
                } else {
                    match chosen {
                        Ok(Ok(Some(mut path))) => {
                            if path.extension().is_none() {
                                path.set_extension(encoding.format.extension());
                            }
                            view.start_save(path, window, cx);
                        }
                        Ok(Ok(None)) | Err(_) => {
                            view.notice = tr!("已取消保存", "Save cancelled").into();
                        }
                        Ok(Err(error)) => {
                            view.notice = error.to_string();
                            view.failed = true;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_save(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_ready() {
            return;
        }
        let Some(image) = self.result.clone() else {
            return;
        };
        let Some(activity) = self.begin(ActivityKind::Save) else {
            return;
        };
        let protected = self.paths.clone();
        let encoding = self.encoding;
        let work = activity.clone();
        let task = cx.background_executor().spawn(async move {
            image.save(work.request_id, &path, encoding, &protected, &work.cancel)
        });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.finish(&activity) {
                    return;
                }
                // 成功提交后不再根据取消标记改判；取消也不提前释放槽或参数锁。
                match result {
                    Ok(saved) => {
                        view.notice = trf!("已保存：{}", "Saved: {}", saved.path.display());
                        view.failed = false;
                    }
                    Err(error) => {
                        view.notice = error.to_string();
                        view.failed = error != ToolError::Cancelled;
                    }
                }
                if view.closing {
                    window.remove_window();
                    return;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Control::Close => {
                if self.request_close(cx) {
                    window.remove_window();
                }
            }
            Control::Cancel => {
                if let Some(activity) = &self.activity {
                    activity.cancel.store(true, Ordering::Release);
                }
                self.notice = tr!(
                    "已请求取消，等待当前任务返回",
                    "Cancellation requested; waiting for the current task"
                )
                .into();
                self.failed = false;
                cx.notify();
            }
            Control::Add => {
                if !self.busy() {
                    self.choose_sources(window, cx);
                }
            }
            Control::Save => self.prompt_save(window, cx),
            Control::Up(index) => self.edit(cx, |view| move_source(&mut view.paths, index, -1)),
            Control::Down(index) => self.edit(cx, |view| move_source(&mut view.paths, index, 1)),
            Control::Remove(index) => self.edit(cx, |view| {
                if index < view.paths.len() {
                    view.paths.remove(index);
                    true
                } else {
                    false
                }
            }),
        }
    }

    fn status(&self) -> &'static str {
        match self.activity.as_ref().map(|activity| activity.kind) {
            Some(ActivityKind::PickSources) => tr!("正在选择来源…", "Choosing sources…"),
            Some(ActivityKind::Preview) => tr!("正在生成预览…", "Generating preview…"),
            Some(ActivityKind::PickSave) => tr!("正在选择保存位置…", "Choosing a save location…"),
            Some(ActivityKind::Save) => {
                tr!("正在保存，参数暂时锁定…", "Saving; options are locked…")
            }
            None if self.paths.is_empty() => tr!(
                "从左侧选择截图，或添加图片文件",
                "Pick captures on the left or add image files"
            ),
            None if self.result.is_some() => tr!("拼接结果已就绪", "Collage ready"),
            None => tr!("即将更新预览…", "Updating preview…"),
        }
    }
}

impl Drop for ImageCollageView {
    fn drop(&mut self) {
        if let Some(activity) = &self.activity {
            activity.cancel.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageCollageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let c = chrome::chrome(cx);
        let palette = c.palette;
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.retired.append(&mut self.retiring);
        if self.preview_pending && self.auto.is_none() {
            self.auto = Some(cx.spawn_in(window, async move |weak, cx| {
                cx.background_executor().timer(AUTO_PREVIEW_DELAY).await;
                let _ = weak.update_in(cx, |view, window, cx| {
                    view.auto = None;
                    // 上一次预览尚未退出时保持待办，下一帧再排。
                    if !view.busy() {
                        view.preview_pending = false;
                        view.start_preview(window, cx);
                    }
                    cx.notify();
                });
            }));
        }
        let editable = self.editable();
        let owner = cx.entity_id();

        // —— 来源 ——
        let mut sources = chrome::field(
            trf!(
                "图片 · {} / {MAX_SOURCES}",
                "Images · {} / {MAX_SOURCES}",
                self.paths.len()
            ),
            c,
        )
        .child(
            self.recent
                .render(&self.paths, editable, c, cx, Self::toggle_capture),
        );
        let add = chrome::action(
            "collage-add",
            tr!("添加图片文件…", "Add image files…"),
            ButtonKind::Secondary,
            !self.busy() && self.paths.len() < MAX_SOURCES,
            c,
        );
        sources = sources.child(if !self.busy() && self.paths.len() < MAX_SOURCES {
            add.on_click(cx.listener(|view, _, window, cx| view.control(Control::Add, window, cx)))
        } else {
            add
        });
        if !self.paths.is_empty() {
            let mut list = div().flex().flex_col();
            let count = self.paths.len();
            for (index, path) in self.paths.iter().enumerate() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let (title, detail) = crate::tool_recent_captures::describe(path);
                let drag = SourceReorderDrag {
                    owner,
                    request_id: self.request_id,
                    source_index: index,
                    source_path: path.clone(),
                    label: format!("{}. {name}", index + 1).chars().take(120).collect(),
                };
                let row = chrome::list_row(
                    ("collage-source", index),
                    index + 1,
                    title,
                    detail.map(Into::into),
                    c,
                )
                .child(chrome::row_icon(
                    ("collage-up", index),
                    Icon::ChevronUp,
                    editable && index > 0,
                    c,
                    cx,
                    move |view, window, cx| view.control(Control::Up(index), window, cx),
                ))
                .child(chrome::row_icon(
                    ("collage-down", index),
                    Icon::Chevron,
                    editable && index + 1 < count,
                    c,
                    cx,
                    move |view, window, cx| view.control(Control::Down(index), window, cx),
                ))
                .child(chrome::row_icon(
                    ("collage-remove", index),
                    Icon::Close,
                    editable,
                    c,
                    cx,
                    move |view, window, cx| view.control(Control::Remove(index), window, cx),
                ));
                list = list.child(if editable {
                    row.cursor_grab()
                        .on_drag(drag, |drag: &SourceReorderDrag, _, _, cx: &mut App| {
                            cx.new(|_| SourceDragGhost(drag.label.clone()))
                        })
                        .drag_over::<SourceReorderDrag>(move |style, _, _, _| {
                            style.bg(rgb(palette.hover))
                        })
                        .on_drop(cx.listener(move |view, drag: &SourceReorderDrag, _, cx| {
                            view.drop_source(drag, index, cx);
                            cx.stop_propagation();
                        }))
                } else {
                    row
                });
            }
            let end = self.paths.len();
            list = list.child(div().id("collage-drop-end").h(px(12.)).rounded_sm().when(
                editable,
                |target| {
                    target
                        .drag_over::<SourceReorderDrag>(move |style, _, _, _| {
                            style.bg(rgb(palette.hover))
                        })
                        .on_drop(cx.listener(move |view, drag: &SourceReorderDrag, _, cx| {
                            view.drop_source(drag, end, cx);
                            cx.stop_propagation();
                        }))
                },
            ));
            sources = sources.child(list).child(chrome::hint(
                tr!(
                    "按列表顺序拼接，可拖动调整。",
                    "Images join in list order; drag to reorder."
                ),
                c,
            ));
        }

        // —— 参数 ——
        let layout = match self.options.direction {
            CollageDirection::Horizontal => Layout::Row,
            CollageDirection::Vertical => Layout::Column,
            CollageDirection::Grid { .. } => Layout::Grid,
        };
        let mut arrange = chrome::field(tr!("排列", "Layout"), c).child(chrome::choice(
            "collage-layout",
            &[
                (tr!("横排", "Row"), Layout::Row),
                (tr!("竖排", "Column"), Layout::Column),
                (tr!("网格", "Grid"), Layout::Grid),
            ],
            layout,
            editable,
            c,
            cx,
            Self::set_layout,
        ));
        if let CollageDirection::Grid { columns } = self.options.direction {
            arrange = arrange.child(chrome::option_row(tr!("每行", "Per row"), c).child(
                chrome::choice(
                    "collage-columns",
                    &[("2", 2), ("3", 3), ("4", 4)],
                    columns,
                    editable,
                    c,
                    cx,
                    Self::set_columns,
                ),
            ));
        }
        let spacing = chrome::field(tr!("间距", "Gap"), c).child(chrome::choice(
            "collage-gap",
            &[(tr!("无", "None"), 0), ("8 px", 8), ("24 px", 24)],
            self.options.gap,
            editable,
            c,
            cx,
            Self::set_gap,
        ));
        let mut sizing = chrome::field(tr!("统一尺寸", "Size"), c).child(chrome::choice(
            "collage-sizing",
            &[
                (tr!("原尺寸", "Original"), CollageSizing::Original),
                (tr!("等宽 720", "Width 720"), CollageSizing::EqualWidth(720)),
                (
                    tr!("等高 480", "Height 480"),
                    CollageSizing::EqualHeight(480),
                ),
            ],
            self.options.sizing,
            editable,
            c,
            cx,
            Self::set_sizing,
        ));
        if self.options.sizing != CollageSizing::Original {
            sizing = sizing.child(chrome::option_row(tr!("缩放", "Scaling"), c).child(
                chrome::choice(
                    "collage-filter",
                    &[
                        (tr!("平滑", "Smooth"), ResizeFilter::Bilinear),
                        (tr!("像素", "Pixel"), ResizeFilter::Nearest),
                    ],
                    self.filter,
                    editable,
                    c,
                    cx,
                    Self::set_filter,
                ),
            ));
        }
        let alignment = chrome::field(tr!("对齐", "Align"), c).child(chrome::choice(
            "collage-alignment",
            &[
                (tr!("起点", "Start"), CollageAlignment::Start),
                (tr!("居中", "Center"), CollageAlignment::Center),
                (tr!("终点", "End"), CollageAlignment::End),
            ],
            self.options.alignment,
            editable,
            c,
            cx,
            Self::set_alignment,
        ));
        let background = chrome::field(tr!("背景", "Background"), c).child(chrome::choice(
            "collage-background",
            &[
                (tr!("透明", "Clear"), [0, 0, 0, 0]),
                (tr!("白色", "White"), [255, 255, 255, 255]),
                (tr!("深色", "Dark"), [32, 32, 32, 255]),
            ],
            self.options.background_bgra,
            editable,
            c,
            cx,
            Self::set_background,
        ));
        let format = chrome::field(tr!("保存格式", "Format"), c)
            .child(chrome::choice(
                "collage-format",
                &[
                    ("PNG", SaveFormat::Png),
                    ("JPEG", SaveFormat::Jpeg),
                    ("BMP", SaveFormat::Bmp),
                    ("WebP", SaveFormat::Webp),
                ],
                self.encoding.format,
                editable,
                c,
                cx,
                Self::set_format,
            ))
            .child(chrome::hint(
                tr!(
                    "PNG、WebP（无损）保留透明，JPEG、BMP 用白底；动图只取首帧。",
                    "PNG and lossless WebP keep transparency; JPEG and BMP use white. Animated images use the first frame."
                ),
                c,
            ));

        let sidebar = chrome::sidebar("collage-sidebar", c)
            .child(sources)
            .child(arrange)
            .child(spacing)
            .child(sizing)
            .child(alignment)
            .child(background)
            .child(format);

        // —— 预览 ——
        let stage = chrome::stage(c)
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .p_4()
            .child(match &self.preview {
                Some(image) => img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                None => chrome::empty_state(Icon::Collage, self.status(), c).into_any_element(),
            });
        let main = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .p_5()
            .child(stage);

        let (status, tone): (String, Tone) = if !self.notice.is_empty() {
            (
                self.notice.clone(),
                if self.failed {
                    Tone::Failed
                } else {
                    Tone::Done
                },
            )
        } else if let Some(result) = self.result.as_ref().filter(|_| self.save_ready()) {
            (
                trf!(
                    "成品 {} × {} 像素",
                    "Result {} × {} px",
                    result.image().width,
                    result.image().height
                ),
                Tone::Done,
            )
        } else if self.busy() {
            (self.status().to_owned(), Tone::Busy)
        } else {
            (self.status().to_owned(), Tone::Idle)
        };
        let mut actions = Vec::new();
        if matches!(
            self.activity.as_ref().map(|activity| activity.kind),
            Some(ActivityKind::Preview | ActivityKind::Save)
        ) {
            actions.push(
                chrome::action(
                    "collage-cancel",
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
        let save = chrome::action(
            "collage-save",
            tr!("另存为…", "Save as…"),
            ButtonKind::Primary,
            self.save_ready(),
            c,
        );
        actions.push(
            if self.save_ready() {
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
                if event.is_held {
                    return;
                }
                if event.keystroke.key == "s" && event.keystroke.modifiers.control {
                    view.control(Control::Save, window, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    view.control(
                        if view.busy() {
                            Control::Cancel
                        } else {
                            Control::Close
                        },
                        window,
                        cx,
                    );
                    cx.stop_propagation();
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

fn limits() -> ToolLimits {
    ToolLimits {
        max_items: MAX_SOURCES,
        ..ToolLimits::default()
    }
}

fn source_count_error() -> ToolError {
    ToolError::Invalid(trf!(
        "最多添加 {MAX_SOURCES} 张图片",
        "Add at most {MAX_SOURCES} images"
    ))
}

fn absolute_paths(paths: Vec<PathBuf>) -> ToolResult<Vec<PathBuf>> {
    if paths.len() > MAX_SOURCES {
        return Err(source_count_error());
    }
    paths
        .into_iter()
        .map(|path| {
            std::path::absolute(path).map_err(|error| ToolError::Invalid(error.to_string()))
        })
        .collect()
}

fn move_source(paths: &mut [PathBuf], index: usize, delta: isize) -> bool {
    let Some(target) = index.checked_add_signed(delta) else {
        return false;
    };
    if index >= paths.len() || target >= paths.len() {
        return false;
    }
    paths.swap(index, target);
    true
}

/// before 是移除来源前的插入边界，len 表示末尾；只有顺序实际变化才返回 true。
fn reorder_before(
    paths: &mut [PathBuf],
    drag: &SourceReorderDrag,
    owner: EntityId,
    request_id: u64,
    before: usize,
) -> bool {
    if drag.owner != owner
        || drag.request_id != request_id
        || drag.source_index >= paths.len()
        || before > paths.len()
        || paths[drag.source_index] != drag.source_path
    {
        return false;
    }
    let source = drag.source_index;
    let target = if before > source { before - 1 } else { before };
    if source == target {
        return false;
    }
    if source < target {
        paths[source..=target].rotate_left(1);
    } else {
        paths[target..=source].rotate_right(1);
    }
    true
}

fn source_error(index: usize, path: &std::path::Path, error: impl std::fmt::Display) -> ToolError {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    ToolError::Failed(trf!(
        "第 {} 张「{name}」：{error}",
        "Image {} ({name}): {error}",
        index + 1
    ))
}

fn decode_sources(paths: &[PathBuf], cancel: &AtomicBool) -> ToolResult<Vec<Image>> {
    let limits = limits();
    if paths.is_empty() || paths.len() > limits.max_items {
        return Err(source_count_error());
    }
    let mut images = Vec::new();
    images.try_reserve_exact(paths.len()).map_err(|_| {
        ToolError::Failed(tr!("无法分配拼接输入列表", "Unable to allocate collage inputs").into())
    })?;
    let mut pixels = 0_u64;
    for (index, path) in paths.iter().enumerate() {
        check_cancel(cancel)?;
        let remaining = limits.max_input_pixels - pixels;
        if remaining == 0 {
            return Err(ToolError::Invalid(
                tr!(
                    "拼接输入合计超过 3200 万像素",
                    "Collage inputs exceed 32 megapixels"
                )
                .into(),
            ));
        }
        let decoded =
            crate::image_decode::decode_path_with_budget_cancellable(path, remaining * 4, cancel);
        check_cancel(cancel)?;
        let decoded = decoded.map_err(|error| source_error(index, path, error))?;
        pixels += validate_input(&decoded.image, limits)?;
        check_cancel(cancel)?;
        images.push(decoded.image);
    }
    Ok(images)
}

fn preview_image(
    image: &Image,
    encoding: SaveEncoding,
    cancel: &AtomicBool,
) -> ToolResult<Arc<RenderImage>> {
    let density = (960. / f64::from(image.width))
        .min(520. / f64::from(image.height))
        .min(1.);
    let width = (f64::from(image.width) * density).round().max(1.) as u32;
    let height = (f64::from(image.height) * density).round().max(1.) as u32;
    let mut preview = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
        limits(),
        cancel,
    )?;
    if matches!(encoding.format, SaveFormat::Jpeg | SaveFormat::Bmp) {
        crate::export::flatten_on_white(&mut preview.bgra);
    }
    check_cancel(cancel)?;
    Ok(crate::canvas_source::render_image(
        width,
        height,
        preview.bgra,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_collage_view_drag_reorder_inserts_before_rows_and_at_the_end() {
        let owner = EntityId::from(11_u64);
        for (source, before, expected) in [
            (0, 2, ["b.png", "a.png", "c.png", "d.png"]),
            (2, 0, ["c.png", "a.png", "b.png", "d.png"]),
            (1, 4, ["a.png", "c.png", "d.png", "b.png"]),
            (3, 1, ["a.png", "d.png", "b.png", "c.png"]),
        ] {
            let mut paths = order_paths();
            let drag = order_drag(&paths, source);
            assert!(reorder_before(&mut paths, &drag, owner, 7, before));
            assert_eq!(paths, expected.map(PathBuf::from));
        }
    }

    #[test]
    fn image_collage_view_drag_same_position_and_invalid_boundaries_keep_order() {
        let owner = EntityId::from(11_u64);
        for (source, before) in [(0, 0), (0, 1), (1, 1), (1, 2), (3, 4)] {
            let mut paths = order_paths();
            let drag = order_drag(&paths, source);
            assert!(!reorder_before(&mut paths, &drag, owner, 7, before));
            assert_eq!(paths, order_paths());
        }
        for (source, before) in [(0, 5), (0, usize::MAX), (4, 0), (usize::MAX, 0)] {
            let mut paths = order_paths();
            let mut drag = order_drag(&paths, 0);
            drag.source_index = source;
            assert!(!reorder_before(&mut paths, &drag, owner, 7, before));
            assert_eq!(paths, order_paths());
        }
        let drag = order_drag(&order_paths(), 0);
        assert!(!reorder_before(&mut [], &drag, owner, 7, 0));
    }

    #[test]
    fn image_collage_view_drag_rejects_stale_other_owner_and_changed_source() {
        for (owner, request_id) in [(EntityId::from(12_u64), 7), (EntityId::from(11_u64), 8)] {
            let mut paths = order_paths();
            let drag = order_drag(&paths, 2);
            assert!(!reorder_before(&mut paths, &drag, owner, request_id, 0));
            assert_eq!(paths, order_paths());
        }
        let mut paths = order_paths();
        let mut drag = order_drag(&paths, 2);
        drag.source_path = PathBuf::from("replaced.png");
        assert!(!reorder_before(
            &mut paths,
            &drag,
            EntityId::from(11_u64),
            7,
            0
        ));
        assert_eq!(paths, order_paths());
    }

    fn order_paths() -> Vec<PathBuf> {
        ["a.png", "b.png", "c.png", "d.png"]
            .map(PathBuf::from)
            .to_vec()
    }

    fn order_drag(paths: &[PathBuf], source_index: usize) -> SourceReorderDrag {
        SourceReorderDrag {
            owner: EntityId::from(11_u64),
            request_id: 7,
            source_index,
            source_path: paths[source_index].clone(),
            label: "image".into(),
        }
    }

    #[test]
    fn image_collage_view_empty_initial_sources_and_item_limit_need_no_file_reads() {
        assert!(absolute_paths(Vec::new()).unwrap().is_empty());
        assert!(absolute_paths(vec![PathBuf::from("unused.png"); MAX_SOURCES + 1]).is_err());
    }

    #[test]
    fn image_collage_view_reorder_changes_only_list_order_and_checks_boundaries() {
        let mut paths = vec![
            PathBuf::from("a.png"),
            PathBuf::from("b.png"),
            PathBuf::from("c.png"),
        ];
        assert!(move_source(&mut paths, 1, -1));
        assert_eq!(
            paths,
            [
                PathBuf::from("b.png"),
                PathBuf::from("a.png"),
                PathBuf::from("c.png")
            ]
        );
        assert!(move_source(&mut paths, 0, 1));
        assert_eq!(
            paths,
            [
                PathBuf::from("a.png"),
                PathBuf::from("b.png"),
                PathBuf::from("c.png")
            ]
        );
        assert!(!move_source(&mut paths, 0, -1));
        assert!(!move_source(&mut paths, 2, 1));
        assert!(!move_source(&mut paths, usize::MAX, 1));
    }

    #[test]
    fn image_collage_view_format_preview_matches_alpha_contract_and_cancels() {
        let source = Image {
            width: 1,
            height: 1,
            bgra: vec![10, 20, 30, 128],
            readback_ms: 0.,
        };
        for format in [SaveFormat::Png, SaveFormat::Webp] {
            let preview = preview_image(
                &source,
                SaveEncoding {
                    format,
                    ..Default::default()
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(preview.as_bytes(0).unwrap(), [10, 20, 30, 128]);
        }
        for format in [SaveFormat::Jpeg, SaveFormat::Bmp] {
            let preview = preview_image(
                &source,
                SaveEncoding {
                    format,
                    ..Default::default()
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(preview.as_bytes(0).unwrap(), [132, 137, 142, 255]);
        }
        assert_eq!(source.bgra, [10, 20, 30, 128]);
        assert!(matches!(
            preview_image(&source, SaveEncoding::default(), &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
    }
}
