//! 将已经证明属于嵌入RGB配置的直alpha像素转换为固定sRGB副本。
//!
//! 可见缓冲的预算不涵盖系统CMM内部的配置解析和变换缓存；本层不宣称约束原生峰值。
use crate::{capture::Image, image_color_profile};
use std::{marker::PhantomData, rc::Rc, sync::Arc};
use windows::Win32::{
    Storage::FileSystem::OPEN_EXISTING,
    UI::ColorSystem::{
        BM_16b_RGB, CloseColorProfile, CreateMultiProfileTransform, DeleteColorTransform,
        IsColorProfileValid, TranslateBitmapBits, WcsOpenColorProfileW, BEST_MODE, BM_RGBTRIPLETS,
        DONT_USE_EMBEDDED_WCS_PROFILES, INDEX_DONT_CARE, INTENT_RELATIVE_COLORIMETRIC, PROFILE,
        PROFILE_MEMBUFFER, PROFILE_READ,
    },
};

const SRGB_PROFILE: &[u8] = include_bytes!("../assets/color/srgb.icc");
const MAX_SIDE: u32 = 65_535;
const PROFILE_WORKSPACE: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct Layout {
    pixels_bytes: usize,
    row_bgra: usize,
    row_bgr: usize,
}

#[derive(Clone, Copy, Debug)]
struct Layout16 {
    pixels_bytes: usize,
    row_source_samples: usize,
    row_rgb_samples: usize,
    // 不含新目标和两条行缓冲；调用方实际驻留已在这里，不能再次漏计原16位整图。
    base_bytes: u64,
}

impl Layout16 {
    fn check_buffers(self, target: usize, input: usize, output: usize) -> Result<(), String> {
        self.base_bytes
            .checked_add(target as u64)
            .and_then(|bytes| bytes.checked_add((input as u64).checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add((output as u64).checked_mul(2)?))
            .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
            .ok_or_else(|| {
                message(
                    "16位颜色处理超过256MiB可见缓冲预算",
                    "16-bit color processing exceeds the 256 MiB visible-buffer budget",
                )
            })?;
        Ok(())
    }
}

fn layout16(
    data: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    profile_bytes: usize,
    resident_bytes: u64,
    max_output_bytes: u64,
) -> Result<Layout16, String> {
    let checked = budget_layout16(
        dimensions,
        channels,
        resident_bytes,
        profile_bytes,
        max_output_bytes,
    )?;
    if u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(channels as u64))
        != Some(data.len() as u64)
    {
        return Err(message(
            "16位颜色缓冲长度与尺寸不符",
            "16-bit color buffer length does not match its dimensions",
        ));
    }
    Ok(checked)
}

fn budget_layout16(
    dimensions: (u32, u32),
    channels: usize,
    resident_bytes: u64,
    profile_bytes: usize,
    max_output_bytes: u64,
) -> Result<Layout16, String> {
    let (width, height) = dimensions;
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || pixels > crate::image_decode::MAX_DECODE_PIXELS
        || !matches!(channels, 3 | 4)
    {
        return Err(message(
            "16位颜色尺寸、通道或缓冲长度无效",
            "Invalid 16-bit color dimensions, channels or buffer length",
        ));
    }
    let input_bytes = pixels
        .checked_mul(channels as u64 * 2)
        .ok_or_else(|| message("16位颜色长度溢出", "16-bit color length overflow"))?;
    let pixels_bytes = pixels * 4;
    if resident_bytes < input_bytes || pixels_bytes > max_output_bytes {
        return Err(message(
            "16位颜色驻留或输出预算不足",
            "Insufficient resident or output budget for 16-bit color",
        ));
    }
    let base_bytes = resident_bytes
        .checked_add(profile_bytes as u64)
        .and_then(|bytes| bytes.checked_add(SRGB_PROFILE.len() as u64))
        .and_then(|bytes| bytes.checked_add(PROFILE_WORKSPACE))
        .ok_or_else(|| message("16位颜色预算溢出", "16-bit color budget overflow"))?;
    let checked = Layout16 {
        pixels_bytes: usize::try_from(pixels_bytes)
            .map_err(|_| message("16位颜色长度溢出", "16-bit color length overflow"))?,
        row_source_samples: width as usize * channels,
        row_rgb_samples: width as usize * 3,
        base_bytes,
    };
    checked.check_buffers(
        checked.pixels_bytes,
        checked.row_rgb_samples,
        checked.row_rgb_samples,
    )?;
    Ok(checked)
}

/// 解码器分配16位输入之前的同一预算；resident_bytes此时包含预计的6P/8P输入。
/// 解码后manage_rgb16会用实际输入capacity再次检查，不能用本次预计值代替。
pub(crate) fn check_rgb16_budget(
    dimensions: (u32, u32),
    channels: usize,
    resident_bytes: u64,
    profile_bytes: usize,
    max_output_bytes: u64,
) -> Result<(), String> {
    budget_layout16(
        dimensions,
        channels,
        resident_bytes,
        profile_bytes,
        max_output_bytes,
    )
    .map(|_| ())
}

/// 在ICC摘要或系统CMM调用前检查所有可见缓冲；原生CMM私有内存不受此模型约束。
pub(crate) fn validate_rgb16(
    data: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    profile_bytes: usize,
    resident_bytes: u64,
    max_output_bytes: u64,
) -> Result<(), String> {
    layout16(
        data,
        dimensions,
        channels,
        profile_bytes,
        resident_bytes,
        max_output_bytes,
    )
    .map(|_| ())
}

