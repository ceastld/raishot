//! 手动 OCR 测速（默认忽略）：图片预解码后只计 det→cls→rec 全流程与各模型推理耗时。
//! `RUSTSHOT_OCR_BENCH_DIR=<目录> cargo test --lib ppocr::bench_tests -- --ignored --nocapture`
//! 可选 `RUSTSHOT_OCR_BENCH_ROUNDS`（默认 3）、`RUSTSHOT_OCR_BENCH_OUT`（逐图文字 JSON）。
use super::*;
use std::time::Instant;

struct Timed<'a> {
    inner: &'a mut dyn ModelRunner,
    ms: f64,
    calls: usize,
}

impl ModelRunner for Timed<'_> {
    fn run(&mut self, input: Tensor) -> Result<Tensor> {
        let start = Instant::now();
        let output = self.inner.run(input);
        self.ms += start.elapsed().as_secs_f64() * 1000.;
        self.calls += 1;
        output
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[test]
#[ignore]
fn ocr_bench_dir() {
    let Ok(dir) = std::env::var("RUSTSHOT_OCR_BENCH_DIR") else {
        return;
    };
    let rounds = std::env::var("RUSTSHOT_OCR_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3usize);
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "jpg" | "jpeg" | "png"))
        })
        .collect();
    paths.sort();
    let images: Vec<_> = paths
        .iter()
        .map(|path| crate::export::read_image(path).unwrap())
        .collect();
    println!("images={} threads={}", images.len(), threads::current());
    recognize(&images[0]).unwrap();

    let owner = CancelOwner::new();
    let task = owner.token();
    let mut texts = Vec::new();
    for round in 0..rounds {
        let (mut total, mut det_ms, mut cls_ms, mut rec_ms) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut rec_calls, mut det_calls, mut cls_calls) = (0, 0, 0);
        texts.clear();
        for image in &images {
            let mut state = ENGINES.get().unwrap().lock();
            let engine =
                engine(&mut state, OcrProfile::Tiny, &task, &mut Timing::default()).unwrap();
            let run = runtime::TaskRunContext::new(&task).unwrap();
            let recognition = engine.recognition.as_mut().unwrap();
            let (mut det_runner, mut cls_runner, mut rec_runner) = (
                run.runner(&mut engine.det),
                run.runner(&mut recognition.cls),
                run.runner(&mut recognition.rec),
            );
            let mut det = Timed {
                inner: &mut det_runner,
                ms: 0.,
                calls: 0,
            };
            let mut cls = Timed {
                inner: &mut cls_runner,
                ms: 0.,
                calls: 0,
            };
            let mut rec = Timed {
                inner: &mut rec_runner,
                ms: 0.,
                calls: 0,
            };
            let start = Instant::now();
            let result = pipeline::recognize_profile_cancellable(
                image,
                &mut det,
                &mut cls,
                &mut rec,
                &recognition.dictionary,
                OcrProfile::Tiny,
                &task,
            )
            .unwrap();
            total.push(start.elapsed().as_secs_f64() * 1000.);
            det_ms.push(det.ms);
            cls_ms.push(cls.ms);
            rec_ms.push(rec.ms);
            rec_calls += rec.calls;
            det_calls += det.calls;
            cls_calls += cls.calls;
            texts.push(
                result
                    .lines
                    .iter()
                    .map(|line| line.text.clone())
                    .collect::<Vec<_>>(),
            );
        }
        let sum: f64 = total.iter().sum();
        let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
        println!(
            "round={round} mean det={:.1}({det_calls}) cls={:.1}({cls_calls}) rec={:.1}",
            mean(&det_ms),
            mean(&cls_ms),
            mean(&rec_ms),
        );
        println!(
            "round={round} median_ms={:.1} mean_ms={:.1} det_med={:.1} cls_med={:.1} rec_med={:.1} rec_calls={rec_calls}",
            median(&mut total),
            sum / images.len() as f64,
            median(&mut det_ms),
            median(&mut cls_ms),
            median(&mut rec_ms),
        );
    }
    if let Ok(out) = std::env::var("RUSTSHOT_OCR_BENCH_OUT") {
        let map: serde_json::Map<_, _> = paths
            .iter()
            .zip(&texts)
            .map(|(path, lines)| {
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    serde_json::json!(lines),
                )
            })
            .collect();
        std::fs::write(out, serde_json::to_string_pretty(&map).unwrap()).unwrap();
    }
}

/// 同形状纯推理对比：只计 ORT Run，输入为固定伪随机张量。
#[test]
#[ignore]
fn ocr_engine_shapes() {
    let Ok(dir) = std::env::var("RUSTSHOT_OCR_BENCH_DIR") else {
        return;
    };
    let first = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "jpg"))
        .unwrap();
    recognize(&crate::export::read_image(&first).unwrap()).unwrap();
    let owner = CancelOwner::new();
    let task = owner.token();
    let assets = assets::ensure_cancellable(&task).unwrap();
    let cases: [(&str, &std::path::Path, [usize; 4], usize); 7] = [
        ("det", &assets.detection, [1, 3, 736, 736], 10),
        ("det", &assets.detection, [1, 3, 960, 960], 10),
        ("det", &assets.detection, [1, 3, 1312, 1312], 6),
        ("cls", &assets.classification, [1, 3, 80, 160], 60),
        ("rec", &assets.recognition, [1, 3, 48, 320], 60),
        ("rec", &assets.recognition, [1, 3, 48, 960], 40),
        ("rec", &assets.recognition, [8, 3, 48, 320], 20),
    ];
    for threads in [1, 4] {
        for (name, model, shape, iterations) in &cases {
            let mut session = runtime::CpuSession::new(model, threads).unwrap();
            let count: usize = shape.iter().product();
            let mut seed = 12345u32;
            let values: Vec<f32> = (0..count)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.
                })
                .collect();
            let mut times = Vec::new();
            for i in 0..iterations + 2 {
                let run = runtime::TaskRunContext::new(&task).unwrap();
                let mut runner = run.runner(&mut session);
                let start = Instant::now();
                runner
                    .run(Tensor {
                        shape: shape.to_vec(),
                        values: values.clone(),
                    })
                    .unwrap();
                if i >= 2 {
                    times.push(start.elapsed().as_secs_f64() * 1000.);
                }
            }
            println!(
                "ort threads={threads} {name} {:?} median_ms={:.2}",
                shape,
                median(&mut times)
            );
        }
    }
}
