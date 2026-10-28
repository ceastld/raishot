use super::{avx2_available, Backend, Conv2d, MAX_OUTPUT_ELEMENTS};

fn depthwise(kernel: usize, stride: usize) -> Conv2d {
    Conv2d {
        batch: 2,
        in_channels: 3,
        height: 9,
        width: 41,
        out_channels: 3,
        kernel_h: kernel,
        kernel_w: kernel,
        stride_h: stride,
        stride_w: stride,
        pad_top: kernel / 2,
        pad_left: kernel / 2,
        pad_bottom: kernel / 2,
        pad_right: kernel / 2,
        groups: 3,
    }
}

fn pointwise() -> Conv2d {
    Conv2d {
        batch: 2,
        in_channels: 9,
        height: 3,
        width: 7,
        out_channels: 7,
        kernel_h: 1,
        kernel_w: 1,
        stride_h: 1,
        stride_w: 1,
        pad_top: 0,
        pad_left: 0,
        pad_bottom: 0,
        pad_right: 0,
        groups: 1,
    }
}

fn values(count: usize, seed: usize) -> Vec<f32> {
    (0..count)
        .map(|index| ((index * 37 + seed * 19) % 211) as f32 / 83.0 - 1.25)
        .collect()
}

/// 独立按输入像素散射累加，使用 f64；不调用被测实现的形状、坐标或逐像素函数。
fn reference(conv: Conv2d, input: &[f32], weights: &[f32], bias: Option<&[f32]>) -> Vec<f64> {
    let oh = (conv.height + conv.pad_top + conv.pad_bottom - conv.kernel_h) / conv.stride_h + 1;
    let ow = (conv.width + conv.pad_left + conv.pad_right - conv.kernel_w) / conv.stride_w + 1;
    let mut output = vec![0.0; conv.batch * conv.out_channels * oh * ow];
    let in_per_group = conv.in_channels / conv.groups;
    let out_per_group = conv.out_channels / conv.groups;
    for batch in 0..conv.batch {
        for oc in 0..conv.out_channels {
            let output_base = (batch * conv.out_channels + oc) * oh * ow;
            output[output_base..output_base + oh * ow]
                .fill(bias.map_or(0.0, |values| values[oc] as f64));
            let first_ic = (oc / out_per_group) * in_per_group;
            for relative_ic in 0..in_per_group {
                let ic = first_ic + relative_ic;
                for iy in 0..conv.height {
                    for ix in 0..conv.width {
                        let value = input
                            [((batch * conv.in_channels + ic) * conv.height + iy) * conv.width + ix]
                            as f64;
                        for ky in 0..conv.kernel_h {
                            let dy = iy as isize + conv.pad_top as isize - ky as isize;
                            if dy < 0 || dy % conv.stride_h as isize != 0 {
                                continue;
                            }
                            let oy = dy as usize / conv.stride_h;
                            if oy >= oh {
                                continue;
                            }
                            for kx in 0..conv.kernel_w {
                                let dx = ix as isize + conv.pad_left as isize - kx as isize;
                                if dx < 0 || dx % conv.stride_w as isize != 0 {
                                    continue;
                                }
                                let ox = dx as usize / conv.stride_w;
                                if ox < ow {
                                    let weight =
                                        weights[((oc * in_per_group + relative_ic) * conv.kernel_h
                                            + ky)
                                            * conv.kernel_w
                                            + kx] as f64;
                                    output[output_base + oy * ow + ox] += value * weight;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    output
}

fn assert_close(actual: &[f32], expected: &[f64], description: &str) {
    assert_eq!(actual.len(), expected.len());
    for (index, (&actual, &expected)) in actual.iter().zip(expected).enumerate() {
        let difference = (actual as f64 - expected).abs();
        let tolerance = 3e-5 * (1.0 + expected.abs());
        assert!(
            difference <= tolerance,
            "{description} index={index} actual={actual} expected={expected} diff={difference} tolerance={tolerance}"
        );
    }
}

fn compare_all_backends(conv: Conv2d, with_bias: bool) {
    let input = values(conv.input_len().unwrap(), 1);
    let weights = values(conv.weights_len().unwrap(), 7);
    let bias = with_bias.then(|| values(conv.out_channels, 13));
    let bias = bias.as_deref();
    let expected = reference(conv, &input, &weights, bias);
    let shape = conv.output_shape().unwrap();
    assert_eq!(shape[0], conv.batch);
    assert_eq!(shape[1], conv.out_channels);
    assert_eq!(shape.iter().product::<usize>(), expected.len());
    for backend in [Backend::Scalar, Backend::Auto, Backend::Avx2] {
        let mut output = vec![f32::NAN; conv.output_len().unwrap()];
        let result = conv.run(&input, &weights, bias, &mut output, backend);
        if backend == Backend::Avx2 && !avx2_available() {
            assert!(result.is_err());
            assert!(output.iter().all(|value| value.is_nan()));
            continue;
        }
        assert_eq!(
            result.unwrap(),
            if backend != Backend::Scalar && avx2_available() {
                Backend::Avx2
            } else {
                Backend::Scalar
            }
        );
        assert_close(&output, &expected, &format!("{backend} {conv:?}"));
    }
}

#[test]
fn kernels_depthwise_3x3_odd_width_padding_and_batch() {
    compare_all_backends(depthwise(3, 1), true);
}

#[test]
fn kernels_depthwise_5x5_odd_width_and_no_bias() {
    compare_all_backends(depthwise(5, 1), false);
}

#[test]
fn kernels_depthwise_stride2_3x3_and_5x5() {
    compare_all_backends(depthwise(3, 2), true);
    compare_all_backends(depthwise(5, 2), false);
}

#[test]
fn kernels_depthwise_asymmetric_padding_and_strides() {
    let mut conv = depthwise(5, 1);
    conv.pad_top = 3;
    conv.pad_left = 4;
    conv.pad_bottom = 0;
    conv.pad_right = 1;
    conv.stride_h = 2;
    compare_all_backends(conv, true);
    conv.stride_h = 1;
    conv.stride_w = 2;
    compare_all_backends(conv, false);
}

#[test]
fn kernels_depthwise_no_padding() {
    let mut conv = depthwise(3, 1);
    conv.pad_top = 0;
    conv.pad_left = 0;
    conv.pad_bottom = 0;
    conv.pad_right = 0;
    compare_all_backends(conv, true);
}

#[test]
fn kernels_depthwise_tiny_input_and_padding_larger_than_kernel() {
    let mut conv = depthwise(5, 2);
    conv.height = 1;
    conv.width = 2;
    conv.pad_top = 9;
    conv.pad_left = 11;
    conv.pad_bottom = 8;
    conv.pad_right = 7;
    compare_all_backends(conv, true);
    conv.pad_top = 2;
    conv.pad_left = 2;
    conv.pad_bottom = 2;
    conv.pad_right = 2;
    compare_all_backends(conv, false);
}

#[test]
fn kernels_masked_edges_match_reference_across_lane_boundaries() {
    for width in [1, 2, 3, 7, 8, 9, 15, 16, 17, 23, 31, 32, 33] {
        for kernel in [3, 5] {
            for stride_h in [1, 2] {
                for stride_w in [1, 2] {
                    for variant in 0..3 {
                        let mut conv = depthwise(kernel, stride_w);
                        conv.in_channels = 2;
                        conv.out_channels = 2;
                        conv.groups = 2;
                        conv.width = width;
                        conv.height = [1, 3, 8][variant];
                        conv.stride_h = stride_h;
                        conv.pad_top = [kernel / 2, 0, kernel + 3][variant];
                        conv.pad_bottom = [kernel / 2, kernel, kernel + 1][variant];
                        conv.pad_left = [0, kernel / 2, kernel + 5][variant];
                        conv.pad_right = [kernel, kernel / 2, 1][variant];
                        compare_all_backends(conv, variant != 1);
                    }
                }
            }
        }
    }
}

#[test]
fn kernels_pointwise_four_channel_tiles_and_tails() {
    compare_all_backends(pointwise(), true);
    let mut conv = pointwise();
    conv.in_channels = 3;
    conv.out_channels = 5;
    conv.width = 13;
    compare_all_backends(conv, false);
}

#[test]
fn kernels_pointwise_tiny_and_long_channel_reduction() {
    let mut conv = pointwise();
    conv.batch = 1;
    conv.in_channels = 1;
    conv.out_channels = 1;
    conv.height = 1;
    conv.width = 1;
    compare_all_backends(conv, true);
    conv.in_channels = 129;
    conv.out_channels = 9;
    conv.width = 17;
    compare_all_backends(conv, true);
}

#[test]
fn kernels_reject_invalid_dimensions_before_writing() {
    let base = depthwise(3, 1);
    let mut invalid = Vec::new();
    let mut conv = base;
    conv.batch = 0;
    invalid.push(conv);
    conv = base;
    conv.width = 0;
    invalid.push(conv);
    conv = base;
    conv.stride_w = 0;
    invalid.push(conv);
    conv = base;
    conv.groups = 0;
    invalid.push(conv);
    conv = base;
    conv.groups = 2;
    invalid.push(conv);
    conv = base;
    conv.out_channels *= 2; // 不支持 depthwise multiplier > 1。
    invalid.push(conv);
    conv = base;
    conv.kernel_h = 7;
    conv.kernel_w = 7;
    invalid.push(conv);
    conv = base;
    conv.stride_h = 3;
    invalid.push(conv);
    conv = base;
    conv.height = 1;
    conv.pad_top = 0;
    conv.pad_bottom = 0;
    invalid.push(conv);
    conv = base;
    conv.width = usize::MAX;
    invalid.push(conv);
    conv = base;
    conv.batch = usize::MAX;
    invalid.push(conv);
    conv = base;
    conv.pad_top = usize::MAX;
    invalid.push(conv);
    conv = base;
    conv.width = MAX_OUTPUT_ELEMENTS + 1;
    invalid.push(conv);
    conv = pointwise();
    conv.pad_left = 1;
    invalid.push(conv);

    for conv in invalid {
        assert!(conv.output_shape().is_err(), "{conv:?}");
        assert!(conv.output_len().is_err(), "{conv:?}");
        let mut output = [1234.5; 7];
        assert!(conv
            .run(&[], &[], None, &mut output, Backend::Auto)
            .is_err());
        assert_eq!(output, [1234.5; 7], "{conv:?}");
    }
}

#[test]
fn kernels_reject_buffer_lengths_before_writing() {
    let conv = pointwise();
    let input = values(conv.input_len().unwrap(), 1);
    let weights = values(conv.weights_len().unwrap(), 2);
    let bias = values(conv.out_channels, 3);
    let output_len = conv.output_len().unwrap();
    for case in 0..7 {
        let mut output = vec![1234.5; output_len + usize::from(case == 5)];
        if case == 6 {
            output.pop();
        }
        let input: &[f32] = if case == 0 {
            &input[..input.len() - 1]
        } else if case == 1 {
            &[]
        } else {
            &input
        };
        let weights = if case == 2 {
            &weights[..weights.len() - 1]
        } else {
            &weights
        };
        let bias = if case == 3 {
            Some(&bias[..bias.len() - 1])
        } else if case == 4 {
            Some(&[][..])
        } else {
            Some(&bias[..])
        };
        assert!(conv
            .run(input, weights, bias, &mut output, Backend::Auto)
            .is_err());
        assert!(output.iter().all(|&value| value == 1234.5));
    }
}

/// 同一份无依赖内核源码可直接用 rustc --test 编译，单独衡量开发检查开销。
/// 数据来自导出的三组 Tiny 权重；输入为固定合成中间张量，不是端到端 OCR。
#[test]
#[ignore = "需本地 Tiny 子图权重；用于相同源码不同编译检查开关的拆分测速"]
fn kernels_microbench_real_weights() {
    use std::{hint::black_box, path::Path, time::Instant};

    fn read_f32(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len() % 4, 0);
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    fn time(mut run: impl FnMut()) -> f64 {
        run();
        let mut samples = Vec::new();
        for _ in 0..11 {
            let start = Instant::now();
            run();
            let seconds = start.elapsed().as_secs_f64();
            let iterations = ((0.002 / seconds.max(1e-9)).ceil() as usize).clamp(1, 64);
            let start = Instant::now();
            for _ in 0..iterations {
                run();
            }
            samples.push(start.elapsed().as_secs_f64() * 1000. / iterations as f64);
        }
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }

    let root = std::env::var_os("RUSTSHOT_OCR_KERNEL_DIR")
        .expect("请设置 RUSTSHOT_OCR_KERNEL_DIR 为三组 Tiny det 子图目录");
    let root = Path::new(&root);
    let oracle_root = std::env::var_os("RUSTSHOT_OCR_KERNEL_VALUES").map(std::path::PathBuf::from);
    println!(
        "debug_assertions={} avx2_fma={} opt-level=2 inputs={}; 真实权重、输出复用，仅计各算子调用",
        cfg!(debug_assertions),
        avx2_available(),
        if oracle_root.is_some() {
            "ORT前缀激活"
        } else {
            "固定合成张量"
        }
    );
    for (index, (channels, height, width, kernel, stride, out_channels)) in [
        (32, 184, 184, 3, 1, 64),
        (32, 184, 184, 3, 2, 64),
        (64, 23, 23, 5, 1, 16),
    ]
    .into_iter()
    .enumerate()
    {
        let directory = root.join(format!("case-{index:02}"));
        let mut dw = depthwise(kernel, stride);
        dw.batch = 1;
        dw.in_channels = channels;
        dw.out_channels = channels;
        dw.groups = channels;
        dw.height = height;
        dw.width = width;
        let dw_shape = dw.output_shape().unwrap();
        let mut pw = pointwise();
        pw.batch = 1;
        pw.in_channels = channels;
        pw.height = dw_shape[2];
        pw.width = dw_shape[3];
        pw.out_channels = out_channels;
        let oracle_case = oracle_root
            .as_ref()
            .map(|root| root.join(format!("case-{index:02}")));
        let input = oracle_case.as_ref().map_or_else(
            || values(dw.input_len().unwrap(), 17),
            |root| read_f32(&root.join("random-input.f32")),
        );
        let dw_weights = read_f32(&directory.join("dw-weights.f32"));
        let dw_bias = read_f32(&directory.join("dw-bias.f32"));
        let pw_weights = read_f32(&directory.join("pw-weights.f32"));
        // 第三组真实模型的 pointwise 没有 bias；导出器不伪造零 bias。
        let pw_bias_path = directory.join("pw-bias.f32");
        let pw_bias = pw_bias_path.is_file().then(|| read_f32(&pw_bias_path));
        let mut first = vec![0.; dw.output_len().unwrap()];
        let mut second = vec![0.; pw.output_len().unwrap()];
        if let Some(root) = &oracle_case {
            let mut max_abs = 0f32;
            let mut compare = |actual: &[f32], expected: &[f32]| {
                assert_eq!(actual.len(), expected.len());
                for (index, (&actual, &expected)) in actual.iter().zip(expected).enumerate() {
                    assert!(actual.is_finite() && expected.is_finite());
                    let difference = (actual - expected).abs();
                    max_abs = max_abs.max(difference);
                    assert!(
                        difference <= 1e-5 + 1e-4 * expected.abs(),
                        "ORT 数值不符：index={index} actual={actual} expected={expected}"
                    );
                }
            };
            for pattern in ["random", "zero", "ramp"] {
                let source = read_f32(&root.join(format!("{pattern}-input.f32")));
                let expected_dw = read_f32(&root.join(format!("{pattern}-dw.f32")));
                let expected_pw = read_f32(&root.join(format!("{pattern}-pw.f32")));
                dw.run(
                    &source,
                    &dw_weights,
                    Some(&dw_bias),
                    &mut first,
                    Backend::Auto,
                )
                .unwrap();
                compare(&first, &expected_dw);
                pw.run(
                    &expected_dw,
                    &pw_weights,
                    pw_bias.as_deref(),
                    &mut second,
                    Backend::Auto,
                )
                .unwrap();
                compare(&second, &expected_pw);
                pw.run(
                    &first,
                    &pw_weights,
                    pw_bias.as_deref(),
                    &mut second,
                    Backend::Auto,
                )
                .unwrap();
                compare(&second, &expected_pw);
            }
            println!("case-{index:02} oracle_patterns=3 max_abs={max_abs:e}");
        }
        dw.run(
            &input,
            &dw_weights,
            Some(&dw_bias),
            &mut first,
            Backend::Auto,
        )
        .unwrap();
        let dw_ms = time(|| {
            dw.run(
                black_box(&input),
                &dw_weights,
                Some(&dw_bias),
                &mut first,
                Backend::Auto,
            )
            .unwrap();
            black_box(&first);
        });
        let pw_ms = time(|| {
            pw.run(
                black_box(&first),
                &pw_weights,
                pw_bias.as_deref(),
                &mut second,
                Backend::Auto,
            )
            .unwrap();
            black_box(&second);
        });
        let pair_ms = time(|| {
            dw.run(
                black_box(&input),
                &dw_weights,
                Some(&dw_bias),
                &mut first,
                Backend::Auto,
            )
            .unwrap();
            pw.run(
                &first,
                &pw_weights,
                pw_bias.as_deref(),
                &mut second,
                Backend::Auto,
            )
            .unwrap();
            black_box(&second);
        });
        assert!(second.iter().all(|value| value.is_finite()));
        println!("case-{index:02} dw_ms={dw_ms:.4} pw_ms={pw_ms:.4} pair_ms={pair_ms:.4}");
    }
}
