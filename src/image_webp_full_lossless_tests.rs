use super::*;

#[derive(Default)]
struct BitsWriter {
    bytes: Vec<u8>,
    bit: usize,
}
impl BitsWriter {
    fn put(&mut self, value: u32, count: u8) {
        for shift in 0..count {
            if self.bit / 8 == self.bytes.len() {
                self.bytes.push(0);
            }
            self.bytes[self.bit / 8] |= (((value >> shift) & 1) as u8) << (self.bit & 7);
            self.bit += 1;
        }
    }
    fn code(&mut self, value: u32, count: u8) {
        for shift in (0..count).rev() {
            self.put((value >> shift) & 1, 1);
        }
    }
    fn single(&mut self, value: u8) {
        self.put(1, 1);
        self.put(0, 1);
        self.put(1, 1);
        self.put(u32::from(value), 8);
    }
    fn normal(&mut self, alphabet: usize, nonzero: &[(usize, u8)]) {
        self.put(0, 1);
        self.put(2, 4); // 17/18/0/1/2/3 六个描述；0/1/2/3长度为1/2/3/3。
        for length in [0, 0, 1, 2, 3, 3] {
            self.put(length, 3);
        }
        self.put(0, 1);
        for symbol in 0..alphabet {
            match nonzero
                .iter()
                .find(|&&(s, _)| s == symbol)
                .map_or(0, |&(_, l)| l)
            {
                0 => self.code(0, 1),
                1 => self.code(2, 2),
                2 => self.code(6, 3),
                3 => self.code(7, 3),
                _ => panic!("fixture length"),
            }
        }
    }
    fn constant_map(&mut self, rgba: [u8; 4]) {
        self.put(0, 1);
        for value in [rgba[1], rgba[0], rgba[2], rgba[3], 0] {
            self.single(value);
        }
    }
    fn alternating_map(&mut self, count: usize) {
        assert!(count >= 8 && count % 2 == 0);
        let length = prefix_for(count - 4);
        self.put(1, 1);
        self.put(1, 4); // 两个不同的cache槽。
        self.normal(
            282,
            &[
                (1, 2),
                (2, 2),
                (256 + usize::from(length.0), 2),
                (280, 3),
                (281, 3),
            ],
        );
        for value in [0, 0, 255, 4] {
            self.single(value);
        }
        self.code(0, 2);
        self.code(1, 2); // literal模式1、2。
        self.code(6, 3);
        self.code(7, 3); // cache模式1、2。
        self.code(2, 2);
        self.put(length.1, length.2);
        self.put(1, 1); // distance prefix4+extra1 -> plane6 -> 物理距离2。
    }
}

fn prefix_for(value: usize) -> (u16, u32, u8) {
    for code in 0..24u16 {
        let bits = if code < 4 { 0 } else { ((code - 2) / 2) as u8 };
        let first = if code < 4 {
            usize::from(code + 1)
        } else {
            ((2 + (usize::from(code) & 1)) << bits) + 1
        };
        if value >= first && value - first < 1 << bits {
            return (code, (value - first) as u32, bits);
        }
    }
    panic!("fixture copy length");
}

struct RichStream {
    data: Vec<u8>,
    first_meta_bit: usize,
}

fn rich_stream(dimensions: (u32, u32), wide_meta: bool) -> RichStream {
    let mut bits = BitsWriter::default();
    bits.put(1, 1);
    bits.put(3, 2);
    bits.put(16, 8); // 17色，不缩宽。
    bits.constant_map([17, 33, 71, 128]);
    let count = dimensions.0.div_ceil(4) as usize * dimensions.1.div_ceil(4) as usize;
    for kind in [0, 1] {
        bits.put(1, 1);
        bits.put(kind, 2);
        bits.put(0, 3);
        bits.alternating_map(count);
    }
    bits.put(1, 1);
    bits.put(2, 2);
    bits.put(0, 1); // 四变换完成。
    bits.put(0, 1); // 主cache。
    bits.put(1, 1);
    bits.put(0, 3); // meta map。
    bits.put(0, 1); // meta map cache。
    bits.normal(280, &[(0, 1), (255, 1)]);
    for _ in 0..4 {
        bits.single(0);
    }
    let first_meta_bit = bits.bit;
    for i in 0..count {
        bits.code(u32::from(wide_meta && i == 0), 1);
    }
    // 256组均是真正完整表；主图为零残差。低组版本的余下字节不被像素解码器消费。
    for _ in 0..256 {
        for _ in 0..5 {
            bits.single(0);
        }
    }
    bits.bytes.extend_from_slice(&[0; 8]);
    RichStream {
        data: bits.bytes,
        first_meta_bit,
    }
}

