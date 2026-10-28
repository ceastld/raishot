//! 邻图只缓存同次读取的原始像素与来源事实，不创建显示块或额外工作线程。
//! 文件身份能发现常见替换与修改，但不是内容CAS，也不能封住检查后的路径替换。
use crate::{
    file_snapshot::{self, FileSnapshot},
    image_decode::{self, DecodedImage, MAX_DECODE_BYTES},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub(super) const MAX_ENTRY_BYTES: u64 = 48 * 1024 * 1024;
/// 绝对路径最多64KiB；逐级检查的路径副本、祖先数组及属性句柄另留有界余量。
pub(super) const QUALIFICATION_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 64 * 1024;
// Entry/Grant栈值、Arc头及元数据对象外壳；其拥有的可变长存储另按实际capacity计。
const BOOKKEEPING_BYTES: u64 = 64 * 1024;

/// 宿主在授予任务时冻结驻留账本，并在同一代次内取消过期任务。
pub(super) struct Grant {
    pub serial: u64,
    pub epoch: u64,
    /// 使用目录模型给出的绝对路径，不在后台解析链接或改变工作目录语义。
    pub path: PathBuf,
    pub resident_bytes: u64,
    pub max_entry_bytes: u64,
    pub cancel: Arc<AtomicBool>,
}

pub(super) struct Entry {
    pub path: PathBuf,
    pub source_file: FileSnapshot,
    /// 包括像素、sidecar、警告、路径的实际容量，以及有界对象外壳余量。
    pub retained_bytes: u64,
    decoded: DecodedImage,
}

fn message(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn budget_error() -> String {
    message(
        "邻图预取超过缓存或共享驻留预算",
        "Neighbor prefetch exceeds its cache or shared resident budget",
    )
}

fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(message("邻图预取已取消", "Neighbor prefetch canceled"))
    } else {
        Ok(())
    }
}

fn total(parts: &[u64]) -> Result<u64, String> {
    parts.iter().try_fold(0u64, |sum, part| {
        sum.checked_add(*part).ok_or_else(budget_error)
    })
}

fn bookkeeping(path: &PathBuf) -> Result<u64, String> {
    if !path.is_absolute() || path.capacity() > MAX_PATH_BYTES {
        return Err(message(
            "邻图预取需要有界的绝对文件路径",
            "Neighbor prefetch requires a bounded absolute file path",
        ));
    }
    total(&[BOOKKEEPING_BYTES, path.capacity() as u64])
}

fn source_changed() -> String {
    message(
        "邻图源文件已变化或不存在",
        "Neighbor source changed or is missing",
    )
}

fn local_snapshot_with(
    cancel: &AtomicBool,
    local: impl FnOnce() -> Result<(), String>,
    inspect: impl FnOnce() -> Result<Option<FileSnapshot>, String>,
) -> Result<FileSnapshot, String> {
    cancelled(cancel)?;
    let local = local();
    cancelled(cancel)?;
    local?;
    let snapshot = inspect();
    cancelled(cancel)?;
    snapshot?.ok_or_else(source_changed)
}

fn local_snapshot(path: &Path, cancel: &AtomicBool) -> Result<FileSnapshot, String> {
    local_snapshot_with(
        cancel,
        || crate::image_collection::ensure_local_file(path),
        || file_snapshot::inspect(path).map_err(|error| error.to_string()),
    )
}

