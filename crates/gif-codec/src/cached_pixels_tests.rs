use crate::{valid_keys, Histogram, Palette, Scaler};

fn fixture(size: (u32, u32), phase: u32) -> Vec<u8> {
    let mut frame = Vec::with_capacity(size.0 as usize * size.1 as usize * 4);
    for y in 0..size.1 {
        for x in 0..size.0 {
            frame.extend_from_slice(&[
                (x * 73 + y * 31 + phase * 43) as u8,
                (x * 23 + y * 97 + phase * 71) as u8,
                (x * 117 + y * 11 + phase * 17) as u8,
                (x * 19 + y * 5 + phase * 61) as u8,
            ]);
        }
    }
    frame
}

fn check_histogram(size: (u32, u32), output: (u32, u32), frame: &[u8]) -> Vec<u16> {
    let scaler = Scaler::new(size, output);
    let mut original = Histogram::new();
    // 直接执行保留的实际实现，避免另写采样规则作为镜像 oracle。
    scaler.accumulate(&mut original, frame);
    let mut cached = Histogram::new();
    let keys = scaler.accumulate_keys(&mut cached, frame);
    assert_eq!(
        cached.samples(),
        original.samples(),
        "源 {size:?}，输出 {output:?}"
    );
    assert_eq!(keys.len(), output.0 as usize * output.1 as usize);
    assert!(keys.iter().all(|key| *key < 1 << 15));
    keys
}

fn palette() -> Palette {
    let mut histogram = Histogram::new();
    for rgb in [
        [0, 0, 0],
        [255, 255, 255],
        [255, 0, 0],
        [31, 113, 241],
        [18, 22, 26],
    ] {
        histogram.sample_rgb(rgb);
    }
    histogram.build(&mut |_| true).unwrap()
}

#[test]
fn gif_cached_pixels_disk_key_validation_accepts_all_buckets_and_rejects_corruption() {
    let keys: Vec<u16> = (0..1 << 15).collect();
    assert!(valid_keys(&keys));
    assert!(valid_keys(&[]));
    assert!(!valid_keys(&[0, 32768, 32767]));
    assert!(!valid_keys(&[u16::MAX]));
    assert!(!valid_keys(&[32767, 0, u16::MAX]));
}

#[test]
fn gif_cached_pixels_coarse_cancel_checks_keep_actual_keys_and_histogram_on_success() {
    let size = (14, 66);
    let output = (7, 33);
    let frame = fixture(size, 4);
    let scaler = Scaler::new(size, output);
    let mut histogram = Histogram::new();
    let mut checks = 0;
    let keys = scaler
        .accumulate_keys_with(&mut histogram, &frame, &mut || {
            checks += 1;
            true
        })
        .unwrap();
    assert_eq!(checks, 4, "第0、16、32行及完成时检查，避免逐像素回调");
    let mut original = Histogram::new();
    scaler.accumulate(&mut original, &frame);
    assert_eq!(histogram.samples(), original.samples());
    assert_eq!(keys, check_histogram(size, output, &frame));
}

#[test]
fn gif_cached_pixels_cancellation_at_each_row_checkpoint_returns_no_key_plane() {
    let size = (14, 66);
    let frame = fixture(size, 4);
    let scaler = Scaler::new(size, (7, 33));
    for stop in 1..=4 {
        let mut histogram = Histogram::new();
        histogram.sample_rgb([255, 255, 255]);
        let prior_samples = histogram.samples();
        let mut checks = 0;
        let result = scaler.accumulate_keys_with(&mut histogram, &frame, &mut || {
            checks += 1;
            checks != stop
        });
        assert_eq!(checks, stop);
        assert_eq!(result.err().unwrap().to_string(), "已取消 GIF 转换");
        if stop == 1 {
            assert_eq!(histogram.samples(), prior_samples);
        } else {
            assert!(
                histogram
                    .samples()
                    .iter()
                    .map(|(count, _)| *count)
                    .sum::<u64>()
                    > 1
            );
        }
        // 与宿主合同一致：取消后整个Histogram丢弃，不提交部分帧或建立调色板。
        drop(histogram);
    }
}

#[test]
fn gif_cached_pixels_all_small_same_sizes_match_actual_histogram_counts_and_sums() {
    for height in 1..=9 {
        for width in 1..=9 {
            let size = (width, height);
            check_histogram(size, size, &fixture(size, width + height));
        }
    }
}

#[test]
fn gif_cached_pixels_all_resize_ratios_match_actual_samples_and_indices() {
    let palette = palette();
    for (size, output) in [
        ((8, 6), (4, 3)),
        ((12, 8), (3, 2)),
        ((2, 1), (1, 1)),
        ((11, 7), (6, 4)),
        ((13, 9), (7, 5)),
        ((3, 5), (1, 2)),
        ((5, 3), (8, 7)),
        ((17, 11), (9, 6)),
        ((31, 19), (7, 3)),
        ((13, 9), (13, 9)),
        ((1, 9), (1, 9)),
        ((13, 1), (13, 1)),
        ((1, 1), (1, 1)),
    ] {
        for phase in [0, 1, 7] {
            let frame = fixture(size, phase);
            let keys = check_histogram(size, output, &frame);
            assert_eq!(
                palette.index_keys(&keys),
                Scaler::new(size, output).index(&frame, &palette),
                "源 {size:?}，输出 {output:?}，相位 {phase}"
            );
        }
    }
}

