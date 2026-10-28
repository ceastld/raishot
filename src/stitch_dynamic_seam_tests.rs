use super::*;

fn changed_rows(width: u32, height: u32, default: u8) -> (Vec<u8>, Vec<u8>) {
    (
        vec![100; width as usize * height as usize],
        vec![default; width as usize * height as usize],
    )
}

#[test]
fn actual_seam_rechecks_in_range_dynamic_row_and_preserves_offset_on_both_directions() {
    for offset in [24, -24] {
        let (previous, mut current) = changed_rows(32, 192, 117);
        current[80 * 32..81 * 32].fill(115);
        // 预测行在正文范围内，但17>15；不得未经SAD复核直接返回。
        assert_eq!(
            moving_seam(&previous, &current, (32, 192), (16, 16), offset, 96),
            Some(((80 + offset) as u32, 80))
        );
    }
}

#[test]
fn actual_seam_rejects_dynamic_shared_content_without_using_fixed_chrome() {
    for offset in [24, -24] {
        let (previous, mut current) = changed_rows(32, 192, 117);
        // 固定头尾完全相同，不能用它们替代正文中的可靠接缝。
        current[..16 * 32].fill(100);
        current[176 * 32..].fill(100);
        assert_eq!(
            moving_seam(&previous, &current, (32, 192), (16, 16), offset, 96),
            None
        );
    }
}

#[test]
fn actual_seam_keeps_valid_prediction_and_checks_zero_border_frames() {
    for offset in [24, -24] {
        let (previous, mut current) = changed_rows(32, 192, 115);
        assert_eq!(
            moving_seam(&previous, &current, (32, 192), (0, 0), offset, 96),
            Some(((96 + offset) as u32, 96))
        );
        current[96 * 32..97 * 32].fill(117);
        assert_eq!(
            moving_seam(&previous, &current, (32, 192), (0, 0), offset, 96),
            Some(((95 + offset) as u32, 95))
        );
    }
}

fn frame(axis: ScrollAxis, start: u32, borders: (u32, u32), changed: Option<(u32, u32)>) -> Image {
    let (cross, length) = (64, 192);
    let (head, tail) = borders;
    let (width, height) = axis.dimensions(cross, length);
    let mut image = Image {
        width,
        height,
        bgra: vec![0; width as usize * height as usize * 4],
        readback_ms: 0.,
    };
    for along in 0..length {
        for across in 0..cross {
            let mut value = if along < head || along >= length - tail {
                40
            } else {
                let world = start + along - head;
                let mut hash =
                    world.wrapping_mul(2_654_435_761) ^ (across / 4).wrapping_mul(2_246_822_519);
                hash ^= hash >> 16;
                (80 + hash % 96) as i16
            };
            if let Some((from, to)) = changed.filter(|_| (head..length - tail).contains(&along)) {
                // 每个实际16-band的均值保持不变，完整像素SAD分别为14.5或17。
                // 这是对候选出口的反例，不把band均值当作真实接缝的证明。
                let difference = if (from..=to).contains(&along) {
                    17
                } else if across % 4 < 2 {
                    14
                } else {
                    15
                };
                value += if across % 2 == 0 {
                    difference
                } else {
                    -difference
                };
            }
            let (x, y) = match axis {
                ScrollAxis::Vertical => (across, along),
                ScrollAxis::Horizontal => (along, across),
            };
            let index = ((y * width + x) * 4) as usize;
            image.bgra[index..index + 4].copy_from_slice(&[
                value as u8,
                value as u8,
                value as u8,
                255,
            ]);
        }
    }
    image
}

fn pixel(image: &Image, axis: ScrollAxis, along: u32, across: u32) -> &[u8] {
    let (x, y) = match axis {
        ScrollAxis::Vertical => (across, along),
        ScrollAxis::Horizontal => (along, across),
    };
    let index = ((y * image.width + x) * 4) as usize;
    &image.bgra[index..index + 4]
}

