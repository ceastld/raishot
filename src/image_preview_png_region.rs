//! 已验证冻结PNG的局部行采样；ROI不缩减完整源行的解码工作区。
use super::*;
use crate::{
    geometry::Rect,
    image_preview::{region_geometry::RegionGeometry, source::FrozenPreviewSource},
};

const MAX_REGION_PIXELS: u64 = 4_000_000;

pub(crate) struct RegionDecoded {
    pub image: Image,
    pub geometry: RegionGeometry,
    pub source: Summary,
    pub sampling: PreviewSampling,
    pub color: PreviewColorStatus,
}

/// 借用冻结来源创建的新视图，不拥有第二份压缩输入或元数据导出sidecar。
pub(crate) struct RegionPlan<'a> {
    view: SegmentedView<'a>,
    header: Header,
    geometry: RegionGeometry,
    source: Summary,
    workspace: PreviewWorkspace,
    source_retained_bytes: u64,
    external_other_resident: u64,
}

impl RegionPlan<'_> {
    pub(crate) fn geometry(&self) -> RegionGeometry {
        self.geometry
    }

    pub(crate) fn source(&self) -> Summary {
        self.source
    }

    pub(crate) fn workspace(&self) -> PreviewWorkspace {
        self.workspace
    }
}

/// R排除当前source的整个retained_bytes，包含旧总览/详情/退役纹理等其它驻留。
pub(crate) fn inspect_region<'a>(
    source: &'a FrozenPreviewSource,
    normalized_region: Rect,
    limits: PreviewLimits,
    external_other_resident: u64,
    cancel: &AtomicBool,
) -> Result<RegionPlan<'a>> {
    check_cancel(cancel)?;
    let result = inspect_inner(
        source,
        normalized_region,
        limits,
        external_other_resident,
        cancel,
    );
    check_cancel(cancel)?;
    result
}

fn inspect_inner<'a>(
    source: &'a FrozenPreviewSource,
    normalized_region: Rect,
    mut limits: PreviewLimits,
    external_other_resident: u64,
    cancel: &AtomicBool,
) -> Result<RegionPlan<'a>> {
    let bytes = source.input();
    let source_retained_bytes = source.retained_bytes();
    if source.input_reserved_bytes() < bytes.len() as u64
        || source_retained_bytes < source.input_reserved_bytes()
    {
        return Err(budget());
    }
    // retained已经含来源Veccap和控制槽，不能再叠加input_reserved_bytes。
    ledger(external_other_resident, source_retained_bytes, 0)?;
    let header = scan(bytes, cancel)?;
    let source_geometry = source.geometry();
    let expected_source = source.source();
    if header.dimensions != source_geometry.encoded_dimensions()
        || expected_source.orientation != source_geometry.source_orientation()
    {
        return Err(invalid());
    }
    let crop = RegionGeometry::new(
        header.dimensions,
        normalized_region,
        (1, 1),
        source_geometry.source_orientation(),
    )?
    .encoded_region();
    limits.max_pixels = limits.max_pixels.min(MAX_REGION_PIXELS);
    let sampled = sampled_dimensions((crop.width, crop.height), limits)?;
    let geometry = RegionGeometry::new(
        header.dimensions,
        normalized_region,
        sampled,
        source_geometry.source_orientation(),
    )?;
    let base_decode = add(&[
        private_workspace(header.raw_row)?,
        INPUT_BUFFER as u64,
        header.expanded_row as u64,
        pixel_bytes(geometry.raster_dimensions())?,
    ])?;
    ledger(
        external_other_resident,
        source_retained_bytes,
        METADATA_WORKSPACE.max(base_decode),
    )?;
    let inspected = crate::image_container_metadata::inspect_cancellable(bytes, cancel)?;
    check_cancel(cancel)?;
    let crate::image_container_metadata::InspectedImage {
        summary,
        icc_profile,
        supported_exif,
        view,
    } = inspected;
    drop(icc_profile);
    drop(supported_exif);
    // 同一冻结来源的完整尺寸、方向和元数据事实不可因ROI请求改变。
    if summary != expected_source || summary.orientation != geometry.source_orientation() {
        return Err(invalid());
    }
    let workspace = PreviewWorkspace {
        metadata_bytes: METADATA_WORKSPACE,
        decode_bytes: add(&[base_decode, view.retained_bytes()?])?,
    };
    ledger(
        external_other_resident,
        source_retained_bytes,
        workspace.peak_bytes(),
    )?;
    Ok(RegionPlan {
        view,
        header,
        geometry,
        source: summary,
        workspace,
        source_retained_bytes,
        external_other_resident,
    })
}

