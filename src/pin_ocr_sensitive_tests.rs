//! Pin 宿主直接模型：真实坐标映射、安装/主题 ABA、创建空画布的窄授权。
//! 这些模型不证明 GPUI 真实 paint、焦点、鼠标或实际输出。
use super::*;
use crate::{
    recognition::{
        HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
    },
    viewport::Viewport,
};

fn recognized() -> Arc<RecognizedText> {
    let text = "a@example.org";
    let rect = Rect {
        x: 10,
        y: 12,
        width: 120,
        height: 18,
    };
    let geometry = Some(TextGeometry {
        quad: [
            Point { x: 10., y: 12. },
            Point { x: 130., y: 12. },
            Point { x: 130., y: 30. },
            Point { x: 10., y: 30. },
        ],
    });
    Arc::new(RecognizedText {
        text: text.into(),
        regions: Vec::new(),
        words: vec![RecognizedWord {
            text: text.into(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry,
        }],
        lines: vec![RecognizedLine {
            text: text.into(),
            rect: Some(rect),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry,
            graphemes: None,
        }],
    })
}

fn source() -> Source {
    Source {
        stamp: (17, None, None, None, true),
        generation: 9,
        body: recognized(),
        roi: Rect {
            x: 100,
            y: 200,
            width: 200,
            height: 80,
        },
        image_size: (640, 480),
        processing: 4,
        theme: Arc::new(()),
    }
}

fn stamp(source: Source) -> Stamp {
    Stamp {
        source,
        preview: Some(Arc::new(())),
        viewport: Viewport {
            zoom: 0.75,
            offset: Point { x: -24., y: 10. },
        },
        clip: Region {
            x: 32.,
            y: 0.,
            width: 500.,
            height: 480.,
        },
        dpi: 1.25,
        host: (320., 240.),
    }
}

#[test]
fn pin_sensitive_regions_translate_actual_output_roi_once_before_projection() {
    let source = source();
    let local = [Region {
        x: 10.,
        y: 12.,
        width: 120.,
        height: 18.,
    }];
    let mapped = document_regions(&local, source.roi, source.image_size).unwrap();
    assert_eq!(
        mapped,
        vec![Region {
            x: 110.,
            y: 212.,
            width: 120.,
            height: 18.
        }]
    );
    // 视口 pan/zoom/DPI 在 paint 才使用，绝不能再写入对象区域。
    let stamp = stamp(source);
    let screen_x = (stamp.viewport.offset.x + mapped[0].x * stamp.viewport.zoom) / stamp.dpi;
    let screen_y = (stamp.viewport.offset.y + mapped[0].y * stamp.viewport.zoom) / stamp.dpi;
    assert_eq!((screen_x, screen_y), (46.8, 135.2));
    assert_eq!(
        document_regions(&[], stamp.source.roi, stamp.source.image_size),
        Some(Vec::new())
    );
}

#[test]
fn pin_sensitive_one_invalid_region_rejects_batch_instead_of_clipping() {
    let source = source();
    let good = Region {
        x: 10.,
        y: 12.,
        width: 120.,
        height: 18.,
    };
    for bad in [
        Region { x: -0.01, ..good },
        Region {
            y: f32::NAN,
            ..good
        },
        Region {
            width: f32::INFINITY,
            ..good
        },
        Region { width: 0., ..good },
        Region {
            x: 199.,
            width: 2.,
            ..good
        },
        Region {
            y: 79.,
            height: 2.,
            ..good
        },
    ] {
        assert!(document_regions(&[good, bad], source.roi, source.image_size).is_none());
    }
    let partial_roi = Rect {
        x: 630,
        y: 200,
        width: 20,
        height: 80,
    };
    assert!(document_regions(&[good], partial_roi, source.image_size).is_none());
}

#[test]
fn pin_sensitive_same_body_bytes_theme_and_document_aba_keep_old_stamp_stale() {
    let original = source();
    assert!(original == original.clone());
    let changes: [fn(&mut Source); 9] = [
        |s| s.body = recognized(),  // A→B→A 的新安装 bytes 一致，Arc 不一致。
        |s| s.theme = Arc::new(()), // 模式/颜色回到 A，change identity 仍不同。
        |s| s.generation += 1,
        |s| s.processing += 1,
        |s| s.stamp.0 += 1,
        |s| s.stamp.2 = Some(11),
        |s| s.stamp.3 = Some(Rect::full(320, 240)),
        |s| s.roi.x += 1,
        |s| s.image_size.0 += 1,
    ];
    for change in changes {
        let mut current = original.clone();
        change(&mut current);
        assert!(original != current);
    }
    let installed = Snapshot::from_recognized(&original.body).unwrap();
    let mut preview = Preview::new(original.clone(), &installed);
    let identity = preview.identity().clone();
    assert!(preview.set_selected(0, true));
    assert!(preview.set_selected(0, false));
    assert!(!preview.current(&original, &original.body.text, &identity));
    let before = stamp(original);
    let mut after = before.clone();
    after.preview = Some(preview.identity().clone());
    assert!(!before.matches(&after));
    after = before.clone();
    after.viewport.offset.x += 1.;
    assert!(!before.matches(&after));
    after = before.clone();
    after.clip.width -= 1.;
    assert!(!before.matches(&after));
    after = before.clone();
    after.dpi = 2.;
    assert!(!before.matches(&after));
}

#[test]
fn pin_sensitive_new_canvas_authorization_only_accepts_own_empty_source_transition() {
    let before = source();
    let mut after = before.clone();
    after.stamp.1 = Some(gpui::EntityId::from(31u64));
    after.stamp.2 = Some(0);
    after.stamp.3 = Some(Rect::full(before.image_size.0, before.image_size.1));
    assert!(created_source_matches(&before, &after));
    for change in [
        |s: &mut Source| s.stamp.2 = Some(1),
        |s: &mut Source| s.stamp.3 = Some(Rect::full(320, 240)),
        |s: &mut Source| s.body = recognized(),
        |s: &mut Source| s.theme = Arc::new(()),
        |s: &mut Source| s.processing += 1,
    ] {
        let mut wrong = after.clone();
        change(&mut wrong);
        assert!(!created_source_matches(&before, &wrong));
    }
    assert!(!created_source_matches(&after, &after));
}

#[test]
fn pin_sensitive_release_needs_new_actual_paint_and_consumes_original_stamp_once() {
    let down = stamp(source());
    let mut intent = find_ui::FindClickIntent::default();
    intent.arm(Arc::new(()), down.clone());
    let held_epoch = intent.begin_render();
    assert!(intent.painted(&held_epoch).is_none());
    assert!(intent.queue());
    let release_epoch = intent.begin_render();
    assert!(intent.painted(&held_epoch).is_none());
    let ticket = intent.painted(&release_epoch).unwrap();
    assert!(intent.painted(&release_epoch).is_none());
    let consumed = intent.take(&ticket).unwrap();
    assert!(consumed.matches(&down));
    assert!(intent.take(&ticket).is_none());
    intent.arm(Arc::new(()), down.clone());
    let token = intent.token().cloned().unwrap();
    intent.release_outside(&token);
    assert!(!intent.queue());
    intent.arm(Arc::new(()), down);
    assert!(intent.queue());
    let epoch = intent.begin_render();
    let ticket = intent.painted(&epoch).unwrap();
    intent.cancel();
    assert!(intent.take(&ticket).is_none());
}
