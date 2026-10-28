//! 主界面导航：上半是内容页，下半是设置分类。
use super::SettingsView;
use crate::toolbar_icons::Icon;
use gpui::Context;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MainPage {
    Pins,
    History,
    #[default]
    Settings,
    /// 常用操作与快捷键说明，托盘“帮助与快捷键”打开这里。
    Help,
}
impl MainPage {
    pub(crate) const CONTENT: [Self; 2] = [Self::Pins, Self::History];

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "pins" => Some(Self::Pins),
            "history" => Some(Self::History),
            "settings" => Some(Self::Settings),
            "help" => Some(Self::Help),
            // 工具已并入图库：旧入口打开图库。
            "tools" => Some(Self::History),
            _ => None,
        }
    }
    /// `--page <名>`；缺省或名称未知时为 None，已打开的窗口保持当前页。
    pub(crate) fn from_args(args: &[String]) -> Option<Self> {
        let index = args.iter().position(|arg| arg == "--page")?;
        Self::parse(args.get(index + 1)?)
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pins => "pins",
            Self::History => "history",
            Self::Settings => "settings",
            Self::Help => "help",
        }
    }
    /// 跨进程切页请求写进窗口属性，0 留作“无请求”。
    pub(crate) fn wire(self) -> usize {
        match self {
            Self::Pins => 1,
            Self::History => 2,
            Self::Settings => 3,
            Self::Help => 4,
        }
    }
    pub(crate) fn from_wire(value: usize) -> Option<Self> {
        match value {
            1 => Some(Self::Pins),
            2 => Some(Self::History),
            3 => Some(Self::Settings),
            4 => Some(Self::Help),
            5 => Some(Self::History),
            _ => None,
        }
    }
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Pins => "main-page-pins",
            Self::History => "main-page-history",
            Self::Settings => "main-page-settings",
            Self::Help => "main-page-help",
        }
    }
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Pins => tr!("贴图", "Pins"),
            Self::History => tr!("图库", "Library"),
            Self::Settings => tr!("设置", "Settings"),
            Self::Help => tr!("帮助与快捷键", "Help & shortcuts"),
        }
    }
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Pins => tr!(
                "屏幕上的贴图、分组与最近关闭",
                "Pins on screen, groups and recently closed"
            ),
            Self::History => tr!(
                "截图、贴图与录屏按时间排列；选中后直接对照、拼接、转 PDF",
                "Captures, pins and recordings by time; select to compare, join or make a PDF"
            ),
            Self::Settings => "",
            Self::Help => tr!(
                "常用操作与快捷键；绘制工具的数字随工具栏顺序变化",
                "Common actions and shortcuts; tool numbers follow the toolbar order"
            ),
        }
    }
    pub(crate) fn icon(self) -> Icon {
        match self {
            Self::Pins => Icon::Pin,
            Self::History => Icon::History,
            Self::Settings => Icon::Settings,
            Self::Help => Icon::Keyboard,
        }
    }
}

/// 截图、录屏等会话里打开主界面：不占用会话进程，已有窗口时只切页。
pub(crate) fn launch(page: MainPage) -> crate::Result<()> {
    use std::os::windows::process::CommandExt;
    if SettingsView::activate_existing(Some(page))? {
        return Ok(());
    }
    let child =
        std::process::Command::new(std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE))
            .args(["--settings", "--page", page.name()])
            .creation_flags(0x08000000)
            .spawn()?;
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id());
    }
    Ok(())
}

impl SettingsView {
    pub(super) fn refresh_content(&mut self, cx: &mut Context<Self>) {
        match self.page {
            MainPage::Pins => self.pins_reload(cx),
            MainPage::History => self.history_refresh_if_stale(cx),
            MainPage::Settings | MainPage::Help => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_page_parses_args_and_round_trips_wire_values() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            MainPage::from_args(&args(&["exe", "--settings", "--page", "pins"])),
            Some(MainPage::Pins)
        );
        assert_eq!(MainPage::from_args(&args(&["exe", "--settings"])), None);
        assert_eq!(MainPage::from_args(&args(&["exe", "--page"])), None);
        assert_eq!(MainPage::from_args(&args(&["exe", "--page", "x"])), None);
        for page in [
            MainPage::Pins,
            MainPage::History,
            MainPage::Settings,
            MainPage::Help,
        ] {
            assert_eq!(MainPage::from_wire(page.wire()), Some(page));
            assert_eq!(MainPage::parse(page.name()), Some(page));
        }
        assert_eq!(MainPage::from_wire(0), None);
        assert_eq!(MainPage::parse("tools"), Some(MainPage::History));
        assert_eq!(MainPage::from_wire(5), Some(MainPage::History));
    }
}
