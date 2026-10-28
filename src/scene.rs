//! 共享的渲染结果：GPUI 画路径/色块，导出使用同一组路径/色块。
//! 马赛克从冻结像素和下层标注采样，按全屏原点对齐，不随裁剪变化。
use crate::{
    annotation::{distance_to_segment, Annotation, HighlighterBlend, Point, Style, Tool},
    capture::Image,
    geometry::Rect,
};
use std::sync::Arc;

pub struct PixelPlane<'a> {
    pub rect: Rect,
    pub bgra: &'a [u8],
}
pub struct SourcePixels<'a> {
    pub canvas: Rect,
    /// 矢量纯色底（如文字贴图）：平面未覆盖处按需取此 BGRA，不预先生成整幅位图。
    pub fill: Option<[u8; 4]>,
    pub planes: Vec<PixelPlane<'a>>,
    pub overlay: Option<PixelPlane<'a>>,
}
/// 冻结像素由不可变 RenderImage 持有；只比较身份，不逐帧扫描整屏像素。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    canvas: Rect,
    fill: Option<[u8; 4]>,
    planes: Vec<(Rect, usize, usize)>,
    overlay: Option<(Rect, usize, usize)>,
}
impl SourceIdentity {
    pub(crate) fn of(source: &SourcePixels<'_>) -> Self {
        let plane = |p: &PixelPlane<'_>| (p.rect, p.bgra.as_ptr() as usize, p.bgra.len());
        Self {
            canvas: source.canvas,
            fill: source.fill,
            planes: source.planes.iter().map(plane).collect(),
            overlay: source.overlay.as_ref().map(plane),
        }
    }
}
impl SourcePixels<'_> {
    pub fn read(&self, region: Rect) -> Image {
        let mut image = Image {
            width: region.width,
            height: region.height,
            bgra: vec![0; (region.width as usize) * (region.height as usize) * 4],
            readback_ms: 0.,
        };
        let fill = self.fill.unwrap_or([0, 0, 0, 255]);
        for p in image.bgra.chunks_exact_mut(4) {
            p.copy_from_slice(&fill);
        }
        for plane in &self.planes {
            if let Some(part) = plane.rect.intersection(region) {
                for y in part.y..part.y + part.height {
                    let src = (((y - plane.rect.y) * plane.rect.width + part.x - plane.rect.x)
                        as usize)
                        * 4;
                    let dst = (((y - region.y) * region.width + part.x - region.x) as usize) * 4;
                    let n = part.width as usize * 4;
                    image.bgra[dst..dst + n].copy_from_slice(&plane.bgra[src..src + n]);
                }
            }
        }
        if let Some(overlay) = &self.overlay {
            copy_plane(&mut image, region, overlay);
        }
        image
    }
}
fn copy_plane(image: &mut Image, region: Rect, plane: &PixelPlane<'_>) {
    if let Some(part) = plane.rect.intersection(region) {
        for y in part.y..part.y + part.height {
            let src =
                (((y - plane.rect.y) * plane.rect.width + part.x - plane.rect.x) as usize) * 4;
            let dst = (((y - region.y) * region.width + part.x - region.x) as usize) * 4;
            let n = part.width as usize * 4;
            image.bgra[dst..dst + n].copy_from_slice(&plane.bgra[src..src + n]);
        }
    }
}

