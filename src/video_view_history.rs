//! 历史详情的单个嵌入视频会话；隐藏详情时须显式clear，而非仅停止构建child。
use super::VideoView;
use gpui::{div, prelude::*, px, rgb, AnyElement, App, Entity, FocusHandle, Window};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(crate) struct HistoryVideoPreview {
    selection: Option<(String, PathBuf)>,
    view: Option<Entity<VideoView>>,
    error: Option<String>,
}

/// 只读测试观测不延长会话、图像或媒体工作线程的生命周期。
#[cfg(test)]
pub(crate) struct HistoryVideoPreviewProbe {
    pub(crate) entity: gpui::WeakEntity<VideoView>,
    pub(crate) images: Vec<std::sync::Weak<gpui::RenderImage>>,
    pub(crate) worker: Option<crate::recording_preview::PlaybackShutdownProbe>,
    pub(crate) source: Option<PathBuf>,
    pub(crate) submitted: Option<(u64, std::time::Duration)>,
    pub(crate) acknowledged: Option<(u64, std::time::Duration)>,
    pub(crate) position: Option<std::time::Duration>,
    pub(crate) playing: Option<bool>,
    pub(crate) muted: bool,
    pub(crate) error: Option<String>,
}

impl HistoryVideoPreview {
    /// 每次父页构建时同步选择；相同key/path的失败保留到下一次真正换选。
    /// 离开历史、隐藏详情、进入恢复页或移除当前条目时传None，及时停止后台工作。
    pub(crate) fn reconcile(
        &mut self,
        selected: Option<(&str, &Path)>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.reconcile_inner(selected, None, window, cx);
    }

    /// 子预览仍持有逻辑焦点时，释放后交回宿主；已转移到搜索等控件时不干预。
    pub(crate) fn reconcile_with_focus_return(
        &mut self,
        selected: Option<(&str, &Path)>,
        return_focus: &FocusHandle,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.reconcile_inner(selected, Some(return_focus), window, cx);
    }

    fn reconcile_inner(
        &mut self,
        selected: Option<(&str, &Path)>,
        return_focus: Option<&FocusHandle>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let current = self
            .selection
            .as_ref()
            .map(|(key, path)| (key.as_str(), path.as_path()));
        if current == selected {
            return;
        }
        self.clear(cx);
        let Some((key, path)) = selected else {
            return;
        };
        let path = path.to_path_buf();
        self.selection = Some((key.to_owned(), path.clone()));
        match VideoView::embedded(path, window, cx) {
            Ok(view) => {
                if let Some(return_focus) = return_focus.cloned() {
                    view.update(cx, |_, cx| {
                        cx.on_release_in(window, move |view, window, _| {
                            if view.embedded_has_focus(window) {
                                window.focus(&return_focus);
                            }
                        })
                        .detach();
                    });
                }
                self.view = Some(view);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub(crate) fn element(&self, cx: &App) -> AnyElement {
        if let Some(view) = &self.view {
            return view.clone().into_any_element();
        }
        let p = crate::theme::palette(cx);
        let message = self.error.as_ref().map_or_else(
            || tr!("选择一个视频查看预览", "Select a video to preview").to_string(),
            |error| {
                trf!(
                    "无法打开视频预览：{error}",
                    "Cannot open video preview: {error}"
                )
            },
        );
        div()
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            .p(px(12.))
            .bg(rgb(p.background))
            .text_color(rgb(if self.error.is_some() {
                p.danger
            } else {
                p.muted
            }))
            .text_size(px(12.))
            .child(message)
            .into_any_element()
    }

    /// 返回是否有当前预览接收这次播放动作；载入期间也由该会话负责。
    pub(crate) fn toggle(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let Some(view) = &self.view else {
            return false;
        };
        view.update(cx, |view, cx| view.toggle_embedded(window, cx));
        true
    }

    pub(crate) fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.read(cx).embedded_has_focus(window))
    }

    /// 先停工再释放实体；仅不再.child仍会由本适配器保留播放器及轮询任务。
    pub(crate) fn clear(&mut self, cx: &mut App) {
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| view.shutdown_embedded(cx));
        }
        self.view.take();
        self.selection = None;
        self.error = None;
    }

    #[cfg(test)]
    pub(super) fn entity(&self) -> Option<&Entity<VideoView>> {
        self.view.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn probe(&self, cx: &App) -> Option<HistoryVideoPreviewProbe> {
        let entity = self.view.as_ref()?;
        let view = entity.read(cx);
        Some(HistoryVideoPreviewProbe {
            entity: entity.downgrade(),
            images: view
                .image
                .iter()
                .chain(view.submitted_frame.iter())
                .map(|frame| std::sync::Arc::downgrade(&frame.image))
                .chain(view.retiring.iter().map(std::sync::Arc::downgrade))
                .chain(view.retired.iter().map(std::sync::Arc::downgrade))
                .collect(),
            worker: view.player.as_ref().map(|player| player.shutdown_probe()),
            source: view.path.clone(),
            submitted: view
                .submitted_frame
                .as_ref()
                .map(|frame| (frame.serial, frame.position)),
            acknowledged: view
                .player
                .as_ref()
                .and_then(|player| player.submitted_frame_point()),
            position: view.player.as_ref().map(|player| player.position().0),
            playing: view.player.as_ref().map(|player| player.playing()),
            muted: view.muted,
            error: self
                .error
                .clone()
                .or_else(|| view.open_error.clone())
                .or_else(|| view.frame_draw_error.clone())
                .or_else(|| view.player.as_ref().and_then(|player| player.error())),
        })
    }
}
