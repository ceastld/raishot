//! 文字样式、按需系统字体目录及 GDI 字体生命周期。
use crate::{annotation::Style, Result};
use std::sync::OnceLock;
use windows::{
    core::PCWSTR,
    Win32::{Foundation::LPARAM, Graphics::Gdi::*},
};
// 与 GPUI 共用现有同步库；锁覆盖原生设备/字形整个生命周期，同一 UI 线程允许嵌套排版。
pub type NativeTextLease = parking_lot::ReentrantMutexGuard<'static, ()>;
static NATIVE_TEXT: parking_lot::ReentrantMutex<()> = parking_lot::ReentrantMutex::new(());
pub fn native_text_lease() -> NativeTextLease {
    NATIVE_TEXT.lock()
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FontFamily(pub usize);
impl FontFamily {
    pub fn name(self) -> &'static str {
        if self.0 == 0 {
            return "Microsoft YaHei UI";
        }
        fonts()
            .get(self.0)
            .map_or("Microsoft YaHei UI", String::as_str)
    }
    pub fn named(name: &str) -> Self {
        Self(
            fonts()
                .iter()
                .position(|s| s.eq_ignore_ascii_case(name))
                .unwrap_or(0),
        )
    }
}
/// 点阵／矢量字体（System、Fixedsys、Modern 等）只有 GDI 能用，DirectWrite 按名加载不到。
static NON_OUTLINE: OnceLock<Vec<String>> = OnceLock::new();
/// 字体能否交给 GPUI 按名加载（菜单里用自身字体预览）。
pub fn outline_font(name: &str) -> bool {
    fonts();
    !NON_OUTLINE
        .get()
        .is_some_and(|names| names.iter().any(|s| s.eq_ignore_ascii_case(name)))
}
pub fn fonts() -> &'static [String] {
    static FONTS: OnceLock<Vec<String>> = OnceLock::new();
    if let Some(fonts) = FONTS.get() {
        return fonts;
    }
    let _native = native_text_lease();
    FONTS.get_or_init(|| {
        let mut names = Vec::<(String, bool)>::new();
        unsafe extern "system" fn visit(
            lf: *const LOGFONTW,
            tm: *const TEXTMETRICW,
            kind: u32,
            data: LPARAM,
        ) -> i32 {
            let face = &(*lf).lfFaceName;
            let end = face.iter().position(|c| *c == 0).unwrap_or(face.len());
            let name = String::from_utf16_lossy(&face[..end]);
            if !name.starts_with('@') && !name.is_empty() {
                // 回调里的 TEXTMETRICW 对 TrueType/OpenType 实为 NEWTEXTMETRICW。
                let flags = (*(tm as *const NEWTEXTMETRICW)).ntmFlags;
                let outline = kind & TRUETYPE_FONTTYPE != 0
                    || flags & (NTM_PS_OPENTYPE | NTM_TT_OPENTYPE) != 0;
                (*(data.0 as *mut Vec<(String, bool)>)).push((name, outline));
            }
            1
        }
        unsafe {
            let dc = CreateCompatibleDC(None);
            if !dc.is_invalid() {
                let lf = LOGFONTW {
                    lfCharSet: DEFAULT_CHARSET,
                    ..Default::default()
                };
                EnumFontFamiliesExW(
                    dc,
                    &lf,
                    Some(visit),
                    LPARAM(&mut names as *mut _ as isize),
                    0,
                );
                let _ = DeleteDC(dc);
            }
        }
        let _ = NON_OUTLINE.set(
            names
                .iter()
                .filter(|(_, outline)| !outline)
                .map(|(name, _)| name.clone())
                .collect(),
        );
        let mut names: Vec<String> = names.into_iter().map(|(name, _)| name).collect();
        names.sort_by_key(|s| s.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let mut ordered = vec!["Microsoft YaHei UI".to_owned()];
        for preferred in ["Segoe UI", "Consolas", "Arial", "SimSun", "Times New Roman"] {
            if let Some(i) = names.iter().position(|s| s.eq_ignore_ascii_case(preferred)) {
                ordered.push(names.remove(i));
            }
        }
        names.retain(|s| !s.eq_ignore_ascii_case("Microsoft YaHei UI"));
        ordered.extend(names);
        ordered
    })
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Alignment {
    #[default]
    Left,
    Center,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub font: FontFamily,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub alignment: Alignment,
    pub outline: bool,
    pub outline_color: u32,
    pub outline_width: f32,
    pub background: bool,
    pub background_color: u32,
    pub background_opacity: f32,
    pub background_radius: f32,
}
impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font: FontFamily::default(),
            italic: false,
            underline: false,
            strike: false,
            alignment: Alignment::Left,
            outline: false,
            outline_color: 0xffffff,
            outline_width: 3.,
            background: false,
            background_color: 0xffffff,
            background_opacity: 0.85,
            background_radius: 4.,
        }
    }
}
impl TextStyle {
    pub fn padding(self) -> f32 {
        if self.background {
            4.
        } else {
            0.
        }
    }
    pub fn updated(mut self, before: Self, after: Self) -> Self {
        macro_rules! fields { ($($f:ident),+) => { $(if before.$f != after.$f { self.$f = after.$f; })+ }; }
        fields!(
            font,
            italic,
            underline,
            strike,
            alignment,
            outline,
            outline_color,
            outline_width,
            background,
            background_color,
            background_opacity,
            background_radius
        );
        self
    }
}
pub fn create_font(style: Style, angle: i32) -> Result<HFONT> {
    let family: Vec<u16> = style
        .text
        .font
        .name()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let font = unsafe {
        CreateFontW(
            -(style.font_size.max(1.).round() as i32),
            0,
            angle,
            angle,
            if style.bold { 700 } else { 400 },
            style.text.italic as u32,
            style.text.underline as u32,
            style.text.strike as u32,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(family.as_ptr()),
        )
    };
    if font.is_invalid() {
        Err("无法创建文字字体".into())
    } else {
        Ok(font)
    }
}
pub struct FontContext {
    pub dc: HDC,
    font: HFONT,
    old: HGDIOBJ,
    _native: NativeTextLease,
}
impl FontContext {
    pub fn new(style: Style) -> Result<Self> {
        let native = native_text_lease();
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return Err("无法创建文字测量设备".into());
            }
            let font = match create_font(style, 0) {
                Ok(f) => f,
                Err(e) => {
                    let _ = DeleteDC(dc);
                    return Err(e);
                }
            };
            let old = SelectObject(dc, font.into());
            Ok(Self {
                dc,
                font,
                old,
                _native: native,
            })
        }
    }
}
impl Drop for FontContext {
    fn drop(&mut self) {
        unsafe {
            let _ = GdiFlush();
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.font.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
