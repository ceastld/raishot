//! 剪辑四边数值复用原生文本控件；有效提交只产生一次剪辑增量命令。
use crate::{
    annotation::Point,
    capture_view::CaptureView,
    long_clip::LongClip,
    long_clip_chrome::ClipTheme,
    stitch::ScrollAxis,
    text_field::{TextField, TextFieldEvent},
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, AppContext, Context, Entity, KeyDownEvent, Render,
    Subscription, WeakEntity, Window,
};
use std::ops::Range;

pub(crate) struct LongClipParameters {
    owner: WeakEntity<CaptureView>,
    baseline: LongClip,
    inputs: [Entity<TextField>; 4],
    dirty: [bool; 4],
    theme: ClipTheme,
    error: String,
    _subscriptions: Vec<Subscription>,
}

fn edge_values(clip: &LongClip) -> [u32; 4] {
    let (width, height) = clip.source_size();
    let length = match clip.axis() {
        ScrollAxis::Vertical => height,
        ScrollAxis::Horizontal => width,
    };
    let start = clip
        .removed()
        .first()
        .filter(|range| range.start == 0)
        .map_or(0, |range| range.end);
    let end = clip
        .removed()
        .last()
        .filter(|range| range.end == length)
        .map_or(0, |range| length - range.start);
    let cross = clip.cross_range();
    match clip.axis() {
        ScrollAxis::Vertical => [start, width - cross.end, end, cross.start],
        ScrollAxis::Horizontal => [cross.start, end, height - cross.end, start],
    }
}

fn keep_ranges(clip: &LongClip, edges: [u32; 4]) -> Result<(Range<u32>, Range<u32>)> {
    let (width, height) = clip.source_size();
    let [top, right, bottom, left] = edges;
    if top.checked_add(bottom).is_none_or(|sum| sum >= height)
        || left.checked_add(right).is_none_or(|sum| sum >= width)
    {
        return Err(tr!(
            "四边裁剪后必须至少保留 1 × 1 像素",
            "Keep at least 1 × 1 pixel"
        )
        .into());
    }
    let horizontal = left..width - right;
    let vertical = top..height - bottom;
    let (long, cross) = match clip.axis() {
        ScrollAxis::Vertical => (vertical, horizontal),
        ScrollAxis::Horizontal => (horizontal, vertical),
    };
    // 内部删段可能已经覆盖剩余范围；提交前同时验证这种空成品。
    let mut candidate = clip.clone();
    candidate.trim(long.clone(), cross.clone())?;
    Ok((long, cross))
}

