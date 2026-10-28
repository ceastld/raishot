//! 有界图片读入：先查文件头和尺寸，再分配像素；方向只在本层应用一次。
use crate::{capture::Image, image_metadata::ImageMetadata, Result};
use image::{DynamicImage, ImageDecoder, ImageReader};
use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

#[path = "image_decode_exr_adapter.rs"]
mod exr_adapter;
#[path = "image_decode_exr_budget.rs"]
mod exr_budget;
#[path = "image_decode_exr_compression.rs"]
mod exr_compression;
#[path = "image_decode_exr.rs"]
mod exr_pipeline;
#[path = "image_decode_gif_budget.rs"]
mod gif_budget;
#[path = "image_decode_input.rs"]
mod input;
#[path = "image_platform.rs"]
pub(crate) mod platform;
#[path = "image_decode_psd.rs"]
mod psd;
#[path = "image_decode_simple_budget.rs"]
mod simple_budget;
#[path = "image_svg.rs"]
pub(crate) mod svg;
#[path = "image_decode_text_budget.rs"]
mod text_budget;
#[path = "image_decode_viewer.rs"]
pub(crate) mod viewer;
#[path = "image_decode_webp_budget.rs"]
mod webp_budget;
pub(crate) use webp_budget::WebpWorkspace;

#[cfg(test)]
#[path = "image_decode_foreground_tests.rs"]
mod foreground_tests;

pub(crate) const MAX_DECODE_PIXELS: u64 = 32_000_000;
pub(crate) const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const METADATA_WORKING_BYTES: u64 = 16 * 1024 * 1024;
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// 动画来源先冻结唯一静态单帧视图，再与普通缩略入口共用完整工作区观察。
/// 返回值不含来源或宿主R；调用方另留观察器固定256KiB及最多一幅辅助图。
pub(crate) fn inspect_webp_workspace_with_limit(
    bytes: &[u8],
    external_source_bytes: u64,
    total_limit: u64,
    cancel: &AtomicBool,
) -> Result<WebpWorkspace> {
    webp_budget::inspect_with_limit(bytes, external_source_bytes, total_limit, cancel)
}

#[derive(Clone, Copy)]
enum DecodeContent {
    File,
    IconPng,
    IconDib,
}

#[derive(Clone, Copy)]
enum DecodeBudget {
    Ordinary,
    Shared(u64),
    Foreground(u64),
    StrictPrefetch(u64),
}

impl DecodeBudget {
    fn external_resident(self) -> Option<u64> {
        match self {
            Self::Ordinary => None,
            Self::Shared(bytes) | Self::Foreground(bytes) | Self::StrictPrefetch(bytes) => {
                Some(bytes)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct CodecWorkspace {
    dimensions: (u32, u32),
    native_bytes: u64,
    constructor_bytes: u64,
    decode_extra_bytes: u64,
    backend_limit_floor: u64,
}

pub(crate) struct DecodedImage {
    pub image: Image,
    pub metadata: ImageMetadata,
    pub warning: Option<String>,
    pub metadata_source: Option<std::sync::Arc<crate::image_export_metadata::SourceMetadata>>,
}

/// 原容器的独立图项目录，默认索引0是原主图；只持有当前项像素。
pub(crate) struct PlatformFrame {
    pub decoded: DecodedImage,
    pub frame_count: u32,
    pub frame_index: u32,
    pub item_id: u32,
    pub is_primary: bool,
    pub color: Option<platform::ColorFacts>,
}

fn check_encoded_size(bytes: u64) -> Result<()> {
    if bytes > MAX_DECODE_BYTES {
        return Err(tr!(
            "图片文件超过 256 MiB 读入上限",
            "Image file exceeds the 256 MiB input limit"
        )
        .into());
    }
    Ok(())
}

/// 不递归、不解析快捷方式；读取前由目录模型检查占位与路径链接。
pub(crate) fn decode_path(path: &Path) -> Result<DecodedImage> {
    decode_path_with_budget(path, MAX_DECODE_BYTES)
}

/// 预取使用更小的输出预算；尺寸来自本次解码器，检查后才分配整图。
pub(crate) fn decode_path_with_budget(path: &Path, max_output_bytes: u64) -> Result<DecodedImage> {
    decode_path_with_budget_cancellable(path, max_output_bytes, &NEVER_CANCELLED)
}

pub(crate) fn decode_path_with_budget_cancellable(
    path: &Path,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let bytes = read_path_input(path, cancel)?;
    decode_bytes_inner(
        &bytes,
        bytes.capacity() as u64,
        max_output_bytes,
        cancel,
        DecodeContent::File,
    )
}

/// 缩略图在已有codec工作集模型内，把宿主仍驻留的缓冲计入各阶段。
pub(crate) fn decode_path_for_thumbnail_cancellable(
    path: &Path,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_path_with_shared_budget_cancellable(
        path,
        max_output_bytes,
        external_resident_bytes,
        cancel,
    )
}

/// 缩略图仅使用已有codec工作集模型；严格预取资格另由独立入口控制。
pub(crate) fn decode_path_with_shared_budget_cancellable(
    path: &Path,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let result = (|| {
        let bytes = input::read_path_input_with_resident(
            path,
            cancel,
            external_resident_bytes,
            MAX_DECODE_BYTES,
        )?;
        decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            max_output_bytes,
            cancel,
            DecodeContent::File,
            DecodeBudget::Shared(external_resident_bytes),
        )
    })();
    check_cancel(cancel)?;
    result
}

/// 主图保留普通解码格式，并把宿主驻留量计入输入、原生像素、方向与转换。
/// 已建模型的codec沿用共享预算；其它后端的私有工作区尚未全部闭合，
/// 不能据此入口宣称所有格式的分配峰值都已受控。
pub(crate) fn decode_path_with_foreground_budget_cancellable(
    path: &Path,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let result = (|| {
        let bytes = input::read_path_input_with_resident(
            path,
            cancel,
            external_resident_bytes,
            MAX_DECODE_BYTES,
        )?;
        decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            max_output_bytes,
            cancel,
            DecodeContent::File,
            DecodeBudget::Foreground(external_resident_bytes),
        )
    })();
    check_cancel(cancel)?;
    result
}

/// 重新从同次冻结SVG字节按精确画布栅格化；不会缩放既有8位显示副本。
pub(crate) fn decode_svg_path_with_budget_cancellable(
    path: &Path,
    dimensions: (u32, u32),
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_svg_path_with_resident_budget_cancellable(path, dimensions, max_output_bytes, 0, cancel)
}

/// 宿主可保留旧冻结图；读入、XML转换及栅格缓冲都计入同一外部驻留量。
pub(crate) fn decode_svg_path_with_resident_budget_cancellable(
    path: &Path,
    dimensions: (u32, u32),
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let result = (|| {
        let bytes = input::read_path_input_with_resident(
            path,
            cancel,
            external_resident_bytes,
            svg::MAX_INPUT as u64,
        )?;
        let reserved = (bytes.capacity() as u64)
            .checked_add(external_resident_bytes)
            .ok_or_else(shared_budget_error)?;
        svg::decode(&bytes, reserved, Some(dimensions), max_output_bytes, cancel)
    })();
    check_cancel(cancel)?;
    result
}

/// 把宿主仍持有的像素与元数据计入每个可见缓冲阶段。
/// 先开放已核定的PNG/BMP与单帧8位JPEG路径；其它格式仍可由普通入口读取。
/// 这份预算不约束系统CMM私有缓存，也不替代调用方对缓存交付的代次检查。
pub(crate) fn decode_path_with_resident_budget_cancellable(
    path: &Path,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let result = (|| {
        check_shared_budget(&[external_resident_bytes, METADATA_WORKING_BYTES, 64 * 1024])?;
        check_cancel(cancel)?;
        crate::image_collection::ensure_local_file(path)?;
        let opened = File::open(path);
        check_cancel(cancel)?;
        let mut file = opened?;
        let metadata = file.metadata();
        check_cancel(cancel)?;
        let length = metadata?.len();
        check_encoded_size(length)?;
        check_shared_budget(&[
            external_resident_bytes,
            METADATA_WORKING_BYTES,
            64 * 1024,
            length,
        ])?;
        let bytes = read_input_with_resident(&mut file, cancel, external_resident_bytes)?;
        decode_bytes_inner_scoped(
            &bytes,
            bytes.capacity() as u64,
            max_output_bytes,
            cancel,
            DecodeContent::File,
            Some(external_resident_bytes),
        )
    })();
    check_cancel(cancel)?;
    result
}

fn read_path_input(path: &Path, cancel: &AtomicBool) -> Result<Vec<u8>> {
    input::read_path_input(path, cancel)
}

/// 与统一入口共用冻结读入和预算；只解码指定原图项，不预取全部图项。
pub(crate) fn decode_platform_path_frame(
    path: &Path,
    frame_index: u32,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Option<PlatformFrame>> {
    decode_platform_path_frame_with_resident(path, frame_index, max_output_bytes, 0, cancel)
}

/// 独立图项沿同次冻结输入解码，宿主驻留贯穿读入、原生图项与颜色阶段。
pub(crate) fn decode_platform_path_frame_with_resident(
    path: &Path,
    frame_index: u32,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Option<PlatformFrame>> {
    let result = (|| {
        let bytes = input::read_path_input_with_resident(
            path,
            cancel,
            external_resident_bytes,
            MAX_DECODE_BYTES,
        )?;
        let Some(format) = platform::detect(&bytes)? else {
            return Ok(None);
        };
        let source_reserved = (bytes.capacity() as u64)
            .checked_add(external_resident_bytes)
            .ok_or_else(shared_budget_error)?;
        decode_platform_frame(
            &bytes,
            source_reserved,
            format,
            frame_index,
            max_output_bytes,
            cancel,
        )
        .map(Some)
    })();
    check_cancel(cancel)?;
    result
}

fn decode_platform_frame(
    bytes: &[u8],
    input_reserved_bytes: u64,
    format: platform::PlatformFormat,
    frame_index: u32,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<PlatformFrame> {
    let decoded = platform::decode_with_input_reservation(
        bytes,
        input_reserved_bytes,
        format,
        frame_index,
        max_output_bytes,
        cancel,
    )?;
    check_cancel(cancel)?;
    Ok(platform_frame(decoded, format, bytes.len() as u64))
}

fn platform_frame(
    decoded: platform::PlatformDecoded,
    format: platform::PlatformFormat,
    encoded_bytes: u64,
) -> PlatformFrame {
    let display_dimensions = (decoded.image.width, decoded.image.height);
    let orientation = Some(
        decoded
            .orientation
            .unwrap_or(image::metadata::Orientation::NoTransforms),
    );
    let details = decoded.managed.or_else(|| {
        decoded.bits_per_channel.map(|bits| {
            use crate::{
                image_color_management::{ColorConversion, ManagedMetadata},
                image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
            };
            let unread = FieldSummary {
                status: MetadataStatus::NotInspected,
                encoded_bytes: 0,
                decoded_bytes: None,
            };
            ManagedMetadata {
                source: Summary {
                    icc: unread,
                    exif: unread,
                    color_model: ColorModel::Unknown,
                    bits_per_channel: Some(bits),
                    orientation,
                },
                profile: None,
                color_conversion: ColorConversion::NotInspected,
            }
        })
    });
    PlatformFrame {
        frame_count: decoded.frame_count,
        frame_index: decoded.frame_index,
        item_id: decoded.item_id,
        is_primary: decoded.is_primary,
        color: decoded.color,
        decoded: DecodedImage {
            image: decoded.image,
            metadata: ImageMetadata {
                format: match format {
                    platform::PlatformFormat::Avif => image::ImageFormat::Avif.into(),
                    platform::PlatformFormat::Heif => crate::image_metadata::SourceFormat::Heif,
                },
                encoded_dimensions: decoded.encoded_dimensions,
                display_dimensions,
                orientation,
                encoded_bytes: Some(encoded_bytes),
                decoded_color: decoded.decoded_color,
                details,
            },
            warning: decoded.warning,
            metadata_source: decoded.metadata_source,
        },
    }
}

pub(crate) fn decode_bytes(bytes: &[u8]) -> Result<DecodedImage> {
    decode_bytes_with_budget(bytes, MAX_DECODE_BYTES)
}

pub(crate) fn decode_bytes_cancellable(bytes: &[u8], cancel: &AtomicBool) -> Result<DecodedImage> {
    decode_bytes_inner(
        bytes,
        bytes.len() as u64,
        MAX_DECODE_BYTES,
        cancel,
        DecodeContent::File,
    )
}

pub(crate) fn decode_bytes_with_budget(
    bytes: &[u8],
    max_output_bytes: u64,
) -> Result<DecodedImage> {
    decode_bytes_inner(
        bytes,
        bytes.len() as u64,
        max_output_bytes,
        &NEVER_CANCELLED,
        DecodeContent::File,
    )
}

/// 已知来源Vec的调用方传实际capacity；借用字节入口只知道slice长度。
pub(crate) fn decode_bytes_with_reservation(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_bytes_inner(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeContent::File,
    )
}

/// 同次冻结字节的前台入口；来源实际capacity与其它仍驻留的缓冲分别计费。
/// 保留前台格式范围，不把尚未闭合的codec私有堆声明为已受统一硬限控制。
pub(crate) fn decode_bytes_with_foreground_budget_cancellable(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    let result = decode_bytes_inner_with_budget(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeContent::File,
        DecodeBudget::Foreground(external_resident_bytes),
    );
    check_cancel(cancel)?;
    result
}

/// 图标目录决定尺寸和热点，嵌入PNG的EXIF不改变容器坐标；ICC仍走统一处理。
pub(crate) fn decode_icon_png_with_budget(
    bytes: &[u8],
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_icon_png_with_reservation(bytes, bytes.len() as u64, max_output_bytes, cancel)
}

/// 容器调用方传入仍驻留的完整来源、虚拟图标及目录容量，不只计算PNG切片长度。
pub(crate) fn decode_icon_png_with_reservation(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_icon_png_inner(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeBudget::Ordinary,
    )
}

pub(crate) fn decode_icon_png_with_shared_reservation(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    // 宿主驻留量已经包含在容器给出的来源总量中，递归读取只启用阶段预算。
    decode_icon_png_inner(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeBudget::Shared(0),
    )
}

fn decode_icon_png_inner(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    budget: DecodeBudget,
) -> Result<DecodedImage> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(tr!("图标负载不是PNG", "Icon payload is not PNG").into());
    }
    decode_bytes_inner_with_budget(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeContent::IconPng,
        budget,
    )
}

/// 仅接收已校验目录构造的单项DIB图标，防止内部读取再次分派到图标目录。
pub(crate) fn decode_icon_dib_with_budget(
    bytes: &[u8],
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_icon_dib_with_reservation(bytes, bytes.len() as u64, max_output_bytes, cancel)
}

pub(crate) fn decode_icon_dib_with_reservation(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_icon_dib_inner(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeBudget::Ordinary,
    )
}

pub(crate) fn decode_icon_dib_with_shared_reservation(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<DecodedImage> {
    decode_icon_dib_inner(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeBudget::Shared(0),
    )
}

fn decode_icon_dib_inner(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    budget: DecodeBudget,
) -> Result<DecodedImage> {
    if bytes.len() < 34
        || bytes[..6] != [0, 0, 1, 0, 1, 0]
        || bytes[18..22] != 22u32.to_le_bytes()
        || !matches!(
            u32::from_le_bytes(bytes[22..26].try_into().unwrap()),
            12 | 40 | 52 | 56 | 108 | 124
        )
    {
        return Err(tr!("内部图标DIB负载无效", "Invalid internal icon DIB payload").into());
    }
    decode_bytes_inner_with_budget(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        DecodeContent::IconDib,
        budget,
    )
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn shared_budget_error() -> Box<dyn std::error::Error> {
    tr!(
        "图片读取超过共享驻留预算",
        "Image reading exceeds the shared resident budget"
    )
    .into()
}

fn check_shared_budget(parts: &[u64]) -> Result<()> {
    parts
        .iter()
        .try_fold(0u64, |total, bytes| total.checked_add(*bytes))
        .filter(|bytes| *bytes <= MAX_DECODE_BYTES)
        .ok_or_else(shared_budget_error)?;
    Ok(())
}

fn read_input_with_resident(
    reader: &mut impl Read,
    cancel: &AtomicBool,
    external_resident_bytes: u64,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    check_shared_budget(&[external_resident_bytes, METADATA_WORKING_BYTES, 64 * 1024])?;
    let mut bytes = Vec::new();
    let mut part = [0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let result = reader.read(&mut part);
        check_cancel(cancel)?;
        let count = match result {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if count == 0 {
            break;
        }
        let needed = bytes
            .len()
            .checked_add(count)
            .ok_or_else(shared_budget_error)?;
        check_encoded_size(needed as u64)?;
        if needed > bytes.capacity() {
            // 不假定扩容能原地完成；新旧压缩Vec同时存在时也必须在宿主额度内。
            let available = MAX_DECODE_BYTES
                .checked_sub(external_resident_bytes)
                .and_then(|bytes| bytes.checked_sub(METADATA_WORKING_BYTES + 64 * 1024))
                .and_then(|available| available.checked_sub(bytes.capacity() as u64))
                .ok_or_else(shared_budget_error)?;
            if needed as u64 > available {
                return Err(shared_budget_error());
            }
            let target = needed
                .max(bytes.capacity().max(64 * 1024).saturating_mul(2))
                .min(usize::try_from(available).unwrap_or(usize::MAX));
            let mut grown = Vec::new();
            grown.try_reserve_exact(target)?;
            check_shared_budget(&[
                external_resident_bytes,
                METADATA_WORKING_BYTES,
                64 * 1024,
                bytes.capacity() as u64,
                grown.capacity() as u64,
            ])?;
            grown.extend_from_slice(&bytes);
            grown.extend_from_slice(&part[..count]);
            bytes = grown;
        } else {
            check_shared_budget(&[
                external_resident_bytes,
                METADATA_WORKING_BYTES,
                64 * 1024,
                bytes.capacity() as u64,
            ])?;
            bytes.extend_from_slice(&part[..count]);
        }
    }
    check_cancel(cancel)?;
    Ok(bytes)
}

struct ReadEndBudget<'a> {
    length: u64,
    resident_bytes: u64,
    copied_capacity: Arc<AtomicU64>,
    cancel: &'a AtomicBool,
}

/// eager读取的私有压缩Vec也先分配精确额度，并记录实际容量供原生像素阶段使用。
struct ScopedReader<'a, R> {
    inner: R,
    budget: Option<ReadEndBudget<'a>>,
}

impl<R: BufRead + Seek> Read for ScopedReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(output)
    }

    fn read_to_end(&mut self, output: &mut Vec<u8>) -> std::io::Result<usize> {
        let Some(budget) = &self.budget else {
            return self.inner.read_to_end(output);
        };
        let checked = || {
            check_cancel(budget.cancel).map_err(|error| std::io::Error::other(error.to_string()))
        };
        let memory = |parts: &[u64]| {
            check_shared_budget(parts).map_err(|error| std::io::Error::other(error.to_string()))
        };
        checked()?;
        let position = self.inner.stream_position();
        checked()?;
        let remaining = budget.length.saturating_sub(position?);
        let remaining = usize::try_from(remaining)
            .map_err(|_| std::io::Error::other("image input length overflow"))?;
        let old_len = output.len();
        let required = old_len
            .checked_add(remaining)
            .ok_or_else(|| std::io::Error::other("image input length overflow"))?;
        if required > output.capacity() {
            memory(&[
                budget.resident_bytes,
                output.capacity() as u64,
                required as u64,
            ])?;
            let mut grown = Vec::new();
            grown
                .try_reserve_exact(required)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            memory(&[
                budget.resident_bytes,
                output.capacity() as u64,
                grown.capacity() as u64,
            ])?;
            grown.extend_from_slice(output);
            std::mem::swap(output, &mut grown);
        } else {
            memory(&[budget.resident_bytes, output.capacity() as u64])?;
        }
        output.resize(required, 0);
        for chunk in output[old_len..].chunks_mut(64 * 1024) {
            let mut filled = 0;
            while filled < chunk.len() {
                checked()?;
                let result = self.inner.read(&mut chunk[filled..]);
                checked()?;
                match result {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "image input truncated",
                        ))
                    }
                    Ok(count) => filled += count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        checked()?;
        budget
            .copied_capacity
            .store(output.capacity() as u64, Ordering::Relaxed);
        Ok(remaining)
    }
}

impl<R: BufRead + Seek> BufRead for ScopedReader<'_, R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.inner.fill_buf()
    }
    fn consume(&mut self, count: usize) {
        self.inner.consume(count);
    }
}

