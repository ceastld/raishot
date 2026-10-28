use super::*;
use crate::{
    annotation::{Point, Tool},
    pin_raster_document::capture_source::test_fixture::Case,
};
#[test]
fn full_capture_payload_keeps_inside_border_and_unexpanded_screen_bounds() {
    let case = Case::new();
    let selected = Rect::full(case.pixels.bounds.width, case.pixels.bounds.height);
    let (source, previews) = CanvasSource::capture_context(case.pixels.clone(), selected).unwrap();
    let editor = Editor::new(source.full(), Some(selected));
    let scene = Scene::prepare(&[], &case.pixels.source_pixels());
    let style = SelectionFinishStyle {
        border_enabled: true,
        border_inside: true,
        border_thickness: 2.25,
        ..Default::default()
    };
    let payload =
        CaptureDocumentPayload::prepare(&source, &previews, &editor, &scene, selected, None, style)
            .unwrap();
    let (raw, presentation, _, document) = payload.into_parts();
    assert_eq!(presentation.effective_style(), style);
    assert_eq!(presentation.outer_screen(), case.pixels.bounds);
    assert_eq!(raw.bgra, case.pixels.read_content(selected).unwrap().bgra);
    assert_eq!(document.storage_size(), (selected.width, selected.height));
}
#[test]
fn actual_capture_payload_keeps_source_objects_nested_groups_both_stacks_and_effect_pixels() {
    let mut case = Case::new();
    let before = case.editor.document.exact_history_snapshot().unwrap();
    let expected = case.output(&case.editor);
    let scene = case.scene(&case.editor);
    let payload = CaptureDocumentPayload::prepare(
        &case.source,
        &case.previews,
        &case.editor,
        &scene,
        case.selected,
        Some(&case.cursor),
        case.style,
    )
    .unwrap();
    let (raw, presentation, pixels, document) = payload.into_parts();
    assert_eq!(
        raw.bgra,
        case.raw().bgra,
        "兼容底图不能烘焙Scene、光标或套索"
    );
    assert_ne!(raw.bgra, expected.bgra);
    assert!(Arc::ptr_eq(&pixels, &case.pixels));
    assert_eq!(
        case.editor.document.exact_history_snapshot().unwrap(),
        before
    );
    assert_eq!(document.size(), (96, 72));
    assert_eq!(
        document.storage_size(),
        (case.selected.width, case.selected.height)
    );
    let mut installed = case.editor_from(&document);
    assert_eq!(installed.crop, case.editor.crop);
    assert_eq!(installed.document.exact_history_snapshot().unwrap(), before);
    assert!(installed
        .document
        .items()
        .iter()
        .any(|o| !o.nested_groups.is_empty()));
    assert_eq!(
        case.output(&installed).bgra,
        expected.bgra,
        "跨边界Blur/Multiply/Mosaic必须保持原来源相位"
    );
    let outer = presentation.outer_screen();
    let expected_origin =
        crate::capture_pin_output::finished_origin(case.pixels.bounds, case.selected, case.style);
    assert_eq!((outer.left, outer.top), expected_origin);
    for redo in [true, false, false] {
        if redo {
            installed.document.redo();
            case.editor.document.redo();
        } else {
            installed.document.undo();
            case.editor.document.undo();
        }
        assert_eq!(
            installed.document.exact_history_snapshot().unwrap(),
            case.editor.document.exact_history_snapshot().unwrap()
        );
        assert_eq!(case.output(&installed).bgra, case.output(&case.editor).bgra);
    }
}
#[test]
fn duplicate_installs_independent_document_and_shared_immutable_sampling_context() {
    let case = Case::new();
    let document = case.document();
    let mut first = case.editor_from(&document);
    let second = case.editor_from(&document);
    let (source, _) = CanvasSource::capture_context(case.pixels.clone(), case.selected).unwrap();
    let shared = source.shared_cpu_planes(&[]).unwrap();
    assert!(Arc::ptr_eq(&shared, &case.pixels));
    let before = second.document.exact_history_snapshot().unwrap();
    let bytes = shared.planes[0].image.as_bytes(0).unwrap().to_vec();
    first.document.redo();
    first.document.clear();
    assert_eq!(second.document.exact_history_snapshot().unwrap(), before);
    assert_eq!(shared.planes[0].image.as_bytes(0).unwrap(), bytes);
    assert_eq!(case.output(&second).bgra, case.output(&case.editor).bgra);
}
#[test]
fn large_virtual_canvas_small_selection_preserves_signed_origin_without_visible_budget_rejection() {
    use crate::{geometry::ScreenRect, pin_raster_document::capture_source::OwnedPlane};
    let bounds = ScreenRect {
        left: -8000,
        top: -1100,
        width: 12000,
        height: 4000,
    };
    assert!(u64::from(bounds.width) * u64::from(bounds.height) > 32_000_000);
    let selected = Rect {
        x: 7901,
        y: 1103,
        width: 12,
        height: 9,
    };
    let pixels = Arc::new(
        CapturePixels::new(
            bounds,
            Some([8, 18, 28, 255]),
            vec![OwnedPlane {
                rect: selected,
                image: crate::canvas_source::render_image(12, 9, [11, 21, 31, 255].repeat(108)),
            }],
        )
        .unwrap(),
    );
    let (source, previews) = CanvasSource::capture_context(pixels.clone(), selected).unwrap();
    let editor = Editor::new(source.full(), Some(selected));
    let scene = Scene::prepare(&[], &pixels.source_pixels());
    let payload = CaptureDocumentPayload::prepare(
        &source,
        &previews,
        &editor,
        &scene,
        selected,
        None,
        SelectionFinishStyle::default(),
    )
    .unwrap();
    let (image, presentation, _, document) = payload.into_parts();
    assert_eq!(image.bgra, [11, 21, 31, 255].repeat(108));
    assert_eq!(document.size(), (12000, 4000));
    let viewport = crate::pin_presentation_host::source_content_viewport(
        selected,
        (12, 9),
        None,
        crate::pin_model::PinWindowSizing::General,
    );
    let origin = viewport.window_point(Point {
        x: selected.x as f32,
        y: selected.y as f32,
    });
    assert_eq!((origin.x, origin.y), (0., 0.));
    assert_eq!(
        (
            presentation.outer_screen().left,
            presentation.outer_screen().top
        ),
        (-99, 3)
    );
    assert_eq!(viewport.image_point(origin).x, selected.x as f32);
}
#[test]
fn handoff_preparation_failure_keeps_capture_drafts_stacks_source_and_invalid_scene_unpublished() {
    let mut case = Case::new();
    let before = case.editor.document.exact_history_snapshot().unwrap();
    let ptr = Arc::as_ptr(&case.pixels);
    let scene = case.scene(&case.editor);
    let mut broken = Scene::default();
    broken.errors.push("强制采样准备失败".into());
    assert!(CaptureDocumentPayload::prepare(
        &case.source,
        &case.previews,
        &case.editor,
        &broken,
        case.selected,
        None,
        case.style
    )
    .is_err());
    let outside = Rect {
        x: 95,
        y: 71,
        width: 2,
        height: 2,
    };
    assert!(CaptureDocumentPayload::prepare(
        &case.source,
        &case.previews,
        &case.editor,
        &scene,
        outside,
        None,
        case.style
    )
    .is_err());
    case.editor.select_tool(Tool::Pen);
    case.editor.pointer_down((24, 23), 1.);
    case.editor.pointer_move((31, 32));
    assert!(case.editor.is_dragging(), "必须先有真实未提交的铅笔手势");
    assert!(CaptureDocumentPayload::prepare(
        &case.source,
        &case.previews,
        &case.editor,
        &scene,
        case.selected,
        None,
        case.style
    )
    .is_err());
    assert!(case.editor.is_dragging(), "失败不能消费未提交的铅笔手势");
    assert_eq!(
        case.editor.document.exact_history_snapshot().unwrap(),
        before
    );
    assert_eq!(Arc::as_ptr(&case.pixels), ptr);
    assert!(case.editor.document.can_redo());
}
#[test]
fn capture_source_wire_validation_is_atomic_and_old_missing_source_stays_compatible() {
    let case = Case::new();
    let wire = serde_json::to_value(case.document()).unwrap();
    for (path, value) in [
        ("version", serde_json::json!(2)),
        ("width", serde_json::json!(0)),
        ("left", serde_json::json!(i32::MAX)),
        ("content", serde_json::json!([95, 71, 2, 2])),
        ("planes", serde_json::json!([[0, 0, 97, 72]])),
        ("cursor", serde_json::json!({"rect":[0,0,2,2],"bgra":[0]})),
    ] {
        let mut bad = wire.clone();
        bad["captureSource"][path] = value;
        let bad: WireRasterDocument = serde_json::from_value(bad).unwrap();
        let mut target = Editor::new(case.source.full(), Some(case.selected));
        let crop = target.crop;
        assert!(bad.restore_into(&mut target).is_err());
        assert!(target.document.items().is_empty());
        assert!(!target.document.can_undo() && !target.document.can_redo());
        assert_eq!(target.crop, crop);
    }
    let mut null = wire;
    null["captureSource"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<WireRasterDocument>(null).is_err());
    let mut old =
        serde_json::to_value(WireRasterDocument::from_editor(&case.editor, (96, 72)).unwrap())
            .unwrap();
    old.as_object_mut().unwrap().remove("captureSource");
    let old: WireRasterDocument = serde_json::from_value(old).unwrap();
    assert!(old.capture_source().is_none());
    assert_eq!(
        case.editor_from(&old)
            .document
            .exact_history_snapshot()
            .unwrap(),
        case.editor.document.exact_history_snapshot().unwrap()
    );
}

