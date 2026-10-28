//! 真实滚动计划与像素约束的回归；只使用拼接器公开接口，不依赖旧匹配实现。
use super::*;

fn options(axis: ScrollAxis) -> StitchOptions {
    StitchOptions {
        axis,
        detect_fixed_borders: true,
        ..Default::default()
    }
}

// 逐行唯一的内容像素，包含高位行号避免 256 行周期。
fn content_pixel(row: u32, x: u32) -> [u8; 4] {
    let mut hash = row.wrapping_mul(2_654_435_761);
    hash ^= hash >> 16;
    [
        (row.wrapping_mul(3) + x * 17 + ((hash >> 16) & 255)) as u8,
        (row.wrapping_mul(13) + x * 5 + ((hash >> 8) & 255)) as u8,
        (row.wrapping_mul(7) + x * 11 + (hash & 255)) as u8,
        255,
    ]
}

fn viewport(axis: ScrollAxis, cross: u32, length: u32, start: u32, head: u32, tail: u32) -> Image {
    let (width, height) = axis.dimensions(cross, length);
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
            let color = if along < head || along >= length - tail {
                [40, 40, 40, 255]
            } else {
                content_pixel(start + along - head, across)
            };
            let index = ((y * width + x) * 4) as usize;
            image.bgra[index..index + 4].copy_from_slice(&color);
        }
    }
    image
}

fn assert_pixels(actual: &Image, expected: &Image) {
    assert_eq!(
        (actual.width, actual.height),
        (expected.width, expected.height)
    );
    if actual.bgra != expected.bgra {
        let byte = actual
            .bgra
            .iter()
            .zip(&expected.bgra)
            .position(|(a, b)| a != b)
            .unwrap();
        panic!(
            "像素不同：x={} y={} channel={}",
            byte / 4 % actual.width as usize,
            byte / 4 / actual.width as usize,
            byte % 4
        );
    }
}

#[test]
fn stitch_regression_large_overlap_small_scroll_continues_past_two_frames_with_full_viewport() {
    // 大重叠、小步滚动时仍持续延长。
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut stitch =
            Stitcher::with_options(viewport(axis, 180, 800, 0, 40, 30), options(axis)).unwrap();
        for frame in 1..15 {
            let top = frame * 25;
            let update = stitch
                .process(viewport(axis, 180, 800, top, 40, 30))
                .unwrap();
            assert_eq!(
                (update.kind, update.delta),
                (UpdateKind::Added, 25),
                "frame={frame}, {:?}",
                update.reason
            );
            assert_eq!(stitch.snapshot().viewport, axis.rect(top, 800, 180));
        }
        assert_pixels(&stitch.image(), &viewport(axis, 180, 1150, 0, 40, 30));
    }
}

#[test]
fn stitch_regression_consecutive_up_scroll_uses_trusted_short_overlap_prior() {
    // 连续向上滚动时建立稳定的短重叠先验。
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let mut stitch =
            Stitcher::with_options(viewport(axis, 64, 100, 0, 0, 0), options(axis)).unwrap();
        for top in [60, 120, 180, 240, 300, 240, 180, 111] {
            let update = stitch.process(viewport(axis, 64, 100, top, 0, 0)).unwrap();
            assert!(
                !matches!(update.kind, UpdateKind::Rejected | UpdateKind::Limit),
                "top={top}, {:?}",
                update.reason
            );
            assert_eq!(stitch.snapshot().viewport, axis.rect(top, 100, 64));
            if top == 111 {
                assert_eq!(update.kind, UpdateKind::ViewportOnly);
            }
        }
        assert_pixels(&stitch.image(), &viewport(axis, 64, 400, 0, 0, 0));
    }
}

fn mix(value: u32) -> u32 {
    let mut value = value.wrapping_mul(2_654_435_761);
    value ^= value >> 16;
    value = value.wrapping_mul(2_246_822_519);
    value ^ (value >> 13)
}

// 分块纹理页，模拟快速滚动的真实内容。
fn texture_page(width: u32, length: u32, start: u32, seed: u32) -> Image {
    let mut image = Image {
        width,
        height: length,
        bgra: vec![0; width as usize * length as usize * 4],
        readback_ms: 0.,
    };
    for y in 0..length {
        for x in 0..width {
            let value = mix(seed + (start + y) * 127 + x / (width / 32).max(1));
            let index = ((y * width + x) * 4) as usize;
            image.bgra[index..index + 4].copy_from_slice(&[
                value as u8,
                (value >> 8) as u8,
                (value >> 16) as u8,
                255,
            ]);
        }
    }
    image
}

