use super::*;

fn fixture() -> Image {
    let mut bgra = Vec::new();
    for _y in 0..16 {
        for x in 0..32 {
            bgra.extend_from_slice(&match x / 8 {
                0 => [71, 139, 203, 0],
                1 => [29, 43, 229, 1],
                2 => [31, 211, 47, 128],
                _ => [219, 53, 37, 255],
            });
        }
    }
    Image {
        width: 32,
        height: 16,
        bgra,
        readback_ms: 0.0,
    }
}

fn chunks(bytes: &[u8]) -> Vec<(&[u8], &[u8])> {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
        bytes.len()
    );
    let mut output = Vec::new();
    let mut position = 12;
    while position < bytes.len() {
        let length =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        output.push((
            &bytes[position..position + 4],
            &bytes[position + 8..position + 8 + length],
        ));
        position += 8 + length + (length & 1);
    }
    assert_eq!(position, bytes.len());
    output
}

#[test]
fn image_webp_encode_typed_options_and_native_config_keep_quality_independent() {
    let mut settings = WebpEncoding::default();
    assert_eq!(settings.mode, WebpMode::Lossless);
    assert_eq!(settings.quality, 90);
    settings.mode = WebpMode::Lossy;
    settings.quality = 37;
    settings.mode = WebpMode::Lossless;
    assert_eq!(settings.quality, 37);
    assert!(settings.validate().is_ok());
    for mode in [WebpMode::Lossless, WebpMode::Lossy] {
        for quality in [0, 101, 255] {
            assert!(WebpEncoding { mode, quality }.validate().is_err());
        }
    }
    for quality in [1, 37, 90, 100] {
        let config = config(quality).unwrap();
        assert_eq!(config.lossless, 0);
        assert_eq!(config.quality, f32::from(quality));
        assert_eq!(config.alpha_quality, 100);
        assert_eq!(config.alpha_compression, 0);
        assert_eq!(config.alpha_filtering, 0);
        assert_eq!(config.exact, 1);
        assert_eq!(config.thread_level, 0);
        assert_eq!(config.low_memory, 1);
        assert_eq!(config.use_sharp_yuv, 0);
        assert_eq!(config.use_delta_palette, 0);
    }
}

#[test]
fn image_webp_encode_lossy_qualities_roundtrip_visible_colors_and_exact_alpha() {
    let input = fixture();
    let original = input.bgra.clone();
    let cancel = AtomicBool::new(false);
    let mut artifacts = Vec::new();
    for quality in [30, 75, 95] {
        let bytes = encode_lossy_bgra(&input, quality, 0, 1024 * 1024, &cancel).unwrap();
        let parts = chunks(&bytes);
        assert_eq!(parts.iter().filter(|(kind, _)| *kind == b"VP8 ").count(), 1);
        assert!(!parts.iter().any(|(kind, _)| *kind == b"VP8L"));
        let alpha = parts.iter().find(|(kind, _)| *kind == b"ALPH").unwrap().1;
        assert_eq!(
            alpha[0], 0,
            "raw alpha has no filter or level preprocessing"
        );
        assert_eq!(alpha.len(), 1 + 32 * 16);
        for (actual, source) in alpha[1..].iter().zip(input.bgra.chunks_exact(4)) {
            assert_eq!(*actual, source[3]);
        }
        let decoded = crate::image_decode::decode_bytes(&bytes).unwrap();
        assert_eq!((decoded.image.width, decoded.image.height), (32, 16));
        assert_eq!(decoded.metadata.format, image::ImageFormat::WebP);
        for (actual, source) in decoded
            .image
            .bgra
            .chunks_exact(4)
            .zip(input.bgra.chunks_exact(4))
        {
            assert_eq!(actual[3], source[3]);
        }
        for x in [12usize, 20, 28] {
            let index = (8 * 32 + x) * 4;
            for channel in 0..3 {
                assert!(
                    decoded.image.bgra[index + channel].abs_diff(input.bgra[index + channel]) <= 48,
                    "q={quality}, x={x}, channel={channel}, actual={:?}, source={:?}",
                    &decoded.image.bgra[index..index + 4],
                    &input.bgra[index..index + 4]
                );
            }
        }
        artifacts.push(bytes);
    }
    assert_ne!(
        artifacts.first(),
        artifacts.last(),
        "quality must change the encoded artifact"
    );
    assert_eq!(input.bgra, original);
}

