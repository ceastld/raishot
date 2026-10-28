//! 图片查看窗顶栏：按“打开与翻页 · 缩放与旋转 · 动图／多页 · 图片工具 · 复制与更多”分组的图标按钮，
//! 悬停提示带快捷键；文件类操作收进“更多”菜单。
use super::{animation, ImageViewer, LoadedImage, PlaybackState, SnapshotTarget};
use crate::{capture_toolbar::ui_kit, toolbar_icons::Icon};
use gpui::{div, prelude::*, px, rgb, AnyElement, Context, MouseButton, SharedString, Window};

/// 顶栏高度；图片区域、最近列表和属性面板都排在它下面。
pub(super) const TOOLBAR_HEIGHT: f32 = 52.;
const BUTTON: f32 = 32.;
const MENU_WIDTH: f32 = 228.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Act {
    Open,
    Recent,
    Previous,
    Next,
    Fit,
    Actual,
    Fullscreen,
    RotateLeft,
    RotateRight,
    Grid,
    Pick,
    Edit,
    Crop,
    Ocr,
    Pin,
    Tools,
    CopyImage,
    Drag,
    More,
    Properties,
    SvgSize,
    CopyPath,
    CopyFile,
    Rename,
    Recycle,
    Reveal,
    Retry,
    Cancel,
    BrowsePages,
    Play,
    Step,
    Restart,
    Input,
}

impl ImageViewer {
    fn pixels_ready(&self) -> bool {
        self.image
            .as_ref()
            .is_some_and(LoadedImage::has_full_pixels)
    }

    fn act_enabled(&self, act: Act) -> bool {
        if self.modal_is_open() {
            return false;
        }
        let tools = self.pixels_ready() && !self.loading && !self.snapshot_pending;
        let file = self.image.is_some() && self.source_file.is_some() && !self.loading;
        match act {
            Act::Fullscreen | Act::Tools if self.popup => false,
            Act::Previous => self
                .collection
                .as_ref()
                .is_some_and(|c| c.neighbor(-1).is_some()),
            Act::Next => self
                .collection
                .as_ref()
                .is_some_and(|c| c.neighbor(1).is_some()),
            Act::Fit | Act::Actual => self.image.is_some(),
            Act::RotateLeft | Act::RotateRight => {
                self.pixels_ready()
                    && !self.animation.as_ref().is_some_and(|snapshot| {
                        matches!(
                            snapshot.state,
                            PlaybackState::Failed | PlaybackState::Cancelled
                        )
                    })
            }
            Act::Grid | Act::Pick => self.pixels_ready(),
            Act::Edit | Act::Crop | Act::Ocr | Act::Pin | Act::Tools | Act::CopyImage => tools,
            Act::SvgSize => self.source_file.is_some() && !self.loading && !self.snapshot_pending,
            Act::CopyPath | Act::Reveal => self.path.is_some(),
            Act::CopyFile => self.source_file.is_some() && !self.loading,
            Act::Rename | Act::Recycle => file && !self.snapshot_pending,
            Act::Drag => file && self.file_drag_request.is_none(),
            Act::Play | Act::Step | Act::Input => {
                !self.animation.as_ref().is_some_and(|snapshot| {
                    matches!(
                        snapshot.state,
                        PlaybackState::Failed | PlaybackState::Cancelled
                    )
                })
            }
            _ => true,
        }
    }

