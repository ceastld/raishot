//! 固定录屏库的发现与显式回读；另存始终重新验证，并仅发布到新文件。
use super::{library, widgets, ButtonKind, SettingsView};
use crate::recording::recovery::{
    self as model,
    verification::{self, copy, VerificationLimits, VerificationReport, VerificationState},
    DiagnosticKind, MediaName, MediaState, Phase, ScanReport,
};
use gpui::{div, prelude::*, px, rgb, Context, Div, FontWeight, KeyDownEvent, Window};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
pub(super) struct RecoveryPanel {
    pub(super) visible: bool,
    /// 取消后仍占槽，直到后台真正返回；重开只排队，不并发扫描。
    running: Option<Arc<AtomicBool>>,
    work: Option<Arc<RecoveryWork>>,
    extended_budget: bool,
    scan_after_exit: bool,
    report: Option<(PathBuf, ScanReport)>,
    verification: Option<(PathBuf, MediaName, VerificationReport)>,
    message: Option<String>,
}

impl RecoveryPanel {
    pub(super) fn hide(&mut self) {
        self.visible = false;
        self.cancel();
    }

    fn cancel(&mut self) {
        self.scan_after_exit = false;
        if let Some(cancel) = &self.running {
            cancel.store(true, Ordering::Release);
        }
        if let Some(work) = &self.work {
            work.cancel.store(true, Ordering::Release);
        }
    }
}

impl Drop for RecoveryPanel {
    fn drop(&mut self) {
        self.cancel();
    }
}

enum ScanOutcome {
    Cancelled,
    Finished(PathBuf, ScanReport),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryAction {
    Verify,
    Save,
}

struct RecoveryWork {
    root: PathBuf,
    directory: PathBuf,
    name: MediaName,
    action: RecoveryAction,
    limits: VerificationLimits,
    cancel: Arc<AtomicBool>,
}

enum RecoveryOutcome {
    Verified(VerificationReport),
    Prepared(copy::PreparedRecoveryCopy),
    Failed(String, Option<Box<VerificationReport>>),
    Cancelled,
}

fn work_is_current(
    visible: bool,
    active: Option<&Arc<RecoveryWork>>,
    work: &Arc<RecoveryWork>,
    root: Option<&Path>,
    experimental: bool,
) -> bool {
    visible
        && experimental
        && active.is_some_and(|active| Arc::ptr_eq(active, work))
        && !work.cancel.load(Ordering::Acquire)
        && root == Some(work.root.as_path())
}

fn copy_failure(failure: copy::RecoveryCopyFailure) -> RecoveryOutcome {
    let message = failure.to_string();
    let (_, report, _, _, _, source) = failure.into_parts();
    // 原始 dyn Error 不承诺 Send；在产生它的 worker 内释放，回 UI 只携带值。
    drop(source);
    RecoveryOutcome::Failed(message, Some(report))
}

impl SettingsView {
    pub(super) fn history_recovery_open(&mut self, cx: &mut Context<Self>) {
        self.history.selected = None;
        self.history.chosen.clear();
        let panel = &mut self.history.recovery;
        let was_visible = panel.visible;
        panel.visible = true;
        if let Some(cancel) = &panel.running {
            // 关闭后立即重开：等旧任务释放文件句柄后再开始。
            panel.scan_after_exit = cancel.load(Ordering::Acquire);
        } else if let Some(work) = &panel.work {
            panel.scan_after_exit = work.cancel.load(Ordering::Acquire);
        } else if !was_visible || panel.report.is_none() {
            self.history_recovery_scan(cx);
        }
        cx.notify();
    }

