//! 图库顶栏“新建 ▾”：往图库里添内容的入口——截图、录屏、剪贴板贴图、生成二维码、加入图片文件；
//! 和“工具 ▾”（对已选内容的动作）分开。截图与贴图由托盘常驻进程执行，主窗口只发请求。
use super::{
    tools::{action_button, menu_item, menu_panel, menu_separator, MenuOrigin},
    widgets, SettingsView,
};
use crate::toolbar_icons::{self, Icon};
use gpui::{div, prelude::*, px, rgb, Context, Div, SharedString, Window};
use std::time::Duration;

/// 托盘里的截图入口；数值与托盘菜单的“区域截图”“当前屏幕”一致。
#[derive(Clone, Copy)]
enum Capture {
    Region,
    Screen,
}

/// 主窗口最小化动画结束后再冻结桌面，免得截到正在收起的主窗口。
const MINIMIZE_SETTLE: Duration = Duration::from_millis(300);

/// 找到托盘常驻窗口并允许它切到前台；托盘没在运行时返回 None。
fn tray_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::{
        core::w,
        Win32::UI::WindowsAndMessaging::{
            AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId,
        },
    };
    unsafe {
        let window = FindWindowW(w!("RustShot.MVP.Window"), None).ok()?;
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        Some(window)
    }
}

fn post_tray(message: u32, parameter: usize) -> bool {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::PostMessageW,
    };
    tray_window().is_some_and(|window| unsafe {
        PostMessageW(Some(window), message, WPARAM(parameter), LPARAM(0)).is_ok()
    })
}

fn tray_missing() -> String {
    crate::trf!(
        "{} 托盘没有运行，无法截图或贴图；请先启动它",
        "The {} tray isn't running; start it to capture or pin",
        crate::brand::NAME
    )
}

impl SettingsView {
    /// “新建 ▾”按钮连同菜单；和“工具”共用展开状态与菜单外框。
    pub(super) fn history_new_trigger(&self, cx: &mut Context<Self>) -> Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let open = self.history.tools.menu == Some(MenuOrigin::New);
        let button = action_button(
            "history-new",
            Some(Icon::Capture),
            crate::tr!("新建", "New"),
            false,
            palette,
            surface,
        )
        .child(
            toolbar_icons::action(
                if open { Icon::ChevronUp } else { Icon::Chevron },
                palette.muted,
            )
            .size(px(12.)),
        )
        .on_click(cx.listener(|view, _, _, cx| {
            view.history.tools.menu = if view.history.tools.menu == Some(MenuOrigin::New) {
                None
            } else {
                Some(MenuOrigin::New)
            };
            cx.notify();
        }));
        let menu = open.then(|| self.history_new_menu(cx).into_any_element());
        self.history_menu_slot(MenuOrigin::New, button, menu, cx)
    }

    fn history_new_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let item = |id: &'static str, icon: Icon, title: &'static str, hint: String| {
            menu_item(SharedString::from(id), icon, title, hint, true, palette)
        };
        menu_panel("history-new-menu", cx)
            .child(
                item(
                    "history-new-region",
                    Icon::RegionRect,
                    crate::tr!("区域截图", "Capture region"),
                    String::new(),
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    view.history_new_capture(Capture::Region, window, cx)
                })),
            )
            .child(
                item(
                    "history-new-screen",
                    Icon::Monitor,
                    crate::tr!("截取当前屏幕", "Capture current screen"),
                    String::new(),
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    view.history_new_capture(Capture::Screen, window, cx)
                })),
            )
            .child(
                // 录屏从区域截图开始：框好区域后在工具栏点“录屏”或按 G。
                item(
                    "history-new-record",
                    Icon::Video,
                    crate::tr!("录屏", "Record screen"),
                    crate::tr!("框选后按 G", "Select, then G").into(),
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    view.history_new_capture(Capture::Region, window, cx)
                })),
            )
            .child(
                item(
                    "history-new-clipboard",
                    Icon::Copy,
                    crate::tr!("从剪贴板贴图", "Pin from clipboard"),
                    String::new(),
                )
                .on_click(cx.listener(|view, _, _, cx| view.history_new_clipboard_pin(cx))),
            )
            .child(menu_separator(surface))
            .child(
                item(
                    "history-new-qr",
                    Icon::Qr,
                    crate::tr!("生成二维码", "Create QR code"),
                    String::new(),
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.history.tools.menu = None;
                    view.open_tool_popup(
                        None,
                        crate::tr!("生成二维码", "Create QR code"),
                        crate::image_tool_view::qr_view::popup,
                        cx,
                    );
                })),
            )
            .child(
                item(
                    "history-new-files",
                    Icon::Image,
                    crate::tr!("加入图片文件…", "Add image files…"),
                    "Ctrl+O".into(),
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.history.tools.menu = None;
                    view.history_choose_files(cx);
                })),
            )
            .child(div().h(px(2.)))
            .child(
                div()
                    .px(px(10.))
                    .pb_1()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(crate::tr!(
                        "截图前主窗口先收起；完成的截图和贴图会出现在图库里。",
                        "The main window minimizes first; new captures and pins appear here."
                    )),
            )
    }

    /// 先收起主窗口，等动画结束再请托盘开始截图。
    fn history_new_capture(&mut self, kind: Capture, window: &mut Window, cx: &mut Context<Self>) {
        self.history.tools.menu = None;
        if tray_window().is_none() {
            self.status = tray_missing();
            cx.notify();
            return;
        }
        window.minimize_window();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MINIMIZE_SETTLE).await;
            let parameter = match kind {
                Capture::Region => 0,
                Capture::Screen => 1,
            };
            if !post_tray(crate::ui::CAPTURE_MESSAGE, parameter) {
                let _ = this.update(cx, |view, cx| {
                    view.status = tray_missing();
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// 请托盘按剪贴板内容贴图；贴图出现后稍等重扫，让它进图库。
    fn history_new_clipboard_pin(&mut self, cx: &mut Context<Self>) {
        self.history.tools.menu = None;
        self.status = if post_tray(crate::ui::PIN_MESSAGE, crate::tray::PIN_CLIPBOARD as usize) {
            self.history_rescan_later(cx);
            crate::tr!("已请求贴出剪贴板内容", "Pinning the clipboard").into()
        } else {
            tray_missing()
        };
        cx.notify();
    }
}
