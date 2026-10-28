//! 已知像素 oracle；不依赖窗口、桌面、剪贴板或 Scene 的实现。
use crate::local_fill::{erase_bgra, FillOptions, FillPath, FillStats, MAX_FILL_PIXELS};

fn solid(width: usize, height: usize, color: [u8; 4]) -> Vec<u8> {
    color.repeat(width * height)
}

fn set_pixel(image: &mut [u8], stride: usize, x: usize, y: usize, color: [u8; 4]) {
    let i = y * stride + x * 4;
    image[i..i + 4].copy_from_slice(&color);
}

fn pixel(image: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
    let i = y * stride + x * 4;
    image[i..i + 4].try_into().unwrap()
}

fn fill(
    width: usize,
    height: usize,
    mut image: Vec<u8>,
    mask: &[u8],
    options: FillOptions,
) -> (Vec<u8>, FillStats) {
    let before = image.clone();
    let stats = erase_bgra(
        &mut image,
        width as u32,
        height as u32,
        width * 4,
        mask,
        width,
        options,
    )
    .unwrap();
    for y in 0..height {
        for x in 0..width {
            if mask[y * width + x] <= options.threshold {
                assert_eq!(
                    pixel(&image, width * 4, x, y),
                    pixel(&before, width * 4, x, y),
                    "掩模外像素 ({x}, {y}) 不得改变"
                );
            }
        }
    }
    (image, stats)
}

#[test]
fn local_fill_ring_restores_bubble_color_using_actual_bucket_mean() {
    // 9×9 页面灰，7×7 气泡绿，中心 3×3 白字。2 px 外环只覆盖气泡。
    let mut image = solid(9, 9, [220, 225, 230, 255]);
    let mut mask = vec![0; 81];
    let mut green_count = 0;
    for y in 1..8 {
        for x in 1..8 {
            if (3..6).contains(&x) && (3..6).contains(&y) {
                mask[y * 9 + x] = 255;
                set_pixel(&mut image, 36, x, y, [255, 255, 255, 7]);
            } else {
                // 同一个量化桶的两种真实颜色各 20 个，不能使用桶中心代替均值。
                let color = if green_count < 20 {
                    [40, 160, 72, 13]
                } else {
                    [47, 167, 79, 241]
                };
                set_pixel(&mut image, 36, x, y, color);
                green_count += 1;
            }
        }
    }
    assert_eq!(green_count, 40);
    let (image, stats) = fill(9, 9, image, &mask, FillOptions::default());
    assert_eq!(
        stats,
        FillStats {
            path: FillPath::Ring,
            requested: 9,
            filled: 9,
        }
    );
    for y in 3..6 {
        for x in 3..6 {
            assert_eq!(pixel(&image, 36, x, y), [43, 163, 75, 255]);
        }
    }
}

#[test]
fn local_fill_ring_ties_keep_first_sample_bucket_and_ignore_source_alpha() {
    // 两桶各 4 个；先出现的桶 key 较大，不能以数组/HashMap 顺序打破平局。
    let mut image = solid(3, 3, [16, 240, 8, 255]);
    for (index, color) in [
        (0, [200, 8, 16, 0]),
        (1, [201, 9, 17, 128]),
        (7, [202, 10, 18, 3]),
        (8, [203, 11, 19, 255]),
    ] {
        set_pixel(&mut image, 12, index % 3, index / 3, color);
    }
    let mut mask = vec![0; 9];
    mask[4] = 255;
    let (image, stats) = fill(3, 3, image, &mask, FillOptions::default());
    assert_eq!(stats.path, FillPath::Ring);
    assert_eq!(pixel(&image, 12, 1, 1), [201, 9, 17, 255]);
}

