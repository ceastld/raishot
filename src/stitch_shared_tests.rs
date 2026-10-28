//! 管线/证明同时持有原帧时，拼接器仍共享输入而不等待 Arc 唯一所有权。
use super::{ScrollAxis, StitchOptions, Stitcher, UpdateKind};
use crate::capture::Image;
use std::sync::Arc;

fn frame(axis: ScrollAxis, start: u32, length: u32) -> Image {
    let (width, height) = axis.dimensions(64, length);
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let (along, across) = match axis {
                ScrollAxis::Vertical => (y, x),
                ScrollAxis::Horizontal => (x, y),
            };
            let row = start + along;
            let mut hash = row.wrapping_mul(2_654_435_761);
            hash ^= hash >> 16;
            bgra.extend_from_slice(&[
                (row.wrapping_mul(3) + across * 17 + ((hash >> 16) & 255)) as u8,
                (row.wrapping_mul(13) + across * 5 + ((hash >> 8) & 255)) as u8,
                (row.wrapping_mul(7) + across * 11 + (hash & 255)) as u8,
                255,
            ]);
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}

#[test]
fn stitch_shared_arc_handoff_and_undo_retain_exact_source_pixels() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let first = Arc::new(frame(axis, 100, 160));
        let options = StitchOptions {
            axis,
            ..Default::default()
        };
        let mut stitch = Stitcher::with_options_shared(first.clone(), options).unwrap();
        let snapshot = stitch.snapshot();
        assert!(Arc::ptr_eq(&snapshot.segments[0].image, &first));
        drop(snapshot);

        let second = Arc::new(frame(axis, 124, 160));
        let proof = second.clone();
        let weak = Arc::downgrade(&second);
        let update = stitch.process_shared(second.clone()).unwrap();
        assert_eq!(
            (update.kind, update.delta, update.added),
            (UpdateKind::Added, 24, 24)
        );
        // 队列/锁外证明的 Arc 仍在，last 直接持原 Arc；不是另外 clone 像素。
        assert_eq!(Arc::strong_count(&second), 3);
        let result = stitch.image();
        let oracle = frame(axis, 100, 184);
        assert_eq!(
            (result.width, result.height, result.bgra),
            (oracle.width, oracle.height, oracle.bgra)
        );
        assert_eq!(
            stitch.process_shared(second.clone()).unwrap().kind,
            UpdateKind::Unchanged
        );
        assert_eq!(Arc::strong_count(&second), 3);

        drop(second);
        drop(proof);
        assert!(weak.upgrade().is_some(), "可信基线持有移交的原帧");
        assert!(stitch.undo());
        assert!(weak.upgrade().is_none(), "撤销后旧基线没有不可达保留");
        let result = stitch.image();
        assert_eq!(result.bgra, first.bgra);
    }
}

#[test]
fn stitch_shared_invalid_handoff_does_not_replace_baseline_or_history() {
    let first = Arc::new(frame(ScrollAxis::Vertical, 100, 160));
    let mut stitch =
        Stitcher::with_options_shared(first.clone(), StitchOptions::default()).unwrap();
    let invalid = Arc::new(frame(ScrollAxis::Vertical, 120, 161));
    let weak = Arc::downgrade(&invalid);
    assert!(stitch.process_shared(invalid.clone()).is_err());
    assert_eq!(Arc::strong_count(&invalid), 1);
    drop(invalid);
    assert!(weak.upgrade().is_none());
    let snapshot = stitch.snapshot();
    assert!(Arc::ptr_eq(&snapshot.segments[0].image, &first));
    assert!(!stitch.can_undo());
    assert_eq!(stitch.image().bgra, first.bgra);
}
