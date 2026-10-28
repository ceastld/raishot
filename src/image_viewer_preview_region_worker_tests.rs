use super::snapshot_backpressure_tests::Worker;
use super::snapshot_tests::{static_png, TempDir};
use super::*;
use std::io::Write;

fn wide_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 200_000, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        {
            let mut stream = writer.stream_writer().unwrap();
            let mut block = [0u8; 4096];
            let mut start = 0u32;
            while start < 200_000 {
                let count = (200_000 - start).min(1024);
                for i in 0..count {
                    let x = start + i;
                    block[i as usize * 4..i as usize * 4 + 4].copy_from_slice(&[
                        (x % 251) as u8,
                        50 + (x % 101) as u8,
                        240 - (x % 97) as u8,
                        [0, 128, 255, 64][x as usize % 4],
                    ]);
                }
                stream.write_all(&block[..count as usize * 4]).unwrap();
                start += count;
            }
            stream.finish().unwrap();
        }
        writer.finish().unwrap();
    }
    bytes
}

fn load_preview(worker: &Worker, path: PathBuf) -> LoadedImage {
    worker.loader.request(Request {
        serial: 1,
        path,
        scan: false,
        page: None,
        raster: None,
        resident_bytes: THUMBNAIL_CACHE_BYTES as u64,
    });
    worker.wait_for(|mailbox| {
        mailbox
            .completed
            .as_ref()
            .is_some_and(|done| done.serial == 1)
    });
    let image = worker.loader.take().unwrap().result.unwrap();
    assert!(!image.has_full_pixels());
    assert!(image.preview_source.is_some());
    assert!(image.raw_page.is_none());
    assert!(image.raw_frame.is_none());
    image
}

fn crop() -> Rect {
    Rect {
        x: 100_000,
        y: 0,
        width: 4,
        height: 1,
    }
}

#[test]
fn image_viewer_preview_region_worker_holds_completion_and_defers_thumbnail_until_take() {
    let temporary = TempDir::new();
    let path = temporary.path.join("wide.png");
    std::fs::write(&path, wide_png()).unwrap();
    let worker = Worker::new(Mailbox::default());
    let image = load_preview(&worker, path);
    let source = image.preview_source.as_ref().unwrap().clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let other = presentation_resident_bytes_for_excluding_preview_source(
        Some(&image),
        std::iter::empty(),
        std::iter::empty(),
        Some(&source),
    )
    .checked_add(THUMBNAIL_CACHE_BYTES as u64)
    .unwrap();
    worker
        .loader
        .request_preview_region(preview_region::Request {
            serial: 1,
            epoch: 7,
            source: source.clone(),
            region: crop(),
            external_other_resident: other,
            cancel: cancel.clone(),
        });
    worker.wait_for(|mailbox| mailbox.preview_region_completed.is_some());
    let (waits, thumbnail_resident) = {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        let completion = mailbox.preview_region_completed.as_ref().unwrap();
        let surface = completion.result.as_ref().unwrap();
        // 即使take后worker立即被唤醒，这份grant也已计入仍由测试持有的Surface。
        let resident = presentation_resident_bytes_for_excluding_preview_source(
            Some(&image),
            std::iter::once(&surface.image),
            std::iter::empty(),
            None,
        )
        .checked_add(THUMBNAIL_CACHE_BYTES as u64)
        .unwrap();
        (mailbox.preview_region_backpressure_waits, resident)
    };
    worker.loader.request_thumbnails(ThumbnailRequest {
        serial: 1,
        epoch: 11,
        resident_bytes: thumbnail_resident,
        cancel: Arc::new(AtomicBool::new(false)),
        paths: [(3, temporary.path.join("missing-thumbnail.png"))].into(),
    });
    worker.wait_for(|mailbox| mailbox.preview_region_backpressure_waits > waits);
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert!(mailbox.thumbnails.is_empty());
        assert_eq!(mailbox.thumbnail_request.as_ref().unwrap().paths.len(), 1);
        assert!(mailbox.preview_region_request.is_none());
        assert!(mailbox.preview_region_completed.is_some());
    }
    let completion = worker.loader.take_preview_region().unwrap();
    assert_eq!(completion.serial, 1);
    assert_eq!(completion.epoch, 7);
    assert_eq!(completion.region, crop());
    assert!(Arc::ptr_eq(&completion.source, &source));
    let surface = completion.result.unwrap();
    assert_eq!(surface.geometry.normalized_source_region(), crop());
    assert_eq!(surface.geometry.raster_dimensions(), (4, 1));
    assert_eq!(
        surface.image.as_bytes(0).unwrap(),
        &[150, 60, 102, 0, 149, 61, 103, 128, 148, 62, 104, 255, 147, 63, 105, 64,]
    );
    worker.wait_for(|mailbox| !mailbox.thumbnails.is_empty());
    let thumbnails = worker.loader.take_thumbnails();
    assert_eq!(thumbnails.len(), 1);
    assert_eq!(thumbnails[0].serial, 1);
    assert_eq!(thumbnails[0].epoch, 11);
    assert_eq!(thumbnails[0].index, 3);
    assert!(matches!(&thumbnails[0].thumbnail, Thumbnail::Failed));
    // 关闭线程不要求宿主先释放旧总览、来源或已取走的ROI图像。
    worker.finish();
}