fn message(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn check_cancel(cancel: &mut impl FnMut() -> bool) -> Result<(), String> {
    if cancel() {
        Err(message("颜色变换已取消", "Color conversion canceled"))
    } else {
        Ok(())
    }
}

fn layout(
    width: u32,
    height: u32,
    buffer_bytes: usize,
    source_profile_bytes: usize,
) -> Result<Layout, String> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_SIDE
        || height > MAX_SIDE
        || pixels > crate::image_decode::MAX_DECODE_PIXELS
    {
        return Err(message(
            "颜色变换尺寸无效或超过3200万像素上限",
            "Invalid color conversion dimensions or more than 32 million pixels",
        ));
    }
    let pixels_bytes = pixels
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| message("颜色像素长度溢出", "Color pixel length overflow"))?;
    if buffer_bytes != pixels_bytes {
        return Err(message(
            "颜色变换的BGRA缓冲长度与尺寸不符",
            "Color conversion BGRA buffer length does not match its dimensions",
        ));
    }
    let row_bgra = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| message("颜色像素行长度溢出", "Color pixel row length overflow"))?;
    let row_bgr = (width as usize)
        .checked_mul(3)
        .ok_or_else(|| message("颜色像素行长度溢出", "Color pixel row length overflow"))?;
    // 源和新图同时存在；另有一对BGR行缓冲及借用的源/目标配置。
    // 配置摘要的短期分配发生在新图分配前，不与两幅整图叠加。
    let _visible_bytes = (pixels_bytes as u64)
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add((row_bgr as u64).checked_mul(2)?))
        .and_then(|bytes| bytes.checked_add(source_profile_bytes as u64))
        .and_then(|bytes| bytes.checked_add(SRGB_PROFILE.len() as u64))
        .filter(|bytes| *bytes <= crate::image_decode::MAX_DECODE_BYTES)
        .ok_or_else(|| {
            message(
                "颜色变换的像素副本超过256MiB预算",
                "Color conversion pixel copies exceed the 256 MiB budget",
            )
        })?;
    Ok(Layout {
        pixels_bytes,
        row_bgra,
        row_bgr,
    })
}

fn check_rgb_profile(bytes: &[u8]) -> Result<(), String> {
    let profile = image_color_profile::inspect(bytes)?;
    if profile.color_space != *b"RGB "
        || !matches!(&profile.connection_space, b"XYZ " | b"Lab ")
        || !matches!(&profile.device_class, b"scnr" | b"mntr" | b"prtr" | b"spac")
    {
        return Err(message(
            "颜色变换仅支持RGB输入、显示、输出或颜色空间配置，PCS须为XYZ或Lab",
            "Color conversion requires an RGB input, display, output or color-space profile with XYZ or Lab PCS",
        ));
    }
    Ok(())
}

fn allocate(bytes: usize) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(bytes)
        .map_err(|_| message("颜色像素分配失败", "Color pixel allocation failed"))?;
    if buffer.capacity() > bytes {
        return Err(message(
            "颜色缓冲实际容量超过预留",
            "Color buffer capacity exceeds its reservation",
        ));
    }
    buffer.resize(bytes, 0);
    Ok(buffer)
}

fn native_error(zh: &'static str, en: &'static str) -> String {
    // 必须在其它系统调用之前读取当前线程的原生错误。
    let error = windows::core::Error::from_win32();
    format!("{}: {error}", crate::i18n::tr(zh, en))
}

struct ColorProfile(isize);

impl ColorProfile {
    fn open(bytes: &[u8]) -> Result<Self, String> {
        let descriptor = PROFILE {
            dwType: PROFILE_MEMBUFFER,
            pProfileData: bytes.as_ptr().cast_mut().cast(),
            cbDataSize: u32::try_from(bytes.len()).map_err(|_| {
                message(
                    "ICC配置长度超出系统范围",
                    "ICC profile length exceeds the system range",
                )
            })?,
        };
        // 只读内存配置；忽略ICC中嵌入的WCS XML，使用已经检查的ICC数据。
        let handle = unsafe {
            WcsOpenColorProfileW(
                &descriptor,
                None,
                None,
                PROFILE_READ,
                0,
                OPEN_EXISTING.0,
                DONT_USE_EMBEDDED_WCS_PROFILES as u32,
            )
        };
        if handle == 0 {
            return Err(native_error(
                "无法打开ICC配置",
                "Could not open ICC profile",
            ));
        }
        let profile = Self(handle);
        let mut valid = windows::core::BOOL::default();
        if !unsafe { IsColorProfileValid(profile.0, &mut valid) }.as_bool() {
            return Err(native_error(
                "ICC配置验证失败",
                "ICC profile validation failed",
            ));
        }
        if !valid.as_bool() {
            return Err(message(
                "系统拒绝此ICC配置",
                "The system rejected this ICC profile",
            ));
        }
        Ok(profile)
    }
}

impl Drop for ColorProfile {
    fn drop(&mut self) {
        let _ = unsafe { CloseColorProfile(Some(self.0)) };
    }
}

struct ColorTransform(isize);

impl ColorTransform {
    fn new(source: &ColorProfile, destination: &ColorProfile, flags: u32) -> Result<Self, String> {
        // 显式请求最高图像质量；该参数是CMM提示，不构成误差或原生内存上限。
        let handle = unsafe {
            CreateMultiProfileTransform(
                &[source.0, destination.0],
                &[INTENT_RELATIVE_COLORIMETRIC, INTENT_RELATIVE_COLORIMETRIC],
                flags,
                INDEX_DONT_CARE,
            )
        };
        if handle == 0 {
            Err(native_error(
                "无法创建颜色变换",
                "Could not create color transform",
            ))
        } else {
            Ok(Self(handle))
        }
    }

    fn row(&self, width: u32, source: &[u8], destination: &mut [u8]) -> Result<(), String> {
        let stride = width
            .checked_mul(3)
            .ok_or_else(|| message("颜色像素行长度溢出", "Color pixel row length overflow"))?;
        if source.len() != stride as usize || destination.len() != stride as usize {
            return Err(message(
                "颜色行缓冲长度不符",
                "Color row buffer length mismatch",
            ));
        }
        // BM_RGBTRIPLETS按低字节到高字节排列为B/G/R，与独立BGR行一致。
        // 系统输出不能覆盖原图或alpha字节。
        let translated = unsafe {
            TranslateBitmapBits(
                self.0,
                source.as_ptr().cast(),
                BM_RGBTRIPLETS,
                width,
                1,
                stride,
                destination.as_mut_ptr().cast(),
                BM_RGBTRIPLETS,
                stride,
                None,
                None,
            )
        };
        if translated.as_bool() {
            Ok(())
        } else {
            Err(native_error(
                "颜色像素变换失败",
                "Color pixel conversion failed",
            ))
        }
    }

