//! 本窗口的高级 H.264 请求输入；保留原文，显示解析预览并在开始录制时重新校验。
use super::{
    quality_draft::{
        NumberMode, OptionalMode, OptionalNumberDraft, QualityDraft, ResolvedDraftRequest,
    },
    Flyout, RecordingView, Stage,
};
use crate::{
    capture_toolbar::Palette,
    geometry::ScreenRect,
    recording_options::{
        advanced::{Override, ResolvedQualityRequest},
        Preset, QualityProfile, RateControl, RecordingOptions,
    },
    recording_size_layout::RecordingSizeLayout,
    text_field::{TextField, TextFieldEvent},
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, App, AppContext, Context, Div, Entity, SharedString, Subscription,
    Window,
};
use std::cell::Cell;

pub(super) struct QualityEditor {
    pub draft: QualityDraft,
    pub error: Option<String>,
    last_context: Option<(ScreenRect, u32, Preset, Option<QualityProfile>, bool)>,
    resize_width: Entity<TextField>,
    resize_height: Entity<TextField>,
    mean: Entity<TextField>,
    speed: Entity<TextField>,
    peak: Entity<TextField>,
    buffer: Entity<TextField>,
    gop: Entity<TextField>,
    enabled_mask: Cell<u8>,
    focus_configured: bool,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy)]
enum OptionalField {
    Peak,
    Buffer,
    Gop,
}