impl LongClipParameters {
    fn new(
        owner: WeakEntity<CaptureView>,
        clip: LongClip,
        theme: ClipTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let values = edge_values(&clip);
        let inputs: [Entity<TextField>; 4] = std::array::from_fn(|index| {
            cx.new(|cx| TextField::new(values[index].to_string(), "0", cx).bare())
        });
        let group: Vec<_> = inputs.iter().map(Entity::downgrade).collect();
        for input in &inputs {
            input.update(cx, |field, cx| {
                field.configure_focus(group.clone(), window, cx)
            });
        }
        let mut subscriptions = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |state, _, event, window, cx| match event {
                    TextFieldEvent::Changed => {
                        state.dirty[index] = true;
                        state.error.clear();
                        let _ = state.owner.update(cx, |owner, cx| {
                            if let Some(ui) = owner.long_clip_ui.as_mut() {
                                ui.parameters_pending = true;
                            }
                            cx.notify();
                        });
                        cx.notify();
                    }
                    TextFieldEvent::Submit => {
                        if state.commit(window, cx) {
                            state.host_focus(window, cx);
                        }
                    }
                },
            ));
            let focus = input.read(cx).focus_handle.clone();
            subscriptions.push(cx.on_blur(&focus, window, |state, window, cx| {
                state.commit(window, cx);
            }));
        }
        Self {
            owner,
            baseline: clip,
            inputs,
            dirty: [false; 4],
            theme,
            error: String::new(),
            _subscriptions: subscriptions,
        }
    }

    fn composing(&self, cx: &App) -> bool {
        self.inputs
            .iter()
            .any(|input| input.read(cx).is_composing())
    }

    pub(crate) fn focused(&self, window: &Window, cx: &App) -> bool {
        self.inputs
            .iter()
            .any(|input| input.read(cx).focus_handle.is_focused(window))
    }

    fn reset(&mut self, clip: LongClip, cx: &mut Context<Self>) {
        let values = edge_values(&clip);
        for (input, value) in self.inputs.iter().zip(values) {
            input.update(cx, |field, cx| field.set_text(value.to_string(), cx));
        }
        self.baseline = clip;
        self.dirty = [false; 4];
        self.error.clear();
        cx.notify();
    }

    fn sync(&mut self, clip: &LongClip, theme: ClipTheme, cx: &mut Context<Self>) -> bool {
        self.theme = theme;
        if &self.baseline != clip && !self.composing(cx) {
            // 撤销或画布手势改变了几何时，旧数字草稿不能覆盖新文档。
            self.reset(clip.clone(), cx);
            return true;
        }
        false
    }

    fn host_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.owner.update(cx, |owner, _| window.focus(&owner.focus));
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.dirty.iter().any(|dirty| *dirty) {
            return true;
        }
        if self.composing(cx) {
            return false;
        }
        let values: std::result::Result<Vec<_>, _> = self
            .inputs
            .iter()
            .map(|input| input.read(cx).text().trim().parse::<u32>())
            .collect();
        let values = match values {
            Ok(values) => [values[0], values[1], values[2], values[3]],
            Err(_) => {
                self.error = tr!("请输入非负整数像素", "Enter whole pixels, zero or more").into();
                cx.notify();
                return false;
            }
        };
        let (long, cross) = match keep_ranges(&self.baseline, values) {
            Ok(ranges) => ranges,
            Err(error) => {
                self.error = error.to_string();
                cx.notify();
                return false;
            }
        };
        let baseline = self.baseline.clone();
        let result = self.owner.update(cx, move |owner, cx| {
            if owner.editor.document.long_clip() != Some(&baseline) {
                return Err(tr!(
                    "剪辑已变化，请重新输入",
                    "Clip changed; enter the value again"
                )
                .to_owned());
            }
            let before = owner.long_clip_projection();
            let anchor = owner
                .long_clip_source_center(window)
                .unwrap_or(Point { x: 0., y: 0. });
            let result = owner.editor.trim_long_clip(long, cross);
            let error = result.as_ref().err().map(ToString::to_string);
            owner.long_clip_complete_command(result, before, anchor, window, cx);
            if let Some(error) = error {
                Err(error)
            } else {
                if let Some(ui) = owner.long_clip_ui.as_mut() {
                    ui.parameters_pending = false;
                }
                Ok(owner.editor.document.long_clip().cloned())
            }
        });
        match result {
            Ok(Ok(Some(clip))) => {
                // Enter 会紧接着引起失焦；先清 dirty，后一事件就不会再提交命令。
                self.reset(clip, cx);
                true
            }
            Ok(Err(error)) => {
                self.error = error;
                cx.notify();
                false
            }
            _ => false,
        }
    }

    fn capture_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .inputs
            .iter()
            .any(|input| input.read(cx).focus_handle.is_focused(window))
        {
            return;
        }
        if event.keystroke.key == "escape" && !self.composing(cx) {
            let clip = self
                .owner
                .upgrade()
                .and_then(|owner| owner.read(cx).editor.document.long_clip().cloned());
            if let Some(clip) = clip {
                self.reset(clip, cx);
            }
            let _ = self.owner.update(cx, |owner, cx| {
                if let Some(ui) = owner.long_clip_ui.as_mut() {
                    ui.parameters_pending = false;
                }
                cx.notify();
            });
            self.host_focus(window, cx);
            cx.stop_propagation();
        }
    }
}

