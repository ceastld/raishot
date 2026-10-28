//! 界面主题与本机偏好分离；只影响辅助界面，不参与截图像素或导出。
use crate::{capture_toolbar::Palette, preferences::Preferences};
use gpui::{App, Global, Window, WindowAppearance, WindowId};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}
impl ThemeMode {
    pub(crate) fn from_wire(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Light => tr!("浅色", "Light"),
            Self::Dark => tr!("深色", "Dark"),
            Self::System => tr!("跟随系统", "System"),
        }
    }
    pub(crate) fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
    pub(crate) fn is_light(self, system: WindowAppearance) -> bool {
        match self {
            Self::Light => true,
            Self::Dark => false,
            Self::System => matches!(
                system,
                WindowAppearance::Light | WindowAppearance::VibrantLight
            ),
        }
    }
}

struct ThemeState {
    mode: ThemeMode,
    light: bool,
    revision: u64,
    change_identity: Arc<()>,
    window_styles: HashMap<WindowId, (bool, WindowAppearance)>,
    observed_windows: HashSet<WindowId>,
}
impl ThemeState {
    fn update_values(&mut self, mode: ThemeMode, light: bool) -> bool {
        if self.mode == mode && self.light == light {
            return false;
        }
        self.mode = mode;
        self.light = light;
        self.change_identity = Arc::new(());
        true
    }
}
impl Global for ThemeState {}

pub(crate) fn mode(cx: &App) -> ThemeMode {
    cx.try_global::<ThemeState>()
        .map_or(ThemeMode::System, |state| state.mode)
}
pub(crate) fn is_light(cx: &App) -> bool {
    cx.try_global::<ThemeState>().map_or_else(
        || ThemeMode::System.is_light(cx.window_appearance()),
        |state| state.light,
    )
}
/// 有效主题变化的身份；即使切换后又切回，旧交互意图也不能复用。
pub(crate) fn change_identity(cx: &App) -> Option<Arc<()>> {
    cx.try_global::<ThemeState>()
        .map(|state| Arc::clone(&state.change_identity))
}
pub(crate) fn palette(cx: &App) -> Palette {
    Palette::new(is_light(cx))
}
pub(crate) fn set_mode(mode: ThemeMode, cx: &mut App) {
    if cx.has_global::<ThemeState>() {
        let state = cx.global_mut::<ThemeState>();
        state.revision = state.revision.wrapping_add(1);
    }
    apply_mode(mode, cx);
}
fn apply_mode(mode: ThemeMode, cx: &mut App) {
    let light = match mode {
        ThemeMode::System => mode.is_light(cx.window_appearance()),
        ThemeMode::Light => true,
        ThemeMode::Dark => false,
    };
    if let Some(state) = cx.try_global::<ThemeState>() {
        if state.mode == mode && state.light == light {
            return;
        }
        let state = cx.global_mut::<ThemeState>();
        state.update_values(mode, light);
    } else {
        cx.set_global(ThemeState {
            mode,
            light,
            revision: 0,
            change_identity: Arc::new(()),
            window_styles: HashMap::new(),
            observed_windows: HashSet::new(),
        });
    }
    // 不同步重借正在保存设置的实体，刷新在 GPUI 当前更新结束后执行。
    cx.refresh_windows();
}

/// 仅在按需 GPUI 应用启动后安装；托盘和待命工作进程不增加轮询或 GPU 初始化。
pub(crate) fn install(cx: &mut App) {
    if cx.has_global::<ThemeState>() {
        return;
    }
    let preferences = Preferences::load();
    crate::i18n::apply(preferences.language);
    set_mode(preferences.theme, cx);
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        let Ok(revision) = cx.update(|cx| cx.global::<ThemeState>().revision) else {
            break;
        };
        // 设置由多个独立会话共享，磁盘读取放后台；渲染只访问缓存的颜色。
        let (mode, language) = cx
            .background_executor()
            .spawn(async {
                let preferences = Preferences::load();
                (preferences.theme, preferences.language)
            })
            .await;
        if cx
            .update(|cx| {
                if cx.global::<ThemeState>().revision == revision {
                    apply_mode(mode, cx);
                }
                // 语言与主题同源轮询；切换后重绘已打开窗口，不夺取焦点。
                if crate::i18n::apply(language) {
                    cx.refresh_windows();
                }
                let windows: Vec<_> = cx
                    .windows()
                    .into_iter()
                    .map(|window| window.window_id())
                    .collect();
                cx.global_mut::<ThemeState>()
                    .window_styles
                    .retain(|id, _| windows.contains(id));
                cx.global_mut::<ThemeState>()
                    .observed_windows
                    .retain(|id| windows.contains(id));
            })
            .is_err()
        {
            break;
        }
    })
    .detach();
}

