use super::*;
use std::cell::Cell;

const RGB: [[u8; 3]; 6] = [
    [10, 30, 70],
    [10, 31, 71],
    [10, 32, 72],
    [200, 150, 100],
    [201, 150, 101],
    [202, 150, 102],
];

struct Fixture {
    bytes: Vec<u8>,
    compression: usize,
    table: usize,
    layer_count: Option<usize>,
}

fn u16_to(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}
fn u32_to(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn resource(id: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = b"8BIM".to_vec();
    u16_to(&mut bytes, id);
    bytes.extend_from_slice(b"\x06opaque\0"); // Pascal名称连同长度填充到偶数。
    u32_to(&mut bytes, payload.len() as u32);
    bytes.extend_from_slice(payload);
    if payload.len() % 2 != 0 {
        bytes.push(0);
    }
    bytes
}

fn layer() -> (Vec<u8>, usize) {
    let mut info = Vec::new();
    u16_to(&mut info, 1);
    // 一条完整的不透明普通图层记录；其像素故意不同于保存合并图。
    for coordinate in [0, 0, 2, 3] {
        u32_to(&mut info, coordinate);
    }
    u16_to(&mut info, 3);
    for channel in 0..3 {
        u16_to(&mut info, channel);
        u32_to(&mut info, 8); // Raw压缩字与3×2通道样本。
    }
    info.extend_from_slice(b"8BIMnorm");
    info.extend_from_slice(&[255, 0, 0, 0]);
    u32_to(&mut info, 12);
    u32_to(&mut info, 0); // 图层蒙版数据。
    u32_to(&mut info, 0); // 混合范围。
    info.extend_from_slice(b"\x01L\0\0"); // Pascal名称填充到四字节。
    for _ in 0..3 {
        u16_to(&mut info, 0);
        info.extend_from_slice(&[222; 6]);
    }
    if info.len() % 2 != 0 {
        info.push(0);
    }
    let mut section = Vec::new();
    u32_to(&mut section, info.len() as u32);
    let count_offset = section.len();
    section.extend_from_slice(&info);
    u32_to(&mut section, 0); // 全局蒙版。
    (section, count_offset)
}

fn fixture(rle: bool, resources: &[u8], with_layer: bool) -> Fixture {
    let mut bytes = b"8BPS".to_vec();
    u16_to(&mut bytes, 1);
    bytes.extend_from_slice(&[0; 6]);
    u16_to(&mut bytes, 3);
    u32_to(&mut bytes, 2);
    u32_to(&mut bytes, 3);
    u16_to(&mut bytes, 8);
    u16_to(&mut bytes, 3);
    u32_to(&mut bytes, 0);
    u32_to(&mut bytes, resources.len() as u32);
    bytes.extend_from_slice(resources);
    let layer_count = if with_layer {
        let (section, count) = layer();
        u32_to(&mut bytes, section.len() as u32);
        let count = bytes.len() + count;
        bytes.extend_from_slice(&section);
        Some(count)
    } else {
        u32_to(&mut bytes, 0);
        None
    };
    let compression = bytes.len();
    u16_to(&mut bytes, rle as u16);
    let table = bytes.len();
    if rle {
        let mut rows = Vec::new();
        for channel in 0..3 {
            for row in RGB.chunks_exact(3) {
                let mut encoded = vec![0x80]; // 合法NoOp也可出现在行首和行尾。
                if row.iter().all(|pixel| pixel[channel] == row[0][channel]) {
                    encoded.extend_from_slice(&[0xfe, row[0][channel]]);
                } else {
                    encoded.push(2);
                    encoded.extend(row.iter().map(|pixel| pixel[channel]));
                }
                encoded.push(0x80);
                rows.push(encoded);
            }
        }
        for row in &rows {
            u16_to(&mut bytes, row.len() as u16);
        }
        for row in rows {
            bytes.extend_from_slice(&row);
        }
    } else {
        for channel in 0..3 {
            bytes.extend(RGB.iter().map(|pixel| pixel[channel]));
        }
    }
    Fixture {
        bytes,
        compression,
        table,
        layer_count,
    }
}

fn read(bytes: &Vec<u8>) -> Result<Decoded> {
    decode(
        bytes,
        bytes.capacity() as u64,
        0,
        crate::image_decode::MAX_DECODE_BYTES,
        &AtomicBool::new(false),
    )
}

fn expected() -> Vec<u8> {
    RGB.iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], 255])
        .collect()
}

