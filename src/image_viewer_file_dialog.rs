//! 用户明确提交的源文件操作；名称输入、确认和实际回执属于同一次来源。
use super::*;
use crate::text_field::{TextField, TextFieldEvent};

#[derive(Default)]
pub(super) struct State {
    dialog: Option<Dialog>,
    pending: Option<Pending>,
    next_token: u64,
}

impl State {
    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn close(&mut self) {
        self.dialog = None;
        self.pending = None;
    }
}

struct Pending {
    token: u64,
    cancel: Arc<AtomicBool>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct Target {
    serial: u64,
    path: PathBuf,
    expected: crate::file_snapshot::FileSnapshot,
    neighbor: Option<PathBuf>,
}

enum Kind {
    Rename {
        field: gpui::Entity<TextField>,
        _subscription: gpui::Subscription,
    },
    Recycle,
}

struct Dialog {
    target: Target,
    kind: Kind,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    status: Option<String>,
}

impl ImageViewer {
    pub(super) fn show_file_dialog(
        &mut self,
        rename: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loading || self.snapshot_pending || self.modal_is_open() {
            return;
        }
        let (Some(path), Some(expected)) = (self.path.clone(), self.source_file.clone()) else {
            return;
        };
        self.cancel_file_drag();
        self.invalidate_prefetch();
        self.pan = None;
        self.preview_pan = None;
        self.recent.hide();
        if self.animation.is_some() {
            self.animation_control(animation::Action::Pause, cx);
        }
        let target = Target {
            serial: self.serial,
            path: path.clone(),
            expected,
            neighbor: self
                .collection
                .as_ref()
                .and_then(|collection| collection.neighbor(1).or_else(|| collection.neighbor(-1)))
                .map(Path::to_path_buf),
        };
        let cancel_focus = cx.focus_handle();
        let confirm_focus = cx.focus_handle();
        let kind = if rename {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let field = cx.new(|cx| TextField::new(name, tr!("完整文件名", "Full file name"), cx));
            field.update(cx, |field, cx| {
                field.configure_focus(Vec::new(), window, cx)
            });
            let subscription = cx.subscribe_in(
                &field,
                window,
                |view: &mut Self, _, event, window, cx| match event {
                    TextFieldEvent::Submit => view.submit_file_dialog(window, cx),
                    TextFieldEvent::Changed if !view.file_mutations.busy() => {
                        if let Some(dialog) = &mut view.file_mutations.dialog {
                            dialog.status = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            );
            window.focus(&field.read(cx).focus_handle);
            Kind::Rename {
                field,
                _subscription: subscription,
            }
        } else {
            window.focus(&cancel_focus);
            Kind::Recycle
        };
        self.file_mutations.dialog = Some(Dialog {
            target,
            kind,
            cancel_focus,
            confirm_focus,
            status: None,
        });
        cx.notify();
    }

    pub(super) fn cancel_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pending) = &self.file_mutations.pending {
            pending.cancel.store(true, Ordering::Relaxed);
            if let Some(dialog) = &mut self.file_mutations.dialog {
                dialog.status = Some(
                    tr!(
                        "正在取消，等待文件操作回执…",
                        "Canceling; waiting for the file operation…"
                    )
                    .into(),
                );
            }
        } else {
            self.file_mutations.close();
            window.focus(&self.focus);
        }
        cx.notify();
    }

    pub(super) fn file_dialog_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &self.file_mutations.dialog else {
            return false;
        };
        let field = match &dialog.kind {
            Kind::Rename { field, .. } => Some(field),
            Kind::Recycle => None,
        };
        if field.is_some_and(|field| field.read(cx).is_composing()) {
            return true;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if event.is_held {
            cx.stop_propagation();
            return true;
        }
        if key == "escape" {
            self.cancel_file_dialog(window, cx);
        } else if key == "tab"
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !self.file_mutations.busy()
        {
            let mut focuses = Vec::new();
            if let Some(field) = field {
                focuses.push(field.read(cx).focus_handle.clone());
            }
            focuses.push(dialog.cancel_focus.clone());
            focuses.push(dialog.confirm_focus.clone());
            let current = focuses
                .iter()
                .position(|focus| focus.is_focused(window))
                .unwrap_or(0);
            let next = if modifiers.shift {
                (current + focuses.len() - 1) % focuses.len()
            } else {
                (current + 1) % focuses.len()
            };
            window.focus(&focuses[next]);
            cx.notify();
        } else if matches!(key, "enter" | "space")
            && !modifiers.control
            && !modifiers.alt
            && !modifiers.platform
        {
            if dialog.cancel_focus.is_focused(window) {
                self.cancel_file_dialog(window, cx);
            } else if dialog.confirm_focus.is_focused(window) {
                self.submit_file_dialog(window, cx);
            }
        }
        cx.stop_propagation();
        true
    }

    pub(super) fn submit_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_mutations.busy() {
            return;
        }
        let Some(dialog) = &self.file_mutations.dialog else {
            return;
        };
        if dialog.target.serial != self.serial
            || self.path.as_ref() != Some(&dialog.target.path)
            || self.source_file.as_ref() != Some(&dialog.target.expected)
        {
            self.file_mutations.close();
            self.error = Some(
                tr!(
                    "来源已变化，请重新加载后操作",
                    "Source changed; reload before continuing"
                )
                .into(),
            );
            window.focus(&self.focus);
            cx.notify();
            return;
        }
        let action = match &dialog.kind {
            Kind::Rename { field, .. } => {
                if field.read(cx).is_composing() {
                    return;
                }
                file_mutation::Action::Rename {
                    new_leaf: field.read(cx).text().to_owned(),
                }
            }
            Kind::Recycle => file_mutation::Action::Recycle,
        };
        let target = dialog.target.clone();
        self.file_mutations.next_token = self.file_mutations.next_token.wrapping_add(1);
        let token = self.file_mutations.next_token;
        let cancel = Arc::new(AtomicBool::new(false));
        self.file_mutations.pending = Some(Pending {
            token,
            cancel: cancel.clone(),
        });
        if let Some(dialog) = &mut self.file_mutations.dialog {
            dialog.status = Some(tr!("正在处理源文件…", "Updating source file…").into());
            if let Kind::Rename { field, .. } = &dialog.kind {
                field.update(cx, |field, cx| field.set_enabled(false, cx));
            }
            // 禁用的名称框会消费按键；忙态把焦点移到取消，Esc仍交给宿主。
            window.focus(&dialog.cancel_focus);
        }
        let request = file_mutation::Request {
            path: target.path.clone(),
            expected: target.expected.clone(),
            token,
            cancel,
        };
        let scan_cancel = request.cancel.clone();
        let task = cx.background_executor().spawn(async move {
            let receipt = file_mutation::execute(request, action);
            let collection = match &receipt.outcome {
                file_mutation::Outcome::Renamed { path, .. } => {
                    ImageCollection::scan_cancellable(path, &scan_cancel).ok()
                }
                _ => None,
            };
            (receipt, collection)
        });
        cx.spawn_in(window, async move |view, cx| {
            let (receipt, collection) = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                if view.serial != target.serial
                    || view.path.as_ref() != Some(&target.path)
                    || !view
                        .file_mutations
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.token == receipt.token)
                    || receipt.source_path != target.path
                    || receipt.expected != target.expected
                {
                    return;
                }
                view.file_mutations.pending = None;
                match receipt.outcome {
                    file_mutation::Outcome::Renamed { path, snapshot } => {
                        view.file_mutations.close();
                        view.path = Some(path.clone());
                        view.source_file = snapshot;
                        view.collection = collection;
                        view.clear_file_thumbnails();
                        view.recent.renamed(&target.path, &path);
                        view.notice = receipt
                            .warning
                            .or_else(|| Some(tr!("已重命名", "File renamed").into()));
                    }
                    file_mutation::Outcome::Recycled { .. } => {
                        view.file_mutations.close();
                        view.recent.forget(&target.path);
                        if let Some(next) = target.neighbor {
                            view.open_path(next, true, cx);
                        } else {
                            view.clear_recycled_source();
                        }
                        view.notice = receipt
                            .warning
                            .or_else(|| Some(tr!("已移入回收站", "Moved to Recycle Bin").into()));
                    }
                    file_mutation::Outcome::Unchanged | file_mutation::Outcome::Cancelled => {
                        view.file_mutations.close();
                    }
                    file_mutation::Outcome::Failed(error) => {
                        if !matches!(receipt.source_after, file_mutation::Observation::Matches) {
                            view.source_file = None;
                        }
                        if let Some(dialog) = &mut view.file_mutations.dialog {
                            dialog.status = Some(error);
                            if let Kind::Rename { field, .. } = &dialog.kind {
                                field.update(cx, |field, cx| field.set_enabled(true, cx));
                            }
                        }
                    }
                }
                if let Some(dialog) = &view.file_mutations.dialog {
                    if let Kind::Rename { field, .. } = &dialog.kind {
                        window.focus(&field.read(cx).focus_handle);
                    }
                } else {
                    window.focus(&view.focus);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn clear_file_thumbnails(&mut self) {
        for slot in self.thumbnails.drain(..) {
            if let Some(Thumbnail::Ready(image)) = slot.thumbnail {
                self.retiring.push(image);
            }
        }
        self.thumbnail_epoch = self.thumbnail_epoch.wrapping_add(1);
        self.thumbnail_plan = None;
        self.thumbnail_range = 0..0;
    }

    fn clear_recycled_source(&mut self) {
        self.serial = self.serial.wrapping_add(1);
        self.loader.cancel(self.serial);
        self.retire_image();
        self.retiring
            .extend(self.original_tiles.drain(..).map(|(_, image)| image));
        self.clear_file_thumbnails();
        self.path = None;
        self.source_file = None;
        self.collection = None;
        self.animation = None;
        self.pages = None;
        self.source_kind = None;
        self.projection = None;
        self.desired_projection = None;
        self.snapshot_pending = false;
        self.loading = false;
        self.error = None;
    }

    pub(super) fn file_dialog(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dialog = self.file_mutations.dialog.as_ref()?;
        let palette = crate::theme::palette(cx);
        let rename = matches!(dialog.kind, Kind::Rename { .. });
        let busy = self.file_mutations.busy();
        let mut panel = div()
            .id("image-file-mutation-panel")
            .w(px(460.))
            .max_h(px(420.))
            .overflow_y_scroll()
            .p(px(18.))
            .rounded(px(8.))
            .bg(rgb(palette.background))
            .border_1()
            .border_color(rgb(palette.border))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(div().text_size(px(16.)).child(if rename {
                tr!("重命名源文件", "Rename source file")
            } else {
                tr!("移入回收站", "Move to Recycle Bin")
            }))
            .child(
                div()
                    .text_color(rgb(palette.muted))
                    .child(dialog.target.path.to_string_lossy().into_owned()),
            );
        if let Kind::Rename { field, .. } = &dialog.kind {
            panel = panel.child(field.clone()).child(tr!(
                "重命名不会转换图片格式。",
                "Renaming does not convert the image format."
            ));
        } else {
            panel = panel.child(tr!("将这个源文件移入回收站？无法回收时会报错。", "Move this source file to Recycle Bin? An unavailable recycle operation returns an error."));
        }
        if let Some(status) = &dialog.status {
            panel = panel.child(div().text_color(rgb(palette.muted)).child(status.clone()));
        }
        let mut buttons = div().flex().justify_end().gap(px(8.));
        for (confirm, label, focus) in [
            (false, tr!("取消", "Cancel"), &dialog.cancel_focus),
            (
                true,
                if rename {
                    tr!("重命名", "Rename")
                } else {
                    tr!("移入回收站", "Move to Recycle Bin")
                },
                &dialog.confirm_focus,
            ),
        ] {
            let enabled = !confirm || !busy;
            buttons = buttons.child(
                div()
                    .id(if confirm {
                        "file-mutation-confirm"
                    } else {
                        "file-mutation-cancel"
                    })
                    .track_focus(focus)
                    .px(px(12.))
                    .py(px(7.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(if focus.is_focused(window) {
                        palette.foreground
                    } else {
                        palette.border
                    }))
                    .text_color(rgb(if enabled {
                        palette.foreground
                    } else {
                        palette.muted
                    }))
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(palette.hover)))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            if enabled {
                                if confirm {
                                    view.submit_file_dialog(window, cx);
                                } else {
                                    view.cancel_file_dialog(window, cx);
                                }
                            }
                        }),
                    )
                    .child(label),
            );
        }
        Some(
            div()
                .id("image-file-mutation-overlay")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(gpui::rgba(0x00000099))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(panel.child(buttons))
                .into_any_element(),
        )
    }
}
