//! 截图图片翻译：复用原位 OCR 的段落，逐块在后台请求已配置的翻译服务；全部返回后把带
//! 取样底色的译文文字标注一次插入文档，一次撤销。只发送块文字，不发送图片或剪贴板；
//! 取色与排版在本机完成，译文之后可像普通文字标注一样双击修改。
//! 设置 ▸ 翻译 填写百度凭证后（引擎 auto/baidu）改走百度图片翻译 V2：上传选区图片，
//! 贴合图在图片编辑窗口打开，可继续标注、复制、保存或贴图。
use crate::{
    capture_view::CaptureView,
    image_translate::{self, Colors, SourceBlock},
};
use gpui::{AppContext, Context, Window};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

/// 同时进行的块请求数；单块失败不影响其它块。
const WORKERS: usize = 3;

struct Block {
    source: SourceBlock,
    colors: Colors,
    result: Option<Result<String, String>>,
}

pub(crate) struct Job {
    id: u64,
    /// 发起时的输出范围；完成时范围已变则放弃，避免译文落到别处。
    selected: crate::geometry::Rect,
    target: String,
    blocks: Vec<Block>,
    pending: usize,
    skipped: usize,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub(crate) struct ImageTranslation {
    job: Option<Job>,
    /// 尚无识别结果时先走 OCR，结果安装后接着翻译。
    pub(super) after_ocr: bool,
    request: u64,
    /// 会话内按（目标语言，原文）缓存；不写磁盘。
    cache: HashMap<(String, String), String>,
    /// 百度整图请求进行中：（请求号，取消标记）。
    baidu: Option<(u64, Arc<AtomicBool>)>,
}

impl ImageTranslation {
    pub(crate) fn busy(&self) -> bool {
        self.job.is_some() || self.baidu.is_some()
    }
    /// Esc 先只取消进行中的百度整图请求，不直接退出截图。
    pub(crate) fn cancel_whole_image(&mut self) -> bool {
        match self.baidu.take() {
            Some((_, cancel)) => {
                cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
    /// 停止派发剩余块；已发出的请求返回后被丢弃。
    pub(crate) fn cancel(&mut self) -> bool {
        if let Some((_, cancel)) = self.baidu.take() {
            cancel.store(true, Ordering::Relaxed);
            return true;
        }
        match self.job.take() {
            Some(job) => {
                job.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }
}

impl CaptureView {
    /// 工具栏“更多”、Y 键与 OCR 面板菜单的共同入口。
    pub(crate) fn translate_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.image_translation.busy() {
            self.status = tr!("正在翻译图片…", "Translating image…").into();
            cx.notify();
            return;
        }
        let preferences = crate::preferences::Preferences::load();
        if let crate::baidu_image_translate::Engine::Baidu(credentials) =
            crate::baidu_image_translate::engine(&preferences)
        {
            self.start_baidu_translation(credentials, preferences.translate_target, window, cx);
            return;
        }
        if crate::translate::Service::from_preferences(&preferences).is_none() {
            self.status = tr!(
                "翻译服务未配置；请在 设置 ▸ 翻译 中填写接口地址",
                "Translation is not configured; set it in Settings ▸ Translation"
            )
            .into();
            cx.notify();
            return;
        }
        if self.ocr_overlay.is_some() {
            self.start_image_translation(cx);
            return;
        }
        if !self.inline_ocr_host() {
            self.status = tr!(
                "这里暂不支持图片翻译",
                "Image translation is not available here"
            )
            .into();
            cx.notify();
            return;
        }
        self.recognize(false, window, cx);
        // 识别没有开始（输入法、草稿或保存中）时不留下待续标记。
        self.image_translation.after_ocr = self.processing;
    }

    fn start_baidu_translation(
        &mut self,
        credentials: crate::baidu_image_translate::Credentials,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.processing {
            return;
        }
        if !self.finish_content_or_status() {
            cx.notify();
            return;
        }
        let selected = match self.output_rect() {
            Ok(rect) => rect,
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
        };
        // 上传当前可见内容（含已有标注），与导出像素一致。
        let clip_revision = self.editor.long_clip().map(|_| self.editor.document.revision());
        let image = match if clip_revision.is_some() {
            self.selected_content_image()
        } else {
            self.content_image_in(selected)
        } {
            Ok(image) => image,
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
        };
        let state = &mut self.image_translation;
        state.request = state.request.wrapping_add(1);
        let id = state.request;
        let cancel = Arc::new(AtomicBool::new(false));
        state.baidu = Some((id, cancel.clone()));
        self.status = tr!(
            "正在用百度图片翻译…  Esc 取消",
            "Translating with Baidu…  Esc cancels"
        )
        .into();
        cx.notify();
        let handle = window.window_handle();
        let (sender, receiver) = futures::channel::oneshot::channel();
        // 网络请求可持续数秒，独立线程避免占用 GPUI 后台执行器。
        std::thread::spawn(move || {
            let result = crate::baidu_image_translate::translate(&credentials, &image, &target);
            let _ = sender.send(result);
        });
        cx.spawn(async move |view, cx| {
            let Ok(result) = receiver.await else {
                return;
            };
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let _ = cx.update_window(handle, |_, window, cx| {
                let _ = view.update(cx, |view, cx| {
                    if !view
                        .image_translation
                        .baidu
                        .as_ref()
                        .is_some_and(|(request, _)| *request == id)
                    {
                        return;
                    }
                    view.image_translation.baidu = None;
                    cx.notify();
                    if view.output_rect().ok() != Some(selected)
                        || clip_revision.is_some_and(|revision| revision != view.editor.document.revision()) {
                        view.status = tr!(
                            "截图范围已改变，已放弃本次图片翻译",
                            "The capture area changed; image translation discarded"
                        )
                        .into();
                        return;
                    }
                    let opened = result.and_then(|image| {
                        if view.quick_action.current()
                            == Some(crate::quick_action::Step::TranslatePin)
                        {
                            let selected = view.output_rect().map_err(|error| error.to_string())?;
                            let origin = if clip_revision.is_some() {
                                crate::long_output::pin_origin((image.width, image.height), view.long_clip_placement)
                            } else { (
                                view.source.bounds.left + selected.x as i32,
                                view.source.bounds.top + selected.y as i32,
                            ) };
                            let result = if view.quick_action.record_history() {
                                crate::pin_view::PinView::open_with_history(image, origin, cx)
                            } else {
                                crate::pin_view::PinView::open(image, origin, cx)
                            };
                            result.map_err(|error| error.to_string())
                        } else {
                            crate::image_editor::open(image, None, cx)
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        }
                    });
                    match opened {
                        Err(error) => view.status = error,
                        Ok(_) => {
                            view.remember_last_region();
                            view.quick_action.fail();
                            view.status.clear();
                            // 截图会话交给图片编辑窗口；贴图与图片编辑宿主保持原窗口。
                            if view.image_canvas.is_none() && !view.pin_mode {
                                view.flush_tool_presets();
                                view.capture_lock.take();
                                window.remove_window();
                            }
                        }
                    }
                });
            });
        })
        .detach();
    }

    /// 由 OCR 回执在安装原位结果后调用；只消费一次待续标记。
    pub(crate) fn resume_image_translation(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.image_translation.after_ocr) {
            return;
        }
        if self.ocr_overlay.is_some() {
            self.start_image_translation(cx);
        } else if self.status.is_empty() {
            self.status = tr!("没有识别到可翻译的文字", "No text found to translate").into();
        }
    }

    fn start_image_translation(&mut self, cx: &mut Context<Self>) {
        let Some(service) =
            crate::translate::Service::from_preferences(&crate::preferences::Preferences::load())
        else {
            return;
        };
        let Some((selected, blocks)) = self.ocr_overlay_translation_blocks() else {
            return;
        };
        if let Some(notice) = service.first_use_notice() {
            self.notify_output_warning(notice);
        }
        // 取色使用当前可见内容（含已有标注），与导出像素一致。
        let image = match self.content_image_in(selected) {
            Ok(image) => image,
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                return;
            }
        };
        let origin = (selected.x as f32, selected.y as f32);
        let state = &mut self.image_translation;
        let mut skipped = 0;
        let mut reasons: Vec<&'static str> = Vec::new();
        let mut job_blocks = Vec::new();
        for source in blocks {
            if let Some(skip) = image_translate::skip_reason(&source, &service.target) {
                skipped += 1;
                if !reasons.contains(&skip.label()) {
                    reasons.push(skip.label());
                }
                continue;
            }
            let colors = image_translate::sample_colors(&image, origin, source.bounds);
            let cached = state
                .cache
                .get(&(service.target.clone(), source.text.clone()))
                .cloned();
            job_blocks.push(Block {
                source,
                colors,
                result: cached.map(Ok),
            });
        }
        if job_blocks.is_empty() {
            self.status = if skipped > 0 {
                trf!(
                    "没有需要翻译的文字（跳过 {} 段：{}）",
                    "Nothing to translate ({} blocks skipped: {})",
                    skipped,
                    reasons.join(tr!("、", ", "))
                )
            } else {
                tr!("没有识别到可翻译的文字", "No text found to translate").into()
            };
            cx.notify();
            return;
        }
        let queue: VecDeque<(usize, String)> = job_blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.result.is_none())
            .map(|(index, block)| (index, block.source.text.clone()))
            .collect();
        state.request = state.request.wrapping_add(1);
        let id = state.request;
        let cancel = Arc::new(AtomicBool::new(false));
        let pending = queue.len();
        state.job = Some(Job {
            id,
            selected,
            target: service.target.clone(),
            blocks: job_blocks,
            pending,
            skipped,
            cancel: cancel.clone(),
        });
        if pending == 0 {
            self.finish_image_translation(cx);
            return;
        }
        self.show_image_translation_progress();
        cx.notify();
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        let queue = Arc::new(Mutex::new(queue));
        // 网络请求可持续数秒，独立线程避免占用 GPUI 后台执行器。
        for _ in 0..WORKERS.min(pending) {
            let (queue, sender, cancel, service) = (
                queue.clone(),
                sender.clone(),
                cancel.clone(),
                service.clone(),
            );
            std::thread::spawn(move || loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let Some((index, text)) = queue.lock().ok().and_then(|mut q| q.pop_front()) else {
                    break;
                };
                let result = crate::translate::translate(&service, &text);
                if sender.unbounded_send((index, result)).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        cx.spawn(async move |view, cx| {
            use futures::StreamExt;
            while let Some((index, result)) = receiver.next().await {
                let finished = view
                    .update(cx, |view, cx| {
                        view.image_translation_progress(id, index, result, cx)
                    })
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// 返回 true 表示本任务已结束（完成、取消或被新任务替换）。
    fn image_translation_progress(
        &mut self,
        id: u64,
        index: usize,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let state = &mut self.image_translation;
        let Some(job) = state.job.as_mut().filter(|job| job.id == id) else {
            return true;
        };
        let Some(block) = job.blocks.get_mut(index) else {
            return false;
        };
        if let Ok(text) = &result {
            state.cache.insert(
                (job.target.clone(), block.source.text.clone()),
                text.clone(),
            );
        }
        block.result = Some(result);
        job.pending = job.pending.saturating_sub(1);
        if job.pending == 0 {
            self.finish_image_translation(cx);
            return true;
        }
        self.show_image_translation_progress();
        cx.notify();
        false
    }

    fn show_image_translation_progress(&mut self) {
        if let Some(job) = &self.image_translation.job {
            let total = job.blocks.len();
            self.status = trf!(
                "正在翻译图片 {}/{} 段…  Esc 取消",
                "Translating image {}/{}…  Esc to cancel",
                total - job.pending,
                total
            );
        }
    }

    fn finish_image_translation(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.image_translation.job.take() else {
            return;
        };
        if self.output_rect().ok() != Some(job.selected) {
            self.status = tr!(
                "截图范围已改变，已放弃本次图片翻译",
                "The capture area changed; image translation discarded"
            )
            .into();
            cx.notify();
            return;
        }
        let mut annotations = Vec::new();
        let (mut failed, mut overflow) = (0, 0);
        let mut first_error = None;
        for block in &job.blocks {
            let fitted = match &block.result {
                Some(Ok(text)) => image_translate::fitted_text(&block.source, text, block.colors)
                    .map_err(|error| error.to_string()),
                Some(Err(error)) => Err(error.clone()),
                None => Err(tr!("翻译没有返回", "No translation returned").into()),
            };
            match fitted {
                Ok(fitted) => {
                    overflow += usize::from(fitted.overflow);
                    annotations.push(fitted.annotation);
                }
                Err(error) => {
                    failed += 1;
                    first_error.get_or_insert(error);
                }
            }
        }
        if annotations.is_empty() {
            self.status = trf!(
                "图片翻译失败：{}",
                "Image translation failed: {}",
                first_error.unwrap_or_default()
            );
            cx.notify();
            return;
        }
        let count = annotations.len();
        // 退出原位文字层，译文作为普通文字标注留在文档里，可撤销、双击修改。
        self.dismiss_recognition_result();
        self.editor.reset_group_scope();
        self.editor.document.insert_many(annotations);
        self.quick_translation_ready = failed == 0;
        let mut status = trf!(
            "已翻译 {} 段，Ctrl+Z 撤销，双击译文可修改",
            "Translated {} blocks; Ctrl+Z to undo, double-click to edit",
            count
        );
        if job.skipped > 0 {
            status.push_str(&trf!("；跳过 {} 段", "; {} skipped", job.skipped));
        }
        if overflow > 0 {
            status.push_str(&trf!(
                "；{} 段译文超出原区域",
                "; {} overflow the original area",
                overflow
            ));
        }
        if failed > 0 {
            status.push_str(&trf!(
                "；{} 段失败：{}",
                "; {} failed: {}",
                failed,
                first_error.unwrap_or_default()
            ));
        }
        self.status = status;
        cx.notify();
    }
}
