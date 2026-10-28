use super::*;
use crate::image_animation::{GifOptions, GifSource};
use std::{borrow::Cow, fs, io::Cursor, sync::atomic::AtomicU64, time::UNIX_EPOCH};

pub(super) struct TempDir {
    root: PathBuf,
    pub(super) path: PathBuf,
}

impl TempDir {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().canonicalize().unwrap();
        let name = format!(
            "rustshot-viewer-snapshot-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        );
        let path = root.join(name);
        fs::create_dir(&path).unwrap();
        Self { root, path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 只清理本测试创建、且解析后仍是临时根直接子目录的目标。
        if let Ok(resolved) = self.path.canonicalize() {
            if resolved.parent() == Some(self.root.as_path()) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }
}

#[test]
fn image_viewer_snapshot_gif_uses_the_captured_raw_frame_without_a_readable_source() {
    let mut bytes = Vec::new();
    {
        let palette = [9, 19, 29, 201, 41, 19, 11, 183, 97];
        let mut encoder = gif::Encoder::new(&mut bytes, 3, 2, &palette).unwrap();
        encoder
            .write_frame(&gif::Frame {
                width: 3,
                height: 2,
                transparent: Some(0),
                buffer: Cow::Borrowed(&[1, 0, 2, 0, 1, 2]),
                ..Default::default()
            })
            .unwrap();
    }
    let mut source = GifSource::from_reader(
        Cursor::new(bytes),
        GifOptions::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let frame = Arc::new(source.next_frame().unwrap().unwrap());
    let temporary = TempDir::new();
    let path = temporary.path.join("absent-directory").join("source.gif");
    let request = SnapshotRequest {
        serial: 1,
        path,
        frame: Some(frame.clone()),
        raw_page: None,
        metadata_source: None,
        resident_bytes: frame.image().bgra.capacity() as u64,
        target: SnapshotTarget::Editor,
    };
    let snapshot = source_snapshot(&request).unwrap();
    assert_eq!((snapshot.width, snapshot.height), (3, 2));
    assert_eq!(
        snapshot.bgra,
        [
            19, 41, 201, 255, 0, 0, 0, 0, 97, 183, 11, 255, 0, 0, 0, 0, 19, 41, 201, 255, 97, 183,
            11, 255,
        ]
    );
    assert_eq!(snapshot.bgra, frame.image().bgra);
    assert_ne!(snapshot.bgra.as_ptr(), frame.image().bgra.as_ptr());
    assert!(!request.path.exists());
}

#[test]
fn image_viewer_snapshot_apng_keeps_the_selected_raw_frame_after_source_closes() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(2, 1).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[255, 0, 0, 255, 255, 0, 0, 255])
            .unwrap();
        writer.set_blend_op(png::BlendOp::Source).unwrap();
        writer
            .write_image_data(&[23, 61, 107, 80, 0, 0, 0, 0])
            .unwrap();
        writer.finish().unwrap();
    }
    let mut source = crate::image_apng::ApngSource::from_reader(
        Cursor::new(bytes),
        crate::image_apng::ApngOptions::default(),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    drop(source.next_frame().unwrap().unwrap());
    let frame = Arc::new(source.next_frame().unwrap().unwrap());
    assert_eq!(frame.index, 1);
    drop(source);
    let temporary = TempDir::new();
    let request = SnapshotRequest {
        serial: 1,
        path: temporary.path.join("absent-source.png"),
        frame: Some(frame.clone()),
        raw_page: None,
        metadata_source: None,
        resident_bytes: frame.image().bgra.capacity() as u64,
        target: SnapshotTarget::Tools,
    };
    let snapshot = source_snapshot(&request).unwrap();
    assert_eq!((snapshot.width, snapshot.height), (2, 1));
    assert_eq!(snapshot.bgra, [107, 61, 23, 80, 0, 0, 0, 0]);
    assert_eq!(snapshot.bgra, frame.image().bgra);
    assert_ne!(snapshot.bgra.as_ptr(), frame.image().bgra.as_ptr());
    assert!(!request.path.exists());
}

pub(super) fn static_png(dimensions: (u32, u32), rgba: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, dimensions.0, dimensions.1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // 固定存储块使等尺寸A/B长度相同，回归不能靠长度变化被旧stamp逻辑挡住。
        encoder.set_compression(png::Compression::NoCompression);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        writer.finish().unwrap();
    }
    bytes
}

