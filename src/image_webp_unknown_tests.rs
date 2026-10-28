use super::*;

const EXTENSION: &[u8; 4] = b"zZ01";

#[derive(Clone)]
struct ChildRange {
    kind: [u8; 4],
    header: usize,
    payload: std::ops::Range<usize>,
    end: usize,
}

fn children(bytes: &[u8], start: usize, end: usize) -> Vec<ChildRange> {
    let mut output = Vec::new();
    let mut position = start;
    while position < end {
        assert!(end - position >= 8);
        let size = declared_length(bytes, position + 4);
        let payload = position + 8..position + 8 + size;
        let next = payload.end + (size & 1);
        assert!(next <= end);
        output.push(ChildRange {
            kind: bytes[position..position + 4].try_into().unwrap(),
            header: position,
            payload,
            end: next,
        });
        position = next;
    }
    assert_eq!(position, end);
    output
}

fn copy_frame(frame: &TestFrame, payload: Vec<u8>) -> TestFrame {
    TestFrame {
        rect: frame.rect,
        payload,
        replace: frame.replace,
        dispose: frame.dispose,
        delay: frame.delay,
    }
}

fn extended(frame: &TestFrame, position: usize) -> TestFrame {
    let mut payload = frame.payload[..position].to_vec();
    for bytes in [&[][..], &[17, 23, 31][..], &b"EXIF\0ICCP\0VP8L"[..]] {
        write_chunk(&mut payload, EXTENSION, bytes);
    }
    payload.extend_from_slice(&frame.payload[position..]);
    copy_frame(frame, payload)
}

fn ordinary(frame: &TestFrame) -> Vec<u8> {
    let encoded = lossless_animation(
        (frame.rect.2, frame.rect.3),
        1,
        [0; 4],
        &[copy_frame(frame, frame.payload.clone())],
        None,
        None,
    );
    crate::image_decode::decode_bytes(&encoded)
        .unwrap()
        .image
        .bgra
}

#[derive(Clone)]
struct UnknownInput {
    cursor: Arc<Mutex<Cursor<Vec<u8>>>>,
    children: Vec<ChildRange>,
    reads: Arc<AtomicUsize>,
    payload_reads: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    cancel_header: Arc<AtomicBool>,
}
impl UnknownInput {
    fn new(bytes: Vec<u8>, cancel: Arc<AtomicBool>) -> Self {
        let (_, frame, length) = top_chunks(&bytes)
            .into_iter()
            .find(|(kind, _, _)| *kind == b"ANMF")
            .unwrap();
        let children = children(&bytes, frame + 8 + 16, frame + 8 + length)
            .into_iter()
            .filter(|child| matches!(&child.kind, b"zZ01" | b"RIFF" | b"WEBP"))
            .collect();
        Self {
            cursor: Arc::new(Mutex::new(Cursor::new(bytes))),
            children,
            reads: Arc::new(AtomicUsize::new(0)),
            payload_reads: Arc::new(AtomicUsize::new(0)),
            cancel,
            cancel_header: token(),
        }
    }
}
impl Read for UnknownInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut cursor = self.cursor.lock().unwrap();
        let position = cursor.position() as usize;
        let end = position
            + output
                .len()
                .min(cursor.get_ref().len().saturating_sub(position));
        if self.children.iter().any(|child| {
            !child.payload.is_empty() && position < child.payload.end && end > child.payload.start
        }) {
            self.payload_reads.fetch_add(1, Ordering::Relaxed);
            return Err(io::Error::other(
                "ignored extension payload must not be read",
            ));
        }
        let count = cursor.read(output)?;
        if self
            .children
            .iter()
            .any(|child| position < child.header + 8 && position + count > child.header)
            && self.cancel_header.swap(false, Ordering::Relaxed)
        {
            self.cancel.store(true, Ordering::Release);
        }
        Ok(count)
    }
}
impl Seek for UnknownInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cursor.lock().unwrap().seek(position)
    }
}

