use super::*;
use std::sync::{atomic::AtomicU64, mpsc};

const TIMEOUT: Duration = Duration::from_secs(2);
static MISSING_PATH_ID: AtomicU64 = AtomicU64::new(0);

fn missing_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "rustshot-viewer-backpressure-{}-{}",
            std::process::id(),
            MISSING_PATH_ID.fetch_add(1, Ordering::Relaxed)
        ))
        .join("not-created.png")
}

fn completion(result: std::result::Result<SnapshotOutput, String>) -> SnapshotCompletion {
    SnapshotCompletion {
        serial: 1,
        path: missing_path(),
        target: SnapshotTarget::Editor,
        metadata_source: None,
        result,
    }
}

fn image() -> crate::capture::Image {
    crate::capture::Image {
        width: 1,
        height: 2,
        bgra: vec![3, 2, 1, 0, 6, 5, 4, 128],
        readback_ms: 0.,
    }
}

fn rotation() -> RotationRequest {
    RotationRequest {
        serial: 1,
        epoch: 1,
        projection: ImageRotation::new((1, 2), Rotation::R90).unwrap(),
        tiles: vec![(
            Rect::full(1, 2),
            crate::canvas_source::render_image(1, 2, image().bgra),
        )],
        resident_bytes: 8,
        cancel: Arc::new(AtomicBool::new(false)),
    }
}

pub(super) struct Worker {
    pub(super) loader: Loader,
    finished: mpsc::Receiver<()>,
}

impl Worker {
    pub(super) fn new(mailbox: Mailbox) -> Self {
        let shared = Arc::new((Mutex::new(mailbox), Condvar::new()));
        let worker = shared.clone();
        let (finished_tx, finished) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            decode_loop(worker);
            let _ = finished_tx.send(());
        });
        Self {
            loader: Loader {
                shared,
                thread: Some(thread),
            },
            finished,
        }
    }

    pub(super) fn wait_for(&self, predicate: impl Fn(&Mailbox) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        let mut mailbox = self.loader.shared.0.lock().unwrap();
        while !predicate(&mailbox) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "worker did not reach the expected state"
            );
            mailbox = self
                .loader
                .shared
                .1
                .wait_timeout(mailbox, remaining.min(Duration::from_millis(20)))
                .unwrap()
                .0;
        }
    }

    pub(super) fn finish(mut self) {
        let thread = self.loader.thread.take().unwrap();
        drop(self.loader);
        self.finished
            .recv_timeout(TIMEOUT)
            .expect("closing must wake a worker blocked by snapshot backpressure");
        thread.join().unwrap();
    }
}

#[test]
fn image_viewer_snapshot_backpressure_defers_rotation_and_thumbnail_until_take() {
    let rotation = rotation();
    let worker = Worker::new(Mailbox {
        serial: 1,
        snapshot_completed: Some(completion(Ok(SnapshotOutput::Image(image())))),
        rotation_cancel: Some(rotation.cancel.clone()),
        rotation_request: Some(rotation),
        thumbnail_epoch: 4,
        thumbnail_request: Some(ThumbnailRequest {
            serial: 1,
            epoch: 4,
            resident_bytes: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            paths: [(9, missing_path())].into(),
        }),
        ..Mailbox::default()
    });
    // 工作线程持锁确认已经进入实际背压等待，不以固定睡眠猜测调度时序。
    worker.wait_for(|mailbox| mailbox.snapshot_backpressure_waits > 0);
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert!(mailbox.rotation_request.is_some());
        assert!(mailbox.rotation_completed.is_none());
        assert_eq!(mailbox.thumbnail_request.as_ref().unwrap().paths.len(), 1);
        assert!(mailbox.thumbnails.is_empty());
    }
    let taken = worker.loader.take_snapshot().unwrap();
    match taken.result.unwrap() {
        SnapshotOutput::Image(taken) => assert_eq!(taken.bgra, image().bgra),
        SnapshotOutput::Clipboard(_) => panic!("unexpected clipboard payload"),
    }
    // 消费整图后旧账本失效；模拟宿主应用结果后按新驻留量重授予。
    worker.loader.request_thumbnails(ThumbnailRequest {
        serial: 1,
        epoch: 5,
        resident_bytes: 32 * 1024 * 1024,
        cancel: Arc::new(AtomicBool::new(false)),
        paths: [(9, missing_path())].into(),
    });
    worker.wait_for(|mailbox| mailbox.rotation_completed.is_some());
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert!(mailbox.thumbnails.is_empty());
        assert_eq!(mailbox.thumbnail_request.as_ref().unwrap().paths.len(), 1);
    }
    let rotated = worker.loader.take_rotation().unwrap().result.unwrap();
    assert_eq!(rotated[0].0, Rect::full(2, 1));
    assert_eq!(
        rotated[0].1.as_bytes(0).unwrap(),
        [6, 5, 4, 128, 3, 2, 1, 0]
    );
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
        epoch: 6,
        resident_bytes: 32 * 1024 * 1024,
        cancel: Arc::new(AtomicBool::new(false)),
        paths: [(9, missing_path())].into(),
    });
    worker.wait_for(|mailbox| mailbox.thumbnails.len() == 1);
    assert!(matches!(
        &worker.loader.take_thumbnails()[0].thumbnail,
        Thumbnail::Failed
    ));
    worker.finish();
}