fn assert_same_committed_state(actual: &State, expected: &State) {
    assert!(Arc::ptr_eq(&actual.last, &expected.last));
    assert!(Arc::ptr_eq(&actual.gray, &expected.gray));
    assert_eq!(
        (actual.viewport_top, actual.length, actual.accepted_count),
        (
            expected.viewport_top,
            expected.length,
            expected.accepted_count
        )
    );
    assert_eq!(
        actual.borders.content_rect(),
        expected.borders.content_rect()
    );
    assert_eq!(
        actual.borders.fixed_borders(),
        expected.borders.fixed_borders()
    );
    let history = |state: &State| {
        state
            .history
            .iter()
            .map(|h| {
                (
                    h.frame_index,
                    h.offset,
                    h.diff,
                    h.seam_row_in_curr,
                    h.is_trusted,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(history(actual), history(expected));
    assert_eq!(actual.segments.len(), expected.segments.len());
    for (actual, expected) in actual.segments.iter().zip(&expected.segments) {
        assert_eq!(
            (actual.source, actual.origin),
            (expected.source, expected.origin)
        );
        assert!(Arc::ptr_eq(&actual.image, &expected.image));
    }
}

fn assert_same_stitcher_after_rejection(actual: &Stitcher, expected: &Stitcher) {
    let actual_snapshot = actual.snapshot();
    let expected_snapshot = expected.snapshot();
    assert_eq!(
        (
            actual_snapshot.width,
            actual_snapshot.height,
            actual_snapshot.viewport,
            actual_snapshot.axis
        ),
        (
            expected_snapshot.width,
            expected_snapshot.height,
            expected_snapshot.viewport,
            expected_snapshot.axis
        )
    );
    assert_same_committed_state(&actual.state, &expected.state);
    assert_eq!(actual.image().bgra, expected.image().bgra);
    assert_eq!(actual.can_undo(), expected.can_undo());
    assert_eq!(actual.pixel_bytes(), expected.pixel_bytes());
    match (&actual.previous, &expected.previous) {
        (Some(actual), Some(expected)) => assert_same_committed_state(actual, expected),
        (None, None) => {}
        _ => panic!("拒绝不得替换既有撤销状态"),
    }
}

fn tall_fixed_frame(axis: ScrollAxis, start: u32, changed: bool) -> Image {
    let (cross, length, head, tail) = (64, 256, 80, 80);
    let (width, height) = axis.dimensions(cross, length);
    let mut image = Image {
        width,
        height,
        bgra: vec![0; width as usize * height as usize * 4],
        readback_ms: 0.,
    };
    for along in 0..length {
        for across in 0..cross {
            // 不同头尾避免远距离把相同固定栏互配；正文96像素符合实际配置门。
            let mut value = if along < head {
                40
            } else if along >= length - tail {
                200
            } else {
                let world = start + along - head;
                let mut hash =
                    world.wrapping_mul(2_654_435_761) ^ (across / 4).wrapping_mul(2_246_822_519);
                hash ^= hash >> 16;
                (80 + hash % 96) as i16
            };
            if changed && (head..length - tail).contains(&along) {
                // 正文所有行实际SAD17；16-band均值仍相同，不伪造AlignResult。
                value += if across % 2 == 0 { 17 } else { -17 };
            }
            let (x, y) = match axis {
                ScrollAxis::Vertical => (across, along),
                ScrollAxis::Horizontal => (along, across),
            };
            let index = ((y * width + x) * 4) as usize;
            image.bgra[index..index + 4].copy_from_slice(&[
                value as u8,
                value as u8,
                value as u8,
                255,
            ]);
        }
    }
    image
}

#[test]
fn real_stitcher_dynamic_prediction_uses_verified_seam_and_undo_keeps_original_strips() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for borders in [(0, 0), (16, 16)] {
            let options = StitchOptions {
                axis,
                detect_fixed_borders: false,
                fixed_start: Some(borders.0),
                fixed_end: Some(borders.1),
            };
            let mut stitch =
                Stitcher::with_options(frame(axis, 0, borders, None), options).unwrap();
            for start in [24, 48] {
                let update = stitch.process(frame(axis, start, borders, None)).unwrap();
                assert_eq!((update.kind, update.delta), (UpdateKind::Added, 24));
            }
            let before = stitch.snapshot();
            let before_pixels = stitch.image();
            let before_last = stitch.state.last.clone();
            let before_history: Vec<_> = stitch
                .state
                .history
                .iter()
                .map(|h| (h.offset, h.seam_row_in_curr, h.is_trusted))
                .collect();
            let content = stitch.content_rect(&stitch.state).unwrap();
            let overlap = content.height as i32 - 24;
            let predicted =
                seam_alignment::predict_seam_in_overlap(&stitch.state.history, 24, overlap);
            assert!(predicted >= 16, "必须形成真实已提交的接缝预测");
            let predicted_row = content.y + predicted as u32;
            let incoming = Arc::new(frame(
                axis,
                72,
                borders,
                Some((predicted_row - 16, predicted_row + 16)),
            ));
            let incoming_gray = grayscale(&incoming, axis).unwrap();
            let (align, mapping) = choose_alignment(
                &stitch.state.gray,
                &incoming_gray,
                stitch.cross,
                stitch.frame_length,
                Some(content),
                &stitch.state.history,
                &mut Workspace::default(),
            )
            .unwrap();
            assert_eq!(align.offset, 24);
            assert!(align.is_acceptable(8., 15.), "{align:?}");
            let map_y = mapping.map_or(0, |rect| rect.y);
            assert_eq!(
                map_y, borders.0,
                "固定正文应胜过整帧固定栏，无边带保留全帧映射"
            );
            let chosen = map_y + align.seam_row_in_curr as u32;
            assert_eq!(chosen, predicted_row, "预测实际落进局部变化区");
            assert_eq!(align.min_seam_row_diff, 14.5);
            let chosen_difference = (0..stitch.cross)
                .map(|x| {
                    let a = stitch.state.gray[((chosen + 24) * stitch.cross + x) as usize];
                    let b = incoming_gray[(chosen * stitch.cross + x) as usize];
                    u64::from(a.abs_diff(b))
                })
                .sum::<u64>() as f64
                / stitch.cross as f64;
            assert_eq!(chosen_difference, 17., "不能用全局最低差14.5代替实际接缝17");
            let (seam_previous, seam_current) = moving_seam(
                &stitch.state.gray,
                &incoming_gray,
                (stitch.cross, stitch.frame_length),
                borders,
                align.offset,
                chosen,
            )
            .unwrap();
            assert_eq!(seam_current, predicted_row - 17);
            let update = stitch.process_shared(incoming.clone()).unwrap();
            assert_eq!(
                (update.kind, update.delta, update.added),
                (UpdateKind::Added, 24, 24)
            );
            assert_eq!(
                stitch.state.history.last().unwrap().seam_row_in_curr + map_y as i32,
                seam_current as i32
            );
            let actual = stitch.image();
            let seam_output = before.viewport.y + seam_previous;
            // 横向快照的 viewport.y=0；真实逻辑原点由同一物理轴读取。
            let seam_output = if axis == ScrollAxis::Horizontal {
                before.viewport.x + seam_previous
            } else {
                seam_output
            };
            for along in 0..stitch.state.length {
                for across in 0..stitch.cross {
                    let expected = if along <= seam_output {
                        pixel(&before_pixels, axis, along, across)
                    } else {
                        pixel(&incoming, axis, seam_current + along - seam_output, across)
                    };
                    assert_eq!(pixel(&actual, axis, along, across), expected);
                }
            }
            assert!(stitch.undo());
            let restored = stitch.snapshot();
            assert_eq!(
                (restored.width, restored.height, restored.viewport),
                (before.width, before.height, before.viewport)
            );
            assert_eq!(stitch.image().bgra, before_pixels.bgra);
            assert!(Arc::ptr_eq(&stitch.state.last, &before_last));
            assert_eq!(
                stitch
                    .state
                    .history
                    .iter()
                    .map(|h| (h.offset, h.seam_row_in_curr, h.is_trusted))
                    .collect::<Vec<_>>(),
                before_history
            );
            assert_eq!(restored.segments.len(), before.segments.len());
            for (restored, original) in restored.segments.iter().zip(&before.segments) {
                assert_eq!(
                    (restored.source, restored.origin),
                    (original.source, original.origin)
                );
                assert!(Arc::ptr_eq(&restored.image, &original.image));
            }
        }
    }

    // 保留上面原追加夹具和断言；补头独立从两次真实负位移形成预测与撤销。
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for borders in [(0, 0), (16, 16)] {
            let options = StitchOptions {
                axis,
                detect_fixed_borders: false,
                fixed_start: Some(borders.0),
                fixed_end: Some(borders.1),
            };
            let mut stitch =
                Stitcher::with_options(frame(axis, 96, borders, None), options).unwrap();
            for start in [72, 48] {
                let update = stitch.process(frame(axis, start, borders, None)).unwrap();
                assert_eq!(
                    (update.kind, update.delta, update.added),
                    (UpdateKind::Added, -24, 24)
                );
            }
            let before = stitch.clone();
            let before_pixels = before.image();
            let content = stitch.content_rect(&stitch.state).unwrap();
            let overlap = content.height as i32 - 24;
            let predicted =
                seam_alignment::predict_seam_in_overlap(&stitch.state.history, -24, overlap);
            assert!(predicted >= 16, "必须形成真实已提交的反向接缝预测");
            // predict 返回共有区行；负位移当前帧共有区由24开始。
            let predicted_row = content.y + 24 + predicted as u32;
            let incoming = Arc::new(frame(
                axis,
                24,
                borders,
                Some((predicted_row - 16, predicted_row + 16)),
            ));
            let incoming_gray = grayscale(&incoming, axis).unwrap();
            let (align, mapping) = choose_alignment(
                &stitch.state.gray,
                &incoming_gray,
                stitch.cross,
                stitch.frame_length,
                Some(content),
                &stitch.state.history,
                &mut Workspace::default(),
            )
            .unwrap();
            assert_eq!(align.offset, -24);
            assert!(align.is_acceptable(8., 15.), "{align:?}");
            let map_y = mapping.map_or(0, |rect| rect.y);
            assert_eq!(map_y, borders.0);
            let chosen = map_y + align.seam_row_in_curr as u32;
            assert_eq!(chosen, predicted_row, "反向预测实际落进局部变化区");
            assert_eq!(align.min_seam_row_diff, 14.5);
            let chosen_difference = (0..stitch.cross)
                .map(|x| {
                    let a = stitch.state.gray[((chosen - 24) * stitch.cross + x) as usize];
                    let b = incoming_gray[(chosen * stitch.cross + x) as usize];
                    u64::from(a.abs_diff(b))
                })
                .sum::<u64>() as f64
                / stitch.cross as f64;
            assert_eq!(chosen_difference, 17.);
            let (seam_previous, seam_current) = moving_seam(
                &stitch.state.gray,
                &incoming_gray,
                (stitch.cross, stitch.frame_length),
                borders,
                align.offset,
                chosen,
            )
            .unwrap();
            assert_eq!(seam_current, predicted_row - 17);
            let update = stitch.process_shared(incoming.clone()).unwrap();
            assert_eq!(
                (update.kind, update.delta, update.added),
                (UpdateKind::Added, -24, 24)
            );
            assert_eq!(
                update.viewport,
                axis.rect(0, stitch.frame_length, stitch.cross)
            );
            assert_eq!(stitch.state.length, before.state.length + 24);
            assert_eq!(
                stitch.state.history.last().unwrap().seam_row_in_curr + map_y as i32,
                seam_current as i32
            );
            let seam_output = before.state.viewport_top + seam_previous;
            let actual = stitch.image();
            for along in 0..stitch.state.length {
                for across in 0..stitch.cross {
                    let expected = if along < seam_current {
                        pixel(&incoming, axis, along, across)
                    } else {
                        pixel(
                            &before_pixels,
                            axis,
                            seam_output + along - seam_current,
                            across,
                        )
                    };
                    assert_eq!(pixel(&actual, axis, along, across), expected);
                }
            }
            assert!(Arc::ptr_eq(&stitch.state.last, &incoming));
            assert!(stitch.can_undo());
            assert!(stitch.undo());
            assert_same_committed_state(&stitch.state, &before.state);
            assert_eq!(stitch.image().bgra, before_pixels.bgra);
            assert_eq!(stitch.snapshot().viewport, before.snapshot().viewport);
            assert_eq!((stitch.width, stitch.height), (before.width, before.height));
            assert!(!stitch.can_undo());
            assert!(!stitch.undo());
        }
    }

    // 原192/16边带的全17会先被旧质量门拒绝，不能冒称新增出口。
    // 合法厚条带使整帧确实可能接受，而正文88共有行全部不可靠；实际入口仍需证明。
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for delta in [8, -8] {
            for accepted_steps in [0, 2] {
                let initial_start = if delta > 0 { 0 } else { 48 };
                let mut stitch = Stitcher::with_options(
                    tall_fixed_frame(axis, initial_start, false),
                    StitchOptions {
                        axis,
                        detect_fixed_borders: false,
                        fixed_start: Some(80),
                        fixed_end: Some(80),
                    },
                )
                .unwrap();
                for step in 1..=accepted_steps {
                    let start = (initial_start as i32 + step * delta) as u32;
                    let update = stitch
                        .process(tall_fixed_frame(axis, start, false))
                        .unwrap();
                    assert_eq!(
                        (update.kind, update.delta, update.added),
                        (UpdateKind::Added, delta, 8)
                    );
                }
                assert_eq!(stitch.can_undo(), accepted_steps > 0);
                let before = stitch.clone();
                let next_start = (initial_start as i32 + (accepted_steps + 1) * delta) as u32;
                let rejected = Arc::new(tall_fixed_frame(axis, next_start, true));
                let rejected_gray = grayscale(&rejected, axis).unwrap();
                let content = stitch.content_rect(&stitch.state).unwrap();
                assert_eq!(
                    content,
                    Rect {
                        x: 0,
                        y: 80,
                        width: 64,
                        height: 96
                    }
                );
                let (align, mapping) = choose_alignment(
                    &stitch.state.gray,
                    &rejected_gray,
                    stitch.cross,
                    stitch.frame_length,
                    Some(content),
                    &stitch.state.history,
                    &mut Workspace::default(),
                )
                .unwrap();
                assert_eq!(align.offset, delta, "必须来自真实搜索，不得人为设置位移");
                assert!(align.is_acceptable(8., 15.), "{axis:?}/{delta}: {align:?}");
                assert!(mapping.is_none(), "正文全部差17不能获得alt质量资格");
                assert_eq!(align.min_seam_row_diff, 0., "固定栏只使整帧最低差为0");
                let new_top = stitch.state.viewport_top as i64 + i64::from(delta);
                assert!(
                    new_top < 0
                        || new_top + i64::from(stitch.frame_length)
                            > i64::from(stitch.state.length),
                    "新增出口只对真实输出触发，不能使用回访路径"
                );
                let mut shared_body_rows = 0;
                for current_row in 80..176 {
                    let previous_row = current_row as i32 + delta;
                    if !(80..176).contains(&previous_row) {
                        continue;
                    }
                    let actual_difference = (0..stitch.cross)
                        .map(|x| {
                            let a = stitch.state.gray
                                [(previous_row as u32 * stitch.cross + x) as usize];
                            let b = rejected_gray[(current_row * stitch.cross + x) as usize];
                            u64::from(a.abs_diff(b))
                        })
                        .sum::<u64>() as f64
                        / f64::from(stitch.cross);
                    assert_eq!(actual_difference, 17.);
                    shared_body_rows += 1;
                }
                assert_eq!(shared_body_rows, 88);
                assert_eq!(
                    moving_seam(
                        &stitch.state.gray,
                        &rejected_gray,
                        (stitch.cross, stitch.frame_length),
                        (80, 80),
                        align.offset,
                        align.seam_row_in_curr as u32,
                    ),
                    None
                );
                let update = stitch.process_shared(rejected).unwrap();
                assert_eq!(
                    (update.kind, update.delta, update.added),
                    (UpdateKind::Rejected, 0, 0)
                );
                assert_eq!(
                    update.reason.as_deref(),
                    Some(tr!(
                        "共有正文没有可靠接缝；保留最后成功帧",
                        "No reliable seam in the shared content; keeping the last good frame"
                    ))
                );
                assert_eq!(update.viewport, before.snapshot().viewport);
                assert_same_stitcher_after_rejection(&stitch, &before);

                let mut undo_probe = stitch.clone();
                if let Some(previous) = &before.previous {
                    assert!(undo_probe.undo());
                    assert_same_committed_state(&undo_probe.state, previous);
                    let mut original_undo = before.clone();
                    assert!(original_undo.undo());
                    assert_same_stitcher_after_rejection(&undo_probe, &original_undo);
                } else {
                    assert!(!undo_probe.undo());
                    assert_same_stitcher_after_rejection(&undo_probe, &before);
                }
                assert!(!undo_probe.can_undo());

                // 下一帧从同一可信基线恢复；与没处理过拒绝帧的真实Stitcher比较。
                let recovery = Arc::new(tall_fixed_frame(axis, next_start, false));
                let mut pristine = before.clone();
                let expected_update = pristine.process_shared(recovery.clone()).unwrap();
                let recovery_update = stitch.process_shared(recovery.clone()).unwrap();
                assert_eq!(
                    (
                        expected_update.kind,
                        expected_update.delta,
                        expected_update.added
                    ),
                    (UpdateKind::Added, delta, 8)
                );
                assert_eq!(
                    (
                        recovery_update.kind,
                        recovery_update.delta,
                        recovery_update.added,
                        recovery_update.viewport
                    ),
                    (
                        expected_update.kind,
                        expected_update.delta,
                        expected_update.added,
                        expected_update.viewport
                    )
                );
                assert_eq!(
                    (stitch.width, stitch.height),
                    (pristine.width, pristine.height)
                );
                assert_eq!(stitch.image().bgra, pristine.image().bgra);
                assert_eq!(stitch.state.accepted_count, before.state.accepted_count + 1);
                assert!(Arc::ptr_eq(&stitch.state.last, &recovery));
                assert_eq!(stitch.state.gray.as_ref(), pristine.state.gray.as_ref());
                let history = |stitch: &Stitcher| {
                    stitch
                        .state
                        .history
                        .iter()
                        .map(|h| {
                            (
                                h.frame_index,
                                h.offset,
                                h.diff,
                                h.seam_row_in_curr,
                                h.is_trusted,
                            )
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(history(&stitch), history(&pristine));
                assert!(stitch.can_undo());
                assert!(stitch.undo());
                assert_same_committed_state(&stitch.state, &before.state);
                assert_eq!(stitch.image().bgra, before.image().bgra);
                assert_eq!(stitch.snapshot().viewport, before.snapshot().viewport);
                assert!(!stitch.can_undo());
                assert!(!stitch.undo());
            }
        }
    }
}
