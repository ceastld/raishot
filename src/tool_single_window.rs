//! 单图工具（分割、取色与测量、识别条码）的独立窗口：顶栏三段切换，三个工具共用同一份源图，
//! 切换只在窗口里换工具视图，不重读文件、不复制像素。主界面弹层在自己的顶栏里做同样的切换。
use super::{
    barcode_view::{self, ImageBarcodeView},
    host::{self, HostedTool, ToolEvent, ToolHost},
    measure_view::{self, ImageMeasureView},
    split_view::{self, ImageSplitView},
};
use crate::{
    capture::Image,
    image_tools::{validate_input, ProcessedImage, ToolLimits},
    tool_registry::ToolId,
    tool_window_chrome as chrome, Result,
};
use gpui::{
    div, prelude::*, px, rgb, AnyView, App, Context, Entity, EventEmitter, FocusHandle,
    SharedString, Subscription, Window, WindowHandle,
};
use std::{path::PathBuf, sync::Arc};

/// 单图工具的只读来源：图库或文件里解码好的像素，或图片处理的当前产物（保留未保存的调整）。
#[derive(Clone)]
pub(crate) enum SingleImage {
    Shared(Arc<Image>),
    Processed(Arc<ProcessedImage>),
}

impl SingleImage {
    fn image(&self) -> &Image {
        match self {
            Self::Shared(image) => image,
            Self::Processed(image) => image.image(),
        }
    }
}

/// 可以互切的单图工具，按切换栏顺序。
pub(crate) const TOOLS: [ToolId; 3] = [ToolId::Split, ToolId::Measure, ToolId::Barcode];

/// 打开单图工具窗；已有空闲的单图工具窗时在原窗口里换上新来源和工具。
pub(crate) fn open(
    tool: ToolId,
    source: SingleImage,
    protected: Arc<Vec<PathBuf>>,
    cx: &mut App,
) -> Result<WindowHandle<SingleToolWindow>> {
    if !TOOLS.contains(&tool) {
        return Err(tr!("该工具不是单图工具", "This is not a single-image tool").into());
    }
    validate_input(source.image(), ToolLimits::default())?;
    host::open_window(
        "single",
        title(tool),
        (1060., 904.),
        (440., 464.),
        cx,
        move |_, window, cx| SingleToolWindow::new(tool, source, protected, window, cx),
    )
}

fn title(tool: ToolId) -> SharedString {
    match tool {
        ToolId::Split => brand_title!("图片分割", "Split image"),
        ToolId::Barcode => brand_title!("识别条码", "Read barcodes"),
        _ => brand_title!("取色与测量", "Color & measure"),
    }
    .into()
}

enum Active {
    Split(Entity<ImageSplitView>),
    Measure(Entity<ImageMeasureView>),
    Barcode(Entity<ImageBarcodeView>),
}

