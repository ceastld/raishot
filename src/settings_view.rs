//! 独立 GPUI 设置页：常用偏好直接操作，热键变更通知原生托盘即时重新注册。
use crate::{
    preferences::Preferences,
    settings_hotkey_native::NativeHotkeyRecorder,
    settings_widgets::{self as widgets, Surface},
    toolbar_config::ToolbarConfig,
    toolbar_icons::Icon,
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, size, AnyElement, App, AppContext, Bounds, Context, FocusHandle,
    FontWeight, MouseButton, Render, Subscription, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

#[path = "settings_instance.rs"]
mod instance;
#[path = "main_help.rs"]
mod main_help;
#[path = "main_history.rs"]
mod main_history;
#[path = "main_media_popup.rs"]
mod main_media_popup;
#[path = "main_page.rs"]
pub(crate) mod main_page;
#[path = "main_pins.rs"]
mod main_pins;
#[path = "main_tool_popup.rs"]
mod main_tool_popup;
#[path = "settings_status.rs"]
mod status;
#[path = "settings_titlebar.rs"]
mod titlebar;
use main_page::MainPage;
#[path = "settings_translation.rs"]
mod translation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Capture,
    Pin,
    Translation,
    Hotkeys,
    Save,
    Toolbar,
}
impl SettingsTab {
    const ALL: [Self; 7] = [
        Self::General,
        Self::Capture,
        Self::Pin,
        Self::Translation,
        Self::Hotkeys,
        Self::Save,
        Self::Toolbar,
    ];
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::General => "settings-tab-general",
            Self::Capture => "settings-tab-capture",
            Self::Pin => "settings-tab-pin",
            Self::Translation => "settings-tab-translation",
            Self::Hotkeys => "settings-tab-hotkeys",
            Self::Save => "settings-tab-save",
            Self::Toolbar => "settings-tab-toolbar",
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::General => tr!("常规", "General"),
            Self::Capture => tr!("截图", "Capture"),
            Self::Pin => tr!("贴图", "Pin"),
            Self::Translation => tr!("翻译", "Translation"),
            Self::Hotkeys => tr!("快捷键", "Hotkeys"),
            Self::Save => tr!("保存", "Save"),
            Self::Toolbar => tr!("工具栏", "Toolbar"),
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::General => tr!(
                "界面外观、开机启动与本机历史",
                "Appearance, startup and local history"
            ),
            Self::Capture => tr!(
                "框选、控件吸附与截图内容",
                "Selection, control snapping and capture content"
            ),
            Self::Pin => tr!("贴图恢复与滚轮缩放", "Pin restore and wheel zoom"),
            Self::Translation => tr!(
                "识别出的文字译成哪种语言",
                "Which language recognized text is translated into"
            ),
            Self::Hotkeys => tr!(
                "配置截图、贴图分组与应用操作的全局快捷键",
                "Global shortcuts for captures, pin groups and app commands"
            ),
            Self::Save => tr!(
                "默认目录、文件命名与图片格式",
                "Default folder, file naming and image format"
            ),
            Self::Toolbar => tr!(
                "截图与贴图共用的工具及排列顺序",
                "Tools and their order, shared by captures and pins"
            ),
        }
    }
    fn icon(self) -> Icon {
        match self {
            Self::General => Icon::Settings,
            Self::Capture => Icon::Capture,
            Self::Pin => Icon::Pin,
            Self::Translation => Icon::Text,
            Self::Hotkeys => Icon::Keyboard,
            Self::Save => Icon::Save,
            Self::Toolbar => Icon::Toolbar,
        }
    }
    pub(crate) fn for_control(id: &str) -> Option<Self> {
        if id.starts_with("settings-theme-")
            || id.starts_with("settings-language-")
            || id.starts_with("settings-autostart")
            || id == "settings-history"
        {
            Some(Self::General)
        } else if id.starts_with("settings-pin-") {
            Some(Self::Pin)
        } else if id.starts_with("settings-translation-") {
            Some(Self::Translation)
        } else if id.starts_with("hotkey") || id.starts_with("pin-hotkey-") {
            Some(Self::Hotkeys)
        } else if id.starts_with("settings-save-") {
            Some(Self::Save)
        } else if id.starts_with("toolbar-") {
            Some(Self::Toolbar)
        } else if id.starts_with("settings-ocr-profile-")
            || matches!(
                id,
                "settings-magnifier"
                    | "settings-guides"
                    | "settings-auto-adsorb"
                    | "settings-text-copy-only"
                    | "settings-cursor"
            )
        {
            Some(Self::Capture)
        } else {
            None
        }
    }
}
pub struct SettingsView {
    pub(crate) quick_actions: crate::settings_quick_actions::QuickActionSettings,
    pub(crate) tab: SettingsTab,
    pub(crate) preferences: Preferences,
    pub(crate) toolbar_config: ToolbarConfig,
    pub(crate) autostart: std::result::Result<crate::autostart::AutostartState, String>,
    pub(crate) status: String,
    pub(crate) hotkey_focus: FocusHandle,
    pub(crate) recording_hotkey: bool,
    pub(crate) recording_hotkey_target: crate::hotkey::HotkeyTarget,
    pub(crate) hotkey_request: Option<Preferences>,
    pub(crate) hotkey_request_started: u64,
    /// 某个全局快捷键行最近一次校验、冲突或注册失败的说明，就近显示在该行。
    pub(crate) hotkey_error: Option<(crate::hotkey::HotkeyTarget, String)>,
    pub(crate) recording_native: Option<usize>,
    pub(crate) hotkey_native: Option<NativeHotkeyRecorder>,
    /// 全局键盘／鼠标组合录制；首次录制时创建，失败时退回窗口内按键录制。
    pub(crate) trigger_recorder: Option<crate::settings_trigger::TriggerRecorder>,
    /// 录制中当前按住的键，用于实时显示。
    pub(crate) recording_held: Vec<crate::trigger::Button>,
    pub(crate) recording_generation: u64,
    _hotkey_subscriptions: Vec<Subscription>,
    pub(crate) ui_positions: Rc<RefCell<HashMap<String, (f32, f32)>>>,
    pub(crate) save_settings: gpui::Entity<crate::settings_save::SaveSettings>,
    translation_fields: translation::TranslationFields,
    /// 主界面当前页；设置分类仍由 `tab` 记住，切回设置时还原。
    pub(crate) page: MainPage,
    pins: main_pins::PinsPage,
    history: main_history::HistoryPage,
    media_popup: main_media_popup::MediaPopup,
    tool_popup: main_tool_popup::ToolPopup,
    _instance: Option<instance::WindowRegistration>,
    /// 保存结果的浮动提示状态。
    pub(crate) toast: status::StatusToast,
}
impl SettingsView {
    pub(crate) fn activate_existing(page: Option<MainPage>) -> Result<bool> {
        instance::activate_existing(page)
    }
    pub fn open(cx: &mut App) -> Result<()> {
        Self::open_page(cx, None)
    }
    /// `page` 为 None 时新窗口进设置、已有窗口保持当前页。
    pub(crate) fn open_page(cx: &mut App, page: Option<MainPage>) -> Result<()> {
        Self::open_unique(cx, page)?;
        Ok(())
    }
    pub(crate) fn open_window(cx: &mut App) -> Result<WindowHandle<Self>> {
        Self::open_unique(cx, None)?.ok_or_else(|| {
            tr!(
                "设置已在另一个进程中打开。",
                "Settings are already open in another process."
            )
            .into()
        })
    }
    fn open_unique(cx: &mut App, page: Option<MainPage>) -> Result<Option<WindowHandle<Self>>> {
        // 枚举与新建共用短期跨进程锁，连点或两个入口同时打开也只建一窗。
        let _creation = instance::CreationLock::acquire()?;
        for window in cx.windows() {
            if let Some(settings) = window.downcast::<Self>() {
                settings.update(cx, |view, window, cx| {
                    if let Some(page) = page {
                        view.show_page(page, cx);
                    }
                    instance::activate(crate::gpui_capture::hwnd(window));
                })?;
                return Ok(Some(settings));
            }
        }
        if instance::activate_existing(page)? {
            return Ok(None);
        }
        let dimensions = cx
            .primary_display()
            .map_or(size(px(1120.), px(740.)), |display| {
                let bounds = display.bounds();
                size(
                    px(1120_f32.min((f32::from(bounds.size.width) - 48.).max(200.))),
                    px(740_f32.min((f32::from(bounds.size.height) - 80.).max(220.))),
                )
            });
        let initial_bounds = Bounds::centered(None, dimensions, cx);
        let settings = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(initial_bounds)),
                titlebar: Some(TitlebarOptions {
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_min_size: Some(size(
                    px(600_f32.min(f32::from(dimensions.width))),
                    px(400_f32.min(f32::from(dimensions.height))),
                )),
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(crate::brand::NAME);
                instance::activate(crate::gpui_capture::hwnd(window));
                cx.new(|cx| {
                    let native = NativeHotkeyRecorder::install(crate::gpui_capture::hwnd(window));
                    let (hotkey_native, native_status) = match native {
                        Ok(recorder) => (Some(recorder), String::new()),
                        Err(error) => (
                            None,
                            trf!(
                                "原生热键录制不可用：{error}",
                                "Native hotkey recording unavailable: {error}"
                            ),
                        ),
                    };
                    let hotkey_focus = cx.focus_handle();
                    let blur = cx.on_blur(&hotkey_focus, window, |v: &mut Self, _, cx| {
                        if v.recording_hotkey {
                            v.stop_recording_hotkey();
                            v.status = tr!(
                                "已取消录制，保留原有快捷键。",
                                "Recording canceled; the previous hotkey is kept."
                            )
                            .into();
                            cx.notify();
                        }
                    });
                    let activation =
                        cx.observe_window_activation(window, |v: &mut Self, window, cx| {
                            if window.is_window_active() {
                                // 托盘等入口经窗口属性请求切页，激活时取走。
                                if let Some(page) =
                                    instance::take_requested_page(crate::gpui_capture::hwnd(window))
                                {
                                    if v.page == MainPage::History && page != MainPage::History {
                                        v.history.hide_recovery();
                                        v.history_clear_video_preview(cx);
                                    }
                                    v.show_page(page, cx);
                                }
                                v.refresh_content(cx);
                                let latest = Preferences::load();
                                v.preferences.pin_zoom_anchor = latest.pin_zoom_anchor;
                                v.preferences.theme = latest.theme;
                                v.preferences.language = latest.language;
                                v.preferences.ocr_profile = latest.ocr_profile;
                                crate::theme::set_mode(latest.theme, cx);
                                crate::i18n::apply(latest.language);
                                v.refresh_autostart();
                                cx.notify();
                            }
                            if !window.is_window_active() && v.recording_hotkey {
                                v.stop_recording_hotkey();
                                v.status = tr!(
                                    "已取消录制，保留原有快捷键。",
                                    "Recording canceled; the previous hotkey is kept."
                                )
                                .into();
                                cx.notify();
                            }
                        });
                    Self::watch_content(window, cx);
                    let ui_positions = Rc::new(RefCell::new(HashMap::new()));
                    let save_settings = cx.new(|cx| {
                        crate::settings_save::SaveSettings::new(window, ui_positions.clone(), cx)
                    });
                    let preferences = Preferences::load();
                    let translation_fields =
                        translation::TranslationFields::new(&preferences, window, cx);
                    Self {
                        quick_actions: crate::settings_quick_actions::QuickActionSettings::new(
                            window, cx,
                        ),
                        tab: SettingsTab::default(),
                        preferences,
                        toolbar_config: ToolbarConfig::load(),
                        autostart: crate::autostart::state().map_err(|error| error.to_string()),
                        status: native_status,
                        hotkey_focus,
                        recording_hotkey: false,
                        recording_hotkey_target: crate::hotkey::HotkeyTarget::Capture,
                        hotkey_request: None,
                        hotkey_request_started: 0,
                        hotkey_error: None,
                        recording_native: None,
                        hotkey_native,
                        trigger_recorder: None,
                        recording_held: Vec::new(),
                        recording_generation: 0,
                        _hotkey_subscriptions: vec![blur, activation],
                        ui_positions,
                        save_settings,
                        translation_fields,
                        page: page.unwrap_or_default(),
                        pins: main_pins::PinsPage::new(window, cx),
                        history: main_history::HistoryPage::new(window, cx),
                        media_popup: main_media_popup::MediaPopup::default(),
                        tool_popup: main_tool_popup::ToolPopup::default(),
                        _instance: None,
                        toast: Default::default(),
                    }
                })
            },
        )?;
        let registration = settings.update(cx, |view, window, _| -> Result<()> {
            view._instance = Some(instance::WindowRegistration::new(
                crate::gpui_capture::hwnd(window),
            )?);
            Ok(())
        })?;
        if let Err(error) = registration {
            let _ = settings.update(cx, |_, window, _| window.remove_window());
            return Err(error);
        }
        Ok(Some(settings))
    }
    fn watch_content(window: &mut Window, cx: &mut Context<Self>) {
        let hwnd = crate::gpui_capture::hwnd(window);
        // 其它进程的切页请求不一定能激活本窗口（前台授权），打开期间轻量轮询。
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let alive = this.update_in(cx, |view: &mut Self, window, cx| {
                if let Some(page) = instance::take_requested_page(hwnd) {
                    view.show_page(page, cx);
                }
                if view.page == MainPage::Pins {
                    view.pins_tick(cx);
                }
                let visible = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool()
                        && !windows::Win32::UI::WindowsAndMessaging::IsIconic(hwnd).as_bool()
                };
                if !visible && view.media_popup.is_open() {
                    view.close_media_popup(window, cx);
                }
                view.history_preview_tick(visible, cx);
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
    }
    pub(crate) fn save(&mut self) -> bool {
        // 设置窗可长期打开；独立入口更新的字段不能被旧快照覆盖。
        let latest = Preferences::load();
        // 非快捷键设置保存时，保留其它入口已录制的新绑定。
        for target in crate::hotkey::HotkeyTarget::ALL {
            self.preferences.restore_hotkey_from(target, &latest);
        }
        self.preferences.unknown_command_hotkeys = latest.unknown_command_hotkeys.clone();
        self.preferences.pin_zoom_anchor = latest.pin_zoom_anchor;
        self.preferences.theme = latest.theme;
        self.preferences.language = latest.language;
        self.preferences.ocr_profile = latest.ocr_profile;
        // 译文页原文开关在截图面板里切换，设置窗旧快照不能覆盖。
        self.preferences.translation_show_source = latest.translation_show_source;
        translation::preserve_latest(&mut self.preferences, &latest);
        self.save_preferences()
    }
    fn save_ocr_profile(&mut self, profile: crate::ppocr::OcrProfile) -> bool {
        let latest = Preferences::load();
        self.preferences = latest.with_ocr_profile_change(profile);
        if self.save_preferences() {
            let model = match profile {
                crate::ppocr::OcrProfile::Tiny => "Tiny",
                crate::ppocr::OcrProfile::Small => "Small",
            };
            self.status = trf!(
                "OCR 模型已切换为 {model}，下一次识别时生效。",
                "OCR model set to {model} for the next recognition."
            );
            true
        } else {
            self.preferences = latest;
            false
        }
    }
    fn save_ocr_warmup(&mut self, mode: crate::ppocr::WarmupMode) -> bool {
        // 只改预热一项；下一次截图待命／贴图宿主启动时生效。
        let latest = Preferences::load();
        self.preferences = latest.clone();
        self.preferences.ocr_warmup = mode;
        if self.save_preferences() {
            true
        } else {
            self.preferences = latest;
            false
        }
    }
    fn save_language(&mut self, mode: crate::i18n::LanguageMode, cx: &mut Context<Self>) -> bool {
        // 与主题相同，只改语言一项；切换后立即重绘本进程所有窗口。
        let latest = Preferences::load();
        self.preferences = latest.clone();
        self.preferences.language = mode;
        if self.save_preferences() {
            crate::i18n::apply(mode);
            cx.refresh_windows();
            self.status = tr!("语言已保存并立即应用。", "Language saved and applied.").into();
            true
        } else {
            self.preferences = latest;
            false
        }
    }
    fn save_theme(&mut self, mode: crate::theme::ThemeMode, cx: &mut Context<Self>) -> bool {
        // 显式主题单选只改这一项，不能回写这个窗口长期持有的其它字段。
        let latest = Preferences::load();
        self.preferences = latest.clone();
        self.preferences.theme = mode;
        if self.save_preferences() {
            crate::theme::set_mode(mode, cx);
            self.status = tr!("主题已保存并立即应用。", "Theme saved and applied.").into();
            true
        } else {
            self.preferences = latest;
            false
        }
    }
    pub(crate) fn save_pin_anchor(&mut self, next: crate::pin_zoom::ZoomAnchor) -> bool {
        // 单选按钮只改这一项；不能把长期打开设置窗的其它旧字段写回磁盘。
        let latest = Preferences::load();
        self.preferences = latest.clone();
        self.preferences.pin_zoom_anchor = next;
        let saved = self.save_preferences();
        if !saved {
            self.preferences = latest;
        }
        saved
    }
    pub(crate) fn save_pin_title_position(
        &mut self,
        next: crate::pin_title::TitlePosition,
    ) -> bool {
        let latest = Preferences::load();
        self.preferences = latest.clone();
        self.preferences.pin_title_position = next;
        let saved = self.save_preferences();
        if !saved {
            self.preferences = latest;
        }
        saved
    }
    fn save_preferences(&mut self) -> bool {
        let mut saved = false;
        self.status = match self.preferences.save() {
            Ok(()) => {
                saved = true;
                unsafe {
                    use windows::{
                        core::w,
                        Win32::{Foundation::*, UI::WindowsAndMessaging::*},
                    };
                    if std::env::var_os("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").is_none() {
                        if let Ok(h) = FindWindowW(w!("RustShot.MVP.Window"), None) {
                            let _ = PostMessageW(
                                Some(h),
                                crate::ui::RELOAD_MESSAGE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                }
                tr!(
                    "已保存。新的截图会话使用这些设置。",
                    "Saved. New capture sessions use these settings."
                )
                .into()
            }
            Err(e) => e.to_string(),
        };
        saved
    }
    pub(crate) fn settings_probe(&self, id: impl Into<String>) -> impl IntoElement {
        let id = id.into();
        let positions = self.ui_positions.clone();
        gpui::canvas(
            |_, _, _| (),
            move |bounds, _, _, _| {
                positions.borrow_mut().insert(
                    id.clone(),
                    (
                        f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.,
                        f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.,
                    ),
                );
            },
        )
        .absolute()
        .left(px(0.))
        .top(px(0.))
        .size_full()
    }
    fn theme_row(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::theme::ThemeMode;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        for (mode, id, label) in [
            (
                ThemeMode::System,
                "settings-theme-system",
                tr!("跟随系统", "System"),
            ),
            (
                ThemeMode::Light,
                "settings-theme-light",
                tr!("浅色", "Light"),
            ),
            (ThemeMode::Dark, "settings-theme-dark", tr!("深色", "Dark")),
        ] {
            choices = choices.child(
                widgets::segment(id, label, crate::theme::mode(cx) == mode, palette, surface)
                    .child(self.settings_probe(id))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.save_theme(mode, cx);
                        cx.notify();
                    })),
            );
        }
        widgets::row(
            tr!("主题", "Theme"),
            Some(
                tr!(
                    "默认跟随 Windows 系统外观，也可固定为浅色或深色。",
                    "Follows the Windows appearance by default, or stays light or dark."
                )
                .into(),
            ),
            palette,
        )
        .child(choices)
        .into_any_element()
    }
    fn language_row(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::i18n::LanguageMode;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        // 语言名称保持各自原文，误切换后仍能认出要选的项。
        for (mode, id, label) in [
            (
                LanguageMode::System,
                "settings-language-system",
                tr!("跟随系统", "System"),
            ),
            (LanguageMode::Chinese, "settings-language-zh", "中文"),
            (LanguageMode::English, "settings-language-en", "English"),
        ] {
            choices = choices.child(
                widgets::segment(
                    id,
                    label,
                    self.preferences.language == mode,
                    palette,
                    surface,
                )
                .child(self.settings_probe(id))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.save_language(mode, cx);
                    cx.notify();
                })),
            );
        }
        widgets::row(
            tr!("语言", "Language"),
            Some(
                tr!(
                    "默认跟随 Windows 显示语言；已打开的窗口立即切换。",
                    "Follows the Windows display language by default; open windows switch immediately."
                )
                .into(),
            ),
            palette,
        )
        .child(choices)
        .into_any_element()
    }
    fn ocr_profile_row(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::ppocr::OcrProfile;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        for (profile, id, label) in [
            (
                OcrProfile::Tiny,
                "settings-ocr-profile-tiny",
                tr!("Tiny · 轻量", "Tiny"),
            ),
            (
                OcrProfile::Small,
                "settings-ocr-profile-small",
                tr!("Small · 标准", "Small"),
            ),
        ] {
            choices = choices.child(
                widgets::segment(
                    id,
                    label,
                    self.preferences.ocr_profile == profile,
                    palette,
                    surface,
                )
                .child(self.settings_probe(id))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.save_ocr_profile(profile);
                    cx.notify();
                })),
            );
        }
        widgets::row(
            tr!("模型", "Model"),
            Some(
                tr!(
                    "Tiny 更轻量；识别不理想时可尝试 Small。",
                    "Tiny is lighter; try Small if recognition is poor."
                )
                .into(),
            ),
            palette,
        )
        .child(choices)
        .into_any_element()
    }
    fn ocr_warmup_row(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::ppocr::WarmupMode;
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut choices = widgets::segmented(surface);
        for (mode, id, label) in [
            (
                WarmupMode::Off,
                "settings-ocr-warmup-off",
                tr!("关闭", "Off"),
            ),
            (
                WarmupMode::Standby,
                "settings-ocr-warmup-standby",
                tr!("截图时", "Capture"),
            ),
            (
                WarmupMode::Always,
                "settings-ocr-warmup-always",
                tr!("始终", "Always"),
            ),
        ] {
            choices = choices.child(
                widgets::segment(
                    id,
                    label,
                    self.preferences.ocr_warmup == mode,
                    palette,
                    surface,
                )
                .child(self.settings_probe(id))
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.save_ocr_warmup(mode);
                    cx.notify();
                })),
            );
        }
        widgets::row(
            tr!("预热", "Preload"),
            Some(
                tr!(
                    "截图时：待命时提前加载模型；始终：贴图窗口也提前加载，多占内存；关闭：首次识别时再加载，稍慢一些。",
                    "Capture: load the model while waiting to capture. Always: pins load it too, using more memory. Off: load on first use, slightly slower."
                )
                .into(),
            ),
            palette,
        )
        .child(choices)
        .into_any_element()
    }
    pub(crate) fn show_page(&mut self, page: MainPage, cx: &mut Context<Self>) {
        if self.page != page {
            self.media_popup.dismiss_requested = true;
            if self.tool_popup.is_open() {
                self.tool_popup.dismiss_requested = true;
            }
            if self.page == MainPage::History {
                self.history.hide_recovery();
                self.history_clear_video_preview(cx);
            }
            self.page = page;
            self.refresh_content(cx);
            cx.notify();
        }
    }
    fn switch_tab(&mut self, tab: SettingsTab, window: &mut Window, cx: &mut Context<Self>) {
        // 上一页的保存结果不带到新页面；下面取消录制等提示会重新写入。
        if self.page != MainPage::Settings || self.tab != tab {
            self.status.clear();
        }
        if self.page != MainPage::Settings {
            self.show_page(MainPage::Settings, cx);
        }
        if self.tab == tab {
            return;
        }
        if self.save_settings.read(cx).is_composing(cx) || self.translation_fields.is_composing(cx)
        {
            self.status = tr!(
                "请先确认或取消输入法候选，再切换设置分类。",
                "Confirm or cancel the IME candidate before switching tabs."
            )
            .into();
            cx.notify();
            return;
        }
        if self.recording_hotkey {
            self.stop_recording_hotkey();
            self.status = tr!(
                "已取消录制，保留原有快捷键。",
                "Recording canceled; the previous hotkey is kept."
            )
            .into();
        }
        // 隐藏输入区域前释放其焦点，保存规则的 Entity 和未应用草稿继续保留。
        if matches!(
            self.tab,
            SettingsTab::Save | SettingsTab::Hotkeys | SettingsTab::Translation
        ) {
            window.blur();
        }
        self.tab = tab;
        cx.notify();
    }
    fn sidebar(&self, width: f32, band: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut tabs = div()
            .id("settings-tabs-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px_2()
            .pt_1()
            .pb_3();
        for page in MainPage::CONTENT {
            let selected = self.page == page;
            tabs = tabs.child(
                nav_item(
                    page.id(),
                    page.icon(),
                    page.title(),
                    selected,
                    palette,
                    surface,
                )
                .child(self.settings_probe(page.id()))
                .on_click(cx.listener(move |view, _, _, cx| {
                    if view.page != page {
                        view.status.clear();
                    }
                    view.show_page(page, cx)
                })),
            );
        }
        tabs = tabs.child(
            div()
                .flex_shrink_0()
                .mt_3()
                .mb_1()
                .px_3()
                .text_size(px(12.))
                .text_color(rgb(palette.muted))
                .child(MainPage::Settings.title()),
        );
        for tab in SettingsTab::ALL {
            let selected = self.page == MainPage::Settings && self.tab == tab;
            tabs = tabs.child(
                nav_item(
                    tab.id(),
                    tab.icon(),
                    tab.title(),
                    selected,
                    palette,
                    surface,
                )
                .child(self.settings_probe(tab.id()))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.switch_tab(tab, window, cx);
                })),
            );
        }
        div()
            .w(px(width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(surface.page))
            .child(titlebar::brand(band, cx))
            .child(tabs)
            .child(
                div().flex_shrink_0().px_2().child(
                    nav_item(
                        MainPage::Help.id(),
                        MainPage::Help.icon(),
                        tr!("帮助", "Help"),
                        self.page == MainPage::Help,
                        palette,
                        surface,
                    )
                    .child(self.settings_probe(MainPage::Help.id()))
                    .on_click(cx.listener(|view, _, _, cx| {
                        if view.page != MainPage::Help {
                            view.status.clear();
                        }
                        view.show_page(MainPage::Help, cx)
                    })),
                ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_5()
                    .py_3()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(trf!("版本 {}", "Version {}", env!("CARGO_PKG_VERSION"))),
            )
    }
    pub(crate) fn toggle_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: &'static str,
        value: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        widgets::row(title, Some(description.into()), palette)
            .id(id)
            .cursor_pointer()
            .hover(move |style| style.bg(rgb(palette.hover)))
            .child(widgets::switch(value, palette, surface))
            .child(self.settings_probe(id))
            .on_click(cx.listener(move |view, _, _, cx| {
                // 只在最新配置上切换这一项，避免长期打开的窗口覆盖其它入口的设置。
                let previous = Preferences::load();
                view.preferences = previous.clone();
                let field = match id {
                    "settings-pin-restore" => &mut view.preferences.pin_restore,
                    "settings-magnifier" => &mut view.preferences.magnifier,
                    "settings-guides" => &mut view.preferences.guides,
                    "settings-auto-adsorb" => &mut view.preferences.auto_adsorb,
                    "settings-text-copy-only" => &mut view.preferences.text_capture_copy_only,
                    "settings-cursor" => &mut view.preferences.include_cursor,
                    _ => &mut view.preferences.history,
                };
                *field = !*field;
                if !view.save() {
                    view.preferences = previous;
                }
                cx.notify();
            }))
            .into_any_element()
    }
    fn content_probe(&self) -> impl IntoElement {
        let positions = self.ui_positions.clone();
        gpui::canvas(
            |_, _, _| (),
            move |bounds, _, _, _| {
                let (x, y) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
                let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                let mut positions = positions.borrow_mut();
                positions.insert("settings-content".into(), (x + width / 2., y + height / 2.));
                positions.insert("settings-content-top-left".into(), (x, y));
                positions.insert(
                    "settings-content-bottom-right".into(),
                    (x + width, y + height),
                );
            },
        )
        .absolute()
        .left(px(0.))
        .top(px(0.))
        .size_full()
    }
}
impl SettingsView {
    fn tab_content(&self, window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let column = div()
            .w_full()
            .max_w(px(760.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_6();
        match self.tab {
            SettingsTab::General => {
                let theme = self.theme_row(cx);
                let language = self.language_row(cx);
                let history = self.toggle_row(
                    "settings-history",
                    tr!("保留本机截图历史", "Keep local capture history"),
                    tr!("默认关闭；开启后只在本机保留完成的图片，最多 50 张、256 MiB、30 天。", "Off by default; when on, finished images are kept on this PC only, up to 50 images, 256 MiB and 30 days."),
                    self.preferences.history,
                    cx,
                );
                column
                    .child(
                        widgets::section(tr!("外观", "Appearance"), palette)
                            .child(widgets::card(surface, [theme, language])),
                    )
                    .child(self.autostart_settings(cx))
                    .child(
                        widgets::section(tr!("历史", "History"), palette)
                            .child(widgets::card(surface, [history])),
                    )
            }
            SettingsTab::Capture => {
                let preferences = &self.preferences;
                let selection = [
                    (
                        "settings-magnifier",
                        tr!("框选放大镜", "Selection magnifier"),
                        tr!("框选时在光标旁显示放大像素与颜色值。", "Shows magnified pixels and the color value next to the cursor while selecting."),
                        preferences.magnifier,
                    ),
                    (
                        "settings-guides",
                        tr!("始终显示辅助线", "Always show guides"),
                        tr!("框选时显示跟随光标的十字辅助线；关闭时可按 Alt 临时显示。", "Shows crosshair guides that follow the cursor while selecting; when off, hold Alt to show them."),
                        preferences.guides,
                    ),
                    (
                        "settings-auto-adsorb",
                        tr!("自动吸附界面控件", "Snap to UI controls"),
                        tr!("悬停时识别窗口与控件边界，单击即可选中。", "Detects window and control bounds on hover; click to select."),
                        preferences.auto_adsorb,
                    ),
                ]
                .map(|(id, title, description, value)| {
                    self.toggle_row(id, title, description, value, cx)
                });
                let content = [
                    (
                        "settings-cursor",
                        tr!("包含鼠标指针", "Include mouse pointer"),
                        tr!("截图保留开始截图时的鼠标指针。", "Captures keep the mouse pointer as it was when the capture started."),
                        preferences.include_cursor,
                    ),
                    (
                        "settings-text-copy-only",
                        tr!("识别文字后直接复制", "Copy recognized text directly"),
                        tr!("按 R 或 Shift+C 捕获文字后直接复制，不打开结果面板。", "Copies text captured with R or Shift+C without opening the result panel."),
                        preferences.text_capture_copy_only,
                    ),
                ]
                .map(|(id, title, description, value)| {
                    self.toggle_row(id, title, description, value, cx)
                });
                // 最常改的“完成动作”放最前；低频的远程桌面兼容放最后。
                column
                    .child(self.quick_finish_settings(cx))
                    .child(
                        widgets::section(tr!("框选", "Selection"), palette)
                            .child(widgets::card(surface, selection)),
                    )
                    .child(
                        widgets::section(tr!("截图内容", "Capture content"), palette)
                            .child(widgets::card(surface, content)),
                    )
                    .child(
                        widgets::section(tr!("文字识别", "Text recognition"), palette)
                            .child(widgets::card(
                                surface,
                                [self.ocr_profile_row(cx), self.ocr_warmup_row(cx)],
                            ))
                            .child(widgets::note(
                                tr!(
                                    "首次使用时下载所选模型及运行依赖；OCR 与二维码识别在本机完成。",
                                    "The selected model and runtime download on first use. OCR and QR recognition run locally."
                                ),
                                palette,
                            )),
                    )
                    .child(
                        widgets::section(tr!("远程桌面", "Remote desktop"), palette)
                            .child(widgets::card(surface, [self.remote_display_settings(cx)])),
                    )
            }
            SettingsTab::Pin => {
                let restore = self.toggle_row(
                    "settings-pin-restore",
                    tr!("保留最近关闭的贴图", "Keep recently closed pins"),
                    tr!("保留最近 20 张、7 天内关闭的贴图，可从托盘菜单或主界面贴图页恢复；贴图记录共用 256 MiB 空间。", "Keeps up to 20 pins closed in the last 7 days, restorable from the tray menu or the Pins page; pin records share 256 MiB."),
                    self.preferences.pin_restore,
                    cx,
                );
                let anchor = self.pin_settings(cx);
                let title = self.pin_title_settings(cx);
                column
                    .child(
                        widgets::section(tr!("关闭与恢复", "Close and restore"), palette)
                            .child(widgets::card(surface, [restore])),
                    )
                    .child(
                        widgets::section(tr!("滚轮缩放", "Wheel zoom"), palette)
                            .child(widgets::card(surface, [anchor])),
                    )
                    .child(
                        widgets::section(tr!("贴图标题", "Pin title"), palette)
                            .child(widgets::card(surface, [title])),
                    )
            }
            SettingsTab::Translation => column.child(self.translation_settings(cx)),
            SettingsTab::Hotkeys => column.child(self.hotkey_settings(cx)),
            SettingsTab::Save => column.child(self.save_settings.clone()),
            SettingsTab::Toolbar => {
                // 正文列：视口减侧栏与左右 px_8，最宽 760。
                let width = (f32::from(window.viewport_size().width) - sidebar_width(window) - 64.)
                    .min(760.);
                column.child(self.toolbar_settings(width, window, cx))
            }
        }
    }
}
/// 侧栏按当前语言实测最长页签与品牌名取宽，中文更窄、英文不截断；窄窗口再让给正文。
/// 侧栏导航项：选中时左侧强调条、底色加深、图标用强调色。
fn nav_item(
    id: &'static str,
    icon: Icon,
    title: &'static str,
    selected: bool,
    palette: crate::capture_toolbar::Palette,
    surface: Surface,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .flex_shrink_0()
        .h(px(38.))
        .pl_3()
        .pr_2()
        .flex()
        .items_center()
        .gap_3()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(14.))
        .text_color(rgb(palette.foreground))
        .when(selected, |item| {
            item.bg(rgb(surface.nav_selected))
                .font_weight(FontWeight::SEMIBOLD)
        })
        .when(!selected, |item| {
            item.hover(move |style| style.bg(rgb(surface.nav_hover)))
        })
        .child(
            div()
                .absolute()
                .left(px(0.))
                .top(px(11.))
                .w(px(3.))
                .h(px(16.))
                .rounded_full()
                .when(selected, |pill| pill.bg(rgb(palette.accent))),
        )
        .child(
            crate::toolbar_icons::action(
                icon,
                if selected {
                    palette.accent
                } else {
                    palette.muted
                },
            )
            .size(px(18.)),
        )
        .child(title)
}
fn sidebar_width(window: &Window) -> f32 {
    // 页签：外边距 8×2、左内边距 12、图标 18、间距 12、文字右侧留白 14。
    let tabs = SettingsTab::ALL
        .iter()
        .map(|tab| tab.title())
        .chain(MainPage::CONTENT.iter().map(|page| page.title()))
        .chain([tr!("帮助", "Help")])
        .map(|title| widgets::text_width(window, title, 14., FontWeight::SEMIBOLD) + 72.)
        .fold(0., f32::max);
    // 标题栏品牌：左右 20、图标 18、间距 8。
    let brand = widgets::text_width(window, crate::brand::NAME, 16., FontWeight::SEMIBOLD) + 66.;
    let limit = (f32::from(window.viewport_size().width) * 0.3).max(120.);
    tabs.max(brand).clamp(120., 220.).min(limit)
}
impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let recovery_notices = crate::config_recovery::take_notices();
        if !recovery_notices.is_empty() {
            self.status = recovery_notices.join("；");
        }
        crate::theme::sync_window(window, cx);
        self.reconcile_media_popup(window, cx);
        self.reconcile_tool_popup(window, cx);
        self.ui_positions.borrow_mut().clear();
        let palette = crate::theme::palette(cx);
        let surface: Surface = widgets::surface(cx);
        let body_content = match self.page {
            MainPage::Settings => self.tab_content(window, cx),
            MainPage::Help => self.help_content(window, cx),
            MainPage::Pins | MainPage::History => div(),
        };
        let scroll_key = match self.page {
            MainPage::Settings => self.tab as usize,
            page => 100 + page.wire(),
        };
        // 贴图页与历史页自带布局和滚动区，不套设置页的整页滚动。
        let content = if self.page == MainPage::History {
            self.history_body(window, cx)
        } else if self.page == MainPage::Pins {
            self.pins_body(window, cx)
        } else {
            div()
                .id(("settings-scroll", scroll_key))
                .size_full()
                .overflow_y_scroll()
                .px_8()
                .pt_1()
                .pb_8()
                .flex()
                .flex_col()
                .min_w(px(0.))
                .child(body_content)
                .into_any_element()
        };
        let toast = self.status_toast(cx);
        let apply_bar = self.save_apply_bar(cx);
        let (page_title, page_description) = if self.page == MainPage::Settings {
            (self.tab.title(), self.tab.description())
        } else {
            (self.page.title(), self.page.description())
        };
        // 页面级操作放在标题右侧，不必滚到底部寻找。
        // 标题行整体是原生拖动区，操作按钮所在宽度要从拖动层里让出，否则点击会变成拖窗口。
        let (header_action, header_action_width) = match (self.page, self.tab) {
            (MainPage::Settings, SettingsTab::Toolbar) => {
                let width = Self::toolbar_reset_width(window);
                (Some(self.toolbar_reset_button(width, cx)), width)
            }
            (MainPage::Settings, SettingsTab::Save) => {
                let width = Self::save_reset_width(window);
                (Some(self.save_reset_button(width, cx)), width)
            }
            _ => (None, 0.),
        };
        let sidebar_width = sidebar_width(window);
        let band = titlebar::resize_band(window);
        // 表单 IME/快捷键录制守卫挂在正文上；标题行的拖动与窗口按钮走原生
        // 非客户区命中，不产生客户区按下，因此始终可用。
        let body = div()
            .size_full()
            .min_h(px(0.))
            .flex()
            .overflow_hidden()
            // 外部图片拖进主窗口任意位置：加入图库的“外部文件”组并选中。
            .on_drop(
                cx.listener(|view, paths: &gpui::ExternalPaths, window, cx| {
                    view.history_add_files(paths.paths().to_vec(), cx);
                    window.focus(&view.history_focus_handle());
                }),
            )
            .capture_any_mouse_down(cx.listener(|view, _, _, cx| {
                if view.save_settings.read(cx).is_composing(cx)
                    || view.translation_fields.is_composing(cx)
                {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    if view.recording_hotkey {
                        view.stop_recording_hotkey();
                        view.status = tr!(
                            "已取消录制，保留原有快捷键。",
                            "Recording canceled; the previous hotkey is kept."
                        )
                        .into();
                        cx.notify();
                    }
                }),
            )
            .child(self.sidebar(sidebar_width, band, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .min_h(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    // 页眉即标题行：页标题与侧栏品牌、窗口按钮同一中线。
                    .child(
                        div()
                            .relative()
                            .flex_shrink_0()
                            .child(
                                div()
                                    .pl_8()
                                    .pr(px(titlebar::CAPTION_WIDTH + 16.))
                                    .pt(px((titlebar::HEIGHT - 32.) / 2.))
                                    .pb_4()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .h(px(32.))
                                            .flex()
                                            .items_center()
                                            .gap_3()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .text_size(px(24.))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_ellipsis()
                                                    .child(page_title),
                                            )
                                            .children(header_action),
                                    )
                                    .child(
                                        div()
                                            .mt_1()
                                            .text_size(px(13.))
                                            .text_color(rgb(palette.muted))
                                            .child(page_description),
                                    ),
                            )
                            .child(titlebar::drag_layer(
                                if header_action_width > 0. {
                                    titlebar::CAPTION_WIDTH + 16. + header_action_width + 8.
                                } else {
                                    titlebar::CAPTION_WIDTH
                                },
                                band,
                            ))
                            .child(titlebar::caption_buttons(band, window, cx)),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_hidden()
                            .child(content)
                            .child(self.content_probe())
                            .children(toast),
                    )
                    .children(apply_bar),
            );
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(surface.page))
            .text_color(rgb(palette.foreground))
            .font_family("Microsoft YaHei UI")
            .text_size(px(14.))
            .child(body)
            .children(self.media_popup_element(window, cx))
            .children(self.tool_popup_element(window, cx))
    }
}
