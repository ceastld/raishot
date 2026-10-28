//! 关闭贴图的有界本机恢复记录；固定 INI 字段，不与截图历史混用。
use crate::{
    capture::Image,
    export,
    pin_model::{
        Adjustments, PinState, PinWindowSizing, Placement, WireProcessingHistory,
        MAX_PIN_WINDOW_SIDE,
    },
    pin_raster_document::WireRasterDocument,
    pin_text_document::WireTextDocument,
    preferences,
    selection_finish::SelectionFinishStyle,
    Result,
};
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
const LIMIT: usize = 20;
const BYTE_LIMIT: u64 = 128 * 1024 * 1024;
const PRESENTATION_METADATA_LIMIT: usize = 512 * 1024;
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PresentationMetadata {
    version: u32,
    #[serde(default)]
    style: Option<SelectionFinishStyle>,
    document: WireRasterDocument,
    #[serde(default)]
    window: Option<WirePresentationWindow>,
    #[serde(
        default,
        deserialize_with = "deserialize_processing_history",
        skip_serializing_if = "Option::is_none"
    )]
    processing_history: Option<WireProcessingHistory>,
}
fn deserialize_processing_history<'de, D>(
    d: D,
) -> std::result::Result<Option<WireProcessingHistory>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}
pub(crate) struct RasterClosedMetadata {
    pub(crate) style: Option<SelectionFinishStyle>,
    pub(crate) document: WireRasterDocument,
    pub(crate) processing_history: Option<WireProcessingHistory>,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextMetadata {
    version: u32,
    document: WireTextDocument,
    window: WirePresentationWindow,
}
/// 关闭前实际客户区矩形与窗口模式；恢复不能按 zoom 再推导一次尺寸。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PresentationWindow {
    pub(crate) placement: Placement,
    pub(crate) sizing: PinWindowSizing,
}
impl PresentationWindow {
    pub(crate) fn validate(self) -> Result<()> {
        let p = self.placement;
        if !(1..=MAX_PIN_WINDOW_SIDE).contains(&p.width)
            || !(1..=MAX_PIN_WINDOW_SIDE).contains(&p.height)
            || p.x.checked_add(p.width as i32).is_none()
            || p.y.checked_add(p.height as i32).is_none()
        {
            return Err("贴图恢复客户区矩形为空、越界或溢出".into());
        }
        Ok(())
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePresentationWindow {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    sizing: String,
}
impl From<PresentationWindow> for WirePresentationWindow {
    fn from(window: PresentationWindow) -> Self {
        let p = window.placement;
        Self {
            x: p.x,
            y: p.y,
            width: p.width,
            height: p.height,
            sizing: match window.sizing {
                PinWindowSizing::General => "general",
                PinWindowSizing::ExactCapture => "exact-capture",
            }
            .into(),
        }
    }
}
impl WirePresentationWindow {
    fn into_model(self) -> Result<PresentationWindow> {
        let window = PresentationWindow {
            placement: Placement {
                x: self.x,
                y: self.y,
                width: self.width,
                height: self.height,
            },
            sizing: match self.sizing.as_str() {
                "general" => PinWindowSizing::General,
                "exact-capture" => PinWindowSizing::ExactCapture,
                _ => return Err("贴图恢复窗口模式无效".into()),
            },
        };
        window.validate()?;
        Ok(window)
    }
}
fn folder() -> PathBuf {
    preferences::data_dir().join("closed-pins")
}
fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for file in std::fs::read_dir(dir)? {
        let f = file?;
        let name = f.file_name().to_string_lossy().into_owned();
        if f.file_type()?.is_file() && name.starts_with("pin-") && name.ends_with(".ini") {
            paths.push(f.path());
        }
    }
    paths.sort();
    paths.reverse();
    Ok(paths)
}
fn encode(s: &PinState, p: Placement) -> String {
    let a = s.adjustments;
    format!(
        "version=1\nx={}\ny={}\nzoom={}\nopacity={}\nlocked={}\ntopmost={}\nshowDropShadow={}\nthumbnail={}\nnormalZoom={}\nturns={}\nflipX={}\nflipY={}\ngrayscale={}\ninvert={}\nbrightness={}\n",
        p.x,
        p.y,
        s.zoom,
        s.opacity,
        s.locked,
        s.topmost,
        s.shadow_enabled,
        s.thumbnail,
        s.normal_zoom,
        a.turns,
        a.flip_x,
        a.flip_y,
        a.grayscale,
        a.invert,
        a.brightness
    )
}
fn decode(text: &str) -> Result<(PinState, (i32, i32))> {
    let fields: std::collections::HashMap<_, _> =
        text.lines().filter_map(|l| l.split_once('=')).collect();
    let value = |key| fields.get(key).copied().ok_or("贴图记录字段缺失");
    if value("version")? != "1" {
        return Err("不支持的贴图记录版本".into());
    }
    let zoom: f32 = value("zoom")?.parse()?;
    let normal_zoom: f32 = value("normalZoom")?.parse()?;
    if !zoom.is_finite() || !normal_zoom.is_finite() || zoom <= 0. || normal_zoom <= 0. {
        return Err("贴图缩放记录无效".into());
    }
    let state = PinState {
        zoom: zoom.clamp(0.0001, 8.),
        normal_zoom: normal_zoom.clamp(0.0001, 8.),
        opacity: value("opacity")?.parse::<u8>()?.max(26),
        locked: value("locked")?.parse()?,
        topmost: value("topmost")?.parse()?,
        shadow_enabled: fields
            .get("showDropShadow")
            .map_or(Ok(crate::pin_model::default_shadow_enabled()), |value| {
                value.parse::<bool>()
            })?,
        thumbnail: value("thumbnail")?.parse()?,
        adjustments: Adjustments {
            turns: value("turns")?.parse::<u8>()? % 4,
            flip_x: value("flipX")?.parse()?,
            flip_y: value("flipY")?.parse()?,
            grayscale: value("grayscale")?.parse()?,
            invert: value("invert")?.parse()?,
            brightness: value("brightness")?.parse::<i16>()?.clamp(-100, 100),
        },
    };
    Ok((state, (value("x")?.parse()?, value("y")?.parse()?)))
}
/// 新历史必须与原始六参数相同；旧无历史文件仍沿 decode 的旧正规化合同。
fn validate_history_adjustments(text: &str, expected: Adjustments) -> Result<()> {
    let mut fields = std::collections::HashMap::new();
    for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
        if matches!(
            key,
            "turns" | "flipX" | "flipY" | "grayscale" | "invert" | "brightness"
        ) && fields.insert(key, value).is_some()
        {
            return Err("图像处理历史的原始参数字段重复".into());
        }
    }
    let value = |key| {
        fields
            .get(key)
            .copied()
            .ok_or("图像处理历史的原始参数字段缺失")
    };
    let original = Adjustments {
        turns: value("turns")?.parse()?,
        flip_x: value("flipX")?.parse()?,
        flip_y: value("flipY")?.parse()?,
        grayscale: value("grayscale")?.parse()?,
        invert: value("invert")?.parse()?,
        brightness: value("brightness")?.parse()?,
    };
    if original.turns > 3 || !(-100..=100).contains(&original.brightness) || original != expected {
        return Err("图像处理历史的原始参数越界或与当前状态不一致".into());
    }
    Ok(())
}
fn prune(dir: &Path) -> Result<()> {
    let mut bytes: u64 = 0;
    for (index, path) in files(dir)?.into_iter().enumerate() {
        let png = path.with_extension("png");
        let Ok(meta) = std::fs::metadata(&png) else {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(png.with_extension("json"));
            continue;
        };
        let metadata_bytes = match std::fs::metadata(path.with_extension("json")) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        let ini_bytes = std::fs::metadata(&path)?.len();
        bytes = bytes
            .checked_add(meta.len())
            .and_then(|n| n.checked_add(metadata_bytes))
            .and_then(|n| n.checked_add(ini_bytes))
            .ok_or("贴图恢复容量统计溢出")?;
        let age = SystemTime::now()
            .duration_since(meta.modified()?)
            .unwrap_or_default();
        if (index >= LIMIT || bytes > BYTE_LIMIT || age > Duration::from_secs(7 * 86400))
            && std::fs::remove_file(&path).is_ok()
        {
            let _ = std::fs::remove_file(png);
            let _ = std::fs::remove_file(path.with_extension("json"));
        }
    }
    Ok(())
}
#[cfg(test)]
fn save_in(dir: &Path, image: Image, s: &PinState, p: Placement) -> Result<()> {
    save_image_with_cleanup(dir, image, s, p, prune)
}
#[cfg(test)]
fn save_image_with_cleanup(
    dir: &Path,
    image: Image,
    s: &PinState,
    p: Placement,
    cleanup: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let key = unique_key()?;
    let mut pending = PendingPresentation {
        ini: dir.join(format!("{key}.ini")),
        committed: false,
    };
    export::save_png(image, &pending.ini.with_extension("png"))?;
    let tmp = pending.ini.with_extension("tmp");
    std::fs::write(&tmp, encode(s, p))?;
    std::fs::rename(tmp, &pending.ini)?;
    pending.committed = true;
    cleanup_committed_record(dir, cleanup);
    Ok(())
}
fn unique_key() -> Result<String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    Ok(format!(
        "pin-{:020}-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros(),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}
#[cfg(test)]
fn save_presentation_in(
    dir: &Path,
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    style: SelectionFinishStyle,
    document: WireRasterDocument,
) -> Result<()> {
    save_raster_document_in(dir, image, s, p, sizing, Some(style), document)
}
#[cfg(test)]
fn save_raster_document_in(
    dir: &Path,
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    style: Option<SelectionFinishStyle>,
    document: WireRasterDocument,
) -> Result<()> {
    save_processed_document_in(
        dir,
        image,
        s,
        p,
        sizing,
        RasterClosedMetadata {
            style,
            document,
            processing_history: None,
        },
    )
}
pub(crate) fn save_processed_document(
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    metadata: RasterClosedMetadata,
) -> Result<()> {
    if !preferences::Preferences::load().pin_restore {
        return Ok(());
    }
    save_processed_document_in(&folder(), image, s, p, sizing, metadata)
}
pub(crate) fn save_capture_document(
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    metadata: RasterClosedMetadata,
    pixels: std::sync::Arc<crate::pin_raster_document::capture_source::CapturePixels>,
) -> Result<()> {
    if !preferences::Preferences::load().pin_restore {
        return Ok(());
    }
    save_processed_document_with_context_in(&folder(), image, s, p, sizing, metadata, Some(pixels))
}
fn save_processed_document_in(
    dir: &Path,
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    metadata: RasterClosedMetadata,
) -> Result<()> {
    save_processed_document_with_context_in(dir, image, s, p, sizing, metadata, None)
}
fn save_processed_document_with_context_in(
    dir: &Path,
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    metadata: RasterClosedMetadata,
    pixels: Option<std::sync::Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
) -> Result<()> {
    let RasterClosedMetadata {
        style,
        document,
        processing_history,
    } = metadata;
    if let Some(style) = style {
        crate::pin_workspace::validate_presentation_style(style)?;
    }
    document.validate()?;
    let window = PresentationWindow {
        placement: p,
        sizing,
    };
    window.validate()?;
    // encode/decode使用同一状态校验，禁止把不可恢复的参数发布成新记录。
    let state = encode(s, p);
    if decode(&state)?.0 != *s {
        return Err("贴图恢复图像处理或窗口状态无效".into());
    }
    if s.adjustments.dimensions(image.width, image.height) != document.storage_size() {
        return Err("贴图恢复原图与处理参数、文档尺寸不匹配".into());
    }
    match (document.capture_source(), pixels.as_ref()) {
        (Some(source), Some(pixels))
            if source.matches(pixels)
                && s.adjustments == Adjustments::default()
                && processing_history.is_none() =>
        {
            pixels.validate()?;
        }
        (None, None) => {}
        _ => return Err("截图关闭恢复缺少完整且匹配的采样上下文".into()),
    }
    if let Some(history) = &processing_history {
        history.validate(s.adjustments, (image.width, image.height), document.size())?;
    }
    let mut metadata = PresentationMetadata {
        version: 1,
        style,
        document,
        window: Some(window.into()),
        processing_history,
    };
    let encoded = loop {
        // 先拒绝语义错误，不能借容量裁剪掩盖坏栈；只裁当前最远处理参数。
        if let Some(history) = &metadata.processing_history {
            history.validate(
                s.adjustments,
                (image.width, image.height),
                metadata.document.size(),
            )?;
        }
        let encoded = serde_json::to_vec(&metadata)?;
        if encoded.len() <= PRESENTATION_METADATA_LIMIT {
            break encoded;
        }
        if !metadata
            .processing_history
            .as_mut()
            .is_some_and(WireProcessingHistory::trim_oldest_history)
        {
            return Err("贴图恢复外观和文档元数据超过容量限制".into());
        }
    };
    match pixels {
        Some(pixels) => save_metadata_with_context_cleanup(
            dir,
            image,
            &state,
            &encoded,
            "presentation",
            prune,
            Some((
                metadata
                    .document
                    .capture_source()
                    .ok_or("截图来源元数据缺失")?,
                pixels.as_ref(),
            )),
        ),
        None => save_metadata_in(dir, image, &state, &encoded, "presentation"),
    }
}
/// 纯色来源的独立关闭事务；不以 PNG 或 annotations 重造文字历史。
pub(crate) fn save_text_document(
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    document: WireTextDocument,
) -> Result<()> {
    if !preferences::Preferences::load().pin_restore {
        return Ok(());
    }
    save_text_document_in(&folder(), image, s, p, sizing, document)
}
fn save_text_document_in(
    dir: &Path,
    image: Image,
    s: &PinState,
    p: Placement,
    sizing: PinWindowSizing,
    document: WireTextDocument,
) -> Result<()> {
    document.validate()?;
    if s.adjustments != Adjustments::default()
        || (image.width, image.height) != (document.width, document.height)
    {
        return Err("纯文字恢复来源尺寸或图像参数不匹配".into());
    }
    let window = PresentationWindow {
        placement: p,
        sizing,
    };
    window.validate()?;
    let state = encode(s, p);
    if decode(&state)?.0 != *s {
        return Err("文字贴图恢复窗口状态无效".into());
    }
    let metadata = serde_json::to_vec(&TextMetadata {
        version: 1,
        document,
        window: window.into(),
    })?;
    if metadata.len() > PRESENTATION_METADATA_LIMIT {
        return Err("纯文字贴图文档元数据超过容量限制".into());
    }
    save_metadata_in(dir, image, &state, &metadata, "textDocument")
}
fn save_metadata_in(
    dir: &Path,
    image: Image,
    state: &str,
    metadata: &[u8],
    marker: &str,
) -> Result<()> {
    save_metadata_with_cleanup(dir, image, state, metadata, marker, prune)
}
fn save_metadata_with_cleanup(
    dir: &Path,
    image: Image,
    state: &str,
    metadata: &[u8],
    marker: &str,
    cleanup: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    save_metadata_with_context_cleanup(dir, image, state, metadata, marker, cleanup, None)
}
fn save_metadata_with_context_cleanup(
    dir: &Path,
    image: Image,
    state: &str,
    metadata: &[u8],
    marker: &str,
    cleanup: impl FnOnce(&Path) -> Result<()>,
    source: Option<(
        &crate::pin_raster_document::capture_source::CaptureSourceMetadata,
        &crate::pin_raster_document::capture_source::CapturePixels,
    )>,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let key = unique_key()?;
    let ini = dir.join(format!("{key}.ini"));
    let mut pending = PendingPresentation {
        ini,
        committed: false,
    };
    if let Some((metadata, pixels)) = source {
        crate::pin_raster_document::capture_source::resource::save_png_pixels_with_context(
            &image,
            metadata,
            pixels,
            &pending.ini.with_extension("png"),
        )?;
    } else {
        export::save_png(image, &pending.ini.with_extension("png"))?;
    }
    let mut meta = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pending.ini.with_extension("json"))?;
    meta.write_all(metadata)?;
    meta.sync_all()?;
    drop(meta);
    let tmp = pending.ini.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(format!("{state}{marker}=1\n").as_bytes())?;
    file.sync_all()?;
    drop(file);
    let bytes = std::fs::metadata(pending.ini.with_extension("png"))?
        .len()
        .checked_add(metadata.len() as u64)
        .and_then(|n| n.checked_add(state.len() as u64 + marker.len() as u64 + 3))
        .ok_or("贴图恢复容量统计溢出")?;
    if bytes > BYTE_LIMIT {
        return Err("单张贴图原图与文档超过关闭恢复容量限制".into());
    }
    std::fs::rename(tmp, &pending.ini)?;
    pending.committed = true;
    cleanup_committed_record(dir, cleanup);
    Ok(())
}
/// INI 发布后只清理旧记录；清理失败不能再表示本次保存失败。
fn cleanup_committed_record(dir: &Path, cleanup: impl FnOnce(&Path) -> Result<()>) {
    if let Err(error) = cleanup(dir) {
        eprintln!("贴图恢复记录已保存，旧记录清理失败：{error}");
    }
}
struct PendingPresentation {
    ini: PathBuf,
    committed: bool,
}
impl Drop for PendingPresentation {
    fn drop(&mut self) {
        if !self.committed {
            for extension in ["ini", "png", "json", "tmp"] {
                let _ = std::fs::remove_file(self.ini.with_extension(extension));
            }
        }
    }
}
pub struct Restored {
    pub image: Option<Image>,
    pub state: PinState,
    pub origin: (i32, i32),
    pub(crate) presentation_style: Option<SelectionFinishStyle>,
    pub(crate) raster_document: Option<WireRasterDocument>,
    pub(crate) text_document: Option<WireTextDocument>,
    pub(crate) processing_history: Option<WireProcessingHistory>,
    pub(crate) presentation_window: Option<PresentationWindow>,
    pub(crate) capture_pixels:
        Option<std::sync::Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
    path: PathBuf,
    lease: Option<std::fs::File>,
}
impl Restored {
    pub fn commit(mut self) -> Result<()> {
        // 独占记录直到创建窗口成功；进程意外退出时锁自动释放，记录仍可恢复。
        let file = self.lease.as_mut().expect("restore lease");
        file.set_len(0)?;
        file.write_all(b"consumed=1\n")?;
        file.sync_data()?;
        self.lease.take();
        std::fs::remove_file(&self.path)?;
        let _ = std::fs::remove_file(self.path.with_extension("png"));
        let _ = std::fs::remove_file(self.path.with_extension("json"));
        Ok(())
    }
}
fn restore_in(dir: &Path) -> Result<Restored> {
    prune(dir)?;
    for path in files(dir)? {
        let Ok(mut lease) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
        else {
            continue;
        };
        let mut text = String::new();
        lease.read_to_string(&mut text)?;
        if text == "consumed=1\n" {
            continue;
        }
        let (state, origin) = decode(&text)?;
        let image = export::read_image(&path.with_extension("png"))?;
        let text_version = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .find(|(key, _)| *key == "textDocument")
            .map(|(_, version)| version);
        if text_version.is_some() && text.lines().any(|line| line.starts_with("presentation=")) {
            return Err("贴图恢复记录不能混合文字和图片文档".into());
        }
        let (text_document, text_window) = if let Some(version) = text_version {
            if version != "1" {
                return Err("不支持的文字贴图恢复版本".into());
            }
            let metadata: TextMetadata = serde_json::from_slice(&read_metadata(&path)?)?;
            if metadata.version != 1 {
                return Err("不支持的文字贴图文档版本".into());
            }
            metadata.document.validate()?;
            if state.adjustments != Adjustments::default()
                || (image.width, image.height)
                    != (metadata.document.width, metadata.document.height)
            {
                return Err("文字贴图恢复来源尺寸或图像参数不匹配".into());
            }
            let window = metadata.window.into_model()?;
            if (window.placement.x, window.placement.y) != origin {
                return Err("文字贴图恢复客户区原点与状态不一致".into());
            }
            (Some(metadata.document), Some(window))
        } else {
            (None, None)
        };
        let (presentation_style, raster_document, presentation_window, processing_history) =
            if let Some((_, version)) = text
                .lines()
                .filter_map(|line| line.split_once('='))
                .find(|(key, _)| *key == "presentation")
            {
                if version != "1" {
                    return Err("不支持的贴图恢复外观版本".into());
                }
                let encoded = read_metadata(&path)?;
                let metadata: PresentationMetadata = serde_json::from_slice(&encoded)?;
                if metadata.version != 1 {
                    return Err("不支持的贴图恢复文档版本".into());
                }
                if let Some(style) = metadata.style {
                    crate::pin_workspace::validate_presentation_style(style)?;
                }
                metadata.document.validate()?;
                if state.adjustments.dimensions(image.width, image.height)
                    != metadata.document.storage_size()
                {
                    return Err("贴图恢复原图与处理参数、文档尺寸不匹配".into());
                }
                if let Some(history) = &metadata.processing_history {
                    validate_history_adjustments(&text, state.adjustments)?;
                    history.validate(
                        state.adjustments,
                        (image.width, image.height),
                        metadata.document.size(),
                    )?;
                }
                let window = metadata
                    .window
                    .map(WirePresentationWindow::into_model)
                    .transpose()?;
                if window.is_some_and(|w| (w.placement.x, w.placement.y) != origin) {
                    return Err("贴图恢复客户区原点与状态记录不一致".into());
                }
                (
                    metadata.style,
                    Some(metadata.document),
                    window,
                    metadata.processing_history,
                )
            } else {
                (None, None, None, None)
            };
        let capture_pixels = raster_document
            .as_ref()
            .and_then(|document| document.capture_source())
            .map(|source| {
                if state.adjustments != Adjustments::default() || processing_history.is_some() {
                    return Err("截图完整来源不能混入旧图像处理参数".into());
                }
                crate::pin_raster_document::capture_source::resource::load_context(
                    &path.with_extension("png"),
                    source,
                    &image,
                )
            })
            .transpose()?;
        // commit 从头写消费标记。
        std::io::Seek::rewind(&mut lease)?;
        return Ok(Restored {
            image: Some(image),
            state,
            origin,
            presentation_style,
            raster_document,
            text_document,
            processing_history,
            presentation_window: text_window.or(presentation_window),
            capture_pixels,
            path,
            lease: Some(lease),
        });
    }
    Err("没有可恢复的已关闭贴图".into())
}
fn read_metadata(path: &Path) -> Result<Vec<u8>> {
    let json = path.with_extension("json");
    let meta = std::fs::symlink_metadata(&json)?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.len() > PRESENTATION_METADATA_LIMIT as u64
    {
        return Err("贴图恢复元数据必须是有界普通文件".into());
    }
    let mut encoded = Vec::new();
    std::fs::File::open(&json)?
        .take(PRESENTATION_METADATA_LIMIT as u64 + 1)
        .read_to_end(&mut encoded)?;
    if encoded.len() > PRESENTATION_METADATA_LIMIT {
        return Err("贴图恢复元数据读取超过容量限制".into());
    }
    Ok(encoded)
}
pub fn restore() -> Result<Restored> {
    restore_in(&folder())
}
pub fn clear() -> Result<()> {
    for path in files(&folder())? {
        std::fs::remove_file(&path)?;
        let _ = std::fs::remove_file(path.with_extension("png"));
        let _ = std::fs::remove_file(path.with_extension("json"));
    }
    Ok(())
}
#[cfg(test)]
#[path = "pin_mixed_text_host_store_tests.rs"]
mod pin_mixed_text_host_store_tests;

#[cfg(test)]
#[path = "pin_capture_closed_store_tests.rs"]
mod capture_closed_tests;
#[cfg(test)]
#[path = "pin_undo_store_tests.rs"]
mod pin_undo_store_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn presentation_image() -> Image {
        Image {
            width: 9,
            height: 7,
            bgra: [10, 30, 50, 123].repeat(9 * 7),
            readback_ms: 0.,
        }
    }
    #[test]
    fn closed_presentation_roundtrips_raw_processing_and_sidecar_without_double_baking() {
        let dir = PathBuf::from(format!(
            "target/qa/pin-store-presentation-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = PinState {
            zoom: 1.25,
            adjustments: Adjustments {
                turns: 1,
                brightness: 20,
                flip_x: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let placement = Placement {
            x: -1933,
            y: -711,
            width: 24,
            height: 27,
        };
        let style = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 3.,
            shadow_enabled: true,
            border_enabled: true,
            ..Default::default()
        };
        let document = WireRasterDocument::empty((7, 9)).unwrap();
        save_presentation_in(
            &dir,
            presentation_image(),
            &state,
            placement,
            PinWindowSizing::General,
            style,
            document.clone(),
        )
        .unwrap();
        let paths = files(&dir).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].with_extension("json").exists());
        {
            let restored = restore_in(&dir).unwrap();
            assert_eq!(restored.state, state);
            assert_eq!(restored.origin, (-1933, -711));
            assert_eq!(restored.presentation_style, Some(style));
            assert_eq!(restored.raster_document, Some(document));
            assert_eq!(
                restored.presentation_window,
                Some(PresentationWindow {
                    placement,
                    sizing: PinWindowSizing::General,
                })
            );
            assert_eq!(
                restored.image.as_ref().unwrap().bgra,
                presentation_image().bgra
            );
            assert_eq!(
                restored
                    .state
                    .adjustments
                    .apply(restored.image.as_ref().unwrap())
                    .unwrap()
                    .bgra[..4],
                [61, 81, 101, 123]
            );
        }
        // 放弃恢复不消费；成功创建宿主后commit清除三件资源。
        let restored = restore_in(&dir).unwrap();
        restored.commit().unwrap();
        assert!(!paths[0].exists());
        assert!(!paths[0].with_extension("png").exists());
        assert!(!paths[0].with_extension("json").exists());
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn closed_presentation_missing_or_invalid_sidecar_keeps_record_for_retry() {
        let dir = PathBuf::from(format!(
            "target/qa/pin-store-presentation-failure-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let placement = Placement {
            x: -19,
            y: 13,
            width: 9,
            height: 7,
        };
        save_presentation_in(
            &dir,
            presentation_image(),
            &PinState::default(),
            placement,
            PinWindowSizing::ExactCapture,
            SelectionFinishStyle::default(),
            WireRasterDocument::empty((9, 7)).unwrap(),
        )
        .unwrap();
        let path = files(&dir).unwrap().remove(0);
        let json = path.with_extension("json");
        let original = std::fs::read(&json).unwrap();
        std::fs::write(&json, b"{}").unwrap();
        assert!(restore_in(&dir).is_err());
        assert!(path.exists());
        assert!(path.with_extension("png").exists());
        std::fs::write(&json, &original).unwrap();
        let restored = restore_in(&dir).unwrap();
        drop(restored);
        std::fs::remove_file(&json).unwrap();
        assert!(restore_in(&dir).is_err());
        std::fs::write(&json, original).unwrap();
        restore_in(&dir).unwrap().commit().unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn closed_legacy_records_have_no_presentation_metadata() {
        let dir = PathBuf::from(format!(
            "target/qa/pin-store-legacy-presentation-{}",
            std::process::id()
        ));
        save_in(
            &dir,
            presentation_image(),
            &PinState::default(),
            Placement {
                x: 3,
                y: 5,
                width: 9,
                height: 7,
            },
        )
        .unwrap();
        let restored = restore_in(&dir).unwrap();
        assert!(restored.presentation_style.is_none());
        assert!(restored.raster_document.is_none());
        assert!(restored.presentation_window.is_none());
        restored.commit().unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn closed_presentation_retains_exact_small_client_and_styled_content_offset() {
        let dir = PathBuf::from(format!(
            "target/qa/pin-store-small-window-{}",
            std::process::id()
        ));
        let content = (10, 10);
        for (index, style, sizing) in [
            (
                0,
                SelectionFinishStyle::default(),
                PinWindowSizing::ExactCapture,
            ),
            (
                1,
                SelectionFinishStyle {
                    shadow_enabled: true,
                    border_enabled: true,
                    border_thickness: 1.25,
                    corner_enabled: true,
                    corner_radius: 3.,
                    ..Default::default()
                },
                PinWindowSizing::ExactCapture,
            ),
            (2, SelectionFinishStyle::default(), PinWindowSizing::General),
        ] {
            let outer = crate::pin_presentation_host::window_size(content, Some(style)).unwrap();
            let dimensions = if sizing == PinWindowSizing::General {
                (24, 24)
            } else {
                outer
            };
            let placement = Placement {
                x: -1923 - index,
                y: -713,
                width: dimensions.0,
                height: dimensions.1,
            };
            let mut state = PinState::default();
            if index == 1 {
                // 倍率与真实客户区分别保存；混合 DPI/手动调整不能要求重新 size() 匹配。
                state.zoom = 1.375;
            }
            let before =
                crate::pin_presentation_host::content_viewport(content, dimensions, Some(style));
            save_presentation_in(
                &dir,
                Image {
                    width: content.0,
                    height: content.1,
                    bgra: [11, 23, 47, 97].repeat(100),
                    readback_ms: 0.,
                },
                &state,
                placement,
                sizing,
                style,
                WireRasterDocument::empty(content).unwrap(),
            )
            .unwrap();
            let restored = restore_in(&dir).unwrap();
            let window = restored.presentation_window.unwrap();
            assert_eq!(window.placement, placement);
            assert_eq!(window.sizing, sizing);
            assert_eq!(restored.state, state);
            let after = crate::pin_presentation_host::content_viewport(
                content,
                (window.placement.width, window.placement.height),
                restored.presentation_style,
            );
            assert_eq!(before.zoom, after.zoom);
            assert_eq!(before.offset, after.offset);
            if index == 0 {
                assert_eq!(dimensions, (10, 10));
                assert_eq!(after.zoom, 1.);
                assert_eq!(after.offset, crate::annotation::Point { x: 0., y: 0. });
            } else if index == 1 {
                assert!(after.offset.x > 0. && after.offset.y > 0.);
            }
            restored.commit().unwrap();
        }
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn closed_presentation_v1_without_window_keeps_prior_restore_route() {
        let dir = PathBuf::from(format!(
            "target/qa/pin-store-old-presentation-window-{}",
            std::process::id()
        ));
        save_presentation_in(
            &dir,
            presentation_image(),
            &PinState::default(),
            Placement {
                x: 3,
                y: 5,
                width: 9,
                height: 7,
            },
            PinWindowSizing::ExactCapture,
            SelectionFinishStyle::default(),
            WireRasterDocument::empty((9, 7)).unwrap(),
        )
        .unwrap();
        let json = files(&dir).unwrap()[0].with_extension("json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        metadata.as_object_mut().unwrap().remove("window");
        std::fs::write(&json, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let restored = restore_in(&dir).unwrap();
        assert!(restored.presentation_style.is_some());
        assert!(restored.raster_document.is_some());
        assert!(restored.presentation_window.is_none());
        restored.commit().unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn closed_presentation_window_rejects_empty_oversize_overflow_and_unknown_mode() {
        let valid = PresentationWindow {
            placement: Placement {
                x: -1923,
                y: -713,
                width: 10,
                height: 10,
            },
            sizing: PinWindowSizing::ExactCapture,
        };
        valid.validate().unwrap();
        for placement in [
            Placement {
                width: 0,
                ..valid.placement
            },
            Placement {
                height: MAX_PIN_WINDOW_SIDE + 1,
                ..valid.placement
            },
            Placement {
                x: i32::MAX - 9,
                ..valid.placement
            },
            Placement {
                y: i32::MAX - 9,
                ..valid.placement
            },
        ] {
            assert!(PresentationWindow { placement, ..valid }
                .validate()
                .is_err());
        }
        let mut wire = WirePresentationWindow::from(valid);
        wire.sizing = "unknown".into();
        assert!(wire.into_model().is_err());
    }
    #[test]
    fn pin_shadow_state_roundtrips_and_legacy_closed_records_default_enabled() {
        let placement = Placement {
            x: -1921,
            y: -701,
            width: 22,
            height: 100,
        };
        for shadow_enabled in [false, true] {
            let state = PinState {
                shadow_enabled,
                ..Default::default()
            };
            let encoded = encode(&state, placement);
            let (restored, origin) = decode(&encoded).unwrap();
            assert_eq!(restored, state);
            assert_eq!(origin, (placement.x, placement.y));
            let legacy = encoded
                .lines()
                .filter(|line| !line.starts_with("showDropShadow="))
                .collect::<Vec<_>>()
                .join("\n");
            let (restored, _) = decode(&legacy).unwrap();
            assert!(restored.shadow_enabled);
            assert_eq!(restored.adjustments, state.adjustments);
            assert!(decode(&encoded.replace(
                &format!("showDropShadow={shadow_enabled}"),
                "showDropShadow=invalid"
            ))
            .is_err());
        }
    }
    #[test]
    fn pin_restore_roundtrips_source_processing_and_claim_rollback() {
        let dir = PathBuf::from(format!("target/qa/pin-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = || Image {
            width: 2,
            height: 1,
            bgra: vec![10, 20, 30, 128, 80, 90, 100, 255],
            readback_ms: 0.,
        };
        let state = PinState {
            zoom: 0.75,
            locked: true,
            adjustments: Adjustments {
                turns: 1,
                flip_x: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let p = Placement {
            x: -600,
            y: 70,
            width: 20,
            height: 30,
        };
        save_in(&dir, image(), &state, p).unwrap();
        {
            let pin = restore_in(&dir).unwrap();
            assert_eq!(pin.origin, (-600, 70));
            assert_eq!(pin.state.adjustments, state.adjustments);
            assert_eq!(pin.image.as_ref().unwrap().bgra, image().bgra);
        }
        let pin = restore_in(&dir).unwrap();
        pin.commit().unwrap();
        assert!(files(&dir).unwrap().is_empty());
        std::fs::remove_dir(&dir).unwrap();
        assert!(decode("version=1\nzoom=NaN\nnormalZoom=1").is_err());
    }
}

#[cfg(test)]
mod text_history_host_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        editor::Editor,
        geometry::Rect,
    };
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/qa")
                .join(format!("text-host-{}", unique_key().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    fn image() -> Image {
        Image {
            width: 2,
            height: 2,
            bgra: [255; 4].repeat(4),
            readback_ms: 0.,
        }
    }
    fn placement() -> Placement {
        Placement {
            x: -1933,
            y: -711,
            width: 24,
            height: 24,
        }
    }
    fn empty_current() -> (WireTextDocument, u64, u64) {
        let full = Rect::full(2, 2);
        let mut editor = Editor::new(full, Some(full));
        let text = |body: &str| {
            let mut a = Annotation::new(
                Tool::Text,
                Point { x: 0., y: 0. },
                Style {
                    color: crate::pin_text_source::colors(true).1,
                    ..Default::default()
                },
            );
            a.update(Point { x: 2., y: 2. });
            a.text = body.into();
            a
        };
        let first = editor.document.insert(text("已清除\t e\u{301}\r\n🙂"));
        editor.document.clear();
        let second = editor.document.insert(text("可重做"));
        editor.document.undo();
        (
            WireTextDocument::try_from_text_editor(&editor, (2, 2), true).unwrap(),
            first,
            second,
        )
    }
    #[test]
    fn closed_text_record_restores_empty_current_both_stacks_and_consumes_only_after_install() {
        let dir = TestDir::new();
        let (document, first, second) = empty_current();
        save_text_document_in(
            &dir.0,
            image(),
            &PinState::default(),
            placement(),
            PinWindowSizing::General,
            document.clone(),
        )
        .unwrap();
        let path = files(&dir.0).unwrap().remove(0);
        let restored = restore_in(&dir.0).unwrap();
        assert!(restored.raster_document.is_none() && restored.presentation_style.is_none());
        assert_eq!(restored.text_document, Some(document.clone()));
        assert_eq!(restored.presentation_window.unwrap().placement, placement());
        let wrong = Rect::full(3, 2);
        assert!(document
            .restore_into(&mut Editor::new(wrong, Some(wrong)), false)
            .is_err());
        drop(restored);
        assert!(path.exists(), "安装失败不消费记录");
        let restored = restore_in(&dir.0).unwrap();
        let full = Rect::full(2, 2);
        let mut editor = Editor::new(full, Some(full));
        restored
            .text_document
            .as_ref()
            .unwrap()
            .restore_into(&mut editor, false)
            .unwrap();
        assert!(editor.document.items().is_empty());
        assert!(editor.document.can_undo() && editor.document.can_redo());
        editor.document.redo();
        assert_eq!(editor.document.items()[0].id, second);
        assert_eq!(editor.document.items()[0].annotation.text, "可重做");
        assert_eq!(
            editor.document.items()[0].annotation.style.color,
            crate::pin_text_source::colors(false).1
        );
        editor.document.undo();
        editor.document.undo();
        assert_eq!(editor.document.items()[0].id, first);
        assert_eq!(
            editor.document.items()[0].annotation.text,
            "已清除\t e\u{301}\r\n🙂"
        );
        restored.commit().unwrap();
        assert!(!path.exists());
        assert!(!path.with_extension("png").exists());
        assert!(!path.with_extension("json").exists());
    }
    #[test]
    fn closed_text_encode_or_sidecar_failure_preserves_previous_record_and_retry() {
        let dir = TestDir::new();
        let (document, _, _) = empty_current();
        save_text_document_in(
            &dir.0,
            image(),
            &PinState::default(),
            placement(),
            PinWindowSizing::General,
            document.clone(),
        )
        .unwrap();
        let path = files(&dir.0).unwrap().remove(0);
        let ini = std::fs::read(&path).unwrap();
        let png = std::fs::read(path.with_extension("png")).unwrap();
        let json = std::fs::read(path.with_extension("json")).unwrap();
        let mut bad_image = image();
        bad_image.bgra.pop();
        assert!(save_text_document_in(
            &dir.0,
            bad_image,
            &PinState::default(),
            placement(),
            PinWindowSizing::General,
            document
        )
        .is_err());
        assert_eq!(files(&dir.0).unwrap(), vec![path.clone()]);
        assert_eq!(std::fs::read(&path).unwrap(), ini);
        assert_eq!(std::fs::read(path.with_extension("png")).unwrap(), png);
        assert_eq!(std::fs::read(path.with_extension("json")).unwrap(), json);
        let entries = std::fs::read_dir(&dir.0).unwrap().count();
        assert_eq!(entries, 3, "编码失败清理本次文件，不发布降级记录");
        std::fs::write(path.with_extension("json"), b"{}").unwrap();
        assert!(restore_in(&dir.0).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), ini);
        std::fs::write(path.with_extension("json"), json).unwrap();
        restore_in(&dir.0).unwrap().commit().unwrap();
        assert!(files(&dir.0).unwrap().is_empty());

        let (document, _, _) = empty_current();
        let state = encode(&PinState::default(), placement());
        let metadata = serde_json::to_vec(&TextMetadata {
            version: 1,
            document: document.clone(),
            window: PresentationWindow {
                placement: placement(),
                sizing: PinWindowSizing::General,
            }
            .into(),
        })
        .unwrap();
        let cleanup_called = std::cell::Cell::new(false);
        // 真实事务先发布三件，再在实际清理边界注入失败；不是模拟保存算法。
        save_metadata_with_cleanup(&dir.0, image(), &state, &metadata, "textDocument", |dir| {
            cleanup_called.set(true);
            let paths = files(dir)?;
            assert_eq!(paths.len(), 1);
            assert_eq!(std::fs::read(paths[0].with_extension("json"))?, metadata);
            Err("注入已提交后的旧记录清理错误".into())
        })
        .unwrap();
        assert!(cleanup_called.get());
        let restored = restore_in(&dir.0).unwrap();
        assert_eq!(restored.text_document, Some(document));
        restored.commit().unwrap();
        assert!(files(&dir.0).unwrap().is_empty());
    }
}

#[cfg(test)]
mod processing_history_host_tests {
    use super::*;
    use crate::pin_model::ProcessingHistory;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/qa")
                .join(format!(
                    "pin-closed-processing-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    fn raw() -> Image {
        Image {
            width: 3,
            height: 2,
            bgra: vec![
                4, 20, 60, 255, 8, 40, 80, 127, 12, 60, 100, 255, 16, 80, 120, 63, 20, 100, 140,
                255, 24, 120, 160, 255,
            ],
            readback_ms: 0.,
        }
    }
    fn fixture() -> (PinState, ProcessingHistory, RasterClosedMetadata) {
        let mut history = ProcessingHistory::default();
        let mut current = Adjustments::default();
        assert!(history.change(
            &mut current,
            Adjustments {
                turns: 1,
                ..Default::default()
            }
        ));
        let next = Adjustments {
            brightness: 23,
            flip_x: true,
            ..current
        };
        assert!(history.change(&mut current, next));
        let next = Adjustments {
            invert: true,
            ..current
        };
        assert!(history.change(&mut current, next));
        assert!(history.undo(&mut current));
        (
            PinState {
                adjustments: current,
                ..Default::default()
            },
            history.clone(),
            RasterClosedMetadata {
                style: None,
                document: WireRasterDocument::empty((2, 3)).unwrap(),
                processing_history: Some(
                    WireProcessingHistory::from_history(&history, current, (3, 2), (2, 3)).unwrap(),
                ),
            },
        )
    }
    fn placement() -> Placement {
        Placement {
            x: -1933,
            y: -711,
            width: 24,
            height: 27,
        }
    }

    #[test]
    fn closed_processing_png_restore_preserves_original_both_stacks_and_commit_boundary() {
        let dir = TestDir::new();
        let (state, mut source, metadata) = fixture();
        save_processed_document_in(
            &dir.0,
            raw(),
            &state,
            placement(),
            PinWindowSizing::General,
            metadata,
        )
        .unwrap();
        let path = files(&dir.0).unwrap().remove(0);
        let restored = restore_in(&dir.0).unwrap();
        assert!(restored.text_document.is_none());
        let original = restored.image.as_ref().unwrap();
        assert_eq!(
            (original.width, original.height, &original.bgra),
            (3, 2, &raw().bgra)
        );
        let mut target = ProcessingHistory::default();
        let wire = restored.processing_history.as_ref().unwrap();
        assert!(wire
            .restore_into(state.adjustments, (2, 3), (2, 3), &mut target)
            .is_err());
        drop(restored);
        assert!(path.exists(), "首显安装失败不消费记录");
        let restored = restore_in(&dir.0).unwrap();
        let original = restored.image.as_ref().unwrap();
        restored
            .processing_history
            .as_ref()
            .unwrap()
            .restore_into(
                state.adjustments,
                (original.width, original.height),
                restored.raster_document.as_ref().unwrap().size(),
                &mut target,
            )
            .unwrap();
        let mut expected = state.adjustments;
        let mut actual = restored.state.adjustments;
        for undo in [false, true, true, true, false, false, false] {
            assert!(if undo {
                source.undo(&mut expected)
            } else {
                source.redo(&mut expected)
            });
            assert!(if undo {
                target.undo(&mut actual)
            } else {
                target.redo(&mut actual)
            });
            assert_eq!(
                source.bounded_snapshot(expected).unwrap(),
                target.bounded_snapshot(actual).unwrap()
            );
            let expected_image = expected.apply(&raw()).unwrap();
            let actual_image = actual.apply(original).unwrap();
            assert_eq!(
                (actual_image.width, actual_image.height, &actual_image.bgra),
                (
                    expected_image.width,
                    expected_image.height,
                    &expected_image.bgra
                )
            );
        }
        restored.commit().unwrap();
        assert!(
            !path.exists()
                && !path.with_extension("png").exists()
                && !path.with_extension("json").exists()
        );
    }

    #[test]
    fn closed_processing_bad_history_or_source_keeps_previous_record_and_legacy_is_empty() {
        let dir = TestDir::new();
        let (state, _, metadata) = fixture();
        let good_wire = metadata.processing_history.clone().unwrap();
        save_processed_document_in(
            &dir.0,
            raw(),
            &state,
            placement(),
            PinWindowSizing::General,
            metadata,
        )
        .unwrap();
        let path = files(&dir.0).unwrap().remove(0);
        let ini = std::fs::read(&path).unwrap();
        let png = std::fs::read(path.with_extension("png")).unwrap();
        let json = std::fs::read(path.with_extension("json")).unwrap();
        let mut invalid = serde_json::to_value(good_wire).unwrap();
        invalid["history"]["redo"][0]["brightness"] = serde_json::json!(101);
        assert!(save_processed_document_in(
            &dir.0,
            raw(),
            &state,
            placement(),
            PinWindowSizing::General,
            RasterClosedMetadata {
                style: None,
                document: WireRasterDocument::empty((2, 3)).unwrap(),
                processing_history: Some(serde_json::from_value(invalid).unwrap()),
            }
        )
        .is_err());
        assert_eq!(files(&dir.0).unwrap(), vec![path.clone()]);
        assert_eq!(std::fs::read(&path).unwrap(), ini);
        assert_eq!(std::fs::read(path.with_extension("png")).unwrap(), png);
        assert_eq!(std::fs::read(path.with_extension("json")).unwrap(), json);
        let wrong = Image {
            width: 4,
            height: 2,
            bgra: [20, 40, 80, 255].repeat(8),
            readback_ms: 0.,
        };
        export::save_png(wrong, &path.with_extension("png")).unwrap();
        assert!(
            restore_in(&dir.0).is_err(),
            "真实原图尺寸不合，不能安装处理历史或消费记录"
        );
        assert_eq!(std::fs::read(&path).unwrap(), ini);
        std::fs::write(path.with_extension("png"), &png).unwrap();
        let mut old: serde_json::Value = serde_json::from_slice(&json).unwrap();
        old["processingHistory"] = serde_json::Value::Null;
        std::fs::write(
            path.with_extension("json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        assert!(restore_in(&dir.0).is_err(), "null不是无历史旧记录");
        old.as_object_mut().unwrap().remove("processingHistory");
        std::fs::write(
            path.with_extension("json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let restored = restore_in(&dir.0).unwrap();
        assert!(restored.processing_history.is_none());
        assert_eq!(restored.state.adjustments, state.adjustments);
        let mut empty = ProcessingHistory::default();
        let mut current = restored.state.adjustments;
        assert!(!empty.undo(&mut current) && !empty.redo(&mut current));
        assert_eq!(current, state.adjustments);
        drop(restored);
        assert!(path.exists());

        // 实际 INI 曾先被 decode 正规化成与 wire 相等的值；有新历史必须拒绝。
        // 去掉可选历史后保持旧文件合同，不因本次功能收紧旧恢复格式。
        for (from, to) in [
            ("turns=0\n", "turns=4\n"),
            ("brightness=100\n", "brightness=101\n"),
            ("flipX=false\n", "flipX=false\nflipX=false\n"),
        ] {
            let strict_dir = TestDir::new();
            let mut history = ProcessingHistory::default();
            let mut current = Adjustments::default();
            assert!(history.change(
                &mut current,
                Adjustments {
                    brightness: 100,
                    ..Default::default()
                }
            ));
            let state = PinState {
                adjustments: current,
                ..Default::default()
            };
            let metadata = RasterClosedMetadata {
                style: None,
                document: WireRasterDocument::empty((3, 2)).unwrap(),
                processing_history: Some(
                    WireProcessingHistory::from_history(&history, current, (3, 2), (3, 2)).unwrap(),
                ),
            };
            save_processed_document_in(
                &strict_dir.0,
                raw(),
                &state,
                placement(),
                PinWindowSizing::General,
                metadata,
            )
            .unwrap();
            let path = files(&strict_dir.0).unwrap().remove(0);
            let source = std::fs::read_to_string(&path).unwrap();
            assert_eq!(source.matches(from).count(), 1);
            let changed = source.replace(from, to);
            std::fs::write(&path, &changed).unwrap();
            let png = std::fs::read(path.with_extension("png")).unwrap();
            let json = std::fs::read(path.with_extension("json")).unwrap();
            assert!(restore_in(&strict_dir.0).is_err());
            assert!(
                restore_in(&strict_dir.0).is_err(),
                "失败释放自己的租约，可继续重试"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
            assert_eq!(std::fs::read(path.with_extension("png")).unwrap(), png);
            assert_eq!(std::fs::read(path.with_extension("json")).unwrap(), json);
            let mut legacy: serde_json::Value = serde_json::from_slice(&json).unwrap();
            legacy.as_object_mut().unwrap().remove("processingHistory");
            std::fs::write(
                path.with_extension("json"),
                serde_json::to_vec(&legacy).unwrap(),
            )
            .unwrap();
            let restored = restore_in(&strict_dir.0).unwrap();
            assert!(restored.processing_history.is_none());
            assert_eq!(
                restored.state.adjustments, current,
                "旧文件仍按原decode正规化参数"
            );
            drop(restored);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
            std::fs::write(&path, source).unwrap();
            std::fs::write(path.with_extension("json"), json).unwrap();
            let recovered = restore_in(&strict_dir.0).unwrap();
            assert!(recovered.processing_history.is_some());
            recovered.commit().unwrap();
        }
    }
}

#[cfg(test)]
mod commit_cleanup_tests {
    use super::*;
    use std::{collections::BTreeMap, ffi::OsString};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "target/qa/pin-closed-commit-cleanup-model-{}",
                unique_key().unwrap()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    fn image() -> Image {
        Image {
            width: 2,
            height: 2,
            bgra: [17, 41, 73, 255].repeat(4),
            readback_ms: 0.,
        }
    }
    fn placement() -> Placement {
        Placement {
            x: -19,
            y: -7,
            width: 24,
            height: 24,
        }
    }
    fn bytes_in(dir: &Path) -> BTreeMap<OsString, Vec<u8>> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect()
    }

    #[test]
    fn image_committed_cleanup_error_is_success_and_invalid_png_input_keeps_old_record() {
        let dir = TestDir::new();
        save_in(&dir.0, image(), &PinState::default(), placement()).unwrap();
        let older = bytes_in(&dir.0);
        let cleanup_called = std::cell::Cell::new(false);
        // 使用正常 save_in 的同一实际事务；仅在 INI 发布之后注入清理失败。
        save_image_with_cleanup(&dir.0, image(), &PinState::default(), placement(), |dir| {
            cleanup_called.set(true);
            let paths = files(dir)?;
            assert_eq!(paths.len(), 2);
            let state = std::fs::read_to_string(&paths[0])?;
            assert_eq!(decode(&state)?.1, (-19, -7));
            assert_eq!(
                export::read_image(&paths[0].with_extension("png"))?.bgra,
                image().bgra
            );
            assert!(!paths[0].with_extension("tmp").exists());
            Err("注入普通图片已提交后的清理失败".into())
        })
        .unwrap();
        assert!(cleanup_called.get());
        let before = bytes_in(&dir.0);
        assert_eq!(before.len(), 4);
        for (name, bytes) in &older {
            assert_eq!(before.get(name), Some(bytes), "注入清理错误前未动旧记录");
        }
        let cleanup_called = std::cell::Cell::new(false);
        let mut bad_image = image();
        bad_image.bgra.pop();
        assert!(save_image_with_cleanup(
            &dir.0,
            bad_image,
            &PinState::default(),
            placement(),
            |_| {
                cleanup_called.set(true);
                Err("提交前失败不应进入此清理".into())
            }
        )
        .is_err());
        assert!(!cleanup_called.get());
        assert_eq!(bytes_in(&dir.0), before, "输入校验失败不新增或修改记录");
        let restored = restore_in(&dir.0).unwrap();
        assert_eq!(restored.image.as_ref().unwrap().bgra, image().bgra);
        assert!(restored.text_document.is_none() && restored.raster_document.is_none());
        assert_eq!(restored.origin, (-19, -7));
        restored.commit().unwrap();
        assert_eq!(bytes_in(&dir.0), older, "只消费新关闭记录");
        restore_in(&dir.0).unwrap().commit().unwrap();
        assert!(bytes_in(&dir.0).is_empty());
    }

    #[test]
    fn raster_committed_cleanup_error_is_success_and_precommit_failure_keeps_old_record() {
        // 有/无截图外观均走原 presentation 记录，同一生产发布边界。
        for style in [None, Some(SelectionFinishStyle::default())] {
            let dir = TestDir::new();
            let document = WireRasterDocument::empty((2, 2)).unwrap();
            let state = PinState::default();
            save_raster_document_in(
                &dir.0,
                image(),
                &state,
                placement(),
                PinWindowSizing::General,
                style,
                document.clone(),
            )
            .unwrap();
            let older = bytes_in(&dir.0);
            let metadata = serde_json::to_vec(&PresentationMetadata {
                version: 1,
                style,
                document: document.clone(),
                processing_history: None,
                window: Some(
                    PresentationWindow {
                        placement: placement(),
                        sizing: PinWindowSizing::General,
                    }
                    .into(),
                ),
            })
            .unwrap();
            let cleanup_called = std::cell::Cell::new(false);
            save_metadata_with_cleanup(
                &dir.0,
                image(),
                &encode(&state, placement()),
                &metadata,
                "presentation",
                |dir| {
                    cleanup_called.set(true);
                    let paths = files(dir)?;
                    assert_eq!(paths.len(), 2);
                    assert_eq!(std::fs::read(paths[0].with_extension("json"))?, metadata);
                    assert_eq!(
                        export::read_image(&paths[0].with_extension("png"))?.bgra,
                        image().bgra
                    );
                    assert!(std::fs::read_to_string(&paths[0])?.contains("presentation=1\n"));
                    assert!(!paths[0].with_extension("tmp").exists());
                    Err("注入截图文档已提交后的清理失败".into())
                },
            )
            .unwrap();
            assert!(cleanup_called.get());
            let before = bytes_in(&dir.0);
            assert_eq!(before.len(), 6);
            for (name, bytes) in &older {
                assert_eq!(before.get(name), Some(bytes), "注入清理错误前未动旧三件");
            }
            let mut bad_image = image();
            bad_image.bgra.pop();
            assert!(save_raster_document_in(
                &dir.0,
                bad_image,
                &state,
                placement(),
                PinWindowSizing::General,
                style,
                document.clone()
            )
            .is_err());
            assert_eq!(bytes_in(&dir.0), before, "PNG 输入校验失败保留已提交的三件");
            assert!(save_raster_document_in(
                &dir.0,
                image(),
                &state,
                placement(),
                PinWindowSizing::General,
                style,
                WireRasterDocument::empty((3, 2)).unwrap()
            )
            .is_err());
            assert_eq!(bytes_in(&dir.0), before, "文档尺寸验证失败保留已提交的三件");
            let restored = restore_in(&dir.0).unwrap();
            assert_eq!(restored.raster_document, Some(document));
            assert_eq!(restored.presentation_style, style);
            assert!(restored.text_document.is_none());
            assert_eq!(restored.image.as_ref().unwrap().bgra, image().bgra);
            assert_eq!(restored.presentation_window.unwrap().placement, placement());
            restored.commit().unwrap();
            assert_eq!(bytes_in(&dir.0), older, "只消费新关闭三件");
            restore_in(&dir.0).unwrap().commit().unwrap();
            assert!(bytes_in(&dir.0).is_empty());
        }
    }
}
