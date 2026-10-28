//! 图标尺寸、多页与独立图项集合；每次选择仍沿用一次主图请求，不解释为动画帧。
use super::{thumbnail, ImageViewer, LoadedImage};
use crate::{
    canvas_source::CanvasSource,
    capture::Image,
    geometry::Rotation,
    image_decode::{
        platform::{self, PlatformFormat},
        DecodedImage,
    },
    image_ico::IcoSource,
    image_tiff::TiffSource,
};
use gpui::{div, prelude::*, px, rgb, Context, MouseButton};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

// 来源输出先限额；保留原帧、长图切片与透明棋盘的呈现驻留量另在loaded分配前检查。
const DECODE_BYTES: u64 = crate::image_decode::MAX_DECODE_BYTES / 3;
const PROBE_BYTES: u64 = 64 * 1024;
const CONSTRUCTOR_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct Info {
    pub(super) kind: Kind,
    pub(super) entries: Vec<Choice>,
    pub(super) selected: usize,
    /// 原容器目录的图项总量；entries只保存实际解码成功的当前项，不虚构其它项的尺寸。
    pub(super) reported_total: Option<usize>,
}

impl Info {
    pub(super) fn retained_bytes(&self) -> u64 {
        (self.entries.capacity() as u64).saturating_mul(std::mem::size_of::<Choice>() as u64)
    }

    pub(super) fn count(&self) -> usize {
        self.reported_total.unwrap_or(self.entries.len())
    }

