use super::*;

fn ftyp(major: &[u8; 4], compatible: &[[u8; 4]], extended: bool) -> Vec<u8> {
    let length = if extended { 24 } else { 16 } + compatible.len() * 4;
    let mut result = Vec::new();
    result.extend_from_slice(&(if extended { 1u32 } else { length as u32 }).to_be_bytes());
    result.extend_from_slice(b"ftyp");
    if extended {
        result.extend_from_slice(&(length as u64).to_be_bytes());
    }
    result.extend_from_slice(major);
    result.extend_from_slice(&0u32.to_be_bytes());
    for brand in compatible {
        result.extend_from_slice(brand);
    }
    result
}

#[test]
fn image_platform_ftyp_uses_image_brands_and_avif_priority_not_the_minor_version() {
    for extended in [false, true] {
        for brand in [b"avif", b"avis"] {
            assert_eq!(
                detect(&ftyp(brand, &[*b"mif1"], extended)),
                Ok(Some(PlatformFormat::Avif))
            );
        }
        for brand in [b"heic", b"heix", b"hevc", b"hevx", b"mif1", b"msf1"] {
            assert_eq!(
                detect(&ftyp(brand, &[], extended)),
                Ok(Some(PlatformFormat::Heif))
            );
        }
        assert_eq!(
            detect(&ftyp(b"mif1", &[*b"msf1", *b"avif"], extended)),
            Ok(Some(PlatformFormat::Avif))
        );
        assert_eq!(
            detect(&ftyp(b"zzzz", &[*b"mif1", *b"avis"], extended)),
            Ok(Some(PlatformFormat::Avif))
        );
    }
    let mut unknown = ftyp(b"isom", &[*b"mp42"], false);
    unknown[12..16].copy_from_slice(b"avif");
    assert_eq!(detect(&unknown), Ok(None));
    assert_eq!(detect(b"\x89PNG\r\n\x1a\n"), Ok(None));
    assert_eq!(detect(&[]), Ok(None));
}

#[test]
fn image_platform_actual_sample_headers_do_not_require_installed_components() {
    for (bytes, format) in [
        (
            include_bytes!("../tests/data/image_platform/white_1x1.avif").as_slice(),
            PlatformFormat::Avif,
        ),
        (
            include_bytes!("../tests/data/image_platform/colors-animated-8bpc.avif").as_slice(),
            PlatformFormat::Avif,
        ),
        (
            include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic")
                .as_slice(),
            PlatformFormat::Heif,
        ),
    ] {
        assert_eq!(detect(bytes), Ok(Some(format)));
    }
}

