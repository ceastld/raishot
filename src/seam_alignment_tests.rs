//! 接缝对齐的灰度输入与断言：已知位移、固定栏、均匀列表、周期歧义与历史预测。
use super::*;

fn align(
    prev: &[u8],
    curr: &[u8],
    width: u32,
    height: u32,
    predict: i32,
    history: &[OffsetHistoryItem],
) -> AlignResult {
    find_best_offset(
        prev,
        curr,
        width,
        height,
        AlignmentOptions {
            predict_offset: predict,
            ..Default::default()
        },
        history,
        &mut AlignmentWorkspace::default(),
    )
    .unwrap()
}
fn history(offsets: &[i32]) -> Vec<OffsetHistoryItem> {
    offsets
        .iter()
        .enumerate()
        .map(|(index, &offset)| OffsetHistoryItem {
            frame_index: index as i32 + 1,
            offset,
            diff: 0.,
            seam_row_in_curr: -1,
            is_trusted: true,
        })
        .collect()
}
fn content_pixel(row: u32, x: u32) -> [u8; 3] {
    let mut hash = row.wrapping_mul(2_654_435_761);
    hash ^= hash >> 16;
    [
        (row * 7 + x * 11 + (hash & 255)) as u8,
        (row * 13 + x * 5 + ((hash >> 8) & 255)) as u8,
        (row * 3 + x * 17 + ((hash >> 16) & 255)) as u8,
    ]
}
fn gray([r, g, b]: [u8; 3]) -> u8 {
    ((r as u32 + g as u32 * 2 + b as u32) >> 2) as u8
}
fn viewport(width: u32, height: u32, scroll: u32, top: u32, bottom: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                if y < top || y >= height - bottom {
                    40
                } else {
                    gray(content_pixel(scroll + y - top, x))
                }
            })
        })
        .collect()
}

#[test]
fn seam_known_downward_shift_returns_expected_offset() {
    let (width, height, offset) = (120u32, 200u32, 80u32);
    let prev: Vec<_> = (0..height)
        .flat_map(|y| (0..width).map(move |x| ((y * 5 + x * 3) % 256) as u8))
        .collect();
    let curr: Vec<_> = (0..height)
        .flat_map(|y| {
            let prev = &prev;
            (0..width).map(move |x| {
                if y + offset < height {
                    prev[((y + offset) * width + x) as usize]
                } else {
                    0
                }
            })
        })
        .collect();
    let result = align(&prev, &curr, width, height, offset as i32, &[]);
    assert_eq!(result.offset, offset as i32, "{result:?}");
    assert!(result.is_acceptable(8., 15.), "{result:?}");
}

#[test]
fn seam_constant_row_mean_uses_spatial_texture() {
    let (width, height, offset) = (96u32, 180u32, 64u32);
    let prev: Vec<_> = (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                if (x + (y * 37) % width) % width < width / 2 {
                    0
                } else {
                    254
                }
            })
        })
        .collect();
    let curr: Vec<_> = (0..height)
        .flat_map(|y| {
            let prev = &prev;
            (0..width).map(move |x| {
                if y + offset < height {
                    prev[((y + offset) * width + x) as usize]
                } else {
                    127
                }
            })
        })
        .collect();
    let result = align(&prev, &curr, width, height, 0, &[]);
    assert_eq!(result.offset, offset as i32, "{result:?}");
    assert!(result.is_acceptable(8., 15.), "{result:?}");
}

#[test]
fn seam_fixed_top_bottom_chrome_keeps_real_scroll() {
    let prev = viewport(180, 260, 0, 24, 16);
    let curr = viewport(180, 260, 90, 24, 16);
    let result = align(&prev, &curr, 180, 260, 0, &[]);
    assert_eq!(result.offset, 90, "{result:?}");
    assert!(result.is_acceptable(8., 15.), "{result:?}");
}

#[test]
fn seam_content_rect_small_scroll_large_overlap() {
    let prev = viewport(180, 730, 0, 0, 0);
    let curr = viewport(180, 730, 25, 0, 0);
    let result = align(&prev, &curr, 180, 730, 25, &[]);
    assert_eq!(result.offset, 25, "{result:?}");
    assert!(result.is_acceptable(8., 15.), "{result:?}");
}

#[test]
fn seam_seam_prediction_keeps_previous_row_stable() {
    let history = vec![OffsetHistoryItem {
        frame_index: 1,
        offset: 100,
        diff: 1.,
        seam_row_in_curr: 50,
        is_trusted: true,
    }];
    assert_eq!(predict_seam_in_overlap(&history, 100, 700), 50);
    assert_eq!(predict_seam_in_overlap(&history, 120, 680), 30);
    assert_eq!(
        100 + predict_seam_in_overlap(&history, 100, 700),
        120 + predict_seam_in_overlap(&history, 120, 680)
    );
}