    fn row16(&self, width: u32, source: &[u16], destination: &mut [u16]) -> Result<(), String> {
        let stride = width
            .checked_mul(6)
            .ok_or_else(|| message("颜色像素行长度溢出", "Color pixel row length overflow"))?;
        if source.len() != stride as usize / 2 || destination.len() != source.len() {
            return Err(message(
                "16位颜色行缓冲长度不符",
                "16-bit color row buffer length mismatch",
            ));
        }
        // BM_16b_RGB为R/G/B三个小端WORD，和8位BM_RGBTRIPLETS的B/G/R顺序不同。
        // 显式6W字节stride避免系统按DWORD补齐；alpha不进入CMM。
        let translated = unsafe {
            TranslateBitmapBits(
                self.0,
                source.as_ptr().cast(),
                BM_16b_RGB,
                width,
                1,
                stride,
                destination.as_mut_ptr().cast(),
                BM_16b_RGB,
                stride,
                None,
                None,
            )
        };
        if translated.as_bool() {
            Ok(())
        } else {
            Err(native_error(
                "16位颜色像素变换失败",
                "16-bit color pixel conversion failed",
            ))
        }
    }
}

impl Drop for ColorTransform {
    fn drop(&mut self) {
        let _ = unsafe { DeleteColorTransform(self.0) };
    }
}

struct NativeTransform {
    // 字段按声明顺序释放：变换必须早于它引用的两个配置句柄。
    transform: ColorTransform,
    _source: ColorProfile,
    _destination: ColorProfile,
}

impl NativeTransform {
    fn new(icc: &[u8], cancel: &mut impl FnMut() -> bool) -> Result<Self, String> {
        Self::with_flags(icc, BEST_MODE, cancel)
    }

    fn with_flags(
        icc: &[u8],
        flags: u32,
        cancel: &mut impl FnMut() -> bool,
    ) -> Result<Self, String> {
        check_cancel(cancel)?;
        let source = ColorProfile::open(icc)?;
        check_cancel(cancel)?;
        let destination = ColorProfile::open(SRGB_PROFILE)?;
        check_cancel(cancel)?;
        let transform = ColorTransform::new(&source, &destination, flags)?;
        check_cancel(cancel)?;
        Ok(Self {
            transform,
            _source: source,
            _destination: destination,
        })
    }
}

enum TransformMode {
    Identity,
    Native(NativeTransform),
}

/// 同一worker内复用已冻结来源的配置和原生变换；不跨worker或并发使用。
pub(crate) struct SrgbTransform {
    // mode先释放transform及profile句柄，再释放保留的原始配置字节。
    mode: TransformMode,
    profile: Arc<[u8]>,
    _worker_only: PhantomData<Rc<()>>,
}

impl SrgbTransform {
    /// 调用者仍须先证明来源RGB模型；创建时验证ICC，逐帧转换不重复解析配置。
    pub(crate) fn new(icc: Arc<[u8]>, mut cancel: impl FnMut() -> bool) -> Result<Self, String> {
        check_cancel(&mut cancel)?;
        check_rgb_profile(&icc)?;
        check_rgb_profile(SRGB_PROFILE)?;
        check_cancel(&mut cancel)?;
        let mode = if icc.as_ref() == SRGB_PROFILE {
            TransformMode::Identity
        } else {
            TransformMode::Native(NativeTransform::new(&icc, &mut cancel)?)
        };
        Ok(Self {
            mode,
            profile: icc,
            _worker_only: PhantomData,
        })
    }

    /// 保留的来源ICC字节数；不代表系统CMM内部的句柄与缓存分配。
    pub(crate) fn retained_profile_bytes(&self) -> usize {
        self.profile.len()
    }

    /// 直接从未修改的借用画布生成唯一交付图；不先复制一幅owned原图。
    /// 错误返回前会释放部分输出，调用者随后才能从原画布生成fallback。
    pub(crate) fn convert_pixels(
        &mut self,
        dimensions: (u32, u32),
        bgra: &[u8],
        mut cancel: impl FnMut() -> bool,
    ) -> Result<Image, String> {
        check_cancel(&mut cancel)?;
        let layout = layout(dimensions.0, dimensions.1, bgra.len(), self.profile.len())?;
        match &self.mode {
            TransformMode::Identity => {
                transform_pixels(dimensions, bgra, layout, &mut cancel, |input, output| {
                    output.copy_from_slice(input);
                    Ok(())
                })
            }
            TransformMode::Native(native) => {
                transform_pixels(dimensions, bgra, layout, &mut cancel, |input, output| {
                    native.transform.row(dimensions.0, input, output)
                })
            }
        }
    }

    pub(crate) fn convert(
        &mut self,
        source: &Image,
        cancel: impl FnMut() -> bool,
    ) -> Result<Image, String> {
        let mut image = self.convert_pixels((source.width, source.height), &source.bgra, cancel)?;
        image.readback_ms = source.readback_ms;
        Ok(image)
    }
}

fn transform_pixels(
    dimensions: (u32, u32),
    source: &[u8],
    layout: Layout,
    cancel: &mut impl FnMut() -> bool,
    mut translate: impl FnMut(&[u8], &mut [u8]) -> Result<(), String>,
) -> Result<Image, String> {
    check_cancel(cancel)?;
    let mut bgra = allocate(layout.pixels_bytes)?;
    let mut input_row = allocate(layout.row_bgr)?;
    let mut output_row = allocate(layout.row_bgr)?;
    for (input, output) in source
        .chunks_exact(layout.row_bgra)
        .zip(bgra.chunks_exact_mut(layout.row_bgra))
    {
        check_cancel(cancel)?;
        for (pixel, bgr) in input.chunks_exact(4).zip(input_row.chunks_exact_mut(3)) {
            bgr.copy_from_slice(&pixel[..3]);
        }
        translate(&input_row, &mut output_row)?;
        check_cancel(cancel)?;
        for ((original, pixel), bgr) in input
            .chunks_exact(4)
            .zip(output.chunks_exact_mut(4))
            .zip(output_row.chunks_exact(3))
        {
            pixel[..3].copy_from_slice(bgr);
            pixel[3] = original[3];
        }
    }
    check_cancel(cancel)?;
    Ok(Image {
        width: dimensions.0,
        height: dimensions.1,
        bgra,
        readback_ms: 0.0,
    })
}