fn rich_frame(dimensions: (u32, u32), wide_meta: bool) -> (TestFrame, usize, u8) {
    let stream = rich_stream(dimensions, wide_meta);
    let header = (dimensions.0 - 1) | ((dimensions.1 - 1) << 14) | (1 << 28);
    let mut payload = vec![0x2f];
    payload.extend_from_slice(&header.to_le_bytes());
    payload.extend_from_slice(&stream.data);
    let mut chunks = Vec::new();
    write_chunk(&mut chunks, b"VP8L", &payload);
    (
        TestFrame {
            rect: (0, 0, dimensions.0, dimensions.1),
            payload: chunks,
            replace: true,
            dispose: false,
            delay: 25,
        },
        5 + stream.first_meta_bit / 8,
        1 << (stream.first_meta_bit & 7),
    )
}

#[test]
fn image_webp_full_lossless_four_transforms_nonconstant_maps_and_256_groups_match_pixels() {
    let (frame, _, _) = rich_frame((8, 16), true);
    let expected = decoded_static_frame(&frame, true);
    // palette最先声明、最后还原；零索引取首色，不能把predictor的alpha当输出。
    assert!(expected.chunks_exact(4).all(|p| p == [71, 33, 17, 128]));
    let bytes = lossless_animation((8, 16), 1, [0; 4], &[frame], None, None);
    let mut source = source(bytes);
    let delivered = source.next_frame().unwrap().unwrap();
    assert_eq!(delivered.image().bgra, expected);
    assert_eq!((delivered.index, delivered.cycle), (0, 0));
    assert!(source.next_frame().unwrap().is_none());
}

#[test]
fn image_webp_full_lossless_compressed_alpha_maps_and_meta_match_actual_static_codec() {
    let stream = rich_stream((64, 32), true);
    let mut frame = lossy_frame((0, 0, 64, 32), false);
    let opaque = std::mem::take(&mut frame.payload);
    let mut alpha = vec![1]; // 隐含尺寸的完整lossless流，非VP8L文件头。
    alpha.extend_from_slice(&stream.data);
    write_chunk(&mut frame.payload, b"ALPH", &alpha);
    frame.payload.extend_from_slice(&opaque);
    let expected = decoded_static_frame(&frame, true);
    assert!(expected.chunks_exact(4).all(|p| p[3] == 33));
    let bytes = lossless_animation((64, 32), 1, [0; 4], &[frame], None, None);
    let delivered = source(bytes).next_frame().unwrap().unwrap();
    assert_eq!(delivered.image().bgra, expected);
}

