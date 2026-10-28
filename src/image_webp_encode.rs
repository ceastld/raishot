//! WebP写出选项与独立有损像素编码；保存事务由统一导出入口负责。
use std::sync::atomic::{AtomicBool, Ordering};
use std::{ffi::c_int, mem::MaybeUninit, panic::AssertUnwindSafe};

use crate::capture::Image;
use libwebp_sys as webp;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WebpMode {
    Lossless,
    Lossy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WebpEncoding {
    pub mode: WebpMode,
    pub quality: u8,
}

impl Default for WebpEncoding {
    fn default() -> Self {
        Self {
            mode: WebpMode::Lossless,
            quality: 90,
        }
    }
}

impl WebpEncoding {
    pub(crate) fn validate(self) -> crate::Result<()> {
        if !(1..=100).contains(&self.quality) {
            return Err(tr!("WebP质量须为1–100", "WebP quality must be 1–100").into());
        }
        Ok(())
    }
}

const MAX_RESIDENT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PIXELS: u64 = 32_000_000;
const NATIVE_FIXED_BYTES: u64 = 4 * 1024 * 1024;
const COPY_CHUNK_BYTES: usize = 64 * 1024;

fn cancelled(cancel: &AtomicBool) -> crate::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("WebP编码已取消", "WebP encoding canceled").into())
    } else {
        Ok(())
    }
}

fn budget_error() -> Box<dyn std::error::Error> {
    tr!(
        "WebP编码的已建模工作区超过256MiB预算",
        "Modeled WebP encode workspace exceeds the 256 MiB budget"
    )
    .into()
}

fn sum(parts: &[u64]) -> crate::Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or_else(budget_error)
}

fn fits(parts: &[u64]) -> crate::Result<()> {
    if sum(parts)? > MAX_RESIDENT_BYTES {
        Err(budget_error())
    } else {
        Ok(())
    }
}

/// 只计算导入平面、转换行、raw ALPH与控制结构预留，不声称已穷尽C编码器私有heap。
#[derive(Clone, Copy, Debug)]
struct NativeModel {
    picture_bytes: u64,
    import_extra_bytes: u64,
    encode_extra_bytes: u64,
}

impl NativeModel {
    fn new(width: u32, height: u32, has_alpha: bool) -> crate::Result<Self> {
        let width = u64::from(width);
        let height = u64::from(height);
        let pixels = width.checked_mul(height).ok_or_else(budget_error)?;
        let uv_width = width.div_ceil(2);
        let uv = uv_width
            .checked_mul(height.div_ceil(2))
            .ok_or_else(budget_error)?;
        let picture_bytes = sum(&[
            pixels,
            uv.checked_mul(2).ok_or_else(budget_error)?,
            if has_alpha { pixels } else { 0 },
        ])?;
        // 直接BGRA→YUVA转换只保一条4×uv_width的u16累计行，无ARGB整幅副本。
        let import_extra_bytes = uv_width.checked_mul(8).ok_or_else(budget_error)?;
        // raw ALPH路径不调用VP8L；quant_alpha与初始bitwriter同时存在。
        // 该writer一次预留1+P（最低1024），随后写入header+P不再扩容。
        let alpha = if has_alpha {
            sum(&[pixels, sum(&[pixels, 1])?.max(1024)])?
        } else {
            0
        };
        let blocks = width
            .div_ceil(16)
            .checked_mul(height.div_ceil(16))
            .ok_or_else(budget_error)?;
        // 保守控制结构/预测目录及行预留；码流私有bitwriter尚无完整硬容量证明。
        let control = sum(&[
            NATIVE_FIXED_BYTES,
            blocks.checked_mul(128).ok_or_else(budget_error)?,
            width.checked_mul(64).ok_or_else(budget_error)?,
        ])?;
        Ok(Self {
            picture_bytes,
            import_extra_bytes,
            encode_extra_bytes: sum(&[control, alpha])?,
        })
    }

    fn peak_bytes(self) -> crate::Result<u64> {
        sum(&[
            self.picture_bytes,
            self.import_extra_bytes.max(self.encode_extra_bytes),
        ])
    }
}

struct Picture(webp::WebPPicture);