#[test]
fn gif_cached_pixels_keep_duplicate_edges_and_full_eight_bit_sample_sums() {
    for (size, expected_count) in [((1, 1), 4), ((4, 4), 4), ((5, 5), 9), ((9, 9), 16)] {
        let frame = [25, 17, 9, 3].repeat(size.0 as usize * size.1 as usize);
        let mut cached = Histogram::new();
        let keys = Scaler::new(size, size).accumulate_keys(&mut cached, &frame);
        let samples = cached.samples();
        assert_eq!(
            samples.iter().map(|(count, _)| *count).sum::<u64>(),
            expected_count
        );
        let sample = samples[usize::from(keys[0])];
        assert_eq!(
            sample,
            (
                expected_count,
                [9 * expected_count, 17 * expected_count, 25 * expected_count]
            )
        );
        assert_eq!(keys, check_histogram(size, size, &frame));
    }

    // 两个不同的 8 位 RGB 同落一个桶；不能用桶扩展色替代实采和。
    let frame = [25, 17, 9, 0, 31, 23, 15, 255];
    let mut histogram = Histogram::new();
    let keys = Scaler::new((2, 1), (2, 1)).accumulate_keys(&mut histogram, &frame);
    assert_eq!(keys[0], keys[1]);
    assert_eq!(
        histogram.samples()[usize::from(keys[0])],
        (4, [48, 80, 112])
    );
    assert_eq!(keys, check_histogram((2, 1), (2, 1), &frame));
}

#[test]
fn gif_cached_pixels_every_bucket_maps_to_actual_original_palette_lookup() {
    let palette = palette();
    let keys: Vec<u16> = (0..1 << 15).collect();
    let expected: Vec<_> = keys
        .iter()
        .map(|key| {
            let key = usize::from(*key);
            let expand = |value: usize| ((value << 3) | (value >> 2)) as u8;
            palette.index([expand(key >> 10), expand((key >> 5) & 31), expand(key & 31)])
        })
        .collect();
    assert_eq!(palette.index_keys(&keys), expected);
    assert!(palette.index_keys(&[]).is_empty());
}

#[test]
fn gif_cached_pixels_global_palette_and_build_progress_match_actual_original_frames() {
    let size = (17, 11);
    let output = (9, 5);
    let scaler = Scaler::new(size, output);
    let mut frames: Vec<_> = (0..7).map(|phase| fixture(size, phase)).collect();
    // 尾帧独有颜色，覆盖非 4 倍数末行和末列。
    let end = frames.last_mut().unwrap().len();
    frames.last_mut().unwrap()[end - 4..].copy_from_slice(&[160, 32, 255, 7]);
    let mut original = Histogram::new();
    let mut cached = Histogram::new();
    let keys: Vec<_> = frames
        .iter()
        .map(|frame| {
            scaler.accumulate(&mut original, frame);
            scaler.accumulate_keys(&mut cached, frame)
        })
        .collect();
    assert_eq!(cached.samples(), original.samples());
    let mut original_progress = Vec::new();
    let original_palette = original
        .build(&mut |value| {
            original_progress.push(value);
            true
        })
        .unwrap();
    let mut cached_progress = Vec::new();
    let cached_palette = cached
        .build(&mut |value| {
            cached_progress.push(value);
            true
        })
        .unwrap();
    assert_eq!(cached_progress, original_progress);
    assert_eq!(cached_palette.table, original_palette.table);
    for (frame, keys) in frames.iter().zip(keys) {
        assert_eq!(
            cached_palette.index_keys(&keys),
            scaler.index(frame, &original_palette)
        );
    }
    let all_keys: Vec<u16> = (0..1 << 15).collect();
    assert_eq!(
        cached_palette.index_keys(&all_keys),
        original_palette.index_keys(&all_keys)
    );
}

#[test]
fn gif_cached_pixels_alpha_does_not_change_samples_or_mapping() {
    let size = (13, 9);
    let output = (7, 5);
    let mut alpha0 = fixture(size, 3);
    let mut alpha255 = alpha0.clone();
    for pixel in alpha0.chunks_exact_mut(4) {
        pixel[3] = 0;
    }
    for pixel in alpha255.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    assert_eq!(
        check_histogram(size, output, &alpha0),
        check_histogram(size, output, &alpha255)
    );
}

#[test]
fn gif_cached_pixels_original_palette_cancellation_never_mutates_saved_keys() {
    let size = (17, 11);
    let frame = fixture(size, 2);
    for stop_at in [0.05, 0.4, 0.8, 1.0] {
        let mut histogram = Histogram::new();
        let keys = Scaler::new(size, size).accumulate_keys(&mut histogram, &frame);
        let original_keys = keys.clone();
        let mut cancelled = false;
        let result = histogram.build(&mut |value| {
            if value >= stop_at {
                cancelled = true;
                false
            } else {
                true
            }
        });
        assert!(cancelled);
        assert_eq!(result.err().unwrap().to_string(), "已取消 GIF 转换");
        assert_eq!(keys, original_keys);
    }
}