fn retained_bytes(decoded: &DecodedImage, bookkeeping: u64) -> Result<u64, String> {
    let expected = u64::from(decoded.image.width)
        .checked_mul(u64::from(decoded.image.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(budget_error)?;
    if expected == 0
        || expected != decoded.image.bgra.len() as u64
        || decoded.metadata.display_dimensions != (decoded.image.width, decoded.image.height)
    {
        return Err(message(
            "邻图像素与解码尺寸不一致",
            "Neighbor pixels do not match decoded dimensions",
        ));
    }
    total(&[
        bookkeeping,
        decoded.image.bgra.capacity() as u64,
        decoded
            .metadata_source
            .as_ref()
            .map_or(0, |source| source.retained_bytes() as u64),
        decoded
            .warning
            .as_ref()
            .map_or(0, |warning| warning.capacity() as u64),
    ])
}

fn check_retained(actual: u64, limit: u64, resident: u64) -> Result<(), String> {
    if actual > limit.min(MAX_ENTRY_BYTES)
        || total(&[resident, actual, QUALIFICATION_BYTES])? > MAX_DECODE_BYTES
    {
        return Err(budget_error());
    }
    Ok(())
}

/// 仅交付完整成功的条目；失败由宿主静默跳过，不据此标记文件损坏。
/// resident_bytes必须包含宿主在整个任务期间仍持有的像素、缓存与退休对象。
pub(super) fn load(grant: Grant) -> Result<Entry, String> {
    let Grant {
        serial: _,
        epoch: _,
        path,
        resident_bytes,
        max_entry_bytes,
        cancel,
    } = grant;
    let result = (|| {
        cancelled(&cancel)?;
        let bookkeeping = bookkeeping(&path)?;
        let limit = max_entry_bytes.min(MAX_ENTRY_BYTES);
        let max_output = limit.checked_sub(bookkeeping).ok_or_else(budget_error)?;
        if max_output < 4 {
            return Err(budget_error());
        }
        let decode_resident = total(&[resident_bytes, bookkeeping, QUALIFICATION_BYTES])?;
        if decode_resident > MAX_DECODE_BYTES {
            return Err(budget_error());
        }
        let before = local_snapshot(&path, &cancel)?;
        let decoded = image_decode::decode_path_with_resident_budget_cancellable(
            &path,
            max_output,
            decode_resident,
            &cancel,
        );
        cancelled(&cancel)?;
        let decoded = decoded.map_err(|error| error.to_string())?;
        let retained_bytes = retained_bytes(&decoded, bookkeeping)?;
        check_retained(retained_bytes, limit, resident_bytes)?;
        let after = local_snapshot(&path, &cancel)?;
        if before != after {
            return Err(source_changed());
        }
        Ok(Entry {
            path,
            source_file: before,
            decoded,
            retained_bytes,
        })
    })();
    cancelled(&cancel)?;
    result
}

fn matches_with(
    cached_path: &Path,
    expected: &FileSnapshot,
    path: &Path,
    cancel: &AtomicBool,
    inspect: impl FnOnce() -> Result<FileSnapshot, String>,
) -> Result<bool, String> {
    cancelled(cancel)?;
    if path != cached_path {
        return Ok(false);
    }
    let actual = inspect();
    cancelled(cancel)?;
    Ok(*expected == actual?)
}

impl Entry {
    /// 消费前重新检查本地资格与完整身份；精确路径不同可以安全地退回普通加载。
    /// 宿主须为本次属性检查保留QUALIFICATION_BYTES，并另行核对serial/epoch。
    pub(super) fn matches(&self, path: &Path, cancel: &AtomicBool) -> Result<bool, String> {
        matches_with(&self.path, &self.source_file, path, cancel, || {
            local_snapshot(path, cancel)
        })
    }

    /// 只有宿主已通过matches和当前代次检查后才消费；这里不生成显示缓存。
    pub(super) fn into_decoded(self) -> DecodedImage {
        self.decoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{capture::Image, image_metadata::ImageMetadata};
    use std::sync::atomic::AtomicUsize;

    fn identity() -> FileSnapshot {
        FileSnapshot {
            volume: 1,
            id: [2; 16],
            created: 3,
            written: 4,
            bytes: 5,
            attributes: 0x20,
        }
    }

    #[test]
    fn image_viewer_prefetch_rechecks_full_identity_and_prioritizes_cancel() {
        let path = Path::new("image.png");
        let expected = identity();
        let cancel = AtomicBool::new(false);
        assert!(matches_with(path, &expected, path, &cancel, || Ok(expected.clone())).unwrap());
        let mut replacement = expected.clone();
        replacement.id[15] ^= 1;
        assert!(!matches_with(path, &expected, path, &cancel, || Ok(replacement)).unwrap());
        assert!(
            !matches_with(path, &expected, Path::new("other.png"), &cancel, || {
                panic!("a different path must not be inspected")
            })
            .unwrap()
        );
        let error = local_snapshot_with(
            &cancel,
            || {
                cancel.store(true, Ordering::Release);
                Err("local error".into())
            },
            || panic!("canceled qualification must not open the source"),
        )
        .unwrap_err();
        assert_eq!(error, cancelled(&cancel).unwrap_err());
        cancel.store(false, Ordering::Release);
        let error = matches_with(path, &expected, path, &cancel, || {
            cancel.store(true, Ordering::Release);
            Err("identity error".into())
        })
        .unwrap_err();
        assert_eq!(error, cancelled(&cancel).unwrap_err());
    }

    #[test]
    fn image_viewer_prefetch_accounts_capacity_and_both_budget_boundaries() {
        use crate::{
            image_color_management::{ColorConversion, ManagedMetadata},
            image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
            image_export_metadata::SourceMetadata,
        };
        let mut bgra = Vec::with_capacity(4096);
        bgra.extend_from_slice(&[3, 2, 1, 128]);
        let mut warning = String::with_capacity(8192);
        warning.push('x');
        let absent = FieldSummary {
            status: MetadataStatus::Absent,
            encoded_bytes: 0,
            decoded_bytes: None,
        };
        let facts = ManagedMetadata {
            source: Summary {
                icc: absent,
                exif: absent,
                color_model: ColorModel::Rgb,
                bits_per_channel: Some(8),
                orientation: Some(image::metadata::Orientation::NoTransforms),
            },
            profile: None,
            color_conversion: ColorConversion::AssumedSrgb,
        };
        let mut reason = String::with_capacity(16 * 1024);
        reason.push('x');
        let sidecar = Arc::new(
            SourceMetadata::new(facts, None)
                .unwrap()
                .with_exif(Err(reason)),
        );
        let sidecar_bytes = sidecar.retained_bytes() as u64;
        let decoded = DecodedImage {
            image: Image {
                width: 1,
                height: 1,
                bgra,
                readback_ms: 0.,
            },
            metadata: ImageMetadata {
                format: image::ImageFormat::Png.into(),
                encoded_dimensions: Some((1, 1)),
                display_dimensions: (1, 1),
                orientation: Some(image::metadata::Orientation::NoTransforms),
                encoded_bytes: Some(1),
                decoded_color: Some(image::ColorType::Rgba8),
                details: Some(facts),
            },
            warning: Some(warning),
            metadata_source: Some(sidecar),
        };
        let actual = retained_bytes(&decoded, 17).unwrap();
        assert_eq!(
            actual,
            17 + decoded.image.bgra.capacity() as u64
                + decoded.warning.as_ref().unwrap().capacity() as u64
                + sidecar_bytes
        );
        let resident = MAX_DECODE_BYTES - actual - QUALIFICATION_BYTES;
        check_retained(actual, actual, resident).unwrap();
        assert!(check_retained(actual, actual - 1, resident).is_err());
        assert!(check_retained(actual, actual, resident + 1).is_err());
        assert!(check_retained(MAX_ENTRY_BYTES + 1, u64::MAX, 0).is_err());
        assert!(check_retained(actual, actual, u64::MAX).is_err());
    }

    struct PngFixture(PathBuf);
    impl PngFixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "rustshot-prefetch-{stamp}-{}",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("neighbor.png");
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().unwrap();
                writer
                    .write_image_data(&[11, 22, 33, 128, 44, 55, 66, 0])
                    .unwrap();
            }
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for PngFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir(self.0.parent().unwrap());
        }
    }

    #[test]
    fn image_viewer_prefetch_loads_raw_png_and_invalidates_changed_source() {
        let fixture = PngFixture::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let entry = load(Grant {
            serial: 1,
            epoch: 2,
            path: fixture.0.clone(),
            resident_bytes: 8 * 1024 * 1024,
            max_entry_bytes: MAX_ENTRY_BYTES,
            cancel: cancel.clone(),
        })
        .unwrap();
        assert_eq!(entry.decoded.image.bgra, [33, 22, 11, 128, 66, 55, 44, 0]);
        assert_eq!(entry.decoded.metadata.display_dimensions, (2, 1));
        assert!(entry.matches(&fixture.0, &cancel).unwrap());
        assert_eq!(
            entry.retained_bytes,
            retained_bytes(&entry.decoded, bookkeeping(&entry.path).unwrap()).unwrap()
        );
        assert!(entry.retained_bytes <= MAX_ENTRY_BYTES);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.0)
            .unwrap()
            .write_all(&[0])
            .unwrap();
        assert!(!entry.matches(&fixture.0, &cancel).unwrap());
        cancel.store(true, Ordering::Release);
        assert!(entry.matches(&fixture.0, &cancel).is_err());
        // 消费转移原Vec和同次sidecar，不复制为显示块或丢弃透明像素的RGB。
        let pixels = entry.decoded.image.bgra.as_ptr();
        let decoded = entry.into_decoded();
        assert_eq!(decoded.image.bgra.as_ptr(), pixels);
    }
}
