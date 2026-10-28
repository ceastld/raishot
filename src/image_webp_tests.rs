//! 用实际编码帧构造动画容器；像素、时序、循环及失败回归不依赖窗口或仓库外文件。
use super::*;
use crate::{
    image_color_management::{ColorConversion, UnappliedReason},
    image_container_metadata::MetadataStatus,
};
use std::{
    io::{self, Cursor, Read, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const GREEN_HALF: [u8; 4] = [0, 255, 0, 128];

struct TestFrame {
    rect: (u32, u32, u32, u32),
    payload: Vec<u8>,
    replace: bool,
    dispose: bool,
    delay: u32,
}

fn write_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) -> usize {
    let start = bytes.len();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    if payload.len() & 1 != 0 {
        bytes.push(0);
    }
    start
}

fn value24(bytes: &mut Vec<u8>, value: u32) {
    assert!(value <= 0xff_ffff);
    bytes.extend_from_slice(&value.to_le_bytes()[..3]);
}

fn declared_length(bytes: &[u8], offset: usize) -> usize {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn top_chunks(bytes: &[u8]) -> Vec<(&[u8], usize, usize)> {
    let mut result = Vec::new();
    let mut offset = 12;
    while offset < bytes.len() {
        let length = declared_length(bytes, offset + 4);
        result.push((&bytes[offset..offset + 4], offset, length));
        offset += 8 + length + (length & 1);
    }
    assert_eq!(offset, bytes.len());
    result
}

fn lossless_frame(
    rect: (u32, u32, u32, u32),
    rgba: &[[u8; 4]],
    replace: bool,
    dispose: bool,
) -> TestFrame {
    assert_eq!(rgba.len(), (rect.2 * rect.3) as usize);
    let rgba: Vec<u8> = rgba.iter().flatten().copied().collect();
    let mut encoded = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
        .encode(&rgba, rect.2, rect.3, image::ExtendedColorType::Rgba8)
        .unwrap();
    let mut payload = Vec::new();
    for (kind, offset, length) in top_chunks(&encoded) {
        if kind == b"VP8L" {
            payload.extend_from_slice(&encoded[offset..offset + 8 + length + (length & 1)]);
        }
    }
    assert!(!payload.is_empty());
    TestFrame {
        rect,
        payload,
        replace,
        dispose,
        delay: 25,
    }
}

fn lossless_animation(
    dimensions: (u32, u32),
    plays: u16,
    background_bgra: [u8; 4],
    frames: &[TestFrame],
    icc: Option<&[u8]>,
    exif: Option<&[u8]>,
) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    let mut extended = vec![
        0x12 | if icc.is_some() { 0x20 } else { 0 } | if exif.is_some() { 0x08 } else { 0 },
        0,
        0,
        0,
    ];
    value24(&mut extended, dimensions.0 - 1);
    value24(&mut extended, dimensions.1 - 1);
    write_chunk(&mut bytes, b"VP8X", &extended);
    if let Some(icc) = icc {
        write_chunk(&mut bytes, b"ICCP", icc);
    }
    let mut anim = background_bgra.to_vec();
    anim.extend_from_slice(&plays.to_le_bytes());
    write_chunk(&mut bytes, b"ANIM", &anim);
    for frame in frames {
        assert_eq!((frame.rect.0 & 1, frame.rect.1 & 1), (0, 0));
        let mut payload = Vec::new();
        value24(&mut payload, frame.rect.0 / 2);
        value24(&mut payload, frame.rect.1 / 2);
        value24(&mut payload, frame.rect.2 - 1);
        value24(&mut payload, frame.rect.3 - 1);
        value24(&mut payload, frame.delay);
        payload.push(u8::from(frame.dispose) | (u8::from(frame.replace) << 1));
        payload.extend_from_slice(&frame.payload);
        write_chunk(&mut bytes, b"ANMF", &payload);
    }
    if let Some(exif) = exif {
        write_chunk(&mut bytes, b"EXIF", exif);
    }
    let length = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    bytes
}

fn token() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn source(bytes: Vec<u8>) -> WebpSource<Cursor<Vec<u8>>> {
    WebpSource::from_reader(Cursor::new(bytes), WebpOptions::default(), token()).unwrap()
}

fn bgra(rgba: &[[u8; 4]]) -> Vec<u8> {
    rgba.iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
        .collect()
}

fn loop_fixture(plays: u16) -> Vec<u8> {
    lossless_animation(
        (3, 1),
        plays,
        [9, 19, 29, 64],
        &[
            lossless_frame((0, 0, 1, 1), &[RED], true, false),
            lossless_frame((2, 0, 1, 1), &[BLUE], true, false),
        ],
        None,
        None,
    )
}

fn rgba_at(image: &crate::capture::Image, x: usize, y: usize) -> [u8; 4] {
    let offset = (y * image.width as usize + x) * 4;
    let value = &image.bgra[offset..offset + 4];
    [value[2], value[1], value[0], value[3]]
}

fn over_reference(source: [u8; 4], destination: [u8; 4]) -> [u8; 4] {
    let source_alpha = f64::from(source[3]);
    let destination_alpha = f64::from(destination[3]);
    let alpha = source_alpha + destination_alpha * (1. - source_alpha / 255.);
    if alpha == 0. {
        return [0; 4];
    }
    let mut result = [0; 4];
    for channel in 0..3 {
        result[channel] = ((f64::from(source[channel]) * source_alpha
            + f64::from(destination[channel]) * destination_alpha * (1. - source_alpha / 255.))
            / alpha)
            .round() as u8;
    }
    result[3] = alpha.round() as u8;
    result
}

fn assert_close(actual: [u8; 4], expected: [u8; 4]) {
    for channel in 0..4 {
        assert!(
            actual[channel].abs_diff(expected[channel]) <= 1,
            "RGBA {actual:?}, expected {expected:?}"
        );
    }
}

#[test]
fn image_webp_source_replaces_transparent_pixels_and_over_blends_straight_alpha() {
    let hidden = [17, 29, 43, 0];
    let bytes = lossless_animation(
        (3, 1),
        1,
        [0; 4],
        &[
            lossless_frame((0, 0, 3, 1), &[RED; 3], true, false),
            lossless_frame((0, 0, 1, 1), &[hidden], true, false),
            lossless_frame((0, 0, 1, 1), &[GREEN_HALF], false, false),
        ],
        None,
        None,
    );
    let mut source = source(bytes);
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!(first.image().bgra, bgra(&[RED; 3]));
    let second = source.next_frame().unwrap().unwrap();
    assert_eq!(rgba_at(second.image(), 0, 0), hidden);
    assert_eq!(rgba_at(second.image(), 2, 0), RED);
    let third = source.next_frame().unwrap().unwrap();
    assert_eq!(rgba_at(third.image(), 0, 0), GREEN_HALF);
    assert_eq!(rgba_at(third.image(), 2, 0), RED);
    assert!(third.warning().is_none());
    assert_eq!(
        third.metadata_source().facts().color_conversion,
        ColorConversion::AssumedSrgb
    );
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_webp_disposal_clears_only_previous_rect_before_full_canvas_over() {
    let background_bgra = [20, 40, 60, 128];
    let background_rgba = [60, 40, 20, 128];
    let bytes = lossless_animation(
        (3, 1),
        1,
        background_bgra,
        &[
            lossless_frame((0, 0, 3, 1), &[RED; 3], true, false),
            lossless_frame((0, 0, 1, 1), &[BLUE], true, true),
            lossless_frame((0, 0, 3, 1), &[GREEN_HALF; 3], false, false),
        ],
        None,
        None,
    );
    let mut source = source(bytes);
    drop(source.next_frame().unwrap().unwrap());
    let second = source.next_frame().unwrap().unwrap();
    assert_eq!(rgba_at(second.image(), 0, 0), BLUE);
    assert_eq!(rgba_at(second.image(), 1, 0), RED);
    let third = source.next_frame().unwrap().unwrap();
    assert_close(
        rgba_at(third.image(), 0, 0),
        over_reference(GREEN_HALF, background_rgba),
    );
    assert_close(
        rgba_at(third.image(), 1, 0),
        over_reference(GREEN_HALF, RED),
    );
    assert_close(
        rgba_at(third.image(), 2, 0),
        over_reference(GREEN_HALF, RED),
    );
    assert_ne!(rgba_at(third.image(), 0, 0), rgba_at(third.image(), 1, 0));
}

#[test]
fn image_webp_finite_play_count_and_restart_reset_partial_first_frame_canvas() {
    let mut source = source(loop_fixture(2));
    assert_eq!(source.dimensions(), (3, 1));
    assert_eq!(source.num_plays(), 2);
    assert!(source.reserved_bytes() > 0);
    assert!(source.reserved_bytes() <= WebpOptions::default().max_total_bytes);
    for cycle in 0..2 {
        let first = source.next_frame().unwrap().unwrap();
        assert_eq!((first.generation, first.cycle, first.index), (0, cycle, 0));
        assert_eq!(rgba_at(first.image(), 0, 0), RED);
        assert_eq!(rgba_at(first.image(), 2, 0), [29, 19, 9, 64]);
        drop(first);
        let second = source.next_frame().unwrap().unwrap();
        assert_eq!((second.cycle, second.index), (cycle, 1));
        assert_eq!(rgba_at(second.image(), 2, 0), BLUE);
    }
    assert!(source.next_frame().unwrap().is_none());
    source.restart(token()).unwrap();
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!((first.generation, first.cycle, first.index), (1, 0, 0));
    assert_eq!(rgba_at(first.image(), 2, 0), [29, 19, 9, 64]);
}

#[test]
fn image_webp_output_leases_enforce_four_slots_across_restart_without_consuming_a_frame() {
    let mut source = source(loop_fixture(0));
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(source.next_frame().unwrap().unwrap());
    }
    assert!(matches!(source.next_frame(), Err(WebpError::Backpressure)));
    source.restart(token()).unwrap();
    assert!(matches!(source.next_frame(), Err(WebpError::Backpressure)));
    let first = held.remove(0);
    let first_pixels = first.image().bgra.clone();
    drop(first);
    let new = source.next_frame().unwrap().unwrap();
    assert_eq!((new.generation, new.cycle, new.index), (1, 0, 0));
    assert_eq!(new.image().bgra, first_pixels);
    assert_eq!(held[0].generation, 0);
    assert_eq!(rgba_at(held[0].image(), 2, 0), BLUE);
}

#[test]
fn image_webp_raw_zero_and_maximum_delays_survive_source_timing_policy() {
    let mut first = lossless_frame((0, 0, 1, 1), &[RED], true, false);
    first.delay = 0;
    let mut second = lossless_frame((0, 0, 1, 1), &[BLUE], true, false);
    second.delay = 0xff_ffff;
    let bytes = lossless_animation((1, 1), 1, [0; 4], &[first, second], None, None);
    let options = WebpOptions {
        zero_delay: Duration::from_millis(7),
        ..Default::default()
    };
    let mut source = WebpSource::from_reader(Cursor::new(bytes), options, token()).unwrap();
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!(first.timing.raw_delay_ms, 0);
    assert_eq!(first.timing.delay, Duration::from_millis(7));
    let second = source.next_frame().unwrap().unwrap();
    assert_eq!(second.timing.raw_delay_ms, 0xff_ffff);
    assert_eq!(second.timing.delay, Duration::from_millis(0xff_ffff));
}

#[test]
fn image_webp_nonempty_exif_is_explicitly_unsupported_for_animation() {
    let exif = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let bytes = lossless_animation(
        (1, 1),
        1,
        [0; 4],
        &[lossless_frame((0, 0, 1, 1), &[RED], true, false)],
        None,
        Some(exif),
    );
    assert!(matches!(
        WebpSource::from_reader(Cursor::new(bytes), WebpOptions::default(), token()),
        Err(WebpError::Unsupported(_))
    ));
}

#[test]
fn image_webp_bad_and_large_icc_keep_pixels_but_never_claim_conversion() {
    for (icc, status, reason) in [
        (
            b"broken profile".to_vec(),
            MetadataStatus::Present,
            UnappliedReason::InvalidProfile,
        ),
        (
            vec![0; 4 * 1024 * 1024 + 1],
            MetadataStatus::TooLarge,
            UnappliedReason::TooLarge,
        ),
    ] {
        let bytes = lossless_animation(
            (1, 1),
            1,
            [0; 4],
            &[lossless_frame((0, 0, 1, 1), &[GREEN_HALF], true, false)],
            Some(&icc),
            None,
        );
        let mut source = source(bytes);
        let frame = source.next_frame().unwrap().unwrap();
        assert_eq!(rgba_at(frame.image(), 0, 0), GREEN_HALF);
        assert_eq!(frame.metadata_source().facts().source.icc.status, status);
        assert_eq!(
            frame.metadata_source().facts().color_conversion,
            ColorConversion::EmbeddedUnapplied(reason)
        );
        assert!(frame.warning().is_some());
    }
}

#[test]
fn image_webp_canvas_input_output_and_single_frame_encoded_budgets_fail_before_delivery() {
    let bytes = loop_fixture(1);
    for options in [
        WebpOptions {
            max_canvas_pixels: 2,
            ..Default::default()
        },
        WebpOptions {
            max_input_bytes: bytes.len() as u64 - 1,
            ..Default::default()
        },
        WebpOptions {
            max_total_bytes: 64,
            ..Default::default()
        },
        WebpOptions {
            max_frame_encoded_bytes: 1,
            ..Default::default()
        },
        WebpOptions {
            max_output_frames: 0,
            ..Default::default()
        },
        WebpOptions {
            max_output_frames: 5,
            ..Default::default()
        },
        WebpOptions {
            zero_delay: Duration::ZERO,
            ..Default::default()
        },
    ] {
        assert!(WebpSource::from_reader(Cursor::new(bytes.clone()), options, token()).is_err());
    }
    let mut metadata = loop_fixture(1);
    metadata[20] |= 0x04;
    write_chunk(&mut metadata, b"XMP ", &vec![0; 8 * 1024 * 1024 + 1]);
    let length = (metadata.len() - 8) as u32;
    metadata[4..8].copy_from_slice(&length.to_le_bytes());
    assert!(
        WebpSource::from_reader(Cursor::new(metadata), WebpOptions::default(), token()).is_err()
    );
}

fn lossy_frame(rect: (u32, u32, u32, u32), alpha: bool) -> TestFrame {
    assert_eq!((rect.2, rect.3), (64, 32));
    let encoded = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
    let mut payload = Vec::new();
    if alpha {
        let mut plane = vec![0]; // 非压缩ALPH，沿用实际VP8样本的四象限alpha。
        for y in 0..32 {
            for x in 0..64 {
                plane.push(match (x >= 32, y >= 16) {
                    (false, false) => 255,
                    (true, false) => 128,
                    (false, true) => 0,
                    (true, true) => 64,
                });
            }
        }
        write_chunk(&mut payload, b"ALPH", &plane);
    }
    for (kind, offset, length) in top_chunks(encoded) {
        if kind == b"VP8 " {
            payload.extend_from_slice(&encoded[offset..offset + 8 + length + (length & 1)]);
        }
    }
    assert!(!payload.is_empty());
    TestFrame {
        rect,
        payload,
        replace: true,
        dispose: false,
        delay: 25,
    }
}

fn decoded_static_frame(frame: &TestFrame, alpha: bool) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    let mut extended = vec![if alpha { 0x10 } else { 0 }, 0, 0, 0];
    value24(&mut extended, frame.rect.2 - 1);
    value24(&mut extended, frame.rect.3 - 1);
    write_chunk(&mut bytes, b"VP8X", &extended);
    bytes.extend_from_slice(&frame.payload);
    let length = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
    let mut rgba = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap()
        .into_rgba8()
        .into_raw();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    rgba
}

