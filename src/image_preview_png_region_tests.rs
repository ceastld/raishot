use super::*;
use crate::image_container_metadata::ColorModel;
use std::{io::Write, sync::Arc};

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn limits(max_side: u32, max_pixels: u64) -> PreviewLimits {
    PreviewLimits {
        max_side,
        max_pixels,
    }
}

fn png(dimensions: (u32, u32), color: png::ColorType, depth: png::BitDepth, raw: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, dimensions.0, dimensions.1);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(raw).unwrap();
        writer.finish().unwrap();
    }
    bytes
}

fn with_exif(bytes: &[u8], orientation: u8) -> Vec<u8> {
    let mut tiff = b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00".to_vec();
    tiff.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0, 0]);
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&(tiff.len() as u32).to_be_bytes());
    chunk.extend_from_slice(b"eXIf");
    chunk.extend_from_slice(&tiff);
    let mut crc = !0u32;
    for byte in &chunk[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32 & 0u32.wrapping_sub(crc & 1));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    let mut result = bytes[..33].to_vec();
    result.extend_from_slice(&chunk);
    result.extend_from_slice(&bytes[33..]);
    result
}

fn freeze(bytes: Vec<u8>) -> Arc<FrozenPreviewSource> {
    let (source, overview) =
        FrozenPreviewSource::open(bytes, 0, PreviewLimits::default(), &AtomicBool::new(false))
            .unwrap();
    // ROI的R=0不隐匿仍持有的总览像素；只保留单个冻结来源。
    drop(overview);
    source
}

fn region(source: &FrozenPreviewSource, rect: Rect, limits: PreviewLimits) -> RegionDecoded {
    let cancel = AtomicBool::new(false);
    let plan = inspect_region(source, rect, limits, 0, &cancel).unwrap();
    decode_region(plan, &cancel).unwrap()
}

#[test]
fn image_preview_png_region_real_frozen_source_keeps_all_orientation_crops() {
    let mut raw = Vec::new();
    for id in 0..165u8 {
        raw.extend_from_slice(&[id, 30 + id, 220 - id, [0, 128, 255, 64][id as usize % 4]]);
    }
    let bytes = png((15, 11), png::ColorType::Rgba, png::BitDepth::Eight, &raw);
    // 同编码ROI(2,1,8,6)实际采样x=3/6/8、y=2/5。每项是归正请求和
    // 最终局部栅格中编码像素的literal编号，不能在反射后重新等分归正ROI。
    let cases: [(Rect, [u8; 6]); 8] = [
        (rect(2, 1, 8, 6), [33, 36, 38, 78, 81, 83]),
        (rect(5, 1, 8, 6), [38, 36, 33, 83, 81, 78]),
        (rect(5, 4, 8, 6), [83, 81, 78, 38, 36, 33]),
        (rect(2, 4, 8, 6), [78, 81, 83, 33, 36, 38]),
        (rect(1, 2, 6, 8), [33, 78, 36, 81, 38, 83]),
        (rect(4, 2, 6, 8), [78, 33, 81, 36, 83, 38]),
        (rect(4, 5, 6, 8), [83, 38, 81, 36, 78, 33]),
        (rect(1, 5, 6, 8), [38, 83, 36, 81, 33, 78]),
    ];
    for (index, (requested, ids)) in cases.into_iter().enumerate() {
        let source = freeze(with_exif(&bytes, index as u8 + 1));
        let decoded = region(&source, requested, limits(3, 6));
        assert_eq!(decoded.geometry.encoded_dimensions(), (15, 11));
        assert_eq!(decoded.geometry.encoded_region(), rect(2, 1, 8, 6));
        assert_eq!(decoded.geometry.normalized_source_region(), requested);
        assert_eq!(decoded.geometry.sampled_encoded_dimensions(), (3, 2));
        assert_eq!(
            (decoded.image.width, decoded.image.height),
            if index < 4 { (3, 2) } else { (2, 3) }
        );
        assert_eq!(decoded.source, source.source());
        assert_eq!(
            decoded.source.orientation.unwrap().to_exif(),
            index as u8 + 1
        );
        assert_eq!(decoded.sampling, PreviewSampling::PixelCenterNearest);
        assert_eq!(decoded.color, PreviewColorStatus::NotColorManaged);
        assert_eq!(decoded.image.bgra.capacity(), 24);
        for (i, id) in ids.into_iter().enumerate() {
            assert_eq!(
                &decoded.image.bgra[i * 4..i * 4 + 4],
                &[220 - id, 30 + id, id, [0, 128, 255, 64][id as usize % 4]]
            );
        }
    }
}

