use super::*;

fn pixel(cross: u32, along: i64) -> [u8; 4] {
    let mut n = (along as u64)
        .wrapping_add(100_000)
        .wrapping_mul(0x9e3779b185ebca87)
        ^ (cross as u64 + 1).wrapping_mul(0xc2b2ae3d27d4eb4f);
    n ^= n >> 29;
    n = n.wrapping_mul(0x165667b19e3779f9);
    [(n >> 8) as u8, (n >> 24) as u8, (n >> 40) as u8, 255]
}
fn fixture(
    axis: ScrollAxis,
    start: i64,
    body: u32,
    head: u32,
    foot: u32,
    footer_version: i64,
) -> Image {
    let (width, height) = axis.dimensions(64, head + body + foot);
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let (cross, along) = match axis {
                ScrollAxis::Vertical => (x, y),
                ScrollAxis::Horizontal => (y, x),
            };
            let absolute = if along < head {
                -2000 + along as i64
            } else if along >= head + body {
                -1000 + (along - head - body) as i64 + footer_version * 100
            } else {
                start + (along - head) as i64
            };
            bgra.extend_from_slice(&pixel(cross, absolute));
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}
fn frame(start: i64) -> Image {
    fixture(ScrollAxis::Vertical, start, 120, 0, 0, 0)
}
fn assert_output(
    stitch: &Stitcher,
    axis: ScrollAxis,
    start: i64,
    body: u32,
    head: u32,
    foot: u32,
    footer_version: i64,
) {
    let actual = stitch.image();
    let reference = fixture(axis, start, body, head, foot, footer_version);
    assert_eq!(
        (actual.width, actual.height),
        (reference.width, reference.height)
    );
    assert_eq!(actual.bgra, reference.bgra);
    let snapshot = stitch.snapshot();
    let mut covered = vec![false; actual.width as usize * actual.height as usize];
    for segment in snapshot.segments {
        assert!(segment
            .source
            .valid(segment.image.width, segment.image.height));
        assert!(Rect {
            x: segment.origin.0,
            y: segment.origin.1,
            width: segment.source.width,
            height: segment.source.height
        }
        .valid(actual.width, actual.height));
        for y in segment.origin.1..segment.origin.1 + segment.source.height {
            for x in segment.origin.0..segment.origin.0 + segment.source.width {
                let i = (y * actual.width + x) as usize;
                assert!(!covered[i], "条带不可重复覆盖结果像素");
                covered[i] = true;
            }
        }
    }
    assert!(covered.into_iter().all(|p| p));
}

#[test]
fn legacy_forward_pixels_and_one_undo_remain_compatible() {
    let mut stitch = Stitcher::new(frame(0));
    assert_eq!(stitch.append(frame(45)).unwrap(), 45);
    assert_output(&stitch, ScrollAxis::Vertical, 0, 165, 0, 0, 0);
    assert_eq!(stitch.append(frame(45)).unwrap(), 0);
    assert!(stitch.can_undo());
    assert!(stitch.append(frame(800)).is_err());
    let mut wrong = frame(0);
    wrong.width = 50;
    assert!(stitch.append(wrong).is_err());
    assert!(stitch.undo());
    assert_output(&stitch, ScrollAxis::Vertical, 0, 120, 0, 0, 0);
    assert!(!stitch.undo());
}

#[test]
fn reverse_prepend_and_distant_revisit_preserve_pixels_and_baseline() {
    let mut stitch = Stitcher::new(frame(256));
    for start in [288, 320, 352, 384] {
        let update = stitch.process(frame(start)).unwrap();
        assert_eq!(
            (update.kind, update.delta, update.added),
            (UpdateKind::Added, 32, 32)
        );
    }
    let back = stitch.process(frame(256)).unwrap();
    assert_eq!(
        (back.kind, back.delta, back.added, back.viewport.y),
        (UpdateKind::ViewportOnly, -128, 0, 0)
    );
    let prepend = stitch.process(frame(224)).unwrap();
    assert_eq!(
        (prepend.kind, prepend.delta, prepend.added),
        (UpdateKind::Added, -32, 32)
    );
    assert_output(&stitch, ScrollAxis::Vertical, 224, 280, 0, 0, 0);
    assert!(stitch.undo());
    assert_output(&stitch, ScrollAxis::Vertical, 256, 248, 0, 0, 0);
    let next = stitch.process(frame(288)).unwrap();
    assert_eq!(
        (next.kind, next.delta, next.viewport.y),
        (UpdateKind::ViewportOnly, 32, 32)
    );
    assert!(stitch.undo());
    assert_eq!(stitch.snapshot().viewport.y, 0);
}