#[test]
fn capture_native_ready_marker_refuses_placeholder_border_only_wrong_size_and_lost_preparation() {
    use crate::pin_raster_document::capture_source::content_presented;
    use std::sync::atomic::{AtomicU64, Ordering};
    let marker = AtomicU64::new(0);
    assert!(!content_presented(&marker, (52, 46)));
    assert!(!content_presented(&marker, (0, 0)));
    marker.store((1u64 << 32) | 1, Ordering::Release);
    assert!(!content_presented(&marker, (52, 46)));
    marker.store((52u64 << 32) | 46, Ordering::Release);
    assert!(content_presented(&marker, (52, 46)));
    assert!(!content_presented(&marker, (53, 46)));
    assert!(!content_presented(&marker, (52, 47)));
    marker.store(0, Ordering::Release);
    assert!(
        !content_presented(&marker, (52, 46)),
        "prepare/render失败清零后不能借旧Present序号关闭Capture"
    );
}

#[test]
fn capture_exact_history_over_budget_is_rejected_without_trimming_or_consuming_source_stacks() {
    use crate::annotation::{Annotation, Style};
    let mut case = Case::new();
    let mut text = Annotation::new(Tool::Text, Point { x: 30., y: 30. }, Style::default());
    text.update(Point { x: 55., y: 45. });
    text.text = "原始历史不得截断".repeat(6_000);
    let id = case.editor.document.insert(text);
    for suffix in ["甲", "乙", "丙"] {
        let mut changed = case.editor.document.get(id).unwrap().clone();
        changed.text.push_str(suffix);
        case.editor.document.replace(id, changed);
    }
    case.editor.document.undo();
    let before = case.editor.document.exact_history_snapshot().unwrap();
    assert!(
        WireRasterDocument::from_capture_editor(&case.editor, (96, 72), case.metadata()).is_err()
    );
    assert_eq!(
        case.editor.document.exact_history_snapshot().unwrap(),
        before
    );
    assert!(case.editor.document.can_undo() && case.editor.document.can_redo());
    // 正常capture栈即使调用manifest裁剪入口也不能被悄悄截短。
    let normal = Case::new();
    let mut document = normal.document();
    let before = document.clone();
    assert!(!document.trim_oldest_history());
    assert_eq!(document, before);
}