#[test]
fn image_viewer_snapshot_backpressure_preserves_result_and_defers_next_snapshot() {
    let worker = Worker::new(Mailbox {
        serial: 1,
        snapshot_completed: Some(completion(Err("first snapshot result".into()))),
        ..Mailbox::default()
    });
    worker.wait_for(|mailbox| mailbox.snapshot_backpressure_waits > 0);
    worker.loader.snapshot(SnapshotRequest {
        serial: 1,
        path: missing_path(),
        frame: None,
        raw_page: Some(Arc::new(image())),
        metadata_source: None,
        resident_bytes: 8,
        target: SnapshotTarget::Editor,
    });
    worker.wait_for(|mailbox| mailbox.snapshot_backpressure_waits > 1);
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert!(mailbox.snapshot_request.is_some());
        assert!(matches!(
            &mailbox.snapshot_completed.as_ref().unwrap().result,
            Err(error) if error == "first snapshot result"
        ));
    }
    assert!(matches!(
        worker.loader.take_snapshot().unwrap().result,
        Err(error) if error == "first snapshot result"
    ));
    worker.wait_for(|mailbox| {
        mailbox.snapshot_request.is_none() && mailbox.snapshot_completed.is_some()
    });
    match worker.loader.take_snapshot().unwrap().result.unwrap() {
        SnapshotOutput::Image(taken) => assert_eq!(taken.bgra, image().bgra),
        SnapshotOutput::Clipboard(_) => panic!("unexpected clipboard payload"),
    }
    worker.finish();
}

#[test]
fn image_viewer_snapshot_backpressure_allows_cancel_and_new_main_request() {
    let rotation = rotation();
    let rotation_cancel = rotation.cancel.clone();
    let worker = Worker::new(Mailbox {
        serial: 1,
        snapshot_completed: Some(completion(Ok(SnapshotOutput::Image(image())))),
        rotation_cancel: Some(rotation_cancel.clone()),
        rotation_request: Some(rotation),
        ..Mailbox::default()
    });
    worker.wait_for(|mailbox| mailbox.snapshot_backpressure_waits > 0);
    worker.loader.cancel(2);
    {
        let mailbox = worker.loader.shared.0.lock().unwrap();
        assert_eq!(mailbox.serial, 2);
        assert!(mailbox.snapshot_completed.is_none());
        assert!(mailbox.rotation_request.is_none());
        assert!(rotation_cancel.load(Ordering::Relaxed));
    }
    worker.loader.request(Request {
        serial: 3,
        path: missing_path(),
        scan: false,
        page: None,
        raster: None,
        resident_bytes: 0,
    });
    worker.wait_for(|mailbox| {
        mailbox
            .completed
            .as_ref()
            .is_some_and(|done| done.serial == 3)
    });
    assert!(worker.loader.take().unwrap().result.is_err());
    worker.finish();
}

#[test]
fn image_viewer_snapshot_backpressure_close_wakes_waiting_worker() {
    let worker = Worker::new(Mailbox {
        serial: 1,
        snapshot_completed: Some(completion(Ok(SnapshotOutput::Image(image())))),
        rotation_request: Some(rotation()),
        ..Mailbox::default()
    });
    worker.wait_for(|mailbox| mailbox.snapshot_backpressure_waits > 0);
    worker.finish();
}