impl QualityEditor {
    pub(super) fn new(cx: &mut Context<RecordingView>) -> Self {
        let field = |placeholder, cx: &mut Context<RecordingView>| {
            cx.new(|cx| {
                let mut field = TextField::new_settings(String::new(), placeholder, cx);
                field.set_enabled(false, cx);
                field
            })
        };
        let resize_width = field(crate::i18n::Text("例：960", "e.g. 960"), cx);
        let resize_height = field(crate::i18n::Text("例：540", "e.g. 540"), cx);
        let mean = field(crate::i18n::Text("例：2.5", "e.g. 2.5"), cx);
        let speed = field(crate::i18n::Text("0–100 整数", "Integer 0–100"), cx);
        let peak = field(crate::i18n::Text("例：4", "e.g. 4"), cx);
        let buffer = field(crate::i18n::Text("例：4000", "e.g. 4000"), cx);
        let gop = field(crate::i18n::Text("例：60", "e.g. 60"), cx);
        let mut subscriptions = Vec::new();
        for field in [
            &resize_width,
            &resize_height,
            &mean,
            &speed,
            &peak,
            &buffer,
            &gop,
        ] {
            subscriptions.push(cx.subscribe(
                field,
                |view: &mut RecordingView, _, event: &TextFieldEvent, cx| {
                    if matches!(event, TextFieldEvent::Changed) {
                        view.quality_editor.error = None;
                        cx.notify();
                    }
                },
            ));
        }
        Self {
            draft: QualityDraft::default(),
            error: None,
            last_context: None,
            resize_width,
            resize_height,
            mean,
            speed,
            peak,
            buffer,
            gop,
            enabled_mask: Cell::new(0),
            focus_configured: false,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn request(
        &mut self,
        region: ScreenRect,
        options: &RecordingOptions,
        profile: Option<QualityProfile>,
        experimental: bool,
        cx: &App,
    ) -> Result<Option<ResolvedQualityRequest>> {
        self.request_with_resize(region, options, profile, experimental, cx)
            .map(|request| request.map(|request| request.quality))
    }

    pub(super) fn request_with_resize(
        &mut self,
        region: ScreenRect,
        options: &RecordingOptions,
        profile: Option<QualityProfile>,
        experimental: bool,
        cx: &App,
    ) -> Result<Option<ResolvedDraftRequest>> {
        self.refresh_context(region, options, profile, experimental);
        // 不 trim 或改写输入；未应用的坏值也保留在草稿里。
        self.draft = self.read_draft(cx);
        if experimental && !options.preset.is_gif() && self.draft.enabled && self.composing(cx) {
            let message = tr!(
                "请先完成或取消输入法组合，再开始录制。",
                "Finish or cancel text composition before recording."
            );
            self.error = Some(message.to_owned());
            return Err(message.into());
        }
        let result = self
            .draft
            .resolve_request(region, options, profile, experimental);
        self.error = result.as_ref().err().map(ToString::to_string);
        result
    }

    pub(super) fn refresh_context(
        &mut self,
        region: ScreenRect,
        options: &RecordingOptions,
        profile: Option<QualityProfile>,
        experimental: bool,
    ) {
        let context = (region, options.fps, options.preset, profile, experimental);
        if self.last_context != Some(context) {
            self.error = None;
            self.last_context = Some(context);
        }
    }

    fn read_draft(&self, cx: &App) -> QualityDraft {
        let mut draft = self.draft.clone();
        draft.resize.width_text = self.resize_width.read(cx).text().to_owned();
        draft.resize.height_text = self.resize_height.read(cx).text().to_owned();
        draft.mean_mbps.text = self.mean.read(cx).text().to_owned();
        draft.quality_vs_speed.text = self.speed.read(cx).text().to_owned();
        draft.peak_mbps.text = self.peak.read(cx).text().to_owned();
        draft.buffer_kbits.text = self.buffer.read(cx).text().to_owned();
        draft.gop_frames.text = self.gop.read(cx).text().to_owned();
        draft
    }

    fn fields(&self) -> [&Entity<TextField>; 7] {
        [
            &self.resize_width,
            &self.resize_height,
            &self.mean,
            &self.speed,
            &self.peak,
            &self.buffer,
            &self.gop,
        ]
    }

    pub(super) fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.fields()
            .into_iter()
            .any(|field| field.read(cx).focus_handle.is_focused(window))
    }

    pub(super) fn composing(&self, cx: &App) -> bool {
        self.fields()
            .into_iter()
            .any(|field| field.read(cx).is_composing())
    }

    fn configure_focus(&mut self, window: &mut Window, cx: &mut Context<RecordingView>) {
        if self.focus_configured {
            return;
        }
        let group = vec![
            self.resize_width.downgrade(),
            self.resize_height.downgrade(),
            self.mean.downgrade(),
            self.speed.downgrade(),
            self.peak.downgrade(),
            self.buffer.downgrade(),
            self.gop.downgrade(),
        ];
        for field in self.fields() {
            field.update(cx, |field, cx| {
                field.configure_focus(group.clone(), window, cx)
            });
        }
        self.focus_configured = true;
    }

    fn sync_enabled(&self, available: bool, cx: &mut Context<RecordingView>) {
        let active = available && self.draft.enabled;
        let enabled = [
            active && self.draft.resize.mode == NumberMode::Set,
            active && self.draft.resize.mode == NumberMode::Set,
            active && self.draft.mean_mbps.mode == NumberMode::Set,
            active && self.draft.quality_vs_speed.mode == NumberMode::Set,
            active && self.draft.peak_mbps.mode == OptionalMode::Set,
            active && self.draft.buffer_kbits.mode == OptionalMode::Set,
            active && self.draft.gop_frames.mode == OptionalMode::Set,
        ];
        let before = self.enabled_mask.get();
        let mut after = 0;
        for (index, field) in self.fields().into_iter().enumerate() {
            let flag = 1 << index;
            if enabled[index] {
                after |= flag;
            }
            if (before & flag != 0) != enabled[index] {
                field.update(cx, |field, cx| field.set_enabled(enabled[index], cx));
            }
        }
        self.enabled_mask.set(after);
    }

    fn optional(&self, field: OptionalField) -> (&OptionalNumberDraft, &Entity<TextField>) {
        match field {
            OptionalField::Peak => (&self.draft.peak_mbps, &self.peak),
            OptionalField::Buffer => (&self.draft.buffer_kbits, &self.buffer),
            OptionalField::Gop => (&self.draft.gop_frames, &self.gop),
        }
    }

    fn optional_mut(&mut self, field: OptionalField) -> &mut OptionalNumberDraft {
        match field {
            OptionalField::Peak => &mut self.draft.peak_mbps,
            OptionalField::Buffer => &mut self.draft.buffer_kbits,
            OptionalField::Gop => &mut self.draft.gop_frames,
        }
    }

    fn reset(&mut self, cx: &mut Context<RecordingView>) {
        self.draft = QualityDraft::default();
        self.error = None;
        for field in self.fields() {
            field.update(cx, |field, cx| field.set_text(String::new(), cx));
        }
        self.sync_enabled(false, cx);
    }
}

fn available(view: &RecordingView) -> bool {
    view.stage == Stage::Idle
        && crate::file_launch_command::experimental_media_enabled()
        && !view.options.preset.is_gif()
}

fn editable(view: &RecordingView) -> bool {
    available(view) && view.quality_editor.draft.enabled
}

fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    selected: bool,
    enabled: bool,
    p: Palette,
) -> gpui::Stateful<Div> {
    let label: SharedString = label.into();
    div()
        .id(id)
        .px(px(8.))
        .py(px(5.))
        .rounded_md()
        .border_1()
        .border_color(rgb(if selected { p.accent } else { p.border }))
        .bg(rgb(if selected { p.selected } else { p.background }))
        .text_size(px(11.))
        .text_color(rgb(if selected { p.accent } else { p.foreground }))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(move |button| button.bg(rgb(p.hover)))
        })
        .when(!enabled, |button| button.opacity(0.45))
        .child(label)
        .when(
            cfg!(test)
                && matches!(
                    id,
                    "rec-quality-enabled"
                        | "rec-quality-size-set"
                        | "rec-quality-size-inherit"
                        | "rec-quality-reset"
                        | "rec-quality-close"
                ),
            |button| button.relative().child(super::bounds_probe(id)),
        )
}

