//! 只移除全部图片来源已明确丢失的非活动记录；现存资源与其它清理政策互不影响。
use super::*;

pub(crate) fn prune_missing_records() -> Result<usize> {
    prune_in(&folder())
}

fn prune_in(dir: &Path) -> Result<usize> {
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    crate::image_collection::ensure_local_directory(dir)?;
    // 使用同一把事务锁；此任务不顺带迁移布局、修剪容量或删除孤儿资源。
    let _lock = global_lock_with_layout(dir, false)?;
    let workspace = db::load_summary(dir)?;
    // 该次摘要读取可能刚导入旧JSON；在决定缺源之前再做一次锁内补齐。
    relocation::reconcile_locked(dir)?;
    let removed = workspace
        .pins
        .iter()
        .filter(|pin| {
            // 来源标记随文档在同一事务提交；兼容帧丢失不等于文字内容丢失。
            if pin.text || lease_is_live(dir, &pin.id) {
                return false;
            }
            let mut assets = [&pin.image_file, &pin.original_file]
                .into_iter()
                .flatten()
                .peekable();
            // 尚未写入首次快照的记录不作“文件丢失”推断。
            assets.peek().is_some() && assets.all(|name| definitely_missing(dir, name))
        })
        .map(|pin| pin.id.clone())
        .collect::<Vec<_>>();
    db::remove_records(dir, &removed)
}

fn definitely_missing(dir: &Path, name: &str) -> bool {
    let Ok(path) = asset_path_in(dir, name) else {
        return false;
    };
    // 只查元数据；目录链接、云端占位、权限或临时读取问题都保持原记录。
    if crate::image_collection::ensure_local_file_or_missing(&path).is_err() {
        return false;
    }
    std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(new_id("rustshot-pin-missing"));
            std::fs::create_dir(&path).unwrap();
            std::fs::create_dir(path.join(PINS_DIR)).unwrap();
            Self(path)
        }
        fn save(&self, pins: Vec<PinEntry>) {
            let workspace = Workspace {
                pins,
                revision: 7,
                ..Workspace::default()
            };
            write_manifest(&self.0, &encoded_manifest(&workspace).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // 仅删除本测试刚创建、位于系统临时目录的唯一目录。
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            assert!(self
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rustshot-pin-missing-"));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(index: usize) -> PinEntry {
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
            image_file: Some(format!("20261023-1200{index:02}.png")),
            text_document: None,
            presentation_style: None,
            original_file: None,
            raster_document: None,
            processing_history: None,
            updated_ms: now_ms(),
        }
    }

    fn text_document() -> WireTextDocument {
        use crate::annotation::{Annotation, Point, Style, Tool};
        let full = crate::geometry::Rect::full(2, 2);
        let mut editor = crate::editor::Editor::new(full, Some(full));
        let mut annotation = Annotation::new(Tool::Text, Point { x: 0., y: 0. }, Style::default());
        annotation.update(Point { x: 2., y: 2. });
        annotation.text = "可恢复的文字".into();
        editor.document.insert(annotation);
        WireTextDocument::try_from_text_editor(&editor, (2, 2), true).unwrap()
    }

    #[test]
    fn pin_workspace_prune_missing_keeps_live_text_original_and_uncertain_sources() {
        let fixture = Fixture::new();
        let missing = entry(0);
        let live = entry(1);
        let lease = acquire_lease(&fixture.0, &live.id).unwrap();
        let mut text = entry(2);
        text.text_document = Some(text_document());
        let mut raster = entry(3);
        raster.original_file = Some("20261023-120003.original.png".into());
        raster.raster_document = Some(WireRasterDocument::empty((2, 2)).unwrap());
        let original = asset_path_in(&fixture.0, raster.original_file.as_deref().unwrap()).unwrap();
        export::save_png_pixels(2, 2, &[255; 16], &original).unwrap();
        let corrupted = entry(4);
        let corrupted_path =
            asset_path_in(&fixture.0, corrupted.image_file.as_deref().unwrap()).unwrap();
        std::fs::write(&corrupted_path, b"incomplete png").unwrap();
        let not_file = entry(5);
        let directory = asset_path_in(&fixture.0, not_file.image_file.as_deref().unwrap()).unwrap();
        std::fs::create_dir(&directory).unwrap();
        let mut no_snapshot = entry(6);
        no_snapshot.image_file = None;
        let retained = vec![live, text, raster, corrupted, not_file, no_snapshot];
        let retained_ids: Vec<_> = retained.iter().map(|pin| pin.id.clone()).collect();
        let mut pins = vec![missing];
        pins.extend(retained);
        fixture.save(pins);

        assert_eq!(prune_in(&fixture.0).unwrap(), 1);
        let workspace = load_in(&fixture.0).unwrap();
        assert_eq!(workspace.revision, 8);
        assert_eq!(
            workspace
                .pins
                .iter()
                .map(|pin| pin.id.clone())
                .collect::<Vec<_>>(),
            retained_ids
        );
        assert!(original.is_file());
        assert_eq!(std::fs::read(corrupted_path).unwrap(), b"incomplete png");
        assert!(directory.is_dir());
        let manifest_before = db::export_wire_bytes(&fixture.0).unwrap();
        assert_eq!(prune_in(&fixture.0).unwrap(), 0);
        assert_eq!(db::export_wire_bytes(&fixture.0).unwrap(), manifest_before);
        drop(lease);
    }

    #[test]
    fn pin_workspace_prune_missing_does_not_migrate_or_delete_unreferenced_assets() {
        let fixture = Fixture::new();
        let missing = entry(0);
        let orphan = fixture
            .0
            .join(format!("{}-asset-{}.png", new_id("pin"), unique_suffix()));
        std::fs::write(&orphan, b"existing legacy resource").unwrap();
        let orphan_current = fixture.0.join(PINS_DIR).join("20261023-130000.png");
        std::fs::write(&orphan_current, b"existing resource").unwrap();
        fixture.save(vec![missing]);

        assert_eq!(prune_in(&fixture.0).unwrap(), 1);
        assert!(load_in(&fixture.0).unwrap().pins.is_empty());
        assert_eq!(std::fs::read(orphan).unwrap(), b"existing legacy resource");
        assert_eq!(std::fs::read(orphan_current).unwrap(), b"existing resource");
    }

    #[test]
    fn pin_workspace_prune_missing_handles_absent_pins_directory_without_creating_it() {
        let fixture = Fixture::new();
        std::fs::remove_dir(fixture.0.join(PINS_DIR)).unwrap();
        fixture.save(vec![entry(0)]);
        assert_eq!(prune_in(&fixture.0).unwrap(), 1);
        assert!(!fixture.0.join(PINS_DIR).exists());
    }
}
