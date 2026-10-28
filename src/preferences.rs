//! 小型本机偏好（settings.json）；字段名固定，未知字段忽略。历史默认关闭，不自动上传。
use crate::config_json::JsonFile;
use crate::hotkey::{HotkeySpec, HotkeyTarget, ALT, CTRL, SHIFT, WIN};
use crate::i18n::LanguageMode;
use crate::pin_zoom::ZoomAnchor;
use crate::ppocr::OcrProfile;
use crate::theme::ThemeMode;
use crate::trigger::{InputTrigger, Trigger};
use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
const MAX_FILE_BYTES: u64 = 64 * 1024;
const SCHEMA_VERSION: u32 = 1;

/// 新命令共用一个绑定模型；禁用时仍记住上次录制的组合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandHotkey {
    mode: u8,
    custom: HotkeySpec,
    input: Option<InputTrigger>,
}
impl Default for CommandHotkey {
    fn default() -> Self {
        Self {
            mode: 4,
            custom: HotkeySpec::default(),
            input: None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct CommandHotkeyFile {
    mode: u8,
    modifiers: u32,
    key: u32,
    trigger: String,
}
impl CommandHotkey {
    fn from_value(value: serde_json::Value) -> Self {
        let Ok(file) = serde_json::from_value::<CommandHotkeyFile>(value) else {
            return Self::default();
        };
        let custom = HotkeySpec {
            modifiers: file.modifiers,
            key: file.key,
        };
        let input = InputTrigger::from_wire(&file.trigger);
        let mode = match file.mode {
            3 if custom.validate().is_ok() => 3,
            6 if input.is_some() => 6,
            _ => 4,
        };
        Self {
            mode,
            custom: if custom.validate().is_ok() {
                custom
            } else {
                HotkeySpec::default()
            },
            input,
        }
    }
    fn value(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode,
            "modifiers": self.custom.modifiers,
            "key": self.custom.key,
            "trigger": self.input.as_ref().map(InputTrigger::to_wire).unwrap_or_default(),
        })
    }
}

#[derive(Clone)]
pub struct Preferences {
    pub theme: ThemeMode,
    pub language: LanguageMode,
    pub ocr_profile: OcrProfile,
    /// OCR 模型何时提前加载；默认只在截图待命进程预热。
    pub ocr_warmup: crate::ppocr::WarmupMode,
    pub history: bool,
    pub pin_restore: bool,
    pub pin_zoom_anchor: ZoomAnchor,
    /// 贴图可视标题条在图片下方或上方。
    pub pin_title_position: crate::pin_title::TitlePosition,
    pub magnifier: bool,
    pub guides: bool,
    pub auto_adsorb: bool,
    pub text_capture_copy_only: bool,
    pub include_cursor: bool,
    pub quick_dir: PathBuf,
    pub hotkey: u8,
    pub custom_hotkey: HotkeySpec,
    pub pin_hotkey: u8,
    pub custom_pin_hotkey: HotkeySpec,
    /// 热键模式为 6 时使用的键盘／鼠标组合，由常驻进程的输入监听匹配。
    pub input_trigger: Option<InputTrigger>,
    pub pin_input_trigger: Option<InputTrigger>,
    /// 除旧截图／剪贴板入口外，按稳定操作 ID 保存的通用绑定。
    pub(crate) command_hotkeys: BTreeMap<String, CommandHotkey>,
    /// 保留新版本命令的数据，但本版本不会注册或执行未知命令。
    pub(crate) unknown_command_hotkeys: BTreeMap<String, serde_json::Value>,
    /// OpenAI 兼容翻译接口根地址；空值关闭翻译入口。
    pub translate_endpoint: String,
    pub translate_model: String,
    /// 仅内存中的 API Key；磁盘只保存当前 Windows 用户的 DPAPI 密文。
    pub translate_api_key: String,
    /// 读取失败时保留原密文，避免其它设置页或关闭服务将其静默清空。
    pub(crate) translate_api_key_encrypted: String,
    /// `auto` 或目标语言代码。
    pub translate_target: String,
    /// 图片翻译引擎：`auto` 默认本机识别、已配置的文本服务翻译并本机排版；`baidu` 显式上传，`local` 本机排版。
    pub image_translate_engine: String,
    /// 百度翻译开放平台 APP ID 与「图片翻译 V2.0」API Key；只存本机配置。
    pub baidu_app_id: String,
    pub baidu_api_key: String,
    /// OCR 面板译文页是否在每段译文上方显示原文。
    pub translation_show_source: bool,
    /// 第一次用内置翻译服务时已说明文字会发往该服务；只提示一次。
    pub translation_notice_shown: bool,
    /// 截图左下快捷键提示卡；F1 切换并写回。
    pub capture_key_hints: bool,
    /// 工具窗是否按远程桌面／远控画面兼容显示，见 `remote_display`。
    pub remote_display: crate::remote_display::RemoteDisplayMode,
    /// 读取失败时的临时默认快照不能覆盖原配置；重新读取成功后才可保存。
    pub(crate) config_read_failed: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            language: LanguageMode::System,
            ocr_profile: OcrProfile::Tiny,
            ocr_warmup: crate::ppocr::WarmupMode::Standby,
            history: false,
            pin_restore: true,
            pin_zoom_anchor: ZoomAnchor::Pointer,
            pin_title_position: Default::default(),
            magnifier: true,
            guides: false,
            auto_adsorb: true,
            text_capture_copy_only: false,
            include_cursor: false,
            quick_dir: std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join("Pictures")
                .join(crate::brand::DATA_DIR),
            hotkey: 5,
            custom_hotkey: HotkeySpec::default(),
            pin_hotkey: 0,
            custom_pin_hotkey: HotkeyTarget::ClipboardPin.default_spec(),
            input_trigger: None,
            pin_input_trigger: None,
            command_hotkeys: BTreeMap::new(),
            unknown_command_hotkeys: BTreeMap::new(),
            translate_endpoint: crate::translate::DEFAULT_ENDPOINT.into(),
            translate_model: crate::translate::DEFAULT_MODEL.into(),
            translate_api_key: String::new(),
            translate_api_key_encrypted: String::new(),
            translate_target: "auto".into(),
            image_translate_engine: "auto".into(),
            baidu_app_id: String::new(),
            baidu_api_key: String::new(),
            translation_show_source: true,
            translation_notice_shown: false,
            capture_key_hints: true,
            remote_display: Default::default(),
            config_read_failed: false,
        }
    }
}
pub fn data_dir() -> PathBuf {
    std::env::var_os("RUSTSHOT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(crate::brand::local_data_root)
}
impl Preferences {
    pub fn load() -> Self {
        Self::load_at(&data_dir().join("settings.json"))
    }
    fn load_at(path: &Path) -> Self {
        match json_file(path).load() {
            Ok(Some(file)) => file.preferences(),
            Ok(None) => Self::default(),
            Err(error) => {
                crate::config_recovery::report_read_failure(path, &error);
                Self {
                    config_read_failed: true,
                    ..Self::default()
                }
            }
        }
    }
    /// 旧 settings.ini 解析器，只用于迁移；下个版本删除。
    fn parse_legacy_ini(text: &str) -> Self {
        let mut p = Self::default();
        let mut modifiers = None;
        let mut hotkey_key = None;
        let mut pin_modifiers = None;
        let mut pin_key = None;
        let mut pin_mode_present = false;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "theme" => p.theme = ThemeMode::from_wire(value),
                "language" => p.language = LanguageMode::from_wire(value),
                "ocrProfile" => p.ocr_profile = OcrProfile::parse(value).unwrap_or_default(),
                "ocrWarmup" => {
                    p.ocr_warmup = crate::ppocr::WarmupMode::parse(value).unwrap_or_default()
                }
                "history" => p.history = value == "true",
                "restoreClosedPins" => p.pin_restore = value != "false",
                "pinZoomAnchor" => {
                    p.pin_zoom_anchor = ZoomAnchor::from_wire(value).unwrap_or_default();
                }
                "magnifier" => p.magnifier = value != "false",
                "guides" => p.guides = value == "true",
                "autoAdsorb" => p.auto_adsorb = value != "false",
                "textCaptureCopyOnly" => p.text_capture_copy_only = value == "true",
                "translationShowSource" => p.translation_show_source = value != "false",
                "captureKeyHints" => p.capture_key_hints = value != "false",
                "includeCursor" => p.include_cursor = value == "true",
                "quickDirectory" => {
                    let path = PathBuf::from(value);
                    if path.is_absolute() {
                        p.quick_dir = path;
                    }
                }
                "hotkey" => {
                    p.hotkey = value
                        .parse::<u8>()
                        .ok()
                        .filter(|mode| *mode <= 5)
                        .unwrap_or(5);
                }
                "hotkeyModifiers" => modifiers = value.parse::<u32>().ok(),
                "hotkeyKey" => hotkey_key = value.parse::<u32>().ok(),
                "pinHotkey" => {
                    pin_mode_present = true;
                    p.pin_hotkey = value
                        .parse::<u8>()
                        .ok()
                        .filter(|mode| matches!(*mode, 0 | 3 | 4))
                        .unwrap_or(0);
                }
                "pinHotkeyModifiers" => pin_modifiers = value.parse::<u32>().ok(),
                "pinHotkeyKey" => pin_key = value.parse::<u32>().ok(),
                "translateEndpoint" => p.translate_endpoint = value.trim().into(),
                "translateModel" => p.translate_model = value.trim().into(),
                "translateTarget" => p.translate_target = value.trim().into(),
                "imageTranslateEngine" => p.image_translate_engine = value.trim().into(),
                "baiduAppId" => p.baidu_app_id = value.trim().into(),
                "baiduApiKey" => p.baidu_api_key = value.trim().into(),
                _ => {}
            }
        }
        let custom = modifiers
            .zip(hotkey_key)
            .map(|(modifiers, key)| HotkeySpec { modifiers, key });
        if let Some(custom) = custom.filter(|spec| spec.validate().is_ok()) {
            p.custom_hotkey = custom;
        } else if p.hotkey == 3 {
            // 老配置没有自定义字段；损坏组合也不能变成裸字符的全局热键。
            p.hotkey = 5;
        }
        let pin_custom = pin_modifiers
            .zip(pin_key)
            .map(|(modifiers, key)| HotkeySpec { modifiers, key });
        if let Some(custom) = pin_custom.filter(|spec| spec.validate().is_ok()) {
            p.custom_pin_hotkey = custom;
        } else if p.pin_hotkey == 3 {
            p.pin_hotkey = 0;
        }
        // 旧用户若已把 Ctrl+2 用作截图，新增默认不能夺走其明确配置。
        if !pin_mode_present && p.hotkeys_conflict() {
            p.pin_hotkey = 4;
        }
        p
    }
    pub fn save(&self) -> Result<()> {
        self.save_at(&data_dir().join("settings.json"))
    }
    /// 锁内读取最新设置，仅保存这一条快捷键；其它设置及 API 密文原样保留。
    pub fn save_hotkey_change(&self, target: HotkeyTarget) -> Result<Self> {
        self.save_hotkey_change_at(target, &data_dir().join("settings.json"))
    }
    fn save_hotkey_change_at(&self, target: HotkeyTarget, path: &Path) -> Result<Self> {
        if self.config_read_failed {
            return Err(tr!(
                "上次读取设置失败，请重新打开设置后再保存快捷键",
                "Settings could not be read; reopen settings before saving the shortcut"
            )
            .into());
        }
        let json = json_file(path);
        let _lock = json.lock()?;
        let mut file = json.read_locked()?.unwrap_or_default();
        let mut latest = file.clone().preferences();
        latest.restore_hotkey_from(target, self);
        latest.validate()?;
        file.copy_hotkey_from(target, &latest);
        json.write_locked(&file)?;
        Ok(latest)
    }
    /// 失败回滚只修改仍等于对应请求的绑定，迟到失败不会覆盖后续设置。
    pub fn revert_hotkeys(
        expected: &Self,
        previous: &Self,
        targets: &[HotkeyTarget],
    ) -> Result<()> {
        Self::revert_hotkeys_at(
            expected,
            previous,
            targets,
            &data_dir().join("settings.json"),
        )
    }
    fn revert_hotkeys_at(
        expected: &Self,
        previous: &Self,
        targets: &[HotkeyTarget],
        path: &Path,
    ) -> Result<()> {
        let json = json_file(path);
        let _lock = json.lock()?;
        let mut file = json.read_locked()?.unwrap_or_default();
        let mut latest = file.clone().preferences();
        let mut changed = false;
        for target in targets.iter().copied() {
            if latest.hotkey_matches(target, expected) {
                latest.restore_hotkey_from(target, previous);
                file.copy_hotkey_from(target, &latest);
                changed = true;
            }
        }
        if changed {
            latest.validate()?;
            json.write_locked(&file)?;
        }
        Ok(())
    }
    fn save_at(&self, path: &Path) -> Result<()> {
        if self.config_read_failed {
            return Err(tr!(
                "上次读取设置失败，为保护原配置，请重新打开设置后再保存",
                "Settings could not be read; reopen settings before saving to protect the original"
            )
            .into());
        }
        self.validate()?;
        let mut file = SettingsFile::from(self);
        if self.translate_api_key_unreadable() {
            if !self.translate_endpoint.is_empty() {
                return Err(tr!(
                    "无法读取已保存的 API Key，请在翻译设置重新填写；原配置未修改。",
                    "Could not read the saved API key. Enter it again in Translation settings; the original settings are unchanged."
                )
                .into());
            }
        } else {
            // 加密失败先返回，原子替换入口尚未运行，磁盘和内存草稿都保留。
            file.translate_api_key_encrypted = crate::api_secret::protect(&self.translate_api_key)?;
        }
        let json = json_file(path);
        if json.encode(&file).is_err() {
            return Err(tr!(
                "设置内容过长，请缩短目录或翻译配置",
                "Settings are too long; shorten the folder or translation settings"
            )
            .into());
        }
        // 加锁后先复核原文件：损坏的按规则备份，旧 INI 先迁移，再整份替换。
        Ok(json.replace(&file)?)
    }
    fn validate(&self) -> Result<()> {
        if let Some((first, second)) = self.hotkey_conflict_pair() {
            return Err(trf!(
                "{} 与 {} 的全局快捷键冲突",
                "The global shortcuts for {} and {} conflict",
                first.label(),
                second.label()
            )
            .into());
        }
        let path = self.quick_dir.to_string_lossy();
        if path.contains(['\n', '\r']) {
            return Err(tr!(
                "保存目录包含不支持的换行字符",
                "The save folder contains an unsupported line break"
            )
            .into());
        }
        Ok(())
    }
    pub(crate) fn translate_api_key_unreadable(&self) -> bool {
        !self.translate_api_key_encrypted.is_empty() && self.translate_api_key.is_empty()
    }
    pub(crate) fn set_translate_api_key(&mut self, key: String) {
        self.translate_api_key = key;
        self.translate_api_key_encrypted.clear();
    }
    pub(crate) fn preserve_translate_api_key_from(&mut self, latest: &Self) {
        self.translate_api_key = latest.translate_api_key.clone();
        self.translate_api_key_encrypted = latest.translate_api_key_encrypted.clone();
    }
    #[cfg(test)]
    fn serialize_legacy_ini(&self) -> Result<String> {
        self.validate()?;
        let path = self.quick_dir.to_string_lossy();
        let hotkey = match self.hotkey {
            0..=2 | 4..=5 => self.hotkey,
            3 if self.custom_hotkey.validate().is_ok() => 3,
            _ => 5,
        };
        let mut text = format!(
            "version=1\ntheme={}\nhistory={}\nmagnifier={}\nguides={}\nincludeCursor={}\nquickDirectory={}\nhotkey={}\n",
            self.theme.wire_name(), self.history, self.magnifier,
            self.guides, self.include_cursor, path, hotkey,
        );
        text.push_str(&format!("language={}\n", self.language.wire_name()));
        text.push_str(&format!("ocrProfile={}\n", self.ocr_profile.as_str()));
        text.push_str(&format!("ocrWarmup={}\n", self.ocr_warmup.as_str()));
        text.push_str(&format!("restoreClosedPins={}\n", self.pin_restore));
        text.push_str(&format!("autoAdsorb={}\n", self.auto_adsorb));
        text.push_str(&format!(
            "textCaptureCopyOnly={}\n",
            self.text_capture_copy_only
        ));
        text.push_str(&format!(
            "translationShowSource={}\n",
            self.translation_show_source
        ));
        text.push_str(&format!("captureKeyHints={}\n", self.capture_key_hints));
        text.push_str(&format!(
            "pinZoomAnchor={}\n",
            self.pin_zoom_anchor.wire_name()
        ));
        let custom = if self.custom_hotkey.validate().is_ok() {
            self.custom_hotkey
        } else {
            HotkeySpec::default()
        };
        text.push_str(&format!(
            "hotkeyModifiers={}\nhotkeyKey={}\n",
            custom.modifiers, custom.key
        ));
        let pin_hotkey = match self.pin_hotkey {
            0 | 4 => self.pin_hotkey,
            3 if self.custom_pin_hotkey.validate().is_ok() => 3,
            _ => 0,
        };
        let custom_pin = if self.custom_pin_hotkey.validate().is_ok() {
            self.custom_pin_hotkey
        } else {
            HotkeyTarget::ClipboardPin.default_spec()
        };
        text.push_str(&format!(
            "pinHotkey={}\npinHotkeyModifiers={}\npinHotkeyKey={}\n",
            pin_hotkey, custom_pin.modifiers, custom_pin.key
        ));
        for (key, value) in [
            ("translateEndpoint", &self.translate_endpoint),
            ("translateModel", &self.translate_model),
            ("translateTarget", &self.translate_target),
            ("imageTranslateEngine", &self.image_translate_engine),
            ("baiduAppId", &self.baidu_app_id),
            ("baiduApiKey", &self.baidu_api_key),
        ] {
            // 单行值；含换行的手改配置不能注入其它键。
            if !value.contains(['\n', '\r']) {
                text.push_str(&format!("{key}={value}\n"));
            }
        }
        Ok(text)
    }
    /// 写入用的规范值：无效的自定义组合不落盘，回到对应默认。
    fn normalized_hotkeys(&self) -> (u8, HotkeySpec, u8, HotkeySpec) {
        let hotkey = match self.hotkey {
            0..=2 | 4..=5 => self.hotkey,
            3 if self.custom_hotkey.validate().is_ok() => 3,
            6 if self.input_trigger.is_some() => 6,
            _ => 5,
        };
        let custom = if self.custom_hotkey.validate().is_ok() {
            self.custom_hotkey
        } else {
            HotkeySpec::default()
        };
        let pin_hotkey = match self.pin_hotkey {
            0 | 4 => self.pin_hotkey,
            3 if self.custom_pin_hotkey.validate().is_ok() => 3,
            6 if self.pin_input_trigger.is_some() => 6,
            _ => 0,
        };
        let custom_pin = if self.custom_pin_hotkey.validate().is_ok() {
            self.custom_pin_hotkey
        } else {
            HotkeyTarget::ClipboardPin.default_spec()
        };
        (hotkey, custom, pin_hotkey, custom_pin)
    }
    pub fn hotkey_label(&self) -> String {
        self.hotkey_label_for(HotkeyTarget::Capture)
    }
    pub fn hotkey_label_for(&self, target: HotkeyTarget) -> String {
        self.trigger_for(target)
            .map(|trigger| trigger.label())
            .unwrap_or_else(|| {
                if target.default_binding().is_some() {
                    tr!("已禁用", "Disabled").to_string()
                } else {
                    tr!("未设置", "Not set").to_string()
                }
            })
    }
    /// 当前生效的触发方式；系统热键与输入监听组合二选一。
    pub fn trigger_for(&self, target: HotkeyTarget) -> Option<Trigger> {
        match self.input_trigger_for(target) {
            Some(trigger) => Some(Trigger::Input(trigger.clone())),
            None => self.hotkey_spec_for(target).map(Trigger::Hotkey),
        }
    }
    /// 仅模式 6 返回；其余模式由系统热键负责。
    pub fn input_trigger_for(&self, target: HotkeyTarget) -> Option<&InputTrigger> {
        if self.hotkey_mode(target) != 6 {
            return None;
        }
        self.custom_input_trigger_for(target).as_ref()
    }
    pub(crate) fn custom_input_trigger_for(&self, target: HotkeyTarget) -> &Option<InputTrigger> {
        match target {
            HotkeyTarget::Capture => &self.input_trigger,
            HotkeyTarget::ClipboardPin => &self.pin_input_trigger,
            _ => self
                .command_hotkeys
                .get(target.id())
                .map_or(&None, |binding| &binding.input),
        }
    }
    fn set_input_trigger(&mut self, target: HotkeyTarget, trigger: Option<InputTrigger>) {
        match target {
            HotkeyTarget::Capture => self.input_trigger = trigger,
            HotkeyTarget::ClipboardPin => self.pin_input_trigger = trigger,
            _ => {
                self.command_hotkeys
                    .entry(target.id().into())
                    .or_default()
                    .input = trigger
            }
        }
    }
    /// 在刚读入的最新配置上改为录制得到的触发方式。
    pub fn with_trigger_change(&self, target: HotkeyTarget, trigger: Trigger) -> Self {
        match trigger {
            Trigger::Hotkey(spec) => self.with_hotkey_change(target, 3, Some(spec)),
            Trigger::Input(input) => {
                let mut updated = self.with_hotkey_change(target, 6, None);
                updated.set_input_trigger(target, Some(input));
                updated
            }
        }
    }
    pub fn hotkey(
        &self,
    ) -> Option<(
        windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS,
        u32,
    )> {
        self.hotkey_for(HotkeyTarget::Capture)
    }
    pub fn hotkey_for(
        &self,
        target: HotkeyTarget,
    ) -> Option<(
        windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS,
        u32,
    )> {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let spec = self.hotkey_spec_for(target)?;
        let mut modifiers = MOD_NOREPEAT;
        for (bit, native) in [
            (CTRL, MOD_CONTROL),
            (ALT, MOD_ALT),
            (SHIFT, MOD_SHIFT),
            (WIN, MOD_WIN),
        ] {
            if spec.modifiers & bit != 0 {
                modifiers |= native;
            }
        }
        Some((modifiers, spec.key))
    }
    pub fn hotkey_spec_for(&self, target: HotkeyTarget) -> Option<HotkeySpec> {
        if !matches!(target, HotkeyTarget::Capture | HotkeyTarget::ClipboardPin) {
            let binding = self.command_hotkeys.get(target.id())?;
            return (binding.mode == 3 && binding.custom.validate().is_ok()).then_some(binding.custom);
        }
        if target == HotkeyTarget::ClipboardPin {
            return match self.pin_hotkey {
                3 if self.custom_pin_hotkey.validate().is_ok() => Some(self.custom_pin_hotkey),
                4 => None,
                6 if self.pin_input_trigger.is_some() => None,
                _ => Some(target.default_spec()),
            };
        }
        match self.hotkey {
            0 => Some(HotkeySpec {
                modifiers: CTRL | ALT | SHIFT,
                key: u32::from(b'S'),
            }),
            1 => Some(HotkeySpec {
                modifiers: CTRL | ALT,
                key: 0x41,
            }),
            2 => Some(HotkeySpec {
                modifiers: 0,
                key: 0x2c,
            }),
            3 if self.custom_hotkey.validate().is_ok() => Some(self.custom_hotkey),
            4 => None,
            6 if self.input_trigger.is_some() => None,
            _ => Some(HotkeySpec::default()),
        }
    }
    pub fn hotkey_mode(&self, target: HotkeyTarget) -> u8 {
        match target {
            HotkeyTarget::Capture => self.hotkey,
            HotkeyTarget::ClipboardPin => self.pin_hotkey,
            _ => self
                .command_hotkeys
                .get(target.id())
                .map_or(4, |binding| binding.mode),
        }
    }
    pub fn set_hotkey_mode(&mut self, target: HotkeyTarget, mode: u8) {
        match target {
            HotkeyTarget::Capture => self.hotkey = mode,
            HotkeyTarget::ClipboardPin => self.pin_hotkey = mode,
            _ => {
                self.command_hotkeys
                    .entry(target.id().into())
                    .or_default()
                    .mode = if matches!(mode, 3 | 6) { mode } else { 4 }
            }
        }
    }
    pub fn custom_hotkey_for(&self, target: HotkeyTarget) -> HotkeySpec {
        match target {
            HotkeyTarget::Capture => self.custom_hotkey,
            HotkeyTarget::ClipboardPin => self.custom_pin_hotkey,
            _ => self
                .command_hotkeys
                .get(target.id())
                .map_or(HotkeySpec::default(), |binding| binding.custom),
        }
    }
    pub fn set_custom_hotkey(&mut self, target: HotkeyTarget, spec: HotkeySpec) {
        match target {
            HotkeyTarget::Capture => self.custom_hotkey = spec,
            HotkeyTarget::ClipboardPin => self.custom_pin_hotkey = spec,
            _ => {
                self.command_hotkeys
                    .entry(target.id().into())
                    .or_default()
                    .custom = spec
            }
        }
    }
    /// 在刚读入的最新配置上仅应用该动作的增量；另一动作和公共偏好保持原值。
    pub fn with_hotkey_change(
        &self,
        target: HotkeyTarget,
        mode: u8,
        custom: Option<HotkeySpec>,
    ) -> Self {
        let mut updated = self.clone();
        updated.set_hotkey_mode(target, mode);
        if let Some(custom) = custom {
            updated.set_custom_hotkey(target, custom);
        }
        updated
    }
    /// 基于刚读入的最新配置仅改变模型；不下载、不准备运行时，也不写回旧设置快照。
    pub(crate) fn with_ocr_profile_change(&self, profile: OcrProfile) -> Self {
        let mut updated = self.clone();
        updated.ocr_profile = profile;
        updated
    }
    pub fn restore_hotkey_from(&mut self, target: HotkeyTarget, previous: &Self) {
        if !matches!(target, HotkeyTarget::Capture | HotkeyTarget::ClipboardPin) {
            if let Some(binding) = previous.command_hotkeys.get(target.id()) {
                self.command_hotkeys
                    .insert(target.id().into(), binding.clone());
            } else {
                self.command_hotkeys.remove(target.id());
            }
            return;
        }
        self.set_hotkey_mode(target, previous.hotkey_mode(target));
        self.set_custom_hotkey(target, previous.custom_hotkey_for(target));
        self.set_input_trigger(target, previous.custom_input_trigger_for(target).clone());
    }
    pub fn hotkeys_conflict(&self) -> bool {
        self.hotkey_conflict_pair().is_some()
    }
    pub fn hotkey_conflict_pair(&self) -> Option<(HotkeyTarget, HotkeyTarget)> {
        let bindings: Vec<_> = HotkeyTarget::ALL
            .into_iter()
            .filter_map(|target| self.trigger_for(target).map(|trigger| (target, trigger)))
            .collect();
        for (index, (second, trigger)) in bindings.iter().enumerate() {
            for (first, earlier) in &bindings[..index] {
                if crate::trigger::triggers_conflict(earlier, trigger) {
                    return Some((*first, *second));
                }
            }
        }
        None
    }
    /// 比较该动作全部绑定字段，包含停用后保留的自定义值。
    pub fn hotkey_matches(&self, target: HotkeyTarget, other: &Self) -> bool {
        self.hotkey_mode(target) == other.hotkey_mode(target)
            && self.custom_hotkey_for(target) == other.custom_hotkey_for(target)
            && self.custom_input_trigger_for(target) == other.custom_input_trigger_for(target)
    }
}

