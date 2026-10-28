use super::*;

thread_local! {
    static CANDIDATES: std::cell::RefCell<Option<(PathBuf, Vec<PathBuf>)>> = const {
        std::cell::RefCell::new(None)
    };
}

pub(super) fn candidates_for(dir: &Path) -> Option<Vec<PathBuf>> {
    CANDIDATES.with(|value| {
        value
            .borrow()
            .as_ref()
            .and_then(|(primary, candidates)| same_path(primary, dir).then(|| candidates.clone()))
    })
}

struct CandidateGuard;
impl CandidateGuard {
    fn new(primary: &Path, legacy: PathBuf) -> Self {
        CANDIDATES.with(|value| {
            assert!(value.borrow().is_none());
            *value.borrow_mut() = Some((primary.into(), vec![legacy]));
        });
        Self
    }
}
impl Drop for CandidateGuard {
    fn drop(&mut self) {
        CANDIDATES.with(|value| *value.borrow_mut() = None);
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(new_id("rustshot-pin-relocation"));
        std::fs::create_dir(&base).unwrap();
        Self(base)
    }
    fn primary(&self) -> PathBuf {
        self.0.join(crate::brand::DATA_DIR).join("pin-workspace")
    }
    fn legacy(&self) -> PathBuf {
        self.0.join("previous-brand").join("pin-workspace")
    }
    fn database(&self) -> PinEntry {
        let pin = entry();
        let workspace = Workspace {
            pins: vec![pin.clone()],
            ..Workspace::default()
        };
        write_manifest(&self.primary(), &encoded_manifest(&workspace).unwrap()).unwrap();
        pin
    }
    fn run(&self) -> Result<()> {
        let primary = self.primary();
        let _lock = global_lock_with_layout(&primary, false)?;
        reconcile_with(&primary, &[self.legacy()], crate::brand::DATA_DIR)
    }
    fn old_images(&self, pin: &PinEntry) {
        for name in [&pin.image_file, &pin.original_file].into_iter().flatten() {
            image(&asset_path_in(&self.legacy(), name).unwrap(), 41);
        }
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
            .starts_with("rustshot-pin-relocation-"));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn entry() -> PinEntry {
    PinEntry {
        id: new_id("pin"),
        group_id: DEFAULT_GROUP_ID.into(),
        title: String::new(),
        status: PinStatus::Closed,
        hidden: false,
        click_through: false,
        placement: Placement {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        },
        window_sizing: PinWindowSizing::General,
        state: PinState::default(),
        image_file: Some("20261023-170001.png".into()),
        text_document: None,
        presentation_style: None,
        original_file: Some("20261023-170001.original.png".into()),
        raster_document: Some(WireRasterDocument::empty((2, 2)).unwrap()),
        processing_history: None,
        updated_ms: now_ms(),
    }
}

fn image(path: &Path, value: u8) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    export::save_png_pixels(2, 2, &[value; 16], path).unwrap();
}

#[test]
fn pin_workspace_relocation_copies_registered_assets_and_keeps_legacy_sources() {
    let fixture = Fixture::new();
    let pin = fixture.database();
    fixture.old_images(&pin);
    fixture.run().unwrap();
    for name in [&pin.image_file, &pin.original_file].into_iter().flatten() {
        let source = asset_path_in(&fixture.legacy(), name).unwrap();
        let target = asset_path_in(&fixture.primary(), name).unwrap();
        assert_eq!(
            std::fs::read(source).unwrap(),
            std::fs::read(target).unwrap()
        );
    }
    assert_eq!(db::load_summary(&fixture.primary()).unwrap().pins.len(), 1);
    assert!(
        db::relocation_summary(&fixture.primary(), crate::brand::DATA_DIR)
            .unwrap()
            .is_none()
    );
}

#[test]
fn pin_workspace_relocation_does_not_overwrite_existing_primary_assets() {
    let fixture = Fixture::new();
    let pin = fixture.database();
    fixture.old_images(&pin);
    let target = asset_path_in(&fixture.primary(), pin.image_file.as_deref().unwrap()).unwrap();
    image(&target, 99);
    let before = std::fs::read(&target).unwrap();
    fixture.run().unwrap();
    assert_eq!(std::fs::read(target).unwrap(), before);
    assert!(
        asset_path_in(&fixture.primary(), pin.original_file.as_deref().unwrap())
            .unwrap()
            .is_file()
    );
}

