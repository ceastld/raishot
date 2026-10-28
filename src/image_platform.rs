//! 条件系统图片预览：原目录选项重建为单图输入，仅请求8位直通BGRA。
//! 本层在交付复制时应用EXIF一次，随后处理受支持ICC；HDR映射与原生峰值仍不保证。
use crate::capture::Image;
use image::metadata::Orientation;
use std::{
    fmt,
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
};
use windows::{
    core::HRESULT,
    Graphics::Imaging::{
        BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode,
        ExifOrientationMode,
    },
    Storage::Streams::IRandomAccessStream,
    Win32::{
        Foundation::{
            E_NOINTERFACE, E_OUTOFMEMORY, REGDB_E_CLASSNOTREG, RPC_E_CHANGED_MODE,
            WINCODEC_ERR_BADHEADER, WINCODEC_ERR_BADIMAGE, WINCODEC_ERR_BADMETADATAHEADER,
            WINCODEC_ERR_BADSTREAMDATA, WINCODEC_ERR_COMPONENTNOTFOUND, WINCODEC_ERR_FRAMEMISSING,
            WINCODEC_ERR_UNKNOWNIMAGEFORMAT, WINCODEC_ERR_UNSUPPORTEDOPERATION,
            WINCODEC_ERR_UNSUPPORTEDPIXELFORMAT,
        },
        Media::MediaFoundation::{MF_E_INVALIDMEDIATYPE, MF_E_TOPO_CODEC_NOT_FOUND},
        System::WinRT::{
            CreateRandomAccessStreamOverStream, RoInitialize, RoUninitialize, BSOS_DEFAULT,
            RO_INIT_MULTITHREADED,
        },
        UI::Shell::SHCreateMemStream,
    },
};

const MAX_FTYP_BYTES: u64 = 4096;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PIXELS: u64 = 32_000_000;
const MAX_SIDE: u32 = 65_535;
// 预检的8MiB目录/元数据上限、同时存在的有界目录Vec、单份256KiB EXIF与码流头工作区。
// 先留足余量，避免源Vec预留接近上限时，尚未进入最终预算就分配目录或元数据。
const INSPECTION_RESERVE_BYTES: u64 = 16 * 1024 * 1024;

#[path = "image_platform_coded.rs"]
mod coded;
#[path = "image_platform_color.rs"]
mod color;
#[path = "image_platform_hdr.rs"]
mod hdr;
#[path = "image_platform_hdr_decode.rs"]
mod hdr_decode;
#[path = "image_platform_hevc_native.rs"]
mod hevc_native;
#[path = "image_platform_metadata.rs"]
mod metadata;
#[path = "image_platform_p010.rs"]
mod p010;
#[path = "image_platform_preflight.rs"]
mod preflight;
pub(crate) use coded::{Cicp, SampleModel};

/// 来自同一原图项的容器和实际码流事实；不代表这些信号已映射到显示空间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColorFacts {
    pub container_cicp: Option<Cicp>,
    pub bitstream_cicp: Option<Cicp>,
    pub sample_model: SampleModel,
    pub hdr_mapped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlatformFormat {
    Avif,
    Heif,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlatformError {
    ComponentUnavailable(i32),
    Unsupported {
        hresult: Option<i32>,
        message: String,
    },
    Invalid {
        hresult: Option<i32>,
        message: String,
    },
    Budget(String),
    ThreadContext(i32),
    Native(i32),
    Cancelled,
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (message, hresult) = match self {
            Self::ComponentUnavailable(code) => (
                tr!(
                    "系统HEIF/AVIF解码组件或接口不可用",
                    "System HEIF/AVIF decoder component or interface is unavailable"
                ),
                Some(*code),
            ),
            Self::Unsupported { hresult, message } | Self::Invalid { hresult, message } => {
                (message.as_str(), *hresult)
            }
            Self::Budget(message) => (message.as_str(), None),
            Self::ThreadContext(code) => (
                tr!(
                    "系统图片解码需要独立MTA线程，当前线程上下文不适用",
                    "System image decoding requires an MTA worker; this thread context is unsuitable"
                ),
                Some(*code),
            ),
            Self::Native(code) => (
                tr!("系统图片解码失败", "System image decoding failed"),
                Some(*code),
            ),
            Self::Cancelled => (
                tr!("系统图片读取已取消", "System image reading canceled"),
                None,
            ),
        };
        formatter.write_str(message)?;
        if let Some(code) = hresult {
            write!(formatter, " (HRESULT 0x{:08X})", code as u32)?;
        }
        Ok(())
    }
}

