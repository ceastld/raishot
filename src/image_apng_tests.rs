//! 由PNG编码器在内存生成强判别样本，不依赖外部图片或图形窗口。
use super::*;
use png::{BitDepth, BlendOp, ColorType, DisposeOp};
use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];
const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const GREEN_HALF: [u8; 4] = [0, 255, 0, 128];
const YELLOW_HALF: [u8; 4] = [255, 255, 0, 128];
const CYAN: [u8; 4] = [0, 255, 255, 255];

struct TestFrame {
    rect: (u32, u32, u32, u32),
    pixels: Vec<u8>,
    blend: BlendOp,
    dispose: DisposeOp,
    delay: (u16, u16),
}

fn rgba_frame(
    rect: (u32, u32, u32, u32),
    pixels: &[[u8; 4]],
    blend: BlendOp,
    dispose: DisposeOp,
) -> TestFrame {
    TestFrame {
        rect,
        pixels: pixels.iter().flatten().copied().collect(),
        blend,
        dispose,
        delay: (1, 10),
    }
}

fn encode(
    dimensions: (u32, u32),
    color: ColorType,
    plays: u32,
    poster: Option<&[u8]>,
    frames: Vec<TestFrame>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, dimensions.0, dimensions.1);
        encoder.set_color(color);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_animated(frames.len() as u32, plays).unwrap();
        encoder.set_sep_def_img(poster.is_some()).unwrap();
        let mut writer = encoder.write_header().unwrap();
        if let Some(poster) = poster {
            writer.write_image_data(poster).unwrap();
        }
        for frame in frames {
            // 先移回原点再改变大小，避免上一块偏移影响新尺寸的边界检查。
            writer.set_frame_position(0, 0).unwrap();
            writer
                .set_frame_dimension(frame.rect.2, frame.rect.3)
                .unwrap();
            writer
                .set_frame_position(frame.rect.0, frame.rect.1)
                .unwrap();
            writer
                .set_frame_delay(frame.delay.0, frame.delay.1)
                .unwrap();
            writer.set_blend_op(frame.blend).unwrap();
            writer.set_dispose_op(frame.dispose).unwrap();
            writer.write_image_data(&frame.pixels).unwrap();
        }
        writer.finish().unwrap();
    }
    bytes
}

fn token() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn source(bytes: Vec<u8>) -> ApngSource<Cursor<Vec<u8>>> {
    ApngSource::from_reader(Cursor::new(bytes), ApngOptions::default(), token()).unwrap()
}

fn bgra(pixels: &[[u8; 4]]) -> Vec<u8> {
    pixels
        .iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
        .collect()
}

fn loop_fixture(plays: u32) -> Vec<u8> {
    // 无fcTL的品红IDAT不应进入动画，也不能污染第一帧未覆盖的右侧像素。
    encode(
        (2, 1),
        ColorType::Rgba,
        plays,
        Some(&[255, 0, 255, 255, 255, 0, 255, 255]),
        vec![
            rgba_frame((0, 0, 1, 1), &[RED], BlendOp::Source, DisposeOp::None),
            rgba_frame((1, 0, 1, 1), &[BLUE], BlendOp::Source, DisposeOp::None),
        ],
    )
}

#[test]
fn image_apng_local_source_over_background_and_previous_have_distinct_pixels() {
    let bytes = encode(
        (3, 2),
        ColorType::Rgba,
        1,
        None,
        vec![
            rgba_frame((0, 0, 3, 2), &[RED; 6], BlendOp::Source, DisposeOp::None),
            rgba_frame(
                (0, 0, 2, 1),
                &[BLUE, GREEN_HALF],
                BlendOp::Source,
                DisposeOp::Background,
            ),
            rgba_frame(
                (0, 0, 3, 1),
                &[YELLOW_HALF, TRANSPARENT, GREEN_HALF],
                BlendOp::Over,
                DisposeOp::Previous,
            ),
            rgba_frame(
                (2, 1, 1, 1),
                &[CYAN],
                BlendOp::Source,
                DisposeOp::Background,
            ),
        ],
    );
    let mut source = source(bytes);
    assert_eq!(source.dimensions(), (3, 2));
    let expected = [
        [RED; 6],
        [BLUE, GREEN_HALF, RED, RED, RED, RED],
        [YELLOW_HALF, TRANSPARENT, [127, 128, 0, 255], RED, RED, RED],
        [TRANSPARENT, TRANSPARENT, RED, RED, RED, CYAN],
    ];
    let mut last = None;
    for (index, pixels) in expected.iter().enumerate() {
        let output = source.next_frame().unwrap().unwrap();
        assert_eq!(
            (output.generation, output.cycle, output.index),
            (0, 0, index as u64)
        );
        assert_eq!((output.image().width, output.image().height), (3, 2));
        assert_eq!(output.image().bgra, bgra(pixels));
        last = Some(output);
    }
    assert!(source.next_frame().unwrap().is_none());
    assert!(source.next_frame().unwrap().is_none());
    // 最后一帧使用Background，有限播放结束后保留已交付的青色画面。
    assert_eq!(source.canvas, bgra(expected.last().unwrap()));
    assert_eq!(last.unwrap().image().bgra, bgra(expected.last().unwrap()));
}

