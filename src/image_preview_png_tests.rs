use super::*;
use crate::image_container_metadata::{ColorModel, MetadataStatus};
use std::io::Write;

fn png(dimensions: (u32, u32), color: png::ColorType, depth: png::BitDepth, raw: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, dimensions.0, dimensions.1);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(raw).unwrap();
        writer.finish().unwrap();
    }
    output
}

fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    result.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    result.extend_from_slice(kind);
    result.extend_from_slice(payload);
    let mut crc = !0u32;
    for byte in &result[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32 & 0u32.wrapping_sub(crc & 1));
        }
    }
    result.extend_from_slice(&(!crc).to_be_bytes());
    result
}

fn with_chunk(bytes: &[u8], kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut output = bytes[..33].to_vec();
    output.extend_from_slice(&chunk(kind, data));
    output.extend_from_slice(&bytes[33..]);
    output
}

fn exif(orientation: u16) -> Vec<u8> {
    let mut tiff = b"II\x2a\x00\x08\x00\x00\x00\x01\x00".to_vec();
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0; 6]);
    tiff
}

fn limits(max_side: u32) -> PreviewLimits {
    PreviewLimits {
        max_side,
        max_pixels: 1_000_000,
    }
}

fn preview(bytes: &Vec<u8>, max_side: u32) -> PreviewDecoded {
    let cancel = AtomicBool::new(false);
    let plan = inspect(bytes, bytes.capacity() as u64, 0, limits(max_side), &cancel).unwrap();
    decode(plan, &cancel).unwrap()
}

/// 只有一行，写入也分块；不为200000宽来源创建完整RGBA native数组。
fn very_wide_png() -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, 200_000, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        {
            let mut stream = writer.stream_writer().unwrap();
            let mut block = [0u8; 4096];
            let mut start = 0u32;
            while start < 200_000 {
                let count = (200_000 - start).min(1024);
                for i in 0..count {
                    let band = ((start + i) / 25_000) as u8;
                    block[i as usize * 4..i as usize * 4 + 4].copy_from_slice(&[
                        band,
                        40 + band,
                        240 - band,
                        [0, 128, 255][band as usize % 3],
                    ]);
                }
                stream.write_all(&block[..count as usize * 4]).unwrap();
                start += count;
            }
            stream.finish().unwrap();
        }
        writer.finish().unwrap();
    }
    output
}

#[test]
fn image_preview_png_real_oversized_row_keeps_source_facts_and_tiny_samples() {
    let bytes = very_wide_png();
    // 完整入口仍拒绝超65535边；这个新入口没有扩大其能力或完整图像资格。
    assert!(crate::image_decode::decode_bytes(&bytes).is_err());
    let cancel = AtomicBool::new(false);
    let plan = inspect(
        &bytes,
        bytes.capacity() as u64,
        32 * 1024 * 1024,
        limits(8),
        &cancel,
    )
    .unwrap();
    assert_eq!(plan.geometry().encoded_dimensions(), (200_000, 1));
    assert_eq!(plan.geometry().sampled_encoded_dimensions(), (8, 1));
    assert!(plan.workspace().decode_bytes > METADATA_WORKSPACE);
    let decoded = decode(plan, &cancel).unwrap();
    assert_eq!((decoded.image.width, decoded.image.height), (8, 1));
    assert_eq!(decoded.image.bgra.capacity(), 32);
    for band in 0..8u8 {
        assert_eq!(
            &decoded.image.bgra[band as usize * 4..band as usize * 4 + 4],
            &[
                240 - band,
                40 + band,
                band,
                [0, 128, 255][band as usize % 3]
            ]
        );
    }
    assert_eq!(decoded.source.bits_per_channel, Some(8));
    assert_eq!(decoded.source.orientation, None);
    assert_eq!(decoded.source.color_model, ColorModel::Rgb);
    assert_eq!(decoded.color, PreviewColorStatus::NotColorManaged);
    assert_eq!(decoded.sampling, PreviewSampling::PixelCenterNearest);
}