impl std::error::Error for PlatformError {}

pub(crate) struct PlatformDecoded {
    pub image: Image,
    /// 原容器中可独立浏览的图项数量；默认索引0对应原主图。
    pub frame_count: u32,
    pub frame_index: u32,
    pub item_id: u32,
    pub is_primary: bool,
    /// 只读选中图项所关联EXIF的方向；None表示未声明。
    pub orientation: Option<Orientation>,
    /// 由选中原图项的实际码流头确认。
    pub encoded_dimensions: Option<(u32, u32)>,
    pub bits_per_channel: Option<u8>,
    /// 系统未可靠报告转换前类型时为None；已请求的BGRA8由image与warning说明。
    pub decoded_color: Option<image::ColorType>,
    pub color: Option<ColorFacts>,
    pub managed: Option<crate::image_color_management::ManagedMetadata>,
    pub metadata_source: Option<std::sync::Arc<crate::image_export_metadata::SourceMetadata>>,
    pub warning: Option<String>,
}

fn invalid(zh: &'static str, en: &'static str) -> PlatformError {
    PlatformError::Invalid {
        hresult: None,
        message: crate::i18n::tr(zh, en).into(),
    }
}

fn budget() -> PlatformError {
    PlatformError::Budget(
        tr!(
            "系统图片超过尺寸、输出或256MiB可见缓冲预算",
            "System image exceeds the dimension, output, or 256 MiB visible-buffer budget"
        )
        .into(),
    )
}

fn native_error(code: HRESULT) -> PlatformError {
    if code == RPC_E_CHANGED_MODE {
        PlatformError::ThreadContext(code.0)
    } else if [
        WINCODEC_ERR_COMPONENTNOTFOUND,
        REGDB_E_CLASSNOTREG,
        E_NOINTERFACE,
        MF_E_TOPO_CODEC_NOT_FOUND,
    ]
    .contains(&code)
    {
        PlatformError::ComponentUnavailable(code.0)
    } else if [
        WINCODEC_ERR_UNKNOWNIMAGEFORMAT,
        WINCODEC_ERR_UNSUPPORTEDOPERATION,
        WINCODEC_ERR_UNSUPPORTEDPIXELFORMAT,
        MF_E_INVALIDMEDIATYPE,
    ]
    .contains(&code)
    {
        PlatformError::Unsupported {
            hresult: Some(code.0),
            message: tr!(
                "系统不支持此图片编码、操作或像素格式",
                "The system does not support this image encoding, operation, or pixel format"
            )
            .into(),
        }
    } else if [
        WINCODEC_ERR_BADIMAGE,
        WINCODEC_ERR_BADHEADER,
        WINCODEC_ERR_BADMETADATAHEADER,
        WINCODEC_ERR_BADSTREAMDATA,
        WINCODEC_ERR_FRAMEMISSING,
    ]
    .contains(&code)
    {
        PlatformError::Invalid {
            hresult: Some(code.0),
            message: tr!(
                "系统报告图片数据无效",
                "The system reported invalid image data"
            )
            .into(),
        }
    } else {
        PlatformError::Native(code.0)
    }
}

fn cancelled(cancel: &AtomicBool) -> Result<(), PlatformError> {
    if cancel.load(Ordering::Acquire) {
        Err(PlatformError::Cancelled)
    } else {
        Ok(())
    }
}

fn call<T>(
    cancel: &AtomicBool,
    action: impl FnOnce() -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    cancelled(cancel)?;
    let result = action();
    cancelled(cancel)?;
    result
}

fn native<T>(
    cancel: &AtomicBool,
    action: impl FnOnce() -> windows::core::Result<T>,
) -> Result<T, PlatformError> {
    call(cancel, || {
        action().map_err(|error| native_error(error.code()))
    })
}

fn native_at<T>(
    _stage: &'static str,
    cancel: &AtomicBool,
    action: impl FnOnce() -> windows::core::Result<T>,
) -> Result<T, PlatformError> {
    let result = native(cancel, action);
    #[cfg(test)]
    if let Err(error) = &result {
        eprintln!("image platform native stage={_stage}: {error}");
    }
    result
}

