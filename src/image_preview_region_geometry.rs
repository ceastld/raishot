//! 局部采样预览的独立几何：矩形边界、完整源像素与局部栅格索引分别处理。
use crate::{geometry::Rect, image_preview::PreviewGeometry, Result};
use image::metadata::Orientation;

/// 只描述ROI采样；不能作为全幅总览、完整像素或编辑导出的资格凭据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegionGeometry {
    // 仅复用完整源尺寸、方向事实与栅格尺寸校验，不调用全幅采样映射。
    facts: PreviewGeometry,
    encoded_region: Rect,
    normalized_source_region: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegionSample {
    pub raster_pixel: (u32, u32),
    pub normalized_source_pixel: (u32, u32),
    pub encoded_source_pixel: (u32, u32),
}

impl RegionGeometry {
    pub(crate) fn new(
        encoded_dimensions: (u32, u32),
        normalized_source_region: Rect,
        sampled_encoded_dimensions: (u32, u32),
        orientation: Option<Orientation>,
    ) -> Result<Self> {
        let facts =
            PreviewGeometry::new(encoded_dimensions, sampled_encoded_dimensions, orientation)?;
        let (width, height) = facts.normalized_source_dimensions();
        checked_region(normalized_source_region, (width, height))?;
        let encoded_region = inverse_rect(
            encoded_dimensions,
            normalized_source_region,
            orientation.unwrap_or(Orientation::NoTransforms).to_exif(),
        )?;
        checked_region(encoded_region, encoded_dimensions)?;
        if sampled_encoded_dimensions.0 > encoded_region.width
            || sampled_encoded_dimensions.1 > encoded_region.height
        {
            return Err(invalid());
        }
        Ok(Self {
            facts,
            encoded_region,
            normalized_source_region,
        })
    }

    pub(crate) fn encoded_dimensions(self) -> (u32, u32) {
        self.facts.encoded_dimensions()
    }

    pub(crate) fn normalized_source_dimensions(self) -> (u32, u32) {
        self.facts.normalized_source_dimensions()
    }

    pub(crate) fn encoded_region(self) -> Rect {
        self.encoded_region
    }

    pub(crate) fn normalized_source_region(self) -> Rect {
        self.normalized_source_region
    }

    pub(crate) fn sampled_encoded_dimensions(self) -> (u32, u32) {
        self.facts.sampled_encoded_dimensions()
    }

    pub(crate) fn raster_dimensions(self) -> (u32, u32) {
        self.facts.raster_dimensions()
    }

    /// None表示来源未声明方向，计算时按无变换处理，不补造已确认方向事实。
    pub(crate) fn source_orientation(self) -> Option<Orientation> {
        self.facts.source_orientation()
    }

    /// 输入为完整归正源的边界坐标；ROI外不钳制到纹理边缘。
    pub(crate) fn sample_at_source(self, x: f64, y: f64) -> Option<RegionSample> {
        let region = self.normalized_source_region;
        let right = region.x.checked_add(region.width)?;
        let bottom = region.y.checked_add(region.height)?;
        if !x.is_finite()
            || !y.is_finite()
            || x < f64::from(region.x)
            || y < f64::from(region.y)
            || x >= f64::from(right)
            || y >= f64::from(bottom)
        {
            return None;
        }
        let (width, height) = self.raster_dimensions();
        let raster_pixel = (
            (((x - f64::from(region.x)) * f64::from(width) / f64::from(region.width)).floor()
                as u32)
                .min(width - 1),
            (((y - f64::from(region.y)) * f64::from(height) / f64::from(region.height)).floor()
                as u32)
                .min(height - 1),
        );
        let (sample_x, sample_y) = self.sampled_encoded_at_raster(raster_pixel);
        let encoded_region = self.encoded_region;
        let (sample_width, sample_height) = self.sampled_encoded_dimensions();
        // 在编码矩形中取中心并加原点，再按完整源方向归正。反射后的非整除
        // 采样中心不能用归正ROI重新等分代替，否则可能偏移一个原像素。
        let encoded_source_pixel = (
            encoded_region
                .x
                .checked_add(center(sample_x, encoded_region.width, sample_width)?)?,
            encoded_region.y.checked_add(center(
                sample_y,
                encoded_region.height,
                sample_height,
            )?)?,
        );
        Some(RegionSample {
            raster_pixel,
            normalized_source_pixel: self.normalized_source_pixel(encoded_source_pixel),
            encoded_source_pixel,
        })
    }

