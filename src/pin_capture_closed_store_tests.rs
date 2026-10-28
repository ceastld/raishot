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
fn closed(case: &Case) -> RasterClosedMetadata {
    RasterClosedMetadata {
        style: Some(case.style),
        document: case.document(),
        processing_history: None,
    }
}
#[test]
fn actual_closed_capture_resource_restores_sampling_domain_exact_stacks_and_only_consumes_after_install(
) {
    let case = Case::new();
    let dir = TestDir::new("closed");
    let raw = case.raw();
    save_processed_document_with_context_in(
        &dir.0,
        raw,
        &PinState::default(),
        placement(),
        PinWindowSizing::General,
        closed(&case),
        Some(case.pixels.clone()),
    )
    .unwrap();
    let path = files(&dir.0).unwrap().remove(0);
    let png = std::fs::read(path.with_extension("png")).unwrap();
    let restored = restore_in(&dir.0).unwrap();
    assert_eq!(restored.image.as_ref().unwrap().bgra, case.raw().bgra);
    let pixels = restored.capture_pixels.as_ref().unwrap();
    assert_eq!(pixels.bounds, case.pixels.bounds);
    assert_eq!(
        pixels.planes[0].image.as_bytes(0).unwrap(),
        case.pixels.planes[0].image.as_bytes(0).unwrap()
    );
    let document = restored.raster_document.as_ref().unwrap();
    let installed = case.editor_from(document);
    assert_eq!(
        installed.document.exact_history_snapshot().unwrap(),
        case.editor.document.exact_history_snapshot().unwrap()
    );
    assert_eq!(case.output(&installed).bgra, case.output(&case.editor).bgra);
    drop(restored);
    assert!(path.exists());
    assert_eq!(std::fs::read(path.with_extension("png")).unwrap(), png);
    restore_in(&dir.0).unwrap().commit().unwrap();
    assert!(
        !path.exists()
            && !path.with_extension("png").exists()
            && !path.with_extension("json").exists()
    );
}
#[test]
fn closed_capture_bad_context_or_visible_frame_never_publishes_and_bad_restore_keeps_record() {
    let case = Case::new();
    let dir = TestDir::new("closed-bad");
    save_processed_document_with_context_in(
        &dir.0,
        case.raw(),
        &PinState::default(),
        placement(),
        PinWindowSizing::General,
        closed(&case),
        Some(case.pixels.clone()),
    )
    .unwrap();
    let path = files(&dir.0).unwrap().remove(0);
    let ini = std::fs::read(&path).unwrap();
    let png = std::fs::read(path.with_extension("png")).unwrap();
    let json = std::fs::read(path.with_extension("json")).unwrap();
    assert!(save_processed_document_with_context_in(
        &dir.0,
        case.raw(),
        &PinState::default(),
        placement(),
        PinWindowSizing::General,
        closed(&case),
        None
    )
    .is_err());
    let mut bad = case.raw();
    bad.bgra[0] ^= 1;
    assert!(save_processed_document_with_context_in(
        &dir.0,
        bad,
        &PinState::default(),
        placement(),
        PinWindowSizing::General,
        closed(&case),
        Some(case.pixels.clone())
    )
    .is_err());
    assert_eq!(files(&dir.0).unwrap(), vec![path.clone()]);
    assert_eq!(std::fs::read(&path).unwrap(), ini);
    assert_eq!(std::fs::read(path.with_extension("png")).unwrap(), png);
    crate::export::save_png(case.raw(), &path.with_extension("png")).unwrap();
    let malformed = std::fs::read(path.with_extension("png")).unwrap();
    assert!(restore_in(&dir.0).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), ini);
    assert_eq!(
        std::fs::read(path.with_extension("png")).unwrap(),
        malformed
    );
    assert_eq!(std::fs::read(path.with_extension("json")).unwrap(), json);
    std::fs::write(path.with_extension("png"), &png).unwrap();
    restore_in(&dir.0).unwrap().commit().unwrap();
}