#[cfg(test)]
#[path = "preferences_command_hotkey_tests.rs"]
mod command_hotkey_tests;

fn json_file(path: &Path) -> JsonFile<'_, SettingsFile> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: |text| SettingsFile::from(&Preferences::parse_legacy_ini(text)),
    }
}

/// settings.json 的磁盘结构；枚举按固定线名存字符串，读入后逐项校验。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct SettingsFile {
    theme: String,
    language: String,
    ocr_profile: String,
    ocr_warmup: String,
    history: bool,
    restore_closed_pins: bool,
    pin_zoom_anchor: String,
    pin_title_position: String,
    remote_display: String,
    magnifier: bool,
    guides: bool,
    auto_adsorb: bool,
    text_capture_copy_only: bool,
    translation_show_source: bool,
    translation_notice_shown: bool,
    capture_key_hints: bool,
    include_cursor: bool,
    quick_directory: PathBuf,
    /// 0–2 预设、3 自定义、4 禁用、5 默认、6 键盘／鼠标组合。
    hotkey: u8,
    hotkey_modifiers: u32,
    hotkey_key: u32,
    /// 模式 6 的组合线格式，见 `InputTrigger::to_wire`；空串表示未录制。
    capture_trigger: String,
    /// 0 默认、3 自定义、4 禁用、6 键盘／鼠标组合。
    pin_hotkey: u8,
    pin_hotkey_modifiers: u32,
    pin_hotkey_key: u32,
    pin_trigger: String,
    command_hotkeys: BTreeMap<String, serde_json::Value>,
    translate_endpoint: String,
    translate_model: String,
    translate_api_key_encrypted: String,
    translate_target: String,
    image_translate_engine: String,
    baidu_app_id: String,
    baidu_api_key: String,
}
impl Default for SettingsFile {
    fn default() -> Self {
        Self::from(&Preferences::default())
    }
}
impl From<&Preferences> for SettingsFile {
    fn from(p: &Preferences) -> Self {
        let (hotkey, custom, pin_hotkey, custom_pin) = p.normalized_hotkeys();
        Self {
            theme: p.theme.wire_name().into(),
            language: p.language.wire_name().into(),
            ocr_profile: p.ocr_profile.as_str().into(),
            ocr_warmup: p.ocr_warmup.as_str().into(),
            history: p.history,
            restore_closed_pins: p.pin_restore,
            pin_zoom_anchor: p.pin_zoom_anchor.wire_name().into(),
            pin_title_position: p.pin_title_position.wire_name().into(),
            remote_display: p.remote_display.wire_name().into(),
            magnifier: p.magnifier,
            guides: p.guides,
            auto_adsorb: p.auto_adsorb,
            text_capture_copy_only: p.text_capture_copy_only,
            translation_show_source: p.translation_show_source,
            translation_notice_shown: p.translation_notice_shown,
            capture_key_hints: p.capture_key_hints,
            include_cursor: p.include_cursor,
            quick_directory: p.quick_dir.clone(),
            hotkey,
            hotkey_modifiers: custom.modifiers,
            hotkey_key: custom.key,
            capture_trigger: p
                .input_trigger
                .as_ref()
                .map(InputTrigger::to_wire)
                .unwrap_or_default(),
            pin_hotkey,
            pin_hotkey_modifiers: custom_pin.modifiers,
            pin_hotkey_key: custom_pin.key,
            pin_trigger: p
                .pin_input_trigger
                .as_ref()
                .map(InputTrigger::to_wire)
                .unwrap_or_default(),
            command_hotkeys: p
                .unknown_command_hotkeys
                .iter()
                .map(|(id, value)| (id.clone(), value.clone()))
                .chain(
                    p.command_hotkeys
                        .iter()
                        .map(|(id, binding)| (id.clone(), binding.value())),
                )
                .collect(),
            translate_endpoint: p.translate_endpoint.clone(),
            translate_model: p.translate_model.clone(),
            translate_api_key_encrypted: p.translate_api_key_encrypted.clone(),
            translate_target: p.translate_target.clone(),
            image_translate_engine: p.image_translate_engine.clone(),
            baidu_app_id: p.baidu_app_id.clone(),
            baidu_api_key: p.baidu_api_key.clone(),
        }
    }
}
impl SettingsFile {
    fn copy_hotkey_from(&mut self, target: HotkeyTarget, preferences: &Preferences) {
        match target {
            HotkeyTarget::Capture => {
                let (mode, custom, _, _) = preferences.normalized_hotkeys();
                self.hotkey = mode;
                self.hotkey_modifiers = custom.modifiers;
                self.hotkey_key = custom.key;
                self.capture_trigger = preferences
                    .input_trigger
                    .as_ref()
                    .map(InputTrigger::to_wire)
                    .unwrap_or_default();
            }
            HotkeyTarget::ClipboardPin => {
                let (_, _, mode, custom) = preferences.normalized_hotkeys();
                self.pin_hotkey = mode;
                self.pin_hotkey_modifiers = custom.modifiers;
                self.pin_hotkey_key = custom.key;
                self.pin_trigger = preferences
                    .pin_input_trigger
                    .as_ref()
                    .map(InputTrigger::to_wire)
                    .unwrap_or_default();
            }
            _ => {
                if let Some(binding) = preferences.command_hotkeys.get(target.id()) {
                    self.command_hotkeys
                        .insert(target.id().into(), binding.value());
                } else {
                    self.command_hotkeys.remove(target.id());
                }
            }
        }
    }
    /// 与旧 INI 相同的校验：未知线名回到默认，相对目录忽略，损坏的自定义热键不生效。
    fn preferences(self) -> Preferences {
        // 解密失败只保留密文和明确的不可用状态，不退回明文、不清空磁盘字段。
        let api_key =
            crate::api_secret::unprotect(&self.translate_api_key_encrypted).unwrap_or_default();
        let mut p = Preferences {
            theme: ThemeMode::from_wire(&self.theme),
            language: LanguageMode::from_wire(&self.language),
            ocr_profile: OcrProfile::parse(&self.ocr_profile).unwrap_or_default(),
            ocr_warmup: crate::ppocr::WarmupMode::parse(&self.ocr_warmup).unwrap_or_default(),
            history: self.history,
            pin_restore: self.restore_closed_pins,
            pin_zoom_anchor: ZoomAnchor::from_wire(&self.pin_zoom_anchor).unwrap_or_default(),
            pin_title_position: crate::pin_title::TitlePosition::from_wire(
                &self.pin_title_position,
            )
            .unwrap_or_default(),
            remote_display: crate::remote_display::RemoteDisplayMode::from_wire(
                &self.remote_display,
            ),
            magnifier: self.magnifier,
            guides: self.guides,
            auto_adsorb: self.auto_adsorb,
            text_capture_copy_only: self.text_capture_copy_only,
            translation_show_source: self.translation_show_source,
            translation_notice_shown: self.translation_notice_shown,
            capture_key_hints: self.capture_key_hints,
            include_cursor: self.include_cursor,
            hotkey: if self.hotkey <= 6 { self.hotkey } else { 5 },
            pin_hotkey: if matches!(self.pin_hotkey, 0 | 3 | 4 | 6) {
                self.pin_hotkey
            } else {
                0
            },
            input_trigger: InputTrigger::from_wire(&self.capture_trigger),
            pin_input_trigger: InputTrigger::from_wire(&self.pin_trigger),
            translate_endpoint: self.translate_endpoint.trim().into(),
            translate_model: self.translate_model.trim().into(),
            translate_api_key: api_key,
            translate_api_key_encrypted: self.translate_api_key_encrypted,
            translate_target: self.translate_target.trim().into(),
            image_translate_engine: self.image_translate_engine.trim().into(),
            baidu_app_id: self.baidu_app_id.trim().into(),
            baidu_api_key: self.baidu_api_key.trim().into(),
            ..Preferences::default()
        };
        // 只迁移精确的历史内置组合；自定义模型、其它端点与关闭状态保持原值。
        if p.translate_endpoint.trim_end_matches('/') == "https://index-translate.bilibili.com/v1"
            && p.translate_model == "Index-Translate-35B-A3B"
        {
            p.translate_endpoint = crate::translate::DEFAULT_ENDPOINT.into();
            p.translate_model = crate::translate::DEFAULT_MODEL.into();
        }
        if self.quick_directory.is_absolute() {
            p.quick_dir = self.quick_directory;
        }
        let custom = HotkeySpec {
            modifiers: self.hotkey_modifiers,
            key: self.hotkey_key,
        };
        if custom.validate().is_ok() {
            p.custom_hotkey = custom;
        } else if p.hotkey == 3 {
            // 损坏组合不能变成裸字符的全局热键。
            p.hotkey = 5;
        }
        let pin_custom = HotkeySpec {
            modifiers: self.pin_hotkey_modifiers,
            key: self.pin_hotkey_key,
        };
        if pin_custom.validate().is_ok() {
            p.custom_pin_hotkey = pin_custom;
        } else if p.pin_hotkey == 3 {
            p.pin_hotkey = 0;
        }
        // 组合损坏或由旧版写回时回到默认，不会静默变成别的组合。
        if p.hotkey == 6 && p.input_trigger.is_none() {
            p.hotkey = 5;
        }
        if p.pin_hotkey == 6 && p.pin_input_trigger.is_none() {
            p.pin_hotkey = 0;
        }
        // 旧两个动作仍沿旧字段读取，避免通用表的同名条目覆盖已有配置。
        for (id, value) in self.command_hotkeys {
            if HotkeyTarget::ALL.into_iter().any(|target| {
                target.id() == id
                    && !matches!(target, HotkeyTarget::Capture | HotkeyTarget::ClipboardPin)
            }) {
                p.command_hotkeys
                    .insert(id, CommandHotkey::from_value(value));
            } else {
                p.unknown_command_hotkeys.insert(id, value);
            }
        }
        // 手改出的冲突组合不能让剪贴板贴图抢走截图快捷键。
        if p.trigger_for(HotkeyTarget::Capture)
            .zip(p.trigger_for(HotkeyTarget::ClipboardPin))
            .is_some_and(|(capture, pin)| crate::trigger::triggers_conflict(&capture, &pin))
        {
            p.pin_hotkey = 4;
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestPreferencesDirectory(PathBuf);
    impl TestPreferencesDirectory {
        fn new() -> Self {
            static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rustshot-preferences-recovery-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> PathBuf {
            self.0.join("settings.json")
        }
        fn backups(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
                .collect()
        }
    }
    impl Drop for TestPreferencesDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preferences_config_recovery_preserves_invalid_bytes_before_loading_defaults() {
        for bytes in [vec![0xff, 0xfe], vec![b'x'; MAX_FILE_BYTES as usize + 1]] {
            let dir = TestPreferencesDirectory::new();
            std::fs::write(dir.config(), &bytes).unwrap();
            let loaded = Preferences::load_at(&dir.config());
            assert_eq!(loaded.theme, ThemeMode::System);
            assert!(!dir.config().exists());
            let backups = dir.backups();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read(&backups[0]).unwrap(), bytes);
        }
    }

    #[test]
    fn preferences_config_recovery_rechecks_file_before_saving_old_snapshot() {
        let dir = TestPreferencesDirectory::new();
        std::fs::write(dir.config(), r#"{"theme":"light","future_setting":"ok"}"#).unwrap();
        let mut loaded = Preferences::load_at(&dir.config());
        assert_eq!(loaded.theme, ThemeMode::Light);
        assert!(dir.backups().is_empty());
        let invalid = b"version=1\nsecret=\xff\n";
        std::fs::write(dir.config(), invalid).unwrap();
        loaded.theme = ThemeMode::Dark;
        loaded.save_at(&dir.config()).unwrap();
        assert_eq!(Preferences::load_at(&dir.config()).theme, ThemeMode::Dark);
        assert_eq!(std::fs::read(&dir.backups()[0]).unwrap(), invalid);
    }

    #[cfg(windows)]
    #[test]
    fn preferences_config_recovery_read_failure_blocks_replacement() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = TestPreferencesDirectory::new();
        let original = br#"{"schema_version":1,"theme":"light"}"#;
        std::fs::write(dir.config(), original).unwrap();
        let denied = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.config())
            .unwrap();
        let mut defaults = Preferences::load_at(&dir.config());
        defaults.theme = ThemeMode::Dark;
        assert!(defaults.save_at(&dir.config()).is_err());
        drop(denied);
        assert_eq!(std::fs::read(dir.config()).unwrap(), original);
        assert!(dir.backups().is_empty());
        // 权限恢复后，失败默认快照仍不可覆盖；必须重新读取有效配置。
        assert!(defaults.save_at(&dir.config()).is_err());
        let mut reread = Preferences::load_at(&dir.config());
        reread.theme = ThemeMode::Dark;
        reread.save_at(&dir.config()).unwrap();
        assert_eq!(Preferences::load_at(&dir.config()).theme, ThemeMode::Dark);
    }

    #[test]
    fn ocr_warmup_defaults_to_standby_and_round_trips() {
        use crate::ppocr::WarmupMode;
        assert_eq!(Preferences::default().ocr_warmup, WarmupMode::Standby);
        assert_eq!(
            Preferences::parse_legacy_ini("ocrWarmup=bogus\n").ocr_warmup,
            WarmupMode::Standby
        );
        for mode in [WarmupMode::Off, WarmupMode::Always] {
            let mut preferences = Preferences::default();
            preferences.ocr_warmup = mode;
            let text = preferences.serialize_legacy_ini().unwrap();
            assert_eq!(Preferences::parse_legacy_ini(&text).ocr_warmup, mode);
        }
    }

    #[test]
    fn ocr_profile_missing_invalid_and_legacy_preferences_keep_tiny() {
        assert_eq!(Preferences::default().ocr_profile, OcrProfile::Tiny);
        for text in [
            "",
            "version=1\n",
            "ocrProfile=\n",
            "ocrProfile=unknown\n",
            "ocrProfile=SMALL\n",
        ] {
            let p = Preferences::parse_legacy_ini(text);
            assert_eq!(p.ocr_profile, OcrProfile::Tiny, "{text}");
        }
        assert_eq!(
            Preferences::parse_legacy_ini("ocrProfile=small\n").ocr_profile,
            OcrProfile::Small
        );
    }
    #[test]
    fn ocr_profiles_round_trip_preserves_all_other_serialized_settings() {
        let original = Preferences::parse_legacy_ini("theme=dark\nlanguage=en\nhistory=true\nrestoreClosedPins=false\npinZoomAnchor=center\nmagnifier=false\nguides=true\nautoAdsorb=false\ntextCaptureCopyOnly=true\nincludeCursor=true\nquickDirectory=D:\\截图\\RustShot\nhotkey=4\npinHotkey=4\ntranslateEndpoint=http://127.0.0.1:8000/v1\ntranslateModel=m\ntranslateTarget=ja\n");
        let baseline = original.serialize_legacy_ini().unwrap();
        for profile in [OcrProfile::Tiny, OcrProfile::Small] {
            let next = original.with_ocr_profile_change(profile);
            let serialized = next.serialize_legacy_ini().unwrap();
            assert!(serialized.contains(&format!("ocrProfile={}\n", profile.as_str())));
            let restored = Preferences::parse_legacy_ini(&serialized);
            assert_eq!(restored.ocr_profile, profile);
            assert_eq!(restored.serialize_legacy_ini().unwrap(), serialized);
            assert_eq!(
                serialized.replace(&format!("ocrProfile={}\n", profile.as_str()), ""),
                baseline.replace("ocrProfile=tiny\n", "")
            );
        }
    }
    #[test]
    fn profile_delta_uses_latest_preferences_and_other_deltas_preserve_profile() {
        let latest = Preferences::parse_legacy_ini(
            "ocrProfile=small\ntheme=dark\nlanguage=en\nhistory=true\ntranslateTarget=ja\n",
        );
        let hotkey = latest.with_hotkey_change(HotkeyTarget::ClipboardPin, 4, None);
        assert_eq!(hotkey.ocr_profile, OcrProfile::Small);
        let changed = hotkey.with_ocr_profile_change(OcrProfile::Tiny);
        assert_eq!(changed.ocr_profile, OcrProfile::Tiny);
        assert_eq!(changed.pin_hotkey, 4);
        assert_eq!(changed.theme, ThemeMode::Dark);
        assert_eq!(changed.language, LanguageMode::English);
        assert!(changed.history);
        assert_eq!(changed.translate_target, "ja");
        assert_eq!(hotkey.ocr_profile, OcrProfile::Small);
    }
    #[test]
    fn translate_service_round_trips_and_empty_endpoint_disables() {
        let defaults = Preferences::parse_legacy_ini("");
        assert_eq!(
            defaults.translate_endpoint,
            crate::translate::DEFAULT_ENDPOINT
        );
        assert_eq!(defaults.translate_target, "auto");
        let custom = Preferences::parse_legacy_ini(
            "translateEndpoint=http://127.0.0.1:8000/v1\ntranslateModel=m\ntranslateTarget=ja\n",
        );
        let loaded = Preferences::parse_legacy_ini(&custom.serialize_legacy_ini().unwrap());
        assert_eq!(loaded.translate_endpoint, "http://127.0.0.1:8000/v1");
        assert_eq!(loaded.translate_model, "m");
        assert_eq!(loaded.translate_target, "ja");
        let disabled = Preferences::parse_legacy_ini(
            &Preferences::parse_legacy_ini("translateEndpoint=\n")
                .serialize_legacy_ini()
                .unwrap(),
        );
        assert!(disabled.translate_endpoint.is_empty());
    }
    #[test]
    fn obsolete_ocr_thread_field_is_ignored_and_not_saved() {
        for value in ["1", "2", "4", "", "invalid"] {
            let preferences = Preferences::parse_legacy_ini(&format!(
                "theme=dark\nlanguage=en\nhistory=true\nocrThreads={value}\n"
            ));
            let serialized = preferences.serialize_legacy_ini().unwrap();
            assert!(!serialized.contains("ocrThreads="));
            let loaded = Preferences::parse_legacy_ini(&serialized);
            assert_eq!(loaded.theme, ThemeMode::Dark);
            assert_eq!(loaded.language, LanguageMode::English);
            assert!(loaded.history);
        }
    }

    #[test]
    fn theme_preferences_default_unknown_and_legacy_wire() {
        assert_eq!(Preferences::default().theme, ThemeMode::System);
        for text in [
            "",
            "version=1\n",
            "theme=system\n",
            "theme=unknown\n",
            "theme=\n",
        ] {
            assert_eq!(
                Preferences::parse_legacy_ini(text).theme,
                ThemeMode::System,
                "{text}"
            );
        }
        assert_eq!(
            Preferences::parse_legacy_ini("theme=light\n").theme,
            ThemeMode::Light
        );
        assert_eq!(
            Preferences::parse_legacy_ini("theme=dark\n").theme,
            ThemeMode::Dark
        );
    }

    #[test]
    fn language_preferences_default_unknown_and_round_trip() {
        assert_eq!(Preferences::default().language, LanguageMode::System);
        assert_eq!(
            Preferences::parse_legacy_ini("language=fr\n").language,
            LanguageMode::System
        );
        // 旧配置没有 language 字段时跟随系统，其它字段不受影响。
        let old = Preferences::parse_legacy_ini("version=1\ntheme=dark\nhistory=true\n");
        assert_eq!(old.language, LanguageMode::System);
        assert_eq!(old.theme, ThemeMode::Dark);
        for mode in [
            LanguageMode::System,
            LanguageMode::Chinese,
            LanguageMode::English,
        ] {
            let mut preferences = old.clone();
            preferences.language = mode;
            let restored =
                Preferences::parse_legacy_ini(&preferences.serialize_legacy_ini().unwrap());
            assert_eq!(restored.language, mode);
            assert_eq!(restored.theme, ThemeMode::Dark);
            assert!(restored.history);
        }
    }

    #[test]
    fn theme_preferences_three_modes_round_trip_preserve_other_fields() {
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            let mut preferences = Preferences::parse_legacy_ini("history=true\nrestoreClosedPins=false\npinZoomAnchor=pointer\nhotkey=3\nhotkeyModifiers=9\nhotkeyKey=120\n");
            preferences.theme = mode;
            let serialized = preferences.serialize_legacy_ini().unwrap();
            assert!(serialized.contains(&format!("theme={}\n", mode.wire_name())));
            let restored = Preferences::parse_legacy_ini(&serialized);
            assert_eq!(restored.theme, mode);
            assert!(restored.history);
            assert!(!restored.pin_restore);
            assert_eq!(restored.pin_zoom_anchor, ZoomAnchor::Pointer);
            assert_eq!(restored.hotkey(), preferences.hotkey());
            assert_eq!(restored.custom_hotkey, preferences.custom_hotkey);
        }
    }