/// 自有窗口标题栏跟随界面亮暗，不激活窗口、不改变输入焦点。
pub(crate) fn sync_window(window: &Window, cx: &mut App) {
    use windows::{
        core::BOOL,
        Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE},
    };
    let light = is_light(cx);
    let id = window.window_handle().window_id();
    let appearance = window.appearance();
    if cx.has_global::<ThemeState>() && !cx.global::<ThemeState>().observed_windows.contains(&id) {
        window
            .observe_window_appearance(|_, cx| {
                apply_mode(mode(cx), cx);
                // GPUI 随后会设置系统标题栏；只排队刷新，在后续渲染中恢复显式选择。
                cx.refresh_windows();
            })
            .detach();
        cx.global_mut::<ThemeState>().observed_windows.insert(id);
    }
    if let Some(state) = cx.try_global::<ThemeState>() {
        if state.window_styles.get(&id) == Some(&(light, appearance)) {
            return;
        }
    }
    let dark: BOOL = (!light).into();
    let applied = unsafe {
        DwmSetWindowAttribute(
            crate::gpui_capture::hwnd(window),
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as _,
            std::mem::size_of::<BOOL>() as u32,
        )
        .is_ok()
    };
    if applied && cx.has_global::<ThemeState>() {
        cx.global_mut::<ThemeState>()
            .window_styles
            .insert(id, (light, appearance));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_theme_identity_keeps_noop_and_rejects_mode_or_light_aba() {
        let mut state = ThemeState {
            mode: ThemeMode::Light,
            light: true,
            revision: 17,
            change_identity: Arc::new(()),
            window_styles: HashMap::new(),
            observed_windows: HashSet::new(),
        };
        let initial = Arc::clone(&state.change_identity);
        assert!(!state.update_values(ThemeMode::Light, true));
        assert!(Arc::ptr_eq(&initial, &state.change_identity));
        assert!(state.update_values(ThemeMode::Dark, false));
        assert!(state.update_values(ThemeMode::Light, true));
        assert!(!Arc::ptr_eq(&initial, &state.change_identity));
        let light = Arc::clone(&state.change_identity);
        assert!(state.update_values(ThemeMode::System, true));
        assert!(!Arc::ptr_eq(&light, &state.change_identity));
        let system = Arc::clone(&state.change_identity);
        assert!(state.update_values(ThemeMode::System, false));
        assert!(state.update_values(ThemeMode::System, true));
        assert!(!Arc::ptr_eq(&system, &state.change_identity));
        assert_eq!(state.revision, 17); // 偏好轮询的旧回执代次保持其原用途。
    }
    #[test]
    fn theme_modes_resolve_all_system_appearances_and_keep_explicit_choice() {
        for (appearance, light) in [
            (WindowAppearance::Light, true),
            (WindowAppearance::VibrantLight, true),
            (WindowAppearance::Dark, false),
            (WindowAppearance::VibrantDark, false),
        ] {
            assert_eq!(ThemeMode::System.is_light(appearance), light);
            assert!(ThemeMode::Light.is_light(appearance));
            assert!(!ThemeMode::Dark.is_light(appearance));
        }
    }
    #[test]
    fn theme_cycle_returns_to_system_without_losing_wire_choice() {
        let mut mode = ThemeMode::default();
        for expected in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(mode, expected);
            assert_eq!(ThemeMode::from_wire(mode.wire_name()), mode);
            mode = mode.next();
        }
        assert_eq!(mode, ThemeMode::System);
    }
}
