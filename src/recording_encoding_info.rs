//! 原录制编码证据的只读面板；不读取当前派生文件，也不控制播放器。
use super::{encoding_feedback, RecordingView};
use crate::capture_toolbar::Palette;
use gpui::{div, prelude::*, px, rgb, Context, Div};

impl RecordingView {
    pub(super) fn encoding_info_panel(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let report = match &self.last_encoding_session {
            Some(session) => session.encoding_report(),
            None => self
                .result
                .as_ref()
                .and_then(|result| result.encoding.clone()),
        };
        let feedback = encoding_feedback::from_report(report.as_ref());
        let mut rows = div().flex().flex_col().gap(px(8.)).min_w(px(0.));
        for row in feedback.rows {
            rows = rows.child(div().w_full().min_w(px(0.)).child(row));
        }
        rows = rows.child(div().text_color(rgb(p.muted)).child(feedback.note));
        div()
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .child(tr!("原录制编码信息", "Original recording encoding")),
                    )
                    .child(
                        div()
                            .id("rec-encoding-close")
                            .px(px(8.))
                            .py(px(4.))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(p.border))
                            .cursor_pointer()
                            .child(tr!("关闭", "Close"))
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.flyout = None;
                                window.focus(&view.focus);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(p.accent))
                    .child(feedback.summary),
            )
            .child(
                div()
                    .id("rec-encoding-rows")
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .text_size(px(11.))
                    .child(rows),
            )
    }
}
