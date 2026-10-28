//! 后台 OCR 路由；默认 Rust PP-OCR 管线，Windows OCR 是显式独立提供者。
use crate::{capture::Image, geometry::Rect, Result};
use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};

#[path = "recognition_layout.rs"]
pub mod layout;

#[path = "recognition_table.rs"]
pub mod table;

#[path = "recognition_task.rs"]
pub mod task;

#[path = "recognition_geometry.rs"]
pub mod geometry;
pub use geometry::TextGeometry;

#[path = "recognition_qr.rs"]
mod qr_scan;

#[path = "recognition_graphemes.rs"]
pub mod graphemes;
pub use graphemes::GraphemeSampling;

/// 原图轴对齐框内的水平停点方向；不代表 Unicode 双向排版或精确字符投影。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HorizontalDirection {
    #[default]
    LeftToRight,
    RightToLeft,
}

impl HorizontalDirection {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::LeftToRight => "ltr",
            Self::RightToLeft => "rtl",
        }
    }
}

#[derive(Clone)]
pub struct RecognizedWord {
    pub text: String,
    pub rect: Rect,
    pub line: usize,
    pub horizontal_direction: HorizontalDirection,
    /// 阅读坐标有序的原图裁行投影；词宽来自 CTC 估算，不是精确墨迹字框。
    pub geometry: Option<TextGeometry>,
}
#[derive(Clone)]
pub struct RecognizedLine {
    pub text: String,
    /// 原图中的词框联合范围；缺失或无效词框不冒充完整行的几何证据。
    pub rect: Option<Rect>,
    pub horizontal_direction: HorizontalDirection,
    /// 完整裁行四角及阅读轴；不表示自动版面重排或 Unicode 双向排版。
    pub geometry: Option<TextGeometry>,
    /// 可选的完整来源字素采样证据；缺失或绑定失效时保留原估算路线。
    pub graphemes: Option<GraphemeSampling>,
}
#[derive(Clone)]
pub struct RecognizedText {
    pub text: String,
    pub regions: Vec<Rect>,
    pub words: Vec<RecognizedWord>,
    pub lines: Vec<RecognizedLine>,
}

impl RecognizedText {
    /// 独立复制模型保留原文范围；不覆盖识别原文、词框或停点方向。
    pub fn copy_layout(&self) -> layout::OcrCopyLayout {
        layout::OcrCopyLayout::new(self)
    }