fn section(
    title: &'static str,
    hint: &'static str,
    field: Entity<TextField>,
    modes: Div,
    p: Palette,
) -> Div {
    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(div().text_size(px(12.)).child(title))
        .child(modes)
        .child(field)
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(p.muted))
                .child(hint),
        )
}

impl RecordingView {
    pub(super) fn advanced_quality_panel(&self, p: Palette, cx: &mut Context<Self>) -> Div {
        let can_edit = available(self);
        let active = can_edit && self.quality_editor.draft.enabled;
        self.quality_editor.sync_enabled(can_edit, cx);
        let resize_width = self.quality_editor.resize_width.clone().into_any_element();
        #[cfg(test)]
        let resize_width = div()
            .relative()
            .child(resize_width)
            .child(super::bounds_probe("rec-quality-size-width"))
            .into_any_element();
        let source_dimensions = match RecordingSizeLayout::new(self.region) {
            Ok(layout) => {
                let mut text = trf!(
                    "裁边后的有效源范围 {}×{} · {} FPS",
                    "Cropped source extent {}×{} · {} FPS",
                    layout.encoded_width,
                    layout.encoded_height,
                    self.options.fps
                );
                if layout.cropped_right() != 0 || layout.cropped_bottom() != 0 {
                    text.push_str(&trf!(
                        "；选区 {}×{}，右裁 {}、底裁 {} 像素",
                        "; selection {}×{}, cropping {} px from right and {} px from bottom",
                        layout.source_width,
                        layout.source_height,
                        layout.cropped_right(),
                        layout.cropped_bottom()
                    ));
                }
                text
            }
            Err(error) => trf!(
                "选区暂不可录制：{error}",
                "Selection cannot be recorded: {error}"
            ),
        };
        let target_dimensions = if !active {
            tr!(
                "本草稿的目标请求未应用",
                "This draft's target request is not applied"
            )
            .to_owned()
        } else if self.quality_editor.composing(cx) {
            tr!(
                "输入法组合中，目标请求尚未解析",
                "Finish text composition to resolve the target request"
            )
            .to_owned()
        } else {
            match self.quality_editor.read_draft(cx).resolve_request(
                self.region,
                &self.options,
                self.quality_profile,
                can_edit,
            ) {
                Ok(Some(request)) => {
                    let mode = if request.resize.is_some() {
                        tr!("显式设置", "Explicit")
                    } else {
                        tr!("继承源范围", "Inherit source extent")
                    };
                    trf!(
                        "已解析目标请求（{mode}）：{}×{} · {} FPS",
                        "Resolved target request ({mode}): {}×{} · {} FPS",
                        request.quality.plan.width,
                        request.quality.plan.height,
                        request.quality.plan.fps
                    )
                }
                Ok(None) => tr!(
                    "本草稿的目标请求未应用",
                    "This draft's target request is not applied"
                )
                .to_owned(),
                Err(error) => trf!(
                    "高级请求尚未通过校验：{error}",
                    "Advanced request is not yet valid: {error}"
                ),
            }
        };
        let mut resize_modes = div().flex().gap(px(5.));
        for (mode, label, id) in [
            (
                NumberMode::Inherit,
                tr!("继承", "Inherit"),
                "rec-quality-size-inherit",
            ),
            (NumberMode::Set, tr!("设置", "Set"), "rec-quality-size-set"),
        ] {
            resize_modes = resize_modes.child(
                button(
                    id,
                    label,
                    self.quality_editor.draft.resize.mode == mode,
                    active,
                    p,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    if editable(view) {
                        view.quality_editor.configure_focus(window, cx);
                        view.quality_editor.draft.resize.mode = mode;
                        view.quality_editor.error = None;
                        view.quality_editor.sync_enabled(true, cx);
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            );
        }
        let mut mean_modes = div().flex().gap(px(5.));
        for (mode, label, id) in [
            (
                NumberMode::Inherit,
                tr!("继承", "Inherit"),
                "rec-quality-mean-inherit",
            ),
            (NumberMode::Set, tr!("设置", "Set"), "rec-quality-mean-set"),
        ] {
            mean_modes = mean_modes.child(
                button(
                    id,
                    label,
                    self.quality_editor.draft.mean_mbps.mode == mode,
                    active,
                    p,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    if editable(view) {
                        view.quality_editor.configure_focus(window, cx);
                        view.quality_editor.draft.mean_mbps.mode = mode;
                        view.quality_editor.error = None;
                        view.quality_editor.sync_enabled(true, cx);
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            );
        }
        let mut speed_modes = div().flex().gap(px(5.));
        for (mode, label, id) in [
            (
                NumberMode::Inherit,
                tr!("继承", "Inherit"),
                "rec-quality-speed-inherit",
            ),
            (NumberMode::Set, tr!("设置", "Set"), "rec-quality-speed-set"),
        ] {
            speed_modes = speed_modes.child(
                button(
                    id,
                    label,
                    self.quality_editor.draft.quality_vs_speed.mode == mode,
                    active,
                    p,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    if editable(view) {
                        view.quality_editor.configure_focus(window, cx);
                        view.quality_editor.draft.quality_vs_speed.mode = mode;
                        view.quality_editor.error = None;
                        view.quality_editor.sync_enabled(true, cx);
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            );
        }
        let mut rows = div()
            .id("rec-advanced-quality-rows")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(14.))
            .px(px(4.))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(source_dimensions),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(target_dimensions),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .child(div().text_size(px(12.)).child(tr!("输出尺寸请求 · 像素", "Output size request · pixels")))
                    .child(resize_modes)
                    .child(
                        div().flex().gap(px(8.))
                            .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap(px(4.))
                                .child(tr!("宽", "Width"))
                                .child(resize_width))
                            .child(div().flex_1().min_w(px(0.)).flex().flex_col().gap(px(4.))
                                .child(tr!("高", "Height"))
                                .child(self.quality_editor.resize_height.clone())),
                    )
                    .child(div().text_size(px(10.)).text_color(rgb(p.muted)).child(tr!(
                        "宽高独立设置，不锁定比例。填写偶数像素整数，逐轴不超过有效源范围；目标不会自动裁边。",
                        "Width and height are independent, with no aspect lock. Use even pixel integers no larger than the source on either axis; targets are not cropped automatically."
                    ))),
            )
            .child(section(
                tr!("平均码率 · Mbps", "Mean bitrate · Mbps"),
                tr!(
                    "十进制 Mbps；例如 2.5 表示 2,500,000 bit/s。",
                    "Decimal Mbps; 2.5 means 2,500,000 bit/s."
                ),
                self.quality_editor.mean.clone(),
                mean_modes,
                p,
            ))
            .child(section(
                tr!("质量/速度权衡 · 0–100", "Quality vs speed · 0–100"),
                tr!(
                    "仅请求：0 偏速度，100 偏质量；不代表实际收益或恒定画质。",
                    "Request only: 0 favors speed, 100 favors quality; no proven gain or constant-quality mode."
                ),
                self.quality_editor.speed.clone(),
                speed_modes,
                p,
            ));
        for (kind, title, hint, ids) in [
            (
                OptionalField::Peak,
                tr!("峰值码率 · Mbps", "Peak bitrate · Mbps"),
                tr!(
                    "仅峰值受限 VBR 请求峰值；清除表示撤回请求。",
                    "Peak applies to peak VBR; Clear withdraws the request."
                ),
                [
                    "rec-quality-peak-inherit",
                    "rec-quality-peak-clear",
                    "rec-quality-peak-set",
                ],
            ),
            (
                OptionalField::Buffer,
                tr!("缓冲大小 · kbit", "Buffer size · kbit"),
                tr!(
                    "十进制 kbit，不是 KiB；原生需整 byte，不自动取整。",
                    "Decimal kbit, not KiB; whole bytes are required, without rounding."
                ),
                [
                    "rec-quality-buffer-inherit",
                    "rec-quality-buffer-clear",
                    "rec-quality-buffer-set",
                ],
            ),
            (
                OptionalField::Gop,
                tr!("GOP 最大帧数", "Maximum GOP frames"),
                tr!(
                    "仅请求最大 picture 数，不保证精确关键帧或 IDR 周期。",
                    "Requests a maximum picture count, not an exact keyframe or IDR interval."
                ),
                [
                    "rec-quality-gop-inherit",
                    "rec-quality-gop-clear",
                    "rec-quality-gop-set",
                ],
            ),
        ] {
            let (draft, field) = self.quality_editor.optional(kind);
            let mut modes = div().flex().gap(px(5.));
            for ((mode, label), id) in [
                (OptionalMode::Inherit, tr!("继承", "Inherit")),
                (OptionalMode::Clear, tr!("清除请求", "Clear request")),
                (OptionalMode::Set, tr!("设置", "Set")),
            ]
            .into_iter()
            .zip(ids)
            {
                modes = modes.child(button(id, label, draft.mode == mode, active, p).on_click(
                    cx.listener(move |view, _, window, cx| {
                        if editable(view) {
                            view.quality_editor.configure_focus(window, cx);
                            view.quality_editor.optional_mut(kind).mode = mode;
                            view.quality_editor.error = None;
                            view.quality_editor.sync_enabled(true, cx);
                            window.focus(&view.focus);
                            cx.notify();
                        }
                    }),
                ));
            }
            rows = rows.child(section(title, hint, field.clone(), modes, p));
        }
        let mut rate_modes = div().flex().flex_wrap().gap(px(5.));
        for (mode, label, id) in [
            (
                Override::Inherit,
                tr!("继承", "Inherit"),
                "rec-quality-rate-inherit",
            ),
            (
                Override::Set(RateControl::EncoderDefault),
                tr!("编码器默认", "Encoder default"),
                "rec-quality-rate-default",
            ),
            (
                Override::Set(RateControl::PeakConstrainedVbr),
                tr!("峰值受限 VBR", "Peak VBR"),
                "rec-quality-rate-peak",
            ),
        ] {
            rate_modes = rate_modes.child(
                button(
                    id,
                    label,
                    self.quality_editor.draft.rate_control == mode,
                    active,
                    p,
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    if editable(view) {
                        view.quality_editor.draft.rate_control = mode;
                        view.quality_editor.error = None;
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            );
        }
        rows = rows
            .child(div().flex_shrink_0().flex().flex_col().gap(px(5.)).child(tr!("码率控制", "Rate control")).child(rate_modes))
            .child(div().flex_shrink_0().text_size(px(10.)).text_color(rgb(p.muted)).child(tr!(
                "这些是 H.264 请求，不代表编码器已采用。清除仅撤回属性请求，不保证关闭编码器功能；实际值与码流效果仍须验证。",
                "H.264 requests do not prove adoption. Clear withdraws a request, without guaranteeing a feature is off. Actual values and bitstream effects need verification."
            )));
        if !can_edit {
            rows = rows.child(div().flex_shrink_0().text_size(px(11.)).text_color(rgb(p.muted)).child(tr!(
                "仅实验模式、待录制且非 GIF 时可编辑；草稿已保留，不会应用。",
                "Edit only before recording, with experiments enabled and a non-GIF preset. The draft is retained and will not apply."
            )));
        }
        div()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_size(px(12.))
                            .child(tr!("高级画质请求", "Advanced quality")),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .child(
                                button("rec-quality-back", tr!("画质", "Quality"), false, true, p)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.flyout =
                                            (view.stage == Stage::Idle).then_some(Flyout::Quality);
                                        window.focus(&view.focus);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button("rec-quality-close", tr!("关闭", "Close"), false, true, p)
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.flyout = None;
                                        window.focus(&view.focus);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                button(
                    "rec-quality-enabled",
                    if !can_edit {
                        tr!("草稿已保留，不应用", "Draft retained; not applied")
                    } else if self.quality_editor.draft.enabled {
                        tr!("已启用高级请求", "Advanced requests enabled")
                    } else {
                        tr!("启用高级请求", "Enable advanced requests")
                    },
                    self.quality_editor.draft.enabled,
                    can_edit,
                    p,
                )
                .flex_shrink_0()
                .on_click(cx.listener(|view, _, window, cx| {
                    if available(view) {
                        view.quality_editor.configure_focus(window, cx);
                        view.quality_editor.draft.enabled = !view.quality_editor.draft.enabled;
                        view.quality_editor.error = None;
                        view.quality_editor.sync_enabled(true, cx);
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            )
            .children(self.quality_editor.error.as_ref().map(|error| {
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(p.danger))
                    .child(error.clone())
            }))
            .child(rows)
            .child(
                button(
                    "rec-quality-reset",
                    tr!("重置并停用高级请求", "Reset and disable"),
                    false,
                    can_edit,
                    p,
                )
                .flex_shrink_0()
                .on_click(cx.listener(|view, _, window, cx| {
                    if available(view) {
                        view.quality_editor.reset(cx);
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            )
    }
}

#[cfg(all(test, windows))]
#[path = "recording_quality_editor_smoke.rs"]
mod smoke;
