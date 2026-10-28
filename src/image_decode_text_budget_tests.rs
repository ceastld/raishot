use super::*;

fn gray(values: &[u8]) -> Vec<u8> {
    values.iter().flat_map(|&v| [v, v, v, 255]).collect()
}

fn pam(depth: u32, maxval: u32, tuple: Option<&str>, raster: &[u8]) -> Vec<u8> {
    let width = if raster.len() > 4 { 2 } else { 1 };
    let mut bytes =
        format!("P7\nWIDTH +{width}\nHEIGHT +1\nDEPTH {depth}\nMAXVAL {maxval}\n").into_bytes();
    if let Some(tuple) = tuple {
        bytes.extend_from_slice(format!("TUPLTYPE {tuple}\n").as_bytes());
    }
    bytes.extend_from_slice(b"ENDHDR\n");
    bytes.extend_from_slice(raster);
    bytes
}

fn pnm_cases() -> Vec<(Vec<u8>, u64, Vec<u8>)> {
    let alternating = [
        255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0,
    ];
    let mut p4 = b"P4\n9 2\n".to_vec();
    // 每行尾部的7个pad bit必须忽略，不能串入下一行。
    p4.extend_from_slice(&[0x55, 0x7f, 0xaa, 0x80]);
    let mut p5 = b"P5\n3 1\n65535\n".to_vec();
    p5.extend_from_slice(&[0, 0, 128, 128, 255, 255]);
    let mut p6 = b"P6\n2 1\n65535\n".to_vec();
    for value in [17u16, 33, 71, 220, 5, 13] {
        p6.extend_from_slice(&(value * 257).to_be_bytes());
    }
    vec![
        (
            b"P1\n9 2\n010101010 101010101".to_vec(),
            18,
            gray(&alternating),
        ),
        (b"P2\n3 1\n15\n0 8 15".to_vec(), 3, gray(&[0, 136, 255])),
        (
            b"P3\n2 1\n255\n255 0 0 0 0 255".to_vec(),
            6,
            vec![0, 0, 255, 255, 255, 0, 0, 255],
        ),
        (p4, 18, gray(&alternating)),
        (p5, 6, gray(&[0, 128, 255])),
        (p6, 12, vec![71, 33, 17, 255, 13, 5, 220, 255]),
        (
            pam(4, 255, Some("RGB_ALPHA"), &[17, 33, 71, 128, 220, 5, 13, 0]),
            8,
            vec![71, 33, 17, 128, 13, 5, 220, 0],
        ),
        (
            pam(
                2,
                65535,
                Some("GRAYSCALE_ALPHA"),
                &[17, 17, 128, 128, 220, 220, 0, 0],
            ),
            8,
            vec![17, 17, 17, 128, 220, 220, 220, 0],
        ),
        // 未声明tuple的现后端选择U8，即使MAXVAL大于255；不按规范另猜16位。
        (pam(3, 65535, None, &[255, 0, 255]), 3, vec![1, 0, 1, 255]),
        // 现后端GRAYSCALE的U16条件是depth<=1，此旧成功分支仍保持。
        (
            pam(0, 65535, Some("GRAYSCALE"), &[128, 128]),
            2,
            gray(&[128]),
        ),
        (
            pam(2, 1, Some("BLACKANDWHITE_ALPHA"), &[1, 0]),
            2,
            vec![255, 255, 255, 0],
        ),
    ]
}

fn hdr_bytes(width: u32, attributes: &[u8], raster: &[u8]) -> Vec<u8> {
    let mut bytes = b"#?RADIANCE\n".to_vec();
    bytes.extend_from_slice(attributes);
    bytes.extend_from_slice(format!("\n-Y 1 +X {width}\n").as_bytes());
    bytes.extend_from_slice(raster);
    bytes
}

