//! 开发用真实权重子图对照；不进入普通 OCR，也不下载模型。
//! 先运行 Tools/prepare-ocr-kernel-probe.py，再设置 RUSTSHOT_OCR_KERNEL_DIR。
//! `cargo test --lib ppocr::kernel_probe_tests -- --ignored --nocapture --test-threads=1`
use super::{assets, kernels, runtime};
use crate::{recognition::task::CancelOwner, Result};
use kernels::{Backend, Conv2d};
use ort::{
    session::{builder::GraphOptimizationLevel, IoBinding, Session},
    value::{Tensor, TensorRef},
};
use serde::Deserialize;
use std::{hint::black_box, path::Path, time::Instant};

const ATOL: f32 = 1e-5;
const RTOL: f32 = 1e-4;

#[derive(Deserialize)]
struct Case {
    prefix_model: String,
    pair_model: String,
    original_input_shape: [usize; 4],
    prefix_input_name: String,
    input_shape: [usize; 4],
    input_name: String,
    output_names: [String; 2],
    layers: [Layer; 2],
}

#[derive(Deserialize)]
struct Layer {
    name: String,
    params: Parameters,
    weights: String,
    bias: Option<String>,
}

#[derive(Deserialize)]
struct Parameters {
    batch: usize,
    in_channels: usize,
    height: usize,
    width: usize,
    out_channels: usize,
    kernel_h: usize,
    kernel_w: usize,
    stride_h: usize,
    stride_w: usize,
    pad_top: usize,
    pad_left: usize,
    pad_bottom: usize,
    pad_right: usize,
    groups: usize,
}

impl Parameters {
    fn convolution(&self) -> Conv2d {
        Conv2d {
            batch: self.batch,
            in_channels: self.in_channels,
            height: self.height,
            width: self.width,
            out_channels: self.out_channels,
            kernel_h: self.kernel_h,
            kernel_w: self.kernel_w,
            stride_h: self.stride_h,
            stride_w: self.stride_w,
            pad_top: self.pad_top,
            pad_left: self.pad_left,
            pad_bottom: self.pad_bottom,
            pad_right: self.pad_right,
            groups: self.groups,
        }
    }
}

struct LoadedLayer {
    convolution: Conv2d,
    weights: Vec<f32>,
    bias: Option<Vec<f32>>,
}

impl LoadedLayer {
    fn load(root: &Path, layer: &Layer) -> Result<Self> {
        Ok(Self {
            convolution: layer.params.convolution(),
            weights: read_f32(&root.join(&layer.weights))?,
            bias: layer
                .bias
                .as_ref()
                .map(|name| read_f32(&root.join(name)))
                .transpose()?,
        })
    }

    fn run(&self, input: &[f32], output: &mut [f32], backend: Backend) -> Result<Backend> {
        self.convolution
            .run(input, &self.weights, self.bias.as_deref(), output, backend)
            .map_err(Into::into)
    }
}

fn read_f32(path: &Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() % 4 != 0 {
        return Err(format!("F32 文件长度不是四的倍数：{}", path.display()).into());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect())
}

fn session(path: &Path, threads: usize, optimization: GraphOptimizationLevel) -> Result<Session> {
    Ok(Session::builder()?
        .with_intra_threads(threads)?
        .with_inter_threads(1)?
        .with_parallel_execution(false)?
        .with_config_entry("session.intra_op.allow_spinning", "0")?
        .with_optimization_level(optimization)?
        .commit_from_file(path)?)
}

fn input(shape: [usize; 4], pattern: usize) -> Vec<f32> {
    let mut seed = 0x14_15_16_17u32;
    (0..shape.iter().product())
        .map(|index| match pattern {
            0 => {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 22) as f32 - 2.
            }
            1 => 0.,
            _ => (index % shape[3]) as f32 / shape[3] as f32 * 4. - 2.,
        })
        .collect()
}

fn compare(actual: &[f32], expected: &[f32]) -> (f32, f64) {
    assert_eq!(actual.len(), expected.len());
    let (mut max_abs, mut max_scaled, mut squares, mut bad) = (0f32, 0f32, 0f64, 0);
    for (&a, &b) in actual.iter().zip(expected) {
        assert!(a.is_finite() && b.is_finite(), "输出含非有限值");
        let difference = (a - b).abs();
        let scaled = difference / (ATOL + RTOL * b.abs());
        max_abs = max_abs.max(difference);
        max_scaled = max_scaled.max(scaled);
        squares += f64::from(difference).powi(2);
        bad += usize::from(scaled > 1.);
    }
    assert_eq!(
        bad, 0,
        "数值不符：max_abs={max_abs:e} max_scaled={max_scaled:.3}"
    );
    (max_abs, (squares / actual.len() as f64).sqrt())
}

