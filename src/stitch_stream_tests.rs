//! 通过公开拼接契约验证共享像素、手动固定栏、双向接缝与一次撤销。
use crate::{
    capture::Image,
    stitch::{ScrollAxis, StitchOptions, StitchSnapshot, Stitcher, UpdateKind},
};
use std::sync::Arc;

fn pixel(row: u32, x: u32) -> [u8; 4] {
    let mut hash = row.wrapping_mul(2_654_435_761);
    hash ^= hash >> 16;
    [
        (row.wrapping_mul(3) + x * 17 + ((hash >> 16) & 255)) as u8,
        (row.wrapping_mul(13) + x * 5 + ((hash >> 8) & 255)) as u8,
        (row.wrapping_mul(7) + x * 11 + (hash & 255)) as u8,
        255,
    ]
}
fn frame(axis: ScrollAxis, cross: u32, length: u32, start: u32, head: u32, tail: u32) -> Image {
    let (width, height) = axis.dimensions(cross, length);
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let (along, across) = match axis {
                ScrollAxis::Vertical => (y, x),
                ScrollAxis::Horizontal => (x, y),
            };
            let color = if along < head {
                [40, 70, 100, 255]
            } else if along >= length - tail {
                [100, 70, 40, 255]
            } else {
                pixel(start + along - head, across)
            };
            bgra.extend_from_slice(&color);
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}
fn automatic(axis: ScrollAxis) -> StitchOptions {
    StitchOptions {
        axis,
        detect_fixed_borders: true,
        ..Default::default()
    }
}
fn same_pixels(actual: &Image, expected: &Image) {
    assert_eq!(
        (actual.width, actual.height),
        (expected.width, expected.height)
    );
    if actual.bgra != expected.bgra {
        let byte = actual
            .bgra
            .iter()
            .zip(&expected.bgra)
            .position(|(actual, expected)| actual != expected)
            .unwrap();
        let pixel = byte / 4;
        panic!(
            "首个像素差异：x={}，y={}，channel={}，实际={}，预期={}",
            pixel % actual.width as usize,
            pixel / actual.width as usize,
            byte % 4,
            actual.bgra[byte],
            expected.bgra[byte]
        );
    }
}
fn same_shared_snapshot(a: &StitchSnapshot, b: &StitchSnapshot) {
    assert_eq!(
        (a.width, a.height, a.viewport, a.axis),
        (b.width, b.height, b.viewport, b.axis)
    );
    assert_eq!(a.segments.len(), b.segments.len());
    for (a, b) in a.segments.iter().zip(&b.segments) {
        assert_eq!((a.source, a.origin), (b.source, b.origin));
        assert!(Arc::ptr_eq(&a.image, &b.image));
    }
}

#[test]
fn stitch_stream_small_steps_fixed_chrome_keep_shared_pixel_memory_bounded() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let (cross, length, head, tail) = (64, 160, 16, 16);
        let frame_bytes = cross as usize * length as usize * 4;
        for delta in [1, -1] {
            let initial_start = if delta > 0 { 0 } else { 64 };
            let mut stitch = Stitcher::with_options(
                frame(axis, cross, length, initial_start, head, tail),
                automatic(axis),
            )
            .unwrap();
            assert_eq!(stitch.snapshot().viewport, axis.rect(0, length, cross));
            for step in 1..=32 {
                let start = (initial_start as i32 + delta * step as i32) as u32;
                let update = stitch
                    .process(frame(axis, cross, length, start, head, tail))
                    .unwrap();
                assert_eq!(
                    (update.kind, update.delta, update.added),
                    (UpdateKind::Added, delta, 1),
                    "{axis:?},delta={delta},start={start}: {:?}",
                    update.reason
                );
                let snapshot = stitch.snapshot();
                assert_eq!(
                    (snapshot.width, snapshot.height),
                    axis.dimensions(cross, length + step),
                    "{axis:?},delta={delta},step={step}"
                );
                assert_eq!(
                    snapshot.viewport,
                    axis.rect(if delta > 0 { step } else { 0 }, length, cross),
                    "{axis:?},delta={delta},step={step}: 必须保留完整捕获视口"
                );
                same_pixels(
                    &stitch.image(),
                    &frame(
                        axis,
                        cross,
                        length + step,
                        initial_start.min(start),
                        head,
                        tail,
                    ),
                );
                let output_bytes = snapshot.width as usize * snapshot.height as usize * 4;
                // 两份可信原帧 + 一次撤销保留的旧边缘最多一帧；不能随滚动次数累计旧固定栏。
                assert!(
                    stitch.pixel_bytes() <= output_bytes + 3 * frame_bytes,
                    "{axis:?},delta={delta},step={step}: 小步滚动不能永久保留旧固定栏；实际{}，上限{}",
                    stitch.pixel_bytes(),
                    output_bytes + 3 * frame_bytes
                );
            }
        }
    }
}

