use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(new_id("rustshot-pin-db"));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn legacy(&self, workspace: &Workspace) -> Vec<u8> {
        let bytes = encoded_manifest(workspace).unwrap();
        std::fs::write(manifest(&self.0), &bytes).unwrap();
        bytes
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
            .starts_with("rustshot-pin-db-"));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn entry(index: usize, text: Option<WireTextDocument>) -> PinEntry {
    PinEntry {
        id: new_id("pin"),
        group_id: DEFAULT_GROUP_ID.into(),
        title: format!("项目 {index}"),
        status: PinStatus::Active,
        hidden: false,
        click_through: false,
        placement: Placement {
            x: 0,
            y: 0,
            width: 300,
            height: 200,
        },
        window_sizing: PinWindowSizing::General,
        state: PinState::default(),
        image_file: Some(format!("20261023-1500{index:02}.png")),
        text_document: text,
        presentation_style: None,
        original_file: None,
        raster_document: None,
        processing_history: None,
        updated_ms: now_ms(),
    }
}

fn text_document(body: &str) -> WireTextDocument {
    use crate::annotation::{Annotation, Point, Style, Tool};
    let full = crate::geometry::Rect::full(300, 200);
    let mut editor = crate::editor::Editor::new(full, Some(full));
    let mut text = Annotation::new(Tool::Text, Point { x: 2., y: 3. }, Style::default());
    text.update(Point { x: 290., y: 190. });
    text.text = body.into();
    editor.document.insert(text);
    WireTextDocument::try_from_text_editor(&editor, (300, 200), true).unwrap()
}

#[test]
fn pin_workspace_db_import_once_preserves_legacy_and_uses_database_afterwards() {
    let fixture = Fixture::new();
    let pin = entry(0, Some(text_document("当前矢量文字")));
    let id = pin.id.clone();
    let workspace = Workspace {
        pins: vec![pin],
        revision: 12,
        ..Workspace::default()
    };
    let legacy = fixture.legacy(&workspace);
    assert_eq!(load(&fixture.0).unwrap().revision, 12);
    assert!(fixture.0.join("library.sqlite3").is_file());
    persist_interaction(&fixture.0, &id, true, true).unwrap();
    assert_eq!(std::fs::read(manifest(&fixture.0)).unwrap(), legacy);
    std::fs::write(manifest(&fixture.0), b"old writer changed the backup").unwrap();
    let loaded = load(&fixture.0).unwrap();
    assert!(loaded.pins[0].state.locked);
    assert!(loaded.pins[0].click_through);
    assert_eq!(
        loaded.pins[0].text_document,
        workspace.pins[0].text_document
    );
    assert_eq!(loaded.revision, 13);
}

#[test]
fn pin_workspace_db_failed_import_rolls_back_and_can_retry() {
    let fixture = Fixture::new();
    let mut wire = WireWorkspace::from(&Workspace::default());
    wire.version = 999;
    let bad = serde_json::to_vec(&wire).unwrap();
    std::fs::write(manifest(&fixture.0), &bad).unwrap();
    assert!(load(&fixture.0).is_err());
    assert_eq!(std::fs::read(manifest(&fixture.0)).unwrap(), bad);
    let connection = crate::storage_db::open_workspace(&fixture.0).unwrap();
    assert!(!initialized(&connection).unwrap());
    drop(connection);
    let workspace = Workspace {
        pins: vec![entry(0, None)],
        ..Workspace::default()
    };
    fixture.legacy(&workspace);
    assert_eq!(load(&fixture.0).unwrap().pins[0].id, workspace.pins[0].id);
}