    /// 仅为完整可信短矩阵生成 TSV；不改原文／几何，不适用时保持普通复制路线。
    pub fn copy_table(&self) -> Option<table::OcrTableCopy> {
        table::OcrTableCopy::new(self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OcrProvider {
    #[default]
    Ppocr,
    Windows,
}

impl OcrProvider {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "ppocr" => Ok(Self::Ppocr),
            "windows" => Ok(Self::Windows),
            _ => Err("OCR 提供者须为 ppocr 或 windows".into()),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ppocr => "ppocr",
            Self::Windows => "windows",
        }
    }
}

pub fn recognize_with_provider(image: Image, provider: OcrProvider) -> Result<RecognizedText> {
    match provider {
        OcrProvider::Ppocr => crate::ppocr::recognize(&image),
        OcrProvider::Windows => recognize_windows(image),
    }
}

/// 旧 Tiny CPU 诊断宿主的固定档位入口；产品请求显式携带 profile。
#[cfg(test)]
pub fn recognize_with_provider_cancellable(
    image: Image,
    provider: OcrProvider,
    token: &task::TaskToken,
) -> Result<RecognizedText> {
    recognize_with_profile_cancellable(image, provider, crate::ppocr::OcrProfile::Tiny, token)
}

/// 模型由请求明确捕获；Windows 提供者仍走自己的 WinRT 路线，不读取 GUI 偏好。
pub(crate) fn recognize_with_profile_cancellable(
    image: Image,
    provider: OcrProvider,
    profile: crate::ppocr::OcrProfile,
    token: &task::TaskToken,
) -> Result<RecognizedText> {
    token.checkpoint()?;
    let result = match provider {
        OcrProvider::Ppocr => crate::ppocr::recognize_profile_cancellable(&image, profile, token),
        // WinRT本轮仅边界取消；内部真实错误先保留，不以取消布尔覆盖。
        OcrProvider::Windows => recognize_windows(image),
    }?;
    token.checkpoint()?;
    Ok(result)
}

/// 独立检测只准备所选检测资产，返回 ROI 局部物理框。
pub(crate) fn detect_regions_with_profile_cancellable(
    image: Image,
    profile: crate::ppocr::OcrProfile,
    token: &task::TaskToken,
) -> Result<Vec<Rect>> {
    token.checkpoint()?;
    let result = crate::ppocr::detect_profile_cancellable(&image, profile, token)?;
    token.checkpoint()?;
    Ok(result)
}

fn line_bounds(words: &[RecognizedWord], width: u32, height: u32) -> Option<Rect> {
    let first = words.first()?.rect;
    if !words.iter().all(|word| word.rect.valid(width, height)) {
        return None;
    }
    let mut bounds = first;
    for word in &words[1..] {
        let right = (bounds.x + bounds.width).max(word.rect.x + word.rect.width);
        let bottom = (bounds.y + bounds.height).max(word.rect.y + word.rect.height);
        bounds.x = bounds.x.min(word.rect.x);
        bounds.y = bounds.y.min(word.rect.y);
        bounds.width = right - bounds.x;
        bounds.height = bottom - bounds.y;
    }
    Some(bounds)
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
pub fn recognize_windows(mut image: Image) -> Result<RecognizedText> {
    crate::export::flatten_on_white(&mut image.bgra);
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
    }
    let _apartment = Apartment;
    let maximum = OcrEngine::MaxImageDimension()?;
    let factor = (maximum as f32 / image.width.max(image.height) as f32).min(1.);
    let (width, height) = (
        (image.width as f32 * factor).round().max(1.) as u32,
        (image.height as f32 * factor).round().max(1.) as u32,
    );
    let bytes = if factor < 1. {
        let buffer = image::RgbaImage::from_raw(image.width, image.height, image.bgra)
            .ok_or("识别图像无效")?;
        image::imageops::resize(
            &buffer,
            width,
            height,
            image::imageops::FilterType::Triangle,
        )
        .into_raw()
    } else {
        image.bgra
    };
    let writer = DataWriter::new()?;
    writer.WriteBytes(&bytes)?;
    let bitmap = SoftwareBitmap::CreateWithAlpha(
        BitmapPixelFormat::Bgra8,
        width as i32,
        height as i32,
        BitmapAlphaMode::Ignore,
    )?;
    bitmap.CopyFromBuffer(&writer.DetachBuffer()?)?;
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|_| tr!("Windows 尚未安装可用 OCR 语言包，请在系统语言设置中安装中文或英文识别功能", "No Windows OCR language pack is installed; install Chinese or English recognition in the system language settings"))?;
    let result = engine.RecognizeAsync(&bitmap)?.get()?;
    let mut text = Vec::new();
    let mut regions = Vec::new();
    let mut words = Vec::new();
    let mut lines = Vec::new();
    for line in result.Lines()? {
        text.push(line.Text()?.to_string());
        let first_word = words.len();
        for word in line.Words()? {
            let r = word.BoundingRect()?;
            let rect = Rect {
                x: (r.X / factor).max(0.) as u32,
                y: (r.Y / factor).max(0.) as u32,
                width: (r.Width / factor).ceil() as u32,
                height: (r.Height / factor).ceil() as u32,
            };
            regions.push(rect);
            words.push(RecognizedWord {
                text: word.Text()?.to_string(),
                rect,
                line: text.len() - 1,
                horizontal_direction: HorizontalDirection::default(),
                geometry: None,
            });
        }
        lines.push(RecognizedLine {
            text: text.last().expect("当前 OCR 行已保存").clone(),
            rect: line_bounds(&words[first_word..], image.width, image.height),
            horizontal_direction: HorizontalDirection::default(),
            geometry: None,
            graphemes: None,
        });
    }
    Ok(RecognizedText {
        text: text.join("\n"),
        regions,
        words,
        lines,
    })
}