fn hdr_cases() -> Vec<(Vec<u8>, u64, Vec<u8>)> {
    let raw = hdr_bytes(2, b"", &[128, 0, 0, 129, 0, 0, 128, 129]);
    let old_rle = hdr_bytes(
        3,
        b"FORMAT=32-bit_rle_rgbe\n",
        &[128, 0, 0, 129, 1, 1, 1, 2],
    );
    let new_rle = hdr_bytes(
        8,
        b"FORMAT=32-bit_rle_rgbe\n",
        &[2, 2, 0, 8, 136, 128, 136, 0, 136, 0, 136, 129],
    );
    let mut attributes = b"EXPOSURE=1\nEXPOSURE=2\nPIXASPECT=1\nCOLORCORR=1 1 1\n".to_vec();
    for _ in 0..19 {
        attributes.extend_from_slice(b"CUSTOM=x\n");
    }
    attributes.extend_from_slice(b"LONG=");
    attributes.extend_from_slice(&vec![b'z'; 17001]);
    attributes.extend_from_slice(b"\nLOSSY=\xff\n#\xff\n");
    let long = hdr_bytes(1, &attributes, &[128, 0, 0, 129]);
    let mut unicode_dimensions = "#?RADIANCE\nCUSTOM=值\n\n-Y\u{2003}+1\u{a0}+X\u{2003}+1\n"
        .as_bytes()
        .to_vec();
    unicode_dimensions.extend_from_slice(&[128, 0, 0, 129]);
    vec![
        (raw, 24, vec![0, 0, 255, 255, 255, 0, 0, 255]),
        (old_rle, 36, [0, 0, 255, 255].repeat(3)),
        (new_rle, 96, [0, 0, 255, 255].repeat(8)),
        (long, 12, vec![0, 0, 255, 255]),
        (unicode_dimensions, 12, vec![0, 0, 255, 255]),
    ]
}

fn compare(bytes: &[u8], native: u64, expected: &[u8]) {
    let cancel = AtomicBool::new(false);
    let workspace = inspect(bytes, bytes.len() as u64, &cancel)
        .unwrap()
        .unwrap();
    assert_eq!(workspace.native_bytes, native);
    assert_eq!(workspace.backend_limit_floor, 0);
    let ordinary = super::super::decode_bytes(bytes).unwrap();
    assert_eq!(ordinary.image.bgra, expected);
    assert_eq!(
        workspace.dimensions,
        ordinary.metadata.encoded_dimensions.unwrap()
    );
    let foreground = super::super::decode_bytes_inner_with_budget(
        bytes,
        bytes.len() as u64,
        super::super::MAX_DECODE_BYTES,
        &cancel,
        super::super::DecodeContent::File,
        super::super::DecodeBudget::Foreground(32 * 1024 * 1024),
    )
    .unwrap();
    assert_eq!(foreground.image.bgra, ordinary.image.bgra);
    assert_eq!(foreground.metadata, ordinary.metadata);
    assert_eq!(foreground.warning, ordinary.warning);
}

#[test]
fn image_decode_text_workspace_p1_p7_pixels_padding_alpha_and_native_types() {
    for (bytes, native, expected) in pnm_cases() {
        compare(&bytes, native, &expected);
    }
}

#[test]
fn image_decode_text_workspace_hdr_raw_old_new_rle_and_retained_attributes() {
    for (bytes, native, expected) in hdr_cases() {
        compare(&bytes, native, &expected);
    }
}

#[test]
fn image_decode_text_workspace_exact_phases_source_capacity_and_overflow() {
    let cancel = AtomicBool::new(false);
    for (bytes, _, _) in pnm_cases().into_iter().chain(hdr_cases()) {
        let workspace = inspect(&bytes, bytes.len() as u64, &cancel)
            .unwrap()
            .unwrap();
        let peak = workspace
            .constructor_bytes
            .max(workspace.native_bytes + workspace.decode_extra_bytes);
        let exact = super::super::MAX_DECODE_BYTES - peak;
        assert!(inspect(&bytes, exact, &cancel).is_ok());
        assert!(inspect(&bytes, exact + 1, &cancel).is_err());
        assert!(inspect(&bytes, bytes.len() as u64 - 1, &cancel).is_err());
        assert!(inspect(&bytes, u64::MAX, &cancel).is_err());
    }
    assert!(inspect(b"unrelated", 0, &cancel).unwrap().is_none());
}

