//! 视口固定的小圆按钮不能在滚动成品中重复或被接缝裁成残片。
use super::*;

const CROSS: u32 = 256;
const LENGTH: u32 = 480;
const FRAMES: u32 = 7;
const BUTTON_CROSS: i32 = 220;
const BUTTON_COLOR: [u8; 4] = [216, 97, 15, 255];

fn body_pixel(along: u32, across: u32) -> [u8; 4] {
    let mut value = along.wrapping_mul(2_654_435_761) ^ across.wrapping_mul(2_246_822_519);
    value ^= value >> 16;
    let gray = 24 + (value % 192) as u8;
    [gray, gray, gray, 255]
}

fn button_pixel(along: i32, across: i32, center: i32) -> Option<[u8; 4]> {
    let x = across - BUTTON_CROSS;
    let y = along - center;
    if x * x + y * y <= 18 * 18 {
        Some(BUTTON_COLOR)
    } else if (x - 2) * (x - 2) + (y - 3) * (y - 3) <= 21 * 21 {
        Some([12, 12, 12, 255])
    } else {
        None
    }
}

fn page(axis: ScrollAxis, start: u32, length: u32, buttons: &[i32]) -> Image {
    let (width, height) = axis.dimensions(CROSS, length);
    let mut image = Image {
        width,
        height,
        bgra: vec![0; width as usize * height as usize * 4],
        readback_ms: 0.,
    };
    for y in 0..height {
        for x in 0..width {
            let (along, across) = match axis {
                ScrollAxis::Vertical => (y, x),
                ScrollAxis::Horizontal => (x, y),
            };
            let world = start + along;
            let color = buttons
                .iter()
                .find_map(|&center| button_pixel(world as i32, across as i32, center))
                .unwrap_or_else(|| body_pixel(world, across));
            let index = ((y * width + x) * 4) as usize;
            image.bgra[index..index + 4].copy_from_slice(&color);
        }
    }
    image
}

fn mismatch_pixels(actual: &Image, expected: &Image) -> usize {
    actual
        .bgra
        .chunks_exact(4)
        .zip(expected.bgra.chunks_exact(4))
        .filter(|(actual, expected)| actual != expected)
        .count()
}

fn page_with_borders(
    axis: ScrollAxis,
    start: u32,
    length: u32,
    buttons: &[i32],
    borders: (u32, u32),
) -> Image {
    let mut image = page(axis, start, length, buttons);
    for y in 0..image.height {
        for x in 0..image.width {
            let along = match axis {
                ScrollAxis::Vertical => y,
                ScrollAxis::Horizontal => x,
            };
            let gray = if along < borders.0 {
                48
            } else if along >= length - borders.1 {
                80
            } else {
                continue;
            };
            let index = ((y * image.width + x) * 4) as usize;
            image.bgra[index..index + 4].copy_from_slice(&[gray, gray, gray, 255]);
        }
    }
    image
}

fn assert_single_button(
    actual: &Image,
    axis: ScrollAxis,
    starts: &[u32],
    center: i32,
    borders: (u32, u32),
) {
    let min = *starts.iter().min().unwrap();
    let max = *starts.iter().max().unwrap();
    let length = LENGTH + max - min;
    assert_eq!(
        (actual.width, actual.height),
        axis.dimensions(CROSS, length)
    );
    let clean = page_with_borders(axis, min, length, &[], borders);
    // 首尾背景可能从未露出，允许保留任意一个完整按钮，也允许全部移除。
    let mut closest = mismatch_pixels(actual, &clean);
    for &start in starts {
        let expected = page_with_borders(axis, min, length, &[start as i32 + center], borders);
        closest = closest.min(mismatch_pixels(actual, &expected));
    }
    let marker_pixels = actual
        .bgra
        .chunks_exact(4)
        .filter(|pixel| *pixel == BUTTON_COLOR)
        .count();
    assert_eq!(
        closest, 0,
        "axis={axis:?}, starts={starts:?}, button={center}, borders={borders:?}: 允许至多一个完整按钮，仍有 {closest} 个错误像素；标记色像素={marker_pixels}"
    );
}

fn check_button_sequence(center: i32, distances: &[u32], borders: (u32, u32)) {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for reverse in [false, true] {
            let starts: Vec<u32> = distances
                .iter()
                .map(|&distance| {
                    if reverse {
                        distances.last().unwrap() - distance
                    } else {
                        distance
                    }
                })
                .collect();
            let mut stitch = Stitcher::with_options(
                page_with_borders(
                    axis,
                    starts[0],
                    LENGTH,
                    &[starts[0] as i32 + center],
                    borders,
                ),
                StitchOptions {
                    axis,
                    detect_fixed_borders: true,
                    ..Default::default()
                },
            )
            .unwrap();
            for (index, &start) in starts.iter().enumerate().skip(1) {
                let update = stitch
                    .process(page_with_borders(
                        axis,
                        start,
                        LENGTH,
                        &[start as i32 + center],
                        borders,
                    ))
                    .unwrap();
                assert_eq!(
                    (update.kind, update.delta),
                    (UpdateKind::Added, start as i32 - starts[index - 1] as i32),
                    "axis={axis:?}, reverse={reverse}, button={center}, start={start}: {:?}",
                    update.reason
                );
                assert_single_button(&stitch.image(), axis, &starts[..=index], center, borders);
            }
        }
    }
}

