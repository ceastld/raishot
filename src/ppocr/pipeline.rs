//! CPU image/tensor pipeline for PP-OCRv6, without managed or image-processing runtimes.
//! Algorithm reference: RapidOcrNet 590d44b2 (Apache-2.0), detector/classifier/recognizer.
//! Coordinates are physical pixels local to the supplied, unmodified BGRA image.
use super::{profile::OcrProfile, ModelRunner, Tensor};
use crate::{
    annotation::Point as TextPoint,
    capture::Image,
    geometry::Rect,
    recognition::{
        graphemes::{
            GraphemeSampling, SamplingRun, MAX_EMISSION_RUNS, MAX_LINE_BYTES, MAX_SAMPLING_STEPS,
            MAX_SOURCE_BYTES, MAX_TOTAL_GRAPHEMES, MAX_TOTAL_RUNS,
        },
        task::{is_cancelled, TaskToken},
        HorizontalDirection, RecognizedLine, RecognizedText, RecognizedWord, TextGeometry,
    },
    Result,
};
use std::collections::VecDeque;

#[path = "duplicate_lines.rs"]
mod duplicate_lines;

#[path = "ink_bounds.rs"]
mod ink_bounds;

type Point = [f32; 2];

#[derive(Clone, Debug)]
pub struct DetectedLine {
    /// Top-left, top-right, bottom-right, bottom-left; not an axis-aligned approximation.
    pub quad: [Point; 4],
    pub confidence: f32,
    pub rect: Rect,
}

#[derive(Clone, Copy, Debug)]
pub struct DetectionConfig {
    pub max_side: usize,
    pub threshold: f32,
    pub box_threshold: f32,
    pub unclip_ratio: f32,
    pub max_candidates: usize,
    pub padding: usize,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            max_side: 736,
            threshold: 0.3,
            box_threshold: 0.5,
            unclip_ratio: 1.,
            max_candidates: 1000,
            padding: 0,
        }
    }
}

fn default_config(image: &Image, unclip_ratio: f32) -> DetectionConfig {
    let short = image.width.min(image.height);
    DetectionConfig {
        max_side: if image.width.max(image.height) >= 1280 {
            3072
        } else {
            736
        },
        padding: if short < 120 { 24 } else { 0 },
        unclip_ratio,
        ..DetectionConfig::default()
    }
}

/// Only the detector is run. No recognizer, dictionary, or classifier is loaded here.
#[cfg(test)]
pub fn detect(image: &Image, det: &mut dyn ModelRunner) -> Result<Vec<DetectedLine>> {
    detect_with_config(image, det, default_config(image, 1.))
}

pub fn detect_cancellable(
    image: &Image,
    det: &mut dyn ModelRunner,
    task: &TaskToken,
) -> Result<Vec<DetectedLine>> {
    checkpoint(Some(task))?;
    let rgb = RgbImage::from_bgra(image)?;
    checkpoint(Some(task))?;
    detect_rgb_with_task(&rgb, det, default_config(image, 1.), Some(task))
}

#[cfg(test)]
pub fn detect_with_config(
    image: &Image,
    det: &mut dyn ModelRunner,
    config: DetectionConfig,
) -> Result<Vec<DetectedLine>> {
    let rgb = RgbImage::from_bgra(image)?;
    detect_rgb(&rgb, det, config)
}

/// `dictionary` contains exactly the original UTF-8 file lines, without the CTC blank.
/// Word rectangles and projected bands are estimated from CTC time steps and the
/// inverse crop transform; neither is an exact ink or glyph boundary.
#[cfg(test)]
pub fn recognize(
    image: &Image,
    det: &mut dyn ModelRunner,
    cls: &mut dyn ModelRunner,
    rec: &mut dyn ModelRunner,
    dictionary: &[String],
) -> Result<RecognizedText> {
    recognize_with_task(image, det, cls, rec, dictionary, None)
}

#[cfg(test)]
pub fn recognize_cancellable(
    image: &Image,
    det: &mut dyn ModelRunner,
    cls: &mut dyn ModelRunner,
    rec: &mut dyn ModelRunner,
    dictionary: &[String],
    task: &TaskToken,
) -> Result<RecognizedText> {
    recognize_with_task(image, det, cls, rec, dictionary, Some(task))
}

/// The selected fixed dictionary and recognizer must belong to the same profile.
/// Shape validation wraps inference; routing, CTC and inverse geometry stay shared.
pub fn recognize_profile_cancellable(
    image: &Image,
    det: &mut dyn ModelRunner,
    cls: &mut dyn ModelRunner,
    rec: &mut dyn ModelRunner,
    dictionary: &[String],
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<RecognizedText> {
    task.checkpoint()?;
    let validated = profile.validate_dictionary(dictionary);
    task.checkpoint()?;
    validated?;
    let mut rec = ProfileRecognizer {
        inner: rec,
        profile,
        task,
    };
    recognize_with_task(image, det, cls, &mut rec, dictionary, Some(task))
}

struct ProfileRecognizer<'a> {
    inner: &'a mut dyn ModelRunner,
    profile: OcrProfile,
    task: &'a TaskToken,
}

impl ModelRunner for ProfileRecognizer<'_> {
    fn run(&mut self, input: Tensor) -> Result<Tensor> {
        let output = self.inner.run(input)?;
        self.task.checkpoint()?;
        let expected = self.profile.dict_class_contract().ctc_classes;
        match output.shape.as_slice() {
            [1, _, classes] if *classes == expected => Ok(output),
            _ => Err(format!(
                "OCR {} 识别输出应为 [1,T,{expected}]，实际 {:?}",
                self.profile.as_str(),
                output.shape
            )
            .into()),
        }
    }
}

fn checkpoint(task: Option<&TaskToken>) -> Result<()> {
    if let Some(task) = task {
        task.checkpoint()?;
    }
    Ok(())
}

fn recognize_with_task(
    image: &Image,
    det: &mut dyn ModelRunner,
    cls: &mut dyn ModelRunner,
    rec: &mut dyn ModelRunner,
    dictionary: &[String],
    task: Option<&TaskToken>,
) -> Result<RecognizedText> {
    checkpoint(task)?;
    if dictionary.is_empty() || dictionary.iter().any(|entry| entry.is_empty()) {
        return Err("OCR 词表为空或含空项".into());
    }
    let rgb = RgbImage::from_bgra(image)?;
    checkpoint(task)?;
    let speculative = rec_only_line(&rgb);
    checkpoint(task)?;
    if let Some(line) = speculative {
        // The bypass only skips detection inference. Classification, CTC fragments and
        // inverse geometry use the same path as detected lines; low quality or a failed
        // speculative inference falls through to one ordinary detector pass.
        match recognize_lines_with_task(&rgb, &[line], cls, rec, dictionary, task) {
            Ok(candidate) => {
                if candidate.confidence >= 0.9 && valid_rec_only_result(&candidate.result, &rgb) {
                    checkpoint(task)?;
                    return finish_recognition(&rgb, candidate.result, task);
                }
            }
            Err(error) if is_cancelled(error.as_ref()) => return Err(error),
            // 取消只能阻止继续回退，不能把刚返回的真实失败改判为取消。
            Err(error) if task.is_some_and(TaskToken::is_cancelled) => return Err(error),
            Err(_) => {}
        }
    }
    checkpoint(task)?;
    let detected = detect_rgb_with_task(&rgb, det, default_config(image, 1.6), task)?;
    // 首轮硬错误保持原错误合同；补救只能改善一个已经成功返回的空/稀疏结果。
    let primary = recognize_lines_with_task(&rgb, &detected, cls, rec, dictionary, task)?;
    checkpoint(task)?;
    let plan = quality_retry_plan(image, &rgb, &primary, &detected);
    checkpoint(task)?;
    let Some(plan) = plan else {
        return finish_recognition(&rgb, primary.result, task);
    };
    let recovery = (|| -> Result<Option<RecognizedText>> {
        checkpoint(task)?;
        let retried = detect_rgb_with_task(&rgb, det, plan.config, task)?;
        let selected = quality_retry_targets(&rgb, &primary, &plan, retried);
        checkpoint(task)?;
        let Some(selected) = selected else {
            return Ok(None);
        };
        let candidate = recognize_lines_with_task(&rgb, &selected, cls, rec, dictionary, task)?;
        // 预算不允许裁掉待识别行；候选的每一行都须成功并具有完整原图合同。
        if candidate.accepted_detected.len() != selected.len()
            || !valid_quality_candidate(&candidate, &rgb)
        {
            checkpoint(task)?;
            return Ok(None);
        }
        let merged = merge_quality_recovery(&primary.result, candidate.result, &rgb);
        checkpoint(task)?;
        Ok(merged)
    })();
    // 一次推理失败、低质量或预算不足都保留首轮，不循环、不切换提供者。
    let result = match recovery {
        Err(error) if is_cancelled(error.as_ref()) => return Err(error),
        Err(error) if task.is_some_and(TaskToken::is_cancelled) => return Err(error),
        Ok(candidate) => candidate.unwrap_or(primary.result),
        Err(_) => primary.result,
    };
    checkpoint(task)?;
    finish_recognition(&rgb, result, task)
}

fn finish_recognition(
    rgb: &RgbImage,
    result: RecognizedText,
    task: Option<&TaskToken>,
) -> Result<RecognizedText> {
    // 候选质量、去重与补救合并已决定后才收紧投影；不改变原候选资格。
    ink_bounds::tighten(result, rgb, || checkpoint(task))
}

struct RecognitionCandidate {
    result: RecognizedText,
    confidence: f32,
    accepted_detected: Vec<usize>,
    retryable_missed: Vec<usize>,
}

#[cfg(test)]
fn recognize_lines(
    rgb: &RgbImage,
    detected: &[DetectedLine],
    cls: &mut dyn ModelRunner,
    rec: &mut dyn ModelRunner,
    dictionary: &[String],
) -> Result<RecognitionCandidate> {
    recognize_lines_with_task(rgb, detected, cls, rec, dictionary, None)
}

