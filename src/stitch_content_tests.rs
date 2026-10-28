use super::*;

fn image(width: u32, height: u32, mut color: impl FnMut(u32, u32) -> [u8; 4]) -> Image {
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            bgra.extend_from_slice(&color(x, y));
        }
    }
    Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    }
}
fn viewport(width: u32, height: u32, offset: u32, insets: FixedBorderInsets) -> Image {
    image(width, height, |x, y| {
        if y < insets.top
            || y >= height - insets.bottom
            || x < insets.left
            || x >= width - insets.right
        {
            return [17, 34, 51, 13];
        }
        let world = y + offset;
        [
            world.wrapping_mul(31).wrapping_add(x * 7) as u8,
            world.wrapping_mul(17).wrapping_add(x * 11) as u8,
            world.wrapping_mul(13).wrapping_add(x * 19) as u8,
            255,
        ]
    })
}
fn transpose(source: &Image) -> Image {
    image(source.height, source.width, |x, y| {
        let offset = (x as usize * source.width as usize + y as usize) * 4;
        source.bgra[offset..offset + 4].try_into().unwrap()
    })
}

#[test]
fn long_scroll_content_merge_uses_all_four_minima_until_reset() {
    let mut tracker = StableContentBorderTracker::default();
    for insets in [
        FixedBorderInsets {
            top: 30,
            bottom: 18,
            left: 14,
            right: 20,
        },
        FixedBorderInsets {
            top: 24,
            bottom: 22,
            left: 10,
            right: 16,
        },
        FixedBorderInsets {
            top: 28,
            bottom: 16,
            left: 12,
            right: 18,
        },
    ] {
        tracker.merge(insets, 300, 260);
    }
    assert_eq!(
        tracker.content_rect(),
        Some(Rect {
            x: 8,
            y: 22,
            width: 278,
            height: 224
        })
    );
    tracker.reset();
    assert_eq!(tracker.content_rect(), None);
    tracker.merge(
        FixedBorderInsets {
            top: 36,
            bottom: 20,
            left: 20,
            right: 20,
        },
        300,
        260,
    );
    assert_eq!(tracker.content_rect().unwrap().y, 34);
}

#[test]
fn long_scroll_content_frame_pairs_converge_and_horizontal_view_is_equivalent() {
    let insets = FixedBorderInsets {
        top: 24,
        bottom: 16,
        left: 10,
        right: 12,
    };
    let expected = Rect {
        x: 8,
        y: 22,
        width: 162,
        height: 224,
    };
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut tracker = StableContentBorderTracker::default();
        for i in 1..5 {
            let previous = viewport(180, 260, (i - 1) * 90, insets);
            let current = viewport(180, 260, i * 90, insets);
            let (previous, current) = if axis == ScrollAxis::Horizontal {
                (transpose(&previous), transpose(&current))
            } else {
                (previous, current)
            };
            assert_eq!(
                estimate_fixed_borders(&previous, &current, axis),
                Some(insets)
            );
            assert_eq!(
                tracker.observe_frame_pair(&previous, &current, axis),
                Some(expected)
            );
        }
    }
    assert_eq!(physical_rect(expected, ScrollAxis::Vertical), expected);
    assert_eq!(
        physical_rect(expected, ScrollAxis::Horizontal),
        Rect {
            x: 22,
            y: 8,
            width: 224,
            height: 162
        }
    );
}

#[test]
fn long_scroll_content_detection_failure_preserves_stable_region_without_a_frame_guard() {
    let insets = FixedBorderInsets {
        top: 24,
        bottom: 16,
        left: 0,
        right: 0,
    };
    let a = viewport(180, 260, 0, insets);
    let b = viewport(180, 260, 90, insets);
    let mut tracker = StableContentBorderTracker::default();
    let detected = tracker.observe_frame_pair(&a, &b, ScrollAxis::Vertical);
    assert!(detected.is_some());
    assert_eq!(
        tracker.observe_frame_pair(&b, &b, ScrollAxis::Vertical),
        detected
    );
    let no_chrome_a = viewport(180, 260, 0, FixedBorderInsets::default());
    let no_chrome_b = viewport(180, 260, 90, FixedBorderInsets::default());
    assert_eq!(
        estimate_fixed_borders(&no_chrome_a, &no_chrome_b, ScrollAxis::Vertical),
        None
    );
    let mut fresh = StableContentBorderTracker::default();
    assert_eq!(
        fresh.observe_frame_pair(&no_chrome_a, &no_chrome_b, ScrollAxis::Vertical),
        None
    );
}

