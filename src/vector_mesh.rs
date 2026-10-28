//! 矢量覆盖网格：描边/填充先做几何并集，透明对象在交叉处也只合成一次。
//! 模型保留曲线，网格不包含图像像素；显示比例变化时边缘按设备像素重新抗锯齿。
use crate::{
    annotation::{Point, Style},
    vector_path::{Span, VectorPath},
    Result,
};
use lyon_path::{math::point, Path};
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexBuffers,
};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
#[derive(Default)]
pub struct VectorMesh {
    pub vertices: Vec<Point>,
    pub indices: Vec<u32>,
    pub edges: Vec<(u32, u32)>,
    pub normals: Vec<Point>,
    /// 显示网格未做三角形并集时的来源；CPU 覆盖率首次需要时才按它并集。
    exact: Option<Arc<ExactSource>>,
}
/// 并集网格的来源与惰性结果。只有放大导出、贴图取色等 CPU 覆盖率读取才会计算。
struct ExactSource {
    path: VectorPath,
    style: Style,
    dashed_arrow: bool,
    mesh: OnceLock<std::result::Result<VectorMesh, String>>,
}
/// 草稿分块细分的上一帧结果：每块记录结束跨度下标与块网格。
#[derive(Default)]
pub struct DraftPieces {
    style: Option<Style>,
    spans: Vec<Span>,
    pieces: Vec<(usize, Arc<VectorMesh>)>,
}
/// 尾块最多重新细分的跨度数；足够小让长笔迹每帧成本不随点数增长。
const DRAFT_PIECE_SPANS: usize = 32;
/// 描边曲线与圆头／圆角连接的细分容差（全屏物理像素）。与 `VectorPath::flattened`
/// 的 0.03 px 一致，铅笔拟合已为它预留误差预算；更细只会成倍增加顶点而看不出差别。
const STROKE_TOLERANCE: f32 = 0.03;
fn signed_area(a: Point, b: Point, c: Point) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}
fn point_from(p: lyon_path::math::Point) -> Point {
    Point { x: p.x, y: p.y }
}
fn polygon(points: &[Point]) -> Path {
    let mut builder = Path::builder();
    if let Some(first) = points.first() {
        builder.begin(point(first.x, first.y));
        for p in &points[1..] {
            builder.line_to(point(p.x, p.y));
        }
        builder.end(true);
    }
    builder.build()
}
impl VectorMesh {
    /// 一次填充所有同绕向轮廓，重叠区域只覆盖一次；不隐式增加描边。
    pub fn fill_contours(contours: &[Vec<Point>]) -> Result<Self> {
        let mut builder = Path::builder();
        for contour in contours {
            if contour.len() < 3 {
                continue;
            }
            builder.begin(point(contour[0].x, contour[0].y));
            for p in &contour[1..] {
                builder.line_to(point(p.x, p.y));
            }
            builder.end(true);
        }
        let mut mesh = VertexBuffers::<Point, u32>::new();
        FillTessellator::new()
            .tessellate_path(
                &builder.build(),
                &FillOptions::default()
                    .with_fill_rule(FillRule::NonZero)
                    .with_tolerance(0.01),
                &mut BuffersBuilder::new(&mut mesh, |v: FillVertex| point_from(v.position())),
            )
            .map_err(|e| e.to_string())?;
        Ok(Self::from_buffers(mesh))
    }

    /// 已经消去内部交线的闭合边界，保留外环/内环绕向。
    pub fn boundary_contours(&self) -> Result<Vec<Vec<Point>>> {
        let mut outgoing = HashMap::<u32, Vec<u32>>::new();
        for &(a, b) in &self.edges {
            outgoing.entry(a).or_default().push(b);
        }
        let mut contours = Vec::new();
        for &(start, _) in &self.edges {
            while outgoing.get(&start).is_some_and(|v| !v.is_empty()) {
                let mut contour = Vec::new();
                let mut current = start;
                loop {
                    contour.push(self.vertices[current as usize]);
                    current = outgoing
                        .get_mut(&current)
                        .and_then(Vec::pop)
                        .ok_or("矢量边界未闭合")?;
                    if current == start {
                        break;
                    }
                    if contour.len() > self.edges.len() {
                        return Err("矢量边界连接异常".into());
                    }
                }
                if contour.len() >= 3 {
                    contours.push(contour);
                }
            }
        }
        Ok(contours)
    }