struct Apartment {
    owned: bool,
    _thread: PhantomData<Rc<()>>,
}

impl Apartment {
    fn initialize(cancel: &AtomicBool) -> Result<Self, PlatformError> {
        cancelled(cancel)?;
        let result = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        // S_OK与S_FALSE都拥有一次初始化；晚到取消也必须归还本次引用。
        let guard = Self {
            owned: result.is_ok(),
            _thread: PhantomData,
        };
        cancelled(cancel)?;
        result.map_err(|error| native_error(error.code()))?;
        Ok(guard)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.owned {
            unsafe { RoUninitialize() };
        }
    }
}

/// 只识别首个有界ftyp，不把其它BMFF容器猜成图片；AVIF品牌优先于通用HEIF品牌。
pub(crate) fn detect(bytes: &[u8]) -> Result<Option<PlatformFormat>, PlatformError> {
    // 已有光栅头优先；JPEG APP段长度与内容也可能恰好在偏移4组成ftyp。
    if image::guess_format(bytes).is_ok_and(|format| format != image::ImageFormat::Avif) {
        return Ok(None);
    }
    if bytes.len() < 8 {
        return if bytes.len() > 4 && b"ftyp".starts_with(&bytes[4..]) {
            Err(invalid("图片ftyp头被截断", "Truncated image ftyp header"))
        } else {
            Ok(None)
        };
    }
    if &bytes[4..8] != b"ftyp" {
        return Ok(None);
    }
    let short = u32::from_be_bytes(bytes[..4].try_into().unwrap());
    let (length, header) = if short == 1 {
        if bytes.len() < 16 {
            return Err(invalid(
                "图片ftyp扩展头被截断",
                "Truncated extended image ftyp header",
            ));
        }
        (u64::from_be_bytes(bytes[8..16].try_into().unwrap()), 16)
    } else {
        (u64::from(short), 8)
    };
    if length < header as u64 + 8 || (length - header as u64 - 8) % 4 != 0 {
        return Err(invalid("图片ftyp长度无效", "Invalid image ftyp length"));
    }
    if length > MAX_FTYP_BYTES {
        return Err(PlatformError::Unsupported {
            hresult: None,
            message: tr!(
                "图片ftyp品牌表超过4KiB",
                "Image ftyp brand table exceeds 4 KiB"
            )
            .into(),
        });
    }
    if length > bytes.len() as u64 {
        return Err(invalid(
            "图片ftyp品牌表被截断",
            "Truncated image ftyp brand table",
        ));
    }
    let body = &bytes[header..length as usize];
    let brands = std::iter::once(&body[..4]).chain(body[8..].chunks_exact(4));
    let mut avif = false;
    let mut heif = false;
    for brand in brands {
        match brand {
            b"avif" | b"avis" => avif = true,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => heif = true,
            _ => {}
        }
    }
    Ok(if avif {
        Some(PlatformFormat::Avif)
    } else if heif {
        Some(PlatformFormat::Heif)
    } else {
        None
    })
}

fn input_budget(encoded: usize, input_reserved_bytes: u64) -> Result<u64, PlatformError> {
    let encoded = u64::try_from(encoded).map_err(|_| budget())?;
    let copies = input_reserved_bytes
        .checked_add(encoded)
        .ok_or_else(budget)?;
    if encoded == 0 || encoded > MAX_BYTES || input_reserved_bytes < encoded || copies >= MAX_BYTES
    {
        return Err(budget());
    }
    Ok(copies)
}

fn inspection_budget(source_len: usize, source_reserved_bytes: u64) -> Result<(), PlatformError> {
    if source_len == 0
        || source_len as u64 > source_reserved_bytes
        || source_reserved_bytes
            .checked_add(INSPECTION_RESERVE_BYTES)
            .is_none_or(|bytes| bytes > MAX_BYTES)
    {
        return Err(budget());
    }
    Ok(())
}

fn pixels((width, height): (u32, u32)) -> Result<u64, PlatformError> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(budget());
    }
    Ok(pixels)
}

