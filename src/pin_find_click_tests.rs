//! 新 Pin 点击宿主的组合模型定义；不证明真实 GPUI paint、焦点或元素复用。
use super::*;
use crate::{
    annotation::{Bounds as ImageBounds, Point},
    geometry::Rect,
    pin_text_selection::{
        find::{OcrFind, OcrFindDirection, OcrFindError},
        SelectionText,
    },
    recognition::{
        HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
    },
    viewport::Viewport,
};

fn access() -> OcrFindAccess {
    OcrFindAccess {
        active: true,
        idle: true,
        source_current: true,
        composing: false,
    }
}

fn source() -> PinFindPaintSource {
    PinFindPaintSource {
        source: (11, None, Some(3), Some(Rect::full(640, 480)), false),
        generation: 7,
        clip: ImageBounds {
            x: 0.,
            y: 0.,
            width: 640.,
            height: 480.,
        },
        viewport: Viewport {
            zoom: 1.5,
            offset: Point { x: -24., y: 10. },
        },
        dpi: 1.25,
        image_size: (640, 480),
    }
}

fn document(source: PinFindPaintSource, raw: &str) -> OcrFindDocument<TextSelectionStamp> {
    OcrFindDocument::new(
        OcrFindStamp {
            source: source.source,
            result_generation: source.generation,
            content_revision: source.source.2.unwrap_or(0),
        },
        Arc::from(raw),
    )
    .unwrap()
}

fn selection() -> SelectionText {
    let raw = "中 中";
    let rect = Rect {
        x: 80,
        y: 120,
        width: 60,
        height: 20,
    };
    let geometry = Some(TextGeometry {
        quad: [
            Point { x: 80., y: 120. },
            Point { x: 140., y: 120. },
            Point { x: 140., y: 140. },
            Point { x: 80., y: 140. },
        ],
    });
    SelectionText::from_recognized(&RecognizedText {
        text: raw.into(),
        regions: Vec::new(),
        lines: vec![RecognizedLine {
            graphemes: None,
            text: raw.into(),
            rect: Some(rect),
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry,
        }],
        words: vec![RecognizedWord {
            text: raw.into(),
            rect,
            line: 0,
            horizontal_direction: HorizontalDirection::LeftToRight,
            geometry,
        }],
    })
}

struct Fixture {
    source: PinFindPaintSource,
    document: OcrFindDocument<TextSelectionStamp>,
    model: OcrFind<TextSelectionStamp>,
    text: SelectionText,
}

impl Fixture {
    fn new() -> Self {
        let source = source();
        let document = document(source, "中 中");
        let mut model = OcrFind::default();
        model.update(&document, "中", access()).unwrap();
        Self {
            source,
            document,
            model,
            text: selection(),
        }
    }

    fn down_target(&self) -> PinFindClickTarget {
        let selected = Some(0..3);
        PinFindClickTarget {
            source: self.source,
            document: self.document.clone(),
            keyboard_selection: self.text.keyboard_selection_from_range(0..3),
            selected,
            current: self.model.current_ticket(&self.document, access()),
            all: self.model.all_ticket(&self.document, access()),
        }
    }
}

#[test]
fn down_tickets_reject_hit_query_and_document_aba_after_release() {
    #[derive(Clone, Copy, Debug)]
    enum Change {
        Hit,
        Query,
        Document,
    }

    for change in [Change::Hit, Change::Query, Change::Document] {
        let mut fixture = Fixture::new();
        let mut intent = FindClickIntent::default();
        intent.arm(Arc::new(()), fixture.down_target());
        match change {
            Change::Hit => {
                fixture
                    .model
                    .cycle(&fixture.document, OcrFindDirection::Next, access())
                    .unwrap();
                fixture
                    .model
                    .cycle(&fixture.document, OcrFindDirection::Previous, access())
                    .unwrap();
            }
            Change::Query => {
                fixture
                    .model
                    .update(&fixture.document, " ", access())
                    .unwrap();
                fixture
                    .model
                    .update(&fixture.document, "中", access())
                    .unwrap();
            }
            Change::Document => {
                let other = document(fixture.source, "文 文");
                fixture.model.update(&other, "文", access()).unwrap();
                // 新正文的 stamp 数值和 bytes 均回到 A，快照 Arc 身份仍是新的。
                fixture.document = document(fixture.source, "中 中");
                fixture
                    .model
                    .update(&fixture.document, "中", access())
                    .unwrap();
            }
        }

        // 刻意保留排队以核执行门；真实 UI 可在 Up 前更早取消。
        // Up 只移动原载荷，不能重新取得当前票据替换 Down 的票据。
        assert!(intent.queue());
        let epoch = intent.begin_render();
        let released = intent.token().cloned();
        assert!(release_matches_render(released.as_ref(), intent.token()));
        let paint = intent.painted(&epoch).unwrap();
        let down = intent.take(&paint).unwrap();
        assert!(down.matches_host(fixture.source, &down.selected, &down.keyboard_selection));
        assert_eq!(down.document.text(), fixture.document.text());
        assert_eq!(down.document.stamp(), fixture.document.stamp());
        assert_eq!(
            fixture
                .model
                .summary(&fixture.document, access())
                .unwrap()
                .current,
            Some(1)
        );
        assert_eq!(
            fixture.model.resolve_ticket(
                down.current.as_ref().unwrap(),
                &fixture.document,
                access(),
            ),
            Err(OcrFindError::StaleIntent),
            "{change:?} 不得复活 Down current 票据"
        );
        assert_eq!(
            fixture.model.resolve_all_ticket(
                down.all.as_ref().unwrap(),
                &fixture.document,
                access(),
            ),
            Err(OcrFindError::StaleIntent),
            "{change:?} 不得复活 Down all 票据"
        );

        // 对照：当前模型合法，失败来自原 Down 票据，而非查询为空或访问拒绝。
        let fresh = fixture
            .model
            .current_ticket(&fixture.document, access())
            .unwrap();
        assert_eq!(
            fixture
                .model
                .resolve_ticket(&fresh, &fixture.document, access()),
            Ok(0..3)
        );
        let fresh_all = fixture
            .model
            .all_ticket(&fixture.document, access())
            .unwrap();
        assert_eq!(
            fixture
                .model
                .resolve_all_ticket(&fresh_all, &fixture.document, access())
                .unwrap(),
            &[0..3, 4..7]
        );
    }
}