#[test]
fn image_preview_png_all_exif_orientations_match_actual_sampled_centers() {
    let mut raw = Vec::new();
    for id in 0..35u8 {
        raw.extend_from_slice(&[id, 100 + id, 200 - id, id.wrapping_mul(7)]);
    }
    let source = png((7, 5), png::ColorType::Rgba, png::BitDepth::Eight, &raw);
    let cases: [((u32, u32), [u8; 6]); 8] = [
        ((3, 2), [8, 10, 12, 22, 24, 26]),
        ((3, 2), [12, 10, 8, 26, 24, 22]),
        ((3, 2), [26, 24, 22, 12, 10, 8]),
        ((3, 2), [22, 24, 26, 8, 10, 12]),
        ((2, 3), [8, 22, 10, 24, 12, 26]),
        ((2, 3), [22, 8, 24, 10, 26, 12]),
        ((2, 3), [26, 12, 24, 10, 22, 8]),
        ((2, 3), [12, 26, 10, 24, 8, 22]),
    ];
    for (index, (dimensions, ids)) in cases.into_iter().enumerate() {
        let bytes = with_chunk(&source, b"eXIf", &exif(index as u16 + 1));
        let decoded = preview(&bytes, 3);
        let geometry = decoded.geometry;
        assert_eq!(geometry.raster_dimensions(), dimensions);
        assert_eq!(geometry.encoded_dimensions(), (7, 5));
        assert_eq!(geometry.sampled_encoded_dimensions(), (3, 2));
        assert_eq!(
            geometry.source_orientation().unwrap().to_exif(),
            index as u8 + 1
        );
        assert_eq!(decoded.source.exif.status, MetadataStatus::Present);
        for (pixel_index, id) in ids.into_iter().enumerate() {
            assert_eq!(
                &decoded.image.bgra[pixel_index * 4..pixel_index * 4 + 4],
                &[200 - id, 100 + id, id, id.wrapping_mul(7)]
            );
            let x = (pixel_index as u32 % dimensions.0) as f64 + 0.5;
            let y = (pixel_index as u32 / dimensions.0) as f64 + 0.5;
            let source_dimensions = geometry.normalized_source_dimensions();
            let sample = geometry
                .sample_at_source(
                    x * source_dimensions.0 as f64 / dimensions.0 as f64,
                    y * source_dimensions.1 as f64 / dimensions.1 as f64,
                )
                .unwrap();
            assert_eq!(
                sample.encoded_source_pixel.1 * 7 + sample.encoded_source_pixel.0,
                u32::from(id)
            );
        }
    }
}

#[test]
fn image_preview_png_low_bit_palette_expands_trns_without_premultiplying() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 5, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Two);
        encoder.set_palette(vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 20, 30]);
        encoder.set_trns(vec![0, 64, 128, 255]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0x1b, 0x40]).unwrap();
        writer.finish().unwrap();
    }
    let decoded = preview(&bytes, 3);
    assert_eq!(decoded.geometry.sampled_encoded_dimensions(), (3, 1));
    assert_eq!(
        decoded.image.bgra,
        [0, 0, 255, 0, 255, 0, 0, 128, 0, 255, 0, 64]
    );
    assert_eq!(decoded.source.bits_per_channel, Some(2));
    assert_eq!(decoded.source.color_model, ColorModel::Palette);
}

#[test]
fn image_preview_png_sixteen_bit_rows_keep_straight_alpha_and_unmanaged_icc_fact() {
    let gray_values = [0u16, 65535, 32768, 32768, 65535, 0];
    let gray_raw: Vec<u8> = gray_values.into_iter().flat_map(u16::to_be_bytes).collect();
    let gray = png(
        (1, 3),
        png::ColorType::GrayscaleAlpha,
        png::BitDepth::Sixteen,
        &gray_raw,
    );
    let decoded = preview(&gray, 3);
    assert_eq!(
        decoded.image.bgra,
        [0, 0, 0, 255, 128, 128, 128, 128, 255, 255, 255, 0]
    );
    assert_eq!(decoded.source.bits_per_channel, Some(16));
    assert_eq!(decoded.source.color_model, ColorModel::Gray);

    let rgba_values = [
        0u16, 0, 0, 65535, 128, 32768, 65535, 32768, 0, 0, 0, 65535, 65535, 257, 0, 0,
    ];
    let rgba_raw: Vec<u8> = rgba_values.into_iter().flat_map(u16::to_be_bytes).collect();
    let rgba = png(
        (4, 1),
        png::ColorType::Rgba,
        png::BitDepth::Sixteen,
        &rgba_raw,
    );
    let mut compressor = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    compressor
        .write_all(include_bytes!("../assets/color/srgb.icc"))
        .unwrap();
    let mut iccp = b"RGB\0\0".to_vec();
    iccp.extend_from_slice(&compressor.finish().unwrap());
    let with_icc = with_chunk(&rgba, b"iCCP", &iccp);
    let decoded = preview(&with_icc, 2);
    assert_eq!(decoded.image.bgra, [255, 128, 0, 128, 0, 1, 255, 0]);
    assert_eq!(decoded.source.bits_per_channel, Some(16));
    assert_eq!(decoded.source.icc.status, MetadataStatus::Present);
    assert_eq!(decoded.color, PreviewColorStatus::NotColorManaged);
}