#[test]
fn long_scroll_content_solid_small_band_returns_none_for_full_frame_fallback() {
    let a = image(200, 400, |_, _| [248, 248, 248, 255]);
    let b = image(200, 400, |_, y| {
        if (120..200).contains(&y) {
            [20, 30, 40, 255]
        } else {
            [248, 248, 248, 255]
        }
    });
    let mut tracker = StableContentBorderTracker::default();
    assert_eq!(
        tracker.observe_frame_pair(&a, &b, ScrollAxis::Vertical),
        None
    );
    assert_eq!(
        build_content_rect(
            100,
            100,
            FixedBorderInsets {
                top: 16,
                bottom: 18,
                ..Default::default()
            }
        )
        .unwrap()
        .height,
        70
    );
    assert_eq!(
        build_content_rect(
            100,
            100,
            FixedBorderInsets {
                top: 17,
                bottom: 18,
                ..Default::default()
            }
        ),
        None
    );
    assert_eq!(
        build_content_rect(49, 100, FixedBorderInsets::default()),
        None
    );
}

#[test]
fn long_scroll_content_border_noise_keeps_original_rgb_thresholds_and_ignores_alpha() {
    let insets = FixedBorderInsets {
        top: 24,
        bottom: 16,
        left: 0,
        right: 0,
    };
    let a = viewport(180, 260, 0, insets);
    for (noise, accepted) in [(1u8, true), (4u8, false)] {
        let mut b = viewport(180, 260, 90, insets);
        for y in 0..260 {
            for x in 0..180 {
                let offset = (y * 180 + x) as usize * 4;
                b.bgra[offset + 3] = 0;
                if !(24..244).contains(&y) {
                    b.bgra[offset] += noise;
                }
            }
        }
        assert_eq!(
            estimate_fixed_borders(&a, &b, ScrollAxis::Vertical).is_some(),
            accepted
        );
    }
}

// 独立全行/全列慢速 oracle，用于核对快速边扫描。
fn exhaustive_reference(a: &Image, b: &Image) -> Option<FixedBorderInsets> {
    let (width, height) = (a.width, a.height);
    let mut differences = vec![0u64; width as usize * height as usize];
    let mut row_sums = vec![0u64; height as usize];
    let (mut top, mut bottom) = (height, None);
    for y in 0..height {
        for x in 0..width {
            let pixel = (y * width + x) as usize;
            let offset = pixel * 4;
            let diff = (0..3)
                .map(|c| u64::from(a.bgra[offset + c].abs_diff(b.bgra[offset + c])))
                .sum::<u64>();
            differences[pixel] = diff;
            row_sums[y as usize] += diff;
        }
        if row_sums[y as usize] > width as u64 * 3 * 4 {
            top = top.min(y);
            bottom = Some(y);
        }
    }
    let bottom = bottom?;
    let (mut left, mut right) = (width, None);
    for x in 0..width {
        let sum = (top..=bottom)
            .map(|y| differences[(y * width + x) as usize])
            .sum::<u64>();
        if sum > (bottom - top + 1) as u64 * 3 * 4 {
            left = left.min(x);
            right = Some(x);
        }
    }
    let right = right?;
    if right - left + 1 < 50 || bottom - top + 1 < 50 {
        return None;
    }
    let candidate_top = top.saturating_sub(2);
    let candidate_bottom = (bottom + 3).min(height);
    if (candidate_bottom - candidate_top) as f64 / (height as f64) < 0.70
        || (top < 8 && height - bottom - 1 < 8)
    {
        return None;
    }
    if top >= 8 && (0..top).any(|y| row_sums[y as usize] > width as u64 * 3) {
        return None;
    }
    if height - bottom > 8 && (bottom + 1..height).any(|y| row_sums[y as usize] > width as u64 * 3)
    {
        return None;
    }
    Some(FixedBorderInsets {
        top,
        bottom: height - bottom - 1,
        left,
        right: width - right - 1,
    })
}

#[test]
fn long_scroll_content_edge_scan_matches_exhaustive_mixed_borders_and_noise() {
    let mut random = 10_247u32;
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        random
    };
    for iteration in 0..60 {
        let insets = FixedBorderInsets {
            top: next() % 32,
            bottom: next() % 32,
            left: next() % 16,
            right: next() % 16,
        };
        let a = image(97, 141, |_, _| {
            [next() as u8, next() as u8, next() as u8, 255]
        });
        let mut b = image(97, 141, |_, _| {
            [next() as u8, next() as u8, next() as u8, 255]
        });
        for y in 0..141 {
            for x in 0..97 {
                if y < insets.top
                    || y >= 141 - insets.bottom
                    || x < insets.left
                    || x >= 97 - insets.right
                {
                    let offset = (y * 97 + x) as usize * 4;
                    b.bgra[offset..offset + 4].copy_from_slice(&a.bgra[offset..offset + 4]);
                    if iteration % 5 == 0 {
                        b.bgra[offset] = b.bgra[offset].saturating_add((next() % 5) as u8);
                    }
                }
            }
        }
        assert_eq!(
            estimate_fixed_borders(&a, &b, ScrollAxis::Vertical),
            exhaustive_reference(&a, &b),
            "iteration={iteration}"
        );
    }
}
