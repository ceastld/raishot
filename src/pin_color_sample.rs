//! 当前显示倍率的内容快照取色：共享 CPU 底图与 Scene，不读 GPU 或烘焙整张图。
use crate::{
    annotation::Point,
    capture::Image,
    geometry::Rect,
    output_projection::OutputProjection,
    scene::{Scene, SourcePixels},
    viewport::Viewport,
    Result,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColorSample {
    /// 内容快照的预乘 BGRA；不包含棋盘格、窗口透明度或辅助界面。
    pub bgra: [u8; 4],
    pub source_point: (u32, u32),
}
#[cfg(test)]
impl ColorSample {
    pub fn hex(self) -> String {
        format!(
            "#{:02X}{:02X}{:02X}",
            self.bgra[2], self.bgra[1], self.bgra[0]
        )
    }
}

pub(crate) struct ColorPatch {
    /// 邻域保持 straight BGRA，供放大镜绘制；最多 15×9 个目标像素。
    pub image: Image,
    /// 被采样像素在裁边后的邻域中的实际位置。
    pub center: (u32, u32),
    pub sample: ColorSample,
}

struct SampleTarget {
    projection: OutputProjection,
    index: (u32, u32),
    source_point: (u32, u32),
}

fn snapshot_projection(source: &SourcePixels<'_>, zoom: f32) -> Result<OutputProjection> {
    let rect = source.canvas;
    if rect.width == 0 || rect.height == 0 || !zoom.is_finite() || zoom <= 0. {
        return Err("取色图像尺寸或缩放无效".into());
    }
    let width = (f64::from(rect.width) * f64::from(zoom))
        .round_ties_even()
        .max(1.);
    let height = (f64::from(rect.height) * f64::from(zoom))
        .round_ties_even()
        .max(1.);
    if width > u32::MAX as f64 || height > u32::MAX as f64 {
        return Err("取色快照尺寸溢出".into());
    }
    Ok(OutputProjection {
        source: rect,
        width: width as u32,
        height: height as u32,
        sx: width as f32 / rect.width as f32,
        sy: height as f32 / rect.height as f32,
        origin: Point { x: 0., y: 0. },
    })
}

fn pixel(source: &SourcePixels<'_>, x: u32, y: u32) -> Result<[u8; 4]> {
    let mut result = None;
    for plane in source.planes.iter().chain(source.overlay.iter()) {
        if x >= plane.rect.x
            && y >= plane.rect.y
            && x - plane.rect.x < plane.rect.width
            && y - plane.rect.y < plane.rect.height
        {
            let index = ((y - plane.rect.y) as usize)
                .checked_mul(plane.rect.width as usize)
                .and_then(|row| row.checked_add((x - plane.rect.x) as usize))
                .and_then(|index| index.checked_mul(4))
                .ok_or("取色像素索引溢出")?;
            let end = index.checked_add(4).ok_or("取色像素索引溢出")?;
            let bytes = plane.bgra.get(index..end).ok_or("取色源像素不完整")?;
            result = Some([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
    }
    result
        .or_else(|| {
            let canvas = source.canvas;
            (x >= canvas.x
                && y >= canvas.y
                && x - canvas.x < canvas.width
                && y - canvas.y < canvas.height)
                .then_some(source.fill)
                .flatten()
        })
        .ok_or_else(|| "取色位置缺少源像素".into())
}

pub(crate) fn linear_pixel(source: &SourcePixels<'_>, point: Point) -> Result<[u8; 4]> {
    let rect = source.canvas;
    let right = rect.x.checked_add(rect.width).ok_or("取色源范围溢出")?;
    let bottom = rect.y.checked_add(rect.height).ok_or("取色源范围溢出")?;
    let x = (f64::from(point.x) - 0.5).clamp(f64::from(rect.x), f64::from(right - 1));
    let y = (f64::from(point.y) - 0.5).clamp(f64::from(rect.y), f64::from(bottom - 1));
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = x0.saturating_add(1).min(right - 1);
    let y1 = y0.saturating_add(1).min(bottom - 1);
    let dx = x - f64::from(x0);
    let dy = y - f64::from(y0);
    let mut sums = [0.; 4];
    for (px, py, weight) in [
        (x0, y0, (1. - dx) * (1. - dy)),
        (x1, y0, dx * (1. - dy)),
        (x0, y1, (1. - dx) * dy),
        (x1, y1, dx * dy),
    ] {
        if weight == 0. {
            continue;
        }
        let p = pixel(source, px, py)?;
        let alpha = f64::from(p[3]) / 255.;
        for c in 0..3 {
            sums[c] += f64::from(p[c]) * alpha * weight;
        }
        sums[3] += f64::from(p[3]) * weight;
    }
    // Scene 使用 straight BGRA；插值先按预乘色进行，避免透明像素隐藏RGB串色。
    let mut result = [0; 4];
    if sums[3] > 0. {
        for c in 0..3 {
            result[c] = (sums[c] * 255. / sums[3]).round().clamp(0., 255.) as u8;
        }
        result[3] = sums[3].round().clamp(0., 255.) as u8;
    }
    Ok(result)
}

fn background(source: &SourcePixels<'_>, projection: &OutputProjection) -> Result<Image> {
    linear_background(source, projection)
}

/// 一个目标列/行在源图上的两个相邻下标与小数权重；与 [`linear_pixel`] 同一 f64 计算。
#[derive(Clone, Copy)]
struct Tap {
    first: u32,
    second: u32,
    fraction: f64,
}
fn tap(value: f32, start: u32, end: u32) -> Tap {
    let v = (f64::from(value) - 0.5).clamp(f64::from(start), f64::from(end - 1));
    let first = v.floor() as u32;
    Tap {
        first,
        second: first.saturating_add(1).min(end - 1),
        fraction: v - f64::from(first),
    }
}

const SAMPLE_OK: u8 = 0;
const SAMPLE_MISSING: u8 = 1;
const SAMPLE_INCOMPLETE: u8 = 2;
const SAMPLE_OVERFLOW: u8 = 3;
fn sample_error(state: u8) -> &'static str {
    match state {
        SAMPLE_INCOMPLETE => "取色源像素不完整",
        SAMPLE_OVERFLOW => "取色像素索引溢出",
        _ => "取色位置缺少源像素",
    }
}

/// 一行源像素按 `pixel` 的同一优先级合成：后面的平面与光标覆盖前面的，任一覆盖平面
/// 缺字节即为错误，空隙用填充色。保存 `通道 × alpha` 的 f64，与逐像素路径同一乘积。
struct SourceRow {
    y: u32,
    used: u64,
    values: Vec<[f64; 4]>,
    states: Vec<u8>,
    /// 整行都有像素时逐像素不再检查状态。
    complete: bool,
}
impl SourceRow {
    fn fill(&mut self, source: &SourcePixels<'_>, y: u32, left: u32, span: u32) {
        self.y = y;
        let mut pixels = vec![[0_u8; 4]; span as usize];
        self.states.clear();
        self.states.resize(span as usize, SAMPLE_MISSING);
        let end = u64::from(left) + u64::from(span);
        for plane in source.planes.iter().chain(source.overlay.iter()) {
            let r = plane.rect;
            if y < r.y || y - r.y >= r.height {
                continue;
            }
            let from = left.max(r.x);
            let to = end.min(u64::from(r.x) + u64::from(r.width)) as u32;
            for x in from..to.max(from) {
                let i = (x - left) as usize;
                if self.states[i] > SAMPLE_MISSING {
                    continue;
                }
                let index = ((y - r.y) as usize)
                    .checked_mul(r.width as usize)
                    .and_then(|row| row.checked_add((x - r.x) as usize))
                    .and_then(|index| index.checked_mul(4));
                let Some(index) = index.filter(|index| index.checked_add(4).is_some()) else {
                    self.states[i] = SAMPLE_OVERFLOW;
                    continue;
                };
                match plane.bgra.get(index..index + 4) {
                    Some(bytes) => {
                        pixels[i] = [bytes[0], bytes[1], bytes[2], bytes[3]];
                        self.states[i] = SAMPLE_OK;
                    }
                    None => self.states[i] = SAMPLE_INCOMPLETE,
                }
            }
        }
        if let Some(fill) = source.fill {
            for (pixel, state) in pixels.iter_mut().zip(&mut self.states) {
                if *state == SAMPLE_MISSING {
                    *pixel = fill;
                    *state = SAMPLE_OK;
                }
            }
        }
        self.complete = self.states.iter().all(|&state| state == SAMPLE_OK);
        self.values.clear();
        self.values.extend(pixels.iter().map(|p| {
            let alpha = f64::from(p[3]) / 255.;
            [
                f64::from(p[0]) * alpha,
                f64::from(p[1]) * alpha,
                f64::from(p[2]) * alpha,
                f64::from(p[3]),
            ]
        }));
    }
}

/// 非负有限值的 `f64::round`（远离零取整）。`round` 在基线目标上是库调用，每像素四次；
/// 截断为整数后的差值精确（Sterbenz），结果与 `round` 逐位相同。
#[inline]
fn round_unsigned(value: f64) -> f64 {
    if !(0. ..4503599627370496.).contains(&value) {
        return value.round();
    }
    let whole = value as i64 as f64;
    if value - whole >= 0.5 {
        whole + 1.
    } else {
        whole
    }
}

fn sample_band(
    source: &SourcePixels<'_>,
    columns: &[Tap],
    rows: &[Tap],
    left: u32,
    span: u32,
    out: &mut [u8],
) -> std::result::Result<(), &'static str> {
    let mut cache: Vec<SourceRow> = Vec::with_capacity(3);
    let mut clock = 0_u64;
    let row_index = |cache: &mut Vec<SourceRow>, y: u32, clock: u64| -> usize {
        if let Some(index) = cache.iter().position(|row| row.y == y) {
            cache[index].used = clock;
            return index;
        }
        let index = if cache.len() < 3 {
            cache.push(SourceRow {
                y,
                used: clock,
                values: Vec::new(),
                states: Vec::new(),
                complete: false,
            });
            cache.len() - 1
        } else {
            (0..cache.len()).min_by_key(|&i| cache[i].used).unwrap()
        };
        cache[index].used = clock;
        cache[index].fill(source, y, left, span);
        index
    };
    let width = columns.len();
    for (r, row_tap) in rows.iter().enumerate() {
        clock += 2;
        let top = row_index(&mut cache, row_tap.first, clock);
        let bottom = row_index(&mut cache, row_tap.second, clock + 1);
        let (top, bottom) = (&cache[top], &cache[bottom]);
        let dy = row_tap.fraction;
        let line = &mut out[r * width * 4..(r + 1) * width * 4];
        for (column, pixel) in columns.iter().zip(line.chunks_exact_mut(4)) {
            let dx = column.fraction;
            let a = (column.first - left) as usize;
            let b = (column.second - left) as usize;
            let mut sums = [0.; 4];
            for (row, i, weight) in [
                (top, a, (1. - dx) * (1. - dy)),
                (top, b, dx * (1. - dy)),
                (bottom, a, (1. - dx) * dy),
                (bottom, b, dx * dy),
            ] {
                if weight == 0. {
                    continue;
                }
                if !row.complete && row.states[i] != SAMPLE_OK {
                    return Err(sample_error(row.states[i]));
                }
                let p = row.values[i];
                for c in 0..3 {
                    sums[c] += p[c] * weight;
                }
                sums[3] += p[3] * weight;
            }
            let mut result = [0; 4];
            if sums[3] > 0. {
                for c in 0..3 {
                    result[c] = round_unsigned(sums[c] * 255. / sums[3]).clamp(0., 255.) as u8;
                }
                result[3] = round_unsigned(sums[3]).clamp(0., 255.) as u8;
            }
            pixel.copy_from_slice(&result);
        }
    }
    Ok(())
}

/// 整块目标的双线性底图，与逐像素 [`linear_pixel`] 逐字节一致：列/行坐标与权重各算一次，
/// 每个源行只合成一次，大块按行带并行。贴图缩放时每帧都要按新倍率重采样可见底图。
pub(crate) fn linear_background(
    source: &SourcePixels<'_>,
    projection: &OutputProjection,
) -> Result<Image> {
    linear_background_with(source, projection, None)
}
fn linear_background_with(
    source: &SourcePixels<'_>,
    projection: &OutputProjection,
    threads: Option<usize>,
) -> Result<Image> {
    let rect = source.canvas;
    let (width, height) = (projection.width, projection.height);
    if rect.width == 0 || rect.height == 0 || width == 0 || height == 0 {
        // 退化尺寸保持原逐像素行为（含其错误/边界语义）。
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(&linear_pixel(source, projection.source_center(x, y))?);
            }
        }
        return Ok(Image {
            width,
            height,
            bgra,
            readback_ms: 0.,
        });
    }
    let right = rect.x.checked_add(rect.width).ok_or("取色源范围溢出")?;
    let bottom = rect.y.checked_add(rect.height).ok_or("取色源范围溢出")?;
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|v| v.checked_mul(4))
        .ok_or("贴图显示目标栅格溢出")?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(len)?;
    bgra.resize(len, 0);
    let columns: Vec<Tap> = (0..width)
        .map(|x| tap(projection.source_center(x, 0).x, rect.x, right))
        .collect();
    let rows: Vec<Tap> = (0..height)
        .map(|y| tap(projection.source_center(0, y).y, rect.y, bottom))
        .collect();
    let left = columns.iter().map(|t| t.first).min().unwrap_or(rect.x);
    let span = columns.iter().map(|t| t.second).max().unwrap_or(left) - left + 1;
    // 每 256K 目标像素一个线程，最多用一半逻辑核（不超过 16），给 UI 与系统留余量。
    let threads = threads.unwrap_or_else(|| {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        (len / 4 / (1 << 18)).clamp(1, (cores / 2).clamp(1, 16))
    });
    let band = (height as usize).div_ceil(threads);
    let result = if threads == 1 {
        sample_band(source, &columns, &rows, left, span, &mut bgra)
    } else {
        std::thread::scope(|scope| {
            let workers: Vec<_> = bgra
                .chunks_mut(band * width as usize * 4)
                .zip(rows.chunks(band))
                .map(|(out, rows)| {
                    let columns = &columns;
                    scope.spawn(move || sample_band(source, columns, rows, left, span, out))
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap_or(Err("贴图底图采样线程异常")))
                .find(|result| result.is_err())
                .unwrap_or(Ok(()))
        })
    };
    result?;
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

fn premultiply(pixel: &[u8]) -> [u8; 4] {
    let alpha = u32::from(pixel[3]);
    [
        ((u32::from(pixel[0]) * alpha + 127) / 255) as u8,
        ((u32::from(pixel[1]) * alpha + 127) / 255) as u8,
        ((u32::from(pixel[2]) * alpha + 127) / 255) as u8,
        pixel[3],
    ]
}

fn sample_target(
    source: &SourcePixels<'_>,
    viewport: Viewport,
    pointer: Point,
) -> Result<Option<SampleTarget>> {
    if !pointer.x.is_finite()
        || !pointer.y.is_finite()
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
    {
        return Ok(None);
    }
    let projection = snapshot_projection(source, viewport.zoom)?;
    let point = viewport.image_point(pointer);
    let rect = source.canvas;
    if point.x < rect.x as f32
        || point.y < rect.y as f32
        || point.x >= rect.x as f32 + rect.width as f32
        || point.y >= rect.y as f32 + rect.height as f32
    {
        return Ok(None);
    }
    // 保留连续源坐标直到最终快照索引，放大时不能先截成原图整数像素。
    let projected = projection.map(point);
    let x = projected
        .x
        .round_ties_even()
        .clamp(0., (projection.width - 1) as f32) as u32;
    let y = projected
        .y
        .round_ties_even()
        .clamp(0., (projection.height - 1) as f32) as u32;
    Ok(Some(SampleTarget {
        projection,
        index: (x, y),
        source_point: (point.x as u32, point.y as u32),
    }))
}

fn compose_region(
    source: &SourcePixels<'_>,
    scene: Option<&Scene>,
    mut projection: OutputProjection,
    region: Rect,
) -> Result<Image> {
    projection.width = region.width;
    projection.height = region.height;
    projection.origin = Point {
        x: region.x as f32,
        y: region.y as f32,
    };
    let mut image = background(source, &projection)?;
    if let Some(scene) = scene {
        scene.apply_display(&mut image, source, &projection)?;
    }
    Ok(image)
}

#[cfg(test)]
pub(crate) fn sample(
    source: &SourcePixels<'_>,
    scene: Option<&Scene>,
    viewport: Viewport,
    pointer: Point,
) -> Result<Option<ColorSample>> {
    let Some(target) = sample_target(source, viewport, pointer)? else {
        return Ok(None);
    };
    let image = compose_region(
        source,
        scene,
        target.projection,
        Rect {
            x: target.index.0,
            y: target.index.1,
            width: 1,
            height: 1,
        },
    )?;
    Ok(Some(ColorSample {
        bgra: premultiply(&image.bgra),
        source_point: target.source_point,
    }))
}

pub(crate) fn sample_patch(
    source: &SourcePixels<'_>,
    scene: Option<&Scene>,
    viewport: Viewport,
    pointer: Point,
) -> Result<Option<ColorPatch>> {
    let Some(target) = sample_target(source, viewport, pointer)? else {
        return Ok(None);
    };
    let (x, y) = target.index;
    let left = x.saturating_sub(7);
    let top = y.saturating_sub(4);
    let right = x.saturating_add(8).min(target.projection.width);
    let bottom = y.saturating_add(5).min(target.projection.height);
    let center = (x - left, y - top);
    let image = compose_region(
        source,
        scene,
        target.projection,
        Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        },
    )?;
    let at = (center.1 as usize * image.width as usize + center.0 as usize) * 4;
    let sample = ColorSample {
        bgra: premultiply(&image.bgra[at..at + 4]),
        source_point: target.source_point,
    };
    Ok(Some(ColorPatch {
        image,
        center,
        sample,
    }))
}

