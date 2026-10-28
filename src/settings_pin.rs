//! 贴图缩放与标题位置偏好；与贴图菜单／贴图窗口使用同一枚举及持久化字段。
use crate::{pin_zoom::ZoomAnchor, settings_view::SettingsView, settings_widgets as widgets};
use gpui::{prelude::*, AnyElement, Context};

impl SettingsView {
    pub(crate) fn pin_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        for (anchor, id) in [
            (ZoomAnchor::TopLeft, "settings-pin-anchor-top-left"),
            (ZoomAnchor::Pointer, "settings-pin-anchor-pointer"),
            (ZoomAnchor::Center, "settings-pin-anchor-center"),
        ] {
            let selected = self.preferences.pin_zoom_anchor == anchor;
            choices = choices.child(
                widgets::segment(id, anchor.label(), selected, palette, surface)
                    .child(self.settings_probe(id))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        // 显式设置由宿主保存并回滚失败；普通偏好保存只刷新这个字段。
                        if view.save_pin_anchor(anchor) {
                            cx.defer(move |cx| crate::pin_zoom::sync_open_preferences(anchor, cx));
                        }
                        cx.notify();
                    })),
            );
        }
        widgets::row(
            tr!("滚轮缩放中心", "Wheel zoom anchor"),
            Some(tr!("仅影响普通滚轮。中键恢复 100% 时保持鼠标位置；比例菜单保持图片左上角。", "Applies to the plain mouse wheel only. Middle-click reset to 100% keeps the pointer position; the zoom menu keeps the top-left corner.").into()),
            palette,
        )
        .flex_wrap()
        .child(choices)
        .into_any_element()
    }
    pub(crate) fn pin_title_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::pin_title::TitlePosition;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        for (position, id) in [
            (TitlePosition::Bottom, "settings-pin-title-bottom"),
            (TitlePosition::Top, "settings-pin-title-top"),
        ] {
            let selected = self.preferences.pin_title_position == position;
            choices = choices.child(
                widgets::segment(id, position.label(), selected, palette, surface)
                    .child(self.settings_probe(id))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if view.save_pin_title_position(position) {
                            cx.defer(move |cx| {
                                crate::pin_title::sync_open_preferences(position, cx)
                            });
                        }
                        cx.notify();
                    })),
            );
        }
        widgets::row(
            tr!("标题位置", "Title position"),
            Some(tr!("F2 或右键菜单“设置标题…”为贴图加标题，显示在图片外；工具栏展开时隐藏，不进入复制、保存和截图。", "Press F2 or use “Set title…” in the pin menu. The title shows outside the image, hides while the toolbar is open, and never appears in copies, saves or screenshots.").into()),
            palette,
        )
        .flex_wrap()
        .child(choices)
        .into_any_element()
    }
}
