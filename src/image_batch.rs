//! 内部顺序批处理；只访问明确传入的路径，不扫描目录或建立全局队列。
#[path = "image_tool_watermark.rs"]
pub(crate) mod watermark;

use self::watermark::WatermarkOptions;
use crate::{
    image_tools::{
        check_cancel, require_request, ProcessedImage, SavedImage, ToolError, ToolLimits,
        ToolResult,
    },
    image_transform::{
        color::ColorAdjustments, sharpen::SharpenOptions, TransformOptions, TransformPlan,
    },
    save_options::{SaveConfig, SaveEncoding, SaveMoment},
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BatchNaming {
    /// 源文件名（不含扩展名）加后缀。最终始终追加原始输入序号，重名源不会互相覆盖。
    SourceStem { suffix: String },
    /// 沿用现有保存模板；支持的占位符由 SaveConfig 决定，不新增解析器。
    Template {
        template: String,
        date_format: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchOptions {
    /// 调用者明确选择的已存在目录，不读取或更新全局保存设置。
    pub directory: PathBuf,
    pub encoding: SaveEncoding,
    pub transform: TransformOptions,
    pub naming: BatchNaming,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BatchLimits {
    pub image: ToolLimits,
    /// 累计已解码像素；失败项成功解码过的像素也计入。
    pub max_decoded_pixels: u64,
    /// 累计已开始生成的结果像素；处理或保存失败时不返还预算。
    pub max_processed_pixels: u64,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            image: ToolLimits::default(),
            max_decoded_pixels: 256_000_000,
            max_processed_pixels: 256_000_000,
        }
    }
}

struct BatchInput {
    path: PathBuf,
    unavailable: Option<String>,
}

pub(crate) struct BatchPlan {
    inputs: Vec<BatchInput>,
    options: BatchOptions,
    template: Option<SaveConfig>,
    moment: SaveMoment,
    limits: BatchLimits,
    request_id: u64,
}

impl BatchPlan {
    /// 参数和路径列表冻结在计划里；来源或参数变化时调用者递增代次并取消旧任务。
    /// 这里只检查路径元数据，不读取源文件内容；坏路径仍保留为一个失败项。
    pub(crate) fn new(
        paths: &[PathBuf],
        options: BatchOptions,
        limits: BatchLimits,
        request_id: u64,
    ) -> ToolResult<Self> {
        if paths.is_empty() || paths.len() > limits.image.max_items {
            return Err(ToolError::Invalid(
                tr!(
                    "批处理图片数量为空或超过上限",
                    "Invalid number of batch images"
                )
                .into(),
            ));
        }
        if limits.max_decoded_pixels == 0
            || limits.max_processed_pixels == 0
            || limits.image.max_input_pixels == 0
            || limits.image.max_output_pixels == 0
            || limits.image.max_side == 0
        {
            return Err(ToolError::Invalid(
                tr!(
                    "批处理像素预算必须大于零",
                    "Batch pixel limits must be positive"
                )
                .into(),
            ));
        }
        crate::save_options::validate_directory(&options.directory)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        options
            .encoding
            .validate()
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        ensure_local_output_directory(&options.directory)?;
        let template = match &options.naming {
            BatchNaming::SourceStem { suffix } => {
                crate::save_options::file_name(
                    &format!("image{suffix}_0001"),
                    options.encoding.format,
                )
                .map_err(|error| ToolError::Invalid(error.to_string()))?;
                None
            }
            BatchNaming::Template {
                template,
                date_format,
            } => {
                let config = SaveConfig {
                    directory: options.directory.clone(),
                    encoding: options.encoding,
                    filename_template: template.clone(),
                    date_format: date_format.clone(),
                };
                config
                    .validate()
                    .map_err(|error| ToolError::Invalid(error.to_string()))?;
                Some(config)
            }
        };
        let mut inputs = Vec::new();
        inputs.try_reserve_exact(paths.len()).map_err(|_| {
            ToolError::Failed(
                tr!(
                    "没有足够内存建立批处理计划",
                    "Not enough memory for the batch plan"
                )
                .into(),
            )
        })?;
        for path in paths {
            let (path, unavailable) = match std::path::absolute(path) {
                Ok(path) => {
                    let unavailable = crate::image_collection::ensure_local_file(&path).err();
                    (path, unavailable)
                }
                Err(error) => (path.clone(), Some(error.to_string())),
            };
            inputs.push(BatchInput { path, unavailable });
        }
        Ok(Self {
            inputs,
            options,
            template,
            moment: SaveMoment::now(),
            limits,
            request_id,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.inputs.len()
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    /// 只规划某项输出名；尺寸指调整和缩放后的物理像素，预览与保存共用。
    pub(crate) fn output_path(&self, index: usize, width: u32, height: u32) -> ToolResult<PathBuf> {
        let input = self.inputs.get(index).ok_or_else(|| {
            ToolError::Invalid(tr!("批处理索引无效", "Invalid batch item index").into())
        })?;
        let stem = match &self.options.naming {
            BatchNaming::SourceStem { suffix } => {
                let source = input
                    .path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .ok_or_else(|| {
                        ToolError::Invalid(
                            tr!(
                                "源文件名不是有效的 Unicode 名称",
                                "Source file name is not valid Unicode"
                            )
                            .into(),
                        )
                    })?;
                format!("{source}{suffix}")
            }
            BatchNaming::Template { .. } => self
                .template
                .as_ref()
                .unwrap()
                .suggested_stem_at(width, height, self.moment)
                .map_err(|error| ToolError::Invalid(error.to_string()))?,
        };
        let numbered = format!("{stem}_{:04}", index + 1);
        let name = crate::save_options::file_name(&numbered, self.options.encoding.format)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        Ok(self.options.directory.join(name))
    }

    /// 顺序执行，最多持有当前源、调整中间图和当前结果，不留存其它项的像素。
    /// 回调每项完成后恰好一次；调用者可在回调中取消，后续项不再读取或处理。
    /// 读取、元数据扫描与ICC变换响应取消；库内整帧解码、既有调整和编码仅在边界检查。
    /// 已提交项始终报告为成功。
    pub(crate) fn run(
        &self,
        expected_request_id: u64,
        cancel: &AtomicBool,
        progress: impl FnMut(&BatchItemResult),
    ) -> ToolResult<BatchReport> {
        self.run_with_color(
            expected_request_id,
            ColorAdjustments::default(),
            cancel,
            progress,
        )
    }

    /// 颜色参数在访问单项前统一验证；每项直接调整现有处理帧，不另分配整图。
    pub(crate) fn run_with_color(
        &self,
        expected_request_id: u64,
        colors: ColorAdjustments,
        cancel: &AtomicBool,
        progress: impl FnMut(&BatchItemResult),
    ) -> ToolResult<BatchReport> {
        self.run_with_effects(expected_request_id, colors, None, cancel, progress)
    }

    /// 保留未启用锐化的调用方式；未启用水印时不访问字体资源。
    pub(crate) fn run_with_effects(
        &self,
        expected_request_id: u64,
        colors: ColorAdjustments,
        watermark: Option<WatermarkOptions>,
        cancel: &AtomicBool,
        progress: impl FnMut(&BatchItemResult),
    ) -> ToolResult<BatchReport> {
        self.run_with_sharpening(
            expected_request_id,
            colors,
            SharpenOptions::default(),
            watermark,
            cancel,
            progress,
        )
    }

    /// 整批效果先校验，再逐项按最终尺寸的颜色、锐化、水印顺序处理。
    pub(crate) fn run_with_sharpening(
        &self,
        expected_request_id: u64,
        colors: ColorAdjustments,
        sharpening: SharpenOptions,
        watermark: Option<WatermarkOptions>,
        cancel: &AtomicBool,
        progress: impl FnMut(&BatchItemResult),
    ) -> ToolResult<BatchReport> {
        self.run_indices_with_sharpening(
            expected_request_id,
            0..self.inputs.len(),
            false,
            colors,
            sharpening,
            watermark,
            cancel,
            progress,
        )
    }

    /// 仅重试显式选中的原始索引；命名时间、序号和原片保护仍来自整个冻结计划。
    /// 每次尝试独立累计预算，允许重新读取已恢复的来源，不自动重试失败项。
    pub(crate) fn run_selected_with_sharpening(
        &self,
        expected_request_id: u64,
        indices: &[usize],
        colors: ColorAdjustments,
        sharpening: SharpenOptions,
        watermark: Option<WatermarkOptions>,
        cancel: &AtomicBool,
        progress: impl FnMut(&BatchItemResult),
    ) -> ToolResult<BatchReport> {
        if indices.iter().any(|index| *index >= self.inputs.len())
            || indices.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(ToolError::Invalid(
                tr!(
                    "重试索引必须按原顺序排列且不重复",
                    "Retry indices must be ordered, unique, and in range"
                )
                .into(),
            ));
        }
        self.run_indices_with_sharpening(
            expected_request_id,
            indices.iter().copied(),
            true,
            colors,
            sharpening,
            watermark,
            cancel,
            progress,
        )
    }

    fn run_indices_with_sharpening(
        &self,
        expected_request_id: u64,
        indices: impl ExactSizeIterator<Item = usize>,
        retry_unavailable: bool,
        colors: ColorAdjustments,
        sharpening: SharpenOptions,
        watermark: Option<WatermarkOptions>,
        cancel: &AtomicBool,
        mut progress: impl FnMut(&BatchItemResult),
    ) -> ToolResult<BatchReport> {
        require_request(self.request_id, expected_request_id)?;
        colors.validate()?;
        sharpening.validate()?;
        if let Some(options) = &watermark {
            options.validate()?;
        }
        let mut items = Vec::new();
        items.try_reserve_exact(indices.len()).map_err(|_| {
            ToolError::Failed(
                tr!(
                    "没有足够内存保存批处理结果",
                    "Not enough memory for batch results"
                )
                .into(),
            )
        })?;
        let mut usage = BatchUsage::default();
        let mut cancelled = false;
        for index in indices {
            let input = &self.inputs[index];
            let mut output = None;
            let result = if cancelled {
                Err(ToolError::Cancelled)
            } else {
                self.process_item(
                    index,
                    retry_unavailable,
                    colors,
                    sharpening,
                    watermark.as_ref(),
                    &mut usage,
                    &mut output,
                    cancel,
                )
            };
            let outcome = match result {
                Ok(saved) => BatchOutcome::Saved(saved),
                Err(ToolError::Cancelled) => {
                    cancelled = true;
                    BatchOutcome::Cancelled
                }
                Err(error) => BatchOutcome::Failed(error.to_string()),
            };
            let item = BatchItemResult {
                index,
                source: input.path.clone(),
                output,
                outcome,
            };
            progress(&item);
            items.push(item);
        }
        Ok(BatchReport {
            request_id: self.request_id,
            items,
            decoded_pixels: usage.decoded_pixels,
            processed_pixels: usage.processed_pixels,
        })
    }

    fn process_item(
        &self,
        index: usize,
        retry_unavailable: bool,
        colors: ColorAdjustments,
        sharpening: SharpenOptions,
        watermark: Option<&WatermarkOptions>,
        usage: &mut BatchUsage,
        output: &mut Option<PathBuf>,
        cancel: &AtomicBool,
    ) -> ToolResult<SavedImage> {
        check_cancel(cancel)?;
        let input = &self.inputs[index];
        if let Some(error) = &input.unavailable {
            if !retry_unavailable {
                return Err(ToolError::Invalid(error.clone()));
            }
            let availability = crate::image_collection::ensure_local_file(&input.path);
            check_cancel(cancel)?;
            availability.map_err(ToolError::Invalid)?;
        }
        let remaining_input = self.limits.max_decoded_pixels - usage.decoded_pixels;
        let remaining_output = self.limits.max_processed_pixels - usage.processed_pixels;
        if remaining_input == 0 || remaining_output == 0 {
            return Err(batch_budget_error());
        }
        let allowed_input_pixels = remaining_input.min(self.limits.image.max_input_pixels);
        let decode_bytes = allowed_input_pixels.saturating_mul(4);
        // 解码器在分配输出前使用剩余批预算；每次打开前仍重新检查本地可用性。
        let decoded = crate::image_decode::decode_path_with_budget_cancellable(
            &input.path,
            decode_bytes,
            cancel,
        );
        check_cancel(cancel)?;
        let decoded = decoded.map_err(|error| ToolError::Failed(error.to_string()))?;
        let source = decoded.image;
        let source_pixels = u64::from(source.width) * u64::from(source.height);
        usage.decoded_pixels = usage
            .decoded_pixels
            .checked_add(source_pixels)
            .filter(|pixels| *pixels <= self.limits.max_decoded_pixels)
            .ok_or_else(batch_budget_error)?;
        check_cancel(cancel)?;
        let plan = TransformPlan::new(
            &source,
            self.options.transform,
            self.limits.image,
            self.request_id,
        )?;
        let (width, height) = plan.size();
        let output_pixels = u64::from(width) * u64::from(height);
        if output_pixels > remaining_output {
            return Err(batch_budget_error());
        }
        let path = self.output_path(index, width, height)?;
        *output = Some(path.clone());
        // 不为保护其它源而打开/规范化它们；它们可能是等待处理的云端占位文件。
        // 词法检查同时保护尚不存在的显式输入；现存路径别名仍由 create-only 提交保护。
        let absolute_output =
            std::path::absolute(&path).map_err(|error| ToolError::Invalid(error.to_string()))?;
        if self
            .inputs
            .iter()
            .any(|source| same_absolute_path(&source.path, &absolute_output))
        {
            return Err(ToolError::Invalid(
                tr!(
                    "输出名称与批处理源路径冲突，请更换名称或目录",
                    "Output name conflicts with a batch source; choose another name or folder"
                )
                .into(),
            ));
        }
        usage.processed_pixels += output_pixels;
        let result = plan.render_with_effects(&source, colors, sharpening, cancel)?;
        let result = if let Some(options) = watermark {
            let request_id = result.request_id();
            let mut image = result.into_image();
            watermark::apply(&mut image, options, self.limits.image, cancel)?;
            ProcessedImage::new(image, request_id)
        } else {
            result
        };
        ensure_local_output_directory(&self.options.directory)?;
        // 只有当前源经过本地可用性检查和读取；其它原片靠不覆盖提交保持安全。
        result.save(
            self.request_id,
            &path,
            self.options.encoding,
            std::slice::from_ref(&input.path),
            cancel,
        )
    }
}

fn batch_budget_error() -> ToolError {
    ToolError::Invalid(
        tr!(
            "本批累计图片像素超过处理预算",
            "Batch images exceed the total pixel limit"
        )
        .into(),
    )
}

/// 保存事务会规范化父目录；先逐级读元数据，避免打开链接或可能触发回读的目录。
fn ensure_local_output_directory(path: &Path) -> ToolResult<()> {
    use std::os::windows::fs::MetadataExt;
    const UNAVAILABLE: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;
    let absolute =
        std::path::absolute(path).map_err(|error| ToolError::Invalid(error.to_string()))?;
    let ancestors: Vec<_> = absolute.ancestors().collect();
    for ancestor in ancestors.into_iter().rev() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .map_err(|error| ToolError::Invalid(error.to_string()))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.file_attributes() & UNAVAILABLE != 0
        {
            return Err(ToolError::Invalid(
                tr!("输出目录必须已存在且在本地可用，不能是链接或云端占位目录", "The output folder must exist locally and cannot be a link or cloud placeholder").into(),
            ));
        }
    }
    Ok(())
}

/// 比较已经转为绝对路径的文本，统一常规与长路径前缀，不打开文件或解析链接。
fn same_absolute_path(left: &Path, right: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let left: Vec<_> = comparable_path(left).as_os_str().encode_wide().collect();
    let right: Vec<_> = comparable_path(right).as_os_str().encode_wide().collect();
    unsafe {
        windows::Win32::Globalization::CompareStringOrdinal(&left, &right, true)
            == windows::Win32::Globalization::CSTR_EQUAL
    }
}

fn comparable_path(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::VerbatimDisk(letter) => normalized.push(format!("{}:", char::from(letter))),
                Prefix::VerbatimUNC(server, share) => {
                    let mut unc = std::ffi::OsString::from(r"\\");
                    unc.push(server);
                    unc.push(r"\");
                    unc.push(share);
                    normalized.push(unc);
                }
                _ => normalized.push(prefix.as_os_str()),
            },
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Default)]
struct BatchUsage {
    decoded_pixels: u64,
    processed_pixels: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BatchOutcome {
    Saved(SavedImage),
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchItemResult {
    pub index: usize,
    pub source: PathBuf,
    /// 解码/参数验证前失败或尚未处理时没有最终文件名。
    pub output: Option<PathBuf>,
    pub outcome: BatchOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchReport {
    pub request_id: u64,
    pub items: Vec<BatchItemResult>,
    pub decoded_pixels: u64,
    pub processed_pixels: u64,
}

pub(crate) fn run(
    paths: &[PathBuf],
    options: BatchOptions,
    limits: BatchLimits,
    request_id: u64,
    cancel: &AtomicBool,
    progress: impl FnMut(&BatchItemResult),
) -> ToolResult<BatchReport> {
    BatchPlan::new(paths, options, limits, request_id)?.run(request_id, cancel, progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture::Image, image_resize::ResizeFilter, image_transform::ResizeOptions};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "rustshot-image-batch-{}-{stamp}",
                std::process::id()
            ));
            std::fs::create_dir(&root).unwrap();
            for name in ["in", "other", "out"] {
                std::fs::create_dir(root.join(name)).unwrap();
            }
            Self(root)
        }

        fn output(&self) -> PathBuf {
            self.0.join("out")
        }

        fn input(&self, name: &str, width: u32) -> PathBuf {
            let path = self.0.join(name);
            let image = Image {
                width,
                height: 1,
                bgra: (0..width)
                    .flat_map(|index| [index as u8 + 10, 20, 30, 128])
                    .collect(),
                readback_ms: 0.,
            };
            crate::export::save_png(image, &path).unwrap();
            path
        }

        fn options(&self) -> BatchOptions {
            BatchOptions {
                directory: self.output(),
                encoding: SaveEncoding::default(),
                transform: TransformOptions::default(),
                naming: BatchNaming::SourceStem {
                    suffix: "_processed".into(),
                },
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            for name in ["in", "other", "out"] {
                let directory = self.0.join(name);
                if let Ok(entries) = std::fs::read_dir(&directory) {
                    for entry in entries.flatten() {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
                let _ = std::fs::remove_dir(directory);
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn image_batch_bad_image_does_not_stop_ordered_conversion_and_source_stays_unchanged() {
        let directory = TestDirectory::new();
        let first = directory.input("in/first.png", 2);
        let before = std::fs::read(&first).unwrap();
        let bad = directory.0.join("in/bad.png");
        std::fs::write(&bad, b"not an image").unwrap();
        let third = directory.input("in/third.png", 3);
        let mut options = directory.options();
        options.transform.resize = Some(ResizeOptions {
            width: 2,
            height: 2,
            filter: ResizeFilter::Bilinear,
        });
        let mut events = Vec::new();
        let report = run(
            &[first.clone(), bad, third],
            options,
            BatchLimits::default(),
            30,
            &AtomicBool::new(false),
            |item| events.push(item.index),
        )
        .unwrap();
        assert_eq!(events, [0, 1, 2]);
        assert_eq!(report.request_id, 30);
        assert_eq!((report.decoded_pixels, report.processed_pixels), (5, 8));
        assert!(matches!(report.items[0].outcome, BatchOutcome::Saved(_)));
        assert!(matches!(report.items[1].outcome, BatchOutcome::Failed(_)));
        assert!(matches!(report.items[2].outcome, BatchOutcome::Saved(_)));
        assert_eq!(
            report.items[0]
                .output
                .as_ref()
                .unwrap()
                .file_name()
                .unwrap(),
            "first_processed_0001.png"
        );
        assert_eq!(
            report.items[2]
                .output
                .as_ref()
                .unwrap()
                .file_name()
                .unwrap(),
            "third_processed_0003.png"
        );
        let result = crate::export::read_image(report.items[2].output.as_ref().unwrap()).unwrap();
        assert_eq!((result.width, result.height), (2, 2));
        assert!(result.bgra.chunks_exact(4).all(|pixel| pixel[3] == 128));
        assert_eq!(std::fs::read(first).unwrap(), before);
    }

    #[test]
    fn image_batch_watermark_invalid_global_options_and_prior_cancel_write_nothing() {
        let directory = TestDirectory::new();
        let path = directory.input("in/watermark.png", 2);
        let original = std::fs::read(&path).unwrap();
        let plan = BatchPlan::new(
            std::slice::from_ref(&path),
            directory.options(),
            BatchLimits::default(),
            32,
        )
        .unwrap();
        let mut progress_calls = 0;
        assert!(plan
            .run_with_effects(
                32,
                ColorAdjustments::default(),
                Some(WatermarkOptions {
                    text: "字".repeat(4097),
                    ..WatermarkOptions::default()
                }),
                &AtomicBool::new(false),
                |_| progress_calls += 1
            )
            .is_err());
        assert_eq!(progress_calls, 0);
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
        let report = plan
            .run_with_effects(
                32,
                ColorAdjustments::default(),
                Some(WatermarkOptions {
                    text: "Mark".into(),
                    ..WatermarkOptions::default()
                }),
                &AtomicBool::new(true),
                |_| {},
            )
            .unwrap();
        assert!(matches!(report.items[0].outcome, BatchOutcome::Cancelled));
        assert!(report.items[0].output.is_none());
        assert_eq!((report.decoded_pixels, report.processed_pixels), (0, 0));
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    #[ignore = "需要 Windows GDI 字体，集成时显式验证"]
    fn image_batch_watermark_roundtrip_follows_color_and_uses_output_budget_without_changing_source(
    ) {
        let directory = TestDirectory::new();
        let path = directory.0.join("in/watermark-color.png");
        crate::export::save_png(
            Image {
                width: 128,
                height: 80,
                bgra: [13, 67, 193, 96].repeat(128 * 80),
                readback_ms: 0.,
            },
            &path,
        )
        .unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut options = directory.options();
        options.transform.resize = Some(ResizeOptions {
            width: 320,
            height: 192,
            filter: ResizeFilter::Bilinear,
        });
        let limits = BatchLimits {
            image: ToolLimits {
                max_input_pixels: 128 * 80,
                max_output_pixels: 320 * 192,
                ..ToolLimits::default()
            },
            ..BatchLimits::default()
        };
        let plan = BatchPlan::new(std::slice::from_ref(&path), options, limits, 33).unwrap();
        let report = plan
            .run_with_effects(
                33,
                ColorAdjustments {
                    contrast: 10,
                    saturation: -100,
                },
                Some(WatermarkOptions {
                    text: "Mark".into(),
                    font_size: 20.,
                    color: 0xc7357c,
                    opacity: 0.5,
                }),
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let BatchOutcome::Saved(saved) = &report.items[0].outcome else {
            panic!("{:?}", report.items[0]);
        };
        let output = crate::export::read_image(&saved.path).unwrap();
        assert_eq!((output.width, output.height), (320, 192));
        assert_eq!(
            (report.decoded_pixels, report.processed_pixels),
            (128 * 80, 320 * 192)
        );
        assert!(output.bgra.chunks_exact(4).all(|pixel| pixel[3] >= 96));
        assert!(output
            .bgra
            .chunks_exact(4)
            .any(|pixel| pixel[3] == 96 && pixel[0] == pixel[1] && pixel[1] == pixel[2]));
        assert!(output
            .bgra
            .chunks_exact(4)
            .any(|pixel| pixel[3] > 96 && (pixel[0] != pixel[1] || pixel[1] != pixel[2])));
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn image_batch_color_roundtrip_desaturates_all_outputs_and_keeps_alpha_and_sources() {
        let directory = TestDirectory::new();
        let paths = [
            directory.input("in/color-first.png", 2),
            directory.input("in/color-second.png", 3),
        ];
        let originals: Vec<_> = paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let plan = BatchPlan::new(&paths, directory.options(), BatchLimits::default(), 31).unwrap();
        let cancel = AtomicBool::new(false);
        let mut progress_calls = 0;
        assert!(plan
            .run_with_color(
                31,
                ColorAdjustments {
                    contrast: 101,
                    saturation: -100
                },
                &cancel,
                |_| progress_calls += 1
            )
            .is_err());
        assert_eq!(progress_calls, 0);
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);

        let report = plan
            .run_with_color(
                31,
                ColorAdjustments {
                    contrast: 0,
                    saturation: -100,
                },
                &cancel,
                |_| {},
            )
            .unwrap();
        assert_eq!(report.items.len(), 2);
        assert_eq!((report.decoded_pixels, report.processed_pixels), (5, 5));
        for (index, item) in report.items.iter().enumerate() {
            let BatchOutcome::Saved(saved) = &item.outcome else {
                panic!("{item:?}");
            };
            let result = crate::export::read_image(&saved.path).unwrap();
            assert_eq!((result.width, result.height), (index as u32 + 2, 1));
            assert!(result
                .bgra
                .chunks_exact(4)
                .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2] && pixel[3] == 128));
        }
        for (path, original) in paths.iter().zip(&originals) {
            assert_eq!(&std::fs::read(path).unwrap(), original);
        }
    }

    #[test]
    fn image_batch_sharpening_default_and_resized_png_keep_alpha_and_source() {
        let directory = TestDirectory::new();
        let path = directory.0.join("in/sharpen.png");
        let source = Image {
            width: 8,
            height: 3,
            bgra: (0..3)
                .flat_map(|y| {
                    (0..8).flat_map(move |x| {
                        if x == 0 {
                            [7, 83, 191, 0]
                        } else {
                            let gray = if x < 4 { 80 } else { 160 };
                            [gray, gray, gray, [64, 128, 192][(x + y) % 3]]
                        }
                    })
                })
                .collect(),
            readback_ms: 0.,
        };
        crate::export::save_png(source, &path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut options = directory.options();
        options.transform.resize = Some(ResizeOptions {
            width: 16,
            height: 6,
            filter: ResizeFilter::Nearest,
        });
        let limits = BatchLimits {
            image: ToolLimits {
                max_input_pixels: 24,
                max_output_pixels: 96,
                ..ToolLimits::default()
            },
            ..BatchLimits::default()
        };
        let plan = |suffix: &str| {
            let mut options = options.clone();
            options.naming = BatchNaming::SourceStem {
                suffix: suffix.into(),
            };
            BatchPlan::new(std::slice::from_ref(&path), options, limits, 41).unwrap()
        };
        let read_saved = |report: &BatchReport| {
            assert_eq!((report.decoded_pixels, report.processed_pixels), (24, 96));
            let BatchOutcome::Saved(saved) = &report.items[0].outcome else {
                panic!("{:?}", report.items[0]);
            };
            crate::export::read_image(&saved.path).unwrap()
        };
        let cancel = AtomicBool::new(false);
        let legacy = read_saved(
            &plan("_legacy")
                .run_with_effects(41, ColorAdjustments::default(), None, &cancel, |_| {})
                .unwrap(),
        );
        let disabled = read_saved(
            &plan("_disabled")
                .run_with_sharpening(
                    41,
                    ColorAdjustments::default(),
                    SharpenOptions::default(),
                    None,
                    &cancel,
                    |_| {},
                )
                .unwrap(),
        );
        assert_eq!(legacy.bgra, disabled.bgra);
        let sharpened = read_saved(
            &plan("_sharp")
                .run_with_sharpening(
                    41,
                    ColorAdjustments::default(),
                    SharpenOptions {
                        amount_percent: 100,
                        threshold: 0,
                    },
                    None,
                    &cancel,
                    |_| {},
                )
                .unwrap(),
        );
        assert_eq!((sharpened.width, sharpened.height), (16, 6));
        assert_ne!(sharpened.bgra, disabled.bgra);
        assert_eq!(&sharpened.bgra[..4], &[7, 83, 191, 0]);
        for (before, after) in disabled
            .bgra
            .chunks_exact(4)
            .zip(sharpened.bgra.chunks_exact(4))
        {
            assert_eq!(before[3], after[3]);
            if before[3] == 0 {
                assert_eq!(after, &[7, 83, 191, 0]);
            }
        }
        let dark_edge = (2 * 16 + 7) * 4;
        let light_edge = (2 * 16 + 8) * 4;
        for channel in 0..3 {
            assert!(sharpened.bgra[dark_edge + channel] < disabled.bgra[dark_edge + channel]);
            assert!(sharpened.bgra[light_edge + channel] > disabled.bgra[light_edge + channel]);
        }
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn image_batch_sharpening_global_validation_and_cancel_keep_first_saved_png() {
        let directory = TestDirectory::new();
        let paths = [
            directory.input("in/sharp-first.png", 2),
            directory.input("in/sharp-second.png", 3),
        ];
        let originals: Vec<_> = paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let plan = BatchPlan::new(&paths, directory.options(), BatchLimits::default(), 42).unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = 0;
        assert!(plan
            .run_with_sharpening(
                42,
                ColorAdjustments::default(),
                SharpenOptions {
                    amount_percent: 201,
                    threshold: 0,
                },
                None,
                &cancel,
                |_| calls += 1,
            )
            .is_err());
        assert_eq!(calls, 0);
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
        let report = plan
            .run_with_sharpening(
                42,
                ColorAdjustments::default(),
                SharpenOptions {
                    amount_percent: 100,
                    threshold: 0,
                },
                None,
                &cancel,
                |item| {
                    calls += 1;
                    if matches!(item.outcome, BatchOutcome::Saved(_)) {
                        cancel.store(true, std::sync::atomic::Ordering::Release);
                    }
                },
            )
            .unwrap();
        assert_eq!(calls, 2);
        let BatchOutcome::Saved(saved) = &report.items[0].outcome else {
            panic!("{:?}", report.items[0]);
        };
        let output = crate::export::read_image(&saved.path).unwrap();
        assert_eq!((output.width, output.height), (2, 1));
        assert!(output.bgra.chunks_exact(4).all(|pixel| pixel[3] == 128));
        assert_eq!(report.items[1].outcome, BatchOutcome::Cancelled);
        assert!(report.items[1].output.is_none());
        assert_eq!((report.decoded_pixels, report.processed_pixels), (2, 2));
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 1);
        for (path, original) in paths.iter().zip(&originals) {
            assert_eq!(&std::fs::read(path).unwrap(), original);
        }
    }

    #[test]
    fn image_batch_existing_output_fails_one_item_and_never_overwrites() {
        let directory = TestDirectory::new();
        let first = directory.input("in/same.png", 1);
        let second = directory.input("other/same.png", 1);
        let plan = BatchPlan::new(
            &[first, second],
            directory.options(),
            BatchLimits::default(),
            31,
        )
        .unwrap();
        let existing = plan.output_path(0, 1, 1).unwrap();
        std::fs::write(&existing, b"keep output").unwrap();
        let report = plan.run(31, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(matches!(report.items[0].outcome, BatchOutcome::Failed(_)));
        assert!(matches!(report.items[1].outcome, BatchOutcome::Saved(_)));
        assert_eq!(report.processed_pixels, 2);
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep output");
        let again = plan.run(31, &AtomicBool::new(false), |_| {}).unwrap();
        assert!(again
            .items
            .iter()
            .all(|item| matches!(item.outcome, BatchOutcome::Failed(_))));
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 2);
    }

    #[test]
    fn image_batch_stale_request_writes_nothing_and_cancel_keeps_first_success() {
        let directory = TestDirectory::new();
        let inputs = [
            directory.input("in/first.png", 1),
            directory.input("in/second.png", 1),
            directory.input("in/third.png", 1),
        ];
        let plan =
            BatchPlan::new(&inputs, directory.options(), BatchLimits::default(), 32).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.request_id(), 32);
        let cancel = AtomicBool::new(false);
        assert!(plan
            .run(33, &cancel, |_| panic!(
                "stale request must not report progress"
            ))
            .is_err());
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
        let report = plan
            .run(32, &cancel, |item| {
                if matches!(item.outcome, BatchOutcome::Saved(_)) {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
            })
            .unwrap();
        assert!(matches!(report.items[0].outcome, BatchOutcome::Saved(_)));
        assert_eq!(report.items[1].outcome, BatchOutcome::Cancelled);
        assert_eq!(report.items[2].outcome, BatchOutcome::Cancelled);
        assert!(report.items[1].output.is_none());
        assert_eq!((report.decoded_pixels, report.processed_pixels), (1, 1));
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 1);
    }

    #[test]
    fn image_batch_remaining_decode_budget_rejects_large_item_and_allows_later_small_item() {
        let directory = TestDirectory::new();
        let inputs = [
            directory.input("in/first.png", 1),
            directory.input("in/large.png", 2),
            directory.input("in/last.png", 1),
        ];
        let limits = BatchLimits {
            max_decoded_pixels: 2,
            ..BatchLimits::default()
        };
        let report = run(
            &inputs,
            directory.options(),
            limits,
            0,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!((report.decoded_pixels, report.processed_pixels), (2, 2));
        assert!(matches!(report.items[0].outcome, BatchOutcome::Saved(_)));
        assert!(matches!(report.items[1].outcome, BatchOutcome::Failed(_)));
        assert!(matches!(report.items[2].outcome, BatchOutcome::Saved(_)));
        assert!(report.items[1].output.is_none());
        let limits = BatchLimits {
            max_processed_pixels: 1,
            ..BatchLimits::default()
        };
        let report = run(
            &inputs,
            directory.options(),
            limits,
            1,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        // 第一项即使因现存输出保存失败，已做的处理仍消耗预算，之后不会再解码。
        assert_eq!((report.decoded_pixels, report.processed_pixels), (1, 1));
        assert!(report
            .items
            .iter()
            .all(|item| matches!(item.outcome, BatchOutcome::Failed(_))));
    }

    #[test]
    fn image_batch_selected_resume_keeps_indices_frozen_names_and_saved_bytes() {
        let directory = TestDirectory::new();
        let sources = [
            directory.input("in/first.png", 2),
            directory.input("in/second.png", 3),
        ];
        let source_bytes: Vec<_> = sources
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let mut options = directory.options();
        options.naming = BatchNaming::Template {
            template: "resume_{width}x{height}_{date}_{time}".into(),
            date_format: "yyyyMMdd".into(),
        };
        let plan = BatchPlan::new(
            &sources,
            options,
            BatchLimits {
                max_decoded_pixels: 3,
                max_processed_pixels: 3,
                ..BatchLimits::default()
            },
            51,
        )
        .unwrap();
        let expected_first = plan.output_path(0, 2, 1).unwrap();
        let expected_second = plan.output_path(1, 3, 1).unwrap();
        let cancel = AtomicBool::new(false);
        let initial = plan
            .run(51, &cancel, |item| {
                if item.index == 0 {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
            })
            .unwrap();
        assert!(matches!(initial.items[0].outcome, BatchOutcome::Saved(_)));
        assert_eq!(initial.items[0].output.as_ref(), Some(&expected_first));
        assert_eq!(initial.items[1].outcome, BatchOutcome::Cancelled);
        let saved_first_bytes = std::fs::read(&expected_first).unwrap();
        let mut events = Vec::new();
        let resumed = plan
            .run_selected_with_sharpening(
                51,
                &[1],
                ColorAdjustments::default(),
                SharpenOptions::default(),
                None,
                &AtomicBool::new(false),
                |item| events.push(item.index),
            )
            .unwrap();
        assert_eq!(events, [1]);
        assert_eq!(resumed.request_id, 51);
        assert_eq!(resumed.items.len(), 1);
        assert_eq!(resumed.items[0].index, 1);
        assert_eq!(resumed.items[0].source, sources[1]);
        assert_eq!(resumed.items[0].output.as_ref(), Some(&expected_second));
        assert!(matches!(resumed.items[0].outcome, BatchOutcome::Saved(_)));
        assert!(expected_second.to_string_lossy().ends_with("_0002.png"));
        assert_eq!((resumed.decoded_pixels, resumed.processed_pixels), (3, 3));
        assert_eq!(std::fs::read(&expected_first).unwrap(), saved_first_bytes);
        for (path, original) in sources.iter().zip(source_bytes) {
            assert_eq!(std::fs::read(path).unwrap(), original);
        }
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 2);
    }

    #[test]
    fn image_batch_selected_recovers_missing_input_without_reprocessing_saved_item() {
        let directory = TestDirectory::new();
        let first = directory.input("in/first.png", 1);
        let missing = directory.0.join("in/recovered.png");
        let plan = BatchPlan::new(
            &[first.clone(), missing.clone()],
            directory.options(),
            BatchLimits::default(),
            52,
        )
        .unwrap();
        assert!(plan.inputs[1].unavailable.is_some());
        let initial = plan.run(52, &AtomicBool::new(false), |_| {}).unwrap();
        let BatchOutcome::Saved(saved) = &initial.items[0].outcome else {
            panic!("first item was not saved");
        };
        let saved_bytes = std::fs::read(&saved.path).unwrap();
        assert!(matches!(initial.items[1].outcome, BatchOutcome::Failed(_)));
        std::fs::copy(&first, &missing).unwrap();
        let repaired_bytes = std::fs::read(&missing).unwrap();
        let retry = plan
            .run_selected_with_sharpening(
                52,
                &[1],
                ColorAdjustments::default(),
                SharpenOptions::default(),
                None,
                &AtomicBool::new(false),
                |item| assert_eq!(item.index, 1),
            )
            .unwrap();
        assert_eq!(retry.items.len(), 1);
        assert!(matches!(retry.items[0].outcome, BatchOutcome::Saved(_)));
        assert_eq!(
            retry.items[0].output,
            Some(plan.output_path(1, 1, 1).unwrap())
        );
        assert_eq!(std::fs::read(&saved.path).unwrap(), saved_bytes);
        assert_eq!(std::fs::read(&missing).unwrap(), repaired_bytes);
        assert_eq!(std::fs::read(&first).unwrap(), repaired_bytes);
    }

    #[test]
    fn image_batch_selected_validates_selection_before_work_and_honors_precancel() {
        let directory = TestDirectory::new();
        let paths = [
            directory.0.join("in/missing-a.png"),
            directory.0.join("in/missing-b.png"),
        ];
        let plan = BatchPlan::new(&paths, directory.options(), BatchLimits::default(), 53).unwrap();
        for indices in [&[0, 0][..], &[1, 0], &[2], &[usize::MAX]] {
            assert!(matches!(
                plan.run_selected_with_sharpening(
                    53,
                    indices,
                    ColorAdjustments::default(),
                    SharpenOptions::default(),
                    None,
                    &AtomicBool::new(false),
                    |_| panic!("invalid selection reached progress"),
                ),
                Err(ToolError::Invalid(_))
            ));
        }
        assert!(plan
            .run_selected_with_sharpening(
                54,
                &[0],
                ColorAdjustments::default(),
                SharpenOptions::default(),
                None,
                &AtomicBool::new(false),
                |_| panic!("stale request reached progress"),
            )
            .is_err());
        let empty = plan
            .run_selected_with_sharpening(
                53,
                &[],
                ColorAdjustments::default(),
                SharpenOptions::default(),
                None,
                &AtomicBool::new(true),
                |_| panic!("empty selection reached progress"),
            )
            .unwrap();
        assert!(empty.items.is_empty());
        assert_eq!((empty.decoded_pixels, empty.processed_pixels), (0, 0));
        let mut events = Vec::new();
        let cancelled = plan
            .run_selected_with_sharpening(
                53,
                &[0, 1],
                ColorAdjustments::default(),
                SharpenOptions::default(),
                None,
                &AtomicBool::new(true),
                |item| events.push(item.index),
            )
            .unwrap();
        assert_eq!(events, [0, 1]);
        assert!(cancelled
            .items
            .iter()
            .all(|item| item.outcome == BatchOutcome::Cancelled && item.output.is_none()));
        assert_eq!(
            (cancelled.decoded_pixels, cancelled.processed_pixels),
            (0, 0)
        );
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn image_batch_selected_protects_unselected_explicit_source_paths() {
        let directory = TestDirectory::new();
        let source = directory.input("in/input.png", 1);
        let before = std::fs::read(&source).unwrap();
        let protected_missing = directory.output().join("input_processed_0001.png");
        let plan = BatchPlan::new(
            &[source.clone(), protected_missing.clone()],
            directory.options(),
            BatchLimits::default(),
            55,
        )
        .unwrap();
        let report = plan
            .run_selected_with_sharpening(
                55,
                &[0],
                ColorAdjustments::default(),
                SharpenOptions::default(),
                None,
                &AtomicBool::new(false),
                |item| assert_eq!(item.index, 0),
            )
            .unwrap();
        assert_eq!(report.items.len(), 1);
        assert!(matches!(report.items[0].outcome, BatchOutcome::Failed(_)));
        assert_eq!(report.items[0].output.as_ref(), Some(&protected_missing));
        assert_eq!((report.decoded_pixels, report.processed_pixels), (1, 0));
        assert!(!protected_missing.exists());
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }

    #[test]
    fn image_batch_template_uses_final_dimensions_and_one_fixed_moment() {
        let directory = TestDirectory::new();
        let source = directory.input("in/input.png", 2);
        let mut options = directory.options();
        options.naming = BatchNaming::Template {
            template: "batch_{width}x{height}_{date}_{time}".into(),
            date_format: "yyyyMMdd".into(),
        };
        options.transform.resize = Some(ResizeOptions {
            width: 1,
            height: 2,
            filter: ResizeFilter::Nearest,
        });
        let plan = BatchPlan::new(&[source], options, BatchLimits::default(), 0).unwrap();
        let preview = plan.output_path(0, 1, 2).unwrap();
        assert_eq!(preview, plan.output_path(0, 1, 2).unwrap());
        assert!(preview
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("batch_1x2_"));
        let report = plan.run(0, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(report.items[0].output.as_ref(), Some(&preview));
        assert!(matches!(report.items[0].outcome, BatchOutcome::Saved(_)));
        let image = crate::export::read_image(&preview).unwrap();
        assert_eq!((image.width, image.height), (1, 2));
    }

    #[test]
    fn image_batch_output_never_replaces_any_explicit_input_even_if_missing() {
        let directory = TestDirectory::new();
        let source = directory.input("in/input.png", 1);
        // Windows canonicalize 返回长路径前缀；缺失的末级本身不参与规范化或读取。
        for parent in [
            directory.output(),
            std::fs::canonicalize(directory.output()).unwrap(),
        ] {
            let input_in_output = parent.join("INPUT_processed_0001.PNG");
            let report = run(
                &[source.clone(), input_in_output.clone()],
                directory.options(),
                BatchLimits::default(),
                0,
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            assert!(report
                .items
                .iter()
                .all(|item| matches!(item.outcome, BatchOutcome::Failed(_))));
            assert_eq!(report.processed_pixels, 0);
            assert!(!input_in_output.exists());
            assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
        }
    }

    #[test]
    fn image_batch_path_protection_compares_dos_and_unc_long_prefixes() {
        assert!(same_absolute_path(
            Path::new(r"D:\out\next.png"),
            Path::new(r"\\?\D:\OUT\NEXT.PNG")
        ));
        assert!(same_absolute_path(
            Path::new(r"\\server\share\out\next.png"),
            Path::new(r"\\?\UNC\SERVER\share\out\NEXT.PNG")
        ));
        assert!(!same_absolute_path(
            Path::new(r"D:\out\next.png"),
            Path::new(r"\\?\D:\other\next.png")
        ));
    }

    #[test]
    fn image_batch_missing_input_does_not_become_a_later_generated_input() {
        let directory = TestDirectory::new();
        let source = directory.input("in/input.png", 1);
        let missing = directory.0.join("in/late.png");
        let plan = BatchPlan::new(
            &[source, missing.clone()],
            directory.options(),
            BatchLimits::default(),
            0,
        )
        .unwrap();
        let report = plan
            .run(0, &AtomicBool::new(false), |item| {
                if let BatchOutcome::Saved(saved) = &item.outcome {
                    std::fs::copy(&saved.path, &missing).unwrap();
                }
            })
            .unwrap();
        assert!(missing.exists());
        assert!(matches!(report.items[0].outcome, BatchOutcome::Saved(_)));
        assert!(matches!(report.items[1].outcome, BatchOutcome::Failed(_)));
        assert_eq!((report.decoded_pixels, report.processed_pixels), (1, 1));
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 1);
    }

    #[test]
    fn image_batch_invalid_global_options_fail_before_output() {
        let directory = TestDirectory::new();
        let source = directory.input("in/input.png", 1);
        let paths = [source];
        let mut options = directory.options();
        options.naming = BatchNaming::Template {
            template: "{index}".into(),
            date_format: "yyyyMMdd".into(),
        };
        assert!(BatchPlan::new(&paths, options, BatchLimits::default(), 0).is_err());
        let mut options = directory.options();
        options.directory = directory.0.join("not-created");
        assert!(BatchPlan::new(&paths, options, BatchLimits::default(), 0).is_err());
        assert!(BatchPlan::new(&[], directory.options(), BatchLimits::default(), 0).is_err());
        assert!(BatchPlan::new(
            &paths,
            directory.options(),
            BatchLimits {
                max_decoded_pixels: 0,
                ..BatchLimits::default()
            },
            0
        )
        .is_err());
        assert!(BatchPlan::new(
            &paths,
            directory.options(),
            BatchLimits {
                image: ToolLimits {
                    max_items: 0,
                    ..ToolLimits::default()
                },
                ..BatchLimits::default()
            },
            0
        )
        .is_err());
        assert_eq!(std::fs::read_dir(directory.output()).unwrap().count(), 0);
    }
}