fn transform_rows(
    source: &Image,
    layout: Layout,
    cancel: &mut impl FnMut() -> bool,
    translate: impl FnMut(&[u8], &mut [u8]) -> Result<(), String>,
) -> Result<Image, String> {
    let mut image = transform_pixels(
        (source.width, source.height),
        &source.bgra,
        layout,
        cancel,
        translate,
    )?;
    image.readback_ms = source.readback_ms;
    Ok(image)
}

fn native_to_srgb(
    source: &Image,
    layout: Layout,
    icc: &[u8],
    cancel: &mut impl FnMut() -> bool,
) -> Result<Image, String> {
    let native = NativeTransform::new(icc, cancel)?;
    // 变换在同一worker上创建、使用和释放；原生创建及单行调用期间不能强制中断。
    transform_rows(source, layout, cancel, |input, output| {
        native.transform.row(source.width, input, output)
    })
}

/// 调用者须先证明编码输入为RGB/YCbCr，且解码后的直alpha RGB仍属于此ICC配置。
/// 仅凭解码器输出Rgb8不能排除它已经将CMYK或YCCK转为RGB。
/// 返回独立副本；错误或取消时不发布部分结果，也不修改输入。
pub(crate) fn to_srgb(
    source: &Image,
    icc: &[u8],
    mut cancel: impl FnMut() -> bool,
) -> Result<Image, String> {
    check_cancel(&mut cancel)?;
    if icc.len() > image_color_profile::MAX_PROFILE_BYTES {
        return Err(message(
            "ICC配置超过4MiB上限",
            "ICC profile exceeds the 4 MiB limit",
        ));
    }
    let layout = layout(source.width, source.height, source.bgra.len(), icc.len())?;
    check_rgb_profile(icc)?;
    check_rgb_profile(SRGB_PROFILE)?;
    check_cancel(&mut cancel)?;
    if icc == SRGB_PROFILE {
        // 相同的固定配置无需经过CMM，避免无意义的舍入；仍返回独立、可取消的副本。
        return transform_rows(source, layout, &mut cancel, |input, output| {
            output.copy_from_slice(input);
            Ok(())
        });
    }
    native_to_srgb(source, layout, icc, &mut cancel)
}

fn quantize_word(value: u16) -> u8 {
    ((u32::from(value) * 255 + 32_767) / 65_535) as u8
}

fn allocate_words(samples: usize) -> Result<Vec<u16>, String> {
    let mut words = Vec::new();
    words
        .try_reserve_exact(samples)
        .map_err(|_| message("16位颜色行分配失败", "16-bit color row allocation failed"))?;
    Ok(words)
}

fn transform_words(
    source: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    layout: Layout16,
    cancel: &mut impl FnMut() -> bool,
    mut translate: impl FnMut(&[u16], &mut [u16]) -> Result<(), String>,
) -> Result<Image, String> {
    check_cancel(cancel)?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(layout.pixels_bytes)
        .map_err(|_| message("颜色像素分配失败", "Color pixel allocation failed"))?;
    layout.check_buffers(
        bgra.capacity(),
        layout.row_rgb_samples,
        layout.row_rgb_samples,
    )?;
    let mut input_row = allocate_words(layout.row_rgb_samples)?;
    layout.check_buffers(
        bgra.capacity(),
        input_row.capacity(),
        layout.row_rgb_samples,
    )?;
    let mut output_row = allocate_words(layout.row_rgb_samples)?;
    layout.check_buffers(bgra.capacity(), input_row.capacity(), output_row.capacity())?;
    bgra.resize(layout.pixels_bytes, 0);
    input_row.resize(layout.row_rgb_samples, 0);
    output_row.resize(layout.row_rgb_samples, 0);
    for (input, output) in source
        .chunks_exact(layout.row_source_samples)
        .zip(bgra.chunks_exact_mut(dimensions.0 as usize * 4))
    {
        check_cancel(cancel)?;
        for (pixel, rgb) in input
            .chunks_exact(channels)
            .zip(input_row.chunks_exact_mut(3))
        {
            rgb.copy_from_slice(&pixel[..3]);
        }
        let result = translate(&input_row, &mut output_row);
        check_cancel(cancel)?;
        result?;
        for ((original, target), rgb) in input
            .chunks_exact(channels)
            .zip(output.chunks_exact_mut(4))
            .zip(output_row.chunks_exact(3))
        {
            target.copy_from_slice(&[
                quantize_word(rgb[2]),
                quantize_word(rgb[1]),
                quantize_word(rgb[0]),
                quantize_word(if channels == 4 { original[3] } else { 65_535 }),
            ]);
        }
    }
    check_cancel(cancel)?;
    Ok(Image {
        width: dimensions.0,
        height: dimensions.1,
        bgra,
        readback_ms: 0.,
    })
}

/// 无配置/不支持/变换失败时，从完整原WORD输入生成原色8位副本。
pub(crate) fn quantize_rgb16(
    data: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    profile_bytes: usize,
    resident_bytes: u64,
    max_output_bytes: u64,
    mut cancel: impl FnMut() -> bool,
) -> Result<Image, String> {
    check_cancel(&mut cancel)?;
    let layout = layout16(
        data,
        dimensions,
        channels,
        profile_bytes,
        resident_bytes,
        max_output_bytes,
    )?;
    transform_words(
        data,
        dimensions,
        channels,
        layout,
        &mut cancel,
        |input, output| {
            output.copy_from_slice(input);
            Ok(())
        },
    )
}

