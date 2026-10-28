use super::*;
use crate::{history, recording_library_model::Entry};
use std::{
    cell::Cell,
    fs::{self, FileTimes},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustshot-library-catalog-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        Self(directory)
    }
    fn open(&self) -> Catalog {
        Catalog::open_in(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
        assert!(self
            .0
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("rustshot-library-catalog-"));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn shot(key: &str, created: SystemTime) -> Item {
    Item {
        key: key.into(),
        created,
        title: String::new(),
        size: Some((80, 60)),
        bytes: Some(17),
        source: Source::Shot(history::Entry {
            path: PathBuf::from(format!("{key}.png")),
            bytes: 17,
            modified: created,
            dimensions: Some((80, 60)),
        }),
    }
}

fn recording(path: &Path, created: SystemTime) -> Item {
    Item {
        key: format!("rec:{}", path.display()),
        created,
        title: String::new(),
        size: Some((640, 360)),
        bytes: Some(15),
        source: Source::Recording(Entry {
            path: path.into(),
            name: "recording.mp4".into(),
            bytes: 15,
            modified: Some(created),
            info: VideoInfo {
                width: Some(640),
                height: Some(360),
                duration: Some(Duration::new(5, 123_456_789)),
            },
        }),
    }
}

#[test]
fn library_catalog_persists_order_and_preserves_user_title() {
    let fixture = Fixture::new();
    let before_epoch = UNIX_EPOCH - Duration::from_millis(1234);
    let newer = UNIX_EPOCH + Duration::new(4, 789);
    let mut catalog = fixture.open();
    let items = vec![shot("older", before_epoch), shot("newer", newer)];
    let indexed = catalog.reconcile(&items, [true; 3], &|| false).unwrap();
    assert_eq!(
        indexed
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        ["newer", "older"]
    );
    assert_eq!(indexed[1].created, before_epoch);
    catalog
        .connection
        .execute(
            "UPDATE catalog_items SET title = '自定标题' WHERE item_key = 'newer'",
            [],
        )
        .unwrap();
    drop(catalog);

    let mut reopened = fixture.open();
    let mut changed = items;
    changed[1].title = "来源的新标题".into();
    let indexed = reopened.reconcile(&changed, [true; 3], &|| false).unwrap();
    assert_eq!(indexed[0].title, "自定标题");
    assert_eq!(indexed[0].created, newer);
    assert_eq!(indexed[0].size, Some((80, 60)));
    let fields: (String, u64, i64, u32) = reopened.connection.query_row(
        "SELECT kind, source_bytes, modified_secs, modified_nanos FROM catalog_items WHERE item_key = 'newer'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    ).unwrap();
    // Windows SystemTime 使用100ns刻度，按真正写入索引的时间验证持久化。
    let modified = newer.duration_since(UNIX_EPOCH).unwrap();
    assert_eq!(fields, ("shot".into(), 17, 4, modified.subsec_nanos()));
}

#[test]
fn library_catalog_reconcile_clears_removed_sources_but_keeps_failed_source_index() {
    let fixture = Fixture::new();
    let mut catalog = fixture.open();
    let items = vec![
        shot("shot-a", UNIX_EPOCH),
        recording(&fixture.0.join("recording.mp4"), UNIX_EPOCH),
    ];
    catalog.reconcile(&items, [true; 3], &|| false).unwrap();
    let duration: (u64, u32) = catalog
        .connection
        .query_row(
            "SELECT duration_secs, duration_nanos FROM catalog_items WHERE kind = 'recording'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(duration, (5, 123_456_789));
    assert!(catalog
        .reconcile(&[], [false; 3], &|| false)
        .unwrap()
        .is_empty());
    let count: u64 = catalog
        .connection
        .query_row("SELECT COUNT(*) FROM catalog_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let indexed = catalog
        .reconcile(&items[..1], [true; 3], &|| false)
        .unwrap();
    assert_eq!(indexed.len(), 1);
    let count: u64 = catalog
        .connection
        .query_row("SELECT COUNT(*) FROM catalog_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert!(catalog
        .reconcile(&[], [true; 3], &|| false)
        .unwrap()
        .is_empty());
}

#[test]
fn library_catalog_unseen_recordings_keep_existing_or_uncertain_files_without_reviving_ui() {
    let fixture = Fixture::new();
    let path = fixture.0.join("previous-brand.mp4");
    fs::write(&path, b"owned recording").unwrap();
    let uncertain = fixture.0.join("not-a-regular-file.mp4");
    fs::create_dir(&uncertain).unwrap();
    let mut catalog = fixture.open();
    catalog
        .probe_recording_with(&path, &|| false, |_, _| {
            Ok(Probe::Complete(VideoInfo::default()))
        })
        .unwrap();
    let items = [
        recording(&path, UNIX_EPOCH),
        recording(&uncertain, UNIX_EPOCH),
    ];
    catalog.reconcile(&items, [true; 3], &|| false).unwrap();

    // 索引仍保留，但没有本轮验证的Source时不能成为可打开的界面条目。
    assert!(catalog
        .reconcile(&[], [true; 3], &|| false)
        .unwrap()
        .is_empty());
    let count: u64 = catalog
        .connection
        .query_row("SELECT COUNT(*) FROM catalog_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let cached: u64 = catalog
        .connection
        .query_row(
            "SELECT COUNT(*) FROM catalog_recording_metadata",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cached, 1);

    fs::remove_file(&path).unwrap();
    assert!(catalog
        .reconcile(&[], [true; 3], &|| false)
        .unwrap()
        .is_empty());
    let remaining: String = catalog
        .connection
        .query_row("SELECT source_path FROM catalog_items", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(Path::new(&remaining), uncertain);
    let cached: u64 = catalog
        .connection
        .query_row(
            "SELECT COUNT(*) FROM catalog_recording_metadata",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cached, 0);
}

#[test]
fn library_catalog_recording_cache_reuses_metadata_and_rejects_replaced_file() {
    let fixture = Fixture::new();
    let path = fixture.0.join("缓存 # sample.mp4");
    fs::write(&path, b"first owned fixture").unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let original = fingerprint(&path).unwrap();
    let calls = Cell::new(0);
    let info = VideoInfo {
        width: Some(640),
        height: Some(360),
        duration: Some(Duration::new(4, 987_654_321)),
    };
    let catalog = fixture.open();
    assert_eq!(
        catalog
            .probe_recording_with(&path, &|| false, |_, _| {
                calls.set(calls.get() + 1);
                Ok(Probe::Complete(info))
            })
            .unwrap(),
        Probe::Complete(info)
    );
    drop(catalog);
    let catalog = fixture.open();
    assert_eq!(
        catalog
            .probe_recording_with(&path, &|| false, |_, _| {
                panic!("unchanged file was probed again")
            })
            .unwrap(),
        Probe::Complete(info)
    );

    let replacement = fixture.0.join("replacement.mp4");
    fs::write(&replacement, b"other owned fixture").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    assert_ne!(fingerprint(&path).unwrap(), original);
    let new_info = VideoInfo {
        width: Some(800),
        ..info
    };
    assert_eq!(
        catalog
            .probe_recording_with(&path, &|| false, |_, _| {
                calls.set(calls.get() + 1);
                Ok(Probe::Complete(new_info))
            })
            .unwrap(),
        Probe::Complete(new_info)
    );
    assert_eq!(calls.get(), 2);
}

#[test]
fn library_catalog_cancel_and_changed_during_probe_never_publish_cache() {
    let fixture = Fixture::new();
    let path = fixture.0.join("owned.mp4");
    fs::write(&path, b"owned").unwrap();
    let mut catalog = fixture.open();
    assert_eq!(
        catalog
            .probe_recording_with(&path, &|| true, |_, _| panic!("cancelled probe started"))
            .unwrap(),
        Probe::Cancelled
    );
    let changed = catalog
        .probe_recording_with(&path, &|| false, |path, _| {
            fs::write(path, b"owned file changed while probing")?;
            Ok(Probe::Complete(VideoInfo::default()))
        })
        .unwrap();
    assert_eq!(changed, Probe::Unavailable);
    let count: u64 = catalog
        .connection
        .query_row(
            "SELECT COUNT(*) FROM catalog_recording_metadata",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert!(catalog
        .reconcile(&[shot("shot-a", UNIX_EPOCH)], [true; 3], &|| true)
        .is_err());
    let count: u64 = catalog
        .connection
        .query_row("SELECT COUNT(*) FROM catalog_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}
