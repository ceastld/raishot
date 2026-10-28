//! 当前图像的一维条码扫描；位置只表示实际扫描线，不表示完整包围框。
use crate::{
    capture::Image,
    image_tools::{allocate_pixels, validate_input, ToolError, ToolLimits, ToolResult},
};
use rxing::{
    common::{BitArray, GlobalHistogramBinarizer},
    oned::{MultiFormatOneDReader, OneDReader},
    Binarizer, DecodeHints, Luma8Source, LuminanceSource,
};
use std::{
    borrow::Cow,
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const MAX_SCRATCH_BYTES: u64 = 128 * 1024 * 1024;
const MIN_SEGMENT_PIXELS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BarcodeFormat {
    Code128,
    Code39,
    Ean13,
    Ean8,
    Upca,
    Upce,
    Itf,
}

impl BarcodeFormat {
    pub(crate) const ALL: [Self; 7] = [
        Self::Code128,
        Self::Code39,
        Self::Ean13,
        Self::Ean8,
        Self::Upca,
        Self::Upce,
        Self::Itf,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Code128 => "Code 128",
            Self::Code39 => "Code 39",
            Self::Ean13 => "EAN-13",
            Self::Ean8 => "EAN-8",
            Self::Upca => "UPC-A",
            Self::Upce => "UPC-E",
            Self::Itf => "ITF",
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Self::Code128 => 0,
            Self::Code39 => 1,
            Self::Ean13 => 2,
            Self::Ean8 => 3,
            Self::Upca => 4,
            Self::Upce => 5,
            Self::Itf => 6,
        }
    }

    fn engine(self) -> rxing::BarcodeFormat {
        match self {
            Self::Code128 => rxing::BarcodeFormat::CODE_128,
            Self::Code39 => rxing::BarcodeFormat::CODE_39,
            Self::Ean13 => rxing::BarcodeFormat::EAN_13,
            Self::Ean8 => rxing::BarcodeFormat::EAN_8,
            Self::Upca => rxing::BarcodeFormat::UPC_A,
            Self::Upce => rxing::BarcodeFormat::UPC_E,
            Self::Itf => rxing::BarcodeFormat::ITF,
        }
    }

    fn from_engine(format: &rxing::BarcodeFormat) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|value| value.engine() == *format)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScanOptions {
    pub formats: [bool; 7],
    pub try_inverted: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            formats: [true; 7],
            try_inverted: false,
        }
    }
}

