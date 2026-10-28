//! 调用生产的懒任务桥接和实际 TaskSlot/RecognitionSession/LocalRetry；不启动 GUI。
use super::*;
use crate::{
    annotation::Point,
    capture_view::{
        local_retry::{Binding, Completion, LocalRetry, SourceFrame},
        ocr_task::Receipt,
        text_result::Origin,
    },
    geometry::ScreenRect,
    recognition::{
        HorizontalDirection, OcrProvider, RecognizedLine, RecognizedText, RecognizedWord,
    },
    scene::{PixelPlane, SourceIdentity, SourcePixels},
};
use std::{sync::Arc, time::Duration};

static PIXELS: [u8; 200 * 120 * 4] = [0; 200 * 120 * 4];

fn target() -> OcrTarget {
    let full = Rect::full(200, 120);
    OcrTarget {
        canvas: ScreenRect {
            left: -200,
            top: -120,
            width: 200,
            height: 120,
        },
        selected: Rect {
            x: 20,
            y: 30,
            width: 100,
            height: 60,
        },
        revisions: (4, 7),
        stamp: (11, 13),
        identity: SourceIdentity::of(&SourcePixels {
            fill: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &PIXELS,
            }],
            overlay: None,
        }),
    }
}

#[test]
fn initial_animation_uses_real_owner_and_completion_removes_it() {
    let target = target();
    let mut session = RecognitionSession::default();
    assert!(session_task(&session, Some(&target), 0).is_none());
    let request = session.begin_initial(target.clone(), 0);
    assert_eq!(
        session_task(&session, Some(&target), 0),
        Some((
            TaskKey {
                kind: TaskKind::Initial,
                id: request.id
            },
            target.selected
        ))
    );
    let token = session.initial_token(request.id).unwrap();
    assert!(session.finish_initial(request.id).is_some());
    assert!(token.is_cancelled());
    assert!(session_task(&session, Some(&target), 0).is_none());
}

#[test]
fn replaced_initial_old_receipt_does_not_reset_new_animation_phase() {
    let target = target();
    let mut session = RecognitionSession::default();
    let first = session.begin_initial(target.clone(), 0);
    let second = session.begin_initial(target.clone(), 0);
    let now = Instant::now();
    let mut clock = ProgressClock::default();
    let key = session_task(&session, Some(&target), 0).unwrap().0;
    assert_eq!(clock.observe(Some(key), now), Some(Duration::ZERO));
    assert!(session.finish_initial(first.id).is_none());
    let still_current = session_task(&session, Some(&target), 0).unwrap().0;
    assert_eq!(still_current.id, second.id);
    assert_eq!(
        clock.observe(Some(still_current), now + Duration::from_millis(42)),
        Some(Duration::from_millis(42))
    );
}

#[test]
fn committed_scene_source_roi_and_body_changes_cannot_paint_old_initial() {
    let target = target();
    let mut session = RecognitionSession::default();
    session.begin_initial(target.clone(), 0);
    for mutation in 0..4 {
        let mut changed = target.clone();
        match mutation {
            0 => changed.selected.x += 1,
            1 => changed.revisions.0 += 1,
            2 => changed.stamp.0 += 1,
            _ => changed.canvas.left += 1,
        }
        assert!(session_task(&session, Some(&changed), 0).is_none());
    }
    assert!(session_task(&session, Some(&target), 1).is_none());
    assert!(session_task(&session, None, 0).is_none());
    session.invalidate_requests();
    assert!(session_task(&session, Some(&target), 0).is_none());
}

#[test]
fn retry_animation_does_not_require_processing_and_retains_old_result_origin() {
    let target = target();
    let mut session = RecognitionSession::default();
    session.show(Origin::Ocr(target.clone()));
    let request = session.begin_retry(target.clone(), 3).unwrap();
    assert_eq!(
        session_task(&session, Some(&target), 3).unwrap().0.kind,
        TaskKind::Retry
    );
    assert!(session_task(&session, Some(&target), 4).is_none());
    assert!(session.finish_retry(request.id).is_some());
    assert!(session_task(&session, Some(&target), 3).is_none());
    assert!(session.ocr_target() == Some(&target));
}

#[test]
fn foreground_completion_failure_cancel_and_stale_receipt_use_existing_slot() {
    let target = target();
    for receipt in [
        Receipt::Completed(()),
        Receipt::Failed("实际失败".into()),
        Receipt::Cancelled,
    ] {
        let mut slot = TaskSlot::default();
        let request = slot.start(InputTarget::Content(target.clone()));
        assert_eq!(
            foreground_task(&slot, Some(&target)).unwrap().0.id,
            request.id
        );
        assert!(slot.finish_receipt(request.id, receipt).is_some());
        assert!(foreground_task(&slot, Some(&target)).is_none());
    }
    let mut slot = TaskSlot::default();
    let old = slot.start(InputTarget::Content(target.clone()));
    let new = slot.start(InputTarget::Content(target.clone()));
    assert!(slot
        .finish_receipt(old.id, Receipt::Completed(()))
        .is_none());
    assert_eq!(foreground_task(&slot, Some(&target)).unwrap().0.id, new.id);
    slot.cancel("显式取消");
    assert!(foreground_task(&slot, Some(&target)).is_none());
}

