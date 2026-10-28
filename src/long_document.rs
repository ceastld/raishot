//! 长图编辑只修改共享条带的几何；预览直接采样，只有最终输出才物化成品。
use crate::{
    capture::Image,
    geometry::Rect,
    stitch::{ScrollAxis, StitchSegment, StitchSnapshot},
    Result,
};
use std::sync::Arc;

const MAX_OUTPUT_PIXELS: u64 = 32_000_000;
const MAX_HISTORY: usize = 128;
const MAX_HISTORY_METADATA_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct LongDocument {
    original: StitchSnapshot,
    current: StitchSnapshot,
    undo: Vec<StitchSnapshot>,
    redo: Vec<StitchSnapshot>,
}
impl LongDocument {
    pub(crate) fn new(snapshot: StitchSnapshot) -> Result<Self> {
        validate(&snapshot)?;
        Ok(Self {
            original: snapshot.clone(),
            current: snapshot,
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }
    pub(crate) fn snapshot(&self) -> StitchSnapshot {
        self.current.clone()
    }
    pub(crate) fn width(&self) -> u32 {
        self.current.width
    }
    pub(crate) fn height(&self) -> u32 {
        self.current.height
    }
    pub(crate) fn axis(&self) -> ScrollAxis {
        self.current.axis
    }
    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    fn commit(&mut self, next: StitchSnapshot) -> bool {
        if same_geometry(&next, &self.current) {
            return false;
        }
        // 满栈先移除最远记录，避免第 129 项令外层 Vec 容量翻倍。
        if self.undo.len() == MAX_HISTORY {
            self.undo.remove(0);
        }
        self.undo.push(std::mem::replace(&mut self.current, next));
        self.redo.clear();
        self.trim_history();
        true
    }
    /// 统计实际几何容量；Arc 指向的像素由 original 共享，不重复计入历史。
    pub(crate) fn history_metadata_bytes(&self) -> usize {
        let outer = std::mem::size_of::<Vec<StitchSnapshot>>()
            .saturating_mul(2)
            .saturating_add(
                self.undo
                    .capacity()
                    .saturating_add(self.redo.capacity())
                    .saturating_mul(std::mem::size_of::<StitchSnapshot>()),
            );
        self.undo
            .iter()
            .chain(&self.redo)
            .fold(outer, |bytes, snapshot| {
                bytes.saturating_add(
                    snapshot
                        .segments
                        .capacity()
                        .saturating_mul(std::mem::size_of::<StitchSegment>()),
                )
            })
    }
    fn trim_history(&mut self) {
        while self.undo.len().saturating_add(self.redo.len()) > MAX_HISTORY
            || self.history_metadata_bytes() > MAX_HISTORY_METADATA_BYTES
        {
            // 两栈的末项都是紧邻状态；优先淘汰最远 Undo，再淘汰最远 Redo。
            // 异常超大单项也保留紧邻一步，不能为预算把刚刚 Undo 的 Redo 删掉。
            if self.undo.len() > 1 {
                self.undo.remove(0);
            } else if self.redo.len() > 1 {
                self.redo.remove(0);
            } else {
                break;
            }
        }
    }
    pub(crate) fn crop(&mut self, rect: Rect) -> Result<bool> {
        if !rect.valid(self.width(), self.height()) {
            return Err("裁剪范围必须在长图内且非空".into());
        }
        Ok(self.commit(cropped(&self.current, rect)))
    }
    pub(crate) fn cut(&mut self, start: u32, end: u32) -> Result<bool> {
        let long = axis_length(&self.current);
        if start > end || end > long || end - start == long {
            return Err("删除范围越界或会删除整张图片".into());
        }
        if start == end {
            return Ok(false);
        }
        let mut next = self.current.clone();
        let before = axis_rect(&self.current, 0, start);
        let after = axis_rect(&self.current, end, long - end);
        next.segments.clear();
        for segment in &self.current.segments {
            for (range, shift) in [(before, 0), (after, end - start)] {
                if let Some(mut piece) = clip_segment(segment, range) {
                    match self.axis() {
                        ScrollAxis::Vertical => piece.origin.1 -= shift,
                        ScrollAxis::Horizontal => piece.origin.0 -= shift,
                    }
                    next.segments.push(piece);
                }
            }
        }
        match self.axis() {
            ScrollAxis::Vertical => next.height -= end - start,
            ScrollAxis::Horizontal => next.width -= end - start,
        }
        next.viewport = cut_viewport(self.current.viewport, self.axis(), start, end);
        sort_segments(&mut next);
        Ok(self.commit(next))
    }
    pub(crate) fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo
            .push(std::mem::replace(&mut self.current, previous));
        self.trim_history();
        true
    }
    pub(crate) fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.current, next));
        self.trim_history();
        true
    }
    pub(crate) fn reset(&mut self) -> bool {
        self.commit(self.original.clone())
    }
    pub(crate) fn export(&self) -> Result<Image> {
        if self.width() > 65535
            || self.height() > 65535
            || self.width() as u64 * self.height() as u64 > MAX_OUTPUT_PIXELS
        {
            return Err("成品超过 65535 单边或 3200 万像素，请先裁剪".into());
        }
        render_region(
            &self.current,
            Rect::full(self.width(), self.height()),
            self.width(),
            self.height(),
        )
    }
}