pub fn self_test() -> Result<()> {
    self_test_with_provider(OcrProvider::default())
}

pub fn self_test_with_provider(provider: OcrProvider) -> Result<()> {
    use crate::annotation::{Annotation, Point, Style, Tool};
    let full = Rect::full(800, 160);
    let mut a = Annotation::new(
        Tool::Text,
        Point { x: 20., y: 20. },
        Style {
            color: 0,
            font_size: 56.,
            ..Style::default()
        },
    );
    a.update(Point { x: 780., y: 140. });
    a.text = "RustShot 2026".into();
    let mut image = crate::raster::empty(full);
    for p in image.bgra.chunks_exact_mut(4) {
        p.copy_from_slice(&[255, 255, 255, 255]);
    }
    let (rect, text) = crate::raster::text(&a, full)?.ok_or("OCR 测试文字无效")?;
    crate::raster::composite(&mut image, full, &text, rect);
    let result = recognize_with_provider(image, provider)?;
    if !result.text.contains("2026") {
        return Err(format!("OCR 测试没有识别数字：{}", result.text).into());
    }
    let multiline = if provider == OcrProvider::Ppocr {
        Some(ppocr_transparent_multiline_self_test()?)
    } else {
        None
    };
    let orientation_cases = if provider == OcrProvider::Ppocr {
        Some(ppocr_orientation_self_test(
            &result,
            multiline.as_ref().ok_or("ppocr 多行自测结果缺失")?,
        )?)
    } else {
        None
    };
    println!(
        "{}",
        serde_json::json!({
            "test": match provider {
                OcrProvider::Ppocr => "ppocr_inprocess",
                OcrProvider::Windows => "windows_offline_ocr",
            },
            "provider": provider.name(),
            "text": result.text,
            "regions": result.regions.len(),
            "transparent_multiline": multiline.map(|result| serde_json::json!({
                "text": result.text,
                "words": result.words.len(),
                "lines": result.lines.len(),
                "regions": result.regions.len(),
            })),
            "orientation_cases": orientation_cases,
            "ok": true,
        })
    );
    Ok(())
}

fn ppocr_transparent_multiline_self_test() -> Result<RecognizedText> {
    let full = Rect::full(800, 260);
    let image = ppocr_self_test_fixture(full, "中文测试 2026\n第二行文字", true)?;
    let result = recognize_with_provider(image, OcrProvider::Ppocr)?;
    validate_ppocr_self_test_result(
        "中英透明多行正向",
        &result,
        full,
        &["中文测试", "第二行文字"],
        &["2026"],
    )?;
    Ok(result)
}

// 自有内存 GDI 文字，不创建窗口、不读取桌面或剪贴板。
fn ppocr_self_test_fixture(full: Rect, content: &str, transparent: bool) -> Result<Image> {
    use crate::annotation::{Annotation, Point, Style, Tool};
    let mut annotation = Annotation::new(
        Tool::Text,
        Point { x: 20., y: 20. },
        Style {
            color: 0,
            font_size: 56.,
            ..Style::default()
        },
    );
    annotation.update(Point {
        x: full.width as f32 - 20.,
        y: full.height as f32 - 20.,
    });
    annotation.text = content.into();
    // 空底图保留透明 alpha；提供者必须在识别副本上合成白底。
    let mut image = crate::raster::empty(full);
    if !transparent {
        image.bgra.fill(255);
    }
    let (rect, text) = crate::raster::text(&annotation, full)?.ok_or("中文 OCR 测试文字无效")?;
    crate::raster::composite(&mut image, full, &text, rect);
    if transparent && !image.bgra.chunks_exact(4).any(|pixel| pixel[3] == 0) {
        return Err("中文 OCR 夹具丢失透明背景".into());
    }
    Ok(image)
}

fn rotate_self_test_image_180(mut image: Image) -> Image {
    let pixels = image.bgra.len() / 4;
    for pixel in 0..pixels / 2 {
        for channel in 0..4 {
            image
                .bgra
                .swap(pixel * 4 + channel, (pixels - 1 - pixel) * 4 + channel);
        }
    }
    image
}

