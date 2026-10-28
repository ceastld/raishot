//! 工作区控制只验证合成数据和本次自有磁盘租约，不创建窗口。
use super::*;
use std::cell::Cell;

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(new_id("pin-workspace-control-store"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        // 只递归删除工作区自有的 pins/ 与 .runtime/，其余按平铺文件删除，不跟随链接。
        for sub in ["pins", ".runtime"] {
            let _ = std::fs::remove_dir_all(self.0.join(sub));
        }
        if let Ok(entries) = std::fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}
fn placement() -> Placement {
    Placement {
        x: -1200,
        y: -320,
        width: 80,
        height: 72,
    }
}
fn image() -> Image {
    Image {
        width: 2,
        height: 2,
        bgra: vec![255; 16],
        readback_ms: 0.,
    }
}
fn registered(dir: &Path, status: PinStatus) -> Session {
    let session = Session::register_in(dir, PinState::default(), placement()).unwrap();
    save_snapshot_in(
        dir,
        &session.id,
        &image(),
        &PinState::default(),
        placement(),
        status,
    )
    .unwrap();
    session
}
fn text_document() -> WireTextDocument {
    use crate::annotation::{Annotation, Point, Style, Tool};
    let full = crate::geometry::Rect::full(2, 2);
    let mut editor = crate::editor::Editor::new(full, Some(full));
    let mut text = Annotation::new(Tool::Text, Point { x: 0., y: 0. }, Style::default());
    text.update(Point { x: 2., y: 2. });
    text.text = "模型文字".into();
    editor.document.insert(text);
    WireTextDocument::from_text_editor(&editor, (2, 2), true).unwrap()
}

#[test]
fn pin_workspace_control_click_through_wire_defaults_and_roundtrip() {
    let dir = TestDir::new();
    let session = registered(&dir.0, PinStatus::Active);
    let mut workspace = load_in(&dir.0).unwrap();
    assert!(!workspace.pins[0].click_through);
    workspace.pins[0].click_through = true;
    let value = serde_json::to_value(WireWorkspace::from(&workspace)).unwrap();
    assert_eq!(value["pins"][0]["clickThrough"], true);
    let restored = serde_json::from_value::<WireWorkspace>(value.clone())
        .unwrap()
        .into_model()
        .unwrap();
    assert_eq!(
        encoded_manifest(&workspace).unwrap(),
        encoded_manifest(&restored).unwrap()
    );
    let mut legacy = value;
    legacy["pins"][0]
        .as_object_mut()
        .unwrap()
        .remove("clickThrough");
    let restored = serde_json::from_value::<WireWorkspace>(legacy.clone())
        .unwrap()
        .into_model()
        .unwrap();
    workspace.pins[0].click_through = false;
    assert_eq!(
        encoded_manifest(&workspace).unwrap(),
        encoded_manifest(&restored).unwrap()
    );
    legacy["pins"][0]["clickThrough"] = serde_json::json!("false");
    assert!(serde_json::from_value::<WireWorkspace>(legacy).is_err());
    drop(session);
}

#[test]
fn pin_workspace_control_owner_generations_change_on_claim_without_consuming_records() {
    let dir = TestDir::new();
    let (session, entry) =
        Session::register_with_entry_in(&dir.0, PinState::default(), placement()).unwrap();
    assert_eq!(session.owner_generation, load_in(&dir.0).unwrap().revision);
    let first_generation = session.owner_generation;
    save_snapshot_in(
        &dir.0,
        &entry.id,
        &image(),
        &entry.state,
        placement(),
        PinStatus::Closed,
    )
    .unwrap();
    apply_in(
        &dir.0,
        Action::SetHidden {
            id: entry.id.clone(),
            hidden: true,
        },
    )
    .unwrap();
    drop(session);
    let mut expected = load_in(&dir.0).unwrap();
    expected.revision += 1;
    let (claimed, decoded, restored) = Session::claim_in(&dir.0, &entry.id).unwrap();
    assert!(claimed.owner_generation > first_generation);
    assert_eq!(claimed.owner_generation, expected.revision);
    assert_eq!(decoded.bgra, image().bgra);
    assert_eq!(restored.status, PinStatus::Closed);
    assert!(restored.hidden);
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    let generation = claimed.owner_generation;
    drop(claimed);
    expected.revision += 1;
    let (claimed, _, _) = Session::claim_in(&dir.0, &entry.id).unwrap();
    assert_eq!(claimed.owner_generation, generation + 1);
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
}

#[test]
fn pin_workspace_control_decode_failure_releases_lease_and_spends_generation() {
    let dir = TestDir::new();
    let session = registered(&dir.0, PinStatus::Closed);
    let id = session.id.clone();
    drop(session);
    let mut expected = load_in(&dir.0).unwrap();
    let path = asset_path_in(&dir.0, expected.pins[0].image_file.as_deref().unwrap()).unwrap();
    let png = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"controlled broken PNG").unwrap();
    assert!(Session::claim_in(&dir.0, &id).is_err());
    expected.revision += 1;
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    assert!(!lease_is_live(&dir.0, &id));
    assert_eq!(closed_in(&dir.0).unwrap()[0].id, id);
    std::fs::write(&path, &png).unwrap();
    let (claimed, decoded, entry) = Session::claim_in(&dir.0, &id).unwrap();
    assert_eq!(claimed.owner_generation, expected.revision + 1);
    assert_eq!(decoded.bgra, image().bgra);
    assert_eq!(entry.status, PinStatus::Closed);
    assert_eq!(std::fs::read(&path).unwrap(), png);
}