#[test]
fn horizontal_manual_borders_keep_first_header_and_latest_footer_once() {
    let options = StitchOptions {
        axis: ScrollAxis::Horizontal,
        fixed_start: Some(24),
        fixed_end: Some(20),
        ..Default::default()
    };
    let mut stitch =
        Stitcher::with_options(fixture(options.axis, 256, 120, 24, 20, 0), options).unwrap();
    let forward = stitch
        .process(fixture(ScrollAxis::Horizontal, 288, 120, 24, 20, 0))
        .unwrap();
    assert_eq!(
        (forward.kind, forward.delta, forward.viewport.x),
        (UpdateKind::Added, 32, 56)
    );
    let footer = stitch
        .process(fixture(ScrollAxis::Horizontal, 288, 120, 24, 20, 1))
        .unwrap();
    assert_eq!(
        (footer.kind, footer.delta, footer.added),
        (UpdateKind::Added, 0, 0)
    );
    assert_output(&stitch, ScrollAxis::Horizontal, 256, 152, 24, 20, 1);
    assert!(stitch.undo());
    assert_output(&stitch, ScrollAxis::Horizontal, 256, 152, 24, 20, 0);
    let reverse = stitch
        .process(fixture(ScrollAxis::Horizontal, 240, 120, 24, 20, 2))
        .unwrap();
    assert_eq!(
        (reverse.kind, reverse.delta, reverse.added),
        (UpdateKind::Added, -48, 16)
    );
    assert_output(&stitch, ScrollAxis::Horizontal, 240, 168, 24, 20, 2);
}

#[test]
fn automatic_textured_borders_require_unique_body_motion_on_both_axes() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut stitch = Stitcher::with_options(
            fixture(axis, 256, 120, 24, 20, 0),
            StitchOptions {
                axis,
                detect_fixed_borders: true,
                ..Default::default()
            },
        )
        .unwrap();
        let update = stitch.process(fixture(axis, 288, 120, 24, 20, 0)).unwrap();
        assert_eq!(
            (update.kind, update.delta, update.added),
            (UpdateKind::Added, 32, 32)
        );
        assert_eq!(stitch.state.start, 24);
        assert_eq!(stitch.state.end, 20);
        assert!(!stitch.state.auto_pending);
        assert_output(&stitch, axis, 256, 152, 24, 20, 0);
        assert!(stitch.undo());
        assert!(stitch.state.auto_pending);
        assert_eq!(
            stitch
                .process(fixture(axis, 288, 120, 24, 20, 0))
                .unwrap()
                .kind,
            UpdateKind::Added
        );
    }
}

