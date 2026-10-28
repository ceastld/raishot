//! GPUI 公共 Path API 的解析边缘覆盖；不把标注转换成纹理或放大位图。
use crate::{
    annotation::{Point, Style},
    geometry::Rect,
    vector_mesh::VectorMesh,
};
use gpui::{point, px, rgba, size, App, Bounds, Path, Pixels, RenderImage, Window};
use std::{ops::Range, sync::Arc};

#[derive(Clone, Copy)]
pub(crate) enum PathPart {
    Main = 0,
    Border = 1,
}

#[derive(Clone, Default)]
struct CachedPath {
    prepared: bool,
    path: Option<Path<Pixels>>,
}

/// 一个 paint callback 独占的路径缓存；区域 mask 与图像画刷不参与几何准备。
/// 每帧重新创建，避免跨帧沿用不同 mesh、缩放、DPI 或分数坐标的路径。
pub(crate) struct FramePaths {
    paths: Vec<[CachedPath; 2]>,
    layers: usize,
    origin: gpui::Point<Pixels>,
    scale: f32,
    dpi: f32,
    #[cfg(test)]
    preparations: usize,
}
impl FramePaths {
    pub(crate) fn new(layers: usize, origin: gpui::Point<Pixels>, scale: f32, dpi: f32) -> Self {
        Self {
            paths: Vec::new(),
            layers,
            origin,
            scale,
            dpi,
            #[cfg(test)]
            preparations: 0,
        }
    }
    pub(crate) fn get(
        &mut self,
        layer: usize,
        part: PathPart,
        mesh: &VectorMesh,
        style: Style,
        repeated: bool,
    ) -> Option<Path<Pixels>> {
        // 单区域、单画刷路径直接交给 GPUI；不增加缓存分配或顶点 Clone。
        if !repeated {
            #[cfg(test)]
            {
                self.preparations += 1;
            }
            return vector_path(mesh, style, self.origin, self.scale, self.dpi);
        }
        if self.paths.is_empty() {
            self.paths.resize_with(self.layers, Default::default);
        }
        let cached = &mut self.paths[layer][part as usize];
        if !cached.prepared {
            cached.path = vector_path(mesh, style, self.origin, self.scale, self.dpi);
            cached.prepared = true;
            #[cfg(test)]
            {
                self.preparations += 1;
            }
        }
        cached.path.clone()
    }
}

