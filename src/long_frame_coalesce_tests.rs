//! 独立全局坐标像素 oracle：删除 middle 后，保留端点必须覆盖其所有字节。
use super::{exact_coalesce, prove};
use crate::capture::Image;
use std::time::Duration;

fn texture(width: u32, height: u32, origin: u32) -> Image {
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in origin..origin + height {
        for x in 0..width {
            let value = y.wrapping_mul(2_654_435_761) ^ x.wrapping_mul(2_246_822_519);
            // 首个像素编码独立全局行号；其它像素不是整行纯色。
            bgra.extend_from_slice(&[
                if x == 0 { y as u8 } else { value as u8 },
                if x == 0 {
                    (y >> 8) as u8
                } else {
                    (value >> 8) as u8
                },
                (value >> 16) as u8,
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

fn middle_is_preserved(frames: [&Image; 3], starts: [i64; 3]) -> bool {
    let [first, middle, last] = frames;
    let row_bytes = middle.width as usize * 4;
    for y in 0..middle.height as usize {
        let world_y = starts[1] + y as i64;
        for byte in 0..row_bytes {
            let expected = middle.bgra[y * row_bytes + byte];
            let mut covered = false;
            for (image, origin) in [(first, starts[0]), (last, starts[2])] {
                let local_y = world_y - origin;
                if local_y < 0 || local_y >= i64::from(image.height) {
                    continue;
                }
                covered = true;
                if image.bgra[local_y as usize * row_bytes + byte] != expected {
                    return false;
                }
            }
            if !covered {
                return false;
            }
        }
    }
    true
}

#[test]
fn exact_coalesce_forward_reverse_zero_and_endpoint_duplicates_preserve_middle() {
    for starts in [
        [120, 140, 160],
        [160, 140, 120],
        [120, 120, 120],
        [120, 120, 160],
        [120, 160, 160],
    ] {
        let frames = starts.map(|origin| texture(64, 160, origin));
        assert!(middle_is_preserved(
            [&frames[0], &frames[1], &frames[2]],
            starts.map(i64::from),
        ));
        assert!(
            exact_coalesce(&frames[0], &frames[1], &frames[2]),
            "{starts:?}"
        );
    }
}

#[test]
fn exact_coalesce_forty_percent_is_inclusive_but_one_more_pixel_is_rejected() {
    for direction in [1_i64, -1] {
        for end in [40, 41] {
            let starts = [100, 100 + 20 * direction, 100 + end * direction];
            let frames = starts.map(|origin| texture(64, 100, origin as u32));
            assert!(middle_is_preserved(
                [&frames[0], &frames[1], &frames[2]],
                starts
            ));
            assert_eq!(
                exact_coalesce(&frames[0], &frames[1], &frames[2]),
                end == 40
            );
        }
    }
}

#[test]
fn exact_coalesce_sparse_middle_pixels_and_alpha_are_not_hidden_by_hash() {
    for (row, channel) in [(2, 0), (70, 1), (150, 3)] {
        let first = texture(64, 160, 100);
        let mut middle = texture(64, 160, 120);
        let last = texture(64, 160, 140);
        // x=1 不在生产粗筛 hash 的采样位置；整行/重叠必须仍能发现。
        let byte = (row * 64 + 1) * 4 + channel;
        middle.bgra[byte] ^= 0x5a;
        assert!(!middle_is_preserved(
            [&first, &middle, &last],
            [100, 120, 140]
        ));
        assert!(!exact_coalesce(&first, &middle, &last));
    }
    let first = texture(64, 160, 100);
    let middle = texture(64, 160, 120);
    let mut last = texture(64, 160, 140);
    last.bgra[(40 * 64 + 1) * 4 + 2] ^= 0x31;
    assert!(!exact_coalesce(&first, &middle, &last));
}

#[test]
fn exact_coalesce_hash_collisions_periodic_and_flat_moving_frames_use_fifo() {
    let collision = |origin| {
        let mut image = texture(64, 160, origin);
        image
            .bgra
            .chunks_exact_mut(4)
            .for_each(|pixel| pixel.copy_from_slice(&[30, 50, 70, 255]));
        for y in 0..160_usize {
            // 所有粗筛采样像素相同，仅未采样列含唯一全局行号。
            let row = origin + y as u32;
            let offset = (y * 64 + 1) * 4;
            image.bgra[offset] = row as u8;
            image.bgra[offset + 1] = (row >> 8) as u8;
        }
        image
    };
    let [first, middle, last] = [100, 120, 140].map(collision);
    assert!(middle_is_preserved(
        [&first, &middle, &last],
        [100, 120, 140]
    ));
    assert!(
        !exact_coalesce(&first, &middle, &last),
        "碰撞不能冒充唯一锚点"
    );

    let periodic = |origin| {
        let mut image = texture(64, 160, 0);
        for y in 0..160_usize {
            let pattern = ((y as u32 + origin) % 8) as u8;
            image.bgra[y * 64 * 4..(y + 1) * 64 * 4]
                .chunks_exact_mut(4)
                .for_each(|pixel| pixel.copy_from_slice(&[pattern, 90, 120, 255]));
        }
        image
    };
    let [first, middle, last] = [0, 1, 2].map(periodic);
    assert!(!exact_coalesce(&first, &middle, &last));
    // 全等的纯色/周期帧合法走零位移；不同纯色帧不凭任何近似判重。
    assert!(exact_coalesce(&first, &first, &first));
    let mut flat = texture(64, 160, 0);
    flat.bgra.fill(17);
    let mut other = texture(64, 160, 0);
    other.bgra.fill(18);
    assert!(exact_coalesce(&flat, &flat, &flat));
    assert!(!exact_coalesce(&flat, &other, &flat));
    assert!(!exact_coalesce(&flat, &other, &other));
}

#[test]
fn exact_coalesce_fixed_bars_conflicting_anchors_and_foldback_keep_middle() {
    let starts = [100, 120, 140];
    let mut frames = starts.map(|origin| texture(64, 160, origin));
    for image in &mut frames {
        for y in (0..12).chain(148..160) {
            image.bgra[y * 64 * 4..(y + 1) * 64 * 4].fill(75);
        }
    }
    assert!(!exact_coalesce(&frames[0], &frames[1], &frames[2]));

    for starts in [[100, 140, 120], [100, 120, 100], [140, 100, 120]] {
        let frames = starts.map(|origin| texture(64, 160, origin));
        assert!(
            !middle_is_preserved([&frames[0], &frames[1], &frames[2]], starts.map(i64::from),),
            "折返点含保留端点未覆盖的独有尾部"
        );
        assert!(!exact_coalesce(&frames[0], &frames[1], &frames[2]));
    }

    let first = texture(64, 160, 100);
    let mut middle = texture(64, 160, 120);
    let last = texture(64, 160, 140);
    // 将另一幅图的唯一行移入不同位置，制造两种精确行位移证据。
    middle.bgra[75 * 64 * 4..76 * 64 * 4].copy_from_slice(&first.bgra[56 * 64 * 4..57 * 64 * 4]);
    middle.bgra[36 * 64 * 4..37 * 64 * 4].copy_from_slice(&last.bgra[159 * 64 * 4..160 * 64 * 4]);
    assert!(!exact_coalesce(&first, &middle, &last));
}

#[test]
fn exact_coalesce_invalid_frames_and_exhausted_budget_leave_input_untouched() {
    let first = texture(64, 160, 100);
    let middle = texture(64, 160, 120);
    let last = texture(64, 160, 140);
    assert_eq!(prove(&first, &middle, &last, Duration::ZERO), None);
    let saved = first.bgra.clone();
    for invalid in [
        Image {
            width: 0,
            height: 0,
            bgra: Vec::new(),
            readback_ms: 0.,
        },
        Image {
            width: 64,
            height: 160,
            bgra: vec![0; 64 * 160 * 4 - 1],
            readback_ms: 0.,
        },
        Image {
            width: 65,
            height: 160,
            bgra: vec![0; 65 * 160 * 4],
            readback_ms: 0.,
        },
        Image {
            width: 8_000,
            height: 4_001,
            bgra: Vec::new(),
            readback_ms: 0.,
        },
        Image {
            width: u32::MAX,
            height: u32::MAX,
            bgra: Vec::new(),
            readback_ms: 0.,
        },
    ] {
        assert!(!exact_coalesce(&first, &invalid, &last));
        assert!(!exact_coalesce(&invalid, &middle, &last));
    }
    assert_eq!(first.bgra, saved);
}