fn validate_ppocr_self_test_result(
    case: &str,
    result: &RecognizedText,
    full: Rect,
    expected_lines: &[&str],
    fragments: &[&str],
) -> Result<()> {
    let valid_geometry = !result.words.is_empty()
        && !result.regions.is_empty()
        && result.words.len() == result.regions.len()
        && result
            .regions
            .iter()
            .all(|rect| rect.valid(full.width, full.height))
        && result.words.iter().all(|word| {
            !word.text.is_empty()
                && word.line < result.lines.len()
                && word.rect.valid(full.width, full.height)
        })
        && result.lines.iter().all(|line| {
            line.rect
                .is_some_and(|rect| rect.valid(full.width, full.height))
        });
    if result.lines.len() != expected_lines.len()
        || !result
            .lines
            .iter()
            .zip(expected_lines)
            .all(|(line, expected)| line.text.contains(*expected))
        || !fragments
            .iter()
            .all(|fragment| result.text.contains(*fragment))
        || !valid_geometry
    {
        return Err(format!(
            "ppocr {case}文字／视觉行／原图词框自测失败：{}",
            result.text
        )
        .into());
    }
    Ok(())
}

fn self_test_word<'a>(result: &'a RecognizedText, text: &str) -> Result<&'a RecognizedWord> {
    result
        .words
        .iter()
        .find(|word| word.text == text)
        .ok_or_else(|| format!("ppocr 方向自测缺少词框 {text}：{}", result.text).into())
}

fn self_test_word_center(word: &RecognizedWord) -> (f64, f64) {
    (
        f64::from(word.rect.x) + f64::from(word.rect.width) / 2.,
        f64::from(word.rect.y) + f64::from(word.rect.height) / 2.,
    )
}

fn validate_flipped_self_test_words(
    before: &RecognizedText,
    after: &RecognizedText,
    original: Rect,
    destination_y: u32,
    texts: &[&str],
) -> Result<()> {
    let mut x_positions = Vec::new();
    for text in texts {
        let source = self_test_word(before, text)?;
        let target = self_test_word(after, text)?;
        let (source_x, source_y) = self_test_word_center(source);
        let (target_x, target_y) = self_test_word_center(target);
        let expected_x = f64::from(original.width) - source_x;
        let expected_y = f64::from(destination_y + original.height) - source_y;
        // CTC 时间步与检测裁行允许采样差异；验证逆投影位置，不要求框逐像素一致。
        let tolerance = (f64::from(source.rect.height.max(target.rect.height)) * 0.75).max(24.);
        if (target_x - expected_x).abs() > tolerance || (target_y - expected_y).abs() > tolerance {
            return Err(format!(
                "ppocr 180° 词框 {text} 未回到原图倒置位置：实际({target_x:.1},{target_y:.1})，预期({expected_x:.1},{expected_y:.1})"
            )
            .into());
        }
        x_positions.push((source_x, target_x));
    }
    if x_positions
        .windows(2)
        .any(|pair| (pair[1].0 - pair[0].0) * (pair[1].1 - pair[0].1) >= 0.)
    {
        return Err("ppocr 180° 纠正后词框没有保留原图中的反向左右顺序".into());
    }
    Ok(())
}

fn ppocr_self_test_case_json(
    case: &str,
    result: &RecognizedText,
    inverse_projection_checked: bool,
) -> serde_json::Value {
    serde_json::json!({
        "case": case,
        "text": result.text,
        "words": result.words.len(),
        "lines": result.lines.len(),
        "regions": result.regions.len(),
        "inverse_projection_checked": inverse_projection_checked,
        "ok": true,
    })
}