    #[test]
    fn pin_zoom_preferences_missing_and_unknown_wire_default_to_pointer() {
        assert_eq!(ZoomAnchor::default(), ZoomAnchor::Pointer);
        assert_eq!(Preferences::default().pin_zoom_anchor, ZoomAnchor::Pointer);
        let old = Preferences::parse_legacy_ini(
            "version=1\ntheme=light\nrestoreClosedPins=false\nhotkey=1\n",
        );
        assert_eq!(old.pin_zoom_anchor, ZoomAnchor::Pointer);
        assert_eq!(old.theme, ThemeMode::Light);
        assert!(!old.pin_restore);
        assert_eq!(old.hotkey_label(), "Ctrl+Alt+A");
        for wire in ["", "unknown", "0", "NaN", "mouse", "TopLeft"] {
            let loaded =
                Preferences::parse_legacy_ini(&format!("hotkey=2\npinZoomAnchor={wire}\n"));
            assert_eq!(loaded.pin_zoom_anchor, ZoomAnchor::Pointer, "{wire}");
            assert_eq!(loaded.hotkey_label(), "PrintScreen");
        }
    }

    #[test]
    fn pin_zoom_preferences_three_anchors_round_trip_preserve_hotkeys_and_common_fields() {
        for (anchor, wire) in [
            (ZoomAnchor::TopLeft, "top-left"),
            (ZoomAnchor::Pointer, "pointer"),
            (ZoomAnchor::Center, "center"),
        ] {
            let mut preferences = Preferences::parse_legacy_ini("version=1\ntheme=light\nhistory=true\nrestoreClosedPins=false\nmagnifier=false\nguides=true\nincludeCursor=true\nquickDirectory=D:\\截图\\RustShot\nhotkey=3\nhotkeyModifiers=9\nhotkeyKey=120\n");
            preferences.pin_zoom_anchor = anchor;
            let serialized = preferences.serialize_legacy_ini().unwrap();
            assert!(serialized.contains(&format!("pinZoomAnchor={wire}\n")));
            let loaded = Preferences::parse_legacy_ini(&serialized);
            assert_eq!(loaded.pin_zoom_anchor, anchor);
            let tolerant = Preferences::parse_legacy_ini(&format!(
                "pinZoomAnchor= {} \n",
                wire.to_ascii_uppercase()
            ));
            assert_eq!(tolerant.pin_zoom_anchor, anchor);
            assert_eq!(loaded.hotkey, 3);
            assert_eq!(loaded.custom_hotkey, preferences.custom_hotkey);
            assert_eq!(loaded.hotkey_label(), "Ctrl+Win+F9");
            assert_eq!(loaded.hotkey(), preferences.hotkey());
            assert_eq!(loaded.quick_dir, preferences.quick_dir);
            assert_eq!(loaded.theme, ThemeMode::Light);
            assert!(loaded.history && loaded.guides && loaded.include_cursor);
            assert!(!loaded.pin_restore && !loaded.magnifier);
            // 禁用模式仍需保留录制的组合，切回自定义不会丢失。
            preferences.hotkey = 4;
            let disabled =
                Preferences::parse_legacy_ini(&preferences.serialize_legacy_ini().unwrap());
            assert_eq!(disabled.pin_zoom_anchor, anchor);
            assert!(disabled.hotkey().is_none());
            assert_eq!(disabled.custom_hotkey, preferences.custom_hotkey);
        }
    }