#[test]
fn image_webp_full_lossless_frozen_capacity_and_exact_source_budget_are_charged() {
    let (frame, _, _) = rich_frame((8, 16), true);
    let bytes = lossless_animation((8, 16), 1, [0; 4], &[frame], None, None);
    let initial = source(bytes.clone());
    let exact = initial.reserved_bytes();
    drop(initial);
    let options = WebpOptions {
        max_total_bytes: exact,
        ..WebpOptions::default()
    };
    let mut exact_source =
        WebpSource::from_reader(Cursor::new(bytes.clone()), options, token()).unwrap();
    assert_eq!(exact_source.reserved_bytes(), exact);
    assert!(exact_source.next_frame().unwrap().is_some());
    assert!(WebpSource::from_reader(
        Cursor::new(bytes.clone()),
        WebpOptions {
            max_total_bytes: exact - 1,
            ..options
        },
        token()
    )
    .is_err());

    let cancel = token();
    let mut reader = Cursor::new(bytes);
    let length = reader.get_ref().len() as u64;
    let scanned = scan(&mut reader, 0, length, WebpOptions::default(), &cancel).unwrap();
    let frame = scanned.frames[0];
    let base = source_base(&scanned, WebpOptions::default(), 0, 8 * 16 * 4).unwrap();
    let frozen = freeze_frame(&mut reader, 0, frame, base, MAX_BYTES, &cancel).unwrap();
    let (_, original) = inspect_frame(&frozen, frame, base, MAX_BYTES, &cancel).unwrap();
    let mut spare = Vec::with_capacity(frozen.capacity() + 1024);
    spare.extend_from_slice(&frozen);
    let (_, grown) = inspect_frame(&spare, frame, base, MAX_BYTES, &cancel).unwrap();
    assert_eq!(
        grown - original,
        (spare.capacity() - frozen.capacity()) as u64
    );
    assert!(inspect_frame(&spare, frame, base, base + grown - 1, &cancel).is_err());
    let mut boundary = frame;
    boundary.chunks[0].as_mut().unwrap().length = MAX_FRAME_BYTES - 24;
    assert_eq!(
        frame_virtual_length(boundary).unwrap(),
        MAX_FRAME_BYTES + 14
    );
    boundary.chunks[0].as_mut().unwrap().length += 1;
    assert!(frame_virtual_length(boundary).is_err());
}

#[test]
fn image_webp_full_lossless_color_warning_pool_keeps_the_announced_base_stable() {
    let reserve = 3 * 588;
    assert_eq!(color_pool(reserve, 0, 0, reserve, 4).unwrap(), 0);
    let warning = color_pool(588 + 128, 128, 0, reserve, 4).unwrap();
    assert_eq!(warning, 128);
    // 失败后raw ICC只剩一份，warning由固定池承担，不重新抬高来源B。
    let bytes = loop_fixture(1);
    let scanned = scan(
        &mut Cursor::new(&bytes),
        0,
        bytes.len() as u64,
        WebpOptions::default(),
        &token(),
    )
    .unwrap();
    let before = canvas_budget(&scanned, WebpOptions::default(), reserve).unwrap();
    assert_eq!(
        color_pool(588 + 128, 128, warning, reserve, 4).unwrap(),
        warning
    );
    assert_eq!(
        canvas_budget(&scanned, WebpOptions::default(), reserve).unwrap(),
        before
    );
    let largest = MAX_METADATA as usize / 8;
    assert_eq!(
        color_pool(588 + largest, largest, 0, reserve, 4).unwrap(),
        largest
    );
    assert!(color_pool(588 + largest + 1, largest + 1, 0, reserve, 4).is_err());
    assert_eq!(
        color_pool(588 + 1, 1, largest, reserve, 4).unwrap(),
        largest
    );
    assert!(color_pool(reserve + 1, 0, 0, reserve, 4).is_err());
    assert!(color_pool(0, 1, 0, reserve, 4).is_err());
    assert!(color_pool(usize::MAX, usize::MAX, 0, reserve, 4).is_err());
}