#[test]
fn image_webp_real_lossy_vp8_with_raw_alpha_matches_static_pixels_and_exact_alpha() {
    let frame = lossy_frame((0, 0, 64, 32), true);
    let expected = decoded_static_frame(&frame, true);
    let bytes = lossless_animation((64, 32), 1, [0; 4], &[frame], None, None);
    let mut source = source(bytes);
    let frame = source.next_frame().unwrap().unwrap();
    assert_eq!(frame.image().bgra, expected);
    for (x, y, alpha) in [(8, 8, 255), (48, 8, 128), (8, 24, 0), (48, 24, 64)] {
        assert_eq!(rgba_at(frame.image(), x, y)[3], alpha);
    }
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_webp_real_lossy_vp8_with_compressed_alpha_matches_static_pixels_and_exact_alpha() {
    let encoded = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
    let mut frame = lossy_frame((0, 0, 64, 32), false);
    frame.payload.clear();
    let mut saw_compressed_alpha = false;
    let mut saw_vp8 = false;
    for (kind, offset, length) in top_chunks(encoded) {
        if kind == b"ALPH" {
            assert!(length > 1);
            assert_eq!(encoded[offset + 8] & 3, 1, "actual compressed ALPH sample");
            saw_compressed_alpha = true;
        } else if kind == b"VP8 " {
            saw_vp8 = true;
        } else {
            continue;
        }
        // 连同原始块长度与padding逐字节保留，不重新编码或重建alpha平面。
        frame
            .payload
            .extend_from_slice(&encoded[offset..offset + 8 + length + (length & 1)]);
    }
    assert!(saw_compressed_alpha && saw_vp8);
    let expected = decoded_static_frame(&frame, true);
    let bytes = lossless_animation((64, 32), 1, [0; 4], &[frame], None, None);
    let mut source = source(bytes);
    let frame = source.next_frame().unwrap().unwrap();
    assert_eq!(frame.image().bgra, expected);
    for (x, y, alpha) in [(8, 8, 255), (48, 8, 128), (8, 24, 0), (48, 24, 64)] {
        assert_eq!(rgba_at(frame.image(), x, y)[3], alpha);
    }
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_webp_opaque_vp8_after_disposal_preserves_rgba_canvas_stride_and_background() {
    let background = [11, 23, 37, 80];
    let frame = lossy_frame((64, 0, 64, 32), false);
    let expected = decoded_static_frame(&frame, false);
    let bytes = lossless_animation(
        (128, 32),
        1,
        background,
        &[
            lossless_frame((0, 0, 64, 32), &vec![RED; 64 * 32], true, true),
            frame,
        ],
        None,
        None,
    );
    let mut source = source(bytes);
    drop(source.next_frame().unwrap().unwrap());
    let frame = source.next_frame().unwrap().unwrap();
    for y in 0..32 {
        let row = &frame.image().bgra[y * 128 * 4..(y + 1) * 128 * 4];
        assert!(row[..64 * 4]
            .chunks_exact(4)
            .all(|pixel| pixel == background));
        assert_eq!(&row[64 * 4..], &expected[y * 64 * 4..(y + 1) * 64 * 4]);
    }
}

#[derive(Clone)]
struct ControlledInput {
    cursor: Arc<Mutex<Cursor<Vec<u8>>>>,
    reads: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    cancel_on_read: Arc<AtomicBool>,
    fail_on_read: Arc<AtomicBool>,
    mutate_after_read: Arc<Mutex<Option<(usize, usize)>>>,
    fail_and_cancel_on_seek_end: Arc<AtomicBool>,
}

impl ControlledInput {
    fn new(bytes: Vec<u8>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            cursor: Arc::new(Mutex::new(Cursor::new(bytes))),
            reads: Arc::new(AtomicUsize::new(0)),
            cancel,
            cancel_on_read: token(),
            fail_on_read: token(),
            mutate_after_read: Arc::new(Mutex::new(None)),
            fail_and_cancel_on_seek_end: token(),
        }
    }
}

impl Read for ControlledInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.fail_on_read.swap(false, Ordering::Relaxed) {
            if self.cancel_on_read.swap(false, Ordering::Relaxed) {
                self.cancel.store(true, Ordering::Relaxed);
            }
            return Err(io::Error::other("injected image source read failure"));
        }
        let mut cursor = self.cursor.lock().unwrap();
        let position = cursor.position() as usize;
        let count = cursor.read(output)?;
        let mut mutation = self.mutate_after_read.lock().unwrap();
        if let Some((trigger, changed)) = *mutation {
            if position <= trigger && position + count > trigger {
                cursor.get_mut()[changed] ^= 1;
                *mutation = None;
            }
        }
        drop(mutation);
        drop(cursor);
        if self.cancel_on_read.swap(false, Ordering::Relaxed) {
            self.cancel.store(true, Ordering::Relaxed);
        }
        Ok(count)
    }
}