#[test]
fn actual_capture_selected_and_output_hosts_remain_within_visible_content_after_mask_undo_and_crop_changes(
) {
    let case = Case::new();
    let before = case.editor.document.exact_history_snapshot().unwrap();
    let wire = case.document();
    let mut installed = case.editor_from(&wire);
    let export_rect = |editor: &Editor| {
        crate::capture_pin_output::content_output_rect(
            case.source.full(),
            editor.crop,
            Some(case.selected),
            editor.document.crop_mask.as_ref(),
        )
    };
    assert_eq!(export_rect(&installed).unwrap(), case.selected);
    while installed.document.crop_mask.is_some() {
        installed.document.undo();
    }
    assert_eq!(
        installed.crop, case.editor.crop,
        "保留原始host与Undo栈，只有出口范围相交"
    );
    assert_eq!(
        export_rect(&installed).unwrap(),
        case.selected,
        "去掉mask不能扩大为选区外原截图像素"
    );
    for crop in [
        None,
        Some(case.source.full()),
        Some(crate::geometry::Rect {
            x: case.selected.x + 3,
            y: case.selected.y + 2,
            width: 9,
            height: 8,
        }),
        Some(crate::geometry::Rect {
            x: 1,
            y: 1,
            width: 8,
            height: 8,
        }),
    ] {
        installed.crop = crop;
        let expected = crop.unwrap_or(case.selected).intersection(case.selected);
        match expected {
            Some(rect) => assert_eq!(export_rect(&installed).unwrap(), rect),
            None => assert!(export_rect(&installed).is_err()),
        }
    }
    assert_eq!(
        case.editor.document.exact_history_snapshot().unwrap(),
        before
    );
}