#[test]
fn image_apng_rgb_poster_is_not_an_animation_frame_or_canvas_background() {
    let frames = vec![
        TestFrame {
            rect: (0, 0, 1, 1),
            pixels: vec![255, 0, 0],
            blend: BlendOp::Source,
            dispose: DisposeOp::None,
            delay: (1, 10),
        },
        TestFrame {
            rect: (1, 0, 1, 1),
            pixels: vec![0, 0, 255],
            blend: BlendOp::Source,
            dispose: DisposeOp::None,
            delay: (1, 10),
        },
    ];
    let mut source = source(encode(
        (2, 1),
        ColorType::Rgb,
        1,
        Some(&[0, 255, 0, 0, 255, 0]),
        frames,
    ));
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!(first.index, 0);
    assert_eq!(first.image().bgra, bgra(&[RED, TRANSPARENT]));
    let second = source.next_frame().unwrap().unwrap();
    assert_eq!(second.index, 1);
    assert_eq!(second.image().bgra, bgra(&[RED, BLUE]));
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_apng_num_plays_counts_total_plays_and_every_cycle_starts_transparent() {
    for (plays, frames) in [(1u32, 2u64), (2, 4), (0, 6)] {
        let mut source = source(loop_fixture(plays));
        assert_eq!(source.num_plays(), plays);
        for index in 0..frames {
            let output = source.next_frame().unwrap().unwrap();
            assert_eq!((output.cycle, output.index), (index / 2, index % 2));
            let expected = if index % 2 == 0 {
                [RED, TRANSPARENT]
            } else {
                [RED, BLUE]
            };
            assert_eq!(output.image().bgra, bgra(&expected));
        }
        if plays > 0 {
            assert!(source.next_frame().unwrap().is_none());
        }
    }
}

#[test]
fn image_apng_delay_preserves_raw_fraction_and_normalizes_zero_fields() {
    let cases = [
        ((1, 0), Duration::from_millis(10)),
        ((3, 20), Duration::from_millis(150)),
        ((1, 4), Duration::from_millis(250)),
        ((0, 0), Duration::from_millis(100)),
    ];
    let frames = cases
        .iter()
        .map(|(delay, _)| {
            let mut frame = rgba_frame((0, 0, 1, 1), &[RED], BlendOp::Source, DisposeOp::None);
            frame.delay = *delay;
            frame
        })
        .collect();
    let mut source = source(encode((1, 1), ColorType::Rgba, 1, None, frames));
    for ((numerator, denominator), delay) in cases {
        let output = source.next_frame().unwrap().unwrap();
        assert_eq!(
            (output.timing.numerator, output.timing.denominator),
            (numerator, denominator)
        );
        assert_eq!(output.timing.delay, delay);
    }
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_apng_output_leases_survive_restart_and_backpressure_does_not_consume_frames() {
    let mut source = source(loop_fixture(0));
    let mut held = Vec::new();
    for _ in 0..4 {
        let frame = source.next_frame().unwrap().unwrap();
        assert!(frame.image().bgra.capacity() <= frame.image().bgra.len());
        held.push(frame);
    }
    assert!(matches!(source.next_frame(), Err(ApngError::Backpressure)));
    drop(held.remove(0));
    let next = source.next_frame().unwrap().unwrap();
    assert_eq!((next.cycle, next.index), (2, 0));
    held.push(next);
    source.restart(token()).unwrap();
    assert!(matches!(source.next_frame(), Err(ApngError::Backpressure)));
    held.clear();
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!((first.generation, first.cycle, first.index), (1, 0, 0));
    assert_eq!(first.image().bgra, bgra(&[RED, TRANSPARENT]));
}

#[test]
fn image_apng_capacity_checks_measure_storage_and_cancel_before_allocation() {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(&[1, 2, 3, 4]);
    assert!(bytes.capacity() > bytes.len());
    assert!(matches!(
        check_capacity(bytes.capacity(), bytes.len()),
        Err(ApngError::Invalid(_))
    ));
    check_capacity(bytes.capacity(), bytes.capacity()).unwrap();
    assert!(matches!(
        allocate(usize::MAX, &AtomicBool::new(true)),
        Err(ApngError::Cancelled)
    ));

    let mut profile = Vec::with_capacity(SRGB_PROFILE.len() * 4);
    profile.extend_from_slice(SRGB_PROFILE);
    assert!(profile.capacity() > profile.len() * 2);
    assert_eq!(
        profile_constructor_bytes(Some(&profile)).unwrap(),
        profile.capacity() + profile.len()
    );
    assert_eq!(profile_constructor_bytes(None).unwrap(), 0);
}

#[test]
fn image_apng_smaller_previous_reuses_canvas_slot_and_failed_lease_is_released() {
    let mut source = source(encode(
        (2, 2),
        ColorType::Rgba,
        0,
        None,
        vec![
            // 首帧Previous等同Background；先建红底才能实际分配并复用恢复区。
            rgba_frame((0, 0, 2, 2), &[RED; 4], BlendOp::Source, DisposeOp::None),
            rgba_frame(
                (0, 0, 2, 2),
                &[GREEN_HALF; 4],
                BlendOp::Source,
                DisposeOp::Previous,
            ),
            rgba_frame((1, 1, 1, 1), &[BLUE], BlendOp::Source, DisposeOp::Previous),
        ],
    ));
    let baseline = source.next_frame().unwrap().unwrap();
    assert_eq!(baseline.image().bgra, bgra(&[RED; 4]));
    let first = source.next_frame().unwrap().unwrap();
    let restore_capacity = source.restore.capacity();
    let second = source.next_frame().unwrap().unwrap();
    assert_eq!(source.restore.capacity(), restore_capacity);
    assert!(restore_capacity > source.restore.len());
    assert_eq!(second.image().bgra, bgra(&[RED, RED, RED, BLUE]));

    source.restore = Vec::with_capacity(32);
    assert!(matches!(source.next_frame(), Err(ApngError::Invalid(_))));
    assert_eq!(source.index, 3);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 3);
    source.restart(token()).unwrap();
    let restarted = source.next_frame().unwrap().unwrap();
    assert_eq!(
        (restarted.generation, restarted.cycle, restarted.index),
        (1, 0, 0)
    );
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 4);
    drop((baseline, first, second, restarted));
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
}