#[test]
fn image_webp_encode_opaque_odd_size_uses_vp8_without_alpha_or_source_mutation() {
    let input = Image {
        width: 17,
        height: 9,
        bgra: [137, 137, 137, 255].repeat(17 * 9),
        readback_ms: 0.0,
    };
    let before = input.bgra.clone();
    let cancel = AtomicBool::new(false);
    let bytes = encode_lossy_bgra(&input, 80, 0, 1024 * 1024, &cancel).unwrap();
    let parts = chunks(&bytes);
    assert!(parts.iter().any(|(kind, _)| *kind == b"VP8 "));
    assert!(!parts
        .iter()
        .any(|(kind, _)| *kind == b"ALPH" || *kind == b"VP8L"));
    let decoded = crate::image_decode::decode_bytes(&bytes).unwrap().image;
    assert_eq!((decoded.width, decoded.height), (17, 9));
    for pixel in decoded.bgra.chunks_exact(4) {
        assert_eq!(pixel[3], 255);
        assert!(pixel[..3].iter().all(|channel| channel.abs_diff(137) <= 8));
    }
    assert_eq!(input.bgra, before);
}

#[test]
fn image_webp_encode_output_limit_rejects_partial_actual_bitstream() {
    let input = fixture();
    let cancel = AtomicBool::new(false);
    let bytes = encode_lossy_bgra(&input, 70, 0, 1024 * 1024, &cancel).unwrap();
    let exact = encode_lossy_bgra(&input, 70, 0, bytes.len(), &cancel).unwrap();
    assert_eq!(exact, bytes);
    let error = encode_lossy_bgra(&input, 70, 0, bytes.len() - 1, &cancel).unwrap_err();
    assert_eq!(
        error.to_string(),
        CallbackFailure::OutputLimit.error().to_string()
    );
    assert!(encode_lossy_bgra(&input, 70, 0, 0, &cancel).is_err());
}

#[test]
fn image_webp_encode_actual_progress_abort_is_cancelled_and_later_call_recovers() {
    let input = fixture();
    let cancel = AtomicBool::new(false);
    let error = encode_configured(&input, 75, 0, 1024 * 1024, &cancel, |context| {
        context.cancel_on_progress = true
    })
    .unwrap_err();
    assert!(
        cancel.load(Ordering::Relaxed),
        "actual encoder must invoke progress hook"
    );
    assert_eq!(
        error.to_string(),
        CallbackFailure::Cancelled.error().to_string()
    );
    cancel.store(false, Ordering::Relaxed);
    let bytes = encode_lossy_bgra(&input, 75, 0, 1024 * 1024, &cancel).unwrap();
    assert!(chunks(&bytes).iter().any(|(kind, _)| *kind == b"VP8 "));
    crate::image_decode::decode_bytes(&bytes).unwrap();
}