fn ppocr_orientation_self_test(
    english: &RecognizedText,
    multiline: &RecognizedText,
) -> Result<Vec<serde_json::Value>> {
    let english_full = Rect::full(800, 160);
    let multiline_full = Rect::full(800, 260);
    validate_ppocr_self_test_result("英文正向", english, english_full, &["RustShot"], &["2026"])?;
    let mut cases = vec![
        ppocr_self_test_case_json("english_upright", english, false),
        ppocr_self_test_case_json("transparent_multiline_upright", multiline, false),
    ];
    for (case, full, content, transparent, expected_lines, source, markers) in [
        (
            "english_180",
            english_full,
            "RustShot 2026",
            false,
            vec!["RustShot"],
            english,
            vec!["RustShot", "2026"],
        ),
        (
            "transparent_multiline_180",
            multiline_full,
            "中文测试 2026\n第二行文字",
            true,
            vec!["第二行文字", "中文测试"],
            multiline,
            vec!["中", "2026"],
        ),
    ] {
        let image =
            rotate_self_test_image_180(ppocr_self_test_fixture(full, content, transparent)?);
        let result = recognize_with_provider(image, OcrProvider::Ppocr)?;
        validate_ppocr_self_test_result(case, &result, full, &expected_lines, &["2026"])?;
        validate_flipped_self_test_words(source, &result, full, 0, &markers)?;
        if transparent {
            validate_flipped_self_test_words(source, &result, full, 0, &["第", "字"])?;
        }
        cases.push(ppocr_self_test_case_json(case, &result, true));
    }
    let short = recognize_with_provider(
        ppocr_self_test_fixture(english_full, "RustShot", true)?,
        OcrProvider::Ppocr,
    )?;
    validate_ppocr_self_test_result("短词正向", &short, english_full, &["RustShot"], &[])?;
    cases.push(ppocr_self_test_case_json(
        "short_word_upright",
        &short,
        false,
    ));
    let short_rotated = recognize_with_provider(
        rotate_self_test_image_180(ppocr_self_test_fixture(english_full, "RustShot", true)?),
        OcrProvider::Ppocr,
    )?;
    validate_ppocr_self_test_result("短词倒置", &short_rotated, english_full, &["RustShot"], &[])?;
    validate_flipped_self_test_words(&short, &short_rotated, english_full, 0, &["RustShot"])?;
    cases.push(ppocr_self_test_case_json(
        "short_word_180",
        &short_rotated,
        true,
    ));

    let lower_text = "中文测试 12345";
    let lower = recognize_with_provider(
        ppocr_self_test_fixture(english_full, lower_text, true)?,
        OcrProvider::Ppocr,
    )?;
    validate_ppocr_self_test_result(
        "混合图下行基准",
        &lower,
        english_full,
        &["中文测试"],
        &["12345"],
    )?;
    let mixed_full = Rect::full(800, 320);
    let mut mixed = crate::raster::empty(mixed_full);
    let upper_image = ppocr_self_test_fixture(english_full, "RustShot 2026", false)?;
    crate::raster::composite(&mut mixed, mixed_full, &upper_image, english_full);
    let lower_image =
        rotate_self_test_image_180(ppocr_self_test_fixture(english_full, lower_text, true)?);
    crate::raster::composite(
        &mut mixed,
        mixed_full,
        &lower_image,
        Rect {
            y: 160,
            ..english_full
        },
    );
    let mixed_result = recognize_with_provider(mixed, OcrProvider::Ppocr)?;
    validate_ppocr_self_test_result(
        "同图正向与倒置",
        &mixed_result,
        mixed_full,
        &["RustShot", "中文测试"],
        &["2026", "12345"],
    )?;
    if mixed_result.lines[0].rect.is_none_or(|rect| rect.y >= 160)
        || mixed_result.lines[1].rect.is_none_or(|rect| rect.y < 160)
    {
        return Err("ppocr 混合方向的视觉行未留在原图对应上下区域".into());
    }
    validate_flipped_self_test_words(&lower, &mixed_result, english_full, 160, &["中", "12345"])?;
    cases.push(ppocr_self_test_case_json(
        "mixed_upright_and_180",
        &mixed_result,
        true,
    ));
    Ok(cases)
}

pub fn qr(image: Image) -> Result<String> {
    let results: Vec<_> = qr_codes(image).into_iter().map(|hit| hit.text).collect();
    if results.is_empty() {
        return Err("未识别到二维码，请扩大选区并保留二维码周围空白".into());
    }
    Ok(results.join("\n\n"))
}