#[test]
fn down_target_requires_every_actual_host_snapshot_field() {
    let fixture = Fixture::new();
    let down = fixture.down_target();
    let base = fixture.source;
    assert!(down.keyboard_selection.is_some());
    assert!(down.matches_host(base, &down.selected, &down.keyboard_selection));

    let mut changed = Vec::new();
    let mut value = base;
    value.source.0 += 1;
    changed.push(("source", value));
    let mut value = base;
    value.source.2 = Some(4);
    changed.push(("revision", value));
    let mut value = base;
    value.source.3 = Some(Rect::full(639, 480));
    changed.push(("source region", value));
    let mut value = base;
    value.source.4 = true;
    changed.push(("source mode", value));
    let mut value = base;
    value.generation += 1;
    changed.push(("generation", value));
    let mut value = base;
    value.clip.x = 1.;
    changed.push(("clip", value));
    let mut value = base;
    value.viewport.zoom = 1.6;
    changed.push(("zoom", value));
    let mut value = base;
    value.viewport.offset.x = -23.;
    changed.push(("physical offset", value));
    let mut value = base;
    value.dpi = 1.5;
    changed.push(("DPI", value));
    let mut value = base;
    value.image_size.0 = 639;
    changed.push(("image size", value));
    for (field, value) in changed {
        assert!(
            !down.matches_host(value, &down.selected, &down.keyboard_selection),
            "宿主 {field} 改变后不能执行原点击"
        );
    }
    assert!(!down.matches_host(base, &Some(4..7), &down.keyboard_selection));
    assert!(!down.matches_host(base, &None, &down.keyboard_selection));
    let keyboard = fixture.text.keyboard_selection_from_range(4..7);
    assert!(keyboard.is_some());
    assert!(!down.matches_host(base, &down.selected, &keyboard));
    assert!(!down.matches_host(base, &down.selected, &None));
}

#[test]
fn release_requires_its_own_new_render_and_one_current_paint() {
    let fixture = Fixture::new();
    let mut intent = FindClickIntent::default();
    intent.arm(Arc::new(()), fixture.down_target());
    let held_epoch = intent.begin_render();
    let held_release: Option<Arc<()>> = None;
    assert!(intent.queue());
    assert!(intent.epoch_is_current(&held_epoch));
    // epoch 仍是最新，但该 render 开始时尚未 Up；宿主不能调用 painted。
    assert!(!release_matches_render(
        held_release.as_ref(),
        intent.token()
    ));
    assert!(!release_matches_render(None, None));

    let first_epoch = intent.begin_render();
    let first_release = intent.token().cloned().unwrap();
    assert!(release_matches_render(Some(&first_release), intent.token()));
    let down = intent.payload().unwrap();
    assert!(down.matches_host(fixture.source, &down.selected, &down.keyboard_selection));
    let old_paint = intent.painted(&first_epoch).unwrap();

    let current_epoch = intent.begin_render();
    assert!(intent.take(&old_paint).is_none());
    assert!(intent.painted(&first_epoch).is_none());
    assert!(release_matches_render(Some(&first_release), intent.token()));
    let current_paint = intent.painted(&current_epoch).unwrap();
    let repeated_callback = current_paint.clone();
    assert!(intent.painted(&current_epoch).is_none());
    let down = intent.take(&current_paint).unwrap();
    assert_eq!(
        fixture
            .model
            .resolve_ticket(down.current.as_ref().unwrap(), &fixture.document, access()),
        Ok(0..3)
    );
    assert_eq!(
        fixture
            .model
            .resolve_all_ticket(down.all.as_ref().unwrap(), &fixture.document, access())
            .unwrap(),
        &[0..3, 4..7]
    );
    assert!(intent.take(&repeated_callback).is_none());

    let successor = Arc::new(());
    intent.arm(successor.clone(), fixture.down_target());
    assert!(intent.queue());
    // 旧 render 仍拥有最新 epoch，也不能替同值 Arc<()> 的后继 queue 出票。
    assert!(intent.epoch_is_current(&current_epoch));
    assert!(!release_matches_render(
        Some(&first_release),
        intent.token()
    ));
    assert!(intent.take(&repeated_callback).is_none());
    assert!(Arc::ptr_eq(intent.token().unwrap(), &successor));
    assert!(intent.payload().is_some());

    let successor_epoch = intent.begin_render();
    let successor_release = intent.token().cloned();
    assert!(release_matches_render(
        successor_release.as_ref(),
        intent.token()
    ));
    let successor_paint = intent.painted(&successor_epoch).unwrap();
    assert!(intent.take(&successor_paint).is_some());
    assert!(intent.take(&successor_paint).is_none());
}