#[test]
fn automatic_flat_borders_reject_without_consuming_first_frame() {
    let flat = |start| {
        let mut image = fixture(ScrollAxis::Vertical, start, 120, 24, 20, 0);
        image.bgra[..24 * 64 * 4].fill(255);
        image.bgra[144 * 64 * 4..].fill(255);
        image
    };
    let mut stitch = Stitcher::with_options(
        flat(256),
        StitchOptions {
            detect_fixed_borders: true,
            ..Default::default()
        },
    )
    .unwrap();
    let first = stitch.state.last.clone();
    assert_eq!(
        stitch.process(flat(288)).unwrap().kind,
        UpdateKind::Rejected
    );
    assert!(Arc::ptr_eq(&first, &stitch.state.last));
    assert!(!stitch.can_undo());
    let mut manual = Stitcher::with_options(
        flat(256),
        StitchOptions {
            fixed_start: Some(24),
            fixed_end: Some(20),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(manual.process(flat(288)).unwrap().added, 32);
}

#[test]
fn periodic_and_blank_matches_are_rejected_instead_of_guessing() {
    let periodic = |start| {
        let mut image = frame(start);
        for y in 0..120 {
            for x in 0..64 {
                let offset = (y * 64 + x) as usize * 4;
                image.bgra[offset..offset + 4].copy_from_slice(&pixel(x, (start + y as i64) % 8));
            }
        }
        image
    };
    let mut stitch = Stitcher::new(periodic(0));
    assert_eq!(
        stitch.process(periodic(3)).unwrap().kind,
        UpdateKind::Rejected
    );
    assert_eq!(stitch.height, 120);
    let solid = |value| Image {
        width: 64,
        height: 120,
        bgra: [value, 19, 29, 255].repeat(64 * 120),
        readback_ms: 0.,
    };
    let mut blank = Stitcher::new(solid(9));
    assert_eq!(blank.process(solid(10)).unwrap().kind, UpdateKind::Rejected);
    assert!(!blank.can_undo());
}

#[test]
fn automatic_header_does_not_swallow_adjacent_flat_scrolling_content() {
    let blank_body = |start: i64| {
        let mut image = fixture(ScrollAxis::Vertical, start, 120, 24, 20, 0);
        for row in 0..120 {
            if start + row < 320 {
                image.bgra[(24 + row as usize) * 64 * 4..(25 + row as usize) * 64 * 4].fill(255);
            }
        }
        image
    };
    let mut stitch = Stitcher::with_options(
        blank_body(256),
        StitchOptions {
            detect_fixed_borders: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        stitch.process(blank_body(288)).unwrap().kind,
        UpdateKind::Rejected
    );
    assert!(stitch.state.auto_pending);
    assert!(!stitch.can_undo());
    let mut manual = Stitcher::with_options(
        blank_body(256),
        StitchOptions {
            fixed_start: Some(24),
            fixed_end: Some(20),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(manual.process(blank_body(288)).unwrap().added, 32);
}

#[test]
fn long_stitch_localized_cursor_keeps_existing_pixels_and_remains_undoable() {
    let mut stitch = Stitcher::new(frame(0));
    stitch.process(frame(32)).unwrap();
    let expected = stitch.image().bgra;
    let mut animated = frame(64);
    // 与稀疏采样点错开；窄小动态允许定位，但绝不覆写已经拼接的像素。
    let offset = ((17 * 64 + 13) * 4) as usize;
    animated.bgra[offset] ^= 128;
    let update = stitch.process(animated).unwrap();
    assert_eq!(
        (update.kind, update.delta, update.added),
        (UpdateKind::Added, 32, 32)
    );
    assert_eq!(&stitch.image().bgra[..expected.len()], &expected);
    assert!(stitch.can_undo());
    assert!(stitch.undo());
    assert_eq!(stitch.image().bgra, expected);
    let update = stitch.process(frame(64)).unwrap();
    assert_eq!(
        (update.kind, update.delta, update.added),
        (UpdateKind::Added, 32, 32)
    );
    assert_output(&stitch, ScrollAxis::Vertical, 0, 184, 0, 0, 0);
}

#[test]
fn long_stitch_large_animation_and_excessive_line_or_total_outliers_preserve_result() {
    for (width, height) in [(24, 24), (6, 2), (3, 88)] {
        let mut stitch = Stitcher::new(frame(0));
        stitch.process(frame(32)).unwrap();
        let last = stitch.state.last.clone();
        let expected = stitch.image().bgra;
        let mut next = frame(64);
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * 64 + x) * 4) as usize;
                next.bgra[offset] ^= 128;
            }
        }
        assert_eq!(stitch.process(next).unwrap().kind, UpdateKind::Rejected);
        assert_eq!(stitch.image().bgra, expected);
        assert!(Arc::ptr_eq(&last, &stitch.state.last));
        assert!(stitch.can_undo());
    }
}

#[test]
fn long_stitch_noise_fallback_runs_after_false_exact_blank_anchor() {
    let mut first = frame(0);
    first.bgra[10 * 64 * 4..11 * 64 * 4].fill(255);
    for rgba in first.bgra[102 * 64 * 4..103 * 64 * 4].chunks_exact_mut(4) {
        rgba.copy_from_slice(&[254, 254, 254, 255]);
    }
    let mut next = frame(32);
    for rgba in next.bgra.chunks_exact_mut(4) {
        for channel in &mut rgba[..3] {
            *channel = channel.saturating_add(1);
        }
    }
    // 唯一完全相等的指纹来自错误白行 -60；真实 +32 的重叠只有轻微采集噪声。
    next.bgra[70 * 64 * 4..71 * 64 * 4].fill(255);
    let mut stitch = Stitcher::new(first);
    let update = stitch.process(next).unwrap();
    assert_eq!(
        (update.kind, update.delta, update.added),
        (UpdateKind::Added, 32, 32)
    );
    assert_eq!(stitch.height, 152);
}

#[test]
fn long_stitch_low_texture_soft_noise_is_rejected_before_search() {
    let solid = |value| Image {
        width: 1920,
        height: 1080,
        bgra: [value, value, value, 255].repeat(1920 * 1080),
        readback_ms: 0.,
    };
    let mut stitch = Stitcher::new(solid(250));
    let first = stitch.state.last.clone();
    assert_eq!(
        stitch.process(solid(251)).unwrap().kind,
        UpdateKind::Rejected
    );
    assert!(Arc::ptr_eq(&first, &stitch.state.last));
    assert!(!stitch.can_undo());
}

#[test]
fn long_stitch_automatic_textured_header_flat_padding_and_plain_frames() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let padded = |start| {
            let mut image = fixture(axis, start, 120, 24, 20, 0);
            for along in [23, 144] {
                for cross in 0..64 {
                    let (x, y) = axis.origin(along);
                    let (x, y) = match axis {
                        ScrollAxis::Vertical => (cross, y),
                        ScrollAxis::Horizontal => (x, cross),
                    };
                    let offset = ((y * image.width + x) * 4) as usize;
                    image.bgra[offset..offset + 4].fill(255);
                }
            }
            image
        };
        let options = StitchOptions {
            axis,
            detect_fixed_borders: true,
            ..Default::default()
        };
        let mut stitch = Stitcher::with_options(padded(256), options.clone()).unwrap();
        let update = stitch.process(padded(288)).unwrap();
        assert_eq!(
            (update.kind, update.delta, update.added),
            (UpdateKind::Added, 32, 32)
        );
        assert_eq!((stitch.state.start, stitch.state.end), (24, 20));
        assert!(!stitch.state.auto_pending);
        assert!(stitch.undo());
        assert!(stitch.state.auto_pending);
        let mut plain = Stitcher::with_options(fixture(axis, 0, 120, 0, 0, 0), options).unwrap();
        assert_eq!(
            plain
                .process(fixture(axis, 32, 120, 0, 0, 0))
                .unwrap()
                .added,
            32
        );
        assert_output(&plain, axis, 0, 152, 0, 0, 0);
    }
}