impl Picture {
    fn new() -> crate::Result<Self> {
        let mut picture = MaybeUninit::<webp::WebPPicture>::uninit();
        // 只在ABI初始化成功后读取完整C结构；失败时不访问未初始化字段。
        if unsafe {
            webp::WebPPictureInitInternal(
                picture.as_mut_ptr(),
                webp::WEBP_ENCODER_ABI_VERSION as c_int,
            )
        } == 0
        {
            return Err(tr!(
                "WebP图片接口版本不兼容",
                "Incompatible WebP picture API version"
            )
            .into());
        }
        Ok(Self(unsafe { picture.assume_init() }))
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // 只释放本结构通过Import分配的平面，不接管借用的来源BGRA。
        unsafe { webp::WebPPictureFree(&mut self.0) };
    }
}

fn config(quality: u8) -> crate::Result<webp::WebPConfig> {
    WebpEncoding {
        mode: WebpMode::Lossy,
        quality,
    }
    .validate()?;
    // ABI初始化器不保证写入reserved整数；先清零，确保成功后整个Rust值均已初始化。
    let mut config = MaybeUninit::<webp::WebPConfig>::zeroed();
    if unsafe {
        webp::WebPConfigInitInternal(
            config.as_mut_ptr(),
            webp::WebPPreset::WEBP_PRESET_DEFAULT,
            f32::from(quality),
            webp::WEBP_ENCODER_ABI_VERSION as c_int,
        )
    } == 0
    {
        return Err(tr!(
            "WebP配置接口版本不兼容",
            "Incompatible WebP configuration API version"
        )
        .into());
    }
    let mut config = unsafe { config.assume_init() };
    config.lossless = 0;
    config.quality = f32::from(quality);
    config.alpha_quality = 100;
    config.alpha_compression = 0;
    config.alpha_filtering = 0;
    config.exact = 1;
    config.thread_level = 0;
    config.low_memory = 1;
    config.use_sharp_yuv = 0;
    config.preprocessing = 0;
    config.show_compressed = 0;
    if unsafe { webp::WebPValidateConfig(&config) } == 0 {
        return Err(tr!("WebP编码配置无效", "Invalid WebP encode configuration").into());
    }
    Ok(config)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallbackFailure {
    OutputLimit,
    ResidentBudget,
    Allocation,
    InvalidInput,
    Panicked,
    Cancelled,
}

impl CallbackFailure {
    fn error(self) -> Box<dyn std::error::Error> {
        match self {
            Self::OutputLimit => tr!(
                "WebP编码字节超过输出上限",
                "WebP encoded bytes exceed the output limit"
            )
            .into(),
            Self::ResidentBudget => budget_error(),
            Self::Allocation => {
                tr!("无法分配WebP输出缓冲", "Cannot allocate WebP output buffer").into()
            }
            Self::InvalidInput => {
                tr!("WebP写出回调参数无效", "Invalid WebP writer callback input").into()
            }
            Self::Panicked => tr!("WebP写出回调失败", "WebP writer callback failed").into(),
            Self::Cancelled => tr!("WebP编码已取消", "WebP encoding canceled").into(),
        }
    }
}

struct EncodeContext<'a> {
    bytes: Vec<u8>,
    max_encoded_bytes: usize,
    resident_bytes: u64,
    cancel: &'a AtomicBool,
    failure: Option<CallbackFailure>,
    #[cfg(test)]
    cancel_on_progress: bool,
    #[cfg(test)]
    progress_calls: usize,
}

impl<'a> EncodeContext<'a> {
    fn new(resident_bytes: u64, max_encoded_bytes: usize, cancel: &'a AtomicBool) -> Self {
        Self {
            bytes: Vec::new(),
            max_encoded_bytes,
            resident_bytes,
            cancel,
            failure: None,
            #[cfg(test)]
            cancel_on_progress: false,
            #[cfg(test)]
            progress_calls: 0,
        }
    }

    fn fail(&mut self, failure: CallbackFailure) -> bool {
        if self.failure.is_none() {
            self.failure = Some(failure);
        }
        false
    }

    fn check_cancel(&mut self) -> bool {
        if self.cancel.load(Ordering::Relaxed) {
            self.fail(CallbackFailure::Cancelled)
        } else {
            true
        }
    }