#[test]
fn frozen_foreground_keeps_its_source_contract_while_content_ocr_rejects_edit() {
    let target = target();
    let mut changed = target.clone();
    changed.revisions.0 += 1;
    changed.stamp.0 += 1;
    let mut slot = TaskSlot::default();
    slot.start(InputTarget::Content(target.clone()));
    assert!(foreground_task(&slot, Some(&changed)).is_none());
    slot.start(InputTarget::Frozen(target));
    assert!(foreground_task(&slot, Some(&changed)).is_some());
    changed.selected.x += 1;
    assert!(foreground_task(&slot, Some(&changed)).is_none());
}

fn local_binding() -> Binding<u64> {
    let rect = Rect {
        x: 20,
        y: 30,
        width: 100,
        height: 20,
    };
    let geometry = crate::recognition::TextGeometry {
        quad: [
            Point { x: 20., y: 30. },
            Point { x: 120., y: 30. },
            Point { x: 120., y: 50. },
            Point { x: 20., y: 50. },
        ],
    };
    let body_identity = Arc::new(());
    Binding {
        source: 41,
        source_identity: Arc::new(()),
        result: Arc::new(RecognizedText {
            text: "first".into(),
            regions: vec![rect],
            words: vec![RecognizedWord {
                text: "first".into(),
                rect,
                line: 0,
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
            }],
            lines: vec![RecognizedLine {
                text: "first".into(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: Some(geometry),
                graphemes: None,
            }],
        }),
        result_generation: 7,
        installed_body_identity: body_identity.clone(),
        body_identity,
        installed_body_revision: 0,
        body_revision: 0,
        frame: SourceFrame {
            width: 256,
            height: 256,
            origin: (-1920, -100),
        },
        provider: OcrProvider::Ppocr,
    }
}

#[test]
fn local_visual_getter_uses_actual_intent_and_old_receipt_cannot_release_new_id() {
    let binding = local_binding();
    let mut local = LocalRetry::default();
    assert!(local.pending_visual().is_none());
    let first = local
        .begin(&binding, &binding.result.text, 1..3, 2, true)
        .unwrap();
    let first_id = local.pending_visual().unwrap().0;
    assert_eq!(
        local.pending_visual().unwrap().1.roi(),
        first.target().roi()
    );
    let second = local
        .begin(&binding, &binding.result.text, 1..3, 2, true)
        .unwrap();
    let second_id = local.pending_visual().unwrap().0;
    assert_ne!(first_id, second_id);
    assert_eq!(
        local.complete(
            &first,
            &binding,
            &binding.result.text,
            true,
            Receipt::Failed("旧失败".into())
        ),
        Completion::Stale
    );
    assert_eq!(local.pending_visual().unwrap().0, second_id);
    assert_eq!(
        local.complete(
            &second,
            &binding,
            &binding.result.text,
            true,
            Receipt::Failed("当前失败".into())
        ),
        Completion::Failed
    );
    assert!(local.pending_visual().is_none());
    assert_eq!(binding.result.text, "first");
}

#[test]
fn local_cancel_and_completed_candidate_both_stop_visual_without_replacing_original() {
    let binding = local_binding();
    let mut local = LocalRetry::default();
    let request = local
        .begin(&binding, &binding.result.text, 1..3, 2, true)
        .unwrap();
    local.cancel("显式取消局部");
    assert!(request.token().is_cancelled());
    assert!(local.pending_visual().is_none());
    let request = local
        .begin(&binding, &binding.result.text, 1..3, 2, true)
        .unwrap();
    let result = RecognizedText {
        text: "candidate".into(),
        regions: vec![],
        words: vec![],
        lines: vec![],
    };
    assert_eq!(
        local.complete(
            &request,
            &binding,
            &binding.result.text,
            true,
            Receipt::Completed(result)
        ),
        Completion::Ready
    );
    assert!(local.pending_visual().is_none());
    assert_eq!(
        local
            .candidate(&binding, &binding.result.text, true)
            .unwrap()
            .text(),
        "candidate"
    );
    assert_eq!(binding.result.text, "first");
}

#[test]
fn click_trace_scan_matches_only_bound_real_kind_and_id_never_local() {
    let target = target();
    let mut session = RecognitionSession::default();
    let initial_request = session.begin_initial(target.clone(), 0);
    let initial = session_task(&session, Some(&target), 0).unwrap().0;
    assert_eq!(initial.id, initial_request.id);
    assert!(
        !trace_scan_key_matches(None, initial),
        "未绑定不能补写扫描记录"
    );
    assert!(trace_scan_key_matches(Some(initial), initial));
    assert!(!trace_scan_key_matches(
        Some(TaskKey {
            id: initial.id + 1,
            ..initial
        }),
        initial,
    ));

    // 两个真实任务槽的首个数字 ID 相同，类型仍须参与匹配。
    let mut foreground_slot = TaskSlot::default();
    let foreground_request = foreground_slot.start(InputTarget::Content(target.clone()));
    let foreground = foreground_task(&foreground_slot, Some(&target)).unwrap().0;
    assert_eq!(foreground.id, initial.id);
    assert_eq!(foreground.id, foreground_request.id);
    assert_eq!(foreground.kind, TaskKind::Foreground);
    assert!(!trace_scan_key_matches(Some(initial), foreground));
    assert!(!trace_scan_key_matches(Some(foreground), initial));
    assert!(trace_scan_key_matches(Some(foreground), foreground));
    assert!(foreground_slot
        .finish_receipt(foreground_request.id, Receipt::Completed(()))
        .is_some());
    assert!(foreground_task(&foreground_slot, Some(&target)).is_none());

    assert!(session.finish_initial(initial_request.id).is_some());
    assert!(session_task(&session, Some(&target), 0).is_none());
    let binding = local_binding();
    let mut local = LocalRetry::default();
    let _local_request = local
        .begin(&binding, &binding.result.text, 1..3, 2, true)
        .unwrap();
    let local_key = TaskKey {
        kind: TaskKind::Local,
        id: local.pending_visual().unwrap().0,
    };
    assert!(
        !trace_scan_key_matches(Some(initial), local_key),
        "旧完整识别已完成，局部动效不能给它补写 scan_paint"
    );
    assert!(
        !trace_scan_key_matches(Some(local_key), local_key),
        "局部任务即使数字与类型自匹配，也不归属于完整点击 Trace"
    );
    local.cancel("模型结束局部动效");
    assert!(local.pending_visual().is_none());

    session.show(Origin::Ocr(target.clone()));
    let retry_request = session.begin_retry(target.clone(), 3).unwrap();
    let retry = session_task(&session, Some(&target), 3).unwrap().0;
    assert_eq!(retry.kind, TaskKind::Retry);
    assert_eq!(retry.id, retry_request.id);
    assert!(trace_scan_key_matches(Some(retry), retry));
    assert!(!trace_scan_key_matches(Some(initial), retry));
    assert!(!trace_scan_key_matches(None, retry));
    assert!(session.finish_retry(retry_request.id).is_some());
    assert!(session_task(&session, Some(&target), 3).is_none());
}

#[test]
fn click_trace_result_present_waits_for_success_after_observed_paint_and_resets_when_unpainted() {
    use ResultPresentStep::{PaintObserved, Pending, Presented};

    let mut observation = ResultPresentObservation::default();
    // 旧成功帧已经递增串号，但没有当前有效结果 paint，不能形成终点。
    assert!(matches!(observation.observe(false, 41), Pending));
    assert!(matches!(observation.observe(false, 73), Pending));
    assert!(matches!(observation.observe(true, 73), PaintObserved));
    // 仅模拟未 Present／失败造成串号不增的观测；不执行或复现 renderer 错误。
    assert!(matches!(observation.observe(true, 73), Pending));
    assert!(matches!(observation.observe(true, 73), Pending));
    assert!(matches!(observation.observe(true, 72), Pending));
    assert!(matches!(observation.observe(true, 74), Presented));

    // blocked 清空旧 paint 观察；期间成功帧的增量也不能越过重新观察门。
    assert!(matches!(observation.observe(false, 90), Pending));
    assert!(matches!(observation.observe(false, 101), Pending));
    assert!(matches!(observation.observe(true, 101), PaintObserved));
    assert!(matches!(observation.observe(true, 101), Pending));
    assert!(matches!(observation.observe(true, 102), Presented));

    let mut interrupted = ResultPresentObservation::default();
    assert!(matches!(interrupted.observe(true, 5), PaintObserved));
    assert!(matches!(interrupted.observe(false, 6), Pending));
    assert!(
        matches!(interrupted.observe(true, 6), PaintObserved),
        "前一次 paint 后虽有成功帧，失效后仍必须重新等待后继提交"
    );
    assert!(matches!(interrupted.observe(true, 6), Pending));
    assert!(matches!(interrupted.observe(true, 7), Presented));
}
