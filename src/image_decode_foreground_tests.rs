use super::*;
use image::ImageFormat;

fn pnm() -> Vec<u8> {
    let mut bytes = b"P6\n2 1\n255\n".to_vec();
    bytes.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
    bytes
}

fn farbfeld() -> Vec<u8> {
    let mut bytes = b"farbfeld".to_vec();
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    for value in [17u16, 33, 71, 128, 220, 5, 13, 0] {
        bytes.extend_from_slice(&(value * 257).to_be_bytes());
    }
    bytes
}

fn qoi() -> Vec<u8> {
    let mut bytes = b"qoif".to_vec();
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&[4, 0]);
    bytes.extend_from_slice(&[255, 17, 33, 71, 128, 255, 220, 5, 13, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
    bytes
}

fn dds() -> Vec<u8> {
    let mut bytes = b"DDS ".to_vec();
    for value in [124u32, 0x1007, 4, 4, 8, 0, 0] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[0; 44]);
    bytes.extend_from_slice(&32u32.to_le_bytes());
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(b"DXT1");
    bytes.extend_from_slice(&[0; 20]);
    bytes.extend_from_slice(&0x1000u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    assert_eq!(bytes.len(), 128);
    bytes.extend_from_slice(&0xf800u16.to_le_bytes());
    bytes.extend_from_slice(&0x001fu16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes
}

fn hdr() -> Vec<u8> {
    let mut bytes = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\n".to_vec();
    bytes.extend_from_slice(&[128, 0, 0, 129, 0, 0, 128, 129]);
    bytes
}

#[test]
fn image_decode_foreground_keeps_existing_magic_formats_and_pixels() {
    let red_blue = vec![0, 0, 255, 255, 255, 0, 0, 255];
    let translucent = vec![71, 33, 17, 128, 13, 5, 220, 0];
    let cases = [
        (ImageFormat::Pnm, pnm(), (2, 1), red_blue.clone()),
        (
            ImageFormat::Farbfeld,
            farbfeld(),
            (2, 1),
            translucent.clone(),
        ),
        (ImageFormat::Qoi, qoi(), (2, 1), translucent),
        (ImageFormat::Dds, dds(), (4, 4), [0, 0, 255, 255].repeat(16)),
        (ImageFormat::Hdr, hdr(), (2, 1), red_blue),
    ];
    for (format, bytes, dimensions, expected) in cases {
        let ordinary = decode_bytes(&bytes).unwrap();
        let foreground = decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            MAX_DECODE_BYTES,
            &NEVER_CANCELLED,
            DecodeContent::File,
            DecodeBudget::Foreground(32 * 1024 * 1024),
        )
        .unwrap();
        assert_eq!(ordinary.metadata.format, format, "{format:?}");
        assert_eq!(foreground.metadata.format, format, "{format:?}");
        assert_eq!(foreground.metadata.display_dimensions, dimensions);
        assert_eq!(foreground.metadata.encoded_dimensions, Some(dimensions));
        assert_eq!(ordinary.image.bgra, expected, "{format:?}");
        assert_eq!(foreground.image.bgra, ordinary.image.bgra, "{format:?}");
        assert_eq!(
            foreground.metadata.decoded_color,
            ordinary.metadata.decoded_color
        );
        assert_eq!(foreground.warning, ordinary.warning);
        // 主图兼容范围不会自动变成缩略/严格预取的工作集保证。
        for budget in [DecodeBudget::Shared(0), DecodeBudget::StrictPrefetch(0)] {
            assert!(
                decode_bytes_inner_with_budget(
                    &bytes,
                    bytes.capacity() as u64,
                    MAX_DECODE_BYTES,
                    &NEVER_CANCELLED,
                    DecodeContent::File,
                    budget,
                )
                .is_err(),
                "{format:?}"
            );
        }
        assert!(decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            MAX_DECODE_BYTES,
            &NEVER_CANCELLED,
            DecodeContent::File,
            DecodeBudget::Foreground(MAX_DECODE_BYTES),
        )
        .is_err());
    }
}

#[test]
fn image_decode_foreground_exhausted_resident_fails_before_path_access() {
    let error = decode_path_with_foreground_budget_cancellable(
        Path::new(""),
        MAX_DECODE_BYTES,
        MAX_DECODE_BYTES,
        &NEVER_CANCELLED,
    )
    .err()
    .expect("resident budget must fail before path validation");
    assert_eq!(
        error.to_string(),
        tr!(
            "图片来源与读取工作区超过256MiB预算",
            "Image source and read workspace exceed the 256 MiB budget"
        )
    );
}

#[test]
fn image_decode_foreground_cancellation_precedes_budget_and_format_errors() {
    let cancel = AtomicBool::new(true);
    let path_error = decode_path_with_foreground_budget_cancellable(
        Path::new(""),
        MAX_DECODE_BYTES,
        MAX_DECODE_BYTES,
        &cancel,
    )
    .err()
    .unwrap();
    let bytes_error = decode_bytes_inner_with_budget(
        b"not an image",
        12,
        MAX_DECODE_BYTES,
        &cancel,
        DecodeContent::File,
        DecodeBudget::Foreground(MAX_DECODE_BYTES),
    )
    .err()
    .unwrap();
    for error in [path_error, bytes_error] {
        assert_eq!(
            error.to_string(),
            tr!("图片读取已取消", "Image reading canceled")
        );
    }
}
