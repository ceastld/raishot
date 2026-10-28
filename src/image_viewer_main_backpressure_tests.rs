use super::snapshot_backpressure_tests::Worker;
use super::snapshot_tests::{static_png, TempDir};
use super::*;

fn completed_image() -> Completion {
    let bytes = static_png((1, 2), &[23, 61, 107, 80, 9, 17, 31, 255]);
    let decoded = crate::image_decode::decode_bytes(&bytes).unwrap();
    let image = pages::loaded(decoded, &AtomicBool::new(false)).unwrap();
    Completion {
        serial: 1,
        collection: None,
        result: Ok(image),
        animation: None,
        pages: None,
        source_kind: None,
        warning: None,
        source_file: None,
    }
}

#[test]
fn image_viewer_main_backpressure_preserves_raw_and_defers_thumbnail_until_take() {
    let temporary = TempDir::new();
    let missing = temporary.path.join("missing-thumbnail.png");
    let worker = Worker::new(Mailbox {
        serial: 1,
        completed: Some(completed_image()),
        thumbnail_epoch: 2,
        thumbnail_request: Some(ThumbnailRequest {
            serial: 1,
            epoch: 2,
            resident_bytes: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            paths: [(0, missing.clone())].into(),
        }),
        ..Mailbox::default()
    });
    worker.wait_for(|mailbox| mailbox.main_backpressure_waits > 0);
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert!(mailbox.thumbnails.is_empty());
        assert_eq!(mailbox.thumbnail_request.as_ref().unwrap().paths.len(), 1);
        let image = mailbox.completed.as_ref().unwrap().result.as_ref().unwrap();
        assert_eq!(
            image.raw_page.as_ref().unwrap().bgra,
            [107, 61, 23, 80, 31, 17, 9, 255]
        );
    }
    let completion = worker.loader.take().unwrap();
    assert_eq!(completion.serial, 1);
    let image = completion.result.unwrap();
    let raw = image.raw_page.as_ref().unwrap();
    assert_eq!((raw.width, raw.height), (1, 2));
    assert_eq!(raw.bgra, [107, 61, 23, 80, 31, 17, 9, 255]);
    assert!(worker
        .loader
        .shared
        .0
        .lock()
        .unwrap()
        .thumbnail_request
        .is_none());
    worker.loader.request_thumbnails(ThumbnailRequest {
        serial: 1,
        epoch: 3,
        resident_bytes: 32 * 1024 * 1024,
        cancel: Arc::new(AtomicBool::new(false)),
        paths: [(0, missing)].into(),
    });
    worker.wait_for(|mailbox| !mailbox.thumbnails.is_empty());
    let thumbnails = worker.loader.take_thumbnails();
    assert_eq!(thumbnails.len(), 1);
    assert_eq!(thumbnails[0].serial, 1);
    assert_eq!(thumbnails[0].epoch, 3);
    assert_eq!(thumbnails[0].index, 0);
    assert!(matches!(&thumbnails[0].thumbnail, Thumbnail::Failed));
    worker.finish();
}

#[test]
fn image_viewer_main_backpressure_allows_cancel_new_main_and_close() {
    let temporary = TempDir::new();
    let path = temporary.path.join("next.png");
    std::fs::write(&path, static_png((2, 1), &[9, 17, 31, 255].repeat(2))).unwrap();
    let worker = Worker::new(Mailbox {
        serial: 1,
        completed: Some(completed_image()),
        ..Mailbox::default()
    });
    worker.wait_for(|mailbox| mailbox.main_backpressure_waits > 0);
    worker.loader.cancel(2);
    let previous_waits = {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert_eq!(mailbox.serial, 2);
        assert!(mailbox.pending.is_none());
        assert!(mailbox.completed.is_none());
        mailbox.main_backpressure_waits
    };
    worker.loader.request(Request {
        serial: 3,
        path,
        scan: false,
        page: None,
        raster: None,
        resident_bytes: THUMBNAIL_CACHE_BYTES as u64,
    });
    worker.wait_for(|mailbox| {
        mailbox.main_backpressure_waits > previous_waits
            && mailbox
                .completed
                .as_ref()
                .is_some_and(|completion| completion.serial == 3)
    });
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        let image = mailbox.completed.as_ref().unwrap().result.as_ref().unwrap();
        let raw = image.raw_page.as_ref().unwrap();
        assert_eq!((raw.width, raw.height), (2, 1));
        assert_eq!(raw.bgra, [31, 17, 9, 255].repeat(2));
    }
    // 留下新的真实完成像素；关闭必须唤醒背压门，而不是等待消费者先take。
    worker.finish();
}
