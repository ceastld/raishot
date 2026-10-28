//! 内部图片工具共用的预算、取消与创建新文件的保存合同。
use crate::{capture::Image, save_options::SaveEncoding};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub(crate) type ToolResult<T> = std::result::Result<T, ToolError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolError {
    Cancelled,
    Invalid(String),
    Failed(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str(tr!("已取消图片处理", "Image processing cancelled")),
            Self::Invalid(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ToolError {}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolLimits {
    /// 一次任务持有的所有输入图的总像素数。
    pub max_input_pixels: u64,
    /// 整个分割任务或一张拼接结果的总像素数。
    pub max_output_pixels: u64,
    pub max_side: u32,
    pub max_items: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_input_pixels: 32_000_000,
            max_output_pixels: 32_000_000,
            max_side: 65_535,
            max_items: 4096,
        }
    }
}

pub(crate) fn check_cancel(cancel: &AtomicBool) -> ToolResult<()> {
    if cancel.load(Ordering::Acquire) {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

/// 已提交的成功不受晚到取消影响；未提交的错误先采用当前取消状态。
pub(crate) fn require_committed(
    result: std::result::Result<bool, impl fmt::Display>,
    cancel: &AtomicBool,
) -> ToolResult<()> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => Err(ToolError::Cancelled),
        Err(error) => {
            check_cancel(cancel)?;
            Err(ToolError::Failed(error.to_string()))
        }
    }
}

fn pixel_bytes(width: u32, height: u32, max_side: u32, max_pixels: u64) -> ToolResult<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > max_side || height > max_side || pixels > max_pixels {
        return Err(ToolError::Invalid(
            tr!(
                "图片尺寸无效或超过处理预算",
                "Image dimensions are invalid or exceed the processing limit"
            )
            .into(),
        ));
    }
    pixels
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .ok_or_else(|| ToolError::Invalid(tr!("图片尺寸溢出", "Image dimensions overflow").into()))
}

pub(crate) fn validate_input(image: &Image, limits: ToolLimits) -> ToolResult<u64> {
    let expected = pixel_bytes(
        image.width,
        image.height,
        limits.max_side,
        limits.max_input_pixels,
    )?;
    if image.bgra.len() != expected {
        return Err(ToolError::Invalid(
            tr!(
                "图片尺寸与 BGRA 数据不一致",
                "Image dimensions do not match the BGRA data"
            )
            .into(),
        ));
    }
    Ok(expected as u64 / 4)
}

pub(crate) fn validate_output(width: u32, height: u32, limits: ToolLimits) -> ToolResult<usize> {
    pixel_bytes(width, height, limits.max_side, limits.max_output_pixels)
}

