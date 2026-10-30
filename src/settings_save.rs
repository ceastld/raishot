//! 独立保存规则草稿；应用只提交修改字段，不写热键/工具栏或最近目录。
use crate::{
    save_options::{PngCompression, SaveConfig, SaveFormat, SaveMoment, DATE_FORMATS},
    save_panel::{probe, QualityEvent, QualitySlider},
    save_store::SaveStore,
    settings_widgets::{self as widgets, ButtonKind},
    text_field::{TextField, TextFieldEvent},
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, AppContext, Context, Div, Entity, Render, Stateful,
    Subscription, Window,
};
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};
/// Apply／Cancel／Reset 也由设置窗口底部的应用条和页眉按钮调用。
#[derive(Clone)]
pub(crate) enum Control {
    Apply,
    Cancel,
    Reset,
    Browse,
    OpenFolder,
    Format(SaveFormat),
    Compression(PngCompression),
    Quality(i16),
    Date(usize),
    Token(&'static str),
}
pub(crate) struct SaveSettings {
    store: SaveStore,
    pub(crate) draft: SaveConfig,
    pub(crate) directory: Entity<TextField>,
    pub(crate) template: Entity<TextField>,
    quality: Entity<QualitySlider>,
    pub(crate) status: String,
    positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
    _subscriptions: Vec<Subscription>,
}
impl SaveSettings {
    pub(crate) fn new(
        window: &mut Window,
        positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::from_store(SaveStore::load(), window, positions, cx)
    }

    #[cfg(test)]
    pub(crate) fn new_in_memory(
        window: &mut Window,
        positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::from_store(SaveStore::default(), window, positions, cx)
    }