#[test]
fn seam_history_predicts_observed_motion_without_inventing_speed() {
    assert_eq!(predict_next_offset(&history(&[120, 240]), 300), 240);
    assert_eq!(predict_next_offset(&history(&[20, 20, 20]), 300), 20);
    let mut values = history(&[20, 20, 300]);
    values[2].is_trusted = false;
    assert_eq!(predict_next_offset(&values, 300), 20);
    assert_eq!(try_get_stable_prediction(&values), Some(20));
    assert_eq!(try_get_stable_prediction(&history(&[20, 80])), None);
}

fn row_accent(row: u32) -> [u8; 3] {
    let hue = (row * 47 % 360) as f32;
    let c = 0.75f32 * 0.55f32;
    let x = c * (1. - ((hue / 60.) % 2. - 1.).abs());
    let m = 0.75f32 - c;
    let [r, g, b] = if hue < 60. {
        [c, x, 0.]
    } else if hue < 120. {
        [x, c, 0.]
    } else if hue < 180. {
        [0., c, x]
    } else if hue < 240. {
        [0., x, c]
    } else if hue < 300. {
        [x, 0., c]
    } else {
        [c, 0., x]
    };
    [
        ((r + m) * 255.) as u8,
        ((g + m) * 255.) as u8,
        ((b + m) * 255.) as u8,
    ]
}
fn uniform_list(width: u32, height: u32, start: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                let world = start + y;
                if world % 72 == 71 {
                    gray([42, 42, 46])
                } else if x < 56 {
                    gray(row_accent(world / 72))
                } else {
                    gray(content_pixel(world / 72, x))
                }
            })
        })
        .collect()
}

#[test]
fn seam_uniform_list_matches_all_five_viewports_without_excess_growth() {
    let (width, height, step) = (515u32, 515u32, 300u32);
    let mut prev = uniform_list(width, height, 0);
    let mut history = Vec::new();
    let mut scratch = AlignmentWorkspace::default();
    for frame in 1..5 {
        let curr = uniform_list(width, height, frame * step);
        let result = find_best_offset(
            &prev,
            &curr,
            width,
            height,
            AlignmentOptions {
                predict_offset: step as i32,
                ..Default::default()
            },
            &history,
            &mut scratch,
        )
        .unwrap();
        assert_eq!(result.offset, step as i32, "frame={frame} {result:?}");
        assert!(result.is_acceptable(8., 15.), "frame={frame} {result:?}");
        history.push(OffsetHistoryItem {
            frame_index: frame as i32,
            offset: result.offset,
            diff: result.score,
            seam_row_in_curr: result.seam_row_in_curr,
            is_trusted: result.is_visually_unique,
        });
        prev = curr;
    }
}

fn periodic(width: u32, height: u32, start: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                let row = (start + y) % 40;
                let card = if !(3..38).contains(&row) { 35 } else { 215 };
                if x > 24 && x < 132 && (10..16).contains(&row) {
                    if (x / 7 + row).is_multiple_of(2) {
                        55
                    } else {
                        125
                    }
                } else {
                    card
                }
            })
        })
        .collect()
}

#[test]
fn seam_periodic_alias_without_history_remains_ambiguous() {
    let prev = periodic(160, 320, 0);
    let curr = periodic(160, 320, 20);
    let result = align(&prev, &curr, 160, 320, 0, &[]);
    assert!(result.is_ambiguous, "{result:?}");
    assert!(!result.is_acceptable(8., 15.));
    assert!(result.finalist_count <= 34);
}

#[test]
fn seam_stable_prediction_does_not_override_clear_visual_winner() {
    let blocks = [100u8, 110, 100, 100, 110, 100, 110, 110, 100];
    let source: Vec<_> = (0..344)
        .flat_map(|y| std::iter::repeat_n(blocks[y / 40], 160))
        .collect();
    let result = align(
        &source[..320 * 160],
        &source[24 * 160..344 * 160],
        160,
        320,
        0,
        &history(&[20, 20]),
    );
    assert_eq!(result.offset, 24, "{result:?}");
    assert!(result.is_visually_unique);
    assert!(!result.resolved_by_prediction);
}

