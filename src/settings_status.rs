//! 设置窗口外壳上的反馈：保存结果的短暂浮动提示、跨分类可见的“保存规则未应用”条，
//! 以及保存页页眉的“恢复默认”。
use super::SettingsView;
use crate::{
    settings_save::Control,
    settings_widgets::{self as widgets, ButtonKind},
    toolbar_icons::Icon,
};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, Window};
use std::time::{Duration, Instant};

/// 提示显示多久后淡出。
const TOAST_DURATION: Duration = Duration::from_secs(4);

/// 当前浮动提示的文字与截止时间；文字变化时重新计时。
#[derive(Default)]
pub(crate) struct StatusToast {
    text: String,
    until: Option<Instant>,
}

impl SettingsView {
    /// 本页要显示的最新结果：保存页优先显示保存规则自己的提示。
    fn current_status(&self, cx: &Context<Self>) -> String {
        let save = &self.save_settings.read(cx).status;
        if self.on_save_tab() && !save.is_empty() {
            save.clone()
        } else {
            self.status.clone()
        }
    }

    fn on_save_tab(&self) -> bool {
        self.page == super::MainPage::Settings && self.tab == super::SettingsTab::Save
    }

    /// 保存结果以浮动提示显示约 4 秒，不占正文高度；到时清掉同一条消息，
    /// 下一次出现相同文字时也会重新提示。
    pub(crate) fn status_toast(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let text = self.current_status(cx);
        if text != self.toast.text {
            self.toast.text = text.clone();
            self.toast.until = (!text.is_empty()).then(|| Instant::now() + TOAST_DURATION);
            if !text.is_empty() {
                let shown = text.clone();
                cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(TOAST_DURATION).await;
                    let _ = view.update(cx, |view, cx| {
                        if view.status == shown {
                            view.status.clear();
                        }
                        view.save_settings.update(cx, |save, _| {
                            if save.status == shown {
                                save.status.clear();
                            }
                        });
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        let visible = self.toast.until.is_some_and(|until| Instant::now() < until);
        if !visible || text.is_empty() {
            return None;
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(16.))
                .px_8()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("settings-status")
                        .max_w(px(640.))
                        .px(px(14.))
                        .py(px(8.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(surface.line))
                        .bg(rgb(surface.raised))
                        .shadow_md()
                        .text_size(px(12.))
                        .text_color(rgb(palette.foreground))
                        .child(
                            div()
                                .size(px(6.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(rgb(palette.accent)),
                        )
                        .child(div().min_w(px(0.)).child(text)),
                )
                .into_any_element(),
        )
    }

    /// 保存规则有未应用的输入时，窗口底部固定显示应用条；切到其它分类也保留，并可跳回保存页。
    pub(crate) fn save_apply_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.save_settings.read(cx).has_changes() {
            return None;
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let button = |id: &'static str, label: &'static str, kind: ButtonKind| {
            widgets::button(id, label, kind, palette, surface).child(self.settings_probe(id))
        };
        let mut bar = div()
            .flex_shrink_0()
            .min_h(px(52.))
            .px_8()
            .py_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_color(rgb(surface.line))
            .bg(gpui::rgba((palette.accent << 8) | 0x1c))
            .child(div().flex_1().min_w(px(160.)).text_size(px(13.)).child(tr!(
                "保存规则有未应用的修改。",
                "Save rules have unapplied changes."
            )));
        if !self.on_save_tab() {
            bar = bar.child(
                button(
                    "settings-save-show",
                    tr!("查看", "Show"),
                    ButtonKind::Secondary,
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    view.switch_tab(super::SettingsTab::Save, window, cx);
                })),
            );
        }
        Some(
            bar.child(
                button(
                    "settings-save-cancel",
                    tr!("取消修改", "Cancel"),
                    ButtonKind::Secondary,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| {
                        view.save_control(Control::Cancel, window, cx)
                    }),
                ),
            )
            .child(
                button(
                    "settings-save-apply",
                    tr!("应用", "Apply"),
                    ButtonKind::Primary,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| {
                        view.save_control(Control::Apply, window, cx)
                    }),
                ),
            )
            .into_any_element(),
        )
    }

    fn save_control(&mut self, action: Control, window: &mut Window, cx: &mut Context<Self>) {
        self.save_settings
            .update(cx, |save, cx| save.control(action, window, cx));
        cx.notify();
    }

    pub(crate) fn save_reset_width(window: &Window) -> f32 {
        widgets::header_button_width(
            window,
            &[
                tr!("恢复默认", "Restore defaults"),
                tr!("已是默认", "Using defaults"),
            ],
        )
    }

    /// 保存页页眉的“恢复默认”：把默认规则填入草稿，底部应用条确认后生效。
    pub(crate) fn save_reset_button(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let changed = !self.save_settings.read(cx).draft_is_default();
        let label = if changed {
            tr!("恢复默认", "Restore defaults")
        } else {
            tr!("已是默认", "Using defaults")
        };
        let button = widgets::header_button(
            "settings-save-reset",
            Icon::Refresh,
            label,
            width,
            changed,
            palette,
            surface,
        )
        .child(self.settings_probe("settings-save-reset"));
        if !changed {
            return button.into_any_element();
        }
        button
            .on_click(
                cx.listener(|view, _, window, cx| view.save_control(Control::Reset, window, cx)),
            )
            .into_any_element()
    }
}