    fn history_recovery_scan(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.history.recovery;
        if !panel.visible || panel.running.is_some() || panel.work.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        panel.running = Some(cancel.clone());
        panel.scan_after_exit = false;
        panel.report = None;
        panel.verification = None;
        panel.message = None;
        let worker_cancel = cancel.clone();
        let task = cx.background_executor().spawn(async move {
            // 连根路径解析都留在后台；预取消时不开始目录检查。
            if worker_cancel.load(Ordering::Acquire) {
                return ScanOutcome::Cancelled;
            }
            let result = std::panic::catch_unwind(|| {
                let root = std::path::absolute(crate::recording::library_dir())
                    .map_err(|error| error.to_string())?;
                if worker_cancel.load(Ordering::Acquire) {
                    return Ok(ScanOutcome::Cancelled);
                }
                let report = model::scan(&root, SystemTime::now(), &|| {
                    worker_cancel.load(Ordering::Acquire)
                });
                Ok(ScanOutcome::Finished(root, report))
            });
            match result {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => ScanOutcome::Failed(error),
                Err(_) => ScanOutcome::Failed(
                    tr!("后台扫描异常结束", "The background scan ended unexpectedly").into(),
                ),
            }
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |view, cx| {
                view.history_recovery_scanned(cancel, outcome, cx)
            });
        })
        .detach();
        cx.notify();
    }