    #[test]
    fn control_selection_and_text_copy_preferences_preserve_legacy_defaults_and_round_trip() {
        let legacy = Preferences::parse_legacy_ini("version=1\n");
        assert!(legacy.auto_adsorb);
        assert!(!legacy.text_capture_copy_only);
        let preferences =
            Preferences::parse_legacy_ini("autoAdsorb=false\ntextCaptureCopyOnly=true\n");
        let restored = Preferences::parse_legacy_ini(&preferences.serialize_legacy_ini().unwrap());
        assert!(!restored.auto_adsorb);
        assert!(restored.text_capture_copy_only);
    }

    #[test]
    fn translation_show_source_defaults_on_and_round_trips_off() {
        assert!(Preferences::parse_legacy_ini("version=1\n").translation_show_source);
        let hidden = Preferences::parse_legacy_ini("translationShowSource=false\n");
        assert!(!hidden.translation_show_source);
        let restored = Preferences::parse_legacy_ini(&hidden.serialize_legacy_ini().unwrap());
        assert!(!restored.translation_show_source);
    }

    #[test]
    fn capture_key_hints_default_on_and_round_trip_off() {
        assert!(Preferences::parse_legacy_ini("version=1\n").capture_key_hints);
        let hidden = Preferences::parse_legacy_ini("captureKeyHints=false\n");
        assert!(!hidden.capture_key_hints);
        let restored = Preferences::parse_legacy_ini(&hidden.serialize_legacy_ini().unwrap());
        assert!(!restored.capture_key_hints);
    }