fn elapsed_ms(mut run: impl FnMut() -> Result<()>) -> Result<f64> {
    let start = Instant::now();
    run()?;
    let first = start.elapsed().as_secs_f64();
    let iterations = ((0.003 / first.max(1e-9)).ceil() as usize).clamp(1, 64);
    let start = Instant::now();
    for _ in 0..iterations {
        run()?;
    }
    Ok(start.elapsed().as_secs_f64() * 1000. / iterations as f64)
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn binding(
    session: &Session,
    case: &Case,
    values: &[f32],
    shapes: [[usize; 4]; 2],
) -> Result<IoBinding> {
    let mut binding = session.create_binding()?;
    let input = Tensor::from_array((case.input_shape, values.to_vec()))?;
    binding.bind_input(&case.input_name, &input)?;
    for (name, shape) in case.output_names.iter().zip(shapes) {
        let output = Tensor::from_array((shape, vec![0f32; shape.iter().product()]))?;
        binding.bind_output(name, output)?;
    }
    Ok(binding)
}

fn run_case(path: &Path) -> Result<()> {
    let root = path.parent().ok_or("子图描述缺少父目录")?;
    let case: Case = serde_json::from_slice(&std::fs::read(path)?)?;
    let [depthwise, pointwise] = [
        LoadedLayer::load(root, &case.layers[0])?,
        LoadedLayer::load(root, &case.layers[1])?,
    ];
    assert_eq!(
        case.input_shape,
        [
            depthwise.convolution.batch,
            depthwise.convolution.in_channels,
            depthwise.convolution.height,
            depthwise.convolution.width
        ]
    );
    assert_eq!(
        depthwise.convolution.output_shape()?,
        [
            pointwise.convolution.batch,
            pointwise.convolution.in_channels,
            pointwise.convolution.height,
            pointwise.convolution.width
        ]
    );
    let mut prefix = session(
        &root.join(&case.prefix_model),
        1,
        GraphOptimizationLevel::Level3,
    )?;
    let mut reference = session(
        &root.join(&case.pair_model),
        1,
        GraphOptimizationLevel::Disable,
    )?;
    let mut ort_one = session(
        &root.join(&case.pair_model),
        1,
        GraphOptimizationLevel::Level3,
    )?;
    let mut ort_four = session(
        &root.join(&case.pair_model),
        4,
        GraphOptimizationLevel::Level3,
    )?;
    let mut first = vec![0.; depthwise.convolution.output_len()?];
    let mut second = vec![0.; pointwise.convolution.output_len()?];
    let mut selected = Vec::new();
    let mut max_abs = [0f32; 2];
    let mut max_rmse = [0f64; 2];
    let mut used = Backend::Scalar;
    for pattern in 0..3 {
        let source = input(case.original_input_shape, pattern);
        let tensor = TensorRef::from_array_view((case.original_input_shape, source.as_slice()))?;
        let values = prefix.run(ort::inputs![case.prefix_input_name.as_str() => tensor])?;
        let (shape, activation) = values[0].try_extract_tensor::<f32>()?;
        assert_eq!(shape.as_ref(), case.input_shape.map(|value| value as i64));
        let tensor = TensorRef::from_array_view((case.input_shape, activation))?;
        let outputs = reference.run(ort::inputs![case.input_name.as_str() => tensor])?;
        let (shape_one, expected_one) =
            outputs[case.output_names[0].as_str()].try_extract_tensor::<f32>()?;
        let (shape_two, expected_two) =
            outputs[case.output_names[1].as_str()].try_extract_tensor::<f32>()?;
        assert_eq!(
            shape_one.as_ref(),
            depthwise.convolution.output_shape()?.map(|v| v as i64)
        );
        assert_eq!(
            shape_two.as_ref(),
            pointwise.convolution.output_shape()?.map(|v| v as i64)
        );
        // 优化过的计时模型也须与禁优化参考模型一致。
        for optimized_session in [&mut ort_one, &mut ort_four] {
            let tensor = TensorRef::from_array_view((case.input_shape, activation))?;
            let optimized =
                optimized_session.run(ort::inputs![case.input_name.as_str() => tensor])?;
            compare(
                optimized[case.output_names[0].as_str()]
                    .try_extract_tensor::<f32>()?
                    .1,
                expected_one,
            );
            compare(
                optimized[case.output_names[1].as_str()]
                    .try_extract_tensor::<f32>()?
                    .1,
                expected_two,
            );
        }
        for backend in [Backend::Scalar, Backend::Auto] {
            depthwise.run(activation, &mut first, backend)?;
            let error = compare(&first, expected_one);
            max_abs[0] = max_abs[0].max(error.0);
            max_rmse[0] = max_rmse[0].max(error.1);
            // 单层对齐用 ORT 的 DW 输出，随后再检查我们自己的串联误差。
            pointwise.run(expected_one, &mut second, backend)?;
            let error = compare(&second, expected_two);
            max_abs[1] = max_abs[1].max(error.0);
            max_rmse[1] = max_rmse[1].max(error.1);
            used = pointwise.run(&first, &mut second, backend)?;
            let error = compare(&second, expected_two);
            max_abs[1] = max_abs[1].max(error.0);
            max_rmse[1] = max_rmse[1].max(error.1);
        }
        if pattern == 0 {
            selected = activation.to_vec();
        }
    }
    drop(prefix);
    drop(reference);
    let shapes = [
        depthwise.convolution.output_shape()?,
        pointwise.convolution.output_shape()?,
    ];
    let bindings = [
        binding(&ort_one, &case, &selected, shapes)?,
        binding(&ort_four, &case, &selected, shapes)?,
    ];
    depthwise.run(&selected, &mut first, Backend::Scalar)?;
    pointwise.run(&first, &mut second, Backend::Scalar)?;
    for (session, binding) in [&mut ort_one, &mut ort_four].into_iter().zip(&bindings) {
        let outputs = session.run_binding(binding)?;
        compare(
            outputs[case.output_names[0].as_str()]
                .try_extract_tensor::<f32>()?
                .1,
            &first,
        );
        compare(
            outputs[case.output_names[1].as_str()]
                .try_extract_tensor::<f32>()?
                .1,
            &second,
        );
    }
    println!(
        "case={} dw={} pw={} input={:?} selected={} max_abs={:?} max_rmse={:?}",
        path.parent().unwrap().display(),
        case.layers[0].name,
        case.layers[1].name,
        case.input_shape,
        used.as_str(),
        max_abs,
        max_rmse
    );
    let mut timings: [Vec<f64>; 6] = std::array::from_fn(|_| Vec::new());
    for round in 0..18 {
        for offset in 0..6 {
            let engine = (round + offset) % 6;
            let ms = if engine < 2 {
                let backend = [Backend::Scalar, Backend::Auto][engine];
                elapsed_ms(|| {
                    depthwise.run(black_box(&selected), &mut first, backend)?;
                    pointwise.run(&first, &mut second, backend)?;
                    black_box(&second);
                    Ok(())
                })?
            } else if engine < 4 {
                let session = if engine == 2 {
                    &mut ort_one
                } else {
                    &mut ort_four
                };
                elapsed_ms(|| {
                    let tensor =
                        TensorRef::from_array_view((case.input_shape, selected.as_slice()))?;
                    let output = session.run(ort::inputs![case.input_name.as_str() => tensor])?;
                    black_box(&output);
                    Ok(())
                })?
            } else {
                let session = if engine == 4 {
                    &mut ort_one
                } else {
                    &mut ort_four
                };
                elapsed_ms(|| {
                    let output = session.run_binding(&bindings[engine - 4])?;
                    black_box(&output);
                    Ok(())
                })?
            };
            if round >= 3 {
                timings[engine].push(ms);
            }
        }
    }
    let medians: [f64; 6] = std::array::from_fn(|i| median(&mut timings[i]));
    println!(
        "pair_median_ms scalar={:.4} auto={:.4} ort1={:.4} ort4={:.4} ort1_bound={:.4} ort4_bound={:.4} scalar_over_auto={:.2} ort1_bound_over_auto={:.2}",
        medians[0], medians[1], medians[2], medians[3], medians[4], medians[5],
        medians[0] / medians[1], medians[4] / medians[1]
    );
    Ok(())
}

#[test]
#[ignore = "需先导出真实模型子图并设置 RUSTSHOT_OCR_KERNEL_DIR；不下载模型"]
fn real_model_convolution_pairs() -> Result<()> {
    let root = std::env::var_os("RUSTSHOT_OCR_KERNEL_DIR")
        .ok_or("请设置 RUSTSHOT_OCR_KERNEL_DIR 为导出子图目录")?;
    let root = Path::new(&root);
    let cases: Vec<String> = serde_json::from_slice(&std::fs::read(root.join("cases.json"))?)?;
    if cases.is_empty() {
        return Err("没有可验证的卷积子图".into());
    }
    let owner = CancelOwner::new();
    let cached = assets::cached_cancellable(&owner.token())?
        .ok_or("需要已校验的本地 Tiny OCR 运行库缓存；本实验不自动下载")?;
    runtime::initialize(&cached.runtime)?;
    println!(
        "kernel_probe cases={} avx2_fma={} atol={} rtol={} ORT=1.24.4 reference_graph=Disable timing_graph=Level3 spin=off patterns=random/zero/ramp; Rust复用输出，ORT分别测正常Run和预绑定输出bound，二者均含API开销；均不计prefix/拷贝/绑定/模型加载；不是端到端OCR测速",
        cases.len(), kernels::avx2_available(), ATOL, RTOL
    );
    for case in cases {
        run_case(&root.join(case))?;
    }
    Ok(())
}
