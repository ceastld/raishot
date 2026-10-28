//! 保存格式与文件名的纯模型；默认 PNG 保留透明通道，编码器由 export 负责。
use crate::Result;
use std::path::{Path, PathBuf};

pub(crate) const DATE_FORMATS: [&str; 6] = [
    "yyyyMMdd",
    "yyyy-MM-dd",
    "yyyy.MM.dd",
    "yyMMdd",
    "yyyy年M月d日",
    "MMdd",
];
/// 未保存过规则时按界面语言给出默认模板；已保存的模板保持原样。
pub(crate) fn default_template() -> &'static str {
    tr!(
        "截图_{date}_{time}_{width}x{height}",
        "Screenshot_{date}_{time}_{width}x{height}"
    )
}
const MAX_TEMPLATE_BYTES: usize = 1024;
const MAX_NAME_UNITS: usize = 240;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SaveFormat {
    #[default]
    Png,
    Jpeg,
    Bmp,
    /// 默认无损；图片工具可显式选择有损模式与独立质量。
    Webp,
}
impl SaveFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Bmp => "bmp",
            Self::Webp => "webp",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Bmp => "BMP",
            Self::Webp => "WebP",
        }
    }
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        Self::from_id(path.extension()?.to_str()?)
    }
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "bmp" => Some(Self::Bmp),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            _ => self.extension(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PngCompression {
    #[default]
    Fast,
    Balanced,
    Small,
}
impl PngCompression {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Small => "small",
        }
    }
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        match id {
            "fast" => Some(Self::Fast),
            "balanced" => Some(Self::Balanced),
            "small" => Some(Self::Small),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SaveEncoding {
    pub format: SaveFormat,
    pub jpeg_quality: u8,
    pub png_compression: PngCompression,
    pub webp: crate::image_webp_encode::WebpEncoding,
}
impl Default for SaveEncoding {
    fn default() -> Self {
        Self {
            format: SaveFormat::Png,
            jpeg_quality: 90,
            png_compression: PngCompression::Fast,
            webp: Default::default(),
        }
    }
}
impl SaveEncoding {
    pub(crate) fn validate(self) -> Result<()> {
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err(tr!("JPEG 质量应为 1–100", "JPEG quality must be 1–100").into());
        }
        self.webp.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SaveConfig {
    pub directory: PathBuf,
    pub encoding: SaveEncoding,
    pub filename_template: String,
    pub date_format: String,
}
impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            directory: crate::preferences::Preferences::load().quick_dir,
            encoding: SaveEncoding::default(),
            filename_template: default_template().into(),
            date_format: DATE_FORMATS[0].into(),
        }
    }
}
impl SaveConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_directory(&self.directory)?;
        self.encoding.validate()?;
        if !DATE_FORMATS.contains(&self.date_format.as_str()) {
            return Err(tr!(
                "日期格式应选择支持的日期预设",
                "Choose a supported date preset"
            )
            .into());
        }
        self.suggested_stem_at(100, 100, SaveMoment::EXAMPLE)?;
        Ok(())
    }
    pub(crate) fn suggested_stem(&self, width: u32, height: u32) -> Result<String> {
        self.suggested_stem_at(width, height, SaveMoment::now())
    }
    pub(crate) fn suggested_stem_at(
        &self,
        width: u32,
        height: u32,
        moment: SaveMoment,
    ) -> Result<String> {
        moment.validate()?;
        if !DATE_FORMATS.contains(&self.date_format.as_str()) {
            return Err(tr!(
                "日期格式应选择支持的日期预设",
                "Choose a supported date preset"
            )
            .into());
        }
        let template = self.filename_template.trim();
        if template.is_empty() || self.filename_template.len() > MAX_TEMPLATE_BYTES {
            return Err(tr!(
                "文件名模板不能为空，且不能超过 1024 字节",
                "The file name template must be non-empty and at most 1024 bytes"
            )
            .into());
        }
        if self.filename_template.chars().any(char::is_control) {
            return Err(tr!(
                "文件名模板不能包含控制字符",
                "The file name template can't contain control characters"
            )
            .into());
        }
        let mut rendered = String::new();
        let mut chars = template.chars();
        while let Some(ch) = chars.next() {
            if ch == '{' {
                let mut token = String::new();
                let mut closed = false;
                for ch in chars.by_ref() {
                    if ch == '}' {
                        closed = true;
                        break;
                    }
                    if ch == '{' {
                        return Err(tr!(
                            "文件名模板占位符不能嵌套",
                            "Placeholders in the file name template can't be nested"
                        )
                        .into());
                    }
                    token.push(ch);
                }
                if !closed {
                    return Err(tr!(
                        "文件名模板占位符缺少右花括号",
                        "A placeholder in the file name template is missing its closing brace"
                    )
                    .into());
                }
                let folded = token.to_ascii_lowercase();
                match folded.as_str() {
                    "date" => rendered.push_str(&moment.format(&self.date_format)?),
                    "time" => rendered.push_str(&moment.format("HHmmssfff")?),
                    "width" => rendered.push_str(&width.to_string()),
                    "height" => rendered.push_str(&height.to_string()),
                    _ if folded.starts_with("date:") => {
                        rendered.push_str(&moment.format(&token[5..])?);
                    }
                    _ => {
                        return Err(tr!(
                            "支持 {date}、{date:格式}、{time}、{width}、{height}",
                            "Supported: {date}, {date:format}, {time}, {width}, {height}"
                        )
                        .into())
                    }
                }
            } else {
                if ch == '}' || ch == '/' || ch == '\\' || ch.is_control() {
                    return Err(tr!("文件名模板不能包含路径分隔符、控制字符或未配对花括号", "The file name template can't contain path separators, control characters or unmatched braces").into());
                }
                rendered.push(ch);
            }
        }
        // 日期格式内的 /、: 等允许作为格式符，但展开后只能成为普通文件名。
        let stem: String = rendered
            .chars()
            .map(|ch| if invalid_name_char(ch) { '_' } else { ch })
            .collect();
        let stem = stem.trim().trim_end_matches(['.', ' ']).to_string();
        file_name(&stem, self.encoding.format)?;
        Ok(stem)
    }
}

