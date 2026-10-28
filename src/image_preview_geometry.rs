//! 受限采样预览的几何事实；映射结果不授予原图编辑或导出资格。
use crate::Result;
use image::metadata::Orientation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewGeometry {
    encoded_dimensions: (u32, u32),
    sampled_encoded_dimensions: (u32, u32),
    source_orientation: Option<Orientation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewSample {
    pub raster_pixel: (u32, u32),
    pub normalized_source_pixel: (u32, u32),
    pub encoded_source_pixel: (u32, u32),
}

impl PreviewGeometry {
    pub(crate) fn new(
        encoded_dimensions: (u32, u32),
        sampled_encoded_dimensions: (u32, u32),
        source_orientation: Option<Orientation>,
    ) -> Result<Self> {
        let (source_width, source_height) = encoded_dimensions;
        let (sample_width, sample_height) = sampled_encoded_dimensions;
        if source_width == 0
            || source_height == 0
            || sample_width == 0
            || sample_height == 0
            || sample_width > source_width
            || sample_height > source_height
        {
            return Err(tr!(
                "预览尺寸为空或超过原图尺寸",
                "Preview dimensions are empty or exceed the source"
            )
            .into());
        }
        if sample_width > crate::export::MAX_DECODE_SIDE
            || sample_height > crate::export::MAX_DECODE_SIDE
            || u64::from(sample_width) * u64::from(sample_height)
                > crate::image_decode::MAX_DECODE_PIXELS
        {
            return Err(tr!(
                "预览像素超过解码上限",
                "Preview pixels exceed the decode limit"
            )
            .into());
        }
        Ok(Self {
            encoded_dimensions,
            sampled_encoded_dimensions,
            source_orientation,
        })
    }

    pub(crate) fn encoded_dimensions(self) -> (u32, u32) {
        self.encoded_dimensions
    }

    pub(crate) fn normalized_source_dimensions(self) -> (u32, u32) {
        self.normalized_dimensions(self.encoded_dimensions)
    }

    pub(crate) fn sampled_encoded_dimensions(self) -> (u32, u32) {
        self.sampled_encoded_dimensions
    }

    pub(crate) fn raster_dimensions(self) -> (u32, u32) {
        self.normalized_dimensions(self.sampled_encoded_dimensions)
    }

    pub(crate) fn source_orientation(self) -> Option<Orientation> {
        self.source_orientation
    }

    /// 输入为归正后的原图坐标，返回预览中实际存在的那个采样像素。
    pub(crate) fn sample_at_source(self, x: f64, y: f64) -> Option<PreviewSample> {
        let (source_width, source_height) = self.normalized_source_dimensions();
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.
            || y < 0.
            || x >= f64::from(source_width)
            || y >= f64::from(source_height)
        {
            return None;
        }
        let (raster_width, raster_height) = self.raster_dimensions();
        let raster_pixel = (
            ((x * f64::from(raster_width) / f64::from(source_width)).floor() as u32)
                .min(raster_width - 1),
            ((y * f64::from(raster_height) / f64::from(source_height)).floor() as u32)
                .min(raster_height - 1),
        );
        // 先回到编码方向再取中心；反射后的整数中心不能在归正域重新采样。
        let encoded_raster = self.encoded_pixel(raster_pixel);
        let encoded_source_pixel = (
            sampled_center(
                encoded_raster.0,
                self.encoded_dimensions.0,
                self.sampled_encoded_dimensions.0,
            )?,
            sampled_center(
                encoded_raster.1,
                self.encoded_dimensions.1,
                self.sampled_encoded_dimensions.1,
            )?,
        );
        Some(PreviewSample {
            raster_pixel,
            normalized_source_pixel: self.normalized_pixel(encoded_source_pixel),
            encoded_source_pixel,
        })
    }

    fn exif(self) -> u8 {
        self.source_orientation
            .unwrap_or(Orientation::NoTransforms)
            .to_exif()
    }

    fn normalized_dimensions(self, dimensions: (u32, u32)) -> (u32, u32) {
        if self.exif() >= 5 {
            (dimensions.1, dimensions.0)
        } else {
            dimensions
        }
    }

    fn encoded_pixel(self, (x, y): (u32, u32)) -> (u32, u32) {
        let (width, height) = self.sampled_encoded_dimensions;
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

    fn normalized_pixel(self, (x, y): (u32, u32)) -> (u32, u32) {
        let (width, height) = self.encoded_dimensions;
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

fn sampled_center(index: u32, source: u32, samples: u32) -> Option<u32> {
    let numerator = u64::from(index)
        .checked_mul(2)?
        .checked_add(1)?
        .checked_mul(u64::from(source))?;
    let denominator = u64::from(samples).checked_mul(2)?;
    u32::try_from(numerator / denominator).ok()
}

#[cfg(test)]
#[path = "image_preview_geometry_tests.rs"]
mod tests;
