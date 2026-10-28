//! 图片工具视图的宿主：独立窗口或主界面弹层。视图只通过这里关闭、改标题；
//! 独立窗口按工具复用，同一工具再次打开时在原窗口里换上新内容。
use crate::Result;
use gpui::{
    px, size, AnyWindowHandle, App, AppContext, Bounds, Context, Entity, EventEmitter, FocusHandle,
    Global, Render, SharedString, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{collections::HashMap, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolHost {
    Window,
    Popup,
}

/// 弹层里的工具发给宿主的请求。
pub(crate) enum ToolEvent {
    /// 后台任务已归还，宿主可以移除它。
    Close,
    /// 回到图库按当前来源继续挑图，挑完由宿主调用 [`HostedTool::set_sources`]。
    PickSources,
}

pub(crate) trait HostedTool: Render + EventEmitter<ToolEvent> + Sized {
    /// 正在处理或保存；此时不在原窗口里换内容，也不直接关闭。
    fn working(&self) -> bool;
    /// 请求关闭并取消后台任务；返回 false 时任务归还后由视图调用 [`finish_close`]。
    fn request_close(&mut self, cx: &mut Context<Self>) -> bool;
    /// 工具的键盘焦点；弹层收起再展开后交还给它。
    fn focus_handle(&self) -> FocusHandle;
    /// 当前按顺序使用的文件来源；单图工具和没有文件来源的工具为空。
    fn sources(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    /// 换成图库里重新挑好的来源（已按挑选顺序）。
    fn set_sources(&mut self, _paths: Vec<PathBuf>, _window: &mut Window, _cx: &mut Context<Self>) {
    }
}

/// 来源列表的“添加”：弹层回图库挑，独立窗口返回 false 由调用方弹文件框。
pub(crate) fn request_pick<V: HostedTool>(host: ToolHost, cx: &mut Context<V>) -> bool {
    if host == ToolHost::Popup {
        cx.emit(ToolEvent::PickSources);
        true
    } else {
        false
    }
}

/// 视图确认可以关闭：独立窗口移除自身，弹层通知宿主。
pub(crate) fn finish_close<V: HostedTool>(
    host: ToolHost,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    match host {
        ToolHost::Window => window.remove_window(),
        ToolHost::Popup => cx.emit(ToolEvent::Close),
    }
}

/// 独立窗口才改系统标题；弹层的标题由宿主画在弹层顶栏。
pub(crate) fn set_title(host: ToolHost, window: &mut Window, title: impl Into<SharedString>) {
    if host == ToolHost::Window {
        window.set_window_title(&title.into());
    }
}

#[derive(Default)]
struct ToolWindows(HashMap<&'static str, AnyWindowHandle>);
impl Global for ToolWindows {}

/// 打开某个工具的独立窗口。同一工具已有空闲窗口时在原处换上新内容并激活，
/// 位置和大小不变；原窗口正在处理时另开一个，不打断它。
pub(crate) fn open_window<V: HostedTool>(
    kind: &'static str,
    title: SharedString,
    preferred: (f32, f32),
    minimum: (f32, f32),
    cx: &mut App,
    build: impl FnOnce(ToolHost, &mut Window, &mut Context<V>) -> V + 'static,
) -> Result<WindowHandle<V>> {
    crate::theme::install(cx);
    let mut build = Some(build);
    let existing = cx
        .default_global::<ToolWindows>()
        .0
        .get(kind)
        .and_then(|handle| handle.downcast::<V>());
    if let Some(handle) = existing {
        let idle = handle
            .read_with(cx, |view, _| !view.working())
            .unwrap_or(false);
        if idle {
            let reused = handle.update(cx, |_, window, cx| {
                let build = build.take().expect("tool builder");
                let view =
                    window.replace_root(cx, |window, cx| build(ToolHost::Window, window, cx));
                watch_close(&view, window, cx);
                window.set_window_title(&title);
                crate::gpui_capture::foreground_tool(window);
            });
            if reused.is_ok() {
                return Ok(handle);
            }
            // 窗口已经不在（关闭途中），建造函数没有被用掉，照常另开。
        }
    }
    let dimensions =
        cx.primary_display()
            .map_or(size(px(preferred.0), px(preferred.1)), |display| {
                let bounds = display.bounds();
                size(
                    px(preferred
                        .0
                        .min((f32::from(bounds.size.width) - 48.).max(minimum.0))),
                    px(preferred
                        .1
                        .min((f32::from(bounds.size.height) - 72.).max(minimum.1))),
                )
            });
    let bounds = Bounds::centered(None, dimensions, cx);
    let build = build.take().expect("tool builder");
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        move |window, cx| {
            window.set_window_title(&title);
            crate::gpui_capture::foreground_tool(window);
            let view = cx.new(|cx| build(ToolHost::Window, window, cx));
            watch_close(&view, window, cx);
            view
        },
    )?;
    cx.default_global::<ToolWindows>()
        .0
        .insert(kind, handle.into());
    Ok(handle)
}

/// 系统关闭按钮走视图自己的关闭流程：先取消后台任务，归还后再移除窗口。
fn watch_close<V: HostedTool>(view: &Entity<V>, window: &mut Window, cx: &mut App) {
    let weak = view.downgrade();
    window.on_window_should_close(cx, move |_, cx| {
        weak.update(cx, |view, cx| view.request_close(cx))
            .unwrap_or(true)
    });
}