impl<R: BufRead + Seek> Seek for ScopedReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

#[derive(Clone, Copy)]
struct JpegWorkspace {
    dimensions: (u32, u32),
    bytes: u64,
}

fn jpeg_workspace(bytes: &[u8], cancel: &AtomicBool) -> Result<JpegWorkspace> {
    jpeg_workspace_with_sampling(bytes, cancel, true)
}

fn jpeg_workspace_with_sampling(
    bytes: &[u8],
    cancel: &AtomicBool,
    strict_prefetch: bool,
) -> Result<JpegWorkspace> {
    let invalid = || {
        tr!(
            "JPEG结构不符合有界读取范围",
            "JPEG structure is outside bounded reading support"
        )
    };
    let mut position = 2usize;
    let mut entropy = false;
    let mut frame: Option<JpegWorkspace> = None;
    let mut component_ids = [0u8; 4];
    let mut components = 0usize;
    let mut markers = 0usize;
    let mut scans = 0usize;
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(invalid().into());
    }
    while position < bytes.len() {
        check_cancel(cancel)?;
        if entropy {
            while position < bytes.len() && bytes[position] != 0xff {
                position += 1;
                if position & 0xffff == 0 {
                    check_cancel(cancel)?;
                }
            }
        }
        if position >= bytes.len() || bytes[position] != 0xff {
            return Err(invalid().into());
        }
        while position < bytes.len() && bytes[position] == 0xff {
            position += 1;
            if position & 0xffff == 0 {
                check_cancel(cancel)?;
            }
        }
        let marker = *bytes.get(position).ok_or_else(invalid)?;
        position += 1;
        if entropy && (marker == 0 || matches!(marker, 0xd0..=0xd7 | 0x01)) {
            continue;
        }
        entropy = false;
        if marker == 0xd9 {
            if scans == 0 || (strict_prefetch && position != bytes.len()) {
                return Err(invalid().into());
            }
            return frame.ok_or_else(|| invalid().into());
        }
        if matches!(marker, 0 | 0xd8 | 0xd0..=0xd7) {
            return Err(invalid().into());
        }
        if marker == 0x01 {
            continue;
        }
        markers += 1;
        if strict_prefetch && markers > 4096 {
            return Err(invalid().into());
        }
        let length_bytes = bytes
            .get(position..position.checked_add(2).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        let length = usize::from(u16::from_be_bytes(length_bytes.try_into().unwrap()));
        if length < 2 {
            return Err(invalid().into());
        }
        let end = position
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid)?;
        let data = &bytes[position + 2..end];
        match marker {
            0xc0..=0xc2 => {
                if frame.is_some() || data.len() < 6 || data[0] != 8 {
                    return Err(invalid().into());
                }
                let height = u32::from(u16::from_be_bytes(data[1..3].try_into().unwrap()));
                let width = u32::from(u16::from_be_bytes(data[3..5].try_into().unwrap()));
                components = usize::from(data[5]);
                if !matches!(components, 1 | 3 | 4) || data.len() != 6 + components * 3 {
                    return Err(invalid().into());
                }
                check_dimensions((width, height), u64::from(width) * u64::from(height) * 4)?;
                let mut sampling = [(0u64, 0u64); 4];
                let mut h_max = 1u64;
                let mut v_max = 1u64;
                for (index, component) in data[6..].chunks_exact(3).enumerate() {
                    if component_ids[..index].contains(&component[0]) {
                        return Err(invalid().into());
                    }
                    component_ids[index] = component[0];
                    let (h, v) = (u64::from(component[1] >> 4), u64::from(component[1] & 15));
                    let valid_sampling = if strict_prefetch {
                        matches!(h, 1 | 2) && matches!(v, 1 | 2)
                    } else {
                        matches!(h, 1 | 2 | 4) && (1..=4).contains(&v)
                    };
                    if !valid_sampling
                        || component[2] > 3
                        || (strict_prefetch && components == 1 && (h != 1 || v != 1))
                    {
                        return Err(invalid().into());
                    }
                    sampling[index] = (h, v);
                    h_max = h_max.max(h);
                    v_max = v_max.max(v);
                }
                let mcu_x = u64::from(width).div_ceil(h_max * 8);
                let mcu_y = u64::from(height).div_ceil(v_max * 8);
                let mut coefficients = 0u64;
                for (h, v) in &sampling[..components] {
                    coefficients = coefficients
                        .checked_add(
                            mcu_x
                                .checked_mul(mcu_y)
                                .and_then(|blocks| blocks.checked_mul(*h))
                                .and_then(|blocks| blocks.checked_mul(*v))
                                .and_then(|blocks| blocks.checked_mul(64 * 2))
                                .ok_or_else(shared_budget_error)?,
                        )
                        .ok_or_else(shared_budget_error)?;
                }
                // 为baseline多扫描和progressive都保留完整i16系数平面。
                // 普通首图保留后端支持的4倍水平及1..4垂直采样；宽采样的MCU行、
                // 上下行、通用上采样和resize新旧容量另按32像素填充列预留。
                let row_factor = if h_max > 2 || v_max > 2 { 4096 } else { 1024 };
                let padding = if strict_prefetch { 16 } else { 32 };
                let rows = (u64::from(width) + padding)
                    .checked_mul(row_factor)
                    .ok_or_else(shared_budget_error)?;
                frame = Some(JpegWorkspace {
                    dimensions: (width, height),
                    bytes: coefficients
                        .checked_add(rows)
                        .and_then(|bytes| bytes.checked_add(4 * 1024 * 1024))
                        .ok_or_else(shared_budget_error)?,
                });
            }
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf | 0xdc | 0xde | 0xdf => {
                // 不允许后置尺寸、层级或第二SOF在解码器内改变已核定工作集。
                return Err(invalid().into());
            }
            0xda => {
                if frame.is_none() || data.is_empty() {
                    return Err(invalid().into());
                }
                let count = usize::from(data[0]);
                if count == 0 || count > components || data.len() != 1 + count * 2 + 3 {
                    return Err(invalid().into());
                }
                let mut scan_ids = [0u8; 4];
                for (index, item) in data[1..1 + count * 2].chunks_exact(2).enumerate() {
                    if !component_ids[..components].contains(&item[0])
                        || scan_ids[..index].contains(&item[0])
                        || item[1] >> 4 > 3
                        || item[1] & 15 > 3
                    {
                        return Err(invalid().into());
                    }
                    scan_ids[index] = item[0];
                }
                scans += 1;
                entropy = true;
            }
            0xe0 | 0xee if strict_prefetch && scans != 0 => return Err(invalid().into()),
            _ => {}
        }
        position = end;
    }
    Err(invalid().into())
}

fn decode_bytes_inner(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    content: DecodeContent,
) -> Result<DecodedImage> {
    decode_bytes_inner_with_budget(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        content,
        DecodeBudget::Ordinary,
    )
}

fn decode_bytes_inner_scoped(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    content: DecodeContent,
    external_resident_bytes: Option<u64>,
) -> Result<DecodedImage> {
    decode_bytes_inner_with_budget(
        bytes,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
        content,
        external_resident_bytes.map_or(DecodeBudget::Ordinary, DecodeBudget::StrictPrefetch),
    )
}

