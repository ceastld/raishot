//! 任意双文件对照的事务读取；调用方在后台执行并在整对成功后替换旧结果。
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};

use crate::{
    capture::Image,
    image_decode::{decode_path_with_foreground_budget_cancellable, MAX_DECODE_BYTES},
    image_tools::{check_cancel, validate_input, ToolError, ToolLimits, ToolResult},
};

const MAX_PAIR_PIXELS: u64 = 32_000_000;

pub(crate) struct FileComparePair {
    pub images: [Arc<Image>; 2],
    pub labels: [String; 2],
    pub warnings: [Option<String>; 2],
}

/// 路径顺序即左右顺序，重复来源也逐项读取。外部驻留量须包含仍保留的旧对照。
/// 方向、颜色和首帧/首页规则沿用统一解码器；不保留来源元数据。
/// 前景解码器尚未闭合所有格式的私有工作区，不能据此承诺全格式峰值上界。
pub(crate) fn load_pair(
    paths: &[PathBuf],
    external_resident_bytes: u64,
    cancel: &AtomicBool,
) -> ToolResult<FileComparePair> {
    load_pair_with_limits(paths, external_resident_bytes, MAX_PAIR_PIXELS, cancel)
}

fn load_pair_with_limits(
    paths: &[PathBuf],
    external_resident_bytes: u64,
    max_pixels: u64,
    cancel: &AtomicBool,
) -> ToolResult<FileComparePair> {
    let result = (|| {
        check_cancel(cancel)?;
        let [left, right] = paths else {
            return Err(ToolError::Invalid(
                tr!("请选择恰好两个图片文件", "Choose exactly two image files").into(),
            ));
        };
        if max_pixels == 0 || max_pixels > MAX_PAIR_PIXELS {
            return Err(budget_error());
        }
        let labels = [
            left.to_string_lossy().into_owned(),
            right.to_string_lossy().into_owned(),
        ];
        let mut warnings = [None, None];
        let resident = resident_bytes(external_resident_bytes, &[], &labels, &warnings)?;
        let (first, warning, first_pixels) =
            load_one(left, &labels[0], max_pixels, resident, cancel)?;
        warnings[0] = warning;
        let remaining = max_pixels
            .checked_sub(first_pixels)
            .ok_or_else(budget_error)?;
        let resident = resident_bytes(external_resident_bytes, &[&first], &labels, &warnings)?;
        let (second, warning, second_pixels) =
            load_one(right, &labels[1], remaining, resident, cancel)?;
        warnings[1] = warning;
        first_pixels
            .checked_add(second_pixels)
            .filter(|pixels| *pixels <= max_pixels)
            .ok_or_else(budget_error)?;
        resident_bytes(
            external_resident_bytes,
            &[&first, &second],
            &labels,
            &warnings,
        )?;
        check_cancel(cancel)?;
        Ok(FileComparePair {
            images: [Arc::new(first), Arc::new(second)],
            labels,
            warnings,
        })
    })();
    // 错误与取消竞争时优先返回取消；局部已完成的图片随整次失败一起释放。
    check_cancel(cancel)?;
    result
}

fn load_one(
    path: &Path,
    label: &str,
    remaining_pixels: u64,
    resident: u64,
    cancel: &AtomicBool,
) -> ToolResult<(Image, Option<String>, u64)> {
    check_cancel(cancel)?;
    if remaining_pixels == 0 {
        return Err(source_error(label, budget_error()));
    }
    let local = crate::image_collection::ensure_local_file(path);
    check_cancel(cancel)?;
    local.map_err(|error| source_error(label, error))?;
    let max_output_bytes = remaining_pixels.checked_mul(4).ok_or_else(budget_error)?;
    let decoded =
        decode_path_with_foreground_budget_cancellable(path, max_output_bytes, resident, cancel);
    check_cancel(cancel)?;
    let (image, warning) = {
        let decoded = decoded.map_err(|error| source_error(label, error))?;
        // 块结束时释放未移出的metadata与metadata_source，避免带到下一次解码。
        (decoded.image, decoded.warning)
    };
    let pixels = validate_input(
        &image,
        ToolLimits {
            max_input_pixels: remaining_pixels,
            max_side: u32::MAX,
            ..ToolLimits::default()
        },
    )
    .map_err(|error| source_error(label, error))?;
    check_cancel(cancel)?;
    Ok((image, warning, pixels))
}

fn resident_bytes(
    external: u64,
    images: &[&Image],
    labels: &[String; 2],
    warnings: &[Option<String>; 2],
) -> ToolResult<u64> {
    let mut resident = external;
    for capacity in images
        .iter()
        .map(|image| image.bgra.capacity())
        .chain(labels.iter().map(String::capacity))
        .chain(warnings.iter().flatten().map(String::capacity))
    {
        let capacity = u64::try_from(capacity).map_err(|_| budget_error())?;
        resident = resident.checked_add(capacity).ok_or_else(budget_error)?;
    }
    if resident > MAX_DECODE_BYTES {
        return Err(budget_error());
    }
    Ok(resident)
}