#[test]
fn image_viewer_preview_region_worker_new_main_cancels_old_completion_and_closes() {
    let temporary = TempDir::new();
    let preview_path = temporary.path.join("wide.png");
    let full_path = temporary.path.join("full.png");
    std::fs::write(&preview_path, wide_png()).unwrap();
    std::fs::write(
        &full_path,
        static_png((2, 1), &[70, 140, 210, 255, 60, 120, 180, 128]),
    )
    .unwrap();
    let worker = Worker::new(Mailbox::default());
    let image = load_preview(&worker, preview_path);
    let source = image.preview_source.as_ref().unwrap().clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let other = presentation_resident_bytes_for_excluding_preview_source(
        Some(&image),
        std::iter::empty(),
        std::iter::empty(),
        Some(&source),
    )
    .checked_add(THUMBNAIL_CACHE_BYTES as u64)
    .unwrap();
    worker
        .loader
        .request_preview_region(preview_region::Request {
            serial: 1,
            epoch: 9,
            source: source.clone(),
            region: crop(),
            external_other_resident: other,
            cancel: cancel.clone(),
        });
    worker.wait_for(|mailbox| mailbox.preview_region_completed.is_some());
    let (main_waits, main_resident) = {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        let surface = mailbox
            .preview_region_completed
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        let resident = presentation_resident_bytes_for_excluding_preview_source(
            Some(&image),
            std::iter::once(&surface.image),
            std::iter::empty(),
            Some(&source),
        )
        // 原LoadedImage和额外source Arc仍由宿主持有；普通Main不能隐匿这个旧来源。
        .checked_add(source.retained_bytes())
        .unwrap()
        .checked_add(THUMBNAIL_CACHE_BYTES as u64)
        .unwrap();
        (mailbox.main_backpressure_waits, resident)
    };
    worker.loader.request(Request {
        serial: 2,
        path: full_path,
        scan: false,
        page: None,
        raster: None,
        resident_bytes: main_resident,
    });
    assert!(cancel.load(Ordering::Acquire));
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert_eq!(mailbox.serial, 2);
        assert!(mailbox.preview_region_request.is_none());
        assert!(mailbox.preview_region_completed.is_none());
    }
    assert!(worker.loader.take_preview_region().is_none());
    worker.wait_for(|mailbox| {
        mailbox.main_backpressure_waits > main_waits
            && mailbox
                .completed
                .as_ref()
                .is_some_and(|completion| completion.serial == 2)
    });
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert!(mailbox.preview_region_completed.is_none());
        let full = mailbox.completed.as_ref().unwrap().result.as_ref().unwrap();
        assert!(full.has_full_pixels());
        assert!(full.preview_source.is_none());
        assert!(full.preview_geometry.is_none());
        assert_eq!(full.metadata.display_dimensions, (2, 1));
        assert_eq!(
            full.raw_page.as_ref().unwrap().bgra,
            [210, 140, 70, 255, 180, 120, 60, 128]
        );
    }
    // 保留新Main完成槽以检查close可唤醒真实背压门并join，而非依赖take。
    worker.finish();
}