    #[test]
    fn settings_parse_unicode_paths_defaults_and_unknown_fields() {
        let p = Preferences::parse_legacy_ini("version=1\ntheme=light\nhistory=true\nmagnifier=false\nincludeCursor=true\nquickDirectory=D:\\截图\\RustShot\nhotkey=1\nfutureSetting=anything\n");
        assert_eq!(p.theme, ThemeMode::Light);
        assert!(p.history && p.include_cursor);
        assert!(!p.magnifier);
        assert_eq!(p.hotkey_label(), "Ctrl+Alt+A");
        assert!(p.quick_dir.is_absolute());
        let defaults = Preferences::parse_legacy_ini("hotkey=broken\nquickDirectory=relative\n");
        assert_eq!(defaults.hotkey, 5);
        assert!(!defaults.history);
        assert_eq!(defaults.quick_dir, Preferences::default().quick_dir);
    }

    #[test]
    fn old_presets_and_invalid_custom_settings_keep_a_usable_binding() {
        for (mode, label) in [
            (0, "Ctrl+Alt+Shift+S"),
            (1, "Ctrl+Alt+A"),
            (2, "PrintScreen"),
        ] {
            let p = Preferences::parse_legacy_ini(&format!("version=1\nhotkey={mode}\n"));
            assert_eq!(p.hotkey, mode);
            assert_eq!(p.hotkey_label(), label);
            assert_eq!(p.custom_hotkey, HotkeySpec::default());
        }
        for text in [
            "hotkey=3\n",
            "hotkey=3\nhotkeyModifiers=1\n",
            "hotkey=3\nhotkeyModifiers=0\nhotkeyKey=83\n",
            "hotkey=3\nhotkeyModifiers=16\nhotkeyKey=83\n",
            "hotkey=3\nhotkeyModifiers=1\nhotkeyKey=17\n",
            "hotkey=3\nhotkeyModifiers=broken\nhotkeyKey=83\n",
            "hotkey=255\n",
        ] {
            let p = Preferences::parse_legacy_ini(text);
            assert_eq!(p.hotkey, 5, "{text}");
            assert_eq!(p.hotkey_label(), "Ctrl+1");
            assert!(p.hotkey().is_some());
        }
    }