#[test]
fn pin_workspace_relocation_completed_migration_does_not_restore_later_deleted_images() {
    let fixture = Fixture::new();
    let pin = fixture.database();
    fixture.old_images(&pin);
    fixture.run().unwrap();
    for name in [&pin.image_file, &pin.original_file].into_iter().flatten() {
        std::fs::remove_file(asset_path_in(&fixture.primary(), name).unwrap()).unwrap();
    }
    fixture.run().unwrap();
    assert!(std::fs::read_dir(fixture.primary().join(PINS_DIR))
        .unwrap()
        .next()
        .is_none());
    assert_eq!(db::load_summary(&fixture.primary()).unwrap().pins.len(), 1);
}

#[test]
fn pin_workspace_relocation_read_failure_preserves_record_and_does_not_mark_complete() {
    let fixture = Fixture::new();
    let pin = fixture.database();
    fixture.old_images(&pin);
    let source = asset_path_in(&fixture.legacy(), pin.image_file.as_deref().unwrap()).unwrap();
    let unavailable = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&source)
        .unwrap();
    assert!(fixture.run().is_err());
    assert!(
        db::relocation_summary(&fixture.primary(), crate::brand::DATA_DIR)
            .unwrap()
            .is_some()
    );
    assert_eq!(db::load_summary(&fixture.primary()).unwrap().pins.len(), 1);
    assert!(!fixture.primary().join(PINS_DIR).exists());
    drop(unavailable);
    fixture.run().unwrap();
}

#[test]
fn pin_workspace_relocation_failure_defers_reclaim_but_allows_unrelated_writes() {
    let fixture = Fixture::new();
    let primary = fixture.primary();
    let mut old = fixture.database();
    old.updated_ms = 0;
    write_manifest(
        &primary,
        &encoded_manifest(&Workspace {
            pins: vec![old.clone()],
            ..Workspace::default()
        })
        .unwrap(),
    )
    .unwrap();
    fixture.old_images(&old);
    let _candidates = CandidateGuard::new(&primary, fixture.legacy());
    let source = asset_path_in(&fixture.legacy(), old.image_file.as_deref().unwrap()).unwrap();
    let _unavailable = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(source)
        .unwrap();
    let orphan = primary.join(PINS_DIR).join("20261023-170002.png");
    image(&orphan, 35);
    assert!(fixture.run().is_err());

    let state = PinState::default();
    let placement = old.placement;
    let session = Session::register_in(&primary, state.clone(), placement).unwrap();
    save_snapshot_with_policy_in(
        &primary,
        &session.id,
        (2, 2),
        (&state, placement, None),
        PinStatus::Active,
        None,
        |path| export::save_png_pixels(2, 2, &[75; 16], path),
    )
    .unwrap();
    session.persist_interaction(true, true).unwrap();
    let moved = Placement { x: 5, ..placement };
    update_state_in(&primary, &session.id, &state, moved).unwrap();
    let summary = db::load_summary(&primary).unwrap();
    assert_eq!(summary.pins.len(), 2);
    assert!(summary.pins.iter().any(|pin| pin.id == old.id));
    assert!(orphan.exists());
    assert!(db::relocation_summary(&primary, crate::brand::DATA_DIR)
        .unwrap()
        .is_some());

    let new_asset = summary
        .pins
        .iter()
        .find(|pin| pin.id == session.id)
        .unwrap()
        .image_file
        .as_deref()
        .unwrap();
    let new_path = asset_path_in(&primary, new_asset).unwrap();
    session.forget().unwrap();
    assert!(!new_path.exists());
    assert!(orphan.exists());
    assert!(db::load_summary(&primary)
        .unwrap()
        .pins
        .iter()
        .any(|pin| pin.id == old.id));

    // 用户明确删除不依赖旧来源可读；仍不擅自修剪其它记录。
    forget_in(&primary, &old.id, false).unwrap();
    assert!(db::load_summary(&primary).unwrap().pins.is_empty());
}

