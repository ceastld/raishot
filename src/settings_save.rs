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
#[derive(Clone)]
enum Control {
    Apply,
    Cancel,
    Reset,
    Browse,
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
    pub(crate) date_format: Entity<TextField>,
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
        let directory = cx.new(|cx| {
            TextField::new(
                draft.directory.to_string_lossy().into_owned(),
                tr!("绝对路径", "Absolute path"),
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
        let date_format = cx.new(|cx| TextField::new(draft.date_format.clone(), "yyyyMMdd", cx));
        let quality = cx.new(|cx| QualitySlider::new(draft.encoding.jpeg_quality, cx));
        let group: Vec<_> = [&directory, &template, &date_format]
            .iter()
            .map(|field| field.downgrade())
            .collect();
        for field in [&directory, &template, &date_format] {
            field.update(cx, |v, cx| v.configure_focus(group.clone(), window, cx));
        }
        quality.update(cx, |v, _| v.focus_group = group);
        let mut subscriptions = Vec::new();
        for field in [&directory, &template, &date_format] {
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
            date_format,
            quality,
            status: String::new(),
            positions,
            _subscriptions: subscriptions,
        }
    }
    fn read_fields(&mut self, cx: &App) {
        self.draft.directory = PathBuf::from(self.directory.read(cx).text());
        self.draft.filename_template = self.template.read(cx).text().to_owned();
        self.draft.date_format = self.date_format.read(cx).text().to_owned();
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
        self.date_format
            .update(cx, |v, cx| v.set_text(self.draft.date_format.clone(), cx));
        self.quality.update(cx, |v, cx| {
            v.set_value(self.draft.encoding.jpeg_quality, cx)
        });
    }
    pub(crate) fn is_composing(&self, cx: &App) -> bool {
        [&self.directory, &self.template, &self.date_format]
            .iter()
            .any(|field| field.read(cx).is_composing())
    }
    fn apply(&mut self, cx: &mut Context<Self>) {
        if [&self.directory, &self.template, &self.date_format]
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
    fn control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
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
            Control::Format(format) => {
                self.draft.encoding.format = format;
                self.status.clear();
            }
            Control::Compression(compression) => {
                self.draft.encoding.png_compression = compression;
                self.status.clear();
            }
            Control::Quality(delta) => {
                let value = (self.draft.encoding.jpeg_quality as i16 + delta).clamp(1, 100) as u8;
                self.quality.update(cx, |v, cx| v.change(value, cx));
            }
            Control::Date(index) => {
                self.draft.date_format = DATE_FORMATS[index].into();
                self.date_format
                    .update(cx, |v, cx| v.set_text(DATE_FORMATS[index].into(), cx));
                self.status.clear();
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
                            v.draft.directory = path.clone();
                            v.directory.update(cx, |v, cx| {
                                v.set_text(path.to_string_lossy().into_owned(), cx)
                            });
                            v.status = tr!(
                                "目录已更改，点击“应用”后生效。",
                                "Folder changed; click Apply to use it."
                            )
                            .into();
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
    fn naming(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut tokens = div().flex().flex_wrap().items_center().gap_2().child(
            div()
                .text_size(px(12.))
                .text_color(rgb(palette.muted))
                .child(tr!("插入", "Insert")),
        );
        for (id, token) in [
            ("settings-save-token-date", "{date}"),
            ("settings-save-token-time", "{time}"),
            ("settings-save-token-width", "{width}"),
            ("settings-save-token-height", "{height}"),
        ] {
            tokens = tokens.child(self.bind(
                widgets::chip(id, token, false, palette, surface),
                id,
                Control::Token(token),
                cx,
            ));
        }
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
        let (preview, preview_color) = match self
            .draft
            .suggested_stem_at(1920, 1080, SaveMoment::EXAMPLE)
            .and_then(|stem| crate::save_options::file_name(&stem, self.draft.encoding.format))
        {
            Ok(name) => (name, palette.foreground),
            Err(error) => (error.to_string(), palette.accent),
        };
        let template = widgets::field_block(tr!("文件名模板", "File name template"), palette)
            .child(self.field("settings-save-template", self.template.clone()))
            .child(tokens);
        let date = widgets::field_block(tr!("日期格式", "Date format"), palette)
            .child(self.field("settings-save-date-format", self.date_format.clone()))
            .child(dates);
        let example = div()
            .px_3()
            .py_3()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "示例（1920 × 1080 截图）",
                        "Example (1920 × 1080 capture)"
                    )),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(preview_color))
                    .child(preview),
            );
        widgets::card(
            surface,
            [
                template.into_any_element(),
                date.into_any_element(),
                example.into_any_element(),
            ],
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
                    )),
            );
        // 与翻译服务一致：只有草稿和已保存的规则不同时才出现“取消修改／应用”条。
        let changed = self.draft != self.store.config;
        let apply_bar = changed.then(|| {
            div()
                .px_3()
                .py(px(10.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .rounded_md()
                .bg(gpui::rgba((palette.accent << 8) | 0x24))
                .child(div().flex_1().min_w(px(140.)).text_size(px(13.)).child(tr!(
                    "保存规则有改动，应用后下一次保存生效。",
                    "Save rule changes take effect after Apply."
                )))
                .child(self.button(
                    "settings-save-cancel",
                    tr!("取消修改", "Cancel"),
                    ButtonKind::Secondary,
                    Control::Cancel,
                    cx,
                ))
                .child(self.button(
                    "settings-save-apply",
                    tr!("应用", "Apply"),
                    ButtonKind::Primary,
                    Control::Apply,
                    cx,
                ))
        });
        let mut actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(self.button(
                "settings-save-reset",
                tr!("恢复默认", "Restore defaults"),
                ButtonKind::Secondary,
                Control::Reset,
                cx,
            ));
        if !self.status.is_empty() {
            actions = actions.child(
                div()
                    .px_2()
                    .text_size(px(12.))
                    .text_color(rgb(palette.accent))
                    .child(self.status.clone()),
            );
        }
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_6()
            .child(widgets::note(
                tr!("截图、贴图与图片编辑共用保存规则。改动后底部出现“应用”，点击后生效；切换分类不会丢失未应用的修改。", "Captures, pins and the image editor share these save rules. After a change, click \"Apply\" at the bottom to use it; switching tabs keeps unapplied changes."),
                palette,
            ))
            .child(
                widgets::section(tr!("保存位置", "Location"), palette)
                    .child(widgets::card(surface, [location.into_any_element()])),
            )
            .child(widgets::section(tr!("文件名", "File name"), palette).child(self.naming(cx)))
            .child(widgets::section(tr!("图片格式", "Image format"), palette).child(self.encoding(cx)))
            .children(apply_bar)
            .child(actions)
            .into_any_element()
    }
}
