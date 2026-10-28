//! 不可变图片快照的取色与测距；标记只在窗口绘制，不修改输入像素。
#[path = "image_tool_colors.rs"]
mod colors;
#[path = "image_tool_measure.rs"]
mod model;

use self::colors::{ColorFormat, ColorValue};
use self::model::{measure, ruler_ticks, sample, ContentRect, PixelPoint, PixelSample};
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tools::{check_cancel, ProcessedImage, ToolError, ToolLimits, ToolResult},
    text_field::{TextField, TextFieldEvent},
    Result,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context,
    ElementId, Entity, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    PathBuilder, Render, RenderImage, Subscription, Window, WindowBounds, WindowHandle,
    WindowOptions,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

const MAX_SAMPLES: usize = 8;
const RULER_LEFT: f64 = 48.;
const RULER_TOP: f64 = 20.;

#[derive(Clone)]
enum Source {
    Processed(Arc<ProcessedImage>),
    Shared(Arc<Image>),
}

impl Source {
    fn image(&self) -> &Image {
        match self {
            Self::Processed(image) => image.image(),
            Self::Shared(image) => image,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickMode {
    Color,
    First,
    Second,
}

#[derive(Clone, Copy)]
enum Control {
    Mode(PickMode),
    ClearPoints,
    ClearColors,
    CopyColor(usize),
    CopyProbe,
    CopyMeasurement,
    Format(ColorFormat),
    ConvertColor,
    CopyConverted,
    UseProbe,
    UseColor(usize),
    CopyPalette,
    Rulers,
    Preview,
    Cancel,
    Close,
}

pub(crate) struct ImageMeasureView {
    source: Source,
    preview: Option<Arc<RenderImage>>,
    active: Option<Arc<AtomicBool>>,
    content: Arc<Mutex<Option<ContentRect>>>,
    mode: PickMode,
    probe: Option<PixelSample>,
    samples: Vec<PixelSample>,
    color_input: Entity<TextField>,
    applied_input: String,
    converted: ColorValue,
    color_format: ColorFormat,
    rulers: bool,
    first: Option<PixelPoint>,
    second: Option<PixelPoint>,
    notice: String,
    failed: bool,
    closing: bool,
    focus: FocusHandle,
    _subscription: Subscription,
}

pub(crate) fn open_processed(
    image: Arc<ProcessedImage>,
    cx: &mut App,
) -> Result<WindowHandle<ImageMeasureView>> {
    open(Source::Processed(image), cx)
}

pub(crate) fn open_shared(
    image: Arc<Image>,
    cx: &mut App,
) -> Result<WindowHandle<ImageMeasureView>> {
    open(Source::Shared(image), cx)
}

fn open(source: Source, cx: &mut App) -> Result<WindowHandle<ImageMeasureView>> {
    crate::theme::install(cx);
    let dimensions = cx
        .primary_display()
        .map_or(size(px(1060.), px(860.)), |display| {
            let bounds = display.bounds();
            size(
                px(1060_f32.min((f32::from(bounds.size.width) - 48.).max(440.))),
                px(860_f32.min((f32::from(bounds.size.height) - 72.).max(420.))),
            )
        });
    let bounds = Bounds::centered(None, dimensions, cx);
    Ok(cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(brand_title!(
                "取色与测距",
                "Color and distance"
            ));
            crate::gpui_capture::foreground_auxiliary(window);
            let view = cx.new(|cx| {
                let focus = cx.focus_handle();
                window.focus(&focus);
                let applied_input = "#4A90D9".to_owned();
                let color_input = cx
                    .new(|cx| TextField::new(applied_input.clone(), "#RRGGBB / rgb(0, 0, 0)", cx));
                let focus_group = vec![color_input.downgrade()];
                color_input.update(cx, |field, cx| {
                    field.configure_focus(focus_group, window, cx)
                });
                let subscription = cx.subscribe_in(
                    &color_input,
                    window,
                    |view: &mut ImageMeasureView, _, event, window, cx| match event {
                        TextFieldEvent::Submit => view.convert_color(window, cx),
                        TextFieldEvent::Changed => cx.notify(),
                    },
                );
                let mut view = ImageMeasureView {
                    source,
                    preview: None,
                    active: None,
                    content: Arc::new(Mutex::new(None)),
                    mode: PickMode::Color,
                    probe: None,
                    samples: Vec::new(),
                    color_input,
                    applied_input,
                    converted: ColorValue {
                        r: 74,
                        g: 144,
                        b: 217,
                        alpha: 255,
                    },
                    color_format: ColorFormat::Hex,
                    rulers: true,
                    first: None,
                    second: None,
                    notice: String::new(),
                    failed: false,
                    closing: false,
                    focus,
                    _subscription: subscription,
                };
                view.start_preview(window, cx);
                view
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                weak.update(cx, |view, cx| view.request_close(cx))
                    .unwrap_or(true)
            });
            view
        },
    )?)
}

impl ImageMeasureView {
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(cancel) = &self.active else {
            return true;
        };
        cancel.store(true, Ordering::Release);
        self.closing = true;
        self.enable_fields(cx);
        self.notice = tr!("正在取消并关闭…", "Cancelling and closing…").into();
        self.failed = false;
        cx.notify();
        false
    }