#[test]
fn image_decode_psd_composite_entry_keeps_source_facts_and_unmapped_pixels() {
    use crate::{
        image_color_management::ColorConversion,
        image_container_metadata::{ColorModel, MetadataStatus},
        image_decode::{DecodeBudget, DecodeContent},
        image_metadata::SourceFormat,
    };
    for rle in [false, true] {
        let fixture = fixture(rle, &resource(0xf000, &[11, 22, 33]), true);
        for budget in [
            DecodeBudget::Ordinary,
            DecodeBudget::Shared(1024),
            DecodeBudget::Foreground(1024),
        ] {
            let decoded = crate::image_decode::decode_bytes_inner_with_budget(
                &fixture.bytes,
                fixture.bytes.capacity() as u64,
                24,
                &AtomicBool::new(false),
                DecodeContent::File,
                budget,
            )
            .unwrap();
            assert_eq!(decoded.image.bgra, expected());
            assert_eq!(decoded.metadata.format, SourceFormat::Psd);
            assert_eq!(decoded.metadata.format_name(), "PSD");
            assert_eq!(decoded.metadata.encoded_dimensions, Some((3, 2)));
            assert_eq!(decoded.metadata.display_dimensions, (3, 2));
            assert_eq!(
                decoded.metadata.encoded_bytes,
                Some(fixture.bytes.len() as u64)
            );
            assert_eq!(decoded.metadata.orientation, None);
            assert_eq!(decoded.metadata.decoded_color, Some(image::ColorType::Rgb8));
            let details = decoded.metadata.details.unwrap();
            assert_eq!(details.color_conversion, ColorConversion::SignalUnmapped);
            assert_eq!(details.profile, None);
            assert_eq!(details.source.color_model, ColorModel::Rgb);
            assert_eq!(details.source.bits_per_channel, Some(8));
            assert_eq!(details.source.icc.status, MetadataStatus::Absent);
            assert_eq!(details.source.exif.status, MetadataStatus::Absent);
            assert_eq!(details.source.orientation, None);
            let sidecar = decoded.metadata_source.unwrap();
            assert_eq!(sidecar.facts(), details);
            assert_eq!(sidecar.retained_bytes(), 0);
            assert!(decoded.warning.is_some_and(|warning| !warning.is_empty()));
        }
    }
}

#[test]
fn image_decode_psd_entry_counts_source_capacity_and_external_resident_once() {
    use crate::image_decode::{DecodeBudget, DecodeContent, MAX_DECODE_BYTES};
    let mut sample = fixture(true, &[], true);
    sample.bytes.reserve_exact(4096 - sample.bytes.len());
    assert!(sample.bytes.capacity() >= 4096);
    let decode = |budget, max_output| {
        crate::image_decode::decode_bytes_inner_with_budget(
            &sample.bytes,
            sample.bytes.capacity() as u64,
            max_output,
            &AtomicBool::new(false),
            DecodeContent::File,
            budget,
        )
    };
    let output_capacity = decode(DecodeBudget::Foreground(0), 24)
        .unwrap()
        .image
        .bgra
        .capacity() as u64;
    let exact_r =
        MAX_DECODE_BYTES - sample.bytes.capacity() as u64 - output_capacity - CONTROL_BYTES;
    for budget in [
        DecodeBudget::Shared(exact_r),
        DecodeBudget::Foreground(exact_r),
    ] {
        assert_eq!(decode(budget, 24).unwrap().image.bgra, expected());
    }
    assert!(decode(DecodeBudget::Foreground(exact_r + 1), 24).is_err());
    assert!(decode(DecodeBudget::Shared(u64::MAX), 24).is_err());
    assert!(decode(DecodeBudget::Foreground(0), 23).is_err());
    assert!(decode(DecodeBudget::StrictPrefetch(0), 24).is_err());
    // 颜色配置不会在统一入口被静默剥去再交给reader。
    let tagged = fixture(false, &resource(1039, &[1, 2, 3]), false);
    assert!(crate::image_decode::decode_bytes_inner_with_budget(
        &tagged.bytes,
        tagged.bytes.capacity() as u64,
        24,
        &AtomicBool::new(false),
        DecodeContent::File,
        DecodeBudget::Foreground(0),
    )
    .is_err());
}