#[test]
fn pin_workspace_control_tickets_require_live_active_lease_and_hold_transaction_lock() {
    let dir = TestDir::new();
    let session = registered(&dir.0, PinStatus::Active);
    let id = session.id.clone();
    let mut expected = load_in(&dir.0).unwrap();
    assert!(Session::claim_in(&dir.0, &id).is_err());
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    let first = with_control_ticket_in(&dir.0, &id, |ticket| {
        assert!(global_lock(&dir.0).is_err());
        assert_eq!(load_in(&dir.0).unwrap().revision, ticket);
        Ok(ticket)
    })
    .unwrap();
    assert_eq!(first, expected.revision + 1);
    expected.revision = first;
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    assert!(with_control_ticket_in::<()>(&dir.0, &id, |ticket| {
        assert_eq!(ticket, first + 1);
        Err("受控发送失败".into())
    })
    .is_err());
    let next = with_control_ticket_in(&dir.0, &id, Ok).unwrap();
    assert_eq!(next, first + 2);
    apply_in(&dir.0, Action::Close { id: id.clone() }).unwrap();
    let before = db::export_wire_bytes(&dir.0).unwrap();
    let called = Cell::new(false);
    assert!(with_control_ticket_in(&dir.0, &id, |_| {
        called.set(true);
        Ok(())
    })
    .is_err());
    assert!(!called.get());
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    session.mark_active().unwrap();
    drop(session);
    let before = db::export_wire_bytes(&dir.0).unwrap();
    assert!(with_control_ticket_in(&dir.0, &id, Ok).is_err());
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
}

#[test]
fn pin_workspace_control_busy_lock_never_calls_or_mutates() {
    let dir = TestDir::new();
    let session = registered(&dir.0, PinStatus::Active);
    let before = db::export_wire_bytes(&dir.0).unwrap();
    let lock = global_lock(&dir.0).unwrap();
    let called = Cell::new(false);
    assert!(with_control_ticket_in(&dir.0, &session.id, |_| {
        called.set(true);
        Ok(())
    })
    .is_err());
    assert!(with_control_lock_in(&dir.0, || {
        called.set(true);
        Ok(())
    })
    .is_err());
    assert!(session.persist_interaction(true, true).is_err());
    assert!(Session::claim_in(&dir.0, &session.id).is_err());
    assert!(!called.get());
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    drop(lock);
    with_control_lock_in(&dir.0, || {
        assert!(global_lock(&dir.0).is_err());
        Ok(())
    })
    .unwrap();
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
}

#[test]
fn pin_workspace_control_persist_interaction_is_owned_idempotent_and_preserves_record() {
    let dir = TestDir::new();
    let mut session = registered(&dir.0, PinStatus::Active);
    let other = registered(&dir.0, PinStatus::Active);
    let state = PinState {
        zoom: 0.75,
        opacity: 117,
        topmost: false,
        shadow_enabled: false,
        thumbnail: true,
        normal_zoom: 1.5,
        adjustments: Adjustments {
            turns: 1,
            flip_x: true,
            grayscale: true,
            brightness: 12,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut canonical = state.clone();
    canonical.adjustments = Default::default();
    save_snapshot_with_text_document_in(
        &dir.0,
        &session.id,
        (2, 2),
        &canonical,
        placement(),
        PinStatus::Active,
        Some(text_document()),
        |path| export::save_png(image(), path),
    )
    .unwrap();
    update_state_in(&dir.0, &session.id, &state, placement()).unwrap();
    let group = apply_in(
        &dir.0,
        Action::Create {
            name: "有界交互模型".into(),
        },
    )
    .unwrap()
    .current_group;
    apply_in(
        &dir.0,
        Action::MovePin {
            id: session.id.clone(),
            group_id: group,
        },
    )
    .unwrap();
    apply_in(
        &dir.0,
        Action::SetHidden {
            id: session.id.clone(),
            hidden: true,
        },
    )
    .unwrap();
    apply_in(
        &dir.0,
        Action::SetTitle {
            id: session.id.clone(),
            title: "保留标题与图片".into(),
        },
    )
    .unwrap();
    let mut expected = load_in(&dir.0).unwrap();
    let index = expected
        .pins
        .iter()
        .position(|pin| pin.id == session.id)
        .unwrap();
    let path = asset_path_in(&dir.0, expected.pins[index].image_file.as_deref().unwrap()).unwrap();
    let png = std::fs::read(&path).unwrap();
    session.persist_interaction(true, true).unwrap();
    expected.pins[index].state.locked = true;
    expected.pins[index].click_through = true;
    expected.revision += 1;
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    session.persist_interaction(true, true).unwrap();
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    // pub id 不能让持有 A 租约的 Session 冒充 B；原记录和版本都不得改变。
    let id = session.id.clone();
    session.id = other.id.clone();
    assert!(session.persist_interaction(false, false).is_err());
    session.id = id;
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    session.persist_interaction(false, false).unwrap();
    expected.pins[index].state.locked = false;
    expected.pins[index].click_through = false;
    expected.revision += 1;
    session.persist_interaction(false, false).unwrap();
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), png);
}

