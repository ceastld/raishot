//! 持久标记投影模型；只构造文档/Scene，定义后尚未运行。
use super::*;
use crate::document::Document;

// 独立固定矩阵 oracle，不调用 Projection、Frame.map 或现有 quad 分割方法。
fn world(u: f64, v: f64) -> Point {
    let denominator = 1. + u * 0.5;
    Point {
        x: ((160. * u + 20. * v + 20.) / denominator) as f32,
        y: ((20. * u + 60. * v + 20.) / denominator) as f32,
    }
}
fn local_u(p: Point) -> f64 {
    let (x, y) = (f64::from(p.x), f64::from(p.y));
    (60. * (x - 20.) - 20. * (y - 20.)) / (60. * (160. - 0.5 * x) - 20. * (20. - 0.5 * y))
}
fn strip(a: f64, b: f64) -> [Point; 4] {
    [world(a, 0.), world(b, 0.), world(b, 1.), world(a, 1.)]
}
fn close(a: Point, b: Point) {
    assert!(
        (a.x - b.x).abs() < 0.001 && (a.y - b.y).abs() < 0.001,
        "{a:?} != {b:?}"
    );
}
fn close_quad(a: [Point; 4], b: [Point; 4]) {
    for (a, b) in a.into_iter().zip(b) {
        close(a, b);
    }
}
fn apply(
    ledger: &mut MarkLedger,
    document: &mut Document,
    mark: TextMark,
    quads: &[[Point; 4]],
    color: u32,
) {
    let edit = ledger
        .prepare(
            document,
            mark,
            quads,
            Style {
                color,
                ..Style::default()
            },
        )
        .unwrap();
    let expected = edit.annotations(document);
    assert!(ledger.commit(document, edit));
    assert_eq!(
        document
            .items()
            .iter()
            .map(|object| object.annotation.clone())
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn perspective_highlight_preserves_four_world_vertices_in_existing_polygon_object() {
    let quad = strip(0., 1.);
    let item = annotations(TextMark::Highlight, &[quad], Style::default())
        .unwrap()
        .remove(0);
    assert_eq!(item.tool, Tool::Polyline);
    assert!(item.style.filled);
    assert!(!item.style.polyline_smooth && !item.style.dashed);
    assert!(item.style.opacity <= 0.35);
    assert_eq!(item.rotation, 0.);
    assert!(item.valid());
    close_quad(item.points.clone().try_into().unwrap(), quad);
}

#[test]
fn projected_strokes_follow_reading_baseline_and_center() {
    for (mark, v) in [(TextMark::Underline, 1.), (TextMark::Strike, 0.55)] {
        let item = annotations(mark, &[strip(0.2, 0.8)], Style::default())
            .unwrap()
            .remove(0);
        assert_eq!(item.tool, Tool::Pen);
        assert_eq!(item.points.len(), 2);
        close(item.points[0], world(0.2, v));
        close(item.points[1], world(0.8, v));
    }
}

#[test]
fn projected_wave_uses_exact_baseline_endpoints_and_monotone_reading_path() {
    let item = annotations(TextMark::Wave, &[strip(0., 1.)], Style::default())
        .unwrap()
        .remove(0);
    close(item.points[0], world(0., 1.));
    close(*item.points.last().unwrap(), world(1., 1.));
    assert!(item.points.len() > 8 && item.points.len() <= MAX_POINTS);
    assert!(item
        .points
        .windows(2)
        .all(|pair| local_u(pair[1]) > local_u(pair[0])));
    assert!(!item.style.pencil_smooth && !item.style.dashed);
}

#[test]
fn vertical_up_down_and_half_turn_marks_preserve_reading_edges() {
    for turn in [1, 2, 3] {
        let rotate = |p: Point| match turn {
            1 => Point {
                x: 300. - p.y,
                y: p.x,
            },
            2 => Point {
                x: 300. - p.x,
                y: 300. - p.y,
            },
            _ => Point {
                x: p.y,
                y: 300. - p.x,
            },
        };
        for mark in [TextMark::Underline, TextMark::Wave] {
            let item = annotations(mark, &[strip(0., 1.).map(rotate)], Style::default())
                .unwrap()
                .remove(0);
            close(item.points[0], rotate(world(0., 1.)));
            close(*item.points.last().unwrap(), rotate(world(1., 1.)));
        }
    }
}

#[test]
fn projected_roi_translation_keeps_all_four_mark_types_in_same_space() {
    let offset = |p: Point| Point {
        x: p.x - 240.,
        y: p.y + 310.,
    };
    for mark in [
        TextMark::Highlight,
        TextMark::Underline,
        TextMark::Wave,
        TextMark::Strike,
    ] {
        let original = annotations(mark, &[strip(0., 1.)], Style::default())
            .unwrap()
            .remove(0);
        let translated = annotations(mark, &[strip(0., 1.).map(offset)], Style::default())
            .unwrap()
            .remove(0);
        assert_eq!(original.tool, translated.tool);
        for (a, b) in original.points.into_iter().zip(translated.points) {
            close(offset(a), b);
        }
    }
}

#[test]
fn rectangle_path_still_uses_existing_highlighter_rect_object() {
    let q = [
        Point { x: 100., y: 50. },
        Point { x: 100., y: 150. },
        Point { x: 80., y: 150. },
        Point { x: 80., y: 50. },
    ];
    let item = annotations(TextMark::Highlight, &[q], Style::default())
        .unwrap()
        .remove(0);
    assert_eq!(item.tool, Tool::HighlighterRect);
    assert_eq!(item.points.len(), 2);
    assert_eq!(item.rotation, 90.);
}

#[test]
fn small_real_perspective_is_preserved_instead_of_using_rectangle_tolerance() {
    let q = [
        Point { x: 10., y: 10. },
        Point { x: 110., y: 10. },
        Point { x: 109.9, y: 30. },
        Point { x: 10., y: 30. },
    ];
    let item = annotations(TextMark::Highlight, &[q], Style::default())
        .unwrap()
        .remove(0);
    assert_eq!(item.tool, Tool::Polyline);
    close_quad(item.points.try_into().unwrap(), q);
}

#[test]
fn tiny_projected_highlight_rejects_whole_batch_before_invisible_polygon_enters_scene() {
    let tiny = [
        Point { x: 10., y: 10. },
        Point { x: 10.5, y: 10. },
        Point { x: 10.4, y: 10.5 },
        Point { x: 10., y: 10.5 },
    ];
    assert!(Frame::from_quad(&tiny).is_ok());
    assert!(annotations(
        TextMark::Highlight,
        &[strip(0., 1.), tiny],
        Style::default()
    )
    .is_err());
    let document = Document::default();
    let ledger = MarkLedger::default();
    assert!(ledger
        .prepare(
            &document,
            TextMark::Highlight,
            &[strip(0., 1.), tiny],
            Style::default()
        )
        .is_err());
    assert!(document.items().is_empty() && !document.can_undo());
}

#[test]
fn wave_amplitude_cannot_cross_a_nearby_projective_pole() {
    // 此有效凸裁行的矩阵分母为 1 - 0.99999v；波形扩展 v 会跨过极点。
    let q = [
        Point { x: 0., y: 0. },
        Point { x: 0.25, y: 0. },
        Point { x: 25000., y: 100. },
        Point { x: 0., y: 100. },
    ];
    assert!(Frame::from_quad(&q).is_ok());
    assert!(annotations(TextMark::Underline, &[q], Style::default()).is_ok());
    assert!(annotations(TextMark::Wave, &[q], Style::default()).is_err());
}

#[test]
fn only_matching_projective_reading_strips_share_ledger_intervals() {
    let frame = Frame::from_quad(&strip(0.2, 0.4)).unwrap();
    let matching = Frame::from_quad(&strip(0., 1.)).unwrap();
    let interval = frame.interval(&matching).unwrap();
    close_quad(frame.quad(interval), strip(0., 1.));
    let mut different = strip(0.3, 0.7);
    different[2].y += 3.;
    different[3].y += 3.;
    assert!(frame
        .interval(&Frame::from_quad(&different).unwrap())
        .is_none());
    let reversed = strip(0.3, 0.7);
    let reversed = [reversed[2], reversed[3], reversed[0], reversed[1]];
    assert!(frame
        .interval(&Frame::from_quad(&reversed).unwrap())
        .is_none());
}

#[test]
fn partial_toggle_preserves_projected_survivors_for_every_mark_type() {
    for mark in [
        TextMark::Highlight,
        TextMark::Underline,
        TextMark::Wave,
        TextMark::Strike,
    ] {
        let mut ledger = MarkLedger::default();
        let mut document = Document::default();
        apply(&mut ledger, &mut document, mark, &[strip(0., 1.)], 0xff1122);
        let original = document.items().to_vec();
        apply(
            &mut ledger,
            &mut document,
            mark,
            &[strip(0.3, 0.7)],
            0xff1122,
        );
        assert_eq!(document.items().len(), 2, "{mark:?}");
        let live: Vec<_> = ledger
            .records
            .iter()
            .filter(|record| document.get(record.id) == Some(&record.mark.annotation))
            .collect();
        assert_eq!(live.len(), 2);
        let mut fragments: Vec<_> = live.iter().map(|record| record.mark.quad).collect();
        fragments.sort_by(|a, b| local_u(a[0]).total_cmp(&local_u(b[0])));
        close_quad(fragments[0], strip(0., 0.3));
        close_quad(fragments[1], strip(0.7, 1.));
        document.undo();
        assert_eq!(document.items(), original);
        document.redo();
        assert_eq!(document.items().len(), 2);
        assert!(ledger.matches_underlay(&document));
    }
}

#[test]
fn projected_recolor_replaces_selected_coverage_in_one_undo_batch() {
    let mut ledger = MarkLedger::default();
    let mut document = Document::default();
    apply(
        &mut ledger,
        &mut document,
        TextMark::Highlight,
        &[strip(0., 1.)],
        0xff1122,
    );
    let before = document.items().to_vec();
    apply(
        &mut ledger,
        &mut document,
        TextMark::Highlight,
        &[strip(0.3, 0.7)],
        0x1155ff,
    );
    assert_eq!(document.items().len(), 3);
    assert_eq!(
        document
            .items()
            .iter()
            .filter(|item| item.annotation.style.color == 0x1155ff)
            .count(),
        1
    );
    document.undo();
    assert_eq!(document.items(), before);
    document.redo();
    apply(
        &mut ledger,
        &mut document,
        TextMark::Highlight,
        &[strip(0.3, 0.7)],
        0x1155ff,
    );
    assert_eq!(document.items().len(), 2);
}

#[test]
fn projected_wave_clipping_keeps_original_world_points_and_straight_segment_intersections() {
    let mut ledger = MarkLedger::default();
    let mut document = Document::default();
    apply(
        &mut ledger,
        &mut document,
        TextMark::Wave,
        &[strip(0., 1.)],
        0xff1122,
    );
    let original = document.items()[0].annotation.points.clone();
    apply(
        &mut ledger,
        &mut document,
        TextMark::Wave,
        &[strip(0.31, 0.69)],
        0xff1122,
    );
    let kept: Vec<_> = document
        .items()
        .iter()
        .flat_map(|item| item.annotation.points.iter().copied())
        .collect();
    for p in &original {
        if local_u(*p) < 0.31 - 0.00001 || local_u(*p) > 0.69 + 0.00001 {
            assert!(kept.contains(p));
        }
    }
    for p in kept.iter().filter(|p| !original.contains(p)) {
        let u = local_u(*p);
        assert!((u - 0.31).abs() < 0.00001 || (u - 0.69).abs() < 0.00001);
        let segment = original
            .windows(2)
            .find(|s| local_u(s[0]) <= u + 0.00001 && local_u(s[1]) >= u - 0.00001)
            .unwrap();
        let cross = (p.x - segment[0].x) * (segment[1].y - segment[0].y)
            - (p.y - segment[0].y) * (segment[1].x - segment[0].x);
        assert!(cross.abs() < 0.001);
    }
}

#[test]
fn unrelated_projected_row_never_replaces_owned_marks() {
    let mut ledger = MarkLedger::default();
    let mut document = Document::default();
    apply(
        &mut ledger,
        &mut document,
        TextMark::Underline,
        &[strip(0., 1.)],
        0xff1122,
    );
    let original = document.items()[0].clone();
    let mut other = strip(0.2, 0.8);
    other[2].y += 3.;
    other[3].y += 3.;
    apply(
        &mut ledger,
        &mut document,
        TextMark::Underline,
        &[other],
        0x1155ff,
    );
    assert_eq!(document.items().len(), 2);
    assert_eq!(document.items()[0], original);
}

#[test]
fn malformed_projection_rejects_whole_batch_without_document_side_effects() {
    let valid = strip(0., 1.);
    let mut concave = valid;
    concave[2] = Point { x: 40., y: 30. };
    let mut nonfinite = valid;
    nonfinite[1].x = f32::NAN;
    let mut reversed = valid;
    reversed.swap(1, 3);
    let mut huge = valid;
    huge[1].x = MAX_COORDINATE + 1.;
    let document = Document::default();
    let ledger = MarkLedger::default();
    for invalid in [concave, nonfinite, reversed, huge] {
        assert!(ledger
            .prepare(
                &document,
                TextMark::Highlight,
                &[valid, invalid],
                Style::default()
            )
            .is_err());
        assert!(document.items().is_empty());
        assert!(!document.can_undo());
    }
}

#[test]
fn projected_highlight_uses_shared_scene_for_full_and_cropped_export() {
    use crate::{
        geometry::Rect,
        scene::{PixelPlane, Scene, SourcePixels},
    };
    let canvas = Rect::full(180, 110);
    let bytes = [255, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
    let source = SourcePixels {
        fill: None,
        canvas,
        planes: vec![PixelPlane {
            rect: canvas,
            bgra: &bytes,
        }],
        overlay: None,
    };
    let marks = annotations(
        TextMark::Highlight,
        &[strip(0., 1.)],
        Style {
            color: 0xffff00,
            ..Style::default()
        },
    )
    .unwrap();
    let scene = Scene::prepare(&marks, &source);
    assert!(scene.errors.is_empty());
    let mut full = source.read(canvas);
    scene.apply(&mut full, canvas);
    let crop = Rect {
        x: 25,
        y: 25,
        width: 80,
        height: 55,
    };
    let mut clipped = source.read(crop);
    scene.apply(&mut clipped, crop);
    for y in 0..crop.height {
        let a = (((crop.y + y) * canvas.width + crop.x) * 4) as usize;
        let b = (y * crop.width * 4) as usize;
        assert_eq!(
            &clipped.bgra[b..b + crop.width as usize * 4],
            &full.bgra[a..a + crop.width as usize * 4]
        );
    }
    let center = world(0.5, 0.5);
    let center = ((center.y.round() as u32 * canvas.width + center.x.round() as u32) * 4) as usize;
    assert_ne!(&full.bgra[center..center + 4], &[255, 255, 255, 255]);
    assert_eq!(&full.bgra[..4], &[255, 255, 255, 255]);
}