    fn from_store(
        store: SaveStore,
        window: &mut Window,
        positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = store.config.clone();
        // 设置字段：输入法取消候选送来的空替换不会清掉原文，窗口未激活或未聚焦时不接收原生输入。
        let directory = cx.new(|cx| {
            TextField::new_settings(
                draft.directory.to_string_lossy().into_owned(),
                crate::i18n::Text("绝对路径", "Absolute path"),
                cx,
            )
        });
        let template = cx.new(|cx| {
            TextField::new_settings(
                draft.filename_template.clone(),
                crate::i18n::Text(
                    "截图_{date}_{time}_{width}x{height}",
                    "Screenshot_{date}_{time}_{width}x{height}",
                ),
                cx,
            )
        });
        let quality = cx.new(|cx| QualitySlider::new(draft.encoding.jpeg_quality, cx));
        let group: Vec<_> = [&directory, &template]
            .iter()
            .map(|field| field.downgrade())
            .collect();
        for field in [&directory, &template] {
            field.update(cx, |v, cx| v.configure_focus(group.clone(), window, cx));
        }
        quality.update(cx, |v, _| v.focus_group = group);
        let mut subscriptions = Vec::new();
        for field in [&directory, &template] {
            subscriptions.push(
                cx.subscribe_in(field, window, |v: &mut Self, _, event, _, cx| match event {
                    TextFieldEvent::Submit => v.apply(cx),
                    TextFieldEvent::Changed => {
                        v.read_fields(cx);
                        v.status.clear();
                        cx.notify();
                    }
                }),
            );
        }
        subscriptions.push(
            cx.subscribe(&quality, |v: &mut Self, _, event: &QualityEvent, cx| {
                v.draft.encoding.jpeg_quality = event.0;
                v.status.clear();
                cx.notify();
            }),
        );
        Self {
            store,
            draft,
            directory,
            template,
            quality,
            status: String::new(),
            positions,
            _subscriptions: subscriptions,
        }
    }
    fn read_fields(&mut self, cx: &App) {
        self.draft.directory = PathBuf::from(self.directory.read(cx).text());
        self.draft.filename_template = self.template.read(cx).text().to_owned();
    }
    /// 把草稿和三个输入框、质量滑块一起换成给定配置。
    fn load_draft(&mut self, config: SaveConfig, cx: &mut Context<Self>) {
        self.draft = config;
        self.directory.update(cx, |v, cx| {
            v.set_text(self.draft.directory.to_string_lossy().into_owned(), cx)
        });
        self.template.update(cx, |v, cx| {
            v.set_text(self.draft.filename_template.clone(), cx)
        });
        self.quality.update(cx, |v, cx| {
            v.set_value(self.draft.encoding.jpeg_quality, cx)
        });
    }
    pub(crate) fn is_composing(&self, cx: &App) -> bool {
        [&self.directory, &self.template]
            .iter()
            .any(|field| field.read(cx).is_composing())
    }
    fn apply(&mut self, cx: &mut Context<Self>) {
        if [&self.directory, &self.template]
            .iter()
            .any(|field| field.read(cx).is_composing())
        {
            return;
        }
        self.read_fields(cx);
        if let Err(error) = self.draft.validate() {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        let previous = self.store.config.clone();
        self.store.config = self.draft.clone();
        self.status = match self.store.save() {
            Ok(()) => tr!(
                "保存规则已更新，下一次保存和快速保存使用这些设置。",
                "Save rules updated; the next save and quick save use them."
            )
            .into(),
            Err(error) => {
                self.store.config = previous;
                trf!(
                    "设置未保存，请重试：{error}",
                    "Settings not saved, please retry: {error}"
                )
            }
        };
        cx.notify();
    }
    /// 选择类设置点了就生效：草稿与已保存规则同时修改，草稿里其它未应用的输入保持不变。
    fn commit(&mut self, change: impl Fn(&mut SaveConfig)) {
        change(&mut self.draft);
        let previous = self.store.config.clone();
        change(&mut self.store.config);
        self.status = match self.store.save() {
            Ok(()) => tr!("已保存。", "Saved.").into(),
            Err(error) => {
                self.store.config = previous;
                trf!(
                    "设置未保存，请重试：{error}",
                    "Settings not saved, please retry: {error}"
                )
            }
        };
    }
    /// 草稿与已保存规则不同（有未应用的输入）。
    pub(crate) fn has_changes(&self) -> bool {
        self.draft != self.store.config
    }
    /// 草稿已是默认规则，页眉“恢复默认”无需再点。
    pub(crate) fn draft_is_default(&self) -> bool {
        self.draft == SaveConfig::default()
    }
    pub(crate) fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_composing(cx) {
            self.status = tr!(
                "请先确认或取消输入法候选，再继续操作。",
                "Confirm or cancel the IME candidate first."
            )
            .into();
            cx.notify();
            return;
        }
        match action {
            Control::Apply => self.apply(cx),
            Control::Cancel => {
                self.load_draft(self.store.config.clone(), cx);
                self.status = tr!(
                    "已取消修改，保留已保存的保存规则。",
                    "Changes canceled; the saved rules are kept."
                )
                .into();
            }
            Control::Reset => {
                self.load_draft(SaveConfig::default(), cx);
                self.status = tr!(
                    "已填入默认值，点击“应用”后生效。",
                    "Defaults filled in; click Apply to use them."
                )
                .into();
            }
            Control::Browse => self.browse(window, cx),
            Control::OpenFolder => {
                let directory = self.store.config.directory.clone();
                if !directory.is_dir() {
                    self.status = tr!(
                        "保存目录还不存在，第一次保存时会自动创建。",
                        "The save folder doesn't exist yet; it is created on the first save."
                    )
                    .into();
                } else if let Err(error) = std::process::Command::new("explorer.exe")
                    .arg(&directory)
                    .spawn()
                {
                    self.status = error.to_string();
                }
            }
            Control::Format(format) => self.commit(|config| config.encoding.format = format),
            Control::Compression(compression) => {
                self.commit(|config| config.encoding.png_compression = compression)
            }
            Control::Quality(delta) => {
                let value = (self.draft.encoding.jpeg_quality as i16 + delta).clamp(1, 100) as u8;
                self.quality.update(cx, |v, cx| v.change(value, cx));
            }
            Control::Date(index) => {
                self.commit(|config| config.date_format = DATE_FORMATS[index].into())
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
            prompt: Some(tr!("选择默认保存目录", "Choose the default save folder").into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let chosen = receiver.await;
            let _ = view.update(cx, |v, cx| {
                match chosen {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.first() {
                            // 用对话框选的目录是明确的选择，直接生效。
                            let path = path.clone();
                            v.directory.update(cx, |v, cx| {
                                v.set_text(path.to_string_lossy().into_owned(), cx)
                            });
                            v.commit(|config| config.directory = path.clone());
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
    /// 为按钮、标签或分段挂接定位探针与草稿操作。
    fn bind(
        &self,
        control: Stateful<Div>,
        id: &'static str,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        control
            .child(probe(self.positions.clone(), id.into()))
            .on_click(cx.listener(move |v, _, w, cx| v.control(action.clone(), w, cx)))
            .into_any_element()
    }
    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        kind: ButtonKind,
        action: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        self.bind(
            widgets::button(id, label, kind, palette, surface),
            id,
            action,
            cx,
        )
    }
    fn field(&self, id: &'static str, field: Entity<TextField>) -> AnyElement {
        div()
            .relative()
            .w_full()
            .child(field)
            .child(probe(self.positions.clone(), id.into()))
            .into_any_element()
    }
    /// 文件名：一个模板输入框，下方即时示例与可插入的变量；日期格式只能从预设中选，点了即生效。
    fn naming(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut tokens = div().flex().flex_wrap().items_center().gap_2().child(
            div()
                .text_size(px(12.))
                .text_color(rgb(palette.muted))
                .child(tr!("插入", "Insert")),
        );
        for (id, token, label) in [
            ("settings-save-token-date", "{date}", tr!("日期", "Date")),
            ("settings-save-token-time", "{time}", tr!("时间", "Time")),
            ("settings-save-token-width", "{width}", tr!("宽度", "Width")),
            (
                "settings-save-token-height",
                "{height}",
                tr!("高度", "Height"),
            ),
        ] {
            tokens = tokens.child(self.bind(
                widgets::chip(id, label, false, palette, surface),
                id,
                Control::Token(token),
                cx,
            ));
        }
        let (preview, preview_color) = match self
            .draft
            .suggested_stem_at(1920, 1080, SaveMoment::EXAMPLE)
            .and_then(|stem| crate::save_options::file_name(&stem, self.draft.encoding.format))
        {
            Ok(name) => (trf!("示例：{name}", "Example: {name}"), palette.muted),
            Err(error) => (error.to_string(), palette.accent),
        };
        let template = widgets::field_block(tr!("文件名模板", "File name template"), palette)
            .child(self.field("settings-save-template", self.template.clone()))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(preview_color))
                    .child(preview),
            )
            .child(tokens);
        let mut dates = div().flex().flex_wrap().gap_2();
        for (index, date) in DATE_FORMATS.iter().enumerate() {
            dates = dates.child(
                widgets::chip(
                    ("settings-save-date", index),
                    *date,
                    self.draft.date_format == *date,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |v, _, w, cx| v.control(Control::Date(index), w, cx))),
            );
        }
        let date = widgets::field_block(tr!("日期格式", "Date format"), palette).child(dates);
        widgets::card(
            surface,
            [template.into_any_element(), date.into_any_element()],
        )
        .into_any_element()
    }
    fn encoding(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut formats = widgets::segmented(surface);
        for (id, value) in [
            ("settings-save-format-png", SaveFormat::Png),
            ("settings-save-format-jpeg", SaveFormat::Jpeg),
            ("settings-save-format-bmp", SaveFormat::Bmp),
        ] {
            formats = formats.child(self.bind(
                widgets::segment(
                    id,
                    value.label(),
                    self.draft.encoding.format == value,
                    palette,
                    surface,
                ),
                id,
                Control::Format(value),
                cx,
            ));
        }
        let mut rows = vec![widgets::row(
            tr!("默认格式", "Default format"),
            Some(
                if self.draft.encoding.format == SaveFormat::Webp {
                    tr!(
                        "WebP 无损并保留透明。",
                        "WebP is lossless and keeps transparency."
                    )
                } else {
                    tr!(
                        "PNG 无损；JPEG 文件更小；BMP 不压缩。",
                        "PNG is lossless; JPEG is smaller; BMP is uncompressed."
                    )
                }
                .into(),
            ),
            palette,
        )
        .child(formats)
        .into_any_element()];
        if self.draft.encoding.format == SaveFormat::Jpeg {
            let step =
                |id: &'static str, label: &'static str, delta: i16, cx: &mut Context<Self>| {
                    self.bind(
                        widgets::button(id, label, ButtonKind::Secondary, palette, surface)
                            .w(px(32.))
                            .px(px(0.)),
                        id,
                        Control::Quality(delta),
                        cx,
                    )
                };
            rows.push(
                widgets::field_block(
                    trf!(
                        "JPEG 质量：{}",
                        "JPEG quality: {}",
                        self.draft.encoding.jpeg_quality
                    ),
                    palette,
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(step("settings-save-quality-minus", "−", -1, cx))
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_w(px(0.))
                                .child(self.quality.clone())
                                .child(probe(
                                    self.positions.clone(),
                                    "settings-save-quality-slider".into(),
                                )),
                        )
                        .child(step("settings-save-quality-plus", "+", 1, cx)),
                )
                .into_any_element(),
            );
        } else if self.draft.encoding.format == SaveFormat::Png {
            let mut compression = widgets::segmented(surface);
            for (id, label, value) in [
                (
                    "settings-save-compression-fast",
                    tr!("快速", "Fast"),
                    PngCompression::Fast,
                ),
                (
                    "settings-save-compression-balanced",
                    tr!("均衡", "Balanced"),
                    PngCompression::Balanced,
                ),
                (
                    "settings-save-compression-small",
                    tr!("更小文件", "Smaller"),
                    PngCompression::Small,
                ),
            ] {
                compression = compression.child(self.bind(
                    widgets::segment(
                        id,
                        label,
                        self.draft.encoding.png_compression == value,
                        palette,
                        surface,
                    ),
                    id,
                    Control::Compression(value),
                    cx,
                ));
            }
            rows.push(
                widgets::row(
                    tr!("PNG 压缩", "PNG compression"),
                    Some(
                        tr!(
                            "文件越小，保存耗时越长；画质相同。",
                            "Smaller files take longer to save; quality is identical."
                        )
                        .into(),
                    ),
                    palette,
                )
                .child(compression)
                .into_any_element(),
            );
        }
        widgets::card(surface, rows).into_any_element()
    }
}
impl Render for SaveSettings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let location = widgets::field_block(tr!("默认保存目录", "Default save folder"), palette)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.field("settings-save-directory", self.directory.clone())),
                    )
                    .child(self.button(
                        "settings-save-browse",
                        tr!("浏览…", "Browse…"),
                        ButtonKind::Secondary,
                        Control::Browse,
                        cx,
                    ))
                    .child(self.button(
                        "settings-save-open-folder",
                        tr!("打开文件夹", "Open folder"),
                        ButtonKind::Secondary,
                        Control::OpenFolder,
                        cx,
                    )),
            );
        // “应用／取消修改”条与“恢复默认”由设置窗口底部和页眉提供，切到别的分类也看得到。
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_6()
            .child(widgets::note(
                tr!("截图、贴图与图片编辑共用这些规则。选项点了即生效；手动改了文件夹或文件名后，在窗口底部点“应用”。", "Captures, pins and the image editor share these rules. Choices apply at once; after typing a folder or file name, click \"Apply\" at the bottom of the window."),
                palette,
            ))
            .child(
                widgets::section(tr!("保存位置", "Location"), palette)
                    .child(widgets::card(surface, [location.into_any_element()])),
            )
            .child(widgets::section(tr!("文件名", "File name"), palette).child(self.naming(cx)))
            .child(widgets::section(tr!("图片格式", "Image format"), palette).child(self.encoding(cx)))
            .into_any_element()
    }
}
