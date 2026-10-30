//! 公式与 Markdown 结果直接停靠截图界面；API 请求只使用发起时的选区快照。
use super::{text_result::OcrTarget, CaptureView};
use crate::{
    capture::Image,
    capture_toolbar::Palette,
    markdown_document::{Block, BlockKind, Document, Inline},
    markdown_view::Prepared,
    recognition::task::CancelOwner,
    text_field::{TextField, TextFieldEvent},
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, AppContext, Context, Entity, Focusable, MouseButton,
    RenderImage, Subscription, Task, Window,
};
use std::{sync::Arc, time::Duration};

const FONT_SIZE: f32 = 17.;

#[derive(Default)]
pub(crate) struct FormulaPanel {
    session: Option<Session>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
}

struct Session {
    identity: Arc<()>,
    image: Arc<Image>,
    target: OcrTarget,
    source: Entity<TextField>,
    _subscription: Subscription,
    owner: Option<CancelOwner>,
    revision: u64,
    task: Option<Task<()>>,
    preview: Option<Prepared>,
    output: Option<Image>,
    latex: Option<String>,
    validation: Option<(String, String)>,
    dark: bool,
    source_mode: bool,
    note: String,
    failed: bool,
}

/// 最终译文的本机排版缓存；流式片段不反复触发数学排版。
#[derive(Default)]
pub(crate) struct MarkdownTranslation {
    key: Option<(String, bool)>,
    preview: Option<Prepared>,
    task: Option<Task<()>>,
    retiring: Vec<Arc<RenderImage>>,
    retired: Vec<Arc<RenderImage>>,
}

fn retire(preview: &mut Option<Prepared>, retiring: &mut Vec<Arc<RenderImage>>) {
    if let Some(preview) = preview.take() {
        retiring.extend(preview.images());
    }
}

impl FormulaPanel {
    pub(crate) fn close(&mut self) -> bool {
        if let Some(mut session) = self.session.take() {
            retire(&mut session.preview, &mut self.retiring);
            // CancelOwner 随 session 析构，后台回执还必须通过 identity 检查。
            true
        } else {
            false
        }
    }
}

impl CaptureView {
    pub(crate) fn recognize_formula(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.processing
            || self.saving
            || self.pin_handoff_pending
            || !self.finish_content_or_status()
            || self.result_editor_composing()
        {
            cx.notify();
            return;
        }
        self.editor.apply_lasso();
        let image = self
            .output_rect()
            .and_then(|rect| self.recognition_content_image(rect));
        let (image, target) = match (image, self.current_ocr_target()) {
            (Ok(image), Some(target)) => (image, target),
            (Err(error), _) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
            _ => return,
        };
        self.dismiss_recognition_result();
        self.formula_panel.close();
        let source = cx.new(|cx| TextField::new_multiline(String::new(), "Markdown / LaTeX", cx));
        source.update(cx, |field, cx| {
            field.configure_focus(Vec::new(), window, cx)
        });
        let subscription = cx.subscribe_in(&source, window, |view, _, event, window, cx| {
            if !matches!(event, TextFieldEvent::Changed | TextFieldEvent::Submit) {
                return;
            }
            let Some(session) = &mut view.formula_panel.session else {
                return;
            };
            let field = session.source.read(cx);
            if matches!(event, TextFieldEvent::Changed)
                && !field.is_composing()
                && session
                    .validation
                    .as_ref()
                    .is_some_and(|(source, _)| source != field.text())
            {
                session.validation = None;
            }
            view.schedule_formula_preview(window, cx);
        });
        self.formula_panel.session = Some(Session {
            identity: Arc::new(()),
            image: Arc::new(image),
            target,
            source,
            _subscription: subscription,
            owner: None,
            revision: 0,
            task: None,
            preview: None,
            output: None,
            latex: None,
            validation: None,
            dark: !crate::theme::is_light(cx),
            source_mode: false,
            note: String::new(),
            failed: false,
        });
        window.focus(&self.focus);
        self.request_formula(window, cx);
    }