impl Seek for ControlledInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if matches!(position, SeekFrom::End(_))
            && self
                .fail_and_cancel_on_seek_end
                .swap(false, Ordering::Relaxed)
        {
            self.cancel.store(true, Ordering::Relaxed);
            return Err(io::Error::other("injected image source seek failure"));
        }
        self.cursor.lock().unwrap().seek(position)
    }
}

#[test]
fn image_webp_constructor_cancellation_takes_priority_over_simultaneous_read_or_seek_error() {
    for fail_seek in [false, true] {
        let cancel = token();
        let reader = ControlledInput::new(loop_fixture(1), cancel.clone());
        if fail_seek {
            reader
                .fail_and_cancel_on_seek_end
                .store(true, Ordering::Relaxed);
        } else {
            reader.cancel_on_read.store(true, Ordering::Relaxed);
            reader.fail_on_read.store(true, Ordering::Relaxed);
        }
        assert!(matches!(
            WebpSource::from_reader(reader, WebpOptions::default(), cancel),
            Err(WebpError::Cancelled)
        ));
    }
}

#[test]
fn image_webp_cancel_and_read_failure_poison_delivery_until_restart_without_leaking_slots() {
    let cancel = token();
    cancel.store(true, Ordering::Relaxed);
    let reader = ControlledInput::new(loop_fixture(0), cancel.clone());
    assert!(matches!(
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), cancel),
        Err(WebpError::Cancelled)
    ));
    assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
    for cancel_read in [true, false] {
        let cancel = token();
        let reader = ControlledInput::new(loop_fixture(0), cancel.clone());
        let mut source =
            WebpSource::from_reader(reader.clone(), WebpOptions::default(), cancel).unwrap();
        if cancel_read {
            reader.cancel_on_read.store(true, Ordering::Relaxed);
        } else {
            reader.fail_on_read.store(true, Ordering::Relaxed);
        }
        let failure = source.next_frame();
        if cancel_read {
            assert!(matches!(failure, Err(WebpError::Cancelled)));
        } else {
            assert!(failure.is_err());
        }
        assert!(matches!(
            source.next_frame(),
            Err(WebpError::RequiresRestart)
        ));
        source.restart(token()).unwrap();
        let first = source.next_frame().unwrap().unwrap();
        assert_eq!((first.generation, first.cycle, first.index), (1, 0, 0));
        assert_eq!(rgba_at(first.image(), 0, 0), RED);
    }
}