#[test]
fn stitch_stream_clone_stage_does_not_change_original_snapshot_or_undo() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut original =
            Stitcher::with_options(frame(axis, 64, 160, 100, 0, 0), automatic(axis)).unwrap();
        assert_eq!(
            original
                .process(frame(axis, 64, 160, 124, 0, 0))
                .unwrap()
                .kind,
            UpdateKind::Added
        );
        let snapshot = original.snapshot();
        let bytes = original.pixel_bytes();
        let mut staged = original.clone();
        same_shared_snapshot(&snapshot, &staged.snapshot());
        assert_eq!(
            staged
                .process(frame(axis, 64, 160, 148, 0, 0))
                .unwrap()
                .delta,
            24
        );
        same_shared_snapshot(&snapshot, &original.snapshot());
        assert_eq!(original.pixel_bytes(), bytes);
        let wrong_size = frame(axis, 65, 160, 148, 0, 0);
        assert!(original.process(wrong_size).is_err());
        same_shared_snapshot(&snapshot, &original.snapshot());
        assert!(original.undo());
        same_pixels(&original.image(), &frame(axis, 64, 160, 100, 0, 0));
        same_pixels(&staged.image(), &frame(axis, 64, 208, 100, 0, 0));
        assert!(staged.undo());
        same_shared_snapshot(&snapshot, &staged.snapshot());
    }
}

#[test]
fn stitch_stream_prepend_reverse_revisit_and_undo_preserve_pixels_on_both_axes() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut stitch =
            Stitcher::with_options(frame(axis, 64, 160, 128, 0, 0), automatic(axis)).unwrap();
        let up = stitch.process(frame(axis, 64, 160, 96, 0, 0)).unwrap();
        assert_eq!((up.kind, up.delta, up.added), (UpdateKind::Added, -32, 32));
        let prefixed = stitch.snapshot();
        assert_eq!(prefixed.viewport, axis.rect(0, 160, 64));
        same_pixels(&stitch.image(), &frame(axis, 64, 192, 96, 0, 0));
        let down = stitch.process(frame(axis, 64, 160, 128, 0, 0)).unwrap();
        assert_eq!(
            (down.kind, down.delta, down.added),
            (UpdateKind::ViewportOnly, 32, 0)
        );
        assert_eq!(stitch.snapshot().viewport, axis.rect(32, 160, 64));
        same_pixels(&stitch.image(), &frame(axis, 64, 192, 96, 0, 0));
        assert!(stitch.undo());
        same_shared_snapshot(&prefixed, &stitch.snapshot());
        assert!(!stitch.undo());
    }
}

#[test]
fn stitch_stream_manual_fixed_parameters_keep_full_frames_and_validate_configuration() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let options = StitchOptions {
            axis,
            detect_fixed_borders: false,
            fixed_start: Some(16),
            fixed_end: Some(12),
        };
        let mut stitch = Stitcher::with_options(frame(axis, 96, 192, 0, 16, 12), options).unwrap();
        assert_eq!(stitch.snapshot().viewport, axis.rect(0, 192, 96));
        for start in [32, 64, 96] {
            let update = stitch.process(frame(axis, 96, 192, start, 16, 12)).unwrap();
            assert_eq!(
                (update.kind, update.delta),
                (UpdateKind::Added, 32),
                "{axis:?}: {:?}",
                update.reason
            );
            let quality = update.quality.expect("实际匹配应保留本帧质量");
            assert!((quality.overlap_ratio - 132. / 164.).abs() < 1e-12);
            assert_eq!(stitch.snapshot().viewport, axis.rect(start, 192, 96));
        }
        same_pixels(&stitch.image(), &frame(axis, 96, 288, 0, 16, 12));
        assert!(Stitcher::with_options(
            frame(axis, 96, 192, 0, 16, 12),
            StitchOptions {
                axis,
                fixed_start: Some(100),
                fixed_end: Some(40),
                ..Default::default()
            }
        )
        .is_err());
    }
}

#[test]
fn stitch_stream_match_quality_distinguishes_duplicate_zero_offset_and_motion() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let initial = Arc::new(frame(axis, 64, 160, 0, 0, 0));
        let mut stitch = Stitcher::with_options_shared(
            initial.clone(),
            StitchOptions {
                axis,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(stitch.process_shared(initial).unwrap().quality.is_none());

        // Alpha 不参与灰度匹配；像素不同但真实位移为零，不能走精确重复帧短路。
        let mut same_position = frame(axis, 64, 160, 0, 0, 0);
        same_position.bgra[3] = 254;
        let same_position = Arc::new(same_position);
        let unchanged = stitch.process_shared(same_position.clone()).unwrap();
        assert_eq!(
            (unchanged.kind, unchanged.delta),
            (UpdateKind::Unchanged, 0)
        );
        let quality = unchanged.quality.expect("零位移的实际匹配也有质量");
        assert_eq!(
            (quality.error, quality.matched_ratio, quality.overlap_ratio),
            (0., 1., 1.)
        );
        assert!(stitch
            .process_shared(same_position)
            .unwrap()
            .quality
            .is_none());

        let moved = stitch.process(frame(axis, 64, 160, 32, 0, 0)).unwrap();
        assert_eq!((moved.kind, moved.delta), (UpdateKind::Added, 32));
        let quality = moved.quality.expect("新增条带应提供本帧质量");
        assert!(quality.error.is_finite() && quality.error <= 8.);
        assert_eq!(quality.matched_ratio, 1.);
        assert!(quality.unique);
        assert_eq!(quality.overlap_ratio, 0.8);
    }
}