#[test]
fn new_capture_flat_or_empty_current_requires_true_history_and_cannot_downgrade_to_legacy() {
    use crate::annotation::{Annotation, Style};
    let mut case = Case::new();
    for empty in [false, true] {
        let mut editor = Editor::new(case.source.full(), None);
        let mut annotation =
            Annotation::new(Tool::Rectangle, Point { x: 21., y: 18. }, Style::default());
        annotation.update(Point { x: 45., y: 40. });
        let id = editor.document.insert(annotation.clone());
        let mut after = annotation.clone();
        after.style.color = 0x123456;
        editor.document.replace(id, after);
        editor.document.undo();
        if empty {
            editor.document.redo();
            editor.document.clear();
            editor.document.insert(annotation);
            editor.document.undo();
        }
        assert!(editor.document.can_undo() && editor.document.can_redo());
        assert_eq!(editor.document.items().is_empty(), empty);
        case.editor = editor;
        let wire = case.document();
        let mut bad = serde_json::to_value(&wire).unwrap();
        bad.as_object_mut().unwrap().remove("history");
        let bad: WireRasterDocument = serde_json::from_value(bad).unwrap();
        let mut target = Editor::new(case.source.full(), Some(case.selected));
        let crop = target.crop;
        assert!(bad.restore_into(&mut target).is_err());
        assert!(target.document.items().is_empty());
        assert!(!target.document.can_undo() && !target.document.can_redo());
        assert_eq!(target.crop, crop);
        assert_eq!(
            case.editor_from(&wire)
                .document
                .exact_history_snapshot()
                .unwrap(),
            case.editor.document.exact_history_snapshot().unwrap()
        );
    }
}
