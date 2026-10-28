use super::*;

fn tilted(width: f64, height: f64, degrees: f64) -> TextGeometry {
    let (s, c) = degrees.to_radians().sin_cos();
    TextGeometry {
        quad: [
            [-width / 2., -height / 2.],
            [width / 2., -height / 2.],
            [width / 2., height / 2.],
            [-width / 2., height / 2.],
        ]
        .map(|[x, y]| Point {
            x: (140. + x * c - y * s) as f32,
            y: (90. + x * s + y * c) as f32,
        }),
    }
}

fn white() -> RgbImage {
    RgbImage {
        width: 280,
        height: 190,
        pixels: vec![[255.; 3]; 280 * 190],
    }
}

fn screen_ink(
    rgb: &mut RgbImage,
    geometry: TextGeometry,
    fraction: [f64; 2],
    ys: std::ops::Range<usize>,
) {
    let p = Projection::new(geometry).unwrap();
    let left = p.point(0., 0.5).x.min(p.point(1., 0.5).x) as f64;
    let right = p.point(0., 0.5).x.max(p.point(1., 0.5).x) as f64;
    let start = (left + (right - left) * fraction[0]).ceil() as usize;
    let end = (left + (right - left) * fraction[1]).floor() as usize;
    for y in ys {
        for x in start..end {
            rgb.pixels[y * rgb.width + x] = [0.; 3];
        }
    }
}

fn slanted_ink(rgb: &mut RgbImage, geometry: TextGeometry, vertical: [f64; 2]) {
    let p = Projection::new(geometry).unwrap();
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            let [u, v] = p
                .inverse(Point {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                })
                .unwrap();
            if (0.08..=0.92).contains(&u) && (vertical[0]..=vertical[1]).contains(&v) {
                rgb.pixels[y * rgb.width + x] = [0.; 3];
            }
        }
    }
}

fn align(geometry: TextGeometry, rgb: &RgbImage) -> Option<TextGeometry> {
    let p = Projection::new(geometry).unwrap();
    horizontal_geometry(p, Roi::new(p, rgb).unwrap(), rgb, &mut || Ok(())).unwrap()
}

fn assert_covered(geometry: TextGeometry, rgb: &RgbImage) {
    let p = Projection::new(geometry).unwrap();
    for y in 0..rgb.height {
        for x in 0..rgb.width {
            if rgb.pixels[y * rgb.width + x] == [255.; 3] {
                continue;
            }
            for (x, y) in [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1)] {
                let [u, v] = p
                    .inverse(Point {
                        x: x as f32,
                        y: y as f32,
                    })
                    .unwrap();
                assert!((0. ..=1.).contains(&u) && (0. ..=1.).contains(&v));
            }
        }
    }
}

#[test]
fn horizontal_pixels_flatten_a_slanted_detection_box_and_preserve_reading_x() {
    for degrees in [-2.4, 2.4] {
        for reverse in [false, true] {
            let mut original = tilted(200., 40., degrees);
            if reverse {
                original.quad.rotate_left(2);
            }
            let mut rgb = white();
            screen_ink(&mut rgb, original, [0.08, 0.92], 80..100);
            let aligned = align(original, &rgb).expect("横向各分区上下沿一致，应有水平证据");
            let before = Projection::new(original).unwrap();
            let after = Projection::new(aligned).unwrap();
            assert_eq!(aligned.quad[0].y, aligned.quad[1].y);
            assert_eq!(aligned.quad[2].y, aligned.quad[3].y);
            assert_eq!(before.direction(), after.direction());
            for u in [0., 0.17, 0.5, 0.83, 1.] {
                assert!((before.point(u, 0.5).x - after.point(u, 0.5).x).abs() <= 0.0001);
            }
            assert_covered(aligned, &rgb);
        }
    }
}

#[test]
fn real_positive_and_negative_one_to_three_degree_ink_stays_slanted() {
    for degrees in [-3., -2., -1., 1., 2., 3.] {
        let original = tilted(200., 40., degrees);
        let mut rgb = white();
        slanted_ink(&mut rgb, original, [0.30, 0.70]);
        assert!(
            align(original, &rgb).is_none(),
            "真实 {degrees}° 笔画不能拉平"
        );
    }
    // 两字大小的窄框，量化后的上下沿可能只有 1–2px 总斜升。
    for degrees in [-1.5, 1.5, -2.4, 2.4] {
        let original = tilted(70., 46., degrees);
        let mut rgb = white();
        slanted_ink(&mut rgb, original, [0.22, 0.78]);
        assert!(
            align(original, &rgb).is_none(),
            "窄框真斜字 {degrees}° 仍需保留"
        );
    }
}

