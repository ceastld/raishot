use super::*;
use crate::{
    capture::Image,
    image_container_metadata::{FieldSummary, Summary},
};

const VP8_ALPHA: &[u8] = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
const POLICIES: [MetadataPolicy; 3] = [
    MetadataPolicy::Strip,
    MetadataPolicy::ColorOnly,
    MetadataPolicy::ColorAndSupportedExif,
];

fn chunks(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    assert_eq!(le32(&bytes[4..]) as usize + 8, bytes.len());
    let mut cursor = 12;
    let mut result = Vec::new();
    while cursor < bytes.len() {
        let length = le32(&bytes[cursor + 4..]) as usize;
        let end = cursor + 8 + length;
        result.push((
            bytes[cursor..cursor + 4].try_into().unwrap(),
            &bytes[cursor + 8..end],
        ));
        if length & 1 != 0 {
            assert_eq!(bytes[end], 0);
        }
        cursor = end + (length & 1);
    }
    assert_eq!(cursor, bytes.len());
    result
}

fn riff(parts: &[([u8; 4], &[u8])]) -> Vec<u8> {
    let mut result = b"RIFF\0\0\0\0WEBP".to_vec();
    for (kind, payload) in parts {
        result.extend_from_slice(kind);
        result.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        result.extend_from_slice(payload);
        if payload.len() & 1 != 0 {
            result.push(0);
        }
    }
    let length = (result.len() - 8) as u32;
    result[4..8].copy_from_slice(&length.to_le_bytes());
    result
}

fn vp8x(dimensions: (u32, u32), alpha: bool) -> [u8; 10] {
    let mut result = [0; 10];
    result[0] = if alpha { 0x10 } else { 0 };
    result[4..7].copy_from_slice(&(dimensions.0 - 1).to_le_bytes()[..3]);
    result[7..10].copy_from_slice(&(dimensions.1 - 1).to_le_bytes()[..3]);
    result
}

fn pixels(bytes: &[u8]) -> image::RgbaImage {
    image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
        .unwrap()
        .into_rgba8()
}

fn source() -> SourceMetadata {
    // 此源IFD有旧方向6与Make；导出必须重建方向1和当前输出尺寸。
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&2u16.to_le_bytes());
    for (tag, kind, count, value) in [(0x010fu16, 2u16, 8u32, 38u32), (0x0112, 3, 1, 6)] {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&kind.to_le_bytes());
        tiff.extend_from_slice(&count.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(b"Example\0");
    let supported = crate::image_exif::inspect(&tiff, &AtomicBool::new(false)).unwrap();
    assert_eq!(supported.summary().retained_fields, 1);
    SourceMetadata::new(
        ManagedMetadata {
            source: Summary {
                icc: FieldSummary {
                    status: MetadataStatus::Absent,
                    encoded_bytes: 0,
                    decoded_bytes: None,
                },
                exif: FieldSummary {
                    status: MetadataStatus::Present,
                    encoded_bytes: tiff.len() as u64,
                    decoded_bytes: Some(tiff.len() as u64),
                },
                color_model: ColorModel::Rgb,
                bits_per_channel: Some(8),
                orientation: Some(image::metadata::Orientation::Rotate90),
            },
            profile: None,
            color_conversion: ColorConversion::AssumedSrgb,
        },
        None,
    )
    .unwrap()
    .with_exif(Ok(supported))
}

fn tiff_u16(bytes: &[u8], offset: usize) -> u16 {
    let value = bytes[offset..offset + 2].try_into().unwrap();
    if &bytes[..2] == b"II" {
        u16::from_le_bytes(value)
    } else {
        u16::from_be_bytes(value)
    }
}

fn tiff_u32(bytes: &[u8], offset: usize) -> u32 {
    let value = bytes[offset..offset + 4].try_into().unwrap();
    if &bytes[..2] == b"II" {
        u32::from_le_bytes(value)
    } else {
        u32::from_be_bytes(value)
    }
}

