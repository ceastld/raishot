//! 已有视频队列的只读列表；仅在用户选择其它项时启动该项的打开流程。
use super::{button, VideoView};
use crate::video_file_queue::MAX_VIDEO_FILES;
use gpui::{div, prelude::*, px, rgb, Context};

impl VideoView {
    pub(super) fn queue_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let position = self.queue.position().map_or(0, |(position, _)| position);
        let total = self.queue.len();
        button(
            "video-queue",
            trf!("队列 {position}/{total}", "Queue {position}/{total}"),
            self.show_queue,
            cx,
        )
        .on_click(cx.listener(|view, _, _, cx| {
            cx.stop_propagation();
            if view.prompt_pending {
                return;
            }
            view.show_queue = !view.show_queue;
            if view.show_queue {
                view.hide_recent();
                view.hide_audio();
                view.hide_gif();
                view.show_details = false;
            }
            cx.notify();
        }))
    }

    pub(super) fn hide_queue(&mut self) {
        self.show_queue = false;
    }

    pub(super) fn queue_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.show_queue {
            return None;
        }
        let p = crate::theme::palette(cx);
        let mut menu = div()
            .id("video-queue-menu")
            .w_full()
            .max_h(px(120.))
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex_shrink()
            .px(px(16.))
            .py(px(6.))
            .bg(rgb(p.background))
            .border_b_1()
            .border_color(rgb(p.border));
        if self.queue.is_empty() {
            menu = menu.child(
                div()
                    .py(px(10.))
                    .text_size(px(12.))
                    .text_color(rgb(p.muted))
                    .child(tr!("队列中没有视频", "No videos in the queue")),
            );
        }
        for (index, item) in self.queue.items().iter().take(MAX_VIDEO_FILES).enumerate() {
            let source_generation = self.source_generation;
            let current = self.queue.index() == Some(index);
            let mut text = div().flex_1().min_w(px(0.)).child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(item.path().display().to_string()),
            );
            if let Some(error) = item.error() {
                text = text.child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(11.))
                        .text_color(rgb(if current { p.on_accent } else { p.danger }))
                        .child(trf!("无法打开：{error}", "Cannot open: {error}")),
                );
            }
            menu = menu.child(
                div()
                    .id(("video-queue-row", index))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .py(px(5.))
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(if current { p.on_accent } else { p.foreground }))
                    .bg(rgb(if current { p.accent } else { p.background }))
                    .hover(move |style| {
                        style.bg(rgb(if current { p.accent_hover } else { p.hover }))
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        if view.prompt_pending
                            || !view.show_queue
                            || view.source_generation != source_generation
                        {
                            return;
                        }
                        if view.queue.index() == Some(index) {
                            view.hide_queue();
                            cx.notify();
                        } else if view.queue.select(index) {
                            view.hide_queue();
                            view.start_current();
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .w(px(30.))
                            .flex_shrink_0()
                            .child((index + 1).to_string()),
                    )
                    .child(text),
            );
        }
        Some(menu.into_any_element())
    }
}
