//! 可选外部字体负例：真实字体像素不能被拉平；不加载检测或识别模型。
use super::*;
use serde_json::Value;
use std::path::Path;

#[test]
#[ignore = "需要显式提供含四张旋转字体图与独立掩码的 manifest"]
fn actual_font_rotations_pass_ink_qualification_but_keep_their_angle() {
    let manifest_path =
        std::env::var("RUSTSHOT_INK_FIXTURES").expect("需设置 RUSTSHOT_INK_FIXTURES");
    let manifest_path = Path::new(&manifest_path);
    let root = manifest_path.parent().unwrap();
    let manifest: Value = serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    let cases = manifest["cases"].as_array().unwrap();
    let selected = [
        "rotate-light-m3",
        "rotate-light-p3",
        "rotate-dark-m3",
        "rotate-dark-p3",
    ];
    let mut executed = 0;
    for id in selected {
        let matches: Vec<_> = cases
            .iter()
            .filter(|case| case["id"].as_str() == Some(id))
            .collect();
        assert_eq!(matches.len(), 1, "每个指定字体样例须唯一存在：{id}");
        let case = matches[0];
        assert_eq!(case["source"]["rotation"].as_f64().unwrap().abs(), 3.);
        let image = crate::export::read_image(&root.join(case["image"].as_str().unwrap())).unwrap();
        let mask = crate::export::read_image(&root.join(case["mask"].as_str().unwrap())).unwrap();
        assert_eq!((image.width, image.height), (mask.width, mask.height));
        let (mut left, mut top, mut right, mut bottom) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut pixels = 0u64;
        for y in 0..mask.height {
            for x in 0..mask.width {
                if mask.bgra[((y * mask.width + x) * 4) as usize] != 0 {
                    pixels += 1;
                    left = left.min(x);
                    right = right.max(x + 1);
                    top = top.min(y);
                    bottom = bottom.max(y + 1);
                }
            }
        }
        assert!(
            pixels > 0 && right > left && bottom > top,
            "字体掩码不能为空：{id}"
        );
        assert_eq!(Some(pixels), case["truthPixelCount"].as_u64());
        let original = TextGeometry {
            quad: std::array::from_fn(|i| Point {
                x: case["detectorBox"][i]["x"].as_f64().unwrap() as f32,
                y: case["detectorBox"][i]["y"].as_f64().unwrap() as f32,
            }),
        };
        let original = Projection::new(original).unwrap();
        let a = original.point(0., 0.5);
        let b = original.point(1., 0.5);
        // 取约2.5°，避免3°上界的浮点比较先拒绝，掩盖水平性判据的错误。
        let angle = f64::from(b.y - a.y).atan2(f64::from(b.x - a.x)) * 5. / 6.;
        let (sin, cos) = angle.sin_cos();
        let center_x = f64::from(left + right) / 2.;
        let center_y = f64::from(top + bottom) / 2.;
        let width = f64::from(right - left) + 16.;
        let height = f64::from(bottom - top) + 12.;
        let frame = TextGeometry {
            quad: [
                [-width / 2., -height / 2.],
                [width / 2., -height / 2.],
                [width / 2., height / 2.],
                [-width / 2., height / 2.],
            ]
            .map(|[x, y]| Point {
                x: (center_x + x * cos - y * sin) as f32,
                y: (center_y + x * sin + y * cos) as f32,
            }),
        };
        let projection = Projection::new(frame).unwrap();
        let a = projection.point(0., 0.5);
        let b = projection.point(1., 0.5);
        let actual_angle = f64::from(b.y - a.y)
            .atan2(f64::from(b.x - a.x))
            .to_degrees();
        let rise = (b.y - a.y).abs();
        assert!(
            (0.15..3.).contains(&actual_angle.abs()),
            "测试框角度应处于真实拉平候选区：{id}"
        );
        assert!(rise >= 1.5, "测试框必须有可观察斜升：{id}");
        let rgb = RgbImage::from_bgra(&image).unwrap();
        let roi = Roi::new(projection, &rgb).expect("贴近墨迹的测试框须有完整halo");
        assert!(
            line_evidence(projection, roi, &rgb, &mut || Ok(()))
                .unwrap()
                .is_some(),
            "字体样例必须先通过背景、极性、单带与完整pixelcell资格，不能提前空泛回退：{id}"
        );
        assert!(
            ink_alignment::horizontal_geometry(projection, roi, &rgb, &mut || Ok(()))
                .unwrap()
                .is_none(),
            "真实旋转字体不能因为水平量化平台被拉平：{id}"
        );
        println!("font alignment negative id={id} mask_pixels={pixels} frame_width={width:.2} frame_height={height:.2} angle={actual_angle:.4} rise={rise:.4} ink_qualified=true flattened=false");
        executed += 1;
    }
    assert_eq!(executed, 4, "四张真实字体负例都必须执行");
}
