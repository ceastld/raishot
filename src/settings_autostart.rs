//! 开机启动设置；状态以当前用户启动项与 Windows“启动应用”为准，不写入 settings.ini。
use crate::{autostart::AutostartState, settings_view::SettingsView, settings_widgets as widgets};
use gpui::{prelude::*, rgb, Context};
use std::path::Path;

impl SettingsView {
    pub(crate) fn refresh_autostart(&mut self) {
        self.autostart = crate::autostart::state().map_err(|error| error.to_string());
    }
    fn apply_autostart(&mut self, enabled: bool) {
        match crate::autostart::set(enabled) {
            Ok(state) => {
                self.status = match (&state, enabled) {
                    (AutostartState::On, true) => {
                        trf!("已开启开机启动：登录 Windows 后 {} 在托盘常驻。", "Launch at startup is on: {} stays in the tray after you sign in to Windows.", crate::brand::NAME)
                    }
                    (AutostartState::Off, false) => {
                        tr!("已关闭开机启动。", "Launch at startup is off.").into()
                    }
                    _ => tr!(
                        "开机启动状态已更新，请查看启动设置说明。",
                        "Startup state updated; see the startup notes for details."
                    )
                    .into(),
                };
                self.autostart = Ok(state);
            }
            Err(error) => {
                self.status = trf!(
                    "开机启动设置失败：{error}",
                    "Failed to change launch at startup: {error}"
                );
                self.refresh_autostart();
            }
        }
    }
    pub(crate) fn autostart_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let enabled = matches!(
            self.autostart,
            Ok(AutostartState::On | AutostartState::OtherLocation(_))
        );
        let mut rows = vec![widgets::row(
            tr!("开机自动启动", "Launch at startup"),
            Some(tr!("登录 Windows 后在托盘常驻，不会自动开始截图；仅对当前用户生效。", "Stays in the tray after you sign in to Windows without starting a capture; applies to the current user only.").into()),
            palette,
        )
        .id("settings-autostart")
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(palette.hover)))
        .child(widgets::switch(enabled, palette, surface))
        .child(self.settings_probe("settings-autostart"))
        .on_click(cx.listener(move |view, _, _, cx| {
            view.apply_autostart(!enabled);
            cx.notify();
        }))
        .into_any_element()];
        match &self.autostart {
            Ok(AutostartState::Off | AutostartState::On) => {}
            Ok(AutostartState::OtherLocation(program)) => rows.push(
                widgets::row(
                    if Path::new(program).exists() {
                        tr!(
                            "启动项指向另一份程序",
                            "The startup entry points to another copy of the program"
                        )
                    } else {
                        tr!(
                            "启动项指向的程序已不存在",
                            "The program referenced by the startup entry no longer exists"
                        )
                    },
                    Some(program.clone().into()),
                    palette,
                )
                .child(
                    widgets::button(
                        "settings-autostart-relocate",
                        tr!("改为当前程序", "Use this program"),
                        widgets::ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .child(self.settings_probe("settings-autostart-relocate"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.apply_autostart(true);
                        cx.notify();
                    })),
                )
                .into_any_element(),
            ),
            Ok(AutostartState::BlockedBySystem) => rows.push(
                widgets::row(
                    tr!(
                        "已在 Windows“启动应用”中关闭",
                        "Disabled in Windows Startup Apps"
                    ),
                    Some(
                        trf!(
                            "打开上方开关会重新启用 {} 的启动项。",
                            "Turning on the switch above re-enables {}'s startup entry.",
                            crate::brand::NAME
                        )
                        .into(),
                    ),
                    palette,
                )
                .into_any_element(),
            ),
            Err(error) => rows.push(
                widgets::row(
                    tr!("无法读取开机启动状态", "Cannot read the startup state"),
                    Some(error.clone().into()),
                    palette,
                )
                .into_any_element(),
            ),
        }
        widgets::section(tr!("启动", "Startup"), palette).child(widgets::card(surface, rows))
    }
}