    fn reserve(&mut self, additional: usize) -> bool {
        if !self.check_cancel() || self.failure.is_some() {
            return false;
        }
        let Some(needed) = self.bytes.len().checked_add(additional) else {
            return self.fail(CallbackFailure::OutputLimit);
        };
        if needed > self.max_encoded_bytes || needed > isize::MAX as usize {
            return self.fail(CallbackFailure::OutputLimit);
        }
        if needed <= self.bytes.capacity() {
            return true;
        }
        let old_capacity = self.bytes.capacity() as u64;
        if fits(&[self.resident_bytes, old_capacity, needed as u64]).is_err() {
            return self.fail(CallbackFailure::ResidentBudget);
        }
        // 新旧Vec同时保留，以便实际capacity也可在旧块释放前核查；不依赖realloc实现。
        let mut grown = Vec::new();
        if grown.try_reserve_exact(needed).is_err() {
            return self.fail(CallbackFailure::Allocation);
        }
        if grown.capacity() > self.max_encoded_bytes
            || fits(&[self.resident_bytes, old_capacity, grown.capacity() as u64]).is_err()
        {
            return self.fail(CallbackFailure::ResidentBudget);
        }
        for part in self.bytes.chunks(COPY_CHUNK_BYTES) {
            if self.cancel.load(Ordering::Relaxed) {
                return self.fail(CallbackFailure::Cancelled);
            }
            grown.extend_from_slice(part);
        }
        self.bytes = grown;
        self.check_cancel()
    }

    fn append(&mut self, bytes: &[u8]) -> bool {
        if !self.reserve(bytes.len()) {
            return false;
        }
        for part in bytes.chunks(COPY_CHUNK_BYTES) {
            if !self.check_cancel() {
                return false;
            }
            self.bytes.extend_from_slice(part);
        }
        true
    }

    fn progress(&mut self) -> bool {
        #[cfg(test)]
        {
            self.progress_calls += 1;
            if self.cancel_on_progress {
                self.cancel.store(true, Ordering::Relaxed);
            }
        }
        self.check_cancel() && self.failure.is_none()
    }
}

/// picture/custom_ptr仅在同步WebPEncode期间指向仍存活的本地context。
/// 回调不再调用编码器；thread_level=0确保没有并行访问此可变context。
unsafe extern "C" fn writer(
    data: *const u8,
    size: usize,
    picture: *const webp::WebPPicture,
) -> c_int {
    if picture.is_null() {
        return 0;
    }
    let pointer = unsafe { (*picture).custom_ptr.cast::<EncodeContext<'_>>() };
    let Some(context) = (unsafe { pointer.as_mut() }) else {
        return 0;
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if !context.check_cancel() || !context.reserve(size) {
            return false;
        }
        if size == 0 {
            return true;
        }
        if data.is_null() {
            return context.fail(CallbackFailure::InvalidInput);
        }
        // C编码器保证段内的size字节有效；reserve先限制size，排除超isize范围。
        context.append(unsafe { std::slice::from_raw_parts(data, size) })
    }));
    match result {
        Ok(ok) => c_int::from(ok),
        Err(_) => c_int::from(context.fail(CallbackFailure::Panicked)),
    }
}

unsafe extern "C" fn progress(_percent: c_int, picture: *const webp::WebPPicture) -> c_int {
    if picture.is_null() {
        return 0;
    }
    let pointer = unsafe { (*picture).user_data.cast::<EncodeContext<'_>>() };
    let Some(context) = (unsafe { pointer.as_mut() }) else {
        return 0;
    };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| context.progress()));
    match result {
        Ok(ok) => c_int::from(ok),
        Err(_) => c_int::from(context.fail(CallbackFailure::Panicked)),
    }
}

fn native_error(error: webp::WebPEncodingError) -> Box<dyn std::error::Error> {
    trf!("WebP编码失败：{error:?}", "WebP encoding failed: {error:?}").into()
}

/// external_resident_bytes仅为本图片之外的驻留量；来源按BGRA actual capacity单独计。
/// 本入口约束来源、已建模平面/工作区和输出Vec，不宣称C内部heap的完整256MiB硬界。
/// 取消为合作式：导入和两个回调之间的原生阶段可能延迟响应；失败不返回部分编码。
pub(crate) fn encode_lossy_bgra(
    image: &Image,
    quality: u8,
    external_resident_bytes: u64,
    max_encoded_bytes: usize,
    cancel: &AtomicBool,
) -> crate::Result<Vec<u8>> {
    encode_configured(
        image,
        quality,
        external_resident_bytes,
        max_encoded_bytes,
        cancel,
        |_| {},
    )
}

