//! 图片属性只读取已载入的快照，源文件删除后也不重新打开或探测。
use super::{frame::ViewerFrame, pages};
use crate::{
    image_animation_playback::{PlaybackSnapshot, PlaybackState},
    image_container_metadata::{ColorModel, FieldSummary, MetadataStatus},
    image_export_metadata::SourceMetadata,
    image_metadata::ImageMetadata,
};
use std::path::Path;

pub(super) fn rows(
    path: Option<&Path>,
    metadata: Option<ImageMetadata>,
    animation: Option<&PlaybackSnapshot>,
    frame: Option<&dyn ViewerFrame>,
    header_format: Option<&'static str>,
    metadata_source: Option<&SourceMetadata>,
) -> Vec<(&'static str, String)> {
    let unknown = || tr!("未知", "Unknown").to_string();
    let dimensions = |value: (u32, u32)| format!("{} × {} px", value.0, value.1);
    let mut rows = vec![
        (
            tr!("路径", "Path"),
            path.map_or_else(unknown, |path| path.to_string_lossy().into_owned()),
        ),
        (
            tr!("文件头格式", "Header format"),
            header_format.map_or_else(
                || metadata.map_or_else(unknown, |metadata| metadata.format_name().into()),
                str::to_owned,
            ),
        ),
        (
            tr!("编码尺寸", "Encoded dimensions"),
            metadata
                .and_then(|metadata| metadata.encoded_dimensions)
                .map_or_else(unknown, dimensions),
        ),
        (
            tr!("归一显示尺寸", "Normalized dimensions"),
            metadata.map_or_else(unknown, |metadata| dimensions(metadata.display_dimensions)),
        ),
        (
            tr!("来源体积", "Source size"),
            metadata
                .and_then(|metadata| metadata.encoded_bytes)
                .map_or_else(unknown, file_size),
        ),
        (
            tr!("来源方向（归一）", "Source orientation (normalized)"),
            metadata
                .and_then(|metadata| metadata.orientation)
                .map_or_else(unknown, |value| orientation(value.to_exif())),
        ),
        (
            tr!("转换前的解码像素", "Decoded input pixels"),
            metadata
                .and_then(|metadata| metadata.decoded_color)
                .map_or_else(unknown, |color| format!("{color:?}")),
        ),
        (
            tr!("解码显示像素", "Decoded pixels"),
            metadata.map_or_else(unknown, |_| {
                tr!("BGRA8（每通道8位）", "BGRA8 (8-bit channels)").into()
            }),
        ),
        (
            tr!("源位深", "Source bit depth"),
            metadata
                .and_then(|value| value.details)
                .and_then(|details| details.source.bits_per_channel)
                .map_or_else(unknown, |bits| {
                    trf!("每通道{bits}位", "{bits}-bit channels")
                }),
        ),
        (
            tr!("ICC读取", "ICC read status"),
            field_summary(
                metadata
                    .and_then(|value| value.details)
                    .map(|details| details.source.icc),
            ),
        ),
        (
            tr!("EXIF读取", "EXIF read status"),
            field_summary(
                metadata
                    .and_then(|value| value.details)
                    .map(|details| details.source.exif),
            ),
        ),
        (tr!("总帧数", "Total frames"), unknown()),
        (tr!("总页数", "Total pages"), unknown()),
    ];
    if metadata
        .and_then(|metadata| metadata.decoded_color)
        .is_some_and(|color| {
            matches!(
                color,
                image::ColorType::L16
                    | image::ColorType::La16
                    | image::ColorType::Rgb16
                    | image::ColorType::Rgba16
                    | image::ColorType::Rgb32F
                    | image::ColorType::Rgba32F
            )
        })
    {
        rows.push((
            tr!("精度转换", "Precision conversion"),
            if metadata.and_then(|metadata| metadata.details).is_some_and(|details| details.color_conversion == crate::image_color_management::ColorConversion::ConvertedSrgb16) {
                tr!(
                    "保留16位RGB完成ICC后转8位；alpha单独量化，编辑与另存使用此副本",
                    "16-bit RGB is ICC-converted before 8-bit quantization; alpha is separate. Editing and saving use this copy"
                ).into()
            } else if metadata.and_then(|metadata| metadata.decoded_color).is_some_and(|color| matches!(color, image::ColorType::Rgb16 | image::ColorType::Rgba16)) {
                tr!(
                    "原16位RGB已量化为8位副本；ICC状态见下方，编辑与另存使用此副本",
                    "Original 16-bit RGB quantized to an 8-bit copy; see ICC status below. Editing and saving use this copy"
                ).into()
            } else if metadata.and_then(|metadata| metadata.decoded_color).is_some_and(|color| matches!(color, image::ColorType::Rgb32F | image::ColorType::Rgba32F)) {
                tr!(
                    "浮点值先限制在0–1并量化为8位，再处理ICC；未做HDR色调映射，编辑与另存使用此副本",
                    "Float values are clipped to 0–1 and quantized to 8-bit before ICC. No HDR tone mapping; editing and saving use this copy"
                ).into()
            } else {
                tr!(
                    "统一读入先转8位；ICC处理、编辑与另存均使用此副本",
                    "Converted to 8-bit before ICC and editing; saving uses this 8-bit copy"
                ).into()
            },
        ));
    }
    if metadata.is_some_and(|metadata| {
        matches!(
            metadata.format,
            crate::image_metadata::SourceFormat::Heif
                | crate::image_metadata::SourceFormat::Raster(image::ImageFormat::Avif)
        )
    }) {
        let hdr_mapped = metadata
            .and_then(|metadata| metadata.details)
            .is_some_and(|details| {
                details.color_conversion
                    == crate::image_color_management::ColorConversion::HdrMappedSrgb
            });
        rows.push((
            tr!("系统显示精度", "System preview precision"),
            metadata
                .and_then(|metadata| metadata.details)
                .and_then(|details| details.source.bits_per_channel)
                .map_or_else(
                    || tr!(
                        "系统转换为8位副本；原始精度和HDR映射未验证，编辑及另存使用此副本",
                        "System 8-bit copy; original precision and HDR mapping are unverified. Editing and saving use this copy"
                    ).into(),
                    |bits| if hdr_mapped {
                        trf!(
                            "已检查{bits}位编码；按固定100nit策略映射为8位sRGB，编辑及另存使用此副本",
                            "Checked {bits}-bit source; fixed 100-nit mapping to 8-bit sRGB for editing and saving"
                        )
                    } else {
                        trf!(
                            "已检查{bits}位编码；系统8位副本用于编辑及另存，颜色处理见下方，HDR映射未验证",
                            "Checked {bits}-bit source; editing and saving use the system 8-bit copy. Color status is below; HDR mapping is unverified"
                        )
                    },
                ),
        ));
    }
    if let Some(details) = metadata.and_then(|value| value.details) {
        rows.push((
            tr!("来源颜色模型", "Source color model"),
            color_model(details.source.color_model).into(),
        ));
        rows.push((
            tr!("颜色处理", "Color processing"),
            details.color_conversion.label().into(),
        ));
        if let Some(profile) = details.profile {
            rows.push((
                tr!("ICC结构", "ICC structure"),
                format!(
                    "v{} · {} B · {} {}",
                    profile.version_major,
                    profile.declared_bytes,
                    profile.tag_count,
                    tr!("标签", "tags")
                ),
            ));
            rows.push((
                tr!("ICC颜色／连接空间", "ICC color / connection space"),
                format!(
                    "{} / {}",
                    crate::image_color_profile::signature_label(profile.color_space),
                    crate::image_color_profile::signature_label(profile.connection_space)
                ),
            ));
            rows.push((
                tr!("ICC设备类别", "ICC device class"),
                crate::image_color_profile::signature_label(profile.device_class),
            ));
        }
        if details.source.exif.status == MetadataStatus::Present {
            rows.push((
                tr!("EXIF范围", "EXIF scope"),
                tr!(
                    "方向已用于归一显示；其它字段按白名单独立检查，不代表完整保留",
                    "Orientation normalizes display; other fields are checked separately for partial preservation"
                )
                .into(),
            ));
            if metadata.is_some_and(|value| value.format == image::ImageFormat::Ico) {
                rows.push((
                    tr!("图标方向", "Icon orientation"),
                    tr!(
                        "按容器目录坐标显示；不应用嵌入PNG的EXIF方向",
                        "Use container coordinates; embedded PNG EXIF orientation is ignored"
                    )
                    .into(),
                ));
            }
        }
    }
    if let Some(source) = metadata_source {
        if let Some(exif) = source.exif_summary() {
            rows.push((
                tr!("可保留EXIF", "Supported EXIF"),
                trf!(
                    "已支持 {} 个字段；{} 个已扫描标签不保留",
                    "{} supported fields; {} scanned tags omitted",
                    exif.retained_fields,
                    exif.dropped_fields
                ),
            ));
            rows.push((
                tr!("EXIF排除项", "Excluded EXIF"),
                tr!(
                    "GPS标签、缩略图、厂商及未知字段不保留；输出方向和尺寸重新生成",
                    "GPS tags, thumbnails, vendor and unknown fields are omitted; output orientation and dimensions are rebuilt"
                )
                .into(),
            ));
        } else if let Some(error) = source.exif_error() {
            rows.push((
                tr!("EXIF保留不可用", "EXIF preservation unavailable"),
                error.into(),
            ));
        }
    }
    if metadata.is_some() {
        rows.push((
            tr!("另存元数据", "Saved metadata"),
            tr!(
                "默认重新编码并移除元数据；有已验证的来源信息时，图片处理可保留颜色或颜色及已支持EXIF",
                "Metadata is stripped by default; verified source details enable keeping color or color and supported EXIF"
            )
            .into(),
        ));
    }
    // 静态来源不附加动画字段；GIF尚未建立播放快照时仍明确显示未知。
    if animation.is_some()
        || metadata.is_some_and(|metadata| metadata.format == image::ImageFormat::Gif)
    {
        rows.extend([
            (
                tr!("动画当前帧", "Animation frame"),
                animation
                    .and_then(|animation| animation.frame_index)
                    .and_then(|index| index.checked_add(1))
                    .map_or_else(unknown, |index| index.to_string()),
            ),
            (
                tr!("当前循环", "Current loop"),
                animation
                    .and_then(|animation| animation.cycle)
                    .and_then(|cycle| cycle.checked_add(1))
                    .map_or_else(unknown, |cycle| cycle.to_string()),
            ),
            (
                tr!("播放状态", "Playback"),
                animation.map_or_else(unknown, |animation| playback_state(animation.state).into()),
            ),
            (
                tr!("源帧延时", "Source frame delay"),
                frame.map_or_else(unknown, |frame| frame.raw_delay_label()),
            ),
            (
                tr!("播放帧延时", "Playback frame delay"),
                frame.map_or_else(unknown, |frame| {
                    frame.timeout().map_or_else(
                        || tr!("等待输入", "Wait for input").into(),
                        |delay| format!("{:.3} ms", delay.as_secs_f64() * 1000.),
                    )
                }),
            ),
        ]);
    }
    rows
}