    fn act(
        &mut self,
        act: Act,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if act != Act::More {
            self.more_open = false;
        }
        if !self.act_enabled(act) {
            return;
        }
        match act {
            Act::Open => self.choose(cx),
            Act::Recent => self.recent.toggle(),
            Act::Previous => self.navigate(-1, cx),
            Act::Next => self.navigate(1, cx),
            Act::Fit => {
                self.auto_fit = true;
                self.fit(window);
            }
            Act::Actual => self.zoom(1., self.area_center(window)),
            Act::Fullscreen => window.toggle_fullscreen(),
            Act::RotateLeft => self.rotate(false, cx),
            Act::RotateRight => self.rotate(true, cx),
            Act::Grid => self.show_grid = !self.show_grid,
            Act::Pick => self.picking = !self.picking,
            Act::Edit => self.edit(cx),
            Act::Crop => self.snapshot(SnapshotTarget::Crop, cx),
            Act::Ocr => self.snapshot(SnapshotTarget::Ocr, cx),
            Act::Pin => self.pin(cx),
            Act::Tools => self.snapshot(SnapshotTarget::Tools, cx),
            Act::CopyImage => self.copy_image(window, cx),
            Act::Drag => self.arm_file_drag(position, window, cx),
            Act::More => self.more_open = !self.more_open,
            Act::Properties => self.show_properties = !self.show_properties,
            Act::SvgSize => self.show_svg_dialog(window, cx),
            Act::CopyPath => self.copy_path(cx),
            Act::CopyFile => self.file_action(true, window, cx),
            Act::Rename => self.show_file_dialog(true, window, cx),
            Act::Recycle => self.show_file_dialog(false, window, cx),
            Act::Reveal => self.file_action(false, window, cx),
            Act::Retry => {
                if let Some(path) = self.path.clone() {
                    self.open_path(path, true, cx);
                }
            }
            Act::Cancel => self.cancel(cx),
            Act::BrowsePages => {
                if let Some(path) = self.path.clone() {
                    self.open_path_page(path, false, Some(0), cx);
                }
            }
            Act::Play => self.animation_control(animation::Action::Toggle, cx),
            Act::Step => self.animation_control(animation::Action::Step, cx),
            Act::Restart => self.animation_control(animation::Action::Restart, cx),
            Act::Input => self.animation_control(animation::Action::Input, cx),
        }
    }