impl Active {
    fn build(
        tool: ToolId,
        source: &SingleImage,
        protected: &Arc<Vec<PathBuf>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Self> {
        let source = source.clone();
        Ok(match tool {
            ToolId::Split => Self::Split(split_view::embedded(
                source,
                protected.clone(),
                ToolHost::Window,
                window,
                cx,
            )?),
            ToolId::Barcode => Self::Barcode(barcode_view::embedded(
                source,
                ToolHost::Window,
                window,
                cx,
            )?),
            _ => Self::Measure(measure_view::embedded(source, ToolHost::Window, window, cx)),
        })
    }

    fn tool(&self) -> ToolId {
        match self {
            Self::Split(_) => ToolId::Split,
            Self::Measure(_) => ToolId::Measure,
            Self::Barcode(_) => ToolId::Barcode,
        }
    }

    fn view(&self) -> AnyView {
        match self {
            Self::Split(view) => view.clone().into(),
            Self::Measure(view) => view.clone().into(),
            Self::Barcode(view) => view.clone().into(),
        }
    }

    fn working(&self, cx: &App) -> bool {
        match self {
            Self::Split(view) => HostedTool::working(view.read(cx)),
            Self::Measure(view) => HostedTool::working(view.read(cx)),
            Self::Barcode(view) => HostedTool::working(view.read(cx)),
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self {
            Self::Split(view) => HostedTool::focus_handle(view.read(cx)),
            Self::Measure(view) => HostedTool::focus_handle(view.read(cx)),
            Self::Barcode(view) => HostedTool::focus_handle(view.read(cx)),
        }
    }

    fn request_close(&self, cx: &mut App) -> bool {
        match self {
            Self::Split(view) => view.update(cx, |view, cx| HostedTool::request_close(view, cx)),
            Self::Measure(view) => view.update(cx, |view, cx| HostedTool::request_close(view, cx)),
            Self::Barcode(view) => view.update(cx, |view, cx| HostedTool::request_close(view, cx)),
        }
    }

    /// 工具开始或结束处理时刷新切换栏的可用状态。
    fn observe(&self, cx: &mut Context<SingleToolWindow>) -> Subscription {
        fn changed<V>(
            view: &mut SingleToolWindow,
            _: Entity<V>,
            cx: &mut Context<SingleToolWindow>,
        ) {
            let working = view.active.working(cx);
            if working != view.working {
                view.working = working;
                cx.notify();
            }
        }
        match self {
            Self::Split(view) => cx.observe(view, changed),
            Self::Measure(view) => cx.observe(view, changed),
            Self::Barcode(view) => cx.observe(view, changed),
        }
    }
}

pub(crate) struct SingleToolWindow {
    source: SingleImage,
    protected: Arc<Vec<PathBuf>>,
    active: Active,
    focus: FocusHandle,
    working: bool,
    notice: Option<SharedString>,
    _observe: Subscription,
}

impl EventEmitter<ToolEvent> for SingleToolWindow {}

impl HostedTool for SingleToolWindow {
    fn working(&self) -> bool {
        self.working
    }
    /// 关闭交给当前工具：它取消完后台任务后自己移除窗口。
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        self.active.request_close(cx)
    }
    fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }
}

impl SingleToolWindow {
    fn new(
        tool: ToolId,
        source: SingleImage,
        protected: Arc<Vec<PathBuf>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 打开前已校验过源图；这里仍失败时退回不做校验的取色与测量，并在顶栏说明。
        let (active, notice) = match Active::build(tool, &source, &protected, window, cx) {
            Ok(active) => (active, None),
            Err(error) => (
                Active::Measure(measure_view::embedded(
                    source.clone(),
                    ToolHost::Window,
                    window,
                    cx,
                )),
                Some(error.to_string().into()),
            ),
        };
        Self {
            _observe: active.observe(cx),
            focus: active.focus_handle(cx),
            working: active.working(cx),
            source,
            protected,
            active,
            notice,
        }
    }

    /// 当前工具空闲才换；新工具接过焦点和窗口标题。
    fn switch(&mut self, tool: ToolId, window: &mut Window, cx: &mut Context<Self>) {
        if tool == self.active.tool() || self.active.working(cx) {
            return;
        }
        match Active::build(tool, &self.source, &self.protected, window, cx) {
            Ok(active) => {
                self._observe = active.observe(cx);
                self.focus = active.focus_handle(cx);
                self.working = active.working(cx);
                self.active = active;
                self.notice = None;
                window.focus(&self.focus);
                window.set_window_title(&title(tool));
            }
            Err(error) => self.notice = Some(error.to_string().into()),
        }
        cx.notify();
    }
}

impl Render for SingleToolWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = chrome::chrome(cx);
        let options = TOOLS.map(|tool| (tool.title(), tool));
        let notice = self.notice.clone().or_else(|| {
            self.working.then(|| {
                tr!(
                    "当前工具正在处理，完成后可切换",
                    "The current tool is working; switch when it finishes"
                )
                .into()
            })
        });
        chrome::shell(c)
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(rgb(c.surface.line))
                    .bg(rgb(c.surface.card))
                    .child(chrome::choice(
                        "single-tool-switch",
                        &options,
                        self.active.tool(),
                        !self.working,
                        c,
                        cx,
                        Self::switch,
                    ))
                    .when_some(notice, |bar, notice| {
                        bar.child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(c.palette.muted))
                                .child(notice),
                        )
                    }),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .w_full()
                    .overflow_hidden()
                    .child(self.active.view()),
            )
    }
}