fn tiff_field(bytes: &[u8], ifd: usize, tag: u16) -> Option<(u16, u32, &[u8])> {
    for index in 0..tiff_u16(bytes, ifd) as usize {
        let entry = ifd + 2 + index * 12;
        if tiff_u16(bytes, entry) != tag {
            continue;
        }
        let kind = tiff_u16(bytes, entry + 2);
        let count = tiff_u32(bytes, entry + 4);
        let length = count as usize
            * match kind {
                1 | 2 | 7 => 1,
                3 => 2,
                4 | 9 => 4,
                5 | 10 => 8,
                _ => panic!("unexpected TIFF field"),
            };
        let start = if length <= 4 {
            entry + 8
        } else {
            tiff_u32(bytes, entry + 8) as usize
        };
        return Some((kind, count, &bytes[start..start + length]));
    }
    None
}

fn scalar(field: (u16, u32, &[u8])) -> u32 {
    assert_eq!(field.1, 1);
    // encode输出Classic II TIFF；字段级解读独立于保留模块，不回用其inspect。
    match field.0 {
        3 => u16::from_le_bytes(field.2.try_into().unwrap()) as u32,
        4 => u32::from_le_bytes(field.2.try_into().unwrap()),
        _ => panic!("unexpected scalar type"),
    }
}

fn assert_rebuilt_exif(tiff: &[u8], dimensions: (u32, u32)) {
    assert_eq!(&tiff[..4], b"II*\0");
    let first = tiff_u32(tiff, 4) as usize;
    assert_eq!(scalar(tiff_field(tiff, first, 0x0112).unwrap()), 1);
    assert_eq!(tiff_field(tiff, first, 0x010f).unwrap().2, b"Example\0");
    let photo = scalar(tiff_field(tiff, first, 0x8769).unwrap()) as usize;
    assert_eq!(
        scalar(tiff_field(tiff, photo, 0xa002).unwrap()),
        dimensions.0
    );
    assert_eq!(
        scalar(tiff_field(tiff, photo, 0xa003).unwrap()),
        dimensions.1
    );
    assert_eq!(scalar(tiff_field(tiff, photo, 0xa001).unwrap()), 1);
    let next = first + 2 + tiff_u16(tiff, first) as usize * 12;
    assert_eq!(tiff_u32(tiff, next), 0);
}

fn assert_three_policies(encoded: &[u8], dimensions: (u32, u32), alpha: bool) {
    let source = source();
    let before = pixels(encoded);
    assert_eq!(before.dimensions(), dimensions);
    let compressed = chunks(encoded)
        .into_iter()
        .filter(|(kind, _)| matches!(kind, b"VP8 " | b"VP8L" | b"ALPH"))
        .collect::<Vec<_>>();
    assert!(compressed.iter().any(|(kind, _)| kind == b"VP8 "));
    assert_eq!(compressed.iter().any(|(kind, _)| kind == b"ALPH"), alpha);
    for policy in POLICIES {
        let output = apply(
            encoded.to_vec(),
            SaveFormat::Webp,
            dimensions,
            policy,
            Some(&source),
            &AtomicBool::new(false),
        )
        .unwrap();
        let after = pixels(&output);
        assert_eq!(after, before);
        assert_eq!(
            chunks(&output)
                .into_iter()
                .filter(|(kind, _)| matches!(kind, b"VP8 " | b"VP8L" | b"ALPH"))
                .collect::<Vec<_>>(),
            compressed
        );
        if policy == MetadataPolicy::Strip {
            assert_eq!(output, encoded);
            continue;
        }
        let parts = chunks(&output);
        assert_eq!(parts[0].0, *b"VP8X");
        let flags = 0x20
            | if alpha { 0x10 } else { 0 }
            | if policy == MetadataPolicy::ColorAndSupportedExif {
                0x08
            } else {
                0
            };
        assert_eq!(parts[0].1.len(), 10);
        assert_eq!(parts[0].1[0], flags);
        assert_eq!(&parts[0].1[1..4], &[0, 0, 0]);
        assert_eq!(&parts[0].1[4..7], &(dimensions.0 - 1).to_le_bytes()[..3]);
        assert_eq!(&parts[0].1[7..10], &(dimensions.1 - 1).to_le_bytes()[..3]);
        assert_eq!(parts[1], (*b"ICCP", SRGB_PROFILE));
        assert_eq!(parts.iter().filter(|(kind, _)| kind == b"ICCP").count(), 1);
        let exif = parts.iter().find(|(kind, _)| kind == b"EXIF");
        if policy == MetadataPolicy::ColorAndSupportedExif {
            assert_eq!(parts.last().unwrap().0, *b"EXIF");
            assert_rebuilt_exif(exif.unwrap().1, dimensions);
        } else {
            assert!(exif.is_none());
        }
        // 已附元数据产物绝不能被误当作下一次fresh编码，即使选择Strip。
        for policy in POLICIES {
            assert!(apply(
                output.clone(),
                SaveFormat::Webp,
                dimensions,
                policy,
                Some(&source),
                &AtomicBool::new(false)
            )
            .is_err());
        }
    }
}