pub(crate) fn allocate_pixels(bytes: usize) -> ToolResult<Vec<u8>> {
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(bytes).map_err(|_| {
        ToolError::Failed(
            tr!(
                "没有足够内存处理图片",
                "Not enough memory to process the image"
            )
            .into(),
        )
    })?;
    pixels.resize(bytes, 0);
    Ok(pixels)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// 预览与保存共用同一个处理产物。源或参数变化时调用者必须递增 request_id。
pub(crate) struct ProcessedImage {
    image: Image,
    request_id: u64,
}

impl ProcessedImage {
    pub(crate) fn new(image: Image, request_id: u64) -> Self {
        Self { image, request_id }
    }

    pub(crate) fn image(&self) -> &Image {
        &self.image
    }

    /// 继续处理当前独占工作帧，不复制像素；调用方须保留并复用其请求代次。
    pub(crate) fn into_image(self) -> Image {
        self.image
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    /// 有损WebP支持编码中取消；旧编码器在返回后检查，原子提交成功即计为成功。
    /// 所有输出均创建新文件，包含提交竞态期间新出现的同名文件。
    pub(crate) fn save(
        &self,
        expected_request_id: u64,
        path: &Path,
        encoding: SaveEncoding,
        protected_sources: &[PathBuf],
        cancel: &AtomicBool,
    ) -> ToolResult<SavedImage> {
        require_request(self.request_id, expected_request_id)?;
        validate_save_destination(path, encoding, protected_sources, cancel)?;
        let bytes = crate::export::encode_image_cancellable(&self.image, encoding, cancel);
        check_cancel(cancel)?;
        let bytes = bytes.map_err(|error| ToolError::Failed(error.to_string()))?;
        crate::image_collection::ensure_local_directory(path.parent().expect("validated parent"))
            .map_err(ToolError::Invalid)?;
        require_committed(
            crate::export::save_encoded_cancellable(&bytes, path, false, cancel),
            cancel,
        )?;
        Ok(SavedImage {
            path: path.to_path_buf(),
            width: self.image.width,
            height: self.image.height,
        })
    }
}

/// 在编码或复用既有编码产物前验证目的地，保护路径只读本地元数据。
pub(crate) fn validate_save_destination(
    path: &Path,
    encoding: SaveEncoding,
    protected_sources: &[PathBuf],
    cancel: &AtomicBool,
) -> ToolResult<()> {
    check_cancel(cancel)?;
    encoding
        .validate()
        .map_err(|error| ToolError::Invalid(error.to_string()))?;
    if crate::save_options::SaveFormat::from_path(path) != Some(encoding.format) {
        return Err(ToolError::Invalid(
            tr!(
                "输出扩展名与保存格式不一致",
                "The output extension does not match the selected format"
            )
            .into(),
        ));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ToolError::Invalid(tr!("输出文件名无效", "Invalid output file name").into())
        })?;
    crate::save_options::file_name(name, encoding.format)
        .map_err(|error| ToolError::Invalid(error.to_string()))?;
    let directory = path
        .parent()
        .ok_or_else(|| ToolError::Invalid(tr!("输出目录无效", "Invalid output folder").into()))?;
    crate::save_options::validate_directory(directory)
        .map_err(|error| ToolError::Invalid(error.to_string()))?;
    crate::image_collection::ensure_local_directory(directory).map_err(ToolError::Invalid)?;
    for source in protected_sources {
        check_cancel(cancel)?;
        crate::image_collection::ensure_local_file_or_missing(source)
            .map_err(ToolError::Invalid)?;
    }
    if protected_sources
        .iter()
        .any(|source| crate::export::is_same_path(source, path))
    {
        return Err(ToolError::Invalid(
            tr!(
                "请另存为新文件，保留原图",
                "Save to a new file to preserve the source"
            )
            .into(),
        ));
    }
    check_cancel(cancel)
}

pub(crate) fn require_request(actual: u64, expected: u64) -> ToolResult<()> {
    if actual != expected {
        return Err(ToolError::Invalid(
            tr!(
                "源图或参数已变化，请等待当前预览后再保存",
                "The source or options changed; wait for the current preview before saving"
            )
            .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tools_real_commit_result_keeps_success_and_prioritizes_cancel_over_io_error() {
        let directory = std::env::temp_dir().join(format!(
            "rustshot-commit-result-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("result.png");
        let cancel = AtomicBool::new(false);
        let committed =
            crate::export::save_encoded_cancellable(b"committed", &path, false, &cancel);
        assert_eq!(committed.as_ref().ok().copied(), Some(true));
        cancel.store(true, Ordering::Release);
        assert_eq!(require_committed(committed, &cancel), Ok(()));
        assert_eq!(std::fs::read(&path).unwrap(), b"committed");

        cancel.store(false, Ordering::Release);
        let blocked =
            crate::export::save_encoded_cancellable(b"replacement", &path, false, &cancel);
        assert!(blocked.is_err());
        cancel.store(true, Ordering::Release);
        assert_eq!(
            require_committed(blocked, &cancel),
            Err(ToolError::Cancelled)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"committed");

        cancel.store(false, Ordering::Release);
        let failed = crate::export::save_encoded_cancellable(b"replacement", &path, false, &cancel);
        assert!(failed.is_err());
        assert!(matches!(
            require_committed(failed, &cancel),
            Err(ToolError::Failed(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"committed");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn image_tools_reject_malformed_pixels_and_budget_before_allocation() {
        let mut image = Image {
            width: 2,
            height: 2,
            bgra: vec![0; 15],
            readback_ms: 0.0,
        };
        assert!(validate_input(&image, ToolLimits::default()).is_err());
        image.bgra.push(0);
        assert_eq!(validate_input(&image, ToolLimits::default()).unwrap(), 4);
        let limits = ToolLimits {
            max_output_pixels: 3,
            ..ToolLimits::default()
        };
        assert!(validate_output(2, 2, limits).is_err());
        assert!(validate_output(
            u32::MAX,
            u32::MAX,
            ToolLimits {
                max_side: u32::MAX,
                max_output_pixels: u64::MAX,
                ..limits
            }
        )
        .is_err());
    }

    #[test]
    fn image_tools_cancel_and_stale_result_cannot_save() {
        let result = ProcessedImage::new(
            Image {
                width: 1,
                height: 1,
                bgra: vec![1, 2, 3, 255],
                readback_ms: 0.0,
            },
            7,
        );
        let path = Path::new("unused.png");
        assert!(matches!(
            result.save(
                8,
                path,
                SaveEncoding::default(),
                &[],
                &AtomicBool::new(false)
            ),
            Err(ToolError::Invalid(_))
        ));
        assert_eq!(
            result.save(
                7,
                path,
                SaveEncoding::default(),
                &[],
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        );
    }
}