#[test]
fn stitch_regression_acceleration_to_short_exact_overlap_preserves_whole_page() {
    // 加速到短的精确重叠时仍保留整页。
    let mut stitch = Stitcher::with_options(
        texture_page(960, 640, 0, 781),
        options(ScrollAxis::Vertical),
    )
    .unwrap();
    for top in [120, 280, 720, 1160, 1640] {
        let update = stitch.process(texture_page(960, 640, top, 781)).unwrap();
        assert!(
            !matches!(update.kind, UpdateKind::Rejected | UpdateKind::Limit),
            "top={top}, {:?}",
            update.reason
        );
        assert_eq!(stitch.snapshot().viewport.y, top);
    }
    assert_pixels(&stitch.image(), &texture_page(960, 2280, 0, 781));
}

#[test]
fn stitch_regression_complete_gap_and_corrupt_frame_do_not_poison_recovery_or_undo() {
    // 可信运动后的完整空档、未捕获空档与损坏帧。
    let mut stitch = Stitcher::with_options(
        texture_page(960, 640, 0, 781),
        options(ScrollAxis::Vertical),
    )
    .unwrap();
    for top in [120, 280] {
        assert_eq!(
            stitch
                .process(texture_page(960, 640, top, 781))
                .unwrap()
                .kind,
            UpdateKind::Added
        );
    }
    let trusted = stitch.image();
    let trusted_viewport = stitch.snapshot().viewport;
    for top in [1080, 1240, 1560] {
        let update = stitch.process(texture_page(960, 640, top, 781)).unwrap();
        assert_eq!(update.kind, UpdateKind::Rejected, "gap={top}");
        assert_pixels(&stitch.image(), &trusted);
        assert_eq!(stitch.snapshot().viewport, trusted_viewport);
        assert!(stitch.can_undo());
    }
    assert_eq!(
        stitch
            .process(texture_page(960, 640, 0, 9349))
            .unwrap()
            .kind,
        UpdateKind::Rejected
    );
    let recovery = stitch.process(texture_page(960, 640, 480, 781)).unwrap();
    assert_eq!(
        (recovery.kind, recovery.delta),
        (UpdateKind::Added, 200),
        "{:?}",
        recovery.reason
    );
    assert_eq!(stitch.snapshot().viewport.y, 480);
    assert_pixels(&stitch.image(), &texture_page(960, 1120, 0, 781));
    assert!(stitch.undo());
    assert_pixels(&stitch.image(), &trusted);
    assert_eq!(stitch.snapshot().viewport, trusted_viewport);
    assert_eq!(
        stitch
            .process(texture_page(960, 640, 480, 781))
            .unwrap()
            .delta,
        200
    );
}

fn text_gray(world: u32, x: u32) -> u8 {
    let row = world % 29;
    if !(3..21).contains(&row) || !(8..328).contains(&x) || (x - 8) % 10 >= 7 {
        return 255;
    }
    let bits = mix(world / 29 * 131 + (x - 8) / 10 * 17 + row / 2);
    let bit = (x - 8) % 10;
    if bits & (1 << bit) != 0 {
        20
    } else if bits & (1 << ((bit + 1) % 7)) != 0 {
        150
    } else {
        255
    }
}

fn text_page(start: u32, length: u32, aa_change: i16) -> Image {
    let width = 480;
    let mut image = Image {
        width,
        height: length,
        bgra: vec![255; width as usize * length as usize * 4],
        readback_ms: 0.,
    };
    for y in 0..length {
        for x in 0..width {
            let gray = text_gray(start + y, x);
            let gray = if gray == 150 {
                (gray as i16 + aa_change) as u8
            } else {
                gray
            };
            let index = ((y * width + x) * 4) as usize;
            image.bgra[index..index + 3].fill(gray);
        }
    }
    image
}

