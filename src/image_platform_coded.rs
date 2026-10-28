//! 沿已校验的图项extent读取码流头；不重组或复制整份压缩图项。
use super::PlatformError;
use std::{ops::Range, sync::atomic::AtomicBool};

#[path = "image_platform_av1.rs"]
mod av1;
#[path = "image_platform_hevc.rs"]
mod hevc;

#[derive(Clone, Copy)]
enum Backing<'a> {
    Slice(&'a [u8]),
    Extents {
        source: &'a [u8],
        extents: &'a [Range<usize>],
    },
}

#[derive(Clone, Copy)]
pub(super) struct ByteView<'a> {
    backing: Backing<'a>,
    start: usize,
    length: usize,
}

impl<'a> ByteView<'a> {
    pub(super) fn from_slice(bytes: &'a [u8]) -> Self {
        Self {
            backing: Backing::Slice(bytes),
            start: 0,
            length: bytes.len(),
        }
    }

    pub(super) fn from_extents(
        source: &'a [u8],
        extents: &'a [Range<usize>],
    ) -> Result<Self, PlatformError> {
        if extents.is_empty() || extents.len() > 64 {
            return Err(invalid("图项分段数量无效", "Invalid item extent count"));
        }
        let mut length = 0usize;
        for extent in extents {
            if extent.start >= extent.end || extent.end > source.len() {
                return Err(invalid("图项分段范围无效", "Invalid item extent range"));
            }
            length = length
                .checked_add(extent.end - extent.start)
                .filter(|length| *length as u64 <= super::MAX_BYTES)
                .ok_or_else(budget)?;
        }
        Ok(Self {
            backing: Backing::Extents { source, extents },
            start: 0,
            length,
        })
    }

    pub(super) fn len(self) -> usize {
        self.length
    }

    pub(super) fn byte(self, index: usize) -> Result<u8, PlatformError> {
        if index >= self.length {
            return Err(invalid(
                "图片码流头被截断",
                "Truncated image bitstream header",
            ));
        }
        let mut index = self.start.checked_add(index).ok_or_else(budget)?;
        match self.backing {
            Backing::Slice(bytes) => bytes
                .get(index)
                .copied()
                .ok_or_else(|| invalid("图片码流范围无效", "Invalid image bitstream range")),
            Backing::Extents { source, extents } => {
                for extent in extents {
                    let count = extent.end - extent.start;
                    if index < count {
                        return source.get(extent.start + index).copied().ok_or_else(|| {
                            invalid("图片码流范围无效", "Invalid image bitstream range")
                        });
                    }
                    index -= count;
                }
                Err(invalid("图片码流范围无效", "Invalid image bitstream range"))
            }
        }
    }

    pub(super) fn slice(self, range: Range<usize>) -> Result<Self, PlatformError> {
        if range.start > range.end || range.end > self.length {
            return Err(invalid("图片码流范围无效", "Invalid image bitstream range"));
        }
        Ok(Self {
            backing: self.backing,
            start: self.start.checked_add(range.start).ok_or_else(budget)?,
            length: range.end - range.start,
        })
    }

    /// 按逻辑顺序直接复制到调用方已分配的准确长度缓冲；取消可能留下未完成的目标。
    /// 每次最多复制64KiB，不按字节重复定位extent，也不创建压缩来源副本。
    pub(super) fn copy_to(self, dst: &mut [u8], cancel: &AtomicBool) -> Result<(), PlatformError> {
        self.copy_to_checked(dst, || check_cancel(cancel))
    }