#[test]
fn real_sub_degree_ink_keeps_its_angle_instead_of_following_quantized_platforms() {
    for degrees in [-0.75, -0.6, -0.5, -0.43, 0.43, 0.5, 0.6, 0.75] {
        let original = tilted(200., 40., degrees);
        let mut rgb = white();
        slanted_ink(&mut rgb, original, [0.30, 0.70]);
        assert!(
            align(original, &rgb).is_none(),
            "真实 {degrees}° 笔画的量化平台不能被当作水平证据"
        );
    }
}

#[test]
fn one_common_edge_can_outvote_the_detector_angle_despite_varied_other_edge() {
    let original = tilted(200., 48., 2.4);
    for baseline in [true, false] {
        let mut rgb = white();
        // 仅固定一侧的公共边缘；另一侧模拟字母升降部，不要求全行等高矩形。
        for x in 58..222 {
            let top = if baseline { 76 + (x / 7) % 6 } else { 80 };
            let bottom = if baseline { 100 } else { 100 + (x / 9) % 5 };
            for y in top..bottom {
                rgb.pixels[y * rgb.width + x] = [0.; 3];
            }
        }
        let aligned = align(original, &rgb).expect("一侧宽分布公共边缘已明显更支持水平");
        assert_covered(aligned, &rgb);
    }
}

#[test]
fn actual_ink_height_allows_horizontal_two_character_sized_pixels_in_a_padded_box() {
    let original = tilted(70., 46., 2.4);
    let mut rgb = white();
    screen_ink(&mut rgb, original, [0.08, 0.92], 77..103);
    let aligned = align(original, &rgb).expect("应使用实际墨迹高26，而不是原框高46");
    assert_covered(aligned, &rgb);
}

#[test]
fn weak_antialiasing_outside_strong_edges_is_kept_as_complete_pixel_cells() {
    let original = tilted(200., 48., 2.4);
    let mut rgb = white();
    screen_ink(&mut rgb, original, [0.08, 0.92], 80..100);
    rgb.pixels[77 * rgb.width + 125] = [254.; 3];
    rgb.pixels[103 * rgb.width + 165] = [254.; 3];
    let aligned = align(original, &rgb).unwrap();
    assert!(aligned.quad[0].y <= 76. && aligned.quad[2].y >= 105.);
    assert_covered(aligned, &rgb);
}

#[test]
fn insufficient_distribution_single_character_noise_and_tiny_slope_keep_original() {
    let original = tilted(200., 40., 2.4);
    for extent in [[0.4, 0.6], [0.3, 0.9]] {
        let mut rgb = white();
        screen_ink(&mut rgb, original, extent, 80..100);
        assert!(align(original, &rgb).is_none());
    }
    let mut noise = white();
    for x in (52..228).step_by(6) {
        noise.pixels[90 * noise.width + x] = [0.; 3];
    }
    assert!(align(original, &noise).is_none());
    let tiny = tilted(200., 40., 0.1);
    let mut rgb = white();
    screen_ink(&mut rgb, tiny, [0.08, 0.92], 80..100);
    assert!(align(tiny, &rgb).is_none());
}

#[test]
fn perspective_and_ink_outside_fixed_midline_x_keep_original() {
    let perspective = TextGeometry {
        quad: [
            Point { x: 40., y: 60. },
            Point { x: 240., y: 63. },
            Point { x: 234., y: 113. },
            Point { x: 46., y: 100. },
        ],
    };
    let mut rgb = white();
    screen_ink(&mut rgb, perspective, [0.08, 0.92], 80..100);
    assert!(align(perspective, &rgb).is_none());
    let original = tilted(200., 48., 2.4);
    let mut rgb = white();
    screen_ink(&mut rgb, original, [0.08, 0.92], 80..100);
    let p = Projection::new(original).unwrap();
    let x = p.point(0., 0.5).x.floor() as usize;
    rgb.pixels[90 * rgb.width + x] = [254.; 3];
    assert!(
        align(original, &rgb).is_none(),
        "固定x边界之外的淡像素也不能丢掉"
    );
}

#[test]
fn cancellation_propagates_while_scanning_pixels() {
    let original = tilted(200., 40., 2.4);
    let mut rgb = white();
    screen_ink(&mut rgb, original, [0.08, 0.92], 80..100);
    let p = Projection::new(original).unwrap();
    let mut calls = 0;
    let result = horizontal_geometry(p, Roi::new(p, &rgb).unwrap(), &rgb, &mut || {
        calls += 1;
        if calls == 3 {
            Err("拉平像素检查取消".into())
        } else {
            Ok(())
        }
    });
    assert_eq!(result.unwrap_err().to_string(), "拉平像素检查取消");
}