impl Render for LongClipParameters {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let values = edge_values(&self.baseline);
        let mut grid = div()
            .id("long-clip-trim-fields")
            .flex()
            .flex_col()
            .gap(px(8.))
            .capture_key_down(cx.listener(Self::capture_key));
        // 与前端稿同序：上、下 ／ 左、右；数值顺序为上、右、下、左。
        for row in [[0, 2], [3, 1]] {
            let mut fields = div().flex().gap(px(8.));
            for index in row {
                let on = values[index] > 0;
                fields = fields.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h(px(30.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgb(if on { t.cut_border } else { t.border }))
                        .bg(rgb(t.well))
                        .text_size(px(11.))
                        .text_color(rgb(t.muted))
                        .child(div().flex_shrink_0().child(
                            [
                                tr!("上", "Top"),
                                tr!("右", "Right"),
                                tr!("下", "Bottom"),
                                tr!("左", "Left"),
                            ][index],
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(rgb(if on { t.cut } else { t.fg }))
                                .child(self.inputs[index].clone()),
                        )
                        .child(div().flex_shrink_0().text_color(rgb(t.faint)).child("px")),
                );
            }
            grid = grid.child(fields);
        }
        if !self.error.is_empty() {
            grid = grid.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(t.cut))
                    .child(self.error.clone()),
            );
        }
        grid
    }
}

impl CaptureView {
    pub(crate) fn long_clip_parameter_fields(
        &mut self,
        clip: &LongClip,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = ClipTheme::new(crate::theme::is_light(cx));
        let current = self
            .long_clip_ui
            .as_ref()
            .and_then(|ui| ui.trim_inputs.clone());
        let inputs = current.unwrap_or_else(|| {
            let owner = cx.entity().downgrade();
            let clip = clip.clone();
            let inputs = cx.new(|cx| LongClipParameters::new(owner, clip, theme, window, cx));
            if let Some(ui) = self.long_clip_ui.as_mut() {
                ui.trim_inputs = Some(inputs.clone());
            }
            inputs
        });
        if inputs.update(cx, |state, cx| state.sync(clip, theme, cx)) {
            if let Some(ui) = self.long_clip_ui.as_mut() {
                ui.parameters_pending = false;
            }
        }
        inputs.into_any_element()
    }

    pub(crate) fn long_clip_cancel_parameters(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(ui) = self.long_clip_ui.as_mut() else {
            return false;
        };
        if !ui.parameters_pending {
            return false;
        }
        let input = ui.trim_inputs.clone();
        ui.parameters_pending = false;
        if let (Some(input), Some(clip)) = (input, self.editor.document.long_clip().cloned()) {
            input.update(cx, |state, cx| state.reset(clip, cx));
        }
        self.status.clear();
        window.focus(&self.focus);
        cx.notify();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_edges_swap_with_axis_and_allow_outward_restore() {
        let mut vertical = LongClip::new(100, 500, ScrollAxis::Vertical).unwrap();
        vertical.trim(30..450, 10..90).unwrap();
        vertical.delete(180..200).unwrap();
        assert_eq!(edge_values(&vertical), [30, 10, 50, 10]);
        let (long, cross) = keep_ranges(&vertical, [0, 0, 0, 0]).unwrap();
        vertical.trim(long, cross).unwrap();
        assert_eq!(vertical.removed(), &[180..200]);
        assert_eq!(vertical.cross_range(), 0..100);
        let horizontal = LongClip::new(500, 100, ScrollAxis::Horizontal).unwrap();
        assert_eq!(
            keep_ranges(&horizontal, [10, 50, 20, 30]).unwrap(),
            (30..450, 10..80)
        );
    }

    #[test]
    fn numeric_edges_reject_empty_dimensions_overflow_and_only_deleted_remainder() {
        let mut clip = LongClip::new(100, 500, ScrollAxis::Vertical).unwrap();
        for values in [[250, 0, 250, 0], [0, 50, 0, 50], [u32::MAX, 0, 1, 0]] {
            assert!(keep_ranges(&clip, values).is_err());
        }
        clip.delete(100..200).unwrap();
        assert!(keep_ranges(&clip, [100, 0, 300, 0]).is_err());
        assert_eq!(
            keep_ranges(&clip, [499, 99, 0, 0]).unwrap(),
            (499..500, 0..1)
        );
    }
}