    fn start_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_some() || self.preview.is_some() || self.closing {
            return;
        }
        let source = self.source.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.active = Some(cancel.clone());
        self.enable_fields(cx);
        self.notice = tr!("正在准备预览…", "Preparing preview…").into();
        self.failed = false;
        let work_cancel = cancel.clone();
        let task = cx
            .background_executor()
            .spawn(async move { prepare_preview(source.image(), &work_cancel) });
        cx.spawn_in(window, async move |weak, cx| {
            let result = task.await;
            let _ = weak.update_in(cx, |view, window, cx| {
                if !view.active.as_ref().is_some_and(|active| Arc::ptr_eq(active, &cancel)) {
                    return;
                }
                view.active = None;
                view.enable_fields(cx);
                if view.closing {
                    window.remove_window();
                    return;
                }
                match result {
                    Ok(preview) if !cancel.load(Ordering::Acquire) => {
                        view.preview = Some(preview);
                        view.notice = tr!(
                            "移动鼠标查看像素，点击保留取色；选起点 A 和终点 B 进行测量。",
                            "Move to inspect a pixel; click to keep a color. Pick A and B to measure."
                        ).into();
                    }
                    Ok(_) | Err(ToolError::Cancelled) => {
                        view.notice = tr!("预览已取消，可重新准备", "Preview cancelled; retry when ready").into();
                    }
                    Err(error) => {
                        view.notice = error.to_string();
                        view.failed = true;
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn enable_fields(&self, cx: &mut Context<Self>) {
        let enabled = self.active.is_none() && !self.closing;
        self.color_input
            .update(cx, |field, cx| field.set_enabled(enabled, cx));
    }

    fn convert_color(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_some() || self.closing || self.color_input.read(cx).is_composing() {
            return;
        }
        match ColorValue::parse(self.color_input.read(cx).text()) {
            Ok(color) => {
                self.converted = color;
                self.applied_input = self.color_input.read(cx).text().to_owned();
                self.notice = tr!("颜色已转换", "Color converted").into();
                self.failed = false;
                window.focus(&self.focus);
            }
            Err(error) => {
                self.notice = error.to_string();
                self.failed = true;
            }
        }
        cx.notify();
    }

    fn use_color(&mut self, color: PixelSample, window: &mut Window, cx: &mut Context<Self>) {
        self.converted = color_value(color);
        self.applied_input = self.converted.formatted(ColorFormat::Hex);
        self.color_input.update(cx, |field, cx| {
            field.set_text(self.applied_input.clone(), cx)
        });
        self.notice.clear();
        self.failed = false;
        window.focus(&self.focus);
    }

    fn hit(&self, position: gpui::Point<gpui::Pixels>) -> Option<PixelSample> {
        if self.preview.is_none() || self.active.is_some() || self.closing {
            return None;
        }
        let content = (*self.content.lock().ok()?)?;
        let image = self.source.image();
        let pixel = content.pixel(
            image.width,
            image.height,
            f64::from(f32::from(position.x)),
            f64::from(f32::from(position.y)),
        )?;
        sample(image, pixel)
    }

    fn moved(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let probe = self.hit(event.position);
        if probe != self.probe {
            self.probe = probe;
            cx.notify();
        }
    }

    fn picked(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(color) = self.hit(event.position) else {
            return;
        };
        cx.stop_propagation();
        window.focus(&self.focus);
        self.probe = Some(color);
        match self.mode {
            PickMode::Color => {
                if self.samples.len() == MAX_SAMPLES {
                    self.samples.remove(0);
                }
                self.samples.push(color);
            }
            PickMode::First => {
                self.first = Some(color.point);
                self.mode = PickMode::Second;
            }
            PickMode::Second => {
                self.second = Some(color.point);
                self.mode = PickMode::Color;
            }
        }
        cx.notify();
    }

    fn control(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        match control {
            Control::Close => {
                if self.request_close(cx) {
                    window.remove_window();
                }
                return;
            }
            Control::Cancel => {
                if let Some(active) = &self.active {
                    active.store(true, Ordering::Release);
                    self.notice = tr!("正在取消预览…", "Cancelling preview…").into();
                    cx.notify();
                }
                return;
            }
            Control::Preview => {
                self.start_preview(window, cx);
                return;
            }
            _ if self.active.is_some() || self.closing => return,
            _ if self.color_input.read(cx).is_composing() => return,
            Control::Mode(mode) if self.preview.is_some() => self.mode = mode,
            Control::Mode(_) => return,
            Control::ClearPoints => {
                self.first = None;
                self.second = None;
                self.mode = PickMode::Color;
            }
            Control::ClearColors => self.samples.clear(),
            Control::CopyColor(index) => {
                if let Some(color) = self.samples.get(index) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        color_value(*color).formatted(self.color_format),
                    ));
                }
            }
            Control::CopyProbe => {
                if let Some(color) = self.probe {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        color_value(color).formatted(self.color_format),
                    ));
                }
            }
            Control::CopyMeasurement => {
                if let (Some(a), Some(b)) = (self.first, self.second) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(measurement_text(a, b)));
                }
            }
            Control::Format(format) => self.color_format = format,
            Control::Rulers => {
                self.rulers = !self.rulers;
                self.probe = None;
                if let Ok(mut content) = self.content.lock() {
                    *content = None;
                }
            }
            Control::ConvertColor => {
                self.convert_color(window, cx);
                return;
            }
            Control::CopyConverted => cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                self.converted.formatted(self.color_format),
            )),
            Control::UseProbe => {
                if let Some(color) = self.probe {
                    self.use_color(color, window, cx);
                }
            }
            Control::UseColor(index) => {
                if let Some(color) = self.samples.get(index).copied() {
                    self.use_color(color, window, cx);
                }
            }
            Control::CopyPalette => {
                if !self.samples.is_empty() {
                    let text = self
                        .samples
                        .iter()
                        .map(|color| color_value(*color).formatted(self.color_format))
                        .collect::<Vec<_>>()
                        .join("\n");
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
            }
        }
        cx.notify();
    }

    fn button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<String>,
        selected: bool,
        enabled: bool,
        control: Control,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let label: String = label.into();
        div()
            .id(id)
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(13.))
            .bg(rgb(if selected {
                palette.selected
            } else {
                palette.track
            }))
            .text_color(rgb(palette.foreground))
            .opacity(if enabled { 1. } else { 0.4 })
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.hover)))
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.control(control, window, cx)),
                    )
            })
            .child(label)
            .into_any_element()
    }

    fn preview_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut frame = div()
            .id("measure-image")
            .relative()
            .w_full()
            .h(px(360.))
            .min_h(px(120.))
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(palette.track))
            .on_mouse_move(cx.listener(Self::moved))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::picked));
        if let Some(preview) = &self.preview {
            let preview = preview.clone();
            let image = self.source.image();
            let dimensions = (image.width, image.height);
            let content = self.content.clone();
            let endpoints = (self.first, self.second);
            let probe = self.probe.map(|probe| probe.point);
            let rulers = self.rulers;
            frame = frame.child(
                canvas(
                    move |bounds, _, _| {
                        let viewport = content_rect(bounds);
                        let viewport = if rulers {
                            ContentRect {
                                x: viewport.x + RULER_LEFT,
                                y: viewport.y + RULER_TOP,
                                width: viewport.width - RULER_LEFT - 8.,
                                height: viewport.height - RULER_TOP - 8.,
                            }
                        } else {
                            viewport
                        };
                        let actual = viewport
                            .contain(dimensions.0, dimensions.1)
                            .map(|rect| content_rect(paint_bounds(rect)));
                        if let Ok(mut slot) = content.lock() {
                            *slot = actual;
                        }
                        actual
                    },
                    move |_, content: Option<ContentRect>, window, cx| {
                        let Some(content) = content else {
                            return;
                        };
                        let _ = window.paint_image_exact(
                            paint_bounds(content),
                            px(0.).into(),
                            preview.clone(),
                            0,
                            false,
                        );
                        if rulers {
                            paint_rulers(window, content, dimensions, palette, cx);
                        }
                        let center = |p| content.center(dimensions.0, dimensions.1, p);
                        if let (Some(a), Some(b)) =
                            (endpoints.0.and_then(center), endpoints.1.and_then(center))
                        {
                            line(window, a, b, palette.background, 4.);
                            line(window, a, b, palette.accent, 2.);
                        }
                        for (pixel, color) in [
                            (endpoints.0, palette.accent),
                            (endpoints.1, palette.danger),
                            (probe, palette.foreground),
                        ] {
                            if let Some((x, y)) = pixel.and_then(center) {
                                for (a, b) in
                                    [((x - 5., y), (x + 5., y)), ((x, y - 5.), (x, y + 5.))]
                                {
                                    line(window, a, b, palette.background, 4.);
                                    line(window, a, b, color, 1.5);
                                }
                            }
                        }
                    },
                )
                .size_full(),
            );
        } else {
            frame = frame.child(div().p_4().text_color(rgb(palette.muted)).child(
                if self.active.is_some() {
                    tr!("正在准备预览…", "Preparing preview…")
                } else {
                    tr!("预览未就绪", "Preview is not ready")
                },
            ));
        }
        frame.into_any_element()
    }
}

