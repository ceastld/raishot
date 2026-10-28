//! 原图、标注与套索完成合成后，再按剪辑的保留片段拼接成品。

use crate::{capture::Image, long_clip::LongClip, stitch::ScrollAxis, Result};

/// 输入保留原图坐标；本函数只搬运 BGRA，不重新采样或改变透明度。
pub(crate) fn compose(image: &Image, clip: &LongClip) -> Result<Image> {
    // 在分配像素前统一检查剪辑几何和成品上限，原图本身可大于成品上限。
    clip.check_output()?;
    if (image.width, image.height) != clip.source_size() {
        return Err("剪辑原图尺寸与渲染图片不一致".into());
    }
    let source_bytes = image_bytes(image.width, image.height)?;
    if image.bgra.len() != source_bytes {
        return Err("剪辑原图尺寸与 BGRA 像素数据不一致".into());
    }

    let (width, height) = clip.output_size();
    let output_bytes = image_bytes(width, height)?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(output_bytes)?;
    bgra.resize(output_bytes, 0);

    let source_stride = image.width as usize * 4;
    let output_stride = width as usize * 4;
    let cross = clip.cross_range();
    let segments = clip.segments();
    match clip.axis() {
        ScrollAxis::Vertical => {
            let cross_start = cross.start as usize * 4;
            for segment in segments {
                let rows = (segment.source.end - segment.source.start) as usize;
                let source_start = segment.source.start as usize * source_stride + cross_start;
                let output_start = segment.display_start as usize * output_stride;
                if output_stride == source_stride {
                    // 没裁左右边时，一整个保留片段是连续内存。
                    let bytes = rows * source_stride;
                    bgra[output_start..output_start + bytes]
                        .copy_from_slice(&image.bgra[source_start..source_start + bytes]);
                } else {
                    for row in 0..rows {
                        let src = source_start + row * source_stride;
                        let dst = output_start + row * output_stride;
                        bgra[dst..dst + output_stride]
                            .copy_from_slice(&image.bgra[src..src + output_stride]);
                    }
                }
            }
        }
        ScrollAxis::Horizontal => {
            for (output_y, source_y) in (cross.start..cross.end).enumerate() {
                let source_row = source_y as usize * source_stride;
                let output_row = output_y * output_stride;
                for segment in &segments {
                    let bytes = (segment.source.end - segment.source.start) as usize * 4;
                    let src = source_row + segment.source.start as usize * 4;
                    let dst = output_row + segment.display_start as usize * 4;
                    bgra[dst..dst + bytes].copy_from_slice(&image.bgra[src..src + bytes]);
                }
            }
        }
    }

    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: image.readback_ms,
    })
}

