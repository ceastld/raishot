use crate::Result;
use std::sync::OnceLock;
use windows::{
    core::{w, BOOL, PCSTR, PCWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::{CreateDIBSection, DeleteObject, DIB_RGB_COLORS, HBITMAP},
        UI::{HiDpi::GetDpiForSystem, Shell::*, WindowsAndMessaging::*},
    },
};

#[path = "shortcut_help_context.rs"]
pub(crate) mod shortcut_help;

pub const NOTIFICATION: u32 = WM_APP + 1;
pub const EVENT: u32 = WM_APP + 7;
pub const RESTORE: u32 = WM_APP + 8;
const KEY_SELECT: u32 = NIN_SELECT | 1;

pub const REGION: u32 = 1;
pub const FULL_SCREEN: u32 = 2;
pub const HELP: u32 = 3;
pub const EXIT: u32 = 4;
pub const ALL_SCREENS: u32 = 5;
pub const HISTORY: u32 = 6;
pub const SETTINGS: u32 = 7;
pub const PIN_CLIPBOARD: u32 = 8;
pub const PIN_FILE: u32 = 9;
pub const PIN_RESTORE: u32 = 10;
pub const PIN_HIDE: u32 = 11;
pub const PIN_SHOW: u32 = 12;
pub const PIN_INTERACT: u32 = 13;
pub const PIN_CLOSE: u32 = 14;
pub const PIN_WORKSPACE: u32 = 16;
/// 不是菜单项：主界面经 WM_COPYDATA 请求托盘代启动贴图／编辑进程。
pub const PIN_REQUEST: u32 = 17;
pub const PIN_TOGGLE_GROUP: u32 = 18;
pub const PIN_NEXT_GROUP: u32 = 19;
pub const PIN_PREVIOUS_GROUP: u32 = 20;
pub const QUICK_ACTION_BASE: u32 = 0x100;

#[derive(Debug)]
pub enum Action {
    Capture,
    Menu(Option<POINT>),
}

pub fn taskbar_created() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

pub fn add(data: &NOTIFYICONDATAW) -> Result<bool> {
    unsafe {
        if !Shell_NotifyIconW(NIM_ADD, data).as_bool() {
            return Err(tr!("无法添加托盘图标", "Cannot add the tray icon").into());
        }
        let mut version = *data;
        version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Ok(Shell_NotifyIconW(NIM_SETVERSION, &version).as_bool())
    }
}

pub fn action(wp: WPARAM, lp: LPARAM, modern: bool) -> Option<Action> {
    let (event, id) = if modern {
        (lp.0 as u16 as u32, (lp.0 >> 16) as u16 as u32)
    } else {
        (lp.0 as u32, wp.0 as u32)
    };
    if id != 1 {
        return None;
    }
    match event {
        NIN_SELECT | KEY_SELECT if modern => Some(Action::Capture),
        WM_LBUTTONUP | WM_LBUTTONDBLCLK if !modern => Some(Action::Capture),
        WM_CONTEXTMENU if modern => {
            let x = wp.0 as u16 as i16 as i32;
            let y = (wp.0 >> 16) as u16 as i16 as i32;
            Some(Action::Menu(if x == -1 && y == -1 {
                None
            } else {
                Some(POINT { x, y })
            }))
        }
        WM_RBUTTONUP if !modern => Some(Action::Menu(None)),
        _ => None,
    }
}