impl Drop for ImageMeasureView {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.store(true, Ordering::Release);
        }
    }
}

impl Render for ImageMeasureView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::theme::sync_window(window, cx);
        window.set_window_title(brand_title!(
            "取色与测距",
            "Color and distance"
        ));
        let palette = crate::theme::palette(cx);
        let ready = self.preview.is_some() && self.active.is_none() && !self.closing;
        let converter_ready = self.active.is_none() && !self.closing;
        let image = self.source.image();
        let dimensions = trf!(
            "输入结果：{}×{} 物理像素",
            "Input result: {}×{} physical pixels",
            image.width,
            image.height
        );
        let mut swatches = div().flex().flex_col().gap_2();
        for (index, color) in self.samples.iter().enumerate() {
            let rgb_value =
                (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b);
            swatches = swatches.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(22.))
                            .h(px(22.))
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(palette.border))
                            .bg(rgb(rgb_value)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().text_size(px(12.)).child(color_text(*color)))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .child(color_value(*color).formatted(self.color_format)),
                            ),
                    )
                    .child(self.button(
                        ("measure-copy-color", index),
                        tr!("复制", "Copy"),
                        false,
                        ready,
                        Control::CopyColor(index),
                        cx,
                    ))
                    .child(self.button(
                        ("measure-use-color", index),
                        tr!("填入转换", "Use color"),
                        false,
                        ready,
                        Control::UseColor(index),
                        cx,
                    )),
            );
        }
        let mut formats = div().flex().flex_wrap().items_center().gap_2();
        for (id, label, format) in [
            ("measure-format-hex", "HEX", ColorFormat::Hex),
            ("measure-format-rgb", "RGB", ColorFormat::Rgb),
            ("measure-format-hsl", "HSL", ColorFormat::Hsl),
            ("measure-format-hsv", "HSV", ColorFormat::Hsv),
        ] {
            formats = formats.child(self.button(
                id,
                label,
                self.color_format == format,
                converter_ready,
                Control::Format(format),
                cx,
            ));
        }
        let converted_rgb = (u32::from(self.converted.r) << 16)
            | (u32::from(self.converted.g) << 8)
            | u32::from(self.converted.b);
        let converter = div().flex().flex_col().gap_2()
            .child(div().child(tr!("颜色格式", "Color formats")))
            .child(formats)
            .child(self.color_input.clone())
            .child(div().flex().flex_wrap().gap_2()
                .child(self.button("measure-convert-color", tr!("转换输入", "Convert input"), false, converter_ready, Control::ConvertColor, cx))
                .child(self.button("measure-use-probe", tr!("填入当前像素", "Use current pixel"), false, ready && self.probe.is_some(), Control::UseProbe, cx)))
            .child(div().flex().flex_wrap().items_center().gap_2()
                .child(div().w(px(22.)).h(px(22.)).rounded_sm().border_1().border_color(rgb(palette.border)).bg(rgb(converted_rgb)))
                .child(div().text_size(px(12.)).child(self.converted.formatted(self.color_format)))
                .child(self.button("measure-copy-converted", tr!("复制转换结果", "Copy converted color"), false, converter_ready, Control::CopyConverted, cx)))
            .when(self.color_input.read(cx).text() != self.applied_input, |body| body.child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(tr!(
                "输入尚未转换；上方仍显示已转换的颜色。", "Input is not converted; the result above keeps the applied color."
            ))))
            .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(tr!(
                "支持 #RGB、#RGBA、#RRGGBB、#RRGGBBAA，rgb(0, 128, 255)、hsl(210, 100%, 50%)、hsv(210, 100%, 100%)；透明颜色可用 rgba/hsla/hsva，末项 alpha 为 0–1。转换基于 8 位 RGB，色块显示 RGB。",
                "Use HEX, rgb(0, 128, 255), hsl(210, 100%, 50%) or hsv(210, 100%, 100%). Alpha forms rgba/hsla/hsva take a final 0–1 value. Conversion uses 8-bit RGB; swatches show RGB."
            )));
        let point_text = |point: Option<PixelPoint>| {
            point
                .map(|p| format!("({}, {})", p.x, p.y))
                .unwrap_or_else(|| tr!("未选择", "Not selected").into())
        };
        let measurement = match (self.first, self.second) {
            (Some(a), Some(b)) => measurement_text(a, b),
            _ => trf!(
                "A {} · B {}",
                "A {} · B {}",
                point_text(self.first),
                point_text(self.second)
            ),
        };
        let probe = self.probe.map(color_text).unwrap_or_else(|| {
            tr!(
                "鼠标移到图像内查看源像素",
                "Move over the image to inspect source pixels"
            )
            .into()
        });
        div().size_full().p_4().flex().flex_col().gap_3().bg(rgb(palette.background))
            .text_color(rgb(palette.foreground)).font_family("Microsoft YaHei UI").text_size(px(14.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.color_input.read(cx).is_composing() { return; }
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    if !event.is_held { view.control(if view.active.is_some() { Control::Cancel } else { Control::Close }, window, cx); }
                }
            }))
            .child(div().text_size(px(20.)).child(tr!("取色与测距", "Color and distance")))
            .child(div().id("measure-body").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col().gap_3()
                .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(dimensions))
                .child(self.preview_panel(cx))
                .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(tr!(
                    "坐标从输入图左上 (0,0) 开始；向右 0°，向下 90°。上方 X 和左侧 Y 标尺使用源像素索引；标尺与留白不参与取样，标记不会进入原图。",
                    "Input origin is top-left (0,0); right is 0°, down is 90°. Top X and left Y rulers use source pixel indices. Rulers and margins are not sampled; marks only appear here."
                )))
                .child(div().flex().flex_wrap().gap_2()
                    .child(self.button("measure-mode-color", tr!("点击取色", "Pick color"), self.mode == PickMode::Color, ready, Control::Mode(PickMode::Color), cx))
                    .child(self.button("measure-mode-a", tr!("选择起点 A", "Pick A"), self.mode == PickMode::First, ready, Control::Mode(PickMode::First), cx))
                    .child(self.button("measure-mode-b", tr!("选择终点 B", "Pick B"), self.mode == PickMode::Second, ready, Control::Mode(PickMode::Second), cx))
                    .child(self.button("measure-rulers", if self.rulers { tr!("标尺：开", "Rulers: on") } else { tr!("标尺：关", "Rulers: off") }, self.rulers, ready, Control::Rulers, cx))
                    .child(self.button("measure-clear-points", tr!("清除测量", "Clear points"), false, ready && (self.first.is_some() || self.second.is_some()), Control::ClearPoints, cx)))
                .child(div().text_size(px(12.)).child(measurement))
                .child(self.button("measure-copy-distance", tr!("复制测量", "Copy measurement"), false, ready && self.first.is_some() && self.second.is_some(), Control::CopyMeasurement, cx))
                .child(div().text_size(px(12.)).child(probe))
                .child(self.button("measure-copy-probe", tr!("复制当前像素", "Copy current pixel"), false, ready && self.probe.is_some(), Control::CopyProbe, cx))
                .child(converter)
                .child(div().text_size(px(12.)).text_color(rgb(palette.muted)).child(tr!(
                    "颜色直读源 BGRA（包括 alpha），预览底色不参与取样。最多保留最近 8 个采样色；复制使用上方选定格式并保留 alpha。",
                    "Samples read source BGRA and alpha, without preview background. Keep the latest 8 colors; copy in the chosen format with alpha."
                )))
                .child(swatches)
                .child(div().flex().flex_wrap().gap_2()
                    .child(self.button("measure-copy-palette", tr!("复制采样调色板", "Copy sampled palette"), false, ready && !self.samples.is_empty(), Control::CopyPalette, cx))
                    .child(self.button("measure-clear-colors", tr!("清空采样色", "Clear colors"), false, ready && !self.samples.is_empty(), Control::ClearColors, cx))))
            .child(div().id("measure-notice").max_h(px(64.)).overflow_y_scroll().flex_shrink_0().text_size(px(12.))
                .text_color(rgb(if self.failed { palette.danger } else { palette.muted })).child(self.notice.clone()))
            .child(div().flex().flex_wrap().gap_2().flex_shrink_0()
                .child(self.button("measure-preview", tr!("重新准备预览", "Retry preview"), false, self.preview.is_none() && self.active.is_none() && !self.closing, Control::Preview, cx))
                .child(self.button("measure-cancel", tr!("取消预览", "Cancel preview"), false, self.active.is_some() && !self.closing, Control::Cancel, cx))
                .child(self.button("measure-close", tr!("关闭", "Close"), false, !self.closing, Control::Close, cx)))
            .into_any_element()
    }
}