fn validate(snapshot: &StitchSnapshot) -> Result<()> {
    if snapshot.width == 0 || snapshot.height == 0 || snapshot.segments.is_empty() {
        return Err("长图快照为空".into());
    }
    for segment in &snapshot.segments {
        if !segment
            .source
            .valid(segment.image.width, segment.image.height)
            || segment.image.bgra.len()
                != segment.image.width as usize * segment.image.height as usize * 4
            || !segment_rect(segment).valid(snapshot.width, snapshot.height)
        {
            return Err("长图条带几何或像素无效".into());
        }
    }
    let mut ordered: Vec<_> = snapshot.segments.iter().collect();
    ordered.sort_by_key(|s| match snapshot.axis {
        ScrollAxis::Vertical => s.origin.1,
        ScrollAxis::Horizontal => s.origin.0,
    });
    let mut cursor = 0u32;
    for segment in ordered {
        let (start, cross_origin, length, cross_size, wanted_cross) = match snapshot.axis {
            ScrollAxis::Vertical => (
                segment.origin.1,
                segment.origin.0,
                segment.source.height,
                segment.source.width,
                snapshot.width,
            ),
            ScrollAxis::Horizontal => (
                segment.origin.0,
                segment.origin.1,
                segment.source.width,
                segment.source.height,
                snapshot.height,
            ),
        };
        if start != cursor || cross_origin != 0 || cross_size != wanted_cross {
            return Err("长图条带重叠或缺少完整覆盖".into());
        }
        cursor = cursor.checked_add(length).ok_or("长图条带长度溢出")?;
    }
    if cursor != axis_length(snapshot) {
        return Err("长图条带未覆盖完整图像".into());
    }
    Ok(())
}
fn same_geometry(a: &StitchSnapshot, b: &StitchSnapshot) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.axis == b.axis
        && a.segments.len() == b.segments.len()
        && a.segments.iter().zip(&b.segments).all(|(a, b)| {
            a.origin == b.origin && a.source == b.source && Arc::ptr_eq(&a.image, &b.image)
        })
}
fn segment_rect(segment: &StitchSegment) -> Rect {
    Rect {
        x: segment.origin.0,
        y: segment.origin.1,
        width: segment.source.width,
        height: segment.source.height,
    }
}
fn clip_segment(segment: &StitchSegment, rect: Rect) -> Option<StitchSegment> {
    let original = segment_rect(segment);
    let clip = original.intersection(rect)?;
    Some(StitchSegment {
        image: segment.image.clone(),
        source: Rect {
            x: segment.source.x + clip.x - original.x,
            y: segment.source.y + clip.y - original.y,
            width: clip.width,
            height: clip.height,
        },
        origin: (clip.x, clip.y),
    })
}
fn cropped(snapshot: &StitchSnapshot, rect: Rect) -> StitchSnapshot {
    let mut next = snapshot.clone();
    next.width = rect.width;
    next.height = rect.height;
    next.segments = snapshot
        .segments
        .iter()
        .filter_map(|segment| {
            let mut piece = clip_segment(segment, rect)?;
            piece.origin.0 -= rect.x;
            piece.origin.1 -= rect.y;
            Some(piece)
        })
        .collect();
    next.viewport = snapshot
        .viewport
        .intersection(rect)
        .map_or(Rect::full(0, 0), |v| Rect {
            x: v.x - rect.x,
            y: v.y - rect.y,
            ..v
        });
    sort_segments(&mut next);
    next
}
fn axis_length(snapshot: &StitchSnapshot) -> u32 {
    match snapshot.axis {
        ScrollAxis::Vertical => snapshot.height,
        ScrollAxis::Horizontal => snapshot.width,
    }
}
fn axis_rect(snapshot: &StitchSnapshot, start: u32, length: u32) -> Rect {
    match snapshot.axis {
        ScrollAxis::Vertical => Rect {
            x: 0,
            y: start,
            width: snapshot.width,
            height: length,
        },
        ScrollAxis::Horizontal => Rect {
            x: start,
            y: 0,
            width: length,
            height: snapshot.height,
        },
    }
}
fn cut_viewport(mut viewport: Rect, axis: ScrollAxis, start: u32, end: u32) -> Rect {
    let map = |position: u32| {
        if position <= start {
            position
        } else if position >= end {
            position - (end - start)
        } else {
            start
        }
    };
    match axis {
        ScrollAxis::Vertical => {
            let bottom = map(viewport.y + viewport.height);
            viewport.y = map(viewport.y);
            viewport.height = bottom - viewport.y;
        }
        ScrollAxis::Horizontal => {
            let right = map(viewport.x + viewport.width);
            viewport.x = map(viewport.x);
            viewport.width = right - viewport.x;
        }
    }
    viewport
}
fn sort_segments(snapshot: &mut StitchSnapshot) {
    snapshot.segments.sort_by_key(|s| match snapshot.axis {
        ScrollAxis::Vertical => (s.origin.1, s.origin.0),
        ScrollAxis::Horizontal => (s.origin.0, s.origin.1),
    });
}

pub(crate) fn preview(snapshot: &StitchSnapshot, max_width: u32, max_height: u32) -> Result<Image> {
    if max_width == 0 || max_height == 0 {
        return Err("预览范围不能为空".into());
    }
    let scale = (max_width as f64 / snapshot.width.max(1) as f64)
        .min(max_height as f64 / snapshot.height.max(1) as f64)
        .min(1.);
    let width = (snapshot.width as f64 * scale).round().max(1.) as u32;
    let height = (snapshot.height as f64 * scale).round().max(1.) as u32;
    render_region(
        snapshot,
        Rect::full(snapshot.width, snapshot.height),
        width,
        height,
    )
}

// 侧边预览独立于编辑/导出采样。普通尺寸使用精确面积平均；巨幅来源用有界
// 分层双线性采样，既不展平共享条带，也不为近十亿源像素做逐像素扫描。
const MAX_PREVIEW_READS: u64 = 16_000_000;
const MAX_PREVIEW_PIXELS: u64 = 2_000_000;
const MAX_PREVIEW_SIDE: u32 = 2048;
const MAX_PREVIEW_CACHE_TAPS: usize = 32_768;

#[derive(Clone, Copy)]
enum PreviewFilter {
    Area,
    Stratified(u32),
}