#[test]
fn stitch_regression_aa_text_continues_multiple_frames_without_losing_stable_ink_or_blank_rows() {
    // 文字页滚动计划，补充跨帧字体 AA 边缘 ±2 级扰动。
    let mut stitch =
        Stitcher::with_options(text_page(0, 360, 0), options(ScrollAxis::Vertical)).unwrap();
    for (index, top) in [60, 120, 180, 240, 300, 360, 420].into_iter().enumerate() {
        let update = stitch
            .process(text_page(top, 360, if index % 2 == 0 { 2 } else { -2 }))
            .unwrap();
        assert_eq!(
            (update.kind, update.delta),
            (UpdateKind::Added, 60),
            "top={top}, {:?}",
            update.reason
        );
        assert_eq!(stitch.snapshot().viewport.y, top);
    }
    let actual = stitch.image();
    assert_eq!((actual.width, actual.height), (480, 780));
    for y in 0..actual.height {
        for x in 0..actual.width {
            let expected = text_gray(y, x);
            if expected != 150 {
                let index = ((y * actual.width + x) * 4) as usize;
                assert_eq!(
                    &actual.bgra[index..index + 4],
                    &[expected, expected, expected, 255],
                    "x={x},y={y}"
                );
            }
        }
    }
}

/// 可选的真实用户长截图样本，由环境变量指定；未设置时跳过。
fn user_fixture() -> Option<Image> {
    let Some(path) = std::env::var_os("RUSTSHOT_LONG_FIXTURE").map(std::path::PathBuf::from) else {
        eprintln!("跳过可选样本：未设置 RUSTSHOT_LONG_FIXTURE（长列表 PNG 路径）。");
        return None;
    };
    let file = std::fs::File::open(&path)
        .unwrap_or_else(|error| panic!("读取长截图样本失败：{}，{error}", path.display()));
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().unwrap();
    let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut decoded).unwrap();
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => panic!("PNG palette 未展开"),
    };
    let mut bgra = Vec::with_capacity(info.width as usize * info.height as usize * 4);
    for pixel in decoded[..info.buffer_size()].chunks_exact(channels) {
        bgra.extend_from_slice(&match channels {
            1 => [pixel[0], pixel[0], pixel[0], 255],
            2 => [pixel[0], pixel[0], pixel[0], pixel[1]],
            3 => [pixel[2], pixel[1], pixel[0], 255],
            4 => [pixel[2], pixel[1], pixel[0], pixel[3]],
            _ => unreachable!(),
        });
    }
    Some(Image {
        width: info.width,
        height: info.height,
        bgra,
        readback_ms: 0.,
    })
}

fn crop(source: &Image, top: u32, height: u32) -> Image {
    Image {
        width: source.width,
        height,
        bgra: source.bgra[top as usize * source.width as usize * 4
            ..(top + height) as usize * source.width as usize * 4]
            .to_vec(),
        readback_ms: 0.,
    }
}

#[test]
#[ignore = "需要本地长列表 PNG：设置 RUSTSHOT_LONG_FIXTURE 后用 --ignored 执行"]
fn stitch_regression_user_report_png_square_viewports_keep_full_source_height_and_pixels() {
    // 真实用户长列表按多组方形视口滚动，结果须保留完整高度与像素。
    let Some(source) = user_fixture() else {
        return;
    };
    for (height, step) in [(515, 300), (515, 250), (592, 300)] {
        assert!(source.height >= height);
        let maximum = source.height - height;
        let mut positions: Vec<u32> = (0..=maximum).step_by(step as usize).collect();
        if positions.last().copied() != Some(maximum) {
            positions.push(maximum);
        }
        let mut stitch =
            Stitcher::with_options(crop(&source, 0, height), options(ScrollAxis::Vertical))
                .unwrap();
        let duplicate = stitch.process(crop(&source, 0, height)).unwrap();
        assert_eq!(duplicate.kind, UpdateKind::Unchanged);
        for top in positions.into_iter().skip(1) {
            let update = stitch.process(crop(&source, top, height)).unwrap();
            assert!(
                !matches!(update.kind, UpdateKind::Rejected | UpdateKind::Limit),
                "viewport={height},step={step},top={top}: {:?}",
                update.reason
            );
            assert_eq!(stitch.snapshot().viewport.y, top);
            assert!(stitch.height <= source.height, "不能直接整帧重复追加");
        }
        assert_pixels(&stitch.image(), &source);
    }
}
