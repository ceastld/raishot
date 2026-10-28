//! SVG栅格尺寸输入只提交同一加载来源；原文件与工作线程由宿主管理。
use super::*;
use crate::{
    file_snapshot::FileSnapshot,
    image_metadata::SourceFormat,
    text_field::{TextField, TextFieldEvent},
};

#[derive(Default)]
pub(super) struct State {
    dialog: Option<Dialog>,
}

impl State {
    pub(super) fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    pub(super) fn close(&mut self) {
        self.dialog = None;
    }
}

#[derive(Clone)]
struct Target {
    serial: u64,
    path: PathBuf,
    expected: FileSnapshot,
}

struct Dialog {
    target: Target,
    width: gpui::Entity<TextField>,
    height: gpui::Entity<TextField>,
    cancel_focus: FocusHandle,
    confirm_focus: FocusHandle,
    status: Option<String>,
    _subscriptions: Vec<gpui::Subscription>,
}

fn parse_dimensions(width: &str, height: &str) -> std::result::Result<(u32, u32), String> {
    let invalid = || {
        tr!(
            "宽高须为1–65535的整数。",
            "Width and height must be integers from 1 to 65535."
        )
        .to_string()
    };
    let width = width.trim().parse::<u32>().map_err(|_| invalid())?;
    let height = height.trim().parse::<u32>().map_err(|_| invalid())?;
    if width == 0
        || height == 0
        || width > crate::export::MAX_DECODE_SIDE
        || height > crate::export::MAX_DECODE_SIDE
    {
        return Err(invalid());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(invalid)?;
    if pixels > crate::image_decode::MAX_DECODE_PIXELS {
        return Err(tr!(
            "栅格尺寸不能超过3200万像素。",
            "The raster size must not exceed 32 million pixels."
        )
        .into());
    }
    Ok((width, height))
}

impl ImageViewer {
    pub(super) fn show_svg_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.snapshot_pending || self.modal_is_open() {
            return;
        }
        let (Some(image), Some(path), Some(expected)) = (
            self.image.as_ref(),
            self.path.clone(),
            self.source_file.clone(),
        ) else {
            return;
        };
        if image.metadata.format != SourceFormat::Svg {
            return;
        }
        let dimensions = image.metadata.display_dimensions;
        let target = Target {
            serial: self.serial,
            path,
            expected,
        };
        self.cancel_file_drag();
        self.invalidate_prefetch();
        self.pan = None;
        self.preview_pan = None;
        self.recent.hide();

        let width = cx.new(|cx| TextField::new(dimensions.0.to_string(), tr!("宽度", "Width"), cx));
        let height =
            cx.new(|cx| TextField::new(dimensions.1.to_string(), tr!("高度", "Height"), cx));
        let fields = [width.clone(), height.clone()];
        let mut subscriptions = Vec::new();
        for field in &fields {
            field.update(cx, |field, cx| {
                field.configure_focus(Vec::new(), window, cx)
            });
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |view: &mut Self, _, event, window, cx| match event {
                    TextFieldEvent::Submit => view.submit_svg_dialog(window, cx),
                    TextFieldEvent::Changed => {
                        if let Some(dialog) = &mut view.raster_dialog.dialog {
                            dialog.status = None;
                        }
                        cx.notify();
                    }
                },
            ));
        }
        let initial_focus = width.read(cx).focus_handle.clone();
        self.raster_dialog.dialog = Some(Dialog {
            target,
            width,
            height,
            cancel_focus: cx.focus_handle(),
            confirm_focus: cx.focus_handle(),
            status: None,
            _subscriptions: subscriptions,
        });
        window.focus(&initial_focus);
        cx.notify();
    }

    fn cancel_svg_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.raster_dialog.close();
        window.focus(&self.focus);
        cx.notify();
    }

    pub(super) fn svg_dialog_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(dialog) = &self.raster_dialog.dialog else {
            return false;
        };
        // 输入法合成先处理，不能由宿主的Esc、Enter或Tab提前结束本次输入。
        if dialog.width.read(cx).is_composing() || dialog.height.read(cx).is_composing() {
            cx.stop_propagation();
            return true;
        }
        if event.is_held {
            cx.stop_propagation();
            return true;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let navigation =
            !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.function;
        if key == "escape" {
            self.cancel_svg_dialog(window, cx);
        } else if key == "tab" && navigation {
            let focuses = [
                dialog.width.read(cx).focus_handle.clone(),
                dialog.height.read(cx).focus_handle.clone(),
                dialog.cancel_focus.clone(),
                dialog.confirm_focus.clone(),
            ];
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
        } else if matches!(key, "enter" | "space") && navigation {
            if dialog.cancel_focus.is_focused(window) {
                self.cancel_svg_dialog(window, cx);
            } else if dialog.confirm_focus.is_focused(window) {
                self.submit_svg_dialog(window, cx);
            }
        }
        cx.stop_propagation();
        true
    }

    fn submit_svg_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = &self.raster_dialog.dialog else {
            return;
        };
        if dialog.width.read(cx).is_composing() || dialog.height.read(cx).is_composing() {
            return;
        }
        let dimensions =
            parse_dimensions(dialog.width.read(cx).text(), dialog.height.read(cx).text());
        let target = dialog.target.clone();
        let current = !self.loading
            && self.serial == target.serial
            && self.path.as_ref() == Some(&target.path)
            && self.source_file.as_ref() == Some(&target.expected)
            && self
                .image
                .as_ref()
                .is_some_and(|image| image.metadata.format == SourceFormat::Svg);
        let result = if current {
            dimensions
        } else {
            Err(tr!(
                "SVG来源已变化，请重新加载后设置尺寸。",
                "The SVG source changed; reload it before setting the size."
            )
            .into())
        };
        match result {
            Ok(dimensions) => {
                self.raster_dialog.close();
                window.focus(&self.focus);
                self.rasterize_svg(target.path, target.expected, dimensions, cx);
            }
            Err(error) => {
                if let Some(dialog) = &mut self.raster_dialog.dialog {
                    dialog.status = Some(error);
                }
            }
        }
        cx.notify();
    }

    pub(super) fn svg_dialog(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let dialog = self.raster_dialog.dialog.as_ref()?;
        let palette = crate::theme::palette(cx);
        let mut panel = div()
            .id("image-svg-raster-panel")
            .w(px(420.))
            .max_w(px(420.))
            .max_h(px(440.))
            .overflow_y_scroll()
            .p(px(18.))
            .rounded(px(8.))
            .bg(rgb(palette.background))
            .border_1()
            .border_color(rgb(palette.border))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(16.))
                    .child(tr!("SVG栅格尺寸", "SVG raster size")),
            )
            .child(
                div()
                    .text_color(rgb(palette.muted))
                    .child(dialog.target.path.to_string_lossy().into_owned()),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(tr!("宽度（像素）", "Width (px)"))
                            .child(dialog.width.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(tr!("高度（像素）", "Height (px)"))
                            .child(dialog.height.clone()),
                    ),
            )
            .child(div().text_color(rgb(palette.muted)).child(tr!(
                "透明画布；内容等比适应并居中。原SVG不变。",
                "Transparent canvas; fit and center the content. The SVG is unchanged."
            )));
        if let Some(error) = &dialog.status {
            panel = panel.child(div().text_color(rgb(palette.danger)).child(error.clone()));
        }
        let mut buttons = div().flex().justify_end().gap(px(8.));
        for (confirm, label, focus) in [
            (false, tr!("取消", "Cancel"), &dialog.cancel_focus),
            (true, tr!("栅格化", "Rasterize"), &dialog.confirm_focus),
        ] {
            buttons = buttons.child(
                div()
                    .id(if confirm {
                        "image-svg-raster-confirm"
                    } else {
                        "image-svg-raster-cancel"
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
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(palette.hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            if confirm {
                                view.submit_svg_dialog(window, cx);
                            } else {
                                view.cancel_svg_dialog(window, cx);
                            }
                        }),
                    )
                    .child(label),
            );
        }
        Some(
            div()
                .id("image-svg-raster-overlay")
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

#[cfg(test)]
mod tests {
    use super::parse_dimensions;

    #[test]
    fn image_viewer_svg_size_input_trims_and_requires_positive_integer_sides() {
        assert_eq!(parse_dimensions(" 640 ", "\t480\n").unwrap(), (640, 480));
        for (width, height) in [
            ("", "1"),
            ("1", " "),
            ("0", "1"),
            ("1", "0"),
            ("-1", "2"),
            ("2.5", "2"),
            ("1e3", "2"),
            ("1 0", "2"),
            ("4294967296", "1"),
            ("1", "4294967296"),
        ] {
            assert!(
                parse_dimensions(width, height).is_err(),
                "{width:?} × {height:?}"
            );
        }
    }

    #[test]
    fn image_viewer_svg_size_input_checks_side_and_total_pixel_boundaries() {
        assert_eq!(parse_dimensions("65535", "1").unwrap(), (65535, 1));
        assert_eq!(parse_dimensions("1", "65535").unwrap(), (1, 65535));
        assert_eq!(parse_dimensions("8000", "4000").unwrap(), (8000, 4000));
        for (width, height) in [
            ("65536", "1"),
            ("1", "65536"),
            ("8000", "4001"),
            ("65535", "65535"),
            ("4294967295", "4294967295"),
        ] {
            assert!(
                parse_dimensions(width, height).is_err(),
                "{width} × {height}"
            );
        }
    }
}