#[test]
fn pin_workspace_control_closed_or_released_session_cannot_persist() {
    let dir = TestDir::new();
    let mut session = registered(&dir.0, PinStatus::Closed);
    let before = db::export_wire_bytes(&dir.0).unwrap();
    assert!(session.persist_interaction(true, true).is_err());
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    session.mark_active().unwrap();
    drop(session.lease.take());
    assert!(!lease_is_live(&dir.0, &session.id));
    let before = db::export_wire_bytes(&dir.0).unwrap();
    assert!(session.persist_interaction(true, true).is_err());
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    assert!(with_control_ticket_in(&dir.0, "../outside", Ok).is_err());
    assert!(with_control_ticket_in(&dir.0, "missing-pin", Ok).is_err());
    assert!(!lease_live("../outside"));
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
}

#[test]
fn pin_workspace_control_generation_exhaustion_never_wraps_or_claims() {
    let dir = TestDir::new();
    let session = registered(&dir.0, PinStatus::Active);
    let id = session.id.clone();
    let mut workspace = load_in(&dir.0).unwrap();
    workspace.revision = usize::MAX as u64;
    write_manifest(&dir.0, &encoded_manifest(&workspace).unwrap()).unwrap();
    let before = db::export_wire_bytes(&dir.0).unwrap();
    let called = Cell::new(false);
    assert!(with_control_ticket_in(&dir.0, &id, |_| {
        called.set(true);
        Ok(())
    })
    .is_err());
    assert!(!called.get());
    assert!(Session::register_in(&dir.0, PinState::default(), placement()).is_err());
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    drop(session);
    assert!(Session::claim_in(&dir.0, &id).is_err());
    assert!(!lease_is_live(&dir.0, &id));
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    workspace.revision = u64::MAX;
    assert!(next_control_revision(&workspace).is_err());
}

#[test]
fn pin_workspace_control_closed_snapshot_keeps_saved_interaction_across_restore() {
    let dir = TestDir::new();
    let session = registered(&dir.0, PinStatus::Active);
    let id = session.id.clone();
    let state = PinState {
        locked: true,
        opacity: 117,
        topmost: false,
        ..Default::default()
    };
    let group = apply_in(
        &dir.0,
        Action::Create {
            name: "关闭与恢复".into(),
        },
    )
    .unwrap()
    .current_group;
    apply_in(
        &dir.0,
        Action::MovePin {
            id: id.clone(),
            group_id: group,
        },
    )
    .unwrap();
    apply_in(
        &dir.0,
        Action::SetHidden {
            id: id.clone(),
            hidden: true,
        },
    )
    .unwrap();
    session.persist_interaction(true, true).unwrap();
    // 交互参数先由自身活动租约落盘，紧接着的关闭 PNG 不得盖写穿透字段。
    save_snapshot_in(
        &dir.0,
        &id,
        &image(),
        &state,
        placement(),
        PinStatus::Closed,
    )
    .unwrap();
    let mut expected = load_in(&dir.0).unwrap();
    assert!(expected.pins[0].click_through);
    assert!(expected.pins[0].state.locked);
    assert!(expected.pins[0].hidden);
    assert_eq!(expected.pins[0].state.opacity, 117);
    assert_eq!(expected.pins[0].group_id, expected.current_group);
    assert_eq!(expected.pins[0].status, PinStatus::Closed);
    drop(session);
    expected.revision += 1;
    let (claimed, decoded, entry) = Session::claim_in(&dir.0, &id).unwrap();
    assert!(entry.click_through && entry.state.locked && entry.hidden);
    assert_eq!(entry.state, state);
    assert_eq!(entry.status, PinStatus::Closed);
    assert_eq!(decoded.bgra, image().bgra);
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
    claimed.mark_active().unwrap();
    claimed.persist_interaction(true, false).unwrap();
    save_snapshot_in(
        &dir.0,
        &id,
        &image(),
        &state,
        placement(),
        PinStatus::Closed,
    )
    .unwrap();
    let mut expected = load_in(&dir.0).unwrap();
    assert!(!expected.pins[0].click_through);
    assert_eq!(expected.pins[0].state, state);
    assert!(expected.pins[0].hidden);
    assert_eq!(expected.pins[0].group_id, entry.group_id);
    drop(claimed);
    expected.revision += 1;
    let (_claimed, decoded, restored) = Session::claim_in(&dir.0, &id).unwrap();
    assert!(!restored.click_through);
    assert_eq!(restored.state, state);
    assert_eq!(restored.status, PinStatus::Closed);
    assert_eq!(decoded.bgra, image().bgra);
    assert_eq!(
        encoded_manifest(&expected).unwrap(),
        encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
    );
}
