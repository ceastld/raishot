use super::*;

const CROSS: u32 = 480;
const BODY: u32 = 352;
const BORDER: u32 = 4;

fn sparse_gray(world: i64, cross: u32) -> u8 {
    let digit = (cross - 34) / 5;
    (((world as u64 >> (digit * 3)) & 7) * 8) as u8
}

// 只有 20/480 像素宽的文字列，每行指纹不同；固定白边的错位仍低于动态容差。
fn sparse_fixed_page(
    axis: ScrollAxis,
    start: i64,
    body_length: u32,
    blank: &[(i64, i64)],
) -> Image {
    let length = body_length + 2 * BORDER;
    let (width, height) = axis.dimensions(CROSS, length);
    let mut image = Image {
        width,
        height,
        bgra: [255, 255, 255, 255].repeat(width as usize * height as usize),
        readback_ms: 0.,
    };
    for along in BORDER..BORDER + body_length {
        let world = start + (along - BORDER) as i64;
        if blank.iter().any(|&(from, to)| (from..to).contains(&world)) {
            continue;
        }
        for cross in 34..54 {
            let (x, y) = match axis {
                ScrollAxis::Vertical => (cross, along),
                ScrollAxis::Horizontal => (along, cross),
            };
            let offset = ((y * width + x) * 4) as usize;
            image.bgra[offset..offset + 3].fill(sparse_gray(world, cross));
        }
    }
    image
}

#[test]
fn long_scroll_auto_border_guard_rejects_thin_fixed_white_bands_without_consuming_baseline() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let initial = sparse_fixed_page(axis, 0, BODY, &[]);
        let original = initial.bgra.clone();
        let mut stitch = Stitcher::with_options(
            initial,
            StitchOptions {
                axis,
                detect_fixed_borders: true,
                ..Default::default()
            },
        )
        .unwrap();
        let baseline = stitch.state.last.clone();
        let update = stitch
            .process(sparse_fixed_page(axis, 80, BODY, &[]))
            .unwrap();
        assert_eq!(update.kind, UpdateKind::Rejected);
        assert_eq!(stitch.image().bgra, original);
        assert!(Arc::ptr_eq(&baseline, &stitch.state.last));
        assert!(stitch.state.auto_pending);
        assert!(!stitch.can_undo());

        // 同一对帧明确指定白栏时可精确拼接，拒绝原因不能是正文缺少定位证据。
        let mut manual = Stitcher::with_options(
            sparse_fixed_page(axis, 0, BODY, &[]),
            StitchOptions {
                axis,
                detect_fixed_borders: true,
                fixed_start: Some(BORDER),
                fixed_end: Some(BORDER),
            },
        )
        .unwrap();
        let update = manual
            .process(sparse_fixed_page(axis, 80, BODY, &[]))
            .unwrap();
        assert_eq!(
            (update.kind, update.delta, update.added),
            (UpdateKind::Added, 80, 80)
        );
        assert_eq!(
            manual.image().bgra,
            sparse_fixed_page(axis, 0, BODY + 80, &[]).bgra
        );
    }
}

#[test]
fn long_scroll_auto_border_guard_survives_initial_whitespace_match_and_rejection_preserves_undo() {
    // 第一对帧的重叠首尾恰好都是正文白行，第二次滚动才显露被固定白栏遮住的字。
    let blank = [(76, 80), (352, 356)];
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let original = sparse_fixed_page(axis, 0, BODY, &blank).bgra;
        let mut stitch = Stitcher::with_options(
            sparse_fixed_page(axis, 0, BODY, &blank),
            StitchOptions {
                axis,
                detect_fixed_borders: true,
                ..Default::default()
            },
        )
        .unwrap();
        let accepted = stitch
            .process(sparse_fixed_page(axis, 80, BODY, &blank))
            .unwrap();
        assert_eq!(
            (accepted.kind, accepted.delta, accepted.added),
            (UpdateKind::Added, 80, 80)
        );
        assert_eq!((stitch.state.start, stitch.state.end), (0, 0));
        assert!(!stitch.state.auto_pending);
        let expected = sparse_fixed_page(axis, 0, BODY + 80, &blank).bgra;
        assert_eq!(stitch.image().bgra, expected);
        let baseline = stitch.state.last.clone();
        let viewport = stitch.snapshot().viewport;
        let dimensions = (stitch.width, stitch.height);

        let rejected = stitch
            .process(sparse_fixed_page(axis, 160, BODY, &blank))
            .unwrap();
        assert_eq!(rejected.kind, UpdateKind::Rejected);
        assert_eq!((stitch.width, stitch.height), dimensions);
        assert_eq!(stitch.snapshot().viewport, viewport);
        assert_eq!(stitch.image().bgra, expected);
        assert!(Arc::ptr_eq(&baseline, &stitch.state.last));
        assert!(stitch.can_undo());
        assert!(stitch.undo());
        assert_eq!(stitch.image().bgra, original);
        assert!(stitch.state.auto_pending);
        assert!(!stitch.can_undo());
        assert_eq!(
            stitch
                .process(sparse_fixed_page(axis, 80, BODY, &blank))
                .unwrap()
                .added,
            80
        );
    }
}