#[test]
fn local_fill_ring_counts_each_neighbor_once_even_when_touching_two_holes() {
    let mut image = solid(5, 3, [255, 255, 255, 255]);
    let mut mask = vec![0; 15];
    mask[6] = 255;
    mask[8] = 255;
    // 中列 3 个红样本均与两个孔相邻；绿色有 4 个，蓝/灰各 3 个。
    // 按孔重复统计会错误选红色；正确主色为绿色。
    for y in 0..3 {
        set_pixel(&mut image, 20, 2, y, [220, 10, 10, 1]);
        set_pixel(&mut image, 20, 0, y, [10, 150, 10, 2]);
        set_pixel(&mut image, 20, 4, y, [100, 100, 100, 3]);
    }
    set_pixel(&mut image, 20, 1, 0, [10, 150, 10, 4]);
    for (x, y) in [(1, 2), (3, 0), (3, 2)] {
        set_pixel(&mut image, 20, x, y, [10, 10, 220, 5]);
    }
    let (image, stats) = fill(
        5,
        3,
        image,
        &mask,
        FillOptions {
            ring: 1,
            ..Default::default()
        },
    );
    assert_eq!(stats.filled, 2);
    assert_eq!(pixel(&image, 20, 1, 1), [10, 150, 10, 255]);
    assert_eq!(pixel(&image, 20, 3, 1), [10, 150, 10, 255]);
}

#[test]
fn local_fill_ring_uses_diagonals_and_clamps_ring_to_one_through_three() {
    // 对角线/轴向各 4 个不同颜色；行优先首个样本位于对角线。
    let mut image = solid(3, 3, [8, 200, 8, 99]);
    for (x, y) in [(0, 0), (2, 0), (0, 2), (2, 2)] {
        set_pixel(&mut image, 12, x, y, [96, 64, 32, 0]);
    }
    let mut mask = vec![0; 9];
    mask[4] = 255;
    let (image, _) = fill(
        3,
        3,
        image,
        &mask,
        FillOptions {
            ring: 1,
            ..Default::default()
        },
    );
    assert_eq!(pixel(&image, 12, 1, 1), [96, 64, 32, 255]);

    // 方形距离 1/2/3 的环分别 8/16/24 像素，各用不同颜色。
    let colors = [[24, 48, 72, 255], [80, 104, 128, 255], [136, 160, 184, 255]];
    let mut image = solid(7, 7, [255, 255, 255, 33]);
    let mut mask = vec![0; 49];
    mask[24] = 255;
    for y in 0..7_usize {
        for x in 0..7_usize {
            let distance = x.abs_diff(3).max(y.abs_diff(3));
            if distance != 0 {
                set_pixel(&mut image, 28, x, y, colors[distance - 1]);
            }
        }
    }
    for (ring, expected) in [
        (0, colors[0]),
        (1, colors[0]),
        (2, colors[1]),
        (3, colors[2]),
        (255, colors[2]),
    ] {
        let (output, stats) = fill(
            7,
            7,
            image.clone(),
            &mask,
            FillOptions {
                ring,
                ..Default::default()
            },
        );
        assert_eq!(stats.path, FillPath::Ring);
        assert_eq!(pixel(&output, 28, 3, 3), expected, "ring={ring}");
    }
}

#[test]
fn local_fill_ring_requires_four_samples_and_boundary_uses_only_nearest_radius() {
    let first = [32, 80, 200, 0];
    let second = [200, 120, 40, 77];
    // 孔在 x=1：2 px 环只有 x=0/2/3 三个样本，第二色占多数。
    // 正确结果必须走 boundary，只平均半径 1 的 x=0/2，不能选第二色主桶。
    let mut image = solid(4, 1, second);
    set_pixel(&mut image, 16, 0, 0, first);
    set_pixel(&mut image, 16, 1, 0, [255, 255, 255, 7]);
    let (output, stats) = fill(4, 1, image, &[0, 255, 0, 0], FillOptions::default());
    assert_eq!(
        stats,
        FillStats {
            path: FillPath::Boundary,
            requested: 1,
            filled: 1,
        }
    );
    assert_eq!(pixel(&output, 16, 1, 0), [116, 100, 120, 255]);

    // 孔在 x=2：环恰好四个样本，两色各二个，首桶的 key 更大。
    // 应切换为 ring 并保持先出现的第一色，不能继续取半径 1 的平均色。
    let mut image = solid(5, 1, second);
    for x in [0, 1] {
        set_pixel(&mut image, 20, x, 0, first);
    }
    set_pixel(&mut image, 20, 2, 0, [255, 255, 255, 7]);
    let (output, stats) = fill(5, 1, image, &[0, 0, 255, 0, 0], FillOptions::default());
    assert_eq!(
        stats,
        FillStats {
            path: FillPath::Ring,
            requested: 1,
            filled: 1,
        }
    );
    assert_eq!(pixel(&output, 20, 2, 0), [32, 80, 200, 255]);
}