    #[test]
    fn custom_and_disabled_ini_round_trip_and_translate_modifier_bits() {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        let mut p = Preferences::parse_legacy_ini("hotkey=3\nhotkeyModifiers=9\nhotkeyKey=120\n");
        assert_eq!(p.hotkey_label(), "Ctrl+Win+F9");
        assert_eq!(
            p.hotkey(),
            Some((MOD_CONTROL | MOD_WIN | MOD_NOREPEAT, 0x78))
        );
        let text = p.serialize_legacy_ini().unwrap();
        assert!(text.contains("hotkey=3\n"));
        assert!(text.contains("hotkeyModifiers=9\nhotkeyKey=120\n"));
        let loaded = Preferences::parse_legacy_ini(&text);
        assert_eq!(loaded.custom_hotkey, p.custom_hotkey);
        assert_eq!(loaded.hotkey(), p.hotkey());
        p.hotkey = 4;
        let disabled = Preferences::parse_legacy_ini(&p.serialize_legacy_ini().unwrap());
        assert!(disabled.hotkey().is_none());
        assert_eq!(disabled.hotkey_label(), "已禁用");
        assert_eq!(disabled.custom_hotkey, p.custom_hotkey);
        // 模型 Ctrl/Alt 位故意不同于 Win32，不能直接强转。
        p.hotkey = 3;
        p.custom_hotkey = HotkeySpec::from_key_name("left", ALT).unwrap();
        assert_eq!(p.hotkey(), Some((MOD_ALT | MOD_NOREPEAT, 0x25)));
    }