#[test]
fn image_psd_version_info_rejects_declared_missing_real_composite() {
    let mut version_info = Vec::new();
    u32_to(&mut version_info, 7); // 两个版本字段不限制为某个实现的固定值。
    version_info.push(1);
    for name in ["Writer", "Reader"] {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        u32_to(&mut version_info, utf16.len() as u32);
        for unit in utf16 {
            u16_to(&mut version_info, unit);
        }
    }
    u32_to(&mut version_info, 9);
    let valid = resource(1057, &version_info);
    let mut missing_info = version_info.clone();
    missing_info[4] = 0;
    let missing = resource(1057, &missing_info);
    for rle in [false, true] {
        assert_eq!(
            read(&fixture(rle, &valid, true).bytes).unwrap().image.bgra,
            expected()
        );
        for resources in [
            missing.clone(),
            [valid.as_slice(), missing.as_slice()].concat(),
            [missing.as_slice(), valid.as_slice()].concat(),
        ] {
            let sample = fixture(rle, &resources, true);
            assert_eq!(
                read(&sample.bytes).err(),
                Some(PsdError::Unsupported(UnsupportedFeature::NoMergedComposite))
            );
            // 统一主图入口同样拒绝占位数据，不能转到其它decoder或交出像素。
            assert!(crate::image_decode::decode_bytes_inner_with_budget(
                &sample.bytes,
                sample.bytes.capacity() as u64,
                24,
                &AtomicBool::new(false),
                crate::image_decode::DecodeContent::File,
                crate::image_decode::DecodeBudget::Foreground(0),
            )
            .is_err());
        }
        for length in 0..5 {
            assert_eq!(
                read(&fixture(rle, &resource(1057, &version_info[..length]), true).bytes).err(),
                Some(PsdError::Invalid)
            );
        }
    }
}

#[test]
fn image_psd_probe_raw_and_packbits_non_square_composites_match_all_pixels() {
    for rle in [false, true] {
        let fixture = fixture(rle, &[], false);
        let decoded = read(&fixture.bytes).unwrap();
        assert_eq!((decoded.image.width, decoded.image.height), (3, 2));
        assert_eq!(decoded.image.bgra, expected());
        assert_eq!(decoded.color, ColorStatus::Unmanaged);
        assert_eq!(decoded.image.readback_ms, 0.);
    }
}

#[test]
fn image_psd_probe_borrows_unknown_resources_and_skips_complete_positive_layer() {
    let unknown = resource(0xf000, &[11, 22, 33]);
    let valid = fixture(true, &unknown, true);
    assert_eq!(read(&valid.bytes).unwrap().image.bgra, expected());
    // 未知资源借用跳过；不复制或误读图层中故意不同的222像素。
    assert!(valid.layer_count.is_some());
    for id in [1039, 1058, 1059, 1060] {
        let fixture = fixture(false, &resource(id, &[1, 2, 3]), true);
        assert_eq!(
            read(&fixture.bytes).err(),
            Some(PsdError::Unsupported(UnsupportedFeature::Metadata))
        );
    }
    let mut bad = unknown;
    bad[0] = b'X';
    assert_eq!(
        read(&fixture(false, &bad, false).bytes).err(),
        Some(PsdError::Invalid)
    );
    let valid_resource = resource(0xf000, &[11, 22, 33]);
    let truncated_padding = &valid_resource[..valid_resource.len() - 1];
    assert!(read(&fixture(false, truncated_padding, false).bytes).is_err());
}

#[test]
fn image_psd_probe_rejects_unhandled_headers_transparency_and_compression() {
    // 这些变更只证明头部拒收边界，不宣称是完整的其它格式能力样本。
    for (offset, value, feature) in [
        (4, 2u16, UnsupportedFeature::Psb),
        (12, 4, UnsupportedFeature::Channels),
        (22, 16, UnsupportedFeature::Depth),
        (24, 4, UnsupportedFeature::ColorMode),
    ] {
        let mut fixture = fixture(false, &[], false);
        fixture.bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
        assert_eq!(
            read(&fixture.bytes).err(),
            Some(PsdError::Unsupported(feature))
        );
    }
    let mut negative = fixture(false, &[], true);
    let count = negative.layer_count.unwrap();
    negative.bytes[count..count + 2].copy_from_slice(&(-1i16).to_be_bytes());
    assert_eq!(
        read(&negative.bytes).err(),
        Some(PsdError::Unsupported(UnsupportedFeature::LayerTransparency))
    );
    for compression in [2u16, 3] {
        let mut fixture = fixture(false, &[], false);
        fixture.bytes[fixture.compression..fixture.compression + 2]
            .copy_from_slice(&compression.to_be_bytes());
        assert_eq!(
            read(&fixture.bytes).err(),
            Some(PsdError::Unsupported(UnsupportedFeature::Compression))
        );
    }
    let mut color_data = fixture(false, &[], false);
    color_data.bytes[26..30].copy_from_slice(&2u32.to_be_bytes());
    drop(color_data.bytes.splice(30..30, [0, 0]));
    assert_eq!(
        read(&color_data.bytes).err(),
        Some(PsdError::Unsupported(UnsupportedFeature::ColorData))
    );
}