#[test]
fn image_viewer_snapshot_static_freeze_survives_same_stamp_replace_and_delete() {
    let temporary = TempDir::new();
    let path = temporary.path.join("source.png");
    let original = static_png(
        (2, 2),
        &[
            0, 30, 60, 255, 90, 120, 150, 0, 180, 210, 240, 80, 20, 70, 130, 254,
        ],
    );
    let replacement = static_png((2, 2), &[1, 2, 3, 255].repeat(4));
    assert_ne!(original, replacement);
    assert_eq!(original.len(), replacement.len());
    fs::write(&path, &original).unwrap();
    let stamp = file_stamp(&path).unwrap();
    // 生产load必须保留直alpha原色，不能从已铺棋盘格的显示图倒推快照。
    let loaded = load(
        &path,
        crate::image_decode::MAX_DECODE_BYTES,
        &AtomicBool::new(false),
    )
    .unwrap();
    let raw = loaded
        .raw_page
        .as_ref()
        .expect("ordinary static load must retain raw pixels")
        .clone();
    let metadata = loaded
        .metadata_source
        .as_ref()
        .expect("loaded PNG metadata sidecar")
        .clone();
    let request = SnapshotRequest {
        serial: 1,
        path: path.clone(),
        frame: None,
        raw_page: loaded.raw_page.clone(),
        metadata_source: loaded.metadata_source.clone(),
        resident_bytes: presentation_resident_bytes(
            &loaded,
            std::iter::empty(),
            std::iter::empty(),
        ),
        target: SnapshotTarget::Tools,
    };
    assert!(Arc::ptr_eq(request.raw_page.as_ref().unwrap(), &raw));
    assert!(Arc::ptr_eq(
        request.metadata_source.as_ref().unwrap(),
        &metadata
    ));
    fs::write(&path, replacement).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(stamp.1))
        .unwrap();
    assert_eq!(file_stamp(&path), Some(stamp));
    let snapshot = source_snapshot(&request).unwrap();
    assert_eq!((snapshot.width, snapshot.height), (2, 2));
    assert_eq!(
        snapshot.bgra,
        [60, 30, 0, 255, 150, 120, 90, 0, 240, 210, 180, 80, 130, 70, 20, 254,]
    );
    assert_eq!(snapshot.bgra, raw.bgra);
    assert_ne!(snapshot.bgra.as_ptr(), raw.bgra.as_ptr());
    drop(loaded);
    fs::remove_file(&path).unwrap();
    let after_close = source_snapshot(&request).unwrap();
    assert_eq!(after_close.bgra, snapshot.bgra);
    assert_ne!(after_close.bgra.as_ptr(), raw.bgra.as_ptr());
    assert!(Arc::ptr_eq(
        request.metadata_source.as_ref().unwrap(),
        &metadata
    ));
    assert!(!path.exists());
}

#[test]
fn image_viewer_snapshot_missing_frozen_pixels_cannot_fall_back_to_a_valid_path() {
    let temporary = TempDir::new();
    let path = temporary.path.join("valid-source.png");
    fs::write(&path, static_png((1, 1), &[23, 61, 107, 80])).unwrap();
    let loaded = load(
        &path,
        crate::image_decode::MAX_DECODE_BYTES,
        &AtomicBool::new(false),
    )
    .unwrap();
    let request = SnapshotRequest {
        serial: 1,
        path: path.clone(),
        frame: None,
        raw_page: None,
        metadata_source: loaded.metadata_source.clone(),
        resident_bytes: presentation_resident_bytes(
            &loaded,
            std::iter::empty(),
            std::iter::empty(),
        ),
        target: SnapshotTarget::Editor,
    };
    assert!(path.is_file());
    assert!(file_stamp(&path).is_some());
    assert!(source_snapshot(&request).is_err());
}