#[test]
fn local_fill_threshold_is_strict_and_empty_or_all_hole_preserves_bytes() {
    let mut image = solid(3, 3, [50, 60, 70, 11]);
    set_pixel(&mut image, 12, 1, 1, [200, 210, 220, 99]);
    let mut mask = vec![8; 9];
    mask[4] = 9;
    let (output, stats) = fill(3, 3, image.clone(), &mask, FillOptions::default());
    assert_eq!(stats.requested, 1);
    assert_eq!(pixel(&output, 12, 1, 1), [50, 60, 70, 255]);
    let (output, stats) = fill(
        3,
        3,
        image.clone(),
        &mask,
        FillOptions {
            threshold: 9,
            ..Default::default()
        },
    );
    assert_eq!(stats.path, FillPath::Empty);
    assert_eq!(stats.requested, 0);
    assert_eq!(stats.filled, 0);
    assert_eq!(output, image);

    let before = [1, 2, 3, 4, 5, 6, 7, 8].to_vec();
    let (output, stats) = fill(2, 1, before.clone(), &[255, 255], FillOptions::default());
    assert_eq!(stats.path, FillPath::Boundary);
    assert_eq!(stats.requested, 2);
    assert_eq!(stats.filled, 0);
    assert_eq!(output, before);

    for (width, height) in [(0, 9), (9, 0), (0, 0)] {
        let mut untouched = [11, 22, 33];
        let stats = erase_bgra(
            &mut untouched,
            width,
            height,
            0,
            &[],
            0,
            FillOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.path, FillPath::Empty);
        assert_eq!(untouched, [11, 22, 33]);
    }
}

#[test]
fn local_fill_sparse_boundary_uses_first_nonempty_radius_without_propagating() {
    // 仅两个非孔样本，强制走 boundary。中间孔需跳过所有先前填好的孔。
    let mut image = solid(12, 1, [9, 99, 219, 77]);
    set_pixel(&mut image, 48, 0, 0, [20, 60, 180, 0]);
    set_pixel(&mut image, 48, 11, 0, [180, 40, 40, 1]);
    let mut mask = vec![255; 12];
    mask[0] = 0;
    mask[11] = 0;
    let (image, stats) = fill(12, 1, image, &mask, FillOptions::default());
    assert_eq!(
        stats,
        FillStats {
            path: FillPath::Boundary,
            requested: 10,
            filled: 10,
        }
    );
    for (x, expected) in [
        (1, [20, 60, 180, 255]), // 半径 1
        (2, [20, 60, 180, 255]), // 半径 3
        (3, [20, 60, 180, 255]),
        (4, [100, 50, 110, 255]), // 半径 8 同时找到两端，而非传播左侧填色
        (5, [100, 50, 110, 255]),
        (7, [100, 50, 110, 255]),
        (8, [180, 40, 40, 255]),  // 半径 3
        (10, [180, 40, 40, 255]), // 半径 1
    ] {
        assert_eq!(pixel(&image, 48, x, 0), expected, "孔 x={x}");
    }
}