#[test]
fn image_apng_cancel_poison_requires_restart_and_new_token_isolates_old_generation() {
    let cancel = token();
    let mut source = ApngSource::from_reader(
        Cursor::new(loop_fixture(0)),
        ApngOptions::default(),
        cancel.clone(),
    )
    .unwrap();
    drop(source.next_frame().unwrap().unwrap());
    cancel.store(true, Ordering::Relaxed);
    assert!(matches!(source.next_frame(), Err(ApngError::Cancelled)));
    cancel.store(false, Ordering::Relaxed);
    assert!(matches!(
        source.next_frame(),
        Err(ApngError::RequiresRestart)
    ));
    source.restart(token()).unwrap();
    cancel.store(true, Ordering::Relaxed);
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!((first.generation, first.cycle, first.index), (1, 0, 0));
    assert_eq!(first.image().bgra, bgra(&[RED, TRANSPARENT]));
    assert_eq!(source.next_frame().unwrap().unwrap().index, 1);
}

#[test]
fn image_apng_first_previous_restores_initial_transparency() {
    let mut source = source(encode(
        (2, 1),
        ColorType::Rgba,
        1,
        None,
        vec![
            rgba_frame(
                (0, 0, 2, 1),
                &[RED; 2],
                BlendOp::Source,
                DisposeOp::Previous,
            ),
            rgba_frame((1, 0, 1, 1), &[BLUE], BlendOp::Source, DisposeOp::None),
        ],
    ));
    assert_eq!(
        source.next_frame().unwrap().unwrap().image().bgra,
        bgra(&[RED; 2])
    );
    assert_eq!(
        source.next_frame().unwrap().unwrap().image().bgra,
        bgra(&[TRANSPARENT, BLUE])
    );
    assert!(source.next_frame().unwrap().is_none());
}