#[test]
fn image_webp_same_length_icc_anim_and_frame_declaration_changes_cannot_reuse_frozen_state() {
    for wanted in [b"ICCP", b"ANIM", b"ANMF"] {
        let bytes = lossless_animation(
            (3, 1),
            1,
            [9, 19, 29, 64],
            &[lossless_frame((0, 0, 1, 1), &[RED], true, false)],
            Some(b"broken profile"),
            None,
        );
        let offset = top_chunks(&bytes)
            .iter()
            .find(|(kind, _, _)| *kind == wanted)
            .unwrap()
            .1;
        let reader = ControlledInput::new(bytes, token());
        let mut source =
            WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
        let field = offset + 8 + if wanted == b"ANMF" { 12 } else { 0 };
        reader.cursor.lock().unwrap().get_mut()[field] ^= 1;
        assert!(
            source.next_frame().is_err(),
            "changed {}",
            String::from_utf8_lossy(wanted)
        );
        assert!(matches!(
            source.next_frame(),
            Err(WebpError::RequiresRestart)
        ));
    }
}

#[test]
fn image_webp_next_frame_read_count_is_independent_of_future_frame_count() {
    fn first_frame_reads(frame_count: usize) -> usize {
        let encoded = lossless_frame((0, 0, 1, 1), &[RED], true, false);
        let frames: Vec<_> = (0..frame_count)
            .map(|_| TestFrame {
                rect: encoded.rect,
                payload: encoded.payload.clone(),
                replace: true,
                dispose: false,
                delay: 25,
            })
            .collect();
        let bytes = lossless_animation((1, 1), 1, [0; 4], &frames, None, None);
        let reader = ControlledInput::new(bytes, token());
        let mut source =
            WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
        reader.reads.store(0, Ordering::Relaxed);
        let frame = source.next_frame().unwrap().unwrap();
        assert_eq!(rgba_at(frame.image(), 0, 0), RED);
        assert_eq!((frame.cycle, frame.index), (0, 0));
        reader.reads.load(Ordering::Relaxed)
    }
    let small = first_frame_reads(2);
    let large = first_frame_reads(100);
    assert!(small > 0);
    assert_eq!(
        small, large,
        "delivering frame zero must not reread all future frame declarations"
    );
}