fn recognize_lines_with_task(
    rgb: &RgbImage,
    detected: &[DetectedLine],
    cls: &mut dyn ModelRunner,
    rec: &mut dyn ModelRunner,
    dictionary: &[String],
    task: Option<&TaskToken>,
) -> Result<RecognitionCandidate> {
    checkpoint(task)?;
    let mut result = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    let mut confidence = 1_f32;
    let mut accepted_detected = Vec::new();
    let mut retryable_missed = Vec::new();
    let mut line_quality = Vec::new();
    let mut sampling_budget = GraphemeEvidenceBudget::default();
    let mut sampling_source_bytes = Some(0usize);
    for (detected_index, line) in detected.iter().enumerate() {
        checkpoint(task)?;
        let Ok(mut crop) = Crop::new(line.quad) else {
            continue;
        };
        let classification = classification_input(rgb, &crop);
        let windows = classification.shape[0];
        checkpoint(task)?;
        // 先保留Run与输出验证的真实错误，再检查成功阶段后的取消。
        crop.rotated_180 = classify_180(cls.run(classification)?, windows)?;
        checkpoint(task)?;
        let horizontal_direction = crop.horizontal_direction();
        let (input, content_width, tensor_width) = recognition_input(rgb, &crop)?;
        checkpoint(task)?;
        let output = rec.run(input)?;
        let decoded = decode_ctc(output, dictionary)?;
        checkpoint(task)?;
        if decoded.tokens.is_empty() || decoded.confidence < 0.5 {
            retryable_missed.push(detected_index);
            continue;
        }
        confidence = confidence.min(decoded.confidence);
        let text: String = decoded
            .tokens
            .iter()
            .map(|token| token.text.as_str())
            .collect();
        let line_index = result.lines.len();
        let first_word = result.words.len();
        for fragment in fragments(&decoded.tokens) {
            checkpoint(task)?;
            let start = (fragment.start as f32 / decoded.steps as f32 * tensor_width as f32
                / content_width as f32)
                .clamp(0., 1.);
            let end = (fragment.end as f32 / decoded.steps as f32 * tensor_width as f32
                / content_width as f32)
                .clamp(start, 1.);
            let corners = [
                crop.point(start, 0.),
                crop.point(end, 0.),
                crop.point(end, 1.),
                crop.point(start, 1.),
            ];
            if let Some(rect) = bounds(&corners, rgb.width, rgb.height) {
                result.regions.push(rect);
                result.words.push(RecognizedWord {
                    text: fragment.text,
                    rect,
                    line: line_index,
                    horizontal_direction,
                    geometry: crop
                        .text_geometry(start, end)
                        .filter(|geometry| geometry_within_image(*geometry, rgb)),
                });
            }
        }
        let rect = union_rects(result.words[first_word..].iter().map(|word| word.rect))
            .or(Some(line.rect));
        let mut recognized_line = RecognizedLine {
            text,
            rect,
            horizontal_direction,
            // Retain the full valid crop, including perspective and the applied
            // orientation. The axis-aligned rect keeps its existing word-union contract.
            geometry: crop
                .text_geometry(0., 1.)
                .filter(|geometry| geometry_within_image(*geometry, rgb)),
            graphemes: None,
        };
        sampling_source_bytes = sampling_source_bytes
            .and_then(|bytes| bytes.checked_add(recognized_line.text.len()))
            .and_then(|bytes| bytes.checked_add(usize::from(line_index != 0)))
            .filter(|&bytes| bytes <= MAX_SOURCE_BYTES);
        // 新增证据只使用重写前的实际发射区间；原词框和正文仍沿用既有结果。
        // 先做整批预算守卫，再分配行内word引用和来源快照。
        if let Some(runs) = decoded.emissions.as_deref().filter(|runs| {
            sampling_source_bytes.is_some()
                && recognized_line.text.len() <= MAX_LINE_BYTES
                && sampling_budget.can_add_runs(runs.len())
        }) {
            let words: Vec<_> = result.words[first_word..].iter().collect();
            if let Some(sampling) = GraphemeSampling::from_ctc(
                line_index,
                &recognized_line,
                &words,
                runs,
                decoded.steps,
                content_width,
                tensor_width,
            ) {
                if sampling_budget.add(&sampling) {
                    recognized_line.graphemes = Some(sampling);
                }
            }
        }
        result.lines.push(recognized_line);
        accepted_detected.push(detected_index);
        line_quality.push(duplicate_lines::LineQuality {
            recognition: decoded.confidence,
            detection: line.confidence,
        });
    }
    checkpoint(task)?;
    result.text = result
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut result = duplicate_lines::deduplicate(
        result,
        &line_quality,
        Rect::full(rgb.width as u32, rgb.height as u32),
        || checkpoint(task),
    )?;
    bound_grapheme_evidence(&mut result);
    if result.lines.is_empty() {
        confidence = 0.;
    }
    checkpoint(task)?;
    Ok(RecognitionCandidate {
        result,
        confidence,
        accepted_detected,
        retryable_missed,
    })
}

const RETRY_DETECTOR_PIXELS: usize = 2_359_296;
const RETRY_LINES: usize = 8;
const RETRY_RECOGNITION_WIDTH: usize = 8192;

struct QualityRetryPlan {
    config: DetectionConfig,
    // None 只用于真正空结果；非空结果只能恢复首轮真实遗漏的高置信det范围。
    missing: Option<Vec<Rect>>,
}

fn detector_input_pixels(rgb: &RgbImage, config: DetectionConfig) -> Option<usize> {
    let padding = config.padding.checked_mul(2)?;
    let padded_width = rgb.width.checked_add(padding)?;
    let padded_height = rgb.height.checked_add(padding)?;
    let ratio = (config.max_side as f32 / padded_width.max(padded_height) as f32).min(1.);
    let width = ((padded_width as f32 * ratio / 32.).round() as usize)
        .max(1)
        .checked_mul(32)?;
    let height = ((padded_height as f32 * ratio / 32.).round() as usize)
        .max(1)
        .checked_mul(32)?;
    width.checked_mul(height)
}

fn quality_retry_plan(
    image: &Image,
    rgb: &RgbImage,
    primary: &RecognitionCandidate,
    detected: &[DetectedLine],
) -> Option<QualityRetryPlan> {
    // 正常多行无条件保留；合法短词、单字、标点和低面积覆盖都不是遗漏证据。
    // 去重只改变展示结果；原来成功识别的det数量和最低置信仍决定补救资格。
    if primary.accepted_detected.len() >= 2
        || primary.result.lines.len() >= 2
        || !valid_quality_contract(&primary.result, rgb)
    {
        return None;
    }
    let missing = if primary.result.lines.is_empty() {
        None
    } else {
        let missing: Vec<_> = primary
            .retryable_missed
            .iter()
            .filter(|index| !primary.accepted_detected.contains(index))
            .filter_map(|&index| detected.get(index))
            .filter(|line| {
                line.confidence.is_finite()
                    && line.confidence >= 0.65
                    && line.rect.valid(rgb.width as u32, rgb.height as u32)
            })
            .map(|line| line.rect)
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(missing)
    };
    let mut config = default_config(image, 1.6);
    config.max_side = config.max_side.max(1536);
    config.threshold = 0.25;
    config.box_threshold = 0.45;
    if detector_input_pixels(rgb, config)? > RETRY_DETECTOR_PIXELS {
        return None;
    }
    // 精确扫描已合成的每个RGB像素，不用稀疏抽样把细字或边缘误认作空图。
    let first = *rgb.pixels.first()?;
    if rgb.pixels.iter().all(|pixel| *pixel == first) {
        return None;
    }
    Some(QualityRetryPlan { config, missing })
}

fn quality_retry_targets(
    rgb: &RgbImage,
    primary: &RecognitionCandidate,
    plan: &QualityRetryPlan,
    retried: Vec<DetectedLine>,
) -> Option<Vec<DetectedLine>> {
    let selected: Vec<_> = retried
        .into_iter()
        .filter(|line| {
            if !line.rect.valid(rgb.width as u32, rgb.height as u32) {
                return false;
            }
            plan.missing.as_ref().is_none_or(|missing| {
                // 至少一半新框被原遗漏范围支持，不在另一个页面位置发明新行。
                missing.iter().any(|rect| {
                    line.rect.intersection(*rect).is_some_and(|overlap| {
                        u64::from(overlap.width) * u64::from(overlap.height) * 2
                            >= u64::from(line.rect.width) * u64::from(line.rect.height)
                    })
                }) && primary.result.lines.iter().all(|old| {
                    old.rect
                        .is_none_or(|rect| line.rect.intersection(rect).is_none())
                })
            })
        })
        .collect();
    if !quality_retry_workload(rgb, &selected) {
        return None;
    }
    // 重叠候选不能靠字数选择优胜；不提交可能重复同一行的补救结果。
    for (index, line) in selected.iter().enumerate() {
        if selected[index + 1..]
            .iter()
            .any(|other| line.rect.intersection(other.rect).is_some())
        {
            return None;
        }
    }
    Some(selected)
}

fn quality_retry_workload(rgb: &RgbImage, selected: &[DetectedLine]) -> bool {
    if selected.is_empty() || selected.len() > RETRY_LINES {
        return false;
    }
    let mut width_sum = 0usize;
    for line in selected {
        if !line.rect.valid(rgb.width as u32, rgb.height as u32) {
            return false;
        }
        let Ok(crop) = Crop::new(line.quad) else {
            return false;
        };
        let Ok((_, width)) = recognition_width(crop.width, crop.height) else {
            return false;
        };
        let Some(sum) = width_sum.checked_add(width) else {
            return false;
        };
        if sum > RETRY_RECOGNITION_WIDTH {
            return false;
        }
        width_sum = sum;
    }
    true
}

fn valid_quality_contract(result: &RecognizedText, rgb: &RgbImage) -> bool {
    if result.text
        != result
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
        || result.words.len() != result.regions.len()
    {
        return false;
    }
    if result.lines.is_empty() {
        return result.text.is_empty() && result.words.is_empty();
    }
    result
        .words
        .iter()
        .zip(&result.regions)
        .all(|(word, rect)| {
            word.line < result.lines.len()
                && !word.text.trim().is_empty()
                && word.rect == *rect
                && rect.valid(rgb.width as u32, rgb.height as u32)
                && word.horizontal_direction == result.lines[word.line].horizontal_direction
        })
        && result.lines.iter().enumerate().all(|(index, line)| {
            !line.text.trim().is_empty()
                && line
                    .rect
                    .is_some_and(|rect| rect.valid(rgb.width as u32, rgb.height as u32))
                && union_rects(
                    result
                        .words
                        .iter()
                        .filter(|word| word.line == index)
                        .map(|word| word.rect),
                ) == line.rect
        })
}

fn valid_quality_candidate(candidate: &RecognitionCandidate, rgb: &RgbImage) -> bool {
    candidate.confidence.is_finite()
        && candidate.confidence >= 0.9
        && !candidate.result.lines.is_empty()
        && valid_quality_contract(&candidate.result, rgb)
}