const SRGB_PROFILE: &[u8] = include_bytes!("../assets/color/srgb.icc");

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn metadata_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut output = (payload.len() as u32).to_be_bytes().to_vec();
    output.extend_from_slice(kind);
    output.extend_from_slice(payload);
    let crc = crc32(&output[4..]);
    output.extend_from_slice(&crc.to_be_bytes());
    output
}

fn iccp(profile: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(profile).unwrap();
    let mut payload = b"RabShot\0\0".to_vec();
    payload.extend_from_slice(&encoder.finish().unwrap());
    metadata_chunk(b"iCCP", &payload)
}

fn with_icc(mut bytes: Vec<u8>, chunk: &[u8]) -> Vec<u8> {
    // IHDR之后、任何图像数据之前；不修改帧控制或压缩像素负载。
    bytes.splice(33..33, chunk.iter().copied());
    bytes
}

fn two_color_frames() -> Vec<u8> {
    encode(
        (2, 1),
        ColorType::Rgba,
        0,
        None,
        vec![
            rgba_frame(
                (0, 0, 2, 1),
                &[[20, 40, 60, 0], [80, 100, 120, 255]],
                BlendOp::Source,
                DisposeOp::None,
            ),
            rgba_frame(
                (1, 0, 1, 1),
                &[[200, 160, 80, 128]],
                BlendOp::Over,
                DisposeOp::None,
            ),
        ],
    )
}

#[test]
fn image_apng_icc_identity_frames_keep_alpha_encoded_canvas_and_shared_facts() {
    use crate::image_color_management::ColorConversion;
    let bytes = with_icc(two_color_frames(), &iccp(SRGB_PROFILE));
    let mut source = source(bytes);
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!(
        first.image().bgra,
        bgra(&[[20, 40, 60, 0], [80, 100, 120, 255]])
    );
    assert_eq!(first.image().bgra, source.canvas);
    assert_eq!(
        first.metadata_source().facts().color_conversion,
        ColorConversion::ConvertedSrgb
    );
    assert_eq!(
        first.metadata_source().facts().source.icc.status,
        MetadataStatus::Present
    );
    assert_eq!(
        first.metadata_source().facts().source.bits_per_channel,
        Some(8)
    );
    assert_eq!(first.warning(), None);
    let second = source.next_frame().unwrap().unwrap();
    assert_ne!(first.image().bgra, second.image().bgra);
    assert_eq!(second.image().bgra, source.canvas);
    assert!(Arc::ptr_eq(
        first.metadata_source(),
        second.metadata_source()
    ));
    // 交付图的元数据和租约随旧代帧保留；重播不重复提取配置。
    let mut held = vec![first, second];
    held.push(source.next_frame().unwrap().unwrap());
    held.push(source.next_frame().unwrap().unwrap());
    source.restart(token()).unwrap();
    assert!(matches!(source.next_frame(), Err(ApngError::Backpressure)));
    let facts = held[0].metadata_source().clone();
    held.clear();
    let frame = source.next_frame().unwrap().unwrap();
    assert_eq!((frame.generation, frame.cycle, frame.index), (1, 0, 0));
    assert!(Arc::ptr_eq(&facts, frame.metadata_source()));
}

