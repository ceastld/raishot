//! 截图、贴图与图片编辑共用保存窗口；编码在后台进行，取消不写配置。
use crate::{
    capture::Image,
    export,
    save_options::{self, PngCompression, SaveConfig, SaveFormat, SaveMoment, DATE_FORMATS},
    save_store::SaveStore,
    text_field::{TextField, TextFieldEvent},
    Result,
};
use gpui::{
    div, img, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context,
    Entity, EventEmitter, FocusHandle, KeyDownEvent, MouseButton, ObjectFit, Render, RenderImage,
    Subscription, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc, sync::Arc};

pub(crate) enum SaveOutcome {
    Cancelled,
    Saved {
        path: PathBuf,
        warnings: Vec<String>,
    },
}
type Completion = Box<dyn FnOnce(SaveOutcome, &mut App)>;
#[derive(Clone)]
enum Control {
    Format(SaveFormat),
    Compression(PngCompression),
    Quality(i16),
    Date(usize),
    Token(&'static str),
    Recent(PathBuf),
    Browse,
    Remember,
    Save,
    Cancel,
    Overwrite,
    KeepBoth,
    CancelOverwrite,
}
pub(crate) struct SavePanel {
    image: Arc<Image>,
    preview: Option<Arc<RenderImage>>,
    protected_path: Option<PathBuf>,
    moment: SaveMoment,
    pub(crate) draft: SaveConfig,
    pub(crate) filename: Entity<TextField>,
    pub(crate) directory: Entity<TextField>,
    pub(crate) template: Entity<TextField>,
    pub(crate) date_format: Entity<TextField>,
    pub(crate) quality: Entity<QualitySlider>,
    recent: Vec<PathBuf>,
    pub(crate) remember: bool,
    pub(crate) manual_filename: bool,
    pub(crate) status: String,
    pub(crate) encoding_pending: bool,
    encoding_running: bool,
    encoding_generation: u64,
    pub(crate) encoded: Option<Arc<Vec<u8>>>,
    pub(crate) saving: bool,
    pending_path: Option<PathBuf>,
    completion: Option<Completion>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    pub(crate) ui_positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
}
impl SavePanel {
    pub(crate) fn open(
        image: Arc<Image>,
        protected_path: Option<PathBuf>,
        title: &str,
        on_done: impl FnOnce(SaveOutcome, &mut App) + 'static,
        cx: &mut App,
    ) -> Result<WindowHandle<Self>> {
        if image.width == 0
            || image.height == 0
            || image.bgra.len() != image.width as usize * image.height as usize * 4
        {
            return Err(tr!(
                "保存图像尺寸或像素无效",
                "The image size or pixels to save are invalid"
            )
            .into());
        }
        let title = title.to_owned();
        let dimensions = cx
            .primary_display()
            .map_or(size(px(680.), px(760.)), |display| {
                let bounds = display.bounds();
                size(
                    px(680_f32.min((f32::from(bounds.size.width) - 48.).max(240.))),
                    px(760_f32.min((f32::from(bounds.size.height) - 72.).max(260.))),
                )
            });
        let bounds = Bounds::centered(None, dimensions, cx);
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title(&title);
                crate::gpui_capture::foreground_auxiliary(window);
                let view = cx.new(|cx| {
                    let store = SaveStore::load();
                    let mut draft = store.config;
                    if let Some(directory) = store.recent_directories.first() {
                        draft.directory = directory.clone();
                    }
                    let moment = SaveMoment::now();
                    let stem = draft
                        .suggested_stem_at(image.width, image.height, moment)
                        .unwrap_or_else(|_| tr!("截图", "Screenshot").into());
                    let filename =
                        cx.new(|cx| TextField::new(stem, tr!("文件名", "File name"), cx));
                    let directory = cx.new(|cx| {
                        TextField::new(
                            draft.directory.to_string_lossy().into_owned(),
                            tr!("保存目录", "Save folder"),
                            cx,
                        )
                    });
                    let template = cx.new(|cx| {
                        TextField::new(
                            draft.filename_template.clone(),
                            crate::save_options::default_template(),
                            cx,
                        )
                    });
                    let date_format =
                        cx.new(|cx| TextField::new(draft.date_format.clone(), "yyyyMMdd", cx));
                    let quality = cx.new(|cx| QualitySlider::new(draft.encoding.jpeg_quality, cx));
                    let group: Vec<_> = [&filename, &directory, &template, &date_format]
                        .iter()
                        .map(|field| field.downgrade())
                        .collect();
                    for field in [&filename, &directory, &template, &date_format] {
                        field.update(cx, |v, cx| v.configure_focus(group.clone(), window, cx));
                    }
                    quality.update(cx, |v, _| v.focus_group = group);
                    let mut subscriptions = Vec::new();
                    for (kind, field) in [
                        (&"filename", &filename),
                        (&"directory", &directory),
                        (&"template", &template),
                        (&"date", &date_format),
                    ] {
                        let kind = *kind;
                        subscriptions.push(cx.subscribe_in(
                            field,
                            window,
                            move |v: &mut Self, _, event, window, cx| match event {
                                TextFieldEvent::Submit => v.begin_save(window, cx),
                                TextFieldEvent::Changed => {
                                    if kind == "filename" {
                                        v.manual_filename = true;
                                    }
                                    v.read_fields(cx);
                                    v.pending_path = None;
                                    v.status.clear();
                                    if matches!(kind, "template" | "date") {
                                        v.update_name(cx);
                                    }
                                    cx.notify();
                                }
                            },
                        ));
                    }
                    subscriptions.push(cx.subscribe_in(
                        &quality,
                        window,
                        |v: &mut Self, _, event: &QualityEvent, window, cx| {
                            if v.saving {
                                return;
                            }
                            v.draft.encoding.jpeg_quality = event.0;
                            v.pending_path = None;
                            if v.draft.encoding.format == SaveFormat::Jpeg {
                                v.request_encoding(window, cx);
                            }
                            cx.notify();
                        },
                    ));
                    let focus = cx.focus_handle();
                    let mut panel = Self {
                        preview: None,
                        image,
                        protected_path,
                        moment,
                        draft,
                        filename,
                        directory,
                        template,
                        date_format,
                        quality,
                        recent: store.recent_directories,
                        remember: true,
                        manual_filename: false,
                        status: String::new(),
                        encoding_pending: false,
                        encoding_running: false,
                        encoding_generation: 0,
                        encoded: None,
                        saving: false,
                        pending_path: None,
                        completion: Some(Box::new(on_done)),
                        focus,
                        _subscriptions: subscriptions,
                        ui_positions: Rc::new(RefCell::new(HashMap::new())),
                    };
                    panel.request_encoding(window, cx);
                    panel.prepare_preview(window, cx);
                    panel
                });
                window.focus(&view.read(cx).filename.read(cx).focus_handle);
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |_, cx| {
                    weak.update(cx, |v, cx| {
                        if v.saving {
                            return false;
                        }
                        v.complete(SaveOutcome::Cancelled, cx);
                        true
                    })
                    .unwrap_or(true)
                });
                view
            },
        )?)
    }
    fn read_fields(&mut self, cx: &App) {
        self.draft.directory = PathBuf::from(self.directory.read(cx).text());
        self.draft.filename_template = self.template.read(cx).text().to_owned();
        self.draft.date_format = self.date_format.read(cx).text().to_owned();
    }
    fn prepare_preview(&self, window: &mut Window, cx: &mut Context<Self>) {
        let image = self.image.clone();
        let task = cx
            .background_executor()
            .spawn(async move { thumbnail(&image) });
        cx.spawn_in(window, async move |view, cx| {
            let preview = task.await;
            let _ = view.update(cx, |v, cx| {
                v.preview = Some(preview);
                cx.notify();
            });
        })
        .detach();
    }
    fn update_name(&mut self, cx: &mut Context<Self>) {
        if self.manual_filename {
            return;
        }
        match self
            .draft
            .suggested_stem_at(self.image.width, self.image.height, self.moment)
        {
            Ok(name) => self.filename.update(cx, |v, cx| v.set_text(name, cx)),
            Err(error) => self.status = error.to_string(),
        }
    }
    /// 同时最多一个整图编码任务，滑块变动只保留最新需求，避免后台堆积整图缓存。
    fn request_encoding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.encoding_generation = self.encoding_generation.wrapping_add(1);
        self.encoding_pending = true;
        self.encoded = None;
        self.pending_path = None;
        self.status.clear();
        if !self.encoding_running {
            self.start_encoding(window, cx);
        }
        cx.notify();
    }
    fn start_encoding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.encoding_running = true;
        let generation = self.encoding_generation;
        let encoding = self.draft.encoding;
        let image = self.image.clone();
        let task = cx.background_executor().spawn(async move {
            export::encode_image(&image, encoding)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, window, cx| {
                v.encoding_running = false;
                if generation != v.encoding_generation {
                    v.start_encoding(window, cx);
                    return;
                }
                v.encoding_pending = false;
                match result {
                    Ok(bytes) => v.encoded = Some(bytes),
                    Err(error) => v.status = trf!("编码失败：{error}", "Encoding failed: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn composing(&self, cx: &App) -> bool {
        [
            &self.filename,
            &self.directory,
            &self.template,
            &self.date_format,
        ]
        .iter()
        .any(|field| field.read(cx).is_composing())
    }
    fn final_path(&mut self, cx: &App) -> Result<PathBuf> {
        self.read_fields(cx);
        self.draft.validate()?;
        let name =
            save_options::file_name(self.filename.read(cx).text(), self.draft.encoding.format)?;
        let path = self.draft.directory.join(name);
        if self
            .protected_path
            .as_ref()
            .is_some_and(|original| export::is_same_path(original, &path))
        {
            return Err(tr!(
                "请使用新的文件名，保留原始图片",
                "Use a new file name to keep the original image"
            )
            .into());
        }
        Ok(path)
    }
    fn begin_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving || self.composing(cx) {
            return;
        }
        if self.encoding_pending || self.encoded.is_none() {
            self.status = tr!(
                "图片正在编码，请稍候再保存。",
                "The image is still encoding; please wait before saving."
            )
            .into();
            cx.notify();
            return;
        }
        match self.final_path(cx) {
            Ok(path) if path.exists() => {
                self.status = tr!(
                    "已有同名文件，请选择覆盖或保留两个文件。",
                    "A file with this name exists; choose to replace it or keep both."
                )
                .into();
                self.pending_path = Some(path);
            }
            Ok(path) => self.write(path, false, window, cx),
            Err(error) => self.status = error.to_string(),
        }
        cx.notify();
    }
    fn write(
        &mut self,
        path: PathBuf,
        overwrite: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bytes) = self.encoded.clone() else {
            return;
        };
        if self
            .protected_path
            .as_ref()
            .is_some_and(|original| export::is_same_path(original, &path))
        {
            self.status = tr!(
                "请使用新的文件名，保留原始图片",
                "Use a new file name to keep the original image"
            )
            .into();
            cx.notify();
            return;
        }
        self.saving = true;
        self.pending_path = None;
        self.status = tr!("正在保存…", "Saving…").into();
        self.set_inputs_enabled(false, cx);
        let directory = self.draft.directory.clone();
        let remember = self.remember.then(|| self.draft.clone());
        let write_path = path.clone();
        // 图像和编码结果均活到写盘完成；关闭窗口不能提前发出取消。
        let image = self.image.clone();
        let task = cx.background_executor().spawn(async move {
            let _image = image;
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            export::save_encoded(&bytes, &write_path, overwrite).map_err(|e| e.to_string())?;
            let warnings = SaveStore::record_success(&directory, remember)
                .err()
                .map(|e| {
                    trf!(
                        "图片已保存，但默认设置/最近目录未更新：{e}",
                        "Image saved, but the defaults/recent folders weren't updated: {e}"
                    )
                })
                .into_iter()
                .collect();
            Ok::<Vec<String>, String>(warnings)
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, window, cx| {
                v.saving = false;
                match result {
                    Ok(warnings) => {
                        v.complete(SaveOutcome::Saved { path, warnings }, cx);
                        window.remove_window();
                    }
                    Err(error) => {
                        v.set_inputs_enabled(true, cx);
                        v.status = trf!("保存失败：{error}", "Save failed: {error}");
                        cx.notify();
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }
    fn set_inputs_enabled(&self, enabled: bool, cx: &mut Context<Self>) {
        for field in [
            &self.filename,
            &self.directory,
            &self.template,
            &self.date_format,
        ] {
            field.update(cx, |v, cx| v.set_enabled(enabled, cx));
        }
        self.quality.update(cx, |v, cx| {
            v.enabled = enabled;
            cx.notify();
        });
    }
    fn complete(&mut self, outcome: SaveOutcome, cx: &mut Context<Self>) {
        if let Some(done) = self.completion.take() {
            cx.defer(move |cx| done(outcome, cx));
        }
    }
    fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        if self.composing(cx) && !matches!(action, Control::Cancel | Control::CancelOverwrite) {
            self.status = tr!(
                "请先确认或取消输入法候选，再继续操作。",
                "Confirm or cancel the IME candidate first."
            )
            .into();
            cx.notify();
            return;
        }
        match action {
            Control::Save => self.begin_save(window, cx),
            Control::Cancel => {
                self.complete(SaveOutcome::Cancelled, cx);
                window.remove_window();
            }
            Control::Overwrite => {
                if let Some(path) = self.pending_path.take() {
                    self.write(path, true, window, cx);
                }
            }
            Control::KeepBoth => {
                if let Some(path) = self.pending_path.take() {
                    match unused_name(&path) {
                        Ok(path) => self.write(path, false, window, cx),
                        Err(error) => self.status = error.to_string(),
                    }
                }
            }
            Control::CancelOverwrite => {
                self.pending_path = None;
                self.status.clear();
            }
            Control::Remember => self.remember = !self.remember,
            Control::Browse => self.browse(window, cx),
            Control::Recent(path) => {
                self.draft.directory = path.clone();
                self.directory.update(cx, |v, cx| {
                    v.set_text(path.to_string_lossy().into_owned(), cx)
                });
                self.pending_path = None;
                self.status.clear();
            }
            Control::Format(format) => {
                if self.draft.encoding.format != format {
                    self.draft.encoding.format = format;
                    self.request_encoding(window, cx);
                }
            }
            Control::Compression(compression) => {
                if self.draft.encoding.png_compression != compression {
                    self.draft.encoding.png_compression = compression;
                    self.request_encoding(window, cx);
                }
            }
            Control::Quality(delta) => {
                let value = (self.draft.encoding.jpeg_quality as i16 + delta).clamp(1, 100) as u8;
                self.quality.update(cx, |v, cx| v.change(value, cx));
            }
            Control::Date(index) => {
                self.draft.date_format = DATE_FORMATS[index].into();
                self.date_format
                    .update(cx, |v, cx| v.set_text(DATE_FORMATS[index].into(), cx));
                self.pending_path = None;
                self.status.clear();
                self.update_name(cx);
            }
            Control::Token(token) => self
                .template
                .update(cx, |v, cx| v.insert(token, window, cx)),
        }
        cx.notify();
    }
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr!("选择保存目录", "Choose a save folder").into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let chosen = receiver.await;
            let _ = view.update(cx, |v, cx| {
                match chosen {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            v.draft.directory = path.clone();
                            v.directory.update(cx, |v, cx| {
                                v.set_text(path.to_string_lossy().into_owned(), cx)
                            });
                            v.pending_path = None;
                            v.status.clear();
                        }
                    }
                    Ok(Err(error)) => v.status = error.to_string(),
                    Err(error) => v.status = error.to_string(),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn button(
        &self,
        id: &'static str,
        label: String,
        active: bool,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        div()
            .id(id)
            .relative()
            .px_3()
            .py_2()
            .rounded_md()
            .when(!self.saving, |d| d.cursor_pointer())
            .bg(rgb(if active {
                palette.selected
            } else {
                palette.hover
            }))
            .text_color(rgb(palette.foreground))
            .opacity(if self.saving { 0.45 } else { 1. })
            .when(!self.saving, |d| d.hover(move |s| s.bg(rgb(palette.hover))))
            .child(label)
            .child(self.probe(id))
            .on_click(cx.listener(move |v, _, w, cx| v.control(action.clone(), w, cx)))
            .into_any_element()
    }
    pub(crate) fn probe(&self, id: impl Into<String>) -> AnyElement {
        probe(self.ui_positions.clone(), id.into())
    }
    fn fields(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut section = div().flex().flex_col().gap_2();
        section = section.child(form_row(
            tr!("文件名", "File name"),
            div()
                .relative()
                .child(self.filename.clone())
                .child(self.probe("save-name"))
                .into_any_element(),
        ));
        let name =
            save_options::file_name(self.filename.read(cx).text(), self.draft.encoding.format)
                .map_or_else(
                    |e| e.to_string(),
                    |name| trf!("最终文件名：{name}", "Final file name: {name}"),
                );
        section = section.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(palette.muted))
                .child(name),
        );
        section = section.child(form_row(
            tr!("保存目录", "Save folder"),
            div()
                .relative()
                .child(self.directory.clone())
                .child(self.probe("save-directory"))
                .into_any_element(),
        ));
        let mut dirs = div().flex().flex_wrap().gap_2().child(self.button(
            "save-browse",
            tr!("浏览目录…", "Browse…").into(),
            false,
            Control::Browse,
            cx,
        ));
        for (index, path) in self.recent.iter().take(12).enumerate() {
            let path = path.clone();
            let action = Control::Recent(path.clone());
            dirs = dirs.child(
                div()
                    .id(("save-recent", index))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(!self.saving, |d| d.cursor_pointer())
                    .bg(rgb(palette.hover))
                    .child(path.to_string_lossy().into_owned())
                    .on_click(cx.listener(move |v, _, w, cx| v.control(action.clone(), w, cx))),
            );
        }
        section.child(dirs).into_any_element()
    }
    fn encoding_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut formats = div().flex().flex_wrap().gap_2();
        for (id, format) in [
            ("save-format-png", SaveFormat::Png),
            ("save-format-jpeg", SaveFormat::Jpeg),
            ("save-format-bmp", SaveFormat::Bmp),
        ] {
            formats = formats.child(self.button(
                id,
                format.label().into(),
                self.draft.encoding.format == format,
                Control::Format(format),
                cx,
            ));
        }
        let mut section = div().flex().flex_col().gap_2().child(form_row(
            tr!("图片格式", "Image format"),
            formats.into_any_element(),
        ));
        if self.draft.encoding.format == SaveFormat::Jpeg {
            section = section.child(div().child(trf!(
                "JPEG 质量：{}（透明区域合成白底）",
                "JPEG quality: {} (transparent areas become white)",
                self.draft.encoding.jpeg_quality
            )));
            section = section.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(self.button(
                        "save-quality-minus",
                        "−".into(),
                        false,
                        Control::Quality(-1),
                        cx,
                    ))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .child(self.quality.clone())
                            .child(self.probe("save-quality-slider")),
                    )
                    .child(self.button(
                        "save-quality-plus",
                        "+".into(),
                        false,
                        Control::Quality(1),
                        cx,
                    )),
            );
        } else if self.draft.encoding.format == SaveFormat::Png {
            let mut compression = div().flex().flex_wrap().gap_2();
            for (id, label, value) in [
                (
                    "save-compression-fast",
                    tr!("快速", "Fast"),
                    PngCompression::Fast,
                ),
                (
                    "save-compression-balanced",
                    tr!("均衡", "Balanced"),
                    PngCompression::Balanced,
                ),
                (
                    "save-compression-small",
                    tr!("更小文件", "Smaller"),
                    PngCompression::Small,
                ),
            ] {
                compression = compression.child(self.button(
                    id,
                    label.into(),
                    self.draft.encoding.png_compression == value,
                    Control::Compression(value),
                    cx,
                ));
            }
            section = section
                .child(form_row(tr!("PNG 压缩", "PNG compression"), compression.into_any_element()))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(tr!("PNG 保留透明，压缩档位只影响速度与文件大小。", "PNG keeps transparency; the compression level only affects speed and file size.")),
                );
        } else if self.draft.encoding.format == SaveFormat::Webp {
            section = section.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "WebP 无损并保留透明，不使用 JPEG 质量设置。",
                        "WebP is lossless and keeps transparency; JPEG quality does not apply."
                    )),
            );
        } else {
            section = section.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "BMP 不压缩，透明区域合成白底。",
                        "BMP is uncompressed; transparent areas become white."
                    )),
            );
        }
        section.into_any_element()
    }
    fn naming_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut tokens = div().flex().flex_wrap().gap_2();
        for (id, token) in [
            ("save-token-date", "{date}"),
            ("save-token-time", "{time}"),
            ("save-token-width", "{width}"),
            ("save-token-height", "{height}"),
        ] {
            tokens = tokens.child(self.button(id, token.into(), false, Control::Token(token), cx));
        }
        let mut dates = div().flex().flex_wrap().gap_2();
        for (index, date) in DATE_FORMATS.iter().enumerate() {
            dates = dates.child(
                div()
                    .id(("save-date", index))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(!self.saving, |d| d.cursor_pointer())
                    .bg(rgb(if self.draft.date_format == *date {
                        palette.selected
                    } else {
                        palette.hover
                    }))
                    .child(*date)
                    .on_click(
                        cx.listener(move |v, _, w, cx| v.control(Control::Date(index), w, cx)),
                    ),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(form_row(
                tr!("命名模板", "Name template"),
                div()
                    .relative()
                    .child(self.template.clone())
                    .child(self.probe("save-template"))
                    .into_any_element(),
            ))
            .child(tokens)
            .child(form_row(
                tr!("日期格式", "Date format"),
                div()
                    .relative()
                    .child(self.date_format.clone())
                    .child(self.probe("save-date-format"))
                    .into_any_element(),
            ))
            .child(dates)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(tr!("插入占位符生成名称；手改文件名后，模板变化不会覆盖当前名称。", "Insert placeholders to build the name; once you edit the file name by hand, template changes won't overwrite it.")),
            )
            .into_any_element()
    }
    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut footer = div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .border_t_1()
            .border_color(rgb(palette.border));
        if self.pending_path.is_some() {
            footer = footer.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(self.button(
                        "save-overwrite",
                        tr!("覆盖现有文件", "Replace existing file").into(),
                        false,
                        Control::Overwrite,
                        cx,
                    ))
                    .child(self.button(
                        "save-keep-both",
                        tr!("保留两个文件", "Keep both files").into(),
                        true,
                        Control::KeepBoth,
                        cx,
                    ))
                    .child(self.button(
                        "save-overwrite-cancel",
                        tr!("取消覆盖", "Don't replace").into(),
                        false,
                        Control::CancelOverwrite,
                        cx,
                    )),
            );
        }
        footer
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.accent))
                    .child(self.status.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(self.button(
                        "save-remember",
                        trf!(
                            "{} 记住本次设置",
                            "{} Remember these settings",
                            if self.remember { "☑" } else { "☐" }
                        ),
                        self.remember,
                        Control::Remember,
                        cx,
                    ))
                    .child(div().flex_1())
                    .child(self.button(
                        "save-cancel",
                        tr!("取消", "Cancel").into(),
                        false,
                        Control::Cancel,
                        cx,
                    ))
                    .child(
                        self.button(
                            "save-submit",
                            if self.saving {
                                tr!("正在保存…", "Saving…")
                            } else if self.encoding_pending {
                                tr!("正在编码…", "Encoding…")
                            } else {
                                tr!("保存", "Save")
                            }
                            .into(),
                            true,
                            Control::Save,
                            cx,
                        ),
                    ),
            )
            .into_any_element()
    }
}
impl Render for SavePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        let palette = crate::theme::palette(cx);
        self.ui_positions.borrow_mut().clear();
        let bytes = self.encoded.as_ref().map_or_else(
            || {
                if self.encoding_pending {
                    tr!("正在估算文件大小…", "Estimating file size…").into()
                } else {
                    tr!("编码不可用", "Encoding unavailable").into()
                }
            },
            |bytes| trf!("文件大小：{}", "File size: {}", byte_label(bytes.len())),
        );
        let mut preview = div()
            .w_full()
            .h(px(160.))
            .flex_shrink_0()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(0x273039));
        preview = match &self.preview {
            Some(image) => preview.child(
                img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            ),
            None => preview.child(
                div()
                    .text_color(rgb(0xd9e2e9))
                    .child(tr!("正在生成预览…", "Generating preview…")),
            ),
        };
        let body = div()
            .id("save-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .text_size(px(21.))
                    .child(tr!("保存图片", "Save image")),
            )
            .child(preview)
            .child(div().text_color(rgb(palette.muted)).child(trf!(
                "{} × {} 像素 · {bytes}",
                "{} × {} pixels · {bytes}",
                self.image.width,
                self.image.height
            )))
            .child(self.fields(cx))
            .child(self.encoding_controls(cx))
            .child(self.naming_controls(cx));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .font_family("Microsoft YaHei UI")
            .text_size(px(14.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, e: &KeyDownEvent, w, cx| {
                if v.saving {
                    cx.stop_propagation();
                    return;
                }
                if e.keystroke.key == "escape"
                    && !e.keystroke.modifiers.modified()
                    && !v.composing(cx)
                {
                    // 长按只消费；覆盖确认打开时 Esc 只收起确认，不关闭保存窗口。
                    if !e.is_held {
                        let action = if v.pending_path.is_some() {
                            Control::CancelOverwrite
                        } else {
                            Control::Cancel
                        };
                        v.control(action, w, cx);
                    }
                    cx.stop_propagation();
                }
                if e.keystroke.key == "s" && e.keystroke.modifiers.control {
                    v.begin_save(w, cx);
                    cx.stop_propagation();
                }
            }))
            .child(body)
            .child(self.footer(cx))
            .into_any_element()
    }
}
pub(crate) fn form_row(label: &str, body: AnyElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(12.)).child(label.to_owned()))
        .child(body)
        .into_any_element()
}
pub(crate) fn probe(positions: Rc<RefCell<HashMap<String, (f32, f32)>>>, id: String) -> AnyElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, _, _| {
            positions.borrow_mut().insert(
                id.clone(),
                (
                    f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.,
                    f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.,
                ),
            );
        },
    )
    .absolute()
    .left(px(0.))
    .top(px(0.))
    .size_full()
    .into_any_element()
}
fn byte_label(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    }
}
fn unused_name(path: &std::path::Path) -> Result<PathBuf> {
    let stem = path
        .file_stem()
        .ok_or(tr!("文件名无效", "Invalid file name"))?
        .to_string_lossy();
    let extension = path
        .extension()
        .ok_or(tr!("文件扩展名无效", "Invalid file extension"))?
        .to_string_lossy();
    for index in 2..=10_000 {
        let candidate = path.with_file_name(format!("{stem} ({index}).{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(tr!(
        "同名文件太多，请更改文件名",
        "Too many files with this name; change the file name"
    )
    .into())
}
fn thumbnail(image: &Image) -> Arc<RenderImage> {
    let scale = (960. / image.width as f64)
        .min(600. / image.height as f64)
        .min(1.);
    let width = (image.width as f64 * scale).round().max(1.) as u32;
    let height = (image.height as f64 * scale).round().max(1.) as u32;
    let mut bytes = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let sx = ((x as f64 + 0.5) * image.width as f64 / width as f64 - 0.5)
                .clamp(0., image.width as f64 - 1.);
            let sy = ((y as f64 + 0.5) * image.height as f64 / height as f64 - 0.5)
                .clamp(0., image.height as f64 - 1.);
            let (x0, y0) = (sx.floor() as u32, sy.floor() as u32);
            let (x1, y1) = (
                (x0 + 1).min(image.width - 1),
                (y0 + 1).min(image.height - 1),
            );
            let (fx, fy) = ((sx - x0 as f64) as f32, (sy - y0 as f64) as f32);
            for c in 0..4 {
                let pixel = |x, y| image.bgra[((y * image.width + x) * 4) as usize + c] as f32;
                bytes[((y * width + x) * 4) as usize + c] =
                    ((pixel(x0, y0) * (1. - fx) + pixel(x1, y0) * fx) * (1. - fy)
                        + (pixel(x0, y1) * (1. - fx) + pixel(x1, y1) * fx) * fy)
                        .round() as u8;
            }
        }
    }
    let buffer = image::RgbaImage::from_raw(width, height, bytes).expect("bounded thumbnail");
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}
pub(crate) struct QualityEvent(pub u8);
pub(crate) struct QualitySlider {
    value: u8,
    focus: FocusHandle,
    dragging: bool,
    pub(crate) enabled: bool,
    pub(crate) focus_group: Vec<gpui::WeakEntity<TextField>>,
    bounds: Rc<RefCell<Option<Bounds<gpui::Pixels>>>>,
}
impl EventEmitter<QualityEvent> for QualitySlider {}
impl QualitySlider {
    pub(crate) fn new(value: u8, cx: &mut Context<Self>) -> Self {
        Self {
            value,
            focus: cx.focus_handle(),
            dragging: false,
            enabled: true,
            focus_group: Vec::new(),
            bounds: Rc::new(RefCell::new(None)),
        }
    }
    pub(crate) fn set_value(&mut self, value: u8, cx: &mut Context<Self>) {
        self.value = value.clamp(1, 100);
        cx.notify();
    }
    pub(crate) fn change(&mut self, value: u8, cx: &mut Context<Self>) {
        if self.enabled && self.value != value {
            self.value = value.clamp(1, 100);
            cx.emit(QualityEvent(self.value));
            cx.notify();
        }
    }
    fn at(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let bounds = *self.bounds.borrow();
        if let Some(bounds) = bounds {
            let fraction =
                f32::from(position.x - bounds.left()) / f32::from(bounds.size.width).max(1.);
            self.change((1. + fraction.clamp(0., 1.) * 99.).round() as u8, cx);
        }
    }
}
impl Render for QualitySlider {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let bounds = self.bounds.clone();
        let fraction = (self.value - 1) as f32 / 99.;
        div()
            .w_full()
            .h(px(32.))
            .cursor_pointer()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, e: &gpui::MouseDownEvent, w, cx| {
                    if v.enabled
                        && !v
                            .focus_group
                            .iter()
                            .filter_map(|field| field.upgrade())
                            .any(|field| field.read(cx).is_composing())
                    {
                        w.focus(&v.focus);
                        v.dragging = true;
                        v.at(e.position, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, _, cx| {
                if v.dragging {
                    v.at(e.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|v, _, _, _| v.dragging = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|v, _, _, _| v.dragging = false),
            )
            .on_key_down(cx.listener(|v, e: &KeyDownEvent, _, cx| {
                let value = match e.keystroke.key.as_str() {
                    "left" | "down" => v.value.saturating_sub(1).max(1),
                    "right" | "up" => v.value.saturating_add(1).min(100),
                    "home" => 1,
                    "end" => 100,
                    _ => return,
                };
                v.change(value, cx);
                cx.stop_propagation();
            }))
            .child(
                gpui::canvas(
                    |_, _, _| (),
                    move |rect, _, w, _| {
                        *bounds.borrow_mut() = Some(rect);
                        let track = Bounds::new(
                            point(rect.left(), rect.top() + rect.size.height / 2. - px(2.)),
                            size(rect.size.width, px(4.)),
                        );
                        w.paint_quad(gpui::fill(track, rgb(palette.border)));
                        w.paint_quad(gpui::fill(
                            Bounds::new(
                                track.origin,
                                size(track.size.width * fraction, track.size.height),
                            ),
                            rgb(palette.accent),
                        ));
                        let x = (rect.size.width - px(8.)) * fraction;
                        w.paint_quad(gpui::fill(
                            Bounds::new(
                                point(rect.left() + x, rect.top() + rect.size.height / 2. - px(6.)),
                                size(px(8.), px(12.)),
                            ),
                            rgb(palette.accent),
                        ));
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}