#[test]
fn local_fill_boundary_reports_partial_fill_and_keeps_unreachable_holes() {
    let mut image = solid(20, 1, [3, 4, 5, 6]);
    set_pixel(&mut image, 80, 0, 0, [90, 130, 170, 0]);
    let mut mask = vec![255; 20];
    mask[0] = 0;
    let (image, stats) = fill(20, 1, image, &mask, FillOptions::default());
    assert_eq!(stats.path, FillPath::Boundary);
    assert_eq!(stats.requested, 19);
    assert_eq!(stats.filled, 8);
    for x in 1..9 {
        assert_eq!(pixel(&image, 80, x, 0), [90, 130, 170, 255]);
    }
    for x in 9..20 {
        assert_eq!(pixel(&image, 80, x, 0), [3, 4, 5, 6]);
    }
}

#[test]
fn local_fill_padding_and_minimal_final_rows_are_not_sampled_or_modified() {
    let (width, height, stride, mask_stride) = (3, 3, 17, 5);
    let image_needed = (height - 1) * stride + width * 4;
    let mask_needed = (height - 1) * mask_stride + width;
    for extra_tail in [0, 3] {
        let mut image = vec![0xa9; image_needed + extra_tail];
        let mut mask = vec![255; mask_needed];
        for y in 0..height {
            for x in 0..width {
                set_pixel(&mut image, stride, x, y, [33, 65, 97, 0]);
                mask[y * mask_stride + x] = 8;
            }
        }
        set_pixel(&mut image, stride, 1, 1, [200, 201, 202, 17]);
        mask[mask_stride + 1] = 9;
        let before = image.clone();
        let mask_before = mask.clone();
        let stats = erase_bgra(
            &mut image,
            width as u32,
            height as u32,
            stride,
            &mask,
            mask_stride,
            FillOptions::default(),
        )
        .unwrap();
        assert_eq!(stats.path, FillPath::Ring);
        assert_eq!(stats.requested, 1);
        let mut expected = before;
        set_pixel(&mut expected, stride, 1, 1, [33, 65, 97, 255]);
        assert_eq!(image, expected, "padding 和尾部守卫必须保持逐字节不变");
        assert_eq!(mask, mask_before);
    }
    // 单行没有使用 stride：即使 stride 很大，也只要求真实的一行长度。
    let mut image = [31, 41, 51, 61, 71, 81, 91, 101];
    let stats = erase_bgra(
        &mut image,
        2,
        1,
        usize::MAX,
        &[0, 255],
        usize::MAX,
        FillOptions::default(),
    )
    .unwrap();
    assert_eq!(stats.path, FillPath::Boundary);
    assert_eq!(image, [31, 41, 51, 61, 31, 41, 51, 255]);
}

#[test]
fn local_fill_invalid_input_returns_error_before_changing_any_pixel() {
    // 包括 stride 不足、最后一行不足、长度溢出和像素预算；均不得先处理首行。
    for (width, height, stride, mask_stride, image_len, mask_len) in [
        (3, 2, 11, 3, 24, 6),
        (3, 2, 12, 3, 23, 6),
        (3, 2, 12, 2, 24, 6),
        (3, 2, 12, 3, 24, 5),
        (3, 2, usize::MAX, 3, 24, 6),
        (3, 2, 12, usize::MAX, 24, 6),
        (MAX_FILL_PIXELS as u32 + 1, 1, 0, 0, 24, 6),
        (u32::MAX, u32::MAX, usize::MAX, usize::MAX, 24, 6),
    ] {
        let mut image = vec![0x5a; image_len];
        let before = image.clone();
        let mask = vec![255; mask_len];
        assert!(
            erase_bgra(
                &mut image,
                width,
                height,
                stride,
                &mask,
                mask_stride,
                FillOptions::default(),
            )
            .is_err(),
            "无效布局 {width}×{height}, stride={stride}, mask_stride={mask_stride}"
        );
        assert_eq!(image, before);
    }
}