fn decode_bytes_inner_with_budget(
    bytes: &[u8],
    input_reserved_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    content: DecodeContent,
    budget: DecodeBudget,
) -> Result<DecodedImage> {
    check_cancel(cancel)?;
    check_encoded_size(bytes.len() as u64)?;
    if input_reserved_bytes < bytes.len() as u64 || input_reserved_bytes > MAX_DECODE_BYTES {
        return Err(tr!(
            "图片来源缓冲超过预算或小于实际数据",
            "Image source buffer exceeds its budget or is smaller than its data"
        )
        .into());
    }
    let external_resident_bytes = budget.external_resident();
    let scoped = external_resident_bytes.is_some();
    if matches!(budget, DecodeBudget::StrictPrefetch(_))
        && !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && !bytes.starts_with(b"BM")
        && !bytes.starts_with(&[0xff, 0xd8])
    {
        return Err(tr!(
            "此格式尚未开放共享驻留预算读取",
            "This format is not yet available for shared-budget reading"
        )
        .into());
    }
    if matches!(content, DecodeContent::File) && bytes.starts_with(b"8BPS") {
        // PSD的唯一输出直接还原至BGRA；来源实际容量与外部R在专用账本中各记一次。
        return psd::decode(
            bytes,
            input_reserved_bytes,
            external_resident_bytes.unwrap_or(0),
            max_output_bytes,
            cancel,
        );
    }
    let input_reserved_bytes = input_reserved_bytes
        .checked_add(external_resident_bytes.unwrap_or(0))
        .ok_or_else(shared_budget_error)?;
    let jpeg_workspace = if scoped && bytes.starts_with(&[0xff, 0xd8]) {
        Some(if matches!(budget, DecodeBudget::StrictPrefetch(_)) {
            jpeg_workspace(bytes, cancel)?
        } else {
            jpeg_workspace_with_sampling(bytes, cancel, false)?
        })
    } else {
        None
    };
    if matches!(content, DecodeContent::File) {
        // XML注释可以碰巧位于BMFF品牌位置；先按完整SVG合同验证XML来源。
        if svg::looks_like_svg(bytes) {
            return svg::decode(bytes, input_reserved_bytes, None, max_output_bytes, cancel);
        }
        if let Some(format) = platform::detect(bytes)? {
            return decode_platform_frame(
                bytes,
                input_reserved_bytes,
                format,
                0,
                max_output_bytes,
                cancel,
            )
            .map(|frame| frame.decoded);
        }
    }
    if input_reserved_bytes
        .checked_add(METADATA_WORKING_BYTES)
        .is_none_or(|bytes| bytes > MAX_DECODE_BYTES)
    {
        return Err(tr!(
            "图片来源与元数据预检超过256MiB预算",
            "Image source and metadata preflight exceed the 256 MiB budget"
        )
        .into());
    }
    if matches!(content, DecodeContent::File)
        && (bytes.starts_with(&[0, 0, 1, 0]) || bytes.starts_with(&[0, 0, 2, 0]))
    {
        let cursor = std::io::Cursor::new(bytes);
        let mut source = if bytes[2] == 1 {
            crate::image_ico::IcoSource::from_reader(cursor, cancel)
        } else {
            crate::image_ico::IcoSource::from_cursor_reader(cursor, cancel)
        }?;
        let index = source.default_index();
        return if scoped {
            source.decode_with_shared_input_reservation(
                index,
                max_output_bytes,
                cancel,
                input_reserved_bytes,
            )
        } else {
            source.decode_with_input_reservation(
                index,
                max_output_bytes,
                cancel,
                input_reserved_bytes,
            )
        }
        .map_err(Into::into);
    }
    let big_tiff = bytes.starts_with(b"II+\0") || bytes.starts_with(b"MM\0+");
    let is_tiff = bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") || big_tiff;
    let exr_prepared = if matches!(budget, DecodeBudget::Foreground(_))
        && bytes.starts_with(&[0x76, 0x2f, 0x31, 0x01])
    {
        Some(exr_pipeline::inspect(bytes, input_reserved_bytes, cancel)?)
    } else {
        None
    };
    let codec_workspace = if matches!(
        budget,
        DecodeBudget::Shared(_) | DecodeBudget::Foreground(_)
    ) && is_tiff
    {
        let workspace =
            crate::image_tiff::preflight_workspace(bytes, input_reserved_bytes, cancel)?;
        Some(CodecWorkspace {
            dimensions: workspace.dimensions,
            native_bytes: workspace.native_bytes,
            constructor_bytes: workspace.constructor_bytes,
            decode_extra_bytes: workspace.decode_extra_bytes,
            backend_limit_floor: workspace.backend_limit_floor,
        })
    } else if matches!(
        budget,
        DecodeBudget::Shared(_) | DecodeBudget::Foreground(_)
    ) && bytes.starts_with(b"RIFF")
        && bytes.get(8..12) == Some(&b"WEBP"[..])
    {
        let workspace = webp_budget::inspect(bytes, input_reserved_bytes, cancel)?;
        Some(CodecWorkspace {
            dimensions: workspace.dimensions,
            native_bytes: workspace.native_bytes,
            constructor_bytes: workspace.constructor_bytes,
            decode_extra_bytes: workspace.decode_extra_bytes,
            backend_limit_floor: workspace.backend_limit_floor,
        })
    } else if matches!(
        budget,
        DecodeBudget::Shared(_) | DecodeBudget::Foreground(_)
    ) && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
    {
        let workspace = gif_budget::inspect(bytes, input_reserved_bytes, cancel)?;
        Some(CodecWorkspace {
            dimensions: workspace.dimensions,
            native_bytes: workspace.native_bytes,
            constructor_bytes: workspace.constructor_bytes,
            decode_extra_bytes: workspace.decode_extra_bytes,
            backend_limit_floor: workspace.backend_limit_floor,
        })
    } else if matches!(budget, DecodeBudget::Foreground(_)) {
        // 主图补齐流式额外格式的工作集；不自动扩大缩略或严格预取资格。
        match exr_prepared {
            Some(prepared) => Some(prepared.workspace),
            None => match simple_budget::inspect(bytes, input_reserved_bytes, cancel)? {
                Some(workspace) => Some(workspace),
                None => text_budget::inspect(bytes, input_reserved_bytes, cancel)?,
            },
        }
    } else {
        None
    };
    if is_tiff && codec_workspace.is_none() {
        // 后端构造器会先读IFD；先在同一份冻结字节上约束目录与嵌入JPEG头。
        crate::image_tiff::preflight_metadata_with_budget(bytes, max_output_bytes)?;
        check_cancel(cancel)?;
    }
    let inspected = crate::image_container_metadata::inspect_cancellable(bytes, cancel)?;
    let source = inspected.summary;
    let profile = inspected.icc_profile;
    let supported_exif = inspected.supported_exif;
    let view_reserved_bytes = inspected
        .view
        .retained_bytes()?
        .checked_add(8 * 1024)
        .ok_or_else(|| {
            tr!(
                "图片分段缓冲容量溢出",
                "Image segmented-buffer capacity overflow"
            )
        })?;
    let metadata_reserved_bytes = profile
        .as_ref()
        .map_or(0, |profile| profile.capacity() as u64)
        .checked_add(supported_exif.as_ref().map_or(0, |exif| match exif {
            Ok(exif) => exif.retained_bytes() as u64,
            Err(reason) => reason.capacity() as u64,
        }))
        .ok_or_else(|| tr!("图片元数据容量溢出", "Image metadata capacity overflow"))?;
    let source_resident_bytes = input_reserved_bytes
        .checked_add(metadata_reserved_bytes)
        .ok_or_else(|| tr!("图片来源容量溢出", "Image source capacity overflow"))?;
    check_cancel(cancel)?;
    let private_copy = jpeg_workspace.map(|_| Arc::new(AtomicU64::new(0)));
    let copy_budget = if let Some(copied_capacity) = &private_copy {
        let resident_bytes = source_resident_bytes
            .checked_add(view_reserved_bytes)
            .and_then(|bytes| bytes.checked_add(METADATA_WORKING_BYTES))
            .and_then(|bytes| bytes.checked_add(4 * 1024 * 1024))
            .ok_or_else(shared_budget_error)?;
        Some(ReadEndBudget {
            length: inspected.view.len(),
            resident_bytes,
            copied_capacity: copied_capacity.clone(),
            cancel,
        })
    } else {
        None
    };
    let mut reader = ImageReader::new(ScopedReader {
        inner: BufReader::with_capacity(8 * 1024, inspected.view),
        budget: copy_budget,
    })
    .with_guessed_format()?;
    if big_tiff {
        // 通用头猜测只识别经典TIFF；BigTIFF已在上方检查完整目录，再指定同一后端。
        reader.set_format(image::ImageFormat::Tiff);
    }
    let format = reader
        .format()
        .ok_or_else(|| tr!("无法识别图片文件头", "Unknown image file header"))?;
    if matches!(budget, DecodeBudget::Shared(_))
        && codec_workspace.is_none()
        && !matches!(
            format,
            image::ImageFormat::Png | image::ImageFormat::Bmp | image::ImageFormat::Jpeg
        )
        && !matches!(content, DecodeContent::IconDib)
    {
        return Err(shared_budget_error());
    }
    if let Some(workspace) = codec_workspace {
        check_shared_budget(&[
            source_resident_bytes,
            view_reserved_bytes,
            METADATA_WORKING_BYTES,
            workspace.constructor_bytes,
        ])?;
    }
    let mut reader_limits = crate::export::decode_limits();
    if scoped {
        reader_limits.reserve(source_resident_bytes)?;
        reader_limits.reserve(view_reserved_bytes)?;
        reader_limits.reserve(METADATA_WORKING_BYTES)?;
    }
    reader.limits(reader_limits);
    let mut decoder: Box<dyn ImageDecoder + '_> = if let Some(prepared) = exr_prepared {
        if format != image::ImageFormat::OpenExr {
            return Err(shared_budget_error());
        }
        drop(reader);
        Box::new(exr_adapter::Decoder::new(bytes, cancel, |_, _| {
            Ok(prepared.selection)
        })?)
    } else {
        Box::new(reader.into_decoder()?)
    };
    let encoded_dimensions = decoder.dimensions();
    if jpeg_workspace.is_some_and(|workspace| workspace.dimensions != encoded_dimensions) {
        return Err(shared_budget_error());
    }
    if codec_workspace.is_some_and(|workspace| {
        workspace.dimensions != encoded_dimensions || decoder.total_bytes() > workspace.native_bytes
    }) {
        return Err(shared_budget_error());
    }
    check_dimensions(encoded_dimensions, decoder.total_bytes())?;
    let output_bytes = u64::from(encoded_dimensions.0) * u64::from(encoded_dimensions.1) * 4;
    if output_bytes > max_output_bytes {
        return Err(tr!(
            "图片超过当前读取预算",
            "Image exceeds the current output budget"
        )
        .into());
    }
    let orientation = if !matches!(content, DecodeContent::File) {
        image::metadata::Orientation::NoTransforms
    } else if source.exif.status != crate::image_container_metadata::MetadataStatus::NotInspected {
        source
            .orientation
            .unwrap_or(image::metadata::Orientation::NoTransforms)
    } else {
        decoder.orientation()?
    };
    let decoded_color = decoder.color_type();
    if scoped {
        check_shared_budget(&[
            source_resident_bytes,
            view_reserved_bytes,
            METADATA_WORKING_BYTES,
            private_copy
                .as_ref()
                .map_or(0, |bytes| bytes.load(Ordering::Relaxed)),
            jpeg_workspace.map_or(0, |workspace| workspace.bytes),
            codec_workspace.map_or(0, |workspace| workspace.decode_extra_bytes),
            decoder.total_bytes(),
        ])?;
        scoped_native_budget(
            encoded_dimensions,
            decoded_color,
            decoder.total_bytes(),
            orientation,
            source_resident_bytes,
            view_reserved_bytes,
            max_output_bytes,
        )?;
    }
    if matches!(
        decoded_color,
        image::ColorType::Rgb16 | image::ColorType::Rgba16
    ) {
        high_precision_decode_budget(
            encoded_dimensions,
            decoded_color,
            decoder.total_bytes(),
            orientation,
            source_resident_bytes,
            view_reserved_bytes,
            profile.as_ref().map_or(0, Vec::len),
            max_output_bytes,
        )?;
    } else if matches!(
        decoded_color,
        image::ColorType::L16 | image::ColorType::La16
    ) {
        gray16_decode_budget(
            encoded_dimensions,
            decoded_color,
            decoder.total_bytes(),
            orientation,
            source_resident_bytes,
            view_reserved_bytes,
            max_output_bytes,
        )?;
    }
    // 与 ImageReader::decode 一样，把输出缓冲计入解码器的分配预算。
    let mut limits = crate::export::decode_limits();
    if scoped {
        limits.reserve(source_resident_bytes)?;
        limits.reserve(view_reserved_bytes)?;
        limits.reserve(METADATA_WORKING_BYTES)?;
        limits.reserve(
            private_copy
                .as_ref()
                .map_or(0, |bytes| bytes.load(Ordering::Relaxed)),
        )?;
        limits.reserve(jpeg_workspace.map_or(0, |workspace| workspace.bytes))?;
    }
    limits.reserve(decoder.total_bytes())?;
    if codec_workspace.is_some_and(|workspace| {
        limits
            .max_alloc
            .is_some_and(|available| available < workspace.backend_limit_floor)
    }) {
        return Err(shared_budget_error());
    }
    decoder.set_limits(limits)?;
    let mut decoded = DynamicImage::from_decoder(decoder)?;
    check_cancel(cancel)?;
    if scoped {
        scoped_native_budget(
            encoded_dimensions,
            decoded_color,
            dynamic_capacity(&decoded)?,
            orientation,
            source_resident_bytes,
            0,
            max_output_bytes,
        )?;
    }
    let native_capacity = match &decoded {
        DynamicImage::ImageRgb16(pixels) => Some(pixels.as_raw().capacity()),
        DynamicImage::ImageRgba16(pixels) => Some(pixels.as_raw().capacity()),
        _ => None,
    };
    if let Some(capacity) = native_capacity {
        let bytes = (capacity as u64).checked_mul(2).ok_or_else(|| {
            tr!(
                "高精度缓冲容量溢出",
                "High-precision buffer capacity overflow"
            )
        })?;
        high_precision_decode_budget(
            encoded_dimensions,
            decoded_color,
            bytes,
            orientation,
            source_resident_bytes,
            0,
            profile.as_ref().map_or(0, Vec::len),
            max_output_bytes,
        )?;
    } else if matches!(
        decoded_color,
        image::ColorType::L16 | image::ColorType::La16
    ) {
        gray16_decode_budget(
            encoded_dimensions,
            decoded_color,
            dynamic_capacity(&decoded)?,
            orientation,
            source_resident_bytes,
            0,
            max_output_bytes,
        )?;
    }
    decoded.apply_orientation(orientation);
    check_cancel(cancel)?;
    if scoped {
        check_shared_budget(&[
            source_resident_bytes,
            dynamic_capacity(&decoded)?,
            METADATA_WORKING_BYTES,
        ])?;
    }
    let managed = match decoded {
        DynamicImage::ImageLuma16(pixels) => manage_gray16(
            &pixels,
            source,
            profile.as_deref(),
            source_resident_bytes,
            max_output_bytes,
            cancel,
        )?,
        DynamicImage::ImageLumaA16(pixels) => manage_gray16(
            &pixels,
            source,
            profile.as_deref(),
            source_resident_bytes,
            max_output_bytes,
            cancel,
        )?,
        DynamicImage::ImageRgb16(pixels) => manage_rgb16(
            &pixels,
            3,
            source,
            profile.as_deref(),
            source_resident_bytes,
            max_output_bytes,
            cancel,
        )?,
        DynamicImage::ImageRgba16(pixels) => manage_rgb16(
            &pixels,
            4,
            source,
            profile.as_deref(),
            source_resident_bytes,
            max_output_bytes,
            cancel,
        )?,
        decoded => {
            if scoped {
                let conversion_bytes = if matches!(&decoded, DynamicImage::ImageRgba8(_)) {
                    0
                } else {
                    output_bytes
                };
                check_shared_budget(&[
                    source_resident_bytes,
                    dynamic_capacity(&decoded)?,
                    conversion_bytes,
                    METADATA_WORKING_BYTES,
                ])?;
            }
            let rgba = decoded.into_rgba8();
            let display_dimensions = rgba.dimensions();
            let mut bgra = rgba.into_raw();
            for pixel in bgra.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            if scoped {
                scoped_color_budget(
                    display_dimensions,
                    bgra.capacity() as u64,
                    source_resident_bytes,
                    profile.as_ref().map_or(0, Vec::len),
                )?;
            }
            crate::image_color_management::manage(
                Image {
                    width: display_dimensions.0,
                    height: display_dimensions.1,
                    bgra,
                    readback_ms: 0.,
                },
                source,
                profile.as_deref(),
                cancel,
            )?
        }
    };
    if scoped {
        // SourceMetadata可能短时同时持有原配置、复制Vec和新Arc；字段解析工作区独立预留。
        let copies = (profile.as_ref().map_or(0, Vec::len) as u64)
            .checked_mul(2)
            .ok_or_else(shared_budget_error)?;
        check_shared_budget(&[
            source_resident_bytes,
            managed.image.bgra.capacity() as u64,
            copies,
            METADATA_WORKING_BYTES,
            managed
                .warning
                .as_ref()
                .map_or(0, |warning| warning.capacity() as u64),
        ])?;
    }
    let display_dimensions = (managed.image.width, managed.image.height);
    let metadata_source =
        crate::image_export_metadata::SourceMetadata::new(managed.metadata, profile.as_deref())?;
    let metadata_source = match supported_exif {
        Some(exif) => metadata_source.with_exif(exif),
        None => metadata_source,
    };
    if scoped {
        check_shared_budget(&[
            input_reserved_bytes,
            managed.image.bgra.capacity() as u64,
            metadata_source.retained_bytes() as u64,
            managed
                .warning
                .as_ref()
                .map_or(0, |warning| warning.capacity() as u64),
        ])?;
    }
    check_cancel(cancel)?;
    Ok(DecodedImage {
        image: managed.image,
        warning: managed.warning,
        metadata_source: Some(std::sync::Arc::new(metadata_source)),
        metadata: ImageMetadata {
            format: format.into(),
            encoded_dimensions: Some(encoded_dimensions),
            display_dimensions,
            orientation: Some(orientation),
            encoded_bytes: Some(bytes.len() as u64),
            decoded_color: Some(decoded_color),
            details: Some(managed.metadata),
        },
    })
}

