//! 分组命令只使用合成工作区和测试自有数据库，不创建或操作桌面窗口。
use super::*;

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(new_id("pin-workspace-group-command"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        // 仅清理本测试创建的平铺数据库和工作区运行目录，不遍历其它目录。
        for sub in [PINS_DIR, RUNTIME_DIR] {
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
fn pin(group_id: &str, hidden: bool, status: PinStatus) -> PinEntry {
    PinEntry {
        id: new_id("pin"),
        group_id: group_id.into(),
        title: "分组操作测试".into(),
        status,
        hidden,
        click_through: true,
        placement: Placement {
            x: -720,
            y: -180,
            width: 80,
            height: 60,
        },
        window_sizing: PinWindowSizing::General,
        state: PinState {
            locked: true,
            ..Default::default()
        },
        image_file: None,
        text_document: None,
        presentation_style: None,
        original_file: None,
        raster_document: None,
        processing_history: None,
        updated_ms: 1,
    }
}
fn seed(dir: &Path, workspace: &Workspace) {
    let _lock = global_lock(dir).unwrap();
    write_manifest(dir, &encoded_manifest(workspace).unwrap()).unwrap();
}
fn ids(workspace: &Workspace) -> Vec<String> {
    workspace.pins.iter().map(|pin| pin.id.clone()).collect()
}

#[test]
fn pin_group_toggle_mixed_visibility_is_atomic_and_preserves_pin_properties() {
    let dir = TestDir::new();
    let mut initial = Workspace::default();
    initial.pins = vec![
        pin(&initial.current_group, false, PinStatus::Active),
        pin(&initial.current_group, true, PinStatus::Active),
    ];
    seed(&dir.0, &initial);
    let live_ids = ids(&initial);
    for (index, hidden) in [true, false, true].into_iter().enumerate() {
        assert_eq!(
            toggle_current_group_in(&dir.0, |locked| {
                assert_eq!(locked.revision, initial.revision + index as u64);
                live_ids.clone()
            })
            .unwrap(),
            2
        );
        let saved = load_in(&dir.0).unwrap();
        assert_eq!(saved.revision, initial.revision + index as u64 + 1);
        for (before, after) in initial.pins.iter().zip(&saved.pins) {
            assert_eq!(after.hidden, hidden);
            assert_eq!(after.click_through, before.click_through);
            assert_eq!(after.state, before.state);
            assert_eq!(after.placement, before.placement);
            assert_eq!(after.window_sizing, before.window_sizing);
            assert_eq!(after.status, before.status);
        }
    }
}

#[test]
fn pin_group_toggle_excludes_closed_other_group_and_missing_live_windows() {
    let dir = TestDir::new();
    let mut initial = Workspace::default();
    let other_group = new_id("group");
    initial.groups.push(Group {
        id: other_group.clone(),
        name: "其它组".into(),
    });
    initial.pins = vec![
        pin(&initial.current_group, false, PinStatus::Active),
        pin(&initial.current_group, false, PinStatus::Closed),
        pin(&other_group, false, PinStatus::Active),
        pin(&initial.current_group, false, PinStatus::Active),
    ];
    seed(&dir.0, &initial);
    let live_ids = vec![
        initial.pins[0].id.clone(),
        initial.pins[0].id.clone(), // 重复窗口枚举不得重复计算同一记录。
        initial.pins[1].id.clone(),
        initial.pins[2].id.clone(),
        new_id("pin"),
    ];
    assert_eq!(toggle_current_group_in(&dir.0, |_| live_ids).unwrap(), 1);
    let saved = load_in(&dir.0).unwrap();
    assert!(saved.pins[0].hidden);
    let before = serde_json::to_value(WireWorkspace::from(&initial)).unwrap();
    let after = serde_json::to_value(WireWorkspace::from(&saved)).unwrap();
    for index in 1..initial.pins.len() {
        assert_eq!(after["pins"][index], before["pins"][index]);
    }
    assert_eq!(saved.revision, initial.revision + 1);
}

#[test]
fn pin_group_toggle_empty_or_unmatched_targets_do_not_commit() {
    let dir = TestDir::new();
    let mut workspace = Workspace::default();
    seed(&dir.0, &workspace);
    assert_eq!(toggle_current_group_in(&dir.0, |_| vec![]).unwrap(), 0);
    assert_eq!(load_in(&dir.0).unwrap().revision, workspace.revision);
    workspace.pins = vec![pin(&workspace.current_group, false, PinStatus::Active)];
    seed(&dir.0, &workspace);
    let before = encoded_manifest(&workspace).unwrap();
    assert_eq!(
        toggle_current_group_in(&dir.0, |_| vec![new_id("pin")]).unwrap(),
        0
    );
    assert_eq!(encoded_manifest(&load_in(&dir.0).unwrap()).unwrap(), before);
}

#[test]
fn pin_group_cycle_follows_order_and_wraps_in_both_directions() {
    let dir = TestDir::new();
    let mut workspace = Workspace::default();
    for name in ["参考", "待办"] {
        workspace.groups.push(Group {
            id: new_id("group"),
            name: name.into(),
        });
    }
    // 按持久化的用户排序循环，而不是按创建时间或名称排序。
    workspace.groups.swap(1, 2);
    workspace.pins = vec![pin(&workspace.current_group, true, PinStatus::Active)];
    seed(&dir.0, &workspace);
    let before = serde_json::to_value(WireWorkspace::from(&workspace)).unwrap();
    for (step, (forward, target)) in [(false, 2), (true, 0), (true, 1), (true, 2), (true, 0)]
        .into_iter()
        .enumerate()
    {
        let cycled = cycle_group_in(&dir.0, forward).unwrap();
        assert_eq!(cycled.current_group, workspace.groups[target].id);
        assert_eq!(cycled.revision, workspace.revision + step as u64 + 1);
        let saved = load_in(&dir.0).unwrap();
        assert_eq!(saved.current_group, cycled.current_group);
        assert_eq!(saved.revision, cycled.revision);
        let after = serde_json::to_value(WireWorkspace::from(&saved)).unwrap();
        assert_eq!(after["pins"], before["pins"]);
    }
}

#[test]
fn pin_group_cycle_single_or_empty_group_does_not_change_revision() {
    let dir = TestDir::new();
    let workspace = Workspace::default();
    seed(&dir.0, &workspace);
    for forward in [true, false] {
        let cycled = cycle_group_in(&dir.0, forward).unwrap();
        assert_eq!(cycled.current_group, workspace.current_group);
        assert_eq!(cycled.revision, workspace.revision);
        assert_eq!(load_in(&dir.0).unwrap().revision, workspace.revision);
    }
    let mut empty = Workspace {
        groups: Vec::new(),
        ..Workspace::default()
    };
    for forward in [true, false] {
        assert!(!cycle_group_selection(&mut empty, forward).unwrap());
        assert_eq!(empty.revision, 0);
    }
}