    pub(super) fn selected_entry(&self) -> Option<&Choice> {
        (self.selected < self.count())
            .then(|| {
                self.entries
                    .iter()
                    .find(|entry| entry.index == self.selected)
            })
            .flatten()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Icon,
    Cursor,
    Tiff,
    System(PlatformFormat),
}

impl Kind {
    pub(super) fn format_name(self) -> &'static str {
        match self {
            Self::Icon => "ICO",
            Self::Cursor => "CUR",
            Self::Tiff => "TIFF",
            Self::System(PlatformFormat::Avif) => "AVIF",
            Self::System(PlatformFormat::Heif) => "HEIF",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Choice {
    pub(super) index: usize,
    pub(super) dimensions: (u32, u32),
    pub(super) bits_per_pixel: u16,
    pub(super) hotspot: Option<(u16, u16)>,
    pub(super) source_item: Option<SourceItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SourceItem {
    pub(super) item_id: u32,
    pub(super) is_primary: bool,
    pub(super) color: Option<platform::ColorFacts>,
}

pub(super) struct Opened {
    pub(super) info: Info,
    pub(super) result: Result<LoadedImage, String>,
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(tr!("集合加载已取消", "Collection loading canceled").into())
    } else {
        Ok(())
    }
}

fn resident_error() -> String {
    tr!(
        "集合加载超过共享驻留预算",
        "Collection loading exceeds the shared resident budget"
    )
    .into()
}

fn resident_budget(parts: &[u64], cancel: &AtomicBool) -> Result<u64, String> {
    check_cancel(cancel)?;
    let bytes = parts
        .iter()
        .try_fold(0u64, |total, bytes| total.checked_add(*bytes))
        .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
        .ok_or_else(resident_error);
    check_cancel(cancel)?;
    bytes
}

fn choices<T>(
    source: &[T],
    external_resident: u64,
    cancel: &AtomicBool,
    make: impl Fn(&T) -> Choice,
) -> Result<Vec<Choice>, String> {
    check_cancel(cancel)?;
    let expected = (source.len() as u64)
        .checked_mul(std::mem::size_of::<Choice>() as u64)
        .ok_or_else(resident_error)?;
    resident_budget(&[external_resident, expected], cancel)?;
    let mut entries = Vec::new();
    let reserved = entries.try_reserve_exact(source.len());
    check_cancel(cancel)?;
    reserved.map_err(|_| resident_error())?;
    if entries.capacity() > source.len() {
        return Err(resident_error());
    }
    resident_budget(
        &[
            external_resident,
            (entries.capacity() as u64)
                .checked_mul(std::mem::size_of::<Choice>() as u64)
                .ok_or_else(resident_error)?,
        ],
        cancel,
    )?;
    for item in source {
        check_cancel(cancel)?;
        entries.push(make(item));
    }
    check_cancel(cancel)?;
    Ok(entries)
}

/// 仅提示容器类型，允许首图失败后仍显式进入集合恢复，不解析目录或像素。
pub(super) fn format_hint(path: &Path, cancel: &AtomicBool) -> Result<Option<Kind>, String> {
    check_cancel(cancel)?;
    crate::image_collection::ensure_local_file(path)?;
    let mut reader = File::open(path).map_err(|error| error.to_string())?;
    format_hint_reader(&mut reader, cancel)
}

fn format_hint_reader<R: Read + Seek>(
    reader: &mut R,
    cancel: &AtomicBool,
) -> Result<Option<Kind>, String> {
    check_cancel(cancel)?;
    let signature = read_signature(reader, cancel);
    // 短文件的EOF也必须完成读后取消检查。
    check_cancel(cancel)?;
    let Some(signature) = signature? else {
        return Ok(None);
    };
    if let Some(kind) = signature_kind(&signature) {
        return Ok(Some(kind));
    }
    if crate::image_decode::svg::looks_like_svg(&signature) {
        return Ok(None);
    }
    if &signature[4..] != b"ft" {
        return Ok(None);
    }
    // 沿用刚读完的六字节，整个品牌前缀最多读取4096字节，不探测系统组件。
    let mut prefix = [0; 4096];
    prefix[..signature.len()].copy_from_slice(&signature);
    let mut filled = signature.len();
    while filled < prefix.len() {
        check_cancel(cancel)?;
        let read = reader.read(&mut prefix[filled..]);
        check_cancel(cancel)?;
        match read {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    let detected = platform::detect(&prefix[..filled]);
    check_cancel(cancel)?;
    detected
        .map(|format| format.map(Kind::System))
        .map_err(|error| error.to_string())
}

pub(super) fn open_if_ico(
    path: &Path,
    page: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    let opened = (|| {
        resident_budget(&[PROBE_BYTES], cancel)?;
        crate::image_collection::ensure_local_file(path)?;
        let reader = File::open(path).map_err(|error| error.to_string())?;
        open_reader_if_ico(reader, page, cancel)
    })();
    check_cancel(cancel)?;
    opened
}

pub(super) fn open_if_collection(
    path: &Path,
    page: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    open_if_collection_with_resident(path, page, 0, cancel)
}

pub(super) fn open_if_collection_with_resident(
    path: &Path,
    page: Option<usize>,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    // ICO/CUR默认按尺寸打开；TIFF显式切页；独立图项的索引0始终是原主图。
    let opened = (|| {
        resident_budget(&[external_resident, PROBE_BYTES], cancel)?;
        crate::image_collection::ensure_local_file(path)?;
        if let Some(Kind::System(format)) = format_hint(path, cancel)? {
            return open_system(path, format, page.unwrap_or(0), external_resident, cancel);
        }
        let reader = File::open(path).map_err(|error| error.to_string())?;
        open_reader_if_collection_with_resident(reader, page, external_resident, cancel)
    })();
    check_cancel(cancel)?;
    opened
}

#[cfg(test)]
fn open_reader_if_collection<R: Read + Seek>(
    reader: R,
    page: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    open_reader_if_collection_with_resident(reader, page, 0, cancel)
}

fn open_reader_if_collection_with_resident<R: Read + Seek>(
    mut reader: R,
    page: Option<usize>,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    resident_budget(&[external_resident, PROBE_BYTES], cancel)?;
    let Some(signature) = read_signature(&mut reader, cancel)? else {
        return Ok(None);
    };
    match collection_kind(&signature, page) {
        Some(Kind::Icon) => {
            open_reader_if_ico_with_resident(reader, page, external_resident, cancel)
        }
        Some(Kind::Cursor) => {
            open_reader_if_cursor_with_resident(reader, page, external_resident, cancel)
        }
        Some(Kind::Tiff) => {
            open_reader_if_tiff_with_resident(reader, page.unwrap_or(0), external_resident, cancel)
        }
        Some(Kind::System(_)) | None => Ok(None),
    }
}

#[cfg(test)]
fn system_info(
    format: PlatformFormat,
    total: u32,
    selected: u32,
    item_id: u32,
    is_primary: bool,
    dimensions: (u32, u32),
    color: Option<platform::ColorFacts>,
) -> Result<Info, String> {
    system_info_with_resident(
        format,
        total,
        selected,
        item_id,
        is_primary,
        dimensions,
        color,
        0,
        &AtomicBool::new(false),
    )
}

fn system_info_with_resident(
    format: PlatformFormat,
    total: u32,
    selected: u32,
    item_id: u32,
    is_primary: bool,
    dimensions: (u32, u32),
    color: Option<platform::ColorFacts>,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Info, String> {
    check_cancel(cancel)?;
    if total == 0
        || selected >= total
        || item_id == 0
        || is_primary != (selected == 0)
        || dimensions.0 == 0
        || dimensions.1 == 0
    {
        return Err(tr!(
            "来源独立图项信息无效",
            "Invalid source image item information"
        )
        .into());
    }
    Ok(Info {
        kind: Kind::System(format),
        entries: choices(
            &[Choice {
                index: selected as usize,
                dimensions,
                // 这里只知道交付的8位预览，不能把它冒充来源编码位深。
                bits_per_pixel: 0,
                hotspot: None,
                source_item: Some(SourceItem {
                    item_id,
                    is_primary,
                    color,
                }),
            }],
            external_resident,
            cancel,
            Clone::clone,
        )?,
        selected: selected as usize,
        reported_total: Some(total as usize),
    })
}

fn open_system(
    path: &Path,
    format: PlatformFormat,
    requested: usize,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    let requested = u32::try_from(requested).map_err(|_| {
        tr!(
            "来源图项索引越界",
            "Source image item index is out of range"
        )
        .to_string()
    })?;
    check_cancel(cancel)?;
    let decoded = crate::image_decode::decode_platform_path_frame_with_resident(
        path,
        requested,
        DECODE_BYTES,
        external_resident,
        cancel,
    );
    check_cancel(cancel)?;
    let item = decoded.map_err(|error| error.to_string())?.ok_or_else(|| {
        tr!(
            "系统图片来源格式已改变，请重新打开",
            "System image source format changed; reopen it"
        )
        .to_string()
    })?;
    let matches_format = match format {
        PlatformFormat::Avif => item.decoded.metadata.format == image::ImageFormat::Avif,
        PlatformFormat::Heif => {
            item.decoded.metadata.format == crate::image_metadata::SourceFormat::Heif
        }
    };
    if !matches_format {
        return Err(tr!(
            "系统图片来源格式已改变，请重新打开",
            "System image source format changed; reopen it"
        )
        .into());
    }
    // 原生持有者已在统一入口释放；建立目录时当前像素与sidecar仍然驻留。
    let info_resident = resident_budget(
        &[
            external_resident,
            item.decoded.image.bgra.capacity() as u64,
            item.decoded
                .metadata_source
                .as_ref()
                .map_or(0, |source| source.retained_bytes() as u64),
            item.decoded
                .warning
                .as_ref()
                .map_or(0, |warning| warning.capacity() as u64),
        ],
        cancel,
    )?;
    let info = system_info_with_resident(
        format,
        item.frame_count,
        item.frame_index,
        item.item_id,
        item.is_primary,
        (item.decoded.image.width, item.decoded.image.height),
        item.color,
        info_resident,
        cancel,
    )?;
    let presentation_resident =
        resident_budget(&[external_resident, info.retained_bytes()], cancel)?;
    let result = loaded_with_resident(item.decoded, cancel, presentation_resident);
    check_cancel(cancel)?;
    Ok(Some(Opened { info, result }))
}

#[cfg(test)]
fn open_reader_if_tiff<R: Read + Seek>(
    reader: R,
    requested: usize,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    open_reader_if_tiff_with_resident(reader, requested, 0, cancel)
}

fn open_reader_if_tiff_with_resident<R: Read + Seek>(
    mut reader: R,
    requested: usize,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    // 类型已经确认；16MiB只覆盖本次有界目录构造，不扣普通静态路径。
    resident_budget(&[external_resident, CONSTRUCTOR_BYTES], cancel)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut source = TiffSource::from_reader(reader, cancel)?;
    let entries = choices(source.pages(), external_resident, cancel, |page| Choice {
        index: page.index,
        dimensions: page.display_dimensions,
        bits_per_pixel: page
            .color
            .as_ref()
            .map_or(0, |color| color.bits_per_pixel()),
        hotspot: None,
        source_item: None,
    })?;
    if entries.is_empty() {
        return Err(tr!("TIFF没有可浏览的页", "TIFF has no pages to browse").into());
    }
    let selected = if requested < entries.len() {
        requested
    } else {
        0
    };
    let info = Info {
        kind: Kind::Tiff,
        entries,
        selected,
        reported_total: None,
    };
    let presentation_resident =
        resident_budget(&[external_resident, info.retained_bytes()], cancel)?;
    let decoded =
        source.decode_with_resident(selected, DECODE_BYTES, presentation_resident, cancel);
    check_cancel(cancel)?;
    drop(source);
    let result =
        decoded.and_then(|decoded| loaded_with_resident(decoded, cancel, presentation_resident));
    check_cancel(cancel)?;
    // 不支持的单页保留目录；切到支持页时沿用新的主图请求恢复。
    Ok(Some(Opened { info, result }))
}

fn open_reader_if_ico<R: Read + Seek>(
    reader: R,
    page: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    open_reader_if_ico_with_resident(reader, page, 0, cancel)
}

fn open_reader_if_ico_with_resident<R: Read + Seek>(
    mut reader: R,
    page: Option<usize>,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    resident_budget(&[external_resident, PROBE_BYTES], cancel)?;
    let Some(signature) = read_signature(&mut reader, cancel)? else {
        return Ok(None);
    };
    if collection_kind(&signature, None) != Some(Kind::Icon) {
        return Ok(None);
    }
    resident_budget(&[external_resident, CONSTRUCTOR_BYTES], cancel)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let source = IcoSource::from_reader(reader, cancel)?;
    open_icon_source_with_resident(source, Kind::Icon, page, external_resident, cancel)
}

#[cfg(test)]
fn open_reader_if_cursor<R: Read + Seek>(
    reader: R,
    page: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    open_reader_if_cursor_with_resident(reader, page, 0, cancel)
}

fn open_reader_if_cursor_with_resident<R: Read + Seek>(
    mut reader: R,
    page: Option<usize>,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    resident_budget(&[external_resident, CONSTRUCTOR_BYTES], cancel)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let source = IcoSource::from_cursor_reader(reader, cancel)?;
    open_icon_source_with_resident(source, Kind::Cursor, page, external_resident, cancel)
}

#[cfg(test)]
fn open_icon_source<R: Read + Seek>(
    source: IcoSource<R>,
    kind: Kind,
    page: Option<usize>,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    open_icon_source_with_resident(source, kind, page, 0, cancel)
}

fn open_icon_source_with_resident<R: Read + Seek>(
    mut source: IcoSource<R>,
    kind: Kind,
    page: Option<usize>,
    external_resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<Opened>, String> {
    let entries = choices(source.entries(), external_resident, cancel, |entry| {
        Choice {
            index: entry.index,
            dimensions: entry.dimensions,
            bits_per_pixel: entry.bits_per_pixel,
            hotspot: entry.hotspot,
            source_item: None,
        }
    })?;
    let requested = page.unwrap_or_else(|| source.default_index());
    let selected = if requested < entries.len() {
        requested
    } else {
        source.default_index()
    };
    let info = Info {
        kind,
        entries,
        selected,
        reported_total: None,
    };
    let presentation_resident =
        resident_budget(&[external_resident, info.retained_bytes()], cancel)?;
    let decoded = source.decode_with_shared_input_reservation(
        selected,
        DECODE_BYTES,
        cancel,
        presentation_resident,
    );
    check_cancel(cancel)?;
    drop(source);
    let result =
        decoded.and_then(|decoded| loaded_with_resident(decoded, cancel, presentation_resident));
    check_cancel(cancel)?;
    // 单个尺寸损坏也交回目录，用户仍能选择集合中的其它尺寸。
    Ok(Some(Opened { info, result }))
}

fn read_signature<R: Read + Seek>(
    reader: &mut R,
    cancel: &AtomicBool,
) -> Result<Option<[u8; 6]>, String> {
    check_cancel(cancel)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut signature = [0; 6];
    let mut filled = 0;
    while filled < signature.len() {
        check_cancel(cancel)?;
        match reader.read(&mut signature[filled..]) {
            Ok(0) => return Ok(None),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    check_cancel(cancel)?;
    Ok(Some(signature))
}

fn collection_kind(signature: &[u8; 6], page: Option<usize>) -> Option<Kind> {
    match signature_kind(signature) {
        Some(Kind::Tiff) if page.is_none() => None,
        kind => kind,
    }
}

fn signature_kind(signature: &[u8; 6]) -> Option<Kind> {
    if signature[..4] == [0, 0, 1, 0] {
        Some(Kind::Icon)
    } else if signature[..4] == [0, 0, 2, 0] {
        Some(Kind::Cursor)
    } else if matches!(
        &signature[..4],
        b"II\x2a\x00" | b"MM\x00\x2a" | b"II\x2b\x00" | b"MM\x00\x2b"
    ) {
        // BigTIFF也进入显式来源，由来源给出准确的不支持原因，不伪装为首图成功。
        Some(Kind::Tiff)
    } else {
        None
    }
}

pub(super) fn loaded(decoded: DecodedImage, cancel: &AtomicBool) -> Result<LoadedImage, String> {
    loaded_with_resident(decoded, cancel, 0)
}

pub(super) fn loaded_with_resident(
    decoded: DecodedImage,
    cancel: &AtomicBool,
    external_resident: u64,
) -> Result<LoadedImage, String> {
    check_cancel(cancel)?;
    super::check_presentation_budget_with_resident(
        (decoded.image.width, decoded.image.height),
        decoded.image.bgra.capacity() as u64,
        decoded
            .metadata_source
            .as_ref()
            .map_or(0, |source| source.retained_bytes() as u64)
            .saturating_add(
                decoded
                    .warning
                    .as_ref()
                    .map_or(0, |warning| warning.capacity() as u64),
            ),
        true,
        external_resident,
    )?;
    let raw = Arc::new(decoded.image);
    let thumbnail = thumbnail(&raw)?;
    check_cancel(cancel)?;
    let display = Image {
        width: raw.width,
        height: raw.height,
        bgra: raw.bgra.clone(),
        readback_ms: raw.readback_ms,
    };
    let (source, previews) = CanvasSource::image(display).map_err(|error| error.to_string())?;
    let tiles = previews
        .into_iter()
        .enumerate()
        .map(|(index, image)| (source.screen_rect(index), image))
        .collect();
    check_cancel(cancel)?;
    Ok(LoadedImage {
        metadata: decoded.metadata,
        preview_geometry: None,
        preview_source: None,
        tiles,
        thumbnail,
        raw_frame: None,
        raw_page: Some(raw),
        warning: decoded.warning,
        metadata_source: decoded.metadata_source,
        rotation: Rotation::Identity,
    })
}

impl ImageViewer {
    pub(super) fn icon_controls(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut controls = div().flex().items_center().gap(px(4.)).flex_shrink_0();
        let Some(info) = self.pages.as_ref().filter(|info| info.count() != 0) else {
            return controls.into_any_element();
        };
        let count = info.count();
        let selected = info.selected.min(count - 1);
        let last = count - 1;
        let palette = crate::theme::palette(cx);
        let serial = self.serial;
        if info.kind == Kind::Tiff {
            controls = controls.child(
                div()
                    .id("tiff-first-image-preview")
                    .px(px(7.))
                    .py(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(palette.hover)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            window.focus(&view.focus);
                            if view.serial == serial {
                                if let Some(path) = view.path.clone() {
                                    view.open_path(path, false, cx);
                                }
                            }
                        }),
                    )
                    .child(tr!("首图预览", "First image")),
            );
        }
        for (id, label, index) in [
            ("icon-size-first", "|‹", 0),
            ("icon-size-previous", "‹", selected.saturating_sub(1)),
            ("icon-size-next", "›", selected.saturating_add(1).min(last)),
            ("icon-size-last", "›|", last),
        ] {
            let enabled = index != selected && self.path.is_some();
            controls = controls.child(
                div()
                    .id(id)
                    .px(px(7.))
                    .py(px(3.))
                    .rounded(px(4.))
                    .text_color(rgb(if enabled {
                        palette.foreground
                    } else {
                        palette.muted
                    }))
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(palette.hover)))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            window.focus(&view.focus);
                            if !enabled || view.serial != serial {
                                return;
                            }
                            if let Some(path) = view.path.clone() {
                                view.open_path_page(path, false, Some(index), cx);
                            }
                        }),
                    )
                    .child(label),
            );
        }
        let detail = info.selected_entry().map_or_else(
            || tr!("尺寸和位深未知", "Size and depth unknown").to_string(),
            |entry| {
                let depth = if entry.bits_per_pixel == 0 {
                    tr!("位深未声明", "depth unspecified").to_string()
                } else {
                    format!("{} bpp", entry.bits_per_pixel)
                };
                format!("{}×{} · {}", entry.dimensions.0, entry.dimensions.1, depth)
            },
        );
        let noun = match info.kind {
            Kind::Icon | Kind::Cursor => tr!("尺寸", "Size"),
            Kind::Tiff => tr!("页", "Page"),
            Kind::System(_) => tr!("图项", "Image item"),
        };
        controls
            .child(div().text_color(rgb(palette.muted)).child(format!(
                "{} {}/{} · {}",
                noun,
                selected + 1,
                count,
                detail
            )))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    mod resident {
        include!("image_viewer_pages_resident_tests.rs");
    }

    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels: Vec<_> = rgba
            .into_iter()
            .cycle()
            .take(width as usize * height as usize * 4)
            .collect();
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn icon() -> Vec<u8> {
        let payloads = [png(1, 1, [255, 0, 0, 255]), png(2, 2, [20, 40, 80, 128])];
        let mut bytes = vec![0, 0, 1, 0, 2, 0];
        let mut offset = 6 + payloads.len() * 16;
        for (side, payload) in [1, 2].into_iter().zip(&payloads) {
            bytes.extend_from_slice(&[side, side, 0, 0]);
            bytes.extend_from_slice(&1u16.to_le_bytes());
            bytes.extend_from_slice(&32u16.to_le_bytes());
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += payload.len();
        }
        for payload in payloads {
            bytes.extend_from_slice(&payload);
        }
        bytes
    }

    fn cursor() -> Vec<u8> {
        let mut bytes = icon();
        bytes[2..4].copy_from_slice(&2u16.to_le_bytes());
        for (index, (x, y)) in [(0u16, 0u16), (1, 1)].into_iter().enumerate() {
            let entry = 6 + index * 16;
            bytes[entry + 4..entry + 6].copy_from_slice(&x.to_le_bytes());
            bytes[entry + 6..entry + 8].copy_from_slice(&y.to_le_bytes());
        }
        bytes
    }

    fn tiff_fixture(first_compression: u16) -> Vec<u8> {
        const FIELDS: u16 = 11;
        let table_bytes = 2 + usize::from(FIELDS) * 12 + 4;
        let bits_start = 8 + table_bytes * 2;
        let pixels_start = bits_start + 12;
        let mut bytes = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
        for page in 0..2 {
            bytes.extend_from_slice(&FIELDS.to_le_bytes());
            for (tag, kind, count, value) in [
                (256u16, 4u16, 1u32, 2u32),
                (257, 4, 1, 1),
                (258, 3, 3, (bits_start + page * 6) as u32),
                (
                    259,
                    3,
                    1,
                    u32::from(if page == 0 { first_compression } else { 1 }),
                ),
                (262, 3, 1, 2),
                (273, 4, 1, (pixels_start + page * 6) as u32),
                (274, 3, 1, if page == 0 { 1 } else { 6 }),
                (277, 3, 1, 3),
                (278, 4, 1, 1),
                (279, 4, 1, 6),
                (284, 3, 1, 1),
            ] {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&kind.to_le_bytes());
                bytes.extend_from_slice(&count.to_le_bytes());
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            let next = if page == 0 {
                (8 + table_bytes) as u32
            } else {
                0
            };
            bytes.extend_from_slice(&next.to_le_bytes());
        }
        for _ in 0..6 {
            bytes.extend_from_slice(&8u16.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0, 255, 0, 0, 0]);
        bytes.extend_from_slice(&[255, 0, 0, 0, 255, 0]);
        bytes
    }

    #[test]
    fn image_viewer_pages_non_ico_and_cursor_signatures_leave_the_static_path() {
        for bytes in [
            b"GIF89a".as_slice(),
            b"\x89PNG\r\n\x1a\n".as_slice(),
            &[0, 0, 2, 0, 1, 0],
            &[],
        ] {
            assert!(open_reader_if_ico(Cursor::new(bytes), None, &token())
                .unwrap()
                .is_none());
        }
        assert!(open_reader_if_ico(Cursor::new(cursor()), None, &token())
            .unwrap()
            .is_none());
    }

    #[test]
    fn image_viewer_pages_tiff_signatures_require_explicit_page_mode() {
        for signature in [
            *b"II\x2a\x00\x08\x00",
            *b"MM\x00\x2a\x00\x00",
            *b"II\x2b\x00\x08\x00",
            *b"MM\x00\x2b\x00\x08",
        ] {
            assert_eq!(collection_kind(&signature, None), None);
            assert_eq!(collection_kind(&signature, Some(0)), Some(Kind::Tiff));
        }
        assert_eq!(collection_kind(&[0, 0, 1, 0, 1, 0], None), Some(Kind::Icon));
        assert_eq!(collection_kind(b"GIF89a", Some(0)), None);
        assert!(
            open_reader_if_ico(Cursor::new(tiff_fixture(5)), Some(1), &token())
                .unwrap()
                .is_none()
        );
        assert!(
            open_reader_if_collection(Cursor::new(tiff_fixture(5)), None, &token())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn image_viewer_pages_selected_size_keeps_straight_alpha_raw_for_snapshots() {
        let opened = open_reader_if_ico(Cursor::new(icon()), Some(1), &token())
            .unwrap()
            .unwrap();
        assert_eq!(opened.info.entries.len(), 2);
        assert_eq!(opened.info.selected, 1);
        let image = opened.result.unwrap();
        assert_eq!(image.metadata.display_dimensions, (2, 2));
        assert!(image.raw_frame.is_none());
        let request = super::super::SnapshotRequest {
            serial: 7,
            path: std::path::PathBuf::from("missing-selected-icon.ico"),
            frame: None,
            raw_page: image.raw_page.clone(),
            metadata_source: image.metadata_source.clone(),
            resident_bytes: super::super::presentation_resident_bytes(
                &image,
                std::iter::empty(),
                std::iter::empty(),
            ),
            target: super::super::SnapshotTarget::Editor,
        };
        let snapshot = super::super::source_snapshot(&request).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (2, 2));
        assert_eq!(&snapshot.bgra[..4], &[80, 40, 20, 128]);
        assert_eq!(image.tiles[0].1.as_bytes(0).unwrap()[3], 255);
        drop(image);
        assert_eq!(&snapshot.bgra[..4], &[80, 40, 20, 128]);
    }

    #[test]
    fn image_viewer_pages_bad_size_keeps_other_sizes_available() {
        let mut bytes = icon();
        let first_payload = u32::from_le_bytes(bytes[18..22].try_into().unwrap()) as usize;
        bytes[first_payload] = 0xfe;
        let bad = open_reader_if_ico(Cursor::new(&bytes), Some(0), &token())
            .unwrap()
            .unwrap();
        assert_eq!(bad.info.entries.len(), 2);
        assert_eq!(bad.info.selected, 0);
        assert!(bad.result.is_err());
        let other = open_reader_if_ico(Cursor::new(&bytes), Some(1), &token())
            .unwrap()
            .unwrap();
        assert_eq!(other.info.selected, 1);
        assert_eq!(other.result.unwrap().metadata.display_dimensions, (2, 2));
    }

    #[test]
    fn image_viewer_pages_stale_size_index_loads_the_selected_fallback() {
        let opened = open_reader_if_ico(Cursor::new(icon()), Some(usize::MAX), &token())
            .unwrap()
            .unwrap();
        assert_eq!(opened.info.selected, 1);
        assert_eq!(opened.result.unwrap().metadata.display_dimensions, (2, 2));
    }

    #[test]
    fn image_viewer_pages_tiff_bad_page_keeps_oriented_page_and_snapshot_available() {
        let bytes = tiff_fixture(5);
        let bad = open_reader_if_collection(Cursor::new(&bytes), Some(0), &token())
            .unwrap()
            .unwrap();
        assert_eq!(bad.info.kind, Kind::Tiff);
        assert_eq!(bad.info.entries.len(), 2);
        assert_eq!(bad.info.selected, 0);
        assert_eq!(bad.info.entries[1].dimensions, (1, 2));
        assert_eq!(bad.info.entries[1].bits_per_pixel, 24);
        assert!(bad.result.is_err());
        let opened = open_reader_if_collection(Cursor::new(&bytes), Some(1), &token())
            .unwrap()
            .unwrap();
        assert_eq!(opened.info.selected, 1);
        let image = opened.result.unwrap();
        assert_eq!(image.metadata.encoded_dimensions, Some((2, 1)));
        assert_eq!(image.metadata.display_dimensions, (1, 2));
        assert!(image.raw_frame.is_none());
        let request = super::super::SnapshotRequest {
            serial: 7,
            path: std::path::PathBuf::from("missing-selected-page.tiff"),
            frame: None,
            raw_page: image.raw_page.clone(),
            metadata_source: image.metadata_source.clone(),
            resident_bytes: super::super::presentation_resident_bytes(
                &image,
                std::iter::empty(),
                std::iter::empty(),
            ),
            target: super::super::SnapshotTarget::Editor,
        };
        let snapshot = super::super::source_snapshot(&request).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (1, 2));
        assert_eq!(snapshot.bgra, [0, 0, 255, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn image_viewer_pages_cursor_sizes_hotspots_and_raw_snapshot_stay_separate() {
        let bytes = cursor();
        let first = open_reader_if_collection(Cursor::new(&bytes), Some(0), &token())
            .unwrap()
            .unwrap();
        assert_eq!(first.info.kind, Kind::Cursor);
        assert_eq!(first.info.selected, 0);
        assert_eq!(first.info.entries[0].hotspot, Some((0, 0)));
        let first = first.result.unwrap();
        assert_eq!(first.metadata.display_dimensions, (1, 1));
        assert_eq!(first.raw_page.as_ref().unwrap().bgra, [0, 0, 255, 255]);

        let opened = open_reader_if_collection(Cursor::new(&bytes), None, &token())
            .unwrap()
            .unwrap();
        assert_eq!(opened.info.kind, Kind::Cursor);
        assert_eq!(opened.info.entries.len(), 2);
        assert_eq!(opened.info.selected, 1);
        assert_eq!(opened.info.entries[1].hotspot, Some((1, 1)));
        assert!(opened
            .info
            .entries
            .iter()
            .all(|entry| entry.bits_per_pixel == 0));
        let image = opened.result.unwrap();
        assert_eq!(image.metadata.display_dimensions, (2, 2));
        assert!(image.raw_frame.is_none());
        let request = super::super::SnapshotRequest {
            serial: 7,
            path: std::path::PathBuf::from("missing-selected-cursor.cur"),
            frame: None,
            raw_page: image.raw_page.clone(),
            metadata_source: image.metadata_source.clone(),
            resident_bytes: super::super::presentation_resident_bytes(
                &image,
                std::iter::empty(),
                std::iter::empty(),
            ),
            target: super::super::SnapshotTarget::Editor,
        };
        let snapshot = super::super::source_snapshot(&request).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (2, 2));
        assert_eq!(snapshot.bgra, [80, 40, 20, 128].repeat(4));
        assert_eq!(image.tiles[0].1.as_bytes(0).unwrap()[3], 255);
    }

    #[test]
    fn image_viewer_pages_bad_cursor_hotspot_keeps_other_sizes_available() {
        let mut bytes = cursor();
        // 首个尺寸为1×1，x=1越界；第二尺寸的(1,1)仍合法。
        bytes[10..12].copy_from_slice(&1u16.to_le_bytes());
        let bad = open_reader_if_collection(Cursor::new(&bytes), Some(0), &token())
            .unwrap()
            .unwrap();
        assert_eq!(bad.info.kind, Kind::Cursor);
        assert_eq!(bad.info.entries.len(), 2);
        assert_eq!(bad.info.selected, 0);
        assert_eq!(bad.info.entries[0].hotspot, Some((1, 0)));
        assert!(bad.result.is_err());
        let good = open_reader_if_collection(Cursor::new(&bytes), Some(1), &token())
            .unwrap()
            .unwrap();
        assert_eq!(good.info.selected, 1);
        assert_eq!(good.info.entries[1].hotspot, Some((1, 1)));
        let image = good.result.unwrap();
        assert_eq!(image.metadata.display_dimensions, (2, 2));
        assert_eq!(
            image.raw_page.as_ref().unwrap().bgra,
            [80, 40, 20, 128].repeat(4)
        );
    }

    #[test]
    fn image_viewer_pages_format_hint_is_independent_of_the_tiff_page_mode_gate() {
        for (signature, kind) in [
            ([0, 0, 1, 0, 1, 0], Kind::Icon),
            ([0, 0, 2, 0, 1, 0], Kind::Cursor),
            (*b"II\x2a\x00\x08\x00", Kind::Tiff),
            (*b"MM\x00\x2a\x00\x00", Kind::Tiff),
            (*b"II\x2b\x00\x08\x00", Kind::Tiff),
            (*b"MM\x00\x2b\x00\x08", Kind::Tiff),
        ] {
            assert_eq!(
                format_hint_reader(&mut Cursor::new(signature), &token()).unwrap(),
                Some(kind)
            );
            assert_eq!(
                collection_kind(&signature, None),
                if kind == Kind::Tiff { None } else { Some(kind) }
            );
        }
        for bytes in [b"GIF89a".as_slice(), b"II\x2a\x00".as_slice(), &[]] {
            assert_eq!(
                format_hint_reader(&mut Cursor::new(bytes), &token()).unwrap(),
                None
            );
        }
        let cancelled = AtomicBool::new(true);
        assert!(format_hint_reader(&mut Cursor::new(b"GIF89a"), &cancelled).is_err());
    }

    #[test]
    fn image_viewer_pages_failed_static_tiff_still_exposes_its_recoverable_second_page() {
        let mut bytes = tiff_fixture(1);
        // 首IFD的StripOffsets指向来源之外；链和第二IFD仍保持合法。
        let strip_offset_value = 8 + 2 + 5 * 12 + 8;
        let missing_strip = bytes.len() as u32 + 64;
        bytes[strip_offset_value..strip_offset_value + 4]
            .copy_from_slice(&missing_strip.to_le_bytes());
        assert!(crate::image_decode::decode_bytes(&bytes).is_err());
        assert_eq!(
            format_hint_reader(&mut Cursor::new(&bytes), &token()).unwrap(),
            Some(Kind::Tiff)
        );
        assert!(
            open_reader_if_collection(Cursor::new(&bytes), None, &token())
                .unwrap()
                .is_none()
        );
        let first = open_reader_if_collection(Cursor::new(&bytes), Some(0), &token())
            .unwrap()
            .unwrap();
        assert_eq!(first.info.entries.len(), 2);
        assert!(first.result.is_err());
        let second = open_reader_if_collection(Cursor::new(&bytes), Some(1), &token())
            .unwrap()
            .unwrap();
        assert_eq!(second.info.selected, 1);
        let image = second.result.unwrap();
        assert_eq!(image.metadata.display_dimensions, (1, 2));
        assert_eq!(
            image.raw_page.unwrap().bgra,
            [0, 0, 255, 255, 0, 255, 0, 255]
        );
    }

    #[test]
    fn image_viewer_pages_svg_comment_cannot_masquerade_as_a_platform_brand() {
        let bytes = include_bytes!("../tests/data/image_decode/offline-vector.svg");
        let cancel = AtomicBool::new(false);
        assert_eq!(
            format_hint_reader(&mut Cursor::new(bytes), &cancel).unwrap(),
            None
        );
        assert!(open_reader_if_collection(Cursor::new(bytes), None, &cancel)
            .unwrap()
            .is_none());
    }

    fn ftyp(major: &[u8; 4], compatible: &[[u8; 4]], extended: bool) -> Vec<u8> {
        let length = if extended { 24 } else { 16 } + compatible.len() * 4;
        let mut bytes = (if extended { 1u32 } else { length as u32 })
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(b"ftyp");
        if extended {
            bytes.extend_from_slice(&(length as u64).to_be_bytes());
        }
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        for brand in compatible {
            bytes.extend_from_slice(brand);
        }
        bytes
    }

    #[test]
    fn image_viewer_pages_system_hint_is_bounded_and_does_not_need_a_codec() {
        struct Counted {
            inner: Cursor<Vec<u8>>,
            read_bytes: usize,
        }
        impl Read for Counted {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = self.inner.read(output)?;
                self.read_bytes += count;
                Ok(count)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, request: SeekFrom) -> io::Result<u64> {
                self.inner.seek(request)
            }
        }
        for extended in [false, true] {
            for (major, compatible, expected) in [
                (*b"heic", *b"mif1", PlatformFormat::Heif),
                (*b"mif1", *b"avif", PlatformFormat::Avif),
                (*b"zzzz", *b"avis", PlatformFormat::Avif),
            ] {
                let bytes = ftyp(&major, &[compatible], extended);
                assert_eq!(
                    format_hint_reader(&mut Cursor::new(bytes), &token()).unwrap(),
                    Some(Kind::System(expected))
                );
            }
        }
        let mut minor_version = ftyp(b"isom", &[], false);
        minor_version[12..16].copy_from_slice(b"avif");
        assert_eq!(
            format_hint_reader(&mut Cursor::new(minor_version), &token()).unwrap(),
            None
        );
        let mut bytes = ftyp(b"avif", &vec![*b"mif1"; (4096 - 16) / 4], false);
        bytes.extend_from_slice(&[0; 8192]);
        let mut counted = Counted {
            inner: Cursor::new(bytes),
            read_bytes: 0,
        };
        assert_eq!(
            format_hint_reader(&mut counted, &token()).unwrap(),
            Some(Kind::System(PlatformFormat::Avif))
        );
        assert_eq!(counted.read_bytes, 4096);
        assert_eq!(counted.inner.position(), 4096);
        let mut oversized = ftyp(b"avif", &[], false);
        oversized[..4].copy_from_slice(&4100u32.to_be_bytes());
        oversized.resize(8192, 0);
        let mut counted = Counted {
            inner: Cursor::new(oversized),
            read_bytes: 0,
        };
        assert!(format_hint_reader(&mut counted, &token()).is_err());
        assert_eq!(counted.read_bytes, 4096);
        let truncated = ftyp(b"heic", &[], false);
        assert!(format_hint_reader(&mut Cursor::new(&truncated[..12]), &token()).is_err());
    }

    #[test]
    fn image_viewer_pages_system_info_keeps_only_actual_entry_and_recovers_sparse_selection() {
        let mut info = system_info(
            PlatformFormat::Avif,
            u32::MAX,
            u32::MAX - 1,
            41,
            false,
            (3, 2),
            None,
        )
        .unwrap();
        assert_eq!(info.count(), u32::MAX as usize);
        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.reported_total, Some(u32::MAX as usize));
        assert_eq!(info.selected, u32::MAX as usize - 1);
        let entry = info.selected_entry().unwrap();
        assert_eq!(entry.index, info.selected);
        assert_eq!(entry.dimensions, (3, 2));
        assert_eq!(entry.bits_per_pixel, 0);
        assert_eq!(entry.hotspot, None);
        assert_eq!(
            entry.source_item,
            Some(SourceItem {
                item_id: 41,
                is_primary: false,
                color: None,
            })
        );
        assert_eq!(info.kind.format_name(), "AVIF");
        // 坏图项没有新Choice，仍保留目录总数用于导航，不借旧项的身份和尺寸代填。
        info.selected = 7;
        assert!(info.selected_entry().is_none());
        assert_eq!(info.count(), u32::MAX as usize);
        info.entries.clear();
        assert!(info.selected_entry().is_none());
        assert_eq!(info.count(), u32::MAX as usize);
        assert!(system_info(PlatformFormat::Heif, 0, 0, 7, true, (1, 1), None).is_err());
        assert!(system_info(PlatformFormat::Heif, 2, 2, 7, false, (1, 1), None).is_err());
        assert!(system_info(PlatformFormat::Heif, 2, 0, 7, true, (0, 1), None).is_err());
        assert!(system_info(PlatformFormat::Heif, 2, 0, 0, true, (1, 1), None).is_err());
        assert!(system_info(PlatformFormat::Heif, 2, 0, 7, false, (1, 1), None).is_err());
        assert!(system_info(PlatformFormat::Heif, 2, 1, 7, true, (1, 1), None).is_err());
        let primary = system_info(PlatformFormat::Heif, 2, 0, 73, true, (1, 1), None).unwrap();
        assert_eq!(
            primary.selected_entry().unwrap().source_item,
            Some(SourceItem {
                item_id: 73,
                is_primary: true,
                color: None,
            })
        );
        let dense = Info {
            kind: Kind::Icon,
            entries: vec![
                Choice {
                    index: 0,
                    dimensions: (16, 16),
                    bits_per_pixel: 32,
                    hotspot: None,
                    source_item: None,
                },
                Choice {
                    index: 1,
                    dimensions: (32, 32),
                    bits_per_pixel: 24,
                    hotspot: None,
                    source_item: None,
                },
            ],
            selected: 1,
            reported_total: None,
        };
        assert_eq!(dense.count(), 2);
        assert_eq!(dense.selected_entry().unwrap().dimensions, (32, 32));
        assert_eq!(Kind::System(PlatformFormat::Heif).format_name(), "HEIF");
        assert_eq!(Kind::Cursor.format_name(), "CUR");
        assert_eq!(Kind::Tiff.format_name(), "TIFF");
    }

    #[test]
    fn image_viewer_pages_system_item_snapshot_keeps_selected_raw_after_rotation_and_close() {
        let pixels = vec![3, 2, 1, 0, 60, 40, 20, 128];
        let decoded = DecodedImage {
            image: Image {
                width: 2,
                height: 1,
                bgra: pixels.clone(),
                readback_ms: 0.0,
            },
            metadata: crate::image_metadata::ImageMetadata {
                format: crate::image_metadata::SourceFormat::Heif,
                encoded_dimensions: None,
                display_dimensions: (2, 1),
                orientation: None,
                encoded_bytes: Some(32),
                decoded_color: Some(image::ColorType::Rgba8),
                details: None,
            },
            warning: None,
            metadata_source: None,
        };
        let color = Some(platform::ColorFacts {
            container_cicp: None,
            bitstream_cicp: Some(platform::Cicp {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: true,
            }),
            sample_model: platform::SampleModel::Ycc,
            hdr_mapped: false,
        });
        let info = system_info(PlatformFormat::Heif, 3, 2, 41, false, (2, 1), color).unwrap();
        assert_eq!(info.selected, 2);
        assert_eq!(info.count(), 3);
        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.selected_entry().unwrap().dimensions, (2, 1));
        assert_eq!(
            info.selected_entry().unwrap().source_item,
            Some(SourceItem {
                item_id: 41,
                is_primary: false,
                color,
            })
        );
        let mut image = loaded(decoded, &token()).unwrap();
        assert!(image.raw_frame.is_none());
        assert!(image.metadata_source.is_none());
        assert_eq!(image.tiles[0].1.as_bytes(0).unwrap()[3], 255);
        let raw = image.raw_page.as_ref().unwrap().clone();
        let rotation = super::super::RotationRequest {
            serial: 7,
            epoch: 1,
            projection: super::super::ImageRotation::new((2, 1), Rotation::R90).unwrap(),
            tiles: image.tiles.clone(),
            resident_bytes: 24,
            cancel: Arc::new(token()),
        };
        image.tiles = super::super::static_rotation::rotate(&rotation, || false).unwrap();
        image.rotation = Rotation::R90;
        assert_eq!(image.tiles.len(), 1);
        assert_eq!((image.tiles[0].0.width, image.tiles[0].0.height), (1, 2));
        assert_eq!(raw.bgra, pixels);
        assert_eq!(image.metadata.display_dimensions, (2, 1));
        let request = super::super::SnapshotRequest {
            serial: 7,
            path: std::path::PathBuf::from("missing-selected-system-item.heic"),
            frame: None,
            raw_page: image.raw_page.clone(),
            metadata_source: image.metadata_source.clone(),
            resident_bytes: super::super::presentation_resident_bytes(
                &image,
                std::iter::empty(),
                std::iter::empty(),
            ),
            target: super::super::SnapshotTarget::Tools,
        };
        assert!(Arc::ptr_eq(request.raw_page.as_ref().unwrap(), &raw));
        drop(image);
        drop(info);
        drop(rotation);
        let snapshot = super::super::source_snapshot(&request).unwrap();
        assert_eq!((snapshot.width, snapshot.height), (2, 1));
        assert_eq!(snapshot.bgra, pixels);
        assert_ne!(snapshot.bgra.as_ptr(), raw.bgra.as_ptr());
        assert!(request.metadata_source.is_none());
        assert_eq!(raw.bgra, pixels);
    }

    #[test]
    fn image_viewer_pages_presentation_budget_counts_retained_raw_long_tiles_and_reserve() {
        let normal = (16_384, 1280);
        let normal_bytes = u64::from(normal.0) * u64::from(normal.1) * 4;
        assert!(super::super::check_presentation_budget(normal, normal_bytes, 0, true).is_ok());
        let long = (16_385, 1280);
        let long_bytes = u64::from(long.0) * u64::from(long.1) * 4;
        assert!(super::super::check_presentation_budget(long, long_bytes, 0, false).is_ok());
        assert!(super::super::check_presentation_budget(long, long_bytes, 0, true).is_err());
        assert!(
            super::super::check_presentation_budget((long.1, long.0), long_bytes, 0, true,)
                .is_err()
        );
        // 不分配大图：用元数据占满剩余额度，区分缓存预留、Vec多余容量和单字节越界。
        let metadata = crate::image_decode::MAX_DECODE_BYTES
            - super::super::THUMBNAIL_CACHE_BYTES as u64
            - 8
            - super::super::THUMBNAIL_WORKING_BYTES;
        assert!(super::super::check_presentation_budget((2, 1), 8, metadata, true).is_ok());
        assert!(super::super::check_presentation_budget((2, 1), 8, metadata + 1, true).is_err());
        assert!(super::super::check_presentation_budget((2, 1), 9, metadata, true).is_err());
        assert!(super::super::check_presentation_budget((2, 1), 7, 0, true).is_err());
        let external =
            crate::image_decode::MAX_DECODE_BYTES - 8 - super::super::THUMBNAIL_WORKING_BYTES;
        assert!(super::super::check_presentation_budget_with_resident(
            (2, 1),
            8,
            0,
            true,
            external
        )
        .is_ok());
        assert!(super::super::check_presentation_budget_with_resident(
            (2, 1),
            8,
            0,
            true,
            external + 1
        )
        .is_err());
        // 大图阶段由整图副本主导，但最终256×256缩略仍与其同时驻留。
        let pixel_bytes = 1024 * 1024 * 4u64;
        let external = crate::image_decode::MAX_DECODE_BYTES
            - 3 * pixel_bytes
            - u64::from(super::super::THUMBNAIL_SIDE).pow(2) * 4;
        assert!(super::super::check_presentation_budget_with_resident(
            (1024, 1024),
            pixel_bytes,
            0,
            true,
            external
        )
        .is_ok());
        assert!(super::super::check_presentation_budget_with_resident(
            (1024, 1024),
            pixel_bytes,
            0,
            true,
            external + 1
        )
        .is_err());
        assert!(super::super::check_presentation_budget_with_resident(
            (2, 1),
            8,
            0,
            true,
            u64::MAX
        )
        .is_err());
        assert!(super::super::check_presentation_budget((2, 1), u64::MAX, 0, true).is_err());
        assert!(super::super::check_presentation_budget((2, 1), 8, u64::MAX, true).is_err());
        assert!(super::super::check_presentation_budget((0, 1), 0, 0, true).is_err());
        assert!(super::super::check_presentation_budget((65_536, 1), 262_144, 0, true).is_err());
        assert!(
            super::super::check_presentation_budget((8000, 4001), 128_032_000, 0, true).is_err()
        );
    }

    #[test]
    fn image_viewer_pages_system_hint_read_error_prioritizes_simultaneous_cancel() {
        struct CancelOnPrefixRead {
            inner: Cursor<Vec<u8>>,
            cancel: Arc<AtomicBool>,
        }
        impl Read for CancelOnPrefixRead {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.inner.position() >= 6 {
                    self.cancel.store(true, Ordering::Release);
                    return Err(io::Error::other("source read failed"));
                }
                self.inner.read(output)
            }
        }
        impl Seek for CancelOnPrefixRead {
            fn seek(&mut self, request: SeekFrom) -> io::Result<u64> {
                self.inner.seek(request)
            }
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut reader = CancelOnPrefixRead {
            inner: Cursor::new(ftyp(b"avif", &[], false)),
            cancel: cancel.clone(),
        };
        let error = format_hint_reader(&mut reader, &cancel).unwrap_err();
        assert!(cancel.load(Ordering::Acquire));
        assert!(
            error.contains("取消") || error.contains("canceled"),
            "{error}"
        );
    }
}