    /// producer写入局部采样栅格：x/y是编码ROI的采样索引，不是全图坐标。
    pub(crate) fn raster_at_sampled_encoded(self, x: u32, y: u32) -> Option<(u32, u32)> {
        let (width, height) = self.sampled_encoded_dimensions();
        if x >= width || y >= height {
            return None;
        }
        Some(match self.exif() {
            2 => (width - 1 - x, y),
            3 => (width - 1 - x, height - 1 - y),
            4 => (x, height - 1 - y),
            5 => (y, x),
            6 => (height - 1 - y, x),
            7 => (height - 1 - y, width - 1 - x),
            8 => (y, width - 1 - x),
            _ => (x, y),
        })
    }

    fn exif(self) -> u8 {
        self.source_orientation()
            .unwrap_or(Orientation::NoTransforms)
            .to_exif()
    }

    fn sampled_encoded_at_raster(self, (x, y): (u32, u32)) -> (u32, u32) {
        let (width, height) = self.sampled_encoded_dimensions();
        match self.exif() {
            2 => (width - 1 - x, y),
            3 => (width - 1 - x, height - 1 - y),
            4 => (x, height - 1 - y),
            5 => (y, x),
            6 => (y, height - 1 - x),
            7 => (width - 1 - y, height - 1 - x),
            8 => (width - 1 - y, x),
            _ => (x, y),
        }
    }

    fn normalized_source_pixel(self, (x, y): (u32, u32)) -> (u32, u32) {
        let (width, height) = self.encoded_dimensions();
        match self.exif() {
            2 => (width - 1 - x, y),
            3 => (width - 1 - x, height - 1 - y),
            4 => (x, height - 1 - y),
            5 => (y, x),
            6 => (height - 1 - y, x),
            7 => (height - 1 - y, width - 1 - x),
            8 => (y, width - 1 - x),
            _ => (x, y),
        }
    }
}

fn checked_region(region: Rect, dimensions: (u32, u32)) -> Result<()> {
    if region.width == 0
        || region.height == 0
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|right| right > dimensions.0)
        || region
            .y
            .checked_add(region.height)
            .is_none_or(|bottom| bottom > dimensions.1)
    {
        return Err(invalid());
    }
    Ok(())
}

fn inverse_rect((width, height): (u32, u32), region: Rect, exif: u8) -> Result<Rect> {
    let right = region.x.checked_add(region.width).ok_or_else(invalid)?;
    let bottom = region.y.checked_add(region.height).ok_or_else(invalid)?;
    let (x, y, w, h) = match exif {
        2 => (
            width.checked_sub(right).ok_or_else(invalid)?,
            region.y,
            region.width,
            region.height,
        ),
        3 => (
            width.checked_sub(right).ok_or_else(invalid)?,
            height.checked_sub(bottom).ok_or_else(invalid)?,
            region.width,
            region.height,
        ),
        4 => (
            region.x,
            height.checked_sub(bottom).ok_or_else(invalid)?,
            region.width,
            region.height,
        ),
        5 => (region.y, region.x, region.height, region.width),
        6 => (
            region.y,
            height.checked_sub(right).ok_or_else(invalid)?,
            region.height,
            region.width,
        ),
        7 => (
            width.checked_sub(bottom).ok_or_else(invalid)?,
            height.checked_sub(right).ok_or_else(invalid)?,
            region.height,
            region.width,
        ),
        8 => (
            width.checked_sub(bottom).ok_or_else(invalid)?,
            region.x,
            region.height,
            region.width,
        ),
        _ => (region.x, region.y, region.width, region.height),
    };
    Ok(Rect {
        x,
        y,
        width: w,
        height: h,
    })
}

fn center(index: u32, source: u32, samples: u32) -> Option<u32> {
    let numerator = u64::from(index)
        .checked_mul(2)?
        .checked_add(1)?
        .checked_mul(u64::from(source))?;
    let denominator = u64::from(samples).checked_mul(2)?;
    u32::try_from(numerator.checked_div(denominator)?).ok()
}

fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "局部预览范围为空、越界或采样尺寸超过范围",
        "Preview region is empty, out of bounds, or smaller than its samples"
    )
    .into()
}

#[cfg(test)]
#[path = "image_preview_region_geometry_tests.rs"]
mod tests;