fn merge_quality_recovery(
    primary: &RecognizedText,
    mut recovered: RecognizedText,
    rgb: &RgbImage,
) -> Option<RecognizedText> {
    if !valid_quality_contract(primary, rgb) || !valid_quality_contract(&recovered, rgb) {
        return None;
    }
    if primary.lines.is_empty() {
        // 空结果的候选保留recognize_lines给出的视觉顺序，不按词框重新排版。
        bound_grapheme_evidence(&mut recovered);
        return Some(recovered);
    }
    if recovered.lines.iter().any(|new| {
        primary.lines.iter().any(|old| {
            new.rect
                .zip(old.rect)
                .is_some_and(|(new, old)| new.intersection(old).is_some())
        })
    }) {
        return None;
    }
    // 首轮对象仅逐字段复制；候选绝不重新识别、重写或重投影旧行和词。
    let mut rows = Vec::new();
    let source_in_budget = primary
        .text
        .len()
        .checked_add(recovered.text.len())
        .and_then(|bytes| bytes.checked_add(usize::from(!recovered.lines.is_empty())))
        .is_some_and(|bytes| bytes <= MAX_SOURCE_BYTES);
    let mut sampling_budget = GraphemeEvidenceBudget::default();
    for source in [primary, &recovered] {
        for (index, line) in source.lines.iter().enumerate() {
            let words: Vec<_> = source
                .words
                .iter()
                .filter(|word| word.line == index)
                .map(|word| RecognizedWord {
                    text: word.text.clone(),
                    rect: word.rect,
                    line: word.line,
                    horizontal_direction: word.horizontal_direction,
                    geometry: word.geometry,
                })
                .collect();
            rows.push((
                RecognizedLine {
                    text: line.text.clone(),
                    rect: line.rect,
                    horizontal_direction: line.horizontal_direction,
                    geometry: line.geometry,
                    // line-local绑定不含旧全局行号，完整克隆后可随行稳定重排。
                    graphemes: line
                        .graphemes
                        .as_ref()
                        .filter(|sampling| source_in_budget && sampling_budget.add(sampling))
                        .cloned(),
                },
                words,
            ));
        }
    }
    rows.sort_by_key(|(line, _)| line.rect.expect("通过原图合同的行有范围").y);
    // 与det的既有视觉行合同相同：先沿y稳定分组，相邻差严格小于10px
    // 的一组再按x稳定排序；不按词的y抖动交换同一视觉行，不重排列。
    let mut first = 0;
    while first < rows.len() {
        let mut end = first + 1;
        while end < rows.len()
            && rows[end].0.rect.expect("通过原图合同的行有范围").y
                - rows[end - 1].0.rect.expect("通过原图合同的行有范围").y
                < 10
        {
            end += 1;
        }
        rows[first..end].sort_by_key(|(line, _)| line.rect.expect("通过原图合同的行有范围").x);
        first = end;
    }
    let mut result = RecognizedText {
        text: String::new(),
        regions: Vec::new(),
        words: Vec::new(),
        lines: Vec::new(),
    };
    for (index, (line, words)) in rows.into_iter().enumerate() {
        result.lines.push(line);
        for mut word in words {
            word.line = index;
            result.regions.push(word.rect);
            result.words.push(word);
        }
    }
    result.text = result
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    bound_grapheme_evidence(&mut result);
    valid_quality_contract(&result, rgb).then_some(result)
}

#[derive(Default)]
struct GraphemeEvidenceBudget {
    graphemes: usize,
    runs: usize,
}

impl GraphemeEvidenceBudget {
    fn can_add_runs(&self, runs: usize) -> bool {
        self.graphemes < MAX_TOTAL_GRAPHEMES
            && self
                .runs
                .checked_add(runs)
                .is_some_and(|total| total <= MAX_TOTAL_RUNS)
    }

    fn add(&mut self, sampling: &GraphemeSampling) -> bool {
        let Some(graphemes) = self.graphemes.checked_add(sampling.grapheme_count()) else {
            return false;
        };
        let Some(runs) = self.runs.checked_add(sampling.run_count()) else {
            return false;
        };
        if graphemes > MAX_TOTAL_GRAPHEMES || runs > MAX_TOTAL_RUNS {
            return false;
        }
        self.graphemes = graphemes;
        self.runs = runs;
        true
    }
}

/// 文本和原OCR对象不变；超出整批新增证据预算时仅去掉可选元数据。
fn bound_grapheme_evidence(result: &mut RecognizedText) {
    let mut budget = GraphemeEvidenceBudget::default();
    for line in &mut result.lines {
        if result.text.len() > MAX_SOURCE_BYTES
            || line
                .graphemes
                .as_ref()
                .is_some_and(|sampling| !budget.add(sampling))
        {
            line.graphemes = None;
        }
    }
}

fn valid_rec_only_result(result: &RecognizedText, rgb: &RgbImage) -> bool {
    result.lines.len() == 1
        && !result.text.trim().is_empty()
        && result.lines[0].text == result.text
        && result.lines[0]
            .rect
            .is_some_and(|rect| rect.valid(rgb.width as u32, rgb.height as u32))
        && !result.words.is_empty()
        && result.words.len() == result.regions.len()
        && result
            .words
            .iter()
            .zip(&result.regions)
            .all(|(word, rect)| {
                word.line == 0
                    && !word.text.trim().is_empty()
                    && word.rect == *rect
                    && rect.valid(rgb.width as u32, rgb.height as u32)
                    && word.horizontal_direction == result.lines[0].horizontal_direction
            })
}

/// A bounded, conservative image gate; uncertain crops retain ordinary detection.
/// Edge energy works for dark and light text after the existing alpha composite.
fn rec_only_line(rgb: &RgbImage) -> Option<DetectedLine> {
    let (width, height) = (rgb.width, rgb.height);
    if width < 32
        || !(12..=60).contains(&height)
        || (width as f32) < height as f32 * 2.5
        || recognition_width(width, height).is_err()
    {
        return None;
    }
    // The recognizer width budget and height gate bound this scan to 1.23M pixels.
    // A stride or central difference can alias thin strokes and hide a second line.
    let edge = |x: usize, y: usize| {
        let luma = |pixel: [f32; 3]| (pixel[0] * 30. + pixel[1] * 59. + pixel[2] * 11.) / 100.;
        (luma(rgb.pixels[y * width + x]) - luma(rgb.pixels[y * width + x - 1])).abs()
    };
    let mut energies = vec![0.; height];
    for (y, energy) in energies.iter_mut().enumerate().take(height - 1).skip(1) {
        let mut sum = 0.;
        let mut samples = 0;
        for x in 1..width {
            sum += edge(x, y);
            samples += 1;
        }
        *energy = sum / samples as f32;
    }
    let original = energies.clone();
    for y in 1..height - 1 {
        energies[y] = (original[y - 1] + original[y] * 2. + original[y + 1]) / 4.;
    }
    let peak = energies.iter().copied().fold(0_f32, f32::max);
    if peak < 4. {
        return None;
    }
    let mut sorted = energies.clone();
    sorted.sort_by(f32::total_cmp);
    let threshold = (sorted[height / 2] * 1.35).max(peak * 0.28);
    let active: Vec<_> = energies.iter().map(|energy| *energy >= threshold).collect();
    let mut band = None;
    let mut start = None;
    for (index, active) in active.iter().copied().chain([false]).enumerate() {
        if active {
            start.get_or_insert(index);
        } else if let Some(first) = start.take() {
            if index - first >= (height / 24).max(2) {
                if band.is_some() {
                    return None;
                }
                band = Some((first, index));
            }
        }
    }
    let (top, bottom) = band?;
    let band_height = bottom - top;
    if band_height < (height / 8).max(4) || band_height as f32 > height as f32 * 0.82 {
        return None;
    }
    // Smoothing itself can bridge a one-pixel gap between two lines. Require the
    // raw band to be continuous; a confident recognizer can still omit a line.
    // The peak threshold can also hide a faint second line outside the main band.
    if original[top..bottom].iter().any(|energy| *energy < 1.)
        || original[..top]
            .iter()
            .chain(&original[bottom..])
            .any(|energy| *energy >= 1.)
    {
        return None;
    }
    // A local outline or a couple of remote edges can form one vertical band without
    // being a text line. Require distributed edges across a useful source width.
    let mut first_edge = None;
    let mut last_edge = 0;
    let mut edge_count = 0;
    let mut columns = 0;
    let mut occupied = [false; 8];
    let column_threshold = (peak * 0.18).max(4.);
    for x in 1..width {
        columns += 1;
        let energy = (top..bottom).map(|y| edge(x, y)).sum::<f32>() / band_height as f32;
        if energy >= column_threshold {
            first_edge.get_or_insert(x);
            last_edge = x;
            edge_count += 1;
            let bin = (x * occupied.len() / width).min(occupied.len() - 1);
            occupied[bin] = true;
        }
    }
    if edge_count < 6
        || (edge_count as f32) < columns as f32 * 0.08
        || occupied.iter().filter(|occupied| **occupied).count() < 3
        || ((last_edge - first_edge?) as f32) < width as f32 * 0.35
    {
        return None;
    }
    // A full-image quad makes CTC's first/last fragments inherit remote margins.
    // Keep two physical pixels around the band, but prove the background outside
    // it is uniform before trimming. RGB differences also retain equal-luma or
    // one-level strokes that the strong distribution gate must not delimit.
    let crop_top = top.saturating_sub(2);
    let crop_bottom = (bottom + 2).min(height);
    let difference = |a: [f32; 3], b: [f32; 3]| {
        a.into_iter()
            .zip(b)
            .map(|(a, b)| (a - b).abs())
            .fold(0_f32, f32::max)
    };
    let background = rgb.pixels[0];
    let mut first_ink = None;
    let mut last_ink = 0;
    let mut first_weak_edge = None;
    let mut last_weak_edge = 0;
    for y in 0..height {
        for x in 0..width {
            let pixel = rgb.pixels[y * width + x];
            let ink = difference(pixel, background) >= 1.;
            let weak_edge = x > 0 && difference(pixel, rgb.pixels[y * width + x - 1]) >= 1.;
            if !(crop_top..crop_bottom).contains(&y) && (ink || weak_edge) {
                return None;
            }
            if ink {
                first_ink = Some(first_ink.map_or(x, |first: usize| first.min(x)));
                last_ink = last_ink.max(x);
            }
            if weak_edge {
                first_weak_edge =
                    Some(first_weak_edge.map_or(x - 1, |first: usize| first.min(x - 1)));
                last_weak_edge = last_weak_edge.max(x);
            }
        }
    }
    let first_ink = first_ink?;
    // Both ends differing from the corner background could be a colored stripe
    // or an uncertain background, rather than safely bounded text support.
    if first_ink == 0 && last_ink == width - 1 {
        return None;
    }
    let crop_left = first_ink.saturating_sub(2);
    let crop_right = (last_ink + 3).min(width);
    // A weak edge without a >=1 background difference is possible after alpha
    // compositing. Never silently remove it merely because its pixels are faint.
    if first_weak_edge? < crop_left || last_weak_edge >= crop_right {
        return None;
    }
    let crop_width = crop_right - crop_left;
    let crop_height = crop_bottom - crop_top;
    // A shorter tight crop can exceed the recognizer budget even when the full
    // image's normalized width was legal. Check the actual tensor dimensions.
    if recognition_width(crop_width, crop_height).is_err() {
        return None;
    }
    Some(DetectedLine {
        quad: [
            [crop_left as f32, crop_top as f32],
            [crop_right as f32, crop_top as f32],
            [crop_right as f32, crop_bottom as f32],
            [crop_left as f32, crop_bottom as f32],
        ],
        confidence: 1.,
        rect: Rect {
            x: crop_left as u32,
            y: crop_top as u32,
            width: crop_width as u32,
            height: crop_height as u32,
        },
    })
}