#[test]
fn image_webp_future_frame_declaration_change_is_rejected_before_that_frame_delivery() {
    let bytes = loop_fixture(1);
    let second = top_chunks(&bytes)
        .into_iter()
        .filter(|(kind, _, _)| *kind == b"ANMF")
        .nth(1)
        .unwrap()
        .1;
    let reader = ControlledInput::new(bytes, token());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
    // 第二帧duration低位等长变化；第一帧的声明和像素仍保持构造时的身份。
    reader.cursor.lock().unwrap().get_mut()[second + 8 + 12] ^= 1;
    let first = source.next_frame().unwrap().unwrap();
    assert_eq!((first.cycle, first.index), (0, 0));
    assert_eq!(first.timing.raw_delay_ms, 25);
    assert_eq!(rgba_at(first.image(), 0, 0), RED);
    assert!(matches!(source.next_frame(), Err(WebpError::Invalid(_))));
    assert!(matches!(
        source.next_frame(),
        Err(WebpError::RequiresRestart)
    ));
}

#[test]
fn image_webp_icc_changed_immediately_after_profile_read_cannot_match_a_second_read_hash() {
    let bytes = lossless_animation(
        (1, 1),
        1,
        [0; 4],
        &[lossless_frame((0, 0, 1, 1), &[RED], true, false)],
        Some(b"broken profile"),
        None,
    );
    let (_, offset, length) = top_chunks(&bytes)
        .into_iter()
        .find(|(kind, _, _)| *kind == b"ICCP")
        .unwrap();
    let reader = ControlledInput::new(bytes, token());
    // 第一次完整ICC读回给调用者的仍是旧字节；来源紧接着等长变化。
    // profile指纹若再从来源读一次，就会错误地把新指纹套给旧profile。
    *reader.mutate_after_read.lock().unwrap() = Some((offset + 8 + length - 1, offset + 8));
    assert!(matches!(
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()),
        Err(WebpError::Invalid(_))
    ));
    assert!(reader.mutate_after_read.lock().unwrap().is_none());
}

