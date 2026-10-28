use super::*;

#[derive(Clone)]
struct ConstructorInput {
    cursor: Arc<Mutex<Cursor<Vec<u8>>>>,
    reads: Arc<AtomicUsize>,
    seeks: Arc<AtomicUsize>,
    profile: std::ops::Range<usize>,
    profile_reads: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    cancel_profile: Arc<AtomicBool>,
}
impl ConstructorInput {
    fn new(bytes: Vec<u8>, cancel: Arc<AtomicBool>) -> Self {
        let profile = if bytes.len() >= 12 {
            top_chunks(&bytes)
                .into_iter()
                .find(|(kind, _, _)| *kind == b"ICCP")
                .map_or(0..0, |(_, offset, length)| offset + 8..offset + 8 + length)
        } else {
            0..0
        };
        Self {
            cursor: Arc::new(Mutex::new(Cursor::new(bytes))),
            reads: Arc::new(AtomicUsize::new(0)),
            seeks: Arc::new(AtomicUsize::new(0)),
            profile,
            profile_reads: Arc::new(AtomicUsize::new(0)),
            cancel,
            cancel_profile: token(),
        }
    }
}
impl Read for ConstructorInput {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut cursor = self.cursor.lock().unwrap();
        let position = cursor.position() as usize;
        let count = cursor.read(output)?;
        if position < self.profile.end && position + count > self.profile.start {
            self.profile_reads.fetch_add(1, Ordering::Relaxed);
            if self.cancel_profile.swap(false, Ordering::Relaxed) {
                self.cancel.store(true, Ordering::Release);
            }
        }
        Ok(count)
    }
}
impl Seek for ConstructorInput {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seeks.fetch_add(1, Ordering::Relaxed);
        self.cursor.lock().unwrap().seek(position)
    }
}

fn repeated_frames(count: usize) -> Vec<TestFrame> {
    let first = lossless_frame((0, 0, 1, 1), &[RED], true, false);
    (0..count)
        .map(|_| TestFrame {
            rect: first.rect,
            payload: first.payload.clone(),
            replace: first.replace,
            dispose: first.dispose,
            delay: first.delay,
        })
        .collect()
}

fn refresh_riff_length(bytes: &mut [u8]) {
    let length = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&length.to_le_bytes());
}