    #[test]
    fn new_preferences_default_to_capture_and_clipboard_pin_control_digits() {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        for preferences in [Preferences::default(), Preferences::parse_legacy_ini("")] {
            assert_eq!(preferences.hotkey, 5);
            assert_eq!(preferences.hotkey_label(), "Ctrl+1");
            assert_eq!(
                preferences.hotkey_for(HotkeyTarget::Capture),
                Some((MOD_CONTROL | MOD_NOREPEAT, 0x31))
            );
            assert_eq!(preferences.pin_hotkey, 0);
            assert_eq!(
                preferences.hotkey_label_for(HotkeyTarget::ClipboardPin),
                "Ctrl+2"
            );
            assert_eq!(
                preferences.hotkey_for(HotkeyTarget::ClipboardPin),
                Some((MOD_CONTROL | MOD_NOREPEAT, 0x32))
            );
            assert!(!preferences.hotkeys_conflict());
            let loaded =
                Preferences::parse_legacy_ini(&preferences.serialize_legacy_ini().unwrap());
            assert_eq!(
                loaded.hotkey_for(HotkeyTarget::Capture),
                preferences.hotkey()
            );
            assert_eq!(
                loaded.hotkey_for(HotkeyTarget::ClipboardPin),
                preferences.hotkey_for(HotkeyTarget::ClipboardPin)
            );
        }
    }

    #[test]
    fn clipboard_pin_custom_and_disabled_choices_round_trip_independently() {
        let preferences = Preferences::parse_legacy_ini("hotkey=4\nhotkeyModifiers=9\nhotkeyKey=120\npinHotkey=3\npinHotkeyModifiers=2\npinHotkeyKey=81\n");
        assert_eq!(preferences.hotkey_label(), "已禁用");
        assert_eq!(
            preferences.hotkey_label_for(HotkeyTarget::ClipboardPin),
            "Alt+Q"
        );
        let mut loaded =
            Preferences::parse_legacy_ini(&preferences.serialize_legacy_ini().unwrap());
        assert_eq!(loaded.custom_hotkey, preferences.custom_hotkey);
        assert_eq!(loaded.custom_pin_hotkey, preferences.custom_pin_hotkey);
        loaded.pin_hotkey = 4;
        let disabled = Preferences::parse_legacy_ini(&loaded.serialize_legacy_ini().unwrap());
        assert!(disabled.hotkey_for(HotkeyTarget::ClipboardPin).is_none());
        assert_eq!(disabled.custom_pin_hotkey, preferences.custom_pin_hotkey);
        assert_eq!(disabled.custom_hotkey, preferences.custom_hotkey);
    }

    #[test]
    fn missing_pin_default_preserves_existing_capture_ctrl_two_and_disabled_state() {
        let legacy = Preferences::parse_legacy_ini("hotkey=3\nhotkeyModifiers=1\nhotkeyKey=50\n");
        assert_eq!(legacy.hotkey_label(), "Ctrl+2");
        assert_eq!(legacy.pin_hotkey, 4);
        assert!(!legacy.hotkeys_conflict());
        let loaded = Preferences::parse_legacy_ini(&legacy.serialize_legacy_ini().unwrap());
        assert_eq!(loaded.hotkey_label(), "Ctrl+2");
        assert_eq!(loaded.pin_hotkey, 4);
        let disabled = Preferences::parse_legacy_ini("hotkey=4\n");
        assert!(disabled.hotkey().is_none());
        assert_eq!(
            disabled.hotkey_label_for(HotkeyTarget::ClipboardPin),
            "Ctrl+2"
        );
    }

    #[test]
    fn explicit_identical_action_bindings_are_rejected_but_disabled_custom_is_retained() {
        let mut preferences = Preferences::parse_legacy_ini(
            "hotkey=3\nhotkeyModifiers=1\nhotkeyKey=50\npinHotkey=0\n",
        );
        assert!(preferences.hotkeys_conflict());
        assert!(preferences.serialize_legacy_ini().is_err());
        let defaults = Preferences::default();
        preferences.restore_hotkey_from(HotkeyTarget::Capture, &defaults);
        assert!(!preferences.hotkeys_conflict());
        assert_eq!(preferences.hotkey_label(), "Ctrl+1");
        preferences.pin_hotkey = 4;
        preferences.custom_pin_hotkey = HotkeyTarget::Capture.default_spec();
        assert!(!preferences.hotkeys_conflict());
        let loaded = Preferences::parse_legacy_ini(&preferences.serialize_legacy_ini().unwrap());
        assert_eq!(loaded.custom_pin_hotkey, preferences.custom_pin_hotkey);
    }

    #[test]
    fn invalid_clipboard_pin_configuration_falls_back_to_valid_default() {
        for text in [
            "pinHotkey=255\n",
            "pinHotkey=broken\n",
            "pinHotkey=3\n",
            "pinHotkey=3\npinHotkeyModifiers=0\npinHotkeyKey=50\n",
            "pinHotkey=3\npinHotkeyModifiers=1\npinHotkeyKey=17\n",
        ] {
            let preferences = Preferences::parse_legacy_ini(text);
            assert_eq!(preferences.pin_hotkey, 0, "{text}");
            assert_eq!(
                preferences.hotkey_label_for(HotkeyTarget::ClipboardPin),
                "Ctrl+2"
            );
            assert!(!preferences.hotkeys_conflict());
        }
    }