pub(super) fn collection_rows(info: &pages::Info) -> Vec<(&'static str, String)> {
    let (count_label, selected_label) = match info.kind {
        pages::Kind::Icon => (
            tr!("图标尺寸项", "Icon sizes"),
            tr!("所选尺寸项", "Selected icon size"),
        ),
        pages::Kind::Cursor => (
            tr!("光标尺寸项", "Cursor sizes"),
            tr!("所选尺寸项", "Selected cursor size"),
        ),
        pages::Kind::Tiff => (tr!("页数", "Pages"), tr!("所选页", "Selected page")),
        pages::Kind::System(_) => (
            tr!("独立图项数", "Independent image items"),
            tr!("所选图项", "Selected image item"),
        ),
    };
    let mut rows = vec![(count_label, info.count().to_string())];
    if matches!(info.kind, pages::Kind::System(_)) {
        rows.push((
            tr!("图项目录", "Image item directory"),
            tr!(
                "第1项为原主图；其余未隐藏的独立图项按原目录顺序排列，不代表动画帧",
                "Item 1 is the original primary image; remaining non-hidden independent items follow source directory order, not animation frames"
            )
            .into(),
        ));
    }
    let selected_entry = info.selected_entry();
    if let Some(entry) = selected_entry {
        let depth = if entry.bits_per_pixel == 0 {
            tr!("位深未声明", "Depth unspecified").into()
        } else {
            format!("{} bpp", entry.bits_per_pixel)
        };
        rows.push((
            selected_label,
            format!(
                "{} / {} · {} × {} px · {}",
                info.selected + 1,
                info.count(),
                entry.dimensions.0,
                entry.dimensions.1,
                depth
            ),
        ));
        if let Some((x, y)) = entry.hotspot {
            rows.push((
                tr!("热点（源像素，0起）", "Hotspot (source pixels, 0-based)"),
                format!("({x}, {y})"),
            ));
        }
    } else if matches!(info.kind, pages::Kind::System(_)) {
        rows.push((
            selected_label,
            format!(
                "{} / {} · {}",
                info.selected + 1,
                info.count(),
                tr!("尚未交付像素", "Pixels unavailable")
            ),
        ));
    }
    if matches!(info.kind, pages::Kind::System(_)) {
        let item = selected_entry.and_then(|entry| entry.source_item);
        rows.push((
            tr!("原图项ID", "Source item ID"),
            item.map_or_else(
                || tr!("未知", "Unknown").into(),
                |item| item.item_id.to_string(),
            ),
        ));
        rows.push((
            tr!("原主图", "Original primary image"),
            item.map_or_else(
                || tr!("未知", "Unknown").into(),
                |item| {
                    if item.is_primary {
                        tr!("是", "Yes").into()
                    } else {
                        tr!("否", "No").into()
                    }
                },
            ),
        ));
        let color = item.and_then(|item| item.color);
        rows.push((
            tr!("容器 nclx", "Container nclx"),
            cicp_summary(
                color.and_then(|color| color.container_cicp),
                color.is_some(),
            ),
        ));
        rows.push((
            tr!(
                "码流 CICP（含语法默认）",
                "Bitstream CICP (including defaults)"
            ),
            cicp_summary(
                color.and_then(|color| color.bitstream_cicp),
                color.is_some(),
            ),
        ));
        rows.push((
            tr!("码流采样模型", "Coded sample model"),
            match color.map(|color| color.sample_model) {
                Some(crate::image_decode::platform::SampleModel::Rgb) => "RGB",
                Some(crate::image_decode::platform::SampleModel::Ycc) => "YCC",
                Some(crate::image_decode::platform::SampleModel::Gray) => tr!("灰度", "Grayscale"),
                Some(crate::image_decode::platform::SampleModel::Unknown) | None => {
                    tr!("未知", "Unknown")
                }
            }
            .into(),
        ));
        if color.is_some_and(|color| {
            [color.container_cicp, color.bitstream_cicp]
                .into_iter()
                .flatten()
                .any(|cicp| matches!(cicp.transfer, 16 | 18))
        }) {
            rows.push((
                tr!("HDR信号", "HDR signal"),
                if color.is_some_and(|color| color.hdr_mapped) {
                    tr!(
                        "PQ／HLG已按固定100nit策略映射为8位sRGB；非原精度HDR导出",
                        "PQ/HLG mapped to 8-bit sRGB with a fixed 100-nit policy; export does not retain HDR precision"
                    ).into()
                } else {
                    tr!(
                        "来源包含PQ或HLG传递信号；系统8位副本的HDR映射尚未验证",
                        "Source has a PQ or HLG transfer signal; HDR mapping of the system 8-bit copy is unverified"
                    ).into()
                },
            ));
        }
    }
    rows
}