#[test]
fn image_webp_encode_writer_checks_old_new_capacity_and_preserves_failure() {
    let cancel = AtomicBool::new(false);
    let mut context = EncodeContext::new(MAX_RESIDENT_BYTES - 7, 5, &cancel);
    assert!(context.append(&[1, 2]));
    assert_eq!(context.bytes.capacity(), 2);
    assert!(context.append(&[3, 4, 5]));
    assert_eq!(context.bytes, [1, 2, 3, 4, 5]);
    assert!(!context.append(&[6]));
    assert_eq!(context.failure, Some(CallbackFailure::OutputLimit));
    assert!(!context.fail(CallbackFailure::InvalidInput));
    assert_eq!(context.failure, Some(CallbackFailure::OutputLimit));

    let mut context = EncodeContext::new(MAX_RESIDENT_BYTES - 6, 5, &cancel);
    assert!(context.append(&[1, 2]));
    assert!(!context.append(&[3, 4, 5]));
    assert_eq!(context.failure, Some(CallbackFailure::ResidentBudget));
    assert_eq!(context.bytes, [1, 2]);
    let mut context = EncodeContext::new(0, 8, &cancel);
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&[1, 2]);
    context.bytes = bytes;
    assert!(context.append(&[3, 4]));
    assert_eq!(context.bytes.capacity(), 8);
}

#[test]
fn image_webp_encode_callback_null_bounds_and_cancellation_error_priority() {
    let cancel = AtomicBool::new(false);
    let mut context = EncodeContext::new(0, 8, &cancel);
    let mut picture = Picture::new().unwrap();
    picture.0.custom_ptr = (&mut context as *mut EncodeContext<'_>).cast();
    picture.0.user_data = picture.0.custom_ptr;
    assert_eq!(unsafe { writer(std::ptr::null(), 0, &picture.0) }, 1);
    assert_eq!(unsafe { writer(std::ptr::null(), 1, &picture.0) }, 0);
    assert_eq!(context.failure, Some(CallbackFailure::InvalidInput));
    context.failure = None;
    assert_eq!(
        unsafe { writer(std::ptr::null(), usize::MAX, &picture.0) },
        0
    );
    assert_eq!(context.failure, Some(CallbackFailure::OutputLimit));
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(unsafe { progress(1, &picture.0) }, 0);
    assert_eq!(unsafe { writer(std::ptr::null(), 0, std::ptr::null()) }, 0);
    let invalid = Image {
        width: 0,
        height: 0,
        bgra: Vec::new(),
        readback_ms: 0.0,
    };
    let error = encode_lossy_bgra(&invalid, 0, u64::MAX, 0, &cancel).unwrap_err();
    assert_eq!(
        error.to_string(),
        CallbackFailure::Cancelled.error().to_string()
    );
}

#[test]
fn image_webp_encode_native_model_and_actual_source_capacity_gate_before_encoding() {
    let model = NativeModel::new(17, 9, true).unwrap();
    assert_eq!(model.picture_bytes, 17 * 9 * 2 + 9 * 5 * 2);
    assert_eq!(model.import_extra_bytes, 9 * 8);
    assert!(model.encode_extra_bytes >= 17 * 9 + 1024 + NATIVE_FIXED_BYTES);
    let opaque = NativeModel::new(17, 9, false).unwrap();
    assert_eq!(opaque.picture_bytes, 17 * 9 + 9 * 5 * 2);
    assert!(model.peak_bytes().unwrap() > opaque.peak_bytes().unwrap());
    let cancel = AtomicBool::new(false);
    let mut bytes = Vec::with_capacity(1024);
    bytes.extend_from_slice(&[20, 40, 60, 255]);
    let image = Image {
        width: 1,
        height: 1,
        bgra: bytes,
        readback_ms: 0.0,
    };
    let actual = image.bgra.capacity() as u64;
    let peak = NativeModel::new(1, 1, false).unwrap().peak_bytes().unwrap();
    let external = MAX_RESIDENT_BYTES - actual - peak;
    let error = encode_lossy_bgra(&image, 75, external + 1, 1024, &cancel).unwrap_err();
    assert_eq!(error.to_string(), budget_error().to_string());
    assert!(encode_lossy_bgra(&image, 75, u64::MAX, 1024, &cancel).is_err());
    let long = Image {
        width: 16_384,
        height: 1,
        bgra: Vec::new(),
        readback_ms: 0.0,
    };
    assert!(encode_lossy_bgra(&long, 75, 0, 1024, &cancel).is_err());
}
