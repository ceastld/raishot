use super::snapshot_backpressure_tests::Worker;
use super::snapshot_tests::{static_png, TempDir};
use super::*;

fn grant(path: PathBuf) -> prefetch::Grant {
    prefetch::Grant {
        serial: 1,
        epoch: 1,
        path,
        resident_bytes: THUMBNAIL_CACHE_BYTES as u64,
        max_entry_bytes: prefetch::MAX_ENTRY_BYTES,
        cancel: Arc::new(AtomicBool::new(false)),
    }
}

fn worker(path: PathBuf, snapshot: bool) -> Worker {
    let grant = grant(path.clone());
    Worker::new(Mailbox {
        serial: 1,
        prefetch_epoch: 1,
        prefetch_cancel: Some(grant.cancel.clone()),
        prefetch_request: Some(grant),
        snapshot_completed: snapshot.then(|| SnapshotCompletion {
            serial: 1,
            path,
            target: SnapshotTarget::Editor,
            metadata_source: None,
            result: Err("held snapshot".into()),
        }),
        ..Mailbox::default()
    })
}

fn main_request(worker: &Worker, path: PathBuf) -> LoadedImage {
    worker.loader.request(Request {
        serial: 2,
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
            .is_some_and(|done| done.serial == 2)
    });
    worker.loader.take().unwrap().result.unwrap()
}

#[test]
fn image_viewer_prefetch_worker_moves_frozen_raw_to_main_after_request_cancels_grant() {
    let temporary = TempDir::new();
    let path = temporary.path.join("neighbor.png");
    std::fs::write(&path, static_png((1, 1), &[23, 61, 107, 80])).unwrap();
    let worker = worker(path.clone(), false);
    worker.wait_for(|mailbox| mailbox.prefetch_commits == 1);
    let loaded = main_request(&worker, path);
    assert_eq!(loaded.raw_page.as_ref().unwrap().bgra, [107, 61, 23, 80]);
    assert_eq!(worker.loader.shared.0.lock().unwrap().prefetch_hits, 1);
    assert!(loaded.metadata_source.is_some());
    worker.finish();
}

#[test]
fn image_viewer_prefetch_worker_replaced_source_is_decoded_again_instead_of_cache_hit() {
    let temporary = TempDir::new();
    let path = temporary.path.join("neighbor.png");
    std::fs::write(&path, static_png((1, 1), &[23, 61, 107, 80])).unwrap();
    let worker = worker(path.clone(), false);
    worker.wait_for(|mailbox| mailbox.prefetch_commits == 1);
    std::fs::write(&path, static_png((2, 1), &[9, 17, 31, 255].repeat(2))).unwrap();
    let loaded = main_request(&worker, path);
    let raw = loaded.raw_page.as_ref().unwrap();
    assert_eq!((raw.width, raw.height), (2, 1));
    assert_eq!(raw.bgra, [31, 17, 9, 255].repeat(2));
    assert_eq!(worker.loader.shared.0.lock().unwrap().prefetch_hits, 0);
    worker.finish();
}

#[test]
fn image_viewer_prefetch_worker_waits_for_snapshot_payload_to_be_taken() {
    let temporary = TempDir::new();
    let path = temporary.path.join("neighbor.png");
    std::fs::write(&path, static_png((1, 1), &[23, 61, 107, 80])).unwrap();
    let worker = worker(path, true);
    worker.wait_for(|mailbox| mailbox.snapshot_backpressure_waits > 0);
    assert_eq!(worker.loader.shared.0.lock().unwrap().prefetch_commits, 0);
    assert!(worker.loader.take_snapshot().unwrap().result.is_err());
    worker.wait_for(|mailbox| mailbox.prefetch_commits == 1);
    worker.finish();
}
