//! 可选外部样例回归；默认不读仓库外文件、不加载模型、不创建窗口。
//! RUSTSHOT_INK_FIXTURES 指向含 cases/image/mask/detectorBox 的 manifest.json。
//! RUSTSHOT_INK_IMAGE 指定实际 OCR 输入，RUSTSHOT_INK_OUT 指定诊断输出目录。
use super::*;
use serde_json::{json, Value};
use std::path::Path;

fn quad_json(geometry: TextGeometry) -> Value {
    json!({"quad": geometry.quad.map(|p| [p.x, p.y])})
}

fn area(geometry: TextGeometry) -> f64 {
    let q = geometry.quad;
    (0..4)
        .map(|i| {
            let a = q[i];
            let b = q[(i + 1) % 4];
            f64::from(a.x) * f64::from(b.y) - f64::from(a.y) * f64::from(b.x)
        })
        .sum::<f64>()
        .abs()
        / 2.
}

#[test]
#[ignore = "需要显式提供带独立文字掩码的样例 manifest"]
fn ink_fixture_pixel_coverage() {
    let path = std::env::var("RUSTSHOT_INK_FIXTURES").expect("需设置 RUSTSHOT_INK_FIXTURES");
    let path = Path::new(&path);
    let root = path.parent().unwrap();
    let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut report = Vec::new();
    for case in manifest["cases"].as_array().unwrap() {
        let image = crate::export::read_image(&root.join(case["image"].as_str().unwrap())).unwrap();
        let mask = crate::export::read_image(&root.join(case["mask"].as_str().unwrap())).unwrap();
        assert_eq!((image.width, image.height), (mask.width, mask.height));
        let rgb = RgbImage::from_bgra(&image).unwrap();
        let original = TextGeometry {
            quad: std::array::from_fn(|i| Point {
                x: case["detectorBox"][i]["x"].as_f64().unwrap() as f32,
                y: case["detectorBox"][i]["y"].as_f64().unwrap() as f32,
            }),
        };
        let projection = Projection::new(original).unwrap();
        let mut rectified = false;
        let candidate = case["text"]
            .as_str()
            .filter(|text| text.chars().any(char::is_alphanumeric))
            .and_then(|_| Roi::new(projection, &rgb))
            .and_then(|roi| {
                let ordinary = line_evidence(projection, roi, &rgb, &mut || Ok(())).unwrap()?;
                let aligned = aligned_evidence(projection, roi, &rgb, &mut || Ok(())).unwrap();
                rectified = aligned.is_some();
                Some(aligned.unwrap_or(ordinary))
            });
        let output = candidate.and_then(Evidence::quad).unwrap_or(original);
        let p = Projection::new(output).unwrap();
        let (mut total, mut covered, mut full_covered, mut original_full_covered) = (0, 0, 0, 0);
        for y in 0..mask.height as usize {
            for x in 0..mask.width as usize {
                if mask.bgra[(y * mask.width as usize + x) * 4] == 0 {
                    continue;
                }
                total += 1;
                if p.inverse(Point {
                    x: x as f32 + 0.5,
                    y: y as f32 + 0.5,
                })
                .is_some_and(|[u, v]| (0. ..=1.).contains(&u) && (0. ..=1.).contains(&v))
                {
                    covered += 1;
                }
                let complete = |p: Projection| {
                    [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1)]
                        .into_iter()
                        .all(|(x, y)| {
                            p.inverse(Point {
                                x: x as f32,
                                y: y as f32,
                            })
                            .is_some_and(|[u, v]| {
                                (0. ..=1.).contains(&u) && (0. ..=1.).contains(&v)
                            })
                        })
                };
                full_covered += u64::from(complete(p));
                original_full_covered += u64::from(complete(projection));
            }
        }
        assert_eq!(
            Some(total),
            case["truthPixelCount"].as_u64(),
            "独立掩码须与清单一致"
        );
        let coverage = if total == 0 {
            1.
        } else {
            covered as f64 / total as f64
        };
        let minimum = case["minCoverage"].as_f64().unwrap();
        assert!(
            coverage >= minimum,
            "{}: coverage={coverage} < {minimum}",
            case["id"]
        );
        assert!(
            full_covered >= original_full_covered,
            "{}: 新框丢失原框完整包含的掩码像素单元 {full_covered}/{original_full_covered}",
            case["id"]
        );
        let rotation = case["source"]["rotation"].as_f64().unwrap_or(0.);
        assert!(
            !rectified || rotation == 0.,
            "{}: 真旋转字体不能被拉平",
            case["id"]
        );
        if case["expectUnchanged"].as_bool() == Some(true) {
            assert_eq!(original, output, "负样例不应收紧");
        }
        report.push(json!({
            "id":case["id"], "tags":case["tags"], "coverage":coverage,
            "truth_pixels":total, "covered_pixels":covered,
            "full_covered_pixels":full_covered,"original_full_covered_pixels":original_full_covered,
            "full_cell_coverage":if total==0 {1.} else {full_covered as f64/total as f64},
            "rectified":rectified,
            "changed":output != original, "area_reduction":1.-area(output)/area(original),
            "before":quad_json(original), "after":quad_json(output),
        }));
    }
    let changed = report.iter().filter(|r| r["changed"] == true).count();
    let min = report
        .iter()
        .filter_map(|r| r["coverage"].as_f64())
        .fold(1., f64::min);
    println!(
        "ink fixtures={} changed={changed} coverage_min={min:.6}",
        report.len()
    );
    if let Ok(out) = std::env::var("RUSTSHOT_INK_OUT") {
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            Path::new(&out).join("mask-report.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}

#[test]
#[ignore = "需要显式提供原始截图并使用真实 PPOCR 模型"]
fn ink_actual_ocr_report() {
    let path = std::env::var("RUSTSHOT_INK_IMAGE").expect("需设置 RUSTSHOT_INK_IMAGE");
    let out = std::env::var("RUSTSHOT_INK_OUT").expect("需设置 RUSTSHOT_INK_OUT");
    let profile = std::env::var("RUSTSHOT_INK_PROFILE").unwrap_or_else(|_| "tiny".into());
    let profile = crate::ppocr::profile::OcrProfile::parse(&profile).unwrap();
    let path = Path::new(&path);
    if path.is_dir() {
        let mut paths: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("png"))
            .collect();
        paths.sort();
        assert!(!paths.is_empty(), "指定目录中没有 PNG");
        for path in paths {
            let destination = Path::new(&out).join(path.file_stem().unwrap());
            actual_image(&path, &destination, profile);
        }
    } else {
        actual_image(path, Path::new(&out), profile);
    }
}

fn actual_image(path: &Path, out: &Path, profile: crate::ppocr::profile::OcrProfile) {
    let image = crate::export::read_image(path).unwrap();
    let owner = crate::recognition::task::CancelOwner::new();
    let result =
        crate::ppocr::recognize_profile_cancellable(&image, profile, &owner.token()).unwrap();
    let rgb = RgbImage::from_bgra(&image).unwrap();
    let mut baseline = result.clone();
    for line in &mut baseline.lines {
        let Some(sampling) = &mut line.graphemes else {
            continue;
        };
        sampling.clear_selection_adjustments();
        if let Some(projection) = line.geometry.and_then(Projection::new) {
            if let Some(roi) = Roi::new(projection, &rgb) {
                if let Some(evidence) =
                    line_evidence(projection, roi, &rgb, &mut || Ok(())).unwrap()
                {
                    sampling.tighten_selection_geometry(evidence.geometry);
                }
            }
        }
    }
    let before_selection = crate::pin_text_selection::SelectionText::from_recognized(&baseline);
    let selection = crate::pin_text_selection::SelectionText::from_recognized(&result);
    let mut offset = 0;
    let (mut checked, mut max_x) = (0, 0f64);
    for line in &result.lines {
        for (byte, _) in line
            .text
            .grapheme_indices(true)
            .filter(|(_, glyph)| !glyph.chars().all(char::is_whitespace))
            .skip(1)
        {
            let before = before_selection.caret_segment(offset + byte);
            let after = selection.caret_segment(offset + byte);
            assert_eq!(before.is_some(), after.is_some(), "内部分字停点不能丢失");
            if let (Some(before), Some(after)) = (before, after) {
                let x = |edge: [Point; 2]| (f64::from(edge[0].x) + f64::from(edge[1].x)) / 2.;
                max_x = max_x.max((x(before) - x(after)).abs());
                checked += 1;
            }
        }
        offset += line.text.len() + 1;
    }
    assert!(max_x <= 0.001, "内部实际caret横向位移={max_x}");
    let mut before = diagnostic_json(&baseline, profile);
    let mut after = diagnostic_json(&result, profile);
    assert_eq!(
        before["exported_selection_cells"], after["exported_selection_cells"],
        "不能丢失原有字素几何"
    );
    for value in [&mut before, &mut after] {
        value["comparison"] = "same_inference_height_only_vs_endpoints_and_alignment".into();
        value["words_unchanged_by_construction"] = true.into();
        value["checked_internal_carets"] = checked.into();
        value["internal_caret_x_max_delta"] = max_x.into();
        let expected: usize = result
            .lines
            .iter()
            .map(|line| {
                line.text
                    .graphemes(true)
                    .filter(|glyph| !glyph.chars().all(char::is_whitespace))
                    .count()
                    .saturating_sub(1)
            })
            .sum();
        value["expected_internal_carets"] = expected.into();
    }
    if std::env::var("RUSTSHOT_INK_REQUIRE_NO_RECTIFICATION").as_deref() == Ok("1") {
        assert!(
            result.lines.iter().all(|line| line
                .graphemes
                .as_ref()
                .is_none_or(|s| !s.selection_rectified())),
            "已知真实旋转字体不能拉平"
        );
    }
    let mask_path = std::env::var("RUSTSHOT_INK_MASK")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("RUSTSHOT_INK_MASK_DIR")
                .ok()
                .map(|dir| Path::new(&dir).join(path.file_name().unwrap()))
        });
    if let Some(mask_path) = mask_path {
        let mask = crate::export::read_image(&mask_path).unwrap();
        assert_eq!((mask.width, mask.height), (image.width, image.height));
        let (total, old_centers, old_full) =
            selected_mask_coverage(&before_selection, &baseline.text, &mask);
        let (_, new_centers, new_full) = selected_mask_coverage(&selection, &result.text, &mask);
        assert!(
            total > 0 && !result.text.trim().is_empty(),
            "字体真值与真实识别均应非空"
        );
        assert!(
            new_centers >= old_centers && new_full >= old_full,
            "本轮优化不能减少原有真实选区对字体掩码的覆盖"
        );
        after["mask"] = json!({"truth_pixels":total,"before_centers":old_centers,
            "after_centers":new_centers,"before_full_cells":old_full,"after_full_cells":new_full});
    }
    std::fs::create_dir_all(out).unwrap();
    for (name, value) in [("before.json", before), ("after.json", after)] {
        std::fs::write(out.join(name), serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
    assert_eq!(
        selection.selected(0..result.text.len()).as_deref(),
        Some(result.text.as_str())
    );
    let endpoints = result
        .lines
        .iter()
        .filter(|line| {
            line.graphemes
                .as_ref()
                .is_some_and(|s| s.selection_reading_range() != [0., 1.])
        })
        .count();
    let rectified = result
        .lines
        .iter()
        .filter(|line| {
            line.graphemes
                .as_ref()
                .is_some_and(|s| s.selection_rectified())
        })
        .count();
    println!("actual OCR image={} profile={} lines={} endpoints={endpoints} rectified={rectified} internal_carets={checked} max_x={max_x:.8}",
        path.file_name().unwrap().to_string_lossy(), profile.as_str(), result.lines.len());
}

fn selected_mask_coverage(
    selection: &crate::pin_text_selection::SelectionText,
    text: &str,
    mask: &crate::capture::Image,
) -> (u64, u64, u64) {
    let quads: Vec<_> = selection
        .marking_quads(0..text.len())
        .into_iter()
        .filter_map(|quad| Projection::new(TextGeometry { quad }))
        .collect();
    let mut total = 0;
    let mut centers = 0;
    let mut full = 0;
    for y in 0..mask.height as usize {
        for x in 0..mask.width as usize {
            if mask.bgra[(y * mask.width as usize + x) * 4] == 0 {
                continue;
            }
            total += 1;
            let contains = |p: Projection, x: f32, y: f32| {
                p.inverse(Point { x, y })
                    .is_some_and(|[u, v]| (0. ..=1.).contains(&u) && (0. ..=1.).contains(&v))
            };
            centers += u64::from(
                quads
                    .iter()
                    .any(|p| contains(*p, x as f32 + 0.5, y as f32 + 0.5)),
            );
            full += u64::from(quads.iter().any(|p| {
                [(x, y), (x + 1, y), (x + 1, y + 1), (x, y + 1)]
                    .into_iter()
                    .all(|(x, y)| contains(*p, x as f32, y as f32))
            }));
        }
    }
    (total, centers, full)
}

fn diagnostic_json(result: &RecognizedText, profile: crate::ppocr::profile::OcrProfile) -> Value {
    let selection_text = crate::pin_text_selection::SelectionText::from_recognized(result);
    let mut offset = 0;
    let (mut expected_cells, mut exported_cells) = (0usize, 0usize);
    let lines: Vec<_> = result
        .lines
        .iter()
        .map(|line| {
            let original = line.geometry;
            let selection = line
                .graphemes
                .as_ref()
                .map(|s| s.selection_geometry())
                .or(original);
            let cells: Vec<_> = line.text.grapheme_indices(true).filter_map(|(byte, glyph)| {
                if glyph.chars().all(char::is_whitespace) { return None }
                expected_cells += 1;
                let range = offset + byte..offset + byte + glyph.len();
                let quads = selection_text.marking_quads(range.clone());
                (quads.len() == 1).then(|| json!({
                    "range":[range.start, range.end],
                    "quad":quads[0].map(|point| [point.x, point.y]),
                }))
            }).collect();
            exported_cells += cells.len();
            let value = json!({"text":line.text,"geometry":original.map(quad_json),
                "selection_geometry":selection.map(quad_json),
                "selection_reading_range":line.graphemes.as_ref().map(|s| s.selection_reading_range()).unwrap_or([0.,1.]),
                "selection_rectified":line.graphemes.as_ref().is_some_and(|s| s.selection_rectified()),
                "selection_cells":cells,
            });
            offset += line.text.len() + 1;
            value
        })
        .collect();
    let words: Vec<_> = result.words.iter().map(|word| json!({
        "text":word.text, "line":word.line,
        "rect":{"x":word.rect.x,"y":word.rect.y,"width":word.rect.width,"height":word.rect.height},
        "geometry":word.geometry.map(quad_json),
    })).collect();
    json!({"text":result.text,"profile":profile.as_str(),"lines":lines,"words":words,
        "expected_nonspace_graphemes":expected_cells,"exported_selection_cells":exported_cells,
        "missing_selection_cells":expected_cells.saturating_sub(exported_cells)})
}