#[test]
fn pin_workspace_db_import_preserves_preexisting_catalog_tables() {
    let fixture = Fixture::new();
    let connection = crate::storage_db::open_workspace(&fixture.0).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE catalog_test_existing(value TEXT NOT NULL);
             INSERT INTO catalog_test_existing(value) VALUES('keep catalog');",
        )
        .unwrap();
    let workspace = Workspace {
        pins: vec![entry(0, None)],
        revision: 17,
        ..Workspace::default()
    };
    let legacy = fixture.legacy(&workspace);
    assert_eq!(load(&fixture.0).unwrap().revision, 17);
    let preserved: String = connection
        .query_row("SELECT value FROM catalog_test_existing", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(preserved, "keep catalog");
    assert_eq!(std::fs::read(manifest(&fixture.0)).unwrap(), legacy);
}

#[test]
fn pin_workspace_db_concurrent_first_access_imports_one_consistent_workspace() {
    use std::sync::{Arc, Barrier};
    let fixture = Fixture::new();
    // 与目录扫描先访问同库的情形相同，数据库已存在但尚无贴图表。
    drop(crate::storage_db::open_workspace(&fixture.0).unwrap());
    let workspace = Workspace {
        pins: vec![entry(0, None), entry(1, None)],
        revision: 23,
        ..Workspace::default()
    };
    fixture.legacy(&workspace);
    let expected: Vec<_> = workspace.pins.iter().map(|pin| pin.id.clone()).collect();
    let barrier = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let tasks = (0..2)
            .map(|_| {
                let barrier = barrier.clone();
                let path = fixture.0.clone();
                scope.spawn(move || {
                    barrier.wait();
                    load(&path)
                        .map(|workspace| {
                            (
                                workspace.revision,
                                workspace
                                    .pins
                                    .into_iter()
                                    .map(|pin| pin.id)
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .map_err(|error| error.to_string())
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for task in tasks {
            let (revision, ids) = task.join().unwrap().unwrap();
            assert_eq!(revision, 23);
            assert_eq!(ids, expected);
        }
    });
}

#[test]
fn pin_workspace_db_summary_and_state_updates_do_not_decode_or_rewrite_documents() {
    let fixture = Fixture::new();
    let workspace = Workspace {
        pins: vec![entry(0, Some(text_document("预览文字")))],
        ..Workspace::default()
    };
    write(&fixture.0, &encoded_manifest(&workspace).unwrap()).unwrap();
    let id = &workspace.pins[0].id;
    let connection = crate::storage_db::open_workspace(&fixture.0).unwrap();
    connection
        .execute(
            "UPDATE pin_documents SET document_json='unreadable document' WHERE pin_id=?1",
            [id],
        )
        .unwrap();
    connection.execute_batch("CREATE TRIGGER pin_test_no_document_update BEFORE UPDATE ON pin_documents BEGIN SELECT RAISE(FAIL,'document changed'); END;").unwrap();
    let summary = load_summary(&fixture.0).unwrap();
    assert_eq!(summary.pins[0].preview_text.as_deref(), Some("预览文字"));
    assert!(summary.pins[0].text);
    persist_interaction(&fixture.0, id, true, true).unwrap();
    let mut state = workspace.pins[0].state.clone();
    state.zoom = 1.5;
    let mut placement = workspace.pins[0].placement;
    placement.x = 123;
    assert!(update_state(&fixture.0, id, &state, placement, None).unwrap());
    let summary = load_summary(&fixture.0).unwrap();
    assert_eq!(summary.pins[0].placement.x, 123);
    assert_eq!(summary.pins[0].state.zoom, 1.5);
    assert!(summary.pins[0].click_through);
    assert!(load(&fixture.0).is_err());
    let document: String = connection
        .query_row(
            "SELECT document_json FROM pin_documents WHERE pin_id=?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(document, "unreadable document");
}

#[test]
fn pin_workspace_db_independent_documents_exceed_old_total_limit_without_trimming() {
    let fixture = Fixture::new();
    let document = text_document(&"A".repeat(55 * 1024));
    let workspace = Workspace {
        pins: (0..6)
            .map(|index| entry(index, Some(document.clone())))
            .collect(),
        ..Workspace::default()
    };
    let bytes = encoded_manifest(&workspace).unwrap();
    assert!(bytes.len() as u64 > MAX_MANIFEST_BYTES);
    write(&fixture.0, &bytes).unwrap();
    let loaded = load(&fixture.0).unwrap();
    assert_eq!(loaded.pins.len(), 6);
    for pin in loaded.pins {
        assert_eq!(pin.text_document.as_ref(), Some(&document));
    }
    assert!(!manifest(&fixture.0).exists());
}

#[test]
fn pin_workspace_db_metadata_and_documents_rollback_together_on_commit_failure() {
    let fixture = Fixture::new();
    let mut workspace = Workspace {
        pins: vec![entry(0, Some(text_document("旧文字")))],
        revision: 4,
        ..Workspace::default()
    };
    write(&fixture.0, &encoded_manifest(&workspace).unwrap()).unwrap();
    let before = export_wire_bytes(&fixture.0).unwrap();
    let connection = crate::storage_db::open_workspace(&fixture.0).unwrap();
    connection.execute_batch("CREATE TRIGGER pin_test_reject_meta BEFORE UPDATE ON pin_meta BEGIN SELECT RAISE(FAIL,'test rejection'); END;").unwrap();
    workspace.revision += 1;
    workspace.pins[0].title = "新标题".into();
    workspace.pins[0].text_document = Some(text_document("新文字"));
    assert!(write(&fixture.0, &encoded_manifest(&workspace).unwrap()).is_err());
    assert_eq!(export_wire_bytes(&fixture.0).unwrap(), before);
}

#[test]
fn pin_workspace_db_startup_probe_does_not_create_or_migrate_storage() {
    let fixture = Fixture::new();
    let absent = fixture.0.join("not-created");
    assert!(!startup_restore_needed(&absent).unwrap());
    assert!(!absent.exists());
    let workspace = Workspace {
        pins: vec![entry(0, None)],
        ..Workspace::default()
    };
    std::fs::create_dir(fixture.0.join(PINS_DIR)).unwrap();
    export::save_png_pixels(
        2,
        2,
        &[255; 16],
        &asset_path_in(&fixture.0, workspace.pins[0].image_file.as_deref().unwrap()).unwrap(),
    )
    .unwrap();
    let legacy = fixture.legacy(&workspace);
    assert!(startup_restore_needed(&fixture.0).unwrap());
    assert!(!fixture.0.join("library.sqlite3").exists());
    assert_eq!(std::fs::read(manifest(&fixture.0)).unwrap(), legacy);
    load(&fixture.0).unwrap();
    let connection = crate::storage_db::open_workspace(&fixture.0).unwrap();
    connection
        .execute("UPDATE pin_meta SET restore_on_startup=0", [])
        .unwrap();
    assert!(!startup_restore_needed(&fixture.0).unwrap());
}

#[test]
fn pin_workspace_db_startup_probe_skips_hidden_closed_other_group_and_live_records() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.0.join(PINS_DIR)).unwrap();
    let mut closed = entry(0, None);
    closed.status = PinStatus::Closed;
    let mut hidden = entry(1, None);
    hidden.hidden = true;
    let mut other = entry(2, None);
    other.group_id = new_id("group");
    let mut incomplete = entry(3, None);
    incomplete.image_file = None;
    let missing = entry(4, None);
    let live = entry(5, None);
    let lease = acquire_lease(&fixture.0, &live.id).unwrap();
    for pin in [&closed, &hidden, &other, &live] {
        export::save_png_pixels(
            2,
            2,
            &[255; 16],
            &asset_path_in(&fixture.0, pin.image_file.as_deref().unwrap()).unwrap(),
        )
        .unwrap();
    }
    let mut workspace = Workspace::default();
    workspace.groups.push(Group {
        id: other.group_id.clone(),
        name: "其它分组".into(),
    });
    workspace.pins = vec![closed, hidden, other, incomplete, missing, live];
    write(&fixture.0, &encoded_manifest(&workspace).unwrap()).unwrap();
    assert!(!startup_restore_needed(&fixture.0).unwrap());
    drop(lease);
    assert!(startup_restore_needed(&fixture.0).unwrap());
}