fn dynamic_capacity(image: &DynamicImage) -> Result<u64> {
    let (capacity, sample_bytes) = match image {
        DynamicImage::ImageLuma8(pixels) => (pixels.as_raw().capacity(), 1u64),
        DynamicImage::ImageLumaA8(pixels) => (pixels.as_raw().capacity(), 1),
        DynamicImage::ImageRgb8(pixels) => (pixels.as_raw().capacity(), 1),
        DynamicImage::ImageRgba8(pixels) => (pixels.as_raw().capacity(), 1),
        DynamicImage::ImageLuma16(pixels) => (pixels.as_raw().capacity(), 2),
        DynamicImage::ImageLumaA16(pixels) => (pixels.as_raw().capacity(), 2),
        DynamicImage::ImageRgb16(pixels) => (pixels.as_raw().capacity(), 2),
        DynamicImage::ImageRgba16(pixels) => (pixels.as_raw().capacity(), 2),
        DynamicImage::ImageRgb32F(pixels) => (pixels.as_raw().capacity(), 4),
        DynamicImage::ImageRgba32F(pixels) => (pixels.as_raw().capacity(), 4),
        _ => {
            return Err(tr!(
                "共享预算读取暂不支持此像素类型",
                "Shared-budget reading does not support this pixel type yet"
            )
            .into())
        }
    };
    (capacity as u64)
        .checked_mul(sample_bytes)
        .ok_or_else(shared_budget_error)
}

fn scoped_native_budget(
    dimensions: (u32, u32),
    color: image::ColorType,
    native_bytes: u64,
    orientation: image::metadata::Orientation,
    source_resident_bytes: u64,
    view_reserved_bytes: u64,
    max_output_bytes: u64,
) -> Result<()> {
    if !matches!(
        color,
        image::ColorType::L8
            | image::ColorType::La8
            | image::ColorType::Rgb8
            | image::ColorType::Rgba8
            | image::ColorType::L16
            | image::ColorType::La16
            | image::ColorType::Rgb16
            | image::ColorType::Rgba16
            | image::ColorType::Rgb32F
            | image::ColorType::Rgba32F
    ) {
        return Err(tr!(
            "共享预算读取暂不支持此像素类型",
            "Shared-budget reading does not support this pixel type yet"
        )
        .into());
    }
    let output = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(shared_budget_error)?;
    if output > max_output_bytes {
        return Err(shared_budget_error());
    }
    check_shared_budget(&[
        source_resident_bytes,
        view_reserved_bytes,
        native_bytes,
        METADATA_WORKING_BYTES,
    ])?;
    if matches!(
        orientation,
        image::metadata::Orientation::Rotate90
            | image::metadata::Orientation::Rotate270
            | image::metadata::Orientation::Rotate90FlipH
            | image::metadata::Orientation::Rotate270FlipH
    ) {
        check_shared_budget(&[
            source_resident_bytes,
            native_bytes,
            native_bytes,
            METADATA_WORKING_BYTES,
        ])?;
    }
    if color != image::ColorType::Rgba8 {
        check_shared_budget(&[
            source_resident_bytes,
            native_bytes,
            output,
            METADATA_WORKING_BYTES,
        ])?;
    }
    Ok(())
}

fn scoped_color_budget(
    dimensions: (u32, u32),
    pixels_capacity: u64,
    source_resident_bytes: u64,
    profile_bytes: usize,
) -> Result<()> {
    check_shared_budget(&[
        source_resident_bytes,
        pixels_capacity,
        METADATA_WORKING_BYTES,
    ])?;
    if profile_bytes != 0 {
        let output = u64::from(dimensions.0)
            .checked_mul(u64::from(dimensions.1))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(shared_budget_error)?;
        let rows = u64::from(dimensions.0)
            .checked_mul(6)
            .ok_or_else(shared_budget_error)?;
        check_shared_budget(&[
            source_resident_bytes,
            pixels_capacity,
            output,
            rows,
            profile_bytes as u64,
            include_bytes!("../assets/color/srgb.icc").len() as u64,
            METADATA_WORKING_BYTES,
        ])?;
    }
    Ok(())
}

fn high_precision_decode_budget(
    dimensions: (u32, u32),
    color: image::ColorType,
    native_bytes: u64,
    orientation: image::metadata::Orientation,
    source_resident_bytes: u64,
    view_reserved_bytes: u64,
    profile_bytes: usize,
    max_output_bytes: u64,
) -> Result<()> {
    let channels = match color {
        image::ColorType::Rgb16 => 3,
        image::ColorType::Rgba16 => 4,
        _ => {
            return Err(tr!(
                "高精度颜色输入类型无效",
                "Invalid high-precision color input type"
            )
            .into())
        }
    };
    let expected = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(channels as u64 * 2))
        .ok_or_else(|| {
            tr!(
                "高精度像素容量溢出",
                "High-precision pixel capacity overflow"
            )
        })?;
    if native_bytes < expected {
        return Err(tr!(
            "高精度像素容量小于所需尺寸",
            "High-precision pixel capacity is smaller than its dimensions"
        )
        .into());
    }
    let native_resident = source_resident_bytes
        .checked_add(native_bytes)
        .ok_or_else(|| {
            tr!(
                "高精度缓冲容量溢出",
                "High-precision buffer capacity overflow"
            )
        })?;
    if native_resident
        .checked_add(view_reserved_bytes)
        .is_none_or(|bytes| bytes > MAX_DECODE_BYTES)
    {
        return Err(tr!(
            "高精度解码超过256MiB可见缓冲预算",
            "High-precision decoding exceeds the 256 MiB visible-buffer budget"
        )
        .into());
    }
    // 四分之一方向变换在原生位深上新建整图；分段视图已随decoder释放。
    if matches!(
        orientation,
        image::metadata::Orientation::Rotate90FlipH
            | image::metadata::Orientation::Rotate90
            | image::metadata::Orientation::Rotate270FlipH
            | image::metadata::Orientation::Rotate270
    ) && native_resident
        .checked_add(native_bytes)
        .is_none_or(|bytes| bytes > MAX_DECODE_BYTES)
    {
        return Err(tr!(
            "高精度方向转换超过256MiB可见缓冲预算",
            "High-precision orientation exceeds the 256 MiB visible-buffer budget"
        )
        .into());
    }
    crate::image_color_transform::check_rgb16_budget(
        dimensions,
        channels,
        native_resident,
        profile_bytes,
        max_output_bytes,
    )?;
    Ok(())
}