pub(crate) fn validate_directory(directory: &Path) -> Result<()> {
    let text = directory.to_str().ok_or(tr!(
        "保存目录必须是有效的 Unicode 路径",
        "The save folder must be a valid Unicode path"
    ))?;
    if !directory.is_absolute() || text.len() > 8192 || text.chars().any(char::is_control) {
        return Err(tr!(
            "保存目录必须是绝对路径，且不能包含控制字符或过长路径",
            "The save folder must be an absolute path without control characters and not too long"
        )
        .into());
    }
    Ok(())
}

/// 手工文件名可以含受支持扩展名；最终扩展名始终服从所选格式。
pub(crate) fn file_name(stem: &str, format: SaveFormat) -> Result<String> {
    let name = stem.trim();
    if name.is_empty() || stem.chars().any(invalid_name_char) || name.ends_with('.') {
        return Err(tr!("文件名不能为空或包含路径分隔符、控制字符及 Windows 禁用字符", "The file name can't be empty or contain path separators, control characters or characters Windows forbids").into());
    }
    let stem = if SaveFormat::from_path(Path::new(name)).is_some() {
        Path::new(name)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("")
    } else {
        name
    };
    if stem.is_empty() || stem.ends_with(['.', ' ']) || reserved_name(stem) {
        return Err(tr!(
            "文件名不能使用 Windows 保留设备名或以点、空格结尾",
            "The file name can't be a reserved Windows device name or end with a dot or space"
        )
        .into());
    }
    let name = format!("{stem}.{}", format.extension());
    // 留出同名保存后缀空间，避免路径规划器添加后缀后突破单文件名上限。
    if name.encode_utf16().count() > MAX_NAME_UNITS {
        return Err(tr!(
            "文件名过长，请缩短模板或文件名",
            "The file name is too long; shorten the template or file name"
        )
        .into());
    }
    Ok(name)
}
fn invalid_name_char(ch: char) -> bool {
    ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}