    fn request_formula(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.current_ocr_target();
        let Some(session) = &mut self.formula_panel.session else {
            return;
        };
        if current.as_ref() != Some(&session.target) {
            return;
        }
        if session.owner.is_some() || session.source.read(cx).is_composing() {
            return;
        }
        let Some(service) =
            crate::translate::Service::for_formula(&crate::preferences::Preferences::load())
        else {
            session.note = tr!(
                "公式识别已关闭；请在 设置 ▸ 翻译 中选择服务",
                "Formula recognition is off; choose a service in Settings ▸ Translation"
            )
            .into();
            session.failed = true;
            cx.notify();
            return;
        };
        if let Some(error) = service.configuration_error() {
            session.note = error;
            session.failed = true;
            cx.notify();
            return;
        }
        session.task = None;
        session.revision = session.revision.wrapping_add(1);
        retire(&mut session.preview, &mut self.formula_panel.retiring);
        session.output = None;
        session.latex = None;
        session.validation = None;
        session.note = tr!(
            "正在识别选区… Esc 取消",
            "Recognizing selection… Esc cancels"
        )
        .into();
        session.failed = false;
        session
            .source
            .update(cx, |field, cx| field.set_enabled(false, cx));
        let identity = session.identity.clone();
        let image = session.image.clone();
        let owner = CancelOwner::new();
        let token = owner.token();
        session.owner = Some(owner);
        let (sender, receiver) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let result =
                crate::formula::recognize(&service, &image, &token).map_err(|e| e.to_string());
            let _ = sender.send(result);
        });
        cx.spawn_in(window, async move |view, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            let _ = view.update_in(cx, |view, window, cx| {
                let current = view.current_ocr_target();
                let Some(session) = &mut view.formula_panel.session else {
                    return;
                };
                if !Arc::ptr_eq(&session.identity, &identity)
                    || session.owner.is_none()
                    || current.as_ref() != Some(&session.target)
                {
                    return;
                }
                session.owner = None;
                session
                    .source
                    .update(cx, |field, cx| field.set_enabled(true, cx));
                match result {
                    Ok(result) => {
                        let source = if result.latex.is_empty() {
                            result.raw_output
                        } else {
                            result.latex
                        };
                        session.validation =
                            result.validation_error.map(|error| (source.clone(), error));
                        session
                            .source
                            .update(cx, |field, cx| field.set_text(source, cx));
                        view.schedule_formula_preview(window, cx);
                    }
                    Err(error) => {
                        session.note = error;
                        session.failed = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn schedule_formula_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &mut self.formula_panel.session else {
            return;
        };
        session.task = None;
        session.revision = session.revision.wrapping_add(1);
        retire(&mut session.preview, &mut self.formula_panel.retiring);
        session.output = None;
        session.latex = None;
        if session.source.read(cx).is_composing() {
            return;
        }
        let source = session.source.read(cx).text().to_owned();
        if source.trim().is_empty() {
            session.note.clear();
            cx.notify();
            return;
        }
        let revision = session.revision;
        let identity = session.identity.clone();
        let dark = !crate::theme::is_light(cx);
        session.dark = dark;
        session.note = tr!("正在本机排版…", "Typesetting locally…").into();
        session.failed = false;
        session.task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let result = cx
                .background_executor()
                .spawn(async move {
                    let document = crate::markdown_document::parse_with_mode(&source, true)?;
                    let preview = crate::markdown_view::prepare(&document, dark, FONT_SIZE);
                    let latex = document_latex(&document);
                    let output = if preview.diagnostics.is_empty() {
                        crate::markdown_export::export_image(&document, 720, 26.).map(Some)
                    } else {
                        Ok(None)
                    };
                    Ok::<_, String>((preview, latex, output))
                })
                .await;
            let _ = view.update_in(cx, |view, _, cx| {
                let Some(session) = &mut view.formula_panel.session else {
                    return;
                };
                if !Arc::ptr_eq(&session.identity, &identity) || session.revision != revision {
                    return;
                }
                match result {
                    Ok((preview, latex, output)) => {
                        session.failed =
                            !preview.diagnostics.is_empty() || session.validation.is_some();
                        session.note = session
                            .validation
                            .as_ref()
                            .map(|(_, error)| error.clone())
                            .or_else(|| preview.diagnostics.first().cloned())
                            .unwrap_or_else(|| {
                                tr!(
                                    "编辑源码只在本机排版；复制前请核对公式",
                                    "Edits stay local; check formulas before copying"
                                )
                                .into()
                            });
                        session.preview = Some(preview);
                        session.latex = latex;
                        match output {
                            Ok(image) => session.output = image,
                            Err(error) => {
                                session.note = error;
                                session.failed = true;
                            }
                        }
                    }
                    Err(error) => {
                        session.note = error;
                        session.failed = true;
                        session.source_mode = true;
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(crate) fn formula_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = &self.formula_panel.session else {
            return false;
        };
        if session.source.read(cx).is_composing() {
            cx.stop_propagation();
            return true;
        }
        if event.keystroke.key == "escape"
            && !event.is_held
            && !event.keystroke.modifiers.modified()
        {
            self.formula_panel.close();
            window.focus(&self.focus);
            cx.notify();
            cx.stop_propagation();
            return true;
        }
        if session.source.focus_handle(cx).is_focused(window) {
            cx.stop_propagation();
            return true;
        }
        false
    }

    fn copy_formula(&mut self, kind: CopyKind, window: &Window, cx: &mut Context<Self>) {
        let current = self.current_ocr_target();
        let Some(session) = &mut self.formula_panel.session else {
            return;
        };
        if current.as_ref() != Some(&session.target) {
            return;
        }
        if session.owner.is_some() || session.source.read(cx).is_composing() {
            return;
        }
        let result = match kind {
            CopyKind::Source if session.source.read(cx).text().trim().is_empty() => return,
            CopyKind::Source => crate::export::copy_text(
                session.source.read(cx).text(),
                crate::gpui_capture::hwnd(window),
            ),
            CopyKind::Latex => match &session.latex {
                Some(latex)
                    if session.validation.is_none()
                        && session.preview.as_ref().is_some_and(|p| {
                            p.diagnostics.is_empty() && p.source == session.source.read(cx).text()
                        }) =>
                {
                    crate::export::copy_text(latex, crate::gpui_capture::hwnd(window))
                }
                _ => return,
            },
            CopyKind::Image => match &session.output {
                Some(image)
                    if session.validation.is_none()
                        && session
                            .preview
                            .as_ref()
                            .is_some_and(|p| p.diagnostics.is_empty()) =>
                {
                    crate::export::copy_image(image, crate::gpui_capture::hwnd(window))
                }
                _ => return,
            },
        };
        match result {
            Ok(()) => {
                session.note = tr!("已复制", "Copied").into();
            }
            Err(error) => {
                session.note = error.to_string();
                session.failed = true;
            }
        }
        cx.notify();
    }

    pub(crate) fn render_formula_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        for image in self.formula_panel.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.formula_panel
            .retired
            .append(&mut self.formula_panel.retiring);
        let session = self.formula_panel.session.as_ref()?;
        if self.current_ocr_target().as_ref() != Some(&session.target) {
            self.formula_panel.close();
            return None;
        }
        if session.dark != !crate::theme::is_light(cx) && session.owner.is_none() {
            self.schedule_formula_preview(window, cx);
        }
        let session = self.formula_panel.session.as_ref()?;
        let projection = self.ocr_projection(window)?;
        let selected = session.target.selected;
        let toolbar = self.toolbar_layout(selected, window.scale_factor()).primary;
        let r = crate::ocr_panel::document_panel_layout(
            self.ocr_ui_work(window)?,
            projection.rect(selected),
            Some(toolbar),
        )
        .panel;
        let p = crate::theme::palette(cx);
        let busy = session.owner.is_some();
        let ready =
            !busy && !session.source.read(cx).is_composing() && session.validation.is_none();
        let source_mode = session.source_mode;
        let mut tabs = div().flex().items_center().gap(px(6.));
        for (id, label, source_tab) in [
            ("formula-preview-tab", tr!("预览", "Preview"), false),
            ("formula-source-tab", tr!("源码", "Source"), true),
        ] {
            tabs = tabs.child(
                button(id, label, true, p)
                    .when(source_mode == source_tab, |b| b.bg(rgb(p.selected)))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        if let Some(session) = &mut view.formula_panel.session {
                            session.source_mode = source_tab;
                            if source_tab {
                                window.focus(&session.source.focus_handle(cx));
                            } else {
                                window.focus(&view.focus);
                            }
                            cx.notify();
                        }
                    })),
            );
        }
        let header = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap(px(6.))
            .px(px(10.))
            .h(px(44.))
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(tr!("公式识别", "Formula")),
            )
            .child(
                button("formula-retry", tr!("重试", "Retry"), !busy, p)
                    .on_click(cx.listener(|view, _, window, cx| view.request_formula(window, cx))),
            )
            .child(button("formula-close", "×", true, p).on_click(cx.listener(
                |view, _, window, cx| {
                    view.formula_panel.close();
                    window.focus(&view.focus);
                    cx.notify();
                },
            )));
        let body: AnyElement = if source_mode {
            div()
                .flex_1()
                .min_h(px(0.))
                .p(px(10.))
                .overflow_hidden()
                .child(session.source.clone())
                .child(
                    div()
                        .mt(px(8.))
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(tr!(
                            "支持 Markdown、LaTeX；Ctrl+Enter 更新预览",
                            "Markdown and LaTeX · Ctrl+Enter refreshes preview"
                        )),
                )
                .into_any_element()
        } else {
            div()
                .id("formula-markdown-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .overflow_x_scroll()
                .p(px(12.))
                .when_some(session.preview.as_ref(), |body, preview| {
                    body.child(crate::markdown_view::render(preview, p))
                })
                .when(session.preview.is_none(), |body| {
                    body.child(div().text_color(rgb(p.muted)).child(if busy {
                        tr!("正在识别当前选区…", "Recognizing this selection…")
                    } else {
                        tr!("预览将在这里显示", "The preview appears here")
                    }))
                })
                .into_any_element()
        };
        let actions =
            div()
                .flex()
                .flex_shrink_0()
                .gap(px(6.))
                .p(px(10.))
                .child(
                    button(
                        "formula-copy-latex",
                        tr!("复制 LaTeX", "Copy LaTeX"),
                        ready
                            && session.latex.is_some()
                            && session
                                .preview
                                .as_ref()
                                .is_some_and(|p| p.diagnostics.is_empty()),
                        p,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.copy_formula(CopyKind::Latex, window, cx)
                    })),
                )
                .child(
                    button(
                        "formula-copy-source",
                        tr!("复制源码", "Copy source"),
                        !busy && !session.source.read(cx).text().trim().is_empty(),
                        p,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.copy_formula(CopyKind::Source, window, cx)
                    })),
                )
                .child(
                    button(
                        "formula-copy-image",
                        tr!("复制图片", "Copy image"),
                        ready
                            && session.output.is_some()
                            && session
                                .preview
                                .as_ref()
                                .is_some_and(|p| p.diagnostics.is_empty()),
                        p,
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.copy_formula(CopyKind::Image, window, cx)
                    })),
                );
        Some(
            div()
                .id("capture-formula-panel")
                .absolute()
                .left(px(r.x))
                .top(px(r.y))
                .w(px(r.width))
                .h(px(r.height))
                .flex()
                .flex_col()
                .rounded_lg()
                .border_1()
                .border_color(rgb(p.border))
                .bg(rgb(p.background))
                .shadow_lg()
                .text_color(rgb(p.foreground))
                .text_size(px(13.))
                .overflow_hidden()
                .cursor(gpui::CursorStyle::Arrow)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(header)
                .child(div().px(px(10.)).pb(px(6.)).child(tabs))
                .child(body)
                .child(
                    div()
                        .flex_shrink_0()
                        .px(px(12.))
                        .text_size(px(11.))
                        .text_color(rgb(if session.failed { 0xff6777 } else { p.muted }))
                        .child(session.note.clone()),
                )
                .child(actions)
                .into_any_element(),
        )
    }

    pub(crate) fn sync_markdown_translation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for image in self.markdown_translation.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        self.markdown_translation
            .retired
            .append(&mut self.markdown_translation.retiring);
        let current = self
            .current_translation()
            .filter(|t| t.table.is_none())
            .and_then(|t| match &t.state {
                crate::translate::TranslationState::Done(text) => {
                    Some((text.as_str(), !crate::theme::is_light(cx)))
                }
                _ => None,
            });
        if self
            .markdown_translation
            .key
            .as_ref()
            .map(|(text, dark)| (text.as_str(), *dark))
            == current
        {
            return;
        }
        let key = current.map(|(text, dark)| (text.to_owned(), dark));
        self.markdown_translation.task = None;
        retire(
            &mut self.markdown_translation.preview,
            &mut self.markdown_translation.retiring,
        );
        self.markdown_translation.key = key.clone();
        let Some((text, dark)) = key else {
            return;
        };
        self.markdown_translation.task = Some(cx.spawn_in(window, async move |view, cx| {
            let key = (text.clone(), dark);
            let preview = cx
                .background_executor()
                .spawn(async move {
                    let document = crate::markdown_document::parse_with_mode(&text, false).ok()?;
                    document
                        .has_rich_content()
                        .then(|| crate::markdown_view::prepare(&document, dark, 13.))
                })
                .await;
            let _ = view.update_in(cx, |view, _, cx| {
                if view.markdown_translation.key.as_ref() == Some(&key) {
                    view.markdown_translation.preview = preview;
                    cx.notify();
                }
            });
        }));
    }

    pub(crate) fn markdown_translation_preview(
        &self,
        source: &str,
        dark: bool,
    ) -> Option<&Prepared> {
        self.markdown_translation
            .preview
            .as_ref()
            .filter(|p| p.matches(source, dark, 13.))
    }
}