#[derive(Clone)]
struct TailInput {
    cursor: Arc<Mutex<Cursor<Vec<u8>>>>,
    tail: std::ops::Range<usize>,
    reads: Arc<AtomicUsize>,
    mutation: Arc<Mutex<Option<(usize, u8)>>>,
    cancel: Arc<AtomicBool>,
    cancel_tail: Arc<AtomicBool>,
}
impl TailInput {
    fn new(bytes: Vec<u8>, cancel: Arc<AtomicBool>) -> (Self, usize) {
        let (_, frame, frame_length) = top_chunks(&bytes)
            .into_iter()
            .find(|(kind, _, _)| *kind == b"ANMF")
            .unwrap();
        let payload = frame + 8 + 16 + 8;
        assert_eq!(&bytes[payload - 8..payload - 4], b"VP8L");
        let length = declared_length(&bytes, payload - 4);
        assert!(length > 64);
        let end = payload.checked_add(length).unwrap();
        assert_eq!(end + (length & 1), frame + 8 + frame_length);
        assert!(end <= bytes.len());
        (
            Self {
                cursor: Arc::new(Mutex::new(Cursor::new(bytes))),
                tail: payload + 64..end,
                reads: Arc::new(AtomicUsize::new(0)),
                mutation: Arc::new(Mutex::new(None)),
                cancel,
                cancel_tail: token(),
            },
            payload,
        )
    }
}
impl Read for TailInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let mut cursor = self.cursor.lock().unwrap();
        let position = cursor.position() as usize;
        let count = cursor.read(output)?;
        if position < self.tail.end && position + count > self.tail.start {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if position + count >= self.tail.end {
                if let Some((offset, mask)) = self.mutation.lock().unwrap().take() {
                    cursor.get_mut()[offset] ^= mask;
                }
                if self.cancel_tail.swap(false, Ordering::Relaxed) {
                    self.cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        Ok(count)
    }
}
impl Seek for TailInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cursor.lock().unwrap().seek(position)
    }
}

#[test]
fn image_webp_full_lossless_tail_is_frozen_once_and_budget_changes_are_rechecked() {
    let (frame, meta_offset, meta_mask) = rich_frame((8, 16), false);
    assert!(meta_offset >= 64);
    let low_expected = decoded_static_frame(&frame, true);
    let mut high = TestFrame {
        payload: frame.payload.clone(),
        ..frame
    };
    high.payload[8 + meta_offset] ^= meta_mask;
    assert_eq!(decoded_static_frame(&high, true), low_expected); // 256组版本也是真实可解码输入。
    let bytes = lossless_animation((8, 16), 0, [0; 4], &[frame], None, None);
    let (reader, payload) = TailInput::new(bytes, token());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), token()).unwrap();
    reader.reads.store(0, Ordering::Relaxed);
    *reader.mutation.lock().unwrap() = Some((payload + meta_offset, meta_mask));
    let delivered = source.next_frame().unwrap().unwrap();
    assert_eq!(delivered.image().bgra, low_expected);
    assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
    assert!(reader.mutation.lock().unwrap().is_none());
    // 同长尾部在冻结后变成更多组：第一帧仍用冻结低组字节，下一轮不得越公告预算。
    assert!(source.next_frame().is_err());
    assert!(matches!(
        source.next_frame(),
        Err(WebpError::RequiresRestart)
    ));
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 1);
    drop(delivered);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
}

#[test]
fn image_webp_full_lossless_freeze_cancellation_releases_lease_and_restart_recovers() {
    let (frame, _, _) = rich_frame((8, 16), true);
    let bytes = lossless_animation((8, 16), 0, [0; 4], &[frame], None, None);
    let cancel = token();
    let (reader, _) = TailInput::new(bytes, cancel.clone());
    let mut source =
        WebpSource::from_reader(reader.clone(), WebpOptions::default(), cancel).unwrap();
    reader.cancel_tail.store(true, Ordering::Relaxed);
    assert!(matches!(source.next_frame(), Err(WebpError::Cancelled)));
    assert_eq!(source.index, 0);
    assert_eq!(source.outputs.live.load(Ordering::Acquire), 0);
    assert!(matches!(
        source.next_frame(),
        Err(WebpError::RequiresRestart)
    ));
    source.restart(token()).unwrap();
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(source.next_frame().unwrap().unwrap());
    }
    let before = reader.reads.load(Ordering::Relaxed);
    assert!(matches!(source.next_frame(), Err(WebpError::Backpressure)));
    assert_eq!(reader.reads.load(Ordering::Relaxed), before);
    source.restart(token()).unwrap();
    assert!(matches!(source.next_frame(), Err(WebpError::Backpressure)));
    drop(held);
    assert_eq!(source.next_frame().unwrap().unwrap().index, 0);
}
