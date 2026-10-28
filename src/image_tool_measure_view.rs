//! 取色与测量窗口：左侧像素、测距与采样色，右侧带标尺的图像；标记只在窗口绘制，不修改输入像素。
#[path = "image_tool_colors.rs"]
mod colors;
#[path = "image_tool_measure.rs"]
mod model;

use self::colors::{ColorFormat, ColorValue};
use self::model::{measure, ruler_ticks, sample, ContentRect, PixelPoint, PixelSample};
use crate::{
    capture::Image,
    image_resize::{resize, ResizeFilter},
    image_tool_view::{
        host::{self, HostedTool, ToolEvent, ToolHost},
        single::SingleImage,
    },
    image_tools::{check_cancel, ProcessedImage, ToolError, ToolLimits, ToolResult},
    settings_widgets::{self as widgets, ButtonKind},
    text_field::{TextField, TextFieldEvent},
    tool_window_chrome::{self as chrome, Tone},
    toolbar_icons::Icon,
};
use gpui::{
    canvas, div, point, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context,
    ElementId, Entity, EventEmitter, FocusHandle, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, PathBuilder, Render, RenderImage, Subscription, Window,
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
    host: ToolHost,
}

/// 嵌进单图工具窗或主界面弹层；源图与另外两个单图工具共用。
pub(crate) fn embedded(
    source: SingleImage,
    host: ToolHost,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ImageMeasureView> {
    let source = match source {
        SingleImage::Shared(image) => Source::Shared(image),
        SingleImage::Processed(image) => Source::Processed(image),
    };
    cx.new(|cx| ImageMeasureView::new(source, host, window, cx))
}

impl HostedTool for ImageMeasureView {
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
    fn working(&self) -> bool {
        self.active.is_some()
    }
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        ImageMeasureView::request_close(self, cx)
    }
}

impl EventEmitter<ToolEvent> for ImageMeasureView {}

