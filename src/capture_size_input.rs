//! 选区精确尺寸：全局物理像素草稿只在确认后应用，字段键不穿透宿主。
use crate::{
    capture_view::{Action, CaptureView},
    geometry::{Rect, ScreenRect},
    i18n::Text,
    scene::SourceIdentity,
    text_field::{TextField, TextFieldEvent},
    toolbar_layout::ToolbarPanel,
};
use gpui::{
    div, prelude::*, px, rgb, Context, Entity, KeyDownEvent, Subscription, WeakEntity, Window,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectionSizeDraft {
    pub x: String,
    pub y: String,
    pub width: String,
    pub height: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SizeInputError {
    Integer(&'static str),
    PositiveSize,
    OutsideDesktop,
    InvalidDesktop,
}

impl SizeInputError {
    fn message(self) -> String {
        match self {
            Self::Integer(field) => trf!("{field} 必须是整数", "{field} must be an integer"),
            Self::PositiveSize => tr!(
                "宽和高必须大于 0",
                "Width and height must be greater than 0"
            )
            .into(),
            Self::OutsideDesktop => tr!(
                "选区超出桌面范围，请调整坐标或尺寸",
                "Selection exceeds desktop bounds; adjust X/Y/W/H"
            )
            .into(),
            Self::InvalidDesktop => tr!(
                "桌面范围已失效，请重新截图",
                "Desktop bounds changed; capture again"
            )
            .into(),
        }
    }
}

impl SelectionSizeDraft {
    pub(crate) fn from_crop(desktop: ScreenRect, crop: Rect) -> Self {
        Self {
            x: (i64::from(desktop.left) + i64::from(crop.x)).to_string(),
            y: (i64::from(desktop.top) + i64::from(crop.y)).to_string(),
            width: crop.width.to_string(),
            height: crop.height.to_string(),
        }
    }

    pub(crate) fn checked_rect(&self, desktop: ScreenRect) -> Result<Rect, SizeInputError> {
        if desktop.width == 0 || desktop.height == 0 {
            return Err(SizeInputError::InvalidDesktop);
        }
        let integer = |text: &str, field| {
            text.trim()
                .parse::<i64>()
                .map_err(|_| SizeInputError::Integer(field))
        };
        let x = integer(&self.x, "X")?;
        let y = integer(&self.y, "Y")?;
        let width = integer(&self.width, "W")?;
        let height = integer(&self.height, "H")?;
        if width <= 0 || height <= 0 {
            return Err(SizeInputError::PositiveSize);
        }
        let local_x = x
            .checked_sub(i64::from(desktop.left))
            .ok_or(SizeInputError::OutsideDesktop)?;
        let local_y = y
            .checked_sub(i64::from(desktop.top))
            .ok_or(SizeInputError::OutsideDesktop)?;
        let rect = Rect {
            x: u32::try_from(local_x).map_err(|_| SizeInputError::OutsideDesktop)?,
            y: u32::try_from(local_y).map_err(|_| SizeInputError::OutsideDesktop)?,
            width: u32::try_from(width).map_err(|_| SizeInputError::OutsideDesktop)?,
            height: u32::try_from(height).map_err(|_| SizeInputError::OutsideDesktop)?,
        };
        if !rect.valid(desktop.width, desktop.height) {
            return Err(SizeInputError::OutsideDesktop);
        }
        Ok(rect)
    }
}

pub(crate) struct SelectionSizeInput {
    owner: WeakEntity<CaptureView>,
    desktop: ScreenRect,
    source: SourceIdentity,
    baseline: Rect,
    fields: [Entity<TextField>; 4],
    error: String,
    done: bool,
    _subscriptions: Vec<Subscription>,
}

impl SelectionSizeInput {
    fn new(
        owner: WeakEntity<CaptureView>,
        desktop: ScreenRect,
        source: SourceIdentity,
        crop: Rect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = SelectionSizeDraft::from_crop(desktop, crop);
        let fields = [
            (draft.x, Text("X", "X")),
            (draft.y, Text("Y", "Y")),
            (draft.width, Text("W", "W")),
            (draft.height, Text("H", "H")),
        ]
        .map(|(text, label)| cx.new(|cx| TextField::new_settings(text, label, cx)));
        let group: Vec<_> = fields.iter().map(Entity::downgrade).collect();
        let mut subscriptions = Vec::with_capacity(fields.len());
        for field in &fields {
            field.update(cx, |v, cx| v.configure_focus(group.clone(), window, cx));
            subscriptions.push(
                cx.subscribe_in(field, window, |v, _, event, w, cx| match event {
                    TextFieldEvent::Changed => {
                        v.error.clear();
                        cx.notify();
                    }
                    TextFieldEvent::Submit => v.submit(w, cx),
                }),
            );
        }
        Self {
            owner,
            desktop,
            source,
            baseline: crop,
            fields,
            error: String::new(),
            done: false,
            _subscriptions: subscriptions,
        }
    }

    fn composing(&self, cx: &Context<Self>) -> bool {
        self.fields
            .iter()
            .any(|field| field.read(cx).is_composing())
    }

    fn draft(&self, cx: &Context<Self>) -> SelectionSizeDraft {
        SelectionSizeDraft {
            x: self.fields[0].read(cx).text().to_owned(),
            y: self.fields[1].read(cx).text().to_owned(),
            width: self.fields[2].read(cx).text().to_owned(),
            height: self.fields[3].read(cx).text().to_owned(),
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.done {
            return;
        }
        if self.composing(cx) {
            self.error = tr!(
                "请先完成输入法候选，再应用尺寸",
                "Confirm the IME candidate before applying"
            )
            .into();
            cx.notify();
            return;
        }
        let requested = match self.draft(cx).checked_rect(self.desktop) {
            Ok(rect) => rect,
            Err(error) => {
                self.error = error.message();
                cx.notify();
                return;
            }
        };
        let desktop = self.desktop;
        let source = self.source.clone();
        let baseline = self.baseline;
        let result = self.owner.update(cx, |view, cx| {
            if view.toolbar_panel != ToolbarPanel::SelectionSize
                || view.source.bounds != desktop
                || SourceIdentity::of(&view.source_pixels()) != source
                || view.saving
                || view.processing
                || view.pin_handoff_pending
                || view.toolbar_drag.is_some()
                || !window.is_window_active()
            {
                return Err(tr!(
                    "截图状态已改变，请重新打开尺寸输入",
                    "Capture changed; reopen the size input"
                ));
            }
            view.editor.apply_precise_crop(baseline, requested)?;
            view.clear_pointer_tracking();
            view.perform(Action::Panel(ToolbarPanel::None), window, cx);
            window.focus(&view.focus);
            cx.notify();
            Ok(())
        });
        match result {
            Ok(Ok(())) => self.done = true,
            Ok(Err(error)) => self.error = error.into(),
            Err(_) => self.error = tr!("截图窗口已关闭", "Capture window closed").into(),
        }
        cx.notify();
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.done {
            return;
        }
        self.done = true;
        let _ = self.owner.update(cx, |view, cx| {
            if view.toolbar_panel == ToolbarPanel::SelectionSize {
                view.perform(Action::Panel(ToolbarPanel::None), window, cx);
                window.focus(&view.focus);
            }
        });
    }

    fn capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if !matches!(key, "enter" | "escape") {
            return;
        }
        if !event.is_held && !event.keystroke.modifiers.modified() && !self.composing(cx) {
            match key {
                "enter" => self.submit(window, cx),
                "escape" => self.cancel(window, cx),
                _ => {}
            }
        }
        // 新按、长按、组合输入或修饰键都不进入宿主 Confirm/Cancel。
        cx.stop_propagation();
    }
}

impl Render for SelectionSizeInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let field = |label: &'static str, input: Entity<TextField>| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(
                    div()
                        .text_size(px(11.))
                        .line_height(px(14.))
                        .text_color(rgb(palette.muted))
                        .child(label),
                )
                .child(input)
        };
        div()
            .id("selection-size-input")
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(12.))
            .line_height(px(16.))
            .capture_key_down(cx.listener(Self::capture_key))
            .on_key_down(cx.listener(|_, _: &KeyDownEvent, _, cx| cx.stop_propagation()))
            .child(
                div()
                    .text_size(px(13.))
                    .line_height(px(16.))
                    .child(tr!("选区位置和尺寸", "Selection position and size")),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(field("X", self.fields[0].clone()))
                    .child(field("Y", self.fields[1].clone())),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(field("W", self.fields[2].clone()))
                    .child(field("H", self.fields[3].clone())),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .line_height(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(tr!(
                        "全局物理像素 · Enter 应用 · Esc 取消",
                        "Physical screen pixels · Enter Apply · Esc Cancel"
                    )),
            )
            .child(
                div()
                    .h(px(26.))
                    .text_size(px(11.))
                    .line_height(px(12.))
                    .text_color(rgb(palette.danger))
                    .child(self.error.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("selection-size-cancel")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(palette.hover))
                            .child(tr!("取消", "Cancel"))
                            .on_click(cx.listener(|v, _, w, cx| {
                                v.cancel(w, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        div()
                            .id("selection-size-apply")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(rgb(palette.accent))
                            .text_color(rgb(palette.on_accent))
                            .child(tr!("应用", "Apply"))
                            .on_click(cx.listener(|v, _, w, cx| {
                                v.submit(w, cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
    }
}

impl CaptureView {
    pub(crate) fn open_selection_size_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(crop) = self.editor.crop else {
            return false;
        };
        if self.pin_mode
            || self.image_canvas.is_some()
            || self.saving
            || self.processing
            || self.pin_handoff_pending
            || self.pointer_down_start.is_some()
            || self.toolbar_drag.is_some()
            || self.editor.local_fill_candidate().is_some()
        {
            return false;
        }
        if let Err(error) = self.editor.apply_precise_crop(crop, crop) {
            self.status = error.into();
            cx.notify();
            return false;
        }
        let desktop = self.source.bounds;
        let source = SourceIdentity::of(&self.source_pixels());
        let owner = cx.entity().downgrade();
        let input = cx.new(|cx| SelectionSizeInput::new(owner, desktop, source, crop, window, cx));
        let focus = input.read(cx).fields[0].read(cx).focus_handle.clone();
        window.focus(&focus);
        self.selection_size_input = Some(input);
        self.status.clear();
        cx.notify();
        true
    }

    pub(crate) fn render_selection_size_popup(&self) -> gpui::AnyElement {
        match &self.selection_size_input {
            Some(input) => div().child(input.clone()).into_any_element(),
            None => div()
                .child(tr!("请先选择截图区域", "Select a capture region first"))
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop() -> ScreenRect {
        ScreenRect {
            left: -1920,
            top: -240,
            width: 4480,
            height: 1680,
        }
    }

    #[test]
    fn selection_size_input_preserves_negative_physical_origin_and_exact_edges() {
        let crop = Rect {
            x: 220,
            y: 140,
            width: 1720,
            height: 1080,
        };
        let draft = SelectionSizeDraft::from_crop(desktop(), crop);
        assert_eq!(draft.x, "-1700");
        assert_eq!(draft.y, "-100");
        assert_eq!(draft.checked_rect(desktop()), Ok(crop));
        assert_eq!(
            SelectionSizeDraft::from_crop(desktop(), Rect::full(4480, 1680))
                .checked_rect(desktop()),
            Ok(Rect::full(4480, 1680))
        );
        let edge = Rect {
            x: 4479,
            y: 1679,
            width: 1,
            height: 1,
        };
        assert_eq!(
            SelectionSizeDraft::from_crop(desktop(), edge).checked_rect(desktop()),
            Ok(edge)
        );
    }

    #[test]
    fn selection_size_input_rejects_bad_numbers_and_overflow_without_clamping() {
        let base = SelectionSizeDraft::from_crop(desktop(), Rect::full(120, 90));
        for (field, value, error) in [
            (0, "-1921", SizeInputError::OutsideDesktop),
            (1, "-241", SizeInputError::OutsideDesktop),
            (2, "0", SizeInputError::PositiveSize),
            (3, "-2", SizeInputError::PositiveSize),
            (2, "4481", SizeInputError::OutsideDesktop),
            (3, "1681", SizeInputError::OutsideDesktop),
            (0, "9223372036854775807", SizeInputError::OutsideDesktop),
            (1, "-9223372036854775808", SizeInputError::OutsideDesktop),
            (2, "1.5", SizeInputError::Integer("W")),
            (0, "", SizeInputError::Integer("X")),
            (2, "99999999999999999999", SizeInputError::Integer("W")),
        ] {
            let mut draft = base.clone();
            match field {
                0 => draft.x = value.into(),
                1 => draft.y = value.into(),
                2 => draft.width = value.into(),
                _ => draft.height = value.into(),
            }
            assert_eq!(draft.checked_rect(desktop()), Err(error));
        }
        assert_eq!(
            base.checked_rect(ScreenRect {
                width: 0,
                ..desktop()
            }),
            Err(SizeInputError::InvalidDesktop)
        );
    }
}