#[test]
fn pin_workspace_relocation_conflicting_previous_sources_stop_before_publication() {
    let fixture = Fixture::new();
    let pin = fixture.database();
    fixture.old_images(&pin);
    let conflicting = fixture.0.join("another-previous").join("pin-workspace");
    image(
        &asset_path_in(&conflicting, pin.image_file.as_deref().unwrap()).unwrap(),
        89,
    );
    let primary = fixture.primary();
    let _lock = global_lock_with_layout(&primary, false).unwrap();
    assert!(reconcile_with(
        &primary,
        &[fixture.legacy(), conflicting],
        crate::brand::DATA_DIR
    )
    .is_err());
    assert!(!primary.join(PINS_DIR).exists());
    assert!(db::relocation_summary(&primary, crate::brand::DATA_DIR)
        .unwrap()
        .is_some());
}

#[test]
fn pin_workspace_relocation_empty_or_uninitialized_storage_is_not_created_or_imported() {
    let fixture = Fixture::new();
    let primary = fixture.primary();
    reconcile_with(&primary, &[fixture.legacy()], crate::brand::DATA_DIR).unwrap();
    assert!(!primary.exists());
    let connection = crate::storage_db::open_workspace(&primary).unwrap();
    reconcile_with(&primary, &[fixture.legacy()], crate::brand::DATA_DIR).unwrap();
    let pin_schema: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='pin_meta')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!pin_schema);
    assert!(!primary.join(PINS_DIR).exists());
    assert!(!runtime_dir(&primary).exists());
    assert!(candidates_in(&primary, &fixture.0, true).is_none());
    assert!(candidates_in(
        &fixture.0.join("custom").join("pin-workspace"),
        &fixture.0,
        false
    )
    .is_none());
}

#[test]
fn pin_workspace_prepare_imports_existing_nonempty_legacy_without_opening_management() {
    let fixture = Fixture::new();
    let primary = fixture.primary();
    std::fs::create_dir_all(&primary).unwrap();
    let workspace = Workspace {
        pins: vec![entry()],
        ..Workspace::default()
    };
    let legacy = encoded_manifest(&workspace).unwrap();
    std::fs::write(manifest(&primary), &legacy).unwrap();
    assert_eq!(prepare(&primary).unwrap(), 1);
    assert!(crate::storage_db::path_in(&primary).is_file());
    assert_eq!(std::fs::read(manifest(&primary)).unwrap(), legacy);
    assert_eq!(db::load_summary(&primary).unwrap().pins.len(), 1);
}

#[test]
fn pin_workspace_prepare_empty_default_or_damaged_legacy_does_not_create_database() {
    let fixture = Fixture::new();
    let primary = fixture.primary();
    assert_eq!(prepare(&primary).unwrap(), 0);
    assert!(!primary.exists());
    std::fs::create_dir_all(&primary).unwrap();
    let empty = encoded_manifest(&Workspace::default()).unwrap();
    std::fs::write(manifest(&primary), &empty).unwrap();
    assert_eq!(prepare(&primary).unwrap(), 0);
    assert!(!crate::storage_db::path_in(&primary).exists());
    assert!(!runtime_dir(&primary).exists());
    std::fs::write(manifest(&primary), b"damaged legacy json").unwrap();
    assert!(prepare(&primary).is_err());
    assert!(!crate::storage_db::path_in(&primary).exists());
    assert_eq!(
        std::fs::read(manifest(&primary)).unwrap(),
        b"damaged legacy json"
    );
}

#[test]
fn pin_workspace_prepare_custom_directory_imports_own_json_without_other_brand_assets() {
    let fixture = Fixture::new();
    let custom = fixture.0.join("custom").join("pin-workspace");
    let pin = entry();
    fixture.old_images(&pin);
    std::fs::create_dir_all(&custom).unwrap();
    let workspace = Workspace {
        pins: vec![pin],
        ..Workspace::default()
    };
    std::fs::write(manifest(&custom), encoded_manifest(&workspace).unwrap()).unwrap();
    assert_eq!(prepare(&custom).unwrap(), 1);
    assert!(!custom.join(PINS_DIR).exists());
    assert_eq!(db::load_summary(&custom).unwrap().pins.len(), 1);
}