#[test]
fn image_psd_probe_real_capacities_exact_budget_and_cancellation_are_checked() {
    let mut fixture = fixture(true, &[], false);
    fixture.bytes.reserve(97);
    let source = fixture.bytes.capacity() as u64;
    let decoded = read(&fixture.bytes).unwrap();
    let output = decoded.image.bgra.capacity() as u64;
    assert_eq!(decoded.image.bgra.len(), 24);
    assert!(output >= 24);
    drop(decoded);
    let resident = crate::image_decode::MAX_DECODE_BYTES - source - output - CONTROL_BYTES;
    fits(source, resident, output).unwrap();
    assert_eq!(fits(source, resident + 1, output), Err(PsdError::Budget));
    let decoded = decode(
        &fixture.bytes,
        source,
        resident,
        24,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(decoded.image.bgra, expected());
    assert!(
        source + resident + decoded.image.bgra.capacity() as u64 + CONTROL_BYTES
            <= crate::image_decode::MAX_DECODE_BYTES
    );
    drop(decoded);
    assert_eq!(
        decode(
            &fixture.bytes,
            source,
            resident + 1,
            24,
            &AtomicBool::new(false)
        )
        .err(),
        Some(PsdError::Budget)
    );
    assert_eq!(
        decode(&fixture.bytes, source, 0, 23, &AtomicBool::new(false)).err(),
        Some(PsdError::Budget)
    );
    assert_eq!(
        decode(
            &fixture.bytes,
            fixture.bytes.len() as u64 - 1,
            0,
            24,
            &AtomicBool::new(false)
        )
        .err(),
        Some(PsdError::Invalid)
    );
    assert_eq!(
        decode(&[], 0, u64::MAX, 0, &AtomicBool::new(true)).err(),
        Some(PsdError::Cancelled)
    );
    let calls = Cell::new(0usize);
    let check = || {
        calls.set(calls.get() + 1);
        if calls.get() == 2 {
            Err(PsdError::Cancelled)
        } else {
            Ok(())
        }
    };
    let mut output = [255; 12];
    assert_eq!(
        packbits(&[0x80, 0x80, 2, 1, 2, 3], 3, &mut output, 2, &check),
        Err(PsdError::Cancelled)
    );
    assert_eq!(calls.get(), 2);
    assert_eq!(output, [255; 12]);
    let mut large_header = fixture.bytes.clone();
    large_header[14..18].copy_from_slice(&30_000u32.to_be_bytes());
    large_header[18..22].copy_from_slice(&30_000u32.to_be_bytes());
    assert_eq!(read(&large_header).err(), Some(PsdError::Budget));
}

#[test]
fn image_psd_probe_rejects_bad_lengths_and_packbits_without_partial_output() {
    for rle in [false, true] {
        let fixture = fixture(rle, &[], true);
        let mut short = fixture.bytes.clone();
        short.pop();
        assert_eq!(read(&short).err(), Some(PsdError::Invalid));
        let mut extra = fixture.bytes.clone();
        extra.push(0);
        assert_eq!(read(&extra).err(), Some(PsdError::Invalid));
    }
    let mut rle = fixture(true, &[], false);
    rle.bytes[rle.table..rle.table + 2].copy_from_slice(&u16::MAX.to_be_bytes());
    assert_eq!(read(&rle.bytes).err(), Some(PsdError::Invalid));
    let mut bad_layer = fixture(false, &[], true);
    let count = bad_layer.layer_count.unwrap();
    bad_layer.bytes[count - 4..count].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(read(&bad_layer.bytes).err(), Some(PsdError::Invalid));
    for malformed in [
        &[2, 1, 2][..],
        &[3, 1, 2, 3, 4],
        &[0xfd, 7],
        &[0xfe],
        &[0, 1],
        &[2, 1, 2, 3, 0, 4],
    ] {
        let mut output = [255; 12];
        assert_eq!(
            packbits(malformed, 3, &mut output, 2, &|| Ok(())),
            Err(PsdError::Invalid)
        );
    }
    let mut output = [255; 12];
    packbits(&[0x80, 2, 1, 2, 3, 0x80], 3, &mut output, 2, &|| Ok(())).unwrap();
    assert_eq!(
        output,
        [255, 255, 1, 255, 255, 255, 2, 255, 255, 255, 3, 255]
    );
}