struct RgbImage {
    width: usize,
    height: usize,
    pixels: Vec<[f32; 3]>,
}

impl RgbImage {
    fn from_bgra(image: &Image) -> Result<Self> {
        let width = image.width as usize;
        let height = image.height as usize;
        let count = width.checked_mul(height).ok_or("OCR 图像尺寸溢出")?;
        if width == 0
            || height == 0
            || count > 32_000_000
            || count.checked_mul(4) != Some(image.bgra.len())
        {
            return Err("OCR 图像尺寸或 BGRA 长度无效".into());
        }
        let pixels = image
            .bgra
            .chunks_exact(4)
            .map(|pixel| {
                let alpha = f32::from(pixel[3]) / 255.;
                [2, 1, 0].map(|channel| f32::from(pixel[channel]) * alpha + 255. * (1. - alpha))
            })
            .collect();
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    fn sample(&self, point: Point) -> [f32; 3] {
        let x = point[0].clamp(0., (self.width - 1) as f32);
        let y = point[1].clamp(0., (self.height - 1) as f32);
        let x0 = x.floor() as usize;
        let y0 = y.floor() as usize;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let dx = x - x0 as f32;
        let dy = y - y0 as f32;
        std::array::from_fn(|channel| {
            let top = self.pixels[y0 * self.width + x0][channel] * (1. - dx)
                + self.pixels[y0 * self.width + x1][channel] * dx;
            let bottom = self.pixels[y1 * self.width + x0][channel] * (1. - dx)
                + self.pixels[y1 * self.width + x1][channel] * dx;
            top * (1. - dy) + bottom * dy
        })
    }
}

fn tensor_count(tensor: &Tensor) -> Result<usize> {
    let count = tensor
        .shape
        .iter()
        .try_fold(1usize, |count, &side| count.checked_mul(side))
        .ok_or("OCR 模型输出尺寸溢出")?;
    if count == 0
        || count != tensor.values.len()
        || count > 64_000_000
        || tensor.values.iter().any(|value| !value.is_finite())
    {
        return Err("OCR 模型输出形状或数值无效".into());
    }
    Ok(count)
}

// RapidOcrNet's fixed v6 path writes Bgra8888 byte channels 0/1/2 to NCHW.
// Keep image sampling in RGB and apply the model's B/G/R order only at this boundary.
fn normalized_bgr(pixel: [f32; 3]) -> [f32; 3] {
    [pixel[2], pixel[1], pixel[0]].map(|value| value / 127.5 - 1.)
}

#[cfg(test)]
fn detect_rgb(
    rgb: &RgbImage,
    det: &mut dyn ModelRunner,
    config: DetectionConfig,
) -> Result<Vec<DetectedLine>> {
    detect_rgb_with_task(rgb, det, config, None)
}

fn detect_rgb_with_task(
    rgb: &RgbImage,
    det: &mut dyn ModelRunner,
    config: DetectionConfig,
    task: Option<&TaskToken>,
) -> Result<Vec<DetectedLine>> {
    checkpoint(task)?;
    if !(32..=4096).contains(&config.max_side)
        || config.padding > 256
        || config.max_candidates == 0
        || config.max_candidates > 10000
        || !config.threshold.is_finite()
        || !(0. ..=1.).contains(&config.threshold)
        || !config.box_threshold.is_finite()
        || !(0. ..=1.).contains(&config.box_threshold)
        || !config.unclip_ratio.is_finite()
        || !(0. ..=10.).contains(&config.unclip_ratio)
    {
        return Err("OCR 检测参数无效".into());
    }
    let padded_width = rgb.width + config.padding * 2;
    let padded_height = rgb.height + config.padding * 2;
    let ratio = (config.max_side as f32 / padded_width.max(padded_height) as f32).min(1.);
    let width = ((padded_width as f32 * ratio / 32.).round() as usize).max(1) * 32;
    let height = ((padded_height as f32 * ratio / 32.).round() as usize).max(1) * 32;
    let mut values = vec![1.; width * height * 3];
    for y in 0..height {
        checkpoint(task)?;
        for x in 0..width {
            let sx =
                (x as f32 + 0.5) * padded_width as f32 / width as f32 - 0.5 - config.padding as f32;
            let sy = (y as f32 + 0.5) * padded_height as f32 / height as f32
                - 0.5
                - config.padding as f32;
            if sx < 0. || sy < 0. || sx >= rgb.width as f32 || sy >= rgb.height as f32 {
                continue;
            }
            let pixel = rgb.sample([sx, sy]);
            for (channel, value) in normalized_bgr(pixel).into_iter().enumerate() {
                values[channel * width * height + y * width + x] = value;
            }
        }
    }
    checkpoint(task)?;
    let output = det.run(Tensor {
        shape: vec![1, 3, height, width],
        values,
    })?;
    tensor_count(&output)?;
    let (map_height, map_width) = match output.shape.as_slice() {
        [1, 1, h, w] | [1, h, w] => (*h, *w),
        _ => return Err(format!("OCR 检测输出应为 [1,1,H,W]，实际 {:?}", output.shape).into()),
    };
    if map_height > 4096
        || map_width > 4096
        || output
            .values
            .iter()
            .any(|value| !(0. ..=1.).contains(value))
    {
        return Err("OCR 检测概率图无效".into());
    }
    checkpoint(task)?;
    let mut mask = vec![false; map_height * map_width];
    // DB's 3x3 dilation connects nearby high-probability pixels before contour tracing.
    for y in 0..map_height {
        checkpoint(task)?;
        for x in 0..map_width {
            if output.values[y * map_width + x] <= config.threshold {
                continue;
            }
            for ny in y.saturating_sub(1)..=(y + 1).min(map_height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(map_width - 1) {
                    mask[ny * map_width + nx] = true;
                }
            }
        }
    }
    checkpoint(task)?;
    let contours = contours(&mask, map_width, map_height, config.max_candidates);
    checkpoint(task)?;
    let mut result = Vec::new();
    for contour in contours {
        checkpoint(task)?;
        let Some(mut quad) = minimum_rectangle(&contour) else {
            continue;
        };
        let w = distance(quad[0], quad[1]);
        let h = distance(quad[1], quad[2]);
        if w.min(h) < 3. {
            continue;
        }
        let confidence = contour_score(&contour, &output.values, map_width, map_height);
        if confidence < config.box_threshold {
            continue;
        }
        // A round offset of a rectangle has the same enclosing axes and expands each edge
        // by area * unclip / perimeter. This avoids a native polygon-offset dependency.
        let offset = w * h * config.unclip_ratio / (2. * (w + h));
        quad = expand_rectangle(quad, offset);
        if distance(quad[0], quad[1]).min(distance(quad[1], quad[2])) < 5. {
            continue;
        }
        for point in &mut quad {
            point[0] = (point[0] * padded_width as f32 / map_width as f32 - config.padding as f32)
                .clamp(0., rgb.width as f32);
            point[1] = (point[1] * padded_height as f32 / map_height as f32
                - config.padding as f32)
                .clamp(0., rgb.height as f32);
        }
        if !valid_quad(&quad) {
            continue;
        }
        let Some(rect) = bounds(&quad, rgb.width, rgb.height) else {
            continue;
        };
        result.push(DetectedLine {
            quad,
            confidence,
            rect,
        });
    }
    result.sort_by(|a, b| a.quad[0][1].total_cmp(&b.quad[0][1]));
    // Stable visual rows, then left-to-right; no multi-column text rewriting.
    let mut first = 0;
    while first < result.len() {
        checkpoint(task)?;
        let mut end = first + 1;
        while end < result.len() && result[end].quad[0][1] - result[end - 1].quad[0][1] < 10. {
            end += 1;
        }
        result[first..end].sort_by(|a, b| a.quad[0][0].total_cmp(&b.quad[0][0]));
        first = end;
    }
    checkpoint(task)?;
    Ok(result)
}

const NEIGHBORS: [(isize, isize); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];

fn foreground(mask: &[bool], width: usize, height: usize, x: isize, y: isize) -> bool {
    x >= 0
        && y >= 0
        && (x as usize) < width
        && (y as usize) < height
        && mask[y as usize * width + x as usize]
}

fn contours(mask: &[bool], width: usize, height: usize, limit: usize) -> Vec<Vec<Point>> {
    let mut seen = vec![false; mask.len()];
    let mut queue = VecDeque::new();
    let mut result = Vec::new();
    for index in 0..mask.len() {
        if !mask[index] || seen[index] {
            continue;
        }
        seen[index] = true;
        queue.push_back(index);
        let mut component_pixels = 0usize;
        while let Some(pixel) = queue.pop_front() {
            component_pixels += 1;
            let x = (pixel % width) as isize;
            let y = (pixel / width) as isize;
            for (dx, dy) in NEIGHBORS {
                if foreground(mask, width, height, x + dx, y + dy) {
                    let next = (y + dy) as usize * width + (x + dx) as usize;
                    if !seen[next] {
                        seen[next] = true;
                        queue.push_back(next);
                    }
                }
            }
        }
        let start = ((index % width) as isize, (index / width) as isize);
        let mut current = start;
        let mut back = (start.0 - 1, start.1);
        let mut first_next = None;
        let mut contour = Vec::new();
        let mut closed = false;
        // Each pixel has only eight incoming boundary states. An unclosed walk is
        // discarded instead of returning a partial contour or scanning unrelated pixels.
        for _ in 0..component_pixels.saturating_mul(8) {
            let back_index = NEIGHBORS
                .iter()
                .position(|&(dx, dy)| (current.0 + dx, current.1 + dy) == back)
                .unwrap_or(4);
            let found = (1..=8).map(|step| (back_index + step) % 8).find(|&i| {
                foreground(
                    mask,
                    width,
                    height,
                    current.0 + NEIGHBORS[i].0,
                    current.1 + NEIGHBORS[i].1,
                )
            });
            let Some(direction) = found else {
                break;
            };
            let next = (
                current.0 + NEIGHBORS[direction].0,
                current.1 + NEIGHBORS[direction].1,
            );
            if current == start && first_next == Some(next) {
                closed = true;
                break;
            }
            if first_next.is_none() {
                first_next = Some(next);
            }
            contour.push([current.0 as f32, current.1 as f32]);
            let previous = NEIGHBORS[(direction + 7) % 8];
            back = (current.0 + previous.0, current.1 + previous.1);
            current = next;
        }
        if closed && contour.len() >= 3 {
            // Equivalent to CHAIN_APPROX_SIMPLE: retain turns and reversals rather
            // than every pixel on a straight edge before polygon scoring.
            let simplified: Vec<_> = (0..contour.len())
                .filter_map(|i| {
                    let previous = contour[(i + contour.len() - 1) % contour.len()];
                    let point = contour[i];
                    let next = contour[(i + 1) % contour.len()];
                    let dot = (point[0] - previous[0]) * (next[0] - point[0])
                        + (point[1] - previous[1]) * (next[1] - point[1]);
                    (cross(previous, point, next) != 0. || dot <= 0.).then_some(point)
                })
                .collect();
            if simplified.len() >= 3 {
                result.push(simplified);
            }
        }
        if result.len() >= limit {
            break;
        }
    }
    result
}

fn cross(origin: Point, a: Point, b: Point) -> f32 {
    (a[0] - origin[0]) * (b[1] - origin[1]) - (a[1] - origin[1]) * (b[0] - origin[0])
}

fn hull(points: &[Point]) -> Vec<Point> {
    let mut points = points.to_vec();
    points.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    let mut result = Vec::new();
    for &point in &points {
        while result.len() >= 2
            && cross(result[result.len() - 2], result[result.len() - 1], point) <= 0.
        {
            result.pop();
        }
        result.push(point);
    }
    let lower = result.len();
    for &point in points.iter().rev().skip(1) {
        while result.len() > lower
            && cross(result[result.len() - 2], result[result.len() - 1], point) <= 0.
        {
            result.pop();
        }
        result.push(point);
    }
    result.pop();
    result
}

fn minimum_rectangle(points: &[Point]) -> Option<[Point; 4]> {
    let polygon = hull(points);
    if polygon.len() < 3 {
        return None;
    }
    let mut best_area = f32::INFINITY;
    let mut best = None;
    for i in 0..polygon.len() {
        let first = polygon[i];
        let second = polygon[(i + 1) % polygon.len()];
        let length = distance(first, second);
        if length < f32::EPSILON {
            continue;
        }
        let ux = (second[0] - first[0]) / length;
        let uy = (second[1] - first[1]) / length;
        let mut min_u = f32::INFINITY;
        let mut min_v = f32::INFINITY;
        let mut max_u = f32::NEG_INFINITY;
        let mut max_v = f32::NEG_INFINITY;
        for &[x, y] in &polygon {
            let u = x * ux + y * uy;
            let v = -x * uy + y * ux;
            min_u = min_u.min(u);
            max_u = max_u.max(u);
            min_v = min_v.min(v);
            max_v = max_v.max(v);
        }
        let area = (max_u - min_u) * (max_v - min_v);
        if area < best_area {
            best_area = area;
            best = Some(
                [
                    [min_u, min_v],
                    [max_u, min_v],
                    [max_u, max_v],
                    [min_u, max_v],
                ]
                .map(|[u, v]| [u * ux - v * uy, u * uy + v * ux]),
            );
        }
    }
    let mut quad = best?;
    quad.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    let (tl, bl) = if quad[0][1] <= quad[1][1] {
        (quad[0], quad[1])
    } else {
        (quad[1], quad[0])
    };
    let (tr, br) = if quad[2][1] <= quad[3][1] {
        (quad[2], quad[3])
    } else {
        (quad[3], quad[2])
    };
    Some([tl, tr, br, bl])
}

fn distance(a: Point, b: Point) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn valid_quad(quad: &[Point; 4]) -> bool {
    if quad.iter().flatten().any(|value| !value.is_finite()) {
        return false;
    }
    let turns =
        std::array::from_fn::<_, 4, _>(|i| cross(quad[i], quad[(i + 1) % 4], quad[(i + 2) % 4]));
    turns.iter().all(|turn| *turn > 1e-4) || turns.iter().all(|turn| *turn < -1e-4)
}

fn expand_rectangle(quad: [Point; 4], amount: f32) -> [Point; 4] {
    let w = distance(quad[0], quad[1]);
    let h = distance(quad[0], quad[3]);
    let ux = [(quad[1][0] - quad[0][0]) / w, (quad[1][1] - quad[0][1]) / w];
    let uy = [(quad[3][0] - quad[0][0]) / h, (quad[3][1] - quad[0][1]) / h];
    std::array::from_fn(|i| {
        let sx = if i == 0 || i == 3 { -1. } else { 1. };
        let sy = if i < 2 { -1. } else { 1. };
        [
            quad[i][0] + amount * (sx * ux[0] + sy * uy[0]),
            quad[i][1] + amount * (sx * ux[1] + sy * uy[1]),
        ]
    })
}

/// 射线奇偶与边界判定；只传入纵向接触该点所在行的边，结果与遍历整圈相同。
fn edges_contain(edges: &[(Point, Point)], point: Point) -> bool {
    let mut inside = false;
    for &(a, b) in edges {
        if cross(a, b, point).abs() < 1e-4
            && point[0] >= a[0].min(b[0])
            && point[0] <= a[0].max(b[0])
            && point[1] >= a[1].min(b[1])
            && point[1] <= a[1].max(b[1])
        {
            return true;
        }
        if (a[1] > point[1]) != (b[1] > point[1])
            && point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
    }
    inside
}

fn contour_score(polygon: &[Point], probabilities: &[f32], width: usize, height: usize) -> f32 {
    let Some(rect) = bounds(polygon, width, height) else {
        return 0.;
    };
    // 不接触某行的边既不翻转该行射线奇偶，也不可能让该行像素落在边上；
    // 先按行分桶，逐像素只检查本行几条边，避免长文本行轮廓的像素×顶点开销。
    let top = rect.y as usize;
    let mut rows = vec![Vec::new(); rect.height as usize];
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[(i + 1) % polygon.len()];
        let low = a[1].min(b[1]).ceil().max(top as f32);
        let high = a[1].max(b[1]).floor().min((top + rows.len() - 1) as f32);
        if low <= high {
            for y in low as usize..=high as usize {
                rows[y - top].push((a, b));
            }
        }
    }
    let mut sum = 0.;
    let mut count = 0;
    for (row, edges) in rows.iter().enumerate() {
        let y = top + row;
        for x in rect.x as usize..(rect.x + rect.width) as usize {
            if edges_contain(edges, [x as f32, y as f32]) {
                sum += probabilities[y * width + x];
                count += 1;
            }
        }
    }
    if count == 0 {
        0.
    } else {
        sum / count as f32
    }
}