impl ScanOptions {
    pub(crate) fn validate(&self) -> ToolResult<()> {
        if !self.formats.iter().any(|enabled| *enabled) {
            return Err(ToolError::Invalid(
                tr!(
                    "请至少选择一种条码格式",
                    "Select at least one barcode format"
                )
                .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanLimits {
    pub image: ToolLimits,
    pub max_decode_attempts: usize,
    pub max_results: usize,
    pub max_text_bytes: usize,
    pub max_total_text_bytes: usize,
    /// 灰度生成及每次横/纵二值行访问都计入；完整反色扫描最多为输入像素的五倍。
    pub max_sampled_pixels: u64,
    pub max_duration: Duration,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            image: ToolLimits::default(),
            max_decode_attempts: 16_384,
            max_results: 128,
            max_text_bytes: 65_536,
            max_total_text_bytes: 1_048_576,
            max_sampled_pixels: 160_000_000,
            max_duration: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BarcodeHit {
    pub format: BarcodeFormat,
    pub text: String,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanStatus {
    Complete,
    Cancelled,
    Limited,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScanProgress {
    pub sampled_pixels: u64,
    pub decode_attempts: usize,
    pub results: usize,
}

#[derive(Debug)]
pub(crate) struct ScanReport {
    pub hits: Vec<BarcodeHit>,
    pub status: ScanStatus,
    pub detail: String,
    pub progress: ScanProgress,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

struct Candidate {
    format: BarcodeFormat,
    text: String,
    lo: f64,
    hi: f64,
}

struct Band {
    axis: Axis,
    lo: f64,
    hi: f64,
    first: u32,
    last: u32,
}

struct Worker<'a, F> {
    limits: ScanLimits,
    cancel: &'a AtomicBool,
    notify: F,
    started: Instant,
    hits: Vec<BarcodeHit>,
    bands: Vec<Band>,
    text_bytes: usize,
    invalid_points: usize,
    progress: ScanProgress,
    last_notified_pixels: u64,
    stopped: Option<(ScanStatus, &'static str)>,
}

impl<F: FnMut(ScanProgress)> Worker<'_, F> {
    fn running(&mut self) -> bool {
        if self.stopped.is_none() {
            if self.cancel.load(Ordering::Acquire) {
                self.stopped = Some((
                    ScanStatus::Cancelled,
                    tr!(
                        "已取消，保留已识别结果",
                        "Cancelled; recognized results are kept"
                    ),
                ));
            } else if self.started.elapsed() >= self.limits.max_duration {
                self.limit(tr!(
                    "达到扫描时间预算，保留已识别结果",
                    "Scan time limit reached; recognized results are kept"
                ));
            }
        }
        self.stopped.is_none()
    }

    fn limit(&mut self, detail: &'static str) {
        if self.stopped.is_none() {
            self.stopped = Some((ScanStatus::Limited, detail));
        }
    }

    fn emit(&mut self) {
        self.last_notified_pixels = self.progress.sampled_pixels;
        (self.notify)(self.progress);
    }

    fn sample(&mut self, pixels: u64) -> bool {
        if !self.running() {
            return false;
        }
        let Some(total) = self
            .progress
            .sampled_pixels
            .checked_add(pixels)
            .filter(|total| *total <= self.limits.max_sampled_pixels)
        else {
            self.limit(tr!(
                "达到像素采样预算，保留已识别结果",
                "Pixel sampling limit reached; recognized results are kept"
            ));
            return false;
        };
        self.progress.sampled_pixels = total;
        if total - self.last_notified_pixels >= 262_144 {
            self.emit();
        }
        self.running()
    }

    fn attempt(&mut self) -> bool {
        if !self.running() {
            return false;
        }
        if self.progress.decode_attempts >= self.limits.max_decode_attempts {
            self.limit(tr!(
                "达到解码次数预算，保留已识别结果",
                "Decode attempt limit reached; recognized results are kept"
            ));
            return false;
        }
        self.progress.decode_attempts += 1;
        true
    }

    fn text_allowed(&mut self, bytes: usize) -> bool {
        if bytes > self.limits.max_text_bytes {
            self.limit(tr!(
                "条码文字超过单项预算，保留已识别结果",
                "Barcode text exceeds the per-result limit; recognized results are kept"
            ));
            return false;
        }
        true
    }

    fn record(
        &mut self,
        candidate: &Candidate,
        axis: Axis,
        line: u32,
        previous: &[usize],
        current: &mut Vec<usize>,
    ) -> ToolResult<()> {
        if !self.running() {
            return Ok(());
        }
        let matches = |index: usize| {
            let band = &self.bands[index];
            let hit = &self.hits[index];
            band.axis == axis
                && hit.format == candidate.format
                && hit.text == candidate.text
                && (band.lo - candidate.lo).abs() < 0.001
                && (band.hi - candidate.hi).abs() < 0.001
        };
        // 只合并相邻且连续命中的扫描线。空白行会清掉 previous，因此不会跨白隙合并两码。
        let existing = previous
            .iter()
            .copied()
            .find(|&index| matches(index) && self.bands[index].last.checked_add(1) == Some(line))
            .or_else(|| {
                (0..self.hits.len()).find(|&index| {
                    matches(index)
                        && (self.bands[index].first..=self.bands[index].last).contains(&line)
                })
            });
        let index = if let Some(index) = existing {
            let band = &mut self.bands[index];
            band.first = band.first.min(line);
            band.last = band.last.max(line);
            // 选取一个确实扫描过的整数行作为代表线；不把 band 外沿当成物体包围框。
            let middle = f64::from(band.first + (band.last - band.first) / 2);
            let hit = &mut self.hits[index];
            match axis {
                Axis::Horizontal => {
                    hit.y1 = middle;
                    hit.y2 = middle;
                }
                Axis::Vertical => {
                    hit.x1 = middle;
                    hit.x2 = middle;
                }
            }
            index
        } else {
            if self.hits.len() >= self.limits.max_results {
                self.limit(tr!(
                    "达到条码结果数量预算，保留已识别结果",
                    "Barcode result limit reached; recognized results are kept"
                ));
                return Ok(());
            }
            let Some(total) = self
                .text_bytes
                .checked_add(candidate.text.len())
                .filter(|total| *total <= self.limits.max_total_text_bytes)
            else {
                self.limit(tr!(
                    "达到条码文字总预算，保留已识别结果",
                    "Total barcode text limit reached; recognized results are kept"
                ));
                return Ok(());
            };
            let text = copy_text(&candidate.text)?;
            let position = f64::from(line);
            let (x1, y1, x2, y2) = match axis {
                Axis::Horizontal => (candidate.lo, position, candidate.hi, position),
                Axis::Vertical => (position, candidate.lo, position, candidate.hi),
            };
            let index = self.hits.len();
            self.hits.push(BarcodeHit {
                format: candidate.format,
                text,
                x1,
                y1,
                x2,
                y2,
            });
            self.bands.push(Band {
                axis,
                lo: candidate.lo,
                hi: candidate.hi,
                first: line,
                last: line,
            });
            self.text_bytes = total;
            self.progress.results = self.hits.len();
            self.emit();
            index
        };
        if !current.contains(&index) {
            current.push(index);
        }
        Ok(())
    }

    fn finish(mut self) -> ScanReport {
        self.running();
        self.emit();
        self.running();
        let (status, detail) = self.stopped.unwrap_or((
            ScanStatus::Complete,
            if self.hits.is_empty() {
                tr!(
                    "未发现所选格式的条码",
                    "No barcodes found in the selected formats"
                )
            } else {
                tr!(
                    "扫描完成；定位线不表示完整包围框",
                    "Scan complete; location lines are not bounding boxes"
                )
            },
        ));
        let detail = if self.invalid_points == 0 {
            detail.into()
        } else {
            trf!(
                "{detail}；跳过 {} 个无效定位候选",
                "{detail}; skipped {} invalid location candidates",
                self.invalid_points
            )
        };
        ScanReport {
            hits: self.hits,
            status,
            detail,
            progress: self.progress,
        }
    }
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> ToolResult<()> {
    values.try_reserve_exact(additional).map_err(|_| {
        ToolError::Failed(
            tr!(
                "没有足够内存识别条码",
                "Not enough memory to recognize barcodes"
            )
            .into(),
        )
    })
}

fn copy_text(text: &str) -> ToolResult<String> {
    let mut result = String::new();
    result.try_reserve_exact(text.len()).map_err(|_| {
        ToolError::Failed(
            tr!(
                "没有足够内存保存条码文字",
                "Not enough memory for barcode text"
            )
            .into(),
        )
    })?;
    result.push_str(text);
    Ok(result)
}

fn check_scratch(image: &Image, pixels: u64, limits: ScanLimits) -> ToolResult<()> {
    // 一张 luma；横纵所有二值缓存及按机器字对齐的余量；每行/列保守预留
    // 128B 缓存元数据；行工作区、reader、候选和输出文字、分段队列另行计入。
    let dimensions = u64::from(image.width) + u64::from(image.height);
    let bytes = pixels
        .checked_add(pixels.div_ceil(4))
        .and_then(|n| n.checked_add(dimensions.checked_mul(136)?))
        .and_then(|n| n.checked_add(u64::from(image.width.max(image.height)).checked_mul(64)?))
        .and_then(|n| {
            n.checked_add(
                u64::try_from(limits.max_total_text_bytes)
                    .ok()?
                    .checked_mul(2)?,
            )
        })
        .and_then(|n| n.checked_add(u64::try_from(limits.max_results).ok()?.checked_mul(512)?))
        .and_then(|n| n.checked_add(1_048_576))
        .filter(|bytes| *bytes <= MAX_SCRATCH_BYTES);
    if bytes.is_none() {
        return Err(ToolError::Invalid(
            tr!(
                "条码扫描超过临时内存预算（128 MiB）",
                "Barcode scan exceeds the 128 MiB scratch limit"
            )
            .into(),
        ));
    }
    Ok(())
}

/// 只读源图。取消、超时或预算用尽返回部分结果；参数、像素存储及分配错误返回 Err。
/// 库内单次 decode_row 不能取消，所有行、分段及调库前后都检查任务状态。
pub(crate) fn scan(
    image: &Image,
    options: &ScanOptions,
    limits: ScanLimits,
    cancel: &AtomicBool,
    notify: impl FnMut(ScanProgress),
) -> ToolResult<ScanReport> {
    options.validate()?;
    let pixels = validate_input(image, limits.image)?;
    check_scratch(image, pixels, limits)?;
    let mut worker = Worker {
        limits,
        cancel,
        notify,
        started: Instant::now(),
        hits: Vec::new(),
        bands: Vec::new(),
        text_bytes: 0,
        invalid_points: 0,
        progress: ScanProgress::default(),
        last_notified_pixels: 0,
        stopped: None,
    };
    if !worker.running() {
        return Ok(worker.finish());
    }
    if limits.max_decode_attempts == 0 || limits.max_results == 0 || limits.max_sampled_pixels == 0
    {
        worker.limit(tr!("扫描预算为空", "The scan budget is zero"));
        return Ok(worker.finish());
    }
    reserve(&mut worker.hits, limits.max_results)?;
    reserve(&mut worker.bands, limits.max_results)?;
    let mut gray = allocate_pixels(usize::try_from(pixels).map_err(|_| {
        ToolError::Invalid(tr!("图片尺寸溢出", "Image dimensions overflow").into())
    })?)?;
    for (source, target) in image
        .bgra
        .chunks_exact(image.width as usize * 4)
        .zip(gray.chunks_exact_mut(image.width as usize))
    {
        if !worker.sample(u64::from(image.width)) {
            return Ok(worker.finish());
        }
        for (bgra, luma) in source.chunks_exact(4).zip(target) {
            let value = (u32::from(bgra[2]) * 299
                + u32::from(bgra[1]) * 587
                + u32::from(bgra[0]) * 114
                + 500)
                / 1_000;
            let alpha = u32::from(bgra[3]);
            *luma = ((value * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
    }
    let hints = DecodeHints {
        PossibleFormats: Some(
            BarcodeFormat::ALL
                .into_iter()
                .filter(|format| options.formats[format.index()])
                .map(BarcodeFormat::engine)
                .collect::<HashSet<_>>(),
        ),
        ..DecodeHints::default()
    };
    let mut reader = MultiFormatOneDReader::new(&hints);
    for inverted in [false, true]
        .into_iter()
        .take(if options.try_inverted { 2 } else { 1 })
    {
        if !worker.running() {
            break;
        }
        let mut source = Luma8Source::new_with_slice(&gray, image.width, image.height)
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        if inverted {
            source.invert();
        }
        // 不请求矩阵；库分别缓存行和列，避免整图旋转和第二张灰度底图。
        let binarizer = GlobalHistogramBinarizer::new(source);
        for axis in [Axis::Horizontal, Axis::Vertical] {
            scan_axis(&mut worker, &binarizer, &mut reader, &hints, image, axis)?;
            if !worker.running() {
                break;
            }
        }
    }
    Ok(worker.finish())
}

fn scan_axis<F: FnMut(ScanProgress), B: Binarizer>(
    worker: &mut Worker<'_, F>,
    binarizer: &B,
    reader: &mut MultiFormatOneDReader,
    hints: &DecodeHints,
    image: &Image,
    axis: Axis,
) -> ToolResult<()> {
    let (line_count, line_length) = match axis {
        Axis::Horizontal => (image.height, image.width as usize),
        Axis::Vertical => (image.width, image.height as usize),
    };
    let mut previous_bits: Option<Cow<'_, BitArray>> = None;
    let mut candidates = Vec::new();
    let mut previous_bands = Vec::new();
    let mut current_bands = Vec::new();
    reserve(&mut candidates, worker.limits.max_results)?;
    reserve(&mut previous_bands, worker.limits.max_results)?;
    reserve(&mut current_bands, worker.limits.max_results)?;
    for line in 0..line_count {
        if !worker.sample(line_length as u64) {
            break;
        }
        current_bands.clear();
        let row = match axis {
            Axis::Horizontal => binarizer.get_black_row(line as usize),
            Axis::Vertical => {
                binarizer.get_black_line(line as usize, rxing::common::LineOrientation::Column)
            }
        };
        let Ok(row) = row else {
            previous_bits = None;
            previous_bands.clear();
            candidates.clear();
            continue;
        };
        if previous_bits.as_deref() == Some(row.as_ref()) {
            for candidate in &candidates {
                worker.record(candidate, axis, line, &previous_bands, &mut current_bands)?;
                if !worker.running() {
                    break;
                }
            }
        } else {
            candidates.clear();
            decode_line(
                worker,
                reader,
                hints,
                row.as_ref(),
                axis,
                line,
                line_length,
                &previous_bands,
                &mut current_bands,
                &mut candidates,
            )?;
        }
        previous_bits = Some(row);
        std::mem::swap(&mut previous_bands, &mut current_bands);
        if line % 64 == 0 {
            worker.emit();
        }
        if !worker.running() {
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_line<F: FnMut(ScanProgress)>(
    worker: &mut Worker<'_, F>,
    reader: &mut MultiFormatOneDReader,
    hints: &DecodeHints,
    row: &BitArray,
    axis: Axis,
    line: u32,
    length: usize,
    previous_bands: &[usize],
    current_bands: &mut Vec<usize>,
    candidates: &mut Vec<Candidate>,
) -> ToolResult<()> {
    if row.getNextSet(0) >= length || row.getNextUnset(0) >= length {
        return Ok(());
    }
    let mut segments = Vec::new();
    reserve(
        &mut segments,
        worker
            .limits
            .max_results
            .saturating_mul(2)
            .saturating_add(1),
    )?;
    segments.push((0, length));
    let mut cached_text = 0usize;
    while let Some((start, end)) = segments.pop() {
        if !worker.running() {
            break;
        }
        if end - start < MIN_SEGMENT_PIXELS {
            continue;
        }
        let mut bits = BitArray::with_size(end - start);
        for x in start..end {
            if row.get(x) {
                bits.set(x - start);
            }
        }
        let mut found = None;
        for reversed in [false, true] {
            if reversed {
                bits.reverse();
            }
            if !worker.attempt() {
                break;
            }
            let result = reader.decode_row(line, &bits, hints);
            if !worker.running() {
                break;
            }
            let Ok(result) = result else {
                continue;
            };
            let Some(format) = BarcodeFormat::from_engine(result.getBarcodeFormat()) else {
                continue;
            };
            let Some((lo, hi)) = result_span(
                result.getPoints(),
                &bits,
                format,
                start,
                end,
                reversed,
                line,
                length,
            ) else {
                worker.invalid_points += 1;
                continue;
            };
            if !worker.text_allowed(result.getText().len()) {
                break;
            }
            let Some(total) = cached_text
                .checked_add(result.getText().len())
                .filter(|n| *n <= worker.limits.max_total_text_bytes)
            else {
                worker.limit(tr!(
                    "达到条码文字总预算，保留已识别结果",
                    "Total barcode text limit reached; recognized results are kept"
                ));
                break;
            };
            cached_text = total;
            found = Some(Candidate {
                format,
                text: copy_text(result.getText())?,
                lo,
                hi,
            });
            break;
        }
        let Some(candidate) = found else {
            continue;
        };
        worker.record(&candidate, axis, line, previous_bands, current_bands)?;
        if !worker.running() {
            break;
        }
        // 端点是守卫条中间的定位点；排除端点后继续读左右区域，不按文字全局去重。
        let left_end = candidate.lo.floor() as usize;
        let right_start = (candidate.hi.ceil() as usize).saturating_add(1);
        if right_start < end && end - right_start >= MIN_SEGMENT_PIXELS {
            segments.push((right_start, end));
        }
        if left_end > start && left_end - start >= MIN_SEGMENT_PIXELS {
            segments.push((start, left_end));
        }
        candidates.push(candidate);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn result_span(
    points: &[rxing::Point],
    bits: &BitArray,
    format: BarcodeFormat,
    start: usize,
    end: usize,
    reversed: bool,
    line: u32,
    length: usize,
) -> Option<(f64, f64)> {
    let first = points.first()?;
    let second = points.get(1)?;
    let mut mapped = [0.; 2];
    for (index, (target, point)) in mapped.iter_mut().zip([first, second]).enumerate() {
        let (mut x, y) = (f64::from(point.x), f64::from(point.y));
        if format == BarcodeFormat::Code39 && index == 1 {
            x = code39_stop_center(bits, x, end - start)?;
        }
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.
            || x > (end - start - 1) as f64
            || y != f64::from(line)
        {
            return None;
        }
        *target = if reversed {
            (end - 1) as f64 - x
        } else {
            start as f64 + x
        };
        if !target.is_finite() || *target < 0. || *target > (length - 1) as f64 {
            return None;
        }
    }
    let (lo, hi) = (mapped[0].min(mapped[1]), mapped[0].max(mapped[1]));
    (lo < hi).then_some((lo, hi))
}

/// 固定版本返回的停止定位点是停止图案末端索引的一半；先恢复末端，
/// 再在同一条二值行中回数九个非空交替 run，得到真实停止图案的中心。
/// 此修复发生在裁块偏移、反向和纵向映射前，不推测物体外接框。
fn code39_stop_center(bits: &BitArray, encoded_right: f64, length: usize) -> Option<f64> {
    let end = encoded_right * 2.;
    if !end.is_finite() || end.fract() != 0. || end <= 0. || end > length as f64 {
        return None;
    }
    let end = end as usize;
    let mut start = end;
    let mut black = true;
    for _ in 0..9 {
        let previous = start;
        while start > 0 && bits.get(start - 1) == black {
            start -= 1;
        }
        if start == previous {
            return None;
        }
        black = !black;
    }
    if start > 0 && bits.get(start - 1) {
        return None;
    }
    Some((start as f64 + end as f64) / 2.)
}

#[cfg(test)]
#[path = "image_tool_barcode_fixtures.rs"]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use fixtures::{barcode, FixtureFormat};

    fn only(format: BarcodeFormat) -> ScanOptions {
        let mut formats = [false; 7];
        formats[format.index()] = true;
        ScanOptions {
            formats,
            try_inverted: false,
        }
    }

    fn run(image: &Image, options: &ScanOptions) -> ScanReport {
        scan(
            image,
            options,
            ScanLimits::default(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap()
    }

    #[test]
    fn image_tool_barcode_reads_seven_explicit_formats_and_preserves_source() {
        for (format, fixture) in [
            (BarcodeFormat::Code128, FixtureFormat::Code128),
            (BarcodeFormat::Code39, FixtureFormat::Code39),
            (BarcodeFormat::Ean13, FixtureFormat::Ean13),
            (BarcodeFormat::Ean8, FixtureFormat::Ean8),
            (BarcodeFormat::Upca, FixtureFormat::Upca),
            (BarcodeFormat::Upce, FixtureFormat::Upce),
            (BarcodeFormat::Itf, FixtureFormat::Itf),
        ] {
            let (image, expected) = barcode(fixture);
            let before = image.bgra.clone();
            let report = run(&image, &only(format));
            assert_eq!(
                report.status,
                ScanStatus::Complete,
                "{}: {}",
                format.label(),
                report.detail
            );
            assert_eq!(
                report.hits.len(),
                1,
                "{}: {:?}",
                format.label(),
                report.hits
            );
            assert_eq!(report.hits[0].format, format);
            assert_eq!(report.hits[0].text, expected);
            assert_eq!(image.bgra, before);
            assert!(report.progress.decode_attempts < image.height as usize);
            assert!(report
                .hits
                .iter()
                .all(|hit| hit.y1 == hit.y2 && hit.x1 >= 0. && hit.x2 < f64::from(image.width)));
        }
        let (image, _) = barcode(FixtureFormat::Upca);
        let report = run(&image, &ScanOptions::default());
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].format, BarcodeFormat::Upca);
        assert_eq!(report.hits[0].text, "036000291452");
    }

    #[test]
    fn image_tool_barcode_keeps_same_content_at_left_right_and_above_below() {
        let (single, expected) = barcode(FixtureFormat::Code128);
        for image in [
            fixtures::compose_side_by_side(&single, &single),
            fixtures::stacked(&single, &single),
        ] {
            let report = run(&image, &only(BarcodeFormat::Code128));
            assert_eq!(report.status, ScanStatus::Complete);
            assert_eq!(report.hits.len(), 2, "{:?}", report.hits);
            assert!(report.hits.iter().all(|hit| hit.text == expected));
            let (a, b) = (&report.hits[0], &report.hits[1]);
            assert!(a.x2 < b.x1 || b.x2 < a.x1 || a.y1 != b.y1);
        }
    }

    #[test]
    fn image_tool_barcode_code39_stop_points_stay_inside_each_physical_code() {
        let (single, expected) = barcode(FixtureFormat::Code39);
        for padding in [0, 200] {
            let width = single.width + padding;
            let mut padded = Image {
                width,
                height: single.height,
                bgra: vec![255; width as usize * single.height as usize * 4],
                readback_ms: 0.,
            };
            for y in 0..single.height as usize {
                let source = y * single.width as usize * 4;
                let target = (y * width as usize + padding as usize) * 4;
                padded.bgra[target..target + single.width as usize * 4]
                    .copy_from_slice(&single.bgra[source..source + single.width as usize * 4]);
            }
            let report = run(&padded, &only(BarcodeFormat::Code39));
            assert_eq!(report.status, ScanStatus::Complete);
            assert_eq!(report.hits.len(), 1, "{:?}", report.hits);
            let hit = &report.hits[0];
            assert_eq!(hit.text, expected);
            assert_eq!(hit.x1, f64::from(padding + 36));
            assert_eq!(hit.x2, f64::from(padding + 140));
        }
        let image = fixtures::compose_side_by_side(&single, &single);
        let mut report = run(&image, &only(BarcodeFormat::Code39));
        assert_eq!(report.status, ScanStatus::Complete);
        assert_eq!(report.hits.len(), 2, "{:?}", report.hits);
        report.hits.sort_by(|a, b| a.x1.total_cmp(&b.x1));
        assert_eq!((report.hits[0].x1, report.hits[0].x2), (36., 140.));
        let offset = f64::from(single.width + 16);
        assert_eq!(
            (report.hits[1].x1, report.hits[1].x2),
            (offset + 36., offset + 140.)
        );
    }

    #[test]
    fn image_tool_barcode_reverses_rotates_and_inverts_with_source_coordinates() {
        let (mut reversed, expected) = barcode(FixtureFormat::Code128);
        for row in reversed.bgra.chunks_exact_mut(reversed.width as usize * 4) {
            for x in 0..reversed.width as usize / 2 {
                let opposite = reversed.width as usize - x - 1;
                for channel in 0..4 {
                    row.swap(x * 4 + channel, opposite * 4 + channel);
                }
            }
        }
        for image in [
            reversed,
            fixtures::rotate90(&barcode(FixtureFormat::Code128).0),
        ] {
            let report = run(&image, &only(BarcodeFormat::Code128));
            assert_eq!(report.status, ScanStatus::Complete);
            assert_eq!(report.hits.len(), 1);
            let hit = &report.hits[0];
            assert_eq!(hit.text, expected);
            assert!([hit.x1, hit.x2]
                .iter()
                .all(|x| x.is_finite() && *x >= 0. && *x <= f64::from(image.width - 1)));
            assert!([hit.y1, hit.y2]
                .iter()
                .all(|y| y.is_finite() && *y >= 0. && *y <= f64::from(image.height - 1)));
            if image.width == 24 {
                assert_eq!(hit.x1, hit.x2);
                assert!(hit.y1 < hit.y2);
            } else {
                assert_eq!(hit.y1, hit.y2);
                assert!(hit.x1 < hit.x2);
            }
        }
        let (mut inverted, _) = barcode(FixtureFormat::Code128);
        for pixel in inverted.bgra.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel = 255 - *channel;
            }
        }
        let mut options = only(BarcodeFormat::Code128);
        options.try_inverted = true;
        let report = run(&inverted, &options);
        assert_eq!(report.status, ScanStatus::Complete);
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].text, expected);
    }

    #[test]
    fn image_tool_barcode_empty_bad_checksum_and_hidden_alpha_are_not_hits() {
        let bad = fixtures::invalid_checksum_fixture();
        assert!(run(&bad, &only(BarcodeFormat::Ean13)).hits.is_empty());
        let (mut invisible, _) = barcode(FixtureFormat::Code128);
        for pixel in invisible.bgra.chunks_exact_mut(4) {
            pixel[3] = 0;
        }
        let before = invisible.bgra.clone();
        let report = run(&invisible, &ScanOptions::default());
        assert_eq!(report.status, ScanStatus::Complete);
        assert!(report.hits.is_empty());
        assert_eq!(invisible.bgra, before);
        assert_eq!(report.progress.decode_attempts, 0);
    }

    #[test]
    fn image_tool_barcode_cancel_and_limits_keep_partial_results() {
        let (single, expected) = barcode(FixtureFormat::Code128);
        let image = fixtures::compose_side_by_side(&single, &single);
        let cancel = AtomicBool::new(false);
        let report = scan(
            &image,
            &only(BarcodeFormat::Code128),
            ScanLimits::default(),
            &cancel,
            |progress| {
                if progress.results == 1 {
                    cancel.store(true, Ordering::Release);
                }
            },
        )
        .unwrap();
        assert_eq!(report.status, ScanStatus::Cancelled);
        assert_eq!(report.hits.len(), 1);
        let report = scan(
            &image,
            &only(BarcodeFormat::Code128),
            ScanLimits {
                max_decode_attempts: 1,
                ..ScanLimits::default()
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.status, ScanStatus::Limited);
        assert_eq!(report.hits.len(), 1);
        for limits in [
            ScanLimits {
                max_results: 1,
                ..ScanLimits::default()
            },
            ScanLimits {
                max_total_text_bytes: expected.len(),
                ..ScanLimits::default()
            },
        ] {
            let report = scan(
                &image,
                &only(BarcodeFormat::Code128),
                limits,
                &AtomicBool::new(false),
                |progress| {
                    assert!(progress.sampled_pixels <= limits.max_sampled_pixels);
                    assert!(progress.decode_attempts <= limits.max_decode_attempts);
                    assert!(progress.results <= limits.max_results);
                },
            )
            .unwrap();
            assert_eq!(report.status, ScanStatus::Limited);
            assert_eq!(report.hits.len(), 1);
            assert_eq!(report.hits[0].text, expected);
            assert_eq!(report.progress.results, 1);
        }
        let report = scan(
            &image,
            &only(BarcodeFormat::Code128),
            ScanLimits {
                max_text_bytes: expected.len() - 1,
                ..ScanLimits::default()
            },
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.status, ScanStatus::Limited);
        assert!(report.hits.is_empty());
        assert_eq!(report.progress.results, 0);

        let sampled_limit =
            u64::from(image.width) * u64::from(image.height) + u64::from(image.width);
        let report = scan(
            &image,
            &only(BarcodeFormat::Code128),
            ScanLimits {
                max_sampled_pixels: sampled_limit,
                ..ScanLimits::default()
            },
            &AtomicBool::new(false),
            |progress| assert!(progress.sampled_pixels <= sampled_limit),
        )
        .unwrap();
        assert_eq!(report.status, ScanStatus::Limited);
        assert_eq!(report.hits.len(), 2);
        assert!(report.hits.iter().all(|hit| hit.text == expected));
        assert_eq!(report.progress.results, 2);
        assert_eq!(report.progress.sampled_pixels, sampled_limit);
        for limits in [
            ScanLimits {
                max_sampled_pixels: 0,
                ..ScanLimits::default()
            },
            ScanLimits {
                max_duration: Duration::ZERO,
                ..ScanLimits::default()
            },
        ] {
            let report = scan(
                &image,
                &ScanOptions::default(),
                limits,
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
            assert_eq!(report.status, ScanStatus::Limited);
            assert!(report.hits.is_empty());
        }
        let report = scan(
            &image,
            &ScanOptions::default(),
            ScanLimits::default(),
            &AtomicBool::new(true),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.status, ScanStatus::Cancelled);
        assert!(report.hits.is_empty());
        assert!(scan(
            &image,
            &ScanOptions {
                formats: [false; 7],
                try_inverted: false
            },
            ScanLimits::default(),
            &AtomicBool::new(false),
            |_| {}
        )
        .is_err());
        assert!(scan(
            &image,
            &ScanOptions::default(),
            ScanLimits {
                max_total_text_bytes: usize::MAX,
                ..ScanLimits::default()
            },
            &AtomicBool::new(false),
            |_| {}
        )
        .is_err());
    }
}