/// 菜单与顶层项图标；图标位图要活到菜单销毁之后才释放。
pub struct Menu(HMENU, Vec<HBITMAP>);
impl Menu {
    pub fn handle(&self) -> HMENU {
        self.0
    }
}
impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
            for bitmap in self.1.drain(..) {
                let _ = DeleteObject(bitmap.into());
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub fn menu() -> Result<Menu> {
    unsafe {
        let mut menu = Menu(CreatePopupMenu()?, Vec::new());
        let preferences = crate::preferences::Preferences::load();
        // 菜单每次弹出都重建，语言跟随最新设置。
        crate::i18n::apply(preferences.language);
        let append = |parent: HMENU, flags, id: u32, text: &str| -> Result<()> {
            use crate::hotkey::HotkeyTarget;
            let target = match id {
                REGION => Some(HotkeyTarget::Capture),
                FULL_SCREEN => Some(HotkeyTarget::CurrentScreen),
                ALL_SCREENS => Some(HotkeyTarget::AllScreens),
                PIN_CLIPBOARD => Some(HotkeyTarget::ClipboardPin),
                PIN_FILE => Some(HotkeyTarget::PinFile),
                PIN_RESTORE => Some(HotkeyTarget::PinRestore),
                PIN_TOGGLE_GROUP => Some(HotkeyTarget::PinToggleGroup),
                PIN_NEXT_GROUP => Some(HotkeyTarget::PinNextGroup),
                PIN_PREVIOUS_GROUP => Some(HotkeyTarget::PinPreviousGroup),
                PIN_INTERACT => Some(HotkeyTarget::PinInteract),
                PIN_WORKSPACE => Some(HotkeyTarget::PinWorkspace),
                HISTORY => Some(HotkeyTarget::History),
                SETTINGS => Some(HotkeyTarget::Settings),
                _ => None,
            };
            let label = target
                .and_then(|target| preferences.trigger_for(target))
                .filter(|_| !text.contains('\t'))
                .map(|trigger| format!("{text}\t{}", trigger.label()));
            let text = label.as_deref().unwrap_or(text);
            let text = wide(text);
            AppendMenuW(parent, flags, id as usize, PCWSTR(text.as_ptr()))?;
            Ok(())
        };
        let separator = |parent: HMENU| AppendMenuW(parent, MF_SEPARATOR, 0, None);
        // 顶层按位置登记图标，分隔线占位不画。
        let mut icons: Vec<Option<&str>> = Vec::new();
        append(
            menu.0,
            MF_STRING,
            REGION,
            &trf!(
                "区域截图\t{}",
                "Capture region\t{}",
                preferences.hotkey_label()
            ),
        )?;
        icons.push(Some("region-rect"));
        let screens = CreatePopupMenu()?;
        append(
            screens,
            MF_STRING,
            FULL_SCREEN,
            tr!("当前屏幕", "Current screen"),
        )?;
        append(
            screens,
            MF_STRING,
            ALL_SCREENS,
            tr!("所有屏幕", "All screens"),
        )?;
        append(
            menu.0,
            MF_POPUP,
            screens.0 as u32,
            tr!("全屏截图", "Capture full screen"),
        )?;
        icons.push(Some("monitor"));
        let actions = crate::quick_action_store::QuickActionStore::load();
        if actions.actions.iter().any(|a| a.enabled && a.in_tray) {
            let quick = CreatePopupMenu()?;
            for (index, action) in actions
                .actions
                .iter()
                .enumerate()
                .filter(|(_, a)| a.enabled && a.in_tray)
            {
                let label = action.label();
                let label = action.hotkey.map_or_else(
                    || label.to_string(),
                    |h| format!("{}\t{}", label, h.label()),
                );
                append(quick, MF_STRING, QUICK_ACTION_BASE + index as u32, &label)?;
            }
            append(
                menu.0,
                MF_POPUP,
                quick.0 as u32,
                tr!("快捷动作", "Quick actions"),
            )?;
            icons.push(Some("sparkle"));
        }
        separator(menu.0)?;
        icons.push(None);
        append(
            menu.0,
            MF_STRING,
            PIN_CLIPBOARD,
            &trf!(
                "剪贴板贴图\t{}",
                "Pin clipboard\t{}",
                preferences.hotkey_label_for(crate::hotkey::HotkeyTarget::ClipboardPin)
            ),
        )?;
        icons.push(Some("copy"));
        let pins = CreatePopupMenu()?;
        append(
            pins,
            MF_STRING,
            PIN_FILE,
            tr!("打开图片贴图…", "Pin image file…"),
        )?;
        append(
            pins,
            MF_STRING,
            PIN_RESTORE,
            tr!("恢复最近关闭的贴图", "Restore recently closed pin"),
        )?;
        separator(pins)?;
        append(
            pins,
            MF_STRING,
            PIN_TOGGLE_GROUP,
            crate::hotkey::HotkeyTarget::PinToggleGroup.label(),
        )?;
        append(
            pins,
            MF_STRING,
            PIN_PREVIOUS_GROUP,
            crate::hotkey::HotkeyTarget::PinPreviousGroup.label(),
        )?;
        append(
            pins,
            MF_STRING,
            PIN_NEXT_GROUP,
            crate::hotkey::HotkeyTarget::PinNextGroup.label(),
        )?;
        append(
            pins,
            MF_STRING,
            PIN_INTERACT,
            tr!(
                "恢复贴图交互（关闭鼠标穿透）",
                "Restore pin interaction (turn off click-through)"
            ),
        )?;
        append(
            pins,
            MF_STRING,
            PIN_CLOSE,
            tr!("关闭全部贴图（可恢复）", "Close all pins (restorable)"),
        )?;
        separator(pins)?;
        append(
            pins,
            MF_STRING,
            PIN_WORKSPACE,
            tr!("管理贴图…", "Manage pins…"),
        )?;
        append(menu.0, MF_POPUP, pins.0 as u32, tr!("贴图", "Pins"))?;
        icons.push(Some("pin"));
        separator(menu.0)?;
        icons.push(None);
        append(menu.0, MF_STRING, HISTORY, tr!("图库", "Library"))?;
        icons.push(Some("images"));
        append(menu.0, MF_STRING, SETTINGS, tr!("设置", "Settings"))?;
        icons.push(Some("settings"));
        append(
            menu.0,
            MF_STRING,
            HELP,
            tr!("帮助与快捷键", "Help & shortcuts"),
        )?;
        icons.push(Some("keyboard"));
        separator(menu.0)?;
        icons.push(None);
        append(
            menu.0,
            MF_STRING,
            EXIT,
            &trf!("退出 {}", "Exit {}", crate::brand::NAME),
        )?;
        icons.push(Some("close"));
        SetMenuDefaultItem(menu.0, REGION, 0)?;
        let light = match preferences.theme {
            crate::theme::ThemeMode::Light => true,
            crate::theme::ThemeMode::Dark => false,
            crate::theme::ThemeMode::System => crate::quick_toast::system_is_light(),
        };
        set_icons(&mut menu, &icons, if light { "#404040" } else { "#d4d4d4" });
        Ok(menu)
    }
}

/// 图标画失败只少一个图标，不影响菜单本身。
fn set_icons(menu: &mut Menu, icons: &[Option<&str>], color: &str) {
    let size = (16 * unsafe { GetDpiForSystem() } + 48) / 96;
    for (position, name) in icons.iter().enumerate() {
        let Some(bitmap) = name.and_then(|name| icon_bitmap(name, size, color)) else {
            continue;
        };
        menu.1.push(bitmap);
        let info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            hbmpItem: bitmap,
            ..Default::default()
        };
        let _ = unsafe { SetMenuItemInfoW(menu.0, position as u32, true, &info) };
    }
}