fn cicp_summary(value: Option<crate::image_decode::platform::Cicp>, inspected: bool) -> String {
    match value {
        Some(value) => {
            let range = if value.full_range {
                tr!("全范围", "Full range")
            } else {
                tr!("有限范围", "Limited range")
            };
            format!(
                "{}/{}/{} · {range}",
                value.primaries, value.transfer, value.matrix
            )
        }
        None if inspected => tr!("未声明", "Not declared").into(),
        None => tr!("未知", "Unknown").into(),
    }
}

fn field_summary(value: Option<FieldSummary>) -> String {
    let Some(value) = value else {
        return tr!("未检查", "Not inspected").into();
    };
    let state = match value.status {
        MetadataStatus::NotInspected => tr!("未检查", "Not inspected"),
        MetadataStatus::Absent => tr!("未发现", "Not found"),
        MetadataStatus::Present => tr!("已提取", "Extracted"),
        MetadataStatus::TooLarge => tr!("超过读取上限", "Exceeds the read limit"),
        MetadataStatus::Malformed => tr!("数据损坏", "Malformed data"),
        MetadataStatus::Unsupported => tr!("尚不支持", "Unsupported"),
    };
    if let Some(bytes) = value.decoded_bytes {
        format!("{state} · {bytes} B")
    } else {
        state.into()
    }
}

