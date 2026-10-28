use super::*;
use crate::pin_raster_document::capture_source::test_fixture::{Case, TestDir};
fn placement() -> Placement {
    Placement {
        x: -1910,
        y: -690,
        width: 60,
        height: 52,
    }
}
fn metadata(case: &Case) -> RasterSnapshotMetadata {
    RasterSnapshotMetadata {
        original_size: (case.selected.width, case.selected.height),
        presentation_style: Some(case.style),
        document: case.document(),
        processing_history: None,
    }
}
fn store(dir: &Path, id: &str, case: &Case) -> Result<()> {
    let raw = case.raw();
    let output = case.output(&case.editor);
    let source = case.metadata();
    save_raster_snapshot_with_encoders_in(
        dir,
        id,
        &PinState::default(),
        placement(),
        PinStatus::Closed,
        metadata(case),
        |path| export::save_png_pixels(output.width, output.height, &output.bgra, path),
        |path| {
            crate::pin_raster_document::capture_source::resource::save_png_pixels_with_context(
                &raw,
                &source,
                &case.pixels,
                path,
            )
        },
    )
}
#[test]
fn actual_workspace_capture_claim_restores_full_sampling_document_mask_groups_both_stacks() {
    let case = Case::new();
    let dir = TestDir::new("workspace");
    let session = Session::register_in(&dir.0, PinState::default(), placement()).unwrap();
    let id = session.id.clone();
    store(&dir.0, &id, &case).unwrap();
    drop(session);
    let (session, image, entry) = Session::claim_in(&dir.0, &id).unwrap();
    assert_eq!(entry.status, PinStatus::Closed);
    assert_eq!(image.bgra, case.raw().bgra);
    assert_eq!(entry.raster_document, Some(case.document()));
    let pixels = session.capture_pixels.as_ref().unwrap();
    assert_eq!(pixels.bounds, case.pixels.bounds);
    assert_eq!(
        pixels.planes[0].image.as_bytes(0).unwrap(),
        case.pixels.planes[0].image.as_bytes(0).unwrap()
    );
    let installed = case.editor_from(entry.raster_document.as_ref().unwrap());
    assert_eq!(
        installed.document.exact_history_snapshot().unwrap(),
        case.editor.document.exact_history_snapshot().unwrap()
    );
    assert_eq!(case.output(&installed).bgra, case.output(&case.editor).bgra);
    let canonical = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
    assert!(
        crate::pin_raster_document::capture_source::resource::load_context(
            &canonical,
            &case.metadata(),
            &case.raw()
        )
        .is_err(),
        "canonical只含选区内容，full context属于原始恢复资源"
    );
    assert_eq!(
        export::read_image(&canonical).unwrap().bgra,
        case.output(&case.editor).bgra
    );
}
#[test]
fn workspace_capture_failed_source_encoding_and_corrupt_claim_preserve_previous_assets_and_retry() {
    let case = Case::new();
    let dir = TestDir::new("workspace-bad");
    let session = Session::register_in(&dir.0, PinState::default(), placement()).unwrap();
    let id = session.id.clone();
    store(&dir.0, &id, &case).unwrap();
    let entry = load_in(&dir.0).unwrap().pins.remove(0);
    let manifest_before = db::export_wire_bytes(&dir.0).unwrap();
    let raw_path = asset_path_in(&dir.0, entry.original_file.as_deref().unwrap()).unwrap();
    let canonical = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
    let original = std::fs::read(&raw_path).unwrap();
    let current = std::fs::read(&canonical).unwrap();
    assert!(
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &id,
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            metadata(&case),
            |path| export::save_png(case.output(&case.editor), path),
            |path| export::save_png(case.raw(), path)
        )
        .is_err(),
        "source metadata配普通PNG必须在清单发布前拒绝"
    );
    assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), manifest_before);
    assert_eq!(std::fs::read(&raw_path).unwrap(), original);
    assert_eq!(std::fs::read(&canonical).unwrap(), current);
    drop(session);
    export::save_png(case.raw(), &raw_path).unwrap();
    let broken = std::fs::read(&raw_path).unwrap();
    assert!(Session::claim_in(&dir.0, &id).is_err());
    let after = load_in(&dir.0).unwrap().pins.remove(0);
    assert_eq!(after.status, entry.status);
    assert_eq!(after.original_file, entry.original_file);
    assert_eq!(after.raster_document, entry.raster_document);
    assert_eq!(std::fs::read(&raw_path).unwrap(), broken);
    assert_eq!(std::fs::read(&canonical).unwrap(), current);
    std::fs::write(&raw_path, &original).unwrap();
    let (lease, _, _) = Session::claim_in(&dir.0, &id).unwrap();
    assert!(lease.capture_pixels.is_some());
}