/// 内置 SVG 按菜单文字色栅格化为 32 位预乘 BGRA 位图。
fn icon_bitmap(name: &str, size: u32, color: &str) -> Option<HBITMAP> {
    let pixels = icon_pixels(name, size, color)?;
    unsafe {
        let info = crate::gdi_surface::bitmap_info(size, size);
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(bitmap.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
        Some(bitmap)
    }
}

/// tiny-skia 输出预乘 RGBA，换成 DIB 要的预乘 BGRA。
fn icon_pixels(name: &str, size: u32, color: &str) -> Option<Vec<u8>> {
    use gpui::AssetSource;
    use resvg::{tiny_skia, usvg};
    let svg = crate::toolbar_icons::IconAssets
        .load(&format!("icons/{name}.svg"))
        .ok()??;
    let svg = std::str::from_utf8(&svg)
        .ok()?
        .replace("currentColor", color)
        .replace("black", color);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(
        pixmap
            .data()
            .chunks_exact(4)
            .flat_map(|rgba| [rgba[2], rgba[1], rgba[0], rgba[3]])
            .collect(),
    )
}

pub fn show_menu(hwnd: HWND, anchor: Option<POINT>) -> Result<u32> {
    unsafe {
        let menu = menu()?;
        let mut cursor = anchor.unwrap_or_default();
        if anchor.is_none() {
            GetCursorPos(&mut cursor)?;
        }
        apply_menu_theme(hwnd, crate::preferences::Preferences::load().theme);
        let _ = SetForegroundWindow(hwnd);
        let choice = TrackPopupMenu(
            menu.handle(),
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        )
        .0 as u32;
        // 让菜单在点击外部后能正常关闭，避免托盘菜单粘住。
        PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0))?;
        Ok(choice)
    }
}