fn color_model(value: ColorModel) -> &'static str {
    match value {
        ColorModel::Bgr => "BGR",
        ColorModel::Rgb => "RGB",
        ColorModel::Gray => tr!("灰度", "Grayscale"),
        ColorModel::Cmyk => tr!("CMYK／四分量", "CMYK / 4 components"),
        ColorModel::Palette => tr!("索引调色板", "Indexed palette"),
        ColorModel::Unknown => tr!("未知", "Unknown"),
    }
}

fn file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.2} MiB ({bytes} B)", bytes as f64 / (1024. * 1024.))
    } else if bytes >= 1024 {
        format!("{:.2} KiB ({bytes} B)", bytes as f64 / 1024.)
    } else {
        format!("{bytes} B")
    }
}

fn orientation(value: u8) -> String {
    let description = match value {
        // 解码器未读出方向标签时也返回无变换，无法由现有快照区分。
        1 => tr!("无变换或未指定", "Normal or unspecified"),
        2 => tr!("水平镜像", "Mirror horizontal"),
        3 => tr!("旋转180°", "Rotate 180°"),
        4 => tr!("垂直镜像", "Mirror vertical"),
        5 => tr!("主对角线镜像", "Transpose"),
        6 => tr!("顺时针90°", "90° clockwise"),
        7 => tr!("副对角线镜像", "Transverse"),
        8 => tr!("逆时针90°", "90° counterclockwise"),
        _ => return tr!("未知", "Unknown").into(),
    };
    format!("{value} · {description}")
}

