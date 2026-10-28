use super::*;

fn channel(sample_type: u8) -> Channel<'static> {
    Channel {
        name: b"Q",
        sample_type,
        sampling_x: 1,
        sampling_y: 1,
    }
}
fn geometry(width: u32, height: u32) -> Geometry {
    Geometry {
        width,
        height,
        origin_y: 0,
    }
}
fn stored(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    if bytes.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 255, 255]);
    }
    let count = bytes.chunks(65535).len();
    for (index, chunk) in bytes.chunks(65535).enumerate() {
        out.push(u8::from(index + 1 == count));
        let length = chunk.len() as u16;
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}
fn dwa(unknown: &[u8], ac: &[u8], dc: &[u8], tokens: &[u8], goal: u64) -> Vec<u8> {
    let sections = [
        stored(unknown),
        if ac.is_empty() {
            Vec::new()
        } else {
            stored(ac)
        },
        if dc.is_empty() {
            Vec::new()
        } else {
            stored(dc)
        },
        if goal == 0 {
            Vec::new()
        } else {
            stored(tokens)
        },
    ];
    let header = [
        2,
        unknown.len() as u64,
        sections[0].len() as u64,
        sections[1].len() as u64,
        sections[2].len() as u64,
        sections[3].len() as u64,
        tokens.len() as u64,
        goal,
        (ac.len() / 2) as u64,
        (dc.len() / 2) as u64,
        1,
    ];
    let mut out = Vec::new();
    for value in header {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&2u16.to_le_bytes()); // 空rules表：Q通道沿Unknown。
    for section in sections {
        out.extend_from_slice(&section);
    }
    out
}

#[test]
fn image_decode_exr_compression_deflate_stored_fixed_and_output_guards() {
    use std::io::Write;
    let cancel = AtomicBool::new(false);
    for length in [0, 1, 65535, 65536] {
        let bytes = vec![9; length];
        let compressed = stored(&bytes);
        assert_eq!(
            inflate(&compressed, length as u64, true, None, &cancel).unwrap(),
            (length as u64, None)
        );
        if length != 0 {
            assert!(inflate(&compressed, length as u64 - 1, true, None, &cancel).is_err());
        }
        assert!(inflate(&compressed, length as u64 + 1, true, None, &cancel).is_err());
    }
    // 标准空fixed-Huffman块（03 00），不是stored块或伪压缩头。
    assert_eq!(
        inflate(&[0x78, 1, 3, 0, 0, 0, 0, 1], 0, true, None, &cancel).unwrap(),
        (0, None)
    );
    let mut bad = stored(b"x");
    bad[5] ^= 1;
    assert!(inflate(&bad, 1, true, None, &cancel).is_err());
    // 实际zlib编码器覆盖normal Huffman、跨32KiB的LZ和DWA RLE历史观察。
    let tokens = [127u8, 9].repeat(35000);
    for compression in [flate2::Compression::fast(), flate2::Compression::best()] {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), compression);
        encoder.write_all(&tokens).unwrap();
        let encoded = encoder.finish().unwrap();
        assert_eq!(
            inflate(&encoded, 70000, true, None, &cancel).unwrap(),
            (70000, None)
        );
        assert_eq!(
            inflate(&encoded, 70000, true, Some(1), &cancel).unwrap(),
            (70000, Some(35000 * 128))
        );
    }
}

#[test]
fn image_decode_exr_compression_rle_overshoot_and_dwa_inflated_token_history() {
    let cancel = AtomicBool::new(false);
    assert_eq!(rle(&[7, 9], 8, &cancel).unwrap(), 8);
    assert_eq!(rle(&[8, 9, 1, 3], 8, &cancel).unwrap(), 11);
    assert!(rle(&[255], 1, &cancel).is_err());
    let compressed = stored(&[127, 9]);
    assert_eq!(
        inflate(&compressed, 2, true, Some(1), &cancel).unwrap(),
        (2, Some(128))
    );
    let raw = [9u8; 128];
    let normal = dwa(&raw, &[], &[], &[0, 9], 1);
    let overshoot = dwa(&raw, &[], &[], &[127, 9], 1);
    let first = inspect(
        8,
        &normal,
        128,
        geometry(8, 4),
        [channel(0)].into_iter(),
        &cancel,
    )
    .unwrap();
    let second = inspect(
        8,
        &overshoot,
        128,
        geometry(8, 4),
        [channel(0)].into_iter(),
        &cancel,
    )
    .unwrap();
    assert!(second.temporary_bytes > first.temporary_bytes);
    assert_eq!(second.interleave_scratch_bytes, 0);
}

#[test]
fn image_decode_exr_compression_dwa_independent_hints_sections_and_rules() {
    let cancel = AtomicBool::new(false);
    let raw = [9u8; 128];
    let small = dwa(&raw, &[], &[0; 2], &[], 0);
    let large = dwa(&raw, &[0; 1024], &[0; 2048], &[], 0);
    let a = inspect(
        8,
        &small,
        128,
        geometry(8, 4),
        [channel(0)].into_iter(),
        &cancel,
    )
    .unwrap();
    let b = inspect(
        9,
        &large,
        128,
        geometry(8, 4),
        [channel(0)].into_iter(),
        &cancel,
    )
    .unwrap();
    assert!(b.temporary_bytes > a.temporary_bytes + 1024 + 2048);
    for offset in [8, 16, 24, 32, 40, 48, 64, 72] {
        let mut bytes = large.clone();
        bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(inspect(
            8,
            &bytes,
            128,
            geometry(8, 4),
            [channel(0)].into_iter(),
            &cancel
        )
        .is_err());
    }
    let mut truncated = small.clone();
    truncated.pop();
    assert!(inspect(
        8,
        &truncated,
        128,
        geometry(8, 4),
        [channel(0)].into_iter(),
        &cancel
    )
    .is_err());
    assert!(rules(&[1, 0], 2, &cancel).is_err());
    assert!(rules(&[5, 0, 0, 0xf0, 1], 2, &cancel).is_err());
    let table = [5, 0, 0, 0, 1];
    assert!(rules(&table, 2, &cancel).unwrap().1 >= 4 * 3 * 64);
}