#[test]
fn image_export_metadata_real_vp8_and_alpha_keep_pixels_payload_and_rebuild_metadata() {
    let parts = chunks(VP8_ALPHA);
    let vp8 = parts.iter().find(|(kind, _)| kind == b"VP8 ").unwrap().1;
    let alpha = parts.iter().find(|(kind, _)| kind == b"ALPH").unwrap().1;
    assert_eq!(alpha[0] & 3, 1, "real compressed ALPH fixture");
    let rgba = pixels(VP8_ALPHA);
    assert_eq!(rgba.dimensions(), (64, 32));
    for (x, y, expected) in [(8, 8, 255), (48, 8, 128), (8, 24, 0), (48, 24, 64)] {
        assert_eq!(rgba.get_pixel(x, y).0[3], expected);
    }
    assert_three_policies(VP8_ALPHA, (64, 32), true);
    let opaque = riff(&[(*b"VP8 ", vp8)]);
    assert!(pixels(&opaque).pixels().all(|pixel| pixel.0[3] == 255));
    assert_three_policies(&opaque, (64, 32), false);
}

#[test]
fn image_export_metadata_rejects_malformed_lossy_webp_before_publish() {
    let source = source();
    let parts = chunks(VP8_ALPHA);
    let vp8 = parts.iter().find(|(kind, _)| kind == b"VP8 ").unwrap().1;
    let x = vp8x((64, 32), true);
    let mut raw_alpha = vec![255; 1 + 64 * 32];
    raw_alpha[0] = 0;
    let valid = riff(&[(*b"VP8X", &x), (*b"ALPH", &raw_alpha), (*b"VP8 ", vp8)]);
    assert!(fresh_webp(&valid, (64, 32)).is_ok());
    let opaque = riff(&[(*b"VP8 ", vp8)]);
    let mut malformed = Vec::new();
    for index in [0, 8] {
        let mut bytes = opaque.clone();
        bytes[index] ^= 1;
        malformed.push(bytes);
    }
    let mut bytes = opaque.clone();
    bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
    malformed.push(bytes);
    let mut bytes = valid.clone();
    bytes[16..20].copy_from_slice(&9u32.to_le_bytes());
    malformed.push(bytes);
    for (offset, value) in [(20, 0x30), (20, 0x12), (20, 0), (21, 1), (24, 64)] {
        let mut bytes = valid.clone();
        bytes[offset] = value;
        malformed.push(bytes);
    }
    malformed.push(riff(&[(*b"ALPH", &raw_alpha), (*b"VP8 ", vp8)]));
    malformed.push(riff(&[
        (*b"VP8X", &x),
        (*b"VP8 ", vp8),
        (*b"ALPH", &raw_alpha),
    ]));
    malformed.push(riff(&[
        (*b"VP8X", &x),
        (*b"ALPH", &raw_alpha),
        (*b"ALPH", &raw_alpha),
        (*b"VP8 ", vp8),
    ]));
    malformed.push(riff(&[
        (*b"VP8X", &x),
        (*b"ALPH", &raw_alpha[..raw_alpha.len() - 1]),
        (*b"VP8 ", vp8),
    ]));
    for flag in [2, 0x20, 0x40] {
        let mut alpha = raw_alpha.clone();
        alpha[0] = flag;
        malformed.push(riff(&[(*b"VP8X", &x), (*b"ALPH", &alpha), (*b"VP8 ", vp8)]));
    }
    malformed.push(riff(&[(*b"VP8X", &x), (*b"ALPH", &[1]), (*b"VP8 ", vp8)]));
    let mut bytes = valid.clone();
    bytes[30 + 8 + raw_alpha.len()] = 1;
    malformed.push(bytes);
    let mut bytes = valid.clone();
    bytes[34..38].copy_from_slice(&u32::MAX.to_le_bytes());
    malformed.push(bytes);
    for (index, value) in [
        (0, vp8[0] | 1),
        (0, (vp8[0] & !0x0e) | 8),
        (0, vp8[0] & !0x10),
        (3, 0),
        (7, vp8[7] | 0x40),
        (9, vp8[9] | 0x40),
        (6, 0),
    ] {
        let mut bad_vp8 = vp8.to_vec();
        bad_vp8[index] = value;
        malformed.push(riff(&[(*b"VP8 ", &bad_vp8)]));
    }
    for partition in [0, 0x7ffff] {
        let mut bad_vp8 = vp8.to_vec();
        let tag = (le32(&[vp8[0], vp8[1], vp8[2], 0]) & 31) | (partition << 5);
        bad_vp8[..3].copy_from_slice(&tag.to_le_bytes()[..3]);
        malformed.push(riff(&[(*b"VP8 ", &bad_vp8)]));
    }
    malformed.push(riff(&[(*b"VP8 ", &vp8[..9])]));
    malformed.push(riff(&[(*b"VP8 ", vp8), (*b"JUNK", &[0])]));
    for (index, bytes) in malformed.into_iter().enumerate() {
        for policy in POLICIES {
            assert!(
                apply(
                    bytes.clone(),
                    SaveFormat::Webp,
                    (64, 32),
                    policy,
                    Some(&source),
                    &AtomicBool::new(false)
                )
                .is_err(),
                "malformed case {index}, {policy:?}"
            );
        }
    }
    assert!(apply(
        opaque.clone(),
        SaveFormat::Webp,
        (32, 64),
        MetadataPolicy::Strip,
        None,
        &AtomicBool::new(false)
    )
    .is_err());
    assert!(apply(
        opaque,
        SaveFormat::Webp,
        (64, 32),
        MetadataPolicy::Strip,
        None,
        &AtomicBool::new(true)
    )
    .is_err());
}

