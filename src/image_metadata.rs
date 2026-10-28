//! 读入时的来源事实；实际显示大小与来源尺寸、方向分别记录。
use crate::image_color_management::ManagedMetadata;
use image::{metadata::Orientation, ImageFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// 来源格式与当前显示像素、导出编码分别记录。
pub(crate) enum SourceFormat {
    Raster(ImageFormat),
    Svg,
    Heif,
    Psd,
}

impl From<ImageFormat> for SourceFormat {
    fn from(format: ImageFormat) -> Self {
        Self::Raster(format)
    }
}

impl PartialEq<ImageFormat> for SourceFormat {
    fn eq(&self, other: &ImageFormat) -> bool {
        matches!(self, Self::Raster(format) if format == other)
    }
}

impl PartialEq<SourceFormat> for ImageFormat {
    fn eq(&self, other: &SourceFormat) -> bool {
        other == self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageMetadata {
    pub format: SourceFormat,
    /// 来源未提供可靠像素尺寸时为None，不以显示栅格尺寸代填。
    pub encoded_dimensions: Option<(u32, u32)>,
    pub display_dimensions: (u32, u32),
    /// 未检查或没有可靠来源方向时为None；不能等同于已确认无需变换。
    pub orientation: Option<Orientation>,
    pub encoded_bytes: Option<u64>,
    /// 解码器在统一转为8位显示前的输出类型，不等同文件中的通道布局或ICC信息。
    pub decoded_color: Option<image::ColorType>,
    /// 已预扫的容器证据与实际颜色处理状态；未检查的来源保留None。
    pub details: Option<ManagedMetadata>,
}

impl ImageMetadata {
    pub(crate) fn format_name(self) -> &'static str {
        self.format.format_name()
    }
}

impl SourceFormat {
    pub(crate) fn format_name(self) -> &'static str {
        let format = match self {
            Self::Svg => return "SVG",
            Self::Heif => return "HEIF",
            Self::Psd => return "PSD",
            Self::Raster(format) => format,
        };
        match format {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Bmp => "BMP",
            ImageFormat::Gif => "GIF",
            ImageFormat::WebP => "WebP",
            ImageFormat::Tiff => "TIFF",
            ImageFormat::Ico => "ICO",
            ImageFormat::Avif => "AVIF",
            ImageFormat::Pnm => "PNM",
            ImageFormat::Tga => "TGA",
            ImageFormat::Dds => "DDS",
            ImageFormat::Hdr => "HDR",
            ImageFormat::OpenExr => "OpenEXR",
            ImageFormat::Farbfeld => "Farbfeld",
            ImageFormat::Qoi => "QOI",
            _ => tr!("未知", "Unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_metadata_source_formats_keep_native_identity_and_raster_comparisons() {
        let png = SourceFormat::from(ImageFormat::Png);
        assert_eq!(png, ImageFormat::Png);
        assert_eq!(ImageFormat::Png, png);
        assert_ne!(SourceFormat::Svg, ImageFormat::Png);
        assert_ne!(ImageFormat::Png, SourceFormat::Heif);
        assert_ne!(SourceFormat::Svg, SourceFormat::Heif);
        assert_eq!(png.format_name(), "PNG");
        assert_eq!(SourceFormat::Svg.format_name(), "SVG");
        assert_eq!(SourceFormat::Heif.format_name(), "HEIF");
    }

    #[test]
    fn image_metadata_unknown_source_facts_do_not_replace_actual_display_dimensions() {
        let metadata = ImageMetadata {
            format: SourceFormat::Svg,
            encoded_dimensions: None,
            display_dimensions: (320, 180),
            orientation: None,
            encoded_bytes: Some(42),
            decoded_color: None,
            details: None,
        };
        let copied = metadata;
        assert_eq!(copied, metadata);
        assert_eq!(metadata.format_name(), "SVG");
        assert_eq!(metadata.encoded_dimensions, None);
        assert_eq!(metadata.orientation, None);
        assert_eq!(metadata.display_dimensions, (320, 180));
    }
}
