//! 界面语言：中文与英文。文案以 `tr!("中文", "English")` 就地成对书写，
//! 不引入键表；语言来自本机偏好，`system` 按 Windows 显示语言解析。
//! 只影响界面文字，不参与截图像素、导出、配置字段或日志格式。
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LanguageMode {
    #[default]
    System,
    Chinese,
    English,
}
impl LanguageMode {
    pub(crate) fn from_wire(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "chinese" => Self::Chinese,
            "en" | "en-us" | "english" => Self::English,
            _ => Self::System,
        }
    }
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Chinese => "zh-CN",
            Self::English => "en",
        }
    }
    pub(crate) fn resolve(self) -> Language {
        match self {
            Self::Chinese => Language::Chinese,
            Self::English => Language::English,
            Self::System => system_language(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
}

const UNSET: u8 = 0;
const CHINESE: u8 = 1;
const ENGLISH: u8 = 2;
static CURRENT: AtomicU8 = AtomicU8::new(UNSET);

fn encode(language: Language) -> u8 {
    match language {
        Language::Chinese => CHINESE,
        Language::English => ENGLISH,
    }
}

/// QA 可用 `RUSTSHOT_LANGUAGE=zh-CN|en` 固定语言，避免受本机设置影响。
fn override_mode() -> Option<LanguageMode> {
    let value = std::env::var("RUSTSHOT_LANGUAGE").ok()?;
    match LanguageMode::from_wire(&value) {
        LanguageMode::System if !value.trim().eq_ignore_ascii_case("system") => None,
        mode => Some(mode),
    }
}

fn system_language() -> Language {
    // LANG_CHINESE = 0x04；其它显示语言统一使用英文。
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    if id & 0x3ff == 0x04 {
        Language::Chinese
    } else {
        Language::English
    }
}

/// 按偏好设置当前进程语言；返回语言是否发生变化。
pub fn apply(mode: LanguageMode) -> bool {
    // 单元测试会读到本机设置文件；固定中文，断言不受用户选择影响。
    let mode = if cfg!(test) {
        LanguageMode::Chinese
    } else {
        override_mode().unwrap_or(mode)
    };
    let next = encode(mode.resolve());
    CURRENT.swap(next, Ordering::Relaxed) != next
}

pub fn current() -> Language {
    if CURRENT.load(Ordering::Relaxed) == UNSET {
        // 进程首次取文案时读取偏好；GPUI 进程随后由主题轮询保持同步。
        apply(crate::preferences::Preferences::load().language);
    }
    if CURRENT.load(Ordering::Relaxed) == ENGLISH {
        Language::English
    } else {
        Language::Chinese
    }
}

pub fn is_english() -> bool {
    current() == Language::English
}

pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    if is_english() {
        en
    } else {
        zh
    }
}

/// 常量表里的成对文案；使用处再按当前语言取值。
#[derive(Clone, Copy, Debug)]
pub struct Text(pub &'static str, pub &'static str);
impl Text {
    pub fn get(self) -> &'static str {
        tr(self.0, self.1)
    }
}

/// 成对静态文案：`tr!("中文", "English")`，返回 `&'static str`。
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(,)?) => {
        $crate::i18n::tr($zh, $en)
    };
}

/// 成对格式化文案：`trf!("共 {n} 张", "{n} images")`，返回 `String`。
#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_english() {
            format!($en $(, $arg)*)
        } else {
            format!($zh $(, $arg)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_wire_round_trip_and_unknown_defaults_to_system() {
        for mode in [
            LanguageMode::System,
            LanguageMode::Chinese,
            LanguageMode::English,
        ] {
            assert_eq!(LanguageMode::from_wire(mode.wire_name()), mode);
        }
        assert_eq!(LanguageMode::from_wire("fr"), LanguageMode::System);
        assert_eq!(LanguageMode::from_wire(""), LanguageMode::System);
    }
}
