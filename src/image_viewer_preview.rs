//! 采样预览的呈现桥；没有完整帧快照、色管或像素编辑资格。
use super::*;
use crate::{
    image_color_management::{ColorConversion, ManagedMetadata},
    image_preview::{
        png::{PreviewColorStatus, PreviewDecoded, PreviewSampling},
        source::FrozenPreviewSource,
    },
};

pub(super) fn loaded(
    decoded: PreviewDecoded,
    encoded_bytes: u64,
    cancel: &AtomicBool,
    external_resident: u64,
) -> std::result::Result<LoadedImage, String> {
    loaded_with_source(decoded, encoded_bytes, None, cancel, external_resident)
}

/// external_resident排除本次source的整个retained_bytes；租约另加一次。
/// 预算不足时只有独占租约能安全释放并退为纯总览，不从任意R中扣来源容量。
pub(super) fn loaded_with_source(
    decoded: PreviewDecoded,
    encoded_bytes: u64,
    mut preview_source: Option<Arc<FrozenPreviewSource>>,
    cancel: &AtomicBool,
    external_resident: u64,
) -> std::result::Result<LoadedImage, String> {
    check_cancel(cancel)?;
    let result = (|| {
        let geometry = decoded.geometry;
        let pixel_bytes = u64::from(decoded.image.width)
            .checked_mul(u64::from(decoded.image.height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(invalid)?;
        if (decoded.image.width, decoded.image.height) != geometry.raster_dimensions()
            || decoded.image.bgra.len() as u64 != pixel_bytes
            || decoded.sampling != PreviewSampling::PixelCenterNearest
            || decoded.color != PreviewColorStatus::NotColorManaged
            || preview_source.as_ref().is_some_and(|source| {
                source.geometry() != geometry
                    || source.source() != decoded.source
                    || source.encoded_bytes() != encoded_bytes
                    || source.input_reserved_bytes() < encoded_bytes
            })
        {
            return Err(invalid());
        }
        let check_budget = |source_bytes: u64, warning: &String| {
            // 既有缩略缓存底额不能吸收另行保留的来源租约。
            let resident = external_resident
                .max(THUMBNAIL_CACHE_BYTES as u64)
                .checked_add(source_bytes)
                .ok_or_else(presentation_budget)?;
            super::check_presentation_budget_with_resident(
                geometry.raster_dimensions(),
                decoded.image.bgra.capacity() as u64,
                warning.capacity() as u64,
                false,
                resident,
            )
        };
        let mut warning = preview_warning(preview_source.is_some());
        let retained = preview_source
            .as_ref()
            .map_or(0, |source| source.retained_bytes());
        if let Err(error) = check_budget(retained, &warning) {
            let Some(source) = preview_source.take() else {
                return Err(error);
            };
            match Arc::try_unwrap(source) {
                Ok(owned) => drop(owned),
                Err(_shared) => return Err(error),
            }
            // 只有已真实释放的独占来源才能从本次相位账本中移除。
            drop(warning);
            warning = preview_warning(false);
            check_budget(0, &warning)?;
        }
        check_cancel(cancel)?;
        let thumbnail = thumbnail(&decoded.image)?;
        check_cancel(cancel)?;
        let dimensions = geometry.raster_dimensions();
        let image =
            crate::canvas_source::render_image(dimensions.0, dimensions.1, decoded.image.bgra);
        check_cancel(cancel)?;
        Ok(LoadedImage {
            metadata: ImageMetadata {
                format: image::ImageFormat::Png.into(),
                encoded_dimensions: Some(geometry.encoded_dimensions()),
                display_dimensions: dimensions,
                orientation: geometry.source_orientation(),
                encoded_bytes: Some(encoded_bytes),
                decoded_color: None,
                details: Some(ManagedMetadata {
                    source: decoded.source,
                    profile: None,
                    color_conversion: ColorConversion::SignalUnmapped,
                }),
            },
            preview_geometry: Some(geometry),
            preview_source,
            // 原始straight alpha只移动一次，棋盘由呈现层单独叠加。
            tiles: vec![(Rect::full(dimensions.0, dimensions.1), image)],
            thumbnail,
            raw_frame: None,
            raw_page: None,
            warning: Some(warning),
            metadata_source: None,
            rotation: Rotation::Identity,
        })
    })();
    check_cancel(cancel)?;
    result
}

fn check_cancel(cancel: &AtomicBool) -> std::result::Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(tr!("PNG预览已取消", "PNG preview canceled").into())
    } else {
        Ok(())
    }
}

fn invalid() -> String {
    tr!("图片预览事实不一致", "Image preview facts do not match").into()
}

fn presentation_budget() -> String {
    tr!(
        "图片显示超过总驻留预算",
        "Image presentation exceeds the resident budget"
    )
    .into()
}

fn preview_warning(detail_available: bool) -> String {
    if detail_available {
        tr!(
            "采样预览：未做颜色管理；编辑、OCR和复制图像不可用",
            "Sampled preview: color unmanaged; editing, OCR and image copy unavailable"
        )
        .into()
    } else {
        tr!(
            "采样预览：细节不可用，未做颜色管理；编辑、OCR和复制图像不可用",
            "Sampled preview: detail unavailable, color unmanaged; editing, OCR and image copy unavailable"
        )
        .into()
    }
}

pub(super) fn property_rows(
    rows: &mut Vec<(&'static str, String)>,
    geometry: crate::image_preview::PreviewGeometry,
) {
    let normalized = geometry.normalized_source_dimensions();
    let raster = geometry.raster_dimensions();
    for (label, value) in rows.iter_mut() {
        if *label == tr!("归一显示尺寸", "Normalized dimensions") {
            *value = format!("{} × {} px", normalized.0, normalized.1);
        } else if *label == tr!("另存元数据", "Saved metadata") {
            *value = tr!(
                "采样预览不能作为原图另存；源文件引用操作仍单独检查",
                "A sampled preview cannot be saved as the source; file-reference actions are checked separately"
            ).into();
        }
    }
    rows.push((
        tr!("预览栅格", "Preview raster"),
        format!("{} × {} px · BGRA8", raster.0, raster.1),
    ));
    rows.push((
        tr!("预览采样", "Preview sampling"),
        tr!(
            "像素中心最近邻；未做颜色管理，16位来源量化到8位",
            "Nearest pixel centers; color unmanaged, 16-bit sources quantized to 8-bit"
        )
        .into(),
    ));
    rows.push((
        tr!("预览操作范围", "Preview actions"),
        tr!(
            "可查看和缩放；编辑、OCR、像素复制、旋转、网格及取色暂不可用",
            "View and zoom; editing, OCR, pixel copy, rotation, grid and color sampling are unavailable"
        ).into(),
    ));
}

#[cfg(test)]
#[path = "image_viewer_preview_tests.rs"]
mod tests;