#[test]
fn image_preview_png_source_capacity_and_stage_peak_have_exact_boundaries() {
    let bytes = very_wide_png();
    let cancel = AtomicBool::new(false);
    let reservation = bytes.capacity() as u64 + 8192;
    let first = inspect(&bytes, reservation, 0, limits(8), &cancel).unwrap();
    let peak = first.workspace().peak_bytes();
    assert!(peak > METADATA_WORKSPACE);
    drop(first);
    let resident = MAX_BYTES - reservation - peak;
    let exact = inspect(&bytes, reservation, resident, limits(8), &cancel).unwrap();
    assert_eq!(exact.workspace().peak_bytes(), peak);
    assert_eq!(decode(exact, &cancel).unwrap().image.bgra.len(), 32);
    assert!(inspect(&bytes, reservation, resident + 1, limits(8), &cancel).is_err());
    assert!(inspect(&bytes, bytes.len() as u64 - 1, 0, limits(8), &cancel).is_err());
    assert!(inspect(&bytes, u64::MAX, 1, limits(8), &cancel).is_err());
    assert!(inspect(&bytes, reservation, u64::MAX, limits(8), &cancel).is_err());
    assert!(inspect(
        &bytes,
        reservation,
        0,
        PreviewLimits {
            max_pixels: 0,
            max_side: 8
        },
        &cancel
    )
    .is_err());
}

#[test]
fn image_preview_png_static_gate_checks_full_directory_and_final_checksum() {
    let source = png(
        (1, 1),
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[10, 20, 30, 40],
    );
    let cancel = AtomicBool::new(false);
    let mut animation = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut animation, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(1, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[10, 20, 30, 40]).unwrap();
        writer.finish().unwrap();
    }
    assert!(inspect(
        &animation,
        animation.capacity() as u64,
        0,
        limits(8),
        &cancel
    )
    .is_err());
    let mut interlaced = SIGNATURE.to_vec();
    let mut ihdr = source[16..29].to_vec();
    ihdr[12] = 1;
    interlaced.extend_from_slice(&chunk(b"IHDR", &ihdr));
    interlaced.extend_from_slice(&source[33..]);
    assert!(inspect(
        &interlaced,
        interlaced.capacity() as u64,
        0,
        limits(8),
        &cancel
    )
    .is_err());

    // 即使动画控制出现在IDAT后，静态预览也不能先交付首帧。
    let mut late_control = source[..source.len() - 12].to_vec();
    late_control.extend_from_slice(&chunk(b"fcTL", &[0; 26]));
    late_control.extend_from_slice(&source[source.len() - 12..]);
    assert!(inspect(
        &late_control,
        late_control.capacity() as u64,
        0,
        limits(8),
        &cancel
    )
    .is_err());

    let mut checksum = source.clone();
    *checksum.last_mut().unwrap() ^= 1;
    let plan = inspect(&checksum, checksum.capacity() as u64, 0, limits(8), &cancel).unwrap();
    assert!(decode(plan, &cancel).is_err());
    for data in [&source[..source.len() - 1], b"not png".as_slice()] {
        assert!(inspect(data, data.len() as u64, 0, limits(8), &cancel).is_err());
    }
}

#[test]
fn image_preview_png_cancellation_wins_before_parse_decode_and_io_errors() {
    let cancel = AtomicBool::new(true);
    let error = inspect(b"bad", u64::MAX, u64::MAX, limits(8), &cancel)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("取消") || error.contains("canceled"));
    let bytes = png(
        (1, 1),
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[1, 2, 3, 4],
    );
    cancel.store(false, Ordering::Release);
    let plan = inspect(&bytes, bytes.capacity() as u64, 0, limits(8), &cancel).unwrap();
    cancel.store(true, Ordering::Release);
    let error = decode(plan, &cancel).err().unwrap().to_string();
    assert!(error.contains("取消") || error.contains("canceled"));

    struct CancelAndFail<'a>(&'a AtomicBool);
    impl Read for CancelAndFail<'_> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.0.store(true, Ordering::Release);
            Err(io::Error::other("underlying failure"))
        }
    }
    impl Seek for CancelAndFail<'_> {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            self.0.store(true, Ordering::Release);
            Err(io::Error::other("underlying failure"))
        }
    }
    for seek in [false, true] {
        cancel.store(false, Ordering::Release);
        let mut input = CancelReader {
            inner: CancelAndFail(&cancel),
            cancel: &cancel,
        };
        let error = if seek {
            input.seek(SeekFrom::Start(0)).err().unwrap()
        } else {
            input.read(&mut [0]).err().unwrap()
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!error.to_string().contains("underlying failure"));
    }
}