    fn history_recovery_scanned(
        &mut self,
        cancel: Arc<AtomicBool>,
        outcome: ScanOutcome,
        cx: &mut Context<Self>,
    ) {
        let panel = &mut self.history.recovery;
        if !panel
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, &cancel))
        {
            return;
        }
        // 即使已经隐藏或取消，也要等此次完成才腾出唯一扫描槽。
        panel.running = None;
        let scan_again = std::mem::take(&mut panel.scan_after_exit) && panel.visible;
        if panel.visible && !scan_again {
            if cancel.load(Ordering::Acquire) {
                panel.message = Some(tr!("扫描已取消", "Scan cancelled").into());
            } else {
                match outcome {
                    ScanOutcome::Cancelled => {
                        panel.message = Some(tr!("扫描已取消", "Scan cancelled").into());
                    }
                    ScanOutcome::Failed(error) => {
                        panel.message = Some(trf!(
                            "无法扫描未完成录制：{error}",
                            "Cannot scan unfinished recordings: {error}"
                        ));
                    }
                    ScanOutcome::Finished(root, report) => {
                        let current = std::path::absolute(crate::recording::library_dir());
                        if current.as_ref().ok() != Some(&root) {
                            panel.message = Some(
                                tr!(
                                    "录屏库路径已变化，请刷新后查看。",
                                    "The recording folder changed. Refresh to scan it."
                                )
                                .into(),
                            );
                        } else if report.cancelled {
                            panel.message = Some(tr!("扫描已取消", "Scan cancelled").into());
                        } else {
                            panel.report = Some((root, report));
                        }
                    }
                }
            }
        }
        if scan_again {
            self.history_recovery_scan(cx);
        }
        cx.notify();
    }

    fn history_recovery_begin(
        &mut self,
        directory: PathBuf,
        name: MediaName,
        action: RecoveryAction,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RecoveryWork>> {
        let panel = &mut self.history.recovery;
        if !panel.visible || panel.running.is_some() || panel.work.is_some() {
            return None;
        }
        let (root, report) = panel.report.as_ref()?;
        let listed = report.sessions.iter().any(|session| {
            session.directory == directory
                && session.media.iter().any(|media| {
                    media.name == name
                        && media.state == MediaState::Present
                        && media.identity_matches == Some(true)
                        && supported_mp4(name)
                })
        });
        let current = std::path::absolute(crate::recording::library_dir()).ok();
        if !listed
            || current.as_ref() != Some(root)
            || !crate::file_launch_command::experimental_media_enabled()
        {
            panel.message = Some(
                tr!(
                    "当前来源已变化，请刷新后重试。",
                    "The source changed. Refresh before trying again."
                )
                .into(),
            );
            cx.notify();
            return None;
        }
        let work = Arc::new(RecoveryWork {
            root: root.clone(),
            directory,
            name,
            action,
            limits: verification_limits(panel.extended_budget),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        panel.work = Some(work.clone());
        panel.verification = None;
        panel.message = None;
        cx.notify();
        Some(work)
    }

    fn history_recovery_work_current(&self, work: &Arc<RecoveryWork>) -> bool {
        let current = std::path::absolute(crate::recording::library_dir()).ok();
        let panel = &self.history.recovery;
        work_is_current(
            panel.visible,
            panel.work.as_ref(),
            work,
            current.as_deref(),
            crate::file_launch_command::experimental_media_enabled(),
        )
    }

    fn history_recovery_verify(
        &mut self,
        directory: PathBuf,
        name: MediaName,
        cx: &mut Context<Self>,
    ) {
        let Some(work) = self.history_recovery_begin(directory, name, RecoveryAction::Verify, cx)
        else {
            return;
        };
        let worker = work.clone();
        let task = cx.background_executor().spawn(async move {
            if worker.cancel.load(Ordering::Acquire) {
                return RecoveryOutcome::Cancelled;
            }
            match std::panic::catch_unwind(|| {
                verification::verify(
                    &worker.directory,
                    worker.name,
                    &worker.cancel,
                    worker.limits,
                )
            }) {
                Ok(report) => RecoveryOutcome::Verified(report),
                Err(_) => RecoveryOutcome::Failed(
                    tr!(
                        "后台验证异常结束",
                        "The background verification ended unexpectedly"
                    )
                    .into(),
                    None,
                ),
            }
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |view, cx| {
                view.history_recovery_work_finished(work, outcome, cx)
            });
        })
        .detach();
    }

    fn history_recovery_save(
        &mut self,
        directory: PathBuf,
        name: MediaName,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(work) = self.history_recovery_begin(directory, name, RecoveryAction::Save, cx)
        else {
            return;
        };
        let suggested = format!("recovered-{}", name.basename());
        let receiver = cx.prompt_for_new_path(&work.root, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let chosen = match receiver.await {
                Ok(Ok(Some(path))) => Some(Ok(path)),
                Ok(Err(error)) => Some(Err(error.to_string())),
                Ok(Ok(None)) | Err(_) => None,
            };
            let proceed = this
                .update_in(cx, |view, _, cx| {
                    if !view.history_recovery_work_current(&work) {
                        view.history_recovery_work_finished(
                            work.clone(),
                            RecoveryOutcome::Cancelled,
                            cx,
                        );
                        return false;
                    }
                    match &chosen {
                        Some(Ok(_)) => true,
                        Some(Err(error)) => {
                            view.history_recovery_work_finished(
                                work.clone(),
                                RecoveryOutcome::Failed(
                                    trf!(
                                        "无法打开保存对话框：{error}",
                                        "Cannot open the save dialog: {error}"
                                    ),
                                    None,
                                ),
                                cx,
                            );
                            false
                        }
                        None => {
                            view.history_recovery_work_finished(
                                work.clone(),
                                RecoveryOutcome::Cancelled,
                                cx,
                            );
                            false
                        }
                    }
                })
                .unwrap_or(false);
            if !proceed {
                return;
            }
            let Some(Ok(mut destination)) = chosen else {
                return;
            };
            if destination.extension().is_none() {
                destination.set_extension("mp4");
            }
            let worker = work.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    if worker.cancel.load(Ordering::Acquire) {
                        return RecoveryOutcome::Cancelled;
                    }
                    match std::panic::catch_unwind(|| {
                        copy::prepare(
                            &worker.directory,
                            worker.name,
                            &destination,
                            worker.cancel.clone(),
                            worker.limits,
                        )
                    }) {
                        Ok(Ok(prepared)) => RecoveryOutcome::Prepared(prepared),
                        Ok(Err(failure)) => copy_failure(failure),
                        Err(_) => RecoveryOutcome::Failed(
                            tr!("后台另存异常结束", "The background save ended unexpectedly")
                                .into(),
                            None,
                        ),
                    }
                })
                .await;
            let _ = this.update_in(cx, |view, _, cx| {
                view.history_recovery_work_finished(work, outcome, cx)
            });
        })
        .detach();
    }

    fn history_recovery_work_finished(
        &mut self,
        work: Arc<RecoveryWork>,
        outcome: RecoveryOutcome,
        cx: &mut Context<Self>,
    ) {
        if !self
            .history
            .recovery
            .work
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &work))
        {
            return;
        }
        let current = self.history_recovery_work_current(&work);
        // 迟到的 Prepared 直接 Drop：后台读取成功不授予隐藏页面或旧保存意图发布资格。
        let outcome = if current {
            outcome
        } else {
            drop(outcome);
            RecoveryOutcome::Cancelled
        };
        match outcome {
            RecoveryOutcome::Verified(report) => {
                self.history.recovery.verification =
                    Some((work.directory.clone(), work.name, report));
            }
            RecoveryOutcome::Prepared(prepared) => {
                match prepared.commit(|| self.history_recovery_work_current(&work)) {
                    Ok(saved) => {
                        let message = trf!(
                            "已另存验证通过的 MP4：{}",
                            "Verified MP4 saved: {}",
                            saved.path.display()
                        );
                        self.status = message.clone();
                        self.history.recovery.message = Some(message);
                        self.history.recovery.verification =
                            Some((work.directory.clone(), work.name, saved.verification));
                    }
                    Err(failure) => {
                        let (_, report, _, _, message, source) = failure.into_parts();
                        drop(source);
                        self.history.recovery.message =
                            Some(trf!("另存失败：{message}", "Save failed: {message}"));
                        self.history.recovery.verification =
                            Some((work.directory.clone(), work.name, *report));
                    }
                }
            }
            RecoveryOutcome::Failed(message, report) => {
                self.history.recovery.message =
                    Some(trf!("操作失败：{message}", "Operation failed: {message}"));
                if let Some(report) = report {
                    self.history.recovery.verification =
                        Some((work.directory.clone(), work.name, *report));
                }
            }
            RecoveryOutcome::Cancelled => {
                if self.history.recovery.visible {
                    self.history.recovery.message = Some(if work.cancel.load(Ordering::Acquire) {
                        tr!(
                            "操作已取消，原始数据保留。",
                            "Operation canceled; source data kept."
                        )
                        .into()
                    } else if !current {
                        tr!(
                            "来源或页面已变化，请刷新后重试。",
                            "The source or page changed. Refresh before trying again."
                        )
                        .into()
                    } else {
                        tr!(
                            "未另存文件，原始数据保留。",
                            "No file saved; source data kept."
                        )
                        .into()
                    });
                }
            }
        }
        let panel = &mut self.history.recovery;
        panel.work = None;
        let scan_again = std::mem::take(&mut panel.scan_after_exit) && panel.visible;
        if scan_again {
            self.history_recovery_scan(cx);
        }
        cx.notify();
    }

    pub(super) fn history_recovery_body(&self, cx: &mut Context<Self>) -> Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let panel = &self.history.recovery;
        let mut toolbar = div()
            .flex_shrink_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(tr!("未完成录制（实验）", "Unfinished recordings (Preview)")),
            );
        if let Some(cancel) = &panel.running {
            if cancel.load(Ordering::Acquire) {
                toolbar = toolbar.child(widgets::note(
                    tr!("正在等待扫描退出…", "Waiting for the scan to finish…"),
                    palette,
                ));
            } else {
                toolbar = toolbar.child(
                    widgets::button(
                        "history-recovery-cancel",
                        tr!("取消扫描", "Cancel scan"),
                        ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.history.recovery.cancel();
                        cx.notify();
                    })),
                );
            }
        } else if let Some(work) = &panel.work {
            toolbar = toolbar.child(widgets::note(
                if work.cancel.load(Ordering::Acquire) {
                    tr!(
                        "正在等待当前操作退出…",
                        "Waiting for the current operation…"
                    )
                } else if work.action == RecoveryAction::Verify {
                    tr!("正在完整回读验证…", "Verifying the complete recording…")
                } else {
                    tr!(
                        "选择新文件后验证并另存…",
                        "Choose a new file to verify and save…"
                    )
                },
                palette,
            ));
            if !work.cancel.load(Ordering::Acquire) {
                toolbar = toolbar.child(
                    widgets::button(
                        "history-recovery-cancel-work",
                        tr!("取消操作", "Cancel"),
                        ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.history.recovery.cancel();
                        cx.notify();
                    })),
                );
            }
        } else {
            toolbar = toolbar.child(
                widgets::button(
                    "history-recovery-refresh",
                    tr!("刷新 F5", "Refresh F5"),
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .on_click(cx.listener(|view, _, _, cx| view.history_recovery_scan(cx))),
            );
            toolbar = toolbar.child(
                widgets::button(
                    "history-recovery-budget",
                    if panel.extended_budget {
                        tr!("扩展预算：5 分钟", "Extended: 5 min budget")
                    } else {
                        tr!("标准预算：30 秒", "Standard: 30 s budget")
                    },
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    let panel = &mut view.history.recovery;
                    if panel.running.is_none() && panel.work.is_none() {
                        panel.extended_budget = !panel.extended_budget;
                        cx.notify();
                    }
                })),
            );
        }
        toolbar = toolbar.child(
            widgets::button(
                "history-recovery-close",
                tr!("关闭 Esc", "Close Esc"),
                ButtonKind::Secondary,
                palette,
                surface,
            )
            .on_click(cx.listener(|view, _, _, cx| {
                view.history.recovery.hide();
                cx.notify();
            })),
        );

        let mut results = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_3()
            .child(widgets::note(
                tr!(
                    "先扫描文件状态，再显式完整回读已登记的 MP4。另存会重新验证并保存到新文件；原始数据始终保留，不修复损坏文件。",
                    "Scan file status, then verify registered MP4 files. Saving verifies again and creates a new file. Source data is kept; damaged files are not repaired."
                ),
                palette,
            ));
        if panel.running.is_some() {
            results = results.child(widgets::note(
                tr!(
                    "扫描在后台进行；取消需等待当前文件检查结束。",
                    "Scanning in the background; cancellation waits for the current file check."
                ),
                palette,
            ));
        }
        if panel.extended_budget {
            results = results.child(widgets::note(
                tr!(
                    "扩展预算允许处理更大或更长的录制，仍有上限且可取消；当前系统不支持的编码可能仍无法验证。",
                    "The extended budget allows larger or longer recordings within finite limits. You can cancel; unsupported codecs may remain unverifiable."
                ), palette,
            ));
        }
        if let Some(message) = &panel.message {
            results = results.child(widgets::note(message.clone(), palette));
        }
        if let Some((root, report)) = &panel.report {
            results = results
                .child(widgets::note(
                    trf!("录屏库：{}", "Recording folder: {}", root.display()),
                    palette,
                ))
                .child(widgets::note(
                    trf!(
                        "已检查 {} 个目录项、{} 个候选；列出 {} 个未验证会话；跳过 {} 个活动会话。",
                        "Checked {} directory entries and {} candidates; listed {} unverified sessions; skipped {} active sessions.",
                        report.checked_entries,
                        report.checked_candidates,
                        report.sessions.len(),
                        report.active_skipped,
                    ),
                    palette,
                ));
            if report.truncated {
                results = results.child(widgets::note(
                    tr!(
                        "已达到扫描上限，结果不完整；不能据此判断整个目录没有其它残留。",
                        "The scan limit was reached. Results are incomplete and may omit other files."
                    ),
                    palette,
                ));
            }
            if report.sessions.is_empty() {
                results = results.child(widgets::note(
                    if report.diagnostics.is_empty()
                        && report.omitted_diagnostics == 0
                        && !report.truncated
                    {
                        tr!(
                            "本次未列出符合清单条件的未完成录制候选。",
                            "This scan listed no unfinished recordings matching the manifest checks."
                        )
                    } else {
                        tr!(
                            "本次未列出候选，请查看以下诊断；这不代表没有残留。",
                            "No candidates were listed. Check the diagnostics below; files may remain."
                        )
                    },
                    palette,
                ));
            }
            for (session_index, session) in report.sessions.iter().enumerate() {
                let phase = match session.phase {
                    Phase::Created => tr!("已创建", "Created"),
                    Phase::Recording => tr!("录制阶段", "Recording phase"),
                    Phase::Incomplete => tr!("未完成", "Incomplete"),
                };
                let mut card = div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(surface.line))
                    .bg(rgb(surface.card))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(session.directory.display().to_string()),
                    )
                    .child(widgets::note(
                        trf!(
                            "清单最后阶段：{phase} · 创建：{} · 期限：{} · 未验证",
                            "Last manifest phase: {phase} · Created: {} · Expires: {} · Unverified",
                            unix_date(session.created_unix_seconds),
                            unix_date(session.expires_unix_seconds),
                        ),
                        palette,
                    ));
                if session.expired {
                    card = card.child(widgets::note(
                        tr!(
                            "已超过清单期限；扫描时数据仍保留，本页不会自动清理。",
                            "The manifest has expired; files remained at scan time. This page does not clean them up."
                        ),
                        palette,
                    ));
                }
                for (media_index, media) in session.media.iter().enumerate() {
                    let state = match media.state {
                        MediaState::Missing => tr!("缺失", "Missing"),
                        MediaState::Present => tr!("存在（未验证）", "Present (unverified)"),
                        MediaState::Skipped => tr!("已跳过", "Skipped"),
                        MediaState::InspectFailed => tr!("检查失败", "Inspection failed"),
                    };
                    let mut text = format!("{} · {state}", media.name.basename());
                    if let Some(bytes) = media.bytes {
                        text.push_str(&format!(" · {}", library::bytes_text(bytes)));
                    }
                    if media.state == MediaState::Present {
                        let identity = match media.identity_matches {
                            Some(true) => tr!("身份与清单一致", "Identity matches the manifest"),
                            Some(false) => {
                                tr!("身份与清单不同", "Identity differs from the manifest")
                            }
                            None => tr!("清单未登记身份", "No identity recorded in the manifest"),
                        };
                        text.push_str(&format!(" · {identity}"));
                        if let Some(unchanged) = media.content_unchanged {
                            text.push_str(if unchanged {
                                tr!(
                                    " · 大小及写入时间与清单一致",
                                    " · Size and write time match the manifest"
                                )
                            } else {
                                tr!(
                                    " · 大小或写入时间与清单不同",
                                    " · Size or write time differs from the manifest"
                                )
                            });
                        }
                    }
                    card = card.child(widgets::note(text, palette));
                    if media.state == MediaState::Present
                        && media.identity_matches == Some(true)
                        && supported_mp4(media.name)
                    {
                        if panel.running.is_none() && panel.work.is_none() {
                            let directory = session.directory.clone();
                            let name = media.name;
                            let save_directory = directory.clone();
                            let index = session_index * MediaName::ALL.len() + media_index;
                            card = card.child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap_2()
                                    .child(
                                        widgets::button(
                                            ("history-recovery-verify", index),
                                            tr!("完整验证", "Verify"),
                                            ButtonKind::Secondary,
                                            palette,
                                            surface,
                                        )
                                        .child(self.settings_probe(format!(
                                            "history-recovery-verify-{index}"
                                        )))
                                        .on_click(
                                            cx.listener(move |view, _, _, cx| {
                                                view.history_recovery_verify(
                                                    directory.clone(),
                                                    name,
                                                    cx,
                                                )
                                            }),
                                        ),
                                    )
                                    .child(
                                        widgets::button(
                                            ("history-recovery-save", index),
                                            tr!("验证并另存 MP4…", "Verify and save MP4…"),
                                            ButtonKind::Secondary,
                                            palette,
                                            surface,
                                        )
                                        .on_click(
                                            cx.listener(move |view, _, window, cx| {
                                                view.history_recovery_save(
                                                    save_directory.clone(),
                                                    name,
                                                    window,
                                                    cx,
                                                )
                                            }),
                                        ),
                                    ),
                            );
                        }
                        if let Some((directory, name, verification)) = &panel.verification {
                            if directory == &session.directory && name == &media.name {
                                card = card
                                    .child(widgets::note(verification_text(verification), palette));
                            }
                        }
                    }
                }
                results = results.child(card);
            }
            if !report.diagnostics.is_empty() || report.omitted_diagnostics != 0 {
                results = results.child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tr!("扫描诊断", "Scan diagnostics")),
                );
                for diagnostic in &report.diagnostics {
                    results = results.child(widgets::note(
                        trf!(
                            "{}：{} · {}",
                            "{}: {} · {}",
                            diagnostic_label(diagnostic.kind),
                            diagnostic.path.display(),
                            diagnostic.message,
                        ),
                        palette,
                    ));
                }
                if report.omitted_diagnostics != 0 {
                    results = results.child(widgets::note(
                        trf!(
                            "另有 {} 条诊断因上限未展开。",
                            "{} additional diagnostics were omitted by the limit.",
                            report.omitted_diagnostics,
                        ),
                        palette,
                    ));
                }
            }
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .px_8()
            .pb_3()
            .min_w(px(0.))
            .text_size(px(12.))
            .text_color(rgb(palette.foreground))
            .track_focus(&self.history.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _, cx| {
                if event.keystroke.modifiers.modified() || event.is_held {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" => {
                        view.history.recovery.hide();
                        cx.notify();
                        cx.stop_propagation();
                    }
                    "f5" => {
                        view.history_recovery_scan(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(toolbar)
            .child(
                div()
                    .id("history-recovery-results")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(results),
            )
    }
}

fn supported_mp4(name: MediaName) -> bool {
    name == MediaName::Video || MediaName::ENCODER_ATTEMPTS.contains(&name)
}

fn verification_limits(extended: bool) -> VerificationLimits {
    if extended {
        VerificationLimits {
            max_elapsed: Duration::from_secs(300),
            max_file_bytes: 32 * 1024 * 1024 * 1024,
            max_video_frames: 1_000_000,
            max_decoded_bytes: 512 * 1024 * 1024 * 1024,
            max_audio_sample_values: 2_000_000_000,
            max_media_time: Duration::from_secs(24 * 60 * 60),
            ..VerificationLimits::default()
        }
    } else {
        VerificationLimits::default()
    }
}

fn verification_text(report: &VerificationReport) -> String {
    let mut text = match report.state {
        VerificationState::VerifiedReadable => trf!(
            "本次已完整回读 {} 个视频帧；另存仍会重新验证。",
            "This check decoded all {} video frames. Saving will verify again.",
            report.video_frames,
        ),
        VerificationState::CurrentBackendCannotVerify => tr!(
            "当前系统媒体能力无法完成验证；不能据此认定文件损坏。",
            "The current system cannot verify this file; this does not prove it is damaged."
        )
        .into(),
        VerificationState::Limited => tr!(
            "已达到验证时间或容量上限，尚未验证完整。",
            "The verification time or size limit was reached; completeness is unverified."
        )
        .into(),
        VerificationState::Cancelled => tr!(
            "验证已取消，尚未验证完整。",
            "Verification canceled; completeness is unverified."
        )
        .into(),
        VerificationState::Failed => tr!(
            "验证失败，原始文件保留。",
            "Verification failed; source files kept."
        )
        .into(),
    };
    if report.state == VerificationState::VerifiedReadable {
        text.push_str(match report.audio_presence {
            verification::AudioPresence::Absent => tr!(" 无音轨。", " No audio track."),
            verification::AudioPresence::Present => tr!(
                " 首条音轨也已完整回读。",
                " The first audio track was also decoded to EOF."
            ),
            verification::AudioPresence::Unknown => {
                tr!(" 音轨状态未知。", " Audio status unknown.")
            }
        });
    }
    if let Some(error) = &report.error {
        text.push(' ');
        text.extend(error.chars().take(512));
    }
    text
}

fn unix_date(seconds: u64) -> String {
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .and_then(library::local_time)
        .as_ref()
        .map(library::date_text)
        .unwrap_or_else(|| tr!("时间不可用", "Time unavailable").into())
}

fn diagnostic_label(kind: DiagnosticKind) -> &'static str {
    match kind {
        DiagnosticKind::UnsafePath => tr!("路径已跳过", "Path skipped"),
        DiagnosticKind::DirectoryRead => tr!("目录读取", "Directory read"),
        DiagnosticKind::LeaseMissing => tr!("缺少会话占用记录", "Missing session record"),
        DiagnosticKind::LeaseCheck => tr!("录制占用检查", "Recording access check"),
        DiagnosticKind::ManifestRead => tr!("清单读取", "Manifest read"),
        DiagnosticKind::ManifestInvalid => tr!("清单无效", "Invalid manifest"),
        DiagnosticKind::MediaInspect => tr!("媒体检查", "Media inspection"),
        DiagnosticKind::ChangedDuringScan => tr!("扫描期间变化", "Changed during scan"),
        DiagnosticKind::EntryLimit => tr!("目录项上限", "Entry limit"),
        DiagnosticKind::CandidateLimit => tr!("候选上限", "Candidate limit"),
        DiagnosticKind::Clock => tr!("时钟错误", "Clock error"),
    }
}

#[cfg(all(test, windows))]
#[path = "main_history_recovery_smoke.rs"]
mod smoke;

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> Arc<RecoveryWork> {
        Arc::new(RecoveryWork {
            root: PathBuf::from("C:/recordings"),
            directory: PathBuf::from("C:/recordings/.rustshot-recording-model"),
            name: MediaName::Video,
            action: RecoveryAction::Save,
            limits: VerificationLimits::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    #[test]
    fn recording_recovery_ui_rejects_late_replaced_hidden_and_disabled_work() {
        let first = work();
        let replacement = work();
        let root = Some(first.root.as_path());
        assert!(work_is_current(true, Some(&first), &first, root, true));
        assert!(!work_is_current(
            true,
            Some(&replacement),
            &first,
            root,
            true
        ));
        assert!(!work_is_current(false, Some(&first), &first, root, true));
        assert!(!work_is_current(true, Some(&first), &first, root, false));
        assert!(!work_is_current(
            true,
            Some(&first),
            &first,
            Some(Path::new("C:/other")),
            true
        ));
        assert!(!work_is_current(true, None, &first, root, true));
        first.cancel.store(true, Ordering::Release);
        assert!(!work_is_current(true, Some(&first), &first, root, true));
    }

    #[test]
    fn recording_recovery_ui_hiding_cancels_without_releasing_the_worker_slot() {
        let active = work();
        let scan = Arc::new(AtomicBool::new(false));
        let mut panel = RecoveryPanel::default();
        panel.visible = true;
        panel.work = Some(active.clone());
        panel.running = Some(scan.clone());
        panel.scan_after_exit = true;
        panel.hide();
        assert!(!panel.visible && !panel.scan_after_exit);
        assert!(scan.load(Ordering::Acquire) && active.cancel.load(Ordering::Acquire));
        assert!(panel
            .work
            .as_ref()
            .is_some_and(|stored| Arc::ptr_eq(stored, &active)));
        assert!(panel
            .running
            .as_ref()
            .is_some_and(|stored| Arc::ptr_eq(stored, &scan)));
    }
}