#[derive(Clone, Copy)]
enum CopyKind {
    Source,
    Latex,
    Image,
}

fn button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px(px(10.))
        .h(px(29.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(p.border))
        .text_size(px(12.))
        .text_color(rgb(if enabled { p.foreground } else { p.muted }))
        .when(enabled, |b| {
            b.cursor_pointer().hover(move |b| b.bg(rgb(p.hover)))
        })
        .when(!enabled, |b| b.opacity(0.5))
        .child(label)
}

fn document_latex(document: &Document) -> Option<String> {
    fn inline(items: &[Inline], output: &mut Vec<String>) {
        for item in items {
            match item {
                Inline::Math { source } => output.push(source.clone()),
                Inline::Strong(items)
                | Inline::Emphasis(items)
                | Inline::Strikethrough(items)
                | Inline::Link { content: items, .. } => inline(items, output),
                _ => {}
            }
        }
    }
    fn blocks(items: &[Block], output: &mut Vec<String>) {
        for item in items {
            match &item.kind {
                BlockKind::Math { source } => output.push(source.clone()),
                BlockKind::Paragraph(items) | BlockKind::Heading { content: items, .. } => {
                    inline(items, output)
                }
                BlockKind::List { items, .. } => {
                    for item in items {
                        blocks(item, output);
                    }
                }
                BlockKind::Quote(items) => blocks(items, output),
                BlockKind::Table { rows, .. } => {
                    for row in rows {
                        for cell in row {
                            inline(cell, output);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut formulas = Vec::new();
    blocks(&document.blocks, &mut formulas);
    match formulas.len() {
        0 => None,
        1 => formulas.pop(),
        _ => Some(
            formulas
                .into_iter()
                .map(|f| format!("\\[\n{f}\n\\]"))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mixed_document_copy_preserves_latex_and_table_math_order() {
        let source = "其中 \\(n \\in \\mathbb{N}, \\quad a_n = \\frac{(-1)^{n+1}}{2n-1}\\)";
        let document = crate::markdown_document::parse_with_mode(source, true).unwrap();
        assert_eq!(
            super::document_latex(&document).unwrap(),
            r"n \in \mathbb{N}, \quad a_n = \frac{(-1)^{n+1}}{2n-1}"
        );
        assert_eq!(document.source, source);
        let table = crate::markdown_document::parse_with_mode(
            "| A | B |\n|---|---|\n| $x_1$ | $x_2$ |",
            true,
        )
        .unwrap();
        let result = super::document_latex(&table).unwrap();
        assert!(result.find("x_1").unwrap() < result.find("x_2").unwrap());
    }
}