fn image_bytes(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err("剪辑图片尺寸为空".into());
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "剪辑图片尺寸溢出".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(id: u8) -> [u8; 4] {
        [id, id.wrapping_add(64), id.wrapping_add(128), 255]
    }

    fn pixels(ids: &[u8]) -> Vec<u8> {
        ids.iter().flat_map(|id| pixel(*id)).collect()
    }

    fn numbered_image(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            bgra: (0..width * height).flat_map(|id| pixel(id as u8)).collect(),
            readback_ms: 7.25,
        }
    }

    #[test]
    fn vertical_seams_and_cross_crop_copy_known_rows() {
        let image = numbered_image(4, 7);
        let original = image.bgra.clone();
        let mut clip = LongClip::new(4, 7, ScrollAxis::Vertical).unwrap();
        clip.delete(1..3).unwrap();
        clip.delete(4..6).unwrap();
        clip.set_cross(1..3).unwrap();

        let output = compose(&image, &clip).unwrap();
        assert_eq!((output.width, output.height), (2, 3));
        assert_eq!(output.bgra, pixels(&[1, 2, 13, 14, 25, 26]));
        assert_eq!(output.readback_ms, 7.25);
        assert_eq!(image.bgra, original);
    }

    #[test]
    fn vertical_full_width_segments_copy_contiguous_rows() {
        let image = numbered_image(3, 4);
        let mut clip = LongClip::new(3, 4, ScrollAxis::Vertical).unwrap();
        clip.delete(1..3).unwrap();

        let output = compose(&image, &clip).unwrap();
        assert_eq!((output.width, output.height), (3, 2));
        assert_eq!(output.bgra, pixels(&[0, 1, 2, 9, 10, 11]));
    }

    #[test]
    fn horizontal_seams_and_cross_crop_copy_known_columns() {
        let image = numbered_image(7, 4);
        let mut clip = LongClip::new(7, 4, ScrollAxis::Horizontal).unwrap();
        clip.delete(1..3).unwrap();
        clip.delete(4..6).unwrap();
        clip.set_cross(1..3).unwrap();

        let output = compose(&image, &clip).unwrap();
        assert_eq!((output.width, output.height), (3, 2));
        assert_eq!(output.bgra, pixels(&[7, 10, 13, 14, 17, 20]));
    }

    #[test]
    fn rendered_annotation_crossing_seam_is_cut_in_source_coordinates() {
        const BACKGROUND: [u8; 4] = [10, 20, 30, 255];
        const MARK: [u8; 4] = [0, 0, 255, 255];
        let mut image = Image {
            width: 6,
            height: 6,
            bgra: BACKGROUND.repeat(36),
            readback_ms: 0.,
        };
        // 一条已经叠在原图上的斜线，跨过将删除的两行。
        for point in 0..6 {
            let offset = (point * 6 + point) * 4;
            image.bgra[offset..offset + 4].copy_from_slice(&MARK);
        }
        // 完全位于删除段内的另一处标注也随像素消失。
        image.bgra[48..52].copy_from_slice(&[255, 0, 0, 255]);
        let mut clip = LongClip::new(6, 6, ScrollAxis::Vertical).unwrap();
        clip.delete(2..4).unwrap();

        let output = compose(&image, &clip).unwrap();
        let mut expected = BACKGROUND.repeat(24);
        for (x, y) in [(0, 0), (1, 1), (4, 2), (5, 3)] {
            let offset = (y * 6 + x) * 4;
            expected[offset..offset + 4].copy_from_slice(&MARK);
        }
        assert_eq!((output.width, output.height), (6, 4));
        assert_eq!(output.bgra, expected);
    }

    #[test]
    fn lasso_transparency_and_color_channels_are_preserved_byte_for_byte() {
        let mut image = numbered_image(3, 4);
        for (offset, color) in [
            (4, [11, 22, 33, 0]),
            (8, [44, 55, 66, 100]),
            (40, [77, 88, 99, 255]),
            (44, [111, 122, 133, 1]),
        ] {
            image.bgra[offset..offset + 4].copy_from_slice(&color);
        }
        let mut clip = LongClip::new(3, 4, ScrollAxis::Vertical).unwrap();
        clip.delete(1..3).unwrap();
        clip.set_cross(1..3).unwrap();

        let output = compose(&image, &clip).unwrap();
        assert_eq!(
            output.bgra,
            [11, 22, 33, 0, 44, 55, 66, 100, 77, 88, 99, 255, 111, 122, 133, 1,]
        );
    }

    #[test]
    fn rejects_source_size_mismatch_and_inexact_bgra_length() {
        let clip = LongClip::new(4, 3, ScrollAxis::Vertical).unwrap();
        assert!(compose(&numbered_image(3, 4), &clip).is_err());
        for length in [0, 47, 49] {
            let image = Image {
                width: 4,
                height: 3,
                bgra: vec![0; length],
                readback_ms: 0.,
            };
            assert!(compose(&image, &clip).is_err());
        }
        assert!(image_bytes(0, 3).is_err());
        assert!(image_bytes(3, 0).is_err());
        assert!(image_bytes(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn output_limit_is_checked_before_reading_or_allocating_pixels() {
        for (width, height) in [(65_536, 1), (1, 65_536), (8_000, 4_001)] {
            let clip = LongClip::new(width, height, ScrollAxis::Vertical).unwrap();
            let expected_error = clip.check_output().unwrap_err().to_string();
            let image = Image {
                width,
                height,
                bgra: Vec::new(),
                readback_ms: 0.,
            };
            assert_eq!(
                compose(&image, &clip).err().unwrap().to_string(),
                expected_error
            );
        }
    }

    #[test]
    fn large_source_is_allowed_when_retained_output_fits() {
        let image = Image {
            width: 65_536,
            height: 1,
            bgra: [3, 5, 7, 9].repeat(65_536),
            readback_ms: 0.,
        };
        let mut clip = LongClip::new(65_536, 1, ScrollAxis::Horizontal).unwrap();
        clip.delete(1..65_536).unwrap();

        let output = compose(&image, &clip).unwrap();
        assert_eq!((output.width, output.height), (1, 1));
        assert_eq!(output.bgra, [3, 5, 7, 9]);
    }
}