fn manage_rgb16<P: image::Pixel<Subpixel = u16>>(
    pixels: &image::ImageBuffer<P, Vec<u16>>,
    channels: usize,
    source: crate::image_container_metadata::Summary,
    profile: Option<&[u8]>,
    source_resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<crate::image_color_management::ManagedImage> {
    let native_capacity = (pixels.as_raw().capacity() as u64)
        .checked_mul(2)
        .ok_or_else(|| {
            tr!(
                "高精度缓冲容量溢出",
                "High-precision buffer capacity overflow"
            )
        })?;
    let resident = source_resident_bytes
        .checked_add(native_capacity)
        .ok_or_else(|| {
            tr!(
                "高精度缓冲容量溢出",
                "High-precision buffer capacity overflow"
            )
        })?;
    crate::image_color_management::manage_rgb16(
        pixels.as_raw(),
        pixels.dimensions(),
        channels,
        source,
        profile,
        resident,
        max_output_bytes,
        cancel,
    )
    .map_err(Into::into)
}

fn gray16_decode_budget(
    dimensions: (u32, u32),
    color: image::ColorType,
    native_bytes: u64,
    orientation: image::metadata::Orientation,
    source_resident_bytes: u64,
    view_reserved_bytes: u64,
    max_output_bytes: u64,
) -> Result<()> {
    let channels = match color {
        image::ColorType::L16 => 1u64,
        image::ColorType::La16 => 2,
        _ => return Err(tr!("16位灰度类型无效", "Invalid 16-bit grayscale type").into()),
    };
    check_dimensions(dimensions, native_bytes)?;
    let pixels = u64::from(dimensions.0) * u64::from(dimensions.1);
    let output = pixels.checked_mul(4).ok_or_else(shared_budget_error)?;
    let expected = pixels
        .checked_mul(channels * 2)
        .ok_or_else(shared_budget_error)?;
    if native_bytes < expected || output > max_output_bytes {
        return Err(shared_budget_error());
    }
    check_shared_budget(&[
        source_resident_bytes,
        native_bytes,
        view_reserved_bytes,
        METADATA_WORKING_BYTES,
    ])?;
    if matches!(
        orientation,
        image::metadata::Orientation::Rotate90FlipH
            | image::metadata::Orientation::Rotate90
            | image::metadata::Orientation::Rotate270FlipH
            | image::metadata::Orientation::Rotate270
    ) {
        check_shared_budget(&[
            source_resident_bytes,
            native_bytes,
            native_bytes,
            METADATA_WORKING_BYTES,
        ])?;
    }
    check_shared_budget(&[
        source_resident_bytes,
        native_bytes,
        output,
        METADATA_WORKING_BYTES,
    ])
}

/// 灰度与alpha各自按完整WORD范围量化；只创建一份直alpha BGRA8，不扩成RGBA16。
fn quantize_gray16<P: image::Pixel<Subpixel = u16>>(
    pixels: &image::ImageBuffer<P, Vec<u16>>,
    source_resident_bytes: u64,
    max_output_bytes: u64,
    mut canceled: impl FnMut() -> bool,
) -> Result<Image> {
    let mut checkpoint = || -> Result<()> {
        if canceled() {
            Err(tr!("图片读取已取消", "Image reading canceled").into())
        } else {
            Ok(())
        }
    };
    checkpoint()?;
    let channels = usize::from(P::CHANNEL_COUNT);
    let color = match channels {
        1 => image::ColorType::L16,
        2 => image::ColorType::La16,
        _ => return Err(tr!("16位灰度类型无效", "Invalid 16-bit grayscale type").into()),
    };
    let dimensions = pixels.dimensions();
    let native_bytes = (pixels.as_raw().capacity() as u64)
        .checked_mul(2)
        .ok_or_else(shared_budget_error)?;
    gray16_decode_budget(
        dimensions,
        color,
        native_bytes,
        image::metadata::Orientation::NoTransforms,
        source_resident_bytes,
        0,
        max_output_bytes,
    )?;
    let count = u64::from(dimensions.0) * u64::from(dimensions.1);
    if count.checked_mul(channels as u64) != Some(pixels.as_raw().len() as u64) {
        return Err(tr!(
            "16位灰度长度与尺寸不符",
            "16-bit grayscale length does not match dimensions"
        )
        .into());
    }
    let output_bytes = usize::try_from(count * 4).map_err(|_| shared_budget_error())?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(output_bytes)?;
    check_shared_budget(&[
        source_resident_bytes,
        native_bytes,
        bgra.capacity() as u64,
        METADATA_WORKING_BYTES,
    ])?;
    bgra.resize(output_bytes, 0);
    let quantize = |value: u16| ((u32::from(value) * 255 + 32_767) / 65_535) as u8;
    for (input, output) in pixels
        .as_raw()
        .chunks_exact(dimensions.0 as usize * channels)
        .zip(bgra.chunks_exact_mut(dimensions.0 as usize * 4))
    {
        checkpoint()?;
        for (source, target) in input.chunks_exact(channels).zip(output.chunks_exact_mut(4)) {
            let gray = quantize(source[0]);
            let alpha = if channels == 2 {
                quantize(source[1])
            } else {
                255
            };
            target.copy_from_slice(&[gray, gray, gray, alpha]);
        }
    }
    checkpoint()?;
    Ok(Image {
        width: dimensions.0,
        height: dimensions.1,
        bgra,
        readback_ms: 0.,
    })
}

fn manage_gray16<P: image::Pixel<Subpixel = u16>>(
    pixels: &image::ImageBuffer<P, Vec<u16>>,
    source: crate::image_container_metadata::Summary,
    profile: Option<&[u8]>,
    source_resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<crate::image_color_management::ManagedImage> {
    let image = quantize_gray16(pixels, source_resident_bytes, max_output_bytes, || {
        cancel.load(Ordering::Relaxed)
    })?;
    check_cancel(cancel)?;
    // 继续使用来源灰度/未检查事实；灰度ICC保持未应用，不把复制出的RGB冒充RGB设备域。
    crate::image_color_management::manage(image, source, profile, cancel).map_err(Into::into)
}

fn check_dimensions((width, height): (u32, u32), native_bytes: u64) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(tr!("图片尺寸为空", "Image dimensions are empty").into());
    }
    if width > crate::export::MAX_DECODE_SIDE || height > crate::export::MAX_DECODE_SIDE {
        return Err(tr!(
            "图片单边超过 65535 像素上限",
            "Image side exceeds 65535 pixels"
        )
        .into());
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_DECODE_PIXELS {
        return Err(tr!(
            "图片超过 3200 万像素上限",
            "Image exceeds the 32-megapixel limit"
        )
        .into());
    }
    if native_bytes > MAX_DECODE_BYTES || pixels * 4 > MAX_DECODE_BYTES {
        return Err(tr!(
            "图片超过 256 MiB 解码上限",
            "Image exceeds the 256 MiB decode limit"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, ImageFormat, RgbImage};
    use std::io::Cursor;

    const SVG_SAMPLE: &[u8] = include_bytes!("../tests/data/image_decode/offline-vector.svg");

    #[test]
    fn image_decode_svg_file_and_bytes_share_dispatch_before_platform_brand_detection() {
        use crate::{image_color_management::ColorConversion, image_metadata::SourceFormat};
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/image_decode/offline-vector.svg"
        ));
        assert_eq!(&SVG_SAMPLE[4..8], b"ftyp");
        let file = decode_path(path).unwrap();
        let bytes = decode_bytes(SVG_SAMPLE).unwrap();
        assert_eq!(file.image.bgra, bytes.image.bgra);
        assert_eq!(file.metadata.format, SourceFormat::Svg);
        assert_eq!(file.metadata.encoded_dimensions, None);
        assert_eq!(file.metadata.orientation, None);
        assert_eq!(file.metadata.display_dimensions, (3, 2));
        assert_eq!(&file.image.bgra[..4], &[80, 32, 208, 255]);
        assert_eq!(file.image.bgra[7], 128);
        assert_eq!(file.metadata.encoded_bytes, Some(SVG_SAMPLE.len() as u64));
        assert_eq!(
            file.metadata_source.unwrap().facts().color_conversion,
            ColorConversion::RenderedSrgb
        );
        let mut bom = vec![0xef, 0xbb, 0xbf];
        bom.extend_from_slice(SVG_SAMPLE);
        assert_eq!(decode_bytes(&bom).unwrap().image.bgra, bytes.image.bgra);
    }

    #[test]
    fn image_decode_svg_requested_canvas_rerenders_vectors_with_transparent_meet() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/image_decode/offline-vector.svg"
        ));
        let decoded =
            decode_svg_path_with_budget_cancellable(path, (6, 8), 192, &NEVER_CANCELLED).unwrap();
        assert_eq!(decoded.metadata.display_dimensions, (6, 8));
        assert_eq!(decoded.image.bgra.len(), 192);
        assert!(decoded.image.bgra[..6 * 2 * 4]
            .iter()
            .all(|byte| *byte == 0));
        assert_eq!(&decoded.image.bgra[6 * 2 * 4..][..4], &[80, 32, 208, 255]);
        assert!(
            decode_svg_path_with_budget_cancellable(path, (6, 8), 191, &NEVER_CANCELLED).is_err()
        );
        assert!(
            decode_svg_path_with_budget_cancellable(path, (0, 8), 192, &NEVER_CANCELLED).is_err()
        );
    }

    #[test]
    fn image_decode_svg_keeps_icon_and_prefetch_contracts_and_cancellation() {
        assert!(
            decode_icon_png_with_budget(SVG_SAMPLE, MAX_DECODE_BYTES, &NEVER_CANCELLED).is_err()
        );
        assert!(
            decode_icon_dib_with_budget(SVG_SAMPLE, MAX_DECODE_BYTES, &NEVER_CANCELLED).is_err()
        );
        let error = decode_bytes_inner_scoped(
            SVG_SAMPLE,
            SVG_SAMPLE.len() as u64,
            MAX_DECODE_BYTES,
            &NEVER_CANCELLED,
            DecodeContent::File,
            Some(0),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("共享驻留") || error.contains("shared-budget"));
        assert!(decode_bytes_with_reservation(
            SVG_SAMPLE,
            MAX_DECODE_BYTES - METADATA_WORKING_BYTES,
            MAX_DECODE_BYTES,
            &NEVER_CANCELLED,
        )
        .is_err());
        let cancel = AtomicBool::new(true);
        let error = decode_svg_path_with_budget_cancellable(
            Path::new("missing-vector.svg"),
            (1, 1),
            4,
            &cancel,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("取消") || error.contains("canceled"));
        assert!(decode_bytes_cancellable(b"<invalid", &cancel).is_err());
    }

    fn gray16_png(
        samples: &[u16],
        channels: usize,
        orientation: u8,
        profile: Option<&[u8]>,
    ) -> Vec<u8> {
        use std::io::Write;
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 3, 2);
            encoder.set_color(if channels == 1 {
                png::ColorType::Grayscale
            } else {
                png::ColorType::GrayscaleAlpha
            });
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            if let Some(profile) = profile {
                let mut compressed =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                compressed.write_all(profile).unwrap();
                let mut payload = b"gray\0\0".to_vec();
                payload.extend(compressed.finish().unwrap());
                writer.write_chunk(png::chunk::iCCP, &payload).unwrap();
            }
            let data: Vec<u8> = samples
                .iter()
                .flat_map(|sample| sample.to_be_bytes())
                .collect();
            writer.write_image_data(&data).unwrap();
            let mut exif =
                b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0".to_vec();
            exif[18] = orientation;
            writer.write_chunk(png::chunk::eXIf, &exif).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn image_decode_gray16_png_keeps_existing_quantization_alpha_and_all_directions() {
        use crate::image_color_management::ColorConversion;
        use crate::image_container_metadata::ColorModel;
        let gray = [0, 128, 129, 32_768, 65_406, 65_535];
        let alpha = [65_535, 32_768, 129, 0, 65_407, 65_535];
        for channels in [1, 2] {
            let samples: Vec<u16> = gray
                .iter()
                .zip(alpha)
                .flat_map(|(&gray, alpha)| [gray, alpha].into_iter().take(channels))
                .collect();
            for orientation in 1..=8 {
                let bytes = gray16_png(&samples, channels, orientation, None);
                let decoded = decode_bytes(&bytes).unwrap();
                let mut expected = if channels == 1 {
                    DynamicImage::ImageLuma16(
                        image::ImageBuffer::from_raw(3, 2, samples.clone()).unwrap(),
                    )
                } else {
                    DynamicImage::ImageLumaA16(
                        image::ImageBuffer::from_raw(3, 2, samples.clone()).unwrap(),
                    )
                };
                expected.apply_orientation(
                    image::metadata::Orientation::from_exif(orientation).unwrap(),
                );
                let mut expected = expected.into_rgba8().into_raw();
                for pixel in expected.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                assert_eq!(decoded.image.bgra, expected);
                assert_eq!(decoded.metadata.format, ImageFormat::Png);
                assert_eq!(decoded.metadata.encoded_dimensions, Some((3, 2)));
                assert_eq!(
                    decoded.metadata.display_dimensions,
                    if orientation < 5 { (3, 2) } else { (2, 3) }
                );
                assert_eq!(decoded.metadata.orientation.unwrap().to_exif(), orientation);
                assert_eq!(
                    decoded.metadata.decoded_color,
                    Some(if channels == 1 {
                        image::ColorType::L16
                    } else {
                        image::ColorType::La16
                    })
                );
                let facts = decoded.metadata.details.unwrap();
                assert_eq!(facts.source.color_model, ColorModel::Gray);
                assert_eq!(facts.source.bits_per_channel, Some(16));
                assert_eq!(facts.color_conversion, ColorConversion::AssumedSrgb);
                if channels == 2 && orientation == 1 {
                    assert_eq!(&decoded.image.bgra[12..16], &[128, 128, 128, 0]);
                }
            }
        }

        let samples: Vec<u16> = (0..=u16::MAX).collect();
        let luma = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(256, 256, samples).unwrap();
        let actual = quantize_gray16(&luma, 0, 256 * 256 * 4, || false).unwrap();
        let expected = DynamicImage::ImageLuma16(luma).into_rgba8().into_raw();
        assert_eq!(actual.bgra, expected);

        let samples: Vec<u16> = (0..=u16::MAX)
            .flat_map(|value| [value, u16::MAX - value])
            .collect();
        let luma_alpha =
            image::ImageBuffer::<image::LumaA<u16>, _>::from_raw(256, 256, samples).unwrap();
        let actual = quantize_gray16(&luma_alpha, 0, 256 * 256 * 4, || false).unwrap();
        let expected = DynamicImage::ImageLumaA16(luma_alpha)
            .into_rgba8()
            .into_raw();
        assert_eq!(actual.bgra, expected);
    }

    #[test]
    fn image_decode_gray16_profiles_stay_unapplied_without_changing_pixels() {
        use crate::image_color_management::{ColorConversion, UnappliedReason};
        let samples = [
            128, 0, 129, 32_768, 32_768, 65_535, 65_406, 129, 65_407, 65_535, 65_535, 0,
        ];
        let ordinary = decode_bytes(&gray16_png(&samples, 2, 6, None)).unwrap();
        for (profile, reason) in [
            (&b"not an ICC profile"[..], UnappliedReason::InvalidProfile),
            (
                &include_bytes!("../assets/color/srgb.icc")[..],
                UnappliedReason::UnsupportedSource,
            ),
        ] {
            let decoded = decode_bytes(&gray16_png(&samples, 2, 6, Some(profile))).unwrap();
            assert_eq!(decoded.image.bgra, ordinary.image.bgra);
            assert_eq!(decoded.metadata.display_dimensions, (2, 3));
            let expected = ColorConversion::EmbeddedUnapplied(reason);
            assert_eq!(decoded.metadata.details.unwrap().color_conversion, expected);
            assert_eq!(
                decoded
                    .metadata_source
                    .as_ref()
                    .unwrap()
                    .facts()
                    .color_conversion,
                expected
            );
            assert!(decoded.warning.is_some());
        }
    }

    #[test]
    fn image_decode_gray16_budget_checks_source_native_view_orientation_and_output() {
        use image::metadata::Orientation;
        for (color, native) in [(image::ColorType::L16, 4), (image::ColorType::La16, 8)] {
            let resident = MAX_DECODE_BYTES - METADATA_WORKING_BYTES - native - 8;
            gray16_decode_budget(
                (2, 1),
                color,
                native,
                Orientation::NoTransforms,
                resident,
                0,
                8,
            )
            .unwrap();
            gray16_decode_budget((2, 1), color, native, Orientation::Rotate90, resident, 0, 8)
                .unwrap();
            assert!(gray16_decode_budget(
                (2, 1),
                color,
                native,
                Orientation::NoTransforms,
                resident + 1,
                0,
                8
            )
            .is_err());
            assert!(gray16_decode_budget(
                (2, 1),
                color,
                native,
                Orientation::NoTransforms,
                resident,
                9,
                8
            )
            .is_err());
            assert!(gray16_decode_budget(
                (2, 1),
                color,
                native - 1,
                Orientation::NoTransforms,
                resident,
                0,
                8
            )
            .is_err());
            assert!(gray16_decode_budget(
                (2, 1),
                color,
                native,
                Orientation::NoTransforms,
                resident,
                0,
                7
            )
            .is_err());
        }
        let mut words = vec![129u16, 65_535];
        words.reserve_exact(64);
        let pixels = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(2, 1, words).unwrap();
        let resident = MAX_DECODE_BYTES - METADATA_WORKING_BYTES - 4 - 8;
        assert!(quantize_gray16(&pixels, resident, 8, || false).is_err());
    }

    #[test]
    fn image_decode_gray16_quantization_cancels_before_allocation_and_between_rows() {
        let pixels =
            image::ImageBuffer::<image::LumaA<u16>, _>::from_raw(2, 3, vec![32_768; 12]).unwrap();
        let error = quantize_gray16(&pixels, MAX_DECODE_BYTES, 24, || true)
            .err()
            .unwrap();
        assert!(error.to_string().contains("取消") || error.to_string().contains("canceled"));
        let mut checkpoints = 0;
        let result = quantize_gray16(&pixels, 0, 24, || {
            checkpoints += 1;
            checkpoints == 3
        });
        assert!(result.is_err());
        assert_eq!(checkpoints, 3);
    }

    #[test]
    fn image_decode_shared_budget_checks_before_read_and_old_new_input_growth() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("resident rejection must happen before reading");
            }
        }
        assert!(read_input_with_resident(
            &mut NoRead,
            &NEVER_CANCELLED,
            MAX_DECODE_BYTES - METADATA_WORKING_BYTES - 64 * 1024 + 1
        )
        .is_err());
        let bytes = vec![0; 128 * 1024 + 1];
        let resident = MAX_DECODE_BYTES - METADATA_WORKING_BYTES - 64 * 1024 - 256 * 1024;
        assert!(read_input_with_resident(
            &mut Cursor::new(bytes.clone()),
            &NEVER_CANCELLED,
            resident
        )
        .is_err());
        assert_eq!(
            read_input_with_resident(
                &mut Cursor::new(&bytes),
                &NEVER_CANCELLED,
                resident - 128 * 1024
            )
            .unwrap(),
            bytes
        );
        let cancel = AtomicBool::new(false);
        struct CancelRead<'a>(&'a AtomicBool);
        impl Read for CancelRead<'_> {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                self.0.store(true, Ordering::Relaxed);
                Err(std::io::Error::other("source failure"))
            }
        }
        let error = read_input_with_resident(&mut CancelRead(&cancel), &cancel, 0)
            .err()
            .unwrap();
        assert!(error.to_string().contains("取消") || error.to_string().contains("canceled"));
    }

    #[test]
    fn image_decode_shared_budget_reserves_native_conversion_and_quarter_turn() {
        use image::metadata::Orientation;
        let resident = MAX_DECODE_BYTES - METADATA_WORKING_BYTES - 6 - 8;
        scoped_native_budget(
            (2, 1),
            image::ColorType::Rgb8,
            6,
            Orientation::NoTransforms,
            resident,
            0,
            8,
        )
        .unwrap();
        assert!(scoped_native_budget(
            (2, 1),
            image::ColorType::Rgb8,
            6,
            Orientation::NoTransforms,
            resident + 1,
            0,
            8
        )
        .is_err());
        assert!(scoped_native_budget(
            (2, 1),
            image::ColorType::Rgb8,
            64,
            Orientation::NoTransforms,
            resident,
            0,
            8
        )
        .is_err());
        let resident = MAX_DECODE_BYTES - METADATA_WORKING_BYTES - 16;
        scoped_native_budget(
            (2, 1),
            image::ColorType::Rgba8,
            8,
            Orientation::Rotate90,
            resident,
            0,
            8,
        )
        .unwrap();
        assert!(scoped_native_budget(
            (2, 1),
            image::ColorType::Rgba8,
            8,
            Orientation::Rotate90,
            resident + 1,
            0,
            8
        )
        .is_err());
    }

    #[test]
    fn image_decode_shared_budget_counts_color_output_rows_and_profile_before_cmm() {
        let profile = include_bytes!("../assets/color/srgb.icc").len();
        let resident = MAX_DECODE_BYTES
            - METADATA_WORKING_BYTES
            - 8
            - 8
            - 12
            - profile as u64
            - profile as u64;
        scoped_color_budget((2, 1), 8, resident, profile).unwrap();
        assert!(scoped_color_budget((2, 1), 8, resident + 1, profile).is_err());
        assert!(scoped_color_budget((2, 1), 9, resident, profile).is_err());
    }

    #[test]
    fn image_decode_shared_budget_keeps_png_bmp_pixels_alpha_and_word_orientation() {
        let rgba = image::RgbaImage::from_raw(2, 1, vec![23, 61, 107, 80, 1, 2, 3, 255]).unwrap();
        for format in [ImageFormat::Png, ImageFormat::Bmp] {
            let mut bytes = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(rgba.clone())
                .write_to(&mut bytes, format)
                .unwrap();
            let bytes = bytes.into_inner();
            let decoded = decode_bytes_inner_scoped(
                &bytes,
                bytes.capacity() as u64,
                8,
                &NEVER_CANCELLED,
                DecodeContent::File,
                Some(32 * 1024 * 1024),
            )
            .unwrap();
            assert_eq!(decoded.image.bgra, decode_bytes(&bytes).unwrap().image.bgra);
            assert_eq!(decoded.metadata.display_dimensions, (2, 1));
        }
        let bytes = png16_with_profile_and_orientation(None);
        let decoded = decode_bytes_inner_scoped(
            &bytes,
            bytes.capacity() as u64,
            8,
            &NEVER_CANCELLED,
            DecodeContent::File,
            Some(32 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(decoded.metadata.display_dimensions, (1, 2));
        assert_eq!(
            decoded.metadata.decoded_color,
            Some(image::ColorType::Rgba16)
        );
        assert_eq!(decoded.image.bgra, [0, 0, 0, 128, 255, 255, 255, 255]);
        assert!(decode_bytes_inner_scoped(
            &bytes,
            bytes.capacity() as u64,
            8,
            &NEVER_CANCELLED,
            DecodeContent::File,
            Some(MAX_DECODE_BYTES)
        )
        .is_err());
        assert!(decode_bytes_inner_scoped(
            &bytes,
            bytes.capacity() as u64,
            7,
            &NEVER_CANCELLED,
            DecodeContent::File,
            Some(0)
        )
        .is_err());
    }

    #[test]
    fn image_decode_thumbnail_shared_keeps_gif_webp_tiff_and_float_pixels() {
        let rgba = image::RgbaImage::from_raw(2, 1, vec![23, 61, 107, 0, 1, 2, 3, 255]).unwrap();
        let float = image::ImageBuffer::<image::Rgba<f32>, _>::from_raw(
            2,
            1,
            vec![0.25, 0.5, 1., 0., 1., 0.5, 0.25, 0.5],
        )
        .unwrap();
        for (image, format) in [
            (DynamicImage::ImageRgba8(rgba.clone()), ImageFormat::Gif),
            (DynamicImage::ImageRgba8(rgba.clone()), ImageFormat::WebP),
            (DynamicImage::ImageRgba8(rgba), ImageFormat::Tiff),
            (DynamicImage::ImageRgba32F(float), ImageFormat::Tiff),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image.write_to(&mut encoded, format).unwrap();
            let encoded = encoded.into_inner();
            let ordinary = decode_bytes(&encoded).unwrap();
            let shared = decode_bytes_inner_with_budget(
                &encoded,
                encoded.capacity() as u64,
                8,
                &NEVER_CANCELLED,
                DecodeContent::File,
                DecodeBudget::Shared(32 * 1024 * 1024),
            )
            .unwrap();
            assert_eq!(shared.image.bgra, ordinary.image.bgra);
            assert_eq!(
                shared.metadata.decoded_color,
                ordinary.metadata.decoded_color
            );
            assert!(decode_bytes_inner_with_budget(
                &encoded,
                encoded.capacity() as u64,
                8,
                &NEVER_CANCELLED,
                DecodeContent::File,
                DecodeBudget::Shared(MAX_DECODE_BYTES),
            )
            .is_err());
        }
    }

    #[test]
    fn image_decode_thumbnail_shared_icon_preserves_container_alpha_and_orientation() {
        let exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let png = png_with_chunk(png::chunk::eXIf, exif, true);
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&[3, 2, 1, 0, 6, 5, 4, 0, 0x40, 0, 0, 0]);
        for payload in [&png, &dib] {
            for kind in [1, 2] {
                let bytes = single_icon(payload, kind);
                let ordinary = decode_bytes(&bytes).unwrap();
                let shared = decode_bytes_inner_with_budget(
                    &bytes,
                    bytes.capacity() as u64,
                    8,
                    &NEVER_CANCELLED,
                    DecodeContent::File,
                    DecodeBudget::Shared(32 * 1024 * 1024),
                )
                .unwrap();
                assert_eq!(shared.image.bgra, ordinary.image.bgra);
                assert_eq!(shared.metadata.display_dimensions, (2, 1));
                assert_eq!(shared.metadata.orientation.unwrap().to_exif(), 1);
                assert_eq!(shared.metadata.format, ImageFormat::Ico);
            }
        }
    }

    #[test]
    fn image_decode_thumbnail_shared_jpeg_keeps_wider_sampling_and_trailing_data() {
        for factors in [&[0x41, 0x11, 0x11][..], &[0x13, 0x11, 0x11], &[0x44]] {
            let mut bytes = jpeg_workspace_header(17, 9, factors);
            bytes.extend_from_slice(b"trailing data");
            let workspace = jpeg_workspace_with_sampling(&bytes, &NEVER_CANCELLED, false).unwrap();
            assert_eq!(workspace.dimensions, (17, 9));
            assert!(workspace.bytes > 4 * 1024 * 1024);
            assert!(jpeg_workspace(&bytes, &NEVER_CANCELLED).is_err());
        }
    }

    #[test]
    fn image_decode_thumbnail_shared_real_411_jpeg_uses_ordinary_backend() {
        // 33×9的baseline 4:1:1，四个MCU的六块均只有零DC与EOB，像素恒为中性灰。
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xdb, 0, 67, 0];
        bytes.extend_from_slice(&[1; 64]);
        bytes.extend_from_slice(&[0xff, 0xc0, 0, 17, 8, 0, 9, 0, 33, 3]);
        bytes.extend_from_slice(&[1, 0x41, 0, 2, 0x11, 0, 3, 0x11, 0]);
        bytes.extend_from_slice(&[0xff, 0xc4, 0, 38]);
        for kind in [0, 0x10] {
            bytes.extend_from_slice(&[kind, 1]);
            bytes.extend_from_slice(&[0; 15]);
            bytes.push(0);
        }
        bytes.extend_from_slice(&[0xff, 0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0]);
        bytes.extend_from_slice(&[0; 6]);
        bytes.extend_from_slice(&[0xff, 0xd9]);
        let ordinary = decode_bytes(&bytes).unwrap();
        let shared = decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            33 * 9 * 4,
            &NEVER_CANCELLED,
            DecodeContent::File,
            DecodeBudget::Shared(32 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(shared.image.bgra, ordinary.image.bgra);
        assert_eq!(shared.metadata.display_dimensions, (33, 9));
        assert!(shared
            .image
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel == [128, 128, 128, 255]));
        assert!(jpeg_workspace(&bytes, &NEVER_CANCELLED).is_err());
    }

    #[test]
    fn image_decode_shared_budget_eager_copy_records_actual_capacity_and_rejects_before_copy() {
        let copied = Arc::new(AtomicU64::new(0));
        let mut reader = ScopedReader {
            inner: Cursor::new(&b"12345678"[..]),
            budget: Some(ReadEndBudget {
                length: 8,
                resident_bytes: MAX_DECODE_BYTES - 8,
                copied_capacity: copied.clone(),
                cancel: &NEVER_CANCELLED,
            }),
        };
        let mut output = Vec::new();
        assert_eq!(reader.read_to_end(&mut output).unwrap(), 8);
        assert_eq!(output, b"12345678");
        assert_eq!(copied.load(Ordering::Relaxed), output.capacity() as u64);
        let mut reader = ScopedReader {
            inner: Cursor::new(&b"12345678"[..]),
            budget: Some(ReadEndBudget {
                length: 8,
                resident_bytes: MAX_DECODE_BYTES - 7,
                copied_capacity: copied.clone(),
                cancel: &NEVER_CANCELLED,
            }),
        };
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).is_err());
        assert!(output.is_empty());
        assert_eq!(reader.inner.position(), 0);
    }

    fn jpeg_workspace_header(width: u16, height: u16, factors: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xc2];
        bytes.extend_from_slice(&(8u16 + factors.len() as u16 * 3).to_be_bytes());
        bytes.push(8);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.push(factors.len() as u8);
        for (index, factor) in factors.iter().enumerate() {
            bytes.extend_from_slice(&[index as u8 + 1, *factor, 0]);
        }
        bytes.extend_from_slice(&[0xff, 0xda]);
        bytes.extend_from_slice(&(6u16 + factors.len() as u16 * 2).to_be_bytes());
        bytes.push(factors.len() as u8);
        for index in 0..factors.len() {
            bytes.extend_from_slice(&[index as u8 + 1, 0]);
        }
        bytes.extend_from_slice(&[0, 0, 0, 0xff, 0xd9]);
        bytes
    }

    #[test]
    fn image_decode_shared_budget_jpeg_workspace_covers_padding_subsampling_and_rejects_switches() {
        let reserve = (17 + 16) * 1024 + 4 * 1024 * 1024;
        for (factors, blocks) in [
            (&[0x11, 0x11, 0x11][..], 3 * 3 * 2),
            (&[0x21, 0x11, 0x11][..], 2 * 2 * 4),
            (&[0x22, 0x11, 0x11][..], 2 * 1 * 6),
            (&[0x11, 0x11, 0x11, 0x11][..], 3 * 3 * 2 * 4 / 3),
        ] {
            let bytes = jpeg_workspace_header(17, 9, factors);
            let workspace = jpeg_workspace(&bytes, &NEVER_CANCELLED).unwrap();
            assert_eq!(workspace.dimensions, (17, 9));
            assert_eq!(workspace.bytes, reserve + blocks * 64 * 2);
        }
        let mut bytes = jpeg_workspace_header(17, 9, &[0x11, 0x11, 0x11]);
        let duplicate = bytes[2..21].to_vec();
        bytes.splice(bytes.len() - 2..bytes.len() - 2, duplicate);
        assert!(jpeg_workspace(&bytes, &NEVER_CANCELLED).is_err());
        assert!(jpeg_workspace(
            &jpeg_workspace_header(17, 9, &[0x41, 0x11, 0x11]),
            &NEVER_CANCELLED
        )
        .is_err());
        assert!(jpeg_workspace(
            &jpeg_workspace_header(17, 9, &[0x11]),
            &AtomicBool::new(true)
        )
        .is_err());
    }

    #[test]
    fn image_decode_shared_budget_real_progressive_and_cmyk_keep_pixels_and_exif() {
        for bytes in [
            &include_bytes!("../tests/data/image_decode/progressive_rgb.jpg")[..],
            &include_bytes!("../tests/data/image_decode/adobe_cmyk.jpg")[..],
        ] {
            let ordinary = decode_bytes(bytes).unwrap();
            let workspace = jpeg_workspace(bytes, &NEVER_CANCELLED).unwrap();
            assert_eq!(workspace.dimensions, ordinary.metadata.display_dimensions);
            let decoded = decode_bytes_inner_scoped(
                bytes,
                bytes.len() as u64,
                48 * 1024 * 1024,
                &NEVER_CANCELLED,
                DecodeContent::File,
                Some(32 * 1024 * 1024),
            )
            .unwrap();
            assert_eq!(decoded.image.bgra, ordinary.image.bgra);
            assert_eq!(
                decoded.metadata.decoded_color,
                ordinary.metadata.decoded_color
            );
            let exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
            let mut oriented = vec![0xff, 0xd8, 0xff, 0xe1];
            oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
            oriented.extend_from_slice(exif);
            oriented.extend_from_slice(&bytes[2..]);
            let ordinary = decode_bytes(&oriented).unwrap();
            let decoded = decode_bytes_inner_scoped(
                &oriented,
                oriented.capacity() as u64,
                48 * 1024 * 1024,
                &NEVER_CANCELLED,
                DecodeContent::File,
                Some(32 * 1024 * 1024),
            )
            .unwrap();
            assert_eq!(decoded.metadata.orientation.unwrap().to_exif(), 6);
            assert_eq!(
                decoded.metadata.display_dimensions,
                (workspace.dimensions.1, workspace.dimensions.0)
            );
            assert_eq!(decoded.image.bgra, ordinary.image.bgra);
        }
    }

    #[test]
    fn image_decode_shared_budget_path_uses_bounded_input_and_preserves_real_jpeg() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/image_decode/progressive_rgb.jpg"
        ));
        let decoded = decode_path_with_resident_budget_cancellable(
            path,
            48 * 1024 * 1024,
            32 * 1024 * 1024,
            &NEVER_CANCELLED,
        )
        .unwrap();
        assert_eq!(decoded.metadata.display_dimensions, (32, 16));
        assert_eq!(decoded.image.bgra, decode_path(path).unwrap().image.bgra);
        let error = decode_path_with_resident_budget_cancellable(
            Path::new("missing-shared-budget-source.png"),
            4,
            MAX_DECODE_BYTES,
            &NEVER_CANCELLED,
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("驻留预算") || error.to_string().contains("resident budget")
        );
    }

    fn sample() -> RgbImage {
        RgbImage::from_raw(
            3,
            2,
            vec![
                10, 30, 50, 90, 20, 70, 210, 100, 40, 70, 190, 130, 200, 230, 80, 30, 90, 220,
            ],
        )
        .unwrap()
    }

    #[test]
    fn image_decode_platform_frame_resident_is_checked_before_path_and_cancel_wins() {
        let error = decode_platform_path_frame_with_resident(
            Path::new(""),
            0,
            MAX_DECODE_BYTES,
            MAX_DECODE_BYTES,
            &NEVER_CANCELLED,
        )
        .err()
        .expect("resident limit must fail before path or native codec access");
        assert_eq!(
            error.to_string(),
            tr!(
                "图片来源与读取工作区超过256MiB预算",
                "Image source and read workspace exceed the 256 MiB budget"
            )
        );
        let error = decode_platform_path_frame_with_resident(
            Path::new(""),
            0,
            MAX_DECODE_BYTES,
            MAX_DECODE_BYTES,
            &AtomicBool::new(true),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            tr!("图片读取已取消", "Image reading canceled")
        );
    }

    #[test]
    fn image_decode_system_item_keeps_unknown_color_facts_and_has_no_save_sidecar() {
        for format in [
            platform::PlatformFormat::Avif,
            platform::PlatformFormat::Heif,
        ] {
            let frame = platform_frame(
                platform::PlatformDecoded {
                    image: Image {
                        width: 2,
                        height: 1,
                        bgra: vec![3, 2, 1, 0, 6, 5, 4, 128],
                        readback_ms: 0.,
                    },
                    frame_count: 7,
                    frame_index: 4,
                    item_id: 23,
                    is_primary: false,
                    orientation: None,
                    encoded_dimensions: None,
                    bits_per_channel: None,
                    decoded_color: None,
                    color: None,
                    managed: None,
                    metadata_source: None,
                    warning: Some("8-bit preview".into()),
                },
                format,
                123,
            );
            assert_eq!((frame.frame_count, frame.frame_index), (7, 4));
            assert_eq!((frame.item_id, frame.is_primary), (23, false));
            assert_eq!(frame.decoded.metadata.display_dimensions, (2, 1));
            assert_eq!(frame.decoded.metadata.encoded_dimensions, None);
            assert_eq!(
                frame.decoded.metadata.orientation,
                Some(image::metadata::Orientation::NoTransforms)
            );
            assert_eq!(frame.decoded.metadata.decoded_color, None);
            assert_eq!(frame.decoded.metadata.details, None);
            assert_eq!(frame.decoded.metadata.encoded_bytes, Some(123));
            assert!(frame.decoded.metadata_source.is_none());
            assert_eq!(frame.decoded.image.bgra, [3, 2, 1, 0, 6, 5, 4, 128]);
            assert_eq!(
                frame.decoded.metadata.format_name(),
                match format {
                    platform::PlatformFormat::Avif => "AVIF",
                    platform::PlatformFormat::Heif => "HEIF",
                }
            );
        }
    }

    #[test]
    fn image_decode_system_checked_canvas_and_depth_do_not_grant_color_or_exif_preservation() {
        use crate::{
            image_color_management::{ColorConversion, ManagedMetadata},
            image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
        };
        let color = platform::ColorFacts {
            container_cicp: None,
            bitstream_cicp: Some(platform::Cicp {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: true,
            }),
            sample_model: platform::SampleModel::Ycc,
            hdr_mapped: false,
        };
        let managed = ManagedMetadata {
            source: Summary {
                icc: FieldSummary {
                    status: MetadataStatus::Absent,
                    encoded_bytes: 0,
                    decoded_bytes: None,
                },
                exif: FieldSummary {
                    status: MetadataStatus::NotInspected,
                    encoded_bytes: 0,
                    decoded_bytes: None,
                },
                color_model: ColorModel::Rgb,
                bits_per_channel: Some(10),
                orientation: Some(image::metadata::Orientation::Rotate180),
            },
            profile: None,
            color_conversion: ColorConversion::SignalUnmapped,
        };
        let frame = platform_frame(
            platform::PlatformDecoded {
                image: Image {
                    width: 250,
                    height: 128,
                    bgra: vec![255; 250 * 128 * 4],
                    readback_ms: 0.,
                },
                frame_count: 1,
                frame_index: 0,
                item_id: 41,
                is_primary: true,
                orientation: Some(image::metadata::Orientation::Rotate180),
                encoded_dimensions: Some((256, 128)),
                bits_per_channel: Some(10),
                decoded_color: None,
                color: Some(color),
                managed: Some(managed),
                metadata_source: None,
                warning: Some("8-bit preview".into()),
            },
            platform::PlatformFormat::Heif,
            123,
        );
        assert_eq!(frame.color, Some(color));
        assert_eq!(frame.decoded.metadata.details, Some(managed));
        let metadata = frame.decoded.metadata;
        assert_eq!(metadata.encoded_dimensions, Some((256, 128)));
        assert_eq!(metadata.display_dimensions, (250, 128));
        assert_eq!(
            metadata.orientation,
            Some(image::metadata::Orientation::Rotate180)
        );
        assert_eq!(metadata.decoded_color, None);
        let details = metadata.details.unwrap();
        assert_eq!(
            details.source.orientation,
            Some(image::metadata::Orientation::Rotate180)
        );
        assert_eq!(details.source.bits_per_channel, Some(10));
        assert_eq!(
            details.source.icc.status,
            crate::image_container_metadata::MetadataStatus::Absent
        );
        assert_eq!(
            details.source.exif.status,
            crate::image_container_metadata::MetadataStatus::NotInspected
        );
        assert_eq!(
            details.color_conversion,
            crate::image_color_management::ColorConversion::SignalUnmapped
        );
        assert!(frame.decoded.metadata_source.is_none());
    }

    #[test]
    fn image_decode_ftyp_validation_and_cancellation_precede_native_creation() {
        let mut bytes = 16u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftypavif\0\0\0\0");
        let cancel = AtomicBool::new(true);
        let error = decode_bytes_cancellable(&bytes, &cancel).err().unwrap();
        assert!(error.to_string().contains(tr!("取消", "canceled")));
        bytes[..4].copy_from_slice(&20u32.to_be_bytes());
        let error = decode_bytes(&bytes).err().unwrap();
        assert!(error.to_string().contains("ftyp"));
    }

    #[test]
    fn image_decode_jpeg_app_bytes_do_not_shadow_the_raster_header_as_ftyp() {
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(sample())
            .write_to(&mut encoded, ImageFormat::Jpeg)
            .unwrap();
        // 一个合法的未知APP1：长度0x6674，负载以yp开头，恰好组成偏移4的ftyp。
        let length = usize::from(u16::from_be_bytes(*b"ft"));
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(b"ftyp");
        bytes.resize(4 + length, 0);
        bytes.extend_from_slice(&encoded.get_ref()[2..]);
        assert_eq!(&bytes[4..8], b"ftyp");
        assert_eq!(platform::detect(&bytes), Ok(None));
        let decoded = decode_bytes(&bytes).unwrap();
        assert_eq!(decoded.metadata.format, ImageFormat::Jpeg);
        assert_eq!(decoded.metadata.display_dimensions, (3, 2));
    }

    #[test]
    fn image_decode_baseline_headers_alpha_and_bgra() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Bmp] {
            let mut encoded = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(sample())
                .write_to(&mut encoded, format)
                .unwrap();
            let decoded = decode_bytes(encoded.get_ref()).unwrap();
            assert_eq!(decoded.metadata.format, format);
            assert_eq!(decoded.metadata.encoded_dimensions, Some((3, 2)));
            assert_eq!((decoded.image.width, decoded.image.height), (3, 2));
            assert_eq!(decoded.image.bgra.len(), 24);
            assert!(decoded.image.bgra.chunks_exact(4).all(|p| p[3] == 255));
            if format != ImageFormat::Jpeg {
                assert_eq!(&decoded.image.bgra[..4], &[50, 30, 10, 255]);
            }
        }
        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(&[4, 12, 20, 80], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert_eq!(decode_bytes(&encoded).unwrap().image.bgra, [20, 12, 4, 80]);
    }

    fn jpeg_header_segment(bytes: &[u8], wanted_marker: u8) -> Option<&[u8]> {
        if !bytes.starts_with(&[0xff, 0xd8]) {
            return None;
        }
        let mut offset = 2;
        while offset < bytes.len() {
            if bytes.get(offset) != Some(&0xff) {
                return None;
            }
            while bytes.get(offset) == Some(&0xff) {
                offset += 1;
            }
            let marker = *bytes.get(offset)?;
            offset += 1;
            if matches!(marker, 0xda | 0xd9) {
                return None;
            }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                continue;
            }
            let length_bytes = bytes.get(offset..offset + 2)?;
            let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
            if length < 2 {
                return None;
            }
            let end = offset.checked_add(length)?;
            let segment = bytes.get(offset + 2..end)?;
            if marker == wanted_marker {
                return Some(segment);
            }
            offset = end;
        }
        None
    }

    fn assert_jpeg_color_blocks(
        bytes: &[u8],
        dimensions: (u32, u32),
        centers: &[(u32, u32, [u8; 3])],
    ) {
        let decoded = decode_bytes(bytes).unwrap();
        assert_eq!(decoded.metadata.format, ImageFormat::Jpeg);
        assert_eq!(decoded.metadata.encoded_dimensions, Some(dimensions));
        assert_eq!(decoded.metadata.display_dimensions, dimensions);
        assert_eq!(
            decoded.metadata.orientation.map(|value| value.to_exif()),
            Some(1)
        );
        assert_eq!(decoded.metadata.encoded_bytes, Some(bytes.len() as u64));
        assert_eq!((decoded.image.width, decoded.image.height), dimensions);
        assert_eq!(
            decoded.image.bgra.len(),
            dimensions.0 as usize * dimensions.1 as usize * 4
        );
        assert!(decoded.image.bgra.chunks_exact(4).all(|p| p[3] == 255));
        for &(x, y, expected_rgb) in centers {
            let offset = (y as usize * dimensions.0 as usize + x as usize) * 4;
            let actual = &decoded.image.bgra[offset..offset + 4];
            let expected_bgr = [expected_rgb[2], expected_rgb[1], expected_rgb[0]];
            for (channel, expected) in expected_bgr.into_iter().enumerate() {
                assert!(
                    actual[channel].abs_diff(expected) <= 8,
                    "pixel ({x}, {y}): BGRA {actual:?}, expected RGB {expected_rgb:?}"
                );
            }
        }
    }

    #[test]
    fn image_decode_jpeg_fixture_progressive_rgb_preserves_color_blocks() {
        let bytes = include_bytes!("../tests/data/image_decode/progressive_rgb.jpg");
        let frame = jpeg_header_segment(bytes, 0xc2).expect("progressive JPEG SOF2");
        assert_eq!(&frame[..6], &[8, 0, 16, 0, 32, 3]);
        assert_eq!(frame.len(), 6 + 3 * 3);
        assert!(frame[6..].chunks_exact(3).all(|p| p[1] == 0x11));
        assert!(jpeg_header_segment(bytes, 0xc0).is_none());
        // 色块边界与8像素编码块对齐；中心避开边界，单独验证有损颜色与BGRA通道。
        assert_jpeg_color_blocks(
            bytes,
            (32, 16),
            &[
                (8, 8, [224, 28, 28]),
                (24, 4, [28, 56, 224]),
                (24, 12, [128, 128, 128]),
            ],
        );
    }

    #[test]
    fn image_decode_jpeg_fixture_adobe_cmyk_converts_to_bgra() {
        let bytes = include_bytes!("../tests/data/image_decode/adobe_cmyk.jpg");
        let adobe = jpeg_header_segment(bytes, 0xee).expect("Adobe CMYK APP14");
        assert_eq!(adobe.len(), 12);
        assert_eq!(&adobe[..5], b"Adobe");
        assert_eq!(adobe[11], 0);
        let frame = jpeg_header_segment(bytes, 0xc0).expect("four-component JPEG SOF0");
        assert_eq!(&frame[..6], &[8, 0, 32, 0, 48, 4]);
        assert_eq!(frame.len(), 6 + 4 * 3);
        assert_eq!(
            frame[6..].chunks_exact(3).map(|p| p[0]).collect::<Vec<_>>(),
            b"CMYK"
        );
        assert!(frame[6..].chunks_exact(3).all(|p| p[1] == 0x11));
        assert_jpeg_color_blocks(
            bytes,
            (48, 32),
            &[
                (8, 16, [224, 28, 28]),
                (24, 16, [28, 56, 224]),
                (40, 16, [128, 128, 128]),
            ],
        );
    }

    #[test]
    fn image_decode_webp_fixture_lossy_rgb_keeps_exact_separate_alpha() {
        let bytes = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
            bytes.len()
        );
        let mut chunks = Vec::new();
        let mut offset = 12;
        while offset < bytes.len() {
            let length =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            chunks.push(&bytes[offset..offset + 4]);
            offset = offset.checked_add(8 + length + (length & 1)).unwrap();
        }
        assert_eq!(offset, bytes.len());
        assert!(chunks.contains(&b"VP8 ".as_slice()));
        assert!(chunks.contains(&b"ALPH".as_slice()));
        assert!(!chunks.contains(&b"VP8L".as_slice()));

        let decoded = decode_bytes(bytes).unwrap();
        assert_eq!(decoded.metadata.format, ImageFormat::WebP);
        assert_eq!(decoded.metadata.encoded_dimensions, Some((64, 32)));
        assert_eq!(decoded.metadata.display_dimensions, (64, 32));
        assert_eq!(
            decoded.metadata.decoded_color,
            Some(image::ColorType::Rgba8)
        );
        assert_eq!(decoded.metadata.encoded_bytes, Some(bytes.len() as u64));
        assert_eq!(
            decoded.metadata.orientation.map(|value| value.to_exif()),
            Some(1)
        );
        assert_eq!((decoded.image.width, decoded.image.height), (64, 32));
        assert_eq!(decoded.image.bgra.len(), 64 * 32 * 4);
        for y in 0..32 {
            for x in 0..64 {
                let expected = match (x >= 32, y >= 16) {
                    (false, false) => 255,
                    (true, false) => 128,
                    (false, true) => 0,
                    (true, true) => 64,
                };
                assert_eq!(decoded.image.bgra[(y * 64 + x) * 4 + 3], expected);
            }
        }
        // 自有色块样本使用有损VP8和独立ALPH；只在远离色块边缘的可见像素比较颜色。
        for (x, y, expected_rgb) in [
            (16, 8, [224u8, 28, 28]),
            (48, 8, [28, 56, 224]),
            (48, 24, [28, 200, 72]),
        ] {
            let actual = &decoded.image.bgra[(y * 64 + x) * 4..][..4];
            for (channel, expected) in expected_rgb.into_iter().rev().enumerate() {
                assert!(actual[channel].abs_diff(expected) <= 8, "{actual:?}");
            }
        }
    }

    fn with_orientation(jpeg: &[u8], orientation: u8) -> Vec<u8> {
        let mut exif =
            b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x01\0\0\0\0\0\0\0".to_vec();
        exif[24] = orientation;
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        bytes.extend(exif);
        bytes.extend_from_slice(&jpeg[2..]);
        bytes
    }

    #[test]
    fn image_decode_exif_all_eight_orientations_have_display_coordinates() {
        let mut jpeg = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(sample())
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .unwrap();
        let original = decode_bytes(jpeg.get_ref()).unwrap().image;
        for orientation in 1..=8 {
            let decoded = decode_bytes(&with_orientation(jpeg.get_ref(), orientation)).unwrap();
            let expected_size = if orientation < 5 { (3, 2) } else { (2, 3) };
            assert_eq!(decoded.metadata.encoded_dimensions, Some((3, 2)));
            assert_eq!(decoded.metadata.display_dimensions, expected_size);
            assert_eq!(
                decoded.metadata.orientation.map(|value| value.to_exif()),
                Some(orientation)
            );
            for y in 0..2 {
                for x in 0..3 {
                    let (dx, dy) = match orientation {
                        1 => (x, y),
                        2 => (2 - x, y),
                        3 => (2 - x, 1 - y),
                        4 => (x, 1 - y),
                        5 => (y, x),
                        6 => (1 - y, x),
                        7 => (1 - y, 2 - x),
                        8 => (y, 2 - x),
                        _ => unreachable!(),
                    };
                    let from = (y * 3 + x) * 4;
                    let to = (dy * decoded.image.width as usize + dx) * 4;
                    assert_eq!(
                        &decoded.image.bgra[to..to + 4],
                        &original.bgra[from..from + 4]
                    );
                }
            }
        }
    }

    #[test]
    fn image_decode_rejects_large_header_before_pixel_data() {
        let mut bmp = vec![0u8; 54];
        bmp[..2].copy_from_slice(b"BM");
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&8000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&4001i32.to_le_bytes());
        bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
        bmp[28..30].copy_from_slice(&24u16.to_le_bytes());
        let error = decode_bytes(&bmp).err().unwrap().to_string();
        assert!(
            error.contains("3200") || error.contains("32-megapixel"),
            "{error}"
        );
        bmp[18..22].copy_from_slice(&4000i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&4000i32.to_le_bytes());
        let error = decode_bytes_with_budget(&bmp, 48 * 1024 * 1024)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("预算") || error.contains("budget"),
            "{error}"
        );
        assert!(check_dimensions((65535, 1), 65535 * 4).is_ok());
        assert!(check_dimensions((65536, 1), 65536 * 4).is_err());
        assert!(check_dimensions((1, 1), MAX_DECODE_BYTES + 1).is_err());
        assert!(check_encoded_size(MAX_DECODE_BYTES + 1).is_err());
        assert!(decode_bytes(b"not an image").is_err());
    }

    #[test]
    fn image_decode_reports_high_depth_before_the_eight_bit_display_conversion() {
        let pixels = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(
            2,
            1,
            vec![0, 0x8080, 0xffff, 0x8080, 0xffff, 0, 0x8080, 0xffff],
        )
        .unwrap();
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba16(pixels)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let decoded = decode_bytes(encoded.get_ref()).unwrap();
        assert_eq!(
            decoded.metadata.decoded_color,
            Some(image::ColorType::Rgba16)
        );
        assert_eq!(decoded.metadata.encoded_dimensions, Some((2, 1)));
        assert_eq!(decoded.image.bgra, [255, 128, 0, 128, 128, 0, 255, 255]);
    }

    #[test]
    fn image_decode_reservation_rejects_missing_capacity_and_metadata_headroom_before_decode() {
        let bytes = b"not an image";
        assert!(decode_bytes_with_reservation(
            bytes,
            bytes.len() as u64 - 1,
            4,
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(decode_bytes_with_reservation(
            bytes,
            MAX_DECODE_BYTES - METADATA_WORKING_BYTES + 1,
            4,
            &AtomicBool::new(false)
        )
        .is_err());
        assert!(
            decode_bytes_with_reservation(bytes, u64::MAX, 4, &AtomicBool::new(false)).is_err()
        );
    }

    #[test]
    fn image_decode_rgb16_budget_counts_orientation_source_and_decoder_view_by_stage() {
        let dimensions = (4_000, 4_000);
        let native = 4_000 * 4_000 * 8;
        let output = 4_000 * 4_000 * 4;
        let source = 16 * 1024 * 1024;
        assert!(high_precision_decode_budget(
            dimensions,
            image::ColorType::Rgba16,
            native,
            image::metadata::Orientation::NoTransforms,
            source,
            8 * 1024,
            0,
            output
        )
        .is_ok());
        assert!(high_precision_decode_budget(
            dimensions,
            image::ColorType::Rgba16,
            native,
            image::metadata::Orientation::Rotate90,
            source,
            8 * 1024,
            0,
            output
        )
        .is_err());
        assert!(high_precision_decode_budget(
            dimensions,
            image::ColorType::Rgba16,
            native,
            image::metadata::Orientation::NoTransforms,
            source,
            MAX_DECODE_BYTES,
            0,
            output
        )
        .is_err());
        assert!(high_precision_decode_budget(
            dimensions,
            image::ColorType::Rgba16,
            native,
            image::metadata::Orientation::NoTransforms,
            source,
            0,
            0,
            output - 1
        )
        .is_err());
    }

    fn png16_with_profile_and_orientation(profile: Option<&[u8]>) -> Vec<u8> {
        use std::io::Write;
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            if let Some(profile) = profile {
                let mut compressed =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                compressed.write_all(profile).unwrap();
                let mut payload = b"linear\0\0".to_vec();
                payload.extend(compressed.finish().unwrap());
                writer.write_chunk(png::chunk::iCCP, &payload).unwrap();
            }
            let samples: [u16; 8] = [128, 128, 128, 32768, 65535, 65535, 65535, 65535];
            let data: Vec<u8> = samples.into_iter().flat_map(u16::to_be_bytes).collect();
            writer.write_image_data(&data).unwrap();
            let exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
            writer.write_chunk(png::chunk::eXIf, exif).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn image_decode_rgb16_orientation_and_alpha_apply_once_without_icc() {
        let png = png16_with_profile_and_orientation(None);
        let decoded = decode_bytes(&png).unwrap();
        assert_eq!(decoded.metadata.encoded_dimensions, Some((2, 1)));
        assert_eq!(decoded.metadata.display_dimensions, (1, 2));
        assert_eq!(
            decoded.metadata.decoded_color,
            Some(image::ColorType::Rgba16)
        );
        assert_eq!(decoded.metadata.orientation.unwrap().to_exif(), 6);
        assert_eq!(
            decoded.metadata.details.unwrap().source.bits_per_channel,
            Some(16)
        );
        assert_eq!(decoded.image.bgra, [0, 0, 0, 128, 255, 255, 255, 255]);
    }

    #[test]
    #[ignore = "调用系统16位ICC转换；需显式运行，无窗口"]
    fn image_decode_rgb16_icc_keeps_low_bits_before_quantizing_and_normalizes_orientation_once() {
        use crate::image_color_management::ColorConversion;
        let png = png16_with_profile_and_orientation(Some(include_bytes!(
            "../tests/data/image_color/linear-rgb.icc"
        )));
        let decoded = decode_bytes(&png).unwrap();
        assert_eq!(decoded.metadata.display_dimensions, (1, 2));
        assert_eq!(decoded.metadata.orientation.unwrap().to_exif(), 6);
        assert_eq!(
            decoded.metadata.details.unwrap().color_conversion,
            ColorConversion::ConvertedSrgb16
        );
        assert_eq!(decoded.image.bgra[3], 128);
        assert_eq!(decoded.image.bgra[7], 255);
        // linear 128/65535经sRGB约为6；先量化到8位只能得到0或约13。
        assert!(decoded.image.bgra[..3]
            .iter()
            .all(|value| value.abs_diff(6) <= 2));
        assert!(decoded.image.bgra[4..7]
            .iter()
            .all(|value| value.abs_diff(255) <= 2));
        let sidecar = decoded.metadata_source.unwrap();
        assert_eq!(sidecar.facts().source.bits_per_channel, Some(16));
        assert_eq!(
            sidecar.facts().color_conversion,
            ColorConversion::ConvertedSrgb16
        );
    }

    fn png_with_chunk(chunk: png::chunk::ChunkType, payload: &[u8], after_pixels: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            if !after_pixels {
                writer.write_chunk(chunk, payload).unwrap();
            }
            writer
                .write_image_data(&[230, 20, 40, 128, 10, 60, 220, 64])
                .unwrap();
            if after_pixels {
                writer.write_chunk(chunk, payload).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    fn single_icon(payload: &[u8], kind: u8) -> Vec<u8> {
        let mut bytes = vec![0, 0, kind, 0, 1, 0, 2, 1, 0, 0];
        bytes.extend_from_slice(&1u16.to_le_bytes()); // ICO planes / CUR hotspot x
        bytes.extend_from_slice(&(if kind == 1 { 32u16 } else { 0 }).to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&22u32.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn image_decode_post_idat_exif_rotates_png_once_and_preserves_icon_coordinates() {
        use crate::image_container_metadata::MetadataStatus;
        let exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let png = png_with_chunk(png::chunk::eXIf, exif, true);
        let decoded = decode_bytes(&png).unwrap();
        assert_eq!(decoded.metadata.encoded_dimensions, Some((2, 1)));
        assert_eq!(decoded.metadata.display_dimensions, (1, 2));
        assert_eq!(
            decoded.metadata.orientation.map(|value| value.to_exif()),
            Some(6)
        );
        assert_eq!(decoded.image.bgra, [40, 20, 230, 128, 220, 60, 10, 64]);
        assert_eq!(
            decoded.metadata.details.unwrap().source.exif.status,
            MetadataStatus::Present
        );
        for kind in [1, 2] {
            let bytes = single_icon(&png, kind);
            let icon = decode_bytes(&bytes).unwrap();
            assert_eq!(icon.metadata.format, ImageFormat::Ico);
            assert_eq!(icon.metadata.encoded_dimensions, Some((2, 1)));
            assert_eq!(icon.metadata.display_dimensions, (2, 1));
            assert_eq!((icon.image.width, icon.image.height), (2, 1));
            assert_eq!(
                icon.metadata.orientation.map(|value| value.to_exif()),
                Some(1)
            );
            assert_eq!(
                icon.metadata
                    .details
                    .unwrap()
                    .source
                    .orientation
                    .unwrap()
                    .to_exif(),
                6
            );
            assert_eq!(icon.metadata.encoded_bytes, Some(bytes.len() as u64));
            assert_eq!(icon.image.bgra, decoded.image.bgra);
        }
    }

    #[test]
    fn image_decode_raw_icon_dib_uses_bounded_selection_without_recursive_dispatch() {
        let mut dib = vec![0; 40];
        dib[..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&[3, 2, 1, 0, 6, 5, 4, 0, 0x40, 0, 0, 0]);
        for kind in [1, 2] {
            let bytes = single_icon(&dib, kind);
            let decoded = decode_bytes(&bytes).unwrap();
            assert_eq!(decoded.metadata.format, ImageFormat::Ico);
            assert_eq!(decoded.metadata.display_dimensions, (2, 1));
            assert_eq!(decoded.image.bgra, [3, 2, 1, 255, 6, 5, 4, 0]);
            assert!(decode_bytes_with_budget(&bytes, 7).is_err());
        }
    }

    #[test]
    fn image_decode_bad_and_inflated_icc_are_removed_before_pixel_decoder_construction() {
        use crate::image_color_management::{ColorConversion, UnappliedReason};
        use crate::image_container_metadata::MetadataStatus;
        use std::io::Write;
        let mut compressed =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        compressed.write_all(&vec![0; 4 * 1024 * 1024 + 1]).unwrap();
        let mut inflated = b"profile\0\0".to_vec();
        inflated.extend(compressed.finish().unwrap());
        for (payload, status, reason) in [
            (
                b"profile\0\0bad zlib".as_slice(),
                MetadataStatus::Malformed,
                UnappliedReason::MalformedMetadata,
            ),
            (
                inflated.as_slice(),
                MetadataStatus::TooLarge,
                UnappliedReason::TooLarge,
            ),
        ] {
            let png = png_with_chunk(png::chunk::iCCP, payload, false);
            for bytes in [png.clone(), single_icon(&png, 1), single_icon(&png, 2)] {
                let decoded = decode_bytes(&bytes).unwrap();
                let metadata = decoded.metadata.details.unwrap();
                assert_eq!(metadata.source.icc.status, status);
                assert_eq!(
                    metadata.color_conversion,
                    ColorConversion::EmbeddedUnapplied(reason)
                );
                assert_eq!(decoded.image.bgra, [40, 20, 230, 128, 220, 60, 10, 64]);
                assert!(decoded.warning.is_some());
            }
        }
    }

    #[test]
    fn image_decode_cancellation_during_input_read_stops_before_another_read_or_decode() {
        struct CancelRead<'a> {
            cancel: &'a AtomicBool,
            calls: usize,
        }
        impl Read for CancelRead<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                assert_eq!(self.calls, 1);
                bytes[0] = 0x89;
                self.cancel.store(true, Ordering::Relaxed);
                Ok(1)
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = CancelRead {
            cancel: &cancel,
            calls: 0,
        };
        assert!(input::read_known_length(&mut reader, 1, &cancel).is_err());
        assert_eq!(reader.calls, 1);
        assert!(input::read_known_length(&mut reader, 1, &cancel).is_err());
        assert_eq!(reader.calls, 1);
    }

    fn exif_with_camera_and_direction(bad_subdirectory: bool) -> Vec<u8> {
        let count = if bad_subdirectory { 3u16 } else { 2u16 };
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&count.to_le_bytes());
        for (tag, kind, count, value) in [
            (0x010fu16, 2u16, 7u32, 8 + 2 + u32::from(count) * 12 + 4),
            (0x0112u16, 3u16, 1u32, 6u32),
        ] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&kind.to_le_bytes());
            tiff.extend_from_slice(&count.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        if bad_subdirectory {
            tiff.extend_from_slice(&0x8769u16.to_le_bytes());
            tiff.extend_from_slice(&4u16.to_le_bytes());
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(b"Camera\0");
        tiff
    }

    #[test]
    fn image_decode_exif_sidecar_survives_normalization_and_rebuilds_for_final_pixels() {
        use crate::{
            image_export_metadata::{self, MetadataPolicy},
            save_options::SaveEncoding,
        };
        let bytes = png_with_chunk(
            png::chunk::eXIf,
            &exif_with_camera_and_direction(false),
            true,
        );
        let decoded = decode_bytes(&bytes).unwrap();
        assert_eq!(decoded.metadata.display_dimensions, (1, 2));
        let source = decoded.metadata_source.unwrap();
        assert_eq!(source.exif_summary().unwrap().retained_fields, 1);
        assert_eq!(source.exif_summary().unwrap().dropped_fields, 1);
        assert!(source.exif_error().is_none());
        drop(bytes);
        // 图像处理后尺寸已变化；导出不得使用源EXIF的方向和宽高。
        let result = Image {
            width: 1,
            height: 1,
            bgra: vec![17, 53, 211, 128],
            readback_ms: 0.,
        };
        let fresh = crate::export::encode_image(&result, SaveEncoding::default()).unwrap();
        let encoded = image_export_metadata::apply(
            fresh,
            crate::save_options::SaveFormat::Png,
            (1, 1),
            MetadataPolicy::ColorAndSupportedExif,
            Some(&source),
            &NEVER_CANCELLED,
        )
        .unwrap();
        let roundtrip = decode_bytes(&encoded).unwrap();
        assert_eq!(roundtrip.metadata.display_dimensions, (1, 1));
        assert_eq!(
            roundtrip.metadata.orientation.map(|value| value.to_exif()),
            Some(1)
        );
        assert_eq!(roundtrip.image.bgra, result.bgra);
        // 独立检查新TIFF字段值，避免只用同一个解析器验证自己的重建结果。
        let mut position = 8;
        let mut tiff = None;
        while position < encoded.len() {
            let length =
                u32::from_be_bytes(encoded[position..position + 4].try_into().unwrap()) as usize;
            if &encoded[position + 4..position + 8] == b"eXIf" {
                tiff = Some(&encoded[position + 8..position + 8 + length]);
                break;
            }
            position += length + 12;
        }
        let tiff = tiff.unwrap();
        assert_eq!(&tiff[..8], b"II*\0\x08\0\0\0");
        let tag = |ifd: usize, wanted: u16| {
            let count = usize::from(u16::from_le_bytes(tiff[ifd..ifd + 2].try_into().unwrap()));
            (0..count)
                .map(|i| &tiff[ifd + 2 + i * 12..ifd + 14 + i * 12])
                .find(|entry| u16::from_le_bytes(entry[..2].try_into().unwrap()) == wanted)
                .unwrap()
        };
        let camera = tag(8, 0x010f);
        assert_eq!(u32::from_le_bytes(camera[4..8].try_into().unwrap()), 7);
        let camera_offset = u32::from_le_bytes(camera[8..12].try_into().unwrap()) as usize;
        assert_eq!(&tiff[camera_offset..camera_offset + 7], b"Camera\0");
        assert_eq!(&tag(8, 0x0112)[8..10], &1u16.to_le_bytes());
        let exif_ifd = u32::from_le_bytes(tag(8, 0x8769)[8..12].try_into().unwrap()) as usize;
        for size_tag in [0xa002, 0xa003] {
            assert_eq!(&tag(exif_ifd, size_tag)[8..12], &1u32.to_le_bytes());
        }
        assert_eq!(&tag(exif_ifd, 0xa001)[8..10], &1u16.to_le_bytes());
    }

    #[test]
    fn image_decode_invalid_optional_exif_preservation_keeps_pixels_and_other_save_policies() {
        use crate::{
            image_export_metadata::{self, MetadataPolicy},
            save_options::{SaveEncoding, SaveFormat},
        };
        let bytes = png_with_chunk(
            png::chunk::eXIf,
            &exif_with_camera_and_direction(true),
            false,
        );
        let decoded = decode_bytes(&bytes).unwrap();
        assert_eq!(
            decoded.metadata.orientation.map(|value| value.to_exif()),
            Some(6)
        );
        assert_eq!(decoded.metadata.display_dimensions, (1, 2));
        assert_eq!(decoded.image.bgra, [40, 20, 230, 128, 220, 60, 10, 64]);
        let source = decoded.metadata_source.unwrap();
        assert!(source.exif_summary().is_none());
        assert!(source.exif_error().is_some());
        for policy in [
            MetadataPolicy::Strip,
            MetadataPolicy::ColorOnly,
            MetadataPolicy::ColorAndSupportedExif,
        ] {
            let fresh =
                crate::export::encode_image(&decoded.image, SaveEncoding::default()).unwrap();
            let output = image_export_metadata::apply(
                fresh,
                SaveFormat::Png,
                (1, 2),
                policy,
                Some(&source),
                &NEVER_CANCELLED,
            );
            if policy == MetadataPolicy::ColorAndSupportedExif {
                assert!(output.is_err());
            } else {
                assert_eq!(
                    decode_bytes(&output.unwrap()).unwrap().image.bgra,
                    decoded.image.bgra
                );
            }
        }
    }
}