#[test]
fn image_platform_ftyp_rejects_truncation_invalid_lengths_and_unbounded_brand_tables() {
    for extended in [false, true] {
        let valid = ftyp(b"avif", &[*b"mif1"], extended);
        for length in 5..valid.len() {
            assert!(matches!(
                detect(&valid[..length]),
                Err(PlatformError::Invalid { .. })
            ));
        }
    }
    for length in [0u32, 8, 12, 17, 19] {
        let mut bytes = ftyp(b"avif", &[*b"mif1"], false);
        bytes[..4].copy_from_slice(&length.to_be_bytes());
        assert!(matches!(detect(&bytes), Err(PlatformError::Invalid { .. })));
    }
    let at_limit = ftyp(
        b"avif",
        &vec![*b"mif1"; (MAX_FTYP_BYTES as usize - 16) / 4],
        false,
    );
    assert_eq!(at_limit.len(), MAX_FTYP_BYTES as usize);
    assert_eq!(detect(&at_limit), Ok(Some(PlatformFormat::Avif)));
    let above_limit = ftyp(
        b"avif",
        &vec![*b"mif1"; (MAX_FTYP_BYTES as usize - 16) / 4 + 1],
        false,
    );
    assert!(matches!(
        detect(&above_limit),
        Err(PlatformError::Unsupported { .. })
    ));
    let mut huge = ftyp(b"avif", &[], true);
    huge[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(detect(&huge).is_err());
}

#[test]
fn image_platform_output_and_visible_working_budgets_have_separate_exact_boundaries() {
    assert_eq!(
        output_budget(16, 16, (3, 2), (2, 3), BitmapPixelFormat::Bgra8, 24),
        Ok(24)
    );
    assert!(matches!(
        output_budget(16, 16, (3, 2), (2, 3), BitmapPixelFormat::Bgra8, 23),
        Err(PlatformError::Budget(_))
    ));
    // Bgra8：两份压缩C、native格式4P、交付Array与Vec共8P。
    let encoded = ((MAX_BYTES - 12 * 6) / 2) as usize;
    assert_eq!(
        output_budget(
            encoded,
            encoded as u64,
            (3, 2),
            (2, 3),
            BitmapPixelFormat::Bgra8,
            24
        ),
        Ok(24)
    );
    assert!(matches!(
        output_budget(
            encoded + 1,
            (encoded + 1) as u64,
            (3, 2),
            (2, 3),
            BitmapPixelFormat::Bgra8,
            MAX_BYTES
        ),
        Err(PlatformError::Budget(_))
    ));
    assert!(matches!(
        output_budget(
            encoded,
            encoded as u64,
            (3, 2),
            (2, 3),
            BitmapPixelFormat::Rgba16,
            MAX_BYTES
        ),
        Err(PlatformError::Budget(_))
    ));
    assert_eq!(native_pixel_bytes(BitmapPixelFormat::P010, 1), Ok(8));
    for dimensions in [(0, 1), (1, 0), (65_536, 1), (1, 65_536), (8001, 4000)] {
        assert!(pixels(dimensions).is_err());
    }
    assert_eq!(pixels((8000, 4000)), Ok(MAX_PIXELS));
    assert!(matches!(
        output_budget(16, 16, (3, 2), (3, 3), BitmapPixelFormat::Bgra8, MAX_BYTES),
        Err(PlatformError::Invalid { .. })
    ));
    assert!(matches!(
        output_budget(
            16,
            16,
            (3, 2),
            (2, 3),
            BitmapPixelFormat::Unknown,
            MAX_BYTES
        ),
        Err(PlatformError::Unsupported { .. })
    ));
    assert!(input_budget(0, 0).is_err());
    assert!(input_budget(MAX_BYTES as usize, MAX_BYTES).is_err());
}

#[test]
fn image_platform_input_reservation_counts_capacity_and_rejects_underreporting_or_overflow() {
    let encoded = 16;
    let reserved = MAX_BYTES - encoded as u64 - 12 * 6;
    assert!(reserved > encoded as u64);
    assert_eq!(
        output_budget(
            encoded,
            reserved,
            (3, 2),
            (2, 3),
            BitmapPixelFormat::Bgra8,
            24
        ),
        Ok(24)
    );
    assert!(matches!(
        output_budget(
            encoded,
            reserved + 1,
            (3, 2),
            (2, 3),
            BitmapPixelFormat::Bgra8,
            24
        ),
        Err(PlatformError::Budget(_))
    ));
    assert!(matches!(
        input_budget(encoded, encoded as u64 - 1),
        Err(PlatformError::Budget(_))
    ));
    assert!(matches!(
        input_budget(encoded, u64::MAX),
        Err(PlatformError::Budget(_))
    ));
}

#[test]
fn image_platform_source_reservation_leaves_room_before_directory_or_exif_allocation() {
    let exact = MAX_BYTES - INSPECTION_RESERVE_BYTES;
    assert!(inspection_budget(16, exact).is_ok());
    for reserved in [exact + 1, MAX_BYTES, u64::MAX] {
        assert!(matches!(
            inspection_budget(16, reserved),
            Err(PlatformError::Budget(_))
        ));
    }
    assert!(inspection_budget(0, 16).is_err());
    assert!(inspection_budget(17, 16).is_err());
    // 使用小真实输入和大驻留声明即可验证，测试本身不分配接近上限的Vec。
    let bytes = include_bytes!("../tests/data/image_platform/white_1x1.avif");
    assert!(matches!(
        decode_with_input_reservation(
            bytes,
            exact + 1,
            PlatformFormat::Avif,
            0,
            4,
            &AtomicBool::new(false),
        ),
        Err(PlatformError::Budget(_))
    ));
}

#[test]
fn image_platform_pre_native_budget_counts_source_virtual_input_com_copy_and_selected_model() {
    let source = include_bytes!("../tests/data/image_platform/white_1x1.avif");
    let cancel = AtomicBool::new(false);
    let plan = preflight::inspect(source, 4, &cancel).unwrap();
    let frozen = plan.freeze_item(0, &cancel).unwrap();
    let model = plan.before_native_bytes_for(0).unwrap();
    assert!(model >= 4 + 16);
    let virtual_capacity = frozen.bytes.capacity() as u64;
    let exact_capacity = MAX_BYTES - virtual_capacity - frozen.bytes.len() as u64 - model;
    let check = |source_capacity, virtual_capacity| {
        preflight_budget(
            source.len(),
            source_capacity,
            frozen.bytes.len(),
            virtual_capacity,
            &plan,
            0,
        )
    };
    assert!(check(exact_capacity, virtual_capacity).is_ok());
    assert!(matches!(
        check(exact_capacity + 1, virtual_capacity),
        Err(PlatformError::Budget(_))
    ));
    assert!(check(exact_capacity, virtual_capacity + 1).is_err());
    assert!(check(u64::MAX, virtual_capacity).is_err());
    assert!(check(source.len() as u64 - 1, virtual_capacity).is_err());
    assert!(check(source.len() as u64, frozen.bytes.len() as u64 - 1).is_err());
}

#[test]
fn image_platform_error_classification_preserves_hresult_without_calling_a_codec() {
    for code in [
        WINCODEC_ERR_COMPONENTNOTFOUND,
        REGDB_E_CLASSNOTREG,
        MF_E_TOPO_CODEC_NOT_FOUND,
    ] {
        assert_eq!(
            native_error(code),
            PlatformError::ComponentUnavailable(code.0)
        );
    }
    for code in [
        WINCODEC_ERR_UNSUPPORTEDPIXELFORMAT,
        WINCODEC_ERR_UNSUPPORTEDOPERATION,
    ] {
        assert!(
            matches!(native_error(code), PlatformError::Unsupported { hresult: Some(value), .. } if value == code.0)
        );
    }
    for code in [
        WINCODEC_ERR_BADIMAGE,
        WINCODEC_ERR_BADHEADER,
        WINCODEC_ERR_FRAMEMISSING,
    ] {
        assert!(
            matches!(native_error(code), PlatformError::Invalid { hresult: Some(value), .. } if value == code.0)
        );
    }
    assert_eq!(
        native_error(RPC_E_CHANGED_MODE),
        PlatformError::ThreadContext(RPC_E_CHANGED_MODE.0)
    );
    assert_eq!(
        native_error(E_OUTOFMEMORY),
        PlatformError::Native(E_OUTOFMEMORY.0)
    );
    assert!(native_error(WINCODEC_ERR_COMPONENTNOTFOUND)
        .to_string()
        .contains("0x88982F50"));
    let cancel = AtomicBool::new(false);
    let result: Result<(), _> = call(&cancel, || {
        cancel.store(true, Ordering::Release);
        Err(PlatformError::Native(E_OUTOFMEMORY.0))
    });
    assert_eq!(result, Err(PlatformError::Cancelled));
    let mut invoked = false;
    let result = call(&cancel, || {
        invoked = true;
        Ok(())
    });
    assert_eq!(result, Err(PlatformError::Cancelled));
    assert!(!invoked);
}

#[test]
fn image_platform_bgra_copy_keeps_straight_alpha_and_checks_exact_shape_and_cancellation() {
    let bytes = [3, 2, 1, 0, 6, 5, 4, 128, 9, 8, 7, 255, 12, 11, 10, 1];
    let cancel = AtomicBool::new(false);
    let orientation = Orientation::NoTransforms;
    assert_eq!(
        copy_pixels(&bytes, (2, 2), 16, orientation, &cancel)
            .unwrap()
            .bgra,
        bytes
    );
    assert!(matches!(
        copy_pixels(&bytes[..15], (2, 2), 16, orientation, &cancel),
        Err(PlatformError::Invalid { .. })
    ));
    assert!(matches!(
        copy_pixels(&bytes, (1, 2), 16, orientation, &cancel),
        Err(PlatformError::Invalid { .. })
    ));
    cancel.store(true, Ordering::Release);
    assert!(matches!(
        copy_pixels(&bytes, (2, 2), 16, orientation, &cancel),
        Err(PlatformError::Cancelled)
    ));
}

#[test]
fn image_platform_bgra_copy_applies_each_exif_direction_once_without_changing_channels() {
    // 非方形网格，各通道和alpha均有区分，避免只检尺寸而漏镜像或二次旋转。
    let bytes: Vec<u8> = (1u8..=6)
        .flat_map(|value| [value, value + 10, value + 20, (value - 1) * 50])
        .collect();
    let expected: [[u8; 6]; 8] = [
        [1, 2, 3, 4, 5, 6],
        [3, 2, 1, 6, 5, 4],
        [6, 5, 4, 3, 2, 1],
        [4, 5, 6, 1, 2, 3],
        [1, 4, 2, 5, 3, 6],
        [4, 1, 5, 2, 6, 3],
        [6, 3, 5, 2, 4, 1],
        [3, 6, 2, 5, 1, 4],
    ];
    let cancel = AtomicBool::new(false);
    for value in 1..=8 {
        let orientation = Orientation::from_exif(value).unwrap();
        let image = copy_pixels(&bytes, (3, 2), 24, orientation, &cancel).unwrap();
        assert_eq!(
            (image.width, image.height),
            if value < 5 { (3, 2) } else { (2, 3) }
        );
        let reference: Vec<_> = expected[usize::from(value - 1)]
            .into_iter()
            .flat_map(|value| [value, value + 10, value + 20, (value - 1) * 50])
            .collect();
        assert_eq!(image.bgra, reference, "EXIF {value}");
    }
}

#[test]
fn image_platform_source_item_index_is_rejected_before_native_creation() {
    let bytes = include_bytes!("../tests/data/image_platform/white_1x1.avif");
    let cancel = AtomicBool::new(false);
    for index in [1, u32::MAX] {
        assert!(matches!(
            decode(bytes, PlatformFormat::Avif, index, 4, &cancel),
            Err(PlatformError::Invalid { .. })
        ));
    }
}

fn conditional_native_sample(bytes: &[u8], format: PlatformFormat) -> Option<PlatformDecoded> {
    assert_eq!(detect(bytes), Ok(Some(format)));
    match decode(bytes, format, 0, MAX_BYTES, &AtomicBool::new(false)) {
        Ok(decoded) => {
            eprintln!(
                "{format:?}: decoded {}x{}, independent items={}",
                decoded.image.width, decoded.image.height, decoded.frame_count
            );
            assert_eq!(decoded.frame_index, 0);
            assert_eq!(decoded.frame_count, 1);
            assert_ne!(decoded.item_id, 0);
            assert!(decoded.is_primary);
            assert!(decoded.decoded_color.is_none());
            assert!(decoded.warning.is_some());
            Some(decoded)
        }
        Err(error @ PlatformError::ComponentUnavailable(_))
        | Err(error @ PlatformError::Unsupported { .. }) => {
            // 这是条件能力结果，不能据此记录像素解码通过或确认为缺少某个组件。
            eprintln!("{format:?}: conditional capability unavailable: {error}");
            None
        }
        Err(error) => panic!("{format:?}: actual encoded sample failed: {error}"),
    }
}

#[test]
#[ignore = "Requires an installed AVIF decoder; inspect capability output"]
fn image_platform_native_avif_actual_white_or_reports_conditional_capability() {
    let bytes = include_bytes!("../tests/data/image_platform/white_1x1.avif");
    let Some(decoded) = conditional_native_sample(bytes, PlatformFormat::Avif) else {
        return;
    };
    assert_eq!((decoded.image.width, decoded.image.height), (1, 1));
    assert_eq!(decoded.image.bgra.len(), 4);
    assert_eq!(decoded.encoded_dimensions, Some((1, 1)));
    assert_eq!(decoded.bits_per_channel, Some(8));
    assert_eq!(decoded.image.bgra[3], 255);
    // 来源只承诺白色有损样本；不用伪造的精确通道金值验证此文件。
    assert!(decoded.image.bgra[..3].iter().all(|value| *value >= 250));
}

#[test]
#[ignore = "Requires an installed HEIF/HEVC decoder; inspect capability output"]
fn image_platform_native_heic_actual_conformance_window_or_reports_conditional_capability() {
    let bytes = include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic");
    let Some(decoded) = conditional_native_sample(bytes, PlatformFormat::Heif) else {
        return;
    };
    // HEVC coded为256x128，规范窗口左裁6像素；公开来源给出250x128显示范围。
    assert_eq!((decoded.image.width, decoded.image.height), (250, 128));
    assert_eq!(decoded.image.bgra.len(), 250 * 128 * 4);
    assert_eq!(decoded.encoded_dimensions, Some((256, 128)));
    assert_eq!(decoded.bits_per_channel, Some(8));
    assert!(decoded
        .image
        .bgra
        .chunks_exact(4)
        .all(|pixel| pixel[3] == 255));
}