#[derive(Clone, Copy)]
struct PreviewKernel {
    source: u32,
    destination: u32,
    filter: PreviewFilter,
}
impl PreviewKernel {
    fn area(source: u32, destination: u32) -> Self {
        Self {
            source,
            destination,
            filter: PreviewFilter::Area,
        }
    }
    fn bounded(source: u32, destination: u32, strata: u32) -> Self {
        let mut kernel = Self::area(source, destination);
        if kernel.max_taps() > strata * 2 {
            kernel.filter = PreviewFilter::Stratified(strata);
        }
        kernel
    }
    fn max_taps(self) -> u32 {
        match self.filter {
            PreviewFilter::Area => self.source.div_ceil(self.destination).saturating_add(1),
            PreviewFilter::Stratified(strata) => strata * 2,
        }
    }
    fn visit(self, index: u32, mut emit: impl FnMut(u32, f64)) {
        match self.filter {
            PreviewFilter::Area => {
                // 使用有理数边界，非整数倍率及条带边界也覆盖同一像素面积。
                let divisor = self.destination as u64;
                let start = index as u64 * self.source as u64;
                let end = (index as u64 + 1) * self.source as u64;
                for position in start / divisor..end.div_ceil(divisor) {
                    let overlap = end.min((position + 1) * divisor) - start.max(position * divisor);
                    emit(position as u32, overlap as f64 / self.source as f64);
                }
            }
            PreviewFilter::Stratified(strata) => {
                // 每层取双线性中心而非单个最近邻像素，减少细线/棋盘纹的混叠。
                // 夹取限定在本次 rect 内，避免从被截去的页眉或相邻内容串色。
                let scale = self.source as f64 / self.destination as f64;
                for stratum in 0..strata {
                    let position =
                        ((index as f64 + (stratum as f64 + 0.5) / strata as f64) * scale - 0.5)
                            .clamp(0., self.source.saturating_sub(1) as f64);
                    let before = position.floor() as u32;
                    let fraction = position - before as f64;
                    emit(before, (1. - fraction) / strata as f64);
                    if fraction > 0. && before + 1 < self.source {
                        emit(before + 1, fraction / strata as f64);
                    }
                }
            }
        }
    }
}

struct PreviewKernelCache {
    positions: Vec<u32>,
    weights: Vec<f64>,
    ends: Vec<usize>,
}
impl PreviewKernelCache {
    fn new(kernel: PreviewKernel) -> Result<Option<Self>> {
        let bound = match kernel.filter {
            PreviewFilter::Area => kernel.source as u64 + kernel.destination as u64,
            PreviewFilter::Stratified(strata) => kernel.destination as u64 * strata as u64 * 2,
        };
        if bound > MAX_PREVIEW_CACHE_TAPS as u64 {
            return Ok(None);
        }
        let mut positions = Vec::new();
        let mut weights = Vec::new();
        let mut ends = Vec::new();
        positions.try_reserve_exact(bound as usize)?;
        weights.try_reserve_exact(bound as usize)?;
        ends.try_reserve_exact(kernel.destination as usize)?;
        for index in 0..kernel.destination {
            kernel.visit(index, |position, weight| {
                positions.push(position);
                weights.push(weight);
            });
            ends.push(positions.len());
        }
        Ok(Some(Self {
            positions,
            weights,
            ends,
        }))
    }
}

fn filter_preview_row(
    kernel: PreviewKernel,
    cache: Option<&PreviewKernelCache>,
    output: &mut [[f64; 3]],
    mut pixel: impl FnMut(u32) -> [u8; 3],
) {
    if let Some(cache) = cache {
        let mut start = 0;
        for (value, &end) in output.iter_mut().zip(&cache.ends) {
            *value = [0.; 3];
            for (&position, &weight) in cache.positions[start..end]
                .iter()
                .zip(&cache.weights[start..end])
            {
                let color = pixel(position);
                // 发布使用 opt-level=s；显式三通道避免每个源像素的小数组迭代热点。
                value[0] += color[0] as f64 * weight;
                value[1] += color[1] as f64 * weight;
                value[2] += color[2] as f64 * weight;
            }
            start = end;
        }
    } else {
        for (x, value) in output.iter_mut().enumerate() {
            *value = [0.; 3];
            kernel.visit(x as u32, |position, weight| {
                let color = pixel(position);
                value[0] += color[0] as f64 * weight;
                value[1] += color[1] as f64 * weight;
                value[2] += color[2] as f64 * weight;
            });
        }
    }
}

struct PreviewPlan {
    x: PreviewKernel,
    y: PreviewKernel,
    read_bound: u64,
}
impl PreviewPlan {
    fn new(rect: Rect, width: u32, height: u32) -> Self {
        // 源行单调访问，上一行的横向过滤结果会复用；垂直重叠不重复读源像素。
        // 横向面积核在相邻输出像素间至多重复一次边界像素。
        let exact_reads = (rect.width as u64 + width as u64).saturating_mul(rect.height as u64);
        if exact_reads <= MAX_PREVIEW_READS {
            return Self {
                x: PreviewKernel::area(rect.width, width),
                y: PreviewKernel::area(rect.height, height),
                read_bound: exact_reads,
            };
        }
        let pixels = width as u64 * height as u64;
        let strata = (((MAX_PREVIEW_READS / pixels) as f64).sqrt() as u32 / 2).clamp(1, 4);
        let x = PreviewKernel::bounded(rect.width, width, strata);
        let y = PreviewKernel::bounded(rect.height, height, strata);
        Self {
            x,
            y,
            read_bound: pixels * x.max_taps() as u64 * y.max_taps() as u64,
        }
    }
}