    /// 32×32 图标按钮；开关类选中时用强调色底。
    fn icon_button(
        &self,
        id: &'static str,
        icon: Icon,
        tip: impl Into<SharedString>,
        act: Act,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let enabled = self.act_enabled(act);
        let tip: SharedString = tip.into();
        div()
            .id(id)
            .size(px(BUTTON))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .when(active, |button| button.bg(rgb(palette.selected)))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.hover)))
            })
            .tooltip(ui_kit::tip(tip))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&view.focus);
                    view.act(act, event.position, window, cx);
                    cx.notify();
                }),
            )
            .child(
                crate::toolbar_icons::action(
                    icon,
                    if !enabled {
                        palette.border
                    } else if active {
                        palette.accent
                    } else {
                        palette.foreground
                    },
                )
                .size(px(18.)),
            )
            .into_any_element()
    }

    /// 带边框的文字按钮，用于重试、取消加载等临时操作。
    fn text_button(
        &self,
        id: &'static str,
        label: impl Into<SharedString>,
        act: Act,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let enabled = self.act_enabled(act);
        div()
            .id(id)
            .h(px(28.))
            .px(px(10.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(palette.border))
            .text_size(px(12.))
            .text_color(rgb(if enabled {
                palette.foreground
            } else {
                palette.muted
            }))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.hover)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&view.focus);
                    view.act(act, event.position, window, cx);
                    cx.notify();
                }),
            )
            .child(label.into())
            .into_any_element()
    }

    pub(super) fn controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let separator = || {
            div()
                .w(px(1.))
                .h(px(20.))
                .mx(px(6.))
                .flex_shrink_0()
                .bg(rgb(palette.border))
                .into_any_element()
        };
        let position = self
            .collection
            .as_ref()
            .map(|collection| {
                let (index, count) = collection.position();
                format!("{index} / {count}")
            })
            .unwrap_or_default();

        let mut left = div().flex().items_center().gap(px(2.)).flex_shrink_0();
        left = left
            .child(self.icon_button(
                "open",
                Icon::Folder,
                tr!("打开图片 · Ctrl+O", "Open image · Ctrl+O"),
                Act::Open,
                false,
                cx,
            ))
            .child(self.icon_button(
                "recent",
                Icon::History,
                tr!("最近打开 · Ctrl+R", "Recent · Ctrl+R"),
                Act::Recent,
                self.recent.is_open(),
                cx,
            ))
            .child(separator())
            .child(self.icon_button(
                "previous",
                Icon::ChevronLeft,
                tr!("上一张 · ←", "Previous · ←"),
                Act::Previous,
                false,
                cx,
            ))
            .child(
                div()
                    .min_w(px(44.))
                    .flex_shrink_0()
                    .text_center()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(position),
            )
            .child(self.icon_button(
                "next",
                Icon::ChevronRight,
                tr!("下一张 · →", "Next · →"),
                Act::Next,
                false,
                cx,
            ))
            .child(separator())
            .child(self.icon_button(
                "fit",
                Icon::Fit,
                tr!("适应窗口 · 0", "Fit to window · 0"),
                Act::Fit,
                self.auto_fit && self.image.is_some(),
                cx,
            ))
            .child(self.text_zoom_button(cx))
            .when(!self.popup, |bar| {
                bar.child(self.icon_button(
                    "fullscreen",
                    Icon::Fullscreen,
                    tr!("全屏 · F11", "Full screen · F11"),
                    Act::Fullscreen,
                    false,
                    cx,
                ))
            })
            .child(separator())
            .child(self.icon_button(
                "rotate-left",
                Icon::RotateLeft,
                tr!("向左旋转 · [", "Rotate left · ["),
                Act::RotateLeft,
                false,
                cx,
            ))
            .child(self.icon_button(
                "rotate-right",
                Icon::RotateRight,
                tr!("向右旋转 · ]", "Rotate right · ]"),
                Act::RotateRight,
                false,
                cx,
            ))
            .child(self.icon_button(
                "image-pixel-grid",
                Icon::Grid,
                tr!(
                    "像素网格（放大 8 倍以上显示） · G",
                    "Pixel grid (from 8×) · G"
                ),
                Act::Grid,
                self.show_grid,
                cx,
            ))
            .child(self.icon_button(
                "image-preview-color",
                Icon::Eyedropper,
                tr!("预览取色，按 C 复制颜色 · K", "Pick color, C to copy · K"),
                Act::Pick,
                self.picking,
                cx,
            ));

        // 动图、多页与加载状态只在需要时出现。
        let mut context = div().flex().items_center().gap(px(2.)).flex_shrink_0();
        let mut has_context = false;
        if let Some(snapshot) = &self.animation {
            has_context = true;
            let playing = matches!(
                snapshot.state,
                PlaybackState::Playing | PlaybackState::WaitingForInput
            );
            context = context
                .child(self.icon_button(
                    "play",
                    if playing { Icon::Pause } else { Icon::Play },
                    if playing {
                        tr!("暂停 · Space", "Pause · Space")
                    } else {
                        tr!("播放 · Space", "Play · Space")
                    },
                    Act::Play,
                    false,
                    cx,
                ))
                .child(self.icon_button(
                    "step",
                    Icon::Step,
                    tr!("下一帧 · .", "Next frame · ."),
                    Act::Step,
                    false,
                    cx,
                ))
                .child(self.icon_button(
                    "restart",
                    Icon::Refresh,
                    tr!("重播 · R", "Restart · R"),
                    Act::Restart,
                    false,
                    cx,
                ));
            if self
                .image
                .as_ref()
                .and_then(|image| image.raw_frame.as_ref())
                .is_some_and(|frame| frame.needs_user_input())
            {
                context = context.child(self.text_button(
                    "input",
                    tr!("继续 · Enter", "Continue · Enter"),
                    Act::Input,
                    cx,
                ));
            }
            let state = match snapshot.state {
                PlaybackState::Loading => tr!("加载中", "Loading"),
                PlaybackState::Playing => tr!("播放中", "Playing"),
                PlaybackState::Paused => tr!("已暂停", "Paused"),
                PlaybackState::WaitingForInput => tr!("等待输入", "Waiting"),
                PlaybackState::Ended => tr!("已结束", "Ended"),
                PlaybackState::Cancelled => tr!("已取消", "Cancelled"),
                PlaybackState::Failed => tr!("播放失败", "Failed"),
            };
            context = context.child(
                div()
                    .px(px(6.))
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(trf!(
                        "{state} · 第 {} 帧",
                        "{state} · frame {}",
                        snapshot.frame_index.map_or(0, |index| index + 1)
                    )),
            );
        }
        if self.pages.as_ref().is_some_and(|info| info.count() != 0) {
            has_context = true;
            context = context.child(self.icon_controls(cx));
        } else if self.source_kind.is_some_and(|kind| {
            matches!(
                kind,
                super::pages::Kind::Tiff | super::pages::Kind::System(_)
            )
        }) {
            has_context = true;
            context = context.child(self.text_button(
                "image-tiff-browse-pages",
                if self.source_kind == Some(super::pages::Kind::Tiff) {
                    tr!("浏览页", "Browse pages")
                } else {
                    tr!("原主图", "Primary image")
                },
                Act::BrowsePages,
                cx,
            ));
        }
        if self.loading {
            has_context = true;
            context = context.child(self.text_button(
                "cancel",
                if self.popup {
                    tr!("取消加载", "Cancel")
                } else {
                    tr!("取消加载 · Esc", "Cancel · Esc")
                },
                Act::Cancel,
                cx,
            ));
        } else if self.error.is_some() && self.path.is_some() && self.image.is_none() {
            has_context = true;
            context = context.child(self.text_button(
                "retry",
                tr!("重试 · F5", "Retry · F5"),
                Act::Retry,
                cx,
            ));
        }

        let right = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .flex_shrink_0()
            .child(self.icon_button(
                "edit",
                Icon::Pen,
                tr!("编辑标注 · E", "Edit · E"),
                Act::Edit,
                false,
                cx,
            ))
            .child(self.icon_button(
                "crop",
                Icon::Crop,
                tr!("裁剪 · X", "Crop · X"),
                Act::Crop,
                false,
                cx,
            ))
            .child(self.icon_button(
                "ocr",
                Icon::Ocr,
                tr!("识别文字 · O", "Recognize text · O"),
                Act::Ocr,
                false,
                cx,
            ))
            .child(self.icon_button(
                "pin",
                Icon::Pin,
                tr!("贴到屏幕 · P", "Pin to screen · P"),
                Act::Pin,
                false,
                cx,
            ))
            .when(!self.popup, |bar| {
                bar.child(self.icon_button(
                    "tools",
                    Icon::Sliders,
                    tr!(
                        "图片工具：调整、拼接、导出等",
                        "Image tools: adjust, combine, export"
                    ),
                    Act::Tools,
                    false,
                    cx,
                ))
            })
            .child(separator())
            .child(self.icon_button(
                "copy-image",
                Icon::Copy,
                if self.animation.is_some() {
                    tr!("复制当前帧 · Ctrl+C", "Copy frame · Ctrl+C")
                } else {
                    tr!("复制图片 · Ctrl+C", "Copy image · Ctrl+C")
                },
                Act::CopyImage,
                false,
                cx,
            ))
            .child(self.icon_button(
                "drag-file",
                Icon::Grip,
                tr!(
                    "拖出源文件 · 按住拖到其它程序",
                    "Drag file · hold and drop into another app"
                ),
                Act::Drag,
                false,
                cx,
            ))
            .child(self.icon_button(
                "more",
                Icon::More,
                tr!(
                    "更多：属性、复制路径、重命名…",
                    "More: info, copy path, rename…"
                ),
                Act::More,
                self.more_open,
                cx,
            ));

        div()
            .id("image-viewer-controls")
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(px(TOOLBAR_HEIGHT))
            .px(px(10.))
            .flex()
            .items_center()
            .overflow_x_scroll()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .bg(rgb(palette.background))
            .border_b_1()
            .border_color(rgb(palette.border))
            .child(left)
            .when(has_context, |bar| bar.child(separator()).child(context))
            .child(div().flex_1().min_w(px(8.)))
            .child(right)
            .into_any_element()
    }

    /// “1:1” 文字按钮；当前正好 100% 时高亮。
    fn text_zoom_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let enabled = self.act_enabled(Act::Actual);
        let active = enabled && (self.displayed_zoom() - 1.).abs() < 0.001;
        div()
            .id("actual")
            .h(px(BUTTON))
            .min_w(px(BUTTON))
            .px(px(4.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(rgb(if !enabled {
                palette.border
            } else if active {
                palette.accent
            } else {
                palette.foreground
            }))
            .when(active, |button| button.bg(rgb(palette.selected)))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.hover)))
            })
            .tooltip(ui_kit::tip(tr!("原始大小 · 1", "Actual size · 1")))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&view.focus);
                    view.act(Act::Actual, event.position, window, cx);
                    cx.notify();
                }),
            )
            .child("1:1")
            .into_any_element()
    }

    /// “更多”菜单：文件与信息类操作；点菜单外任意处关闭。
    pub(super) fn more_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.more_open {
            return None;
        }
        let palette = crate::theme::palette(cx);
        let svg = self
            .image
            .as_ref()
            .is_some_and(|image| image.metadata.format == crate::image_metadata::SourceFormat::Svg);
        let mut rows = div().flex().flex_col().gap(px(2.));
        let items: [(&'static str, Icon, &'static str, Option<&'static str>, Act); 7] = [
            (
                "more-properties",
                Icon::File,
                tr!("图片信息", "Image info"),
                Some("I"),
                Act::Properties,
            ),
            (
                "copy-path",
                Icon::Copy,
                tr!("复制路径", "Copy path"),
                Some("Ctrl+Shift+C"),
                Act::CopyPath,
            ),
            (
                "copy-file",
                Icon::Duplicate,
                tr!("复制文件", "Copy file"),
                None,
                Act::CopyFile,
            ),
            (
                "reveal",
                Icon::Folder,
                tr!("在文件夹中显示", "Show in folder"),
                None,
                Act::Reveal,
            ),
            (
                "rename-file",
                Icon::Text,
                tr!("重命名…", "Rename…"),
                None,
                Act::Rename,
            ),
            (
                "svg-size",
                Icon::Crop,
                tr!("栅格尺寸…", "Raster size…"),
                None,
                Act::SvgSize,
            ),
            (
                "recycle-file",
                Icon::Trash,
                tr!("移到回收站…", "Move to Recycle Bin…"),
                None,
                Act::Recycle,
            ),
        ];
        for (id, icon, label, key, act) in items {
            if act == Act::SvgSize && !svg {
                continue;
            }
            let enabled = self.act_enabled(act);
            let color = if act == Act::Recycle && enabled {
                palette.danger
            } else if enabled {
                palette.muted
            } else {
                palette.border
            };
            rows = rows.child(
                ui_kit::menu_row(
                    palette,
                    id,
                    Some(crate::toolbar_icons::action(icon, color)),
                    label,
                    key.map(|key| ui_kit::trailing_text(palette, key)),
                    act == Act::Properties && self.show_properties,
                    enabled,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        view.act(act, event.position, window, cx);
                        cx.notify();
                    }),
                ),
            );
        }
        let backdrop = div()
            .id("image-viewer-more-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    cx.stop_propagation();
                    view.more_open = false;
                    cx.notify();
                }),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation());
        let panel = div()
            .id("image-viewer-more")
            .absolute()
            .top(px(TOOLBAR_HEIGHT + 4.))
            .right(px(10.))
            .w(px(MENU_WIDTH))
            .p(px(ui_kit::MENU_PAD))
            .rounded(px(8.))
            .bg(rgb(palette.background))
            .border_1()
            .border_color(rgb(palette.border))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(rows);
        Some(
            div()
                .absolute()
                .size_full()
                .child(backdrop)
                .child(panel)
                .into_any_element(),
        )
    }
}