/// 首版仍顺序解压全高并finish校验尾部；ROI不是压缩流随机访问。
pub(crate) fn decode_region(plan: RegionPlan<'_>, cancel: &AtomicBool) -> Result<RegionDecoded> {
    check_cancel(cancel)?;
    let result = decode_inner(plan, cancel);
    check_cancel(cancel)?;
    result
}

fn decode_inner(plan: RegionPlan<'_>, cancel: &AtomicBool) -> Result<RegionDecoded> {
    let RegionPlan {
        view,
        header,
        geometry,
        source,
        workspace,
        source_retained_bytes,
        external_other_resident,
    } = plan;
    ledger(
        external_other_resident,
        source_retained_bytes,
        workspace.decode_bytes,
    )?;
    let input = BufReader::with_capacity(
        INPUT_BUFFER,
        CancelReader {
            inner: view,
            cancel,
        },
    );
    let mut decoder = png::Decoder::new(input);
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    decoder.ignore_checksums(false);
    decoder.set_limits(png::Limits {
        bytes: usize::try_from(private_workspace(header.raw_row)?)?,
    });
    let mut reader = decoder.read_info()?;
    check_cancel(cancel)?;
    if reader.info().size() != header.dimensions || reader.info().interlaced {
        return Err(invalid());
    }
    let (color, depth) = reader.output_color_type();
    let expected_depth = if header.depth == 16 {
        png::BitDepth::Sixteen
    } else {
        png::BitDepth::Eight
    };
    if color != expanded_color(header)
        || depth != expected_depth
        || reader.output_line_size(header.dimensions.0) != Some(header.expanded_row)
    {
        return Err(invalid());
    }
    let mut row = slot(header.expanded_row, cancel)?;
    let mut bgra = slot(
        usize::try_from(pixel_bytes(geometry.raster_dimensions())?)?,
        cancel,
    )?;
    let (sample_width, sample_height) = geometry.sampled_encoded_dimensions();
    let crop = geometry.encoded_region();
    let mut target_y = 0;
    let mut selected_y = crop
        .y
        .checked_add(center(0, crop.height, sample_height))
        .ok_or_else(invalid)?;
    for source_y in 0..header.dimensions.1 {
        check_cancel(cancel)?;
        if reader.read_row(&mut row)?.is_none() {
            return Err(invalid());
        }
        check_cancel(cancel)?;
        if target_y == sample_height || source_y != selected_y {
            continue;
        }
        for target_x in 0..sample_width {
            if target_x % 1024 == 0 {
                check_cancel(cancel)?;
            }
            let source_x = crop
                .x
                .checked_add(center(target_x, crop.width, sample_width))
                .ok_or_else(invalid)?;
            let pixel = sample(&row, source_x, color, depth)?;
            let (x, y) = geometry
                .raster_at_sampled_encoded(target_x, target_y)
                .ok_or_else(invalid)?;
            let start = usize::try_from(
                (u64::from(y) * u64::from(geometry.raster_dimensions().0) + u64::from(x))
                    .checked_mul(4)
                    .ok_or_else(budget)?,
            )?;
            bgra.get_mut(start..start + 4)
                .ok_or_else(invalid)?
                .copy_from_slice(&pixel);
        }
        target_y += 1;
        if target_y < sample_height {
            selected_y = crop
                .y
                .checked_add(center(target_y, crop.height, sample_height))
                .ok_or_else(invalid)?;
        }
    }
    if target_y != sample_height || reader.read_row(&mut row)?.is_some() {
        return Err(invalid());
    }
    reader.finish()?;
    check_cancel(cancel)?;
    drop(reader);
    drop(row);
    let (width, height) = geometry.raster_dimensions();
    Ok(RegionDecoded {
        image: Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        },
        geometry,
        source,
        sampling: PreviewSampling::PixelCenterNearest,
        color: PreviewColorStatus::NotColorManaged,
    })
}

#[cfg(test)]
#[path = "image_preview_png_region_tests.rs"]
mod tests;