#[test]
fn long_stitch_white_page_narrow_text_column_and_moving_scrollbar() {
    let page = |start: u32| {
        let (width, height) = (384, 240);
        let mut image = Image {
            width,
            height,
            bgra: [255, 255, 255, 255].repeat(width as usize * height as usize),
            readback_ms: 0.,
        };
        for y in 0..height {
            let along = start + y;
            for x in 20..116 {
                if along % 24 < 16
                    && along % 4 < 2
                    && x % 6 < 4
                    && pixel(x / 6, (along / 24) as i64)[0] & 1 != 0
                {
                    let offset = ((y * width + x) * 4) as usize;
                    image.bgra[offset..offset + 3].fill(24);
                }
            }
            for x in width - 6..width {
                let value = if (24 + start / 2..64 + start / 2).contains(&y) {
                    90
                } else {
                    245
                };
                let offset = ((y * width + x) * 4) as usize;
                image.bgra[offset..offset + 3].fill(value);
            }
        }
        image
    };
    let mut stitch = Stitcher::new(page(0));
    let original = stitch.image().bgra;
    let update = stitch.process(page(32)).unwrap();
    assert_eq!(
        (update.kind, update.delta, update.added),
        (UpdateKind::Added, 32, 32)
    );
    assert_eq!(&stitch.image().bgra[..original.len()], &original);
    assert!(stitch.undo());
    assert_eq!(stitch.image().bgra, original);
}