/// 原16位设备域RGB以BM_16b_RGB交给CMM；目标RGB WORD完成后才量化，alpha独立量化。
/// 调用方须证明RGB设备域和配置匹配，并在resident_bytes计入所有仍持有的实际capacity。
pub(crate) fn to_srgb16(
    data: &[u16],
    dimensions: (u32, u32),
    channels: usize,
    icc: &[u8],
    resident_bytes: u64,
    max_output_bytes: u64,
    mut cancel: impl FnMut() -> bool,
) -> Result<Image, String> {
    check_cancel(&mut cancel)?;
    let layout = layout16(
        data,
        dimensions,
        channels,
        icc.len(),
        resident_bytes,
        max_output_bytes,
    )?;
    let profiles = check_rgb_profile(icc).and_then(|_| check_rgb_profile(SRGB_PROFILE));
    check_cancel(&mut cancel)?;
    profiles?;
    if icc == SRGB_PROFILE {
        return transform_words(
            data,
            dimensions,
            channels,
            layout,
            &mut cancel,
            |input, output| {
                output.copy_from_slice(input);
                Ok(())
            },
        );
    }
    let native = NativeTransform::new(icc, &mut cancel);
    check_cancel(&mut cancel)?;
    let native = native?;
    // 创建及单行系统调用只能在前后响应取消；失败不交付部分目标。
    transform_words(
        data,
        dimensions,
        channels,
        layout,
        &mut cancel,
        |input, output| native.transform.row16(dimensions.0, input, output),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINEAR_RGB_PROFILE: &[u8] = include_bytes!("../tests/data/image_color/linear-rgb.icc");
    const CHANNEL_TRC_RGB_PROFILE: &[u8] =
        include_bytes!("../tests/data/image_color/channel-trc-rgb.icc");

    fn sample() -> Image {
        Image {
            width: 2,
            height: 2,
            bgra: vec![1, 2, 3, 0, 4, 5, 6, 128, 7, 8, 9, 255, 10, 11, 12, 64],
            readback_ms: 3.5,
        }
    }

    #[test]
    fn image_color_transform_word_rgb_order_and_straight_alpha_are_independent() {
        let source = [0x1234, 0x5678, 0x9abc, 0, 65_535, 32_768, 256, 32_768];
        let checked = layout16(&source, (2, 1), 4, SRGB_PROFILE.len(), 16, 8).unwrap();
        let output = transform_words(
            &source,
            (2, 1),
            4,
            checked,
            &mut || false,
            |input, output| {
                // WORD RGB不沿用旧8位BGR行序，透明像素的颜色也必须完整进入CMM。
                assert_eq!(input, [0x1234, 0x5678, 0x9abc, 65_535, 32_768, 256]);
                output.copy_from_slice(input);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output.bgra, [154, 86, 18, 0, 1, 128, 255, 128]);
        assert_eq!(
            source,
            [0x1234, 0x5678, 0x9abc, 0, 65_535, 32_768, 256, 32_768]
        );
        let rgb = to_srgb16(
            &[0x1234, 0x5678, 0x9abc],
            (1, 1),
            3,
            SRGB_PROFILE,
            6,
            4,
            || false,
        )
        .unwrap();
        assert_eq!(rgb.bgra, [154, 86, 18, 255]);
    }

    #[test]
    fn image_color_transform_word_low_bits_reach_the_nonlinear_transform_before_quantization() {
        let source = [128, 128, 128, 32_768];
        let checked = layout16(&source, (1, 1), 4, LINEAR_RGB_PROFILE.len(), 8, 4).unwrap();
        let output = transform_words(
            &source,
            (1, 1),
            4,
            checked,
            &mut || false,
            |input, output| {
                assert_eq!(input, [128, 128, 128]);
                // 纯注入模拟linear RGB→sRGB TRC；不调用系统CMM，也不声称验证了其实现。
                for (&linear, target) in input.iter().zip(output) {
                    let linear = f64::from(linear) / 65_535.;
                    let encoded = if linear <= 0.003_130_8 {
                        12.92 * linear
                    } else {
                        1.055 * linear.powf(1. / 2.4) - 0.055
                    };
                    *target = (encoded * 65_535.).round() as u16;
                }
                Ok(())
            },
        )
        .unwrap();
        // 128/65535的线性灰应到约6/255；先降8位则成为0，无法在后续恢复。
        assert_eq!(output.bgra, [6, 6, 6, 128]);
        assert_eq!(quantize_word(128), 0);
        let unconverted = quantize_rgb16(&source, (1, 1), 4, 0, 8, 4, || false).unwrap();
        assert_eq!(unconverted.bgra, [0, 0, 0, 128]);
    }

    #[test]
    fn image_color_transform_word_budget_covers_resident_rows_profiles_and_actual_capacity() {
        let profile = SRGB_PROFILE.len();
        let overhead = 4 + 12 + profile as u64 + SRGB_PROFILE.len() as u64 + PROFILE_WORKSPACE;
        let resident = crate::image_decode::MAX_DECODE_BYTES - overhead;
        check_rgb16_budget((1, 1), 4, resident, profile, 4).unwrap();
        assert!(check_rgb16_budget((1, 1), 4, resident + 1, profile, 4).is_err());
        let checked = layout16(&[0; 4], (1, 1), 4, profile, resident, 4).unwrap();
        checked.check_buffers(4, 3, 3).unwrap();
        assert!(checked.check_buffers(5, 3, 3).is_err());
        assert!(checked.check_buffers(4, 4, 3).is_err());
        assert!(checked.check_buffers(4, 3, 4).is_err());
        for (dims, channels, resident, output) in [
            ((0, 1), 4, 8, 4),
            ((65_536, 1), 4, 524_288, u64::MAX),
            ((8_000, 4_001), 4, 256_064_000, u64::MAX),
            ((1, 1), 2, 8, 4),
            ((1, 1), 4, 7, 4),
            ((1, 1), 4, 8, 3),
            ((1, 1), 4, u64::MAX, 4),
        ] {
            assert!(check_rgb16_budget(dims, channels, resident, profile, output).is_err());
        }
        assert!(validate_rgb16(&[0; 3], (1, 1), 4, 0, 8, 4).is_err());
        assert!(validate_rgb16(&[0; 5], (1, 1), 4, 0, 10, 4).is_err());
    }

    #[test]
    fn image_color_transform_word_partial_failure_and_cancellation_never_publish_pixels() {
        use std::cell::Cell;
        let source = [1, 2, 3, 0, 4, 5, 6, 65_535];
        let checked = layout16(&source, (1, 2), 4, SRGB_PROFILE.len(), 16, 8).unwrap();
        let rows = Cell::new(0);
        let failed = transform_words(&source, (1, 2), 4, checked, &mut || false, |_, output| {
            output.fill(65_535);
            rows.set(rows.get() + 1);
            if rows.get() == 2 {
                Err("second row failed".into())
            } else {
                Ok(())
            }
        });
        assert!(failed.is_err());
        assert_eq!(rows.get(), 2);
        for fail in [false, true] {
            let cancelled = Cell::new(false);
            let output = transform_words(
                &source,
                (1, 2),
                4,
                checked,
                &mut || cancelled.get(),
                |_, output| {
                    output.fill(65_535);
                    cancelled.set(true);
                    if fail {
                        Err("native row failed".into())
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(
                output.err().unwrap(),
                message("颜色变换已取消", "Color conversion canceled")
            );
        }
        assert_eq!(source, [1, 2, 3, 0, 4, 5, 6, 65_535]);
        assert!(to_srgb16(&[], (0, 0), 0, &[], 0, 0, || true).is_err());
    }

    #[test]
    #[ignore = "调用系统16位颜色管理；需显式运行，无窗口"]
    fn image_color_transform_native_word_linear_low_bits_survive_before_srgb_quantization() {
        let output = to_srgb16(
            &[128, 128, 128, 32_768],
            (1, 1),
            4,
            LINEAR_RGB_PROFILE,
            8,
            4,
            || false,
        )
        .unwrap();
        for value in &output.bgra[..3] {
            assert!(
                value.abs_diff(6) <= 1,
                "native WORD linear gray was {value}, expected about 6"
            );
        }
        assert_eq!(output.bgra[3], 128);
    }

    #[test]
    fn image_color_transform_checks_dimensions_buffer_and_copy_budget_before_allocation() {
        assert!(layout(0, 1, 0, SRGB_PROFILE.len()).is_err());
        assert!(layout(65_536, 1, 65_536 * 4, SRGB_PROFILE.len()).is_err());
        assert!(layout(8_000, 4_001, 8_000 * 4_001 * 4, SRGB_PROFILE.len()).is_err());
        assert!(layout(2, 2, 15, SRGB_PROFILE.len()).is_err());
        assert!(layout(
            8_000,
            4_000,
            128_000_000,
            image_color_profile::MAX_PROFILE_BYTES
        )
        .is_ok());
        assert!(layout(8_000, 4_000, 128_000_000, usize::MAX).is_err());
    }

    #[test]
    fn image_color_transform_rejects_non_rgb_and_unsupported_profile_models() {
        check_rgb_profile(SRGB_PROFILE).unwrap();
        check_rgb_profile(LINEAR_RGB_PROFILE).unwrap();
        check_rgb_profile(CHANNEL_TRC_RGB_PROFILE).unwrap();
        for class in [*b"scnr", *b"mntr", *b"prtr", *b"spac"] {
            let mut profile = SRGB_PROFILE.to_vec();
            profile[12..16].copy_from_slice(&class);
            check_rgb_profile(&profile).unwrap();
        }
        for (offset, value) in [
            (16, *b"CMYK"),
            (16, *b"GRAY"),
            (12, *b"link"),
            (12, *b"abst"),
            (20, *b"RGB "),
        ] {
            let mut profile = SRGB_PROFILE.to_vec();
            profile[offset..offset + 4].copy_from_slice(&value);
            assert!(check_rgb_profile(&profile).is_err());
        }
        assert!(check_rgb_profile(&[0; 132]).is_err());
    }

    #[test]
    fn image_color_transform_maps_only_bgr_and_preserves_straight_alpha_and_input() {
        let source = sample();
        let original = source.bgra.clone();
        let checked = layout(2, 2, source.bgra.len(), SRGB_PROFILE.len()).unwrap();
        let output = transform_rows(&source, checked, &mut || false, |input, output| {
            for (input, output) in input.chunks_exact(3).zip(output.chunks_exact_mut(3)) {
                output.copy_from_slice(&[input[2], input[1], input[0]]);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            output.bgra,
            [3, 2, 1, 0, 6, 5, 4, 128, 9, 8, 7, 255, 12, 11, 10, 64]
        );
        assert_eq!(source.bgra, original);
        assert_eq!(output.readback_ms, source.readback_ms);
    }

    #[test]
    fn image_color_transform_discards_cancelled_or_failed_partial_rows() {
        let source = sample();
        let original = source.bgra.clone();
        let checked = layout(2, 2, source.bgra.len(), SRGB_PROFILE.len()).unwrap();
        let mut checks = 0;
        let mut rows = 0;
        let cancelled = transform_rows(
            &source,
            checked,
            &mut || {
                checks += 1;
                checks == 4
            },
            |input, output| {
                rows += 1;
                output.copy_from_slice(input);
                Ok(())
            },
        );
        assert!(cancelled.is_err());
        assert_eq!(rows, 1);
        let failed = transform_rows(&source, checked, &mut || false, |_, output| {
            output.fill(255);
            Err("stopped".into())
        });
        assert!(failed.is_err());
        assert_eq!(source.bgra, original);
    }

    #[test]
    fn image_color_transform_fixed_srgb_copies_exact_pixels_without_mutating_input() {
        let source = sample();
        let output = to_srgb(&source, SRGB_PROFILE, || false).unwrap();
        assert_eq!(output.bgra, source.bgra);
        assert_ne!(output.bgra.as_ptr(), source.bgra.as_ptr());
        assert!(to_srgb(&source, SRGB_PROFILE, || true).is_err());
    }

    #[test]
    fn image_color_transform_cached_identity_converts_borrowed_frames_without_a_raw_image_copy() {
        let profile: Arc<[u8]> = Arc::from(SRGB_PROFILE);
        let mut transform = SrgbTransform::new(profile.clone(), || false).unwrap();
        assert_eq!(transform.retained_profile_bytes(), SRGB_PROFILE.len());
        assert_eq!(Arc::strong_count(&profile), 2);
        let first = sample();
        let first_output = transform
            .convert_pixels((first.width, first.height), &first.bgra, || false)
            .unwrap();
        assert_eq!(first_output.bgra, first.bgra);
        assert_ne!(first_output.bgra.as_ptr(), first.bgra.as_ptr());
        assert_eq!(first_output.readback_ms, 0.0);
        let second = [29, 61, 113, 0, 5, 8, 13, 128, 17, 19, 23, 255];
        let second_output = transform.convert_pixels((1, 3), &second, || false).unwrap();
        assert_eq!((second_output.width, second_output.height), (1, 3));
        assert_eq!(second_output.bgra, second);
        assert_eq!(first_output.bgra, first.bgra);
        let compatible = transform.convert(&first, || false).unwrap();
        assert_eq!(compatible.bgra, first.bgra);
        assert_eq!(compatible.readback_ms, first.readback_ms);
        drop(transform);
        assert_eq!(Arc::strong_count(&profile), 1);
    }

    #[test]
    fn image_color_transform_cached_identity_keeps_canvas_after_failure_and_cancel_then_recovers() {
        let mut transform = SrgbTransform::new(Arc::from(SRGB_PROFILE), || false).unwrap();
        let source = sample();
        let original = source.bgra.clone();
        assert!(transform
            .convert_pixels((2, 2), &source.bgra[..15], || false)
            .is_err());
        let mut checks = 0;
        assert!(transform
            .convert_pixels((2, 2), &source.bgra, || {
                checks += 1;
                checks >= 5
            })
            .is_err());
        assert_eq!(source.bgra, original);
        let after_cancel = transform
            .convert_pixels((2, 2), &source.bgra, || false)
            .unwrap();
        assert_eq!(after_cancel.bgra, original);
        let mut invalid_profile = SRGB_PROFILE.to_vec();
        invalid_profile[16..20].copy_from_slice(b"CMYK");
        assert!(SrgbTransform::new(Arc::from(invalid_profile), || false).is_err());
        assert!(SrgbTransform::new(Arc::from(SRGB_PROFILE), || true).is_err());
    }

    #[test]
    #[ignore = "调用系统颜色管理；需显式运行，无窗口"]
    fn image_color_transform_native_cmm_keeps_srgb_channels_and_alpha() {
        let source = sample();
        // 先核对生产同配置分支；下面的原生探针刻意绕过该分支，不能替代它的合同。
        let direct = to_srgb(&source, SRGB_PROFILE, || false).unwrap();
        assert_eq!(direct.bgra, source.bgra);
        let mut cached = SrgbTransform::new(Arc::from(SRGB_PROFILE), || false).unwrap();
        assert!(matches!(&cached.mode, TransformMode::Identity));
        let cached_output = cached
            .convert_pixels((source.width, source.height), &source.bgra, || false)
            .unwrap();
        assert_eq!(cached_output.bgra, source.bgra);
        let checked = layout(2, 2, source.bgra.len(), SRGB_PROFILE.len()).unwrap();
        // 绕过相同配置的直接复制，实际覆盖系统profile、transform和逐行调用。
        // 旧flags=0仅作完整输出对照；验收仍要求显式BEST_MODE通过原误差上限。
        let legacy = NativeTransform::with_flags(SRGB_PROFILE, 0, &mut || false)
            .and_then(|native| {
                transform_rows(&source, checked, &mut || false, |input, output| {
                    native.transform.row(source.width, input, output)
                })
            })
            .map(|image| image.bgra);
        let output =
            native_to_srgb(&source, checked, SRGB_PROFILE, &mut || false).unwrap_or_else(|error| {
                panic!("BEST_MODE transform failed: {error}; flags_0_bgra={legacy:?}")
            });
        eprintln!(
            "same-profile CMM input_bgra={:?}, flags_0_bgra={legacy:?}, BEST_MODE_bgra={:?}, dimensions={}x{}, BM_RGBTRIPLETS stride={}",
            source.bgra, output.bgra, source.width, source.height, source.width * 3,
        );
        assert_eq!((output.width, output.height), (source.width, source.height));
        assert_eq!(output.bgra.len(), source.bgra.len());
        for (index, (input, actual)) in source
            .bgra
            .chunks_exact(4)
            .zip(output.bgra.chunks_exact(4))
            .enumerate()
        {
            assert_eq!(
                input[3], actual[3],
                "BEST_MODE alpha pixel={index}, input_bgra={:?}, flags_0_bgra={legacy:?}, BEST_MODE_bgra={:?}",
                source.bgra, output.bgra,
            );
            for (channel, name) in ["B", "G", "R"].into_iter().enumerate() {
                let delta = input[channel].abs_diff(actual[channel]);
                assert!(
                    delta <= 2,
                    "BEST_MODE same-profile CMM pixel=({},{}) channel={name}, input={}, actual={}, delta={delta}, dimensions={}x{}, BM_RGBTRIPLETS stride={}, input_bgra={:?}, flags_0_bgra={legacy:?}, BEST_MODE_bgra={:?}; production direct/cache identity paths were byte-exact",
                    index % source.width as usize,
                    index / source.width as usize,
                    input[channel],
                    actual[channel],
                    source.width,
                    source.height,
                    source.width * 3,
                    source.bgra,
                    output.bgra,
                );
            }
        }
    }

    #[test]
    #[ignore = "调用系统颜色管理；需显式运行，无窗口"]
    fn image_color_transform_native_linear_rgb_matches_reference_and_preserves_alpha() {
        // 固定线性RGB矩阵配置及独立颜色变换参考，使用非边界颜色识别未转换的输出。
        const INPUT: [u8; 24] = [
            128, 128, 128, 255, 192, 128, 64, 128, 160, 96, 32, 0, 40, 80, 200, 255, 220, 140, 20,
            128, 108, 36, 180, 0,
        ];
        const EXPECTED: [u8; 24] = [
            188, 188, 188, 255, 225, 188, 137, 128, 208, 165, 99, 0, 110, 152, 229, 255, 239, 196,
            79, 128, 174, 105, 219, 0,
        ];
        assert_ne!(LINEAR_RGB_PROFILE, SRGB_PROFILE);
        let source = Image {
            width: 3,
            height: 2,
            bgra: INPUT.to_vec(),
            readback_ms: 0.0,
        };
        let output = to_srgb(&source, LINEAR_RGB_PROFILE, || false).unwrap();
        assert_eq!(source.bgra, INPUT);
        assert_eq!((output.width, output.height), (3, 2));
        assert!(output.bgra[0].abs_diff(INPUT[0]) >= 40);
        for ((input, actual), expected) in source
            .bgra
            .chunks_exact(4)
            .zip(output.bgra.chunks_exact(4))
            .zip(EXPECTED.chunks_exact(4))
        {
            assert_eq!(actual[3], input[3]);
            assert_eq!(actual[3], expected[3]);
            for channel in 0..3 {
                assert!(
                    actual[channel].abs_diff(expected[channel]) <= 3,
                    "channel {channel}: actual={}, expected={}",
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    #[ignore = "调用系统颜色管理；需显式运行，无窗口"]
    fn image_color_transform_native_channel_trcs_use_bgra_order_and_match_reference() {
        const INPUT: [u8; 24] = [
            128, 128, 128, 255, 192, 128, 64, 128, 160, 96, 32, 0, 40, 80, 200, 255, 220, 140, 20,
            128, 108, 36, 180, 0,
        ];
        const EXPECTED: [u8; 24] = [
            121, 146, 188, 255, 189, 146, 137, 128, 155, 115, 99, 0, 28, 99, 229, 255, 218, 158,
            79, 128, 100, 48, 219, 0,
        ];
        let source = Image {
            width: 3,
            height: 2,
            bgra: INPUT.to_vec(),
            readback_ms: 0.0,
        };
        // 三通道曲线不同，能识别输入与输出两次红蓝交换彼此抵消的错误。
        let output = to_srgb(&source, CHANNEL_TRC_RGB_PROFILE, || false).unwrap();
        assert_eq!(source.bgra, INPUT);
        assert_eq!((output.width, output.height), (3, 2));
        assert!(u16::from(output.bgra[2]) > u16::from(output.bgra[0]) + 40);
        for ((input, actual), expected) in source
            .bgra
            .chunks_exact(4)
            .zip(output.bgra.chunks_exact(4))
            .zip(EXPECTED.chunks_exact(4))
        {
            assert_eq!(actual[3], input[3]);
            assert_eq!(actual[3], expected[3]);
            for channel in 0..3 {
                assert!(
                    actual[channel].abs_diff(expected[channel]) <= 3,
                    "channel {channel}: actual={}, expected={}",
                    actual[channel],
                    expected[channel]
                );
            }
        }
    }

    #[test]
    #[ignore = "调用系统颜色管理；需显式运行，无窗口"]
    fn image_color_transform_native_cached_frames_match_linear_and_channel_trc_references() {
        const INPUT: [u8; 24] = [
            128, 128, 128, 255, 192, 128, 64, 128, 160, 96, 32, 0, 40, 80, 200, 255, 220, 140, 20,
            128, 108, 36, 180, 0,
        ];
        const LINEAR: [u8; 24] = [
            188, 188, 188, 255, 225, 188, 137, 128, 208, 165, 99, 0, 110, 152, 229, 255, 239, 196,
            79, 128, 174, 105, 219, 0,
        ];
        const CHANNEL_TRC: [u8; 24] = [
            121, 146, 188, 255, 189, 146, 137, 128, 155, 115, 99, 0, 28, 99, 229, 255, 218, 158,
            79, 128, 100, 48, 219, 0,
        ];
        for (profile, expected) in [
            (LINEAR_RGB_PROFILE, LINEAR),
            (CHANNEL_TRC_RGB_PROFILE, CHANNEL_TRC),
        ] {
            let mut transform = SrgbTransform::new(Arc::from(profile), || false).unwrap();
            let handle = match &transform.mode {
                TransformMode::Native(native) => native.transform.0,
                TransformMode::Identity => panic!("nonidentity profile must use the CMM"),
            };
            // 第二帧逐像素倒序，防止缓存了上帧输出；取消后仍可用同一个变换处理新帧。
            for reverse in [false, true, false] {
                let mut input = INPUT.to_vec();
                let mut reference = expected.to_vec();
                if reverse {
                    input = input.chunks_exact(4).rev().flatten().copied().collect();
                    reference = reference.chunks_exact(4).rev().flatten().copied().collect();
                }
                let mut checks = 0;
                assert!(transform
                    .convert_pixels((3, 2), &input, || {
                        checks += 1;
                        checks >= 5
                    })
                    .is_err());
                let original = input.clone();
                let output = transform.convert_pixels((3, 2), &input, || false).unwrap();
                assert_eq!(input, original);
                assert_eq!(output.readback_ms, 0.0);
                for ((source, actual), expected) in input
                    .chunks_exact(4)
                    .zip(output.bgra.chunks_exact(4))
                    .zip(reference.chunks_exact(4))
                {
                    assert_eq!(actual[3], source[3]);
                    for channel in 0..3 {
                        assert!(actual[channel].abs_diff(expected[channel]) <= 3);
                    }
                }
                match &transform.mode {
                    TransformMode::Native(native) => assert_eq!(native.transform.0, handle),
                    TransformMode::Identity => panic!("the cached transform changed mode"),
                }
            }
        }
    }
}