fn native_pixel_bytes(format: BitmapPixelFormat, pixels: u64) -> Result<u64, PlatformError> {
    let bytes_per_pixel = match format {
        BitmapPixelFormat::Rgba16 | BitmapPixelFormat::P010 => 8,
        BitmapPixelFormat::Rgba8 | BitmapPixelFormat::Bgra8 => 4,
        BitmapPixelFormat::Gray16 => 2,
        BitmapPixelFormat::Gray8 => 1,
        // 先按四字节覆盖色度平面及奇数边缘，不承诺原始文件就是这些布局。
        BitmapPixelFormat::Nv12 | BitmapPixelFormat::Yuy2 => 4,
        _ => {
            return Err(PlatformError::Unsupported {
                hresult: None,
                message: tr!(
                    "系统返回了无法核算预算的像素格式",
                    "The system returned a pixel format with an unknown buffer budget"
                )
                .into(),
            })
        }
    };
    pixels.checked_mul(bytes_per_pixel).ok_or_else(budget)
}

fn output_budget(
    encoded: usize,
    input_reserved_bytes: u64,
    raw: (u32, u32),
    oriented: (u32, u32),
    format: BitmapPixelFormat,
    max_output_bytes: u64,
) -> Result<usize, PlatformError> {
    let raw_pixels = pixels(raw)?;
    let oriented_pixels = pixels(oriented)?;
    if raw_pixels != oriented_pixels {
        return Err(invalid(
            "系统图片方向尺寸不一致",
            "Inconsistent system image orientation dimensions",
        ));
    }
    let output = oriented_pixels.checked_mul(4).ok_or_else(budget)?;
    let native_bytes = native_pixel_bytes(format, raw_pixels)?;
    let visible = input_budget(encoded, input_reserved_bytes)?
        .checked_add(output.checked_mul(2).ok_or_else(budget)?)
        .and_then(|bytes| bytes.checked_add(native_bytes))
        .ok_or_else(budget)?;
    if output > max_output_bytes || visible > MAX_BYTES {
        return Err(budget());
    }
    usize::try_from(output).map_err(|_| budget())
}

fn preflight_budget(
    source_len: usize,
    input_reserved_bytes: u64,
    frozen_len: usize,
    frozen_reserved_bytes: u64,
    plan: &preflight::Plan<'_>,
    index: usize,
) -> Result<(), PlatformError> {
    if source_len == 0
        || source_len as u64 > input_reserved_bytes
        || frozen_len as u64 > frozen_reserved_bytes
    {
        return Err(budget());
    }
    let resident = input_reserved_bytes
        .checked_add(frozen_reserved_bytes)
        .ok_or_else(budget)?;
    if input_budget(frozen_len, resident)?
        .checked_add(plan.before_native_bytes_for(index)?)
        .is_none_or(|bytes| bytes > MAX_BYTES)
    {
        return Err(budget());
    }
    Ok(())
}

fn copy_pixels(
    source: &[u8],
    dimensions: (u32, u32),
    expected: usize,
    orientation: Orientation,
    cancel: &AtomicBool,
) -> Result<Image, PlatformError> {
    cancelled(cancel)?;
    if source.len() != expected || pixels(dimensions)?.checked_mul(4) != Some(expected as u64) {
        return Err(invalid(
            "系统图片像素长度不一致",
            "Inconsistent system image pixel length",
        ));
    }
    let mut result = Vec::new();
    result.try_reserve_exact(expected).map_err(|_| budget())?;
    // 预检按Array与最终Vec各一份计算；不额外保留未定向Vec。
    if result.capacity() > expected {
        return Err(budget());
    }
    let swapped = matches!(
        orientation,
        Orientation::Rotate90FlipH
            | Orientation::Rotate90
            | Orientation::Rotate270FlipH
            | Orientation::Rotate270
    );
    let (width, height) = dimensions;
    let (display_width, display_height) = if swapped { (height, width) } else { dimensions };
    let row = usize::try_from(u64::from(dimensions.0) * 4).map_err(|_| budget())?;
    if orientation == Orientation::NoTransforms {
        for data in source.chunks_exact(row) {
            cancelled(cancel)?;
            result.extend_from_slice(data);
        }
    } else {
        result.resize(expected, 0);
        for (y, data) in source.chunks_exact(row).enumerate() {
            cancelled(cancel)?;
            let y = y as u32;
            for (x, pixel) in data.chunks_exact(4).enumerate() {
                let x = x as u32;
                let (target_x, target_y) = match orientation {
                    Orientation::NoTransforms => (x, y),
                    Orientation::FlipHorizontal => (width - 1 - x, y),
                    Orientation::Rotate180 => (width - 1 - x, height - 1 - y),
                    Orientation::FlipVertical => (x, height - 1 - y),
                    Orientation::Rotate90FlipH => (y, x),
                    Orientation::Rotate90 => (height - 1 - y, x),
                    Orientation::Rotate270FlipH => (height - 1 - y, width - 1 - x),
                    Orientation::Rotate270 => (y, width - 1 - x),
                };
                let offset = ((u64::from(target_y) * u64::from(display_width)
                    + u64::from(target_x))
                    * 4) as usize;
                result[offset..offset + 4].copy_from_slice(pixel);
            }
        }
    }
    cancelled(cancel)?;
    Ok(Image {
        width: display_width,
        height: display_height,
        bgra: result,
        readback_ms: 0.,
    })
}