fn budget_error() -> ToolError {
    ToolError::Invalid(
        tr!(
            "双图累计像素或驻留数据超过对照预算",
            "The image pair exceeds the pixel or resident-data budget"
        )
        .into(),
    )
}

fn source_error(label: &str, error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(trf!(
        "无法读取图片“{label}”：{error}",
        "Could not read image \"{label}\": {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use std::{
        fs,
        sync::atomic::AtomicU64,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rabshot-image-compare-files-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn png(&self, name: &str, width: u32, height: u32, rgba: &[u8]) -> PathBuf {
            let mut bytes = Vec::new();
            image::codecs::png::PngEncoder::new(&mut bytes)
                .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
                .unwrap();
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            for name in ["a.png", "b.png", "bad.png"] {
                let _ = fs::remove_file(self.0.join(name));
            }
            let _ = fs::remove_dir(&self.0);
        }
    }

    #[test]
    fn image_compare_files_reads_order_and_duplicate_sources_without_changing_bytes() {
        let directory = TestDirectory::new();
        let a = directory.png("a.png", 2, 1, &[11, 22, 33, 255, 44, 55, 66, 128]);
        let b = directory.png("b.png", 1, 2, &[77, 88, 99, 255, 111, 122, 133, 64]);
        let original = [fs::read(&a).unwrap(), fs::read(&b).unwrap()];
        let cancel = AtomicBool::new(false);
        let pair = load_pair(&[b.clone(), a.clone()], 0, &cancel).unwrap();
        assert_eq!(
            pair.labels,
            [
                b.to_string_lossy().into_owned(),
                a.to_string_lossy().into_owned()
            ]
        );
        assert_eq!((pair.images[0].width, pair.images[0].height), (1, 2));
        assert_eq!(pair.images[0].bgra, [99, 88, 77, 255, 133, 122, 111, 64]);
        assert_eq!(pair.images[1].bgra, [33, 22, 11, 255, 66, 55, 44, 128]);
        let pair = load_pair(&[a.clone(), a.clone()], 0, &cancel).unwrap();
        assert_eq!(pair.labels[0], pair.labels[1]);
        assert_eq!(pair.images[0].bgra, pair.images[1].bgra);
        assert_eq!(fs::read(&a).unwrap(), original[0]);
        assert_eq!(fs::read(&b).unwrap(), original[1]);
    }

    #[test]
    fn image_compare_files_bad_second_source_does_not_replace_previous_pair() {
        let directory = TestDirectory::new();
        let a = directory.png("a.png", 1, 1, &[11, 22, 33, 255]);
        let b = directory.png("b.png", 1, 1, &[44, 55, 66, 128]);
        let bad = directory.0.join("bad.png");
        let bad_bytes = b"not an image";
        fs::write(&bad, bad_bytes).unwrap();
        let cancel = AtomicBool::new(false);
        let previous = load_pair(&[a.clone(), b], 0, &cancel).unwrap();
        let previous_resident = resident_bytes(
            0,
            &[previous.images[0].as_ref(), previous.images[1].as_ref()],
            &previous.labels,
            &previous.warnings,
        )
        .unwrap();
        let result = load_pair(&[a, bad.clone()], previous_resident, &cancel);
        assert!(
            matches!(result, Err(ToolError::Failed(message)) if message.contains(bad.to_string_lossy().as_ref()))
        );
        assert_eq!(previous.images[0].bgra, [33, 22, 11, 255]);
        assert_eq!(previous.images[1].bgra, [66, 55, 44, 128]);
        assert_eq!(fs::read(bad).unwrap(), bad_bytes);
    }

    #[test]
    fn image_compare_files_cumulative_pixels_reject_second_image_with_small_limit() {
        let directory = TestDirectory::new();
        let a = directory.png("a.png", 2, 1, &[11, 22, 33, 255, 44, 55, 66, 255]);
        let b = directory.png("b.png", 1, 2, &[77, 88, 99, 255, 111, 122, 133, 255]);
        let cancel = AtomicBool::new(false);
        let paths = [a, b.clone()];
        assert!(load_pair_with_limits(&paths, 0, 4, &cancel).is_ok());
        assert!(matches!(
            load_pair_with_limits(&paths, 0, 3, &cancel),
            Err(ToolError::Failed(message)) if message.contains(b.to_string_lossy().as_ref())
        ));
        assert!(load_pair_with_limits(&paths, 0, 2, &cancel).is_err());
        assert!(load_pair(&paths, u64::MAX, &cancel).is_err());
    }

    #[test]
    fn image_compare_files_requires_two_paths_and_prioritizes_precancellation() {
        let path = PathBuf::from("not-opened.png");
        let cancel = AtomicBool::new(false);
        for paths in [vec![], vec![path.clone()], vec![path.clone(); 3]] {
            assert!(matches!(
                load_pair(&paths, 0, &cancel),
                Err(ToolError::Invalid(_))
            ));
        }
        assert!(matches!(
            load_pair(&[path.clone(), path], u64::MAX, &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
    }
}