fn sparse_scrolling_page(start: i64) -> Image {
    let (width, height) = (CROSS, 360);
    let mut image = Image {
        width,
        height,
        bgra: [255, 255, 255, 255].repeat(width as usize * height as usize),
        readback_ms: 0.,
    };
    for y in 0..height {
        let world = start + y as i64;
        let blank = [(0, 8), (80, 88), (352, 360), (432, 440)]
            .iter()
            .any(|&(from, to)| (from..to).contains(&world));
        if !blank {
            for x in 34..54 {
                let offset = ((y * width + x) * 4) as usize;
                image.bgra[offset..offset + 3].fill(sparse_gray(world, x));
            }
        }
        for x in width - 6..width {
            let thumb_start = 30 + start as u32 / 2;
            let value = if (thumb_start..thumb_start + 30).contains(&y) {
                90
            } else {
                245
            };
            let offset = ((y * width + x) * 4) as usize;
            image.bgra[offset..offset + 3].fill(value);
        }
    }
    image
}

#[test]
fn long_scroll_auto_border_guard_allows_sparse_white_page_with_moving_six_pixel_scrollbar() {
    let mut stitch = Stitcher::with_options(
        sparse_scrolling_page(0),
        StitchOptions {
            detect_fixed_borders: true,
            ..Default::default()
        },
    )
    .unwrap();
    let original = stitch.image().bgra;
    for start in [80, 160] {
        let update = stitch.process(sparse_scrolling_page(start)).unwrap();
        assert_eq!(
            (update.kind, update.delta, update.added),
            (UpdateKind::Added, 80, 80)
        );
        assert_eq!((stitch.state.start, stitch.state.end), (0, 0));
        assert!(!stitch.state.auto_pending);
        assert_eq!(&stitch.image().bgra[..original.len()], &original);
    }
    assert_eq!((stitch.width, stitch.height), (CROSS, 520));
    assert_eq!(
        stitch.snapshot().viewport,
        ScrollAxis::Vertical.rect(160, 360, CROSS)
    );
    let expected = stitch.image().bgra;
    assert_eq!(
        stitch.process(sparse_scrolling_page(80)).unwrap().kind,
        UpdateKind::ViewportOnly
    );
    assert_eq!(stitch.image().bgra, expected);
}

#[test]
fn long_scroll_auto_border_guard_does_not_exempt_text_near_right_edge() {
    let page = |start| {
        let mut image = sparse_fixed_page(ScrollAxis::Vertical, start, BODY, &[]);
        for y in 0..image.height {
            let row = y as usize * CROSS as usize * 4;
            let text = image.bgra[row + 34 * 4..row + 54 * 4].to_vec();
            image.bgra[row + 34 * 4..row + 54 * 4].fill(255);
            image.bgra[row + (CROSS as usize - 24) * 4..row + (CROSS as usize - 4) * 4]
                .copy_from_slice(&text);
        }
        image
    };
    let mut stitch = Stitcher::with_options(
        page(0),
        StitchOptions {
            detect_fixed_borders: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stitch.process(page(80)).unwrap().kind, UpdateKind::Rejected);
    assert!(stitch.state.auto_pending);
    let mut manual = Stitcher::with_options(
        page(0),
        StitchOptions {
            fixed_start: Some(BORDER),
            fixed_end: Some(BORDER),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(manual.process(page(80)).unwrap().added, 80);
}