#[test]
fn image_apng_bad_duplicate_late_and_oversized_icc_publish_unapplied_raw_frames() {
    use crate::image_color_management::{ColorConversion, UnappliedReason};
    let plain = two_color_frames();
    let valid = iccp(SRGB_PROFILE);
    let mut bad_crc = valid.clone();
    *bad_crc.last_mut().unwrap() ^= 1;
    let mut duplicate = with_icc(plain.clone(), &valid);
    duplicate.splice(33..33, valid.iter().copied());
    let mut late = plain.clone();
    let end = late.len() - 12;
    late.splice(end..end, valid.iter().copied());
    let too_large = iccp(&vec![0; 4 * 1024 * 1024 + 1]);
    for (bytes, status, reason) in [
        (
            with_icc(plain.clone(), &bad_crc),
            MetadataStatus::Malformed,
            UnappliedReason::MalformedMetadata,
        ),
        (
            duplicate,
            MetadataStatus::Malformed,
            UnappliedReason::MalformedMetadata,
        ),
        (
            late,
            MetadataStatus::Malformed,
            UnappliedReason::MalformedMetadata,
        ),
        (
            with_icc(plain.clone(), &too_large),
            MetadataStatus::TooLarge,
            UnappliedReason::TooLarge,
        ),
        (
            with_icc(plain, &iccp(b"not an ICC profile")),
            MetadataStatus::Present,
            UnappliedReason::InvalidProfile,
        ),
    ] {
        let mut source = source(bytes);
        let first = source.next_frame().unwrap().unwrap();
        assert_eq!(first.image().bgra, source.canvas);
        assert_eq!(first.metadata_source().facts().source.icc.status, status);
        assert_eq!(
            first.metadata_source().facts().color_conversion,
            ColorConversion::EmbeddedUnapplied(reason)
        );
        assert!(first.warning().is_some());
        let second = source.next_frame().unwrap().unwrap();
        assert_eq!(second.image().bgra, source.canvas);
        assert!(Arc::ptr_eq(
            first.metadata_source(),
            second.metadata_source()
        ));
        source.restart(token()).unwrap();
        assert!(source.next_frame().unwrap().unwrap().warning().is_some());
    }
}

#[test]
fn image_apng_icc_budget_counts_profile_rows_and_long_side_presentation() {
    let bytes = with_icc(two_color_frames(), &iccp(SRGB_PROFILE));
    let options = ApngOptions::default();
    let required = canvas_budget(2, 1, options).unwrap()
        + SRGB_PROFILE.len() as u64 * 3
        + FrameColorManager::row_buffer_bytes(2).unwrap() as u64;
    assert!(matches!(
        ApngSource::from_reader(
            Cursor::new(bytes.clone()),
            ApngOptions {
                max_total_bytes: required - 1,
                ..options
            },
            token()
        ),
        Err(ApngError::Invalid(_))
    ));
    let source = ApngSource::from_reader(
        Cursor::new(bytes),
        ApngOptions {
            max_total_bytes: required,
            ..options
        },
        token(),
    )
    .unwrap();
    assert_eq!(source.reserved_bytes(), required);
    assert_eq!(
        canvas_budget(16_384, 1, options).unwrap(),
        16_384 * 48 + FIXED_RESERVE
    );
    assert_eq!(
        canvas_budget(16_385, 1, options).unwrap(),
        16_385 * 52 + FIXED_RESERVE
    );
}

#[test]
fn image_apng_icc_identity_rechecked_during_runtime_and_restart_without_reinflation() {
    let original = with_icc(two_color_frames(), &iccp(SRGB_PROFILE));
    let mut input = Cursor::new(original.clone());
    let (scan, profile) = scan_source_with_profile(
        &mut input,
        0,
        original.len() as u64,
        ApngOptions::default(),
        &token(),
        None,
    )
    .unwrap();
    assert_eq!(profile.as_deref(), Some(SRGB_PROFILE));
    let (replayed, profile) = scan_source_with_profile(
        &mut input,
        0,
        original.len() as u64,
        ApngOptions::default(),
        &token(),
        Some(scan),
    )
    .unwrap();
    assert_eq!(replayed, scan);
    assert!(profile.is_none());
    let mut changed = original.clone();
    // 同长且保留原CRC字段，首字节名称变化仍须由冻结内容指纹拒绝。
    changed[41] ^= 1;
    let mut guarded = GuardedRead::new(
        Arc::new(Mutex::new(Cursor::new(changed.clone()))),
        0,
        changed.len() as u64,
        scan.frames,
        scan.icc_layout,
        token(),
    )
    .unwrap();
    let error = std::io::copy(&mut guarded, &mut std::io::sink()).unwrap_err();
    assert!(
        error.to_string().contains("ICC")
            || error.to_string().contains("profile")
            || error.to_string().contains("颜色")
    );
    let mut source = source(original);
    *source.reader.lock().unwrap().get_mut() = changed;
    assert!(matches!(
        source.restart(token()),
        Err(ApngError::Invalid(_))
    ));
    assert!(matches!(
        source.next_frame(),
        Err(ApngError::RequiresRestart)
    ));
}