/// 系统弹出菜单只认进程级的未公开 uxtheme 开关（序号 133/135/136，Windows 10 1903+），
/// 每次弹出前按当前主题设置刷新一次。旧系统找不到入口时保持默认浅色。
fn apply_menu_theme(hwnd: HWND, mode: crate::theme::ThemeMode) {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;
    type FlushMenuThemes = unsafe extern "system" fn();
    // PreferredAppMode：1 跟随系统，2 强制深色，3 强制浅色。
    let app_mode = match mode {
        crate::theme::ThemeMode::System => 1,
        crate::theme::ThemeMode::Dark => 2,
        crate::theme::ThemeMode::Light => 3,
    };
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        let ordinal = |n: usize| GetProcAddress(uxtheme, PCSTR(n as *const u8));
        let (Some(set_mode), Some(allow_window), Some(flush)) =
            (ordinal(135), ordinal(133), ordinal(136))
        else {
            return;
        };
        std::mem::transmute::<_, SetPreferredAppMode>(set_mode)(app_mode);
        let _ = std::mem::transmute::<_, AllowDarkModeForWindow>(allow_window)(hwnd, true.into());
        std::mem::transmute::<_, FlushMenuThemes>(flush)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_keyboard_and_legacy_notifications_route_to_actions() {
        let packed = |event| LPARAM(((1u32 << 16) | event) as isize);
        assert!(matches!(
            action(WPARAM(0), packed(NIN_SELECT), true),
            Some(Action::Capture)
        ));
        assert!(matches!(
            action(WPARAM(0), packed(KEY_SELECT), true),
            Some(Action::Capture)
        ));
        assert!(matches!(
            action(WPARAM(1), LPARAM(WM_LBUTTONUP as isize), false),
            Some(Action::Capture)
        ));
        let coords = WPARAM(((120u32 << 16) | (-50i16 as u16 as u32)) as usize);
        match action(coords, packed(WM_CONTEXTMENU), true) {
            Some(Action::Menu(Some(p))) => assert_eq!((p.x, p.y), (-50, 120)),
            _ => panic!("右键坐标或事件解码错误"),
        }
        assert!(matches!(
            action(WPARAM(u32::MAX as usize), packed(WM_CONTEXTMENU), true),
            Some(Action::Menu(None))
        ));
        assert!(action(
            WPARAM(0),
            LPARAM(((2u32 << 16) | NIN_SELECT) as isize),
            true
        )
        .is_none());
    }

    #[test]
    fn native_menu_has_capture_help_and_exit_commands() {
        let menu = menu().unwrap();
        unsafe {
            // 截图与贴图的部分命令收在子菜单里，逐层收集。
            fn collect(menu: HMENU, out: &mut Vec<u32>) {
                for index in 0..unsafe { GetMenuItemCount(Some(menu)) } {
                    let sub = unsafe { GetSubMenu(menu, index) };
                    if sub.is_invalid() {
                        out.push(unsafe { GetMenuItemID(menu, index) });
                    } else {
                        collect(sub, out);
                    }
                }
            }
            let mut commands = Vec::new();
            collect(menu.handle(), &mut commands);
            for command in [
                REGION,
                FULL_SCREEN,
                ALL_SCREENS,
                PIN_CLIPBOARD,
                PIN_FILE,
                PIN_WORKSPACE,
                PIN_RESTORE,
                HISTORY,
                SETTINGS,
                HELP,
                EXIT,
            ] {
                assert_eq!(commands.iter().filter(|id| **id == command).count(), 1);
            }
            let mut text = [0u16; 80];
            let len = GetMenuStringW(menu.handle(), FULL_SCREEN, Some(&mut text), MF_BYCOMMAND);
            assert_eq!(String::from_utf16_lossy(&text[..len as usize]), "当前屏幕");
        }
    }

    #[test]
    fn tray_menu_icons_render_visible_pixels() {
        for name in [
            "region-rect",
            "monitor",
            "sparkle",
            "copy",
            "pin",
            "images",
            "settings",
            "keyboard",
            "close",
        ] {
            let pixels = icon_pixels(name, 16, "#d4d4d4").unwrap();
            assert!(
                pixels.chunks_exact(4).any(|px| px[3] > 0),
                "{name} 图标为空"
            );
        }
    }
}