#[test]
fn image_webp_unknown_trailing_chunks_preserve_ordinary_lossless_pixels_without_reading_payload() {
    let frame = lossless_frame(
        (0, 0, 2, 2),
        &[RED, GREEN_HALF, BLUE, [23, 61, 101, 0]],
        true,
        false,
    );
    let expected = ordinary(&frame);
    let mut extended = extended(&frame, frame.payload.len());
    // 外层RIFF标记及WEBP form type并不是帧内已知子块，作为扩展仍应忽略。
    write_chunk(&mut extended.payload, b"RIFF", &[3, 5, 7]);
    write_chunk(&mut extended.payload, b"WEBP", &[11, 13]);
    assert_eq!(ordinary(&extended), expected);
    let bytes = lossless_animation((2, 2), 1, [0; 4], &[extended], None, None);
    let reader = UnknownInput::new(bytes, token());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
    let frame = source.next_frame().unwrap().unwrap();
    assert_eq!(frame.image().bgra, expected);
    assert!(source.next_frame().unwrap().is_none());
    assert_eq!(reader.payload_reads.load(Ordering::Relaxed), 0);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 1);
    drop(frame);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
}

#[test]
fn image_webp_unknown_prefix_and_interleaved_chunks_are_read_tolerantly_without_changing_alpha() {
    let frame = lossy_frame((0, 0, 64, 32), true);
    let expected = ordinary(&frame);
    let first = children(&frame.payload, 0, frame.payload.len())[0].clone();
    assert_eq!(first.kind, *b"ALPH");
    for position in [0, first.end, frame.payload.len()] {
        let frame = extended(&frame, position);
        let bytes = lossless_animation((64, 32), 1, [0; 4], &[frame], None, None);
        let reader = UnknownInput::new(bytes, token());
        let delivered = WebpSource::from_reader(reader.clone(), WebpOptions::default(), token())
            .unwrap()
            .next_frame()
            .unwrap()
            .unwrap();
        assert_eq!(delivered.image().bgra, expected);
        assert_eq!(reader.payload_reads.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn image_webp_unknown_chunks_shrink_the_frozen_view_but_keep_the_original_frame_quota() {
    let plain = lossless_frame((0, 0, 1, 1), &[GREEN_HALF], true, false);
    let frame = extended(&plain, plain.payload.len());
    let bytes = lossless_animation((1, 1), 1, [0; 4], &[frame], None, None);
    let original = top_chunks(&bytes)
        .into_iter()
        .find(|(kind, _, _)| *kind == b"ANMF")
        .unwrap()
        .2 as u64;
    let mut reader = Cursor::new(&bytes);
    let scanned = scan(
        &mut reader,
        0,
        bytes.len() as u64,
        WebpOptions::default(),
        &token(),
    )
    .unwrap();
    let frame = scanned.frames[0];
    let length = frame_virtual_length(frame).unwrap();
    assert_eq!(length, 30 + plain.payload.len() as u64);
    assert!(length < original + 14);
    let base = source_base(&scanned, WebpOptions::default(), 0, 4).unwrap();
    let frozen = freeze_frame(&mut reader, 0, frame, base, MAX_BYTES, &token()).unwrap();
    assert_eq!(frozen.len() as u64, length);
    assert_eq!(declared_length(&frozen, 4) + 8, frozen.len());
    let kinds: Vec<_> = top_chunks(&frozen)
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert_eq!(kinds, [b"VP8X".as_slice(), b"VP8L".as_slice()]);
    let mut exact = WebpSource::from_reader(
        Cursor::new(&bytes),
        WebpOptions {
            max_frame_encoded_bytes: original,
            ..Default::default()
        },
        token(),
    )
    .unwrap();
    assert_eq!(
        exact.next_frame().unwrap().unwrap().image().bgra,
        ordinary(&plain)
    );
    drop(exact);
    drop(reader);
    assert!(WebpSource::from_reader(
        Cursor::new(bytes),
        WebpOptions {
            max_frame_encoded_bytes: original - 1,
            ..Default::default()
        },
        token(),
    )
    .is_err());
}

#[test]
fn image_webp_unknown_chunks_do_not_hide_known_order_duplicates_or_bad_ranges_and_padding() {
    let frame = lossy_frame((0, 0, 64, 32), true);
    let parts = children(&frame.payload, 0, frame.payload.len());
    let alpha = &frame.payload[..parts[0].end];
    let vp8 = &frame.payload[parts[1].header..parts[1].end];
    let mut payloads = Vec::new();
    for (left, right) in [(vp8, alpha), (alpha, alpha), (vp8, vp8)] {
        let mut payload = left.to_vec();
        write_chunk(&mut payload, EXTENSION, &[1]);
        payload.extend_from_slice(right);
        payloads.push(payload);
    }
    let lossless = lossless_frame((0, 0, 64, 32), &vec![RED; 64 * 32], true, false);
    let mut duplicate_lossless = lossless.payload.clone();
    write_chunk(&mut duplicate_lossless, EXTENSION, &[]);
    duplicate_lossless.extend_from_slice(&lossless.payload);
    payloads.push(duplicate_lossless);
    let mut alpha_lossless = alpha.to_vec();
    write_chunk(&mut alpha_lossless, EXTENSION, &[]);
    alpha_lossless.extend_from_slice(&lossless.payload);
    payloads.push(alpha_lossless);
    for kind in [b"VP8X", b"ANIM", b"ANMF", b"ICCP", b"EXIF", b"XMP "] {
        let mut payload = frame.payload.clone();
        write_chunk(&mut payload, kind, &[]);
        payloads.push(payload);
    }
    let mut padding = frame.payload.clone();
    write_chunk(&mut padding, EXTENSION, &[7]);
    *padding.last_mut().unwrap() = 1;
    payloads.push(padding);
    let mut range = frame.payload.clone();
    range.extend_from_slice(EXTENSION);
    range.extend_from_slice(&64u32.to_le_bytes());
    payloads.push(range);
    let mut only_unknown = Vec::new();
    write_chunk(&mut only_unknown, EXTENSION, &[7]);
    payloads.push(only_unknown);
    for payload in payloads {
        let candidate = copy_frame(&frame, payload);
        let bytes = lossless_animation((64, 32), 1, [0; 4], &[candidate], None, None);
        assert!(
            WebpSource::from_reader(Cursor::new(bytes), WebpOptions::default(), token()).is_err()
        );
    }
}

#[test]
fn image_webp_unknown_payload_changes_are_ignored_but_header_changes_reject_without_leaking_slots()
{
    let plain = lossless_frame((0, 0, 1, 1), &[GREEN_HALF], true, false);
    let expected = ordinary(&plain);
    let frame = extended(&plain, plain.payload.len());
    let bytes = lossless_animation((1, 1), 0, [0; 4], &[frame], None, None);
    let reader = UnknownInput::new(bytes.clone(), token());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
    let changed = reader
        .children
        .iter()
        .find(|child| !child.payload.is_empty())
        .unwrap()
        .payload
        .start;
    reader.cursor.lock().unwrap().get_mut()[changed] ^= 1;
    let mut held = Vec::new();
    for _ in 0..4 {
        let frame = source.next_frame().unwrap().unwrap();
        assert_eq!(frame.image().bgra, expected);
        held.push(frame);
    }
    let reads = reader.reads.load(Ordering::Relaxed);
    assert!(matches!(source.next_frame(), Err(WebpError::Backpressure)));
    assert_eq!(reader.reads.load(Ordering::Relaxed), reads);
    assert_eq!(reader.payload_reads.load(Ordering::Relaxed), 0);
    drop(held);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);

    let reader = UnknownInput::new(bytes, token());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
    let header = reader.children[0].header;
    reader.cursor.lock().unwrap().get_mut()[header..header + 4].copy_from_slice(b"VP8L");
    assert!(source.next_frame().is_err());
    assert_eq!(source.index, 0);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
    assert!(matches!(
        source.next_frame(),
        Err(WebpError::RequiresRestart)
    ));
}

#[test]
fn image_webp_unknown_header_read_cancellation_releases_slot_and_restart_recovers() {
    let plain = lossless_frame((0, 0, 1, 1), &[RED], true, false);
    let frame = extended(&plain, plain.payload.len());
    let bytes = lossless_animation((1, 1), 1, [0; 4], &[frame], None, None);
    let cancel = token();
    let reader = UnknownInput::new(bytes, cancel.clone());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), cancel).unwrap();
    reader.cancel_header.store(true, Ordering::Relaxed);
    assert!(matches!(source.next_frame(), Err(WebpError::Cancelled)));
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
    source.restart(token()).unwrap();
    let frame = source.next_frame().unwrap().unwrap();
    assert_eq!((frame.generation, frame.index), (1, 0));
    assert_eq!(frame.image().bgra, bgra(&[RED]));
    assert_eq!(reader.payload_reads.load(Ordering::Relaxed), 0);
}