#[test]
fn image_apng_renamed_frozen_icc_is_rejected_before_decoder_can_publish_first_frame() {
    let original = with_icc(two_color_frames(), &iccp(SRGB_PROFILE));
    let (scan, profile) = scan_source_with_profile(
        &mut Cursor::new(&original),
        0,
        original.len() as u64,
        ApngOptions::default(),
        &token(),
        None,
    )
    .unwrap();
    assert_eq!(profile.as_deref(), Some(SRGB_PROFILE));
    let identity = scan.icc_layout.first.unwrap();
    let offset = identity.offset as usize;
    let end = offset + identity.length as usize + 12;
    let mut changed = original;
    changed[offset + 4..offset + 8].copy_from_slice(b"iCCQ");
    let checksum = crc32(&changed[offset + 4..end - 4]);
    changed[end - 4..end].copy_from_slice(&checksum.to_be_bytes());
    assert_eq!(checksum, crc32(&changed[offset + 4..end - 4]));
    let length = changed.len() as u64;
    let reader = Arc::new(Mutex::new(Cursor::new(changed)));
    let result = make_decoder(reader.clone(), 0, length, scan, token());
    assert!(result.is_err());
    // 在冻结位置的块头即拒绝，没有等到图像数据或IEND再发现缺少ICC。
    assert_eq!(reader.lock().unwrap().position(), identity.offset + 8);
}

#[test]
fn image_apng_icc_scan_cancellation_is_not_downgraded_to_unapplied_color() {
    struct CancelRead {
        inner: Cursor<Vec<u8>>,
        cancel: Arc<AtomicBool>,
    }
    impl std::io::Read for CancelRead {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let count = std::io::Read::read(&mut self.inner, output)?;
            if self.inner.position() >= 42 {
                self.cancel.store(true, Ordering::Release);
            }
            Ok(count)
        }
    }
    impl std::io::Seek for CancelRead {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            std::io::Seek::seek(&mut self.inner, position)
        }
    }
    let cancel = token();
    let reader = CancelRead {
        inner: Cursor::new(with_icc(two_color_frames(), &iccp(SRGB_PROFILE))),
        cancel: cancel.clone(),
    };
    assert!(matches!(
        ApngSource::from_reader(reader, ApngOptions::default(), cancel),
        Err(ApngError::Cancelled)
    ));
    let cancel = token();
    let mut source = ApngSource::from_reader(
        Cursor::new(with_icc(two_color_frames(), &iccp(SRGB_PROFILE))),
        ApngOptions::default(),
        cancel.clone(),
    )
    .unwrap();
    cancel.store(true, Ordering::Release);
    assert!(matches!(source.next_frame(), Err(ApngError::Cancelled)));
    source.restart(token()).unwrap();
    cancel.store(true, Ordering::Release);
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!(first.warning(), None);
    assert_eq!(first.generation, 1);
}

