//! 快捷动作调用现有截图输出；后台结果仍由原有 OCR 身份与取消令牌保护。
use super::*;
use crate::quick_action::{QuickRecipe, Step};

fn record_history_once(
    flow: &mut crate::quick_action_flow::QuickActionFlow,
    record: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if flow.record_history() {
        record()?;
        flow.history_succeeded();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quick_action::{HistoryPolicy, Source};
    use crate::quick_action_flow::QuickActionFlow;

    #[test]
    fn capture_output_history_failure_keeps_completed_steps_and_core_retry_target() {
        let mut flow = QuickActionFlow::default();
        flow.install(QuickRecipe {
            source: Source::Region,
            edit: true,
            steps: [
                Some(Step::CopyImage),
                Some(Step::QuickSave),
                Some(Step::Pin),
            ],
            history: HistoryPolicy::Inherit,
        });
        assert!(flow.start(false, true));
        // 剪贴板副作用已经成功；历史失败只影响附属记录。
        flow.succeeded();
        let mut writes = 0;
        let failed = record_history_once(&mut flow, || {
            writes += 1;
            Err("history is read-only".into())
        });
        assert!(failed.is_err());
        assert!(flow.running());
        assert_eq!(flow.completed_steps(), 1);
        assert_eq!(flow.current(), Some(Step::QuickSave));
        assert!(flow.record_history());

        // 下一项主操作失败后，重试从保存继续，不回到已经完成的复制。
        flow.fail();
        assert!(flow.start(false, true));
        assert_eq!(flow.current(), Some(Step::QuickSave));
        flow.succeeded();
        record_history_once(&mut flow, || {
            writes += 1;
            Ok(())
        })
        .unwrap();
        assert!(!flow.record_history());
        assert_eq!(flow.current(), Some(Step::Pin));
        flow.fail();
        assert!(flow.start(false, true));
        assert_eq!(flow.current(), Some(Step::Pin));
        record_history_once(&mut flow, || {
            writes += 1;
            Err("history must not run twice".into())
        })
        .unwrap();
        assert_eq!(writes, 2);
        assert_eq!(flow.completed_steps(), 2);
    }

    #[test]
    fn capture_output_history_never_policy_does_not_invoke_storage() {
        let mut flow = QuickActionFlow::default();
        flow.install(QuickRecipe {
            history: HistoryPolicy::Never,
            ..QuickRecipe::default()
        });
        assert!(flow.start(false, true));
        flow.succeeded();
        let mut writes = 0;
        record_history_once(&mut flow, || {
            writes += 1;
            Err("storage unavailable".into())
        })
        .unwrap();
        assert_eq!(writes, 0);
        assert_eq!(flow.completed_steps(), 1);
        assert!(flow.current().is_none());
    }
}

fn with_output_warnings(message: &str, warnings: &[String]) -> String {
    std::iter::once(message)
        .chain(warnings.iter().map(String::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(tr!("；", "; "))
}

impl CaptureView {
    pub(crate) fn install_quick_action(&mut self, recipe: QuickRecipe) {
        self.quick_completion_message.clear();
        self.quick_completion_warnings.clear();
        self.quick_action.install(recipe);
    }

    pub(crate) fn maybe_start_quick_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pin_mode || self.image_canvas.is_some() || !self.control_selection_ready {
            return;
        }
        if self.quick_action.current() == Some(Step::TranslatePin)
            && !self.processing
            && !self.image_translation.busy()
            && !self.image_translation.after_ocr
        {
            if self.quick_translation_ready {
                self.quick_translation_ready = false;
                self.perform(Action::Pin, window, cx);
                if !self.pin_handoff_pending {
                    self.quick_failed(self.status.clone(), cx);
                }
            } else {
                self.quick_failed(self.status.clone(), cx);
            }
            return;
        }
        if self.quick_action.start(
            true,
            self.editor.crop.is_some() && self.screenshot_output_ready(),
        ) {
            self.run_quick_step(window, cx);
        }
    }

    pub(crate) fn confirm_quick_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_action.recipe.is_none() {
            self.copy(window, cx);
            return;
        }
        if !self.finish_content_or_status() {
            return;
        }
        self.editor.apply_lasso();
        if self
            .quick_action
            .start(false, self.screenshot_output_ready())
        {
            self.run_quick_step(window, cx);
        }
    }
    pub(crate) fn confirm_range(
        &mut self,
        rect: Rect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quick_action.recipe.is_none() {
            self.copy_range(rect, window, cx);
            return;
        }
        self.editor.set_crop(Some(rect));
        self.confirm_quick_action(window, cx);
    }

    pub(super) fn quick_failed(&mut self, error: impl ToString, cx: &mut Context<Self>) {
        let count = self.quick_action.completed_steps();
        self.quick_action.fail();
        show_screenshot_toolbar(&mut self.toolbar_visibility, &self.editor);
        let status = if count == 0 {
            error.to_string()
        } else {
            trf!(
                "已完成 {} 步，后续失败：{}",
                "Completed {} steps; next step failed: {}",
                count,
                error.to_string()
            )
        };
        self.status = with_output_warnings(&status, &self.quick_completion_warnings);
        cx.notify();
    }

    pub(crate) fn cancel_quick_action(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.quick_action.running() {
            return false;
        }
        if self.quick_link_pending {
            // ShellExecute 已发布后无法取消；认领回执后再前进，避免重试重复打开。
            self.status = tr!("正在打开链接，请稍候…", "Opening link; please wait…").into();
            cx.notify();
            return true;
        }
        self.cancel_capture_ocr_tasks("取消快捷动作");
        let save_pending = self.quick_save_cancel.is_some();
        if let Some(cancel) = &self.quick_save_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        if !save_pending {
            self.quick_action_epoch = self.quick_action_epoch.wrapping_add(1);
        }
        self.processing = false;
        self.saving = save_pending;
        self.image_translation.cancel();
        self.quick_failed(
            tr!(
                "已取消快捷动作，选区已保留",
                "Action canceled; selection kept"
            ),
            cx,
        );
        true
    }

    pub(crate) fn record_session_history(&mut self, image: &Image) -> Result<()> {
        self.remember_last_region();
        record_history_once(&mut self.quick_action, || crate::history::record(image))
    }

    pub(crate) fn remember_last_region(&self) {
        if self.pin_mode || self.image_canvas.is_some() {
            return;
        }
        if let Some(rect) = self.editor.crop {
            let _ = crate::last_region::save(self.source.bounds, rect);
        }
    }
    pub(crate) fn notify_quick_completion(&self) {
        // 提示限长；先给附属失败摘要，避免长路径把警告截掉。
        let message = self
            .quick_completion_warnings
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.quick_completion_message.as_str()))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(tr!("；", "; "));
        self.notify_output_warning(&message);
    }

    /// 常驻进程接收提示后独立展示；发送失败不能阻止已经成功的输出结束。
    pub(crate) fn notify_output_warning(&self, message: &str) {
        if message.is_empty() {
            return;
        }
        let monitor = self.source.screen_rect(self.toolbar_monitor);
        let screen = crate::geometry::ScreenRect {
            left: self.source.bounds.left.saturating_add(monitor.x as i32),
            top: self.source.bounds.top.saturating_add(monitor.y as i32),
            width: monitor.width,
            height: monitor.height,
        };
        if !crate::quick_toast::send(message, screen) {
            eprintln!("{message}");
        }
    }

    fn retain_quick_completion_warning(&mut self, warning: String) {
        eprintln!("{warning}");
        if !self.quick_completion_warnings.contains(&warning) {
            self.quick_completion_warnings.push(warning);
        }
    }
    fn quick_text_history(&mut self) {
        if self.quick_action.record_history() {
            let result = self
                .selected_image()
                .and_then(|image| self.record_session_history(&image));
            if let Err(error) = result {
                self.retain_quick_completion_warning(trf!(
                    "文字输出已完成，但历史保存失败：{error}",
                    "Text output completed, but saving history failed: {error}"
                ));
            }
        }
    }
    fn quick_keep_ocr_result(
        &mut self,
        recognized: &crate::recognition::RecognizedText,
        structure: Option<crate::ppocr::table::TableResult>,
    ) {
        if let Some(target) = self.current_ocr_target() {
            self.show_recognition_result(
                recognized.text.clone(),
                text_result::Origin::Ocr(target.clone()),
                tr!("本机文字识别", "Local text recognition"),
            );
            if let Some(structure) = structure {
                self.install_table_copy_detected(recognized, target.clone(), None, Some(structure));
            } else {
                self.install_table_copy(recognized, target.clone());
            }
            self.install_ocr_overlay(recognized, target);
        }
    }

    pub(crate) fn quick_ocr_pending(&self) -> bool {
        matches!(
            self.quick_action.current(),
            Some(Step::OcrCopy | Step::TableCopy)
        )
    }

    pub(crate) fn quick_ocr_received(
        &mut self,
        result: std::result::Result<crate::recognition::RecognizedText, String>,
        structure: Option<crate::ppocr::table::TableResult>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = self.quick_action.current();
        let recognized = match result {
            Ok(text) if !text.text.trim().is_empty() => text,
            Ok(_) => {
                self.quick_failed(tr!("未识别到文字", "No text recognized"), cx);
                return;
            }
            Err(error) => {
                self.quick_failed(error, cx);
                return;
            }
        };
        let mut message = trf!(
            "已复制 {} 字",
            "Copied {} chars",
            recognized.text.chars().count()
        );
        let result = if step == Some(Step::TableCopy) {
            let table_note = structure.as_ref().and_then(|table| table.warning.clone());
            let cache = crate::pin_view::table_copy::TableCopy::with_structure(
                &recognized,
                (),
                0,
                structure.as_ref().and_then(|table| table.grid.clone()),
                structure.as_ref().and_then(|table| table.region.clone()),
            );
            if let Some(grid) = cache.grid(&(), 0, &recognized.text) {
                message = trf!(
                    "已复制表格 {}×{}",
                    "Copied {}×{} table",
                    grid.rows(),
                    grid.columns()
                );
                if let Some(note) = table_note {
                    message.push_str(&format!("；{note}"));
                }
                crate::table_clipboard::copy_table(&grid, hwnd(window))
            } else if let Some(text) = cache.text(&(), 0, &recognized.text) {
                message = tr!("表格已复制（按识别顺序）", "Table copied in reading order").into();
                export::copy_text(text, hwnd(window))
            } else {
                self.quick_keep_ocr_result(&recognized, structure);
                self.quick_failed(
                    table_note.unwrap_or_else(|| {
                        tr!(
                            "未找到可靠表格，已显示文字",
                            "No reliable table; text shown"
                        )
                        .into()
                    }),
                    cx,
                );
                return;
            }
        } else {
            export::copy_text(&recognized.text, hwnd(window))
        };
        if let Err(error) = result {
            self.quick_keep_ocr_result(&recognized, structure);
            self.quick_failed(error, cx);
            return;
        }
        self.remember_last_region();
        self.quick_completion_message = message;
        self.quick_text_history();
        self.quick_action.succeeded();
        self.run_quick_step(window, cx);
    }

    fn run_quick_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(step) = self.quick_action.current() else {
            self.quick_action.complete();
            self.notify_quick_completion();
            self.finish_output(tr!("快捷动作已完成", "Action completed"), window);
            return;
        };
        match step {
            Step::CopyImage => {
                let result = self.selected_image().and_then(|image| {
                    let origin = self.clipboard_image_origin(self.output_rect()?);
                    export::copy_image_at(&image, hwnd(window), origin)?;
                    self.quick_action.succeeded();
                    if let Err(error) = self.record_session_history(&image) {
                        self.retain_quick_completion_warning(trf!(
                            "图片已复制，但历史保存失败：{error}",
                            "Image copied, but saving history failed: {error}"
                        ));
                    }
                    Ok(())
                });
                match result {
                    Ok(()) => {
                        self.run_quick_step(window, cx);
                    }
                    Err(error) => self.quick_failed(error, cx),
                }
            }
            Step::QuickSave => {
                let image = match self.selected_image() {
                    Ok(image) => image,
                    Err(error) => {
                        self.quick_failed(error, cx);
                        return;
                    }
                };
                let protected = self.image_canvas.as_ref().and_then(|s| s.path.clone());
                self.saving = true;
                self.status = tr!("正在快速保存…", "Quick saving…").into();
                let epoch = self.quick_action_epoch;
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                self.quick_save_cancel = Some(cancel.clone());
                let task = cx.background_executor().spawn(async move {
                    crate::save_workflow::quick_save_cancellable(
                        &image,
                        protected.as_deref(),
                        &cancel,
                    )
                    .map(|report| (image, report))
                    .map_err(|error| error.to_string())
                });
                cx.spawn_in(window, async move |view, cx| {
                    let result = task.await;
                    let _ = view.update_in(cx, |v, w, cx| {
                        if v.quick_action_epoch != epoch
                            || v.quick_action.expected() != Some(Step::QuickSave)
                        {
                            return;
                        }
                        v.saving = false;
                        let cancelled = v.quick_save_cancel.take().is_some_and(|cancel| {
                            cancel.load(std::sync::atomic::Ordering::Acquire)
                        });
                        if cancelled {
                            if let Ok((image, report)) = result {
                                v.quick_action.succeeded();
                                if let Err(error) = v.record_session_history(&image) {
                                    v.retain_quick_completion_warning(trf!(
                                        "图片已保存，但历史保存失败：{error}",
                                        "Image saved, but saving history failed: {error}"
                                    ));
                                }
                                for warning in report.warnings {
                                    v.retain_quick_completion_warning(warning);
                                }
                                let saved = crate::save_workflow::message(&report.path, &[]);
                                let message = trf!(
                                    "{saved}；后续步骤已取消",
                                    "{saved}; remaining steps canceled"
                                );
                                v.status =
                                    with_output_warnings(&message, &v.quick_completion_warnings);
                                if !v.quick_completion_warnings.is_empty() {
                                    v.notify_output_warning(
                                        &v.quick_completion_warnings.join(tr!("；", "; ")),
                                    );
                                }
                            }
                            cx.notify();
                            return;
                        }
                        match result {
                            Ok((image, report)) => {
                                v.quick_completion_message =
                                    crate::save_workflow::message(&report.path, &[]);
                                // 保存已经成功；记录错误不使重试重复保存。
                                v.quick_action.succeeded();
                                if let Err(error) = v.record_session_history(&image) {
                                    v.retain_quick_completion_warning(trf!(
                                        "图片已保存，但历史保存失败：{error}",
                                        "Image saved, but saving history failed: {error}"
                                    ));
                                }
                                for warning in report.warnings {
                                    v.retain_quick_completion_warning(warning);
                                }
                                v.run_quick_step(w, cx);
                            }
                            Err(error) => v.quick_failed(error, cx),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            Step::OcrCopy | Step::TableCopy => {
                self.recognize(false, window, cx);
                if !self.processing {
                    self.quick_failed(self.status.clone(), cx);
                }
            }
            Step::Pin => {
                self.perform(Action::Pin, window, cx);
                if !self.pin_handoff_pending {
                    self.quick_failed(self.status.clone(), cx);
                }
            }
            Step::TranslatePin => {
                self.quick_translation_ready = false;
                self.translate_image(window, cx);
                if !self.processing
                    && !self.image_translation.after_ocr
                    && !self.image_translation.busy()
                {
                    self.quick_failed(self.status.clone(), cx);
                }
            }
            Step::QrCopy | Step::QrOpen => {
                let image = match self.selected_content_image() {
                    Ok(image) => image,
                    Err(error) => {
                        self.quick_failed(error, cx);
                        return;
                    }
                };
                self.processing = true;
                self.status = tr!("正在识别二维码…", "Reading QR codes…").into();
                let epoch = self.quick_action_epoch;
                let crop = self.editor.crop;
                let revision = self.editor.document.revision();
                let stamp = self.editor.scene_stamp();
                let task = cx.background_executor().spawn(async move {
                    let text = crate::recognition::qr(image).map_err(|error| error.to_string())?;
                    if step == Step::QrOpen {
                        crate::capture_qr::web_url(&text).ok_or_else(|| {
                            tr!("内容不是单个网页链接", "Content is not a single web link")
                                .to_string()
                        })?;
                    }
                    Ok::<_, String>(text)
                });
                cx.spawn_in(window, async move |view, cx| {
                    let result = task.await;
                    let _ = view.update_in(cx, |v, w, cx| {
                        if v.quick_action_epoch != epoch || v.quick_action.current() != Some(step) {
                            return;
                        }
                        v.processing = false;
                        if v.editor.crop != crop
                            || v.editor.document.revision() != revision
                            || v.editor.scene_stamp() != stamp
                        {
                            v.quick_failed(
                                tr!("截图范围或内容已改变，请重试", "Capture changed; retry"),
                                cx,
                            );
                            return;
                        }
                        if step == Step::QrOpen {
                            match result {
                                Ok(text) => v.quick_open_link(text, w, cx),
                                Err(error) => v.quick_failed(error, cx),
                            }
                            return;
                        }
                        let result = result.and_then(|text| {
                            v.quick_completion_message = trf!(
                                "已复制二维码内容 {} 字",
                                "Copied {} QR chars",
                                text.chars().count()
                            );
                            if step == Step::QrCopy {
                                export::copy_text(&text, hwnd(w))
                                    .map_err(|error| error.to_string())?;
                            }
                            Ok(())
                        });
                        match result {
                            Ok(()) => {
                                v.remember_last_region();
                                v.quick_text_history();
                                v.quick_action.succeeded();
                                v.run_quick_step(w, cx);
                            }
                            Err(error) => v.quick_failed(error, cx),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }
    fn quick_open_link(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = crate::capture_qr::web_url(&text) else {
            self.quick_failed(tr!("内容不是链接", "Content is not a link"), cx);
            return;
        };
        self.processing = true;
        self.quick_link_pending = true;
        self.status = tr!("正在打开链接…", "Opening link…").into();
        let epoch = self.quick_action_epoch;
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                windows::Win32::UI::WindowsAndMessaging::ASFW_ANY,
            );
        }
        let (sender, receiver) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = sender.send(crate::capture_qr::open_in_browser(&url));
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = receiver.await;
            let _ = view.update_in(cx, |v, w, cx| {
                if v.quick_action_epoch != epoch || v.quick_action.current() != Some(Step::QrOpen) {
                    return;
                }
                v.quick_link_pending = false;
                v.processing = false;
                match result {
                    Ok(Ok(())) => {
                        v.remember_last_region();
                        v.quick_text_history();
                        v.quick_action.succeeded();
                        v.run_quick_step(w, cx);
                    }
                    Ok(Err(error)) => v.quick_failed(error, cx),
                    Err(_) => v.quick_failed(tr!("打开链接失败", "Failed to open link"), cx),
                }
            });
        })
        .detach();
    }
}