#[test]
fn image_webp_large_canvas_budget_counts_presentation_long_edge_and_profile_before_pixels() {
    let bytes = loop_fixture(1);
    let length = bytes.len() as u64;
    let options = WebpOptions::default();
    let mut scanned = scan(&mut Cursor::new(bytes), 0, length, options, &token()).unwrap();
    // 单独检查目录扫描阶段的画布部分；完整帧工作集由后续冻结观察得出。
    scanned.width = 10_000;
    scanned.height = 400;
    let normal = canvas_budget(&scanned, options, 0).unwrap();
    let normal_rows = FrameColorManager::row_buffer_bytes(scanned.width).unwrap() as u64;
    scanned.width = 20_000;
    scanned.height = 200;
    let long = canvas_budget(&scanned, options, 0).unwrap();
    let long_rows = FrameColorManager::row_buffer_bytes(scanned.width).unwrap() as u64;
    assert_eq!(long - normal, 4 * 4_000_000 + long_rows - normal_rows);
    // 一份原色画布、四份交付租约及长边呈现的24P都必须覆盖全画布。
    assert!(long >= 4_000_000 * (4 * 5 + 24));
    let exact = WebpOptions {
        max_total_bytes: long,
        ..options
    };
    assert_eq!(canvas_budget(&scanned, exact, 0).unwrap(), long);
    assert!(canvas_budget(
        &scanned,
        WebpOptions {
            max_total_bytes: long - 1,
            ..options
        },
        0,
    )
    .is_err());
    let profile_reserve = 3 * 13_000;
    let profile_exact = WebpOptions {
        max_total_bytes: long + profile_reserve as u64,
        ..options
    };
    assert_eq!(
        canvas_budget(&scanned, profile_exact, profile_reserve).unwrap(),
        long + profile_reserve as u64
    );
    assert!(canvas_budget(
        &scanned,
        WebpOptions {
            max_total_bytes: profile_exact.max_total_bytes - 1,
            ..options
        },
        profile_reserve,
    )
    .is_err());
}