#[test]
#[ignore = "调用系统颜色管理；需显式运行，无窗口"]
fn image_apng_native_icc_maps_completed_frames_and_leaves_composition_in_source_rgb() {
    use crate::image_color_management::ColorConversion;
    let bytes = encode(
        (2, 1),
        ColorType::Rgba,
        1,
        None,
        vec![
            rgba_frame(
                (0, 0, 2, 1),
                &[[128, 128, 128, 255], [128, 128, 128, 0]],
                BlendOp::Source,
                DisposeOp::None,
            ),
            rgba_frame(
                (0, 0, 1, 1),
                &[[255, 0, 0, 128]],
                BlendOp::Over,
                DisposeOp::None,
            ),
        ],
    );
    let mut source = source(with_icc(
        bytes,
        &iccp(include_bytes!("../tests/data/image_color/linear-rgb.icc")),
    ));
    // 线性RGB→sRGB的独立参考：128→188，64→137，192→225。
    for (canonical, expected) in [
        (
            vec![128, 128, 128, 255, 128, 128, 128, 0],
            vec![188, 188, 188, 255, 188, 188, 188, 0],
        ),
        (
            vec![64, 64, 192, 255, 128, 128, 128, 0],
            vec![137, 137, 225, 255, 188, 188, 188, 0],
        ),
    ] {
        let frame = source.next_frame().unwrap().unwrap();
        assert_eq!(source.canvas, canonical);
        assert_eq!(
            frame.metadata_source().facts().color_conversion,
            ColorConversion::ConvertedSrgb
        );
        assert!(frame.warning().is_none());
        for (pixel, reference) in frame
            .image()
            .bgra
            .chunks_exact(4)
            .zip(expected.chunks_exact(4))
        {
            assert_eq!(pixel[3], reference[3]);
            for channel in 0..3 {
                assert!(
                    pixel[channel].abs_diff(reference[channel]) <= 3,
                    "actual={pixel:?}, expected={reference:?}"
                );
            }
        }
    }
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_apng_decoder_initial_rewind_cancellation_returns_cancelled() {
    struct CancelSeek {
        inner: Cursor<Vec<u8>>,
        cancel: Arc<AtomicBool>,
        rewinds: usize,
    }
    impl std::io::Read for CancelSeek {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.inner, output)
        }
    }
    impl std::io::Seek for CancelSeek {
        fn seek(&mut self, request: std::io::SeekFrom) -> std::io::Result<u64> {
            let position = std::io::Seek::seek(&mut self.inner, request)?;
            if matches!(request, std::io::SeekFrom::Start(0)) {
                self.rewinds += 1;
                // 首次是全来源预扫；第二次是GuardedRead构造新解码器。
                if self.rewinds == 2 {
                    self.cancel.store(true, Ordering::Release);
                }
            }
            Ok(position)
        }
    }
    let cancel = token();
    let reader = CancelSeek {
        inner: Cursor::new(with_icc(two_color_frames(), &iccp(SRGB_PROFILE))),
        cancel: cancel.clone(),
        rewinds: 0,
    };
    assert!(matches!(
        ApngSource::from_reader(reader, ApngOptions::default(), cancel.clone()),
        Err(ApngError::Cancelled)
    ));
    assert!(cancel.load(Ordering::Acquire));
}

#[test]
fn image_apng_constructor_prioritizes_cancel_when_seek_or_read_also_fails() {
    #[derive(Clone, Copy, Debug)]
    enum FailAt {
        Position,
        End,
        HeaderRead,
        DecoderSeek,
    }
    struct CancelErrorReader {
        inner: Cursor<Vec<u8>>,
        cancel: Arc<AtomicBool>,
        fail_at: FailAt,
        cancel_on_error: bool,
        rewinds: usize,
    }
    impl CancelErrorReader {
        fn failed(&self) -> std::io::Error {
            if self.cancel_on_error {
                self.cancel.store(true, Ordering::Release);
            }
            std::io::Error::other("source operation failed")
        }
    }
    impl std::io::Read for CancelErrorReader {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if matches!(self.fail_at, FailAt::HeaderRead) {
                return Err(self.failed());
            }
            std::io::Read::read(&mut self.inner, output)
        }
    }
    impl std::io::Seek for CancelErrorReader {
        fn seek(&mut self, request: std::io::SeekFrom) -> std::io::Result<u64> {
            if matches!(request, std::io::SeekFrom::Start(0)) {
                self.rewinds += 1;
            }
            let fail = matches!(
                (self.fail_at, request),
                (FailAt::Position, std::io::SeekFrom::Current(0))
                    | (FailAt::End, std::io::SeekFrom::End(0))
            ) || matches!(self.fail_at, FailAt::DecoderSeek) && self.rewinds == 2;
            if fail {
                return Err(self.failed());
            }
            std::io::Seek::seek(&mut self.inner, request)
        }
    }
    let bytes = with_icc(two_color_frames(), &iccp(SRGB_PROFILE));
    for fail_at in [
        FailAt::Position,
        FailAt::End,
        FailAt::HeaderRead,
        FailAt::DecoderSeek,
    ] {
        for cancel_on_error in [false, true] {
            let cancel = token();
            let reader = CancelErrorReader {
                inner: Cursor::new(bytes.clone()),
                cancel: cancel.clone(),
                fail_at,
                cancel_on_error,
                rewinds: 0,
            };
            let error = ApngSource::from_reader(reader, ApngOptions::default(), cancel.clone())
                .err()
                .unwrap();
            if cancel_on_error {
                assert!(
                    matches!(&error, ApngError::Cancelled),
                    "{fail_at:?}: {error}"
                );
            } else {
                assert!(matches!(&error, ApngError::Io(_)), "{fail_at:?}: {error}");
            }
            assert_eq!(cancel.load(Ordering::Acquire), cancel_on_error);
        }
    }
}