struct PreviewReader<'a> {
    segments: Vec<&'a StitchSegment>,
    axis: ScrollAxis,
    index: usize,
}
impl<'a> PreviewReader<'a> {
    fn new(snapshot: &'a StitchSnapshot) -> Self {
        let mut segments: Vec<_> = snapshot.segments.iter().collect();
        segments.sort_by_key(|s| match snapshot.axis {
            ScrollAxis::Vertical => s.origin.1,
            ScrollAxis::Horizontal => s.origin.0,
        });
        Self {
            segments,
            axis: snapshot.axis,
            index: 0,
        }
    }
    fn position(&mut self, position: u32) {
        let origin = |s: &StitchSegment| match self.axis {
            ScrollAxis::Vertical => s.origin.1,
            ScrollAxis::Horizontal => s.origin.0,
        };
        if origin(self.segments[self.index]) > position {
            self.index = self.segments.partition_point(|s| origin(s) <= position) - 1;
        } else {
            while self.index + 1 < self.segments.len()
                && origin(self.segments[self.index + 1]) <= position
            {
                self.index += 1;
            }
        }
    }
    fn pixel(&mut self, x: u32, y: u32) -> [u8; 3] {
        let position = match self.axis {
            ScrollAxis::Vertical => y,
            ScrollAxis::Horizontal => x,
        };
        self.position(position);
        // validate 已保证每个源位置恰好由一个完整跨轴条带覆盖。
        let segment = self.segments[self.index];
        let offset = (((y - segment.origin.1 + segment.source.y) as usize
            * segment.image.width as usize)
            + (x - segment.origin.0 + segment.source.x) as usize)
            * 4;
        let pixel = &segment.image.bgra[offset..offset + 3];
        [pixel[0], pixel[1], pixel[2]]
    }
    fn filtered_row(
        &mut self,
        x: u32,
        y: u32,
        kernel: PreviewKernel,
        cache: Option<&PreviewKernelCache>,
        output: &mut [[f64; 3]],
    ) {
        match self.axis {
            ScrollAxis::Vertical => {
                // 纵向条带整行共用同一源图，只定位一次，避免每像素查找/计算源行。
                self.position(y);
                let segment = self.segments[self.index];
                let offset = (((y - segment.origin.1 + segment.source.y) as usize
                    * segment.image.width as usize)
                    + segment.source.x as usize)
                    * 4;
                let row = &segment.image.bgra[offset..offset + segment.source.width as usize * 4];
                filter_preview_row(kernel, cache, output, |sx| {
                    let offset = (x + sx) as usize * 4;
                    [row[offset], row[offset + 1], row[offset + 2]]
                });
            }
            ScrollAxis::Horizontal => {
                filter_preview_row(kernel, cache, output, |sx| self.pixel(x + sx, y));
            }
        }
    }
}