fn color_value(color: PixelSample) -> ColorValue {
    ColorValue {
        r: color.r,
        g: color.g,
        b: color.b,
        alpha: color.alpha,
    }
}

fn color_text(color: PixelSample) -> String {
    trf!(
        "({}, {}) · RGB({}, {}, {}) · {} · alpha {}/255",
        "({}, {}) · RGB({}, {}, {}) · {} · alpha {}/255",
        color.point.x,
        color.point.y,
        color.r,
        color.g,
        color.b,
        color.hex(),
        color.alpha
    )
}

fn measurement_text(a: PixelPoint, b: PixelPoint) -> String {
    let result = measure(a, b);
    let direction = result
        .angle_degrees
        .map(|angle| format!("{angle:.2}°"))
        .unwrap_or_else(|| tr!("无方向", "No direction").into());
    trf!(
        "A ({}, {}) → B ({}, {}) · Δx {:.0}, Δy {:.0} px · 距离 {:.3} px · 角度 {direction}",
        "A ({}, {}) → B ({}, {}) · Δx {:.0}, Δy {:.0} px · distance {:.3} px · angle {direction}",
        a.x,
        a.y,
        b.x,
        b.y,
        result.dx,
        result.dy,
        result.distance
    )
}

fn content_rect(bounds: Bounds<gpui::Pixels>) -> ContentRect {
    ContentRect {
        x: f64::from(f32::from(bounds.origin.x)),
        y: f64::from(f32::from(bounds.origin.y)),
        width: f64::from(f32::from(bounds.size.width)),
        height: f64::from(f32::from(bounds.size.height)),
    }
}

