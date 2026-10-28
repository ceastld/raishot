use super::snapshot_backpressure_tests::Worker;
use super::snapshot_tests::{static_png, TempDir};
use super::*;

fn request(epoch: u64, path: &Path, resident_bytes: u64) -> ThumbnailRequest {
    ThumbnailRequest {
        serial: 1,
        epoch,
        resident_bytes,
        cancel: Arc::new(AtomicBool::new(false)),
        paths: [(0, path.to_path_buf())].into(),
    }
}

#[test]
fn image_viewer_thumbnail_shared_budget_failure_retries_same_source_after_new_grant() {
    let temporary = TempDir::new();
    let path = temporary.path.join("thumbnail.png");
    std::fs::write(&path, static_png((1, 1), &[17, 31, 53, 255])).unwrap();
    let worker = Worker::new(Mailbox {
        serial: 1,
        ..Mailbox::default()
    });
    worker
        .loader
        .request_thumbnails(request(1, &path, crate::image_decode::MAX_DECODE_BYTES));
    worker.wait_for(|mailbox| mailbox.thumbnails.len() == 1);
    assert!(matches!(
        worker.loader.take_thumbnails()[0].thumbnail,
        Thumbnail::Failed
    ));
    worker
        .loader
        .request_thumbnails(request(2, &path, 32 * 1024 * 1024));
    worker.wait_for(|mailbox| mailbox.thumbnails.len() == 1);
    let completed = worker.loader.take_thumbnails();
    assert!(matches!(&completed[0].thumbnail, Thumbnail::Ready(_)));
    assert_eq!(completed[0].epoch, 2);
    worker.finish();
}

#[test]
fn image_viewer_thumbnail_shared_main_budget_failure_does_not_poison_thumbnail_cache() {
    let temporary = TempDir::new();
    let path = temporary.path.join("thumbnail.png");
    std::fs::write(&path, static_png((1, 1), &[17, 31, 53, 255])).unwrap();
    let worker = Worker::new(Mailbox::default());
    worker.loader.request(Request {
        serial: 1,
        path: path.clone(),
        scan: false,
        page: None,
        raster: None,
        resident_bytes: crate::image_decode::MAX_DECODE_BYTES,
    });
    worker.wait_for(|mailbox| mailbox.completed.is_some());
    assert!(worker.loader.take().unwrap().result.is_err());
    worker
        .loader
        .request_thumbnails(request(1, &path, 32 * 1024 * 1024));
    worker.wait_for(|mailbox| mailbox.thumbnails.len() == 1);
    assert!(matches!(
        &worker.loader.take_thumbnails()[0].thumbnail,
        Thumbnail::Ready(_)
    ));
    worker.finish();
}

#[test]
fn image_viewer_thumbnail_shared_grant_revokes_without_cancelling_foreground() {
    let loader = Loader {
        shared: Arc::new((
            Mutex::new(Mailbox {
                serial: 1,
                ..Mailbox::default()
            }),
            Condvar::new(),
        )),
        thread: None,
    };
    let first = request(1, Path::new("unused.png"), 1);
    let first_cancel = first.cancel.clone();
    loader.request_thumbnails(first);
    let second = request(2, Path::new("unused.png"), 2);
    let second_cancel = second.cancel.clone();
    loader.request_thumbnails(second);
    assert!(first_cancel.load(Ordering::Relaxed));
    assert!(!second_cancel.load(Ordering::Relaxed));
    assert!(!loader
        .shared
        .0
        .lock()
        .unwrap()
        .generation_cancel
        .load(Ordering::Relaxed));
    loader.revoke_thumbnails();
    assert!(second_cancel.load(Ordering::Relaxed));
    let mailbox = loader.shared.0.lock().unwrap();
    assert!(mailbox.thumbnail_request.is_none());
    assert!(mailbox.thumbnails.is_empty());
    assert!(!mailbox.generation_cancel.load(Ordering::Relaxed));
}

#[test]
fn image_viewer_thumbnail_shared_animation_reply_blocks_and_replaces_old_grant() {
    let temporary = TempDir::new();
    let path = temporary.path.join("thumbnail.png");
    let bytes = static_png((1, 1), &[17, 31, 53, 255]);
    std::fs::write(&path, &bytes).unwrap();
    let image = pages::loaded(
        crate::image_decode::decode_bytes(&bytes).unwrap(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let grant = request(1, &path, 1);
    let cancel = grant.cancel.clone();
    let worker = Worker::new(Mailbox {
        serial: 1,
        thumbnail_epoch: 1,
        thumbnail_cancel: Some(cancel.clone()),
        thumbnail_request: Some(grant),
        animation_update: Some(AnimationUpdate {
            serial: 1,
            epoch: 0,
            tick: animation::Tick {
                image: Some(image),
                snapshot: crate::image_animation_playback::PlaybackSnapshot {
                    state: PlaybackState::Paused,
                    generation: 1,
                    cycle: Some(0),
                    frame_index: Some(0),
                    frame_elapsed: Duration::ZERO,
                    waiting_for_outputs: false,
                    omitted_presentations: 0,
                    error: None,
                },
                error: None,
            },
        }),
        ..Mailbox::default()
    });
    worker.wait_for(|mailbox| mailbox.thumbnail_backpressure_waits > 0);
    assert!(worker.loader.shared.0.lock().unwrap().thumbnails.is_empty());
    let update = worker.loader.take_animation().unwrap();
    assert!(update.tick.image.is_some());
    assert!(cancel.load(Ordering::Relaxed));
    assert!(worker
        .loader
        .shared
        .0
        .lock()
        .unwrap()
        .thumbnail_request
        .is_none());
    worker
        .loader
        .request_thumbnails(request(2, &path, 32 * 1024 * 1024));
    worker.wait_for(|mailbox| mailbox.thumbnails.len() == 1);
    assert!(matches!(
        &worker.loader.take_thumbnails()[0].thumbnail,
        Thumbnail::Ready(_)
    ));
    worker.finish();
}