/// 只解码选中原图项；max_output_bytes与统一入口一样仅限制最终BGRA长度。
pub(crate) fn decode(
    bytes: &[u8],
    format: PlatformFormat,
    frame_index: u32,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<PlatformDecoded, PlatformError> {
    cancelled(cancel)?;
    decode_with_input_reservation(
        bytes,
        u64::try_from(bytes.len()).map_err(|_| budget())?,
        format,
        frame_index,
        max_output_bytes,
        cancel,
    )
}

/// 文件读取端传入Vec容量，字节入口传len；驻留量不能小于本次冻结来源。
pub(crate) fn decode_with_input_reservation(
    bytes: &[u8],
    input_reserved_bytes: u64,
    format: PlatformFormat,
    frame_index: u32,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<PlatformDecoded, PlatformError> {
    cancelled(cancel)?;
    inspection_budget(bytes.len(), input_reserved_bytes)?;
    if detect(bytes)? != Some(format) {
        return Err(invalid(
            "系统图片格式与ftyp声明不一致",
            "System image format does not match ftyp",
        ));
    }
    let plan = preflight::inspect(bytes, max_output_bytes, cancel)?;
    let frame_count = u32::try_from(plan.catalog().len()).map_err(|_| budget())?;
    let index = usize::try_from(frame_index).map_err(|_| budget())?;
    let hdr_warning = match hdr_decode::decode(
        &plan,
        index,
        format,
        input_reserved_bytes,
        max_output_bytes,
        cancel,
    ) {
        Ok(Some(decoded)) => return Ok(decoded),
        Ok(None) => None,
        Err(error @ PlatformError::ComponentUnavailable(_))
        | Err(error @ PlatformError::Unsupported { .. }) => Some(trf!(
            "高精度HDR入口不可用，当前为未映射的系统8位副本：{error}",
            "High-precision HDR decoding is unavailable; showing an unmapped system 8-bit copy: {error}"
        )),
        Err(error) => return Err(error),
    };
    let frozen_length = plan.frozen_bytes(index)?;
    preflight_budget(
        bytes.len(),
        input_reserved_bytes,
        frozen_length,
        frozen_length as u64,
        &plan,
        index,
    )?;
    let frozen = plan.freeze_item(index, cancel)?;
    let item = frozen.item;
    let color_facts = ColorFacts {
        container_cicp: item.color.container_cicp,
        bitstream_cicp: item.color.coded_color.cicp,
        sample_model: item.color.coded_color.model,
        hdr_mapped: false,
    };
    let is_primary = index == plan.primary_index();
    preflight_budget(
        bytes.len(),
        input_reserved_bytes,
        frozen.bytes.len(),
        frozen.bytes.capacity() as u64,
        &plan,
        index,
    )?;
    let resident = input_reserved_bytes
        .checked_add(frozen.bytes.capacity() as u64)
        .ok_or_else(budget)?;
    let _apartment = Apartment::initialize(cancel)?;
    let memory = call(cancel, || {
        unsafe { SHCreateMemStream(Some(&frozen.bytes)) }
            .ok_or(PlatformError::Native(E_OUTOFMEMORY.0))
    })?;
    let stream: IRandomAccessStream = native_at("random-access stream", cancel, || unsafe {
        CreateRandomAccessStreamOverStream(&memory, BSOS_DEFAULT)
    })?;
    // 严格ftyp判型后由系统仲裁已安装的解码器，不把AVIF固定到单一HEIF codec ID。
    let operation = native_at("decoder create", cancel, || {
        BitmapDecoder::CreateAsync(&stream)
    })?;
    // get内部的系统调用不可由AtomicBool强行中断，返回后仍先处理取消。
    let decoder = native_at("decoder create completion", cancel, || operation.get())?;
    drop(operation);
    if native_at("frame count", cancel, || decoder.FrameCount())? != 1 {
        return Err(invalid(
            "系统解码器未将选中图项识别为唯一图片",
            "The system decoder did not identify the selected item as a single image",
        ));
    }
    let operation = native_at("frame open", cancel, || decoder.GetFrameAsync(0))?;
    let frame = native_at("frame open completion", cancel, || operation.get())?;
    drop(operation);
    let raw = (
        native_at("pixel width", cancel, || frame.PixelWidth())?,
        native_at("pixel height", cancel, || frame.PixelHeight())?,
    );
    let oriented = (
        native_at("oriented width", cancel, || frame.OrientedPixelWidth())?,
        native_at("oriented height", cancel, || frame.OrientedPixelHeight())?,
    );
    if raw != item.coded.display_dimensions || oriented != raw {
        return Err(invalid(
            "系统图片尺寸与已检查的码流图项不一致",
            "System image dimensions do not match the selected coded item",
        ));
    }
    let native_format = native_at("pixel format", cancel, || frame.BitmapPixelFormat())?;
    let output = output_budget(
        frozen.bytes.len(),
        resident,
        raw,
        oriented,
        native_format,
        max_output_bytes,
    )?;
    let transform = native_at("bitmap transform", cancel, BitmapTransform::new)?;
    // 冻结视图没有EXIF；显式禁用系统方向处理，本层复制时应用原方向一次。
    let operation = native_at("pixel data request", cancel, || {
        frame.GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Straight,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
    })?;
    let provider = native_at("pixel data completion", cancel, || operation.get())?;
    drop(operation);
    let data = native_at("pixel data detach", cancel, || provider.DetachPixelData())?;
    // 释放已知持有者后再复制CoTaskMem数组。公开API不能限制codec内部峰值。
    drop(provider);
    drop(transform);
    drop(frame);
    drop(decoder);
    drop(stream);
    drop(memory);
    let image = copy_pixels(
        &data,
        raw,
        output,
        item.orientation.unwrap_or(Orientation::NoTransforms),
        cancel,
    )?;
    drop(data);
    // ICC处理只发生在原生解码持有者和虚拟输入释放之后，不叠加两阶段的像素峰值。
    drop(frozen);
    drop(_apartment);
    let profile = plan.icc_profile(index)?;
    let color_resident = input_reserved_bytes
        .checked_add(plan.retained_bytes()?)
        .ok_or_else(budget)?;
    let managed = color::manage(image, item, profile, color_resident, cancel)?;
    cancelled(cancel)?;
    let warning = Some(match (hdr_warning, managed.warning) {
        (Some(hdr), Some(color)) => format!("{hdr} {color}"),
        (Some(hdr), None) => hdr,
        (None, Some(warning)) => trf!(
            "系统8位预览；HDR映射未验证。{warning}",
            "System 8-bit preview; HDR mapping is unverified. {warning}"
        ),
        (None, None) => tr!(
            "系统8位预览；HDR映射未验证。",
            "System 8-bit preview; HDR mapping is unverified."
        )
        .into(),
    });
    Ok(PlatformDecoded {
        image: managed.image,
        frame_count,
        frame_index,
        item_id: item.item_id,
        is_primary,
        orientation: item.orientation,
        encoded_dimensions: Some(item.coded.coded_dimensions),
        bits_per_channel: Some(item.coded.bits_per_channel),
        // 宿主把此字段展示为转换前类型，不能用请求得到的8位输出代填。
        decoded_color: None,
        color: Some(color_facts),
        managed: Some(managed.metadata),
        metadata_source: managed.metadata_source,
        warning,
    })
}

#[cfg(test)]
#[path = "image_platform_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "image_platform_native_diagnostic_tests.rs"]
mod native_diagnostic_tests;

#[cfg(test)]
#[path = "image_platform_avif_baseline_tests.rs"]
mod avif_baseline_tests;