fn paint_bounds(rect: ContentRect) -> Bounds<gpui::Pixels> {
    Bounds::new(
        point(px(rect.x as f32), px(rect.y as f32)),
        size(px(rect.width as f32), px(rect.height as f32)),
    )
}

fn line(window: &mut Window, a: (f64, f64), b: (f64, f64), color: u32, width: f32) {
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(point(px(a.0 as f32), px(a.1 as f32)));
    path.line_to(point(px(b.0 as f32), px(b.1 as f32)));
    if let Ok(path) = path.build() {
        window.paint_path(path, rgb(color));
    }
}

fn ruler_text(window: &Window, text: String, color: u32) -> gpui::ShapedLine {
    let len = text.len();
    window.text_system().shape_line(
        text.into(),
        px(10.),
        &[gpui::TextRun {
            len,
            font: window.text_style().font(),
            color: rgb(color).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    )
}

/// 标尺画在源图外侧；刻度与命中使用同一实际内容矩形和像素中心。
fn paint_rulers(
    window: &mut Window,
    content: ContentRect,
    dimensions: (u32, u32),
    palette: crate::capture_toolbar::Palette,
    cx: &mut App,
) {
    let top = ContentRect {
        x: content.x,
        y: content.y - RULER_TOP,
        width: content.width,
        height: RULER_TOP,
    };
    let left = ContentRect {
        x: content.x - RULER_LEFT,
        y: content.y,
        width: RULER_LEFT,
        height: content.height,
    };
    let corner = ContentRect {
        x: left.x,
        y: top.y,
        width: RULER_LEFT,
        height: RULER_TOP,
    };
    for strip in [top, left, corner] {
        window.paint_quad(gpui::fill(paint_bounds(strip), rgb(palette.background)));
    }
    line(
        window,
        (content.x, content.y),
        (content.x + content.width, content.y),
        palette.border,
        1.,
    );
    line(
        window,
        (content.x, content.y),
        (content.x, content.y + content.height),
        palette.border,
        1.,
    );
    let unit = ruler_text(window, "px".into(), palette.muted);
    let _ = unit.paint(
        point(px((corner.x + 4.) as f32), px((corner.y + 1.) as f32)),
        px(16.),
        window,
        cx,
    );
    let mut previous_right = content.x - 4.;
    for tick in ruler_ticks(dimensions.0, content.width) {
        let x = content.x + tick.offset;
        line(
            window,
            (x, content.y - 4.),
            (x, content.y),
            palette.muted,
            1.,
        );
        let label = ruler_text(window, tick.pixel.to_string(), palette.foreground);
        let width = f64::from(f32::from(label.width));
        if width > content.width {
            continue;
        }
        let label_x = (x - width / 2.)
            .max(content.x)
            .min(content.x + content.width - width);
        if label_x < previous_right + 4. {
            continue;
        }
        let _ = window.with_content_mask(
            Some(gpui::ContentMask {
                bounds: paint_bounds(top),
            }),
            |window| {
                label.paint(
                    point(px(label_x as f32), px((top.y + 1.) as f32)),
                    px(14.),
                    window,
                    cx,
                )
            },
        );
        previous_right = label_x + width;
    }
    let mut previous_bottom = content.y - 2.;
    for tick in ruler_ticks(dimensions.1, content.height) {
        let y = content.y + tick.offset;
        line(
            window,
            (content.x - 4., y),
            (content.x, y),
            palette.muted,
            1.,
        );
        if content.height < 16. {
            continue;
        }
        let label = ruler_text(window, tick.pixel.to_string(), palette.foreground);
        let width = f64::from(f32::from(label.width));
        let label_y = (y - 8.)
            .max(content.y)
            .min(content.y + content.height - 16.);
        if label_y < previous_bottom + 2. || width > RULER_LEFT - 8. {
            continue;
        }
        let _ = window.with_content_mask(
            Some(gpui::ContentMask {
                bounds: paint_bounds(left),
            }),
            |window| {
                label.paint(
                    point(px((content.x - 8. - width) as f32), px(label_y as f32)),
                    px(16.),
                    window,
                    cx,
                )
            },
        );
        previous_bottom = label_y + 16.;
    }
}

fn prepare_preview(image: &Image, cancel: &AtomicBool) -> ToolResult<Arc<RenderImage>> {
    check_cancel(cancel)?;
    let scale = (960. / f64::from(image.width))
        .min(600. / f64::from(image.height))
        .min(1.);
    let width = (f64::from(image.width) * scale).round().max(1.) as u32;
    let height = (f64::from(image.height) * scale).round().max(1.) as u32;
    let preview = resize(
        image,
        width,
        height,
        ResizeFilter::Bilinear,
        ToolLimits::default(),
        cancel,
    )?;
    check_cancel(cancel)?;
    Ok(crate::canvas_source::render_image(
        width,
        height,
        preview.bgra,
    ))
}
