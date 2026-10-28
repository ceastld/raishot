// 真实内存ICO/CUR/TIFF沿父模块样本进入完整集合链路，不访问组件或外部文件。
use super::*;
use std::sync::atomic::AtomicUsize;

const LIMIT: u64 = crate::image_decode::MAX_DECODE_BYTES;

struct CountedReader {
    data: Cursor<Vec<u8>>,
    reads: Arc<AtomicUsize>,
    seeks: Arc<AtomicUsize>,
}

impl Read for CountedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.data.read(output)
    }
}

impl Seek for CountedReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.seeks.fetch_add(1, Ordering::Relaxed);
        self.data.seek(position)
    }
}

fn counted(bytes: Vec<u8>) -> (CountedReader, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    let seeks = Arc::new(AtomicUsize::new(0));
    (
        CountedReader {
            data: Cursor::new(bytes),
            reads: reads.clone(),
            seeks: seeks.clone(),
        },
        reads,
        seeks,
    )
}

#[test]
fn image_viewer_pages_resident_real_collections_preserve_pixels_and_catalogs() {
    for (bytes, kind, expected) in [
        (icon(), Kind::Icon, [80, 40, 20, 128].repeat(4)),
        (cursor(), Kind::Cursor, [80, 40, 20, 128].repeat(4)),
        (
            tiff_fixture(1),
            Kind::Tiff,
            vec![0, 0, 255, 255, 0, 255, 0, 255],
        ),
    ] {
        let old = match kind {
            Kind::Icon => open_icon_source(
                IcoSource::from_reader(Cursor::new(&bytes), &token()).unwrap(),
                kind,
                Some(1),
                &token(),
            ),
            Kind::Cursor => open_reader_if_cursor(Cursor::new(&bytes), Some(1), &token()),
            Kind::Tiff => open_reader_if_tiff(Cursor::new(&bytes), 1, &token()),
            Kind::System(_) => unreachable!(),
        }
        .unwrap()
        .unwrap();
        let shared = open_reader_if_collection_with_resident(
            Cursor::new(&bytes),
            Some(1),
            32 * 1024 * 1024,
            &token(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(shared.info.kind, kind);
        assert_eq!(shared.info.kind, old.info.kind);
        assert_eq!(shared.info.count(), old.info.count());
        assert_eq!(shared.info.selected, old.info.selected);
        assert_eq!(shared.info.retained_bytes(), old.info.retained_bytes());
        let fields = |info: &Info| {
            info.entries
                .iter()
                .map(|entry| {
                    (
                        entry.index,
                        entry.dimensions,
                        entry.bits_per_pixel,
                        entry.hotspot,
                        entry.source_item,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(fields(&shared.info), fields(&old.info));
        let old = old.result.unwrap();
        let shared = shared.result.unwrap();
        assert_eq!(shared.metadata, old.metadata);
        assert_eq!(shared.raw_page.as_ref().unwrap().bgra, expected);
        assert_eq!(
            shared.raw_page.as_ref().unwrap().bgra,
            old.raw_page.as_ref().unwrap().bgra
        );
    }
}

#[test]
fn image_viewer_pages_resident_probe_and_cancel_precede_any_path_or_reader_access() {
    for cancelled in [false, true] {
        let cancel = AtomicBool::new(cancelled);
        let (reader, reads, seeks) = counted(icon());
        let error = open_reader_if_collection_with_resident(reader, None, LIMIT, &cancel)
            .err()
            .unwrap();
        let expected = if cancelled {
            check_cancel(&cancel).unwrap_err()
        } else {
            resident_error()
        };
        assert_eq!(error, expected);
        assert_eq!(reads.load(Ordering::Acquire), 0);
        assert_eq!(seeks.load(Ordering::Acquire), 0);
        let error = open_if_collection_with_resident(Path::new(""), None, LIMIT, &cancel)
            .err()
            .unwrap();
        assert_eq!(error, expected);
    }
    let exact = LIMIT - PROBE_BYTES;
    for bytes in [
        b"GIF89a".to_vec(),
        b"<svg/>".to_vec(),
        b"II\x2a\x00\x08\x00".to_vec(),
    ] {
        assert!(
            open_reader_if_collection_with_resident(Cursor::new(bytes), None, exact, &token())
                .unwrap()
                .is_none()
        );
    }
    assert!(resident_budget(&[exact, PROBE_BYTES], &token()).is_ok());
    assert!(resident_budget(&[exact + 1, PROBE_BYTES], &token()).is_err());
}

#[test]
fn image_viewer_pages_resident_constructor_gate_is_exact_and_only_for_collections() {
    let exact = LIMIT - CONSTRUCTOR_BYTES;
    let (reader, reads, seeks) = counted(tiff_fixture(1));
    assert_eq!(
        open_reader_if_tiff_with_resident(reader, 1, exact + 1, &token())
            .err()
            .unwrap(),
        resident_error()
    );
    assert_eq!(reads.load(Ordering::Acquire), 0);
    assert_eq!(seeks.load(Ordering::Acquire), 0);
    let opened =
        open_reader_if_tiff_with_resident(Cursor::new(tiff_fixture(1)), 1, exact, &token())
            .unwrap()
            .unwrap();
    assert_eq!(opened.info.count(), 2);
    assert_eq!(
        opened.result.unwrap().raw_page.as_ref().unwrap().bgra,
        [0, 0, 255, 255, 0, 255, 0, 255]
    );

    let opened =
        open_reader_if_collection_with_resident(Cursor::new(icon()), Some(1), exact, &token())
            .unwrap()
            .unwrap();
    assert_eq!(opened.info.count(), 2);
    // Info、来源目录和虚拟PNG与16MiB元数据工作区共存，不能把它们吞进同一份R。
    assert!(opened.result.is_err());
    let retry = open_reader_if_collection_with_resident(Cursor::new(icon()), Some(1), 0, &token())
        .unwrap()
        .unwrap();
    assert_eq!(
        retry.result.unwrap().raw_page.as_ref().unwrap().bgra,
        [80, 40, 20, 128].repeat(4)
    );
}

#[test]
fn image_viewer_pages_resident_info_uses_actual_capacity_and_checked_entry_boundaries() {
    let mut info = system_info(PlatformFormat::Heif, 2, 0, 7, true, (1, 1), None).unwrap();
    let before = info.retained_bytes();
    info.entries.reserve(31);
    assert_eq!(info.entries.len(), 1);
    assert!(info.retained_bytes() > before);
    assert_eq!(
        info.retained_bytes(),
        (info.entries.capacity() * std::mem::size_of::<Choice>()) as u64
    );
    let expected = info.entries.len() as u64 * std::mem::size_of::<Choice>() as u64;
    let exact = LIMIT - expected;
    let entries = choices(&info.entries, exact, &token(), Clone::clone).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries.capacity(), 1);
    assert!(choices(&info.entries, exact + 1, &token(), Clone::clone).is_err());
    assert!(resident_budget(&[u64::MAX, 1], &token()).is_err());
}

#[test]
fn image_viewer_pages_resident_loaded_phase_adds_info_once_and_prioritizes_cancel() {
    let info = system_info(PlatformFormat::Heif, 1, 0, 7, true, (2, 1), None).unwrap();
    let bytes = png(2, 1, [20, 40, 80, 128]);
    let decode = || crate::image_decode::decode_bytes(&bytes).unwrap();
    let raw_bytes = decode().image.bgra.capacity() as u64;
    let host =
        LIMIT - raw_bytes - super::super::super::THUMBNAIL_WORKING_BYTES - info.retained_bytes();
    let presentation = resident_budget(&[host, info.retained_bytes()], &token()).unwrap();
    let image = loaded_with_resident(decode(), &token(), presentation).unwrap();
    assert_eq!(
        image.raw_page.as_ref().unwrap().bgra,
        [80, 40, 20, 128].repeat(2)
    );
    assert!(loaded_with_resident(decode(), &token(), presentation + 1).is_err());
    let cancel = AtomicBool::new(true);
    let error = loaded_with_resident(decode(), &cancel, LIMIT)
        .err()
        .unwrap();
    assert_eq!(error, check_cancel(&cancel).unwrap_err());
}