fn encode_configured(
    image: &Image,
    quality: u8,
    external_resident_bytes: u64,
    max_encoded_bytes: usize,
    cancel: &AtomicBool,
    configure: impl FnOnce(&mut EncodeContext<'_>),
) -> crate::Result<Vec<u8>> {
    let result = (|| {
        cancelled(cancel)?;
        WebpEncoding {
            mode: WebpMode::Lossy,
            quality,
        }
        .validate()?;
        let pixels = u64::from(image.width)
            .checked_mul(u64::from(image.height))
            .ok_or_else(budget_error)?;
        let expected = pixels.checked_mul(4).ok_or_else(budget_error)?;
        if image.width == 0
            || image.height == 0
            || image.width > webp::WEBP_MAX_DIMENSION
            || image.height > webp::WEBP_MAX_DIMENSION
            || pixels > MAX_PIXELS
            || image.bgra.len() as u64 != expected
        {
            return Err(tr!(
                "WebP图片尺寸或BGRA缓冲无效",
                "Invalid WebP image dimensions or BGRA buffer"
            )
            .into());
        }
        if max_encoded_bytes == 0 {
            return Err(CallbackFailure::OutputLimit.error());
        }
        let source_resident = sum(&[external_resident_bytes, image.bgra.capacity() as u64])?;
        fits(&[source_resident, NATIVE_FIXED_BYTES])?;
        let mut has_alpha = false;
        for part in image.bgra.chunks(COPY_CHUNK_BYTES) {
            cancelled(cancel)?;
            has_alpha |= part.chunks_exact(4).any(|pixel| pixel[3] != 255);
        }
        let model = NativeModel::new(image.width, image.height, has_alpha)?;
        let modeled_resident = sum(&[source_resident, model.peak_bytes()?])?;
        fits(&[modeled_resident])?;
        let config = config(quality)?;
        let mut picture = Picture::new()?;
        picture.0.width = image.width as c_int;
        picture.0.height = image.height as c_int;
        picture.0.use_argb = 0;
        picture.0.colorspace = webp::WebPEncCSP::WEBP_YUV420;
        cancelled(cancel)?;
        let imported = unsafe {
            webp::WebPPictureImportBGRA(
                &mut picture.0,
                image.bgra.as_ptr(),
                (image.width * 4) as c_int,
            )
        };
        cancelled(cancel)?;
        if imported == 0 {
            return Err(native_error(picture.0.error_code));
        }
        // 在已核查的直接YUVA导入合同下不存在ARGB图；升级后端改变该合同则显式拒绝。
        if picture.0.use_argb != 0
            || !picture.0.argb.is_null()
            || !picture.0.memory_argb_.is_null()
            || picture.0.y_stride != image.width as c_int
            || picture.0.uv_stride != image.width.div_ceil(2) as c_int
            || (!picture.0.a.is_null()) != has_alpha
            || (has_alpha && picture.0.a_stride != image.width as c_int)
        {
            return Err(tr!(
                "WebP原生导入布局与预算不符",
                "WebP native import layout differs from its budget"
            )
            .into());
        }
        let mut context = EncodeContext::new(modeled_resident, max_encoded_bytes, cancel);
        configure(&mut context);
        let pointer = (&mut context as *mut EncodeContext<'_>).cast();
        picture.0.writer = Some(writer);
        picture.0.custom_ptr = pointer;
        picture.0.progress_hook = Some(progress);
        picture.0.user_data = pointer;
        cancelled(cancel)?;
        let status = unsafe { webp::WebPEncode(&config, &mut picture.0) };
        let error = picture.0.error_code;
        // 先销毁原生平面，回调上下文仍存活；之后才交付Rust拥有的完整输出。
        drop(picture);
        cancelled(cancel)?;
        if let Some(failure) = context.failure {
            return Err(failure.error());
        }
        if status == 0 {
            return Err(native_error(error));
        }
        if context.bytes.is_empty() {
            return Err(CallbackFailure::InvalidInput.error());
        }
        fits(&[source_resident, context.bytes.capacity() as u64])?;
        Ok(context.bytes)
    })();
    cancelled(cancel)?;
    result
}

#[cfg(test)]
#[path = "image_webp_encode_tests.rs"]
mod tests;
