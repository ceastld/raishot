//! 截图内帮助复用完整命令说明和当前工具栏快捷键来源，不维护第二张键表。
use crate::{
    capture_toolbar::{shortcut_hints, Palette},
    capture_view::CaptureView,
    hotkey::HotkeyTarget,
    toolbar_groups::Slot,
    toolbar_layout::{PanelRect, ToolbarPanel},
};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, FontWeight};

/// 左下快捷键提示卡显隐：本进程首次用到时读偏好，F1 切换后立即写回 settings.ini。
fn key_hints() -> &'static std::sync::atomic::AtomicBool {
    static SHOW: std::sync::OnceLock<std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();
    SHOW.get_or_init(|| {
        std::sync::atomic::AtomicBool::new(
            crate::preferences::Preferences::load().capture_key_hints,
        )
    })
}

impl CaptureView {
    pub(crate) fn key_hints_visible(&self) -> bool {
        key_hints().load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 只有框选前的截图画布有提示卡；贴图、图片编辑和已有选区时 F1 仍打开完整帮助。
    pub(crate) fn f1_toggles_key_hints(&self) -> bool {
        !self.pin_mode && self.image_canvas.is_none() && self.editor.crop.is_none()
    }

    pub(crate) fn toggle_key_hints(&mut self, cx: &mut Context<Self>) {
        let show = !key_hints().fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
        let mut preferences = crate::preferences::Preferences::load();
        if preferences.capture_key_hints != show {
            preferences.capture_key_hints = show;
            if preferences.save().is_err() {
                self.status = tr!(
                    "快捷键提示的显隐未能保存，下次截图会恢复原设置",
                    "Couldn't save the shortcut hint setting; it resets next capture"
                )
                .into();
            }
        }
        cx.notify();
    }

    pub(crate) fn help_popup(&self, rect: PanelRect, cx: &mut Context<Self>) -> AnyElement {
        let palette = Palette::new(self.light_theme);
        let smart_mosaic = self.smart_mosaic_available();
        let context = if self.pin_mode {
            shortcut_hints::Context::Pin
        } else {
            shortcut_hints::Context::Capture { smart_mosaic }
        };
        let tool = self.editor.tool;
        let current = shortcut_hints::tooltip(
            tool.label(),
            shortcut_hints::for_tool(tool, context, &self.toolbar_config),
        );
        let status = if self.saving || self.processing || self.refresh_request.is_some() {
            tr!(
                "当前正在处理，输出与工具命令暂不可用。",
                "Processing: output and tool commands are temporarily unavailable."
            )
        } else if self.editor.is_dragging() || self.editor.has_pending_drawing() {
            tr!(
                "先结束或取消当前绘制、拖动，再执行输出。",
                "Finish or cancel the current drawing or drag before output."
            )
        } else if self.editor.crop.is_some() {
            tr!(
                "已有选区；输入、参数与识别面板优先处理按键。",
                "Area selected. Input, parameters and recognition panels take priority."
            )
        } else {
            tr!(
                "先框选范围再保存或转录屏、长截图；R 可识别悬停区域。",
                "Select an area to save, record or capture a long page. R reads hovered text."
            )
        };
        let preferences = crate::preferences::Preferences::load();
        let text = crate::tray::shortcut_help::help_text(
            env!("CARGO_PKG_VERSION"),
            &preferences.hotkey_label(),
            &preferences.hotkey_label_for(HotkeyTarget::ClipboardPin),
        );
        let mut bindings = div().flex().flex_col().gap_1().child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(tr!("当前工具栏", "Current toolbar")),
        );
        for slot in self.toolbar_config.slots() {
            let label = match slot {
                Slot::Tool(tool) => shortcut_hints::tooltip(
                    tool.label(),
                    shortcut_hints::for_tool(tool, context, &self.toolbar_config),
                ),
                Slot::Group(group) => {
                    let shown = self.tool_groups.shown(group, tool, smart_mosaic);
                    shortcut_hints::group_tooltip(
                        shown.label(),
                        group,
                        shown,
                        context,
                        &self.toolbar_config,
                    )
                }
            };
            bindings = bindings.child(div().child(label));
        }
        let mut help = div().flex().flex_col().gap(px(3.)).child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .mt_2()
                .child(tr!("命令与工作流", "Commands and workflows")),
        );
        for line in text.lines() {
            help = if line.is_empty() {
                help.child(div().h(px(6.)))
            } else {
                help.child(div().child(line.to_owned()))
            };
        }
        self.panel("capture-help", rect)
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr!("截图快捷键", "Capture shortcuts")),
                    )
                    .child(
                        self.chip("capture-help-close", tr!("关闭", "Close"), false)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.toolbar_panel = ToolbarPanel::None;
                                window.focus(&view.focus);
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_shrink_0()
                    .child(div().child(trf!("当前工具：{current}", "Current tool: {current}")))
                    .child(div().text_color(rgb(palette.muted)).child(status)),
            )
            .child(
                div()
                    .id("capture-help-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(bindings)
                    .child(help),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "Esc / F1 关闭帮助，返回截图；不会执行输出。",
                        "Esc / F1 closes help and returns to capture without output."
                    )),
            )
            .into_any_element()
    }
}