fn vector_path(
    mesh: &VectorMesh,
    style: Style,
    origin: gpui::Point<Pixels>,
    scale: f32,
    dpi: f32,
) -> Option<Path<Pixels>> {
    if mesh.vertices.is_empty() {
        return None;
    }
    let pixel = scale / dpi;
    let inner = (style.width / pixel / 2.).clamp(0.001, 0.5);
    let moved = |i: u32, d: f32| {
        let p = mesh.vertices[i as usize];
        let n = mesh.normals[i as usize];
        Point {
            x: p.x + n.x * d * pixel,
            y: p.y + n.y * d * pixel,
        }
    };
    let screen = |p: Point| origin + point(px(p.x / scale), px(p.y / scale));
    let mut path = Path::new(screen(mesh.vertices[0]));
    let mut min = Point {
        x: f32::INFINITY,
        y: f32::INFINITY,
    };
    let mut max = Point {
        x: f32::NEG_INFINITY,
        y: f32::NEG_INFINITY,
    };
    let mut triangle = |a: Point, b: Point, c: Point, alpha: (f32, f32, f32)| {
        for p in [a, b, c] {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        path.push_triangle(
            (screen(a), screen(b), screen(c)),
            (point(0., alpha.0), point(0., alpha.1), point(0., alpha.2)),
        );
    };
    for t in mesh.indices.chunks_exact(3) {
        let (a, b, c) = (moved(t[0], inner), moved(t[1], inner), moved(t[2], inner));
        if (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x) > 0.0000001 {
            triangle(a, b, c, (1., 1., 1.));
        }
    }
    // GPUI Windows 的曲线片元接口在 st.x=0、st.y 线性变化时提供连续的一像素覆盖率。
    // 外沿扩大到 1.5 设备像素，使 4× MSAA 不再截掉解析边缘的半透明样本。
    for &(a, b) in &mesh.edges {
        let (ai, bi, ao, bo) = (
            moved(a, inner),
            moved(b, inner),
            moved(a, -1.5),
            moved(b, -1.5),
        );
        triangle(ai, bi, bo, (inner, inner, -1.5));
        triangle(ai, bo, ao, (inner, -1.5, -1.5));
    }
    if !min.x.is_finite() {
        return None;
    }
    // 最终矩形拷贝边界按实际设备像素外扩，避免分数位置漏掉右/下抗锯齿行。
    for (p, outward) in [(min, false), (max, true)] {
        let p = screen(p);
        let align = |v: Pixels| {
            let physical = f32::from(v) * dpi;
            px(if outward {
                (physical + 1.).ceil() / dpi
            } else {
                (physical - 1.).floor() / dpi
            })
        };
        let p = point(align(p.x), align(p.y));
        let uv = point(0., 1.);
        path.push_triangle((p, p, p), (uv, uv, uv));
    }
    Some(path)
}
pub(crate) fn paint(path: Option<Path<Pixels>>, style: Style, window: &mut Window) {
    let Some(path) = path else {
        return;
    };
    let alpha = (style.opacity.clamp(0., 1.) * 255.).round() as u32;
    let color = rgba((style.color << 8) | alpha);
    #[cfg(target_os = "windows")]
    window.paint_path_union(path, color);
    #[cfg(not(target_os = "windows"))]
    window.paint_path(path, color);
}
/// 图像只提供颜色/原始 alpha；路径的解析 AA、缩放与普通矢量绘制共用。
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_image(
    path: Option<Path<Pixels>>,
    style: Style,
    origin: gpui::Point<Pixels>,
    scale: f32,
    rect: Rect,
    image_rect: Rect,
    image: &Arc<RenderImage>,
    window: &mut Window,
    cx: &mut App,
) -> crate::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    window
        .paint_path_image(
            path,
            Bounds::new(
                origin
                    + point(
                        px(image_rect.x as f32 / scale),
                        px(image_rect.y as f32 / scale),
                    ),
                size(
                    px(image_rect.width as f32 / scale),
                    px(image_rect.height as f32 / scale),
                ),
            ),
            image.clone(),
            Bounds::new(
                origin + point(px(rect.x as f32 / scale), px(rect.y as f32 / scale)),
                size(
                    px(rect.width as f32 / scale),
                    px(rect.height as f32 / scale),
                ),
            ),
            style.opacity.clamp(0., 1.),
            cx,
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 一层在画布上的绘制身份：Scene 层（按 Arc 身份比较）加上实际使用的预览图地址。
#[derive(Clone)]
pub(crate) struct LayerSignature {
    layer: crate::scene::Layer,
    preview: Option<(usize, Rect)>,
}
impl LayerSignature {
    pub(crate) fn new(layer: &crate::scene::Layer, preview: Option<(usize, Rect)>) -> Self {
        Self {
            layer: layer.clone(),
            preview,
        }
    }
    fn same(&self, other: &Self) -> bool {
        self.preview == other.preview && crate::scene::same_layer(&self.layer, &other.layer)
    }
}

/// 缓存纹理对应的屏幕投影：原点、缩放、DPI、画布与视口尺寸、外层内容组范围。
pub(crate) type RetainedParams = [f32; 12];

struct RetainedSlot {
    layer: Arc<gpui::RetainedLayer>,
    /// renderer 已确认纹理里保存的内容。
    committed: Option<(u64, Vec<LayerSignature>)>,
    /// 上一帧请求、等待 renderer 确认的内容。
    pending: Option<(u64, Vec<LayerSignature>)>,
}
impl Default for RetainedSlot {
    fn default() -> Self {
        Self {
            layer: gpui::RetainedLayer::new(),
            committed: None,
            pending: None,
        }
    }
}
impl RetainedSlot {
    fn sync(&mut self, ready: u64) {
        if let Some(pending) = self.pending.take() {
            if ready != 0 && pending.0 == ready {
                self.committed = Some(pending);
            }
        }
        if self
            .committed
            .as_ref()
            .is_some_and(|(key, _)| *key != ready)
        {
            self.committed = None;
        }
    }
    fn reset(&mut self) {
        self.committed = None;
        self.pending = None;
    }
    fn part(
        &mut self,
        key: u64,
        mode: gpui::RetainedPaint,
        range: Range<usize>,
        covers: &[LayerSignature],
    ) -> RetainedPart {
        if mode != gpui::RetainedPaint::Reuse {
            self.pending = Some((key, covers.to_vec()));
        }
        RetainedPart {
            layer: self.layer.clone(),
            key,
            mode,
            range,
        }
    }
}

/// 一张保留纹理本帧的绘制方式；`range` 是这次真正要画进纹理的层。
pub(crate) struct RetainedPart {
    pub(crate) layer: Arc<gpui::RetainedLayer>,
    pub(crate) key: u64,
    pub(crate) mode: gpui::RetainedPaint,
    pub(crate) range: Range<usize>,
}
pub(crate) struct RetainedPlan {
    pub(crate) below: Option<RetainedPart>,
    pub(crate) live: Range<usize>,
    pub(crate) above: Option<RetainedPart>,
}

/// 大量标注时逐帧重建全部路径会随笔数线性变慢。未变化的下层与上层各画进一张跨帧纹理，
/// 只有正在变化的层（草稿、拖动对象）逐帧重画；提交新笔只把这一笔追加进下层纹理。
/// 预乘 source-over 满足结合律，分成下层/实时/上层三段合成与逐层直接绘制一致。
#[derive(Default)]
pub(crate) struct RetainedAnnotations {
    below: RetainedSlot,
    above: RetainedSlot,
    previous: Vec<LayerSignature>,
    params: Option<RetainedParams>,
    next_key: u64,
    /// 性能对照专项用：关闭后每帧逐层直接绘制，并释放已有纹理。
    pub(crate) disabled: bool,
}
impl RetainedAnnotations {
    pub(crate) fn plan(
        &mut self,
        layers: Vec<LayerSignature>,
        params: RetainedParams,
    ) -> RetainedPlan {
        if self.disabled {
            self.below.reset();
            self.above.reset();
            self.previous.clear();
            self.params = None;
            return RetainedPlan {
                below: None,
                live: 0..layers.len(),
                above: None,
            };
        }
        let ready = [self.below.layer.ready_key(), self.above.layer.ready_key()];
        self.plan_with(layers, params, ready)
    }
    fn plan_with(
        &mut self,
        layers: Vec<LayerSignature>,
        params: RetainedParams,
        ready: [u64; 2],
    ) -> RetainedPlan {
        let n = layers.len();
        // 缩放/平移/窗口变化时纹理全部失效，当帧就整体重建：鼠标静止后 GPUI 会在一秒内
        // 每个 vsync 重复 Present 同一 Scene，重建帧的重复 Present 只合成纹理。
        let fresh = self.params != Some(params);
        if fresh {
            self.below.reset();
            self.above.reset();
            self.params = Some(params);
            self.previous.clear();
        }
        self.below.sync(ready[0]);
        self.above.sync(ready[1]);
        let previous = std::mem::replace(&mut self.previous, layers);
        let layers = &self.previous;
        // 与上一帧比较：[prefix, n - suffix) 是本帧变化的层。
        let prefix = previous
            .iter()
            .zip(layers)
            .take_while(|(a, b)| a.same(b))
            .count();
        let suffix = previous
            .iter()
            .rev()
            .zip(layers.iter().rev())
            .take(n.min(previous.len()) - prefix)
            .take_while(|(a, b)| a.same(b))
            .count();
        let changed = !fresh && prefix + suffix < n;
        // 静止时最上一层单独放进上层纹理：紧接着拖动、微调刚放下的对象（如马赛克）或撤销
        // 最后一步最常见，这时下层纹理仍然有效，不必把全部笔迹重画一遍。代价是画下一笔时
        // 先把这一层追加进下层、提交后再单独建一次上层，各只画一层。
        let stable_end = if changed { prefix } else { n.saturating_sub(1) };
        let below_valid = self
            .below
            .committed
            .as_ref()
            .filter(|(_, cached)| {
                !cached.is_empty()
                    && cached.len() <= n
                    && cached.iter().zip(layers).all(|(a, b)| a.same(b))
            })
            .map(|(_, cached)| cached.len());
        let above_valid = self
            .above
            .committed
            .as_ref()
            .filter(|(_, cached)| {
                !cached.is_empty()
                    && cached.len() <= n
                    && cached
                        .iter()
                        .zip(&layers[n - cached.len()..])
                        .all(|(a, b)| a.same(b))
            })
            .map(|(_, cached)| n - cached.len());
        let (below, below_end) = match below_valid {
            Some(end)
                if stable_end > end && above_valid.is_none_or(|start| start >= stable_end) =>
            {
                self.next_key += 1;
                let key = self.next_key;
                let part = self.below.part(
                    key,
                    gpui::RetainedPaint::Append {
                        base: self.below.committed.as_ref().map_or(0, |(key, _)| *key),
                    },
                    end..stable_end,
                    &layers[..stable_end],
                );
                (Some(part), stable_end)
            }
            Some(end) => {
                let key = self.below.committed.as_ref().map_or(0, |(key, _)| *key);
                let part = self
                    .below
                    .part(key, gpui::RetainedPaint::Reuse, end..end, &[]);
                (Some(part), end)
            }
            None => {
                let end = stable_end.min(above_valid.unwrap_or(n));
                if end > 0 {
                    self.next_key += 1;
                    let key = self.next_key;
                    let part =
                        self.below
                            .part(key, gpui::RetainedPaint::Rebuild, 0..end, &layers[..end]);
                    (Some(part), end)
                } else {
                    (None, 0)
                }
            }
        };
        // 变化时上层纹理应正好是变化区之上的稳定后缀；只缓存了更少的层时重建一次，
        // 免得中间的稳定层逐帧实时重画。静止时沿用已有的上层纹理。
        let want = if changed {
            (n - suffix).max(below_end)
        } else {
            below_end
        };
        let (above, above_start) =
            match above_valid.filter(|&start| start >= below_end && (!changed || start <= want)) {
                Some(start) => {
                    let key = self.above.committed.as_ref().map_or(0, |(key, _)| *key);
                    let part = self
                        .above
                        .part(key, gpui::RetainedPaint::Reuse, start..start, &[]);
                    (Some(part), start)
                }
                None if want < n && (!changed || suffix > 0) => {
                    let start = want;
                    self.next_key += 1;
                    let key = self.next_key;
                    let part = self.above.part(
                        key,
                        gpui::RetainedPaint::Rebuild,
                        start..n,
                        &layers[start..],
                    );
                    (Some(part), start)
                }
                None => (None, n),
            };
        RetainedPlan {
            below,
            live: below_end..above_start,
            above,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector_path::VectorPath;

    fn mesh() -> VectorMesh {
        VectorMesh::build_draft(
            &VectorPath::from_polyline(&[
                Point { x: 2., y: 3. },
                Point { x: 40., y: 20. },
                Point { x: 60., y: 4. },
            ]),
            Style::default(),
            false,
        )
        .unwrap()
    }

    fn layer() -> LayerSignature {
        LayerSignature::new(
            &crate::scene::Layer::Mosaic {
                blocks: Arc::new(Vec::new()),
            },
            None,
        )
    }
    fn mode(part: &Option<RetainedPart>) -> Option<(gpui::RetainedPaint, Range<usize>)> {
        part.as_ref().map(|part| (part.mode, part.range.clone()))
    }
    fn key(part: &Option<RetainedPart>) -> u64 {
        part.as_ref().unwrap().key
    }
    const PARAMS: RetainedParams = [0., 0., 1., 1.25, 800., 600., 640., 480., 0., 0., 0., 0.];

    #[test]
    fn retained_annotations_cache_static_layers_and_append_new_strokes() {
        use gpui::RetainedPaint::*;
        let mut cache = RetainedAnnotations::default();
        let mut layers: Vec<_> = (0..5).map(|_| layer()).collect();
        // 首帧/投影变化当帧整体建缓存；最上一层单独放上层纹理。
        let plan = cache.plan_with(layers.clone(), PARAMS, [0, 0]);
        assert_eq!(mode(&plan.below), Some((Rebuild, 0..4)));
        assert_eq!(mode(&plan.above), Some((Rebuild, 4..5)));
        assert!(plan.live.is_empty());
        let (built, top) = (key(&plan.below), key(&plan.above));
        let plan = cache.plan_with(layers.clone(), PARAMS, [built, top]);
        assert_eq!(mode(&plan.below), Some((Reuse, 4..4)));
        assert_eq!(key(&plan.below), built);
        assert_eq!(mode(&plan.above), Some((Reuse, 4..4)));
        assert_eq!(plan.live, 4..4);
        // 开始画新笔：原最上一层追加进下层纹理（只画这一层），之后只实时画草稿。
        let mut frame = layers.clone();
        frame.push(layer());
        let plan = cache.plan_with(frame, PARAMS, [built, top]);
        assert_eq!(mode(&plan.below), Some((Append { base: built }, 4..5)));
        assert!(plan.above.is_none());
        assert_eq!(plan.live, 5..6);
        let appended = key(&plan.below);
        for _ in 0..3 {
            let mut frame = layers.clone();
            frame.push(layer());
            let plan = cache.plan_with(frame, PARAMS, [appended, top]);
            assert_eq!(mode(&plan.below), Some((Reuse, 5..5)));
            assert_eq!(plan.live, 5..6);
            assert!(plan.above.is_none());
        }
        // 提交后首帧仍实时画新层；稳定后这一层单独建上层纹理，下层不动。
        layers.push(layer());
        let plan = cache.plan_with(layers.clone(), PARAMS, [appended, top]);
        assert_eq!(mode(&plan.below), Some((Reuse, 5..5)));
        assert!(plan.above.is_none());
        assert_eq!(plan.live, 5..6);
        let plan = cache.plan_with(layers.clone(), PARAMS, [appended, top]);
        assert_eq!(mode(&plan.below), Some((Reuse, 5..5)));
        assert_eq!(mode(&plan.above), Some((Rebuild, 5..6)));
        assert_eq!(plan.live, 5..5);
        // 未确认（未 Present）时按 renderer 真实内容再建一次。
        let plan = cache.plan_with(layers.clone(), PARAMS, [appended, top]);
        assert_eq!(mode(&plan.above), Some((Rebuild, 5..6)));
        let newest = key(&plan.above);
        let plan = cache.plan_with(layers.clone(), PARAMS, [appended, newest]);
        assert_eq!(mode(&plan.above), Some((Reuse, 5..5)));
        // 拖动刚放下的最上一层：下层纹理不变，只实时画这一层；松手后单独重建上层纹理。
        for _ in 0..3 {
            layers[5] = layer();
            let plan = cache.plan_with(layers.clone(), PARAMS, [appended, newest]);
            assert_eq!(mode(&plan.below), Some((Reuse, 5..5)));
            assert!(plan.above.is_none());
            assert_eq!(plan.live, 5..6);
        }
        let plan = cache.plan_with(layers.clone(), PARAMS, [appended, newest]);
        assert_eq!(mode(&plan.below), Some((Reuse, 5..5)));
        assert_eq!(mode(&plan.above), Some((Rebuild, 5..6)));
        // renderer 丢失纹理（resize/设备恢复）后整体重建。
        let plan = cache.plan_with(layers.clone(), PARAMS, [0, 0]);
        assert_eq!(mode(&plan.below), Some((Rebuild, 0..5)));
        assert_eq!(mode(&plan.above), Some((Rebuild, 5..6)));
        let (rebuilt, top) = (key(&plan.below), key(&plan.above));
        // 撤销最后一层：下层纹理正好是剩余各层，直接复用，不再整体重建。
        layers.pop();
        let plan = cache.plan_with(layers.clone(), PARAMS, [rebuilt, top]);
        assert_eq!(mode(&plan.below), Some((Reuse, 5..5)));
        assert!(plan.above.is_none());
        assert_eq!(plan.live, 5..5);
        // 缩放/平移改变投影：旧纹理作废，按新投影整体重建。
        let mut moved = PARAMS;
        moved[0] = 3.;
        let plan = cache.plan_with(layers, moved, [key(&plan.below), 0]);
        assert_eq!(mode(&plan.below), Some((Rebuild, 0..4)));
        assert_eq!(mode(&plan.above), Some((Rebuild, 4..5)));
        assert!(plan.live.is_empty());
    }

    #[test]
    fn retained_annotations_split_around_a_dragged_middle_layer() {
        use gpui::RetainedPaint::*;
        let mut cache = RetainedAnnotations::default();
        let mut layers: Vec<_> = (0..8).map(|_| layer()).collect();
        let plan = cache.plan_with(layers.clone(), PARAMS, [0, 0]);
        let (whole, top) = (key(&plan.below), key(&plan.above));
        // 拖动第 3 层：下层 [0,3) 与上层 [4,8) 各建一次，之后只重画被拖动的一层。
        layers[3] = layer();
        let plan = cache.plan_with(layers.clone(), PARAMS, [whole, top]);
        assert_eq!(mode(&plan.below), Some((Rebuild, 0..3)));
        assert_eq!(mode(&plan.above), Some((Rebuild, 4..8)));
        assert_eq!(plan.live, 3..4);
        let (below, above) = (key(&plan.below), key(&plan.above));
        for _ in 0..3 {
            layers[3] = layer();
            let plan = cache.plan_with(layers.clone(), PARAMS, [below, above]);
            assert_eq!(mode(&plan.below), Some((Reuse, 3..3)));
            assert_eq!(mode(&plan.above), Some((Reuse, 4..4)));
            assert_eq!(plan.live, 3..4);
        }
        // 松手静止后保持三段，不为一层把上层全部重画进下层纹理。
        let plan = cache.plan_with(layers.clone(), PARAMS, [below, above]);
        assert_eq!(mode(&plan.below), Some((Reuse, 3..3)));
        assert_eq!(mode(&plan.above), Some((Reuse, 4..4)));
        // 之后在最上方画新笔：上层纹理不再是后缀，合并进下层。
        layers.push(layer());
        let plan = cache.plan_with(layers.clone(), PARAMS, [below, above]);
        assert_eq!(mode(&plan.below), Some((Append { base: below }, 3..8)));
        assert!(plan.above.is_none());
        assert_eq!(plan.live, 8..9);
    }

    /// 大量来回涂抹后逐帧 CPU 成本：模型克隆、Scene 更新与路径重建。
    #[test]
    #[ignore]
    fn many_scribbles_frame_cost() {
        use crate::{
            annotation::Tool,
            editor::Editor,
            scene::{PixelPlane, Scene, SourcePixels},
        };
        let full = Rect::full(2560, 1440);
        let bytes = vec![128u8; 2560 * 1440 * 4];
        let source = SourcePixels {
            fill: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
            overlay: None,
        };
        let mut editor = Editor::new(full, Some(full));
        editor.select_tool(Tool::Pen);
        let mut scene = Arc::new(Scene::prepare(&editor.annotations(), &source));
        let strokes = 40;
        for s in 0..strokes {
            let y0 = 100 + (s * 29) % 1200;
            editor.pointer_down((200, y0 as i32), 4.);
            for i in 0..400 {
                let t = i as f32 / 400.;
                // 来回涂抹：横向往返 8 次，纵向缓慢漂移。
                let x = 200. + 2000. * (0.5 - 0.5 * (t * 8. * std::f32::consts::PI).cos());
                let y = y0 as f32 + 200. * t;
                editor.pointer_move((x as i32, y as i32));
            }
            editor.pointer_up((300, (y0 + 200) as i32));
            scene = Arc::new(scene.updated(&editor.annotations(), &source));
        }
        let (mut verts, mut tris, mut edges) = (0, 0, 0);
        for layer in &scene.layers {
            if let crate::scene::Layer::Vector { mesh, .. } = layer {
                verts += mesh.vertices.len();
                tris += mesh.indices.len() / 3;
                edges += mesh.edges.len();
            }
        }
        eprintln!("{strokes} strokes: mesh verts {verts} tris {tris} edges {edges}");
        let frames = 10;
        let started = std::time::Instant::now();
        for _ in 0..frames {
            let mut paths = FramePaths::new(scene.layers.len(), point(px(0.), px(0.)), 1.25, 1.25);
            for (i, layer) in scene.layers.iter().enumerate() {
                if let crate::scene::Layer::Vector { mesh, style, .. } = layer {
                    std::hint::black_box(paths.get(i, PathPart::Main, mesh, *style, false));
                }
            }
        }
        eprintln!(
            "vector_path per frame {:.2} ms, ~{} path vertices",
            started.elapsed().as_secs_f64() * 1000. / frames as f64,
            (tris + edges * 2) * 3
        );
        // 在已有笔迹上再画一笔：逐次移动的模型克隆与 Scene 更新。
        editor.pointer_down((300, 300), 4.);
        let started = std::time::Instant::now();
        let (mut clone_ms, mut scene_ms) = (0., 0.);
        for i in 0..100 {
            editor.pointer_move((300 + i * 5, 300 + (i * 7) % 90));
            let s = std::time::Instant::now();
            let annotations = editor.annotations();
            clone_ms += s.elapsed().as_secs_f64() * 1000.;
            let s = std::time::Instant::now();
            scene = Arc::new(scene.updated_with_draft(
                &annotations,
                &source,
                editor.live_stroke_draft_index(),
            ));
            scene_ms += s.elapsed().as_secs_f64() * 1000.;
        }
        eprintln!(
            "per move: annotations {:.3} ms, scene {:.3} ms, total {:.3} ms",
            clone_ms / 100.,
            scene_ms / 100.,
            started.elapsed().as_secs_f64() * 1000. / 100.
        );
    }

    #[test]
    fn frame_paths_prepare_each_layer_part_once_across_masks_and_brush_tiles() {
        let mesh = mesh();
        let empty = VectorMesh::default();
        let style = Style::default();
        let mut frame = FramePaths::new(3, point(px(-2.25), px(1.5)), 0.8, 1.25);
        let mut identities = [None; 3];
        // 三块圆角主体区域与每块的多个荧光笔画刷 tile 只改变提交 mask。
        for _region in 0..3 {
            for (slot, layer, part) in [
                (0, 0, PathPart::Main),
                (1, 0, PathPart::Border),
                (2, 1, PathPart::Main),
            ] {
                for _tile in 0..4 {
                    let submitted = frame.get(layer, part, &mesh, style, true).unwrap();
                    let cached = frame.paths[layer][part as usize].path.as_ref().unwrap();
                    assert_eq!(format!("{submitted:?}"), format!("{cached:?}"));
                    let identity = cached as *const Path<Pixels>;
                    if let Some(first) = identities[slot] {
                        assert_eq!(identity, first, "区域和画刷不得重复准备几何");
                    } else {
                        identities[slot] = Some(identity);
                    }
                }
            }
            assert!(frame.get(2, PathPart::Main, &empty, style, true).is_none());
        }
        assert_eq!(frame.preparations, 4, "空几何也只准备一次");
        assert_ne!(identities[0], identities[1], "聚光灯暗层与边框独立");
        assert_ne!(identities[0], identities[2], "不同层保持独立几何");
    }

    #[test]
    fn frame_path_geometry_preserves_projection_and_separates_color_from_coverage() {
        let mesh = mesh();
        let origin = point(px(-2.25), px(1.5));
        let style = Style::default();
        let mut frame = FramePaths::new(1, origin, 0.8, 1.25);
        let geometry = format!(
            "{:?}",
            frame.get(0, PathPart::Main, &mesh, style, true).unwrap()
        );
        let mut once = FramePaths::new(1, origin, 0.8, 1.25);
        assert_eq!(
            geometry,
            format!(
                "{:?}",
                once.get(0, PathPart::Main, &mesh, style, false).unwrap()
            ),
            "缓存与单次移交必须使用同一解析几何"
        );
        assert!(once.paths.is_empty(), "单次移交不得分配路径缓存或克隆顶点");
        assert_eq!(once.preparations, 1);
        let mut recolored = FramePaths::new(1, origin, 0.8, 1.25);
        let brush = Style {
            color: 0x123456,
            opacity: 0.3,
            ..style
        };
        assert_eq!(
            geometry,
            format!(
                "{:?}",
                recolored
                    .get(0, PathPart::Main, &mesh, brush, true)
                    .unwrap()
            ),
            "颜色与透明度在提交时应用，不能改变准备几何"
        );
        for (origin, scale, dpi) in [
            (point(px(-1.75), px(1.5)), 0.8, 1.25),
            (origin, 1.0, 1.25),
            (origin, 0.8, 1.5),
        ] {
            let mut changed = FramePaths::new(1, origin, scale, dpi);
            assert_ne!(
                geometry,
                format!(
                    "{:?}",
                    changed.get(0, PathPart::Main, &mesh, style, true).unwrap()
                ),
                "新帧必须按实际位置、缩放与 DPI 准备抗锯齿边缘"
            );
        }
    }
}