fn check_fixed_button(center: i32, step: u32) {
    let distances: Vec<_> = (0..FRAMES).map(|frame| frame * step).collect();
    check_button_sequence(center, &distances, (0, 0));
}

#[test]
fn stitch_floating_lower_button_does_not_repeat_or_split() {
    check_fixed_button(408, 72);
}

#[test]
fn stitch_floating_middle_button_does_not_repeat_or_split() {
    check_fixed_button(240, 72);
}

#[test]
fn stitch_floating_upper_button_does_not_repeat_or_split() {
    check_fixed_button(72, 72);
}

#[test]
fn stitch_floating_small_steps_do_not_leave_button_fragments() {
    check_fixed_button(408, 16);
}

#[test]
fn stitch_floating_acceleration_keeps_one_button_at_every_step() {
    check_button_sequence(408, &[0, 16, 32, 48, 64, 80, 96, 168, 240, 312], (0, 0));
}

#[test]
fn stitch_floating_acceleration_with_fixed_borders_preserves_all_content() {
    check_button_sequence(408, &[0, 16, 32, 48, 64, 80, 96, 168, 240, 312], (24, 20));
}

#[test]
fn stitch_floating_repeated_document_icons_remain_content() {
    let step = 72;
    let length = LENGTH + (FRAMES - 1) * step;
    // 步长恰好等于图标间距，多个图标会连续出现在同一视口坐标；
    // 它们按正文位移也完全匹配，不能仅凭同位模板稳定就删除。
    let buttons: Vec<i32> = (0..length.div_ceil(step))
        .map(|index| (index * step + 36) as i32)
        .collect();
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for reverse in [false, true] {
            let starts: Vec<u32> = (0..FRAMES)
                .map(|frame| {
                    if reverse {
                        (FRAMES - 1 - frame) * step
                    } else {
                        frame * step
                    }
                })
                .collect();
            let mut stitch = Stitcher::with_options(
                page(axis, starts[0], LENGTH, &buttons),
                StitchOptions {
                    axis,
                    detect_fixed_borders: true,
                    ..Default::default()
                },
            )
            .unwrap();
            for &start in &starts[1..] {
                let update = stitch.process(page(axis, start, LENGTH, &buttons)).unwrap();
                assert_eq!(
                    (update.kind, update.delta),
                    (
                        UpdateKind::Added,
                        if reverse { -(step as i32) } else { step as i32 }
                    ),
                    "axis={axis:?}, reverse={reverse}, start={start}: {:?}",
                    update.reason
                );
            }
            let actual = stitch.image();
            let expected = page(axis, 0, length, &buttons);
            assert_eq!(
                (actual.width, actual.height),
                (expected.width, expected.height)
            );
            assert_eq!(
                mismatch_pixels(&actual, &expected),
                0,
                "正文重复图标不能被当成悬浮物：axis={axis:?}, reverse={reverse}"
            );
        }
    }
}

#[test]
fn stitch_floating_undo_restores_original_button_pixels_and_snapshot() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        for reverse in [false, true] {
            let (initial_start, next_start) = if reverse { (72, 0) } else { (0, 72) };
            let mut stitch = Stitcher::with_options(
                page(axis, initial_start, LENGTH, &[initial_start as i32 + 408]),
                StitchOptions {
                    axis,
                    detect_fixed_borders: true,
                    ..Default::default()
                },
            )
            .unwrap();
            let before = stitch.snapshot();
            let before_pixels = stitch.image();
            let update = stitch
                .process(page(axis, next_start, LENGTH, &[next_start as i32 + 408]))
                .unwrap();
            assert_eq!(update.kind, UpdateKind::Added, "{:?}", update.reason);
            assert!(stitch.undo());
            let after = stitch.snapshot();
            assert_eq!(
                (after.width, after.height, after.viewport, after.axis),
                (before.width, before.height, before.viewport, before.axis)
            );
            assert_eq!(after.segments.len(), before.segments.len());
            for (actual, original) in after.segments.iter().zip(&before.segments) {
                assert_eq!(
                    (actual.source, actual.origin),
                    (original.source, original.origin)
                );
                assert!(std::sync::Arc::ptr_eq(&actual.image, &original.image));
            }
            assert_eq!(mismatch_pixels(&stitch.image(), &before_pixels), 0);
        }
    }
}