/// 外观贴图按实际客户区整数栅格取至多 15×9 像素；不另舍入内容尺寸。
/// 几何圆角只乘一次 alpha，不取装饰层、棋盘格或窗口透明度。
pub(crate) fn sample_presentation_patch(
    source: &SourcePixels<'_>,
    scene: Option<&Scene>,
    plan: crate::pin_presentation::PresentationPreviewPlan,
    viewport: Viewport,
    pointer: Point,
) -> Result<Option<ColorPatch>> {
    if !pointer.x.is_finite()
        || !pointer.y.is_finite()
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
    {
        return Ok(None);
    }
    if !viewport.zoom.is_finite() || viewport.zoom <= 0. {
        return Err("取色图像尺寸或缩放无效".into());
    }
    let point = viewport.image_point(pointer);
    let rect = source.canvas;
    if !point.x.is_finite()
        || !point.y.is_finite()
        || point.x < rect.x as f32
        || point.y < rect.y as f32
        || point.x >= rect.x as f32 + rect.width as f32
        || point.y >= rect.y as f32 + rect.height as f32
    {
        return Ok(None);
    }
    let x = f64::from(pointer.x).floor();
    let y = f64::from(pointer.y).floor();
    if x < f64::from(i32::MIN)
        || y < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y > f64::from(i32::MAX)
    {
        return Err("取色客户区像素坐标溢出".into());
    }
    let (x, y) = (x as i64, y as i64);
    let bounds = plan.content_bounds();
    // 与小角预览同一栅格的 floor/ceil；保留负原点和分数留白的目标相位。
    let left_bound = bounds.left.floor() as i64;
    let top_bound = bounds.top.floor() as i64;
    let right_bound = (bounds.left + bounds.width).ceil() as i64;
    let bottom_bound = (bounds.top + bounds.height).ceil() as i64;
    if x < left_bound || y < top_bound || x >= right_bound || y >= bottom_bound {
        return Ok(None);
    }
    let left = (x - 7).max(left_bound);
    let top = (y - 4).max(top_bound);
    let right = (x + 8).min(right_bound);
    let bottom = (y + 5).min(bottom_bound);
    let region = crate::pin_presentation::PresentationRegion {
        left: i32::try_from(left).map_err(|_| "取色邻域横坐标溢出")?,
        top: i32::try_from(top).map_err(|_| "取色邻域纵坐标溢出")?,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    };
    let empty = Scene::default();
    let image = plan.render_content_region(source, scene.unwrap_or(&empty), None, region)?;
    let center = ((x - left) as u32, (y - top) as u32);
    let at = (center.1 as usize * image.width as usize + center.0 as usize) * 4;
    if image.bgra[at + 3] == 0 {
        return Ok(None);
    }
    let sample = ColorSample {
        bgra: premultiply(&image.bgra[at..at + 4]),
        source_point: (point.x as u32, point.y as u32),
    };
    Ok(Some(ColorPatch {
        image,
        center,
        sample,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        annotation::{Annotation, HighlighterBlend, Style, Tool},
        editor::Editor,
        geometry::Rect,
        pin_presentation::{PhysicalPoint, PresentationPreviewPlan, PresentationRegion},
        scene::{Layer, PixelPlane},
        selection_finish::SelectionFinishStyle,
    };

    fn p(x: f32, y: f32) -> Point {
        Point { x, y }
    }
    fn pixels(bytes: &[u8], rect: Rect) -> SourcePixels<'_> {
        SourcePixels {
            fill: None,
            canvas: rect,
            planes: vec![PixelPlane { rect, bgra: bytes }],
            overlay: None,
        }
    }
    fn mark(tool: Tool, start: Point, end: Point, style: Style) -> Annotation {
        let mut item = Annotation::new(tool, start, style);
        item.update(end);
        item
    }
    // 独立公式：client / host * bitmap-size 后中点取偶舍入，再钳制到最后像素。
    // 不调用生产 projection.map，也不提前截断连续的源坐标。
    fn rounded_bitmap_index(rect: Rect, viewport: Viewport, pointer: Point) -> (u32, u32) {
        let zoom = f64::from(viewport.zoom);
        let width = (f64::from(rect.width) * zoom).round_ties_even().max(1.);
        let height = (f64::from(rect.height) * zoom).round_ties_even().max(1.);
        let x = f64::from(pointer.x) - f64::from(viewport.offset.x) - f64::from(rect.x) * zoom;
        let y = f64::from(pointer.y) - f64::from(viewport.offset.y) - f64::from(rect.y) * zoom;
        (
            (x / (f64::from(rect.width) * zoom) * width)
                .round_ties_even()
                .clamp(0., width - 1.) as u32,
            (y / (f64::from(rect.height) * zoom) * height)
                .round_ties_even()
                .clamp(0., height - 1.) as u32,
        )
    }
    fn full_snapshot(source: &SourcePixels<'_>, scene: Option<&Scene>, zoom: f32) -> Image {
        let projection = snapshot_projection(source, zoom).unwrap();
        let mut image = background(source, &projection).unwrap();
        if let Some(scene) = scene {
            scene
                .apply_display(&mut image, source, &projection)
                .unwrap();
        }
        image
    }
    fn expected_pixel(image: &Image, index: (u32, u32)) -> [u8; 4] {
        let at = (index.1 as usize * image.width as usize + index.0 as usize) * 4;
        let pixel = &image.bgra[at..at + 4];
        let alpha = f64::from(pixel[3]) / 255.;
        [
            (f64::from(pixel[0]) * alpha).round() as u8,
            (f64::from(pixel[1]) * alpha).round() as u8,
            (f64::from(pixel[2]) * alpha).round() as u8,
            pixel[3],
        ]
    }
    fn assert_sample_matches(
        source: &SourcePixels<'_>,
        scene: Option<&Scene>,
        viewport: Viewport,
        pointer: Point,
        full: &Image,
    ) {
        let index = rounded_bitmap_index(source.canvas, viewport, pointer);
        let observed = sample(source, scene, viewport, pointer).unwrap().unwrap();
        assert_eq!(
            observed.bgra,
            expected_pixel(full, index),
            "snapshot pixel {index:?}, pointer {pointer:?}, zoom {}",
            viewport.zoom
        );
    }

    #[test]
    fn display_snapshot_one_to_one_keeps_pixel_centres_and_bgra() {
        let bytes = [
            7, 17, 27, 255, 31, 41, 51, 255, 62, 72, 82, 255, 93, 103, 113, 255, 124, 134, 144,
            255, 155, 165, 175, 255,
        ];
        let source = pixels(&bytes, Rect::full(3, 2));
        for y in 0..2 {
            for x in 0..3 {
                let observed = sample(&source, None, Viewport::default(), p(x as f32, y as f32))
                    .unwrap()
                    .unwrap();
                let at = (y * 3 + x) * 4;
                assert_eq!(observed.bgra.as_slice(), &bytes[at..at + 4]);
                assert_eq!(observed.source_point, (x as u32, y as u32));
            }
        }
        assert_eq!(
            sample(&source, None, Viewport::default(), p(0., 0.))
                .unwrap()
                .unwrap()
                .hex(),
            "#1B1107"
        );
    }

    #[test]
    fn display_snapshot_uses_fractional_pointer_letterbox_and_global_tile_phase() {
        let bytes: Vec<u8> = (0..6).flat_map(|i| [i * 31, i * 17, i * 11, 255]).collect();
        let rect = Rect {
            x: 4,
            y: 3,
            width: 3,
            height: 2,
        };
        let source = pixels(&bytes, rect);
        let viewport = Viewport {
            zoom: 2.5,
            offset: p(-40., -60.),
        };
        let pointer = viewport.window_point(p(4.65, 3.35));
        assert!(pointer.x < 0. && pointer.y < 0.);
        assert_eq!(rounded_bitmap_index(rect, viewport, pointer), (2, 1));
        let full = full_snapshot(&source, None, viewport.zoom);
        assert_sample_matches(&source, None, viewport, pointer, &full);
        let mut tile = snapshot_projection(&source, viewport.zoom).unwrap();
        tile.width = 1;
        tile.height = 1;
        tile.origin = p(2., 1.);
        assert!(!tile.identity());
        let mapped = tile.map(tile.source_center(0, 0));
        assert!((mapped.x - 0.5).abs() < 0.00001 && (mapped.y - 0.5).abs() < 0.00001);
        for outside in [p(3.9, 3.5), p(7., 3.5)] {
            assert!(
                sample(&source, None, viewport, viewport.window_point(outside))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(sample(&source, None, viewport, p(f32::NAN, 0.))
            .unwrap()
            .is_none());
    }

    #[test]
    fn display_snapshot_interpolates_premultiplied_alpha_without_hidden_rgb_bleed() {
        let bytes = [255, 0, 0, 0, 0, 0, 200, 255];
        let source = pixels(&bytes, Rect::full(2, 1));
        let viewport = Viewport {
            zoom: 0.5,
            offset: Point { x: 0., y: 0. },
        };
        let observed = sample(&source, None, viewport, p(0., 0.)).unwrap().unwrap();
        assert_eq!(observed.bgra, [0, 0, 100, 128]);
        assert_eq!(observed.hex(), "#640000");
        let translucent = [90, 60, 30, 128];
        let source = pixels(&translucent, Rect::full(1, 1));
        assert_eq!(
            sample(&source, None, Viewport::default(), p(0., 0.))
                .unwrap()
                .unwrap()
                .bgra,
            [45, 30, 15, 128]
        );
        let invisible = [255, 129, 63, 0];
        let source = pixels(&invisible, Rect::full(1, 1));
        assert_eq!(
            sample(&source, None, Viewport::default(), p(0., 0.))
                .unwrap()
                .unwrap()
                .bgra,
            [0; 4]
        );
    }

    #[test]
    fn display_snapshot_scene_layers_match_full_frame_across_zoom_and_keep_history() {
        let bytes: Vec<u8> = (0..16 * 12)
            .flat_map(|i| {
                [
                    (i % 16 * 11) as u8,
                    (i / 16 * 17) as u8,
                    (i % 7 * 29) as u8,
                    255,
                ]
            })
            .collect();
        let source = pixels(&bytes, Rect::full(16, 12));
        let mut editor = Editor::new(
            source.canvas,
            Some(Rect {
                x: 9,
                y: 7,
                width: 4,
                height: 3,
            }),
        );
        for item in [
            mark(
                Tool::Rectangle,
                p(1., 1.),
                p(14., 10.),
                Style {
                    filled: true,
                    color: 0xb04217,
                    ..Default::default()
                },
            ),
            mark(
                Tool::Highlighter,
                p(2., 3.),
                p(13., 8.),
                Style {
                    color: 0x71d133,
                    width: 4.,
                    opacity: 0.7,
                    highlighter_blend: HighlighterBlend::Multiply,
                    ..Default::default()
                },
            ),
            mark(
                Tool::Mosaic,
                p(2., 2.),
                p(12., 9.),
                Style {
                    block_size: 3,
                    ..Default::default()
                },
            ),
            mark(
                Tool::Blur,
                p(4., 3.),
                p(14., 10.),
                Style {
                    block_size: 3,
                    ..Default::default()
                },
            ),
            mark(
                Tool::PixelEraser,
                p(3., 8.),
                p(11., 4.),
                Style {
                    width: 2.,
                    pencil_smooth: false,
                    ..Default::default()
                },
            ),
            mark(
                Tool::Pen,
                p(1., 10.),
                p(14., 2.),
                Style {
                    color: 0x3149d7,
                    width: 1.5,
                    opacity: 0.6,
                    pencil_smooth: false,
                    ..Default::default()
                },
            ),
        ] {
            editor.document.insert(item);
        }
        let undone = mark(Tool::Rectangle, p(0., 0.), p(2., 2.), Style::default());
        editor.document.insert(undone.clone());
        editor.document.undo();
        let before = editor.document.items().to_vec();
        let revision = editor.document.edit_revision();
        let crop = editor.crop;
        let annotations = editor.document_annotations();
        let scene = Scene::prepare(&annotations, &source);
        assert!(scene.errors.is_empty());
        assert!(matches!(scene.layers[1], Layer::Highlighter { .. }));
        assert!(matches!(scene.layers[2], Layer::Mosaic { .. }));
        assert!(matches!(scene.layers[3], Layer::Raster { .. }));
        assert!(matches!(scene.layers[4], Layer::Restore { .. }));
        let below = Scene::prepare(&annotations[..2], &source);
        for zoom in [0.75, 1., 1.5, 2.25] {
            let viewport = Viewport {
                zoom,
                offset: p(3.25, -7.5),
            };
            let full = full_snapshot(&source, Some(&scene), zoom);
            assert_ne!(full.bgra, full_snapshot(&source, None, zoom).bgra);
            assert_ne!(full.bgra, full_snapshot(&source, Some(&below), zoom).bgra);
            for point in [
                p(1.25, 1.25),
                p(3.4, 7.1),
                p(7.75, 4.25),
                p(12.1, 8.3),
                p(14.5, 10.25),
            ] {
                assert_sample_matches(
                    &source,
                    Some(&scene),
                    viewport,
                    viewport.window_point(point),
                    &full,
                );
            }
        }
        assert_eq!(editor.document.items(), before);
        assert_eq!(editor.document.edit_revision(), revision);
        assert_eq!(editor.crop, crop);
        editor.document.redo();
        assert_eq!(editor.document.items().last().unwrap().annotation, undone);
    }

    #[test]
    fn display_snapshot_text_and_watermark_tiles_keep_global_glyph_phase() {
        let bytes = [37, 53, 71, 255].repeat(240 * 120);
        let source = pixels(&bytes, Rect::full(240, 120));
        let mut text = mark(
            Tool::Text,
            p(18.25, 12.75),
            p(205.25, 77.75),
            Style {
                color: 0xc14372,
                font_size: 19.,
                opacity: 0.7,
                ..Default::default()
            },
        );
        text.text = "中文 phase\nTile 123".into();
        text.rotation = -13.;
        let mut watermark = mark(
            Tool::Watermark,
            p(3.25, 2.75),
            p(235.5, 117.25),
            Style {
                color: 0x2169b5,
                font_size: 18.,
                opacity: 0.5,
                ..Default::default()
            },
        );
        watermark.text = "W phase".into();
        watermark.rotation = 17.;
        let text_scene = Scene::prepare(std::slice::from_ref(&text), &source);
        let scene = Scene::prepare(&[text, watermark], &source);
        assert!(scene.errors.is_empty());
        assert!(matches!(scene.layers[1], Layer::Watermark { .. }));
        // 每档取文字、水印及合成中间三个墨迹点，分别证明两层实际被采到。
        for zoom in [0.65, 1., 1.5] {
            let viewport = Viewport {
                zoom,
                offset: p(-91.25, 44.5),
            };
            let full = full_snapshot(&source, Some(&scene), zoom);
            let raw = full_snapshot(&source, None, zoom);
            let text_full = full_snapshot(&source, Some(&text_scene), zoom);
            let text_ink = text_full
                .bgra
                .chunks_exact(4)
                .zip(raw.bgra.chunks_exact(4))
                .position(|(a, b)| a != b)
                .expect("文字层必须有实际墨迹");
            let watermark_ink = full
                .bgra
                .chunks_exact(4)
                .zip(text_full.bgra.chunks_exact(4))
                .position(|(a, b)| a != b)
                .expect("水印层必须独立改变内容像素");
            let changed: Vec<usize> = full
                .bgra
                .chunks_exact(4)
                .zip(raw.bgra.chunks_exact(4))
                .enumerate()
                .filter_map(|(i, (a, b))| (a != b).then_some(i))
                .collect();
            assert!(!changed.is_empty(), "文字/水印夹具不能只覆盖空白像素");
            for i in [text_ink, changed[changed.len() / 2], watermark_ink] {
                let x = i as u32 % full.width;
                let y = i as u32 / full.width;
                let point = p(
                    (x as f32 + 0.125) * source.canvas.width as f32 / full.width as f32,
                    (y as f32 + 0.125) * source.canvas.height as f32 / full.height as f32,
                );
                let pointer = viewport.window_point(point);
                assert_eq!(rounded_bitmap_index(source.canvas, viewport, pointer), (x, y));
                assert_sample_matches(&source, Some(&scene), viewport, pointer, &full);
            }
        }
    }

    #[test]
    fn display_snapshot_missing_pixels_and_scene_cached_errors_never_return_a_color() {
        let bytes = [19, 29, 39, 255].repeat(4);
        let source = pixels(&bytes, Rect::full(2, 2));
        let missing = SourcePixels {
            fill: None,
            canvas: source.canvas,
            planes: Vec::new(),
            overlay: None,
        };
        assert!(sample(&missing, None, Viewport::default(), p(0., 0.)).is_err());
        let truncated = pixels(&bytes[..4], source.canvas);
        assert!(sample(&truncated, None, Viewport::default(), p(1., 1.)).is_err());
        assert!(sample(
            &source,
            None,
            Viewport {
                zoom: 0.,
                offset: Point { x: 0., y: 0. }
            },
            p(0., 0.)
        )
        .is_err());
        let mut failed = Scene::default();
        failed.errors.push("合成失败".into());
        assert!(sample(&source, Some(&failed), Viewport::default(), p(0., 0.)).is_err());
        let mut invalid = mark(
            Tool::Watermark,
            p(0., 0.),
            p(2., 2.),
            Style {
                font_size: f32::NAN,
                ..Default::default()
            },
        );
        invalid.text = "bad".into();
        let mut cached = Scene::prepare(&[invalid], &source);
        assert!(!cached.errors.is_empty());
        cached.errors.clear();
        assert!(sample(&source, Some(&cached), Viewport::default(), p(1., 1.)).is_err());
        assert_eq!(source.planes[0].bgra, bytes);
    }

    fn assert_patch_matches_full(patch: &ColorPatch, full: &Image, index: (u32, u32)) {
        let left = index.0.saturating_sub(7);
        let top = index.1.saturating_sub(4);
        let right = index.0.saturating_add(8).min(full.width);
        let bottom = index.1.saturating_add(5).min(full.height);
        assert_eq!(
            (patch.image.width, patch.image.height),
            (right - left, bottom - top)
        );
        assert_eq!(patch.center, (index.0 - left, index.1 - top));
        assert!(patch.image.width <= 15 && patch.image.height <= 9);
        for y in 0..patch.image.height {
            let observed = (y as usize * patch.image.width as usize) * 4;
            let expected = ((top + y) as usize * full.width as usize + left as usize) * 4;
            let row_bytes = patch.image.width as usize * 4;
            assert_eq!(
                &patch.image.bgra[observed..observed + row_bytes],
                &full.bgra[expected..expected + row_bytes],
                "full-grid {index:?}, clipped patch row {y}"
            );
        }
        assert_eq!(patch.sample.bgra, expected_pixel(full, index));
    }

    #[test]
    fn display_patch_clips_corners_and_centres_without_changing_selected_sample() {
        let rect = Rect {
            x: 4,
            y: 3,
            width: 23,
            height: 17,
        };
        let bytes: Vec<u8> = (0..rect.width * rect.height)
            .flat_map(|i| {
                [
                    (i * 13) as u8,
                    (i * 7) as u8,
                    (i * 3) as u8,
                    [0, 96, 128, 255][i as usize % 4],
                ]
            })
            .collect();
        let source = pixels(&bytes, rect);
        for zoom in [0.5, 1., 1.75] {
            let viewport = Viewport {
                zoom,
                offset: p(-91.25, -117.5),
            };
            let full = full_snapshot(&source, None, zoom);
            for index in [
                (0, 0),
                (full.width - 1, 0),
                (0, full.height - 1),
                (full.width - 1, full.height - 1),
                (full.width / 2, full.height / 2),
            ] {
                let point = p(
                    rect.x as f32
                        + (index.0 as f32 + 0.125) * rect.width as f32 / full.width as f32,
                    rect.y as f32
                        + (index.1 as f32 + 0.125) * rect.height as f32 / full.height as f32,
                );
                let pointer = viewport.window_point(point);
                assert_eq!(rounded_bitmap_index(rect, viewport, pointer), index);
                let patch = sample_patch(&source, None, viewport, pointer)
                    .unwrap()
                    .unwrap();
                assert_patch_matches_full(&patch, &full, index);
                assert_eq!(
                    patch.sample,
                    sample(&source, None, viewport, pointer).unwrap().unwrap()
                );
                if index == (0, 0) {
                    assert_eq!(patch.center, (0, 0));
                }
                if index == (full.width - 1, full.height - 1) {
                    assert_eq!(
                        patch.center,
                        (patch.image.width - 1, patch.image.height - 1)
                    );
                }
            }
        }
    }

    #[test]
    fn display_patch_text_watermark_match_full_region_and_alpha_is_premultiplied_once() {
        let bytes = [90, 60, 30, 128].repeat(160 * 90);
        let source = pixels(&bytes, Rect::full(160, 90));
        let mut text = mark(
            Tool::Text,
            p(14.25, 11.75),
            p(132.5, 55.25),
            Style {
                color: 0xc14372,
                font_size: 19.,
                opacity: 0.7,
                ..Default::default()
            },
        );
        text.text = "中文 phase\nTile 123".into();
        text.rotation = -11.;
        let mut watermark = mark(
            Tool::Watermark,
            p(3.5, 2.75),
            p(156.5, 88.75),
            Style {
                color: 0x2169b5,
                font_size: 17.,
                opacity: 0.45,
                ..Default::default()
            },
        );
        watermark.text = "W phase".into();
        watermark.rotation = 14.;
        let text_scene = Scene::prepare(std::slice::from_ref(&text), &source);
        let scene = Scene::prepare(&[text, watermark], &source);
        assert!(scene.errors.is_empty());
        for zoom in [0.75, 1.5] {
            let viewport = Viewport {
                zoom,
                offset: p(-91.25, 44.5),
            };
            let full = full_snapshot(&source, Some(&scene), zoom);
            let raw = full_snapshot(&source, None, zoom);
            let text_full = full_snapshot(&source, Some(&text_scene), zoom);
            let text_ink = text_full
                .bgra
                .chunks_exact(4)
                .zip(raw.bgra.chunks_exact(4))
                .position(|(a, b)| a != b)
                .expect("文字层必须有实际墨迹");
            let watermark_ink = full
                .bgra
                .chunks_exact(4)
                .zip(text_full.bgra.chunks_exact(4))
                .position(|(a, b)| a != b)
                .expect("水印层必须独立改变内容像素");
            for i in [text_ink, watermark_ink] {
                let index = (i as u32 % full.width, i as u32 / full.width);
                let point = p(
                    (index.0 as f32 + 0.125) * source.canvas.width as f32 / full.width as f32,
                    (index.1 as f32 + 0.125) * source.canvas.height as f32 / full.height as f32,
                );
                let pointer = viewport.window_point(point);
                assert_eq!(rounded_bitmap_index(source.canvas, viewport, pointer), index);
                let patch = sample_patch(&source, Some(&scene), viewport, pointer)
                    .unwrap()
                    .unwrap();
                assert_patch_matches_full(&patch, &full, index);
                let at = (patch.center.1 as usize * patch.image.width as usize
                    + patch.center.0 as usize)
                    * 4;
                assert_eq!(
                    patch.sample.bgra,
                    premultiply(&patch.image.bgra[at..at + 4])
                );
            }
        }

        let transparent = [255, 0, 0, 0, 0, 0, 200, 255];
        let source = pixels(&transparent, Rect::full(2, 1));
        let patch = sample_patch(
            &source,
            None,
            Viewport {
                zoom: 0.5,
                offset: p(-12., -25.),
            },
            p(-12., -25.),
        )
        .unwrap()
        .unwrap();
        assert_eq!((patch.image.width, patch.image.height), (1, 1));
        assert_eq!(patch.center, (0, 0));
        assert_eq!(patch.image.bgra, [0, 0, 200, 128]);
        assert_eq!(patch.sample.bgra, [0, 0, 100, 128]);
    }

    #[test]
    fn display_patch_missing_neighbour_or_scene_error_never_returns_partial_content() {
        let bytes = [19, 29, 39, 255].repeat(16);
        let source = pixels(&bytes, Rect::full(4, 4));
        let truncated = pixels(&bytes[..4], source.canvas);
        // 单点仍完整，但邻域后续缺像素：不能返回已经填出的第一格。
        assert!(sample(&truncated, None, Viewport::default(), p(0., 0.))
            .unwrap()
            .is_some());
        assert!(sample_patch(&truncated, None, Viewport::default(), p(0., 0.)).is_err());
        let missing = SourcePixels {
            fill: None,
            canvas: source.canvas,
            planes: Vec::new(),
            overlay: None,
        };
        assert!(sample_patch(&missing, None, Viewport::default(), p(0., 0.)).is_err());
        let mut failed = Scene::default();
        failed.errors.push("合成失败".into());
        assert!(sample_patch(&source, Some(&failed), Viewport::default(), p(0., 0.)).is_err());
        let mut invalid = mark(
            Tool::Watermark,
            p(0., 0.),
            p(4., 4.),
            Style {
                font_size: f32::NAN,
                ..Default::default()
            },
        );
        invalid.text = "bad".into();
        let mut cached = Scene::prepare(&[invalid], &source);
        assert!(!cached.errors.is_empty());
        cached.errors.clear();
        assert!(sample_patch(&source, Some(&cached), Viewport::default(), p(1., 1.)).is_err());
        assert!(
            sample_patch(&source, None, Viewport::default(), p(f32::NAN, 0.))
                .unwrap()
                .is_none()
        );
        assert!(sample_patch(&source, None, Viewport::default(), p(4., 0.))
            .unwrap()
            .is_none());
        assert!(sample_patch(
            &source,
            None,
            Viewport {
                zoom: 0.,
                offset: p(0., 0.)
            },
            p(0., 0.)
        )
        .is_err());
        assert_eq!(source.planes[0].bgra, bytes);
    }

    #[test]
    fn presentation_patch_fractional_client_grid_matches_full_scene_and_linear_gradient() {
        let rect = Rect::full(31, 25);
        let bytes: Vec<u8> = (0..rect.height)
            .flat_map(|y| {
                (0..rect.width).flat_map(move |x| {
                    [
                        (x * 7) as u8,
                        (y * 9) as u8,
                        ((x + y) * 3) as u8,
                        (96 + (x * 3 + y * 5) % 128) as u8,
                    ]
                })
            })
            .collect();
        let source = pixels(&bytes, rect);
        let style = SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 1.,
            corner_enabled: true,
            corner_radius: 5.25,
            ..Default::default()
        };
        // 33×27 outer 放入 41×34，内容尺寸不再单独 round 为 39×31。
        let viewport = crate::pin_presentation_host::content_viewport(
            (rect.width, rect.height),
            (41, 34),
            Some(style),
        );
        let plan = PresentationPreviewPlan::new(
            rect,
            style,
            PhysicalPoint {
                x: f64::from(viewport.offset.x),
                y: f64::from(viewport.offset.y),
            },
            f64::from(viewport.zoom),
        )
        .unwrap();
        let scene = Scene::prepare(
            &[mark(
                Tool::Pen,
                p(8., 9.75),
                p(21., 9.75),
                Style {
                    width: 1.25,
                    color: 0x00cc66,
                    opacity: 0.55,
                    pencil_smooth: false,
                    ..Default::default()
                },
            )],
            &source,
        );
        assert!(scene.errors.is_empty());
        let pointer = p(17.8, 16.4);
        let patch = sample_presentation_patch(&source, Some(&scene), plan, viewport, pointer)
            .unwrap()
            .unwrap();
        assert_eq!((patch.image.width, patch.image.height), (15, 9));
        assert_eq!(patch.center, (7, 4));
        let point = viewport.image_point(pointer);
        assert_eq!(patch.sample.source_point, (point.x as u32, point.y as u32));

        let bounds = plan.content_bounds();
        let whole = PresentationRegion {
            left: bounds.left.floor() as i32,
            top: bounds.top.floor() as i32,
            width: ((bounds.left + bounds.width).ceil() - bounds.left.floor()) as u32,
            height: ((bounds.top + bounds.height).ceil() - bounds.top.floor()) as u32,
        };
        let full = plan
            .render_content_region(&source, &scene, None, whole)
            .unwrap();
        let empty = Scene::default();
        let raw = plan
            .render_content_region(&source, &empty, None, whole)
            .unwrap();
        let left = pointer.x.floor() as i32 - 7 - whole.left;
        let top = pointer.y.floor() as i32 - 4 - whole.top;
        let mut scene_changed_patch = false;
        for y in 0..patch.image.height {
            let observed = y as usize * patch.image.width as usize * 4;
            let expected = ((top as u32 + y) as usize * full.width as usize + left as usize) * 4;
            let length = patch.image.width as usize * 4;
            assert_eq!(
                &patch.image.bgra[observed..observed + length],
                &full.bgra[expected..expected + length],
                "绝对客户区分数相位必须保留，ROI row {y}"
            );
            scene_changed_patch |=
                full.bgra[expected..expected + length] != raw.bgra[expected..expected + length];
        }
        assert!(scene_changed_patch, "小邻域必须实际覆盖 Scene 墨迹");
        // 中心远离笔迹与圆角，独立核验 floor 到 (17,16) 的目标像素中心。
        let center_source = viewport.image_point(p(17.5, 16.5));
        let linear = linear_pixel(&source, center_source).unwrap();
        assert_eq!(patch.sample.bgra, premultiply(&linear));
        let nearest = pixel(&source, center_source.x as u32, center_source.y as u32).unwrap();
        assert_ne!(linear, nearest, "渐变夹具必须区分线性与 nearest 采样");
        let legacy = sample_patch(&source, Some(&scene), viewport, pointer)
            .unwrap()
            .unwrap();
        assert_ne!(
            patch.sample.bgra, legacy.sample.bgra,
            "分数留白和外框缩放不能退回独立舍入内容的旧相位"
        );
        assert_eq!(source.planes[0].bgra, bytes);
    }

    #[test]
    fn presentation_patch_rejects_transparent_corner_and_premultiplies_partial_alpha_once() {
        let rect = Rect::full(31, 25);
        let bytes = [23, 45, 67, 128].repeat((rect.width * rect.height) as usize);
        let source = pixels(&bytes, rect);
        let viewport = Viewport {
            zoom: 1.375,
            offset: p(3.25, 5.75),
        };
        let plan = PresentationPreviewPlan::new(
            rect,
            SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 5.25,
                ..Default::default()
            },
            PhysicalPoint { x: 3.25, y: 5.75 },
            f64::from(viewport.zoom),
        )
        .unwrap();
        assert!(sample_presentation_patch(
            &source,
            None,
            plan,
            viewport,
            viewport.window_point(p(1., 1.)),
        )
        .unwrap()
        .is_none());
        let physical = viewport.window_point(p(1.5, 1.5));
        for dpi in [1., 1.25, 1.5, 2.] {
            let pointer = p(physical.x / dpi * dpi, physical.y / dpi * dpi);
            let patch = sample_presentation_patch(&source, None, plan, viewport, pointer)
                .unwrap()
                .unwrap();
            assert_eq!(patch.sample.source_point, (1, 1));
            assert!(patch.image.width <= 15 && patch.image.height <= 9);
            let at = (patch.center.1 as usize * patch.image.width as usize
                + patch.center.0 as usize)
                * 4;
            let pixel = &patch.image.bgra[at..at + 4];
            assert_eq!(&pixel[..3], &[23, 45, 67]);
            assert!(pixel[3] > 0 && pixel[3] < 128, "圆角覆盖率只乘一次 alpha");
            assert_eq!(patch.sample.bgra, premultiply(pixel));
        }
        for outside in [p(-0.1, 5.), p(31., 10.)] {
            assert!(sample_presentation_patch(
                &source,
                None,
                plan,
                viewport,
                viewport.window_point(outside),
            )
            .unwrap()
            .is_none());
        }
        assert!(
            sample_presentation_patch(&source, None, plan, viewport, p(f32::NAN, 0.))
                .unwrap()
                .is_none()
        );
        assert_eq!(source.planes[0].bgra, bytes);
    }

    fn patterned(width: u32, height: u32, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2654435761).max(1);
        (0..width * height * 4)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                // 每 4 字节一组的 alpha 覆盖 0、1、中间值与 255。
                if i % 4 == 3 {
                    [0, 1, 97, 254, 255][(state % 5) as usize]
                } else {
                    state as u8
                }
            })
            .collect()
    }

    #[test]
    fn linear_background_matches_per_pixel_sampling_byte_for_byte() {
        let left = patterned(9, 13, 1);
        let right = patterned(8, 13, 2);
        let lower = patterned(17, 5, 3);
        let cursor = patterned(4, 3, 4);
        let canvas = Rect {
            x: 2,
            y: 1,
            width: 17,
            height: 13,
        };
        let planes = vec![
            PixelPlane {
                rect: Rect {
                    x: 2,
                    y: 1,
                    width: 9,
                    height: 13,
                },
                bgra: &left,
            },
            PixelPlane {
                rect: Rect {
                    x: 11,
                    y: 1,
                    width: 8,
                    height: 13,
                },
                bgra: &right,
            },
            PixelPlane {
                rect: Rect {
                    x: 2,
                    y: 9,
                    width: 17,
                    height: 5,
                },
                bgra: &lower,
            },
        ];
        let overlay = PixelPlane {
            rect: Rect {
                x: 7,
                y: 4,
                width: 4,
                height: 3,
            },
            bgra: &cursor,
        };
        let source = SourcePixels {
            fill: None,
            canvas,
            planes,
            overlay: Some(overlay),
        };
        for (sx, sy, origin) in [
            (1., 1., Point { x: 0.25, y: -0.5 }),
            (1.5, 1.5, Point { x: 3.3, y: 2.7 }),
            (0.37, 0.61, Point { x: -1.25, y: 0. }),
            (4.25, 2., Point { x: -6., y: 5.5 }),
            (0.999, 1.001, Point { x: 0., y: 0. }),
        ] {
            let projection = OutputProjection {
                source: canvas,
                width: (canvas.width as f32 * sx).ceil() as u32 + 3,
                height: (canvas.height as f32 * sy).ceil() as u32 + 2,
                sx,
                sy,
                origin,
            };
            let fast = linear_background(&source, &projection).unwrap();
            for y in 0..projection.height {
                for x in 0..projection.width {
                    let i = ((y * projection.width + x) * 4) as usize;
                    assert_eq!(
                        fast.bgra[i..i + 4],
                        linear_pixel(&source, projection.source_center(x, y)).unwrap(),
                        "缩放 {sx}×{sy} 目标像素 ({x}, {y})"
                    );
                }
            }
        }
        // 缺像素且无填充时与逐像素路径同样报错；补填充色后一致。
        let mut gap = SourcePixels {
            fill: None,
            canvas: Rect::full(6, 4),
            planes: vec![PixelPlane {
                rect: Rect::full(3, 4),
                bgra: &left[..3 * 4 * 4],
            }],
            overlay: None,
        };
        let projection = OutputProjection {
            source: gap.canvas,
            width: 9,
            height: 6,
            sx: 1.5,
            sy: 1.5,
            origin: Point { x: 0., y: 0. },
        };
        assert!(linear_background(&gap, &projection).is_err());
        gap.fill = Some([9, 8, 7, 128]);
        let filled = linear_background(&gap, &projection).unwrap();
        let mut expected = Vec::new();
        for y in 0..6 {
            for x in 0..9 {
                expected.extend(linear_pixel(&gap, projection.source_center(x, y)).unwrap());
            }
        }
        assert_eq!(filled.bgra, expected);
    }

    #[test]
    #[ignore = "按需比较缩放底图整块采样与逐像素采样成本"]
    fn linear_background_performance() {
        use std::{hint::black_box, time::Instant};
        let raw = patterned(1920, 1080, 7);
        let canvas = Rect::full(1920, 1080);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &raw,
            }],
            overlay: None,
        };
        for zoom in [0.75_f32, 1.25, 1.5, 2.] {
            let projection = OutputProjection {
                source: canvas,
                width: (1920. * zoom) as u32,
                height: (1080. * zoom) as u32,
                sx: zoom,
                sy: zoom,
                origin: Point { x: 0.5, y: 0.5 },
            };
            let start = Instant::now();
            let single = black_box(linear_background_with(&source, &projection, Some(1)).unwrap());
            let single_time = start.elapsed();
            let start = Instant::now();
            let fast = black_box(linear_background(&source, &projection).unwrap());
            let fast_time = start.elapsed();
            assert_eq!(single.bgra, fast.bgra);
            let start = Instant::now();
            let mut slow = Vec::with_capacity(fast.bgra.len());
            for y in 0..projection.height {
                for x in 0..projection.width {
                    slow.extend(linear_pixel(&source, projection.source_center(x, y)).unwrap());
                }
            }
            let slow_time = black_box(start.elapsed());
            assert_eq!(fast.bgra, slow);
            eprintln!(
                "{}x{} zoom {zoom}: block={:.1}ms single_thread={:.1}ms per_pixel={:.1}ms",
                projection.width,
                projection.height,
                fast_time.as_secs_f64() * 1000.,
                single_time.as_secs_f64() * 1000.,
                slow_time.as_secs_f64() * 1000.
            );
        }
    }
}
