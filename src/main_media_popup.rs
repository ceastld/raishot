//! 主界面的媒体弹层：复用查看器实体，关闭时释放会话并回到原位置。
use super::{main_page::MainPage, titlebar, widgets, SettingsView};
use crate::{image_viewer::ImageViewer, video_view::VideoView};
use gpui::{
    div, prelude::*, px, rgb, rgba, AnyElement, Context, Entity, FocusHandle, FontWeight,
    KeyDownEvent, MouseButton, PathPromptOptions, Subscription, Window,
};
use std::path::PathBuf;

enum Content {
    Image(Entity<ImageViewer>),
    Video(Entity<VideoView>),
}

#[derive(Default)]
pub(super) struct MediaPopup {
    pending: Option<(PathBuf, bool)>,
    content: Option<Content>,
    path: Option<PathBuf>,
    video: bool,
    error: Option<String>,
    subscriptions: Vec<Subscription>,
    return_focus: Option<FocusHandle>,
    focus: Option<FocusHandle>,
    origin_page: Option<MainPage>,
    picker_serial: u64,
    pub(super) dismiss_requested: bool,
}

impl MediaPopup {
    pub(super) fn is_open(&self) -> bool {
        self.pending.is_some() || self.path.is_some()
    }
}

impl SettingsView {
    pub(super) fn choose_media_popup(&mut self, video: bool, cx: &mut Context<Self>) {
        self.media_popup.picker_serial += 1;
        let serial = self.media_popup.picker_serial;
        let page = self.page;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(if video {
                tr!("选择视频", "Choose a video").into()
            } else {
                tr!("选择图片", "Choose an image").into()
            }),
        });
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let _ = this.update(cx, |view, cx| {
                if view.media_popup.picker_serial != serial || view.page != page {
                    return;
                }
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            view.open_media_popup(path, video, cx);
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        view.status = trf!("无法选择文件：{error}", "Cannot choose file: {error}");
                        cx.notify();
                    }
                    Err(error) => {
                        view.status = trf!("无法选择文件：{error}", "Cannot choose file: {error}");
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn open_media_popup(&mut self, path: PathBuf, video: bool, cx: &mut Context<Self>) {
        self.media_popup.dismiss_requested = false;
        self.media_popup.pending = Some((path, video));
        cx.notify();
    }

    pub(super) fn close_media_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.media_popup.picker_serial += 1;
        self.media_popup.pending = None;
        self.media_popup.dismiss_requested = false;
        self.media_popup.subscriptions.clear();
        match self.media_popup.content.take() {
            Some(Content::Image(view)) => {
                view.update(cx, |view, cx| view.shutdown_popup(window, cx));
            }
            Some(Content::Video(view)) => {
                view.update(cx, |view, cx| view.shutdown_popup(window, cx));
            }
            None => {}
        }
        self.media_popup.path = None;
        self.media_popup.error = None;
        self.media_popup.focus = None;
        if self.media_popup.origin_page.take() == Some(self.page) {
            if let Some(focus) = self.media_popup.return_focus.take() {
                window.focus(&focus);
            }
        } else {
            self.media_popup.return_focus = None;
        }
        cx.notify();
    }

    pub(super) fn reconcile_media_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.media_popup.dismiss_requested {
            self.close_media_popup(window, cx);
        }
        let Some((path, video)) = self.media_popup.pending.take() else {
            return;
        };
        self.close_media_popup(window, cx);
        self.media_popup.return_focus = if self.page == MainPage::History {
            self.history_popup_return_focus(window, cx)
        } else {
            window.focused(cx)
        };
        self.history_clear_video_preview(cx);
        self.media_popup.origin_page = Some(self.page);
        let focus = cx.focus_handle();
        window.focus(&focus);
        self.media_popup.focus = Some(focus);
        self.media_popup.path = Some(path.clone());
        self.media_popup.video = video;
        let result = if video {
            VideoView::popup(path, window, cx).map(|entity| {
                let subscription = cx.subscribe_in(
                    &entity,
                    window,
                    |view, _, _: &crate::video_view::PopupCloseRequested, window, cx| {
                        view.close_media_popup(window, cx);
                    },
                );
                self.media_popup.subscriptions.push(subscription);
                Content::Video(entity)
            })
        } else {
            ImageViewer::popup(path, window, cx).map(|entity| {
                let subscription = cx.subscribe_in(
                    &entity,
                    window,
                    |view, _, _: &crate::image_viewer::PopupCloseRequested, window, cx| {
                        view.close_media_popup(window, cx);
                    },
                );
                self.media_popup.subscriptions.push(subscription);
                Content::Image(entity)
            })
        };
        match result {
            Ok(content) => self.media_popup.content = Some(content),
            Err(error) => self.media_popup.error = Some(error.to_string()),
        }
    }

    pub(super) fn media_popup_element(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.media_popup.path.as_ref()?;
        let surface = widgets::surface(cx);
        let palette = crate::theme::palette(cx);
        let width = (f32::from(window.viewport_size().width) - 32.).clamp(1., 1240.);
        let height =
            (f32::from(window.viewport_size().height) - titlebar::HEIGHT - 32.).clamp(1., 860.);
        let label = if self.media_popup.video {
            tr!("视频预览", "Video preview")
        } else {
            tr!("图片预览", "Image preview")
        };
        let content = match &self.media_popup.content {
            Some(Content::Image(view)) => view.clone().into_any_element(),
            Some(Content::Video(view)) => view.clone().into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_8()
                .text_color(rgb(palette.muted))
                .child(self.media_popup.error.clone().unwrap_or_default())
                .into_any_element(),
        };
        Some(
            div()
                .id("main-media-popup-backdrop")
                .absolute()
                .top(px(titlebar::HEIGHT))
                .bottom_0()
                .left_0()
                .right_0()
                .occlude()
                .bg(rgba(0x00000099))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, window, cx| {
                        cx.stop_propagation();
                        view.close_media_popup(window, cx);
                    }),
                )
                .child(
                    div()
                        .id("main-media-popup")
                        .when_some(self.media_popup.focus.as_ref(), |popup, focus| {
                            popup.track_focus(focus)
                        })
                        .w(px(width))
                        .h(px(height))
                        .flex()
                        .flex_col()
                        .rounded_xl()
                        .border_1()
                        .border_color(rgb(surface.line))
                        .bg(rgb(surface.card))
                        .shadow_lg()
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                        .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                            if event.keystroke.key == "escape" && !event.is_held {
                                view.close_media_popup(window, cx);
                            }
                            cx.stop_propagation();
                        }))
                        .child(
                            div()
                                .h(px(48.))
                                .flex_shrink_0()
                                .px_4()
                                .flex()
                                .items_center()
                                .gap_3()
                                .border_b_1()
                                .border_color(rgb(surface.line))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(label),
                                )
                                .child(div().flex_1().min_w(px(0.)).text_size(px(12.)))
                                .child(
                                    widgets::button(
                                        "main-media-popup-close",
                                        tr!("关闭 · Esc", "Close · Esc"),
                                        widgets::ButtonKind::Secondary,
                                        palette,
                                        surface,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, window, cx| view.close_media_popup(window, cx),
                                    )),
                                ),
                        )
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_h(px(0.))
                                .w_full()
                                .overflow_hidden()
                                .child(content),
                        ),
                )
                .into_any_element(),
        )
    }
}