impl ImageMeasureView {
    #[allow(clippy::too_many_arguments)]
    fn new(source: Source, host: ToolHost, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let applied_input = "#4A90D9".to_owned();
        let color_input =
            cx.new(|cx| TextField::new(applied_input.clone(), "#RRGGBB / rgb(0, 0, 0)", cx));
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
            host,
        };
        view.start_preview(window, cx);
        view
    }
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
                    host::finish_close(view.host, window, cx);
                    return;
                }
                match result {
                    Ok(preview) if !cancel.load(Ordering::Acquire) => {
                        view.preview = Some(preview);
                        view.notice = tr!(
                            "移到图像上查看像素，点击取色；切到“测距”量两点距离。",
                            "Hover to inspect pixels and click to pick; switch to Measure for distances."
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
                self.second = None;
                self.mode = PickMode::Second;
            }
            PickMode::Second => {
                self.second = Some(color.point);
                self.mode = PickMode::First;
            }
        }
        cx.notify();
    }

    fn control(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        match control {
            Control::Close => {
                if self.request_close(cx) {
                    host::finish_close(self.host, window, cx);
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
                if self.mode == PickMode::Second {
                    self.mode = PickMode::First;
                }
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

    fn set_mode(&mut self, measuring: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mode = match (measuring, self.mode) {
            (false, _) => PickMode::Color,
            (true, PickMode::Color) => PickMode::First,
            (true, mode) => mode,
        };
        self.control(Control::Mode(mode), window, cx);
    }
    fn set_format(&mut self, format: ColorFormat, window: &mut Window, cx: &mut Context<Self>) {
        self.control(Control::Format(format), window, cx);
    }

    /// 侧栏里的一行颜色：色块、所选格式的值、坐标说明，右侧放操作。
    fn color_row(
        &self,
        id: impl Into<ElementId>,
        color: ColorValue,
        detail: Option<String>,
        c: chrome::Chrome,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .min_h(px(40.))
            .px_2()
            .rounded_md()
            .hover(move |style| style.bg(rgb(c.surface.nav_hover)))
            .child(swatch(color, 24., c))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .child(color.formatted(self.color_format)),
                    )
                    .when_some(detail, |text, detail| {
                        text.child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(c.palette.muted))
                                .child(detail),
                        )
                    }),
            )
    }

    fn preview_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut frame = div()
            .id("measure-image")
            .relative()
            .size_full()
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
            frame = frame.cursor_crosshair().child(
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
            let c = chrome::chrome(cx);
            frame = frame
                .flex()
                .items_center()
                .justify_center()
                .child(chrome::empty_state(
                    Icon::Eyedropper,
                    if self.active.is_some() {
                        tr!("正在准备预览…", "Preparing preview…")
                    } else {
                        tr!("预览未就绪", "Preview is not ready")
                    },
                    c,
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
        host::set_title(
            self.host,
            window,
            brand_title!("取色与测量", "Color & measure"),
        );
        let c = chrome::chrome(cx);
        let ready = self.preview.is_some() && self.active.is_none() && !self.closing;
        let converter_ready = self.active.is_none() && !self.closing;
        let measuring = self.mode != PickMode::Color;
        let image = self.source.image();

        // —— 模式 ——
        let mode = chrome::field(tr!("模式", "Mode"), c)
            .child(chrome::choice(
                "measure-mode",
                &[
                    (tr!("取色", "Pick color"), false),
                    (tr!("测距", "Measure"), true),
                ],
                measuring,
                ready,
                c,
                cx,
                Self::set_mode,
            ))
            .child(chrome::hint(
                match self.mode {
                    PickMode::Color => tr!(
                        "点击图像保留颜色，最多 8 个。",
                        "Click the image to keep a color (up to 8)."
                    ),
                    PickMode::First => tr!("点击起点 A。", "Click the start point A."),
                    PickMode::Second => tr!("点击终点 B。", "Click the end point B."),
                },
                c,
            ));

        // —— 当前像素 ——
        let probe = match self.probe {
            Some(color) => self
                .color_row(
                    "measure-probe",
                    color_value(color),
                    Some(pixel_detail(color)),
                    c,
                )
                .child(chrome::row_icon(
                    "measure-use-probe",
                    Icon::Edit,
                    ready,
                    c,
                    cx,
                    |view, window, cx| view.control(Control::UseProbe, window, cx),
                ))
                .child(chrome::row_icon(
                    "measure-copy-probe",
                    Icon::Copy,
                    ready,
                    c,
                    cx,
                    |view, window, cx| view.control(Control::CopyProbe, window, cx),
                ))
                .into_any_element(),
            None => chrome::hint(
                tr!(
                    "把鼠标移到图像上查看像素。",
                    "Hover the image to inspect a pixel."
                ),
                c,
            )
            .into_any_element(),
        };
        let format = chrome::field(tr!("当前像素", "Pixel"), c)
            .child(probe)
            .child(chrome::choice(
                "measure-format",
                &[
                    ("HEX", ColorFormat::Hex),
                    ("RGB", ColorFormat::Rgb),
                    ("HSL", ColorFormat::Hsl),
                    ("HSV", ColorFormat::Hsv),
                ],
                self.color_format,
                converter_ready,
                c,
                cx,
                Self::set_format,
            ));

        let mut sidebar = chrome::sidebar("measure-sidebar", c)
            .child(mode)
            .child(format);

        // —— 测距结果 ——
        if measuring || self.first.is_some() {
            let point_text = |point: Option<PixelPoint>| {
                point.map_or_else(
                    || tr!("未选择", "Not set").to_owned(),
                    |p| format!("({}, {})", p.x, p.y),
                )
            };
            let mut distance = chrome::field(tr!("测距", "Distance"), c)
                .child(value_row(
                    tr!("起点 A", "Start A"),
                    point_text(self.first),
                    c,
                ))
                .child(value_row(
                    tr!("终点 B", "End B"),
                    point_text(self.second),
                    c,
                ));
            if let (Some(a), Some(b)) = (self.first, self.second) {
                let result = measure(a, b);
                distance = distance
                    .child(value_row(
                        tr!("距离", "Length"),
                        format!("{:.2} px", result.distance),
                        c,
                    ))
                    .child(value_row(
                        "Δx · Δy",
                        format!("{:.0} · {:.0} px", result.dx, result.dy),
                        c,
                    ))
                    .child(value_row(
                        tr!("角度", "Angle"),
                        result
                            .angle_degrees
                            .map_or_else(|| "—".to_owned(), |angle| format!("{angle:.1}°")),
                        c,
                    ));
            }
            let has_points = self.first.is_some() || self.second.is_some();
            let copy = chrome::action(
                "measure-copy-distance",
                tr!("复制", "Copy"),
                ButtonKind::Secondary,
                ready && self.first.is_some() && self.second.is_some(),
                c,
            );
            let clear = chrome::action(
                "measure-clear-points",
                tr!("清除", "Clear"),
                ButtonKind::Secondary,
                ready && has_points,
                c,
            );
            distance = distance.child(
                div()
                    .flex()
                    .gap_2()
                    .child(if ready && self.first.is_some() && self.second.is_some() {
                        copy.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::CopyMeasurement, window, cx)
                        }))
                    } else {
                        copy
                    })
                    .child(if ready && has_points {
                        clear.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::ClearPoints, window, cx)
                        }))
                    } else {
                        clear
                    }),
            );
            sidebar = sidebar.child(distance);
        }

        // —— 采样色 ——
        let mut samples = chrome::field(
            trf!(
                "采样色 · {} / {MAX_SAMPLES}",
                "Samples · {} / {MAX_SAMPLES}",
                self.samples.len()
            ),
            c,
        );
        if self.samples.is_empty() {
            samples = samples.child(chrome::hint(
                tr!(
                    "取色模式下点击图像添加。",
                    "Click the image in color mode to add."
                ),
                c,
            ));
        } else {
            for (index, color) in self.samples.iter().enumerate().rev() {
                samples = samples.child(
                    self.color_row(
                        ("measure-sample", index),
                        color_value(*color),
                        Some(format!("({}, {})", color.point.x, color.point.y)),
                        c,
                    )
                    .child(chrome::row_icon(
                        ("measure-use-color", index),
                        Icon::Edit,
                        ready,
                        c,
                        cx,
                        move |view, window, cx| view.control(Control::UseColor(index), window, cx),
                    ))
                    .child(chrome::row_icon(
                        ("measure-copy-color", index),
                        Icon::Copy,
                        ready,
                        c,
                        cx,
                        move |view, window, cx| view.control(Control::CopyColor(index), window, cx),
                    )),
                );
            }
            let copy_all = chrome::action(
                "measure-copy-palette",
                tr!("复制全部", "Copy all"),
                ButtonKind::Secondary,
                ready,
                c,
            );
            let clear = chrome::action(
                "measure-clear-colors",
                tr!("清空", "Clear"),
                ButtonKind::Secondary,
                ready,
                c,
            );
            samples = samples.child(
                div()
                    .flex()
                    .gap_2()
                    .child(if ready {
                        copy_all.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::CopyPalette, window, cx)
                        }))
                    } else {
                        copy_all
                    })
                    .child(if ready {
                        clear.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::ClearColors, window, cx)
                        }))
                    } else {
                        clear
                    }),
            );
        }
        sidebar = sidebar.child(samples);

        // —— 颜色转换 ——
        let convert = chrome::action(
            "measure-convert-color",
            tr!("转换", "Convert"),
            ButtonKind::Secondary,
            converter_ready,
            c,
        );
        let dirty = self.color_input.read(cx).text() != self.applied_input;
        let converter = chrome::field(tr!("颜色转换", "Convert color"), c)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w(px(0.)).child(self.color_input.clone()))
                    .child(if converter_ready {
                        convert.on_click(cx.listener(|view, _, window, cx| {
                            view.control(Control::ConvertColor, window, cx)
                        }))
                    } else {
                        convert
                    }),
            )
            .child(
                self.color_row("measure-converted", self.converted, None, c)
                    .child(chrome::row_icon(
                        "measure-copy-converted",
                        Icon::Copy,
                        converter_ready,
                        c,
                        cx,
                        |view, window, cx| view.control(Control::CopyConverted, window, cx),
                    )),
            )
            .child(chrome::hint(
                if dirty {
                    tr!(
                        "按回车或点“转换”更新结果。",
                        "Press Enter or Convert to update."
                    )
                } else {
                    tr!(
                        "支持 HEX、rgb()、hsl()、hsv()，带透明度用 rgba/hsla/hsva。",
                        "Accepts HEX, rgb(), hsl(), hsv(); use rgba/hsla/hsva for alpha."
                    )
                },
                c,
            ));
        sidebar = sidebar.child(converter);

        // —— 预览 ——
        let rulers = widgets::chip(
            "measure-rulers",
            tr!("标尺", "Rulers"),
            self.rulers,
            c.palette,
            c.surface,
        );
        let bar = chrome::stage_bar(c)
            .child(div().flex_1().min_w(px(0.)).truncate().child(trf!(
                "{} × {} 像素 · 坐标从左上 (0, 0) 起",
                "{} × {} px · origin at top-left (0, 0)",
                image.width,
                image.height
            )))
            .child(if ready {
                rulers.on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Rulers, window, cx)),
                )
            } else {
                rulers.opacity(0.45)
            });
        let main = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .px_5()
            .pb_5()
            .pt_2()
            .child(bar)
            .child(
                chrome::stage(c)
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.preview_panel(cx)),
            );

        // —— 底栏 ——
        let (status, tone) = if self.failed {
            (self.notice.clone(), Tone::Failed)
        } else if self.active.is_some() {
            (self.notice.clone(), Tone::Busy)
        } else if let Some(color) = self.probe {
            (pixel_detail(color), Tone::Idle)
        } else {
            (self.notice.clone(), Tone::Idle)
        };
        let mut actions = Vec::new();
        if self.active.is_some() && !self.closing {
            actions.push(
                chrome::action(
                    "measure-cancel",
                    tr!("取消", "Cancel"),
                    ButtonKind::Secondary,
                    true,
                    c,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Cancel, window, cx)),
                )
                .into_any_element(),
            );
        } else if self.preview.is_none() && !self.closing {
            actions.push(
                chrome::action(
                    "measure-preview",
                    tr!("重新准备预览", "Retry preview"),
                    ButtonKind::Primary,
                    true,
                    c,
                )
                .on_click(
                    cx.listener(|view, _, window, cx| view.control(Control::Preview, window, cx)),
                )
                .into_any_element(),
            );
        }

        chrome::shell(c)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                if view.color_input.read(cx).is_composing() {
                    return;
                }
                let keystroke = &event.keystroke;
                if keystroke.key == "escape" && !keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    if !event.is_held {
                        view.control(
                            if view.active.is_some() {
                                Control::Cancel
                            } else {
                                Control::Close
                            },
                            window,
                            cx,
                        );
                    }
                } else if keystroke.key == "c"
                    && keystroke.modifiers.control
                    && view.focus.is_focused(window)
                {
                    // 窗口焦点上 Ctrl+C 复制鼠标下的像素；输入框里仍复制文字。
                    cx.stop_propagation();
                    view.control(Control::CopyProbe, window, cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(sidebar)
                    .child(main),
            )
            .child(chrome::footer(status, tone, actions, c))
            .into_any_element()
    }
}

fn swatch(color: ColorValue, side: f32, c: chrome::Chrome) -> gpui::Div {
    let value = (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b);
    div()
        .size(px(side))
        .flex_shrink_0()
        .rounded_md()
        .border_1()
        .border_color(rgb(c.surface.line))
        .bg(rgb(value))
}

fn value_row(label: impl Into<gpui::SharedString>, value: String, c: chrome::Chrome) -> gpui::Div {
    chrome::option_row(label, c).child(
        div()
            .text_size(px(13.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(value),
    )
}

fn pixel_detail(color: PixelSample) -> String {
    if color.alpha == 255 {
        format!(
            "({}, {}) · RGB {} {} {}",
            color.point.x, color.point.y, color.r, color.g, color.b
        )
    } else {
        format!(
            "({}, {}) · RGB {} {} {} · α {}",
            color.point.x, color.point.y, color.r, color.g, color.b, color.alpha
        )
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