pub(crate) fn render_preview_region(
    snapshot: &StitchSnapshot,
    rect: Rect,
    width: u32,
    height: u32,
) -> Result<Image> {
    validate(snapshot)?;
    if !rect.valid(snapshot.width, snapshot.height)
        || width == 0
        || height == 0
        || width > MAX_PREVIEW_SIDE
        || height > MAX_PREVIEW_SIDE
        || width as u64 * height as u64 > MAX_PREVIEW_PIXELS
    {
        return Err("长图侧边预览范围无效或超过像素限额".into());
    }
    if width == rect.width && height == rect.height {
        return render_region(snapshot, rect, width, height);
    }
    let plan = PreviewPlan::new(rect, width, height);
    debug_assert!(plan.read_bound <= MAX_PREVIEW_READS);
    let x_cache = PreviewKernelCache::new(plan.x)?;
    let mut reader = PreviewReader::new(snapshot);
    // 只保留两条目标宽度的浮点行（500px 时共 24KB），不构造源宽度/整图中间缓存。
    let mut cached = Vec::new();
    cached.try_reserve_exact(width as usize)?;
    cached.resize(width as usize, [0f64; 3]);
    let mut accumulated = Vec::new();
    accumulated.try_reserve_exact(width as usize)?;
    accumulated.resize(width as usize, [0f64; 3]);
    let mut cached_y = None;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(width as usize * height as usize * 4)?;
    for y in 0..height {
        accumulated.fill([0.; 3]);
        plan.y.visit(y, |sy, wy| {
            if cached_y != Some(sy) {
                reader.filtered_row(rect.x, rect.y + sy, plan.x, x_cache.as_ref(), &mut cached);
                cached_y = Some(sy);
            }
            for (value, source) in accumulated.iter_mut().zip(&cached) {
                value[0] += source[0] * wy;
                value[1] += source[1] * wy;
                value[2] += source[2] * wy;
            }
        });
        for value in &accumulated {
            bgra.extend_from_slice(&[
                value[0].round().clamp(0., 255.) as u8,
                value[1].round().clamp(0., 255.) as u8,
                value[2].round().clamp(0., 255.) as u8,
                255,
            ]);
        }
    }
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

/// 长轴上第 `index` 条缩略线覆盖的源位置与权重。全局网格比例为 `source:dest`（与跨轴一致），
/// 末条只覆盖到长图末端时按实际覆盖面积归一，不变暗。
fn preview_line_taps(
    index: u32,
    source: u32,
    dest: u32,
    long: u32,
    strata: Option<u32>,
    mut emit: impl FnMut(u32, f64),
) {
    let divisor = dest as u64;
    let start = index as u64 * source as u64;
    let end = ((index as u64 + 1) * source as u64).min(long as u64 * divisor);
    if end <= start {
        return;
    }
    match strata {
        None => {
            let span = (end - start) as f64;
            for position in start / divisor..end.div_ceil(divisor) {
                let overlap = end.min((position + 1) * divisor) - start.max(position * divisor);
                emit(position as u32, overlap as f64 / span);
            }
        }
        Some(strata) => {
            let (begin, length) = (
                start as f64 / divisor as f64,
                (end - start) as f64 / divisor as f64,
            );
            for stratum in 0..strata {
                let position = (begin + (stratum as f64 + 0.5) / strata as f64 * length - 0.5)
                    .clamp(0., long.saturating_sub(1) as f64);
                let before = position.floor() as u32;
                let fraction = position - before as f64;
                emit(before, (1. - fraction) / strata as f64);
                if fraction > 0. && before + 1 < long {
                    emit(before + 1, fraction / strata as f64);
                }
            }
        }
    }
}

/// 按固定比例 `source:dest`（跨轴源长:缩略长）渲染全局缩略网格中长轴 `lines` 段。
/// 返回逐线 BGRA：纵向每线是一行，横向每线是一列；跨轴始终完整 `dest` 像素。
/// 网格只取决于比例，与当前投影窗口无关，因此已渲染的线可在增长和窗口滑动时复用。
pub(crate) fn render_preview_lines(
    snapshot: &StitchSnapshot,
    source: u32,
    dest: u32,
    lines: std::ops::Range<u32>,
) -> Result<Vec<Vec<u8>>> {
    validate(snapshot)?;
    let (cross, long) = match snapshot.axis {
        ScrollAxis::Vertical => (snapshot.width, snapshot.height),
        ScrollAxis::Horizontal => (snapshot.height, snapshot.width),
    };
    let total = (long as u64 * dest as u64).div_ceil(source.max(1) as u64);
    if source != cross
        || dest == 0
        || dest > source
        || dest > MAX_PREVIEW_SIDE
        || lines.start >= lines.end
        || lines.end as u64 > total
    {
        return Err("长图缩略网格范围无效".into());
    }
    // 倍率很大时改用分层采样，限制单线读取量；常见 1～8 倍保持精确面积平均。
    let strata = (source > dest.saturating_mul(8)).then_some(4);
    let cross_kernel = if strata.is_some() {
        PreviewKernel::bounded(source, dest, 4)
    } else {
        PreviewKernel::area(source, dest)
    };
    let cross_cache = PreviewKernelCache::new(cross_kernel)?;
    let mut reader = PreviewReader::new(snapshot);
    let count = (lines.end - lines.start) as usize;
    let mut accumulated = vec![[0f64; 3]; dest as usize];
    let pack = |value: &[f64; 3], out: &mut Vec<u8>| {
        out.extend_from_slice(&[
            value[0].round().clamp(0., 255.) as u8,
            value[1].round().clamp(0., 255.) as u8,
            value[2].round().clamp(0., 255.) as u8,
            255,
        ]);
    };
    let mut output = Vec::new();
    output.try_reserve_exact(count)?;
    match snapshot.axis {
        ScrollAxis::Vertical => {
            let mut cached = vec![[0f64; 3]; dest as usize];
            let mut cached_y = None;
            for index in lines {
                accumulated.fill([0.; 3]);
                preview_line_taps(index, source, dest, long, strata, |sy, wy| {
                    if cached_y != Some(sy) {
                        reader.filtered_row(0, sy, cross_kernel, cross_cache.as_ref(), &mut cached);
                        cached_y = Some(sy);
                    }
                    for (value, source) in accumulated.iter_mut().zip(&cached) {
                        value[0] += source[0] * wy;
                        value[1] += source[1] * wy;
                        value[2] += source[2] * wy;
                    }
                });
                let mut line = Vec::new();
                line.try_reserve_exact(dest as usize * 4)?;
                for value in &accumulated {
                    pack(value, &mut line);
                }
                output.push(line);
            }
        }
        ScrollAxis::Horizontal => {
            // 先按列收集长轴采样位置，再逐个跨轴源行累加，源行内单调访问条带。
            let taps: Vec<Vec<(u32, f64)>> = lines
                .map(|index| {
                    let mut taps = Vec::new();
                    preview_line_taps(index, source, dest, long, strata, |sx, wx| {
                        taps.push((sx, wx))
                    });
                    taps
                })
                .collect();
            let mut columns = vec![[0f64; 3]; count * dest as usize];
            for y in 0..dest {
                cross_kernel.visit(y, |sy, wy| {
                    for (column, taps) in taps.iter().enumerate() {
                        let value = &mut columns[column * dest as usize + y as usize];
                        for &(sx, wx) in taps {
                            let color = reader.pixel(sx, sy);
                            let weight = wx * wy;
                            value[0] += color[0] as f64 * weight;
                            value[1] += color[1] as f64 * weight;
                            value[2] += color[2] as f64 * weight;
                        }
                    }
                });
            }
            for column in columns.chunks_exact(dest as usize) {
                let mut line = Vec::new();
                line.try_reserve_exact(dest as usize * 4)?;
                for value in column {
                    pack(value, &mut line);
                }
                output.push(line);
            }
        }
    }
    Ok(output)
}

pub(crate) fn render_region(
    snapshot: &StitchSnapshot,
    rect: Rect,
    width: u32,
    height: u32,
) -> Result<Image> {
    validate(snapshot)?;
    if !rect.valid(snapshot.width, snapshot.height)
        || width == 0
        || height == 0
        || width as u64 * height as u64 > MAX_OUTPUT_PIXELS
    {
        return Err("长图预览/输出范围无效或超过像素限额".into());
    }
    let mut ordered: Vec<_> = snapshot.segments.iter().collect();
    ordered.sort_by_key(|s| match snapshot.axis {
        ScrollAxis::Vertical => s.origin.1,
        ScrollAxis::Horizontal => s.origin.0,
    });
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(width as usize * height as usize * 4)?;
    if width == rect.width && height == rect.height {
        // 最终输出按条带逐行拷贝，避免在每个原图像素上重复二分查找。
        bgra.resize(width as usize * height as usize * 4, 0);
        for segment in ordered {
            let origin = segment_rect(segment);
            if let Some(part) = origin.intersection(rect) {
                let bytes = part.width as usize * 4;
                for row in 0..part.height {
                    let source = (((part.y - origin.y + segment.source.y + row) as usize
                        * segment.image.width as usize)
                        + (part.x - origin.x + segment.source.x) as usize)
                        * 4;
                    let dest = (((part.y - rect.y + row) as usize * width as usize)
                        + (part.x - rect.x) as usize)
                        * 4;
                    bgra[dest..dest + bytes]
                        .copy_from_slice(&segment.image.bgra[source..source + bytes]);
                }
            }
        }
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        return Ok(Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        });
    }
    for y in 0..height {
        let sy = rect.y + ((y as u64 * rect.height as u64) / height as u64) as u32;
        for x in 0..width {
            let sx = rect.x + ((x as u64 * rect.width as u64) / width as u64) as u32;
            let position = match snapshot.axis {
                ScrollAxis::Vertical => sy,
                ScrollAxis::Horizontal => sx,
            };
            let index = ordered.partition_point(|s| match snapshot.axis {
                ScrollAxis::Vertical => s.origin.1 <= position,
                ScrollAxis::Horizontal => s.origin.0 <= position,
            });
            let pixel = index
                .checked_sub(1)
                .and_then(|i| {
                    let s = ordered[i];
                    let r = segment_rect(s);
                    if !r.contains((sx as i32, sy as i32)) {
                        return None;
                    }
                    let offset = (((sy - r.y + s.source.y) as usize * s.image.width as usize)
                        + (sx - r.x + s.source.x) as usize)
                        * 4;
                    Some(&s.image.bgra[offset..offset + 4])
                })
                .ok_or("长图快照缺少覆盖该位置的条带")?;
            bgra.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

#[cfg(test)]
mod preview_quality_tests {
    use super::*;

    fn image(width: u32, height: u32, mut color: impl FnMut(u32, u32) -> [u8; 4]) -> Arc<Image> {
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(&color(x, y));
            }
        }
        Arc::new(Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        })
    }
    fn snapshot(image: Arc<Image>, source: Rect, axis: ScrollAxis) -> StitchSnapshot {
        StitchSnapshot {
            width: source.width,
            height: source.height,
            axis,
            viewport: Rect::full(source.width, source.height),
            segments: vec![StitchSegment {
                image,
                source,
                origin: (0, 0),
            }],
        }
    }
    fn pixel(image: &Image, x: u32, y: u32) -> &[u8] {
        let offset = (y as usize * image.width as usize + x as usize) * 4;
        &image.bgra[offset..offset + 4]
    }

    #[test]
    fn long_preview_quality_area_keeps_single_pixel_lines_and_opaque_alpha() {
        let source = image(12, 12, |x, y| {
            let value = if x == 1 || y == 5 { 0 } else { 255 };
            [value, value, value, 0]
        });
        let document = snapshot(source, Rect::full(12, 12), ScrollAxis::Vertical);
        let filtered = render_preview_region(&document, Rect::full(12, 12), 6, 6).unwrap();
        // 最近邻会完全漏掉奇数坐标的细线；面积平均保留其实际覆盖率。
        assert_eq!(pixel(&filtered, 0, 0), [128, 128, 128, 255]);
        assert_eq!(pixel(&filtered, 1, 2), [128, 128, 128, 255]);
        assert_eq!(pixel(&filtered, 0, 2), [64, 64, 64, 255]);
        assert_eq!(pixel(&filtered, 1, 0), [255, 255, 255, 255]);
        assert!(filtered.bgra.chunks_exact(4).all(|p| p[3] == 255));
        assert_eq!(
            pixel(
                &render_region(&document, Rect::full(12, 12), 6, 6).unwrap(),
                0,
                0
            ),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn long_preview_quality_fractional_filter_has_no_vertical_or_horizontal_strip_seams() {
        let source = image(16, 14, |x, y| {
            [(x * 17) as u8, (y * 19) as u8, ((x + y) * 7) as u8, 13]
        });
        let source_rect = Rect {
            x: 2,
            y: 3,
            width: 11,
            height: 9,
        };
        for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
            let full = snapshot(source.clone(), source_rect, axis);
            let mut split = full.clone();
            split.segments = match axis {
                ScrollAxis::Vertical => vec![
                    StitchSegment {
                        image: source.clone(),
                        source: Rect {
                            height: 4,
                            ..source_rect
                        },
                        origin: (0, 0),
                    },
                    StitchSegment {
                        image: source.clone(),
                        source: Rect {
                            y: 7,
                            height: 5,
                            ..source_rect
                        },
                        origin: (0, 4),
                    },
                ],
                ScrollAxis::Horizontal => vec![
                    StitchSegment {
                        image: source.clone(),
                        source: Rect {
                            width: 5,
                            ..source_rect
                        },
                        origin: (0, 0),
                    },
                    StitchSegment {
                        image: source.clone(),
                        source: Rect {
                            x: 7,
                            width: 6,
                            ..source_rect
                        },
                        origin: (5, 0),
                    },
                ],
            };
            split.segments.reverse();
            let rect = Rect {
                x: 1,
                y: 1,
                width: 9,
                height: 7,
            };
            let expected = render_preview_region(&full, rect, 4, 3).unwrap();
            let actual = render_preview_region(&split, rect, 4, 3).unwrap();
            assert_eq!(actual.bgra, expected.bgra, "{axis:?}");
            assert!(split
                .segments
                .iter()
                .all(|s| Arc::ptr_eq(&s.image, &source)));
        }
    }

    #[test]
    fn long_preview_quality_filter_does_not_bleed_outside_requested_region() {
        let source = image(12, 10, |x, y| {
            if (3..9).contains(&x) && (2..8).contains(&y) {
                [240, 245, 250, 21]
            } else {
                [255, 0, 255, 255]
            }
        });
        let document = snapshot(source, Rect::full(12, 10), ScrollAxis::Vertical);
        let rect = Rect {
            x: 3,
            y: 2,
            width: 6,
            height: 6,
        };
        for dimensions in [(4, 3), (11, 9)] {
            let output =
                render_preview_region(&document, rect, dimensions.0, dimensions.1).unwrap();
            assert!(output
                .bgra
                .chunks_exact(4)
                .all(|p| p == [240, 245, 250, 255]));
        }
    }

    #[test]
    fn long_preview_quality_giant_source_has_bounded_reads_and_normalized_kernels() {
        let plan = PreviewPlan::new(Rect::full(16_384, 57_344), 500, 1750);
        assert!(matches!(plan.x.filter, PreviewFilter::Stratified(_)));
        assert!(matches!(plan.y.filter, PreviewFilter::Stratified(_)));
        assert!(plan.read_bound <= MAX_PREVIEW_READS);
        let count = |kernel: PreviewKernel| {
            let mut taps = 0u64;
            for index in 0..kernel.destination {
                let mut weight = 0.;
                kernel.visit(index, |position, value| {
                    assert!(position < kernel.source);
                    assert!(value > 0. && value <= 1.);
                    weight += value;
                    taps += 1;
                });
                assert!((weight - 1.).abs() < 1e-12);
            }
            taps
        };
        // 只枚举约 9000 个核位置，证明实际循环上界；不分配/访问巨幅源图。
        assert!(count(plan.x) * count(plan.y) <= MAX_PREVIEW_READS);
        let normal = PreviewPlan::new(Rect::full(1920, 6720), 400, 1400);
        assert!(matches!(normal.x.filter, PreviewFilter::Area));
        assert!(matches!(normal.y.filter, PreviewFilter::Area));
        assert!(normal.read_bound <= MAX_PREVIEW_READS);
        for kernel in [normal.x, plan.x] {
            let cache = PreviewKernelCache::new(kernel).unwrap().unwrap();
            let mut start = 0;
            for (index, &end) in cache.ends.iter().enumerate() {
                let mut original = Vec::new();
                kernel.visit(index as u32, |position, weight| {
                    original.push((position, weight));
                });
                let cached: Vec<_> = cache.positions[start..end]
                    .iter()
                    .copied()
                    .zip(cache.weights[start..end].iter().copied())
                    .collect();
                assert_eq!(cached, original);
                start = end;
            }
        }
        // 最大权重缓存 + 最大两条浮点行仍小于 0.5 MiB；超限不先生成巨型核。
        let cache = PreviewKernelCache::new(PreviewKernel::area(30_720, 2048))
            .unwrap()
            .unwrap();
        let temporary_bytes = cache.positions.capacity() * std::mem::size_of::<u32>()
            + cache.weights.capacity() * std::mem::size_of::<f64>()
            + cache.ends.capacity() * std::mem::size_of::<usize>()
            + 2 * 2048 * std::mem::size_of::<[f64; 3]>();
        assert!(temporary_bytes < 512 * 1024);
        assert!(PreviewKernelCache::new(PreviewKernel::area(1_000_000, 1))
            .unwrap()
            .is_none());
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    const DENSE_HEIGHT: u32 = 32_000;

    fn dense_document() -> (LongDocument, Arc<Image>) {
        // 真实几何密度，不分配 3 万行 BGRA；共享的 4 KiB 图保留可区分的行。
        let image = Arc::new(Image {
            width: 32,
            height: 32,
            bgra: (0..32u8)
                .flat_map(|row| [row, row ^ 0x5a, 200, 255].repeat(32))
                .collect(),
            readback_ms: 0.,
        });
        let snapshot = StitchSnapshot {
            width: 32,
            height: DENSE_HEIGHT,
            axis: ScrollAxis::Vertical,
            viewport: Rect::full(32, 640),
            segments: (0..DENSE_HEIGHT)
                .map(|row| StitchSegment {
                    image: image.clone(),
                    source: Rect {
                        x: 0,
                        y: row % 32,
                        width: 32,
                        height: 1,
                    },
                    origin: (0, row),
                })
                .collect(),
        };
        (LongDocument::new(snapshot).unwrap(), image)
    }

    fn crop_one_row(document: &mut LongDocument) {
        assert!(document
            .crop(Rect {
                x: 0,
                y: 1,
                width: document.width(),
                height: document.height() - 1,
            })
            .unwrap());
    }

    fn assert_snapshot(actual: &StitchSnapshot, expected: &StitchSnapshot) {
        assert!(same_geometry(actual, expected));
        assert_eq!(actual.viewport, expected.viewport);
    }

    fn assert_shared_pixels(document: &LongDocument, image: &Arc<Image>) {
        for snapshot in std::iter::once(&document.original)
            .chain(std::iter::once(&document.current))
            .chain(&document.undo)
            .chain(&document.redo)
        {
            assert!(snapshot
                .segments
                .iter()
                .all(|segment| Arc::ptr_eq(&segment.image, image)));
        }
        assert_eq!(image.bgra.len(), 4096);
    }

    fn assert_budget(document: &LongDocument) {
        assert!(document.history_metadata_bytes() <= MAX_HISTORY_METADATA_BYTES);
        assert!(document.undo.len() + document.redo.len() <= MAX_HISTORY);
    }

    #[test]
    fn long_storage_dense_strips_trim_history_by_allocated_metadata() {
        let (mut document, image) = dense_document();
        // 容量而非可见长度计费；这份宽裕数组随第一项历史被保留后应受预算管理。
        document
            .current
            .segments
            .reserve_exact(DENSE_HEIGHT as usize);
        let allocated = document.current.segments.capacity() * std::mem::size_of::<StitchSegment>();
        crop_one_row(&mut document);
        assert!(document.history_metadata_bytes() >= allocated);
        for _ in 1..40 {
            crop_one_row(&mut document);
            assert_budget(&document);
        }
        assert_eq!(document.height(), DENSE_HEIGHT - 40);
        assert!(!document.undo.is_empty());
        assert!(
            document.undo.len() < 40,
            "密集几何须由字节预算提前淘汰，不能只靠128项"
        );
        let newest = document.snapshot();
        assert!(document.undo());
        assert_eq!(document.height(), DENSE_HEIGHT - 39);
        assert!(document.redo());
        assert_snapshot(&document.snapshot(), &newest);
        assert_budget(&document);
        assert_shared_pixels(&document, &image);
    }

    #[test]
    fn long_storage_dense_history_keeps_adjacent_undo_redo_after_budget_trim() {
        let (mut document, image) = dense_document();
        for _ in 0..40 {
            crop_one_row(&mut document);
        }
        let newest = document.snapshot();
        for step in 1..=6 {
            assert!(document.undo());
            let removed = 40 - step;
            assert_eq!(document.height(), DENSE_HEIGHT - removed);
            assert_eq!(document.current.segments[0].source.y, removed % 32);
            assert_eq!(document.current.segments[0].origin, (0, 0));
            assert_eq!(document.current.viewport, Rect::full(32, 640 - removed));
            assert_budget(&document);
            assert!(document.can_redo());
        }
        for step in 1..=6 {
            assert!(document.redo());
            let removed = 34 + step;
            assert_eq!(document.height(), DENSE_HEIGHT - removed);
            assert_eq!(document.current.segments[0].source.y, removed % 32);
            assert_budget(&document);
        }
        assert_snapshot(&document.snapshot(), &newest);
        assert_shared_pixels(&document, &image);
    }

    #[test]
    fn long_storage_dense_branch_clears_redo_and_retains_branch_undo() {
        let (mut document, image) = dense_document();
        for _ in 0..40 {
            crop_one_row(&mut document);
        }
        assert!(document.undo());
        assert!(document.undo());
        let before = document.snapshot();
        assert!(document.can_redo());
        assert!(document.cut(10, 13).unwrap());
        assert!(!document.can_redo());
        assert_eq!(document.height(), before.height - 3);
        assert_eq!(document.current.segments[10].source.y, (38 + 13) % 32);
        let branched = document.snapshot();
        assert!(document.undo());
        assert_snapshot(&document.snapshot(), &before);
        assert!(document.redo());
        assert_snapshot(&document.snapshot(), &branched);
        assert_budget(&document);
        assert_shared_pixels(&document, &image);
    }

    #[test]
    fn long_storage_history_count_is_shared_between_undo_and_redo() {
        let image = Arc::new(Image {
            width: 32,
            height: 256,
            bgra: [17, 33, 65, 255].repeat(32 * 256),
            readback_ms: 0.,
        });
        let mut document = LongDocument::new(StitchSnapshot {
            width: 32,
            height: 256,
            axis: ScrollAxis::Vertical,
            viewport: Rect::full(32, 256),
            segments: vec![StitchSegment {
                image,
                source: Rect::full(32, 256),
                origin: (0, 0),
            }],
        })
        .unwrap();
        for _ in 0..150 {
            crop_one_row(&mut document);
        }
        assert_eq!(document.undo.len(), MAX_HISTORY);
        for _ in 0..MAX_HISTORY {
            assert!(document.undo());
            assert_eq!(document.undo.len() + document.redo.len(), MAX_HISTORY);
            assert_budget(&document);
        }
        assert_eq!(document.height(), 234);
        assert!(!document.undo());
        for _ in 0..MAX_HISTORY {
            assert!(document.redo());
            assert_eq!(document.undo.len() + document.redo.len(), MAX_HISTORY);
        }
        assert_eq!(document.height(), 106);
        assert!(!document.redo());
    }

    #[test]
    fn long_storage_noop_and_invalid_geometry_preserve_budgeted_redo() {
        let (mut document, image) = dense_document();
        for _ in 0..40 {
            crop_one_row(&mut document);
        }
        assert!(document.undo());
        let before = document.snapshot();
        let next = document.redo.last().unwrap().clone();
        let history = (
            document.undo.len(),
            document.redo.len(),
            document.history_metadata_bytes(),
        );
        assert!(!document
            .crop(Rect::full(document.width(), document.height()))
            .unwrap());
        assert!(!document.cut(7, 7).unwrap());
        assert!(document
            .crop(Rect::full(document.width() + 1, document.height()))
            .is_err());
        assert!(document.cut(0, document.height()).is_err());
        assert_eq!(
            (
                document.undo.len(),
                document.redo.len(),
                document.history_metadata_bytes(),
            ),
            history
        );
        assert_snapshot(&document.snapshot(), &before);
        assert!(document.redo());
        assert_snapshot(&document.snapshot(), &next);
        assert_budget(&document);
        assert_shared_pixels(&document, &image);
    }

    #[test]
    fn long_storage_reset_is_atomic_shared_and_noop_preserves_redo() {
        let (mut document, image) = dense_document();
        for _ in 0..40 {
            crop_one_row(&mut document);
        }
        let edited = document.snapshot();
        assert!(document.reset());
        assert_eq!(document.height(), DENSE_HEIGHT);
        assert!(document.undo());
        assert_snapshot(&document.snapshot(), &edited);
        assert!(document.redo());
        assert_eq!(document.height(), DENSE_HEIGHT);
        crop_one_row(&mut document);
        let next = document.snapshot();
        assert!(document.undo());
        let bytes = document.history_metadata_bytes();
        assert!(!document.reset());
        assert!(document.can_redo());
        assert_eq!(document.history_metadata_bytes(), bytes);
        assert!(document.redo());
        assert_snapshot(&document.snapshot(), &next);
        assert_budget(&document);
        assert_shared_pixels(&document, &image);
    }
}