#[test]
fn image_webp_constructor_minimum_and_cancel_fail_before_read_or_seek() {
    for (invalid_options, pre_cancelled) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let cancel = Arc::new(AtomicBool::new(pre_cancelled));
        let reader = ConstructorInput::new(Vec::new(), cancel.clone());
        let options = WebpOptions {
            max_total_bytes: CONSTRUCTOR_FIXED_BYTES - 1,
            zero_delay: if invalid_options {
                Duration::ZERO
            } else {
                Duration::from_millis(100)
            },
            ..Default::default()
        };
        let result = WebpSource::from_reader(reader.clone(), options, cancel);
        if pre_cancelled {
            assert!(matches!(result, Err(WebpError::Cancelled)));
        } else {
            assert!(matches!(result, Err(WebpError::Invalid(_))));
        }
        assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
        assert_eq!(reader.seeks.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn image_webp_constructor_directory_growth_counts_old_new_and_actual_capacities() {
    let options = WebpOptions {
        max_total_bytes: CONSTRUCTOR_FIXED_BYTES + 3 * 16 * 32 - 1,
        ..Default::default()
    };
    let mut ledger = ConstructorLedger::new(options, &token()).unwrap();
    let mut values = Vec::<[u8; 32]>::new();
    for _ in 0..16 {
        ledger.push(&mut values, [0; 32], 64, &token()).unwrap();
    }
    let capacity = values.capacity();
    assert_eq!(
        ledger.directories,
        vector_bytes::<[u8; 32]>(capacity).unwrap()
    );
    assert!(ledger.push(&mut values, [0; 32], 64, &token()).is_err());
    assert_eq!((values.len(), values.capacity()), (16, capacity));

    let mut bytes = lossless_animation((1, 1), 1, [0; 4], &repeated_frames(3000), None, None);
    for _ in 0..64 {
        write_chunk(&mut bytes, b"JUNK", &[]);
    }
    refresh_riff_length(&mut bytes);
    let scanned = scan(
        &mut Cursor::new(&bytes),
        0,
        bytes.len() as u64,
        WebpOptions::default(),
        &token(),
    )
    .unwrap();
    assert!(scanned.frames.capacity() > 16);
    assert!(scanned.controls.capacity() > 16);
    assert!(scanned.global_controls.capacity() > 16);
    assert!(scanned.constructor_peak > CONSTRUCTOR_FIXED_BYTES + scanned.directory_bytes());
    let exact = WebpOptions {
        max_total_bytes: scanned.constructor_peak,
        ..Default::default()
    };
    assert_eq!(
        scan(
            &mut Cursor::new(&bytes),
            0,
            bytes.len() as u64,
            exact,
            &token()
        )
        .unwrap()
        .constructor_peak,
        scanned.constructor_peak
    );
    assert!(scan(
        &mut Cursor::new(&bytes),
        0,
        bytes.len() as u64,
        WebpOptions {
            max_total_bytes: exact.max_total_bytes - 1,
            ..exact
        },
        &token()
    )
    .is_err());
}

#[test]
fn image_webp_constructor_icc_is_charged_before_read_and_by_actual_capacity() {
    let mut owned = Some(Vec::with_capacity(4096));
    owned.as_mut().unwrap().extend_from_slice(&[1, 2]);
    let capacity = owned.as_ref().unwrap().capacity();
    let mut ledger = ConstructorLedger::new(WebpOptions::default(), &token()).unwrap();
    ledger.profile_capacity(capacity).unwrap();
    assert_eq!(ledger.profile, 3 * capacity as u64);
    let peak = ledger.peak;
    ledger.discard_profile(&mut owned);
    assert!(owned.is_none());
    assert_eq!(ledger.profile, 0);
    assert_eq!(ledger.peak, peak);
    let mut lower = ConstructorLedger::new(
        WebpOptions {
            max_total_bytes: CONSTRUCTOR_FIXED_BYTES + 3 * capacity as u64 - 1,
            ..Default::default()
        },
        &token(),
    )
    .unwrap();
    assert!(lower.profile_capacity(capacity).is_err());
    assert_eq!(lower.profile, 0);

    let profile = vec![0x5a; 4096];
    let bytes = lossless_animation((1, 1), 1, [0; 4], &repeated_frames(1), Some(&profile), None);
    let cancel = token();
    let reader = ConstructorInput::new(bytes.clone(), cancel.clone());
    let result = WebpSource::from_reader(
        reader.clone(),
        WebpOptions {
            max_total_bytes: CONSTRUCTOR_FIXED_BYTES + 3 * profile.len() as u64 - 1,
            ..Default::default()
        },
        cancel,
    );
    assert!(matches!(result, Err(WebpError::Invalid(_))));
    assert!(reader.reads.load(Ordering::Relaxed) > 0);
    assert_eq!(reader.profile_reads.load(Ordering::Relaxed), 0);

    let first = scan(
        &mut Cursor::new(&bytes),
        0,
        bytes.len() as u64,
        WebpOptions::default(),
        &token(),
    )
    .unwrap();
    assert!(
        first.constructor_peak
            >= CONSTRUCTOR_FIXED_BYTES + 3 * first.profile.as_ref().unwrap().capacity() as u64
    );
    let mut duplicate = bytes;
    write_chunk(&mut duplicate, b"ICCP", &[0x5a; 2]);
    refresh_riff_length(&mut duplicate);
    let duplicate = scan(
        &mut Cursor::new(&duplicate),
        0,
        duplicate.len() as u64,
        WebpOptions::default(),
        &token(),
    )
    .unwrap();
    assert!(duplicate.profile.is_none());
    assert_eq!(duplicate.icc.status, MetadataStatus::Malformed);
    assert!(duplicate.constructor_peak >= first.constructor_peak);
}

#[test]
fn image_webp_constructor_exact_source_plays_and_profile_read_cancel_wins() {
    let bytes = lossless_animation((1, 1), 1, [0; 4], &repeated_frames(3), None, None);
    let required = source(bytes.clone()).reserved_bytes();
    let mut exact = WebpSource::from_reader(
        Cursor::new(bytes),
        WebpOptions {
            max_total_bytes: required,
            ..Default::default()
        },
        token(),
    )
    .unwrap();
    assert_eq!(exact.reserved_bytes(), required);
    for index in 0..3 {
        let frame = exact.next_frame().unwrap().unwrap();
        assert_eq!(frame.index, index);
        assert_eq!(frame.image().bgra, bgra(&[RED]));
    }
    assert!(exact.next_frame().unwrap().is_none());

    let profile = vec![0x5a; 4096];
    let bytes = lossless_animation((1, 1), 1, [0; 4], &repeated_frames(1), Some(&profile), None);
    let cancel = token();
    let reader = ConstructorInput::new(bytes, cancel.clone());
    reader.cancel_profile.store(true, Ordering::Relaxed);
    let result = WebpSource::from_reader(reader.clone(), WebpOptions::default(), cancel);
    assert!(matches!(result, Err(WebpError::Cancelled)));
    assert_eq!(reader.profile_reads.load(Ordering::Relaxed), 1);
}