#[test]
fn image_viewer_snapshot_pre_cancel_rejects_even_a_complete_frozen_static_frame() {
    let temporary = TempDir::new();
    let path = temporary.path.join("source.png");
    fs::write(&path, static_png((1, 1), &[23, 61, 107, 80])).unwrap();
    let loaded = load(
        &path,
        crate::image_decode::MAX_DECODE_BYTES,
        &AtomicBool::new(false),
    )
    .unwrap();
    let request = SnapshotRequest {
        serial: 1,
        path: path.clone(),
        frame: None,
        raw_page: loaded.raw_page.clone(),
        metadata_source: loaded.metadata_source.clone(),
        resident_bytes: presentation_resident_bytes(
            &loaded,
            std::iter::empty(),
            std::iter::empty(),
        ),
        target: SnapshotTarget::Crop,
    };
    let raw = request.raw_page.as_ref().expect("loaded static pixels");
    let reason = source_snapshot_cancellable(&request, &AtomicBool::new(true))
        .err()
        .unwrap();
    assert!(
        reason.contains("取消") || reason.contains("canceled"),
        "{reason}"
    );
    assert_eq!(raw.bgra, [107, 61, 23, 80]);
    assert!(Arc::ptr_eq(
        request.metadata_source.as_ref().unwrap(),
        loaded.metadata_source.as_ref().unwrap()
    ));
}

#[test]
fn image_viewer_snapshot_budget_checks_copy_and_source_capacity_before_allocation() {
    let mut pixels = Vec::with_capacity(64);
    pixels.extend_from_slice(&[3, 2, 1, 0, 6, 5, 4, 128]);
    let raw = Arc::new(crate::capture::Image {
        width: 1,
        height: 2,
        bgra: pixels,
        readback_ms: 0.,
    });
    let mut request = SnapshotRequest {
        serial: 1,
        path: PathBuf::from("missing-budget-source.png"),
        frame: None,
        raw_page: Some(raw.clone()),
        metadata_source: None,
        resident_bytes: crate::image_decode::MAX_DECODE_BYTES - 8,
        target: SnapshotTarget::Editor,
    };
    assert_eq!(source_snapshot(&request).unwrap().bgra, raw.bgra);
    request.resident_bytes += 1;
    assert!(source_snapshot(&request).is_err());
    request.resident_bytes = u64::MAX;
    assert!(source_snapshot(&request).is_err());
    // len足够容纳像素不代表宿主核算过来源Vec多余容量。
    request.resident_bytes = raw.bgra.len() as u64;
    assert!(source_snapshot(&request).is_err());
    request.resident_bytes = raw.bgra.capacity() as u64;
    assert_eq!(source_snapshot(&request).unwrap().bgra, raw.bgra);
}

#[test]
fn image_viewer_snapshot_resident_budget_deduplicates_render_arcs_and_keeps_raw_capacity() {
    let temporary = TempDir::new();
    let path = temporary.path.join("source.png");
    fs::write(&path, static_png((1, 2), &[23, 61, 107, 80].repeat(2))).unwrap();
    let mut image = load(
        &path,
        crate::image_decode::MAX_DECODE_BYTES,
        &AtomicBool::new(false),
    )
    .unwrap();
    let raw = image.raw_page.as_mut().unwrap();
    let raw = Arc::get_mut(raw).unwrap();
    raw.bgra.reserve_exact(128);
    let base = presentation_resident_bytes(&image, std::iter::empty(), std::iter::empty());
    let metadata = image
        .metadata_source
        .as_ref()
        .map_or(0, |source| source.retained_bytes() as u64);
    assert!(!Arc::ptr_eq(&image.thumbnail, &image.tiles[0].1));
    assert_eq!(
        base,
        THUMBNAIL_CACHE_BYTES as u64
            + image.raw_page.as_ref().unwrap().bgra.capacity() as u64
            + metadata
            + 16 // 主图和独立缩略图各占 8 字节。
    );
    let rotated = crate::canvas_source::render_image(2, 1, vec![0; 8]);
    let retained = [
        image.thumbnail.clone(),
        image.tiles[0].1.clone(),
        rotated.clone(),
        rotated,
    ];
    assert_eq!(
        presentation_resident_bytes(&image, retained.iter(), std::iter::empty()),
        base + 8
    );
}