// 真实运行默认打开自动固定栏；纯色上下留白不能让首帧一直处于待确认状态。
#[test]
fn long_scroll_default_auto_borders_preserve_sparse_page_whitespace_and_revisit() {
    let page = |axis: ScrollAxis, start: i64, length: u32| {
        let (width, height) = axis.dimensions(480, length);
        let mut image = Image {
            width,
            height,
            bgra: [255, 255, 255, 255].repeat(width as usize * height as usize),
            readback_ms: 0.,
        };
        for along in 0..length {
            let world = start + along as i64;
            if !(8..22).contains(&(world % 40)) {
                continue;
            }
            for cross in 32..168 {
                if cross % 7 < 5 && pixel(cross / 7, world)[0] & 1 != 0 {
                    let (x, y) = match axis {
                        ScrollAxis::Vertical => (cross, along),
                        ScrollAxis::Horizontal => (along, cross),
                    };
                    let offset = ((y * width + x) * 4) as usize;
                    image.bgra[offset..offset + 3].fill(32);
                }
            }
        }
        image
    };
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut stitch = Stitcher::with_options(
            page(axis, 0, 360),
            StitchOptions {
                axis,
                detect_fixed_borders: true,
                ..Default::default()
            },
        )
        .unwrap();
        for start in [80, 160] {
            let update = stitch.process(page(axis, start, 360)).unwrap();
            assert_eq!(
                (update.kind, update.delta, update.added),
                (UpdateKind::Added, 80, 80)
            );
            assert_eq!((stitch.state.start, stitch.state.end), (0, 0));
            assert!(!stitch.state.auto_pending);
        }
        assert_eq!(stitch.image().bgra, page(axis, 0, 520).bgra);
        let back = stitch.process(page(axis, 80, 360)).unwrap();
        assert_eq!(
            (back.kind, back.delta, back.added),
            (UpdateKind::ViewportOnly, -80, 0)
        );
        assert_eq!(stitch.snapshot().viewport, axis.rect(80, 360, 480));
    }
}

#[test]
fn one_pixel_scrolls_retain_strips_and_arc_clone_does_not_copy_frames() {
    let mut stitch = Stitcher::new(frame(0));
    let initial = stitch.snapshot().segments[0].image.clone();
    for start in 1..=100 {
        assert_eq!(stitch.process(frame(start)).unwrap().added, 1);
    }
    let snapshot = stitch.snapshot();
    assert_eq!(snapshot.segments.len(), 101);
    assert!(Arc::ptr_eq(&initial, &snapshot.segments[0].image));
    assert!(snapshot
        .segments
        .iter()
        .skip(1)
        .all(|s| s.image.height == 1 && s.image.width == 64));
    let output_bytes = 220 * 64 * 4;
    let frame_bytes = 120 * 64 * 4;
    assert!(stitch.pixel_bytes() <= output_bytes + 2 * frame_bytes);
    let mut staged = stitch.clone();
    assert!(Arc::ptr_eq(&staged.state.last, &stitch.state.last));
    assert!(Arc::ptr_eq(
        &staged.previous.as_ref().unwrap().last,
        &stitch.previous.as_ref().unwrap().last
    ));
    staged.process(frame(101)).unwrap();
    assert_eq!(stitch.height, 220);
    assert_eq!(staged.height, 221);
    assert_output(&stitch, ScrollAxis::Vertical, 0, 220, 0, 0, 0);
}

#[test]
fn large_output_exceeds_gpu_side_limit_but_stops_at_model_limit_transactionally() {
    let long_frame = |start| fixture(ScrollAxis::Vertical, start, 8192, 0, 0, 0);
    let mut stitch = Stitcher::new(long_frame(0));
    for step in 1..=9 {
        assert_eq!(
            stitch.process(long_frame(step * 6000)).unwrap().kind,
            UpdateKind::Added
        );
    }
    assert_eq!(stitch.height, 62192);
    assert!(stitch.height > 16384);
    assert!(stitch
        .snapshot()
        .segments
        .iter()
        .all(|s| s.image.height <= 8192));
    let last = stitch.state.last.clone();
    let bytes = stitch.pixel_bytes();
    assert_eq!(
        stitch.process(long_frame(60000)).unwrap().kind,
        UpdateKind::Limit
    );
    assert_eq!((stitch.height, stitch.pixel_bytes()), (62192, bytes));
    assert!(Arc::ptr_eq(&last, &stitch.state.last));
    assert!(stitch.undo());
    assert_eq!(stitch.height, 56192);
    assert!(valid_size(1000, 32000));
    assert!(!valid_size(1000, 32001));
    assert!(valid_size(32, 65535));
    assert!(!valid_size(32, 65536));
    assert!(Stitcher::with_options(
        frame(0),
        StitchOptions {
            fixed_start: Some(57),
            fixed_end: Some(0),
            ..Default::default()
        }
    )
    .is_err());
}