#[test]
fn image_webp_prefixed_reader_preserves_fixed_container_origin_and_dimensions() {
    let prefix = b"prefix outside the image";
    let mut encoded = prefix.to_vec();
    encoded.extend(loop_fixture(1));
    let mut reader = Cursor::new(encoded);
    reader.seek(SeekFrom::Start(prefix.len() as u64)).unwrap();
    let mut source = WebpSource::from_reader(reader, WebpOptions::default(), token()).unwrap();
    assert_eq!(source.dimensions(), (3, 1));
    assert_eq!(
        rgba_at(source.next_frame().unwrap().unwrap().image(), 0, 0),
        RED
    );
}

#[test]
fn image_webp_riff_ranges_frame_rects_and_inner_bitstream_sizes_are_checked_before_decode() {
    let base = loop_fixture(1);
    let anmf = top_chunks(&base)
        .iter()
        .find(|(kind, _, _)| *kind == b"ANMF")
        .unwrap()
        .1;
    let mut bad_riff = base.clone();
    bad_riff[4..8].copy_from_slice(&(base.len() as u32).to_le_bytes());
    let mut bad_range = base.clone();
    bad_range[anmf + 4..anmf + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut outside = base.clone();
    outside[anmf + 8] = 2;
    let mut mismatch = base.clone();
    mismatch[anmf + 8 + 6] = 1;
    let mut huge_side = base.clone();
    huge_side[24..27].copy_from_slice(&[0xff, 0xff, 0]);
    let mut trailing = base.clone();
    trailing.push(0);
    let anim = top_chunks(&base)
        .iter()
        .find(|(kind, _, _)| *kind == b"ANIM")
        .unwrap()
        .1;
    let mut missing_anim = base.clone();
    missing_anim.drain(anim..anim + 14);
    let length = (missing_anim.len() - 8) as u32;
    missing_anim[4..8].copy_from_slice(&length.to_le_bytes());
    for bytes in [
        bad_riff,
        bad_range,
        outside,
        mismatch,
        huge_side,
        trailing,
        missing_anim,
    ] {
        assert!(
            WebpSource::from_reader(Cursor::new(bytes), WebpOptions::default(), token()).is_err()
        );
    }
}

#[test]
fn image_webp_truncated_meta_huffman_is_rejected_before_pixel_decode() {
    let mut payload = Vec::new();
    // VP8L 1×1启用meta-Huffman但没有后续地图/表；不能当作合法复杂编码。
    write_chunk(&mut payload, b"VP8L", &[0x2f, 0, 0, 0, 0x10, 0b0000_0100]);
    let frame = TestFrame {
        rect: (0, 0, 1, 1),
        payload,
        replace: true,
        dispose: false,
        delay: 25,
    };
    let bytes = lossless_animation((1, 1), 1, [0; 4], &[frame], None, None);
    assert!(WebpSource::from_reader(Cursor::new(bytes), WebpOptions::default(), token()).is_err());
}

#[test]
fn image_webp_real_lossless_encoder_preserves_straight_pixels() {
    let rgba = [RED, BLUE, GREEN_HALF, [61, 79, 103, 177]];
    let frame = lossless_frame((0, 0, 2, 2), &rgba, true, false);
    let original = lossless_animation((2, 2), 1, [0; 4], std::slice::from_ref(&frame), None, None);
    let delivered = source(original).next_frame().unwrap().unwrap();
    assert_eq!(delivered.image().bgra, bgra(&rgba));
}

#[path = "image_webp_constructor_tests.rs"]
mod constructor;
#[path = "image_webp_full_lossless_tests.rs"]
mod full_lossless;
#[path = "image_webp_unknown_tests.rs"]
mod unknown;

#[test]
#[ignore = "requires the Windows ICC engine; run explicitly with native color checks"]
fn image_webp_native_linear_icc_is_applied_once_after_composition_and_preserves_alpha() {
    let profile = include_bytes!("../tests/data/image_color/linear-rgb.icc");
    let bytes = lossless_animation(
        (3, 1),
        1,
        [0; 4],
        &[
            lossless_frame((0, 0, 1, 1), &[[128, 128, 128, 128]], true, false),
            lossless_frame((2, 0, 1, 1), &[[64, 64, 64, 64]], true, false),
        ],
        Some(profile),
        None,
    );
    let mut source = source(bytes);
    let first = source.next_frame().unwrap().unwrap();
    let second = source.next_frame().unwrap().unwrap();
    assert_eq!(
        first.metadata_source().facts().color_conversion,
        ColorConversion::ConvertedSrgb
    );
    assert!(first.warning().is_none());
    for frame in [&first, &second] {
        let rgba = rgba_at(frame.image(), 0, 0);
        assert!(rgba[..3].iter().all(|value| value.abs_diff(188) <= 2));
        assert_eq!(rgba[3], 128);
    }
    let rgba = rgba_at(second.image(), 2, 0);
    assert!(rgba[..3].iter().all(|value| value.abs_diff(137) <= 2));
    assert_eq!(rgba[3], 64);
    assert!(Arc::ptr_eq(
        first.metadata_source(),
        second.metadata_source()
    ));
}