    fn copy_to_checked(
        self,
        dst: &mut [u8],
        mut checkpoint: impl FnMut() -> Result<(), PlatformError>,
    ) -> Result<(), PlatformError> {
        fn copy_chunks(
            source: &[u8],
            dst: &mut [u8],
            checkpoint: &mut impl FnMut() -> Result<(), PlatformError>,
        ) -> Result<(), PlatformError> {
            for (source, dst) in source.chunks(64 * 1024).zip(dst.chunks_mut(64 * 1024)) {
                checkpoint()?;
                dst.copy_from_slice(source);
            }
            Ok(())
        }

        checkpoint()?;
        if dst.len() != self.length {
            return Err(invalid(
                "图片码流复制目标长度不匹配",
                "Image bitstream copy destination length does not match",
            ));
        }
        match self.backing {
            Backing::Slice(source) => {
                let end = self.start.checked_add(self.length).ok_or_else(budget)?;
                let source = source
                    .get(self.start..end)
                    .ok_or_else(|| invalid("图片码流范围无效", "Invalid image bitstream range"))?;
                copy_chunks(source, dst, &mut checkpoint)?;
            }
            Backing::Extents { source, extents } => {
                let mut skip = self.start;
                let mut written = 0usize;
                for extent in extents {
                    if written == self.length {
                        break;
                    }
                    let count = extent.end - extent.start;
                    if skip >= count {
                        skip -= count;
                        continue;
                    }
                    let count = (count - skip).min(self.length - written);
                    let start = extent.start.checked_add(skip).ok_or_else(budget)?;
                    let end = start.checked_add(count).ok_or_else(budget)?;
                    let target_end = written.checked_add(count).ok_or_else(budget)?;
                    let source = source.get(start..end).ok_or_else(|| {
                        invalid("图片码流范围无效", "Invalid image bitstream range")
                    })?;
                    let target = dst.get_mut(written..target_end).ok_or_else(|| {
                        invalid("图片码流范围无效", "Invalid image bitstream range")
                    })?;
                    copy_chunks(source, target, &mut checkpoint)?;
                    written = target_end;
                    skip = 0;
                }
                if written != self.length {
                    return Err(invalid("图片码流范围无效", "Invalid image bitstream range"));
                }
            }
        }
        checkpoint()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SampleModel {
    Gray,
    Rgb,
    Ycc,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cicp {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CodedColor {
    pub model: SampleModel,
    /// 码流语法读出或推导的值，不表示容器声明或已完成颜色转换。
    pub cicp: Option<Cicp>,
}

#[cfg(test)]
impl CodedColor {
    pub const UNKNOWN: Self = Self {
        model: SampleModel::Unknown,
        cicp: None,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CodedInfo {
    pub(super) coded_dimensions: (u32, u32),
    pub(super) display_dimensions: (u32, u32),
    /// 显示窗口在编码画布内的左上角；不代表EXIF显示方向。
    pub(super) display_origin: (u32, u32),
    pub(super) bits_per_channel: u8,
    /// 码流声明的picture存储需求（含当前），不代表系统实际分配的缓冲数。
    pub(super) picture_buffers: u8,
    pub color: CodedColor,
}

impl CodedInfo {
    pub(super) fn picture_storage_bytes(self) -> Result<u64, PlatformError> {
        let pixels = checked_pixels(self.coded_dimensions, super::MAX_BYTES)?;
        checked_pixels(self.display_dimensions, super::MAX_BYTES)?;
        if !matches!(self.bits_per_channel, 8 | 10 | 12 | 16)
            || self.picture_buffers == 0
            || self.picture_buffers > 16
        {
            return Err(unsupported(
                "图片码流精度或缓冲声明暂不支持",
                "Unsupported image bit depth or picture-buffer declaration",
            ));
        }
        // 8位按RGBA覆盖；高位深按16位RGBA覆盖色度平面与边缘补齐。
        let bytes_per_pixel = if self.bits_per_channel > 8 { 8 } else { 4 };
        pixels
            .checked_mul(bytes_per_pixel)
            .and_then(|bytes| bytes.checked_mul(u64::from(self.picture_buffers)))
            .filter(|bytes| *bytes <= super::MAX_BYTES)
            .ok_or_else(budget)
    }
}

pub(super) fn checked_pixels(
    (width, height): (u32, u32),
    max_output_bytes: u64,
) -> Result<u64, PlatformError> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > super::MAX_SIDE
        || height > super::MAX_SIDE
        || pixels > super::MAX_PIXELS
        || pixels
            .checked_mul(4)
            .is_none_or(|bytes| bytes > max_output_bytes)
    {
        return Err(budget());
    }
    Ok(pixels)
}

pub(super) fn inspect_av1(
    config: &[u8],
    data: ByteView<'_>,
    declared: (u32, u32),
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<CodedInfo, PlatformError> {
    let info = av1::inspect(config, data, declared, max_output_bytes, cancel)?;
    checked_pixels(info.coded_dimensions, super::MAX_BYTES)?;
    checked_pixels(info.display_dimensions, max_output_bytes)?;
    info.picture_storage_bytes()?;
    Ok(info)
}

pub(super) fn inspect_hevc(
    config: &[u8],
    data: ByteView<'_>,
    declared: (u32, u32),
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<CodedInfo, PlatformError> {
    let info = hevc::inspect(config, data, declared, max_output_bytes, cancel)?;
    checked_pixels(info.coded_dimensions, super::MAX_BYTES)?;
    checked_pixels(info.display_dimensions, max_output_bytes)?;
    info.picture_storage_bytes()?;
    Ok(info)
}

pub(super) fn invalid(zh: &'static str, en: &'static str) -> PlatformError {
    PlatformError::Invalid {
        hresult: None,
        message: crate::i18n::tr(zh, en).into(),
    }
}

pub(super) fn unsupported(zh: &'static str, en: &'static str) -> PlatformError {
    PlatformError::Unsupported {
        hresult: None,
        message: crate::i18n::tr(zh, en).into(),
    }
}

pub(super) fn budget() -> PlatformError {
    PlatformError::Budget(
        tr!(
            "图片编码画布或缓冲声明超过预检预算",
            "Image coded canvas or picture buffers exceed the preflight budget"
        )
        .into(),
    )
}

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<(), PlatformError> {
    super::cancelled(cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_platform_coded_actual_ten_bit_primary_header_is_distinct_from_container_support() {
        // 原文件另含HDR辅助图；此处只查已由目录确定的主图配置和有序码流范围。
        // 不解码像素，不据主图头通过宣称支持整个辅助图容器或HDR映射。
        let source = include_bytes!("../tests/data/image_platform/colors_hdr_rec2020.avif");
        let config = source.get(436..440).unwrap();
        assert_eq!(config, [0x81, 0x20, 0x40, 0]);
        let extents = [687..6065];
        let data = ByteView::from_extents(source, &extents).unwrap();
        let info = inspect_av1(
            config,
            data,
            (200, 200),
            200 * 200 * 4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(info.coded_dimensions, (200, 200));
        assert_eq!(info.display_dimensions, (200, 200));
        assert_eq!(info.display_origin, (0, 0));
        assert_eq!(info.bits_per_channel, 10);
        assert_eq!(info.picture_buffers, 1);
        assert_eq!(
            info.color,
            CodedColor {
                model: SampleModel::Ycc,
                cicp: Some(Cicp {
                    primaries: 9,
                    transfer: 16,
                    matrix: 9,
                    full_range: true,
                }),
            }
        );
        assert_eq!(info.picture_storage_bytes().unwrap(), 200 * 200 * 8);
    }

    #[test]
    fn image_platform_coded_views_keep_logical_order_across_disjoint_and_repeated_extents() {
        let source = [0, 1, 2, 3, 4, 5, 6, 7];
        let extents = [4..6, 0..3, 4..6];
        let view = ByteView::from_extents(&source, &extents).unwrap();
        assert_eq!(view.len(), 7);
        assert_eq!(
            (0..view.len())
                .map(|index| view.byte(index).unwrap())
                .collect::<Vec<_>>(),
            [4, 5, 0, 1, 2, 4, 5]
        );
        let nested = view.slice(1..6).unwrap().slice(1..4).unwrap();
        assert_eq!(
            (0..nested.len())
                .map(|index| nested.byte(index).unwrap())
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(view.byte(7).is_err());
        assert!(view.slice(7..8).is_err());
        assert_eq!(view.slice(7..7).unwrap().len(), 0);
        assert!(ByteView::from_slice(&source).slice(0..9).is_err());
    }

    #[test]
    fn image_platform_coded_views_reject_invalid_ranges_before_reading() {
        assert!(ByteView::from_extents(&[0; 3], &[]).is_err());
        for ranges in [vec![0..0], vec![3..1], vec![0..4], vec![0..1; 65]] {
            assert!(ByteView::from_extents(&[0; 3], &ranges).is_err());
        }
        assert!(ByteView::from_slice(&[]).byte(0).is_err());
    }

    #[test]
    fn image_platform_coded_copy_keeps_nested_extent_order_and_requires_exact_destination() {
        let source = [0, 1, 2, 3, 4, 5, 6, 7];
        let extents = [4..6, 0..3, 4..6];
        let view = ByteView::from_extents(&source, &extents).unwrap();
        let cancel = AtomicBool::new(false);
        let mut all = [0; 7];
        view.copy_to(&mut all, &cancel).unwrap();
        assert_eq!(all, [4, 5, 0, 1, 2, 4, 5]);

        let mut nested = [0; 4];
        view.slice(1..7)
            .unwrap()
            .slice(1..5)
            .unwrap()
            .copy_to(&mut nested, &cancel)
            .unwrap();
        assert_eq!(nested, [0, 1, 2, 4]);
        let mut contiguous = [0; 3];
        ByteView::from_slice(&source)
            .slice(1..7)
            .unwrap()
            .slice(2..5)
            .unwrap()
            .copy_to(&mut contiguous, &cancel)
            .unwrap();
        assert_eq!(contiguous, [3, 4, 5]);
        view.slice(7..7).unwrap().copy_to(&mut [], &cancel).unwrap();
        ByteView::from_slice(&[]).copy_to(&mut [], &cancel).unwrap();
        for mut wrong in [vec![0xee; 6], vec![0xee; 8]] {
            assert!(matches!(
                view.copy_to(&mut wrong, &cancel),
                Err(PlatformError::Invalid { .. })
            ));
            assert!(wrong.iter().all(|byte| *byte == 0xee));
        }
    }

    #[test]
    fn image_platform_coded_copy_checks_cancellation_per_chunk_and_across_extents() {
        use std::sync::atomic::Ordering;

        const CHUNK: usize = 64 * 1024;
        let mut source = vec![0x22; CHUNK * 3 + 5];
        source[CHUNK * 2..].fill(0x11);
        let extents = [CHUNK * 2..CHUNK * 3 + 5, 0..CHUNK];
        let view = ByteView::from_extents(&source, &extents).unwrap();
        // 第三次检查在首extent的第二块前；第四次在下一extent首块前。
        for (cancel_at, copied) in [(3, CHUNK), (4, CHUNK + 5)] {
            let cancel = AtomicBool::new(false);
            let mut checks = 0;
            let mut dst = vec![0xee; view.len()];
            let result = view.copy_to_checked(&mut dst, || {
                checks += 1;
                if checks == cancel_at {
                    cancel.store(true, Ordering::Relaxed);
                }
                check_cancel(&cancel)
            });
            assert!(matches!(result, Err(PlatformError::Cancelled)));
            assert_eq!(checks, cancel_at);
            assert!(dst[..copied].iter().all(|byte| *byte == 0x11));
            assert!(dst[copied..].iter().all(|byte| *byte == 0xee));
        }
        let cancel = AtomicBool::new(true);
        let mut wrong = [0xee];
        assert!(matches!(
            view.copy_to(&mut wrong, &cancel),
            Err(PlatformError::Cancelled)
        ));
        assert_eq!(wrong, [0xee]);
    }

    #[test]
    fn image_platform_coded_picture_model_uses_coded_canvas_depth_and_dpb() {
        let info = CodedInfo {
            coded_dimensions: (256, 128),
            display_dimensions: (250, 128),
            display_origin: (6, 0),
            bits_per_channel: 10,
            picture_buffers: 5,
            color: CodedColor::UNKNOWN,
        };
        assert_eq!(info.picture_storage_bytes().unwrap(), 256 * 128 * 8 * 5);
        assert_eq!(
            CodedInfo {
                bits_per_channel: 8,
                ..info
            }
            .picture_storage_bytes()
            .unwrap(),
            256 * 128 * 4 * 5
        );
        for info in [
            CodedInfo {
                picture_buffers: 0,
                ..info
            },
            CodedInfo {
                picture_buffers: 17,
                ..info
            },
            CodedInfo {
                bits_per_channel: 9,
                ..info
            },
            CodedInfo {
                coded_dimensions: (8000, 4000),
                ..info
            },
        ] {
            assert!(info.picture_storage_bytes().is_err());
        }
        assert!(checked_pixels((3, 2), 23).is_err());
        assert_eq!(checked_pixels((3, 2), 24).unwrap(), 6);
    }
}