#[test]
fn seam_workspace_is_reused_and_reports_original_confidence_fields() {
    let prev = viewport(180, 260, 0, 0, 0);
    let curr = viewport(180, 260, 64, 0, 0);
    let mut scratch = AlignmentWorkspace::default();
    let result = find_best_offset(
        &prev,
        &curr,
        180,
        260,
        Default::default(),
        &[],
        &mut scratch,
    )
    .unwrap();
    let capacity = scratch.prev_spatial.capacity();
    let again = find_best_offset(
        &prev,
        &curr,
        180,
        260,
        Default::default(),
        &[],
        &mut scratch,
    )
    .unwrap();
    assert_eq!(result.offset, 64);
    assert_eq!(result.offset, again.offset);
    assert_eq!(scratch.prev_spatial.capacity(), capacity);
    assert_eq!(result.rect_in_prev.width(), 180);
    assert_eq!(result.rect_in_prev.height(), 196);
    assert_eq!(result.rect_in_curr.top, 0);
    assert_eq!(result.rect_in_curr.height(), 196);
    assert_eq!(result.seam_row_in_prev - result.seam_row_in_curr, 64);
    assert_eq!(result.min_seam_row_diff, 0.);
    assert_eq!(result.matched_row_ratio, 1.);
    assert!(result.second_best_selection_score >= result.selection_score);
    assert!(result.second_best_offset.abs() < 260);
    assert!(result.visual_mismatch_ratio >= 0.);
    assert!(result.alternative_mismatch_ratio >= 0.);
    assert!(result.alternative_evidence_blocks <= 16);
    assert!(!result.as_ambiguous(0.).is_acceptable(8., 15.));
    assert_eq!(history(&[20])[0].frame_index, 1);
    assert_eq!(history(&[20])[0].diff, 0.);
}

fn unique_page(width: u32, height: u32, start: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                let world = start + y;
                let hash = world.wrapping_mul(2_654_435_761);
                ((hash >> 16) + x * 29 + world / 17 * 11) as u8
            })
        })
        .collect()
}

#[test]
fn seam_weak_badge_does_not_claim_periodic_uniqueness_but_strong_anchors_do() {
    for strong in [false, true] {
        let page = |start: u32| {
            let mut image = periodic(192, 320, start);
            for y in 0..320 {
                let world = start + y;
                for x in 0..192 {
                    if (!strong && x >= 187 && world % 71 < 7)
                        || (strong && (48..144).contains(&x) && (8..38).contains(&(world % 113)))
                    {
                        image[(y * 192 + x) as usize] = if strong {
                            (world * 31 + x * 17) as u8
                        } else {
                            (world * 17 + x * 29) as u8
                        };
                    }
                }
            }
            image
        };
        let result = align(&page(0), &page(20), 192, 320, 0, &[]);
        if strong {
            assert_eq!(result.offset, 20, "{result:?}");
            assert!(
                result.is_visually_unique && result.is_acceptable(8., 15.),
                "{result:?}"
            );
        } else {
            assert!(
                result.is_ambiguous && !result.is_acceptable(8., 15.),
                "{result:?}"
            );
        }
    }
}

#[test]
fn seam_tall_frame_refines_at_most_34_finalists() {
    let result = align(
        &unique_page(256, 2160, 0),
        &unique_page(256, 2160, 300),
        256,
        2160,
        0,
        &[],
    );
    assert_eq!(result.offset, 300, "{result:?}");
    assert!(result.is_acceptable(8., 15.));
    assert!(result.finalist_count <= 34);
}

#[test]
fn seam_short_overlap_uses_original_prior_and_bootstrap_gates() {
    let prev = unique_page(64, 100, 0);
    for (offset, prior, stable, acceptable) in [(80, 20, false, false), (69, 60, true, true)] {
        let result = find_best_offset(
            &prev,
            &unique_page(64, 100, offset),
            64,
            100,
            AlignmentOptions {
                min_overlap: 10,
                ..Default::default()
            },
            &history(&[prior, prior]),
            &mut AlignmentWorkspace::default(),
        )
        .unwrap();
        assert_eq!(result.offset, offset as i32, "{result:?}");
        assert_eq!(result.has_stable_motion_prior, stable, "{result:?}");
        assert_eq!(result.is_acceptable(8., 15.), acceptable, "{result:?}");
    }
}

#[test]
fn seam_exact_short_overlap_after_trusted_direction_can_vary_step() {
    let prev = unique_page(160, 400, 0);
    let curr = unique_page(160, 400, 280);
    let result = find_best_offset(
        &prev,
        &curr,
        160,
        400,
        AlignmentOptions::default(),
        &history(&[100, 100]),
        &mut AlignmentWorkspace::default(),
    )
    .unwrap();
    assert_eq!(result.offset, 280, "{result:?}");
    assert!(!result.has_stable_motion_prior);
    assert!(result.has_verified_short_overlap);
    assert!(result.is_acceptable(8., 15.));
    let bootstrap = align(&prev, &curr, 160, 400, 0, &[]);
    assert!(!bootstrap.has_verified_short_overlap && !bootstrap.is_acceptable(8., 15.));
}