fn bounds(points: &[Point], width: usize, height: usize) -> Option<Rect> {
    if points.is_empty() || points.iter().flatten().any(|value| !value.is_finite()) {
        return None;
    }
    let left = points
        .iter()
        .map(|p| p[0])
        .fold(f32::INFINITY, f32::min)
        .floor()
        .clamp(0., width as f32) as u32;
    let top = points
        .iter()
        .map(|p| p[1])
        .fold(f32::INFINITY, f32::min)
        .floor()
        .clamp(0., height as f32) as u32;
    let right = points
        .iter()
        .map(|p| p[0])
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .clamp(0., width as f32) as u32;
    let bottom = points
        .iter()
        .map(|p| p[1])
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .clamp(0., height as f32) as u32;
    (right > left && bottom > top).then_some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn union_rects(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    rects.reduce(|a, b| {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Rect {
            x,
            y,
            width: (a.x + a.width).max(b.x + b.width) - x,
            height: (a.y + a.height).max(b.y + b.height) - y,
        }
    })
}

struct Crop {
    width: usize,
    height: usize,
    transform: [f64; 8],
    rotated_180: bool,
}

impl Crop {
    /// Reading-ordered CTC band, in the unchanged source image. This shares the
    /// sampling transform and is deliberately an estimate rather than ink bounds.
    fn text_geometry(&self, start: f32, end: f32) -> Option<TextGeometry> {
        if !start.is_finite()
            || !end.is_finite()
            || !(0. ..=1.).contains(&start)
            || !(0. ..=1.).contains(&end)
            || end <= start
        {
            return None;
        }
        let quad = [
            self.point(start, 0.),
            self.point(end, 0.),
            self.point(end, 1.),
            self.point(start, 1.),
        ];
        valid_quad(&quad).then_some(TextGeometry {
            quad: quad.map(|[x, y]| TextPoint { x, y }),
        })
    }

    fn horizontal_direction(&self) -> HorizontalDirection {
        if self.point(1., 0.5)[0] < self.point(0., 0.5)[0] {
            HorizontalDirection::RightToLeft
        } else {
            HorizontalDirection::LeftToRight
        }
    }

    fn new(mut quad: [Point; 4]) -> Result<Self> {
        if !valid_quad(&quad) {
            return Err("OCR 检测框退化或自交".into());
        }
        let mut width = distance(quad[0], quad[1])
            .max(distance(quad[3], quad[2]))
            .round() as usize;
        let mut height = distance(quad[0], quad[3])
            .max(distance(quad[1], quad[2]))
            .round() as usize;
        if width == 0 || height == 0 {
            return Err("OCR 检测框退化".into());
        }
        if height as f32 / width as f32 >= 1.5 {
            quad = [quad[1], quad[2], quad[3], quad[0]];
            std::mem::swap(&mut width, &mut height);
        }
        let q = quad.map(|p| [f64::from(p[0]), f64::from(p[1])]);
        let dx1 = q[1][0] - q[2][0];
        let dx2 = q[3][0] - q[2][0];
        let dy1 = q[1][1] - q[2][1];
        let dy2 = q[3][1] - q[2][1];
        let dx3 = q[0][0] - q[1][0] + q[2][0] - q[3][0];
        let dy3 = q[0][1] - q[1][1] + q[2][1] - q[3][1];
        let denominator = dx1 * dy2 - dx2 * dy1;
        if denominator.abs() < 1e-8 {
            return Err("OCR 透视矩阵不可逆".into());
        }
        let g = (dx3 * dy2 - dx2 * dy3) / denominator;
        let h = (dx1 * dy3 - dx3 * dy1) / denominator;
        let transform = [
            q[1][0] - q[0][0] + g * q[1][0],
            q[3][0] - q[0][0] + h * q[3][0],
            q[0][0],
            q[1][1] - q[0][1] + g * q[1][1],
            q[3][1] - q[0][1] + h * q[3][1],
            q[0][1],
            g,
            h,
        ];
        if transform.iter().any(|value| !value.is_finite())
            || [1., 1. + g, 1. + h, 1. + g + h]
                .iter()
                .any(|value| *value <= 1e-8)
        {
            return Err("OCR 透视矩阵无效".into());
        }
        Ok(Self {
            width,
            height,
            transform,
            rotated_180: false,
        })
    }
    fn point(&self, u: f32, v: f32) -> Point {
        let t = self.transform;
        // 采样和 CTC 词框共用此逆变换，翻转只影响识别副本，不改原图。
        let (u, v) = if self.rotated_180 {
            (1. - u, 1. - v)
        } else {
            (u, v)
        };
        let u = f64::from(u);
        let v = f64::from(v);
        let denominator = t[6] * u + t[7] * v + 1.;
        [
            ((t[0] * u + t[1] * v + t[2]) / denominator) as f32,
            ((t[3] * u + t[4] * v + t[5]) / denominator) as f32,
        ]
    }
}

/// Do not clip a projected quad independently of its homography. Outside-image
/// metadata falls back to the existing safe, clipped axis-aligned word rectangles.
fn geometry_within_image(geometry: TextGeometry, rgb: &RgbImage) -> bool {
    geometry
        .validated_in(Rect {
            x: 0,
            y: 0,
            width: rgb.width as u32,
            height: rgb.height as u32,
        })
        .is_some()
}

// 长行整体压进 160 宽时字形被横向压扁十几倍，分类器会给出高置信的随机方向
// （Arial 14px 宽栏英文曾被判为 180°）。按自然宽度切成至多 8 个均匀分布的窗口，
// 每窗横向压缩不超过 4 倍，一次批量推理后取平均；短行仍是单窗、与原输入一致。
const CLASSIFICATION_WINDOW_WIDTH: f64 = 640.;
const CLASSIFICATION_MAX_WINDOWS: usize = 8;

// 固定 PP-LCNet textline_ori 输入为 NCHW 80×160；保持比例，右侧填128。
// 内容沿用现有双线性透视采样（不是 Mitchell），不能声称与 Mitchell 逐像素等价。
fn classification_input(rgb: &RgbImage, crop: &Crop) -> Tensor {
    const HEIGHT: usize = 80;
    const WIDTH: usize = 160;
    let natural = crop.width as f64 * HEIGHT as f64 / crop.height as f64;
    let count = ((natural / CLASSIFICATION_WINDOW_WIDTH).ceil() as usize)
        .clamp(1, CLASSIFICATION_MAX_WINDOWS);
    // 窗口数到上限后不再加宽窗口，改为在行内等距抽样，保持每窗的压缩比。
    let span = (1. / count as f64).min(CLASSIFICATION_WINDOW_WIDTH / natural);
    let stride = if count > 1 {
        (1. - span) / (count - 1) as f64
    } else {
        0.
    };
    let content_width = (natural * span).ceil().clamp(1., WIDTH as f64) as usize;
    let mut values = vec![128. / 127.5 - 1.; count * 3 * HEIGHT * WIDTH];
    for (window, values) in values.chunks_exact_mut(3 * HEIGHT * WIDTH).enumerate() {
        let start = window as f64 * stride;
        for y in 0..HEIGHT {
            for x in 0..content_width {
                let point = crop.point(
                    (start + (x as f64 + 0.5) / content_width as f64 * span) as f32,
                    (y as f32 + 0.5) / HEIGHT as f32,
                );
                for (channel, value) in normalized_bgr(rgb.sample(point)).into_iter().enumerate() {
                    values[channel * HEIGHT * WIDTH + y * WIDTH + x] = value;
                }
            }
        }
    }
    Tensor {
        shape: vec![count, 3, HEIGHT, WIDTH],
        values,
    }
}

fn classify_180(output: Tensor, windows: usize) -> Result<bool> {
    tensor_count(&output)?;
    if output.shape != [windows, 2]
        || output
            .values
            .iter()
            .any(|value| !(0. ..=1.).contains(value))
    {
        return Err("OCR 方向分类模型输出无效".into());
    }
    let mean =
        |class: usize| output.values[class..].iter().step_by(2).sum::<f32>() / windows as f32;
    let (upright, flipped) = (mean(0), mean(1));
    // 0°/180°逐行独立判断（行内各窗口取平均）；平票、低置信180°均保留原方向，
    // 不采用全页多数票。
    Ok(flipped > upright && flipped >= 0.9)
}

fn recognition_input(rgb: &RgbImage, crop: &Crop) -> Result<(Tensor, usize, usize)> {
    let (content_width, width) = recognition_width(crop.width, crop.height)?;
    let mut values = vec![0.; 3 * 48 * width];
    for y in 0..48 {
        for x in 0..content_width {
            let point = crop.point(
                (x as f32 + 0.5) / content_width as f32,
                (y as f32 + 0.5) / 48.,
            );
            let pixel = rgb.sample(point);
            for (channel, value) in normalized_bgr(pixel).into_iter().enumerate() {
                values[channel * 48 * width + y * width + x] = value;
            }
        }
    }
    Ok((
        Tensor {
            shape: vec![1, 3, 48, width],
            values,
        },
        content_width,
        width,
    ))
}

fn recognition_width(crop_width: usize, crop_height: usize) -> Result<(usize, usize)> {
    let content_width = (crop_width as f64 * 48. / crop_height as f64)
        .ceil()
        .max(1.) as usize;
    if content_width > 16384 {
        return Err("OCR 单行过长，请缩小识别范围".into());
    }
    let width = content_width.max(8).next_multiple_of(4);
    if width > 16384 {
        return Err("OCR 单行过长，请缩小识别范围".into());
    }
    Ok((content_width, width))
}

#[cfg(test)]
#[path = "rec_only_tests.rs"]
mod rec_only_tests;

#[cfg(test)]
#[path = "quality_retry_tests.rs"]
mod quality_retry_tests;

#[cfg(test)]
#[path = "cancellation_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
#[path = "duplicate_lines_pipeline_tests.rs"]
mod duplicate_lines_pipeline_tests;

#[cfg(test)]
#[path = "ink_bounds_pipeline_tests.rs"]
mod ink_bounds_pipeline_tests;

#[cfg(test)]
#[path = "pipeline_projected_tests.rs"]
mod projected_tests;

#[cfg(test)]
#[path = "grapheme_pipeline_tests.rs"]
mod grapheme_pipeline_tests;

#[cfg(test)]
#[path = "grapheme_ink_consumer_tests.rs"]
mod grapheme_ink_consumer_tests;

#[cfg(test)]
#[path = "pipeline_profile_tests.rs"]
mod profile_tests;

#[derive(Clone, Debug)]
struct Token {
    text: String,
    start: usize,
    end: usize,
    confidence: f32,
}
struct Decoded {
    tokens: Vec<Token>,
    steps: usize,
    confidence: f32,
    /// Trim后的实际发射区间，不含下方Voronoi补入的blank。
    emissions: Option<Vec<SamplingRun>>,
}

fn decode_ctc(output: Tensor, dictionary: &[String]) -> Result<Decoded> {
    tensor_count(&output)?;
    let (steps, classes) = match output.shape.as_slice() {
        [1, t, c] => (*t, *c),
        _ => return Err(format!("OCR 识别输出应为 [1,T,C]，实际 {:?}", output.shape).into()),
    };
    if classes != dictionary.len() + 2 {
        return Err(format!(
            "OCR 模型类别数 {classes} 与词表 {} 不匹配",
            dictionary.len()
        )
        .into());
    }
    let mut tokens: Vec<Token> = Vec::new();
    let mut previous = 0;
    for step in 0..steps {
        let scores = &output.values[step * classes..(step + 1) * classes];
        // First maximum wins, including a blank tie, as in the reference CTC decoder.
        let mut index = 0;
        let mut score = scores[0];
        for (candidate, &value) in scores.iter().enumerate().skip(1) {
            if value > score {
                index = candidate;
                score = value;
            }
        }
        let probability = if scores.iter().all(|value| (0. ..=1.).contains(value)) {
            score
        } else {
            1. / scores
                .iter()
                .map(|value| (*value - score).exp())
                .sum::<f32>()
        };
        if index != 0 && index != previous {
            let text = if index == classes - 1 {
                " ".to_string()
            } else {
                dictionary[index - 1].clone()
            };
            tokens.push(Token {
                text,
                start: step,
                end: step + 1,
                confidence: probability,
            });
        } else if index != 0 {
            if let Some(token) = tokens.last_mut() {
                token.end = step + 1;
                token.confidence = token.confidence.max(probability);
            }
        }
        previous = index;
    }
    while tokens
        .first()
        .is_some_and(|token| token.text.trim().is_empty())
    {
        tokens.remove(0);
    }
    while tokens
        .last()
        .is_some_and(|token| token.text.trim().is_empty())
    {
        tokens.pop();
    }
    let emissions = capture_emissions(&tokens, steps);
    // Each emitted symbol owns its CTC Voronoi interval, including adjacent blank steps.
    let centers: Vec<f32> = tokens
        .iter()
        .map(|t| (t.start + t.end) as f32 / 2.)
        .collect();
    let token_count = tokens.len();
    for (i, token) in tokens.iter_mut().enumerate() {
        token.start = if i == 0 {
            0
        } else {
            ((centers[i - 1] + centers[i]) / 2.).floor() as usize
        };
        token.end = if i + 1 == token_count {
            steps
        } else {
            ((centers[i] + centers[i + 1]) / 2.).ceil() as usize
        };
    }
    let confidence = if tokens.is_empty() {
        0.
    } else {
        tokens.iter().map(|token| token.confidence).sum::<f32>() / tokens.len() as f32
    };
    Ok(Decoded {
        tokens,
        steps,
        confidence,
        emissions,
    })
}

fn capture_emissions(tokens: &[Token], steps: usize) -> Option<Vec<SamplingRun>> {
    if steps == 0 || steps > MAX_SAMPLING_STEPS || tokens.len() > MAX_EMISSION_RUNS {
        return None;
    }
    // 先证明整行bytes预算，避免不适用数据仍建立新Vec／来源快照。
    let bytes = tokens.iter().try_fold(0usize, |bytes, token| {
        (!token.text.is_empty())
            .then(|| bytes.checked_add(token.text.len()))
            .flatten()
    })?;
    if bytes > MAX_LINE_BYTES {
        return None;
    }
    let mut offset = 0;
    let mut runs = Vec::with_capacity(tokens.len());
    for token in tokens {
        if token.start >= token.end || token.end > steps {
            return None;
        }
        let end = offset + token.text.len();
        runs.push(SamplingRun {
            source_range: offset..end,
            steps: token.start..token.end,
        });
        offset = end;
    }
    Some(runs)
}

struct Fragment {
    text: String,
    start: usize,
    end: usize,
}
fn fragments(tokens: &[Token]) -> Vec<Fragment> {
    let mut result: Vec<Fragment> = Vec::new();
    let mut join = false;
    for token in tokens {
        if token.text.chars().all(char::is_whitespace) {
            join = false;
            continue;
        }
        let latin = token
            .text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_'’-".contains(c));
        if latin && join {
            let last = result.last_mut().expect("joined Latin fragment exists");
            last.text.push_str(&token.text);
            last.end = token.end;
        } else {
            result.push(Fragment {
                text: token.text.clone(),
                start: token.start,
                end: token.end,
            });
        }
        join = latin;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixed {
        output: Option<Tensor>,
        inputs: Vec<Vec<usize>>,
    }
    impl ModelRunner for Fixed {
        fn run(&mut self, input: Tensor) -> Result<Tensor> {
            self.inputs.push(input.shape);
            self.output
                .take()
                .ok_or_else(|| "unexpected inference".into())
        }
    }
    fn image(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            bgra: vec![255; (width * height * 4) as usize],
            readback_ms: 0.,
        }
    }
    fn probability_map() -> Tensor {
        let mut values = vec![0.; 32 * 32];
        for y in 10..20 {
            for x in 5..26 {
                values[y * 32 + x] = 0.95;
            }
        }
        Tensor {
            shape: vec![1, 1, 32, 32],
            values,
        }
    }
    #[test]
    fn alpha_white_rgb_normalization_keeps_source_and_rejects_bad_storage() {
        let mut source = image(2, 1);
        source.bgra = vec![10, 20, 200, 255, 50, 70, 100, 0];
        let saved = source.bgra.clone();
        let rgb = RgbImage::from_bgra(&source).unwrap();
        assert_eq!(rgb.pixels[0], [200., 20., 10.]);
        assert_eq!(rgb.pixels[1], [255.; 3]);
        assert_eq!(source.bgra, saved);
        source.bgra.pop();
        assert!(RgbImage::from_bgra(&source).is_err());

        // Exercise both production tensor builders with unequal B/R values. Gray/white
        // pixels cannot distinguish RGB from the model's required BGR channel order.
        struct InspectDetectionInput;
        impl ModelRunner for InspectDetectionInput {
            fn run(&mut self, input: Tensor) -> Result<Tensor> {
                assert_eq!(input.shape, [1, 3, 32, 32]);
                let index = 16 * 32 + 16;
                let actual = [
                    input.values[index],
                    input.values[32 * 32 + index],
                    input.values[2 * 32 * 32 + index],
                ];
                for (actual, expected) in actual
                    .into_iter()
                    .zip([10., 20., 200.].map(|value| value / 127.5 - 1.))
                {
                    assert!((actual - expected).abs() < 1e-6);
                }
                Ok(Tensor {
                    shape: vec![1, 1, 32, 32],
                    values: vec![0.; 32 * 32],
                })
            }
        }
        let mut color = image(16, 16);
        for pixel in color.bgra.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[10, 20, 200, 255]);
        }
        let saved = color.bgra.clone();
        assert!(detect_with_config(
            &color,
            &mut InspectDetectionInput,
            DetectionConfig {
                max_side: 32,
                ..Default::default()
            }
        )
        .unwrap()
        .is_empty());
        let rgb = RgbImage::from_bgra(&color).unwrap();
        assert_eq!(rgb.pixels[0], [200., 20., 10.]);
        let crop = Crop::new([[0., 0.], [15., 0.], [15., 15.], [0., 15.]]).unwrap();
        let (input, content_width, tensor_width) = recognition_input(&rgb, &crop).unwrap();
        assert_eq!(input.shape, [1, 3, 48, 48]);
        assert_eq!((content_width, tensor_width), (48, 48));
        let index = 24 * 48 + 24;
        let actual = [
            input.values[index],
            input.values[48 * 48 + index],
            input.values[2 * 48 * 48 + index],
        ];
        for (actual, expected) in actual
            .into_iter()
            .zip([10., 20., 200.].map(|value| value / 127.5 - 1.))
        {
            assert!((actual - expected).abs() < 1e-6);
        }
        assert_eq!(color.bgra, saved);
    }
    #[test]
    fn detector_traces_scores_unclips_and_maps_original_coordinates_without_rec() {
        let source = image(320, 160);
        let mut runner = Fixed {
            output: Some(probability_map()),
            inputs: Vec::new(),
        };
        let detected = detect_with_config(
            &source,
            &mut runner,
            DetectionConfig {
                max_side: 320,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(runner.inputs, [vec![1, 3, 160, 320]]);
        assert_eq!(detected.len(), 1);
        assert!(detected[0].rect.valid(320, 160));
        assert!(detected[0].confidence > 0.6);
        assert!(detected[0].quad[0][0] < 60. && detected[0].quad[2][0] > 250.);
        let mut invalid = Fixed {
            output: Some(Tensor {
                shape: vec![1, 2, 32, 32],
                values: vec![0.; 2048],
            }),
            inputs: Vec::new(),
        };
        assert!(detect(&source, &mut invalid).is_err());
    }
    #[test]
    fn contour_walk_bounds_sparse_lines_diagonals_edges_and_separate_components() {
        let width = 24;
        let height = 16;
        let mut mask = vec![false; width * height];
        mask[2 * width + 2] = true;
        for x in 5..10 {
            mask[2 * width + x] = true;
        }
        for (x, y) in [(13, 2), (14, 3), (15, 4)] {
            mask[y * width + x] = true;
        }
        for y in 8..13 {
            for x in 0..5 {
                mask[y * width + x] = true;
            }
        }
        for y in 9..height {
            for x in 19..width {
                mask[y * width + x] = true;
            }
        }
        let traced = contours(&mask, width, height, 16);
        let rectangles: Vec<_> = traced
            .iter()
            .filter_map(|points| minimum_rectangle(points))
            .collect();
        assert_eq!(rectangles.len(), 2);
        assert!(rectangles.iter().all(valid_quad));
        assert!(rectangles
            .iter()
            .all(|quad| distance(quad[0], quad[1]).min(distance(quad[0], quad[3])) >= 4.));
        assert_eq!(contours(&mask, width, height, 1).len(), 1);
        assert!(contours(&[false; 9], 3, 3, 16).is_empty());
        let mut point = [false; 9];
        point[0] = true;
        assert!(contours(&point, 3, 3, 16).is_empty());
    }
    #[test]
    fn rotated_minimum_box_and_perspective_crop_preserve_real_corners() {
        let points = [[20., 10.], [60., 30.], [50., 50.], [10., 30.]];
        let rect = minimum_rectangle(&points).unwrap();
        let crop = Crop::new(rect).unwrap();
        for (point, uv) in rect
            .into_iter()
            .zip([[0., 0.], [1., 0.], [1., 1.], [0., 1.]])
        {
            let actual = crop.point(uv[0], uv[1]);
            assert!(distance(point, actual) < 1e-3);
        }
        let trapezoid = [[10., 10.], [90., 20.], [75., 60.], [15., 50.]];
        let crop = Crop::new(trapezoid).unwrap();
        for (point, uv) in trapezoid
            .into_iter()
            .zip([[0., 0.], [1., 0.], [1., 1.], [0., 1.]])
        {
            assert!(distance(point, crop.point(uv[0], uv[1])) < 1e-3);
        }
        assert!(Crop::new([[0., 0.], [1., 0.], [1., 0.], [0., 1.]]).is_err());
    }
    #[test]
    fn ctc_blank_repeat_unicode_space_and_dictionary_pairing_are_not_interchanged() {
        let dictionary = vec!["中".into(), "文".into(), "A".into()];
        let sequence = [0, 1, 1, 0, 1, 4, 3, 3, 0, 2];
        let mut values = vec![0.; sequence.len() * 5];
        for (step, index) in sequence.into_iter().enumerate() {
            values[step * 5 + index] = 0.99;
        }
        let decoded = decode_ctc(
            Tensor {
                shape: vec![1, 10, 5],
                values,
            },
            &dictionary,
        )
        .unwrap();
        assert_eq!(
            decoded
                .tokens
                .iter()
                .map(|t| t.text.as_str())
                .collect::<String>(),
            "中中 A文"
        );
        assert_eq!(
            fragments(&decoded.tokens)
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>(),
            ["中", "中", "A", "文"]
        );
        assert!(decode_ctc(
            Tensor {
                shape: vec![1, 1, 4],
                values: vec![0.; 4]
            },
            &dictionary
        )
        .is_err());
        assert!(decode_ctc(
            Tensor {
                shape: vec![1, 1, 5],
                values: vec![0.; 5]
            },
            &dictionary
        )
        .unwrap()
        .tokens
        .is_empty());
    }
    #[test]
    fn classifier_preserves_aspect_bgr_and_midgray_padding_without_changing_source() {
        let mut source = image(20, 20);
        for pixel in source.bgra.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[10, 20, 200, 255]);
        }
        let saved = source.bgra.clone();
        let rgb = RgbImage::from_bgra(&source).unwrap();
        let crop = Crop::new([[0., 0.], [19., 0.], [19., 19.], [0., 19.]]).unwrap();
        let input = classification_input(&rgb, &crop);
        assert_eq!(input.shape, [1, 3, 80, 160]);
        for (channel, expected) in [10., 20., 200.].into_iter().enumerate() {
            let offset = channel * 80 * 160;
            assert!((input.values[offset + 40 * 160 + 79] - (expected / 127.5 - 1.)).abs() < 1e-6);
            assert!(input.values[offset + 40 * 160 + 80..offset + 41 * 160]
                .iter()
                .all(|value| (*value - (128. / 127.5 - 1.)).abs() < 1e-6));
        }
        let wide = Crop::new([[0., 0.], [19., 0.], [19., 4.], [0., 4.]]).unwrap();
        let wide_input = classification_input(&rgb, &wide);
        assert!((wide_input.values[159] - (10. / 127.5 - 1.)).abs() < 1e-6);
        assert_eq!(source.bgra, saved);
    }
    #[test]
    fn classifier_requires_confident_180_and_rejects_invalid_model_outputs() {
        let decision = |values: Vec<f32>| {
            classify_180(
                Tensor {
                    shape: vec![1, 2],
                    values,
                },
                1,
            )
        };
        assert!(decision(vec![0.1, 0.9]).unwrap());
        assert!(!decision(vec![0.1001, 0.8999]).unwrap());
        assert!(!decision(vec![0.99, 0.01]).unwrap());
        assert!(!decision(vec![0.95, 0.95]).unwrap());
        assert!(decision(vec![f32::NAN, 0.99]).is_err());
        assert!(decision(vec![0., f32::INFINITY]).is_err());
        assert!(decision(vec![-0.1, 1.1]).is_err());
        assert!(decision(vec![1.]).is_err());
        assert!(classify_180(
            Tensor {
                shape: vec![2, 1],
                values: vec![0., 1.],
            },
            1,
        )
        .is_err());
    }
    #[test]
    fn long_line_classifier_averages_windows_and_requires_matching_batch() {
        let windows = |values: Vec<f32>| {
            classify_180(
                Tensor {
                    shape: vec![values.len() / 2, 2],
                    values,
                },
                3,
            )
        };
        assert!(windows(vec![0.02, 0.98, 0.05, 0.95, 0.1, 0.9]).unwrap());
        // 单个窗口的强 180° 不能翻转整行：平均后仍未达到置信门槛。
        assert!(!windows(vec![0.0, 1.0, 0.6, 0.4, 0.2, 0.8]).unwrap());
        assert!(!windows(vec![0.99, 0.01, 0.98, 0.02, 0.0, 1.0]).unwrap());
        // 批量数与窗口数不一致属于模型输出无效，不能静默按单窗判断。
        assert!(windows(vec![0.0, 1.0]).is_err());
        assert!(windows([0.0, 1.0].repeat(4)).is_err());
    }
    #[test]
    fn long_line_classifier_samples_bounded_windows_across_the_whole_line() {
        let mut source = image(4000, 20);
        for (index, pixel) in source.bgra.chunks_exact_mut(4).enumerate() {
            let x = (index % 4000 / 16) as u8;
            pixel.copy_from_slice(&[x, x, x, 255]);
        }
        let rgb = RgbImage::from_bgra(&source).unwrap();
        let pixel = |input: &Tensor, window: usize, x: usize| {
            input.values[window * 3 * 80 * 160 + 40 * 160 + x]
        };
        // 自然宽度 = 宽 × 80 / 高；短行仍是单窗，与原输入一致。
        let short = Crop::new([[0., 0.], [120., 0.], [120., 19.], [0., 19.]]).unwrap();
        assert_eq!(classification_input(&rgb, &short).shape, [1, 3, 80, 160]);
        // 1599×19 → 自然宽约 6733 → 达到 8 窗上限，按每窗 640 的自然宽等距抽样。
        let long = Crop::new([[0., 0.], [1599., 0.], [1599., 19.], [0., 19.]]).unwrap();
        let input = classification_input(&rgb, &long);
        assert_eq!(input.shape, [8, 3, 80, 160]);
        let span = 640. / (1599. * 80. / 19.) * 1599.;
        for window in 0..8 {
            let start = window as f32 * (1599. - span) / 7.;
            for (x, u) in [(0, 0.5 / 160.), (159, 159.5 / 160.)] {
                let expected = ((start + u * span) / 16.).floor() / 127.5 - 1.;
                assert!((pixel(&input, window, x) - expected).abs() < 1.5 / 127.5);
            }
        }
        // 3999×20 的超长行仍保持 8 窗、每窗不超过 4 倍横向压缩。
        let longest = Crop::new([[0., 0.], [3999., 0.], [3999., 19.], [0., 19.]]).unwrap();
        let input = classification_input(&rgb, &longest);
        assert_eq!(input.shape, [8, 3, 80, 160]);
        assert!((pixel(&input, 0, 0) - (0. / 127.5 - 1.)).abs() < 1e-6);
        assert!((pixel(&input, 7, 159) - (249. / 127.5 - 1.)).abs() < 1e-6);
        // 中等长度按 640 自然宽向上取整切窗，窗间不留空隙。
        let medium = Crop::new([[0., 0.], [399., 0.], [399., 19.], [0., 19.]]).unwrap();
        assert_eq!(classification_input(&rgb, &medium).shape, [3, 3, 80, 160]);
    }
    #[test]
    fn applied_180_inverts_sampling_and_geometry_after_perspective_or_vertical_crop() {
        let quads = [
            [[10., 10.], [90., 20.], [75., 60.], [15., 50.]],
            [[10., 10.], [25., 10.], [25., 90.], [10., 90.]],
        ];
        for quad in quads {
            let mut crop = Crop::new(quad).unwrap();
            let points = [[0., 0.], [1., 0.], [1., 1.], [0., 1.], [0.2, 0.7]];
            let expected = points.map(|[u, v]| crop.point(1. - u, 1. - v));
            crop.rotated_180 = true;
            for ([u, v], expected) in points.into_iter().zip(expected) {
                assert!(distance(crop.point(u, v), expected) < 1e-3);
            }
        }
        let mut source = image(100, 30);
        for (index, pixel) in source.bgra.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[(index % 100) as u8, (index / 100) as u8, 200, 255]);
        }
        let rgb = RgbImage::from_bgra(&source).unwrap();
        let mut crop = Crop::new([[0., 0.], [99., 0.], [99., 29.], [0., 29.]]).unwrap();
        let (upright, content_width, width) = recognition_input(&rgb, &crop).unwrap();
        crop.rotated_180 = true;
        let (flipped, flipped_content, flipped_width) = recognition_input(&rgb, &crop).unwrap();
        assert_eq!((flipped_content, flipped_width), (content_width, width));
        for channel in 0..3 {
            for y in 0..48 {
                for x in 0..content_width {
                    let expected = upright.values
                        [channel * 48 * width + (47 - y) * width + content_width - 1 - x];
                    assert!(
                        (flipped.values[channel * 48 * width + y * width + x] - expected).abs()
                            < 1e-5
                    );
                }
            }
        }
    }
    #[test]
    fn crop_horizontal_direction_tracks_physical_x_after_perspective_and_vertical_normalization() {
        for (quad, upright, flipped) in [
            (
                [[10., 10.], [90., 20.], [75., 60.], [15., 50.]],
                HorizontalDirection::LeftToRight,
                HorizontalDirection::RightToLeft,
            ),
            (
                [[10., 10.], [25., 10.], [25., 90.], [10., 90.]],
                HorizontalDirection::LeftToRight,
                HorizontalDirection::LeftToRight,
            ),
            (
                [[10., 10.], [25., 15.], [20., 95.], [5., 90.]],
                HorizontalDirection::RightToLeft,
                HorizontalDirection::LeftToRight,
            ),
        ] {
            let mut crop = Crop::new(quad).unwrap();
            assert_eq!(crop.horizontal_direction(), upright);
            crop.rotated_180 = true;
            assert_eq!(crop.horizontal_direction(), flipped);
        }
        // 90° 归一化后的直立轴只沿 y 改变；这里仅验证水平投影合同，
        // 不把零 x 分量扩为垂直选字或精确透视字符几何已验。
        let mut vertical = Crop::new([[10., 10.], [25., 10.], [25., 90.], [10., 90.]]).unwrap();
        assert_eq!((vertical.width, vertical.height), (80, 15));
        for (rotated, start_y, end_y) in [(false, 10., 90.), (true, 90., 10.)] {
            vertical.rotated_180 = rotated;
            assert!(distance(vertical.point(0., 0.5), [17.5, start_y]) < 1e-5);
            assert!(distance(vertical.point(1., 0.5), [17.5, end_y]) < 1e-5);
            assert_eq!(
                vertical.horizontal_direction(),
                HorizontalDirection::LeftToRight
            );
        }
    }
    #[test]
    fn mixed_line_directions_preserve_visual_order_and_map_each_word_to_original_image() {
        struct Sequence(VecDeque<Tensor>);
        impl ModelRunner for Sequence {
            fn run(&mut self, _: Tensor) -> Result<Tensor> {
                self.0
                    .pop_front()
                    .ok_or_else(|| "unexpected inference".into())
            }
        }
        let source = image(320, 320);
        let mut values = vec![0.; 32 * 32];
        for top in [3, 20] {
            for y in top..top + 5 {
                for x in 5..26 {
                    values[y * 32 + x] = 0.99;
                }
            }
        }
        let mut det = Fixed {
            output: Some(Tensor {
                shape: vec![1, 1, 32, 32],
                values,
            }),
            inputs: Vec::new(),
        };
        let mut cls = Sequence(VecDeque::from([
            Tensor {
                shape: vec![1, 2],
                values: vec![0.99, 0.01],
            },
            Tensor {
                shape: vec![1, 2],
                values: vec![0.01, 0.99],
            },
        ]));
        let rec_output = || Tensor {
            shape: vec![1, 5, 4],
            values: vec![
                1., 0., 0., 0., 0., 1., 0., 0., 1., 0., 0., 0., 0., 0., 1., 0., 1., 0., 0., 0.,
            ],
        };
        let mut rec = Sequence(VecDeque::from([rec_output(), rec_output()]));
        let result = recognize(
            &source,
            &mut det,
            &mut cls,
            &mut rec,
            &["中".into(), "文".into()],
        )
        .unwrap();
        assert_eq!(result.text, "中文\n中文");
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.words.len(), 4);
        assert_eq!(
            result
                .lines
                .iter()
                .map(|line| line.horizontal_direction)
                .collect::<Vec<_>>(),
            [
                HorizontalDirection::LeftToRight,
                HorizontalDirection::RightToLeft
            ]
        );
        assert_eq!(
            result
                .words
                .iter()
                .map(|word| word.horizontal_direction)
                .collect::<Vec<_>>(),
            [
                HorizontalDirection::LeftToRight,
                HorizontalDirection::LeftToRight,
                HorizontalDirection::RightToLeft,
                HorizontalDirection::RightToLeft
            ]
        );
        assert!(result.words.iter().all(|word| word.rect.valid(320, 320)));
        assert!(result.words[0].rect.y < result.words[2].rect.y);
        assert!(result.words[0].rect.x < result.words[1].rect.x);
        assert!(result.words[2].rect.x > result.words[3].rect.x);
        assert_eq!(
            result
                .words
                .iter()
                .map(|word| word.line)
                .collect::<Vec<_>>(),
            [0, 0, 1, 1]
        );
        assert!(cls.0.is_empty() && rec.0.is_empty());
    }
    #[test]
    fn detector_recognizer_result_has_visual_lines_and_inverse_ctc_fragments() {
        let source = image(320, 160);
        let mut det = Fixed {
            output: Some(probability_map()),
            inputs: Vec::new(),
        };
        let mut rec = Fixed {
            output: Some(Tensor {
                shape: vec![1, 5, 4],
                values: vec![
                    1., 0., 0., 0., 0., 1., 0., 0., 1., 0., 0., 0., 0., 0., 1., 0., 1., 0., 0., 0.,
                ],
            }),
            inputs: Vec::new(),
        };
        let mut cls = Fixed {
            output: Some(Tensor {
                shape: vec![1, 2],
                values: vec![0.99, 0.01],
            }),
            inputs: Vec::new(),
        };
        let result = recognize(
            &source,
            &mut det,
            &mut cls,
            &mut rec,
            &["中".into(), "文".into()],
        )
        .unwrap();
        assert_eq!(result.text, "中文");
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.words.len(), 2);
        assert_eq!(result.words[0].text, "中");
        assert_eq!(result.words[1].text, "文");
        assert!(result
            .words
            .iter()
            .all(|word| word.line == 0 && word.rect.valid(320, 160)));
        assert!(result.words[0].rect.x < result.words[1].rect.x);
        assert_eq!(rec.inputs[0][..3], [1, 3, 48]);
        assert_eq!(cls.inputs, [vec![1, 3, 80, 160]]);
        assert_eq!(source.bgra, vec![255; 320 * 160 * 4]);
    }
}