#[test]
fn image_decode_text_workspace_long_tokens_comments_and_constructor_growth_peaks() {
    let cancel = AtomicBool::new(false);
    let mut zeros = b"P6\n#".to_vec();
    zeros.extend_from_slice(&vec![b'c'; 20003]);
    zeros.extend_from_slice(b"\n");
    zeros.extend_from_slice(&vec![b'0'; 20003]);
    zeros.extend_from_slice(b"1 1 255\n\xff\x00\x00");
    compare(&zeros, 3, &[0, 0, 255, 255]);
    assert_eq!(
        inspect(&zeros, zeros.len() as u64, &cancel)
            .unwrap()
            .unwrap()
            .constructor_bytes,
        FIXED_BYTES
    );

    let mut comments = b"P7\n".to_vec();
    for _ in 0..2 {
        comments.extend_from_slice(b"#abcdefghijklmnop\n");
    }
    comments.extend_from_slice(b"WIDTH 1\nHEIGHT 1\nDEPTH 1\nMAXVAL 255\nENDHDR\n\x80");
    let workspace = inspect(&comments, comments.len() as u64, &cancel)
        .unwrap()
        .unwrap();
    // 新行16+32扩容时，赋值右侧尚有上一行的32B。
    assert_eq!(workspace.constructor_bytes, FIXED_BYTES + 80);
    compare(&comments, 1, &[128, 128, 128, 255]);

    let hdr = hdr_bytes(1, b"X=aaaaaaaaaaaaaaaaa\n", &[128, 0, 0, 129]);
    let workspace = inspect(&hdr, hdr.len() as u64, &cancel).unwrap().unwrap();
    // 19B行cap32，key/value按整行19B保守计，首个属性分配4×(String,String)。
    assert_eq!(
        workspace.constructor_bytes,
        FIXED_BYTES + 32 + 19 + 4 * ATTRIBUTE_BYTES
    );
    assert_eq!(
        workspace.decode_extra_bytes,
        FIXED_BYTES + 19 + 4 * ATTRIBUTE_BYTES + 4
    );
}

#[test]
fn image_decode_text_workspace_invalid_headers_do_not_reach_pixel_backend() {
    let cancel = AtomicBool::new(false);
    let cases: &[&[u8]] = &[
        b"P1",
        b"P2\n1 1 0\n",
        b"P3\n1 1 65536\n",
        b"P5\n0 1 255\n",
        b"P6\n65536 1 255\n",
        b"P6\n8000 4001 255\n",
        b"P7\r\n",
        b"P7\nWIDTH 1\nWIDTH 2\n",
        b"P7\n\xff\n",
        b"P7\nWIDTH 1\nHEIGHT 1\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nTUPLTYPE RGB\nENDHDR\n",
        b"#?RADIANCE",
        b"#?RADIANCE\n\n+Y 1 +X 1\n",
        b"#?RADIANCE\n\n-Y 1 +X 1 extra\n",
        b"#?RADIANCE\n\n-Y 1 +X 65536\n",
    ];
    for bytes in cases {
        assert!(
            inspect(bytes, bytes.len() as u64, &cancel).is_err(),
            "{bytes:?}"
        );
    }
}

#[test]
fn image_decode_text_workspace_lossy_lengths_growth_and_cancel_checkpoints() {
    let cancel = AtomicBool::new(false);
    assert_eq!(lossy_length(b"abc", &cancel).unwrap(), (3, false));
    assert_eq!(lossy_length(b"x\xff\xe2\x82", &cancel).unwrap(), (7, true));
    let mut split_utf8 = vec![b'a'; 4095];
    split_utf8.extend_from_slice("€".as_bytes());
    assert_eq!(lossy_length(&split_utf8, &cancel).unwrap(), (4098, false));
    split_utf8.extend_from_slice(b"\xff\xe2\x82");
    assert_eq!(lossy_length(&split_utf8, &cancel).unwrap(), (4104, true));
    let mut ledger = Ledger::new(0).unwrap();
    let mut buffer = Buffer::default();
    buffer.pushes(17, 8, 0, &mut ledger).unwrap();
    assert_eq!(buffer.capacity, 32);
    assert_eq!(ledger.peak, FIXED_BYTES + 48);
    cancel.store(true, Ordering::Relaxed);
    for bytes in [
        b"unrelated".as_slice(),
        b"P7".as_slice(),
        b"#?RADIANCE".as_slice(),
    ] {
        let error = inspect(bytes, u64::MAX, &cancel).err().unwrap();
        assert_eq!(
            error.to_string(),
            tr!("图片读取已取消", "Image reading canceled")
        );
    }
    assert!(lossy_length(b"valid", &cancel).is_err());
    let source = [b'0'; 8192];
    let mut cursor = Cursor {
        bytes: &source,
        position: 4095,
        cancel: &cancel,
    };
    assert_eq!(cursor.byte().unwrap(), Some(b'0'));
    assert!(cursor.byte().is_err());
}
