use super::*;
use crate::pin_raster_document::capture_source::test_fixture::Case;
#[test]
fn capture_full_ocr_input_keeps_actual_effective_roi_for_none_or_larger_host_and_local_retry() {
    let mut case = Case::new();
    for crop in [None, case.editor.crop] {
        case.editor.crop = crop;
        let roi = crate::capture_pin_output::content_output_rect(
            case.source.full(),
            case.editor.crop,
            Some(case.selected),
            case.editor.document.crop_mask.as_ref(),
        )
        .unwrap();
        let scene = case.scene(&case.editor);
        let image = crate::capture_pin_output::content_image(
            &case.source,
            &scene,
            roi,
            Some(&case.cursor),
            case.editor.document.crop_mask.as_ref(),
        )
        .unwrap();
        let old = (1, None, Some(case.editor.document.revision()), crop, true);
        assert!(FixedInput::copy_with_profile(
            &image,
            old,
            (96, 72),
            OcrProvider::Ppocr,
            OcrProfile::Small
        )
        .is_none());
        let stamp = (
            1,
            None,
            Some(case.editor.document.revision()),
            Some(roi),
            true,
        );
        let fixed = FixedInput::copy_with_profile(
            &image,
            stamp,
            (96, 72),
            OcrProvider::Ppocr,
            OcrProfile::Small,
        )
        .unwrap();
        assert_eq!(fixed.frame.origin, (roi.x as i32, roi.y as i32));
        assert_eq!(
            (fixed.frame.width, fixed.frame.height),
            (roi.width, roi.height)
        );
        assert_eq!(fixed.source, stamp);
        assert_eq!(fixed.image.bgra, image.bgra);
        assert_eq!(fixed.profile, OcrProfile::Small);
    }
}