fn playback_state(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Loading => tr!("加载中", "Loading"),
        PlaybackState::Playing => tr!("播放中", "Playing"),
        PlaybackState::Paused => tr!("已暂停", "Paused"),
        PlaybackState::WaitingForInput => tr!("等待输入", "Waiting for input"),
        PlaybackState::Ended => tr!("已结束", "Ended"),
        PlaybackState::Cancelled => tr!("已取消", "Canceled"),
        PlaybackState::Failed => tr!("播放失败", "Playback failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_metadata::SourceFormat;

    fn item_info(index: usize, item_id: u32, is_primary: bool) -> pages::Info {
        pages::Info {
            kind: pages::Kind::System(crate::image_decode::platform::PlatformFormat::Heif),
            entries: vec![pages::Choice {
                index,
                dimensions: (2, 1),
                bits_per_pixel: 0,
                hotspot: None,
                source_item: Some(pages::SourceItem {
                    item_id,
                    is_primary,
                    color: None,
                }),
            }],
            selected: index,
            reported_total: Some(3),
        }
    }

    fn value<'a>(rows: &'a [(&'static str, String)], label: &str) -> &'a str {
        &rows.iter().find(|row| row.0 == label).unwrap().1
    }

    #[test]
    fn image_viewer_properties_unknown_source_geometry_and_orientation_remain_unknown() {
        let metadata = ImageMetadata {
            format: SourceFormat::Heif,
            encoded_dimensions: None,
            display_dimensions: (2, 1),
            orientation: None,
            encoded_bytes: Some(128),
            decoded_color: None,
            details: None,
        };
        let rows = rows(None, Some(metadata), None, None, None, None);
        assert_eq!(rows[1].1, "HEIF");
        assert!(["未知", "Unknown"].contains(&rows[2].1.as_str()));
        assert_eq!(rows[3].1, "2 × 1 px");
        assert!(["未知", "Unknown"].contains(&rows[5].1.as_str()));
        assert!(["未知", "Unknown"].contains(&rows[6].1.as_str()));
        assert!(rows[7].1.contains("BGRA8"));
    }

    #[test]
    fn image_viewer_properties_checked_item_orientation_does_not_claim_exif_or_icc_inspection() {
        let metadata = ImageMetadata {
            format: SourceFormat::Heif,
            encoded_dimensions: Some((1, 2)),
            display_dimensions: (2, 1),
            orientation: image::metadata::Orientation::from_exif(6),
            encoded_bytes: Some(128),
            decoded_color: Some(image::ColorType::Rgba8),
            details: None,
        };
        let rows = rows(None, Some(metadata), None, None, None, None);
        assert!(value(
            &rows,
            tr!("来源方向（归一）", "Source orientation (normalized)")
        )
        .starts_with("6 · "));
        assert_eq!(
            value(&rows, tr!("ICC读取", "ICC read status")),
            tr!("未检查", "Not inspected")
        );
        assert_eq!(
            value(&rows, tr!("EXIF读取", "EXIF read status")),
            tr!("未检查", "Not inspected")
        );
    }

    #[test]
    fn image_viewer_properties_hdr_mapping_discloses_source_precision_and_sdr_copy() {
        use crate::{
            image_color_management::{ColorConversion, ManagedMetadata},
            image_container_metadata::{ColorModel, FieldSummary, MetadataStatus, Summary},
        };
        let field = |status| FieldSummary {
            status,
            encoded_bytes: 0,
            decoded_bytes: None,
        };
        let metadata = ImageMetadata {
            format: SourceFormat::Heif,
            encoded_dimensions: Some((4, 2)),
            display_dimensions: (2, 2),
            orientation: image::metadata::Orientation::from_exif(6),
            encoded_bytes: Some(128),
            decoded_color: None,
            details: Some(ManagedMetadata {
                source: Summary {
                    icc: field(MetadataStatus::Absent),
                    exif: field(MetadataStatus::NotInspected),
                    color_model: ColorModel::Rgb,
                    bits_per_channel: Some(10),
                    orientation: image::metadata::Orientation::from_exif(6),
                },
                profile: None,
                color_conversion: ColorConversion::HdrMappedSrgb,
            }),
        };
        let mapped_rows = rows(None, Some(metadata), None, None, None, None);
        let precision = value(
            &mapped_rows,
            tr!("系统显示精度", "System preview precision"),
        );
        assert!(precision.contains("10"));
        assert!(precision.contains("100"));
        assert!(precision.contains("8"));
        assert!(precision.contains("sRGB"));
        assert_eq!(
            value(&mapped_rows, tr!("颜色处理", "Color processing")),
            ColorConversion::HdrMappedSrgb.label()
        );
        let mut unmapped = metadata;
        unmapped.details.as_mut().unwrap().color_conversion = ColorConversion::SignalUnmapped;
        let unmapped_rows = rows(None, Some(unmapped), None, None, None, None);
        assert!(!value(
            &unmapped_rows,
            tr!("系统显示精度", "System preview precision")
        )
        .contains("100"));
    }

    #[test]
    fn image_viewer_properties_word_icc_order_is_explicit_and_failure_does_not_claim_eight_bit_icc()
    {
        use crate::{
            image_color_management::{ColorConversion, ManagedMetadata, UnappliedReason},
            image_container_metadata::{ColorModel, Summary},
        };
        let unread = FieldSummary {
            status: MetadataStatus::NotInspected,
            encoded_bytes: 0,
            decoded_bytes: None,
        };
        let mut metadata = ImageMetadata {
            format: SourceFormat::Raster(image::ImageFormat::Png),
            encoded_dimensions: Some((2, 1)),
            display_dimensions: (1, 2),
            orientation: image::metadata::Orientation::from_exif(6),
            encoded_bytes: Some(128),
            decoded_color: Some(image::ColorType::Rgba16),
            details: Some(ManagedMetadata {
                source: Summary {
                    icc: unread,
                    exif: unread,
                    color_model: ColorModel::Rgb,
                    bits_per_channel: Some(16),
                    orientation: image::metadata::Orientation::from_exif(6),
                },
                profile: None,
                color_conversion: ColorConversion::ConvertedSrgb16,
            }),
        };
        let converted = rows(None, Some(metadata), None, None, None, None);
        assert_eq!(
            value(&converted, tr!("颜色处理", "Color processing")),
            ColorConversion::ConvertedSrgb16.label()
        );
        assert_eq!(value(&converted, tr!("精度转换", "Precision conversion")), tr!("保留16位RGB完成ICC后转8位；alpha单独量化，编辑与另存使用此副本", "16-bit RGB is ICC-converted before 8-bit quantization; alpha is separate. Editing and saving use this copy"));
        metadata.details.as_mut().unwrap().color_conversion =
            ColorConversion::EmbeddedUnapplied(UnappliedReason::TransformFailed);
        let failed = rows(None, Some(metadata), None, None, None, None);
        assert_eq!(value(&failed, tr!("精度转换", "Precision conversion")), tr!("原16位RGB已量化为8位副本；ICC状态见下方，编辑与另存使用此副本", "Original 16-bit RGB quantized to an 8-bit copy; see ICC status below. Editing and saving use this copy"));
        assert_ne!(
            value(&failed, tr!("精度转换", "Precision conversion")),
            value(&converted, tr!("精度转换", "Precision conversion"))
        );
    }

    #[test]
    fn image_viewer_properties_item_identity_uses_original_id_and_primary_directory_order() {
        let primary = collection_rows(&item_info(0, 73, true));
        assert_eq!(
            value(&primary, tr!("独立图项数", "Independent image items")),
            "3"
        );
        assert_eq!(value(&primary, tr!("原图项ID", "Source item ID")), "73");
        assert_eq!(
            value(&primary, tr!("原主图", "Original primary image")),
            tr!("是", "Yes")
        );
        assert_eq!(
            value(&primary, tr!("图项目录", "Image item directory")),
            tr!(
                "第1项为原主图；其余未隐藏的独立图项按原目录顺序排列，不代表动画帧",
                "Item 1 is the original primary image; remaining non-hidden independent items follow source directory order, not animation frames"
            )
        );
        let other = collection_rows(&item_info(2, 41, false));
        assert_eq!(value(&other, tr!("原图项ID", "Source item ID")), "41");
        assert_eq!(
            value(&other, tr!("原主图", "Original primary image")),
            tr!("否", "No")
        );
        assert!(
            value(&other, tr!("所选图项", "Selected image item")).starts_with("3 / 3 · 2 × 1 px")
        );
        assert!(!other
            .iter()
            .any(|row| row.0 == tr!("总帧数", "Total frames")));
    }

    #[test]
    fn image_viewer_properties_missing_item_never_reuses_previous_identity_or_geometry() {
        let mut info = item_info(0, 73, true);
        info.selected = 1;
        let rows = collection_rows(&info);
        assert_eq!(
            value(&rows, tr!("独立图项数", "Independent image items")),
            "3"
        );
        assert_eq!(
            value(&rows, tr!("原图项ID", "Source item ID")),
            tr!("未知", "Unknown")
        );
        assert_eq!(
            value(&rows, tr!("原主图", "Original primary image")),
            tr!("未知", "Unknown")
        );
        assert_eq!(
            value(&rows, tr!("所选图项", "Selected image item")),
            format!("2 / 3 · {}", tr!("尚未交付像素", "Pixels unavailable"))
        );
        assert!(!rows.iter().any(|row| row.1.contains("2 × 1")));
    }

    #[test]
    fn image_viewer_properties_container_and_bitstream_colors_do_not_overwrite_each_other() {
        use crate::image_decode::platform::{Cicp, ColorFacts, SampleModel};
        let mut info = item_info(0, 73, true);
        info.entries[0].source_item.as_mut().unwrap().color = Some(ColorFacts {
            container_cicp: Some(Cicp {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: true,
            }),
            bitstream_cicp: Some(Cicp {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            }),
            sample_model: SampleModel::Ycc,
            hdr_mapped: false,
        });
        let rows = collection_rows(&info);
        assert!(value(&rows, tr!("容器 nclx", "Container nclx")).starts_with("1/13/6 · "));
        assert!(value(
            &rows,
            tr!(
                "码流 CICP（含语法默认）",
                "Bitstream CICP (including defaults)"
            )
        )
        .starts_with("9/16/9 · "));
        assert_eq!(
            value(&rows, tr!("码流采样模型", "Coded sample model")),
            "YCC"
        );
        assert!(value(&rows, tr!("HDR信号", "HDR signal")).contains("PQ"));
        info.entries[0]
            .source_item
            .as_mut()
            .unwrap()
            .color
            .as_mut()
            .unwrap()
            .hdr_mapped = true;
        let mapped_rows = collection_rows(&info);
        assert!(value(&mapped_rows, tr!("HDR信号", "HDR signal")).contains("100"));

        info.selected = 1;
        let rows = collection_rows(&info);
        assert_eq!(
            value(&rows, tr!("容器 nclx", "Container nclx")),
            tr!("未知", "Unknown")
        );
        assert!(!rows.iter().any(|row| row.0 == tr!("HDR信号", "HDR signal")));

        info.selected = 0;
        info.entries[0].source_item.as_mut().unwrap().color = Some(ColorFacts {
            container_cicp: None,
            bitstream_cicp: None,
            sample_model: SampleModel::Ycc,
            hdr_mapped: false,
        });
        let rows = collection_rows(&info);
        assert_eq!(
            value(&rows, tr!("容器 nclx", "Container nclx")),
            tr!("未声明", "Not declared")
        );
        assert!(!rows.iter().any(|row| row.0 == tr!("HDR信号", "HDR signal")));
    }

    #[test]
    fn image_viewer_properties_cursor_hotspot_remains_source_coordinates_without_item_identity() {
        let info = pages::Info {
            kind: pages::Kind::Cursor,
            entries: vec![pages::Choice {
                index: 0,
                dimensions: (32, 16),
                bits_per_pixel: 0,
                hotspot: Some((13, 7)),
                source_item: None,
            }],
            selected: 0,
            reported_total: None,
        };
        let rows = collection_rows(&info);
        assert_eq!(value(&rows, tr!("光标尺寸项", "Cursor sizes")), "1");
        assert_eq!(
            value(
                &rows,
                tr!("热点（源像素，0起）", "Hotspot (source pixels, 0-based)")
            ),
            "(13, 7)"
        );
        assert!(!rows
            .iter()
            .any(|row| row.0 == tr!("原图项ID", "Source item ID")));
        assert!(!rows
            .iter()
            .any(|row| row.0 == tr!("原主图", "Original primary image")));
    }
}