/// 一个二维码的内容与四角（图像像素，左上起顺时针）。
#[derive(Clone, Debug, PartialEq)]
pub struct QrHit {
    pub text: String,
    pub corners: [(f32, f32); 4],
}
impl QrHit {
    pub fn center(&self) -> (f32, f32) {
        let (x, y) = self
            .corners
            .iter()
            .fold((0., 0.), |(x, y), p| (x + p.0, y + p.1));
        (x / 4., y / 4.)
    }
    /// 四边平均长度，用于判断按钮是否盖住整个码。
    pub fn side(&self) -> f32 {
        (0..4)
            .map(|i| {
                let (a, b) = (self.corners[i], self.corners[(i + 1) % 4]);
                ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
            })
            .sum::<f32>()
            / 4.
    }
}

/// 整图兜底只在定位角扫描一无所获、且图不大时做：小图模块只有 1～2 像素时
/// 定位角游程可能被抗锯齿打乱，原样和放大 2 倍各交给 quirc 一次。
const QR_FALLBACK_MAX_PIXELS: u64 = 1_500_000;

pub fn qr_codes(mut image: Image) -> Vec<QrHit> {
    crate::export::flatten_on_white(&mut image.bgra);
    let (width, height) = (image.width as usize, image.height as usize);
    let gray: Vec<u8> = image
        .bgra
        .chunks_exact(4)
        .map(|p| ((p[0] as u32 * 29 + p[1] as u32 * 150 + p[2] as u32 * 77) >> 8) as u8)
        .collect();
    drop(image);
    let hits = qr_scan::scan(&gray, width, height);
    if !hits.is_empty() || width == 0 || (width * height) as u64 > QR_FALLBACK_MAX_PIXELS {
        return hits;
    }
    let hits = qr_scan::quirc(&gray, width, height, 1, (0, 0));
    if !hits.is_empty() {
        return hits;
    }
    let mut doubled = vec![0; width * height * 4];
    for y in 0..height * 2 {
        let row = &gray[y / 2 * width..][..width];
        let out = &mut doubled[y * width * 2..][..width * 2];
        for (x, value) in out.iter_mut().enumerate() {
            *value = row[x / 2];
        }
    }
    qr_scan::quirc(&doubled, width * 2, height * 2, 2, (0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_routes_precancel_before_provider_or_asset_work() {
        let owner = task::CancelOwner::new();
        let token = owner.token();
        owner.cancel().unwrap();
        let image = || Image {
            width: 0,
            height: 0,
            bgra: Vec::new(),
            readback_ms: 0.,
        };
        for profile in [
            crate::ppocr::OcrProfile::Tiny,
            crate::ppocr::OcrProfile::Small,
        ] {
            for provider in [OcrProvider::Ppocr, OcrProvider::Windows] {
                let error = recognize_with_profile_cancellable(image(), provider, profile, &token)
                    .err()
                    .expect("预取消必须在 provider 前返回");
                assert!(task::is_cancelled(error.as_ref()));
            }
            let error = detect_regions_with_profile_cancellable(image(), profile, &token)
                .expect_err("预取消必须在检测资产前返回");
            assert!(task::is_cancelled(error.as_ref()));
        }
    }
    #[test]
    fn provider_defaults_to_ppocr_and_windows_selection_is_explicit() {
        assert_eq!(OcrProvider::default(), OcrProvider::Ppocr);
        assert_eq!(OcrProvider::parse("ppocr").unwrap(), OcrProvider::Ppocr);
        assert_eq!(OcrProvider::parse("windows").unwrap(), OcrProvider::Windows);
        assert_eq!(OcrProvider::Windows.name(), "windows");
        for value in ["", "auto", "winrt", "PPOCR"] {
            assert!(OcrProvider::parse(value).is_err());
        }
    }
    fn word(rect: Rect) -> RecognizedWord {
        RecognizedWord {
            text: String::new(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::default(),
            geometry: None,
        }
    }
    #[test]
    fn ocr_line_geometry_preserves_word_gaps_and_rejects_partial_evidence() {
        let words = [
            word(Rect {
                x: 3,
                y: 10,
                width: 4,
                height: 8,
            }),
            word(Rect {
                x: 20,
                y: 8,
                width: 10,
                height: 12,
            }),
        ];
        assert_eq!(
            line_bounds(&words, 40, 30),
            Some(Rect {
                x: 3,
                y: 8,
                width: 27,
                height: 12
            })
        );
        assert_eq!(line_bounds(&[], 40, 30), None);
        let mut invalid = words;
        invalid[1].rect.x = u32::MAX;
        assert_eq!(line_bounds(&invalid, 40, 30), None);
        invalid[1].rect = Rect {
            x: 20,
            y: 8,
            width: 0,
            height: 12,
        };
        assert_eq!(line_bounds(&invalid, 40, 30), None);
        invalid[1].rect = Rect {
            x: 20,
            y: 8,
            width: 21,
            height: 12,
        };
        assert_eq!(line_bounds(&invalid, 40, 30), None);
    }
    #[test]
    fn qr_decodes_generated_utf8_with_quiet_zone_and_rejects_blank_image() {
        let text = "RustShot 二维码 https://example.com/test?q=1";
        let code = qrcode::QrCode::new(text.as_bytes()).unwrap();
        let modules = code.width();
        let scale = 6;
        let size = (modules + 8) * scale;
        let mut image = Image {
            width: size as u32,
            height: size as u32,
            bgra: vec![255; size * size * 4],
            readback_ms: 0.,
        };
        for y in 0..modules {
            for x in 0..modules {
                if code[(x, y)] == qrcode::Color::Dark {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let i = (((y + 4) * scale + dy) * size + (x + 4) * scale + dx) * 4;
                            image.bgra[i..i + 3].fill(0);
                        }
                    }
                }
            }
        }
        assert_eq!(qr(image).unwrap(), text);
        let blank = Image {
            width: 100,
            height: 100,
            bgra: vec![255; 100 * 100 * 4],
            readback_ms: 0.,
        };
        assert!(qr(blank).is_err());
    }
    /// 在白底上按 (left, top) 画一个码，返回码区中心。
    fn paint_qr(image: &mut Image, text: &str, left: usize, top: usize, scale: usize) -> (f32, f32) {
        let code = qrcode::QrCode::new(text.as_bytes()).unwrap();
        let modules = code.width();
        let width = image.width as usize;
        for y in 0..modules {
            for x in 0..modules {
                if code[(x, y)] == qrcode::Color::Dark {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let i = ((top + y * scale + dy) * width + left + x * scale + dx) * 4;
                            image.bgra[i..i + 3].fill(0);
                        }
                    }
                }
            }
        }
        let half = (modules * scale) as f32 / 2.;
        (left as f32 + half, top as f32 + half)
    }
    fn white(width: usize, height: usize) -> Image {
        Image {
            width: width as u32,
            height: height as u32,
            bgra: vec![255; width * height * 4],
            readback_ms: 0.,
        }
    }
    #[test]
    fn qr_codes_report_each_center_and_retry_tiny_modules_upscaled() {
        let mut image = white(420, 200);
        let a = paint_qr(&mut image, "https://example.com/a", 20, 30, 4);
        let b = paint_qr(&mut image, "第二个码", 260, 40, 3);
        let mut hits = qr_codes(image);
        hits.sort_by(|l, r| l.center().0.total_cmp(&r.center().0));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "https://example.com/a");
        assert_eq!(hits[1].text, "第二个码");
        for (hit, expected) in hits.iter().zip([a, b]) {
            let (x, y) = hit.center();
            assert!((x - expected.0).abs() < 4. && (y - expected.1).abs() < 4., "{hit:?}");
        }

        // 每模块 1 像素：直接识别常失败，放大重试后坐标仍按原图返回。
        let mut tiny = white(60, 60);
        let center = paint_qr(&mut tiny, "tiny", 8, 8, 1);
        let hits = qr_codes(tiny);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "tiny");
        let (x, y) = hits[0].center();
        assert!((x - center.0).abs() < 2. && (y - center.1).abs() < 2., "{:?}", hits[0]);
    }
}