#[test]
fn image_preview_png_region_sixteen_bit_gray_and_palette_keep_alpha_and_source_facts() {
    let mut raw = Vec::new();
    for id in 0..15u16 {
        raw.extend_from_slice(&(1000 + 4000 * id).to_be_bytes());
        raw.extend_from_slice(&[0u16, 32768, 65535][id as usize % 3].to_be_bytes());
    }
    let source = freeze(png(
        (5, 3),
        png::ColorType::GrayscaleAlpha,
        png::BitDepth::Sixteen,
        &raw,
    ));
    let decoded = region(&source, rect(1, 1, 3, 2), limits(3, 6));
    assert_eq!((decoded.image.width, decoded.image.height), (3, 2));
    assert_eq!(decoded.source.color_model, ColorModel::Gray);
    assert_eq!(decoded.source.bits_per_channel, Some(16));
    assert_eq!(decoded.color, PreviewColorStatus::NotColorManaged);
    assert_eq!(
        decoded.image.bgra,
        [
            97, 97, 97, 0, 113, 113, 113, 128, 128, 128, 128, 255, 175, 175, 175, 255, 191, 191,
            191, 0, 206, 206, 206, 128
        ]
    );

    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 5, 2);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Two);
        encoder.set_palette(vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 20, 30]);
        encoder.set_trns(vec![0, 64, 128, 255]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0x1b, 0x40, 0xe4, 0x80]).unwrap();
        writer.finish().unwrap();
    }
    let source = freeze(bytes);
    let decoded = region(&source, rect(1, 0, 3, 2), limits(2, 4));
    assert_eq!(decoded.geometry.sampled_encoded_dimensions(), (2, 1));
    assert_eq!(decoded.source.color_model, ColorModel::Palette);
    assert_eq!(decoded.source.bits_per_channel, Some(2));
    assert_eq!(decoded.source.orientation, None);
    assert_eq!(decoded.image.bgra, [255, 0, 0, 128, 0, 0, 255, 0]);
}

fn wide_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 200_000, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        {
            let mut stream = writer.stream_writer().unwrap();
            let block = [7, 31, 91, 128].repeat(1024);
            let mut remaining = 200_000usize;
            while remaining != 0 {
                let count = remaining.min(1024);
                stream.write_all(&block[..count * 4]).unwrap();
                remaining -= count;
            }
            stream.finish().unwrap();
        }
        writer.finish().unwrap();
    }
    bytes
}

#[test]
fn image_preview_png_region_full_source_row_budget_and_lease_are_charged_once() {
    let source = freeze(wide_png());
    let cancel = AtomicBool::new(false);
    let tiny = rect(99_999, 0, 1, 1);
    let first = inspect_region(&source, tiny, limits(1, 1), 0, &cancel).unwrap();
    let workspace = first.workspace();
    assert_eq!(first.geometry().encoded_region(), tiny);
    assert_eq!(first.source(), source.source());
    assert!(workspace.decode_bytes > METADATA_WORKSPACE);
    drop(first);
    let whole = inspect_region(&source, Rect::full(200_000, 1), limits(1, 1), 0, &cancel).unwrap();
    // 两者输出同为1像素，但ROI只剩1列不能令完整200000列的行/私有缓冲消失。
    assert_eq!(whole.workspace(), workspace);
    drop(whole);
    let resident = MAX_BYTES - source.retained_bytes() - workspace.peak_bytes();
    let exact = inspect_region(&source, tiny, limits(1, 1), resident, &cancel).unwrap();
    assert_eq!(
        decode_region(exact, &cancel).unwrap().image.bgra,
        [91, 31, 7, 128]
    );
    assert!(inspect_region(&source, tiny, limits(1, 1), resident + 1, &cancel).is_err());
    assert!(inspect_region(&source, tiny, limits(1, 1), u64::MAX, &cancel).is_err());
    assert!(inspect_region(&source, rect(200_000, 0, 1, 1), limits(1, 1), 0, &cancel).is_err());
    assert!(inspect_region(&source, tiny, limits(1, 0), 0, &cancel).is_err());

    let plan = inspect_region(&source, tiny, limits(1, 1), 0, &cancel).unwrap();
    cancel.store(true, Ordering::Release);
    let error = decode_region(plan, &cancel).err().unwrap().to_string();
    assert!(error.contains("取消") || error.contains("canceled"));
    let error = inspect_region(
        &source,
        rect(u32::MAX, 0, 2, 1),
        limits(1, 1),
        u64::MAX,
        &cancel,
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("取消") || error.contains("canceled"));
}