    pub fn build(path: &VectorPath, style: Style, dashed_arrow: bool) -> Result<Self> {
        Self::build_with(path, style, dashed_arrow, true, STROKE_TOLERANCE)
    }
    /// 画布显示用网格：跳过 CPU 三角形并集。GPU `coverage_union` 以 MAX 合并覆盖率，
    /// 自交、折叠处不加深，与草稿预览同一画法；并集的全量非零重细分对一笔真实手绘要
    /// 40–80 ms，提交时不再付出。需要精确外边界的 CPU 覆盖率经 `exact` 惰性并集。
    pub fn build_display(path: &VectorPath, style: Style, dashed_arrow: bool) -> Result<Self> {
        let mut mesh = Self::build_with(path, style, dashed_arrow, false, STROKE_TOLERANCE)?;
        if !mesh.indices.is_empty() {
            mesh.exact = Some(Arc::new(ExactSource {
                path: path.clone(),
                style,
                dashed_arrow,
                mesh: OnceLock::new(),
            }));
        }
        Ok(mesh)
    }
    /// 平移后的显示网格；并集来源换成平移后的路径，不沿用旧位置的并集结果。
    pub(crate) fn translated(&self, dx: f32, dy: f32, path: &VectorPath) -> Self {
        Self {
            vertices: self
                .vertices
                .iter()
                .map(|p| Point {
                    x: p.x + dx,
                    y: p.y + dy,
                })
                .collect(),
            indices: self.indices.clone(),
            edges: self.edges.clone(),
            normals: self.normals.clone(),
            exact: self.exact.as_ref().map(|source| {
                Arc::new(ExactSource {
                    path: path.clone(),
                    style: source.style,
                    dashed_arrow: source.dashed_arrow,
                    mesh: OnceLock::new(),
                })
            }),
        }
    }
    /// CPU 精确覆盖率（外边界扫描）用的并集网格；本身已是并集时返回自身。
    pub(crate) fn exact(&self) -> Result<&Self> {
        let Some(source) = &self.exact else {
            return Ok(self);
        };
        source
            .mesh
            .get_or_init(|| {
                Self::build(&source.path, source.style, source.dashed_arrow)
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| e.clone().into())
    }
    /// 正在拖动的笔迹草稿：跳过 CPU 三角形并集，直接用 lyon 的共享顶点网格。
    /// 预览由 GPU `coverage_union` 以 MAX 合并覆盖率，自交处不加深；并集的全量
    /// 非零重细分随点数增长，逐帧重算会让长笔迹明显卡顿。提交后由 Scene 以 `build` 重建。
    pub fn build_draft(path: &VectorPath, style: Style, dashed_arrow: bool) -> Result<Self> {
        Self::build_with(path, style, dashed_arrow, false, STROKE_TOLERANCE)
    }
    /// 增长中的草稿按固定跨度数分块细分，跨度前缀未变的块沿用上一帧，每帧只细分尾部。
    /// 每块各自圆头：接缝两侧圆头的并集正是圆角连接覆盖的圆盘，GPU 按 MAX 合并覆盖率，
    /// 接缝不留缝也不加深。虚线相位、箭头与填充轮廓依赖整条路径，仍整笔细分。
    pub fn build_draft_pieces(
        path: &VectorPath,
        style: Style,
        dashed_arrow: bool,
        previous: Option<&DraftPieces>,
    ) -> Result<(Self, DraftPieces)> {
        if style.dashed || dashed_arrow || !path.fills.is_empty() {
            return Ok((
                Self::build_draft(path, style, dashed_arrow)?,
                DraftPieces::default(),
            ));
        }
        let mut pieces = Vec::new();
        if let Some(previous) = previous.filter(|p| p.style == Some(style)) {
            let common = previous
                .spans
                .iter()
                .zip(&path.spans)
                .take_while(|(a, b)| a == b)
                .count();
            pieces.extend(
                previous
                    .pieces
                    .iter()
                    .take_while(|(end, _)| *end <= common)
                    .cloned(),
            );
        }
        let mut start = pieces.last().map_or(0, |(end, _)| *end);
        while start < path.spans.len() {
            let end = (start + DRAFT_PIECE_SPANS).min(path.spans.len());
            let piece = VectorPath {
                spans: path.spans[start..end].to_vec(),
                ..Default::default()
            };
            pieces.push((end, Arc::new(Self::build_draft(&piece, style, false)?)));
            start = end;
        }
        let mut mesh = Self::default();
        for (_, piece) in &pieces {
            let base = mesh.vertices.len() as u32;
            mesh.vertices.extend_from_slice(&piece.vertices);
            mesh.normals.extend_from_slice(&piece.normals);
            mesh.indices.extend(piece.indices.iter().map(|i| i + base));
            mesh.edges
                .extend(piece.edges.iter().map(|&(a, b)| (a + base, b + base)));
        }
        Ok((
            mesh,
            DraftPieces {
                style: Some(style),
                spans: path.spans.clone(),
                pieces,
            },
        ))
    }
    fn build_with(
        path: &VectorPath,
        style: Style,
        dashed_arrow: bool,
        union_triangles: bool,
        tolerance: f32,
    ) -> Result<Self> {
        let mut raw = VertexBuffers::<Point, u32>::new();
        let mut stroke = StrokeTessellator::new();
        let options = StrokeOptions::default()
            .with_line_width(style.width.max(0.01))
            .with_line_cap(lyon_path::LineCap::Round)
            .with_line_join(lyon_path::LineJoin::Round)
            .with_tolerance(tolerance);
        let mut builder = Path::builder();
        let mut first = None;
        let mut last = None;
        for (i, &span) in path.spans.iter().enumerate() {
            if style.dashed && (!dashed_arrow || i == 0) {
                if last.take().is_some() {
                    builder.end(false);
                    first = None;
                }
                let segments = VectorPath {
                    spans: vec![span],
                    ..Default::default()
                }
                .flattened();
                // 箭杆只有第一段；其他工具的虚线由整条曲线累计相位。
                if dashed_arrow {
                    for run in dash_runs(&segments, style.width) {
                        add_polyline(&mut builder, &run);
                    }
                } else if i == 0 {
                    for run in dash_runs(&path.flattened(), style.width) {
                        add_polyline(&mut builder, &run);
                    }
                }
                continue;
            }
            if span.start().distance(span.end()) < 0.0001 && matches!(span, Span::Line { .. }) {
                let b = crate::annotation::Bounds {
                    x: span.start().x - style.width / 2.,
                    y: span.start().y - style.width / 2.,
                    width: style.width,
                    height: style.width,
                };
                let dot = VectorPath::ellipse(b)
                    .flattened()
                    .iter()
                    .map(|&(p, _)| p)
                    .collect::<Vec<_>>();
                FillTessellator::new()
                    .tessellate_path(
                        &polygon(&dot),
                        &FillOptions::default(),
                        &mut BuffersBuilder::new(&mut raw, |v: FillVertex| {
                            point_from(v.position())
                        }),
                    )
                    .map_err(|e| e.to_string())?;
                continue;
            }
            if last != Some(span.start()) {
                if last.is_some() {
                    builder.end(false);
                }
                builder.begin(point(span.start().x, span.start().y));
                first = Some(span.start());
            }
            match span {
                Span::Line { end, .. } => {
                    builder.line_to(point(end.x, end.y));
                }
                Span::Cubic {
                    control1,
                    control2,
                    end,
                    ..
                } => {
                    builder.cubic_bezier_to(
                        point(control1.x, control1.y),
                        point(control2.x, control2.y),
                        point(end.x, end.y),
                    );
                }
            }
            last = Some(span.end());
            if last == first {
                builder.end(true);
                last = None;
                first = None;
            }
        }
        if last.is_some() {
            builder.end(false);
        }
        stroke
            .tessellate_path(
                &builder.build(),
                &options,
                &mut BuffersBuilder::new(&mut raw, |v: StrokeVertex| point_from(v.position())),
            )
            .map_err(|e| e.to_string())?;
        for contour in &path.fills {
            FillTessellator::new()
                .tessellate_path(
                    &polygon(contour),
                    &FillOptions::default()
                        .with_fill_rule(FillRule::EvenOdd)
                        .with_tolerance(0.01),
                    &mut BuffersBuilder::new(&mut raw, |v: FillVertex| point_from(v.position())),
                )
                .map_err(|e| e.to_string())?;
        }
        if raw.indices.is_empty() {
            return Ok(Self::default());
        }
        if !union_triangles {
            return Ok(Self::from_buffers(raw));
        }
        // 同一对象所有三角形按一致绕向做非零并集，去掉交叉和填充/描边内部边界。
        let mut union = Path::builder();
        for t in raw.indices.chunks_exact(3) {
            let (a, mut b, mut c) = (
                raw.vertices[t[0] as usize],
                raw.vertices[t[1] as usize],
                raw.vertices[t[2] as usize],
            );
            let area = signed_area(a, b, c);
            if area.abs() < 0.000001 {
                continue;
            }
            if area < 0. {
                std::mem::swap(&mut b, &mut c);
            }
            union.begin(point(a.x, a.y));
            union.line_to(point(b.x, b.y));
            union.line_to(point(c.x, c.y));
            union.end(true);
        }
        let mut mesh = VertexBuffers::<Point, u32>::new();
        FillTessellator::new()
            .tessellate_path(
                &union.build(),
                &FillOptions::default()
                    .with_fill_rule(FillRule::NonZero)
                    .with_tolerance(0.01),
                &mut BuffersBuilder::new(&mut mesh, |v: FillVertex| point_from(v.position())),
            )
            .map_err(|e| e.to_string())?;
        Ok(Self::from_buffers(mesh))
    }

    fn from_buffers(mut mesh: VertexBuffers<Point, u32>) -> Self {
        let mut edges = HashMap::new();
        for t in mesh.indices.chunks_exact_mut(3) {
            if signed_area(
                mesh.vertices[t[0] as usize],
                mesh.vertices[t[1] as usize],
                mesh.vertices[t[2] as usize],
            ) < 0.
            {
                t.swap(1, 2);
            }
            for (a, b) in [(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
                let key = (a.min(b), a.max(b));
                match edges.entry(key) {
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(Some((a, b)));
                    }
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        e.insert(None);
                    }
                }
            }
        }
        let mut edges: Vec<_> = edges.into_values().flatten().collect();
        edges.sort_unstable();
        let mut normals = vec![Point { x: 0., y: 0. }; mesh.vertices.len()];
        let mut sides = vec![Vec::new(); mesh.vertices.len()];
        for &(a, b) in &edges {
            let p = mesh.vertices[a as usize];
            let q = mesh.vertices[b as usize];
            let len = p.distance(q);
            if len < 0.000001 {
                continue;
            }
            let n = Point {
                x: -(q.y - p.y) / len,
                y: (q.x - p.x) / len,
            };
            for i in [a, b] {
                normals[i as usize].x += n.x;
                normals[i as usize].y += n.y;
                sides[i as usize].push(n);
            }
        }
        for (i, n) in normals.iter_mut().enumerate() {
            let length = n.x.hypot(n.y);
            if length < 0.000001 {
                continue;
            }
            n.x /= length;
            n.y /= length;
            let projection = sides[i]
                .iter()
                .map(|s| n.x * s.x + n.y * s.y)
                .fold(1_f32, f32::min)
                .max(0.25);
            n.x /= projection;
            n.y /= projection;
        }
        Self {
            vertices: mesh.vertices,
            indices: mesh.indices,
            edges,
            normals,
            exact: None,
        }
    }
}
fn add_polyline(builder: &mut lyon_path::path::Builder, points: &[Point]) {
    if points.len() < 2 {
        return;
    }
    builder.begin(point(points[0].x, points[0].y));
    for p in &points[1..] {
        builder.line_to(point(p.x, p.y));
    }
    builder.end(false);
}
fn dash_runs(segments: &[(Point, Point)], width: f32) -> Vec<Vec<Point>> {
    let (on, period) = (width * 3. + 2., width * 5. + 4.);
    let mut distance = 0.;
    let mut runs = Vec::new();
    let mut run = Vec::new();
    for &(a, b) in segments {
        let len = a.distance(b);
        if len < 0.000001 {
            continue;
        }
        let mut pos = 0.;
        while pos < len {
            let phase = (distance + pos).rem_euclid(period);
            let drawing = phase < on;
            let next = (pos + (if drawing { on } else { period } - phase).max(0.0001)).min(len);
            let p = |t: f32| Point {
                x: a.x + (b.x - a.x) * t / len,
                y: a.y + (b.y - a.y) * t / len,
            };
            if drawing {
                if run.is_empty() {
                    run.push(p(pos));
                }
                run.push(p(next));
            } else if !run.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
            pos = next;
        }
        distance += len;
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    fn boundary_length(mesh: &VectorMesh) -> f32 {
        mesh.edges
            .iter()
            .map(|&(a, b)| mesh.vertices[a as usize].distance(mesh.vertices[b as usize]))
            .sum()
    }
    #[test]
    fn draft_mesh_of_simple_stroke_has_the_same_outline_without_union() {
        let points: Vec<_> = (0..200)
            .map(|i| {
                let t = i as f32 / 199.;
                Point {
                    x: 10. + 600. * t,
                    y: 200. + 80. * (t * 9.).sin(),
                }
            })
            .collect();
        let path = VectorPath::from_polyline(&points);
        let style = Style {
            width: 6.,
            ..Default::default()
        };
        let full = VectorMesh::build(&path, style, false).unwrap();
        let draft = VectorMesh::build_draft(&path, style, false).unwrap();
        assert!(!draft.indices.is_empty());
        // 共享顶点：lyon 网格内部边不得被当作外沿，否则草稿会出现内部抗锯齿缝。
        let (a, b) = (boundary_length(&full), boundary_length(&draft));
        assert!((a - b).abs() / a < 0.02, "outline {a} vs draft {b}");
    }
    /// 以 1/4 像素栅格采样三角形并集（MAX 覆盖：任一三角形覆盖即算）。
    fn coverage(mesh: &VectorMesh, size: (usize, usize)) -> Vec<bool> {
        const STEP: f32 = 0.25;
        let (w, h) = (size.0 * 4, size.1 * 4);
        let mut covered = vec![false; w * h];
        for t in mesh.indices.chunks_exact(3) {
            let [a, b, c] = [0, 1, 2].map(|k| mesh.vertices[t[k] as usize]);
            let (x0, x1) = (a.x.min(b.x).min(c.x), a.x.max(b.x).max(c.x));
            let (y0, y1) = (a.y.min(b.y).min(c.y), a.y.max(b.y).max(c.y));
            let col = |v: f32| ((v / STEP).floor().max(0.) as usize).min(w - 1);
            let row = |v: f32| ((v / STEP).floor().max(0.) as usize).min(h - 1);
            for y in row(y0)..=row(y1) {
                for x in col(x0)..=col(x1) {
                    let p = Point {
                        x: (x as f32 + 0.5) * STEP,
                        y: (y as f32 + 0.5) * STEP,
                    };
                    let s = [
                        signed_area(a, b, p),
                        signed_area(b, c, p),
                        signed_area(c, a, p),
                    ];
                    if s.iter().all(|v| *v >= 0.) || s.iter().all(|v| *v <= 0.) {
                        covered[y * w + x] = true;
                    }
                }
            }
        }
        covered
    }
    fn scribble_points(count: usize) -> Vec<Point> {
        (0..count)
            .map(|i| {
                let t = i as f32 / count as f32;
                Point {
                    x: (20. + 300. * (0.5 - 0.5 * (t * 7. * std::f32::consts::PI).cos())).round(),
                    y: (20. + 140. * t + 12. * (t * 31.).sin()).round(),
                }
            })
            .collect()
    }
    #[test]
    fn growing_draft_reuses_unchanged_pieces_and_covers_the_same_area() {
        let points = scribble_points(400);
        let style = Style {
            width: 5.,
            opacity: 0.6,
            ..Default::default()
        };
        let mut previous: Option<DraftPieces> = None;
        let (mut rebuilt, mut frames) = (0, 0);
        for n in 2..=points.len() {
            let path = crate::pencil::fit(&points[..n]);
            let (_, pieces) =
                VectorMesh::build_draft_pieces(&path, style, false, previous.as_ref()).unwrap();
            if let Some(previous) = &previous {
                let kept = pieces
                    .pieces
                    .iter()
                    .zip(&previous.pieces)
                    .take_while(|(a, b)| Arc::ptr_eq(&a.1, &b.1))
                    .count();
                rebuilt += pieces.pieces.len() - kept;
                frames += 1;
            }
            previous = Some(pieces);
        }
        let pieces = previous.as_ref().unwrap();
        assert!(pieces.pieces.len() >= 4, "pieces {}", pieces.pieces.len());
        // 拟合只改动尾部：平均每帧重新细分的块数不随笔迹长度增长。
        let average = rebuilt as f32 / frames as f32;
        assert!(average < 2.5, "rebuilt {average} pieces per frame");
        let path = crate::pencil::fit(&points);
        let whole = VectorMesh::build_draft(&path, style, false).unwrap();
        let (pieced, _) =
            VectorMesh::build_draft_pieces(&path, style, false, Some(pieces)).unwrap();
        let (a, b) = (coverage(&whole, (360, 200)), coverage(&pieced, (360, 200)));
        let covered = a.iter().filter(|v| **v).count();
        let differ = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        // 圆头并集与圆角连接只差曲线细分容差，接缝处不得缺块。
        assert!(covered > 10_000);
        assert!(differ * 2000 < covered, "differ {differ} of {covered}");
        let dashed = Style {
            dashed: true,
            ..style
        };
        let (_, cache) = VectorMesh::build_draft_pieces(&path, dashed, false, None).unwrap();
        assert!(cache.pieces.is_empty(), "虚线相位依赖整条路径");
    }
    /// 超长单笔草稿逐帧细分成本：整笔对分块尾部。
    #[test]
    #[ignore]
    fn long_draft_frame_cost() {
        let points = scribble_points(3000);
        let style = Style {
            width: 4.,
            ..Default::default()
        };
        for n in [400, 1500, 3000] {
            let path = crate::pencil::fit(&points[..n]);
            let time = |f: &mut dyn FnMut()| {
                let started = std::time::Instant::now();
                for _ in 0..10 {
                    f();
                }
                started.elapsed().as_secs_f64() * 100.
            };
            let fit = time(&mut || {
                std::hint::black_box(crate::pencil::fit(&points[..n]));
            });
            let flatten = time(&mut || {
                std::hint::black_box(path.flattened());
            });
            let whole = time(&mut || {
                std::hint::black_box(VectorMesh::build_draft(&path, style, false).unwrap());
            });
            let (_, cache) = VectorMesh::build_draft_pieces(
                &crate::pencil::fit(&points[..n - 1]),
                style,
                false,
                None,
            )
            .unwrap();
            let pieced = time(&mut || {
                std::hint::black_box(
                    VectorMesh::build_draft_pieces(&path, style, false, Some(&cache)).unwrap(),
                );
            });
            eprintln!(
                "{n} points, {} spans: fit {fit:.2} ms, flatten {flatten:.2} ms, \
                 whole draft {whole:.2} ms, pieced draft {pieced:.2} ms",
                path.spans.len()
            );
        }
    }
    /// 真实鼠标手绘：1000Hz 补回样本、整数坐标、相邻约 1–2 px，连续绕圈并缓慢右移。
    pub(crate) fn hand_samples(x: f32, y: f32, samples: usize, seed: u32) -> Vec<Point> {
        let mut out: Vec<Point> = Vec::new();
        for i in 0..samples {
            let t = i as f32 * 0.037;
            let r = 38. + 14. * (t * 0.31 + seed as f32).sin();
            let hash = (i as u32 ^ seed.wrapping_mul(2654435761)).wrapping_mul(2246822519);
            let jitter = (hash >> 16) as f32 / 65535. - 0.5;
            let p = Point {
                x: (x + i as f32 * 0.3 + r * t.cos() + jitter * 0.8).round(),
                y: (y + r * 0.8 * t.sin() - jitter * 0.6).round(),
            };
            if out.last() != Some(&p) {
                out.push(p);
            }
        }
        out
    }
    /// 真实采样密度的一笔：不同细分容差、是否并集的几何量、耗时与覆盖差异。
    #[test]
    #[ignore]
    fn hand_stroke_tolerance_cost() {
        let samples = hand_samples(100., 100., 1500, 1);
        let path = crate::pencil::fit(&samples);
        let style = Style {
            width: 4.,
            ..Default::default()
        };
        eprintln!(
            "samples {}, spans {}, flattened {}",
            samples.len(),
            path.spans.len(),
            path.flattened().len()
        );
        let reference = VectorMesh::build_with(&path, style, false, true, 0.01).unwrap();
        let size = (700, 260);
        let base = coverage(&reference, size);
        let covered = base.iter().filter(|v| **v).count();
        for tolerance in [0.01, 0.03, 0.05, 0.1, 0.2] {
            for union in [false, true] {
                let started = std::time::Instant::now();
                let runs = 5;
                let mut mesh = VectorMesh::default();
                for _ in 0..runs {
                    mesh = VectorMesh::build_with(&path, style, false, union, tolerance).unwrap();
                }
                let ms = started.elapsed().as_secs_f64() * 1000. / runs as f64;
                let differ = coverage(&mesh, size)
                    .iter()
                    .zip(&base)
                    .filter(|(a, b)| a != b)
                    .count();
                eprintln!(
                    "tol {tolerance:<5} union {union:<5}: {ms:7.2} ms, verts {:6} tris {:6} edges {:6} gpu verts {:7}, coverage diff {:.4}%",
                    mesh.vertices.len(),
                    mesh.indices.len() / 3,
                    mesh.edges.len(),
                    (mesh.indices.len() / 3 + mesh.edges.len() * 2) * 3,
                    differ as f64 * 100. / covered as f64
                );
            }
        }
    }
    #[test]
    fn crossing_translucent_stroke_has_one_nonoverlapping_mesh() {
        let p = |x, y| Point { x, y };
        let path = VectorPath::from_polyline(&[p(0., 0.), p(40., 40.), p(0., 40.), p(40., 0.)]);
        let mesh = VectorMesh::build(
            &path,
            Style {
                width: 8.,
                opacity: 0.3,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert!(!mesh.edges.is_empty());
        // 交叉内部不允许出现虚假的外边缘。
        for &(a, b) in &mesh.edges {
            let (a, b) = (mesh.vertices[a as usize], mesh.vertices[b as usize]);
            let mid = p((a.x + b.x) / 2., (a.y + b.y) / 2.);
            assert!(mid.distance(p(20., 20.)) > 3.9);
        }
        assert!(mesh
            .normals
            .iter()
            .all(|p| p.x.is_finite() && p.y.is_finite()));
    }
    #[test]
    fn curved_filled_shape_and_dot_keep_vector_boundaries() {
        let mut path = VectorPath::ellipse(crate::annotation::Bounds {
            x: 0.,
            y: 0.,
            width: 400.,
            height: 140.,
        });
        path.fills
            .push(path.flattened().iter().map(|&(a, _)| a).collect());
        let mesh = VectorMesh::build(&path, Style::default(), false).unwrap();
        assert!(mesh.edges.len() > 100);
        let dot = VectorMesh::build(
            &VectorPath::from_polyline(&[Point { x: 1., y: 2. }]),
            Style::default(),
            false,
        )
        .unwrap();
        assert!(!dot.indices.is_empty());
    }
}