    #[test]
    fn target_hotkey_delta_preserves_latest_other_action_and_common_preferences() {
        let mut latest = Preferences::parse_legacy_ini(
            "theme=dark\nhistory=true\npinHotkey=3\npinHotkeyModifiers=2\npinHotkeyKey=81\n",
        );
        latest.guides = true;
        let capture = latest.with_hotkey_change(
            HotkeyTarget::Capture,
            3,
            Some(HotkeySpec::from_key_name("F9", WIN).unwrap()),
        );
        assert_eq!(capture.hotkey_label(), "Win+F9");
        assert_eq!(capture.pin_hotkey, latest.pin_hotkey);
        assert_eq!(capture.custom_pin_hotkey, latest.custom_pin_hotkey);
        assert_eq!(capture.theme, ThemeMode::Dark);
        assert!(capture.history && capture.guides);
        let pin = capture.with_hotkey_change(HotkeyTarget::ClipboardPin, 4, None);
        assert_eq!(pin.hotkey, capture.hotkey);
        assert_eq!(pin.custom_hotkey, capture.custom_hotkey);
        assert_eq!(pin.custom_pin_hotkey, latest.custom_pin_hotkey);
        assert!(pin.hotkey_for(HotkeyTarget::ClipboardPin).is_none());
        let loaded = Preferences::parse_legacy_ini(&pin.serialize_legacy_ini().unwrap());
        assert_eq!(loaded.hotkey_label(), "Win+F9");
        assert_eq!(loaded.pin_hotkey, 4);
        assert_eq!(loaded.theme, ThemeMode::Dark);
    }

    fn json_value(preferences: &Preferences) -> serde_json::Value {
        serde_json::to_value(SettingsFile::from(preferences)).unwrap()
    }
    fn customized() -> Preferences {
        Preferences::parse_legacy_ini("theme=dark\nlanguage=en\nocrProfile=small\nocrWarmup=always\nhistory=true\nrestoreClosedPins=false\npinZoomAnchor=center\nmagnifier=false\nguides=true\nautoAdsorb=false\ntextCaptureCopyOnly=true\ntranslationShowSource=false\ncaptureKeyHints=false\nincludeCursor=true\nquickDirectory=D:\\截图\\RustShot\nhotkey=3\nhotkeyModifiers=3\nhotkeyKey=65\npinHotkey=3\npinHotkeyModifiers=6\npinHotkeyKey=66\ntranslateEndpoint=http://127.0.0.1:8000/v1\ntranslateModel=m\ntranslateTarget=ja\nimageTranslateEngine=baidu\nbaiduAppId=id\nbaiduApiKey=key\n")
    }

    #[test]
    fn preferences_json_round_trip_keeps_every_field() {
        let dir = TestPreferencesDirectory::new();
        let original = customized();
        assert_eq!(original.hotkey, 3);
        assert_eq!(original.pin_hotkey, 3);
        original.save_at(&dir.config()).unwrap();
        let loaded = Preferences::load_at(&dir.config());
        assert_eq!(json_value(&loaded), json_value(&original));
        let text = std::fs::read_to_string(dir.config()).unwrap();
        assert!(text.starts_with("{\n  \"schema_version\": 1,\n  \"theme\": \"dark\""));
        assert!(text.contains(r#""quick_directory": "D:\\截图\\RustShot""#));
    }

    #[test]
    fn api_key_preferences_only_persists_dpapi_and_round_trips_current_user() {
        let dir = TestPreferencesDirectory::new();
        let mut original = Preferences::default();
        original.set_translate_api_key("test-only-local-api-key".into());
        original.save_at(&dir.config()).unwrap();
        let text = std::fs::read_to_string(dir.config()).unwrap();
        assert!(!text.contains("test-only-local-api-key"));
        assert!(text.contains("\"translate_api_key_encrypted\": \"dpapi-v1:"));
        assert!(!text.contains("\"translate_api_key\":"));
        let loaded = Preferences::load_at(&dir.config());
        assert_eq!(loaded.translate_api_key, "test-only-local-api-key");
        assert!(!loaded.translate_api_key_unreadable());
    }

    #[test]
    fn api_key_decryption_failure_preserves_original_until_replaced() {
        let dir = TestPreferencesDirectory::new();
        let text = r#"{"translate_api_key_encrypted":"dpapi-v1:00","future_setting":true}"#;
        std::fs::write(dir.config(), text).unwrap();
        let mut loaded = Preferences::load_at(&dir.config());
        assert!(loaded.translate_api_key.is_empty());
        assert!(loaded.translate_api_key_unreadable());
        assert!(loaded.save_at(&dir.config()).is_err());
        assert_eq!(std::fs::read_to_string(dir.config()).unwrap(), text);
        loaded.set_translate_api_key("replacement-test-key".into());
        loaded.save_at(&dir.config()).unwrap();
        assert_eq!(
            Preferences::load_at(&dir.config()).translate_api_key,
            "replacement-test-key"
        );
    }

    #[test]
    fn api_key_disabling_service_keeps_unreadable_ciphertext() {
        let dir = TestPreferencesDirectory::new();
        std::fs::write(
            dir.config(),
            r#"{"translate_api_key_encrypted":"dpapi-v1:00"}"#,
        )
        .unwrap();
        let mut loaded = Preferences::load_at(&dir.config());
        loaded.translate_endpoint.clear();
        loaded.save_at(&dir.config()).unwrap();
        let reloaded = Preferences::load_at(&dir.config());
        assert!(reloaded.translate_api_key_unreadable());
        assert_eq!(reloaded.translate_api_key_encrypted, "dpapi-v1:00");
        assert!(reloaded.translate_endpoint.is_empty());
    }

    #[test]
    fn api_key_default_service_migration_only_changes_exact_old_pair() {
        let dir = TestPreferencesDirectory::new();
        std::fs::write(dir.config(), r#"{"translate_endpoint":"https://index-translate.bilibili.com/v1","translate_model":"Index-Translate-35B-A3B"}"#).unwrap();
        let migrated = Preferences::load_at(&dir.config());
        assert_eq!(
            migrated.translate_endpoint,
            crate::translate::DEFAULT_ENDPOINT
        );
        assert_eq!(migrated.translate_model, crate::translate::DEFAULT_MODEL);
        assert!(migrated.translate_api_key.is_empty());
        for (endpoint, model) in [
            ("https://custom.test/v1", "Index-Translate-35B-A3B"),
            ("https://index-translate.bilibili.com/v1", "custom"),
            ("", "Index-Translate-35B-A3B"),
        ] {
            std::fs::write(
                dir.config(),
                serde_json::to_string(
                    &serde_json::json!({"translate_endpoint":endpoint,"translate_model":model}),
                )
                .unwrap(),
            )
            .unwrap();
            let kept = Preferences::load_at(&dir.config());
            assert_eq!(kept.translate_endpoint, endpoint);
            assert_eq!(kept.translate_model, model);
        }
    }

    #[test]
    fn preferences_json_invalid_fields_fall_back_individually() {
        let dir = TestPreferencesDirectory::new();
        std::fs::write(
            dir.config(),
            r#"{"theme":"light","language":42,"ocr_profile":"SMALL","history":"yes","guides":true,"quick_directory":"relative","hotkey":3,"hotkey_modifiers":0,"hotkey_key":65,"pin_hotkey":9,"translate_target":"  ja  ","future_setting":1}"#,
        )
        .unwrap();
        let p = Preferences::load_at(&dir.config());
        let defaults = Preferences::default();
        assert_eq!(p.theme, ThemeMode::Light);
        assert_eq!(p.language, defaults.language);
        assert_eq!(p.ocr_profile, OcrProfile::Tiny);
        assert!(!p.history && p.guides);
        assert_eq!(p.quick_dir, defaults.quick_dir);
        assert_eq!(p.hotkey, 5, "裸字符自定义组合不能生效");
        assert_eq!(p.pin_hotkey, 0);
        assert_eq!(p.translate_target, "ja");
        // 手改出的冲突：剪贴板贴图让位。
        std::fs::write(
            dir.config(),
            r#"{"hotkey":5,"pin_hotkey":3,"pin_hotkey_modifiers":0,"pin_hotkey_key":44}"#,
        )
        .unwrap();
        let conflict = Preferences::load_at(&dir.config());
        assert!(!conflict.hotkeys_conflict());
    }

    #[test]
    fn preferences_migrate_legacy_ini_to_json() {
        let dir = TestPreferencesDirectory::new();
        let original = customized();
        let legacy = original.serialize_legacy_ini().unwrap();
        std::fs::write(dir.0.join("settings.ini"), &legacy).unwrap();
        let loaded = Preferences::load_at(&dir.config());
        assert_eq!(json_value(&loaded), json_value(&original));
        assert!(dir.config().exists() && !dir.0.join("settings.ini").exists());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.ini.bak")).unwrap(),
            legacy
        );
        assert_eq!(
            json_value(&Preferences::load_at(&dir.config())),
            json_value(&original)
        );
        // 旧用户已把剪贴板贴图默认键用作截图时，迁移仍沿用旧规则禁用贴图快捷键。
        std::fs::remove_file(dir.config()).unwrap();
        std::fs::write(
            dir.0.join("settings.ini"),
            "hotkey=3\nhotkeyModifiers=1\nhotkeyKey=50\n",
        )
        .unwrap();
        let migrated = Preferences::load_at(&dir.config());
        assert_eq!(migrated.pin_hotkey, 4);
    }
}