pub struct Block {
    pub rect: Rect,
    pub color: u32,
}
#[derive(Clone)]
pub struct HighlighterTile {
    /// 不重叠的输出范围；同一笔在相邻块边界也只合成一次。
    pub rect: Rect,
    /// 颜色采样保留一像素邻域，分数缩放不会把块边缘当图像边缘钳住。
    pub image_rect: Rect,
    pub image: Arc<Image>,
}
#[derive(Clone)]
pub enum Layer {
    /// 共享聚光灯的后续孔保留索引，不单独重复绘制暗层。
    Empty,
    /// 恢复层保留下层的局部合成结果，按区域替换也能恢复透明底图。
    Restore {
        rect: Rect,
        image: Arc<Image>,
    },
    Raster {
        rect: Rect,
        image: Arc<Image>,
    },
    /// 水印只保留源坐标与样式；按输出/可见 ROI 分块重绘，不缓存全画布透明位图。
    Watermark {
        annotation: Arc<Annotation>,
    },
    LocalFill {
        patches: Arc<[crate::local_fill_scene::FillPatch]>,
    },
    /// 一个复合效果对象对应一个缓存层，离散输出块互不重叠。
    EffectRegions {
        patches: Arc<[crate::smart_effect_scene::EffectPatch]>,
    },
    Spotlight {
        geometry: Arc<crate::spotlight::SpotlightGeometry>,
    },
    Vector {
        arrow: bool,
        mesh: Arc<crate::vector_mesh::VectorMesh>,
        path: Arc<crate::vector_path::VectorPath>,
        segments: Arc<Vec<(Point, Point)>>,
        style: Style,
    },
    /// 图片只是原始底图乘色后的画刷；覆盖率仍由同一个矢量轮廓提供。
    Highlighter {
        mesh: Arc<crate::vector_mesh::VectorMesh>,
        path: Arc<crate::vector_path::VectorPath>,
        segments: Arc<Vec<(Point, Point)>>,
        style: Style,
        tiles: Arc<Vec<HighlighterTile>>,
    },
    Stroke {
        segments: Arc<Vec<(Point, Point)>>,
        style: Style,
    },
    Mosaic {
        blocks: Arc<Vec<Block>>,
    },
}
#[derive(Default)]
pub struct Scene {
    pub layers: Vec<Layer>,
    pub(crate) annotations: Vec<Annotation>,
    pub errors: Vec<String>,
    cached_errors: Vec<(usize, String)>,
    source_identity: Option<SourceIdentity>,
    /// 以草稿快速网格生成的层；下次更新必定重建，提交或导出不会沿用低成本网格。
    pub(crate) draft_layer: Option<usize>,
    /// 同源场景之间共享；拖动马赛克/模糊时下层不变，只为新露出的块合成一次。
    below_cache: Arc<std::sync::Mutex<BelowCache>>,
    /// 实时拖动中按纯平移得到网格的矢量层及其完整质量基准；不再拖动时从基准恢复或完整重建。
    moved: Vec<Arc<MoveBase>>,
    /// 草稿层的分块细分结果；下一帧跨度前缀未变的块直接沿用。
    draft_pieces: Option<Arc<crate::vector_mesh::DraftPieces>>,
}
/// 下层合成结果按全屏对齐分块缓存，以下层 Arc 身份为键；持有层克隆避免地址复用误命中。
#[derive(Default)]
struct BelowCache {
    entries: Vec<(Vec<Layer>, Vec<(Rect, Image)>)>,
}
const BELOW_TILE: u32 = 128;
const BELOW_TILE_LIMIT: usize = 512;
const BELOW_ENTRY_LIMIT: usize = 4;
pub(crate) fn same_layer(a: &Layer, b: &Layer) -> bool {
    match (a, b) {
        (Layer::Empty, Layer::Empty) => true,
        (Layer::Raster { rect: a, image: ai }, Layer::Raster { rect: b, image: bi })
        | (Layer::Restore { rect: a, image: ai }, Layer::Restore { rect: b, image: bi }) => {
            a == b && Arc::ptr_eq(ai, bi)
        }
        (Layer::Watermark { annotation: a }, Layer::Watermark { annotation: b }) => {
            Arc::ptr_eq(a, b)
        }
        (Layer::LocalFill { patches: a }, Layer::LocalFill { patches: b }) => Arc::ptr_eq(a, b),
        (Layer::EffectRegions { patches: a }, Layer::EffectRegions { patches: b }) => {
            Arc::ptr_eq(a, b)
        }
        (Layer::Spotlight { geometry: a }, Layer::Spotlight { geometry: b }) => Arc::ptr_eq(a, b),
        (
            Layer::Vector {
                arrow: a,
                path: ap,
                segments: ag,
                style: ast,
                ..
            },
            Layer::Vector {
                arrow: b,
                path: bp,
                segments: bg,
                style: bst,
                ..
            },
        ) => a == b && ast == bst && Arc::ptr_eq(ap, bp) && Arc::ptr_eq(ag, bg),
        (
            Layer::Highlighter {
                path: ap,
                segments: ag,
                style: ast,
                tiles: at,
                ..
            },
            Layer::Highlighter {
                path: bp,
                segments: bg,
                style: bst,
                tiles: bt,
                ..
            },
        ) => ast == bst && Arc::ptr_eq(ap, bp) && Arc::ptr_eq(ag, bg) && Arc::ptr_eq(at, bt),
        (
            Layer::Stroke {
                segments: a,
                style: ast,
            },
            Layer::Stroke {
                segments: b,
                style: bst,
            },
        ) => ast == bst && Arc::ptr_eq(a, b),
        (Layer::Mosaic { blocks: a }, Layer::Mosaic { blocks: b }) => Arc::ptr_eq(a, b),
        _ => false,
    }
}
/// 结果只由对象自身与源图决定、不读取下层合成的层；上层对象未变时可直接沿用。
fn independent_of_below(a: &Annotation) -> bool {
    !(a.tool.effect()
        || a.effect_regions.is_some()
        || matches!(
            a.tool,
            Tool::PixelEraser | Tool::LocalFill | Tool::Spotlight
        )
        || (matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
            && a.style.highlighter_blend == HighlighterBlend::Multiply))
}
impl Scene {
    pub(crate) fn first_error(&self) -> Option<&str> {
        self.errors
            .first()
            .or_else(|| self.cached_errors.first().map(|(_, error)| error))
            .map(String::as_str)
    }
    #[cfg(test)]
    pub fn prepare(annotations: &[Annotation], source: &SourcePixels<'_>) -> Self {
        Self::default().updated(annotations, source)
    }
    pub fn updated(&self, annotations: &[Annotation], source: &SourcePixels<'_>) -> Self {
        self.updated_with_draft(annotations, source, None)
    }
    /// `draft` 是正在拖动的自由笔迹在 `annotations` 中的下标；仅用于逐帧预览。
    pub(crate) fn updated_with_draft(
        &self,
        annotations: &[Annotation],
        source: &SourcePixels<'_>,
        draft: Option<usize>,
    ) -> Self {
        self.updated_live(annotations, source, draft, &[])
    }
    /// `moving` 是编辑手势中正在拖动／变换的对象下标：纯平移的矢量对象逐帧只平移
    /// 完整质量网格。手势结束后的更新不再列出它们，按完整质量重建，导出不沿用平移网格。
    pub(crate) fn updated_live(
        &self,
        annotations: &[Annotation],
        source: &SourcePixels<'_>,
        draft: Option<usize>,
        moving: &[usize],
    ) -> Self {
        let identity = SourceIdentity::of(source);
        let same_source = self.source_identity.as_ref() == Some(&identity);
        // 未变化的下层共享路径与色块。修改上层画笔时不会重新采样下层马赛克。
        let mut prefix = self
            .annotations
            .iter()
            .zip(annotations)
            .take_while(|(a, b)| a == b)
            .count();
        if !same_source {
            prefix = 0;
        }
        if let Some(index) = self.draft_layer {
            prefix = prefix.min(index);
        }
        for base in &self.moved {
            prefix = prefix.min(base.index);
        }
        let first_spotlight = annotations.iter().position(|a| a.tool == Tool::Spotlight);
        let old_first_spotlight = self
            .annotations
            .iter()
            .position(|a| a.tool == Tool::Spotlight);
        let same_spotlights = self
            .annotations
            .iter()
            .filter(|a| a.tool == Tool::Spotlight)
            .eq(annotations.iter().filter(|a| a.tool == Tool::Spotlight));
        if !same_spotlights {
            // 后面的孔也改变共享层；所有采样下层的效果必须从组首失效。
            for index in [first_spotlight, old_first_spotlight].into_iter().flatten() {
                prefix = prefix.min(index);
            }
        }
        let mut scene = Self {
            layers: self.layers[..prefix].to_vec(),
            annotations: annotations.to_vec(),
            errors: Vec::new(),
            cached_errors: self
                .cached_errors
                .iter()
                .filter(|(i, _)| *i < prefix)
                .cloned()
                .collect(),
            source_identity: Some(identity),
            draft_layer: draft.filter(|&index| index < annotations.len()),
            below_cache: if same_source {
                self.below_cache.clone()
            } else {
                Default::default()
            },
            moved: Vec::new(),
            draft_pieces: None,
        };
        for (i, a) in annotations.iter().enumerate().skip(prefix) {
            if same_source {
                if let Some((layer, base)) = self.moved_layer(i, a, draft, moving) {
                    scene.layers.push(layer);
                    scene.moved.extend(base);
                    continue;
                }
            }
            // 拖动下层对象时，上层不采样下层的对象（矢量、文字、形状）原样沿用，不逐帧重建网格。
            if same_source
                && draft != Some(i)
                && self.draft_layer != Some(i)
                && !self.moved.iter().any(|base| base.index == i)
                && independent_of_below(a)
                && self.annotations.get(i) == Some(a)
            {
                if let Some(layer) = self.layers.get(i) {
                    scene.layers.push(layer.clone());
                    scene.cached_errors.extend(
                        self.cached_errors
                            .iter()
                            .filter(|(index, _)| *index == i)
                            .cloned(),
                    );
                    continue;
                }
            }
            // 清空副栏保留可撤销的空层，但不能为它生成画布大小的文字位图。
            if a.tool == Tool::Watermark && a.text.trim().is_empty() {
                scene.layers.push(Layer::Empty);
                continue;
            }
            if a.tool == Tool::Watermark {
                let projection = crate::output_projection::OutputProjection {
                    source: source.canvas,
                    width: source.canvas.width,
                    height: source.canvas.height,
                    sx: 1.,
                    sy: 1.,
                    origin: Point { x: 0., y: 0. },
                };
                match crate::projected_text::validate(a, &projection) {
                    Ok(()) => scene.layers.push(Layer::Watermark {
                        annotation: Arc::new(a.clone()),
                    }),
                    Err(error) => {
                        scene.cached_errors.push((i, error.to_string()));
                        scene.layers.push(Layer::Empty);
                    }
                }
                continue;
            }
            // 本次重建或缓存前缀已有失败时，不能继续采样不完整的下层。
            // errors 在结束时汇总；这里直接读取已记录的错误，避免越过失败层读旧源。
            let samples_pixels = a.tool.effect()
                || a.tool == Tool::PixelEraser
                || (matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
                    && a.style.highlighter_blend == HighlighterBlend::Multiply);
            if samples_pixels && !scene.cached_errors.is_empty() {
                scene.layers.push(Layer::Empty);
                continue;
            }
            if a.effect_regions.is_some() {
                match crate::smart_effect_scene::prepare(a, source, &scene) {
                    Ok(patches) => scene.layers.push(Layer::EffectRegions { patches }),
                    Err(error) => {
                        scene.cached_errors.push((i, error.to_string()));
                        scene.layers.push(Layer::Empty);
                    }
                }
                continue;
            }
            if a.tool == Tool::LocalFill {
                match crate::local_fill_scene::prepare(a, source, &scene) {
                    Ok(Some(prepared)) => scene.layers.push(Layer::LocalFill {
                        patches: prepared.patches,
                    }),
                    Ok(None) => scene.layers.push(Layer::Empty),
                    Err(error) => {
                        scene.cached_errors.push((i, error.to_string()));
                        scene.layers.push(Layer::Empty);
                    }
                }
                continue;
            }
            if a.tool == Tool::Spotlight {
                if first_spotlight == Some(i) {
                    match crate::spotlight::SpotlightGeometry::build(annotations, source.canvas) {
                        Ok(geometry) => scene.layers.push(Layer::Spotlight {
                            geometry: Arc::new(geometry),
                        }),
                        Err(error) => {
                            scene.cached_errors.push((i, error.to_string()));
                            scene.layers.push(Layer::Empty);
                        }
                    }
                } else {
                    scene.layers.push(Layer::Empty);
                }
                continue;
            }
            if a.tool == Tool::PixelEraser {
                match crate::raster::try_restore(a, source, &scene) {
                    Ok(Some((rect, image))) => scene.layers.push(Layer::Restore {
                        rect,
                        image: Arc::new(image),
                    }),
                    Ok(None) => scene.layers.push(Layer::Empty),
                    Err(error) => {
                        scene.cached_errors.push((i, error.to_string()));
                        scene.layers.push(Layer::Empty);
                    }
                }
                continue;
            }
            if matches!(
                a.tool,
                Tool::Pen
                    | Tool::Arrow
                    | Tool::Polyline
                    | Tool::Rectangle
                    | Tool::Ellipse
                    | Tool::Triangle
                    | Tool::Diamond
                    | Tool::Star
                    | Tool::Highlighter
                    | Tool::HighlighterRect
            ) {
                let path = a.vector_path();
                // 填充矩形与高亮框不描边；线宽/虚线元数据仍供切回空心时使用。
                let fill_only = a.fill_only();
                let style = if fill_only {
                    Style {
                        width: 0.,
                        filled: true,
                        ..a.style
                    }
                } else {
                    a.style
                };
                let segments = Arc::new(if fill_only {
                    Vec::new()
                } else {
                    path.flattened()
                });
                let mesh = match if fill_only {
                    crate::vector_mesh::VectorMesh::fill_contours(&path.fills)
                } else if draft == Some(i) {
                    crate::vector_mesh::VectorMesh::build_draft_pieces(
                        &path,
                        style,
                        a.tool == Tool::Arrow,
                        self.draft_pieces.as_deref(),
                    )
                    .map(|(mesh, pieces)| {
                        scene.draft_pieces = Some(Arc::new(pieces));
                        mesh
                    })
                } else if matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
                    && style.highlighter_blend == HighlighterBlend::Multiply
                {
                    // 画刷路径不走 GPU 覆盖率 MAX 合并，重叠三角形会叠加，仍需并集网格。
                    crate::vector_mesh::VectorMesh::build(&path, style, false)
                } else {
                    crate::vector_mesh::VectorMesh::build_display(
                        &path,
                        style,
                        a.tool == Tool::Arrow,
                    )
                } {
                    Ok(mesh) => mesh,
                    Err(e) => {
                        scene.cached_errors.push((i, e.to_string()));
                        crate::vector_mesh::VectorMesh::default()
                    }
                };
                if matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
                    && style.highlighter_blend == HighlighterBlend::Multiply
                {
                    let Some(rect) = crate::raster::region(a, source.canvas) else {
                        scene.layers.push(Layer::Empty);
                        continue;
                    };
                    // 下层对象修改不改变冻结底图；同一区域与颜色可共享画刷。
                    let mut tiles = Vec::new();
                    let tint = [
                        style.color as u8,
                        (style.color >> 8) as u8,
                        (style.color >> 16) as u8,
                    ];
                    // 固定全图网格：小幅拖动只重建矢量，画刷 Arc 与 GPU 纹理不重新上传。
                    for y in (rect.y / 2048 * 2048..rect.y + rect.height).step_by(2048) {
                        for x in (rect.x / 2048 * 2048..rect.x + rect.width).step_by(2048) {
                            let Some(tile) = (Rect {
                                x,
                                y,
                                width: 2048,
                                height: 2048,
                            })
                            .intersection(source.canvas) else {
                                continue;
                            };
                            let image_rect = Rect {
                                x: tile.x.saturating_sub(1),
                                y: tile.y.saturating_sub(1),
                                width: tile.x + tile.width + 1 - tile.x.saturating_sub(1),
                                height: tile.y + tile.height + 1 - tile.y.saturating_sub(1),
                            }
                            .intersection(source.canvas)
                            .expect("brush core is inside canvas");
                            let cached = scene
                                .layers
                                .iter()
                                .chain(self.layers.iter().filter(|_| same_source))
                                .find_map(|layer| match layer {
                                    Layer::Highlighter {
                                        tiles,
                                        style: old_style,
                                        ..
                                    } if old_style.color == style.color => tiles
                                        .iter()
                                        .find(|old| {
                                            old.rect == tile && old.image_rect == image_rect
                                        })
                                        .map(|old| old.image.clone()),
                                    _ => None,
                                });
                            let image = cached.unwrap_or_else(|| {
                                let mut image = source.read(image_rect);
                                for pixel in image.bgra.chunks_exact_mut(4) {
                                    for channel in 0..3 {
                                        pixel[channel] =
                                            (pixel[channel] as u32 * tint[channel] as u32 / 255)
                                                as u8;
                                    }
                                }
                                Arc::new(image)
                            });
                            tiles.push(HighlighterTile {
                                rect: tile,
                                image_rect,
                                image,
                            });
                        }
                    }
                    scene.layers.push(Layer::Highlighter {
                        mesh: Arc::new(mesh),
                        path: Arc::new(path),
                        segments,
                        style,
                        tiles: Arc::new(tiles),
                    });
                    continue;
                }
                scene.layers.push(Layer::Vector {
                    arrow: a.tool == Tool::Arrow,
                    mesh: Arc::new(mesh),
                    path: Arc::new(path),
                    segments,
                    style,
                });
                continue;
            }
            if a.tool.text()
                || a.tool == Tool::Serial
                || a.tool.effect()
                    && (a.tool != Tool::Mosaic
                        || a.rotation != 0.
                        || (a.supports_radius() && a.corner_radius() > 0.))
                || matches!(a.tool, Tool::Highlighter | Tool::HighlighterRect)
                || (a.style.filled
                    && (a.tool.closed_shape() || a.tool == Tool::Polyline))
                || (a.tool != Tool::Mosaic && (a.style.dashed || a.style.opacity < 1.))
            {
                let result = if a.tool.text() || a.tool == Tool::Serial {
                    crate::raster::text(a, source.canvas)
                } else if a.tool.effect() {
                    crate::raster::try_effect(a, source, &scene)
                } else {
                    Ok(crate::raster::shape(a, source.canvas))
                };
                let result = match result {
                    Ok(value) => value,
                    Err(error) => {
                        scene.cached_errors.push((i, error.to_string()));
                        scene.layers.push(Layer::Empty);
                        continue;
                    }
                };
                let (rect, image) = result
                    .unwrap_or_else(|| (Rect::full(1, 1), crate::raster::empty(Rect::full(1, 1))));
                scene.layers.push(Layer::Raster {
                    rect,
                    image: Arc::new(image),
                });
                continue;
            }
            if a.tool != Tool::Mosaic {
                scene.layers.push(Layer::Stroke {
                    segments: Arc::new(a.segments()),
                    style: a.style,
                });
                continue;
            }
            let Some(rect) = a.bounds().pixels(source.canvas) else {
                scene.layers.push(Layer::Mosaic {
                    blocks: Arc::new(vec![]),
                });
                continue;
            };
            let bs = a.style.block_size.clamp(2, 64);
            let left = rect.x / bs * bs;
            let top = rect.y / bs * bs;
            let right = (rect.x + rect.width).div_ceil(bs) * bs;
            let bottom = (rect.y + rect.height).div_ceil(bs) * bs;
            let grid = Rect {
                x: left,
                y: top,
                width: right.min(source.canvas.width) - left,
                height: bottom.min(source.canvas.height) - top,
            };
            // 只创建马赛克覆盖区域的临时位图。移动/缩放结束后缓存色块，不在每帧重新采样。
            let below = match scene.read_composited(source, grid) {
                Ok(below) => below,
                Err(error) => {
                    scene.cached_errors.push((i, error.to_string()));
                    scene.layers.push(Layer::Empty);
                    continue;
                }
            };
            let mut blocks = Vec::new();
            for y in (top..top + grid.height).step_by(bs as usize) {
                for x in (left..left + grid.width).step_by(bs as usize) {
                    let cell = Rect {
                        x,
                        y,
                        width: bs.min(left + grid.width - x),
                        height: bs.min(top + grid.height - y),
                    };
                    let mut sum = [0_u64; 3];
                    for cy in cell.y..cell.y + cell.height {
                        for cx in cell.x..cell.x + cell.width {
                            let offset = (((cy - top) * grid.width + cx - left) as usize) * 4;
                            for (channel, total) in sum.iter_mut().enumerate() {
                                *total += below.bgra[offset + channel] as u64;
                            }
                        }
                    }
                    let n = cell.width as u64 * cell.height as u64;
                    let color = (sum[0] / n) as u32
                        | ((sum[1] / n) as u32) << 8
                        | ((sum[2] / n) as u32) << 16;
                    if let Some(part) = cell.intersection(rect) {
                        blocks.push(Block { rect: part, color });
                    }
                }
            }
            scene.layers.push(Layer::Mosaic {
                blocks: Arc::new(blocks),
            });
        }
        scene.errors = scene.cached_errors.iter().map(|(_, e)| e.clone()).collect();
        scene
    }
    /// 读取 `region` 内源图叠加本场景全部层的结果，与 `source.read` + `try_apply` 逐像素一致。
    /// 合成按全屏对齐分块缓存；下层不变时拖动效果对象只合成新露出的块。
    pub(crate) fn read_composited(
        &self,
        source: &SourcePixels<'_>,
        region: Rect,
    ) -> crate::Result<Image> {
        let fallback = || -> crate::Result<Image> {
            let mut image = source.read(region);
            self.try_apply(&mut image, region)?;
            Ok(image)
        };
        if region.width == 0
            || region.height == 0
            || region.intersection(source.canvas) != Some(region)
            || self.first_error().is_some()
        {
            return fallback();
        }
        if self
            .layers
            .iter()
            .all(|layer| matches!(layer, Layer::Empty))
        {
            return Ok(source.read(region));
        }
        let Ok(mut cache) = self.below_cache.lock() else {
            return fallback();
        };
        let Some(index) = cache.entries.iter().position(|(layers, _)| {
            layers.len() == self.layers.len()
                && layers
                    .iter()
                    .zip(&self.layers)
                    .all(|(a, b)| same_layer(a, b))
        }) else {
            // 首次只登记下层身份并按原范围合成；同一下层再次采样（拖动中）才分块缓存。
            // 拖动下层时上层效果每帧换键，因此不会为它们多合成整块或占用内存。
            if cache.entries.len() >= BELOW_ENTRY_LIMIT {
                cache.entries.remove(0);
            }
            cache.entries.push((self.layers.clone(), Vec::new()));
            drop(cache);
            return fallback();
        };
        // 最近使用的放到末尾，换键的上层效果只挤掉更旧的登记。
        let entry = cache.entries.remove(index);
        cache.entries.push(entry);
        let tiles = &mut cache.entries.last_mut().expect("just pushed").1;
        let mut image = Image {
            width: region.width,
            height: region.height,
            bgra: vec![0; region.width as usize * region.height as usize * 4],
            readback_ms: 0.,
        };
        let mut used = Vec::new();
        for y in (region.y / BELOW_TILE * BELOW_TILE..region.y + region.height)
            .step_by(BELOW_TILE as usize)
        {
            for x in (region.x / BELOW_TILE * BELOW_TILE..region.x + region.width)
                .step_by(BELOW_TILE as usize)
            {
                let Some(tile) = (Rect {
                    x,
                    y,
                    width: BELOW_TILE,
                    height: BELOW_TILE,
                })
                .intersection(source.canvas) else {
                    continue;
                };
                let slot = match tiles.iter().position(|(rect, _)| *rect == tile) {
                    Some(slot) => slot,
                    None => {
                        let mut pixels = source.read(tile);
                        self.try_apply(&mut pixels, tile)?;
                        tiles.push((tile, pixels));
                        tiles.len() - 1
                    }
                };
                let (tile, pixels) = &tiles[slot];
                if let Some(part) = tile.intersection(region) {
                    let n = part.width as usize * 4;
                    for row in part.y..part.y + part.height {
                        let src = (((row - tile.y) * tile.width + part.x - tile.x) as usize) * 4;
                        let dst =
                            (((row - region.y) * region.width + part.x - region.x) as usize) * 4;
                        image.bgra[dst..dst + n].copy_from_slice(&pixels.bgra[src..src + n]);
                    }
                }
                used.push(*tile);
            }
        }
        // 超限时先丢弃本次未用到的块；整屏大区域本身超限则只保留本次的块。
        if tiles.len() > BELOW_TILE_LIMIT {
            tiles.retain(|(rect, _)| used.contains(rect));
        }
        Ok(image)
    }
    /// 旧核心/原生夹具的兼容入口；正式输出和下层采样必须传播 try_apply 的错误。
    #[doc(hidden)]
    pub fn apply(&self, image: &mut Image, region: Rect) {
        self.try_apply(image, region)
            .expect("验收场景合成失败，不能继续读取部分输出");
    }
    pub fn try_apply(&self, image: &mut Image, region: Rect) -> crate::Result<()> {
        if let Some(error) = self.first_error() {
            return Err(error.into());
        }
        let len = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("场景合成像素尺寸溢出")?;
        if (image.width, image.height) != (region.width, region.height)
            || image.bgra.len() != len
            || region.width == 0
            || region.height == 0
        {
            return Err("场景合成范围与目标图片不一致".into());
        }
        let projection = crate::output_projection::OutputProjection {
            source: region,
            width: image.width,
            height: image.height,
            sx: 1.,
            sy: 1.,
            origin: Point { x: 0., y: 0. },
        };
        // 所有纯检查在第一笔像素修改前完成；GDI 失败由调用者丢弃未提交的结果。
        for layer in &self.layers {
            if let Layer::Watermark { annotation } = layer {
                crate::projected_text::validate(annotation, &projection)?;
            }
        }
        for layer in &self.layers {
            match layer {
                Layer::Empty => {}
                Layer::Restore {
                    rect,
                    image: raster,
                } => crate::raster::replace(image, region, raster, *rect),
                Layer::Raster {
                    rect,
                    image: raster,
                } => crate::raster::composite(image, region, raster, *rect),
                Layer::Watermark { annotation } => {
                    crate::projected_text::paint(annotation, image, &projection)?;
                }
                Layer::LocalFill { patches } => {
                    for patch in patches.iter() {
                        crate::raster::composite(image, region, &patch.image, patch.rect);
                    }
                }
                Layer::EffectRegions { patches } => {
                    for patch in patches.iter() {
                        crate::raster::composite(image, region, &patch.image, patch.rect);
                    }
                }
                Layer::Spotlight { geometry } => geometry.apply(image, region),
                Layer::Vector {
                    path,
                    segments,
                    style,
                    arrow,
                    ..
                } => crate::raster::paint_vector(image, region, path, segments, *style, *arrow),
                Layer::Highlighter {
                    path,
                    segments,
                    style,
                    tiles,
                    ..
                } => {
                    for tile in tiles.iter() {
                        crate::raster::paint_vector_image(
                            image,
                            region,
                            path,
                            segments,
                            *style,
                            tile.rect,
                            tile.image_rect,
                            &tile.image,
                        );
                    }
                }
                Layer::Stroke { segments, style } => {
                    for &(a, b) in segments.iter() {
                        stroke(image, region, a, b, *style);
                    }
                }
                Layer::Mosaic { blocks } => {
                    for block in blocks.iter() {
                        if let Some(part) = block.rect.intersection(region) {
                            let color = [
                                block.color as u8,
                                (block.color >> 8) as u8,
                                (block.color >> 16) as u8,
                                255,
                            ];
                            for y in part.y..part.y + part.height {
                                let offset = (((y - region.y) * region.width + part.x - region.x)
                                    as usize)
                                    * 4;
                                for pixel in image.bgra[offset..offset + part.width as usize * 4]
                                    .chunks_exact_mut(4)
                                {
                                    pixel.copy_from_slice(&color);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) fn with_prepared_fill(
        &self,
        annotation: &Annotation,
        prepared: crate::local_fill_scene::PreparedFill,
        source: &SourcePixels<'_>,
    ) -> crate::Result<Self> {
        if annotation.tool != Tool::LocalFill
            || !annotation.valid()
            || prepared.stats.filled == 0
            || prepared.stats.filled != prepared.stats.requested
            || self.source_identity.as_ref() != Some(&SourceIdentity::of(source))
            || !self.errors.is_empty()
        {
            return Err("局部填充采样基线已改变".into());
        }
        let mut annotations = self.annotations.clone();
        annotations.push(annotation.clone());
        let mut layers = self.layers.clone();
        layers.push(Layer::LocalFill {
            patches: prepared.patches,
        });
        Ok(Self {
            layers,
            annotations,
            errors: self.errors.clone(),
            cached_errors: self.cached_errors.clone(),
            source_identity: self.source_identity.clone(),
            draft_layer: self.draft_layer,
            below_cache: self.below_cache.clone(),
            moved: self.moved.clone(),
            draft_pieces: self.draft_pieces.clone(),
        })
    }
    /// 拖动中的矢量对象只差同一平移量时平移基准网格，跳过细分与并集；路径与线段仍按
    /// 当前标注精确生成，1× 导出不受影响。回到基准位置（取消拖动）时直接沿用基准层。
    fn moved_layer(
        &self,
        i: usize,
        a: &Annotation,
        draft: Option<usize>,
        moving: &[usize],
    ) -> Option<(Layer, Option<Arc<MoveBase>>)> {
        let live = moving.contains(&i) && draft != Some(i);
        let base = match self.moved.iter().find(|base| base.index == i) {
            Some(base) => base.clone(),
            // 首帧以上一帧的完整质量层为基准；草稿网格与失败层不能作基准。
            None if live
                && self.draft_layer != Some(i)
                && !self.cached_errors.iter().any(|(index, _)| *index == i) =>
            {
                let layer = self.layers.get(i)?;
                if !matches!(layer, Layer::Vector { .. }) {
                    return None;
                }
                Arc::new(MoveBase {
                    index: i,
                    annotation: self.annotations.get(i)?.clone(),
                    layer: layer.clone(),
                })
            }
            None => return None,
        };
        if base.annotation == *a {
            return Some((base.layer.clone(), None));
        }
        if !live {
            return None;
        }
        let (dx, dy) = pure_translation(&base.annotation, a)?;
        let Layer::Vector {
            arrow, mesh, style, ..
        } = &base.layer
        else {
            return None;
        };
        let path = a.vector_path();
        let segments = if a.fill_only() {
            Vec::new()
        } else {
            path.flattened()
        };
        let mesh = mesh.translated(dx, dy, &path);
        Some((
            Layer::Vector {
                arrow: *arrow,
                mesh: Arc::new(mesh),
                path: Arc::new(path),
                segments: Arc::new(segments),
                style: *style,
            },
            Some(base),
        ))
    }
}
fn stroke(image: &mut Image, region: Rect, a: Point, b: Point, style: Style) {
    let radius = style.width / 2.;
    let left = (a.x.min(b.x) - radius - 1.).floor().max(region.x as f32) as u32;
    let top = (a.y.min(b.y) - radius - 1.).floor().max(region.y as f32) as u32;
    let right = (a.x.max(b.x) + radius + 1.)
        .ceil()
        .max(0.)
        .min((region.x + region.width) as f32) as u32;
    let bottom = (a.y.max(b.y) + radius + 1.)
        .ceil()
        .max(0.)
        .min((region.y + region.height) as f32) as u32;
    for y in top..bottom {
        for x in left..right {
            let coverage = (radius + 0.5
                - distance_to_segment(
                    Point {
                        x: x as f32 + 0.5,
                        y: y as f32 + 0.5,
                    },
                    a,
                    b,
                ))
            .clamp(0., 1.);
            if coverage > 0. {
                let offset = (((y - region.y) * image.width + x - region.x) as usize) * 4;
                let color = [
                    style.color as u8,
                    (style.color >> 8) as u8,
                    (style.color >> 16) as u8,
                ];
                crate::raster::over_pixel(&mut image.bgra[offset..offset + 4], &color, coverage);
            }
        }
    }
}
/// 拖动开始前按完整质量生成的矢量层；拖动中逐帧只平移它的网格，不累积舍入。
struct MoveBase {
    index: usize,
    annotation: Annotation,
    layer: Layer,
}
/// 除所有点同一平移量外完全相同时返回该平移量；容许逐点 f32 加法的舍入。
fn pure_translation(from: &Annotation, to: &Annotation) -> Option<(f32, f32)> {
    if from.tool != to.tool
        || from.style != to.style
        || from.text != to.text
        || from.rotation != to.rotation
        || from.fill_geometry != to.fill_geometry
        || from.effect_regions.is_some()
        || to.effect_regions.is_some()
        || from.points.len() != to.points.len()
    {
        return None;
    }
    let (a, b) = (from.points.first()?, to.points[0]);
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    if !dx.is_finite() || !dy.is_finite() {
        return None;
    }
    from.points
        .iter()
        .zip(&to.points)
        .all(|(p, q)| {
            let magnitude = p.x.abs().max(p.y.abs()).max(q.x.abs()).max(q.y.abs());
            let tolerance = 1e-3_f32.max(magnitude * 4. * f32::EPSILON);
            ((q.x - p.x) - dx).abs() <= tolerance && ((q.y - p.y) - dy).abs() <= tolerance
        })
        .then_some((dx, dy))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mark(tool: Tool, a: (f32, f32), b: (f32, f32), width: f32) -> Annotation {
        let mut a = Annotation::new(
            tool,
            Point { x: a.0, y: a.1 },
            Style {
                width,
                block_size: 4,
                ..Style::default()
            },
        );
        a.update(Point { x: b.0, y: b.1 });
        a
    }
    fn pixels() -> Vec<u8> {
        (0..16 * 12)
            .flat_map(|i| [(i % 16 * 13) as u8, (i / 16 * 17) as u8, 40, 255])
            .collect()
    }
    fn vector_mesh(layer: &Layer) -> &Arc<crate::vector_mesh::VectorMesh> {
        match layer {
            Layer::Vector { mesh, .. } => mesh,
            _ => panic!("不是矢量层"),
        }
    }
    /// 来回涂抹、自交且半透明的一笔，并集网格与草稿网格拓扑明显不同。
    fn scribble(x: f32, y: f32, turns: usize) -> Annotation {
        let mut pen = mark(Tool::Pen, (x, y), (x + 180., y + 2.), 5.);
        for k in 0..turns {
            pen.update(Point {
                x: x + (k % 2) as f32 * 180. + k as f32 * 0.37,
                y: y + 4. + k as f32 * 2.5,
            });
        }
        pen.style.opacity = 0.7;
        pen
    }
    #[test]
    fn dragging_vector_translates_base_mesh_and_release_matches_fresh_scene() {
        let canvas = Rect::full(320, 240);
        let bytes = [200_u8, 210, 220, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
        };
        let pen = scribble(20., 40., 30);
        let mut items = vec![
            mark(Tool::Rectangle, (10., 10.), (250., 200.), 5.),
            pen.clone(),
            mark(Tool::Arrow, (300., 230.), (30., 40.), 4.),
        ];
        let render = |scene: &Scene| {
            let mut image = source.read(canvas);
            scene.try_apply(&mut image, canvas).unwrap();
            image.bgra
        };
        let start = Scene::prepare(&items, &source);
        let base = vector_mesh(&start.layers[1]).clone();
        let mut scene = start.updated(&items, &source);
        for step in 1..=5 {
            items[1] = pen.clone();
            items[1].translate(7.25 * step as f32, -3.5 * step as f32);
            let next = scene.updated_live(&items, &source, None, &[1]);
            // 只平移完整质量的基准网格，不重新细分或求并集。
            let mesh = vector_mesh(&next.layers[1]);
            assert_eq!(mesh.indices, base.indices);
            assert_eq!(mesh.edges, base.edges);
            assert_eq!(next.moved.len(), 1);
            assert!(same_layer(&scene.layers[0], &next.layers[0]));
            assert!(same_layer(&scene.layers[2], &next.layers[2]));
            // 1× 合成只读路径与线段，拖动中也与全新 Scene 逐像素一致。
            assert!(
                render(&next) == render(&Scene::prepare(&items, &source)),
                "step {step}"
            );
            scene = next;
        }
        // 松手：同一位置不再列为拖动，按完整质量重建，网格与全新 Scene 逐位一致。
        let released = scene.updated_live(&items, &source, None, &[]);
        assert!(released.moved.is_empty());
        let fresh = Scene::prepare(&items, &source);
        let (a, b) = (
            vector_mesh(&released.layers[1]),
            vector_mesh(&fresh.layers[1]),
        );
        assert_eq!(a.vertices, b.vertices);
        assert_eq!(a.indices, b.indices);
        assert_eq!(a.normals, b.normals);
        assert!(same_layer(&scene.layers[2], &released.layers[2]));
        // 取消：回到基准位置直接沿用基准层。
        items[1] = pen.clone();
        let cancelled = scene.updated_live(&items, &source, None, &[]);
        assert!(Arc::ptr_eq(vector_mesh(&cancelled.layers[1]), &base));
        assert!(cancelled.moved.is_empty());
        // 非纯平移（改动单点）完整重建。
        items[1].points[3].x += 1.;
        let reshaped = scene.updated_live(&items, &source, None, &[1]);
        assert!(reshaped.moved.is_empty());
        let fresh = Scene::prepare(&items, &source);
        assert_eq!(
            vector_mesh(&reshaped.layers[1]).vertices,
            vector_mesh(&fresh.layers[1]).vertices
        );
    }
    #[test]
    fn editor_drag_lists_live_object_until_release() {
        let canvas = Rect::full(400, 300);
        let bytes = [128_u8, 128, 128, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
        };
        let mut editor = crate::editor::Editor::new(canvas, Some(canvas));
        editor.select_tool(Tool::Pen);
        editor.pointer_down((50, 50), 4.);
        for i in 0..40 {
            editor.pointer_move((50 + (i % 2) * 200, 52 + i * 3));
        }
        editor.pointer_up((250, 170));
        let mut scene = Scene::prepare(&editor.annotations(), &source);
        editor.select_tool(Tool::Edit);
        editor.pointer_down((150, 51), 4.);
        for k in 1..=6 {
            editor.pointer_move((150 + k * 9, 51 + k * 5));
            let moving = editor.live_object_indices();
            assert_eq!(moving, vec![0]);
            scene = scene.updated_live(
                &editor.annotations(),
                &source,
                editor.live_stroke_draft_index(),
                &moving,
            );
        }
        assert_eq!(scene.moved.len(), 1);
        editor.pointer_up((204, 81));
        assert!(editor.live_object_indices().is_empty());
        let released = scene.updated_live(
            &editor.annotations(),
            &source,
            None,
            &editor.live_object_indices(),
        );
        assert!(released.moved.is_empty());
        let fresh = Scene::prepare(&editor.annotations(), &source);
        assert_eq!(
            vector_mesh(&released.layers[0]).vertices,
            vector_mesh(&fresh.layers[0]).vertices
        );
    }
    /// 拖动长涂抹笔迹的逐帧 Scene 成本：完整重建对平移基准网格。
    #[test]
    #[ignore]
    fn drag_scribble_frame_cost() {
        let canvas = Rect::full(2560, 1440);
        let bytes = vec![128u8; 2560 * 1440 * 4];
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
        };
        let mut pen = mark(Tool::Pen, (200., 300.), (201., 300.), 4.);
        for i in 0..400 {
            let t = i as f32 / 400.;
            let x = 200. + 2000. * (0.5 - 0.5 * (t * 8. * std::f32::consts::PI).cos());
            pen.update(Point {
                x: x.round(),
                y: (300. + 200. * t).round(),
            });
        }
        let items = vec![pen.clone()];
        let scene = Scene::prepare(&items, &source);
        let frames = 20;
        for (name, moving) in [("rebuild", &[][..]), ("translate", &[0][..])] {
            let mut scene = scene.updated(&items, &source);
            let started = std::time::Instant::now();
            for k in 1..=frames {
                let mut moved = pen.clone();
                moved.translate(k as f32 * 3., k as f32 * 2.);
                scene = scene.updated_live(&[moved], &source, None, moving);
            }
            eprintln!(
                "{name}: {:.3} ms/frame",
                started.elapsed().as_secs_f64() * 1000. / frames as f64
            );
        }
        let started = std::time::Instant::now();
        for _ in 0..frames {
            let path = std::hint::black_box(pen.vector_path());
            std::hint::black_box(path.flattened());
        }
        eprintln!(
            "path+flatten: {:.3} ms",
            started.elapsed().as_secs_f64() * 1000. / frames as f64
        );
    }
    /// 真实采样密度下每笔的几何量与各阶段耗时、逐帧草稿成本、在大量笔迹上拖动马赛克。
    #[test]
    #[ignore]
    fn hand_drawn_pencil_cost() {
        use crate::vector_mesh::{tests::hand_samples, VectorMesh};
        let canvas = Rect::full(2560, 1440);
        let bytes = vec![128u8; 2560 * 1440 * 4];
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
        };
        let style = Style {
            width: 4.,
            ..Style::default()
        };
        let samples = hand_samples(300., 300., 1500, 1);
        let mut pen = Annotation::new(Tool::Pen, samples[0], style);
        for &p in &samples[1..] {
            pen.update(p);
        }
        let ms = |s: std::time::Instant| s.elapsed().as_secs_f64() * 1000.;
        let started = std::time::Instant::now();
        let path = pen.vector_path();
        let fit_ms = ms(started);
        let cubics = path
            .spans
            .iter()
            .filter(|s| matches!(s, crate::vector_path::Span::Cubic { .. }))
            .count();
        let segments = path.flattened().len();
        let started = std::time::Instant::now();
        let draft = VectorMesh::build_draft(&path, style, false).unwrap();
        let draft_ms = ms(started);
        let started = std::time::Instant::now();
        let full = VectorMesh::build(&path, style, false).unwrap();
        let full_ms = ms(started);
        eprintln!(
            "one stroke: samples {} (stored {}), fit {fit_ms:.2} ms -> spans {} (cubic {cubics}), flattened {segments}",
            samples.len(),
            pen.points.len(),
            path.spans.len()
        );
        for (name, mesh, t) in [("draft", &draft, draft_ms), ("union", &full, full_ms)] {
            eprintln!(
                "  {name}: {t:.2} ms, verts {} tris {} edges {} -> gpu path verts {}",
                mesh.vertices.len(),
                mesh.indices.len() / 3,
                mesh.edges.len(),
                (mesh.indices.len() / 3 + mesh.edges.len() * 2) * 3
            );
        }
        // 逐帧草稿：每帧到达 8 个样本（1000Hz 鼠标、约 120Hz 刷新）。
        let mut editor = crate::editor::Editor::new(canvas, Some(canvas));
        editor.select_tool(Tool::Pen);
        editor.set_style(style);
        let strokes = 20;
        let mut scene = Arc::new(Scene::prepare(&editor.annotations(), &source));
        let mut last_stroke = (0., 0., 0., 0);
        for s in 0..strokes {
            let samples = hand_samples(
                200. + (s % 5) as f32 * 450.,
                200. + (s / 5) as f32 * 260.,
                1500,
                s as u32 + 1,
            );
            let at = |p: Point| (p.x as i32, p.y as i32);
            editor.pointer_down(at(samples[0]), 4.);
            let (mut frame_ms, mut worst, mut frames) = (0., 0_f64, 0);
            for chunk in samples[1..].chunks(8) {
                let started = std::time::Instant::now();
                for &p in chunk {
                    editor.pointer_move(at(p));
                }
                let annotations = editor.annotations();
                scene = Arc::new(scene.updated_with_draft(
                    &annotations,
                    &source,
                    editor.live_stroke_draft_index(),
                ));
                if let Some(Layer::Vector { mesh, style, .. }) = scene.layers.last() {
                    let mut paths = crate::vector_paint::FramePaths::new(
                        scene.layers.len(),
                        gpui::point(gpui::px(0.), gpui::px(0.)),
                        1.,
                        1.,
                    );
                    std::hint::black_box(paths.get(
                        scene.layers.len() - 1,
                        crate::vector_paint::PathPart::Main,
                        mesh,
                        *style,
                        false,
                    ));
                }
                let t = ms(started);
                frame_ms += t;
                worst = worst.max(t);
                frames += 1;
            }
            editor.pointer_up(at(*samples.last().unwrap()));
            let started = std::time::Instant::now();
            scene = Arc::new(scene.updated(&editor.annotations(), &source));
            last_stroke = (frame_ms / frames as f64, worst, ms(started), frames);
        }
        eprintln!(
            "draw stroke #{strokes}: per frame avg {:.2} ms, worst {:.2} ms over {} frames; commit {:.2} ms",
            last_stroke.0, last_stroke.1, last_stroke.3, last_stroke.2
        );
        // 在全部笔迹之上拖动马赛克。
        let mut items = editor.annotations().to_vec();
        let mut mosaic = mark(Tool::Mosaic, (250., 250.), (650., 550.), 4.);
        mosaic.style.block_size = 10;
        items.push(mosaic.clone());
        let index = items.len() - 1;
        let mut scene = scene.updated(&items, &source);
        let frames = 30;
        let (mut total, mut worst) = (0., 0_f64);
        for k in 1..=frames {
            let mut moved = mosaic.clone();
            moved.translate(k as f32 * 15., k as f32 * 9.);
            items[index] = moved;
            let started = std::time::Instant::now();
            scene = scene.updated_live(&items, &source, None, &[index]);
            let t = ms(started);
            total += t;
            worst = worst.max(t);
        }
        eprintln!(
            "mosaic drag over {strokes} strokes: avg {:.2} ms, worst {worst:.2} ms",
            total / frames as f64
        );
    }
    #[test]
    fn spotlight_opacity_preserves_hole_and_matches_cropped_export() {
        let pixels = [100, 100, 100, 255].repeat(16);
        let canvas = Rect::full(4, 4);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &pixels,
            }],
            overlay: None,
        };
        let mut a = Annotation::new(
            Tool::Spotlight,
            Point { x: 1., y: 1. },
            Style {
                opacity: 0.5,
                width: 0.,
                ..Default::default()
            },
        );
        a.update(Point { x: 3., y: 3. });
        let scene = Scene::prepare(&[a], &source);
        let mut full = source.read(canvas);
        scene.apply(&mut full, canvas);
        assert_eq!(&full.bgra[0..4], &[50, 50, 50, 255]);
        assert_eq!(&full.bgra[20..24], &[100, 100, 100, 255]);
        let crop = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let mut part = source.read(crop);
        scene.apply(&mut part, crop);
        assert_eq!(&part.bgra[12..16], &[100, 100, 100, 255]);
    }
    #[test]
    fn mosaic_is_baked_into_export_and_independent_of_crop_origin() {
        let bytes = pixels();
        let full = Rect::full(16, 12);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
        };
        let scene = Scene::prepare(&[mark(Tool::Mosaic, (2., 2.), (14., 10.), 4.)], &source);
        let mut all = source.read(full);
        scene.apply(&mut all, full);
        let crop = Rect {
            x: 3,
            y: 3,
            width: 7,
            height: 6,
        };
        let mut part = source.read(crop);
        scene.apply(&mut part, crop);
        for y in 0..crop.height {
            for x in 0..crop.width {
                let a = ((y * crop.width + x) as usize) * 4;
                let b = (((y + crop.y) * full.width + x + crop.x) as usize) * 4;
                assert_eq!(part.bgra[a..a + 4], all.bgra[b..b + 4]);
            }
        }
        assert_ne!(all.bgra, bytes);
        assert_eq!(&all.bgra[..4], &bytes[..4]);
        assert_eq!(
            &all.bgra[(2 * 16 + 2) * 4..(2 * 16 + 2) * 4 + 4],
            &[19, 25, 40, 255]
        );
    }
    #[test]
    fn mosaic_covers_lower_annotations_but_preserves_upper_annotations() {
        let bytes = pixels();
        let full = Rect::full(16, 12);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
        };
        let stroke = mark(Tool::Pen, (4., 2.), (4., 10.), 4.);
        let mosaic = mark(Tool::Mosaic, (2., 2.), (14., 10.), 4.);
        let lower = Scene::prepare(&[stroke.clone(), mosaic.clone()], &source);
        let upper = Scene::prepare(&[mosaic, stroke], &source);
        let mut a = source.read(full);
        lower.apply(&mut a, full);
        let mut b = source.read(full);
        upper.apply(&mut b, full);
        let offset = (6 * 16 + 4) * 4;
        assert_ne!(a.bgra[offset..offset + 4], b.bgra[offset..offset + 4]);
        assert_eq!(&b.bgra[offset..offset + 4], &[0x5c, 0x4b, 0xff, 255]);
        if let Layer::Mosaic { blocks } = &lower.layers[1] {
            assert!(!blocks.is_empty());
        } else {
            panic!("missing mosaic");
        }
    }
    #[test]
    fn outside_crop_shapes_are_clipped_and_colors_are_bgra() {
        let bytes = pixels();
        let full = Rect::full(16, 12);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
        };
        let scene = Scene::prepare(&[mark(Tool::Rectangle, (1., 1.), (12., 9.), 2.)], &source);
        let crop = Rect {
            x: 5,
            y: 0,
            width: 7,
            height: 6,
        };
        let mut image = source.read(crop);
        scene.apply(&mut image, crop);
        assert_eq!(
            &image.bgra[(7 + 1) * 4..(7 + 1) * 4 + 4],
            &[0x5c, 0x4b, 0xff, 255]
        );
        assert_eq!(image.bgra.len(), 7 * 6 * 4);
    }
    #[test]
    fn mosaic_past_the_canvas_edge_only_paints_the_visible_intersection() {
        let bytes = pixels();
        let full = Rect::full(16, 12);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
        };
        let scene = Scene::prepare(&[mark(Tool::Mosaic, (-20., -8.), (6., 5.), 4.)], &source);
        let Layer::Mosaic { blocks } = &scene.layers[0] else {
            panic!("missing mosaic");
        };
        assert!(!blocks.is_empty());
        for block in blocks.iter() {
            assert!(block.rect.intersection(full) == Some(block.rect));
            assert!(block.rect.x + block.rect.width <= 6);
            assert!(block.rect.y + block.rect.height <= 5);
        }
        let mut image = source.read(full);
        let before = image.bgra.clone();
        scene.apply(&mut image, full);
        assert_eq!(image.width, full.width);
        assert_eq!(image.height, full.height);
        assert_eq!(image.bgra.len(), before.len());
        assert_ne!(&image.bgra[0..4], &before[0..4]);
        let outside = ((10 * 16 + 14) * 4) as usize;
        assert_eq!(
            &image.bgra[outside..outside + 4],
            &before[outside..outside + 4]
        );
    }
    #[test]
    fn upper_edits_reuse_mosaic_cache_but_lower_edits_invalidate_it() {
        let bytes = pixels();
        let full = Rect::full(16, 12);
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &bytes,
            }],
        };
        let lower = mark(Tool::Rectangle, (2., 2.), (12., 10.), 2.);
        let mosaic = mark(Tool::Mosaic, (2., 2.), (14., 10.), 4.);
        let upper = mark(Tool::Pen, (4., 2.), (4., 10.), 4.);
        let original = Scene::prepare(&[lower.clone(), mosaic.clone(), upper.clone()], &source);
        let mut changed_upper = upper.clone();
        changed_upper.translate(2., 0.);
        let cached = original.updated(&[lower.clone(), mosaic.clone(), changed_upper], &source);
        let (Layer::Mosaic { blocks: a }, Layer::Mosaic { blocks: b }) =
            (&original.layers[1], &cached.layers[1])
        else {
            panic!("missing mosaic");
        };
        assert!(Arc::ptr_eq(a, b));
        let mut changed_lower = lower;
        changed_lower.style.color = 0x0000ff;
        let refreshed = cached.updated(&[changed_lower, mosaic, upper], &source);
        let Layer::Mosaic { blocks: c } = &refreshed.layers[1] else {
            panic!("missing mosaic");
        };
        assert!(!Arc::ptr_eq(b, c));
    }
    #[test]
    fn dragging_mosaic_uses_cached_below_tiles_and_matches_fresh_scene() {
        let canvas = Rect::full(300, 260);
        let bytes: Vec<u8> = (0..canvas.width * canvas.height)
            .flat_map(|i| {
                [
                    (i % 251) as u8,
                    (i / 300 * 3) as u8,
                    (i * 7 % 199) as u8,
                    255,
                ]
            })
            .collect();
        let source = SourcePixels {
            fill: None,
            overlay: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
        };
        let mut filled = mark(Tool::Ellipse, (100., 20.), (220., 200.), 3.);
        filled.style.filled = true;
        filled.style.opacity = 0.6;
        let mut spotlight = mark(Tool::Spotlight, (20., 20.), (280., 240.), 0.);
        spotlight.style.opacity = 0.4;
        let mut items = vec![
            mark(Tool::Rectangle, (10., 10.), (250., 200.), 5.),
            mark(Tool::Pen, (5., 250.), (290., 8.), 6.),
            mark(Tool::Arrow, (280., 240.), (30., 40.), 4.),
            filled,
            spotlight,
            mark(Tool::Blur, (60., 120.), (200., 230.), 6.),
            mark(Tool::Mosaic, (40., 30.), (170., 150.), 6.),
            mark(Tool::Pen, (0., 0.), (299., 259.), 5.),
        ];
        let render = |scene: &Scene| {
            let mut image = source.read(canvas);
            scene.try_apply(&mut image, canvas).unwrap();
            image.bgra
        };
        let mut scene = Scene::prepare(&items, &source);
        for step in 0..6 {
            items[6].translate(17., 11.);
            let next = scene.updated(&items, &source);
            // 未变化且不采样下层的上层对象原样沿用。
            let (Layer::Vector { mesh: a, .. }, Layer::Vector { mesh: b, .. }) =
                (&scene.layers[7], &next.layers[7])
            else {
                panic!("upper pen layer");
            };
            assert!(Arc::ptr_eq(a, b));
            assert!(
                render(&next) == render(&Scene::prepare(&items, &source)),
                "step {step}"
            );
            scene = next;
        }
        let cache = scene.below_cache.lock().unwrap();
        assert!(cache.entries.iter().any(|(_, tiles)| !tiles.is_empty()));
    }
    #[test]
    fn vector_pencil_corners_opacity_hit_and_crop_use_shared_geometry() {
        let canvas = Rect::full(180, 160);
        let pixels = [255_u8, 255, 255, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &pixels,
            }],
            overlay: None,
        };
        let mut pen = mark(Tool::Pen, (30., 30.), (110., 30.), 4.);
        pen.update(Point { x: 110., y: 120. });
        let original = pen.points.clone();
        let scene = Scene::prepare(&[pen.clone()], &source);
        let Layer::Vector { path, segments, .. } = &scene.layers[0] else {
            panic!("铅笔没有进入 GPU 矢量层")
        };
        assert!(path
            .spans
            .iter()
            .any(|span| span.end() == Point { x: 110., y: 30. }));
        assert_eq!(segments.as_ref(), &pen.segments());
        assert!(pen.hit(Point { x: 110., y: 30. }, 0., false));
        assert!(!pen.hit(Point { x: 105., y: 35. }, 0., false));
        let crop = Rect {
            x: 96,
            y: 16,
            width: 40,
            height: 80,
        };
        let mut full = source.read(canvas);
        scene.apply(&mut full, canvas);
        let mut part = source.read(crop);
        scene.apply(&mut part, crop);
        for y in 0..crop.height {
            for x in 0..crop.width {
                let a = (((y + crop.y) * canvas.width + x + crop.x) * 4) as usize;
                let b = ((y * crop.width + x) * 4) as usize;
                assert_eq!(&full.bgra[a..a + 4], &part.bgra[b..b + 4]);
            }
        }
        pen.style.width = 40.;
        assert_eq!(pen.points, original);
        assert_eq!(pen.segments(), segments.as_ref().clone());
        pen.style.width = 4.;
        pen.style.opacity = 0.35;
        let faded = Scene::prepare(&[pen], &source);
        assert!(matches!(faded.layers[0], Layer::Vector { .. }));
    }
    #[test]
    fn basic_curves_fills_dashes_and_opacity_all_keep_vector_layers() {
        let canvas = Rect::full(640, 480);
        let bg = [255, 255, 255, 255].repeat(640 * 480);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bg,
            }],
            overlay: None,
        };
        for tool in [
            Tool::Rectangle,
            Tool::Ellipse,
            Tool::Pen,
            Tool::Polyline,
            Tool::HighlighterRect,
        ] {
            let mut item = mark(tool, (30., 30.), (130., 110.), 4.);
            item.style.opacity = 0.4;
            item.style.dashed = true;
            item.style.filled = true;
            item.style.radius = 18.;
            let scene = Scene::prepare(&[item], &source);
            assert!(scene.errors.is_empty(), "{:?}", scene.errors);
            assert!(
                matches!(scene.layers[0], Layer::Vector { .. }),
                "{tool:?} rasterized"
            );
        }
    }

    #[test]
    fn highlight_radius_rotation_crop_and_raster_share_single_alpha_coverage() {
        let canvas = Rect::full(180, 150);
        let bytes = [100, 100, 100, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
            overlay: None,
        };
        let mut item = mark(Tool::HighlighterRect, (40., 40.), (120., 110.), 24.);
        item.style.color = 0x2244dd;
        item.style.opacity = 0.35;
        item.style.radius = 25.;
        item.rotation = 37.;
        let scene = Scene::prepare(&[item.clone()], &source);
        assert!(scene.errors.is_empty(), "{:?}", scene.errors);
        let Layer::Vector {
            mesh,
            segments,
            style,
            ..
        } = &scene.layers[0]
        else {
            panic!("missing highlighter vector");
        };
        assert!(!mesh.indices.is_empty());
        assert!(segments.is_empty());
        assert_eq!(style.width, 0.);
        let mut full = source.read(canvas);
        scene.apply(&mut full, canvas);
        let center = ((75 * canvas.width + 80) * 4) as usize;
        assert_eq!(&full.bgra[center..center + 4], &[142, 89, 77, 255]);
        let corner = item.to_world(Point { x: 41.5, y: 41.5 });
        let corner =
            ((corner.y.floor() as u32 * canvas.width + corner.x.floor() as u32) * 4) as usize;
        assert_eq!(&full.bgra[corner..corner + 4], &[100, 100, 100, 255]);
        let outside = item.to_world(Point { x: 80., y: 36. });
        let outside =
            ((outside.y.floor() as u32 * canvas.width + outside.x.floor() as u32) * 4) as usize;
        assert_eq!(&full.bgra[outside..outside + 4], &[100, 100, 100, 255]);

        let (rect, raster) = crate::raster::shape(&item, canvas).unwrap();
        assert_eq!(raster.bgra.chunks_exact(4).map(|p| p[3]).max(), Some(89));
        assert!(raster.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 89));
        let mut fallback = source.read(canvas);
        crate::raster::composite(&mut fallback, canvas, &raster, rect);
        for (actual, expected) in full.bgra.iter().zip(&fallback.bgra) {
            assert!((*actual as i16 - *expected as i16).abs() <= 1);
        }
        let mut wide = item.clone();
        wide.style.width = 80.;
        let mut widened = source.read(canvas);
        Scene::prepare(&[wide.clone()], &source).apply(&mut widened, canvas);
        assert_eq!(widened.bgra, full.bgra);
        let (wide_rect, wide_raster) = crate::raster::shape(&wide, canvas).unwrap();
        assert_eq!(wide_rect, rect);
        assert_eq!(wide_raster.bgra, raster.bgra);

        let crop = Rect {
            x: 30,
            y: 30,
            width: 96,
            height: 96,
        };
        let mut part = source.read(crop);
        scene.apply(&mut part, crop);
        for y in 0..crop.height {
            for x in 0..crop.width {
                let a = ((y * crop.width + x) * 4) as usize;
                let b = (((y + crop.y) * canvas.width + x + crop.x) * 4) as usize;
                assert_eq!(&part.bgra[a..a + 4], &full.bgra[b..b + 4]);
            }
        }
    }

    #[test]
    fn highlight_radius_edit_invalidates_downstream_effect_sampling() {
        let canvas = Rect::full(160, 128);
        let bytes = [100, 100, 100, 255].repeat((canvas.width * canvas.height) as usize);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
            overlay: None,
        };
        let mut highlight = mark(Tool::HighlighterRect, (32., 28.), (128., 108.), 24.);
        highlight.style.color = 0x2244dd;
        highlight.style.opacity = 0.35;
        let mosaic = mark(Tool::Mosaic, (32., 28.), (64., 60.), 4.);
        let before = Scene::prepare(&[highlight.clone(), mosaic.clone()], &source);
        highlight.style.radius = 28.;
        let after = before.updated(&[highlight.clone(), mosaic.clone()], &source);
        let (Layer::Mosaic { blocks: old }, Layer::Mosaic { blocks: new }) =
            (&before.layers[1], &after.layers[1])
        else {
            panic!("missing mosaic");
        };
        assert!(!Arc::ptr_eq(old, new));
        assert_eq!(new.first().unwrap().color, 0x646464);
        assert_ne!(old.first().unwrap().color, new.first().unwrap().color);
        let cold = Scene::prepare(&[highlight, mosaic], &source);
        let mut actual = source.read(canvas);
        after.apply(&mut actual, canvas);
        let mut expected = source.read(canvas);
        cold.apply(&mut expected, canvas);
        assert_eq!(actual.bgra, expected.bgra);
    }
    #[test]
    fn vector_dashed_arrow_export_keeps_solid_heads_and_single_alpha() {
        let canvas = Rect::full(240, 160);
        let bg = [255, 255, 255, 255].repeat(240 * 160);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bg,
            }],
            overlay: None,
        };
        let mut item = mark(Tool::Arrow, (20., 40.), (200., 100.), 4.);
        item.style.dashed = true;
        item.style.opacity = 0.6;
        let scene = Scene::prepare(&[item.clone()], &source);
        assert!(scene.errors.is_empty());
        let mut actual = source.read(canvas);
        scene.apply(&mut actual, canvas);
        let mut expected = source.read(canvas);
        let (rect, image) = crate::raster::shape(&item, canvas).unwrap();
        crate::raster::composite(&mut expected, canvas, &image, rect);
        for (a, b) in actual.bgra.iter().zip(expected.bgra) {
            assert!(
                (*a as i16 - b as i16).abs() <= 1,
                "head/dash coverage mismatch: {a}/{b}"
            );
        }
    }
    #[test]
    fn spotlight_group_change_invalidates_first_layer_and_downstream_sampling() {
        let canvas = Rect::full(88, 56);
        let bytes = [100, 100, 100, 255].repeat(88 * 56);
        let source = SourcePixels {
            fill: None,
            canvas,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &bytes,
            }],
            overlay: None,
        };
        let pen = mark(Tool::Pen, (4., 4.), (8., 4.), 2.);
        let mut first = mark(Tool::Spotlight, (8., 8.), (24., 36.), 0.);
        first.style.opacity = 0.5;
        let mut second = mark(Tool::Spotlight, (40., 8.), (64., 36.), 0.);
        second.style.opacity = 0.5;
        let mosaic = mark(Tool::Mosaic, (40., 12.), (64., 32.), 4.);
        let items = vec![pen.clone(), first.clone(), mosaic.clone(), second.clone()];
        let scene = Scene::prepare(&items, &source);
        assert!(scene.errors.is_empty(), "{:?}", scene.errors);
        assert_eq!(scene.layers.len(), items.len());
        assert!(matches!(scene.layers[3], Layer::Empty));
        let Layer::Spotlight { geometry: before } = &scene.layers[1] else {
            panic!()
        };
        // 无关的上层对象变化不重建孔洞几何。
        let mut extended = items.clone();
        extended.push(mark(Tool::Pen, (70., 40.), (82., 40.), 2.));
        let same = scene.updated(&extended, &source);
        let Layer::Spotlight { geometry: kept } = &same.layers[1] else {
            panic!()
        };
        assert!(Arc::ptr_eq(before, kept));
        second.translate(20., 0.);
        let moved_items = vec![pen, first, mosaic, second];
        let moved = scene.updated(&moved_items, &source);
        let Layer::Spotlight { geometry: after } = &moved.layers[1] else {
            panic!()
        };
        assert!(!Arc::ptr_eq(before, after));
        let (Layer::Mosaic { blocks: old }, Layer::Mosaic { blocks: new }) =
            (&scene.layers[2], &moved.layers[2])
        else {
            panic!()
        };
        assert!(!Arc::ptr_eq(old, new));
        assert!(old.iter().zip(new.iter()).any(|(a, b)| a.color != b.color));
        let mut actual = source.read(canvas);
        moved.apply(&mut actual, canvas);
        let mut expected = source.read(canvas);
        Scene::prepare(&moved_items, &source).apply(&mut expected, canvas);
        assert_eq!(actual.bgra, expected.bgra);
        // 删除后孔也必须重建首孔的共享层，而不是只截掉末尾占位。
        let removed = moved.updated(&moved_items[..3], &source);
        let Layer::Spotlight {
            geometry: removed_geometry,
        } = &removed.layers[1]
        else {
            panic!()
        };
        assert!(!Arc::ptr_eq(after, removed_geometry));
        let mut actual = source.read(canvas);
        removed.apply(&mut actual, canvas);
        let mut expected = source.read(canvas);
        Scene::prepare(&moved_items[..3], &source).apply(&mut expected, canvas);
        assert_eq!(actual.bgra, expected.bgra);
    }
}