fn reserved_name(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        base.as_str(),
        "." | ".." | "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

/// 无时区依赖的本地时刻值；固定字段便于验证模板，不读取桌面或剪贴板。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SaveMoment {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub millisecond: u16,
}
impl SaveMoment {
    pub(crate) const EXAMPLE: Self = Self {
        year: 2026,
        month: 9,
        day: 4,
        hour: 15,
        minute: 30,
        second: 22,
        millisecond: 123,
    };
    pub(crate) fn now() -> Self {
        let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        Self {
            year: time.wYear,
            month: time.wMonth as u8,
            day: time.wDay as u8,
            hour: time.wHour as u8,
            minute: time.wMinute as u8,
            second: time.wSecond as u8,
            millisecond: time.wMilliseconds,
        }
    }
    fn validate(self) -> Result<()> {
        let leap = self.year.is_multiple_of(4)
            && (!self.year.is_multiple_of(100) || self.year.is_multiple_of(400));
        let days = match self.month {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            _ => 0,
        };
        if self.year == 0
            || self.year > 9999
            || self.day == 0
            || self.day > days
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
            || self.millisecond > 999
        {
            return Err(tr!(
                "文件名日期或时间无效",
                "Invalid date or time in the file name"
            )
            .into());
        }
        Ok(())
    }
    pub(crate) fn format(self, pattern: &str) -> Result<String> {
        self.validate()?;
        if pattern.is_empty()
            || pattern.len() > 256
            || pattern
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '{' | '}'))
        {
            return Err(tr!("日期格式不能为空或包含控制字符，且不能超过 256 字节", "The date format must be non-empty, without control characters, and at most 256 bytes").into());
        }
        let pattern = match pattern {
            "d" => "MM/dd/yyyy",
            "D" => "dddd, dd MMMM yyyy",
            "f" => "dddd, dd MMMM yyyy HH:mm",
            "F" => "dddd, dd MMMM yyyy HH:mm:ss",
            "g" => "MM/dd/yyyy HH:mm",
            "G" => "MM/dd/yyyy HH:mm:ss",
            "m" | "M" => "MMMM dd",
            "s" => "yyyy-MM-dd'T'HH:mm:ss",
            "t" => "HH:mm",
            "T" => "HH:mm:ss",
            "y" | "Y" => "yyyy MMMM",
            _ => pattern,
        };
        let chars: Vec<char> = pattern.chars().collect();
        let mut result = String::new();
        let mut index = 0;
        while index < chars.len() {
            let ch = chars[index];
            if matches!(ch, '\'' | '"') {
                index += 1;
                while index < chars.len() && chars[index] != ch {
                    if chars[index] == '\\' {
                        index += 1;
                        if index == chars.len() {
                            return Err(tr!(
                                "日期格式转义缺少字符",
                                "An escape in the date format is missing its character"
                            )
                            .into());
                        }
                    }
                    result.push(chars[index]);
                    index += 1;
                }
                if index == chars.len() {
                    return Err(
                        tr!("日期格式引号未闭合", "Unclosed quote in the date format").into(),
                    );
                }
                index += 1;
                continue;
            }
            if ch == '\\' {
                index += 1;
                let Some(ch) = chars.get(index) else {
                    return Err(tr!(
                        "日期格式转义缺少字符",
                        "An escape in the date format is missing its character"
                    )
                    .into());
                };
                result.push(*ch);
                index += 1;
                continue;
            }
            if ch == '%' {
                index += 1;
                let Some(ch) = chars.get(index) else {
                    return Err(tr!(
                        "日期格式 % 后缺少格式字符",
                        "Missing format character after % in the date format"
                    )
                    .into());
                };
                self.component(*ch, 1, &mut result)?;
                index += 1;
                continue;
            }
            if ch.is_ascii_alphabetic() {
                let start = index;
                while index < chars.len() && chars[index] == ch {
                    index += 1;
                }
                self.component(ch, index - start, &mut result)?;
            } else {
                if matches!(ch, '{' | '}') {
                    return Err(tr!(
                        "日期格式不能包含花括号",
                        "The date format can't contain braces"
                    )
                    .into());
                }
                result.push(ch);
                index += 1;
            }
        }
        Ok(result)
    }
    fn component(self, ch: char, count: usize, output: &mut String) -> Result<()> {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        const DAYS: [&str; 7] = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ];
        let number = match ch {
            'y' if count <= 4 => Some(if count <= 2 {
                self.year % 100
            } else {
                self.year
            }),
            'M' if count <= 2 => Some(self.month.into()),
            'd' if count <= 2 => Some(self.day.into()),
            'H' if count <= 2 => Some(self.hour.into()),
            'h' if count <= 2 => Some(
                if self.hour.is_multiple_of(12) {
                    12
                } else {
                    self.hour % 12
                }
                .into(),
            ),
            'm' if count <= 2 => Some(self.minute.into()),
            's' if count <= 2 => Some(self.second.into()),
            _ => None,
        };
        if let Some(number) = number {
            output.push_str(&format!("{number:0count$}"));
        } else {
            match ch {
                'M' if count == 3 || count == 4 => {
                    let month = MONTHS[usize::from(self.month - 1)];
                    output.push_str(if count == 3 { &month[..3] } else { month });
                }
                'd' if count == 3 || count == 4 => {
                    // 公历星期：只依赖固定时刻，负时区/夏令时不影响文件名。
                    let y = i32::from(self.year) - i32::from(self.month < 3);
                    let offsets = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
                    let weekday = (y + y / 4 - y / 100
                        + y / 400
                        + offsets[usize::from(self.month - 1)]
                        + i32::from(self.day))
                        % 7;
                    let day = DAYS[weekday as usize];
                    output.push_str(if count == 3 { &day[..3] } else { day });
                }
                'f' | 'F' if count <= 7 => {
                    let fraction = format!("{:03}0000", self.millisecond);
                    let fraction = &fraction[..count];
                    output.push_str(if ch == 'F' {
                        fraction.trim_end_matches('0')
                    } else {
                        fraction
                    });
                }
                't' if count <= 2 => {
                    let period = if self.hour < 12 { "AM" } else { "PM" };
                    output.push_str(if count == 1 { &period[..1] } else { period });
                }
                'g' if count <= 2 => output.push_str("A.D."),
                _ => {
                    return Err(
                        tr!("日期格式无效或不支持；请使用 yyyy_MM_dd_HH_mm_ss 等格式，字面英文请加引号", "Invalid or unsupported date format; use a format such as yyyy_MM_dd_HH_mm_ss and quote literal letters")
                            .into(),
                    )
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn save_encoding_webp_modes_keep_independent_quality_and_validate() {
        use crate::image_webp_encode::{WebpEncoding, WebpMode};
        let mut encoding = SaveEncoding::default();
        assert_eq!(encoding.webp, WebpEncoding::default());
        encoding.webp.quality = 37;
        encoding.webp.mode = WebpMode::Lossy;
        encoding.jpeg_quality = 94;
        assert!(encoding.validate().is_ok());
        encoding.webp.mode = WebpMode::Lossless;
        assert_eq!(encoding.webp.quality, 37);
        assert_eq!(encoding.jpeg_quality, 94);
        for quality in [0, 101] {
            encoding.webp.quality = quality;
            assert!(encoding.validate().is_err());
        }
    }

    #[test]
    fn fixed_template_resolves_presets_custom_dates_and_dimensions() {
        let mut config = SaveConfig::default();
        assert_eq!(config.encoding, SaveEncoding::default());
        assert_eq!(
            config
                .suggested_stem_at(640, 480, SaveMoment::EXAMPLE)
                .unwrap(),
            "截图_20260904_153022123_640x480"
        );
        for (pattern, expected) in DATE_FORMATS.into_iter().zip([
            "20260904",
            "2026-09-04",
            "2026.09.04",
            "260904",
            "2026年9月4日",
            "0904",
        ]) {
            config.date_format = pattern.into();
            config.filename_template = "{DATE}".into();
            assert_eq!(
                config.suggested_stem_at(1, 2, SaveMoment::EXAMPLE).unwrap(),
                expected
            );
        }
        config.filename_template = "{date:yyyy_MM_dd_HH_mm_ss}_{time}_{width}x{height}".into();
        assert_eq!(
            config
                .suggested_stem_at(12, 34, SaveMoment::EXAMPLE)
                .unwrap(),
            "2026_09_04_15_30_22_153022123_12x34"
        );
        assert_eq!(
            SaveMoment::EXAMPLE
                .format("ddd MMM 'capture' HH:mm:ss.fff")
                .unwrap(),
            "Fri Sep capture 15:30:22.123"
        );
        assert_eq!(SaveMoment::EXAMPLE.format("yyyy\\y").unwrap(), "2026y");
    }
    #[test]
    fn invalid_templates_dates_and_names_cannot_escape_directory() {
        let mut config = SaveConfig::default();
        for template in [
            "",
            "../{date}",
            "sub\\{date}",
            "{unknown}",
            "{date:}",
            "{date:YYYY}",
            "{date:yyyy'{oops}' }",
            "{date",
            "{date:yyyy'bad}",
            "NUL",
        ] {
            config.filename_template = template.into();
            assert!(config.validate().is_err(), "{template}");
        }
        let leap = SaveMoment {
            year: 2024,
            month: 2,
            day: 29,
            ..SaveMoment::EXAMPLE
        };
        assert!(leap.format("yyyyMMdd").is_ok());
        assert!(SaveMoment { year: 2023, ..leap }
            .format("yyyyMMdd")
            .is_err());
        for name in [
            "", ".", "..", "CON", "con.txt", "LPT9.png", "COM¹", "a/b", "a\\b", "a:b", "a\n",
            "tail.",
        ] {
            assert!(file_name(name, SaveFormat::Png).is_err(), "{name}");
        }
        assert_eq!(file_name("测试.PNG", SaveFormat::Jpeg).unwrap(), "测试.jpg");
        assert_eq!(file_name("测试.jpeg", SaveFormat::Bmp).unwrap(), "测试.bmp");
        assert_eq!(SaveFormat::from_id("WebP"), Some(SaveFormat::Webp));
        assert_eq!(
            SaveFormat::from_path(Path::new("透明.WEBP")),
            Some(SaveFormat::Webp)
        );
        assert_eq!(
            file_name("测试.PNG", SaveFormat::Webp).unwrap(),
            "测试.webp"
        );
        assert_eq!(file_name("测试.WEBP", SaveFormat::Png).unwrap(), "测试.png");
        assert_eq!(
            file_name("截图.v1", SaveFormat::Png).unwrap(),
            "截图.v1.png"
        );
        assert!(file_name(&"图".repeat(240), SaveFormat::Png).is_err());
        assert!(SaveEncoding {
            jpeg_quality: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(SaveEncoding {
            jpeg_quality: 101,
            ..Default::default()
        }
        .validate()
        .is_err());
    }
}