#[test]
fn image_export_metadata_native_lossy_producer_outputs_survive_all_policies() {
    for alpha in [false, true] {
        let mut image = Image {
            width: 16,
            height: 8,
            bgra: Vec::new(),
            readback_ms: 0.0,
        };
        for y in 0..image.height {
            for x in 0..image.width {
                let a = if alpha {
                    [0, 64, 128, 255][((x >= 8) as usize) + 2 * ((y >= 4) as usize)]
                } else {
                    255
                };
                image.bgra.extend_from_slice(&[
                    (x * 13 + y * 3) as u8,
                    (y * 29 + x) as u8,
                    (255 - x * 9 - y * 5) as u8,
                    a,
                ]);
            }
        }
        for quality in [1, 50, 100] {
            let encoded = crate::image_webp_encode::encode_lossy_bgra(
                &image,
                quality,
                0,
                1024 * 1024,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert!(
                matches!(fresh_webp(&encoded, (16, 8)).unwrap(), Fresh::Webp { alpha: value, .. } if value == alpha)
            );
            let decoded = pixels(&encoded);
            assert_eq!(decoded.dimensions(), (16, 8));
            for (pixel, source) in decoded.pixels().zip(image.bgra.chunks_exact(4)) {
                assert_eq!(pixel.0[3], source[3]);
            }
            assert_three_policies(&encoded, (16, 8), alpha);
        }
    }
}