#[test]
fn image_decode_exr_compression_all_raw_bypasses_and_padded_b44_workspace() {
    let cancel = AtomicBool::new(false);
    let channels = [channel(1)];
    for code in 0..=9 {
        let raw = inspect(
            code,
            &[0; 32],
            32,
            geometry(4, 4),
            channels.into_iter(),
            &cancel,
        )
        .unwrap();
        assert_eq!(
            raw,
            Workspace {
                temporary_bytes: 32 + FIXED,
                interleave_scratch_bytes: 0
            }
        );
    }
    let padded = Channel {
        sampling_x: 4,
        ..channel(1)
    };
    let workspace = inspect(6, &[1], 30, geometry(5, 3), [padded].into_iter(), &cancel).unwrap();
    assert_eq!(workspace.temporary_bytes, 1 + 5 * 48 + 64 + FIXED);
    let large_sampling = Channel {
        sampling_x: u32::MAX,
        sampling_y: u32::MAX,
        ..channel(1)
    };
    // raw旁路不运行B44 padded算法，不得因未使用的乘法拒绝。
    assert!(inspect(
        6,
        &[0; 2],
        2,
        geometry(1, 1),
        [large_sampling].into_iter(),
        &cancel
    )
    .is_ok());
    assert!(inspect(
        6,
        &[1],
        2,
        geometry(1, 1),
        [large_sampling].into_iter(),
        &cancel
    )
    .is_err());
}

#[test]
fn image_decode_exr_compression_tls_cross_family_growth_and_cancellation() {
    let mut serial = SerialWorkspace::default();
    serial
        .observe(Workspace {
            temporary_bytes: 10,
            interleave_scratch_bytes: 100,
        })
        .unwrap();
    assert_eq!((serial.peak_bytes, serial.scratch_bytes), (110, 100));
    serial
        .observe(Workspace {
            temporary_bytes: 20,
            interleave_scratch_bytes: 200,
        })
        .unwrap();
    assert_eq!((serial.peak_bytes, serial.scratch_bytes), (320, 200));
    serial
        .observe(Workspace {
            temporary_bytes: 300,
            interleave_scratch_bytes: 0,
        })
        .unwrap();
    assert_eq!((serial.peak_bytes, serial.scratch_bytes), (500, 200));
    let mut exact = SerialWorkspace::default();
    exact
        .observe(Workspace {
            temporary_bytes: MAX_BYTES - 1,
            interleave_scratch_bytes: 1,
        })
        .unwrap();
    assert_eq!(exact.peak_bytes, MAX_BYTES);
    assert!(exact
        .observe(Workspace {
            temporary_bytes: MAX_BYTES,
            interleave_scratch_bytes: 0
        })
        .is_err());
    let cancel = AtomicBool::new(true);
    let error = inspect(
        8,
        &[],
        u64::MAX,
        geometry(0, 0),
        [channel(0)].into_iter(),
        &cancel,
    )
    .err()
    .unwrap();
    assert_eq!(
        error.to_string(),
        tr!("图片读取已取消", "Image reading canceled")
    );
}

#[test]
fn image_decode_exr_compression_real_encoder_tiled_rle_payload_has_a_model() {
    use image::{ExtendedColorType, ImageEncoder};
    use std::io::Cursor;
    let mut bytes = Cursor::new(Vec::new());
    let pixels = [0f32, 0.5, 1.0, 1.0].repeat(16 * 16);
    let native: Vec<u8> = pixels
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect();
    image::codecs::openexr::OpenExrEncoder::new(&mut bytes)
        .write_image(&native, 16, 16, ExtendedColorType::Rgba32F)
        .unwrap();
    let bytes = bytes.into_inner();
    let (mut position, mut compression) = (8usize, None);
    loop {
        let name_end = position + bytes[position..].iter().position(|&b| b == 0).unwrap();
        if name_end == position {
            position += 1;
            break;
        }
        let name = &bytes[position..name_end];
        position = name_end + 1;
        position += bytes[position..].iter().position(|&b| b == 0).unwrap() + 1;
        let length = u32_at(&bytes, position).unwrap() as usize;
        position += 4;
        if name == b"compression" {
            compression = Some(bytes[position]);
        }
        position += length;
    }
    let offset = u64::from_le_bytes(bytes[position..position + 8].try_into().unwrap()) as usize;
    let code = compression.unwrap();
    // 默认编码器确实使用64×64 tile及RLE；本16×16图只有一个边缘tile。
    assert_eq!(code, 1);
    assert_ne!(u32_at(&bytes, 4).unwrap() & 0x200, 0);
    let length = u32_at(&bytes, offset + 16).unwrap() as usize;
    let channels = [
        Channel {
            name: b"A",
            ..channel(2)
        },
        Channel {
            name: b"B",
            ..channel(2)
        },
        Channel {
            name: b"G",
            ..channel(2)
        },
        Channel {
            name: b"R",
            ..channel(2)
        },
    ];
    let workspace = inspect(
        code,
        &bytes[offset + 20..offset + 20 + length],
        16 * 16 * 16,
        geometry(16, 16),
        channels.into_iter(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(workspace.temporary_bytes >= length as u64);
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::OpenExr)
        .unwrap()
        .into_rgba32f();
    assert_eq!(decoded.dimensions(), (16, 16));
    assert_eq!(decoded.into_raw(), pixels);
}
